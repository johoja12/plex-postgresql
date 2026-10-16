-- 2026-10-16: full-text search, collations, stock indexes, spellfix, PG-only extras
--
-- Brings a database that plex_schema.sql created before 2026-10-16 (Plex
-- 1.43.4) to the state a fresh plex_schema.sql now creates. Audit items #1
-- (FTS), #24 (collations), #25 (stock indexes), #26 (spellfix) and #27
-- (PG-only extras) of docs/audits/2026-10-16-shim-audit.md.
--
-- Apply, with Plex stopped (the ALTER TABLEs take ACCESS EXCLUSIVE locks on
-- metadata_items, tags and library_sections):
--
--     psql -U postgres -d plex -v ON_ERROR_STOP=1 \
--          -f schema/migrations/2026-10-16-fts-collations-indexes.sql
--
-- Every statement is re-runnable: the file can be applied again after a
-- failure, or to a database that already has part of it.
--
-- Privileges. Nothing here needs a superuser on PostgreSQL 13 or later:
--   * CREATE EXTENSION pg_trgm / unaccent: both are trusted extensions, so
--     the database owner (CREATE on the database) may install them. On
--     PostgreSQL 12 or earlier, or when the applier is not the database
--     owner, these two statements need a superuser.
--   * CREATE COLLATION: CREATE on schema plex.
--   * ALTER TABLE, DROP TRIGGER, CREATE INDEX: ownership of the tables.
-- Run it as postgres (CloudNativePG's superuser) or as the owner of schema
-- plex and its tables.
--
-- Server. PostgreSQL 15 or later (the live database is 15.19 on
-- CloudNativePG). The collations need a server built with ICU; without it
-- the collation block NOTICEs and skips, and the three NOCASE columns stay
-- case-sensitive (everything else still applies).
--
-- The whole file runs in one transaction: a failure leaves the database as
-- it was.

SET search_path = plex, public;
SET client_min_messages = notice;
-- Do not queue behind a running Plex for ever; ON_ERROR_STOP then aborts the
-- transaction and the file can be re-applied once Plex is stopped.
SET lock_timeout = '60s';

BEGIN;

------------------------------------------------------------------------------
-- 0. Extensions
------------------------------------------------------------------------------
-- The entrypoints create pg_trgm in the first schema of the loader's
-- search_path, which is `plex` under PGUSER=plex (plex_schema.sql's
-- `plex.gin_trgm_ops` is that), and `public` elsewhere. IF NOT EXISTS keeps
-- whichever an existing database has; every reference below resolves the
-- extension's schema from pg_extension rather than assuming one.
CREATE EXTENSION IF NOT EXISTS pg_trgm WITH SCHEMA plex;
CREATE EXTENSION IF NOT EXISTS unaccent WITH SCHEMA plex;

------------------------------------------------------------------------------
-- #24 Collations
------------------------------------------------------------------------------
-- Stock Plex declares tags.tag, metadata_items.title_sort and
-- library_sections.name_sort COLLATE NOCASE and sorts with its ICU
-- `icu_root` collation. plex.nocase is ICU root at strength 2 (case folds,
-- accents still count) and non-deterministic, so `=`, IN, GROUP BY, DISTINCT
-- and btree lookups on those columns are case-insensitive as under SQLite.
-- plex.icu_root is ICU root at default strength, for ORDER BY ... COLLATE.
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_collation c JOIN pg_namespace n ON n.oid = c.collnamespace
                   WHERE n.nspname = 'plex' AND c.collname = 'nocase') THEN
        BEGIN
            CREATE COLLATION plex.nocase (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
        EXCEPTION WHEN OTHERS THEN
            RAISE NOTICE 'plex.nocase not created (%: %); tags.tag, metadata_items.title_sort and library_sections.name_sort stay case-sensitive', SQLSTATE, SQLERRM;
        END;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_collation c JOIN pg_namespace n ON n.oid = c.collnamespace
                   WHERE n.nspname = 'plex' AND c.collname = 'icu_root') THEN
        BEGIN
            CREATE COLLATION plex.icu_root (provider = icu, locale = 'und');
        EXCEPTION WHEN OTHERS THEN
            RAISE NOTICE 'plex.icu_root not created (%: %)', SQLSTATE, SQLERRM;
        END;
    END IF;
END $$;

