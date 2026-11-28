-- 2026-11-27: the last three of Plex's UNIQUE indexes
--
-- Brings a database that plex_schema.sql created before 2026-11-27 to the
-- state a fresh plex_schema.sql now creates. Plex's own schema
-- (schema/sqlite_schema.sql) declares eleven UNIQUE indexes; three had no
-- counterpart here:
--
--   blobs (linked_type, linked_guid, blob_type)
--   metadata_agent_providers (identifier)
--   metadata_agent_provider_group_items
--       (metadata_agent_provider_group_id, metadata_agent_provider_id)
--
-- From upstream be86ce0 (cgnl/plex-postgresql), under Plex's names where
-- they fit in 63 characters.
--
-- Why. PMS writes with an upsert that names its index:
--     INSERT INTO blobs (...) VALUES (...)
--     ON CONFLICT(`linked_type`,`linked_guid`,`blob_type`) DO UPDATE SET ...
-- PostgreSQL refuses such a statement outright when no unique index
-- matches ("there is no unique or exclusion constraint matching the ON
-- CONFLICT specification"), so a blob linked by guid could not be stored at
-- all. What Plex keeps in blobs: subtitles it downloads on demand, intro
-- and credits detection data, sonic analysis. Not images.
--
-- Apply, with Plex running or stopped (each CREATE INDEX takes a SHARE lock
-- on its table for the time it takes to read it; all three are small):
--
--     psql -U postgres -d plex -v ON_ERROR_STOP=1 \
--          -f schema/migrations/2026-11-27-stock-unique-indexes.sql
--
-- Re-runnable. Needs ownership of the three tables (run it as postgres or
-- as the owner of schema plex); an index belongs to its table's owner. A
-- table that already holds duplicates (rows whose key columns are all
-- non-null and equal) cannot take its index: they are reported in a NOTICE
-- and that index is skipped, never failed. Resolve them and run the file
-- again.
--
-- Order against the shim: none. The indexes refuse nothing Plex's own
-- schema would accept, and the shim that aims a REPLACE INTO blobs at the
-- guid index (upsert.rs, blobs_conflict_columns) needs it there first or
-- fails as PMS's own statement did before.

\set ON_ERROR_STOP on

BEGIN;

SET LOCAL lock_timeout = '10s';

-- As 2026-10-16's, but a table already unique on the columns under any
-- name (upstream's idx_* ones, say) is left as it is.
CREATE FUNCTION pg_temp.unique_index_if_clean(idx text, tbl text, cols text[])
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    collist  text := (SELECT string_agg(quote_ident(c), ', ') FROM unnest(cols) AS c);
    not_null_cond  text := (SELECT string_agg(quote_ident(c) || ' IS NOT NULL', ' AND ') FROM unnest(cols) AS c);
    dups     text;
BEGIN
    IF EXISTS (SELECT 1 FROM pg_index i
                 JOIN pg_class t ON t.oid = i.indrelid
                 JOIN pg_namespace n ON n.oid = t.relnamespace
                WHERE n.nspname = 'plex' AND t.relname = tbl AND i.indisunique
                  AND i.indpred IS NULL
                  AND pg_get_indexdef(i.indexrelid) LIKE '%(' || collist || ')') THEN
        RAISE NOTICE 'plex.% is already unique on (%), skipping', tbl, collist;
        RETURN;
    END IF;
    EXECUTE format('SELECT string_agg(k, ''; '') FROM (SELECT concat_ws('', '', %s)::text AS k FROM plex.%I WHERE %s GROUP BY %s HAVING count(*) > 1 LIMIT 5) AS d',
                   collist, tbl, not_null_cond, collist)
       INTO dups;
    IF dups IS NOT NULL THEN
        RAISE NOTICE 'unique index % on plex.%(%) not created: duplicate keys exist, for example %', idx, tbl, collist, dups;
        RETURN;
    END IF;
    EXECUTE format('CREATE UNIQUE INDEX %I ON plex.%I (%s)', idx, tbl, collist);
END
$$;

SELECT pg_temp.unique_index_if_clean('index_blobs_on_linked_type_linked_guid_and_blob_type', 'blobs', ARRAY['linked_type', 'linked_guid', 'blob_type']);
SELECT pg_temp.unique_index_if_clean('index_metadata_agent_providers_on_identifier', 'metadata_agent_providers', ARRAY['identifier']);
-- Stock SQLite's name for this one is 109 characters.
SELECT pg_temp.unique_index_if_clean('index_metadata_agent_provider_group_items_on_group_and_provider', 'metadata_agent_provider_group_items', ARRAY['metadata_agent_provider_group_id', 'metadata_agent_provider_id']);

DROP FUNCTION pg_temp.unique_index_if_clean(text, text, text[]);

COMMIT;
