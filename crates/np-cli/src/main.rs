//! `np` — command line entry point for note-processor.
//!
//! Three command groups: `np scan`, `np embed`, and the M2a model commands under `np models`. All are parsers
//! and printers only — every decision about what an install means, which id is legal, or what state an artifact
//! is in lives in [`np_core::models`], so this file holds no SQL and defines no second SHA-256
//! ([`np_core::hash::sha256_file`] is the one digest in the program, and `np models hash` calls it).

use std::error::Error;
use std::io::{self, IsTerminal, Write};
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

mod serve;

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
    ///
    /// **This command may fetch** (**F38**). When `[embeddings] model` names an id this build pins and the artifact is
    /// missing, `embed` transfers it before embedding — because the consumer of this program is increasingly another
    /// program that wants vectors to exist rather than a person willing to sequence three commands. It is **on by
    /// default** (the human's ruling, 2026-11-08) and switched off three ways: `--no-auto-fetch` for one run,
    /// `[models] auto_fetch = false` in the file, `NP_NO_AUTO_FETCH=1` in the environment. It fires only for **pinned**
    /// ids from this build's compiled-in origin, verifies the digest before publishing anything, and never happens in a
    /// build with no inference runtime — there is nothing there to read the bytes with. `np search` has no such flag:
    /// reading a vault must never cost a download.
    Embed {
        /// Workspace root to embed.
        workspace: PathBuf,
        /// Print the report as JSON instead of a summary.
        #[arg(long)]
        json: bool,
        /// Fetch the pinned artifact if this workspace is missing it (the default; see `[models] auto_fetch`).
        #[arg(long)]
        auto_fetch: bool,
        /// Never dial out from this run, even where the config allows it. For air-gapped machines, metered links and CI.
        #[arg(long, conflicts_with = "auto_fetch")]
        no_auto_fetch: bool,
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
        /// Text to match — embedded for the vector list, tokenised for the keyword list.
        query: String,
        /// Which lists to rank: `hybrid` (the default), `semantic` (vectors only) or `keyword` (FTS5 only).
        /// Overrides `[search] mode`; an unknown name is refused rather than ignored.
        /// Absent means whatever `[search] mode` says, which in turn defaults to `hybrid`.
        #[arg(long, value_enum)]
        mode: Option<SearchModeFlag>,
        /// Maximum number of hits to print, highest score first.
        #[arg(long, default_value_t = DEFAULT_SEARCH_LIMIT)]
        limit: usize,
        /// Print the hits as JSON instead of one line each.
        #[arg(long)]
        json: bool,
    },
    /// Answer JSON-RPC 2.0 requests on stdin/stdout until stdin closes — the backend surface for an app.
    ///
    /// One process, no port, no daemon: stdout carries exactly one JSON object per line and nothing else, every human sentence goes to stderr, and
    /// each method's `result` is the same document `np <verb> --json` prints. A workspace is named per request rather than remembered, so one
    /// server can serve several vaults and a stale session cannot answer for a directory you moved. Design:
    /// `docs/superpowers/specs/2026-12-08-np-serve-stdio-design.md`.
    Serve,
    /// Index a workspace, fetch its pinned model if this build can, embed it — one command that ends with vectors or says
    /// exactly where it stopped (**F38**).
    ///
    /// Built for an application that wants vectors to exist and does not want to sequence three commands and interpret
    /// three exit codes. The order is deliberate: **scan first**, because keyword search works the moment indexing
    /// finishes (F36) and a download must not stand between a person and their first results; fetch and embed second.
    ///
    /// Exit codes are split by *kind* of stopping place, which is the one judgement in this command worth reading twice:
    /// offline exits **0** with `vectors: false, offline: true` — the vault is indexed, search answers, and the missing
    /// thing is a cable rather than a mistake to fix — while every other refusal on the vector half (no provider, an
    /// artifact absent with fetching switched off, a binary built without inference) exits **1**, because those need
    /// someone to change something. `np embed` keeps its own stricter contract: it asked for vectors and got none.
    Setup {
        /// Workspace root to index and embed.
        workspace: PathBuf,
        /// Print one composed report as JSON instead of the step lines.
        #[arg(long)]
        json: bool,
        /// Fetch the pinned artifact if this workspace is missing it (the default; see `[models] auto_fetch`).
        #[arg(long)]
        auto_fetch: bool,
        /// Never dial out from this run, even where the config allows it. For air-gapped machines, metered links and CI.
        #[arg(long, conflicts_with = "auto_fetch")]
        no_auto_fetch: bool,
        /// Start a transfer without asking. Implied whenever stdin is not a terminal, which is every app's case.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Install and inspect the model artifacts of one workspace.
    Models {
        #[command(subcommand)]
        command: ModelsCommand,
    },
}

/// `--mode` as clap sees it. A separate `clap` enum because the value parser and the help list have to come from the same
/// place as the names; [`SearchModeFlag::to_core`] is the only bridge, so a fourth spelling cannot appear in one door and not
/// the other (F28's lesson about two lists of the same names).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum SearchModeFlag {
    /// Both lists, fused by rank. The default.
    Hybrid,
    /// Vectors only — the behaviour before FTS5 existed.
    Semantic,
    /// FTS5 only; answers with no model installed and never returns a figure.
    Keyword,
}