------------------------------------------------------------------------------
-- #1 Full-text search: plex.fold, plex.fts_query
------------------------------------------------------------------------------
-- plex.fold(text): unaccent(lower(x)), the one normalisation both sides of a
-- MATCH go through. unaccent() itself is STABLE (its dictionary can be
-- reloaded), so it is wrapped in an IMMUTABLE SQL function naming the
-- dictionary explicitly, which lets the GIN indexes below be built over it.
-- If the unaccent rules file ever changes, REINDEX those four indexes.
-- The result is COLLATE "C": a non-deterministic collation (plex.nocase on
-- tags.tag and title_sort) would otherwise ride through lower() into the
-- result and refuse LIKE, ~ and regexp_* on it; folded text is compared
-- byte for byte, which is what a normalisation is for.
DO $$
DECLARE
    ns text;
BEGIN
    SELECT n.nspname INTO ns FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace
     WHERE e.extname = 'unaccent';
    IF ns IS NULL THEN
        RAISE EXCEPTION 'extension unaccent is not installed';
    END IF;
    EXECUTE format(
        'CREATE OR REPLACE FUNCTION plex.fold(text) RETURNS text LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE AS %L',
        format('SELECT %I.unaccent(%L::regdictionary, lower($1)) COLLATE "C"', ns, ns || '.unaccent'));
END $$;

-- plex.fts_lexemes(text, boolean): the lexemes to_tsvector('simple', plex.fold(x))
-- would make of x, in order of position, as one tsquery fragment joined by &
-- (with :* on each when prefix is true). Using the parser itself keeps a
-- query's tokens identical to the indexed text's, hyphens, numbers and all.
CREATE OR REPLACE FUNCTION plex.fts_lexemes(src text, prefix boolean)
RETURNS text
LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
AS $$
    SELECT string_agg(
               '''' || replace(replace(l.lexeme, '\', '\\'), '''', '''''') || ''''
               || CASE WHEN prefix THEN ':*' ELSE '' END,
               ' & ' ORDER BY l.positions[1])
      FROM unnest(to_tsvector('simple', plex.fold(src))) AS l
$$;

-- plex.fts_phrase(text): the same lexemes chained as a phrase, <N> carrying
-- the parser's own position gaps so a hyphenated word (which the parser
-- emits whole and in parts) still matches the vector it produced.
CREATE OR REPLACE FUNCTION plex.fts_phrase(src text, prefix boolean)
RETURNS text
LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
AS $$
    WITH w AS (
        SELECT l.lexeme, p.pos
          FROM unnest(to_tsvector('simple', plex.fold(src))) AS l,
               unnest(l.positions) AS p(pos)
    ), o AS (
        SELECT lexeme, pos,
               pos - lag(pos) OVER (ORDER BY pos, lexeme) AS gap,
               row_number() OVER (ORDER BY pos DESC, lexeme DESC) AS from_end
          FROM w
    )
    SELECT string_agg(
               CASE WHEN gap IS NULL THEN '' ELSE ' <' || gap || '> ' END
               || '''' || replace(replace(lexeme, '\', '\\'), '''', '''''') || ''''
               || CASE WHEN prefix AND from_end = 1 THEN ':*' ELSE '' END,
               '' ORDER BY pos, lexeme)
      FROM o
$$;

-- plex.fts_term(text, boolean): one term of an FTS4 query as a tsquery
-- fragment, or NULL when nothing is left of it. A leading - negates a
-- plain term; a column filter (title:, title_sort:, original_title:, tag:)
-- is dropped, the column being whatever the caller matches against; a
-- trailing * makes a prefix match. A phrase chains its lexemes.
CREATE OR REPLACE FUNCTION plex.fts_term(tok text, is_phrase boolean)
RETURNS text
LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE
AS $$
DECLARE
    neg    boolean := false;
    prefix boolean;
    piece  text;
BEGIN
    IF NOT is_phrase THEN
        IF left(tok, 1) = '-' THEN
            neg := true;
            tok := substr(tok, 2);
        END IF;
        tok := regexp_replace(tok, '^(title|title_sort|original_title|tag):', '');
    END IF;
    prefix := right(tok, 1) = '*';
    tok := rtrim(tok, '*');
    piece := CASE WHEN is_phrase THEN plex.fts_phrase(tok, prefix)
                  ELSE plex.fts_lexemes(tok, prefix) END;
    IF piece IS NULL OR piece = '' THEN
        RETURN NULL;
    END IF;
    IF position(' ' IN piece) > 0 THEN
        piece := '(' || piece || ')';
    END IF;
    IF neg THEN
        piece := '!' || piece;
    END IF;
    RETURN piece;
