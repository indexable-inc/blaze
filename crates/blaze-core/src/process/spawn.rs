//! Starting a process from what the parent prepared in full, so nothing in the child allocates
//! or locks. On Linux a `clone` that shares the parent's memory and stops the calling thread
//! until the child runs its program (`CLONE_VM | CLONE_VFORK`, as `posix_spawn` does), so a
//! spawn costs the same however large the warm daemon is, where a `fork` copies its page
//! tables; the child joins its cgroup before its program runs, and the pidfd comes back with
//! the pid. On macOS `posix_spawn`, so no code of blaze's runs in the child. Elsewhere std's
//! spawn. What the Linux child may call, and why each is safe there, is on `run`.
//!
//! Why unsafe: nothing safe makes a `clone` whose child runs code, and std's `Command` forks
//! (or `posix_spawn`s with no cgroup join and no pidfd from the same call): a page-table copy
//! per spawn of a large daemon, or a second step that races the child.
//!
//! A spawn whose exec meets `ETXTBSY` is made again, from the parent, of the same program
//! ([`while_busy`], waiting as [`crate::busy`] says). Past its bound the spawn fails, naming the
//! error; never another path, never a copy of the file.

use std::{
    ffi::OsString,
    io,
    os::fd::OwnedFd,
    path::Path,
};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::{
    ffi::{CStr, OsStr},
    os::{
        fd::{AsRawFd as _, FromRawFd as _, RawFd},
        unix::{ffi::OsStrExt as _, fs::PermissionsExt as _},
    },
    ptr,
};

use crate::busy;

#[cfg(test)]
mod tests;

/// `start` made again for as long as its exec fails with `ETXTBSY`, waiting as [`busy::next`]
/// says. In the parent, never in a child: each try is a new child, whose descriptors are
/// copied from the parent's as they are then. A Linux child that retried its own `execve`
/// would keep the copy of the writer's descriptor it was made with, and never run. The
/// calling thread waits, as it waits for the spawn anyway: at most [`busy::MOST_NS`] in all,
/// and only while the file is busy.
fn while_busy<T>(mut start: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let (mut tried, mut waited) = (0, 0);
    loop {
        let error = match start() {
            Ok(it) => return Ok(it),
            Err(error) => error,
        };
        // Of every system call a spawn makes, only exec says `ETXTBSY`; by kind, since the
        // Linux spawn names the step that failed.
        if error.kind() != io::ErrorKind::ExecutableFileBusy {
            return Err(error);
        }
        let Some(wait) = busy::next(tried, waited) else {
            return Err(io::Error::new(
                error.kind(),
                // alloc: an error path.
                format!(
                    "still open for writing after {} ms of tries: {error}",
                    busy::MOST_NS / 1_000_000
                ),
            ));
        };
        std::thread::sleep(std::time::Duration::from_nanos(wait));
        tried += 1;
        waited += wait;
    }
}

/// What to start, borrowed: a command's own program, arguments, environment and directory, or
/// those its sandbox made, so a spawn copies none of them.
pub(super) struct Program<'a> {
    pub(super) program: &'a Path,
    pub(super) args: &'a [OsString],
    pub(super) env: &'a [(OsString, OsString)],
    pub(super) dir: &'a Path,
}

pub(super) struct Spawned {
    pub(super) pid: libc::pid_t,
    #[cfg(target_os = "linux")]
    pub(super) pidfd: OwnedFd,
    pub(super) stdin: Option<OwnedFd>,
    pub(super) stdout: OwnedFd,
    pub(super) stderr: OwnedFd,
}

