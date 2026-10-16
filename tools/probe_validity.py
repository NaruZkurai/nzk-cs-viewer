#!/usr/bin/env python3
"""probe_validity.py — bounded evidence for the M0 Roslyn validity gate.

Runs `csgraph parse-check` (see core/src/compile_check.rs) against:

  1. the `_sealed_src` corpus — a clean run must report `ok` with 0 parse
     diagnostics (or `skipped: <reason>` when no .NET toolchain exists);
  2. a deliberately broken temp file — the gate MUST catch a real Roslyn
     syntax diagnostic and report `ok:false`, never a false `ok`.

The probe fails (exit 1) if the gate ever reports `ok:true` while listing
diagnostics, or if the broken file is not caught. A missing toolchain is an
accepted outcome: it prints `skipped: <reason>` and exits 0.

Bounded: one `csgraph` process per stage, 20 s each, no daemon left running.

Usage: python3 tools/probe_validity.py [--repo DIR] [--bin PATH]
"""

import argparse
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

TIMEOUT = 20  # seconds per stage; the owner rule caps any single op at 30 s


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


def find_repo(crate: Path, override):
    if override:
        p = Path(override).resolve()
        if not (p / "_sealed_src").is_dir():
            die(f"--repo {p} has no _sealed_src/")
        return p
    for base in (crate, crate.parent, crate.parent.parent, crate.parent.parent.parent):
        if (base / "_sealed_src").is_dir():
            return base
    die("could not locate a _sealed_src corpus; pass --repo")


def run_parse_check(binary: Path, argv):
    cmd = [str(binary), "parse-check", *argv]
    try:
        proc = subprocess.run(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=TIMEOUT,
            text=True,
        )
    except subprocess.TimeoutExpired:
        die(f"parse-check exceeded {TIMEOUT}s: {' '.join(cmd)}")
    if not proc.stdout.strip():
        die(f"parse-check produced no stdout (rc={proc.returncode}): {proc.stderr.strip()}")
    try:
        report = json.loads(proc.stdout)
    except json.JSONDecodeError as exc:
        die(f"parse-check stdout is not JSON ({exc}): {proc.stdout[:200]!r}")
    return report, proc.returncode, cmd


def check_no_false_ok(report, where: str):
    """The gate must never claim ok while diagnostics exist, or vice versa."""
    diags = report.get("diagnostics") or []
    ok = report.get("ok")
    skipped = report.get("skipped")
    if skipped is not None:
        if ok is not False:
            die(f"{where}: skipped result must be ok:false, got ok={ok!r}")
        return
    expected = len(diags) == 0
    if ok is not expected:
        die(f"{where}: ok={ok!r} but {len(diags)} diagnostics — false result")
    for d in diags:
        if not d.get("id", "").startswith("CS"):
            die(f"{where}: diagnostic without a Roslyn id: {d!r}")
        if not d.get("severity"):
            die(f"{where}: diagnostic without severity: {d!r}")


def stage_corpus(binary: Path, repo: Path) -> int:
    report, rc, cmd = run_parse_check(
        binary, ["--repo", str(repo), "--root", "_sealed_src", "--json"]
    )
    print(f"$ {' '.join(cmd)}")
    print(f"  exit={rc} toolchain={report.get('toolchain')!r} files={report.get('files')}")
    check_no_false_ok(report, "_sealed_src")
    if report.get("skipped") is not None:
        print(f"skipped: {report['skipped']}")
        return 0
    diags = report["diagnostics"]
    if diags:
        print(f"  {len(diags)} parse diagnostic(s):")
        for d in diags[:10]:
            print(f"  {d['file']}({d['line']},{d['col']}): {d['severity']} {d['id']}: {d['message']}")
        return 1
    print(f"  ok: 0 parse diagnostics in {report['files']} files ({report['toolchain']})")
    return 0


def stage_broken_file(binary: Path, repo: Path) -> int:
    with tempfile.TemporaryDirectory(prefix="probe-validity-") as tmp:
        broken = Path(tmp) / "Broken.cs"
        broken.write_text("namespace N { class C { int X( { return 1; } } }\n", encoding="utf-8")
        report, rc, cmd = run_parse_check(
            binary, ["--repo", tmp, "--files", "Broken.cs", "--json"]
        )
        print(f"$ {' '.join(cmd)}")
        print(f"  exit={rc} toolchain={report.get('toolchain')!r} files={report.get('files')}")
        check_no_false_ok(report, "broken file")
        if report.get("skipped") is not None:
            print(f"skipped: {report['skipped']} (broken file not checked)")
            return 0
        diags = report["diagnostics"]
        if report.get("ok") is not False or not diags:
            die("broken file was NOT caught: " + json.dumps(report))
        first = diags[0]
        print(f"  caught: {first['file']}({first['line']},{first['col']}): "
              f"{first['severity']} {first['id']}: {first['message']}")
        if rc == 0:
            die("broken file caught but exit code was 0")
        return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--repo", help="corpus root containing _sealed_src/")
    ap.add_argument("--bin", help="path to the csgraph binary")
    args = ap.parse_args()

    crate = Path(__file__).resolve().parent.parent
    binary = find_binary(crate, args.bin)
    repo = find_repo(crate, args.repo)

    rc = stage_corpus(binary, repo)
    if rc:
        print("FAIL: _sealed_src has parse diagnostics (see above)", file=sys.stderr)
        return rc

    rc = stage_broken_file(binary, repo)
    if rc:
        print("FAIL: broken-file stage failed", file=sys.stderr)
        return rc

    print("PASS: gate reports real Roslyn diagnostics and never a false ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
