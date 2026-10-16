#!/usr/bin/env bash
# =============================================================================
#  verify_all.sh — prove the v2 tree is a clone, in bounded parallel units.
# =============================================================================
#
#  Every check runs as its own unit, one unit per process, with a small job
#  cap. The offline cargo build is the ONLY thing allowed to exceed the
#  per-unit budget: it runs first, verbose and unbuffered, tee'd to a
#  timestamped log with an elapsed heartbeat, and no unit starts until the
#  binaries (and the unit-test binary) exist.
#
#  Units:
#    build                      cargo build + cargo test --no-run (exceeds budget
#                               by design; monitored the whole time)
#    monaco vendor integrity    tools/vendor_monaco.py --verify
#    Nx cargo unit tests        one `--exact` case per process (enumerated from
#                               the built test binaries, never hard-coded)
#    parser/graph clone         tools/diff_parser.py
#    document clone             tools/diff_doc.py
#    reorg clone (5 units)      tools/diff_reorg.py --spec KEY, one spec per
#                               unit so no unit except the build exceeds 30 s
#    csviewer --selftest        the Rust host, no Python
#    index stop probe           tools/probe_index_stop.mjs via with_serve.sh
#    mcp server probe           tools/probe_mcp.py (spawns csgraph --mcp, stdio)
#    clone dump: oracle         tools/oracle.py            (skipped by --quick)
#    clone dump: rust           csgraph dump               (skipped by --quick)
#    clone diff                 tools/diff_clone.py        (phase 2, --quick skips)
#
#  Usage:
#    tools/verify_all.sh [--repo DIR] [--root REL] [--quick]
#                        [--jobs N] [--logs DIR] [--dry-run]
#
#    --repo DIR       repository to index      (default: the checkout above)
#    --root REL       root inside the repo     (default: _sealed_src)
#    --quick          skip the full clone dump (stages clone dump/diff only);
#                     every other unit still runs
#    --jobs N         parallel units           (default: 4)
#    --logs DIR       log directory            (default: target/verify/<UTC ts>)
#    --dry-run        print the unit plan and the enumerated test names, then
#                     exit without executing anything (uses an already-built
#                     test binary to enumerate; it never builds)
#    --unit-timeout N per-unit timeout in seconds (default: 1800 = 30 min)
#
#  Each unit gets its own log under the log directory; the summary table lists
#  status + wall time per unit and the tail of every failing log is printed.
#  Exit status is 0 only when every unit passed.
# =============================================================================

set -uo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
ROOT="$(cd -- "$HERE/.." && pwd -P)"          # rust-only-csviewer/
BUILDER="$(cd -- "$ROOT/.." && pwd -P)"       # "cs viewer builder/"
REPO="$(cd -- "$BUILDER/.." && pwd -P)"       # the vrcCS checkout
ROOT_REL="_sealed_src"
QUICK=0
JOBS=4
LOGS=""
DRY_RUN=0
UNIT_TIMEOUT=1800

while [ $# -gt 0 ]; do
  case "$1" in
    --repo) REPO="$2"; shift 2 ;;
    --root) ROOT_REL="$2"; shift 2 ;;
    --quick) QUICK=1; shift ;;
    --jobs) JOBS="$2"; shift 2 ;;
    --logs) LOGS="$2"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    --unit-timeout) UNIT_TIMEOUT="$2"; shift 2 ;;
    -h|--help) sed -n '2,46p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "err: unknown option $1" >&2; exit 2 ;;
  esac
done

case "$JOBS" in
  ''|*[!0-9]*|0) echo "err: --jobs must be a positive integer" >&2; exit 2 ;;
esac
case "$UNIT_TIMEOUT" in
  ''|*[!0-9]*|0) echo "err: --unit-timeout must be a positive integer" >&2; exit 2 ;;
esac

[ -n "$LOGS" ] || LOGS="$ROOT/target/verify/$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$LOGS"
ln -sfn "$LOGS" "$ROOT/target/verify/latest" 2>/dev/null || true

source "$HERE/verify_units.sh"

