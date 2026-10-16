-- The virtual tables stock Plex's com.plexapp.plugins.library.db declares,
-- kept out of sqlite_schema.sql on purpose.
--
-- sqlite_schema.sql builds the shadow SQLite databases (the entrypoints'
-- init_sqlite_schema, cluster-plex's bootstrap.BuildShadow), and no SQLite
-- those loaders have can create these: spellfix1 is a loadable extension
-- the sqlite3 command does not carry, Plex's "collating" tokenizer exists
-- only inside Plex Media Server, and the fts4 module is not in the pure-Go
-- SQLite cluster-plex uses. Each would fail with an error line, as the
-- audit's item #29 recorded. Nothing needs them in the shadow: the shim
-- answers every MATCH from PostgreSQL through the fts4_* views and every
-- spellfix lookup through plex.spellfix_suggest (schema/plex_schema.sql).
-- The tables they would make (fts4_*_segments/_segdir/_docsize/_stat,
-- spellfix_*_vocab) stay in sqlite_schema.sql as plain tables, as a Plex
-- .schema dump lists them, so the shadow holds the same object set it
-- always did.
--
-- Load this file only into a SQLite that has the modules, after
-- sqlite_schema.sql, when building a database for a Plex that runs on SQLite.
CREATE VIRTUAL TABLE spellfix_metadata_titles USING spellfix1;
CREATE VIRTUAL TABLE spellfix_tag_titles USING spellfix1;
CREATE VIRTUAL TABLE fts4_metadata_titles USING fts4(content="metadata_items", title, title_sort, original_title)
/* fts4_metadata_titles(title,title_sort,original_title) */;
CREATE VIRTUAL TABLE fts4_tag_titles USING fts4(content="tags", tag)
/* fts4_tag_titles(tag) */;
CREATE VIRTUAL TABLE fts4_metadata_titles_icu USING fts4(content="metadata_items", title, title_sort, original_title, tokenize=collating 'root@colStrength=primary;colAlternate=shifted');
CREATE VIRTUAL TABLE fts4_tag_titles_icu USING fts4(content="tags", tag, tokenize=collating 'root@colStrength=primary;colAlternate=shifted');

-- The rtree builds in the shadow, but Plex's PRAGMA table_info('locations')
-- then read "undersize RTree blobs in locations_node" (SQLITE_CORRUPT_VTAB,
-- 2026-10-16): the table's reads are answered by PostgreSQL's plain
-- locations table, so the shadow keeps no rtree at all.
CREATE VIRTUAL TABLE "locations" USING rtree("id" integer, "lat_min" float, "lat_max" float, "lon_min" float, "lon_max" float);
