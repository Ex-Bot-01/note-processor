//! `np` — command line entry point for note-processor.
//!
//! Three command groups: `np scan`, `np embed`, and the M2a model commands under `np models`. All are parsers
//! and printers only — every decision about what an install means, which id is legal, or what state an artifact
//! is in lives in [`np_core::models`], so this file holds no SQL and defines no second SHA-256
//! ([`np_core::hash::sha256_file`] is the one digest in the program, and `np models hash` calls it).

use std::error::Error;
use std::io::{self, Write};
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use np_core::catalog::size_human;
use np_core::config::{Config, EmbeddingProvider};
use np_core::db::{model_row, orphan_total, Db};
use np_core::embed::{ArtifactCheck, Embedder, NoEmbeddingProvider};
use np_core::embed_run::{self, EmbedOutcome, EmbedReport, NoProvider, Resolution};
#[cfg(feature = "fetch")]
use np_core::fetch;
use np_core::hash::sha256_file;
use np_core::models::{
    self, FetchEntry, FetchState, Install, ListedModel, Listing, ModelError, Unclaimed,
    UnclaimedKind,
};
use np_core::scan::{scan, ScanReport};
use np_core::search::{self, Hit};
use serde_json::{json, Value};

/// §12's default top-k for `np search --limit`.
const DEFAULT_SEARCH_LIMIT: usize = 10;

#[derive(Parser)]
// The binary is `np`, but the package is `np-cli`; without `name` clap would print the package name
// in `--version` and `--help`.
#[command(name = "np", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Index a note workspace and regenerate its catalog.
    Scan {
        /// Workspace root to index.
        workspace: PathBuf,
        /// Print the report as JSON instead of a summary.
        #[arg(long)]
        json: bool,
    },
    /// Embed the notes a workspace owes, for the configured model.
    Embed {
        /// Workspace root to embed.
        workspace: PathBuf,
        /// Print the report as JSON instead of a summary.
        #[arg(long)]
        json: bool,
    },
    /// Rank the indexed notes and images against a text query.
    ///
    /// `<workspace>` is an operand for the same reason `scan` and `embed` take it: the index lives under this
    /// workspace's key ([`Config::workspace_db`]), so a command without one could not find a database to read.
    ///
    /// The command writes **no vector, no stamp and no note row** ([`np_core::search`] prepares nothing but `SELECT`s).
    /// Opening the workspace database is not covered by that promise: like every other command, `workspace_db` creates the
    /// directory and runs migrations if this is the first run here — "read-only" describes what search changes about the
    /// index, not what opening a database touches.
    ///
    /// Exit codes follow every other command: a missing argument is clap's 2, and anything the runtime refuses (no provider,
    /// no artifact to embed the query with, an id nobody installed while another holds the vectors) is 1. **Which kinds are
    /// indexed is not a refusal** — a note-only corpus is searchable, and so is an image-only one — and an index with no
    /// vectors at all prints one line and exits 0.
    Search {
        /// Workspace root whose index to read.
        workspace: PathBuf,
        /// Text to embed and rank against the stored vectors.
        query: String,
        /// Maximum number of hits to print, highest score first.
        #[arg(long, default_value_t = DEFAULT_SEARCH_LIMIT)]
        limit: usize,
        /// Print the hits as JSON instead of one line each.
        #[arg(long)]
        json: bool,
    },
    /// Install and inspect the model artifacts of one workspace.
    Models {
        #[command(subcommand)]
        command: ModelsCommand,
    },
}

