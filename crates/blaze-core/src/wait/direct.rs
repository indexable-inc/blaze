//! The waits of a process that runs where io_uring is refused (Linux): the same sans-IO cores
//! as the ring's (`parking::park` over [`Futex`], `reaping::reap` over [`Children`], the retry
//! loop of [`super::until`]) over direct system calls, one per wait:
//!
//! - a park is `futex(FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG)` to an absolute `CLOCK_MONOTONIC`
//!   time, and its wake `futex(FUTEX_WAKE | FUTEX_PRIVATE_FLAG)`;
//! - a child's exit is `waitid(P_PID, WEXITED | WNOHANG | WNOWAIT)` to look, then
//!   `pidfd_open` and `ppoll` of the pidfd to wait with a deadline, then the look again;
//! - a poll of one descriptor is `ppoll`;
//! - a sleep is `clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME)`.
//!
//! Every call is given the time [`super::until`] recomputed from the wait's clock just before
//! it, as the ring's enter is: an absolute time is `CLOCK_MONOTONIC` now plus what is left, and
//! `ppoll`'s relative one is what is left; neither is carried across a retry. `ppoll` runs
//! under the ring's signal mask ([`WAITING_MASK`]); a futex wait and a sleep take no mask, and
//! a signal that ends one (`EINTR`) is a retry.
//!
//! Who uses it: a process blaze's sandbox launched (`blaze-cargo/src/sandbox/linux.rs`), whose
//! seccomp filter answers `io_uring_setup` with `ENOSYS`: blaze as the `rustc` or `rustdoc` a
//! build script runs, and any other blaze tool there. The filter refuses only `io_uring_setup`,
//! two terminal ioctls and another architecture's calls, so each call above is allowed in it;
//! of them a recorded run asks its supervisor about none (`clock_gettime` is asked about for the
//! realtime clocks alone, and this reads `CLOCK_MONOTONIC`). Which driver a process uses is
//! chosen once, in `main` ([`super::choose`]), never by trying the ring.
//!
//! The calls are behind two traits ([`FutexCalls`], [`ChildCalls`]) so the DST in
//! `wait/tests.rs` runs this driver, its absolute times and its reading of each call's error,
//! against the simulated kernel the cores are proven on; [`Kernel`] is the real one.

use std::{
    io,
    os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd},
    ptr,
    sync::atomic::AtomicU32,
    time::Duration,
};

use super::{
    Clock, Deadline, Once, WaitError, Waited,
    linux::{KERNEL_SIGSET, WAITING_MASK},
    parking::Futex,
    reaping::Children,
    until,
};

/// `FUTEX_BITSET_MATCH_ANY`: a wake of any bit ends the wait.
const FUTEX_ANY: u32 = u32::MAX;

/// The futex calls of a park.
pub(crate) trait FutexCalls {
    /// `CLOCK_MONOTONIC` now: the clock a futex wait's absolute time is on.
    fn monotonic(&self) -> io::Result<Duration>;

    /// One `FUTEX_WAIT_BITSET` on `word` while it holds `expected`, until `at` on
    /// `CLOCK_MONOTONIC` (no limit for `None`): `Ok` when woken or returned with nothing, the
    /// call's own error otherwise (`EAGAIN`: the word held something else; `EINTR`; `ETIMEDOUT`).
    fn futex_wait(&self, word: &AtomicU32, expected: u32, at: Option<Duration>) -> io::Result<()>;

    /// One `FUTEX_WAKE` of one waiter on `word`.
    fn futex_wake(&self, word: &AtomicU32);

    /// [`Futex::switch`], for the DST's kernel; nothing on the real one.
    fn between(&self) {}
}

/// The child calls of a reap.
pub(crate) trait ChildCalls {
    /// What a reap says of a child.
    type Reaped;
    /// A descriptor of a child that polls readable once the child has exited.
    type Pidfd;

    /// `wait4(WNOHANG)`: as [`Children::reap_now`].
    fn reap_now(&self, pid: i32) -> io::Result<Option<Self::Reaped>>;

    /// `waitid(P_PID, WEXITED | WNOHANG)`, with `WNOWAIT` when `keep`: whether `pid` has exited
    /// (and, without `keep`, was reaped by this call); `ECHILD` for a pid that is no child.
    fn exited_now(&self, pid: i32, keep: bool) -> io::Result<bool>;

