#!/usr/bin/env bash
# Cross-build the Windows release asset **from Linux**, with mingw-w64, and package it in the shape the release page expects.
#
# Why a script and not just the recipe in `docs/ops-cross-build-windows-gnu.md`: that document is how v0.2.1 and v0.2.2 got their Windows
# asset, and both times the steps were retyped from prose by whoever was holding the release — the kind of thing that silently drifts (a
# forgotten env var, a missing DLL check). This file keeps three decisions in one place:
#
#   * **which toolchain** (an unpacked mingw-w64 tree, no root, nothing installed system-wide);
#   * **the import-table guard**, which is the test that decides whether the artifact is usable at all: if `libgcc_s_seh-1.dll` or
#     `libwinpthread-1.dll` ever appear in it, `np.exe` is no longer a single file you can drop on a machine, and this script refuses
#     rather than shipping a folder of DLLs behind a zip;
#   * **the naming**, `-gnu-cross` in the file name, because an asset built here is not the MSVC build CI would produce and the name is
#     where that difference is visible before someone downloads it.
#
# What this artifact is NOT: not MSVC, unsigned, and not graded by CI. What it IS since 2026-11-08: built **with** `local-model` and
# `fetch`, so the shipped exe contains the inference runtime (llama.cpp v0.8.0, statically linked) and can embed and auto-fetch like a
# source build. Before that date this script produced a binary with no runtime at all, which is why `np embed` on a downloaded asset said
# `runtime not built`; the story of how that changed, with the four measurements that made it possible, is in
# `docs/ops-cross-build-windows-gnu.md`.
#
# Three things this needs that the runtime-less build did not: a C++ compiler (`g++-mingw-w64-x86-64-win32`, same rootless .deb recipe),
# a cmake toolchain file for the cross build, and the **pinned llama.cpp tree** (`scripts/vendor-llama-cpp.sh`, or `NP_LLAMA_CPP_SRC`).
# The win32 threading model is not a preference: with posix-threaded drivers the exe imports `libwinpthread-1.dll`, and the guard below —
# correctly — refuses to ship it. Measured both ways on 2026-11-08.
#
# Usage:  NP_LLAMA_CMAKE_TOOLCHAIN=/path/to/mingw-toolchain.cmake .github/scripts/cross-build-windows.sh [out-dir]
# Env:    MINGW_BIN (default /home/agent/tmp/mingw/usr/bin), TARGET (default x86_64-pc-windows-gnu),
#         NP_LLAMA_CMAKE_TOOLCHAIN (required; a cmake toolchain naming the win32 mingw drivers), CMAKE (cmake binary)

set -euo pipefail

out="${1:-dist-win}"
target="${TARGET:-x86_64-pc-windows-gnu}"
mingw_bin="${MINGW_BIN:-/home/agent/tmp/mingw/usr/bin}"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

if [ ! -x "$mingw_bin/x86_64-w64-mingw32-gcc" ]; then
  cat >&2 <<EOF
no mingw-w64 at $MINGW_BIN (set MINGW_BIN if it lives elsewhere).

It needs no root and no apt-install — Ubuntu's .debs are ar+tar archives:
  mkdir -p /home/agent/tmp/mingwdebs && cd /home/agent/tmp/mingwdebs
  apt-get install --print-uris -y gcc-mingw-w64-x86-64 | grep "^'" | sed "s/'\\([^']*\\)'.*/\\1/" > uris.txt
  xargs -n1 -P4 curl -sS -O < uris.txt
  mkdir -p ../mingw && for d in *.deb; do dpkg-deb -x "\$d" ../mingw; done
Then symlink the posix build to the plain name if it is missing:
  ln -sf x86_64-w64-mingw32-gcc-13-posix $MINGW_BIN/x86_64-w64-mingw32-gcc
Full background: docs/ops-cross-build-windows-gnu.md
EOF
  exit 1
fi

