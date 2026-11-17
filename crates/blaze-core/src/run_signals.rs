//! The run parent's own signals while the program it started runs (decision D103; run.md piece
//! 6). `blaze run` is a shell's foreground job for the user's program: the program stays in the
//! command's process group, so the terminal's Ctrl-C (SIGINT) and Ctrl-\ (SIGQUIT) reach it
//! directly, and the parent must not die of them while the program decides what they mean; a
//! SIGTERM sent to the parent (a supervisor, `kill <blaze's pid>`) ends the program first, then
//! the parent. Three pieces:
//!
//! - [`Held`] (Unix): the effect. While it lives, this process ignores SIGINT and SIGQUIT and
//!   catches SIGTERM into a flag ([`ParentSignals::take_terminate`]); its drop puts back the
//!   three dispositions it found, on every path (a return, an error, an unwind). One at a time
//!   per process: dispositions are the process's, so a second hold is refused
//!   ([`SignalError::AlreadyHeld`]), never stacked.
//! - [`Relay`]: the sans-IO rule for SIGTERM. The first one seen is forwarded to the program
//!   once ([`RelayAct::Forward`]); the parent then waits for the program's end for its own
//!   bounded step ([`GRACE`]), and kills it when that passes ([`RelayAct::Kill`]). The program's
//!   own lifetime has no deadline (D103); only this step of the parent's has.
//! - [`Scripted`]: the simulation's source. Signals arrive at scripted times on the loop's
//!   virtual clock, as the kernel delivers them under [`Held`]'s dispositions: an ignored
//!   SIGINT or SIGQUIT is discarded (counted, nothing else), a SIGTERM is taken at the loop's
//!   next look.
//!
//! The parent's side only. The program's own process is the launcher's (blaze-launch-rt
//! `signals.rs` installs its handlers with `sigaction` there): dispositions belong to a
//! process, and the two never share one. The order makes that hold across the start: [`Held`]
//! is taken AFTER the child is started, because an ignored disposition survives `execve`
//! (execve(2): "the dispositions of any signals that are being caught are reset to the
//! default; ... ignored signals remain ignored") and the start keeps ignored signals ignored
//! (blaze-core spawn.rs `run`: "Ignored stays ignored across exec"): held before the start, the
//! program would never see its Ctrl-C. The cost of that order is a window of a few system calls
//! between the start and the hold in which a Ctrl-C ends `blaze run` by the default action
//! (Linux: the kernel's kill at the parent's death then ends the program too; macOS: the
//! launcher ends at its next first call). Closing it needs the start to put SIGINT and SIGQUIT
//! back to their defaults in the child (spawn.rs and `posix_spawnattr_setsigdefault`), so the
//! hold can come first, as system(3) does it.
//!
//! A wait interrupted by the caught SIGTERM (EINTR) is retried with the time left by
//! [`crate::wait`] (wait.rs: "retries a call that was interrupted"), so the flag is seen at the
//! loop's next look: within one of its ticks.

use core::fmt;
use core::time::Duration;

use crate::wait::Deadline;

/// How long the parent waits for the program to end after forwarding a SIGTERM before it
/// kills it: the parent's own step, bounded (rule 18). A program that handles SIGTERM to shut
/// down cleanly has this long; ten seconds is what service managers give by default (systemd's
/// `DefaultTimeoutStopSec=90s` is the long end, Docker's `docker stop` default of 10 s the
/// short one; a terminal user's `kill` wants the short one).
pub const GRACE: Duration = Duration::from_secs(10);

/// A signal that reached the run parent, as its rules read it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Arrived {
    /// SIGINT: the terminal's Ctrl-C. The program's (it is in the foreground group).
    Interrupt,
    /// SIGQUIT: the terminal's Ctrl-\. The program's, as SIGINT.
    Quit,
    /// SIGTERM: to the parent alone. Forwarded to the program, then its end waited for.
    Terminate,
}

/// The parent's signal source while the program runs: [`Held`] natively, [`Scripted`] in the
/// simulation.
pub trait ParentSignals {
    /// Whether a SIGTERM came by `now` that the parent has not acted on: reading it clears it.
    /// SIGINT and SIGQUIT never show here: they are ignored while held.
    fn take_terminate(&mut self, now: Duration) -> bool;
}

