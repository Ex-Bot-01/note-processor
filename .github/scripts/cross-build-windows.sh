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
# Env:    MINGW_BIN (default: the directory `x86_64-w64-mingw32-gcc` is found in on PATH),
#         TARGET (default x86_64-pc-windows-gnu),
#         NP_LLAMA_CMAKE_TOOLCHAIN (required; a cmake toolchain naming the win32 mingw drivers), CMAKE (cmake binary)

set -euo pipefail

out="${1:-dist-win}"
target="${TARGET:-x86_64-pc-windows-gnu}"
# **No machine-specific default baked in.** This line hardcoded one operator's scratch directory, which made the script unrunnable for anybody else and
# printed a confusing "no mingw-w64 at " (empty `$MINGW_BIN`) when they tried. The toolchain is found on PATH now; MINGW_BIN stays as the override for
# anyone who unpacked it somewhere unusual.
gcc_on_path="$(command -v x86_64-w64-mingw32-gcc || true)"
mingw_bin="${MINGW_BIN:-${gcc_on_path%/*}}"
if [ -z "${mingw_bin:-}" ] || [ "$mingw_bin" = "$gcc_on_path" ]; then mingw_bin=""; fi
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

if [ ! -x "$mingw_bin/x86_64-w64-mingw32-gcc" ]; then
  cat >&2 <<EOF
no mingw-w64 toolchain found${MINGW_BIN:+ at $MINGW_BIN}${MINGW_BIN:- on PATH as x86_64-w64-mingw32-gcc}.
Set MINGW_BIN to the directory holding x86_64-w64-mingw32-gcc if it lives somewhere unusual.

It needs no root and no apt-install — Ubuntu's .debs are ar+tar archives:
  mkdir -p "\${TMPDIR:-/tmp}/mingwdebs" && cd "\${TMPDIR:-/tmp}/mingwdebs"
  apt-get install --print-uris -y gcc-mingw-w64-x86-64 | grep "^'" | sed "s/'\\([^']*\\)'.*/\\1/" > uris.txt
  xargs -n1 -P4 curl -sS -O < uris.txt
  mkdir -p ../mingw && for d in *.deb; do dpkg-deb -x "\$d" ../mingw; done
Then symlink the posix build to the plain name if it is missing:
  ln -sf x86_64-w64-mingw32-gcc-13-posix <that directory>/x86_64-w64-mingw32-gcc
Full background: docs/ops-cross-build-windows-gnu.md
EOF
  exit 1
fi

# The win32 drivers, not the posix ones: see the header. A toolchain file that names the posix pair produces an exe the guard refuses, so
# refusing here as well would be polite but late — and `build.rs` checks the same fact out of cmake's own records.
for need in x86_64-w64-mingw32-gcc-win32 x86_64-w64-mingw32-g++-win32; do
  if [ ! -x "$mingw_bin/$need" ]; then
    echo "no $need in $mingw_bin — the C++ half needs g++-mingw-w64-x86-64-win32 (rootless .deb recipe in docs/ops-cross-build-windows-gnu.md)." >&2
    exit 1
  fi
done

if [ -z "${NP_LLAMA_CMAKE_TOOLCHAIN:-}" ]; then
  cat >&2 <<'EOF'
NP_LLAMA_CMAKE_TOOLCHAIN is not set. `local-model` cross-compiles llama.cpp, and a cmake toolchain file is how it learns to build for
Windows instead of for this Linux host. It has to name the win32 mingw drivers; a minimal one is:

  set(CMAKE_SYSTEM_NAME Windows)
  set(CMAKE_SYSTEM_PROCESSOR x86_64)
  set(TOOL_PREFIX <directory holding the mingw binaries>/x86_64-w64-mingw32-)
  set(CMAKE_C_COMPILER   ${TOOL_PREFIX}gcc-win32)
  set(CMAKE_CXX_COMPILER ${TOOL_PREFIX}g++-win32)
  set(CMAKE_AR           ${TOOL_PREFIX}ar)
  set(CMAKE_RANLIB       ${TOOL_PREFIX}ranlib)
  set(CMAKE_RC_COMPILER  ${TOOL_PREFIX}windres)
  # (replace the TOOL_PREFIX placeholder with a real directory — printed on the next line, if a toolchain was found at all)

and the pinned source tree comes from scripts/vendor-llama-cpp.sh (or set NP_LLAMA_CPP_SRC at it).
EOF
  # Hand the caller the directory this machine resolved, so the placeholder above is a copy-paste rather than a scavenger hunt.
  if [ -n "$mingw_bin" ]; then
    echo "on this machine that directory is: $mingw_bin" >&2
  fi
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

# A cross build already gets the portable floor from upstream (`GGML_NATIVE_DEFAULT` is OFF when the target is not the host), so
# this mostly documents intent — but setting it also turns on `build.rs`'s cache check, which refuses the build if any option that
# raises the instruction-set floor came out ON. The exe a Windows user downloads has to run on their CPU, not on Linux's.
export NP_LLAMA_PORTABLE="${NP_LLAMA_PORTABLE-1}"
echo "codegen  NP_LLAMA_PORTABLE=$NP_LLAMA_PORTABLE (baseline x86-64; checked against cmake's cache by build.rs)"

