-- PostgreSQL compatibility functions used by the SQLite->PostgreSQL translator.
-- This file is safe to run multiple times.

CREATE OR REPLACE FUNCTION public.jsonb_mergepatch(target jsonb, patch jsonb)
RETURNS jsonb
LANGUAGE plpgsql
IMMUTABLE
STRICT
PARALLEL SAFE
AS $fn$
DECLARE
    result jsonb;
    k text;
    v jsonb;
BEGIN
    -- RFC 7396: non-object patch replaces target entirely.
    IF jsonb_typeof(patch) <> 'object' THEN
        RETURN patch;
    END IF;

    -- RFC 7396: object patch against non-object target starts from {}.
    IF jsonb_typeof(target) <> 'object' THEN
        result := '{}'::jsonb;
    ELSE
        result := target;
    END IF;

    FOR k, v IN SELECT e.key, e.value FROM jsonb_each(patch) AS e(key, value) LOOP
        -- RFC 7396: null in patch means remove key.
        IF v = 'null'::jsonb THEN
            result := result - k;
        ELSIF (result ? k)
              AND jsonb_typeof(result -> k) = 'object'
              AND jsonb_typeof(v) = 'object' THEN
            result := jsonb_set(result, ARRAY[k], public.jsonb_mergepatch(result -> k, v), true);
        ELSE
            result := jsonb_set(result, ARRAY[k], v, true);
        END IF;
    END LOOP;

    RETURN result;
END;
$fn$;

CREATE OR REPLACE FUNCTION public.json_valid(val text)
RETURNS boolean AS $$
BEGIN
  IF val IS NULL THEN
    RETURN NULL;
  END IF;
  PERFORM val::json;
  RETURN TRUE;
EXCEPTION WHEN OTHERS THEN
  RETURN FALSE;
END;
$$ LANGUAGE plpgsql IMMUTABLE;

CREATE OR REPLACE FUNCTION public.eq_bool_int(b boolean, i integer)
RETURNS boolean AS $$
BEGIN
  RETURN (b = (i <> 0));
END;
$$ LANGUAGE plpgsql IMMUTABLE;

CREATE OR REPLACE FUNCTION public.eq_int_bool(i integer, b boolean)
RETURNS boolean AS $$
BEGIN
  RETURN ((i <> 0) = b);
END;
$$ LANGUAGE plpgsql IMMUTABLE;

DROP OPERATOR IF EXISTS public.= (boolean, integer);
CREATE OPERATOR public.= (
  PROCEDURE = public.eq_bool_int,
  LEFTARG = boolean,
  RIGHTARG = integer,
  COMMUTATOR = =
);

DROP OPERATOR IF EXISTS public.= (integer, boolean);
CREATE OPERATOR public.= (
  PROCEDURE = public.eq_int_bool,
  LEFTARG = integer,
  RIGHTARG = boolean,
  COMMUTATOR = =
);



-- Older schema snapshots exposed only title/title_fts in the FTS views, while
-- the translator correctly references SQLite FTS title_sort/original_title/tag.
-- Append missing source columns without changing existing column positions or
-- dropping dependent objects. Re-running this upgrade is safe.
DO $fts_views$
BEGIN
    IF to_regclass('plex.metadata_items') IS NOT NULL THEN
        -- MATCH is translated against these source columns, not title_fts.
        -- Pre-create on a busy live database with CONCURRENTLY before upgrade.
        CREATE INDEX IF NOT EXISTS idx_metadata_items_title_simple_fts
            ON plex.metadata_items USING gin (to_tsvector('simple'::regconfig, title));
        CREATE INDEX IF NOT EXISTS idx_metadata_items_title_sort_simple_fts
            ON plex.metadata_items USING gin (to_tsvector('simple'::regconfig, title_sort));
        CREATE OR REPLACE VIEW plex.fts4_metadata_titles AS
            SELECT id AS rowid, title, title_fts, title_sort, original_title
            FROM plex.metadata_items;
        CREATE OR REPLACE VIEW plex.fts4_metadata_titles_icu AS
            SELECT id AS rowid, title, title_fts, title_sort, original_title
            FROM plex.metadata_items;
    END IF;
    IF to_regclass('plex.tags') IS NOT NULL THEN
        -- Preserve MATCH semantics while indexing its exact source expression.
        -- Production operators can pre-create this index CONCURRENTLY before
        -- upgrading to avoid a write lock on a busy existing database.
        CREATE INDEX IF NOT EXISTS idx_tags_tag_simple_fts
            ON plex.tags USING gin (to_tsvector('simple'::regconfig, tag));
        CREATE OR REPLACE VIEW plex.fts4_tag_titles AS
            SELECT id AS rowid, tag AS title, search_vector AS title_fts, tag
            FROM plex.tags;
        CREATE OR REPLACE VIEW plex.fts4_tag_titles_icu AS
            SELECT id AS rowid, tag AS title, search_vector AS title_fts, tag
            FROM plex.tags;
    END IF;
END;
$fts_views$;
