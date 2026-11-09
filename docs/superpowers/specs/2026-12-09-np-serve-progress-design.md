# Design: progress reporting from `np serve` (`np/progress`, `status.get`)

Date 2026-12-09 · Status: both product decisions resolved by the operator (O1 push-only, O2 search included); this document now awaits approval as written
Owner: controller session
Builds on: `docs/superpowers/specs/2026-12-08-np-serve-stdio-design.md` (the F-series rules it inherits are under Constraints)

## Intent, and why

A frontend that drives `np serve` sends a request and hears nothing until the work is finished. On a fresh machine that silence is long: `embed` may
fetch the pinned ~309 MB text encoder and then encode every chunk in the vault — 30.7 s for the transfer alone on a warm connection, minutes on a cold
one. The operator's deployment downloads the model automatically on first start, so *the first thing a GUI shows its user is a multi-minute unanswered
call*, with no honest way to say "downloading", "embedding 118 of 412", or "the network never answered".

The ask, in the operator's words: *"send status reports to the frontend what the backend is currently doing."*

What that buys is not a progress bar for its own sake: it is the difference between a screen that lies (spinner, then either a result or nothing) and
one that can say *what it is waiting on* — including after the user reloads, and after np dies halfway through a transfer.

## Decisions the operator made (2026-12-09)

**O1 — push-only; no reader thread.** A GUI learns what is happening from `np/progress` notifications, and `status.get` reports what finished plus what
is recorded on disk. The consequence, stated where a client author will read it: while a request runs, np is not reading stdin (one line in, run to
completion, next line — `serve.rs:106`) and there is one client on the pipe, so there is no live query to ask; `running` stays `null` and the reference
document says why rather than letting the field look broken. The reload case belongs to the client that spawned np: it keeps the last status it received
and hands it back after a page reload — one paragraph in the walkthrough, not a threading refactor. "No concurrency" stays a non-goal.

**O2 — `search` is in scope.** A default-mode search performs the same auto-fetch as embed (`main.rs:1110`: `let fetch_allowed = hybrid &&
AutoFetch::Deferred.allowed(&cfg)`), so the first call a GUI makes can be the 310 MB one. Narrating it is part of the feature; §10 says what that costs.

## Goals

- **G1 — Live status while a request runs**, as structured notifications naming the phase and the counters behind it, for `scan`, `embed` and `search`.
- **G2 — Catch-up.** A client can learn what just finished, and what fetch is recorded on disk, without having listened from the start. It cannot ask
  what is running *right now* (O1): that arrives as a notification, or not at all.
- **G3 — Truth across process death.** If np is killed during a fetch, a new connection still reports "a fetch of `embeddinggemma-2` is recorded at N
  bytes done" from the state that already exists on disk — not a second record.
- **G4 — Nothing breaks for existing clients**: same framing, same documents, opt-in extras (restated precisely in §1 after the builder check showed
  "byte-identical" could not hold).
- **G5 — One documented interface.** `docs/reference-serve.md` describes all of `np serve` well enough to build a frontend from, with real transcripts.
  *(Operator: "do not forget to write a docu how the interface is working!")*

## Non-goals

- **No cancellation.** A `cancel` method (a GUI "stop" button) is its own design: aborting mid-write must decide what a half-written batch means.
- **No per-file granularity** for scan, and no per-image granularity for media: phases and throttled counters (§3), not thousands of lines.
- **No HTTP/SSE/WebSocket transport.** M4 (`docs/roadmap-2026-10-27.md`), its own design; nothing here pre-implements it.
- **No telemetry.** np stays silent unless a human or the connected client asks.

## Anti-goals

- **A percentage that was not earned.** For a fetch the denominator *is* known — the pinned artifact's size is compiled in — so a fetch percentage is
  honest and will be reported. What is *not* known is an embed's chunk total (§ "Left to the builder"), so no `chunks_total` ships unless it is cheap;
  absent means unknown, which is a state, not an error.
