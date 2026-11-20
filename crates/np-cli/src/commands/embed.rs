use crate::commands::common::emit_json;
use crate::commands::common::load_config;
use crate::commands::common::workspace_db;
use crate::commands::common::writer_lock;
#[cfg(feature = "fetch")]
use np_core::catalog::size_human;
use np_core::config::Config;
use np_core::config::EmbeddingProvider;
use np_core::db::model_row;
use np_core::embed::ArtifactCheck;
use np_core::embed::Embedder;
use np_core::embed::NoEmbeddingProvider;
use np_core::embed_run;
use np_core::embed_run::EmbedOutcome;
use np_core::embed_run::EmbedReport;
use np_core::embed_run::NoProvider;
use np_core::embed_run::Resolution;
use std::error::Error;
use std::io;
use std::io::Write;
use std::path::Path;

/// Whether this run may fetch a pinned artifact it is missing, resolved from argv before anything else.
///
/// Three values rather than a `bool` because "the operator said yes" and "nobody said anything" must not collapse: the
/// first ignores the config file, the second defers to it. Resolution order is **argv → environment+file** — argv last
/// in the chain of authority but highest, so `--no-auto-fetch` outranks both `[models] auto_fetch = true` and an
/// accidentally exported variable, which is what makes the flag worth having on a metered link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutoFetch {
    /// Neither flag given: ask `Config::auto_fetch_allowed`, which consults `[models] auto_fetch` and `NP_NO_AUTO_FETCH`.
    Deferred,
    /// `--auto-fetch`: fetch even if the file says no (the environment kill switch still wins — see [`AutoFetch::allowed`]).
    Forced,
    /// `--no-auto-fetch`: never fetch from this run.
    Refused,
}

impl AutoFetch {
    pub(crate) fn from_flags(on: bool, off: bool) -> Self {
        if on {
            Self::Forced
        } else if off {
            Self::Refused
        } else {
            Self::Deferred
        }
    }

    /// The answer for this run. `NP_NO_AUTO_FETCH` overrides even `--auto-fetch`, in one direction only: a switch that
    /// an argument can defeat is not a safety switch, and the person who needs it (CI, an air-gapped host) is usually
    /// the one who does not control the argv someone else typed.
    pub(crate) fn allowed(self, cfg: &Config) -> bool {
        let file_or_env = cfg.models.auto_fetch_allowed();
        match self {
            Self::Refused => false,
            Self::Deferred => file_or_env,
            // The operator asked for this fetch here and now, so the file's "no" does not bind them — but the kill switch
            // still does. That asymmetry is the whole design: argv may open a door, only the environment may bolt it.
            Self::Forced => !np_core::config::ModelsConfig::env_forbids_fetch(),
        }
    }
}

