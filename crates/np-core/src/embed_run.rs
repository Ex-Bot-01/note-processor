//! `np embed`'s work: read the scan's chunks, hand them to a provider, store the vectors.
//!
//! Design authority is `docs/design-m2-local-embeddings.md` **§6** (the command), **§7** (verification and the
//! caveat line) and **§9.5**. This module holds every decision behind `np embed`; `np-cli` parses argv and
//! prints, exactly as it does for [`scan`](mod@crate::scan). *(The `mod@` is needed here and nowhere else in this
//! crate: `lib.rs` re-exports the **function** `scan` at the root beside the module of the same name, so a bare
//! `[`crate::scan`]` in a module header is ambiguous to rustdoc — which fails the `--deny warnings` doc gate.)*
//!
//! What it is, in one paragraph: read the chunks the scan already cut, decide which of them are *owed* a
//! vector for the configured model, hand the owed ones to an [`Embedder`] in bounded batches, and store what
//! comes back — one short transaction per batch, never one per corpus, and never any model work inside a
//! transaction (§6). It reads chunking and never writes it: no `chunks` row is created or restamped here, and
//! [`crate::scan::EMBED_VERSION_KEY`] stays the scan's to record.
//!
//! Four rules are load-bearing, and each is where it is because §6 put it there:
//!
//! - **Currency is per-row and liveness-fenced** ([`db::live_chunks`] joins `files.state = 'ok'`, backlog D3):
//!   a stale note's chunks are never owed, so this command cannot be the thing that computes vectors for a
//!   deleted file's text and reports them current.
//! - **The stamp is the digest of the bytes the model will read** (backlog D4). For each owed chunk the provider
//!   is asked [`Embedder::model_inputs`] and the digest covers *that* string, while [`Embedder::embed`] receives the
//!   raw chunk text: the provider owns how a chunk is dressed, so it is the only witness that can say what was read.
//!   Spelling the rule here as well is R6c2's P1 — `prefix_doc` applied twice, once by this module and once by
//!   `local_model::LocalEmbedder` (behind the `local-model` feature) — which left the stamp covering one copy of a string the model saw twice.
//!   Changing `prefix_doc` still makes every stored vector stale, which is the point: a vector made from different
//!   bytes is not the same fact.
//! - **`SQLITE_BUSY` is a reported gap** ([`EmbedReport::busy_gap`], non-zero exit), never a silent skip; the
//!   batch that met the lock stamps nothing, and [`embed_with_inject`]'s hook is what lets a test prove an
//!   embed holds no write lock *between* batches.
//! - **Over-budget text is reported, never truncated** ([`EmbedReport::refused_budget`]): a truncated chunk
//!   would stamp a whole chunk's digest onto a vector that read part of it (§5).
//! - **Counting tokens is not this module's job.** Nothing here measures a sequence against
//!   [`Config::embeddings`](crate::config::EmbeddingsConfig)::`max_tokens`; the number comes from the provider's own
//!   [`EmbedError::TokenBudgetExceeded`], because pricing a sequence needs the model's tokenizer and that lives behind
//!   `local-model`. A fake provider therefore cannot make this module *disagree* with §5's budget — it can only relay
//!   one (R6c1 review D-5, stated here rather than left to inference).
//!
//! ## The four exit arms, and one refusal that rides with the last of them
//!
//! [`EmbedOutcome`] carries §6's states verbatim: (i) nothing owed → 0, (ii) *configuration disabled* → 1
//! before anything is read or written, (iii) artifact absent → 1 naming a command that exists, (iv) owed and
//! not produced → 1, the only arm that means "gave up". §6 lists three of these and §9's table says four; this
//! build implements four, as the round brief directs. No new exit codes exist here: every non-zero arm is
//! `ExitCode::FAILURE`, and argv problems stay clap's 2.
//!
//! A caller that could not build a provider says so through [`Resolution`] rather than handing over something
//! that stands in for a provider. Which arm a state belongs to stays this module's decision — the caller
//! supplies a fact, not a verdict — and [`Resolution::ArtifactAbsent`] exists because arm (iii) is the one
//! state the caller knows and this module cannot recover from configuration.
//!
//! A wrong-dim provider is refused **before any write** naming both numbers, as §6 requires — but it is not a
//! fifth arm. It leaves as [`EmbedRunError::ProviderDimMismatch`], which np-cli prints on stderr with exit 1,
//! because "this run cannot be served at all" is a refusal rather than a state of the corpus.
//!
//! ## What is deliberately *not* here
//!
//! No model loader and no artifact reader: the provider arrives injected, so every case in
//! `tests/embed_run.rs` runs against a fake with no feature flag and no 310 MB file. In a **default** build the
//! provider `np-cli` resolves is [`crate::embed::LocalModelProvider`], which answers every batch with
//! [`crate::embed::EmbedError::RuntimeNotBuilt`] — arm (iv), honestly, with nothing written. With `local-model` it
//! resolves `local_model::LazyLocalEmbedder` instead, and that is the provider real vectors come from: the artifact
//! is loaded on the first batch, after the currency query has said something is owed.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Transaction;
use serde::Serialize;
use thiserror::Error;

use crate::config::{abs_path, Config, ConfigError, EmbeddingProvider};
use crate::db::{self, Db, DbError};
use crate::embed::{EmbedError, Embedder, ImageBlocker, Vector};
use crate::hash::{sha256_bytes, sha256_reader_with_size};
use crate::media::{self, MediaRefusal};
use crate::models;
use crate::walk;

/// How many owed chunks one provider call — and therefore one write transaction — covers.
///
/// §6 asks for "bounded transactions — one per batch, not one per corpus" without naming a size, so this is a
/// scope bound rather than a tuned number: small enough that a batch's transaction is over before another
/// process's `busy_timeout` runs out, large enough that a provider call amortises over several texts. Nothing
/// in the schema or the report depends on it, and R6d's throughput benchmark (§8) is the round that would
/// change it with a measurement.
pub const EMBED_BATCH_CHUNKS: usize = 8;

/// §7's caveat line, verbatim: `self-consistent, not verified`.
///
/// **It is unconditional in this build because no reference golden exists to make it conditional.** §7's three
/// primary checks (batch invariance, unit norm, cross-quantisation agreement) all need the real runtime and
/// artifact, i.e. R6c2 and R6d; until one of them has produced a golden, every report this module writes
/// describes self-consistency only — determinism is not correctness, and §7 says the caveat "must not live in a
/// design doc". A build that *did* have a golden would make this a field rather than a constant; nothing today
/// can tell it otherwise, so claiming "verified" anywhere would be the false green §7 exists to prevent.
pub const CAVEAT: &str = "self-consistent, not verified";

/// Why this run stopped where it did — §6's four exit states, one variant each.
///
/// The labels are what a person reads, so they are printed by np-cli rather than inferred from a count: arm
/// (ii) is documented as "the default state of a fresh install … must not read as a fault", and the only thing
/// that keeps it from reading as one is the words *configuration disabled*.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EmbedOutcome {
    /// Arm (i): after this run no chunk is owed a vector for the configured model. True both when nothing was
    /// ever owed and when everything owed was embedded in this run — [`EmbedReport::embedded`] says which.
    Current,
    /// Arm (ii): configuration made embedding impossible *before* anything was read or written. `key` is the
    /// config key to edit and `id` the value it holds (absent when the key itself is missing).
    ConfigurationDisabled {
        /// The key a human has to change: `embeddings.provider` or `embeddings.model`.
        key: &'static str,
        /// What that key currently says, or `None` when it names nothing at all.
        id: Option<String>,
        /// **F41 option A.** A command that fixes it, present only for the half of this arm where configuration is
        /// *correct* and the artifact has simply never been installed here — `model` naming no row. The other half,
        /// `provider = "none"`, gets `None`: a switch that was thrown deliberately is not an invitation to download
        /// 309 MB, and an install hint there would be a sentence about a file nobody decided to want.
        ///
        /// Skipped when absent so the machine-readable shape of every existing refusal is unchanged; a reader that
        /// wants the remedy looks for `hint`, exactly as it does on [`Self::ArtifactAbsent`].
        #[serde(skip_serializing_if = "Option::is_none")]
        hint: Option<String>,
    },
    /// Arm (iii): the configured model's row exists but its artifact is not there to load. `hint` names a
    /// command that **exists** in this build.
    ArtifactAbsent {
        /// The installed row whose artifact could not be found.
        model: String,
        /// Where it was looked for.
        artifact: PathBuf,
        /// What to run instead — `np models install`, because fetching (§12's `np models fetch`) has not
        /// shipped, and §6's wording "run `np models fetch`" therefore names a command R7b owes.
        hint: String,
    },
    /// Arm (vi): fetching was allowed and needed, and **nothing was reached** (**F38** §5).
    ///
    /// The arm exists because offline is a state, not an error to be dressed up as one. `np embed` may fetch a missing
    /// pinned artifact by default; when the transport says no connection (never a probe — the attempt *is* the probe),
    /// the honest report is "nothing was embedded because nothing could be reached", at exit 1, with the counts all zero
    /// and an instruction to re-run. It is not arm (ii) — configuration is fine and did its job — and it must never be
    /// arm (iv), which would read as a vault whose vectors failed to arrive.
    ///
    /// **Exit 1 on purpose.** `np setup` treats this state as an accepted degradation and exits 0 with `vectors:false`,
    /// because its contract is "get as far as you can and tell me where you are". A caller that asked for vectors and got
    /// none must not see success — that is how an application ends up believing it has embeddings.
    Offline {
        /// The pinned model this run could not obtain.
        id: String,
    },
    /// Arm (v): this **binary** cannot embed at all — no inference runtime was compiled into it (**F40**).
    ///
    /// It exists because arm (iv) was answering here and lying about it. `(iv)` means *our accounting said vectors
    /// were owed and none arrived*, which points at chunking, stamping or the database; a build that links no C++ is a
    /// compile-time fact nobody can retry, re-scan or repair. Measured on two platforms before this arm existed — an
    /// operator installed `embeddinggemma-2` correctly (digest verified) and was told `outcome owed and not produced`,
    /// `errors 1`: a data-integrity sentence about a missing feature, after the corpus had been read.
    ///
    /// Decided **before the currency read**, so `seen` stays 0: refusing honestly should not cost a pass over live
    /// chunks, and a report that says it saw nothing is a report nobody has to reconcile with an empty write.
    RuntimeNotBuilt {
        /// The configured model that stays owed. Same id arm (ii) would name, so the two sentences agree about what
        /// is missing even though only one of them can be fixed by editing a file.
        id: String,
    },
    /// Arm (iv): chunks were owed and this run did not produce vectors for all of them. The only arm that
    /// means "gave up", and the only one whose reason can name a busy database, a token budget or a provider
    /// that refused.
    NotProduced {
        /// What stopped the work, assembled from [`EmbedReport`]'s own counts. Carries a provider's message,
        /// which [`EmbedError`] guarantees to hold no note text and no secret (§3).
        reason: String,
    },
}

