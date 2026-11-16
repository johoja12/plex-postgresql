# The upgrade rig

Runs two Plex Media Server releases side by side on one shim build and two
copies of one database, sends both the same requests, and compares what they
answered and what they asked PostgreSQL. A stock server of the new release,
with no shim, is the reference for what Plex itself does. Built for the
1.43.5.11029 to 1.43.5.11079 upgrade (2026-11-16), which is what the numbers
below are from.

Nothing here is claimed with plex.tv: the preferences `rig.sh` writes carry no
account, and an unclaimed server answers every call without a token.

## Running it

```sh
# 1. One shim build, two Plex releases.
docker build -f Dockerfile.standalone -t pgrig:new .
docker build -f Dockerfile.standalone --build-arg PLEX_VERSION=<old> -t pgrig:old .

# 2. A database to copy: a docker volume holding a PostgreSQL 15 data directory,
#    best a restored pg_dump of the live one. Unset, each side starts empty.
export RIG_DB_VOLUME=<volume> RIG_MEDIA=<a directory with movies/ and tv/>

# 3. Both sides, the reference and a tuner stub.
scripts/upgrade-rig/rig.sh db A && scripts/upgrade-rig/rig.sh db B
scripts/upgrade-rig/rig.sh plex A pgrig:old && scripts/upgrade-rig/rig.sh plex B pgrig:new
scripts/upgrade-rig/rig.sh stock <new version>
scripts/upgrade-rig/rig.sh tuner <dir>      # serves <dir>/hdhr/x1/{discover,lineup}.json, xmltv.xml
for s in A B C; do scripts/upgrade-rig/rig.sh wait $s; done

# 4. The same mix on each, at the same time (the sides share nothing).
python3 scripts/upgrade-rig/mix.py http://127.0.0.1:8091 out/A read,write,scan,voice,livetv,butler,load &
python3 scripts/upgrade-rig/mix.py http://127.0.0.1:8092 out/B read,write,scan,voice,livetv,butler,load &
python3 scripts/upgrade-rig/mix.py http://127.0.0.1:8093 out/C scan,voice,livetv
wait

# 5. Compare.
python3 scripts/upgrade-rig/cmp.py out/A out/B
docker logs pgrigA-pg > pg-A.log 2>&1; docker logs pgrigB-pg > pg-B.log 2>&1
```

`RIG_MEDIA` wants `movies/<Title (Year)>/<Title (Year)>.mkv` and
`tv/<Show (Year)>/Season NN/<Show (Year)> - SNNENN.mkv`; a few seconds of
`ffmpeg -f lavfi -i testsrc2` each is enough, and titles Plex's agent knows
(the Blender films) exercise matching. The tuner stub is cluster-plex's
`hack/guiderig/xmltv-gen.py <dir>/hdhr/x1 http://tuner/hdhr/x1 --profile`.

## The phases

| Phase | What it does | Needs |
| --- | --- | --- |
| `read` | every section, hub, search, filter, sort and item route over the copied library, and the guides it already holds | a copied database |
| `write` | play progress, scrobble, rating, metadata edits and tags, playlists, play queues, collections (plain and smart), section and server preferences | a copied database with 40 movies and episodes |
| `scan` | two new sections over `RIG_MEDIA`, their scan, analysis and refresh, a play, optimize | `RIG_MEDIA` |
| `voice` | voice search over three titles that tell an edit distance's case rule apart, then manual match, unmatch, match | `scan` |
| `livetv` | registers the tuner stub, creates an XMLTV DVR, saves its channel map, reads the loaded guide, reloads it | the tuner |
| `butler` | starts the butler tasks one at a time | |
| `load` | eight clients reading and writing at once, then 200 `/library/sections` probes | a copied database |

## Reading the result

- `cmp.py` pairs the calls by label and prints what differs after masking
  what any two servers differ in (identifiers, versions, times, random hubs).
  What is left on a healthy pair is order among rows added in the same second
  and container addresses. If one side's first scan ran before its
  `tv.plex.provider.metadata` came online (PMS logs "we had 0 cloud providers
  online"), its items are unmatched and every later id differs: delete the two
  `Rig` sections on both sides and run `scan,voice` again.
- **PostgreSQL's log is the stronger test**: `grep -E '\[unknown\] (ERROR|FATAL)'`
  must be empty on both sides, and the statement shapes (literals and
  parameters replaced) only one side sent are the new release's new SQL.
  11079 had one, the System plug-in registering its prefixes again.
- Against the stock server, compare by content, not by row id: titles, guids,
  durations, media and streams of the scanned items, the voice search's
  `distance`s, the Live TV phase's statuses and sizes.
- Each side's `Plex Media Server.log`: "error reading match results",
  "Unexpected result when reading /proc", "Uncaught exception" and
  "Exception inside transaction" must not appear; a stock server shows none.
- PMS downloads its H.264 decoder at the first analysis; on a slow link its
  own 30 s limit ends the first jobs ("exit code ... is -9"), which is no
  fault of the shim.
