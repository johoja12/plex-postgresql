use crate::byte_utils::push_utf8_byte;
use sqlparser::ast::helpers::attached_token::AttachedToken;
/// Module: functions
///
/// Rewrites SQLite function calls to their PostgreSQL equivalents:
///   IFNULL(a,b)           → COALESCE(a,b)
///   iif(cond,then,else)   → CASE WHEN cond THEN then ELSE else END
///   typeof(x)             → plex.sqlite_typeof(x) (schema/plex_schema.sql:
///                           SQLite's names, 'integer' for every integer type)
///   last_insert_rowid()   → lastval()
///   json_each(x)          → json_array_elements_text(x) (SQLite's `value` is the
///                           element itself, so a string comes back without JSON quotes)
///   json_each(x,'$.a')    → jsonb_array_elements_text(x::jsonb #> '{a}')
///   json_extract(x,'$.a') → jsonb_extract_path_text(x::jsonb,'a')
///   json_extract(x,$1)    → jsonb_path_query_first(x::jsonb,$1::jsonpath) #>> '{}'
///   json_set(x,'$.a',v)   → jsonb_set(x::jsonb, string_to_array('a',','), to_jsonb(v), true)
///   group_concat(…)       → string_agg(…::text, sep), DISTINCT and ORDER BY kept
///   SUBSTR(a,b,c)         → SUBSTRING(a,b,c); SUBSTR(a,-n[,m]) → [LEFT(]RIGHT(a,n)[,m)]
///   instr(a,b)            → STRPOS(a,b)   (same arg order)
///   hex(x)                → UPPER(ENCODE(x::bytea,'hex')), UPPER(TO_HEX(n)) for a number
///   round(x,n)            → ROUND(x::numeric,n)
///   printf('%d',x)        → format('%s',x::bigint)
///   strftime('%s',…)      → EXTRACT(EPOCH FROM …)::bigint
///   strftime('%Y-%m-%d',…)→ TO_CHAR(…, 'YYYY-MM-DD')
///   unixepoch(…)          → EXTRACT(EPOCH FROM …)::bigint
///   datetime('now',…)     → NOW() …; datetime(x,…) → TO_CHAR(…, 'YYYY-MM-DD HH24:MI:SS')
///   date(x,…)             → (…)::date::text; time(x,…) → TO_CHAR(…, 'HH24:MI:SS')
///
/// The time functions share one reading of SQLite's modifiers: `±N unit`,
/// `start of day|month|year`, `weekday N`, `unixepoch`, `utc` and `localtime`
/// (no-ops: Plex keeps UTC), `subsec`. A modifier or format the rewrite does
/// not know leaves the call as written, logged, never half-translated.
///
/// The walk is sqlparser's `VisitorMut`, so a call is rewritten in every
/// position: under IS NULL, a LIKE operand, any join's ON, ON CONFLICT DO
/// UPDATE, LIMIT, a subquery, a CASE arm, a window clause. A rewrite works on
/// a copy of the arguments and replaces the call only when it has a result:
/// the old walk drained them first, so `json_extract(extra_data, ?)` reached
/// PostgreSQL as `json_extract()` (2026-10-16 audit, items 11, 17, 18).
use sqlparser::ast::*;
use sqlparser::tokenizer::Span;
use std::ops::ControlFlow;

use crate::rewriter::ast_utils::{take_expr, wrap_double_colon_cast};

pub fn transform(stmt: &mut Statement) {
    let _ = stmt.visit(&mut Rewriter);
}

struct Rewriter;

impl VisitorMut for Rewriter {
    type Break = ();

    /// iif(typeof(X) in (...), X, ...) → X, before the children are rewritten
    /// (the typeof inside is the pattern).
    fn pre_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<()> {
        if let Expr::Function(func) = expr {
            if function_name(func).as_deref() == Some("iif") {
                if let Some(simplified) = try_simplify_typeof(func) {
                    *expr = simplified;
                }
            }
        }
        ControlFlow::Continue(())
    }

    /// Children first, so a call's arguments are already PostgreSQL's.
    fn post_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<()> {
        match expr {
            Expr::Function(_) => rewrite_function(expr),
            // SUBSTR(a,b,c) is parsed as Expr::Substring { shorthand: true };
            // SUBSTRING output is forced by clearing the flag.
            Expr::Substring { .. } => rewrite_substring(expr),
            _ => {}
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, factor: &mut TableFactor) -> ControlFlow<()> {
        transform_table_factor(factor);
        ControlFlow::Continue(())
    }

    /// After the FROM clause's json_each became json_array_elements_text: a
    /// bare `value` in the projection is cast to text, and a `key` resolves
    /// to the element's 0-based index through WITH ORDINALITY.
    fn post_visit_query(&mut self, query: &mut Query) -> ControlFlow<()> {
        if let SetExpr::Select(select) = query.body.as_mut() {
            resolve_json_each_columns(select);
        }
        ControlFlow::Continue(())
    }
}

/// The lowercase name of an unqualified call.
fn function_name(func: &Function) -> Option<String> {
    match func.name.0.as_slice() {
        [ObjectNamePart::Identifier(i)] => Some(i.value.to_lowercase()),
        _ => None,
    }
}

fn log_untranslated(what: &str, call: &Function) {
    crate::log_info_lazy!("functions: {} left untranslated: {}", what, call);
}

// ─── json_each / json_tree in FROM ───────────────────────────────────────────

fn is_json_array_elements_table(tf: &TableFactor) -> bool {
    if let TableFactor::Table { name, .. } = tf {
        return matches!(
            object_last_ident_lower(name).as_deref(),
            Some("json_array_elements" | "json_array_elements_text" | "jsonb_array_elements_text")
        );
    }
    false
}

fn object_last_ident_lower(name: &ObjectName) -> Option<String> {
    name.0.last().and_then(|p| match p {
        ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
        _ => None,
    })
}

fn rename_sqlite_json_table_fn_name_if_present(name: &mut ObjectName) -> Option<&'static str> {
    let source = object_last_ident_lower(name)?;
    let target = match source.as_str() {
        // json_array_elements would hand back each element as JSON, so a
        // string keeps its quotes and `tg.tag IN (SELECT value FROM
        // json_each(:C1))` -- Plex's collection delete -- matched nothing
        // (2026-10-16). _text yields the element's text, as SQLite does.
        "json_each" => "json_array_elements_text",
        "json_tree" => "jsonb_each_text",
        _ => return None,
    };
    if let Some(ObjectNamePart::Identifier(i)) = name.0.last_mut() {
        i.value = target.to_string();
        return Some(target);
    }
    None
}

fn cast_unnamed_function_args_to(args: &mut [FunctionArg], data_type: DataType) {
    for arg in args.iter_mut() {
        if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
            *e = wrap_double_colon_cast(take_expr(e), data_type.clone());
        }
    }
}

/// `json_each(x, path)`: the array at the path, so the elements are its.
fn json_each_path_args(args: &mut Vec<FunctionArg>) -> bool {
    let exprs: Vec<Expr> = args
        .iter()
        .filter_map(|a| match a {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e.clone()),
            _ => None,
        })
        .collect();
    let [doc, path] = exprs.as_slice() else {
        return false;
    };
    let doc = make_jsonb_cast(doc.clone());
    let at_path = match get_literal_json_path(path) {
        Some(segs) if segs.is_empty() => doc,
        Some(segs) => make_binary(
            doc,
            BinaryOperator::HashArrow,
            make_path_text_array_expr(&segs),
        ),
        None => make_function_call(
            "jsonb_path_query_first",
            vec![doc, make_jsonpath_cast(path.clone())],
        ),
    };
    *args = vec![FunctionArg::Unnamed(FunctionArgExpr::Expr(at_path))];
    true
}

