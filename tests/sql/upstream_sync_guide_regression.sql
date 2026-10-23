-- Run after plex_schema.sql and pg_compat_functions.sql in a disposable DB.
BEGIN;
CREATE SCHEMA sync_guide;
SET LOCAL search_path = sync_guide, public;
DO $test$
BEGIN
    IF NOT (42::integer = '42'::text) OR NOT ('42'::text = 42::integer) THEN
        RAISE EXCEPTION 'guide integer/text equality failed';
    END IF;
    IF EXISTS (SELECT 1 FROM pg_operator
               WHERE oprnamespace = 'public'::regnamespace AND oprname = '='
                 AND ((oprleft = 'integer'::regtype AND oprright = 'text'::regtype)
                   OR (oprleft = 'text'::regtype AND oprright = 'integer'::regtype))
                 AND oprcode = 0) THEN
        RAISE EXCEPTION 'guide equality has a shell operator';
    END IF;
    IF (SELECT array_agg(value::bigint ORDER BY value::bigint)
        FROM json_array_elements_text('[42,"43","bad",null]'::json) AS json_each(value)
        WHERE (value IS NULL OR value ~ '^\s*[-+]?[0-9]{1,18}\s*$'))
       IS DISTINCT FROM ARRAY[42,43,NULL]::bigint[] THEN
        RAISE EXCEPTION 'integer JSON membership changed elements';
    END IF;
    IF (99 NOT IN (SELECT value::bigint
                  FROM json_array_elements_text('[42,null]'::json) AS json_each(value)
                  WHERE (value IS NULL OR value ~ '^\s*[-+]?[0-9]{1,18}\s*$'))) IS NOT NULL THEN
        RAISE EXCEPTION 'NOT IN lost JSON null semantics';
    END IF;
END;
$test$;
SET LOCAL search_path = plex, public;
DO $test$
BEGIN
    IF NOT (42::integer = '42'::text) OR NOT ('42'::text = 42::integer) THEN
        RAISE EXCEPTION 'library integer/text equality regressed';
    END IF;
END;
$test$;
ROLLBACK;
