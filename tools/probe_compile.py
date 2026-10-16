#!/usr/bin/env python3
"""probe_compile.py — bounded evidence for the compile-LEVEL validity gate.

Drives `csgraph compile-diff --baseline DIR --candidate DIR` (see
core/src/compile_check.rs `cli_compile_diff` / `differential_ex`), which runs a
real Roslyn compile of both trees through Unity's bundled compiler when present
and otherwise through the .NET SDK's own `csc.dll` — the same offline mechanism
`parse-check` uses.

Stages:

  1. a CLEAN edit (same file, changed constant) must report `ok:true`, no
     `newDiagnostics`, `skipped:null`, exit 0;
  2. a candidate that adds `public UndefinedType M()` must be caught as a NEW
     semantic diagnostic (CS0246) — `ok:false`, exit 1;
  3. a forced-missing toolchain (`--data /nonexistent`) must report
     `skipped:<reason>`, `ok:false`, exit 3 — a `skipped` gate is NEVER a pass;
  4. every document is checked for the false-ok invariant.

Exit code is 1 on any failure, 0 when every stage held. A genuinely missing
toolchain on this machine is a FAILURE here: the whole point of the SDK fallback
is that the compile-level gate actually runs when no Unity exists.

Bounded: one `csgraph` process per stage, 20 s each, no daemon, no repo writes.

Usage: python3 tools/probe_compile.py [--bin PATH]
"""

import argparse
import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

TIMEOUT = 20  # seconds per stage; the owner rule caps any single op at 30 s

VALID_A = "namespace N { public class A { public int X() { return 1; } } }\n"
CLEAN_EDIT = "namespace N { public class A { public int X() { return 2; } } }\n"
BAD_B = "namespace N { public class B { public UndefinedType M() { return null; } } }\n"
# Latin-1 `é` inside a comment plus the same undefined type: the file is not
# valid UTF-8, so a gate that drops undecodable files would miss CS0246.
LATIN1_BAD_B = b"// caf\xe9\n" + BAD_B.encode("utf-8")


def die(msg: str) -> "NoReturn":  # type: ignore[name-defined]
    print(f"FAIL: {msg}", file=sys.stderr)
    sys.exit(1)


def find_binary(crate: Path, override):
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


def run_diff(binary: Path, baseline: Path, candidate: Path, extra=()):
    cmd = [str(binary), "compile-diff",
           "--baseline", str(baseline), "--candidate", str(candidate), *extra]
    try:
        proc = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                              timeout=TIMEOUT, text=True)
    except subprocess.TimeoutExpired:
        die(f"compile-diff exceeded {TIMEOUT}s: {' '.join(cmd)}")
    print(f"$ {' '.join(cmd)}")
    if not proc.stdout.strip():
        die(f"compile-diff produced no stdout (rc={proc.returncode}): {proc.stderr.strip()}")
    try:
        report = json.loads(proc.stdout)
    except json.JSONDecodeError as exc:
        die(f"compile-diff stdout is not JSON ({exc}): {proc.stdout[:200]!r}")
    return report, proc.returncode, proc.stderr.strip()


