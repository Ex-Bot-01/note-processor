use crate::commands::common::emit;
use crate::commands::common::emit_json;
use crate::commands::common::load_config;
use crate::commands::common::now_secs;
use crate::commands::common::workspace_db;
use crate::commands::common::writer_lock;
#[cfg(feature = "fetch")]
use crate::commands::embed::free_space_line;
use crate::commands::embed::insert_drift;
use crate::commands::embed::prefix_drift_notice;
#[cfg(feature = "fetch")]
use crate::commands::embed::FETCHED_KEY;
#[cfg(feature = "fetch")]
use crate::commands::setup::origin_kind_line;
use clap::Subcommand;
use np_core::catalog::size_human;
use np_core::config::Config;
#[cfg(feature = "fetch")]
use np_core::fetch;
use np_core::hash::sha256_file;
use np_core::models;
use np_core::models::FetchEntry;
use np_core::models::FetchState;
use np_core::models::Install;
use np_core::models::ListedModel;
use np_core::models::Listing;
use np_core::models::ModelError;
use np_core::models::Unclaimed;
use np_core::models::UnclaimedKind;
use serde_json::{json, Value};
use std::error::Error;
use std::io;
use std::io::Write;
use std::num::NonZeroU16;
use std::path::Path;
use std::path::PathBuf;