/// Runs the embed half and hands back its report, printing nothing about the outcome.
///
/// Split out of [`embed_workspace`] for `np setup`, which needs the report as a **value** to compose one document from two
/// steps — the alternative was to print JSON and parse it back, which is how two representations of one fact start to
/// disagree. Exit-code decisions stay with the command that owns the caller's expectation.
pub(crate) fn embed_report(
    workspace: &Path,
    auto_fetch: AutoFetch,
    announce: bool,
    progress: &dyn np_core::progress::Progress,
) -> Result<EmbedReport, Box<dyn Error>> {
    let cfg = load_config()?;
    // Destructured rather than reached through the struct: the borrow below has to outlive the statement that takes it,
    // and `Resolution::Provider` needs `&mut` into the box that `state` owns.
    let Resolved {
        mut state,
        artifact_check,
        workers,
        fetched,
        // Not read here: `np embed` refuses when it cannot embed, so "a fetch was declined" adds nothing to a refusal that already
        // names the command. `np search --json` is the caller that needs the distinction, and it reads the field directly.
        fetch_declined: _,
    } = embed_provider(
        &cfg,
        workspace,
        auto_fetch.allowed(&cfg),
        announce,
        progress,
    )?;
    // The reason a provider could not be built travels as a value — [`ProviderState::ArtifactAbsent`] for a missing file,
    // [`ProviderState::Placeholder`]'s reason for the configuration states. Handing over a bare placeholder is how R6c1 D-2
    // happened: an absent artifact became a provider that declares 0 dimensions, and the run refused with a sentence about
    // widths when the truth was a missing file (see [`Resolution`]).
    //
    // Both live providers and placeholders map to `Resolution::Provider`, which is exactly what this call site produced
    // before the states were split — the two arms embed can see are unchanged by construction, and that is the
    // reason `ProviderState` was
    // allowed to exist at all.
    //
    // `as_mut`, not `as_ref`: a provider is the thing that can open a projector, so the run holds it mutably for as long
    // as the resolution lives (`Resolution::Provider`'s note says why).
    // **This mapping splits by build shape.** In a binary that links no runtime, `ProviderState::Ready` holds the
    // seam's placeholder — a provider whose every batch answers `RuntimeNotBuilt`. Handing it to the run is how an
    // operator who installed a model correctly (digest verified, 295.5 MB) got `outcome owed and not produced` with
    // `errors 1` — the report-integrity arm, quoting a per-batch error, after the corpus had been read. This build now
    // refuses before any of that, naming the feature and the two ways forward. A `local-model` build keeps the original
    // mapping untouched — there, "the bytes would not load" genuinely *is* arm (iv), and its own test says so.
    #[cfg(not(feature = "local-model"))]
    let resolution = match &mut state {
        ProviderState::Ready(_) => Resolution::RuntimeNotBuilt {
            id: cfg.embeddings.model.clone().unwrap_or_default(),
        },
        ProviderState::Placeholder { embedder, .. } => Resolution::Provider(embedder.as_mut()),
        ProviderState::ArtifactAbsent => Resolution::ArtifactAbsent,
        // The offline arm: the fetch was allowed, attempted, and nothing answered; the run says so before it reads
        // a chunk.
        ProviderState::FetchSkippedOffline { id } => Resolution::FetchSkippedOffline {
            // Cloned because this match borrows `state` mutably for the provider arm; the offline arm needs a value that
            // outlives the borrow.
            id: id.clone(),
        },
    };
    #[cfg(feature = "local-model")]
    let resolution = match &mut state {
        ProviderState::Ready(embedder) | ProviderState::Placeholder { embedder, .. } => {
            Resolution::Provider(embedder.as_mut())
        }
        ProviderState::ArtifactAbsent => Resolution::ArtifactAbsent,
        ProviderState::FetchSkippedOffline { id } => Resolution::FetchSkippedOffline {
            // Cloned because this match borrows `state` mutably for the provider arm; the offline arm needs a value that
            // outlives the borrow.
            id: id.clone(),
        },
    };
    // **The reporter travels.** Every CLI caller of `embed_report` passes `progress::none()` (its bytes are a contract), so
    // forwarding the parameter here changes nothing for them; `np serve` is the one caller that hands in a real reporter,
    // and this is the call that makes its `embedding` and `writing` lines exist. Handing `none()` to the run instead — which
    // is what the first version of this line did — silences both phases on the wire while every test still passed, because
    // np-core's own seam tests call `embed_with_inject` directly and never come through here.
    let mut report = embed_run::embed_with_resolution(workspace, &cfg, resolution, progress)?;
    // What was concluded about the model's bytes travels on the report rather than being printed here, so `--json`
    // and the text summary cannot disagree about whether this run verified anything (the same reason the verification
    // caveat is a
    // field). `None` says no local artifact was involved — a `provider = "none"` run has nothing to claim.
    report.artifact_check = artifact_check;
    // Likewise the worker count: decided where the provider was built, printed here. `None` means **no local-model
    // provider was constructed** — which is not the same sentence as "nothing was embedded": a `local-model` run whose
    // notes were all current still built a provider, and its report states the count it would have used.
    report.workers = workers;
    // **The machine-readable answer to "did this invocation download a model?"** — set here rather than inside the run
    // because the transfer happened upstream, in provider resolution, and the document that must not contradict the text
    // summary is this one object.
    report.fetched = fetched;
    Ok(report)
}

/// The sentence for an embed run that did not succeed. `np embed` prints it on stderr; `np setup` reuses it **verbatim** so
/// the two commands cannot describe one stopped state in two voices.
///
/// `detail()` is `None` only for arm (i), which returns above; the empty fallback keeps this from being a second place that
/// has to know which arms carry a sentence.
pub(crate) fn embed_refusal(report: &EmbedReport) -> String {
    let label = report.outcome.label();
    let detail = report.outcome.detail().unwrap_or_default();
    // **A refusal must name its state once.** `np embed` on a machine with no network printed
    // "np: offline: offline: could not reach the pinned origin …" — measured on the packaged 0.3.0 asset, which is the first release shape
    // where this arm is reachable at all (before v0.3.0 the runtime was compiled out, so nobody ran it). The duplication is structural:
    // the line is `label: detail`, and one arm's detail began by restating its own label. Rather than edit that sentence and leave the
    // next arm free to repeat the same trick, the composition drops a leading restatement. Pinned in `np-core` by an invariant over every
    // arm's label and detail, so a future arm cannot reintroduce it either.
    let prefix = format!("{label}: ");
    let detail = match detail.strip_prefix(prefix.as_str()) {
        Some(rest) => rest.to_string(),
        None => detail,
    };
    format!("{label}: {detail}")
}

