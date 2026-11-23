//! The thin driver of a suite's zygote and of each test child (docs/v2/plan/design/test.md,
//! "End state", piece 9): the system calls `blaze_run::zygote::Pool` and `blaze_run::child::Child`
//! ask for, and nothing else. The logic (what to fork, when to kill, how a run ended, what a
//! report is, what an operation means) is all in those sans-IO machines, simulated in blaze-run.
//!
//! In the zygote, [`run`]: the test rows are given to the pool; each [`Effect::Fork`] checks the
//! one-thread gate with the thread count read just before ([`threads`]), forks, and in the child
//! runs [`Child`]'s steps and the row; [`Effect::Stacks`] is `SIGQUIT` to the child,
//! [`Effect::Kill`] is `SIGKILL` to its process group. One `poll` waits on every child's two
//! pipes and a self-pipe its `SIGCHLD` handler writes, with the pool's next deadline as its
//! timeout; every wake reaps with `waitpid(WNOHANG)` until none is left. Each result goes to
//! `sink` as it is decided, in the order the runs ended, with its row.
//!
//! In a child, the panic path calls [`panicked`] and the launcher's recording wrappers call
//! [`observe`]; both reach the child's machine through one pointer set before the test's call
//! (the child is one thread until the test starts one; a second thread's panic waits for the
//! first's report to end the process).
//!
//! Platform facts this rests on, each to verify by a test (fork_smoke runs them on the host):
//! `sandbox_init` with flags 0 compiles a profile's text and applies it to this process, after
//! a fork and while it is one thread (libsystem_sandbox; public but deprecated; Nix's macOS
//! sandbox calls it so); `proc_pidinfo(PROC_PIDTASKINFO)`'s `pti_threadnum` counts this
//! process's threads; `PROC_PIDLISTFDS` lists its descriptors; a frame-pointer walk from a
//! signal handler crosses `_sigtramp`'s frame into the interrupted code; a Rust program built
//! with std loads no system framework. Linux: the test sandbox (namespaces, Landlock, seccomp as
//! blaze-proc-macro native_sandbox.rs makes them) is not built here, so [`run`] stops on Linux
//! with [`DriverError::NoSandbox`] (rule 9: where no sandbox can be made, blaze stops).

use core::ffi::{c_char, c_int};
use core::fmt::{self, Write as _};
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, AtomicUsize, Ordering};

use blaze_own::{AVec, Alloc, Vec};
use blaze_run::child::{Child, ChildError, Ending, SETUP, Spec, Step};
use blaze_run::oscall::{Allowed, Observe, Op};
use blaze_run::postings::FnName;
use blaze_run::result::Place;
use blaze_run::wire::{self, FRAMES_CAP, STACKS_CAP};
use blaze_run::zygote::{ChildId, Effect, Finished, ForkError, Gate, OneThread, Pool, PoolError, Status, Stream};
use blaze_run::{Nanos, TestId};

use crate::sys;

/// The suite the zygote serves: the launcher with the suite's code placed.
pub trait Suite {
    /// Row `row`'s child: its temporary folder, environment and control socket.
    fn spec(&self, row: u32) -> Spec<'_>;
    /// Runs row `row`'s function on this thread. Returns when it returned; a panic goes to
    /// [`panicked`], which does not return.
    fn call(&self, row: u32);
    /// The functions this process entered, from the launcher's marks (any order).
    fn entered(&self, out: &mut AVec<'_, FnName>);
    /// Starts the launcher's control thread in this child; the errno when it cannot.
    fn control(&self, socket: &str) -> Result<(), i32>;
}

/// The driver's clock (rule 75: passed in). A refusal is the errno the driver stops with.
pub trait Clock {
    fn now(&self) -> Result<Nanos, i32>;
}

/// `CLOCK_MONOTONIC`.
pub struct Monotonic;

impl Clock for Monotonic {
    fn now(&self) -> Result<Nanos, i32> {
        let mut at = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: `at` is a live local the call fills.
        match unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut at) } {
            0 => Ok(Nanos((at.tv_sec as u64).saturating_mul(1_000_000_000).saturating_add(at.tv_nsec as u64))),
            _ => Err(errno()),
        }
    }
}

fn read_clock<'s>(clock: &dyn Clock) -> Result<Nanos, DriverError<'s>> {
    clock.now().map_err(|errno| DriverError::Call { call: "clock_gettime(CLOCK_MONOTONIC)", errno, row: None })
}

