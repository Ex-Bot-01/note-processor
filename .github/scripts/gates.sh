#!/usr/bin/env bash
# The gate list, as one command, on any machine. CI grades `main` when it has minutes; this script is what "graded" means
# when it does not (`docs/ops-actions-minutes.md`), and it is the same list CI runs — so a green local log and a green run
# say the same thing about the same tree.
#
# Usage:
#   .github/scripts/gates.sh [log-dir]                 hermetic rows (no model artifacts)
#   NP_TEST_MODEL_PATH=… NP_TEST_MMPROJ_PATH=… \
#     .github/scripts/gates.sh [log-dir]               adds the two artifact-backed rows
#
# Two environment facts this script refuses to get wrong:
#   * `local-model` rows need CMake. Without `CMAKE` pointing at a real cmake binary, `crates/np-core/build.rs` panics and
#     three rows fail for a reason that reads exactly like a code defect. If cmake is not on PATH and `$CMAKE` is unset, the
#     local-model rows are reported as SKIPPED rather than run into a false red.
#   * artifact-gated tests print `NOT COVERAGE` when their fixture is absent. A row can therefore be green and vacuous, so
#     every log is counted and the total is printed beside the verdict. Green-with-NOT-COVERAGE is a fact to report, not a pass.
set -uo pipefail

logs="${1:-/tmp/np-gates}"
mkdir -p "$logs"
fail=0
vacuous=0
have_cmake=0
# Two shapes of "$CMAKE" have to work: an absolute path (which `command -v` answers for, but `${CMAKE%%/*}` on a leading slash
# is the empty string — the first version of this check therefore reported "no cmake" on a box where cmake was installed and
# $CMAKE pointed at it) and a bare name on PATH.
if [ -n "${CMAKE:-}" ] && { [ -x "${CMAKE:-}" ] || command -v "$CMAKE" >/dev/null 2>&1; }; then
  have_cmake=1
elif command -v cmake >/dev/null 2>&1; then
  CMAKE="$(command -v cmake)"; export CMAKE; have_cmake=1
fi
export TMPDIR="${TMPDIR:-/tmp}"

# **The second hermeticity lock (F38), and the reason this script is how local runs stay honest.** `np embed` fetches a
# missing pinned artifact by default, so a gate row that exercises the CLI against a workspace with no artifact would, on a
# connected machine, start a 310 MB download inside someone's test run. Every harness strips this variable and sets its own
# value, so the lock is for whatever runs a binary outside them — and for the machine this script exists for: a laptop on a
# metered link. Delete it deliberately (`NP_NO_AUTO_FETCH= .github/scripts/gates.sh`) if you are testing the fetch path by
# hand; nothing in the gate list needs that.
export NP_NO_AUTO_FETCH="${NP_NO_AUTO_FETCH-1}"

# **Linking is the memory-bound step on this class of box, and cargo does not know that.** `nproc` here says 16 while RAM says
# 7.4 GB, and an uncapped `cargo test --workspace` links up to sixteen debug test binaries at once — each with full debuginfo —
# which on 2026-11-07 made `rust-lld` die with `ld terminated with signal 7 [Bus error]` on three rows and left the three
# python checks exiting 120 (CPython's "could not flush stdout at exit" status) with empty logs. All six passed on a re-run, so
# the red was resource pressure, not the tree. Four concurrent link jobs fit; the number is this box's, not a universal one, and
# `CARGO_BUILD_JOBS` in the environment overrides it.
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"

row() {
  name="$1"; shift
  "$@" >"$logs/$name.log" 2>&1
  code=$?
  printf '%-32s exit=%s\n' "$name" "$code"
  [ "$code" -ne 0 ] && fail=1
}

# `row` with a reason to skip: a skipped row is not a pass, and it prints as its own line so no summary can read it as one.
row_or_skip() {
  name="$1"; reason="$2"; shift 2
  if [ -n "$reason" ]; then
    printf '%-32s SKIPPED (%s)\n' "$name" "$reason"
    return
  fi
  row "$name" "$@"
}

