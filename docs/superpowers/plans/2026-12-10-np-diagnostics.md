# Diagnostics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give people building on `np` a granular window into what it did and why — failures with a machine-readable cause, per-target levels, timing, correlation to the request that caused them.

**Architecture:** `tracing` spans/events instrument `np-core`; `np-cli` owns the subscriber and its sinks. Level resolution (default < config file < env < flag) lives in `np-core::config` beside the existing level rules. stderr stays human-only and as silent as today unless asked; a file sink writes stable JSONL.

**Tech Stack:** Rust 2021 workspace (`np-core`, `np-cli`), `tracing`, `tracing-subscriber` (EnvFilter), `serde_json` for the file records, existing gate script `.github/scripts/gates.sh`.

**Spec:** `docs/superpowers/specs/2026-12-10-np-diagnostics-design.md` — including the *Decisions settled at approval* section, which supersedes earlier text.

## Global Constraints

- stdout is protocol (`--json`, `np serve`). Diagnostics go to stderr or a file. Never stdout.
- Note bodies never enter a diagnostic; no origin host or URL ever does (§12 rule 8). Paths, ids, rule tokens, counts only.
- Precedence: default `warn` < `[log] level` (file) < `NP_LOG` < CLI flag (§12 rule 7 — file beats env, pinned at `crates/np-core/src/loglevel.rs:160` / `crates/np-cli/tests/cli.rs:3726`). `RUST_LOG` is **not** honoured.
- Existing CLI/serve output bytes do not change. Every case in `crates/np-cli/tests/cli.rs` and `tests/serve.rs` passes untouched; a refusal that prints one line today (`cli.rs:2338`, `:2634`, `:2868`) still prints exactly one line.
- stderr diagnostics cap at **1 000 lines per process**, checked before formatting, then one suppression notice. Pinned user-facing output is never counted or suppressed.
- File sink: no line cap; `[log] max_bytes` default `8388608`; past it, stop writing and emit one final record. No rotation, no rename.
- An unopenable log path warns once on stderr and continues. Diagnostics are never the reason a command fails.
- JSON appears only in the file. `--log-format jsonl` requires a file path.
- Targets are exactly: `np_core::scan`, `np_core::embed`, `np_core::search`, `np_core::fetch`, `np_core::db`, `np_core::models`, `np_core::config`, `np_cli`, `np_cli::serve`, `np_cli::serve_channel`.
- New dependencies go in the workspace `[workspace.dependencies]` table; `tracing-subscriber` only in `np-cli`.
- llama.cpp's transcript stays on `[embeddings] log_level` / `NP_LLAMA_LOG_LEVEL` (F33) and is not routed through the new channel.

**Falsifiability rule for this plan:** every task names a mutation that must turn one of its own tests red, and the mutation is run and recorded before the commit. A test that passes with its feature deleted does not exist.

---

### Task 1: Level and target resolution

**Files:**
- Modify: `crates/np-core/src/config.rs` (beside `resolve_level`, ~line 797), `crates/np-core/src/loglevel.rs` (reuse the parse shape at `:26-46`)
- Modify: root `Cargo.toml`, `crates/np-core/Cargo.toml`
- Test: `crates/np-core/tests/config.rs`

**Interfaces:**
- Produces: `pub struct DiagSpec { pub default: Level, pub per_target: Vec<(String, Level)> }`, `enum DiagSource { Default, Config, Env, Flag }`, `fn parse_diag_spec(&str) -> Result<DiagSpec, ConfigError>`, `fn resolve_diag(cli: Option<&str>, env: Option<&str>, config: Option<&str>) -> (DiagSpec, DiagSource)`
- Levels: `off | error | warn | info | debug | trace`. Spec grammar: a bare level (`debug`) or comma list of `target=level` with optional bare level first (`info,np_core::search=trace`). Unknown target names are accepted (they may exist in another build); an unparseable value is an error naming the variable it came from.

- [ ] **Step 1:** In `crates/np-core/tests/config.rs`, write `diag_spec_parses_levels_and_target_overrides` asserting `parse_diag_spec("info,np_core::search=trace")` yields default `Info` and one override, and `diag_spec_refuses_naming_its_source` asserting a bad value's error text contains `NP_LOG` when resolved from env.
- [ ] **Step 2:** Run `cargo test -p np-core --test config diag_` — expect FAIL (unresolved names).
- [ ] **Step 3:** Implement the three items above. Precedence inside `resolve_diag`: flag > env > config > default `warn`; return which one won so callers can print it.
- [ ] **Step 4:** Add `diag_resolution_prefers_the_file_over_the_environment` (file `warn`, env `trace` → `warn`, source `Config`) and `diag_flag_beats_everything`. Run the test file — expect PASS.
- [ ] **Step 5:** Mutation: make env outrank config in `resolve_diag`; confirm `diag_resolution_prefers_the_file_over_the_environment` goes red; restore; re-run green. Record the mutation in the commit message.
- [ ] **Step 6:** Commit `feat(config): resolve diagnostic levels from file, environment and flag`.

