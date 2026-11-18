#!/usr/bin/env python3
"""cmp.py <out A> <out B> [phase...] [-v]: compares two mix.py trees call by call.

Values that differ between any two servers are masked first: the machine
identifier, the version, a time within two days of now, a transcode or
session token. Whatever still differs is printed, grouped by the JSON path it
is at, with one example each.
"""
import json, os, re, sys, time

A, B = sys.argv[1], sys.argv[2]
verbose = "-v" in sys.argv
phases = [p for p in sys.argv[3:] if p != "-v"]
NOW = time.time()
MASK_KEYS = {"machineIdentifier", "version", "mediaTagVersion", "contentChangedAt", "updatedAt", "transcoderActiveVideoSessions", "sessionKey", "playQueueVersion",
             "myPlexSubscription", "startState", "claimed", "pushNotifications", "ownerFeatures", "diagnostics",
             "freeSpace", "storageTotal", "platformVersion", "address", "countryCode", "installed"}
HEX32 = re.compile(r"\b[0-9a-f]{32,40}\b")
VER = re.compile(r"1\.43\.5\.110(29|79)(\.0)?-[0-9a-f]{9}")
RANDOM_HUB = re.compile(r"\.(genre|by\.actor\.or\.director|topunwatched|more\.|similar)")
UUID = re.compile(r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b")
STAMP = re.compile(r"\b1[78]\d{8}\b")


def norm(v, key=None):
    if isinstance(v, dict):
        hid = v.get("hubIdentifier")
        if isinstance(hid, str) and (v.get("random") or RANDOM_HUB.search(hid)):
            return {"hubIdentifier": re.sub(r"\d+$", "N", hid), "random hub": True, "type": v.get("type"), "nonempty": bool(v.get("size"))}
        return {k: norm(x, k) for k, x in v.items()}
    if isinstance(v, list):
        return [norm(x, key) for x in v]
    if key in MASK_KEYS:
        return "<%s>" % key
    if isinstance(v, (int, float)) and not isinstance(v, bool) and abs(v - NOW) < 2 * 86400:
        return "<now>"
    if isinstance(v, str):
        v = VER.sub("<ver>", v)
        v = HEX32.sub("<hex>", v)
        v = UUID.sub("<uuid>", v)
        v = STAMP.sub(lambda m: "<now>" if abs(int(m.group()) - NOW) < 2 * 86400 else m.group(), v)
    return v


def diff(a, b, path, out):
    if type(a) is not type(b):
        out.append((path, a, b))
    elif isinstance(a, dict):
        for k in sorted(set(a) | set(b)):
            if k not in a:
                out.append((path + "." + k + " (only B)", None, b[k]))
            elif k not in b:
                out.append((path + "." + k + " (only A)", a[k], None))
            else:
                diff(a[k], b[k], path + "." + k, out)
    elif isinstance(a, list):
        if len(a) != len(b):
            out.append((path + "[len]", len(a), len(b)))
        for x, y in zip(a, b):
            diff(x, y, path + "[]", out)
    elif a != b:
        out.append((path, a, b))


def short(v):
    s = json.dumps(v, ensure_ascii=False)
    return s if len(s) < 160 else s[:157] + "..."


total = same = 0
groups = {}
for ph in sorted(set(os.listdir(A)) | set(os.listdir(B))):
    if phases and ph not in phases:
        continue
    da, db = os.path.join(A, ph), os.path.join(B, ph)
    if not os.path.isdir(da) and not os.path.isdir(db):
        continue
    def load(d):
        """A phase's calls keyed by label and occurrence, so one call more or
        fewer on a side (a match that found nothing) shifts nothing else."""
        out, seen = {}, {}
        for f in sorted(x for x in os.listdir(d) if x[0].isdigit()):
            r = json.load(open(os.path.join(d, f)))
            k = (r["method"], norm(r["label"]))
            seen[k] = seen.get(k, 0) + 1
            out[(k, seen[k])] = (f, r)
        return out
    ca = load(da) if os.path.isdir(da) else {}
    cb = load(db) if os.path.isdir(db) else {}
    onlya = [k for k in ca if k not in cb]
    onlyb = [k for k in cb if k not in ca]
    if onlya or onlyb:
        print("%s: %d calls only in A %s, %d only in B %s" % (ph, len(onlya), [k[0][1][:30] for k in onlya][:4], len(onlyb), [k[0][1][:30] for k in onlyb][:4]))
    statuses = {}
    for key in ca:
        if key not in cb:
            continue
        f, ra = ca[key]
        rb = cb[key][1]
        total += 1
        statuses[ra["status"]] = statuses.get(ra["status"], 0) + 1
        out = []
        if norm(ra["path"]) != norm(rb["path"]):
            out.append(("<request path>", ra["path"], rb["path"]))
        if ra["status"] != rb["status"]:
            out.append(("<status>", ra["status"], rb["status"]))
        diff(norm(ra["json"]), norm(rb["json"]), "", out)
        if norm(ra["text"]) != norm(rb["text"]):
            out.append(("<text>", ra["text"], rb["text"]))
        if "sort=random" in ra["path"] or "shuffle" in ra["label"]:
            out = [o for o in out if o[0] in ("<status>", "<request path>")]
        if not out:
            same += 1
            continue
        for path, x, y in out:
            g = groups.setdefault((ph, path), [])
            g.append((f, ra["method"], ra["label"], x, y))
    print("%s: statuses in A %s" % (ph, dict(sorted(statuses.items()))))
print("\n%d calls compared, %d identical after masking, %d differ" % (total, same, total - same))
for (ph, path), g in sorted(groups.items(), key=lambda kv: (kv[0][0], -len(kv[1]))):
    f, m, label, x, y = g[0]
    print("\n[%s] %s  x%d   e.g. %s %s %s" % (ph, path, len(g), f, m, label[:110]))
    print("    A: %s\n    B: %s" % (short(x), short(y)))
    if verbose:
        for f, m, label, x, y in g[1:6]:
            print("      %s %s %s\n        A: %s\n        B: %s" % (f, m, label[:100], short(x), short(y)))