- **A second source of truth for the download.** The fetch marker (`<models-dir>/<id>.fetching`) already records `nonce`, `started_at`, `bytes_done`,
  `bytes_total` and is crash-safe by design (**§12 rule 4**: liveness is consent, never a clock). Duplicating it would create two records that can
  disagree — the defect class this repository has been carding since F32.
- **Progress as prose.** Every field is data; no client may parse a sentence to learn a phase (the rule behind `locked`, `offline`, `reason`,
  `prefix_drift`).
- **An origin on stdout.** **§12 rule 8**: no host, no URL, in any message including progress. Tests assert `!text.contains("http")`.

## Constraints inherited from the existing design

1. **stdout is the protocol** (`serve.rs` header). Progress lines are permitted only after opt-in; human sentences stay on stderr and the CLI's bytes
   do not move (tests pin them).
2. **Results are the CLI's documents**, built by one function per state. Progress is *additional* output, never a modification: when a request finishes
   the client gets the identical document `np <verb> --json` prints.
3. **States stay states.** A refusal or degraded answer remains a successful response whose result says so; progress adds no JSON-RPC error codes.
4. **A panic costs one response, not the session** (`catch_unwind` around `dispatch`). Emitting must not change that, and a failed write (client gone)
   must not become a panic.
5. **No session state**: every request names its workspace. Progress is attributed by request id (§ "Requests with no id").
6. **np-core does not depend on serde_json** (`crates/np-core/Cargo.toml`: dev-only). The event type crossing the seam is plain Rust; phase→JSON key
   mapping lives in np-cli. *(Found by the builder check; it rules out the "just pass a `Value`" shape.)*

## Approaches considered

**A. Push notifications + `status.get` (chosen).** During a request, np writes unsolicited JSON-RPC notifications to stdout; `status.get` reports what
is recorded. Trades: clients must tolerate non-response lines, and framing/ordering needs pinning by test. This is what makes G1/G2 possible.

**B. Poll only.** Zero new framing, trivially compatible — but it answers "what are you doing" only as often as the client guesses, and phase changes
arrive late. Rejected for G1; remains usable as a *degenerate* mode (a client may ignore notifications and poll).

**C. Progress on stderr.** Free, and useless here: it would make the frontend parse English — what F32–F45 exist to prevent. Rejected.

The operator delegated the choice ("I would follow your recommendation"), so A is picked for G1/G2. Everything below marked *my call* is mine, not
theirs.

## The how, decided

