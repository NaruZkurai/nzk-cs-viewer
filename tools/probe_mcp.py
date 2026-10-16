#!/usr/bin/env python3
"""probe_mcp.py — end-to-end proof for `csgraph --mcp`.

Spawns the real built binary and drives it as an MCP client over stdio:

  1. initialize              protocol negotiation + advertised capabilities
  2. tools/list              every /api/* route is declared by some tool
  3. route coverage          each route executes without "unknown endpoint"
  4. read call               a real read against a pre-indexed repo
  5. write call              POST viewer_state, then read it back
  6. concurrency             N reads + ping answered while a slow call is in
                             flight, with measured wall times
  7. lifecycle               stdin EOF exits cleanly; a restarted server answers
                             out of the on-disk cache (fromCache true); SIGTERM
                             kills the process and no server is left behind

No browser, no `csviewer --serve`, no daemon. One server per phase, each one
shut down and reaped before the probe returns.

Usage:
    python3 tools/probe_mcp.py [--bin PATH] [--files N] [--threads N] [--keep]
    python3 tools/probe_mcp.py --shard {read,concurrency,cancel,restart}

The whole probe is one process and runs ~11 s; `--shard NAME` runs one bounded
slice (<10 s) in its own process, and verify_all.sh runs the four slices as
separate units. The union of the slices is exactly the 31 checks above:
read=14, concurrency=5, cancel=9, restart=3.

Exit status 0 only when every check passes.
"""

import argparse
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
CRATE = HERE.parent  # rust-only-csviewer/
# `../rust-env.sh` points CARGO_TARGET_DIR at the builder dir, so the binary
# lands there; fall back to the crate-local target for a plain cargo run.
_TARGET_DIR = Path(os.environ.get("CARGO_TARGET_DIR") or (CRATE.parent / "target"))
DEFAULT_BIN = _TARGET_DIR / "debug" / "csgraph"
API_RS = CRATE / "core" / "src" / "api.rs"

# Routes that need a real argument; everything else gets an empty object. The
# values only need to reach the route — a routed 400/404 is a pass, an
# "unknown endpoint" body is a failure.
ROUTE_ARGS = {
    "/api/browse": {"path": ""},
    "/api/search": {"q": "C"},
    "/api/find": {"q": "C"},
    "/api/refs": {"q": "M0"},
    "/api/node": {"id": "N0.T0_0"},
    "/api/relation": {"node": "N0.T0_0", "key": "calls:out"},
    "/api/source": {"file": "File000.cs", "line": 1},
    "/api/doc": {"node": "N0.T0_0"},
    "/api/chain": {"id": "N0.T0_0"},
    # writes / side-effectful routes get real bodies
    "/api/index": {},
    "/api/index/cancel": {},
    "/api/viewer_state": {"body": {"probe": "route-coverage"}},
    "/api/settings": {"body": {"uiTheme": "probe"}},
    "/api/reorg": {"action": "plan", "spec": {"group": "single"}},
}
POST_ROUTES = {"/api/index", "/api/index/cancel", "/api/viewer_state", "/api/settings", "/api/reorg"}


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
    """One spawned `csgraph --mcp` plus a reader thread so requests overlap."""

    def __init__(self, argv, env):
        self.argv = argv
        self.proc = subprocess.Popen(
            argv,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            text=True,
            bufsize=1,
        )
        self.msgs = {}
        self.notifications = []
        self.malformed = []
        self.stderr_lines = []
        self._cv = threading.Condition()
        self._next = 0
        self._readers = [
            threading.Thread(target=self._read_stdout, daemon=True),
            threading.Thread(target=self._read_stderr, daemon=True),
        ]
        for t in self._readers:
            t.start()

    # -- transport ---------------------------------------------------------
    def _read_stdout(self):
        for line in self.proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                with self._cv:
                    self.malformed.append(line)
                    self._cv.notify_all()
                continue
            with self._cv:
                if isinstance(msg, dict) and msg.get("id") is not None:
                    self.msgs[msg["id"]] = msg
                else:
                    self.notifications.append(msg)
                self._cv.notify_all()

    def _read_stderr(self):
        for line in self.proc.stderr:
            self.stderr_lines.append(line.rstrip("\n"))

    def send(self, obj):
        self.proc.stdin.write(json.dumps(obj) + "\n")
        self.proc.stdin.flush()

    def send_request(self, method, params=None):
        """Queue a request; return (id, t_sent) without waiting for the reply."""
        self._next += 1
        rid = self._next
        msg = {"jsonrpc": "2.0", "id": rid, "method": method}
        if params is not None:
            msg["params"] = params
        self.send(msg)
        return rid, time.monotonic()

    def wait(self, rid, timeout=30.0):
        deadline = time.monotonic() + timeout
        with self._cv:
            while rid not in self.msgs:
                left = deadline - time.monotonic()
                if left <= 0:
                    raise CheckFailed(f"timeout waiting for response id={rid}")
                self._cv.wait(left)
            return self.msgs.pop(rid)

    def request(self, method, params=None, timeout=30.0):
        rid, _ = self.send_request(method, params)
        return self.wait(rid, timeout)

    def notify(self, method, params=None):
        msg = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            msg["params"] = params
        self.send(msg)

    def call_tool(self, name, args=None, timeout=30.0):
        resp = self.request(
            "tools/call", {"name": name, "arguments": args or {}}, timeout
        )
        if "result" not in resp:
            raise CheckFailed(f"tools/call {name} returned an error: {resp}")
        return resp["result"]

    def tool_envelope(self, name, args=None, timeout=30.0):
        """Call a tool and parse the {"status","body"} envelope out of the text."""
        result = self.call_tool(name, args, timeout)
        text = result["content"][0]["text"]
        return json.loads(text), result

    # -- lifecycle ---------------------------------------------------------
    def close_stdin(self):
        try:
            self.proc.stdin.close()
        except Exception:
            pass

    def wait_exit(self, timeout=20.0):
        try:
            return self.proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            return None

    def alive(self):
        return self.proc.poll() is None

    def kill(self):
        if self.alive():
            self.proc.kill()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                pass


