# `np serve` progress reporting — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a program driving `np serve` see what the backend is doing — opt-in `np/progress` notifications during `scan`, `embed` and `search`, plus a `status.get` method that reports what finished and what fetch is recorded on disk.

**Architecture:** np-core gains one JSON-free progress seam (`np_core::progress`) that its fetch and embed loops call at checkpoints they already compute; `np serve` owns a single stdout writer (`Channel`) which turns phase events into throttled JSON-RPC notifications when the client opted in at `initialize`. The CLI passes a no-op reporter, so its bytes do not move. `status.get` reads marker files from the workspace's models directory and never opens a database.

**Tech Stack:** Rust 2021, `serde_json` (np-cli only), `rustls`/HTTP transport behind feature `fetch`, existing test harnesses (`crates/np-cli/tests/serve.rs` child-pipe tests, `crates/np-core/tests/fetch.rs` fake transports).

**Spec:** `docs/superpowers/specs/2026-12-09-np-serve-progress-design.md` — read it with this plan; every value below comes from it.

## Global Constraints

- **stdout is the protocol.** Progress lines exist only on an opted-in connection. Human prose stays on stderr. (`serve.rs` module header.)
- **The CLI's bytes do not move.** Every np-core entry point reached from the CLI passes `progress::none()`; `crates/np-cli/tests/cli.rs` (47 cases) and `crates/np-core/tests/*` must stay green untouched.
- **np-core does not depend on `serde_json`** (dev-dependency only). The event type crossing the seam is plain Rust; phase→key mapping and every state key live in np-cli.
- **States stay states.** No new JSON-RPC error codes. A degraded success is a successful result *and* a `completed` notification carrying that state.
- **Absent ≠ null.** An absent key means something (no id, no total). Never write `"x": null` to mean "unknown".
- **§12 rule 8:** no host and no URL in any message, log or test output — assert `!text.contains("http")` on every notification the tests capture.
- **§12 rule 4:** a fetch marker is reported, never deleted, never expired.
- **Never regenerate catalog goldens.** Any diff under `tests/golden*` is a stop-and-report.
- **Gate by exit code:** `.github/scripts/gates.sh <log-dir>` (20 rows) must be run at the end with `CMAKE=/home/agent/tools/cmake/bin/cmake`, `CARGO_BUILD_JOBS=4`, `NP_NO_AUTO_FETCH=1` unset for artifact rows; `NOT COVERAGE` lines are counted, not ignored.
- **Terminal vocabulary:** `completed` when a response *result* was produced (whatever it says), `refused` when an error object was produced. No `ready`/`stopped`.

## Review Focus

The spec's tests cover the happy protocol; these five inputs are where a client author gets hurt, and each is pinned by a named test below.

1. **A client that never opts in**, while a 309 MB fetch runs → exactly one line per request, same documents as today (Task 5).
2. **A two-member fetch** (text encoder + projector) → `bytes_done` restarts at the second member and `members_done` advances; a GUI computing a percentage must never see a negative or a >100 % (Task 2).
3. **`status.get` naming a directory that is not a workspace** → an empty `fetch_markers`, no fault invented, and **no database file created** (Task 7).
4. **The client vanishes mid-write** (stdout pipe closed) → the request finishes, np keeps serving or exits cleanly; no panic, no partial line (Task 4).
5. **A command sent as a JSON-RPC notification** (no id) that triggers a fetch → progress lines carry no `request` key at all rather than a fabricated sequence number (Task 6).

## File Structure

