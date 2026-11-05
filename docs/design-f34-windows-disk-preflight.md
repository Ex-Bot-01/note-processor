# F34 — the fetch disk preflight has no Windows answer

Design for a defect fix. Filed as **F34** in `docs/backlog-m2-followups.md`. Scope: one function split into three platform arms, a
pure decision extracted from its call site, the tests that pin both, and two docstrings this round proves false. No behaviour change
on Linux, no new dependency, no new exit code.

Date: 2026-11-05. Evidence: GitHub Actions run `42061786030`, job `127365138173` (`Test (windows-latest)`), local reading of
`crates/np-core/src/fetch.rs`, and a design review (`9c0c7722`, static read) whose five P1 findings are folded in below and marked
**[review]** where a sentence exists because of them.

## 1. The failure, verbatim

```text
---- preflight_refuses_before_byte_one_when_the_disk_cannot_hold_the_pair stdout ----
thread '…' panicked at crates\np-core\tests\fetch.rs:787:18:
expected DiskShortfall, got Interrupted { id: "test-model", member: "main", got: 50, expected: 9223372036854775807 }
```

`Test (ubuntu-latest)` and `Feature local-model (ubuntu-latest)` pass on the same commit; this is the only failure, and the other 23
tests in that file passed **on Windows** too.

**It is not a regression — and this section's first draft had the history wrong.** Re-measured from the Actions API on 2026-11-05,
job by job: `Test (windows-latest)` ran **10 steps per run continuously from 2026-10-26T09:02 to 2026-10-29T13:11**; runs then
created **zero steps** from 2026-10-29T14:39 to 2026-10-31T20:23 (the account-side block the CI workflow's own comment dates to
2026-10-29); execution resumed at **2026-11-01T20:08** and has run on every push since. The test arrived with `a816b17` (R7b C2),
pushed 2026-11-01T21:21, so **its first executed run was that commit's own** — `41351611843`, which failed on Windows with the panic
above (`15 passed; 1 failed`), and the run immediately before it (`41345323033`, 2026-11-01T20:08) was **green** on Windows. Counting
the conclusions since: **43 CI runs, 43 Windows failures.** So this is a four-day-old red `main`, not a first-run discovery.

**[review]** An earlier revision of this section claimed every run from 2026-10-31 through 2026-11-04 created zero steps and called
the 2026-11-04 runs the first to execute. That came from sampling the oldest entries of a recent run list — all of which happen to
fall inside the block — and generalising from six rows to four days. The correction is the point: CI was executing Windows, and
reporting this failure, the whole time R7b and R7c were being reviewed and merged. What changed is not that the runner came back; it
is that somebody read it.

## 2. What the preflight is, and what "cannot say" already means

§12 rule 6: before a fetch opens a socket, check that the target filesystem can hold the pinned bytes plus a margin. Today
(pointers are pre-fix, `main` at `833cb06`: `fetch.rs:877-920`):

```rust
#[cfg(unix)]      fn available_bytes(path: &Path) -> Option<u64> { /* statvfs, f_bavail × f_frsize */ }
#[cfg(not(unix))] fn available_bytes(_path: &Path) -> Option<u64> { None }
```

with the refusal only inside an answer (`fetch.rs:552-561`). `None` is not treated as a pass — `FetchReport::free_bytes` is an
`Option`, printed as "not checked on this platform". What was dishonest is different: on Windows the check does not happen while a
test named "refuses before byte one" asserts as if it did.

**[review]** Two sentences about who gets `None` are false today and are corrected by this round rather than repeated:

- The docstring at `fetch.rs:873-876` says "**Windows and macOS** get `None`". macOS is `cfg(unix)`, `libc` declares `statvfs` for
  every unix but linux-gnu (`libc-0.2.191/src/unix/mod.rs:1672-1674`) with `struct statvfs` for apple-darwin
  (`…/unix/bsd/apple/mod.rs:420`), and np-core's `libc` dependency is gated on `cfg(unix)` (`crates/np-core/Cargo.toml:57-58`). So
  the shipped macOS binary (`release.yml:35-36,45`, target `aarch64-apple-darwin`) compiles the **statvfs arm**, which is a build fact.
  That it therefore *prints a number* is a step further than evidence goes: the call has to succeed and `f_frsize` must be non-zero
  (`fetch.rs:907-912` returns `None` otherwise), and nobody has run a macOS binary here.
- §12 rule 6 therefore already applies on macOS. What is unknown there is whether its *number* is right — no runner, no measurement,
  and this round says nothing either way about that.

