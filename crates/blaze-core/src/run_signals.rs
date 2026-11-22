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
use core::sync::atomic::{AtomicU8, Ordering};
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

/// Which signal asked a command that answers and exits (`blaze engine-check`, its
/// engine_status.rs): a stop (SIGTERM from `timeout` or a supervisor, SIGINT from Ctrl-C) or a
/// status (SIGINFO, the terminal's Ctrl-T on macOS; SIGUSR1, which Linux has in its place).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum By {
    Interrupt = 1,
    Terminate = 2,
    Info = 3,
    User1 = 4,
}

impl By {
    /// The request's word back as its signal; 0 is none.
    const fn of_code(code: u8) -> Option<By> {
        match code {
            1 => Some(By::Interrupt),
            2 => Some(By::Terminate),
            3 => Some(By::Info),
            4 => Some(By::User1),
            _ => None,
        }
    }

    /// The signal's name as `kill -l` prints it, with its `SIG`.
    pub const fn name(self) -> &'static str {
        match self {
            By::Interrupt => "SIGINT",
            By::Terminate => "SIGTERM",
            By::Info => "SIGINFO",
            By::User1 => "SIGUSR1",
        }
    }

    /// The signal's number on this system: a stop's status is 128 and it, as a shell reports a
    /// process a signal ended (POSIX.1-2017 2.8.2). SIGINFO is macOS's alone (29 there).
    pub const fn number(self) -> u8 {
        match self {
            By::Interrupt => 2,
            By::Terminate => 15,
            By::Info => 29,
            #[cfg(target_os = "linux")]
            By::User1 => 10,
            #[cfg(not(target_os = "linux"))]
            By::User1 => 30,
        }
    }
}

/// The requests a command is asked by its signals while it works: two words a handler stores
/// (a lock-free atomic store, async-signal-safe) and the command's driver reads at its
/// scheduling points (rule 14: the driver is handed this value; no core reads a global). Made
/// by [`Asked`] for the process's own signals, or by a test, which asks it itself.
#[derive(Debug, Default)]
pub struct Requests {
    stop: AtomicU8,
    status: AtomicU8,
}

impl Requests {
    pub const fn new() -> Requests {
        Requests { stop: AtomicU8::new(0), status: AtomicU8::new(0) }
    }

    /// Asked to stop by `by`. The first ask is kept: a Ctrl-C after a SIGTERM still names the
    /// SIGTERM, which is what began the stop.
    pub fn ask_stop(&self, by: By) {
        let _first = self.stop.compare_exchange(0, by as u8, Ordering::AcqRel, Ordering::Acquire);
    }

    /// Asked for the status by `by`; asked twice before a look, it is written once.
    pub fn ask_status(&self, by: By) {
        self.status.store(by as u8, Ordering::Release);
    }

    /// The stop asked, if one was: it stays asked.
    pub fn stop(&self) -> Option<By> {
        By::of_code(self.stop.load(Ordering::Acquire))
    }

    /// The status asked since the last take, taken.
    pub fn take_status(&self) -> Option<By> {
        By::of_code(self.status.swap(0, Ordering::AcqRel))
    }

    /// Nothing asked: a new hold's start.
    fn clear(&self) {
        self.stop.store(0, Ordering::Release);
        self.status.store(0, Ordering::Release);
    }
}

#[cfg(unix)]
pub use self::native::{Asked, Held};

#[cfg(unix)]
mod native {
    use blaze_sys::posix::{signal as sig, types::sighandler_t};
    use core::ffi::c_int;
    use core::sync::atomic::{AtomicBool, Ordering};

    use super::{By, ParentSignals, Requests, SignalError};

    #[cfg(target_os = "macos")]
    type Action = sig::sigaction;
    // On Linux blaze-sys installs its own restorer even when putting back an action
    // read from libc. Keep libc's install semantics for these saved dispositions.
    #[cfg(target_os = "linux")]
    type Action = libc::sigaction;

    /// A SIGTERM came while held and was not taken yet. Written by [`on_terminate`] (a
    /// lock-free store, async-signal-safe), read and cleared by [`Held::take_terminate`].
    static TERMINATE: AtomicBool = AtomicBool::new(false);

    /// A [`Held`] lives.
    static HELD: AtomicBool = AtomicBool::new(false);

