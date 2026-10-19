# Schema migrations

`schema/plex_schema.sql` is what a fresh database gets. A database that
already exists is brought to the same state by the files under
`schema/migrations/`, one per change, named by date. Both are kept in step:
a change to the schema lands in `plex_schema.sql` and in a new migration,
and the migration's end state is the fresh file's (`schema/migrations/`'s
functions, views, indexes and triggers are the ones `plex_schema.sql`
declares, sliced from the same text).

Nothing applies a migration on its own: neither the entrypoints nor
cluster-plex's bootstrap look in `schema/migrations/`. Apply each one by hand,
once, in date order.

## Applying one

Every migration is idempotent -- `CREATE OR REPLACE`, `IF NOT EXISTS`, `DROP
... IF EXISTS`, and `DO` blocks that check the catalogs first -- so a file
can be run again after a failure, or against a database that already has
part of it. Each runs in one transaction under `ON_ERROR_STOP`, so a failure
leaves the database as it was.

Stop Plex first: the `ALTER TABLE`s take `ACCESS EXCLUSIVE` locks that queue
behind, and then block, Plex's statements (the file sets `lock_timeout` so
it gives up rather than wait for ever; re-run it once Plex is stopped).

Against a local server:

```sh
psql -U postgres -d plex -v ON_ERROR_STOP=1 \
     -f schema/migrations/2026-10-16-fts-collations-indexes.sql
```

Against the CloudNativePG cluster, from a checkout:

```sh
kubectl scale deployment/<plex> --replicas=0
kubectl exec -i <cluster>-1 -c postgres -- \
    psql -U postgres -d plex -v ON_ERROR_STOP=1 \
    < schema/migrations/2026-10-16-fts-collations-indexes.sql
kubectl scale deployment/<plex> --replicas=1
```

Run it as `postgres` (the superuser CloudNativePG keeps), or as the owner of
schema `plex` and its tables. Each file's header says what it needs beyond
that; on PostgreSQL 13 and later nothing in the current one does:
`pg_trgm` and `unaccent` are trusted extensions, which the database owner
may install, `CREATE COLLATION` needs `CREATE` on the schema, and the rest
needs table ownership. The collations need a server built with ICU (every
Debian and CloudNativePG build is); without one the collation block
NOTICEs and is skipped, and the rest still applies.

`NOTICE` lines are expected: `extension "pg_trgm" already exists,
skipping`, `trigger ... does not exist, skipping` on a first run, and
`relation ... already exists, skipping` on a second. A `NOTICE` that names a
unique index not created because of duplicate keys is the one to act on:
the file lists the duplicates, and the index can be created once they are
resolved.

## Migrations

### 2026-10-20-title-pattern-indexes.sql

Apply after the 2026-10-16 migration, with Plex stopped. Run as the owner of
`plex.metadata_items` (or a superuser). The migration adds the two missing
trigram GIN indexes for `original_title` and `title_sort COLLATE plex.icu_root`,
and retains or creates the existing `title` trigram index. It resolves the
`pg_trgm` extension's schema instead of assuming it is installed in `plex`.

Plex's ordinary `/search` uses `ILIKE 'term%'` and `ILIKE '% term%'` across
all three columns. The MATCH/tsvector indexes serve a different predicate;
without all three pattern indexes PostgreSQL may scan every item of the
requested type and evaluate the ICU/case-insensitive comparisons row by row.
The explicit collation on `title_sort` matches the translator exactly.
Matching, Unicode handling, sort order, and catalogue data are unchanged.
Very short or non-selective patterns may still require a scan.

This migration is transactional and re-runnable. Index creation blocks writes
and adds disk space and index maintenance on title edits. There is no automatic
migration application; fresh databases receive the same indexes from
`schema/plex_schema.sql`. An installation needing an online build can create
these same indexes with `CREATE INDEX CONCURRENTLY` outside a transaction,
then apply the migration to confirm they exist.

Regression check after applying (in an isolated database):

```sh
psql -v ON_ERROR_STOP=1 -f tests/sql/title_pattern_indexes.sql
```


### 2026-10-16-fts-collations-indexes.sql

Audit items #1, #24, #25, #26, #27 of `docs/audits/2026-10-16-shim-audit.md`:

- Full-text search: the `fts4_metadata_titles[_icu]` and
  `fts4_tag_titles[_icu]` views carry every column Plex names, with
  `plex.fold(text)` (unaccent + lower, over the `unaccent` extension) and
  `plex.fts_query(text)` (FTS4 query syntax to `tsquery`) and GIN expression
  indexes `to_tsvector('simple', plex.fold(col))` on `metadata_items.title`,
  `title_sort`, `original_title` and `tags.tag`. The dead `title_fts` and
  `search_vector` columns, their indexes and triggers go.
- Collations: `plex.nocase` (ICU, `und-u-ks-level2`, non-deterministic) on
  `tags.tag`, `metadata_items.title_sort` and `library_sections.name_sort`,
  which stock Plex declares `COLLATE NOCASE`; `plex.icu_root` (ICU, `und`)
  for `ORDER BY ... COLLATE icu_root`.
- Indexes stock Plex has, under their stock names (Live TV's
  `media_items(channel_id, begins_at)` and friends, `metadata_item_views`,
  `tags(tag_type, tag)`, ...), the four UNIQUE ones Plex's `ON CONFLICT`
  targets, and a trigram index on `media_items.extra_data` for cluster-plex's
  `cp:mediaFile` lookups.
- spellfix: `plex.spellfix_vocab` (the folded words of every title and tag,
  kept by triggers and seeded from the library) and
  `plex.spellfix_suggest(term, n)` over `pg_trgm` similarity.
- PG-only extras removed: the `subtype` generated column, the
  `chk_not_orphan` CHECK, the cross-section and self-reference parent
  triggers and the two orphan-season regex triggers.
  `metadata_items_set_available_at` stays (it only fills a NULL
  `available_at` from `added_at`).

Verification, after applying:

```sql
SET search_path = plex, public;
-- #1: a title search answers rows and the query is served by the index
EXPLAIN SELECT rowid FROM fts4_metadata_titles
 WHERE to_tsvector('simple', plex.fold(title)) @@ plex.fts_query('the*');
-- #24: the three columns carry plex.nocase
SELECT attrelid::regclass, attname, attcollation::regcollation FROM pg_attribute
 WHERE attcollation = 'plex.nocase'::regcollation;
-- #25: the stock indexes exist
SELECT indexname FROM pg_indexes WHERE schemaname = 'plex' AND indexname LIKE 'index\_%';
-- #26: a misspelling finds its word
SELECT * FROM plex.spellfix_suggest('matrx');
-- #27: no guard trigger or extra column is left
SELECT tgname FROM pg_trigger WHERE tgrelid = 'plex.metadata_items'::regclass AND NOT tgisinternal;
```

Applied as a superuser, a migration hands what it creates to the `plex` role at its end (the 2026-10-16 file does); check with `SELECT relname FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='plex' AND pg_get_userbyid(c.relowner)<>'plex';` after applying.
