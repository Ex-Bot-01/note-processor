use crate::commands::common::emit_json;
use crate::commands::common::load_config;
use crate::commands::common::number;
use crate::commands::common::workspace_db;
use crate::commands::embed::embed_provider;
use crate::commands::embed::AutoFetch;
use crate::commands::embed::ProviderState;
use crate::commands::embed::Resolved;
use np_core::embed_run;
use np_core::embed_run::NoProvider;
use np_core::search;
use np_core::search::Hit;
use serde_json::{json, Value};
use std::error::Error;
use std::io;
use std::io::Write;
use std::path::Path;

/// `--mode` as clap sees it. A separate `clap` enum because the value parser and the help list have to come from the same
/// place as the names; [`SearchModeFlag::to_core`] is the only bridge, so a fourth spelling cannot appear in one door and not
/// the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum SearchModeFlag {
    /// Both lists, fused by rank. The default.
    Hybrid,
    /// Vectors only — the behaviour before FTS5 existed.
    Semantic,
    /// FTS5 only; answers with no model installed and never returns a figure.
    Keyword,
}

impl SearchModeFlag {
    pub(crate) fn to_core(self) -> search::SearchMode {
        match self {
            Self::Hybrid => search::SearchMode::Hybrid,
            Self::Semantic => search::SearchMode::Semantic,
            Self::Keyword => search::SearchMode::Keyword,
        }
    }
}

/// Rank `query` against `workspace`'s index and print the hits.
///
/// The provider is built exactly as `np embed` builds it, because the query must be embedded by the **same** model whose
/// rows are being read — and under its `prefix_query` spelling, not the document one ([`np_core::embed::Embedder::embed_query`]). That is also
/// where a run that cannot embed refuses at exit 1, with the sentence for its own [`ProviderState`]: an absent artifact, an
/// unbuilt runtime, a missing row, a switched-off provider. A query nobody can embed has no ranking to print —
/// and until recently four of those five shared one sentence about `provider = "none"`.
/// What one search produced, before anybody printed it.
///
/// **Why this type exists.** `np search` used to interleave "work out the answer" with "write bytes to stdout", which meant the only way to get a
/// search document was to print one. `np serve` needs the same object on a different channel, and the alternative to this struct was a second
/// implementation of provider resolution, fetch policy and note selection — two vocabularies for one state, which is the failure every card from
/// these states have been about. The CLI prints this; the server serialises it; neither can drift, because there is
/// one of each function.
pub(crate) struct SearchRun {
    pub(crate) hits: Vec<search::Hit>,
    pub(crate) mode: search::SearchMode,
    /// Whether the index holds vectors at all — the truth about the workspace, which must stay `true` even when this
    /// answer could
    /// not use them, because a reader that sees `false` will go and re-fetch a model it does not need.
    pub(crate) present: bool,
    /// Whether anyone has ever scanned this workspace. `mode`, `vectors` and `reason` all answer
    /// questions *about the answer*; this one answers a question about the workspace, and until now an empty list
    /// from a vault nobody indexed was indistinguishable from an empty list from a vault with nothing to find.
    pub(crate) indexed: bool,
    /// Which halves of the mode actually ran. `mode` is what the caller *asked for* — and since hybrid answers from
    /// keywords alone while `vectors` keeps reporting the truth about the workspace,
    /// so neither of those two says what this answer was built from. A client that shows "hybrid" here is showing
    /// a word `np` chose for an intent, not a word for a result; these two are the words for the result.
    pub(crate) keyword_used: bool,
    pub(crate) semantic_used: bool,
    /// Whether this answer matched any word at a word beginning, and which words (**#6**). Read off the answer rather than
    /// off the flag: `--prefix` on a query with no word in it widened nothing, and a key that contradicts `hits: []` is the
    /// kind of pair a caller has to reconcile.
    pub(crate) prefix_used: bool,
    pub(crate) prefix_terms: Vec<String>,
    pub(crate) note: Option<search::RankNote>,
    pub(crate) fetched: Option<Value>,
    pub(crate) offline: bool,
    /// The id whose model (if any) embedded the query — needed only by the text channel's "nothing indexed for …" sentence.
    pub(crate) model: String,
    /// What the optional cross-encoder stage did. Always carried, including on a run that never asked (**F51**): "was this list
    /// reranked?" is answered from the document rather than inferred from an absent key.
    pub(crate) rerank: search::RerankReport,
    /// The reranker id when a stage was planned, `None` when it was not — so `model` says which artifact could have scored, and
    /// never names one that was not asked for.
    pub(crate) rerank_model: Option<String>,
    /// Which text of a note the policy reads. Carried even when the stage is off, because the answer to "what would it score?"
    /// is a fact about this run's configuration and a reader measuring the two policies needs it before they ask.
    pub(crate) rerank_text: search::RerankText,
}

