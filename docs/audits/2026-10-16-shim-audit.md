# plex-postgresql shim audit, 2026-10-16

Three read-only audits (translator, runtime, schema) of plex-postgresql at v1.3.17-clusterplex.30
plus live probes against Plex 1.43.4.10903 on talos (cluster-plex 07de78a). Live-confirmed items are marked.

## Tier 1: breaks a Plex feature today

1. Full-text search never returns rows (live-confirmed). Views fts4_metadata_titles(_icu) lack title_sort/original_title, fts4_tag_titles(_icu) lacks tag (schema/plex_schema.sql:1136-1195); convert_fts_term has no `*` arm and passes `(` `)` through (query.rs:1393-1577); a bound term becomes `@@ $1` (raw string cast to tsquery). The `title=` library filter answers 0 items; hub search only works through Plex's non-FTS sibling statements.
2. Permanent PG errors handled two ways by a substring test (live-confirmed): should_use_streaming (db_interpose_step_read_utils/reexecution.rs:28-36) checks for "limit 1" as a substring; streaming path retries any error 5x (~10.5 s) then SQLITE_ERROR/HTTP 500 (streaming_fetch.rs:82-103, step/retry.rs:24-61); eager path logs "EAGER FALLBACK", returns DONE with 0 rows and no error (eager_fetch.rs:40-57). Fix: one classifier (retry only on connection loss, 08/57P0x/40001/26000; fail fast on 22/23/42) used by step and exec; eager must return ERROR.
3. PG write errors reported as success (write_exec.rs:170-205, cached_write.rs:257-279, exec/pg_path.rs:289-307): constraint/type violations return SQLITE_DONE, changes() keeps the previous count, last_insert_rowid returns the previous id.
4. Projection alias in WHERE left as-is (query.rs:105): `min(begins_at, ends_at) as minimum_at ... where minimum_at > ?` (live corpus, Live TV) -> PG column does not exist.
5. LIKE case-insensitivity inverted (live-confirmed: studio=a24 -> 0, A24 -> 7). SQLite LIKE is ASCII case-insensitive; shim keeps LIKE unless COLLATE NOCASE (query.rs:641). GLOB -> ILIKE is wrong the other way (preprocess.rs:2282).
6. Pool exhaustion / low stack silently routes to the empty shadow SQLite (pg_client/registry_api.rs:80-88, prepare/internal_flow.rs:290-300): reads empty, writes lost.
7. Pool slot leaks: streaming_active never cleared when "streaming_conn invalid" fires (pg_statement/stmt_lifecycle.rs:21-27; reset_conn also); slots thread-owned and released only by the 300 s reaper; db_to_pool reassigned by every errcode/changes call.

## Tier 2: wrong results or statement failures in plausible shapes