### 1. Opt-in, negotiated in `initialize`; protocol stays 1

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"notifications":true}}
{"jsonrpc":"2.0","id":1,"result":{"np":"0.3.1","protocol":1,"progress":true,"methods":["initialize","search","scan","embed","models.list","status.get","shutdown"], …}}
```

- `params.notifications` defaults to **false**; omitting it reproduces today's behaviour for every method (§ "Compatibility, stated precisely").
- *My call:* keep `protocol: 1`. Added methods and an optional parameter are compatible changes; `PROTOCOL` is for the incompatible ones. Support is
  advertised by `"progress": true`, so a client that asks an older server gets no key and can fall back to polling without a version check.
- The flag is per connection, read at `initialize`. It is the only session-level fact in the server; requests still name their own workspace.

### 2. The notification line

```json
{"jsonrpc":"2.0","method":"np/progress","params":{"request":7,"command":"embed","phase":"fetching","member":"model","bytes_done":67108864,"bytes_total":309855456,"since_ms":4120}}
{"jsonrpc":"2.0","method":"np/progress","params":{"request":7,"command":"embed","phase":"embedding","chunks_done":118,"since_ms":51340}}
{"jsonrpc":"2.0","method":"np/progress","params":{"request":7,"command":"embed","phase":"completed","outcome":"nothing owed","offline":false,"since_ms":96210}}
```

A JSON-RPC **notification**: no `id` member at all (absent ≠ null — the same reasoning as the eleven-key hit rule). Always present: `request`,
`command`, `phase`, `since_ms` (ms since that request started, monotonic source `std::time::Instant`, so an NTP correction cannot make progress go
backwards — *my call*).

| `phase` | Emitted by | Counters |
| --- | --- | --- |
| `locking` | mutating paths, before the F44 write lock is taken | — (its absence says contention has not begun) |
| `indexing` | scan: one line at entry, one at exit with final counts (**bookended**, see §9) | `files_seen`, `notes_seen` on the exit line |
| `fetching` | fetch's byte loop (`fetch.rs`, beside the 8 MiB marker write) | `member`, `members_done`, `members_total`, `bytes_done`, `bytes_total` |
| `embedding` | embed_run's chunk loop | `chunks_done`, `embedded`; `chunks_total` only if cheap |
| `writing` | the closing batch write / catalog rewrite | — |
| `completed` | the request produced a **result** | `outcome` (the report's own word) plus any state keys that result carries (`offline`, `locked`, `reason`, `vectors`) |
| `refused` | the request produced an **error object** | `code`, and `reason` where the CLI already has a token |

**Terminal rule, one line:** *the last notification mirrors the response kind.* A degraded success (`{locked:true}`, `offline:true`, `fetch_declined`,
embed arms (i)/(iii)/(iv), `RuntimeNotBuilt`) is **`completed`** carrying those states as data — because that is what the client received; only a real
JSON-RPC error (`-32601`, `-32602`, `-32000`, `-32603`) is `refused`. This replaces the draft's `ready`/`stopped`, whose names implied success and which
the builder check correctly called a contradiction with constraint 3.

**Requests with no id.** A notification-shaped command still runs (`a_notification_runs_and_answers_nothing`). Its progress lines omit `request`
entirely rather than inventing a sequence number — absent means "no id was given", consistent everywhere, and the test asserts every line's `request`
is either absent or an id that was actually sent. *My call.*

### 3. Counters are per member, and monotonic within one

Measured from code: `fetch_attempt` writes the marker at each member's start with `bytes_done: 0`, and `receive_member` rewrites it every
`MARKER_INTERVAL_BYTES` (8 MiB) with **that member's** `size_bytes()`. So a two-member fetch (text encoder + projector) resets `bytes_done`. The draft
asserted "never decreases" and would have failed on day one.

- Counters are therefore **per member**, labelled by `member` (`"model"` / `"mmproj"`), with `members_done` / `members_total` for the whole call.
  Monotonicity is asserted per `(request, member)`, and a new member restarts bytes — documented as expected, not a bug.
- `bytes_total` is the **pinned** size (always known for a fetchable id). The transport-announced `Body::bytes_total: Option` (**§12 rule 12**) is not
  reported in progress lines: two denominators in one field name is how a GUI renders two different percentages from the same key.

### 4. Throttling, so status does not become the traffic

- One line per phase change, immediately; within a phase at most one line per second and never more than one per checkpoint interval.
- A first-run download is ~40 lines over its life (310 MB ÷ 8 MiB); a 4 000-note scan is under ten.
- `since_ms` never decreases; `bytes_done` / `chunks_done` never decrease within their member.

### 5. Status: what is recorded, and why "what is running" is not in it

- The server keeps one slot: the last finished activity `{request, command, phase, outcome, elapsed_ms}`. Requests run inline, so there is exactly zero
  or one activity — no map, no contention, nothing to leak.
- `status.get` params `{ "workspace": "…" }`. Result keys always present:

  ```json
  {"last": {"request":7,"command":"embed","phase":"completed","outcome":"nothing owed","elapsed_ms":96210},
   "fetch_markers": [{"id":"embeddinggemma-2","state":"in-progress","bytes_done":268435456,"bytes_total":309855456,"started_at":1793893791}],
   "running": null}
  ```

- **`fetch_markers` is a list**, because one workspace's models directory can hold markers for several ids, and it reuses the existing read path
  (`fetch_state`, a directory read) rather than `models::list`, which opens the workspace database — an unacceptable side effect for a status call that
  may name a typo'd vault (the server already refuses to *create* anything: `a_workspace_that_is_not_a_directory_refuses_without_creating_anything`).
  Entries report what the marker says, including unreadable ones as `{"state":"unreadable"}`; np never deletes or expires a marker (**§12 rule 4**).
  `nonce` and the marker's path stay out of the contract: nothing a GUI renders.
- **`running` is always `null` in this build, and that is documented rather than hidden.** O1 chose push-only, so nothing can ask mid-run. The key ships
  anyway so the shape survives M4 adding a queue, and `docs/reference-serve.md` states plainly that live activity arrives as a notification — a client
  author must not discover a dead field by staring at it. *Operator's decision.*

### 6. Interaction with the F44 write lock

`locking` is emitted before `server_writer_lock`. A contended request answers exactly as today — a successful result carrying
`{"command":…,"locked":true,"waited_ms":2000}` — and its terminal line is `completed` with `locked: true` (per §2's rule). Progress never holds a lock
across requests; readers (`search`, `models.list`) take none — which makes them cheap to serve, not evidence that anything is listening while a writer
runs (O1).

### 7. Plumbing, and who owns stdout

- **One owner stays one owner.** `serve()` currently holds `io::stdout().lock()` for the loop's life while `handle_line()` is a pure function (which is
  why unit tests call it with no pipes). Introduce `Printer { out: StdoutLock<'static> }` with `response(..)` and `notify(..)`, created in `serve()` and
  passed as `Option<&mut Printer>` down through `handle_line` → `dispatch`. No second handle, and no reliance on std's reentrant stdout lock (it would
  work on this thread but reads like an accident waiting to interleave).
