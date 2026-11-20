# Release archive caveats, per version

Which assets exist, what is compiled into each one, and which claims about them have never been measured. Written because a downloaded binary
    cannot tell you its own build flags, and because "it is the same version" is not the same as "it can do the same things".

> *Moved out of `README.md` on 2026-11-09, when the README became a document you read before installing rather than one you read after something broke. The wording is the same as it was there — including the dates, the measured numbers and the claims about what has not been measured — because those are the parts that carry weight.*

### Pre-built binaries

GitHub Releases carry `np` archives named `np-<version>-<target>`. **`v0.1.0`'s assets are stale** — that tag points at the
2026-10-28 tree, whose CLI had exactly one subcommand (`scan`) — so use them for the indexer only, never for `embed`, `search` or
`models`.

The body of a release page comes from `release-notes/<tag>.md` in this repository when one exists — so the prose a downloader reads can be
reviewed, diffed and cited by the next release note instead of living in a browser tab. A tag cut without one gets GitHub's generated
commit list, which is honest but says nothing about CPUs.

From **`v0.2.0` onward, read the release notes before downloading**, because which machine built an asset is part of what that asset
is. Since `v0.4.0` `.github/workflows/release.yml` builds both shipped assets by running the two scripts a human runs —
`.github/scripts/package-assets.sh` for Linux and `.github/scripts/cross-build-windows.sh` for the Windows cross-build — so there is
one build definition and CI grades it. Before that the workflow carried its own `cargo build` over a Windows/Linux/macOS matrix while
the published files came from those scripts, and the two disagreed about features: an asset CI produced could not embed although the
page said it could (**F40**). Hosted runners are also metered, so when an account is out of included minutes no release can be built
there at all (`docs/developing.md`); a locally packaged release carries **the one platform the packager was standing on**,
and its notes say so. Source builds need no CI and no minutes:

```sh
cargo install --git https://github.com/Ex-Bot-01/note-processor --tag v0.6.0 -p np-cli --locked
```

A source build is the only shape you can choose features on: `--features local-model` puts the inference runtime in the binary, and adding
`,vision` adds figure encoding. Neither is needed to index or to search by keyword.

**Current release: `v0.6.0` (2026-11-19).** Five findings from one reporter's pass over the shipped binary, each reproduced before it was
touched. The protocol stays **1** and no method is added — but keys appear, so this is a minor release rather than a patch: a client that
asserts an exact key list must add two names, and `INDEX.md`'s bytes change for one class of file name.

