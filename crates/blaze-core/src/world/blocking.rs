//! A blocking call of the kernel's, as a reference model: a call that waits for one event,
//! with a time limit or none, and what the kernel may answer it.
//!
//! A wait is a loop of such calls (`blaze_core::wait::until`): each is given what is left of
//! the wait's deadline and says how it ended. The kernel has a choice at every call: it may
//! end it for a signal, for nothing at all, or before its limit, at any moment, and the caller
//! reads the clock some time after. What it may not do is the world's rule:
//!
//! - a call takes time: the caller's next reading is after the call began;
//! - a call given a limit is read at most [`Blocking::late`] past that limit;
//! - a call says ready only once the event has come, and says anything else only where the
//!   event had not come when the kernel last looked ([`Answer::at`]); so a call made after the
//!   event is ready, whatever interrupts it.
//!
//! [`Blocking::late`] bounds two delays as one: from the caller's reading of the clock to the
//! call's start in the kernel, and from the call's end in the kernel to the caller's next
//! reading. The model makes a call at the reading itself, so a real call that starts later is
//! one read later past its limit.

#![blaze::sans_io]

use std::time::Duration;

/// How a call ended, as the kernel says it: `blaze_core::wait::Once` without the value a
/// ready call carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// The event has come.
    Ready,
    /// A signal ended the call (EINTR).
    Interrupted,
    /// The call woke with nothing.
    Spurious,
    /// The call's limit ran out, or the kernel said so before it had.
    TimedOut,
}

/// One answer of the kernel's to a blocking call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Answer {
    /// How the call ended.
    pub ended: Ended,
    /// When it ended in the kernel, on the caller's clock: the last moment the kernel looked
    /// for the event.
    pub at: Duration,
    /// How long after that the caller reads the clock.
    pub after: Duration,
}

/// The reference model: the kernel of one wait, as the clock at the caller's last reading,
/// when the awaited event comes, and how late a call is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocking {
    /// The clock at the caller's last reading, where its next call is made.
    pub now: Duration,
    /// When the event the calls wait for comes; `None`: never.
    pub event: Option<Duration>,
    /// The longest a call's end is read after it, and a limited call's return after its limit.
    pub late: Duration,
}

impl Blocking {
    /// Whether the kernel may give `answer` to a call made now with `left` to wait (`None`:
    /// no limit).
    pub fn allows(&self, left: Option<Duration>, answer: &Answer) -> bool {
        // The caller's next reading, which the clock has room for.
        let Some(read) = answer.at.checked_add(answer.after) else {
            return false;
        };
        // A call takes time, does not end before it began, and its end is read within `late`.
        if answer.after.is_zero() || answer.after > self.late || answer.at < self.now {
            return false;
        }
        // A call given a limit is read at most `late` past it. Where the sum passes the
        // clock's end there is no limit to pass.
        if let Some(left) = left {
            if read > self.now.saturating_add(left).saturating_add(self.late) {
                return false;
            }
        }
        let came = match self.event {
            Some(event) => event <= answer.at,
            None => false,
        };
        match answer.ended {
            Ended::Ready => came,
            Ended::Interrupted | Ended::Spurious => !came,
            Ended::TimedOut => !came && left.is_some(),
        }
    }

    /// The kernel after it gave `answer`, one that [`Blocking::allows`] said it may: the clock
    /// at the caller's next reading.
    pub fn answer(&mut self, answer: &Answer) {
        self.now = answer.at.saturating_add(answer.after);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns(n: u64) -> Duration {
        Duration::from_nanos(n)
    }

    fn answer(ended: Ended, at: u64, after: u64) -> Answer {
        Answer { ended, at: ns(at), after: ns(after) }
    }

    /// The rule on its edges: a call made after the event is ready and nothing else, a call
    /// is never read past its limit by more than `late`, and no call takes no time.
    #[test]
    fn the_world_allows_what_a_kernel_does() {
        let world = Blocking { now: ns(100), event: Some(ns(150)), late: ns(10) };
        // Interrupted before the event; ready at it; never ready before it.
        assert!(world.allows(Some(ns(80)), &answer(Ended::Interrupted, 120, 3)));
        assert!(world.allows(Some(ns(80)), &answer(Ended::Ready, 150, 10)));
        assert!(!world.allows(Some(ns(80)), &answer(Ended::Ready, 149, 1)));
        // Once the event has come, a call says nothing but ready.
        assert!(!world.allows(Some(ns(80)), &answer(Ended::Interrupted, 150, 1)));
        assert!(!world.allows(Some(ns(80)), &answer(Ended::TimedOut, 160, 1)));
        // A timeout may come early, and needs a limit.
        assert!(world.allows(Some(ns(30)), &answer(Ended::TimedOut, 110, 1)));
        assert!(!world.allows(None, &answer(Ended::TimedOut, 110, 1)));
        // Read at most `late` past the limit: 100 + 30 + 10.
        assert!(world.allows(Some(ns(30)), &answer(Ended::Spurious, 131, 9)));
        assert!(!world.allows(Some(ns(30)), &answer(Ended::Spurious, 132, 9)));
        // A call takes time, within `late`, and does not end before it began.
        assert!(!world.allows(Some(ns(30)), &answer(Ended::Spurious, 120, 0)));
        assert!(!world.allows(Some(ns(30)), &answer(Ended::Spurious, 110, 11)));
        assert!(!world.allows(Some(ns(30)), &answer(Ended::Spurious, 99, 1)));
        // No room on the clock: refused, not wrapped.
        let end = Blocking { now: Duration::MAX, event: None, late: ns(10) };
        assert!(!end.allows(None, &Answer { ended: Ended::Spurious, at: Duration::MAX, after: ns(1) }));
    }

    #[test]
    fn an_answer_moves_the_clock_to_the_next_reading() {
        let mut world = Blocking { now: ns(100), event: None, late: ns(10) };
        world.answer(&answer(Ended::Interrupted, 120, 3));
        assert_eq!(world.now, ns(123));
    }
}
