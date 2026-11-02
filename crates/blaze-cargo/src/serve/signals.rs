//! SIGTERM and SIGINT as a flag the loop reads between turns, SIGUSR1 as an ask to write what
//! every thread is inside to the daemon's log ([`dump_asked`]), and a line in the daemon's log
//! for each signal that ends it ([`record_ends`]), a fault too, said before std's handler has
//! it. The one place `serve` uses `unsafe`:
//! installing a handler is a libc call, and std has no safe way to do it.

#[cfg(unix)]
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicUsize};
use std::sync::atomic::{AtomicBool, Ordering};

/// What the line a signal that ends the daemon writes starts with, then its pid, then
/// [`ENDED_BY`], the signal's number and its name: `blaze serve: pid 7 ended by signal 6,
/// SIGABRT: process abort signal`. The client quotes it from the log.
const PID_SAYS: &str = "blaze serve: pid ";
const ENDED_BY: &str = " ended by signal ";

static SIGNALLED: AtomicBool = AtomicBool::new(false);

/// The first SIGTERM or SIGINT's number, 0 before one, then who sent it: the kernel's word in
/// its `siginfo_t`.
#[cfg(unix)]
static SIGNAL: AtomicI32 = AtomicI32::new(0);
#[cfg(unix)]
static SENDER_PID: AtomicI32 = AtomicI32::new(0);
#[cfg(unix)]
static SENDER_UID: AtomicU32 = AtomicU32::new(0);

/// The first SIGTERM or SIGINT, and the process that sent it, as the daemon's log says it:
/// `SIGTERM from pid 7 (uid 501)`. Pid 0 is the kernel's, or a terminal's.
#[cfg(unix)]
pub fn stopped_by() -> Option<String> {
    if !signalled() {
        return None;
    }
    let name = match SIGNAL.load(Ordering::Relaxed) {
        libc::SIGTERM => "SIGTERM",
        libc::SIGINT => "SIGINT",
        _ => "a signal",
    };
    let (pid, uid) = (SENDER_PID.load(Ordering::Relaxed), SENDER_UID.load(Ordering::Relaxed));
    Some(format!("{name} from pid {pid} (uid {uid})")) // alloc: once, as the daemon stops
}

#[cfg(not(unix))]
pub fn stopped_by() -> Option<String> {
    signalled().then(|| "a stop signal".to_owned()) // alloc: once, as the daemon stops
}

static DUMP: AtomicBool = AtomicBool::new(false);

/// Whether SIGTERM or SIGINT has arrived since [`catch`].
pub fn signalled() -> bool {
    SIGNALLED.load(Ordering::Acquire)
}

/// Whether SIGUSR1 arrived since the last call: the loop then writes what every thread is
/// inside ([`blaze_profile::watchdog::running`]) to the daemon's log, and the daemon goes on.
pub fn dump_asked() -> bool {
    DUMP.swap(false, Ordering::AcqRel)
}

/// Turns SIGTERM and SIGINT into [`signalled`], and SIGUSR1 into [`dump_asked`], for the rest
/// of the process.
#[cfg(unix)]
pub fn catch() {
    extern "C" fn on(signal: libc::c_int, info: *mut libc::siginfo_t, _: *mut libc::c_void) {
        if SIGNAL.compare_exchange(0, signal, Ordering::Relaxed, Ordering::Relaxed).is_ok() && !info.is_null() {
            // SAFETY: an `SA_SIGINFO` handler's `info` is the kernel's, whole and live for the
            // handler; a SIGTERM or SIGINT's sender fields are the ones it filled.
            let (pid, uid) = unsafe { ((*info).si_pid(), (*info).si_uid()) };
            SENDER_PID.store(pid, Ordering::Relaxed);
            SENDER_UID.store(uid, Ordering::Relaxed);
        }
        SIGNALLED.store(true, Ordering::Release);
    }
    // SAFETY: `sigaction` is integers, a mask of integers and a handler address; zeroed is
    // its empty value.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction =
        on as extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) as libc::sighandler_t;
    action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
    for signal in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: `action` is live and whole; its handler, `on`, only does atomic operations
        // and reads `info`, which are async-signal-safe; were it to lock or allocate, a signal
        // arriving inside malloc could deadlock or corrupt the heap.
        unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) };
    }
    catch_dump();
}

