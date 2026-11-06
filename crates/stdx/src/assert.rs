// Vendored from https://github.com/matklad/always-assert/commit/4cf564eea6fcf18b30c3c3483a611004dc03afbb
//! Recoverable assertions, inspired by [the use of `assert()` in
//! SQLite](https://www.sqlite.org/assert.html).
//!
//! `never!` and `always!` return the actual value of the condition if
//! `debug_assertions` are disabled.
//!
//! Use them when terminating on assertion failure is worse than continuing.
//!
//! One example would be a critical application like a database:
//!
//! ```ignore
//! use stdx::never;
//!
//! fn apply_transaction(&mut self, tx: Transaction) -> Result<(), TransactionAborted> {
//!     let delta = self.compute_delta(&tx);
//!
//!     if never!(!self.check_internal_invariant(&delta)) {
//!         // Ok, something in this transaction messed up our internal state.
//!         // This really shouldn't be happening, and this signifies a bug.
//!         // Luckily, we can recover by just rejecting the transaction.
//!         return abort_transaction(tx);
//!     }
//!     self.commit(delta);
//!     Ok(())
//! }
//! ```
//!
//! Another example is assertions about non-critical functionality in usual apps
//!
//! ```ignore
//! use stdx::never;
//!
//! let english_message = "super app installed!"
//! let mut local_message = localize(english_message);
//! if never!(local_message.is_empty(), "missing localization for {}", english_message) {
//!     // We localized all the messages but this one slipper through the cracks?
//!     // Better to show the english one then than to fail outright;
//!     local_message = english_message;
//! }
//! println!("{}", local_message);
//! ```

/// Asserts that the condition is always true and returns its actual value.
///
/// If the condition is true does nothing and evaluates to true.
///
/// If the condition is false:
/// * panics if `force` feature or `debug_assertions` are enabled,
/// * otherwise reports a bug at the caller ([`reported`]) and logs an error,
/// * evaluates to false.
///
/// Accepts `format!` style arguments.
#[macro_export]
macro_rules! always {
    ($cond:expr) => {
        $crate::always!($cond, "assertion failed: {}", stringify!($cond))
    };

    ($cond:expr, $fmt:literal $($arg:tt)*) => {{
        let cond = $cond;
        if cfg!(debug_assertions) {
            assert!(cond, $fmt $($arg)*);
        }
        if !cond {
            $crate::assert::reported::report("assertion", $fmt);
            $crate::assert::__tracing_error!($fmt $($arg)*);
        }
        cond
    }};
}

/// Asserts that the condition is never true and returns its actual value.
///
/// If the condition is false does nothing and evaluates to false.
///
/// If the condition is true:
/// * panics if `force` feature or `debug_assertions` are enabled,
/// * otherwise reports a bug at the caller ([`reported`]) and logs an error,
/// * evaluates to true.
///
/// Accepts `format!` style arguments.
///
/// Empty condition is equivalent to false:
///
/// ```ignore
/// never!("oups") ~= unreachable!("oups")
/// ```
#[macro_export]
macro_rules! never {
    (true $($tt:tt)*) => { $crate::never!((true) $($tt)*) };
    (false $($tt:tt)*) => { $crate::never!((false) $($tt)*) };
    () => { $crate::never!("assertion failed: entered unreachable code") };
    ($fmt:literal $(, $($arg:tt)*)?) => {{
        if cfg!(debug_assertions) {
            unreachable!($fmt $(, $($arg)*)?);
        }
        $crate::assert::reported::report("assertion", $fmt);
        $crate::assert::__tracing_error!($fmt $(, $($arg)*)?);
    }};

    ($cond:expr) => {{
        let cond = !$crate::always!(!$cond);
        cond
    }};

    ($cond:expr, $fmt:literal $($arg:tt)*) => {{
        let cond = !$crate::always!(!$cond, $fmt $($arg)*);
        cond
    }};
}

#[doc(hidden)]
pub use tracing::error as __tracing_error;

// --- Reported bugs ------------------------------------------------------------------------