| File | Responsibility |
| --- | --- |
| Create `crates/np-core/src/progress.rs` | The vocabulary of phases and the `Progress` seam. No JSON, no I/O, no clock |
| Modify `crates/np-core/src/lib.rs` | `pub mod progress;` |
| Modify `crates/np-core/src/fetch.rs` | Report per-member byte checkpoints where `bytes_done` is already computed; add `fetch_with_progress`; keep `fetch` / `fetch_with_table` as wrappers so the ~15 existing call sites do not move |
| Modify `crates/np-core/src/embed_run.rs` | Report chunk-progress at the point `between_batches` fires today; add `embed_with_inject`'s progress parameter, `embed` delegates with `none()` |
| Modify `crates/np-core/src/models.rs` | `list_fetch_markers(models_dir) -> Vec<FetchMarkerInfo>` — a directory read reusing the existing marker parser, skipping `.tmp` files |
| Create `crates/np-cli/src/serve_channel.rs` | `LineSink`, `Clock`, `Channel` (stdout ownership, opt-in flag, `last` slot, throttle) and the notification builder |
| Modify `crates/np-cli/src/serve.rs` | Use `Channel`; add `initialize` negotiation, terminal notifications, `status.get`; `handle_line(line, &mut Channel)` |
| Modify `crates/np-cli/src/main.rs` | Thread `&dyn Progress` through `embed_report`, `search_run` and the fetch call sites they reach; CLI callers pass `progress::none()` |
| Modify `crates/np-cli/tests/serve.rs` | In-process loop tests + real-pipe tests, including a localhost test origin |
| Create `crates/np-core/tests/progress.rs` | Seam-level tests: fetch sequence, two-member restart, embed counters |
| Create `docs/reference-serve.md` | The interface reference the operator asked for (Task 9) |
| Modify `README.md` | Serve section shrinks and links to the reference (Task 9) |

---

### Task 1: The progress seam in np-core

**Files:** Create `crates/np-core/src/progress.rs`; Modify `crates/np-core/src/lib.rs`; Test: unit tests in `progress.rs`

**Interfaces:**
- Produces:
  ```rust
  pub enum Phase { Locking, Resolving, Indexing, Fetching, Embedding, Writing }   // Copy, Clone, Eq, Debug
  pub struct ProgressEvent<'a> {           // Default gives zeros/None/Absent-ish defaults
      pub phase: Phase,
      pub member: Option<&'a str>,         // "model" | "mmproj" | None
      pub bytes_done: u64,
      pub bytes_total: Option<u64>,        // pinned size, not transport-announced
      pub members_done: usize,
      pub members_total: usize,
      pub chunks_done: usize,
      pub embedded: usize,
  }
  pub trait Progress: Sync { fn report(&self, event: &ProgressEvent<'_>); }
  pub fn none() -> &'static dyn Progress;  // shared no-op every CLI path passes
  ```
- Consumes: nothing.

- [ ] **Step 1: Write the failing test** — in `crates/np-core/src/progress.rs` `mod tests`: build a `ProgressEvent` with only `phase` and `bytes_done` set, assert `Default::default()` gives `bytes_total == None`, `member == None`, and that `none().report(&event)` returns without panicking.
- [ ] **Step 2: Run it, confirm failure** — `cargo test -p np-core --lib progress` → compile error "unresolved module `progress`".
- [ ] **Step 3: Implement** the types above and add `pub mod progress;` to `lib.rs`. Derive `Debug, Clone, Copy, PartialEq, Eq` on `Phase`; `Default` on `ProgressEvent`. Document on the trait: *must not block, must not panic* — np-core calls it inline inside the transfer loop.
  The spec writes the no-op as `Progress::NONE`; a `&dyn` value cannot be an associated const, so it ships as the free function `progress::none()`, and the spec is amended to match (Task 10 Step 4).
- [ ] **Step 4: Run** `cargo test -p np-core --lib progress` → PASS; then `cargo clippy -p np-core --all-targets -- -D warnings` → clean (no `clippy::needless_pass_by_value` regressions).
- [ ] **Step 5: Commit** `git add crates/np-core/src/progress.rs crates/np-core/src/lib.rs && git commit -m "feat(core): a progress seam with no JSON and no clock"`.

### Task 2: Fetch reports per-member byte progress

**Files:** Modify `crates/np-core/src/fetch.rs` (`fetch`, `fetch_with_table`, `fetch_attempt`, `receive_member`, marker write near `MARKER_INTERVAL_BYTES`); Test `crates/np-core/tests/progress.rs`

