//! Processes, their exits and their signals: what a spawn, a reap and the sandbox's own
//! setup use.

use core::ffi::CStr;

use super::{
    fs::{OwnedFd, arg, owned},
    nr, raw,
    wait::SigSet,
};
use crate::{Errno, Fd};

/// `pidfd_open`: a descriptor of the process `pid` that polls readable once it has exited.
/// Used by the direct driver's wait for a child's exit.
/// replaces libc::syscall(libc::SYS_pidfd_open) (flags 0; as blaze_sys::linux::pidfd_open(pid); returns Result<OwnedFd, Errno>, the site's -1 check becomes the Err)
/// replaces libc::SYS_pidfd_open
#[blaze::noalloc]
#[blaze::nopanic]
pub fn pidfd_open(pid: i32) -> Result<OwnedFd, Errno> {
    // SAFETY: the call takes a pid and flags and no memory.
    owned(unsafe { raw::syscall2(nr::PIDFD_OPEN, pid as usize, 0) })
}

/// `pidfd_send_signal(pidfd, signal, NULL, 0)` (5.1): `signal` (0 asks only whether the
/// process is there) to the process `pidfd` is of. Exact even after the process is reaped: a
/// pidfd never names another process that took its pid. [`Errno::Srch`] where it has ended;
/// [`Errno::NoSys`] on a kernel before 5.1 (the slot `linux::slot::Signal` chooses the form
/// once from `KernelFacts` where a caller must run there too).
/// replaces libc::syscall(libc::SYS_pidfd_send_signal) (with a null siginfo and flags 0; as blaze_sys::linux::pidfd_send_signal(pidfd: Fd, signal: u32); returns Result<(), Errno>, the site's nonzero check becomes the Err)
/// replaces libc::SYS_pidfd_send_signal (the call; the number as data stays nr::PIDFD_SEND_SIGNAL)
#[blaze::noalloc]
#[blaze::nopanic]
pub fn pidfd_send_signal(pidfd: Fd, signal: u32) -> Result<(), Errno> {
    // SAFETY: a null `siginfo` and no flags: the kernel reads no memory.
    raw::check(unsafe { raw::syscall4(nr::PIDFD_SEND_SIGNAL, arg(pidfd), signal as usize, 0, 0) })
        .map(|_| ())
}

/// Which children a [`waitid`] looks at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Children {
    /// `P_ALL`: any child.
    Any,
    /// `P_PID`: the child with this pid.
    Pid(i32),
    /// `P_PIDFD`: the child this pidfd is of.
    Pidfd(Fd),
}

flag_set! {
    /// `waitid`'s options.
    WaitOptions: u32 {
        /// `WNOHANG`: look, and wait for nothing.
        NO_HANG = 0x1;
        /// `WEXITED`: children that have exited.
        EXITED = 0x4;
        /// `WNOWAIT`: leave the child to be waited for again.
        NO_WAIT = 0x0100_0000;
    }
}

/// How a child ended (`si_code` of a `SIGCHLD`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ended {
    /// `CLD_EXITED`, with its exit status.
    Exited(i32),
    /// `CLD_KILLED` or `CLD_DUMPED`, with the signal.
    Signaled(i32),
    /// Another `si_code`, with it and `si_status`.
    Other(i32, i32),
}

/// A child a [`waitid`] found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Waited {
    /// The child's pid.
    pub pid: i32,
    /// How it ended.
    pub ended: Ended,
}

/// The kernel's `siginfo_t`: 128 bytes. For a `SIGCHLD` on a 64-bit target: `si_signo` at 0,
/// `si_errno` at 4, `si_code` at 8, `si_pid` at 16, `si_uid` at 20, `si_status` at 24.
#[repr(C, align(8))]
struct SigInfo([i32; 32]);

