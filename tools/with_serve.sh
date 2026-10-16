#!/usr/bin/env bash
# =============================================================================
#  with_serve.sh — run ONE bounded frontend probe against a private engine.
# =============================================================================
#
#  `csviewer --serve` is a daemon: left running it exceeds any per-task time
#  budget. This wrapper makes it a *fixture* instead — it picks private ports,
#  boots the real Rust engine + frontend, waits for `/api/state`, runs exactly
#  one command with the URL already wired to it, and always tears the server
#  down (including on Ctrl-C or a failing probe).
#
#  Usage:
#    tools/with_serve.sh -- node tools/probe_features.mjs
#    tools/with_serve.sh --port 8795 -- node tools/headless_check.mjs --node '#ns:NZK'
#    tools/with_serve.sh --set-define WIP -- node tools/probe_language.mjs
#
#  Options:
#    --port N        engine port (default: an unused port chosen by python)
#    --cdp-port N    Chrome DevTools port for the probe (default: unused)
#    --repo DIR      repository to index (default: the checkout above this tree)
#    --root REL      root inside the repo (default: _sealed_src; repeatable)
#    --bin PATH      csviewer binary (default: <builder>/target/debug/csviewer)
#    --timeout N     seconds to wait for /api/state (default 90)
#    --keep-log      keep the server log even on success (default: keep on fail)
#    --              end of wrapper options; everything after is the command
#
#  The command runs with these in its environment:
#    URL          http://127.0.0.1:<port>/
#    PORT         the engine port
#    CDP_PORT     a free port for a headless Chrome
#    SERVE_LOG    the engine's log file
#
#  If the command does not mention `--url`/`--cdp-port`, both are appended, so
#  the tools in this directory need no changes:
#    tools/with_serve.sh -- node tools/probe_features.mjs
#
#  Exit status is the command's; 124 means the engine never answered.
# =============================================================================

set -uo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
ROOT="$(cd -- "$HERE/.." && pwd -P)"              # rust-only-csviewer/
BUILDER="$(cd -- "$ROOT/.." && pwd -P)"           # "cs viewer builder/"
DEFAULT_REPO="$(cd -- "$BUILDER/.." && pwd -P)"   # the vrcCS checkout

PORT=""
CDP_PORT=""
REPO="$DEFAULT_REPO"
BIN="$BUILDER/target/debug/csviewer"
TIMEOUT=90
KEEP_LOG=0
declare -a ROOTS=()

while [ $# -gt 0 ]; do
  case "$1" in
    --port) PORT="$2"; shift 2 ;;
    --cdp-port) CDP_PORT="$2"; shift 2 ;;
    --repo) REPO="$2"; shift 2 ;;
    --root) ROOTS+=("$2"); shift 2 ;;
    --bin) BIN="$2"; shift 2 ;;
    --timeout) TIMEOUT="$2"; shift 2 ;;
    --keep-log) KEEP_LOG=1; shift ;;
    --) shift; break ;;
    -h|--help) sed -n '2,40p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "with_serve: unknown option $1 (use -- before the command)" >&2; exit 2 ;;
  esac
done

if [ $# -eq 0 ]; then
  echo "with_serve: no command given; usage: tools/with_serve.sh -- <command...>" >&2
  exit 2
fi

if [ "${#ROOTS[@]}" -eq 0 ]; then
  ROOTS=("_sealed_src")
fi

if [ ! -x "$BIN" ]; then
  echo "with_serve: $BIN is not executable; build it with" >&2
  echo "  source \"$BUILDER/rust-env.sh\" && cargo build --offline -p csviewer" >&2
  exit 2
fi

free_port() {
  python3 - <<'PY'
import socket
s = socket.socket()
s.bind(('127.0.0.1', 0))
print(s.getsockname()[1])
s.close()
PY
}

[ -n "$PORT" ] || PORT="$(free_port)"
[ -n "$CDP_PORT" ] || CDP_PORT="$(free_port)"

SERVE_LOG="${TMPDIR:-/tmp}/csviewer-serve-$PORT.log"
: > "$SERVE_LOG"

declare -a SRV_CMD=("$BIN" --serve --port "$PORT" --repo "$REPO")
for r in "${ROOTS[@]}"; do SRV_CMD+=(--root "$r"); done

"${SRV_CMD[@]}" >"$SERVE_LOG" 2>&1 &
SRV=$!

cleanup() {
  if kill -0 "$SRV" 2>/dev/null; then
    kill -TERM "$SRV" 2>/dev/null
    for _ in $(seq 1 30); do
      kill -0 "$SRV" 2>/dev/null || break
      sleep 0.1
    done
    kill -KILL "$SRV" 2>/dev/null
  fi
  wait "$SRV" 2>/dev/null
}
trap cleanup EXIT INT TERM

BASE="http://127.0.0.1:$PORT"
URL="$BASE/"
# NB: the engine 404s on a doubled slash (`//api/state`), so health-check the
# base URL without the trailing slash the frontend wants.
ready=0
for _ in $(seq 1 "$TIMEOUT"); do
  if ! kill -0 "$SRV" 2>/dev/null; then
    echo "with_serve: engine exited early; log follows" >&2
    tail -n 40 "$SERVE_LOG" >&2
    exit 2
  fi
  if curl -sf -o /dev/null "$BASE/api/state"; then ready=1; break; fi
  sleep 1
done

if [ "$ready" != 1 ]; then
  echo "with_serve: /api/state did not answer within ${TIMEOUT}s; log follows" >&2
  tail -n 40 "$SERVE_LOG" >&2
  exit 124
fi

export URL PORT CDP_PORT SERVE_LOG

# Wire the probe to the fixture unless it was already told where to look.
declare -a CMD=("$@")
for a in "${CMD[@]}"; do
  case "$a" in
    --url|--url=*) URL_GIVEN=1 ;;
    --cdp-port|--cdp-port=*) CDP_GIVEN=1 ;;
  esac
done
if [ "${URL_GIVEN:-0}" != 1 ]; then CMD+=(--url "$URL"); fi
if [ "${CDP_GIVEN:-0}" != 1 ]; then CMD+=(--cdp-port "$CDP_PORT"); fi

echo "with_serve: engine pid=$SRV url=$URL cdp=$CDP_PORT log=$SERVE_LOG"
"${CMD[@]}"
rc=$?

if [ "$rc" -ne 0 ] || [ "$KEEP_LOG" = 1 ]; then
  echo "with_serve: engine log ($SERVE_LOG):"
  tail -n 20 "$SERVE_LOG"
fi
echo "TEST_EXIT=$rc"
exit "$rc"
