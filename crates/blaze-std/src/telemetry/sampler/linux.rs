//! The Linux driver: a CPU-time timer per worker and its `SIGPROF` handler (the module doc of
//! [`super`] says why this is the default and what the other choice is). Thin and safe: the
//! handler, its reads of the interrupted stack and the timers are blaze-sys's
//! `linux::sample`; here are the order and the errors.
//!
//! Order: [`install`] once, before the first worker enters; [`Roster::enter`] on each worker's
//! thread as it starts (arms its timer); [`stop`] before the pool stops, which returns the
//! proof [`Stopped`] the fold takes.

use blaze_sys::{
    Timespec,
    linux::sample::{self, SampleError},
};

use super::{Roster, SamplerError, SamplerRow, Stopped};

fn linux(worker: u32, why: SampleError) -> SamplerError {
    match why {
        SampleError::Call { call, errno } => SamplerError::Linux { worker, call, errno: errno.raw(), name: errno.name() },
        SampleError::Entered | SampleError::Full | SampleError::NotInstalled => SamplerError::Enter { worker },
        SampleError::NoStack => SamplerError::NoStack { worker },
    }
}

/// The handler on `SIGPROF`, process-wide (a second call does nothing).
pub fn install() -> Result<(), SamplerError> {
    sample::install().map_err(|why| linux(0, why))
}

impl Roster<'static> {
    /// Worker `worker`'s thread enters, on that thread (hook: the start of a worker's thread,
    /// before its loop): its tid, its stack's mapping and its CPU-time timer, armed at the
    /// period. A roster that does not sample has no slots, and this does nothing.
    pub fn enter(&self, worker: u32) -> Result<(), SamplerError> {
        let Some(period) = self.period() else {
            return Ok(());
        };
        let slot = self.slot(worker).ok_or(SamplerError::Enter { worker })?;
        let every = Timespec { sec: (period.ns() / 1_000_000_000) as i64, nsec: (period.ns() % 1_000_000_000) as i64 };
        sample::enter(slot, every).map_err(|why| linux(worker, why))
    }
}

/// Every worker's timer deleted and every slot closed: the proof no ring is written. A timer
/// that would not delete is in the row; the close goes on.
pub fn stop(roster: Roster<'_>, row: &mut SamplerRow) -> Result<Stopped, SamplerError> {
    for (worker, slot) in roster.slots().iter().enumerate() {
        if let Err(why) = sample::stop(slot) {
            row.stopped = Some(linux(worker as u32, why));
        }
    }
    roster.close()
}