/// `program` started with its output piped, its input piped where it has `input` (else
/// `/dev/null`), joining the cgroup whose `cgroup.procs` is `procs` before its program runs.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn spawn(program: &Program<'_>, input: bool, procs: Option<&CStr>) -> io::Result<Spawned> {
    let plan = Plan::new(program)?;
    let (stdin_child, stdin) = match input {
        true => {
            let (read, write) = pipe()?;
            (read, Some(write))
        }
        false => (open(c"/dev/null", libc::O_RDONLY)?, None),
    };
    let (stdout, stdout_child) = pipe()?;
    let (stderr, stderr_child) = pipe()?;
    let fds = [stdin_child.as_raw_fd(), stdout_child.as_raw_fd(), stderr_child.as_raw_fd()];
    // The child's ends close when this returns: by then the child has its own copies.
    #[cfg(target_os = "linux")]
    {
        let procs = procs.map(|path| open(path, libc::O_WRONLY)).transpose()?;
        let joins = procs.as_ref().map_or(-1, |it| it.as_raw_fd());
        let (pid, pidfd) = while_busy(|| clone(&plan, fds, joins))?;
        Ok(Spawned { pid, pidfd, stdin, stdout, stderr })
    }
    #[cfg(target_os = "macos")]
    {
        debug_assert!(procs.is_none(), "no cgroups on macOS");
        let pid = posix_spawn(&plan, fds)?;
        Ok(Spawned { pid, stdin, stdout, stderr })
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn spawn(
    program: &Program<'_>,
    input: bool,
    procs: Option<&std::ffi::CStr>,
) -> io::Result<Spawned> {
    use std::process::Stdio;
    debug_assert!(procs.is_none(), "no cgroups here");
    let mut command = std::process::Command::new(program.program);
    command
        .args(program.args)
        .env_clear()
        .envs(program.env.iter().map(|(name, value)| (name, value)))
        .current_dir(program.dir)
        .stdin(if input { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = while_busy(|| command.spawn())?;
    let pid = child.id() as libc::pid_t;
    let stdin = child.stdin.take().map(OwnedFd::from);
    let stdout = OwnedFd::from(child.stdout.take().expect("a piped stdout"));
    let stderr = OwnedFd::from(child.stderr.take().expect("a piped stderr"));
    // Dropping a `Child` neither waits for nor kills it: the driver reaps it.
    drop(child);
    Ok(Spawned { pid, stdin, stdout, stderr })
}

/// The program, its arguments, environment and directory as the system calls take them.
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct Plan {
    program: *const libc::c_char,
    argv: Vec<*const libc::c_char>,
    envp: Vec<*const libc::c_char>,
    dir: *const libc::c_char,
    /// What every pointer above points into: each string NUL-terminated, one after another,
    /// in the thread's byte buffer. A `Vec`'s bytes stay put when it moves, and nothing writes
    /// them once the plan is made.
    _bytes: blaze_mem::scratch::Lent<Vec<u8>>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Plan {
    /// As std's spawn makes them: `argv[0]` the program as named, the environment with a later
    /// value of a name replacing an earlier one, in order of name. Its strings go in the
    /// thread's byte buffer, so only the two arrays of pointers are made per spawn.
    fn new(program: &Program<'_>) -> io::Result<Plan> {
        let nul = |what: &str| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                // alloc: an error path.
                format!("{}: {what} has a NUL byte", program.program.display()),
            )
        };
        let mut bytes = blaze_mem::scratch::lend_bytes();
        let push = |bytes: &mut Vec<u8>, parts: &[&[u8]], what: &str| -> io::Result<usize> {
            if parts.iter().any(|it| it.contains(&0)) {
                return Err(nul(what));
            }
            let at = bytes.len();
            for part in parts {
                bytes.extend_from_slice(part);
            }
            bytes.push(0);
            Ok(at)
        };
        let resolved = bytes.len();
        resolve(program.program, program.env, program.dir, &mut bytes)?;
        if bytes[resolved..].contains(&0) {
            return Err(nul("its path"));
        }
        bytes.push(0);
        // Offsets into `bytes` until it is whole, then pointers.
        let offset = ptr::without_provenance::<libc::c_char>;
        // alloc: once per spawn, the arguments as the system takes them.
        let mut argv = Vec::with_capacity(program.args.len() + 2);
        argv.push(offset(push(&mut bytes, &[program.program.as_os_str().as_bytes()], "its name")?));
        for arg in program.args {
            argv.push(offset(push(&mut bytes, &[arg.as_bytes()], "an argument")?));
        }
        // alloc: once per spawn, the environment as the system takes it.
        let mut envp = Vec::with_capacity(program.env.len() + 1);
        let env = program.env;
        let name = |at: u32| env[at as usize].0.as_bytes();
        blaze_mem::scratch::with_u32s(|order| {
            order.extend(0..env.len() as u32);
            order.sort_unstable_by(|&a, &b| name(a).cmp(name(b)).then(a.cmp(&b)));
            let mut at = 0;
            while at < order.len() {
                let mut last = at;
                while last + 1 < order.len() && name(order[last + 1]) == name(order[at]) {
                    last += 1;
                }
                let (key, value) = &env[order[last] as usize];
                let pair = [key.as_bytes(), b"=", value.as_bytes()];
                envp.push(offset(push(&mut bytes, &pair, "its environment")?));
                at = last + 1;
            }
            Ok::<(), io::Error>(())
        })?;
        let dir = push(&mut bytes, &[program.dir.as_os_str().as_bytes()], "its directory")?;
        let base = bytes.as_ptr().cast::<libc::c_char>();
        for pointer in argv.iter_mut().chain(envp.iter_mut()) {
            *pointer = base.wrapping_add(pointer.addr());
        }
        argv.push(ptr::null());
        envp.push(ptr::null());
        Ok(Plan {
            program: base.wrapping_add(resolved),
            argv,
            envp,
            dir: base.wrapping_add(dir),
            _bytes: bytes,
        })
    }
}

/// Writes to `into` the file `program` names: itself where it has a `/`, else the first
/// executable file of that name on the command's `PATH`. Relative and empty entries are
/// relative to the command's directory, where the child will run, never the daemon's.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn resolve(
    program: &Path,
    env: &[(OsString, OsString)],
    dir: &Path,
    into: &mut Vec<u8>,
) -> io::Result<()> {
    let named = program.as_os_str().as_bytes();
    if named.contains(&b'/') {
        into.extend_from_slice(named);
        return Ok(());
    }
    let path = env.iter().rev().find(|(name, _)| name == "PATH").map(|(_, value)| value.as_os_str()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            // alloc: an error path.
            format!("{}: PATH is not set in the command's environment; set PATH or name the program by path", program.display()),
        )
    })?;
    // A relative working directory is relative to the parent once; exec runs after chdir.
    let absolute;
    let dir = if dir.is_absolute() {
        dir
    } else {
        // alloc: a spawn whose working directory is relative.
        absolute = std::env::current_dir()?.join(dir);
        &absolute
    };
    let found = blaze_mem::scratch::with_path(|candidate| {
        for entry in path.as_bytes().split(|&it| it == b':') {
            candidate.as_mut_os_string().clear();
            candidate.push(dir);
            candidate.push(OsStr::from_bytes(entry));
            candidate.push(program);
            let runs = std::fs::metadata(&*candidate)
                .is_ok_and(|it| it.is_file() && it.permissions().mode() & 0o111 != 0);
            if runs {
                into.extend_from_slice(candidate.as_os_str().as_bytes());
                return true;
            }
        }
        false
    });
    match found {
        true => Ok(()),
        false => Err(io::Error::new(
            io::ErrorKind::NotFound,
            // alloc: an error path.
            format!("{} is in no directory of its PATH", program.display()),
        )),
    }
}

