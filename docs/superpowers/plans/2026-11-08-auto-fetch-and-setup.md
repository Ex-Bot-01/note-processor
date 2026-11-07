# F38 auto-fetch and `np setup` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let an application call `np` and end up with vectors — fetching the pinned embedding model when it is missing, and degrading honestly when there is no connection.

**Architecture:** One new config key (`[models] auto_fetch`, default **true**) gates a fetch that fires inside provider resolution for the commands that write vectors, before any transaction opens. Failure classification reuses `fetch.rs`'s existing transport classes: transport-level failure means *offline → skip*, anything the origin actually answered stays a loud refusal. A new `np setup` command sequences ensure-artifact → scan → embed and reports where it stopped.

**Tech Stack:** Rust 2021, rusqlite (bundled), clap 4, ureq via the `Transport` seam, this repository's own gates in `.github/scripts/gates.sh`.

**Spec:** `docs/design-f38-auto-fetch.md` — read it first; this plan argues from it.

## Global Constraints

- Default for `[models] auto_fetch` is **true**; `--no-auto-fetch` and `NP_NO_AUTO_FETCH=1` switch it off, and with either set every pre-F38 refusal must return **word for word at its old exit code**.
- Auto-fetch fires only for ids in `crate::pins` (today exactly `embeddinggemma-2`) and only from the resolved origin. No URL or host ever appears in a report or log line (§12 rule 8) — origins are named by *kind*.
- Never fetch on: `RuntimeNotBuilt`, `np search`, any unpinned id, or inside the embed batch loop. Fetch happens before a transaction opens.
- Offline means *skip and say so*: `embed` exits **1**, `setup` exits **0** with `vectors:false`. TLS failures are **not** offline. Digest mismatch is never offline.
- Exit codes unchanged: 0 success, 1 runtime refusal, 2 argv. Exactly one JSON document on stdout; progress and notices go to stderr.
- Config stays strict (`deny_unknown_fields`, F35): a misspelled `auto_fetch` refuses naming file, line and key.
- Never touch anything under `tests/golden*`. `CMAKE=/home/agent/tools/cmake/bin/cmake` for every `local-model` row.
- Commit after each task with the task number in the subject.

## Review Focus

Five things a reviewer must check that no single task's tests fully cover: (1) hermeticity is now load-bearing — an unpinned id refusing before a transport is chosen is what keeps the suite off the network, so it needs its own test; (2) the `fetched`/`offline` keys must be present-and-null/false in *every* document, not added only on the paths that fetched; (3) offline classification is derived from failure text that already exists — a new class string added to `fetch.rs` later must be placed in one side of the offline table or the other, never left ambiguous; (4) `setup`'s exit 0 while offline is deliberate and must not leak into `embed`; (5) nothing may stamp, publish or count rows while the artifact is absent.

---

## Task 1 — The key: `[models] auto_fetch`, default true, two kill switches

**Files:** `crates/np-core/src/config.rs`, `crates/np-core/tests/config.rs`

