#!/usr/bin/env bash
# Build and package a release asset for this machine's target triple, in the shape `.github/workflows/release.yml`
# expects — same archive name, same "the binary is the only member" rule, same refusal of an empty archive.
#
# Why this file exists: the published assets were built by CI, and CI is metered (`docs/ops-actions-minutes.md`). When the
# allowance ran out on 2026-11-07 the release path stopped producing anything, which is how `v0.1.0` came to advertise four
# subcommands while shipping a binary with one. A human with a checkout can now produce the same artifact and say honestly
# where it came from — this script does **not** pretend to be the runner, and neither may the release notes that cite it.
#
# One platform per invocation, and only the platform you are standing on: a macOS asset needs Apple's SDK, which no
# cross-compiler here has, and `crates/np-core/build.rs` refuses to compile llama.cpp for MSVC rather than meet a linker
# error. The other shape this repository ships — `x86_64-pc-windows-gnu-cross` — comes from its own file,
# `.github/scripts/cross-build-windows.sh`, which cross-builds that asset from Linux with mingw-w64. That limit is stated in
# `docs/ops-distribution-and-ci-decision.md` rather than hidden behind a script that appears to build releases.
#
# **Since 2026-11-13 the release workflow runs this file.** It used to carry its own `cargo build` over a Windows/Linux/macOS
# matrix, and the two recipes disagreed about features: this script built `local-model,fetch`, the workflow built default
# features. Every asset CI produced was therefore a binary that could not embed while the release page said it could (F40).
# There is now one build definition, and it is the one in this file and its Windows sibling.
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

# **The asset is run by machines that did not build it.** `NP_LLAMA_PORTABLE=1` makes `build.rs` configure llama.cpp at the
# baseline x86-64 floor instead of `-march=native`: on 2026-11-13 the first CI-built Linux asset was compiled for its runner's CPU
# and answered `Illegal instruction` on an older x86-64 host before printing a version number. Override with
# `NP_LLAMA_PORTABLE=0` only when the machine building this is the machine that will run it — `build.rs` reads the flag back out of
# cmake's cache and refuses if the tree did not take it, so the promise cannot be decorative.
export NP_LLAMA_PORTABLE="${NP_LLAMA_PORTABLE-1}"
echo "codegen  NP_LLAMA_PORTABLE=$NP_LLAMA_PORTABLE (1 = baseline x86-64, runs on machines other than this one)"

cargo build --release --locked -p np-cli --features local-model,fetch --target "$target"

# **Say what the binary was actually compiled for.** `build.rs` refuses a portable build whose cmake cache disagrees, so a
# successful build already proves the configuration — but a release log that only says "it worked" cannot be read by anyone
# wondering why an asset will not start on their machine, and 2026-11-13 needed two rehearsals to find out that a flag we set
# was not the flag that mattered. So print the cache values it checked, and count wide-vector instructions in the linked
# binary: `%zmm` must be zero at the baseline floor, and `%ymm` is reported rather than asserted because Rust crates carry
# runtime-dispatched AVX2 of their own (a pre-AVX host measured 4,940 of them and runs fine). The instruction counts are the
# part that caught the real fault: they did not move between two builds whose cmake options differed.
cache=$(find "target/$target/release/build" -path '*/out/*/CMakeCache.txt' -newer Cargo.toml 2>/dev/null | head -1)
if [ -n "$cache" ]; then
  echo "cmake cache used by this link: $cache"
  grep -E '^(GGML_NATIVE|GGML_SSE42|GGML_AVX|GGML_AVX2|GGML_FMA|GGML_F16C|GGML_BMI2|GGML_OPENMP):' "$cache" | sed 's/^/  /'
else
  echo "note: no CMakeCache.txt found under target/$target/release/build — nothing linked llama.cpp?" >&2
fi
binary="target/$target/release/np${ext}"
if command -v objdump >/dev/null 2>&1 && [ -f "$binary" ]; then
  objdump -d --section=.text "$binary" 2>/dev/null | awk '
    /%zmm/ { z++ } /%ymm/ { y++ }
    END { printf "codegen  .text instructions: %%zmm %d (must be 0 at the baseline floor), %%ymm %d\n", z + 0, y + 0 }'
fi

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
  # **Stage, then strip the copy.** Stripping `target/.../$binary` in place makes a second run of this script re-strip an already-stripped file and
  # emit different bytes from an unchanged build (measured across five v0.3.0 packaging runs: same compile, five archive hashes). A published digest
  # should describe the artefact, not the number of times the packager was run.
  stage="$out/.staging"
  rm -rf "$stage"
  mkdir -p "$stage"
  cp "target/$target/release/$binary" "$stage/$binary"
  if command -v strip >/dev/null 2>&1; then
    strip "$stage/$binary"
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
  # Deterministic archive: sorted names, numeric root ownership, a fixed mtime, and `gzip -n` so the compressor stores neither a filename nor the
  # clock. Every one of those removes a fact that has no business being inside something whose sha256 is published for other people to check.
  tar --sort=name --owner=0 --group=0 --numeric-owner --mtime='UTC 2026-01-01' \
      -cf - -C "$stage" "$binary" -C "$repo_root" THIRD_PARTY_NOTICES.md LICENSE \
    | gzip -n > "$archive"
  # Both files, counted: the MIT notice for the code we link and the licence for our own. A count rather than one grep per name because a single
  # missing member must still fail, and `grep -c` over the listing answers "did both land" in one pass without SIGPIPE-ing tar (see above).
  listed=$(tar -tzf "$archive" | { grep -cE '(THIRD_PARTY_NOTICES\.md|^LICENSE$)' || true; })
  if [ "$listed" -lt 2 ]; then
    echo "refusing to publish: the asset carries $listed of 2 required licence files" >&2
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
