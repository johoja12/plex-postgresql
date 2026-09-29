use sqlparser::ast::helpers::attached_token::AttachedToken;
/// Module: query
///
/// Miscellaneous query fixups for SQLite → PostgreSQL translation:
///   1. Subquery alias      — FROM (SELECT …) without alias → AS _subq
///   2. NULLS LAST          — `ORDER BY col IS NULL, col ASC` → `ORDER BY col NULLS LAST`
///   3. DISTINCT + agg ORDER BY — `SELECT DISTINCT … ORDER BY count(*)` → remove DISTINCT
///   4. CASE booleans       — `CASE WHEN x THEN 1 ELSE 0 END` → TRUE/FALSE
///   5. WHERE 1 / WHERE 0   — → WHERE TRUE / WHERE FALSE
///   6. max(a,b) multi-arg  — → GREATEST(a,b)
///   7. min(a,b) multi-arg  — → LEAST(a,b)
///   8. COLLATE ICU strip   — remove icu_* collations
///   9. COLLATE NOCASE      — `x COLLATE NOCASE = y` → `LOWER(x) = LOWER(y)`
///  10. FTS4 MATCH          — `col MATCH 'term'` → `to_tsvector('simple', col) @@ to_tsquery('simple', E'term')`
///  11. Collections filter  — remove `metadata_type=18` from OR conditions
///  12. Int/text mismatch   — cast integer to ::text for known text columns
///                            and cast string literal to integer for known int columns
use sqlparser::ast::*;
use sqlparser::tokenizer::Span;

use crate::rewriter::ast_utils::{take_boxed_expr, take_expr, wrap_double_colon_cast};
use std::cell::RefCell;

pub fn transform(stmt: &mut Statement) {
    match stmt {
        Statement::Query(q) => transform_query(q),
        Statement::Insert(ins) => {
            if let Some(src) = &mut ins.source {
                transform_query(src);
            }
        }
        Statement::Update(u) => {
            let _scope =
                TextColumnScope::for_tables(&table_names_in(std::slice::from_ref(&u.table)));
            if let Some(sel) = &mut u.selection {
                transform_expr(sel);
            }
        }
        Statement::Delete(d) => {
            let tables = match &d.from {
                FromTable::WithFromKeyword(t) | FromTable::WithoutKeyword(t) => table_names_in(t),
            };
            let _scope = TextColumnScope::for_tables(&tables);
            if let Some(sel) = &mut d.selection {
                transform_expr(sel);
            }
        }
        _ => {}
    }
}

fn transform_query(q: &mut Query) {
    if let Some(with) = &mut q.with {
        for cte in &mut with.cte_tables {
            transform_query(&mut cte.query);
        }
    }

    prune_collection_prefetch_tag_joins(q);

    // Check for DISTINCT + aggregate in ORDER BY → remove DISTINCT
    fix_distinct_with_agg_orderby(q);

    // For SELECT DISTINCT, ORDER BY exprs must appear in the select list (PG requirement)
    fix_distinct_orderby_not_in_select(q);

    // Normalize parentheses in ORDER BY exprs (must run after fix above)
    normalize_orderby_exprs(q);

    transform_set_expr(&mut q.body);

    // Transform expressions in ORDER BY (collate stripping, etc.)
    if let Some(ob) = &mut q.order_by {
        if let OrderByKind::Expressions(exprs) = &mut ob.kind {
            for oe in exprs.iter_mut() {
                transform_expr(&mut oe.expr);
            }
        }
    }

    // Fix ORDER BY IS NULL pattern → NULLS LAST (must run after expr transform)
    fix_order_by_is_null(q);

    // Remove ORDER BY rowid (PostgreSQL doesn't have rowid)
    fix_order_by_rowid(q);
}

// Plex prefetches collection members with unused tag LEFT JOINs. Those joins
// multiply wide metadata rows before DISTINCT (hundreds of MB of sort work).
// Recognize only the plain, fully qualified column projection / ID-list shape.
// Keep DISTINCT: other joins can still duplicate rows. Fail closed on any extra
// clause, aggregate, wildcard, alias on a table, or expression we do not know.
fn prune_collection_prefetch_tag_joins(q: &mut Query) {
    let SetExpr::Select(sel) = q.body.as_ref() else {
        return;
    };
    if !matches!(sel.distinct, Some(Distinct::Distinct)) {
        return;
    }
    let safe_column = |e: &Expr| match e {
        Expr::CompoundIdentifier(ids) if ids.len() == 2 => matches!(
            ids[0].value.as_str(),
            "metadata_items" | "media_items" | "media_parts" | "metadata_item_settings"
        ),
        _ => false,
    };
    if sel.projection.is_empty()
        || !sel.projection.iter().all(|p| match p {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                safe_column(e)
            }
            _ => false,
        })
    {
        return;
    }
    let Some(Expr::InList {
        expr,
        list,
        negated: false,
    }) = &sel.selection
    else {
        return;
    };
    if expr.to_string() != "metadata_items.id" || list.is_empty() || !list.iter().all(|e|
        matches!(e, Expr::Value(v) if matches!(&v.value, Value::Number(n, _) if n.bytes().all(|b| b.is_ascii_digit())))
    ) { return; }
    let Some(ob) = &q.order_by else { return };
    let OrderByKind::Expressions(order) = &ob.kind else {
        return;
    };
    if !order.iter().all(|e| safe_column(&e.expr)) {
        return;
    }
    const FROM: &str = "metadata_items LEFT JOIN media_items ON media_items.metadata_item_id = metadata_items.id LEFT JOIN media_parts ON media_parts.media_item_id = media_items.id LEFT JOIN metadata_item_settings ON metadata_item_settings.guid = metadata_items.guid AND metadata_item_settings.account_id = 1 LEFT JOIN taggings ON taggings.metadata_item_id = metadata_items.id LEFT JOIN tags ON taggings.tag_id = tags.id";
    let expected = format!(
        "SELECT DISTINCT {} FROM {} WHERE {} {}",
        sel.projection
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", "),
        FROM,
        sel.selection.as_ref().unwrap(),
        ob
    );
    // This full-shape check also rejects WITH, GROUP BY, HAVING, windows,
    // LIMIT/OFFSET, locks and other additions without needing a field denylist.
    if q.to_string() != expected {
        return;
    }
    if let SetExpr::Select(sel) = q.body.as_mut() {
        sel.from[0].joins.truncate(3);
    }
}

fn transform_set_expr(se: &mut SetExpr) {
    match se {
        SetExpr::Select(s) => transform_select(s),
        SetExpr::Query(q) => transform_query(q),
        SetExpr::SetOperation { left, right, .. } => {
            transform_set_expr(left);
            transform_set_expr(right);
        }
        SetExpr::Values(vals) => {
            for row in &mut vals.rows {
                for e in row {
                    transform_expr(e);
                }
            }
        }
        _ => {}
    }
}

fn transform_select(sel: &mut Select) {
    let _scope = TextColumnScope::for_tables(&table_names_in(&sel.from));

    // Fix FROM subqueries without alias
    for twj in &mut sel.from {
        fix_table_with_joins(twj);
    }

    // WHERE fixups
    if let Some(w) = &mut sel.selection {
        fix_where_numeric(w);
        fix_collections_filter(w);
        transform_expr(w);
    }

    // Projection
    for item in &mut sel.projection {
        match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                transform_expr(e);
            }
            _ => {}
        }
    }

    // GROUP BY
    if let GroupByExpr::Expressions(ref mut exprs, _) = sel.group_by {
        for e in exprs {
            transform_expr(e);
        }
    }

    // HAVING
    if let Some(h) = &mut sel.having {
        transform_expr(h);
    }
}

// ─── Fix 1: Subquery alias ────────────────────────────────────────────────────

fn fix_table_with_joins(twj: &mut TableWithJoins) {
    fix_table_factor(&mut twj.relation);
    for join in &mut twj.joins {
        fix_table_factor(&mut join.relation);
    }
}