/// **Say it out loud and keep going.** One stderr line when the model row on file declares instruction prefixes this build's
/// pin does not, plus the value `--json` carries under `prefix_drift` (absent when there is no drift, so every existing document shape is
/// untouched).
///
/// Why a warning rather than a refusal: `np models install` exists for bytes a person holds, and it cannot assume the pin describes them — a
/// model installed from a paper's recommended instruction prefix is a deliberate act, and refusing it would close the only door that works
/// offline. It is not nothing either: prefixes change vector *bytes*, so a drifted index silently stops being comparable with what
/// `np models fetch` would install, and both honest commands then refuse to change the declaration (`install` will not rewrite an identity;
/// `fetch` returns `IdentityDisagrees` before transferring a byte). Before this, the only repair was SQL.
///
/// Which halves differ is reported separately, because they are not symmetric: the document prefix is what every stored vector was computed
/// under, while the query prefix affects only future queries. And the case that is *not* warned about matters as much — this build's pin
/// declares empty prefixes (`pins::PINNED_IDENTITIES`), so an install that omits both flags agrees with it and stays silent. The operator's
/// review row was that case, and the card that claimed otherwise has been corrected.
pub(crate) fn prefix_drift_notice(
    cfg: &Config,
    workspace: &Path,
    id: &str,
) -> Result<Option<np_core::pins::PrefixDrift>, Box<dyn Error>> {
    let db = workspace_db(cfg, workspace)?;
    let Some(row) = model_row(&db, id)? else {
        return Ok(None);
    };
    let Some(drift) = np_core::pins::prefix_drift(id, &row.prefix_doc, &row.prefix_query) else {
        return Ok(None);
    };
    let halves = match (drift.doc, drift.query) {
        (true, true) => "document and query prefix",
        (true, false) => "document prefix",
        (false, true) => "query prefix",
        (false, false) => return Ok(None),
    };
    let _ = writeln!(
        io::stderr(),
        "np: note: `{id}` is recorded with a {halves} this build does not declare for that id — its vectors mean \
         something different from a fetched model's, and no command rewrites them (`np models list` shows the \
         declaration on file)"
    );
    // …and the same fact as a diagnostic, with the id as `what`. The stderr sentence
    // above is pinned output and stays byte-for-byte; this reaches only whoever asked for diagnostics, which is why both
    // can exist without a doubled line. Which halves drifted travels in the message rather than in `expected`/`actual`,
    // because those keys are for numbers and "the document prefix" is not one.
    tracing::warn!(
        target: np_core::diagnostics::target::EMBED,
        rule = np_core::diagnostics::rule::PREFIX_DRIFT,
        what = id,
        "`{id}` is stored with a {halves} this build does not declare for that id"
    );
    Ok(Some(drift))
}

/// Insert `prefix_drift` into a document only when there is drift to report. A key that is usually null would be schema noise; a key that
/// appears exactly when the reader has something to act on is a signal.
pub(crate) fn insert_drift(
    mut doc: serde_json::Value,
    drift: Option<np_core::pins::PrefixDrift>,
) -> Result<serde_json::Value, Box<dyn Error>> {
    if let (Some(obj), Some(drift)) = (doc.as_object_mut(), drift) {
        obj.insert("prefix_drift".to_string(), serde_json::to_value(drift)?);
    }
    Ok(doc)
}

/// `np embed`: run the half, print it in the channel the caller asked for, and turn a stopped run into exit 1.
///
/// Three of the four arms leave as exit 1 with the arm's sentence on stderr *and* in the printed report: two
/// channels for one fact, the posture `np models list` already takes for a directory-level failure. No new exit
/// code is introduced for it, and clap keeps argv failures at 2.
pub(crate) fn embed_workspace(
    workspace: &Path,
    json: bool,
    auto_fetch: AutoFetch,
) -> Result<(), Box<dyn Error>> {
    // The lock spans the run: an `embed` writes vectors and re-renders `INDEX.md`, and a second writer in the same
    // vault is what this lock exists to stop.
    let cfg_for_lock = load_config()?;
    let _writer = writer_lock(&cfg_for_lock, workspace, "embed")
        .map_err(|failure| failure.report_cli(json))?;
    let report = embed_report(workspace, auto_fetch, !json, np_core::progress::none())?;
    // Read the row that was actually used rather than re-deriving from argv: `[embeddings] model` is what `embed_run` embedded for, and a
    // config naming something other than the installed row is exactly the disagreement worth noticing.
    let drift = match report.model.as_deref() {
        // `None` here means nothing was configured or named, which cannot have a drifted row: no id, nothing on file to compare.
        Some(id) => prefix_drift_notice(&load_config()?, workspace, id)?,
        None => None,
    };
    if json {
        let doc = insert_drift(serde_json::to_value(&report)?, drift)?;
        emit_json(&doc);
    } else {
        print_embed_summary(&report);
    }
    if report.succeeded() {
        return Ok(());
    }
    Err(embed_refusal(&report).into())
}