impl EmbedOutcome {
    /// The arm's label, as the report prints it. Stable text: it is what a reader greps for.
    /// Every arm's detail must add something to its label rather than restate it — `np embed` composes the refusal as
    /// `"{}: {}"` of the two, and on 2026-11-08 the packaged 0.3.0 asset printed `np: offline: offline: could not reach …` because
    /// arm (vi)'s sentence began by repeating its own label. Asserting the invariant over all arms (not just fixing that one string) is
    /// what stops the next arm doing the same, and asserting the two halves separately keeps this falsifiable: a `label()` or `detail()`
    /// edit that reintroduces the echo fails here, while an edit that strips labels wholesale fails the second loop.
    pub fn refusal_names_the_state_once(&self) -> Result<(), String> {
        let label = self.label();
        let detail = match self.detail() {
            Some(text) => text,
            None => return Ok(()),
        };
        if detail.eq(label) || detail.starts_with(&format!("{label}: ")) {
            return Err(format!(
                "arm {:?} restates its own label: label={label:?} detail starts with {label:?}\n  \
                 np embed prints \"{{label}}: {{detail}}\", so that becomes the state twice on one line.",
                std::mem::discriminant(self)
            ));
        }
        Ok(())
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Current => "nothing owed",
            Self::ConfigurationDisabled { .. } => "configuration disabled",
            Self::ArtifactAbsent { .. } => "artifact absent",
            Self::RuntimeNotBuilt { .. } => "runtime not built",
            Self::Offline { .. } => "offline",
            Self::NotProduced { .. } => "owed and not produced",
        }
    }

    /// The arm's one-line detail, or `None` for arm (i), which the counts already describe.
    pub fn detail(&self) -> Option<String> {
        match self {
            Self::Current => None,
            // The key and id lead, because F32 §6's reading ("this is a configuration state") is what the label carries;
            // the hint follows after an em dash so a reader who only needs the command still finds it in one line.
            Self::ConfigurationDisabled { key, id, hint } => {
                let stated = match id {
                    Some(value) => format!("{key} = \"{value}\""),
                    None => format!("{key} names nothing"),
                };
                Some(match hint {
                    Some(hint) => format!("{stated} — {hint}"),
                    None => stated,
                })
            }
            Self::ArtifactAbsent { hint, .. } => Some(hint.clone()),
            // Two ways forward and the one fact that matters: nothing was written, so the model is still owed rather
            // than half-embedded. Names a flag that exists (`--features local-model`) and a command that exists in this
            // very binary (`np search --mode keyword`, F36) — the rule §6's hints follow, because arm (iii)'s first
            // version named `np models fetch` before it shipped.
            // The sentence must not open with its own label: `np embed` prints `"{}: {}"` of label and detail, and this arm's first
            // version produced `np: offline: offline: could not reach the pinned origin …` (measured on the packaged 0.3.0 asset, the
            // first shape where the arm is reachable in a release). Pinned by `a_refusal_never_prints_its_state_twice`.
            Self::Offline { id } => Some(format!(
                "could not reach the pinned origin to fetch `{id}`, so nothing was embedded — connect and \
                 run `np embed` again, or search meanwhile by keyword (`np search --mode keyword`)"
            )),
            Self::RuntimeNotBuilt { id } => Some(format!(
                "this binary was built without an embedding runtime: install one built with `--features local-model`, \
                 or search by keyword (`np search --mode keyword`) — `{id}` stays owed"
            )),
            Self::NotProduced { reason } => Some(reason.clone()),
        }
    }
}

/// The diagnostics rule, level and `what` for one arm of an embed refusal (diagnostics design §1 point 4, §4).
///
/// Arm (i) answers `None`: a run that produced everything it owed is not a degraded state, and an event per successful
/// embed would be the noise §4 keeps out of the log. Everything else maps one-to-one onto a published token, with two
/// readings worth stating:
///
/// * arm (ii) splits by **which key** refused — `embeddings.provider` is a switched-off feature (`provider-disabled`),
///   while `embeddings.model` naming no row is an installation gap (`model-not-installed`). One arm, two causes, and a
///   reader told to "install a model" by a workspace that has embeddings switched off would chase the wrong file.
/// * arm (iii) reports the **artifact path** rather than the model id, because its diagnosis is a filesystem question
///   (wrong data dir, unmounted volume, read-only image) and the id is in the message either way.
///
/// `model` is the configured id, used by the arms that carry no identity of their own — arm (iv) reports against the run.
fn diag_of(
    outcome: &EmbedOutcome,
    model: Option<&str>,
) -> Option<(&'static str, tracing::Level, String)> {
    use crate::diagnostics::rule;
    match outcome {
        EmbedOutcome::Current => None,
        EmbedOutcome::ConfigurationDisabled { key, id, .. } => {
            let rule = if *key == "embeddings.model" {
                rule::MODEL_NOT_INSTALLED
            } else {
                rule::PROVIDER_DISABLED
            };
            let what = match (rule, id) {
                (rule::MODEL_NOT_INSTALLED, Some(id)) => id.clone(),
                _ => (*key).to_string(),
            };
            Some((rule, tracing::Level::WARN, what))
        }
        EmbedOutcome::ArtifactAbsent { artifact, .. } => Some((
            rule::ARTIFACT_MISSING,
            tracing::Level::WARN,
            artifact.display().to_string(),
        )),
        EmbedOutcome::Offline { id } => Some((rule::OFFLINE, tracing::Level::WARN, id.clone())),
        EmbedOutcome::RuntimeNotBuilt { id } => Some((
            rule::LOCAL_MODEL_NOT_BUILT,
            tracing::Level::WARN,
            id.clone(),
        )),
        // The failure half: something this run could have done did not happen (§4 — "a state to fix" is `warn`,
        // "something failed" is `error`).
        EmbedOutcome::NotProduced { .. } => Some((
            rule::NOT_PRODUCED,
            tracing::Level::ERROR,
            model.unwrap_or("the configured model").to_string(),
        )),
    }
}

/// One refusal on the embed target, with the rule as a field rather than as prose.
///
/// `what` and `rule` are recorded as strings (not through `Debug`) so the file sink writes `"offline"` and not a
/// quoted, escaped copy of it — the published JSONL keys are an interface a reader parses.
fn emit(level: tracing::Level, rule: &'static str, what: &str, detail: &str) {
    match level {
        tracing::Level::ERROR => tracing::error!(
            target: crate::diagnostics::target::EMBED,
            rule,
            what,
            "{detail}"
        ),
        _ => tracing::warn!(
            target: crate::diagnostics::target::EMBED,
            rule,
            what,
            "{detail}"
        ),
    }
}

/// What this run decided about worker threads for the local model, carried on the report so a slow run can be
/// diagnosed from its own output.
///
/// The field exists because of a measurement, not a feature request: llama.cpp's default worker count is a compile-time
/// literal (`GGML_DEFAULT_N_THREADS`, `ggml/include/ggml.h:232`), and using all of this host's CPUs instead of it is
/// 2.49×–3.60× faster (`docs/design-m2-local-embeddings.md` §14's D13 rows). Once the count is derived from the machine,
/// "how many did that run get" stops being guessable from the source — so it is reported. `available` is the denominator
/// `available_parallelism()` answered (the CPU allowance this process may use, which follows an affinity mask), and
/// `configured` says whether the number came from the rule or from `[embeddings] threads = <n>`, because "12" printed on
/// its own would credit the operator with a choice they did not make.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WorkerPlan {
    /// Workers this run asks the runtime for.
    pub count: usize,
    /// The CPU allowance the rule was applied to.
    pub available: usize,
    /// `"auto"` when the rule chose [`WorkerPlan::count`], `"explicit"` when configuration named it.
    pub configured: &'static str,
}

impl WorkerPlan {
    /// The one-line spelling for the text report: `12 of 16 available (auto)`.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "{} of {} available ({})",
            self.count, self.available, self.configured
        )
    }
}