fn rename_json_table_function(name: &mut ObjectName, args: &mut Vec<FunctionArg>) {
    let Some(target) = rename_sqlite_json_table_fn_name_if_present(name) else {
        return;
    };
    if target == "jsonb_each_text" {
        cast_unnamed_function_args_to(args, DataType::JSONB);
    } else if json_each_path_args(args) {
        if let Some(ObjectNamePart::Identifier(i)) = name.0.last_mut() {
            i.value = "jsonb_array_elements_text".to_string();
        }
    } else {
        cast_unnamed_function_args_to(args, DataType::JSON);
    }
}

fn transform_table_factor(f: &mut TableFactor) {
    match f {
        TableFactor::Table {
            name, args, alias, ..
        } => {
            if let Some(TableFunctionArgs { args, .. }) = args {
                let was_json_each = object_last_ident_lower(name).as_deref() == Some("json_each");
                rename_json_table_function(name, args);
                // `json_each.value` names the function's table in SQLite;
                // the alias keeps that name on PostgreSQL's.
                if was_json_each && alias.is_none() {
                    *alias = Some(TableAlias {
                        explicit: true,
                        name: Ident::new("json_each"),
                        columns: vec![],
                    });
                }
            }
        }
        TableFactor::Function { name, args, .. } => {
            rename_json_table_function(name, args);
        }
        TableFactor::TableFunction { expr, .. } => {
            if let Expr::Function(func) = expr {
                if let FunctionArguments::List(arg_list) = &mut func.args {
                    rename_json_table_function(&mut func.name, &mut arg_list.args);
                }
            }
        }
        _ => {}
    }
}

/// A `key` reference of a json_each table: SQLite's 0-based index of the
/// element, which WITH ORDINALITY gives 1-based.
struct KeyShift<'a> {
    alias: &'a str,
}

impl KeyShift<'_> {
    fn refers_to_key(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Identifier(i) => i.value.eq_ignore_ascii_case("key"),
            Expr::CompoundIdentifier(parts) => {
                parts.len() == 2
                    && parts[0].value.eq_ignore_ascii_case(self.alias)
                    && parts[1].value.eq_ignore_ascii_case("key")
            }
            _ => false,
        }
    }
}

impl VisitorMut for KeyShift<'_> {
    type Break = ();

    fn post_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<()> {
        if self.refers_to_key(expr) {
            let key = take_expr(expr);
            *expr = Expr::Nested(Box::new(Expr::BinaryOp {
                left: Box::new(key),
                op: BinaryOperator::Minus,
                right: Box::new(make_number_literal("1")),
            }));
        }
        ControlFlow::Continue(())
    }
}

fn resolve_json_each_columns(select: &mut Select) {
    let Some(alias) = select.from.iter().find_map(|twj| {
        std::iter::once(&twj.relation)
            .chain(twj.joins.iter().map(|j| &j.relation))
            .find(|tf| is_json_array_elements_table(tf))
            .and_then(|tf| match tf {
                TableFactor::Table { alias, .. } => {
                    Some(alias.as_ref().map(|a| a.name.value.clone()))
                }
                _ => None,
            })
    }) else {
        return;
    };
    for item in &mut select.projection {
        if let SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } = item {
            cast_json_value_to_text(e);
        }
    }

    let Some(alias) = alias else {
        return;
    };
    let shift = KeyShift { alias: &alias };
    let mut refers = false;
    let _ = visit_expressions(&*select, |e| {
        if shift.refers_to_key(e) {
            refers = true;
            return ControlFlow::Break(());
        }
        ControlFlow::<()>::Continue(())
    });
    if !refers {
        return;
    }
    for item in &mut select.projection {
        if let SelectItem::UnnamedExpr(e) = item {
            if shift.refers_to_key(e) {
                *item = SelectItem::ExprWithAlias {
                    expr: take_expr(e),
                    alias: Ident::new("key"),
                };
            }
        }
    }
    let mut shift = KeyShift { alias: &alias };
    let _ = select.visit(&mut shift);
    for twj in &mut select.from {
        for tf in
            std::iter::once(&mut twj.relation).chain(twj.joins.iter_mut().map(|j| &mut j.relation))
        {
            if !is_json_array_elements_table(tf) {
                continue;
            }
            if let TableFactor::Table {
                with_ordinality,
                alias: Some(table_alias),
                ..
            } = tf
            {
                if table_alias.name.value.eq_ignore_ascii_case(&alias) {
                    *with_ordinality = true;
                    table_alias.columns = vec![
                        TableAliasColumnDef::from_name("value"),
                        TableAliasColumnDef::from_name("key"),
                    ];
                }
            }
        }
    }
}

/// Cast bare `value` identifiers to ::text (for json_array_elements output)
fn cast_json_value_to_text(expr: &mut Expr) {
    if let Expr::Identifier(ref ident) = expr {
        if ident.value.to_lowercase() == "value" {
            *expr = wrap_double_colon_cast(take_expr(expr), DataType::Text);
        }
    }
}

// ─── Expression builders ─────────────────────────────────────────────────────

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

fn make_custom_cast(expr: Expr, type_name: &str) -> Expr {
    Expr::Cast {
        kind: CastKind::DoubleColon,
        expr: Box::new(expr),
        data_type: DataType::Custom(
            ObjectName(vec![ObjectNamePart::Identifier(Ident::new(type_name))]),
            vec![],
        ),
        array: false,
        format: None,
    }
}

fn make_jsonb_cast(expr: Expr) -> Expr {
    make_custom_cast(expr, "jsonb")
}

fn make_jsonpath_cast(expr: Expr) -> Expr {
    make_custom_cast(expr, "jsonpath")
}

fn make_to_jsonb(expr: Expr) -> Expr {
    // to_jsonb(anyelement) cannot take an untyped literal or parameter.
    let expr = if is_placeholder(&expr) || get_literal_single_quoted_string(&expr).is_some() {
        wrap_double_colon_cast(expr, DataType::Text)
    } else {
        expr
    };
    make_function_call("to_jsonb", vec![expr])
}

fn make_string_literal(s: &str) -> Expr {
    Expr::Value(ValueWithSpan {
        value: Value::SingleQuotedString(s.to_string()),
        span: Span::empty(),
    })
}

fn make_number_literal(s: &str) -> Expr {
    Expr::Value(ValueWithSpan {
        value: Value::Number(s.to_string(), false),
        span: Span::empty(),
    })
}

fn make_case_when(condition: Expr, then_expr: Expr, else_expr: Expr) -> Expr {
    Expr::Case {
        case_token: AttachedToken::empty(),
        end_token: AttachedToken::empty(),
        operand: None,
        conditions: vec![CaseWhen {
            condition,
            result: then_expr,
        }],
        else_result: Some(Box::new(else_expr)),
    }
}

