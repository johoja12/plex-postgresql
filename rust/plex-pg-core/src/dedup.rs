use sqlparser::ast::*;
use sqlparser::dialect::SQLiteDialect;
use sqlparser::parser::Parser;
use std::ops::ControlFlow;

/// Remove duplicate column assignments in UPDATE statements, keeping only the
/// last assignment for each column.  PostgreSQL rejects duplicate target columns
/// whereas SQLite silently accepts them (last-writer-wins).
///
/// A dropped value's named parameters are kept: the caller still binds them
/// by name (Plex records a folder's scan time with
/// `SET updated_at=:U1, updated_at=:U2 WHERE id=:C1` and binds all three), and
/// a name the translation no longer has fails `sqlite3_bind_parameter_index`,
/// so the statement never runs. Each is referenced from a predicate that is
/// always true and ANDed onto the WHERE clause, so the bind succeeds and its
/// value changes nothing. A dropped value holding a positional `?` is dropped
/// outright as before: a predicate at the end would shift the positions of the
/// parameters after it.
pub fn transform(stmt: &mut Statement) {
    if let Statement::Update(update) = stmt {
        for value in dedup_assignments(&mut update.assignments) {
            keep_named_parameters(&mut update.selection, &value);
        }
    }
}

/// Removes every assignment but the last per column and returns the values of
/// the removed ones, in statement order.
fn dedup_assignments(assignments: &mut Vec<Assignment>) -> Vec<Expr> {
    // Walk backwards so the *last* assignment for a given column is the one we
    // keep (it is encountered first during the reverse scan).
    let mut seen = std::collections::HashSet::new();
    let mut dropped = Vec::new();
    let mut i = assignments.len();
    while i > 0 {
        i -= 1;
        let key = assignment_column_key(&assignments[i]);
        if !seen.insert(key) {
            dropped.push(assignments.remove(i).value);
        }
    }
    dropped.reverse();
    dropped
}

/// ANDs `(CAST(value AS TEXT) <> '' OR TRUE)` onto selection (a comparison,
/// not IS NULL, which the placeholders pass does not descend into) when value holds
/// named parameters and no positional one.
fn keep_named_parameters(selection: &mut Option<Expr>, value: &Expr) {
    let mut placeholders = Vec::new();
    let _ = visit_expressions(value, |e| {
        if let Expr::Value(v) = e {
            if let Value::Placeholder(p) = &v.value {
                placeholders.push(p.clone());
            }
        }
        ControlFlow::<()>::Continue(())
    });
    if placeholders.is_empty() || placeholders.iter().any(|p| p.starts_with('?')) {
        return;
    }
    let text = value.to_string();
    let keep = format!("(CAST(({}) AS TEXT) <> '' OR TRUE)", text);
    let keep = match Parser::new(&SQLiteDialect {})
        .try_with_sql(&keep)
        .and_then(|mut p| p.parse_expr())
    {
        Ok(expr) => expr,
        Err(_) => return,
    };
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
}
