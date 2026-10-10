use crate::byte_utils::push_utf8_byte;
use sqlparser::ast::helpers::attached_token::AttachedToken;
/// Module: functions
///
/// Rewrites SQLite function calls to their PostgreSQL equivalents:
///   IFNULL(a,b)           → COALESCE(a,b)
///   iif(cond,then,else)   → CASE WHEN cond THEN then ELSE else END
///   typeof(x)             → pg_typeof(x)::text
///   last_insert_rowid()   → lastval()
///   json_each(x)          → json_array_elements(x)
///   json_extract(x,'$.a') → jsonb_extract_path_text(x::jsonb,'a')
///   json_set(x,'$.a',v)   → jsonb_set(x::jsonb, string_to_array('a',','), to_jsonb(v), true)
///   SUBSTR(a,b,c)         → SUBSTRING(a,b,c)
///   instr(a,b)            → STRPOS(a,b)   (same arg order)
///   strftime('%s',…)      → EXTRACT(EPOCH FROM …)::bigint
///   strftime('%Y-%m-%d',…)→ TO_CHAR(…, 'YYYY-MM-DD')
///   unixepoch(…)          → EXTRACT(EPOCH FROM …)::bigint
///   datetime('now')       → NOW()
use sqlparser::ast::*;
use sqlparser::tokenizer::Span;

use crate::rewriter::ast_utils::{take_expr, wrap_double_colon_cast};

pub fn transform(stmt: &mut Statement) {
    transform_stmt(stmt);
}

fn transform_stmt(stmt: &mut Statement) {
    match stmt {
        Statement::Query(q) => transform_query(q),
        Statement::Insert(ins) => {
            if let Some(src) = &mut ins.source {
                transform_query(src);
            }
        }
        Statement::Update(update) => {
            for assign in &mut update.assignments {
                transform_expr(&mut assign.value);
            }
            if let Some(sel) = &mut update.selection {
                transform_expr(sel);
            }
        }
        Statement::Delete(del) => {
            if let Some(sel) = &mut del.selection {
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
    transform_set_expr(&mut q.body);
    if let Some(ob) = &mut q.order_by {
        if let OrderByKind::Expressions(exprs) = &mut ob.kind {
            for oe in exprs {
                transform_expr(&mut oe.expr);
            }
        }
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
    for item in &mut sel.projection {
        match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                transform_expr(e);
            }
            _ => {}
        }
    }
    for table in &mut sel.from {
        transform_table_with_joins(table);
    }

    // After FROM transformation, check if json_array_elements is present.
    // If so, cast bare `value` references in projection to ::text.
    if has_json_array_elements_in_from(&sel.from) {
        for item in &mut sel.projection {
            match item {
                SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                    cast_json_value_to_text(e);
                }
                _ => {}
            }
        }
    }

    if let Some(sel_where) = &mut sel.selection {
        transform_expr(sel_where);
    }
    match &mut sel.group_by {
        GroupByExpr::Expressions(exprs, _) => {
            for e in exprs {
                transform_expr(e);
            }
        }
        _ => {}
    }
    if let Some(having) = &mut sel.having {
        transform_expr(having);
    }
}

/// Check if any FROM table is json_array_elements (was json_each before transformation)
fn has_json_array_elements_in_from(from: &[TableWithJoins]) -> bool {
    for twj in from {
        if is_json_array_elements_table(&twj.relation) {
            return true;
        }
        for join in &twj.joins {
            if is_json_array_elements_table(&join.relation) {
                return true;
            }
        }
    }
    false
}

fn is_json_array_elements_table(tf: &TableFactor) -> bool {
    if let TableFactor::Table { name, .. } = tf {
        let table_name = name
            .0
            .last()
            .and_then(|p| match p {
                ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
                _ => None,
            })
            .unwrap_or_default();
        return table_name == "json_array_elements";
    }
    false
}

/// Cast bare `value` identifiers to ::text (for json_array_elements output)
fn cast_json_value_to_text(expr: &mut Expr) {
    if let Expr::Identifier(ref ident) = expr {
        if ident.value.to_lowercase() == "value" {
            *expr = wrap_double_colon_cast(take_expr(expr), DataType::Text);
        }
    }
}

fn transform_table_with_joins(t: &mut TableWithJoins) {
    transform_table_factor(&mut t.relation);
    for join in &mut t.joins {
        transform_table_factor(&mut join.relation);
        match &mut join.join_operator {
            JoinOperator::Join(JoinConstraint::On(e))
            | JoinOperator::Inner(JoinConstraint::On(e))
            | JoinOperator::LeftOuter(JoinConstraint::On(e))
            | JoinOperator::RightOuter(JoinConstraint::On(e))
            | JoinOperator::FullOuter(JoinConstraint::On(e)) => {
                transform_expr(e);
            }
            _ => {}
        }
    }
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
        "json_each" => "json_array_elements",
        "json_tree" => "jsonb_each_text",
        _ => return None,
    };
    if let Some(last) = name.0.last_mut() {
        if let ObjectNamePart::Identifier(i) = last {
            i.value = target.to_string();
            return Some(target);
        }
    }
    None
}