8. Placeholder walker gaps (placeholders.rs:377 `_ => {}`): IS NULL on a param (likely the live orig_q=2/translated_params=1), COLLATE, SUBSTR/TRIM, window/FILTER, tuples, ?NNN/@name/$name, SetExpr::Update. Bind past the translated count is silently dropped (bind/support.rs:256-275) and earlier removals shift later values left. Fix: sqlparser VisitorMut (feature already on) and SQLITE_RANGE on unmapped index.
9. `LIMIT ?, ?` swaps count/offset binds (preprocess.rs:284-361); UPDATE ... LIMIT loses SET binds (preprocess.rs:94-183).
10. `qualified.col COLLATE NOCASE` is a parse error (preprocess.rs:2565 / sqlparser 0.61).
11. functions.rs walker gaps: json_extract/ifnull/strftime under IS NULL, LIKE, LEFT JOIN ON, ON CONFLICT SET, LIMIT survive into PG (functions.rs:1026-1108, :175-179). Bailing rewrites drain the args (functions.rs:1208): `json_extract(extra_data, ?)` -> `json_extract()`.
12. query.rs::transform_expr never enters subqueries/JOIN ON (query.rs:421-685, :145-150): max(a,b), COLLATE nocase, id='' inside IN/EXISTS reach PG.
13. CASE 1/0 -> TRUE/FALSE in every context (query.rs:451): sum(case ... then 1 else 0) fails.
14. KNOWN_TEXT_COLUMNS lists integer columns (query.rs:893): `status <> 1` -> `<> 1::TEXT` fails; id='' fold should cover every integer column and IN lists.
15. `(metadata_type=18)` alone folds to TRUE (query.rs:822): a collections-only query returns the whole section.
16. Positional duplicate SET dedup drops a bind slot (dedup.rs:59).
17. strftime/datetime modifiers: 'utc','localtime','start of day' -> INTERVAL error (functions.rs:1243); datetime() drops modifiers, date()/time() unmapped (functions.rs:1012); non-%s strftime on epoch columns skips TO_TIMESTAMP (functions.rs:1278-1357).
18. group_concat(DISTINCT/ORDER BY) loses both (functions.rs:665); substr(x,-n) wrong (functions.rs:964); typeof() outside IN wrong (functions.rs:835, plex.sqlite_typeof exists); `IS <expr>` parse error; Shape-A self-join text rewrite breaks on LEFT JOIN (preprocess.rs:2023); LIMIT -1 reaches PG; ORDER BY x IS NULL DESC nulls order.
19. sqlite3_exec never invokes the callback / sets errmsg on PG handles (db_interpose_exec.rs:206-216).
20. Accessors: sqlite3_value_text 16 KiB process-global ring (type_text.rs:139-148); column_text 64-slot ring lifetime; timestamp OIDs as INTEGER -> atoll; decltype TEXT for expression columns without metadata; control bytes in text binds sent as bytea hex (pure_helpers.rs:165-179).
21. last_insert_rowid falls back to a process-global value (connection_state.rs:101-108); errmsg/changes acquire a pool slot and can block ~10 s.
22. Transactions are no-ops on PG (step.rs:213-225): ROLLBACK leaves partial writes committed. Slots are thread-affine so BEGIN/COMMIT/SAVEPOINT could be forwarded.
23. Stale-plan detection only on 26000; add 0A000 (hash_sqlstate.rs:20-22).

## Tier 3: schema and features stock Plex has

24. Collations: tags.tag, metadata_items.title_sort, library_sections.name_sort are COLLATE NOCASE in stock; PG plain text -> case-sensitive tag lookups (duplicate genres/people). COLLATE icu_root stripped (preprocess.rs:2567) and no DB collation set -> sort order is lc_collate. Fix: CREATE COLLATION plex.nocase (icu, und-u-ks-level2) and plex.icu_root (icu, und).
25. Missing stock indexes: media_items(channel_id,begins_at),(begins_at),(ends_at),(channel_id) [Live TV EPG]; metadata_items(library_section_id,metadata_type,added_at) [Recently Added]; metadata_item_views(viewed_at),(library_section_id); tags(tag_type,tag); deleted_at/created_at/changed_at/resources_changed_at; media_items(media_analysis_version); UNIQUE play_queues(client_identifier,account_id,metadata_type), media_grabs(uuid), media_provider_resources(uuid), external_metadata_sources(uri) (ON CONFLICT targets).
26. spellfix1 ("did you mean") skipped outright (pg_config.rs:137-139); candidate replacement: pg_trgm similarity over a vocab table.
27. PG-only behaviour not in stock: chk_not_orphan CHECK, prevent_cross_section_parent/self_referential triggers, trg_fix_orphan_season/_media heuristics, metadata_items_set_available_at, `subtype` GENERATED column (rating_count mod 100; explains subtype 99 vs 1), dead `title_fts` column and GIN index, `ALTER TABLE plex.test` on a table never created.
28. PRAGMA integrity_check/quick_check/optimize/wal_checkpoint stripped to nothing (preprocess.rs:1149): "Optimize database" returns no `ok` row.
29. Startup noise: spellfix1/fts4 modules absent from the modernc shadow (harmless; FTS is blocked from the shadow); `locations_rowid already exists` is sqlite_schema.sql:59-61 pre-creating the rtree's own tables (delete those lines).
30. cluster-plex projection: insertItem writes extra_data='' (use NULL); changed_at/resources_changed_at left 0, hash/refreshed_at NULL; findMedia's `extra_data LIKE '%"cp:mediaFile":"<uid>"%'` has no index (gin trgm or jsonb expression index).

## Verified OK
Corpus of 87 live statements translates with exact parameter counts; prepared-statement names per connection with DEALLOCATE on reset; search_path reapplied per reset; pg_query_cache dead code. Paginated views answer in 0.2-1.2 s; unpaginated 900-item listings cost ~1,600 statements (Plex's own N+1).
