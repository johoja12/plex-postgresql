use sqlparser::ast::helpers::attached_token::AttachedToken;
/// Module: query
///
/// Miscellaneous query fixups for SQLite → PostgreSQL translation:
///   1. Subquery alias      — FROM (SELECT …) without alias → AS _subq
///   2. NULLS LAST/FIRST    — `ORDER BY col IS NULL [DESC], col` → `ORDER BY col NULLS LAST|FIRST`
///   3. DISTINCT + agg ORDER BY — `SELECT DISTINCT … ORDER BY count(*)` → remove DISTINCT
///   4. CASE booleans       — `CASE WHEN x THEN 1 ELSE 0 END` → TRUE/FALSE, in a boolean
///                            context only (WHERE, HAVING, ON, AND/OR, NOT); an aggregate,
///                            a projection or a comparison keeps the integers
///   5. WHERE 1 / WHERE 0   — → WHERE TRUE / WHERE FALSE
///   6. max(a,b) multi-arg  — → GREATEST(a,b)
///   7. min(a,b) multi-arg  — → LEAST(a,b)
///   8. COLLATE ICU strip   — remove icu_* collations
///   9. COLLATE NOCASE      — `x COLLATE NOCASE = y` → `LOWER(x) = LOWER(y)`
///  10. FTS4 MATCH          — `col MATCH term` → `to_tsvector('simple', plex.fold(col)) @@ plex.fts_query(term)`
///  11. Collections filter  — remove `metadata_type=18` from OR conditions
///  12. Affinity            — a text column compared with a number casts the number to
///                            text; an integer column compared with a string casts the
///                            string, or folds the comparison when the string is no
///                            integer (SQLite's affinity rules, from the schema's column
///                            types)
///  13. LIKE                — bare LIKE is ASCII case-insensitive in SQLite → ILIKE
///                            (unless `PRAGMA case_sensitive_like` was turned on)
///  14. GLOB                — `x GLOB pat` (SIMILAR TO after preprocessing) → `x ~ '^regex$'`
///  15. Aliases             — a projection alias used in WHERE or HAVING is replaced by
///                            its expression
///  16. LIMIT -1            — no limit; `LIMIT off, n` → `LIMIT n OFFSET off`
///  17. UPDATE … LIMIT n    — `WHERE ctid IN (SELECT ctid FROM t WHERE … LIMIT n)`
///
/// Every rewrite enters subqueries (IN, EXISTS, scalar) and JOIN … ON expressions.
use sqlparser::ast::*;
use sqlparser::tokenizer::Span;

