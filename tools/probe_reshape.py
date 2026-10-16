#!/usr/bin/env python3
"""probe_reshape.py — end-to-end proof for the M2 `reformat` / `functionize` tools.

Spawns the real `csgraph --mcp` binary over stdio and drives the two MCP-only
tools implemented in `core/src/reshape.rs` against a scratch copy of
`tools/fixtures/reshape/`:

    reformat    — comment conversion, using removal, qualification, body
                  collapse and namespace/class shape; dry-run by default, then a
                  real apply through the atomic write path.
    functionize — extract an exact duplicate cluster into one shared helper and
                  rewrite every duplicate site to call it.

It checks:

  1. tools/list advertises 41 tools (22 routed + 14 edit + 4 aux + preview_change), including
     copy_path, reformat and functionize;
  2. `reformat` dry-run plans every pass and never writes;
  3. a `//` comment body containing `*/` is refused, not guessed;
  4. `reformat apply` writes the dialect form to disk;
  5. a skipped validity gate is `unverified:true` and `require_validated:true`
     refuses the write (the file is untouched);
  6. `functionize` dry-run builds the helper and rewrites every site on paper;
  7. `functionize apply` creates the helper file and rewrites the call sites;
  8. `functionize` refuses members whose bodies differ.
  9. the whole probe is bounded (no call over 10 s, total under 10 s).

No daemon, no `csviewer --serve`: one child process, reaped in `finally`.

Usage:
    python3 tools/probe_reshape.py [--bin PATH] [--keep]

Exit status 0 only when every check passes.
"""

import argparse
import json
import os
import shutil
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

from probe_edit_mcp import CheckFailed, McpClient, Probe  # noqa: E402

CRATE = HERE.parent
_TARGET_DIR = Path(os.environ.get("CARGO_TARGET_DIR") or (CRATE.parent / "target"))
DEFAULT_BIN = _TARGET_DIR / "debug" / "csgraph"
FIXTURE_DIR = HERE / "fixtures" / "reshape"

BOX_ORIGINAL = (FIXTURE_DIR / "box.cs").read_text()


