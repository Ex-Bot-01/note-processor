//! Turning `[search] rerank` / `--rerank` into something that can score a pair (**F51**).
//!
//! This is the *door*, not the stage: it decides whether a reranker was asked for, what it was asked to read, and whether one
//! can actually run here — then hands the stage a scorer or a reason. Two properties shape every line in this file:
//!
//! * **`np search` never fetches a reranker.** No transport is constructed on any path here, so `[models] auto_fetch = true`
//!   cannot volunteer 635 MB because a ranking preference was set. The artifact arrives through `np models install`, or not at
//!   all — see `docs/reference-models.md`.
//! * **Unverified bytes never reach a forward pass.** A file whose length the pin can already rule out is refused for free; one
//!   of the right length is hashed before it scores anything. Wrong bytes producing plausible-looking order is the failure this
//!   command cannot detect afterwards, so it detects it beforehand.
//!
//! In a build without `local-model` the same entry point returns [`Opened::Declined`] with
//! [`RerankNote::RuntimeNotBuilt`], which is what lets one code path serve both feature shapes: the flag and the config key mean
//! the same thing everywhere, and only what happens here differs.

use std::path::{Path, PathBuf};

use np_core::config::Config;
use np_core::pins::RERANKER_ID;
use np_core::rerank::Reranker;
use np_core::search::{RerankAsk, RerankNote, RerankText};

/// What the human asked for, after config and flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Plan {
    /// Configuration or a flag — decides whether a decline warns or refuses.
    pub(crate) ask: RerankAsk,
    /// How many of the top hits to score.
    pub(crate) top_n: usize,
    /// Which text of a note the model reads.
    pub(crate) text: RerankText,
}

/// Resolve the request: `[search] rerank*` is the standing choice and each flag is the exception to it — in **both**
/// directions, which is why `--no-rerank` exists beside `--rerank`. A machine that turns reranking on for its own vault should
/// still be able to ask one question the old way, and a person who enabled it globally must be able to measure the difference.
///
/// `None` means no stage: nothing loads, nothing hashes, and the answer is retrieval order with `disabled` in the document.
pub(crate) fn plan(cfg: &Config, rerank: Option<bool>, text: Option<RerankText>) -> Option<Plan> {
    let on = rerank.unwrap_or(cfg.search.rerank);
    if !on {
        return None;
    }
    Some(Plan {
        // `Some(true)` from a flag is an expectation stated for this run; anything else is the standing preference. Note that
        // `--rerank` when config already says true stays `Explicit`: what matters is that *this invocation* named it.
        ask: if rerank == Some(true) {
            RerankAsk::Explicit
        } else {
            RerankAsk::ByConfig
        },
        top_n: cfg.search.rerank_top_n,
        text: text.unwrap_or(cfg.search.rerank_text),
    })
}

/// A scorer that can run, or the reason it cannot.
///
/// `Ready` is constructed only by a build with `local-model`, which is why it carries an allow rather than a cfg: the *shape* is
/// the same in both shapes — callers match on both arms either way — and cfg-ing the variant would force every caller to do the
/// same feature test the door exists to hide.
#[allow(dead_code)]
pub(crate) enum Opened {
    /// Verified bytes, loaded context. Boxed because the stage holds a `dyn Reranker` either way, and a build without the
    /// runtime has nothing to box but the reason.
    Ready(Box<dyn Reranker>),
    /// The stage declines, with one cause that carries its own sentence, token and fix.
    Declined(RerankNote),
}

/// Where `np models install --id bge-reranker-v2-m3` put the artifact for this workspace: `<models_dir>/<id>`, the directory
/// that already holds the embedding model.
///
/// Same rule as every other pinned member rather than a machine-wide directory of its own: one place where np's artifacts live
/// means `np models list` names it, the disk preflight bounds it, and an operator deleting one directory frees both. The cost is
/// that two vaults on one machine hold two copies of 635 MB — accepted deliberately, because the alternative (a shared path with
/// no workspace owner) would make uninstalling a workspace's models a judgement about somebody else's vault.
pub(crate) fn artifact_path(cfg: &Config, workspace: &Path) -> PathBuf {
    cfg.workspace_models_dir(workspace).join(RERANKER_ID)
}

#[cfg(feature = "local-model")]
mod verify {
    use np_core::pins::RERANKER_ID;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::SystemTime;

    /// The verdict of one verification, remembered for the process.
    #[derive(Debug, Clone)]
    pub(super) struct Verdict {
        /// These exact bytes, at this size and stamp, are the pinned digest.
        pub(super) size: u64,
        pub(super) mtime: Option<SystemTime>,
    }

