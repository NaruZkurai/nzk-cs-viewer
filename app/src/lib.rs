//! Library half of `native-csviewer`.
//!
//! `main.rs` keeps its own module tree for the window binary. This library
//! exists so the auxiliary bins (`source_shot`) can reach the shared pane code
//! (`app::source_pane`, `app::codelens_wire`, `model`) through the crate instead
//! of `#[path]`-including a subset of files that no longer compiles on its own
//! (`source_pane` now depends on `crate::app::codelens_wire` and `crate::model`).
//!
//! The module roots below are the same set `main.rs` declares; a new module must
//! be added to both (the compile fails loudly on whichever side is missed). The
//! window-facing modules stay private here — only `app` is public, and the
//! auxiliary bin consumes `app::source_pane`.

pub mod app;

mod actions;
mod blocks_peek;
mod chords;
mod control;
mod doc_engine;
mod drawer_wire;
mod edit_args;
mod edit_host;
mod edit_transport;
mod edit_wire;
mod find_wire;
mod form_wire;
mod graphviz;
mod model;
mod outline;
mod picker;
mod pov;
mod preview_wire;
mod render;
mod reorg_wire;
mod search;
mod tokens;
mod typography;
mod view;
mod windows;