/// The three model commands of `docs/design-m2-models-install.md` §2.
///
/// `<workspace>` is an operand of `install` and `list` rather than an option because both the `models` row
/// and the artifact live under one workspace's key ([`Config::workspace_db`], [`Config::workspace_models_dir`])
/// — a command without it could not resolve a database at all, and would "succeed" into a hashed path nobody
/// will ever scan. `hash` takes a file and touches no database.
#[derive(Subcommand)]
enum ModelsCommand {
    /// Install an artifact the human already has, verified against a declared digest.
    Install {
        /// Workspace root to install into.
        workspace: PathBuf,
        /// Model identity: `models.id` and the artifact's file name.
        #[arg(long)]
        id: String,
        /// The artifact file to install. It is read and copied, never moved or truncated.
        #[arg(long)]
        path: PathBuf,
        /// Declared vector width, 1 to 65535 — declared here, never probed.
        #[arg(long, value_parser = parse_dim)]
        dim: NonZeroU16,
        /// The digest this artifact must hash to: 64 hex characters, from `np models hash <file>`.
        #[arg(long)]
        sha256: String,
        /// Prefix added to documents. Empty when omitted.
        #[arg(long, default_value = "")]
        prefix_doc: String,
        /// Prefix added to queries. Empty when omitted.
        #[arg(long, default_value = "")]
        prefix_query: String,
        /// Print the outcome as JSON instead of one word.
        #[arg(long)]
        json: bool,
    },
    /// Fetch a pinned model's artifacts and record them in this workspace.
    ///
    /// The only command in this program that dials out (§12 rule 1), and it dials where `[models] base_url` says, or
    /// `NP_MODELS_BASE_URL` when the file leaves that key out, or the one origin baked into the pin table when neither
    /// speaks. Nothing else does: `np scan` never fetches, and `np embed` keeps refusing with
    /// §6's *artifact absent* arm rather than starting a nine-hundred-megabyte download inside a command that then
    /// writes rows. The automatic trigger belongs to the daemon (M4), not to a command line.
    #[cfg(feature = "fetch")]
    Fetch {
        /// Workspace root to fetch into. Its models directory receives the artifacts.
        workspace: PathBuf,
        /// Model identity to fetch: it must be one this build pins bytes **and** an identity for.
        #[arg(long)]
        id: String,
        /// Which member to fetch, repeatable (`main`, `mmproj`). Default: the members this binary can use — the
        /// projector is left out of a build without image support, and naming it explicitly is refused.
        #[arg(long = "member")]
        members: Vec<String>,
        /// Discard this id's fetch marker (and the partial download it names) and then fetch.
        ///
        /// §12 rule 4's explicit door: a marker means some process claimed this download, and liveness is consent, so
        /// no clock may expire it. If you know no fetch is running, this is how you say so.
        #[arg(long)]
        discard_marker: bool,
        /// Print the outcome as JSON instead of one line per member.
        #[arg(long)]
        json: bool,
    },
    /// Print a file's sha256, the way `install --sha256` wants it.
    Hash {
        /// File to digest.
        file: PathBuf,
    },
    /// List this workspace's installed models and what their artifacts look like on disk.
    List {
        /// Workspace root to inspect: its model rows, what their artifacts look like on disk, and any fetch in
        /// progress.
        workspace: PathBuf,
        /// Print the listing as JSON instead of one line per model.
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Scan { workspace, json } => scan_workspace(&workspace, json),
        Command::Embed { workspace, json } => embed_workspace(&workspace, json),
        Command::Search {
            workspace,
            query,
            limit,
            json,
        } => search_workspace(&workspace, &query, limit, json),
        Command::Models { command } => match command {
            ModelsCommand::Install {
                workspace,
                id,
                path,
                dim,
                sha256,
                prefix_doc,
                prefix_query,
                json,
            } => {
                let request = Install {
                    id,
                    source: path,
                    dim,
                    sha256,
                    prefix_doc,
                    prefix_query,
                };
                models_install(&workspace, &request, json)
            }
            #[cfg(feature = "fetch")]
            ModelsCommand::Fetch {
                workspace,
                id,
                members,
                discard_marker,
                json,
            } => models_fetch(&workspace, &id, &members, discard_marker, json),
            ModelsCommand::Hash { file } => models_hash(&file),
            ModelsCommand::List { workspace, json } => models_list(&workspace, json),
        },
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // Written through a handle with the result dropped, for the reason `np_core::scan` gives for
            // its own stderr receipt: `eprintln!` panics when descriptor 2 refuses the write, which would
            // replace a reportable failure with exit 101 and a panic message that cannot be printed
            // either. The exit code is the part the caller can still observe, so it stays FAILURE.
            //
            // `ModelError` travels this same channel unedited on purpose: its own text already names the
            // paths and both digests (`DigestMismatch`, `ArtifactDisagrees`, `AlreadyInstalled`), so anything
            // this command wrapped it in would be a shorter version of a message that was already complete.
            let _ = writeln!(io::stderr(), "np: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Index `workspace` and print its report.
///
/// Every failure leaves as one message on stderr and exit code 1 — the message is best effort, because a
/// descriptor that refuses writes must not cost the caller the exit code (see the call site in `main`). A
/// configuration that names no data dir is reported as the missing value it is by [`load_config`], because
/// [`Config::load`] refuses rather than deriving `workspaces/<hash>.sqlite` next to the current directory.
fn scan_workspace(workspace: &Path, json: bool) -> Result<(), Box<dyn Error>> {
    let cfg = load_config()?;
    let report = scan(workspace, &cfg)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_summary(&report);
    }
    Ok(())
}

/// The human-readable form of a report: one aligned line per counted field, then the details.
///
/// Every line goes to one locked stdout handle with its result dropped, so a descriptor that refuses writes
/// (a closed pipe, `/dev/full`) cannot turn a scan whose work is committed into the panic `println!` would
/// raise — exit 101 over a summary nobody can read. The bytes on stdout are exactly what they were.
fn print_summary(report: &ScanReport) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "added      {}", report.added);
    let _ = writeln!(out, "modified   {}", report.modified);
    let _ = writeln!(out, "moved      {}", report.moved);
    let _ = writeln!(out, "removed    {}", report.removed);
    let _ = writeln!(out, "unchanged  {}", report.unchanged);
    // The upgrade's orphan receipt, echoed once by the run whose own `Db::open` did the deleting
    // (`ScanReport::upgrade_orphans` says why it is once and not always). Dropped when stdout refuses
    // writes (descriptor 1) like every other line here — with deliberate reason this time: the deletions are
    // already recorded in `meta` under `np_core::db::FK_ORPHANS_KEY`, so this line is the copy a person
    // gets to read, not the only place the fact lives.
    //
    // So the same sentence can appear **twice in one run** — once on stderr from the migration
    // (`np_core::db::report_orphans`) and once here. That duplication is the design, not a bug to file:
    // the stderr line belongs to the command that did the deleting and prints even when no report is
    // printed, this one belongs to the report a person is reading, and neither has to arrive for the fact
    // to survive.
    if let Some(receipt) = &report.upgrade_orphans {
        let _ = writeln!(
            out,
            "upgrade    removed {} orphan row(s) while adding note foreign keys ({})",
            orphan_total(receipt),
            receipt
        );
    }
    // What this run's own `Db::open` merged — F19's duplicate-note repair. One value, printed verbatim as
    // `meta` holds it under `np_core::db::NOTE_MERGE_KEY`, so this line and the durable record cannot drift
    // and no second count is computed here.
    //
    // Once per database rather than on every scan (`ScanReport::merged_notes` gives the reason), and dropped
    // when stdout refuses writes like every other line here: the merge itself is committed, and its receipt
    // is in `meta`. The same sentence can also appear on stderr from the migration
    // (`np_core::db::report_merges`), which is the deliberate duplication `report_orphans` documents.
    if let Some(receipt) = &report.merged_notes {
        let _ = writeln!(out, "merge      {receipt}");
    }
    let _ = writeln!(out, "notes      {}", report.notes);
    let _ = write!(out, "unresolved {}", report.unresolved_links);
    if report.unresolved_links > 0 {
        // An ambiguous target is left unresolved instead of guessed, so the number alone is not
        // actionable. Spelling the path usually is (`np_core::links::resolve` matches an exact rel first) —
        // except for a pair differing only in case, whose spellings fold identically and so place neither.
        let _ = write!(out, " — a target matched more than one file or none; spell its path to place it, e.g. [[notes/a.md]]");
    }
    let _ = writeln!(out);
    // The targets behind that count, spelled as their author wrote them. The scan reads both from one
    // transaction, so this list can never answer a different question than the number above it.
    for target in &report.unresolved_targets {
        let _ = writeln!(out, "  {target}");
    }
    let _ = writeln!(out, "skipped    {}", report.skipped.len());
    for path in &report.skipped {
        let _ = writeln!(out, "  {path}");
    }
    // What that list actually **was**, counted by kind instead of merged (F22 §3): `skipped 2` hides that one
    // path is a permission problem nobody fixes by renaming and the other a name this build can never key.
    // Printed only when something was lost, and printed as the same string `meta` records under
    // `np_core::scan::UNINDEXABLE_KEY` — one value, two channels, so the line a person read yesterday can be
    // compared with the record they open today.
    //
    // One column wider than the labels above: `unindexable` is eleven letters, those are ten, and re-spacing
    // them to fit a new word would break the `grep` of anyone reading their own old logs. Dropped when stdout
    // refuses writes (descriptor 1) like every other line here — the durable copy is in `meta`, so this line
    // is the copy a person reads rather than the only place the fact lives.
    if let Some(losses) = &report.unindexable {
        let _ = writeln!(out, "unindexable {}", losses.value());
    }
    let _ = writeln!(out, "index      {}", report.index_files.join(", "));
    let _ = writeln!(out, "elapsed    {} ms", report.elapsed_ms);
}