/// What `[embeddings]` resolved to, and — on the one path that could — what it proved about the artifact's bytes.
///
/// The second field exists because the check happens *here* (this is where the `models` row and its artifact path are
/// both at hand) while the report is written downstream. Carrying it in one value keeps `embed_provider` honest
/// without an out-parameter, and keeps the alternative — verifying inside `embed_run`, which is handed a provider and
/// cannot know how its bytes were produced — off the table.
pub(crate) struct Resolved {
    /// What `[embeddings]` resolved to, including the states where nothing can embed. This field replaced the
    /// `Option<Box<dyn Embedder>>` this field used to be: an `Option` could say "no provider" without saying which of five
    /// reasons, and only one of those reasons was ever explained.
    pub(crate) state: ProviderState,
    /// The verify-on-change result, or `None` when no local artifact was read at all.
    pub(crate) artifact_check: Option<ArtifactCheck>,
    /// The worker-thread plan for this run, or `None` when no local-model provider was built (the arms that
    /// `provider = "none"` have no worker count to report).
    pub(crate) workers: Option<np_core::embed_run::WorkerPlan>,
    /// What this run **transferred from an origin** before it could embed, or `None` when nothing arrived.
    ///
    /// Produced where the fetch happens — provider resolution — and copied onto the report by [`embed_workspace`] for the
    /// same reason `artifact_check` and `workers` are: the fact is decided next to the config and the filesystem, while the
    /// document that must not contradict itself is written downstream.
    pub(crate) fetched: Option<np_core::embed_run::FetchedRecord>,
    /// The artifact is missing, this build *could* have fetched it (pinned id, `local-model` and `fetch` compiled in),
    /// and policy said no — `[models] auto_fetch = false` or `NP_NO_AUTO_FETCH=1`.
    ///
    /// Carried as a fact rather than re-derived by each caller because "could this build have downloaded it" is one rule with two
    /// callers: `np embed` refuses (it has nothing to answer with), `np search --json` degrades and must say *why* it degraded.
    /// A second copy of the predicate in the search path would eventually disagree with the first, and the reader would see an
    /// `artifact-missing` note about a workspace where somebody switched fetching off.
    pub(crate) fetch_declined: bool,
}

/// What `[embeddings]` and the `models` table added up to, as one value.
///
/// Four states, which is how many answers exist: a provider this run can embed with, a provider-shaped placeholder for a
/// state that refuses before a vector is asked for, a filesystem fact no downstream code can re-derive, and — since
/// a fetch that was permitted, attempted, and met no connection. The last two look alike from the filesystem
/// (no artifact on disk) and must not share a sentence: one names a file to install, the other names a cable to plug in.
pub(crate) enum ProviderState {
    /// A local provider built from the installed row. Nothing here claims its bytes verified anything — that is
    /// [`Resolved::artifact_check`]'s job, taken where the row and the path were both in hand.
    Ready(Box<dyn Embedder>),
    /// A state that refuses before asking for a vector: configuration off, no model named, no row, an unusable declared
    /// width. The placeholder is what [`np_core::embed_run`] wants for three of those four (its arm (ii) derives the arm from
    /// configuration and the table itself), and the reason is what `np search` needs — so both travel.
    Placeholder {
        embedder: Box<dyn Embedder>,
        reason: NoProvider,
    },
    /// The row says installed and the file is not there. Not a configuration state, and not recoverable downstream: only
    /// this function holds the row and the path at the same time (R6c1 D-2).
    ArtifactAbsent,
    /// A fetch was permitted, attempted, and **nothing answered**. Not a filesystem fact and not a
    /// configuration state: the artifact is absent *and* unreachable, which is its own answer with its own remedy
    /// (connect, re-run) and must not be dressed as either neighbour.
    ///
    /// Only the trigger can construct it, so a build without `local-model` + `fetch` never does — but the variant stays
    /// compiled in rather than gated, because the matches over `ProviderState` are exactly the places a future
    /// "search may fetch too" change must be forced to answer. Gating it would hand them a `_` arm instead of an error.
    #[cfg_attr(not(all(feature = "local-model", feature = "fetch")), allow(dead_code))]
    FetchSkippedOffline {
        /// The pinned model that could not be obtained.
        id: String,
    },
}

/// A resolution that refuses for one of the four placeholder reasons, with the seam's own placeholder provider.
fn placeholder(reason: NoProvider) -> Resolved {
    Resolved {
        state: ProviderState::Placeholder {
            embedder: Box::new(NoEmbeddingProvider),
            reason,
        },
        artifact_check: None,
        workers: None,
        fetched: None,
        fetch_declined: false,
    }
}

/// Attach "a fetch was possible and policy refused it" to a resolution that did not produce a provider.
///
/// Gated on the state on purpose: with a provider in hand nothing was declined, and letting a caller stamp `true` beside a
/// `Ready` would be a document contradicting itself. Applied at each non-`Ready` return inside the local-model branch, which is
/// where the facts behind it (row, artifact, pin) are all in scope.
pub(crate) fn declined(mut resolved: Resolved, fetch_declined: bool) -> Resolved {
    resolved.fetch_declined = fetch_declined && !matches!(resolved.state, ProviderState::Ready(_));
    resolved
}

/// A resolution for the row-installed-file-absent state: no provider at all, by the reason in [`ProviderState`].
fn artifact_absent() -> Resolved {
    Resolved {
        state: ProviderState::ArtifactAbsent,
        artifact_check: None,
        workers: None,
        fetched: None,
        fetch_declined: false,
    }
}

/// The resolution for "we asked and nothing answered": no provider, and the offline arm as the answer.
#[cfg(all(feature = "local-model", feature = "fetch"))]
fn fetch_offline(id: &str) -> Resolved {
    Resolved {
        state: ProviderState::FetchSkippedOffline { id: id.to_string() },
        artifact_check: None,
        workers: None,
        // Nothing arrived, so nothing is claimed. `EmbedReport::offline` is set downstream from the arm itself, which is
        // why this field stays `None` rather than carrying a half-populated record.
        fetched: None,
        // Not declined: it was *attempted*. The distinction is the whole point — "we asked and nothing answered" and
        // "we were not allowed to ask" are different notes with different next commands.
        fetch_declined: false,
    }
}

