#!/usr/bin/env python3
"""bench_find.py — bounded latency + payload-stability bench for /api/find and /api/refs.

`find` and `refs` both funnel through `Index::text_hits` (core/src/index.rs),
which used to rebuild a regex and re-sort the file list on every call before
scanning every line of the corpus. This tool measures the per-call cost on a
fixed query set and fingerprints the exact response bytes, so a performance
change can be proven to be byte-identical.

It never starts a server itself: boot one through tools/with_serve.sh and point
the bench at it (the wrapper injects URL into the environment).

    tools/with_serve.sh -- python3 tools/bench_find.py --out /tmp/new.json
    tools/with_serve.sh --bin target/bench/baseline-csviewer \
        -- python3 tools/bench_find.py --out /tmp/base.json
    python3 tools/bench_find.py --compare /tmp/base.json /tmp/new.json

The compare mode exits 1 if any query's /api/find or /api/refs payload differs
byte-for-byte, so a speed change that alters an answer fails the gate.

Modes:
  (default)          measure against --url (or $URL), print JSON, optionally --out
  --compare A B      diff two saved runs (payload hashes + latency), exit 1 on drift
  --list-queries     print the fixed query set and exit

Every unit is bounded: `reps` is small (default 3), the query set is fixed, and
a per-request timeout (default 20 s) aborts instead of hanging.
"""

import argparse
import hashlib
import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

# Fixed query set. A mix of bare identifiers (the `(?i)\b...\b` path), dotted
# names (which make `find` run text_hits twice: exact then simple), hits that
# dominate one file, and a token that matches nothing (the pure scan floor).
QUERIES = [
    "NaN",
    "NZK",
    "Bake",
    "Mesh",
    "Bone",
    "socket",
    "plug",
    "ClipScaleToNaN",
    "FolderFor",
    "compare",
    "NZK.Core.NaN",
    "NZK.B.compare",
    "sps_socket",
    "public static",
    "zzzz_no_such_symbol_zzzz",
]

REPS = 3
TIMEOUT_S = 20.0


def fetch(url, path, params, timeout):
    qs = urllib.parse.urlencode(params)
    full = url + path + ("?" + qs if qs else "")
    with urllib.request.urlopen(full, timeout=timeout) as r:
        body = r.read()
    return body


def hash_body(body):
    return hashlib.sha256(body).hexdigest()


def measure(url, reps, timeout):
    rows = []
    for q in QUERIES:
        row = {"q": q}
        for path, key in (("/api/find", "find"), ("/api/refs", "refs")):
            # One warm-up call, untimed: the timed reps then measure steady state
            # (a repeat query), which is what the bounded regex cache targets.
            body = fetch(url, path, {"q": q, "limit": 400}, timeout)
            row[key + "_sha256"] = hash_body(body)
            row[key + "_bytes"] = len(body)
            try:
                parsed = json.loads(body)
                usages = parsed.get("usages") if key == "refs" else parsed.get("usages")
                row[key + "_usages"] = len(usages) if isinstance(usages, list) else None
                row[key + "_truncated"] = bool(parsed.get("truncated"))
            except Exception:
                row[key + "_usages"] = None
                row[key + "_truncated"] = None
            t0 = time.perf_counter()
            for _ in range(reps):
                fetch(url, path, {"q": q, "limit": 400}, timeout)
            dt = time.perf_counter() - t0
            row[key + "_ms"] = round(dt * 1000.0 / reps, 4)
        rows.append(row)
    return rows


def totals(rows):
    out = {}
    for key in ("find", "refs"):
        vals = [r[key + "_ms"] for r in rows]
        out[key + "_ms_per_call_mean"] = round(sum(vals) / len(vals), 4)
        out[key + "_ms_per_call_max"] = round(max(vals), 4)
    return out


def run_bench(args):
    url = args.url or os.environ.get("URL") or "http://127.0.0.1:8799/"
    url = url.rstrip("/")
    rows = measure(url, args.reps, args.timeout)
    doc = {
        "url": url,
        "reps": args.reps,
        "queries": rows,
        "totals": totals(rows),
    }
    if args.out:
        with open(args.out, "w") as fh:
            json.dump(doc, fh, indent=1, sort_keys=True)
            fh.write("\n")
    print(json.dumps(doc, indent=1, sort_keys=True))
    t = doc["totals"]
    print("BENCH find=%.2f ms/call refs=%.2f ms/call over %d queries x %d reps"
          % (t["find_ms_per_call_mean"], t["refs_ms_per_call_mean"], len(rows), args.reps))
    return 0


def compare(a_path, b_path):
    with open(a_path) as fh:
        a = json.load(fh)
    with open(b_path) as fh:
        b = json.load(fh)
    aq = {r["q"]: r for r in a["queries"]}
    bq = {r["q"]: r for r in b["queries"]}
    failures = 0

    def line(name, ok, detail):
        nonlocal failures
        if not ok:
            failures += 1
        print(("PASS " if ok else "FAIL ") + name + (" — " + detail if detail else ""))

    line("query sets match", set(aq) == set(bq),
         "only-baseline=%s only-new=%s" % (sorted(set(aq) - set(bq)), sorted(set(bq) - set(aq))))
    for q in QUERIES:
        if q not in aq or q not in bq:
            continue
        for key in ("find", "refs"):
            line("%s %r byte-identical" % (key, q),
                 aq[q][key + "_sha256"] == bq[q][key + "_sha256"],
                 "baseline=%s new=%s" % (aq[q][key + "_sha256"][:16], bq[q][key + "_sha256"][:16]))

    ta, tb = a["totals"], b["totals"]
    for key in ("find", "refs"):
        ba, bn = ta[key + "_ms_per_call_mean"], tb[key + "_ms_per_call_mean"]
        speed = (ba / bn) if bn > 0 else float("inf")
        print("STAT %s %.2f -> %.2f ms/call (%.2fx)" % (key, ba, bn, speed))
    print("COMPARE %s" % ("OK" if failures == 0 else "FAILED (%d)" % failures))
    return 0 if failures == 0 else 1


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--url", default=None, help="engine base URL (default $URL)")
    ap.add_argument("--reps", type=int, default=REPS)
    ap.add_argument("--timeout", type=float, default=TIMEOUT_S)
    ap.add_argument("--out", default=None, help="write the JSON result here")
    ap.add_argument("--compare", nargs=2, metavar=("BASELINE", "NEW"),
                    help="diff two saved runs and exit non-zero on payload drift")
    ap.add_argument("--list-queries", action="store_true")
    # with_serve.sh appends --url/--cdp-port by default; accept and ignore the
    # CDP one so the bench can be run through the wrapper with no extra flags.
    ap.add_argument("--cdp-port", default=None)
    args = ap.parse_args()
    if args.list_queries:
        print("\n".join(QUERIES))
        return 0
    if args.compare:
        return compare(args.compare[0], args.compare[1])
    return run_bench(args)


if __name__ == "__main__":
    sys.exit(main())
