#!/usr/bin/env python3
"""probe_copy_recover.py — R2: `copy_path` survives a crash across processes.

The durable-`copy_path` contract (frozen spec, section R2) is that a crashed
apply leaves a fsynced journal plus backup-by-rename targets under
`<repo>/.csgraph-copy/`, and a *second* server process restores the original
bytes through `copy_path {action:"recover", apply:true}`. Recovery follows the
family's dry-run convention: without `apply:true` it returns a read-only plan.

This probe proves exactly that, over the real MCP stdio protocol:

  1. server process A is started with `CSGRAPH_COPY_FAULT=after-rename:1`;
  2. A runs an overwriting `copy_path {apply:true}` that faults after the first
     rename. The result is `{ok:false, fault:"after-rename:1"}` and the crash
     state is left on disk with NO auto-rollback;
  3. A is SIGKILLed — nothing in memory can help;
  4. server process B starts fresh (no fault); `copy_path {action:"recover"}`
     without `apply` reports the plan and changes nothing, then
     `{action:"recover", apply:true}` restores the original bytes;
  5. the original bytes are back, every backup/journal/temp is gone, and a
     second recover is a no-op;
  6. B can still do a normal durable apply afterwards, leaving zero artifacts;
  7. a second pair of processes repeats the test at the *commit* boundary:
     C faults with `after-commit:1` (the fsynced `status:"committed"` marker was
     written and one backup was already deleted), D runs `action:"recover"` and
     proves cleanup-only recovery — the fully-copied bytes and an intentionally
     recreated empty dir survive, no artifacts remain, and a second recover is
     a no-op. This closes the crash window between "all files landed" and
     "journal removed".
  8. journal hardening: a `.csgraph-copy` that is a symlink to an outside dir is
     refused (no journal lands outside), a symlinked `.csgraph-copy/*.json` is
     skipped, and a crafted journal whose *target* is reached through an in-repo
     symlink is refused — no file outside the repo is ever deleted.
  9. hardlink policy: a hardlinked source and a hardlinked target are both
     refused with the offending path named (verified through `st_nlink`), a
     synced (`nlink == 1`) file still copies byte-for-byte on the normal path,
     and the refusals leave no journal behind, so `action:"recover"` stays an
     idempotent no-op.
  10. untrusted journals: a real in-repo `.csgraph-copy/*.json` that is not
      engine-shaped, whose `existed:true` `backup` is not
      `<target>.csgraph-bak-<nonce>`, or whose recorded post-write hash does not
      match the bytes on disk, is refused (incomplete) with `apply:true` and
      deletes nothing; the dry run is likewise a plan only. An engine-shaped
      journal whose hash matches is still honoured.

Bounded: every call under 10 s, total under 10 s.

Usage:
    python3 tools/probe_copy_recover.py [--bin PATH] [--keep]

Exit status 0 only when every check passes.
"""

import argparse
import hashlib
import json
import os
import queue
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
CRATE = HERE.parent
_TARGET_DIR = Path(os.environ.get("CARGO_TARGET_DIR") or (CRATE.parent / "target"))
DEFAULT_BIN = _TARGET_DIR / "debug" / "csgraph"


class CheckFailed(Exception):
    pass


class Probe:
    def __init__(self):
        self.results = []

    def check(self, name, ok, detail=""):
        self.results.append((name, bool(ok), detail))
        mark = "ok  " if ok else "FAIL"
        print(f"  [{mark}] {name}" + (f" — {detail}" if detail else ""), flush=True)
        if not ok:
            raise CheckFailed(name)