export VU_TOOLS="$HERE"
export VU_REPO="$REPO"
export VU_ROOT_REL="$ROOT_REL"
export VU_LOGS="$LOGS"
export VU_BUILDER="$BUILDER"

if command -v stdbuf >/dev/null 2>&1; then
  STDBUF="stdbuf -oL -eL"
else
  STDBUF=""
fi

TESTBINS=()
TEST_NAMES=()
TEST_BIN=()

discover_testbin() {
  local dir="${CARGO_TARGET_DIR:-$BUILDER/target}/debug/deps"
  [ -d "$dir" ] || return 1
  find "$dir" -maxdepth 1 -type f -name 'csgraph_core-*' ! -name '*.d' \
       -perm -u+x -printf '%T@ %p\n' 2>/dev/null \
    | sort -n | tail -n 1 | cut -d' ' -f2-
}

# `cargo test -p csgraph-core --no-run` yields one executable per test target
# (the library and any bin targets); list every case against its own binary.
enumerate_tests() {
  TEST_NAMES=(); TEST_BIN=()
  local bin name
  for bin in "${TESTBINS[@]}"; do
    [ -x "$bin" ] || continue
    while IFS= read -r name; do
      [ -n "$name" ] || continue
      TEST_NAMES+=("$name")
      TEST_BIN+=("$bin")
    done < <("$bin" --list 2>/dev/null | awk '/: test$/ { sub(/: test$/, ""); print }')
  done
  [ "${#TEST_NAMES[@]}" -gt 0 ]
}

load_testbins() {                # load_testbins <cargo-json-log>
  local log="$1"
  mapfile -t TESTBINS < <(python3 - "$log" <<'PY'
import json
import sys

seen = set()
try:
    with open(sys.argv[1], encoding='utf-8', errors='replace') as fh:
        for line in fh:
            try:
                obj = json.loads(line)
            except Exception:                      # noqa: BLE001
                continue
            if obj.get('reason') == 'compiler-artifact' and obj.get('executable'):
                exe = obj['executable']
                if exe not in seen:
                    seen.add(exe)
                    print(exe)
except OSError:
    pass
PY
)
  if [ "${#TESTBINS[@]}" -eq 0 ]; then
    local one
    one="$(discover_testbin || true)"
    [ -n "$one" ] && TESTBINS=("$one")
  fi
  [ "${#TESTBINS[@]}" -gt 0 ]
}

