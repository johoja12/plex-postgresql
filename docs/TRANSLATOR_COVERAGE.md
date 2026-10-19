# Translator Coverage Ledger (Draft)

Entry point: `docs/translator/README.md`

## Purpose
A living map of SQLite surface area vs. translator support, with direct pointers to the tests that cover each behavior.

## Status Legend
- Supported: intended to work for Plex workloads; tests exist.
- Partial: some patterns supported; gaps exist or behavior is heuristic.
- Skipped/No-op: explicitly ignored by the translator/runtime.
- Gap: known unsupported behavior; tracked in `docs/translator/KNOWN_GAPS.md`.

## Coverage Matrix
| Area | Status | Tag | Tests | Notes |
| --- | --- | --- | --- | --- |
| SELECT core (joins, aliases, subqueries) | Supported | `subset/core`, `compat/aliases` | `rust/plex-pg-core/src/query.rs`, `rust/plex-pg-core/src/keywords_tests.rs` | Core translator rules + alias fixes. |
| INSERT / UPDATE / DELETE | Supported | `subset/core` | `rust/plex-pg-core/src/query.rs`, `rust/plex-pg-core/src/dedup.rs` | UPDATE dedup rule is explicit. |
| INSERT OR REPLACE / IGNORE | Supported | `subset/core` | `rust/plex-pg-core/src/upsert.rs`, `rust/plex-pg-core/src/emit.rs` | Mapped to `ON CONFLICT` patterns. |
| Placeholders `?` / `:name` | Supported | `rewrite/placeholders` | `rust/plex-pg-core/src/placeholders.rs` | Deterministic bind ordering. |
| Identifier quoting (backticks) | Supported | `compat/backticks` | `rust/plex-pg-core/src/quotes.rs`, `rust/plex-pg-core/src/emit.rs` | Backticks → double quotes. |
| DISTINCT + ORDER BY | Supported | `rewrite/distinct-orderby` | `rust/plex-pg-core/src/query.rs` (distinct_fix tests) | Adds missing select items. |
| GROUP BY strictness | Supported | `rewrite/groupby` | `rust/plex-pg-core/src/groupby.rs` | Adds non-aggregate columns. |
| Function rewrites (IFNULL, IIF, STRFTIME, etc.) | Supported | `subset/core` | `rust/plex-pg-core/src/functions.rs`, `rust/plex-pg-core/src/emit.rs` | Includes common Plex patterns. |
| JSON functions / operators | Partial | `subset/json` | `rust/plex-pg-core/src/functions.rs`, `rust/plex-pg-core/src/query.rs` | JSONPath fallback used for some patterns. |
| FTS MATCH rewrite | Partial | `subset/fts` | `rust/plex-pg-core/src/db_interpose_helpers_tests.rs` | Limited to known Plex queries. The schema side -- the `fts4_*` views, `plex.fold`, `plex.fts_query`, the GIN expression indexes, `plex.spellfix_suggest` -- is in `docs/translator/KNOWN_GAPS.md` ("Schema-side support"). |
| DDL-lite (CREATE TABLE/INDEX IF NOT EXISTS) | Partial | `subset/ddl-lite` | `rust/plex-pg-core/src/quotes.rs`, `rust/plex-pg-core/src/db_interpose_helpers_tests.rs`, `rust/plex-pg-core/src/emit.rs` | Only patterns used by Plex. |
| PRAGMA / extension hooks | Skipped/No-op | `subset/pragma` | `rust/plex-pg-core/src/keywords_tests.rs`, `rust/plex-pg-core/src/pg_config.rs` | Explicit skip list and prefix checks. |
| Transaction control (BEGIN/COMMIT/ROLLBACK/SAVEPOINT) | Supported for Plex workloads | `subset/txn` | `rust/plex-pg-core/src/pg_client/txn.rs` | Forwarded on a connection pinned to the handle; reads inside transactions use eager fetch. `PLEX_PG_TRANSACTIONS=0` restores no-op behavior. |
| Type affinity / decltype mapping | Supported | `subset/core` | `rust/plex-pg-core/src/types.rs`, `rust/plex-pg-core/src/pg_statement.rs` | SQLite decltype → PG types. |
| Output validation (PG parser check) | Supported | `validation/output` | `rust/plex-pg-core/src/lib.rs` tests | Optional validation gate. |

## Notes
- Known gaps are tracked in `docs/translator/KNOWN_GAPS.md` and the overall intended surface is summarized in `docs/translator/SUPPORTED_SQLITE_SUBSET.md`.
- Test tagging conventions live in `TRANSLATOR_TEST_TAGS.md`; new tests should include tags that map to the matrix above.
- This document is not a guarantee of full SQLite compatibility; it is a practical ledger for the Plex translator.