    /// `pidfd_open(pid)`.
    fn pidfd(&self, pid: i32) -> io::Result<Self::Pidfd>;

    /// One `ppoll` of `fd` for up to `left` (no limit for `None`): `Ready` once the child has
    /// exited.
    fn poll_exit(&self, fd: &Self::Pidfd, left: Option<Duration>) -> io::Result<Once<()>>;
}

/// The direct driver over the calls `S`: [`Kernel`], or the DST's simulated kernel.
pub(crate) struct Direct<'a, S>(pub(crate) &'a S);

impl<S: FutexCalls> Futex for Direct<'_, S> {
    fn wait<C: Clock>(
        &self,
        what: &'static str,
        word: &AtomicU32,
        expected: u32,
        deadline: Option<Deadline>,
        clock: &C,
    ) -> Result<Waited<()>, WaitError> {
        until(what, clock, deadline, |left| {
            // The absolute time of this call alone, from what is left now.
            let at = match left {
                Some(left) => Some(self.0.monotonic()?.saturating_add(left)),
                None => None,
            };
            match self.0.futex_wait(word, expected, at) {
                // Woken, or returned with nothing: the parker looks at the word again.
                Ok(()) => Ok(Once::Ready(())),
                Err(error) => match error.raw_os_error() {
                    // The word no longer held `expected`: a wake came before the wait.
                    Some(libc::EAGAIN) => Ok(Once::Ready(())),
                    Some(libc::EINTR) => Ok(Once::Interrupted),
                    Some(libc::ETIMEDOUT) => Ok(Once::TimedOut),
                    _ => Err(error),
                },
            }
        })
    }

    fn wake(&self, word: &AtomicU32) {
        self.0.futex_wake(word);
    }

    fn switch(&self) {
        self.0.between();
    }
}

impl<S: ChildCalls> Children for Direct<'_, S> {
    type Reaped = S::Reaped;

    fn reap_now(&self, pid: i32) -> io::Result<Option<Self::Reaped>> {
        self.0.reap_now(pid)
    }

    fn exited<C: Clock>(
        &self,
        what: &'static str,
        pid: i32,
        keep: bool,
        deadline: Option<Deadline>,
        clock: &C,
    ) -> Result<Waited<()>, WaitError> {
        // Looked at first: a pid that is no child fails here (`ECHILD`), before a pidfd of
        // some other process is waited on, and one that has exited needs no wait.
        if self.0.exited_now(pid, keep).map_err(|error| WaitError::io(what, error))? {
            return Ok(Waited::Ready(()));
        }
        // A child not yet reaped keeps its pid, so the pidfd is this child's.
        let fd = self.0.pidfd(pid).map_err(|error| WaitError::io(what, error))?;
        match until(what, clock, deadline, |left| self.0.poll_exit(&fd, left))? {
            Waited::Deadline => Ok(Waited::Deadline),
            Waited::Ready(()) => {
                match self.0.exited_now(pid, keep).map_err(|error| WaitError::io(what, error))? {
                    true => Ok(Waited::Ready(())),
                    false => Err(WaitError::io(
                        what,
                        io::Error::other(
                            // alloc: an error path.
                            format!(
                                "process {pid}: its pidfd polled readable, and waitid after found it running"
                            ),
                        ),
                    )),
                }
            }
        }
    }
}

/// The real kernel's calls.
pub(crate) struct Kernel;

fn errno() -> io::Error {
    io::Error::last_os_error()
}

/// `at` as the kernel's `timespec`, saturating at its end.
fn timespec(at: Duration) -> libc::timespec {
    libc::timespec {
        // `time_t` is 64 bits on every Linux blaze runs on.
        tv_sec: i64::try_from(at.as_secs()).unwrap_or(i64::MAX),
        tv_nsec: at.subsec_nanos().into(),
    }
}

/// `CLOCK_MONOTONIC` now.
fn monotonic() -> io::Result<Duration> {
    let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: a timespec for the kernel to write.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } != 0 {
        return Err(errno());
    }
    Ok(Duration::new(u64::try_from(now.tv_sec).unwrap_or(0), now.tv_nsec as u32))
}

