//! How a store whose misses are pumped ([`crate::Loader::Pumped`]) loads one
//! (docs/v2/plan/design/store-pool.md, lane 4b). The store does no IO: a read that needs pages
//! not in memory asks for them as effects (reads into the pool's pages), and the caller, who
//! drives the store's effects itself, carries them out where the read is made: a test on the
//! simulation, a tool on the target's native driver. Deterministic, and with no thread of the
//! store's own.

use std::{fmt, time::Duration};

use blaze_core::{Completion, Driver, Effect, IDLE_STALL, wait::Deadline};

/// Whoever drives a pumped store's effects.
pub trait Pump {
    /// Carries out `effects` and hands every completion that comes to `done`, until each of
    /// `effects` has completed. `done` gives back a completion that is not of these effects
    /// (the driver had others in flight): the pump keeps it for whoever asked.
    ///
    /// # Errors
    ///
    /// Nothing completed for as long as a driver may stall: how many were left.
    fn pump(
        &mut self,
        effects: &mut Vec<Effect>,
        done: &mut dyn FnMut(Completion) -> Option<Completion>,
    ) -> Result<(), PumpError>;
}

/// A pump's driver went [`IDLE_STALL`] with nothing completing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PumpError {
    /// The effects of the pump that had not completed.
    pub left: usize,
    /// How long nothing completed, on the driver's clock.
    pub waited: Duration,
}

impl fmt::Display for PumpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the driver pumping the store pool's reads completed nothing for {:?}, with {} of them \
             in flight",
            self.waited, self.left
        )
    }
}

impl std::error::Error for PumpError {}

/// A pump on a driver the caller holds, run where the read is made: [`blaze_core::sim::Sim`] in
/// a test, the target's native driver in a tool. Completions of the driver's other effects that
/// come meanwhile are kept in the order they came ([`Inline::strays`]), for the caller to hand
/// to whoever asked.
pub struct Inline<'d, D: Driver> {
    driver: &'d mut D,
    strays: Vec<Completion>,
}

impl<'d, D: Driver> Inline<'d, D> {
    pub fn new(driver: &'d mut D) -> Inline<'d, D> {
        // alloc: empty, allocates nothing
        Inline { driver, strays: Vec::new() }
    }

    /// The completions that came for effects other than the pumped ones, taken.
    pub fn strays(&mut self) -> Vec<Completion> {
        std::mem::take(&mut self.strays)
    }
}

impl<D: Driver> Pump for Inline<'_, D> {
    fn pump(
        &mut self,
        effects: &mut Vec<Effect>,
        done: &mut dyn FnMut(Completion) -> Option<Completion>,
    ) -> Result<(), PumpError> {
        let mut left = effects.len();
        // alloc: a pumped miss, a test's or a tool's, never the daemon's
        let mut out = Vec::new();
        let mut since = self.driver.now();
        let span = tracing::info_span!(target: "blaze", "store pool pump", reads = left);
        let _in = span.enter();
        // Bounded: each turn completes one of the effects or waits out part of the stall, which
        // ends it.
        while left > 0 {
            self.driver.submit(effects);
            let stall = Deadline::after(since, IDLE_STALL);
            if self.driver.poll(&mut out, stall) > 0 {
                since = self.driver.now();
            } else if stall.passed(self.driver.now()) {
                let waited = self.driver.now().saturating_sub(since);
                return Err(PumpError { left, waited });
            }
            for completion in out.drain(..) {
                match done(completion) {
                    None => left = left.saturating_sub(1),
                    // alloc: a stray completion kept for its asker, a test's or a tool's
                    Some(stray) => self.strays.push(stray),
                }
            }
        }
        Ok(())
    }
}