/// One `waitid` of `children` with `options`: the child it found, or `None` where
/// [`WaitOptions::NO_HANG`] found none ready. [`Errno::Child`] where there is no such child;
/// [`Errno::Intr`]. Without [`WaitOptions::NO_WAIT`] the child is reaped. The direct
/// driver's look at a child before and after its pidfd is polled.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn waitid(children: Children, options: WaitOptions) -> Result<Option<Waited>, Errno> {
    let (which, id) = match children {
        Children::Any => (0, 0),
        Children::Pid(pid) => (1, pid as usize),
        Children::Pidfd(fd) => (3, arg(fd)),
    };
    // Zeroed, so that a child still running, of which the kernel writes nothing it need not,
    // reads as pid 0.
    let mut info = SigInfo([0; 32]);
    // SAFETY: `info` is 128 live bytes (integers only, any bytes valid) the kernel writes a
    // `siginfo_t` into; no `rusage` is asked for (null); it keeps no pointer.
    raw::check(unsafe {
        raw::syscall5(
            nr::WAITID,
            which,
            id,
            core::ptr::from_mut(&mut info) as usize,
            options.bits() as usize,
            0,
        )
    })?;
    let SigInfo(words) = info;
    let [_signo, _errno, code, _pad, pid, _uid, status, ..] = words;
    if pid == 0 {
        return Ok(None);
    }
    let ended = match code {
        1 => Ended::Exited(status),
        2 | 3 => Ended::Signaled(status),
        other => Ended::Other(other, status),
    };
    Ok(Some(Waited { pid, ended }))
}

/// `struct rusage`: two `timeval`s and fourteen `long`s, 144 bytes on a 64-bit target.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Rusage {
    /// User time: seconds and microseconds.
    pub user: [i64; 2],
    /// System time: seconds and microseconds.
    pub system: [i64; 2],
    /// The largest resident set, in kilobytes.
    pub max_rss_kb: i64,
    /// `ru_ixrss` to `ru_nivcsw`, in the kernel's order.
    pub rest: [i64; 13],
}

/// One `wait4(pid, WNOHANG)`: the child's wait status and what it used, once it has exited
/// (and it is reaped); `None` while it runs. The direct driver's `reap_now`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn wait4_now(pid: i32) -> Result<Option<(i32, Rusage)>, Errno> {
    /// `WNOHANG`.
    const NO_HANG: usize = 1;
    let mut status = 0i32;
    let mut rusage = Rusage { user: [0; 2], system: [0; 2], max_rss_kb: 0, rest: [0; 13] };
    // SAFETY: `status` (an `int`) and `rusage` (144 bytes of integers, any bytes valid) are
    // live locals the kernel writes and does not keep.
    let reaped = raw::check(unsafe {
        raw::syscall4(
            nr::WAIT4,
            pid as isize as usize,
            core::ptr::from_mut(&mut status) as usize,
            NO_HANG,
            core::ptr::from_mut(&mut rusage) as usize,
        )
    })?;
    if reaped == 0 {
        return Ok(None);
    }
    Ok(Some((status, rusage)))
}

flag_set! {
    /// `clone3`'s flags. A child that shares the caller's memory (`CLONE_VM`, a thread) is
    /// not among them: [`clone3_process`] refuses it.
    /// replaces libc::CLONE_CLEAR_SIGHAND (blaze_sys::linux::CloneFlags::CLEAR_SIGHAND, in CloneArgs for clone3_process)
    /// replaces libc::CLONE_INTO_CGROUP (blaze_sys::linux::CloneFlags::INTO_CGROUP, with CloneArgs::cgroup)
    CloneFlags: u64 {
        /// `CLONE_PIDFD`: a pidfd of the child is written to [`CloneArgs::pidfd`].
        PIDFD = 0x1000;
        /// `CLONE_CLEAR_SIGHAND`: the child's signal handlers are the defaults.
        CLEAR_SIGHAND = 0x1_0000_0000;
        /// `CLONE_INTO_CGROUP`: the child starts in [`CloneArgs::cgroup`].
        INTO_CGROUP = 0x2_0000_0000;
    }
}

/// `CLONE_VM`.
const CLONE_VM: u64 = 0x100;

/// `struct clone_args`, to its `cgroup` field (88 bytes, Linux 5.7).
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct CloneArgs {
    /// The flags' bits ([`CloneFlags::bits`]).
    pub flags: u64,
    /// Where the kernel writes the child's pidfd (an `int`), with [`CloneFlags::PIDFD`].
    pub pidfd: u64,
    /// Where it writes the child's tid in the child.
    pub child_tid: u64,
    /// Where it writes the child's tid in the parent.
    pub parent_tid: u64,
    /// The signal the parent gets when the child exits (`SIGCHLD` is 17).
    pub exit_signal: u64,
    /// The child's stack: 0 for a copy of the caller's.
    pub stack: u64,
    /// The stack's size.
    pub stack_size: u64,
    /// The child's thread pointer.
    pub tls: u64,
    /// Pids to give the child, innermost namespace first.
    pub set_tid: u64,
    /// How many.
    pub set_tid_size: u64,
    /// A cgroup directory's descriptor, with [`CloneFlags::INTO_CGROUP`].
    pub cgroup: u64,
}

