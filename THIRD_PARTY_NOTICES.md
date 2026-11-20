# Third-party code in `np`

**What this file covers.** The licenses of code that is *compiled into a binary you download or build*. `np`'s own terms are MIT and live in
[`LICENSE`](LICENSE); this file is about everybody else's code inside the artefact. (Until 2026-11-08 there was no `LICENSE` at the root at all,
which is **F47** — three releases went out under no stated terms before anyone noticed while writing this page.)

Why this file exists now and not earlier: through `v0.2.3`, published binaries contained no C/C++ at all — `build.rs` refused every non-Linux
target and the release assets were built without `local-model`. From `v0.3.0` the shipped Windows and Linux binaries embed llama.cpp's inference
runtime, which is MIT-licensed code linked statically. MIT's obligation is small but it is not zero: the copyright notice and the license text
must accompany the distribution. So they do — here, and inside every packaged asset.

## llama.cpp and ggml — MIT

Linked statically whenever the binary is built with `local-model` (which every published asset since `v0.3.0` is).

| | |
| --- | --- |
| Upstream | [`ggml-org/llama.cpp`](https://github.com/ggml-org/llama.cpp), tag `v0.8.0` |
| Source tarball SHA-256 | `51b1e025d7b12e83fa55736e65bc254acd9d23e49f546be7531368fef82210ee` |
| Pin of record | [`third_party/llama-cpp.toml`](third_party/llama-cpp.toml), which also records the cmake configuration the build must use |
| Archives linked, in link order | `libllama.a`, `libggml.a`, `libggml-cpu.a`, `libggml-base.a` — plus `libmtmd.a` and the `vendor-hash` archive when built with `vision` |
| Copyright line as it appears upstream | `Copyright (c) 2023-2026 The ggml authors` |

Verbatim license text, as shipped in the pinned tree:

```text
MIT License

Copyright (c) 2023-2026 The ggml authors

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

**What is *not* linked, and therefore not this distribution's obligation.** The pinned tree carries other licenses for code `np` never compiles:
`common/` (whose JSON handling bundles nlohmann/json, `licenses/LICENSE-jsonhpp`, `Copyright (c) 2013-2025 Niels Lohmann`), the HTTP server's
`vendor/cpp-httplib`, `gguf-py/`, and the examples. `crates/np-core/build.rs` builds the `llama` target — and additionally `mtmd` and
`vendor-hash` under `vision` — and nothing else, so those headers and libraries are absent from the binary and their notices do not travel with
it. This paragraph is here because a notice file that names everything in a vendored tree is worse than one that names what shipped: it teaches a
reader to assume linkage where there is none. If a future build links `common/` (or anything else), this file gains an entry in the same change.

## Rust crates

`np` is a Rust workspace and its binary also contains code from crates.io. The list of those names and licenses is derived,
not hand-written, and it is now generated rather than promised: [`docs/dependency-licenses.md`](docs/dependency-licenses.md)
is written by `.github/scripts/check-deps.py --write-report`, and the same script fails the build when that file no longer
matches what it would write. A dependency bump that changes a license therefore shows up as a diff in a tracked file instead
of as a rumour in a binary nobody inspected.

What the fence enforces, and what it leaves to CI:

* **Every dependency must offer a permissive option** (`MIT`, `Apache-2.0`, `ISC`, `BSD-2/3`, `Zlib`, `Unlicense`,
  `Unicode-3.0`, `CDLA-Permissive-2.0`, `CC0-1.0`, `MulanPSL-2.0`, and `WITH` variants of those). A crate whose only option is
  copyleft is a distribution decision for a human, not something a build script may quietly accept — which is what the earlier
  version of this page asked tooling to catch. One crate today offers a choice that includes a copyleft alternative
  (`r-efi`: `MIT OR Apache-2.0 OR LGPL-2.1-or-later`); the report names it and the option taken, because "the distributor
  picks MIT" is a decision worth recording rather than one to bury.
* **One major version per crate**, unless `check-deps.py` records who pulls both and what would collapse them. Five coexist
  today, each with a reason written down; an unlisted duplicate fails locally and in CI.
* **Advisories are graded where the database is reachable** — `.github/workflows/supply-chain.yml` runs `cargo deny` and
  `cargo audit` on every pull request and nightly. Offline, neither can answer anything, so no claim about "no known
  advisories" is made in this file or anywhere else in the tree.

## Model weights are not distributed here

The embedding model (`embeddinggemma-2`, ~310 MB) is **not** part of np's source or binary distribution. It is fetched on a human's explicit
action — `np setup`, `np embed`, or a first hybrid search that finds nothing installed (**F38**, **F45**) — from the origin compiled into the pin
table, and verified against a pinned SHA-256 before it is used. Its own terms (a Google model license) govern its use and redistribution; nothing
here grants them. A build with `fetch` compiled out cannot download anything, and `NP_NO_AUTO_FETCH=1` disables automatic fetching in a build that
can: the relevant binary facts are described in [`README.md`](README.md) rather than trusted from this file.

## Reproducing the C/C++ build from source

```bash
scripts/vendor-llama-cpp.sh          # downloads the tarball named above, refuses on digest mismatch
cargo build --release --features local-model
```

The Windows binaries are cross-built the same way from Linux; [`docs/ops-cross-build-windows-gnu.md`](docs/ops-cross-build-windows-gnu.md) records
the toolchain requirements (win32-threaded mingw drivers, and why a posix-threaded driver produces an executable that imports a runtime DLL).

## Packaging obligation, restated so it cannot be lost

MIT's condition is satisfied by shipping the notice with *the distribution*, not by mentioning it in a repository. Therefore
`.github/scripts/package-assets.sh` and `.github/scripts/cross-build-windows.sh` copy this file into every tarball and zip alongside the binary.
A release asset that contains `np.exe` without this file is a defective asset even if its digest matches.
