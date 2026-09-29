# Tag search and statement-history performance

## Changes

Translated tag MATCH queries evaluate `to_tsvector('simple', tag)`. The existing
GIN index on `tags.search_vector` cannot serve that expression. Add a GIN index
on the exact expression, preserving source-column search semantics.

Statement diagnostic history keeps its existing capacities (2,048 finalized and
4,096 prepared statements). A hash index replaces linear scans of the bounded
rings. Entries and address mappings share the same mutex; address reuse removes
the old slot and eviction removes its mapping. Storage remains heap allocated.
Tests cover bounded eviction, address reuse, removal, and clearing.

The per-query eager-fetch status message moves from INFO to DEBUG. Error and
fallback logging and streaming configuration are unchanged.

## Existing databases

Before upgrading a busy database, run the following outside a transaction:

```sh
psql -v ON_ERROR_STOP=1 "$DATABASE_URL" -f scripts/search-expression-index.sql
```

The script creates the index concurrently, analyzes tags, and reports index
validity/readiness. Both flags must be true. If a concurrent build fails, inspect
and remove any invalid index before retrying: `IF NOT EXISTS` does not repair an
invalid index. Adjust the script's timeouts for larger databases as needed.
Keep `ANALYZE`: expression statistics matter for consistent query planning.

The initial schema includes the index. Compatibility SQL creates it if tags
exists, using a regular index build; without the concurrent pre-build this can
block writers while building. The additive index remains compatible with older
shim versions. This change does not replace the FTS view fixes in PR #8 or boolean
MATCH translation fixes in PR #10.

## Measurements (2026-09-28)

A representative translated tag query evaluated its text vector 353,856 times,
touched roughly 1.43 million shared buffers and took 1,771.708 ms. With the index
and refreshed statistics it used a bitmap index scan, 19 buffers, and 0.304 ms.
This specific query returned zero rows in both cases.

On the same isolated restored database, three serial searches for `Resident Evil`
with limit 5 and movie/TV/music/other-video search types measured:

| Version | Runs (seconds) | Median |
| --- | --- | --- |
| Before | 4.4925 / 4.4356 / 4.3684 | 4.4356 s |
| Candidate | 0.8192 / 0.4636 / 0.4545 | 0.4636 s |

The candidate median was 89.5% lower (9.57x faster); result IDs and response bytes
matched, with five positive results. An index-only intermediate build measured
0.5674 / 0.5624 / 0.5294 s, indicating the index accounts for most of the benefit.
The statement-history change's independent HTTP benefit was not isolated.
A baseline profile attributed 3.5% of sampled CPU to finalized-history lookup;
this is not a claim of measured total CPU reduction.

Watched-state writes were already fast: nine live scrobble requests took 12-55 ms
(median 15 ms). Isolated watched/unwatched tests verified persisted database state,
with serial candidate requests around 5-13 ms. Production watch history was not
mutated by tests. Separate 30-second metadata delays were associated with media
read failures during scanner analysis and are not resolved by this change.

The composite integration build passed 797 library tests, full catalogue ID
checks, and 80 metadata requests across five scanner refresh rounds. Following
deployment, three searches measured 0.8911 / 0.5129 / 0.5112 s with matching IDs.
These runtime measurements used a patched Plex 1.43.4.10903 integration image
containing earlier fixes (including pending upstream PRs); they do not establish
that a stock upstream image was tested. This PR is based directly on upstream
main and only contains the index, statement-history, and logging changes.