    /// SIGTERM's handler while held: notes it, nothing else (signal-safety(7): only
    /// async-signal-safe work in a handler; a lock-free atomic store is).
    extern "C" fn on_terminate(_: c_int) {
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
        interrupt: Action,
        quit: Action,
        /// SIGTERM's disposition before, where this hold caught it; `None` where it was
        /// ignored already (this process was started with SIGTERM ignored, so it stays so: the
        /// one who started it asked for that).
        terminate: Option<Action>,
        plant: Plant,
    }

    /// One `sigaction(signal, new, old)`: `new` none reads only.
    #[cfg(target_os = "macos")]
    fn sigaction(signal: c_int, new: Option<&Action>) -> Result<Action, SignalError> {
        match new {
            None => sig::get_action(signal),
            // SAFETY: this module installs SIG_IGN, a static one-argument async-signal-safe
            // handler, or the original action read for this signal with its matching flags.
            Some(action) => unsafe { sig::set_action(signal, action) },
        }
        .map_err(|error| SignalError::Sigaction { signal, os: i32::from(error.raw()) })
    }

    #[cfg(target_os = "linux")]
    fn sigaction(signal: c_int, new: Option<&Action>) -> Result<Action, SignalError> {
        // SAFETY: zero is a valid disposition with an empty mask and no flags.
        let mut old: Action = unsafe { core::mem::zeroed() };
        let new = new.map_or(core::ptr::null(), core::ptr::from_ref);
        // SAFETY: old is writable, new is null or live, and each handler is either the
        // previously installed one or a static async-signal-safe function with matching flags.
        match unsafe { libc::sigaction(signal, new, &mut old) } {
            0 => Ok(old),
            _ => Err(SignalError::Sigaction { signal, os: std::io::Error::last_os_error().raw_os_error().unwrap_or(0) }),
        }
    }

