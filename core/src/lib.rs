//! csgraph-core — the csgraph C# relation-graph engine, in Rust.
//!
//! This is a faithful port of the frozen Python engine in `tools/csgraph/`:
//!
//! | Python            | Rust                        |
//! |-------------------|-----------------------------|
//! | `cs_parser.py`    | [`lex`], [`parser`], [`graph`] |
//! | `cs_doc.py`       | [`doc`]                     |
//! | `cs_reorg.py`     | [`reorg`]                   |
//! | `server.py`       | [`index`], [`api`], [`config`], [`cache`] |
//!
//! The port is verified differentially: `tools/oracle.py` dumps everything the
//! Python engine knows into one canonical JSON document and the `csgraph` CLI
//! dumps the same document from this crate; `tools/diff_clone.py` compares
//! them. "A clone" here means byte-identical answers, not a reimplementation
//! that merely looks similar.

pub mod lex;
pub mod model;
pub mod parser;
pub mod graph;
pub mod hash;
pub mod cache;
pub mod doc;
pub mod reorg;
pub mod config;
pub mod guard;
pub mod index;
pub mod registry;
pub mod api;
pub mod mcp;
pub mod edit;
pub mod compile_check;
pub mod dedupe;
pub mod reshape;
pub mod preview;
pub mod viewer;

/// The engine version, reported by `/api/state` and the CLI.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