/// Which side of a [`clone3_process`] this is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cloned {
    /// The new process.
    Child,
    /// The caller, with the child's pid.
    Parent(i32),
}

/// `clone3`: a child process with a copy of the caller's memory (a fork). A request for a
/// child that shares it (`CLONE_VM`) is [`Errno::Inval`] before any call: a thread's entry
/// needs its own stack switch, which is `bstd::rt`'s (sys.md, "Runtime"). Used by a spawn
/// that must run code between the fork and the exec (the sandbox's setup).
///
/// # Safety
///
/// The child of a process with other threads holds only the calling thread: until it execs
/// or exits it may call only what is async-signal-safe (the functions of this crate are; an
/// allocator's or a lock's are not). Every pointer field of `args` is 0 or valid for what its
/// flag has the kernel write.
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn clone3_process(args: &CloneArgs) -> Result<Cloned, Errno> {
    if args.flags & CLONE_VM != 0 || args.stack != 0 {
        return Err(Errno::Inval);
    }
    // SAFETY: `args` is a live `clone_args` of the size passed, which the kernel reads; with
    // no `CLONE_VM` and no new stack the child returns here on a copy of this stack.
    let pid = raw::check(unsafe {
        raw::syscall2(nr::CLONE3, core::ptr::from_ref(args) as usize, size_of::<CloneArgs>())
    })?;
    Ok(match pid {
        0 => Cloned::Child,
        pid => Cloned::Parent(pid as i32),
    })
}

/// `exit`: the calling thread ends with `status`; the process goes on while it has others.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn exit_thread(status: i32) -> ! {
    loop {
        // SAFETY: the call takes a number and never returns; the loop is what the type
        // checker needs, not a wait.
        _ = unsafe { raw::syscall1(nr::EXIT, status as usize) };
    }
}

/// `exit_group`: the whole process ends with `status`, no destructor run. `Host::exit`, and
/// the end of the panic handler's abort.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn exit_group(status: i32) -> ! {
    loop {
        // SAFETY: as `exit_thread`.
        _ = unsafe { raw::syscall1(nr::EXIT_GROUP, status as usize) };
    }
}

/// `getpid`: the process's id. `Host`'s pid.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn getpid() -> i32 {
    // SAFETY: the call takes nothing and cannot fail.
    (unsafe { raw::syscall0(nr::GETPID) }) as i32
}

/// `kill`: `signal` sent to the process `pid`. `Effect::Kill`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn kill(pid: i32, signal: u32) -> Result<(), Errno> {
    // SAFETY: the call takes numbers and no memory.
    raw::check(unsafe { raw::syscall2(nr::KILL, pid as isize as usize, signal as usize) }).map(|_| ())
}

/// `kill(-group)`: `signal` sent to every process of the process group `group`: a child
/// started as the leader of its own group (`setpgid(0, 0)` in the child) and all it left
/// running. [`Errno::Inval`] for a group of 1 or less, refused before the call: `kill(0)` is
/// the caller's own group and `kill(-1)` every process it may signal, neither of which a group
/// id names. Used by the sandbox's end of a native build script, and the test runner's end of a
/// test binary.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn kill_group(group: i32, signal: u32) -> Result<(), Errno> {
    if group <= 1 {
        return Err(Errno::Inval);
    }
    kill(group.wrapping_neg(), signal)
}

/// `sched_yield`: the CPU given to another runnable thread, if one is waiting; back at once
/// otherwise. A spin's back-off before it parks (blaze-core's locks, blaze-spin). Linux's
/// call returns 0; a filter that refuses it is the error.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn sched_yield() -> Result<(), Errno> {
    // SAFETY: the call takes nothing.
    raw::check(unsafe { raw::syscall0(nr::SCHED_YIELD) }).map(|_| ())
}

/// One argument or environment string of an [`execveat`], lent: a pointer to a NUL-terminated
/// string, or the [`Arg::END`] that closes a list. One word, the layout of a `char *`.
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct Arg<'a>(*const u8, core::marker::PhantomData<&'a CStr>);