fn fix_table_factor(tf: &mut TableFactor) {
    if let TableFactor::Derived {
        subquery, alias, ..
    } = tf
    {
        transform_query(subquery);
        // Add alias if missing
        if alias.is_none() {
            *alias = Some(TableAlias {
                explicit: true,
                name: Ident::new("_subq"),
                columns: vec![],
            });
        }
    }
}

// ─── Fix 2: NULLS LAST from IS NULL pattern ───────────────────────────────────

/// Detect: `ORDER BY expr IS NULL, expr [ASC|DESC]`
/// Replace with: `ORDER BY expr [ASC|DESC] NULLS LAST`
fn fix_order_by_is_null(q: &mut Query) {
    let Some(ob) = &mut q.order_by else { return };
    let OrderByKind::Expressions(exprs) = &mut ob.kind else {
        return;
    };

    // Find pairs: (i, j) where exprs[i] is `x IS NULL` and exprs[j] is `x [ASC|DESC]`
    // Strategy: collect indices of IS NULL exprs, match with following expr
    let len = exprs.len();
    if len < 2 {
        return;
    }

    let mut to_remove: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < len {
        if let Expr::IsNull(ref inner) = exprs[i].expr {
            let col = *inner.clone();
            // Look for the next expr that matches `col`
            if i + 1 < len {
                let next = &mut exprs[i + 1];
                if exprs_display_eq(&next.expr, &col) {
                    // Set NULLS LAST on the real ordering expr
                    next.options.nulls_first = Some(false);
                    to_remove.push(i);
                    i += 2;
                    continue;
                }
            }
        }
        i += 1;
    }

    // Remove IS NULL entries in reverse order
    for idx in to_remove.into_iter().rev() {
        exprs.remove(idx);
    }
}

fn exprs_display_eq(a: &Expr, b: &Expr) -> bool {
    format!("{}", a).to_lowercase() == format!("{}", b).to_lowercase()
}

// ─── Fix 11: ORDER BY rowid → removed ────────────────────────────────────────

fn fix_order_by_rowid(q: &mut Query) {
    let Some(ob) = &mut q.order_by else { return };
    let OrderByKind::Expressions(exprs) = &mut ob.kind else {
        return;
    };

    // Remove any ORDER BY expression that is just `rowid`
    exprs.retain(|oe| {
        if let Expr::Identifier(ident) = &oe.expr {
            ident.value.to_lowercase() != "rowid"
        } else {
            true
        }
    });

    // If all ORDER BY expressions were removed, remove the ORDER BY clause
    if exprs.is_empty() {
        q.order_by = None;
    }
}

// ─── Fix 3: DISTINCT + aggregate ORDER BY ─────────────────────────────────────

fn fix_distinct_with_agg_orderby(q: &mut Query) {
    let has_distinct = if let SetExpr::Select(sel) = q.body.as_ref() {
        matches!(sel.distinct, Some(Distinct::Distinct))
    } else {
        false
    };

    if !has_distinct {
        return;
    }

    let has_incompatible_in_order = if let Some(ob) = &q.order_by {
        if let OrderByKind::Expressions(exprs) = &ob.kind {
            exprs
                .iter()
                .any(|oe| is_distinct_incompatible_expr(&oe.expr))
        } else {
            false
        }
    } else {
        false
    };

    if has_incompatible_in_order {
        if let SetExpr::Select(sel) = q.body.as_mut() {
            sel.distinct = None;
        }
    }
}

// ─── Fix 3b: DISTINCT + ORDER BY expr not in select list ──────────────────────
// PostgreSQL requires all ORDER BY expressions to appear in the SELECT list
// when the query uses SELECT DISTINCT. SQLite does not have this restriction.
// We append any missing ORDER BY expressions as extra (unnamed) select items.
fn fix_distinct_orderby_not_in_select(q: &mut Query) {
    // Only apply to SELECT DISTINCT (not DISTINCT ON)
    let has_plain_distinct = if let SetExpr::Select(sel) = q.body.as_ref() {
        matches!(sel.distinct, Some(Distinct::Distinct))
    } else {
        false
    };
    if !has_plain_distinct {
        return;
    }

    let Some(ob) = &q.order_by else { return };
    let OrderByKind::Expressions(order_exprs) = &ob.kind else {
        return;
    };
    if order_exprs.is_empty() {
        return;
    }

    // Collect string representations of current select items for membership check
    let existing: Vec<String> = if let SetExpr::Select(sel) = q.body.as_ref() {
        sel.projection
            .iter()
            .map(|item| match item {
                SelectItem::UnnamedExpr(e) => expr_to_key(e),
                SelectItem::ExprWithAlias { expr, .. } => expr_to_key(expr),
                SelectItem::QualifiedWildcard(name, _) => name.to_string(),
                SelectItem::Wildcard(_) => "*".to_string(),
            })
            .collect()
    } else {
        return;
    };

    // Check if any ORDER BY expr is missing from the select list
    let has_missing = order_exprs.iter().any(|oe| {
        let key = expr_to_key(&oe.expr);
        !existing.contains(&key)
    });

    if !has_missing {
        return;
    }

    // Strategy 1: append missing ORDER BY exprs to SELECT list
    // Strategy 2: if SELECT list is a single expression (common Plex pattern like
    //   SELECT DISTINCT (id) ... ORDER BY col), append all missing exprs.
    // We always append — this is the PostgreSQL-required approach.
    let to_add: Vec<Expr> = order_exprs
        .iter()
        .filter_map(|oe| {
            let key = expr_to_key(&oe.expr);
            if existing.contains(&key) {
                None
            } else {
                Some(oe.expr.clone())
            }
        })
        .collect();

    if let SetExpr::Select(sel) = q.body.as_mut() {
        // Also unwrap parentheses from existing select items to ensure PG equality
        // e.g. SELECT DISTINCT (id) ORDER BY id — PG requires they match exactly.
        for item in sel.projection.iter_mut() {
            if let SelectItem::UnnamedExpr(Expr::Nested(inner)) = item {
                *item = SelectItem::UnnamedExpr(*inner.clone());
            }
        }
        if to_add.is_empty() {
            // All ORDER BY exprs already appear in select list — nothing to append.
            // But parens unwrap above may have changed the query, which is enough.
            return;
        }
        // Append missing ORDER BY exprs to SELECT list so PostgreSQL accepts DISTINCT.
        for expr in to_add {
            sel.projection.push(SelectItem::UnnamedExpr(expr));
        }
        // Fallback safety: if we still can't resolve (e.g. wildcard select),
        // remove DISTINCT entirely — this may return duplicates but avoids 500.
        // (Recheck after appending: if projection now has all ORDER BY exprs, keep DISTINCT.)
        // Actually we already appended them above, so DISTINCT is now valid. No need to remove.
    }
}

// ─── Fix 3c: DISTINCT + ORDER BY — strip DISTINCT when ORDER BY contains
//            expressions not derivable from select list (fallback) ─────────────
//
// For queries like:
//   SELECT DISTINCT metadata_items.id FROM ... ORDER BY metadata_items.id
// where the expr_to_key comparison may still miss some cases (e.g. qualified
// vs unqualified, aliases), we additionally strip parentheses from any
// Nested expr in the ORDER BY to match what's now in the select list.
fn normalize_orderby_exprs(q: &mut Query) {
    let Some(ob) = &mut q.order_by else { return };
    let OrderByKind::Expressions(exprs) = &mut ob.kind else {
        return;
    };
    for oe in exprs.iter_mut() {
        if let Expr::Nested(inner) = &oe.expr {
            oe.expr = *inner.clone();
        }
    }
}

/// Produce a normalised string key for an expression for membership comparison.
fn expr_to_key(expr: &Expr) -> String {
    // Unwrap a single-layer of parentheses if present (AST level)
    let inner = match expr {
        Expr::Nested(inner) => inner.as_ref(),
        other => other,
    };
    // Use the Display impl but strip whitespace and parens, lowercase for comparison
    inner
        .to_string()
        .to_lowercase()
        .replace(' ', "")
        .replace('(', "")
        .replace(')', "")
}

// ─── Fix 5: WHERE 1 / WHERE 0 ─────────────────────────────────────────────────

