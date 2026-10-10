//! Non-ASCII text in SQL must come out of the translator exactly as it went in.
//!
//! Plex writes paths and titles such as "Caché (2005)" and "La Jetée (1962)"
//! into its SQL, and a scan of a library holding them crashed Plex: a keyword
//! search sliced the statement at a byte that fell inside a multi-byte
//! character, and the panic, inside an extern "C" function, aborted the
//! process. The character it fell inside was 'Ã', which none of those names
//! contains -- an earlier pass had already copied 'é' byte by byte into "Ã©".

use plex_pg_core::translate;

/// Every statement here passes through a preprocess pass that walks the SQL
/// byte by byte: the REGEXP and RAISE rewrites, LIMIT with a comma, UPDATE and
/// DELETE with LIMIT, CREATE TABLE options and GLOB.
const STATEMENTS: &[(&str, &str)] = &[
    (
        "select with limit",
        "SELECT id FROM directories WHERE path = 'Caché (2005) {tmdb-445}' LIMIT 1",
    ),
    (
        "limit with a comma",
        "SELECT id FROM directories WHERE path = 'La Jetée (1962)' LIMIT 10, 5",
    ),
    (
        "update with limit",
        "UPDATE metadata_items SET title = 'Amélie' WHERE title = 'Dìdi 弟弟' LIMIT 1",
    ),
    (
        "delete with limit",
        "DELETE FROM directories WHERE path = 'WALL·E (2008)' LIMIT 1",
    ),
    (
        "regexp",
        "SELECT id FROM metadata_items WHERE title REGEXP 'Shōgun' AND title_sort = 'Caché'",
    ),
    // RAISE(...) becomes NULL, message and all, so the accented text sits
    // around it rather than in it.
    (
        "raise",
        "SELECT CASE WHEN title = 'Amélie' THEN RAISE(ABORT, 'no') ELSE 'déjà vu' END FROM metadata_items",
    ),
    (
        "create table options",
        "CREATE TABLE t (a TEXT DEFAULT 'é' NOT NULL ON CONFLICT IGNORE)",
    ),
    // Without a wildcard, since GLOB's * and ? become ILIKE's % and _.
    ("glob", "SELECT id FROM media_parts WHERE file GLOB '/library/Caché (2005)'"),
    (
        "or conflict prefix",
        "INSERT OR IGNORE INTO tags (tag) VALUES ('Anne Rice’s Mayfair Witches')",
    ),
];

#[test]
fn non_ascii_literals_survive_translation() {
    for (name, sql) in STATEMENTS {
        let out = match translate(sql) {
            Ok(t) => t.sql,
            Err(e) => panic!("{name}: did not translate: {e}\n  in:  {sql}"),
        };
        for literal in non_ascii_literals(sql) {
            assert!(
                out.contains(&literal),
                "{name}: {literal:?} did not survive translation\n  in:  {sql}\n  out: {out}"
            );
        }
        assert!(
            !out.contains('Ã') && !out.contains('Â') && !out.contains('Å'),
            "{name}: the translation re-encoded text byte by byte\n  in:  {sql}\n  out: {out}"
        );
    }
}

/// A keyword search must not panic whatever lies within a keyword's length of
/// the byte it is looking at: here the multi-byte character sits at every
/// offset from the opening quote, so some window always ends inside it.
#[test]
fn keyword_search_never_splits_a_character() {
    for pad in 0..8 {
        let literal = format!("{}é弟", "a".repeat(pad));
        for sql in [
            format!("SELECT id FROM t WHERE a = '{literal}' LIMIT 1"),
            format!("SELECT id FROM t WHERE a = '{literal}' LIMIT 2, 3"),
            format!("UPDATE t SET a = '{literal}' WHERE b = 1 LIMIT 1"),
            format!("DELETE FROM t WHERE a = '{literal}' LIMIT 1"),
        ] {
            let t =
                translate(&sql).unwrap_or_else(|e| panic!("did not translate: {e}\n  in:  {sql}"));
            assert!(
                t.sql.contains(&literal),
                "lost {literal:?}\n  in:  {sql}\n  out: {}",
                t.sql
            );
        }
    }
}

/// The contents of every single-quoted literal holding a non-ASCII character.
fn non_ascii_literals(sql: &str) -> Vec<String> {
    sql.split('\'')
        .skip(1)
        .step_by(2)
        .filter(|s| !s.is_ascii())
        .map(str::to_string)
        .collect()
}
