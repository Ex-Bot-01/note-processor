#!/usr/bin/env bash
# Run the `local-model,vision` tests under AddressSanitizer, and prove the sanitizer was actually in the build.
#
# Why this is a script rather than one line in `gates.sh`: three ways this row can look green while checking nothing, and
# each needs a deliberate answer rather than a hope.
#
#  1. **The vendored C++ is where the danger is.** Almost every `unsafe` site in this tree calls into llama.cpp, so an ASan
#     run over Rust alone would grade the wrapper and skip the code that can corrupt memory. `build.rs` reuses one cmake
#     directory for host builds, so setting `CFLAGS` on an already-configured tree changes nothing — cmake keeps its cached
#     flags, and cargo is not even told to re-run (its `rerun-if-env-changed` list names `CC`/`CXX`, not `CFLAGS`). So this
#     script builds from a **private copy** of the pinned source into a private directory, and then checks the archives for
#     ASan symbols before trusting a single test result.
#  2. **A separate target directory.** ASan objects in the shared `target/` would either be relinked wrongly or make an
#     ordinary build fail with a linker error that reads like a code break.
#  3. **Nightly is not a given.** `gates.sh` skips this row (loudly) when the toolchain, `rust-src`, cmake or the pinned
#     source is missing; it never reports a skip as a pass.
set -euo pipefail

SRC="${NP_LLAMA_CPP_SRC:?asan: name the pinned llama.cpp tree with NP_LLAMA_CPP_SRC}"
VERSION="$(sed -n 's/^tag *= *"v\{0,1\}\([^"]*\)".*/\1/p' third_party/llama-cpp.toml | head -1)"
[ -n "$VERSION" ] || { echo "asan: cannot read the pinned tag from third_party/llama-cpp.toml"; exit 1; }

# **The working directory is on disk, not in `/tmp`.** Measured here: `/tmp` is a 3.7 GiB tmpfs, and an ASan-instrumented
# translation unit of `llama-model-loader.cpp` overflowed it — `fatal error: error writing to /tmp/cc*.s: Disk quota
# exceeded`, which looks like an account limit and is really the compiler's temporaries having nowhere to go. So the tree,
# the target directory and `TMPDIR` itself all live under `$HOME/tmp/np-asan`.
work="${NP_ASAN_DIR:-$HOME/tmp/np-asan}"
build="$work/llama.cpp-v$VERSION"
export CARGO_TARGET_DIR="$work/target"
export TMPDIR="$work/cc"
mkdir -p "$CARGO_TARGET_DIR" "$TMPDIR"

# A fresh copy, so cmake configures from scratch with the sanitizer flags rather than reusing a cached host build.
if [ ! -f "$work/.asan-symbols-ok" ]; then
  rm -rf "$build"
  mkdir -p "$(dirname "$build")"
  cp -a "$SRC" "$build"
  rm -rf "$build/build"
fi

export NP_LLAMA_CPP_SRC="$build"
# **One compile job, measured rather than guessed.** ASan-instrumented unity builds of the pinned tree push a single cc1plus
# past a gigabyte; at this host's default parallelism several were OOM-killed mid-build (`c++: fatal error: Killed signal
# terminated program cc1plus`), which reads exactly like a compile break in the sanitizer. Slow and true beats fast and
# misdiagnosed — raise it on a machine with memory to spare.
export NP_LLAMA_CPP_JOBS="${NP_LLAMA_CPP_JOBS:-1}"
export CMAKE="${CMAKE:-cmake}"
export CFLAGS="-fsanitize=address -fno-omit-frame-pointer -g ${CFLAGS:-}"
export CXXFLAGS="-fsanitize=address -fno-omit-frame-pointer -g ${CXXFLAGS:-}"
export RUSTFLAGS="-Zsanitizer=address -Cforce-frame-pointers ${RUSTFLAGS:-}"
target="$(rustc -vV | sed -n 's/^host: //p')"

echo "asan: building np-core (local-model,vision) against $build, target $target"
cargo +nightly test -Zbuild-std --target "$target" -p np-core --features local-model,vision --lib --no-run

# **The guard.** If the archives carry no ASan references, the C++ was built without the sanitizer and every result below
# describes a build that is not the one this row claims to grade. Say so instead of passing.
# The cmake tree lands under cargo's output directory, not beside the sources — `build.rs` configures into
# `$CARGO_TARGET_DIR/.../out/llama-cpp-*`, so searching the source copy finds nothing and reports a false gap.
archives=$(find "$CARGO_TARGET_DIR" -name 'libllama.a' -o -name 'libggml*.a' -o -name 'libmtmd.a' 2>/dev/null)
[ -n "$archives" ] || { echo "NOT COVERAGE asan: no llama.cpp archives under $CARGO_TARGET_DIR"; exit 1; }
# An ASan-compiled object emits `__odr_asan.*` indicators and references to the runtime; neither appears in a plain build.
instrumented=0
for a in $archives; do
  n=$(nm "$a" 2>/dev/null | grep -cE '__odr_asan|__asan_|__lsan' || true)
  [ "$n" -gt 0 ] && instrumented=$((instrumented + 1))
done
[ "$instrumented" -gt 0 ] || { echo "NOT COVERAGE asan: 0 of $(echo "$archives" | wc -l) archives carry ASan symbols"; exit 1; }
touch "$work/.asan-symbols-ok"
echo "asan: $instrumented archive(s) instrumented — the C++ under test is the sanitized one"

cargo +nightly test -Zbuild-std --target "$target" -p np-core --features local-model,vision --lib -- --nocapture
echo "ASAN CHECK: pass"