def extract_routes(api_rs: Path):
    """Every `/api/...` literal api.rs routes on, minus the test module."""
    src = api_rs.read_text()
    cut = src.find("#[cfg(test)]")
    declared = src[: cut if cut != -1 else len(src)]
    routes = set(re.findall(r'"(/api/[A-Za-z0-9_./-]*)"', declared))
    routes.discard("/api/")
    return sorted(routes)


def make_fixture(root: Path, files: int, types_per_file: int, methods_per_type: int):
    """A deterministic C# corpus, small enough to parse in well under a second."""
    root.mkdir(parents=True, exist_ok=True)
    for i in range(files):
        body = [f"namespace N{i} {{"]
        for t in range(types_per_file):
            body.append(f" public partial class T{i}_{t} {{")
            for m in range(methods_per_type):
                body.append(
                    f"  public int M{m}(int x) {{ return x + {t}; }}"
                )
            body.append(" }")
        body.append("}")
        (root / f"File{i:03d}.cs").write_text("\n".join(body) + "\n")


def invalidate(root: Path):
    """Change every file's size+mtime so the on-disk graph cache misses and the
    next `index` really parses instead of loading."""
    for p in sorted(root.glob("File*.cs")):
        with p.open("a") as f:
            f.write("// invalidate\n")


