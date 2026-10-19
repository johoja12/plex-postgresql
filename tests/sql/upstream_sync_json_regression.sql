-- Run with psql -v ON_ERROR_STOP=1 on an isolated PostgreSQL database.
-- These are the translator's json_each outputs for bare/qualified value,
-- explicit aliases, a JSON path and SQLite's zero-based key.
BEGIN;
CREATE TEMP TABLE json_membership_fixture(tag text);
INSERT INTO json_membership_fixture VALUES ('A24'), ('日本'), ('keep');
DELETE FROM json_membership_fixture
 WHERE tag IN (SELECT value::TEXT
                 FROM json_array_elements_text('["A24","日本"]'::JSON)
                      AS json_each (value));
DO $test$
DECLARE actual text[]; keys bigint[];
BEGIN
    IF (SELECT array_agg(tag) FROM json_membership_fixture) <> ARRAY['keep'] THEN
        RAISE EXCEPTION 'json_each collection deletion did not match element text';
    END IF;
    SELECT array_agg(j.value) INTO actual
      FROM json_array_elements_text('["A24","日本"]'::JSON) AS j (value);
    IF actual <> ARRAY['A24','日本'] THEN
        RAISE EXCEPTION 'json_each aliased value contract failed: %', actual;
    END IF;
    SELECT array_agg(value::TEXT) INTO actual
      FROM jsonb_array_elements_text('{"a":["A24","日本"]}'::jsonb
                                    #> string_to_array('a', ',')) AS json_each (value);
    IF actual <> ARRAY['A24','日本'] THEN
        RAISE EXCEPTION 'json_each path value contract failed: %', actual;
    END IF;
    SELECT array_agg(key - 1) INTO keys
      FROM json_array_elements_text('["A24","日本"]'::JSON)
           WITH ORDINALITY AS json_each (value, key);
    IF keys <> ARRAY[0,1]::bigint[] THEN
        RAISE EXCEPTION 'json_each key contract failed: %', keys;
    END IF;
END;
$test$;
ROLLBACK;
