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
# What this artifact is NOT: not MSVC, unsigned, and built **without** `local-model` (llama.cpp for that target is its own project — see
# the doc). It cannot embed and, since F38, it therefore cannot auto-fetch either: the trigger is compiled out of a binary with no runtime
# to read the bytes with. The release notes must say all of this; the script prints a reminder because the notes are written by whoever ran it.
#
# Usage:  .github/scripts/cross-build-windows.sh [out-dir]
# Env:    MINGW_BIN (default /home/agent/tmp/mingw/usr/bin), TARGET (default x86_64-pc-windows-gnu)

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

export PATH="$mingw_bin:$HOME/.cargo/bin:$PATH"
export CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc
export AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc

version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1)
rustup target list --installed 2>/dev/null | grep -qx "$target" || rustup target add "$target"

echo "build    cargo build --release --locked -p np-cli --target $target"
cargo build --release --locked -p np-cli --target "$target"

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

mkdir -p "$out"
name="np-$version-$target-cross"
archive="$out/$name.zip"
python3 - "$exe" "$archive" <<'PY'
import sys, zipfile
exe, archive = sys.argv[1], sys.argv[2]
with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as z:
    z.write(exe, "np.exe")
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
echo "Say in the release notes: cross-built with mingw-w64 on Linux, unsigned, NOT MSVC, built without \`local-model\` (so no embed and no"
echo "auto-fetch), and not graded by CI. Nothing here replaces a hosted-runner build; it replaces silence."