def run_cancel_checks(probe, mcp, repo, files, threads):
    """index_cancel must interrupt a rebuild, not queue behind its write gate."""
    print("\n[3] index_cancel interrupts an in-flight rebuild", flush=True)
    index_args = {"roots": ["."]}

    # Control: a full cold-cache rebuild, to measure the window cancel must beat.
    invalidate(repo)
    rid, t0 = mcp.send_request("tools/call", {"name": "index", "arguments": index_args})
    ctrl = mcp.wait(rid, timeout=60)
    control_wall = time.monotonic() - t0
    ctrl_env = json.loads(ctrl["result"]["content"][0]["text"])
    probe.check(
        "control rebuild (cold cache) completes normally",
        ctrl_env["status"] == 200
        and ctrl_env["body"]["indexed"] is True
        and not ctrl_env["body"].get("cancelled"),
        f"wall {control_wall:.3f}s fileCount={ctrl_env['body'].get('fileCount')}",
    )

    # Now start another cold rebuild and cancel it while it is in flight.
    invalidate(repo)
    rid, t_rebuild = mcp.send_request("tools/call", {"name": "index", "arguments": index_args})
    time.sleep(0.15)  # land inside the parse window
    cid, t_cancel = mcp.send_request(
        "tools/call", {"name": "index_cancel", "arguments": {}}
    )
    cancel_msg = mcp.wait(cid, timeout=10)
    t_cancel_recv = time.monotonic()
    cancel_wall = t_cancel_recv - t_cancel
    cancel_env = json.loads(cancel_msg["result"]["content"][0]["text"])
    probe.check(
        "index_cancel returns immediately while the rebuild holds the gate",
        cancel_env["status"] == 200
        and cancel_env["body"] == {"ok": True, "cancelled": True}
        and cancel_wall < 0.1,
        f"cancel wall {cancel_wall*1000:.1f} ms (control rebuild {control_wall:.3f}s)",
    )

    rebuild = mcp.wait(rid, timeout=60)
    t_rebuild_recv = time.monotonic()
    rebuild_wall = t_rebuild_recv - t_rebuild
    rebuild_env = json.loads(rebuild["result"]["content"][0]["text"])
    probe.check(
        "the in-flight rebuild returns cancelled:true",
        rebuild_env["status"] == 200 and rebuild_env["body"].get("cancelled") is True,
        f"rebuild wall {rebuild_wall:.3f}s body.cancelled={rebuild_env['body'].get('cancelled')}",
    )
    probe.check(
        "the cancel was answered before the rebuild it interrupted",
        t_cancel_recv < t_rebuild_recv,
        f"cancel at +{t_cancel_recv - t_rebuild:.3f}s, rebuild returned at "
        f"+{t_rebuild_recv - t_rebuild:.3f}s",
    )
    probe.check(
        "cancelling cut the rebuild short",
        rebuild_wall < control_wall,
        f"cancelled rebuild {rebuild_wall:.3f}s < full rebuild {control_wall:.3f}s",
    )

    # The previous graph must still be served, and the stale flag must not
    # block the next build (Index::build clears it at its start).
    state_env, _ = mcp.tool_envelope("state")
    probe.check(
        "reads still answer after a cancelled rebuild",
        state_env["status"] == 200
        and state_env["body"]["indexed"] is True
        and state_env["body"]["fileCount"] == files,
        f"indexed={state_env['body']['indexed']} fileCount={state_env['body']['fileCount']}",
    )

    rid, ts = mcp.send_request("tools/call", {"name": "index", "arguments": index_args})
    redo = mcp.wait(rid, timeout=60)
    redo_wall = time.monotonic() - ts
    redo_env = json.loads(redo["result"]["content"][0]["text"])
    probe.check(
        "a subsequent index starts clean and succeeds",
        redo_env["status"] == 200
        and redo_env["body"]["indexed"] is True
        and not redo_env["body"].get("cancelled"),
        f"wall {redo_wall:.3f}s",
    )

    # Add a file so the rebuilt graph is distinguishable from the pre-cancel one,
    # rebuild, then saturate the pool with concurrent reads. A slot stuck on the
    # old graph would report the old fileCount; every slot must report the new one.
    (repo / "Extra.cs").write_text(
        "namespace ExtraNs { public class Extra { public int E() { return 1; } } }\n"
    )
    expected = files + 1
    rid, ts = mcp.send_request("tools/call", {"name": "index", "arguments": index_args})
    grown = mcp.wait(rid, timeout=60)
    grown_wall = time.monotonic() - ts
    grown_env = json.loads(grown["result"]["content"][0]["text"])
    probe.check(
        "a subsequent index picks up the new file",
        grown_env["status"] == 200 and grown_env["body"]["fileCount"] == expected,
        f"wall {grown_wall:.3f}s fileCount={grown_env['body'].get('fileCount')}",
    )

    ids = []
    for _ in range(threads * 2):
        rid, _ = mcp.send_request(
            "tools/call",
            {"name": "api_call", "arguments": {"method": "GET", "path": "/api/state"}},
        )
        ids.append(rid)
    seen = []
    for rid in ids:
        env = json.loads(mcp.wait(rid, timeout=30)["result"]["content"][0]["text"])
        seen.append((env["status"], env["body"]["indexed"], env["body"]["fileCount"]))
    probe.check(
        "the successful rebuild is visible on every worker slot",
        all(s == (200, True, expected) for s in seen),
        f"{len(seen)} concurrent reads, expected fileCount={expected}, "
        f"distinct answers={sorted(set(seen))}",
    )
    return expected