/// Where the driver's waits are told (rule 15): the launcher's waits table.
pub trait Waits {
    fn waiting(&mut self, on: &'static str, until: Option<Nanos>);
    fn woke(&mut self);
}

/// One test row to run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Asked {
    pub row: u32,
    pub test: TestId,
    /// Its deadline, from its fork.
    pub budget: Nanos,
}

/// Why the zygote stopped. Each names the call and the row where there is one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DriverError<'s> {
    Pool(PoolError),
    Fork(ForkError),
    Spec { row: u32, error: ChildError<'s> },
    Call { call: &'static str, errno: i32, row: Option<u32> },
    /// A system framework is loaded: its state does not survive a fork (zygote.rs, "macOS").
    Framework { image: &'static str },
    /// No test sandbox is built for this platform.
    NoSandbox { platform: &'static str },
}

impl fmt::Display for DriverError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            DriverError::Pool(error) => write!(f, "{error}"),
            DriverError::Fork(error) => write!(f, "{error}"),
            DriverError::Spec { row, error } => write!(f, "test row {row}: {error}"),
            DriverError::Call { call, errno, row: Some(row) } => write!(f, "test row {row}: `{call}` failed: errno {errno}"),
            DriverError::Call { call, errno, row: None } => write!(f, "the test zygote's `{call}` failed: errno {errno}"),
            DriverError::Framework { image } => write!(
                f,
                "the test program loads {image}; a system framework's threads and Mach ports do not survive a fork, \
                 so its tests cannot run one process each from a zygote"
            ),
            DriverError::NoSandbox { platform } => write!(f, "no test sandbox is built for {platform}: blaze does not run tests unsandboxed"),
        }
    }
}

impl core::error::Error for DriverError<'_> {}

fn errno() -> i32 {
    // SAFETY: reads this thread's errno.
    #[cfg(target_os = "macos")]
    let at = unsafe { *libc::__error() };
    // SAFETY: reads this thread's errno.
    #[cfg(target_os = "linux")]
    let at = unsafe { *libc::__errno_location() };
    at
}

fn call<'s>(name: &'static str, row: Option<u32>) -> DriverError<'s> {
    DriverError::Call { call: name, errno: errno(), row }
}

/// The write end of the zygote's self-pipe, for its `SIGCHLD` handler.
static WAKE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_child(_: c_int) {
    let byte = 1u8;
    // SAFETY: `write` is async-signal-safe; a full pipe already holds a wake, so its EAGAIN
    // loses nothing.
    unsafe { libc::write(WAKE.load(Ordering::Relaxed), (&byte as *const u8).cast(), 1) };
}

/// The process's thread count now.
pub fn threads<'s>() -> Result<u32, DriverError<'s>> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: a plain C struct; all zeros is a valid value of it.
        let mut info: libc::proc_taskinfo = unsafe { core::mem::zeroed() };
        let size = core::mem::size_of::<libc::proc_taskinfo>() as c_int;
        // SAFETY: `info` is a live local of exactly `size` bytes.
        let got = unsafe {
            libc::proc_pidinfo(libc::getpid(), libc::PROC_PIDTASKINFO, 0, (&mut info as *mut libc::proc_taskinfo).cast(), size)
        };
        if got != size {
            return Err(call("proc_pidinfo(PROC_PIDTASKINFO)", None));
        }
        return Ok(info.pti_threadnum.max(0) as u32);
    }
    #[cfg(target_os = "linux")]
    {
        // Field 20 of /proc/self/stat: after the command's closing parenthesis, the 18th field.
        let mut buf = [0u8; 4096];
        // SAFETY: a constant NUL-terminated path.
        let fd = unsafe { libc::open(c"/proc/self/stat".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(call("open(/proc/self/stat)", None));
        }
        // SAFETY: `buf` is a live local of its length.
        let len = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        // SAFETY: `fd` was opened above.
        unsafe { libc::close(fd) };
        if len <= 0 {
            return Err(call("read(/proc/self/stat)", None));
        }
        let text = &buf[..len as usize];
        let Some(close) = text.iter().rposition(|byte| *byte == b')') else {
            return Err(DriverError::Call { call: "parse /proc/self/stat", errno: 0, row: None });
        };
        let Some(field) = text[close + 1..].split(|byte| *byte == b' ').filter(|part| !part.is_empty()).nth(17) else {
            return Err(DriverError::Call { call: "parse /proc/self/stat", errno: 0, row: None });
        };
        let mut count = 0u32;
        for digit in field {
            if !digit.is_ascii_digit() {
                return Err(DriverError::Call { call: "parse /proc/self/stat", errno: 0, row: None });
            }
            count = count.saturating_mul(10).saturating_add(u32::from(digit - b'0'));
        }
        return Ok(count);
    }
}