# The Rust half of the same floor. A cross target already defaults to plain x86-64, but this asset is built on a machine whose
# `RUSTFLAGS` may say otherwise — and 2026-11-13 measured exactly that on the Linux sibling: two assets with identical cmake
# caches, one with nine times the AVX in it. State the floor instead of inheriting the host's.
if [ "${NP_LLAMA_PORTABLE:-0}" != "0" ]; then
  export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-cpu=x86-64"
  # The C that bypasses cmake too: `libsqlite3-sys` and `ring` build through the `cc` crate, which reads `CFLAGS` from the
  # environment. A cross build gets its floor for llama.cpp from the toolchain file and nothing at all for those two — and on
  # 2026-11-13 that is where the last AVX in a "portable" Linux asset was hiding, with `np scan` dying of `SIGILL` on a pre-AVX
  # host while `np --version`, which never opens a database, ran fine.
  export CFLAGS="${CFLAGS:+$CFLAGS }-march=x86-64"
  export CXXFLAGS="${CXXFLAGS:+$CXXFLAGS }-march=x86-64"
  echo "codegen  RUSTFLAGS=$RUSTFLAGS"
  echo "codegen  CFLAGS=$CFLAGS"
fi

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
# **Strip a copy, never the build artefact.** Measured on 2026-12-09 while cutting v0.3.0: five consecutive runs of this script produced five different
# `np.exe` hashes from a build cargo reported unchanged ("Finished in 0.04s"), and the reason turned out to be two separate things — worth separating,
# because only one of them is ours to fix:
#
#   * **Ours:** the script stripped `target/.../np.exe` *in place*, so every run mutated the build artefact and the next run packaged a file that had
#     been through strip again. Fixed by staging: copy, strip the copy, package and hash that. `target/` is now left byte-identical (verified before
#     and after a run: `9871b857…` both times).
#   * **Not ours:** `x86_64-w64-mingw32-strip` on this toolchain is not deterministic across invocations. Three strips of the same input inside one
#     shell invocation produce three identical files (`cb74ea02…`, `cmp` reports zero differing bytes); repeating the same command minutes later yields
#     a different file from the same 16,326,144-byte input (`b0b975a9…`, then `ef3b059e…`). File mtime was ruled out (two copies stamped 2020 and 2030
#     strip identically), as was the archive layer (zip member timestamps are pinned to 1980-01-01, and `LICENSE` plus `THIRD_PARTY_NOTICES.md` hash
#     identically across every run).
#
# The consequence is stated where a user can read it rather than buried here: **the Windows asset is not bit-reproducible today.** Its published sha256
# identifies the file you downloaded — verify that, do not try to reproduce it by rebuilding. The Linux archive *is* reproducible (two runs of
# `package-assets.sh` produced byte-identical tarballs once staging plus `--sort=name --owner=0 --group=0 --mtime` and `gzip -n` were in place).
stage="$out/.staging"
mkdir -p "$stage"
staged="$stage/np.exe"
cp "$exe" "$staged"
unstripped=$(stat -c%s "$staged")
x86_64-w64-mingw32-strip "$staged"
echo "strip    $unstripped B -> $(stat -c%s "$staged") B (in a staged copy; target/ is left untouched)"
exe="$staged"

mkdir -p "$out"
name="np-$version-$target-cross"
archive="$out/$name.zip"
# **MIT travels with the distribution** (see `THIRD_PARTY_NOTICES.md`): the license text for the statically linked llama.cpp/ggml code has to be
# inside the zip, not merely in the repository the zip came from. The listing is then read back from the archive, because "we meant to add it" and
# "the downloader receives it" are different claims.
notices="$repo_root/THIRD_PARTY_NOTICES.md"
licence="$repo_root/LICENSE"
test -s "$notices" || { echo "refusing to package: THIRD_PARTY_NOTICES.md is missing or empty" >&2; exit 1; }
test -s "$licence" || { echo "refusing to package: LICENSE is missing or empty" >&2; exit 1; }
python3 - "$exe" "$archive" "$notices" "$licence" <<'PY'
import os, sys, zipfile
exe, archive, notices, licence = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]

def put(z, name, path):
    """Write one member with a FIXED timestamp.

    zipfile stamps every entry with the current clock by default, so two runs over identical bytes produce different archives — which turns a
    published sha256 into a receipt for one particular minute rather than a fact anyone can check. 1980-01-01 is zip's epoch; there is no
    information in "when did the packager run" that belongs in an artefact a person verifies.
    """
    with open(path, "rb") as handle:
        payload = handle.read()
    info = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
    info.compress_type = zipfile.ZIP_DEFLATED
    # Keep the executable bit for the binary only; a zip member's mode is what an unzipper hands to chmod.
    info.external_attr = (0o755 if name.endswith(".exe") else 0o644) << 16
    z.writestr(info, payload)

with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as z:
    put(z, "np.exe", exe)
    put(z, "THIRD_PARTY_NOTICES.md", notices)
    put(z, "LICENSE", licence)
with zipfile.ZipFile(archive) as z:
    names = z.namelist()
for want in ("np.exe", "THIRD_PARTY_NOTICES.md", "LICENSE"):
    assert want in names, f"asset lacks {want}: {names}"
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