END
$$;

-- plex.fts_query(text): FTS4's query syntax, as Plex writes it, to a tsquery.
--   * surrounding whitespace and any enclosing (...) pairs are stripped;
--   * "quoted words" are a phrase (<-> chained; a trailing * prefixes its
--     last word); ( and ) elsewhere only separate terms;
--   * terms are split on whitespace, each folded through plex.fold and
--     tokenised by the 'simple' parser (plex.fts_lexemes), so punctuation
--     never breaks the query; a term ending in * is a prefix match (token:*);
--   * OR (upper case, as FTS4 requires) is |, and binds tighter than the
--     implicit AND between terms, as in FTS4: a b OR c is a & (b | c);
--   * a leading - negates the term (!);
--   * nothing left yields a query no row matches.
-- The assembled string, every lexeme single-quoted, is cast with ::tsquery
-- rather than passed to to_tsquery(): to_tsquery() re-parses a quoted
-- token, which would split a lexeme the parser already produced
-- ('foo-bar':* becomes 'foo-bar':* <-> 'foo':* <-> 'bar':*); the cast takes
-- the lexemes as they are.
CREATE OR REPLACE FUNCTION plex.fts_query(term text)
RETURNS tsquery
LANGUAGE plpgsql IMMUTABLE STRICT PARALLEL SAFE
AS $$
DECLARE
    s            text := btrim(term);
    n            int;
    i            int := 1;
    c            text;
    tok          text := '';
    in_phrase    boolean := false;
    do_flush     boolean;
    flush_phrase boolean := false;
    pending_or   boolean := false;
    piece        text;
    cur          text := NULL;      -- the OR group being built
    groups       text[] := '{}';    -- finished OR groups, joined by &
    q            text;
BEGIN
    WHILE length(s) >= 2 AND left(s, 1) = '(' AND right(s, 1) = ')' LOOP
        s := btrim(substr(s, 2, length(s) - 2));
    END LOOP;
    n := length(s);

    -- one past the end closes an open phrase, or ends the last term
    WHILE i <= n + 1 LOOP
        c := CASE WHEN i <= n THEN substr(s, i, 1)
                  WHEN in_phrase THEN '"'
                  ELSE ' ' END;
        do_flush := false;
        IF in_phrase THEN
            IF c = '"' THEN
                do_flush := true;
                flush_phrase := true;
                in_phrase := false;
            ELSE
                tok := tok || c;
            END IF;
        ELSIF c = '"' THEN
            do_flush := true;
            flush_phrase := false;
            in_phrase := true;
        ELSIF c ~ '^\s$' OR c IN ('(', ')') THEN
            do_flush := true;
            flush_phrase := false;
        ELSE
            tok := tok || c;
        END IF;

        IF do_flush AND tok <> '' THEN
            IF tok = 'OR' AND NOT flush_phrase THEN
                pending_or := cur IS NOT NULL;
            ELSE
                piece := plex.fts_term(tok, flush_phrase);
                IF piece IS NOT NULL THEN
                    IF cur IS NULL THEN
                        cur := piece;
                    ELSIF pending_or THEN
                        cur := cur || ' | ' || piece;
                    ELSE
                        groups := groups || cur;
                        cur := piece;
                    END IF;
                END IF;
                pending_or := false;
            END IF;
        END IF;
        IF do_flush THEN
            tok := '';
        END IF;
        i := i + 1;
    END LOOP;

    IF cur IS NOT NULL THEN
        groups := groups || cur;
    END IF;
    IF cardinality(groups) = 0 THEN
        -- a query no row matches, without the empty-tsquery notice
        RETURN '''plex.fts_query.nothing'' & !''plex.fts_query.nothing'''::tsquery;
    END IF;
    SELECT string_agg(CASE WHEN position(' | ' IN g) > 0 THEN '(' || g || ')' ELSE g END, ' & ')
      INTO q
      FROM unnest(groups) AS g;
    RETURN q::tsquery;