## 3. Decision: answer the question on Windows too

`GetDiskFreeSpaceExW`, declared by hand against `kernel32`:

```rust
#[link(name = "kernel32")]
extern "system" {
    fn GetDiskFreeSpaceExW(
        directory_name: *const u16,
        free_bytes_available_to_caller: *mut u64,
        total_number_of_bytes: *mut u64,
        total_number_of_free_bytes: *mut u64,
    ) -> i32; /* BOOL */
}
```

- **It returns bytes**, so there is no block-size multiplication to overflow. The unix half's two guards (`f_frsize == 0` and
  `checked_mul`) stay where they are and do not migrate.
- **Its first out-parameter is free bytes available to *the caller*** — the same *shape* of choice as the unix half's `f_bavail`
  over `f_bfree`: ask what this writer may actually use. **[review]** Not "the same reason": the unix difference is root-reserved
  blocks, the Windows one is quota. The distinction is why the first out-parameter is chosen and not
  `total_number_of_free_bytes`, which would over-report.
- **No dependency.** One symbol does not justify adding a Windows bindings crate to every binary; hand-written `extern` blocks are
  the pattern this repository already owns (`crates/np-core/src/llama_ffi.rs:203`).

## 4. Semantics, stated per case

| Situation | Windows behaviour | Why |
| --- | --- | --- |
| Call succeeds | `Some(bytes)`, **including 0** | A successful answer is an answer; `0` refuses the fetch, which is what rule 6 wants. Unix discards only an *unusable* answer (`f_frsize == 0`), never an unwelcome one — same posture |
| Call fails (permission, gone, not a directory) | `None`, printed as "not checked on this platform" | Absence is stated, never a pass. Note this arm is now **hard to reach deliberately**: the climb below means most bad paths still answer from a valid ancestor, and `encode_wide` cannot fail — so no test pretends to cover it by passing a nonsense path **[review]** |
| Models directory does not exist yet (a first fetch) | climb to the nearest existing ancestor and ask about its filesystem | Load-bearing, because the preflight runs *before* `create_dir_all` (`fetch.rs:557`): a first fetch always asks about an absent directory. Parity with the unix half's reason |
| Path climbs out of every existing ancestor (a relative path under nothing) | `None`, before the free-space query is reached — the climb's own `symlink_metadata` calls are the only OS work done | The deterministic decline arm, reachable on **every** host and therefore testable on every host **[review]** |
| Name contains an interior NUL, under an existing ancestor | **answers about the ancestor** — the same thing unix does, measured on this host: `symlink_metadata` refuses a name holding NUL, so the climb never sees it | **[review]** The first draft of this row said Windows "declines explicitly" via the encoder check. That check is unreachable *from* `available_bytes`, for exactly this reason: the climb only stops on a path `symlink_metadata` accepted, and such a path holds no NUL. So both halves decline to invent an answer for the named path, and neither refuses to answer about an ancestor — there is no asymmetry to test |
| `wide_name` handed a string holding U+0000 (no caller does today) | `None` | Kept as defensive depth in a general helper: `encode_wide` encodes U+0000 faithfully and Win32 ends the string there, so a future caller that skipped the climb would get an answer about the **surviving prefix**. Pinned directly on the helper, on Windows only; **no test claims this is a route `available_bytes` takes** |
| Path is long enough that Win32 refuses it (`MAX_PATH`) | asked through **the same spelling `walk::to_open_path` gives the `.part` write and the publish rename**: `walk::long_prefix(…, true)`, which adds `\\?\` for long absolute paths | **[review]** Declining here while the write and rename go through that helper (`fetch.rs:703`, `806`) would mean "rule 6 skipped, bytes written anyway", and this crate already prefixes long paths deliberately and tests both answers of it (`walk.rs:395-406`, `tests/walk.rs:213-219`): reuse, not invention. Two hedges. It is **a prediction** that an unprefixed path over `MAX_PATH` refuses — no test plants a >260-char models dir on any platform. And not every call on this path agrees: `create_dir_all` (`fetch.rs:557`) and the target-absence checks (`fetch.rs:568-571`) still take the bare path. `\\?\` also needs an absolute path, so a relative `data_dir` keeps the plain spelling and may answer `None`; and the spelling is lossy (`to_string_lossy`) and un-normalised, which can only cost an answer, never fake one |
| Models directory exists but **is not a directory** | expected to fail → `None`. **A prediction, not a measurement**: unix's `statvfs` happily answers for a file, so the halves differ here | **[review]** Either way the next line, `create_dir_all` (`fetch.rs:563`), turns it into `FetchError::Io`, so no fetch proceeds on either platform. The Windows answer is recorded as unverified; if the runner returns `Some`, this row gets corrected rather than defended |

## 5. Shape: three arms, and a decision that can be tested anywhere

**[review]** Three arms, never two — and the repository already states why verbatim (`path_keys.rs:214-216`): gate on `windows`,
not `not(unix)`, "because that is what its body needs: `encode_wide` … exists on no other target, so a wider gate would not be a
fallback — it would be a build failure". Writing `#[cfg(windows)]` and leaving `#[cfg(not(unix))]` behind would hand macOS the FFI
and break a **shipped release target** (`release.yml:35-36`) while every CI row stayed green: `ci.yml` has three rows (two from the
`os` matrix plus `Feature local-model`) and none of them builds `aarch64-apple-darwin` — only `release.yml` does, after merge.

