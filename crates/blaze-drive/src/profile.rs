//! The profile every command keeps (drive.md, "The system side"; rule 17): per task kind, how
//! many jobs stood on their row, were found in the store, were made, were fetched, and how many
//! times tasks were entered; and the store traffic. Time is the caller's: it measures each run
//! and adds it with [`Profile::ran`], since the machine has no clock (rule 75).

use core::fmt;

use crate::key::{KINDS, Kind};

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct KindCounts {
    /// Jobs whose row stood: no store read, no run.
    pub standing: u32,
    /// Jobs whose record was found by key.
    pub found: u32,
    /// Jobs whose record was made by a run here.
    pub made: u32,
    /// Jobs of a crate with no text, found by a record key.
    pub fetched: u32,
    /// Runs emitted, re-entries included.
    pub entries: u32,
    /// Runs that answered `Need`.
    pub needs: u32,
    /// Nanoseconds of runs, as the caller measured.
    pub ns: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Profile {
    pub kinds: [KindCounts; KINDS],
    pub gets: u32,
    /// Gets that found bytes.
    pub hits: u32,
    pub puts: u32,
    /// Answer pages read.
    pub pages: u32,
    /// Answer pages written.
    pub pages_written: u32,
    /// Whether the certificate stood.
    pub certified: bool,
    /// Tests run again and tests whose stored result was reported.
    pub tests_run: u32,
    pub tests_reused: u32,
}

impl Profile {
    pub fn of(&mut self, kind: Kind) -> &mut KindCounts {
        &mut self.kinds[kind.index()]
    }

    /// A run of `kind` took `ns`, as the caller measured it.
    pub fn ran(&mut self, kind: Kind, ns: u64) {
        let counts = self.of(kind);
        counts.ns = counts.ns.saturating_add(ns);
    }

    /// Runs over every kind.
    pub fn runs(&self) -> u32 {
        self.kinds.iter().map(|k| k.entries).sum()
    }
}

/// One line per kind with any count, then the store's line: what `blaze` writes to its profile
/// file and, shortened, to the terminal.
impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for kind in Kind::ALL {
            let k = self.kinds[kind.index()];
            if k == KindCounts::default() {
                continue;
            }
            writeln!(
                f,
                "{:<12} standing {:>6} found {:>6} made {:>6} fetched {:>6} entries {:>6} needs {:>6} {:>9.3} ms",
                kind.name(),
                k.standing,
                k.found,
                k.made,
                k.fetched,
                k.entries,
                k.needs,
                k.ns as f64 / 1e6,
            )?;
        }
        writeln!(
            f,
            "store        gets {} hits {} puts {} pages read {} written {}; certificate {}; tests run {} reused {}",
            self.gets,
            self.hits,
            self.puts,
            self.pages,
            self.pages_written,
            if self.certified { "stood" } else { "missed" },
            self.tests_run,
            self.tests_reused,
        )
    }
}
