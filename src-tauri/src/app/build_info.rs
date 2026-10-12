//! Which build this binary is. Stamped by `build.rs`; shown in the settings
//! window, in `relay --status` (the resident app's) and `relay --version`
//! (this binary's), so two Macs can be compared at a glance.

use serde::Serialize;

/// The package version plus the git commit and date `build.rs` stamped in.
#[derive(Clone, Debug, Serialize)]
pub struct BuildInfo {
    pub version: &'static str,
    /// Short hash, `+` appended when the tree was dirty; or `unknown`.
    pub commit: &'static str,
    /// `YYYY-MM-DD`, or `unknown`.
    pub date: &'static str,
}

/// This binary's build.
pub const CURRENT: BuildInfo = BuildInfo {
    version: env!("CARGO_PKG_VERSION"),
    commit: env!("RELAY_COMMIT"),
    date: env!("RELAY_BUILD_DATE"),
};

impl BuildInfo {
    /// `0.1.0 · caaf9b0 · 2026-10-10`. The settings window formats the same
    /// string from the fields; keep the two in step.
    pub fn label(&self) -> String {
        format!("{} · {} · {}", self.version, self.commit, self.date)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_joins_the_three_parts_with_middle_dots() {
        let info = BuildInfo {
            version: "0.1.0",
            commit: "caaf9b0+",
            date: "2026-10-10",
        };
        assert_eq!(info.label(), "0.1.0 · caaf9b0+ · 2026-10-10");
    }

    #[test]
    fn the_current_build_is_stamped() {
        assert!(!CURRENT.commit.is_empty());
        // `build.rs` falls back to "unknown" when `date` fails.
        assert!(
            CURRENT.date == "unknown" || CURRENT.date.len() == 10,
            "{}",
            CURRENT.date
        );
    }
}