```rust
#[cfg(unix)]                            fn available_bytes(path: &Path) -> Option<u64> { /* unchanged */ }
#[cfg(windows)]                         fn available_bytes(path: &Path) -> Option<u64> { /* GetDiskFreeSpaceExW */ }
#[cfg(all(not(unix), not(windows)))]    fn available_bytes(_path: &Path) -> Option<u64> { None }
```

Any Windows-only helper (`wide_name`, the `extern` block, its tests) carries `#[cfg(windows)]` itself. `check_cfg_dead_code.py`
records why in its header — "eight consecutive CI runs failed on `windows-latest` with `function … is never used -D warnings`" — and
the visible history corroborates the shape without confirming the count: five Windows runs failed on 2026-10-26 before the first
green at 10:57. This round does not re-verify that count; it adds a fresh instance of the same class (§10).

The reason this gap could hide at all is that the decision and the OS query were fused in one inline `if`. Split them, keeping
behaviour identical:

```rust
/// §12 rule 6's admission check, given what the platform could say. Pure, so all three arms are covered on any host.
fn preflight(needed: u64, available: Option<u64>) -> Result<(), FetchError>
```

- `Some(available)` with `available < needed` → `DiskShortfall { needed, available, margin }`, fields and message unchanged.
- `Some(_)` otherwise → `Ok(())`.
- `None` → `Ok(())`, and the report keeps its "not checked" line.

The call site binds the value first — the report prints exactly the number that was compared, so it must not ask the OS twice:

```rust
let free = available_bytes(models_dir);
preflight(needed, free)?;
```

**Ordering stays put**: identity (`fetch.rs:500-521`) → marker (rule 4, `fetch.rs:524-540`) → preflight → `create_dir_all`
(`fetch.rs:557`). **[review]** That ordering is unfalsifiable today — no test combines a live marker with an oversized pin, so if
`preflight` drifted above the marker arm, `MarkerExists` would become `DiskShortfall` and nothing would fail. §6 adds that test.

## 6. Test plan, and where each test actually runs