/// The process's reported bugs: each place blaze found a bug in itself and went on past it
/// (a [`never!`] or [`always!`] that failed in a release build, a value formatted with no
/// database attached), counted where it happened, so the daemon says them in `blaze status`
/// and the profile's `errors` table, and a test process that reported one fails.
///
/// A report is not a fallback: what went on past it is marked as a bug where it lands (a
/// marker in the text, a refused assertion's `false`), and the report is the bug's record.
/// It never panics and never ends the process that made it: a daemon keeps serving.
///
/// Reported while a test expects it ([`expecting`]), on the expecting thread, a bug is the
/// test's own and is counted there instead.
pub mod reported {
    use std::{
        backtrace::Backtrace,
        cell::Cell,
        panic::Location,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
    };

    use blaze_core::lock::Lock;

    /// Bugs reported so far in this process, every site's.
    static TOTAL: AtomicU64 = AtomicU64::new(0);

    /// Each site that reported, in the order each first did: a few at most in a healthy
    /// process, so a list.
    static SITES: Lock<Vec<Site>> = Lock::new("stdx.assert.sites", Vec::new());

    /// Whether this process's exit is left as it is when it reported bugs ([`keep_exit_status`]).
    static KEEP_EXIT: AtomicBool = AtomicBool::new(false);

    /// Whether the exit check is registered ([`fail_exit`]).
    static AT_EXIT: AtomicBool = AtomicBool::new(false);