/// One `ppoll` of `fd` for `events`, for up to `left` (no limit for `None`), under
/// [`WAITING_MASK`]: its `revents` when it is ready.
fn ppoll_one(
    fd: RawFd,
    events: libc::c_short,
    left: Option<Duration>,
) -> io::Result<Once<libc::c_short>> {
    let mut fds = libc::pollfd { fd, events, revents: 0 };
    let ts = left.map(timespec);
    let mask = WAITING_MASK;
    // SAFETY: `fds` is one live pollfd the kernel writes `revents` of; the timespec (null for
    // no limit) and the mask are live locals it reads, the mask of the kernel's own size; it
    // keeps no pointer after the call. The raw call, as glibc's takes its own larger sigset.
    let ready = unsafe {
        libc::syscall(
            libc::SYS_ppoll,
            &raw mut fds,
            1 as libc::nfds_t,
            ts.as_ref().map_or(ptr::null(), ptr::from_ref),
            &raw const mask,
            KERNEL_SIGSET as usize,
        )
    };
    if ready > 0 {
        return Ok(Once::Ready(fds.revents));
    }
    if ready == 0 {
        return Ok(Once::TimedOut);
    }
    let error = errno();
    match error.raw_os_error() {
        Some(libc::EINTR) => Ok(Once::Interrupted),
        // The kernel could not allocate for the call this time.
        Some(libc::EAGAIN) => Ok(Once::Spurious),
        _ => Err(error),
    }
}

impl FutexCalls for Kernel {
    fn monotonic(&self) -> io::Result<Duration> {
        monotonic()
    }

    fn futex_wait(&self, word: &AtomicU32, expected: u32, at: Option<Duration>) -> io::Result<()> {
        let ts = at.map(timespec);
        // SAFETY: `word` is a live `u32` of this process the kernel only reads; the timespec
        // (null for no limit) is a live local it reads, an absolute `CLOCK_MONOTONIC` time as
        // `FUTEX_WAIT_BITSET` takes it; no pointer is kept after the call.
        let waited = unsafe {
            libc::syscall(
                libc::SYS_futex,
                word.as_ptr(),
                libc::FUTEX_WAIT_BITSET | libc::FUTEX_PRIVATE_FLAG,
                expected,
                ts.as_ref().map_or(ptr::null(), ptr::from_ref),
                ptr::null::<u32>(),
                FUTEX_ANY,
            )
        };
        match waited {
            0 => Ok(()),
            _ => Err(errno()),
        }
    }

    fn futex_wake(&self, word: &AtomicU32) {
        // SAFETY: a wake of a word of this process; the kernel reads no memory of it.
        unsafe {
            libc::syscall(
                libc::SYS_futex,
                word.as_ptr(),
                libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG,
                1,
                ptr::null::<libc::timespec>(),
                ptr::null::<u32>(),
                0,
            )
        };
    }
}

impl ChildCalls for Kernel {
    type Reaped = (libc::c_int, libc::rusage);
    type Pidfd = OwnedFd;

    fn reap_now(&self, pid: i32) -> io::Result<Option<Self::Reaped>> {
        let mut status = 0;
        // SAFETY: all-zero bytes is a valid `rusage`.
        let mut rusage: libc::rusage = unsafe { std::mem::zeroed() };
        // `WNOHANG` never blocks; an interruption is retried a few times, then is the error.
        for _ in 0..4 {
            // SAFETY: live locals the call writes and does not keep.
            let reaped = unsafe { libc::wait4(pid, &mut status, libc::WNOHANG, &mut rusage) };
            match reaped {
                0 => return Ok(None),
                n if n == pid => return Ok(Some((status, rusage))),
                _ if errno().raw_os_error() == Some(libc::EINTR) => {}
                _ => return Err(errno()),
            }
        }
        Err(io::Error::from_raw_os_error(libc::EINTR))
    }

    fn exited_now(&self, pid: i32, keep: bool) -> io::Result<bool> {
        let options = libc::WEXITED | libc::WNOHANG | if keep { libc::WNOWAIT } else { 0 };
        let id = libc::id_t::try_from(pid)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a negative process id"))?;
        // As `reap_now`: `WNOHANG` never blocks.
        for _ in 0..4 {
            // SAFETY: all-zero bytes is a valid `siginfo_t`, zeroed so that a child still
            // running, of which the call writes nothing it need not, reads as pid 0.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: a live local the call writes and does not keep.
            let waited = unsafe { libc::waitid(libc::P_PID, id, &mut info, options) };
            if waited == 0 {
                // SAFETY: `waitid` returned 0, so `info` is a `SIGCHLD` one, whose pid field
                // is valid to read: the child's once it has exited, 0 while it runs.
                return Ok(unsafe { info.si_pid() } == pid);
            }
            let error = errno();
            if error.raw_os_error() != Some(libc::EINTR) {
                return Err(error);
            }
        }
        Err(io::Error::from_raw_os_error(libc::EINTR))
    }

