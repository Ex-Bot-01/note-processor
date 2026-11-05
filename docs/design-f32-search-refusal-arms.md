# F32 — every reason `np search` cannot embed a query gets its own sentence

Branch `f32/search-refusal-arms`. Backlog: `docs/backlog-m2-followups.md` §F32. Design written 2026-11-05; every
message quoted below was produced by running `target/debug/np` on a scratch vault on this host, in both the default
and the `local-model` build.

## 1. The symptom, measured

Scratch workspace: one note, scanned, `[embeddings] provider = "local-model"`, `model = "embeddinggemma-2"`, and **no
`models` row** (a fresh data directory — the state a reader is in after editing config by hand before installing
anything):

```
$ np search <vault> alpha
np: embedding disabled: embeddings.provider = "none"          exit 1
```

The config says `local-model`. The sentence describes a configuration the user does not have, and tells them to change
a setting that is already right. Measured identically in **both** builds (`np search … | head -2`, 2026-11-05):

| State | Sentence today | Verdict |
| --- | --- | --- |
| no `[embeddings]` section (`provider = "none"`) | `embedding disabled: embeddings.provider = "none"` | true, but names nothing to change |
| `provider = "local-model"`, `model` unset | `configuration error: config io: embeddings.provider = "local-model" needs embeddings.model to name an installed model; none is set` | **accurate — and it comes from `Config::load`, so search never runs** |
| `provider = "local-model"`, model named, no row installed | `embedding disabled: embeddings.provider = "none"` | **false. This is F32** |
| row installed, artifact file absent | `no embedding provider for this workspace: its model artifact is absent, so the query cannot be embedded` | accurate, pinned by `search_with_the_artifact_file_missing_says_the_query_cannot_be_embedded` |
| row + artifact present, default build (no runtime linked) | `the local embedding runtime is not built yet: model \`embeddinggemma-2\` was asked to embed and produced nothing` | accurate, pinned by `searching_without_the_runtime_names_the_model_that_stayed_owed` |

Two more facts measured while writing this, because they shrink the fix:

- **`model` unset is refused at config load**, so `embed_provider`'s "no model named" placeholder arm cannot be reached
  through a config file. It stays defensive and is pinned as such.
- **A row with a width outside 1..=65535 cannot be installed**: `np models install --dim` validates at argv
  (`--dim <DIM> Declared vector width, 1 to 65535 — declared here, never probed`). So the "declared width is not this
  run's number" placeholder is reachable only with a hand-written row — a unit-test arm, stated as one.

## 2. Why it happens

`embed_provider` (`crates/np-cli/src/main.rs:507`) resolves `[embeddings]` into `Resolved { provider, artifact_check,
workers }`, and hands over [`NoEmbeddingProvider`] as a **placeholder** for four states — config-disabled, model-unnamed,
no row, unusable declared width — while returning `provider: None` for exactly one state: the row exists and its artifact
file is not there.

That asymmetry is deliberate and correct **for `np embed`**: `embed_run` derives each refusal arm from configuration plus
the `models` table — the sources that hold the fact — and its arm (ii) returns before anything reads a provider's width,
which is why R6c1 D-2's "placeholder declares 0 dimensions" trap does not apply to it. `search_workspace` has no such
derivation: it special-cases `provider == None` (the artifact sentence) and otherwise calls the provider, so every
placeholder falls through to the trait's generic refusal text — `EmbedError::Disabled`, which reads
`embedding disabled: embeddings.provider = "none"`.

The two no-provider arms resolve differently and therefore **report** differently. That is the defect, not the wording:
one code path (`search`) has its own copy of "why is there no provider", and that copy only knows one of the five answers.

## 3. Decision: carry the reason out of resolution as a value

`Resolved` gains a state that names *why* there is no usable provider, and `search` matches on it exhaustively. The
sentences live in np-core next to `EmbedOutcome`, which already carries `label()` + `detail()` for the same question on
the embed side.

