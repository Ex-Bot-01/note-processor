# R6c1 — `np embed <workspace>`: state of the round (controller-authored)

**Who did what.** An implementer subagent (`f75ced6e`, resumed as `4eca2b5f`) wrote the code and tests in
`crates/np-core/src/embed_run.rs`, `crates/np-core/tests/embed_run.rs`, the `db.rs` readers, and the np-cli wiring. It
timed out **twice** — first at 30 min with an uncommitted tree, then at the 25 min I set explicitly on the resume — both
times mid-round with no report. It committed `cc1b745` (its checkpoint) after my instruction; I committed the remaining
dirty state as `ca625ab` so nothing could be lost, then ran every gate myself and fixed three rustdoc errors. Everything
claimed below was run by me on this host; nothing here is inherited from the subagent's word.

**Branch:** `m14/embed-write`. Commits: `cc1b745` (implementer checkpoint), `ca625ab` (controller checkpoint), plus the
doc-link fix in HEAD.

## What exists and is verified by exit code (measured 2026-10-31, this host)

| Gate | Result |
| --- | --- |
| `cargo fmt --check` | 0 |
| `cargo clippy --all-targets -- -D warnings` | 0 |
| `cargo test --workspace` | 0 — **455 passed / 0 failed** (baseline was 436; +19 new cases in `tests/embed_run.rs`) |
| `env -u HOME -u XDG_CONFIG_HOME cargo test -p np-core --test config` | 0 (33) |
| `RUSTDOCFLAGS="--deny warnings" cargo doc --locked --workspace --no-deps` | **101 before my fix, 0 after** (see Defects D-1) |
| same with `--features local-model -p np-core` | 101 → 0 |
| `cargo check/clippy --features local-model` (np-core and np-cli) | 0 |
| `check_cfg_dead_code.py`, `check_attr_displacement.py` | pass |
| `verify_catalog_goldens.py` | **GOLDEN CHECK: pass** — still zero golden bytes moved since M1 |
| `cargo test -p np-cli --test cli` | 0 (19) — unchanged count: **no CLI-level case for `np embed`** (D-3) |

### Real-binary behaviour (my own smoke, `/tmp/np-smoke`, default build without `local-model`)

* `np scan <ws>` → exit 0, `notes 1`.
* `np embed <ws>` with no config → report + stderr line
  `np: configuration disabled: embeddings.provider = "none"`, **exit 1**, counts all zero, `verify self-consistent, not verified`. Arm (ii) ✓
* same with `provider = "local-model"`, `model = "ghost-model@1"` (no row) →
  `configuration disabled: embeddings.model = "ghost-model@1"`, exit 1. Arm (ii) naming the id ✓
* after `np models install <ws> --id tiny-model@1 --path fake.bin --dim 768 --sha256 …`:
  `outcome owed and not produced`, detail *"the local embedding runtime is not built yet…"*,
  `seen 1 / errors 1`, exit 1 — so the currency query really reads the scan's chunk rows, and the default build refuses
  through `RuntimeNotBuilt` as designed. Arm (iv) ✓
* `np embed --json` → one object with `outcome.configuration-disabled.{id,key}` and every count; text and JSON agree ✓
* `np embed` with no operand → **exit 2** (clap), no new exit code ✓

## Defects found by running, not by reading

**D-1 (fixed by me).** The doc gate failed three ways the implementer never reached: `[`StoredChunk::text`]` and
`[`Owed::digest`]` linked to private items, and `[`crate::scan`]` in `embed_run.rs`'s module header is **ambiguous**
because `lib.rs:31` re-exports the *function* `scan` beside the module of the same name. Fixed: prose instead of links for
the private items, `mod@crate::scan` for the module, with a sentence explaining why the `mod@` is needed there and nowhere
else in the crate. Both doc gates now 0.

**D-2 (open, P1): arm (iii) is unreachable from the binary, and its absence produces a false sentence.** With a model row
installed and the artifact file moved away, the real `np embed` prints:

> `np: the model declares 0 dimensions but this run needs 768; refused before any write, because one model id cannot mean two widths`

Nothing about that is true — the row declares **768**, and the actual fact is *the artifact file is not there*. Mechanism:
`main.rs::embed_provider` maps `EmbedError::ArtifactUnreadable` to a `NoEmbeddingProvider` placeholder whose `dim()` is 0,
and `embed_run` then compares widths and takes its dim-mismatch arm. So the most common first-run state for someone who
followed §6 — configured a model, not yet installed weights — gets a wrong label and a wrong remedy, while
`an_absent_artifact_names_a_command_that_exists` stays green because it drives `embed_run` directly rather than through
the CLI. The fix is structural, not cosmetic: the *reason* a provider could not be built must travel to `embed_run` as a
value (or `embed_run` resolves configuration state itself) instead of being inferred from a placeholder's width — one
placeholder currently stands in for four different states (`provider = "none"`, missing `model`, id naming no row, absent
artifact), and only the first three are distinguishable from configuration alone.