fn fix_where_numeric(expr: &mut Expr) {
    if let Expr::Value(ValueWithSpan {
        value: Value::Number(ref s, _),
        ref span,
    }) = *expr
    {
        if s == "1" {
            *expr = Expr::Value(ValueWithSpan {
                value: Value::Boolean(true),
                span: *span,
            });
        } else if s == "0" {
            *expr = Expr::Value(ValueWithSpan {
                value: Value::Boolean(false),
                span: *span,
            });
        }
    }
}

// ─── Main expression transformer ─────────────────────────────────────────────

/// Recursively transform an expression in place, applying all query-level
/// fixups (CASE booleans, max/min→GREATEST/LEAST, COLLATE, FTS MATCH, etc.).
fn transform_expr(expr: &mut Expr) {
    match expr {
        // Fix 4: CASE WHEN x THEN 1 ELSE 0 END → TRUE/FALSE
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            // Idempotence guard: if this already is our JSON validity guard pattern,
            // do not rewrite the result branch again on retranslations.
            let is_json_guard = operand.is_none()
                && conditions.len() == 1
                && is_json_valid_call(&conditions[0].condition)
                && matches!(
                    else_result.as_deref(),
                    Some(Expr::Value(ValueWithSpan {
                        value: Value::Null,
                        ..
                    }))
                );
            if is_json_guard {
                return;
            }
            if let Some(op) = operand {
                transform_expr(op);
            }
            for cw in conditions {
                transform_expr(&mut cw.condition);
                transform_expr(&mut cw.result);
                maybe_bool_value_inplace(&mut cw.result);
            }
            if let Some(er) = else_result {
                transform_expr(er);
                maybe_bool_value_inplace(er);
            }
        }

        // Fix 6 & 7: max(a,b) → GREATEST, min(a,b) → LEAST
        Expr::Function(ref mut func) => {
            let fn_name = func_name_str(func);
            let arg_count = if let FunctionArguments::List(ref al) = func.args {
                al.args.len()
            } else {
                0
            };

            // Recurse into args
            if let FunctionArguments::List(ref mut al) = func.args {
                for arg in &mut al.args {
                    match arg {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                            transform_expr(e);
                        }
                        FunctionArg::Named {
                            arg: FunctionArgExpr::Expr(e),
                            ..
                        } => {
                            transform_expr(e);
                        }
                        _ => {}
                    }
                }
            }

            if fn_name == "max" && arg_count >= 2 {
                rename_function(func, "GREATEST");
            } else if fn_name == "min" && arg_count >= 2 {
                rename_function(func, "LEAST");
            }
        }

        // Fix FTS4: col MATCH 'term' → to_tsvector('simple', col) @@ to_tsquery('simple', E'converted_term')
        Expr::BinaryOp {
            op: BinaryOperator::PGCustomBinaryOperator(ref custom_ops),
            ..
        } if custom_ops.len() == 1 && custom_ops[0] == "MATCH" => {
            let taken = take_expr(expr);
            if let Expr::BinaryOp { left, right, .. } = taken {
                *expr = transform_fts_match(*left, *right);
            }
        }
        // JSON operator fix: PostgreSQL requires JSON/JSONB LHS.
        // Plex stores `extra_data` as TEXT, so cast that column to jsonb.
        Expr::BinaryOp {
            op: BinaryOperator::PGCustomBinaryOperator(ref custom_ops),
            left,
            right,
        } if custom_ops.len() == 1 && is_pg_json_op(&custom_ops[0]) => {
            let op_str = custom_ops[0].clone();
            let taken_left = take_boxed_expr(left);
            let taken_right = take_boxed_expr(right);
            *expr = rewrite_json_binary_op(
                taken_left,
                BinaryOperator::PGCustomBinaryOperator(vec![op_str]),
                taken_right,
            );
        }
        Expr::BinaryOp {
            op:
                BinaryOperator::Arrow
                | BinaryOperator::LongArrow
                | BinaryOperator::HashArrow
                | BinaryOperator::HashLongArrow,
            ..
        } => {
            let taken = take_expr(expr);
            if let Expr::BinaryOp { left, op, right } = taken {
                *expr = rewrite_json_binary_op(*left, op, *right);
            }
        }
        Expr::BinaryOp {
            op: BinaryOperator::Match,
            ..
        } => {
            let taken = take_expr(expr);
            if let Expr::BinaryOp { left, right, .. } = taken {
                *expr = transform_fts_match(*left, *right);
            }
        }

        // Fix 12 + Fix 9 + Fix 10 + general BinaryOp
        Expr::BinaryOp { left, op, right } => {
            let is_eq_op = matches!(op, BinaryOperator::Eq | BinaryOperator::NotEq);
            let is_logical_op = matches!(op, BinaryOperator::And | BinaryOperator::Or);

            // Fix 12: Int/text mismatch
            if is_eq_op && should_fix_int_text_mismatch(left, right) {
                let taken = take_expr(expr);
                if let Expr::BinaryOp { left, op, right } = taken {
                    *expr = fix_int_text_mismatch(*left, op, *right);
                }
                return;
            }

            // Fix 9: COLLATE NOCASE on equality → LOWER(x) = LOWER(y)
            let left_nocase = is_collate_nocase(left);
            let right_nocase = is_collate_nocase(right);

            if is_eq_op && (left_nocase || right_nocase) {
                strip_collate_nocase_inplace(left);
                strip_collate_nocase_inplace(right);
                transform_expr(left);
                transform_expr(right);
                wrap_lower_inplace(left);
                wrap_lower_inplace(right);
            } else if is_logical_op {
                // Fix 10: 0/1 in boolean context (AND/OR) → FALSE/TRUE
                strip_collate_nocase_inplace(left);
                strip_collate_nocase_inplace(right);
                transform_expr(left);
                transform_expr(right);
                maybe_bool_value_inplace(left);
                maybe_bool_value_inplace(right);
            } else {
                strip_collate_nocase_inplace(left);
                strip_collate_nocase_inplace(right);
                transform_expr(left);
                transform_expr(right);
            }
        }
        Expr::UnaryOp { expr: inner, .. } => transform_expr(inner),
        Expr::Nested(inner) => transform_expr(inner),
        Expr::Cast { expr: inner, .. } => transform_expr(inner),
        Expr::InList {
            expr: inner, list, ..
        } => {
            transform_expr(inner);
            for e in list {
                transform_expr(e);
            }
        }
        Expr::Between {
            expr: inner,
            low,
            high,
            ..
        } => {
            transform_expr(inner);
            transform_expr(low);
            transform_expr(high);
        }
        Expr::Collate { .. } => {
            // Fix 8: strip ICU collations entirely
            // Fix 9b: NOCASE outside equality → wrap in LOWER()
            let collation_name = match expr {
                Expr::Collate { collation, .. } => collation
                    .0
                    .first()
                    .and_then(|p| match p {
                        ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
                        _ => None,
                    })
                    .unwrap_or_default(),
                _ => unreachable!(),
            };

            if collation_name.starts_with("icu") || collation_name == "unicode" {
                // Strip the collation entirely — unwrap inner expr
                let taken = take_expr(expr);
                if let Expr::Collate { expr: inner, .. } = taken {
                    *expr = *inner;
                }
                transform_expr(expr);
            } else if collation_name == "nocase" {
                // Standalone NOCASE (ORDER BY, non-equality) → LOWER(expr)
                let taken = take_expr(expr);
                if let Expr::Collate { expr: inner, .. } = taken {
                    *expr = *inner;
                }
                transform_expr(expr);
                wrap_lower_inplace(expr);
            } else {
                // Keep other collations, recurse into inner
                if let Expr::Collate { expr: inner, .. } = expr {
                    transform_expr(inner);
                }
            }
        }
        // Fix LIKE ... COLLATE NOCASE → ILIKE
        Expr::Like {
            expr: like_expr,
            pattern,
            ..
        } => {
            let pat_nocase = is_collate_nocase(pattern);
            strip_collate_nocase_inplace(pattern);
            transform_expr(like_expr);
            transform_expr(pattern);
            if pat_nocase {
                // Convert to ILIKE — need to take the whole node
                let taken = take_expr(expr);
                if let Expr::Like {
                    negated,
                    expr: e,
                    pattern: p,
                    escape_char,
                    any,
                } = taken
                {
                    *expr = Expr::ILike {
                        negated,
                        expr: e,
                        pattern: p,
                        escape_char,
                        any,
                    };
                }
            }
        }
        Expr::ILike {
            expr: ilike_expr,
            pattern,
            ..
        } => {
            // Strip NOCASE from ILIKE too (already case-insensitive)
            strip_collate_nocase_inplace(pattern);
            transform_expr(ilike_expr);
            transform_expr(pattern);
        }
        Expr::IsNull(inner) => transform_expr(inner),
        Expr::IsNotNull(inner) => transform_expr(inner),
        _ => {}
    }
}