/// A pipe, its read end first, both closed on exec and numbered past 2.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut ends = [0; 2];
    // SAFETY: two descriptors written by the call.
    #[cfg(target_os = "linux")]
    let made = unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC) };
    // SAFETY: as above; macOS has no `pipe2`, and its spawns close what they are not given.
    #[cfg(target_os = "macos")]
    let made = unsafe { libc::pipe(ends.as_mut_ptr()) };
    if made != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call made both; each is owned once from here.
    let read = unsafe { OwnedFd::from_raw_fd(ends[0]) };
    // SAFETY: as above.
    let write = unsafe { OwnedFd::from_raw_fd(ends[1]) };
    Ok((above_stdio(read)?, above_stdio(write)?))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open(path: &CStr, flags: libc::c_int) -> io::Result<OwnedFd> {
    // SAFETY: a NUL-terminated path.
    let fd = unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        let error = io::Error::last_os_error();
        // alloc: an error path.
        return Err(io::Error::new(error.kind(), format!("{}: {error}", path.to_string_lossy())));
    }
    // SAFETY: the call returned a descriptor of its own.
    above_stdio(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `fd` closed on exec and numbered past 2, so the child's `dup2`s onto 0, 1 and 2 never land
/// on a descriptor another of them still needs (a daemon started with its standard ones closed).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn above_stdio(fd: OwnedFd) -> io::Result<OwnedFd> {
    if fd.as_raw_fd() > 2 {
        #[cfg(target_os = "macos")]
        crate::net::set_cloexec(fd.as_raw_fd())?;
        return Ok(fd);
    }
    // SAFETY: an open descriptor; the copy is a descriptor of its own.
    let moved = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if moved < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    Ok(unsafe { OwnedFd::from_raw_fd(moved) })
}

