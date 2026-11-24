//! One module per command, plus [`common`] for the helpers several of them share.
//!
//! `main.rs` parses argv and dispatches; the functions here do the work and print what came back. A helper
//! belongs in the module of the command that owns its meaning, and moves to [`common`] only when a second
//! command starts calling it — which is why the lock, the config load and the stdout writers live there and
//! the provider resolution does not: only `embed` and `search` ask that question, and `embed` owns the answer.
//!
//! The JSON-RPC server keeps its own top-level module ([`crate::serve`]): it is a protocol with its own framing
//! and lifecycle, not a command body, and `np serve`'s dispatch is one line of [the entry point](crate).

pub(crate) mod common;
pub(crate) mod embed;
pub(crate) mod embeddings;
pub(crate) mod hold;
pub(crate) mod models;
pub(crate) mod rerank;
pub(crate) mod scan;
pub(crate) mod search;
pub(crate) mod setup;