/// What one `np embed` run saw and did.
///
/// Six counts, in §6's order, plus the model they were counted against, the arm and §7's caveat. Every field
/// is a count of **chunks** except [`EmbedReport::busy_gap`], which counts **batches** — the transaction, not
/// the text, is the thing that met the lock, and one batch may hold several chunks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EmbedReport {
    /// The configured model id this run answered questions about, or `None` when configuration named none
    /// (arm ii before any read, so nothing was counted either).
    pub model: Option<String>,
    /// Which exit arm this report belongs to; np-cli exits 0 only for [`EmbedOutcome::Current`].
    pub outcome: EmbedOutcome,
    /// Chunks the liveness fence admitted — live notes' chunk rows, current or owed. A stale note's chunks are
    /// in neither half of this number (backlog D3).
    pub chunks_seen: usize,
    /// Chunks whose stored vector carries this run's digest for this model id.
    pub current: usize,
    /// Chunks this run embedded and stamped.
    pub embedded: usize,
    /// Chunks refused because the prefixed sequence is over `embeddings.max_tokens`. Reported, never truncated
    /// (§5), so they stay owed and the run exits non-zero.
    pub refused_budget: usize,
    /// Batches that met `SQLITE_BUSY` and stamped nothing (§6's reported gap).
    pub busy_gap: usize,
    /// Chunks or batches this run could not embed for a reason that is neither of the two above: a provider
    /// refusal, a vector of the wrong width, a chunk row whose text is not UTF-8.
    pub errors: usize,
    /// [`CAVEAT`], carried on the report so `--json` and the text summary cannot disagree about whether this
    /// run verified anything.
    pub verification: &'static str,
    /// What was concluded about the **model artifact's bytes** before this run wrote anything, or `None` when no
    /// local artifact was involved.
    ///
    /// Set by the caller that performed the check — `local_model::verify_artifact` (feature `local-model`) today, called from
    /// `np embed` — rather than discovered here: `embed_run` is handed a provider and cannot know whether its bytes
    /// were hashed a moment ago. Every constructor leaves it `None`, which is the truthful default: a run that never
    /// verified has no verification to report, and a caller that checked and left this unset is understating itself
    /// rather than overstating. It is a field and not a parameter for the same reason the report is one object: the
    /// text summary and `--json` must not be able to disagree about it.
    pub artifact_check: Option<crate::embed::ArtifactCheck>,
    /// What this run decided about worker threads, or `None` when no local model was involved.
    ///
    /// Same shape as [`EmbedReport::artifact_check`] and for the same reason: the decision is made where the provider is
    /// built (np-cli's `embed_provider`, which holds both the config and the machine's CPU allowance) while the report is
    /// written downstream, and every constructor leaves it `None`. That default is the truthful one — a run with
    /// `provider = "none"` has no worker count to report, and printing one would be a number about nothing.
    pub workers: Option<WorkerPlan>,
    /// What this run **transferred from an origin**, or `None` when it transferred nothing (**F38**).
    ///
    /// Present-and-null in every document, like [`EmbedReport::artifact_check`], because the question a caller asks is
    /// binary and cheap: *"did this invocation move 310 MB, or were the vectors already fine?"* An absent key forces them
    /// to parse prose to find out; `null` answers it in one field. Set by np-cli where the fetch actually happened —
    /// `embed_run` is handed a provider and cannot know that bytes arrived a moment ago.
    pub fetched: Option<FetchedRecord>,
    /// Whether a permitted fetch was **skipped because nothing answered** (**F38**).
    ///
    /// `true` only when this run attempted a fetch and the transport reported no connection; never `true` alongside a
    /// [`EmbedOutcome::Offline`] produced without an attempt, and never `true` when the origin *answered* (404, 403, a
    /// redirect, wrong bytes) — those are refusals, not outages, and skipping past them is how a broken mirror becomes
    /// permanent.
    pub offline: bool,
    /// Image links this run looked at, counted **per link occurrence after per-note de-duplication of the written
    /// target** — so a figure linked twice in one note is one here (§7 rule 1), and a byte-identical copy in a second note
    /// is two.
    ///
    /// **Counted before resolution, so it includes the links that then refused.** The first version counted only links that
    /// resolved, which made `images_seen == 0` mean "no figure embedded successfully" rather than "no figure was written",
    /// and this field is what gates the text line: a note whose three figures are all `.svg`, or all misspelled, would print
    /// nothing while its refusals sat unread in `--json`. A count that silences the line reporting it reports nothing.
    ///
    /// These fields count images; [`EmbedReport::chunks_seen`] and its siblings count chunks. Nothing an image did moves any
    /// of those, and nothing an image failed to do moves the exit code (§8 rule 6).
    pub images_seen: usize,
    /// Images this run encoded and stamped for this model id.
    pub images_embedded: usize,
    /// Images refused for a §8 class — a count, and always [`EmbedReport::image_refusals`]'s length. Never an error
    /// count and never an exit-code change.
    pub images_refused: usize,
    /// One entry per refusal, as **(locator, class)** — and the locator's meaning depends on how far the link got:
    ///
    /// - a refusal raised **before a path existed** (`missing-target` from resolution, `outside-workspace`) names the
    ///   **linking note**, because there is no figure path to name and the written target may be a URL, which §8's rule 8
    ///   keeps out of a report;
    /// - a refusal raised **after resolution** (`unsupported-format` from the content gate, `undecodable`, `over-budget`,
    ///   `vision-unavailable`) names the **figure's workspace path**, because that is the file to go and look at.
    ///
    /// Both are bytes-derived strings shown lossily, like every other reported path here. Nothing in this list can carry an
    /// origin or an inline payload: a post-resolution refusal requires a regular in-workspace file, and a provider's message
    /// text is dropped by the classifier `image_refusal_of`, which keeps only the class. No URL and no origin crosses this boundary
    /// (§8's rule 8), and a target that appears twice in one note is one entry because it is one image.
    pub image_refusals: Vec<(String, MediaRefusal)>,
    /// **Notes**, not images — the live notes this run could not examine, so their figures were neither counted nor swept.
    ///
    /// Two faults reach this number, and both mean "this pass did not look": a body that would not read (permissions, a
    /// device error, a file replaced by a directory between scan and embed), and the tail of a corpus after the pass stopped
    /// early on a database fault. The consequence is the one that matters: [`db::prune_unlinked_media`] **does not run**
    /// while this is non-zero, because the sweep asks "which figures did no live note link?", and a note this run never read
    /// cannot answer it. Absence of evidence is not evidence of deletion — the vectors wait for a run that can read the file
    /// instead of being destroyed by a run that could not.
    pub images_unchecked: usize,
    /// Whether §7 rule 4's stale-figure sweep **ran to completion** in this run.
    ///
    /// `false` covers three situations, which is why the text line mentions it only for a run that looked at figures: no
    /// media pass at all (an arm that never opened the database), a pass held off by unread notes
    /// ([`EmbedReport::images_unchecked`]), and a pass stopped by a database fault — including one that failed *inside* the
    /// sweep, where some rows were already deleted and the rest remain. It is not derivable from `images_unchecked`, which is
    /// zero in that last case and zero for the arms too; guessing "zero means every note was read" would be reading a fact
    /// that is not there.
    pub images_sweep_ran: bool,
    /// **Media rows** this run deleted along with their vectors — not path rows, and not figures it looked at.
    ///
    /// §7 rule 4's cleanup was a number nobody could see until now, and the reason to surface it is the failure mode
    /// [`EmbedReport::images_sweep_ran`] cannot cover on its own: a database fault *inside* the sweep leaves some rows gone
    /// and the rest standing, most plausibly on a run where the author had just deleted every figure link — so
    /// `images_seen` is zero and the image line is suppressed. Without this count that run deletes vectors and prints
    /// nothing at all. A healthy sweep that removes nothing reports `0`, which is the common case and stays quiet.
    pub images_pruned: usize,
}

impl EmbedReport {
    /// The image refusals grouped by §8 class, in §8's table order, with zero-count classes left out.
    ///
    /// The text summary prints this and `--json` prints the per-path list, from one report object, so the two cannot
    /// disagree about how many `.svg` links a corpus has (§6's reason the report is one object at all). Order is the enum's
    /// declaration order rather than a sort by count: a reader comparing two runs line by line wants the same line to mean
    /// the same class in both.
    pub fn image_refusal_classes(&self) -> Vec<(&'static str, usize)> {
        let order = [
            MediaRefusal::MissingTarget,
            MediaRefusal::OutsideWorkspace,
            MediaRefusal::UnsupportedFormat,
            MediaRefusal::Undecodable,
            MediaRefusal::OverBudget,
            MediaRefusal::VisionUnavailable,
        ];
        order
            .into_iter()
            .filter_map(|class| {
                let count = self
                    .image_refusals
                    .iter()
                    .filter(|(_, got)| {
                        std::mem::discriminant(got) == std::mem::discriminant(&class)
                    })
                    .count();
                (count > 0).then(|| (class.label(), count))
            })
            .collect()
    }

    /// Whether this report may leave exit code 0 — arm (i) alone.
    pub fn succeeded(&self) -> bool {
        matches!(self.outcome, EmbedOutcome::Current)
    }

    /// A report that read nothing, for the arms that refuse before touching the corpus.
    fn unread(model: Option<String>, outcome: EmbedOutcome) -> Self {
        // **The refusal is a diagnostic as well as a sentence** (diagnostics design §1 point 4). This constructor is the
        // one place every arm that refuses before doing work passes through, so reporting here is what makes "every
        // refusal names its rule" true without six copies of the same call. Arm (iv) built at the end of a partial run
        // reports where it is assembled, for the reason [`diag_of`] gives for counting rather than arms.
        if let Some((rule, level, what)) = diag_of(&outcome, model.as_deref()) {
            let detail = outcome
                .detail()
                .unwrap_or_else(|| outcome.label().to_string());
            emit(level, rule, &what, &detail);
        }
        Self {
            model,
            outcome,
            chunks_seen: 0,
            current: 0,
            embedded: 0,
            refused_budget: 0,
            busy_gap: 0,
            errors: 0,
            verification: CAVEAT,
            artifact_check: None,
            workers: None,
            fetched: None,
            offline: false,
            images_seen: 0,
            images_embedded: 0,
            images_refused: 0,
            image_refusals: Vec::new(),
            images_unchecked: 0,
            images_sweep_ran: false,
            images_pruned: 0,
        }
    }
}

/// One fetch's report card, carried on [`EmbedReport::fetched`] (**F38**).
///
/// Deliberately **no URL and no host**: §12 rule 8 bounds what a report may say about where bytes came from, so this names
/// the *kind* of origin (`compiled-in-pin` / `configured-mirror`) and everything else that is actionable — which members,
/// how many bytes, whether each was digest-verified before it was published. A caller that wants the host already has it:
/// they set `[models] base_url`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct FetchedRecord {
    /// The pinned id whose bytes arrived.
    pub id: String,
    /// One entry per member transferred in this run.
    pub members: Vec<FetchedMember>,
    /// Which kind of origin served: `compiled-in-pin` or `configured-mirror`. Never a hostname.
    pub origin: &'static str,
}

/// One member's arrival, as [`EmbedReport::fetched`] reports it (**F38**).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct FetchedMember {
    /// `main` or `mmproj` — the same name `models install --member` and `model_files.name` use.
    pub name: String,
    /// Exact bytes written by this run.
    pub bytes: u64,
    /// Always `true` when this exists: fetch publishes only after the digest matches the pin (**F12** §12 rule 6), so a
    /// record of an unverified member would describe an event that cannot happen. It is a field rather than an omission
    /// because a reader should not have to know that rule to trust the row.
    pub digest_verified: bool,
}

/// What the media pass counted, assembled into [`EmbedReport`]'s image fields.
#[derive(Debug, Default, Clone)]
struct MediaTally {
    seen: usize,
    embedded: usize,
    refusals: Vec<(String, MediaRefusal)>,
    /// Live notes this pass did not examine — see [`EmbedReport::images_unchecked`]. The unit is notes.
    unchecked: usize,
    /// Notes taken off the list so far, and the whole list's length: when the pass stops early on a database fault, the
    /// difference is what it never reached, and that belongs here rather than in silence.
    processed: usize,
    total: usize,
    /// Whether §7 rule 4's sweep ran to completion this run — see [`EmbedReport::images_sweep_ran`].
    swept: bool,
    /// Media rows deleted so far this run — see [`EmbedReport::images_pruned`].
    pruned: usize,
}

/// Why `embed` could not answer at all — as distinct from the four arms, which it answers *with* a report.
#[derive(Debug, Error)]
pub enum EmbedRunError {
    /// Configuration made the run impossible in a way that is not arm (ii): an empty `data_dir`, which would
    /// put the workspace database beside the current directory. Mirrors [`crate::ScanError::Config`].
    #[error("configuration: {0}")]
    Config(#[from] ConfigError),
    /// The workspace database refused a read, or refused in a way that is not a lock. A `SQLITE_BUSY` met while
    /// writing is **not** this variant: it becomes [`EmbedReport::busy_gap`] so the run still reports what it
    /// did before the lock.
    #[error("workspace database: {0}")]
    Db(#[from] DbError),
    /// §6's "wrong-dim provider is refused before any write, naming both numbers". Raised before the corpus is
    /// read, so no vector row is attempted and the CHECK constraint that used to be the only thing noticing
    /// stays unused.
    #[error(
        "the model declares {provider} dimensions but this run needs {configured}; refused before any write, because one model id cannot mean two widths"
    )]
    ProviderDimMismatch {
        /// The width this run would store: `[embeddings] dim`, or the installed row's own declaration when the
        /// config declares none.
        configured: usize,
        /// What [`Embedder::dim`] answers. An `i64` for [`db::ModelRow::dim`]'s reason: SQLite's `INTEGER` may
        /// hold a negative or absurd width that must be printed, not narrowed.
        provider: i64,
    },
}