fn make_binary(left: Expr, op: BinaryOperator, right: Expr) -> Expr {
    Expr::BinaryOp {
        left: Box::new(left),
        op,
        right: Box::new(right),
    }
}

fn is_placeholder(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Value(ValueWithSpan {
            value: Value::Placeholder(_),
            ..
        })
    )
}

/// A number, as `3` or `-3`.
fn numeric_literal(expr: &Expr) -> Option<(bool, String)> {
    match expr {
        Expr::Value(ValueWithSpan {
            value: Value::Number(n, _),
            ..
        }) => Some((n.starts_with('-'), n.trim_start_matches('-').to_string())),
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => numeric_literal(expr).map(|(neg, n)| (!neg, n)),
        Expr::UnaryOp {
            op: UnaryOperator::Plus,
            expr,
        } => numeric_literal(expr),
        _ => None,
    }
}

// ─── JSON paths ──────────────────────────────────────────────────────────────

fn is_integer_path_segment(seg: &str) -> bool {
    seg.parse::<i64>().is_ok()
}

fn escape_jsonpath_key_segment(seg: &str) -> String {
    seg.replace('\\', "\\\\").replace('"', "\\\"")
}

fn jsonpath_from_segments(path_segments: &[String]) -> String {
    let mut out = String::from("$");
    for seg in path_segments {
        if is_integer_path_segment(seg) {
            out.push('[');
            out.push_str(seg);
            out.push(']');
        } else {
            out.push_str("[\"");
            out.push_str(&escape_jsonpath_key_segment(seg));
            out.push_str("\"]");
        }
    }
    out
}

/// Parse a limited SQLite JSON path syntax into segments:
///   $.a.b[0].c -> ["a","b","0","c"]
/// Returns None for unsupported path syntax.
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

                // Quoted key: $['a.b'] or $["a.b"]
                if bytes[i] == b'\'' || bytes[i] == b'"' {
                    let quote = bytes[i];
                    i += 1;
                    let mut key = String::new();
                    while i < bytes.len() {
                        if bytes[i] == b'\\' && i + 1 < bytes.len() {
                            i += 1;
                            push_utf8_byte(&mut key, path, i);
                            i += 1;
                            continue;
                        }
                        if bytes[i] == quote {
                            // SQLite-style doubled quote escape inside same quote
                            if i + 1 < bytes.len() && bytes[i + 1] == quote {
                                key.push(quote as char);
                                i += 2;
                                continue;
                            }
                            break;
                        }
                        push_utf8_byte(&mut key, path, i);
                        i += 1;
                    }
                    if i >= bytes.len() || bytes[i] != quote {
                        return None;
                    }
                    i += 1;
                    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                        i += 1;
                    }
                    if i >= bytes.len() || bytes[i] != b']' {
                        return None;
                    }
                    i += 1;
                    out.push(key);
                    continue;
                }

                // Numeric index (supports negative): $[0], $[-1]
                let token_start = i;
                if bytes[i] == b'-' {
                    i += 1;
                }
                let digit_start = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                if i > digit_start {
                    let numeric_token = path[token_start..i].trim().to_string();
                    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                        i += 1;
                    }
                    if i >= bytes.len() || bytes[i] != b']' {
                        return None;
                    }
                    i += 1;
                    out.push(numeric_token);
                    continue;
                }

                // Bracket key without quotes: $[foo_bar]
                let key_start = token_start;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric()
                        || bytes[i] == b'_'
                        || bytes[i] == b':'
                        || bytes[i] == b'-')
                {
                    i += 1;
                }
                if i == key_start {
                    return None;
                }
                let key = path[key_start..i].trim().to_string();
                while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                if i >= bytes.len() || bytes[i] != b']' {
                    return None;
                }
                i += 1;
                out.push(key);
            }
            _ => return None,
        }
    }
    Some(out)
}

fn get_literal_json_path(expr: &Expr) -> Option<Vec<String>> {
    parse_json_path_segments(&get_literal_single_quoted_string(expr)?)
}

fn get_literal_single_quoted_string(expr: &Expr) -> Option<String> {
    if let Expr::Value(ValueWithSpan {
        value: Value::SingleQuotedString(s),
        ..
    }) = expr
    {
        return Some(s.clone());
    }
    None
}

fn make_path_text_array_expr(path_segments: &[String]) -> Expr {
    let joined = path_segments.join(",");
    make_function_call(
        "string_to_array",
        vec![make_string_literal(&joined), make_string_literal(",")],
    )
}

/// The text[] of a path known only at run time (a bound `$.a.b`):
/// string_to_array(ltrim(path, '$.'), '.').
fn make_bound_path_text_array_expr(path: Expr) -> Expr {
    make_function_call(
        "string_to_array",
        vec![
            make_function_call("ltrim", vec![path, make_string_literal("$.")]),
            make_string_literal("."),
        ],
    )
}

/// `jsonb_path_exists(doc, path)` for a literal or a bound path.
fn make_jsonb_path_exists(doc: Expr, path: &Expr) -> Expr {
    let path_expr = match get_literal_json_path(path) {
        Some(segs) => make_string_literal(&jsonpath_from_segments(&segs)),
        None => make_jsonpath_cast(path.clone()),
    };
    make_function_call("jsonb_path_exists", vec![doc, path_expr])
}

/// The JSON at a path: `jsonb_extract_path(doc, 'a', 'b')` for a literal
/// path, `jsonb_path_query_first(doc, path::jsonpath)` otherwise.
fn make_json_at_path(doc: Expr, path: &Expr) -> Expr {
    if let Some(segs) = get_literal_json_path(path) {
        let mut p = vec![doc];
        p.extend(segs.iter().map(|s| make_string_literal(s)));
        return make_function_call("jsonb_extract_path", p);
    }
    let path = match get_literal_single_quoted_string(path) {
        Some(s) => make_string_literal(&s),
        None => make_jsonpath_cast(path.clone()),
    };
    make_function_call("jsonb_path_query_first", vec![doc, path])
}

fn rewrite_json_extract_call(args: &[Expr]) -> Option<Expr> {
    let [doc, path, ..] = args else {
        return None;
    };
    let doc = make_jsonb_cast(doc.clone());
    if let Some(segs) = get_literal_json_path(path) {
        if segs.is_empty() {
            return Some(wrap_double_colon_cast(doc, DataType::Text));
        }
        let mut fn_args = vec![doc];
        fn_args.extend(segs.iter().map(|s| make_string_literal(s)));
        return Some(make_function_call("jsonb_extract_path_text", fn_args));
    }
    // A richer SQLite path (wildcards, filters) or one bound at run time:
    // PostgreSQL's jsonpath, then the text of the value (`#>> '{}'`, so a
    // string comes back without its quotes, as SQLite's json_extract does).
    // The `::jsonb` on the (already jsonb) value is the mark query.rs's
    // guard of the JSON operators reads: without it the operand is wrapped
    // in a json_valid(...) CASE over its text.
    Some(make_binary(
        make_jsonb_cast(make_json_at_path(doc, path)),
        BinaryOperator::HashLongArrow,
        make_string_literal("{}"),
    ))
}

fn rewrite_json_type_call(args: &[Expr]) -> Option<Expr> {
    let [doc, rest @ ..] = args else {
        return None;
    };
    let doc = make_jsonb_cast(doc.clone());
    let at = match rest.first() {
        None => doc,
        Some(path) => make_json_at_path(doc, path),
    };
    Some(make_function_call("jsonb_typeof", vec![at]))
}

