#!/usr/bin/env python3
"""Minimal newline-delimited JSON-RPC 2.0 client for `csgraph --mcp`.

Used to drive the csgraph MCP server from scripts and from the shell, so every
change to a corpus can be made through the MCP surface instead of by editing
files directly.

Usage:
  tools/mcp_drive.py --repo /path/to/repo --root _sealed_src tools
  tools/mcp_drive.py ... call fuzzy_search '{"q":"Normalise","limit":5}'
  tools/mcp_drive.py ... batch '[
      {"name":"state","arguments":{}},
      {"name":"find","arguments":{"q":"NZK.Core.MA"}}
  ]'
  tools/mcp_drive.py ... session        # reads one JSON request per line on stdin

Options:
  --bin PATH     csgraph binary (default: <crate>/../target/debug/csgraph)
  --root REL     pre-index root; repeatable (default: none = cold start)
  --threads N    worker slots (default 4)
  --timeout SEC  per-call timeout (default 60); calls are bounded on purpose.
  --raw          print the full JSON-RPC envelope instead of just the tool text
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import threading
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
DEFAULT_BIN = HERE.parent.parent / "target" / "debug" / "csgraph"


class McpError(RuntimeError):
    pass


class McpClient:
    """Spawns the server, speaks one JSON message per line, and always tears it down."""

    def __init__(self, binary: Path, repo: Path, roots, threads=4, timeout=60.0, extra=()):
        argv = [str(binary), "--mcp", "--repo", str(repo), "--threads", str(threads)]
        for r in roots:
            argv += ["--root", r]
        argv += list(extra)
        self.argv = argv
        self.timeout = timeout
        self._next_id = 1
        self._lock = threading.Lock()
        self.stderr_path = Path("/tmp/csgraph-mcp-stderr.log")
        self._stderr = open(self.stderr_path, "wb")
        # stderr is never merged into stdout: stdout is protocol-only.
        self.proc = subprocess.Popen(
            argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self._stderr
        )

    def _send(self, obj) -> None:
        line = json.dumps(obj, separators=(",", ":")) + "\n"
        self.proc.stdin.write(line.encode("utf-8"))
        self.proc.stdin.flush()

    def _read_line(self) -> dict:
        while True:
            raw = self.proc.stdout.readline()
            if not raw:
                raise McpError("server closed stdout (see %s)" % self.stderr_path)
            text = raw.decode("utf-8", "replace").strip()
            if not text:
                continue
            try:
                return json.loads(text)
            except json.JSONDecodeError:
                raise McpError("non-JSON line on stdout: %r" % text[:200])

    def request(self, method: str, params=None) -> dict:
        with self._lock:
            rid = self._next_id
            self._next_id += 1
            msg = {"jsonrpc": "2.0", "id": rid, "method": method}
            if params is not None:
                msg["params"] = params
            self._send(msg)
        deadline = time.monotonic() + self.timeout
        while True:
            if time.monotonic() > deadline:
                raise McpError("timeout waiting for id=%s" % rid)
            resp = self._read_line()
            if resp.get("id") == rid:
                if "error" in resp:
                    raise McpError("jsonrpc error: %s" % json.dumps(resp["error"]))
                return resp

    def notify(self, method: str, params=None) -> None:
        msg = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            msg["params"] = params
        with self._lock:
            self._send(msg)

    def initialize(self) -> dict:
        res = self.request(
            "initialize",
            {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "mcp_drive.py", "version": "1.0"},
            },
        )
        self.notify("notifications/initialized")
        return res["result"]

    def tools(self) -> list:
        return self.request("tools/list")["result"]["tools"]

    def call(self, name: str, arguments=None) -> dict:
        """Returns the raw JSON-RPC result (content blocks + isError)."""
        return self.request(
            "tools/call", {"name": name, "arguments": arguments or {}}
        )["result"]

    def call_body(self, name: str, arguments=None):
        """Returns the parsed `body` of the {status, body} envelope, or raises."""
        res = self.call(name, arguments)
        text = res["content"][0]["text"] if res.get("content") else ""
        try:
            env = json.loads(text)
        except json.JSONDecodeError:
            raise McpError("tool %s returned non-JSON: %r" % (name, text[:200]))
        if res.get("isError"):
            raise McpError("tool %s error (status %s): %s" % (name, env.get("status"), text[:400]))
        return env.get("body")

    def close(self) -> int:
        try:
            if self.proc.stdin and not self.proc.stdin.closed:
                self.proc.stdin.close()
            try:
                return self.proc.wait(timeout=15)
            except subprocess.TimeoutExpired:
                self.proc.terminate()
                return self.proc.wait(timeout=10)
        finally:
            self._stderr.close()

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bin", default=str(DEFAULT_BIN))
    ap.add_argument("--repo", default=str(HERE.parent.parent))
    ap.add_argument("--root", action="append", default=[])
    ap.add_argument("--threads", type=int, default=4)
    ap.add_argument("--timeout", type=float, default=60.0)
    ap.add_argument("--raw", action="store_true")
    ap.add_argument("command", choices=["tools", "call", "batch", "session", "info"])
    ap.add_argument("arg", nargs="?")
    ap.add_argument("json", nargs="?")
    ns = ap.parse_args(argv)

    binary = Path(ns.bin).expanduser()
    if not binary.exists():
        print("csgraph binary not found: %s" % binary, file=sys.stderr)
        return 2

    with McpClient(binary, Path(ns.repo), ns.root, ns.threads, ns.timeout) as c:
        info = c.initialize()
        if not ns.raw:
            print(
                "# connected to %s v%s threads=%s"
                % (info["serverInfo"]["name"], info["serverInfo"]["version"], info["serverInfo"].get("threads")),
                file=sys.stderr,
            )
        if ns.command == "info":
            print(json.dumps(info, indent=2))
            return 0
        if ns.command == "tools":
            tools = c.tools()
            if ns.raw:
                print(json.dumps(tools, indent=2))
            else:
                for t in tools:
                    print(t["name"])
                print("# %d tools" % len(tools), file=sys.stderr)
            return 0
        if ns.command == "call":
            args = json.loads(ns.json) if ns.json else {}
            res = c.call(ns.arg, args)
            print(json.dumps(res, indent=2) if ns.raw else res["content"][0]["text"])
            return 1 if res.get("isError") else 0
        if ns.command == "batch":
            if ns.arg and Path(ns.arg).is_file():
                reqs = json.loads(Path(ns.arg).read_text())
            elif ns.arg:
                reqs = json.loads(ns.arg)
            else:
                reqs = json.load(sys.stdin)
            for r in reqs:
                res = c.call(r["name"], r.get("arguments", {}))
                print(json.dumps({"tool": r["name"], "result": res}, separators=(",", ":")))
            return 0
        if ns.command == "session":
            for line in sys.stdin:
                line = line.strip()
                if not line:
                    continue
                req = json.loads(line)
                res = c.call(req["name"], req.get("arguments", {}))
                print(json.dumps({"tool": req["name"], "result": res}, separators=(",", ":")))
                sys.stdout.flush()
            return 0
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except McpError as e:
        print("mcp_drive: %s" % e, file=sys.stderr)
        sys.exit(3)
