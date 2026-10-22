use plex_pg_core::translate;
use proptest::prelude::*;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

fn tr(sql: &str) -> String {
    translate(sql)
        .unwrap_or_else(|e| panic!("translate failed for `{}`: {}", sql, e))
        .sql
}

fn assert_pg(sql: &str) {
    Parser::parse_sql(&PostgreSqlDialect {}, sql)
        .unwrap_or_else(|e| panic!("postgres parse failed for `{}`: {}", sql, e));
}

fn assert_idempotent(sql: &str) {
    let once = tr(sql);
    let twice = tr(&once);
    assert_pg(&once);
    assert_pg(&twice);
    assert_eq!(
        once, twice,
        "translation should be idempotent\ninput: {}\nonce: {}\ntwice: {}",
        sql, once, twice
    );
}

#[test]
fn idempotence_known_corpus() {
    let cases = [
        "SELECT `id`, `title` FROM `metadata_items` WHERE `id` = ?",
        "SELECT id AS myAlias FROM metadata_items WHERE id = :id",
        "SELECT * FROM metadata_items WHERE title LIKE '%test%' ORDER BY title",
        "SELECT DISTINCT id FROM metadata_items ORDER BY title",
        "SELECT a, count(*) FROM t GROUP BY a",
        "SELECT * FROM t WHERE extra_data ->> '$.pv:version' < $3",
        "UPDATE t SET a = ?, b = :name WHERE id = ?",
        "INSERT OR IGNORE INTO schema_migrations (version) VALUES ('20230101')",
    ];
    for sql in cases {
        assert_idempotent(sql);
    }
}

fn ident() -> impl Strategy<Value = String> {
    "[A-Za-z_][A-Za-z0-9_]{0,10}"
        .prop_map(|s| s.to_string())
        // These identifiers are emitted without quotes (including table
        // qualifiers). A keyword such as IS makes the generated SQL invalid,
        // so it cannot exercise translation idempotence.
        .prop_filter("identifier is not an SQL keyword", |s| {
            !sqlparser::keywords::ALL_KEYWORDS.contains(&s.to_ascii_uppercase().as_str())
        })
}

fn col_expr() -> impl Strategy<Value = String> {
    prop_oneof![
        ident().prop_map(|c| format!("`{}`", c)),
        ident().prop_map(|c| c),
        (ident(), ident()).prop_map(|(t, c)| format!("{}.{}", t, c)),
    ]
}

fn value_expr() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("?".to_string()),
        ident().prop_map(|n| format!(":{}", n)),
        any::<i16>().prop_map(|n| n.to_string()),
        "[A-Za-z0-9]{1,8}".prop_map(|s| format!("'{}'", s)),
    ]
}

proptest! {
    #[test]
    fn property_select_idempotence(
        table in ident(),
        c1 in col_expr(),
        c2 in col_expr(),
        wc in col_expr(),
        wv in value_expr(),
    ) {
        let sql = format!(
            "SELECT {} AS alias_a, {} FROM {} WHERE {} = {} ORDER BY {}",
            c1, c2, table, wc, wv, c1
        );
        assert_idempotent(&sql);
    }

    #[test]
    fn property_groupby_idempotence(
        table in ident(),
        gc in ident(),
    ) {
        let sql = format!(
            "SELECT {}, count(*) FROM {} GROUP BY {}",
            gc, table, gc
        );
        assert_idempotent(&sql);
    }
}