**Interfaces:**
- Consumes: Task 1's `Progress`, `ProgressEvent`, `Phase::Fetching`, `none()`.
- Produces: `pub fn fetch_with_progress<T: MemberPin>(db, models_dir, id, requested, transport, now, plan, progress: &dyn Progress) -> FetchOutcome` with the same parameter order as `fetch_with_table` plus `progress` last; `fetch` and `fetch_with_table` become wrappers passing `none()`.

- [ ] **Step 1: Write the failing tests** in `crates/np-core/tests/progress.rs`, reusing the fake `Transport` pattern from `tests/fetch.rs`:
  - `a_single_member_fetch_reports_monotonic_byte_checkpoints` — collect events in a `Vec<ProgressEvent>` (test-local recorder); assert first event has `member == Some("model")` and `bytes_done == 0`, every later `bytes_done` strictly greater, the last equals the pinned member size, and every event has `phase == Phase::Fetching` with `bytes_total == Some(size)`.
  - `a_two_member_fetch_restarts_bytes_at_the_second_member` (Review Focus 2) — assert the sequence contains `member "model"` reaching its size, then a `"model"`→`"mmproj"` event whose `bytes_done` is smaller than the previous event's, with `members_done` advanced. Assert no event ever has `bytes_done > bytes_total.unwrap()`.
- [ ] **Step 2: Run** `cargo test -p np-core --test progress` → FAIL (`fetch_with_progress` unresolved).
- [ ] **Step 3: Implement.** Add the `progress` parameter to `fetch_attempt` and thread it into `receive_member`; report at member start (before the first read), at each existing marker checkpoint (same `next_marker` branch — no new accounting), and once after a member's digest check passes with `bytes_done == size`. `members_total` is `requested.len()`, `members_done` counts members completed before this one.
- [ ] **Step 4: Run** `cargo test -p np-core --test progress` → PASS, then `cargo test -p np-core --test fetch` → the existing suite still passes unchanged (wrappers kept the old signatures).
- [ ] **Step 5: Commit** `"feat(core): fetch reports per-member byte checkpoints"`.

### Task 3: Embed reports chunk progress; the CLI stays silent

**Files:** Modify `crates/np-core/src/embed_run.rs` (`embed_with_resolution` at :689 — the entry np-cli calls at `main.rs:725` — plus `embed`, `embed_with_inject`, `run`, near the `between_batches(&db_path)` call at ~1044); Test: extend `crates/np-core/tests/progress.rs`