queue_units() {
  vu_reset

  vu_add 1 "monaco vendor integrity" \
    "python3 \"\$VU_TOOLS/vendor_monaco.py\" --verify"
  vu_add 1 "parser/graph clone" \
    "python3 \"\$VU_TOOLS/diff_parser.py\" --repo \"\$VU_REPO\" --root \"\$VU_ROOT_REL\""
  vu_add 1 "document clone" \
    "python3 \"\$VU_TOOLS/diff_doc.py\" --repo \"\$VU_REPO\" --root \"\$VU_ROOT_REL\""
  # The reorg clone is five independent specs. Each spec runs as its own unit
  # (both producers limited with `--spec`), so the heaviest unit is one spec
  # instead of the sum; the Rust side reuses the corpus graph for `before`, and
  # the Python producer is a few seconds per spec. Build is the only unit
  # allowed past the 30 s budget.
  local spec
  for spec in 'group=single' 'group=size|size=4000' \
              'granularity=leaf|group=size|size=3000' \
              'group=namespace' 'group=owner'; do
    vu_add 1 "reorg clone: $spec" \
      "python3 \"\$VU_TOOLS/diff_reorg.py\" --repo \"\$VU_REPO\" --root \"\$VU_ROOT_REL\" --spec '$spec'"
  done
  vu_add 1 "csviewer --selftest" \
    "\"\$VU_BUILDER/target/debug/csviewer\" --repo \"\$VU_REPO\" --root \"\$VU_ROOT_REL\" --selftest"
  # The two new browser-free acceptance probes. `probe_index_stop.mjs` drives
  # the real Stop route through `with_serve.sh`, so the dev server is a fixture
  # that is always torn down; `probe_mcp.py` spawns its own `csgraph --mcp` on
  # stdio and kills it at the end. Both are bounded well under the unit budget.
  vu_add 1 "index stop probe" \
    "tools/with_serve.sh -- node tools/probe_index_stop.mjs"
  # The MCP probe is sharded so no unit exceeds the 10 s rule: each shard is one
  # process (read 4.7 s, concurrency 2.8 s, cancel 5.7 s, restart 4.6 s), the
  # union is the original 31 checks, and all four can run in parallel.
  vu_add 1 "mcp probe: read" \
    "python3 \"\$VU_TOOLS/probe_mcp.py\" --shard read"
  vu_add 1 "mcp probe: concurrency" \
    "python3 \"\$VU_TOOLS/probe_mcp.py\" --shard concurrency"
  vu_add 1 "mcp probe: cancel" \
    "python3 \"\$VU_TOOLS/probe_mcp.py\" --shard cancel"
  vu_add 1 "mcp probe: restart" \
    "python3 \"\$VU_TOOLS/probe_mcp.py\" --shard restart"
  # The app-side half of user/agent parity: GET/POST /api/edit and
  # /api/analyze through the real host over `--serve` (the same Engine::handle
  # the Tauri IPC uses), plus the edit/reorg panels in the page. Sharded into
  # three bounded processes (the 54-check union, nothing lost) so no unit
  # exceeds the 10 s rule: edit 41 checks 2.3 s, analyze 10 checks 7.1 s,
  # reorg 3 checks 5.0 s. Always torn down.
  vu_add 1 "edit app probe: edit" \
    "tools/with_serve.sh -- node tools/probe_edit_app.mjs --shard edit"
  vu_add 1 "edit app probe: analyze" \
    "tools/with_serve.sh -- node tools/probe_edit_app.mjs --shard analyze"
  vu_add 1 "edit app probe: reorg" \
    "tools/with_serve.sh -- node tools/probe_edit_app.mjs --shard reorg"
  # The MCP edit-tool probe (select_decls / move_decls / rename / write / apply)
  # is browser-free and runs in ~0.2 s.
  vu_add 1 "mcp edit probe" \
    "python3 \"\$VU_TOOLS/probe_edit_mcp.py\""
  # The M0 validity gate: real Roslyn parse diagnostics over the corpus, and a
  # deliberately broken file must be caught. Exits non-zero on a false ok only;
  # a missing toolchain is an honest `skipped`, not a failure.
  vu_add 1 "validity gate probe" \
    "python3 \"\$VU_TOOLS/probe_validity.py\""
  # The compile-level differential (M0 final item): SDK Roslyn `csgraph
  # compile-diff` over a fixture baseline/candidate. Exit 0 = no new
  # diagnostics, 1 = new/worse, 3 = honest `skipped` (never a pass), 2 = usage.
  vu_add 1 "compile-level gate probe" \
    "python3 \"\$VU_TOOLS/probe_compile.py\""
  # M2 slice 1 (dedupe): fixture detection, two byte-identical corpus runs, and
  # the MCP `find_duplicates` body must equal the CLI `--json`. Runs just under
  # the 10 s unit budget (measured 9.7 s).
  vu_add 1 "dedupe probe" \
    "python3 \"\$VU_TOOLS/probe_dedupe.py\""
  # M2 slice 2 (reformat + functionize): the MCP-only reshape tools against a
  # scratch copy of tools/fixtures/reshape — dry-run plans, refusal cases, the
  # atomic apply, the unverified/require_validated policy, and cluster
  # extraction. Bounded (~0.4 s).
  vu_add 1 "reshape probe" \
    "python3 \"\$VU_TOOLS/probe_reshape.py\""
  # WS1 corpus regression probe: the eight `reformat` engine defects found while
  # reshaping a COPY of the real corpus, each as a bounded self-contained
  # fixture at the MCP surface (~0.1 s).
  vu_add 1 "reshape corpus probe" \
    "python3 \"\$VU_TOOLS/probe_reshape_corpus.py\""
  # Fourth pass (residual limits closed): R1 binds ONE `csgraph --mcp` process
  # to two repos via repeated `--repo` and drives the per-call `repo` argument
  # (isolation, byte-identity, refusal); R2 SIGKILLs a `copy_path` apply
  # mid-flight and recovers it from a *second* process, including the
  # committed-cleanup window. Both bounded (~0.1 s / ~0.3 s), one process each.
  vu_add 1 "mcp multi-repo probe" \
    "python3 \"\$VU_TOOLS/probe_multirepo.py\""
  vu_add 1 "copy recover probe" \
    "python3 \"\$VU_TOOLS/probe_copy_recover.py\""
  # M3 slice 1: the read-only "Blocks" Scratch-like canvas — block cards, typed
  # ports, reference edges, hover peek and click-to-navigate in the real page,
  # with 0 console errors. Browses over `with_serve.sh`, always torn down.
  vu_add 1 "scratch blocks probe" \
    "tools/with_serve.sh -- node tools/probe_scratch.mjs"
  # M3 slice 2: the read-only signature/call-site change preview. `probe_preview.py`
  # drives one `csgraph --mcp` process over a synthetic repo (deterministic plan,
  # all refusal cases, zero writes proven by sha256); `probe_preview.mjs` drives
  # the app twin `POST /api/preview` and the panel through `with_serve.sh`.
  # Both bounded (0.07 s / ~2 s), one process each.
  vu_add 1 "preview change probe" \
    "python3 \"\$VU_TOOLS/probe_preview.py\""
  vu_add 1 "preview app probe" \
    "tools/with_serve.sh -- node tools/probe_preview.mjs"

  if [ "$QUICK" != 1 ]; then
    vu_add 1 "clone dump: oracle" \
      "python3 \"\$VU_TOOLS/oracle.py\" --repo \"\$VU_REPO\" -o \"\$VU_LOGS/clone-gold.json\""
    vu_add 1 "clone dump: rust" \
      "\"\$VU_BUILDER/target/debug/csgraph\" dump --repo \"\$VU_REPO\" -o \"\$VU_LOGS/clone-rust.json\""
    vu_add 2 "clone diff" \
      "python3 \"\$VU_TOOLS/diff_clone.py\" \"\$VU_LOGS/clone-gold.json\" \"\$VU_LOGS/clone-rust.json\""
  fi

  local i q b
  for i in "${!TEST_NAMES[@]}"; do
    q="$(printf '%q' "${TEST_NAMES[$i]}")"
    b="$(printf '%q' "${TEST_BIN[$i]}")"
    vu_add 1 "test: ${TEST_NAMES[$i]}" "$b --exact $q --nocapture"
  done
}

