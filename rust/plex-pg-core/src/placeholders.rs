/// Module: placeholders
///
/// Replaces SQLite's placeholders with PostgreSQL's `$1`, `$2`, ... and
/// records `param_names`: `None` for a positional slot, `Some("name")` for a
/// named one (the prefix dropped, as the bind layer compares names).
///
/// The numbering is SQLite's (sqlite3_bind_parameter_index): a `?` takes the
/// slot after the largest assigned so far, `?NNN` is slot NNN (the same slot
/// wherever it repeats), and `:name`, `@name` and `$name` take a new slot the
/// first time and reuse it after (`?NNN:name` is the dedup pass's token for a
/// name whose slot it fixed before moving it). Slots are assigned in source order over the
/// whole statement through sqlparser's `VisitorMut`, so a placeholder in any
/// position -- an IS NULL operand, COLLATE, SUBSTRING, a window's PARTITION BY
/// or FILTER, a tuple, a CTE, EXPLAIN, ON CONFLICT DO UPDATE, LIMIT and OFFSET
/// in either form -- is numbered; the hand-written walk this replaced skipped
/// every one of those (`_ => {}`), so Plex's `? IS NULL` was prepared with one
/// parameter fewer than it bound (2026-10-16 audit, item 8).
///
/// `LIMIT ?, ?` (MySQL/SQLite order: offset first) is numbered offset, then
/// limit, and rewritten to `LIMIT $2 OFFSET $1`, so the bind order stays
/// SQLite's.
use sqlparser::ast::*;
use std::collections::HashMap;
use std::ops::ControlFlow;

pub fn transform(stmt: &mut Statement, param_names: &mut Vec<Option<String>>) {
    let mut numbering = Numbering {
        param_names,
        names: HashMap::new(),
    };
    let _ = stmt.visit(&mut numbering);
}

struct Numbering<'a> {
    /// One entry per slot; `len() + 1` is the next fresh slot.
    param_names: &'a mut Vec<Option<String>>,
    /// Named placeholder (without its prefix) → slot.
    names: HashMap<String, usize>,
}

impl Numbering<'_> {
    fn fresh(&mut self, name: Option<&str>) -> usize {
        self.param_names.push(name.map(str::to_string));
        self.param_names.len()
    }

    /// `?NNN`: slot NNN, padding the slots before it when it skips ahead.
    fn explicit(&mut self, n: usize) {
        while self.param_names.len() < n {
            self.param_names.push(None);
        }
    }

    /// `?NNN:name`, the dedup pass's token for a named placeholder whose
    /// SQLite slot it already knows (it moves values about, so this pass's
    /// walk order could not tell).
    fn explicit_named(&mut self, n: usize, name: &str) {
        self.explicit(n);
        self.param_names[n - 1] = Some(name.to_string());
        self.names.insert(name.to_string(), n);
    }

    fn named(&mut self, name: &str) -> usize {
        if let Some(&n) = self.names.get(name) {
            return n;
        }
        let n = self.fresh(Some(name));
        self.names.insert(name.to_string(), n);
        n
    }

    /// The slot of a placeholder token, or None when it is left as written
    /// (a `$N` already numbered, or a token that is no placeholder).
    fn slot(&mut self, token: &str) -> Option<usize> {
        if token == "?" {
            return Some(self.fresh(None));
        }
        if let Some(rest) = token.strip_prefix('?') {
            let (digits, name) = match rest.split_once(':') {
                Some((digits, name)) if !name.is_empty() => (digits, Some(name)),
                _ => (rest, None),
            };
            let n: usize = digits.parse().ok().filter(|&n| n >= 1)?;
            match name {
                Some(name) => self.explicit_named(n, name),
                None => self.explicit(n),
            }
            return Some(n);
        }
        let name = token.strip_prefix([':', '@', '$'])?;
        if name.is_empty() {
            return None;
        }
        if token.starts_with('$') && name.bytes().all(|b| b.is_ascii_digit()) {
            // Already PostgreSQL's: a translation translated again.
            return None;
        }
        Some(self.named(name))
    }
}