// ---------------------------------------------------------------------------
// `np embed` — design-m2-local-embeddings.md §6.
//
// The command resolves *which* provider the configuration names and prints what came back; every decision
// about currency, batching, exit arms and the stamp rule lives in [`np_core::embed_run`]. That split is why
// there is no SQL here beyond `db.rs`'s own readers and no second digest.
// ---------------------------------------------------------------------------

/// Embed `workspace` and print its report.
///
/// Three of the four arms leave as exit 1 with the arm's sentence on stderr *and* in the printed report: two
/// channels for one fact, the posture `np models list` already takes for a directory-level failure. No new exit
/// code is introduced — §6 says so, and clap keeps argv failures at 2.
fn embed_workspace(workspace: &Path, json: bool) -> Result<(), Box<dyn Error>> {
    let cfg = load_config()?;
    // Destructured rather than reached through the struct: the borrow below has to outlive the statement that takes it,
    // and `Resolution::Provider` needs `&mut` into the box that `state` owns.
    let Resolved {
        mut state,
        artifact_check,
        workers,
    } = embed_provider(&cfg, workspace)?;
    // The reason a provider could not be built travels as a value — [`ProviderState::ArtifactAbsent`] for a missing file,
    // [`ProviderState::Placeholder`]'s reason for the configuration states. Handing over a bare placeholder is how R6c1 D-2
    // happened: an absent artifact became a provider that declares 0 dimensions, and the run refused with a sentence about
    // widths when the truth was a missing file (see [`Resolution`]).
    //
    // Both live providers and placeholders map to `Resolution::Provider`, which is exactly what this call site produced
    // before F32 — the two arms embed can see are unchanged by construction, and that is the reason `ProviderState` was
    // allowed to exist at all (F32 §5).
    //
    // `as_mut`, not `as_ref`: a provider is the thing that can open a projector, so the run holds it mutably for as long
    // as the resolution lives (`Resolution::Provider`'s note says why).
    let resolution = match &mut state {
        ProviderState::Ready(embedder) | ProviderState::Placeholder { embedder, .. } => {
            Resolution::Provider(embedder.as_mut())
        }
        ProviderState::ArtifactAbsent => Resolution::ArtifactAbsent,
    };
    let mut report = embed_run::embed_with_resolution(workspace, &cfg, resolution)?;
    // What was concluded about the model's bytes travels on the report rather than being printed here, so `--json`
    // and the text summary cannot disagree about whether this run verified anything (the same reason §7's caveat is a
    // field). `None` says no local artifact was involved — a `provider = "none"` run has nothing to claim.
    report.artifact_check = artifact_check;
    // Likewise the worker count: decided where the provider was built, printed here. `None` means **no local-model
    // provider was constructed** — which is not the same sentence as "nothing was embedded": a `local-model` run whose
    // notes were all current still built a provider, and its report states the count it would have used.
    report.workers = workers;
    if json {
        emit_json(&serde_json::to_value(&report)?);
    } else {
        print_embed_summary(&report);
    }
    if report.succeeded() {
        return Ok(());
    }
    // `detail()` is `None` only for arm (i), which returned above; the empty fallback keeps this from being a
    // second place that has to know which arms carry a sentence.
    Err(format!(
        "{}: {}",
        report.outcome.label(),
        report.outcome.detail().unwrap_or_default()
    )
    .into())
}

/// Rank `query` against `workspace`'s index and print the hits.
///
/// The provider is built exactly as `np embed` builds it, because the query must be embedded by the **same** model whose
/// rows are being read — and under its `prefix_query` spelling, not the document one ([`Embedder::embed_query`]). That is also
/// where a run that cannot embed refuses at exit 1, with the sentence for its own [`ProviderState`]: an absent artifact, an
/// unbuilt runtime, a missing row, a switched-off provider. A query nobody can embed has no ranking to print (F32: until this
/// round four of those five shared one sentence about `provider = "none"`).
fn search_workspace(
    workspace: &Path,
    query: &str,
    limit: usize,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let cfg = load_config()?;
    let db = workspace_db(&cfg, workspace)?;
    // One `match`, no `_` arm: adding a state to `ProviderState` has to fail to compile *here*, because the last time this
    // call site knew only one of the five answers it printed `embeddings.provider = "none"` about a configuration that said
    // `local-model` (F32). Every placeholder reason collapses into a single arm on purpose — §1 of the F32 design measures
    // that no CLI invocation can reach two of them, so per-state arms here would be mapping sites nothing can test.
    let provider = match embed_provider(&cfg, workspace)?.state {
        ProviderState::Ready(provider) => provider,
        ProviderState::Placeholder { reason, .. } => {
            return Err(reason.sentence(workspace).into())
        }
        // One line, spelled out here rather than assembled across a wrapped literal: the first version broke mid-sentence
        // and printed a run of twenty spaces between "be" and "embedded". The pointer is `install_hint`, the same
        // feature-gated wording embed's arm (iii) prints — a build that compiled `fetch` out must not be sent to it.
        ProviderState::ArtifactAbsent => {
            return Err(format!(
                "no embedding provider for this workspace: its model artifact is absent, so the query cannot be embedded: {}",
                embed_run::install_hint(workspace)
            )
            .into())
        }
    };
    // The id the index is keyed by, taken from the provider that made those rows rather than re-read from config: a
    // `[embeddings] model` that disagrees with the installed row would otherwise search one model and report another.
    let model = provider.model_id().to_string();
    let vector = provider.embed_query(query)?;
    let hits = search::search(&db, &model, &vector.to_bytes(), limit)?;

    let mut out = io::stdout().lock();
    if json {
        // One object per hit under `hits`, and nothing else on stdout (§12). The empty case is an empty array — a machine
        // reader does not need a sentence, and a second key would be a schema change in disguise.
        let objects: Vec<Value> = hits.iter().map(hit_json).collect();
        drop(out);
        emit_json(&json!({ "hits": objects }));
        return Ok(());
    }
    if hits.is_empty() {
        // §12: an empty index is a fact. Written to stdout, exit 0 — the caller has nothing to fix.
        // `--limit 0` asks for no hits and gets no lines, which is not the same sentence, so it is not printed.
        if limit > 0 {
            let _ = writeln!(out, "nothing indexed for {model}");
        }
        return Ok(());
    }
    for hit in &hits {
        // Score to four decimals (full precision travels in `--json`), kind padded so the paths start in one column.
        let score = format!("{:.4}", hit.score);
        match &hit.source_note {
            Some(note) => {
                let _ = writeln!(
                    out,
                    "{score} {:<5}  {}   (from {note})",
                    hit.kind.label(),
                    hit.path
                );
            }
            None => {
                let _ = writeln!(out, "{score} {:<5}  {}", hit.kind.label(), hit.path);
            }
        }
    }
    Ok(())
}

