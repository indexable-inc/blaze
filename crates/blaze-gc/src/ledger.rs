//! What the collector keeps across restarts (AGENTS.md rule 26): when its last run ended, so
//! the periodic deadline does not restart with the daemon, and its last [`REPORTS_KEPT`] runs'
//! reports (without verdicts), which status shows. In `<state>/gc-ledger.json`, written whole
//! (temporary, sync, rename).
//!
//! A ledger that does not read is not taken as empty: [`read`] returns the error, which the
//! daemon logs and shows in status, and only then starts a new ledger.
//!
//! [`REPORTS_KEPT`]: crate::plan::REPORTS_KEPT

use std::path::Path;

use blaze_serde::{Deserialize, Serialize};

use crate::plan::RunReport;

/// The ledger's file in the daemon's state directory.
pub const LEDGER: &str = "gc-ledger.json";

pub const FORMAT: u32 = 1;

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Ledger {
    pub format: u32,
    pub last_end_ns: Option<u64>,
    pub runs: Vec<RunReport>,
}

impl Ledger {
    pub fn encode(&self) -> Vec<u8> {
        // Numbers, strings, options, vectors and unit-or-struct enums: nothing that fails.
        serde_json::to_vec_pretty(self).expect("a ledger serializes")
    }

    pub fn decode(bytes: &[u8]) -> Result<Ledger, String> {
        let ledger: Ledger = serde_json::from_slice(bytes)
            .map_err(|error| format!("it does not read as a ledger: {error}"))?;
        if ledger.format != FORMAT {
            return Err(format!(
                "it is of format {}, and this blaze reads format {FORMAT}",
                ledger.format
            ));
        }
        Ok(ledger)
    }
}

/// The ledger at `path`; `None` when there is none yet.
pub fn read(path: &Path) -> Result<Option<Ledger>, String> {
    match std::fs::read(path) {
        Ok(bytes) => {
            Ledger::decode(&bytes).map(Some).map_err(|why| format!("{}: {why}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("reading {}: {error}", path.display())),
    }
}

/// Writes `ledger` at `path`, whole.
pub fn write(path: &Path, ledger: &Ledger) -> Result<(), String> {
    crate::record::write_whole(path, &ledger.encode())
}
