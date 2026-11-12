-- Disposable database only, after plex_schema.sql. Exercise the combined
-- startup migration against old statistics triggers and legacy FTS views.
\set ON_ERROR_STOP on
BEGIN;
CREATE SCHEMA sync_statistics_guide;
CREATE FUNCTION plex.reject_empty_statistics() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.count = 0 AND NEW.duration = 0 THEN RETURN NULL; END IF;
    RETURN NEW;
END;
$$;
CREATE FUNCTION sync_statistics_guide.reject_empty_statistics() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.count = 0 AND NEW.duration = 0 THEN RETURN NULL; END IF;
    RETURN NEW;
END;
$$;
CREATE TABLE sync_statistics_guide.statistics_media
    (LIKE plex.statistics_media INCLUDING ALL);
CREATE TRIGGER statistics_media_reject_empty
BEFORE INSERT ON plex.statistics_media
FOR EACH ROW EXECUTE FUNCTION plex.reject_empty_statistics();
CREATE TRIGGER statistics_media_reject_empty
BEFORE INSERT ON sync_statistics_guide.statistics_media
FOR EACH ROW EXECUTE FUNCTION sync_statistics_guide.reject_empty_statistics();
-- A guide may depend on the library function rather than its own copy.
CREATE TRIGGER statistics_media_reject_empty_shared
BEFORE INSERT ON sync_statistics_guide.statistics_media
FOR EACH ROW EXECUTE FUNCTION plex.reject_empty_statistics();

-- Reproduce the old stored-vector snapshot that the fork repairs on startup.
DROP VIEW plex.fts4_metadata_titles, plex.fts4_metadata_titles_icu,
          plex.fts4_tag_titles, plex.fts4_tag_titles_icu;
ALTER TABLE plex.metadata_items ADD COLUMN title_fts tsvector;
ALTER TABLE plex.tags ADD COLUMN search_vector tsvector;
CREATE VIEW plex.fts4_metadata_titles AS
    SELECT id AS rowid, title, title_fts FROM plex.metadata_items;
CREATE VIEW plex.fts4_metadata_titles_icu AS
    SELECT id AS rowid, title, title_fts FROM plex.metadata_items;
CREATE VIEW plex.fts4_tag_titles AS
    SELECT id AS rowid, tag AS title, search_vector AS title_fts FROM plex.tags;
CREATE VIEW plex.fts4_tag_titles_icu AS
    SELECT id AS rowid, tag AS title, search_vector AS title_fts FROM plex.tags;

\ir ../../schema/pg_compat_functions.sql
\ir ../../schema/pg_compat_functions.sql

DO $test$
DECLARE schema_name text; bucket_id integer; changed integer;
BEGIN
    IF EXISTS (SELECT 1 FROM pg_proc WHERE proname = 'reject_empty_statistics')
       OR EXISTS (SELECT 1 FROM pg_trigger
                  WHERE tgname LIKE 'statistics_media_reject_empty%') THEN
        RAISE EXCEPTION 'startup retained a statistics rejection function/trigger';
    END IF;
    FOREACH schema_name IN ARRAY ARRAY['plex', 'sync_statistics_guide'] LOOP
        EXECUTE format('INSERT INTO %I.statistics_media (count, duration)
                        VALUES (0, 0) RETURNING id', schema_name) INTO bucket_id;
        IF bucket_id IS NULL OR bucket_id <= 0 THEN
            RAISE EXCEPTION 'empty statistics bucket returned no id in %', schema_name;
        END IF;
        EXECUTE format('UPDATE %I.statistics_media SET count=1, duration=60
                        WHERE id=$1', schema_name) USING bucket_id;
        GET DIAGNOSTICS changed = ROW_COUNT;
        IF changed <> 1 THEN
            RAISE EXCEPTION 'statistics bucket update missed in %', schema_name;
        END IF;
    END LOOP;
    -- Referencing the repaired columns also catches a lost FTS conflict side.
    PERFORM title_sort, original_title FROM plex.fts4_metadata_titles;
    PERFORM title_sort, original_title FROM plex.fts4_metadata_titles_icu;
    PERFORM tag FROM plex.fts4_tag_titles;
    PERFORM tag FROM plex.fts4_tag_titles_icu;
    IF to_regclass('plex.idx_metadata_items_title_simple_fts') IS NULL
       OR to_regclass('plex.idx_metadata_items_title_sort_simple_fts') IS NULL
       OR to_regclass('plex.idx_tags_tag_simple_fts') IS NULL THEN
        RAISE EXCEPTION 'legacy FTS startup indexes were lost';
    END IF;
END;
$test$;
ROLLBACK;
