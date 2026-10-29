//! Core of note-processor: workspace discovery, catalog rendering.

pub mod catalog;
pub mod change;
pub mod chunk;
pub mod config;
pub mod db;
pub mod embed;
pub mod hash;
pub mod index_names;
pub mod items;
pub mod links;
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
