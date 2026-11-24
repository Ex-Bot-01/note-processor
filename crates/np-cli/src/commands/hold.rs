//! Loaded models kept between requests, for the one caller that lives long enough to ask twice: `np serve`.
//!
//! A one-shot `np search` builds its provider, loads the weights on first use and exits, so there is nothing to keep. A
//! server answers many questions, and before this module every `search` paid the whole load again — the embedding model
//! (~300 MB) and, with a rerank stage, the reranker (~635 MB) — because the provider, and the cell holding its weights,
//! belonged to the request. This module holds them per thread instead, and hands each request a shared handle.
//!
//! **What makes a held model reusable is the same rule the reranker's digest cache already follows:** the artifact's
//! path, size and modification time, plus every input that shapes the loaded context (row identity, width, token
//! budget, thread policy, projector). Any of those moving means a fresh load, and the stale one is dropped when it is
//! replaced, so a re-installed or re-fetched artifact is never answered by the bytes it replaced.
//!
//! **Per thread, not per process,** because a llama.cpp context documents no thread-safety and none of the loaded types
//! is `Send`. `np serve` runs every model-touching request on one worker thread, so per thread is per server.
//!
//! **Off unless a caller turns it on.** The CLI never enables it: a process that exits after one answer would only
//! delay freeing memory it is about to free anyway.

use std::cell::RefCell;
#[cfg(feature = "local-model")]
use std::path::{Path, PathBuf};
#[cfg(feature = "local-model")]
use std::time::SystemTime;

/// A file as the hold sees it: where it is, how long it is, and when it last changed. Two stamps that differ in any part
/// are two different artifacts.
#[cfg(feature = "local-model")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileStamp {
    path: PathBuf,
    size: u64,
    mtime: Option<SystemTime>,
}

#[cfg(feature = "local-model")]
impl FileStamp {
    /// Read the stamp, or `None` when the file cannot be stat'ed — in which case nothing may be reused for it.
    pub(crate) fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        Some(Self {
            path: path.to_path_buf(),
            size: meta.len(),
            mtime: meta.modified().ok(),
        })
    }
}

/// How a model reached a request: loaded now, or already held from an earlier one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Acquired {
    /// `true` when the weights were already resident and nothing was read from disk.
    pub(crate) reused: bool,
    /// Time spent loading for this request; zero when `reused`.
    pub(crate) load_ms: u64,
}

#[derive(Default)]
struct Hold {
    enabled: bool,
    #[cfg(feature = "local-model")]
    embedder: Option<local::HeldEmbedder>,
    #[cfg(feature = "local-model")]
    ranker: Option<local::HeldRanker>,
}

thread_local! {
    static HOLD: RefCell<Hold> = RefCell::new(Hold::default());
}

/// Keep models on this thread from now on.
pub(crate) fn enable() {
    HOLD.with(|hold| hold.borrow_mut().enabled = true);
}

/// Drop every held model and name what was resident. A request still holding a handle keeps its model alive until it
/// finishes; the memory goes back when that handle drops.
pub(crate) fn release() -> Vec<String> {
    let names = held();
    #[cfg(feature = "local-model")]
    HOLD.with(|hold| {
        let mut hold = hold.borrow_mut();
        hold.embedder = None;
        hold.ranker = None;
    });
    names
}

/// The ids whose weights are resident on this thread right now. A provider that is held but has not loaded yet (a
/// hybrid search over a workspace with no vectors never embeds its query) is not resident and is not named.
pub(crate) fn held() -> Vec<String> {
    #[cfg(feature = "local-model")]
    {
        HOLD.with(|hold| {
            let hold = hold.borrow();
            let mut names = Vec::new();
            if let Some(embedder) = &hold.embedder {
                if embedder.provider.borrow().loaded() {
                    names.push(embedder.id.clone());
                }
            }
            if hold.ranker.is_some() {
                names.push(np_core::pins::RERANKER_ID.to_string());
            }
            names
        })
    }
    #[cfg(not(feature = "local-model"))]
    Vec::new()
}

