use plex_pg_core::translate;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

fn assert_pg(sqlite_sql: &str) {
    let out = translate(sqlite_sql)
        .unwrap_or_else(|e| panic!("translate failed for `{}`: {}", sqlite_sql, e));
    Parser::parse_sql(&PostgreSqlDialect {}, &out.sql).unwrap_or_else(|e| {
        panic!(
            "postgres parse failed\ninput: {}\noutput: {}\nerror: {}",
            sqlite_sql, out.sql, e
        )
    });
}

fn translate_sql(sqlite_sql: &str) -> String {
    translate(sqlite_sql)
        .unwrap_or_else(|e| panic!("translate failed for `{}`: {}", sqlite_sql, e))
        .sql
}

#[test]
fn category_insert_conflict_resolution_prefix_variants() {
    let sqls = [
        "INSERT OR ABORT INTO t(id, v) VALUES (1, 'x')",
        "INSERT OR FAIL INTO t(id, v) VALUES (1, 'x')",
        "INSERT OR ROLLBACK INTO t(id, v) VALUES (1, 'x')",
    ];
    for s in sqls {
        assert_pg(s);
        let out = translate_sql(s).to_ascii_lowercase();
        assert!(
            !out.contains("insert or "),
            "expected INSERT OR <algo> to be normalized, got: {}",
            out
        );
    }
}

#[test]
fn category_update_conflict_resolution_prefix_variants() {
    let sqls = [
        "UPDATE OR IGNORE t SET v = 'x' WHERE id = 1",
        "UPDATE OR ABORT t SET v = 'x' WHERE id = 1",
        "UPDATE OR FAIL t SET v = 'x' WHERE id = 1",
        "UPDATE OR ROLLBACK t SET v = 'x' WHERE id = 1",
        "UPDATE OR REPLACE t SET v = 'x' WHERE id = 1",
    ];
    for s in sqls {
        assert_pg(s);
        let out = translate_sql(s).to_ascii_lowercase();
        assert!(
            !out.contains("update or "),
            "expected UPDATE OR <algo> to be normalized, got: {}",
            out
        );
    }
}

#[test]
fn category_limit_offset_comma_syntax() {
    let sqls = [
        "SELECT id FROM t ORDER BY id LIMIT 5, 10",
        "SELECT id FROM t ORDER BY id LIMIT 0, 25",
    ];
    for s in sqls {
        assert_pg(s);
        let out = translate_sql(s).to_ascii_lowercase();
        assert!(
            out.contains(" offset "),
            "expected LIMIT x,y rewrite to OFFSET form, got: {}",
            out
        );
        assert!(
            !out.contains("limit 5, 10") && !out.contains("limit 0, 25"),
            "expected no comma LIMIT syntax in output, got: {}",
            out
        );
    }
}

#[test]
fn category_update_delete_order_by_limit_forms() {
    let sqls = [
        "UPDATE t SET v = 'x' WHERE v IS NOT NULL ORDER BY id DESC LIMIT 3",
        "UPDATE t SET v = 'x' LIMIT 1 RETURNING id",
        "DELETE FROM t WHERE v IS NOT NULL ORDER BY id LIMIT 2",
        "DELETE FROM t LIMIT 4 RETURNING id",
    ];
    for s in sqls {
        assert_pg(s);
        let out = translate_sql(s).to_ascii_lowercase();
        // A DELETE targets its rows through a CTE; an UPDATE through a
        // subquery after its SET, so the SET placeholders keep their order.
        assert!(
            out.contains("with _plex_target as (select ctid from")
                || out.contains("where ctid in (select ctid from"),
            "expected ctid targeting rewrite, got: {}",
            out
        );
    }
}