/// What `[embeddings]` names, as one of three [`ProviderState`]s.
///
/// [`ProviderState::ArtifactAbsent`] means **exactly that one thing** — the row says installed, the file is not there — and
/// `np embed` turns it into [`Resolution::ArtifactAbsent`] so the artifact-absent arm can say so. The four states that are *not* about
/// bytes (`provider = "none"`, a missing [`np_core::config::EmbeddingsConfig::model`], an id naming no installed row, a row whose
/// declared width cannot be stored) hand over [`NoEmbeddingProvider`] as a **placeholder** — what [`np_core::embed_run`] wants,
/// since it derives those arms from configuration and the `models` table, the sources that hold the fact — **and** a
/// [`NoProvider`] reason, which is what `np search` prints instead of the placeholder's generic refusal.
///
/// Two of those four reasons are unreachable from argv: `Config::load` refuses `local-model` with no `model`, and
/// `np models install --dim` validates 1..=65535 at the command line. They stay because a `Config` can be built in memory, and
/// because the alternative is a state with no sentence — and both paths were measured, not guessed.
///
/// The row is read through a connection that closes when this function returns, before `embed_run` opens its
/// own: np-cli holds no SQL of its own, and one workspace's [`np_core::db::ModelRow`] is where a local provider's
/// identity comes from.
/// Resolve `[embeddings]` into something that can embed — or into the one refusal that explains why nothing can.
///
/// `auto_fetch` is the fetch trigger, and it is a parameter rather than a global so that each call site states its
/// permission: `np embed` passes the operator's decision, `np search` passes `false` unconditionally. Even when true, a
/// fetch needs three more things — an id this build **pins**, a build that can run inference, and the compile-time
/// `fetch` feature — so no configuration value alone can make a binary dial out.
/// `announce` is text mode only: `false` under `--json`, where stdout holds exactly one document and a progress line
/// would corrupt it. The fetch notice is therefore suppressed rather than smuggled onto stderr, keeping that promise
/// the simple thing it is: a program reads the `fetched` key instead.
pub(crate) fn embed_provider(
    cfg: &Config,
    workspace: &Path,
    auto_fetch: bool,
    announce: bool,
    progress: &dyn np_core::progress::Progress,
) -> Result<Resolved, Box<dyn Error>> {
    // A build with no fetch engine has no bytes to narrate, so the reporter is genuinely unused there. Naming it
    // `_progress` would make the gated call site below read like a throwaway; this says which half is conditional.
    let _ = &progress;
    match cfg.embeddings.provider {
        // The default. Nothing is built, nothing is contacted, and the report says *configuration disabled*
        // rather than a fault.
        EmbeddingProvider::None => Ok(placeholder(NoProvider::ConfigDisabled)),
        EmbeddingProvider::OpenAiCompatible => {
            // Unreachable through `Config::load`, which refuses this variant by name; kept as a refusal rather
            // than a fallthrough into the local branch, because dialling out is not what this build does.
            Err("embeddings.provider = \"openai-compatible\" names an endpoint provider this build cannot serve".into())
        }
        EmbeddingProvider::LocalModel => {
            let Some(id) = cfg.embeddings.model.as_deref() else {
                // Defensive: `Config::load` refuses this combination, so argv cannot produce it. See the function's note.
                return Ok(placeholder(NoProvider::ModelUnnamed));
            };
            let db = workspace_db(cfg, workspace)?;
            let models_dir = cfg.workspace_models_dir(workspace);
            let artifact = models_dir.join(id);
            // `mut` only where the trigger is compiled in: a fetch records the `models` row, so that arm re-reads it
            // below. Everywhere else the binding is read-only from birth, and the warning would be true of that build.
            #[cfg_attr(
                not(all(feature = "local-model", feature = "fetch")),
                allow(unused_mut)
            )]
            let mut row = model_row(&db, id)?;

            // ---- The fetch trigger: fetch the pinned artifact this workspace is missing. One place, before any provider
            // is constructed and long before any transaction opens — a download that starts mid-embed would hold a write
            // lock through someone else's egress.
            //
            // Four things must all be true, and each one is a different mistake prevented:
            //   * `auto_fetch`             — the operator said so (argv, config, or the default), and `NP_NO_AUTO_FETCH`
            //                              has already had its veto inside [`AutoFetch::allowed`];
            //   * `pins::identity(id)`     — this build pins bytes for the id. That is what makes a default-on fetch
            //                              defensible: the target is one known artifact from one origin verified by
            //                              digest, never "whatever config named";
            //   * `feature = "local-model"`— a binary that links no inference runtime must not spend 310 MB it can never
            //                              read; here it is a compile-time gate, not a runtime guess;
            //   * `feature = "fetch"`      — a build that compiled the HTTP client out cannot advertise a dial-out.
            //
            // Nothing here prints in `--json` mode (`announce`), and nothing here ever names a host: the only
            // origin fact allowed out is *which kind* of origin was resolved.
            #[cfg(all(feature = "local-model", feature = "fetch"))]
            let mut fetched_this_run: Option<np_core::embed_run::FetchedRecord> = None;
            #[cfg(all(feature = "local-model", feature = "fetch"))]
            let attempted_fetch = auto_fetch
                && np_core::pins::identity(id).is_some()
                && (row.is_none() || !artifact.is_file());
            #[cfg(all(feature = "local-model", feature = "fetch"))]
            if attempted_fetch {
                match fetch_pinned_missing(cfg, &db, &models_dir, id, announce, progress)? {
                    FetchStep::Offline => return Ok(fetch_offline(id)),
                    FetchStep::Fetched(record) => {
                        fetched_this_run = Some(record);
                        // The fetch records the `models` row itself, so a workspace that had neither row nor file is now a
                        // workspace with both — re-read rather than assume, and let the missing-row arm still answer
                        // if it somehow
                        // did not.
                        row = model_row(&db, id)?;
                    }
                }
            }
            #[cfg(not(all(feature = "local-model", feature = "fetch")))]
            let fetched_this_run: Option<np_core::embed_run::FetchedRecord> = None;
            #[cfg(not(all(feature = "local-model", feature = "fetch")))]
            let _ = (announce, &models_dir, &artifact);

            // The same facts the trigger above tests, with the operator's veto flipped — read BEFORE any fetch could have
            // run, so it describes the state the caller walked into rather than the state a fetch left behind. This is the only
            // place "could this build have downloaded it?" is asked: `np search` reads the answer off [`Resolved::fetch_declined`]
            // instead of restating the predicate and drifting from it.
            #[cfg(all(feature = "local-model", feature = "fetch"))]
            let fetch_declined = !auto_fetch
                && np_core::pins::identity(id).is_some()
                && (row.is_none() || !artifact.is_file());
            #[cfg(not(all(feature = "local-model", feature = "fetch")))]
            let fetch_declined = false;
            #[cfg(not(all(feature = "local-model", feature = "fetch")))]
            let _ = auto_fetch;

            let Some(row) = row else {
                // The missing-row arm: this is the state that used to print a sentence about `provider = "none"`.
                return Ok(declined(
                    placeholder(NoProvider::RowMissing { id: id.to_string() }),
                    fetch_declined,
                ));
            };
            // The width this run needs: `[embeddings] dim`, or the row's own declaration when the config
            // declares none. `embed_run` applies the same rule and refuses a disagreement before any write, so
            // a declared width that is not a number at all travels to it as the arm that names the row.
            let Some(configured) = cfg
                .embeddings
                .dim
                .map(usize::from)
                .or_else(|| usize::try_from(row.dim).ok())
            else {
                // Also defensive: `--dim` bounds what can be installed. Never a declined fetch — the row is here and its
                // bytes are not in question, so stamping that fact would attach the declined-fetch note to the wrong
                // state.
                return Ok(declined(
                    placeholder(NoProvider::RowDimUnusable {
                        id: id.to_string(),
                        declared: row.dim.to_string(),
                    }),
                    false,
                ));
            };

            // With the runtime compiled in, the provider is the one that loads llama.cpp **on first use**
            // (`local_model::LazyLocalEmbedder`): resolving a provider happens before anyone knows whether
            // the workspace owes a chunk, and an eager load would spend ~420 MiB of resident weights plus load time
            // on a run whose answer is "nothing changed". The two refusals that need no model — a row width that
            // is not this run's, and an artifact that is not a file — are still taken here, so the *artifact
            // absent* arm stays arm (iii) instead of arriving late as a failure.
            #[cfg(feature = "local-model")]
            {
                // Decide how loud llama.cpp may be on this run's stderr, once, before anything can load. The rules —
                // the five names,
                // which source wins, what an empty value means — live in `np_core::loglevel` beside the vocabulary; this call is only the
                // decision to opt in, because *this* process owns its stderr and a library quietly installing a global sink would not.
                // `Config::load` already refused a value that cannot be honoured, so the error arm is unreachable from argv; propagating
                // rather than unwrapping keeps it honest if that ever stops being true.
                let level = np_core::loglevel::resolve_level_from_env(cfg.embeddings.log_level)?;
                np_core::local_model::set_log_level(level);
                match np_core::local_model::LazyLocalEmbedder::new(
                    &row,
                    &artifact,
                    configured,
                    cfg.embeddings.max_tokens,
                    cfg.embeddings.threads,
                ) {
                    // Built lazily, so nothing is resident yet — which is exactly where D12's check belongs: a
                    // `verify_artifact` that finds the stamp intact costs one `stat`, so a run with nothing owed still
                    // pays nothing. When it does hash (the first run under this build, or bytes that moved) it refuses
                    // **before** `embed_run` reads a note, so a swapped artifact never earns a stamped vector.
                    Ok(embedder) => {
                        // **A build with the image code asks this workspace for a projector.** Which bytes qualify is
                        // np-core's rule, not this file's (`local_model::projector_artifact`: recorded by *this* database,
                        // present on disk, and either stamped or digest-matching), because two copies of that rule could
                        // disagree in exactly one direction — the CLI accepting a file the pin table would refuse.
                        //
                        // Three outcomes, and none of them is silent. `Ok(Some)` configures the projector, so figures encode.
                        // `Ok(None)` leaves the provider text-only and every figure then refuses as `projector-absent`, whose
                        // advice names the file to install or fetch — the class exists so this line does not have to print a
                        // warning of its own to be honest. `Err` is a recorded projector whose bytes are not the recorded
                        // ones, and it ends the run: encoding every figure with an encoder nobody vouched for, then stamping
                        // those rows as though the pinned pair produced them, is not a degraded mode (R6c2's defect, aimed at
                        // the second artifact this program loads).
                        #[cfg(feature = "vision")]
                        let embedder = match np_core::local_model::projector_artifact(
                            &db,
                            &row.id,
                            &cfg.workspace_models_dir(workspace),
                        ) {
                            Ok(Some(projector)) => embedder.with_mmproj(projector),
                            Ok(None) => embedder,
                            Err(err) => return Err(err.into()),
                        };

                        // Read before the embedder is boxed into a trait object: `worker_plan` is a method on the
                        // concrete type, and the plan belongs to the provider that was just built.
                        let plan = embedder.worker_plan();
                        match np_core::local_model::verify_artifact(&db, &row, &artifact) {
                            Ok(check) => Ok(Resolved {
                                state: ProviderState::Ready(Box::new(embedder)),
                                artifact_check: Some(check),
                                workers: Some(plan),
                                fetched: fetched_this_run,
                                fetch_declined: false,
                            }),
                            Err(err) => Err(err.into()),
                        }
                    }
                    Err(np_core::embed::EmbedError::ArtifactUnreadable { .. }) => {
                        Ok(declined(artifact_absent(), fetch_declined))
                    }
                    Err(err) => Err(err.into()),
                }
            }

            // Without the runtime there is nothing to load, and the seam's default half says so: every batch it is
            // handed answers `runtime not built`, which `embed_run` reports as arm (iv) rather than writing a
            // placeholder. Same two refusals and the same `None` for a missing artifact, so both builds resolve the
            // four arms identically and only the middle of the run differs.
            #[cfg(not(feature = "local-model"))]
            match np_core::embed::LocalModelProvider::new(&row, &artifact, configured) {
                // No digest check on this half: without the runtime nothing reads the artifact, so there is no claim
                // about its bytes to make — and printing one would be a false report.
                Ok(provider) => Ok(Resolved {
                    state: ProviderState::Ready(Box::new(provider)),
                    artifact_check: None,
                    workers: None,
                    fetched: fetched_this_run,
                    fetch_declined: false,
                }),
                // The row is installed and its width is the run's; only the bytes are missing. That is a fact
                // about the filesystem, not about configuration, so it cannot be recovered downstream — hence
                // `None` here rather than a placeholder provider whose declared width would then be compared
                // with the row's (R6c1 D-2).
                Err(np_core::embed::EmbedError::ArtifactUnreadable { .. }) => {
                    // The declined-fetch fact stays reportable without `local-model` as well; a build that cannot fetch simply
                    // never sets the flag, because its `fetch_declined` is compile-time false.
                    Ok(declined(artifact_absent(), fetch_declined))
                }
                Err(err) => Err(err.into()),
            }
        }
    }
}