# The win32 drivers, not the posix ones: see the header. A toolchain file that names the posix pair produces an exe the guard refuses, so
# refusing here as well would be polite but late — and `build.rs` checks the same fact out of cmake's own records.
for need in x86_64-w64-mingw32-gcc-win32 x86_64-w64-mingw32-g++-win32; do
  if [ ! -x "$mingw_bin/$need" ]; then
    echo "no $need in $MINGW_BIN — the C++ half needs g++-mingw-w64-x86-64-win32 (rootless .deb recipe in docs/ops-cross-build-windows-gnu.md)." >&2
    exit 1
  fi
done

if [ -z "${NP_LLAMA_CMAKE_TOOLCHAIN:-}" ]; then
  cat >&2 <<'EOF'
NP_LLAMA_CMAKE_TOOLCHAIN is not set. `local-model` cross-compiles llama.cpp, and a cmake toolchain file is how it learns to build for
Windows instead of for this Linux host. It has to name the win32 mingw drivers; a minimal one is:

  set(CMAKE_SYSTEM_NAME Windows)
  set(CMAKE_SYSTEM_PROCESSOR x86_64)
  set(TOOL_PREFIX /home/agent/tmp/mingw/usr/bin/x86_64-w64-mingw32-)
  set(CMAKE_C_COMPILER   ${TOOL_PREFIX}gcc-win32)
  set(CMAKE_CXX_COMPILER ${TOOL_PREFIX}g++-win32)
  set(CMAKE_AR           ${TOOL_PREFIX}ar)
  set(CMAKE_RANLIB       ${TOOL_PREFIX}ranlib)
  set(CMAKE_RC_COMPILER  ${TOOL_PREFIX}windres)

and the pinned source tree comes from scripts/vendor-llama-cpp.sh (or set NP_LLAMA_CPP_SRC at it).
EOF
  exit 1
fi
test -f "$NP_LLAMA_CMAKE_TOOLCHAIN" || { echo "NP_LLAMA_CMAKE_TOOLCHAIN=$NP_LLAMA_CMAKE_TOOLCHAIN is not a file" >&2; exit 1; }
if ! grep -q 'win32' "$NP_LLAMA_CMAKE_TOOLCHAIN"; then
  echo "$NP_LLAMA_CMAKE_TOOLCHAIN names no win32 driver; the exe would import libwinpthread-1.dll and the guard below would be right to refuse it." >&2
  exit 1
fi

export PATH="$mingw_bin:$HOME/.cargo/bin:$PATH"
export CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc-win32
export CXX_x86_64_pc_windows_gnu=x86_64-w64-mingw32-g++-win32
export AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc-win32

version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1)
rustup target list --installed 2>/dev/null | grep -qx "$target" || rustup target add "$target"

echo "build    cargo build --release --locked -p np-cli --features local-model,fetch --target $target"
cargo build --release --locked -p np-cli --features local-model,fetch --target "$target"

exe="target/$target/release/np.exe"
test -s "$exe" || { echo "no binary at $exe" >&2; exit 1; }

# **The guard that decides whether this is shippable.** A GNU-target Windows binary can grow a dependency on the mingw runtime and the
# winpthread shim; either one turns `np.exe` into "put these DLLs somewhere on PATH", which is a different product than one file you drop
# in a folder. Both have to be absent for the asset to mean what its name says.
dlls=$(x86_64-w64-mingw32-objdump -p "$exe" | sed -n 's/^[[:space:]]*DLL Name: //p' | sort -u)
echo "imports  $(echo "$dlls" | tr '\n' ' ')"
for forbidden in libgcc_s_seh-1.dll libwinpthread-1.dll libstdc++-6.dll; do
  if echo "$dlls" | grep -qix "$forbidden"; then
    echo "refusing to package: $exe imports $forbidden, so it is no longer a single-file artifact." >&2
    echo "Fix the link (static CRT) before shipping; see docs/ops-cross-build-windows-gnu.md's import-table row." >&2
    exit 1
  fi
done