/// Refuses a test program that loaded a system framework (macOS).
fn no_frameworks<'s>() -> Result<(), DriverError<'s>> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: dyld's image list; each name is a NUL-terminated string dyld keeps for the
        // process's life.
        let count = unsafe { libc::_dyld_image_count() };
        for index in 0..count {
            // SAFETY: `index` is below the count read above (images are only added).
            let name = unsafe { libc::_dyld_get_image_name(index) };
            if name.is_null() {
                continue;
            }
            // SAFETY: non-null, NUL-terminated, kept for the process's life (above).
            let name: &'static core::ffi::CStr = unsafe { core::ffi::CStr::from_ptr(name) };
            let bytes = name.to_bytes();
            let framework = |marker: &[u8]| bytes.windows(marker.len()).any(|window| window == marker);
            if framework(b"/System/Library/Frameworks/") || framework(b"/System/Library/PrivateFrameworks/") {
                let image = core::str::from_utf8(bytes).unwrap_or("a system framework");
                return Err(DriverError::Framework { image });
            }
        }
    }
    Ok(())
}

fn pipe<'s>(row: Option<u32>) -> Result<[c_int; 2], DriverError<'s>> {
    let mut ends = [-1; 2];
    // SAFETY: `ends` is a live local of two descriptors.
    if unsafe { libc::pipe(ends.as_mut_ptr()) } != 0 {
        return Err(call("pipe", row));
    }
    Ok(ends)
}

fn parent_end<'s>(fd: c_int, row: Option<u32>) -> Result<(), DriverError<'s>> {
    // SAFETY: `fd` is a descriptor this process holds; the flags are plain integers.
    let ok = unsafe {
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) == 0
            && libc::fcntl(fd, libc::F_SETFL, libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK) == 0
    };
    match ok {
        true => Ok(()),
        false => Err(call("fcntl(O_NONBLOCK)", row)),
    }
}

/// Forks. Takes the one-thread proof by value: no fork without a count of one just before.
fn fork<'s>(proof: OneThread, row: u32) -> Result<libc::pid_t, DriverError<'s>> {
    let _ = proof;
    // SAFETY: the process is one thread (the proof above), so the child holds no lock another
    // thread held, and may call the allocator and the C library.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(call("fork", Some(row)));
    }
    Ok(pid)
}

struct Pipes {
    child: ChildId,
    report: c_int,
    output: c_int,
}

/// The self-pipe and the `SIGCHLD` handler that writes it, for one [`run`]; dropping it puts
/// the default disposition back and closes both ends.
struct Wake {
    ends: [c_int; 2],
}

impl Wake {
    fn new<'s>() -> Result<Wake, DriverError<'s>> {
        let wake = Wake { ends: pipe(None)? };
        parent_end(wake.ends[0], None)?;
        parent_end(wake.ends[1], None)?;
        WAKE.store(wake.ends[1], Ordering::Relaxed);
        // SAFETY: a plain C struct; all zeros is valid, then the fields set.
        let mut action: libc::sigaction = unsafe { core::mem::zeroed() };
        action.sa_sigaction = on_child as usize;
        action.sa_flags = libc::SA_RESTART | libc::SA_NOCLDSTOP;
        // SAFETY: the handler only writes one byte to a non-blocking pipe (async-signal-safe).
        if unsafe { libc::sigaction(libc::SIGCHLD, &action, core::ptr::null_mut()) } != 0 {
            return Err(call("sigaction(SIGCHLD)", None));
        }
        Ok(wake)
    }
}

impl Drop for Wake {
    fn drop(&mut self) {
        // SAFETY: the default disposition first, so no handler writes a closed descriptor;
        // then the two ends this value opened.
        unsafe {
            libc::signal(libc::SIGCHLD, libc::SIG_DFL);
            WAKE.store(-1, Ordering::Relaxed);
            libc::close(self.ends[0]);
            libc::close(self.ends[1]);
        }
    }
}

