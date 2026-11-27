//! `np` — command line entry point for note-processor.
//!
//! Three command groups: `np scan`, `np embed`, and the M2a model commands under `np models`. All are parsers
//! and printers only — every decision about what an install means, which id is legal, or what state an artifact
//! is in lives in [`np_core::models`], so this file holds no SQL and defines no second SHA-256
//! ([`np_core::hash::sha256_file`] is the one digest in the program, and `np models hash` calls it).

// **Same rule as `np-core`: unsafe is documented or it does not compile.** The blocks here are all libc calls behind the
// diagnostics pipe (`diag.rs`) — small, but they are the reason a log line cannot make `np` hang, and "small" is not a
// safety argument. Denied at crate level for every feature combination, with no `allow` in the tree.
#![deny(unsafe_op_in_unsafe_fn, clippy::undocumented_unsafe_blocks)]

mod commands;
mod diag;
mod serve;
mod serve_channel;

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use np_core::search;

use crate::commands::embeddings::EmbeddingsCommand;
use crate::commands::models::{InstallArgs, ModelsCommand};
use crate::commands::search::SearchModeFlag;

/// The default top-k for `np search --limit`.
const DEFAULT_SEARCH_LIMIT: usize = 10;

#[derive(Parser)]
// The binary is `np`, but the package is `np-cli`; without `name` clap would print the package name
// in `--version` and `--help`.
#[command(name = "np", version, about)]
struct Cli {
    /// How much to say about what the command did, on stderr: `-v` info, `-vv` debug, `-vvv` trace.
    ///
    /// Asking is what turns stderr printing on at all — an ordinary `np scan` prints exactly what it printed before these flags
    /// existed (design point 1), and the first line of an asked run names the effective level *and* where it came from, so a
    /// `NP_LOG` that the config file outranks is visible rather than mysterious. Global so it reads naturally in either
    /// position (`np -v scan …`, `np scan … -v`).
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count, global = true)]
    verbose: u8,
    /// Per-target levels instead of one dial for everything: `--log-spec info,np_core::search=trace`.
    ///
    /// Levels are `off`, `error`, `warn`, `info`, `debug`, `trace`; targets are the ten in `docs/reference-diagnostics.md`, and
    /// a name this build does not emit is accepted rather than refused. A bad value refuses naming this flag, as `NP_LOG`
    /// refuses naming itself.
    #[arg(long = "log-spec", value_name = "SPEC", global = true)]
    log_spec: Option<String>,
    /// Also write every captured record to this file as JSONL, one object per line.
    ///
    /// The flag rather than only `[log] file`, because the config file is the thing most likely to be broken in the run you are
    /// debugging — and a file that cannot load cannot configure its own sink. Human text stays on stderr; JSON goes
    /// here and nowhere else.
    #[arg(long = "log-file", value_name = "PATH", global = true)]
    log_file: Option<PathBuf>,

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
    /// **This command may fetch**. When `[embeddings] model` names an id this build pins and the artifact is
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
    /// workspace's key ([`np_core::config::Config::workspace_db`]), so a command without one could not find a database to read.
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
        /// Match every word at the **start** of a word instead of as a whole one: `grand` also reaches `grandma` and
        /// `grandmother`. Off by default — keyword search matches whole tokens, because this vault holds identifiers — and
        /// one marked word needs no flag here: `"grand*"` asks for word beginnings on its own (**#6**). This widens *every*
        /// word in the query, which is what a "match word beginnings" toggle in a front end means.
        #[arg(long)]
        prefix: bool,
        /// Reorder the top hits with a cross-encoder, overriding `[search] rerank`. Needs the pinned reranker installed
        /// (`np models install --id bge-reranker-v2-m3`), and **never fetches it**: this command will not volunteer 635 MB for a
        /// ranking preference, even where `[models] auto_fetch = true`. Asking with this flag and not having it refuses at exit 1;
        /// the config key alone answers un-reranked with one line on stderr (**F51**).
        #[arg(long, overrides_with = "no_rerank")]
        rerank: bool,
        /// Do not rerank, overriding `[search] rerank = true`. Exists so one query can be measured against the same vault with
        /// the stage off — the difference is what a person is trying to see, and it cannot be seen from one run.
        #[arg(long)]
        no_rerank: bool,
        /// Which text of a note the reranker reads: `title-body` (the default) scores the title, a newline and the body; `body`
        /// scores the body alone. Overrides `[search] rerank_text`; any other spelling is refused naming both (**F51**).
        #[arg(long, value_parser = commands::search::parse_rerank_text)]
        rerank_text: Option<np_core::search::RerankText>,
        /// Print the hits as JSON instead of one line each.
        #[arg(long)]
        json: bool,
    },
    /// Answer JSON-RPC 2.0 requests on stdin/stdout until stdin closes — the backend surface for an app.
    ///
    /// One process, no port, no daemon: stdout carries exactly one JSON object per line and nothing else, every human sentence goes to stderr, and
    /// each method's `result` is the same document `np <verb> --json` prints. A workspace is named per request rather than remembered, so one
    /// server can serve several vaults and a stale session cannot answer for a directory you moved.
    Serve,
    /// Index a workspace, fetch its pinned model if this build can, embed it — one command that ends with vectors or says
    /// exactly where it stopped.
    ///
    /// Built for an application that wants vectors to exist and does not want to sequence three commands and interpret
    /// three exit codes. The order is deliberate: **scan first**, because keyword search works the moment indexing
    /// finishes and a download must not stand between a person and their first results; fetch and embed second.
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
    /// Turn embeddings on or off in np's config file — the door a consent answer needs (**#7**).
    ///
    /// An application that asked "may np download the embedding model?" and was told yes had nothing to do with the answer:
    /// only a person editing `config.toml` could set `embeddings.provider`. These two commands are that action, with np's own
    /// validation on the way in and a provenance comment in the file afterwards.
    Embeddings {
        #[command(subcommand)]
        command: EmbeddingsCommand,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // **Phase one of the diagnostic sinks** (`diag`'s module header says why there are two): levels from argv and the
    // environment, and `--log-file` opened, before anything reads a config file. The order is the point — the most common
    // integrator failure is a `config.toml` with one section misspelled, and it must not become the one failure with no
    // diagnostic. A value that cannot be honoured refuses here, as every other unreadable argument does: one sentence, exit 1.
    let verbose = cli.verbose;
    let log_spec = cli.log_spec.clone();
    let log_file = cli.log_file.clone();
    if let Err(err) = diag::bootstrap(verbose, log_spec.as_deref(), log_file.as_deref()) {
        // Written through a handle with the result dropped for the reason given below, at the exit that needs it most.
        let _ = writeln!(io::stderr(), "np: {err}");
        return ExitCode::FAILURE;
    }
    let command_name = match &cli.command {
        Command::Scan { .. } => "scan",
        Command::Embed { .. } => "embed",
        Command::Setup { .. } => "setup",
        Command::Serve => "serve",
        Command::Search { .. } => "search",
        Command::Embeddings { .. } => "embeddings",
        Command::Models { .. } => "models",
    };
    let rule_records_before = diag::rule_records();
    let result = match cli.command {
        Command::Scan { workspace, json } => commands::scan::scan_workspace(&workspace, json),
        Command::Embed {
            workspace,
            json,
            auto_fetch,
            no_auto_fetch,
        } => commands::embed::embed_workspace(
            &workspace,
            json,
            commands::embed::AutoFetch::from_flags(auto_fetch, no_auto_fetch),
        ),
        Command::Setup {
            workspace,
            json,
            auto_fetch,
            no_auto_fetch,
            yes,
        } => commands::setup::setup_workspace(
            &workspace,
            json,
            commands::embed::AutoFetch::from_flags(auto_fetch, no_auto_fetch),
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
            prefix,
            rerank,
            no_rerank,
            rerank_text,
            json,
        } => commands::search::search_workspace(
            &workspace,
            &query,
            limit,
            mode.map(commands::search::SearchModeFlag::to_core),
            if prefix {
                search::Prefix::EveryTerm
            } else {
                search::Prefix::Marked
            },
            // Two flags, one option: neither named means "say nothing", so `[search] rerank` decides; `--rerank` and
            // `--no-rerank` are the two directions of saying something. clap's `overrides_with` lets a later flag undo an
            // earlier one on the same line, which is why this cannot see both as set.
            if rerank {
                Some(true)
            } else if no_rerank {
                Some(false)
            } else {
                None
            },
            rerank_text,
            json,
        ),
        Command::Embeddings { command } => match command {
            EmbeddingsCommand::Enable { id, json } => commands::embeddings::embeddings_run(
                np_core::config_write::EmbeddingsTarget::On { model: &id },
                json,
            ),
            EmbeddingsCommand::Disable { json } => commands::embeddings::embeddings_run(
                np_core::config_write::EmbeddingsTarget::Off,
                json,
            ),
        },
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
                // **A pinned reranker takes a different route**, decided by the pin table rather than by a flag nobody
                // would have to remember: it declares no width, gets no row, and refuses the three flags that describe
                // stored vectors. An explicit `--member` is left to the arm below, which refuses it honestly
                // (`MemberWithoutModel`) rather than pretending a reranker has members.
                match member {
                    None if np_core::pins::role(&id) == Some(np_core::pins::Role::Rerank) => {
                        commands::models::install_reranker(&InstallArgs {
                            workspace,
                            id,
                            source: path,
                            dim,
                            sha256,
                            prefix_doc,
                            prefix_query,
                            json,
                        })
                    }
                    None => commands::models::install_main(&InstallArgs {
                        workspace,
                        id,
                        source: path,
                        dim,
                        sha256,
                        prefix_doc,
                        prefix_query,
                        json,
                    }),
                    Some(member) => commands::models::models_install_member(
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
            } => commands::models::models_fetch(&workspace, &id, &members, discard_marker, json),
            ModelsCommand::Hash { file } => commands::models::models_hash(&file),
            ModelsCommand::List { workspace, json } => {
                commands::models::models_list(&workspace, json)
            }
        },
    };
    // The outcome record, written while the sinks are still open: the `np: …` line below goes to stderr only, so a failure
    // whose code logged no rule record of its own would otherwise leave the `[log] file` with nothing to say about it.
    if let Err(err) = &result {
        commands::common::log_command_failure(
            command_name,
            err.as_ref(),
            diag::rule_records() != rule_records_before,
        );
    }
    // What stderr swallowed, said once at the end because the notice owes a total. Before the exit paths, so a run that
    // failed *and* was truncated reports both — the truncation is often the reason nobody saw the failure.
    diag::finish();
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