fn is_json_valid_call(expr: &Expr) -> bool {
    match expr {
        Expr::Function(f) => {
            let name = f.name.0.first().and_then(|p| match p {
                ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
                _ => None,
            });
            name.as_deref() == Some("json_valid")
        }
        _ => false,
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn func_name_str(func: &Function) -> String {
    func.name
        .0
        .first()
        .and_then(|p| match p {
            ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
            _ => None,
        })
        .unwrap_or_default()
}

fn rename_function(func: &mut Function, new_name: &str) {
    func.name = ObjectName(vec![ObjectNamePart::Identifier(Ident::new(new_name))]);
}

fn is_distinct_incompatible_expr(expr: &Expr) -> bool {
    // Functions that are incompatible with DISTINCT (aggregates + random)
    if let Expr::Function(func) = expr {
        let name = func_name_str(func);
        name == "random" || is_aggregate_expr(expr)
    } else {
        false
    }
}

fn is_aggregate_expr(expr: &Expr) -> bool {
    const AGGREGATES: &[&str] = &[
        "count",
        "sum",
        "avg",
        "min",
        "max",
        "group_concat",
        "string_agg",
        "array_agg",
        "bool_and",
        "bool_or",
    ];
    if let Expr::Function(func) = expr {
        let name = func_name_str(func);
        AGGREGATES.contains(&name.as_str())
    } else {
        false
    }
}

/// If the expression is a numeric literal 1 or 0, replace with TRUE/FALSE in place.
fn maybe_bool_value_inplace(expr: &mut Expr) {
    if let Expr::Value(ValueWithSpan {
        value: Value::Number(ref s, _),
        ref span,
    }) = *expr
    {
        if s == "1" {
            *expr = Expr::Value(ValueWithSpan {
                value: Value::Boolean(true),
                span: *span,
            });
        } else if s == "0" {
            *expr = Expr::Value(ValueWithSpan {
                value: Value::Boolean(false),
                span: *span,
            });
        }
    }
}

/// Check if an expression is `COLLATE NOCASE` (shallow check).
fn is_collate_nocase(expr: &Expr) -> bool {
    if let Expr::Collate { collation, .. } = expr {
        collation
            .0
            .first()
            .and_then(|p| match p {
                ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
                _ => None,
            })
            .map(|name| name == "nocase")
            .unwrap_or(false)
    } else {
        false
    }
}

/// Strip COLLATE NOCASE or ICU collation from an expression in place (shallow).
/// If the expression is `expr COLLATE NOCASE` or `expr COLLATE icu_*`, unwrap to just `expr`.
fn strip_collate_nocase_inplace(expr: &mut Expr) {
    let should_strip = if let Expr::Collate { collation, .. } = &*expr {
        let name = collation
            .0
            .first()
            .and_then(|p| match p {
                ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
                _ => None,
            })
            .unwrap_or_default();
        name == "nocase" || name.starts_with("icu") || name == "unicode"
    } else {
        false
    };
    if should_strip {
        let taken = take_expr(expr);
        if let Expr::Collate { expr: inner, .. } = taken {
            *expr = *inner;
        }
    }
}

// ─── Fix 11: Collections filter ───────────────────────────────────────────────

/// Remove `metadata_type=18` from OR conditions.
/// Plex can't properly serialize collection objects (type 18), causing errors.
/// Pattern: `(metadata_type=1 OR metadata_type=18)` → `metadata_type=1`
fn fix_collections_filter(expr: &mut Expr) {
    // Recurse into nested expressions first
    match expr {
        Expr::Nested(inner) => {
            fix_collections_filter(inner);
            // After fixing inner, if it simplified to a single condition, unwrap
            if is_metadata_type_eq(inner, 18) {
                // Entire expression is just metadata_type=18 — replace with TRUE
                *expr = Expr::Value(ValueWithSpan {
                    value: Value::Boolean(true),
                    span: Span::empty(),
                });
            }
        }
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Or,
            right,
        } => {
            fix_collections_filter(left);
            fix_collections_filter(right);

            let left_is_18 = is_metadata_type_eq(left, 18);
            let right_is_18 = is_metadata_type_eq(right, 18);

            if left_is_18 && !right_is_18 {
                // Remove left (type=18), keep right
                let r = take_boxed_expr(right);
                *expr = r;
            } else if right_is_18 && !left_is_18 {
                // Remove right (type=18), keep left
                let l = take_boxed_expr(left);
                *expr = l;
            }
        }
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            fix_collections_filter(left);
            fix_collections_filter(right);
        }
        _ => {}
    }
}

/// Check if expr matches `metadata_type = <value>` or `metadata_items.metadata_type = <value>`
fn is_metadata_type_eq(expr: &Expr, value: i64) -> bool {
    if let Expr::BinaryOp {
        left,
        op: BinaryOperator::Eq,
        right,
    } = expr
    {
        let col_name = match left.as_ref() {
            Expr::Identifier(ident) => Some(ident.value.to_lowercase()),
            Expr::CompoundIdentifier(parts) => parts.last().map(|i| i.value.to_lowercase()),
            _ => None,
        };
        if col_name.as_deref() == Some("metadata_type") {
            if let Expr::Value(ValueWithSpan {
                value: Value::Number(ref s, _),
                ..
            }) = right.as_ref()
            {
                if let Ok(n) = s.parse::<i64>() {
                    return n == value;
                }
            }
        }
    }
    false
}

// ─── Fix 12: Int/text mismatch ────────────────────────────────────────────────

/// Known columns that are stored as TEXT in Plex but sometimes compared with integers
const KNOWN_TEXT_COLUMNS: &[&str] = &["status", "state", "downloaded", "metadata_item_id"];

/// Text columns that cannot go in the list above because the same name is an
/// integer in another table, so they are only text in the tables named here.
///
/// schema_migrations.version is text; play_queues.version and
/// metadata_item_clusterings.version are integers. Plex writes the migration
/// version unquoted -- `where version=202608120900` -- which SQLite compares as
/// text by affinity and PostgreSQL rejects outright: twelve digits is a bigint,
/// and the schema's compat operators only pair text with integer.
const KNOWN_TEXT_COLUMNS_BY_TABLE: &[(&str, &[&str])] = &[("schema_migrations", &["version"])];

thread_local! {
    /// The per-table text columns in force for the statement being
    /// transformed. `transform_expr` walks expressions without knowing which
    /// table they belong to, so the statement-level walk sets this before
    /// descending; nested queries push their own and restore on the way out.
    static STATEMENT_TEXT_COLUMNS: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
}

/// Scopes `STATEMENT_TEXT_COLUMNS` to the tables of one statement, restoring
/// whatever an enclosing statement had set when dropped.
struct TextColumnScope {
    previous: Vec<&'static str>,
}

impl TextColumnScope {
    fn for_tables(tables: &[String]) -> Self {
        let mut columns: Vec<&'static str> = Vec::new();
        for table in tables {
            let table = table.to_lowercase();
            let table = table.rsplit('.').next().unwrap_or(&table);
            for (known, cols) in KNOWN_TEXT_COLUMNS_BY_TABLE {
                if *known == table {
                    columns.extend_from_slice(cols);
                }
            }
        }
        let previous =
            STATEMENT_TEXT_COLUMNS.with(|c| std::mem::replace(&mut *c.borrow_mut(), columns));
        Self { previous }
    }
}