/// Runs `asked` in children of this process, `width` at once, handing each result to `sink` in
/// the order the runs ended.
pub fn run<'s, S: Suite>(
    suite: &'s S,
    asked: &[Asked],
    width: u32,
    clock: &dyn Clock,
    waits: &mut dyn Waits,
    alloc: Alloc<'_>,
    sink: &mut dyn FnMut(&Finished<'_>),
) -> Result<(), DriverError<'s>> {
    if cfg!(target_os = "linux") {
        return Err(DriverError::NoSandbox { platform: "linux" });
    }
    no_frameworks()?;
    let mut pool = Pool::new(width, alloc).map_err(DriverError::Pool)?;
    for one in asked {
        pool.ask(one.row, one.test, one.budget).map_err(DriverError::Pool)?;
    }
    let guard = Wake::new()?;
    let wake = guard.ends;

    let mut gate = Gate::new();
    let mut effects: AVec<'_, Effect> = Vec::new_in(alloc);
    let mut pipes: AVec<'_, Pipes> = Vec::new_in(alloc);
    let mut polled: AVec<'_, libc::pollfd> = Vec::new_in(alloc);
    let mut buf = [0u8; 64 * 1024];
    loop {
        let now = read_clock(clock)?;
        pool.pump(&mut effects);
        pool.tick(now, &mut effects);
        let mut at = 0;
        while at < effects.len() {
            match effects[at] {
                Effect::Fork { row } => {
                    let made = fork_one(suite, row, &mut gate, alloc)?;
                    pool.forked(row, made.child, read_clock(clock)?).map_err(DriverError::Pool)?;
                    pipes.push(made);
                }
                Effect::Stacks { child } => {
                    // SAFETY: a signal to a child of ours; one already gone gives ESRCH, which
                    // changes nothing.
                    unsafe { libc::kill(child.0 as libc::pid_t, libc::SIGQUIT) };
                }
                Effect::Kill { child } => {
                    // Our child's process group (its pid, set by both sides before any test code
                    // ran), through blaze-sys's one group kill (D177). A group already gone
                    // (ESRCH) is the end the kill asks for; any other answer is named.
                    #[cfg(target_os = "linux")]
                    let killed = blaze_sys::linux::kill_group(child.0 as i32, 9);
                    #[cfg(target_os = "macos")]
                    let killed = blaze_sys::seatbelt::kill_group(child.0 as i32, 9);
                    match killed {
                        Ok(()) | Err(blaze_sys::Errno::Srch) => {}
                        Err(errno) => return Err(DriverError::Call { call: "kill_group", errno: i32::from(errno.raw()), row: None }),
                    }
                }
            }
            at += 1;
        }
        effects.clear();
        while let Some(finished) = pool.next_finished() {
            sink(&finished);
        }
        if pool.idle() {
            return Ok(());
        }

        polled.clear();
        polled.push(libc::pollfd { fd: wake[0], events: libc::POLLIN, revents: 0 });
        for one in pipes.iter() {
            for fd in [one.report, one.output] {
                if fd >= 0 {
                    polled.push(libc::pollfd { fd, events: libc::POLLIN, revents: 0 });
                }
            }
        }
        let until = pool.next_wake();
        let timeout = match until {
            Some(until) => until.0.saturating_sub(now.0).div_ceil(1_000_000).min(60_000) as c_int,
            // Only children decided at a kill and not yet reaped: the reap's SIGCHLD wakes us;
            // the bound keeps the wait finite (rule 18).
            None => 1_000,
        };
        waits.waiting("a test child's report, output or end", until);
        // SAFETY: `polled` is a live array of its length.
        let ready = unsafe { libc::poll(polled.as_mut_ptr(), polled.len() as libc::nfds_t, timeout) };
        waits.woke();
        if ready < 0 && errno() != libc::EINTR {
            return Err(call("poll", None));
        }
        let now = read_clock(clock)?;
        for slot in polled.iter() {
            if slot.revents == 0 {
                continue;
            }
            if slot.fd == wake[0] {
                // SAFETY: drains the self-pipe into `buf`; non-blocking.
                while unsafe { libc::read(wake[0], buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
                continue;
            }
            let Some(one) = pipes.iter_mut().find(|one| one.report == slot.fd || one.output == slot.fd) else { continue };
            let stream = if one.report == slot.fd { Stream::Report } else { Stream::Output };
            loop {
                // SAFETY: `slot.fd` is this child's open, non-blocking pipe; `buf` a live local.
                let got = unsafe { libc::read(slot.fd, buf.as_mut_ptr().cast(), buf.len()) };
                if got > 0 {
                    pool.bytes(one.child, stream, &buf[..got as usize], now, &mut effects).map_err(DriverError::Pool)?;
                    continue;
                }
                if got == 0 {
                    // SAFETY: closes the descriptor read to its end; not used again.
                    unsafe { libc::close(slot.fd) };
                    match stream {
                        Stream::Report => one.report = -1,
                        Stream::Output => one.output = -1,
                    }
                    pool.closed(one.child, stream, now).map_err(DriverError::Pool)?;
                    break;
                }
                match errno() {
                    libc::EINTR => continue,
                    libc::EAGAIN => break,
                    _ => return Err(call("read(test child's pipe)", None)),
                }
            }
        }
        pipes.retain(|one| one.report >= 0 || one.output >= 0);
        loop {
            let mut status: c_int = 0;
            // SAFETY: `status` is a live local; WNOHANG never blocks.
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid == 0 || (pid < 0 && errno() == libc::ECHILD) {
                break;
            }
            if pid < 0 {
                return Err(call("waitpid", None));
            }
            let status = if libc::WIFEXITED(status) {
                Status::Exited { code: libc::WEXITSTATUS(status) }
            } else if libc::WIFSIGNALED(status) {
                Status::Signaled { number: libc::WTERMSIG(status) }
            } else {
                return Err(DriverError::Call { call: "waitpid: a status neither exited nor signaled", errno: status, row: None });
            };
            pool.exited(ChildId(pid as u32), status, now, &mut effects).map_err(DriverError::Pool)?;
        }
    }
}

/// One fork: the child never returns from here.
fn fork_one<'s, S: Suite>(
    suite: &'s S,
    row: u32,
    gate: &mut Gate,
    alloc: Alloc<'_>,
) -> Result<Pipes, DriverError<'s>> {
    let mut child = Child::new(suite.spec(row), alloc).map_err(|error| DriverError::Spec { row, error })?;
    let report = pipe(Some(row))?;
    let output = pipe(Some(row))?;
    let proof = gate.check(threads()?).map_err(DriverError::Fork)?;
    let pid = fork(proof, row)?;
    if pid == 0 {
        // The child. The zygote's descriptors (its self-pipe, the other children's pipes, the
        // read ends here) are closed by the descriptors step: every one above 3.
        child_main(suite, row, &mut child, report[1], output[1], alloc);
    }
    // SAFETY: the write ends are the child's now; the zygote's copies are closed so an end of
    // file comes when the child's do.
    unsafe {
        libc::close(report[1]);
        libc::close(output[1]);
    }
    // The child sets its own group first; this closes the window before it does. ESRCH: it has
    // already ended; EACCES: it already ran `exec` (a test that replaced itself) after its own
    // `setpgid`. Both leave the group as it must be.
    // SAFETY: `pid` is our child.
    if unsafe { libc::setpgid(pid, pid) } != 0 && !matches!(errno(), libc::ESRCH | libc::EACCES) {
        return Err(call("setpgid", Some(row)));
    }
    parent_end(report[0], Some(row))?;
    parent_end(output[0], Some(row))?;
    Ok(Pipes { child: ChildId(pid as u32), report: report[0], output: output[0] })
}

