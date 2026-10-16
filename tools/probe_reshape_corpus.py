#!/usr/bin/env python3
"""probe_reshape_corpus.py — bounded regression probe for the `reformat` engine
defects found while reshaping the 52-file `_sealed_src` corpus (WS1).

The full-corpus run lives in `target/reshape-corpus/REPORT.md`; this probe is a
small, fast, self-contained fixture that reproduces each defect at the MCP
surface, so a regression fails here in a few seconds instead of needing the
52-file corpus.

Each fixture mirrors one corpus shape:

  1. `nested_same_line.cs`  — ClipFixer.MI.cs.nzk: a nested class declaration
     shares the namespace/class opener line. The old shape pass dropped the
     trailing declaration and emitted an invalid bare `{`.
  2. `nested_next_line.cs`  — MI.NaN.cs.nzk: the class body brace opens on the
     line after the namespace line. The old shape pass took `rposition("{")`
     (the namespace brace) and panicked slicing `ctoks[kind_idx..brace_pos]`.
  3. `numeric.cs`           — Shorthand.cs.nzk: `1.0E-08f` was rewritten to
     `1.0NZK.E-08f`.
  4. `enum_member.cs`       — monoliths: `Head = 5` was rewritten to
     `NZK.T.Head = 5`.
  5. `params.cs`            — Barcodekiller.cs.nzk: a parameter `a` was
     rewritten to the unrelated static field `NZK.P.a`.
  6. `declarators.cs`       — monoliths: `System.String[] Known` and
     `int SV, VC, SS;` definition names were rewritten.
  7. `comment_carry.cs`     — NZK.Toolkit.cs.nzk: a line entered inside a
     block comment that closes mid-line used to empty the scanner stack and
     panic the worker thread (the caller then hung until its timeout).

The probe drives the real `csgraph --mcp` binary over stdio, reaped in a
`finally`. Every call is bounded and the whole probe must finish under 10 s.

Usage:
    python3 tools/probe_reshape_corpus.py [--bin PATH] [--keep]

Exit status 0 only when every check passes.
"""

from __future__ import annotations

import argparse
import os
import shutil
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

from mcp_drive import McpClient  # noqa: E402

CRATE = HERE.parent
_TARGET_DIR = Path(os.environ.get("CARGO_TARGET_DIR") or (CRATE.parent / "target"))
DEFAULT_BIN = _TARGET_DIR / "debug" / "csgraph"

FIXTURES = {
    "nested_same_line.cs": (
        "namespace NZK { public static partial class Core {"
        " public static partial class MenuItems\n{\n"
        "  public static int Twice(int x){ return x * 2; }\n}}}\n"
    ),
    "nested_next_line.cs": (
        "#if UNITY_EDITOR\nnamespace NZK{ public static partial class Core\n"
        "{ public static class Nested\n{\n"
        "  public static int Twice(int x){ return x * 2; }\n}}}\n#endif\n"
    ),
    "numeric.cs": (
        "namespace NZK{  public static partial class S{\n"
        "  public const float W = 1.0E-08f;\n}}\n"
    ),
    "e_ext.cs": "namespace NZK{ public class E { } }\n",
    "enum_member.cs": (
        "namespace NZK{  public static partial class S{ public enum Slot"
        " { None = -1, Head = 5, Extra = 100 } } }\n"
    ),
    "head_ext.cs": (
        "namespace NZK{ public static partial class T{"
        " public static int Head(){ return 1; } } }\n"
    ),
    "params.cs": (
        "namespace NZK{  public static partial class S{\n"
        "  public static void OK(string a, string b){ Show(a, b); }\n}}\n"
    ),
    "a_ext.cs": "namespace NZK{ public static class P { public static int a; } }\n",
    "declarators.cs": (
        "namespace NZK{  public static partial class D{\n"
        "  public static readonly System.String[] Known = { \"x\" };\n"
        "  public struct R { public int SV, VC, SS; }\n}}\n"
    ),
    "comment_carry.cs": (
        "/* open block comment\n"
        "*/ namespace NZK{ public static partial class T{"
        " public static int X(){ return 1; } }}\n"
    ),
}

CHECKS = [
    (
        "nested_same_line.cs",
        lambda a: "public static partial class MenuItems" in a
        and a.count("{") == a.count("}")
        and "class Core{\n  {\n" not in a,
        "nested declaration on the opener line survives, braces balanced",
    ),
    (
        "nested_next_line.cs",
        lambda a: "public static class Nested" in a
        and a.count("{") == a.count("}")
        and a.rstrip().endswith("#endif"),
        "class brace on the next line is shaped without panicking",
    ),
    (
        "numeric.cs",
        lambda a: "1.0E-08f" in a and "NZK.E-08" not in a,
        "numeric-literal exponent is never qualified",
    ),
    (
        "enum_member.cs",
        lambda a: "Head = 5" in a and "NZK.T.Head" not in a,
        "enum member names are never qualified",
    ),
    (
        "params.cs",
        lambda a: "string a, string b" in a and "NZK.P.a" not in a,
        "parameters shadowing a static field are never qualified",
    ),
    (
        "declarators.cs",
        lambda a: "System.String[] Known" in a
        and "int SV, VC, SS;" in a
        and "NZK." not in a.split("class D{")[1],
        "array and multi-declarator definition names are never qualified",
    ),
    (
        "comment_carry.cs",
        lambda a: "public static int X()" in a,
        "a line closing a carried block comment does not panic the worker",
    ),
]


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--bin", default=os.environ.get("CSGRAPH_BIN", str(DEFAULT_BIN)))
    ap.add_argument("--keep", action="store_true")
    args = ap.parse_args()

    bin_path = Path(args.bin)
    if not bin_path.is_file():
        print(f"probe_reshape_corpus: {bin_path} not found; build with\n"
              f"  source ../rust-env.sh && cargo build --offline -p csgraph-core",
              file=sys.stderr)
        return 2

    tmp = Path(tempfile.mkdtemp(prefix="csgraph-corpus-probe-"))
    results = []
    mcp = None
    t0 = time.monotonic()
    try:
        for name, text in FIXTURES.items():
            (tmp / name).write_text(text)
        mcp = McpClient(bin_path, tmp, ["."], threads=2, timeout=15.0)
        mcp.initialize()
        for name, ok_fn, label in CHECKS:
            t1 = time.monotonic()
            res = mcp.call("reformat", {"file": name})
            wall = time.monotonic() - t1
            text = res["content"][0]["text"] if res.get("content") else ""
            import json

            body = json.loads(text)
            after = body.get("after", "")
            ok = (
                res.get("isError") is not True
                and body.get("ok") is True
                and ok_fn(after)
                and wall < 5.0
            )
            results.append((name, ok, wall, body.get("ok"), label))
            detail = ""
            if not ok:
                detail = f" wall={wall:.2f}s after={after[:220]!r}"
            print(f"[{'ok' if ok else 'FAIL'}] {name}: {label}{detail}", flush=True)
    finally:
        if mcp is not None:
            mcp.close()
        if args.keep:
            print(f"kept fixture at {tmp}")
        else:
            shutil.rmtree(tmp, ignore_errors=True)

    wall = time.monotonic() - t0
    passed = sum(1 for _, ok, *_ in results if ok)
    print("=" * 68)
    print(f"probe_reshape_corpus: {passed}/{len(results)} checks passed in {wall:.2f}s")
    if passed != len(results):
        return 1
    if wall > 10.0:
        print(f"FAILED: probe wall {wall:.2f}s exceeds the 10 s budget")
        return 1
    print("PROBE OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
