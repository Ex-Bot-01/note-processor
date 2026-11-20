use crate::commands::common::emit;
use crate::commands::common::emit_json;
use clap::Subcommand;
use np_core::catalog::size_human;
use np_core::config::Config;
use np_core::config::EmbeddingProvider;
use serde_json::{json, Value};
use std::error::Error;
use std::path::Path;

/// `np embeddings …`, the mirror of serve's `embeddings.enable` / `embeddings.disable`. Both doors answer one
/// document, because a GUI and a terminal asking the same question must be able to read the same answer.
#[derive(Debug, Subcommand)]
pub(crate) enum EmbeddingsCommand {
    /// Say yes: write `[embeddings] provider = "local-model"` and `model = <id>`. **Fetches nothing** — the download happens
    /// on the first `np embed` (or `np serve`'s `embed`), under the fetch command's announcement and digest
    Enable {
        /// The model to use, which must be one this build pins. `np models list --json` prints them under `pinned`, with the
        /// byte count each one costs.
        id: String,
        /// Print what changed as JSON instead of as sentences.
        #[arg(long)]
        json: bool,
    },
    /// Revoke it: `[embeddings] provider = "none"`. **`embeddings.model` is left alone** — withdrawing consent to download is
    /// not a licence to erase which model was chosen, and the file stays one key away from being on again.
    Disable {
        /// Print what changed as JSON instead of as sentences.
        #[arg(long)]
        json: bool,
    },
}

/// The document **`links.list`** answers with: who links here, where the dead targets are written, and
/// whether anybody has ever scanned this folder.
///
/// Three keys on every state, and no more:
///
/// * **`edges`** — links that land on a live note, each naming both ends as a display path plus the `kind`, the `target`
///   as written and the 1-based `line`. A pane filters this by comparing `dst` to the note it is showing.
/// * **`unresolved`** — links no live note answers, named by their target and where they were written. **No `dst` key**:
///   the destination is the thing in question, and a path beside a dead target would look like somewhere to click.
/// * **`indexed`** — issue **#5**'s distinction, carried here because this method walks into the same trap: an empty pair
///   of lists on a folder nobody scanned is not the finding "nothing links here", and a rename-rewriting tool that reads
///   it as one silently orphans every note in the vault.
///
/// Order is the core's reading order (source path, then line), pinned because this document is rendered into a list a
/// person reads top-down. `line` is 1-based for the same reason `np scan` names a line: it is the number an editor jumps
/// to, and the one number an LF and a CRLF copy of the same note agree on.
///
/// This is the second method with no terminal twin — like `status.get`, it answers a question a person at a shell does not
/// ask, so there is no CLI text channel for it and no stdout to keep byte-pinned beside this document.
/// Run `np embeddings enable|disable`, and answer with [`embeddings_document`] on both channels.
///
/// The config file is the one every run reads — there is no per-workspace config to write, which is why neither door takes a
/// workspace operand: only np can decide which of those a consent answer belongs to, and its answer is *this* one.
/// What one `embeddings.enable` / `embeddings.disable` call did, in the shape both doors need: the shared document for a
/// machine, and the four facts the terminal sentence is assembled from.
pub(crate) struct EmbeddingsOutcome {
    /// The document [`embeddings_document`] builds — what `--json` prints and what serve answers.
    pub document: Value,
    /// The keys np wrote, empty when nothing moved.
    pub changed: Vec<String>,
    /// Which of the two doors ran, because "already on" and "already off" are different sentences.
    pub enabling: bool,
    /// What the config says afterwards, as its own words rather than as an enum a caller would have to know.
    pub provider: String,
    /// The model the config names afterwards — `Some` after a disable too, because np leaves that key alone.
    pub model: Option<String>,
}

/// Apply one embeddings target to np's config file and report it. Shared by `np embeddings …` and serve's two methods so the
/// refusals, the reload and the document are one code path, and neither door can validate less than the other.
pub(crate) fn embeddings_apply(
    target: np_core::config_write::EmbeddingsTarget<'_>,
    surface: np_core::config_write::WriteSurface,
) -> Result<EmbeddingsOutcome, Box<dyn Error>> {
    let enabling = matches!(target, np_core::config_write::EmbeddingsTarget::On { .. });
    let path = np_core::config::config_file_path().ok_or_else(|| -> Box<dyn Error> {
        "np cannot tell where your config file belongs: set $XDG_CONFIG_HOME or $HOME (Windows: %APPDATA%) so \
         note-processor/config.toml has a place to live — refusing to write one somewhere surprising"
            .into()
    })?;

    // Every refusal this returns is np's own sentence about its own rules (`local-model-not-built`, an unpinned id, a config
    // that does not load today), and the same sentence reaches `serve` as `-32000`: one fact, one wording, two doors.
    let write = np_core::config_write::set_embeddings(&path, target, surface)?;

    // What the run will see **now**, through the loader every command uses — not what this call asked for. The writer refuses
    // if the two disagree, so reaching here means they match; reading it anyway is what keeps this honest if that ever stops
    // being true. `None` rather than `Some(&path)` because an installation with no config file is a real state this door
    // answers from (disabling on one writes nothing), and "no file" means np's defaults, which is exactly what the next run uses.
    let config = Config::load(None)?;
    let provider = match config.embeddings.provider {
        EmbeddingProvider::LocalModel => "local-model",
        EmbeddingProvider::OpenAiCompatible => "openai-compatible",
        EmbeddingProvider::None => "none",
    };
    let model = config.embeddings.model.clone();
    let document = embeddings_document(
        &write.changed,
        provider,
        model.as_deref(),
        write.persisted_to.as_deref(),
    );
    Ok(EmbeddingsOutcome {
        document,
        changed: write.changed,
        enabling,
        provider: provider.to_string(),
        model,
    })
}

