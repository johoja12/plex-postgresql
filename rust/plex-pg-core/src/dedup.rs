use sqlparser::ast::*;
use sqlparser::tokenizer::Span;
use std::collections::HashMap;
use std::ops::ControlFlow;

/// Remove duplicate column assignments in UPDATE statements, keeping only the
/// last assignment for each column.  PostgreSQL rejects duplicate target columns
/// whereas SQLite silently accepts them (last-writer-wins).
///
/// A dropped value's parameters are kept: the caller still binds them
/// (Plex records a folder's scan time with
/// `SET updated_at=:U1, updated_at=:U2 WHERE id=:C1` and binds all three), and
/// a name the translation no longer has fails `sqlite3_bind_parameter_index`,
/// so the statement never runs. Each is referenced from a predicate that is
/// always true and ANDed onto the WHERE clause, so the bind succeeds and its
/// value changes nothing.
///
/// A dropped value holding a positional `?` used to be dropped outright,
/// which lost its slot: `SET a=?, a=? WHERE id=?` was prepared with two
/// parameters, so the WHERE's value was bound to the SET and the third bind
/// was past the end (2026-10-16 audit, item 16); and a dropped `:U1` moved
/// to the end took the slot after every `?`, so a `?` after it in the
/// statement was bound one slot early. Now, before anything moves, every
/// placeholder of such a statement is numbered the way SQLite numbers it
/// (`?` takes the slot after the largest so far, `?NNN` is slot NNN, a name
/// takes a new slot once) as an explicit token -- `?NNN`, or `?NNN:name` for
/// a named one -- which the placeholders pass, the next rule, turns into
/// `$NNN` (recording the name) wherever it stands. So the predicate at the
/// end shifts nothing, and SQLite's last value wins at SQLite's slot.
pub fn transform(stmt: &mut Statement) {
    let Statement::Update(update) = stmt else {
        return;
    };
    let dropped = duplicate_assignments(&update.assignments);
    if dropped.is_empty() {
        return;
    }
    if dropped
        .iter()
        .any(|&i| !placeholders(&update.assignments[i].value).is_empty())
    {
        number_placeholders(stmt);
    }
    let Statement::Update(update) = stmt else {
        unreachable!()
    };
    for value in dedup_assignments(&mut update.assignments) {
        keep_parameters(&mut update.selection, value);
    }
}

/// The indexes of every assignment a later one of the same column overrides.
fn duplicate_assignments(assignments: &[Assignment]) -> Vec<usize> {
    let mut seen = std::collections::HashSet::new();
    let mut dropped = Vec::new();
    for i in (0..assignments.len()).rev() {
        if !seen.insert(assignment_column_key(&assignments[i])) {
            dropped.push(i);
        }
    }
    dropped
}

/// Removes every assignment but the last per column and returns the values of
/// the removed ones, in statement order.
fn dedup_assignments(assignments: &mut Vec<Assignment>) -> Vec<Expr> {
    let mut dropped = duplicate_assignments(assignments);
    // Highest index first, so each removal leaves the lower ones in place.
    dropped.sort_unstable_by(|a, b| b.cmp(a));
    let mut values: Vec<Expr> = dropped
        .into_iter()
        .map(|i| assignments.remove(i).value)
        .collect();
    values.reverse();
    values
}

/// Every placeholder token of an expression, in order.
fn placeholders(value: &Expr) -> Vec<String> {
    let mut found = Vec::new();
    let _ = visit_expressions(value, |e| {
        if let Expr::Value(ValueWithSpan {
            value: Value::Placeholder(p),
            ..
        }) = e
        {
            found.push(p.clone());
        }
        ControlFlow::<()>::Continue(())
    });
    found
}