/// One owed chunk: the row to stamp, the text to hand the model, and the digest of what the model will read.
struct Owed {
    /// `chunks.id`, which `embeddings.chunk_id` references.
    id: i64,
    /// The chunk's own bytes, undressed: exactly what [`Embedder::embed`] is called with. Prefixing is the
    /// provider's business ([`Embedder::model_inputs`]); pre-applying it here would apply it twice.
    text: String,
    /// [`sha256_bytes`] of the provider's declared input for [`Owed::text`] — the value §6 makes
    /// `embeddings.source_hash` (§5's stamp rule, backlog D4), and the comparison [`db::vectors_for`] answers.
    digest: String,
}

/// What a batch write did to the database.
enum BatchWrite {
    /// Committed: every row in the batch is stamped.
    Written,
    /// Rolled back on `SQLITE_BUSY`: nothing in the batch is stamped, and the next run owes it again.
    Busy,
}

/// What resolving `[embeddings]` to a provider produced: a provider that can serve, or the one fact about the
/// artifact that this module cannot recover by itself.
///
/// **Why a reason travels as a value (R6c1 D-2).** np-cli used to answer an unreadable artifact with
/// [`crate::embed::NoEmbeddingProvider`], whose [`Embedder::dim`] is 0, and the width comparison inside this
/// module's private `run` read that 0 as a claim about the model. The run then refused with §6's wrong-dim
/// sentence — *"the model declares 0 dimensions but this run needs 768"* — about a row that declares **768** and
/// an artifact file that is simply not there. A width cannot carry a reason: the placeholder had no width to
/// declare, so the reason now arrives as [`Resolution::ArtifactAbsent`] and the comparison is skipped when there
/// is nothing to compare.
///
/// The three *configuration* states (`provider = "none"`, no [`crate::config::EmbeddingsConfig::model`], an id
/// naming no installed row) are deliberately not variants: arm (ii) derives them from configuration and the
/// `models` table in that same `run`, which is where those facts live, and it returns before anything here reads
/// a provider — for **these three**, which is the limit F32's design §2 records: the fourth state np-cli hands a
/// placeholder for (a row whose declared width cannot be stored) does reach the comparison above and takes §6's
/// wrong-dim sentence.
///
/// A caller that does not run an embed gets nothing from this enum, which is why [`NoProvider`] exists: `np search`
/// needs the reason as a value too, and re-deriving it at its print site is what made it say
/// `embeddings.provider = "none"` about a configuration that said `local-model` (F32).
pub enum Resolution<'a> {
    /// This build links no inference runtime (**F40**). Carried as its own resolution rather than handed to the writer
    /// loop as a provider that refuses every batch, because doing that reported arm (iv) — see
    /// [`EmbedOutcome::RuntimeNotBuilt`] for what it cost an operator who had done everything right.
    RuntimeNotBuilt {
        /// The configured model id, so the refusal names what stays owed.
        id: String,
    },
    /// A fetch was permitted and attempted for `id`, and the transport said **no connection** (**F38**). Carried in so
    /// the run answers arm (vi) before reading a chunk: nothing this process can do will produce vectors now, and the
    /// currency read would only be an expensive way of saying so.
    FetchSkippedOffline {
        /// The pinned model that could not be obtained.
        id: String,
    },
    /// A provider built from the configured row — the only thing this run can write a vector from, and the only
    /// [`Embedder::dim`] §6's wrong-dim refusal may quote.
    ///
    /// `&mut` because [`Embedder::embed_image`] is `&mut self`: a local provider opens its projector on the first image it
    /// is asked about, and that has to happen through the one provider this run holds rather than through a shared borrow
    /// that would need interior mutability over FFI handles. Text embedding stays `&self`, so nothing here mutates a
    /// provider today beyond what an image call will do next round.
    Provider(&'a mut dyn Embedder),
    /// The caller tried to build a local provider from the installed row and could not read its artifact file.
    ///
    /// Taken as the fact for this run instead of re-statting the path: a file appearing between the caller's
    /// check and here would otherwise turn a refusal into an embed attempt with no provider at all, which is a
    /// worse answer to the same question. The next run re-reads the directory.
    ArtifactAbsent,
}

/// Why this workspace has no usable embedding provider, decided where the decision is made — `[embeddings]` and the
/// `models` table — and handed to any caller that must explain itself rather than embed.
///
/// It sits beside [`EmbedOutcome`] because they answer the same question for two different commands, and both print
/// sentences nothing in np-core prints: arm (ii)'s labels are read by np-cli, and so are these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoProvider {
    /// `[embeddings] provider = "none"`, the default state of a fresh install. Not a fault, so the sentence names the
    /// key to change rather than a failure.
    ConfigDisabled,
    /// `provider = "local-model"` with no `model`. [`crate::config::Config::load`] refuses this combination before any
    /// command runs, so reaching this variant needs a `Config` built in memory — hence the unit-test-only coverage F32 §6 records.
    ModelUnnamed,
    /// The configured id names nothing installed in **this** workspace. The common first-run state: config edited by hand
    /// before any artifact was installed, which is the case whose message used to blame the provider setting.
    RowMissing {
        /// The id `[embeddings] model` asks for.
        id: String,
    },
    /// A row exists and its file is readable, but its declared width is not a number this run can store. Also
    /// unit-test-only: `np models install --dim` validates 1..=65535 at argv, so no CLI invocation produces it.
    RowDimUnusable {
        /// The id the row was installed under.
        id: String,
        /// What the row declares, verbatim — a width is a fact about stored bytes, and quoting it beats paraphrasing one.
        declared: String,
    },
}

impl NoProvider {
    /// The arm's one-line remedy, naming what to change and, where a command fixes it, the command.
    ///
    /// `workspace` is a parameter because the remedy pastes argv a human can run, and argv needs the workspace — the same
    /// reason [`install_hint`] takes it. Every sentence here is asserted to stay one line and free of double spaces:
    /// Task 6 shipped a search refusal that wrapped mid-sentence and printed twenty spaces between two words.
    pub fn sentence(&self, workspace: &Path) -> String {
        match self {
            Self::ConfigDisabled => concat!(
                r#"embedding disabled: embeddings.provider = "none" — set it to "local-model" and name an "#,
                "installed model with embeddings.model, then run np embed",
            )
            .to_string(),
            // The same words `Config::load` uses for this state, so the two places that can say it cannot disagree.
            Self::ModelUnnamed => concat!(
                r#"embeddings.provider = "local-model" needs embeddings.model to name an installed model; "#,
                "none is set",
            )
            .to_string(),
            Self::RowMissing { id } => format!(
                "no model row for \"{id}\" in this workspace: {}",
                install_hint(workspace)
            ),
            // "this run cannot store", not "this build cannot use": the width is a fact about the row, and embed's own
            // arm (iv) sentence for the same state says "this run needs" (§2's limit).
            Self::RowDimUnusable { id, declared } => format!(
                "the row for \"{id}\" declares a width this run cannot store (\"{declared}\")"
            ),
        }
    }
}

/// Embed what this workspace owes, through the injected provider.
///
/// Equivalent to [`embed_with_inject`] with an empty hook.
pub fn embed(
    workspace: &Path,
    cfg: &Config,
    provider: &mut dyn Embedder,
) -> Result<EmbedReport, EmbedRunError> {
    embed_with_inject(
        workspace,
        cfg,
        provider,
        &mut |_| {},
        crate::progress::none(),
    )
}

/// The entry point for a caller that resolved `[embeddings]` rather than building a provider it had to invent an
/// answer for.
///
/// Equivalent to [`embed`] with an empty hook, and both go through one body: the arms and their order cannot
/// differ between the two, so a caller that built no provider still cannot skip the currency read that decides
/// whether anything is owed.
pub fn embed_with_resolution(
    workspace: &Path,
    cfg: &Config,
    resolution: Resolution<'_>,
    progress: &dyn crate::progress::Progress,
) -> Result<EmbedReport, EmbedRunError> {
    run(workspace, cfg, resolution, &mut |_| {}, progress)
}

/// The same run with a hook called **between batches**, outside every transaction.
///
/// `between_batches` receives the workspace database path after each batch's write has committed or rolled
/// back, and before the next batch is handed to the provider. It exists for §6's contention rule: with the
/// model work outside transactions, a `np scan` must be able to take the write lock while an embed is running,
/// and `tests/embed_run.rs` runs one from inside this hook to prove it. A production caller passes nothing.
pub fn embed_with_inject(
    workspace: &Path,
    cfg: &Config,
    provider: &mut dyn Embedder,
    between_batches: &mut dyn FnMut(&Path),
    progress: &dyn crate::progress::Progress,
) -> Result<EmbedReport, EmbedRunError> {
    run(
        workspace,
        cfg,
        Resolution::Provider(provider),
        between_batches,
        progress,
    )
}