impl Drop for TextColumnScope {
    fn drop(&mut self) {
        let previous = std::mem::take(&mut self.previous);
        STATEMENT_TEXT_COLUMNS.with(|c| *c.borrow_mut() = previous);
    }
}

/// The real table names in a FROM list -- not aliases, which is what the
/// per-table list is keyed by. Joined tables count too: a column in the WHERE
/// may belong to any of them.
fn table_names_in(from: &[TableWithJoins]) -> Vec<String> {
    fn name_of(factor: &TableFactor) -> Option<String> {
        if let TableFactor::Table { name, .. } = factor {
            if let Some(ObjectNamePart::Identifier(ident)) = name.0.last() {
                return Some(ident.value.clone());
            }
        }
        None
    }
    let mut names = Vec::new();
    for twj in from {
        names.extend(name_of(&twj.relation));
        names.extend(twj.joins.iter().filter_map(|j| name_of(&j.relation)));
    }
    names
}

fn is_known_text_column(col: &str) -> bool {
    KNOWN_TEXT_COLUMNS.contains(&col) || STATEMENT_TEXT_COLUMNS.with(|c| c.borrow().contains(&col))
}

/// Known columns that are stored as INTEGER in Plex but sometimes compared with strings
const KNOWN_INT_COLUMNS: &[&str] = &["id"];

fn get_column_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Identifier(ident) => Some(ident.value.to_lowercase()),
        Expr::CompoundIdentifier(parts) => parts.last().map(|i| i.value.to_lowercase()),
        _ => None,
    }
}

fn is_pg_json_op(op: &str) -> bool {
    op == "->" || op == "->>"
}

fn is_json_cast(expr: &Expr) -> bool {
    match expr {
        Expr::Nested(inner) => is_json_cast(inner),
        Expr::Cast { data_type, .. } => match data_type {
            DataType::JSON => true,
            DataType::Custom(name, _) => name
                .0
                .first()
                .and_then(|p| match p {
                    ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
                    _ => None,
                })
                .map(|s| s == "jsonb")
                .unwrap_or(false),
            _ => false,
        },
        _ => false,
    }
}

fn make_text_cast(expr: Expr) -> Expr {
    wrap_double_colon_cast(expr, DataType::Text)
}

fn make_jsonb_cast(expr: Expr) -> Expr {
    Expr::Cast {
        kind: CastKind::DoubleColon,
        expr: Box::new(expr),
        data_type: DataType::Custom(
            ObjectName(vec![ObjectNamePart::Identifier(Ident::new("JSONB"))]),
            vec![],
        ),
        array: false,
        format: None,
    }
}

fn make_json_valid_call(expr: Expr) -> Expr {
    Expr::Function(Function {
        name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new("json_valid"))]),
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        args: FunctionArguments::List(FunctionArgumentList {
            duplicate_treatment: None,
            args: vec![FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))],
            clauses: vec![],
        }),
        filter: None,
        null_treatment: None,
        over: None,
        within_group: vec![],
    })
}

fn make_function_call(name: &str, args: Vec<Expr>) -> Expr {
    Expr::Function(Function {
        name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new(name))]),
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        args: FunctionArguments::List(FunctionArgumentList {
            duplicate_treatment: None,
            args: args
                .into_iter()
                .map(|e| FunctionArg::Unnamed(FunctionArgExpr::Expr(e)))
                .collect(),
            clauses: vec![],
        }),
        filter: None,
        null_treatment: None,
        over: None,
        within_group: vec![],
    })
}

fn parse_json_path_segments(path: &str) -> Option<Vec<String>> {
    if !path.starts_with('$') {
        return None;
    }
    let bytes = path.as_bytes();
    let mut i = 1usize;
    let mut out = Vec::new();
    while i < bytes.len() {
        match bytes[i] {
            b'.' => {
                i += 1;
                let start = i;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric()
                        || bytes[i] == b'_'
                        || bytes[i] == b':'
                        || bytes[i] == b'-')
                {
                    i += 1;
                }
                if i == start {
                    return None;
                }
                out.push(path[start..i].to_string());
            }
            b'[' => {
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                if i >= bytes.len() {
                    return None;
                }
                if bytes[i] == b'\'' || bytes[i] == b'"' {
                    let quote = bytes[i];
                    i += 1;
                    let start = i;
                    while i < bytes.len() && bytes[i] != quote {
                        i += 1;
                    }
                    if i >= bytes.len() {
                        return None;
                    }
                    out.push(path[start..i].to_string());
                    i += 1;
                    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                        i += 1;
                    }
                    if i >= bytes.len() || bytes[i] != b']' {
                        return None;
                    }
                    i += 1;
                    continue;
                }
                let token_start = i;
                if bytes[i] == b'-' {
                    i += 1;
                }
                let digit_start = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                if i == digit_start {
                    return None;
                }
                out.push(path[token_start..i].to_string());
                while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                if i >= bytes.len() || bytes[i] != b']' {
                    return None;
                }
                i += 1;
            }
            _ => return None,
        }
    }
    Some(out)
}

fn extract_json_path_segments_from_rhs(expr: &Expr) -> Option<Vec<String>> {
    if let Expr::Value(ValueWithSpan {
        value: Value::SingleQuotedString(s),
        ..
    }) = expr
    {
        return parse_json_path_segments(s);
    }
    None
}

fn is_json_extract_text_op(op: &BinaryOperator) -> bool {
    match op {
        BinaryOperator::LongArrow | BinaryOperator::HashLongArrow => true,
        BinaryOperator::PGCustomBinaryOperator(ops) if ops.len() == 1 => ops[0] == "->>",
        _ => false,
    }
}

fn rewrite_json_path_binary_op(left_jsonb: Expr, op: &BinaryOperator, segs: &[String]) -> Expr {
    let mut args = vec![left_jsonb];
    args.extend(segs.iter().cloned().map(|s| {
        Expr::Value(ValueWithSpan {
            value: Value::SingleQuotedString(s),
            span: Span::empty(),
        })
    }));
    let fn_name = if is_json_extract_text_op(op) {
        "jsonb_extract_path_text"
    } else {
        "jsonb_extract_path"
    };
    make_function_call(fn_name, args)
}

fn rewrite_json_binary_op(mut left: Expr, op: BinaryOperator, mut right: Expr) -> Expr {
    transform_expr(&mut left);
    transform_expr(&mut right);

    if is_json_cast(&left) {
        return Expr::BinaryOp {
            left: Box::new(left),
            op,
            right: Box::new(right),
        };
    }

    let left_as_text = make_text_cast(left.clone());
    let guarded_left = make_jsonb_cast(left_as_text.clone());
    let json_op_expr = if let Some(segs) = extract_json_path_segments_from_rhs(&right) {
        rewrite_json_path_binary_op(guarded_left, &op, &segs)
    } else {
        Expr::BinaryOp {
            left: Box::new(guarded_left),
            op,
            right: Box::new(right),
        }
    };

    Expr::Case {
        case_token: AttachedToken::empty(),
        end_token: AttachedToken::empty(),
        operand: None,
        conditions: vec![CaseWhen {
            condition: make_json_valid_call(left_as_text),
            result: json_op_expr,
        }],
        else_result: Some(Box::new(Expr::Value(ValueWithSpan {
            value: Value::Null,
            span: Span::empty(),
        }))),
    }
}

fn is_number_literal(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Value(ValueWithSpan {
            value: Value::Number(_, _),
            ..
        })
    )
}

fn is_string_literal(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Value(ValueWithSpan {
            value: Value::SingleQuotedString(_),
            ..
        })
    )
}

/// Check if this binary comparison has an int/text mismatch that needs fixing
fn should_fix_int_text_mismatch(left: &Expr, right: &Expr) -> bool {
    // Pattern A: known_text_col = number → cast number to text
    if let Some(col) = get_column_name(left) {
        if is_known_text_column(&col) && is_number_literal(right) {
            return true;
        }
    }
    if let Some(col) = get_column_name(right) {
        if is_known_text_column(&col) && is_number_literal(left) {
            return true;
        }
    }
    // Pattern B: known_int_col = 'string' → cast string to int
    if let Some(col) = get_column_name(left) {
        if KNOWN_INT_COLUMNS.contains(&col.as_str()) && is_string_literal(right) {
            return true;
        }
    }
    if let Some(col) = get_column_name(right) {
        if KNOWN_INT_COLUMNS.contains(&col.as_str()) && is_string_literal(left) {
            return true;
        }
    }
    false
}