/// The child's stack: what `run` and the calls it makes need, with room to spare.
#[cfg(target_os = "linux")]
const STACK: usize = 64 << 10;

/// What the child reads, and where it says what failed.
#[cfg(target_os = "linux")]
struct Child<'a> {
    plan: &'a Plan,
    fds: [RawFd; 3],
    /// `cgroup.procs` of its cgroup, or -1.
    procs: RawFd,
    /// The step that failed (0: none) and its errno.
    step: u8,
    failed: libc::c_int,
}

#[cfg(target_os = "linux")]
fn clone(plan: &Plan, fds: [RawFd; 3], procs: RawFd) -> io::Result<(libc::pid_t, OwnedFd)> {
    blaze_mem::scratch::with_bytes(|stack| {
        // alloc: once per thread that spawns, kept: the child's stack, its room unwritten.
        stack.reserve(STACK);
        // SAFETY: `reserve` made the capacity at least `STACK` bytes, so `STACK` bytes from
        // `as_mut_ptr()` are in the allocation, and the thread's byte buffer is lent to this
        // closure alone until it returns, after `clone_on` has.
        unsafe { clone_on(stack.as_mut_ptr(), plan, fds, procs) }
    })
}

/// As [`clone`], the child on the `STACK` bytes at `stack`.
///
/// # Safety
///
/// `stack` is the start of at least `STACK` writable bytes, which nothing else reads or writes
/// until this returns; the child's frames go there while the calling thread waits.
#[cfg(target_os = "linux")]
unsafe fn clone_on(stack: *mut u8, plan: &Plan, fds: [RawFd; 3], procs: RawFd) -> io::Result<(libc::pid_t, OwnedFd)> {
    // It grows down from its end, which both ABIs (x86-64, AArch64) want on 16 bytes.
    let top = stack.wrapping_add(STACK).map_addr(|it| it & !15);
    let mut child = Child { plan, fds, procs, step: 0, failed: 0 };
    let mut pidfd: libc::c_int = -1;
    // SAFETY: all zeros is a valid `sigset_t`; the calls below fill it.
    let mut all: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    let mut was: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: a `sigset_t` to fill.
    unsafe { libc::sigfillset(&mut all) };
    // Until the child has reset its handlers, a signal would run one of the daemon's on the
    // daemon's memory from the child.
    // SAFETY: two `sigset_t`s, one to read and one to write.
    unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &all, &mut was) };
    // SAFETY: `run` takes the `Child` it is given and touches nothing else of the parent's.
    // With CLONE_VM the child runs on `stack` in this process's memory (the caller's `STACK`
    // bytes, `top` their 16-byte-aligned end), and with CLONE_VFORK this thread waits until the
    // child has run its program or exited, so `child`, `plan` and `stack` outlive every use of
    // them in the child. CLONE_PIDFD writes the pidfd to `pidfd`. Every signal is blocked
    // across it (above), so no handler of the daemon's runs in the child on the shared memory.
    let pid = unsafe {
        libc::clone(
            run,
            top.cast(),
            libc::CLONE_VM | libc::CLONE_VFORK | libc::CLONE_PIDFD | libc::SIGCHLD,
            (&raw mut child).cast(),
            &raw mut pidfd,
        )
    };
    let error = io::Error::last_os_error();
    // SAFETY: the mask read above.
    unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &was, ptr::null_mut()) };
    if pid < 0 {
        return Err(error);
    }
    // SAFETY: the kernel made it for this call.
    let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd) };
    // SAFETY: written by the child, which has run its program or exited; volatile because the
    // compiler cannot see a write made through the pointer given to `clone`.
    let step = unsafe { ptr::read_volatile(&raw const child.step) };
    // SAFETY: as above.
    let failed = unsafe { ptr::read_volatile(&raw const child.failed) };
    if step == 0 {
        return Ok((pid, pidfd));
    }
    _ = super::reap(pid);
    let what = match step {
        1 => "giving it its input and output",
        2 => "putting it in its cgroup",
        3 => "changing to its directory",
        _ => "running its program",
    };
    let error = io::Error::from_raw_os_error(failed);
    // alloc: an error path.
    Err(io::Error::new(error.kind(), format!("{what}: {error}")))
}

