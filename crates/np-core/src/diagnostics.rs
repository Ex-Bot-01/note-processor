//! The published diagnostics vocabulary: the targets an event can name and the rule tokens it can report.
//!
//! Design: `docs/superpowers/specs/2026-12-10-np-diagnostics-design.md` §1 (contract), §4 (levels), §12 (privacy).
//! This module holds **names only** — no formatting, no sink, no subscriber. Choosing a subscriber and writing to
//! stderr or a file is np-cli's job ([`np_cli::diag`](../../np_cli/diag/index.html)); np-core emits events against the
//! names here and nothing else, which is what lets a library embed in a process that has its own logging.
//!
//! # Why the list lives in code
//!
//! A rule token is an interface: an integrator's alerting rule matches on it. Interfaces rot when their list lives in a
//! document nobody edits, so `RULE_TOKENS` and `TARGETS` are what the documentation test compares the manual against
//! (`crates/np-cli/tests/diag.rs`, plan Task 6 Step 2) and what every emitted token is checked to belong to here. Adding
//! an event without adding its token therefore fails a test, in this crate or the one that documents it.
//!
//! # What a rule token is *not*
//!
//! It is not a new state. Every token here names a refusal or a degraded state `np` already answers with a sentence —
//! `configuration disabled`, `offline`, `locked` — and the token is that same fact, spelled once, for the reader that
//! cannot parse prose. Where a token differs from the human label it is because two labels describe one state
//! (`np search` says "no vectors are stored", `np embed` says "this workspace has no vectors"; both report
//! `no-vectors`) or because the label was three words long and a machine does not need its grammar.

/// The ten targets, in the order the design lists them: one per module whose failures a reader needs to tell apart.
///
/// A target is set with `tracing`'s `target:` field at the emission site. Filtering by target is how an operator says
/// "only the fetcher" (`NP_LOG=fetch=debug`, plan Task 5), so the ten names are stable strings, not a module path
/// derived from `module_path!()`: renaming a module must not silently rename a published filter.
pub const TARGETS: [&str; 10] = [
    target::CONFIG,
    target::SCAN,
    target::EMBED,
    target::SEARCH,
    target::MODELS,
    target::FETCH,
    target::LOCK,
    target::SERVE,
    target::SERVE_CHANNEL,
    target::CLI,
];

/// The published target names.
pub mod target {
    /// Configuration loading and refusals: a file that was not read, a key that was refused.
    pub const CONFIG: &str = "np_core::config";
    /// Indexing: what was walked, what could be read, what the catalog refused.
    pub const SCAN: &str = "np_core::scan";
    /// The embedding pipeline: provider resolution, batching, writing vectors.
    pub const EMBED: &str = "np_core::embed";
    /// Ranking and retrieval: why a list came back empty or short.
    pub const SEARCH: &str = "np_core::search";
    /// Model rows and artifacts: what is installed, what is on disk, what disagrees.
    pub const MODELS: &str = "np_core::models";
    /// Transport: what was asked for, what the network said.
    pub const FETCH: &str = "np_core::fetch";
    /// Locks and busy timeouts, whichever command took them.
    pub const LOCK: &str = "np_core::lock";
    /// Request lifecycle in `np serve`.
    pub const SERVE: &str = "np_cli::serve";
    /// One connection's read/write lifecycle in `np serve`.
    pub const SERVE_CHANNEL: &str = "np_cli::serve_channel";
    /// Argument parsing, sink bootstrap, the announce line.
    pub const CLI: &str = "np_cli";
}

