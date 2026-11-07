# Distribution and graded CI: what is broken, what was done, what one decision unlocks

Measured on 2026-11-07 from this box. Every claim below is either a command you can re-run or an HTTP status this session
received. Nothing here guesses at the account's billing state, because the endpoints that would show it answer 403 to this
token (§3).

## 1. The two blockers, stated as facts

**Graded CI.** Included Actions minutes ran out at `2026-11-07T01:57:05Z` (`docs/ops-actions-minutes.md`: 2,028 billed minutes
in October, 2,137 in the first 6.4 days of November). Since then every run on `main` completes as a failure with **zero steps on
every job** — including `42394561369` on `2777aeb`. A job that never reaches a step cannot be a code defect; the last tree CI
actually graded is `5e368ac` (run `42325538290`, four jobs success).

**Distribution.** `.github/workflows/release.yml` builds on `windows-latest`, `ubuntu-latest` and `macos-latest`, so it is metered
like everything else and cannot run either. The result is the failure mode the repository warns about elsewhere: **the published
`v0.1.0` assets (2026-10-28) ship a binary with one subcommand** (`scan`) while `main` has `scan`, `embed`, `search` and `models`.
A user following README's "Pre-built binaries" section today downloads something that cannot search.

## 2. What was done today, without spending anything

| Act | Where | Honest limit |
| --- | --- | --- |
| Version moved to **0.2.0** so a release can name the tree it contains | `Cargo.toml`, `Cargo.lock` | — |
| `.github/scripts/package-assets.sh` builds and packages an asset in exactly the workflow's shape (same archive name, same empty-archive refusal, plus `SHA256SUMS.txt`) | repo | **one platform per machine**: a Windows asset needs the MSVC linker, a macOS asset needs Apple's SDK. This box produces Linux x86-64 and nothing else |
| `.github/scripts/gates.sh` runs the whole 17-row gate list (plus the two artifact-backed rows when `NP_TEST_MODEL_PATH` / `NP_TEST_MMPROJ_PATH` are set), prints the tree sha, and counts `NOT COVERAGE` lines so a vacuous green cannot hide | repo | grades Linux; it is not a runner and does not pretend to be |
| `v0.2.0` published with the locally built Linux asset and notes that say where it came from | GitHub Releases | Windows and macOS are **source-only in this release**, stated in the notes rather than left as a missing file |

Reproduce any of it:

```bash
.github/scripts/gates.sh /tmp/np-gates                       # graded locally, 17 rows
CMAKE=$HOME/tools/cmake/bin/cmake .github/scripts/gates.sh   # with the local-model rows
.github/scripts/package-assets.sh dist                       # then read the printed sha256
```

### 2.1 What the first local grading run of `v0.2.0` reported, and what was wrong with the script

The first run of `.github/scripts/gates.sh` on the tagged tree came back **red on six rows** — three cargo rows at exit 101 and
the three python checks at exit 120 with empty logs. Reading the logs before believing them:

* `ld terminated with signal 7 [Bus error]`, inside `rust-lld`, while linking `np-cli`'s test binaries.
* CPython's exit status **120 means "failed to flush stdout at interpreter exit"**, which is what a process does when the machine
  is out of memory rather than out of ideas.
* This box reports `nproc` = 16 and `MemTotal` = 7.2 GB; an uncapped `cargo test --workspace` links up to sixteen debug binaries
  with full debuginfo at once. All six rows passed on a plain re-run, so the tree was never implicated.

Two script bugs came out of the same run and are fixed in `.github/scripts/gates.sh`: cmake detection used `${CMAKE%%/*}`, which is
the empty string for an absolute path, so the four `local-model` rows reported **SKIPPED on a box where cmake was installed and
`$CMAKE` pointed at it** — a skip that would have quietly shrunk every future local grade; and rows now run with
`CARGO_BUILD_JOBS=4` (overridable), which is this box's fit, not a universal number. The header prints the tree sha, the job cap and
RAM so a log says what it was graded on.

Re-graded after the fixes, same tree `4c5200f`: **17 rows exit 0**, artifact rows skipped because no artifact paths were exported,
`NOT COVERAGE` count 0 across all logs. The asset published for `v0.2.0` is `np-0.2.0-x86_64-unknown-linux-gnu.tar.gz`,
3,948,871 bytes, sha256 `51644f639801364eb2b82999511c720309007dc4598fc7eb293f05784619c1d2`, built from the tagged tree by
`.github/scripts/package-assets.sh` and smoke-tested out of the archive (`np 0.2.0`; four subcommands; `scan`; a keyword hit at exit 0
with the F36 caveat on stderr; `--mode semantic` refusing at exit 1). The tag stays where the binary was built; this paragraph's fixes
touch only the scripts and this file.