    thread_local! {
        /// The kind a test on this thread expects reported, and how many it has seen.
        static EXPECTING: Cell<Option<(&'static str, u64)>> = const { Cell::new(None) };
    }

    /// One site's reports: the value kind or assertion, what went wrong, where, how often,
    /// and on which thread and with which stack it first did.
    #[derive(Clone, Debug)]
    pub struct Site {
        /// The kind of value or check the bug is of: `AdtDef`, `assertion` (a `never!` or `always!`).
        pub kind: &'static str,
        /// What went wrong: "formatted with no database attached".
        pub what: &'static str,
        /// The site that reported it (`#[track_caller]` through the reporting function).
        pub at: &'static Location<'static>,
        pub count: u64,
        /// The thread of its first report, as named: a libtest test's thread is its test's name.
        pub thread: Option<Arc<str>>,
        /// The stack of its first report: the caller of `at`, which `#[track_caller]` can't
        /// reach through a `Debug` impl.
        pub backtrace: Option<Arc<Backtrace>>,
    }

    impl std::fmt::Display for Site {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                f,
                "a blaze bug: `{}` {} at {}, {} time",
                self.kind, self.what, self.at, self.count
            )?;
            if self.count != 1 {
                f.write_str("s")?;
            }
            if let Some(thread) = &self.thread {
                write!(f, ", first on thread `{thread}`")?;
            }
            Ok(())
        }
    }

    /// Reports a bug of `kind` (what went wrong: `what`) at the caller, and goes on. See the
    /// module. Cold: it takes a lock and, on a site's first report, a backtrace.
    #[cold]
    #[inline(never)]
    #[track_caller]
    pub fn report(kind: &'static str, what: &'static str) {
        report_at(kind, what, Location::caller());
    }

    /// [`report`] of a site given.
    #[cold]
    #[inline(never)]
    pub fn report_at(kind: &'static str, what: &'static str, at: &'static Location<'static>) {
        let expected = EXPECTING.with(|it| match it.get() {
            Some((expects, seen)) if expects == kind => {
                it.set(Some((expects, seen + 1)));
                true
            }
            _ => false,
        });
        if expected {
            return;
        }
        TOTAL.fetch_add(1, Ordering::Relaxed);
        let same = |it: &Site| std::ptr::eq(it.at, at) && it.kind == kind && it.what == what;
        if let Some(site) = SITES.lock().iter_mut().find(|it| same(it)) {
            site.count += 1;
            return;
        }
        // A site's first report: its thread and stack, taken with no lock held.
        let thread = std::thread::current().name().map(Arc::from);
        let backtrace = Some(Arc::new(Backtrace::force_capture()));
        {
            let mut sites = SITES.lock();
            match sites.iter_mut().find(|it| same(it)) {
                Some(site) => site.count += 1,
                // alloc: a site's first report, a few in a process at most
                None => sites.push(Site { kind, what, at, count: 1, thread, backtrace }),
            }
        }
        if !AT_EXIT.swap(true, Ordering::Relaxed) {
            exit_check::register();
        }
    }

    /// Bugs reported so far in this process.
    pub fn total() -> u64 {
        TOTAL.load(Ordering::Relaxed)
    }

    /// Each site that reported so far, in the order each first did.
    pub fn sites() -> Vec<Site> {
        // alloc: a reader's own copy, read by `blaze status` and the profile's flush
        SITES.lock().clone()
    }

    /// Runs `f`, which a test expects to report bugs of `kind`, and gives back what it
    /// returned and how many it reported on this thread. Those are the test's own: neither
    /// counted in [`total`] nor failing the process's exit.
    pub fn expecting<R>(kind: &'static str, f: impl FnOnce() -> R) -> (R, u64) {
        struct Restore(Option<(&'static str, u64)>);
        impl Drop for Restore {
            fn drop(&mut self) {
                EXPECTING.with(|it| it.set(self.0));
            }
        }
        let before = Restore(EXPECTING.with(|it| it.replace(Some((kind, 0)))));
        let out = f();
        let seen = EXPECTING.with(|it| it.get()).map_or(0, |(_, seen)| seen);
        drop(before);
        (out, seen)
    }

    /// Leaves this process's exit status as it is when it reported bugs: the blaze binary's,
    /// whose daemon says them in `blaze status` and the profile instead. Every other process
    /// (a test binary, a tool) that reported one exits failing, naming each ([`fail_exit`]).
    pub fn keep_exit_status() {
        KEEP_EXIT.store(true, Ordering::Relaxed);
    }

    /// The exit check: at the process's exit, with bugs reported and its exit status not kept,
    /// says each site on stderr and ends the process with 101, libtest's failure. Under
    /// libtest every test has ended by then and passed or failed as it did; this fails the run
    /// and names each test (its thread) that reported.
    extern "C" fn fail_exit() {
        if KEEP_EXIT.load(Ordering::Relaxed) || total() == 0 {
            return;
        }
        use std::io::Write as _;
        let mut err = std::io::stderr().lock();
        let _ = writeln!(
            err,
            "\nerror: this process reported {} blaze bug(s) and went on past them:",
            total()
        );
        // Bounded: a thread still running at exit may hold the lock for its update.
        match SITES.try_lock_for(std::time::Duration::from_secs(1)) {
            Some(sites) => {
                for site in sites.iter() {
                    let _ = writeln!(err, "  {site}");
                }
            }
            None => {
                _ = writeln!(
                    err,
                    "  (each site unread: another thread held their list for 1s at exit)"
                )
            }
        }
        let _ = err.flush();
        exit_check::exit(101);
    }

    #[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
    mod exit_check {
        unsafe extern "C" {
            fn atexit(f: extern "C" fn()) -> std::ffi::c_int;
            fn _exit(code: std::ffi::c_int) -> !;
        }

        pub(super) fn register() {
            // SAFETY: `fail_exit` is a plain function of this crate's, which lives as long
            // as the process; libc's `atexit` only stores it.
            let registered = unsafe { atexit(super::fail_exit) };
            // A process whose check can't be registered is told now, never quietly let go.
            if registered != 0 {
                eprintln!(
                    "error: stdx::assert::reported: atexit refused the exit check ({registered})"
                );
            }
        }

        pub(super) fn exit(code: std::ffi::c_int) -> ! {
            // SAFETY: called only from the exit check, inside libc's `exit`, where calling
            // `exit` again is undefined and `_exit` ends the process at once.
            unsafe { _exit(code) }
        }
    }

    /// wasm32-unknown-unknown has no libc and no exit: the counter and sites alone.
    #[cfg(all(target_family = "wasm", target_os = "unknown"))]
    mod exit_check {
        pub(super) fn register() {
            let _ = super::fail_exit;
        }

        pub(super) fn exit(_: i32) -> ! {
            unreachable!("no exit check is registered on wasm32-unknown-unknown")
        }
    }

    #[cfg(test)]
    mod tests;
}