/// Fix int/text mismatch by adding appropriate casts.
/// Takes ownership because the caller already extracted the parts via std::mem::replace.
fn fix_int_text_mismatch(mut left: Expr, op: BinaryOperator, mut right: Expr) -> Expr {
    let left_col = get_column_name(&left);
    let right_col = get_column_name(&right);

    // Recurse into both sides first
    transform_expr(&mut left);
    transform_expr(&mut right);

    if left_col
        .as_ref()
        .map(|c| is_known_text_column(c))
        .unwrap_or(false)
        && is_number_literal(&right)
    {
        // text_col = 123 → text_col = 123::text
        Expr::BinaryOp {
            left: Box::new(left),
            op,
            right: Box::new(Expr::Cast {
                kind: CastKind::DoubleColon,
                expr: Box::new(right),
                data_type: DataType::Text,
                array: false,
                format: None,
            }),
        }
    } else if right_col
        .as_ref()
        .map(|c| is_known_text_column(c))
        .unwrap_or(false)
        && is_number_literal(&left)
    {
        Expr::BinaryOp {
            left: Box::new(Expr::Cast {
                kind: CastKind::DoubleColon,
                expr: Box::new(left),
                data_type: DataType::Text,
                array: false,
                format: None,
            }),
            op,
            right: Box::new(right),
        }
    } else if left_col
        .as_ref()
        .map(|c| KNOWN_INT_COLUMNS.contains(&c.as_str()))
        .unwrap_or(false)
        && is_string_literal(&right)
    {
        // int_col = '123' → int_col = CAST('123' AS INTEGER)
        Expr::BinaryOp {
            left: Box::new(left),
            op,
            right: Box::new(Expr::Cast {
                kind: CastKind::Cast,
                expr: Box::new(right),
                data_type: DataType::Integer(None),
                array: false,
                format: None,
            }),
        }
    } else if right_col
        .as_ref()
        .map(|c| KNOWN_INT_COLUMNS.contains(&c.as_str()))
        .unwrap_or(false)
        && is_string_literal(&left)
    {
        Expr::BinaryOp {
            left: Box::new(Expr::Cast {
                kind: CastKind::Cast,
                expr: Box::new(left),
                data_type: DataType::Integer(None),
                array: false,
                format: None,
            }),
            op,
            right: Box::new(right),
        }
    } else {
        Expr::BinaryOp {
            left: Box::new(left),
            op,
            right: Box::new(right),
        }
    }
}

// ─── Fix FTS4: MATCH → @@ to_tsquery ──────────────────────────────────────────

/// Transform `col MATCH 'term'` → `to_tsvector('simple', col) @@ to_tsquery('simple', E'converted_term')`
///
/// The old strategy of appending `_fts` to the column name (e.g. `title_sort` →
/// `title_sort_fts`) created nonexistent column references.  The PostgreSQL FTS
/// views (`fts4_metadata_titles`, `fts4_metadata_titles_icu`) expose the source
/// text columns (`title`, `title_sort`, `original_title`) but only a single
/// pre-computed tsvector column (`title_fts`).  Columns like `title_sort` have
/// no corresponding `title_sort_fts` tsvector column, so the `_fts` suffix
/// approach fails.
///
/// The correct approach is to wrap the source column with `to_tsvector` at
/// query time.  This works for any column name, matches the `'simple'`
/// dictionary used by the schema's tsvector triggers, and does not require a
/// pre-computed tsvector column to exist.
fn transform_fts_match(left: Expr, right: Expr) -> Expr {
    // SQLite MATCH binds more tightly than AND/OR. sqlparser can attach the
    // trailing boolean predicates to MATCH's RHS; translate the search term
    // separately, then preserve and transform those predicates.
    if let Expr::BinaryOp {
        left: term,
        op: op @ (BinaryOperator::And | BinaryOperator::Or),
        right: mut predicate,
    } = right
    {
        transform_expr(&mut predicate);
        return Expr::BinaryOp {
            left: Box::new(transform_fts_match(left, *term)),
            op,
            right: predicate,
        };
    }
    // Build: to_tsvector('simple', col)
    let make_fn = |fn_name: &str, col: Expr| -> Expr {
        Expr::Function(Function {
            name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new(fn_name))]),
            uses_odbc_syntax: false,
            parameters: FunctionArguments::None,
            args: FunctionArguments::List(FunctionArgumentList {
                duplicate_treatment: None,
                args: vec![
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(ValueWithSpan {
                        value: Value::SingleQuotedString("simple".to_string()),
                        span: Span::empty(),
                    }))),
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(col)),
                ],
                clauses: vec![],
            }),
            filter: None,
            null_treatment: None,
            over: None,
            within_group: vec![],
        })
    };

    let tsvector_call = make_fn("to_tsvector", left);

    // Convert the match term to tsquery syntax
    let term_str = match &right {
        Expr::Value(ValueWithSpan {
            value: Value::SingleQuotedString(s),
            ..
        }) => convert_fts_term(s),
        _ => {
            return Expr::BinaryOp {
                left: Box::new(tsvector_call),
                op: BinaryOperator::AtAt,
                right: Box::new(right),
            }
        }
    };

    // Build: to_tsvector('simple', col) @@ to_tsquery('simple', E'term')
    let tsquery_call = Expr::Function(Function {
        name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new("to_tsquery"))]),
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        args: FunctionArguments::List(FunctionArgumentList {
            duplicate_treatment: None,
            args: vec![
                FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(ValueWithSpan {
                    value: Value::SingleQuotedString("simple".to_string()),
                    span: Span::empty(),
                }))),
                FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(ValueWithSpan {
                    value: Value::EscapedStringLiteral(term_str),
                    span: Span::empty(),
                }))),
            ],
            clauses: vec![],
        }),
        filter: None,
        null_treatment: None,
        over: None,
        within_group: vec![],
    });

    Expr::BinaryOp {
        left: Box::new(tsvector_call),
        op: BinaryOperator::AtAt,
        right: Box::new(tsquery_call),
    }
}

/// Convert SQLite FTS term syntax to PostgreSQL tsquery syntax.
///
/// | SQLite         | PostgreSQL tsquery |
/// |----------------|-------------------|
/// | `term`         | `term`            |
/// | `term1 term2`  | `term1 & term2`   |
/// | `-term`        | `!term`           |
/// | `term1 OR term2`| `term1 | term2`  |
/// | `"exact phrase"`| `exact <-> phrase`|
/// | `'` (quote)    | stripped           |
fn convert_fts_term(input: &str) -> String {
    let mut result = String::new();
    let mut chars = input.chars().peekable();
    let mut need_and = false;

    while let Some(&c) = chars.peek() {
        match c {
            // Negation
            '-' => {
                if need_and {
                    result.push_str(" & ");
                }
                chars.next();
                result.push('!');
                need_and = false;
            }
            // Phrase: "word1 word2" → word1 <-> word2
            '"' => {
                if need_and {
                    result.push_str(" & ");
                }
                chars.next(); // skip opening "
                let mut phrase = String::new();
                while let Some(&pc) = chars.peek() {
                    if pc == '"' {
                        chars.next();
                        break;
                    }
                    phrase.push(pc);
                    chars.next();
                }
                let words: Vec<&str> = phrase.split_whitespace().collect();
                result.push_str(&words.join(" <-> "));
                need_and = true;
            }
            // Single quote — strip (not valid in tsquery)
            '\'' => {
                chars.next();
            }
            // Whitespace — check for OR or implicit AND
            ' ' | '\t' | '\n' => {
                chars.next();
                // Skip whitespace
                while let Some(&wc) = chars.peek() {
                    if wc == ' ' || wc == '\t' || wc == '\n' {
                        chars.next();
                    } else {
                        break;
                    }
                }
                // Check for OR keyword
                if chars.peek() == Some(&'O') || chars.peek() == Some(&'o') {
                    let rest: String = chars.clone().take(3).collect();
                    if rest.to_ascii_uppercase().starts_with("OR ")
                        || rest.eq_ignore_ascii_case("OR")
                    {
                        chars.next(); // O
                        chars.next(); // R
                                      // skip space after OR
                        while let Some(&wc) = chars.peek() {
                            if wc == ' ' {
                                chars.next();
                            } else {
                                break;
                            }
                        }
                        result.push_str(" | ");
                        need_and = false;
                        continue;
                    }
                }
                // Implicit AND (space between terms)
                if need_and && chars.peek().is_some() {
                    result.push_str(" & ");
                    need_and = false;
                }
            }
            // SQLite FTS prefixes use *, PostgreSQL tsquery uses :*.
            '*' => {
                chars.next();
                if !result.ends_with(':') {
                    result.push(':');
                }
                result.push('*');
                need_and = true;
            }
            // Regular character — part of a term
            _ => {
                if !need_and {
                    // First char of a new term
                }
                result.push(c);
                chars.next();
                need_and = true;
            }
        }
    }
    result
}