impl<'a> Arg<'a> {
    /// The null that ends a list.
    pub const END: Arg<'static> = Arg(core::ptr::null(), core::marker::PhantomData);

    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn new(text: &'a CStr) -> Arg<'a> {
        Arg(text.as_ptr().cast(), core::marker::PhantomData)
    }

    #[blaze::noalloc]
    #[blaze::nopanic]
    const fn is_end(self) -> bool {
        self.0.is_null()
    }
}

/// A list of [`Arg`]s whose last, and only last, is [`Arg::END`]: what `execve`'s `argv` and
/// `envp` are. Checked as it is made, so a call never reads past a list.
#[derive(Clone, Copy, Debug)]
pub struct Args<'a>(&'a [Arg<'a>]);

impl<'a> Args<'a> {
    /// `list`, or [`Errno::Inval`] where it is empty, does not end in [`Arg::END`] or holds
    /// one before its end.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn new(list: &'a [Arg<'a>]) -> Result<Args<'a>, Errno> {
        let Some((last, before)) = list.split_last() else {
            return Err(Errno::Inval);
        };
        if !last.is_end() || before.iter().any(|arg| arg.is_end()) {
            return Err(Errno::Inval);
        }
        Ok(Args(list))
    }
}

/// `execveat`: this process replaced by the program `path` (in `dir`; with
/// [`AtFlags::EMPTY_PATH`](super::AtFlags::EMPTY_PATH) and [`crate::Path::EMPTY`], the open
/// file `dir` itself, so a sandbox runs the very file it checked), given `argv` and `envp` and
/// nothing of the caller's environment. It returns only when refused, with why. Takes
/// [`AtFlags::NO_FOLLOW`](super::AtFlags::NO_FOLLOW) and `EMPTY_PATH`. Called in a child
/// between `clone3` and the program, where only system calls run.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn execveat(dir: Fd, path: crate::Path<'_>, argv: Args<'_>, envp: Args<'_>, flags: super::AtFlags) -> Errno {
    // SAFETY: `path` is a live NUL-terminated string; `argv` and `envp` are live arrays of
    // pointers to live NUL-terminated strings, each ending in a null (`Args`' invariant); the
    // kernel reads them, and on success this process's memory is gone with them.
    let result = unsafe {
        raw::syscall5(
            nr::EXECVEAT,
            arg(dir),
            path.as_ptr() as usize,
            argv.0.as_ptr() as usize,
            envp.0.as_ptr() as usize,
            flags.bits() as usize,
        )
    };
    match raw::check(result) {
        Err(errno) => errno,
        // `execveat` does not return on success: a value here is no errno the kernel could give.
        Ok(_) => Errno::Inval,
    }
}

/// One typed `prctl`.
/// replaces libc::PR_SET_PDEATHSIG (with prctl: blaze_sys::linux::prctl(Prctl::ParentDeathSignal(signal)))
/// replaces libc::PR_SET_NO_NEW_PRIVS (with prctl: blaze_sys::linux::prctl(Prctl::NoNewPrivs))
#[derive(Clone, Copy, Debug)]
pub enum Prctl<'a> {
    /// `PR_SET_PDEATHSIG`: the signal this process gets when its parent's thread ends.
    ParentDeathSignal(u32),
    /// `PR_SET_DUMPABLE`.
    Dumpable(bool),
    /// `PR_SET_NAME`: the calling thread's name, cut to 15 bytes by the kernel.
    Name(&'a CStr),
    /// `PR_SET_NO_NEW_PRIVS`: no exec of this process gains privileges; what an unprivileged
    /// seccomp filter needs first.
    NoNewPrivs,
    /// `PR_SET_THP_DISABLE`: no transparent huge pages for this process (and what it forks).
    /// replaces libc::PR_SET_THP_DISABLE
    ThpDisable(bool),
    /// `PR_SET_THP_DISABLE` with `PR_THP_DISABLE_EXCEPT_ADVISED` (6.18): transparent huge
    /// pages only where `madvise(MADV_HUGEPAGE)` asked for them. Older kernels refuse it
    /// ([`Errno::Inval`]).
    /// replaces libc::prctl(libc::PR_SET_THP_DISABLE, 1, PR_THP_DISABLE_EXCEPT_ADVISED, 0, 0) (as blaze_sys::linux::prctl(Prctl::ThpDisableExceptAdvised); blaze-mem os.rs's private PR_THP_DISABLE_EXCEPT_ADVISED)
    ThpDisableExceptAdvised,
}

