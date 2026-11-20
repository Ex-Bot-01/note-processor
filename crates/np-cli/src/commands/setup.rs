use crate::commands::common::emit;
use crate::commands::common::emit_json;
use crate::commands::common::load_config;
use crate::commands::common::workspace_db;
use crate::commands::common::writer_lock;
use crate::commands::embed::embed_refusal;
use crate::commands::embed::embed_report;
use crate::commands::embed::insert_drift;
use crate::commands::embed::prefix_drift_notice;
use crate::commands::embed::print_embed_summary;
use crate::commands::embed::AutoFetch;
use crate::commands::scan::print_summary;
use np_core::catalog::size_human;
use np_core::config::Config;
use np_core::db::model_row;
use np_core::scan;
use serde_json::json;
use std::error::Error;
use std::io;
use std::io::IsTerminal;
use std::path::Path;

/// `np setup`: index first, then embed (which may fetch), then say in one place how far this got.
///
/// Scan runs **before** anything network-facing on purpose: keyword search works the moment indexing finishes, so a
/// 310 MB transfer — or the four seconds spent discovering there is no network — must not stand between a person and their
/// first results. The embed half is [`embed_report`] unchanged, which means its arm order, its fetch trigger, and its
/// report fields are the same code `np embed` runs; setup adds an ordering and a verdict, not a second implementation.
///
/// Exit codes split by kind of stopping place: **offline exits 0** (indexed, searchable, waiting for a cable — nothing for
/// anyone to fix), every other vector-half refusal exits 1 with embed's own sentence. `vectors` is the machine-readable
/// verdict either way, so a caller never has to infer it from an exit code.
pub(crate) fn setup_workspace(
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
    let embedded = embed_report(workspace, auto_fetch, !json, np_core::progress::none())?;

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
        // condition that needs no code change, and the keyword index answers queries meanwhile.
        return Ok(());
    }
    if embedded.succeeded() {
        return Ok(());
    }
    Err(embed_refusal(&embedded).into())
}

/// Ask before transferring, when a person could answer — and never ask when one cannot.
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
    // The plan's own sum, from the table — the same number `models list --json` publishes under `pinned`, so a terminal
    // prompt and a GUI on one machine cannot quote two prices for one download (#3).
    let bytes: u64 = np_core::pins::plan_size(&id);
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
pub(crate) fn origin_kind(cfg: &Config) -> &'static str {
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

/// `np models install` — verify the human's file against their declaration, then record it.
///
/// The one sentence that says **which kind** of origin this run would dial.
///
/// Gated on `fetch` because both doors that print it — `np models fetch` and the auto-fetch trigger — are gated on the same feature: a build with
/// no HTTP client has no origin to describe, and an ungated helper there is a dead-function warning in the default CI row for
/// `--no-default-features`.
///
/// A *kind*, never a host: `np models fetch`'s existing line and this one share the wording so they cannot drift into
/// disagreeing about the same config state. The distinction it draws is real and cheap to lose — `[models] base_url`
/// unset means the origin baked into the pin table, while an empty `NP_MODELS_BASE_URL=` counts as unset too, so "no
/// configuration" and "configuration lost to a shell" look identical from here and both spend someone's egress.
#[cfg(feature = "fetch")]
pub(crate) fn origin_kind_line(cfg: &Config) -> &'static str {
    if cfg.models.base_url.is_none() {
        "origin      this build's compiled-in default — no [models] base_url is configured (an empty \
         NP_MODELS_BASE_URL counts as unset)"
    } else {
        "origin      from [models] base_url"
    }
}

/// What one auto-fetch attempt produced.
#[cfg(all(feature = "local-model", feature = "fetch"))]
pub(crate) enum FetchStep {
    /// Bytes arrived and were published by digest. Carries the record `--json` will print.
    Fetched(np_core::embed_run::FetchedRecord),
    /// Nothing was reached. Not an error: a state, with its own report arm downstream.
    Offline,
}

/// Fetch the pinned id's members that **this build can use**, because this workspace is missing them.
///
/// Called only from the trigger in [`embed_provider`], which has already decided a fetch is permitted; this function's own
/// job is to be honest about the transfer and to classify its failure rather than flatten it.
///
/// * **Before byte one**, in text mode, it says what is about to happen: id, the members this build would load, their
///   combined size, and the origin's *kind*. `np models fetch` prints the same pair of lines for the same reason — a
///   silent 310 MB transfer is not a side effect anyone should discover afterwards.
/// * **Offline is its own answer** (`FetchError::is_offline`): no probe was run, and none will be — this call *is*
///   the probe, made only where the artifact was already missing, so a machine that holds its model pays nothing. The
///   sentence says what could not be reached, that nothing was embedded, and what to re-run; it does not dress a missing
///   network up as a broken vault, which is the same lesson applied one arm over.
/// * **Anything the origin actually answered** — 404, 403, a redirect, wrong bytes — propagates as `fetch`'s own refusal,
///   unchanged. Those are not outages and must never be skipped past: an unreachable mirror behind "offline" is how a
///   broken install becomes permanent.
#[cfg(all(feature = "local-model", feature = "fetch"))]
pub(crate) fn fetch_pinned_missing(
    cfg: &Config,
    db: &Db,
    models_dir: &Path,
    id: &str,
    announce: bool,
    progress: &dyn np_core::progress::Progress,
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
    match fetch::fetch_reporting(
        db,
        models_dir,
        id,
        &requested,
        &transport,
        now_secs(),
        progress,
    ) {
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
                // `fetch` publishes a member only after its digest matches the pin, so every member that
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
                // A *kind*, never a host: the same distinction `origin_kind_line` prints for humans.
                origin: origin_kind_token(cfg),
            }))
        }
        // **Offline is a state, so it returns one.** It used to come back as an error string from here, which meant `--json`
        // had no document to carry the fact and a caller had to read prose to learn that nothing was downloaded.
        Err(err) if err.is_offline() => Ok(FetchStep::Offline),
        Err(err) => Err(err.into()),
    }
}
