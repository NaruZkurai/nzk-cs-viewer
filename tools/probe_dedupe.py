#!/usr/bin/env python3
"""probe_dedupe.py — bounded evidence for the M2 slice-1 duplicate-code report.

Runs `csgraph dedupe` (see core/src/dedupe.rs) on:

  1. a synthetic temp repo with an exact duplicate pair (one body re-spaced)
     and a whitespace-only variant — the report MUST find the exact cluster;
  2. the real `_sealed_src` corpus — the report MUST be well-formed, and two
     consecutive runs MUST be byte-identical (deterministic);
  3. the same report through the real MCP server (`find_duplicates`), which MUST
     equal the CLI `--json` body (agent/CLI parity).

Bounded: one `csgraph` process per stage, 25 s each; the corpus run is ~5 s, so
the whole probe stays far under 30 s and leaves no daemon behind.

Usage: python3 tools/probe_dedupe.py [--repo DIR] [--bin PATH]
"""

import argparse
import json
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

TIMEOUT = 25  # seconds per stage; owner rule caps any single op at 30 s

FIXTURE = """namespace N {
  class A {
    public int One(int a, int b) {
      int s = a + b;
      int t = s * 2;
      int u = t + s;
      int v = u - 1;
      return v + s;
    }
    public int Two(int a, int b) {
      int  s = a+b;
      int t = s * 2;
      int u = t + s;
      int v = u - 1;
      return v + s;
    }
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


def find_repo(crate, override):
    if override:
        p = Path(override).resolve()
        if not (p / "_sealed_src").is_dir():
            die(f"--repo {p} has no _sealed_src/")
        return p
    for base in (crate, crate.parent, crate.parent.parent, crate.parent.parent.parent):
        if (base / "_sealed_src").is_dir():
            return base
    die("could not locate a _sealed_src corpus; pass --repo")


def run_dedupe(binary, argv):
    cmd = [str(binary), "dedupe", *argv]
    start = time.monotonic()
    try:
        proc = subprocess.run(
            cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            timeout=TIMEOUT, text=True,
        )
    except subprocess.TimeoutExpired:
        die(f"dedupe exceeded {TIMEOUT}s: {' '.join(cmd)}")
    elapsed = time.monotonic() - start
    if proc.returncode != 0:
        die(f"dedupe exited {proc.returncode}: {proc.stderr.strip()}")
    try:
        report = json.loads(proc.stdout)
    except json.JSONDecodeError as exc:
        die(f"dedupe stdout is not JSON ({exc}): {proc.stdout[:200]!r}")
    return report, proc.stdout, elapsed


def check_shape(report, where):
    for key in ("ok", "files", "units", "minLines", "threshold",
                "exact", "near", "blocks", "stats", "truncated"):
        if key not in report:
            die(f"{where}: report is missing {key!r}")
    if report["ok"] is not True:
        die(f"{where}: ok is not true: {report['ok']!r}")
    for key in ("exact", "near", "blocks"):
        if not isinstance(report[key], list):
            die(f"{where}: {key} is not a list")
    # exact clusters: deterministic order, hash agreement, capped members
    counts = [c["count"] for c in report["exact"]]
    if counts != sorted(counts, reverse=True):
        die(f"{where}: exact clusters are not sorted by count desc: {counts}")
    for c in report["exact"]:
        if len(c["members"]) != min(c["count"], 20):
            die(f"{where}: cluster count {c['count']} vs {len(c['members'])} members")
        for m in c["members"]:
            if m["hash"] != c["hash"]:
                die(f"{where}: member hash {m['hash']} != cluster {c['hash']}")
    # near clusters: threshold respected
    for c in report["near"]:
        if c["similarity"]["min"] < report["threshold"] - 1e-9:
            die(f"{where}: near similarity {c['similarity']} below threshold")
        if c["count"] < 2:
            die(f"{where}: near cluster with {c['count']} members")
    # blocks: sorted longest first, each occurrence set > 1
    lens = [b["lines"] for b in report["blocks"]]
    if lens != sorted(lens, reverse=True):
        die(f"{where}: blocks are not sorted by length desc: {lens}")
    for b in report["blocks"]:
        if b["count"] < 2 or len(b["occurrences"]) < 2:
            die(f"{where}: block with {b['count']} occurrences")
    stats = report["stats"]
    if stats["exactClusters"] < len(report["exact"]) or stats["blockGroups"] < len(report["blocks"]):
        die(f"{where}: stats undercount the reported clusters: {stats}")


def stage_fixture(binary):
    with tempfile.TemporaryDirectory(prefix="probe-dedupe-") as tmp:
        src = Path(tmp) / "src"
        src.mkdir()
        (src / "A.cs").write_text(FIXTURE, encoding="utf-8")
        report, _, elapsed = run_dedupe(binary, ["--repo", tmp, "--root", "src", "--json"])
        print(f"$ csgraph dedupe --repo {tmp} --root src --json   ({elapsed:.2f}s)")
        check_shape(report, "fixture")
        exact = report["exact"]
        if not exact:
            die(f"fixture: identical bodies were not detected: {json.dumps(report)}")
        top = exact[0]
        if top["count"] < 2:
            die(f"fixture: top cluster has {top['count']} members")
        names = {m["name"] for m in top["members"]}
        if names != {"One", "Two"}:
            die(f"fixture: wrong members in top cluster: {names}")
        print(f"  exact cluster: count={top['count']} hash={top['hash']} "
              f"members={sorted(names)} tokens={top['tokens']}")


def stage_mcp(binary, repo, cli_report):
    """Drive the same report through MCP and prove it equals the CLI output.

    `core/src/edit.rs::find_duplicates` calls `dedupe::dedupe` directly, so the
    MCP body must be byte-identical to the `csgraph dedupe` JSON for the same
    repo/root. This is the agent-parity half of task-11.
    """
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    from probe_edit_mcp import McpClient  # local script, not an installed package

    argv = [
        str(binary), "--mcp", "--repo", str(repo),
        "--root", "_sealed_src", "--threads", "1",
    ]
    env = dict(os.environ)
    client = McpClient(argv, env)
    try:
        client.request(
            "initialize",
            {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "probe_dedupe", "version": "1"},
            },
        )
        body, wall = client.tool("find_duplicates", {}, timeout=TIMEOUT)
    finally:
        client.kill()
    print(f"$ tools/call find_duplicates   ({wall:.2f}s)")
    if body.get("ok") is not True:
        die(f"find_duplicates did not return ok: {json.dumps(body)[:300]}")
    # Same function, same repo/root -> the whole report must match the CLI's.
    if body != cli_report:
        keys = sorted(set(body) | set(cli_report))
        diff = [k for k in keys if body.get(k) != cli_report.get(k)]
        die(f"MCP find_duplicates differs from the CLI report on {diff}")
    if set(body["stats"]) != set(cli_report["stats"]):
        die("find_duplicates stats shape differs from the CLI")
    print("  MCP body == CLI --json (cluster counts and ordering identical)")
    return body


def stage_corpus(binary, repo):
    argv = ["--repo", str(repo), "--root", "_sealed_src", "--json"]
    first, text1, elapsed = run_dedupe(binary, argv)
    print(f"$ csgraph dedupe --repo {repo} --root _sealed_src --json   ({elapsed:.2f}s)")
    check_shape(first, "_sealed_src")
    if elapsed > 30:
        die(f"_sealed_src run took {elapsed:.1f}s (>30s)")

    second, text2, elapsed2 = run_dedupe(binary, argv)
    if text1 != text2:
        die("_sealed_src report is not byte-deterministic across runs")
    print(f"  deterministic: two runs byte-identical ({elapsed2:.2f}s)")
    print(f"  files={first['files']} methodBodies={first['methodBodies']} "
          f"units={first['units']} truncated={first['truncated']}")
    print(f"  totals: exact={first['stats']['exactClusters']} "
          f"near={first['stats']['nearClusters']} blocks={first['stats']['blockGroups']} "
          f"pairComparisons={first['stats']['pairComparisons']}")
    for c in first["exact"][:5]:
        where = ", ".join(f"{Path(m['file']).name}:{m['line']} {m['name']}"
                          for m in c["members"][:3])
        print(f"  exact x{c['count']} ({c['tokens']} tok): {where}")
    for b in first["blocks"][:5]:
        where = ", ".join(f"{Path(o['file']).name}:{o['line']}"
                          for o in b["occurrences"][:3])
        print(f"  block {b['lines']} lines x{b['count']}: {where}")
    return first


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--repo", help="corpus root containing _sealed_src/")
    ap.add_argument("--bin", help="path to the csgraph binary")
    args = ap.parse_args()

    crate = Path(__file__).resolve().parent.parent
    binary = find_binary(crate, args.bin)
    repo = find_repo(crate, args.repo)

    stage_fixture(binary)
    cli_report = stage_corpus(binary, repo)
    stage_mcp(binary, repo, cli_report)
    print("PASS: dedupe detects exact duplicates, is deterministic, is bounded, "
          "and its MCP tool matches the CLI byte-for-byte")
    return 0


if __name__ == "__main__":
    sys.exit(main())
