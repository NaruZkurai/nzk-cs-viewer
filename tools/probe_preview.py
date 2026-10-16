#!/usr/bin/env python3
"""probe_preview.py — bounded evidence for M3 slice 2 (`preview_change`).

One `csgraph --mcp` process over a tiny synthetic repo proves:

  1. the tool is listed in `tools/list` and the body is deterministic
     (two identical calls return byte-identical JSON);
  2. a `rename` preview reports the declaration's current/proposed signature,
     the call sites, the edits, the counts, and claims no writes;
  3. a `retype` preview reports the replaced declared type and the call sites;
  4. refusals are loud: unknown symbol, ambiguous symbol, bad `to`, bad `op`;
  5. ZERO WRITES: every file's sha256 is unchanged after every call, and the
     body always carries `writes: []`.

One process, one unit, bounded on purpose; the probe leaves no daemon behind.

Usage: python3 tools/probe_preview.py [--repo DIR] [--bin PATH]
"""

import argparse
import hashlib
import json
import subprocess
import sys
import tempfile
import time
from pathlib import Path

TIMEOUT = 25  # seconds per MCP call; the owner rule caps any single op at 30 s

FIXTURE = """namespace N {
  public class C {
    public int Add(int a, int b) { return a + b; }
    public int Sum() { return Add(1, 2) + Add(3, 4); }
    public int Value = 7;
  }
  public class D {
    public int Add(int x) { return x; }
  }
}
"""


def die(msg):
    print(f"FAIL: {msg}", file=sys.stderr)
    sys.exit(1)


def find_binary(crate, override):
    if override:
        p = Path(override)
        if not p.is_file():
            die(f"--bin {p} does not exist")
        return p
    for cand in (crate.parent / "target" / "debug" / "csgraph",
                 crate / "target" / "debug" / "csgraph"):
        if cand.is_file():
            return cand
    die("csgraph binary not found; build it or pass --bin")


def sha_tree(root: Path):
    """sha256 of every source file, ignoring the engine's own `.csgraph.v2/`
    parse/graph cache (indexing writes that cache; the preview must not touch a
    single source byte)."""
    out = {}
    for p in sorted(root.rglob("*")):
        rel = p.relative_to(root)
        if not p.is_file() or any(part.startswith(".csgraph") for part in rel.parts):
            continue
        out[str(rel)] = hashlib.sha256(p.read_bytes()).hexdigest()
    return out


class Probe:
    def __init__(self, binary, repo):
        sys.path.insert(0, str(Path(__file__).resolve().parent))
        from mcp_drive import McpClient  # local script, not an installed package

        self.client = McpClient(binary, repo, ["."], threads=1, timeout=TIMEOUT)
        self.client.initialize()

    def call(self, args, expect_error=False):
        res = self.client.call("preview_change", args)
        is_err = bool(res.get("isError"))
        text = res["content"][0]["text"] if res.get("content") else ""
        try:
            body = json.loads(text)
        except json.JSONDecodeError:
            die(f"preview_change returned non-JSON: {text[:300]!r}")
        if expect_error and not is_err:
            die(f"expected a refusal (isError) for {args}, got {text[:300]}")
        if not expect_error and is_err:
            die(f"unexpected refusal for {args}: {text[:300]}")
        if not isinstance(body, dict):
            die(f"preview_change body is not an object: {text[:200]}")
        return body, text, is_err

    def close(self):
        self.client.close()


def check_read_only(body, where):
    if body.get("writes") != []:
        die(f"{where}: writes must be [] — got {body.get('writes')!r}")
    for key, want in (("readOnly", True), ("dryRun", True), ("applied", False)):
        if body.get(key) != want:
            die(f"{where}: {key} must be {want!r} — got {body.get(key)!r}")
    if body.get("ok") is not True:
        die(f"{where}: ok must be true — got {body.get('ok')!r}")


def check_rename(body, before, after, where):
    check_read_only(body, where)
    sig = body.get("currentSignature") or ""
    prop = body.get("proposedSignature") or ""
    if "Add" not in sig:
        die(f"{where}: currentSignature does not name the declaration: {sig!r}")
    if "Plus" not in prop:
        die(f"{where}: proposedSignature does not carry the new name: {prop!r}")
    counts = body.get("counts") or {}
    for key in ("declarations", "files", "edits", "callSites", "changedLines"):
        if key not in counts:
            die(f"{where}: counts is missing {key!r}")
    if counts["edits"] < 2:
        die(f"{where}: rename edits should cover the declaration + its call line: {counts}")
    if counts["callSites"] < 1:
        die(f"{where}: rename must name the call site: {counts}")
    if body["declaration"]["full"] != "N.C.Add" or not body["declaration"]["id"].startswith("N.C.Add("):
        die(f"{where}: resolved the wrong declaration: {body['declaration']}")
    # edits are sorted by (file, line)
    keys = [(e.get("file"), e.get("line")) for e in body["edits"]]
    if keys != sorted(keys):
        die(f"{where}: edits are not sorted by (file, line): {keys}")
    for e in body["edits"]:
        if not isinstance(e.get("before"), str) or not isinstance(e.get("after"), str):
            die(f"{where}: an edit lacks before/after: {e}")
    # the call site line is attributed to the caller declaration
    sites = body["callSites"]
    if not any("Sum" in (s.get("node") or "") for s in sites):
        die(f"{where}: call sites do not attribute the caller: {sites}")
    if any(s.get("line", 0) < 1 for s in sites):
        die(f"{where}: a call site has no line number: {sites}")
    if before != after:
        die(f"{where}: files changed on disk — the preview wrote")


