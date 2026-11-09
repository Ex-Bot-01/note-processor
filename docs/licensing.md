# Licensing, in full

The README's version is two lines: MIT, notices shipped inside every archive. This is the long form — why the copyright line reads the way it does, what the obligation attaches to, and what is deliberately not covered.

> *Moved out of `README.md` on 2026-12-09, when the README became a document you read before installing rather than one you read after something broke. The wording is the same as it was there — including the dates, the measured numbers and the claims about what has not been measured — because those are the parts that carry weight.*

**`np` is MIT**, in [`LICENSE`](../LICENSE) — ruled on 2026-12-08, which closed **F47**. The copyright line reads
`Copyright (c) 2026 The note-processor authors` because no personal or organisational name was given to put there; it is a placeholder in the
sense that it is yours to replace with whatever you want credited, not in the sense that the grant is provisional. MIT was chosen to match what
this binary already statically links (llama.cpp and ggml are MIT), so nothing we ask of a downstream user is stricter than an obligation we are
already satisfying ourselves: keep the notices attached. Both crates declare it (`license = "MIT"` in `[workspace.package]`, inherited by
`np-core` and `np-cli`), and both release assets carry `LICENSE` next to the binary.

What *is* settled is the obligation that arrived with `v0.3.0`. Published binaries now statically link llama.cpp's and ggml's inference runtime
(MIT, `Copyright (c) 2023-2026 The ggml authors`), and MIT's condition — ship the copyright notice and license text with the distribution —
attaches to the artefact a person downloads, not to the repository it came from. So [`THIRD_PARTY_NOTICES.md`](../THIRD_PARTY_NOTICES.md) carries
the verbatim text plus exactly which archives are linked and which parts of the pinned tree are **not** (and therefore do not need naming), and
both packaging scripts put that file *inside* every tarball and zip and refuse to publish an asset that lacks it.

The model weights are a separate matter and are not redistributed here: `embeddinggemma-2` is fetched on your explicit action, digest-verified,
and its own model license governs it. See [Third-party code](../THIRD_PARTY_NOTICES.md) for the full statement, including the reproducible build
commands and the open item on generated Rust crate notices.
