# Known Gaps

These are the areas where the translator is intentionally partial or where semantics still differ from native SQLite.

## Partial Areas
- JSON wildcard and exotic path forms may fall back to guarded PostgreSQL JSONPath rewrites instead of exact SQLite behavior.
- FTS: the schema answers FTS4's query syntax as Plex writes it (see below); the full FTS3/4/5 grammar (`NEAR`, `^`, the enhanced syntax's `NOT`) is not implemented, and a column filter (`title:term`) is dropped rather than honoured.
- DDL normalization is intentionally narrow and does not aim to cover arbitrary SQLite schema statements.
- Transaction keywords are translated for Plex workloads, but lock timing and savepoint behavior are not a full SQLite clone.

## Intentional Non-Support
- Virtual table modules such as `rtree` and custom tokenizer extensions (`spellfix1` and `fts4` are answered from PostgreSQL, below, not emulated as modules)
- SQLite extension loading and extension-specific PRAGMAs
- Table-valued helpers such as `json_tree`

## Schema-side support (since 2026-10-16)

Objects `schema/plex_schema.sql` provides for the translator to target. An
existing database gets them from `schema/migrations/2026-10-16-fts-collations-indexes.sql`
(`docs/migrations.md`).

### Full-text search

- `fts4_metadata_titles` and `fts4_metadata_titles_icu` are views over
  `metadata_items` with `rowid`, `docid`, `title`, `title_sort`,
  `original_title`; `fts4_tag_titles` and `fts4_tag_titles_icu` are views
  over `tags` with `rowid`, `docid`, `tag` and `tag AS title`.
- `plex.fold(text)` is `unaccent(lower(x))`, IMMUTABLE, result `COLLATE "C"`.
- `plex.fts_query(text)` turns an FTS4 query into a `tsquery`: enclosing
  parentheses stripped, `"quoted words"` a `<->` phrase, terms split on
  whitespace and tokenised by the `simple` parser (so punctuation never
  breaks a query), `term*` a prefix match, `OR` is `|` and binds tighter
  than the implicit AND (as in FTS4), a leading `-` negates, and an empty
  query matches nothing.
- A `col MATCH term` on one of the views is written
  `to_tsvector('simple', plex.fold(col)) @@ plex.fts_query(term)`, which the
  planner serves from the GIN expression indexes on `metadata_items.title`,
  `title_sort`, `original_title` and `tags.tag`. The expression has to be
  exactly that (`'simple'`, `plex.fold`) to match the index.

### spellfix

- `plex.spellfix_suggest(term text, n int DEFAULT 5)` returns `(word,
  distance, score)`, spellfix1's shape, for the `n` words of
  `plex.spellfix_vocab` (the folded words of every `metadata_items.title`
  and `tags.tag`, kept by triggers) closest to `term` by trigram
  similarity. `distance` and `score` are `100 * (1 - similarity)`.
- Words are only ever added to the vocabulary; a deleted title's words
  stay.

### Collations

- `plex.nocase`: ICU root at strength 2, non-deterministic. `tags.tag`,
  `metadata_items.title_sort` and `library_sections.name_sort` carry it,
  as stock Plex declares them `COLLATE NOCASE`, so `=`, `IN`, `GROUP BY`,
  `DISTINCT` and btree lookups on them are case-insensitive.
- `plex.icu_root`: ICU root, deterministic, for `ORDER BY title_sort
  COLLATE icu_root` -- emit `COLLATE plex.icu_root` rather than stripping
  the clause.
- **The rule for those three columns:** PostgreSQL 15 refuses `LIKE`,
  `ILIKE`, `~`, `strpos()` and the trigram operators on a non-deterministic
  collation (`nondeterministic collations are not supported for LIKE`). A
  pattern match or substring search against `tags.tag`, `title_sort` or
  `name_sort` must compare through `plex.fold(col)` (deterministic, folded
  on both sides: `plex.fold(tags.tag) LIKE plex.fold(?)`), or re-collate
  explicitly (`tags.tag COLLATE plex.icu_root LIKE ?`). Plain `title`,
  `studio`, `summary` and every other text column are unaffected. No
  statement in the live corpus pattern-matches one of the three.

### typeof

- `plex.sqlite_typeof(anyelement)` answers SQLite's `typeof()` names
  (`null`, `integer`, `real`, `text`, `blob`).

## Engineering Rule
- New unsupported syntax should be documented here when it is discovered in the field or added as a failing compatibility test.
- When a gap becomes supported, remove it here and add coverage in `docs/TRANSLATOR_COVERAGE.md`.
