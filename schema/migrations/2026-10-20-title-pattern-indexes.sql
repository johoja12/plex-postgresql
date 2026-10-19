-- 2026-10-20: index Plex's case-insensitive title pattern searches.
-- Apply after 2026-10-16-fts-collations-indexes.sql, with Plex stopped:
--   psql -v ON_ERROR_STOP=1 -f schema/migrations/2026-10-20-title-pattern-indexes.sql
-- Requires ownership of plex.metadata_items and CREATE on the database if
-- pg_trgm is not installed. The extension is trusted on PostgreSQL 13+.
-- Existing pg_trgm installations in public or plex are both supported.
-- No rows, matching rules, sort order, or FTS objects are changed.
-- CREATE INDEX blocks writes; this transactional migration is intended for
-- the maintenance window, not an online CONCURRENTLY index build.
SET search_path = plex, public;
SET lock_timeout = '60s';
BEGIN;
CREATE EXTENSION IF NOT EXISTS pg_trgm;

-- Plex's /search uses ILIKE on all three title fields, including leading
-- wildcards. Its MATCH/tsvector indexes cannot serve those predicates.
-- The title_sort expression must match the translator's deterministic
-- collation; indexing the NOCASE column directly cannot serve that query.
DO $title_patterns$
DECLARE
    ns text;
BEGIN
    SELECT n.nspname INTO ns
      FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace
     WHERE e.extname = 'pg_trgm';
    IF ns IS NULL THEN
        RAISE EXCEPTION 'extension pg_trgm is not installed';
    END IF;
    IF to_regcollation('plex.icu_root') IS NULL THEN
        RAISE EXCEPTION 'plex.icu_root is missing; apply the 2026-10-16 migration on an ICU-enabled server first';
    END IF;
    EXECUTE format('CREATE INDEX IF NOT EXISTS idx_metadata_items_title_trgm ON plex.metadata_items USING gin (title %I.gin_trgm_ops)', ns);
    EXECUTE format('CREATE INDEX IF NOT EXISTS idx_metadata_items_title_sort_trgm ON plex.metadata_items USING gin ((title_sort COLLATE plex.icu_root) %I.gin_trgm_ops)', ns);
    EXECUTE format('CREATE INDEX IF NOT EXISTS idx_metadata_items_original_title_trgm ON plex.metadata_items USING gin (original_title %I.gin_trgm_ops)', ns);
END
$title_patterns$;

COMMIT;
