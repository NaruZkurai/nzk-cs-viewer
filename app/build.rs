//! Tauri build hook for the CS Viewer host.
//!
//! `tauri_build::build()` emits `cargo:rerun-if-changed` for `tauri.conf.json`
//! and the capabilities directory, but **not** for the frontend dist. Tauri
//! embeds that dist into the binary through `tauri::generate_context!` at
//! compile time, so without an explicit instruction here a frontend-only edit
//! (arrange.js, interaction.js, index.html, …) leaves the crate fingerprint
//! unchanged: cargo prints "Finished" without recompiling and the AppImage keeps
//! serving the *previous* frontend from the old binary. That is the failure mode
//! where a fix looks like it was never built.
//!
//! Emitting the dist directory alone is not enough — cargo watches a directory's
//! own mtime, which does not change when an existing file is edited — so every
//! file in the tree is listed.

use std::path::Path;

/// Emit `cargo:rerun-if-changed` for `dir` and every file and directory under it.
fn track_tree(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    println!("cargo:rerun-if-changed={}", dir.display());
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            track_tree(&path);
        } else {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

fn main() {
    tauri_build::build();

    // `frontendDist` in app/tauri.conf.json is "../frontend", relative to this
    // package directory.
    let frontend = Path::new("../frontend");
    if frontend.exists() {
        track_tree(frontend);
    }
}
