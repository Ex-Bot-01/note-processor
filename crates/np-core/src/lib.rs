//! Core of note-processor: workspace discovery, catalog rendering.

pub mod config;

/// Milestone version this crate implements, taken from the workspace version.
pub const M_VERSION: &str = env!("CARGO_PKG_VERSION");
