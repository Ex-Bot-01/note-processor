# Building from source, and developing

Feature builds, the pinned llama.cpp vendor step, the gate list that everything else on this repository is judged by, and what hosted CI does
    and does not grade.

> *Moved out of `README.md` on 2026-12-09, when the README became a document you read before installing rather than one you read after something broke. The wording is the same as it was there — including the dates, the measured numbers and the claims about what has not been measured — because those are the parts that carry weight.*

## Installing `np`

## From source (the supported path)

You need a stable Rust toolchain — the repository pins none, and everything here was built with 1.99.1. Nothing else is needed
for the default build: no C++ compiler and no cmake. The only thing cargo reaches for is its own registry of crates.

```sh
git clone https://github.com/Ex-Bot-01/note-processor.git
cd note-processor
cargo build --workspace --release        # binary at target/release/np
cargo install --path crates/np-cli       # or install it as `np` on your PATH
np --help
```

```text
Usage: np <COMMAND>

Commands:
  scan    Index a note workspace and regenerate its catalog
  embed   Embed the notes a workspace owes, for the configured model
  search  Rank the indexed notes and images against a text query
  models  Install and inspect the model artifacts of one workspace
```

The default build links **no** inference runtime: `np scan` is fully functional; `np embed` reports that nothing was produced and
`np search --mode semantic` refuses to turn your query into numbers — both at exit 1, naming what to change. Default (hybrid) search still
answers, from keywords, with a note saying the semantic half is missing (**F45**): it has an answer to give either way, so the runtime complaint
is left to the command that cannot do its job without it. Add
`--features local-model` (and cmake) if you want the runtime in the binary; see
[Building from source](#building-from-source-and-developing).


```sh
cargo build --workspace                 # no C++, no runtime: the default everyone gets
cargo test --workspace                  # 645 tests pass here; none needs a model
```

The `local-model` feature builds llama.cpp from sha256-pinned source. It is **Linux-only by refusal** in `build.rs`, and it
needs two explicit steps, because `cargo build` never fetches anything:

```sh
CMAKE=/path/to/cmake scripts/vendor-llama-cpp.sh      # fetch + verify the pinned tarball into .vendor/
CMAKE=/path/to/cmake cargo build --release -p np-cli --features local-model
```

The vendor script downloads the tarball named in `third_party/llama-cpp.toml`, refuses on any digest mismatch (printing both),
and records provenance outside the tree; re-running it on an existing tree prints `already vendored: …` and fetches nothing.
On this host a cold feature build of `np-cli` took 57 s with `-j 4`.

**Watch your disk.** A target directory that has run every gate here reached **10.7 GiB**; `cargo clean` reclaims it, and the
pinned source in `.vendor/` (184 MB) survives so a rebuild needs no download.

Working on the code means running the whole gate list by exit code — subsetting it is how a green run stops meaning anything:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --locked -p np-core --features local-model --all-targets -- -D warnings
cargo clippy --locked -p np-cli --features local-model --all-targets -- -D warnings
cargo clippy --locked -p np-core --features local-model,vision --all-targets -- -D warnings
cargo clippy --locked -p np-cli --features local-model,vision --all-targets -- -D warnings   # the row whose absence was F28
cargo clippy --locked -p np-cli --no-default-features --all-targets -- -D warnings
cargo test --workspace
cargo test -p np-cli --no-default-features
env -u HOME -u XDG_CONFIG_HOME cargo test -p np-core --test config
RUSTDOCFLAGS="--deny warnings" cargo doc --locked --workspace --no-deps
```

Two rules that are not negotiable here: **never regenerate the catalog goldens** (any diff in `tests/golden*` is a stop-and-
report), and a test whose assertion cannot be broken by mutating the code it covers is a defect, not coverage.

The model-backed suites skip loudly instead of failing quietly. Set the two variables and they run for real:

```sh
NP_TEST_MODEL_PATH=~/models/embeddinggemma-2-Q8_0.gguf \
NP_TEST_MMPROJ_PATH=~/models/mmproj-embeddinggemma-2-Q8_0.gguf \
  cargo test -p np-core --features local-model,vision --test local_vision
```

Without them, nothing that needs weights runs. Counted on 2026-11-06 on this host with both variables unset — `cargo test --workspace` prints
**5** such lines (most artifact-gated suites are not even compiled without the runtime), `-p np-core -p np-cli --features local-model` prints
**36**, and the same pair with `--features local-model,vision` prints **45**. They mean "compiled, never ran", never "passed". The commands are
printed beside the numbers on purpose: an earlier pair recorded here (29 / 38) could not be reproduced by any command in this file once F28's
artifact-free cases landed, and a count that moves with every gated case is worth re-measuring rather than trusting. `$NP_TEST_VISION_LARGE` and
`$NP_THREAD_*` gate two further cells the same way. **GitHub Actions does run this repository.** That sentence used to read "cannot execute
jobs for this account today", which was true during the gap `todo.md` dates (no Windows steps from 2026-10-29 to 2026-10-31) and false ever since:
measured on 2026-11-06, `Test (ubuntu-latest)`, `Test (windows-latest)` and `Feature local-model (ubuntu-latest)` each executed and each passed on
every push to `main` at `2c65525`, `935b2cd` and `64a6b73` (runs `42171010611`, `42187057325`, `42188471272`) and on a branch push at `03cb04a`
(run `42192715021`). So a build or test claim about Linux or Windows has CI behind it as well as the local exit codes quoted above — and absent CI
output is still not evidence either way, which is the rule that round wrote down.

What CI never has is the model weights, so the artifact-gated rows print their `NOT COVERAGE` there by design and every *measured* figure in these
docs — bytes, milliseconds, vector counts — still comes from this host with those two variables set. macOS keeps its standing limit: the build fact
only (the unix `statvfs` arm compiles), its number never read here.

## What hosted CI actually grades, and when

Written down because a green check mark and an empty job look identical from the command line unless you ask which one you have — and this repository has been out of included minutes twice.

CI runs again, and reading it is what surfaced **F34**. Measured from the Actions API on 2026-11-05: hosted jobs ran continuously until
2026-10-29T13:11 UTC, created zero steps from 2026-10-29T14:39 through 2026-10-31T20:23, and have run on every push since
2026-11-01T20:08. The run just before `a816b17` was green on Windows, and the **next 43 CI runs all failed**
`Test (windows-latest)` on the same disk-preflight assertion — four days of merges that were green on Linux and red on Windows. F34's
fixes turned that around: green on the branch from `2ba7dc0`, and green on `main` at `b2840f9` (run `42090000404`, all three jobs). "Green" in this README still means the exit codes measured locally and listed
above; the Windows job is now a second reader, and it has already caught something local gates could not.

A fourth job joins on this branch — **`Feature local-model,vision (ubuntu-latest)`** — and it exists because of what the three above could not see:
every `--features local-model,vision` row ever run was graded on `-p np-core`, so a green CI said nothing about whether `np` itself could be built
with image support. It could not (**F28**), and no failure any of the other jobs produce would have said so. Its first grading is the run that adds it,
which is worth stating rather than backdating: this sentence was written before that job had ever gone green.

**Why the zero-step windows happen, now measured (`docs/ops-actions-minutes.md`).** This repository is private, so its hosted minutes are metered against
the account's monthly allowance — and both unexplained gaps in CI line up with that allowance running out. October spent **2,028 billed minutes** and went
zero-step on 2026-10-29T14:39; November had no C++ cache and spent **2,137 in six days**, crossing the cap at `2026-11-07T01:57:05Z`, seven seconds before
the first zero-step failure. A Windows job is 54.6 % of that spend, and both feature rows rebuild the vendored llama.cpp from scratch every run. So a red
main gets one check before anything else: do the jobs have **steps**? `steps=0` on all four means the account is out, not the tree — re-triggering until it
goes green would only teach everyone to ignore CI.

**What CI grades, as of 2026-11-07 (`ci.yml` reshaped for the allowance).** A **push to `main`** runs `Test (ubuntu-latest)` — fmt, clippy, rustdoc with
`--deny warnings`, and the whole workspace test suite including the release-only cases — and nothing else: about 4.6 billed minutes, landing within minutes of
the merge. The **Windows row and both C++ feature rows** now run on a **nightly schedule (03:17 UTC), on any pull request, and on a manual dispatch**, which is
where platform and feature-gated evidence comes from: `gh workflow run ci.yml --ref <sha>` buys the full matrix for one specific tree (~25.4 billed minutes) when
a claim needs it before anyone writes it down. **Pushes to other branches run no CI at all.** That is a trade, not a saving: between a merge and the next nightly,
`main` is graded on Linux only, so a Windows-only lint or a feature-gated compile break can sit there for up to a day. The 17-row local gate list — which includes
the `local-model` and `local-model,vision` clippy, rustdoc and test rows — is what stands in during that window, and it is what ran before every merge this week.

The arithmetic is in `docs/ops-actions-minutes.md`, including the correction it forced: at November's measured cadence (15 runs a day, 6.25 of them onto `main`)
the earlier recommendation to keep Windows and the feature rows on every merge would still have cost ~5,900 billed minutes a month against an allowance inferred at
~2,000. The shape above projects ~1,620.