/// What the parent does on this turn of its loop ([`Relay::turn`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RelayAct {
    /// Nothing.
    Nothing,
    /// Send SIGTERM to the program, once.
    Forward,
    /// The grace after the forward passed with the program still running: kill it.
    Kill,
}

/// Where the SIGTERM rule stands.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    Running,
    Forwarded { at: Duration },
    Killed,
}

/// The SIGTERM rule as a state machine on the loop's clock. See the module's doc.
#[derive(Clone, Copy, Debug)]
pub struct Relay {
    grace: Duration,
    stage: Stage,
}

impl Relay {
    /// A relay whose kill comes `grace` after the forward ([`GRACE`] for `blaze run`).
    pub const fn new(grace: Duration) -> Relay {
        Relay { grace, stage: Stage::Running }
    }

    /// The turn at `now`, `terminate` being whether a SIGTERM was taken on it. A second
    /// SIGTERM after the forward changes nothing: the program was asked once, and the parent's
    /// bound is already running.
    pub fn turn(&mut self, now: Duration, terminate: bool) -> RelayAct {
        match self.stage {
            Stage::Running if terminate => {
                self.stage = Stage::Forwarded { at: now };
                RelayAct::Forward
            }
            Stage::Running => RelayAct::Nothing,
            Stage::Forwarded { at } if now >= at.saturating_add(self.grace) => {
                self.stage = Stage::Killed;
                RelayAct::Kill
            }
            Stage::Forwarded { .. } | Stage::Killed => RelayAct::Nothing,
        }
    }

    /// The latest the loop's next wait may end: the grace's end once forwarded (so the kill
    /// comes when it passes, not a tick later), [`Deadline::NEVER`] otherwise.
    pub fn bound(&self) -> Deadline {
        match self.stage {
            Stage::Forwarded { at } => Deadline::at(at.saturating_add(self.grace)),
            Stage::Running | Stage::Killed => Deadline::NEVER,
        }
    }

    /// Whether a SIGTERM was forwarded.
    pub fn forwarded(&self) -> bool {
        !matches!(self.stage, Stage::Running)
    }
}

/// The simulation's signal source: `arrivals` (time, signal) in time order, delivered as the
/// kernel does under [`Held`]'s dispositions. Nothing allocates: the script is the test's.
#[derive(Debug)]
pub struct Scripted<'s> {
    arrivals: &'s [(Duration, Arrived)],
    next: usize,
    ignored: u32,
    terminate_taken: Option<Duration>,
}

impl<'s> Scripted<'s> {
    /// The source that plays `arrivals`.
    pub const fn new(arrivals: &'s [(Duration, Arrived)]) -> Scripted<'s> {
        Scripted { arrivals, next: 0, ignored: 0, terminate_taken: None }
    }

    /// The SIGINTs and SIGQUITs discarded so far (ignored: they reached the program, not the
    /// parent).
    pub fn ignored(&self) -> u32 {
        self.ignored
    }

    /// When the parent first took a SIGTERM (its loop's clock), if it did.
    pub fn terminate_taken(&self) -> Option<Duration> {
        self.terminate_taken
    }
}

impl ParentSignals for Scripted<'_> {
    fn take_terminate(&mut self, now: Duration) -> bool {
        let mut terminate = false;
        while let Some(&(at, signal)) = self.arrivals.get(self.next) {
            if at > now {
                break;
            }
            self.next += 1;
            match signal {
                Arrived::Interrupt | Arrived::Quit => self.ignored += 1,
                Arrived::Terminate => terminate = true,
            }
        }
        if terminate && self.terminate_taken.is_none() {
            self.terminate_taken = Some(now);
        }
        terminate
    }
}

/// Why the parent's signals could not be held.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SignalError {
    /// Another [`Held`] lives in this process: dispositions are the process's, one holder.
    AlreadyHeld,
    /// `sigaction` refused `signal` with the system's error `os`.
    Sigaction { signal: i32, os: i32 },
}

impl fmt::Display for SignalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rule = "the run parent's signals while the program runs (D103)";
        match self {
            SignalError::AlreadyHeld => write!(f, "{rule}: already held in this process; a second program cannot be run beside the first"),
            SignalError::Sigaction { signal, os } => {
                write!(f, "{rule}: sigaction of signal {signal}{} failed (os error {os})", crate::signal::described(*signal))
            }
        }
    }
}