/// Turns SIGUSR1 into [`dump_asked`].
#[cfg(unix)]
fn catch_dump() {
    extern "C" fn dump(_: libc::c_int) {
        DUMP.store(true, Ordering::Release);
    }
    let dump = dump as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // SAFETY: `dump` only stores to an atomic, as `catch`'s handler does; the dump itself is
    // written by the loop, outside the handler.
    unsafe { libc::signal(libc::SIGUSR1, dump) };
}

#[cfg(not(unix))]
pub fn catch() {}

/// The signals that end a process by default and that a handler sees: SIGKILL and SIGSTOP
/// can't be caught, std ignores SIGPIPE, [`catch`] has SIGTERM, SIGINT and SIGUSR1, and SIGSEGV
/// and SIGBUS have std's handlers, which [`record_faults`] says each in front of.
#[cfg(unix)]
const ENDING: [libc::c_int; 13] = [
    libc::SIGHUP,
    libc::SIGQUIT,
    libc::SIGILL,
    libc::SIGTRAP,
    libc::SIGABRT,
    libc::SIGFPE,
    libc::SIGUSR2,
    libc::SIGALRM,
    libc::SIGXCPU,
    libc::SIGXFSZ,
    libc::SIGVTALRM,
    libc::SIGPROF,
    libc::SIGSYS,
];

/// Has each of [`ENDING`] that has its default action write `blaze serve: pid N ended by
/// signal S, NAME: ...` to stderr (the daemon's log) and then end the process as the default
/// would, so a client the daemon hung up on can say how it ended. A signal something else
/// handles or ignores is left so. A process forked from the daemon has the handler until it
/// runs a program (blaze-core's spawn resets every one before its exec) and says its own pid.
#[cfg(unix)]
pub fn record_ends() {
    extern "C" fn ended(signal: libc::c_int) {
        let mut line = Line { bytes: [0; 160], len: 0 };
        line.put(PID_SAYS.as_bytes());
        // SAFETY: `getpid` takes nothing and is async-signal-safe.
        line.number(u64::from(unsafe { libc::getpid() }.unsigned_abs()));
        line.put(ENDED_BY.as_bytes());
        line.number(u64::from(signal.unsigned_abs()));
        line.put(blaze_core::signal::described(signal).as_bytes());
        line.put(b"\n");
        // SAFETY: `line.bytes` is live for the call and `line.len` of it are written; `write`
        // is async-signal-safe. A short or failed write loses the line, nothing else.
        unsafe { libc::write(libc::STDERR_FILENO, line.bytes.as_ptr().cast(), line.len) };
        // SAFETY: `SA_RESETHAND` made the signal's action the default again as this handler
        // was entered, and `SA_NODEFER` left it unblocked, so this ends the process as the
        // signal would have; `raise` is async-signal-safe.
        unsafe { libc::raise(signal) };
    }
    for signal in ENDING {
        // SAFETY: `sigaction` is integers, a mask of integers and a handler address, for
        // which all-zero bytes are a valid value (no handler, no flags, an empty mask).
        let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: `old` is live for the call to fill; a null new action changes nothing.
        if unsafe { libc::sigaction(signal, std::ptr::null(), &mut old) } != 0 || old.sa_sigaction != libc::SIG_DFL {
            continue;
        }
        // SAFETY: as for `old`.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = ended as extern "C" fn(libc::c_int) as libc::sighandler_t;
        action.sa_flags = libc::SA_RESETHAND | libc::SA_NODEFER | libc::SA_ONSTACK;
        // SAFETY: `action.sa_mask` is a live `sigset_t` of this frame for the call to fill.
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        // SAFETY: `action` is live and whole for the call, and a null old action is allowed.
        // Its handler `ended` writes from its own stack and calls `getpid`, `write` and
        // `raise`, all async-signal-safe: it neither allocates nor locks, which could deadlock
        // a process interrupted inside malloc or holding the lock.
        unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) };
    }
    record_faults();
}