```rust
/// Why this workspace has no usable embedding provider, as decided where the decision is made: `[embeddings]` and the
/// `models` table. `np embed` derives its own arms from those sources; `np search` reads these.
pub enum NoProvider {
    ConfigDisabled,
    ModelUnnamed,
    RowMissing { id: String },
    RowDimUnusable { id: String, declared: String },
}

/// What `[embeddings]` resolved to. One source of truth for "can this run embed?", so `search` cannot drift from
/// `embed_provider` the way a second `if provider.is_none()` did.
pub enum ProviderState {
    Ready(Box<dyn Embedder>),
    /// A placeholder that refuses before touching a model: what `embed_run` wants, and why the reason rides along.
    Placeholder { embedder: Box<dyn Embedder>, reason: NoProvider },
    /// The row is installed and its file is not. Not a configuration state — a filesystem fact only this function can see.
    ArtifactAbsent,
}
```

Rejected alternatives, with the reason each was rejected rather than silenced:

- **Reword `EmbedError::Disabled`.** That text is also `embed`'s arm (i) sentence and `LocalModelProvider`'s refusal; a
  message that fits "no row installed" would then be wrong for the two callers where it is currently right.
- **Let `search` re-derive the reason** from config and the database. That is a second copy of resolution rules — the
  exact drift this round exists to remove, and it would need to keep pace with `embed_provider` forever.
- **Match on the message string.** A refusal classified by prose is a classification that changes when someone fixes a
  typo; §12's refusals carry a class, never a guess.

## 4. Semantics per arm, and where each is pinned

| Arm | New sentence (`np: ` prefix added by the printer) | Names what to change | Pinned by |
| --- | --- | --- | --- |
| `provider = "none"` | `embedding disabled: embeddings.provider = "none" — set it to "local-model" and name an installed model with embeddings.model, then run np embed` | yes | CLI case (exact line) + existing substring assertion |
| `model` unset | unchanged: refused by `Config::load`, search never runs | yes (already) | existing config suite; **no new sentence invented here** |
| no row for the named id | `no model row for "<id>" in this workspace: see np models list, then install the artifact with np models install or fetch it with np models fetch` | yes | **new CLI case — red before this round, green after** |
| row installed, file absent | unchanged sentence, plus ` (install it with np models install, or fetch it with np models fetch)` | now yes | existing CLI case (`contains("artifact") && contains("absent")`, plus its no-double-space guard) |
| row + file present, default build | unchanged: the provider's own runtime-not-built refusal, naming the model id | already names it | existing CLI case |
| declared width unusable / `model` unnamed (defensive) | `the models row for "<id>" declares a vector width this build cannot use ("<declared>")` / `embeddings.provider = "local-model" needs embeddings.model to name an installed model` | yes | unit tests on the mapping, **labelled defensive**: §1 measures why no CLI case can reach either one |

Rules these sentences obey, unchanged from the rest of the project: a refusal carries a class and the local names needed
to act on it, never a URL (§12 rule 8); exit status stays 1 for a runtime refusal and 2 stays clap's (`np search` with
no query is clap's business, already pinned); `--json` refuses on stderr with the same sentence and writes nothing to
stdout — measured today (`search … --json` → exit 1, one stderr line, empty stdout), so this round changes no schema.

## 5. Shape of the change

- `np-core`: `NoProvider` + `ProviderState` in `embed_run.rs` beside `EmbedOutcome`, with `fn sentence(&self) -> String`
  on `NoProvider`. No behaviour in `embed_run::embed_with_resolution`; it keeps taking `Resolution`.
- `np-cli`: `Resolved { state: ProviderState, artifact_check, workers }` replaces `provider: Option<…>`;
  `embed_workspace` maps `Ready`/`Placeholder` → `Resolution::Provider`, `ArtifactAbsent` → `Resolution::ArtifactAbsent` —
  **the same two arms it produces today**, so embed's report cannot change by construction.
- `search_workspace`: one `match` over `ProviderState` with no `_` arm, so adding a state later fails to compile here
  instead of printing the wrong sentence again.

