#!/usr/bin/env bash
# Fetch and verify the pinned llama.cpp source — the ONE network step in this repository's build.
#
# Why a script instead of doing it in `crates/np-core/build.rs`: a build script that quietly reaches
# out is a build that fails in ways we cannot reproduce (§14: "no network during `cargo build` by
# default"). So the fetch is explicit, run by a human or by the one CI job that asks for it, and the
# build script only ever reads a directory.
#
# What it does: read the pin from `third_party/llama-cpp.toml`, download the tarball, verify its
# sha256 against the pin (refusing on mismatch, printing both digests), unpack to
# `.vendor/llama.cpp-<tag>/` (gitignored), and print that path. Idempotent: a directory whose stamp
# file already records the pinned digest is left alone and nothing is downloaded.
#
# After it succeeds, build with the feature on:
#     cargo test -p np-core --features local-model --test local_runtime
# cmake is not on PATH here; if yours lives elsewhere, name it — build.rs honours `CMAKE`:
#     CMAKE=$HOME/tools/cmake/bin/cmake  # Kitware tarball, no root needed (§14)
# Or skip vendoring entirely and point at any tree that is the pin:
#     NP_LLAMA_CPP_SRC=/path/to/llama.cpp-0.8.0
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
pin="$root/third_party/llama-cpp.toml"
vendor="$root/.vendor"

# A quoted string on one line, nothing else — the same subset build.rs parses (§14: one pin, two readers).
pin_value() {
    sed -n 's/^'"$1"' = "\(.*\)"$/\1/p' "$pin" | head -n 1
}

[[ -f "$pin" ]] || { echo "vendor-llama-cpp: no pin file at $pin" >&2; exit 1; }

tag="$(pin_value tag)"
url="$(pin_value url)"
want_sha="$(pin_value sha256)"

for pair in "tag=$tag" "url=$url" "sha256=$want_sha"; do
    [[ -n "${pair#*=}" ]] || { echo "vendor-llama-cpp: $pin has no ${pair%%=*}" >&2; exit 1; }
done

dest="$vendor/llama.cpp-$tag"

if [[ -d "$dest" ]]; then
    have_sha="$(sed -n 's/^sha256=\(.*\)$/\1/p' "$dest/.vendor-stamp" 2>/dev/null | head -n 1 || true)"
    if [[ "$have_sha" == "$want_sha" ]]; then
        echo "already vendored: $dest (stamp sha256 $have_sha matches the pin; nothing fetched)"
        exit 0
    fi
    # Half an unpack, or a different source under the same name. We do not delete what we did not
    # create in this run — and a tree we cannot identify must not be silently trusted.
    echo "vendor-llama-cpp: $dest exists but its stamp does not record sha256 $want_sha${have_sha:+ (found $have_sha)}." >&2
    echo "  If you did not put it there, inspect it. If you did, remove it and re-run this script:" >&2
    echo "    rm -rf '$dest'" >&2
    exit 1
fi

[[ -d "$vendor" ]] || mkdir -- "$vendor"

# Staged inside .vendor so the final move stays on one filesystem, and so a killed run leaves a
# dot-directory rather than a directory named like the pin that is only half a source tree.
stage="$vendor/.unpack-$tag.$$"
tarball="$stage/llama.cpp-$tag.tar.gz"
trap 'rm -rf -- "$stage"' EXIT

mkdir -- "$stage"
echo "fetching $url"
curl --fail --location --proto '=https' --silent --show-error --output "$tarball" "$url"

got_sha="$(sha256sum "$tarball" | cut -d' ' -f1)"
if [[ "$got_sha" != "$want_sha" ]]; then
    echo "vendor-llama-cpp: digest mismatch, refusing to unpack." >&2
    echo "    pinned: $want_sha" >&2
    echo "    got:    $got_sha" >&2
    echo "    file:   $tarball ($(stat -c%s "$tarball") bytes)" >&2
    exit 1
fi

# GitHub's archive wraps everything in `llama.cpp-<tag without v>/`; strip it so the destination is a
# source root (include/llama.h directly under it), which is what build.rs verifies. Unpacking lands in
# the stage, not in `$dest`, so an interrupted run can never leave a directory that looks like the pin
# and is half a tree — `$dest` appears only when the whole thing worked.
tree="$stage/src"
mkdir -- "$tree"
tar --extract --gzip --file "$tarball" --strip-components=1 --directory "$tree"

printf 'tag=%s\nurl=%s\nsha256=%s\n' "$tag" "$url" "$want_sha" >"$tree/.vendor-stamp"
mv -- "$tree" "$dest"
trap - EXIT
rm -rf -- "$stage"

echo "vendored $tag (sha256 $want_sha) at:"
echo "$dest"