fn cast_expr_to_json_inplace(expr: &mut Expr) {
    *expr = wrap_double_colon_cast(take_expr(expr), DataType::JSON);
}

fn cast_unnamed_function_args_to_json(args: &mut [FunctionArg]) {
    for arg in args.iter_mut() {
        if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
            cast_expr_to_json_inplace(e);
        }
    }
}

fn cast_expr_to_jsonb_inplace(expr: &mut Expr) {
    *expr = wrap_double_colon_cast(take_expr(expr), DataType::JSONB);
}

fn cast_unnamed_function_args_to_jsonb(args: &mut [FunctionArg]) {
    for arg in args.iter_mut() {
        if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
            cast_expr_to_jsonb_inplace(e);
        }
    }
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

fn make_jsonb_cast(expr: Expr) -> Expr {
    Expr::Cast {
        kind: CastKind::DoubleColon,
        expr: Box::new(expr),
        data_type: DataType::Custom(
            ObjectName(vec![ObjectNamePart::Identifier(Ident::new("jsonb"))]),
            vec![],
        ),
        array: false,
        format: None,
    }
}

fn make_to_jsonb(expr: Expr) -> Expr {
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

fn make_jsonb_path_exists(doc: Expr, path_segments: &[String]) -> Expr {
    make_function_call(
        "jsonb_path_exists",
        vec![
            doc,
            make_string_literal(&jsonpath_from_segments(path_segments)),
        ],
    )
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
    if let Expr::Value(ValueWithSpan {
        value: Value::SingleQuotedString(path),
        ..
    }) = expr
    {
        return parse_json_path_segments(path);
    }
    None
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

fn rewrite_json_extract_call(mut args: Vec<Expr>) -> Option<Expr> {
    if args.len() < 2 {
        return None;
    }
    let doc = make_jsonb_cast(args.remove(0));
    let path = args.remove(0);
    if let Some(segs) = get_literal_json_path(&path) {
        if segs.is_empty() {
            return Some(wrap_double_colon_cast(doc, DataType::Text));
        }

        let mut fn_args = vec![doc];
        fn_args.extend(segs.iter().map(|s| make_string_literal(s)));
        return Some(make_function_call("jsonb_extract_path_text", fn_args));
    }

    // Fallback for richer SQLite JSON paths (wildcards/filters/etc.):
    // use PostgreSQL jsonpath execution and cast to text.
    let path_str = get_literal_single_quoted_string(&path)?;
    let jsonpath_call = make_function_call(
        "jsonb_path_query_first",
        vec![doc, make_string_literal(&path_str)],
    );
    Some(wrap_double_colon_cast(jsonpath_call, DataType::Text))
}

fn rewrite_json_type_call(mut args: Vec<Expr>) -> Option<Expr> {
    if args.is_empty() {
        return None;
    }
    let doc = make_jsonb_cast(args.remove(0));
    if args.is_empty() {
        return Some(make_function_call("jsonb_typeof", vec![doc]));
    }
    if let Some(segs) = get_literal_json_path(&args[0]) {
        let mut p = vec![doc];
        p.extend(segs.iter().map(|s| make_string_literal(s)));
        return Some(make_function_call(
            "jsonb_typeof",
            vec![make_function_call("jsonb_extract_path", p)],
        ));
    }
    let path_str = get_literal_single_quoted_string(&args[0])?;
    Some(make_function_call(
        "jsonb_typeof",
        vec![make_function_call(
            "jsonb_path_query_first",
            vec![doc, make_string_literal(&path_str)],
        )],
    ))
}

fn rewrite_json_array_length_call(mut args: Vec<Expr>) -> Option<Expr> {
    if args.is_empty() {
        return None;
    }
    let doc = make_jsonb_cast(args.remove(0));
    if args.is_empty() {
        return Some(make_function_call("jsonb_array_length", vec![doc]));
    }
    if let Some(segs) = get_literal_json_path(&args[0]) {
        let mut p = vec![doc];
        p.extend(segs.iter().map(|s| make_string_literal(s)));
        return Some(make_function_call(
            "jsonb_array_length",
            vec![make_function_call("jsonb_extract_path", p)],
        ));
    }
    let path_str = get_literal_single_quoted_string(&args[0])?;
    Some(make_function_call(
        "jsonb_array_length",
        vec![make_function_call(
            "jsonb_path_query_first",
            vec![doc, make_string_literal(&path_str)],
        )],
    ))
}

enum JsonSetMode {
    Set,
    Insert,
    Replace,
}

fn rewrite_json_set_like_call(args: Vec<Expr>, mode: JsonSetMode) -> Option<Expr> {
    if args.len() < 3 {
        return None;
    }
    let mut iter = args.into_iter();
    let first = iter.next()?;
    let mut current = make_jsonb_cast(first);
    let rest: Vec<Expr> = iter.collect();
    let mut i = 0usize;
    let mut rewrote_any = false;
    while i + 1 < rest.len() {
        let path_expr = &rest[i];
        let value_expr = rest[i + 1].clone();
        if let Some(path_segments) = get_literal_json_path(path_expr) {
            let should_create = matches!(mode, JsonSetMode::Set | JsonSetMode::Insert);
            let rewritten = make_function_call(
                "jsonb_set",
                vec![
                    current.clone(),
                    make_path_text_array_expr(&path_segments),
                    make_to_jsonb(value_expr),
                    Expr::Value(ValueWithSpan {
                        value: Value::Boolean(should_create),
                        span: Span::empty(),
                    }),
                ],
            );
            current = match mode {
                JsonSetMode::Set => rewritten,
                JsonSetMode::Insert => make_case_when(
                    make_jsonb_path_exists(current.clone(), &path_segments),
                    current,
                    rewritten,
                ),
                JsonSetMode::Replace => make_case_when(
                    make_jsonb_path_exists(current.clone(), &path_segments),
                    rewritten,
                    current,
                ),
            };
            rewrote_any = true;
        }
        i += 2;
    }
    if rewrote_any {
        Some(current)
    } else {
        None
    }
}

fn rewrite_json_remove_call(args: Vec<Expr>) -> Option<Expr> {
    if args.len() < 2 {
        return None;
    }
    let mut iter = args.into_iter();
    let first = iter.next()?;
    let mut current = make_jsonb_cast(first);
    let mut rewrote_any = false;
    for path_expr in iter {
        if let Some(path_segments) = get_literal_json_path(&path_expr) {
            if path_segments.is_empty() {
                continue;
            }
            current = Expr::BinaryOp {
                left: Box::new(current),
                op: BinaryOperator::PGCustomBinaryOperator(vec!["#-".to_string()]),
                right: Box::new(make_path_text_array_expr(&path_segments)),
            };
            rewrote_any = true;
        }
    }
    if rewrote_any {
        Some(current)
    } else {
        None
    }
}

fn rewrite_json_patch_call(args: Vec<Expr>) -> Option<Expr> {
    if args.len() != 2 {
        return None;
    }
    let left = make_jsonb_cast(args[0].clone());
    let right = make_jsonb_cast(args[1].clone());
    Some(make_function_call("jsonb_mergepatch", vec![left, right]))
}

fn rewrite_group_concat_call(args: Vec<Expr>) -> Option<Expr> {
    if args.is_empty() {
        return None;
    }
    let value_expr = wrap_double_colon_cast(args[0].clone(), DataType::Text);
    let sep_expr = if args.len() > 1 {
        args[1].clone()
    } else {
        make_string_literal(",")
    };
    Some(make_function_call("string_agg", vec![value_expr, sep_expr]))
}

fn rewrite_julianday_call(args: Vec<Expr>) -> Option<Expr> {
    let source = args.into_iter().next().unwrap_or_else(make_now_call);
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
    let days = Expr::BinaryOp {
        left: Box::new(epoch_double),
        op: BinaryOperator::Divide,
        right: Box::new(make_number_literal("86400.0")),
    };
    Some(Expr::BinaryOp {
        left: Box::new(days),
        op: BinaryOperator::Plus,
        right: Box::new(make_number_literal("2440587.5")),
    })
}

fn rewrite_total_call(args: Vec<Expr>) -> Option<Expr> {
    let value = args.into_iter().next()?;
    let sum_expr = make_function_call("SUM", vec![value]);
    Some(make_function_call(
        "COALESCE",
        vec![sum_expr, make_number_literal("0.0")],
    ))
}

fn transform_table_factor(f: &mut TableFactor) {
    match f {
        TableFactor::Derived { subquery, .. } => {
            transform_query(subquery);
        }
        TableFactor::Table { name, args, .. } => {
            // SQLite JSON SRFs in FROM clause:
            // - json_each(x) -> json_array_elements(x::json)
            // - json_tree(x) -> jsonb_each_text(x::jsonb)
            if let Some(target) = rename_sqlite_json_table_fn_name_if_present(name) {
                if let Some(TableFunctionArgs { args, .. }) = args {
                    if target == "jsonb_each_text" {
                        cast_unnamed_function_args_to_jsonb(args);
                    } else {
                        cast_unnamed_function_args_to_json(args);
                    }
                }
            }
        }
        TableFactor::Function { name, args, .. } => {
            if let Some(target) = rename_sqlite_json_table_fn_name_if_present(name) {
                if target == "jsonb_each_text" {
                    cast_unnamed_function_args_to_jsonb(args);
                } else {
                    cast_unnamed_function_args_to_json(args);
                }
            }
        }
        TableFactor::TableFunction { expr, .. } => {
            if let Expr::Function(func) = expr {
                if let Some(target) = rename_sqlite_json_table_fn_name_if_present(&mut func.name) {
                    if let FunctionArguments::List(arg_list) = &mut func.args {
                        if target == "jsonb_each_text" {
                            cast_unnamed_function_args_to_jsonb(&mut arg_list.args);
                        } else {
                            cast_unnamed_function_args_to_json(&mut arg_list.args);
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// Recursively transform an expression in place, rewriting SQLite function
/// calls to their PostgreSQL equivalents.
fn transform_expr(expr: &mut Expr) {
    match expr {
        Expr::Function(ref mut func) => {
            // Get the lowercase function name for matching
            let fn_name = func
                .name
                .0
                .first()
                .and_then(|p| match p {
                    ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
                    _ => None,
                })
                .unwrap_or_default();

            // simplify_typeof_fixup: iif(typeof(X) in ('integer','real'), X, ...) → X
            // Must run BEFORE iif→CASE conversion
            if fn_name == "iif" {
                if let Some(simplified) = try_simplify_typeof(func) {
                    *expr = simplified;
                    transform_expr(expr);
                    return;
                }
            }

            // First recurse into the args
            if let FunctionArguments::List(ref mut arg_list) = func.args {
                for arg in &mut arg_list.args {
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

            match fn_name.as_str() {
                // IFNULL → COALESCE
                "ifnull" => {
                    rename_function(func, "COALESCE");
                }

                // iif(cond, then, else) → CASE WHEN cond THEN then ELSE else END
                "iif" => {
                    let args = extract_unnamed_args(func);
                    if args.len() >= 3 {
                        let mut iter = args.into_iter();
                        let cond = iter.next().unwrap();
                        let then = iter.next().unwrap();
                        let else_e = iter.next().unwrap();
                        *expr = Expr::Case {
                            case_token: AttachedToken::empty(),
                            end_token: AttachedToken::empty(),
                            operand: None,
                            conditions: vec![CaseWhen {
                                condition: cond,
                                result: then,
                            }],
                            else_result: Some(Box::new(else_e)),
                        };
                    }
                }

                // typeof(x) → pg_typeof(x)::text
                "typeof" => {
                    rename_function(func, "pg_typeof");
                    *expr = wrap_double_colon_cast(take_expr(expr), DataType::Text);
                }

                // last_insert_rowid() → lastval()
                "last_insert_rowid" => {
                    rename_function(func, "lastval");
                }

                // json_each(x) → json_array_elements(x)
                "json_each" => {
                    rename_function(func, "json_array_elements");
                }

                // unicode(x) → ASCII(x)
                "unicode" => {
                    rename_function(func, "ASCII");
                }

                // json_tree(x) → jsonb_each_text(x::jsonb)
                "json_tree" => {
                    rename_function(func, "jsonb_each_text");
                    if let FunctionArguments::List(ref mut arg_list) = func.args {
                        cast_unnamed_function_args_to_jsonb(&mut arg_list.args);
                    }
                }

                // json_extract(x, '$.a.b') -> jsonb_extract_path_text(x::jsonb, 'a', 'b')
                "json_extract" => {
                    let args = extract_unnamed_args(func);
                    if let Some(new_expr) = rewrite_json_extract_call(args) {
                        *expr = new_expr;
                    }
                }

                // json_type(x[, '$.a']) -> jsonb_typeof(...)
                "json_type" => {
                    let args = extract_unnamed_args(func);
                    if let Some(new_expr) = rewrite_json_type_call(args) {
                        *expr = new_expr;
                    }
                }

                // json_array_length(x[, '$.a']) -> jsonb_array_length(...)
                "json_array_length" => {
                    let args = extract_unnamed_args(func);
                    if let Some(new_expr) = rewrite_json_array_length_call(args) {
                        *expr = new_expr;
                    }
                }

                // json_set/json_insert/json_replace -> jsonb_set(...)
                "json_set" => {
                    let args = extract_unnamed_args(func);
                    if let Some(new_expr) = rewrite_json_set_like_call(args, JsonSetMode::Set) {
                        *expr = new_expr;
                    }
                }
                "json_insert" => {
                    let args = extract_unnamed_args(func);
                    if let Some(new_expr) = rewrite_json_set_like_call(args, JsonSetMode::Insert) {
                        *expr = new_expr;
                    }
                }
                "json_replace" => {
                    let args = extract_unnamed_args(func);
                    if let Some(new_expr) = rewrite_json_set_like_call(args, JsonSetMode::Replace) {
                        *expr = new_expr;
                    }
                }
                "json_remove" => {
                    let args = extract_unnamed_args(func);
                    if let Some(new_expr) = rewrite_json_remove_call(args) {
                        *expr = new_expr;
                    }
                }
                "json_patch" => {
                    let args = extract_unnamed_args(func);
                    if let Some(new_expr) = rewrite_json_patch_call(args) {
                        *expr = new_expr;
                    }
                }
                "json_object" => {
                    rename_function(func, "jsonb_build_object");
                }
                "json_array" => {
                    rename_function(func, "jsonb_build_array");
                }
                "json_group_array" => {
                    rename_function(func, "jsonb_agg");
                }
                "json_group_object" => {
                    rename_function(func, "jsonb_object_agg");
                }
                "group_concat" => {
                    let args = extract_unnamed_args(func);
                    if let Some(new_expr) = rewrite_group_concat_call(args) {
                        *expr = new_expr;
                    }
                }
                "printf" => {
                    rename_function(func, "format");
                }
                "julianday" => {
                    let args = extract_unnamed_args(func);
                    if let Some(new_expr) = rewrite_julianday_call(args) {
                        *expr = new_expr;
                    }
                }
                "total" => {
                    let args = extract_unnamed_args(func);
                    if let Some(new_expr) = rewrite_total_call(args) {
                        *expr = new_expr;
                    }
                }

                // json_quote(x) -> to_json(x)::text
                "json_quote" => {
                    let args = extract_unnamed_args(func);
                    if let Some(first) = args.into_iter().next() {
                        *expr = wrap_double_colon_cast(
                            make_function_call("to_json", vec![first]),
                            DataType::Text,
                        );
                    }
                }

                // SUBSTR → SUBSTRING
                "substr" => {
                    rename_function(func, "SUBSTRING");
                }

                // instr(haystack, needle) → STRPOS(haystack, needle)  (same arg order)
                "instr" => {
                    rename_function(func, "STRPOS");
                }

                // strftime('%s', expr) → EXTRACT(EPOCH FROM expr)::bigint
                // strftime('%Y-%m-%d', expr) → TO_CHAR(expr, 'YYYY-MM-DD')
                "strftime" => {
                    // Need to take the func out to pass to transform_strftime
                    let func_node = match take_expr(expr) {
                        Expr::Function(f) => f,
                        _ => unreachable!(),
                    };
                    *expr = transform_strftime(func_node);
                }

                // unixepoch(expr) → EXTRACT(EPOCH FROM expr)::bigint
                // unixepoch('now', '-7 day') → EXTRACT(EPOCH FROM NOW() - INTERVAL '7 day')::bigint
                "unixepoch" => {
                    let args = extract_unnamed_args(func);
                    let mut iter = args.into_iter();
                    let inner_expr = iter.next().unwrap_or_else(make_now_call);
                    let mut source = if is_now_literal(&inner_expr) {
                        make_now_call()
                    } else {
                        inner_expr
                    };
                    if let Some(interval_expr) = iter.next() {
                        source = apply_interval(source, interval_expr);
                    }
                    *expr = Expr::Cast {
                        kind: CastKind::DoubleColon,
                        expr: Box::new(Expr::Extract {
                            field: DateTimeField::Epoch,
                            syntax: ExtractSyntax::From,
                            expr: Box::new(source),
                        }),
                        data_type: DataType::BigInt(None),
                        array: false,
                        format: None,
                    };
                }

                // datetime('now') → NOW()
                "datetime" => {
                    let args = extract_unnamed_args(func);
                    let inner = args.into_iter().next().unwrap_or_else(make_now_call);
                    if is_now_literal(&inner) {
                        *expr = make_now_call();
                    } else {
                        *expr = inner;
                    }
                }

                _ => {}
            }
        }

        // Recurse into other expression forms
        Expr::BinaryOp { left, right, .. } => {
            transform_expr(left);
            transform_expr(right);
        }
        Expr::UnaryOp { expr: inner, .. } => transform_expr(inner),
        Expr::Nested(inner) => transform_expr(inner),
        Expr::Cast { expr: inner, .. } => transform_expr(inner),
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(op) = operand {
                transform_expr(op);
            }
            for cw in conditions {
                transform_expr(&mut cw.condition);
                transform_expr(&mut cw.result);
            }
            if let Some(er) = else_result {
                transform_expr(er);
            }
        }
        Expr::InList {
            expr: inner, list, ..
        } => {
            transform_expr(inner);
            for e in list.iter_mut() {
                transform_expr(e);
            }
            // typeof type remapping: when expr is pg_typeof(...)::text,
            // add PostgreSQL type aliases to the IN list
            if is_pg_typeof_cast(inner) {
                add_typeof_remappings(list);
            }
        }
        Expr::InSubquery {
            expr: inner,
            subquery,
            ..
        } => {
            transform_expr(inner);
            transform_query(subquery);
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
        // SUBSTR(a,b,c) is parsed as Expr::Substring { shorthand: true }
        // We convert it to SUBSTRING by clearing the shorthand flag
        Expr::Substring {
            expr: inner,
            substring_from,
            substring_for,
            shorthand,
            ..
        } => {
            transform_expr(inner);
            if let Some(f) = substring_from {
                transform_expr(f);
            }
            if let Some(f) = substring_for {
                transform_expr(f);
            }
            if *shorthand {
                *shorthand = false; // force SUBSTRING output
            }
        }
        Expr::Exists { subquery, .. } => {
            transform_query(subquery);
        }
        Expr::Subquery(q) => {
            transform_query(q);
        }
        _ => {}
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn rename_function(func: &mut Function, new_name: &str) {
    func.name = ObjectName(vec![ObjectNamePart::Identifier(Ident::new(new_name))]);
}

/// Check if an expression is pg_typeof(...)::text (the result of typeof → pg_typeof conversion)
fn is_pg_typeof_cast(expr: &Expr) -> bool {
    if let Expr::Cast {
        expr: inner,
        data_type: DataType::Text,
        ..
    } = expr
    {
        if let Expr::Function(func) = inner.as_ref() {
            let name = func
                .name
                .0
                .first()
                .and_then(|p| match p {
                    ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
                    _ => None,
                })
                .unwrap_or_default();
            return name == "pg_typeof";
        }
    }
    false
}

/// Remap PostgreSQL type names in an IN list for pg_typeof comparisons.
/// SQLite typeof returns 'integer', 'real', etc. PostgreSQL pg_typeof returns
/// 'bigint', 'double precision', etc.
/// - 'integer' → keep AND add 'bigint'
/// - 'real' → REPLACE with 'double precision'
fn add_typeof_remappings(list: &mut Vec<Expr>) {
    let mut additions = Vec::new();
    for item in list.iter_mut() {
        if let Expr::Value(ValueWithSpan {
            value: Value::SingleQuotedString(ref mut s),
            ..
        }) = item
        {
            match s.to_lowercase().as_str() {
                "integer" => {
                    additions.push(make_string_literal("bigint"));
                }
                "real" => {
                    // Replace 'real' with 'double precision' (not alongside)
                    *s = "double precision".to_string();
                }
                _ => {}
            }
        }
    }
    list.extend(additions);
}

/// Detect the pattern: iif(typeof(X) in ('integer','real'), X, strftime('%s', X, 'utc'))
/// and simplify to just X. In PostgreSQL, columns have fixed types so the
/// conditional typeof check is unnecessary.
fn try_simplify_typeof(func: &Function) -> Option<Expr> {
    if let FunctionArguments::List(ref al) = func.args {
        if al.args.len() >= 2 {
            // First arg should be: typeof(X) IN ('integer', 'real', ...)
            let first = match &al.args[0] {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => e,
                _ => return None,
            };
            // Check if first arg is typeof(...) IN (...)
            if let Expr::InList { expr, .. } = first {
                if let Expr::Function(inner_func) = expr.as_ref() {
                    let inner_name = inner_func
                        .name
                        .0
                        .first()
                        .and_then(|p| match p {
                            ObjectNamePart::Identifier(i) => Some(i.value.to_lowercase()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    if inner_name == "typeof" {
                        // Second arg is the "then" value — just return it
                        if let FunctionArg::Unnamed(FunctionArgExpr::Expr(then_expr)) = &al.args[1]
                        {
                            return Some(then_expr.clone());
                        }
                    }
                }
            }
        }
    }
    None
}

/// Extract all `FunctionArg::Unnamed(FunctionArgExpr::Expr(…))` args.
fn extract_unnamed_args(func: &mut Function) -> Vec<Expr> {
    let mut result = Vec::new();
    if let FunctionArguments::List(ref mut al) = func.args {
        for arg in al.args.drain(..) {
            if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
                result.push(e);
            }
        }
    }
    result
}

fn is_now_literal(expr: &Expr) -> bool {
    matches!(expr, Expr::Value(ValueWithSpan { value: Value::SingleQuotedString(s), .. }) if s.to_lowercase() == "now")
}

fn make_now_call() -> Expr {
    Expr::Function(Function {
        name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new("NOW"))]),
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        args: FunctionArguments::List(FunctionArgumentList {
            duplicate_treatment: None,
            args: vec![],
            clauses: vec![],
        }),
        filter: None,
        null_treatment: None,
        over: None,
        within_group: vec![],
    })
}

/// Apply a SQLite interval modifier (e.g. `'-7 day'`) to a source expression.
/// Produces `source + INTERVAL '...'` or `source - INTERVAL '...'` depending on sign.
fn apply_interval(source: Expr, interval_expr: Expr) -> Expr {
    let interval_str = match &interval_expr {
        Expr::Value(ValueWithSpan {
            value: Value::SingleQuotedString(s),
            ..
        }) => s.clone(),
        _ => return source,
    };

    let (op, cleaned) = if let Some(rest) = interval_str.strip_prefix('-') {
        (BinaryOperator::Minus, rest.trim().to_string())
    } else if let Some(rest) = interval_str.strip_prefix('+') {
        (BinaryOperator::Plus, rest.trim().to_string())
    } else {
        (BinaryOperator::Plus, interval_str.trim().to_string())
    };

    let interval = Expr::Interval(Interval {
        value: Box::new(Expr::Value(ValueWithSpan {
            value: Value::SingleQuotedString(cleaned),
            span: Span::empty(),
        })),
        leading_field: None,
        leading_precision: None,
        last_field: None,
        fractional_seconds_precision: None,
    });

    Expr::BinaryOp {
        left: Box::new(source),
        op,
        right: Box::new(interval),
    }
}

fn transform_strftime(mut func: Function) -> Expr {
    let args = extract_unnamed_args(&mut func);
    if args.len() < 2 {
        return Expr::Function(func);
    }
    let mut iter = args.into_iter();
    let fmt_arg = iter.next().unwrap();
    let time_arg = iter.next().unwrap();
    let interval_arg = iter.next();

    let fmt_str = match &fmt_arg {
        Expr::Value(ValueWithSpan {
            value: Value::SingleQuotedString(s),
            ..
        }) => s.clone(),
        _ => return Expr::Function(func),
    };

    let is_now = is_now_literal(&time_arg);
    let mut source = if is_now { make_now_call() } else { time_arg };

    // Apply interval if 3rd arg present
    if let Some(interval_expr) = interval_arg {
        source = apply_interval(source, interval_expr);
    }

    match fmt_str.as_str() {
        "%s" => {
            // For non-NOW column sources, wrap in TO_TIMESTAMP() first
            // because the column likely stores a Unix timestamp integer
            if !is_now {
                source = make_to_timestamp(source);
            }
            // → EXTRACT(EPOCH FROM source)::bigint
            Expr::Cast {
                kind: CastKind::DoubleColon,
                expr: Box::new(Expr::Extract {
                    field: DateTimeField::Epoch,
                    syntax: ExtractSyntax::From,
                    expr: Box::new(source),
                }),
                data_type: DataType::BigInt(None),
                array: false,
                format: None,
            }
        }
        _ => {
            // Translate strftime format to PostgreSQL TO_CHAR format
            let pg_fmt_str = translate_strftime_format(&fmt_str);
            make_to_char(source, &pg_fmt_str)
        }
    }
}

/// Translate a SQLite strftime format string to a PostgreSQL TO_CHAR format string.
fn translate_strftime_format(fmt: &str) -> String {
    let mut result = String::new();
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            if let Some(&spec) = chars.peek() {
                chars.next();
                match spec {
                    'Y' => result.push_str("YYYY"),
                    'm' => result.push_str("MM"),
                    'd' => result.push_str("DD"),
                    'H' => result.push_str("HH24"),
                    'M' => result.push_str("MI"),
                    'S' => result.push_str("SS"),
                    '%' => result.push('%'),
                    other => {
                        result.push('%');
                        result.push(other);
                    }
                }
            } else {
                result.push('%');
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Build a TO_TIMESTAMP(expr) function call.
fn make_to_timestamp(source: Expr) -> Expr {
    Expr::Function(Function {
        name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new("TO_TIMESTAMP"))]),
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        args: FunctionArguments::List(FunctionArgumentList {
            duplicate_treatment: None,
            args: vec![FunctionArg::Unnamed(FunctionArgExpr::Expr(source))],
            clauses: vec![],
        }),
        filter: None,
        null_treatment: None,
        over: None,
        within_group: vec![],
    })
}

/// Build a TO_CHAR(expr, 'fmt') function call.
fn make_to_char(source: Expr, pg_fmt: &str) -> Expr {
    let fmt_expr = Expr::Value(ValueWithSpan {
        value: Value::SingleQuotedString(pg_fmt.to_string()),
        span: sqlparser::tokenizer::Span::empty(),
    });
    Expr::Function(Function {
        name: ObjectName(vec![ObjectNamePart::Identifier(Ident::new("TO_CHAR"))]),
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        args: FunctionArguments::List(FunctionArgumentList {
            duplicate_treatment: None,
            args: vec![
                FunctionArg::Unnamed(FunctionArgExpr::Expr(source)),
                FunctionArg::Unnamed(FunctionArgExpr::Expr(fmt_expr)),
            ],
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
    fn subset_core__function_typeof_to_pg_typeof() {
        let r = translate("SELECT typeof(x) FROM t").unwrap();
        assert!(r.sql.contains("pg_typeof") || r.sql.contains("PG_TYPEOF"));
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
    fn debug_parse() {
        use sqlparser::dialect::SQLiteDialect;
        use sqlparser::parser::Parser;

        let sql1 = "SELECT strftime('%Y-%m-%d %H:%M:%S', created_at) FROM t";
        let stmts = Parser::parse_sql(&SQLiteDialect {}, sql1);
        eprintln!("Parse1: {:?}", stmts);

        let sql2 = "SELECT strftime('%s', 'now', '-7 day')";
        let stmts2 = Parser::parse_sql(&SQLiteDialect {}, sql2);
        eprintln!("Parse2: {:?}", stmts2);

        let sql3 = "SELECT datetime('now') FROM t";
        let stmts3 = Parser::parse_sql(&SQLiteDialect {}, sql3);
        eprintln!("Parse3: {:?}", stmts3);
    }

    #[test]
    fn subset_core__json_path_keeps_non_ascii_keys() {
        let r = translate(r#"SELECT json_extract(extra, '$."clé"') FROM t"#).unwrap();
        assert!(r.sql.contains("clé"), "{}", r.sql);
        assert!(!r.sql.contains('Ã'), "{}", r.sql);
    }
}
