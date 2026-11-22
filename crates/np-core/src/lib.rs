//! Core of note-processor: workspace discovery, catalog rendering.
//!
//! **Breaking for embedders:** [`embed_run`]`::{embed, embed_with_resolution,
//! embed_with_inject}` take `&mut dyn Embedder`, because [`embed::Embedder::embed_image`] may have to open a projector and
//! that is not something `&self` can do without interior mutability over C handles. Every in-tree caller already passes
//! mutable; an out-of-tree caller holding a shared `&dyn Embedder`, or reusing one provider across two runs through a
//! shared reference, needs the same change. There is no CHANGELOG in this tree, so this is where the note lives.

// **The unsafe rules are lints, not a convention.** This crate holds every `unsafe` site in the program: hand-written
// llama.cpp declarations (`llama_ffi`), the loader and decoder (`local_model`, `vision`). Two properties have to hold at
// each one — that an `unsafe fn`'s body is still checked rather than implicitly trusted, and that a human wrote down why
// the preconditions hold — and neither survives contact with a large FFI surface as a code-review custom. So both are
// denied at crate level, for every feature combination, with no `allow` anywhere in the tree: a gate that only bites when
// `local-model` is configured is a gate that stays green while the code that can corrupt memory changes.
#![deny(unsafe_op_in_unsafe_fn, clippy::undocumented_unsafe_blocks)]

pub mod catalog;
pub mod change;
pub mod chunk;
pub mod config;
pub mod config_write;
pub mod db;
pub mod diagnostics;
pub mod embed;
pub mod embed_run;
pub mod fetch;
pub mod hash;
pub mod index_names;
pub mod items;
// The log-level vocabulary behind `[embeddings] log_level`: names, gate and precedence, deliberately ungated so a build with no
// runtime still parses the same config as one with it. Installing anything is `local_model`'s business, not this file's.
pub mod links;
pub mod lock;
pub mod loglevel;
// Media discovery is pure Rust over bytes with no runtime behind it, so it carries no feature gate: the resolver
// runs on every `np embed` and refuses with a class, never with a model.
pub mod media;
// The FFI mirror of the pinned llama.cpp header, compiled only when something exists to link it to:
// `build.rs` hands the linker four static archives under this feature and refuses to build a source
// tree that is not the pin.
#[cfg(feature = "local-model")]
pub mod llama_ffi;
// The one place that opens a GGUF: it needs the archives `build.rs` links, so it shares `llama_ffi`'s gate.
#[cfg(feature = "local-model")]
pub mod local_model;
pub mod models;
pub mod parse;
pub mod path_keys;
// Image vectors: a projector loaded against an already loaded model, so this cannot exist without `local-model`.
// Off by default — it costs two extra archives at link time and a wider attack surface for no benefit to a text-only
// build.
#[cfg(feature = "vision")]
pub mod vision;
// The pin table is data with no dependency and no I/O, so it carries no feature gate: the fetch engine behind it
// arrives later, and a pin a build could compile out would pin nothing.
pub mod pins;
// What a long-running command is doing right now, as data rather than prose. No JSON and no clock here on purpose:
// np-core has no serde_json dependency, and the throttle that keeps a status stream from becoming the traffic is a
// property of the channel printing it.
pub mod progress;
// How a (query, document) pair is assembled, what fits the scoring window, and how scores become an order. Ungated for
// the reason `pins` is: "reranking is off", "this build has no runtime" and "the model is not installed" are sentences
// about definitions this module owns, and a text-only build has to be able to say all three.
pub mod rerank;
// The read side of the index: one cosine pass over both vector kinds, with no write anywhere in its path
// No feature gate — it reads bytes the schema already holds, so a text-only
// build can search the images another build embedded.
pub mod scan;
pub mod search;
pub mod walk;

// The scan pipeline is the crate's entry point, so its three names are reachable without the module
// path. `scan` as a function and `scan` as a module live in different namespaces, which is what lets
// `np_core::scan` mean the call.
pub use scan::{scan, scan_with_inject, ScanError, ScanReport};

/// Milestone version this crate implements, taken from the workspace version.
pub const M_VERSION: &str = env!("CARGO_PKG_VERSION");
