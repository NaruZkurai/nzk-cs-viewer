//! Compile-time build stamp for the window title bar.
//!
//! Emits `CSVIEWER_BUILD`, read back with `env!("CSVIEWER_BUILD")` in
//! `src/buildinfo.rs`. Nothing is read at runtime, so a stale file can never be
//! shown; the value is baked into the binary by rustc.
//!
//! The selection policy lives in `build_stamp.rs`, declared here as a module
//! (and as `../build_stamp.rs` from `src/buildinfo.rs`) so the same pure code
//! is unit-tested from the crate. `cargo test` never runs a build script's own
//! tests — that gap is how a silent fallback shipped once.
//!
//! Rebuild churn: the stamp is a pure function of committed state (or of an
//! explicit override), so repeating `cargo build` on an unchanged tree reuses
//! the cached value. `cargo:rerun-if-changed=src` makes the stamp refresh when
//! sources change (including the `-dirty` flag), and the `rerun-if-env-changed`
//! lines make an override take effect immediately.

// A module (not `include!`): `build_stamp.rs` starts with `//!` inner docs, and
// an included file pasted mid-crate may not.
#[path = "build_stamp.rs"]
mod build_stamp;

use build_stamp::choose;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let epoch = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .and_then(|v| v.trim().parse::<i64>().ok());
    let git = git_head();
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let chosen = choose(env_nonempty("CSVIEWER_BUILD"), env_nonempty("CSVIEWER_VERSION"), epoch, git, now);

    println!("cargo:rustc-env=CSVIEWER_BUILD={}", chosen.stamp);
    // Always report provenance. When git failed this is a warning, so a silent
    // fallback cannot happen again.
    if chosen.source.starts_with("wall clock") {
        println!("cargo:warning=native-csviewer build stamp {} (FALLBACK from {})", chosen.stamp, chosen.source);
    } else {
        println!("cargo:warning=native-csviewer build stamp {} (from {})", chosen.stamp, chosen.source);
    }

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build_stamp.rs");
    println!("cargo:rerun-if-changed=src");
    for var in ["CSVIEWER_BUILD", "CSVIEWER_VERSION", "SOURCE_DATE_EPOCH"] {
        println!("cargo:rerun-if-env-changed={var}");
    }
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// Repo root: the nearest ancestor of `CARGO_MANIFEST_DIR` that contains `.git`.
/// Explicit `git -C <root>` avoids depending on the build script's cwd.
fn repo_root() -> Option<PathBuf> {
    let mut dir = Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn git(args: &[&str]) -> Result<String, String> {
    let root = repo_root().ok_or_else(|| "no .git above CARGO_MANIFEST_DIR".to_string())?;
    let out = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(args)
        .output()
        .map_err(|e| format!("spawn git: {e}"))?;
    if !out.status.success() {
        // Non-zero status is reported with git's own stderr, so "dubious
        // ownership" / "not a repository" are distinguishable from "no git".
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(format!("git {args:?} exited {}: {err}", out.status.code().unwrap_or(-1)));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("git {args:?} non-UTF8: {e}"))
}

fn git_head() -> Result<(i64, String, bool), String> {
    let hash = git(&["rev-parse", "--short=7", "HEAD"])?.trim().to_string();
    if hash.is_empty() {
        return Err("git rev-parse returned an empty hash".to_string());
    }
    let raw = git(&["log", "-1", "--format=%ct"])?;
    // --format=%ct ends with a newline: parse the trimmed string.
    let epoch: i64 = raw
        .trim()
        .parse()
        .map_err(|_| format!("git log %ct was not an integer: {raw:?}"))?;
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    Ok((epoch, hash, dirty))
}