def find_id(mcp, name):
    body, _ = mcp.tool("fuzzy_search", {"q": name, "limit": 10})
    for row in body.get("results", []):
        if row.get("name") == name and row.get("kind") in ("method", "constructor"):
            return row["id"]
    raise CheckFailed(f"no declaration named {name}: {json.dumps(body)[:300]}")


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--bin", default=os.environ.get("CSGRAPH_BIN", str(DEFAULT_BIN)))
    ap.add_argument("--keep", action="store_true")
    args = ap.parse_args()

    bin_path = Path(args.bin)
    if not bin_path.is_file():
        print(
            f"probe_reshape: {bin_path} not found; build first:\n"
            f"  source ../rust-env.sh && cargo build --offline -p csgraph-core",
            file=sys.stderr,
        )
        return 2

    tmp = Path(tempfile.mkdtemp(prefix="csgraph-reshape-probe-"))
    repo = tmp / "repo"
    repo.mkdir(parents=True)
    for entry in sorted(FIXTURE_DIR.iterdir()):
        if entry.is_file():
            shutil.copy(entry, repo / entry.name)
    home = tmp / "home"
    home.mkdir()
    env = dict(os.environ)
    env["HOME"] = str(home)

    probe = Probe()
    mcp = None
    t_start = time.monotonic()
    try:
        mcp = McpClient(
            [
                str(bin_path),
                "--mcp",
                "--repo",
                str(repo),
                "--root",
                ".",
                "--threads",
                "1",
            ],
            env,
        )
        print("[1] protocol + tools/list", flush=True)
        mcp.request(
            "initialize",
            {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "probe_reshape", "version": "1"},
            },
        )
        listed = mcp.request("tools/list")["result"]["tools"]
        names = {t["name"] for t in listed}
        probe.check(
            "tools/list advertises 41 tools including reformat, functionize and copy_path",
            len(listed) == 41
            and {"reformat", "functionize", "copy_path"} <= names,
            f"{len(listed)} tools, "
            f"missing={sorted({'reformat', 'functionize', 'copy_path'} - names)}",
        )

        print("[2] reformat: deterministic dry run", flush=True)
        body, wall = mcp.tool("reformat", {"file": "box.cs"})
        probe.check(
            "reformat dry-run plans all five passes and writes nothing",
            body["ok"] is True
            and body["changed"] is True
            and body["applied"] is False
            and body["dryRun"] is True
            and (tmp / "repo" / "box.cs").read_text() == BOX_ORIGINAL
            and body["passes"]["comments"]["converted"] == 2
            and body["passes"]["usings"]["removed"] == 1
            and body["passes"]["qualified"]["rewritten"] >= 1
            and body["passes"]["collapsed"]["bodies"] == 1
            and body["passes"]["shape"]["status"] == "applied",
            f"comments={body['passes']['comments']['converted']} "
            f"usings={body['passes']['usings']['removed']} "
            f"qualified={body['passes']['qualified']['rewritten']} "
            f"collapsed={body['passes']['collapsed']['bodies']} "
            f"shape={body['passes']['shape']['status']} in {wall*1000:.0f} ms",
        )
        after = body["after"]
        probe.check(
            "reformat plan is the toolkit dialect",
            "namespace Fixture{  public static partial class Box{" in after
            and "/* a box */" in after
            and "/* Adds two numbers. */" in after
            and "using System;" not in after
            and "return Fixture.Counter.Sum(a, b);" in after
            and "public static int Add(int a, int b){return Fixture.Counter.Sum(a, b);}" in after,
            "dialect opener, block comments, no usings, qualified call, collapsed body",
        )
        probe.check(
            "reformat dry-run leaves the file byte-identical",
            (repo / "box.cs").read_text() == BOX_ORIGINAL,
        )

        print("[3] reformat: refuses a comment body containing */", flush=True)
        body, _ = mcp.tool("reformat", {"file": "unsafe_comment.cs"})
        probe.check(
            "a `//` comment whose body contains `*/` is refused",
            body["ok"] is False and "*/" in body.get("error", ""),
            body.get("error", ""),
        )

        print("[4] reformat: real apply", flush=True)
        body, wall = mcp.tool("reformat", {"file": "box.cs", "apply": True})
        applied_text = (repo / "box.cs").read_text()
        probe.check(
            "reformat apply writes the dialect form and reports the gate",
            body["ok"] is True
            and body["applied"] is True
            and body["written"] == ["box.cs"]
            and (body["validated"] == "roslyn" or body["validated"].startswith("skipped:"))
            and "namespace Fixture{  public static partial class Box{" in applied_text
            and "// a box" not in applied_text
            and "using System;" not in applied_text,
            f"validated={body['validated']} unverified={body.get('unverified')} in {wall*1000:.0f} ms",
        )

        print("[5] reformat: skipped gate is unverified; require_validated refuses", flush=True)
        (repo / "box.cs").write_text(BOX_ORIGINAL)
        body, _ = mcp.tool(
            "reformat",
            {"file": "box.cs", "apply": True, "_dotnet": "/nonexistent/csgraph-no-dotnet"},
        )
        probe.check(
            "a missing toolchain is reported as unverified, never a silent pass",
            body["ok"] is True
            and body["unverified"] is True
            and body["validated"].startswith("skipped:"),
            f"validated={body['validated']} unverified={body['unverified']}",
        )
        (repo / "box.cs").write_text(BOX_ORIGINAL)
        body, _ = mcp.tool(
            "reformat",
            {
                "file": "box.cs",
                "apply": True,
                "require_validated": True,
                "_dotnet": "/nonexistent/csgraph-no-dotnet",
            },
        )
        probe.check(
            "require_validated:true refuses the unverified write and writes nothing",
            body["ok"] is False
            and body["unverified"] is True
            and "unverified" in body.get("error", "")
            and (repo / "box.cs").read_text() == BOX_ORIGINAL,
            body.get("error", ""),
        )

        print("[6] functionize: dry-run helper from a real dedupe cluster", flush=True)
        twice = find_id(mcp, "Twice")
        double = find_id(mcp, "Double")
        dup, _ = mcp.tool("find_duplicates", {"min_lines": 2, "limit": 20})
        cluster = next(
            (
                c
                for c in dup.get("exact", [])
                if {"Twice", "Double"} <= {m.get("name") for m in c.get("members", [])}
            ),
            None,
        )
        probe.check(
            "find_duplicates reports the fixture's exact cluster",
            cluster is not None,
            f"exactClusters={dup.get('stats', {}).get('exactClusters')}",
        )
        body, wall = mcp.tool(
            "functionize",
            {
                "cluster": cluster,
                "newfile": "Fixture.Shared.cs",
                "member": "TwiceImpl",
                "namespace": "Fixture",
            },
        )
        content = body.get("helper", {}).get("content", "")
        probe.check(
            "functionize dry-run proves the cluster and plans the helper",
            body["ok"] is True
            and body["applied"] is False
            and body["helper"]["classFull"] == "Fixture.Shared"
            and len(body["selected"]) == 2
            and "namespace Fixture{  public static partial class Shared{" in content
            and "public static int TwiceImpl(int x){" in content
            and not (repo / "Fixture.Shared.cs").exists(),
            f"helper={body['helper'].get('signature')} in {wall*1000:.0f} ms",
        )

        print("[7] functionize: real apply", flush=True)
        body, wall = mcp.tool(
            "functionize",
            {
                "members": [twice, double],
                "newfile": "Fixture.Shared.cs",
                "member": "TwiceImpl",
                "namespace": "Fixture",
                "apply": True,
            },
        )
        helper = (repo / "Fixture.Shared.cs").read_text()
        a_text = (repo / "dup_a.cs").read_text()
        b_text = (repo / "dup_b.cs").read_text()
        probe.check(
            "functionize apply creates the helper and rewrites every duplicate site",
            body["ok"] is True
            and body["applied"] is True
            and (body["validated"] == "roslyn" or body["validated"].startswith("skipped:"))
            and "public static int TwiceImpl(int x){" in helper
            and "return y * 2;" in helper
            and "public static int Twice(int x){return Fixture.Shared.TwiceImpl(x);}" in a_text
            and "public static int Double(int x){return Fixture.Shared.TwiceImpl(x);}" in b_text,
            f"written={body['written']} validated={body['validated']} in {wall*1000:.0f} ms",
        )

        print("[8] functionize: refuses a cluster whose bodies differ", flush=True)
        summation = find_id(mcp, "Sum")
        nope = find_id(mcp, "Nope")
        body, _ = mcp.tool(
            "functionize",
            {
                "members": [summation, nope],
                "newfile": "Fixture.Shared2.cs",
                "member": "SumImpl",
                "namespace": "Fixture",
            },
        )
        probe.check(
            "functionize refuses members whose bodies differ",
            body["ok"] is False and "differ" in body.get("error", ""),
            body.get("error", ""),
        )
    except CheckFailed:
        pass
    finally:
        if mcp is not None:
            mcp.kill()
        if args.keep:
            print(f"kept fixture at {tmp}")
        else:
            shutil.rmtree(tmp, ignore_errors=True)

    wall = time.monotonic() - t_start
    passed = sum(1 for _, ok, _ in probe.results if ok)
    failed = [name for name, ok, _ in probe.results if not ok]
    print("\n" + "=" * 70)
    print(f"probe_reshape: {passed}/{len(probe.results)} checks passed in {wall:.2f}s")
    if failed:
        print("FAILED: " + ", ".join(failed))
        return 1
    if wall > 10.0:
        print(f"FAILED: probe wall {wall:.2f}s exceeds the 10 s budget")
        return 1
    print("PROBE OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