    fn pidfd(&self, pid: i32) -> io::Result<OwnedFd> {
        // SAFETY: the call takes a pid and flags and returns a descriptor of its own.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if fd < 0 {
            return Err(errno());
        }
        // SAFETY: the call returned a descriptor no one else holds.
        Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
    }

    fn poll_exit(&self, fd: &OwnedFd, left: Option<Duration>) -> io::Result<Once<()>> {
        Ok(match ppoll_one(fd.as_raw_fd(), libc::POLLIN, left)? {
            Once::Ready(_) => Once::Ready(()),
            Once::Interrupted => Once::Interrupted,
            Once::Spurious => Once::Spurious,
            Once::TimedOut => Once::TimedOut,
        })
    }
}

/// Waits until `fd` is ready for `events` with `ppoll`: its `revents`.
pub(crate) fn poll(
    what: &'static str,
    fd: RawFd,
    events: libc::c_short,
    deadline: Option<Deadline>,
    clock: &impl Clock,
) -> Result<Waited<libc::c_short>, WaitError> {
    until(what, clock, deadline, |left| ppoll_one(fd, events, left))
}

/// Sleeps until `deadline` on `clock`: `clock_nanosleep` to the absolute `CLOCK_MONOTONIC` time
/// that is now plus what is left, recomputed after a signal ended it early.
pub(crate) fn sleep_until(
    what: &'static str,
    deadline: Deadline,
    clock: &impl Clock,
) -> Result<(), WaitError> {
    // Never ready: it ends as its deadline passes on `clock`.
    until(what, clock, Some(deadline), |left| {
        let at = monotonic()?.saturating_add(left.unwrap_or(Duration::ZERO));
        match nanosleep_until(&timespec(at)) {
            // The kernel's time came: the wait ends once the deadline has passed on its clock.
            0 => Ok(Once::<()>::TimedOut),
            libc::EINTR => Ok(Once::Interrupted),
            error => Err(io::Error::from_raw_os_error(error)),
        }
    })
    .map(|_| ())
}

/// One `clock_nanosleep` to `at` on `CLOCK_MONOTONIC`: 0, or the error's number (the call
/// returns it, and sets no `errno`).
#[expect(
    clippy::disallowed_methods,
    reason = "the one `clock_nanosleep` of the direct driver's `sleep_until`, to an absolute time recomputed from the deadline"
)]
fn nanosleep_until(at: &libc::timespec) -> libc::c_int {
    // SAFETY: `at` is a live timespec the kernel reads; no remainder is asked for, as the
    // time is absolute.
    unsafe {
        libc::clock_nanosleep(libc::CLOCK_MONOTONIC, libc::TIMER_ABSTIME, at, ptr::null_mut())
    }
}

#[cfg(test)]
mod tests {
    //! The direct driver on the real kernel, called as itself (the test binary's own waits are
    //! the ring's): a park woken, one that ends at its deadline, one a signal interrupts, a
    //! child's exit waited for past a deadline and to its end, a poll and a sleep.

    use std::sync::{
        Arc,
        atomic::{AtomicU32, AtomicUsize, Ordering},
    };

    use super::*;
    use crate::wait::{
        Monotonic,
        parking::{self, ParkPlant},
        reaping::{self, ReapPlant},
    };

    const LONG: Duration = Duration::from_secs(60);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Sleeps `wait` on the direct driver.
    fn nap(wait: Duration) {
        let clock = Monotonic::new();
        sleep_until("a test's pause", Deadline::after(clock.now(), wait), &clock).unwrap();
    }

    fn thread(name: &str, run: impl FnOnce() + Send + 'static) -> std::thread::JoinHandle<()> {
        // alloc: a test's thread.
        std::thread::Builder::new().name(name.to_owned()).spawn(run).unwrap()
    }