/// The faults [`record_faults`] says before the handler that was there (std's) has them.
#[cfg(unix)]
const FAULTS: [libc::c_int; 2] = [libc::SIGSEGV, libc::SIGBUS];

/// Each of [`FAULTS`]' handler and flags before [`record_faults`]: atomics, as the handler reads
/// them inside a signal.
#[cfg(unix)]
static BEFORE: [(AtomicUsize, AtomicI32); 2] = [const { (AtomicUsize::new(libc::SIG_DFL), AtomicI32::new(0)) }; 2];

/// Has SIGSEGV and SIGBUS write `blaze serve: pid N ended by signal S, NAME: ... at 0xADDR on
/// thread T (tid I)` to stderr, then hands the signal to the handler that was there with what
/// it was given: std's, which says a stack overflow and aborts, and leaves any other fault to
/// the default (the fault comes again, and ends the process). A fault ends the log with a line
/// that names it, never silently.
#[cfg(unix)]
fn record_faults() {
    extern "C" fn faulted(signal: libc::c_int, info: *mut libc::siginfo_t, context: *mut libc::c_void) {
        let mut line = Line { bytes: [0; 160], len: 0 };
        line.put(PID_SAYS.as_bytes());
        // SAFETY: `getpid` takes nothing and is async-signal-safe.
        line.number(u64::from(unsafe { libc::getpid() }.unsigned_abs()));
        line.put(ENDED_BY.as_bytes());
        line.number(u64::from(signal.unsigned_abs()));
        line.put(blaze_core::signal::described(signal).as_bytes());
        line.put(b" at 0x");
        // SAFETY: the kernel passes a live `siginfo_t` to an `SA_SIGINFO` handler.
        line.hex(unsafe { fault_address(info) } as u64);
        line.put(b" on thread ");
        // Room for the line's end within its 160 bytes: Linux names are at most 15 bytes,
        // and a longer macOS one is cut.
        let mut name = [0u8; 32];
        // SAFETY: `name` is live and its length is passed. For the calling thread
        // `pthread_getname_np` copies the name the thread keeps (Linux: `prctl`, a system
        // call), with no lock and no allocation.
        let named =
            unsafe { libc::pthread_getname_np(libc::pthread_self(), name.as_mut_ptr().cast(), name.len()) } == 0;
        let len = name.iter().position(|&it| it == 0).unwrap_or(name.len());
        line.put(if named && len > 0 { &name[..len] } else { b"<unnamed>" });
        line.put(b" (tid ");
        line.number(thread_id());
        line.put(b")\n");
        // SAFETY: as in `record_ends`'s handler: `write` of the line's written bytes.
        unsafe { libc::write(libc::STDERR_FILENO, line.bytes.as_ptr().cast(), line.len) };
        let at = usize::from(signal != libc::SIGSEGV);
        let (handler, flags) = (BEFORE[at].0.load(Ordering::Acquire), BEFORE[at].1.load(Ordering::Acquire));
        if handler == libc::SIG_DFL || handler == libc::SIG_IGN {
            // SAFETY: as for `old` in `record_ends`.
            let mut default: libc::sigaction = unsafe { std::mem::zeroed() };
            default.sa_sigaction = libc::SIG_DFL;
            // SAFETY: `default` is live and whole; `sigaction` is async-signal-safe. Returning
            // runs the faulting instruction again, which the default then ends the process on.
            unsafe { libc::sigaction(signal, &default, std::ptr::null_mut()) };
            // SAFETY: `info` is live, as above. A fault another process sent comes no second
            // time by itself: it is raised again.
            if unsafe { (*info).si_code } <= 0 {
                // SAFETY: `raise` is async-signal-safe.
                unsafe { libc::raise(signal) };
            }
            return;
        }
        if flags & libc::SA_SIGINFO != 0 {
            // SAFETY: `handler` was the `sa_sigaction` of an `SA_SIGINFO` action, so it is an
            // `extern "C" fn(c_int, *mut siginfo_t, *mut c_void)`, given what this one was.
            let handler: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) =
                unsafe { std::mem::transmute(handler) };
            handler(signal, info, context);
        } else {
            // SAFETY: an action without `SA_SIGINFO` has an `extern "C" fn(c_int)` handler.
            let handler: extern "C" fn(libc::c_int) = unsafe { std::mem::transmute(handler) };
            handler(signal);
        }
    }
    for (at, signal) in FAULTS.into_iter().enumerate() {
        // SAFETY: as for `old` in `record_ends`.
        let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: `old` is live for the call to fill; a null new action changes nothing.
        if unsafe { libc::sigaction(signal, std::ptr::null(), &mut old) } != 0 {
            continue;
        }
        BEFORE[at].0.store(old.sa_sigaction, Ordering::Release);
        BEFORE[at].1.store(old.sa_flags, Ordering::Release);
        // SAFETY: as for `old`.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction =
            faulted as extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) as libc::sighandler_t;
        // On the alternate stack std gives each thread, as std's own handler is: a stack
        // overflow leaves no room on the thread's stack.
        action.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
        // SAFETY: `action.sa_mask` is a live `sigset_t` of this frame for the call to fill.
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        // SAFETY: `action` is live and whole. `faulted` writes from its own stack and calls
        // only async-signal-safe functions, then the handler that was there, as it would have
        // been called.
        unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) };
    }
}

