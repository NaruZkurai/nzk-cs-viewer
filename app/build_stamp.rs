//! Pure build-stamp policy, shared by the build script and the crate.
//!
//! `build.rs` `include!`s this file; `src/buildinfo.rs` declares it as a module
//! with `#[path = "../build_stamp.rs"]`. Keeping the policy pure (inputs in,
//! stamp out) is what makes the precedence unit-testable — a build script's own
//! tests are never run by `cargo test`, so an untested policy is how the
//! wall-clock fallback once shipped silently.
//!
//! Precedence (first available wins):
//!   1. `CSVIEWER_BUILD`    — explicit override, verbatim.
//!   2. `CSVIEWER_VERSION`  — what `build-appimage.sh --version` honours.
//!   3. `SOURCE_DATE_EPOCH` — reproducible-builds convention, UTC.
//!   4. git HEAD commit time (UTC) + short hash (+ `-dirty`).
//!   5. wall clock          — last resort, always reports why git was unusable.

/// The chosen stamp and a human-readable provenance for the build log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chosen {
    pub stamp: String,
    pub source: String,
}

/// `yymmdd.hhmmss` in UTC — the shape `build-appimage.sh` auto-stamps.
pub fn format_utc(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let secs = epoch.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    format!("{:02}{:02}{:02}.{:02}{:02}{:02}", y.rem_euclid(100), m, d, hh, mm, ss)
}

/// Howard Hinnant's civil-from-days (proleptic Gregorian, UTC).
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Pick the stamp. `git` is `Ok((commit_epoch, short_hash, dirty))` or the
/// concrete reason git could not be used — never a bare `None`, so "no git" is
/// always distinguishable from "git refused" and is reported.
pub fn choose(
    env_build: Option<String>,
    env_version: Option<String>,
    source_date_epoch: Option<i64>,
    git: Result<(i64, String, bool), String>,
    now_epoch: i64,
) -> Chosen {
    if let Some(v) = env_build.filter(|v| !v.trim().is_empty()) {
        return Chosen { stamp: v, source: "CSVIEWER_BUILD override".to_string() };
    }
    if let Some(v) = env_version.filter(|v| !v.trim().is_empty()) {
        return Chosen {
            stamp: v,
            source: "CSVIEWER_VERSION (matches build-appimage.sh)".to_string(),
        };
    }
    if let Some(epoch) = source_date_epoch {
        return Chosen { stamp: format_utc(epoch), source: "SOURCE_DATE_EPOCH".to_string() };
    }
    match git {
        Ok((epoch, hash, dirty)) => {
            let mut stamp = format!("{}-g{}", format_utc(epoch), hash);
            if dirty {
                stamp.push_str("-dirty");
            }
            Chosen { stamp, source: "git HEAD commit time + short hash".to_string() }
        }
        Err(reason) => Chosen {
            stamp: format!("{}-local", format_utc(now_epoch)),
            source: format!("wall clock (git unavailable: {reason})"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "f42f44d";

    fn git_ok() -> Result<(i64, String, bool), String> {
        Ok((1_792_127_703, HASH.to_string(), false))
    }

    #[test]
    fn format_utc_matches_the_appimage_shape() {
        // 2026-10-16 05:15:03 UTC, and the epoch itself.
        assert_eq!(format_utc(1_792_127_703), "261016.051503");
        assert_eq!(format_utc(0), "700101.000000");
        assert_eq!(format_utc(86_399), "700101.235959");
        assert_eq!(format_utc(86_400), "700102.000000");
    }

    #[test]
    fn explicit_build_override_wins_over_everything() {
        let c = choose(
            Some("my-build".into()),
            Some("9.9.9".into()),
            Some(0),
            git_ok(),
            1,
        );
        assert_eq!(c.stamp, "my-build");
        assert_eq!(c.source, "CSVIEWER_BUILD override");
    }

    #[test]
    fn version_wins_over_source_date_epoch_and_git() {
        let c = choose(None, Some("9.9.9".into()), Some(0), git_ok(), 1);
        assert_eq!(c.stamp, "9.9.9");
        assert!(c.source.contains("CSVIEWER_VERSION"));
    }

    #[test]
    fn source_date_epoch_wins_over_git() {
        let c = choose(None, None, Some(1_792_127_703), git_ok(), 1);
        assert_eq!(c.stamp, "261016.051503");
        assert_eq!(c.source, "SOURCE_DATE_EPOCH");
    }

    #[test]
    fn git_derived_stamp_has_the_commit_shape() {
        let c = choose(None, None, None, git_ok(), 1);
        assert_eq!(c.stamp, "261016.051503-gf42f44d");
        assert!(c.source.contains("git"));
        // never the wall-clock fallback when git is available
        assert!(!c.stamp.ends_with("-local"));
    }

    #[test]
    fn dirty_marker_is_appended() {
        let c = choose(None, None, None, Ok((1_792_127_703, HASH.into(), true)), 1);
        assert_eq!(c.stamp, "261016.051503-gf42f44d-dirty");
    }

    #[test]
    fn wall_clock_is_provably_last_and_reports_the_reason() {
        let c = choose(None, None, None, Err("git not on PATH".into()), 1_792_127_703);
        assert_eq!(c.stamp, "261016.051503-local");
        assert!(c.source.contains("git unavailable"));
        assert!(c.source.contains("git not on PATH"));
    }

    #[test]
    fn identical_inputs_give_identical_stamps() {
        let a = choose(None, None, None, git_ok(), 1);
        let b = choose(None, None, None, git_ok(), 999);
        assert_eq!(a, b, "git-derived stamp must not depend on build time");
    }
}