fn rewrite_json_array_length_call(args: &[Expr]) -> Option<Expr> {
    let [doc, rest @ ..] = args else {
        return None;
    };
    let doc = make_jsonb_cast(doc.clone());
    let at = match rest.first() {
        None => doc,
        Some(path) => make_json_at_path(doc, path),
    };
    Some(make_function_call("jsonb_array_length", vec![at]))
}

enum JsonSetMode {
    Set,
    Insert,
    Replace,
}

/// A path's text[] for jsonb_set: from a literal, or from the bound value.
fn path_text_array(path: &Expr) -> Option<Expr> {
    if let Some(segs) = get_literal_json_path(path) {
        return (!segs.is_empty()).then(|| make_path_text_array_expr(&segs));
    }
    if get_literal_single_quoted_string(path).is_some() {
        // A literal the segment parser refused (a wildcard): not a jsonb_set path.
        return None;
    }
    Some(make_bound_path_text_array_expr(path.clone()))
}

fn rewrite_json_set_like_call(args: &[Expr], mode: JsonSetMode) -> Option<Expr> {
    let [doc, rest @ ..] = args else {
        return None;
    };
    if rest.len() < 2 {
        return None;
    }
    let mut current = make_jsonb_cast(doc.clone());
    for pair in rest.chunks(2) {
        let [path_expr, value_expr] = pair else {
            break;
        };
        let path_array = path_text_array(path_expr)?;
        let should_create = matches!(mode, JsonSetMode::Set | JsonSetMode::Insert);
        let rewritten = make_function_call(
            "jsonb_set",
            vec![
                current.clone(),
                path_array,
                make_to_jsonb(value_expr.clone()),
                Expr::Value(ValueWithSpan {
                    value: Value::Boolean(should_create),
                    span: Span::empty(),
                }),
            ],
        );
        current = match mode {
            JsonSetMode::Set => rewritten,
            JsonSetMode::Insert => make_case_when(
                make_jsonb_path_exists(current.clone(), path_expr),
                current,
                rewritten,
            ),
            JsonSetMode::Replace => make_case_when(
                make_jsonb_path_exists(current.clone(), path_expr),
                rewritten,
                current,
            ),
        };
    }
    Some(current)
}

fn rewrite_json_remove_call(args: &[Expr]) -> Option<Expr> {
    let [doc, paths @ ..] = args else {
        return None;
    };
    if paths.is_empty() {
        return None;
    }
    let mut current = make_jsonb_cast(doc.clone());
    for path_expr in paths {
        let Some(path_array) = path_text_array(path_expr) else {
            if get_literal_json_path(path_expr).is_some_and(|segs| segs.is_empty()) {
                continue;
            }
            return None;
        };
        current = make_binary(current, BinaryOperator::HashMinus, path_array);
    }
    Some(current)
}

fn rewrite_json_patch_call(args: &[Expr]) -> Option<Expr> {
    let [left, right] = args else {
        return None;
    };
    Some(make_function_call(
        "jsonb_mergepatch",
        vec![
            make_jsonb_cast(left.clone()),
            make_jsonb_cast(right.clone()),
        ],
    ))
}

// ─── Other builtins ──────────────────────────────────────────────────────────

/// group_concat(x[, sep]) → string_agg(x::text, sep), in place, so DISTINCT,
/// ORDER BY and FILTER stay where they are (they were lost before).
fn rewrite_group_concat(func: &mut Function) {
    let FunctionArguments::List(list) = &mut func.args else {
        return;
    };
    let mut unnamed = list.args.iter_mut().filter_map(|a| match a {
        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
        _ => None,
    });
    let Some(value) = unnamed.next() else {
        return;
    };
    *value = wrap_double_colon_cast(take_expr(value), DataType::Text);
    if unnamed.next().is_none() {
        list.args.push(FunctionArg::Unnamed(FunctionArgExpr::Expr(
            make_string_literal(","),
        )));
    }
    rename_function(func, "string_agg");
}

fn rewrite_julianday_call(args: &[Expr]) -> Option<Expr> {
    let source = args.first().cloned().unwrap_or_else(make_now_call);
    let source = if is_now_literal(&source) {
        make_now_call()
    } else {
        source
    };
    let epoch = Expr::Extract {
        field: DateTimeField::Epoch,
        syntax: ExtractSyntax::From,
        expr: Box::new(source),
    };
    let epoch_double = Expr::Cast {
        kind: CastKind::DoubleColon,
        expr: Box::new(epoch),
        data_type: DataType::Double(ExactNumberInfo::None),
        array: false,
        format: None,
    };
    let days = make_binary(
        epoch_double,
        BinaryOperator::Divide,
        make_number_literal("86400.0"),
    );
    Some(make_binary(
        days,
        BinaryOperator::Plus,
        make_number_literal("2440587.5"),
    ))
}

fn rewrite_total_call(args: &[Expr]) -> Option<Expr> {
    let value = args.first()?.clone();
    let sum_expr = make_function_call("SUM", vec![value]);
    Some(make_function_call(
        "COALESCE",
        vec![sum_expr, make_number_literal("0.0")],
    ))
}

/// hex(x): the bytes' hex in upper case; a number's hex for a number.
fn rewrite_hex_call(args: &[Expr]) -> Option<Expr> {
    let [value] = args else {
        return None;
    };
    let hex = match numeric_literal(value) {
        Some((false, _)) => make_function_call("TO_HEX", vec![value.clone()]),
        _ => make_function_call(
            "ENCODE",
            vec![
                wrap_double_colon_cast(value.clone(), DataType::Bytea),
                make_string_literal("hex"),
            ],
        ),
    };
    Some(make_function_call("UPPER", vec![hex]))
}

/// round(x, n): PostgreSQL rounds a double only to whole numbers.
fn rewrite_round_call(args: &[Expr]) -> Option<Expr> {
    let [value, digits] = args else {
        return None;
    };
    Some(make_function_call(
        "ROUND",
        vec![
            wrap_double_colon_cast(value.clone(), DataType::Numeric(ExactNumberInfo::None)),
            digits.clone(),
        ],
    ))
}

