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
  COMMUTATOR = OPERATOR(public.=)
);

DROP OPERATOR IF EXISTS public.= (integer, boolean);
CREATE OPERATOR public.= (
  PROCEDURE = public.eq_int_bool,
  LEFTARG = integer,
  RIGHTARG = boolean,
  COMMUTATOR = OPERATOR(public.=)
);

-- The plex schema's integer/text = (plex_schema.sql), in public as well: a
-- Live TV guide's connection runs on the guide's own schema and public, so
-- it could not see plex's, and PostgreSQL refused every comparison of an
-- integer with text there ("operator does not exist: integer = text", talos,
-- 2026-10-21). The library's search path finds plex's first, which is the
-- same function.
CREATE OR REPLACE FUNCTION public.integer_equals_text(integer, text)
RETURNS boolean
LANGUAGE sql
IMMUTABLE
AS $$
    SELECT $1 = $2::integer;
$$;

CREATE OR REPLACE FUNCTION public.text_equals_integer(text, integer)
RETURNS boolean
LANGUAGE sql
IMMUTABLE
AS $$
    SELECT $1::integer = $2;
$$;

DROP OPERATOR IF EXISTS public.= (integer, text);
CREATE OPERATOR public.= (
  FUNCTION = public.integer_equals_text,
  LEFTARG = integer,
  RIGHTARG = text,
  COMMUTATOR = OPERATOR(public.=)
);

DROP OPERATOR IF EXISTS public.= (text, integer);
CREATE OPERATOR public.= (
  FUNCTION = public.text_equals_integer,
  LEFTARG = text,
  RIGHTARG = integer,
  COMMUTATOR = OPERATOR(public.=)
);



-- statistics_media's "reject empty" trigger is gone (v1.3.17-clusterplex.58,
-- 2026-11-10), from every schema that carries it, guide schemas included.
-- PMS opens a statistics bucket with count 0 and duration 0, then adds to it
-- with UPDATE ... WHERE id = <the inserted row's id>; the trigger dropped
-- that insert, so no bucket ever existed, every update missed, and PMS read
-- back another insert's id, or 0 on a fresh connection, where it threw
-- (talos held 0 rows against a sequence at 16445). Dropping it here, which
-- runs at every start, reaches databases made before it left the dump.
DO $drop_reject_empty$
DECLARE
    fn regprocedure;
BEGIN
    FOR fn IN
        SELECT p.oid::regprocedure FROM pg_proc p WHERE p.proname = 'reject_empty_statistics'
    LOOP
        EXECUTE format('DROP FUNCTION IF EXISTS %s CASCADE', fn);
    END LOOP;
END
$drop_reject_empty$;

-- levenshtein(text, text): Plex's voice search (GET /hubs/search/voice) orders
-- each library's items by LEVENSHTEIN('<what was said>', metadata_items.title),
-- a function PMS registers on its own SQLite handles. Without one here every
-- voice search answered 500 ("function levenshtein(unknown, text) does not
-- exist"; found on the upgrade rig, 2026-11-16, on PMS 1.43.5.11029 and
-- 1.43.5.11079 alike). PMS's function folds case: a stock PMS 1.43.5.11079
-- reports distance 0 between ABCDEF and the title abcdef, 1 to ABCDEX and 2
-- to Abcdxx, whichever case the query is in.
--
-- fuzzystrmatch's C function does the work when the extension can be had (it
-- is trusted, so the database's owner may install it): in a schema of its
-- own, off the search path, because it compares case as written and refuses
-- an argument over 255 characters, where a refusal would fail the whole
-- search. Without the extension a PL/pgSQL one answers the same, about a
-- hundred times slower (1 s over 7,679 episode titles on the rig).
DO $levenshtein$
DECLARE
    ext_schema name;
    wrapped boolean := false;
BEGIN
    BEGIN
        CREATE SCHEMA IF NOT EXISTS plex_fuzzy;
        CREATE EXTENSION IF NOT EXISTS fuzzystrmatch WITH SCHEMA plex_fuzzy;
    EXCEPTION WHEN OTHERS THEN
        RAISE NOTICE 'fuzzystrmatch not installed (%)', SQLERRM;
    END;

    -- The extension's own function, wherever an administrator put it.
    SELECT n.nspname INTO ext_schema
      FROM pg_proc p
      JOIN pg_namespace n ON n.oid = p.pronamespace
      JOIN pg_depend d ON d.classid = 'pg_proc'::regclass AND d.objid = p.oid AND d.deptype = 'e'
      JOIN pg_extension e ON e.oid = d.refobjid AND e.extname = 'fuzzystrmatch'
     WHERE p.proname = 'levenshtein' AND p.pronargs = 2
     LIMIT 1;

    IF ext_schema = 'public' THEN
        -- Already at the name PMS calls, where no wrapper can stand: it
        -- answers, case as written.
        RAISE NOTICE 'fuzzystrmatch is installed in public; its levenshtein() serves voice search as it is';
        RETURN;
    END IF;

    IF ext_schema IS NOT NULL THEN
        -- Refused when this role may not use the schema the extension is in
        -- (an administrator's, never granted): the PL/pgSQL one below then.
        BEGIN
            EXECUTE format($f$
                CREATE OR REPLACE FUNCTION public.levenshtein(a text, b text) RETURNS integer
                LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
                AS 'SELECT %I.levenshtein(left(lower(a), 255), left(lower(b), 255))'
            $f$, ext_schema);
            PERFORM public.levenshtein('a', 'b');
            wrapped := true;
        EXCEPTION WHEN OTHERS THEN
            RAISE NOTICE 'fuzzystrmatch in schema % cannot be used (%)', ext_schema, SQLERRM;
        END;
    END IF;

    IF NOT wrapped THEN
        RAISE NOTICE 'levenshtein() is the PL/pgSQL one';
        CREATE OR REPLACE FUNCTION public.levenshtein(a text, b text) RETURNS integer
        LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE
        AS $fn$
        DECLARE
            s text[] := string_to_array(left(lower(a), 255), NULL);
            t text[] := string_to_array(left(lower(b), 255), NULL);
            n int := coalesce(array_length(s, 1), 0);
            m int := coalesce(array_length(t, 1), 0);
            prev int[];
            cur int[];
            c text;
            v int;
        BEGIN
            IF n = 0 THEN RETURN m; END IF;
            IF m = 0 THEN RETURN n; END IF;
            prev := array(SELECT generate_series(0, m));
            FOR i IN 1..n LOOP
                c := s[i];
                cur := ARRAY[i];
                FOR j IN 1..m LOOP
                    v := prev[j] + (c <> t[j])::int;
                    IF prev[j + 1] + 1 < v THEN v := prev[j + 1] + 1; END IF;
                    IF cur[j] + 1 < v THEN v := cur[j] + 1; END IF;
                    cur := cur || v;
                END LOOP;
                prev := cur;
            END LOOP;
            RETURN prev[m + 1];
        END
        $fn$;
    END IF;
END
$levenshtein$;
