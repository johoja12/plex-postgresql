#!/usr/bin/env python3
"""mix.py <base URL> <out dir> <phase>[,<phase>...]

The upgrade rig's request mix: the same calls against two Plex servers on one
shim build and two copies of one database. Every answer is written under
<out dir>/<phase>/<nnnn>.json with its request, status and body; cmp.py reads
two such trees. Phases: read, write, scan, voice, livetv, butler, load
(README.md says what each does and what it needs).
"""
import json, os, sys, time, threading, urllib.request, urllib.parse, urllib.error

BASE = sys.argv[1].rstrip("/")
OUT = sys.argv[2]
PHASES = sys.argv[3].split(",")
HDRS = {
    "Accept": "application/json",
    "X-Plex-Client-Identifier": "pgrig-harness-0001",
    "X-Plex-Product": "pgrig",
    "X-Plex-Version": "1.0",
    "X-Plex-Platform": "Linux",
    "X-Plex-Device": "rig",
    "X-Plex-Device-Name": "pgrig harness",
    "X-Plex-Session-Identifier": "pgrig-session-0001",
}
seq = 0
phase = "init"
lock = threading.Lock()


def call(method, path, label=None, record=True, timeout=120, headers=None):
    global seq
    url = BASE + path
    req = urllib.request.Request(url, method=method, headers={**HDRS, **(headers or {})})
    if method in ("POST", "PUT"):
        req.data = b""
    t0 = time.time()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            status, body = r.status, r.read()
    except urllib.error.HTTPError as e:
        status, body = e.code, e.read()
    except Exception as e:  # noqa: BLE001 - the rig records every failure
        status, body = 0, repr(e).encode()
    dt = time.time() - t0
    try:
        parsed = json.loads(body) if body else None
        text = None
    except Exception:  # noqa: BLE001
        parsed, text = None, body[:4000].decode("utf-8", "replace")
    if record:
        with lock:
            seq += 1
            n = seq
        d = os.path.join(OUT, phase)
        os.makedirs(d, exist_ok=True)
        with open(os.path.join(d, "%04d.json" % n), "w") as f:
            json.dump({"n": n, "label": label or path, "method": method, "path": path,
                       "status": status, "seconds": round(dt, 3), "json": parsed, "text": text}, f)
    return status, parsed


def mc(parsed):
    return (parsed or {}).get("MediaContainer", {}) if isinstance(parsed, dict) else {}


def q(**kw):
    return urllib.parse.urlencode(kw, quote_via=urllib.parse.quote)


def wait_idle(what="", limit=900):
    """Waits until /activities lists nothing, at most limit seconds."""
    t0 = time.time()
    quiet = 0
    while time.time() - t0 < limit:
        s, p = call("GET", "/activities", record=False)
        acts = mc(p).get("Activity", []) if s == 200 else [1]
        quiet = quiet + 1 if not acts else 0
        if quiet >= 3:
            return True
        time.sleep(2)
    print("  wait_idle(%s): still busy after %ds" % (what, limit), flush=True)
    return False


def sections():
    s, p = call("GET", "/library/sections", record=False)
    return mc(p).get("Directory", [])


