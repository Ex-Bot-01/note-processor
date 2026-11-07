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
if [ -n "${CMAKE:-}" ] && command -v "${CMAKE%%/*}" >/dev/null 2>&1; then
  have_cmake=1
elif command -v cmake >/dev/null 2>&1; then
  CMAKE="$(command -v cmake)"; export CMAKE; have_cmake=1
fi
export TMPDIR="${TMPDIR:-/tmp}"

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

echo "# tree $(git rev-parse HEAD 2>/dev/null || echo 'not a git checkout') on $(uname -srm)"
echo "# cmake  ${CMAKE:-none} (local-model rows need it)"

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