**D-3 (open): no CLI-level tests.** All 19 arms/cases live in `crates/np-core/tests/embed_run.rs`. `crates/np-cli/tests/cli.rs`
gained nothing, so argv shape, the two-channel stderr+report rule, exit codes and JSON field names are untested — which is
exactly how D-2 survived a green suite.

**D-4 (open): the prefix rule lives in two places.** `embed_run` builds the model input as
`format!("{}{text}", row.prefix_doc)` while `LocalModelProvider::model_inputs` owns the same spelling. Today they agree;
if one moves, `embeddings.source_hash` silently stops describing the bytes the model read — which is the whole point of the
§6/D4 stamp rule. One of them should call the other, or both should call a shared helper.

**D-5 (open): budget refusals are inherited, not checked.** The runtime's `TokenBudgetExceeded` is counted into
`refused_budget`; nothing in this round independently prices a chunk against `embeddings.max_tokens`. That is defensible
(a tokenizer needs the feature) but the module doc must say so plainly rather than implying §5's budget is enforced here.

## Not done / not verified

* Docs: README does not list `np embed`; design §6 still tells the reader to run `np models fetch` (which does not exist —
  the *code's* message correctly names `np models install`, so only the doc diverges); `todo.md` and the `EmbeddingsConfig`
  field comments that claim `model`/`dim` are read by nothing are now false.
* D-2 through D-5 above.
* No artifact-gated run of this path (the round is designed to be testable without one), so no real vector has been written
  to a database yet — that is R6c2, together with batch invariance under a non-empty `prefix_doc`.
* CI: cannot start jobs account-side since 2026-10-29; nothing here was verified on any platform but Linux.

---

## 2026-10-31 — controller section: D-2 fixed and verified, D-3 landed, third timeout

The fix round (`b5feb5ab`) timed out at its 25-minute deadline **after committing D-2 (`98f701e`) and leaving D-3's
`crates/np-cli/tests/cli.rs` (+364 lines) uncommitted**. I recovered it from the tree rather than rerunning the work: it
passes as written. Its own report section was never written — this section is mine, and every claim below is something I
ran.

**D-2, as fixed.** np-cli's provider resolution now returns `Option<Box<dyn Embedder>>`, and `None` means exactly one
thing — the row exists, its width matches the run, only the bytes are missing — which the call site turns into
`Resolution::ArtifactAbsent`. `embed_run` skips the width comparison in that case rather than comparing a placeholder's
zero. The comment at the mutation point says why: a filesystem fact cannot be recovered downstream from configuration.

**Verified against the binary by me** (scratch workspace, default build, artifact file moved aside):

```
outcome    artifact absent
detail     install the artifact with `np models install /tmp/np-smoke2/ws --id <id> --path <file> --dim <width> --sha256 <digest>` (fetching a model is not implemented in this build)
artifact   /tmp/np-smoke2/data/workspaces/f4163026c4741177.models/tiny-model@1
seen 1  current 0  embedded 0  refused 0  busy-gap 0  errors 0
verify     self-consistent, not verified
exit 1
```
`--json` carries the same fact as `outcome["artifact-absent"]{artifact, hint, model}`; putting the file back returns to arm
(iv) *owed and not produced* through `RuntimeNotBuilt`. No "dimensions" sentence anywhere.

**Mutation evidence for D-3's regression test.** Reintroducing the old routing (`ArtifactUnreadable => placeholder`) makes
exactly two cases fail — `embed_with_the_artifact_file_missing_names_installing_it` and
`embed_json_reports_the_same_outcome_and_counts_as_the_summary` — 23 passed / 2 failed; restored with `cp`, verified with
`cmp`, suite back to 25 passed. So the test bites on the routing decision, not just on a string.

**Gates after this state (all by exit code):** fmt 0 · clippy `--all-targets -D warnings` 0 · **default suite 461 passed /
0 failed** (436 baseline → +19 np-core embed_run cases → +6 np-cli argv-level cases) · config-no-HOME 33 · rustdoc
`--deny warnings` 0 both ways · fences pass · **GOLDEN CHECK: pass** · feature-on check/clippy(np-core+np-cli)/doc 0.

**Still open after this round:** D-4 (the prefix spelling lives in both `embed_run` and `LocalModelProvider::model_inputs`),
D-5 (budget refusals are inherited from the runtime's error, which the module doc must state plainly), README not listing
`np embed`, design §6 still naming the non-existent `np models fetch`, `todo.md` and the `EmbeddingsConfig.model`/`.dim`
comments claiming nothing reads them, and R6c2 (real runtime wiring, artifact-gated vectors, batch invariance under a
non-empty `prefix_doc`).

**Orchestration note, because it is now three for three:** every implementer round in this session has died at its deadline
— 30 min twice, then 25 min — while committing checkpoints only when told to. The rounds are not too big conceptually; they
are paying for full-workspace gates (clippy `--all-targets`, `cargo test --workspace`, four doc builds) inside the same
window as the code. Next round starts with the smallest falsifiable change committed first, and the gate sweep moves to the
controller.

---

## 2026-10-31 — review of the round, performed by the controller (conflict disclosed)

**Both scoped reviewers timed out at their 24-minute deadlines having written nothing** (`5f832231`, `65f20efe`; 72 and 65
messages each, median inter-message gap 0 s but p95 ≈ 100 s and max ≈ 255 s — the deep-thinking turns are what the budget
goes on, not the tool calls). That is five consecutive timeouts this round. Rather than spend a sixth window, I reviewed my
own work — **disclosing that I authored or committed most of the range under review** — and compensated with the one thing
a shell-less reviewer cannot do: run the mutations. Everything below is measured, nothing is asserted from reading.

**Mutations run against `crates/np-core/src/embed_run.rs` / `db.rs`, restored with `cp`, verified with `cmp`:**

| Mutant | Result | Killed by |
| --- | --- | --- |
| M1 — drop §6's liveness fence (`WHERE f.state = 'ok'` → `WHERE 1 = 1`) | killed, 18/1 | `a_stale_note_s_chunks_are_never_owed` |
| M2 — stamp a prefix-free digest (hash the chunk text, not what the model is handed) | killed, 17/2 | `a_changed_prefix_makes_a_stored_vector_stale`, `every_text_in_a_batch_travels_with_its_own_prefix` |
| M3 — count a `SQLITE_BUSY` batch as embedded (both call sites) | killed, 18/1 | `a_busy_batch_stamps_nothing_and_is_reported` |
| **M4 — delete `paired`'s one-for-one length arm** | **survived all 19 cases** | new case below |
| **M5 — delete `paired`'s per-vector width arm** | **survived all 19 cases** | new case below |

M4 is the serious one: with that arm gone, a provider answering N texts with N−1 vectors lets `zip` pair the first N−1
silently, stamp them current, and nothing ever re-embeds them. M5's sentence ("… produced X dimensions but this run stores Y;
refused before any write") existed with no fence behind it — the existing wrong-width case refuses *earlier*, on declared
width, so it never reaches the loop. Both arms are now covered by two new cases (`…answering_the_wrong_number_of_texts_writes_nothing`,
`a_vector_wider_than_the_provider_declares_is_refused_before_any_write`), each verified to fail its mutant and pass clean
code: **21 passed** in `tests/embed_run.rs`, default suite **463 passed / 0 failed**, goldens unchanged, fmt/clippy/rustdoc 0.

**What I checked and found correct (static reading, corroborated where a mutation could reach it):** `vectors_for` filters
on an explicit model id everywhere and has no un-filtered sibling; `live_chunks` joins through `notes.rel_bytes = files.rel_bytes`
(the byte key, F13) rather than the display spelling; the digest written is the digest compared, and neither path reads
`chunks.source_hash` or `files.hash`; arm (iii) is evaluated *after* the currency read on purpose (§6's exit state (i) owes
nothing to a model that has nothing to do), and both routes into it share one report literal so the two halves cannot grow
different sentences; `install_hint` names `np models install` with its real flags and says fetching is unimplemented; the
over-budget path retries per item rather than truncating, and keeps the token numbers for the report.

**Findings carried forward, unchanged from before:** D-4 (`prefix_doc` spelling lives in both `embed_run` and
`LocalModelProvider::model_inputs` — M2 shows a drift here would be caught by two tests, but it is still one rule written
twice), D-5 (budget refusals are inherited from the runtime's error; the module doc says so, and no code in this round
prices tokens), plus README/§6/`todo.md` updates. **Not covered by anything:** §7's batch invariance and unit-norm checks
against a real model — those need R6c2's artifact-gated path, and no amount of fake-provider coverage substitutes for them.

**Independent review still outstanding.** If one is wanted, scope it to one question per child with a hard tool budget; the
briefs that produced nothing asked each reviewer to cover five areas in 24 minutes.