# A log that names a commit while the working tree differs from it is a log that will mislead someone later, so say plainly how many
# tracked files differ from the named commit. `dirty=0` is what a graded tree looks like.
tree=$(git rev-parse HEAD 2>/dev/null || echo 'not-a-checkout')
dirty=$(git status --porcelain 2>/dev/null | wc -l | tr -d ' ')
echo "# tree $tree dirty=$dirty on $(uname -srm)"
echo "# cmake  ${CMAKE:-none} (local-model rows need it)"
ram='?'
[ -r /proc/meminfo ] && ram=$(awk '/^MemTotal:/ { printf "%.1f GB", $2 / 1048576 }' /proc/meminfo)
echo "# jobs   CARGO_BUILD_JOBS=$CARGO_BUILD_JOBS  ram=$ram"

row fmt cargo fmt --all -- --check
row clippy-default cargo clippy --workspace --all-targets -- -D warnings
cmake_missing=""
[ "$have_cmake" = 1 ] || cmake_missing="no cmake on PATH; set CMAKE"
row_or_skip clippy-localmodel "$cmake_missing" cargo clippy -p np-core -p np-cli --features local-model --all-targets -- -D warnings
row_or_skip clippy-lm-vision "$cmake_missing" cargo clippy -p np-core -p np-cli --features local-model,vision --all-targets -- -D warnings
row clippy-nodefaults cargo clippy -p np-core -p np-cli --no-default-features --all-targets -- -D warnings
row check-cli-vision cargo check -p np-cli --features local-model,vision
row test-workspace cargo test --workspace
row test-cli-nodefaults cargo test -p np-cli --no-default-features
row_or_skip test-cli-vision "$cmake_missing" cargo test -p np-cli --features local-model,vision
row_or_skip test-core-vision "$cmake_missing" cargo test -p np-core --features local-model,vision
row test-config-nohome env -u HOME -u XDG_CONFIG_HOME cargo test -p np-core --test config

RUSTDOCFLAGS="--deny warnings" row rustdoc-deny-default cargo doc --workspace --no-deps
row_or_skip rustdoc-deny-localmodel "$cmake_missing" env RUSTDOCFLAGS=--deny\ warnings cargo doc -p np-core -p np-cli --features local-model --no-deps
row_or_skip rustdoc-deny-lm-vision "$cmake_missing" env RUSTDOCFLAGS=--deny\ warnings cargo doc -p np-core -p np-cli --features local-model,vision --no-deps

S=.superpowers/sdd/2026-10-23-note-processor-m1-core-catalog
row cfg-dead-code python3 "$S/check_cfg_dead_code.py"
row attr-displacement python3 "$S/check_attr_displacement.py"
row catalog-goldens python3 "$S/verify_catalog_goldens.py"
# **F31's harness arithmetic, with no binary and no model.** The metric functions are self-tested against hand-computed numbers, so
# this row costs milliseconds and catches the one failure mode that would matter: a scoring function that quietly flatters itself (an
# MRR computed over only the answered queries rises when recall falls). The harness's *usefulness* still needs labelled queries — see
# `docs/design-f31-quality-harness.md`, which is written to keep that distinction visible.
# Not `$S`: the three checkers above live under `.superpowers/sdd/…`, this one lives beside this script. The first version of
# this row used `$S` and failed with "can't open file", which is the sound of a gate row skipping itself while still printing
# a red exit code — better than silently passing, but only just.
row f31-selftest python3 .github/scripts/f31_eval.py --self-test

# The two rows that need real artifacts. They run only when the paths are set, because a vacuous pass here is the exact
# failure mode F33's review named: green because nothing was loaded.
artifacts=""
[ -n "${NP_TEST_MODEL_PATH:-}" ] || artifacts="NP_TEST_MODEL_PATH unset"
row_or_skip test-cli-realmodel "$artifacts" env CARGO_BUILD_JOBS=2 cargo test -p np-cli --features local-model
[ -n "${NP_TEST_MMPROJ_PATH:-}" ] || artifacts="NP_TEST_MODEL_PATH or NP_TEST_MMPROJ_PATH unset"
row_or_skip test-core-realvision "$artifacts" env CARGO_BUILD_JOBS=2 cargo test -p np-core --features local-model,vision

vacuous=$(grep -rho 'NOT COVERAGE' "$logs" 2>/dev/null | wc -l | tr -d ' ')
echo "vacuous  NOT COVERAGE lines across all logs: $vacuous"
echo "OVERALL fail=$fail"
[ "$fail" = 0 ] && echo "graded locally at $(git rev-parse --short HEAD 2>/dev/null) — logs in $logs"
exit "$fail"