# ---------------------------------------------------------------- read
def phase_read():
    for p in ["/identity", "/", "/library", "/library/sections", "/library/sections/all",
              "/library/recentlyAdded", "/library/onDeck", "/hubs", "/hubs/continueWatching",
              "/hubs/promoted", "/playlists", "/playlists?playlistType=video",
              "/status/sessions", "/status/sessions/history/all",
              "/status/sessions/history/all?sort=viewedAt:desc&X-Plex-Container-Start=0&X-Plex-Container-Size=50",
              "/statistics/media?timespan=4", "/statistics/media?timespan=6", "/statistics/bandwidth?timespan=6",
              "/statistics/resources?timespan=6", "/butler", "/activities", "/accounts", "/devices",
              "/media/providers", "/livetv/dvrs", "/livetv/sessions", "/media/grabbers", "/media/grabbers/devices",
              "/media/subscriptions", "/media/subscriptions/scheduled", "/library/collections",
              "/library/all?type=1&X-Plex-Container-Start=0&X-Plex-Container-Size=30",
              "/library/all?type=2&X-Plex-Container-Start=0&X-Plex-Container-Size=30",
              "/library/all?type=4&sort=addedAt:desc&X-Plex-Container-Start=0&X-Plex-Container-Size=30",
              "/library/all?type=18", "/library/tags?type=1", "/library/tags?type=6", "/library/metadata/augmentations",
              "/sync/items", "/system/agents", "/:/prefs", "/security/resources", "/servers",
              "/transcode/sessions", "/updater/status", "/myplex/account", "/library/streams/1",
              "/library/sections/prefs?type=1&agent=tv.plex.agents.movie",
              "/hubs/home/recentlyAdded?type=1", "/hubs/home/recentlyAdded?type=2",
              "/hubs/home/onDeck", "/library/parts/1"]:
        call("GET", p)
    for term in ["the", "star", "dragon", "a", "one piece", "sopranos", "akira", "o'brien", "café", "100%"]:
        e = urllib.parse.quote(term)
        call("GET", "/hubs/search?query=%s&limit=10" % e)
        call("GET", "/hubs/search?query=%s&limit=10&includeCollections=1&includeExternalMedia=0" % e)
        call("GET", "/search?query=%s" % e)
        call("GET", "/library/search?query=%s&limit=20&searchTypes=movies,tv" % e)
        call("GET", "/hubs/search/voice?query=%s" % e)
    for sec in sections():
        k, typ = sec["key"], sec["type"]
        main = {"movie": 1, "show": 2, "artist": 8}.get(typ, 1)
        b = "/library/sections/%s" % k
        for p in ["", "/all?X-Plex-Container-Start=0&X-Plex-Container-Size=50",
                  "/all?type=%d&sort=titleSort&X-Plex-Container-Start=0&X-Plex-Container-Size=50" % main,
                  "/all?type=%d&sort=titleSort:desc&X-Plex-Container-Start=100&X-Plex-Container-Size=50" % main,
                  "/all?type=%d&sort=addedAt:desc&X-Plex-Container-Start=0&X-Plex-Container-Size=50" % main,
                  "/all?type=%d&sort=originallyAvailableAt:desc&X-Plex-Container-Start=0&X-Plex-Container-Size=50" % main,
                  "/all?type=%d&sort=rating:desc&X-Plex-Container-Start=0&X-Plex-Container-Size=50" % main,
                  "/all?type=%d&sort=audienceRating:desc&X-Plex-Container-Start=0&X-Plex-Container-Size=50" % main,
                  "/all?type=%d&sort=duration&X-Plex-Container-Start=0&X-Plex-Container-Size=50" % main,
                  "/all?type=%d&sort=lastViewedAt:desc&X-Plex-Container-Start=0&X-Plex-Container-Size=50" % main,
                  "/all?type=%d&sort=viewCount:desc&X-Plex-Container-Start=0&X-Plex-Container-Size=50" % main,
                  "/all?type=%d&sort=random&X-Plex-Container-Start=0&X-Plex-Container-Size=5" % main,
                  "/all?type=%d&unwatched=1&X-Plex-Container-Start=0&X-Plex-Container-Size=50" % main,
                  "/all?type=%d&year=2019&sort=titleSort" % main,
                  "/all?type=%d&year>>=2020&sort=titleSort&X-Plex-Container-Size=50&X-Plex-Container-Start=0" % main,
                  "/all?type=%d&title=the&sort=titleSort" % main,
                  "/all?type=%d&title<=the&sort=titleSort&X-Plex-Container-Size=50&X-Plex-Container-Start=0" % main,
                  "/all?type=%d&contentRating=PG-13&sort=titleSort" % main,
                  "/all?type=%d&resolution=1080&sort=titleSort&X-Plex-Container-Size=50&X-Plex-Container-Start=0" % main,
                  "/all?type=%d&audioLanguage=en&sort=titleSort&X-Plex-Container-Size=50&X-Plex-Container-Start=0" % main,
                  "/all?type=%d&hdr=1&sort=titleSort&X-Plex-Container-Size=50&X-Plex-Container-Start=0" % main,
                  "/all?type=%d&duplicate=1" % main,
                  "/all?type=%d&inProgress=1" % main,
                  "/all?type=%d&addedAt>>=-90d&sort=addedAt:desc&X-Plex-Container-Size=50&X-Plex-Container-Start=0" % main,
                  "/all?type=%d&push=1&genre=Action&or=1&genre=Comedy&pop=1&year>>=2000&sort=titleSort&X-Plex-Container-Size=50&X-Plex-Container-Start=0" % main,
                  "/all?type=%d&includeCollections=1&X-Plex-Container-Start=0&X-Plex-Container-Size=50" % main,
                  "/all?type=%d&includeMeta=1&includeAdvanced=1&X-Plex-Container-Start=0&X-Plex-Container-Size=5" % main,
                  "/all?type=%d&includeGuids=1&X-Plex-Container-Start=0&X-Plex-Container-Size=50" % main,
                  "/recentlyAdded", "/recentlyViewed", "/onDeck", "/newest", "/unwatched", "/firstCharacter",
                  "/firstCharacter?type=%d" % main, "/firstCharacter/T", "/genre", "/year", "/decade", "/contentRating",
                  "/collection", "/collections", "/collections?includeCollections=1", "/director", "/actor",
                  "/country", "/studio", "/resolution", "/rating", "/filters", "/sorts", "/prefs",
                  "/filterTypes", "/categories", "/cluster?type=%d" % main, "/common?type=%d" % main,
                  "/search?type=%d&query=the" % main, "/albums", "/byFolder", "/folder",
                  "/all?type=18", "/all?type=15", "/all?type=42",
                  "/computePath?type=%d" % main, "/agents"]:
            call("GET", b + p)
        call("GET", "/hubs/sections/%s" % k)
        call("GET", "/hubs/sections/%s?count=12&includeMeta=1" % k)
        call("GET", "/hubs/sections/%s/manage" % k)
        call("GET", "/hubs/promoted?contentDirectoryID=%s" % k)
        if typ == "show":
            for p in ["/all?type=4&sort=addedAt:desc&X-Plex-Container-Start=0&X-Plex-Container-Size=50",
                      "/all?type=3&X-Plex-Container-Start=0&X-Plex-Container-Size=50",
                      "/all?type=4&episode.unwatched=1&X-Plex-Container-Start=0&X-Plex-Container-Size=50",
                      "/all?type=2&show.unwatchedLeaves=1&X-Plex-Container-Start=0&X-Plex-Container-Size=50",
                      "/all?type=4&episode.audioLanguage=ja&X-Plex-Container-Start=0&X-Plex-Container-Size=50",
                      "/all?type=2&network=HBO", "/all?type=4&sort=episode.originallyAvailableAt:desc&X-Plex-Container-Start=0&X-Plex-Container-Size=50"]:
                call("GET", b + p)
        # a deterministic sample of items, through every item route
        s, p = call("GET", b + "/all?type=%d&sort=titleSort&X-Plex-Container-Start=0&X-Plex-Container-Size=400" % main, record=False)
        items = mc(p).get("Metadata", [])
        for it in items[::max(1, len(items) // 12)][:12]:
            rk = it["ratingKey"]
            m = "/library/metadata/%s" % rk
            for p in ["", "?includeChildren=1&includeGuids=1&includeMarkers=1&includeChapters=1&includeRelated=1&includeExternalMedia=1&includeExtras=0&includeReviews=1&includeOnDeck=1&includePreferences=1&asyncAugmentMetadata=0",
                      "/children", "/related", "/similar", "/posters", "/arts", "/hubs", "/collections", "/matches?manual=1&title=x&agent=none"]:
                call("GET", m + p)
            call("GET", "/hubs/metadata/%s/related" % rk)
            call("GET", "/hubs/metadata/%s/postplay" % rk)
            if typ == "show":
                call("GET", m + "/allLeaves")
                call("GET", m + "/grandchildren")
                call("GET", m + "/onDeck")
                s2, p2 = call("GET", m + "/children", record=False)
                for sea in mc(p2).get("Metadata", [])[:2]:
                    call("GET", "/library/metadata/%s" % sea["ratingKey"])
                    s3, p3 = call("GET", "/library/metadata/%s/children" % sea["ratingKey"])
                    for ep in mc(p3).get("Metadata", [])[:2]:
                        call("GET", "/library/metadata/%s?includeMarkers=1&includeChapters=1" % ep["ratingKey"])
                        for med in ep.get("Media", [])[:1]:
                            for part in med.get("Part", [])[:1]:
                                call("GET", "/library/parts/%s" % part["id"])
    # collections and playlists that exist
    s, p = call("GET", "/library/collections", record=False)
    for c in mc(p).get("Metadata", [])[:6]:
        call("GET", "/library/collections/%s" % c["ratingKey"])
        call("GET", "/library/collections/%s/children" % c["ratingKey"])
    # the guides the database already holds
    s, p = call("GET", "/livetv/dvrs", record=False)
    for d in mc(p).get("Dvr", []):
        epg = "/" + d.get("epgIdentifier", "")
        call("GET", "/livetv/dvrs/%s" % d["key"])
        for p2 in ["", "/sections", "/hubs/discover", "/hubs/discover?promoted=1", "/hubs/sections/all",
                   "/lineups/dvr/channels", "/grid?type=1&sort=beginsAt&endsAt>=0&beginsAt<=9999999999&X-Plex-Container-Start=0&X-Plex-Container-Size=40",
                   "/grid?type=4&X-Plex-Container-Start=0&X-Plex-Container-Size=40",
                   "/sections/1/all?type=1&X-Plex-Container-Start=0&X-Plex-Container-Size=40",
                   "/sections/2/all?type=2&X-Plex-Container-Start=0&X-Plex-Container-Size=40",
                   "/sections/1/all?type=4&X-Plex-Container-Start=0&X-Plex-Container-Size=40",
                   "/hubs/search?query=the&limit=10", "/sections/1/genre", "/sections/1/all?type=1&sort=titleSort&X-Plex-Container-Start=0&X-Plex-Container-Size=40"]:
            call("GET", epg + p2)
    call("GET", "/livetv/epg/countries")
    call("GET", "/livetv/epg/languages")


# ---------------------------------------------------------------- write
def phase_write():
    secs = sections()
    movie = next((s for s in secs if s["type"] == "movie"), None)
    show = next((s for s in secs if s["type"] == "show"), None)
    picks = {}
    if movie:
        s, p = call("GET", "/library/sections/%s/all?type=1&sort=titleSort&X-Plex-Container-Start=0&X-Plex-Container-Size=40" % movie["key"], record=False)
        picks["movies"] = mc(p).get("Metadata", [])[:40]
    if show:
        s, p = call("GET", "/library/sections/%s/all?type=4&sort=titleSort&X-Plex-Container-Start=0&X-Plex-Container-Size=40" % show["key"], record=False)
        picks["eps"] = mc(p).get("Metadata", [])[:40]
        s, p = call("GET", "/library/sections/%s/all?type=2&sort=titleSort&X-Plex-Container-Start=0&X-Plex-Container-Size=10" % show["key"], record=False)
        picks["shows"] = mc(p).get("Metadata", [])[:10]
    mv = picks.get("movies", [])
    eps = picks.get("eps", [])
    s, p = call("GET", "/identity", record=False)
    mid = mc(p).get("machineIdentifier", "")

    # playback: timeline progress, stop, resume, finish
    for it in (mv[:3] + eps[:3]):
        rk, dur = it["ratingKey"], int(it.get("duration", 1200000))
        base = "/:/timeline?" + q(ratingKey=rk, key="/library/metadata/%s" % rk, identifier="com.plexapp.plugins.library", duration=dur, hasMDE=1)
        call("GET", base + "&state=playing&time=5000", label="timeline playing 5s")
        call("GET", base + "&state=playing&time=%d" % (dur // 4), label="timeline playing 25%")
        call("GET", base + "&state=paused&time=%d" % (dur // 3), label="timeline paused")
        call("GET", base + "&state=stopped&time=%d" % (dur // 3), label="timeline stopped 33%")
        call("GET", "/library/metadata/%s" % rk, label="item after partial play")
    call("GET", "/library/onDeck", label="onDeck after plays")
    call("GET", "/hubs/continueWatching", label="continueWatching after plays")
    call("GET", "/status/sessions/history/all?sort=viewedAt:desc", label="history after plays")
    for it in (mv[:2] + eps[:2]):
        rk, dur = it["ratingKey"], int(it.get("duration", 1200000))
        base = "/:/timeline?" + q(ratingKey=rk, key="/library/metadata/%s" % rk, identifier="com.plexapp.plugins.library", duration=dur, hasMDE=1)
        call("GET", base + "&state=playing&time=%d" % (dur - 30000), label="timeline playing near end")
        call("GET", base + "&state=stopped&time=%d" % (dur - 1000), label="timeline stopped at end")
        call("GET", "/library/metadata/%s" % rk, label="item after full play")
    for it in mv[3:6] + eps[3:6]:
        rk = it["ratingKey"]
        call("GET", "/:/scrobble?" + q(key=rk, identifier="com.plexapp.plugins.library"), label="scrobble")
        call("GET", "/library/metadata/%s" % rk, label="item after scrobble")
    for it in mv[3:4] + eps[3:4]:
        rk = it["ratingKey"]
        call("GET", "/:/unscrobble?" + q(key=rk, identifier="com.plexapp.plugins.library"), label="unscrobble")
        call("GET", "/library/metadata/%s" % rk, label="item after unscrobble")
    for it in mv[6:9]:
        rk = it["ratingKey"]
        call("PUT", "/:/rate?" + q(key=rk, identifier="com.plexapp.plugins.library", rating=8), label="rate 8")
        call("PUT", "/:/progress?" + q(key=rk, identifier="com.plexapp.plugins.library", time=123000, state="stopped"), label="progress")
        call("GET", "/library/metadata/%s" % rk, label="item after rate")
    for sh in picks.get("shows", [])[:2]:
        rk = sh["ratingKey"]
        call("GET", "/:/scrobble?" + q(key=rk, identifier="com.plexapp.plugins.library"), label="scrobble show")
        call("GET", "/library/metadata/%s" % rk, label="show after scrobble")
        call("GET", "/library/metadata/%s/allLeaves" % rk, label="show leaves after scrobble")
        call("GET", "/:/unscrobble?" + q(key=rk, identifier="com.plexapp.plugins.library"), label="unscrobble show")
        call("GET", "/library/metadata/%s" % rk, label="show after unscrobble")
    call("GET", "/status/sessions/history/all?sort=viewedAt:desc", label="history after scrobbles")
    call("GET", "/statistics/media?timespan=4", label="statistics after plays")
    call("GET", "/statistics/media?timespan=6", label="statistics after plays 6")

    # metadata edits
    if mv and movie:
        rk = mv[9]["ratingKey"]
        e = "/library/sections/%s/all?type=1&id=%s&" % (movie["key"], rk)
        call("PUT", e + q(**{"title.value": "Rig Title éè 'quoted' \"double\" 100% _x_", "title.locked": 1, "summary.value": "A summary with 'quotes', a backslash \\ and emoji \U0001F600.", "summary.locked": 1}), label="edit title+summary")
        call("PUT", e + q(**{"titleSort.value": "Zzz Rig", "titleSort.locked": 1, "year.value": 1999, "contentRating.value": "PG", "studio.value": "Rig Studio", "originallyAvailableAt.value": "1999-03-31", "tagline.value": "tag", "userRating.value": 7}), label="edit sort/year/rating/studio")
        call("PUT", e + q(**{"genre[0].tag.tag": "Rig Genre", "genre[1].tag.tag": "Rig O'Genre", "label[0].tag.tag": "rig-label", "collection[0].tag.tag": "Rig Tag Collection", "director[0].tag.tag": "Rig Director", "country[0].tag.tag": "Rigland"}), label="edit add tags")
        call("GET", "/library/metadata/%s" % rk, label="item after edits")
        call("GET", "/library/sections/%s/all?type=1&genre=Rig%%20Genre" % movie["key"], label="filter by new genre (title)")
        call("GET", "/library/sections/%s/all?type=1&label=rig-label" % movie["key"], label="filter by new label")
        call("GET", "/library/sections/%s/genre" % movie["key"], label="genres after edit")
        call("GET", "/library/sections/%s/collections" % movie["key"], label="collections after tag")
        call("GET", "/hubs/search?query=Rig%20Title&limit=10", label="search edited title")
        call("PUT", e + q(**{"genre[].tag.tag-": "Rig Genre", "label[].tag.tag-": "rig-label", "collection[].tag.tag-": "Rig Tag Collection"}), label="edit remove tags")
        call("PUT", e + q(**{"title.locked": 0, "summary.locked": 0, "titleSort.locked": 0}), label="edit unlock")
        call("GET", "/library/metadata/%s" % rk, label="item after tag removal")
        call("PUT", "/library/metadata/%s/prefs?" % rk + q(**{"hidden": 0}), label="item prefs")
    if show and picks.get("shows"):
        rk = picks["shows"][2]["ratingKey"]
        e = "/library/sections/%s/all?type=2&id=%s&" % (show["key"], rk)
        call("PUT", e + q(**{"title.value": "Rig Show", "title.locked": 1, "genre[0].tag.tag": "Rig Show Genre"}), label="edit show")
        call("PUT", "/library/metadata/%s/prefs?" % rk + q(episodeSort="1", flattenSeasons="1"), label="show prefs")
        call("GET", "/library/metadata/%s?includePreferences=1" % rk, label="show after edit")
        call("GET", "/library/metadata/%s/children" % rk, label="show children after flatten")
        call("PUT", "/library/metadata/%s/prefs?" % rk + q(episodeSort="-1", flattenSeasons="-1"), label="show prefs reset")

    # playlists
    if mv:
        uri = "server://%s/com.plexapp.plugins.library/library/metadata/%s" % (mid, ",".join(m["ratingKey"] for m in mv[10:15]))
        s, p = call("POST", "/playlists?" + q(type="video", title="Rig Playlist", smart=0, uri=uri), label="playlist create")
        pl = (mc(p).get("Metadata") or [{}])[0].get("ratingKey")
        if pl:
            call("GET", "/playlists/%s" % pl, label="playlist get")
            s, p = call("GET", "/playlists/%s/items" % pl, label="playlist items")
            its = mc(p).get("Metadata", [])
            uri2 = "server://%s/com.plexapp.plugins.library/library/metadata/%s" % (mid, ",".join(m["ratingKey"] for m in (mv[15:17] + eps[6:8])))
            call("PUT", "/playlists/%s/items?" % pl + q(uri=uri2), label="playlist add")
            if len(its) >= 3:
                call("PUT", "/playlists/%s/items/%s/move?after=%s" % (pl, its[0]["playlistItemID"], its[2]["playlistItemID"]), label="playlist move")
                call("DELETE", "/playlists/%s/items/%s" % (pl, its[1]["playlistItemID"]), label="playlist remove item")
            call("PUT", "/playlists/%s?" % pl + q(title="Rig Playlist Renamed", summary="sum 'q'"), label="playlist rename")
            call("GET", "/playlists/%s/items" % pl, label="playlist items after")
            call("GET", "/playlists", label="playlists list")
            s, p = call("POST", "/playQueues?" + q(type="video", playlistID=pl, shuffle=0, repeat=0, continuous=0, own=1), label="playQueue from playlist")
            pq = mc(p).get("playQueueID")
            if pq:
                call("GET", "/playQueues/%s" % pq, label="playQueue get")
                call("PUT", "/playQueues/%s/shuffle" % pq, label="playQueue shuffle")
                call("PUT", "/playQueues/%s/unshuffle" % pq, label="playQueue unshuffle")
        if movie:
            s, p = call("POST", "/playlists?" + q(type="video", title="Rig Smart Playlist", smart=1, uri="server://%s/com.plexapp.plugins.library/library/sections/%s/all?type=1&sort=titleSort&year>>=2015" % (mid, movie["key"])), label="smart playlist create")
            spl = (mc(p).get("Metadata") or [{}])[0].get("ratingKey")
            if spl:
                call("GET", "/playlists/%s/items?X-Plex-Container-Start=0&X-Plex-Container-Size=30" % spl, label="smart playlist items")
        s, p = call("POST", "/playQueues?" + q(type="video", uri="server://%s/com.plexapp.plugins.library/library/metadata/%s" % (mid, mv[0]["ratingKey"]), shuffle=0, repeat=0, continuous=1, own=1), label="playQueue from item")

    # collections
    if mv and movie:
        uri = "server://%s/com.plexapp.plugins.library/library/metadata/%s" % (mid, ",".join(m["ratingKey"] for m in mv[20:25]))
        s, p = call("POST", "/library/collections?" + q(type=1, title="Rig Collection", smart=0, sectionId=movie["key"], uri=uri), label="collection create")
        col = (mc(p).get("Metadata") or [{}])[0].get("ratingKey")
        if col:
            call("GET", "/library/collections/%s" % col, label="collection get")
            call("GET", "/library/collections/%s/children" % col, label="collection children")
            call("PUT", "/library/collections/%s/items?" % col + q(uri="server://%s/com.plexapp.plugins.library/library/metadata/%s" % (mid, mv[25]["ratingKey"])), label="collection add")
            call("PUT", "/library/collections/%s/prefs?" % col + q(collectionMode=2, collectionSort=2), label="collection prefs")
            call("PUT", "/library/collections/%s/items/%s/move?after=%s" % (col, mv[20]["ratingKey"], mv[22]["ratingKey"]), label="collection move")
            call("DELETE", "/library/collections/%s/items/%s" % (col, mv[21]["ratingKey"]), label="collection remove item")
            call("PUT", "/library/sections/%s/all?type=18&id=%s&" % (movie["key"], col) + q(**{"title.value": "Rig Collection Renamed", "summary.value": "s", "titleSort.value": "!rig"}), label="collection edit")
            call("GET", "/library/collections/%s/children" % col, label="collection children after")
            call("GET", "/hubs/sections/%s/manage" % movie["key"], label="hubs manage after collection")
            call("POST", "/hubs/sections/%s/manage?" % movie["key"] + q(metadataItemId=col, promotedToRecommended=1, promotedToOwnHome=1, promotedToSharedHome=0), label="collection promote")
            call("GET", "/hubs/sections/%s" % movie["key"], label="section hubs after promote")
            call("GET", "/hubs", label="home hubs after promote")
        s, p = call("POST", "/library/collections?" + q(type=1, title="Rig Smart Collection", smart=1, sectionId=movie["key"], uri="server://%s/com.plexapp.plugins.library/library/sections/%s/all?type=1&sort=titleSort&year>>=2018&genre=Action" % (mid, movie["key"])), label="smart collection create")
        scol = (mc(p).get("Metadata") or [{}])[0].get("ratingKey")
        if scol:
            call("GET", "/library/collections/%s" % scol, label="smart collection get")
            call("GET", "/library/collections/%s/children?X-Plex-Container-Start=0&X-Plex-Container-Size=30" % scol, label="smart collection children")
        call("GET", "/library/sections/%s/collections" % movie["key"], label="collections list after")
        call("GET", "/library/sections/%s/all?type=1&includeCollections=1&collection=Rig%%20Collection%%20Renamed" % movie["key"], label="filter by collection")
    # section and server settings
    if movie:
        call("PUT", "/library/sections/%s/prefs?" % movie["key"] + q(**{"prefs[collectionMode]": 2}), label="section prefs")
        call("GET", "/library/sections/%s/prefs" % movie["key"], label="section prefs get")
    call("PUT", "/:/prefs?" + q(FriendlyName="pgrig"), label="server pref")
    call("GET", "/", label="root after pref")
    # the deletes last
    s, p = call("GET", "/playlists", record=False)
    for pl in mc(p).get("Metadata", []):
        if pl.get("title", "").startswith("Rig "):
            call("DELETE", "/playlists/%s" % pl["ratingKey"], label="playlist delete")
    s, p = call("GET", "/library/collections", record=False)
    for c in mc(p).get("Metadata", []):
        if c.get("title", "").startswith("Rig "):
            call("DELETE", "/library/collections/%s" % c["ratingKey"], label="collection delete")
    call("GET", "/playlists", label="playlists after delete")
    call("GET", "/library/collections", label="collections after delete")


# ---------------------------------------------------------------- scan
def phase_scan():
    have = {s["title"]: s for s in sections()}
    if "Rig Movies" not in have:
        call("POST", "/library/sections?" + q(name="Rig Movies", type="movie", agent="tv.plex.agents.movie", scanner="Plex Movie", language="en-US", location="/media/movies"), label="section create movies")
    if "Rig TV" not in have:
        call("POST", "/library/sections?" + q(name="Rig TV", type="show", agent="tv.plex.agents.series", scanner="Plex TV Series", language="en-US", location="/media/tv"), label="section create tv")
    time.sleep(5)
    wait_idle("first scan", 600)
    have = {s["title"]: s for s in sections()}
    for name, main in (("Rig Movies", 1), ("Rig TV", 2)):
        sec = have.get(name)
        if not sec:
            continue
        k = sec["key"]
        call("GET", "/library/sections/%s/refresh" % k, label="section refresh %s" % name)
        time.sleep(3)
        wait_idle("refresh " + name, 600)
        # the scanner and the agents run on; give them a bounded settle
        for _ in range(60):
            s, p = call("GET", "/library/sections/%s/all?type=%d" % (k, main), record=False)
            ms = mc(p).get("Metadata", [])
            want = 3 if main == 1 else 2
            if len(ms) >= want and all(m.get("guid", "").startswith("plex://") or m.get("updatedAt") for m in ms):
                break
            time.sleep(3)
        time.sleep(10)
        call("GET", "/library/sections/%s/all?type=%d&sort=titleSort&includeGuids=1" % (k, main), label="scanned items %s" % name)
        s, p = call("GET", "/library/sections/%s/all?type=%d&sort=titleSort" % (k, main), record=False)
        for it in mc(p).get("Metadata", []):
            rk = it["ratingKey"]
            call("GET", "/library/metadata/%s?includeChildren=1&includeGuids=1&includeMarkers=1&includeChapters=1&includePreferences=1" % rk, label="scanned item")
            if main == 2:
                call("GET", "/library/metadata/%s/allLeaves" % rk, label="scanned leaves")
                call("GET", "/library/metadata/%s/children" % rk, label="scanned seasons")
            call("PUT", "/library/metadata/%s/analyze" % rk, label="analyze item")
            call("PUT", "/library/metadata/%s/refresh?force=1" % rk, label="refresh item")
        time.sleep(5)
        wait_idle("item refresh " + name, 300)
        call("GET", "/library/sections/%s/all?type=%d&sort=titleSort&includeGuids=1" % (k, main), label="items after refresh %s" % name)
        call("GET", "/library/sections/%s/all?type=%d&sort=titleSort" % (k, 1 if main == 1 else 4), label="leaves with media %s" % name)
        call("GET", "/hubs/sections/%s" % k, label="hubs of %s" % name)
        call("PUT", "/library/sections/%s/emptyTrash" % k, label="empty trash %s" % name)
        call("PUT", "/library/sections/%s/analyze" % k, label="analyze section %s" % name)
        call("GET", "/library/sections/%s/all?type=%d&unmatched=1" % (k, main), label="unmatched %s" % name)
    wait_idle("analyze", 300)
    # play one scanned file through the timeline, and fetch its stream list
    sec = have.get("Rig Movies")
    if sec:
        s, p = call("GET", "/library/sections/%s/all?type=1&sort=titleSort" % sec["key"], record=False)
        for it in mc(p).get("Metadata", [])[:2]:
            rk, dur = it["ratingKey"], int(it.get("duration", 20000))
            base = "/:/timeline?" + q(ratingKey=rk, key="/library/metadata/%s" % rk, identifier="com.plexapp.plugins.library", duration=dur)
            call("GET", base + "&state=playing&time=3000", label="scanned timeline playing")
            call("GET", base + "&state=stopped&time=%d" % (dur - 500), label="scanned timeline end")
            call("GET", "/library/metadata/%s" % rk, label="scanned item after play")
            for med in it.get("Media", [])[:1]:
                for part in med.get("Part", [])[:1]:
                    call("GET", "/library/parts/%s" % part["id"], label="scanned part")
                    call("PUT", "/library/parts/%s?" % part["id"] + q(subtitleStreamID=0, allParts=1), label="part select subtitle none")
    call("GET", "/library/sections", label="sections after scan")
    call("GET", "/library/recentlyAdded", label="recentlyAdded after scan")
    call("GET", "/hubs", label="hubs after scan")
    call("PUT", "/library/optimize?async=1", label="optimize")
    call("PUT", "/library/clean/bundles?async=1", label="clean bundles")
    wait_idle("optimize", 600)


# ---------------------------------------------------------------- voice
def phase_voice():
    """Voice search over three titles chosen to tell an edit distance's case
    rule apart, then a manual match, an unmatch and a match again."""
    sec = next((s for s in sections() if s["title"] == "Rig Movies"), None)
    if not sec:
        return
    k = sec["key"]
    s, p = call("GET", "/library/sections/%s/all?type=1&sort=titleSort" % k, record=False)
    ms = mc(p).get("Metadata", [])
    if len(ms) < 3:
        return
    orig = [(m["ratingKey"], m["title"]) for m in ms[:3]]
    for (rk, _), t in zip(orig, ["abcdef", "ABCDEX", "Abcdxx"]):
        call("PUT", "/library/sections/%s/all?type=1&id=%s&" % (k, rk) + q(**{"title.value": t, "title.locked": 1}), label="voice title set")
    for term in ["ABCDEF", "abcdef", "ABCDEF zzzzzzzzzz", "abc", "Abcdx", "x" * 300, "caf\u00e9 o'brien 100%"]:
        s, p = call("GET", "/hubs/search/voice?" + q(query=term), label="voice %s" % term[:20])
        for h in mc(p).get("Hub", []) if s == 200 else []:
            if h.get("type") == "movie":
                print("    voice %-22s -> %s" % (term[:20], [(m.get("title"), m.get("distance")) for m in h.get("Metadata", []) if m.get("librarySectionID") == int(k) or True][:6]), flush=True)
    for rk, t in orig:
        call("PUT", "/library/sections/%s/all?type=1&id=%s&" % (k, rk) + q(**{"title.value": t, "title.locked": 0}), label="voice title restore")
    # match management on the first item
    rk, title = orig[0]
    s, p = call("GET", "/library/metadata/%s/matches?" % rk + q(manual=1, title=title, agent="tv.plex.agents.movie", language="en-US"), label="matches manual", timeout=180)
    res = mc(p).get("SearchResult", []) if s == 200 else []
    call("PUT", "/library/metadata/%s/unmatch" % rk, label="unmatch")
    time.sleep(3)
    call("GET", "/library/metadata/%s" % rk, label="item after unmatch")
    call("GET", "/library/sections/%s/all?type=1&unmatched=1" % k, label="unmatched list after unmatch")
    if res:
        call("PUT", "/library/metadata/%s/match?" % rk + q(guid=res[0]["guid"], name=res[0].get("name", title)), label="match", timeout=180)
        time.sleep(5)
        wait_idle("match", 300)
        call("GET", "/library/metadata/%s?includeGuids=1" % rk, label="item after match")
    call("GET", "/library/sections/%s/all?type=1&sort=titleSort&includeGuids=1" % k, label="items after match")


# ---------------------------------------------------------------- livetv
def phase_livetv():
    tuner = "http://tuner/hdhr/x1"
    call("GET", "/media/grabbers/devices", label="devices before")
    call("POST", "/media/grabbers/devices/discover?" + q(uri=tuner), label="device discover")
    s, p = call("POST", "/media/grabbers/devices?" + q(uri=tuner), label="device register")
    dev = next((d for d in mc(p).get("Device", []) if "CAFE0011" in d.get("uuid", "")), None)
    if not dev:
        s, p = call("GET", "/media/grabbers/devices", record=False)
        dev = next((d for d in mc(p).get("Device", []) if "CAFE0011" in d.get("uuid", "")), None)
    if not dev:
        print("  livetv: no device registered", flush=True)
        return
    lineup = "lineup://tv.plex.providers.epg.xmltv/" + tuner + "/xmltv.xml#rig x1"
    s, p = call("GET", "/livetv/dvrs", record=False)
    dvr = next((d for d in mc(p).get("Dvr", []) if d.get("lineup") == lineup), None)
    if not dvr:
        s, p = call("POST", "/livetv/dvrs?" + q(language="eng", device=dev["uuid"], lineup=lineup), label="dvr create", timeout=300)
        dvr = (mc(p).get("Dvr") or [None])[0]
    if not dvr:
        print("  livetv: no dvr", flush=True)
        return
    call("GET", "/livetv/epg/channelmap?" + q(device=dev["uuid"], lineup=lineup), label="channelmap suggestion")
    chans = ["9001", "9002", "9003", "9004"]
    params = [("channelsEnabled", ",".join(chans))]
    for c in chans:
        params.append(("channelMappingByKey[%s]" % c, c))
        params.append(("channelMapping[%s]" % c, c))
    call("PUT", "/media/grabbers/devices/%s/channelmap?" % dev["key"] + urllib.parse.urlencode(params, quote_via=urllib.parse.quote), label="channelmap save")
    time.sleep(5)
    wait_idle("guide load", 900)
    call("GET", "/livetv/dvrs", label="dvrs after")
    call("GET", "/livetv/dvrs/%s" % dvr["key"], label="dvr after")
    call("GET", "/media/grabbers/devices/%s" % dev["key"], label="device after")
    epg = "/" + dvr.get("epgIdentifier", "")

    def guide(tag):
        for p2 in ["", "/sections", "/hubs/discover", "/hubs/discover?promoted=1", "/lineups/dvr/channels",
                   "/grid?type=1&sort=beginsAt&X-Plex-Container-Start=0&X-Plex-Container-Size=60",
                   "/grid?type=4&sort=beginsAt&X-Plex-Container-Start=0&X-Plex-Container-Size=60",
                   "/hubs/search?query=soup&limit=10", "/hubs/search?query=movie&limit=10",
                   "/sections/1/all?type=1&sort=titleSort", "/sections/2/all?type=2&sort=titleSort",
                   "/sections/2/all?type=4&sort=titleSort", "/sections/1/genre", "/sections/2/genre",
                   "/sections/3/all?type=4&sort=titleSort", "/sections/4/all?type=4&sort=titleSort",
                   "/hubs/sections/1", "/hubs/sections/2"]:
            call("GET", epg + p2, label="guide %s %s" % (tag, p2))
        s, p = call("GET", epg + "/grid?type=4&sort=beginsAt&X-Plex-Container-Start=0&X-Plex-Container-Size=10", record=False)
        for it in mc(p).get("Metadata", [])[:4]:
            call("GET", epg + "/metadata/%s" % urllib.parse.quote(it["ratingKey"], safe=""), label="guide %s airing" % tag)
            if it.get("grandparentRatingKey"):
                call("GET", epg + "/metadata/%s" % urllib.parse.quote(it["grandparentRatingKey"], safe=""), label="guide %s show" % tag)
                call("GET", epg + "/metadata/%s/children" % urllib.parse.quote(it["grandparentRatingKey"], safe=""), label="guide %s show children" % tag)
        return mc(p).get("Metadata", [])

    airings = guide("first")
    # a recording rule on the first airing, read back, then removed
    if airings:
        rk = airings[0]["ratingKey"]
        s, p = call("GET", "/media/subscriptions/template?" + q(guid=airings[0].get("guid", "")), label="subscription template")
        hints = {"hints[title]": airings[0].get("grandparentTitle") or airings[0].get("title", "x"), "hints[type]": 2 if airings[0].get("grandparentTitle") else 1}
        prm = {"type": 2 if airings[0].get("grandparentTitle") else 1, "targetLibrarySectionID": next((s_["key"] for s_ in sections() if s_["type"] == ("show" if airings[0].get("grandparentTitle") else "movie")), "1"),
               "targetSectionLocationID": "", "prefs[minVideoQuality]": 0, "prefs[replaceLowerQuality]": "false", "prefs[recordPartials]": "true",
               "prefs[startOffsetMinutes]": 0, "prefs[endOffsetMinutes]": 0, "prefs[lineupChannel]": "", "prefs[startTimeslot]": -1, "prefs[oneShot]": "true",
               "params[airingChannels]": "", "params[airingTimes]": "", "params[libraryType]": 2, "params[mediaProviderID]": dvr["key"],
               "includeGrabs": 1, "hints[ratingKey]": rk, **hints}
        s, p = call("POST", "/media/subscriptions?" + q(**prm), label="subscription create")
        call("GET", "/media/subscriptions", label="subscriptions list")
        call("GET", "/media/subscriptions/scheduled", label="subscriptions scheduled")
        for sub in mc(p).get("MediaSubscription", []) if isinstance(p, dict) else []:
            call("DELETE", "/media/subscriptions/%s" % sub["key"], label="subscription delete")
    call("POST", "/livetv/dvrs/%s/reloadGuide" % dvr["key"], label="reloadGuide")
    time.sleep(5)
    wait_idle("guide reload", 900)
    guide("reloaded")
    call("GET", "/hubs", label="home hubs with live tv")
    call("GET", "/media/providers", label="providers with live tv")


# ---------------------------------------------------------------- butler
def phase_butler():
    for t in ["OptimizeDatabase", "CleanOldBundles", "CleanOldCacheFiles", "RefreshLocalMedia", "GarbageCollectBlobs",
              "GarbageCollectLibraryMedia", "GenerateAutoTags", "RefreshEpgGuides", "UpgradeMediaAnalysis", "BuildGracenoteCollections",
              "GenerateBlurHashes", "ButlerTaskGenerateMediaIndexFiles", "RefreshPeriodicMetadata", "LoudnessAnalysis", "MusicAnalysis"]:
        call("POST", "/butler/%s" % t, label="butler %s" % t)
        time.sleep(3)
        wait_idle("butler " + t, 240)
    call("GET", "/butler", label="butler after")
    call("GET", "/library/sections", label="sections after butler")


# ---------------------------------------------------------------- load
def phase_load():
    secs = sections()
    paths = ["/library/sections", "/hubs", "/library/recentlyAdded", "/library/onDeck", "/hubs/search?query=the&limit=10",
             "/status/sessions", "/playlists", "/library/collections", "/livetv/dvrs", "/hubs/continueWatching"]
    for sec in secs:
        k = sec["key"]
        main = {"movie": 1, "show": 2}.get(sec["type"], 1)
        paths += ["/library/sections/%s/all?type=%d&sort=titleSort&X-Plex-Container-Start=%d&X-Plex-Container-Size=50" % (k, main, off) for off in (0, 50, 100)]
        paths += ["/hubs/sections/%s" % k, "/library/sections/%s/genre" % k, "/library/sections/%s/firstCharacter" % k,
                  "/library/sections/%s/all?type=%d&sort=addedAt:desc&X-Plex-Container-Start=0&X-Plex-Container-Size=50" % (k, main)]
        s, p = call("GET", "/library/sections/%s/all?type=%d&sort=titleSort&X-Plex-Container-Start=0&X-Plex-Container-Size=40" % (k, main), record=False)
        for it in mc(p).get("Metadata", [])[:25]:
            paths.append("/library/metadata/%s" % it["ratingKey"])
            paths.append("/library/metadata/%s/children" % it["ratingKey"])
    counts = {}
    slow = []

    def worker(i):
        for r in range(6):
            for j, pth in enumerate(paths):
                if (j + r) % 8 != i:
                    continue
                t0 = time.time()
                s, _ = call("GET", pth, record=False, timeout=180)
                dt = time.time() - t0
                with lock:
                    counts[s] = counts.get(s, 0) + 1
                    if dt > 10:
                        slow.append((round(dt, 1), pth))
                # a write among the reads
                if j % 17 == 0 and "/library/metadata/" in pth and "children" not in pth:
                    rk = pth.rsplit("/", 1)[1]
                    call("GET", "/:/timeline?" + q(ratingKey=rk, key=pth, identifier="com.plexapp.plugins.library", duration=1200000, state="playing", time=1000 * (r + 1)), record=False,
                         headers={"X-Plex-Client-Identifier": "pgrig-load-%d" % i, "X-Plex-Session-Identifier": "pgrig-load-%d" % i})

    ts = [threading.Thread(target=worker, args=(i,)) for i in range(8)]
    t0 = time.time()
    for t in ts:
        t.start()
    for t in ts:
        t.join()
    probes = {}
    for _ in range(200):
        s, _ = call("GET", "/library/sections", record=False)
        probes[s] = probes.get(s, 0) + 1
    d = os.path.join(OUT, "load")
    os.makedirs(d, exist_ok=True)
    with open(os.path.join(d, "summary.json"), "w") as f:
        json.dump({"requests": sum(counts.values()), "statuses": counts, "probes": probes, "seconds": round(time.time() - t0, 1), "slow": sorted(slow, reverse=True)[:20]}, f, indent=1)
    print("  load:", counts, "probes:", probes, "slow:", len(slow), flush=True)


for ph in PHASES:
    phase = ph
    t0 = time.time()
    n0 = seq
    print("phase %s ..." % ph, flush=True)
    globals()["phase_" + ph]()
    print("phase %s: %d calls in %.0fs" % (ph, seq - n0, time.time() - t0), flush=True)