| Test | Runs on | Asserts |
| --- | --- | --- |
| `preflight_refuses_before_byte_one_when_the_disk_cannot_hold_the_pair` (existing) | every runner — it never had a `#[cfg]` on it; there was simply nothing behind it on Windows | `DiskShortfall`, no transport opened, models dir absent. **[review]** Its own assertion is weak here: with `needed ≈ 9.2e18` any answer satisfies `needed > available`, so on Windows this proves "the call returned `Some` and did not crash", nothing more. The rows below carry the weight |
| `available_bytes_answers_a_plausible_number_for_an_existing_directory` (`#[cfg(windows)]`) | Windows CI only | `Some(n)` with `n > 1 MiB` for a temp directory — catches a wrong signature, an inverted `BOOL`, or an out-parameter that never gets written |
| `the_three_out_parameters_keep_their_order` (`#[cfg(windows)]`) | Windows CI only | calls the FFI directly with all three zero-initialised and asserts `caller_available ≤ total_free ≤ total_bytes`. **[review]** The only runner-executable check that pins **parameter order**: rows above read one number, so a swap of `free_bytes_available_to_caller` and `total_number_of_free_bytes` would pass them all while over-reporting — precisely the distinction §3 argues for. Slack for concurrent writers is stated in the assertion message |
| `available_bytes_climbs_out_of_a_directory_that_does_not_exist_yet` (`#[cfg(windows)]`) | Windows CI only | `Some(_)` from `<temp>/f34-absent/models`, pinning the climb that a first fetch depends on |
| `a_name_holding_an_interior_nul_never_yields_an_answer` (`#[cfg(windows)]`) | Windows CI only | two arms kept apart on purpose: `available_bytes("\0f34")` is `None` because the **climb** runs out, and `wide_name("a\0b")` is `None` because the encoder refuses. **[review]** The first draft claimed this row "proves the encoder's zero-unit check fires"; it cannot — no path reaching `wide_name` from `available_bytes` holds a NUL (§4). What it proves is that neither route yields an answer about a prefix |
| `the_unix_arm_answers_about_an_existing_ancestor` (`#[cfg(unix)]`) | unix runners | measures what the unix half really does: answers for a real directory, and still answers (about the ancestor) for `<temp>/f34\0models`. Written because §4's first draft asserted the opposite of this **[review]** |
| `available_bytes_declines_when_no_ancestor_exists` (unit test, host-independent) | every runner | `available_bytes(Path::new("f34-no-such-root").join("models"))` is `None` on unix and Windows alike — the climb-exhausted arm, reached before the free-space query **[review]** |
| `preflight_arms_are_the_three_outcomes` (unit test) | every runner | the pure function directly: `None` is not a shortfall, `Some(0)` is, enough space is neither |
| `a_marker_refuses_before_the_disk_check_does` (integration, new) | every runner | marker written **and** `size_bytes: u64::MAX / 2` → `MarkerExists`, pinning the order §5 promises **[review]** |

The unit tests live in a new `#[cfg(test)] mod tests` inside `src/fetch.rs` — `preflight` and `available_bytes` are private, so
`tests/fetch.rs` cannot reach them; house precedent at `src/path_keys.rs:241`. **[review]**

**Housekeeping that rides along** (zero behaviour risk, found while reading this function): the comment at `fetch.rs:905-906` is
duplicated byte-for-byte, and `fetch.rs:873-876`'s docstring carries the false macOS claim from §2. Both are corrected here because
this round is what reads them.

## 7. What CI proves here, and what it does not

**Proves on real Windows:** that the `extern "system"` signature links against `kernel32`; that parameter order is as declared (the
relational test); that a successful call yields a plausible number; that the climb works on NTFS; that a NUL-bearing name yields no
answer and that `wide_name` refuses such a string in isolation; and that rule 6 refuses before byte one. It newly executes **the FFI**
and the four `#[cfg(windows)]` unit tests — the other 23 tests in `tests/fetch.rs` already passed on that runner (run `42061786030`,
so nothing else is being claimed **[review]**).

**It also proves a thing no local gate can:** the first push of this branch failed Windows **clippy**, not a test — see §10. Local
gates were green on 15/15 rows.

**Does not prove:** whether macOS's statvfs number is right (it compiles the unix arm and has no runner here); quota behaviour
(chosen by reasoning about which out-parameter to read); whether a models path that is a *file* answers or declines on Windows (§4
records it as a prediction); concurrency slack in the relational test; or anything about targets with neither unix nor windows.

**No untested platform claim (F18):** this round adds no assertion about macOS, and its docstring stops saying macOS gets `None`.
What it *does* say about macOS is a build fact only: the target compiles the unix `statvfs` arm (`Cargo.toml:57-58`,
`release.yml:35-36`), and its number has never been read here.

`docs/plan-2026-10-28-state-and-roadmap.md:62` says "CI green Linux + Windows" for the shipped milestones. **That was true** — those
milestones' Windows jobs ran, 10 steps each, through 2026-10-29 (§1). An earlier draft of this round rewrote it to "nothing in this
subgraph was ever executed on Windows", which the job-by-job scan falsifies; the wording now restored adds only what is measured:
the block window, the resumption, and that `main` has been red on Windows since `a816b17`. The same scan retires the "CI cannot start
jobs on this account" sentences in `README.md`, `docs/plan-…:161`, `docs/roadmap-2026-10-27.md` and `.github/workflows/ci.yml`.

## 8. Non-goals

