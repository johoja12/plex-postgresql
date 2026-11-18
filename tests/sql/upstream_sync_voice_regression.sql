-- Disposable database only, after pg_compat_functions.sql. Run with both
-- fuzzystrmatch available and unavailable to the caller, and after reload.
BEGIN;
DO $test$
BEGIN
    IF public.levenshtein('ABCDEF', 'abcdef') <> 0
       OR public.levenshtein('ABCDEX', 'abcdef') <> 1
       OR public.levenshtein('Abcdxx', 'abcdef') <> 2
       OR public.levenshtein('', 'abc') <> 3
       OR public.levenshtein('abc', '') <> 3
       OR public.levenshtein('日本語', '日本') <> 1
       OR public.levenshtein(repeat('a', 300), repeat('A', 300)) <> 0
       OR public.levenshtein(NULL, 'a') IS NOT NULL
    THEN RAISE EXCEPTION 'voice search case, Unicode, empty, long or NULL regression'; END IF;
    -- PostgreSQL execution of boolean instr() translation, including NULL.
    IF (SELECT array_agg(id ORDER BY id) FROM
        (VALUES (1, 'pv%3Acredits'), (2, 'other'), (3, NULL)) AS t(id, extra_data)
        WHERE (strpos(extra_data, 'pv%3Acredits') > 0)) IS DISTINCT FROM ARRAY[1]
       OR (SELECT array_agg(id ORDER BY id) FROM
        (VALUES (1, 'pv%3Acredits'), (2, 'other'), (3, NULL)) AS t(id, extra_data)
        WHERE NOT (strpos(extra_data, 'pv%3Acredits') > 0)) IS DISTINCT FROM ARRAY[2]
    THEN RAISE EXCEPTION 'boolean instr translation regression'; END IF;
END;
$test$;
ROLLBACK;
