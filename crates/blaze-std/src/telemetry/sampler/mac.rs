//! The macOS driver: one sampler thread stops each worker in turn (the module doc of [`super`]
//! gives the calls and the rule). Thin and safe: the stop, the read of the stopped stack and
//! the walk are blaze-sys's `macos_sample::sample`; the loop and its accounting are here.

use core::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use blaze_core::wait::{Clock, Deadline, sleep_until};
use blaze_sys::macos_sample::{self, Kern};

use super::{Period, Roster, SamplerError, SamplerRow, Stopped};

fn mach(worker: u32, call: &'static str, kern: Kern) -> SamplerError {
    SamplerError::Mach { worker, call, kern: kern.0 }
}

impl Roster<'_> {
    /// Worker `worker`'s thread enters, on that thread (hook: the start of a worker's thread,
    /// before its loop): its Mach port (one more send reference held) and its stack's exact
    /// range. A roster that does not sample has no slots, and this does nothing.
    pub fn enter(&self, worker: u32) -> Result<(), SamplerError> {
        if self.slots().is_empty() {
            return Ok(());
        }
        let slot = self.slot(worker).ok_or(SamplerError::Enter { worker })?;
        match macos_sample::enter(slot) {
            Ok(true) => Ok(()),
            Ok(false) => Err(SamplerError::Enter { worker }),
            Err(kern) => Err(mach(worker, "mach_port_mod_refs", kern)),
        }
    }
}

/// The sampler thread's loop.
pub struct MacSampler<'r> {
    roster: Roster<'r>,
    period: Period,
    row: SamplerRow,
    tick: u64,
}

impl<'r> MacSampler<'r> {
    pub fn new(roster: Roster<'r>, period: Period) -> MacSampler<'r> {
        MacSampler { roster, period, row: SamplerRow::new(period), tick: 0 }
    }

    /// One pass over every entered worker. Called on the sampler thread only, which never
    /// enters the roster.
    pub fn pass(&mut self) -> Result<(), SamplerError> {
        self.tick += 1;
        for (worker, slot) in self.roster.slots().iter().enumerate() {
            macos_sample::sample(slot, self.tick).map_err(|kern| mach(worker as u32, "thread_suspend/get_state/resume", kern))?;
        }
        Ok(())
    }

    /// Passes every period until `stop` is set; the first error stops it and is in its row.
    /// Then every slot is closed: the row, and the proof the fold takes.
    pub fn run(mut self, stop: &AtomicBool, clock: &impl Clock) -> (SamplerRow, Result<Stopped, SamplerError>) {
        let period = Duration::from_nanos(self.period.ns());
        let mut next = clock.now();
        let mut own = 0u64;
        while !stop.load(Ordering::Acquire) {
            let began = clock.now();
            if let Err(why) = self.pass() {
                self.row.stopped = Some(why);
                break;
            }
            let ended = clock.now();
            own = own.saturating_add(ended.saturating_sub(began).as_nanos() as u64);
            self.row.passes += 1;
            next = next.saturating_add(period);
            if next < ended {
                self.row.late += 1;
                next = ended.saturating_add(period);
            }
            if sleep_until("the sampler's next pass", Deadline::at(next), clock).is_err() {
                self.row.stopped = Some(SamplerError::Sleep);
                break;
            }
        }
        self.row.own_ns = Some(own);
        (self.row, self.roster.close())
    }
}