/// printf(fmt, ...) → format(fmt, ...): PostgreSQL's format knows %s, %I
/// and %L, so every other conversion becomes %s over a cast or a rendering
/// of its argument. A conversion that has no rendering leaves the call.
fn rewrite_printf_call(args: &[Expr]) -> Option<Expr> {
    let [fmt, values @ ..] = args else {
        return None;
    };
    let fmt = get_literal_single_quoted_string(fmt)?;
    let mut out = String::new();
    let mut new_args = Vec::new();
    let mut next_value = values.iter();
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'%') {
            chars.next();
            out.push_str("%%");
            continue;
        }
        let mut flags = String::new();
        while let Some(&f) = chars.peek() {
            if "-+ 0#".contains(f) {
                flags.push(f);
                chars.next();
            } else {
                break;
            }
        }
        let mut width = String::new();
        while let Some(&d) = chars.peek() {
            if d.is_ascii_digit() {
                width.push(d);
                chars.next();
            } else {
                break;
            }
        }
        let mut precision: Option<String> = None;
        if chars.peek() == Some(&'.') {
            chars.next();
            let mut p = String::new();
            while let Some(&d) = chars.peek() {
                if d.is_ascii_digit() {
                    p.push(d);
                    chars.next();
                } else {
                    break;
                }
            }
            precision = Some(p);
        }
        while matches!(chars.peek(), Some('l') | Some('h')) {
            chars.next();
        }
        let conversion = chars.next()?;
        let value = next_value.next()?.clone();
        // format supports the `-` flag and a width, nothing else.
        if flags.chars().any(|f| f != '-') {
            return None;
        }
        let rendered = match conversion {
            's' if precision.is_none() => value,
            'd' | 'i' | 'u' if precision.is_none() => {
                wrap_double_colon_cast(value, DataType::BigInt(None))
            }
            'f' => {
                let decimals: usize = match precision.as_deref() {
                    None => 6,
                    Some(p) => p.parse().ok()?,
                };
                let mut pattern = String::from("FM999999999999999990");
                if decimals > 0 {
                    pattern.push('.');
                    pattern.extend(std::iter::repeat('0').take(decimals));
                }
                make_function_call(
                    "TO_CHAR",
                    vec![
                        wrap_double_colon_cast(value, DataType::Numeric(ExactNumberInfo::None)),
                        make_string_literal(&pattern),
                    ],
                )
            }
            'x' | 'X' if precision.is_none() => {
                let hex = make_function_call(
                    "TO_HEX",
                    vec![wrap_double_colon_cast(value, DataType::BigInt(None))],
                );
                if conversion == 'X' {
                    make_function_call("UPPER", vec![hex])
                } else {
                    hex
                }
            }
            'c' if precision.is_none() => make_function_call(
                "CHR",
                vec![wrap_double_colon_cast(value, DataType::Int(None))],
            ),
            _ => return None,
        };
        out.push('%');
        out.push_str(&flags);
        out.push_str(&width);
        out.push('s');
        new_args.push(rendered);
    }
    let mut call_args = vec![make_string_literal(&out)];
    call_args.extend(new_args);
    Some(make_function_call("format", call_args))
}

/// SUBSTR(x, -n[, m]): SQLite counts a negative start from the end.
fn rewrite_substring(expr: &mut Expr) {
    let Expr::Substring {
        expr: inner,
        substring_from,
        substring_for,
        shorthand,
        ..
    } = expr
    else {
        return;
    };
    if let Some((true, n)) = substring_from.as_deref().and_then(numeric_literal) {
        let from_end = make_function_call("RIGHT", vec![take_expr(inner), make_number_literal(&n)]);
        *expr = match substring_for.take() {
            Some(len) => make_function_call("LEFT", vec![from_end, *len]),
            None => from_end,
        };
        return;
    }
    if *shorthand {
        *shorthand = false; // force SUBSTRING output
    }
}

// ─── Time functions ──────────────────────────────────────────────────────────

/// What SQLite's time value is, which decides how it becomes a timestamp.
enum TimeSource {
    /// `'now'`
    Now,
    /// A date or time written out, `'2024-01-01 10:00:00'`.
    Text(String),
    /// Seconds since the epoch: Plex's columns, a number, a bound value,
    /// anything the `'unixepoch'` modifier names.
    Epoch(Expr),
}

/// A timestamp built from SQLite's time value and modifiers.
struct TimeValue {
    ts: Expr,
    /// Whether `ts` carries a zone (NOW() and to_timestamp do; a literal
    /// does not), so text output is formatted in UTC as SQLite's is.
    zoned: bool,
    /// `'subsec'`: text output keeps the fraction of a second.
    subsec: bool,
}

impl TimeValue {
    /// The timestamp as UTC wall-clock time, for formatting.
    fn utc(&self) -> Expr {
        if !self.zoned {
            return self.ts.clone();
        }
        Expr::AtTimeZone {
            timestamp: Box::new(self.ts.clone()),
            time_zone: Box::new(make_string_literal("UTC")),
        }
    }
}

/// None when a modifier is not understood: the call is left as written.
fn time_value(args: &[Expr]) -> Option<TimeValue> {
    let (source, modifiers) = match args.split_first() {
        Some((first, rest)) => (first.clone(), rest),
        None => (make_string_literal("now"), &[][..]),
    };
    let modifiers: Option<Vec<String>> = modifiers
        .iter()
        .map(|m| get_literal_single_quoted_string(m).map(|s| s.trim().to_lowercase()))
        .collect();
    let modifiers = modifiers?;
    let unixepoch = modifiers.iter().any(|m| m == "unixepoch");

    let source = if is_now_literal(&source) {
        TimeSource::Now
    } else if unixepoch {
        TimeSource::Epoch(source)
    } else if let Some(text) = get_literal_single_quoted_string(&source) {
        TimeSource::Text(text)
    } else {
        TimeSource::Epoch(source)
    };
    let (mut ts, zoned) = match source {
        TimeSource::Now => (make_now_call(), true),
        TimeSource::Text(text) => (
            wrap_double_colon_cast(
                make_string_literal(&text),
                DataType::Timestamp(None, TimezoneInfo::None),
            ),
            false,
        ),
        TimeSource::Epoch(e) => (make_to_timestamp(e), true),
    };
    let mut subsec = false;
    for modifier in &modifiers {
        match modifier.as_str() {
            "unixepoch" | "utc" | "localtime" => {}
            "subsec" | "subsecond" => subsec = true,
            "start of day" => ts = make_date_trunc("day", ts),
            "start of month" => ts = make_date_trunc("month", ts),
            "start of year" => ts = make_date_trunc("year", ts),
            _ => {
                if let Some(weekday) = modifier.strip_prefix("weekday ") {
                    let n: u8 = weekday.trim().parse().ok().filter(|n| *n <= 6)?;
                    ts = make_weekday_advance(ts, n);
                } else {
                    ts = apply_interval(ts, modifier)?;
                }
            }
        }
    }
    Some(TimeValue { ts, zoned, subsec })
}

fn make_date_trunc(field: &str, ts: Expr) -> Expr {
    make_function_call("date_trunc", vec![make_string_literal(field), ts])
}

/// `weekday N`: forward to the next day whose day of week is N (Sunday 0),
/// the same day when it already is:
/// ts + ((N - EXTRACT(DOW FROM ts)::int + 7) % 7) * INTERVAL '1 day'.
fn make_weekday_advance(ts: Expr, n: u8) -> Expr {
    let dow = Expr::Cast {
        kind: CastKind::DoubleColon,
        expr: Box::new(Expr::Extract {
            field: DateTimeField::Dow,
            syntax: ExtractSyntax::From,
            expr: Box::new(ts.clone()),
        }),
        data_type: DataType::Int(None),
        array: false,
        format: None,
    };
    let ahead = make_binary(
        make_binary(
            make_number_literal(&n.to_string()),
            BinaryOperator::Minus,
            dow,
        ),
        BinaryOperator::Plus,
        make_number_literal("7"),
    );
    let days = make_binary(
        Expr::Nested(Box::new(ahead)),
        BinaryOperator::Modulo,
        make_number_literal("7"),
    );
    make_binary(
        ts,
        BinaryOperator::Plus,
        make_binary(
            Expr::Nested(Box::new(days)),
            BinaryOperator::Multiply,
            make_interval("1 day"),
        ),
    )
}

