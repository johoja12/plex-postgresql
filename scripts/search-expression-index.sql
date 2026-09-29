-- Run with psql -v ON_ERROR_STOP=1 outside a transaction before upgrading.
-- Matches translated MATCH queries without changing search semantics.
SET lock_timeout = '5s';
SET statement_timeout = '120s';
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_tags_tag_simple_fts
    ON plex.tags USING gin (to_tsvector('simple'::regconfig, tag));
ANALYZE plex.tags;
SELECT indexrelid::regclass AS index_name, indisvalid, indisready
FROM pg_index
WHERE indexrelid = 'plex.idx_tags_tag_simple_fts'::regclass;
