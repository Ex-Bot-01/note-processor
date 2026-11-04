//! Core of note-processor: workspace discovery, catalog rendering.
//!
//! **Breaking for embedders, as of R7c (2026-11-03):** [`embed_run`]`::{embed, embed_with_resolution,
//! embed_with_inject}` take `&mut dyn Embedder`, because [`embed::Embedder::embed_image`] may have to open a projector and
//! that is not something `&self` can do without interior mutability over C handles. Every in-tree caller already passes
//! mutable; an out-of-tree caller holding a shared `&dyn Embedder`, or reusing one provider across two runs through a
//! shared reference, needs the same change. There is no CHANGELOG in this tree, so this is where the note lives.

pub mod catalog;
pub mod change;
pub mod chunk;
pub mod config;
pub mod db;
pub mod embed;
pub mod embed_run;
pub mod fetch;
pub mod hash;
pub mod index_names;
pub mod items;
pub mod links;
// Media discovery is pure Rust over bytes with no runtime behind it, so it carries no feature gate: the resolver
// runs on every `np embed` and refuses with a class, never with a model (`docs/design-m2-image-embeddings.md` §7).
pub mod media;
// The FFI mirror of the pinned llama.cpp header, compiled only when something exists to link it to:
// `build.rs` hands the linker four static archives under this feature and refuses to build a source
// tree that is not the pin (`docs/design-m2-local-embeddings.md` §14).
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
// build (`docs/design-m2-image-embeddings.md` §10).
#[cfg(feature = "vision")]
pub mod vision;
// The pin table is data with no dependency and no I/O, so it carries no feature gate: the fetch engine behind it
// arrives later (§12 rule 7), and a pin a build could compile out would pin nothing.
pub mod pins;
// The read side of the index: one cosine pass over both vector kinds, with no write anywhere in its path
// (`docs/design-m2-image-embeddings.md` §12). No feature gate — it reads bytes the schema already holds, so a text-only
// build can search the images another build embedded.
pub mod search;
pub mod scan;
pub mod walk;

// The scan pipeline is the crate's entry point, so its three names are reachable without the module
// path. `scan` as a function and `scan` as a module live in different namespaces, which is what lets
// `np_core::scan` mean the call.
pub use scan::{scan, scan_with_inject, ScanError, ScanReport};

/// Milestone version this crate implements, taken from the workspace version.
pub const M_VERSION: &str = env!("CARGO_PKG_VERSION");
