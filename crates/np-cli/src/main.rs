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
use np_core::embed::{Embedder, LocalModelProvider, NoEmbeddingProvider};
use np_core::embed_run::{self, EmbedOutcome, EmbedReport};
use np_core::hash::sha256_file;
use np_core::models::{
    self, FetchEntry, FetchState, Install, ListedModel, Listing, ModelError, Unclaimed,
    UnclaimedKind,
};
use np_core::scan::{scan, ScanReport};
use serde_json::{json, Value};

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
    let provider = embed_provider(&cfg, workspace)?;
    let report = embed_run::embed(workspace, &cfg, provider.as_ref())?;
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
    Err(format!("{}: {}", report.outcome.label(), report.outcome.detail().unwrap_or_default()).into())
}

/// The provider `[embeddings]` names.
///
/// Two of the three returns hand over [`NoEmbeddingProvider`] as a **placeholder for a state that refuses before
/// any vector is asked for**: no provider can be built from `provider = "none"`, from a missing
/// [`np_core::config::EmbeddingsConfig::model`], from an id naming no installed row, or from a row whose declared
/// width is not a number. Each of those is answered by [`np_core::embed_run`] as arm (ii) — or, for a missing
/// artifact, arm (iii), whose message names the command that exists — and none of them reaches the placeholder,
/// because both arms fire before the writer loop.
///
/// The row is read through a connection that closes when this function returns, before `embed_run` opens its
/// own: np-cli holds no SQL of its own, and one workspace's [`np_core::db::ModelRow`] is where a local provider's
/// identity comes from (§3).
fn embed_provider(cfg: &Config, workspace: &Path) -> Result<Box<dyn Embedder>, Box<dyn Error>> {
    match cfg.embeddings.provider {
        // The default. Nothing is built, nothing is contacted, and the report says *configuration disabled*
        // rather than a fault (§2, §6).
        EmbeddingProvider::None => Ok(Box::new(NoEmbeddingProvider)),
        EmbeddingProvider::OpenAiCompatible => {
            // Unreachable through `Config::load`, which refuses this variant by name; kept as a refusal rather
            // than a fallthrough into the local branch, because dialling out is not what this build does.
            Err("embeddings.provider = \"openai-compatible\" names an endpoint provider this build cannot serve".into())
        }
        EmbeddingProvider::LocalModel => {
            let Some(id) = cfg.embeddings.model.as_deref() else {
                return Ok(Box::new(NoEmbeddingProvider));
            };
            let db = workspace_db(cfg, workspace)?;
            let Some(row) = model_row(&db, id)? else {
                return Ok(Box::new(NoEmbeddingProvider));
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
                return Ok(Box::new(NoEmbeddingProvider));
            };
            let artifact = cfg.workspace_models_dir(workspace).join(id);
            match LocalModelProvider::new(&row, &artifact, configured) {
                Ok(provider) => Ok(Box::new(provider)),
                Err(np_core::embed::EmbedError::ArtifactUnreadable { .. }) => {
                    Ok(Box::new(NoEmbeddingProvider))
                }
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
    let _ = writeln!(out, "verify     {}", report.verification);
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
