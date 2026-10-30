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
# `.vendor/llama.cpp-<tag>/` (gitignored), and print that path.
#
# Idempotence is proven by content, not by a note. An existing tree is accepted only if its
# `include/llama.h` hashes to the pin's `header_sha256`; the tarball digest we fetched from is recorded in
# `.vendor/.stamps/<tag>.sha256`, **outside** the tree, as provenance. It used to live inside the tree as
# `.vendor-stamp`, which meant `mkdir <dest> && printf 'sha256=…' > <dest>/.vendor-stamp` made this script
# print "already vendored" and exit 0 over an empty directory — measured, R6b0 review P2-2. A tree cannot be
# allowed to vouch for itself.
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
want_header="$(pin_value header_sha256)"

for pair in "tag=$tag" "url=$url" "sha256=$want_sha" "header_sha256=$want_header"; do
    [[ -n "${pair#*=}" ]] || { echo "vendor-llama-cpp: $pin has no ${pair%%=*}" >&2; exit 1; }
done

dest="$vendor/llama.cpp-$tag"
stamp_dir="$vendor/.stamps"

if [[ -d "$dest" ]]; then
    header="$dest/include/llama.h"
    # The tree proves what it is. `include/llama.h` is the file `crates/np-core/src/llama_ffi.rs` mirrors, so
    # its digest is the claim the whole build rests on — and re-hashing 70 KB costs less than a download.
    have_header=""
    if [[ -f "$header" ]]; then
        have_header="$(sha256sum "$header" | cut -d' ' -f1)"
        if [[ "$have_header" == "$want_header" ]]; then
            echo "already vendored: $dest (include/llama.h hashes to the pinned header_sha256; nothing fetched)"
            exit 0
        fi
        echo "vendor-llama-cpp: $dest/include/llama.h is not the pinned header." >&2
        echo "    pinned: $want_header" >&2
        echo "    found:  $have_header" >&2
    else
        echo "vendor-llama-cpp: $dest has no include/llama.h, so it is not a llama.cpp source tree." >&2
    fi
    # Half an unpack, or a different source under the same name. We do not delete what we did not
    # create in this run — and a tree we cannot identify must not be silently trusted.
    echo "  Nothing was fetched and nothing was changed. If you did not put this tree there, inspect it;" >&2
    echo "  if you did, remove it and re-run this script:" >&2
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
    # Keep the evidence. The trap deletes `$stage` on exit, so a message naming a file it is about to delete
    # costs another 38.7 MB download to re-diagnose (R6b0 review P2-4). `wc -c` rather than GNU `stat`, because
    # the size line has to work on macOS too.
    keep="$vendor/.downloads"
    mkdir -- "$keep"
    moved="$keep/mismatch-$tag-$(date -u +%Y%m%dT%H%M%SZ).tar.gz"
    mv -- "$tarball" "$moved"
    echo "vendor-llama-cpp: digest mismatch, refusing to unpack." >&2
    echo "    pinned: $want_sha" >&2
    echo "    got:    $got_sha" >&2
    echo "    kept:   $moved ($(wc -c < "$moved") bytes)" >&2
    exit 1
fi

# GitHub's archive wraps everything in `llama.cpp-<tag without v>/`; strip it so the destination is a
# source root (include/llama.h directly under it), which is what build.rs verifies. Unpacking lands in
# the stage, not in `$dest`, so an interrupted run can never leave a directory that looks like the pin
# and is half a tree — `$dest` appears only when the whole thing worked.
tree="$stage/src"
mkdir -- "$tree"
tar --extract --gzip --file "$tarball" --strip-components=1 --directory "$tree"

# Verify the unpacked tree against the header digest before it becomes `$dest`. The tarball digest already
# proves what this is, but checking here means a pin whose two digests disagree surfaces as a vendor-time
# refusal rather than a build that cannot start — and it is the value build.rs re-checks on every feature-on
# build, so the two readers must agree from the first run.
actual_header="$(sha256sum "$tree/include/llama.h" | cut -d' ' -f1)"
if [[ "$actual_header" != "$want_header" ]]; then
    echo "vendor-llama-cpp: the tarball matches its digest but its include/llama.h is not header_sha256." >&2
    echo "    pinned: $want_header" >&2
    echo "    found:  $actual_header" >&2
    echo "  $pin is internally inconsistent: the two digests describe different trees." >&2
    exit 1
fi

# Provenance, kept outside the tree so it cannot travel with one or be written into a directory that was never
# verified. Nothing reads this to decide whether a tree is trustworthy — the header digest does that.
mkdir -- "$stamp_dir"
printf 'tag=%s\nurl=%s\ntarball_sha256=%s\nheader_sha256=%s\nvendored_at=%s\n' \
    "$tag" "$url" "$want_sha" "$actual_header" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >"$stamp_dir/$tag.sha256"
mv -- "$tree" "$dest"
trap - EXIT
rm -rf -- "$stage"

echo "vendored $tag (tarball sha256 $want_sha, header sha256 $actual_header) at:"
echo "$dest"
