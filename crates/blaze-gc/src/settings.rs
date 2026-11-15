//! What a user sets of the collector: `<root>/gc.json`, one JSON object whose every field is
//! optional and whose unknown fields are refused, naming them (AGENTS.md rule 42): the levels
//! of free space ([`crate::pace::Levels`]), and whether the daemon collects on its own:
//!
//! ```json
//! { "comfortable_percent": 15, "comfortable_gib": 30, "floor_percent": 10, "floor_gib": 20, "share_percent": 25, "budget_gib": 64, "auto": true }
//! ```
//!
//! `"auto": false` turns the daemon's paced collector off (`blaze cache gc` still runs when
//! asked). A file that does not read, or sets a level that cannot hold (a share of 0%, a
//! comfortable level or floor of 100% or more, a floor above the comfortable level), fails the
//! daemon's start and `blaze cache gc`, naming it and the field; there is no default taken in
//! its place.

use std::path::Path;

use blaze_serde::Deserialize;

use crate::pace::Levels;

/// The file under blaze's root.
pub const SETTINGS: &str = "gc.json";

const GIB: u64 = 1 << 30;

#[derive(Deserialize, Default, Debug)]
#[serde(deny_unknown_fields)]
struct Settings {
    #[serde(default)]
    comfortable_percent: Option<u8>,
    #[serde(default)]
    comfortable_gib: Option<u64>,
    #[serde(default)]
    floor_percent: Option<u8>,
    #[serde(default)]
    floor_gib: Option<u64>,
    #[serde(default)]
    share_percent: Option<u8>,
    /// The absolute budget, in GiB, or (`budget_mib`, for a small disk or a test) in MiB;
    /// only one of the two.
    #[serde(default)]
    budget_gib: Option<u64>,
    #[serde(default)]
    budget_mib: Option<u64>,
    /// How long a pack of the daemon's store goes unused before records are forgotten out
    /// of it, in seconds (an hour unless set).
    #[serde(default)]
    store_grace_s: Option<u64>,
    #[serde(default)]
    auto: Option<bool>,
}

/// What `gc.json` sets.
#[derive(Clone, Debug)]
pub struct GcSettings {
    pub levels: Levels,
    /// Whether the daemon collects on its own, paced; `true` unless set.
    pub auto: bool,
    /// The grace of the daemon's store, in seconds, when the file sets it.
    pub store_grace_s: Option<u64>,
}

impl GcSettings {
    /// What `root`'s `gc.json` sets, over `levels`; `levels` and `auto` on when there is none.
    pub fn read(root: &Path, mut levels: Levels) -> Result<GcSettings, String> {
        let path = root.join(SETTINGS);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(GcSettings { levels, auto: true, store_grace_s: None });
            }
            Err(error) => return Err(format!("reading {}: {error}", path.display())),
        };
        let set: Settings = serde_json::from_slice(&bytes)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        // A free level of 100% or more is never met, so every run would be urgent; a share of
        // 0% would ask to free everything blaze holds (review C14).
        let percent = |name: &str, it: Option<u8>, low: u8, high: u8| match it {
            Some(it) if it < low || it > high => {
                Err(format!("{}: {name} is {it}, not between {low} and {high}", path.display()))
            }
            it => Ok(it),
        };
        if let Some(it) = percent("comfortable_percent", set.comfortable_percent, 0, 99)? {
            levels.comfortable_percent = it;
        }
        if let Some(it) = percent("floor_percent", set.floor_percent, 0, 99)? {
            levels.floor_percent = it;
        }
        if let Some(it) = percent("share_percent", set.share_percent, 1, 100)? {
            levels.share_percent = it;
        }
        if let Some(it) = set.comfortable_gib {
            levels.comfortable_bytes = it.saturating_mul(GIB);
        }
        if let Some(it) = set.floor_gib {
            levels.floor_bytes = it.saturating_mul(GIB);
        }
        match (set.budget_gib, set.budget_mib) {
            (Some(_), Some(_)) => {
                return Err(format!(
                    "{}: budget_gib and budget_mib are both set; set one",
                    path.display()
                ));
            }
            (Some(0), _) | (_, Some(0)) => {
                return Err(format!("{}: the budget is 0", path.display()));
            }
            (Some(it), None) => levels.budget_bytes = it.saturating_mul(GIB),
            (None, Some(it)) => levels.budget_bytes = it.saturating_mul(1 << 20),
            (None, None) => {}
        }
        if levels.floor_percent > levels.comfortable_percent {
            return Err(format!(
                "{}: floor_percent {} is above comfortable_percent {}",
                path.display(),
                levels.floor_percent,
                levels.comfortable_percent
            ));
        }
        if levels.floor_bytes > levels.comfortable_bytes {
            return Err(format!(
                "{}: the floor of {} GiB is above the comfortable level of {} GiB",
                path.display(),
                levels.floor_bytes / GIB,
                levels.comfortable_bytes / GIB
            ));
        }
        Ok(GcSettings {
            levels,
            auto: set.auto.unwrap_or(true),
            store_grace_s: set.store_grace_s,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(text: &str) -> Result<GcSettings, String> {
        let dir = blaze_core::temp::TempDir::for_test("blaze-gc-settings-");
        std::fs::write(dir.join(SETTINGS), text).unwrap();
        GcSettings::read(dir.path(), Levels::default())
    }

    #[test]
    fn gc_json_refuses_levels_that_cannot_hold_and_reads_auto() {
        assert!(read(r#"{"share_percent": 0}"#).unwrap_err().contains("share_percent is 0"));
        assert!(
            read(r#"{"comfortable_percent": 100}"#)
                .unwrap_err()
                .contains("comfortable_percent is 100")
        );
        assert!(read(r#"{"floor_percent": 100}"#).is_err());
        assert!(read(r#"{"autos": false}"#).is_err());
        assert!(!read(r#"{"auto": false}"#).unwrap().auto);
        assert!(read("{}").unwrap().auto);
        let defaults = read("{}").unwrap().levels;
        assert_eq!((defaults.budget_bytes, defaults.share_percent), crate::pace::BUDGET);
        assert_eq!(read(r#"{"budget_gib": 128}"#).unwrap().levels.budget_bytes, 128 << 30);
        assert_eq!(read(r#"{"budget_mib": 512}"#).unwrap().levels.budget_bytes, 512 << 20);
        assert!(read(r#"{"budget_gib": 0}"#).unwrap_err().contains("the budget is 0"));
        assert!(read(r#"{"budget_gib": 1, "budget_mib": 1}"#).unwrap_err().contains("set one"));
    }
}
