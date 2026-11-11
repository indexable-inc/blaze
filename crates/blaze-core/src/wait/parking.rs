//! The park of one thread on a word and its wake from any other, a sans-IO core: the word's
//! states and the order of its atomics, with the kernel's wait and wake handed in ([`Futex`]).
//! On Linux the kernel is the thread's wait ring (`IORING_OP_FUTEX_WAIT` with the deadline in
//! its registered wait region, `IORING_OP_FUTEX_WAKE`); on macOS the wait on an address
//! (`os_sync_wait_on_address_with_deadline`, `wait/macos.rs`); elsewhere std's thread park. Its proof
//! is the DST in `wait/tests.rs`, which runs these functions against a simulated futex whose
//! wakes come between any two of their atomics ([`Futex::switch`]), and plants a lost wakeup
//! ([`ParkPlant::CheckThenStore`]) it must catch.
//!
//! The word is a token, as std's park is: [`unpark`] leaves [`NOTIFIED`], which the next
//! [`park`] takes and returns at once; a park waits while the word is [`PARKED`]. Only one
//! thread parks on a word; any thread unparks it.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::sync::atomic::{AtomicU32, Ordering};

use super::{
    Clock, Deadline, WaitError, Waited,
    retry::{Next, Retry},
};

/// No token and no one parked.
pub(crate) const EMPTY: u32 = 0;
/// An unpark's token, not yet taken.
pub(crate) const NOTIFIED: u32 = 1;
/// Its thread waits in the kernel, or is about to.
pub(crate) const PARKED: u32 = u32::MAX;

/// The kernel a park waits in.
pub(crate) trait Futex {
    /// Before the parker says it is [`PARKED`]: what a wake needs to find it (std's park
    /// records the thread). Nothing on a futex.
    fn prepare(&self, word: &AtomicU32) {
        _ = word;
    }

    /// Blocks while `word` holds `expected`, until a wake of it or `deadline` on `clock`
    /// (forever for `None`): `Ready` on a wake, a word that no longer held `expected`, or a
    /// return with nothing (the parker looks at the word again); `Deadline` once it passed.
    fn wait<C: Clock>(
        &self,
        what: &'static str,
        word: &AtomicU32,
        expected: u32,
        deadline: Option<Deadline>,
        clock: &C,
    ) -> Result<Waited<()>, WaitError>;

    /// Wakes the thread waiting on `word`, if one is.
    fn wake(&self, word: &AtomicU32);

    /// A point between two atomics of the park or the unpark where another thread may run:
    /// nothing outside the DST, which runs its unparks here.
    fn switch(&self) {}
}

/// A safeguard of the park the DST removes, to see that it catches the removal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ParkPlant {
    None,
    /// The park looks for a token, then stores [`PARKED`] over whatever came since, in place
    /// of one compare-and-swap: an unpark between the look and the store leaves its token
    /// overwritten and wakes no one, the classic lost wakeup.
    #[cfg(test)]
    CheckThenStore,
}

/// Parks on `word` until an unpark's token (`Ready`) or `deadline` on `clock` (`Deadline`;
/// forever for `None`). A token left before the park is taken at once.
pub(crate) fn park<K: Futex, C: Clock>(
    word: &AtomicU32,
    kernel: &K,
    what: &'static str,
    deadline: Option<Deadline>,
    clock: &C,
    plant: ParkPlant,
) -> Result<Waited<()>, WaitError> {
    kernel.switch();
    if word.compare_exchange(NOTIFIED, EMPTY, Ordering::Acquire, Ordering::Relaxed).is_ok() {
        return Ok(Waited::Ready(()));
    }
    kernel.prepare(word);
    kernel.switch();
    if plant == ParkPlant::None {
        // The only other state is a token that came since the look above: taken.
        if word.compare_exchange(EMPTY, PARKED, Ordering::AcqRel, Ordering::Acquire).is_err() {
            kernel.switch();
            word.store(EMPTY, Ordering::Relaxed);
            return Ok(Waited::Ready(()));
        }
    } else {
        if word.load(Ordering::Acquire) == NOTIFIED {
            word.store(EMPTY, Ordering::Relaxed);
            return Ok(Waited::Ready(()));
        }
        kernel.switch();
        word.store(PARKED, Ordering::Release);
    }
    let mut retry = Retry::new(what, deadline, clock.now());
    loop {
        kernel.switch();
        let waited = match kernel.wait(what, word, PARKED, deadline, clock) {
            Ok(waited) => waited,
            // Not parked any more, whatever the wait says; a token that came is still one.
            Err(error) => {
                return match word.swap(EMPTY, Ordering::Acquire) {
                    NOTIFIED => Ok(Waited::Ready(())),
                    _ => Err(error),
                };
            }
        };
        kernel.switch();
        if word.compare_exchange(NOTIFIED, EMPTY, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            return Ok(Waited::Ready(()));
        }
        let passed = match waited {
            Waited::Deadline => true,
            // Woken with no token: a spurious return, looked at against the deadline and the
            // fuel of a clock that does not move.
            Waited::Ready(()) => match retry.after(clock.now()) {
                Ok(Next::Deadline) => true,
                Ok(Next::Again) => false,
                Err(error) => {
                    return match word.swap(EMPTY, Ordering::Acquire) {
                        NOTIFIED => Ok(Waited::Ready(())),
                        _ => Err(error),
                    };
                }
            },
        };
        if passed {
            kernel.switch();
            return Ok(match word.swap(EMPTY, Ordering::Acquire) {
                NOTIFIED => Waited::Ready(()),
                _ => Waited::Deadline,
            });
        }
    }
}

/// Leaves a token on `word` and wakes its parker if it is parked.
pub(crate) fn unpark<K: Futex>(word: &AtomicU32, kernel: &K) {
    kernel.switch();
    if word.swap(NOTIFIED, Ordering::Release) == PARKED {
        kernel.switch();
        kernel.wake(word);
    }
}
