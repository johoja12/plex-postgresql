#!/bin/sh
# Regenerates rust/plex-pg-core/src/schema_columns.rs from
# schema/sqlite_column_types.sql. Run from the repository root.
set -eu
out=rust/plex-pg-core/src/schema_columns.rs
{
cat <<'HDR'
// Generated from schema/sqlite_column_types.sql by scripts/gen-schema-columns.sh.
// Do not edit by hand; rerun the script when the schema changes.
//
// Plex's SQLite column types, as the shim's `plex.sqlite_column_types` table
// records them, reduced to the affinity the translator needs: an integer
// column takes SQLite's INTEGER affinity (INTEGER, INTEGER(8), DT_INTEGER(8),
// INT, BOOLEAN), a text column its TEXT affinity (VARCHAR(255), TEXT, STRING),
// and anything else (FLOAT, REAL, BLOB, TIMESTAMP) is left alone.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColumnKind {
    Integer,
    Text,
    Other,
}

/// `(table, column, kind)` for every column the schema file records.
pub(crate) const COLUMNS: &[(&str, &str, ColumnKind)] = &[
HDR
awk -F"'" '/^INSERT INTO plex.sqlite_column_types VALUES/ {
    t = $2; c = $4; ty = toupper($6); kind = "Other";
    if (ty ~ /^(INTEGER|INTEGER\(8\)|DT_INTEGER\(8\)|INT|BOOLEAN)$/) kind = "Integer";
    else if (ty ~ /^(VARCHAR\(255\)|TEXT|STRING)$/) kind = "Text";
    printf "    (\"%s\", \"%s\", ColumnKind::%s),\n", t, c, kind
}' schema/sqlite_column_types.sql
echo "];"
} > "$out"
rustfmt --edition 2021 "$out"