END
$$;

------------------------------------------------------------------------------
-- #1 / #27 The FTS views, the dead tsvector columns and their triggers
------------------------------------------------------------------------------
-- The views are what Plex's `fts4_metadata_titles[_icu]` and
-- `fts4_tag_titles[_icu]` resolve to. Every column Plex names exists:
-- rowid (and FTS4's docid alias), title, title_sort, original_title for the
-- metadata views, rowid, docid, tag (and tag AS title, which the old views
-- exposed) for the tag views. A MATCH is rewritten to
-- to_tsvector('simple', plex.fold(<col>)) @@ plex.fts_query(<term>), which
-- the planner serves from the expression indexes on the base tables.
-- The views have to go before the columns they named can be dropped or
-- re-collated.
DROP VIEW IF EXISTS plex.fts4_metadata_titles;
DROP VIEW IF EXISTS plex.fts4_metadata_titles_icu;
DROP VIEW IF EXISTS plex.fts4_tag_titles;
DROP VIEW IF EXISTS plex.fts4_tag_titles_icu;

DROP TRIGGER IF EXISTS metadata_items_search_update ON plex.metadata_items;
DROP TRIGGER IF EXISTS tags_search_update ON plex.tags;
DROP FUNCTION IF EXISTS plex.metadata_items_search_trigger();
DROP FUNCTION IF EXISTS plex.tags_search_trigger();
DROP INDEX IF EXISTS plex.idx_metadata_items_fts;
DROP INDEX IF EXISTS plex.idx_metadata_items_search;
DROP INDEX IF EXISTS plex.idx_tags_fts;
ALTER TABLE plex.metadata_items DROP COLUMN IF EXISTS title_fts;
ALTER TABLE plex.metadata_items DROP COLUMN IF EXISTS search_vector;
ALTER TABLE plex.tags DROP COLUMN IF EXISTS search_vector;
-- #27: subtype (rating_count mod 100) is not a Plex column; nothing in the
-- shim or cluster-plex reads it.
ALTER TABLE plex.metadata_items DROP COLUMN IF EXISTS subtype;

------------------------------------------------------------------------------
-- #24 Apply the NOCASE collation to the three stock NOCASE columns
------------------------------------------------------------------------------
-- A non-deterministic collation refuses LIKE, ILIKE, ~, strpos() and the
-- trigram operators on the column (PostgreSQL 15): "nondeterministic
-- collations are not supported for LIKE". No statement in the live corpus
-- does that on these three columns; a translator path that must pattern-
-- match one writes `col COLLATE plex.icu_root LIKE ...` or compares through
-- plex.fold(col). See docs/translator/KNOWN_GAPS.md.
-- ALTER COLUMN ... TYPE with only the collation changed rewrites no rows but
-- rebuilds every index on the column (idx_tags_tag, idx_metadata_items_
-- title_sort, idx_metadata_items_section_type_sort, ...).
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_collation c JOIN pg_namespace n ON n.oid = c.collnamespace
                   WHERE n.nspname = 'plex' AND c.collname = 'nocase') THEN
        RAISE NOTICE 'plex.nocase does not exist; leaving tags.tag, metadata_items.title_sort and library_sections.name_sort as they are';
        RETURN;
    END IF;
    IF (SELECT a.attcollation <> 'plex.nocase'::regcollation FROM pg_attribute a
         WHERE a.attrelid = 'plex.tags'::regclass AND a.attname = 'tag') THEN
        ALTER TABLE plex.tags ALTER COLUMN tag TYPE text COLLATE plex.nocase;
    END IF;
    IF (SELECT a.attcollation <> 'plex.nocase'::regcollation FROM pg_attribute a
         WHERE a.attrelid = 'plex.metadata_items'::regclass AND a.attname = 'title_sort') THEN
        ALTER TABLE plex.metadata_items ALTER COLUMN title_sort TYPE text COLLATE plex.nocase;
    END IF;
    IF (SELECT a.attcollation <> 'plex.nocase'::regcollation FROM pg_attribute a
         WHERE a.attrelid = 'plex.library_sections'::regclass AND a.attname = 'name_sort') THEN
        ALTER TABLE plex.library_sections ALTER COLUMN name_sort TYPE text COLLATE plex.nocase;
    END IF;
END $$;