// In a child.

/// What the panic path and the wrappers reach: set before the test's call, never cleared (the
/// process ends with `_exit`).
struct Running<'c, 's, 'a> {
    child: &'c mut Child<'s, 'a>,
    suite: &'c dyn Suite,
    alloc: Alloc<'a>,
}

static RUNNING: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static REPORTING: AtomicBool = AtomicBool::new(false);
static ROW: AtomicU32 = AtomicU32::new(0);
static STACK_LOW: AtomicUsize = AtomicUsize::new(0);
static STACK_HIGH: AtomicUsize = AtomicUsize::new(0);

/// A fixed buffer `fmt::Write` writes into, for messages written with no allocation.
struct Line {
    bytes: [u8; 256],
    len: usize,
}

impl fmt::Write for Line {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let take = text.len().min(self.bytes.len() - self.len);
        self.bytes[self.len..self.len + take].copy_from_slice(&text.as_bytes()[..take]);
        self.len += take;
        Ok(())
    }
}

fn write_all(fd: c_int, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        // SAFETY: `bytes` is live; `write` is async-signal-safe.
        let wrote = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if wrote > 0 {
            bytes = &bytes[wrote as usize..];
        } else if wrote < 0 && errno() == libc::EINTR {
            continue;
        } else {
            // The pipe's reader is gone (the zygote ended): nothing can hear this child.
            return;
        }
    }
}

