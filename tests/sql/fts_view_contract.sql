-- Run after schema initialization and again after pg_compat_functions.sql on an
-- existing database: psql -v ON_ERROR_STOP=1 -f tests/sql/fts_view_contract.sql
-- Read-only contract: missing columns fail at parse time, changed values raise.
DO $test$
DECLARE view_name text;
DECLARE mismatch boolean;
BEGIN
    FOREACH view_name IN ARRAY ARRAY['fts4_metadata_titles', 'fts4_metadata_titles_icu'] LOOP
        EXECUTE format('SELECT EXISTS (SELECT 1 FROM plex.%I v FULL JOIN plex.metadata_items m ON m.id=v.rowid WHERE v.rowid IS DISTINCT FROM m.id OR v.title IS DISTINCT FROM m.title OR v.title_sort IS DISTINCT FROM m.title_sort OR v.original_title IS DISTINCT FROM m.original_title)', view_name) INTO mismatch;
        IF mismatch THEN RAISE EXCEPTION 'FTS view % differs from source', view_name; END IF;
    END LOOP;
    FOREACH view_name IN ARRAY ARRAY['fts4_tag_titles', 'fts4_tag_titles_icu'] LOOP
        EXECUTE format('SELECT EXISTS (SELECT 1 FROM plex.%I v FULL JOIN plex.tags t ON t.id=v.rowid WHERE v.rowid IS DISTINCT FROM t.id OR v.title IS DISTINCT FROM t.tag OR v.tag IS DISTINCT FROM t.tag)', view_name) INTO mismatch;
        IF mismatch THEN RAISE EXCEPTION 'FTS view % differs from source', view_name; END IF;
    END LOOP;
END;
$test$;