impl SearchModeFlag {
    fn to_core(self) -> search::SearchMode {
        match self {
            Self::Hybrid => search::SearchMode::Hybrid,
            Self::Semantic => search::SearchMode::Semantic,
            Self::Keyword => search::SearchMode::Keyword,
        }
    }
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
        /// Declared vector width, 1 to 65535 — declared here, never probed. Required unless `--member` names a member;
        /// refused outright when it does, because only the main artifact has a width.
        #[arg(long, value_parser = parse_dim, required_unless_present = "member")]
        dim: Option<NonZeroU16>,
        /// Which artifact of the model this file is: `main` (the text encoder, the default) or `mmproj` (its projector).
        ///
        /// A member install writes no width and no prefixes — those describe the text encoder — and it requires the model
        /// to be installed already. It exists because a machine that cannot reach Hugging Face still has to be able to
        /// place the 554 MB projector that `np embed` then tells it it is missing (F28).
        #[arg(long)]
        member: Option<String>,
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
    /// The **explicit** dial-out (§12 rule 1): it dials where `[models] base_url` says, or `NP_MODELS_BASE_URL` when the
    /// file leaves that key out, or the one origin baked into the pin table when neither speaks. You ran it, so it runs.
    ///
    /// It is no longer the *only* command that transfers bytes, and this paragraph used to claim it was. **F38** (shipped
    /// 2026-11-09) gave `np embed` and `np setup` an automatic trigger for a missing **pinned** artifact, on by default;
    /// that trigger reuses this command's origin rules, pin, marker and digest verification rather than inventing a second
    /// transport, which is why the surface stayed one door wide at the wire level even though a second command may now
    /// knock. What still never fetches: `np scan`, `np search` (**F38** §5 spells that out) and any id this build does not
    /// pin — an unpinned id is refused before a socket opens.
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
        Command::Embed {
            workspace,
            json,
            auto_fetch,
            no_auto_fetch,
        } => embed_workspace(
            &workspace,
            json,
            AutoFetch::from_flags(auto_fetch, no_auto_fetch),
        ),
        Command::Setup {
            workspace,
            json,
            auto_fetch,
            no_auto_fetch,
            yes,
        } => setup_workspace(
            &workspace,
            json,
            AutoFetch::from_flags(auto_fetch, no_auto_fetch),
            yes,
        ),
        // No `--json` flag to honour here, because there is no other channel to choose: stdout *is* the protocol. The global flag is ignored
        // rather than rejected, so a client that sets it out of habit gets the same stream either way.
        Command::Serve => serve::serve(),
        Command::Search {
            workspace,
            query,
            limit,
            mode,
            json,
        } => search_workspace(
            &workspace,
            &query,
            limit,
            mode.map(SearchModeFlag::to_core),
            json,
        ),
        Command::Models { command } => match command {
            ModelsCommand::Install {
                workspace,
                id,
                path,
                dim,
                sha256,
                prefix_doc,
                prefix_query,
                member,
                json,
            } => {
                // No `--member` is the ordinary install, and it keeps its exact contract: clap has already required the
                // width, so the `None`/`None` arm below is a guard against a future flag change rather than a state a
                // human can reach — and it answers with a message instead of a panic. Written as a `match` because this
                // arm's value is the program's result; `main` has no `Err` to return into.
                match member {
                    None => install_main(&InstallArgs {
                        workspace,
                        id,
                        source: path,
                        dim,
                        sha256,
                        prefix_doc,
                        prefix_query,
                        json,
                    }),
                    Some(member) => models_install_member(
                        &InstallArgs {
                            workspace,
                            id,
                            source: path,
                            dim,
                            sha256,
                            prefix_doc,
                            prefix_query,
                            json,
                        },
                        &member,
                    ),
                }
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
/// Take this workspace's write lock (**F44**), or report that somebody else holds it and stop.
///
/// Every command that changes state calls this before it touches anything, because the defect it closes is not two writers arguing inside
/// SQLite — that argument ends in `database is locked` in 5 ms, which is ugly but survivable. It is that `scan` renders `INDEX.md` *inside*
/// its write transaction: a loser can stop between "the database committed" and "the files were written", leaving an index that describes
/// notes the vault does not contain, and no rollback reaches a file. Exclusion has to happen before the first statement, which is what this
/// returns instead of a boolean: a guard held for the whole command, released by being dropped.
///
/// The refusal is a **state**, reported the way `offline` is: exit 1, one sentence on stderr naming what to do, and — under `--json`, which is
/// how an app asks — one machine-readable object on stdout. Before this, a losing `np scan --json` printed *nothing at all* and an app had to
/// infer contention from an empty document plus exit 1. Readers (`search`, `models list`) never call this function, so
/// a long `embed` cannot lock anybody out of reading: that asymmetry is the whole design, and `crates/np-cli/tests/cli.rs` pins both halves.
fn writer_lock(
    cfg: &Config,
    workspace: &Path,
    command: &str,
) -> Result<np_core::lock::WorkspaceLock, LockFailure> {
    // **§3 step 0 comes first, and it belongs here rather than in each caller**: taking the lock creates a file, and a typo'd workspace path
    // must not leave one behind. `Config::workspace_db` happily derives a name for a directory that does not exist, so a lock taken before
    // this check would create a data directory and a `<hash>.lock.db` for a vault nobody has — the exact failure §3 step 0 exists to prevent,
    // re-opened through a new door. Every mutating command requires an existing workspace directory anyway, so nothing loses coverage.
    models::require_workspace_dir(workspace).map_err(|source| LockFailure::Other(source.into()))?;
    let db_path = cfg.workspace_db(workspace);
    // The lock is named after the workspace database rather than recomputing an id, so there is no second derivation to drift from the first:
    // same directory, same stem, `.lock.db`. Copy a workspace's data somewhere else and its lock comes with it; two workspaces never contend.
    let dir = db_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let stem = db_path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| {
            LockFailure::Other(
                format!("cannot name the workspace lock for {}", db_path.display()).into(),
            )
        })?;
    match np_core::lock::WorkspaceLock::acquire(
        dir,
        stem,
        np_core::lock::WorkspaceLock::DEFAULT_WAIT,
    ) {
        Ok(held) => Ok(held),
        Err(np_core::lock::LockError::Held { waited }) => Err(LockFailure::Locked {
            command: command.to_string(),
            waited_ms: u64::try_from(waited.as_millis()).unwrap_or(u64::MAX),
        }),
        Err(other) => Err(LockFailure::Other(other.into())),
    }
}

impl LockFailure {
    /// The machine-readable form of a refusal, built once for every channel (**F44**).
    ///
    /// `locked` sits beside `offline` and `fetched` as the third thing that can stop a mutating command without being anybody's bug. It is a
    /// function rather than a literal at a print site because `np serve` has to put *this same object* in a JSON-RPC result: two places building
    /// it is two chances for a screen and a terminal to disagree about what contention means.
    fn document(&self) -> Option<Value> {
        match self {
            Self::Locked { command, waited_ms } => Some(json!({
                "command": command,
                "locked": true,
                "waited_ms": waited_ms,
            })),
            Self::Other(_) => None,
        }
    }

    /// The CLI's shape of a refusal: under `--json` the document goes to stdout first, then the sentence travels the error channel and decides the
    /// exit code — two channels, one fact. Other channels render [`Self::document`] their own way, which is why this takes the channel as an
    /// argument instead of the lock helper taking a boolean.
    fn report_cli(self, json: bool) -> Box<dyn Error> {
        // Printed before `self` is consumed below: a JSON caller that gets only a sentence on stderr is the exact F44 defect this type exists to
        // prevent (before it, a losing `np scan --json` printed nothing at all).
        if json {
            if let Some(doc) = self.document() {
                emit_json(&doc);
            }
        }
        match self {
            Self::Locked { waited_ms, .. } => format!(
                "locked: another process is writing this workspace ({waited_ms} ms) — try again once it finishes; \
                 `np search` and `np models list` answer while a writer runs"
            )
            .into(),
            Self::Other(source) => source,
        }
    }
}

/// Whether the server may work, as three outcomes rather than two.
///
/// **F44 says contention is a state, and a state must survive every channel.** A JSON-RPC *fault* would fire an app's generic "the backend refused"
/// handler on something it is meant to retry, so `np serve` turns contention back into the very document `np scan --json` prints under contention
/// ([`LockFailure::document`]) and lets the caller branch on `locked`.
enum LockGrant {
    /// Ours to work under; the guard drops once the request's document is built.
    Held(np_core::lock::WorkspaceLock),
    /// Somebody else is writing, and this is the document that says so.
    Contended(Value),
}

fn server_writer_lock(
    cfg: &Config,
    workspace: &Path,
    command: &str,
) -> Result<LockGrant, Box<dyn Error>> {
    match writer_lock(cfg, workspace, command) {
        Ok(guard) => Ok(LockGrant::Held(guard)),
        Err(LockFailure::Locked { waited_ms, command }) => Ok(LockGrant::Contended(
            LockFailure::Locked { command, waited_ms }
                .document()
                .expect("a locked refusal always has a document"),
        )),
        Err(LockFailure::Other(source)) => Err(source),
    }
}

/// Why a mutating command could not take the workspace, as data rather than as a printed sentence.
///
/// The distinction that matters is between *contention* — a state, retry it, and readers keep answering — and *everything else*, which is a
/// refusal that needs somebody to change something. Collapsing both into `Box<dyn Error>` was fine while the CLI was the only channel: there was
/// exactly one way to render either. `np serve` needs the first one's document, so the two kinds are separate arms now.
enum LockFailure {
    /// Another process holds the workspace. The two fields are what [`Self::document`] carries, so a caller never parses a sentence to learn this.
    Locked { command: String, waited_ms: u64 },
    /// Anything else that stopped us before work: no workspace directory, an unreadable data dir, a name we cannot derive from the database path.
    Other(Box<dyn Error>),
}

fn scan_workspace(workspace: &Path, json: bool) -> Result<(), Box<dyn Error>> {
    let cfg = load_config()?;
    // Held for the whole command, including the `INDEX.md` rendering that SQLite cannot roll back.
    let _writer =
        writer_lock(&cfg, workspace, "scan").map_err(|failure| failure.report_cli(json))?;
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
/// Whether this run may fetch a pinned artifact it is missing (**F38**), resolved from argv before anything else.
///
/// Three values rather than a `bool` because "the operator said yes" and "nobody said anything" must not collapse: the
/// first ignores the config file, the second defers to it. Resolution order is **argv → environment+file** — argv last
/// in the chain of authority but highest, so `--no-auto-fetch` outranks both `[models] auto_fetch = true` and an
/// accidentally exported variable, which is what makes the flag worth having on a metered link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoFetch {
    /// Neither flag given: ask `Config::auto_fetch_allowed`, which consults `[models] auto_fetch` and `NP_NO_AUTO_FETCH`.
    Deferred,
    /// `--auto-fetch`: fetch even if the file says no (the environment kill switch still wins — see [`AutoFetch::allowed`]).
    Forced,
    /// `--no-auto-fetch`: never fetch from this run.
    Refused,
}

impl AutoFetch {
    fn from_flags(on: bool, off: bool) -> Self {
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
    fn allowed(self, cfg: &Config) -> bool {
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
fn embed_report(
    workspace: &Path,
    auto_fetch: AutoFetch,
    announce: bool,
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
        // names the command. `np search --json` is the caller that needs the distinction (**F45**), and it reads the field directly.
        fetch_declined: _,
    } = embed_provider(&cfg, workspace, auto_fetch.allowed(&cfg), announce)?;
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
    // **F40 splits this mapping by build shape.** In a binary that links no runtime, `ProviderState::Ready` holds the
    // seam's placeholder — a provider whose every batch answers `RuntimeNotBuilt`. Handing it to the run is how an
    // operator who installed a model correctly (digest verified, 295.5 MB) got `outcome owed and not produced` with
    // `errors 1`: §6's report-integrity arm, quoting a per-batch error, after the corpus had been read. This build now
    // refuses before any of that, naming the feature and the two ways forward. A `local-model` build keeps the original
    // mapping untouched — there, "the bytes would not load" genuinely *is* arm (iv), and its own test says so.
    #[cfg(not(feature = "local-model"))]
    let resolution = match &mut state {
        ProviderState::Ready(_) => Resolution::RuntimeNotBuilt {
            id: cfg.embeddings.model.clone().unwrap_or_default(),
        },
        ProviderState::Placeholder { embedder, .. } => Resolution::Provider(embedder.as_mut()),
        ProviderState::ArtifactAbsent => Resolution::ArtifactAbsent,
        // **F38 arm (vi).** The fetch was allowed, attempted, and nothing answered; the run says so before reading a chunk.
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
    let mut report = embed_run::embed_with_resolution(workspace, &cfg, resolution)?;
    // What was concluded about the model's bytes travels on the report rather than being printed here, so `--json`
    // and the text summary cannot disagree about whether this run verified anything (the same reason §7's caveat is a
    // field). `None` says no local artifact was involved — a `provider = "none"` run has nothing to claim.
    report.artifact_check = artifact_check;
    // Likewise the worker count: decided where the provider was built, printed here. `None` means **no local-model
    // provider was constructed** — which is not the same sentence as "nothing was embedded": a `local-model` run whose
    // notes were all current still built a provider, and its report states the count it would have used.
    report.workers = workers;
    // **F38's machine-readable answer to "did this invocation download a model?"** — set here rather than inside the run
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
fn embed_refusal(report: &EmbedReport) -> String {
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

/// **F42 option A: say it out loud and keep going.** One stderr line when the model row on file declares instruction prefixes this build's
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
/// round-four row was that case; **F42**'s card said otherwise and has been corrected.
fn prefix_drift_notice(
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
    Ok(Some(drift))
}

/// Insert `prefix_drift` into a document only when there is drift to report. A key that is usually null would be schema noise; a key that
/// appears exactly when the reader has something to act on is a signal.
fn insert_drift(
    mut doc: serde_json::Value,
    drift: Option<np_core::pins::PrefixDrift>,
) -> Result<serde_json::Value, Box<dyn Error>> {
    if let (Some(obj), Some(drift)) = (doc.as_object_mut(), drift) {
        obj.insert("prefix_drift".to_string(), serde_json::to_value(drift)?);
    }
    Ok(doc)
}

/// `np embed`: run the half, print it in the channel the caller asked for, and turn a stopped run into exit 1.
fn embed_workspace(
    workspace: &Path,
    json: bool,
    auto_fetch: AutoFetch,
) -> Result<(), Box<dyn Error>> {
    // The lock spans the run: an `embed` writes vectors and re-renders `INDEX.md`, and a second writer in the same vault is what F44 measured.
    let cfg_for_lock = load_config()?;
    let _writer = writer_lock(&cfg_for_lock, workspace, "embed")
        .map_err(|failure| failure.report_cli(json))?;
    let report = embed_report(workspace, auto_fetch, !json)?;
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

/// `np setup`: index first, then embed (which may fetch), then say in one place how far this got.
///
/// Scan runs **before** anything network-facing on purpose: keyword search works the moment indexing finishes (F36), so a
/// 310 MB transfer — or the four seconds spent discovering there is no network — must not stand between a person and their
/// first results. The embed half is [`embed_report`] unchanged, which means its arm order, its fetch trigger, and its
/// report fields are the same code `np embed` runs; setup adds an ordering and a verdict, not a second implementation.
///
/// Exit codes split by kind of stopping place: **offline exits 0** (indexed, searchable, waiting for a cable — nothing for
/// anyone to fix), every other vector-half refusal exits 1 with embed's own sentence. `vectors` is the machine-readable
/// verdict either way, so a caller never has to infer it from an exit code.
fn setup_workspace(
    workspace: &Path,
    json: bool,
    auto_fetch: AutoFetch,
    yes: bool,
) -> Result<(), Box<dyn Error>> {
    // ---- step 1: index. No `--json` forwarding here: setup composes one document at the end, and a summary printed on
    // the way would be a second, partial account of the same run.
    let cfg = load_config()?;
    // One lock for the whole composed run. It is taken before the consent prompt in step 2 and held through it, which is deliberate: by then
    // this process has decided it intends to write, and a second writer that started while a human was reading the question would land in the
    // gap between this command's index and its embed. Readers are unaffected — `np status` answers over a held lock, and the tests say so.
    let _writer =
        writer_lock(&cfg, workspace, "setup").map_err(|failure| failure.report_cli(json))?;
    let indexed = scan(workspace, &cfg)?;

    if !json {
        // Printed before step 3 so the lines arrive in the order the work happened: index counts, then any fetch
        // announcement, then the embed outcome. `--json` prints nothing here and composes one document at the end.
        print_summary(&indexed);
    }

    // ---- step 2: the courtesy, if anyone is there to be asked.
    let auto_fetch = if consent_to_fetch(&cfg, workspace, &auto_fetch, yes)? {
        auto_fetch
    } else {
        // "No" from a person is exactly as binding as `--no-auto-fetch`, and travels by the same value rather than by a
        // flag re-parse — one representation of "this run must not dial out".
        AutoFetch::Refused
    };

    // ---- step 3: embed, through the same function `np embed` calls.
    let embedded = embed_report(workspace, auto_fetch, !json)?;

    let drift = match embedded.model.as_deref() {
        Some(id) => prefix_drift_notice(&cfg, workspace, id)?,
        None => None,
    };
    if json {
        let doc = insert_drift(
            json!({
            "workspace": workspace.display().to_string(),
            "scan": serde_json::to_value(&indexed)?,
            "embed": serde_json::to_value(&embedded)?,
            // The two verdicts a caller branches on, spelled at the top level so nobody has to know that `vectors` lives
            // inside embed's report or that `offline` is a field of it: setup's contract is one question — "do I have
            // vectors now, and if not, is that because of the network?"
            "vectors": embedded.succeeded(),
            "offline": embedded.offline,
            "fetched": serde_json::to_value(&embedded.fetched)?,
            }),
            drift,
        )?;
        emit_json(&doc);
    } else {
        print_embed_summary(&embedded);
        let verdict = if embedded.offline {
            // The sentence the design names, kept short because it is the line a person reads and an app may match.
            "setup      indexed; vectors skipped — offline"
        } else if embedded.succeeded() {
            "setup      indexed and embedded — semantic search is ready"
        } else {
            // Not offline, not done: state which, in the same words `np embed` would have used, and let the exit code agree.
            "setup      indexed; no vectors — see the outcome above"
        };
        emit(verdict);
    }

    if embedded.offline {
        // Indexed and searchable is a result, not a failure: exiting 1 here would tell an app to retry forever over a
        // condition that needs no code change, and F36 already answers queries meanwhile.
        return Ok(());
    }
    if embedded.succeeded() {
        return Ok(());
    }
    Err(embed_refusal(&embedded).into())
}

/// Ask before transferring, when a person could answer — and never ask when one cannot (**F38**).
///
/// Returns `true` when the fetch may proceed. It can only ever **veto**: if this predicate and the trigger in
/// [`embed_provider`] ever disagree, the worst outcome is a question that did not need asking, or no question before a
/// transfer that was always permitted. That asymmetry is deliberate — the trigger stays the single authority on *whether*
/// bytes may be fetched, and this function borrows its conditions only to put a byte count in front of someone.
///
/// Silent (so: proceeds) when `--yes` was passed, when stdin is not a terminal — which is every app's case, and the reason
/// the default cannot be "ask" — or when the kill switch already forbids the transfer, in which case there is nothing to
/// ask about.
fn consent_to_fetch(
    cfg: &Config,
    workspace: &Path,
    auto_fetch: &AutoFetch,
    yes: bool,
) -> Result<bool, Box<dyn Error>> {
    if yes || !auto_fetch.allowed(cfg) || !io::stdin().is_terminal() {
        return Ok(true);
    }
    let Some(id) = pending_pinned_fetch(cfg, workspace)? else {
        return Ok(true);
    };
    let bytes: u64 = np_core::pins::members_for_build()
        .iter()
        .filter_map(|name| np_core::pins::pinned_member(&id, name))
        .map(|pin| pin.size_bytes)
        .sum();
    emit(&format!(
        "about to fetch {id} ({}) from {}",
        size_human(bytes),
        origin_kind(cfg)
    ));
    emit("this downloads a model into this workspace's data dir. proceed? [y/N] ");
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// The pinned id whose artifact this workspace is missing, or `None` when nothing is pending. Mirrors the trigger's
/// condition in [`embed_provider`]; see [`consent_to_fetch`] for why a mirror that drifts fails safe.
fn pending_pinned_fetch(cfg: &Config, workspace: &Path) -> Result<Option<String>, Box<dyn Error>> {
    let db = workspace_db(cfg, workspace)?;
    let Some(id) = cfg.embeddings.model.clone() else {
        return Ok(None);
    };
    if np_core::pins::identity(&id).is_none() {
        return Ok(None);
    }
    // The same path and the same existence test as the trigger two functions up: `models_dir.join(id)`, no extension.
    let artifact = cfg.workspace_models_dir(workspace).join(&id);
    if model_row(&db, &id)?.is_some() && artifact.is_file() {
        return Ok(None);
    }
    Ok(Some(id))
}

/// Which *kind* of origin a fetch would go to — the half of [`origin_kind_line`] that fits in a sentence. One source for
/// both, so the prompt and the announcement can never disagree about where bytes come from, and neither says a host.
fn origin_kind(cfg: &Config) -> &'static str {
    if cfg.models.base_url.is_none() {
        "this build's compiled-in default"
    } else {
        "[models] base_url"
    }
}

/// The same fact as [`origin_kind`] in the stable token `--json` carries (`fetched.origin`). Distinct from the human
/// wording on purpose — a program keys on this, a person reads that — and defined next to it so one `match` decides both.
///
/// Only the fetch path reads it, so a build with no fetch has nothing to use it for.
#[cfg_attr(not(all(feature = "local-model", feature = "fetch")), allow(dead_code))]
fn origin_kind_token(cfg: &Config) -> &'static str {
    if cfg.models.base_url.is_none() {
        "compiled-in-pin"
    } else {
        "configured-mirror"
    }
}

/// Rank `query` against `workspace`'s index and print the hits.
///
/// The provider is built exactly as `np embed` builds it, because the query must be embedded by the **same** model whose
/// rows are being read — and under its `prefix_query` spelling, not the document one ([`Embedder::embed_query`]). That is also
/// where a run that cannot embed refuses at exit 1, with the sentence for its own [`ProviderState`]: an absent artifact, an
/// unbuilt runtime, a missing row, a switched-off provider. A query nobody can embed has no ranking to print (F32: until this
/// round four of those five shared one sentence about `provider = "none"`).
/// What one search produced, before anybody printed it.
///
/// **Why this type exists.** `np search` used to interleave "work out the answer" with "write bytes to stdout", which meant the only way to get a
/// search document was to print one. `np serve` needs the same object on a different channel, and the alternative to this struct was a second
/// implementation of provider resolution, fetch policy and note selection — two vocabularies for one state, which is the failure every card from
/// F32 onward has been about. The CLI prints this; the server serialises it; neither can drift, because there is one of each function.
struct SearchRun {
    hits: Vec<search::Hit>,
    mode: search::SearchMode,
    /// Whether the index holds vectors at all — the truth about the workspace, which **F45** ruled must stay `true` even when this answer could
    /// not use them, because a reader that sees `false` will go and re-fetch a model it does not need.
    present: bool,
    note: Option<search::RankNote>,
    fetched: Option<Value>,
    offline: bool,
    /// The id whose model (if any) embedded the query — needed only by the text channel's "nothing indexed for …" sentence.
    model: String,
}

/// Run one search. `announce` is the human channel: it lets provider resolution print what it is about to download, and is off for `--json` and
/// for `np serve`, where the document carries those facts instead (`fetched.origin_kind`, `reason`).
fn search_run(
    workspace: &Path,
    query: &str,
    limit: usize,
    mode: Option<search::SearchMode>,
    announce: bool,
) -> Result<SearchRun, Box<dyn Error>> {
    let cfg = load_config()?;
    // Config first, flag over it: `[search] mode` is the standing choice and `--mode` is the exception to it.
    let mode = mode.unwrap_or(cfg.search.mode);
    let db = workspace_db(&cfg, workspace)?;

    // **Which halves this query can ask about, decided before any model is loaded.** Keyword mode needs no provider at all —
    // that is the property the human asked for when hybrid became the default: search answers on a machine that has never
    // downloaded 310 MB.
    //
    // **Two rulings rewrote this block on 2026-11-09, which is why the comments are long.**
    //
    // *F45 (option A).* Hybrid is the mode that always answers. Until now it degraded to keyword only when the index held no
    // vectors; with vectors in it and the semantic half unbuildable — `embeddings.provider = "none"`, a model that was never
    // installed here, bytes deleted after an earlier embed — it exited 1 having written **zero bytes to stdout**, while
    // `--mode keyword` answered the same query happily. An application calling the default mode could not tell "no results" from
    // "search is broken", and the operator's own harness produced that state on every run. So hybrid no longer refuses for a
    // missing semantic half: it answers from FTS5 and says which half was absent, in prose on stderr and in a stable token under
    // `reason`. `--mode semantic` keeps every refusal it had — asking for semantics and silently receiving keyword results is the
    // exact failure F32 and F36 were built to prevent.
    //
    // *First install (the operator's amendment to F38 §5).* "When the program has it first install on a machine it tries to
    // install the model; if it has no internet connection then it falls back." Hybrid may therefore trigger the pinned fetch, and
    // offline is a state it reports rather than a failure it throws. The metered-laptop objection F38 §5 recorded is answered by
    // the conditions rather than by a new flag: configuration must name a `local-model` provider and an id **this build pins**,
    // the runtime and the HTTP client must be compiled in, `[models] auto_fetch = false` / `NP_NO_AUTO_FETCH=1` still veto,
    // `--mode keyword` never dials out, and neither does `--mode semantic` — refusing *is* that mode's answer, so a download
    // could buy nothing. Hybrid can still answer after fetching, and that is the difference. Search writes no vectors and touches
    // no `INDEX.md`: the artifact cache is the only thing it may create, and `np embed` / `np setup` stay the commands that make a
    // vault semantic.
    let semantic_wanted = mode != search::SearchMode::Keyword;
    let present = search::vectors_present(&db)?;
    let hybrid = mode == search::SearchMode::Hybrid;
    // Keyword mode never resolves a provider, so it can never reach the transport. Both other modes resolve one — and semantic
    // resolving even when the index holds no vectors is not an oversight to be tidied away: it is how `--mode semantic` tells
    // "this workspace has never been embedded" (empty answer, exit 0) apart from "this workspace was embedded under a different
    // id" (`SearchError::NoVectorsForModel`, a refusal). An earlier draft of this change gated semantic mode on `present` and
    // turned that refusal into silence; the test that caught it is `search_with_embedding_disabled_refuses_and_names_the_setting`.
    //
    // Semantic therefore never fetches: resolution runs with `auto_fetch = false`, so a mode whose honest answer can be "I cannot
    // do what you asked" does not spend 309 MB first. Hybrid passes its own name in, which is what makes the first-install
    // download happen on the mode that can still answer afterwards.
    let resolve = semantic_wanted;
    let mut note: Option<search::RankNote> = None;
    let mut vector: Option<Vec<u8>> = None;
    let mut model = String::new();
    // `fetched` and `offline` carry F38's meanings into search's document so one caller vocabulary covers both commands: "what
    // arrived from an origin", and "the pinned origin could not be reached". `reason` (below) says which half is missing.
    let mut fetched: Option<Value> = None;
    let mut offline = false;

    if resolve {
        // **The mode opens the door; only configuration and the environment bolt it.** `AutoFetch::Deferred` is the same question
        // `np embed` asks when its caller passed neither `--auto-fetch` nor `--no-auto-fetch`: `[models] auto_fetch` first, then
        // `NP_NO_AUTO_FETCH=1`, which outranks everything. Search has no fetch flags of its own on purpose — `--mode` is the only
        // lever it exposes — so a search that ignored this would download 309 MB on a host whose operator had switched fetching
        // off for every command. That is not a hypothetical: the first draft of this change passed `hybrid` straight through, and
        // `search_with_fetching_switched_off_says_so_instead_of_blaming_the_network` failed on the `local-model,fetch` row saying
        // `offline:true` where the switch meant "nothing was attempted".
        let fetch_allowed = hybrid && AutoFetch::Deferred.allowed(&cfg);
        let resolved = embed_provider(&cfg, workspace, fetch_allowed, announce)?;
        let Resolved {
            state,
            fetched: arrived,
            fetch_declined,
            ..
        } = resolved;
        // Produced where the transfer happens (provider resolution), not re-derived here — the same reasoning that put
        // `artifact_check` and `workers` on `Resolved`.
        if let Some(record) = arrived {
            fetched = Some(serde_json::to_value(&record)?);
        }
        match (mode, state) {
            (_, ProviderState::Ready(provider)) => {
                // The id the index is keyed by, taken from the provider that made those rows rather than re-read from config: a
                // `[embeddings] model` that disagrees with the installed row would otherwise search one model and report another.
                model = provider.model_id().to_string();
                // **Semantic always embeds the query; hybrid only when there is something to compare it with.** That asymmetry is
                // not a micro-optimisation: embedding the query is what surfaces provider-level refusals, and one of them is F40's
                // `runtime is not built` — a build that links no inference has to say so to the mode that asked for semantics and got
                // nothing, naming the id that stayed owed. An earlier draft skipped the call whenever the index held no vectors and
                // turned that refusal into a silent empty answer; `searching_without_the_runtime_names_the_model_that_stayed_owed`
                // is what caught it. Hybrid may skip, because hybrid has an answer to give either way — and when it does skip, the
                // runtime complaint is not lost, only handed to the command that cannot do its job without it (`np embed`, arm (v)).
                if present || !hybrid {
                    vector = Some(provider.embed_query(query)?.to_bytes());
                } else {
                    // Hybrid with nothing embedded yet. The provider resolved — and may have just arrived on disk — but a model
                    // sitting in a cache writes no vectors by itself, so say which of the two states this is: "run `np embed`" means
                    // something different after a download than it does before one.
                    note = Some(if fetched.is_some() {
                        search::RankNote::ArtifactFetched
                    } else {
                        search::RankNote::NoVectors
                    });
                }
            }
            // ---- `--mode semantic`: every refusal below is unchanged from F32/F38, wording included. Asked for semantics, cannot
            // do semantics: exit 1, not a keyword list with an apology attached.
            (search::SearchMode::Semantic, ProviderState::Placeholder { reason, .. }) => {
                return Err(reason.sentence(workspace).into())
            }
            // One line, spelled out here rather than assembled across a wrapped literal: the first version broke mid-sentence and
            // printed a run of twenty spaces between "be" and "embedded". The pointer is `install_hint`, the feature-gated wording
            // embed's arm (iii) prints — a build that compiled `fetch` out must not be sent to it.
            (search::SearchMode::Semantic, ProviderState::ArtifactAbsent) => {
                return Err(format!(
                    "no embedding provider for this workspace: its model artifact is absent, so the query cannot be embedded: {}",
                    embed_run::install_hint(workspace)
                )
                .into())
            }
            (search::SearchMode::Semantic, ProviderState::FetchSkippedOffline { id }) => {
                return Err(format!(
                    "offline: could not reach the pinned origin to fetch `{id}`, so the query cannot be embedded — \
                     connect and try again, or search by keyword (`np search --mode keyword`)")
                    .into())
            }
            // ---- Hybrid (**F45**): a half is missing and the answer is not withheld. These are separate arms rather than one `_`
            // mapping everything to "no vectors" because each is a different reader's mistake with a different next command.
            (_, ProviderState::FetchSkippedOffline { .. }) => {
                offline = true;
                note = Some(search::RankNote::FetchOffline);
            }
            // Policy said no, which outranks the filesystem facts below: someone who set `NP_NO_AUTO_FETCH=1` and forgot must not
            // be told to install bytes they deliberately declined to download.
            (_, _) if fetch_declined => note = Some(search::RankNote::FetchDisabled),
            (
                search::SearchMode::Hybrid,
                ProviderState::Placeholder {
                    reason: NoProvider::ConfigDisabled | NoProvider::ModelUnnamed,
                    ..
                },
            ) => note = Some(search::RankNote::ProviderDisabled),
            // RowMissing and RowDimUnusable both mean "the model you named is not what this workspace holds". Kept apart from
            // `provider-disabled` because the remedy differs: one edits a file, the other fetches 309 MB.
            (search::SearchMode::Hybrid, ProviderState::Placeholder { .. }) => {
                note = Some(search::RankNote::ModelNotInstalled)
            }
            (search::SearchMode::Hybrid, ProviderState::ArtifactAbsent) => {
                note = Some(search::RankNote::ArtifactMissing)
            }
            // ---- Unreachable by construction: `resolve` is false when the mode is keyword, so no provider state can arrive here.
            // Spelled as refusals rather than `unreachable!()` or a `_` arm for two reasons. The F32 rule at this call site is
            // "every `ProviderState` has an arm", which a wildcard would silently end; and if a future change does resolve a
            // provider for keyword mode, a visible wrong sentence is better to debug than a panic in someone's app.
            (_, ProviderState::Placeholder { reason, .. }) => {
                return Err(reason.sentence(workspace).into())
            }
            (_, ProviderState::ArtifactAbsent) => {
                return Err(format!(
                    "no embedding provider for this workspace: its model artifact is absent, so the query cannot be embedded: {}",
                    embed_run::install_hint(workspace)
                )
                .into())
            }
        }
    } else if semantic_wanted {
        note = Some(search::RankNote::NoVectors);
    }

    let answer = search::ranked(
        &db,
        mode,
        // Hybrid asks no question of a model it did not load; `ranked` reads the id only when a vector arrives.
        if vector.is_some() {
            Some(model.as_str())
        } else {
            None
        },
        vector.as_deref(),
        query,
        limit,
    )?;
    let hits = answer.hits;
    let note = note.or(answer.note);

    Ok(SearchRun {
        hits,
        mode,
        present,
        note,
        fetched,
        offline,
        model,
    })
}

/// The document `np search --json` prints, in one function so no second channel can invent its own keys.
///
/// One object per hit under `hits`, beside the two keys that say how the list was made (§12). The empty case is an empty array — a machine reader
/// does not need a sentence, and a second key per hit would be a schema change in disguise; `mode` and `vectors` describe the *list*, so they sit
/// beside it.
///
/// **F45/F38 keys, and why `reason` is not the same thing as `note`.** `note` is prose for a human and may be reworded; `reason` is the token a
/// caller branches on when it wants to know which half answered. `fetched` and `offline` mean exactly what they mean in `np embed --json` (F38),
/// so one reader handles both commands: an application that called `np search` first on a fresh machine can now see `fetched` arrive,
/// `vectors:false`, and `reason:"artifact-fetched"` telling it the next call is `np embed`. Nothing here changes when the semantic half worked:
/// `reason` is null and `note` stays null, exactly as before.
fn search_document(run: &SearchRun) -> Value {
    let objects: Vec<Value> = run.hits.iter().map(hit_json).collect();
    json!({
        "hits": objects,
        "mode": run.mode.label(),
        "vectors": run.present,
        "note": run.note.as_ref().map(search::RankNote::label),
        "reason": run.note.as_ref().map(search::RankNote::token),
        "fetched": run.fetched.clone(),
        "offline": run.offline,
    })
}

/// `np search`: run the query, then print it in the channel the caller asked for.
fn search_workspace(
    workspace: &Path,
    query: &str,
    limit: usize,
    mode: Option<search::SearchMode>,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let run = search_run(workspace, query, limit, mode, !json)?;
    if json {
        emit_json(&search_document(&run));
        return Ok(());
    }
    let SearchRun {
        hits,
        mode,
        note,
        model,
        ..
    } = run;
    let mut out = io::stdout().lock();
    if let Some(reason) = note {
        // One line on stderr for a half that was not there to answer. Stderr because stdout stays parseable; the exit code is
        // still 0 when the keyword list answered, which is the point of the mode.
        let _ = writeln!(io::stderr(), "{reason}");
    }
    if hits.is_empty() {
        // §12: an empty index is a fact. Written to stdout, exit 0 — the caller has nothing to fix.
        // `--limit 0` asks for no hits and gets no lines, which is not the same sentence, so it is not printed.
        if limit > 0 {
            // Three sentences, because the old two printed a lie on Windows' most likely first run. With no `[embeddings]`
            // section and no vectors, `model` is the empty string — and the hybrid arm interpolated it, producing
            // `nothing indexed for  by hybrid`: a double space where a model name should be, and a claim about an index that
            // the keyword half had just consulted. When there is no model to name, the sentence names the query and says the
            // vectors are missing instead (found by the first interactive Windows run, 2026-11-07).
            let _ = match (mode, model.is_empty()) {
                (search::SearchMode::Keyword, _) => writeln!(out, "nothing matched \"{query}\""),
                (_, true) => writeln!(
                    out,
                    "nothing matched \"{query}\" by {mode}: this workspace holds no vectors"
                ),
                (_, false) => writeln!(out, "nothing indexed for {model} by {mode}"),
            };
        }
        return Ok(());
    }
    for hit in &hits {
        // Score to four decimals (full precision travels in `--json`), kind padded so the paths start in one column. In
        // hybrid the score is a fused rank sum, not a similarity — which is why the raw numbers ride beside it here too.
        let score = number(hit.score);
        let evidence = match (hit.cosine, hit.bm25) {
            (Some(cosine), Some(bm25)) => format!("cos {} bm25 {}", number(cosine), number(bm25)),
            (Some(cosine), None) => format!("cos {}", number(cosine)),
            (None, Some(bm25)) => format!("bm25 {}", number(bm25)),
            (None, None) => String::new(),
        };
        let evidence = if evidence.is_empty() {
            String::new()
        } else {
            format!("  [{evidence}]")
        };
        match &hit.source_note {
            Some(note) => {
                // One linker gets named; more than one says so, because the count used to be invisible here and a figure
                // belonging to four notes should not read as though it belongs to the one printed (F28). The note that is
                // printed is `source_note` — the head of the same list `--json` carries in full.
                let extra = match hit.linked_notes.len() {
                    0 | 1 => String::new(),
                    n => format!(" +{} more", n - 1),
                };
                let _ = writeln!(
                    out,
                    "{score} {:<5}  {}   (from {note}{extra}){evidence}",
                    hit.kind.label(),
                    hit.path
                );
            }
            None => {
                let snippet = hit
                    .snippet
                    .as_deref()
                    .map(|text| format!("   … {}", text.replace('\n', " ")))
                    .unwrap_or_default();
                let _ = writeln!(
                    out,
                    "{score} {:<5}  {}{evidence}{snippet}",
                    hit.kind.label(),
                    hit.path
                );
            }
        }
    }
    Ok(())
}
/// A number for the text channel: four decimals when that says something, scientific notation when it does not.
///
/// The fixed `{:.4}` this function replaced was right about cosine, which lives in `[-1, 1]`, and wrong about everything
/// else — SQLite's `bm25()` on a two-note vault answers `-0.000001`-sized values, so a keyword hit led with `0.0000` and
/// read as "no score computed" rather than "a real, tiny, higher-is-better number". The threshold is not tuned: it is the
/// point below which four decimals stop carrying a digit. Full precision has always travelled in `--json`.
fn number(value: f64) -> String {
    if value.abs() < 5e-4 {
        format!("{value:.2e}")
    } else {
        format!("{value:.4}")
    }
}

/// One hit as JSON: **the same eleven keys whatever the kind and whatever the mode** —
/// `{score, cosine, bm25, semantic_rank, keyword_rank, kind, path, snippet, linked_from, source_note, linked_notes}`.
///
/// F36 added the five evidence keys to this list rather than printing them only in the modes that produce them, for the
/// reason below: a schema whose keys depend on a value inside the object makes every reader branch before it reads.
///
/// The first version added `linked_from` and `source_note` only for image hits, which made the schema depend on a value
/// inside the object — a reader then has to branch on `kind` before it knows whether a key is there, and a note hit whose
/// linker field is simply absent reads differently from one that has none. Absent and zero are different statements, so this
/// emits both keys always: `source_note: null` and `linked_from: 0` for a hit that has no linking note (every note hit, and
/// an orphaned figure). Same rule `EmbedReport::artifact_check` already follows — present and null, not missing.
///
/// `linked_notes` follows the same rule in list form: **an empty array, never `null`** (F28). It is the whole relation — every
/// live note linking a figure, byte-ordered — where `source_note` was its head and `linked_from` its length, so the three
/// cannot tell different stories about one hit. An empty array means "this hit has no live linker", which is true of both a
/// note hit and an orphaned figure; distinguishing those two is what `kind` is for, and giving the list a null on one kind
/// and an empty array on the other would put the schema back where `kind`-dependent keys left it.
fn hit_json(hit: &Hit) -> Value {
    json!({
        "score": hit.score,
        // The two raw numbers beside the fused score. In `hybrid` a reader cannot recover them from `score`, and F31 will
        // need both columns to judge ranking at all; in a single-list mode one of them is `null`, which is the same
        // present-and-null statement `source_note` already makes rather than a key that appears only in some modes.
        "cosine": hit.cosine,
        "bm25": hit.bm25,
        // Which list put this hit where it is, and how far down. `null` means that half did not run — no vectors, or
        // `--mode keyword` — which is a different fact from "rank 0".
        "semantic_rank": hit.semantic_rank,
        "keyword_rank": hit.keyword_rank,
        "kind": hit.kind.label(),
        "path": hit.path,
        // A matched phrase from the note body with the hit marked, or `null` on a vector-only hit: keyword evidence is a
        // property of the keyword half, not of a kind of note.
        "snippet": hit.snippet.clone(),
        "linked_from": hit.linked_from,
        "source_note": match &hit.source_note {
            Some(note) => json!(note),
            None => Value::Null,
        },
        "linked_notes": hit.linked_notes,
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
    /// What this run **transferred from an origin** before it could embed (**F38**), or `None` when nothing arrived.
    ///
    /// Produced where the fetch happens — provider resolution — and copied onto the report by [`embed_workspace`] for the
    /// same reason `artifact_check` and `workers` are: the fact is decided next to the config and the filesystem, while the
    /// document that must not contradict itself is written downstream.
    fetched: Option<np_core::embed_run::FetchedRecord>,
    /// **F45:** the artifact is missing, this build *could* have fetched it (pinned id, `local-model` and `fetch` compiled in),
    /// and policy said no — `[models] auto_fetch = false` or `NP_NO_AUTO_FETCH=1`.
    ///
    /// Carried as a fact rather than re-derived by each caller because "could this build have downloaded it" is one rule with two
    /// callers: `np embed` refuses (it has nothing to answer with), `np search --json` degrades and must say *why* it degraded.
    /// A second copy of the predicate in the search path would eventually disagree with the first, and the reader would see an
    /// `artifact-missing` note about a workspace where somebody switched fetching off.
    fetch_declined: bool,
}

/// What `[embeddings]` and the `models` table added up to, as one value.
///
/// Four states, which is how many answers exist: a provider this run can embed with, a provider-shaped placeholder for a
/// state that refuses before a vector is asked for, a filesystem fact no downstream code can re-derive, and — since
/// **F38** — a fetch that was permitted, attempted, and met no connection. The last two look alike from the filesystem
/// (no artifact on disk) and must not share a sentence: one names a file to install, the other names a cable to plug in.
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
    /// A fetch was permitted, attempted, and **nothing answered** (**F38**). Not a filesystem fact and not a
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

/// Attach the **F45** "a fetch was possible and policy refused it" fact to a resolution that did not produce a provider.
///
/// Gated on the state on purpose: with a provider in hand nothing was declined, and letting a caller stamp `true` beside a
/// `Ready` would be a document contradicting itself. Applied at each non-`Ready` return inside the local-model branch, which is
/// where the facts behind it (row, artifact, pin) are all in scope.
fn declined(mut resolved: Resolved, fetch_declined: bool) -> Resolved {
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

/// The **F38** resolution for "we asked and nothing answered": no provider, and the offline arm as the answer.
#[cfg(all(feature = "local-model", feature = "fetch"))]
fn fetch_offline(id: &str) -> Resolved {
    Resolved {
        state: ProviderState::FetchSkippedOffline { id: id.to_string() },
        artifact_check: None,
        workers: None,
        // Nothing arrived, so nothing is claimed. `EmbedReport::offline` is set downstream from the arm itself, which is
        // why this field stays `None` rather than carrying a half-populated record.
        fetched: None,
        // Not declined: it was *attempted*. The distinction is F45's whole point — "we asked and nothing answered" and
        // "we were not allowed to ask" are different notes with different next commands.
        fetch_declined: false,
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
/// Resolve `[embeddings]` into something that can embed — or into the one refusal that explains why nothing can.
///
/// `auto_fetch` is **F38's trigger**, and it is a parameter rather than a global so that each call site states its
/// permission: `np embed` passes the operator's decision, `np search` passes `false` unconditionally. Even when true, a
/// fetch needs three more things — an id this build **pins**, a build that can run inference, and the compile-time
/// `fetch` feature — so no configuration value alone can make a binary dial out.
/// `announce` is text mode only: `false` under `--json`, where stdout holds exactly one document and a progress line
/// would corrupt it. **F38**'s notice is therefore suppressed rather than smuggled onto stderr, keeping that promise the
/// simple thing it is — Task 4 replaces the line with a `fetched` key, which is what a program should read anyway.
fn embed_provider(
    cfg: &Config,
    workspace: &Path,
    auto_fetch: bool,
    announce: bool,
) -> Result<Resolved, Box<dyn Error>> {
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
            let models_dir = cfg.workspace_models_dir(workspace);
            let artifact = models_dir.join(id);
            // `mut` only where the trigger is compiled in: a fetch records the `models` row, so that arm re-reads it
            // below. Everywhere else the binding is read-only from birth, and the warning would be true of that build.
            #[cfg_attr(
                not(all(feature = "local-model", feature = "fetch")),
                allow(unused_mut)
            )]
            let mut row = model_row(&db, id)?;

            // ---- **F38's trigger: fetch the pinned artifact this workspace is missing.** One place, before any provider
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
            //                              read. F40 gave this state a name; here it is a compile-time gate;
            //   * `feature = "fetch"`      — a build that compiled the HTTP client out cannot advertise a dial-out.
            //
            // Nothing here prints in `--json` mode (`announce`), and nothing here ever names a host (§12 rule 8): the only
            // origin fact allowed out is *which kind* of origin was resolved.
            #[cfg(all(feature = "local-model", feature = "fetch"))]
            let mut fetched_this_run: Option<np_core::embed_run::FetchedRecord> = None;
            #[cfg(all(feature = "local-model", feature = "fetch"))]
            let attempted_fetch = auto_fetch
                && np_core::pins::identity(id).is_some()
                && (row.is_none() || !artifact.is_file());
            #[cfg(all(feature = "local-model", feature = "fetch"))]
            if attempted_fetch {
                match fetch_pinned_missing(cfg, &db, &models_dir, id, announce)? {
                    FetchStep::Offline => return Ok(fetch_offline(id)),
                    FetchStep::Fetched(record) => {
                        fetched_this_run = Some(record);
                        // The fetch records the `models` row itself, so a workspace that had neither row nor file is now a
                        // workspace with both — re-read rather than assume, and let F32's arm still answer if it somehow
                        // did not.
                        row = model_row(&db, id)?;
                    }
                }
            }
            #[cfg(not(all(feature = "local-model", feature = "fetch")))]
            let fetched_this_run: Option<np_core::embed_run::FetchedRecord> = None;
            #[cfg(not(all(feature = "local-model", feature = "fetch")))]
            let _ = (announce, &models_dir, &artifact);

            // **F45:** the same facts the trigger above tests, with the operator's veto flipped — read BEFORE any fetch could have
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
                // F32's arm: this is the state that used to print a sentence about `provider = "none"`.
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
                // Also defensive: `--dim` bounds what can be installed (F32 §1). Never a declined fetch — the row is here and its
                // bytes are not in question, so stamping that fact would attach F45's note to the wrong state.
                return Ok(declined(
                    placeholder(NoProvider::RowDimUnusable {
                        id: id.to_string(),
                        declared: row.dim.to_string(),
                    }),
                    false,
                ));
            };

            // With the runtime compiled in, the provider is the one that loads llama.cpp **on first use**
            // (`local_model::LazyLocalEmbedder`, §14): resolving a provider happens before anyone knows whether
            // the workspace owes a chunk, and an eager load would spend ~420 MiB of resident weights plus load time
            // on a run whose answer is "nothing changed". The two refusals that need no model — a row width that
            // is not this run's, and an artifact that is not a file — are still taken here, so §6's *artifact
            // absent* arm stays arm (iii) instead of arriving late as a failure.
            #[cfg(feature = "local-model")]
            {
                // F33: decide how loud llama.cpp may be on this run's stderr, once, before anything can load. The rules — the five names,
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
                        // **F28: a build with the image code asks this workspace for a projector.** Which bytes qualify is
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
                // about its bytes to make — and printing one would be the false report §7 refuses.
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
    let cfg = Config::load(None).map_err(|err| format!("configuration error: {err}"))?;
    // **F37's one-release note, on stderr.** Stdout stays exactly one document when `--json` is asked for — the fact this
    // line carries is about *where* the run is reading from, not about the run's result, and a reader who needs it is a
    // human standing at the machine. Nothing here is actionable: that is the point of the fallback, and saying "moved"
    // would imply a move happened, which this release deliberately does not do.
    if let Some(legacy) = &cfg.legacy_data_dir {
        let _ = writeln!(io::stderr(), "np: {}", legacy_data_dir_note(legacy));
    }
    Ok(cfg)
}

/// The sentence [`load_config`] prints when **F37**'s fallback found an index at the pre-nesting Windows location.
///
/// A function rather than an inline literal for one reason: the fallback itself cannot happen on this platform or on CI
/// (Unix has no pre-nesting shape to fall back from), so the *words* are the only part of this path that Linux can check at
/// all. What they must do is name the directory, say no action is needed, and stay out of the report — asserted in this
/// module's tests, while the resolution that decides whether they print is asserted in
/// `crates/np-core/tests/config.rs` through [`np_core::config::DataDirPlatform`].
fn legacy_data_dir_note(legacy: &Path) -> String {
    format!(
        "reading your index from {} — where it was written before this release nested its data folder. \
         nothing to do: it stays there until a later release moves it",
        legacy.display()
    )
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
/// The one sentence that says **which kind** of origin this run would dial (**F38**, §12 rule 8).
///
/// Gated on `fetch` because both doors that print it — `np models fetch` and F38's trigger — are gated on the same feature: a build with
/// no HTTP client has no origin to describe, and an ungated helper there is a dead-function warning in the default CI row for
/// `--no-default-features`.
///
/// A *kind*, never a host: `np models fetch`'s existing line and this one share the wording so they cannot drift into
/// disagreeing about the same config state. The distinction it draws is real and cheap to lose — `[models] base_url`
/// unset means the origin baked into the pin table, while an empty `NP_MODELS_BASE_URL=` counts as unset too, so "no
/// configuration" and "configuration lost to a shell" look identical from here and both spend someone's egress.
#[cfg(feature = "fetch")]
fn origin_kind_line(cfg: &Config) -> &'static str {
    if cfg.models.base_url.is_none() {
        "origin      this build's compiled-in default — no [models] base_url is configured (an empty \
         NP_MODELS_BASE_URL counts as unset)"
    } else {
        "origin      from [models] base_url"
    }
}

/// What one auto-fetch attempt produced (**F38**).
#[cfg(all(feature = "local-model", feature = "fetch"))]
enum FetchStep {
    /// Bytes arrived and were published by digest. Carries the record `--json` will print.
    Fetched(np_core::embed_run::FetchedRecord),
    /// Nothing was reached. Not an error: a state, with its own report arm downstream (**F40's** lesson, one arm over).
    Offline,
}

/// Fetch the pinned id's members that **this build can use**, because this workspace is missing them (**F38**).
///
/// Called only from the trigger in [`embed_provider`], which has already decided a fetch is permitted; this function's own
/// job is to be honest about the transfer and to classify its failure rather than flatten it.
///
/// * **Before byte one**, in text mode, it says what is about to happen: id, the members this build would load, their
///   combined size, and the origin's *kind*. `np models fetch` prints the same pair of lines for the same reason — a
///   silent 310 MB transfer is not a side effect anyone should discover afterwards.
/// * **Offline is its own answer** (`FetchError::is_offline`, F38 §5): no probe was run, and none will be — this call *is*
///   the probe, made only where the artifact was already missing, so a machine that holds its model pays nothing. The
///   sentence says what could not be reached, that nothing was embedded, and what to re-run; it does not dress a missing
///   network up as a broken vault, which is F40's lesson applied one arm over.
/// * **Anything the origin actually answered** — 404, 403, a redirect, wrong bytes — propagates as `fetch`'s own refusal,
///   unchanged. Those are not outages and must never be skipped past: an unreachable mirror behind "offline" is how a
///   broken install becomes permanent.
#[cfg(all(feature = "local-model", feature = "fetch"))]
fn fetch_pinned_missing(
    cfg: &Config,
    db: &Db,
    models_dir: &Path,
    id: &str,
    announce: bool,
) -> Result<FetchStep, Box<dyn Error>> {
    let members = np_core::pins::members_for_build();
    let bytes: u64 = members
        .iter()
        .filter_map(|name| np_core::pins::pinned_member(id, name))
        .map(|pin| pin.size_bytes)
        .sum();

    if announce {
        emit(&format!(
            "fetching     {id} ({}, {}) — this workspace has no artifact to embed with",
            members.join(", "),
            size_human(bytes),
        ));
        emit(origin_kind_line(cfg));
    }

    let origin = fetch::resolve_origin(cfg.models.base_url.as_deref());
    let transport = fetch::HttpTransport::new(origin);
    let requested: Vec<&str> = members.to_vec();
    match fetch::fetch(db, models_dir, id, &requested, &transport, now_secs()) {
        Ok(report) => {
            if announce {
                for member in &report.members {
                    emit(&format!(
                        "{FETCHED_KEY}{} ({}, sha256 {})",
                        member.member,
                        size_human(member.bytes),
                        member.sha256
                    ));
                }
                emit(&free_space_line(report.free_bytes));
            }
            Ok(FetchStep::Fetched(np_core::embed_run::FetchedRecord {
                id: report.id.clone(),
                // `fetch` publishes a member only after its digest matches the pin (§12 rule 6), so every member that
                // arrives here is verified — the flag is a promise the reader can check against the type's doc, not a
                // claim this call site makes up.
                members: report
                    .members
                    .iter()
                    .map(|member| np_core::embed_run::FetchedMember {
                        name: member.member.clone(),
                        bytes: member.bytes,
                        digest_verified: true,
                    })
                    .collect(),
                // A *kind*, never a host (§12 rule 8): the same distinction `origin_kind_line` prints for humans.
                origin: origin_kind_token(cfg),
            }))
        }
        // **Offline is a state, so it returns one.** It used to come back as an error string from here, which meant `--json`
        // had no document to carry the fact and a caller had to read prose to learn that nothing was downloaded.
        Err(err) if err.is_offline() => Ok(FetchStep::Offline),
        Err(err) => Err(err.into()),
    }
}

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
    // **F44**, and it comes before the discard below rather than after it. `--discard-marker` is documented as an action that does not depend on
    // the rest of the run, and that stays true of its *failures*; what it must not do is delete a marker while another process is mid-fetch in
    // this same workspace — which is exactly the situation the lock detects, and exactly what §12 rule 4's marker exists to make visible. A
    // refusal here therefore leaves the marker where it is, for the other run to finish with.
    let _writer = writer_lock(&config, workspace, "models fetch")
        .map_err(|failure| failure.report_cli(json))?;
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
        emit(origin_kind_line(&config));
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
    // An install writes a row and copies an artifact into the per-workspace model directory. Neither is in the transaction that `scan` would
    // open, so a concurrent index can observe a half-installed model — which is why this command is inside the same lock as everything else
    // that changes state (**F44**), not merely next to it.
    let _writer = writer_lock(&config, workspace, "models install")
        .map_err(|failure| failure.report_cli(json))?;
    let db = workspace_db(&config, workspace)?;
    let outcome = models::install(&db, &config, workspace, request, now_secs())?;
    if json {
        // `state` is the key §3 pins and its three strings come from
        // [`np_core::models::InstallOutcome::state_label`], not from a format string here. `id` rides along
        // because a JSON reader of a redirected run has no argv to look at, and the same two keys are present
        // for all three outcomes — one shape per command, as §3 requires.
        let drift = prefix_drift_notice(&config, workspace, &request.id)?;
        let doc = insert_drift(
            json!({ "id": request.id, "state": outcome.state_label() }),
            drift,
        )?;
        emit_json(&doc);
    } else {
        // The notice prints on stderr either way, so the text channel gets it too — a person piping stdout to a file still sees the line.
        let drift = prefix_drift_notice(&config, workspace, &request.id)?;
        let _ = drift;
        emit(outcome.state_label());
    }
    Ok(())
}

/// `np models install --member …` — one member of an already-installed model, installed from a file with no network.
///
/// Three refusals come before anything is opened, because each is a fact about argv that a 554 MB read would not teach:
/// an invented member name, `--dim` on a member that has no width, and the main member's own missing width. The last one
/// is reachable only if clap's `required_unless_present` stops working; it is a refusal rather than an `expect` so that a
/// future flag change turns into a message instead of a panic.
///
/// The prefixes travel to the main path and are refused for `mmproj` by omission: they are fields of `models`, and a
/// projector install writes no row there.
/// What `np models install` was given, before anyone decided which member those flags describe. One carrier because the
/// two paths read overlapping halves of it, and passing nine positional arguments turned a lint into a design question:
/// the width is `None` exactly when the human named a member that has no width, so the pair belongs together.
struct InstallArgs {
    workspace: PathBuf,
    id: String,
    source: PathBuf,
    /// `None` for a member install — which is why it is not a `NonZeroU16` here even though [`Install`] insists on one.
    dim: Option<NonZeroU16>,
    sha256: String,
    prefix_doc: String,
    prefix_query: String,
    json: bool,
}

/// The default shape: `--member` absent, or spelled `main`. clap requires the width on this path; the guard below answers
/// only if that requirement is ever loosened, and answers with a message rather than a panic.
fn install_main(args: &InstallArgs) -> Result<(), Box<dyn Error>> {
    let Some(dim) = args.dim else {
        return Err(
            "`--dim` is required when installing a model: it declares the vector width".into(),
        );
    };
    models_install(
        &args.workspace,
        &Install {
            id: args.id.clone(),
            source: args.source.clone(),
            dim,
            sha256: args.sha256.clone(),
            prefix_doc: args.prefix_doc.clone(),
            prefix_query: args.prefix_query.clone(),
        },
        args.json,
    )
}

fn models_install_member(args: &InstallArgs, member: &str) -> Result<(), Box<dyn Error>> {
    if !models::INSTALLABLE_MEMBERS.contains(&member) {
        return Err(format!(
            "unknown member `{member}`: this command installs {} and records nothing else",
            models::INSTALLABLE_MEMBERS.join(" or ")
        )
        .into());
    }
    if member == models::MAIN_FILE {
        return install_main(args);
    }
    if args.dim.is_some() {
        return Err(format!(
            "`--dim` describes the width of a model's text encoder; the `{member}` member carries no width — drop the flag"
        )
        .into());
    }
    let config = load_config()?;
    let db = workspace_db(&config, &args.workspace)?;
    let outcome = models::install_projector(
        &db,
        &config,
        &args.workspace,
        &models::ProjectorInstall {
            id: args.id.clone(),
            source: args.source.clone(),
            sha256: args.sha256.clone(),
        },
    )?;
    if args.json {
        // Three keys where the model install has two, and deliberately so: `state` alone cannot say *which* artifact this
        // run placed, and a JSON reader of a redirected run has no argv to look back at. The main member's pinned
        // `{id, state}` shape is untouched — adding a key to that one would be the breaking change.
        emit_json(&json!({ "id": args.id, "member": member, "state": outcome.state_label() }));
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

/// One row under `--json`, with exactly the eight keys §3 pins — and the full digest, which the text column
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
        // Every file this identity has besides its main artifact — present/absent by stat, never hashed here. A model
        // with only its main artifact answers `[]`, which is a fact and not an omission (**F28** exposed the gap:
        // confirming an offline projector install used to mean watching `np embed` stop reporting `projector-absent`).
        "members": model
            .members
            .iter()
            .map(|member| {
                json!({
                    "name": member.name,
                    "file": member.file,
                    "present": member.present,
                })
            })
            .collect::<Vec<Value>>(),
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
        // `files` appears only when there are members to name. A line that always carried it would put
        // `files main` — which duplicates this row's own `state` verdict — beside every model, or `files -`,
        // which is a new token for "nothing"; either way the common line changes shape for no information.
        let files = if model.members.is_empty() {
            String::new()
        } else {
            format!(
                "  files {}",
                model
                    .members
                    .iter()
                    .map(|member| format!(
                        "{}:{}",
                        member.name,
                        if member.present { "present" } else { "absent" }
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        };
        let _ = writeln!(
                out,
                "{:<width$}  {}  dim {}  sha256 {}  installed_at {}  prefix_doc \"{}\"  prefix_query \"{}\"{}",
                model.row.id,
                model.state.state_label(),
                model.row.dim,
                text_digest(&model.row.sha256),
                model.row.installed_at,
                model.row.prefix_doc,
                model.row.prefix_query,
                files,
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

    /// **F37**: the fallback note is the only part of that path a Linux machine can execute, so it is tested here — while the
    /// decision to print it is tested in np-core, where both platform shapes resolve. Together they cover the sentence and
    /// the condition; neither alone would be evidence about the other.
    #[test]
    fn the_fallback_note_names_the_index_and_asks_for_nothing() {
        let note = legacy_data_dir_note(std::path::Path::new(r"C:\Users\ana\AppData\Local"));
        assert!(
            note.contains(r"C:\Users\ana\AppData\Local"),
            "it has to name the directory it is reading: {note}"
        );
        assert!(
            note.contains("nothing to do"),
            "a person reading this must not go looking for a fix: {note}"
        );
        assert!(
            !note.contains("moved"),
            "this release reads where the index is; it does not claim to have moved anything: {note}"
        );
    }

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