- **A meanings-only search says which vectors are missing** ([#11](https://github.com/Ex-Bot-01/note-processor/issues/11)). `mode: semantic`
  with the artifact installed and nothing embedded answered `hits: []`, `note: null`, `reason: null` — indistinguishable from "I searched
  your notes and found nothing". New token `semantic-without-vectors`, on every door that can produce the state.
- **A tracked attachment is counted and named** ([#12](https://github.com/Ex-Bot-01/note-processor/issues/12)). `scan` results carry
  `attachments` (exact) and `attachment_examples` (up to five paths), and the terminal summary gains a line when the count is non-zero. Five
  files scanned, one searchable note, nothing in the report said so.
- **An over-long body is refused at the pin** ([#13](https://github.com/Ex-Bot-01/note-processor/issues/13)). A mirror sending 500 MB under a
  309 MB pin used to write all of it before refusing; one read buffer (1 MiB) is now the most a member can overshoot by, and the refusal says
  "sent at least N" because a stream abandoned at 310 MB did not *end* there.
- **Angle-bracket destinations resolve; code spans hold their links** ([#14](https://github.com/Ex-Bot-01/note-processor/issues/14)).
  `[x](<a note.md>)` was filed as an attachment called `<a note.md>` and reported nowhere — the one silent link failure in the product. Links
  inside backticks are no longer edges or unresolved targets.
- **A catalog entry stays on its line** ([#15](https://github.com/Ex-Bot-01/note-processor/issues/15)). A file name containing a newline took
  three physical lines in `INDEX.md`; brackets and parens in a name or title broke the link around it. Labels escape `[`, `]`, `\` and put
  U+FFFD where a control character was; destinations are percent-escaped, `#` and `?` included, because both mean "inside the target".

The two `v0.6.0` archives, as published (2026-11-19), measured from a fresh download of the page and confirmed by `sha256sum -c SHA256SUMS.txt`
(which lists exactly these two names and nothing inside them):

```text
7e7b8acf292625ea2274bf6f2dfc43a263fa3b1d6fcba25f791bf2d6fa937e8a  np-0.6.0-x86_64-unknown-linux-gnu.tar.gz
c776fc8f8f1bb71a0ca07d8c2cb9521fc6d0dc8ac3642f642a288b630b8c9d4b  np-0.6.0-x86_64-pc-windows-gnu-cross.zip
```

Inside the Windows archive, `np.exe` is 16,740,864 bytes and digests to
`0408a191ae4f2c4df011adf30a1eb69ca4b7af51ab2e4c9771abb64b43f531ab` — prose, not `SHA256SUMS.txt`, because that file is run through
`sha256sum -c` against files a person downloaded and must never name a file inside an archive (**v0.4.0's** mistake).

Both archives came from one run of the tag ([`44582261113`](https://github.com/Ex-Bot-01/note-processor/actions/runs/44582261113)), and **no
rehearsal was dispatched this time**, so this release adds no evidence to the Linux cross-run reproducibility claim or to the Windows
non-claim: those stand on the three builds measured through `v0.5.1` above, and a digest pinned here is one file at one moment either way.
The reason not to rehearse is recorded in the tag's chore commit — `release.yml` and both packaging scripts are byte-identical to the pair
that produced `v0.5.1`'s published assets, and a rehearsal costs the same hosted minutes as the run that publishes.

**Four of the five fixes were then measured in the published Linux asset rather than in a test binary.** Downloaded, `sha256sum -c` clean,
run against fresh `XDG_*` directories: `np --version` → `np 0.6.0`; an `initialize` handshake answering
`"features":{"fetch":true,"local_model":true,"vision":false}` — the check **F40** exists to make, that the shipped file really carries the
runtime its own release page promises; a four-file `scan` reporting `"attachments":2,"attachment_examples":["plan.org","scratch.txt"]`
(**#12**) in the same session as a `"mode":"semantic"` search answering `"reason":"semantic-without-vectors"` (**#11**); a three-note vault
whose `INDEX.md` entries came out escaped, one per note (**#15**); and `[angle](<a note.md>)` producing `→1` on the source note and `←1` on
the file with a space in its name, with `"unresolved_links":0` (**#14**). What was *not* measured: this binary never ran on a machine whose
CPU stops at SSE4.2, and nothing from the Windows archive executed anywhere — no such host here, so `np.exe` is accounted for as bytes only.

`.text`, counted over both published Linux assets with one command (`objdump -d --section=.text | grep -oE '%(zmm|ymm)[0-9]+'`): **zero
`%zmm` and 11,154 `%ymm` on `v0.5.1` and on `v0.6.0` alike** — same instruction-set floor, unchanged by this round. That count is not
comparable with the "4,940 `%ymm`" printed in the `v0.5.1` entry above, whose command was not recorded; what the pair establishes is that one
command gives one number for both releases.

**What a client must do.** Add `attachments` and `attachment_examples` to any exact-key assertion over a `scan` result — absence now means
"built before 0.6.0", which is not zero. Handle `reason: "semantic-without-vectors"`. And expect `INDEX.md` bytes to change once, on the first
scan after upgrading, for vaults holding a name with a newline, bracket, paren, `#`, `?` or `<`.

**What is not in it.** The other half of #12 — making `.txt` and friends *searchable* rather than counted — needs a decision about where a
text file's title comes from, so it stays an open design card. See [`todo.md`](../todo.md).

**`v0.5.1` (2026-11-15).** A patch, and the kind of patch that is not cosmetic: it fixes two things a client could not work around, and adds
nothing a parser can see. The protocol stayed **1**, no new method and no new key appeared, and `np embeddings enable` at a terminal writes
byte-identical bytes to what 0.5.0 wrote.

- **A write made over `np serve` now records the method that made it** ([#10](https://github.com/Ex-Bot-01/note-processor/issues/10)). On 0.5.0
  both doors — the terminal and the pipe — wrote ``# note-processor set by `np embeddings enable <id>` …``, so a config file edited over stdin
  credited a command line nobody had typed. That comment is the only durable record of who set what, and it is what an application's audit trail
  renders, so the false half was not cosmetic. The pipe now writes ``the `embeddings.enable` method over `np serve` ``. **Breaking for Rust
  consumers:** `config_write::set_embeddings` / `set_embeddings_in` gained a surface parameter and there is deliberately no default — a default
  would be "the terminal", which is the bug this removes. Code compiled against 0.5.0 fails at the call site rather than writing a false record.
- **Windows nightly went from red to green** ([#9](https://github.com/Ex-Bot-01/note-processor/issues/9)). Two `walk.rs` cases asserted that a name
  refused for holding an unpaired surrogate is recorded with lossy identity bytes; F18 round four had made those bytes exact (WTF-8) on 2026-11-08,
  and three doc comments had said so too. Nothing could notice, because the assertions were `#[cfg(windows)]` and the prose was not executable. The
  behaviour is now asserted as it actually is — two refused names produce two keys with one display spelling each — and the refusal itself stays in
  force for F18's stated reason.

The two `v0.5.1` archives, as published (2026-11-15), measured from a fresh download of the page and confirmed by `sha256sum -c SHA256SUMS.txt`
(which lists exactly these two names):

```text
9b08f426bc2e679908ad43a8323b0df8e77ea0dd66596cb820cb925d9813a1e6  np-0.5.1-x86_64-unknown-linux-gnu.tar.gz
c7fa67cd7fc7031eeaa54dd60cc4a4b562e3689d01dc738483ddbfe2d1f92d61  np-0.5.1-x86_64-pc-windows-gnu-cross.zip
```

Inside the Windows archive, `np.exe` is 16,732,160 bytes and digests to
`1a2a1111e4425d3dc5eea6131b0a4fbc5fa1c3668732dfc52614fba75880dbc8` — prose, not `SHA256SUMS.txt`, because that file is run through `sha256sum -c`
against files a person downloaded and must never name a file inside an archive (**v0.4.0's** mistake).

**The Linux reproducibility claim now has three runs behind it, and the Windows non-claim has two.** The rehearsal build of this tag
([`43737892031`](https://github.com/Ex-Bot-01/note-processor/actions/runs/43737892031), `publish=false`) and the tag build
([`43738485291`](https://github.com/Ex-Bot-01/note-processor/actions/runs/43738485291)) produced the identical Linux archive — `9b08f426…` both times,
the digest above — and different Windows archives: the rehearsal's was `47416df0…`, the page carries `c7fa67cd…`. Anyone pinning a Windows digest
pins one file at one moment; compare against this page, never against a rehearsal log line (which is exactly how `v0.5.0`'s entry below got a wrong
number into print).

The published Linux binary was run before and after publication on a machine whose CPU stops at SSE4.2: `np --version` → `np 0.5.1`, `np scan`
indexed a vault, a prefix search widened `grand` → `grandmother`, and one `embeddings.enable` request over stdin wrote
``# note-processor set by the `embeddings.enable` method over `np serve` (np 0.5.1) at …`` — **#10** verified in the shipped asset rather than in a
test binary. Its `.text` holds zero `%zmm` and 4,940 `%ymm` references, the same figures `v0.4.0` and `v0.5.0` carry: three releases, one floor.

**Since `v0.4.0`, shipped in `v0.5.0`.** `initialize` answers **ten** methods; the protocol stayed **1** and nothing existing changed shape.

- **A word can match at its beginning** (**#6**): `"grand*"` in a query, `--prefix` to widen every word, or `"prefix": true` over the
  pipe. The answer admits what it did — `prefix_used`, `prefix_terms`, and per-hit `matched_exactly` (`true` whole words only, `false`
  reached by widening, `null` for a hit that came from vectors and was never in a whole-word test). A query with no `*` still matches
  whole words exactly as `0.4.0` did: widening is opt-in because a vault holds identifiers.
- **Consent has a door** (**#7**): `np embeddings enable <id>` / `disable`, and `embeddings.enable` / `embeddings.disable` over the pipe.
  np writes its own config, names the keys it changed, quotes what a first `np embed` would cost and **moves no bytes**; the prospective
  file is loaded back through np's own reader before it replaces the old one, so a file that would not load is never published. Comments,
  unrelated keys and line endings survive; enabling what is already enabled writes nothing; `disable` leaves `embeddings.model` in place,
  because revoking permission to download is not a licence to erase which model was chosen. These two are the only methods that refuse a
  parameter they cannot use — a client that sends `"modle"` is told so rather than told `ok`.
- **A checksum file may only name files the page carries.** `v0.4.0`'s first `SHA256SUMS.txt` listed `np.exe`, which lives *inside* the
  zip, so the documented `sha256sum -c` reported `FAILED open or read` for a file nobody could download. The published page was corrected
  by hand and says so; the packaging script is fixed here, and the publish job now checks coverage both ways.
- **Windows, where five red tests were one bug.** F37 nested np's Windows data dir under `%LOCALAPPDATA%\note-processor`; the test
  harnesses derived their assertion paths from the raw variable — one floor below where np writes — and six dispatched runs removed that
  assumption through five call sites. Every other test file then ran on a Windows runner for the first time. One real finding survived:
  [#9](https://github.com/Ex-Bot-01/note-processor/issues/9), that a name refused for holding an unpaired surrogate carries WTF-8 identity
  bytes there rather than U+FFFD, which contradicts what F20's card asserted. **Nightly is red on those two cases**; nothing in the shipped
  binaries depends on them, and they are about how a refusal *report* is spelled.

The two `v0.5.0` archives, as published (2026-11-14), so a reader can check a download against something that is not the same
response they are verifying (`v0.5.1`'s digests are named in its own [release page](https://github.com/Ex-Bot-01/note-processor/releases/tag/v0.5.1)
and its `SHA256SUMS.txt`, which is the file to check against):

```text
5c71f53d73e5a08fa5e23f437c1aa09e11dadc1cdfc0d3f81c7a32d8987be3ec  np-0.5.0-x86_64-unknown-linux-gnu.tar.gz
4cfb21833582ceed761e501dcf7dec0502f2fe5bf3803aa194e34df16c820db2  np-0.5.0-x86_64-pc-windows-gnu-cross.zip
```

Both are the digests of **the files on the page**, taken from a fresh download and confirmed by `sha256sum -c SHA256SUMS.txt` — which lists
exactly these two names and nothing else. The Windows archive's `np.exe` (16,731,648 bytes) digests to
`cb583af1fefd0a1dbe0c33b8cc8d528c4faa38ba587368d4ff3e8ced4669246f`, and it is named here rather than in `SHA256SUMS.txt`: that file gets run
through `sha256sum -c` against what a person downloaded, so naming a member of an archive is **v0.4.0's** mistake (`np.exe: FAILED open or read`).

**One half of this release reproduced byte for byte, and one did not — measured rather than asserted.** The rehearsal build
([run 43642586210](https://github.com/Ex-Bot-01/note-processor/actions/runs/43642586210), `publish=false`) and the tag build
([run 43643367475](https://github.com/Ex-Bot-01/note-processor/actions/runs/43643367475)) were independent CI runs, and their Linux archives
digest identically: `5c71f53d…`. The Windows cross-build does not — rehearsal produced `c2a2da1c…`, the page carries `4cfb2183…`, and its
`np.exe` differs likewise (`ba310fb9…` against `cb583af1…`). So README's table is right in a way it had not been checked to be: Linux is
byte-reproducible, and the cross-build's `ABOUT-windows-gnu-cross.md` is right that its published sha256 identifies *the file you
downloaded* rather than a recipe. Anyone comparing a v0.5.0 Windows download should compare against the page, not against a log line from a
rehearsal run — which is where the numbers in this paragraph's first draft came from, and why they were corrected here instead of left alone.

The Linux binary was executed on a machine whose CPU stops at SSE4.2 before publishing: `np --version`, `np scan`, and a prefix search all
ran, and its `.text` holds zero `%zmm` and 4,940 `%ymm` references — the same figures `v0.4.0`'s shipped binary carries.

**Since `v0.3.1`, shipped in `v0.4.0`.** `initialize` named **eight** methods there.

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

Both assets for every release since `v0.4.0` are built by CI on `ubuntu-latest`, by the two scripts named above, and each carries `np`
(or `np.exe`), `LICENSE` and `THIRD_PARTY_NOTICES.md`.

**They run on any x86-64 machine, and that costs them speed.** Three things decide the instruction set of a shipped binary, and for
three rehearsals only the first was being said out loud:

- **llama.cpp is configured at the floor**: `GGML_NATIVE=OFF` *and* every `GGML_<ISA>` option off. A static llama.cpp has no runtime
  dispatch — its CPUID-selected variants live in dynamic backend modules (`GGML_CPU_ALL_VARIANTS`, which upstream refuses to build
  without `GGML_BACKEND_DL`) and this project ships one file. `GGML_NATIVE=OFF` on its own *raises* the floor rather than lowering it:
  on a native build that flag flips upstream's `INS_ENB` on, and the per-ISA options arrive as `-msse4.2 … -mavx2` across the whole cpu
  backend.
- **The C/C++ compiler is told the floor**: `-march=x86-64`, appended after anything `CFLAGS`/`CXXFLAGS` already carried.
- **Rust is told the floor**: `-C target-cpu=x86-64` in `RUSTFLAGS`.

The last two are the ones that mattered, and they were not what the first two explanations said. Measured on the runner with every
llama.cpp option OFF: its `cc`, handed an empty command line, predefines `__AVX__` and `__AVX2__` — **that image's compiler defaults to
a target above the baseline** — so 76 translation units containing no `-m` token between them still produced 36,203 wide-vector
instructions (`ggml-quants.c` alone: 6,972; `arch/x86/repack.cpp`: 6,534; every one of those files sits at zero in a local build of the
same pin). Options can only withdraw features something requested; nothing had requested AVX, so nothing was withdrawn. `build.rs` now
asks the compiler directly — runs it over empty input and refuses a portable build whose predefined macros name anything above SSE2 —
after the first version of that check read the wrong token out of `#define __AVX2__ 1` for three rehearsals and found nothing forbidden
in any compiler on earth.

The symptom that started it, for the record: the first CI-built Linux asset was compiled `-march=native` for its runner, carried 14,540
AVX-512 instructions, and answered `Illegal instruction (core dumped)` on an older x86-64 host before printing a version number. **A
release asset is run by machines that did not build it.**

**Measured on 2026-11-13, on that host, against the archive CI built in run `43522182273`**: `np --version` → `np 0.4.0`; `np scan`
indexed a vault; `np serve` answered `initialize`, `links.list` and `models.list`; `np embed` fetched the pinned 295.5 MB artifact,
re-checked its digest and wrote vectors with `errors 0`; hybrid search then scored a keyword-free query at `cos 0.8362`. Shipped binary:
`%zmm 0`, `%ymm 4,940` — the same count as a local build of the same tree. **The Windows asset has not been run by anything**: it is
cross-built and inspected, and no claim beyond that appears in its notes.

**A source build stays fast on the machine that built it**: `NP_LLAMA_PORTABLE` is unset there, so upstream's `-march=native` applies,
nothing touches `RUSTFLAGS`, and `np embed` gets the AVX2/AVX-512 paths your CPU has. That is the difference between downloading and
building, and it is now the only difference. **An asset built before 2026-11-13 may carry its build machine's instruction set** — if an
older release's binary refuses to start on yours, that is the reason. **No Windows-MSVC asset and no macOS asset exist**, and this is not a minutes decision:
`crates/np-core/build.rs` compiles llama.cpp for Linux and for `x86_64-pc-windows-gnu` with a cmake toolchain file, and refuses every
other target rather than meeting a linker error — "no platform claim without a build measured on it". A runtime-less macOS or MSVC binary
would arrive under a name that matches the runtime-bearing ones, which is how `F40`'s trap worked. On those platforms build from source,
with or without `--features local-model`.

**One caveat specific to `v0.4.0`'s page**: the `SHA256SUMS.txt` CI first uploaded listed `np.exe`, a file that lives inside the zip, so
`sha256sum -c SHA256SUMS.txt` printed `np.exe: FAILED open or read` next to two correct digests. Corrected the same day — the file now
covers exactly what the page hands you, and the exe's digest moved to `ABOUT-windows-gnu-cross.md`. Both archives are untouched, and
`.github/scripts/cross-build-windows.sh` no longer writes that line; the publish job now checks checksum coverage in both directions.

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