/// `prctl`, one typed request. Used by a spawned child (its parent-death signal), a thread's
/// name, and the sandbox before its filter.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn prctl(request: Prctl<'_>) -> Result<(), Errno> {
    /// `PR_THP_DISABLE_EXCEPT_ADVISED` (linux/prctl.h): `PR_SET_THP_DISABLE`'s third argument.
    const THP_DISABLE_EXCEPT_ADVISED: usize = 1 << 1;
    let (option, argument, third) = match request {
        Prctl::ParentDeathSignal(signal) => (1, signal as usize, 0),
        Prctl::Dumpable(dumpable) => (4, dumpable as usize, 0),
        Prctl::Name(name) => (15, name.as_ptr() as usize, 0),
        Prctl::NoNewPrivs => (38, 1, 0),
        Prctl::ThpDisable(off) => (41, off as usize, 0),
        Prctl::ThpDisableExceptAdvised => (41, 1, THP_DISABLE_EXCEPT_ADVISED),
    };
    // SAFETY: of these requests only `PR_SET_NAME` reads memory: a live NUL-terminated
    // string, not kept. The unused arguments are 0, as `PR_SET_NO_NEW_PRIVS` requires.
    raw::check(unsafe { raw::syscall5(nr::PRCTL, option, argument, third, 0, 0) }).map(|_| ())
}

/// `prctl(PR_GET_NAME)`: the calling thread's name, NUL-terminated in the kernel's 16 bytes.
/// replaces libc::PR_GET_NAME (blaze_sys::linux::thread_name())
#[blaze::noalloc]
#[blaze::nopanic]
pub fn thread_name() -> Result<[u8; 16], Errno> {
    let mut name = [0u8; 16];
    // SAFETY: `PR_GET_NAME` writes at most 16 bytes (TASK_COMM_LEN) into `name`, a live local,
    // and keeps no pointer.
    raw::check(unsafe { raw::syscall5(nr::PRCTL, 16, name.as_mut_ptr() as usize, 0, 0, 0) })?;
    Ok(name)
}

/// `struct sock_filter`: one classic BPF instruction.
/// replaces libc::sock_filter (blaze_sys::linux::SockFilter { code, jt, jf, k }; the opcodes are blaze_sys::posix::net::BPF_*)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct SockFilter {
    /// The opcode.
    pub code: u16,
    /// Jump if true.
    pub jt: u8,
    /// Jump if false.
    pub jf: u8,
    /// The operand.
    pub k: u32,
}

/// `struct sock_fprog`.
#[repr(C)]
struct SockFprog {
    len: u16,
    filter: *const SockFilter,
}

flag_set! {
    /// `seccomp(SECCOMP_SET_MODE_FILTER)`'s flags.
    SeccompFlags: u32 {
        /// `SECCOMP_FILTER_FLAG_TSYNC`: every thread of the process gets the filter.
        ALL_THREADS = 1;
    }
}

/// `seccomp(SECCOMP_SET_MODE_FILTER)`: `filter` added to the calling thread's system call
/// filters, for good. [`Errno::Inval`] for an empty filter or one over 4096 instructions
/// (`BPF_MAXINSNS`) before any call; [`Errno::Acces`] without [`Prctl::NoNewPrivs`] or
/// `CAP_SYS_ADMIN`. The sandbox's filter (`blaze-cargo/src/sandbox/linux.rs`).
/// replaces libc::syscall(libc::SYS_seccomp) (SECCOMP_SET_MODE_FILTER; as blaze_sys::linux::seccomp_filter(&[SockFilter], SeccompFlags); the sock_fprog is built for you; returns Result<(), Errno>; set no_new_privs first by blaze_sys::linux::prctl)
/// replaces libc::SYS_seccomp
/// replaces libc::sock_fprog (none to build: blaze_sys::linux::seccomp_filter takes &[SockFilter] and makes it)
/// replaces libc::SECCOMP_SET_MODE_FILTER (blaze_sys::linux::seccomp_filter is that operation)
/// replaces libc::PR_SET_SECCOMP (with prctl and SECCOMP_MODE_FILTER: blaze_sys::linux::seccomp_filter(&filter, SeccompFlags::NONE), the same filter installed by the seccomp call)
/// replaces libc::SECCOMP_MODE_FILTER (with prctl(PR_SET_SECCOMP): blaze_sys::linux::seccomp_filter)
#[blaze::noalloc]
#[blaze::nopanic]
pub fn seccomp_filter(filter: &[SockFilter], flags: SeccompFlags) -> Result<(), Errno> {
    /// `SECCOMP_SET_MODE_FILTER`.
    const SET_MODE_FILTER: usize = 1;
    let len = match u16::try_from(filter.len()) {
        Ok(len @ 1..=4096) => len,
        _ => return Err(Errno::Inval),
    };
    let program = SockFprog { len, filter: filter.as_ptr() };
    // SAFETY: `program` is a live `sock_fprog` naming `len` live instructions; the kernel
    // copies them and keeps no pointer.
    raw::check(unsafe {
        raw::syscall3(nr::SECCOMP, SET_MODE_FILTER, flags.bits() as usize, core::ptr::from_ref(&program) as usize)
    })
    .map(|_| ())
}