### Task 2: Sinks — stderr with a cap, JSONL file with a budget

**Files:**
- Create: `crates/np-cli/src/diag.rs`
- Modify: `crates/np-cli/src/main.rs` (install before dispatch, after flag parse; upgrade after first `load_config()`), `crates/np-cli/Cargo.toml`, `crates/np-core/src/config.rs` (`[log] level|file|max_bytes` keys)
- Test: `crates/np-cli/tests/diag.rs` (new)

**Interfaces:**
- Consumes: Task 1's `resolve_diag`.
- Produces: `pub fn install(spec: &DiagSpec, file: Option<&Path>, max_bytes: u64, announce: bool) -> DiagHandle`, JSONL record keys exactly `ts, level, target, msg` plus optional `request, workspace, rule, what, expected, actual`.

- [ ] **Step 1:** Write `stderr_stays_silent_at_default_level`: run the built binary with no flags over a temp vault; assert stderr equals today's bytes for the same command (reuse an existing pinned expectation) and contains no `np_core::` target prefix.
- [ ] **Step 2:** Write `verbose_prints_the_effective_level_and_its_source` (`-v` → first stderr line matches `^log level info from ` + one of `default|config.toml|NP_LOG|command line``) and `stderr_caps_at_one_thousand_lines` (force trace on a vault of 1 200 notes; assert ≤1 000 diagnostic lines, then exactly one line containing `diagnostics truncated after 1000 lines`).
- [ ] **Step 3:** Write `the_file_holds_jsonl_and_stops_at_its_budget`: `[log] file` + tiny `max_bytes` → each line parses as JSON, keys ⊆ the frozen set, last record has `"msg":"log full"`, and the process still exits 0. Add `an_unopenable_log_path_warns_once_and_the_command_succeeds` (path is a directory → one stderr warning, exit code unchanged).
- [ ] **Step 4:** Run `cargo test -p np-cli --test diag` — expect FAIL, then implement `diag.rs`: a `Layer` writing human text to stderr with an atomic counter that checks the level and the cap before formatting; a file layer that opens append, writes one object per line, and latches off past `max_bytes`.
- [ ] **Step 5:** Run the whole `np-cli` suite. Expected: every pre-existing case passes **unedited**. Any edit to `cli.rs`/`serve.rs` expectations is a design violation — stop and report instead of adjusting the pin.
- [ ] **Step 6:** Mutation: remove the cap check; confirm `stderr_caps_at_one_thousand_lines` red; restore. Commit `feat(cli): diagnostic sinks with a stderr cap and a file budget`.

### Task 3: Failure-rule events on existing tokens

**Files:**
- Modify: `crates/np-core/src/search.rs`, `embed_run.rs`, `fetch.rs`, `lock.rs`, `models.rs` — emit where the result document is already built
- Test: `crates/np-core/tests/diagnostics.rs` (new)

**Interfaces:**
- Produces: one event per refusal/degraded state, at `warn` (state) or `error` (failure), always with fields `rule`, `what`, and `expected`/`actual` where numbers exist. Tokens reused verbatim: `provider-disabled`, `local-model-not-built`, `prefix-drift`, `token-budget`, `locked`, `offline`, `artifact-missing`.

- [ ] **Step 1:** Write `a_refusal_reports_its_rule_field_not_only_its_sentence`: run embed with embeddings switched off, capture diagnostics at `debug` into a file, assert one record has `rule == "provider-disabled"` and `target == "np_core::embed"`.
- [ ] **Step 2:** Run it — expect FAIL (no records). Implement events at the same call sites that build the existing result documents; no new prose, fields only.
- [ ] **Step 3:** Add the offline case (`rule: "offline"` on a search during outage mode) and the lock case (`rule: "locked"`, `what` = workspace path). Run — expect PASS.
- [ ] **Step 4:** Privacy test `a_diagnostic_never_carries_note_text_or_an_origin`: build a vault containing a unique body string and a link to an origin host, run scan+embed with trace to a file, assert the file contains the note *path*, not the body, and `!text.contains("http")`.
- [ ] **Step 5:** Mutation: drop the `rule` field from one event; confirm step 1 red; restore. Commit `feat(core): diagnostics name the rule behind every refusal`.

### Task 4: Scan's missing vocabulary

**Files:**
- Modify: `crates/np-core/src/scan.rs` (skip sites ~`:1322`, counters for `uncovered`/`hidden`), `walk.rs:132-136`
- Test: `crates/np-core/tests/diagnostics.rs`, `docs/reference-diagnostics.md` (new, token list)

**Interfaces:**
- Produces: tokens `not-markdown`, `unreadable`, `unrepresentable-path`, `parse-failed`, `unchanged-content`, `hidden-by-ignore` emitted with `what` = path; the doc lists every token in code and vice versa.

- [ ] **Step 1:** Write `a_skipped_note_says_which_rule_skipped_it` (vault with one `.txt`, one unreadable `.md`, one unchanged `.md`; assert three records with those tokens, each carrying the path).
- [ ] **Step 2:** Run — expect FAIL. Add a small `Skip { path, rule }` alongside the existing bare-path pushes so counters keep their current meaning; emit at `debug`.
- [ ] **Step 3:** Write `the_documented_token_list_matches_the_code`: a test that greps the token constants from the crate and compares to the fenced list in `docs/reference-diagnostics.md`; run — expect FAIL until the doc is written, then PASS.
- [ ] **Step 4:** Mutation: emit `unchanged-content` for the unreadable case; confirm step 1 red; restore. Commit `feat(scan): diagnostics report why each note was skipped`.

### Task 5: Spans, timing, and request correlation

**Files:**
- Modify: `crates/np-core/src/{scan.rs,embed_run.rs,search.rs,fetch.rs}`, `crates/np-cli/src/serve.rs`, `serve_channel.rs`
- Test: `crates/np-cli/tests/diag.rs`, `crates/np-cli/tests/serve.rs` (additive only)

**Interfaces:**
- Produces: spans named `scan`, `embed`, `search`, `fetch`, `serve.request`; every record inside a serve request carries `request` = the JSON-RPC id, **omitted entirely** when the request had none; `elapsed_ms` on span close.

- [ ] **Step 1:** Write `a_serve_request_tags_every_line_it_causes`: two requests over one pipe (one `id: 9`, one id-less) with diagnostics to a file; assert records tag `request: 9`, and that the id-less request's records have no `request` key at all (never `"none"`, never `null`).
- [ ] **Step 2:** Run — expect FAIL. Add spans (explicit guards inside hot loops, attribute elsewhere) and the per-request span in `serve.rs`.
- [ ] **Step 3:** Write `a_slow_phase_reports_its_cost` asserting a scan record at `info` carries `elapsed_ms` > 0 and search reports its mode fallback at `debug`. Run — expect PASS.
- [ ] **Step 4:** Mutation: close the request span before dispatch; confirm step 1 red; restore. Commit `feat(serve): correlate diagnostics to the request that caused them`.

### Task 6: Documentation and measured cost

**Files:**
- Create: `docs/reference-diagnostics.md`, `docs/measurement-np-diagnostics-cost.md`
- Modify: `README.md` (Build on `np` table row), `docs/reference-config.md` (three `[log]` keys), `docs/reference-serve.md` (diagnostics note)

- [ ] **Step 1:** Write `docs/reference-diagnostics.md`: the ten targets, level grammar, precedence with its source-print line, both caps, the frozen JSONL schema, and the full token list. Every example transcript is captured from a real binary into `/tmp` files and pasted verbatim — never hand-composed.
- [ ] **Step 2:** Add a doc-sync test in `crates/np-cli/tests/diag.rs`: `the_diagnostics_reference_documents_every_target_and_token`, reusing the completeness helpers from Tasks 3–4 so docs cannot drift from code.
- [ ] **Step 3:** Measure cost: run the existing corpus harness with diagnostics off and with `NP_LOG=trace,np_core::scan=off`, write both numbers and the method into `docs/measurement-np-diagnostics-cost.md`, stating plainly that this is a measurement on one memory-tight box, not a gate fence.
- [ ] **Step 4:** Add the README row pointing at the reference; run the link validator (fences and inline code stripped) — expect zero broken links across the markdown set.
- [ ] **Step 5:** Run `.github/scripts/gates.sh /tmp/gates-diag` and read `driver.log`: every row's exit code, the tree sha with `dirty=0`, and the `NOT COVERAGE` count stated in the commit message rather than assumed. Commit `docs: a reference for reading np's diagnostics`.

---

**Deliberately not in this plan:** the optional `np/diagnostic` notification mirror from the spec. It is separable, ships nothing the
file sink does not already give an integrator, and adding it would put a new wire key through the serve suite in the same pass that is
still proving stderr stays untouched.

## Review Focus

Inputs no task above pins by name, most likely to bite someone integrating:

1. **A config that refuses** (unknown key, bad `[log] level`) — `np serve` must still start and answer requests while reporting the error; add to Task 2 Step 3: `a_bad_config_reports_but_still_serves`.
2. **Two processes writing one log file** (a serve child plus a concurrent `np scan`) — interleaving must never produce a half-written JSON line; Task 2 Step 3: `concurrent_processes_never_split_a_record`.
3. **A vault path containing note-like text** — truncation of `what` at a bound, never redaction of the whole field; Task 3 Step 4 extended with a long path case.
4. **Deleting the log file mid-run** (documented as safe) — writes continue or stop cleanly, no panic; Task 2 Step 3: `deleting_the_log_file_mid_run_does_not_fail_the_command`.
5. **A client that never reads stderr while the pipe fills** — diagnostics must not block the JSON-RPC loop; Task 5 Step 1 extended with a case that emits >64 KiB of diagnostics on a request and still answers it.
