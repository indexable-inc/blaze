//! What a user sets of the collector: `<root>/gc.json`, one JSON object whose every field is
//! optional and whose unknown fields are refused, naming them (AGENTS.md rule 42): the levels
//! of free space ([`crate::pace::Levels`]), and whether the daemon collects on its own:
//!
//! ```json
//! { "comfortable_percent": 15, "comfortable_gib": 30, "floor_percent": 10, "floor_gib": 20, "share_percent": 50, "auto": true }
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
    #[serde(default)]
    auto: Option<bool>,
}

/// What `gc.json` sets.
#[derive(Clone, Debug)]
pub struct GcSettings {
    pub levels: Levels,
    /// Whether the daemon collects on its own, paced; `true` unless set.
    pub auto: bool,
}

impl GcSettings {
    /// What `root`'s `gc.json` sets, over `levels`; `levels` and `auto` on when there is none.
    pub fn read(root: &Path, mut levels: Levels) -> Result<GcSettings, String> {
        let path = root.join(SETTINGS);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(GcSettings { levels, auto: true });
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
        Ok(GcSettings { levels, auto: set.auto.unwrap_or(true) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(text: &str) -> Result<GcSettings, String> {
        let dir = std::env::temp_dir().join(format!(
            "blaze-gc-settings-{}-{}",
            std::process::id(),
            crate::record::now_ns()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(SETTINGS), text).unwrap();
        let read = GcSettings::read(&dir, Levels::default());
        std::fs::remove_dir_all(&dir).unwrap();
        read
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
    }
}