/// The model commands, and which of them touch this workspace's database.
///
/// `<workspace>` is an operand of `install` and `list` rather than an option because both the `models` row
/// and the artifact live under one workspace's key ([`Config::workspace_db`], [`Config::workspace_models_dir`])
/// — a command without it could not resolve a database at all, and would "succeed" into a hashed path nobody
/// will ever scan. `hash` takes a file and touches no database.
#[derive(Subcommand)]
pub(crate) enum ModelsCommand {
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
        /// Declared vector width, 1 to 65535 — declared here, never probed. Refused outright on a `--member` install,
        /// because only the main artifact has a width.
        ///
        /// **Required by `np`, not by clap**, since the rerank stage: an embedding install without it is refused with
        /// this command's own sentence ([`install_main`]), and a pinned reranker is refused for carrying it — its bytes
        /// declare their own width, and there is no row to store a claimed one in. Moving the requirement from clap to
        /// code is what lets one subcommand serve both, and the refusal names the flag either way.
        #[arg(long, value_parser = parse_dim)]
        dim: Option<NonZeroU16>,
        /// Which artifact of the model this file is: `main` (the text encoder, the default) or `mmproj` (its projector).
        ///
        /// A member install writes no width and no prefixes — those describe the text encoder — and it requires the model
        /// to be installed already. It exists because a machine that cannot reach Hugging Face still has to be able to
        /// place the 554 MB projector that `np embed` then tells it it is missing.
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
    /// The **explicit** dial-out: it dials where `[models] base_url` says, or `NP_MODELS_BASE_URL` when the
    /// file leaves that key out, or the one origin baked into the pin table when neither speaks. You ran it, so it runs.
    ///
    /// It is no longer the *only* command that transfers bytes: `np embed` and `np setup` carry an automatic
    /// trigger for a missing **pinned** artifact, on by default. That trigger reuses this command's origin rules,
    /// pin, marker and digest verification rather than inventing a second
    /// transport, which is why the surface stayed one door wide at the wire level even though a second command may now
    /// knock. What still never fetches: `np scan`, `np search` and any id this build does not
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
        /// The explicit door for a claim that will not expire itself: a marker means some process claimed this
        /// download, and liveness is consent, so
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

/// Hex characters shown in the human-readable digest column: 12, with the full value under `--json`.
const DIGEST_TEXT_CHARS: usize = 12;

/// Parse `--dim`: a width from 1 to `u16::MAX`, refused while parsing rather than at the write.
///
/// `u16` because [`np_core::config::EmbeddingsConfig::dim`] already measures that quantity in `u16`; non-zero
/// because migration 7's `CHECK (length(vector) = 4 * dim)` admits a zero-byte vector when `dim` is 0, so a
/// dim-0 row would accept an embedding of nothing for every chunk. Both messages name the ceiling, which is
/// what a human who typed `153600` needs to read — and refusing here rather than in the schema is what keeps
/// the "no new migration" promise true.
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

/// Nothing is printed before [`models::install`] returns: the durable record comes
/// first and the echo second, so a refusal cannot leave a success line behind on stdout.
/// Fetch `id`'s pinned members into `workspace` and record the model.
///
/// **The origin is read, never printed.** What a report may say about where bytes came from is bounded on purpose, and a
/// success line lands in CI logs as readily as a refusal does — so the report says an origin was configured and names
/// the members, sizes and digests, which is everything a person can act on. The one thing that must not appear is the
/// URL, because a mirror's hostname is someone's infrastructure detail and this program has no business publishing it
/// in a build log. Since 2026-11-04 an origin exists on every run — `[models] base_url` and `NP_MODELS_BASE_URL` both
/// silent means [`np_core::pins::DEFAULT_ORIGIN`] — which makes *not* printing it the more important half of the rule.
#[cfg(feature = "fetch")]
pub(crate) fn models_fetch(
    workspace: &Path,
    id: &str,
    members: &[String],
    discard_marker: bool,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let config = load_config()?;
    // The lock, and it comes before the discard below rather than after it. `--discard-marker` is documented as an action that does not depend on
    // the rest of the run, and that stays true of its *failures*; what it must not do is delete a marker while another process is mid-fetch in
    // this same workspace — which is exactly the situation the lock detects, and exactly what the marker exists to make visible. A
    // refusal here therefore leaves the marker where it is, for the other run to finish with.
    let _writer = writer_lock(&config, workspace, "models fetch")
        .map_err(|failure| failure.report_cli(json))?;
    let db = workspace_db(&config, workspace)?;
    let models_dir = config.workspace_models_dir(workspace);

    // **The discard happens first.** `--discard-marker` is a filesystem action a human takes because they know no fetch
    // is running, so it may not depend on anything the rest of the run needs — not which
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

    // The origin, in the one order this crate settles on: `[models] base_url` as config left it
    // (`NP_MODELS_BASE_URL` fills that key only when the file leaves it unset — `np_core::config`'s rule for every key
    // in that section), then the origin baked into the pin table. A machine with no configuration therefore makes one
    // outbound request during installation instead of being refused; a mirror is still one config line away, and this
    // line stays true of every run: the resolved origin is read here and printed nowhere.
    // A default origin exists for every id this build can fetch, so `None` means the pin table and the origin table
    // disagree — a bug in this build rather than a state a user can be in. Refusing by id names it, and naming no host
    // is origin privacy applied to an error path.
    let Some(origin) = fetch::resolve_origin(id, config.models.base_url.as_deref()) else {
        return Err(format!(
            "`{id}` is pinned by digest but this build compiles in no origin for it; \
             set [models] base_url to where its files are hosted"
        )
        .into());
    };

    // **Say which origin this run is about to dial, in class terms, before any byte moves.** Origin privacy still forbids the host, so
    // the line names where the answer came from rather than what it is. It exists because `NP_MODELS_BASE_URL=` (set, empty)
    // counts as unset — a rule `tests/config.rs` pins on purpose, mirroring how an empty `$XDG_*` is treated — and
    // "unset" no longer means "refuse": it means the compiled-in origin. A mirror whose value
    // was lost to a shell (`NP_MODELS_BASE_URL=$MIRROR` with `MIRROR` unset) would otherwise spend the operator's egress
    // budget in silence.
    if !json {
        emit(origin_kind_line(&config));
    }

    // **Fetch what this binary can use**: with no `--member`, the plan is
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

fn models_install(workspace: &Path, request: &Install, json: bool) -> Result<(), Box<dyn Error>> {
    let config = load_config()?;
    // An install writes a row and copies an artifact into the per-workspace model directory. Neither is in the transaction that `scan` would
    // open, so a concurrent index can observe a half-installed model — which is why this command is inside the same lock as everything else
    // that changes state, not merely next to it.
    let _writer = writer_lock(&config, workspace, "models install")
        .map_err(|failure| failure.report_cli(json))?;
    let db = workspace_db(&config, workspace)?;
    let outcome = models::install(&db, &config, workspace, request, now_secs())?;
    if json {
        // `state` is a pinned key and its three strings come from
        // [`np_core::models::InstallOutcome::state_label`], not from a format string here. `id` rides along
        // because a JSON reader of a redirected run has no argv to look at, and the same two keys are present
        // for all three outcomes — one shape per command, as the document contract requires.
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

/// `np models install` for a pinned reranker: the same door, three fewer flags, and no row at the end.
///
/// `--dim`, `--prefix-doc` and `--prefix-query` are refused **by name** rather than quietly ignored. They describe how a
/// model stores vectors, and a scoring head stores none; accepting them would let someone believe they had declared
/// something about the reranker, and the row that could hold such a declaration does not exist for it
/// ([`np_core::pins::Role`]).
///
/// `--sha256` stays required so this command has one shape for every artifact — and for a pinned reranker it must name
/// the bytes the pin already names. A declared digest nobody compares would be the worst of both: a flag that looks like
/// a check and is not one.
pub(crate) fn install_reranker(args: &InstallArgs) -> Result<(), Box<dyn Error>> {
    let mut refused: Vec<&str> = Vec::new();
    if args.dim.is_some() {
        refused.push("--dim");
    }
    if !args.prefix_doc.is_empty() {
        refused.push("--prefix-doc");
    }
    if !args.prefix_query.is_empty() {
        refused.push("--prefix-query");
    }
    if !refused.is_empty() {
        return Err(format!(
            "{} describe how a model stores vectors; `{}` ranks pairs and stores none, so they are not what this \
             artifact installs — drop {}",
            refused.join(", "),
            args.id,
            if refused.len() == 1 {
                "the flag"
            } else {
                "those flags"
            }
        )
        .into());
    }

    let pin = np_core::pins::pinned_member(&args.id, "main").ok_or_else(|| {
        format!(
            "`{}` is pinned as a reranker but has no `main` member to install",
            args.id
        )
    })?;
    let declared = args.sha256.trim().to_lowercase();
    if declared != pin.sha256 {
        return Err(format!(
            "`{}` is pinned as sha256 {}; the digest given describes other bytes, and a reranker's bytes are the \
             pin's decision, not the caller's",
            args.id, pin.sha256
        )
        .into());
    }

    // No database is opened, because nothing is written to one: `install_reranker` in np-core takes no `Db` for exactly
    // that reason, and asking for a workspace's database here would create the file this command then declines to touch.
    let config = load_config()?;
    let outcome = np_core::models::install_reranker(
        &config,
        &args.workspace,
        &np_core::models::RerankerInstall {
            id: args.id.clone(),
            source: args.source.clone(),
        },
    )?;
    if args.json {
        emit_json(&json!({
            "id": args.id,
            // The role beside the state, so a reader of `--json` can tell this record from an embedding install without
            // knowing which ids are which.
            "role": "rerank",
            "state": outcome.state_label(),
        }));
    } else {
        emit(outcome.state_label());
    }
    Ok(())
}

/// `np models install --member …` — one member of an already-installed model, installed from a file with no network.
///
/// Three refusals come before anything is opened, because each is a fact about argv that a 554 MB read would not teach:
/// an invented member name, `--dim` on a member that has no width, and the main member's own missing width — the last of
/// which clap no longer enforces, because a pinned reranker installs through this same subcommand and must be allowed to
/// arrive without a width (see [`install_reranker`]).
///
/// The prefixes travel to the main path and are refused for `mmproj` by omission: they are fields of `models`, and a
/// projector install writes no row there.
/// What `np models install` was given, before anyone decided which member those flags describe. One carrier because the
/// two paths read overlapping halves of it, and passing nine positional arguments turned a lint into a design question:
/// the width is `None` exactly when the human named a member that has no width, so the pair belongs together.
pub(crate) struct InstallArgs {
    pub(crate) workspace: PathBuf,
    pub(crate) id: String,
    pub(crate) source: PathBuf,
    /// `None` for a member install — which is why it is not a `NonZeroU16` here even though [`Install`] insists on one.
    pub(crate) dim: Option<NonZeroU16>,
    pub(crate) sha256: String,
    pub(crate) prefix_doc: String,
    pub(crate) prefix_query: String,
    pub(crate) json: bool,
}

/// The default shape: `--member` absent, or spelled `main`. clap requires the width on this path; the guard below answers
/// only if that requirement is ever loosened, and answers with a message rather than a panic.
pub(crate) fn install_main(args: &InstallArgs) -> Result<(), Box<dyn Error>> {
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

pub(crate) fn models_install_member(
    args: &InstallArgs,
    member: &str,
) -> Result<(), Box<dyn Error>> {
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

/// `np models hash <file>` — `<sha256> <path>`, the two-space spelling `sha256sum` writes, so the first field
/// pastes straight into `install --sha256`.
///
/// No database and no digest of its own: [`sha256_file`] streams the file and returns lowercase hex, which is
/// what makes the test that compares this command's output with that function's, byte for byte, a tautology rather than a
/// coincidence.
pub(crate) fn models_hash(file: &Path) -> Result<(), Box<dyn Error>> {
    // `models::access` is the spelling the OS wants (absolute, no symlink resolution); on Windows a long relative
    // argument would otherwise reach `walk::to_open_path` still relative and fail with a raw prefix error.
    let digest = sha256_file(&models::access(file))?;
    emit(&format!("{digest}  {}", file.display()));
    Ok(())
}

/// `np models list <workspace>` — every row and what its artifact looks like on disk.
///
/// The directory-level failure is handled apart from the rest because this command refuses to let it become a per-row
/// verdict: [`models::list`] answers it with no [`Listing`] at all, so there are no rows to print and no
/// `missing` to claim against any of them. In text mode stdout stays empty; under `--json` the pinned key set
/// still goes out with `"models_dir_error"` holding the message, since that key exists precisely to be the
/// machine-readable form of this state. The same sentence then travels the error channel and the exit code is
/// non-zero — two channels, one fact, exactly as the scan receipts do it.
pub(crate) fn models_list(workspace: &Path, json: bool) -> Result<(), Box<dyn Error>> {
    let config = load_config()?;
    let db = workspace_db(&config, workspace)?;
    match models::list(&db, &config, workspace) {
        Ok(listing) => {
            if json {
                emit_json(&listing_json(
                    &listing.models,
                    &listing.fetching,
                    &listing.unclaimed,
                    listing.reranker.as_ref(),
                    &pinned_json(&config),
                    None,
                ));
            } else {
                print_listing(&listing);
            }
            Ok(())
        }
        Err(err @ ModelError::ModelsDir { .. }) => {
            if json {
                // The plan still goes out beside the failure: it is a fact about this build and this config, and the
                // person whose artifact directory is unreadable is exactly the one wondering what a re-fetch costs.
                // No reranker section either: the directory could not be listed, so nothing about the bytes under it
                // is known — including whether a reranker is there. `null` says "unknown here", which is what happened.
                emit_json(&listing_json(
                    &[],
                    &[],
                    &[],
                    None,
                    &pinned_json(&config),
                    Some(&err.to_string()),
                ));
            }
            Err(err.into())
        }
        Err(err) => Err(err.into()),
    }
}

pub(crate) fn links_json(listing: &np_core::links::LinkListing, indexed: bool) -> Value {
    json!({
        "indexed": indexed,
        "edges": listing
            .edges
            .iter()
            .map(|edge| {
                json!({
                    "src": edge.src,
                    "dst": edge.dst,
                    "kind": edge.kind,
                    "target": edge.target,
                    "line": edge.line,
                })
            })
            .collect::<Vec<Value>>(),
        "unresolved": listing
            .unresolved
            .iter()
            .map(|edge| {
                json!({
                    "src": edge.src,
                    "kind": edge.kind,
                    "target": edge.target,
                    "line": edge.line,
                })
            })
            .collect::<Vec<Value>>(),
    })
}

/// The `--json` body of `np models list`: one object whose key set is identical for every state, built by
/// this one function so the success path and the directory-level failure cannot grow different keys.
///
/// The three top-level names and the seven per-row names are pinned by a test that compares the whole
/// set rather than looking one key up — `state` or `unclaimed` renamed must fail here, not in a reader written
/// against the old spelling.
///
/// **`fetching` is a deliberate protocol change**:
/// a fetch is orthogonal to the four `ArtifactState` verdicts (first start has no row at all, and a present row
/// can be re-fetching), so it gets a listing-level section beside `unclaimed` rather than a fifth arm. The key
/// set is pinned precisely so that adding it had to fail a test — and did.
pub(crate) fn listing_json(
    models: &[ListedModel],
    fetching: &[FetchEntry],
    unclaimed: &[Unclaimed],
    reranker: Option<&np_core::models::ListedReranker>,
    pinned: &[Value],
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
        // The reranker this build pins, or `null` when it pins none. A key rather than an entry in `models` because
        // there is no row behind it: a scoring head declares no vector width, so it is not a model in the sense that
        // table means (`np_core::pins::Role`). Present-and-null on a build without one, for the same reason
        // `models_dir_error` is: a reader should not need a missing-key branch to ask an answered question.
        "reranker": reranker.map(|entry| {
            json!({
                "id": entry.id,
                "role": "rerank",
                "file": entry.file,
                "state": entry.state.state_label(),
                "pinned_bytes": entry.pinned_bytes,
            })
        }),
        // **What a fetch would cost before it starts** (**#3**), beside the sections that say what is already here.
        // Empty whenever no pinned id is configured — an absent key would cost every reader a branch, and a `null`
        // would make "no model named" and "no size known" one answer.
        "pinned": pinned,
        // Present and null whenever the directory could be listed: suppressing the key on the good path would
        // cost every reader a missing-key branch to answer a question `null` already answers (the precedent is
        // `scan --json`'s `upgrade_orphans`).
        "models_dir_error": models_dir_error,
    })
}

/// The `pinned` section of the listing document: **what this build would fetch for the workspace's configured model**,
/// priced from the pin table (**#3**).
///
/// Zero or one entry today, because `[embeddings] model` names one id — a list so that a build which pins a second
/// configurable model publishes it without a schema change.
///
/// Three properties the caller depends on, each of which is a choice rather than an accident:
///
/// * **Pinned bytes, never a transport's `Content-Length`** — the rule for progress, extended to the question
///   asked *before* a transfer. A chunked response announces nothing; this number is in the binary.
/// * **The build's plan, not the whole table** — [`np_core::pins::plan_size`] sums what this binary volunteers, which is
///   also what the terminal consent prompt prints, so one machine gives one price. `total_size` (every pinned member,
///   864,676,512 today) is the number the disk preflight uses, and it would over-quote a text-only build by 554 MB.
/// * **Independent of the filesystem** — no stat, no row, no lock: it answers for a workspace with nothing installed,
///   for one whose artifact directory cannot be listed, and while an embed is running. Whether *these* bytes are
///   already present is `models[].state`, and a client that wants "download needed?" reads both keys rather than having
///   this one mean two things.
pub(crate) fn pinned_json(config: &Config) -> Vec<Value> {
    let Some(id) = config.embeddings.model.as_deref() else {
        return Vec::new();
    };
    let plan = np_core::pins::fetch_plan(id);
    if plan.is_empty() {
        // Configured, but this build pins no member of it: an id from `np models install` has no price to name,
        // and borrowing another model's number would be the exact lie this section exists to prevent.
        return Vec::new();
    }
    std::iter::once(json!({
        "id": id,
        "bytes_total": plan.iter().map(|entry| entry.size_bytes).sum::<u64>(),
        // The member roles in fetch order, so a client can line this plan up with the per-member `fetching` progress
        // lines it will see later. File names stay out: they are where an origin lives, and the report rules keep that
        // shape out of reports.
        "members": plan.iter().map(|entry| entry.member).collect::<Vec<&str>>(),
    }))
    .collect()
}

/// One row under `--json`, with exactly the eight pinned keys — and the full digest, which the text column
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
        // with only its main artifact answers `[]`, which is a fact and not an omission (the gap this field closed:
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
/// `path` is here because the advice that goes with a stale marker — "if no fetch is running, delete this file and
/// re-run" — cannot be
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
        // Null means "this fetch never announced a size" — never zero, which would be a
        // denominator this program invented.
        "bytes_total": fields.map(|fields| fields.bytes_total),
        "marker_error": marker_error,
    })
}

/// The two words the listing needs from an unclaimed entry: a subdirectory has to be **reported as one**, which is why
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
    // a 64-character id.
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
    // The reranker gets one line of its own — outside the `models` count above it, because counting a scoring head
    // among the models would tell a reader they can select it as an embedder. Its expected size travels beside the
    // verdict so "present, size differs" is actionable against `ls` without looking the number up.
    if let Some(reranker) = &listing.reranker {
        let _ = writeln!(
            out,
            "reranker   {}  {}  (pin expects {} bytes)",
            reranker.id,
            reranker.state.state_label(),
            reranker.pinned_bytes
        );
    }
    // One line per fetch in progress, and **no section header**: `models` and `unclaimed` print their counts even
    // when empty because "looked and found none" must not read as "printed nothing", while a fetch line is an
    // event — the pinned shape asks for nothing extra when nothing is fetching, so an idle workspace prints exactly
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
/// The count rides on every line rather than heading a block because that is the pinned shape — one line per
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
/// deciding that the size pair is impossible when only the marker's writer can say whether it happens.
fn fetch_line(count: usize, entry: &FetchEntry) -> String {
    let plural = if count == 1 { "fetch" } else { "fetches" };
    let head = format!("{count} {plural} in progress · ");
    match &entry.state {
        FetchState::Present(fields) => {
            let done = size_human(fields.bytes_done);
            // No denominator invented: an unknown total says so in words, which is the whole point — a
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
/// read as a truncated digest; `NULL` is the row's own statement that no artifact is expected.
fn text_digest(sha256: &Option<String>) -> String {
    match sha256 {
        None => "NULL".to_string(),
        Some(hex) => hex.chars().take(DIGEST_TEXT_CHARS).collect(),
    }
}
