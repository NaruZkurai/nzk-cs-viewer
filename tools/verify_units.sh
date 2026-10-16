#!/usr/bin/env bash
# =============================================================================
#  verify_units.sh — bounded, parallel unit runner used by verify_all.sh.
# =============================================================================
#
#  A tiny job pool in pure bash: add units (name + command), run them with a
#  fixed concurrency cap, and get a per-unit PASS/FAIL table with wall times and
#  one log file per unit. Nothing here starts a long-lived server; every unit
#  is wrapped in `timeout` so a hung unit fails instead of hanging the run.
#
#  Sourced API (see verify_all.sh):
#     vu_reset                       clear the unit list
#     vu_add PHASE NAME COMMAND      queue a unit (PHASE is an integer; lower
#                                    phases run to completion first)
#     vu_run JOBS LOGDIR TIMEOUT_S ROOTDIR
#     vu_table                       print the result table (sets VU_FAILED)
#     vu_fail_tails                  print the tail of every failed unit's log
#     vu_max_ms                      print the longest unit wall time in ms
#
#  Commands run with `bash -c` from ROOTDIR and expand $VU_* / $VU_TESTBIN,
#  which the caller exports. One test per process is the caller's job.
#
#  Running this file directly prints this header.
# =============================================================================

VU_NAMES=()
VU_CMDS=()
VU_PHASES=()
VU_RC=()
VU_MS=()
VU_LOG=()
VU_JOBS=4
VU_TIMEOUT=1800
VU_LOGDIR="."
VU_ROOTDIR="."
VU_FAILED=0
VU_MAX_MS=0
declare -A VU_PID_NAME=()
declare -A VU_PID_T0=()

vu_now_ms() { date +%s%3N; }

vu_slug() { printf '%s' "$1" | tr -c 'A-Za-z0-9._-' '_'; }

vu_ms_str() {
  local ms="${1:-0}"
  printf '%d.%03ds' $((ms / 1000)) $((ms % 1000))
}

vu_reset() {
  VU_NAMES=(); VU_CMDS=(); VU_PHASES=()
  VU_RC=(); VU_MS=(); VU_LOG=()
  VU_PID_NAME=(); VU_PID_T0=()
  VU_FAILED=0; VU_MAX_MS=0
}

vu_add() {                       # vu_add <phase> <name> <command>
  VU_PHASES+=("$1")
  VU_NAMES+=("$2")
  VU_CMDS+=("$3")
}

vu_launch() {
  local idx="$1"
  local name="${VU_NAMES[$idx]}"
  local cmd="${VU_CMDS[$idx]}"
  local log
  log="$VU_LOGDIR/$(printf '%03d' "$idx")-$(vu_slug "$name").log"
  ( cd "$VU_ROOTDIR" && timeout --signal=TERM "$VU_TIMEOUT" bash -c "$cmd" ) \
    >"$log" 2>&1 &
  local pid=$!
  VU_PID_NAME[$pid]="$idx"
  VU_PID_T0[$pid]="$(vu_now_ms)"
  VU_LOG[$idx]="$log"
}

vu_wait_one() {
  local pid="" rc=0 idx t0 ms
  wait -n -p pid 2>/dev/null || rc=$?
  [ -n "$pid" ] || return 0
  idx="${VU_PID_NAME[$pid]:-}"
  [ -n "$idx" ] || return 0
  t0="${VU_PID_T0[$pid]}"
  unset 'VU_PID_NAME[$pid]' 'VU_PID_T0[$pid]'
  ms=$(( $(vu_now_ms) - t0 ))
  VU_RC[$idx]="$rc"
  VU_MS[$idx]="$ms"
  local status=PASS
  if [ "$rc" -eq 124 ]; then status=TIMEOUT
  elif [ "$rc" -ne 0 ]; then status=FAIL; fi
  printf '  [%-7s %9s] %s\n' "$status" "$(vu_ms_str "$ms")" "${VU_NAMES[$idx]}"
}

# Run every unit, phase by phase, at most $VU_JOBS at a time.
vu_run() {
  VU_JOBS="$1"; VU_LOGDIR="$2"; VU_TIMEOUT="$3"; VU_ROOTDIR="$4"
  mkdir -p "$VU_LOGDIR"
  local phase running=0 i
  local phases
  phases="$(printf '%s\n' "${VU_PHASES[@]}" | sort -n -u)"
  for phase in $phases; do
    running=0
    for i in "${!VU_NAMES[@]}"; do
      [ "${VU_PHASES[$i]}" = "$phase" ] || continue
      while [ "$running" -ge "$VU_JOBS" ]; do
        vu_wait_one
        running=$((running - 1))
      done
      vu_launch "$i"
      running=$((running + 1))
    done
    while [ "$running" -gt 0 ]; do
      vu_wait_one
      running=$((running - 1))
    done
  done
}

vu_table() {
  local i status failed=0 max=0
  printf '\n%-8s %11s  %s\n' STATUS WALL UNIT
  printf '%-8s %11s  %s\n' ------ ----------- ----
  for i in "${!VU_NAMES[@]}"; do
    local rc="${VU_RC[$i]:-99}"
    case "$rc" in
      0) status=PASS ;;
      124) status=TIMEOUT ;;
      *) status=FAIL ;;
    esac
    [ "$status" = PASS ] || failed=$((failed + 1))
    local ms="${VU_MS[$i]:-0}"
    [ "$ms" -gt "$max" ] && max="$ms"
    printf '%-8s %11s  %s\n' "$status" "$(vu_ms_str "$ms")" "${VU_NAMES[$i]}"
  done
  VU_FAILED="$failed"
  VU_MAX_MS="$max"
}

vu_fail_tails() {
  local i rc
  for i in "${!VU_NAMES[@]}"; do
    rc="${VU_RC[$i]:-99}"
    [ "$rc" -eq 0 ] && continue
    printf '\n--- tail %s (%s)\n' "${VU_NAMES[$i]}" "${VU_LOG[$i]:-no-log}"
    tail -n 15 "${VU_LOG[$i]:-/dev/null}" 2>/dev/null | sed 's/^/    /'
  done
}

vu_max_ms() { printf '%s\n' "$VU_MAX_MS"; }

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  sed -n '2,30p' "$0"
  exit 2
fi