------------------------------------------------------------------------------
-- #1 The FTS views and their expression indexes
------------------------------------------------------------------------------
CREATE OR REPLACE VIEW plex.fts4_metadata_titles AS
 SELECT metadata_items.id AS rowid,
    metadata_items.id AS docid,
    metadata_items.title,
    metadata_items.title_sort,
    metadata_items.original_title
   FROM plex.metadata_items;

CREATE OR REPLACE VIEW plex.fts4_metadata_titles_icu AS
 SELECT metadata_items.id AS rowid,
    metadata_items.id AS docid,
    metadata_items.title,
    metadata_items.title_sort,
    metadata_items.original_title
   FROM plex.metadata_items;

CREATE OR REPLACE VIEW plex.fts4_tag_titles AS
 SELECT tags.id AS rowid,
    tags.id AS docid,
    tags.tag,
    tags.tag AS title
   FROM plex.tags;

CREATE OR REPLACE VIEW plex.fts4_tag_titles_icu AS
 SELECT tags.id AS rowid,
    tags.id AS docid,
    tags.tag,
    tags.tag AS title
   FROM plex.tags;

CREATE INDEX IF NOT EXISTS idx_metadata_items_title_fts
    ON plex.metadata_items USING gin (to_tsvector('simple', plex.fold(title)));
CREATE INDEX IF NOT EXISTS idx_metadata_items_title_sort_fts
    ON plex.metadata_items USING gin (to_tsvector('simple', plex.fold(title_sort)));
CREATE INDEX IF NOT EXISTS idx_metadata_items_original_title_fts
    ON plex.metadata_items USING gin (to_tsvector('simple', plex.fold(original_title)));
CREATE INDEX IF NOT EXISTS idx_tags_tag_fts
    ON plex.tags USING gin (to_tsvector('simple', plex.fold(tag)));

