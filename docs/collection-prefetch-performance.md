# Collection prefetch and availability latency

## Cause

Plex collection-item move requests held a metadata transaction while fetching
collection members. The owner-account prefetch query uses SELECT DISTINCT over
wide media/metadata rows, with unused LEFT JOINs to taggings and tags. On an
isolated 334-member collection these joins expanded 501 result rows into 51,742
rows before DISTINCT. EXPLAIN ANALYZE showed an external merge sort using
163,480 KiB and total execution time of 7,805.933 ms.

Live Plex logs linked slow authenticated GET / requests to device-statistics
transactions waiting on those collection moves. The moves took 7.795/8.218 s;
23 FluxPlay root requests in the observed burst had median 3.1 s and maximum
7.341 s. Queued requests completed together when the transaction was released.

## Rewrite and scope

Remove the two unused tag LEFT JOINs only for the observed plain DISTINCT
projection, literal metadata ID list, owner account 1, and fully qualified column
ordering. Retain DISTINCT and all other joins. Full canonical query-shape checking
rejects additional clauses, wildcards, aggregates, functions, subqueries, aliases
on tables, other accounts and references to tag columns. This is deliberately a
narrow optimization, not a general join optimizer. No schema change is needed.

## Measurements (2026-09-29)

| Workload | Before | Candidate |
| --- | ---: | ---: |
| SQL execution, EXPLAIN ANALYZE | 7,805.933 ms | 132.012 ms |
| SQL result transfer included | 7.934 s | 0.420 s |
| Collection children HTTP | 7.865 s | 0.273 s |
| Three collection moves HTTP | 8.027 / 8.047 / 7.991 s | 0.300 / 0.541 / 0.250 s |

The original and rewritten queries returned byte-for-byte identical
3,098,895-byte COPY output, SHA-256:
78b7bb36c4704f2a664de235baf20ab415425caaf4eb61e1d41a9bb2dc52ded7.
The rewritten plan sorts only 501 rows.

After deployment, eight concurrent public root checks returned HTTP 200 in
16-36 ms during collection reads taking 316-383 ms. The isolated unauthenticated
root checks did not reproduce the authenticated device-statistics lock wait;
the live request/transaction logs establish that relationship. Production
collection ordering was not mutated by acceptance tests.

Runtime measurements used a composite patched Plex 1.43.4.10903 image with earlier
shim fixes, including pending upstream PRs. They do not establish that a stock
upstream image was tested. This PR is based directly on upstream main and does
not include those other changes. Integration validation passed 799 library tests,
full catalogue ID comparisons, scanner execution, positive ASCII/Unicode search,
and media range reads. Branch-specific test results are recorded in the PR.

## Remaining limitations

The observation window is bounded, not a long-term latency guarantee. Separate
native-scanner file-hash reads also held the metadata lock during failed media
reads lasting 85-116 seconds. This rewrite does not resolve missing media or
bound scanner I/O. Existing startup collection-fixup and media-provider errors
were also present before this change and are not claimed resolved.