    #[test]
    fn a_park_is_woken_by_an_unpark() {
        let word = Arc::new(AtomicU32::new(parking::EMPTY));
        let waker = {
            let word = Arc::clone(&word); // clone: a reference count, for the waking thread
            thread("direct-unpark", move || {
                nap(ms(30));
                parking::unpark(&word, &Direct(&Kernel));
            })
        };
        let clock = Monotonic::new();
        let deadline = Some(Deadline::after(clock.now(), LONG));
        let (what, plant) = ("a test's park", ParkPlant::None);
        let parked = parking::park(&word, &Direct(&Kernel), what, deadline, &clock, plant);
        assert_eq!(parked.unwrap(), Waited::Ready(()));
        assert!(clock.now() < LONG, "woken by the unpark, not the deadline");
        waker.join().unwrap();
        assert_eq!(word.load(Ordering::SeqCst), parking::EMPTY);
    }

    #[test]
    fn an_unpark_before_the_park_is_taken_at_once() {
        let word = AtomicU32::new(parking::EMPTY);
        parking::unpark(&word, &Direct(&Kernel));
        let clock = Monotonic::new();
        let parked =
            parking::park(&word, &Direct(&Kernel), "a test's park", None, &clock, ParkPlant::None);
        assert_eq!(parked.unwrap(), Waited::Ready(()));
    }

    #[test]
    fn a_park_ends_at_its_deadline() {
        let word = AtomicU32::new(parking::EMPTY);
        let clock = Monotonic::new();
        let deadline = Deadline::after(clock.now(), ms(40));
        let parked = parking::park(
            &word,
            &Direct(&Kernel),
            "a test's park",
            Some(deadline),
            &clock,
            ParkPlant::None,
        );
        assert_eq!(parked.unwrap(), Waited::Deadline);
        let end = clock.now();
        assert!(deadline.passed(end), "it said its deadline passed at {end:?}, before it");
        assert!(end < LONG, "it ended {end:?} in, far past its 40 ms");
        assert_eq!(word.load(Ordering::SeqCst), parking::EMPTY);
    }

    /// The kernel's calls, counting the futex waits a signal ended.
    struct Counting(AtomicUsize);

    impl FutexCalls for Counting {
        fn monotonic(&self) -> io::Result<Duration> {
            Kernel.monotonic()
        }

        fn futex_wait(
            &self,
            word: &AtomicU32,
            expected: u32,
            at: Option<Duration>,
        ) -> io::Result<()> {
            let waited = Kernel.futex_wait(word, expected, at);
            if waited.as_ref().is_err_and(|it| it.raw_os_error() == Some(libc::EINTR)) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
            waited
        }

        fn futex_wake(&self, word: &AtomicU32) {
            Kernel.futex_wake(word);
        }
    }

    extern "C" fn nothing(_: libc::c_int) {}