/// The human-readable form of an embed report: the arm first, then the six counts, then the verification caveat.
///
/// Same aligned-label shape as [`print_summary`], and every line written to one locked stdout handle with its
/// result dropped for the same reason: a descriptor that refuses writes must not turn a finished run into a
/// panic. The verification line is printed even when nothing was embedded, because the whole point is that the
/// caveat is read at 2 a.m. rather than filed.
pub(crate) fn print_embed_summary(report: &EmbedReport) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "model      {}", report.model.as_deref().unwrap_or("-"));
    let _ = writeln!(out, "outcome    {}", report.outcome.label());
    if let Some(detail) = report.outcome.detail() {
        let _ = writeln!(out, "detail     {detail}");
    }
    // The path arm (iii) looked at, printed apart from the hint so a reader can `ls` it. It is a data-dir file
    // this program wrote, which is why naming it is safe.
    if let EmbedOutcome::ArtifactAbsent { artifact, .. } = &report.outcome {
        let _ = writeln!(out, "artifact   {}", artifact.display());
    }
    let _ = writeln!(out, "seen       {}", report.chunks_seen);
    let _ = writeln!(out, "current    {}", report.current);
    let _ = writeln!(out, "embedded   {}", report.embedded);
    let _ = writeln!(out, "refused    {}", report.refused_budget);
    let _ = writeln!(out, "busy-gap   {}", report.busy_gap);
    let _ = writeln!(out, "errors     {}", report.errors);
    // Images get their own line and no place in any other count: a corpus with no figures prints nothing here, so the
    // text-only report keeps the shape it had before images existed.
    if report.images_seen > 0 {
        // The image line, with the classes spelled out: a count of three refusals tells a reader nothing about whether their
        // vault is full of `.svg` (a format decision) or missing files (their own edit), and those two answers lead to
        // different commands. Classes come from `image_refusal_classes`, in one fixed order, so this line and `--json` cannot
        // drift apart.
        let classes = report
            .image_refusal_classes()
            .into_iter()
            .map(|(class, count)| format!("{class} {count}"))
            .collect::<Vec<_>>()
            .join(", ");
        let suffix = if classes.is_empty() {
            String::new()
        } else {
            format!(" ({classes})")
        };
        // `links`, because that is the unit: one figure written two ways counts twice here and once in `embedded`, and a
        // bare "seen 2 · embedded 1" reads like an unexplained loss.
        let _ = writeln!(
            out,
            "images     links {} · embedded {} · refused {}{suffix}",
            report.images_seen, report.images_embedded, report.images_refused
        );
    }
    // Its own line because it counts **notes**, not images, and its consequence is the run's most surprising one: nothing
    // was pruned. Silence here means every live note was read, which is exactly when the sweep is allowed to delete.
    if report.images_unchecked > 0 {
        let _ = writeln!(
            out,
            "unchecked  {} note(s) not read",
            report.images_unchecked
        );
    }
    // Said only for a run that looked at figures, because `false` means three different things (no pass, held off, stopped
    // early) and two of them are invisible in the counts above. What it always means is "nothing was decided about stale
    // figures", which is worth a line exactly when the reader has figures that could be stale.
    if report.images_seen > 0 && !report.images_sweep_ran {
        let _ = writeln!(
            out,
            "sweep      not completed — stale image rows may remain"
        );
    }
    let _ = writeln!(out, "verify     {}", report.verification);
    // Printed after the standing verification caveat because the two answer different questions: that one asks whether these
    // *vectors* were checked against a reference implementation, this one asks whether the model's *bytes* were.
    // Omitted entirely when no local artifact was involved — silence is the truthful report there, not "verified".
    if let Some(check) = &report.artifact_check {
        let _ = writeln!(out, "digest     {}", check.label());
    }
    // Last, for the same reason `digest` is: it answers a question about this run's machine rather than about the
    // vectors. Omitted when no local-model provider was built — printing `threads 4 of 16` for a `provider = "none"` run
    // would be a number about nothing.
    if let Some(plan) = &report.workers {
        let _ = writeln!(out, "threads    {}", plan.describe());
    }
}

