#!/usr/bin/env bash
# The gate list, as one command, on any machine. CI grades `main` when it has minutes; this script is what "graded" means
# when it does not (`docs/developing.md`), and it is the same list CI runs — so a green local log and a green run
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

# **`-- --nocapture` on every test row, and the reason it is not optional.** libtest captures a passing test's stdout and throws
# it away, so the `NOT COVERAGE` lines that artifact-gated cases print when their fixture is absent never reached these logs —
# a row could be green because nothing ran, and the vacuity counter below would read zero while lying (`ci.yml` records the same
# discovery for the hosted jobs). Un-captured output makes "green" and "green but empty" distinguishable, which is the only way
# the count at the bottom of this log means anything.
row fmt cargo fmt --all -- --check
row clippy-default cargo clippy --workspace --all-targets -- -D warnings
cmake_missing=""
[ "$have_cmake" = 1 ] || cmake_missing="no cmake on PATH; set CMAKE"
row_or_skip clippy-localmodel "$cmake_missing" cargo clippy -p np-core -p np-cli --features local-model --all-targets -- -D warnings
row_or_skip clippy-lm-vision "$cmake_missing" cargo clippy -p np-core -p np-cli --features local-model,vision --all-targets -- -D warnings
row clippy-nodefaults cargo clippy -p np-core -p np-cli --no-default-features --all-targets -- -D warnings
# Needs the same cmake as the two clippy rows above: `build.rs` refuses to guess where cmake lives, so without it this
# row panics in the build script and prints a red that reads exactly like a compile break.
row_or_skip check-cli-vision "$cmake_missing" cargo check -p np-cli --features local-model,vision
row test-workspace cargo test --workspace -- --nocapture
row test-cli-nodefaults cargo test -p np-cli --no-default-features -- --nocapture
row_or_skip test-cli-vision "$cmake_missing" cargo test -p np-cli --features local-model,vision -- --nocapture
row_or_skip test-core-vision "$cmake_missing" cargo test -p np-core --features local-model,vision -- --nocapture
# **The row that was missing until 2026-11-09.** `crates/np-cli/tests/serve.rs` gates its fetch and chunk-narration cases on
# `all(feature = "local-model", feature = "fetch")`, because a default build has no transport to narrate and no runtime to embed
# with. No other row compiles that pair for np-cli — `test-cli-vision` uses `vision`, `test-cli-realmodel` uses `local-model`
# alone — so every one of those cases ran as its "NOT COVERAGE" stub in every graded log, and the wire-level claims about byte
# checkpoints, pinned denominators and a terminal `refused -32000` had no row behind them at all. This is that row; with
# `NP_TEST_MODEL_PATH` set it also embeds a real artifact and reads the `embedding`/`writing` lines off the pipe.
row_or_skip test-cli-lm-fetch "$cmake_missing" cargo test -p np-cli --features local-model,fetch --test serve -- --nocapture
row test-config-nohome env -u HOME -u XDG_CONFIG_HOME cargo test -p np-core --test config -- --nocapture

RUSTDOCFLAGS="--deny warnings" row rustdoc-deny-default cargo doc --workspace --no-deps
row_or_skip rustdoc-deny-localmodel "$cmake_missing" env RUSTDOCFLAGS=--deny\ warnings cargo doc -p np-core -p np-cli --features local-model --no-deps
row_or_skip rustdoc-deny-lm-vision "$cmake_missing" env RUSTDOCFLAGS=--deny\ warnings cargo doc -p np-core -p np-cli --features local-model,vision --no-deps

