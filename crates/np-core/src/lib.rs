//! Core of note-processor: workspace discovery, catalog rendering.

pub mod change;
pub mod config;
pub mod db;
pub mod hash;
pub mod links;
pub mod parse;
pub mod walk;

/// Milestone version this crate implements, taken from the workspace version.
pub const M_VERSION: &str = env!("CARGO_PKG_VERSION");
