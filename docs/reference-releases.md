# Release archive caveats, per version

Which assets exist, what is compiled into each one, and which claims about them have never been measured. Written because a downloaded binary
    cannot tell you its own build flags, and because "it is the same version" is not the same as "it can do the same things".

> *Moved out of `README.md` on 2026-12-09, when the README became a document you read before installing rather than one you read after something broke. The wording is the same as it was there — including the dates, the measured numbers and the claims about what has not been measured — because those are the parts that carry weight.*

### Pre-built binaries

GitHub Releases carry `np` archives named `np-<version>-<target>`. **`v0.1.0`'s assets are stale** — that tag points at the
2026-10-28 tree, whose CLI had exactly one subcommand (`scan`) — so use them for the indexer only, never for `embed`, `search` or
`models`.

From **`v0.2.0` onward, read the release notes before downloading**, because which machine built an asset is part of what that asset
is. Since `v0.4.0` `.github/workflows/release.yml` builds both shipped assets by running the two scripts a human runs —
`.github/scripts/package-assets.sh` for Linux and `.github/scripts/cross-build-windows.sh` for the Windows cross-build — so there is
one build definition and CI grades it. Before that the workflow carried its own `cargo build` over a Windows/Linux/macOS matrix while
the published files came from those scripts, and the two disagreed about features: an asset CI produced could not embed although the
page said it could (**F40**). Hosted runners are also metered, so when an account is out of included minutes no release can be built
there at all (`docs/ops-actions-minutes.md`); a locally packaged release carries **the one platform the packager was standing on**,
and its notes say so. Source builds need no CI and no minutes:

```sh
cargo install --git https://github.com/Ex-Bot-01/note-processor --tag v0.4.0 -p np-cli --locked
```

A source build is the only shape you can choose features on: `--features local-model` puts the inference runtime in the binary, and adding
`,vision` adds figure encoding. Neither is needed to index or to search by keyword.

**Current release: `v0.4.0` (2026-11-13).** Everything below is user-visible since `v0.3.1`; the protocol stays **1**, and `initialize`
now names **eight** methods.

- **A diagnostics surface with a reference.** Levels resolve from file, environment and flag; every refusal names its rule on the
  diagnostic channel; `scan` reports why each note was skipped; records raised inside `np serve` carry the `request` id that caused them
  and each phase closes with `elapsed_ms`. stderr is capped and bounded — a blocked pipe, a long path or a second writer costs a
  *diagnostic*, never a request. See [`reference-diagnostics.md`](reference-diagnostics.md).