class McpClient:
    def __init__(self, argv, env):
        self.proc = subprocess.Popen(
            argv,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            text=True,
            bufsize=1,
        )
        self._q = queue.Queue()
        self._next = 0
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.proc.stdout:
            line = line.strip()
            if line:
                try:
                    self._q.put(json.loads(line))
                except json.JSONDecodeError:
                    pass

    def request(self, method, params=None, timeout=10.0):
        self._next += 1
        rid = self._next
        msg = {"jsonrpc": "2.0", "id": rid, "method": method}
        if params is not None:
            msg["params"] = params
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()
        deadline = time.monotonic() + timeout
        while True:
            left = deadline - time.monotonic()
            if left <= 0:
                raise CheckFailed(f"timeout waiting for {method} id={rid}")
            try:
                reply = self._q.get(timeout=left)
            except queue.Empty:
                raise CheckFailed(f"timeout waiting for {method} id={rid}")
            if reply.get("id") == rid:
                return reply

    def tool(self, name, args=None, timeout=10.0):
        t0 = time.monotonic()
        reply = self.request(
            "tools/call", {"name": name, "arguments": args or {}}, timeout
        )
        wall = time.monotonic() - t0
        if wall > 10.0:
            raise CheckFailed(f"{name} took {wall:.2f}s (> 10s)")
        if "result" not in reply:
            raise CheckFailed(f"{name} returned an error: {reply}")
        body = json.loads(reply["result"]["content"][0]["text"])
        return body, wall

    def initialize(self):
        self.request(
            "initialize",
            {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "probe_copy_recover", "version": "1"},
            },
        )

    def kill(self):
        if self.proc.poll() is None:
            self.proc.kill()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                pass


