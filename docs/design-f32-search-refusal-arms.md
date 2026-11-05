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
the `models` table — the sources that hold the fact. **[review]** With a limit, because the first draft claimed too much here:
arm (ii) returns before anything reads a provider's width **for the three states it takes** (config-disabled, model-unnamed,
no row). The fourth placeholder state — a row whose declared width is not storable — does *not* take arm (ii): the run reaches
the width comparison at `embed_run.rs:546-566` and prints "the model declares `<row.dim>` dimensions but this run needs 0"
against a placeholder. That sentence is embed's problem, not search's, and §8 keeps it out of scope; what it costs this design
is honesty in §4's wording for that arm.

`search_workspace` has no such derivation: it special-cases `provider == None` (the artifact sentence) and otherwise calls the
provider, so every placeholder falls through to the trait's generic refusal text — `EmbedError::Disabled`, which reads
`embedding disabled: embeddings.provider = "none"`.

The two no-provider arms resolve differently and therefore **report** differently. That is the defect, not the wording:
one code path (`search`) has its own copy of "why is there no provider", and that copy only knows one of the five answers.

## 3. Decision: carry the reason out of resolution as a value

`Resolved` gains a state that names *why* there is no usable provider, and `search` matches on it exhaustively. The
sentences live in np-core next to `EmbedOutcome`, which already carries `label()` + `detail()` for the same question on
the embed side.

```rust
// np-core, beside `EmbedOutcome`, whose `label()/detail()` set the precedent: sentences nothing in np-core prints.
pub enum NoProvider {
    ConfigDisabled,
    ModelUnnamed,
    RowMissing { id: String },
    /// A row whose declared width this run cannot store. Reachable only with a hand-written row (§1), so its wording
    /// follows embed's own vocabulary ("this run needs") rather than blaming the build.
    RowDimUnusable { id: String, declared: String },
}

impl NoProvider {
    /// The one-line remedy. `workspace` is needed because the pointer names argv a human can paste — and it is
    /// [`embed_run::install_hint`], the already feature-gated wording embed's arm (iii) prints.
    pub fn sentence(&self, workspace: &Path) -> String;
}

// np-cli, beside `Resolved` and `embed_provider`: the state is what *this* function decided, and putting it in np-core
// would leave two overlapping "provider-or-artifact-absent" enums whose mapping is the drift class §3 removes.
pub enum ProviderState {
    Ready(Box<dyn Embedder>),
    /// A placeholder that refuses before touching a model: what `embed_run` wants, and why the reason rides along.
    Placeholder { embedder: Box<dyn Embedder>, reason: NoProvider },
    /// The row is installed and its file is not. Not a configuration state — a filesystem fact only this function can see.
    ArtifactAbsent,
}
```

**[review]** Placement split this way on the design review's argument, which I checked rather than accepted: `NoProvider` +
`sentence()` in np-core because np-core already owns printed sentences (`EmbedOutcome::label/detail`) and a caller-supplied fact
enum (`Resolution`); `ProviderState` in np-cli because it is the shape of one function's decision and nothing outside this crate
constructs or matches it.

Rejected alternatives, with the reason each was rejected rather than silenced:

- **Reword `EmbedError::Disabled`.** **[review]** The first draft justified this by claiming the text is also embed's arm (i)
  sentence and `LocalModelProvider`'s refusal — both wrong, and §1 already quotes the second one: embed's configuration arm comes
  from `EmbedOutcome::label()/detail()` (`embed_run.rs:142-160`) and is arm **(ii)**, while a default build's local provider
  refuses with `RuntimeNotBuilt`. The real reason to leave it alone is that one trait refusal covering four distinct states is
  what produced F32; giving it five wordings inside one message repeats the mistake at a different level. After this round no
  production path prints it (see §5's last bullet), and `embed.rs:565-568` says so.
- **Let `search` re-derive the reason** from config and the database. That is a second copy of resolution rules — the
  exact drift this round exists to remove, and it would need to keep pace with `embed_provider` forever.
- **Match on the message string.** A refusal classified by prose is a classification that changes when someone fixes a
  typo; §12's refusals carry a class, never a guess.

## 4. Semantics per arm, and where each is pinned

| Arm | New sentence (`np: ` prefix added by the printer) | Names what to change | Pinned by |
| --- | --- | --- | --- |
| `provider = "none"` | `embedding disabled: embeddings.provider = "none" — set it to "local-model" and name an installed model with embeddings.model, then run np embed` | yes | CLI case, whole line against a **literal**, plus exactly-one-stderr-line |
| `model` unset | unchanged: refused by `Config::load`, search never runs | yes (already) | existing config suite; **no new sentence invented here** |
| no row for the named id | `no model row for "<id>" in this workspace: <install_hint>` | yes | **new CLI case — red before this round, green after** |
| row installed, file absent | unchanged first clause, then `: <install_hint>` | now yes | existing CLI case, plus a new `contains("np models install")` so the pointer cannot be deleted silently |
| row + file present, default build | unchanged: the provider's own runtime-not-built refusal, naming the model id | already names it | existing CLI case |
| declared width unusable (defensive) | `the row for "<id>" declares a width this run cannot store ("<declared>")` — **[review]** not "this build cannot use": the value is not build-dependent, and embed's own arm (iv) sentence for the same state says "this run needs" (§2) | yes | unit test on `sentence()`, **labelled defensive**: §1 measures why no CLI case can reach it |
| `model` unnamed (defensive) | `embeddings.provider = "local-model" needs embeddings.model to name an installed model; none is set` — the same words `Config::load` uses, so the two places cannot disagree | yes (names the key, not an id: there is no id) | unit test on `sentence()`, labelled defensive |

**[review] `<install_hint>` is `embed_run::install_hint(workspace)`, reused rather than re-worded**, because it already exists in
two feature-gated halves (`embed_run.rs:1231-1250`): a `fetch` build is told to fetch or install with full argv, and a
`--no-default-features` build is told the truth for its own shape ("this build has no fetch command"). The first draft of this
table invented prose that named `np models fetch` unconditionally — in a build where that subcommand is compiled out of the help
table (`np-cli/Cargo.toml:21-25`, `main.rs:136`), which is the exact defect `cli.rs:1658-1673` pins both ways for embed. Reusing
one definition also means this round cannot drift from it.

One honesty note about the pointer, recorded rather than smoothed: `install_hint` tells a reader to run `np models fetch <ws>
--id <id>`, and that command refuses with `NothingPinned` for any id this build does not pin — §12 rule 7 was amended
2026-11-04 so the *origin* is compiled in (`pins::DEFAULT_ORIGIN`), which means the risk is the id, not the origin. Fixing that
hint's wording is not this round's: it is pinned by two existing cases and shared with embed.

Rules these sentences obey, unchanged from the rest of the project: a refusal carries a class and the local names needed
to act on it, never a URL (§12 rule 8); exit status stays 1 for a runtime refusal and 2 stays clap's (`np search` with
no query is clap's business, already pinned); `--json` refuses on stderr with the same sentence and writes nothing to
stdout — measured today (`search … --json` → exit 1, one stderr line, empty stdout), so this round changes no schema.

## 5. Shape of the change

- `np-core`: `NoProvider` in `embed_run.rs` beside `EmbedOutcome`, with `fn sentence(&self, workspace: &Path) -> String`, and
  `install_hint` made `pub` (its two feature-gated halves stay exactly as they are). No behaviour in
  `embed_run::embed_with_resolution`; it keeps taking `Resolution`.
- `np-cli`: `enum ProviderState` beside `Resolved`; `Resolved { state, artifact_check, workers }` replaces
  `provider: Option<…>`; `embed_workspace` maps `Ready`/`Placeholder` → `Resolution::Provider`,
  `ArtifactAbsent` → `Resolution::ArtifactAbsent` — **the same two arms it produces today**, so embed's report cannot change by
  construction. **[review]** Mechanically that means destructuring `Resolved` first: today `main.rs:342` works because it *moves*
  `resolved.provider` out of the struct, and with the state inside the same value the borrow for `Resolution::Provider(… as_mut)`
  has to be taken from the destructured binding, not from a temporary.
- `search_workspace`: one `match` over `ProviderState` with no `_` arm, so adding a state later fails to compile here instead of
  printing the wrong sentence again. **[review]** All four placeholder reasons collapse to **one** arm —
  `Placeholder { reason, .. } => Err(reason.sentence(workspace).into())` — because a per-state match would create four sites whose
  mapping nothing can test: §1 proves argv cannot reach two of them, so a swap or a hardcoded string in one of those arms would
  survive every case in §6. Sentence-level pinning is the strongest claim available for defensive arms and §6 says exactly that.
- **[review] Documents that go false if nobody edits them** (rustdoc will not notice, and no test pins prose):
  `main.rs:384-387` ("`embed_provider` answers `None` when the artifact is absent"), `main.rs:475-483` (`Resolved.provider`
  "or `None` for §6's arm (iii)"), `main.rs:493-505` (the whole "`None` means exactly that one thing" rule, now expressed by
  `ProviderState::ArtifactAbsent`), `embed_run.rs:438-442` ("np-cli still hands over `NoEmbeddingProvider` for those states"), and
  `README.md:329-330` — the troubleshooting row that documents F32's wrong message as a defect, plus the artifact row whose text
  this round appends to. All five are edited in the implementation commit.
- **[review] And one deliberate decision about `EmbedError::Disabled`:** after this change no production path prints it — every
  placeholder state refuses in `search_workspace` before `embed_query`, and embed's arms come from `embed_run`. The override on
  `NoEmbeddingProvider::embed_query` (`embed.rs:565-570`) **stays**, because a provider asked to embed while embedding is off still
  has to refuse with the setting's name rather than "this provider cannot", and its np-core tests exercise it directly; what changes
  is its justification, which currently reads "`np search` prints this text verbatim" and becomes false.

## 6. Test plan

| Case | Runs on | Asserts |
| --- | --- | --- |
| `search_with_no_row_names_the_missing_row_and_says_what_to_run` (new, red today) | every runner, both builds — and on `windows-latest`, which runs `cargo test --workspace` | exit 1; the refusal is **exactly one stderr line**, matched against a literal prefix (`np: no model row for "tiny-model@1" in this workspace:`) rather than against `sentence()`; names the id; does **not** contain `embeddings.provider = "none"`; `--json` variant refuses the same way with empty stdout; stdout empty |
| `search_with_embedding_disabled_refuses_and_names_the_setting` (existing, strengthened) | every runner | keeps its substring assertion and adds whole-line equality **against a literal** — building the expected string from `NoProvider::sentence()` would be self-fulfilling and would not catch the reword this case exists to catch |
| `search_with_the_artifact_file_missing_says_the_query_cannot_be_embedded` (existing) | every runner | unchanged assertions plus `contains("np models install")`, so the appended pointer cannot be deleted silently; its `!contains("   ")` guard stays — it is what caught a mid-sentence wrap in Task 6 |
| `no_provider_sentences_are_one_line_each` (new, unit) | every runner | every `NoProvider::sentence(ws)` contains no `\n` and no run of two spaces — the wrap class, pinned for arms no CLI case can reach |
| `each_defensive_arm_names_its_own_cause` (new, unit) | every runner | **[review]** split, because one assertion asked for a field the enum does not have: `ModelUnnamed` names `embeddings.model` and nothing else; `RowDimUnusable { id, declared }` names **both** the id and `<declared>`. Comment records §1's measurement of why argv cannot produce either |
| `a_no_fetch_build_names_no_fetch_command` (existing pattern extended) | `-p np-cli --no-default-features` | the row-missing refusal must not contain `np models fetch` in that build — same `cfg!(feature = "fetch")` shape as `cli.rs:1658-1673`, which is why §4 reuses `install_hint` |
| existing embed suites (`tests/embed_run.rs`, `tests/cli.rs` embed cases) | every runner | unchanged: they are the proof the `ProviderState` mapping did not move an embed arm. **[review]** Stated at its real strength: the no-row *embed* case asserts two `contains`, so it catches a moved arm, not a reworded one |

Nothing here needs a model artifact, so no case prints `NOT COVERAGE`; the artifact-gated suites stay as they are.

**Assertions deliberately not built from the code under test.** The strengthened provider-off case compares against a
literal sentence, and the new row-missing case against a literal prefix: an expectation assembled from
`NoProvider::sentence()` would pass whatever the sentence became, which is the one thing a message test exists to prevent.
"Exactly one stderr line" replaces whole-buffer equality because `Db::open` writes orphan and merge receipts to **stderr**
(`np-core/src/db.rs:688-691`) whenever a run's first open does that work: today the preceding `scan` consumes them, so a case
that asserts whole-buffer equality would be asserting an accident of test order. One line is the claim that stays true — and a
future case that skips the scan fails loudly instead of comparing receipts.

## 7. What this round does not claim

- Nothing about whether search *finds the right note* — that is F31 and still owes a labelled set.
- **[review]** Restated after review, because the first draft was wrong in the pessimistic direction: the new default-build cases
  **do** execute on `windows-latest` (`ci.yml` runs `cargo test --locked --workspace` there, which is how F34's Windows evidence
  arrived). What has no runner is a Windows **`local-model`** build (`ci.yml:52-60` is ubuntu-only) and any macOS test job at all
  (`release.yml:35` builds, never tests). So "the row-missing sentence on NTFS" is evidence the CI run will produce; "…with a real
  model loaded" and "…on macOS" are not.
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
| The `ProviderState` mapping quietly moves an embed arm | Two mutants, run rather than argued. **Mapping `Ready` to `ArtifactAbsent` is killed** by `embed_with_everything_present_reports_the_runtime_that_is_not_built`. **Mapping `Placeholder` to `ArtifactAbsent` survived — and is an equivalent mutant, recorded not dropped:** arm (ii) returns from configuration and the `models` table at `embed_run.rs:586-603`, before anything reads the resolution, so for every placeholder state argv can reach the embed report is identical either way. The mapping only carries information in the defensive width state, which §1 shows argv cannot produce |
| A reason gets wired to the wrong state | killed by mutants, both measured: `RowMissing` resolved as `ConfigDisabled` fails the new CLI case (it prints the provider sentence and the assertion quotes it back); artifact-absent resolved as `RowMissing` fails the existing artifact case. Both files restored with `cmp` after |
| A new sentence wraps mid-line like Task 6's did | the unit test on every sentence + the existing CLI guard against three consecutive spaces |
| A refusal names a command this build does not have | §4 reuses `install_hint`, whose two halves are already pinned in both directions (`cli.rs:1658-1673`), plus a no-fetch assertion on the new case |
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