- **`links.list`** reads the link graph over the pipe (**#2**): who links here, and every dead target with the note and line that wrote
  it. One `rel` parameter names a subject at either end of an edge; `indexed` separates "never scanned" from "nothing links here".
  Figure embeds are in neither list, and that gap is written down rather than left as a surprise.
- **The price of a fetch, before anyone agrees to one** (**#3**): `np models list --json` and `models.list` carry `pinned` —
  `{id, bytes_total, members}` for *this build* — and the terminal consent prompt quotes that same number instead of adding up the whole
  pin table.
- **An answer says what ran** (**#5**): search results carry `indexed`, `keyword_used` and `semantic_used`, and a workspace that has never
  been scanned is named as such rather than reported as "no matches".
- **One directory is one workspace** (**#4**): a trailing separator, a doubled separator, a `.` component and Windows's separator style no
  longer key a second index, a second model cache and a second registry row. **The upgrade cost**: a workspace first indexed under a
  spelling this rewrites now files its index under the canonical key, so it needs one `np scan` (and `np embed` to rebuild vectors).
- **`status.get` answers "is this index stale?"** with a real embedding denominator rather than a note count.
- Keyword search wants **every** word of the query — documented where a reader meets it (**#1**), because AND-of-words is what makes a
  narrow query narrow.

Both assets for `v0.4.0` are built by CI on `ubuntu-latest`, by the two scripts named above, and each carries `np` (or `np.exe`),
`LICENSE` and `THIRD_PARTY_NOTICES.md`. **No Windows-MSVC asset and no macOS asset exist**, and this is not a minutes decision:
`crates/np-core/build.rs` compiles llama.cpp for Linux and for `x86_64-pc-windows-gnu` with a cmake toolchain file, and refuses every
other target rather than meeting a linker error — "no platform claim without a build measured on it". A runtime-less macOS or MSVC binary
would arrive under a name that matches the runtime-bearing ones, which is how `F40`'s trap worked. On those platforms build from source,
with or without `--features local-model`.

**Previous release: `v0.3.1`** — `np serve` grew opt-in progress notifications (`locking`, `resolving`, `indexing`, `fetching` with byte
counters, `embedding`, `writing`) and the `status.get` method; four defects an independent review found are listed in that release's notes.
Its assets were packaged by hand on Linux, the Windows one cross-built — the last release whose files CI did not build.

**`v0.3.0`.** Two things arrived in it that change what you can build on top of `np`:

- **`np serve`** — JSON-RPC 2.0 over stdin/stdout inside the same binary, so an application can use `np` as its backend without a port, a token or a
  daemon. Six methods; every response is the document the equivalent `--json` command prints; states (`locked`, `offline`, `reason`, `prefix_drift`)
  come back as data rather than as prose. See [Driving `np` from a program](../README.md#use-np-from-your-app).
- **A license: MIT.** `LICENSE` is in the repository root and inside every release archive.

Both shipped assets carry the inference runtime (`local-model` plus `fetch`), so a downloaded binary can embed and auto-fetch, not just index — the
model itself is never inside the archive; ~309 MB arrives only when you run `np setup` or `np embed`, and `NP_NO_AUTO_FETCH=1` switches that off. The
Linux asset (`x86_64-unknown-linux-gnu`) was built here by `.github/scripts/package-assets.sh`. The Windows asset
(`x86_64-pc-windows-gnu-cross.zip`) is cross-compiled on Linux with mingw-w64 — **unsigned, and not an MSVC build** — with its import table checked at
packaging time so it stays a single file rather than a folder of DLLs. **No macOS asset exists**: that needs an Apple SDK, which no cross-compiler here
has; on macOS build from source. Every archive contains `np`, `LICENSE` and `THIRD_PARTY_NOTICES.md`, and each release publishes `SHA256SUMS.txt`.

**What was not measured about `v0.3.0`**, because you should not have to find out by hand: GitHub Actions was out of included minutes since
2026-11-07, so no CI job has graded this tree — the local gate list (20 rows: fmt, four clippy feature rows, workspace and per-feature test runs, a
no-`HOME` config run, three rustdoc `--deny warnings` runs, two static checks, catalog goldens, and the quality-harness self-test) is what stands in
its place, and the release notes for each tag say which rows ran. The Windows write-lock behaviour (`np scan` and `np embed` refusing cleanly when a
second writer holds a vault) is believed equivalent to Linux — SQLite uses `LockFileEx` there instead of POSIX record locks — and has never been
measured on Windows. MSVC parity is unmeasured too: `build.rs` refuses that target rather than pretending.

**Previous release: `v0.2.3`** — one fix on top of v0.2.2, and it is a Windows one. An upgraded machine whose index still sat at the
pre-F37 location was correctly *read* from there (nothing re-indexed, no duplicate), but printed none of the `reading your index from …` notice
that release promised: `Config::load` asked a merged struct whether the operator had declared a directory, and a resolved default always looks
like an instruction. Found by a field run on real Windows; graded on Linux through the same injectable-platform seam F37 already used.

**`v0.2.2` is where `np setup` and auto-fetch (F38) arrive**, along with the Windows data folder moving under
`%LOCALAPPDATA%\note-processor` (F37). Its assets are built locally, not by CI: Linux (`x86_64-unknown-linux-gnu`, by
`package-assets.sh`) and Windows (`x86_64-pc-windows-gnu`, cross-compiled with mingw-w64 — recipe in
[`ops-cross-build-windows-gnu.md`](ops-cross-build-windows-gnu.md)). Both are unsigned, and the Windows one is compiled **without**
the inference feature — which also means it cannot auto-fetch: F38's trigger is compiled out of a binary that could not read the bytes, so
`np embed` and `np setup` refuse there instead of spending 310 MB. `scan`, `search` in all three modes, `models list/install/fetch` and
everything about paths and identity do work. If you can build from source with `--features local-model`, do that instead — it is the only shape
with a runtime in it. No macOS asset exists yet: that needs an Apple SDK, which no cross-compiler here has.