/// The child, between `clone` and its program: system calls alone, nothing that allocates,
/// locks or could panic. It writes only `Child`, through which the parent hears what failed.
///
/// It shares the parent's memory (CLONE_VM) and its thread's TLS (no CLONE_SETTLS), while
/// another thread of the daemon may hold any lock (the allocator's, stdio's, the C library's).
/// So every call it makes is async-signal-safe (POSIX.1-2017 section 2.4.3, the functions
/// allowed after `fork` in a multithreaded process): `sigaction`, `sigemptyset`,
/// `sigprocmask`, `dup2`, `write`, `chdir`, `execve`, `_exit`; and `__errno_location`, which
/// only returns this thread's TLS address. Each is a thin wrapper of one system call in glibc
/// and musl, taking no lock. The rest is plain loads and stores into `Child`, the stack
/// (`mem::zeroed` structs, stores or a `memset`, async-signal-safe since POSIX.1-2016), and
/// the parent's `Plan`, which nothing writes while it waits.
///
/// Its stack is the `STACK` bytes `clone` lends, with no guard page below them: its deepest
/// frame is this one (two `sigaction`s and a `sigset_t`, under 400 bytes) plus one libc
/// wrapper's, far under `STACK`, and nothing in it recurses or grows the stack by a length
/// it is given. Past `STACK` it would write the heap below the buffer.
#[cfg(target_os = "linux")]
extern "C" fn run(arg: *mut libc::c_void) -> libc::c_int {
    // SAFETY: the `Child` `clone` was given; the thread that made it waits (CLONE_VFORK) and
    // nothing else has it.
    let child = unsafe { &mut *arg.cast::<Child<'_>>() };
    let fail = |child: &mut Child<'_>, step: u8| -> ! {
        // SAFETY: the calling thread's errno, in the TLS the child shares with the waiting
        // parent thread; async-signal-safe, an address and no lock.
        let errno = unsafe { libc::__errno_location() };
        // SAFETY: as above; nothing else writes it until the parent runs again.
        child.failed = unsafe { *errno };
        child.step = step;
        // SAFETY: the end of the child, running nothing of the parent's (no atexit handler, no
        // stdio flush); async-signal-safe.
        unsafe { libc::_exit(127) }
    };
    for signal in 1..65 {
        // SAFETY: all zeros is a valid `sigaction`; the call writes it.
        let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: a `sigaction` on the stack to write; a signal the C library keeps for itself
        // fails. Async-signal-safe.
        if unsafe { libc::sigaction(signal, ptr::null(), &mut old) } != 0 {
            continue;
        }
        // Ignored stays ignored across exec, but SIGPIPE, which std ignores in the daemon and
        // gives back to its children.
        if old.sa_sigaction == libc::SIG_IGN && signal != libc::SIGPIPE {
            continue;
        }
        // SAFETY: as above.
        let mut default: libc::sigaction = unsafe { std::mem::zeroed() };
        default.sa_sigaction = libc::SIG_DFL;
        // SAFETY: a `sigaction` on the stack to read. Async-signal-safe. Signals stay blocked
        // (the parent's mask, inherited) until every handler of the daemon's is reset.
        unsafe { libc::sigaction(signal, &default, ptr::null_mut()) };
    }
    // SAFETY: all zeros is a valid `sigset_t`.
    let mut none: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: a `sigset_t` on the stack to empty. Async-signal-safe.
    unsafe { libc::sigemptyset(&mut none) };
    // SAFETY: a `sigset_t` to read; every handler is the default now, so an unblocked signal
    // runs nothing of the daemon's. Async-signal-safe.
    unsafe { libc::sigprocmask(libc::SIG_SETMASK, &none, ptr::null_mut()) };
    for (target, fd) in (0..).zip(child.fds) {
        // SAFETY: an open descriptor past 2 onto one of 0, 1 and 2; the copy keeps no
        // close-on-exec. Async-signal-safe.
        if unsafe { libc::dup2(fd, target) } < 0 {
            fail(child, 1);
        }
    }
    // SAFETY: one byte from a static, to an open descriptor. Async-signal-safe.
    if child.procs >= 0 && unsafe { libc::write(child.procs, b"0".as_ptr().cast(), 1) } != 1 {
        fail(child, 2);
    }
    // SAFETY: a NUL-terminated path the parent keeps. Async-signal-safe.
    if unsafe { libc::chdir(child.plan.dir) } != 0 {
        fail(child, 3);
    }
    // SAFETY: a NUL-terminated path and two NULL-terminated arrays of NUL-terminated strings,
    // all kept by the parent until the program runs. Async-signal-safe; on success the child
    // leaves the shared memory and the parent runs again.
    unsafe {
        libc::execve(child.plan.program, child.plan.argv.as_ptr(), child.plan.envp.as_ptr())
    };
    fail(child, 4)
}