/// The address a fault was at.
///
/// # Safety
///
/// `info` is the live `siginfo_t` of an `SA_SIGINFO` handler.
#[cfg(unix)]
unsafe fn fault_address(info: *const libc::siginfo_t) -> usize {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    // SAFETY: as this function's own `# Safety` says.
    let address = unsafe { (*info).si_addr() };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    // SAFETY: as this function's own `# Safety` says.
    let address = unsafe { (*info).si_addr };
    address.addr()
}

/// The calling thread's id as the system names it, as Rust's panic message does.
#[cfg(unix)]
fn thread_id() -> u64 {
    #[cfg(target_os = "macos")]
    {
        let mut id = 0u64;
        // SAFETY: `id` is live for the call; a null thread is the calling one. It reads the
        // thread's own record: no lock, no allocation.
        unsafe { libc::pthread_threadid_np(0, &mut id) };
        id
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `gettid` takes nothing and is a system call.
        u64::try_from(unsafe { libc::syscall(libc::SYS_gettid) }).unwrap_or(0)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        0
    }
}

#[cfg(not(unix))]
pub fn record_ends() {}

/// A line built in a signal handler: on the stack, cut at its room, never allocated.
#[cfg(unix)]
struct Line {
    bytes: [u8; 160],
    len: usize,
}

#[cfg(unix)]
impl Line {
    fn put(&mut self, bytes: &[u8]) {
        let room = &mut self.bytes[self.len..];
        let taken = bytes.len().min(room.len());
        room[..taken].copy_from_slice(&bytes[..taken]);
        self.len += taken;
    }