use crate::rewriter::ast_utils::{take_boxed_expr, take_expr, wrap_double_colon_cast};
use crate::schema_columns::{ColumnKind, COLUMNS};
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
            let _scope = TableScope::enter(scoped_tables(std::slice::from_ref(&u.table)));
            fix_table_with_joins(&mut u.table);
            for assignment in &mut u.assignments {
                transform_expr(&mut assignment.value);
            }
            if let Some(sel) = &mut u.selection {
                transform_bool_expr(sel);
            }
            rewrite_update_limit(u);
        }
        Statement::Delete(d) => {
            let tables = match &d.from {
                FromTable::WithFromKeyword(t) | FromTable::WithoutKeyword(t) => scoped_tables(t),
            };
            let _scope = TableScope::enter(tables);
            if let Some(sel) = &mut d.selection {
                transform_bool_expr(sel);
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

    deduplicate_smart_tv_hub_pages(q);
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

    fix_limit_clause(q);
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
    let _scope = TableScope::enter(scoped_tables(&sel.from));

    // Fix 15: a projection alias in WHERE or HAVING stands for its expression.
    expand_projection_aliases(sel);

    // Fix FROM subqueries without alias, and every JOIN's ON expression
    for twj in &mut sel.from {
        fix_table_with_joins(twj);
    }

    // WHERE fixups
    if let Some(w) = &mut sel.selection {
        fix_where_numeric(w);
        fix_collections_filter(w);
        transform_bool_expr(w);
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
        transform_bool_expr(h);
    }
}

// ─── Fix 15: projection aliases in WHERE / HAVING ────────────────────────────

/// Plex's Live TV guide writes `min(begins_at, ends_at) as minimum_at …
/// where minimum_at > ?`. SQLite resolves the alias; PostgreSQL answers
/// "column minimum_at does not exist". The alias is replaced by its
/// expression, as HAVING already was -- unless a column of that name exists
/// on a table in scope, since SQLite resolves the column before the alias.
fn expand_projection_aliases(sel: &mut Select) {
    if sel.selection.is_none() && sel.having.is_none() {
        return;
    }
    let alias_map: Vec<(String, Expr)> = crate::groupby::build_alias_map(&sel.projection)
        .into_iter()
        .filter(|(alias, expr)| {
            let same_name = match expr {
                Expr::Identifier(ident) => ident.value.eq_ignore_ascii_case(alias),
                Expr::CompoundIdentifier(parts) => parts
                    .last()
                    .is_some_and(|p| p.value.eq_ignore_ascii_case(alias)),
                _ => false,
            };
            !same_name && !is_known_column(alias)
        })
        .collect();
    if alias_map.is_empty() {
        return;
    }
    if let Some(w) = &mut sel.selection {
        crate::groupby::expand_aliases_in_expr(w, &alias_map);
    }
    if let Some(h) = &mut sel.having {
        crate::groupby::expand_aliases_in_expr(h, &alias_map);
    }
}

// ─── Fix 1: Subquery alias, and JOIN … ON ────────────────────────────────────

fn fix_table_with_joins(twj: &mut TableWithJoins) {
    order_plex_descendant_media_joins(twj);
    fix_table_factor(&mut twj.relation);
    for join in &mut twj.joins {
        fix_table_factor(&mut join.relation);
        if let Some(on) = join_on_expr_mut(&mut join.join_operator) {
            transform_bool_expr(on);
        }
    }
}

// SQLite accepts Plex's INNER media join before the children/grandchildren
// alias its ON clause references. PostgreSQL requires that alias to be in
// scope already. Move only the exact INNER metadata hierarchy joins; never
// reorder an outer join or an unknown ON expression.
fn order_plex_descendant_media_joins(twj: &mut TableWithJoins) {
    fn table(tf: &TableFactor, name: &str, alias: Option<&str>) -> bool {
        matches!(tf, TableFactor::Table { name: n, alias: a, .. }
            if n.to_string() == name && a.as_ref().map(|v| v.name.value.as_str()) == alias)
    }
    fn column(e: &Expr, alias: &str, name: &str) -> bool {
        matches!(e, Expr::CompoundIdentifier(v) if v.len() == 2 && v[0].value == alias && v[1].value == name)
    }
    fn inner_eq(j: &Join, lhs: (&str, &str), rhs: (&str, &str)) -> bool {
        let on = match &j.join_operator {
            JoinOperator::Inner(JoinConstraint::On(e))
            | JoinOperator::Join(JoinConstraint::On(e)) => e,
            _ => return false,
        };
        matches!(on, Expr::BinaryOp { left, op: BinaryOperator::Eq, right }
            if column(left, lhs.0, lhs.1) && column(right, rhs.0, rhs.1))
    }
    if !table(&twj.relation, "metadata_items", None) {
        return;
    }
    if twj.joins.iter().any(|j| match &j.join_operator {
        JoinOperator::Inner(_) | JoinOperator::Join(_) => false,
        JoinOperator::LeftOuter(JoinConstraint::On(e)) | JoinOperator::Left(JoinConstraint::On(e)) => {
            !table(&j.relation, "metadata_item_settings", None)
                || e.to_string().replace(['`', '"'], "")
                    != "metadata_item_settings.guid = metadata_items.guid AND metadata_item_settings.account_id = 1"
        }
        _ => true,
    }) {
        return;
    }
    for descendant in ["children", "grandchildren"] {
        let Some(media) = twj.joins.iter().position(|j| {
            table(&j.relation, "media_items", None)
                && inner_eq(j, ("media_items", "metadata_item_id"), (descendant, "id"))
        }) else {
            continue;
        };
        let hierarchy = if descendant == "children" {
            vec![("children", "metadata_items")]
        } else {
            vec![
                ("children", "metadata_items"),
                ("grandchildren", "children"),
            ]
        };
        let mut indices = Vec::new();
        for (child, parent) in hierarchy {
            let Some(i) = twj.joins.iter().position(|j| {
                table(&j.relation, "metadata_items", Some(child))
                    && inner_eq(j, (child, "parent_id"), (parent, "id"))
            }) else {
                return;
            };
            indices.push(i);
        }
        // Already valid, or an unusual partially ordered chain: leave it alone.
        if indices.iter().any(|&i| i <= media) {
            continue;
        }
        let joins: Vec<_> = indices.iter().map(|&i| twj.joins[i].clone()).collect();
        indices.sort_unstable();
        for i in indices.into_iter().rev() {
            twj.joins.remove(i);
        }
        for (offset, join) in joins.into_iter().enumerate() {
            twj.joins.insert(media + offset, join);
        }
    }
}

fn join_on_expr_mut(op: &mut JoinOperator) -> Option<&mut Expr> {
    match op {
        JoinOperator::Join(JoinConstraint::On(e))
        | JoinOperator::Inner(JoinConstraint::On(e))
        | JoinOperator::Left(JoinConstraint::On(e))
        | JoinOperator::LeftOuter(JoinConstraint::On(e))
        | JoinOperator::Right(JoinConstraint::On(e))
        | JoinOperator::RightOuter(JoinConstraint::On(e))
        | JoinOperator::FullOuter(JoinConstraint::On(e))
        | JoinOperator::CrossJoin(JoinConstraint::On(e))
        | JoinOperator::Semi(JoinConstraint::On(e))
        | JoinOperator::LeftSemi(JoinConstraint::On(e))
        | JoinOperator::RightSemi(JoinConstraint::On(e))
        | JoinOperator::Anti(JoinConstraint::On(e))
        | JoinOperator::LeftAnti(JoinConstraint::On(e))
        | JoinOperator::RightAnti(JoinConstraint::On(e))
        | JoinOperator::StraightJoin(JoinConstraint::On(e)) => Some(e),
        _ => None,
    }
}

fn fix_table_factor(tf: &mut TableFactor) {
    match tf {
        TableFactor::Derived {
            subquery, alias, ..
        } => {
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
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => fix_table_with_joins(table_with_joins),
        _ => {}
    }
}

// ─── Fix 2: NULLS LAST / NULLS FIRST from IS NULL pattern ────────────────────

/// Detect: `ORDER BY expr IS [NOT] NULL [ASC|DESC], expr [ASC|DESC]`
/// Replace with: `ORDER BY expr [ASC|DESC] NULLS LAST|FIRST`
///
/// `IS NULL` is 1 for a null, so ascending puts nulls last and descending
/// first; `IS NOT NULL` the other way round.
fn fix_order_by_is_null(q: &mut Query) {
    let Some(ob) = &mut q.order_by else { return };
    let OrderByKind::Expressions(exprs) = &mut ob.kind else {
        return;
    };

    let len = exprs.len();
    if len < 2 {
        return;
    }

    let mut to_remove: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < len {
        let (col, nulls_first_when_asc) = match &exprs[i].expr {
            Expr::IsNull(inner) => (*inner.clone(), false),
            Expr::IsNotNull(inner) => (*inner.clone(), true),
            _ => {
                i += 1;
                continue;
            }
        };
        let descending = exprs[i].options.asc == Some(false);
        let nulls_first = nulls_first_when_asc != descending;
        if i + 1 < len {
            let next = &mut exprs[i + 1];
            if exprs_display_eq(&next.expr, &col) {
                next.options.nulls_first = Some(nulls_first);
                to_remove.push(i);
                i += 2;
                continue;
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

// ─── Fix 16: LIMIT ───────────────────────────────────────────────────────────

/// `LIMIT off, n` is `LIMIT n OFFSET off` (the placeholder pass numbers the
/// offset first, as SQLite binds it), and a negative LIMIT is no limit at all
/// in SQLite -- PostgreSQL refuses it, so the clause goes.
fn fix_limit_clause(q: &mut Query) {
    if matches!(q.limit_clause, Some(LimitClause::OffsetCommaLimit { .. })) {
        if let Some(LimitClause::OffsetCommaLimit { offset, limit }) = q.limit_clause.take() {
            q.limit_clause = Some(LimitClause::LimitOffset {
                limit: Some(limit),
                offset: Some(Offset {
                    value: offset,
                    rows: OffsetRows::None,
                }),
                limit_by: vec![],
            });
        }
    }
    if let Some(LimitClause::LimitOffset { limit, .. }) = &mut q.limit_clause {
        if limit.as_ref().is_some_and(is_negative_number) {
            *limit = None;
        }
    }
}

fn is_negative_number(expr: &Expr) -> bool {
    match expr {
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => is_number_literal(expr),
        Expr::Value(ValueWithSpan {
            value: Value::Number(s, _),
            ..
        }) => s.starts_with('-'),
        Expr::Nested(inner) => is_negative_number(inner),
        _ => false,
    }
}

// ─── Fix 17: UPDATE … LIMIT n ────────────────────────────────────────────────

/// PostgreSQL has no LIMIT on UPDATE. The rows are picked by ctid through a
/// subquery in the WHERE -- after placeholder numbering, so the SET
/// placeholders keep their order before the WHERE's (the text rewrite this
/// replaces hoisted the WHERE into a CTE ahead of the SET, and every bind
/// landed on the wrong column).
fn rewrite_update_limit(u: &mut Update) {
    let Some(limit) = u.limit.take() else { return };
    if is_negative_number(&limit) {
        return;
    }
    let selection = u.selection.take();
    let subquery = select_ctid_query(u.table.clone(), selection, limit);
    u.selection = Some(Expr::InSubquery {
        expr: Box::new(Expr::Identifier(Ident::new("ctid"))),
        subquery: Box::new(subquery),
        negated: false,
    });
}

fn select_ctid_query(table: TableWithJoins, selection: Option<Expr>, limit: Expr) -> Query {
    Query {
        with: None,
        body: Box::new(SetExpr::Select(Box::new(Select {
            select_token: AttachedToken::empty(),
            optimizer_hint: None,
            distinct: None,
            select_modifiers: None,
            top: None,
            top_before_distinct: false,
            projection: vec![SelectItem::UnnamedExpr(Expr::Identifier(Ident::new(
                "ctid",
            )))],
            exclude: None,
            into: None,
            from: vec![table],
            lateral_views: vec![],
            prewhere: None,
            selection,
            connect_by: vec![],
            group_by: GroupByExpr::Expressions(vec![], vec![]),
            cluster_by: vec![],
            distribute_by: vec![],
            sort_by: vec![],
            having: None,
            named_window: vec![],
            qualify: None,
            window_before_qualify: false,
            value_table_mode: None,
            flavor: SelectFlavor::Standard,
        }))),
        order_by: None,
        limit_clause: Some(LimitClause::LimitOffset {
            limit: Some(limit),
            offset: None,
            limit_by: vec![],
        }),
        fetch: None,
        locks: vec![],
        for_clause: None,
        settings: None,
        format_clause: None,
        pipe_operators: vec![],
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

const UNWATCHED_LEAF_COUNT: &str = "SELECT count(*) FROM metadata_items AS leaves JOIN metadata_items AS parents ON leaves.parent_id = parents.id LEFT JOIN metadata_item_settings ON metadata_item_settings.guid = leaves.guid AND metadata_item_settings.account_id = 1 WHERE (parents.parent_id = metadata_items.id OR parents.id = metadata_items.id) AND (metadata_item_settings.view_count = 0 OR metadata_item_settings.view_count IS NULL) AND leaves.metadata_type IN (1, 4, 10)";

// The smart hub joins each show to every episode, then deduplicates IDs in
// Plex after LIMIT/OFFSET. A 12-show hub can therefore fetch dozens of pages.
// Select each show once before paging, ordered by its first occurrence in the
// original descending episode order. Preserve PG's existing NULL-first DESC
// behavior explicitly; MAX alone would ignore NULL and change ordering.
fn deduplicate_smart_tv_hub_pages(q: &mut Query) {
    let SetExpr::Select(sel) = q.body.as_ref() else {
        return;
    };
    if sel.projection.len() != 1
        || !matches!(&sel.projection[0], SelectItem::UnnamedExpr(e) if expr_to_key(e) == "metadata_items.id")
    {
        return;
    }
    const FROM: &str = "metadata_items JOIN metadata_items AS children ON children.parent_id = metadata_items.id JOIN metadata_items AS grandchildren ON grandchildren.parent_id = children.id LEFT JOIN metadata_item_settings ON metadata_item_settings.guid = metadata_items.guid AND metadata_item_settings.account_id = 1";
    let predicate = format!("metadata_items.library_section_id IN (2) AND (({UNWATCHED_LEAF_COUNT}) >= 1 AND metadata_item_settings.rating = 10 AND (metadata_items.metadata_type = 2 OR metadata_items.metadata_type = 10)) AND metadata_items.metadata_type = 2");
    let Some(selection) = &sel.selection else {
        return;
    };
    if !selection.to_string().eq_ignore_ascii_case(&predicate) {
        return;
    }
    let Some(order) = &q.order_by else {
        return;
    };
    let order = order.to_string();
    if order != "ORDER BY grandchildren.added_at DESC, metadata_items.title_sort, metadata_items.title_sort" { return; }
    let prefix = format!(
        "SELECT {} FROM {FROM} WHERE {selection} {order} LIMIT ",
        sel.projection[0]
    );
    let rendered = q.to_string();
    let Some(page) = rendered.strip_prefix(&prefix) else {
        return;
    };
    let Some((limit, offset)) = page.split_once(" OFFSET ") else {
        return;
    };
    if limit.is_empty()
        || offset.is_empty()
        || !limit
            .bytes()
            .chain(offset.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return;
    }
    // The full string match above rejects DISTINCT, additional clauses, joins,
    // projections, expressions, other account/section/filter/order shapes.
    let sql = format!("SELECT metadata_items.id FROM {FROM} WHERE {selection} GROUP BY metadata_items.id, metadata_items.title_sort ORDER BY CASE WHEN bool_or(grandchildren.added_at IS NULL) THEN NULL ELSE max(grandchildren.added_at) END DESC, metadata_items.title_sort LIMIT {limit} OFFSET {offset}");
    if let Ok(mut statements) =
        sqlparser::parser::Parser::parse_sql(&sqlparser::dialect::PostgreSqlDialect {}, &sql)
    {
        if let Statement::Query(replacement) = statements.remove(0) {
            *q = *replacement;
        }
    }
}

// Plex pages smart TV hubs repeatedly while deduplicating shows expanded by
// episode joins. COUNT(*) >= 1 rescans every episode (and can trigger JIT) on
// every page. EXISTS is equivalent for this exact ungrouped, non-null count,
// lets PG apply the favourite filter first, and stops after one unwatched leaf.
// Match the whole known subquery so HAVING, OFFSET, windows, other aggregates
// and expression side effects cannot accidentally change meaning.
fn optimize_unwatched_leaf_exists(expr: &mut Expr) {
    let Expr::BinaryOp {
        left,
        op: BinaryOperator::GtEq,
        right,
    } = expr
    else {
        return;
    };
    if !matches!(right.as_ref(), Expr::Value(v) if matches!(&v.value, Value::Number(n, _) if n == "1"))
    {
        return;
    }
    let Expr::Subquery(q) = left.as_ref() else {
        return;
    };

    if !q.to_string().eq_ignore_ascii_case(UNWATCHED_LEAF_COUNT) {
        return;
    }
    let mut subquery = q.clone();
    if let SetExpr::Select(sel) = subquery.body.as_mut() {
        sel.projection = vec![SelectItem::UnnamedExpr(Expr::Value(
            Value::Number("1".into(), false).into(),
        ))];
    }
    *expr = Expr::Exists {
        subquery,
        negated: false,
    };
}

/// Transform an expression that yields a value (a projection, an argument, a
/// comparison operand).
fn transform_expr(expr: &mut Expr) {
    transform_expr_in(expr, false);
}

/// Transform an expression read as a condition (WHERE, HAVING, ON, the
/// operands of AND, OR and NOT, a CASE's WHEN).
fn transform_bool_expr(expr: &mut Expr) {
    transform_expr_in(expr, true);
}

/// Recursively transform an expression in place, applying all query-level
/// fixups (CASE booleans, max/min→GREATEST/LEAST, COLLATE, FTS MATCH, etc.).
/// `boolean` says whether the expression is read as a condition.
fn transform_expr_in(expr: &mut Expr, boolean: bool) {
    optimize_unwatched_leaf_exists(expr);
    match expr {
        // Fix 4: CASE WHEN x THEN 1 ELSE 0 END → TRUE/FALSE, in a boolean context
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
            let has_operand = operand.is_some();
            if let Some(op) = operand {
                transform_expr(op);
            }
            for cw in conditions.iter_mut() {
                // `CASE x WHEN 1` compares; `CASE WHEN a` tests.
                transform_expr_in(&mut cw.condition, !has_operand);
                transform_expr_in(&mut cw.result, boolean);
            }
            if let Some(er) = else_result.as_deref_mut() {
                transform_expr_in(er, boolean);
            }
            // Every result a 0/1 or a boolean literal: a condition reads them
            // as booleans, and a boolean literal among them makes the 0/1
            // booleans too, or PostgreSQL would mix integer and boolean.
            let results: Vec<&Expr> = conditions
                .iter()
                .map(|cw| &cw.result)
                .chain(else_result.as_deref())
                .collect();
            // A branch that is itself a condition (`THEN id IN (...)`,
            // Plex's playlists filter, 2026-10-16) is boolean on PostgreSQL,
            // so the 0/1 beside it must be TRUE/FALSE too, or "CASE types
            // integer and boolean cannot be matched". A CASE read as a
            // condition whose branches are integer expressions is compared
            // with 0 instead, as SQLite reads a non-zero integer.
            let all_bool_like = results
                .iter()
                .all(|r| is_zero_or_one_literal(r) || is_boolean_literal(r) || is_boolean_typed(r));
            let any_boolean = results
                .iter()
                .any(|r| is_boolean_literal(r) || is_boolean_typed(r));
            if all_bool_like && (boolean || any_boolean) {
                for cw in conditions.iter_mut() {
                    maybe_bool_value_inplace(&mut cw.result);
                }
                if let Some(er) = else_result {
                    maybe_bool_value_inplace(er);
                }
            } else if boolean && !any_boolean {
                let case = take_expr(expr);
                *expr = Expr::BinaryOp {
                    left: Box::new(Expr::Nested(Box::new(case))),
                    op: BinaryOperator::NotEq,
                    right: Box::new(Expr::Value(ValueWithSpan {
                        value: Value::Number("0".to_string(), false),
                        span: Span::empty(),
                    })),
                };
            }
        }

        // Fix 6 & 7: max(a,b) → GREATEST, min(a,b) → LEAST; glob(pat, x) → x ~ regex
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
            if let Some(filter) = &mut func.filter {
                transform_bool_expr(filter);
            }

            if fn_name == "max" && arg_count >= 2 {
                rename_function(func, "GREATEST");
            } else if fn_name == "min" && arg_count >= 2 {
                rename_function(func, "LEAST");
            } else if fn_name == "glob" && arg_count == 2 {
                // glob(X, Y) is Y GLOB X
                let taken = take_expr(expr);
                if let Expr::Function(Function {
                    args: FunctionArguments::List(al),
                    ..
                }) = taken
                {
                    let mut args = al.args.into_iter().filter_map(|a| match a {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                        _ => None,
                    });
                    if let (Some(pattern), Some(subject)) = (args.next(), args.next()) {
                        *expr = glob_match(subject, pattern, false);
                    }
                }
            }
        }

        // Fix FTS4: col MATCH term → to_tsvector('simple', plex.fold(col)) @@ plex.fts_query(term)
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

            // Fix 12: affinity between a column and a literal of the other type
            if is_comparison_op(op) {
                if let Some(fixed) = fix_affinity_comparison(left, op, right) {
                    *expr = fixed;
                    return;
                }
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
                transform_bool_expr(left);
                transform_bool_expr(right);
                maybe_bool_value_inplace(left);
                maybe_bool_value_inplace(right);
            } else {
                strip_collate_nocase_inplace(left);
                strip_collate_nocase_inplace(right);
                transform_expr(left);
                transform_expr(right);
            }
        }
        Expr::UnaryOp {
            op: UnaryOperator::Not,
            expr: inner,
        } => transform_bool_expr(inner),
        Expr::UnaryOp { expr: inner, .. } => transform_expr(inner),
        Expr::Nested(inner) => transform_expr_in(inner, boolean),
        Expr::Cast { expr: inner, .. } => transform_expr(inner),
        Expr::InList {
            expr: inner,
            list,
            negated,
        } => {
            transform_expr(inner);
            for e in list.iter_mut() {
                transform_expr(e);
            }
            if let Some(folded) = fix_affinity_in_list(inner, list, *negated) {
                *expr = folded;
            }
        }
        Expr::InSubquery {
            expr: inner,
            subquery,
            ..
        } => {
            transform_expr(inner);
            transform_query(subquery);
            if column_kind(inner) == Some(ColumnKind::Integer) {
                integer_json_each_elements(subquery);
            }
        }
        Expr::Exists { subquery, .. } => transform_query(subquery),
        Expr::Subquery(subquery) => transform_query(subquery),
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
                    *expr = unwrap_nested_column(*inner);
                }
                transform_expr(expr);
            } else if collation_name == "nocase" {
                // Standalone NOCASE (ORDER BY, non-equality) → LOWER(expr)
                let taken = take_expr(expr);
                if let Expr::Collate { expr: inner, .. } = taken {
                    *expr = unwrap_nested_column(*inner);
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
        // Fix 13: LIKE is ASCII case-insensitive in SQLite → ILIKE
        Expr::Like {
            expr: like_expr,
            pattern,
            ..
        } => {
            let nocase = is_collate_nocase(pattern) || is_collate_nocase(like_expr);
            strip_collate_nocase_inplace(pattern);
            strip_collate_nocase_inplace(like_expr);
            transform_expr(like_expr);
            transform_expr(pattern);
            if nocase || !crate::keywords::case_sensitive_like() {
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
            strip_collate_nocase_inplace(ilike_expr);
            transform_expr(ilike_expr);
            transform_expr(pattern);
        }
        // Fix 14: GLOB, which the preprocessor spells SIMILAR TO so that it parses
        Expr::SimilarTo { .. } => {
            let taken = take_expr(expr);
            if let Expr::SimilarTo {
                negated,
                expr: subject,
                pattern,
                ..
            } = taken
            {
                let mut subject = *subject;
                let mut pattern = *pattern;
                let nocase = is_collate_nocase(&pattern) || is_collate_nocase(&subject);
                strip_collate_nocase_inplace(&mut pattern);
                strip_collate_nocase_inplace(&mut subject);
                transform_expr(&mut subject);
                transform_expr(&mut pattern);
                *expr = glob_match(subject, pattern, nocase);
                if negated {
                    *expr = negate_regex_match(take_expr(expr));
                }
            }
        }
        Expr::RLike {
            expr: inner,
            pattern,
            ..
        } => {
            transform_expr(inner);
            transform_expr(pattern);
        }
        Expr::IsNull(inner)
        | Expr::IsNotNull(inner)
        | Expr::IsTrue(inner)
        | Expr::IsNotTrue(inner)
        | Expr::IsFalse(inner)
        | Expr::IsNotFalse(inner)
        | Expr::IsUnknown(inner)
        | Expr::IsNotUnknown(inner) => transform_expr(inner),
        Expr::IsDistinctFrom(a, b) | Expr::IsNotDistinctFrom(a, b) => {
            transform_expr(a);
            transform_expr(b);
        }
        Expr::AnyOp { left, right, .. } | Expr::AllOp { left, right, .. } => {
            transform_expr(left);
            transform_expr(right);
        }
        Expr::Tuple(items) => {
            for e in items {
                transform_expr(e);
            }
        }
        Expr::Extract { expr: inner, .. }
        | Expr::Ceil { expr: inner, .. }
        | Expr::Floor { expr: inner, .. } => transform_expr(inner),
        Expr::Substring {
            expr: inner,
            substring_from,
            substring_for,
            ..
        } => {
            transform_expr(inner);
            if let Some(e) = substring_from {
                transform_expr(e);
            }
            if let Some(e) = substring_for {
                transform_expr(e);
            }
        }
        Expr::Trim {
            expr: inner,
            trim_what,
            trim_characters,
            ..
        } => {
            transform_expr(inner);
            if let Some(e) = trim_what {
                transform_expr(e);
            }
            if let Some(list) = trim_characters {
                for e in list {
                    transform_expr(e);
                }
            }
        }
        Expr::Position { expr: inner, r#in } => {
            transform_expr(inner);
            transform_expr(r#in);
        }
        _ => {}
    }
}

fn is_comparison_op(op: &BinaryOperator) -> bool {
    matches!(
        op,
        BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Lt
            | BinaryOperator::LtEq
            | BinaryOperator::Gt
            | BinaryOperator::GtEq
    )
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

fn is_zero_or_one_literal(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Value(ValueWithSpan {
            value: Value::Number(s, _),
            ..
        }) if s == "0" || s == "1"
    )
}

fn is_boolean_literal(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Value(ValueWithSpan {
            value: Value::Boolean(_),
            ..
        })
    )
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
            *expr = unwrap_nested_column(*inner);
        }
    }
}

/// The preprocessor parenthesises `table.col COLLATE NOCASE`, which sqlparser
/// only accepts after a bare identifier; the parentheses have no further use.
fn unwrap_nested_column(expr: Expr) -> Expr {
    match expr {
        Expr::Nested(inner)
            if matches!(*inner, Expr::Identifier(_) | Expr::CompoundIdentifier(_)) =>
        {
            *inner
        }
        other => other,
    }
}

// ─── Fix 11: Collections filter ───────────────────────────────────────────────

/// Remove `metadata_type=18` from OR conditions.
/// Pattern: `(metadata_type=1 OR metadata_type=18)` → `metadata_type=1`
///
/// A predicate that is only `(metadata_type=18)` is kept: collections are
/// served, and folding it to TRUE answered the whole section where Plex asked
/// for its collections alone.
fn fix_collections_filter(expr: &mut Expr) {
    // Recurse into nested expressions first
    match expr {
        Expr::Nested(inner) => fix_collections_filter(inner),
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

// ─── Fix 12: column affinity ──────────────────────────────────────────────────
//
// SQLite compares a column with a literal under the column's affinity: a
// number against a TEXT column is compared as text, and a string against an
// INTEGER column is converted to a number -- or, when it is no number, stays
// text and sorts after every integer, so `id = ''` is false and `id <> ''`
// true. Plex writes `metadata_items.id=''` for an empty id list (deleting a
// smart collection, 2026-10-16); the CAST it used to get made PostgreSQL
// refuse the statement (`invalid input syntax for type integer: ""`), so the
// delete answered 500 after five retries. schema_migrations.version is text,
// and Plex writes `where version=202608120900`, which PostgreSQL rejects
// outright: twelve digits is a bigint, and the schema's compat operators only
// pair text with integer.
//
// Which columns are which comes from the schema's column types
// (`schema_columns`), resolved against the tables of the statement being
// transformed: a qualified column through its table or alias, a bare one
// through every table in scope. `status`, `state`, `downloaded` and
// `metadata_item_id` were once listed as text columns here; they are integers
// everywhere, and `status <> 1` became `<> 1::TEXT` and failed.

/// A table a statement reads, with the name a column may qualify it by.
#[derive(Clone, Debug)]
struct ScopedTable {
    qualifier: String,
    table: String,
}

thread_local! {
    /// The tables in scope for the expression being transformed, innermost
    /// level first. `transform_expr` walks expressions without knowing which
    /// table they belong to, so the statement-level walk sets this before
    /// descending; a subquery pushes its own level and restores on the way
    /// out, keeping the outer tables visible for correlated references.
    static SCOPE: RefCell<Vec<Vec<ScopedTable>>> = const { RefCell::new(Vec::new()) };
}

/// Scopes `SCOPE` to the tables of one statement or subquery.
struct TableScope;

impl TableScope {
    fn enter(tables: Vec<ScopedTable>) -> Self {
        SCOPE.with(|s| s.borrow_mut().insert(0, tables));
        Self
    }
}

impl Drop for TableScope {
    fn drop(&mut self) {
        SCOPE.with(|s| {
            let mut levels = s.borrow_mut();
            if !levels.is_empty() {
                levels.remove(0);
            }
        });
    }
}

/// The tables of a FROM list, joined ones included: a column in the WHERE may
/// belong to any of them.
fn scoped_tables(from: &[TableWithJoins]) -> Vec<ScopedTable> {
    fn push(out: &mut Vec<ScopedTable>, factor: &TableFactor) {
        match factor {
            TableFactor::Table { name, alias, .. } => {
                if let Some(ObjectNamePart::Identifier(ident)) = name.0.last() {
                    let table = ident.value.to_lowercase();
                    let qualifier = alias
                        .as_ref()
                        .map(|a| a.name.value.to_lowercase())
                        .unwrap_or_else(|| table.clone());
                    out.push(ScopedTable { qualifier, table });
                }
            }
            TableFactor::NestedJoin {
                table_with_joins, ..
            } => {
                push(out, &table_with_joins.relation);
                for join in &table_with_joins.joins {
                    push(out, &join.relation);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for twj in from {
        push(&mut out, &twj.relation);
        for join in &twj.joins {
            push(&mut out, &join.relation);
        }
    }
    out
}

fn schema_kind(table: &str, column: &str) -> Option<ColumnKind> {
    COLUMNS
        .iter()
        .find(|(t, c, _)| *t == table && *c == column)
        .map(|(_, _, kind)| *kind)
}

fn schema_has_table(table: &str) -> bool {
    COLUMNS.iter().any(|(t, _, _)| *t == table)
}

/// Every Plex `id` is an integer, whether or not the table is in the schema.
fn id_kind(column: &str) -> Option<ColumnKind> {
    (column == "id").then_some(ColumnKind::Integer)
}

/// The affinity of a column reference, or None when it is no column, the
/// column is unknown, or tables in scope disagree about it.
fn column_kind(expr: &Expr) -> Option<ColumnKind> {
    match expr {
        Expr::Identifier(ident) => {
            let column = ident.value.to_lowercase();
            SCOPE.with(|s| {
                for level in s.borrow().iter() {
                    let kinds: Vec<ColumnKind> = level
                        .iter()
                        .filter_map(|t| schema_kind(&t.table, &column))
                        .collect();
                    if let Some(first) = kinds.first() {
                        return kinds.iter().all(|k| k == first).then_some(*first);
                    }
                    // A table the schema does not know (a CTE, a derived
                    // table's name) may own the column: look no further out.
                    if level.iter().any(|t| !schema_has_table(&t.table)) {
                        return id_kind(&column);
                    }
                }
                id_kind(&column)
            })
        }
        Expr::CompoundIdentifier(parts) if parts.len() >= 2 => {
            let qualifier = parts[parts.len() - 2].value.to_lowercase();
            let column = parts[parts.len() - 1].value.to_lowercase();
            SCOPE.with(|s| {
                for level in s.borrow().iter() {
                    if let Some(t) = level.iter().find(|t| t.qualifier == qualifier) {
                        return schema_kind(&t.table, &column).or_else(|| id_kind(&column));
                    }
                }
                schema_kind(&qualifier, &column).or_else(|| id_kind(&column))
            })
        }
        Expr::Nested(inner) => column_kind(inner),
        _ => None,
    }
}

/// Whether a table of the innermost scope has a column of this name.
fn is_known_column(name: &str) -> bool {
    let name = name.to_lowercase();
    name == "id"
        || SCOPE.with(|s| {
            s.borrow()
                .first()
                .is_some_and(|level| level.iter().any(|t| schema_kind(&t.table, &name).is_some()))
        })
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
    make_function_call("json_valid", vec![expr])
}

fn make_function_call(name: &str, args: Vec<Expr>) -> Expr {
    make_named_function_call(
        ObjectName(vec![ObjectNamePart::Identifier(Ident::new(name))]),
        args,
    )
}

/// `schema.name(args)`
fn make_schema_function_call(schema: &str, name: &str, args: Vec<Expr>) -> Expr {
    make_named_function_call(
        ObjectName(vec![
            ObjectNamePart::Identifier(Ident::new(schema)),
            ObjectNamePart::Identifier(Ident::new(name)),
        ]),
        args,
    )
}

fn make_named_function_call(name: ObjectName, args: Vec<Expr>) -> Expr {
    Expr::Function(Function {
        name,
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

fn make_string(s: &str) -> Expr {
    Expr::Value(ValueWithSpan {
        value: Value::SingleQuotedString(s.to_string()),
        span: Span::empty(),
    })
}

fn make_number(s: &str) -> Expr {
    Expr::Value(ValueWithSpan {
        value: Value::Number(s.to_string(), false),
        span: Span::empty(),
    })
}

fn make_boolean(b: bool) -> Expr {
    Expr::Value(ValueWithSpan {
        value: Value::Boolean(b),
        span: Span::empty(),
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
    args.extend(segs.iter().map(|s| make_string(s)));
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

/// An expression PostgreSQL types as boolean: a comparison, a membership or
/// pattern test, a logical connective, EXISTS, or a boolean literal.
fn is_boolean_typed(expr: &Expr) -> bool {
    match expr {
        Expr::Nested(inner) => is_boolean_typed(inner),
        Expr::BinaryOp { op, .. } => matches!(
            op,
            BinaryOperator::Eq
                | BinaryOperator::NotEq
                | BinaryOperator::Lt
                | BinaryOperator::LtEq
                | BinaryOperator::Gt
                | BinaryOperator::GtEq
                | BinaryOperator::And
                | BinaryOperator::Or
                | BinaryOperator::Xor
                | BinaryOperator::AtAt
                | BinaryOperator::PGRegexMatch
                | BinaryOperator::PGRegexIMatch
                | BinaryOperator::PGRegexNotMatch
                | BinaryOperator::PGRegexNotIMatch
        ),
        Expr::UnaryOp {
            op: UnaryOperator::Not,
            ..
        }
        | Expr::IsNull(_)
        | Expr::IsNotNull(_)
        | Expr::IsTrue(_)
        | Expr::IsFalse(_)
        | Expr::IsNotTrue(_)
        | Expr::IsNotFalse(_)
        | Expr::IsDistinctFrom(_, _)
        | Expr::IsNotDistinctFrom(_, _)
        | Expr::InList { .. }
        | Expr::InSubquery { .. }
        | Expr::Between { .. }
        | Expr::Like { .. }
        | Expr::ILike { .. }
        | Expr::SimilarTo { .. }
        | Expr::Exists { .. } => true,
        _ => is_boolean_literal(expr),
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

fn string_literal(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Value(ValueWithSpan {
            value: Value::SingleQuotedString(s),
            ..
        }) => Some(s),
        _ => None,
    }
}

/// How SQLite reads a string literal under INTEGER affinity.
enum NumericString {
    Integer,
    Real,
    Text,
}

fn classify_numeric_string(s: &str) -> NumericString {
    let t = s.trim();
    if t.parse::<i64>().is_ok() {
        NumericString::Integer
    } else if t.parse::<f64>().is_ok() && !t.eq_ignore_ascii_case("nan") && !t.contains("inf") {
        NumericString::Real
    } else {
        NumericString::Text
    }
}

/// An integer column compared with a string literal that is not an integer:
/// the literal stays TEXT, which sorts after every integer, so the comparison
/// is a constant. With the literal on the right: integer < text, always.
fn non_integer_literal_comparison(op: &BinaryOperator, literal_on_left: bool) -> Option<Expr> {
    let value = match op {
        BinaryOperator::Eq => false,
        BinaryOperator::NotEq => true,
        BinaryOperator::Lt | BinaryOperator::LtEq => !literal_on_left,
        BinaryOperator::Gt | BinaryOperator::GtEq => literal_on_left,
        _ => return None,
    };
    Some(make_boolean(value))
}

/// A string literal compared with an integer column: the number it converts
/// to, or None when it is no number.
fn integer_affinity_literal(s: &str) -> Option<Expr> {
    match classify_numeric_string(s) {
        NumericString::Integer => Some(Expr::Cast {
            kind: CastKind::Cast,
            expr: Box::new(make_string(s)),
            data_type: DataType::Integer(None),
            array: false,
            format: None,
        }),
        NumericString::Real => Some(make_number(s.trim())),
        NumericString::Text => None,
    }
}

/// Fix a comparison between a column and a literal of the other affinity.
/// Returns the replacement, with both sides transformed, or None when the
/// comparison is not of that shape.
fn fix_affinity_comparison(left: &mut Expr, op: &BinaryOperator, right: &mut Expr) -> Option<Expr> {
    let left_kind = column_kind(left);
    let right_kind = column_kind(right);

    let (column_side, literal_side, literal_on_left, kind) = match (left_kind, right_kind) {
        (Some(kind), None) if kind != ColumnKind::Other => (&mut *left, &mut *right, false, kind),
        (None, Some(kind)) if kind != ColumnKind::Other => (&mut *right, &mut *left, true, kind),
        _ => return None,
    };

    let replacement = match kind {
        ColumnKind::Text if is_number_literal(literal_side) => {
            // text_col = 123 → text_col = 123::text
            make_text_cast(take_expr(literal_side))
        }
        ColumnKind::Integer => {
            let s = string_literal(literal_side)?.to_string();
            match integer_affinity_literal(&s) {
                Some(number) => number,
                None => return non_integer_literal_comparison(op, literal_on_left),
            }
        }
        _ => return None,
    };
    let column = take_expr(column_side);
    let (l, r) = if literal_on_left {
        (replacement, column)
    } else {
        (column, replacement)
    };
    Some(Expr::BinaryOp {
        left: Box::new(l),
        op: op.clone(),
        right: Box::new(r),
    })
}

/// `int_col IN ('', '3', 'x')`: each member takes the column's affinity, so
/// a member that is no integer can never match and is dropped; an emptied
/// list is FALSE (TRUE for NOT IN).
fn fix_affinity_in_list(inner: &Expr, list: &mut Vec<Expr>, negated: bool) -> Option<Expr> {
    if column_kind(inner) != Some(ColumnKind::Integer) {
        return None;
    }
    if !list.iter().any(|e| string_literal(e).is_some()) {
        return None;
    }
    let members: Vec<Expr> = std::mem::take(list)
        .into_iter()
        .filter_map(|e| match string_literal(&e) {
            Some(s) => integer_affinity_literal(s),
            None => Some(e),
        })
        .collect();
    if members.is_empty() {
        return Some(make_boolean(negated));
    }
    *list = members;
    None
}

/// `int_col IN (SELECT value FROM json_each(?))`: the elements take the
/// column's affinity as a literal list's members do, so the subquery yields
/// each element that is an integer, as a bigint, and drops the rest. Its
/// `value` is json_array_elements_text's text by now, which PostgreSQL
/// compares with an integer only through the plex schema's `=`, invisible to
/// a Live TV guide's connection.
fn integer_json_each_elements(subquery: &mut Query) {
    let SetExpr::Select(select) = subquery.body.as_mut() else {
        return;
    };
    let [twj] = select.from.as_slice() else {
        return;
    };
    if !twj.joins.is_empty() || !is_json_elements_text_table(&twj.relation) {
        return;
    }
    let [SelectItem::UnnamedExpr(projected)] = select.projection.as_mut_slice() else {
        return;
    };
    if !is_json_each_value(projected) {
        return;
    }
    *projected = wrap_double_colon_cast(json_each_value(), DataType::BigInt(None));
    let is_integer = Expr::BinaryOp {
        left: Box::new(json_each_value()),
        op: BinaryOperator::PGRegexMatch,
        right: Box::new(make_string(r"^\s*[-+]?[0-9]{1,18}\s*$")),
    };
    // A JSON null must remain a SQL NULL: NOT IN with a null member is
    // unknown for an unmatched integer, rather than true.
    let keep_element = Expr::Nested(Box::new(Expr::BinaryOp {
        left: Box::new(Expr::IsNull(Box::new(json_each_value()))),
        op: BinaryOperator::Or,
        right: Box::new(is_integer),
    }));
    select.selection = Some(match select.selection.take() {
        Some(existing) => Expr::BinaryOp {
            left: Box::new(Expr::Nested(Box::new(existing))),
            op: BinaryOperator::And,
            right: Box::new(keep_element),
        },
        None => keep_element,
    });
}

fn json_each_value() -> Expr {
    Expr::Identifier(Ident::new("value"))
}

/// The table functions json_each becomes, whose `value` is the element's text.
fn is_json_elements_text_table(relation: &TableFactor) -> bool {
    let TableFactor::Table {
        name,
        args: Some(_),
        ..
    } = relation
    else {
        return false;
    };
    matches!(
        name.0.last(),
        Some(ObjectNamePart::Identifier(i))
            if i.value.eq_ignore_ascii_case("json_array_elements_text")
                || i.value.eq_ignore_ascii_case("jsonb_array_elements_text")
    )
}

/// `value`, or `value::TEXT` as the json_each rewrite leaves it.
fn is_json_each_value(expr: &Expr) -> bool {
    match expr {
        Expr::Identifier(i) => i.value.eq_ignore_ascii_case("value"),
        Expr::Cast {
            expr,
            data_type: DataType::Text,
            ..
        } => is_json_each_value(expr),
        _ => false,
    }
}

// ─── Fix 14: GLOB → regular expression ────────────────────────────────────────
//
// GLOB is case-sensitive, and LIKE's `%`/`_` cannot carry its character
// classes, so the pattern becomes an anchored POSIX regular expression: `*`
// is `.*`, `?` is `.`, `[abc]` stays a class, and every other metacharacter
// is escaped. A pattern Plex builds at run time (`'*' || ? || '*'`) is
// converted piece by piece: the literal pieces here, the bound ones in
// PostgreSQL with regexp_replace, so the bound text's own `*` and `?` count
// as wildcards and its punctuation does not.

fn glob_match(subject: Expr, pattern: Expr, nocase: bool) -> Expr {
    let regex = glob_pattern_to_regex_expr(pattern);
    Expr::BinaryOp {
        left: Box::new(subject),
        op: if nocase {
            BinaryOperator::PGRegexIMatch
        } else {
            BinaryOperator::PGRegexMatch
        },
        right: Box::new(regex),
    }
}

fn negate_regex_match(expr: Expr) -> Expr {
    match expr {
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left,
            op: match op {
                BinaryOperator::PGRegexMatch => BinaryOperator::PGRegexNotMatch,
                BinaryOperator::PGRegexIMatch => BinaryOperator::PGRegexNotIMatch,
                other => other,
            },
            right,
        },
        other => Expr::UnaryOp {
            op: UnaryOperator::Not,
            expr: Box::new(Expr::Nested(Box::new(other))),
        },
    }
}

/// The anchored regular expression for a GLOB pattern expression.
fn glob_pattern_to_regex_expr(pattern: Expr) -> Expr {
    let mut pieces = Vec::new();
    flatten_concat(pattern, &mut pieces);
    if pieces.len() == 1 {
        if let Some(s) = string_literal(&pieces[0]) {
            return make_string(&format!("^{}$", glob_to_regex(s)));
        }
    }
    let mut parts = vec![make_string("^")];
    for piece in pieces {
        parts.push(match string_literal(&piece) {
            Some(s) => make_string(&glob_to_regex(s)),
            None => glob_fragment_to_regex_at_runtime(piece),
        });
    }
    parts.push(make_string("$"));
    let mut iter = parts.into_iter();
    let mut acc = iter.next().expect("at least the anchor");
    for part in iter {
        acc = Expr::BinaryOp {
            left: Box::new(acc),
            op: BinaryOperator::StringConcat,
            right: Box::new(part),
        };
    }
    Expr::Nested(Box::new(acc))
}

fn flatten_concat(expr: Expr, out: &mut Vec<Expr>) {
    match expr {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::StringConcat,
            right,
        } => {
            flatten_concat(*left, out);
            flatten_concat(*right, out);
        }
        Expr::Nested(inner) => flatten_concat(*inner, out),
        other => out.push(other),
    }
}

/// A GLOB pattern as an unanchored regular expression.
fn glob_to_regex(glob: &str) -> String {
    let chars: Vec<char> = glob.chars().collect();
    let mut out = String::with_capacity(glob.len() + 8);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '*' => out.push_str(".*"),
            '?' => out.push('.'),
            '[' => {
                // A class runs to the next `]`; a `]` right after `[` or `[^`
                // is a member, as in SQLite.
                let mut j = i + 1;
                if j < chars.len() && chars[j] == '^' {
                    j += 1;
                }
                if j < chars.len() && chars[j] == ']' {
                    j += 1;
                }
                while j < chars.len() && chars[j] != ']' {
                    j += 1;
                }
                if j < chars.len() {
                    let class: String = chars[i..=j].iter().collect();
                    out.push_str(&class.replace('\\', "\\\\"));
                    i = j;
                } else {
                    out.push_str("\\[");
                }
            }
            '.' | '^' | '$' | '+' | '(' | ')' | '{' | '}' | '|' | '\\' | ']' => {
                out.push('\\');
                out.push(c);
            }
            other => out.push(other),
        }
        i += 1;
    }
    out
}

/// A run-time GLOB fragment (a bound parameter, a column) as a regular
/// expression: metacharacters escaped, then `*` → `.*` and `?` → `.`.
fn glob_fragment_to_regex_at_runtime(fragment: Expr) -> Expr {
    let escaped = make_function_call(
        "regexp_replace",
        vec![
            fragment,
            make_string("([\\\\.^$+(){}|])"),
            make_string("\\\\\\1"),
            make_string("g"),
        ],
    );
    let stars = make_function_call(
        "regexp_replace",
        vec![
            escaped,
            make_string("\\*"),
            make_string(".*"),
            make_string("g"),
        ],
    );
    make_function_call(
        "regexp_replace",
        vec![
            stars,
            make_string("\\?"),
            make_string("."),
            make_string("g"),
        ],
    )
}

// ─── Fix FTS4: MATCH → @@ plex.fts_query ──────────────────────────────────────

/// The columns an fts4 view's own name stands for in `fts4_x MATCH term`.
const FTS_TABLE_COLUMNS: &[(&str, &[&str])] = &[
    (
        "fts4_metadata_titles",
        &["title", "title_sort", "original_title"],
    ),
    (
        "fts4_metadata_titles_icu",
        &["title", "title_sort", "original_title"],
    ),
    ("fts4_tag_titles", &["tag"]),
    ("fts4_tag_titles_icu", &["tag"]),
];

/// `col MATCH term` → `to_tsvector('simple', plex.fold(col)) @@ plex.fts_query(term)`
///
/// `plex.fold` lowers and unaccents the text and `plex.fts_query` turns an
/// FTS4 query into a tsquery (the schema's functions), for a literal term and
/// a bound one alike -- the translator used to convert a literal itself and
/// send a bound term as `@@ $1`, a raw string cast to tsquery, so a search
/// Plex bound never matched a row. A match against the view's own name
/// searches every column it indexes.
fn transform_fts_match(left: Expr, right: Expr) -> Expr {
    // sqlparser gives MATCH a lower precedence than SQLite does: it reads
    // `col MATCH 'x' AND a = 1` as `col MATCH ('x' AND a = 1)`, which put
    // Plex's whole WHERE inside plex.fts_query and failed as a boolean cast
    // (2026-10-16). SQLite binds MATCH like LIKE, so the term is the
    // leftmost leaf of such a conjunction and the rest stays outside.
    if let Expr::BinaryOp {
        left: term_side,
        op: op @ (BinaryOperator::And | BinaryOperator::Or),
        right: rest,
    } = right
    {
        return Expr::BinaryOp {
            left: Box::new(transform_fts_match(left, *term_side)),
            op,
            right: rest,
        };
    }
    let text = match &left {
        Expr::Identifier(ident) => {
            let name = ident.value.to_lowercase();
            match FTS_TABLE_COLUMNS.iter().find(|(t, _)| *t == name) {
                Some((_, columns)) => fts_columns_text(columns),
                None => left,
            }
        }
        _ => left,
    };
    let folded = make_schema_function_call("plex", "fold", vec![text]);
    let tsvector_call = make_function_call("to_tsvector", vec![make_string("simple"), folded]);
    let tsquery_call = make_schema_function_call("plex", "fts_query", vec![right]);
    Expr::BinaryOp {
        left: Box::new(tsvector_call),
        op: BinaryOperator::AtAt,
        right: Box::new(tsquery_call),
    }
}

/// `coalesce(a, '') || ' ' || coalesce(b, '') …`
fn fts_columns_text(columns: &[&str]) -> Expr {
    let mut iter = columns.iter().map(|c| {
        make_function_call(
            "coalesce",
            vec![Expr::Identifier(Ident::new(*c)), make_string("")],
        )
    });
    let mut acc = iter.next().expect("an fts view has a column");
    for next in iter {
        let spaced = Expr::BinaryOp {
            left: Box::new(acc),
            op: BinaryOperator::StringConcat,
            right: Box::new(make_string(" ")),
        };
        acc = Expr::BinaryOp {
            left: Box::new(spaced),
            op: BinaryOperator::StringConcat,
            right: Box::new(next),
        };
    }
    acc
}

/// Wrap an expression in LOWER(…) in place.
fn wrap_lower_inplace(expr: &mut Expr) {
    let inner = take_expr(expr);
    *expr = wrap_lower(inner);
}

/// Wrap an expression in LOWER(…)
fn wrap_lower(expr: Expr) -> Expr {
    make_function_call("LOWER", vec![expr])
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use crate::translate;

    #[test]
    fn a_placeholder_written_against_a_keyword_keeps_the_keyword() {
        // Plex's tag directory (/library/sections/N/genre, the Categories
        // view) writes `account_id=?left join media_items` with no space.
        // In SQLite `?` takes only digits, so that is a placeholder and then
        // LEFT JOIN. Read as one placeholder named `?left`, the LEFT went
        // and the join became an inner one: a show has no media_items row,
        // so every TV library's genres came back empty (2026-10-08).
        // The backtick sends it through the MySQL fallback, as Plex's does.
        let r = translate(
            "select tags.tag from tags join taggings on taggings.tag_id=tags.id \
             join metadata_items on metadata_items.id=taggings.metadata_item_id \
             left join metadata_item_settings on metadata_item_settings.guid = metadata_items.guid \
             and metadata_item_settings.account_id=?left join media_items \
             on media_items.metadata_item_id=metadata_items.id \
             where tags.tag_type=? and (taggings.`index` <= ? or -1 = ?)",
        )
        .unwrap();
        let sql = r.sql.to_uppercase();
        assert!(
            sql.contains("LEFT JOIN MEDIA_ITEMS"),
            "the LEFT of `?left join` must survive, got: {}",
            r.sql
        );
        assert_eq!(
            r.param_names.len(),
            4,
            "four placeholders, got {:?}",
            r.param_names
        );
    }

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
        // A CASE read as a condition yields booleans; one that yields a
        // value (a projection, an aggregate's argument) keeps SQLite's
        // integers, since `sum(CASE WHEN a THEN TRUE ELSE FALSE END)` is
        // no PostgreSQL.
        let r = translate("SELECT (CASE WHEN a THEN 1 ELSE 0 END) FROM t").unwrap();
        assert!(
            r.sql.contains("CASE WHEN a THEN 1 ELSE 0 END"),
            "a projected CASE keeps its integers, got: {}",
            r.sql
        );
        let r = translate("SELECT * FROM t WHERE (CASE WHEN a THEN 1 ELSE 0 END)").unwrap();
        assert!(
            r.sql.contains("CASE WHEN a THEN true ELSE false END"),
            "a CASE as a condition is boolean, got: {}",
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
    fn subset_core__query_icu_collation_is_the_schemas() {
        let r = translate("SELECT * FROM t ORDER BY name COLLATE icu_root").unwrap();
        assert!(
            r.sql.contains("ORDER BY name COLLATE plex.icu_root"),
            "Got: {}",
            r.sql
        );
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
            sql.contains("plex.fts_query('plex')"),
            "Expected plex.fts_query() call, got: {}",
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
        // The prefix star is plex.fts_query's to turn into `war:*`; the
        // translator hands the term over whole. (This test once passed on
        // `sql.contains("war")`, which any output satisfied.)
        let r =
            translate("SELECT rowid FROM fts4_metadata_titles WHERE title MATCH 'War*'").unwrap();
        assert_eq!(
            r.sql,
            "SELECT rowid FROM fts4_metadata_titles \
             WHERE to_tsvector('simple', plex.fold(title)) @@ plex.fts_query('War*')"
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
                result.contains("plex.fts_query('(日本*)')"),
                "{result}"
            );
            assert!(
                result.contains(&format!("{conjunction} rowid = 42")),
                "{result}"
            );
        }
    }

    #[test]
    fn fts_prefix_and_unicode_terms_reach_schema_converter_intact() {
        for term in ["(star*)", "é* OR 日本*", "a O日"] {
            let sql = format!("SELECT rowid FROM fts4_metadata_titles WHERE title MATCH '{term}'");
            let result = crate::translate(&sql).unwrap().sql;
            assert!(result.contains(&format!("plex.fts_query('{term}')")), "{result}");
        }
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

#[cfg(test)]
mod unwatched_leaf_exists_tests {
    const COUNT: &str = "SELECT count(*) FROM metadata_items AS leaves JOIN metadata_items AS parents ON leaves.parent_id = parents.id LEFT JOIN metadata_item_settings ON metadata_item_settings.guid = leaves.guid AND metadata_item_settings.account_id = 1 WHERE (parents.parent_id = metadata_items.id OR parents.id = metadata_items.id) AND (metadata_item_settings.view_count = 0 OR metadata_item_settings.view_count IS NULL) AND leaves.metadata_type IN (1, 4, 10)";
    #[test]
    fn smart_tv_hub_uses_exists_without_changing_paging_or_order() {
        let sql = format!("SELECT metadata_items.id FROM metadata_items WHERE ({COUNT}) >= 1 AND metadata_items.metadata_type = 2 ORDER BY metadata_items.title_sort LIMIT 21 OFFSET 42");
        let result = crate::translate(&sql).unwrap().sql;
        assert!(result.contains("EXISTS (SELECT 1 FROM"), "{result}");
        assert!(!result.contains("count(*)"), "{result}");
        assert!(
            result.contains("ORDER BY metadata_items.title_sort LIMIT 21 OFFSET 42"),
            "{result}"
        );
    }
    #[test]
    fn other_count_semantics_fail_closed() {
        for sub in [
            COUNT.replace("count(*)", "count(leaves.view_count)"),
            format!("{COUNT} HAVING count(*) > 1"),
            format!("{COUNT} LIMIT 1 OFFSET 1"),
            COUNT.replace("account_id = 1", "account_id = 2"),
            COUNT.replace("count(*)", "count(random())"),
        ] {
            let result = crate::translate(&format!(
                "SELECT metadata_items.id FROM metadata_items WHERE ({sub}) >= 1"
            ))
            .unwrap()
            .sql;
            assert!(!result.contains("EXISTS"), "{result}");
        }
        for comparison in [">= 2", "= 1", "> 1", "< 1"] {
            let result = crate::translate(&format!(
                "SELECT metadata_items.id FROM metadata_items WHERE ({COUNT}) {comparison}"
            ))
            .unwrap()
            .sql;
            assert!(!result.contains("EXISTS"), "{result}");
        }
    }
}

#[cfg(test)]
mod smart_tv_page_tests {
    use super::*;
    fn sql() -> String {
        format!("SELECT metadata_items.id FROM metadata_items JOIN metadata_items AS children ON children.parent_id = metadata_items.id JOIN metadata_items AS grandchildren ON grandchildren.parent_id = children.id LEFT JOIN metadata_item_settings ON metadata_item_settings.guid = metadata_items.guid AND metadata_item_settings.account_id = 1 WHERE metadata_items.library_section_id IN (2) AND (({UNWATCHED_LEAF_COUNT}) >= 1 AND metadata_item_settings.rating = 10 AND (metadata_items.metadata_type = 2 OR metadata_items.metadata_type = 10)) AND metadata_items.metadata_type = 2 ORDER BY grandchildren.added_at DESC, metadata_items.title_sort, metadata_items.title_sort LIMIT 21 OFFSET 0")
    }
    #[test]
    fn hub_pages_distinct_shows_in_first_episode_order() {
        let result = crate::translate(&sql()).unwrap().sql;
        assert!(
            result.contains("GROUP BY metadata_items.id, metadata_items.title_sort"),
            "{result}"
        );
        assert!(
            result.contains("bool_or(grandchildren.added_at IS NULL)"),
            "{result}"
        );
        assert!(result.contains("max(grandchildren.added_at)"), "{result}");
        assert!(result.contains("EXISTS (SELECT 1 FROM"), "{result}");
        assert!(result.ends_with("LIMIT 21 OFFSET 0"), "{result}");
    }
    #[test]
    fn other_hub_shapes_are_not_grouped() {
        for sql in [
            sql().replace("rating = 10", "rating = 5"),
            sql().replace("account_id = 1", "account_id = 2"),
            sql().replace("IN (2)", "IN (3)"),
            sql().replace("LIMIT 21 OFFSET 0", ""),
            sql().replace("DESC", "ASC"),
            sql().replace(
                "SELECT metadata_items.id FROM",
                "SELECT metadata_items.id, metadata_items.title FROM",
            ),
        ] {
            let result = crate::translate(&sql).unwrap().sql;
            assert!(!result.contains("GROUP BY metadata_items.id"), "{result}");
        }
    }
    #[test]
    fn plex_descendant_media_joins_are_in_postgres_scope() {
        for (descendant, hierarchy) in [
            ("children", "JOIN metadata_items AS children ON children.parent_id = metadata_items.id"),
            ("grandchildren", "JOIN metadata_items AS children ON children.parent_id = metadata_items.id JOIN metadata_items AS grandchildren ON grandchildren.parent_id = children.id"),
        ] {
            let sql = format!("SELECT DISTINCT metadata_items.id FROM metadata_items JOIN media_items ON media_items.metadata_item_id = {descendant}.id JOIN media_parts ON media_parts.media_item_id = media_items.id LEFT JOIN metadata_item_settings ON metadata_item_settings.guid = metadata_items.guid AND metadata_item_settings.account_id = 1 {hierarchy} WHERE metadata_items.id IN (1, 2)");
            let result = crate::translate(&sql).unwrap().sql;
            assert!(result.find(&format!("JOIN metadata_items AS {descendant}")).unwrap() < result.find("JOIN media_items").unwrap(), "{result}");
            assert!(result.contains("LEFT JOIN metadata_item_settings"), "{result}");
            if descendant == "grandchildren" {
                assert!(result.find("JOIN metadata_items AS children").unwrap() < result.find("JOIN metadata_items AS grandchildren").unwrap(), "{result}");
            }
        }
    }

    #[test]
    fn plex_descendant_media_join_outer_hierarchy_is_unchanged() {
        let sql = "SELECT metadata_items.id FROM metadata_items JOIN media_items ON media_items.metadata_item_id = children.id LEFT JOIN metadata_items AS children ON children.parent_id = metadata_items.id";
        let result = crate::translate(sql).unwrap().sql;
        assert!(
            result.find("JOIN media_items").unwrap()
                < result.find("LEFT JOIN metadata_items AS children").unwrap(),
            "{result}"
        );
    }
}