    /// A park whose thread is signalled every few milliseconds is interrupted, waits again for
    /// what is left, and ends at its deadline, not a whole wait after the last signal.
    #[test]
    fn a_park_interrupted_by_signals_ends_at_its_deadline() {
        // A handler that does nothing, so the signal ends the wait and not the process. The
        // one place a blaze thread is signalled: a test of what an interruption does.
        // SAFETY: all-zero bytes is a valid `sigaction`; the handler is async-signal-safe (it
        // does nothing), and is installed with no `SA_RESTART`.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = nothing as extern "C" fn(libc::c_int) as usize;
            libc::sigemptyset(&mut action.sa_mask);
            assert_eq!(libc::sigaction(libc::SIGUSR2, &action, ptr::null_mut()), 0);
        }
        // SAFETY: the calling thread's own handle.
        let me = unsafe { libc::pthread_self() } as usize;
        let parked = Arc::new(AtomicU32::new(0));
        let signaller = {
            let parked = Arc::clone(&parked); // clone: a reference count, for the signalling thread
            thread("direct-signal", move || {
                for _ in 0..40 {
                    if parked.load(Ordering::SeqCst) != 0 {
                        return;
                    }
                    nap(ms(5));
                    // SAFETY: `me` is the handle of the parking thread, which lives until it
                    // has joined this one.
                    unsafe { libc::pthread_kill(me as libc::pthread_t, libc::SIGUSR2) };
                }
            })
        };
        let calls = Counting(AtomicUsize::new(0));
        let word = AtomicU32::new(parking::EMPTY);
        let clock = Monotonic::new();
        let deadline = Deadline::after(clock.now(), ms(120));
        let ended = parking::park(
            &word,
            &Direct(&calls),
            "a test's park under signals",
            Some(deadline),
            &clock,
            ParkPlant::None,
        );
        let end = clock.now();
        parked.store(1, Ordering::SeqCst);
        signaller.join().unwrap();
        assert_eq!(ended.unwrap(), Waited::Deadline);
        assert!(deadline.passed(end), "it said its deadline passed at {end:?}, before it");
        assert!(end < ms(120) + Duration::from_secs(5), "it ended {end:?} in, far past its 120 ms");
        assert!(
            calls.0.load(Ordering::SeqCst) > 0,
            "no futex wait was interrupted: the test signalled nothing"
        );
    }

    #[allow(clippy::disallowed_methods, reason = "a test's child, run where the test runs")]
    fn child(script: &str) -> std::process::Child {
        std::process::Command::new("/bin/sh")
            .args(["-c", script])
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }

    #[test]
    fn a_childs_exit_waited_for_past_a_deadline_leaves_it_to_be_reaped() {
        let mut running = child("exec sleep 600");
        let pid = i32::try_from(running.id()).unwrap();
        let clock = Monotonic::new();
        let deadline = Deadline::after(clock.now(), ms(50));
        let waited = Direct(&Kernel).exited("a test's child", pid, true, Some(deadline), &clock);
        assert_eq!(waited.unwrap(), Waited::Deadline);
        assert!(deadline.passed(clock.now()));
        running.kill().unwrap();
        let deadline = Some(Deadline::after(clock.now(), LONG));
        let (what, plant) = ("a test's child", ReapPlant::None);
        let reaped = reaping::reap(what, &Direct(&Kernel), pid, deadline, &clock, plant);
        match reaped.unwrap() {
            Waited::Ready((status, _)) => {
                assert!(libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGKILL);
            }
            Waited::Deadline => panic!("a killed child was not reaped within 60 s"),
        }
        // Reaped: it is no child any more.
        let again = Direct(&Kernel).exited("a test's child", pid, true, None, &clock);
        let error = again.expect_err("a reaped child was waited for again");
        assert_eq!(error.io_error().and_then(io::Error::raw_os_error), Some(libc::ECHILD));
    }

    #[test]
    fn a_child_that_exits_is_reaped_with_its_status() {
        let exiting = child("sleep 0.05; exit 7");
        let pid = i32::try_from(exiting.id()).unwrap();
        let clock = Monotonic::new();
        let deadline = Some(Deadline::after(clock.now(), LONG));
        let (what, plant) = ("a test's child", ReapPlant::None);
        let reaped = reaping::reap(what, &Direct(&Kernel), pid, deadline, &clock, plant);
        match reaped.unwrap() {
            Waited::Ready((status, _)) => {
                assert!(libc::WIFEXITED(status));
                assert_eq!(libc::WEXITSTATUS(status), 7);
            }
            Waited::Deadline => panic!("a child that exits at once was not reaped within 60 s"),
        }
    }

    #[test]
    fn a_poll_ends_at_its_deadline_and_is_ready_once_written() {
        let mut ends = [0; 2];
        // SAFETY: two descriptors for the kernel to write.
        assert_eq!(unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        // SAFETY: the pipe's two ends, which no one else holds.
        let (read, write) =
            unsafe { (OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])) };
        let clock = Monotonic::new();
        let deadline = Deadline::after(clock.now(), ms(30));
        let polled = poll("a test's pipe", read.as_raw_fd(), libc::POLLIN, Some(deadline), &clock);
        assert_eq!(polled.unwrap(), Waited::Deadline);
        assert!(deadline.passed(clock.now()));
        // SAFETY: one byte of a live local written to the pipe's own end.
        assert_eq!(unsafe { libc::write(write.as_raw_fd(), [1u8].as_ptr().cast(), 1) }, 1);
        let deadline = Some(Deadline::after(clock.now(), LONG));
        match poll("a test's pipe", read.as_raw_fd(), libc::POLLIN, deadline, &clock).unwrap() {
            Waited::Ready(revents) => assert_ne!(revents & libc::POLLIN, 0),
            Waited::Deadline => panic!("a written pipe did not poll readable within 60 s"),
        }
    }

    #[test]
    fn a_sleep_ends_once_its_deadline_has_passed() {
        let clock = Monotonic::new();
        let deadline = Deadline::after(clock.now(), ms(25));
        sleep_until("a test's sleep", deadline, &clock).unwrap();
        let end = clock.now();
        assert!(deadline.passed(end) && end < LONG, "slept until {end:?}");
    }
}