/// Rewrites every placeholder of the statement to its explicit slot,
/// numbering as SQLite does over the statement in order: `?` and `?NNN`
/// become `?NNN`; `:name`, `@name` and `$name` become `?NNN:name`, the form
/// `placeholders::transform` reads as slot NNN named `name`.
fn number_placeholders(stmt: &mut Statement) {
    let mut next = 1usize;
    let mut names: HashMap<String, usize> = HashMap::new();
    let _ = visit_expressions_mut(stmt, |e| {
        if let Expr::Value(ValueWithSpan {
            value: Value::Placeholder(token),
            ..
        }) = e
        {
            if token == "?" {
                *token = format!("?{next}");
                next += 1;
            } else if let Some(n) = token
                .strip_prefix(['?', '$'])
                .and_then(|digits| digits.parse::<usize>().ok())
            {
                *token = format!("?{n}");
                next = next.max(n + 1);
            } else if let Some(name) = token.strip_prefix([':', '@', '$']) {
                let n = *names.entry(name.to_string()).or_insert_with(|| {
                    next += 1;
                    next - 1
                });
                *token = format!("?{n}:{name}");
            }
        }
        ControlFlow::<()>::Continue(())
    });
}

/// ANDs `(CAST((value) AS TEXT) <> '' OR TRUE)` onto selection when value
/// holds a placeholder. The CAST types the parameter for PostgreSQL.
fn keep_parameters(selection: &mut Option<Expr>, value: Expr) {
    if placeholders(&value).is_empty() {
        return;
    }
    let cast = Expr::Cast {
        kind: CastKind::Cast,
        expr: Box::new(Expr::Nested(Box::new(value))),
        data_type: DataType::Text,
        array: false,
        format: None,
    };
    let keep = Expr::Nested(Box::new(Expr::BinaryOp {
        left: Box::new(Expr::BinaryOp {
            left: Box::new(cast),
            op: BinaryOperator::NotEq,
            right: Box::new(Expr::Value(ValueWithSpan {
                value: Value::SingleQuotedString(String::new()),
                span: Span::empty(),
            })),
        }),
        op: BinaryOperator::Or,
        right: Box::new(Expr::Value(ValueWithSpan {
            value: Value::Boolean(true),
            span: Span::empty(),
        })),
    }));
    *selection = Some(match selection.take() {
        Some(existing) => Expr::BinaryOp {
            left: Box::new(Expr::Nested(Box::new(existing))),
            op: BinaryOperator::And,
            right: Box::new(keep),
        },
        None => keep,
    });
}