/// The rule tokens: one per refusal or degraded state that has a name.
///
/// Sites reference these constants rather than repeating literals, so a token cannot be emitted by one call site and
/// documented under another spelling.
pub mod rule {
    /// A `config.toml` was refused; the command may still serve from defaults (**F35**).
    pub const CONFIG_REFUSED: &str = "config-refused";
    /// Embeddings are switched off — `embeddings.provider = "none"`, or a key that names no provider.
    pub const PROVIDER_DISABLED: &str = "provider-disabled";
    /// This binary was built without the local inference runtime, so the configured model cannot run here.
    pub const LOCAL_MODEL_NOT_BUILT: &str = "local-model-not-built";
    /// A row names an artifact the disk does not have.
    pub const ARTIFACT_MISSING: &str = "artifact-missing";
    /// A transfer was allowed and attempted, and the transport said no connection (**F38**).
    pub const OFFLINE: &str = "offline";
    /// Work was asked for and a run finished without producing it — arm (iv), the failure half of `np embed`.
    pub const NOT_PRODUCED: &str = "not-produced";
    /// A sequence went over `embeddings.max_tokens` and was refused rather than truncated.
    pub const TOKEN_BUDGET: &str = "token-budget";
    /// An installed row's prefix no longer matches the pinned prefix, so its vectors are stale by construction.
    pub const PREFIX_DRIFT: &str = "prefix-drift";
    /// Another writer holds this workspace and the wait expired (F44).
    pub const LOCKED: &str = "locked";
    /// No vectors are stored under the configured model, so the semantic half has nothing to rank.
    pub const NO_VECTORS: &str = "no-vectors";
    /// `[embeddings] model` names an id this workspace has no row for.
    pub const MODEL_NOT_INSTALLED: &str = "model-not-installed";
    /// Fetching was switched off (`[models] auto_fetch = false`, `NP_NO_AUTO_FETCH=1`, or `--no-auto-fetch`), so a
    /// missing artifact stayed missing.
    pub const FETCH_DISABLED: &str = "fetch-disabled";
    /// An artifact arrived during this command. Not a degradation, so it is reported at info; it is in the list because
    /// a reader that sees vectors where it saw none last run needs to know which run made them.
    pub const ARTIFACT_FETCHED: &str = "artifact-fetched";

    // ---- scan: one token per reason a file on disk was not indexed (**design §1 point 4**, plan Task 4).
    //
    // `ScanReport::skipped` merges several of these into one list of paths, and two of them (`not-markdown`,
    // `unchanged-content`) were never in it at all: they are not failures, so prose had no reason to name them. The
    // tokens exist because "which of my 400 files did not get indexed, and why" is a question with a per-file answer.

    /// A discovered file that is not markdown, so nothing about it is parsed into the catalog. Its `files` row is still
    /// current — an attachment is tracked, not skipped as a loss.
    pub const NOT_MARKDOWN: &str = "not-markdown";
    /// This scan could not read the file (permission denied, or it changed between the listing and the read). The rows
    /// it already had are kept, because unknown is not absent.
    pub const UNREADABLE: &str = "unreadable";
    /// A name this build's byte key cannot hold — on Windows, a name carrying an unpaired surrogate (F18). Refused for
    /// good, so re-running changes nothing; the fix is the filename.
    pub const UNREPRESENTABLE_PATH: &str = "unrepresentable-path";
    /// The bytes matched the stored digest, so nothing was re-parsed or rewritten. Reported at `trace`: on a warm vault
    /// this is the answer for nearly every file, and `debug` exists to be readable.
    pub const UNCHANGED_CONTENT: &str = "unchanged-content";
    /// A live note row sits under a directory this scan could not read, so it is hidden from the catalog rather than
    /// deleted. The directory is `unreadable`; this names the note it swallowed.
    pub const HIDDEN_BEHIND_UNREADABLE: &str = "hidden-behind-unreadable";
    /// A stored row whose path only has a lossy spelling (F23) and which no route could account for this scan, so it
    /// stayed out of the catalog. The remedy is on disk: make the file listable with a lossless name.
    pub const UNRESOLVED_AMBIGUITY: &str = "unresolved-ambiguity";
    /// Another path already holds this note identity (case-folded collision), so this file kept no new rows and its
    /// existing note was left exactly as it was.
    pub const IDENTITY_TAKEN: &str = "identity-taken";
    /// More than one note row sits on this path's key — the legacy duplicate F13 §7 refuses to guess about — so no note
    /// was re-parsed and nothing about its contents was rewritten.
    pub const DUPLICATE_ROWS: &str = "duplicate-rows";
}

/// Every published rule token.
///
/// The documentation test asserts this list against `docs/reference-diagnostics.md` in both directions, so a token can
/// be neither undocumented nor invented.
pub const RULE_TOKENS: [&str; 21] = [
    rule::CONFIG_REFUSED,
    rule::PROVIDER_DISABLED,
    rule::LOCAL_MODEL_NOT_BUILT,
    rule::ARTIFACT_MISSING,
    rule::OFFLINE,
    rule::NOT_PRODUCED,
    rule::TOKEN_BUDGET,
    rule::PREFIX_DRIFT,
    rule::LOCKED,
    rule::NO_VECTORS,
    rule::MODEL_NOT_INSTALLED,
    rule::FETCH_DISABLED,
    rule::ARTIFACT_FETCHED,
    rule::NOT_MARKDOWN,
    rule::UNREADABLE,
    rule::UNREPRESENTABLE_PATH,
    rule::UNCHANGED_CONTENT,
    rule::HIDDEN_BEHIND_UNREADABLE,
    rule::UNRESOLVED_AMBIGUITY,
    rule::IDENTITY_TAKEN,
    rule::DUPLICATE_ROWS,
];