# Run a command with an elapsed heartbeat; stdout/stderr go to <log>.
run_monitored() {                # run_monitored <label> <log> <command>
  local label="$1" log="$2" cmd="$3"
  local t0 pid el
  t0="$(vu_now_ms)"
  ( cd "$ROOT" || exit 1; source ../rust-env.sh || exit 1; eval "$cmd" ) \
    >"$log" 2>&1 &
  pid=$!
  while kill -0 "$pid" 2>/dev/null; do
    sleep 10
    kill -0 "$pid" 2>/dev/null || break
    el=$(( ($(vu_now_ms) - t0) / 1000 ))
    printf '[%s] %dm%02ds elapsed; last: %s\n' "$label" $((el / 60)) $((el % 60)) \
      "$(tail -n 1 "$log" 2>/dev/null | cut -c1-100)"
  done
  wait "$pid"
  return $?
}

# ---------------------------------------------------------------------------
# dry run
# ---------------------------------------------------------------------------

if [ "$DRY_RUN" = 1 ]; then
  one="$(discover_testbin || true)"
  [ -n "$one" ] && TESTBINS=("$one")
  enumerate_tests || true
  queue_units
  echo "== verify_all.sh dry run (nothing executed)"
  echo "repo           : $REPO"
  echo "root           : $ROOT_REL"
  echo "jobs           : $JOBS"
  echo "unit timeout   : ${UNIT_TIMEOUT}s (build is exempt)"
  echo "logs           : $LOGS"
  echo "quick          : $QUICK"
  echo
  echo "build unit:"
  echo "  $STDBUF cargo build --offline -v --color=never -p csgraph-core -p csviewer"
  echo "  $STDBUF cargo test --offline -p csgraph-core --no-run --message-format=json"
  echo
  echo "parallel units: ${#VU_NAMES[@]}"
  local_i=0
  for local_i in "${!VU_NAMES[@]}"; do
    printf '  [phase %s] %s\n' "${VU_PHASES[$local_i]}" "${VU_NAMES[$local_i]}"
    printf '      %s\n' "${VU_CMDS[$local_i]}"
  done
  echo
  if [ "${#TESTBINS[@]}" -gt 0 ]; then
    echo "test binary: ${TESTBINS[*]}"
  else
    echo "test binary: <not built; run without --dry-run to build + enumerate>"
  fi
  echo "cargo unit tests: ${#TEST_NAMES[@]}"
  local_i=0
  for local_i in "${!TEST_NAMES[@]}"; do
    printf '  %2d. %s\n' "$((local_i + 1))" "${TEST_NAMES[$local_i]}"
  done
  exit 0
