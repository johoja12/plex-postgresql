# Large Plex catalogue reads: result conversion cost

A private trial restored a production PostgreSQL 15 dump and ran the same Plex
1.43.4.10903 image/shim as production, with external connectivity disabled.
Streaming was disabled for the initial comparison, so both measurements use the
same eager libpq result path. No database migration or query changes are needed.

## Root cause

A 10-second, 99 Hz `perf record --call-graph dwarf` sample while Plex converted a
large episode result attributed 39.60% of CPU samples to `strlen`, 21.59% to
`rust_decltype_cache_lookup_alias`, and 16.19% to UTF-8 validation. PostgreSQL
connections were idle during this work.

* The collection metadata compatibility predicate decoded the complete SQL string
  before testing whether the cell was even metadata_type=18. Plex's long IN lists
  made this repeated work scale with SQL length times cells read.
* Aliased or unknown column names repeatedly scanned every schema type key.
  Type consistency validation invokes this during ordinary column access.

## Changes

Check the numeric value and column name before inspecting SQL for the special
collection rule. Cache alias resolutions, including misses, under the existing
schema cache lock. New schema entries invalidate alias resolutions. Bound the
alias map to 4096 entries. Never replace published type CStrings, since Plex can
retain pointers to their contents. Preserve longest matching schema key behavior.

These changes preserve SQLite compatibility rules and apply to both eager and
streamed results. They do not impose an API pagination requirement or drop rows.

## Regression coverage

Helper tests cover collection masking with a 700 KB SQL string, ordinary values,
non-metadata columns, null and invalid UTF-8 input; alias hits, misses, schema
insertion invalidation, longer matches, bounded caching and pointer stability.
Run `cargo test --manifest-path rust/plex-pg-core/Cargo.toml --lib --features interpose`.

For integration acceptance, compare authenticated full movie and episode API
responses on the same restored database: status, elapsed time, item count,
unique rating keys and a SHA-256 of sorted rating keys. Check a paged request
while the full request runs. Record eager and streaming modes separately.

## Baseline (isolated trial)

* Full movie listing: 3166 unique items, 8,025,720 bytes, 14.858 seconds.
* Full episode listing: exceeded the 120-second client timeout (18107 expected).
* Episode page with both Container-Start=0 and Container-Size=50: 50 unique items,
  totalSize=18107, 1.625 seconds.
* 766 Rust library tests passed with the fix.

A Size parameter alone did not paginate this Plex endpoint; both pagination
parameters are required for the page comparison. Full requests are intentional
for the performance acceptance test.

## Fixed shim (same trial)

* Eager full movies: 4.080 seconds, byte-for-byte identical baseline XML.
* Eager full episodes: 31.411 seconds; repeat under concurrent local build load:
  50.303 seconds. Both returned 18107 unique IDs matching PostgreSQL exactly.
* Concurrent eager episode page: 0.787 seconds, same 50 IDs as baseline.
* Streaming full movies: 5.257 seconds; full episodes: 44.984 seconds. Same
  counts and sorted-ID checksums as eager mode.
* Concurrent streaming episode page: 1.312 seconds, same IDs as baseline.

Keep the existing eager production setting for this release; the isolated
comparison proves the helpers benefit both modes, but does not establish an
advantage for streaming under all production callers. These timings are
observations, not controlled latency bounds: local compilation ran during some
requests. The full 46 MB episode response still has substantial serialization
and per-cell compatibility overhead.

`cargo bench --manifest-path rust/plex-pg-core/Cargo.toml --bench bench_result_hotpath`
benchmarks ordinary-cell checks across 64/64000/700000-byte SQL and memoized type
aliases across 10/100/1000 schema entries. It needs no live database or preload.
Do not enable the interpose feature for this benchmark: unrelated standalone
compatibility harness binaries have pre-existing interpose linking conflicts.
