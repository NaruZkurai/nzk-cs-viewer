#!/usr/bin/env python3
"""probe_multirepo.py — R1: one multi-repo MCP server with a per-call `repo`.

Spawns the real `csgraph --mcp` binary over stdio twice:

* a MULTI server bound to two fixture repos with repeated `--repo`;
* a SINGLE server bound to the first fixture repo only.

It checks, in under 10 seconds:

  1. `tools/list` advertises 41 tools and every inputSchema carries a `repo`
     string property (with `additionalProperties` still true);
  2. a call with no `repo` argument answers from the primary graph;
  3. `repo:"2"`, `repo:2` (integer), `repo:"<abs path>"` and a unique basename
     all answer from the second repo;
  4. a routed call with no `repo` (and an explicit `repo` selecting the primary)
     is byte-identical to the single-repo server — the `repo` key never leaks
     into the routed payload;
  5. an unknown `repo` is an `isError` result naming every bound repo;
  6. `index {repo:"2"}` re-indexes the selected repo only: repo 2 picks up a
     newly added file, repo 1's state is byte-identical afterwards and its graph
     does not contain the new declaration;
  7. `workspace` lists both bound repos with index/default/loaded.

No daemon, no `csviewer --serve`: child processes, reaped in `finally`.

Usage:
    python3 tools/probe_multirepo.py [--bin PATH] [--keep]

Exit status 0 only when every check passes.
"""

import argparse
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

