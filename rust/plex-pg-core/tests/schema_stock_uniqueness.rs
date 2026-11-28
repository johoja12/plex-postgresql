//! Every UNIQUE index of Plex's own schema has its counterpart in the
//! PostgreSQL one.
//!
//! PMS writes with upserts that name their index
//! (`ON CONFLICT(`linked_type`,`linked_guid`,`blob_type`) DO UPDATE SET ...`),
//! and PostgreSQL refuses such a statement outright when no unique index is
//! on exactly those columns. Three of Plex's eleven were missing until
//! 2026-11-27: the second one of `blobs`, so a blob linked by guid could not
//! be stored, and the two of the metadata agent provider tables.
//!
//! Runs without a server: it reads schema/sqlite_schema.sql (Plex's schema,
//! as its shadow databases get it) and schema/plex_schema.sql.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use regex::Regex;

type Key = (String, Vec<String>);

fn read(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../schema")
        .join(name);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"))
}

fn columns(list: &str) -> Vec<String> {
    list.split(',')
        .map(|c| {
            c.trim()
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .trim_matches(|q| q == '"' || q == '`')
                .to_lowercase()
        })
        .collect()
}

/// Plex's UNIQUE indexes: (table, columns), with the index's name.
fn stock_unique(sqlite: &str) -> Vec<(Key, String)> {
    let index =
        Regex::new(r#"(?i)CREATE UNIQUE INDEX "?(\w+)"? ON "?(\w+)"? \(([^)]*)\)"#).unwrap();
    index
        .captures_iter(sqlite)
        .map(|c| ((c[2].to_lowercase(), columns(&c[3])), c[1].to_string()))
        .collect()
}

/// What PostgreSQL holds unique: indexes, and UNIQUE and PRIMARY KEY
/// constraints, none of them partial.
fn postgres_unique(schema: &str) -> BTreeSet<Key> {
    let index =
        Regex::new(r"(?i)CREATE UNIQUE INDEX \w+ ON plex\.(\w+)(?: USING btree)? \(([^)]*)\)\s*;")
            .unwrap();
    let constraint = Regex::new(
        r"(?i)ALTER TABLE ONLY plex\.(\w+)\s+ADD CONSTRAINT \w+ (?:UNIQUE|PRIMARY KEY) \(([^)]*)\)",
    )
    .unwrap();
    index
        .captures_iter(schema)
        .chain(constraint.captures_iter(schema))
        .map(|c| (c[1].to_lowercase(), columns(&c[2])))
        .collect()
}

#[test]
fn every_unique_index_of_plex_has_its_counterpart() {
    let stock = stock_unique(&read("sqlite_schema.sql"));
    let have = postgres_unique(&read("plex_schema.sql"));
    // Plex 1.43.5 declares eleven; fewer means the pattern stopped matching.
    assert!(
        stock.len() >= 11,
        "found only {} UNIQUE indexes in sqlite_schema.sql",
        stock.len()
    );

    let missing: Vec<String> = stock
        .iter()
        .filter(|(key, _)| !have.contains(key))
        .map(|((table, cols), name)| format!("{table} ({}) -- Plex's {name}", cols.join(", ")))
        .collect();
    assert!(
        missing.is_empty(),
        "plex_schema.sql has no unique index on:\n  {}\nadd each there and in a new schema/migrations/ file",
        missing.join("\n  ")
    );
}

/// The two a blob is written under, by name: the test above would pass on a
/// sqlite_schema.sql that lost them.
#[test]
fn blobs_is_unique_by_number_and_by_guid() {
    let have = postgres_unique(&read("plex_schema.sql"));
    for link in ["linked_id", "linked_guid"] {
        let key = (
            "blobs".to_string(),
            vec![
                "linked_type".to_string(),
                link.to_string(),
                "blob_type".to_string(),
            ],
        );
        assert!(
            have.contains(&key),
            "plex.blobs is not unique on (linked_type, {link}, blob_type)"
        );
    }
}

/// A schema change lands in plex_schema.sql and in a migration
/// (docs/migrations.md): every index the fresh schema names `index_*` and
/// UNIQUE is created by some file under schema/migrations/.
#[test]
fn every_stock_named_unique_index_has_a_migration() {
    let schema = read("plex_schema.sql");
    let named = Regex::new(r"(?i)CREATE UNIQUE INDEX (index_\w+) ON plex\.").unwrap();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schema/migrations");
    let migrations: String = fs::read_dir(&dir)
        .expect("read schema/migrations")
        .map(|e| fs::read_to_string(e.expect("entry").path()).expect("read migration"))
        .collect();
    let missing: Vec<String> = named
        .captures_iter(&schema)
        .map(|c| c[1].to_string())
        .filter(|name| !migrations.contains(name.as_str()))
        .collect();
    assert!(missing.is_empty(), "no migration creates: {missing:?}");
}