// ---------------------------------------------------------------------------
// `np models` — install, fetch, hash and list.
//
// Three commands, and the whole of their intelligence is in `np_core::models`: these functions turn argv
// into an `Install`, open one workspace's database, call the library, and print what came back. That split is
// why there is no SQL here (every read and write lives in `db.rs` instead) and no `sha2`
// dependency (`hash` calls `np_core::hash::sha256_file`, and a test exists to keep that the only digest reader).
// ---------------------------------------------------------------------------

/// The two padded keys of `np models fetch`'s text report, spelled out so their shared column is a named thing a test can pin:
/// widening one alone used to misalign the block with every gate green, which is how the line below ended up one space wider.
#[cfg(feature = "fetch")]
pub(crate) const FETCHED_KEY: &str = "fetched     ";
#[cfg(feature = "fetch")]
const FREE_SPACE_KEY: &str = "free space  ";

/// The free-space line of `np models fetch`'s text report, in both arms.
///
/// Built by a function so the second arm can be asserted at all: a successful CLI fetch has to serve bytes that hash to the
/// **pinned** digest, which no fixture can do, so no end-to-end test ever reaches either sentence — which is precisely how this line
/// acquired a dangling preposition and a column one wider than its sibling `fetched     `. Both are pinned now.
///
/// Stated, never implied: an arm that could not run is not an arm that passed.
#[cfg(feature = "fetch")]
pub(crate) fn free_space_line(free: Option<u64>) -> String {
    match free {
        Some(free) => format!("{FREE_SPACE_KEY}{} before the fetch", size_human(free)),
        None => format!("{FREE_SPACE_KEY}not checked on this platform"),
    }
}
#[cfg(all(test, feature = "fetch"))]
mod tests {
    use super::*;

    #[test]
    fn the_free_space_line_names_its_number_and_keeps_its_column() {
        assert_eq!(
            FREE_SPACE_KEY, "free space  ",
            "the key's own bytes are the contract"
        );
        assert_eq!(
            FETCHED_KEY.len(),
            FREE_SPACE_KEY.len(),
            "the two keys must put their values in the same column"
        );
        let measured = free_space_line(Some(309_855_456));
        assert_eq!(measured, "free space  295.5 MB before the fetch");
        let declined = free_space_line(None);
        assert_eq!(declined, "free space  not checked on this platform");
        for line in [measured, declined] {
            assert!(
                line.starts_with(FREE_SPACE_KEY),
                "the line does not begin with the padded key: {line}"
            );
            assert!(
                !line[FREE_SPACE_KEY.len()..].starts_with(' '),
                "the value is indented twice and the block stops aligning: {line}"
            );
            assert!(
                !line.contains("before  "),
                "the dangling preposition came back: {line}"
            );
        }
    }

    #[test]
    fn an_unanswered_preflight_is_reported_as_unanswered() {
        assert!(free_space_line(None).contains("not checked"));
        assert!(!free_space_line(None).contains("ok"));
    }
}
