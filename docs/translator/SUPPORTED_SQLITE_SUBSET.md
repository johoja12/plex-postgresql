# Supported SQLite Subset

The translator intentionally targets the SQLite surface area Plex actually exercises.

## Supported
- Core `SELECT`, `INSERT`, `UPDATE`, and `DELETE`
- Placeholder rewriting for `?` and `:name`
- Identifier quoting with backticks
- Common SQLite function rewrites such as `IFNULL`, `IIF`, `STRFTIME`, `JULIANDAY`, `GROUP_CONCAT`, and `LAST_INSERT_ROWID`
- `INSERT OR REPLACE` and `INSERT OR IGNORE` mapped to PostgreSQL `ON CONFLICT`
- DISTINCT plus `ORDER BY` projection fixes
- Strict `GROUP BY` projection completion
- Decltype and type-affinity normalization used by the shim result path
- Output validation with PostgreSQL parsing when enabled

## Supported With Constraints
- JSON functions and operators: broad support, but some path forms still use guarded fallbacks
- Transaction control: routed and normalized for Plex usage, but SQLite and PostgreSQL semantics are not identical
- DDL-lite: limited `CREATE TABLE` and `CREATE INDEX` normalization used by Plex bootstrap and migrations
- FTS: rewritten only for known Plex query shapes, onto the schema's `fts4_*` views, `plex.fold` and `plex.fts_query` (`docs/translator/KNOWN_GAPS.md`, "Schema-side support")
- PRAGMA handling: supported where it can be mapped safely; otherwise it is explicitly skipped or downgraded

## Non-Goals
- Full SQLite engine compatibility
- Virtual table ecosystems and arbitrary extension compatibility
- Byte-for-byte SQLite transaction behavior on PostgreSQL

For the current gap list, see `docs/translator/KNOWN_GAPS.md`.