fi

# ---------------------------------------------------------------------------
# 1. build (the only unit allowed past the per-unit budget)
# ---------------------------------------------------------------------------

echo "== build (offline, vendored; verbose; log: $LOGS/build.log)"
BUILD_RC=0
run_monitored "build" "$LOGS/build.log" \
  "$STDBUF cargo build --offline -v --color=never -p csgraph-core -p csviewer" \
  || BUILD_RC=$?

TESTBIN_RC=0
run_monitored "test-build" "$LOGS/testbin.jsonl" \
  "$STDBUF cargo test --offline -p csgraph-core --no-run --message-format=json" \
  || TESTBIN_RC=$?

if [ "$BUILD_RC" -ne 0 ] || [ "$TESTBIN_RC" -ne 0 ]; then
  echo
  echo "BUILD FAILED (cargo build rc=$BUILD_RC, cargo test --no-run rc=$TESTBIN_RC)"
  echo "  build log:     $LOGS/build.log"
  echo "  testbin log:   $LOGS/testbin.jsonl"
  tail -n 25 "$LOGS/build.log" 2>/dev/null
  exit 1
fi

load_testbins "$LOGS/testbin.jsonl" || true

for bin in "$BUILDER/target/debug/csgraph" "$BUILDER/target/debug/csviewer"; do
  if [ ! -x "$bin" ]; then
    echo "BUILD FAILED: $bin missing after a green build" >&2
    exit 1
  fi
done
if [ "${#TESTBINS[@]}" -eq 0 ]; then
  echo "BUILD FAILED: no csgraph-core test binary found" >&2
  exit 1
fi
for bin in "${TESTBINS[@]}"; do
  if [ ! -x "$bin" ]; then
    echo "BUILD FAILED: test binary $bin is not executable" >&2
    exit 1
  fi
done

enumerate_tests || { echo "BUILD FAILED: could not enumerate cargo tests" >&2; exit 1; }
echo "== build ok: ${#TESTBINS[@]} test target(s), ${#TEST_NAMES[@]} cargo test cases"

# ---------------------------------------------------------------------------
# 2. parallel units
# ---------------------------------------------------------------------------

queue_units
echo
echo "== running ${#VU_NAMES[@]} units, at most $JOBS at a time, ${UNIT_TIMEOUT}s each"
echo "   logs: $LOGS"
vu_run "$JOBS" "$LOGS" "$UNIT_TIMEOUT" "$ROOT"

# ---------------------------------------------------------------------------
# 3. summary
# ---------------------------------------------------------------------------

vu_table
TOTAL="${#VU_NAMES[@]}"
FAILED="$VU_FAILED"
PASSED=$((TOTAL - FAILED))

echo
printf '  units=%d passed=%d failed=%d\n' "$TOTAL" "$PASSED" "$FAILED"
if [ "$FAILED" -gt 0 ]; then
  vu_fail_tails
fi
echo "  max unit wall (build exempt): $(vu_ms_str "$(vu_max_ms)")"
echo "  logs: $LOGS"
echo
if [ "$FAILED" -gt 0 ]; then
  echo "VERIFY FAILED"
  exit 1
fi
echo "ALL UNITS PASSED"
