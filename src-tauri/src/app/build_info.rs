//! Which build this binary is. Stamped by `build.rs`; shown in the settings
//! window, in `relay --status` (the resident app's) and `relay --version`
//! (this binary's), so two Macs can be compared at a glance.

use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};

/// The package version plus the git commit and date `build.rs` stamped in.
#[derive(Clone, Debug)]
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
    /// `0.1.0 · caaf9b0 · 2026-10-10`.
    pub fn label(&self) -> String {
        format!("{} · {} · {}", self.version, self.commit, self.date)
    }
}

/// The wire form carries the ready-made `label` too, so the settings window
/// shows exactly what `relay --version` prints instead of formatting its own.
impl Serialize for BuildInfo {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("BuildInfo", 4)?;
        state.serialize_field("version", self.version)?;
        state.serialize_field("commit", self.commit)?;
        state.serialize_field("date", self.date)?;
        state.serialize_field("label", &self.label())?;
        state.end()
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
    fn the_wire_form_carries_the_label() {
        let info = BuildInfo {
            version: "0.1.0",
            commit: "caaf9b0",
            date: "2026-10-10",
        };
        let json = serde_json::to_value(&info).expect("serializes");
        assert_eq!(json["label"], "0.1.0 · caaf9b0 · 2026-10-10");
        assert_eq!(json["commit"], "caaf9b0");
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
