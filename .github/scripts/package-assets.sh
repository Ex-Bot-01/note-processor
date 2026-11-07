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
# asset needs Apple's SDK, so the other two rows of the release matrix stay CI's (or their own machine's) job. That limit is
# stated in `docs/ops-distribution-and-ci-decision.md` rather than hidden behind a script that appears to build releases.
set -euo pipefail

out="${1:-dist}"
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1)
target=$(rustc -vV | sed -n 's/^host: //p')
binary=np
[ "$(uname -s)" = Windows_NT ] && binary=np.exe

cargo build --release --locked -p np-cli --target "$target"

mkdir -p "$out"
name="np-$version-$target"
archive="$out/$name.tar.gz"
if [ "$(uname -s)" = Windows_NT ]; then
  archive="$out/$name.zip"
  echo "zip packaging is left to the workflow (pwsh Compress-Archive); this script builds and checksums only" >&2
else
  tar -C "target/$target/release" -czf "$archive" "$binary"
fi

# The workflow's own guard, kept here too: a truncated or empty archive is worse than no release at all.
test -s "$archive"

( cd "$out" && sha256sum "$(basename "$archive")" > SHA256SUMS.txt )

bytes=$(stat -c%s "$archive" 2>/dev/null || stat -f%z "$archive")
echo "asset    $archive"
echo "target   $target"
echo "bytes    $bytes (binary before compression: $(stat -c%s "target/$target/release/$binary" 2>/dev/null || stat -f%z "target/$target/release/$binary"))"
echo "sha256   $(awk '{print $1}' "$out/SHA256SUMS.txt")"