- **np-core stays ignorant of JSON and stdout** (constraint 6):

  ```rust
  // np_core::progress (new module)
  pub struct ProgressEvent { pub phase: Phase, pub member: Option<Member>, pub bytes_done: u64, … }
  pub trait Progress: Sync { fn report(&self, event: &ProgressEvent); }   // must not block, must not panic
  ```

  with a shared no-op (`progress::none()` — a `&dyn` value cannot be an associated const) used by every CLI path so **the CLI's bytes do not move** (the same no-change evidence that the serve extraction produced:
  47 `tests/cli.rs` cases green untouched). Because `report(&self)` cannot write through `&mut Printer`, the emitter wraps its handle in interior
  mutability (`RefCell`/`Mutex` — plan's call) and nothing else locks it.
- **Reuse the seams that exist** rather than widening seven signatures: `embed_run::embed_with_inject(..., between_batches: &mut dyn FnMut(&Path))`
  (`embed_run.rs:703`) and `scan_with_inject(..., after_walk)` (`scan.rs:444`) already exist; where a phase needs finer granularity than a seam gives,
  the builder adds one *inside* np-core and keeps the public signature stable. Fetch's hook goes where `bytes_done` is already computed for the marker —
  not new accounting.
- **Emission never fails a request.** A write error means the client left: counted and ignored, as the response path ignores it today; a panic inside a
  reporter is caught by the existing boundary, and the reporter does nothing that can panic (serialise to string, ignore errors).

### 8. Compatibility, stated precisely

Adding `status.get` to `METHODS` and `"progress"` to the initialize result changes **that one response's** bytes for every client, so G4 cannot mean
"byte-identical". It means: identical framing (one line per request unless opted in), identical documents for `search`, `scan`, `embed`, `models.list`,
and additive-only keys in `initialize`. The guard test asserts exactly that — the rest equal, `initialize` equal after removing the two new keys — so it
cannot be quietly written to assert nothing.

### 9. Scan is bookended, not live — *my call*

A solo scan of 4 000 notes measured **2.6 s** (F44's contention run), and `scan_inner` has no per-file callback today; adding one means surgery in the
crate's hottest loop for a phase that lasts seconds. So: one `indexing` line at entry, one at exit with `files_seen` / `notes_seen`. If a real vault
makes scan slow enough to matter, live counters become a card of their own with a measurement attached.

### 10. Search coverage, since O2 brought it in

`search` takes no write lock and normally finishes in milliseconds, so its ranking work needs no narration. What needs narrating is the part that can
stall: the auto-fetch that fires inside provider resolution when hybrid mode finds no artifact for the configured id.

- Phases from `search`: **`resolving`** (provider and artifact decision), then **`fetching`** with §3's per-member counters when a transfer really
  starts, then terminal `completed`. No `locking`, no `writing` — neither happens on this path.
- Plumbing: the same `&dyn Progress` seam reaches `search_run` and provider resolution. Search is read-only, so this adds no lock or lifetime
  interaction; it is one more caller of an interface that already exists.
- A search that fetches still writes nothing but the artifact cache (unchanged rule) and its result document is unchanged; progress is additive output on
  an opted-in connection. `reason: "artifact-fetched"` stays the machine-readable hint that `np embed` is next, so a client that ignores notifications
  loses nothing it needs.
- Terminal line for a search whose semantic half could not run: **`completed`** with `vectors: false` and its `reason` token — a degraded success under
  §2's rule, not a refusal.

## Left to the builder

- Counter key spellings (proposed above; stable once shipped — they are a contract).
- Whether `chunks_total` ships: prefer a cheap count on the chunk table; omit rather than add cost to every embed, and record which was chosen with the
  measured cost either way.
- The test seam for serve-level progress: an in-process entry (`serve_with(transport, clock, &dyn Progress)`) versus a feature-gated hook versus an
  env-var gate like `NP_TEST_MODEL_PATH`. Whichever is chosen must say which feature-matrix rows run the monotonicity and throttle tests, so they are
  real rows rather than `NOT COVERAGE` on most of them.
- Whether `writing` is distinct from `indexing` for scan: decide by what a GUI would render; do not add a phase nobody shows.
- Media/image progress: the counters there are images, not chunks (`embed_run.rs:1127`). If O2 brings search in, decide whether images get their own
  phase line at the same time.

## Documentation deliverable (operator-requested)

1. **`docs/reference-serve.md`** — the interface reference, so "the README is the contract" becomes a document one can build against: transport and
   framing; every method with its params, its result document and which CLI command builds it, and its refusals; the states-as-data table (`locked`,
   `offline`, `reason`, `prefix_drift`, `vectors`); fault codes; **the notification section** with §2's table plus a real transcript of a first-run
   embed (fetching → embedding → completed) captured from an actual run, not typed; `status.get` semantics including what it *cannot* see (O1's answer,
   spelled out, including that live activity arrives as a notification and not as an answer; and a "writing a client" walkthrough — spawn, `initialize`
   with `notifications:true`, one request at a time, EOF to stop, `snippet` may contain newlines, hits have no `title` — **plus the reload recipe**: the
   process that spawned np keeps the last notification it received and re-serves it to a fresh page, because np cannot answer "what are you doing now" to
   a client that reconnects mid-request.
2. **README**'s *Use `np` from your app* shrinks to transcript + three rules + a link (the contract moves out of it).
3. **This spec gains an as-built section** after implementation, recording anything the implementation contradicted — same shape as the serve spec's.

## Test plan (falsifiable, per this repository's rules)

- **Compatibility guard (§8)**: opted-out client gets one line per request; documents equal to today's except additive `initialize` keys.
- **Correlation**: with notifications on, every line's `request` is absent or an id that was sent; no notification appears after its request's response;
  every line is a JSON-RPC object with no `id` member.
- **Per-member monotonicity**: fake in-memory transport yielding fixed chunks → `bytes_done` increases within a member, the last fetch line for each
  member equals that pinned size, and a two-member run shows the documented restart at the second `member`.
- **Throttle**: injected clock — many reads inside one second produce one line; a phase change mid-second produces another. *Mutant check:* delete the
  throttle and this case must fail; if it survives, the assertion is decoration.
- **States unchanged**: hold `WorkspaceLock` as the existing contention test does → response still `{locked:true,…}` **and** terminal `completed` with
  `locked: true`; server keeps answering afterwards.
- **Offline fetch**: transport that never answers → result carries `offline` as today, terminal line is `completed` with `offline: true`, no new error
  code and no prose to parse.
- **Search coverage (O2)**: hybrid search against a workspace with no artifact, driven through the fake transport → `resolving`, then `fetching` with
  per-member counters, then `completed`; and a search whose semantic half cannot run → terminal `completed` with `vectors: false` and its `reason`. Both
  assert the *response document* is equal to today's, so progress stayed additive.
- **Marker visibility (G3)**: hand-write a marker with known fields → reported verbatim in `fetch_markers`; corrupt it → `{"state":"unreadable"}`; none →
  `[]`. Assert the response contains no `http` substring (**§12 rule 8**) and that no database file was created for a workspace that does not exist.
- **Artifact-gated rows**: embed-phase tests use `NP_TEST_MODEL_PATH`; without it they print `NOT COVERAGE`, and the gate row counts them.
- **Docs are checked, not trusted**: a test reads `docs/reference-serve.md` and asserts every name in `METHODS` appears in it, so the reference cannot
  silently fall behind the dispatcher.

## Builder check record

A fresh reviewer read this document before the operator saw it (`delegate`, 2026-12-09; artifact
`c81713ad-6849-44c7-89c5-3b0c9495e9e7_delegate_0_output.md`) and raised ten questions. Where each landed:

| # | Question | Where it went |
| --- | --- | --- |
| 1 | Can `status.get` be answered *during* a request? | **Operator chose push-only (O1)**: §5/§6 no longer claim what this build cannot do, and `running` is documented as always `null` |
| 2 | Which terminal notification does a degraded success get? | Answered by constraint 3: `completed` mirroring the response kind, states as data; names changed from `ready`/`stopped` |
| 3 | Is `search` in scope (it auto-fetches too)? | **Operator put it in scope (O2)**: §10 covers its phases, plumbing and terminal rule |
| 4 | Per-member or cumulative fetch bytes? | Answered from code (§3): per member, with `members_done` / `members_total`; monotonicity restated; two denominators separated |
| 5 | What exactly is `fetch_marker`, and how is it read? | Answered (§5): a list, via the cheap `fetch_state` directory read, never opening a database; unreadable reported not deleted |
| 6 | Can the G4 test be "byte-identical"? | No — corrected in §8 to framing + documents equal except additive `initialize` keys |
| 7 | What seam makes serve-level progress testable? | Left to the builder, with a requirement that it names which feature rows run |
| 8 | Trait shape vs existing seams; np-core has no serde_json | Answered: constraint 6 added, plain-Rust event type, reuse `between_batches` / `after_walk`, keep public signatures stable |
| 9 | Live or bookended scan? | Decided (§9): bookended, with the measured 2.6 s as the reason |
| 10 | What goes in `request` for an id-less request? | Decided (§2): omit the key; test asserts absent-or-sent |

## Rollout

**Status: shipped on `main`.** It rides the next release as a feature addition — protocol stays `1`, so it is a `0.3.x` bump unless something
else in flight forces otherwise. Nothing here changes CLI behaviour, exit codes, `--json` documents, or on-disk format; a client that ignores
notifications sees the v0.3.0 server as documented, which is asserted by a test rather than promised here (the 47 pre-existing `tests/cli.rs`
cases pass untouched, in both feature configurations). See [As built](#as-built-2026-12-09) for the twelve places the implementation landed
differently from this design.

---

## As built (2026-12-09)

The implementation is shipped; this section records where it landed differently from the design, and why. Each entry names the
commit or test that pins it, so a future reader can re-check the claim instead of trusting this prose.

**Shaped as designed.** Opt-in via `initialize.params.notifications`, protocol version unchanged at `1`, notification method
`np/progress` with **no `id` member**, always-present `command`/`phase`/`since_ms` and `request` only when the request had an id,
per-member byte counters with a pinned denominator, terminal line mirroring the response kind, `status.get` returning exactly
`{last, fetch_markers, running}` with `running` permanently `null`, bookended scan, search narrating `resolving`.

**Where the design was wrong, and what replaced it.**

1. **`Progress::NONE` cannot exist.** The design wrote the no-op reporter as an associated constant of type `&dyn Progress`;
   Rust does not allow a `&dyn Trait` value to be an associated const. Shipped as the free function `progress::none()`, used by
   every CLI call site — which is what keeps `np search --json` byte-identical (the 47 pre-existing `tests/cli.rs` cases pass
   untouched on default features; under `--features local-model,fetch` that file runs 61 cases, because fourteen of them are
   feature-gated additions rather than the same set re-run — the first draft of this sentence conflated the two).
2. **`Phase::Resolving` was missing from the enum.** Search needs a phase for "deciding which provider and artifact", because
   that is the stretch where it can start downloading 309 MB without writing an answer. It is also the *default* phase, so a
   silent reporter means "still deciding", never a false claim of work in progress.
3. **`Progress` is not `Sync`.** The design implied a shareable reporter. The adapter holds `&mut Channel` (stdout) through a
   `RefCell`; requiring `Sync` would have made it impossible to compile, and the server is single-threaded by design. Removed
   rather than worked around with locks.
4. **Fetch reporting takes a `Transfer` struct, not eight arguments.** The planned signature tripped `clippy::too_many_arguments`
   under `-D warnings`. `fetch::Transfer { transport, clock, progress, … }` bundles the environment; `fetch_with_progress` does
   the work and `fetch` / `fetch_with_table` stay thin wrappers over `progress::none()`, so no existing caller changed shape.
5. **`fetch_reporting(...)` was added to np-core.** Both embed and search reach auto-fetch through `fetch_pinned_missing`
   (`main.rs:1971`), and rebuilding pin expectations at each call site to thread a reporter would have created a second path to
   get wrong. One function, one reporter parameter, both commands covered.
6. **Members are `main` and `mmproj`,** as `pins.rs` and `models::MAIN_FILE` name them — not `model`. The design's §6 prose was
   wrong about the wire format; `docs/reference-serve.md` shows captured lines with the real spelling.
7. **`methods` does not change on opt-in.** It lists what the build accepts, and `status.get` works either way. Advertising it
   only to opted-in clients would understate the server: the flag that changes unsolicited traffic is `progress`.
8. **The scan bookend's closing line bypasses the throttle** (`Channel::notify_unthrottled`). Applying one-line-per-second to a
   pair that lands in the same second drops the ending on every workspace that scans in under a second — which is all of them —
   and a client left with only the opening line believes a walk is still running. Throttling exists to keep long phases from
   becoming traffic, not to break sentences in half.
9. **`chunks_total` did not ship — and the reason given for not shipping it was wrong.** This section claimed np-core's chunk pass
   discovers work lazily, so any total at phase start would be a guess. It does not: `embed_run::run` materialises its owed list into
   a `Vec` before the writer loop, which is why the run can batch over it — so the total was always free, and every GUI that wanted a
   percentage was being handed an indeterminate bar on a false premise. Fixed 2026-12-09: the opening line and every batch line carry
   `chunks_total`, fixed for the whole request (a denominator recomputed from what remains would let a bar reach 100 % with work
   queued, and a run that stops early ends below its total — both asserted by name in `crates/np-core/tests/progress.rs`). The lesson
   is narrower than "double-check assumptions": this claim was written because it *sounded* like the shape of an embed loop, and no
   one read the loop before typing it.
10. **`writing` is emitted only when something was actually written** — `embedded` increased between batch callbacks. An embed
    whose batches were all refused by budget or contention therefore ends in `embedding`, and does not claim a commit that did
    not happen. Scan never emits `writing`: it writes the catalog through its own path, and inventing a phase for it would make
    the phase mean two different things.
11. **Terminal ordering is response-then-notification,** deliberately: a client can read the answer as the thing that ends a
    command and use the notification only for continuity. Reversing them would let a client see "completed" for a result it has
    not received yet, which is the one ordering mistake that produces a stuck spinner.
12. **`status.get` never opens or creates the workspace database.** Marker visibility comes from listing the models directory —
    the same path F46's fetch-state read uses — so asking about a path that is not a workspace returns an empty list and leaves
    no `.sqlite` file behind. There is a test for exactly that, because "read-only" is only worth the word if something proves
    it does not write.

13. **The chunk loop was unreachable for a day, and no test noticed.** `embed_report` forwarded its reporter to provider
    resolution and then passed `progress::none()` to `embed_run::embed_with_resolution`, so `np serve embed` never emitted
    `embedding` or `writing` while the comment above that line said it did. np-core's seam tests call `embed_with_inject`
    directly, so they were true; the pipe tests asserted fetch and lock phases, so they were true; twenty gate rows were green.
    A fresh review read the code rather than the ledger and found it in one pass. Fixed by forwarding the reporter, and closed
    with `an_embed_that_works_narrates_the_chunks_it_encodes`: install the pinned artifact, scan, embed for real over a pipe,
    and require a `writing` line whose `embedded` matches the response — plus an unindexed vault producing *no* chunk lines, so
    the case cannot pass by having nothing to do. **The lesson is not "review helps"**: it is that a claim about a channel needs
    a test on that channel. Seam-level proof of the same behaviour is what made twenty green rows possible while the feature was
    off.
14. **Throttle memory is per request, not per connection.** `begin_request` originally reset only the clock, so two searches sent
    within a second lost the second one's `resolving`. The rule in §4 was always "at most one line per second *within a phase of
    one request*"; carrying it across the boundary swallowed exactly the line that says what a request is about to wait for.
15. **`shutdown` ends the session.** The design and the reference both said the loop stops; the implementation noted the request
    and waited for EOF, so a client that obeyed the documentation hung. Answering first and then stopping reading keeps framing
    and exit code identical.
16. **`notifications` of the wrong type is `-32602`.** §1 said "opt-in"; it did not say what `"notifications": "yes"` means, and
    the implementation's answer — silence — was the one answer a client cannot debug. Absent still means off.

**What the tests refuse to pretend.** The fetch-narration case over a real pipe serves the pinned `Content-Length` and then hangs
up after 12 MiB rather than completing a 309 MB download inside `cargo test`; it proves byte checkpoints, the pinned denominator,
forward-only bytes within a member, and a terminal `refused -32000`, and its doc comment says plainly that a *verified* download
belongs to the artifact-gated rows. The origin dribbles one chunk per 1.2 s so the throttle's window is observable instead of
fought — asserting "many lines" against a fast loopback transfer would be asserting that the throttle is broken.

**Measured, not assumed.** Mutants were run for each claim that could otherwise survive by being untested: ignoring the opt-in,
dropping the opening bookend, deleting the `resolving` report, using the transport-announced size as the denominator, letting bytes
run backwards between members, reporting `writing` after a batch that wrote nothing, restoring the dropped chunk reporter, keeping
throttle memory across requests, ignoring `shutdown`, and hand-editing a line inside a documented transcript. Each one turns a named
test red. Two of those four were not: the chunk-reporter mutant was the shipped code (see 13), and a `shutdown` that is noted but
ignored hung the suite instead of failing it until the case learned to poll with a deadline — both are recorded here because a list
of kills reads like a method unless the misses are in it too.

**Evidence.** `.superpowers/sdd/2026-12-09-np-serve-progress/progress.md` carries per-task commits, the rulings above at the
moment they were made, and the gate log paths; `docs/reference-serve.md` is the interface reference with four captured
transcripts, one of which shows a *second* process reporting an in-flight download that the first process could not have
described about itself.