**Interfaces:**
- Consumes: Task 1.
- Produces: `embed_with_resolution(workspace, cfg, resolution, progress: &dyn Progress)` (np-cli's entry) and `embed_with_inject(workspace, cfg, provider, between_batches, progress: &dyn Progress)`; `embed(...)` keeps its current signature and delegates with `none()`.

- [ ] **Step 1: Write the failing test** — with a stub `Embedder` (the pattern in `tests/embed_run.rs`) over a vault of ~40 notes: assert at least one `Phase::Embedding` event, `chunks_done` non-decreasing across events, the final `embedded` equals the report's `embedded`, and that no event has `phase == Phase::Writing` unless a batch was actually written.
- [ ] **Step 2: Run** `cargo test -p np-core --test progress` → FAIL (arity).
- [ ] **Step 3: Implement.** Report once when the chunk pass begins (`chunks_done: 0`) and again wherever `between_batches` fires, carrying running `seen`/`embedded`. Do **not** add a count query for `chunks_total`: this task ships no total (spec §"Left to the builder" — omit rather than cost every embed).
- [ ] **Step 4: Verify nothing moved** — `cargo test -p np-core` and `cargo test -p np-cli --test cli` → all pass with the same counts as before this branch (`cargo test -p np-cli --test cli 2>&1 | tail -3`, record the number in the commit message).
- [ ] **Step 5: Commit** `"feat(core): embed reports chunk progress; CLI passes the no-op reporter"`.

### Task 4: `Channel` — one stdout owner, an injectable clock, and a testable loop

**Files:** Create `crates/np-cli/src/serve_channel.rs`; Modify `crates/np-cli/src/serve.rs` (`serve`, `handle_line`), `crates/np-cli/src/main.rs` (module decl); Test: unit tests in `serve_channel.rs`, plus existing `tests/serve.rs` unchanged

**Interfaces:**
- Produces:
  ```rust
  pub trait LineSink { fn line(&mut self, text: &str); }          // impls: StdoutLock<'static>, Vec<u8>
  pub trait Clock: Sync { fn since_start(&self) -> Duration; }    // SystemClock(Instant), FakeClock in tests
  pub struct Channel<'a> { sink: &'a mut dyn LineSink, clock: &'a dyn Clock,
                           notifications: bool, last: Option<Value>,
                           emitted_phase: Option<Phase>, emitted_at: Duration }
  impl Channel<'_> {
      pub fn response(&mut self, value: &Value);                                    // one line + flush
      pub fn notify(&mut self, params: &Value);                                     // notification envelope
      pub fn want_progress(&self) -> bool;
      pub fn set_last(&mut self, v: Value);  pub fn last(&self) -> Option<&Value>;
  }
  pub(crate) fn serve_loop(input: impl BufRead, ch: &mut Channel<'_>) -> io::Result<()>;
  ```
- Consumes: `serve::handle_line` (signature becomes `fn handle_line(line: &str, ch: &mut Channel<'_>) -> Option<Value>`).

- [ ] **Step 1: Write the failing test** — in `serve_channel.rs`: `a_buffer_sink_receives_whole_lines` pushes two values through `response()` and asserts the `Vec<u8>` holds exactly two newline-terminated lines and that `serde_json::from_str` parses each (Review Focus 4's framing half).
- [ ] **Step 2: Run** `cargo test -p np-cli --lib serve_channel` → FAIL.
- [ ] **Step 3: Implement** the types; rewrite `serve()` as: build a `Channel` over `io::stdout().lock()` and `SystemClock`, call `serve_loop(io::stdin().lock(), &mut ch)`. Move the read loop into `serve_loop`, keeping today's behaviours exactly (blank line ignored, unparseable → `-32700` with `id: null`, notification → no response, `catch_unwind` around dispatch, EOF → `Ok(())`). `handle_line` takes `&mut Channel`; existing unit tests in `serve.rs` are updated to pass a `Channel` over a `Vec<u8>` sink and a `FakeClock::default()` — **their assertions must not change**.
- [ ] **Step 4: Run** `cargo test -p np-cli --lib serve` → all previously passing cases pass; `cargo test -p np-cli --test serve` (real child pipe) → unchanged PASS. Add one loop test: `feed("…initialize…")` through `serve_loop` into a buffer and assert one line out.
- [ ] **Step 5: Run the broken-pipe case** (Review Focus 4): a unit test where the sink panics-free counts writes and returns an error flag (`FailingSink`), drive a request through `serve_loop`, assert `serve_loop` returns `Ok` and the recorder saw zero lines — no panic, no partial line. **Commit** `"refactor(serve): one stdout owner behind a Channel, with an injectable clock"`.

### Task 5: Opt-in negotiation and throttled notifications

**Files:** Modify `crates/np-cli/src/serve.rs` (`initialize`, `dispatch`, handlers), `serve_channel.rs` (throttle + notification builder); Test: `crates/np-cli/tests/serve.rs`, unit tests in `serve_channel.rs`

**Interfaces:**
- Consumes: Task 1 `Phase`, Task 4 `Channel`.
- Produces: `fn initialize(params: &Value) -> Value` now echoes `"progress": true` and, when asked, records the opt-in; `pub fn progress_params(request: &Value, command: &str, phase: Phase, counters: &ProgressEvent<'_>, since: Duration) -> Value` (np-cli side of the seam: snake_case keys exactly as spec §2/§3).

- [ ] **Step 1: Write the failing tests**
  - `opting_in_at_initialize_turns_progress_on` — after `{"method":"initialize","params":{"notifications":true},"id":1}`, the result has `"progress": true`.
  - `a_client_that_never_opts_in_sees_one_line_per_request` (Review Focus 1) — with a workspace whose fetch will run, send initialize + scan + embed and assert exactly three lines, and that each of `scan`/`embed` results equals the same document an opted-in session produces.
  - `throttle_allows_one_line_per_phase_change_and_one_per_second` — `FakeClock`: five reports inside one second in the same phase → one line; a phase change at +50 ms → a second line. **Then mutate**: comment out the throttle comparison, re-run, confirm this test fails; restore.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement.** `initialize` reads `params.notifications` (default false) into `Channel`. Add the adapter in `serve_channel.rs`:
  `struct ServeProgress<'a> { ch: &'a mut Channel<'_>, request: Option<Value>, command: &'static str }`, implementing `Progress` by mapping `Phase` to the spec's key names and calling `Channel::notify`. Thread it through np-cli in this order: `fetch_pinned_missing(..., progress: &dyn Progress)` (`main.rs:1971`) calls `fetch::fetch_with_progress`; `embed_report(workspace, auto_fetch, announce, progress)` (`main.rs:665`) passes it to `embed_with_resolution` and to `fetch_pinned_missing`; serve handlers build the adapter, **every CLI call site passes `progress::none()`**. A request with no id gets an adapter that omits `request` entirely (Review Focus 5's construction). Throttle lives in `Channel::notify`: immediate on phase change, otherwise ≥1 s since the last line of that phase.
- [ ] **Step 4: Run** → PASS; then `cargo test -p np-cli --test serve` and confirm the opted-out assertions still hold byte-for-byte against the pre-feature expectations.
- [ ] **Step 5: Commit** `"feat(serve): opt-in np/progress notifications with a phase-change-first throttle"`.

### Task 6: Terminal notifications, states as data

**Files:** Modify `crates/np-cli/src/serve.rs` (post-response hook in `handle_line`); Test `crates/np-cli/tests/serve.rs`

**Interfaces:**
- Consumes: `Channel::set_last`, the response `Value`.
- Produces: terminal params builder `fn terminal_params(request: &Value, command: &str, response: &Result<Value, Fault>, since: Duration) -> Value` → `{"request"?, "command", "phase": "completed"|"refused", "since_ms", …state keys copied from the result when present: locked, offline, reason, vectors, outcome}`.

- [ ] **Step 1: Write the failing tests**
  - `a_contended_embed_ends_completed_with_locked_true` — hold `WorkspaceLock` as the existing contention test does; assert the response still equals `{command, locked: true, waited_ms}` and the last line is `phase: "completed"` carrying `"locked": true`, and that a following `search` still answers.
  - `an_offline_embed_ends_completed_with_offline_true` — run with `NP_NO_AUTO_FETCH=1` unset and `NP_MODELS_BASE_URL` pointed at a closed localhost port; assert response carries today's offline state and the terminal line says `completed`, not `refused`.
  - `an_unknown_method_ends_refused_with_the_code` — `phase: "refused"`, `"code": -32601`.
  - `a_notification_command_produces_progress_without_a_request_key` (Review Focus 5) — send `scan` with no `id`; assert any progress line present has **no** `request` member (`!params.contains_key("request")`) and the response count stays zero.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement.** After `dispatch` returns, build terminal params from the *response kind* (result → `completed`, error → `refused`) and copy through only the state keys the result actually carries; store `{request, command, phase, outcome?, elapsed_ms}` in `Channel::set_last`.
- [ ] **Step 4: Run** → PASS. Assert across every new test that no captured line contains `http` (**§12 rule 8**).
- [ ] **Step 5: Commit** `"feat(serve): terminal notifications mirror the response kind, states as data"`.

### Task 7: `status.get`, reading markers without touching the database

**Files:** Modify `crates/np-core/src/models.rs` (new `list_fetch_markers`), `crates/np-cli/src/serve.rs` (`METHODS`, `dispatch`, new `status` handler); Test `crates/np-core/tests/models.rs` + `crates/np-cli/tests/serve.rs`

**Interfaces:**
- Consumes: `Config::workspace_models_dir`, existing marker consts `FETCH_MARKER_PREFIX` / `FETCH_MARKER_SUFFIX` and the parser behind `fetch_state`.
- Produces:
  ```rust
  pub enum MarkerState { Running { bytes_done: u64, bytes_total: Option<u64>, started_at: i64 }, Unreadable }
  pub struct FetchMarkerInfo { pub id: String, pub state: MarkerState }
  pub fn list_fetch_markers(models_dir: &Path) -> Vec<FetchMarkerInfo>;   // directory read; skips *.tmp
  ```
  and the method `status.get` → `{"last": <value|null>, "fetch_markers": [ … ], "running": null}`.

- [ ] **Step 1: Write the failing core tests** — write two marker files (`.fetching.alpha.json`, `.fetching.beta.json`) plus a temp file (`.fetching.gamma.json.tmp7`) and one corrupt marker into a temp dir; assert `list_fetch_markers` returns alpha and beta with their fields, marks the corrupt one `Unreadable`, ignores the `.tmp`, and that the directory still contains every file afterwards (**§12 rule 4**: reported, never deleted).
- [ ] **Step 2: Run** `cargo test -p np-core --test models list_fetch_markers` → FAIL.
- [ ] **Step 3: Implement** `list_fetch_markers`, reusing the marker parser rather than re-implementing it.
- [ ] **Step 4: Write the failing serve test** — `status.get` on a fresh workspace returns `last: null`, `fetch_markers: []`, `running: null`; after hand-writing a marker it reports it verbatim; and (Review Focus 3) **`status.get` against a path that is not a directory creates no `.sqlite` file** — snapshot the data dir before and after and compare names. Add `"status.get"` to `METHODS`.
- [ ] **Step 5: Run** → PASS; `cargo test -p np-cli --test serve` fully green; assert no response contains `http`.
- [ ] **Step 6: Commit** `"feat(serve): status.get reports the last request and recorded fetch markers"`.

### Task 8: Search coverage (O2), proven over a real pipe

**Files:** Modify `crates/np-cli/src/main.rs` (`search_run` at :1045 and its provider-resolution branch near :1110); Test `crates/np-cli/tests/serve.rs`

**Interfaces:**
- Consumes: Task 5's `ServeProgress` adapter and the already-threaded `fetch_pinned_missing`.
- Produces: `search_run(workspace, query, limit, mode, announce, progress: &dyn Progress) -> Result<SearchRun, Box<dyn Error>>`; CLI call sites pass `progress::none()`. The serve `scan` handler emits `Phase::Indexing` bookended lines around its work (spec §9: no np-core change).

- [ ] **Step 1: Write the failing test** — in `tests/serve.rs`, gated on `cfg!(feature = "fetch")` and printing `NOT COVERAGE: no fetch feature` when absent: start a `TcpListener` on an ephemeral localhost port that serves a body of the pinned member length, point `NP_MODELS_BASE_URL` at it (the URL appears only in the child's environment — never assert or print it), opt in, then run `embed` and collect lines. Assert the sequence contains `phase: "fetching"` with `"member": "model"`, that `bytes_done` values are non-decreasing within that member, and that a terminal line arrives last. Then close the listener, run `search` on a second vault and assert `resolving` appears and the terminal line is `completed` with the offline state — proving O2's stall is narrated.
- [ ] **Step 2: Run** `cargo test -p np-cli --features local-model,fetch --test serve` → FAIL.
- [ ] **Step 3: Implement.** Add `progress` to `search_run`; report `Phase::Resolving` when provider/artifact resolution starts, and pass the reporter into `fetch_pinned_missing` when hybrid mode decides to fetch. Keep `announce` semantics untouched, so CLI stderr and exit codes stay byte-identical.
- [ ] **Step 4: Run** → PASS with the artifact-gated rows recorded; then `cargo test -p np-cli --test cli` again to confirm CLI bytes unchanged.
- [ ] **Step 5: Commit** `"feat(serve): narrate search's auto-fetch and scan bookends (O2)"`.

### Task 9: The interface reference the operator asked for

**Files:** Create `docs/reference-serve.md`; Modify `README.md`; Test: new doc-sync test in `crates/np-cli/tests/serve.rs`

**Interfaces:**
- Consumes: shipped behaviour from Tasks 1–8.
- Produces: documentation only.

- [ ] **Step 1: Capture real transcripts first** — run the packaged-or-`target/release` binary against a scratch vault with the localhost origin from Task 8 and save the exact stdout of (a) an opted-out session, (b) an opted-in first-run embed, (c) `status.get` with a marker present. These blocks go into the doc verbatim; nothing typed by hand.
- [ ] **Step 2: Write `docs/reference-serve.md`** — transport and framing; every method in `METHODS` with params, result document, which CLI command builds it, and its refusals; the states-as-data table (`locked`, `offline`, `reason`, `prefix_drift`, `vectors`); fault codes `-32700/-32600/-32601/-32602/-32000/-32603`; the notification section with the phase table and per-member counters; `status.get` including what it cannot see (`running` is always `null`, live arrives as a notification); and the client walkthrough — spawn, `initialize` with `notifications: true`, one request at a time, EOF to stop, `snippet` may contain newlines, hits carry no `title`, **and the reload recipe**: the spawning process keeps the last notification it saw and re-serves it after a page reload.
- [ ] **Step 3: Write the failing doc-sync test** — `the_reference_documents_every_method`: read `docs/reference-serve.md` from the repo root (resolve via `CARGO_MANIFEST_DIR/../..`) and assert every name in `METHODS` appears in it, and that it contains the strings `"np/progress"` and `"notifications"`.
- [ ] **Step 4: Run** `cargo test -p np-cli --test serve the_reference_documents_every_method` → FAIL, then finish the doc until it PASSes. Shrink README's *Use `np` from your app* to transcript + three rules + a link; keep its existing claims true (no dead anchors — re-run the link check used for the README rewrite).
- [ ] **Step 5: Commit** `"docs: reference-serve.md — the whole np serve interface, with captured transcripts"`.

### Task 10: Full gates, then push

- [ ] **Step 1:** `PATH="$HOME/.cargo/bin:$PATH" TMPDIR=/home/agent/tmp CARGO_BUILD_JOBS=4 CMAKE=/home/agent/tools/cmake/bin/cmake NP_LLAMA_CPP_SRC=/home/agent/note-processor/.vendor/llama.cpp-v0.8.0 .github/scripts/gates.sh /home/agent/tmp/gates-progress` → expect `fail=0`, 20 rows, and read the `NOT COVERAGE` count: it must not rise versus `gates-v030` except for rows that legitimately cannot run without weights.
- [ ] **Step 2:** Artifact-backed rows with weights: `NP_TEST_MODEL_PATH=/home/agent/models/embeddinggemma-2-Q8_0.gguf NP_TEST_MMPROJ_PATH=/home/agent/models/mmproj-embeddinggemma-2-Q8_0.gguf` for `test-core-vision` / `test-cli-realmodel`, unsetting `NP_NO_AUTO_FETCH`.
- [ ] **Step 3:** Re-run the three rustdoc rows with their exact invocations (`RUSTDOCFLAGS="--deny warnings" cargo doc --workspace --no-deps` per feature row) — a green from a command I invented is not evidence.
- [ ] **Step 4:** Push `main`; append an as-built section to the spec recording anything the implementation contradicted, including whether `chunks_total` shipped and why.

## Self-review notes

Spec coverage: G1 → Tasks 2, 3, 5, 8; G2/G3 → Task 7; G4 → Tasks 4, 5 (framing guard); G5 → Task 9; §2's phase table → Tasks 2, 3, 5, 6, 8 (`writing` deliberately not emitted for scan — spec §"Left to the builder", recorded in Task 10 Step 4); §3 per-member → Task 2; §4 throttle → Task 5 with a mutation check; §5 `running: null` → Task 7; §6 lock interaction → Task 6; §8 compatibility wording → Tasks 4–5; §9 bookended scan → Task 8 Step 3; §10 search → Task 8. No spec requirement is unassigned.