    /// A disposition of `handler` (SIG_IGN, or a function's address), restarting interrupted
    /// calls (SA_RESTART: a read of another thread's is not cut by the parent's SIGTERM), with
    /// no signal blocked while it runs.
    fn disposition(handler: sighandler_t) -> Action {
        #[cfg(target_os = "macos")]
        let mut action = Action::DEFAULT;
        #[cfg(target_os = "linux")]
        // SAFETY: the all-zero libc action has an empty mask, a default handler and no flags.
        let mut action: Action = unsafe { core::mem::zeroed() };
        action.sa_sigaction = handler;
        action.sa_flags = sig::SA_RESTART;
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
            let ignore = disposition(sig::SIG_IGN);
            let interrupt = match sigaction(sig::SIGINT, Some(&ignore)) {
                Ok(old) => old,
                Err(error) => {
                    HELD.store(false, Ordering::Release);
                    return Err(error);
                }
            };
            let quit = match sigaction(sig::SIGQUIT, Some(&ignore)) {
                Ok(old) => old,
                Err(error) => {
                    put_back(sig::SIGINT, &interrupt);
                    HELD.store(false, Ordering::Release);
                    return Err(error);
                }
            };
            let undo = |error| {
                put_back(sig::SIGQUIT, &quit);
                put_back(sig::SIGINT, &interrupt);
                HELD.store(false, Ordering::Release);
                error
            };
            let before = sigaction(sig::SIGTERM, None).map_err(undo)?;
            let terminate = match before.sa_sigaction == sig::SIG_IGN {
                true => None,
                false => {
                    let catch = disposition(on_terminate as extern "C" fn(c_int) as sighandler_t);
                    Some(sigaction(sig::SIGTERM, Some(&catch)).map_err(undo)?)
                }
            };
            Ok(Held { interrupt, quit, terminate, plant })
        }
    }

    /// `old` back as `signal`'s disposition. It cannot fail: sigaction(2)'s errors are EFAULT
    /// (both pointers here are to live values) and EINVAL ("an invalid signal was specified",
    /// or SIGKILL or SIGSTOP), and `signal` is SIGINT, SIGQUIT or SIGTERM with a disposition
    /// this process read for it; a failure would be a broken C library, stopped by name.
    fn put_back(signal: c_int, old: &Action) {
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
                put_back(sig::SIGTERM, terminate);
            }
            if self.plant != Plant::RestoreSkipsQuit {
                put_back(sig::SIGQUIT, &self.quit);
            }
            put_back(sig::SIGINT, &self.interrupt);
            TERMINATE.store(false, Ordering::Release);
            HELD.store(false, Ordering::Release);
        }
    }

    impl core::fmt::Debug for Held {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            write!(f, "Held(terminate caught: {})", self.terminate.is_some())
        }
    }

    /// The requests the process's signals make while an [`Asked`] lives: the one word a
    /// handler can reach (a handler takes no argument). Read only through [`Asked::requests`],
    /// which hands it to the command's driver.
    static ASKED: Requests = Requests::new();

    /// The stop signals' handler while asked: notes the request, nothing else (signal-safety(7):
    /// a lock-free atomic compare-exchange is async-signal-safe; the match is pure).
    extern "C" fn on_stop(signal: c_int) {
        match signal {
            sig::SIGINT => ASKED.ask_stop(By::Interrupt),
            sig::SIGTERM => ASKED.ask_stop(By::Terminate),
            _ => {}
        }
    }

    /// The status signals' handler while asked: as [`on_stop`], a store and nothing else.
    extern "C" fn on_status(signal: c_int) {
        match signal {
            sig::SIGUSR1 => ASKED.ask_status(By::User1),
            #[cfg(target_os = "macos")]
            sig::SIGINFO => ASKED.ask_status(By::Info),
            _ => {}
        }
    }

    /// A command's own signals while it works (engine_status.rs in blaze-cli): SIGINT and
    /// SIGTERM caught as a stop request, SIGUSR1 (and SIGINFO on macOS) as a status request,
    /// each into [`Asked::requests`]; the drop puts back every disposition it changed, on every
    /// path. One holder per process with [`Held`] (dispositions are the process's): a second is
    /// [`SignalError::AlreadyHeld`]. A stop signal this process was started with ignored stays
    /// ignored (whoever started it asked for that: a `nohup` or a background job's SIGINT).
    pub struct Asked {
        interrupt: Option<Action>,
        terminate: Option<Action>,
        user1: Option<Action>,
        info: Option<Action>,
    }

    /// `signal` caught by `handler`, its disposition before returned; `None` (nothing changed)
    /// for a stop signal found ignored.
    fn catch(signal: c_int, handler: extern "C" fn(c_int), keep_ignored: bool) -> Result<Option<Action>, SignalError> {
        let before = sigaction(signal, None)?;
        if keep_ignored && before.sa_sigaction == sig::SIG_IGN {
            return Ok(None);
        }
        let caught = disposition(handler as sighandler_t);
        sigaction(signal, Some(&caught)).map(Some)
    }

    impl Asked {
        /// The stop and status signals caught until the drop. Refused while a [`Held`] or
        /// another [`Asked`] lives; a `sigaction` that fails puts back what this one changed
        /// (the drop of the part made).
        pub fn hold() -> Result<Asked, SignalError> {
            if HELD.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
                return Err(SignalError::AlreadyHeld);
            }
            ASKED.clear();
            let mut asked = Asked { interrupt: None, terminate: None, user1: None, info: None };
            asked.interrupt = catch(sig::SIGINT, on_stop, true)?;
            asked.terminate = catch(sig::SIGTERM, on_stop, true)?;
            asked.user1 = catch(sig::SIGUSR1, on_status, false)?;
            #[cfg(target_os = "macos")]
            {
                asked.info = catch(sig::SIGINFO, on_status, false)?;
            }
            Ok(asked)
        }

        /// What the signals asked: handed to the command's driver, which reads it at its
        /// scheduling points.
        pub fn requests(&self) -> &'static Requests {
            &ASKED
        }
    }

    impl Drop for Asked {
        fn drop(&mut self) {
            if let Some(old) = &self.info {
                #[cfg(target_os = "macos")]
                put_back(sig::SIGINFO, old);
                #[cfg(not(target_os = "macos"))]
                let _never_set = old;
            }
            if let Some(old) = &self.user1 {
                put_back(sig::SIGUSR1, old);
            }
            if let Some(old) = &self.terminate {
                put_back(sig::SIGTERM, old);
            }
            if let Some(old) = &self.interrupt {
                put_back(sig::SIGINT, old);
            }
            ASKED.clear();
            HELD.store(false, Ordering::Release);
        }
    }

    impl core::fmt::Debug for Asked {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            write!(f, "Asked(interrupt caught: {}, terminate caught: {})", self.interrupt.is_some(), self.terminate.is_some())
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
        extern "C" fn noted(_: c_int) {}

        fn handler(signal: c_int) -> sighandler_t {
            sigaction(signal, None).expect("a read of a disposition").sa_sigaction
        }

        /// Prints "run signals: held ignores INT and QUIT, catches TERM, restores on return and
        /// unwind; the planted skipped restore is caught". A hold that did not ignore SIGINT
        /// ends the test binary at `raise`; one that did not put back fails the after-checks;
        /// the planted drop that skips SIGQUIT must leave it differing.
        #[test]
        fn held_ignores_catches_and_restores_on_every_path() {
            let mine = noted as extern "C" fn(c_int) as sighandler_t;
            let set = disposition(mine);
            let first_int = sigaction(sig::SIGINT, Some(&set)).expect("SIGINT set");
            let first_quit = sigaction(sig::SIGQUIT, Some(&set)).expect("SIGQUIT set");
            let first_term = sigaction(sig::SIGTERM, None).expect("SIGTERM read");
            {
                let mut held = Held::hold().expect("a hold");
                assert_eq!(Held::hold().map(drop), Err(SignalError::AlreadyHeld), "one holder per process");
                assert_eq!(handler(sig::SIGINT), sig::SIG_IGN);
                assert_eq!(handler(sig::SIGQUIT), sig::SIG_IGN);
                assert_eq!(sig::raise_signal(sig::SIGINT), Ok(()));
                assert_eq!(sig::raise_signal(sig::SIGQUIT), Ok(()));
                assert!(!held.take_terminate(Duration::ZERO), "an ignored SIGINT or SIGQUIT is no SIGTERM");
                assert_eq!(sig::raise_signal(sig::SIGTERM), Ok(()));
                assert!(held.take_terminate(Duration::ZERO), "a caught SIGTERM is taken");
                assert!(!held.take_terminate(Duration::ZERO), "taking it clears it");
            }
            assert_eq!(handler(sig::SIGINT), mine, "SIGINT back after a return");
            assert_eq!(handler(sig::SIGQUIT), mine, "SIGQUIT back after a return");
            assert_eq!(handler(sig::SIGTERM), first_term.sa_sigaction, "SIGTERM back after a return");
            let unwound = std::panic::catch_unwind(|| {
                let _held = Held::hold().expect("a hold after the first ended");
                panic!("an unwind through a hold");
            });
            assert!(unwound.is_err());
            assert_eq!(handler(sig::SIGINT), mine, "SIGINT back after an unwind");
            assert_eq!(handler(sig::SIGQUIT), mine, "SIGQUIT back after an unwind");
            drop(Held::hold_with(Plant::RestoreSkipsQuit).expect("a planted hold"));
            let planted_caught = handler(sig::SIGQUIT) != mine;
            assert!(planted_caught, "the planted drop that skips SIGQUIT is seen by the after-check");
            sigaction(sig::SIGQUIT, Some(&first_quit)).expect("the test's SIGQUIT back");
            // A command's own signals (`Asked`), in this one function for the same reason:
            // SIGINT and SIGTERM are a stop (the first kept), SIGUSR1 a status taken once.
            let first_usr1 = sigaction(sig::SIGUSR1, None).expect("SIGUSR1 read");
            {
                let asked = Asked::hold().expect("an asked hold");
                assert_eq!(Held::hold().map(drop), Err(SignalError::AlreadyHeld), "one holder of the dispositions: Held beside Asked");
                assert_eq!(Asked::hold().map(drop), Err(SignalError::AlreadyHeld), "one Asked");
                let requests = asked.requests();
                assert_eq!(requests.stop(), None);
                assert_eq!(sig::raise_signal(sig::SIGUSR1), Ok(()));
                assert_eq!(requests.take_status(), Some(By::User1), "a status asked by SIGUSR1");
                assert_eq!(requests.take_status(), None, "taken once");
                assert_eq!(sig::raise_signal(sig::SIGTERM), Ok(()));
                assert_eq!(sig::raise_signal(sig::SIGINT), Ok(()));
                assert_eq!(requests.stop(), Some(By::Terminate), "the first stop is kept");
            }
            assert_eq!(handler(sig::SIGINT), mine, "SIGINT back after an asked hold");
            assert_eq!(handler(sig::SIGTERM), first_term.sa_sigaction, "SIGTERM back after an asked hold");
            assert_eq!(handler(sig::SIGUSR1), first_usr1.sa_sigaction, "SIGUSR1 back after an asked hold");
            sigaction(sig::SIGINT, Some(&first_int)).expect("the test's SIGINT back");
            println!("run signals: held ignores INT and QUIT, catches TERM, restores on return and unwind; the planted skipped restore is caught; asked takes INT and TERM as a stop, USR1 as a status, and restores");
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
