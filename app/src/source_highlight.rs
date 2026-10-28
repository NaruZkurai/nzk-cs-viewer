//! C# highlighting for the native source pane.
//!
//! The implementation moved to the IDE module (`nzkide_module`), where the
//! shared tokenizer is used and the `head`/`tail` comment-colour bug was fixed.
//! This re-export keeps the host's existing `source_highlight::Tok` /
//! `highlight_document` call sites and the host tests pointed at one table.
pub use nzkide_module::highlight_nzkide::*;
