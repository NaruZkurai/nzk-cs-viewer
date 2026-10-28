//! The build stamp, the product identity and the window titles built from them.
//!
//! `BUILD` is baked in at compile time by `app/build.rs`; nothing here reads a
//! file or the clock at runtime, so the title can never show a stale build.
//!
//! `main_title` / `pane_title` / `version_line` are **pure functions of the
//! stamp**, which is what makes the titles checkable in a unit test with no
//! display and no window.
//!
//! The stamp *policy* lives in `../build_stamp.rs`, shared with `build.rs`; the
//! tests there pin the precedence and run under `cargo test`.
#[path = "../build_stamp.rs"]
pub mod stamp;

/// Product identity. The owner's own words: "when webgui is finished that is
/// cs viewer v3". The completion bar for this version is written down in
/// `notes/csviewer-v3.md`; nothing here declares it met.
pub const PRODUCT: &str = "cs viewer v3";

/// Cargo package version (`[workspace.package] version`).
///
/// This is deliberately *not* repeated as a literal anywhere: the version moves
/// (the workspace went `2.0.0` → `3.0.0-alpha.1`), and a test that pins a moving
/// value only ever records the moment it was written. The identity the tests are
/// entitled to assert is that the product string is right and that this constant
/// is the crate's own version — never a frozen copy of it.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Compile-time stamp, e.g. `261016.051503-gf42f44d-dirty`.
pub const BUILD: &str = env!("CSVIEWER_BUILD");

/// The main window's title bar text: identity + build stamp.
pub fn main_title(build: &str) -> String {
    format!("{PRODUCT} {build} — engine-free graph view")
}

/// A detached pane window's title bar text (`pane` is `Pane::title()`).
pub fn pane_title(build: &str, pane: &str) -> String {
    format!("{PRODUCT} {build} — {pane}")
}

/// One-line identity for `--version`-style output.
pub fn version_line() -> String {
    format!("{PRODUCT} {VERSION} ({BUILD})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_cs_viewer_v3() {
        assert_eq!(PRODUCT, "cs viewer v3");
        // Assert the version is *this crate's* version and is well-formed, rather
        // than freezing a literal. Pinning "3.0.0" here only recorded the day it
        // was written and broke the moment the workspace moved to 3.0.0-alpha.1;
        // the v3 identity is carried by PRODUCT (and by the workspace version),
        // not by a copy of the number in a test.
        assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
        // Shape check on the semver *core* only. Splitting the whole string on '.'
        // would count the pre-release identifiers too: "3.0.0-alpha.1" splits into
        // four parts, so a naive `count() == 3` rejects the very version this
        // workspace is on. Strip the `-pre`/`+build` suffix first, then require a
        // three-part all-numeric core.
        let core = VERSION.split(['-', '+']).next().unwrap_or(VERSION);
        assert_eq!(core.split('.').count(), 3, "not x.y.z: {VERSION:?}");
        assert!(
            core.split('.')
                .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())),
            "non-numeric core version part: {VERSION:?}"
        );
        let v = version_line();
        assert!(v.contains(PRODUCT), "{v:?}");
        assert!(v.contains(VERSION), "{v:?}");
        assert!(v.contains(BUILD), "{v:?}");
    }

    #[test]
    fn build_stamp_is_not_empty_or_placeholder() {
        assert!(!BUILD.trim().is_empty(), "CSVIEWER_BUILD was not stamped");
        assert_ne!(BUILD, "unknown");
        assert_eq!(BUILD, env!("CSVIEWER_BUILD"));
    }

    /// Guard against the silent wall-clock fallback: with no override in play
    /// the compiled stamp must be the git-derived one, not `...-local`.
    #[test]
    fn compiled_build_is_not_a_silent_git_fallback() {
        let overridden = ["CSVIEWER_BUILD", "CSVIEWER_VERSION", "SOURCE_DATE_EPOCH"]
            .iter()
            .any(|v| std::env::var(v).is_ok());
        if overridden {
            return;
        }
        assert!(
            !BUILD.ends_with("-local"),
            "compiled BUILD is the wall-clock fallback ({BUILD}); build.rs must report why git failed"
        );
    }

    #[test]
    fn main_title_contains_identity_and_build_stamp() {
        let t = main_title(BUILD);
        assert!(t.contains(PRODUCT), "{t:?} does not contain {PRODUCT:?}");
        assert!(t.contains(BUILD), "{t:?} does not contain {BUILD:?}");
        assert!(t.starts_with("cs viewer v3 "), "{t:?} does not lead with the product");
    }

    #[test]
    fn every_pane_title_contains_identity_and_build_stamp() {
        for pane in ["Source", "Node", "Find", "Blocks"] {
            let t = pane_title(BUILD, pane);
            assert!(t.contains(PRODUCT), "{t:?} does not contain {PRODUCT:?}");
            assert!(t.contains(BUILD), "{t:?} does not contain {BUILD:?}");
            assert!(t.contains(pane), "{t:?} does not name the pane");
        }
    }

    #[test]
    fn titles_are_a_pure_function_of_the_stamp() {
        // Same stamp => same title; different stamp => different title. No
        // display, no environment, no clock is consulted.
        assert_eq!(main_title("X"), main_title("X"));
        assert_ne!(main_title("X"), main_title("Y"));
        assert!(main_title("BUILD-1").contains("BUILD-1"));
        assert!(pane_title("BUILD-1", "Source").contains("BUILD-1"));
        assert!(pane_title("BUILD-1", "Source").contains("Source"));
    }
}