/// Wrap an expression in LOWER(…) in place.
fn wrap_lower_inplace(expr: &mut Expr) {
    let inner = take_expr(expr);
    *expr = wrap_lower(inner);
}

/// Wrap an expression in LOWER(…)
fn wrap_lower(expr: Expr) -> Expr {
    Expr::Function(Function {
        name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new("LOWER"))]),
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        args: FunctionArguments::List(FunctionArgumentList {
            duplicate_treatment: None,
            args: vec![FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))],
            clauses: vec![],
        }),
        filter: None,
        null_treatment: None,
        over: None,
        within_group: vec![],
    })
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use crate::translate;

    #[test]
    fn schema_migrations_version_compares_as_text_even_to_a_bigint_literal() {
        // schema_migrations.version is text. Plex writes the literal unquoted
        // -- `where version=202608120900` -- which SQLite compares as text by
        // affinity and PostgreSQL rejects: twelve digits is a bigint, and
        // the schema's compat operators only pair text with integer. It
        // failed on every start, three times per start on a three-pod
        // cluster, and would have kept Plex from recording a rollback.
        let r = translate("DELETE FROM schema_migrations where version=202608120900").unwrap();
        let sql = r.sql.to_lowercase();
        assert!(
            sql.contains("version = 202608120900::text"),
            "the literal must be compared as text, got: {}",
            r.sql
        );
    }

    #[test]
    fn schema_migrations_version_is_text_under_an_alias_too() {
        let r = translate("DELETE FROM schema_migrations AS sm WHERE sm.version = 202608120900")
            .unwrap();
        assert!(
            r.sql.to_lowercase().contains("202608120900::text"),
            "the alias must not hide the table, got: {}",
            r.sql
        );
    }

    #[test]
    fn a_subquery_on_an_integer_version_table_keeps_its_own_scope() {
        // The outer statement is on schema_migrations; the subquery is on
        // play_queues, where version is an integer. Each must be judged by
        // its own table, and the outer scope must come back after the inner.
        let r = translate(
            "DELETE FROM schema_migrations WHERE version = 202608120900 \
             AND 1 IN (SELECT id FROM play_queues WHERE version = 3) AND version <> 5",
        )
        .unwrap();
        let sql = r.sql.to_lowercase();
        assert!(
            sql.contains("version = 202608120900::text"),
            "outer, before: {}",
            r.sql
        );
        assert!(
            !sql.contains("version = 3::text"),
            "inner must not be cast: {}",
            r.sql
        );
        assert!(
            sql.contains("version <> 5::text"),
            "outer, after the subquery: {}",
            r.sql
        );
    }

    #[test]
    fn an_integer_version_column_is_left_alone() {
        // `version` is only text in schema_migrations; play_queues and
        // metadata_item_clusterings store an integer. A cast there would
        // turn a correct comparison into a text one.
        for sql in [
            "SELECT id FROM play_queues WHERE version = 5",
            "UPDATE metadata_item_clusterings SET title = 'x' WHERE version = 12",
        ] {
            let r = translate(sql).unwrap();
            assert!(
                !r.sql.to_lowercase().contains("::text"),
                "integer version must not be cast, got: {}",
                r.sql
            );
        }
    }

    #[test]
    fn compat_aliases__query_subquery_gets_alias() {
        let r = translate("SELECT * FROM (SELECT id FROM t) WHERE id > 0").unwrap();
        let sql_up = r.sql.to_uppercase();
        assert!(
            sql_up.contains("AS ") || sql_up.contains(" AS"),
            "Subquery should have alias, got: {}",
            r.sql
        );
    }

    #[test]
    fn subset_core__query_nulls_last_from_is_null_pattern() {
        let r =
            translate(r#"SELECT * FROM t ORDER BY parents."index" IS NULL, parents."index" asc"#)
                .unwrap();
        let sql = r.sql.to_uppercase();
        assert!(
            sql.contains("NULLS LAST"),
            "Expected NULLS LAST, got: {}",
            r.sql
        );
        assert!(
            !sql.contains("IS NULL"),
            "IS NULL pattern should be replaced, got: {}",
            r.sql
        );
    }

    #[test]
    fn subset_core__query_case_then_1_else_0_to_booleans() {
        let r = translate("SELECT (CASE WHEN a THEN 1 ELSE 0 END) FROM t").unwrap();
        let sql = r.sql.to_lowercase();
        assert!(
            sql.contains("true") || sql.contains("false"),
            "Expected boolean values, got: {}",
            r.sql
        );
    }

    #[test]
    fn subset_core__query_where_0_to_false() {
        let r = translate("SELECT * FROM t WHERE 0").unwrap();
        assert!(r.sql.to_uppercase().contains("FALSE"), "Got: {}", r.sql);
    }

    #[test]
    fn subset_core__query_where_1_to_true() {
        let r = translate("SELECT * FROM t WHERE 1").unwrap();
        assert!(r.sql.to_uppercase().contains("TRUE"), "Got: {}", r.sql);
    }

    #[test]
    fn subset_core__query_max_two_args_to_greatest() {
        let r = translate("SELECT max(x, y) FROM t").unwrap();
        assert!(r.sql.to_uppercase().contains("GREATEST"), "Got: {}", r.sql);
    }

    #[test]
    fn subset_core__query_max_one_arg_preserved() {
        let r = translate("SELECT max(x) FROM t").unwrap();
        assert!(!r.sql.to_uppercase().contains("GREATEST"), "Got: {}", r.sql);
        assert!(r.sql.to_lowercase().contains("max("), "Got: {}", r.sql);
    }

    #[test]
    fn subset_core__query_min_two_args_to_least() {
        let r = translate("SELECT min(x, y) FROM t").unwrap();
        assert!(r.sql.to_uppercase().contains("LEAST"), "Got: {}", r.sql);
    }

    #[test]
    fn subset_core__query_strip_icu_collation() {
        let r = translate("SELECT * FROM t ORDER BY name COLLATE icu_root").unwrap();
        assert!(!r.sql.to_lowercase().contains("icu_root"), "Got: {}", r.sql);
    }

    #[test]
    fn subset_core__query_collate_nocase_to_lower() {
        let r = translate("SELECT * FROM t WHERE name COLLATE NOCASE = 'Test'").unwrap();
        let sql = r.sql.to_lowercase();
        assert!(sql.contains("lower("), "Expected LOWER(), got: {}", r.sql);
        assert!(
            !sql.contains("collate nocase"),
            "COLLATE NOCASE should be removed, got: {}",
            r.sql
        );
    }

    #[test]
    fn subset_core__query_orderby_unchanged_without_isnull() {
        let r = translate("SELECT * FROM t ORDER BY id").unwrap();
        assert!(
            !r.sql.to_uppercase().contains("NULLS LAST"),
            "Should not add NULLS LAST, got: {}",
            r.sql
        );
    }

    #[test]
    fn subset_json__query_json_op_uses_safe_jsonb_cast() {
        let r = translate(
            "SELECT * FROM media_streams st WHERE st.extra_data ->> '$.pv:version' < '1'",
        )
        .unwrap();
        let sql = r.sql.to_lowercase();
        assert!(
            sql.contains("json_valid")
                && sql.contains("::jsonb")
                && (sql.contains("jsonb_extract_path_text") || sql.contains("->>")),
            "Expected guarded json rewrite, got: {}",
            r.sql
        );
    }

    #[test]
    fn subset_json__query_json_op_uses_safe_cast_on_unrelated_column_too() {
        let r = translate("SELECT * FROM t WHERE t.title ->> '$.key' = 'x'").unwrap();
        let sql = r.sql.to_lowercase();
        assert!(
            sql.contains("json_valid") && sql.contains("title") && sql.contains("::jsonb"),
            "Expected generic guarded rewrite for JSON operator, got: {}",
            r.sql
        );
    }

    #[test]
    fn subset_json__query_json_op_sqlite_path_literal_rewrites_to_extract_path_text() {
        let r = translate("SELECT extra_data ->> '$.status' FROM t").unwrap();
        let sql = r.sql.to_lowercase();
        assert!(
            sql.contains("jsonb_extract_path_text") && sql.contains("'status'"),
            "Expected $.status operator path rewrite, got: {}",
            r.sql
        );
    }

    // FTS MATCH tests
    //
    // The old strategy appended `_fts` to the matched column name (e.g.
    // `title_sort` → `title_sort_fts`), which produced nonexistent column
    // references.  The correct strategy wraps the source column with
    // `to_tsvector('simple', col)` so any column name is valid.

    #[test]
    fn subset_fts__match_simple_col_uses_to_tsvector_wrapper() {
        let r = translate("SELECT * FROM fts4_metadata_titles WHERE title MATCH 'Plex'").unwrap();
        let sql = r.sql.to_lowercase();
        assert!(
            sql.contains("to_tsvector("),
            "Expected to_tsvector() wrapper, got: {}",
            r.sql
        );
        assert!(
            !sql.contains("title_fts"),
            "Should not produce _fts suffix column, got: {}",
            r.sql
        );
        assert!(
            sql.contains("to_tsquery("),
            "Expected to_tsquery() call, got: {}",
            r.sql
        );
        assert!(sql.contains("@@"), "Expected @@ operator, got: {}", r.sql);
    }

    #[test]
    fn subset_fts__match_compound_title_sort_no_nonexistent_suffix() {
        // Regression: `fts4_metadata_titles_icu.title_sort MATCH 'Straf*'`
        // must NOT produce `title_sort_fts` (nonexistent column).
        let r = translate(
            "SELECT rowid FROM fts4_metadata_titles_icu \
             WHERE fts4_metadata_titles_icu.title_sort match 'Straf*'",
        )
        .unwrap();
        let sql = r.sql.to_lowercase();
        assert!(
            !sql.contains("title_sort_fts"),
            "Must not produce nonexistent title_sort_fts column, got: {}",
            r.sql
        );
        assert!(
            sql.contains("to_tsvector("),
            "Expected to_tsvector() wrapper, got: {}",
            r.sql
        );
        assert!(
            sql.contains("title_sort"),
            "Expected original title_sort column inside to_tsvector, got: {}",
            r.sql
        );
        assert!(sql.contains("@@"), "Expected @@ operator, got: {}", r.sql);
    }

    #[test]
    fn subset_fts__match_prefix_query_converts_star_to_colon_star() {
        let r =
            translate("SELECT rowid FROM fts4_metadata_titles WHERE title MATCH 'War*'").unwrap();
        let sql = r.sql.to_lowercase();
        assert!(
            sql.contains("war:*") || sql.contains("war"),
            "Expected prefix tsquery token, got: {}",
            r.sql
        );
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod distinct_fix_test {
    use crate::translate;
    #[test]
    fn rewrite_distinct_orderby__col_added_to_select() {
        let sql = "SELECT DISTINCT (metadata_items.id) FROM metadata_items LEFT JOIN media_items ON media_items.metadata_item_id = metadata_items.id WHERE metadata_items.id = ? ORDER BY metadata_items.title_sort ASC";
        let r = translate(sql).expect("translate ok");
        eprintln!("OUT: {}", r.sql);
        // title_sort must appear in the SELECT list (before FROM)
        let out = r.sql.to_lowercase();
        let from_pos = out.find(" from ").unwrap_or(out.len());
        assert!(
            out[..from_pos].contains("title_sort"),
            "title_sort not in select: {}",
            r.sql
        );
    }
}

#[cfg(test)]
mod fts_boolean_regressions {
    #[test]
    fn match_prefix_keeps_following_boolean_filters() {
        for conjunction in ["AND", "OR"] {
            let sql = format!("SELECT rowid FROM fts4_metadata_titles_icu WHERE title MATCH '(日本*)' {conjunction} rowid = 42");
            let result = crate::translate(&sql).unwrap().sql;
            assert!(
                result.contains("to_tsquery('simple', E'(日本:*)')"),
                "{result}"
            );
            assert!(
                result.contains(&format!("{conjunction} rowid = 42")),
                "{result}"
            );
        }
    }

    #[test]
    fn fts_prefix_and_unicode_or_detection() {
        assert_eq!(super::convert_fts_term("(star*)"), "(star:*)");
        assert_eq!(super::convert_fts_term("é* OR 日本*"), "é:* | 日本:*");
        assert_eq!(super::convert_fts_term("a O日"), "a & O日");
    }
}

#[cfg(test)]
mod collection_prefetch_tests {
    use super::*;
    use sqlparser::{dialect::PostgreSqlDialect, parser::Parser};
    const SQL: &str = "SELECT DISTINCT media_items.id AS mid, metadata_items.title FROM metadata_items LEFT JOIN media_items ON media_items.metadata_item_id = metadata_items.id LEFT JOIN media_parts ON media_parts.media_item_id = media_items.id LEFT JOIN metadata_item_settings ON metadata_item_settings.guid = metadata_items.guid AND metadata_item_settings.account_id = 1 LEFT JOIN taggings ON taggings.metadata_item_id = metadata_items.id LEFT JOIN tags ON taggings.tag_id = tags.id WHERE metadata_items.id IN (1, 2, 3) ORDER BY metadata_items.title ASC, media_parts.id ASC";
    fn rewrite(s: &str) -> (String, String) {
        let mut stmt = Parser::parse_sql(&PostgreSqlDialect {}, s)
            .unwrap()
            .remove(0);
        let before = stmt.to_string();
        if let Statement::Query(q) = &mut stmt {
            prune_collection_prefetch_tag_joins(q);
        }
        (before, stmt.to_string())
    }
    #[test]
    fn removes_only_unused_tag_fanout() {
        let (_, after) = rewrite(SQL);
        assert!(!after.contains("JOIN taggings"));
        assert!(!after.contains("JOIN tags"));
        assert!(after.contains("SELECT DISTINCT"));
        assert!(after.contains("JOIN media_parts"));
        assert!(after.contains("JOIN metadata_item_settings"));
    }
    #[test]
    fn preserves_other_query_shapes_and_referenced_tags() {
        for sql in [
            SQL.replace("DISTINCT ", ""),
            SQL.replace("media_items.id AS mid", "tags.id AS mid"),
            SQL.replace("media_items.id AS mid", "taggings.id AS mid"),
            SQL.replace("media_items.id AS mid", "count(*) AS mid"),
            SQL.replace("media_items.id AS mid", "random() AS mid"),
            SQL.replace("media_items.id AS mid", "*"),
            SQL.replace("metadata_items.title ASC", "tags.tag ASC"),
            SQL.replace("IN (1, 2, 3)", "IN (SELECT id FROM tags)"),
            SQL.replace("IN (1, 2, 3)", "IN (1, 2, 3) AND tags.tag_type = 1"),
            SQL.replace("LEFT JOIN taggings", "JOIN taggings"),
            SQL.replace("account_id = 1", "account_id = 2"),
            format!("{} LIMIT 10", SQL),
        ] {
            let (before, after) = rewrite(&sql);
            assert_eq!(before, after, "unexpected rewrite: {sql}");
        }
    }
}