/// One hit as JSON: **the same five keys whatever the kind** — `{score, kind, path, linked_from, source_note}`.
///
/// The first version added `linked_from` and `source_note` only for image hits, which made the schema depend on a value
/// inside the object — a reader then has to branch on `kind` before it knows whether a key is there, and a note hit whose
/// linker field is simply absent reads differently from one that has none. Absent and zero are different statements, so this
/// emits both keys always: `source_note: null` and `linked_from: 0` for a hit that has no linking note (every note hit, and
/// an orphaned figure). Same rule `EmbedReport::artifact_check` already follows — present and null, not missing.
fn hit_json(hit: &Hit) -> Value {
    json!({
        "score": hit.score,
        "kind": hit.kind.label(),
        "path": hit.path,
        "linked_from": hit.linked_from,
        "source_note": match &hit.source_note {
            Some(note) => json!(note),
            None => Value::Null,
        },
    })
}

/// What `[embeddings]` resolved to, and — on the one path that could — what it proved about the artifact's bytes.
///
/// The second field exists because the check happens *here* (this is where the `models` row and its artifact path are
/// both at hand) while the report is written downstream. Carrying it in one value keeps `embed_provider` honest
/// without an out-parameter, and keeps the alternative — verifying inside `embed_run`, which is handed a provider and
/// cannot know how its bytes were produced — off the table.
struct Resolved {
    /// What `[embeddings]` resolved to, including the states where nothing can embed. F32 replaced the
    /// `Option<Box<dyn Embedder>>` this field used to be: an `Option` could say "no provider" without saying which of five
    /// reasons, and only one of those reasons was ever explained.
    state: ProviderState,
    /// D12's verify-on-change result, or `None` when no local artifact was read at all.
    artifact_check: Option<ArtifactCheck>,
    /// The worker-thread plan for this run, or `None` when no local-model provider was built (§6's arms and
    /// `provider = "none"` have no worker count to report).
    workers: Option<np_core::embed_run::WorkerPlan>,
}

/// What `[embeddings]` and the `models` table added up to, as one value.
///
/// Three states because that is how many answers exist: a provider this run can embed with, a provider-shaped placeholder
/// for a state that refuses before a vector is asked for, and a filesystem fact no downstream code can re-derive.
enum ProviderState {
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
    }
}

/// A resolution for the row-installed-file-absent state: no provider at all, by the reason in [`ProviderState`].
fn artifact_absent() -> Resolved {
    Resolved {
        state: ProviderState::ArtifactAbsent,
        artifact_check: None,
        workers: None,
    }
}

/// What `[embeddings]` names, as one of three [`ProviderState`]s.
///
/// [`ProviderState::ArtifactAbsent`] means **exactly that one thing** — the row says installed, the file is not there — and
/// `np embed` turns it into [`Resolution::ArtifactAbsent`] so §6's arm (iii) can say so. The four states that are *not* about
/// bytes (`provider = "none"`, a missing [`np_core::config::EmbeddingsConfig::model`], an id naming no installed row, a row whose
/// declared width cannot be stored) hand over [`NoEmbeddingProvider`] as a **placeholder** — what [`np_core::embed_run`] wants,
/// since it derives those arms from configuration and the `models` table, the sources that hold the fact — **and** a
/// [`NoProvider`] reason, which is what `np search` prints instead of the placeholder's generic refusal (F32).
///
/// Two of those four reasons are unreachable from argv: `Config::load` refuses `local-model` with no `model`, and
/// `np models install --dim` validates 1..=65535 at the command line. They stay because a `Config` can be built in memory, and
/// because the alternative is a state with no sentence — F32 §1 records both measurements.
///
/// The row is read through a connection that closes when this function returns, before `embed_run` opens its
/// own: np-cli holds no SQL of its own, and one workspace's [`np_core::db::ModelRow`] is where a local provider's
/// identity comes from (§3).
fn embed_provider(cfg: &Config, workspace: &Path) -> Result<Resolved, Box<dyn Error>> {
    match cfg.embeddings.provider {
        // The default. Nothing is built, nothing is contacted, and the report says *configuration disabled*
        // rather than a fault (§2, §6).
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
            let Some(row) = model_row(&db, id)? else {
                // F32's arm: this is the state that used to print a sentence about `provider = "none"`.
                return Ok(placeholder(NoProvider::RowMissing { id: id.to_string() }));
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
                // Also defensive: `--dim` bounds what can be installed (F32 §1).
                return Ok(placeholder(NoProvider::RowDimUnusable {
                    id: id.to_string(),
                    declared: row.dim.to_string(),
                }));
            };
            let artifact = cfg.workspace_models_dir(workspace).join(id);

            // With the runtime compiled in, the provider is the one that loads llama.cpp **on first use**
            // (`local_model::LazyLocalEmbedder`, §14): resolving a provider happens before anyone knows whether
            // the workspace owes a chunk, and an eager load would spend ~420 MiB of resident weights plus load time
            // on a run whose answer is "nothing changed". The two refusals that need no model — a row width that
            // is not this run's, and an artifact that is not a file — are still taken here, so §6's *artifact
            // absent* arm stays arm (iii) instead of arriving late as a failure.
            #[cfg(feature = "local-model")]
            {
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
                        // Read before the embedder is boxed into a trait object: `worker_plan` is a method on the
                        // concrete type, and the plan belongs to the provider that was just built.
                        let plan = embedder.worker_plan();
                        match np_core::local_model::verify_artifact(&db, &row, &artifact) {
                            Ok(check) => Ok(Resolved {
                                state: ProviderState::Ready(Box::new(embedder)),
                                artifact_check: Some(check),
                                workers: Some(plan),
                            }),
                            Err(err) => Err(err.into()),
                        }
                    }
                    Err(np_core::embed::EmbedError::ArtifactUnreadable { .. }) => {
                        Ok(artifact_absent())
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
                // about its bytes to make — and printing one would be the false report §7 refuses.
                Ok(provider) => Ok(Resolved {
                    state: ProviderState::Ready(Box::new(provider)),
                    artifact_check: None,
                    workers: None,
                }),
                // The row is installed and its width is the run's; only the bytes are missing. That is a fact
                // about the filesystem, not about configuration, so it cannot be recovered downstream — hence
                // `None` here rather than a placeholder provider whose declared width would then be compared
                // with the row's (R6c1 D-2).
                Err(np_core::embed::EmbedError::ArtifactUnreadable { .. }) => Ok(artifact_absent()),
                Err(err) => Err(err.into()),
            }
        }
    }
}

