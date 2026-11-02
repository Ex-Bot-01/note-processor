#!/usr/bin/env bash
# D13 thread-count sweep: one process per cell, rows appended to a committed log.
#
# Why separate processes: a ggml threadpool is process-wide state and `VmHWM` is a process-lifetime high-water mark, so
# a second measurement inside one process inherits the first pool's memory ceiling (R6d round 2 established this for
# OpenMP; the same reasoning applies to a per-context worker count). Separate processes are also what a user runs —
# `np embed` loads, works, exits — so peak RSS here means "peak RSS of one embed command".
#
# Usage:
#   NP_TEST_MODEL_PATH=/path/to/model.gguf ./thread_sweep.sh <short|long> <log-path> [threads...]
#   SWEEP_PREFIX="taskset -c 0-3" ./thread_sweep.sh short "$LOG" 2      # a restricted-cpuset row
#
# `short` decodes ~403-token notes (the realistic chunk), `long` decodes near-window notes. Thread counts default to
# 1 2 4 6 8 12 16 32 for short and 2 4 8 16 for long; pass explicit counts as trailing arguments to override. The
# defaults include 32 on purpose: a count above the machine's own is the case that shows "more" stops being better, and
# a claim resting on it has to leave a row behind (round-2 review P1 — the first version of this script measured it in
# an ad-hoc loop and the docs quoted a number no log contained).
#
# What the script contributes beyond the test's own rows: the wall/user/sys triple from /usr/bin/time (CPU utilisation
# belongs to the process, not to anything inside it), any llama.cpp warning line copied verbatim so "no warnings" is
# observable rather than a grep that could never match, and the determinism table computed from the vectors this run
# dumped. A cell that exits non-zero is announced on stdout as well as in the log.
set -u

PROFILE="${1:-short}"
LOG="${2:-/dev/stdout}"
shift 2 2>/dev/null || true

if [ -z "${NP_TEST_MODEL_PATH:-}" ]; then
  echo "thread_sweep.sh: NP_TEST_MODEL_PATH must name the artifact to measure against" >&2
  exit 2
fi
if [ ! -f "$NP_TEST_MODEL_PATH" ]; then
  echo "thread_sweep.sh: $NP_TEST_MODEL_PATH is not a file" >&2
  exit 2
fi

case "$PROFILE" in
  short)
    TOKENS=403
    REPEATS=3
    DEFAULT_THREADS="1 2 4 6 8 12 16 32"
    ;;
  long)
    TOKENS=7992
    REPEATS=2
    DEFAULT_THREADS="2 4 8 16"
    ;;
  *)
    echo "thread_sweep.sh: profile must be 'short' or 'long', got '$PROFILE'" >&2
    exit 2
    ;;
esac

if [ "$#" -gt 0 ]; then
  THREADS="$*"
else
  THREADS="$DEFAULT_THREADS"
fi

PREFIX="${SWEEP_PREFIX:-}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
DUMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/thread-sweep-XXXXXX")"
FAILED=""

echo "SWEEP      profile=$PROFILE tokens_target=$TOKENS repeats=$REPEATS threads='$THREADS' prefix='${PREFIX:-none}' dumps=$DUMP_DIR" | tee -a "$LOG"

for threads in $THREADS; do
  cell_log="$DUMP_DIR/cell-$threads-$TOKENS.log"
  time_log="$DUMP_DIR/time-$threads-$TOKENS.txt"
  # One process per cell, and `--exact` so a future case added to the file cannot silently join the measurement.
  # `/usr/bin/time -v` wraps it: elapsed/user/sys are the only way to say what the machine was asked to do overall.
  NP_THREAD_THREADS="$threads" NP_THREAD_TOKENS="$TOKENS" NP_THREAD_REPEATS="$REPEATS" \
    NP_THREAD_DUMP="$DUMP_DIR/vec-$threads.f32" \
    /usr/bin/time -v -o "$time_log" $PREFIX \
    cargo test -p np-core --features local-model,fetch --test local_threads \
      -- --exact --nocapture one_cell_of_the_thread_sweep >"$cell_log" 2>&1
  rc=$?

  # Elapsed/user/sys, and CPU utilisation as a percentage of one core. A failed cell has no meaningful numbers, so the
  # line says which of the two it is instead of printing zeros.
  if [ -s "$time_log" ]; then
    # GNU time indents every field with a tab, so the patterns are deliberately unanchored — anchored ones matched
    # nothing and this line silently disappeared from the first version of the script. The percentage is taken from
    # time's own field rather than recomputed, and it is **for the whole process**, cargo harness included: the number
    # that belongs to the decode alone is in the test's `DECODE` row.
    awk -v threads="$threads" -v rc="$rc" '
      /Elapsed/ { split($NF, t, ":"); wall = t[1]*60 + t[2] + t[3] }
      /User time/ { user = $NF }
      /System time/ { sys = $NF }
      /Percent of CPU/ { cpu = $NF }
      END {
        if (rc == 0 && wall > 0) {
          printf "CELLCPU    threads=%s wall_s=%.1f user_s=%.1f sys_s=%.1f cpu_pct_whole_process=%s\n", \
                 threads, wall, user, sys, cpu
        } else {
          printf "CELLCPU    threads=%s wall_s=%.1f rc=%s (no utilisation for a failed cell)\n", threads, wall, rc
        }
      }' "$time_log" | tee -a "$LOG"
  fi

  {
    echo "CELL_BEGIN threads=$threads tokens_target=$TOKENS profile=$PROFILE rc=$rc"
    grep -E '^(CELL|LOAD|DECODE|RSS|SELF|SUMMARY) ' "$cell_log"
    # Copied, not counted: the day llama.cpp warns that it clamped something, the row has to show what it said.
    warnings=$(grep -iE 'warn|assert|more threads' "$cell_log" || true)
    if [ -n "$warnings" ]; then
      echo "CELL_WARN  threads=$threads"
      printf '%s\n' "$warnings" | sed 's/^/# warn: /'
    fi
    if [ "$rc" -ne 0 ]; then
      echo "CELL_FAIL  threads=$threads tail:"
      tail -20 "$cell_log" | sed 's/^/# /'
      FAILED="$FAILED $threads"
    fi
    echo "CELL_END   threads=$threads"
  } | tee -a "$LOG"
done

# **The determinism table is part of the evidence, not a side effect.** Each cell dumped the vector it produced; this
# compares every count against the default (4 workers) and appends the result, so a claim like "bit-identical across
# worker counts" points at rows in this file rather than at a terminal someone scrolled past.
if [ -f "$DUMP_DIR/vec-4.f32" ]; then
  {
    echo "DETERMINISM baseline=vec-4.f32 (the default worker count) computed by probes/thread_vectors_compare.py"
    python3 "$SCRIPT_DIR/thread_vectors_compare.py" "$DUMP_DIR/vec-4.f32" "$DUMP_DIR"/vec-*.f32 2>&1 | tail -n +2
  } | tee -a "$LOG"
else
  echo "DETERMINISM skipped: no vec-4.f32 in $DUMP_DIR, so nothing to compare against" | tee -a "$LOG"
fi

echo "DUMPS      dir=$DUMP_DIR (one raw little-endian float-32 row per worker count)" | tee -a "$LOG"
ls "$DUMP_DIR"/vec-*.f32 >>"$LOG" 2>/dev/null

if [ -n "$FAILED" ]; then
  echo "SWEEP FAILED cells:$FAILED (log: $LOG)" >&2
  exit 1
fi