impl core::error::Error for SignalError {}

#[cfg(unix)]
pub use self::native::Held;

#[cfg(unix)]
mod native {
    use core::ptr;
    use core::sync::atomic::{AtomicBool, Ordering};

    use super::{ParentSignals, SignalError};

    /// A SIGTERM came while held and was not taken yet. Written by [`on_terminate`] (a
    /// lock-free store, async-signal-safe), read and cleared by [`Held::take_terminate`].
    static TERMINATE: AtomicBool = AtomicBool::new(false);

    /// A [`Held`] lives.
    static HELD: AtomicBool = AtomicBool::new(false);

    /// SIGTERM's handler while held: notes it, nothing else (signal-safety(7): only
    /// async-signal-safe work in a handler; a lock-free atomic store is).
    extern "C" fn on_terminate(_: libc::c_int) {
        TERMINATE.store(true, Ordering::Release);
    }

    /// A bug a test plants in [`Held`]: [`Plant::None`] but in the test's own control.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) enum Plant {
        None,
        /// The drop puts back SIGINT and SIGTERM and leaves SIGQUIT ignored.
        RestoreSkipsQuit,
    }

    /// The parent's dispositions while a program runs; see the module's doc. Dropped, the
    /// dispositions it found are back.
    pub struct Held {
        interrupt: libc::sigaction,
        quit: libc::sigaction,
        /// SIGTERM's disposition before, where this hold caught it; `None` where it was
        /// ignored already (this process was started with SIGTERM ignored, so it stays so: the
        /// one who started it asked for that).
        terminate: Option<libc::sigaction>,
        plant: Plant,
    }

    /// One `sigaction(signal, new, old)`: `new` none reads only.
    fn sigaction(signal: libc::c_int, new: Option<&libc::sigaction>) -> Result<libc::sigaction, SignalError> {
        // SAFETY: all zeros is a valid `sigaction` (a handler of 0 is SIG_DFL, an empty mask,
        // no flags); the call writes it.
        let mut old: libc::sigaction = unsafe { core::mem::zeroed() };
        let new = new.map_or(ptr::null(), |it| it as *const libc::sigaction);
        // SAFETY: `new` is null or a `sigaction` that lives across the call; `old` is one on
        // the stack to write. `signal` is SIGINT, SIGQUIT or SIGTERM, each catchable.
        let done = unsafe { libc::sigaction(signal, new, &mut old) };
        match done {
            0 => Ok(old),
            _ => Err(SignalError::Sigaction { signal, os: std::io::Error::last_os_error().raw_os_error().unwrap_or(0) }),
        }
    }

    /// A disposition of `handler` (SIG_IGN, or a function's address), restarting interrupted
    /// calls (SA_RESTART: a read of another thread's is not cut by the parent's SIGTERM), with
    /// no signal blocked while it runs.
    fn disposition(handler: libc::sighandler_t) -> libc::sigaction {
        // SAFETY: all zeros is a valid `sigaction`.
        let mut action: libc::sigaction = unsafe { core::mem::zeroed() };
        action.sa_sigaction = handler;
        action.sa_flags = libc::SA_RESTART;
        // SAFETY: a `sigset_t` inside `action` to empty.
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        action
    }

    impl Held {
        /// SIGINT and SIGQUIT ignored and SIGTERM caught, until the drop. Refused while another
        /// lives in this process; a `sigaction` that fails puts back what this one changed.
        pub fn hold() -> Result<Held, SignalError> {
            Held::hold_with(Plant::None)
        }

        pub(crate) fn hold_with(plant: Plant) -> Result<Held, SignalError> {
            if HELD.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
                return Err(SignalError::AlreadyHeld);
            }
            TERMINATE.store(false, Ordering::Release);
            let ignore = disposition(libc::SIG_IGN);
            let interrupt = match sigaction(libc::SIGINT, Some(&ignore)) {
                Ok(old) => old,
                Err(error) => {
                    HELD.store(false, Ordering::Release);
                    return Err(error);
                }
            };
            let quit = match sigaction(libc::SIGQUIT, Some(&ignore)) {
                Ok(old) => old,
                Err(error) => {
                    put_back(libc::SIGINT, &interrupt);
                    HELD.store(false, Ordering::Release);
                    return Err(error);
                }
            };
            let undo = |error| {
                put_back(libc::SIGQUIT, &quit);
                put_back(libc::SIGINT, &interrupt);
                HELD.store(false, Ordering::Release);
                error
            };
            let before = sigaction(libc::SIGTERM, None).map_err(undo)?;
            let terminate = match before.sa_sigaction == libc::SIG_IGN {
                true => None,
                false => {
                    let catch = disposition(on_terminate as extern "C" fn(libc::c_int) as libc::sighandler_t);
                    Some(sigaction(libc::SIGTERM, Some(&catch)).map_err(undo)?)
                }
            };
            Ok(Held { interrupt, quit, terminate, plant })
        }
    }

    /// `old` back as `signal`'s disposition. It cannot fail: sigaction(2)'s errors are EFAULT
    /// (both pointers here are to live values) and EINVAL ("an invalid signal was specified",
    /// or SIGKILL or SIGSTOP), and `signal` is SIGINT, SIGQUIT or SIGTERM with a disposition
    /// this process read for it; a failure would be a broken C library, stopped by name.
    fn put_back(signal: libc::c_int, old: &libc::sigaction) {
        if let Err(error) = sigaction(signal, Some(old)) {
            panic!("{error}: putting back a disposition this process read for it");
        }
    }

    impl ParentSignals for Held {
        fn take_terminate(&mut self, _now: core::time::Duration) -> bool {
            TERMINATE.swap(false, Ordering::AcqRel)
        }
    }

    impl Drop for Held {
        fn drop(&mut self) {
            if let Some(terminate) = &self.terminate {
                put_back(libc::SIGTERM, terminate);
            }
            if self.plant != Plant::RestoreSkipsQuit {
                put_back(libc::SIGQUIT, &self.quit);
            }
            put_back(libc::SIGINT, &self.interrupt);
            TERMINATE.store(false, Ordering::Release);
            HELD.store(false, Ordering::Release);
        }
    }

    impl core::fmt::Debug for Held {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            write!(f, "Held(terminate caught: {})", self.terminate.is_some())
        }
    }

    #[cfg(test)]
    mod tests {
        //! The rules (D103 and POSIX): an ignored signal is discarded (sigaction(2): "SIG_IGN
        //! ... the signal is ignored"), so `raise(SIGINT)` while held returns and the test
        //! process lives; a caught one runs the handler; the drop puts back what it found.
        //! One test function: dispositions are the process's, and a second test holding at
        //! once would be refused ([`SignalError::AlreadyHeld`]), which this test also checks.

        use core::time::Duration;

        use super::*;

        /// SIGINT's and SIGQUIT's handler for the test's own "before" state: a distinct
        /// address to see come back, and a stray signal does not end the test binary.
        extern "C" fn noted(_: libc::c_int) {}

        fn handler(signal: libc::c_int) -> libc::sighandler_t {
            sigaction(signal, None).expect("a read of a disposition").sa_sigaction
        }

        /// Prints "run signals: held ignores INT and QUIT, catches TERM, restores on return and
        /// unwind; the planted skipped restore is caught". A hold that did not ignore SIGINT
        /// ends the test binary at `raise`; one that did not put back fails the after-checks;
        /// the planted drop that skips SIGQUIT must leave it differing.
        #[test]
        fn held_ignores_catches_and_restores_on_every_path() {
            let mine = noted as extern "C" fn(libc::c_int) as libc::sighandler_t;
            let set = disposition(mine);
            let first_int = sigaction(libc::SIGINT, Some(&set)).expect("SIGINT set");
            let first_quit = sigaction(libc::SIGQUIT, Some(&set)).expect("SIGQUIT set");
            let first_term = sigaction(libc::SIGTERM, None).expect("SIGTERM read");
            {
                let mut held = Held::hold().expect("a hold");
                assert_eq!(Held::hold().map(drop), Err(SignalError::AlreadyHeld), "one holder per process");
                assert_eq!(handler(libc::SIGINT), libc::SIG_IGN);
                assert_eq!(handler(libc::SIGQUIT), libc::SIG_IGN);
                // SAFETY: SIGINT and SIGQUIT are ignored (checked above): the calls return.
                assert_eq!(unsafe { libc::raise(libc::SIGINT) }, 0);
                // SAFETY: as above.
                assert_eq!(unsafe { libc::raise(libc::SIGQUIT) }, 0);
                assert!(!held.take_terminate(Duration::ZERO), "an ignored SIGINT or SIGQUIT is no SIGTERM");
                // SAFETY: SIGTERM is caught by `on_terminate` (it was not ignored before: the
                // test harness does not ignore it), which only stores a flag.
                assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
                assert!(held.take_terminate(Duration::ZERO), "a caught SIGTERM is taken");
                assert!(!held.take_terminate(Duration::ZERO), "taking it clears it");
            }
            assert_eq!(handler(libc::SIGINT), mine, "SIGINT back after a return");
            assert_eq!(handler(libc::SIGQUIT), mine, "SIGQUIT back after a return");
            assert_eq!(handler(libc::SIGTERM), first_term.sa_sigaction, "SIGTERM back after a return");
            let unwound = std::panic::catch_unwind(|| {
                let _held = Held::hold().expect("a hold after the first ended");
                panic!("an unwind through a hold");
            });
            assert!(unwound.is_err());
            assert_eq!(handler(libc::SIGINT), mine, "SIGINT back after an unwind");
            assert_eq!(handler(libc::SIGQUIT), mine, "SIGQUIT back after an unwind");
            drop(Held::hold_with(Plant::RestoreSkipsQuit).expect("a planted hold"));
            let planted_caught = handler(libc::SIGQUIT) != mine;
            assert!(planted_caught, "the planted drop that skips SIGQUIT is seen by the after-check");
            sigaction(libc::SIGINT, Some(&first_int)).expect("the test's SIGINT back");
            sigaction(libc::SIGQUIT, Some(&first_quit)).expect("the test's SIGQUIT back");
            println!("run signals: held ignores INT and QUIT, catches TERM, restores on return and unwind; the planted skipped restore is caught");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Prints "run signals: relay forwards once, kills at the grace's end, bounds the wait".
    #[test]
    fn relay_forwards_once_then_kills_at_the_grace() {
        let s = Duration::from_secs;
        let mut relay = Relay::new(s(10));
        assert_eq!(relay.bound(), Deadline::NEVER, "no bound while nothing was asked (D103)");
        assert_eq!(relay.turn(s(1), false), RelayAct::Nothing);
        assert_eq!(relay.turn(s(2), true), RelayAct::Forward);
        assert_eq!(relay.bound(), Deadline::at(s(12)));
        assert_eq!(relay.turn(s(3), true), RelayAct::Nothing, "a second SIGTERM is not forwarded again");
        assert_eq!(relay.turn(s(12) - Duration::from_nanos(1), false), RelayAct::Nothing);
        assert_eq!(relay.turn(s(12), false), RelayAct::Kill);
        assert_eq!(relay.turn(s(13), true), RelayAct::Nothing, "killed once");
        assert!(relay.forwarded());
        println!("run signals: relay forwards once, kills at the grace's end, bounds the wait");
    }

    /// Prints "run signals: scripted INT and QUIT are discarded, TERM is taken at the next look".
    #[test]
    fn scripted_discards_the_ignored_and_takes_terminate() {
        let ms = Duration::from_millis;
        let arrivals = [(ms(5), Arrived::Interrupt), (ms(6), Arrived::Quit), (ms(30), Arrived::Terminate), (ms(31), Arrived::Interrupt)];
        let mut signals = Scripted::new(&arrivals);
        assert!(!signals.take_terminate(ms(10)));
        assert_eq!(signals.ignored(), 2);
        assert!(!signals.take_terminate(ms(29)));
        assert!(signals.take_terminate(ms(40)));
        assert_eq!(signals.terminate_taken(), Some(ms(40)));
        assert!(!signals.take_terminate(ms(50)), "taken once");
        assert_eq!(signals.ignored(), 3);
        println!("run signals: scripted INT and QUIT are discarded, TERM is taken at the next look");
    }
}