    fn number(&mut self, mut value: u64) {
        let mut digits = [0u8; 20];
        let mut at = digits.len();
        loop {
            at -= 1;
            digits[at] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        self.put(&digits[at..]);
    }

    fn hex(&mut self, value: u64) {
        let mut digits = [0u8; 16];
        let mut at = digits.len();
        let mut value = value;
        loop {
            at -= 1;
            digits[at] = b"0123456789abcdef"[(value & 0xf) as usize];
            value >>= 4;
            if value == 0 {
                break;
            }
        }
        self.put(&digits[at..]);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_line_says_its_numbers_and_is_cut_at_its_room() {
        let mut line = Line { bytes: [0; 160], len: 0 };
        line.put(PID_SAYS.as_bytes());
        line.number(4_294_967_295);
        line.put(ENDED_BY.as_bytes());
        line.number(0);
        assert_eq!(&line.bytes[..line.len], b"blaze serve: pid 4294967295 ended by signal 0");
        line.put(&[b'x'; 200]);
        assert_eq!(line.len, 160);
    }

    /// SIGUSR1 asks for one dump and ends nothing: its handler sets the flag, which the loop's
    /// next turn takes once.
    #[test]
    fn sigusr1_asks_for_one_dump_and_ends_nothing() {
        assert!(!ENDING.contains(&libc::SIGUSR1));
        catch_dump();
        // SAFETY: `raise` sends this thread SIGUSR1, whose handler `catch_dump` just installed
        // and which only stores to an atomic.
        assert_eq!(unsafe { libc::raise(libc::SIGUSR1) }, 0);
        assert!(dump_asked());
        assert!(!dump_asked(), "taken once");
        assert!(!signalled(), "a dump stops nothing");
    }

    /// A fault writes the line naming it, its address and its thread, and the process still
    /// ends by it, std's handler (there before, as in every std binary) having had it after.
    #[test]
    fn a_fault_writes_a_line_naming_it_then_ends_the_process_by_it() {
        // SAFETY: as for `old` in `record_ends`.
        let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: `old` is live for the call to fill.
        unsafe { libc::sigaction(libc::SIGSEGV, std::ptr::null(), &mut old) };
        assert_ne!(old.sa_sigaction, libc::SIG_DFL, "std handles SIGSEGV in a test binary");
        let mut fds = [0; 2];
        // SAFETY: `fds` is live and holds the two descriptors `pipe` writes.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: the child calls only async-signal-safe functions (`dup2`, `sigaction`, the
        // fault and `_exit`), as a fork of a process with other threads must.
        let child = unsafe { libc::fork() };
        if child == 0 {
            // SAFETY: both descriptors are open; the child's stderr becomes the pipe.
            unsafe { libc::dup2(fds[1], libc::STDERR_FILENO) };
            record_faults();
            // SAFETY: none: the read faults, on purpose, at an address no mapping has.
            let _fault = unsafe { std::ptr::read_volatile(std::ptr::without_provenance::<u8>(0x40)) };
            // SAFETY: `_exit` ends the child at once; reached only if the fault was not one.
            unsafe { libc::_exit(0) };
        }
        assert!(child > 0, "fork");
        // SAFETY: the parent's copy of the write end, closed so the read ends with the child.
        unsafe { libc::close(fds[1]) };
        let mut said = Vec::new();
        let mut chunk = [0u8; 256];
        loop {
            // SAFETY: `chunk` is live and its length is passed.
            let read = unsafe { libc::read(fds[0], chunk.as_mut_ptr().cast(), chunk.len()) };
            if read <= 0 {
                break;
            }
            said.extend_from_slice(&chunk[..read as usize]);
        }
        let mut status = 0;
        // SAFETY: `child` is this process's child and `status` is live.
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        let said = String::from_utf8_lossy(&said);
        assert!(libc::WIFSIGNALED(status), "the fault ended the child: {status:#x}, {said}");
        let by = libc::WTERMSIG(status);
        assert!(by == libc::SIGSEGV || by == libc::SIGBUS, "ended by the fault, not {by}: {said}");
        assert!(said.starts_with(&format!("{PID_SAYS}{child}{ENDED_BY}{by}")), "{said}");
        assert!(said.contains(" at 0x40 on thread ") && said.contains(" (tid ") && said.ends_with(")\n"), "{said}");
    }
}
