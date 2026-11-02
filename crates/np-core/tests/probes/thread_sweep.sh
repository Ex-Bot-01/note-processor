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
#
# `short` decodes ~403-token notes (the realistic chunk), `long` decodes near-window notes. Thread counts default to
# 1 2 4 6 8 12 16 for short and 2 4 8 16 for long; pass explicit counts as trailing arguments to override.
#
# Every row is printed by the test itself (`tests/local_threads.rs`); this script adds only the wall/user/sys triple
# from /usr/bin/time, because CPU utilisation belongs to the process rather than to anything inside it.
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
    DEFAULT_THREADS="1 2 4 6 8 12 16"
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

DUMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/thread-sweep-XXXXXX")"
echo "SWEEP      profile=$PROFILE tokens_target=$TOKENS repeats=$REPEATS threads='$THREADS' dumps=$DUMP_DIR" | tee -a "$LOG"

for threads in $THREADS; do
  cell_log="$DUMP_DIR/cell-$threads-$TOKENS.log"
  time_log="$DUMP_DIR/time-$threads-$TOKENS.txt"
  # One process per cell, and `--exact` so a future case added to the file cannot silently join the measurement.
  NP_THREAD_THREADS="$threads" NP_THREAD_TOKENS="$TOKENS" NP_THREAD_REPEATS="$REPEATS" \
    NP_THREAD_DUMP="$DUMP_DIR/vec-$threads.f32" \
    cargo test -p np-core --features local-model,fetch --test local_threads \
      -- --exact --nocapture one_cell_of_the_thread_sweep >"$cell_log" 2>&1
  rc=$?
  /usr/bin/time -v -o "$time_log" true 2>/dev/null # keep the format file present even when a cell failed
  {
    echo "CELL_BEGIN threads=$threads tokens_target=$TOKENS profile=$PROFILE rc=$rc"
    grep -E '^(CELL|LOAD|DECODE|RSS|SELF|SUMMARY) ' "$cell_log"
    if [ "$rc" -ne 0 ]; then
      echo "CELL_FAIL  tail:"
      tail -20 "$cell_log" | sed 's/^/# /'
    fi
    echo "CELL_END   threads=$threads"
  } | tee -a "$LOG"
done

echo "DUMPS      dir=$DUMP_DIR (one raw little-endian float-32 row per thread count, $(( $(ls "$DUMP_DIR"/vec-*.f32 2>/dev/null | wc -l) )) files)" | tee -a "$LOG"
echo "DUMPS      kept for comparison; the determinism table below is computed from them" | tee -a "$LOG"
ls "$DUMP_DIR"/vec-*.f32 >>"$LOG" 2>/dev/null