/// The body of every entry point above.
fn run(
    workspace: &Path,
    cfg: &Config,
    mut resolution: Resolution<'_>,
    between_batches: &mut dyn FnMut(&Path),
    progress: &dyn crate::progress::Progress,
) -> Result<EmbedReport, EmbedRunError> {
    // The same fence `crate::scan` raises: an empty data dir would derive a database path beside the current
    // directory, possibly inside the workspace being embedded.
    if cfg.data_dir.as_os_str().is_empty() {
        return Err(ConfigError::MissingField("data_dir").into());
    }
    let root = abs_path(workspace);
    // One span for the whole embed run, named `embed` for the same reason `scan` is named `scan` (diagnostics design
    // point 7). It carries the workspace so a line from inside a batch says which vault it came from; the model id needs
    // no field of its own, because every refusal that names an id already puts it in `what`.
    let _span = tracing::info_span!(
        target: crate::diagnostics::target::EMBED,
        "embed",
        workspace = %root.display()
    )
    .entered();
    let db_path = cfg.workspace_db(&root);
    let db = Db::open(&db_path)?;

    // ---- arm (vi): a fetch was permitted, attempted, and met no connection (**F38**). First arm in the function, on
    // purpose: nothing about configuration or the model table can outrank "we tried to get the model and could not reach
    // anyone". Placed lower, arm (ii) answered `configuration disabled` for a workspace whose only fault was the network —
    // the config was fine, it asked for a fetch, and the fetch was allowed. Refused before the currency read like arm (v),
    // so refusing honestly still costs no pass over live chunks.
    if let Resolution::FetchSkippedOffline { id } = &resolution {
        let mut report =
            EmbedReport::unread(Some(id.clone()), EmbedOutcome::Offline { id: id.clone() });
        // The flag is the machine-readable half of the same fact, so a caller never has to string-match an outcome label
        // to learn that this run was offline (`fetched` stays `null`: nothing transferred).
        report.offline = true;
        return Ok(report);
    }

    // ---- arm (ii): configuration, decided before one chunk row is read.
    let model_id = match configured_model(cfg) {
        Ok(id) => id,
        Err(outcome) => return Ok(EmbedReport::unread(configured_id(cfg), outcome)),
    };
    let row = match db::model_row(&db, &model_id)? {
        Some(row) => row,
        // The other half of arm (ii): a `model` naming no installed row. Naming the key and the id is what
        // makes this readable as a configuration state rather than a fault (§6).
        None => {
            // **F41 option A.** The configuration is correct — it names a provider and an id this build pins. What is
            // missing is 309 MB, which is arm (iii)'s problem seen from before the first install, so this refusal
            // carries arm (iii)'s remedy. Without it the air-gapped first run (the exact audience for
            // `NP_NO_AUTO_FETCH=1`) had to find `np models install` in the README, while two neighbouring arms named
            // their commands.
            return Ok(EmbedReport::unread(
                Some(model_id.clone()),
                EmbedOutcome::ConfigurationDisabled {
                    key: "embeddings.model",
                    id: Some(model_id),
                    hint: Some(install_hint(workspace)),
                },
            ));
        }
    };

    // ---- wrong width: refused here, before the corpus is read and long before any write (§6).
    // `configured` is what this run needs a vector to be — `[embeddings] dim`, or the installed row's own
    // declaration when the config declares none, which is the only width that row can honour — and
    // `provider.dim()` is what would actually be written. Reading `dim` here is the whole reason its doc comment
    // could stop claiming nothing reads it.
    let declared = usize::try_from(row.dim).ok();
    let configured = cfg.embeddings.dim.map(usize::from).or(declared);
    // Borrowed rather than bound by value: `Resolution::Provider` now holds a `&mut`, which is not `Copy`, so taking it out
    // of `resolution` here would move the provider away from the arm (iii) match below.
    if let Resolution::Provider(provider) = &resolution {
        match configured {
            // A row whose `dim` is not a non-negative integer cannot describe a width this run stores, and there
            // is no number to compare `provider.dim()` against. Report the row's own value rather than narrow it.
            None => {
                return Err(EmbedRunError::ProviderDimMismatch {
                    configured: provider.dim(),
                    provider: row.dim,
                })
            }
            Some(width) if width == provider.dim() => {}
            Some(width) => {
                return Err(EmbedRunError::ProviderDimMismatch {
                    configured: width,
                    provider: i64::try_from(provider.dim()).unwrap_or(i64::MAX),
                })
            }
        }
    }
    // `Resolution::ArtifactAbsent` skips the comparison, and skipping it is not a hole: §6's refusal compares two
    // declarations and one of them does not exist. A real disagreement still refuses — np-cli builds its local
    // provider through [`crate::embed::LocalModelProvider::new`], which checks the row against the width this run
    // needs *before* it can report an unreadable artifact, so `ArtifactAbsent` arrives only with widths that
    // already agreed.

    // ---- arm (v): no runtime in this binary. Decided here, after configuration and width (both of which a person can
    // fix by editing a file, and both of which stay more specific than "no runtime") and **before** the currency read,
    // so nothing is scanned for work that this process could never do (**F40**).
    if let Resolution::RuntimeNotBuilt { id } = &resolution {
        return Ok(EmbedReport::unread(
            Some(id.clone()),
            EmbedOutcome::RuntimeNotBuilt { id: id.clone() },
        ));
    }

    // ---- the currency read: live chunks against this model's stored stamps.
    let stamps = db::vectors_for(&db, &model_id)?
        .into_iter()
        .map(|stamp| (stamp.chunk_id, stamp.source_hash))
        .collect::<HashMap<i64, String>>();
    // Who can say what the model would read. `None` means §6's arm (iii) is waiting downstream: with no provider
    // there is no witness for [`Embedder::model_inputs`], so nothing may be called *current* and every live chunk
    // counts as owed. That costs no work — the run returns before the writer loop either way — but it keeps
    // `current` from claiming a match against bytes nobody produced, which is the difference between "up to date"
    // and "we could not tell" in a report a person acts on.
    // `&**provider` is a shared reborrow of the run's `&mut`: the currency read only ever asks [`Embedder::model_inputs`],
    // and the mutable borrow is free again for the writer loop as soon as this loop ends.
    let witness: Option<&dyn Embedder> = match &resolution {
        Resolution::Provider(provider) => Some(&**provider),
        Resolution::ArtifactAbsent
        | Resolution::RuntimeNotBuilt { .. }
        | Resolution::FetchSkippedOffline { .. } => None,
    };

    let mut owed = Vec::new();
    let mut seen = 0usize;
    let mut current = 0usize;
    let mut errors = 0usize;
    // Set when a provider breaks the `model_inputs` shape contract while the run was still classifying chunks.
    let mut input_shape: Option<String> = None;
    // A chunk this run could not classify or could not embed: owed, and not produced. It keeps the run out of
    // arm (i) without being anyone's fault message.
    let mut unfinished = false;
    for chunk in db::live_chunks(&db)? {
        seen += 1;
        // The stamp rule needs the bytes as they will be handed over, so a row whose text is not UTF-8 cannot
        // be embedded at all: report it, never repair it (§5's posture). Such a row today cannot exist — every
        // chunk comes from a decoded body — but a hand-edited database can hold one.
        let Ok(text) = String::from_utf8(chunk.text) else {
            errors += 1;
            unfinished = true;
            continue;
        };
        // Ask the provider what the model would read for this chunk (A′). The stamp has to answer a question about
        // the model, not about this module's habits, and R6c2's P1 is what happens when the caller answers it by
        // guessing: `prefix_doc` applied here *and* inside the provider, with a digest covering one copy.
        //
        // A provider that answers with nothing, or with more than one string per text, has broken the
        // [`Embedder::model_inputs`] contract; this cannot embed it honestly, so it reports and moves on rather
        // than stamping a guess. `paired` applies the same rule to vectors after the fact; here there is no write
        // at risk, only the run's classification, which becomes arm (iv)'s gap.
        let Some(witness) = witness else {
            owed.push(Owed {
                id: chunk.id,
                text,
                digest: String::new(),
            });
            continue;
        };
        let declared = witness.model_inputs(std::slice::from_ref(&text.as_str()));
        let input = match declared.as_slice() {
            [only] => only.clone(),
            _ => {
                errors += 1;
                unfinished = true;
                input_shape = Some(format!(
                    "the provider declared {} inputs for a chunk it was asked about once",
                    declared.len()
                ));
                continue;
            }
        };
        let digest = sha256_bytes(input.as_bytes());
        match stamps.get(&chunk.id) {
            Some(stored) if stored == &digest => current += 1,
            // Either no vector for this model id, or one made from different bytes — including the case §6
            // cares about, where only `prefix_doc` moved.
            _ => owed.push(Owed {
                id: chunk.id,
                text,
                digest,
            }),
        }
    }

    if owed.is_empty() {
        // Nothing is owed as *text*, which does not mean nothing is owed as an image: a figure added since the last run
        // is owed even in a corpus whose chunks are all current. So arm (i) still runs the pass, and only a run with no
        // provider to ask skips it — counting images that nobody could encode would report work this build cannot do.
        let media = media_through(&mut resolution, &db, &root, &model_id);
        return Ok(EmbedReport {
            model: Some(model_id),
            outcome: EmbedOutcome::Current,
            chunks_seen: seen,
            current,
            embedded: 0,
            refused_budget: 0,
            busy_gap: 0,
            errors,
            verification: CAVEAT,
            artifact_check: None,
            workers: None,
            ..media_fields(&media)
        });
    }

    // ---- arm (iii): something is owed, so the model has to run, and its artifact has to be there to run.
    // Checked here rather than first because a corpus that needs no work needs no model; §6's exit state (i)
    // says nothing owed is exit 0 whatever else is true.
    let artifact = cfg.workspace_models_dir(&root).join(&model_id);
    let provider = match resolution {
        // The artifact is there and readable, so the provider that was built against it can run.
        Resolution::Provider(provider) if artifact_file_exists(&artifact) => provider,
        // Two ways to meet this arm: the caller already failed to read the file (`ArtifactAbsent`, taken as the
        // fact for this run — see its doc), or the file stopped being there between that read and here. Either
        // way there is no provider to hand the owed chunks to, so §6's arm (iii) answers, and one report literal
        // means the two halves cannot grow different sentences.
        // **F40, defensive and unreachable today** (arm (v) returns before the currency read). Spelled out rather than
        // `unreachable!()` on purpose: if a later edit drops that early return, the reading this arm must *not* fall into
        // is the one below it — answering "artifact absent" about a build with no runtime would name a file nobody can
        // install to fix anything, which is the same class of wrong sentence this card was filed for.
        Resolution::RuntimeNotBuilt { .. } => {
            return Ok(EmbedReport::unread(
                Some(model_id.clone()),
                EmbedOutcome::RuntimeNotBuilt { id: model_id },
            ));
        }
        // **F38, defensive and unreachable today** (arm (vi) returns above), spelled for the same reason as the arm over
        // it: falling through to "artifact absent" would tell a machine with no network to go install a file it cannot
        // reach, which is a different wrong sentence from the one F40 filed.
        Resolution::FetchSkippedOffline { id } => {
            let mut report = EmbedReport::unread(Some(model_id), EmbedOutcome::Offline { id });
            report.offline = true;
            return Ok(report);
        }
        Resolution::Provider(_) | Resolution::ArtifactAbsent => {
            let outcome = EmbedOutcome::ArtifactAbsent {
                model: model_id.clone(),
                hint: install_hint(workspace),
                artifact,
            };
            // Arms (ii), (v) and (vi) reach a diagnostic through [`EmbedReport::unread`]; this one and arm (iv) build
            // their reports where the run's counts are already known, so they report at their own site. Every arm that
            // refuses therefore reports exactly once, which is what §1 point 4 of the diagnostics design asks for.
            if let Some((rule, level, what)) = diag_of(&outcome, Some(model_id.as_str())) {
                emit(
                    level,
                    rule,
                    &what,
                    outcome
                        .detail()
                        .as_deref()
                        .unwrap_or_else(|| outcome.label()),
                );
            }
            return Ok(EmbedReport {
                model: Some(model_id),
                outcome,
                chunks_seen: seen,
                current,
                embedded: 0,
                refused_budget: 0,
                busy_gap: 0,
                errors,
                verification: CAVEAT,
                artifact_check: None,
                // Nothing transferred, and nothing was skipped for being unreachable: arm (iii) says the file is not
                // there, which is a filesystem fact and stays one even when the network is down (**F38**).
                fetched: None,
                offline: false,
                workers: None,
                images_seen: 0,
                images_embedded: 0,
                images_refused: 0,
                image_refusals: Vec::new(),
                images_unchecked: 0,
                images_sweep_ran: false,
                images_pruned: 0,
            });
        }
    };

    // ---- the writer loop: model work outside the transaction, one bounded transaction per batch.
    let mut embedded = 0usize;
    // Chunks handed to the provider so far. Its own counter rather than `owed.len() - remaining`, because the run can
    // leave the loop early and a progress line must describe the work that happened, not the plan.
    let mut chunks_handed = 0usize;
    let mut refused_budget = 0usize;
    let mut busy_gap = 0usize;
    // The token numbers of the last over-budget refusal, kept for §6's report line.
    let mut budget_detail: Option<String> = None;
    // The provider's own sentence for the last refusal it raised, which becomes arm (iv)'s reason. Every
    // `EmbedError` message is input-free and secret-free by that type's own rule (§3).
    let mut reason: Option<String> = None;

    // The pass says where it starts, and how much work it starts with. `owed.len()` is free — this is the vector the run
    // just built to decide what to embed, not another query — so a client gets a denominator from the first line instead of
    // an indeterminate bar. The total is fixed here and never recomputed: owed chunks leave the owed set as they are
    // written, and a shrinking denominator would make the fraction run backwards at the end.
    let chunks_total = owed.len();
    progress.report(&crate::progress::ProgressEvent {
        phase: crate::progress::Phase::Embedding,
        chunks_done: 0,
        chunks_total: Some(chunks_total),
        ..Default::default()
    });

    'batches: for batch in owed.chunks(EMBED_BATCH_CHUNKS) {
        let embedded_before = embedded;
        // Raw chunk text, undressed: whatever the model is given is the provider's doing, and it is the same
        // dressing [`Embedder::model_inputs`] described when this run decided the chunk was owed.
        let texts: Vec<&str> = batch.iter().map(|item| item.text.as_str()).collect();
        match provider.embed(&texts) {
            Ok(vectors) => match paired(batch, &vectors, provider.dim()) {
                Ok(rows) => match write_batch(&db, &model_id, &rows)? {
                    BatchWrite::Written => embedded += rows.len(),
                    BatchWrite::Busy => busy_gap += 1,
                },
                Err(message) => {
                    errors += 1;
                    reason = Some(message);
                    break 'batches;
                }
            },
            // One over-budget chunk must not refuse the batch it travelled in, and must not be truncated
            // either, so its batch is retried one text at a time to find which ones are over (§5).
            Err(EmbedError::TokenBudgetExceeded { .. }) => {
                for item in batch {
                    match provider.embed(&[item.text.as_str()]) {
                        Ok(vectors) => {
                            match paired(std::slice::from_ref(item), &vectors, provider.dim()) {
                                Ok(rows) => match write_batch(&db, &model_id, &rows)? {
                                    BatchWrite::Written => embedded += rows.len(),
                                    BatchWrite::Busy => busy_gap += 1,
                                },
                                Err(message) => {
                                    errors += 1;
                                    reason = Some(message);
                                    break 'batches;
                                }
                            }
                        }
                        Err(EmbedError::TokenBudgetExceeded { tokens, budget }) => {
                            refused_budget += 1;
                            // Keep the numbers even when the run goes on: they are the whole diagnosis, and
                            // they survive into the arm sentence below rather than being summarised away.
                            budget_detail =
                                Some(format!("{tokens} tokens needed, budget {budget}"));
                            // …and the same two numbers go as fields (`token-budget`), because the sentence reaches a
                            // person while these reach an alert. `what` is the chunk id: a batch item carries no path —
                            // [`Owed`] holds id, text and stamp only — and an id resolves to a note in one query, which
                            // is inside what the diagnostics design §12 allows (paths, ids, tokens, counts) where a body
                            // never goes.
                            tracing::warn!(
                                target: crate::diagnostics::target::EMBED,
                                rule = crate::diagnostics::rule::TOKEN_BUDGET,
                                what = %item.id,
                                expected = budget,
                                actual = tokens,
                                "chunk refused: over the embeddings.max_tokens budget"
                            );
                        }
                        Err(other) => {
                            errors += 1;
                            reason = Some(other.to_string());
                            break 'batches;
                        }
                    }
                }
            }
            Err(other) => {
                errors += 1;
                reason = Some(other.to_string());
                break 'batches;
            }
        }
        // One line per batch, at the same point `between_batches` fires — after the write committed or rolled back,
        // before the next batch is handed to the model. `writing` means this batch's rows are in the database; a batch
        // that refused, was over budget, or found the table busy says `embedding` instead, because claiming a write
        // that did not happen is the one thing a progress line must never do.
        chunks_handed += batch.len();
        progress.report(&crate::progress::ProgressEvent {
            phase: if embedded > embedded_before {
                crate::progress::Phase::Writing
            } else {
                crate::progress::Phase::Embedding
            },
            chunks_done: chunks_handed,
            chunks_total: Some(chunks_total),
            embedded,
            ..Default::default()
        });

        between_batches(&db_path);
    }

    // ---- the media pass: images are a second vector source with their own counts, and they cannot change the arm.
    let media = run_media_pass(&db, &root, &model_id, Some(provider));

    // ---- arm (iv) when anything is still owed and unproduced; arm (i) when nothing is.
    let mut gaps: Vec<String> = Vec::new();
    if busy_gap > 0 {
        gaps.push(format!(
            "{busy_gap} batch(es) met a locked database (SQLITE_BUSY) and stamped nothing"
        ));
    }
    if refused_budget > 0 {
        let detail = budget_detail
            .as_deref()
            .map(|detail| format!(" ({detail})"))
            .unwrap_or_default();
        gaps.push(format!(
            "{refused_budget} chunk(s) were over the token budget and were refused rather than truncated{detail}"
        ));
    }
    if let Some(detail) = input_shape {
        gaps.push(format!(
            "{detail}, so nothing it produced could be stamped and the run cannot claim currency"
        ));
    }
    if errors > 0 {
        let detail = reason.as_deref().unwrap_or("the provider refused");
        gaps.push(format!("{errors} chunk(s) or batch(es) failed: {detail}"));
    }
    let outcome = if gaps.is_empty() && !unfinished {
        EmbedOutcome::Current
    } else {
        EmbedOutcome::NotProduced {
            reason: gaps.join("; "),
        }
    };
    // Arm (iv) is assembled at the end of a partial run rather than by [`EmbedReport::unread`], so this is where its
    // diagnostic is reported — and arm (i) reports nothing, because a run that produced what it owed is not a degraded
    // state (§4: one event per refusal, none per success).
    if let Some((rule, level, what)) = diag_of(&outcome, Some(model_id.as_str())) {
        emit(
            level,
            rule,
            &what,
            outcome
                .detail()
                .as_deref()
                .unwrap_or_else(|| outcome.label()),
        );
    }

    Ok(EmbedReport {
        model: Some(model_id),
        outcome,
        chunks_seen: seen,
        current,
        embedded,
        refused_budget,
        busy_gap,
        errors,
        verification: CAVEAT,
        artifact_check: None,
        workers: None,
        ..media_fields(&media)
    })
}