impl VisitorMut for Numbering<'_> {
    type Break = ();

    fn pre_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<()> {
        if let Expr::Value(ValueWithSpan {
            value: Value::Placeholder(token),
            ..
        }) = expr
        {
            if let Some(n) = self.slot(token) {
                *token = format!("${n}");
            }
        }
        ControlFlow::Continue(())
    }

    /// `LIMIT offset, limit` was numbered offset first (the fields' order, as
    /// SQLite binds it); print it as PostgreSQL's `LIMIT limit OFFSET offset`.
    fn post_visit_query(&mut self, query: &mut Query) -> ControlFlow<()> {
        if matches!(
            query.limit_clause,
            Some(LimitClause::OffsetCommaLimit { .. })
        ) {
            let Some(LimitClause::OffsetCommaLimit { offset, limit }) = query.limit_clause.take()
            else {
                unreachable!()
            };
            query.limit_clause = Some(LimitClause::LimitOffset {
                limit: Some(limit),
                offset: Some(Offset {
                    value: offset,
                    rows: OffsetRows::None,
                }),
                limit_by: vec![],
            });
        }
        ControlFlow::Continue(())
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use crate::translate;
    use sqlparser::ast::Statement;
    use sqlparser::dialect::SQLiteDialect;
    use sqlparser::parser::Parser;

    #[test]
    fn rewrite_placeholders__placeholder_question_mark() {
        let r = translate("SELECT * FROM t WHERE a = ? AND b = ?").unwrap();
        assert!(r.sql.contains("$1") && r.sql.contains("$2"));
        assert_eq!(r.param_names.len(), 2, "sql={}", r.sql);
        assert_eq!(r.param_names[0], None);
        assert_eq!(r.param_names[1], None);
    }

    #[test]
    fn rewrite_placeholders__placeholder_named() {
        let r = translate("SELECT * FROM t WHERE id = :id").unwrap();
        assert!(r.sql.contains("$1"));
        assert_eq!(r.param_names[0], Some("id".to_string()));
    }

    #[test]
    fn rewrite_placeholders__placeholder_named_multiple() {
        let r = translate("SELECT * FROM t WHERE a = :foo AND b = :bar AND c = :baz").unwrap();
        assert!(r.sql.contains("$1") && r.sql.contains("$2") && r.sql.contains("$3"));
        assert_eq!(r.param_names.len(), 3);
    }

    #[test]
    fn rewrite_placeholders__placeholder_named_reuse() {
        let r = translate("SELECT * FROM t WHERE a = :id OR b = :id").unwrap();
        // same :id reuses same $N
        assert_eq!(r.param_names.len(), 1);
        assert_eq!(r.sql.matches("$1").count(), 2);
    }

    #[test]
    fn rewrite_placeholders__placeholder_in_string_literal_ignored() {
        let r = translate("SELECT * FROM t WHERE a = ':not_a_param'").unwrap();
        assert_eq!(r.param_names.len(), 0);
        assert!(r.sql.contains(":not_a_param"));
    }

    #[test]
    fn rewrite_placeholders__placeholder_mixed_question_and_named() {
        let r = translate("SELECT * FROM t WHERE a = ? AND b = :foo AND c = ?").unwrap();
        assert_eq!(r.param_names.len(), 3);
        assert!(r.sql.contains("$1") && r.sql.contains("$2") && r.sql.contains("$3"));
    }

    #[test]
    fn rewrite_placeholders__placeholder_in_join_on_clause() {
        let r = translate("select metadata_items.id from metadata_item_settings join metadata_items on metadata_items.guid=metadata_item_settings.guid and metadata_item_settings.account_id=? where metadata_items.library_section_id=? order by metadata_item_settings.updated_at desc limit ?").unwrap();
        assert_eq!(r.param_names.len(), 3);
        assert!(r.sql.contains("account_id = $1"), "sql={}", r.sql);
        assert!(r.sql.contains("library_section_id = $2"), "sql={}", r.sql);
        assert!(r.sql.contains("LIMIT $3"), "sql={}", r.sql);
        assert!(!r.sql.contains("?"), "sql={}", r.sql);
    }

    #[test]
    fn rewrite_placeholders__placeholder_in_nested_join_on_clause() {
        let r = translate(
            "SELECT * FROM (a JOIN b ON a.id = b.a_id AND b.account_id = ?) LEFT JOIN c ON c.id = b.c_id AND c.library_id = ?",
        )
        .unwrap();
        assert_eq!(r.param_names.len(), 2, "sql={}", r.sql);
        assert!(r.sql.contains("$1"), "sql={}", r.sql);
        assert!(r.sql.contains("$2"), "sql={}", r.sql);
        assert!(!r.sql.contains("?"), "sql={}", r.sql);
    }

    // The parser's own `LIMIT offset, limit` node (the preprocess pass rewrites
    // the text before the parser sees it, so only a direct call reaches it):
    // the offset is bound first, as SQLite binds `LIMIT ?, ?`.
    #[test]
    fn rewrite_placeholders__offset_comma_limit_numbers_the_offset_first() {
        let mut stmt: Statement =
            Parser::parse_sql(&SQLiteDialect {}, "SELECT * FROM t WHERE a = ? LIMIT ?, ?")
                .unwrap()
                .remove(0);
        let mut names = Vec::new();
        super::transform(&mut stmt, &mut names);
        assert_eq!(
            stmt.to_string(),
            "SELECT * FROM t WHERE a = $1 LIMIT $3 OFFSET $2"
        );
        assert_eq!(names, vec![None, None, None]);
    }
}