/// Produce a lowercase, dot-joined string key for the target of an assignment
/// so that `a`, `"a"`, and `` `a` `` all compare equal.
fn assignment_column_key(assignment: &Assignment) -> String {
    match &assignment.target {
        AssignmentTarget::ColumnName(name) => name
            .0
            .iter()
            .map(|part| match part {
                ObjectNamePart::Identifier(ident) => ident.value.to_lowercase(),
                other => other.to_string().to_lowercase(),
            })
            .collect::<Vec<_>>()
            .join("."),
        // Tuple targets (a, b) = (1, 2) — stringify as-is for dedup key
        AssignmentTarget::Tuple(cols) => cols
            .iter()
            .map(|c| c.to_string().to_lowercase())
            .collect::<Vec<_>>()
            .join(","),
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use crate::translate;
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    /// The `$n` a named parameter became.
    fn placeholder(names: &[Option<String>], name: &str) -> String {
        let i = names
            .iter()
            .position(|n| n.as_deref() == Some(name))
            .unwrap_or_else(|| {
                panic!(
                    "{} is not a parameter of the translation: {:?}",
                    name, names
                )
            });
        format!("${}", i + 1)
    }

    // Plex records a scanned folder's time with this statement and binds
    // :U1, :U2 and :C1 by name (sqlite3_bind_parameter_index). Dropping
    // `updated_at`=:U1 dropped :U1 from the statement, the bind found no
    // such parameter, and Plex threw before the UPDATE ran: every library
    // scan of a changed folder aborted with std::exception (kind-cluster-plex,
    // 2026-10-06). Each name stays a parameter; the last assignment wins.
    #[test]
    fn subset_core__update_duplicate_set_column_keeps_last_and_every_named_parameter() {
        let r =
            translate("UPDATE directories SET `updated_at`=:U1,`updated_at`=:U2 WHERE `id`=:C1")
                .unwrap();
        let sql = r.sql.to_lowercase();
        for name in ["U1", "U2", "C1"] {
            placeholder(&r.param_names, name);
        }
        assert_eq!(r.param_names.len(), 3, "{:?}", r.param_names);

        let u2 = placeholder(&r.param_names, "U2");
        let c1 = placeholder(&r.param_names, "C1");
        assert!(
            sql.contains(&format!("set \"updated_at\" = {}", u2)),
            "the last assignment is kept, got: {}",
            r.sql
        );
        assert_eq!(
            sql.matches("\"updated_at\"").count(),
            1,
            "one assignment, got: {}",
            r.sql
        );
        assert!(sql.contains(&format!("\"id\" = {}", c1)), "got: {}", r.sql);
        Parser::parse_sql(&PostgreSqlDialect {}, &r.sql)
            .unwrap_or_else(|e| panic!("not PostgreSQL: {}: {}", r.sql, e));
    }

    // Without a WHERE clause the dropped parameter still has to stay.
    #[test]
    fn subset_core__update_duplicate_set_column_without_where_keeps_every_named_parameter() {
        let r = translate("UPDATE t SET `a`=:U1,`a`=:U2").unwrap();
        placeholder(&r.param_names, "U1");
        let u2 = placeholder(&r.param_names, "U2");
        assert!(
            r.sql
                .to_lowercase()
                .contains(&format!("set \"a\" = {}", u2)),
            "got: {}",
            r.sql
        );
        Parser::parse_sql(&PostgreSqlDialect {}, &r.sql)
            .unwrap_or_else(|e| panic!("not PostgreSQL: {}: {}", r.sql, e));
    }

    // A dropped value with no parameter leaves nothing to keep.
    #[test]
    fn subset_core__update_duplicate_set_column_drops_a_literal_outright() {
        let r = translate("UPDATE t SET `a`=1,`a`=:U1 WHERE `id`=:C1").unwrap();
        assert_eq!(
            r.param_names,
            vec![Some("U1".to_string()), Some("C1".to_string())]
        );
        assert!(!r.sql.to_lowercase().contains("or true"), "got: {}", r.sql);
    }

    // A dropped positional value keeps its slot: the statement binds three,
    // SQLite's last value ($2) is the one written, and the WHERE's is $3.
    #[test]
    fn subset_core__update_duplicate_set_column_keeps_a_positional_slot() {
        let r = translate("UPDATE t SET `a`=?,`a`=? WHERE `id`=?").unwrap();
        assert_eq!(r.param_names, vec![None, None, None]);
        assert_eq!(
            r.sql,
            "UPDATE t SET \"a\" = $2 WHERE (\"id\" = $3) AND (CAST(($1) AS TEXT) <> '' OR true)"
        );
        Parser::parse_sql(&PostgreSqlDialect {}, &r.sql)
            .unwrap_or_else(|e| panic!("not PostgreSQL: {}: {}", r.sql, e));
    }

    // Named and positional mixed: SQLite numbers :U1 first, then each `?`,
    // and the `?` after the dropped :U1 keep their slots once it moves.
    #[test]
    fn subset_core__update_duplicate_set_column_keeps_slots_beside_named_parameters() {
        let r = translate("UPDATE t SET `a`=:U1,`b`=?,`a`=? WHERE `id`=?").unwrap();
        assert_eq!(
            r.param_names,
            vec![Some("U1".to_string()), None, None, None]
        );
        assert_eq!(
            r.sql,
            "UPDATE t SET \"b\" = $2, \"a\" = $3 WHERE (\"id\" = $4) AND (CAST(($1) AS TEXT) <> '' OR true)"
        );
    }

    // A name used in the dropped value and again in the kept statement is one
    // slot, wherever each occurrence ends up.
    #[test]
    fn subset_core__update_duplicate_set_column_keeps_one_slot_per_repeated_name() {
        let r = translate("UPDATE t SET `a`=:V,`a`=? WHERE `id`=:V").unwrap();
        assert_eq!(r.param_names, vec![Some("V".to_string()), None]);
        assert_eq!(
            r.sql,
            "UPDATE t SET \"a\" = $2 WHERE (\"id\" = $1) AND (CAST(($1) AS TEXT) <> '' OR true)"
        );
    }
}