No new extended-path logic — `walk::long_prefix` is reused rather than reinvented (§4). No free-space polling or retry-on-ENOSPC
during transfer: a disk that fills mid-stream already fails at `write_all` (`fetch.rs:722-725`) or `sync_all` (`fetch.rs:744-746`) as
`FetchError::Io` — **not** `Interrupted`, which is the short-stream arm at `fetch.rs:753` — and the error path at `fetch.rs:601-615`
removes the `.part` file and the marker before returning the reason. The preflight refusal needs none of that: it answers before
`create_dir_all`, so there is nothing to clean up. Which platforms have *executed* that cleanup is a separate question — the
digest/length cases exercising it ran on Windows in `42061786030` and have never run on macOS.
No change to the unix arithmetic. No new dependency, no new exit code, no changed report field or golden byte.

## 9. Risks, and what retires each

| Risk | Retired by |
| --- | --- |
| Wrong FFI dialect (an `extern` block this crate's edition rejects) | the probe: an isolated crate with exactly this block type-checked for `x86_64-pc-windows-msvc --all-targets` at edition 2021 (§10). **That is all the probe retires** **[review]** |
| Wrong signature, arity or **parameter order** | linking and calling on the Windows runner, plus the relational out-parameter test. Neither the probe nor any Linux run can see this: a self-declared `extern` block compiles against nothing but itself, kernel32 exports are undecorated, and every other planned test reads a single number |
| Reading `BOOL` backwards (`TRUE` is non-zero, not `1`) | the success side only: read as `!= 0`, pinned on the runner by the temp-directory test (inverted, it would read `Some(0)` and fail its ">1 MiB" assertion). **The failure side is untested** — no case makes the call return 0, and §4 explains why one cannot be planted — so inverting `if ok != 0` would read as "free space not checked" on every Windows fetch and no gate here would notice |
| A Windows-only lint that no Linux command can see | **nothing local.** The first push of this branch failed exactly there (§10). Retired only by step 3: push the branch, read the Windows job, before asking for a merge |
| Garbage out-parameter read as data | all three are zero-initialised before the call, and the temp-directory test asserts a plausible magnitude |
| macOS release build breaks on Windows-only code | the three-arm rule in §5, stated with the repository's own reason. **No CI row catches this** — none of the three builds `aarch64-apple-darwin`; only `release.yml:35-36` would, after merge — so it rests on the gate shape and the precedent at `path_keys.rs:206-233`, and that is recorded rather than glossed |
| The refactor changes unix behaviour | the fetch suite as it stood before this round (24 tests in `tests/fetch.rs`, 25 now) plus the full local gate list, run where they have always run — and, since this row was written, on a Windows runner too |
| A wrong `Some(0)` refusing every Windows fetch | the temp-directory test asserts more than a megabyte, on the runner, before anything ships |

## 10. Rollout

1. **Probe, measured 2026-11-05** (`/home/agent/tmp/winprobe`, throwaway): exactly this `#[link(name = "kernel32")] extern "system"`
   block, the `encode_wide` conversion and the climb loop. `cargo check --target x86_64-pc-windows-msvc --all-targets` → **exit 0** at
   the workspace's own edition (2021), where a plain `extern "system"` is legal. The same file compiled for the host → **fails**, as
   it must, since the code lives under `#[cfg(windows)]`. A whole-crate cross-check is impossible on this host: bundled SQLite needs
   MSVC's `lib.exe` (measured: `failed to find tool "lib.exe"`).
2. Implement on branch `f34/windows-disk-preflight`, then the full local gate list — fmt; clippy at default, `local-model` (core and
   cli) and `local-model,vision`; `-p np-cli --no-default-features` for clippy and tests; workspace tests; config tests without
   `$HOME`; rustdoc `--deny warnings` at default and feature levels; **`check_cfg_dead_code.py` and `check_attr_displacement.py`**,
   which exist precisely because Windows caught this cfg shape twice after local green; goldens.
3. Push the branch **before asking for a merge**, so the Windows job is evidence: the four `#[cfg(windows)]` tests plus the un-gated
   preflight test either pass on real Windows or this round goes back to the drawing board.
   **Measured 2026-11-05, this step paid for itself at once.** Head `9de3b2d`, with all 15 local gate rows at exit 0 and 651 tests
   passing, failed `Test (windows-latest)` at the **Clippy** step: `error: unused doc comment` at `fetch.rs:979` — a `///` above the
   `extern` block, and rustdoc generates no documentation for extern blocks, so `-D warnings` rejects it on the one runner that
   compiles the block. Both ubuntu rows passed. Fixed by making those comments plain `//`, with the reason written where someone
   would otherwise "fix" it back. Neither cfg-integrity script covers this lint class: only the runner could.
4. Reviewers (code, and platform-claims) read the diff; fix loop; re-measure.
5. Merge only on the human's word.