    /// Verification verdicts by path, so a long-lived `np serve` hashes 635 MB once instead of once per query.
    ///
    /// Keyed by path and re-checked against **size and mtime**: an artifact replaced or truncated between two searches has a
    /// different stamp, and the new bytes get hashed rather than inheriting the old verdict. A same-size, same-mtime swap is not
    /// detectable from metadata and is not claimed to be — that would take a deliberate atomic rename preserving both, and
    /// `np models install` refuses to leave such a file in place in the first place.
    static VERIFIED: Mutex<Vec<(PathBuf, Verdict)>> = Mutex::new(Vec::new());

    pub(super) fn verified(path: &Path, size: u64, mtime: Option<SystemTime>) -> bool {
        let Ok(table) = VERIFIED.lock() else {
            // A poisoned lock means another thread panicked *inside* a hash, not that the bytes are wrong. Recomputing is always
            // safe and costs one pass; answering "unverified" would refuse a good artifact, and answering "verified" would be the
            // exact failure this function exists to prevent.
            return verify_now(path);
        };
        if let Some(known) = table.iter().find(|(known_path, _)| known_path == path) {
            if known.1.size == size && known.1.mtime == mtime {
                return true;
            }
        }
        drop(table);
        if !verify_now(path) {
            return false;
        }
        if let Ok(mut table) = VERIFIED.lock() {
            table.retain(|(known_path, _)| known_path != path);
            table.push((path.to_path_buf(), Verdict { size, mtime }));
        }
        true
    }

    /// One pass over the bytes against the pinned digest, using np-core's one digest implementation — this file defines no second
    /// SHA-256, which is the same fence `np_core::models` documents for installs.
    fn verify_now(path: &Path) -> bool {
        let Some(pin) = np_core::pins::pinned_member(RERANKER_ID, "main") else {
            return false;
        };
        match np_core::hash::sha256_file(path) {
            Ok(hex) => hex.eq_ignore_ascii_case(pin.sha256),
            Err(_) => false,
        }
    }
}

/// Open the scorer the plan asked for.
///
/// Three checks before any model loads, cheapest first, because the expensive one is the last: **present**, **the pinned
/// length** (a `stat`, which rules out a truncated download without reading it), then **the pinned digest** — once per process,
/// re-validated against size and mtime. A mismatch is reported as
/// [`RerankNote::ArtifactUnverified`] rather than loaded "anyway": the failure mode worth preventing is not a crash but a wrong
/// order that looks like a good answer.
pub(crate) fn open(cfg: &Config, workspace: &Path, _plan: &Plan) -> Opened {
    open_at(&artifact_path(cfg, workspace))
}

#[cfg(feature = "local-model")]
fn open_at(path: &Path) -> Opened {
    use std::fs;

    let meta = match fs::metadata(path) {
        Ok(meta) if meta.is_file() => meta,
        Ok(_) => return Opened::Declined(RerankNote::ArtifactAbsent),
        Err(_) => return Opened::Declined(RerankNote::ArtifactAbsent),
    };
    let Some(pin) = np_core::pins::pinned_member(RERANKER_ID, "main") else {
        // A build that pins no reranker cannot run one. Not reachable today (the row is compiled in); refusing rather than
        // unwrapping keeps it a sentence instead of a panic if a future build drops the pin.
        return Opened::Declined(RerankNote::ArtifactAbsent);
    };
    if meta.len() != pin.size_bytes {
        return Opened::Declined(RerankNote::ArtifactUnverified(format!(
            "{} is {} bytes, this build pins {}",
            path.display(),
            meta.len(),
            pin.size_bytes
        )));
    }
    if !verify::verified(path, meta.len(), meta.modified().ok()) {
        return Opened::Declined(RerankNote::ArtifactUnverified(format!(
            "{} does not hash to the pinned digest",
            path.display()
        )));
    }
    match np_core::local_model::load_rank(path) {
        Ok(ranker) => Opened::Ready(Box::new(ranker)),
        // Anything the runtime refuses at load is, from here, "the bytes on disk are not a usable ranker" — which is what
        // `ArtifactUnverified` means to a reader, and its sentence names `np models install` as the fix.
        Err(error) => Opened::Declined(RerankNote::ArtifactUnverified(error.to_string())),
    }
}

/// The shape without the runtime: nothing on disk can be loaded, so the cause is this binary rather than the machine's files.
///
/// Reported by the same type as every other decline, which is what lets `--rerank` exit 1 here with a sentence that says *rebuild
/// with the feature* instead of the misleading "install the model" that would send a reader to look at their disk.
#[cfg(not(feature = "local-model"))]
fn open_at(_path: &Path) -> Opened {
    Opened::Declined(RerankNote::RuntimeNotBuilt)
}