def check_invariant(report, where: str):
    """The gate must never report ok while skipped, and a skip must exit non-zero."""
    skipped = report.get("skipped")
    ok = report.get("ok")
    if skipped is not None:
        if ok is not False:
            die(f"{where}: skipped result must be ok:false, got ok={ok!r}")
        return
    if ok is True and report.get("newDiagnostics"):
        die(f"{where}: ok:true but newDiagnostics is non-empty — false pass")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--bin", help="path to the csgraph binary")
    args = ap.parse_args()

    crate = Path(__file__).resolve().parent.parent
    binary = find_binary(crate, args.bin)

    with tempfile.TemporaryDirectory(prefix="probe-compile-") as tmp:
        root = Path(tmp)
        baseline = root / "baseline"
        clean = root / "candidate-clean"
        broken = root / "candidate-broken"
        for d in (baseline, clean, broken):
            d.mkdir()
        (baseline / "A.cs").write_text(VALID_A, encoding="utf-8")
        (clean / "A.cs").write_text(CLEAN_EDIT, encoding="utf-8")
        (broken / "A.cs").write_text(VALID_A, encoding="utf-8")
        (broken / "B.cs").write_text(BAD_B, encoding="utf-8")

        # 1. clean edit passes
        report, rc, err = run_diff(binary, baseline, clean)
        print(f"  exit={rc} ok={report.get('ok')!r} skipped={report.get('skipped')!r} "
              f"toolchain={report.get('toolchain')!r} new={len(report.get('newDiagnostics') or [])}")
        check_invariant(report, "clean edit")
        if report.get("skipped") is not None:
            die("clean edit was SKIPPED — the compile-level gate did not run "
                f"({report['skipped']}); expected the Unity/SDK Roslyn fallback")
        if rc != 0 or report.get("ok") is not True:
            die(f"clean edit did not pass: rc={rc} {json.dumps(report)[:300]}")
        if report.get("newDiagnostics"):
            die(f"clean edit produced new diagnostics: {report['newDiagnostics']}")
        print("  clean edit passes (0 new diagnostics)")

        # 2. a new semantic diagnostic is caught
        report, rc, err = run_diff(binary, baseline, broken)
        print(f"  exit={rc} ok={report.get('ok')!r} skipped={report.get('skipped')!r} "
              f"new={[(d.get('code'), d.get('message')) for d in (report.get('newDiagnostics') or [])]}")
        check_invariant(report, "broken candidate")
        if report.get("skipped") is not None:
            die(f"broken candidate was SKIPPED, not checked: {report['skipped']}")
        if rc != 1 or report.get("ok") is not False:
            die(f"new semantic diagnostic was NOT caught: rc={rc} {json.dumps(report)[:300]}")
        codes = [d.get("code") for d in (report.get("newDiagnostics") or [])]
        if "CS0246" not in codes:
            die(f"expected CS0246 for `UndefinedType`, got {codes}")
        diag = (report["newDiagnostics"][0])
        print(f"  caught: {diag.get('code')}: {diag.get('message')}")

        # 3. skipped is never a pass
        report, rc, err = run_diff(binary, baseline, clean,
                                   ("--data", "/nonexistent-csviewer-data"))
        print(f"  exit={rc} ok={report.get('ok')!r} skipped={report.get('skipped')!r} "
              f"toolchain={report.get('toolchain')!r}")
        check_invariant(report, "forced-skip")
        if report.get("skipped") is None:
            die("--data /nonexistent did not skip; the explicit pin was escaped")
        if rc != 3:
            die(f"skipped result must exit 3, got {rc}")
        if report.get("ok") is not False:
            die(f"skipped result must be ok:false, got {report.get('ok')!r}")
        print(f"  skipped is refused as a pass: {report['skipped']}")

        # 4. a non-UTF-8 (Latin-1) source is never silently dropped: the
        #    undefined type inside it must still be caught, not reported clean.
        latin1 = root / "candidate-latin1"
        latin1.mkdir()
        (latin1 / "A.cs").write_text(VALID_A, encoding="utf-8")
        (latin1 / "B.cs").write_bytes(LATIN1_BAD_B)
        report, rc, err = run_diff(binary, baseline, latin1)
        print(f"  exit={rc} ok={report.get('ok')!r} skipped={report.get('skipped')!r} "
              f"new={[d.get('code') for d in (report.get('newDiagnostics') or [])]}")
        check_invariant(report, "latin1 candidate")
        if report.get("skipped") is not None:
            die(f"non-UTF-8 candidate was skipped, not compiled: {report['skipped']}")
        if rc == 0 or report.get("ok") is not False:
            die(f"non-UTF-8 candidate reported as a pass: {json.dumps(report)[:300]}")
        codes = [d.get("code") for d in (report.get("newDiagnostics") or [])]
        if "CS0246" not in codes:
            die(f"the Latin-1 file was not compiled (no CS0246): {codes}")
        print("  non-UTF-8 file compiled; CS0246 caught")

        # 5. a `--data` tree that is not a working compiler must be refused, not
        #    trusted because the two paths exist.
        fake = root / "fakedata"
        (fake / "NetCoreRuntime").mkdir(parents=True)
        (fake / "DotNetSdkRoslyn").mkdir(parents=True)
        shutil.copyfile("/bin/true", fake / "NetCoreRuntime" / "dotnet")
        (fake / "NetCoreRuntime" / "dotnet").chmod(0o755)
        (fake / "DotNetSdkRoslyn" / "csc.dll").write_bytes(b"")
        report, rc, err = run_diff(binary, baseline, clean, ("--data", str(fake)))
        print(f"  exit={rc} ok={report.get('ok')!r} skipped={report.get('skipped')!r}")
        check_invariant(report, "bogus --data")
        if report.get("skipped") is None:
            die("bogus --data was trusted: " + json.dumps(report)[:300])
        if rc != 3 or report.get("ok") is not False:
            die(f"bogus --data must skip rc3, got rc={rc}: {json.dumps(report)[:200]}")
        print(f"  bogus toolchain refused: {report['skipped']}")

    print("PASS: compile-level differential catches new diagnostics, passes clean "
          "edits, never drops a source file, and never treats skipped as a pass")
    return 0


if __name__ == "__main__":
    sys.exit(main())
