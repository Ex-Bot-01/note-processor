#!/usr/bin/env bash
# Build and package a release asset for this machine's target triple, in the shape `.github/workflows/release.yml`
# expects — same archive name, same "the binary is the only member" rule, same refusal of an empty archive.
#
# Why this file exists: the published assets were built by CI, and CI is metered (`docs/ops-actions-minutes.md`). When the
# allowance ran out on 2026-11-07 the release path stopped producing anything, which is how `v0.1.0` came to advertise four
# subcommands while shipping a binary with one. A human with a checkout can now produce the same artifact and say honestly
# where it came from — this script does **not** pretend to be the runner, and neither may the release notes that cite it.
#
# One platform per invocation, and only the platform you are standing on: a Windows asset needs the MSVC linker and a macOS
# asset needs Apple's SDK, so the other two rows of the release matrix stay CI's (or their own machine's) job — for Windows there is
# also `.github/scripts/cross-build-windows.sh`, which produces the `…-gnu-cross` asset from Linux. That limit is stated in
# `docs/ops-distribution-and-ci-decision.md` rather than hidden behind a script that appears to build releases.
#
# **Since 2026-11-08 this builds `local-model,fetch`**, so the asset contains the inference runtime (llama.cpp statically linked) and can
# embed and auto-fetch. That costs cmake and a C++ compiler at *build* time — not at run time, which is the point: the user's machine needs
# neither. §14's size ceiling (≤ 40 MB stripped) has ~24 MB of headroom on this shape, measured. The model itself is never in the asset.
set -euo pipefail

out="${1:-dist}"
# Absolute, because tar's `-C` composes against the directory it is already in (see the packaging line below for what that costs when it is a
# relative `.`), and because the script may be run from anywhere.
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1)
target=$(rustc -vV | sed -n 's/^host: //p')
binary=np
[ "$(uname -s)" = Windows_NT ] && binary=np.exe

# `local-model` compiles llama.cpp, which needs cmake; say so before cargo's build-script panic buries it. CMAKE names the binary when
# cmake is not on PATH (this repo's own gates set it that way).
if ! command -v "${CMAKE:-cmake}" >/dev/null 2>&1; then
  echo "local-model builds llama.cpp and needs cmake (set CMAKE=/path/to/cmake)." >&2
  exit 1
fi

cargo build --release --locked -p np-cli --features local-model,fetch --target "$target"

mkdir -p "$out"
name="np-$version-$target"
archive="$out/$name.tar.gz"
if [ "$(uname -s)" = Windows_NT ]; then
  archive="$out/$name.zip"
  echo "zip packaging is left to the workflow (pwsh Compress-Archive); this script builds and checksums only" >&2
else
  # Strip before packaging. §14's size ceiling has always been quoted against **stripped** binaries while the first assets shipped
  # unstripped (measured 2026-11-08: 10,295,048 B becomes 8,389,040 B for this target). Symbols are a debugging convenience; what
  # identifies the file for a downloader is the digest in the release notes.
  if command -v strip >/dev/null 2>&1; then
    strip "target/$target/release/$binary"
  else
    echo "warning: no strip on PATH — packaging an unstripped binary, larger than §14's quoted figures" >&2
  fi

  # The same promise cross-build-windows.sh makes for the Windows asset: a runtime claim that is not in the bytes is F40 all over again.
  # Read the strings, not the symbol table — `strip` removes the latter (measured: 2 symbols before, 0 after).
  if ! { strings -a "target/$target/release/$binary" | { grep -c 'llama_model_load_from_file' || true; }; } | grep -qv '^0$'; then
    echo "refusing to package: the binary carries no llama_model_load_from_file, so local-model is not in it." >&2
    exit 1
  fi

  # **MIT travels with the distribution.** `THIRD_PARTY_NOTICES.md` carries the copyright line and license text for the llama.cpp/ggml code
  # statically linked into this binary, and an MIT notice that lives only in the repository satisfies nobody: the obligation attaches to the
  # artefact a person downloads. So it goes inside the tarball, and the listing is checked rather than assumed — with `grep -c` rather than
  # `grep -q`, because `-q` exits on first match and can SIGPIPE `tar` into a failure that pipefail then reports as ours.
  test -s "$repo_root/THIRD_PARTY_NOTICES.md" || { echo "refusing to package: THIRD_PARTY_NOTICES.md is missing or empty" >&2; exit 1; }
  # **An absolute `-C`, not `-C .`.** Measured on the first run of this line: `tar` applies `-C` in order, so after
  # `-C target/x86_64-unknown-linux-gnu/release` a following `-C .` means *that* directory, and the notice file is not there —
  # `tar: THIRD_PARTY_NOTICES.md: Cannot stat`, exit 2, no asset. The listing check below never even ran, because the failure was
  # loud enough to stop the script; a quieter tar would have published an asset missing its license text.
  tar -C "target/$target/release" -czf "$archive" "$binary" -C "$repo_root" THIRD_PARTY_NOTICES.md
  if ! { tar -tzf "$archive" | { grep -c 'THIRD_PARTY_NOTICES\.md' || true; }; } | grep -qv '^0$'; then
    echo "refusing to publish: the asset does not contain THIRD_PARTY_NOTICES.md" >&2
    exit 1
  fi
fi

# The workflow's own guard, kept here too: a truncated or empty archive is worse than no release at all.
test -s "$archive"

( cd "$out" && sha256sum "$(basename "$archive")" > SHA256SUMS.txt )

bytes=$(stat -c%s "$archive" 2>/dev/null || stat -f%z "$archive")
echo "asset    $archive"
echo "target   $target"
echo "bytes    $bytes (binary before compression: $(stat -c%s "target/$target/release/$binary" 2>/dev/null || stat -f%z "target/$target/release/$binary"))"
echo "sha256   $(awk '{print $1}' "$out/SHA256SUMS.txt")"