def walks(root, needle):
    """Every path under `root` whose name contains `needle`."""
    out = []
    for dirpath, dirnames, filenames in os.walk(root):
        for name in list(dirnames) + list(filenames):
            if needle in name:
                out.append(Path(dirpath) / name)
    return sorted(out)


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--bin", default=os.environ.get("CSGRAPH_BIN", str(DEFAULT_BIN)))
    ap.add_argument("--keep", action="store_true")
    args = ap.parse_args()

    bin_path = Path(args.bin)
    if not bin_path.is_file():
        print(
            f"probe_copy_recover: {bin_path} not found; build first:\n"
            f"  source ../rust-env.sh && cargo build --offline -p csgraph-core",
            file=sys.stderr,
        )
        return 2

    tmp = Path(tempfile.mkdtemp(prefix="csgraph-copy-recover-"))
    repo = tmp / "repo"
    repo.mkdir(parents=True)
    original = "namespace R { public class Original { } }\n"
    new_text = "namespace R { public class Replacement { } }\n"
    (repo / "src.cs").write_text(new_text)
    (repo / "dst.cs").write_text(original)
    original_sha = hashlib.sha256(original.encode()).hexdigest()

    home = tmp / "home"
    home.mkdir()
    env = dict(os.environ)
    env["HOME"] = str(home)
    env.pop("CSGRAPH_COPY_FAULT", None)

    def server_argv(repo_path):
        return [
            str(bin_path),
            "--mcp",
            "--repo",
            str(repo_path),
            "--root",
            ".",
            "--threads",
            "1",
        ]

    probe = Probe()
    a = None
    b = None
    c = None
    d = None
    e = None
    f = None
    g = None
    t_start = time.monotonic()
    try:
        print("[1] process A: faulted overwriting apply leaves the crash state", flush=True)
        env_a = dict(env)
        env_a["CSGRAPH_COPY_FAULT"] = "after-rename:1"
        a = McpClient(server_argv(repo), env_a)
        a.initialize()
        body, wall = a.tool(
            "copy_path",
            {
                "from": "src.cs",
                "to": "dst.cs",
                "overwrite": True,
                "apply": True,
            },
        )
        probe.check(
            "copy_path reports the injected fault without rolling back",
            body["ok"] is False
            and body.get("fault") == "after-rename:1"
            and body.get("applied") is False,
            f"fault={body.get('fault')!r} error={body.get('error')!r} "
            f"in {wall*1000:.0f} ms",
        )
        backups = walks(repo, ".csgraph-bak-")
        journals = sorted((repo / ".csgraph-copy").glob("*.json")) if (repo / ".csgraph-copy").is_dir() else []
        temps = walks(repo, ".csgraph-tmp-")
        probe.check(
            "crash state is on disk: target renamed away, one backup and one journal",
            not (repo / "dst.cs").exists()
            and len(backups) == 1
            and len(journals) == 1
            and temps == [],
            f"backups={[p.name for p in backups]} journals={[p.name for p in journals]}",
        )

        # SIGKILL: nothing in A's memory survives; only the journal can help.
        a.kill()
        a = None
        probe.check(
            "process A is gone with the crash state still on disk",
            not (repo / "dst.cs").exists() and len(walks(repo, ".csgraph-bak-")) == 1,
            f"dst exists={ (repo / 'dst.cs').exists() }",
        )

        print("[2] process B: recover is a dry run until apply:true; then restores", flush=True)
        b = McpClient(server_argv(repo), env)
        b.initialize()
        plan, wall = b.tool("copy_path", {"action": "recover"})
        probe.check(
            "recover without apply returns the plan and changes nothing on disk",
            plan.get("ok") is True
            and plan.get("apply") is False
            and plan.get("applied") is False
            and plan.get("dryRun") is True
            and plan.get("journals") == 1
            and plan.get("restored") == 1
            and any(r.get("action") == "restore" for r in (plan.get("plan") or []))
            and not (repo / "dst.cs").exists()
            and len(walks(repo, ".csgraph-bak-")) == 1
            and len(list((repo / ".csgraph-copy").glob("*.json"))) == 1,
            f"apply={plan.get('apply')} journals={plan.get('journals')} "
            f"restored={plan.get('restored')} in {wall*1000:.0f} ms",
        )
        rec, wall = b.tool("copy_path", {"action": "recover", "apply": True})
        probe.check(
            "recover with apply:true restores the journal and reports it applied",
            rec.get("ok") is True
            and rec.get("action") == "recover"
            and rec.get("apply") is True
            and rec.get("applied") is True
            and rec.get("journals") == 1
            and rec.get("restored") == 1
            and rec.get("incomplete") is False,
            f"journals={rec.get('journals')} restored={rec.get('restored')} "
            f"removed={rec.get('removed')} removedDirs={rec.get('removedDirs')} "
            f"in {wall*1000:.0f} ms",
        )
        restored = (repo / "dst.cs").read_bytes() if (repo / "dst.cs").is_file() else b""
        probe.check(
            "a second process restored the original bytes, byte for byte",
            hashlib.sha256(restored).hexdigest() == original_sha
            and restored.decode() == original,
            f"sha={hashlib.sha256(restored).hexdigest()[:12]}…",
        )
        probe.check(
            "recovery leaves no backup, journal or temp artifact",
            walks(repo, ".csgraph-bak-") == []
            and walks(repo, ".csgraph-tmp-") == []
            and not (repo / ".csgraph-copy").exists(),
            f"leftovers={[str(p.relative_to(repo)) for p in walks(repo, '.csgraph-')]}",
        )

        again, _ = b.tool("copy_path", {"action": "recover", "apply": True})
        probe.check(
            "a second recover is an idempotent no-op",
            again.get("ok") is True
            and again.get("journals") == 0
            and again.get("restored") == 0
            and again.get("incomplete") is False,
            f"journals={again.get('journals')} restored={again.get('restored')}",
        )

        print("[3] process B: a normal durable apply leaves zero artifacts", flush=True)
        fresh, wall = b.tool(
            "copy_path",
            {
                "from": "src.cs",
                "to": "dst.cs",
                "overwrite": True,
                "apply": True,
            },
        )
        probe.check(
            "copy_path apply still works after a recover and leaves no artifacts",
            fresh.get("ok") is True
            and fresh.get("applied") is True
            and "unrecovered" not in fresh
            and (repo / "dst.cs").read_text() == new_text
            and walks(repo, ".csgraph-bak-") == []
            and walks(repo, ".csgraph-tmp-") == []
            and not (repo / ".csgraph-copy").exists(),
            f"written={fresh.get('written')} in {wall*1000:.0f} ms",
        )

        print("[3b] hardlink policy: linked source/target refused, plain copy still works", flush=True)
        if not hasattr(os, "link"):
            probe.check("hardlink policy needs os.link", False, "platform has no os.link")
        else:
            # A hardlinked source: pair_b.cs shares pair_a.cs's inode.
            (repo / "pair_a.cs").write_text("// pair\n")
            os.link(repo / "pair_a.cs", repo / "pair_b.cs")
            hard_src, wall = b.tool(
                "copy_path", {"from": "pair_b.cs", "to": "pair_copy.cs"}
            )
            probe.check(
                "a hardlinked source is refused with the path named; nothing written",
                hard_src.get("ok") is False
                and "hardlink" in (hard_src.get("error") or "")
                and "pair_b.cs" in (hard_src.get("error") or "")
                and not (repo / "pair_copy.cs").exists(),
                f"ok={hard_src.get('ok')} error={hard_src.get('error')!r} "
                f"in {wall*1000:.0f} ms",
            )

            # A hardlinked target: hl_target.cs shares keep.cs's inode.
            (repo / "keep.cs").write_text("// keep me\n")
            os.link(repo / "keep.cs", repo / "hl_target.cs")
            hard_dst, wall = b.tool(
                "copy_path",
                {
                    "from": "src.cs",
                    "to": "hl_target.cs",
                    "overwrite": True,
                    "apply": True,
                },
            )
            probe.check(
                "a hardlinked target is refused with the path named; both names intact",
                hard_dst.get("ok") is False
                and "hardlink" in (hard_dst.get("error") or "")
                and "hl_target.cs" in (hard_dst.get("error") or "")
                and (repo / "hl_target.cs").read_text() == "// keep me\n"
                and (repo / "keep.cs").read_text() == "// keep me\n"
                and (repo / "hl_target.cs").stat().st_nlink == 2,
                f"ok={hard_dst.get('ok')} error={hard_dst.get('error')!r} "
                f"in {wall*1000:.0f} ms",
            )

            # A synced (nlink == 1) file still copies byte-for-byte.
            plain, wall = b.tool(
                "copy_path",
                {"from": "src.cs", "to": "fresh.cs", "apply": True},
            )
            probe.check(
                "a normal synced file still copies byte-for-byte and leaves no artifact",
                plain.get("ok") is True
                and plain.get("applied") is True
                and (repo / "fresh.cs").read_text() == new_text
                and walks(repo, ".csgraph-bak-") == []
                and walks(repo, ".csgraph-tmp-") == [],
                f"written={plain.get('written')} in {wall*1000:.0f} ms",
            )

            # The refusals staged nothing; recover stays an idempotent no-op.
            noop, wall = b.tool("copy_path", {"action": "recover", "apply": True})
            probe.check(
                "hardlink refusals leave no journal, so recover stays idempotent",
                noop.get("ok") is True
                and noop.get("journals") == 0
                and noop.get("restored") == 0
                and not (repo / ".csgraph-copy").exists(),
                f"journals={noop.get('journals')} in {wall*1000:.0f} ms",
            )

        print("[4] process C/D: after-commit crash recovers to the committed copy", flush=True)
        repo2 = tmp / "repo2"
        (repo2 / "src" / "empty").mkdir(parents=True)
        (repo2 / "src" / "a.cs").write_text("// new a\n")
        (repo2 / "src" / "zzz.cs").write_text("// new z\n")
        (repo2 / "out").mkdir()
        (repo2 / "out" / "a.cs").write_text("// old a\n")
        (repo2 / "out" / "zzz.cs").write_text("// old z\n")

        env_c = dict(env)
        env_c["CSGRAPH_COPY_FAULT"] = "after-commit:1"
        c = McpClient(server_argv(repo2), env_c)
        c.initialize()
        body, wall = c.tool(
            "copy_path",
            {"from": "src", "to": "out", "overwrite": True, "apply": True},
        )
        probe.check(
            "after-commit fault reports ok:false with the fault name",
            body["ok"] is False and body.get("fault") == "after-commit:1",
            f"fault={body.get('fault')!r} error={body.get('error')!r} "
            f"in {wall*1000:.0f} ms",
        )
        probe.check(
            "committed crash state: both targets hold the new bytes, one backup "
            "already deleted, one left, journal present",
            (repo2 / "out" / "a.cs").read_text() == "// new a\n"
            and (repo2 / "out" / "zzz.cs").read_text() == "// new z\n"
            and len(walks(repo2, ".csgraph-bak-")) == 1
            and (repo2 / ".csgraph-copy").is_dir()
            and len(list((repo2 / ".csgraph-copy").glob("*.json"))) == 1,
            f"backups={[p.name for p in walks(repo2, '.csgraph-bak-')]}",
        )
        c.kill()
        c = None

        d = McpClient(server_argv(repo2), env)
        d.initialize()
        rec, wall = d.tool("copy_path", {"action": "recover", "apply": True})
        probe.check(
            "recovering a committed journal is cleanup-only: no restore, not incomplete",
            rec.get("ok") is True
            and rec.get("action") == "recover"
            and rec.get("journals") == 1
            and rec.get("restored") == 0
            and rec.get("incomplete") is False,
            f"journals={rec.get('journals')} restored={rec.get('restored')} "
            f"removed={rec.get('removed')} in {wall*1000:.0f} ms",
        )
        probe.check(
            "the fully-copied state survives: new bytes and the recreated empty dir",
            (repo2 / "out" / "a.cs").read_text() == "// new a\n"
            and (repo2 / "out" / "zzz.cs").read_text() == "// new z\n"
            and (repo2 / "out" / "empty").is_dir()
            and walks(repo2, ".csgraph-bak-") == []
            and walks(repo2, ".csgraph-tmp-") == []
            and not (repo2 / ".csgraph-copy").exists(),
            f"leftovers={[str(p.relative_to(repo2)) for p in walks(repo2, '.csgraph-')]}",
        )
        again, _ = d.tool("copy_path", {"action": "recover", "apply": True})
        probe.check(
            "a second committed recover is an idempotent no-op",
            again.get("ok") is True
            and again.get("journals") == 0
            and again.get("restored") == 0
            and again.get("incomplete") is False,
            f"journals={again.get('journals')} restored={again.get('restored')}",
        )

        print("[5] journal hardening: symlinked root / file / target never followed", flush=True)
        # F3: <repo>/.csgraph-copy pre-created as a symlink to an outside dir.
        repo3 = tmp / "repo3"
        repo3.mkdir()
        (repo3 / "src.cs").write_text(new_text)
        (repo3 / "dst.cs").write_text(original)
        outside3 = tmp / "outside3"
        outside3.mkdir()
        os.symlink(outside3, repo3 / ".csgraph-copy")
        e = McpClient(server_argv(repo3), env)
        e.initialize()
        r3, wall = e.tool(
            "copy_path",
            {"from": "src.cs", "to": "dst.cs", "overwrite": True, "apply": True},
        )
        probe.check(
            "F3: a symlinked journal root is refused; no journal lands outside the repo",
            r3.get("ok") is False
            and "symlink" in (r3.get("error") or "")
            and (repo3 / "dst.cs").read_text() == original
            and list(outside3.iterdir()) == [],
            f"ok={r3.get('ok')} error={r3.get('error')!r} "
            f"outside={[p.name for p in outside3.iterdir()]} in {wall*1000:.0f} ms",
        )
        e.kill()
        e = None

        # F2 (symlinked journal file) and F1 (journal target through an in-repo
        # symlinked dir) share one repo/server.
        repo4 = tmp / "repo4"
        repo4.mkdir()
        (repo4 / "src.cs").write_text("// keep\n")
        outside4 = tmp / "outside4"
        outside4.mkdir()
        evil = outside4 / "evil.json"
        evil.write_text(
            json.dumps(
                {
                    "version": 1,
                    "nonce": "evil",
                    "status": "pending",
                    "from": "x",
                    "to": "y",
                    "createdDirs": [],
                    "entries": [
                        {
                            "target": "src.cs",
                            "backup": None,
                            "existed": False,
                            "sha256Before": None,
                        }
                    ],
                }
            )
        )
        j4 = repo4 / ".csgraph-copy"
        j4.mkdir()
        os.symlink(evil, j4 / "evil.json")
        f = McpClient(server_argv(repo4), env)
        f.initialize()
        r4, wall = f.tool("copy_path", {"action": "recover", "apply": True})
        probe.check(
            "F2: a symlinked journal file is skipped, so its outside entry is never acted on",
            r4.get("ok") is True
            and r4.get("journals") == 0
            and (repo4 / "src.cs").is_file(),
            f"journals={r4.get('journals')} ok={r4.get('ok')} "
            f"in {wall*1000:.0f} ms",
        )

        outside5 = tmp / "outside5"
        outside5.mkdir()
        victim = outside5 / "victim.txt"
        victim.write_text("PRECIOUS\n")
        os.symlink(outside5, repo4 / "link")
        (j4 / "crafted.json").write_text(
            json.dumps(
                {
                    # Engine-shaped (so this reaches the path walk rather than
                    # the foreign-journal gate): the target is refused because it
                    # is reached through the in-repo `link` symlink.
                    "tool": "copy_path",
                    "version": 1,
                    "nonce": "crafted",
                    "repo": str(repo4.resolve()),
                    "status": "pending",
                    "from": "src.cs",
                    "to": "x",
                    "createdDirs": [],
                    "entries": [
                        {
                            "target": "link/victim.txt",
                            "backup": None,
                            "existed": False,
                            "sha256Before": None,
                            "sha256After": hashlib.sha256(b"PRECIOUS\n").hexdigest(),
                        }
                    ],
                }
            )
        )
        r5, wall = f.tool("copy_path", {"action": "recover", "apply": True})
        probe.check(
            "F1: a journal target reached through an in-repo symlink is refused, "
            "so no file outside the repo is deleted",
            r5.get("ok") is False
            and r5.get("incomplete") is True
            and victim.read_text() == "PRECIOUS\n"
            and (j4 / "crafted.json").is_file(),
            f"ok={r5.get('ok')} incomplete={r5.get('incomplete')} "
            f"victim_gone={not victim.exists()} in {wall*1000:.0f} ms",
        )

        print("[6] untrusted in-repo journals: plan is read-only, apply refuses", flush=True)
        # A real, in-repo `.csgraph-copy/*.json` that is NOT engine-shaped, and
        # one that is engine-shaped but records a post-write hash that does not
        # match the bytes on disk. Neither may ever drive a delete.
        repo6 = tmp / "repo6"
        repo6.mkdir()
        (repo6 / "victim_a.cs").write_text("// keep a\n")
        (repo6 / "victim_b.cs").write_text("// keep b\n")
        j6 = repo6 / ".csgraph-copy"
        j6.mkdir()
        (j6 / "foreign.json").write_text(
            json.dumps(
                {
                    "version": 1,
                    "nonce": "evil",
                    "status": "pending",
                    "from": "x",
                    "to": "y",
                    "createdDirs": [],
                    "entries": [
                        {
                            "target": "victim_a.cs",
                            "backup": None,
                            "existed": False,
                            "sha256Before": None,
                        }
                    ],
                }
            )
        )
        (j6 / "mismatch.json").write_text(
            json.dumps(
                {
                    "tool": "copy_path",
                    "version": 1,
                    "nonce": "mismatch",
                    "repo": str(repo6.resolve()),
                    "status": "pending",
                    "from": "x",
                    "to": "y",
                    "createdDirs": [],
                    "entries": [
                        {
                            "target": "victim_b.cs",
                            "backup": None,
                            "existed": False,
                            "sha256Before": None,
                            "sha256After": hashlib.sha256(b"something else\n").hexdigest(),
                        }
                    ],
                }
            )
        )
        g = McpClient(server_argv(repo6), env)
        g.initialize()
        dry6, wall = g.tool("copy_path", {"action": "recover"})
        probe.check(
            "a crafted in-repo journal: recover without apply is a plan, deletes nothing",
            dry6.get("ok") is False
            and dry6.get("apply") is False
            and dry6.get("incomplete") is True
            and dry6.get("journals") == 2
            and (repo6 / "victim_a.cs").read_text() == "// keep a\n"
            and (repo6 / "victim_b.cs").read_text() == "// keep b\n"
            and (j6 / "foreign.json").is_file()
            and (j6 / "mismatch.json").is_file(),
            f"ok={dry6.get('ok')} incomplete={dry6.get('incomplete')} "
            f"in {wall*1000:.0f} ms",
        )
        applied6, wall = g.tool("copy_path", {"action": "recover", "apply": True})
        probe.check(
            "a crafted in-repo journal with apply is refused/*incomplete*, nothing deleted",
            applied6.get("ok") is False
            and applied6.get("apply") is True
            and applied6.get("applied") is False
            and applied6.get("incomplete") is True
            and applied6.get("restored") == 0
            and applied6.get("removed") == 0
            and (repo6 / "victim_a.cs").read_text() == "// keep a\n"
            and (repo6 / "victim_b.cs").read_text() == "// keep b\n"
            and (j6 / "foreign.json").is_file()
            and (j6 / "mismatch.json").is_file()
            and not (j6 / "resolved.json").exists(),
            f"ok={applied6.get('ok')} incomplete={applied6.get('incomplete')} "
            f"removed={applied6.get('removed')} in {wall*1000:.0f} ms",
        )
        # A fully engine-shaped journal whose created target *does* match its
        # recorded hash is still honoured: this is the legitimate path.
        (j6 / "foreign.json").unlink()
        (j6 / "mismatch.json").unlink()
        (repo6 / "victim_b.cs").write_text("// written by the copy\n")
        (j6 / "resolved.json").write_text(
            json.dumps(
                {
                    "tool": "copy_path",
                    "version": 1,
                    "nonce": "resolved",
                    "repo": str(repo6.resolve()),
                    "status": "pending",
                    "from": "x",
                    "to": "y",
                    "createdDirs": [],
                    "entries": [
                        {
                            "target": "victim_b.cs",
                            "backup": None,
                            "existed": False,
                            "sha256Before": None,
                            "sha256After": hashlib.sha256(
                                b"// written by the copy\n"
                            ).hexdigest(),
                        }
                    ],
                }
            )
        )
        ok6, wall = g.tool("copy_path", {"action": "recover", "apply": True})
        probe.check(
            "an engine-shaped journal whose recorded hash matches is still honoured",
            ok6.get("ok") is True
            and ok6.get("applied") is True
            and ok6.get("removed") == 1
            and not (repo6 / "victim_b.cs").exists()
            and (repo6 / "victim_a.cs").read_text() == "// keep a\n",
            f"ok={ok6.get('ok')} removed={ok6.get('removed')} in {wall*1000:.0f} ms",
        )

        # Variants A/C: engine-shaped except that `backup` names an unrelated
        # in-repo file. Recovery must refuse before anything is moved.
        print("[7] forged backup names (variants A/C) are refused, nothing moved", flush=True)
        (repo6 / "keep.cs").write_text("// keep\n")
        (repo6 / "unrelated.cs").write_text("// unrelated\n")
        (repo6 / "target.cs").write_text("// target\n")
        j6.mkdir(exist_ok=True)
        journal_base = {
            "tool": "copy_path",
            "version": 1,
            "repo": str(repo6.resolve()),
            "status": "pending",
            "from": "x",
            "to": "y",
            "createdDirs": [],
        }
        for tag, backup_name in (("A", "keep.cs"), ("C", "unrelated.cs")):
            forged_entry = {
                "target": "target.cs",
                "backup": backup_name,
                "existed": True,
                "sha256Before": hashlib.sha256(b"// keep\n").hexdigest(),
                "sha256After": hashlib.sha256(b"// target\n").hexdigest(),
            }
            forged_journal = dict(
                journal_base, nonce=f"forge{tag}", entries=[forged_entry]
            )
            (j6 / f"forge{tag}.json").write_text(json.dumps(forged_journal))
        forged, wall = g.tool("copy_path", {"action": "recover", "apply": True})
        probe.check(
            "variant A: backup:\"keep.cs\" is refused; nothing moved or deleted",
            forged.get("ok") is False
            and forged.get("incomplete") is True
            and forged.get("restored") == 0
            and forged.get("removed") == 0
            and (repo6 / "target.cs").read_text() == "// target\n"
            and (repo6 / "keep.cs").read_text() == "// keep\n"
            and (j6 / "forgeA.json").is_file(),
            f"ok={forged.get('ok')} restored={forged.get('restored')} "
            f"removed={forged.get('removed')} in {wall*1000:.0f} ms",
        )
        probe.check(
            "variant C: backup:\"unrelated.cs\" is refused; target and unrelated file intact",
            (repo6 / "unrelated.cs").read_text() == "// unrelated\n"
            and (repo6 / "target.cs").read_text() == "// target\n"
            and (j6 / "forgeC.json").is_file(),
            f"target={(repo6 / 'target.cs').read_text()!r}",
        )
        g.kill()
        g = None
    except CheckFailed:
        pass
    finally:
        if a is not None:
            a.kill()
        if b is not None:
            b.kill()
        if c is not None:
            c.kill()
        if d is not None:
            d.kill()
        if e is not None:
            e.kill()
        if f is not None:
            f.kill()
        if g is not None:
            g.kill()
        if args.keep:
            print(f"kept fixture at {tmp}")
        else:
            shutil.rmtree(tmp, ignore_errors=True)

    wall = time.monotonic() - t_start
    passed = sum(1 for _, ok, _ in probe.results if ok)
    failed = [name for name, ok, _ in probe.results if not ok]
    print("\n" + "=" * 70)
    print(f"probe_copy_recover: {passed}/{len(probe.results)} checks passed in {wall:.2f}s")
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