/// The human-readable form of an embed report: the arm first, then §6's six counts, then §7's caveat.
///
/// Same aligned-label shape as [`print_summary`], and every line written to one locked stdout handle with its
/// result dropped for the same reason: a descriptor that refuses writes must not turn a finished run into a
/// panic. The verification line is printed even when nothing was embedded, because §7's whole point is that the
/// caveat is read at 2 a.m. rather than filed.
fn print_embed_summary(report: &EmbedReport) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "model      {}", report.model.as_deref().unwrap_or("-"));
    let _ = writeln!(out, "outcome    {}", report.outcome.label());
    if let Some(detail) = report.outcome.detail() {
        let _ = writeln!(out, "detail     {detail}");
    }
    // The path arm (iii) looked at, printed apart from the hint so a reader can `ls` it. It is a data-dir file
    // this program wrote, which is why naming it is safe (§3's payload rule).
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
    // text-only report keeps the shape it had before images existed (§6's order puts this after the chunk counts).
    if report.images_seen > 0 {
        // §6's line, with the classes spelled out: a count of three refusals tells a reader nothing about whether their
        // vault is full of `.svg` (a format decision) or missing files (their own edit), and those two answers lead to
        // different commands. Classes come from `image_refusal_classes`, in §8's order, so this line and `--json` cannot
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
    // Printed after §7's standing caveat because the two answer different questions: that one asks whether these
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
// `np models` — M2a round 4a (`docs/design-m2-models-install.md` §2, §3).
//
// Three commands, and the whole of their intelligence is in `np_core::models`: these functions turn argv
// into an `Install`, open one workspace's database, call the library, and print what came back. That split is
// why there is no SQL here (round 4a §6 names every read and write in `db.rs` instead) and no `sha2`
// dependency (`hash` calls `np_core::hash::sha256_file`, which §7's test 9 exists to keep the only one).
// ---------------------------------------------------------------------------

/// Hex characters shown in the human-readable digest column (§3: "12 hex, full value under `--json`").
const DIGEST_TEXT_CHARS: usize = 12;

/// Parse `--dim` as §2 states it: a width from 1 to `u16::MAX`, refused while parsing rather than at the write.
///
/// `u16` because [`np_core::config::EmbeddingsConfig::dim`] already measures that quantity in `u16`; non-zero
/// because migration 7's `CHECK (length(vector) = 4 * dim)` admits a zero-byte vector when `dim` is 0, so a
/// dim-0 row would accept an embedding of nothing for every chunk. Both messages name the ceiling, which is
/// what a human who typed `153600` needs to read — and refusing here rather than in the schema is what keeps
/// §6's "no new migration" true.
fn parse_dim(raw: &str) -> Result<NonZeroU16, String> {
    let ceiling = u16::MAX;
    let value: u32 = raw
        .parse()
        .map_err(|_| format!("`{raw}` is not a number; --dim takes a width from 1 to {ceiling}"))?;
    let width = u16::try_from(value)
        .map_err(|_| format!("--dim {value} is above the ceiling of {ceiling}"))?;
    // Zero gets its own reason rather than an arithmetic complaint. `NonZeroU16` is how the refusal reaches
    // [`np_core::models::Install`]: the bound travels as a type, so no caller can skip it.
    NonZeroU16::new(width).ok_or_else(|| {
        format!("--dim cannot be 0: a zero-width model has no vector to compare (the range is 1..={ceiling})")
    })
}

/// The configuration every command needs, reported as the missing value it is: [`Config::load`] refuses rather
/// than deriving `workspaces/<hash>.sqlite` next to the current directory, and a model command that guessed a
/// data dir would install artifacts nobody can find.
fn load_config() -> Result<Config, Box<dyn Error>> {
    Config::load(None).map_err(|err| format!("configuration error: {err}").into())
}

/// This workspace's database, opened in the order §3 step 0 requires.
///
/// [`models::require_workspace_dir`] comes **first** because `Config::workspace_db` is infallible by contract
/// and `Db::open` creates directories: check after opening would have already minted an empty database for a
/// typo'd name, which the check can no longer un-mint.
fn workspace_db(config: &Config, workspace: &Path) -> Result<Db, Box<dyn Error>> {
    models::require_workspace_dir(workspace)?;
    Ok(Db::open(&config.workspace_db(workspace))?)
}

/// `np models install` — verify the human's file against their declaration, then record it.
///
/// Nothing is printed before [`models::install`] returns: §3 step 7 and F21 both say the durable record comes
/// first and the echo second, so a refusal cannot leave a success line behind on stdout.
/// Fetch `id`'s pinned members into `workspace` and record the model.
///
/// **The origin is read, never printed.** §12 rule 8 bounds what a report may say about where bytes came from, and a
/// success line lands in CI logs as readily as a refusal does — so the report says an origin was configured and names
/// the members, sizes and digests, which is everything a person can act on. The one thing that must not appear is the
/// URL, because a mirror's hostname is someone's infrastructure detail and this program has no business publishing it
/// in a build log. Since 2026-11-04 an origin exists on every run — `[models] base_url` and `NP_MODELS_BASE_URL` both
/// silent means [`np_core::pins::DEFAULT_ORIGIN`] — which makes *not* printing it the more important half of the rule.
#[cfg(feature = "fetch")]
fn models_fetch(
    workspace: &Path,
    id: &str,
    members: &[String],
    discard_marker: bool,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let config = load_config()?;
    let db = workspace_db(&config, workspace)?;
    let models_dir = config.workspace_models_dir(workspace);

    // **The discard happens first.** `--discard-marker` is a filesystem action a human takes because they know no fetch
    // is running (§12 rule 4's only door), so it may not depend on anything the rest of the run needs — not which
    // origin resolves, not whether this build can use the members it would plan. Dialing out is the part that can fail
    // for reasons outside the workspace, and a case where it does (`crates/np-cli/tests/models_cli.rs`) still expects
    // the discard to have happened.
    if discard_marker {
        let discarded = fetch::discard_marker(&models_dir, id)?;
        if !json {
            emit(&if discarded.marker_removed {
                format!(
                    "discarded   marker for {id} ({} partial file(s) removed)",
                    discarded.parts_removed.len()
                )
            } else {
                format!("no marker   for {id} — nothing to discard")
            });
        }
    }

    // The origin, in the one order §12 rule 7 sets out as amended on 2026-11-04: `[models] base_url` as config left it
    // (`NP_MODELS_BASE_URL` fills that key only when the file leaves it unset — `np_core::config`'s rule for every key
    // in that section), then the origin baked into the pin table. A machine with no configuration therefore makes one
    // outbound request during installation instead of being refused; a mirror is still one config line away, and this
    // line stays true of every run: the resolved origin is read here and printed nowhere (§12 rule 8).
    let origin = fetch::resolve_origin(config.models.base_url.as_deref());

    // **Say which origin this run is about to dial, in class terms, before any byte moves.** Rule 8 still forbids the host, so
    // the line names where the answer came from rather than what it is. It exists because `NP_MODELS_BASE_URL=` (set, empty)
    // counts as unset — a rule `tests/config.rs` pins on purpose, mirroring how an empty `$XDG_*` is treated — and since §12
    // rule 7's 2026-11-04 amendment, "unset" no longer means "refuse": it means the compiled-in origin. A mirror whose value
    // was lost to a shell (`NP_MODELS_BASE_URL=$MIRROR` with `MIRROR` unset) would otherwise spend the operator's egress
    // budget in silence.
    if !json {
        emit(if config.models.base_url.is_none() {
            "origin      this build's compiled-in default — no [models] base_url is configured (an empty \
             NP_MODELS_BASE_URL counts as unset)"
        } else {
            "origin      from [models] base_url"
        });
    }

    // **Fetch what this binary can use** (`docs/design-m2-image-embeddings.md` §13): with no `--member`, the plan is
    // the members this build could load, so a text-only install never transfers 554 MB of projector it has no code for.
    let requested_owned: Vec<String> = if members.is_empty() {
        np_core::pins::members_for_build()
            .iter()
            .map(|name| (*name).to_string())
            .collect()
    } else {
        members.to_vec()
    };

    // A member this build cannot use is refused by name, naming the **capability** that is missing. The engine's own
    // refusal would be a false sentence here — `mmproj` *is* pinned, so "no recorded digest" describes a different
    // problem — and the reason to refuse is this binary rather than the pin table or the origin. Today exactly one
    // member can reach this arm (`np_core::pins::MMPROJ_MEMBER`, absent from `members_for_build` without `vision`);
    // a second capability-gated member would need that function to say which capability each member needs before this
    // wording could stay true, so it is written for the one case that exists.
    let usable = np_core::pins::members_for_build();
    if let Some(name) = requested_owned.iter().find(|name| {
        !usable.contains(&name.as_str()) && np_core::pins::pinned_member(id, name).is_some()
    }) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "cannot fetch member `{name}` of model `{id}`: this binary was built without image support, so the \
                 projector's bytes would never be read here; this build fetches {} by default, and an \
                 image-capable build (np-core's `vision` feature) can fetch `{name}`",
                usable.join(", "),
            ),
        )
        .into());
    }

    let requested: Vec<&str> = requested_owned.iter().map(String::as_str).collect();

    let transport = fetch::HttpTransport::new(origin);
    let report = fetch::fetch(&db, &models_dir, id, &requested, &transport, now_secs())?;

    if json {
        emit_json(&json!({
            "id": report.id,
            // Present-and-null when the platform could not say, so a reader never has to guess whether an absent key
            // meant "no room problem" or "no statvfs" (the `models_dir_error` posture).
            "free_bytes": report.free_bytes,
            "total_bytes": report.total_bytes,
            "members": report.members.iter().map(|member| json!({
                "member": member.member,
                "path": member.path.to_string_lossy(),
                "bytes": member.bytes,
                "sha256": member.sha256,
            })).collect::<Vec<_>>(),
        }));
    } else {
        for member in &report.members {
            emit(&format!(
                // `size_human` spells the unit itself, so the word must not be repeated here: "295.5 MB bytes" is the
                // kind of line that makes a reader distrust the number next to it.
                "{FETCHED_KEY}{} ({}, sha256 {})",
                member.member,
                size_human(member.bytes),
                member.sha256
            ));
        }
        emit(&free_space_line(report.free_bytes));
    }
    Ok(())
}