/// `np embeddings enable|disable`'s own channel: the same facts serve answers with, in sentences.
pub(crate) fn embeddings_run(
    target: np_core::config_write::EmbeddingsTarget<'_>,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let outcome = embeddings_apply(target, np_core::config_write::WriteSurface::Cli)?;
    if json {
        emit_json(&outcome.document);
        return Ok(());
    }
    let path_shown = outcome
        .document
        .get("persisted_to")
        .and_then(Value::as_str)
        .unwrap_or("your config file")
        .to_string();

    if outcome.changed.is_empty() {
        // The idempotent answer, said as the fact it is: nothing moved, and the file that already carries the decision is
        // named, so a person can look instead of trusting.
        emit(&match (outcome.enabling, outcome.model.as_deref()) {
            (true, Some(model)) => format!(
                "embeddings are already on with model \"{model}\" — nothing changed in {path_shown}"
            ),
            _ => format!("embeddings are already off — nothing changed in {path_shown}"),
        });
        return Ok(());
    }

    emit(&format!(
        "wrote {} to {path_shown}",
        outcome.changed.join(", ")
    ));
    if outcome.enabling {
        emit(&format!(
            "embeddings on: provider = \"{}\", model = \"{}\"",
            outcome.provider,
            outcome.model.as_deref().unwrap_or("none")
        ));
        if let Some(model) = outcome.model.as_deref() {
            // The price, named before anything is fetched (#3's rule that the cost travels with the consent), and the fact
            // that this command itself downloaded nothing.
            emit(&format!(
                "nothing has been downloaded yet: the first `np embed` fetches {} ({}) and checks its digest",
                size_human(np_core::pins::plan_size(model)),
                np_core::pins::members_for_build().join(", ")
            ));
        }
    } else {
        emit(&format!(
            "embeddings off: provider = \"{}\"",
            outcome.provider
        ));
        if let Some(model) = outcome.model.as_deref() {
            // Saying plainly what np did *not* touch, because the reason someone revokes consent is often "stop talking about
            // this model", and a file that still names it should not surprise them.
            emit(&format!(
                "model = \"{model}\" is still in the file; remove that key yourself if you want it gone"
            ));
        }
    }
    Ok(())
}

/// The document both doors answer, built once so `np embeddings enable --json` and serve's `embeddings.enable` cannot drift.
///
/// `provider` and `model` are what the file says **after** the call rather than what was asked for — the caller's question is
/// "what is my setup now?", and a reload that disagreed would have refused before this was built.
pub(crate) fn embeddings_document(
    changed: &[String],
    provider: &str,
    model: Option<&str>,
    persisted_to: Option<&Path>,
) -> Value {
    // The price of the thing consent was about, from the same pin table as `models list`'s `pinned` and the terminal prompt
    // (#3), so three surfaces quote one number. **Priced only when an embed would actually download**: a disabled config that
    // still names a model (`embeddings.disable` leaves it) has no fetch to price, and quoting 309 MB beside
    // `provider: "none"` would be a cost the next run will not incur.
    let fetch = match (provider, model) {
        ("local-model", Some(id)) => Some(id),
        _ => None,
    }
    .map(|id| {
        json!({
            "bytes_total": np_core::pins::plan_size(id),
            "members": np_core::pins::members_for_build(),
        })
    });
    json!({
        // **The keys np wrote**, not the keys involved in the request: an empty list means nothing was written, and switching
        // one model for another lists only `embeddings.model` (#7's "answering *Allow* twice must not append a second
        // `[embeddings]`", made visible rather than merely true).
        "changed": changed,
        "provider": provider,
        "model": model,
        "fetch": fetch,
        // Which file now carries the decision — the same path About and diagnostics name, because a person goes there to
        // check what an application did. `null` only when nothing resolved a config path *and* nothing needed writing.
        "persisted_to": persisted_to.map(|path| path.display().to_string()),
    })
}
