-- Run on an isolated database after fresh schema creation or the migration.
-- Fixtures are temporary and every change is rolled back.
\set ON_ERROR_STOP on
BEGIN;
SET LOCAL search_path = pg_catalog, pg_temp;

DO $indexes$
DECLARE
    found integer;
BEGIN
    SELECT count(*) INTO found
      FROM pg_index i
      JOIN pg_class c ON c.oid = i.indexrelid
      JOIN pg_opclass op ON op.oid = i.indclass[0]
      JOIN pg_namespace opns ON opns.oid = op.opcnamespace
      JOIN pg_am am ON am.oid = c.relam
      JOIN pg_extension e ON e.extnamespace = opns.oid AND e.extname = 'pg_trgm'
     WHERE i.indrelid = 'plex.metadata_items'::regclass
       AND c.relname IN ('idx_metadata_items_title_trgm',
                         'idx_metadata_items_title_sort_trgm',
                         'idx_metadata_items_original_title_trgm')
       AND i.indisvalid AND i.indisready
       AND am.amname = 'gin' AND op.opcname = 'gin_trgm_ops'
       AND c.relowner = (SELECT relowner FROM pg_class WHERE oid = i.indrelid)
       AND (c.relname <> 'idx_metadata_items_title_sort_trgm'
            OR i.indcollation[0] = 'plex.icu_root'::regcollation);
    IF found <> 3 THEN
        RAISE EXCEPTION 'expected three valid title pattern indexes with the right operator class, owner and collation, found %', found;
    END IF;
END
$indexes$;

CREATE TEMP TABLE pattern_fixture (
    id integer PRIMARY KEY,
    metadata_type integer,
    title text,
    title_sort text COLLATE plex.nocase,
    original_title text
);
INSERT INTO pattern_fixture
SELECT n, 4, 'background item ' || n, 'background item ' || n, NULL
  FROM generate_series(100, 12100) AS n;
INSERT INTO pattern_fixture VALUES
 (1, 4, 'Star beacon', NULL, NULL),
 (2, 4, 'a sTaR beacon', NULL, NULL),
 (3, 4, NULL, 'sTaR sort-only', NULL),
 (4, 4, NULL, NULL, 'a STAR original-only'),
 (5, 4, NULL, 'Pokémon 日本', NULL),
 (6, 4, '日本語の星', NULL, NULL),
 (7, 4, NULL, NULL, 'Éclair'),
 (8, 1, 'Star excluded type', 'Star excluded type', NULL),
 (9, 4, NULL, NULL, NULL);

-- Reuse the actual index definitions on the fixture. This catches a migration
-- indexing title_sort's NOCASE collation rather than the query's icu_root.
DO $copy_indexes$
DECLARE
    idx record;
BEGIN
    FOR idx IN
        SELECT c.relname, pg_get_indexdef(c.oid) AS ddl
          FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid
         WHERE i.indrelid = 'plex.metadata_items'::regclass
           AND c.relname IN ('idx_metadata_items_title_trgm',
                             'idx_metadata_items_title_sort_trgm',
                             'idx_metadata_items_original_title_trgm')
    LOOP
        EXECUTE replace(idx.ddl,
            format('CREATE INDEX %I ON plex.metadata_items', idx.relname),
            format('CREATE INDEX %I ON pattern_fixture', 'fixture_' || idx.relname));
    END LOOP;
END
$copy_indexes$;
ANALYZE pattern_fixture;

DO $matching$
DECLARE
    term text;
    predicate text;
    sequential integer[];
    indexed integer[];
    expected integer[];
    plan json;
BEGIN
    FOREACH term IN ARRAY ARRAY['Star', 'pokémon', '日本', 'éCLAIR', 'absentneedle'] LOOP
        predicate := format($q$
            metadata_type = 4 AND (
              title ILIKE %L OR title ILIKE %L
              OR (title_sort COLLATE plex.icu_root) ILIKE %L
              OR (title_sort COLLATE plex.icu_root) ILIKE %L
              OR original_title ILIKE %L OR original_title ILIKE %L)
        $q$, term || '%', '% ' || term || '%',
             term || '%', '% ' || term || '%',
             term || '%', '% ' || term || '%');
        PERFORM set_config('enable_indexscan', 'off', true);
        PERFORM set_config('enable_bitmapscan', 'off', true);
        PERFORM set_config('enable_seqscan', 'on', true);
        EXECUTE 'SELECT array_agg(id ORDER BY id) FROM pattern_fixture WHERE ' || predicate
          INTO sequential;
        PERFORM set_config('enable_indexscan', 'on', true);
        PERFORM set_config('enable_bitmapscan', 'on', true);
        PERFORM set_config('enable_seqscan', 'off', true);
        EXECUTE 'SELECT array_agg(id ORDER BY id) FROM pattern_fixture WHERE ' || predicate
          INTO indexed;
        expected := CASE term WHEN 'Star' THEN ARRAY[1,2,3,4]
                             WHEN 'pokémon' THEN ARRAY[5]
                             WHEN '日本' THEN ARRAY[5,6]
                             WHEN 'éCLAIR' THEN ARRAY[7]
                             ELSE NULL::integer[] END;
        IF sequential IS DISTINCT FROM expected OR indexed IS DISTINCT FROM expected THEN
            RAISE EXCEPTION 'pattern %: expected %, sequential %, indexed %', term, expected, sequential, indexed;
        END IF;
        IF term = 'Star' THEN
            -- Check the normal cost-based plan, not a forced-index timing.
            PERFORM set_config('enable_seqscan', 'on', true);
            EXECUTE 'EXPLAIN (FORMAT JSON) SELECT id FROM pattern_fixture WHERE ' || predicate
              INTO plan;
            IF plan::text NOT LIKE '%fixture_idx_metadata_items_title_trgm%'
               OR plan::text NOT LIKE '%fixture_idx_metadata_items_title_sort_trgm%'
               OR plan::text NOT LIKE '%fixture_idx_metadata_items_original_title_trgm%'
            THEN
                RAISE EXCEPTION 'selective title search does not use all three pattern indexes: %', plan;
            END IF;
        END IF;
    END LOOP;
END
$matching$;
ROLLBACK;