# **The hand-written FFI against the header it mirrors.** `crates/np-core/src/llama_ffi.rs` declares llama.cpp's C API by
# hand (§14 says why), so a re-pin that changes a signature produces no compile error — only behaviour nothing predicts. This
# compares name, arity and parameter/return shape against the pinned tree's own headers, admits what it cannot compare
# (callbacks, opaque handles) instead of passing it silently, and fails on anything else. Skips when the pinned source is not
# on disk: `./scripts/vendor-llama-cpp.sh` once, or `NP_LLAMA_CPP_SRC`.
llama_src="${NP_LLAMA_CPP_SRC:-$PWD/.vendor/llama.cpp-v$(sed -n 's/^tag *= *"v\{0,1\}\([^"]*\)".*/\1/p' third_party/llama-cpp.toml | head -1)}"
decl_missing=""
[ -d "$llama_src" ] || decl_missing="no pinned llama.cpp source at $llama_src (run ./scripts/vendor-llama-cpp.sh or set NP_LLAMA_CPP_SRC)"
row_or_skip llama-decls "$decl_missing" env NP_LLAMA_CPP_SRC="$llama_src" python3 "$G/check-llama-decls.py"

# The three static fences. They live beside this script because a gate that is not in the repository is a gate that
# quietly stops running: these three lived in a git-ignored scratch directory, and every row here kept exiting 2 with
# "can't open file" until somebody noticed.
G=.github/scripts
row cfg-dead-code python3 "$G/check_cfg_dead_code.py"
row attr-displacement python3 "$G/check_attr_displacement.py"
row catalog-goldens python3 "$G/verify_catalog_goldens.py"
# The fences that check the fences. Each python fence carries `--self-test`: a tree of planted violations it must name,
# one per rule it claims to enforce. A rule nobody has ever seen fail is indistinguishable from a rule someone deleted,
# which is how three of these rows sat red for weeks with nothing but an exit code to say so.
for fence in check_cfg_dead_code check_attr_displacement verify_catalog_goldens check-dates-ahead check-links check-deps check-llama-decls check-unsafe-lints; do
  row "selftest-${fence}" python3 "$G/${fence}.py" --self-test
done

# **F31's harness arithmetic, with no binary and no model.** The metric functions are self-tested against hand-computed numbers, so
# this row costs milliseconds and catches the one failure mode that would matter: a scoring function that quietly flatters itself (an
# MRR computed over only the answered queries rises when recall falls). The harness's *usefulness* still needs labelled queries — see
# `docs/design-f31-quality-harness.md`, which is written to keep that distinction visible.
row f31-selftest python3 "$G/f31_eval.py" --self-test
# **A `run:` block that is not a program bash can read.** Actions parses a step's script when the step starts, so on 2026-11-13
# a mis-nested quote in `release.yml` was discovered by a runner — after it had compiled llama.cpp for Windows. This row costs
# milliseconds and is the difference between that and a red local run.
row workflow-shells "$G/check-workflow-shells.sh"
# **A date in the future, which is how a month gets written wrong.** A stretch of this project's prose was dated one
# month ahead of the work it described — "while cutting v0.3.0" beside a tag dated 2026-11-09, a spec first committed
# 2026-11-09 whose file name claimed another month — and a wrong month reads exactly like a right one, so nothing caught
# it. This row costs milliseconds: any calendar date in a tracked file later than HEAD's own commit date is reported.
row dates-ahead python3 "$G/check-dates-ahead.py"

# **A link is a promise.** Both spellings are checked — `[text](target)` and a backticked path — because documents move
# directories and the paths inside them do not move with them. The exclusions (sample data, archived plans, syntax written
# inside backticks) are listed in the script and pinned by its `--self-test` row below, so they cannot quietly widen into a
# checker that passes everything.
row links python3 "$G/check-links.py"

# **What this tree links, checked offline.** Licenses (every dependency must offer a permissive option) and one major version
# per crate unless an exception is recorded with a reason — plus a staleness check on `docs/dependency-licenses.md`, so the
# notices file's claim that the list is "derived, not hand-written" cannot mean "nobody regenerated it". The advisory half
# lives in CI (`supply-chain.yml`), because RustSec's database is not readable from a machine without network.
row deps python3 "$G/check-deps.py"
# **Nobody may mute the unsafe lints from inside a module.** Both crates deny them at the top; this fence fails if a crate
# stops denying either one, or if any file under `crates/` — tests included — allows one. A denial with a silent exception is
# a denial of everything except the block that needed it.
row unsafe-lints python3 "$G/check-unsafe-lints.py"

# The two rows that need real artifacts. They run only when the paths are set, because a vacuous pass here is the exact
# failure mode F33's review named: green because nothing was loaded.
artifacts=""
[ -n "${NP_TEST_MODEL_PATH:-}" ] || artifacts="NP_TEST_MODEL_PATH unset"
row_or_skip test-cli-realmodel "$artifacts" env CARGO_BUILD_JOBS=2 cargo test -p np-cli --features local-model -- --nocapture
[ -n "${NP_TEST_MMPROJ_PATH:-}" ] || artifacts="NP_TEST_MODEL_PATH or NP_TEST_MMPROJ_PATH unset"
row_or_skip test-core-realvision "$artifacts" env CARGO_BUILD_JOBS=2 cargo test -p np-core --features local-model,vision -- --nocapture

# **AddressSanitizer over the FFI boundary.** Nearly every `unsafe` site in this tree calls into llama.cpp, so the sanitizer
# has to be under the Rust as well as around it — which is why the row is a script that refuses to trust a cmake cache
# (`asan-localmodel.sh`: it builds a private copy of the pinned source with sanitizer flags and checks the archives for ASan
# symbols before running a single test). Skips, each naming what to install or export: no nightly toolchain, no `rust-src`,
# no cmake, or no pinned llama.cpp tree.
llama_src="${NP_LLAMA_CPP_SRC:-$PWD/.vendor/llama.cpp-v$(sed -n 's/^tag *= *"v\{0,1\}\([^"]*\)".*/\1/p' third_party/llama-cpp.toml | head -1)}"
asan_missing=""
[ "$have_cmake" = 1 ] || asan_missing="no cmake on PATH; set CMAKE"
rustup toolchain list 2>/dev/null | grep -q '^nightly' || asan_missing="$asan_missing; no nightly toolchain (rustup toolchain install nightly)"
rustup component list --toolchain nightly 2>/dev/null | grep -q '^rust-src.*installed' || asan_missing="$asan_missing; no rust-src for nightly"
[ -d "$llama_src" ] || asan_missing="$asan_missing; no pinned llama.cpp source at $llama_src (run ./scripts/vendor-llama-cpp.sh or set NP_LLAMA_CPP_SRC)"
row_or_skip asan-localmodel-vision "$asan_missing" env NP_LLAMA_CPP_SRC="$llama_src" .github/scripts/asan-localmodel.sh

# **`--exclude=driver.log`, and the reason it is a guard rather than a fix.** On a fresh log directory this cannot matter: the count
# runs *before* the summary line is echoed, so the phrase is not in that file yet — measured on 2026-11-09, where the tally over
# `$TMPDIR/gates-progress2` was 14 with and without the exclusion. It matters when a run reuses a directory holding an older
# log, because the previous verdict sentence does contain the phrase; that is exactly how the v0.3.0 release notes came to publish
# "zero" beside a tally of 1. Excluding it makes the count independent of what happened to be in the directory before.
vacuous=$(grep -rho --exclude=driver.log 'NOT COVERAGE' "$logs" 2>/dev/null | wc -l | tr -d ' ')
echo "vacuous  NOT COVERAGE lines across all logs: $vacuous"
echo "OVERALL fail=$fail"
[ "$fail" = 0 ] && echo "graded locally at $(git rev-parse --short HEAD 2>/dev/null) — logs in $logs"
exit "$fail"
