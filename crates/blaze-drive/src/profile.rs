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
    /// Whether the walk went forward from this tool's and shape's last answer (its changed
    /// files as seeds), never from the units down.
    pub forward: bool,
    /// Files the walk opened as it reached them (`Effect::Open`): not in the leaf table.
    pub opens: u32,
    /// Tests run again and tests whose stored result was reported.
    pub tests_run: u32,
    pub tests_reused: u32,
    /// The store's segments proved by their sealed row at open, and checked whole at open
    /// (blaze-cas `report().sealed` / `.checked`, store-next.md): the caller's to fill.
    pub segments_sealed: u32,
    pub segments_checked: u32,
    /// The one store (drive.md, "The store under the command"): the open's nanoseconds, the
    /// segments its head names and those mapped by the answer, and the probes gets made
    /// (`store_probes` over `store_probe_gets` gets, `store_probes_most` the most one made).
    /// The caller's to fill.
    pub store_open_ns: u64,
    pub store_named: u32,
    pub store_mapped: u32,
    pub store_probe_gets: u64,
    pub store_probes: u64,
    pub store_probes_most: u32,
    /// Scheduling (drive.md, "Scheduling on many cores"): tasks emitted, batches answered,
    /// coordinator steps, re-entries a worker made in place from the settled table.
    pub tasks: u32,
    pub batches: u32,
    pub steps: u32,
    pub in_place: u32,
    /// Jobs by how many times they were entered when they settled: 0, 1, 2, 3, 4, 5-8, 9-16,
    /// more. A cold body's expected count is 1 or 2 (drive.md); a warm one's 1.
    pub entries_hist: [u32; 8],
    /// The caller's: nanoseconds the coordinator spent in `step`, and workers spent idle.
    pub coordinator_ns: u64,
    pub idle_ns: u64,
    /// Items by how many conditions they were checked under: 1, 2, 3-4, 5 or more
    /// (drive.md, "One check covers every configuration").
    pub conditions_hist: [u32; 4],
    /// The gate (drive.md, "The gate"), the caller's clock: from the request's start to the
    /// program's start and to the verdict; items the gate checked while the program ran.
    pub start_ns: u64,
    pub verdict_ns: u64,
    pub checked_beside: u32,
}

impl Profile {
    /// An item settled under `conditions` conditions.
    pub fn conditions_seen(&mut self, conditions: u32) {
        let bucket = match conditions {
            0 | 1 => 0,
            2 => 1,
            3 | 4 => 2,
            _ => 3,
        };
        self.conditions_hist[bucket] += 1;
    }

    /// A job settled after `entries` entries.
    pub fn entries_seen(&mut self, entries: u32) {
        let bucket = match entries {
            0..=4 => entries as usize,
            5..=8 => 5,
            9..=16 => 6,
            _ => 7,
        };
        self.entries_hist[bucket] += 1;
    }

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
            "store        gets {} hits {} puts {} pages read {} written {}; certificate {}; tests run {} reused {}; segments sealed {} checked {}",
            self.gets,
            self.hits,
            self.puts,
            self.pages,
            self.pages_written,
            if self.certified { "stood" } else { "missed" },
            self.tests_run,
            self.tests_reused,
            self.segments_sealed,
            self.segments_checked,
        )?;
        writeln!(
            f,
            "store open   {:.3} ms; segments mapped {} of {} named; probes a get {:.2} mean, {} most ({} gets)",
            self.store_open_ns as f64 / 1e6,
            self.store_mapped,
            self.store_named,
            if self.store_probe_gets == 0 { 0.0 } else { self.store_probes as f64 / self.store_probe_gets as f64 },
            self.store_probes_most,
            self.store_probe_gets,
        )?;
        writeln!(
            f,
            "schedule     tasks {} batches {} steps {} in place {} coordinator {:.3} ms idle {:.3} ms; entries 0|1|2|3|4|5-8|9-16|more {:?}",
            self.tasks,
            self.batches,
            self.steps,
            self.in_place,
            self.coordinator_ns as f64 / 1e6,
            self.idle_ns as f64 / 1e6,
            self.entries_hist,
        )?;
        writeln!(
            f,
            "gate         start {:.3} ms verdict {:.3} ms checked beside {}; items by conditions 1|2|3-4|5+ {:?}",
            self.start_ns as f64 / 1e6,
            self.verdict_ns as f64 / 1e6,
            self.checked_beside,
            self.conditions_hist,
        )
    }
}