#[cfg(target_os = "macos")]
fn posix_spawn(plan: &Plan, fds: [RawFd; 3]) -> io::Result<libc::pid_t> {
    unsafe extern "C" {
        fn posix_spawn_file_actions_addchdir_np(
            actions: *mut libc::posix_spawn_file_actions_t,
            path: *const libc::c_char,
        ) -> libc::c_int;
    }
    /// Every descriptor not given by a file action is closed in the child (Apple's).
    const CLOEXEC_DEFAULT: libc::c_short = 0x4000;
    let check = |code: libc::c_int| match code {
        0 => Ok(()),
        _ => Err(io::Error::from_raw_os_error(code)),
    };
    let mut actions: libc::posix_spawn_file_actions_t = ptr::null_mut();
    // SAFETY: a `posix_spawn_file_actions_t` to make.
    check(unsafe { libc::posix_spawn_file_actions_init(&mut actions) })?;
    let mut attr: libc::posix_spawnattr_t = ptr::null_mut();
    // SAFETY: a `posix_spawnattr_t` to make.
    if let Err(error) = check(unsafe { libc::posix_spawnattr_init(&mut attr) }) {
        // SAFETY: made above.
        unsafe { libc::posix_spawn_file_actions_destroy(&mut actions) };
        return Err(error);
    }
    let spawned = (|| {
        for (target, fd) in (0..).zip(fds) {
            // SAFETY: made above; an open descriptor onto one of 0, 1 and 2.
            check(unsafe { libc::posix_spawn_file_actions_adddup2(&mut actions, fd, target) })?;
        }
        // SAFETY: made above; a NUL-terminated path kept until the spawn.
        check(unsafe { posix_spawn_file_actions_addchdir_np(&mut actions, plan.dir) })?;
        // SIGPIPE, which std ignores in the daemon, back to its default in the child, and the
        // child's mask empty, as std's spawn makes them. On macOS `sigset_t` is an integer.
        let mut defaults: libc::sigset_t = 0;
        // SAFETY: a `sigset_t` to fill.
        unsafe { libc::sigaddset(&mut defaults, libc::SIGPIPE) };
        // SAFETY: made above; a `sigset_t` to read.
        check(unsafe { libc::posix_spawnattr_setsigdefault(&mut attr, &defaults) })?;
        let none: libc::sigset_t = 0;
        // SAFETY: as above.
        check(unsafe { libc::posix_spawnattr_setsigmask(&mut attr, &none) })?;
        let flags = CLOEXEC_DEFAULT
            | libc::POSIX_SPAWN_SETSIGDEF as libc::c_short
            | libc::POSIX_SPAWN_SETSIGMASK as libc::c_short;
        // SAFETY: made above.
        check(unsafe { libc::posix_spawnattr_setflags(&mut attr, flags) })?;
        let mut pid = 0;
        while_busy(|| {
            // SAFETY: a NUL-terminated path, the actions and attributes made above, and two
            // NULL-terminated arrays of NUL-terminated strings kept by `plan`.
            check(unsafe {
                libc::posix_spawn(
                    &mut pid,
                    plan.program,
                    &actions,
                    &attr,
                    plan.argv.as_ptr().cast(),
                    plan.envp.as_ptr().cast(),
                )
            })
        })?;
        Ok(pid)
    })();
    // SAFETY: made above, destroyed once.
    unsafe { libc::posix_spawnattr_destroy(&mut attr) };
    // SAFETY: as above.
    unsafe { libc::posix_spawn_file_actions_destroy(&mut actions) };
    spawned
}