- [ ] Failing test in `tests/config.rs`: `an_absent_auto_fetch_means_the_pinned_model_may_be_fetched` — a config with no `[models]` section and none elsewhere yields `models.auto_fetch == true`.
- [ ] Failing test: `auto_fetch_can_be_switched_off_in_the_file` — `[models]\nauto_fetch = false` reads back false; `auto_fetch = "yes"` refuses as a type error naming the key.
- [ ] Failing test: extend `every_section_refuses_an_unknown_key` and `a_file_of_real_keys_still_loads_every_section` with an `auto_fetche` row and a real `auto_fetch` row.
- [ ] Failing test: `the_kill_switch_lives_outside_the_file_too` — `NP_NO_AUTO_FETCH=1` forces false even when the file says true (precedence: env beats file, matching every other key in that section; if `config.rs` documents the opposite order for `[models]`, follow the file and update this step's assertion).
- [ ] Implement: `pub struct ModelsConfig { …, #[serde(default = "default_true")] pub auto_fetch: bool }` plus `ModelsConfig::auto_fetch_effective()` (or a free function next to the existing env reader) that applies `NP_NO_AUTO_FETCH`.
- [ ] `cargo test -p np-core --test config` green; commit `feat(f38): [models] auto_fetch defaults to true, with a kill switch outside the file`.

## Task 2 — Offline is a fact about the failure, not a probe

**Files:** `crates/np-core/src/fetch.rs`, `crates/np-core/tests/fetch.rs`

- [ ] Failing test: `the_classes_that_mean_no_connection_are_one_group` — assert `class_kind("could not resolve the configured host")`, `"timed out"`, `"transport failure"` are `ClassKind::Offline`, while `"not found"`, `"refused by the server (authentication or permissions)"`, `"server error"`, `"redirect refused"`, `"TLS failure"` and the integrity class are **not**.
- [ ] Failing test: a transport whose `FetchError` carries one of the offline classes answers `FetchError::is_offline() == true`, and one carrying `"not found"` answers false.
- [ ] Implement `pub enum ClassKind { Offline, NotFound, Forbidden, Server, Redirect, Tls, Integrity, Other }`, a `pub fn class_kind(class: &str) -> ClassKind` beside the existing classifier, and `FetchError::is_offline()`. Doc-comment the table from §5 of the spec verbatim, including why TLS is excluded.
- [ ] Add a compile-time tripwire: `#[cfg(test)]` test asserting every class string the existing classifier can return appears in exactly one `ClassKind` arm's test list (a list literal is fine; a later new class must be placed deliberately).
- [ ] `cargo test -p np-core --test fetch` green; commit `feat(f38): classify fetch failures so a missing connection can be skipped and a wrong answer cannot`.

## Task 3 — The trigger: provider resolution may fetch, once, before anything opens

**Files:** `crates/np-cli/src/main.rs`, `crates/np-cli/tests/cli.rs`, `crates/np-cli/tests/models_cli.rs`

- [ ] Failing test in `tests/cli.rs`: with `[embeddings] model = "embeddinggemma-2"` set, no `models` row and no artifact, and **no** kill switch, `np embed <ws> --json` reports a fetch attempt whose failure class is offline when the origin cannot be reached (run it with `NP_MODELS_BASE_URL=http://127.0.0.1:9/` — nothing listens on port 9, so the transport fails in an offline class without touching the internet) and exits 1 saying nothing was embedded.
- [ ] Failing test: the same command with `NP_NO_AUTO_FETCH=1` prints today's `ArtifactAbsent` sentence unchanged and never mentions a fetch.
- [ ] Failing test: an **unpinned** id (`model = "my-private-embedder"`) with auto-fetch on refuses exactly as before — no fetch attempt, no origin language. This is the hermeticity invariant.
- [ ] Failing test: a default build (no `local-model`) with a pinned id and no artifact keeps today's refusal and adds that fetching would not help this binary. Assert the sentence; assert no transport was attempted by pointing `NP_MODELS_BASE_URL` at a dead port and seeing no fetch line.
- [ ] Implement: resolve auto-fetch once (`flag → env → config → true`), and in provider resolution, when state is `NoModelRow`/`ArtifactAbsent` **and** `pins::identity(id).is_some()` **and** the runtime is built, call the existing `fetch::fetch_one(transport, …)` for the `main` member with `HttpTransport::for_origin(configured_origin)`. On `is_offline()`, return a new provider-state arm `FetchSkippedOffline`; on any other error, propagate `fetch`'s own refusal text.
- [ ] Add the notice printed before byte one: id, member name, expected bytes, origin *kind* — reuse `np models fetch`'s existing report type rather than inventing a second one.
- [ ] `cargo test -p np-cli --test cli --test models_cli` green; commit `feat(f38): embed fetches the pinned artifact when it is missing, and says so before byte one`.

## Task 4 — The contract an app parses: `fetched` and `offline` in every document

**Files:** `crates/np-cli/src/main.rs`, `crates/np-cli/tests/cli.rs`, `docs/design-m2-embeddings.md` (the JSON section) if it enumerates keys

- [ ] Failing test: `embed --json` always carries `"fetched"` and `"offline"`. Assert three shapes: nothing fetched → `null`/`false`; offline skip → `null`/`true`; fetched (real artifact row written, no transport — assert the *key shape* here, the real transfer is Task 6's gated test) → object with `id`, `members[].{name,bytes,digest_verified}`, `origin`.
- [ ] Failing test: no document contains a scheme or host — assert `!text.contains("http")` and `!text.contains("huggingface")` on the offline-skip run, which is the only one that names an origin at all.
- [ ] Implement the two keys in the embed JSON writer; text mode prints one stderr line when a fetch was skipped for being offline: `offline: could not reach the pinned origin, so nothing was embedded — connect and re-run np embed`.
- [ ] Green; commit `feat(f38): embed's report says what it fetched and whether the world was offline`.

## Task 5 — `np setup <workspace>`: one command that ends with vectors, or says where it stopped

**Files:** `crates/np-cli/src/main.rs`, `crates/np-cli/tests/cli.rs`

- [ ] Failing test: `np setup <ws> --json` on a fresh vault with no artifact and a dead origin exits **0**, with `scan` counts present, `"vectors": false`, `"offline": true`, and `"fetched": null`; text output ends with `indexed; vectors skipped — offline`.
- [ ] Failing test: `np setup --help` exists and the subcommand parses; an invented flag exits 2.
- [ ] Failing test (gated on `local-model` + `NP_TEST_MODEL_PATH`, prints `NOT COVERAGE` otherwise): with a real artifact already installed, `setup` runs scan → embed and reports `"vectors": true`, `"offline": false`, `"fetched": null`.
- [ ] Implement: `Setup` subcommand = ensure-artifact (Task 3's trigger) → scan → embed, each step reusing the existing command bodies rather than duplicating them. TTY courtesy: when stdin is a terminal and a transfer is about to start, ask `[y/N]` with the byte count; skip the question when `--yes`, when not a terminal, or when the kill switch is set. Use `std::io::IsTerminal` (Rust 1.70+, no new dependency).
- [ ] Green; commit `feat(f38): np setup — index now, fetch if needed, and report exactly where it stopped`.

## Task 6 — Locks, docs, and the amendment trail

**Files:** `.github/workflows/ci.yml`, `.github/scripts/gates.sh`, `README.md`, `docs/design-m3-hybrid-search.md` §7, `docs/§12 design doc` (rule 1), `docs/backlog-m2-followups.md`

- [ ] Export `NP_NO_AUTO_FETCH=1` for the test rows in CI and in `gates.sh`, with a comment saying why it is a second lock and not the first (unpinned fixture ids already refuse before a transport).
- [ ] Failing test that proves the lock: a workspace whose `[embeddings] model` names the pinned id, run under `NP_NO_AUTO_FETCH=1`, refuses without attempting a transport. (Task 3 has the shape; keep both — one guards behaviour, one guards CI.)
- [ ] README: first-run paragraph states plainly that `np embed` may transfer ~310 MB from the pinned origin on a machine that has nothing installed, and gives both kill switches; the "only command that dials out" sentence is amended rather than deleted.
- [ ] `docs/design-m3-hybrid-search.md` §7: record the amendment — search still never fetches; embed and setup may.
- [ ] Backlog: F38 card with the ruling dated 2026-11-08 (default true, offline skips), and a line in F34's card that auto-fetch inherits the Windows preflight answer rather than bypassing it.
- [ ] Full gates `.github/scripts/gates.sh /home/agent/tmp/gates-f38` — every row 0, `NOT COVERAGE` counted; then merge with `--no-ff` and push.
