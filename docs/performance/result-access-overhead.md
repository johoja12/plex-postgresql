# Full-catalogue result access overhead

## Reproduction and measurements

Use the same Plex server binary and frozen SQLite snapshot in isolated native
SQLite and PostgreSQL instances. Import with the migration scripts, verify table
counts, and issue full episode/movie catalogue requests over localhost. Measure
HTTP through complete response receipt, separately from XML parsing. Alternate
engine order, use five runs each, and compare counts and unique item IDs.

On a QNAP test host, the final candidate medians were:

| Workload | Rows | Native SQLite | PostgreSQL shim | Overhead |
|---|---:|---:|---:|---:|
| Full episodes | 18,049 | 8.0668 s | 8.2595 s | 2.39% |
| Full movies | 3,166 | 1.5745 s | 1.6974 s | 7.81% |

The previous shim baseline was 37.1330 s / 6.0362 s. The earlier native baseline
was 7.8493 s / 1.7507 s; use contemporary controls because host load changes.
These are workload-specific medians, not per-request latency guarantees.
The measured runtime includes the separate Unicode, FTS and grouping fixes
submitted in #7–#11. This branch includes prerequisites from #6 and #11 but no
NAS entrypoint, scanner wrapper, migration-script or Plex binary changes.

## Causes addressed

- Repeated schema lookup and recursive type/decltype accessor calls per value.
- Per-cell formatted query copies and global diagnostic history locks.
- Value copying/parsing in column_type that cannot affect its answer.
- Connection-pool and parameter work before every materialized row advance.
- Repeated registry lookups for the same statement.
- Fixed-size copying of ordinary text and an inline scalar buffer whose pointer
  could become invalid when its containing struct moved.
- Executing a full query for field metadata, discarding it, then executing again
  for sqlite3_step. Two equivalent ~1.06 s executions were observed. Prepared
  statement description obtains names/OIDs/origin tables without retrieving rows.
- Redundant numeric scans, naive substring search and per-NULL environment scans.

## Safety boundaries

Descriptors invalidate on result release/replacement, SQL identity and schema
publication. The registry cache retains one owning reference per thread and
checks a registration generation; address reuse and cross-thread unregister
have tests. Borrowed live text remains owned by PGresult; transformed text is
owned per column until step/reset. The existing query-cache text path is unchanged.

Default column breadcrumbs retain thread-local statement/column/phase context.
`PLEX_PG_TRACE_COLUMN_PHASES=1` enables full per-cell query history; DEBUG and
bad-cast tracing retain detailed column diagnostics. The NULL type override is a
process-start diagnostic setting and requires restart after an environment change.

Validation includes NULL/empty and metadata-only types, cached NULL decltypes,
schema invalidation, aggregate aliases, long Unicode and transformed strings,
text/bytes lifetime, registry reuse/unregister, eager EOF and streaming fallback.
All full-catalogue counts and ID hashes match. Candidate item XML matches the
previous shim exactly. Existing SQLite differences remain: media-version order
can change which duration Plex displays, and some accent/punctuation searches
return different results. This performance change does not claim to fix those.

Integration unit suite: 792 tests passed. Clean upstream branch: 785 tests passed.
A separate native SQLite aggregate-decltype comparison also passed.

Concurrent browse check: full 18,049-episode response in 8.1841 s while a
50-item page completed in 0.2887 s, both HTTP 200. A prior five-run candidate
series independently measured 4.85% episode and 2.97% movie overhead.