/// The two padded keys of `np models fetch`'s text report, spelled out so their shared column is a named thing a test can pin:
/// widening one alone used to misalign the block with every gate green, which is how the line below ended up one space wider.
#[cfg(feature = "fetch")]
const FETCHED_KEY: &str = "fetched     ";
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
fn free_space_line(free: Option<u64>) -> String {
    match free {
        Some(free) => format!("{FREE_SPACE_KEY}{} before the fetch", size_human(free)),
        None => format!("{FREE_SPACE_KEY}not checked on this platform"),
    }
}

fn models_install(workspace: &Path, request: &Install, json: bool) -> Result<(), Box<dyn Error>> {
    let config = load_config()?;
    let db = workspace_db(&config, workspace)?;
    let outcome = models::install(&db, &config, workspace, request, now_secs())?;
    if json {
        // `state` is the key §3 pins and its three strings come from
        // [`np_core::models::InstallOutcome::state_label`], not from a format string here. `id` rides along
        // because a JSON reader of a redirected run has no argv to look at, and the same two keys are present
        // for all three outcomes — one shape per command, as §3 requires.
        emit_json(&json!({ "id": request.id, "state": outcome.state_label() }));
    } else {
        emit(outcome.state_label());
    }
    Ok(())
}

/// `np models hash <file>` — `<sha256>  <path>`, the two-space spelling `sha256sum` writes, so the first field
/// pastes straight into `install --sha256`.
///
/// No database and no digest of its own: [`sha256_file`] streams the file and returns lowercase hex, which is
/// what makes §7's test 9 (this command's output equals that function's, same bytes) a tautology rather than a
/// coincidence.
fn models_hash(file: &Path) -> Result<(), Box<dyn Error>> {
    // `models::access` is the spelling the OS wants (absolute, no symlink resolution); on Windows a long relative
    // argument would otherwise reach `walk::to_open_path` still relative and fail with a raw prefix error.
    let digest = sha256_file(&models::access(file))?;
    emit(&format!("{digest}  {}", file.display()));
    Ok(())
}

