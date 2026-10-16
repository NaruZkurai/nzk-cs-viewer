#!/usr/bin/env python3
"""probe_edit_mcp.py — end-to-end proof for the MCP-only edit-tool family.

Spawns the real `csgraph --mcp` binary over stdio and drives the tools that
bypass `/api/*` (`core/src/edit.rs`):

    fuzzy_search, resolve_chain, code_block,
    rename_symbol, qualify_symbols, expand_namespace,
    write_file, apply_edits, select_decls, move_decls, grep,
    reformat, functionize, copy_path

It also drives the aux (non-edit) tools `parse_check` — the Roslyn validity
gate — `find_duplicates` — the dedupe report — `counts` — first-class
corpus/symbol counts, now scoped by `roots`/`exclude`/`file` (gap 4) — and
`workspace` — the bound repo/roots (gap 2).

It checks:

  1. `--mcp --help` exits 0, prints the MCP usage on stdout and starts no server;
  2. tools/list advertises 41 tools (23 routed + 14 edit + 4 aux, the routed set
     including the read-only `preview_change`), including `copy_path`;
  3. the read-only tools return real graph answers;
  4. resolve_chain refuses an ambiguous name and lists candidates;
  5. code_block carries the `// ── node · file:line-endLine ──` part markers;
  6. every write tool is a dry run unless `apply` is passed, and `apply` really
     changes the file on disk through a temp file + rename;
  7. `qualify_symbols` strips usings and fully qualifies a unique type name;
  8. `expand_namespace` plans a stale header back to the graph namespace, and
     with `dest` delegates to the reorg namespace write;
  9. `move_decls` extracts a declaration (with its leading comment) into a new
     re-namespaced file and re-references the call site, and the rename-into form
     `"<Ns>.<Class>.<member>"` creates the class/namespace;
  10. `grep` does regex/literal/glob-scoped search with per-file and total counts,
      `count_only`, and refuses an invalid pattern;
  11. scoped `counts`: `roots` narrows the corpus totals, `exclude` subtracts,
      `byRoot` splits two roots, and the scoped file count is cross-checked
      against the unscoped total by an independent `grep` recount;
  12. `copy_path` end-to-end: dry-run writes nothing; `apply` copies a file and
      the sha256 matches; a recursive directory copy recreates an empty dir and
      reports a skipped symlink; `overwrite:false` on an existing destination
      refuses with `collisions` and leaves the bytes unchanged; `overwrite:true`
      replaces; `to` inside/equal to `from`, `../` and absolute escapes, and a
      symlinked source are refused; a dry run creates no directories;
  13. every tool result text (routed and MCP-only) carries a boolean `ok`;
  14. the whole probe is bounded (no call over 10 s, total under 10 s).
No daemon, no `csviewer --serve`: one child process, reaped in `finally`.

Usage:
    python3 tools/probe_edit_mcp.py [--bin PATH] [--keep]

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

EDIT_TOOLS = {
    "fuzzy_search",
    "resolve_chain",
    "code_block",
    "rename_symbol",
    "qualify_symbols",
    "expand_namespace",
    "write_file",
    "apply_edits",
    "select_decls",
    "move_decls",
    "grep",
    "reformat",
    "functionize",
    "copy_path",
}

AUX_TOOLS = {"parse_check", "find_duplicates", "counts", "workspace"}

# The two root directories used by the scoped-`counts` checks. The fixture files
# below live under `scope_l/` and `scope_r/` so `roots`, `exclude` and `byRoot`
# have a non-trivial partition to work on.
SCOPE_L = "scope_l"
SCOPE_R = "scope_r"

FIXTURE = {
    "a.cs": "namespace A { public class Widget { } }\n",
    "b.cs": (
        "using A;\n"
        "namespace B {\n"
        " public class User {\n"
        "  public Widget W;\n"
        "  public int Hit() { return 1; }\n"
        " }\n"
        "}\n"
    ),
    "c.cs": (
        "namespace N {\n"
        " public class C {\n"
        "  public int A() { return B(); }\n"
        "  public int B() { return 2; }\n"
        " }\n"
        "}\n"
    ),
    "d.cs": (
        "namespace D {\n"
        " public class E {\n"
        "  public void Dup() {}\n"
        "  public int Dup(int x) { return x; }\n"
        " }\n"
        "}\n"
    ),
    "e.cs": "namespace E { public class G { } }\n",
    "f.cs": (
        "namespace F {\n"
        " public class H {\n"
        "  // doc\n"
        "  public int Move() { return 3; }\n"
        "  public int Call() { return Move(); }\n"
        " }\n"
        "}\n"
    ),
    "g.cs": (
        "namespace G {\n"
        " public class K {\n"
        "  public int Zeta() { return 4; }\n"
        "  public int Caller() { return Zeta(); }\n"
        " }\n"
        "}\n"
    ),
    "h.cs": (
        "namespace H {\n"
        " public class P {\n"
        "  public int Gamma() { return 5; }\n"
        "  public int Delta() { return 6; }\n"
        " }\n"
        "}\n"
    ),
    "bad.cs": (
        "namespace N{  public static partial class B{public static bool Eq<T>(T a, T b)"
        "{return System.Object.Equals(a, b);}}}\n"
    ),
    # Two disjoint scope roots for the `counts` scoping checks (D4). Every
    # fixture file under either root contains the word `namespace`, which the
    # independent `grep` recount relies on.
    "scope_l/alpha.cs": (
        "namespace ScopeL {\n"
        " public class Alpha {\n"
        "  public int One() { return 1; }\n"
        " }\n"
        "}\n"
    ),
    "scope_l/beta.cs": "namespace ScopeL { public class Beta { } }\n",
    "scope_r/gamma.cs": (
        "namespace ScopeR {\n"
        " public class Gamma {\n"
        "  public int Two() { return 2; }\n"
        " }\n"
        "}\n"
    ),
}


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

    def kill(self):
        if self.proc.poll() is None:
            self.proc.kill()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                pass


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--bin", default=os.environ.get("CSGRAPH_BIN", str(DEFAULT_BIN)))
    ap.add_argument("--keep", action="store_true")
    args = ap.parse_args()

    bin_path = Path(args.bin)
    if not bin_path.is_file():
        print(
            f"probe_edit_mcp: {bin_path} not found; build first:\n"
            f"  source ../rust-env.sh && cargo build --offline -p csgraph-core",
            file=sys.stderr,
        )
        return 2

    tmp = Path(tempfile.mkdtemp(prefix="csgraph-edit-probe-"))
    repo = tmp / "repo"
    repo.mkdir(parents=True)
    for name, text in FIXTURE.items():
        dest = repo / name
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_text(text)
    home = tmp / "home"
    home.mkdir()
    env = dict(os.environ)
    env["HOME"] = str(home)

    probe = Probe()
    mcp = None
    mcp0 = None
    mcp_real = None
    mcp_sealed = None
    mcp_broken = None
    t_start = time.monotonic()

    def server_argv(repo_path, root="."):
        return [
            str(bin_path),
            "--mcp",
            "--repo",
            str(repo_path),
            "--root",
            root,
            "--threads",
            "1",
        ]

    try:
        print("[0] --mcp --help: usage on stdout, no server", flush=True)
        help_run = subprocess.run(
            [str(bin_path), "--mcp", "--help"],
            capture_output=True,
            text=True,
            env=env,
            stdin=subprocess.DEVNULL,
            timeout=10,
        )
        probe.check(
            "`csgraph --mcp --help` exits 0 with the MCP usage on stdout and starts no server",
            help_run.returncode == 0
            and "usage: csgraph --mcp" in help_run.stdout
            and help_run.stderr == ""
            and "jsonrpc" not in (help_run.stdout + help_run.stderr).lower()
            and all(
                flag in help_run.stdout
                for flag in ("--repo", "--root", "--exclude", "--threads")
            )
            and "tools/list" in help_run.stdout,
            f"exit={help_run.returncode} first={help_run.stdout.splitlines()[:1]}",
        )
        help_short = subprocess.run(
            [str(bin_path), "--mcp", "-h"],
            capture_output=True,
            text=True,
            env=env,
            stdin=subprocess.DEVNULL,
            timeout=10,
        )
        probe.check(
            "`csgraph --mcp -h` takes the same help path",
            help_short.returncode == 0
            and "usage: csgraph --mcp" in help_short.stdout
            and help_short.stderr == "",
            f"exit={help_short.returncode}",
        )
        help_bad = subprocess.run(
            [str(bin_path), "--mcp", "--bogus"],
            capture_output=True,
            text=True,
            env=env,
            stdin=subprocess.DEVNULL,
            timeout=10,
        )
        probe.check(
            "an unknown --mcp option stays exit 2 with usage on stderr",
            help_bad.returncode == 2
            and help_bad.stdout == ""
            and "usage" in help_bad.stderr,
            f"exit={help_bad.returncode} stderr={help_bad.stderr.strip()[:60]!r}",
        )

        neg_threads = subprocess.run(
            [str(bin_path), "--mcp", "--repo", str(repo), "--threads", "-1"],
            capture_output=True,
            text=True,
            env=env,
            stdin=subprocess.DEVNULL,
            timeout=10,
        )
        probe.check(
            "a negative `--threads` is still refused with exit 2",
            neg_threads.returncode == 2,
            f"exit={neg_threads.returncode} stderr={neg_threads.stderr.strip()[:60]!r}",
        )

        print("[0b] --threads 0 = all cores and serves a read tool", flush=True)
        mcp0 = McpClient(
            [
                str(bin_path),
                "--mcp",
                "--repo",
                str(repo),
                "--root",
                ".",
                "--threads",
                "0",
            ],
            env,
        )
        init0 = mcp0.request(
            "initialize",
            {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "probe_edit_mcp", "version": "1"},
            },
        )
        state0, wall = mcp0.tool("state", {})
        probe.check(
            "`--threads 0` resolves to at least one worker and serves a read tool",
            init0["result"]["serverInfo"]["threads"] >= 1
            and state0.get("ok") is True,
            f"threads={init0['result']['serverInfo']['threads']} "
            f"in {wall*1000:.0f} ms",
        )
        mcp0.kill()
        mcp0 = None

        argv = server_argv(repo)
        mcp = McpClient(argv, env)
        print("[1] protocol + tools/list", flush=True)
        init = mcp.request(
            "initialize",
            {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "probe_edit_mcp", "version": "1"},
            },
        )
        instructions = init["result"].get("instructions", "")
        listed = mcp.request("tools/list")["result"]["tools"]
        names = {t["name"] for t in listed}
        routed = names - EDIT_TOOLS - AUX_TOOLS
        probe.check(
            "tools/list advertises 41 tools: 23 routed + 14 edit + 4 aux, including "
            "copy_path and preview_change",
            len(listed) == 41
            and len(EDIT_TOOLS) == 14
            and EDIT_TOOLS <= names
            and AUX_TOOLS <= names
            and len(routed) == 23
            and "copy_path" in names
            and "preview_change" in names,
            f"{len(listed)} tools ({len(routed)} routed), missing={sorted(EDIT_TOOLS - names)}",
        )
        probe.check(
            "initialize instructions state the unified boolean `ok` rule",
            "ok" in instructions and "status" in instructions and "body" in instructions,
            instructions[:80] + ("…" if len(instructions) > 80 else ""),
        )

        print("[2] read-only: fuzzy_search / resolve_chain / code_block", flush=True)
        body, wall = mcp.tool("fuzzy_search", {"q": "N.C.B", "limit": 5})
        probe.check(
            "fuzzy_search finds the exact full name",
            body["ok"]
            and body["results"]
            and body["results"][0]["id"] == "N.C.B()"
            and body["results"][0]["match"] == "full",
            f"first={body['results'][0]['id'] if body['results'] else None} wall {wall*1000:.0f} ms",
        )
        body, _ = mcp.tool("fuzzy_search", {"q": "Widget", "kind": "class"})
        probe.check(
            "fuzzy_search honours the kind filter",
            body["ok"] and body["results"] and all(r["kind"] == "class" for r in body["results"]),
            f"{len(body['results'])} class hits",
        )

        body, _ = mcp.tool("resolve_chain", {"symbol": "N.C.B()", "depth": 2})
        probe.check(
            "resolve_chain resolves an id and returns the chain payload",
            body["ok"] and body["resolved"]["id"] == "N.C.B()" and isinstance(body.get("flat"), list),
            f"resolved={body.get('resolved', {}).get('id')} flat={len(body.get('flat', []))}",
        )
        body, _ = mcp.tool("resolve_chain", {"symbol": "Dup"})
        probe.check(
            "resolve_chain refuses an ambiguous name and lists candidates",
            body["ok"] is False and len(body.get("candidates", [])) >= 2,
            f"candidates={len(body.get('candidates', []))}",
        )

        body, _ = mcp.tool("code_block", {"node": "N.C"})
        text = body.get("text", "")
        probe.check(
            "code_block emits the part marker and the real body",
            body["ok"]
            and "// \u2500\u2500 N.C \u00b7 c.cs:" in text
            and "public int B()" in text
            and body["files"] == ["c.cs"],
            f"{body['lines']} lines, files={body['files']}",
        )
        body, _ = mcp.tool("code_block", {"node": "N.C", "path_only": True})
        probe.check(
            "code_block path_only suppresses the text",
            body["pathOnly"] is True and body["text"] == "",
            f"files={body['files']}",
        )

        print("[3] rename_symbol: dry run, then apply", flush=True)
        before = (repo / "c.cs").read_text()
        body, wall = mcp.tool("rename_symbol", {"from": "N.C.B()", "to": "Beta"})
        probe.check(
            "rename_symbol is a dry run by default",
            body["ok"] and body["applied"] is False and body["edits"]
            and (repo / "c.cs").read_text() == before,
            f"{len(body['edits'])} edits planned in {wall*1000:.0f} ms",
        )
        body, _ = mcp.tool(
            "rename_symbol", {"from": "N.C.B()", "to": "Beta", "apply": True}
        )
        after = (repo / "c.cs").read_text()
        probe.check(
            "rename_symbol apply rewrites the declaration and the call site",
            body["ok"]
            and body["applied"] is True
            and "public int Beta()" in after
            and "return Beta();" in after
            and "int B()" not in after,
            f"written={body['written']}",
        )
        body, _ = mcp.tool("rename_symbol", {"from": "N.C.A()", "to": "1bad"})
        probe.check(
            "rename_symbol refuses an illegal identifier",
            body["ok"] is False,
            body.get("error", ""),
        )

        print("[4] qualify_symbols: usings + full qualification", flush=True)
        b_before = (repo / "b.cs").read_text()
        body, wall = mcp.tool("qualify_symbols", {})
        probe.check(
            "qualify_symbols is a dry run by default",
            body["ok"]
            and body["applied"] is False
            and any(r["line"] == 1 for r in body["remove_usings"])
            and (repo / "b.cs").read_text() == b_before,
            f"{len(body['edits'])} edits in {wall*1000:.0f} ms",
        )
        probe.check(
            "qualify_symbols plans a fully qualified unique type",
            any(
                e["file"] == "b.cs" and "A.Widget W" in e["after"]
                for e in body["edits"]
            ),
            f"skipped_ambiguous={len(body['skipped_ambiguous'])}",
        )
        body, _ = mcp.tool("qualify_symbols", {"apply": True})
        b_after = (repo / "b.cs").read_text()
        probe.check(
            "qualify_symbols apply drops the using and qualifies the field",
            body["ok"] and "using A;" not in b_after and "A.Widget W" in b_after,
            f"written={body['written']}",
        )

        print("[5] expand_namespace: stale header + reorg delegation", flush=True)
        # Make the source drift from the indexed graph on purpose.
        (repo / "e.cs").write_text("namespace Q { public class G { } }\n")
        body, _ = mcp.tool("expand_namespace", {})
        planned = [p for p in body.get("planned", []) if p["file"] == "e.cs"]
        probe.check(
            "expand_namespace is a dry run and plans the graph namespace",
            body["ok"] and body["applied"] is False and planned
            and planned[0]["declared"] == "Q"
            and planned[0]["namespace"] == "E",
            f"planned={[(p['file'], p['declared'], p['namespace']) for p in planned]}",
        )
        body, _ = mcp.tool("expand_namespace", {"apply": True})
        probe.check(
            "expand_namespace apply normalises the stale header",
            body["ok"] and (repo / "e.cs").read_text().startswith("namespace E {"),
            f"written={body['written']}",
        )
        dest = repo / "out"
        body, _ = mcp.tool("expand_namespace", {"dest": str(dest)})
        probe.check(
            "expand_namespace with dest delegates to the reorg namespace write",
            body["ok"] and body["delegated"] is True and dest.is_dir(),
            f"dest={dest}",
        )
        # The reorg copy is a generated tree; drop it so later re-indexes do not
        # merge its duplicate declarations into the fixture's nodes.
        shutil.rmtree(dest, ignore_errors=True)

        print("[6] write_file + apply_edits: create and edit", flush=True)
        content = "namespace G { public class Gadget { } }\n"
        body, wall = mcp.tool(
            "write_file", {"path": "new.cs", "content": content, "create": True}
        )
        probe.check(
            "write_file is a dry run by default",
            body["ok"]
            and body["applied"] is False
            and body["bytes"] == len(content)
            and body["old_sha256"] is None
            and not (repo / "new.cs").exists(),
            f"bytes={body['bytes']} in {wall*1000:.0f} ms",
        )
        body, _ = mcp.tool(
            "write_file",
            {"path": "new.cs", "content": content, "create": True, "apply": True},
        )
        probe.check(
            "write_file apply creates the file atomically",
            body["ok"]
            and body["applied"] is True
            and body["written"] == ["new.cs"]
            and (repo / "new.cs").read_text() == content,
            f"new_sha256={body['new_sha256'][:12]}…",
        )
        body, _ = mcp.tool(
            "write_file",
            {"path": "new.cs", "content": "x", "expected_sha256": "deadbeef"},
        )
        probe.check(
            "write_file refuses a stale expected_sha256",
            body["ok"] is False,
            body.get("error", ""),
        )

        body, wall = mcp.tool(
            "apply_edits",
            {"edits": [{"file": "new.cs", "old": "Gadget", "new": "Widget"}]},
        )
        probe.check(
            "apply_edits is a dry run with a per-edit report",
            body["ok"]
            and body["applied"] is False
            and body["edits"][0]["matched"] == 1
            and body["edits"][0]["replaced"] == 1
            and len(body["edits"][0]["old_sha256"]) == 64
            and "Gadget" in (repo / "new.cs").read_text(),
            f"old_sha256={body['edits'][0]['old_sha256'][:12]}… in {wall*1000:.0f} ms",
        )
        body, _ = mcp.tool(
            "apply_edits",
            {
                "edits": [{"file": "new.cs", "old": "Gadget", "new": "Widget"}],
                "apply": True,
            },
        )
        probe.check(
            "apply_edits apply rewrites the file",
            body["ok"]
            and body["applied"] is True
            and "class Widget" in (repo / "new.cs").read_text(),
            f"written={body['written']}",
        )
        before = (repo / "new.cs").read_text()
        body, _ = mcp.tool(
            "apply_edits",
            {
                "edits": [{"file": "new.cs", "old": "Widget", "new": "X", "count": 3}],
                "apply": True,
            },
        )
        probe.check(
            "apply_edits refuses a wrong match count and writes nothing",
            body["ok"] is False and (repo / "new.cs").read_text() == before,
            body.get("error", ""),
        )

        print("[7] move_decls: extract + rename-into, dry run then apply", flush=True)
        f_before = (repo / "f.cs").read_text()
        body, wall = mcp.tool(
            "move_decls",
            {
                "like": ["Move"],
                "mtype": ["int"],
                "newfile": "NZK.VF.cs",
                "prefix": "NZK.VF.NS",
            },
        )
        content = body.get("destination", {}).get("content", "")
        probe.check(
            "move_decls extract wraps the member, is a dry run and exposes the text",
            body["ok"]
            and body["mode"] == "extract"
            and body["applied"] is False
            and body["reindex_required"] is True
            and body["target"]["class"] == "VF"
            and body["files"] == ["NZK.VF.cs", "f.cs"]
            and not (repo / "NZK.VF.cs").exists()
            and (repo / "f.cs").read_text() == f_before
            and "namespace NZK.VF.NS{  public static partial class VF{" in content
            and content.rstrip().endswith("}}")
            and any(
                e["kind"] == "delete" and e["file"] == "f.cs" and e["from"] == 3
                for e in body["edits"]
            ),
            f"{len(body['edits'])} edits in {wall*1000:.0f} ms",
        )
        body, _ = mcp.tool(
            "move_decls",
            {
                "like": ["Move"],
                "mtype": ["int"],
                "newfile": "NZK.VF.cs",
                "prefix": "NZK.VF.NS",
                "apply": True,
            },
        )
        f_after = (repo / "f.cs").read_text()
        moved_text = (repo / "NZK.VF.cs").read_text()
        probe.check(
            "move_decls apply moves, deletes and re-references",
            body["ok"]
            and body["applied"] is True
            and body["created"] == ["NZK.VF.cs"]
            and "namespace NZK.VF.NS{  public static partial class VF{" in moved_text
            and "// doc" in moved_text
            and "public int Move()" in moved_text
            and "int Move()" not in f_after
            and "NZK.VF.NS.VF.Move()" in f_after,
            f"written={body['written']}",
        )
        probe.check(
            "move_decls apply reports the Roslyn validation outcome",
            body.get("validated") == "roslyn"
            or str(body.get("validated", "")).startswith("skipped:"),
            f"validated={body.get('validated')!r}",
        )
        body, _ = mcp.tool(
            "move_decls",
            {"from": "G.K.Zeta()", "to": "NZK.Q.zeta", "newfile": "home.cs"},
        )
        probe.check(
            "move_decls rename-into accepts a fully-qualified home (dry run)",
            body["ok"]
            and body["mode"] == "rename-into"
            and body["applied"] is False
            and body["target"]["namespace"] == "NZK"
            and body["target"]["class"] == "Q"
            and body["target"]["member"] == "zeta"
            and body["selected"][0]["newFull"] == "NZK.Q.zeta"
            and not (repo / "home.cs").exists(),
            f"files={body['files']}",
        )
        body, _ = mcp.tool(
            "move_decls",
            {
                "from": "G.K.Zeta()",
                "to": "NZK.Q.zeta",
                "newfile": "home.cs",
                "apply": True,
            },
        )
        home = (repo / "home.cs").read_text()
        g_after = (repo / "g.cs").read_text()
        probe.check(
            "move_decls rename-into creates the class/namespace and re-references",
            body["ok"]
            and body["applied"] is True
            and "public class Q {" in home
            and "public int zeta()" in home
            and "NZK.Q.zeta()" in g_after
            and "public int Zeta()" not in g_after,
            f"created={body['created']}",
        )

        print("[7b] move_decls: existing destination + stale `from` (task-14)", flush=True)

        def move_h(member, apply=False, newfile="NZK.H.VF.cs"):
            return {
                "from": f"H.P.{member}()",
                "newfile": newfile,
                "prefix": "NZK.H.NS",
                "apply": apply,
            }

        # 1st move creates the destination.
        body, _ = mcp.tool("move_decls", move_h("Gamma", apply=True))
        probe.check(
            "move_decls creates the destination on the first move",
            body["ok"]
            and body["applied"] is True
            and body["created"] == ["NZK.H.VF.cs"]
            and "public int Gamma()" in (repo / "NZK.H.VF.cs").read_text(),
            f"written={body['written']}",
        )
        # Documented workflow: re-index after apply so line numbers are current.
        mcp.tool("index", {"roots": ["."]})

        # 2nd move, dry run: changes neither the origin nor the existing dest.
        h_src = (repo / "h.cs").read_text()
        h_dest = (repo / "NZK.H.VF.cs").read_text()
        body, _ = mcp.tool("move_decls", move_h("Delta"))
        probe.check(
            "move_decls dry run into an existing destination changes nothing",
            body["ok"]
            and body["applied"] is False
            and "NZK.H.VF.cs" in body["files"]
            and "h.cs" in body["files"]
            and (repo / "h.cs").read_text() == h_src
            and (repo / "NZK.H.VF.cs").read_text() == h_dest,
            f"ok={body.get('ok')} files={body.get('files')} err={body.get('error')}",
        )

        # 2nd move, apply: origin delete AND destination append are one commit.
        body, _ = mcp.tool("move_decls", move_h("Delta", apply=True))
        h_src_after = (repo / "h.cs").read_text()
        h_dest_after = (repo / "NZK.H.VF.cs").read_text()
        probe.check(
            "move_decls appends to an existing destination (no deletion without a write)",
            body["ok"]
            and body["applied"] is True
            and body["created"] == []
            and set(body["written"]) == {"NZK.H.VF.cs", "h.cs"}
            and "public int Gamma()" in h_dest_after
            and "public int Delta()" in h_dest_after
            and "int Gamma()" not in h_src_after
            and "int Delta()" not in h_src_after,
            f"written={body['written']}",
        )

        # Re-running a `from` whose symbol has moved must refuse, not match a
        # sibling by substring, and must touch nothing.
        mcp.tool("index", {"roots": ["."]})
        h_dest2 = (repo / "NZK.H.VF.cs").read_text()
        h_src2 = (repo / "h.cs").read_text()
        body, _ = mcp.tool("move_decls", move_h("Gamma", newfile="NZK.H.VF2.cs"))
        probe.check(
            "move_decls stale `from` refuses with near-matches and no writes",
            body["ok"] is False
            and "does not name a declaration" in body.get("error", "")
            and not (repo / "NZK.H.VF2.cs").exists()
            and (repo / "NZK.H.VF.cs").read_text() == h_dest2
            and (repo / "h.cs").read_text() == h_src2,
            f"near={[n['full'] for n in body.get('near', [])][:2]}",
        )

        print("[7c] move_decls: Roslyn gate on the candidate content", flush=True)
        bad_dest = repo / "NZK.BAD.cs"
        bad_src_before = (repo / "bad.cs").read_text()
        body, wall = mcp.tool(
            "move_decls",
            {
                "from": "N.B.Eq(T, T)",
                "newfile": "NZK.BAD.cs",
                "prefix": "NZK.X.NS",
                "apply": True,
            },
        )
        validated = str(body.get("validated", ""))
        if validated == "roslyn":
            probe.check(
                "move_decls refuses a candidate Roslyn rejects, writing nothing",
                body["ok"] is False
                and len(body.get("diagnostics", [])) >= 1
                and not bad_dest.exists()
                and (repo / "bad.cs").read_text() == bad_src_before,
                f"diagnostics={len(body.get('diagnostics', []))} in {wall*1000:.0f} ms",
            )
        else:
            probe.check(
                "move_decls never claims a pass when validation is skipped",
                validated.startswith("skipped:") and body["ok"] is True,
                f"validated={validated!r}",
            )

        print("[8] move_decls on the real sealed corpus", flush=True)
        real_file = (
            HERE.parents[2] / "_sealed_src" / "NZKNaNimateMeshFolder.cs.nzk"
        )
        if not real_file.is_file():
            probe.check(
                "move_decls real-corpus check skipped (_sealed_src absent)",
                True,
                str(real_file),
            )
        else:
            realrepo = tmp / "real"
            realrepo.mkdir()
            (realrepo / real_file.name).write_text(real_file.read_text())
            nbh_file = HERE.parents[2] / "_sealed_src" / "NBH.fn.cs.nzk"
            if nbh_file.is_file():
                (realrepo / nbh_file.name).write_text(nbh_file.read_text())
            mcp_real = McpClient(server_argv(realrepo), env)
            mcp_real.request(
                "initialize",
                {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "probe_edit_mcp", "version": "1"},
                },
            )
            args_real = {
                "from": "NZK.Core.NZKNaNimateMeshFolder.FolderFor",
                "newfile": "NZK.VF.cs",
                "prefix": "NZK.VF.NS",
            }
            body, wall = mcp_real.tool("move_decls", args_real)
            content = body.get("destination", {}).get("content", "")
            probe.check(
                "move_decls real corpus: class wrapper + re-qualified dependencies (dry run)",
                body["ok"]
                and body["target"]["class"] == "VF"
                and "namespace NZK.VF.NS{  public static partial class VF{" in content
                and "NZK.Core.NZKNaNimateMeshFolder.AvatarRoot(" in content
                and "NZK.Core.NZKNaNimateMeshFolder.LeafNameOf(" in content
                and "NZK.Core.NZKNaNimateMeshFolder.MeshesFolderName" in content
                and not (realrepo / "NZK.VF.cs").exists(),
                f"{len(body.get('edits', []))} edits in {wall*1000:.0f} ms",
            )
            applied, _ = mcp_real.tool(
                "move_decls", {**args_real, "apply": True}
            )
            written = (realrepo / "NZK.VF.cs").read_text()
            probe.check(
                "move_decls real corpus: apply output is wrapped and resolving",
                applied["ok"]
                and applied["applied"] is True
                and "public static partial class VF{" in written
                and "NZK.Core.NZKNaNimateMeshFolder.AvatarRoot(" in written
                and (
                    applied.get("validated") == "roslyn"
                    or str(applied.get("validated", "")).startswith("skipped:")
                ),
                f"created={applied.get('created')} validated={applied.get('validated')!r}",
            )
            if nbh_file.is_file():
                positive, wall = mcp_real.tool(
                    "move_decls",
                    {
                        "from": "NZK.S.NZKNaNimatePrefix(string)",
                        "newfile": "NZK.X1.cs",
                        "prefix": "NZK.X.NS",
                        "apply": True,
                    },
                )
                x1 = realrepo / "NZK.X1.cs"
                probe.check(
                    "move_decls positive end-to-end is Roslyn-clean (validated, no warnings)",
                    positive["ok"]
                    and positive["applied"] is True
                    and positive["unresolved"] == []
                    and positive["warnings"] == []
                    and x1.is_file()
                    and "NZKNaNimatePrefix(string message)" in x1.read_text()
                    and (
                        positive.get("validated") == "roslyn"
                        or str(positive.get("validated", "")).startswith("skipped:")
                    ),
                    f"validated={positive.get('validated')!r} in {wall*1000:.0f} ms",
                )
            grepped, wall = mcp_real.tool(
                "grep", {"pattern": "public static|public class", "glob": "*.nzk"}
            )
            probe.check(
                "grep real corpus: regex alternation returns counted rows",
                grepped["ok"]
                and grepped["total_matches"] > 0
                and any(
                    "NZKNaNimateMeshFolder" in f["file"] for f in grepped["files"]
                )
                and all(
                    "line" in m and "col" in m and "text" in m
                    for f in grepped["files"]
                    for m in f["matches"]
                ),
                f"{grepped['total_matches']} matches in {wall*1000:.0f} ms",
            )

        print("[9] grep: regex/glob/counts over the indexed corpus", flush=True)
        body, wall = mcp.tool(
            "grep", {"pattern": "public static|public class", "glob": "*.cs"}
        )
        probe.check(
            "grep regex alternation is counted and row-shaped (dry, deterministic)",
            body["ok"]
            and body["total_matches"] >= 3
            and body["truncated"] is False
            and all(
                f["matches"] and all(set(m) >= {"line", "col", "text"} for m in f["matches"])
                for f in body["files"]
            ),
            f"{body['total_matches']} matches in {wall*1000:.0f} ms",
        )
        again, _ = mcp.tool(
            "grep", {"pattern": "public static|public class", "glob": "*.cs"}
        )
        probe.check(
            "grep is deterministic",
            again["files"] == body["files"] and again["total_matches"] == body["total_matches"],
            f"files={len(body['files'])}",
        )
        body, _ = mcp.tool(
            "grep", {"pattern": "WIDGET", "glob": "a.cs", "literal": True, "case": False}
        )
        probe.check(
            "grep literal + case-sensitive finds nothing for WIDGET",
            body["ok"] and body["total_matches"] == 0,
            f"total={body['total_matches']}",
        )
        body, _ = mcp.tool(
            "grep",
            {"pattern": "Widget", "literal": True, "case": False, "glob": "a.cs"},
        )
        probe.check(
            "grep literal + case-sensitive finds the exact row",
            body["ok"] and body["total_matches"] == 1 and body["files"][0]["file"] == "a.cs",
            f"{body['files'][0]['matches'] if body['files'] else []}",
        )
        body, _ = mcp.tool("grep", {"pattern": "public", "count_only": True})
        probe.check(
            "grep count_only omits rows but keeps counts",
            body["ok"]
            and body["total_matches"] >= 3
            and all(f["matches"] == [] for f in body["files"]),
            f"{body['total_matches']} matches in {len(body['files'])} files",
        )
        body, _ = mcp.tool("grep", {"pattern": "("})
        probe.check(
            "grep refuses an invalid pattern",
            body["ok"] is False and "invalid pattern" in body.get("error", ""),
            body.get("error", ""),
        )

        print("[10] parse_check: Roslyn validity gate through MCP", flush=True)
        sealed = HERE.parents[2] / "_sealed_src"
        if not sealed.is_dir():
            probe.check(
                "parse_check real-corpus check skipped (_sealed_src absent)",
                True,
                str(sealed),
            )
        else:
            mcp_sealed = McpClient(server_argv(HERE.parents[2], "_sealed_src"), env)
            mcp_sealed.request(
                "initialize",
                {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "probe_edit_mcp", "version": "1"},
                },
            )
            body, wall = mcp_sealed.tool("parse_check", {}, timeout=30)
            diag = body.get("diagnostics")
            skipped = body.get("skipped")
            toolchain = body.get("toolchain")
            probe.check(
                "parse_check on _sealed_src returns the real gate report, never a false ok",
                isinstance(body.get("ok"), bool)
                and isinstance(diag, list)
                and body.get("count") == len(diag)
                and (skipped is not None or isinstance(toolchain, str))
                and (skipped is None or body.get("ok") is False)
                and (
                    skipped is not None
                    or (
                        toolchain.startswith("dotnet-roslyn")
                        and body["files"] == 52
                        and body["ok"] is True
                        and body["count"] == 0
                    )
                ),
                f"ok={body.get('ok')} toolchain={toolchain} files={body.get('files')} "
                f"diagnostics={body.get('count')} skipped={skipped!r} in {wall*1000:.0f} ms",
            )
            # Scoping: a single file still goes through the same gate.
            one, _ = mcp_sealed.tool(
                "parse_check", {"files": ["NZKNaNimateMeshFolder.cs.nzk"]}, timeout=30
            )
            probe.check(
                "parse_check honours the `files` scope",
                one["ok"] is True and one["files"] == 1 and one["count"] == 0,
                f"files={one['files']} skipped={one['skipped']!r}",
            )

        # A syntax error must be caught by the same call (never a false ok).
        broken_repo = tmp / "broken"
        broken_repo.mkdir()
        (broken_repo / "bad.cs").write_text(
            "namespace X {\n public class Bad {\n  public void M() { int y = ; }\n }\n}\n"
        )
        mcp_broken = McpClient(server_argv(broken_repo), env)
        mcp_broken.request(
            "initialize",
            {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "probe_edit_mcp", "version": "1"},
            },
        )
        state = mcp_broken.request("tools/call", {"name": "state", "arguments": {}})
        state_env = json.loads(state["result"]["content"][0]["text"])["body"]
        if state_env.get("indexed"):
            bad, wall = mcp_broken.tool("parse_check", {}, timeout=30)
            probe.check(
                "parse_check catches a real syntax error (ok:false with a diagnostic)",
                bad["ok"] is False
                and bad["skipped"] is None
                and bad["count"] >= 1
                and bad["diagnostics"]
                and {"severity", "id", "file", "line", "col", "message"}
                <= set(bad["diagnostics"][0]),
                f"count={bad['count']} first={bad['diagnostics'][0]['id'] if bad['diagnostics'] else None}",
            )
        else:
            probe.check(
                "parse_check on the broken fixture (indexer rejected it)",
                True,
                "fixture not indexed; diagnostic mapping is covered by compile_check unit tests",
            )

        print("[11] find_duplicates: dedupe report through MCP", flush=True)
        body, wall = mcp.tool("find_duplicates", {}, timeout=30)
        probe.check(
            "find_duplicates returns the dedupe engine report",
            body["ok"] is True
            and isinstance(body.get("files"), int)
            and body["files"] >= 1
            and isinstance(body.get("stats"), dict)
            and isinstance(body.get("truncated"), bool)
            and all(k in body for k in ("exact", "near", "blocks")),
            f"files={body.get('files')} exactClusters={body.get('stats', {}).get('exactClusters')} "
            f"blocks={body.get('stats', {}).get('blockGroups')} in {wall*1000:.0f} ms",
        )

        print("[12] counts: first-class counts (gap 4)", flush=True)
        body, wall = mcp.tool("counts", {})
        probe.check(
            "counts summarises the whole index in one call",
            body["ok"] is True
            and body["scope"] == "corpus"
            and body["nodes"] >= 1
            and body["files"] >= 1
            and isinstance(body["byKind"], dict)
            and isinstance(body["byEdgeKind"], dict)
            and body["declarations"] >= 1,
            f"files={body.get('files')} nodes={body.get('nodes')} edges={body.get('edges')} "
            f"decls={body.get('declarations')} in {wall*1000:.0f} ms",
        )
        body, wall = mcp.tool("counts", {"q": "N.C.Beta()"})
        probe.check(
            "counts gives one symbol's graph counts (definitions/usages/incoming/outgoing)",
            body["ok"] is True
            and body["scope"] == "symbol"
            and body["symbol"]["id"] == "N.C.Beta()"
            and body["definitions"] == 1
            and body["matches"] == 1
            and isinstance(body["incoming"], dict)
            and isinstance(body["outgoing"], dict)
            and body["incomingTotal"] >= 1
            and body["callers"] >= 1
            and body["usages"] >= 1,
            f"{body.get('symbol', {}).get('id')} incoming={body.get('incoming')} "
            f"callers={body.get('callers')} in {wall*1000:.0f} ms",
        )
        body, _ = mcp.tool("counts", {"q": "Dup"})
        probe.check(
            "counts refuses an ambiguous symbol with the candidate rows",
            body["ok"] is False and len(body.get("candidates", [])) >= 2,
            f"candidates={len(body.get('candidates', []))}",
        )

        print("[12b] counts: roots/exclude/file scope + byRoot (D4)", flush=True)
        corpus, _ = mcp.tool("counts", {})
        probe.check(
            "unscoped counts is unchanged: no filter/scoped/byRoot keys",
            corpus["ok"] is True
            and corpus["scope"] == "corpus"
            and "filter" not in corpus
            and "scoped" not in corpus
            and "byRoot" not in corpus
            and isinstance(corpus["roots"], list)
            and corpus["roots"],
            f"roots={corpus.get('roots')} files={corpus.get('files')}",
        )
        left, _ = mcp.tool("counts", {"roots": [SCOPE_L]})
        right, _ = mcp.tool("counts", {"roots": [SCOPE_R]})
        both, wall = mcp.tool("counts", {"roots": [SCOPE_L, SCOPE_R]})
        probe.check(
            "counts roots: narrows the corpus totals, echoes the scope, adds filter/scoped",
            both["ok"] is True
            and both.get("scoped") is True
            and both["scope"] == "corpus"
            and both["files"] < corpus["files"]
            and both["nodes"] <= corpus["nodes"]
            and both["edges"] <= corpus["edges"]
            and both["declarations"] <= corpus["declarations"]
            and sorted(both["roots"]) == [SCOPE_L, SCOPE_R]
            and sorted((both.get("filter") or {}).get("roots") or []) == [SCOPE_L, SCOPE_R]
            and (both.get("filter") or {}).get("exclude") == [],
            f"files {both.get('files')}/{corpus.get('files')} nodes {both.get('nodes')}/"
            f"{corpus.get('nodes')} in {wall*1000:.0f} ms",
        )
        recount, _ = mcp.tool(
            "grep",
            {"pattern": "namespace", "paths": [SCOPE_L, SCOPE_R], "count_only": True},
        )
        probe.check(
            "scoped file count cross-checks an independent grep recount and left+right",
            recount["ok"] is True
            and recount["file_count"] == both["files"]
            and both["files"] == left["files"] + right["files"],
            f"counts={both.get('files')} grep={recount.get('file_count')} "
            f"left={left.get('files')} right={right.get('files')}",
        )
        excl, _ = mcp.tool(
            "counts", {"roots": [SCOPE_L, SCOPE_R], "exclude": [SCOPE_R]}
        )
        probe.check(
            "counts exclude subtracts exactly the excluded root",
            excl["ok"] is True
            and excl.get("scoped") is True
            and excl["files"] == left["files"]
            and excl["files"] == both["files"] - right["files"]
            and (excl.get("filter") or {}).get("exclude") == [SCOPE_R],
            f"excl={excl.get('files')} left={left.get('files')} both={both.get('files')}",
        )
        by_root = both.get("byRoot", {})
        by_root_files = {
            r: by_root.get(r, {}).get("files") for r in (SCOPE_L, SCOPE_R)
        }
        probe.check(
            "counts byRoot splits the requested roots with deterministic key order",
            isinstance(by_root, dict)
            and list(by_root.keys()) == [SCOPE_L, SCOPE_R]
            and all(
                {"files", "nodes", "edges", "declarations"} <= set(by_root[r])
                for r in (SCOPE_L, SCOPE_R)
            )
            and by_root[SCOPE_L]["files"] == left["files"]
            and by_root[SCOPE_R]["files"] == right["files"]
            and by_root_files[SCOPE_L] + by_root_files[SCOPE_R] == both["files"],
            f"byRoot files={by_root_files}",
        )
        file_scope, _ = mcp.tool("counts", {"file": "a.cs"})
        alias, _ = mcp.tool("counts", {"paths": [SCOPE_L]})
        qscoped, _ = mcp.tool("counts", {"q": "ScopeL.Alpha.One()", "roots": [SCOPE_L]})
        probe.check(
            "counts `file` sugar, `paths` alias and q+scope restriction",
            file_scope["ok"] is True
            and file_scope.get("scoped") is True
            and file_scope["files"] == 1
            and alias["ok"] is True
            and alias.get("scoped") is True
            and alias["files"] == left["files"]
            and qscoped["ok"] is True
            and qscoped["scope"] == "symbol"
            and qscoped["definitions"] == 1
            and (qscoped.get("filter") or {}).get("roots") == [SCOPE_L],
            f"file={file_scope.get('files')} alias={alias.get('files')} "
            f"qdefs={qscoped.get('definitions')} qfilter={qscoped.get('filter')}",
        )

        print("[13] copy_path: dry run, file + dir copy, refusals (D1)", flush=True)
        copy_src = repo / "copy_src"
        (copy_src / "sub").mkdir(parents=True)
        (copy_src / "empty").mkdir()
        keep_text = "namespace CS { public class Keep { } }\n"
        nested_text = "namespace CS { public class Nested { } }\n"
        (copy_src / "keep.cs").write_text(keep_text)
        (copy_src / "sub" / "nested.cs").write_text(nested_text)
        os.symlink("keep.cs", copy_src / "link.cs")
        keep_sha = hashlib.sha256(keep_text.encode()).hexdigest()
        nested_sha = hashlib.sha256(nested_text.encode()).hexdigest()

        dest_keep = repo / "copy_out" / "keep.cs"
        plan, wall = mcp.tool(
            "copy_path", {"from": "copy_src/keep.cs", "to": "copy_out/keep.cs"}
        )
        entry = (plan.get("files") or [{}])[0]
        probe.check(
            "copy_path dry run lists the file (sha256, bytes) and creates nothing",
            plan["ok"] is True
            and plan.get("apply") is False
            and plan.get("applied") is False
            and plan.get("kind") == "file"
            and plan.get("overwrite") is False
            and plan.get("written") == []
            and plan.get("totalBytes") == len(keep_text)
            and entry.get("from") == "copy_src/keep.cs"
            and entry.get("to") == "copy_out/keep.cs"
            and entry.get("bytes") == len(keep_text)
            and entry.get("sha256") == keep_sha
            and entry.get("action") == "create"
            and not dest_keep.exists()
            and not (repo / "copy_out").exists(),
            f"files={len(plan.get('files', []))} dirs={plan.get('dirs')} in {wall*1000:.0f} ms",
        )

        applied, _ = mcp.tool(
            "copy_path",
            {"from": "copy_src/keep.cs", "to": "copy_out/keep.cs", "apply": True},
        )
        copied = dest_keep.read_bytes() if dest_keep.is_file() else b""
        probe.check(
            "copy_path apply copies the file and the sha256 matches the bytes on disk",
            applied["ok"] is True
            and applied.get("applied") is True
            and applied.get("written") == ["copy_out/keep.cs"]
            and applied.get("created") == 1
            and applied.get("overwritten") == 0
            and hashlib.sha256(copied).hexdigest() == keep_sha
            and copied.decode() == keep_text
            and "index" in applied.get("note", ""),
            f"written={applied.get('written')} note={applied.get('note')!r}",
        )

        tree, wall = mcp.tool(
            "copy_path", {"from": "copy_src", "to": "copy_all", "apply": True}
        )
        skipped = tree.get("skipped", [])
        probe.check(
            "copy_path recursive dir copy recreates the empty dir and skips the symlink",
            tree["ok"] is True
            and tree.get("applied") is True
            and tree.get("kind") == "dir"
            and (repo / "copy_all" / "empty").is_dir()
            and (repo / "copy_all" / "keep.cs").read_text() == keep_text
            and (repo / "copy_all" / "sub" / "nested.cs").read_text() == nested_text
            and "copy_all/empty" in tree.get("dirs", [])
            and not (repo / "copy_all" / "link.cs").exists()
            and any(
                s.get("reason") == "symlink" and s.get("path", "").endswith("link.cs")
                for s in skipped
            )
            and {f["to"] for f in tree.get("files", [])}
            == {"copy_all/keep.cs", "copy_all/sub/nested.cs"}
            and {f["sha256"] for f in tree.get("files", [])} == {keep_sha, nested_sha},
            f"files={len(tree.get('files', []))} dirs={tree.get('dirs')} "
            f"skipped={skipped} in {wall*1000:.0f} ms",
        )

        before_bytes = dest_keep.read_bytes()
        refuse, _ = mcp.tool(
            "copy_path", {"from": "copy_src/keep.cs", "to": "copy_out/keep.cs"}
        )
        probe.check(
            "copy_path overwrite:false refuses an existing destination with collisions",
            refuse["ok"] is False
            and refuse.get("collisions") == ["copy_out/keep.cs"]
            and dest_keep.read_bytes() == before_bytes,
            f"collisions={refuse.get('collisions')} error={refuse.get('error')!r}",
        )
        refuse_apply, _ = mcp.tool(
            "copy_path",
            {"from": "copy_src/keep.cs", "to": "copy_out/keep.cs", "apply": True},
        )
        probe.check(
            "copy_path overwrite:false apply leaves the bytes unchanged",
            refuse_apply["ok"] is False
            and dest_keep.read_bytes() == before_bytes,
            f"collisions={refuse_apply.get('collisions')}",
        )
        dest_keep.write_text("stale\n")
        replaced, _ = mcp.tool(
            "copy_path",
            {
                "from": "copy_src/keep.cs",
                "to": "copy_out/keep.cs",
                "overwrite": True,
                "apply": True,
            },
        )
        probe.check(
            "copy_path overwrite:true replaces the colliding file",
            replaced["ok"] is True
            and replaced.get("applied") is True
            and replaced.get("overwritten") == 1
            and replaced.get("created") == 0
            and dest_keep.read_text() == keep_text,
            f"overwritten={replaced.get('overwritten')} created={replaced.get('created')}",
        )

        inside, _ = mcp.tool("copy_path", {"from": "copy_src", "to": "copy_src/inner"})
        same, _ = mcp.tool(
            "copy_path", {"from": "copy_src/keep.cs", "to": "copy_src/keep.cs"}
        )
        probe.check(
            "copy_path refuses `to` inside or equal to `from`",
            inside["ok"] is False
            and same["ok"] is False
            and not (copy_src / "inner").exists(),
            f"inside={inside.get('error')!r} same={same.get('error')!r}",
        )
        outside = tmp / "outside.txt"
        outside.write_text("outside\n")
        esc_from, _ = mcp.tool(
            "copy_path", {"from": "../outside.txt", "to": "escaped_in.cs"}
        )
        esc_to, _ = mcp.tool(
            "copy_path", {"from": "copy_src/keep.cs", "to": "../escaped_out.cs"}
        )
        esc_abs, _ = mcp.tool("copy_path", {"from": str(outside), "to": "abs_in.cs"})
        probe.check(
            "copy_path refuses `../` and absolute paths outside the repo",
            esc_from["ok"] is False
            and esc_to["ok"] is False
            and esc_abs["ok"] is False
            and not (repo / "escaped_in.cs").exists()
            and not (repo / "abs_in.cs").exists()
            and not (tmp / "escaped_out.cs").exists(),
            f"from={esc_from.get('error')!r} to={esc_to.get('error')!r} "
            f"abs={esc_abs.get('error')!r}",
        )
        sym_source, _ = mcp.tool(
            "copy_path", {"from": "copy_src/link.cs", "to": "link_copy.cs"}
        )
        probe.check(
            "copy_path refuses a symlinked source",
            sym_source["ok"] is False and not (repo / "link_copy.cs").exists(),
            f"error={sym_source.get('error')!r}",
        )

        print("[14] workspace: the bound repo/roots and the bound set", flush=True)
        body, wall = mcp.tool("workspace", {})
        probe.check(
            "workspace names the bound repo/roots and reports the bound set",
            body["ok"] is True
            and body["servers"] != "one repo per process"
            and "1 bound repo" in body["servers"]
            and body["repo"]
            and isinstance(body["roots"], list)
            and body["indexed"] is True
            and "`repo` argument" in body["limitation"]
            and isinstance(body["repos"], list)
            and len(body["repos"]) == 1
            and body["repos"][0]["default"] is True
            and body["repos"][0]["repo"] == body["repo"]
            and body["repos"][0]["index"] == 1
            and "repo" in body["scoping"]["repoSelector"]
            and "index {roots:" in body["scoping"]["reScope"],
            f"repo={body.get('repo')} roots={body.get('roots')} in {wall*1000:.0f} ms",
        )

        print("[15] envelope: boolean ok on routed and MCP-only results (D3)", flush=True)
        probe.check(
            "an MCP-only tool result carries a boolean top-level ok",
            isinstance(body.get("ok"), bool),
            f"ok={body.get('ok')!r}",
        )
        ok_env, _ = mcp.tool("state", {})
        probe.check(
            "a routed success carries status/ok/body with ok == status < 400",
            isinstance(ok_env.get("status"), int)
            and isinstance(ok_env.get("ok"), bool)
            and isinstance(ok_env.get("body"), dict)
            and ok_env["status"] == 200
            and ok_env["ok"] == (ok_env["status"] < 400),
            f"status={ok_env.get('status')} ok={ok_env.get('ok')!r}",
        )
        err_env, _ = mcp.tool("api_call", {"path": "/api/definitely-not-a-route"})
        probe.check(
            "a routed refusal carries ok:false with a status >= 400",
            err_env.get("ok") is False
            and isinstance(err_env.get("status"), int)
            and err_env["status"] >= 400
            and err_env["ok"] == (err_env["status"] < 400),
            f"status={err_env.get('status')} ok={err_env.get('ok')!r}",
        )
        for routed_name in ("browse", "kinds"):
            row, _ = mcp.tool(routed_name, {})
            probe.check(
                f"routed `{routed_name}` also carries status/ok/body",
                isinstance(row.get("status"), int)
                and isinstance(row.get("ok"), bool)
                and isinstance(row.get("body"), (dict, list))
                and row["ok"] == (row["status"] < 400),
                f"status={row.get('status')} ok={row.get('ok')!r}",
            )
    except CheckFailed:
        pass
    finally:
        if mcp is not None:
            mcp.kill()
        if mcp0 is not None:
            mcp0.kill()
        if mcp_real is not None:
            mcp_real.kill()
        if mcp_sealed is not None:
            mcp_sealed.kill()
        if mcp_broken is not None:
            mcp_broken.kill()
        if args.keep:
            print(f"kept fixture at {tmp}")
        else:
            shutil.rmtree(tmp, ignore_errors=True)

    wall = time.monotonic() - t_start
    passed = sum(1 for _, ok, _ in probe.results if ok)
    failed = [name for name, ok, _ in probe.results if not ok]
    print("\n" + "=" * 70)
    print(f"probe_edit_mcp: {passed}/{len(probe.results)} checks passed in {wall:.2f}s")
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