def tool_route_coverage(routes, tools, probe):
    described = "\n".join(t.get("description", "") for t in tools)
    missing = [r for r in routes if r not in described]
    probe.check(
        "tools/list declares every /api/* route",
        not missing,
        f"{len(routes)} routes, missing={missing}",
    )
    names = {t["name"] for t in tools}
    required = {
        "api_call", "state", "index", "index_cancel", "browse", "search", "node",
        "relation", "find", "refs", "source", "doc", "decls", "chain", "kinds",
        "definitions", "subgraph", "export", "projects", "viewer_state",
        "settings", "reorg",
    }
    probe.check(
        "tools/list has the typed tools plus api_call",
        required <= names,
        f"missing={sorted(required - names)}",
    )


def initialize(mcp, protocol="2024-11-05"):
    """Protocol handshake; shards that do not run the full [1] section use it."""
    mcp.request(
        "initialize",
        {
            "protocolVersion": protocol,
            "capabilities": {},
            "clientInfo": {"name": "probe_mcp", "version": "1"},
        },
    )
    mcp.notify("notifications/initialized")


def checks_protocol_read_write(probe, mcp, repo, threads, files):
    """[1] protocol + tools/list + read/write calls + route coverage.

    Checks 1-11 of the original single-process probe. Needs a live server
    with the startup index in place (no `index` tool call yet)."""
    t0 = time.monotonic()
    init = mcp.request(
        "initialize",
        {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "probe_mcp", "version": "1"},
        },
    )
    init_wall = time.monotonic() - t0
    r = init["result"]
    probe.check(
        "initialize negotiates the client protocol",
        r["protocolVersion"] == "2024-11-05",
        f"protocolVersion={r['protocolVersion']} in {init_wall*1000:.0f} ms",
    )
    probe.check(
        "initialize advertises tools and the worker count",
        r["capabilities"] == {"tools": {}} and r["serverInfo"]["threads"] == threads,
        f"capabilities={r['capabilities']} serverInfo={r['serverInfo']}",
    )
    mcp.notify("notifications/initialized")

    listed = mcp.request("tools/list")["result"]["tools"]
    routes = extract_routes(API_RS)
    tool_route_coverage(routes, listed, probe)
    print(f"       {len(listed)} tools, {len(routes)} routes extracted from api.rs")

    # ---- read against the startup index (no index tool called yet) ----
    envl, _ = mcp.tool_envelope("state")
    probe.check(
        "read call: /api/state served from the startup index",
        envl["status"] == 200
        and envl["body"]["indexed"] is True
        and envl["body"]["fileCount"] == files,
        f"indexed={envl['body']['indexed']} fileCount={envl['body'].get('fileCount')} "
        f"fromCache={envl['body']['cache']['fromCache']}",
    )

    search, _ = mcp.tool_envelope("search", {"q": "M0", "limit": 5})
    probe.check(
        "read call: /api/search returns real nodes",
        search["status"] == 200 and len(search["body"]["results"]) > 0,
        f"{len(search['body']['results'])} results",
    )

    # ---- write call: POST viewer_state, then read it back ------------
    payload = {"probe": "mcp-write", "n": 7}
    w, _ = mcp.tool_envelope("viewer_state", {"body": payload})
    probe.check("write call: POST /api/viewer_state mutates", w["status"] == 200 and w["body"]["ok"], json.dumps(w["body"]))
    back, _ = mcp.tool_envelope("viewer_state", {})
    probe.check(
        "write call: the mutation is visible on the next read",
        back["status"] == 200 and back["body"]["state"].get("probe") == "mcp-write",
        json.dumps(back["body"]["state"]),
    )
    state_file = repo / ".csgraph.v2" / "viewer.json"
    probe.check(
        "write call: the state file on disk changed",
        state_file.is_file() and "mcp-write" in state_file.read_text(),
        str(state_file),
    )

    # ---- route coverage: every route executes, no "unknown endpoint" --
    unknown = []
    for route in routes:
        method = "POST" if route in POST_ROUTES else "GET"
        args = {"method": method, "path": route}
        if route in ROUTE_ARGS:
            args["body" if method == "POST" else "query"] = ROUTE_ARGS[route]
        try:
            envl, _ = mcp.tool_envelope("api_call", args, timeout=30)
        except CheckFailed as e:
            unknown.append(f"{method} {route}: {e}")
            continue
        body = envl.get("body")
        err = body.get("error") if isinstance(body, dict) else None
        if isinstance(err, str) and err.startswith("unknown endpoint"):
            unknown.append(f"{method} {route}: {err}")
    probe.check(
        "route coverage: every route is reachable through api_call",
        not unknown,
        f"{len(routes)} routes probed, failures={unknown}",
    )
    nope, _ = mcp.tool_envelope("api_call", {"method": "GET", "path": "/api/does-not-exist"})
    probe.check(
        "route coverage: an unknown route is still the app's own 404",
        nope["status"] == 404 and "unknown endpoint" in nope["body"]["error"],
        json.dumps(nope["body"]),
    )