/// `np models list <workspace>` — every row and what its artifact looks like on disk.
///
/// The directory-level failure is handled apart from the rest because §3 refuses to let it become a per-row
/// verdict: [`models::list`] answers it with no [`Listing`] at all, so there are no rows to print and no
/// `missing` to claim against any of them. In text mode stdout stays empty; under `--json` the pinned key set
/// still goes out with `"models_dir_error"` holding the message, since that key exists precisely to be the
/// machine-readable form of this state. The same sentence then travels the error channel and the exit code is
/// non-zero — two channels, one fact, exactly as the scan receipts do it.
fn models_list(workspace: &Path, json: bool) -> Result<(), Box<dyn Error>> {
    let config = load_config()?;
    let db = workspace_db(&config, workspace)?;
    match models::list(&db, &config, workspace) {
        Ok(listing) => {
            if json {
                emit_json(&listing_json(
                    &listing.models,
                    &listing.fetching,
                    &listing.unclaimed,
                    None,
                ));
            } else {
                print_listing(&listing);
            }
            Ok(())
        }
        Err(err @ ModelError::ModelsDir { .. }) => {
            if json {
                emit_json(&listing_json(&[], &[], &[], Some(&err.to_string())));
            }
            Err(err.into())
        }
        Err(err) => Err(err.into()),
    }
}

/// The `--json` body of `np models list`: one object whose key set is identical for every state (§3), built by
/// this one function so the success path and the directory-level failure cannot grow different keys.
///
/// The three top-level names and the seven per-row names are §3's, pinned by a test that compares the whole
/// set rather than looking one key up — `state` or `unclaimed` renamed must fail here, not in a reader written
/// against the old spelling.
///
/// **`fetching` is a deliberate protocol change**, added by R7a under `design-m2-local-embeddings.md` §12 rule 3:
/// a fetch is orthogonal to the four `ArtifactState` verdicts (first start has no row at all, and a present row
/// can be re-fetching), so it gets a listing-level section beside `unclaimed` rather than a fifth arm. The key
/// set is pinned precisely so that adding it had to fail a test — and did.
fn listing_json(
    models: &[ListedModel],
    fetching: &[FetchEntry],
    unclaimed: &[Unclaimed],
    models_dir_error: Option<&str>,
) -> Value {
    json!({
        "models": models.iter().map(model_json).collect::<Vec<Value>>(),
        // One entry per marker file, ordered by id by `np_core::models::list`. Empty on every state where no
        // marker was found — including the directory-level failure below, where nothing could be looked at.
        "fetching": fetching.iter().map(fetch_json).collect::<Vec<Value>>(),
        "unclaimed": unclaimed
            .iter()
            .map(|entry| json!({ "name": entry.name, "kind": unclaimed_kind(entry.kind) }))
            .collect::<Vec<Value>>(),
        // Present and null whenever the directory could be listed: suppressing the key on the good path would
        // cost every reader a missing-key branch to answer a question `null` already answers (the precedent is
        // `scan --json`'s `upgrade_orphans`).
        "models_dir_error": models_dir_error,
    })
}

/// One row under `--json`, with exactly the seven keys §3 pins — and the full digest, which the text column
/// shortens.
fn model_json(model: &ListedModel) -> Value {
    json!({
        "id": model.row.id,
        "dim": model.row.dim,
        "sha256": model.row.sha256,
        "prefix_doc": model.row.prefix_doc,
        "prefix_query": model.row.prefix_query,
        "installed_at": model.row.installed_at,
        "state": model.state.state_label(),
    })
}

/// One fetch under `--json`, in **one shape for every state** — the `models_dir_error` precedent, applied one
/// level down.
///
/// The four marker fields are present-and-null when the file would not parse, and `marker_error` carries why.
/// Suppressing the field keys on that arm would cost every reader a missing-key branch to answer a question null
/// already answers, and a reader branching on "is `nonce` there?" is a reader that guesses at why it is not.
///
/// `path` is here because §12 rule 4's advice — "if no fetch is running, delete this file and re-run" — cannot be
/// followed from an id alone: the artifact directory's own name is a hash of the workspace path.
fn fetch_json(entry: &FetchEntry) -> Value {
    let (fields, marker_error) = match &entry.state {
        FetchState::Present(fields) => (Some(fields), None),
        FetchState::Unreadable { reason, .. } => (None, Some(reason.as_str())),
        // `list` drops these entries (`np_core::models::FetchEntry::state` says why), so the null row below is
        // unreachable from this command rather than a state a reader can see.
        FetchState::Absent => (None, None),
    };
    json!({
        "id": entry.id,
        "path": entry.path.to_string_lossy(),
        "nonce": fields.map(|fields| fields.nonce.as_str()),
        "started_at": fields.map(|fields| fields.started_at),
        "bytes_done": fields.map(|fields| fields.bytes_done),
        // Null means "this fetch never announced a size" (§12 rule 12) — never zero, which would be a
        // denominator this program invented.
        "bytes_total": fields.map(|fields| fields.bytes_total),
        "marker_error": marker_error,
    })
}