/// The media pass seen through whatever the caller resolved: a provider when there is one, nothing when the artifact
/// could not be read.
///
/// A function rather than a `match` at the call site because of a borrow, not a rule: reborrowing the `&mut dyn Embedder`
/// stored inside [`Resolution`] keeps `resolution` borrowed, and arm (i) returns while arm (iii) still has to *move* it.
fn media_through<'a, 'r>(
    resolution: &'r mut Resolution<'a>,
    db: &Db,
    root: &Path,
    model: &str,
) -> MediaTally {
    let asked: Option<&'r mut dyn Embedder> = match resolution {
        Resolution::Provider(provider) => Some(&mut **provider),
        // No provider means no figure can be encoded, and every image row then refuses as unavailable — which is the
        // honest shape. Unreachable under arms (v) and (vi) (**F40**, **F38**), listed so it stays that way by construction.
        Resolution::ArtifactAbsent
        | Resolution::RuntimeNotBuilt { .. }
        | Resolution::FetchSkippedOffline { .. } => None,
    };
    run_media_pass(db, root, model, asked)
}

/// Run [`media_pass`] under §8 rule 6: a locked database stops the image work where it stands, and does not raise.
///
/// Raising would be the one way an image could change an exit code — [`EmbedRunError::Db`] leaves arm (iv) with exit 1 —
/// so `SQLITE_BUSY` ends the pass instead, exactly as it ends a text batch: what did not run is not stamped and not
/// counted. Whatever was already counted stands. Any other database failure still fails the run, because that is the
/// same fault the text half reports.
fn run_media_pass(
    db: &Db,
    root: &Path,
    model: &str,
    provider: Option<&mut dyn Embedder>,
) -> MediaTally {
    let mut tally = MediaTally::default();
    match media_pass(db, root, model, provider, &mut tally) {
        Ok(()) => tally,
        // **Both faults get the same answer, deliberately.** §8 rule 6 says no image outcome may change the exit code, and a
        // workspace-database fault inside the image half is not this command's failure either — so `SQLITE_BUSY` and "the
        // file is not a database" are treated identically here, which is why there is no `is_busy` arm to read. An earlier
        // version had one that fell through to the same code while its comment claimed non-lock faults still failed the run;
        // the claim was false, and a dead arm is a bad place to hide one.
        Err(_) => tally_after_stop(&mut tally),
        // A non-lock failure cannot be a refusal (§8's classes are about images, not about the workspace), and cannot
        // change the exit code either (§8 rule 6). What it can do is say so: the notes past the fault count as unchecked,
        // which holds the sweep back too, so a database that went busy halfway through does not spend the rest of the
        // corpus' vectors on the strength of never having read them.
    }
}