def checks_concurrency(probe, mcp, threads):
    """[2] reads and ping answered while a slow call is in flight; checks 12-16."""
    # ---- concurrency: reads + ping while a slow call is in flight -----
    # `/api/export.json` serialises the whole graph (megabytes on this
    # fixture) on ONE worker slot, so it is a real slow read that must not
    # block the other slots or the stdio read loop.
    print("\n[2] concurrency", flush=True)
    slow_id, t_slow = mcp.send_request(
        "tools/call",
        {
            "name": "api_call",
            "arguments": {"method": "GET", "path": "/api/export.json"},
        },
    )
    time.sleep(0.05)  # let the slow call claim a worker slot
    read_ids = []
    for _ in range(12):
        rid, _ = mcp.send_request(
            "tools/call", {"name": "state", "arguments": {}}
        )
        read_ids.append(rid)
    ping_id, _ = mcp.send_request("ping")

    recv = {}
    for rid in read_ids + [ping_id]:
        msg = mcp.wait(rid, timeout=30)
        recv[rid] = time.monotonic()
    slow = mcp.wait(slow_id, timeout=40)
    t_slow_end = time.monotonic()

    slow_wall = t_slow_end - t_slow
    batch_wall = max(recv.values()) - t_slow
    overlaps = sum(1 for rid in read_ids if recv[rid] < t_slow_end)
    ping_overlap = recv[ping_id] < t_slow_end
    per_read = sorted(recv[rid] - t_slow for rid in read_ids)
    median_read = per_read[len(per_read) // 2]
    slow_env = json.loads(slow["result"]["content"][0]["text"])
    slow_text = len(slow["result"]["content"][0]["text"])
    probe.check(
        "slow call completed with a valid response",
        slow.get("result", {}).get("isError") is False and slow_env["status"] == 200,
        f"GET /api/export.json wall {slow_wall:.3f}s, {slow_text} bytes",
    )
    probe.check(
        "reads are answered while the slow call is in flight",
        overlaps >= max(2, threads - 1),
        f"{overlaps}/12 reads finished before the slow call; "
        f"slow={slow_wall:.3f}s batch={batch_wall:.3f}s median_read={median_read*1000:.1f}ms",
    )
    probe.check(
        "ping is answered while the slow call is in flight (read loop not wedged)",
        ping_overlap,
        f"ping answered {recv[ping_id]-t_slow:.1f} ms after dispatch, "
        f"before the {slow_wall:.3f}s slow call returned",
    )
    probe.check(
        "the concurrent batch is not serialised behind the slow call",
        batch_wall < slow_wall,
        f"batch {batch_wall:.3f}s < slow {slow_wall:.3f}s",
    )
    probe.check("every stdout line was valid JSON", not mcp.malformed, f"{len(mcp.malformed)} bad lines")


def checks_lifecycle_eof(probe, mcp):
    """[4] stdin EOF exits cleanly and leaves no process behind; checks 26-28."""
    # ---- shutdown: EOF must exit cleanly and leave nothing behind -----
    print("\n[4] lifecycle", flush=True)
    pid = mcp.proc.pid
    mcp.close_stdin()
    rc = mcp.wait_exit(timeout=20)
    probe.check(
        "stdin EOF exits the server",
        rc is not None and rc == 0,
        f"exit={rc}",
    )
    probe.check("the server process is gone", not mcp.alive(), f"pid={pid}")
    try:
        os.kill(pid, 0)
        gone = False
    except ProcessLookupError:
        gone = True
    probe.check("no process remains after shutdown", gone, f"pid={pid}")


def run_first_server(probe, bin_path, repo, env, threads, files):
    print("\n[1] pre-indexed server + protocol", flush=True)
    argv = [str(bin_path), "--mcp", "--repo", str(repo), "--root", ".", "--threads", str(threads)]
    mcp = McpClient(argv, env)
    try:
        checks_protocol_read_write(probe, mcp, repo, threads, files)
        checks_concurrency(probe, mcp, threads)
        # ---- Stop must interrupt a rebuild, not queue behind it ----------
        expected_files = run_cancel_checks(probe, mcp, repo, files, threads)
        checks_lifecycle_eof(probe, mcp)
        return expected_files
    finally:
        mcp.kill()


def checks_restart_sigterm(probe, mcp, files):
    """[5] restart serves the on-disk cache, then SIGTERM stops it; checks 29-31."""
    mcp.request("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}})
    mcp.notify("notifications/initialized")
    envl, _ = mcp.tool_envelope("state")
    probe.check(
        "restart serves the pre-indexed graph from the on-disk cache",
        envl["status"] == 200
        and envl["body"]["indexed"] is True
        and envl["body"]["fileCount"] == files
        and envl["body"]["cache"]["fromCache"] is True,
        f"fromCache={envl['body']['cache']['fromCache']} fileCount={envl['body']['fileCount']}",
    )

    pid = mcp.proc.pid
    mcp.proc.send_signal(signal.SIGTERM)
    rc = mcp.wait_exit(timeout=15)
    probe.check("SIGTERM terminates the server", rc is not None, f"exit={rc}")
    probe.check("no process remains after SIGTERM", not mcp.alive(), f"pid={pid}")


def run_second_server(probe, bin_path, repo, env, threads, files):
    print("\n[5] restart from the on-disk cache, then SIGTERM", flush=True)
    argv = [str(bin_path), "--mcp", "--repo", str(repo), "--root", ".", "--threads", str(threads)]
    mcp = McpClient(argv, env)
    try:
        checks_restart_sigterm(probe, mcp, files)
    finally:
        mcp.kill()


def run_shard(shard, probe, bin_path, repo, env, threads, files):
    """Run one bounded shard in its own process.

    Each shard owns its server(s) and its fixture, so no unit runs the whole
    11 s sequence. Nothing is lost: the union of the shards' checks is the
    original 31 (read 14, concurrency 5, cancel 9, restart 3).
    """
    argv = [str(bin_path), "--mcp", "--repo", str(repo), "--root", ".", "--threads", str(threads)]
    if shard == "read":
        print("\n[shard read] protocol, read/write calls, route coverage, EOF", flush=True)
        mcp = McpClient(argv, env)
        try:
            checks_protocol_read_write(probe, mcp, repo, threads, files)
            checks_lifecycle_eof(probe, mcp)
        finally:
            mcp.kill()
    elif shard == "concurrency":
        print("\n[shard concurrency] reads + ping during a slow call", flush=True)
        mcp = McpClient(argv, env)
        try:
            initialize(mcp)
            checks_concurrency(probe, mcp, threads)
        finally:
            mcp.kill()
    elif shard == "cancel":
        print("\n[shard cancel] index_cancel interrupts a rebuild", flush=True)
        mcp = McpClient(argv, env)
        try:
            initialize(mcp)
            run_cancel_checks(probe, mcp, repo, files, threads)
        finally:
            mcp.kill()
    elif shard == "restart":
        print("\n[shard restart] restart from the on-disk cache, then SIGTERM", flush=True)
        # Warm the on-disk graph cache with one explicit index, then restart.
        first = McpClient(argv, env)
        try:
            initialize(first)
            first.tool_envelope("index")
        finally:
            first.close_stdin()
            first.wait_exit(timeout=20)
            first.kill()
        mcp = McpClient(argv, env)
        try:
            initialize(mcp, protocol="2025-06-18")
            checks_restart_sigterm(probe, mcp, files)
        finally:
            mcp.kill()
    else:  # pragma: no cover - argparse choices guard this
        raise CheckFailed(f"unknown shard {shard!r}")


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--bin", default=os.environ.get("CSGRAPH_BIN", str(DEFAULT_BIN)))
    ap.add_argument(
        "--files",
        type=int,
        default=None,
        help="fixture files (default 250; the cancel shard defaults to 150 so "
        "its four cold rebuilds stay well under the 10 s unit rule)",
    )
    ap.add_argument("--types", type=int, default=1, help="types per file")
    ap.add_argument("--methods", type=int, default=16, help="methods per type")
    ap.add_argument("--threads", type=int, default=4)
    ap.add_argument("--keep", action="store_true", help="keep the temp fixture repo")
    ap.add_argument(
        "--shard",
        default="all",
        choices=["all", "read", "concurrency", "cancel", "restart"],
        help="run one bounded shard (<10 s each) instead of the whole 31-check "
        "probe; `all` (default) keeps the original single-process sequence",
    )
    args = ap.parse_args()
    if args.files is None:
        args.files = 150 if args.shard == "cancel" else 250

    bin_path = Path(args.bin)
    if not bin_path.is_file():
        print(
            f"probe_mcp: {bin_path} not found; build it first:\n"
            f"  source ../rust-env.sh && cargo build --offline -p csgraph-core",
            file=sys.stderr,
        )
        return 2

    tmp = Path(tempfile.mkdtemp(prefix="csgraph-mcp-probe-"))
    repo = tmp / "repo"
    home = tmp / "home"
    home.mkdir(parents=True)
    env = dict(os.environ)
    env["HOME"] = str(home)  # isolate /api/settings writes from the real config
    probe = Probe()
    t_start = time.monotonic()
    try:
        make_fixture(repo, args.files, args.types, args.methods)
        print(f"fixture: {args.files} files in {repo}"
              + (f" (shard={args.shard})" if args.shard != "all" else ""))
        if args.shard == "all":
            expected = run_first_server(probe, bin_path, repo, env, args.threads, args.files)
            run_second_server(probe, bin_path, repo, env, args.threads, expected)
        else:
            run_shard(args.shard, probe, bin_path, repo, env, args.threads, args.files)
    except CheckFailed:
        pass
    finally:
        if args.keep:
            print(f"kept fixture at {tmp}")
        else:
            shutil.rmtree(tmp, ignore_errors=True)

    passed = sum(1 for _, ok, _ in probe.results if ok)
    failed = [name for name, ok, _ in probe.results if not ok]
    wall = time.monotonic() - t_start
    print("\n" + "=" * 70)
    print(f"probe_mcp: {passed}/{len(probe.results)} checks passed in {wall:.2f}s")
    if failed:
        print("FAILED: " + ", ".join(failed))
        return 1
    print("PROBE OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