fn make_interval(value: &str) -> Expr {
    Expr::Interval(Interval {
        value: Box::new(make_string_literal(value)),
        leading_field: None,
        leading_precision: None,
        last_field: None,
        fractional_seconds_precision: None,
    })
}

/// SQLite's `[+-]NNN units` modifier as `source ± INTERVAL 'NNN units'`.
/// None for any other word: `INTERVAL 'utc'` is an error, not a translation.
fn apply_interval(source: Expr, modifier: &str) -> Option<Expr> {
    let (op, rest) = if let Some(rest) = modifier.strip_prefix('-') {
        (BinaryOperator::Minus, rest)
    } else if let Some(rest) = modifier.strip_prefix('+') {
        (BinaryOperator::Plus, rest)
    } else {
        (BinaryOperator::Plus, modifier)
    };
    let (amount, unit) = rest.trim().split_once(' ')?;
    if amount.is_empty()
        || !amount.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        || amount.parse::<f64>().is_err()
    {
        return None;
    }
    let unit = unit.trim();
    if !matches!(
        unit,
        "day"
            | "days"
            | "hour"
            | "hours"
            | "minute"
            | "minutes"
            | "second"
            | "seconds"
            | "month"
            | "months"
            | "year"
            | "years"
    ) {
        return None;
    }
    Some(make_binary(
        source,
        op,
        make_interval(&format!("{amount} {unit}")),
    ))
}

fn is_now_literal(expr: &Expr) -> bool {
    matches!(expr, Expr::Value(ValueWithSpan { value: Value::SingleQuotedString(s), .. }) if s.to_lowercase() == "now")
}

fn make_now_call() -> Expr {
    make_function_call("NOW", vec![])
}

/// Build a TO_TIMESTAMP(expr) function call.
fn make_to_timestamp(source: Expr) -> Expr {
    make_function_call("TO_TIMESTAMP", vec![source])
}

/// Build a TO_CHAR(expr, 'fmt') function call.
fn make_to_char(source: Expr, pg_fmt: &str) -> Expr {
    make_function_call("TO_CHAR", vec![source, make_string_literal(pg_fmt)])
}

fn make_epoch(ts: Expr, whole: bool) -> Expr {
    let epoch = Expr::Extract {
        field: DateTimeField::Epoch,
        syntax: ExtractSyntax::From,
        expr: Box::new(ts),
    };
    if whole {
        wrap_double_colon_cast(epoch, DataType::BigInt(None))
    } else {
        epoch
    }
}

/// A piece of a strftime format: TO_CHAR patterns, or an expression TO_CHAR
/// has no pattern for (`%w`, `%u`).
enum FormatPiece {
    Pattern(String),
    Weekday(DateTimeField),
}

/// Translate a SQLite strftime format into TO_CHAR pieces. None for a
/// conversion TO_CHAR cannot render.
fn translate_strftime_format(fmt: &str, subsec: bool) -> Option<Vec<FormatPiece>> {
    let mut pieces: Vec<FormatPiece> = Vec::new();
    let mut pattern = String::new();
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            if c.is_ascii_alphabetic() || c == '"' {
                // A letter is a TO_CHAR pattern unless quoted.
                pattern.push('"');
                if c == '"' {
                    pattern.push('\\');
                }
                pattern.push(c);
                pattern.push('"');
            } else {
                pattern.push(c);
            }
            continue;
        }
        let spec = chars.next()?;
        let mapped = match spec {
            'Y' => "YYYY",
            'm' => "MM",
            'd' => "DD",
            'e' => "FMDD",
            'H' | 'k' => "HH24",
            'I' | 'l' => "HH12",
            'M' => "MI",
            'S' if subsec => "SS.MS",
            'S' => "SS",
            'f' => "SS.MS",
            'j' => "DDD",
            'W' => "WW",
            'p' => "AM",
            'F' => "YYYY-MM-DD",
            'T' => "HH24:MI:SS",
            'R' => "HH24:MI",
            'G' => "IYYY",
            'g' => "IY",
            'V' => "IW",
            '%' => "%",
            'w' | 'u' => {
                if !pattern.is_empty() {
                    pieces.push(FormatPiece::Pattern(std::mem::take(&mut pattern)));
                }
                pieces.push(FormatPiece::Weekday(if spec == 'w' {
                    DateTimeField::Dow
                } else {
                    DateTimeField::Isodow
                }));
                continue;
            }
            _ => return None,
        };
        pattern.push_str(mapped);
    }
    if !pattern.is_empty() {
        pieces.push(FormatPiece::Pattern(pattern));
    }
    Some(pieces)
}

fn render_format(value: &TimeValue, pieces: Vec<FormatPiece>) -> Expr {
    let utc = value.utc();
    let mut rendered = pieces.into_iter().map(|piece| match piece {
        FormatPiece::Pattern(p) => make_to_char(utc.clone(), &p),
        FormatPiece::Weekday(field) => wrap_double_colon_cast(
            wrap_double_colon_cast(
                Expr::Extract {
                    field,
                    syntax: ExtractSyntax::From,
                    expr: Box::new(utc.clone()),
                },
                DataType::Int(None),
            ),
            DataType::Text,
        ),
    });
    let Some(first) = rendered.next() else {
        return make_string_literal("");
    };
    rendered.fold(first, |acc, next| {
        make_binary(acc, BinaryOperator::StringConcat, next)
    })
}

/// strftime(fmt, time[, modifiers...])
fn rewrite_strftime_call(args: &[Expr]) -> Option<Expr> {
    let [fmt, rest @ ..] = args else {
        return None;
    };
    let fmt = get_literal_single_quoted_string(fmt)?;
    let value = time_value(rest)?;
    if fmt == "%s" {
        return Some(make_epoch(value.ts, !value.subsec));
    }
    let pieces = translate_strftime_format(&fmt, value.subsec)?;
    Some(render_format(&value, pieces))
}

fn rewrite_unixepoch_call(args: &[Expr]) -> Option<Expr> {
    let value = time_value(args)?;
    Some(make_epoch(value.ts, !value.subsec))
}

/// datetime('now', ...) stays a timestamp (NOW() - INTERVAL ...); any other
/// time value is formatted as SQLite's text.
fn rewrite_datetime_call(args: &[Expr]) -> Option<Expr> {
    let value = time_value(args)?;
    if args.first().is_none_or(is_now_literal) {
        return Some(value.ts);
    }
    let fmt = if value.subsec {
        "YYYY-MM-DD HH24:MI:SS.MS"
    } else {
        "YYYY-MM-DD HH24:MI:SS"
    };
    Some(make_to_char(value.utc(), fmt))
}

fn rewrite_date_call(args: &[Expr]) -> Option<Expr> {
    let value = time_value(args)?;
    Some(wrap_double_colon_cast(
        wrap_double_colon_cast(Expr::Nested(Box::new(value.utc())), DataType::Date),
        DataType::Text,
    ))
}

fn rewrite_time_call(args: &[Expr]) -> Option<Expr> {
    let value = time_value(args)?;
    let fmt = if value.subsec {
        "HH24:MI:SS.MS"
    } else {
        "HH24:MI:SS"
    };
    Some(make_to_char(value.utc(), fmt))
}

// ─── The function dispatch ───────────────────────────────────────────────────

