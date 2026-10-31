//! Core of note-processor: workspace discovery, catalog rendering.

pub mod catalog;
pub mod change;
pub mod chunk;
pub mod config;
pub mod db;
pub mod embed;
pub mod embed_run;
pub mod hash;
pub mod index_names;
pub mod items;
pub mod links;
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
pub mod scan;
pub mod walk;

// The scan pipeline is the crate's entry point, so its three names are reachable without the module
// path. `scan` as a function and `scan` as a module live in different namespaces, which is what lets
// `np_core::scan` mean the call.
pub use scan::{scan, scan_with_inject, ScanError, ScanReport};

/// Milestone version this crate implements, taken from the workspace version.
pub const M_VERSION: &str = env!("CARGO_PKG_VERSION");