# Did the runtime actually land in the binary? A release asset that claims inference and does not carry it is how F40 was filed, so this
# checks the artifact instead of trusting a feature list. Two hazards in one line, both measured here on 2026-11-08:
#
#   * **read the bytes, not the symbol table.** `objdump -t` reports nothing once an exe is stripped (2 symbols before, 0 after), so a
#     symbol test would refuse every properly packaged artifact. The name survives as a string because llama.cpp carries it in its own
#     logging — and a runtime-less binary has no copy of it: the published v0.2.3 asset, used here as the mutant, matches zero times.
#   * **no `grep -q` under `pipefail`.** `-q` exits at the first match, objdump/strings then die on SIGPIPE, and `pipefail` reports 141 for
#     a check that matched — measured: this guard refused a binary containing the symbol four times over.
runtime_hits=$(strings -a "$exe" | { grep -c 'llama_model_load_from_file' || true; })
if [ "${runtime_hits:-0}" -eq 0 ]; then
  echo "refusing to package: $exe carries no llama_model_load_from_file, so the inference runtime is not in it." >&2
  exit 1
fi
echo "runtime  llama.cpp present ($runtime_hits copies of the loader symbol name)"

# Strip for the asset. §14's ≤ 40 MB ceiling has been quoted against **stripped** sizes since it was written, while every asset so far
# shipped unstripped — measured 2026-11-08: this build 23,297,605 B becomes 16,253,440 B; the runtime-less v0.2.3 asset was 14,073,097 B
# where a stripped one is 9,952,768 B. Both numbers are printed below because the release notes quote the archive and a report quotes the exe.
unstripped=$(stat -c%s "$exe")
x86_64-w64-mingw32-strip "$exe"
echo "strip    $unstripped B -> $(stat -c%s "$exe") B"

mkdir -p "$out"
name="np-$version-$target-cross"
archive="$out/$name.zip"
# **MIT travels with the distribution** (see `THIRD_PARTY_NOTICES.md`): the license text for the statically linked llama.cpp/ggml code has to be
# inside the zip, not merely in the repository the zip came from. The listing is then read back from the archive, because "we meant to add it" and
# "the downloader receives it" are different claims.
notices="$repo_root/THIRD_PARTY_NOTICES.md"
test -s "$notices" || { echo "refusing to package: THIRD_PARTY_NOTICES.md is missing or empty" >&2; exit 1; }
python3 - "$exe" "$archive" "$notices" <<'PY'
import sys, zipfile
exe, archive, notices = sys.argv[1], sys.argv[2], sys.argv[3]
with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as z:
    z.write(exe, "np.exe")
    z.write(notices, "THIRD_PARTY_NOTICES.md")
with zipfile.ZipFile(archive) as z:
    names = z.namelist()
assert "THIRD_PARTY_NOTICES.md" in names, f"asset lacks the notice file: {names}"
assert "np.exe" in names, f"asset lacks the binary: {names}"
PY
test -s "$archive"

# Both hashes, because the release notes quote both: the zip a downloader verifies, and the exe inside it that a report quotes when it says
# "the binary I ran had sha256 …".
exe_sha=$(sha256sum "$exe" | cut -d' ' -f1)
archive_sha=$(sha256sum "$archive" | cut -d' ' -f1)
{
  printf '%s  np.exe\n' "$exe_sha"
  printf '%s  %s\n' "$archive_sha" "$(basename "$archive")"
} > "$out/SHA256SUMS.txt"

echo "asset    $archive  $(stat -c%s "$archive") B  sha256 $archive_sha"
echo "inner    np.exe    $(stat -c%s "$exe") B  sha256 $exe_sha"
echo
echo "Say in the release notes: cross-built with mingw-w64 on Linux, unsigned, NOT MSVC, built WITH \`local-model\` and \`fetch\` — so it"
echo "embeds, and F38's pinned auto-fetch is ON by default (kill switch: NP_NO_AUTO_FETCH=1). The model itself is never in the asset:"
echo "309 MB arrives only when a human runs np setup or np embed. And say plainly that no CI run has graded this tree."
