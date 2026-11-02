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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Transaction;
use serde::Serialize;
use thiserror::Error;

use crate::config::{abs_path, Config, ConfigError, EmbeddingProvider};
use crate::db::{self, Db, DbError};
use crate::embed::{EmbedError, Embedder, Vector};
use crate::hash::sha256_bytes;
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
    pub fn label(&self) -> &'static str {
        match self {
            Self::Current => "nothing owed",
            Self::ConfigurationDisabled { .. } => "configuration disabled",
            Self::ArtifactAbsent { .. } => "artifact absent",
            Self::NotProduced { .. } => "owed and not produced",
        }
    }

    /// The arm's one-line detail, or `None` for arm (i), which the counts already describe.
    pub fn detail(&self) -> Option<String> {
        match self {
            Self::Current => None,
            Self::ConfigurationDisabled { key, id } => Some(match id {
                Some(value) => format!("{key} = \"{value}\""),
                None => format!("{key} names nothing"),
            }),
            Self::ArtifactAbsent { hint, .. } => Some(hint.clone()),
            Self::NotProduced { reason } => Some(reason.clone()),
        }
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
}

impl EmbedReport {
    /// Whether this report may leave exit code 0 — arm (i) alone.
    pub fn succeeded(&self) -> bool {
        matches!(self.outcome, EmbedOutcome::Current)
    }

    /// A report that read nothing, for the arms that refuse before touching the corpus.
    fn unread(model: Option<String>, outcome: EmbedOutcome) -> Self {
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
        }
    }
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
/// a provider. np-cli still hands over [`crate::embed::NoEmbeddingProvider`] for those states, and that
/// placeholder can no longer reach a width comparison.
pub enum Resolution<'a> {
    /// A provider built from the configured row — the only thing this run can write a vector from, and the only
    /// [`Embedder::dim`] §6's wrong-dim refusal may quote.
    Provider(&'a dyn Embedder),
    /// The caller tried to build a local provider from the installed row and could not read its artifact file.
    ///
    /// Taken as the fact for this run instead of re-statting the path: a file appearing between the caller's
    /// check and here would otherwise turn a refusal into an embed attempt with no provider at all, which is a
    /// worse answer to the same question. The next run re-reads the directory.
    ArtifactAbsent,
}

/// Embed what this workspace owes, through the injected provider.
///
/// Equivalent to [`embed_with_inject`] with an empty hook.
pub fn embed(
    workspace: &Path,
    cfg: &Config,
    provider: &dyn Embedder,
) -> Result<EmbedReport, EmbedRunError> {
    embed_with_inject(workspace, cfg, provider, &mut |_| {})
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
) -> Result<EmbedReport, EmbedRunError> {
    run(workspace, cfg, resolution, &mut |_| {})
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
    provider: &dyn Embedder,
    between_batches: &mut dyn FnMut(&Path),
) -> Result<EmbedReport, EmbedRunError> {
    run(
        workspace,
        cfg,
        Resolution::Provider(provider),
        between_batches,
    )
}

/// The body of every entry point above.
fn run(
    workspace: &Path,
    cfg: &Config,
    resolution: Resolution<'_>,
    between_batches: &mut dyn FnMut(&Path),
) -> Result<EmbedReport, EmbedRunError> {
    // The same fence `crate::scan` raises: an empty data dir would derive a database path beside the current
    // directory, possibly inside the workspace being embedded.
    if cfg.data_dir.as_os_str().is_empty() {
        return Err(ConfigError::MissingField("data_dir").into());
    }
    let root = abs_path(workspace);
    let db_path = cfg.workspace_db(&root);
    let db = Db::open(&db_path)?;

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
            return Ok(EmbedReport::unread(
                Some(model_id.clone()),
                EmbedOutcome::ConfigurationDisabled {
                    key: "embeddings.model",
                    id: Some(model_id),
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
    if let Resolution::Provider(provider) = resolution {
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
    let witness: Option<&dyn Embedder> = match &resolution {
        Resolution::Provider(provider) => Some(*provider),
        Resolution::ArtifactAbsent => None,
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
        Resolution::Provider(_) | Resolution::ArtifactAbsent => {
            return Ok(EmbedReport {
                model: Some(model_id.clone()),
                outcome: EmbedOutcome::ArtifactAbsent {
                    model: model_id,
                    hint: install_hint(workspace),
                    artifact,
                },
                chunks_seen: seen,
                current,
                embedded: 0,
                refused_budget: 0,
                busy_gap: 0,
                errors,
                verification: CAVEAT,
                artifact_check: None,
                workers: None,
            });
        }
    };

    // ---- the writer loop: model work outside the transaction, one bounded transaction per batch.
    let mut embedded = 0usize;
    let mut refused_budget = 0usize;
    let mut busy_gap = 0usize;
    // The token numbers of the last over-budget refusal, kept for §6's report line.
    let mut budget_detail: Option<String> = None;
    // The provider's own sentence for the last refusal it raised, which becomes arm (iv)'s reason. Every
    // `EmbedError` message is input-free and secret-free by that type's own rule (§3).
    let mut reason: Option<String> = None;

    'batches: for batch in owed.chunks(EMBED_BATCH_CHUNKS) {
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
        between_batches(&db_path);
    }

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
    })
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
        }),
        EmbeddingProvider::LocalModel | EmbeddingProvider::OpenAiCompatible => cfg
            .embeddings
            .model
            .clone()
            .ok_or(EmbedOutcome::ConfigurationDisabled {
                key: "embeddings.model",
                id: None,
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

/// The command arm (iii) names.
///
/// §6 says "run `np models fetch`"; that command does not exist yet (R7b), and naming it would send a reader to
/// a help screen that has never heard of it. This names the one install path this build has, with its real
/// flags, and says fetching is missing rather than implying it is optional.
fn install_hint(workspace: &Path) -> String {
    format!(
        "install the artifact with `np models install {} --id <id> --path <file> --dim <width> --sha256 <digest>` (fetching a model is not implemented in this build)",
        workspace.display()
    )
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