/// How `rt_sigprocmask` changes the thread's mask.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MaskHow {
    /// `SIG_BLOCK`: the set's signals are added.
    Block,
    /// `SIG_UNBLOCK`: the set's signals are removed.
    Unblock,
    /// `SIG_SETMASK`: the set is the mask.
    Set,
}

/// `rt_sigprocmask`: the calling thread's signal mask changed by `set` as `how` says; the
/// mask before. Used by a worker thread (which blocks the asynchronous signals) and a
/// spawned child (which unblocks them before its exec).
#[blaze::noalloc]
#[blaze::nopanic]
pub fn rt_sigprocmask(how: MaskHow, set: SigSet) -> Result<SigSet, Errno> {
    let how = match how {
        MaskHow::Block => 0,
        MaskHow::Unblock => 1,
        MaskHow::Set => 2,
    };
    let mut before = SigSet::EMPTY;
    // SAFETY: `set` and `before` are live sets of the kernel's own size (8 bytes), read and
    // written and not kept.
    raw::check(unsafe {
        raw::syscall4(
            nr::RT_SIGPROCMASK,
            how,
            core::ptr::from_ref(&set) as usize,
            core::ptr::from_mut(&mut before) as usize,
            size_of::<SigSet>(),
        )
    })?;
    Ok(before)
}

/// What a signal does when it arrives, of the two that need no handler.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Disposition {
    /// `SIG_DFL`: the signal's default action.
    Default,
    /// `SIG_IGN`: nothing.
    Ignore,
}

/// `rt_sigaction`: `signal`'s action set to `disposition`, with no handler (so no restorer).
/// A handler is [`super::signal::set_action`]'s, with blaze-sys's own restorer. Used by a
/// spawned child (its dispositions reset before the exec) and the process's `SIGPIPE`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn rt_sigaction(signal: u32, disposition: Disposition) -> Result<(), Errno> {
    let action = super::signal::SigAction {
        sa_sigaction: match disposition {
            Disposition::Default => 0,
            Disposition::Ignore => 1,
        },
        ..super::signal::SigAction::DEFAULT
    };
    // SAFETY: `action` is a live kernel `sigaction` the kernel reads; no old action is asked
    // for (null); the set's size is the kernel's own. With no handler, no restorer is needed.
    raw::check(unsafe {
        raw::syscall4(nr::RT_SIGACTION, signal as usize, core::ptr::from_ref(&action) as usize, 0, size_of::<SigSet>())
    })
    .map(|_| ())
}

#[cfg(test)]
mod pidfd_signal_tests {
    use super::*;

    /// Signal 0 through a pidfd of this process asks only whether it is there: it is; a
    /// descriptor that is no pidfd is refused.
    #[test]
    fn a_pidfd_is_signalled() {
        let me = match pidfd_open(getpid()) {
            Ok(me) => me,
            Err(Errno::NoSys) => {
                eprintln!("skipped: this kernel has no pidfd_open (5.3)");
                return;
            }
            Err(other) => panic!("pidfd_open: {other}"),
        };
        assert_eq!(pidfd_send_signal(me.fd(), 0), Ok(()));
        assert_eq!(pidfd_send_signal(Fd::new(-1), 0), Err(Errno::BadF));
    }
}
