# F34 — the fetch disk preflight has no Windows answer

Design for a defect fix. Filed as **F34** in `docs/backlog-m2-followups.md`. Scope: one function, one refactor of its call site,
and the tests that pin both. No behaviour change on Linux, no new dependency, no new exit code.

Date: 2026-11-05. Evidence: GitHub Actions run `42061786030`, job `127365138173` (`Test (windows-latest)`), plus local reading of
`crates/np-core/src/fetch.rs`.

## 1. The failure, verbatim

```text
---- preflight_refuses_before_byte_one_when_the_disk_cannot_hold_the_pair stdout ----
thread '…' panicked at crates\np-core\tests\fetch.rs:787:18:
expected DiskShortfall, got Interrupted { id: "test-model", member: "main", got: 50, expected: 9223372036854775807 }
```

`Test (ubuntu-latest)` and `Feature local-model (ubuntu-latest)` pass on the same commit; this is the only failure.

**It is not a regression.** Every CI run from 2026-10-31 through 2026-11-04 created **zero steps** (an account-side block, which
is why every claim in this repository's docs is backed by local exit codes). The runs that first *executed* are from 2026-11-04
onward, so this test has never run on Windows before, and neither has the code it exercises. The test arrived with `a816b17`
(R7b C2); the platform gap arrived with it and stayed invisible until a runner existed.

## 2. What the preflight is, and what "cannot say" already means

§12 rule 6: before a fetch opens a socket, it checks that the target filesystem can hold the pinned bytes plus a margin. Today:

```rust
#[cfg(unix)]      fn available_bytes(path: &Path) -> Option<u64> { /* statvfs, f_bavail × f_frsize */ }
#[cfg(not(unix))] fn available_bytes(_path: &Path) -> Option<u64> { None }
```

and the call site refuses only when it gets an answer:

```rust
let free = available_bytes(models_dir);
if let Some(available) = free {
    if available < needed { return Err(FetchError::DiskShortfall { … }); }
}
```

`None` is **not** treated as a pass in the report — `FetchReport` carries "free space not checked on this platform" for exactly
that case, which was the honest part of the original design. What is *not* honest is that on Windows the check silently does not
happen while a test named "preflight refuses before byte one" asserts as if it did. The test is what CI caught; the gap behind it
is what this round fixes.

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

Three reasons this is the right call and no other:

- **It returns bytes**, so there is no block-size multiplication to overflow — unlike unix, where `f_bavail × f_frsize` needs
  `checked_mul` and a zero-block-size answer has to be discarded as unusable. The two guards in the unix half stay there and do
  not migrate.
- **The first out-parameter is *available to the caller***, which on a quota'd volume is the number that matters — the same reason
  the unix half reads `f_bavail` rather than `f_bfree`. `total_number_of_free_bytes` would over-report what this process may write.
- **No dependency.** A `windows-sys`/`winapi` crate for one symbol is a build-graph change in every binary, including the ones that
  never touch this path; hand-written `extern "system"` is the pattern this repository already owns and documents in
  `crates/np-core/src/llama_ffi.rs`.

## 4. Semantics, stated per case

| Situation | Windows behaviour | Why |
| --- | --- | --- |
| Call succeeds | `Some(bytes)`, **including 0** | A successful answer is an answer. `0` refuses the fetch, which is what rule 6 wants; unix discards only an *unusable* answer (`f_frsize == 0`), not an unwelcome one |
| Call fails (bad path, permission, gone) | `None` → report says "free space not checked on this platform" | Same shape as today's unix failure: absence is stated, never a pass |
| Models directory does not exist yet (a first fetch) | climb to the nearest existing ancestor and ask about its filesystem | Parity with the unix half, whose comment gives the reason: `statvfs`/this call on a missing path answers "not found", which would make every fresh machine an unchecked fetch |
| A path the Win32 layer will not take (beyond `MAX_PATH` without a `\\?\` prefix) | `None`, stated | Handling extended-length prefixes properly is its own design; guessing at it by silently prepending `\\?\` changes path semantics. Declining to answer is the honest failure, and §7 records it as untested |
| Path contains lone surrogates / non-Unicode bytes | encoded with `OsStrExt::encode_wide`, which is lossless for what Windows can hand us | Windows paths are UTF-16 natively; there is no lossy step to take |

## 5. Refactor: make the *decision* testable on every host

The reason one platform gap could hide a whole check is that the decision and the OS query were fused in one inline `if`. Split
them, keeping behaviour identical:

```rust
/// §12 rule 6's admission check, given what the platform could say. Pure, so all three arms are covered on any host.
fn preflight(needed: u64, available: Option<u64>) -> Result<(), FetchError>
```

- `Some(available)` with `available < needed` → `DiskShortfall { needed, available, … }` (unchanged fields and message).
- `Some(_)` otherwise → `Ok(())`.
- `None` → `Ok(())`, and the caller's report keeps its "not checked" line.

The call site becomes `preflight(needed, available_bytes(models_dir))?`. Everything the existing tests assert about messages,
ordering against the marker check, and "no byte transferred" stays true on every platform; the OS query is the only platform-shaped
piece left.

## 6. Test plan, and where each test actually runs

| Test | Runs on | Asserts |
| --- | --- | --- |
| `preflight_refuses_before_byte_one_when_the_disk_cannot_hold_the_pair` (existing) | **every runner, unix and Windows alike** — no longer gated | `DiskShortfall`, `fake.opens() == 0`, models dir absent. On Windows this is the first executed proof of the FFI |
| `available_bytes_reports_for_an_existing_directory` (`#[cfg(windows)]`) | Windows CI only | `Some(n)` with `n > 0` for a temp directory — catches a wrong signature, a bad `BOOL` reading, or an inverted success test |
| `available_bytes_climbs_out_of_a_directory_that_does_not_exist_yet` (`#[cfg(windows)]`) | Windows CI only | the same `Some(_)` from `<temp>/not-created-yet/models`, pinning the climb |
| `available_bytes_declines_for_a_name_that_cannot_cross_the_boundary` (unit test in `src/fetch.rs`, **every** runner) | every runner | `None` for a path whose name holds an interior NUL — the deterministic way to reach the failure arm, and exact parity with the unix half, whose `CString::new(...).ok()?` declines the same input. Chosen because the climb makes *most* bad paths still answer from a valid ancestor: asserting "a nonsense path yields `None`" on Windows would be asserting something the climb deliberately prevents |
| `a_real_answer_is_not_zero` (`#[cfg(windows)]`) | Windows CI only | a successful call on the runner reports more than a megabyte free, so a wrong out-parameter cannot quietly refuse every Windows fetch |
| `preflight_arms_are_the_three_outcomes` (new, host-independent) | every runner | the pure function's three arms directly: `None` is not a shortfall, `Some(0)` is, and enough space is neither — which is how the arm that Windows cannot reach today stays covered anyway |

The `#[cfg(windows)]` tests are **executed only by the Windows runner**; everything else in the table runs on both. That
asymmetry is exactly what §7 exists to state.

## 7. What CI proves here, and what it does not

Proves: the `extern "system"` signature links against real `kernel32`, the wide-string conversion survives a real path, the climb
works on NTFS, a successful call yields a plausible number, a bad path yields `None`, and rule 6 refuses before byte one on
Windows. It is the first Windows execution this repository has ever had, so it also proves the surrounding suite's other 23 tests
in that file still hold there.

Does not prove: quota'd volumes (the caller-available-vs-total distinction is chosen on reasoning, not measurement), paths beyond
`MAX_PATH`, network-redirected drives, or anything at all about **macOS**, which keeps `None` and has no runner in this workflow.
No untested platform claim (F18): the macOS sentence stays exactly as it is — "cannot say", stated as such — and this round adds
no assertion that macOS behaves any other way.

## 8. Non-goals

No `\\?\` extended-path handling. No free-space polling or retry-on-ENOSPC during transfer (a full disk mid-stream is already
`Interrupted`, which is a refusal, not a corruption). No change to the unix arithmetic. No new dependency, no new exit code
(runtime refusals stay 1), no change to any report field name or golden byte.

## 9. Risks, and what retires each

| Risk | Retired by |
| --- | --- |
| Wrong FFI signature or ABI — the classic failure of hand-written Windows bindings | a probe crate compiled for `x86_64-pc-windows-msvc` before this design was accepted (result in §10), plus the Windows runner linking and calling it |
| Reading `BOOL` backwards (`TRUE` is non-zero, not `1`) | success is tested as `!= 0`, and the three Windows tests cover both outcomes |
| Writing to an out-parameter that stays untouched on failure | the failing-path test asserts `None`, not a garbage `Some(0)`; out-params are zero-initialised before the call so a stale-read bug cannot masquerade as data |
| The refactor changes unix behaviour | the existing fetch suite (24 tests) plus the full local gate list, run on Linux where they have always run |
| A wrong `Some(0)` refusing every Windows fetch | the temp-directory test asserts `> 0`, and CI runs it before anything can ship |

## 10. Rollout

1. **Probe, measured 2026-11-05** (`/home/agent/tmp/winprobe`, throwaway): a crate holding exactly the `#[link(name =
   "kernel32")] extern "system"` block, the `encode_wide` conversion, the climb loop and four tests.
   - `cargo check --target x86_64-pc-windows-msvc --all-targets` → **exit 0**, at the workspace's own edition (2021), where a plain
     `extern "system"` block is legal — so no `unsafe extern` ceremony is needed, and the probe proves the exact syntax this crate
     will ship rather than a newer dialect.
   - The same file compiled for the host → **fails**, which is the point: the code only exists under `#[cfg(windows)]`, and the
     failure is what stops it leaking into the Linux build.
   - A whole-crate cross-check (`cargo check -p np-core --target x86_64-pc-windows-msvc`) **cannot** run on this host: bundled
     SQLite needs MSVC's `lib.exe` (measured: `failed to find tool "lib.exe"`). So isolated compile-verification is available here
     and whole-crate verification happens on the Windows runner — which is where behaviour gets proven either way.
2. Implement on branch `f34/windows-disk-preflight`; run the full local gate list (unix rows, the no-fetch row, doc rows, goldens).
3. Push the branch **before asking for a merge**, so the Windows job is evidence rather than hope: the three `#[cfg(windows)]`
   tests plus the un-gated preflight test either pass on real Windows or this round goes back to the drawing board.
4. Reviewers (code + platform claims) read the diff; fix loop; re-measure.
5. Merge only on the human's word.
