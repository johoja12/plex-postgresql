-- Run on an isolated database after schema + metadata + compatibility helpers,
-- and again after applying the 2026-10-16 migration twice and reloading helpers.
BEGIN;
INSERT INTO plex.metadata_items (id, title, title_sort, original_title)
 VALUES (-19191019, 'éclair 日本語', 'éclair 日本語', 'éclair 日本語');
INSERT INTO plex.tags (id, tag) VALUES (-19191019, 'éclair 日本語');
DO $test$
DECLARE v text; missing boolean;
BEGIN
    FOREACH v IN ARRAY ARRAY['fts4_metadata_titles', 'fts4_metadata_titles_icu',
                            'fts4_tag_titles', 'fts4_tag_titles_icu'] LOOP
        EXECUTE format('SELECT EXISTS (SELECT 1 FROM plex.%I WHERE docid IS DISTINCT FROM rowid)', v)
          INTO missing;
        IF missing THEN RAISE EXCEPTION 'FTS docid contract failed: %', v; END IF;
        EXECUTE format('SELECT NOT EXISTS (SELECT 1 FROM plex.%I WHERE rowid = -19191019 AND docid = rowid AND to_tsvector(''simple'', plex.fold(title)) @@ plex.fts_query(''é* OR 日本*''))', v)
          INTO missing;
        IF missing THEN RAISE EXCEPTION 'FTS view search failed: %', v; END IF;
    END LOOP;
    IF NOT (to_tsvector('simple', plex.fold('Star Wars')) @@ plex.fts_query('(star*)'))
       OR NOT (to_tsvector('simple', plex.fold('éclair')) @@ plex.fts_query('é* OR 日本*'))
       OR NOT (to_tsvector('simple', plex.fold('日本語')) @@ plex.fts_query('é* OR 日本*'))
       OR (to_tsvector('simple', plex.fold('other')) @@ plex.fts_query('é* OR 日本*'))
       OR NOT (to_tsvector('simple', plex.fold('a O日')) @@ plex.fts_query('a O日'))
    THEN RAISE EXCEPTION 'FTS prefix, OR or Unicode regression'; END IF;
    IF (SELECT count(*) FROM pg_indexes WHERE schemaname = 'plex'
        AND indexname IN ('idx_metadata_items_title_fts', 'idx_metadata_items_title_sort_fts',
                         'idx_metadata_items_original_title_fts', 'idx_tags_tag_fts')
        AND indexdef LIKE '%plex.fold(%') <> 4
    THEN RAISE EXCEPTION 'FTS expressions lack matching indexes'; END IF;
END;
$test$;
ROLLBACK;