def check_retype(body, before, after, where):
    check_read_only(body, where)
    if body.get("currentType") != "int":
        die(f"{where}: currentType should be int — got {body.get('currentType')!r}")
    prop = body.get("proposedSignature") or ""
    if "long" not in prop:
        die(f"{where}: proposedSignature does not carry the new type: {prop!r}")
    if body.get("counts", {}).get("edits", 0) < 1:
        die(f"{where}: retype must plan the declaration-line edit: {body.get('counts')}")
    if body.get("counts", {}).get("callSites", 0) < 1:
        die(f"{where}: retype must report the affected call sites: {body.get('counts')}")
    for e in body["edits"]:
        if "long" not in (e.get("after") or ""):
            die(f"{where}: retype edit does not carry the new type: {e}")
    if before != after:
        die(f"{where}: files changed on disk — the preview wrote")


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--repo", help="repo to preview against (default: a temp fixture)")
    ap.add_argument("--bin", help="path to the csgraph binary")
    args = ap.parse_args()

    crate = Path(__file__).resolve().parent.parent
    binary = find_binary(crate, args.bin)

    tmp = None
    if args.repo:
        repo = Path(args.repo).resolve()
        if not repo.is_dir():
            die(f"--repo {repo} is not a directory")
    else:
        tmp = tempfile.TemporaryDirectory(prefix="probe-preview-")
        repo = Path(tmp.name)
        (repo / "fixture.cs").write_text(FIXTURE, encoding="utf-8")

    before = sha_tree(repo)
    start = time.monotonic()
    probe = Probe(binary, repo)
    try:
        # 1. listed + deterministic
        names = [t["name"] for t in probe.client.tools()]
        if "preview_change" not in names:
            die("preview_change is not in tools/list")
        print(f"  tools/list: preview_change present ({len(names)} tools)")

        b1, t1, _ = probe.call({"node": "N.C.Add", "op": "rename", "to": "Plus"})
        print(f"$ preview_change rename N.C.Add -> Plus   ({time.monotonic() - start:.2f}s)")
        check_rename(b1, before, sha_tree(repo), "rename")
        b2, t2, _ = probe.call({"node": "N.C.Add", "op": "rename", "to": "Plus"})
        if t1 != t2:
            die("two identical rename previews are not byte-identical")
        print(f"  deterministic: two calls byte-identical "
              f"(edits={b1['counts']['edits']} callSites={b1['counts']['callSites']} "
              f"files={b1['counts']['files']})")
        print(f"  signature: {b1['currentSignature']!r} -> {b1['proposedSignature']!r}")
        for e in b1["edits"]:
            print(f"    edit {e['file']}:{e['line']}  {e['before'].strip()!r} -> {e['after'].strip()!r}")

        # 2. rename with the `id` alias and a compound id selector
        b3, _, _ = probe.call({"id": "N.C.Add(int, int)", "op": "rename", "to": "Plus"})
        check_rename(b3, before, sha_tree(repo), "rename-id")
        print("  `id` selector resolved N.C.Add(int, int)")

        # 3. retype a method and a field
        b4, _, _ = probe.call({"symbol": "N.C.Add", "op": "retype", "to": "long"})
        check_retype(b4, before, sha_tree(repo), "retype-method")
        print(f"  retype method: {b4['currentSignature']!r} -> {b4['proposedSignature']!r} "
              f"(callSites={b4['counts']['callSites']})")
        b5, _, _ = probe.call({"node": "N.C.Value", "op": "retype", "to": "System.Int64"})
        check_read_only(b5, "retype-field")
        if b5.get("currentType") != "int":
            die(f"retype-field: currentType should be int — got {b5.get('currentType')!r}")
        if "System.Int64" not in (b5.get("proposedSignature") or ""):
            die(f"retype-field: proposedSignature: {b5.get('proposedSignature')!r}")
        print(f"  retype field:  {b5['currentSignature']!r} -> {b5['proposedSignature']!r}")
        if sha_tree(repo) != before:
            die("files changed on disk during the successful previews")

        # 4. refusals
        cases = [
            ({"node": "N.C.Missing", "op": "rename", "to": "X"}, "unknown symbol"),
            ({"node": "Add", "op": "rename", "to": "X"}, "ambiguity"),
            ({"node": "N.C.Add", "op": "rename", "to": "not a name"}, "legal C# identifier"),
            ({"node": "N.C.Add", "op": "teleport", "to": "X"}, "op"),
            ({"node": "N.C.Add", "op": "rename"}, "to"),
        ]
        for cargs, needle in cases:
            body, text, is_err = probe.call(cargs, expect_error=True)
            if body.get("ok") is not False or not is_err:
                die(f"refusal {cargs}: ok/isError not set")
            blob = text
            if needle not in blob:
                die(f"refusal {cargs}: expected {needle!r} in {blob[:200]}")
            print(f"  refused {json.dumps(cargs)}: {body.get('error')}")
        # ambiguity carries the candidates
        amb, _, _ = probe.call({"node": "Add", "op": "rename", "to": "X"}, expect_error=True)
        if not amb.get("ambiguity"):
            die("ambiguous selector did not return `ambiguity` candidates")

        elapsed = time.monotonic() - start
        after = sha_tree(repo)
        if after != before:
            die("zero-write proof failed: the tree changed")
        print(f"  zero writes: {len(after)} file(s) sha256 unchanged")
        if elapsed > 30:
            die(f"probe took {elapsed:.1f}s (>30 s)")
        print(f"  bounded: {elapsed:.2f}s, one MCP process, no daemon")
    finally:
        probe.close()
        if tmp is not None:
            tmp.cleanup()

    print("PASS: preview_change is listed, deterministic, read-only, reports "
          "signatures + call sites + edits + counts, and refuses loudly")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as exc:  # noqa: BLE001 — a probe should fail loudly, not traceback
        die(f"{type(exc).__name__}: {exc}")