One loose end from the earlier dispatch experiments: GitHub also holds a release object named **`61dbd85`** — an accidental release
created on a raw-SHA tag by a `workflow_dispatch` test, with no assets. It has been converted to a **draft** rather than deleted, so it
disappears from the releases page and stays recoverable; deleting it outright is one `gh release delete 61dbd85` away if that is wanted.

## 3. What this token can and cannot do (measured 2026-11-07, fine-grained PAT)

| Endpoint | Result | Consequence |
| --- | --- | --- |
| `POST …/releases`, `DELETE …/releases/{id}`, `git push --tags` | works | releases and assets are mine to publish |
| `GET …/actions/cache/usage` | 200 (`38,707,858 B` of cache in use) | cache state is readable |
| `GET users/Ex-Bot-01/settings/billing/actions` | **403** token scope | minutes spent and the spending limit are unreadable from here |
| `GET …/actions/permissions` | **403** | cannot read or change which runners a workflow may use |
| `GET …/actions/runners`, `POST …/runners/registration-token` | **403** | **cannot register a self-hosted runner**, which is the free route to graded CI |
| `PATCH repos/Ex-Bot-01/note-processor` (visibility) | **403** | cannot make the repo public, which is the other free route |

So both remaining fixes need either the web UI or a token with **Administration: write** (visibility) and **Actions: write**
(self-hosted runners). `gh auth status` shows the account; the scope matrix above shows what it is worth.

## 4. Four routes back to graded CI

| Route | Cost | Time to first green run | What it cannot do | Who can do it |
| --- | --- | --- | --- | --- |
| **A. Make the repo public** | $0 — public repos get free GitHub-hosted minutes (2,000 Linux concurrent-minutes/month on the free plan) and this repository's reshaped matrix spends ~1,800 at the current cadence (`docs/ops-actions-minutes.md` §option E arithmetic) | immediately: the next push gets steps | nothing about Windows quality is proven by a hosted runner either way (F18 stays open until someone runs it interactively) | web UI: Settings → General → Danger Zone → Change visibility. Needs Administration scope for an API call |
| **B. Self-hosted runner on this Linux box** | $0 — self-hosted runners are not metered | after registration + one push | Linux only: no Windows or macOS grading, and no hosted release matrix. **Security caveat that matters**: `pull_request` runs would execute contributor code on this machine with this machine's credentials, so keep the runner for `push`/`schedule` on `main` (or gate PRs with the "required reviewers" setting) | needs `POST …/runners/registration-token`, i.e. Actions: write |
| **C. Buy minutes** (raise the spending limit, or GitHub Pro at $4/user/month giving 3,000 included private minutes) | ~$80/month at November's measured pace on the current matrix; ~$4/month plus scoping if Windows and both feature rows stay off `main` pushes | immediately | money is the thing being decided, so this is the human's call, not mine | billing UI |
| **D. Wait for the monthly reset** | $0 | unknown: the reset date is unreadable at this scope; the allowance crossed on 2026-11-07, so plan on the start of the next cycle | nothing gets graded in the meantime, including release builds | nobody has to do anything |

**Recommendation.** A if the vault-indexing code and its history are fit to be public — checked today: `git log -p --all` finds
no token, key or credential pattern, and the only host-specific strings are a `/home/agent/models/…gguf` path in a comment
(`crates/np-core/src/pins.rs:93`) and probe logs under `crates/np-core/tests/probes/`. If the repository must stay private, B is
the cheapest way to get real grading on Linux, with C as the only route that restores the Windows row.

## 5. What the Windows test needs regardless of all this

Nothing in §4 blocks cloning and building on Windows — README's "From source (the supported path)" is exactly that, and it has
never been CI-proven on interactive hardware, which is F18. Expectations for a first clone:

```powershell
winget install Kitware.CMake Ninja   # only for --features local-model / vision
# MSVC Build Tools (Visual Studio "Desktop development with C++") provide link.exe and clang-cl
git clone https://github.com/Ex-Bot-01/note-processor.git
cd note-processor
cargo build --release --locked -p np-cli                 # default: no C++, no model, keyword search works
cargo build --release --locked -p np-cli --features local-model   # adds inference; needs cmake + ninja on PATH
```

The default build is the interesting first test on that platform: `np scan`, then `np search <ws> <words>` answers from FTS5 with
no artifact present (**F36**), which is the capability that did not exist before 2026-11-07. `np embed` there will report the
runtime it does not have, and that sentence — not a crash — is the thing worth reading closely.