/// The two words §3 needs from an unclaimed entry: a subdirectory has to be **reported as one**, which is why
/// an entry is an object rather than a bare name in JSON and carries the word in text.
fn unclaimed_kind(kind: UnclaimedKind) -> &'static str {
    match kind {
        UnclaimedKind::File => "file",
        UnclaimedKind::Directory => "directory",
    }
}

/// The human-readable listing: a count line and one line per model, any fetch-in-progress lines, then the
/// unclaimed section.
///
/// The shape follows `print_summary`'s — aligned label columns, then indented detail lines under a count — so
/// the two commands read alike, and the counts arrive even when there is nothing to list ("looked and found
/// none" must not read as "printed nothing"). Every line goes to one locked stdout handle with its result
/// dropped, for `print_summary`'s reason: a descriptor that refuses writes must not panic over output that is
/// only the echo of a listing already computed.
fn print_listing(listing: &Listing) {
    let mut out = io::stdout().lock();
    // Ids vary in length, so the column is as wide as the widest one rather than a constant that would break on
    // a 64-character id (§2 allows one).
    let width = listing
        .models
        .iter()
        .map(|model| model.row.id.chars().count())
        .max()
        .unwrap_or(0);
    let _ = writeln!(out, "models     {}", listing.models.len());
    for model in &listing.models {
        let _ = writeln!(
            out,
            "{:<width$}  {}  dim {}  sha256 {}  installed_at {}  prefix_doc \"{}\"  prefix_query \"{}\"",
            model.row.id,
            model.state.state_label(),
            model.row.dim,
            text_digest(&model.row.sha256),
            model.row.installed_at,
            model.row.prefix_doc,
            model.row.prefix_query,
        );
    }
    // One line per fetch in progress, and **no section header**: `models` and `unclaimed` print their counts even
    // when empty because "looked and found none" must not read as "printed nothing", while a fetch line is an
    // event — R7a's brief asks for nothing extra when nothing is fetching, so an idle workspace prints exactly
    // what it printed before this section existed.
    let count = listing.fetching.len();
    for entry in &listing.fetching {
        let _ = writeln!(out, "{}", fetch_line(count, entry));
    }
    let _ = writeln!(out, "unclaimed  {}", listing.unclaimed.len());
    for entry in &listing.unclaimed {
        let _ = writeln!(out, "  {}  {}", entry.name, unclaimed_kind(entry.kind));
    }
}

/// One fetch's line: `{n} fetch in progress · {id} ({what the marker says})`.
///
/// The count rides on every line rather than heading a block because that is the shape R7a pins — one line per
/// fetch, readable on its own when someone greps stdout for `fetch`. With two fetches running the number repeats
/// and stays true; `tests/models_cli.rs` pins both the singular and the plural so this is a decision on the
/// record rather than an accident of the one case that was written down.
///
/// Byte sizes come from [`size_human`], the catalog's one formatter. A second formatter here would be two answers
/// to "how big is it" — the reason `CatalogEntry::size_human` is pre-formatted by its caller rather than rendered
/// twice.
///
/// **Fields are printed exactly as the marker wrote them, even when they contradict each other.** A marker saying
/// `bytes_done = 1.4 GB, bytes_total = 100 MB` prints both, because "report what you found" is the whole duty here:
/// clamping would hide a file this program cannot explain, and refusing it as unreadable would be this build
/// deciding that §12 rule 6's pair is impossible when only R7b can say whether it happens.
fn fetch_line(count: usize, entry: &FetchEntry) -> String {
    let plural = if count == 1 { "fetch" } else { "fetches" };
    let head = format!("{count} {plural} in progress · ");
    match &entry.state {
        FetchState::Present(fields) => {
            let done = size_human(fields.bytes_done);
            // No denominator invented: an unknown total says so in words, which is §12 rule 12's whole point — a
            // reader who sees `41.9 MB / 0.0 MB` would divide by it.
            let progress = match fields.bytes_total {
                Some(total) => format!("{done} / {}", size_human(total)),
                None => format!("{done}, size unknown"),
            };
            format!(
                "{head}{} (started {}, {progress})",
                entry.id, fields.started_at
            )
        }
        // The path belongs in this line and only this line: it is the file the human has to find, whereas a
        // readable marker needs nothing done to it.
        FetchState::Unreadable { reason, .. } => format!(
            "{head}{} (unreadable marker {}: {reason})",
            entry.id,
            entry.path.display()
        ),
        // Unreachable through `list`, which drops a vanished marker rather than reporting it; the fields are
        // printed as-is so that a future caller reaching here sees a line, not a panic.
        FetchState::Absent => format!("{head}{} (marker gone)", entry.id),
    }
}

/// The digest column: twelve hex characters, or `NULL` spelled as the schema spells it. An empty value would
/// read as a truncated digest; `NULL` is the row's own statement that no artifact is expected (§3).
fn text_digest(sha256: &Option<String>) -> String {
    match sha256 {
        None => "NULL".to_string(),
        Some(hex) => hex.chars().take(DIGEST_TEXT_CHARS).collect(),
    }
}

/// One line on stdout, with the result dropped for `print_summary`'s reason: a descriptor that refuses writes
/// (a closed pipe, `/dev/full`) must not turn finished work into the panic `println!` would raise.
fn emit(line: &str) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{line}");
}

/// One JSON document on stdout, pretty-printed like `np scan --json`, with the same dropped-write rule as
/// [`emit`].
fn emit_json(value: &Value) {
    let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    let mut out = io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

/// Whole seconds since the Unix epoch for `models.installed_at`, mirroring `np_core::scan`'s private
/// `now_secs`: a clock that reads before the epoch contributes `0`, which is that function's answer to the
/// same failure and keeps a working install from failing because the machine's clock is broken.
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

#[cfg(all(test, feature = "fetch"))]
mod tests {
    use super::*;

    /// The two arms of the fetch report's free-space line, and the column it shares with the member lines. Pinned on **both**
    /// sides of that column: pinning only this line is what let it drift a space wider than its neighbour in the first place.
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

    /// `None` is a stated absence, never a pass: the sentence has to say it was not checked.
    #[test]
    fn an_unanswered_preflight_is_reported_as_unanswered() {
        assert!(free_space_line(None).contains("not checked"));
        assert!(!free_space_line(None).contains("ok"));
    }
}