/// A step failed: says which on descriptor 2 and ends with `SETUP + step`.
fn setup_failed(row: u32, step: &Step<'_>, errno: i32) -> ! {
    let mut line = Line { bytes: [0; 256], len: 0 };
    let _ = writeln!(line, "blaze: test row {row}: could not make {}: errno {errno}", step.name());
    write_all(2, &line.bytes[..line.len]);
    // SAFETY: ends this child at once: no atexit handler of the zygote's runs here.
    unsafe { libc::_exit(SETUP + step.index()) }
}

fn child_main<'a>(suite: &dyn Suite, row: u32, child: &mut Child<'_, 'a>, report: c_int, output: c_int, alloc: Alloc<'a>) -> ! {
    ROW.store(row, Ordering::Relaxed);
    // Kept on `child_main`'s stack until `_exit` so POSIX `environ` points at live memory
    // without leaking.
    let mut env_keep: Option<(AVec<'a, AVec<'a, u8>>, AVec<'a, *mut c_char>)> = None;
    loop {
        let Some(step) = child.next() else { break };
        let failed = match step {
            Step::OwnGroup => {
                // SAFETY: this process's own group.
                (unsafe { libc::setpgid(0, 0) } != 0).then(errno)
            }
            Step::Signals => signals(),
            Step::Descriptors => descriptors(report, output, alloc),
            Step::TempFolder { path } => folder(path, alloc),
            Step::Environment { vars, temp } => environment(vars, temp, alloc, &mut env_keep),
            Step::Sandbox { profile } => sandbox(profile, alloc),
            Step::Control { socket } => suite.control(socket).err(),
            // The spec's row, the one forked for.
            Step::Call { .. } => break,
        };
        if let Some(errno) = failed {
            setup_failed(row, &step, errno);
        }
    }
    let mut running = Running { child, suite, alloc };
    RUNNING.store((&mut running as *mut Running<'_, '_, '_>).cast::<()>(), Ordering::Release);
    suite.call(row);
    let _ = &env_keep;
    end(Ending::Returned)
}

/// The test's panic: its message and place, reported once; the child ends. Called by the
/// program's panic path in a test child.
pub fn panicked(message: &str, at: Option<Place>) -> ! {
    end(Ending::Panicked { message, at })
}

/// What a recording wrapper calls before each operation it wraps, in a test child. A refusal
/// ends the test at once, as refused; so only [`Allowed::Go`] returns. In the zygote (no test
/// running) nothing is a test's and every call goes.
pub fn observe(op: Op<'_>) -> Allowed {
    let ptr = RUNNING.load(Ordering::Acquire);
    if ptr.is_null() {
        return Allowed::Go;
    }
    // SAFETY: set by `child_main` to its live `Running`, which lives until `_exit`; the test
    // is this process's one thread while it observes (a second thread's calls are the
    // `Threads` reason's, not built here).
    let running = unsafe { &mut *ptr.cast::<Running<'_, '_, '_>>() };
    match running.child.saw(op) {
        Allowed::Go => Allowed::Go,
        Allowed::Stop => end(Ending::Returned),
    }
}

fn end(ending: Ending<'_>) -> ! {
    if REPORTING.swap(true, Ordering::AcqRel) {
        // Another thread is writing the report and will end the process; this one waits for
        // that (the deadline kills the group if it never comes).
        loop {
            // SAFETY: waits for a signal; the process ends under it.
            unsafe { libc::pause() };
        }
    }
    let ptr = RUNNING.load(Ordering::Acquire);
    if ptr.is_null() {
        write_all(2, b"blaze: a test's end was reported outside a test child\n");
        // SAFETY: ends the process at once.
        unsafe { libc::_exit(SETUP + 7) }
    }
    // SAFETY: as in `observe`; REPORTING makes this the only reference made from here on.
    let running = unsafe { &mut *ptr.cast::<Running<'_, '_, '_>>() };
    let mut entered: AVec<'_, FnName> = Vec::new_in(running.alloc);
    running.suite.entered(&mut entered);
    match running.child.report(ending, entered.as_mut_slice(), running.alloc) {
        Ok(bytes) => {
            write_all(3, bytes.as_slice());
            // SAFETY: ends the child with its report written; no atexit handler runs.
            unsafe { libc::_exit(0) }
        }
        Err(error) => {
            let mut line = Line { bytes: [0; 256], len: 0 };
            let _ = writeln!(line, "blaze: test row {}: no report: {error}", ROW.load(Ordering::Relaxed));
            write_all(2, &line.bytes[..line.len]);
            // SAFETY: ends the child with no report: the zygote fails it as such.
            unsafe { libc::_exit(SETUP + 7) }
        }
    }
}

/// `SIGQUIT`: this thread's return addresses, as one `Stacks` message on descriptor 3.
extern "C" fn on_stacks(_: c_int) {
    let (low, high) = (STACK_LOW.load(Ordering::Relaxed), STACK_HIGH.load(Ordering::Relaxed));
    let mut frames = [0u64; FRAMES_CAP];
    let mut count = 0;
    let mut fp = sys::frame_pointer();
    while count < FRAMES_CAP && fp >= low && fp.saturating_add(16) <= high && fp % 8 == 0 {
        // SAFETY: `fp` is 8-aligned and its two words lie inside the test thread's stack,
        // whose bounds were read before the test started; a frame record is the caller's frame
        // pointer and then the return address. A signal that lands on another thread fails
        // the bounds check and gives no frames.
        let (next, back) = unsafe { (*(fp as *const usize), *((fp + 8) as *const usize)) };
        if back == 0 {
            break;
        }
        frames[count] = back as u64;
        count += 1;
        if next <= fp {
            break;
        }
        fp = next;
    }
    let mut message = [0u8; STACKS_CAP];
    if let Ok(len) = wire::encode_stacks_into(ROW.load(Ordering::Relaxed), &frames[..count], &mut message) {
        write_all(3, &message[..len]);
    }
}

fn signals() -> Option<i32> {
    for number in 1..32 {
        if number == libc::SIGKILL || number == libc::SIGSTOP {
            continue;
        }
        // SAFETY: a default disposition for a valid signal number.
        if unsafe { libc::signal(number, libc::SIG_DFL) } == libc::SIG_ERR {
            return Some(errno());
        }
    }
    // SAFETY: an empty set, made by `sigemptyset`, then set as the mask.
    unsafe {
        let mut none: libc::sigset_t = core::mem::zeroed();
        libc::sigemptyset(&mut none);
        if libc::sigprocmask(libc::SIG_SETMASK, &none, core::ptr::null_mut()) != 0 {
            return Some(errno());
        }
    }
    let Ok((low, high)) = sys::stack() else { return Some(0) };
    STACK_LOW.store(low, Ordering::Relaxed);
    STACK_HIGH.store(high, Ordering::Relaxed);
    // SAFETY: a plain C struct, zeros then set; the handler allocates nothing and takes no
    // lock (`on_stacks`).
    unsafe {
        let mut action: libc::sigaction = core::mem::zeroed();
        action.sa_sigaction = on_stacks as usize;
        action.sa_flags = libc::SA_RESTART;
        if libc::sigaction(libc::SIGQUIT, &action, core::ptr::null_mut()) != 0 {
            return Some(errno());
        }
    }
    None
}

fn descriptors(report: c_int, output: c_int, alloc: Alloc<'_>) -> Option<i32> {
    // SAFETY: descriptors this child holds; F_DUPFD above 3 first so no target is clobbered
    // before it is copied.
    unsafe {
        let report = libc::fcntl(report, libc::F_DUPFD, 4);
        let output = libc::fcntl(output, libc::F_DUPFD, 4);
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY);
        if report < 0 || output < 0 || null < 0 {
            return Some(errno());
        }
        if libc::dup2(null, 0) < 0 || libc::dup2(output, 1) < 0 || libc::dup2(output, 2) < 0 || libc::dup2(report, 3) < 0 {
            return Some(errno());
        }
    }
    close_above_three(alloc)
}

/// Closes every descriptor above 3: the zygote's ends of its other children's pipes, its
/// self-pipe, its socket.
fn close_above_three(alloc: Alloc<'_>) -> Option<i32> {
    #[cfg(target_os = "linux")]
    {
        let _ = alloc;
        // SAFETY: close_range(4, ~0, 0): closes descriptors this process holds.
        if unsafe { libc::syscall(libc::SYS_close_range, 4u32, u32::MAX, 0u32) } != 0 {
            return Some(errno());
        }
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        let size = core::mem::size_of::<libc::proc_fdinfo>();
        // SAFETY: a null buffer asks for the size the list needs.
        let need = unsafe { libc::proc_pidinfo(libc::getpid(), libc::PROC_PIDLISTFDS, 0, core::ptr::null_mut(), 0) };
        if need <= 0 {
            return Some(errno());
        }
        let room = need as usize / size + 16;
        let mut list: AVec<'_, libc::proc_fdinfo> = Vec::with_capacity_in(room, alloc);
        for _ in 0..room {
            list.push(libc::proc_fdinfo { proc_fd: -1, proc_fdtype: 0 });
        }
        // SAFETY: `list` holds `room` entries, the bytes passed.
        let got = unsafe {
            libc::proc_pidinfo(libc::getpid(), libc::PROC_PIDLISTFDS, 0, list.as_mut_ptr().cast::<core::ffi::c_void>(), (room * size) as c_int)
        };
        if got <= 0 {
            return Some(errno());
        }
        for one in &list[..got as usize / size] {
            if one.proc_fd > 3 {
                // SAFETY: a descriptor the list says this process holds.
                unsafe { libc::close(one.proc_fd) };
            }
        }
        return None;
    }
}

/// A NUL-terminated copy of `text` in `alloc`.
fn c_text<'a>(text: &str, alloc: Alloc<'a>) -> AVec<'a, u8> {
    let mut out = Vec::with_capacity_in(text.len() + 1, alloc);
    out.extend_from_slice(text.as_bytes());
    out.push(0);
    out
}

fn folder(path: &str, alloc: Alloc<'_>) -> Option<i32> {
    let path = c_text(path, alloc);
    // SAFETY: a NUL-terminated path in a live buffer.
    (unsafe { libc::mkdir(path.as_ptr().cast::<c_char>(), 0o700) } != 0).then(errno)
}

fn environment<'a>(
    vars: &[(&str, &str)],
    temp: &str,
    alloc: Alloc<'a>,
    keep: &mut Option<(AVec<'a, AVec<'a, u8>>, AVec<'a, *mut c_char>)>,
) -> Option<i32> {
    let mut lines: AVec<'a, AVec<'a, u8>> = Vec::with_capacity_in(vars.len() + 1, alloc);
    let mut texts: AVec<'a, *mut c_char> = Vec::with_capacity_in(vars.len() + 2, alloc);
    let mut add = |name: &str, value: &str| {
        let mut line: AVec<'a, u8> = Vec::with_capacity_in(name.len() + value.len() + 2, alloc);
        line.extend_from_slice(name.as_bytes());
        line.push(b'=');
        line.extend_from_slice(value.as_bytes());
        line.push(0);
        texts.push(line.as_mut_ptr().cast::<c_char>());
        lines.push(line);
    };
    for (name, value) in vars {
        add(name, value);
    }
    add("TMPDIR", temp);
    texts.push(core::ptr::null_mut());
    let (_, table_vec) = keep.insert((lines, texts));
    let table = table_vec.as_mut_ptr();
    // SAFETY: the process is one thread; `table` and every entry live in `keep` on `child_main`'s
    // diverging stack frame until `_exit`.
    #[cfg(target_os = "macos")]
    unsafe {
        *libc::_NSGetEnviron() = table;
    }
    #[cfg(target_os = "linux")]
    {
        unsafe extern "C" {
            static mut environ: *mut *mut c_char;
        }
        // SAFETY: as above.
        unsafe { environ = table };
    }
    None
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// libsystem_sandbox: applies a Seatbelt profile's text (flags 0) to this process; public,
    /// deprecated, and what Nix's macOS sandbox calls between fork and exec. Its symbol is
    /// checked by fork_smoke's run.
    fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> c_int;
}

fn sandbox(profile: &str, alloc: Alloc<'_>) -> Option<i32> {
    #[cfg(target_os = "macos")]
    {
        let text = c_text(profile, alloc);
        let mut error: *mut c_char = core::ptr::null_mut();
        // SAFETY: a NUL-terminated profile in a live buffer; the process is one thread.
        let refused = unsafe { sandbox_init(text.as_ptr().cast::<c_char>(), 0, &mut error) };
        if refused != 0 {
            if !error.is_null() {
                // SAFETY: Seatbelt's NUL-terminated message.
                let said = unsafe { core::ffi::CStr::from_ptr(error) };
                write_all(2, b"blaze: the test sandbox's profile was refused: ");
                write_all(2, said.to_bytes());
                write_all(2, b"\n");
            }
            return Some(refused);
        }
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        let _ = (profile, alloc);
        // `run` stops before any fork on Linux; a child here has no sandbox to enter.
        return Some(libc::ENOSYS);
    }
}
