//! Module: collation
//!
//! `tags.tag`, `metadata_items.title_sort` and `library_sections.name_sort`
//! carry the non-deterministic `plex.nocase` collation (stock Plex declares
//! them `COLLATE NOCASE`), which makes `=`, `IN` and btree lookups
//! case-insensitive. PostgreSQL refuses a pattern match on such a column
//! ("nondeterministic collations are not supported for LIKE"), so every
//! LIKE, ILIKE, SIMILAR TO, regular-expression operator and STRPOS whose
//! column operand is one of the three compares under the deterministic
//! `plex.icu_root` instead (2026-10-16).

use std::ops::ControlFlow;

use sqlparser::ast::{
    BinaryOperator, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, Ident, ObjectName,
    ObjectNamePart, Statement, VisitMut, VisitorMut,
};

/// The columns stock Plex declares `COLLATE NOCASE`.
const NOCASE_COLUMNS: &[&str] = &["tag", "title_sort", "name_sort"];

pub fn transform(stmt: &mut Statement) {
    let mut v = Collations;
    let _ = stmt.visit(&mut v);
}

struct Collations;

impl VisitorMut for Collations {
    type Break = ();

    fn post_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<Self::Break> {
        match expr {
            Expr::Like { expr: operand, .. }
            | Expr::ILike { expr: operand, .. }
            | Expr::SimilarTo { expr: operand, .. } => recollate(operand),
            Expr::BinaryOp { left, op, .. }
                if matches!(
                    op,
                    BinaryOperator::PGRegexMatch
                        | BinaryOperator::PGRegexIMatch
                        | BinaryOperator::PGRegexNotMatch
                        | BinaryOperator::PGRegexNotIMatch
                ) =>
            {
                recollate(left)
            }
            Expr::Function(f) if is_strpos(&f.name) => {
                if let FunctionArguments::List(list) = &mut f.args {
                    for arg in &mut list.args {
                        if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
                            recollate(e);
                        }
                    }
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }
}

fn is_strpos(name: &ObjectName) -> bool {
    matches!(
        name.0.last(),
        Some(ObjectNamePart::Identifier(i)) if i.value.eq_ignore_ascii_case("strpos")
            || i.value.eq_ignore_ascii_case("instr")
            || i.value.eq_ignore_ascii_case("position")
    )
}

/// Wraps a bare reference to a NOCASE column in `COLLATE plex.icu_root`.
fn recollate(expr: &mut Expr) {
    let column = match expr {
        Expr::Identifier(i) => Some(i.value.as_str()),
        Expr::CompoundIdentifier(parts) => parts.last().map(|i| i.value.as_str()),
        _ => None,
    };
    let Some(column) = column else {
        return;
    };
    if !NOCASE_COLUMNS
        .iter()
        .any(|c| column.eq_ignore_ascii_case(c))
    {
        return;
    }
    let inner = std::mem::replace(expr, Expr::Value(sqlparser::ast::Value::Null.into()));
    *expr = Expr::Nested(Box::new(Expr::Collate {
        expr: Box::new(inner),
        collation: ObjectName(vec![
            ObjectNamePart::Identifier(Ident::new("plex")),
            ObjectNamePart::Identifier(Ident::new("icu_root")),
        ]),
    }));
}
