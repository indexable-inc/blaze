//! The worker's program builds share its memory budget (docs/memory-budget.md).
//!
//! A build begins when the most it is predicted to hold fits in the budget beside what the
//! worker holds and what the builds under way were granted. A program the worker has not yet
//! measured runs alone, and its build is measured: how much more than at its start the worker
//! held at most while it ran is its prediction next time, with a quarter more for growth.
//!
//! A build waiting for memory holds a handle on the database and no query, so a write that
//! interrupts the builds unwinds it as it does the builds under way, rather than waiting for
//! a build that waits for them.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Condvar, Mutex, Once, atomic::Ordering},
    time::Duration,
};

use blaze_cargo::{lock, memory};
use ide_db::{RootDatabase, base_db::salsa};

struct Ledger {
    /// Builds under way.
    running: usize,
    /// What they were granted: the most each is predicted to hold beyond what the worker held.
    granted: u64,
    /// Builds begun, to tell one that ran with no other begun beside it.
    begun: u64,
    /// The most the worker held while builds ran, since the last began alone.
    peak: u64,
    /// For each program, the most more than at its start the worker held when it last ran
    /// alone.
    peaks: BTreeMap<PathBuf, u64>,
}

static LEDGER: Mutex<Ledger> =
    Mutex::new(Ledger { running: 0, granted: 0, begun: 0, peak: 0, peaks: BTreeMap::new() });
static FREED: Condvar = Condvar::new();
static SAMPLING: Once = Once::new();

/// How often the worker's memory is read while builds run, for their peaks.
const EVERY: Duration = Duration::from_millis(100);

/// A build's share of the budget, given back when it is dropped.
pub(crate) struct Grant {
    program: PathBuf,
    bytes: u64,
    /// What the worker held when it began alone, and how many builds had begun with it.
    alone: Option<(u64, u64)>,
}

/// Waits until the build of the program written to `program` fits in the budget, or the
/// builds are interrupted, which unwinds as an interrupted build does.
pub(crate) fn admit(db: &RootDatabase, program: &Path) -> anyhow::Result<Grant> {
    SAMPLING.call_once(|| {
        std::thread::Builder::new()
            .name("memory peaks".into())
            .spawn(sample)
            .expect("a thread to sample the worker's memory");
    });
    let mut waited = false;
    let mut ledger = lock(&LEDGER);
    loop {
        let held = memory::held_by(std::process::id()).unwrap_or(0);
        let budget = memory::Budget::now(held).map_err(anyhow::Error::msg)?;
        let measured = ledger.peaks.get(program).copied();
        let bytes = measured.map_or(u64::MAX, |it| it.saturating_add(it / 4));
        if fits(ledger.running, held, ledger.granted, bytes, budget.as_ref().map(|it| it.bytes)) {
            if ledger.running > 0 {
                eprintln!(
                    "memory: building {} beside {} others, {} MB more predicted",
                    program.display(),
                    ledger.running,
                    bytes >> 20
                );
            }
            let alone = (ledger.running == 0).then(|| {
                ledger.peak = held;
                (held, ledger.begun + 1)
            });
            ledger.running += 1;
            ledger.begun += 1;
            ledger.granted = ledger.granted.saturating_add(bytes);
            return Ok(Grant { program: program.into(), bytes, alone });
        }
        if !waited && let Some(budget) = &budget {
            waited = true;
            let needs = match measured {
                Some(_) => format!("is predicted to hold {} MB more", bytes >> 20),
                None => "has not been measured, so runs alone".into(),
            };
            eprintln!(
                "waiting for memory: {} {needs}; the worker holds {} MB and {} builds under way were granted {} MB, of a budget of {budget}",
                program.display(),
                held >> 20,
                ledger.running,
                ledger.granted.min(budget.bytes) >> 20,
            );
        }
        ledger = FREED.wait_timeout(ledger, EVERY).unwrap_or_else(|it| it.into_inner()).0;
        if crate::INTERRUPTING.load(Ordering::SeqCst) {
            drop(ledger);
            std::panic::resume_unwind(Box::new(salsa::Cancelled::PendingWrite));
        }
        drop(ledger);
        salsa::Database::unwind_if_revision_cancelled(db);
        ledger = lock(&LEDGER);
    }
}

/// Whether a build predicted to hold `bytes` more may begin beside `running` others granted
/// `granted`, with the worker holding `held`: alone it always may.
fn fits(running: usize, held: u64, granted: u64, bytes: u64, budget: Option<u64>) -> bool {
    running == 0 || budget.is_none_or(|it| held.saturating_add(granted).saturating_add(bytes) <= it)
}

impl Drop for Grant {
    fn drop(&mut self) {
        let held = memory::held_by(std::process::id()).unwrap_or(0);
        let mut ledger = lock(&LEDGER);
        ledger.running -= 1;
        ledger.granted = ledger.granted.saturating_sub(self.bytes);
        if let Some((start, begun)) = self.alone
            && ledger.begun == begun
        {
            let peak = ledger.peak.max(held).saturating_sub(start);
            eprintln!(
                "memory: {} built alone, holding {} MB more at most",
                self.program.display(),
                peak >> 20
            );
            ledger.peaks.insert(std::mem::take(&mut self.program), peak);
        }
        FREED.notify_all();
    }
}

fn sample() {
    loop {
        std::thread::sleep(EVERY);
        let mut ledger = lock(&LEDGER);
        if ledger.running > 0 {
            drop(ledger);
            let held = memory::held_by(std::process::id()).unwrap_or(0);
            ledger = lock(&LEDGER);
            ledger.peak = ledger.peak.max(held);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fits;

    const GB: u64 = 1 << 30;

    #[test]
    fn a_build_runs_alone_until_it_is_measured() {
        assert!(fits(0, 10 * GB, 0, u64::MAX, Some(12 * GB)));
        assert!(!fits(1, 2 * GB, GB, u64::MAX, Some(12 * GB)));
        assert!(!fits(1, 2 * GB, u64::MAX, GB, Some(12 * GB)));
    }

    #[test]
    fn measured_builds_run_side_by_side_as_the_budget_allows() {
        assert!(fits(1, 4 * GB, 3 * GB, 5 * GB, Some(12 * GB)));
        assert!(!fits(2, 4 * GB, 6 * GB, 3 * GB, Some(12 * GB)));
        assert!(fits(2, 4 * GB, 6 * GB, 3 * GB, None));
    }
}