// Nine parameters, and the last two are what this signature grew for. A parameter object was considered for the pair and
// refused: they are read once, at the door, and bundling them would name a type that exists only to be unpacked — while every
// existing caller would still pass the other seven. The precedent is `search::ranked`, which carries its stage arguments in
// `RerankInput` because those *do* travel together into one call.
#[allow(clippy::too_many_arguments)]
/// Run one search. `announce` is the human channel: it lets provider resolution print what it is about to download, and is off for `--json` and
/// for `np serve`, where the document carries those facts instead (`fetched.origin_kind`, `reason`).
pub(crate) fn search_run(
    workspace: &Path,
    query: &str,
    limit: usize,
    mode: Option<search::SearchMode>,
    prefix: search::Prefix,
    announce: bool,
    progress: &dyn np_core::progress::Progress,
    rerank: Option<bool>,
    rerank_text: Option<search::RerankText>,
) -> Result<SearchRun, Box<dyn Error>> {
    let cfg = load_config()?;
    // Config first, flag over it: `[search] mode` is the standing choice and `--mode` is the exception to it.
    let mode = mode.unwrap_or(cfg.search.mode);
    // **The rerank door, opened before any retrieval**, so an explicit `--rerank` that cannot be honoured refuses before the run
    // has cost anything — and so a decline carries the same cause into both channels instead of being rediscovered at print time.
    let plan = crate::commands::rerank::plan(&cfg, rerank, rerank_text);
    let mut scorer: Option<Box<dyn np_core::rerank::Reranker>> = None;
    let mut declined: Option<search::RerankNote> = None;
    if let Some(plan) = &plan {
        match crate::commands::rerank::prepare(&cfg, workspace, plan, announce)? {
            crate::commands::rerank::Opened::Ready(loaded) => scorer = Some(loaded),
            crate::commands::rerank::Opened::Declined(note) => declined = Some(note),
        }
    }
    if let (Some(note), Some(plan)) = (&declined, &plan) {
        // Reported once here rather than at every print site; the explicit case then refuses with that same sentence inside its
        // own, so stdout's absence and stderr's line describe one fact.
        note.report();
        if plan.ask == search::RerankAsk::Explicit {
            return Err(Box::new(search::SearchError::RerankUnavailable {
                cause: note.clone(),
            }));
        }
    }
    let db = workspace_db(&cfg, workspace)?;
    // **Asked before any work, because it is a fact about the workspace rather than about the answer** (#5).
    // `workspace_db` above has already opened (and so minted) the database, which is why this reads the scan
    // stamp and the rows rather than asking whether a file exists: an unindexed vault that somebody *searched*
    // has a database file on disk, and answering "indexed" about it would repeat the exact confusion the issue
    // reported, one layer down.
    let indexed = np_core::scan::indexed(&db)?;

    // **Which halves this query can ask about, decided before any model is loaded.** Keyword mode needs no provider at all —
    // that is the property the human asked for when hybrid became the default: search answers on a machine that has never
    // downloaded 310 MB.
    //
    // **Two decisions shape this block, which is why the comments are long.**
    //
    // *Hybrid is the mode that always answers.* It used to degrade to keyword only when the index held no
    // vectors; with vectors in it and the semantic half unbuildable — `embeddings.provider = "none"`, a model that was never
    // installed here, bytes deleted after an earlier embed — it exited 1 having written **zero bytes to stdout**, while
    // `--mode keyword` answered the same query happily. An application calling the default mode could not tell "no results" from
    // "search is broken", and the operator's own harness produced that state on every run. So hybrid no longer refuses for a
    // missing semantic half: it answers from FTS5 and says which half was absent, in prose on stderr and in a stable token under
    // `reason`. `--mode semantic` keeps every refusal it had — asking for semantics and silently receiving keyword results is the
    // exact failure these refusals exist to prevent.
    //
    // *First install.* "When the program has it first install on a machine it tries to
    // install the model; if it has no internet connection then it falls back." Hybrid may therefore trigger the pinned fetch, and
    // offline is a state it reports rather than a failure it throws. The metered-laptop objection to that is answered by
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
    // `fetched` and `offline` carry the meanings embed's document already uses, so one caller vocabulary covers both
    // commands: "what
    // arrived from an origin", and "the pinned origin could not be reached". `reason` (below) says which half is missing.
    let mut fetched: Option<Value> = None;
    let mut offline = false;

    if resolve {
        // **`resolving` says which half of the answer is still missing** (**O2**). This is the stretch where a search can
        // start downloading 309 MB without writing a byte to stdout, so it is the one a client needs a word about before
        // it decides the process has hung. Keyword mode never reaches here — it resolves no provider and dials no
        // transport — which is exactly why the phase is absent there rather than reported as instant.
        progress.report(&np_core::progress::ProgressEvent {
            phase: np_core::progress::Phase::Resolving,
            ..Default::default()
        });
        // **The mode opens the door; only configuration and the environment bolt it.** `AutoFetch::Deferred` is the same question
        // `np embed` asks when its caller passed neither `--auto-fetch` nor `--no-auto-fetch`: `[models] auto_fetch` first, then
        // `NP_NO_AUTO_FETCH=1`, which outranks everything. Search has no fetch flags of its own on purpose — `--mode` is the only
        // lever it exposes — so a search that ignored this would download 309 MB on a host whose operator had switched fetching
        // off for every command. That is not a hypothetical: the first draft of this change passed `hybrid` straight through, and
        // `search_with_fetching_switched_off_says_so_instead_of_blaming_the_network` failed on the `local-model,fetch` row saying
        // `offline:true` where the switch meant "nothing was attempted".
        let fetch_allowed = hybrid && AutoFetch::Deferred.allowed(&cfg);
        let resolved = embed_provider(&cfg, workspace, fetch_allowed, announce, progress)?;
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
                // not a micro-optimisation: embedding the query is what surfaces provider-level refusals, and one of them is the
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
            // ---- `--mode semantic`: every refusal below keeps its existing wording. Asked for semantics, cannot
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
            // ---- Hybrid: a half is missing and the answer is not withheld. These are separate arms rather than one `_`
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
            // Spelled as refusals rather than `unreachable!()` or a `_` arm for two reasons. The rule at this call site is
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

    // **The note is reported where it becomes final**: every state decided above goes out
    // once, with the model id this run resolved. A note that `ranked` adds instead — a hybrid search whose index holds no
    // vectors at all — is reported by [`search`] itself, which is where that fact is decided; the two paths cannot both
    // fire for one run, because reaching `search` needs a query vector and `ranked`'s note needs the absence of one.
    if let Some(note) = &note {
        note.report((!model.is_empty()).then_some(model.as_str()));
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
        prefix,
        match (&mut scorer, &plan) {
            (Some(scorer), Some(plan)) => Some(search::RerankInput {
                reranker: scorer.as_mut(),
                ask: plan.ask,
                top_n: plan.top_n,
                text: plan.text,
            }),
            // Either no stage was asked for, or it was and could not open — a decline already decided above, so the answer here
            // is retrieval order with `disabled`/`note` reported from `declined` below.
            _ => None,
        },
    )?;
    let hits = answer.hits;
    let note = note.or(answer.note);
    // Read off the answer rather than re-derived from `mode` and `vector.is_some()`: `ranked` is the function that
    // decided which lists it consulted, so a second derivation here is one more place to be wrong — and it already
    // would be, because a query with no word characters in it runs no keyword list even in keyword mode.
    let (keyword_used, semantic_used) = (answer.keyword_used, answer.semantic_used);

    Ok(SearchRun {
        hits,
        mode,
        present,
        indexed,
        keyword_used,
        semantic_used,
        prefix_used: answer.prefix_used,
        prefix_terms: answer.prefix_terms,
        note,
        fetched,
        offline,
        model,
        rerank: declined
            .map(|note| search::RerankReport {
                ran: false,
                // `skip` stays `None`: this is not one of the two non-fault reasons, it is a cause with its own token.
                skip: None,
                note: Some(note),
                candidates: 0,
            })
            .unwrap_or(answer.rerank),
        rerank_model: plan
            .as_ref()
            .map(|_| np_core::pins::RERANKER_ID.to_string()),
        // The policy that applies to this run — flag over config — reported **whether or not a stage ran**. Two reasons it is
        // not read off the plan: a measurement wants the key to say what it asked for even on the run where the artifact was
        // missing, and a key that silently reverted to the default whenever reranking was off would make the two directions of
        // `--rerank`/`--no-rerank` indistinguishable in the document.
        rerank_text: rerank_text.unwrap_or(cfg.search.rerank_text),
    })
}

/// The document `np search --json` prints, in one function so no second channel can invent its own keys.
///
/// One object per hit under `hits`, beside the two keys that say how the list was made. The empty case is an empty array — a machine reader
/// does not need a sentence, and a second key per hit would be a schema change in disguise; `mode` and `vectors` describe the *list*, so they sit
/// beside it.
///
/// **Why `reason` is not the same thing as `note`.** `note` is prose for a human and may be reworded; `reason` is the token a
/// caller branches on when it wants to know which half answered. `fetched` and `offline` mean exactly what they mean in `np embed --json`,
/// so one reader handles both commands: an application that called `np search` first on a fresh machine can now see `fetched` arrive,
/// `vectors:false`, and `reason:"artifact-fetched"` telling it the next call is `np embed`. Nothing here changes when the semantic half worked:
/// `reason` is null and `note` stays null, exactly as before.
pub(crate) fn search_document(run: &SearchRun) -> Value {
    let objects: Vec<Value> = run.hits.iter().map(hit_json).collect();
    json!({
        "hits": objects,
        // **What was asked for**, not what ran: `mode` is the caller's request after config, and hybrid may have
        // answered from one half. `keyword_used` / `semantic_used` beside it are what ran.
        "mode": run.mode.label(),
        "keyword_used": run.keyword_used,
        "semantic_used": run.semantic_used,
        // **Whether any word here was matched at a word beginning** (#6), and which ones. Beside the two above because
        // `keyword_used` says the keyword half ran; this says it widened the query, which is the fact a caller needs before
        // printing `"grand"` as though a note containing only `grandma` said "grand". Per-hit truth travels in each hit's
        // `matched_exactly`.
        "prefix_used": run.prefix_used,
        "prefix_terms": run.prefix_terms,
        "vectors": run.present,
        // The workspace's own state, and the only key that answers "should I offer to index this folder?" (#5).
        "indexed": run.indexed,
        "note": run.note.as_ref().map(search::RankNote::label),
        "reason": run.note.as_ref().map(search::RankNote::token),
        "fetched": run.fetched.clone(),
        "offline": run.offline,
        // **What the optional rerank stage did** (**F51**), always present. `reason` is one key for both kinds of "did not run"
        // — a non-fault (`disabled`, `no-candidates`) and a cause (`rerank-artifact-missing`, …) — because they answer the same
        // question, "why is this still retrieval order?", and a caller that branched on two keys would have to know which kind it
        // was asking about. `note` carries the sentence when there is a cause; `model` names the artifact only when one was asked
        // for; `text` says what that artifact reads, so a measurement can record the policy beside its numbers.
        "rerank": {
            "ran": run.rerank.ran,
            "reason": run
                .rerank
                .skip
                .map(|skip| skip.token())
                .or_else(|| run.rerank.note.as_ref().map(|note| note.token())),
            "note": run.rerank.note.as_ref().map(search::RerankNote::label),
            "candidates": run.rerank.candidates,
            "model": run.rerank_model,
            "text": run.rerank_text.label(),
        },
    })
}

/// Parsing `--rerank-text`, with the same sentence [`np_core::search::RerankText`]'s `FromStr` gives a config key: one wording
/// for both doors, so a person who mistypes the value learns the two accepted words whichever door they typed it into.
pub(crate) fn parse_rerank_text(
    text: &str,
) -> Result<search::RerankText, Box<dyn std::error::Error + Send + Sync>> {
    Ok(text.parse()?)
}

/// `np search`: run the query, then print it in the channel the caller asked for.
#[allow(clippy::too_many_arguments)] // argv-shaped: one argument per flag, and every caller is `main`'s dispatch
pub(crate) fn search_workspace(
    workspace: &Path,
    query: &str,
    limit: usize,
    mode: Option<search::SearchMode>,
    prefix: search::Prefix,
    rerank: Option<bool>,
    rerank_text: Option<search::RerankText>,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let run = search_run(
        workspace,
        query,
        limit,
        mode,
        prefix,
        !json,
        np_core::progress::none(),
        rerank,
        rerank_text,
    )?;
    if json {
        emit_json(&search_document(&run));
        return Ok(());
    }
    let SearchRun {
        hits,
        mode,
        note,
        model,
        indexed,
        prefix_used,
        prefix_terms,
        ..
    } = run;
    let mut out = io::stdout().lock();
    if let Some(note) = &run.rerank.note {
        // One line on stderr for the stage that declined, beside the list it did not reorder: exit 0 because the answer is real,
        // a sentence because "silently un-reranked" is how a person stops trusting an ordered list. `--json` carries the same
        // cause in `rerank.note` / `rerank.reason` instead of printing prose.
        let _ = writeln!(io::stderr(), "{note}");
    }
    if let Some(reason) = note {
        // One line on stderr for a half that was not there to answer. Stderr because stdout stays parseable; the exit code is
        // still 0 when the keyword list answered, which is the point of the mode.
        let _ = writeln!(io::stderr(), "{reason}");
    }
    if hits.is_empty() {
        // An empty index is a fact. Written to stdout, exit 0 — the caller has nothing to fix.
        // `--limit 0` asks for no hits and gets no lines, which is not the same sentence, so it is not printed.
        if limit > 0 {
            // Three sentences, because the old two printed a lie on Windows' most likely first run. With no `[embeddings]`
            // section and no vectors, `model` is the empty string — and the hybrid arm interpolated it, producing
            // `nothing indexed for by hybrid`: a double space where a model name should be, and a claim about an index that
            // the keyword half had just consulted. When there is no model to name, the sentence names the query and says the
            // vectors are missing instead (found by the first interactive Windows run, 2026-11-07).
            // **"Never indexed" gets its own sentence** (#5). A person who has just pointed `np` at a folder types
            // the words they can remember; being told *nothing matched* sends them to their notes, when the action
            // is one `np scan` away. The other three arms below stay exactly as they were for a workspace that has
            // been walked, because there the empty list really is an answer about the corpus.
            if !indexed {
                let _ = writeln!(
                    out,
                    "nothing matched \"{query}\": this workspace has never been scanned — \
                     run np scan {} to index it",
                    workspace.display()
                );
                return Ok(());
            }
            // A widened query gets the widening named in the refusal too: `nothing matched "grand"` after `--prefix` would
            // tell a reader their notes hold no word beginning with `grand`, which is exactly what this run did not check.
            let widened = if prefix_used {
                " — not even as the start of a word"
            } else {
                ""
            };
            let _ = match (mode, model.is_empty()) {
                (search::SearchMode::Keyword, _) => writeln!(out, "nothing matched \"{query}\"{widened}"),
                (_, true) => writeln!(
                    out,
                    "nothing matched \"{query}\"{widened} by {mode}: this workspace holds no vectors"
                ),
                (_, false) => writeln!(out, "nothing indexed for {model} by {mode}"),
            };
        }
        return Ok(());
    }
    if prefix_used {
        // One line naming the words this run matched as beginnings, above the list it explains. The terms come from the plan
        // rather than from `query`, so a marked `grand*` prints `grand`: what was searched, not what was typed (#6).
        let _ = writeln!(out, "word beginnings matched: {}", prefix_terms.join(", "));
    }
    for hit in &hits {
        // Score to four decimals (full precision travels in `--json`), kind padded so the paths start in one column. In
        // hybrid the score is a fused rank sum, not a similarity — which is why the raw numbers ride beside it here too.
        // The score column shows whatever ordered this list. After a rerank that is the cross-encoder's number, and printing the
        // retrieval score here instead would put a number beside an order it did not produce — while `--json` keeps both, because
        // a machine reader wants to see the two passes separately.
        let score = number(hit.rerank_score.unwrap_or(hit.score));
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
                // belonging to four notes should not read as though it belongs to the one printed. The note that is
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

/// One hit as JSON: **the same eleven keys whatever the kind and whatever the mode** —
/// `{score, cosine, bm25, semantic_rank, keyword_rank, kind, path, snippet, linked_from, source_note, linked_notes}`.
///
/// The five evidence keys are in this list rather than printed only in the modes that produce them, for the
/// reason below: a schema whose keys depend on a value inside the object makes every reader branch before it reads.
///
/// The first version added `linked_from` and `source_note` only for image hits, which made the schema depend on a value
/// inside the object — a reader then has to branch on `kind` before it knows whether a key is there, and a note hit whose
/// linker field is simply absent reads differently from one that has none. Absent and zero are different statements, so this
/// emits both keys always: `source_note: null` and `linked_from: 0` for a hit that has no linking note (every note hit, and
/// an orphaned figure). Same rule `EmbedReport::artifact_check` already follows — present and null, not missing.
///
/// `linked_notes` follows the same rule in list form: **an empty array, never `null`**. It is the whole relation — every
/// live note linking a figure, byte-ordered — where `source_note` was its head and `linked_from` its length, so the three
/// cannot tell different stories about one hit. An empty array means "this hit has no live linker", which is true of both a
/// note hit and an orphaned figure; distinguishing those two is what `kind` is for, and giving the list a null on one kind
/// and an empty array on the other would put the schema back where `kind`-dependent keys left it.
fn hit_json(hit: &Hit) -> Value {
    json!({
        // **The retrieval score, always** — the number that made this hit a candidate. It is *not* overwritten by a rerank, so a
        // reader can tell which pass produced the order they are looking at and measure the stage against retrieval instead of
        // taking its word for an improvement.
        "score": hit.score,
        // The two raw numbers beside the fused score. In `hybrid` a reader cannot recover them from `score`, and
        // judging ranking needs both columns; in a single-list mode one of them is `null`, which is the same
        // present-and-null statement `source_note` already makes rather than a key that appears only in some modes.
        "cosine": hit.cosine,
        "bm25": hit.bm25,
        // Which list put this hit where it is, and how far down. `null` means that half did not run — no vectors, or
        // `--mode keyword` — which is a different fact from "rank 0".
        "semantic_rank": hit.semantic_rank,
        "keyword_rank": hit.keyword_rank,
        // The cross-encoder's own evidence (**F51**), all three `null` for a hit it did not score. `rerank_score` is a logit on
        // an unbounded scale: comparable within one query only, and never a probability. The two ranks are the pair that shows
        // what the stage moved — retrieval position in, delivered position out.
        "rerank_score": hit.rerank_score,
        "rerank_rank_before": hit.rerank_rank_before,
        "rerank_rank_after": hit.rerank_rank_after,
        "kind": hit.kind.label(),
        "path": hit.path,
        // A matched phrase from the note body with the hit marked, or `null` on a vector-only hit: keyword evidence is a
        // property of the keyword half, not of a kind of note.
        "snippet": hit.snippet.clone(),
        // `true` whole words, `false` reached only by a prefix, `null` not from the keyword list at all (#6). `null` is not
        // `false`: a vector hit did not fail a whole-word test, it was never in one.
        "matched_exactly": hit.matched_exactly,
        "linked_from": hit.linked_from,
        "source_note": match &hit.source_note {
            Some(note) => json!(note),
            None => Value::Null,
        },
        "linked_notes": hit.linked_notes,
    })
}
