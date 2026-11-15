//! The thin driver: a [`Command`]'s effects carried out on any [`Driver`] (the native one in
//! the binary, the simulation in the tests), its completions handed back, and the store
//! flushed when it asks. No logic of the command's is here.

use std::time::Duration;

use blaze_core::{Completion, Driver, Effect, wait::Deadline};
use blaze_kernel::Put;

use crate::run::{Command, Outcome, Poll, SoloError};

/// How long the driver waits with nothing completing before it says the command stalled.
pub const STALL: Duration = Duration::from_secs(60);

/// A store a command puts into, and makes durable when asked.
pub trait Durable {
    type Store: Put + ?Sized;

    fn store(&mut self) -> &mut Self::Store;

    /// What was put so far, durable: done when it returns.
    fn flush(&mut self) -> Result<(), String>;
}

impl Durable for blaze_cas::Driven {
    type Store = blaze_cas::Store;

    fn store(&mut self) -> &mut blaze_cas::Store {
        blaze_cas::Driven::store(self)
    }

    fn flush(&mut self) -> Result<(), String> {
        blaze_cas::Driven::flush(self)
    }
}

/// `command` driven on `driver` to its end, `effects` its first effects.
pub fn drive<D: Driver, S: Durable + ?Sized>(
    driver: &mut D,
    command: &mut Command,
    effects: &mut Vec<Effect>,
    store: &mut S,
) -> Result<Outcome, SoloError> {
    // alloc: once a command, the completions of a poll
    let mut done: Vec<Completion> = Vec::new();
    loop {
        driver.submit(effects);
        done.clear();
        let since = driver.now();
        let got = {
            let _span = tracing::info_span!(target: "blaze", "solo: wait for the command's effects", in_flight = driver.in_flight()).entered();
            driver.poll(&mut done, Deadline::after(since, STALL))
        };
        if got == 0 && Deadline::after(since, STALL).passed(driver.now()) {
            return Err(SoloError {
                thing: "the command's driver".to_owned(),
                rule: "a command's effects complete within the stall bound",
                // alloc: an error
                why: format!("nothing completed in {STALL:?} with {} effects in flight", driver.in_flight()),
            });
        }
        for completion in done.drain(..) {
            let now = driver.now();
            let poll = command.complete(completion, store.store(), || now, effects);
            match poll {
                Poll::Pending => {}
                Poll::Flush => {
                    let flushed = {
                        let _span = tracing::info_span!(target: "blaze", "solo: wait for the store's flush").entered();
                        store.flush()
                    };
                    let now = driver.now();
                    if command.flushed(flushed, || now, effects) == Poll::Done {
                        return command.finish();
                    }
                }
                Poll::Done => return command.finish(),
            }
        }
    }
}
