# Smart TV collection hub delay: 2026-10-03

Favourite TV (`custom.collection.2.40972`) computed in 5,826–5,978 ms in the
FluxPlay incident. The final p22 release computes it in 116–185 ms.

Plex joined shows to every episode, paged those duplicate show IDs, and
removed duplicates after paging. A captured 19-show request ran 28 membership
queries (LIMIT 21, OFFSET 0 through 567), each taking 168–182 ms. Each also
counted every unwatched leaf with a correlated COUNT(*) >= 1, triggering JIT.

The exact owner/section/filter query now uses EXISTS for unwatched membership
and groups each show before paging. Ordering preserves each show's first
occurrence in descending episode order, including NULL dates using bool_or
and CASE. Other query shapes fail closed. Collection definitions are unchanged.

Validation:
- All 807 Rust tests passed; PostgreSQL regression fixture covers duplicate
  episodes, mixed/all NULL dates, sorting and page boundaries.
- Isolated COUNT/EXISTS queries returned exactly the same 4,305 IDs, including
  duplicates and order (98 shows), at 189 ms versus 31 ms.
- Grouped query preserved the exact unique order and took 26 ms per first page.
- Fresh pg_dump restored to a private unclaimed trial with separate identity.
  Restored collection URI alone was adjusted to that trial identity.
- Full catalogues matched database IDs; concurrent paging, ASCII/Unicode
  searches, scanner match shim mapping and two 64 KiB range reads passed.
- Live p22 counts 12/21/22/23 computed collection hubs in 116/146/185/165 ms.
  Their IDs/order and all 98 collection members exactly matched p21 before
  cutover. The complete home request still took 1.99–3.03 s constructing its
  other hubs during these checks.
- Production catalogue IDs matched live PostgreSQL despite active scanner
  updates. Direct/public browsing, scanner match and media reads passed.
- Ordinary result cells stayed about 30–34 ns across SQL lengths 64 to 700,000.
- Trial maintenance surfaced an existing unaliased DELETE FROM locations
  subquery; these narrowly matched SELECT rewrites do not affect that query.

Release: plex-postgresql:1.43.4.10903-tvhub-p22, 08:29:44 UTC.
Image: sha256:a17f782d218fcfd6ca71d0d749dcf46ae2404a0d06d0a0227f7e39d0432696c0.
Shim: 30f3ea236aa18011f6901362f5cf910b2b9ab33e4d72e2103863160a0fd8da7c.
Patched Plex binary unchanged:
f73391e8dbc3ce22c077fb65d5427854855dedfb2f43dcd1bf3b80e317eca8b3.
Only Plex recreated; environment, mounts, GPU runtime and ports unchanged.
p20/p21 images and protected Compose backups retained for rollback.
Private artifacts: /tmp/plex-tvhub-fix-20261003/ and its p22 directory.
Backup: /share/CACHEDEV1_DATA/Container/plex-postgresql-backups/plex-tvhub-fix-20261003.dump.

A final public request with FluxPlay language en computed the collection in
108 ms; the entire home response took 1.463 s. Inventory hashes matched the
deployed Synology/QNAP files. Private trial containers were stopped.
Background TV scanning/ViewStateSync exception warnings existed in the p20
incident logs and remain separate from the fixed collection query. Scanner
match acceptance is narrower than proving every background scan succeeds.