/// Embed what the live notes link to, then sweep what no live note links any more.
///
/// §7's rules, in the order they are executed:
///
/// 1. **one row and one encode per figure per note** — de-duplicated on the **resolved path**, in document order, so the
///    first mention carries the link and the second adds nothing. `images_seen` de-duplicates on the **written target**
///    instead, because it counts what the author wrote; two spellings of one file are two links and one encode, and each
///    field documents the key it uses.
/// 3. **one vector for one content** — `media` is keyed by content hash, so a copy in another note adds a path row and no
///    encode;
/// 2. **unchanged bytes are zero encodes** — two stamps answer this, and they are different questions:
///    [`db::media_path_check`] (size + mtime, per *path*) says whether to read the file at all, and
///    [`db::media_stamp`] (the digest, per *(media, model)* pair) says whether to encode. A path whose file has not
///    moved and whose vector is current costs one `stat` and no read;
/// 4. **a figure nothing links is gone** — the sweep at the end unlinks every `media_path` row no live note linked in this
///    pass, where "linked" is answered by the note's text rather than by whether the file opened
///    ([`media::link_target_rel`]), so a figure that is merely unopenable today keeps its row and its vector, which is what a renamed or deleted figure needs (its old path has no mention left to
///    contradict it), and [`db::prune_unlinked_media`] then takes the media row and its vectors with it. The pass owns
///    this table's whole contents — nothing else writes a media path — so "not referenced by any live note" is the same
///    statement as "no longer linked" — **unless** some live note went unread, in which case the sweep does not run at all
///    and [`EmbedReport::images_unchecked`] says how many notes this pass could not ask. Destroying a figure's vector
///    because `read` returned `EACCES` is not what "nothing links it" means.
///
/// **No image outcome reaches the exit code** (§8 rule 6): every per-image failure below becomes a counted refusal, and
/// only a workspace-database failure leaves this function with an `Err`, which is the same answer the text half gives for
/// the same fault.
fn media_pass(
    db: &Db,
    root: &Path,
    model: &str,
    mut provider: Option<&mut dyn Embedder>,
    tally: &mut MediaTally,
) -> Result<(), DbError> {
    // The platform rules are chosen once per run, not once per link: production has exactly one host OS for the whole
    // walk (`media::PathStyle::native`), and a pass that mixed styles would resolve two spellings of one name to two rows.
    let style = media::PathStyle::native();
    // Asked once for the whole run: capability is a property of the provider, and re-asking per image would let one
    // refusal mid-run change what the images after it are told.
    let capable = provider
        .as_ref()
        .is_some_and(|provider| provider.supports_images());
    // …and asked in the same breath, for the same reason, is *why not* (F28). "This build has no image code" and "this
    // workspace has no projector" were one class until here; they advise different actions — build with a feature, or
    // fetch one file — so folding them kept the counts honest and the advice wrong. `None` means either the provider can
    // encode or there is no provider to ask, and both keep the pre-F28 label below.
    let blocker = provider
        .as_ref()
        .and_then(|provider| provider.image_blocker());
    let mut referenced: HashSet<Vec<u8>> = HashSet::new();

    let notes = db::live_note_paths(db)?;
    tally.total = notes.len();
    for note_rel in notes {
        tally.processed += 1;
        // The body comes from disk, not from `chunks.text`: the chunker may have dropped a fenced block that holds a
        // figure, and §7 extracts links from what the author wrote.
        let body = match media::workspace_path(root, &note_rel) {
            Some(abs) => std::fs::read(walk::to_open_path(&abs)).ok(),
            None => None,
        };
        let Some(body) = body else {
            // **Skip the note and hold the sweep off**, which is what `images_unchecked` exists for. Reading "no links" out
            // of "could not read" would delete this note's figures' rows and vectors on the strength of a permission error:
            // derived data destroyed by an I/O fault, then paid for twice by the next healthy run. §7 rule 4's lifecycle is
            // a statement about notes that were read and did not link; this note is neither.
            tally.unchecked += 1;
            continue;
        };
        let links = media::image_links(&body);
        let note_shown = String::from_utf8_lossy(&note_rel).to_string();
        // §7 rule 1, per note, keyed on the **written target**: the same figure named twice is one image. Two spellings of
        // one file (`a.png`, `%61.png`) are two links that were written and so two `images_seen`, and they still collapse to
        // one row and one encode below, where the key is the resolved path — the two counts answer different questions and
        // are keyed accordingly. A second note naming the same bytes is a separate occurrence either way and reaches
        // `record_media`'s insert-or-get, which answers with the existing row.
        let mut named: HashSet<Vec<u8>> = HashSet::new();
        let mut resolved_paths: HashSet<Vec<u8>> = HashSet::new();
        for link in links {
            // Counted here, before the target is placed: a link that refuses is still a figure the author wrote, and this
            // count gates the text line (§6). Deduplication happens first, so §7 rule 1 holds.
            if !named.insert(link.target.clone()) {
                continue;
            }
            tally.seen += 1;
            let resolved = match media::resolve_image_in(style, root, &note_rel, &link.target) {
                Ok(resolved) => resolved,
                // The locator here is the *note*, because resolution refused before a path existed. A target is never
                // reported: it may be a URL, and §8's rule 8 keeps URLs out of a report (§8's rule 8).
                Err(refusal) => {
                    // **The path stays alive even though nothing resolved.** §7 rule 4 asks whether a live note *links* a
                    // path, and this note links it — the file is absent, unopenable, or intercepted by a symlink today, which
                    // is an event about the disk, not an edit to the note. Without this line the sweep reads the refusal as
                    // "no longer linked", unlinks the row and `prune_unlinked_media` cascades the vector away: exit 0, nothing
                    // reported, and `np search` stops answering for a figure the author never deleted. Targets that never
                    // named this path in the first place — rooted, escaping, or an extension §8 refuses — answer `None` and
                    // protect nothing; `media::link_target_rel` documents the exact set.
                    if let Some(rel) = media::link_target_rel(style, &note_rel, &link.target) {
                        referenced.insert(rel);
                    }
                    tally.refusals.push((note_shown.clone(), refusal));
                    continue;
                }
            };
            // One row and one encode per resolved path per note (§7 rule 1), independent of the count above.
            if !resolved_paths.insert(resolved.rel_bytes.clone()) {
                continue;
            }
            let shown = String::from_utf8_lossy(&resolved.rel_bytes).to_string();
            referenced.insert(resolved.rel_bytes.clone());

            // One `stat` decides whether anything has to be read at all.
            let stat = std::fs::metadata(walk::to_open_path(&resolved.abs));
            let Ok(meta) = stat else {
                tally.refusals.push((shown, MediaRefusal::MissingTarget));
                continue;
            };
            let size = i64::try_from(meta.len()).unwrap_or(i64::MAX);
            let mtime = walk::epoch_secs(&meta);
            let stamped = match db::media_path_check(db, &resolved.rel_bytes)? {
                Some(check) if check.size == size && check.mtime == mtime => {
                    db::media_by_hash(db, &check.hash)?
                }
                _ => None,
            };

            let (row, digest, bytes) = match stamped {
                // The file has not moved since its bytes were digested: no read, no digest.
                Some(row) => {
                    let digest = row.hash.clone();
                    (row, digest, None)
                }
                None => {
                    let Ok(bytes) = std::fs::read(walk::to_open_path(&resolved.abs)) else {
                        tally.refusals.push((shown, MediaRefusal::MissingTarget));
                        continue;
                    };
                    // Content, not the filename, decides what a figure is (F26): `.gif`-shaped bytes that are not a
                    // raster this build decodes refuse here rather than reaching an encoder.
                    let Some(kind) = media::image_kind_of_prefix(&bytes) else {
                        tally
                            .refusals
                            .push((shown, MediaRefusal::UnsupportedFormat));
                        continue;
                    };
                    // The bytes judged and the bytes digested are the same copy, which is what F26 closed; the encoder
                    // below gets that third reading of the same slice.
                    let Ok((digest, _)) = sha256_reader_with_size(&mut bytes.as_slice()) else {
                        tally.refusals.push((shown, MediaRefusal::MissingTarget));
                        continue;
                    };
                    let row = db::record_media(db, &digest, size, kind)?;
                    db::link_media_path(db, &resolved.rel_bytes, &resolved.abs, row.id)?;
                    db::set_media_path_check(db, &resolved.rel_bytes, size, mtime)?;
                    (row, digest, Some(bytes))
                }
            };

            // §7 rule 2: the vector for these exact bytes already exists, so nothing is encoded and nothing is written.
            if db::media_stamp(db, row.id, model)?.as_deref() == Some(digest.as_str()) {
                continue;
            }
            // About to encode, so the bytes themselves are needed. When they came from the path check they were never read
            // this run — and a path check is only *size plus whole-second mtime*, which an editor's atomic save can preserve
            // across completely different pixels. Reading the file is not enough: the digest has to come from the bytes in
            // hand, or a vector of the new content is stored under the old content's hash and `media.hash`, `source_hash` and
            // the file disagree three ways until something re-hashes by chance.
            let (bytes, digest, row) = match bytes {
                Some(bytes) => (bytes, digest, row),
                None => {
                    let Ok(fresh) = std::fs::read(walk::to_open_path(&resolved.abs)) else {
                        tally.refusals.push((shown, MediaRefusal::MissingTarget));
                        continue;
                    };
                    let Some(kind) = media::image_kind_of_prefix(&fresh) else {
                        tally
                            .refusals
                            .push((shown, MediaRefusal::UnsupportedFormat));
                        continue;
                    };
                    let Ok((fresh_digest, _)) = sha256_reader_with_size(&mut fresh.as_slice())
                    else {
                        tally.refusals.push((shown, MediaRefusal::MissingTarget));
                        continue;
                    };
                    if fresh_digest != digest {
                        // The stamp vouched for other bytes. Record what is really here and re-point the path at it — which
                        // also nulls the pair (`db::link_media_path`), so the next run measures this file instead of trusting
                        // one that has already been fooled once — then ask currency again about the bytes actually held.
                        let fresh_row = db::record_media(db, &fresh_digest, size, kind)?;
                        db::link_media_path(db, &resolved.rel_bytes, &resolved.abs, fresh_row.id)?;
                        db::set_media_path_check(db, &resolved.rel_bytes, size, mtime)?;
                        if db::media_stamp(db, fresh_row.id, model)?.as_deref()
                            == Some(fresh_digest.as_str())
                        {
                            continue;
                        }
                        (fresh, fresh_digest, fresh_row)
                    } else {
                        (fresh, digest, row)
                    }
                }
            };
            // No provider at all (`Resolution::ArtifactAbsent`) is §8's `vision-unavailable` in its plainest form: this
            // build and model pair cannot serve the request, and asking was still worth the count.
            if !capable {
                // Written as an exhaustive match with no wildcard on purpose: a future `ImageBlocker` variant has to be
                // given a class here deliberately rather than falling into `vision-unavailable`, which is the closed-set
                // habit [`image_refusal_of`] documents as a decision rather than a discovery.
                let class = match blocker {
                    Some(ImageBlocker::ProjectorAbsent) => MediaRefusal::ProjectorAbsent,
                    Some(ImageBlocker::Build) | None => MediaRefusal::VisionUnavailable,
                };
                tally.refusals.push((shown, class));
                continue;
            }
            let Some(provider) = provider.as_mut() else {
                tally
                    .refusals
                    .push((shown, MediaRefusal::VisionUnavailable));
                continue;
            };
            match provider.embed_image(&bytes) {
                Ok(vector) => {
                    let dim = i64::try_from(vector.dim()).unwrap_or(i64::MAX);
                    db::record_media_embedding(
                        db,
                        row.id,
                        model,
                        dim,
                        &vector.to_bytes(),
                        &digest,
                        now_secs(),
                    )?;
                    tally.embedded += 1;
                }
                // A provider's refusal is a count, never a failed run (§8 rule 6).
                Err(error) => tally.refusals.push((shown, image_refusal_of(&error))),
            }
        }
    }

    // §7 rule 4, **held off when this pass did not read every live note** (`images_unchecked`): the sweep's question is
    // "which figures did no live note link?", and a note whose body would not read never answered it. Pruning waits for a
    // run that can look — a stale row costs disk, a deleted vector costs an encode, and only one of those is destroyed by
    // something that was not the author's edit.
    if tally.unchecked > 0 {
        return Ok(());
    }
    for rel in db::media_paths(db)? {
        if !referenced.contains(&rel) {
            // **Counted as it happens**, before the next statement can fail: a fault part-way through this loop leaves
            // `tally` mutated behind, and "rows were deleted while the run said nothing" is precisely the silence review
            // caught — most likely on a run where the author had just deleted every figure link, so `images_seen` is zero.
            tally.pruned += db::unlink_media_path(db, &rel)?;
        }
    }
    tally.pruned += db::prune_unlinked_media(db)?;
    tally.swept = true;
    Ok(())
}