TOOL_COUNT = 41
CALL_BUDGET_S = 10.0
PROBE_BUDGET_S = 10.0


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
    """One stdio JSON-RPC child; requests are matched by id on a reader thread."""

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

    def request(self, method, params=None, timeout=CALL_BUDGET_S):
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

    def call(self, name, args=None, timeout=CALL_BUDGET_S):
        t0 = time.monotonic()
        reply = self.request("tools/call", {"name": name, "arguments": args or {}}, timeout)
        wall = time.monotonic() - t0
        if wall > CALL_BUDGET_S:
            raise CheckFailed(f"{name} took {wall:.2f}s (> {CALL_BUDGET_S:.0f}s)")
        if "result" not in reply:
            raise CheckFailed(f"{name} returned a JSON-RPC error: {reply}")
        return reply["result"], wall

    def text(self, name, args=None):
        """The raw MCP text of one tool result (byte-comparable)."""
        result, _ = self.call(name, args)
        return result["content"][0]["text"]

    def tool(self, name, args=None):
        """The parsed JSON body of one tool result."""
        return json.loads(self.text(name, args))

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
            f"probe_multirepo: {bin_path} not found; build first:\n"
            f"  source ../rust-env.sh && cargo build --offline -p csgraph-core",
            file=sys.stderr,
        )
        return 2

    tmp = Path(tempfile.mkdtemp(prefix="csgraph-multirepo-"))
    repo_one = tmp / "repo_one"
    repo_two = tmp / "repo_two"
    (repo_one).mkdir(parents=True)
    (repo_two).mkdir(parents=True)
    (repo_one / "alpha.cs").write_text("namespace One { public class Alpha { } }\n")
    (repo_two / "beta.cs").write_text("namespace Two { public class Beta { } }\n")
    (repo_two / "delta.cs").write_text("namespace Two { public class Delta { } }\n")
    home = tmp / "home"
    home.mkdir()
    env = dict(os.environ)
    env["HOME"] = str(home)

    def server_argv(repos):
        argv = [str(bin_path), "--mcp"]
        for repo in repos:
            argv += ["--repo", str(repo)]
        return argv + ["--root", ".", "--threads", "1"]

    probe = Probe()
    multi = None
    single = None
    t_start = time.monotonic()
    try:
        print("[1] tools/list: 41 tools, every schema carries `repo`", flush=True)
        multi = McpClient(server_argv([repo_one, repo_two]), env)
        single = McpClient(server_argv([repo_one]), env)

        listed = multi.request("tools/list")["result"]["tools"]
        probe.check(
            f"tools/list advertises exactly {TOOL_COUNT} tools",
            len(listed) == TOOL_COUNT,
            f"count={len(listed)}",
        )
        bad = [
            t.get("name")
            for t in listed
            if t.get("inputSchema", {}).get("properties", {}).get("repo", {}).get("type") != "string"
        ]
        no_addl = [
            t.get("name")
            for t in listed
            if t.get("inputSchema", {}).get("additionalProperties") is not True
        ]
        probe.check(
            "every tool schema has a `repo` string property and additionalProperties:true",
            not bad and not no_addl,
            f"missing_repo={bad} missing_additionalProperties={no_addl}",
        )

        print("[2] repo selection: primary, index, path, basename", flush=True)
        primary = multi.tool("state", {})
        primary_again = multi.tool("state", {"repo": "1"})
        probe.check(
            "no `repo` (and `repo:\"1\"`) answers from the primary repo",
            primary["body"]["indexed"] is True
            and primary["body"]["fileCount"] == 1
            and Path(primary["body"]["repo"]).resolve() == repo_one.resolve()
            and primary_again["body"]["repo"] == primary["body"]["repo"],
            f"repo={primary['body']['repo']} files={primary['body']['fileCount']}",
        )

        by_index = multi.tool("state", {"repo": "2"})
        by_int = multi.tool("state", {"repo": 2})
        by_path = multi.tool("state", {"repo": str(repo_two)})
        by_base = multi.tool("state", {"repo": "repo_two"})
        probe.check(
            "`repo` index (string + integer), absolute path and basename all select repo 2",
            all(
                b["body"]["fileCount"] == 2
                and Path(b["body"]["repo"]).resolve() == repo_two.resolve()
                for b in (by_index, by_int, by_path, by_base)
            ),
            f"files={[b['body']['fileCount'] for b in (by_index, by_int, by_path, by_base)]}",
        )

        print("[3] `repo` never leaks into a routed payload", flush=True)
        multi_default = multi.text("search", {"q": "Alpha"})
        single_default = single.text("search", {"q": "Alpha"})
        multi_explicit = multi.text("search", {"q": "Alpha", "repo": 1})
        multi_kinds = multi.text("kinds", {})
        single_kinds = single.text("kinds", {})
        probe.check(
            "a routed call with no `repo` is byte-identical to the single-repo server",
            multi_default == single_default
            and multi_explicit == single_default
            and multi_kinds == single_kinds,
            f"search_equal={multi_default == single_default} "
            f"explicit_primary_equal={multi_explicit == single_default} "
            f"kinds_equal={multi_kinds == single_kinds}",
        )

        print("[4] unknown repo -> isError naming every bound repo", flush=True)
        result, _ = multi.call("state", {"repo": "does-not-exist"})
        message = result["content"][0]["text"]
        probe.check(
            "unknown `repo` is an MCP error naming both bound repos with their indices",
            result.get("isError") is True
            and str(repo_one) in message
            and str(repo_two) in message
            and "1:" in message
            and "2:" in message,
            message,
        )

        print("[5] index {repo:\"2\"} is scoped to repo 2", flush=True)
        repo_one_before = multi.text("state", {"repo": 1})
        (repo_two / "gamma.cs").write_text("namespace Two { public class Gamma { } }\n")
        indexed, _ = multi.call("index", {"repo": "2", "roots": ["."]})
        probe.check(
            "index {repo:\"2\"} succeeds",
            indexed.get("isError") is False,
            indexed["content"][0]["text"][:200],
        )
        repo_two_after = multi.tool("state", {"repo": 2})
        repo_one_after = multi.text("state", {"repo": 1})
        repo_one_search = multi.tool("search", {"q": "Gamma"})
        probe.check(
            "index {repo:\"2\"} re-indexes repo 2 only (repo 1 unchanged, no new symbol)",
            repo_two_after["body"]["fileCount"] == 3
            and repo_one_after == repo_one_before
            and repo_one_search["body"]["results"] == [],
            f"repo2_files={repo_two_after['body']['fileCount']} "
            f"repo1_state_equal={repo_one_after == repo_one_before} "
            f"repo1_gamma_hits={len(repo_one_search['body']['results'])}",
        )

        print("[6] workspace lists every bound repo", flush=True)
        ws = multi.tool("workspace", {})
        repos = ws["repos"]
        probe.check(
            "workspace reports both repos with index/default/loaded",
            len(repos) == 2
            and repos[0]["index"] == 1
            and repos[0]["default"] is True
            and repos[1]["index"] == 2
            and repos[1]["default"] is False
            and repos[1]["loaded"] is True
            and Path(repos[1]["repo"]).resolve() == repo_two.resolve(),
            f"repos={[(r['index'], r['default'], r['loaded']) for r in repos]}",
        )

        wall = time.monotonic() - t_start
        probe.check(
            f"the whole probe is bounded (< {PROBE_BUDGET_S:.0f}s)",
            wall < PROBE_BUDGET_S,
            f"{wall:.2f}s",
        )
    finally:
        for client in (multi, single):
            if client is not None:
                client.kill()
        if not args.keep:
            shutil.rmtree(tmp, ignore_errors=True)
        else:
            print(f"kept {tmp}", file=sys.stderr)

    print("=" * 70)
    print(f"probe_multirepo: {len(probe.results)}/{len(probe.results)} checks passed "
          f"in {time.monotonic() - t_start:.2f}s")
    print("PROBE OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