------------------------------------------------------------------------------
-- #26 spellfix: "did you mean" over a vocabulary of title and tag words
------------------------------------------------------------------------------
-- spellfix1's job -- the closest known words to a misspelt one -- is
-- answered by pg_trgm similarity over plex.spellfix_vocab, the distinct
-- folded words of metadata_items.title and tags.tag, kept by triggers.
-- Words are only ever added (a deleted title's words stay), which keeps the
-- triggers to one INSERT ... ON CONFLICT DO NOTHING per new or retitled row.
CREATE TABLE IF NOT EXISTS plex.spellfix_vocab (
    word text NOT NULL,
    CONSTRAINT spellfix_vocab_pkey PRIMARY KEY (word)
);

DO $$
DECLARE
    ns text;
BEGIN
    SELECT n.nspname INTO ns FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace
     WHERE e.extname = 'pg_trgm';
    IF ns IS NULL THEN
        RAISE EXCEPTION 'extension pg_trgm is not installed';
    END IF;
    EXECUTE format('CREATE INDEX IF NOT EXISTS idx_spellfix_vocab_trgm ON plex.spellfix_vocab USING gin (word %I.gin_trgm_ops)', ns);
    -- plex.spellfix_suggest(term, n): the n vocabulary words most similar
    -- to term, as spellfix1's (word, distance, score): distance and score
    -- are 100 * (1 - trigram similarity), 0 for the word itself. The %
    -- operator keeps the lookup on the trigram index and under
    -- pg_trgm.similarity_threshold.
    EXECUTE format($f$
        CREATE OR REPLACE FUNCTION plex.spellfix_suggest(term text, n integer DEFAULT 5)
        RETURNS TABLE(word text, distance integer, score integer)
        LANGUAGE sql STABLE STRICT PARALLEL SAFE
        AS $b$
            SELECT v.word,
                   (100 * (1 - %I.similarity(v.word, q.w)))::integer,
                   (100 * (1 - %I.similarity(v.word, q.w)))::integer
              FROM (SELECT plex.fold(term) AS w) AS q
              JOIN plex.spellfix_vocab AS v ON v.word OPERATOR(%I.%%) q.w
             WHERE q.w <> ''
             ORDER BY %I.similarity(v.word, q.w) DESC, v.word
             LIMIT n
        $b$ $f$, ns, ns, ns, ns);
END $$;

-- The words of one title or tag: folded, split on whitespace and
-- punctuation, two characters or longer.
CREATE OR REPLACE FUNCTION plex.spellfix_words(src text)
RETURNS SETOF text
LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
AS $$
    SELECT DISTINCT w
      FROM regexp_split_to_table(plex.fold(src) COLLATE "C", '[\s[:punct:]]+') AS w
     WHERE length(w) >= 2
$$;

CREATE OR REPLACE FUNCTION plex.spellfix_vocab_add()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    src text;
BEGIN
    IF TG_TABLE_NAME = 'tags' THEN
        src := NEW.tag;
    ELSE
        src := NEW.title;
    END IF;
    IF src IS NOT NULL AND src <> '' THEN
        INSERT INTO plex.spellfix_vocab (word)
        SELECT w FROM plex.spellfix_words(src) AS w
        ON CONFLICT DO NOTHING;
    END IF;
    RETURN NULL;
END
$$;

DROP TRIGGER IF EXISTS spellfix_vocab_metadata_items_insert ON plex.metadata_items;
CREATE TRIGGER spellfix_vocab_metadata_items_insert
    AFTER INSERT ON plex.metadata_items
    FOR EACH ROW WHEN (NEW.title IS NOT NULL)
    EXECUTE FUNCTION plex.spellfix_vocab_add();
DROP TRIGGER IF EXISTS spellfix_vocab_metadata_items_update ON plex.metadata_items;
CREATE TRIGGER spellfix_vocab_metadata_items_update
    AFTER UPDATE OF title ON plex.metadata_items
    FOR EACH ROW WHEN (NEW.title IS DISTINCT FROM OLD.title)
    EXECUTE FUNCTION plex.spellfix_vocab_add();
DROP TRIGGER IF EXISTS spellfix_vocab_tags_insert ON plex.tags;
CREATE TRIGGER spellfix_vocab_tags_insert
    AFTER INSERT ON plex.tags
    FOR EACH ROW WHEN (NEW.tag IS NOT NULL)
    EXECUTE FUNCTION plex.spellfix_vocab_add();
DROP TRIGGER IF EXISTS spellfix_vocab_tags_update ON plex.tags;
CREATE TRIGGER spellfix_vocab_tags_update
    AFTER UPDATE OF tag ON plex.tags
    FOR EACH ROW WHEN (NEW.tag IS DISTINCT FROM OLD.tag)
    EXECUTE FUNCTION plex.spellfix_vocab_add();

-- Seed the vocabulary from what the library already holds.
INSERT INTO plex.spellfix_vocab (word)
SELECT DISTINCT w
  FROM (SELECT title AS src FROM plex.metadata_items WHERE title IS NOT NULL
        UNION ALL
        SELECT tag FROM plex.tags WHERE tag IS NOT NULL) AS s,
       LATERAL plex.spellfix_words(s.src) AS w
ON CONFLICT DO NOTHING;

------------------------------------------------------------------------------
-- #27 PG-only guards stock Plex does not have
------------------------------------------------------------------------------
-- These rejected or rewrote Plex's own writes: a CHECK and a trigger that
-- refuse rows Plex writes legitimately (a parent in another section is how
-- Plex files a collection's items; metadata_type and library_section_id
-- are both NULL on rows Plex makes before it classifies them), a cycle
-- guard, and two triggers that re-parented seasons by a regex over the
-- file name. metadata_items_set_available_at stays: it only fills a NULL
-- available_at from added_at, which Plex's own scanner writes itself and
-- cluster-plex's projection does not.
ALTER TABLE plex.metadata_items DROP CONSTRAINT IF EXISTS chk_not_orphan;
DROP TRIGGER IF EXISTS check_cross_section_parent ON plex.metadata_items;
DROP TRIGGER IF EXISTS prevent_self_ref_parent ON plex.metadata_items;
DROP TRIGGER IF EXISTS trg_fix_orphan_season ON plex.metadata_items;
DROP TRIGGER IF EXISTS trg_fix_orphan_season_media ON plex.media_parts;
DROP FUNCTION IF EXISTS plex.prevent_cross_section_parent();
DROP FUNCTION IF EXISTS plex.prevent_self_referential_parent();
DROP FUNCTION IF EXISTS plex.fix_orphan_season_on_episode_insert();
DROP FUNCTION IF EXISTS plex.fix_orphan_season_on_media_part_insert();

------------------------------------------------------------------------------
-- #25 Indexes stock Plex has, under their stock names
------------------------------------------------------------------------------
-- Already covered, so not duplicated: media_items(deleted_at) by
-- idx_media_items_deleted_at; metadata_items(changed_at) by
-- idx_metadata_items_changed_at (DESC serves both directions);
-- metadata_items(library_section_id, metadata_type, added_at) by
-- idx_metadata_items_section_type_added.
CREATE INDEX IF NOT EXISTS index_media_items_on_channel_id_and_begins_at ON plex.media_items (channel_id, begins_at);
CREATE INDEX IF NOT EXISTS index_media_items_on_begins_at ON plex.media_items (begins_at);
CREATE INDEX IF NOT EXISTS index_media_items_on_ends_at ON plex.media_items (ends_at);
CREATE INDEX IF NOT EXISTS index_media_items_on_channel_id ON plex.media_items (channel_id);
CREATE INDEX IF NOT EXISTS index_media_items_on_media_analysis_version ON plex.media_items (media_analysis_version);
CREATE INDEX IF NOT EXISTS index_metadata_items_on_deleted_at ON plex.metadata_items (deleted_at);
CREATE INDEX IF NOT EXISTS index_metadata_items_on_created_at ON plex.metadata_items (created_at);
CREATE INDEX IF NOT EXISTS index_metadata_items_on_resources_changed_at ON plex.metadata_items (resources_changed_at);
CREATE INDEX IF NOT EXISTS index_metadata_item_views_on_viewed_at ON plex.metadata_item_views (viewed_at);
CREATE INDEX IF NOT EXISTS index_metadata_item_views_on_library_section_id ON plex.metadata_item_views (library_section_id);
CREATE INDEX IF NOT EXISTS index_tags_on_tag_type_and_tag ON plex.tags (tag_type, tag);
CREATE INDEX IF NOT EXISTS index_directories_on_deleted_at ON plex.directories (deleted_at);

-- cluster-plex's findMedia looks a file up by
-- media_items.extra_data LIKE '%"cp:mediaFile":"<uid>"%' (audit #30).
DO $$
DECLARE
    ns text;
BEGIN
    SELECT n.nspname INTO ns FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace
     WHERE e.extname = 'pg_trgm';
    EXECUTE format('CREATE INDEX IF NOT EXISTS idx_media_items_extra_data_trgm ON plex.media_items USING gin (extra_data %I.gin_trgm_ops)', ns);
END $$;

-- The UNIQUE ones are ON CONFLICT targets for Plex. A database that already
-- holds duplicates (rows whose key columns are all non-null and equal)
-- cannot take the index: those are reported and skipped, never failed.
CREATE FUNCTION pg_temp.unique_index_if_clean(idx text, tbl text, cols text[])
RETURNS void
LANGUAGE plpgsql
AS $$
DECLARE
    collist  text := (SELECT string_agg(quote_ident(c), ', ') FROM unnest(cols) AS c);
    not_null_cond  text := (SELECT string_agg(quote_ident(c) || ' IS NOT NULL', ' AND ') FROM unnest(cols) AS c);
    dups     text;
BEGIN
    IF EXISTS (SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
               WHERE n.nspname = 'plex' AND c.relname = idx) THEN
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
-- Stock SQLite's name for this one,
-- index_play_queues_on_client_identifier_and_account_id_and_metadata_type,
-- is 72 characters; PostgreSQL truncates identifiers at 63, so it is given
-- a name that fits as it is.
SELECT pg_temp.unique_index_if_clean('index_play_queues_on_client_identifier_account_id_metadata_type', 'play_queues', ARRAY['client_identifier', 'account_id', 'metadata_type']);
SELECT pg_temp.unique_index_if_clean('media_grabs_on_uuid', 'media_grabs', ARRAY['uuid']);
SELECT pg_temp.unique_index_if_clean('media_provider_resources_on_uuid', 'media_provider_resources', ARRAY['uuid']);
SELECT pg_temp.unique_index_if_clean('index_external_metadata_sources_on_uri', 'external_metadata_sources', ARRAY['uri']);
DROP FUNCTION pg_temp.unique_index_if_clean(text, text, text[]);

COMMIT;

ANALYZE plex.metadata_items;
ANALYZE plex.tags;
ANALYZE plex.spellfix_vocab;