fn rewrite_function(expr: &mut Expr) {
    let Expr::Function(func) = expr else {
        return;
    };
    let Some(fn_name) = function_name(func) else {
        return;
    };
    // The rewrites below read a copy of the arguments and replace the call
    // only with a result; one that bails leaves the call as written.
    let args = unnamed_args(func);
    let replacement = match fn_name.as_str() {
        "ifnull" => {
            rename_function(func, "COALESCE");
            return;
        }
        "iif" => {
            let [cond, then, else_e, ..] = args.as_slice() else {
                return;
            };
            Some(make_case_when(cond.clone(), then.clone(), else_e.clone()))
        }
        "typeof" => {
            func.name = ObjectName(vec![
                ObjectNamePart::Identifier(Ident::new("plex")),
                ObjectNamePart::Identifier(Ident::new("sqlite_typeof")),
            ]);
            return;
        }
        "last_insert_rowid" => {
            rename_function(func, "lastval");
            return;
        }
        "json_each" => {
            rename_function(func, "json_array_elements");
            return;
        }
        "unicode" => {
            rename_function(func, "ASCII");
            return;
        }
        "json_tree" => {
            rename_function(func, "jsonb_each_text");
            if let FunctionArguments::List(ref mut arg_list) = func.args {
                cast_unnamed_function_args_to(&mut arg_list.args, DataType::JSONB);
            }
            return;
        }
        "json_extract" => rewrite_json_extract_call(&args),
        "json_type" => rewrite_json_type_call(&args),
        "json_array_length" => rewrite_json_array_length_call(&args),
        "json_set" => rewrite_json_set_like_call(&args, JsonSetMode::Set),
        "json_insert" => rewrite_json_set_like_call(&args, JsonSetMode::Insert),
        "json_replace" => rewrite_json_set_like_call(&args, JsonSetMode::Replace),
        "json_remove" => rewrite_json_remove_call(&args),
        "json_patch" => rewrite_json_patch_call(&args),
        "json_object" => {
            rename_function(func, "jsonb_build_object");
            return;
        }
        "json_array" => {
            rename_function(func, "jsonb_build_array");
            return;
        }
        "json_group_array" => {
            rename_function(func, "jsonb_agg");
            return;
        }
        "json_group_object" => {
            rename_function(func, "jsonb_object_agg");
            return;
        }
        "group_concat" => {
            rewrite_group_concat(func);
            return;
        }
        "printf" | "format" => rewrite_printf_call(&args),
        "hex" => rewrite_hex_call(&args),
        "round" => {
            if args.len() != 2 {
                return;
            }
            rewrite_round_call(&args)
        }
        "julianday" => rewrite_julianday_call(&args),
        "total" => rewrite_total_call(&args),
        // json_quote(x) -> to_json(x)::text
        "json_quote" => args.first().map(|first| {
            wrap_double_colon_cast(
                make_function_call("to_json", vec![first.clone()]),
                DataType::Text,
            )
        }),
        "substr" => {
            rename_function(func, "SUBSTRING");
            return;
        }
        "instr" => {
            rename_function(func, "STRPOS");
            return;
        }
        "strftime" => rewrite_strftime_call(&args),
        "unixepoch" => rewrite_unixepoch_call(&args),
        "datetime" => rewrite_datetime_call(&args),
        "date" => rewrite_date_call(&args),
        "time" => rewrite_time_call(&args),
        _ => return,
    };
    match replacement {
        Some(new_expr) => *expr = new_expr,
        None => log_untranslated(&fn_name, func),
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn rename_function(func: &mut Function, new_name: &str) {
    func.name = ObjectName(vec![ObjectNamePart::Identifier(Ident::new(new_name))]);
}

/// Detect the pattern: iif(typeof(X) in ('integer','real'), X, strftime('%s', X, 'utc'))
/// and simplify to just X. In PostgreSQL, columns have fixed types so the
/// conditional typeof check is unnecessary.
fn try_simplify_typeof(func: &Function) -> Option<Expr> {
    let FunctionArguments::List(ref al) = func.args else {
        return None;
    };
    let [FunctionArg::Unnamed(FunctionArgExpr::Expr(first)), FunctionArg::Unnamed(FunctionArgExpr::Expr(then_expr)), ..] =
        al.args.as_slice()
    else {
        return None;
    };
    // First arg should be: typeof(X) IN ('integer', 'real', ...)
    let Expr::InList { expr, .. } = first else {
        return None;
    };
    let Expr::Function(inner_func) = expr.as_ref() else {
        return None;
    };
    if function_name(inner_func).as_deref() != Some("typeof") {
        return None;
    }
    // Second arg is the "then" value — just return it
    Some(then_expr.clone())
}

/// A copy of every `FunctionArg::Unnamed(FunctionArgExpr::Expr(…))` arg.
fn unnamed_args(func: &Function) -> Vec<Expr> {
    let FunctionArguments::List(ref al) = func.args else {
        return Vec::new();
    };
    al.args
        .iter()
        .filter_map(|arg| match arg {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e.clone()),
            _ => None,
        })
        .collect()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use crate::translate;

    #[test]
    fn subset_core__function_ifnull_to_coalesce() {
        let r = translate("SELECT IFNULL(a, 0) FROM t").unwrap();
        assert!(r.sql.to_uppercase().contains("COALESCE"));
        assert!(!r.sql.to_uppercase().contains("IFNULL"));
    }

    #[test]
    fn subset_core__function_iif_to_case() {
        let r = translate("SELECT iif(a > 0, 'yes', 'no') FROM t").unwrap();
        assert!(r.sql.to_uppercase().contains("CASE WHEN"));
        assert!(r.sql.to_uppercase().contains("THEN"));
        assert!(r.sql.to_uppercase().contains("ELSE"));
        assert!(!r.sql.to_lowercase().contains("iif"));
    }

    #[test]
    fn subset_core__function_typeof_to_sqlite_typeof() {
        let r = translate("SELECT typeof(x) FROM t").unwrap();
        assert_eq!(r.sql, "SELECT plex.sqlite_typeof(x) FROM t");
    }

    #[test]
    fn subset_core__function_last_insert_rowid_to_lastval() {
        let r = translate("SELECT last_insert_rowid()").unwrap();
        assert!(r.sql.to_lowercase().contains("lastval()"));
        assert!(!r.sql.to_lowercase().contains("last_insert_rowid"));
    }

    #[test]
    fn subset_core__function_substr_to_substring() {
        let r = translate("SELECT SUBSTR(a, 1, 5) FROM t").unwrap();
        assert!(r.sql.to_uppercase().contains("SUBSTRING"));
    }

    #[test]
    fn subset_core__function_length_preserved() {
        let r = translate("SELECT LENGTH(name) FROM t").unwrap();
        assert!(r.sql.to_uppercase().contains("LENGTH"));
    }

    #[test]
    fn subset_core__function_instr_to_strpos() {
        let r = translate("SELECT instr(haystack, needle) FROM t").unwrap();
        assert!(r.sql.to_uppercase().contains("STRPOS"));
        assert!(!r.sql.to_lowercase().contains("instr"));
    }

    #[test]
    fn subset_core__function_strftime_epoch() {
        let r = translate("SELECT strftime('%s', 'now') FROM t").unwrap();
        assert!(r.sql.to_uppercase().contains("EXTRACT"));
        assert!(r.sql.to_uppercase().contains("EPOCH"));
    }

    #[test]
    fn subset_core__function_unixepoch_now() {
        let r = translate("SELECT unixepoch('now') FROM t").unwrap();
        assert!(r.sql.to_uppercase().contains("EXTRACT"));
        assert!(r.sql.to_uppercase().contains("EPOCH"));
    }

    #[test]
    fn subset_json__function_json_extract_bracket_single_quoted_key() {
        let r = translate("SELECT json_extract(extra_data, '$[\"pv:version\"]') FROM t").unwrap();
        let low = r.sql.to_lowercase();
        assert!(low.contains("jsonb_extract_path_text"), "{}", r.sql);
        assert!(low.contains("'pv:version'"), "{}", r.sql);
    }

    #[test]
    fn subset_json__function_json_extract_bracket_double_quoted_key_with_dot() {
        let r = translate("SELECT json_extract(extra_data, '$[\"a.b\"][0]') FROM t").unwrap();
        let low = r.sql.to_lowercase();
        assert!(low.contains("jsonb_extract_path_text"), "{}", r.sql);
        assert!(low.contains("'a.b'"), "{}", r.sql);
        assert!(low.contains("'0'"), "{}", r.sql);
    }

    #[test]
    fn subset_json__function_json_extract_negative_index_supported() {
        let r = translate("SELECT json_extract(extra_data, '$.items[-1]') FROM t").unwrap();
        let low = r.sql.to_lowercase();
        assert!(low.contains("jsonb_extract_path_text"), "{}", r.sql);
        assert!(low.contains("'-1'"), "{}", r.sql);
    }

    #[test]
    fn subset_json__function_json_extract_wildcard_path_uses_jsonpath_fallback() {
        let r = translate("SELECT json_extract(extra_data, '$.items[*].id') FROM t").unwrap();
        let low = r.sql.to_lowercase();
        assert!(low.contains("jsonb_path_query_first"), "{}", r.sql);
    }

    #[test]
    fn subset_json__function_json_type_filter_path_uses_jsonpath_fallback() {
        let r = translate("SELECT json_type(extra_data, '$.items ? (@.id > 1)') FROM t").unwrap();
        let low = r.sql.to_lowercase();
        assert!(low.contains("jsonb_path_query_first"), "{}", r.sql);
        assert!(low.contains("jsonb_typeof"), "{}", r.sql);
    }

    #[test]
    fn subset_json__function_json_array_length_wildcard_path_uses_jsonpath_fallback() {
        let r = translate("SELECT json_array_length(extra_data, '$.items[*]') FROM t").unwrap();
        let low = r.sql.to_lowercase();
        assert!(low.contains("jsonb_path_query_first"), "{}", r.sql);
        assert!(low.contains("jsonb_array_length"), "{}", r.sql);
    }

    #[test]
    fn subset_json__function_json_remove_rewrites_to_jsonb_path_delete() {
        let r = translate("SELECT json_remove(extra_data, '$.a.b') FROM t").unwrap();
        let low = r.sql.to_lowercase();
        assert!(low.contains("#-"), "{}", r.sql);
        assert!(low.contains("string_to_array"), "{}", r.sql);
        assert!(low.contains("::jsonb"), "{}", r.sql);
    }

    #[test]
    fn subset_json__function_json_patch_rewrites_to_jsonb_mergepatch() {
        let r = translate("SELECT json_patch(a, b) FROM t").unwrap();
        let low = r.sql.to_lowercase();
        assert!(low.contains("::jsonb"), "{}", r.sql);
        assert!(low.contains("jsonb_mergepatch"), "{}", r.sql);
    }

    #[test]
    fn subset_json__function_json_object_rewrites_to_jsonb_build_object() {
        let r = translate("SELECT json_object('a', 1, 'b', 2)").unwrap();
        let low = r.sql.to_lowercase();
        assert!(low.contains("jsonb_build_object"), "{}", r.sql);
    }

    #[test]
    fn subset_json__function_json_array_rewrites_to_jsonb_build_array() {
        let r = translate("SELECT json_array(1, 2, 3)").unwrap();
        let low = r.sql.to_lowercase();
        assert!(low.contains("jsonb_build_array"), "{}", r.sql);
    }

    #[test]
    fn subset_json__function_json_group_array_rewrites_to_jsonb_agg() {
        let r = translate("SELECT json_group_array(v) FROM t").unwrap();
        let low = r.sql.to_lowercase();
        assert!(low.contains("jsonb_agg"), "{}", r.sql);
    }

    #[test]
    fn subset_json__function_json_group_object_rewrites_to_jsonb_object_agg() {
        let r = translate("SELECT json_group_object(k, v) FROM t").unwrap();
        let low = r.sql.to_lowercase();
        assert!(low.contains("jsonb_object_agg"), "{}", r.sql);
    }

    #[test]
    fn subset_json__function_json_insert_is_conditional_on_missing_path() {
        let r = translate("SELECT json_insert(extra_data, '$.status', 'ok') FROM t").unwrap();
        let low = r.sql.to_lowercase();
        assert!(low.contains("case"), "{}", r.sql);
        assert!(low.contains("jsonb_path_exists"), "{}", r.sql);
        assert!(low.contains("jsonb_set"), "{}", r.sql);
    }

    #[test]
    fn subset_json__function_json_replace_is_conditional_on_existing_path() {
        let r = translate("SELECT json_replace(extra_data, '$.status', 'ok') FROM t").unwrap();
        let low = r.sql.to_lowercase();
        assert!(low.contains("case"), "{}", r.sql);
        assert!(low.contains("jsonb_path_exists"), "{}", r.sql);
        assert!(low.contains("jsonb_set"), "{}", r.sql);
    }

    #[test]
    fn subset_core__function_group_concat_rewrites_to_string_agg() {
        let r = translate("SELECT group_concat(v, '|') FROM t").unwrap();
        let low = r.sql.to_lowercase();
        assert!(low.contains("string_agg"), "{}", r.sql);
    }

    #[test]
    fn subset_core__function_printf_rewrites_to_format() {
        let r = translate("SELECT printf('%s-%d', name, id) FROM t").unwrap();
        let low = r.sql.to_lowercase();
        assert!(low.contains("format"), "{}", r.sql);
    }

    #[test]
    fn subset_core__function_julianday_rewrites_to_epoch_math() {
        let r = translate("SELECT julianday('now')").unwrap();
        let low = r.sql.to_lowercase();
        assert!(low.contains("extract"), "{}", r.sql);
        assert!(low.contains("epoch"), "{}", r.sql);
        assert!(low.contains("2440587.5"), "{}", r.sql);
    }

    #[test]
    fn subset_core__function_total_rewrites_to_coalesce_sum() {
        let r = translate("SELECT total(v) FROM t").unwrap();
        let low = r.sql.to_lowercase();
        assert!(low.contains("coalesce"), "{}", r.sql);
        assert!(low.contains("sum"), "{}", r.sql);
        assert!(low.contains("0.0"), "{}", r.sql);
    }

    #[test]
    fn subset_core__json_path_keeps_non_ascii_keys() {
        let r = translate(r#"SELECT json_extract(extra, '$."clé"') FROM t"#).unwrap();
        assert!(r.sql.contains("clé"), "{}", r.sql);
        assert!(!r.sql.contains('Ã'), "{}", r.sql);
    }
}