/// The pass stopped early: everything it never reached counts as unchecked, and `swept` stays false.
///
/// A fault in the **last** note, or inside the sweep itself, leaves nothing unexamined — so `unchecked` can legitimately be
/// zero after a stopped pass. That is why "did the stale-figure sweep finish" is its own field rather than something a reader
/// infers from a count that is zero for three unrelated reasons.
fn tally_after_stop(tally: &mut MediaTally) -> MediaTally {
    tally.unchecked = tally
        .unchecked
        .saturating_add(tally.total.saturating_sub(tally.processed));
    tally.clone()
}

/// Which §8 class a provider's refusal belongs to.
///
/// A **variant** answers the one class that has a variant of its own, and an [`EmbedError::Unavailable`] payload is
/// compared to the class constants **in full**, not as a substring. The first version used `contains`, which made a
/// reported §8 class depend on how an error happened to be worded: rephrasing a message that mentions a budget would move
/// refusals between classes with no code change to review, and a payload that merely *contains* `over-budget` — a path, a
/// model id — would be read as a budget refusal. Exact equality moves the mapping only when a provider starts emitting a
/// different class, which is the only thing that should move it.
///
/// Anything unrecognised answers `vision-unavailable`, because §8's set is closed and "this build and model pair did not
/// serve the request" is the claim left over. That default is a decision, not a discovery: a future provider error with no
/// class of its own lands here quietly, so adding a refusal reason must add it here in the same change.
fn image_refusal_of(error: &EmbedError) -> MediaRefusal {
    if matches!(error, EmbedError::TokenBudgetExceeded { .. }) {
        return MediaRefusal::OverBudget;
    }
    if let EmbedError::Unavailable(message) = error {
        if message == media::OVER_BUDGET {
            return MediaRefusal::OverBudget;
        }
        if message == media::UNDECODABLE {
            return MediaRefusal::Undecodable;
        }
    }
    MediaRefusal::VisionUnavailable
}

/// The four image fields of a report, spelled once so the two arms that run the pass cannot count them differently.
fn media_fields(media: &MediaTally) -> EmbedReport {
    EmbedReport {
        model: None,
        outcome: EmbedOutcome::Current,
        chunks_seen: 0,
        current: 0,
        embedded: 0,
        refused_budget: 0,
        busy_gap: 0,
        errors: 0,
        verification: CAVEAT,
        artifact_check: None,
        workers: None,
        fetched: None,
        offline: false,
        images_seen: media.seen,
        images_embedded: media.embedded,
        images_refused: media.refusals.len(),
        image_refusals: media.refusals.clone(),
        images_unchecked: media.unchecked,
        images_sweep_ran: media.swept,
        images_pruned: media.pruned,
    }
}

/// The model id this run embeds against, or arm (ii)'s state.
///
/// Two configuration states refuse, in this order because the first is the one a fresh install has:
/// `provider = "none"` (there is no provider to ask, so no id can be named), then a `local-model` provider with
/// no `model` value. [`Config::load`] already refuses the second for a file it parsed, so that arm is reachable
/// only from a hand-built [`Config`] — it stays a refusal rather than a guess because inventing an id would be a
/// second source of model identity (§4).
fn configured_model(cfg: &Config) -> Result<String, EmbedOutcome> {
    match cfg.embeddings.provider {
        EmbeddingProvider::None => Err(EmbedOutcome::ConfigurationDisabled {
            key: "embeddings.provider",
            id: Some("none".to_string()),
            // No hint (**F41**): embeddings were switched off, which is a decision rather than a missing file.
            hint: None,
        }),
        EmbeddingProvider::LocalModel | EmbeddingProvider::OpenAiCompatible => cfg
            .embeddings
            .model
            .clone()
            .ok_or(EmbedOutcome::ConfigurationDisabled {
                key: "embeddings.model",
                id: None,
                // The key is genuinely absent here, so there is no install that completes the configuration; the
                // sentence stays short and F35's strict-config message is where a typo gets caught.
                hint: None,
            }),
    }
}

/// The id to print on an arm-(ii) report: whatever configuration named, even when it names nothing.
fn configured_id(cfg: &Config) -> Option<String> {
    cfg.embeddings.model.clone()
}

/// Whether the artifact is a file that can be opened, spelled the way `models` spells every access to it.
fn artifact_file_exists(artifact: &Path) -> bool {
    let statable = walk::to_open_path(&models::access(artifact));
    std::fs::metadata(statable).is_ok_and(|meta| meta.is_file())
}

/// The command arm (iii) names: the doors this build can actually walk through.
///
/// Public because `np search`'s refusals name the same doors ([`NoProvider::sentence`]): one definition of "what do I run
/// now", two callers, so a build that compiled `fetch` out cannot be told to run it by one sentence and refused by another.
///
/// Two of them exist now. `np models fetch` shipped in R7b, and this hint used to say the opposite — "fetching a model is
/// not implemented in this build" — which sent an operator away from the one command that could finish the job on a machine
/// with a network. A `fetch`-less build still gets the truth for its own shape rather than a name it cannot run.
#[cfg(feature = "fetch")]
pub fn install_hint(workspace: &Path) -> String {
    format!(
        "fetch it with `np models fetch {} --id <id>`, or install bytes you already hold with \
         `np models install {} --id <id> --path <file> --dim <width> --sha256 <digest>`{}",
        workspace.display(),
        workspace.display(),
        runtime_caveat()
    )
}

/// The same advice for a build that compiled the fetch path out: naming `np models fetch` here would send a reader to a
/// help screen that has never heard of it.
#[cfg(not(feature = "fetch"))]
pub fn install_hint(workspace: &Path) -> String {
    format!(
        "install the artifact with `np models install {} --id <id> --path <file> --dim <width> --sha256 <digest>` \
         (this build has no fetch command){}",
        workspace.display(),
        runtime_caveat()
    )
}

/// One clause, both shapes of [`install_hint`], for the case where the artifact is obtainable and this **binary** still
/// cannot embed: `fetch` is default-on while `local-model` is default-off, so a `cargo install np-cli` build can download
/// 309 MB and meet F40's arm (v) anyway. A remedy that ends in the same refusal is worse than no remedy, because the reader
/// has now spent bandwidth to be told nothing.
///
/// Empty string when the runtime IS compiled in — this clause is a fact about the artifact being run, and asserting it about
/// a build that can embed would be the F40 mistake wearing a hint. Pinned both ways by
/// `an_install_hint_says_when_this_binary_cannot_embed_anyway`, whose two halves are graded by different gate rows.
pub fn runtime_caveat() -> &'static str {
    if cfg!(feature = "local-model") {
        ""
    } else {
        " — and note that this binary links no embedding runtime, so `np embed` will still refuse until you run one \
         built with `--features local-model`"
    }
}

/// Pair each owed chunk with the vector a provider returned for it, refusing anything that cannot be stored.
///
/// Two refusals, both before a transaction opens: a batch that did not answer one vector per text (an
/// off-by-one here would stamp chunk B's digest onto chunk A's vector, which no later check could see), and a
/// vector whose width is not the run's — the schema's `CHECK (length(vector) = 4 * dim)` would refuse the write
/// mid-transaction, and §6 asks for the refusal to come first with both numbers in it.
fn paired<'a>(
    batch: &'a [Owed],
    vectors: &'a [Vector],
    dim: usize,
) -> Result<Vec<(&'a Owed, &'a Vector)>, String> {
    if vectors.len() != batch.len() {
        return Err(format!(
            "the provider answered {} texts with {} vectors; a batch is written one-for-one or not at all",
            batch.len(),
            vectors.len()
        ));
    }
    if let Some(wide) = vectors.iter().find(|vector| vector.dim() != dim) {
        return Err(format!(
            "the provider produced {} dimensions but this run stores {}; refused before any write",
            wide.dim(),
            dim
        ));
    }
    Ok(batch.iter().zip(vectors.iter()).collect())
}

/// Write one batch under its own transaction, or report the lock that stopped it.
///
/// The transaction is opened *after* the provider ran and committed before the next batch is inferred — §6's
/// "model work never runs inside a transaction", and the reason an interrupted run leaves a consistent
/// database. `upsert_vector`'s `ON CONFLICT` keeps a repeated batch idempotent rather than fatal (§5).
fn write_batch(
    db: &Db,
    model: &str,
    rows: &[(&Owed, &Vector)],
) -> Result<BatchWrite, EmbedRunError> {
    let now = now_secs();
    let tx: Transaction<'_> = db.0.unchecked_transaction().map_err(DbError::from)?;
    let mut inner: Result<(), DbError> = Ok(());
    for (owed, vector) in rows {
        inner = db::upsert_vector(
            db,
            owed.id,
            model,
            vector.dim(),
            &vector.to_bytes(),
            &owed.digest,
            now,
        );
        if inner.is_err() {
            break;
        }
    }
    let outcome = match inner {
        Ok(()) => tx.commit().map_err(DbError::from),
        Err(err) => {
            // Nothing is stamped for a batch the database refused. The explicit rollback says so; dropping
            // `tx` would do the same work, and doing neither would leave a half-written batch open.
            let _ = tx.rollback();
            Err(err)
        }
    };
    match outcome {
        Ok(()) => Ok(BatchWrite::Written),
        Err(err) if is_busy(&err) => Ok(BatchWrite::Busy),
        Err(err) => Err(err.into()),
    }
}

/// Whether a failure is the lock §6 wants reported as a gap rather than raised as a fault.
fn is_busy(err: &DbError) -> bool {
    matches!(
        err,
        DbError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::DatabaseBusy,
                ..
            },
            _
        ))
    )
}

/// Whole seconds for `embeddings.embedded_at`, mirroring `np_core::scan`'s private `now_secs` and
/// `np-cli`'s: a clock reading before the epoch contributes `0` rather than failing the run.
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}