/// Make the held embedder resident now. `None` when nothing is held — a build without the runtime, or a thread that does
/// not hold models.
pub(crate) fn warm_embedder() -> Option<Result<Acquired, np_core::embed::EmbedError>> {
    #[cfg(feature = "local-model")]
    {
        HOLD.with(|hold| {
            let hold = hold.borrow();
            let provider = hold.embedder.as_ref()?.provider.borrow();
            if provider.loaded() {
                return Some(Ok(Acquired {
                    reused: true,
                    load_ms: 0,
                }));
            }
            let started = std::time::Instant::now();
            Some(provider.warm().map(|()| Acquired {
                reused: false,
                load_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            }))
        })
    }
    #[cfg(not(feature = "local-model"))]
    None
}

#[cfg(feature = "local-model")]
pub(crate) use local::{embedder, ranker, EmbedKey};

#[cfg(feature = "local-model")]
mod local {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Instant;

    use np_core::config::ThreadCount;
    use np_core::db::ModelRow;
    use np_core::embed::{EmbedError, Embedder, Vector};
    use np_core::local_model::{LazyLocalEmbedder, LoadedRanker};
    use np_core::rerank::{RerankError, Reranker};

    use super::{Acquired, FileStamp, HOLD};

    /// Everything that shapes a loaded embedding context. The row contributes only what describes the model — not the
    /// verification stamp, which the first request rewrites and which would otherwise make the second request miss.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct EmbedKey {
        artifact: FileStamp,
        id: String,
        row_dim: i64,
        sha256: Option<String>,
        prefix_doc: String,
        prefix_query: String,
        dim: usize,
        max_tokens: usize,
        threads: ThreadCount,
        query_threads: ThreadCount,
        mmproj: Option<FileStamp>,
    }

    impl EmbedKey {
        /// `None` when the artifact cannot be stat'ed: such a provider is never shared.
        pub(crate) fn new(
            artifact: &std::path::Path,
            row: &ModelRow,
            dim: usize,
            max_tokens: usize,
            threads: ThreadCount,
            query_threads: ThreadCount,
            mmproj: Option<&std::path::Path>,
        ) -> Option<Self> {
            Some(Self {
                artifact: FileStamp::of(artifact)?,
                id: row.id.clone(),
                row_dim: row.dim,
                sha256: row.sha256.clone(),
                prefix_doc: row.prefix_doc.clone(),
                prefix_query: row.prefix_query.clone(),
                dim,
                max_tokens,
                threads,
                query_threads,
                mmproj: match mmproj {
                    Some(path) => Some(FileStamp::of(path)?),
                    None => None,
                },
            })
        }
    }

    pub(super) struct HeldEmbedder {
        key: EmbedKey,
        pub(super) id: String,
        pub(super) provider: Rc<RefCell<LazyLocalEmbedder>>,
    }

    pub(super) struct HeldRanker {
        stamp: FileStamp,
        ranker: Rc<RefCell<LoadedRanker>>,
    }

    /// The provider a request gets: `fresh` itself when nothing is held, or a shared handle to the held one.
    ///
    /// `warm` loads the weights now instead of at the first query — what a caller wants right after bytes arrived, so the
    /// person's first search does not pay for a load that could have happened while they were reading the download bar.
    /// A warm-up that fails is not reported here: the same failure comes back, typed, from the first real call.
    pub(crate) fn embedder(
        key: Option<EmbedKey>,
        fresh: LazyLocalEmbedder,
        warm: bool,
    ) -> Box<dyn Embedder> {
        HOLD.with(|hold| {
            let mut hold = hold.borrow_mut();
            let Some(key) = key.filter(|_| hold.enabled) else {
                return Box::new(fresh) as Box<dyn Embedder>;
            };
            let reuse = hold.embedder.as_ref().is_some_and(|held| held.key == key);
            if !reuse {
                hold.embedder = Some(HeldEmbedder {
                    id: fresh.model_id().to_string(),
                    key,
                    provider: Rc::new(RefCell::new(fresh)),
                });
            }
            let held = hold
                .embedder
                .as_ref()
                .unwrap_or_else(|| unreachable!("set just above"));
            if warm {
                let _ = held.provider.borrow().warm();
            }
            let dim = held.provider.borrow().dim();
            let shared: Box<dyn Embedder> = Box::new(SharedEmbedder {
                id: held.id.clone(),
                dim,
                provider: Rc::clone(&held.provider),
            });
            shared
        })
    }

    /// The ranker for `stamp`, loaded by `load` only when nothing usable is held, scoring pairs of at most
    /// `pair_budget` tokens for this request.
    pub(crate) fn ranker(
        stamp: Option<FileStamp>,
        pair_budget: usize,
        load: impl FnOnce() -> Result<LoadedRanker, EmbedError>,
    ) -> Result<(Box<dyn Reranker>, Acquired), EmbedError> {
        let held = HOLD.with(|hold| {
            let hold = hold.borrow();
            match (&hold.ranker, &stamp) {
                (Some(held), Some(stamp)) if hold.enabled && &held.stamp == stamp => {
                    Some(Rc::clone(&held.ranker))
                }
                _ => None,
            }
        });
        if let Some(ranker) = held {
            ranker.borrow_mut().set_pair_budget(pair_budget);
            return Ok((
                Box::new(SharedRanker(ranker)),
                Acquired {
                    reused: true,
                    load_ms: 0,
                },
            ));
        }
        let started = Instant::now();
        let mut loaded = load()?;
        loaded.set_pair_budget(pair_budget);
        let acquired = Acquired {
            reused: false,
            load_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        };
        let keep = HOLD.with(|hold| hold.borrow().enabled) && stamp.is_some();
        if !keep {
            return Ok((Box::new(loaded), acquired));
        }
        let shared = Rc::new(RefCell::new(loaded));
        HOLD.with(|hold| {
            hold.borrow_mut().ranker = stamp.map(|stamp| HeldRanker {
                stamp,
                ranker: Rc::clone(&shared),
            });
        });
        Ok((Box::new(SharedRanker(shared)), acquired))
    }

    /// A request's view of a held embedder. The id and width are copied out because the trait hands back borrows that a
    /// `RefCell` guard could not outlive.
    struct SharedEmbedder {
        id: String,
        dim: usize,
        provider: Rc<RefCell<LazyLocalEmbedder>>,
    }

    impl Embedder for SharedEmbedder {
        fn model_id(&self) -> &str {
            &self.id
        }

        fn dim(&self) -> usize {
            self.dim
        }

        fn model_inputs(&self, texts: &[&str]) -> Vec<String> {
            self.provider.borrow().model_inputs(texts)
        }

        fn embed(&self, texts: &[&str]) -> Result<Vec<Vector>, EmbedError> {
            self.provider.borrow().embed(texts)
        }

        fn embed_query(&self, text: &str) -> Result<Vector, EmbedError> {
            self.provider.borrow().embed_query(text)
        }

        #[cfg(feature = "vision")]
        fn supports_images(&self) -> bool {
            self.provider.borrow().supports_images()
        }

        #[cfg(feature = "vision")]
        fn image_blocker(&self) -> Option<np_core::embed::ImageBlocker> {
            self.provider.borrow().image_blocker()
        }

        #[cfg(feature = "vision")]
        fn embed_image(&mut self, bytes: &[u8]) -> Result<Vector, EmbedError> {
            self.provider.borrow_mut().embed_image(bytes)
        }
    }

    struct SharedRanker(Rc<RefCell<LoadedRanker>>);

    impl Reranker for SharedRanker {
        fn score(&mut self, query: &str, docs: &[&str]) -> Result<Vec<f64>, RerankError> {
            self.0.borrow_mut().score(query, docs)
        }
    }
}