## 6. Test plan

| Case | Runs on | Asserts |
| --- | --- | --- |
| `search_with_no_row_names_the_missing_row_and_says_what_to_run` (new) | every runner, both builds | exit 1; stderr **equals** `np: ` + the row-missing sentence with the configured id in it; stdout empty. This is the case that fails on today's code — quoted red output goes in the commit message |
| `search_with_embedding_disabled_refuses_and_names_the_setting` (existing, strengthened) | every runner | keeps its `embeddings.provider = "none"` substring, now against the **whole line**, so a reword that drops the fix-hint fails too |
| `search_with_the_artifact_file_missing_says_the_query_cannot_be_embedded` (existing) | every runner | unchanged assertions, plus the pointer sentence; its `!contains("   ")` guard stays — it is what caught a mid-sentence wrap in Task 6 |
| `no_provider_sentences_are_one_line_each` (new, unit) | every runner | every `NoProvider::sentence()` contains no `\n` and no run of two spaces — the wrap class, pinned for arms no CLI case can reach |
| `the_defensive_arms_say_what_they_are` (new, unit) | every runner | `ModelUnnamed` / `RowDimUnusable` sentences name the id and the declared width; comment records §1's measurement of why argv cannot produce them |
| existing embed suites (`tests/embed_run.rs`, `tests/cli.rs` embed cases) | every runner | unchanged: they are the proof the `ProviderState` mapping did not move an embed arm |

Nothing here needs a model artifact, so no case prints `NOT COVERAGE`; the artifact-gated suites stay as they are.

## 7. What this round does not claim

- Nothing about whether search *finds the right note* — that is F31 and still owes a labelled set.
- No Windows or macOS execution of any of this: the sentences are printed by code paths that run on every platform, but
  only ubuntu and windows runners exist here, and no runner has a macOS job (F18).
- The `local-model` build's successful search path is unchanged and stays covered only by the artifact-gated cases; this
  round touches resolution, not ranking.
- Whether `Config::load`'s refusal for `model` unset should move into resolution is **not** decided here — it is accurate
  where it is, and moving it would change config-error text that other suites pin.

## 8. Non-goals

No new exit code; no JSON key added or renamed; no change to `embed_run`'s arms, counts or labels; no rewording of
`EmbedError::Disabled`; F33 (llama.cpp's stderr volume) stays a separate round because it changes when the model loads,
not what a refusal says.

## 9. Risks, and what retires each

| Risk | Retired by |
| --- | --- |
| The `ProviderState` mapping quietly moves an embed arm | `embed_workspace` produces exactly today's two `Resolution` values; the whole embed suite is in the gate list, and one CLI case re-pins the no-row embed sentence verbatim |
| A new sentence wraps mid-line like Task 6's did | the unit test on every sentence + the existing CLI guard against three consecutive spaces |
| Pinned substrings break for reasons that are not defects | the two existing search cases are updated in the same commit as the wording, and both keep their original assertions plus the new ones — nothing is deleted to make a test pass |
| A defensive arm ships whose sentence nobody has read | both are printed by the same `sentence()` the unit tests assert, and §4 labels them defensive with the measurement that keeps them unreachable |

## 10. Rollout

1. Branch `f32/search-refusal-arms` off `main`.
2. Write the failing CLI case first (no row → row-missing sentence), record its red output.
3. Implement §5; re-run the case green, then the full local gate list — fmt; clippy at default, `local-model` (core and
   cli) and `local-model,vision` (core); `-p np-cli --no-default-features` for clippy and tests; workspace tests; config
   tests without `$HOME`; rustdoc `--deny warnings` at three levels; both cfg-integrity scripts; goldens.
4. Push the branch **before asking to merge** — F34's first push proved a locally green tree can still fail the Windows
   row, and this change touches code both builds compile.
5. Reviewers (design, then code); fix loop; re-measure every claim in §1 and §4 against the new binary, not against this
   document.
