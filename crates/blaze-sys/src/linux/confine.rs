//! A child confined before its program runs (D177, `Level::Inputs` on Linux): what is made in
//! the PARENT (a Landlock ruleset, a seccomp filter, resource limits), held in one
//! [`ChildConfine`], and the one function the CHILD calls between `clone` and `execve`
//! ([`enter`]), where only system calls run: no allocation, no lock, no unwinding.
//!
//! The order in the child, each step the condition of the next:
//! 1. `prctl(PR_SET_NO_NEW_PRIVS)`: no exec gains privileges (Landlock and an unprivileged
//!    seccomp filter both require it).
//! 2. each resource limit (`prlimit64` on the child itself): CPU seconds, address space,
//!    no core file.
//! 3. `landlock_restrict_self` over the ruleset made in the parent.
//! 4. the seccomp filter, last, so the calls above are not filtered.
//!
//! What this holds and what it does not is the bstd backend's doc (blaze-std
//! procs/hermetic/landlock.rs). The filter is built here from a [`Filter`] description, so its
//! instructions have one home and its syscall numbers one table per architecture ([`deny`]).
//!
//! The type lives here, not in blaze-core or bstd: blaze-sys builds it (the filter, the
//! limits' numbers) and is the one crate below both blaze-core, which carries it in
//! `RunLink::confine` and runs [`enter`] in its `clone(CLONE_VM|CLONE_VFORK)` child last before
//! `execve` (`process/spawn.rs`), and bstd, which prepares it from a hermetic run's plan.

use core::ffi::CStr;

use super::{
    AccessFs, AtFlags, CWD, EntryKind, Family, OpenFlags, OwnedFd, PathBeneath, RulesetAttr,
    SockFilter, SockFlags, SockKind, StatxMask, Unlink, getdents64, landlock_abi,
    landlock_add_rule_path, landlock_create_ruleset, mkdirat,
    nr::{CAPSET, IOCTL, MOUNT_SETATTR, MOVE_MOUNT, OPEN_TREE, PRLIMIT64},
    openat, raw, readlinkat, socket, statx, symlinkat,
    task::{MountFlags, Namespaces, UnmountFlags, chdir, mount, pivot_root, umount2, unshare},
    unlinkat, write,
};
use crate::{Errno, Fd, Path};

/// `struct rlimit64`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct Rlimit {
    pub soft: u64,
    pub hard: u64,
}

/// The resources a hermetic child is capped on (`RLIMIT_*`, asm-generic/resource.h; the same
/// numbers on aarch64 and x86-64).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Resource {
    /// `RLIMIT_CPU` (0): CPU seconds; past the soft limit `SIGXCPU`, past the hard `SIGKILL`.
    Cpu,
    /// `RLIMIT_AS` (9): bytes of address space; past it an allocation fails (`ENOMEM`).
    AddressSpace,
    /// `RLIMIT_CORE` (4): bytes of a core file; 0 writes none (a crash leaves nothing beside it).
    Core,
}

impl Resource {
    const fn number(self) -> usize {
        match self {
            Resource::Cpu => 0,
            Resource::AddressSpace => 9,
            Resource::Core => 4,
        }
    }
}

/// `prlimit64` of `pid` (0: the calling process): `resource` set to `limit`. Called in the child
/// on itself, or in the parent on a child it has not yet let run.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn prlimit(pid: i32, resource: Resource, limit: Rlimit) -> Result<(), Errno> {
    // SAFETY: `limit` is a live `struct rlimit64` the kernel reads and does not keep; the old
    // value's pointer is null (not asked).
    raw::check(unsafe {
        raw::syscall4(
            PRLIMIT64,
            pid as isize as usize,
            resource.number(),
            core::ptr::from_ref(&limit) as usize,
            0,
        )
    })
    .map(|_| ())
}

/// The most limits one child is given (one per [`Resource`]).
pub const MOST_LIMITS: usize = 3;

/// The most instructions a [`Filter`] or test filter builds into (the deny list, two each, and
/// the head and tail): a table has a cap (rule 18).
pub const MOST_FILTER: usize = 192;

/// What a hermetic child's seccomp filter refuses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Filter {
    /// Whether the program may start other programs (a helper the spec names: Landlock's
    /// `EXECUTE` right names which). Without it, `clone` without `CLONE_THREAD` is `EPERM` (a
    /// thread is still made), `clone3` is `ENOSYS` (glibc and musl then use `clone`), and on
    /// x86-64 `fork` and `vfork` are `EPERM`.
    pub processes: bool,
}

/// What the parent made for one child: the ruleset, the filter's instructions, the limits, and
/// an optional pre-opened, pre-verified executable descriptor (`O_RDONLY | O_CLOEXEC`) executed
/// via `execveat(fd, "", argv, envp, AT_EMPTY_PATH)` so the child runs the exact verified inode.
/// Borrowed by the child between `clone` and `execve`: it is the parent's memory (`CLONE_VM`)
/// and lives until the start returns.
pub struct ChildConfine<'a> {
    pub ruleset: Fd,
    pub filter: &'a [SockFilter],
    pub limits: &'a [(Resource, Rlimit)],
    pub exec_fd: Option<Fd>,
}

/// Which step of [`enter`] the kernel refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    NoNewPrivs,
    Limit(Resource),
    Landlock,
    Seccomp,
}

/// In the child, before `execve`: the module's four steps, in order. A refusal names the step;
/// the child then writes it to its error pipe and exits, and its program never runs (rule 9).
#[blaze::noalloc]
#[blaze::nopanic]
pub fn enter(confine: &ChildConfine<'_>) -> Result<(), (Step, Errno)> {
    super::prctl(super::Prctl::NoNewPrivs).map_err(|errno| (Step::NoNewPrivs, errno))?;
    for &(resource, limit) in confine.limits {
        prlimit(0, resource, limit).map_err(|errno| (Step::Limit(resource), errno))?;
    }
    super::landlock_restrict_self(confine.ruleset).map_err(|errno| (Step::Landlock, errno))?;
    super::seccomp_filter(confine.filter, super::SeccompFlags::NONE).map_err(|errno| (Step::Seccomp, errno))?;
    Ok(())
}

// Classic BPF (linux/filter.h, linux/seccomp.h).
const LD_W_ABS: u16 = 0x20;
const JEQ_K: u16 = 0x15;
const JGE_K: u16 = 0x35;
const JSET_K: u16 = 0x45;
const RET_K: u16 = 0x06;
const RET_KILL_PROCESS: u32 = 0x8000_0000;
const RET_ERRNO: u32 = 0x0005_0000;
const RET_ALLOW: u32 = 0x7fff_0000;
const EPERM: u32 = 1;
const ENOSYS: u32 = 38;
/// `struct seccomp_data`: `nr` at 0, `arch` at 4, `args[0]`'s low word at 16 and high word at 20
/// (little-endian).
const DATA_NR: u32 = 0;
const DATA_ARCH: u32 = 4;
const DATA_ARG0: u32 = 16;
const DATA_ARG0_HI: u32 = 20;
/// `CLONE_THREAD`.
const CLONE_THREAD: u32 = 0x0001_0000;

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7;

/// The calls every hermetic child is refused with `EPERM`, by this architecture's numbers.
/// Network: `socket` (no socket of any family: no network, no unix socket to a host daemon).
/// Others' memory and tracing: `ptrace`, `process_vm_readv`, `process_vm_writev`. The mount
/// table and namespaces: `mount`, `umount2`, `pivot_root`, `unshare`, `setns`, `open_tree`,
/// `move_mount`, `fsopen`, `fsmount`. The kernel's other doors: `bpf`, `perf_event_open`
/// (the host's counters), `io_uring_setup` (a second path for every call above),
/// `keyctl`, `add_key`, `request_key`.
pub mod deny {
    /// `socket`: blocked outright in hermetic build scripts ([`EPERM`][0]), restricted in the test
    /// sandbox to `AF_UNIX`, `AF_INET`, and `AF_INET6` (inside a private `CLONE_NEWNET` with only
    /// sandbox-local loopback and an isolated mount namespace).
    #[cfg(target_arch = "x86_64")]
    pub const SOCKET: u32 = 41;
    #[cfg(target_arch = "aarch64")]
    pub const SOCKET: u32 = 198;
    /// `socketpair`: in the test sandbox restricted to `AF_UNIX`.
    #[cfg(target_arch = "x86_64")]
    pub const SOCKETPAIR: u32 = 53;
    #[cfg(target_arch = "aarch64")]
    pub const SOCKETPAIR: u32 = 199;
    /// `prlimit64`: in the test sandbox restricted to `pid == 0` (self only).
    #[cfg(target_arch = "x86_64")]
    pub const PRLIMIT64: u32 = 302;
    #[cfg(target_arch = "aarch64")]
    pub const PRLIMIT64: u32 = 261;
    /// Extra syscalls denied with `EPERM` in the test sandbox on top of `&EPERM[1..]` (all of
    /// [`EPERM`] except `socket`, which `build_test` filters by family):
    /// - remaining mount API: `fsconfig` (431), `fspick` (433), `mount_setattr` (442);
    /// - kill-domain escape after setup: `setpgid`, `setsid`;
    /// - privileged host administration (defense-in-depth on top of `CLONE_NEWUSER` + `capset(0)`):
    ///   `reboot`, `kexec_load`, `kexec_file_load`, `init_module`, `finit_module`, `delete_module`,
    ///   `swapon`, `swapoff`, `settimeofday`, `clock_settime`, `clock_adjtime`, `adjtimex`,
    ///   `sethostname`, `setdomainname`, `acct`, `syslog`, `vhangup` (+ `ioperm`, `iopl` on x86-64);
    /// - cross-process control / memory inspection: `pidfd_getfd`, `pidfd_open`, `kcmp`,
    ///   `sched_setscheduler`, `sched_setparam`, `sched_setattr`, `sched_setaffinity`,
    ///   `setpriority`, `ioprio_set`, `migrate_pages`, `move_pages`, `mbind`,
    ///   `process_madvise`, `process_mrelease`.
    #[cfg(target_arch = "x86_64")]
    pub const TEST_EXTRA_EPERM: [u32; 38] = [
        431, 433, 442,
        109, 112,
        169, 246, 320, 175, 313, 176, 167, 168, 164, 227, 305, 159, 170, 171, 163, 103, 153, 173, 172,
        438, 434, 312, 144, 142, 314, 203, 141, 251, 256, 279, 237, 440, 448,
    ];
    #[cfg(target_arch = "aarch64")]
    pub const TEST_EXTRA_EPERM: [u32; 36] = [
        431, 433, 442,
        154, 157,
        142, 104, 294, 105, 273, 106, 224, 225, 170, 112, 266, 171, 161, 162, 89, 116, 58,
        438, 434, 272, 119, 118, 274, 122, 140, 30, 238, 239, 235, 440, 448,
    ];
    #[cfg(target_arch = "x86_64")]
    pub const EPERM: [u32; 19] = [41, 101, 310, 311, 165, 166, 155, 272, 308, 428, 429, 430, 432, 321, 298, 425, 250, 248, 249];
    #[cfg(target_arch = "aarch64")]
    pub const EPERM: [u32; 19] = [198, 117, 270, 271, 40, 39, 41, 97, 268, 428, 429, 430, 432, 280, 241, 425, 219, 217, 218];
    /// `clone`.
    #[cfg(target_arch = "x86_64")]
    pub const CLONE: u32 = 56;
    #[cfg(target_arch = "aarch64")]
    pub const CLONE: u32 = 220;
    /// `clone3` (the same number on both).
    pub const CLONE3: u32 = 435;
    /// `fork` and `vfork`: x86-64 only (aarch64 has neither).
    #[cfg(target_arch = "x86_64")]
    pub const FORKS: [u32; 2] = [57, 58];
    #[cfg(target_arch = "aarch64")]
    pub const FORKS: [u32; 0] = [];
    /// x86-64's x32 ABI marks its numbers with this bit; every such call is refused whole.
    #[cfg(target_arch = "x86_64")]
    pub const X32_BIT: Option<u32> = Some(0x4000_0000);
    #[cfg(target_arch = "aarch64")]
    pub const X32_BIT: Option<u32> = None;
}

const fn stmt(code: u16, k: u32) -> SockFilter {
    SockFilter { code, jt: 0, jf: 0, k }
}

const fn jump(code: u16, k: u32, jt: u8, jf: u8) -> SockFilter {
    SockFilter { code, jt, jf, k }
}

/// `filter` as instructions in `out`: a call of another architecture kills the process; each
/// denied call answers its errno; everything else is allowed. Returns the instructions written.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn build(filter: Filter, out: &mut [SockFilter; MOST_FILTER]) -> &[SockFilter] {
    let mut at = 0;
    let mut put = |instruction: SockFilter| {
        out[at] = instruction;
        at += 1;
    };
    put(stmt(LD_W_ABS, DATA_ARCH));
    put(jump(JEQ_K, AUDIT_ARCH, 1, 0));
    put(stmt(RET_K, RET_KILL_PROCESS));
    put(stmt(LD_W_ABS, DATA_NR));
    if let Some(bit) = deny::X32_BIT {
        put(jump(JGE_K, bit, 0, 1));
        put(stmt(RET_K, RET_KILL_PROCESS));
    }
    for nr in deny::EPERM {
        put(jump(JEQ_K, nr, 0, 1));
        put(stmt(RET_K, RET_ERRNO | EPERM));
    }
    if !filter.processes {
        for nr in deny::FORKS {
            put(jump(JEQ_K, nr, 0, 1));
            put(stmt(RET_K, RET_ERRNO | EPERM));
        }
        put(jump(JEQ_K, deny::CLONE3, 0, 1));
        put(stmt(RET_K, RET_ERRNO | ENOSYS));
        // `clone`: a thread (CLONE_THREAD in its flags, `args[0]`) is allowed, a process not.
        put(jump(JEQ_K, deny::CLONE, 0, 3));
        put(stmt(LD_W_ABS, DATA_ARG0));
        put(jump(JSET_K, CLONE_THREAD, 1, 0));
        put(stmt(RET_K, RET_ERRNO | EPERM));
    }
    put(stmt(RET_K, RET_ALLOW));
    &out[..at]
}

const AF_UNIX: u32 = 1;
const AF_INET: u32 = 2;
const AF_INET6: u32 = 10;
/// All Linux `CLONE_NEW*` namespace bits (`NEWNS | NEWCGROUP | NEWUTS | NEWIPC | NEWUSER | NEWPID | NEWNET | NEWTIME`).
const CLONE_NEW_MASK: u32 = 0x7E02_0080;

const _: () = assert!(deny::EPERM[0] == deny::SOCKET);

/// Seccomp filter for a sandboxed test child (`docs/v2/plan/design/test.md`, piece 9), layered on
/// top of user/net/ipc/mount namespaces, `MOUNT_ATTR_RDONLY` root, capability drop, `RLIMIT_CORE=0`,
/// `PR_SET_NO_NEW_PRIVS`, and Landlock ABI >= 6:
/// - Denies all syscalls in `&deny::EPERM[1..]` (`ptrace`, `process_vm_*`, mounts, namespaces,
///   `bpf`, `perf_event_open`, `io_uring_setup`, keyrings) and [`deny::TEST_EXTRA_EPERM`]
///   (`fsconfig`/`fspick`/`mount_setattr`, `setpgid`/`setsid`, host-administration syscalls, and
///   cross-process control syscalls) with `EPERM`.
/// - Restricts `socket(domain, ..)` to `AF_UNIX`, `AF_INET`, and `AF_INET6` (inside a private
///   `CLONE_NEWNET` with sandbox-local loopback only and masked host runtime/temp mounts) and
///   `socketpair(domain, ..)` to `AF_UNIX`.
/// - Restricts `prlimit64(pid, ..)` to `pid == 0` (self only).
/// - Denies `clone3` with `ENOSYS` (glibc/musl fall back to `clone`) and rejects any `clone`
///   call carrying a `CLONE_NEW*` namespace flag with `EPERM`, while permitting ordinary threads
///   and helper subprocesses.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn build_test(out: &mut [SockFilter; MOST_FILTER]) -> &[SockFilter] {
    let mut at = 0;
    let mut put = |instruction: SockFilter| {
        if let Some(slot) = out.get_mut(at) {
            *slot = instruction;
            at += 1;
        }
    };
    put(stmt(LD_W_ABS, DATA_ARCH));
    put(jump(JEQ_K, AUDIT_ARCH, 1, 0));
    put(stmt(RET_K, RET_KILL_PROCESS));
    put(stmt(LD_W_ABS, DATA_NR));
    if let Some(bit) = deny::X32_BIT {
        put(jump(JGE_K, bit, 0, 1));
        put(stmt(RET_K, RET_KILL_PROCESS));
    }
    for &nr in deny::EPERM.iter().skip(1) {
        put(jump(JEQ_K, nr, 0, 1));
        put(stmt(RET_K, RET_ERRNO | EPERM));
    }
    for nr in deny::TEST_EXTRA_EPERM {
        put(jump(JEQ_K, nr, 0, 1));
        put(stmt(RET_K, RET_ERRNO | EPERM));
    }
    put(jump(JEQ_K, deny::CLONE3, 0, 1));
    put(stmt(RET_K, RET_ERRNO | ENOSYS));
    // `socket(domain, ..)`: allow AF_UNIX, AF_INET, AF_INET6 only (private netns + masked mounts).
    put(jump(JEQ_K, deny::SOCKET, 0, 6));
    put(stmt(LD_W_ABS, DATA_ARG0));
    put(jump(JEQ_K, AF_UNIX, 3, 0));
    put(jump(JEQ_K, AF_INET, 2, 0));
    put(jump(JEQ_K, AF_INET6, 1, 0));
    put(stmt(RET_K, RET_ERRNO | EPERM));
    put(stmt(RET_K, RET_ALLOW));
    // `socketpair(domain, ..)`: allow AF_UNIX only; deny other families with EPERM.
    put(jump(JEQ_K, deny::SOCKETPAIR, 0, 4));
    put(stmt(LD_W_ABS, DATA_ARG0));
    put(jump(JEQ_K, AF_UNIX, 0, 1));
    put(stmt(RET_K, RET_ALLOW));
    put(stmt(RET_K, RET_ERRNO | EPERM));
    // `prlimit64(pid, ..)`: allow only pid == 0 (both low and high 32-bit words zero).
    put(jump(JEQ_K, deny::PRLIMIT64, 0, 6));
    put(stmt(LD_W_ABS, DATA_ARG0));
    put(jump(JEQ_K, 0, 0, 3));
    put(stmt(LD_W_ABS, DATA_ARG0_HI));
    put(jump(JEQ_K, 0, 0, 1));
    put(stmt(RET_K, RET_ALLOW));
    put(stmt(RET_K, RET_ERRNO | EPERM));
    // `clone(flags, ..)`: allow ordinary threads and helpers; deny any CLONE_NEW* flag with EPERM.
    put(jump(JEQ_K, deny::CLONE, 0, 3));
    put(stmt(LD_W_ABS, DATA_ARG0));
    put(jump(JSET_K, CLONE_NEW_MASK, 0, 1));
    put(stmt(RET_K, RET_ERRNO | EPERM));
    put(stmt(RET_K, RET_ALLOW));
    out.get(..at).unwrap_or(&[])
}

/// Minimum Landlock ABI required to confine a Linux test process: ABI 6 adds signal and abstract
/// Unix socket scoping (`LANDLOCK_SCOPE_SIGNAL | LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET`), ensuring
/// a test cannot signal the blaze runner or other same-UID processes outside its sandbox (rule 9).
pub const LEAST_TEST_ABI: u32 = 6;

/// `LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET | LANDLOCK_SCOPE_SIGNAL` (ABI 6).
const TEST_SCOPES: u64 = 0b11;

/// Why a Linux test sandbox could not be entered (rule 9: never falls back to unsandboxed).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TestConfineError {
    /// `unshare(CLONE_NEWUSER | CLONE_NEWNET | CLONE_NEWIPC | CLONE_NEWNS)` failed.
    Unshare(Errno),
    /// Writing `/proc/self/{setgroups,uid_map,gid_map}` failed.
    IdMap { which: &'static str, errno: Errno },
    /// Bringing up `lo` in the private network namespace failed.
    Loopback(Errno),
    /// Isolating the mount namespace (`MS_PRIVATE`, `open_tree`, `tmpfs`, `mount_setattr`, `move_mount`, `chdir`) failed.
    Mount { which: &'static str, errno: Errno },
    /// Dropping process capabilities (`capset`) failed.
    CapDrop(Errno),
    /// `landlock_create_ruleset(VERSION)` failed (`ENOSYS`/`EOPNOTSUPP`).
    AbiProbe(Errno),
    /// Kernel Landlock ABI is older than [`LEAST_TEST_ABI`].
    AbiTooOld { abi: u32, least: u32 },
    /// Creating the Landlock ruleset failed.
    CreateRuleset(Errno),
    /// Opening one of the fixed paths (`/`, `temp`, or `/dev/null`) failed.
    OpenPath { which: &'static str, errno: Errno },
    /// Adding a Landlock rule for `which` failed.
    AddRule { which: &'static str, errno: Errno },
    /// Final confinement step (`PR_SET_NO_NEW_PRIVS`, `RLIMIT_CORE`, `landlock_restrict_self`, or `seccomp`) failed.
    Enter { step: Step, errno: Errno },
}

impl TestConfineError {
    /// Raw errno for callers that report an OS error code (0 when none applies, e.g. ABI too old).
    pub const fn raw_errno(self) -> u16 {
        match self {
            TestConfineError::Unshare(e)
            | TestConfineError::IdMap { errno: e, .. }
            | TestConfineError::Loopback(e)
            | TestConfineError::Mount { errno: e, .. }
            | TestConfineError::CapDrop(e)
            | TestConfineError::AbiProbe(e)
            | TestConfineError::CreateRuleset(e)
            | TestConfineError::OpenPath { errno: e, .. }
            | TestConfineError::AddRule { errno: e, .. }
            | TestConfineError::Enter { errno: e, .. } => e.raw(),
            TestConfineError::AbiTooOld { .. } => 0,
        }
    }
}

/// Owned raw file descriptor closed on drop, local to sandbox setup.
struct LocalFd(i32);

impl LocalFd {
    fn raw(&self) -> i32 {
        self.0
    }
}

impl Drop for LocalFd {
    fn drop(&mut self) {
        if let Ok(raw_fd) = usize::try_from(self.0) {
            // SAFETY: `self.0` is an open file descriptor owned exclusively by this `LocalFd`.
            let _ = unsafe { raw::syscall1(super::nr::CLOSE, raw_fd) };
        }
    }
}

/// Formats `u32` as decimal ASCII into `buf`, returning bytes written.
#[blaze::noalloc]
#[blaze::nopanic]
fn fmt_u32(mut n: u32, buf: &mut [u8]) -> usize {
    let mut tmp = [0u8; 10];
    let mut len = 0usize;
    loop {
        if let Some(slot) = tmp.get_mut(len) {
            *slot = b'0' + u8::try_from(n % 10).unwrap_or(0);
            len += 1;
        }
        n /= 10;
        if n == 0 {
            break;
        }
    }
    let count = len.min(buf.len());
    for (i, dst) in buf.iter_mut().take(count).enumerate() {
        if let Some(&digit) = tmp.get(len.saturating_sub(1 + i)) {
            *dst = digit;
        }
    }
    count
}

#[blaze::noalloc]
#[blaze::nopanic]
fn write_proc_bytes(which: &'static str, path: &CStr, data: &[u8]) -> Result<(), TestConfineError> {
    let flags = OpenFlags::WRITE.with(OpenFlags::CLOSE_ON_EXEC);
    let fd = openat(CWD, Path::new(path), flags, 0).map_err(|errno| TestConfineError::IdMap { which, errno })?;
    let wrote = write(fd.fd(), data).map_err(|errno| TestConfineError::IdMap { which, errno })?;
    if wrote != data.len() {
        return Err(TestConfineError::IdMap { which, errno: Errno::Io });
    }
    Ok(())
}

#[blaze::noalloc]
#[blaze::nopanic]
fn write_id_map(which: &'static str, path: &CStr, id: u32) -> Result<(), TestConfineError> {
    let mut buf = [0u8; 40];
    let mut at = 0usize;
    let push_byte = |buf: &mut [u8; 40], at: &mut usize, b: u8| {
        if let Some(slot) = buf.get_mut(*at) {
            *slot = b;
            *at += 1;
        }
    };
    if let Some(rest) = buf.get_mut(at..) {
        at += fmt_u32(id, rest);
    }
    push_byte(&mut buf, &mut at, b' ');
    if let Some(rest) = buf.get_mut(at..) {
        at += fmt_u32(id, rest);
    }
    push_byte(&mut buf, &mut at, b' ');
    push_byte(&mut buf, &mut at, b'1');
    push_byte(&mut buf, &mut at, b'\n');
    let data = buf.get(..at).ok_or(TestConfineError::IdMap { which, errno: Errno::Inval })?;
    write_proc_bytes(which, path, data)
}

#[repr(C)]
struct IfReqFlags {
    name: [u8; 16],
    flags: u16,
    pad: [u8; 22],
}

#[blaze::noalloc]
#[blaze::nopanic]
fn bring_up_loopback() -> Result<(), TestConfineError> {
    const SIOCSIFFLAGS: usize = 0x8914;
    const IFF_UP: u16 = 0x1;
    const IFF_RUNNING: u16 = 0x40;
    let fd = socket(Family::V4, SockKind::Datagram, SockFlags::CLOSE_ON_EXEC).map_err(TestConfineError::Loopback)?;
    let ifr = IfReqFlags {
        name: [b'l', b'o', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        flags: IFF_UP | IFF_RUNNING,
        pad: [0; 22],
    };
    // SAFETY: `ifr` is a valid 40-byte Linux `struct ifreq` read by `SIOCSIFFLAGS` and not kept.
    raw::check(unsafe {
        raw::syscall3(IOCTL, super::fs::arg(fd.fd()), SIOCSIFFLAGS, core::ptr::from_ref(&ifr).addr())
    })
    .map_err(TestConfineError::Loopback)?;
    Ok(())
}

#[repr(C)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

#[blaze::noalloc]
#[blaze::nopanic]
fn open_tree_clone(which: &'static str, path: &CStr) -> Result<LocalFd, TestConfineError> {
    const OPEN_TREE_CLONE: usize = 1;
    const OPEN_TREE_CLOEXEC: usize = 0o200_0000;
    // SAFETY: `path` is a live NUL-terminated C string read by the kernel and not kept.
    let fd = raw::check(unsafe {
        raw::syscall3(OPEN_TREE, super::fs::arg(CWD), path.as_ptr().addr(), OPEN_TREE_CLONE | OPEN_TREE_CLOEXEC)
    })
    .map_err(|errno| TestConfineError::Mount { which, errno })?;
    let raw_fd = i32::try_from(fd).map_err(|_| TestConfineError::Mount { which, errno: Errno::Inval })?;
    Ok(LocalFd(raw_fd))
}

#[blaze::noalloc]
#[blaze::nopanic]
fn move_mount_empty(which: &'static str, tree: &LocalFd, target: &CStr) -> Result<(), TestConfineError> {
    const MOVE_MOUNT_F_EMPTY_PATH: usize = 4;
    let tree_arg = usize::try_from(tree.raw()).map_err(|_| TestConfineError::Mount { which, errno: Errno::BadF })?;
    // SAFETY: `c""` and `target` are live NUL-terminated C strings; `tree` is an open detached mount fd.
    raw::check(unsafe {
        raw::syscall5(
            MOVE_MOUNT,
            tree_arg,
            c"".as_ptr().addr(),
            super::fs::arg(CWD),
            target.as_ptr().addr(),
            MOVE_MOUNT_F_EMPTY_PATH,
        )
    })
    .map_err(|errno| TestConfineError::Mount { which, errno })?;
    Ok(())
}

#[blaze::noalloc]
#[blaze::nopanic]
fn mount_setattr_ro(which: &'static str, path: &CStr, recursive: bool) -> Result<(), TestConfineError> {
    const AT_RECURSIVE: usize = 0x8000;
    const MOUNT_ATTR_RDONLY: u64 = 1;
    let attr = MountAttr { attr_set: MOUNT_ATTR_RDONLY, attr_clr: 0, propagation: 0, userns_fd: 0 };
    let flags = if recursive { AT_RECURSIVE } else { 0 };
    // SAFETY: `path` and `attr` are live for the duration of the syscall and not kept.
    raw::check(unsafe {
        raw::syscall5(
            MOUNT_SETATTR,
            super::fs::arg(CWD),
            path.as_ptr().addr(),
            flags,
            core::ptr::from_ref(&attr).addr(),
            core::mem::size_of::<MountAttr>(),
        )
    })
    .map_err(|errno| TestConfineError::Mount { which, errno })?;
    Ok(())
}

/// Host temporary/runtime directories masked with fresh empty read-only `tmpfs` mounts so host
/// pathname UNIX sockets (e.g. `/run/user/$UID/*`, `/tmp/*`) and stale host scratch files are
/// invisible inside the test sandbox (`docs/cargo.md:423-425`).
const MASKED_DIRS: [(&CStr, &[u8]); 4] = [
    (c"/run", b"/run/"),
    (c"/var/tmp", b"/var/tmp/"),
    (c"/dev/shm", b"/dev/shm/"),
    (c"/tmp", b"/tmp/"),
];

#[blaze::noalloc]
#[blaze::nopanic]
fn masked_prefix_len(path: &CStr) -> Option<usize> {
    let bytes = path.to_bytes();
    for &(_, prefix) in &MASKED_DIRS {
        if bytes.starts_with(prefix) {
            return Some(prefix.len());
        }
    }
    None
}

/// Creates directory components of `path` beneath its masked root (`start` = prefix length) so
/// `move_mount` has a mountpoint directory inside the freshly mounted empty `tmpfs`.
#[blaze::noalloc]
#[blaze::nopanic]
fn mkdir_under_masked(which: &'static str, path: &CStr, start: usize) -> Result<(), TestConfineError> {
    let bytes = path.to_bytes();
    if bytes.len() + 1 > 4096 {
        return Err(TestConfineError::Mount { which, errno: Errno::NameTooLong });
    }
    let mut buf = [0u8; 4096];
    let mut i = start;
    while i <= bytes.len() {
        let is_sep = i == bytes.len() || bytes.get(i).copied() == Some(b'/');
        if is_sep && i > start {
            if let (Some(dst), Some(src)) = (buf.get_mut(..i), bytes.get(..i)) {
                dst.copy_from_slice(src);
            }
            if let Some(nul) = buf.get_mut(i) {
                *nul = 0;
            }
            if let Some(slice) = buf.get(..=i)
                && let Ok(sub) = CStr::from_bytes_with_nul(slice)
            {
                match mkdirat(CWD, Path::new(sub), 0o700) {
                    Ok(()) | Err(Errno::Exist) | Err(Errno::RoFs) => {}
                    Err(errno) => return Err(TestConfineError::Mount { which, errno }),
                }
            }
        }
        i += 1;
    }
    Ok(())
}

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: i32,
}

#[blaze::noalloc]
#[blaze::nopanic]
fn drop_all_capabilities() -> Result<(), TestConfineError> {
    const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
    let hdr = CapHeader { version: LINUX_CAPABILITY_VERSION_3, pid: 0 };
    let data = [0u32; 6];
    // SAFETY: `hdr` and `data` are valid kernel `cap_user_header_t` / `cap_user_data_t[2]`.
    raw::check(unsafe {
        raw::syscall2(CAPSET, core::ptr::from_ref(&hdr).addr(), data.as_ptr().addr())
    })
    .map_err(TestConfineError::CapDrop)?;
    Ok(())
}

/// Candidate short mountpoints inside the fresh `/tmp` tmpfs for the test's isolated RW `$TMPDIR`
/// (6 bytes each, leaving ample headroom for pathname `AF_UNIX` sockets under Linux's 108-byte
/// `sun_path` limit). Chosen to avoid colliding with any `keep_ro` path under `/tmp`.
const TEMP_MOUNTS: [&CStr; 7] = [c"/tmp/t", c"/tmp/u", c"/tmp/v", c"/tmp/w", c"/tmp/x", c"/tmp/y", c"/tmp/z"];

#[blaze::noalloc]
#[blaze::nopanic]
fn paths_overlap(a: &[u8], b: &[u8]) -> bool {
    if a == b {
        return true;
    }
    if a.len() > b.len() && a.starts_with(b) && a.get(b.len()).copied() == Some(b'/') {
        return true;
    }
    b.len() > a.len() && b.starts_with(a) && b.get(a.len()).copied() == Some(b'/')
}

#[blaze::noalloc]
#[blaze::nopanic]
fn pick_temp_mount(keep_ro: &[&CStr]) -> Result<&'static CStr, TestConfineError> {
    for &candidate in &TEMP_MOUNTS {
        let cand = candidate.to_bytes();
        let mut collides = false;
        for &path in keep_ro {
            if paths_overlap(path.to_bytes(), cand) {
                collides = true;
                break;
            }
        }
        if !collides {
            return Ok(candidate);
        }
    }
    Err(TestConfineError::Mount { which: "pick temp mount", errno: Errno::Exist })
}

#[blaze::noalloc]
#[blaze::nopanic]
fn make_prefixed_cstr<'a>(buf: &'a mut [u8; 512], prefix: &[u8], name: &[u8]) -> Option<&'a CStr> {
    let total = prefix.len().checked_add(name.len())?;
    if total + 1 > buf.len() {
        return None;
    }
    buf.get_mut(..prefix.len())?.copy_from_slice(prefix);
    buf.get_mut(prefix.len()..total)?.copy_from_slice(name);
    *buf.get_mut(total)? = 0;
    CStr::from_bytes_with_nul(buf.get(..=total)?).ok()
}

/// Mirrors every child entry of `src_dir` into `dst_dir` (which must be an existing directory on
/// the new tmpfs root), skipping `skip_a` and `skip_b` (and `.` / `..`). Fails closed on mount
/// errors so resource limits cannot silently leave empty placeholders in the pivoted root.
#[blaze::noalloc]
#[blaze::nopanic]
fn mirror_dir_into(
    src_path: &CStr,
    src_prefix: &[u8],
    dst_path: &CStr,
    dst_prefix: &[u8],
    skip_a: &[u8],
    skip_b: &[u8],
    skip_mount_dir: &[u8],
) -> Result<bool, TestConfineError> {
    let dir_flags = OpenFlags::READ.with(OpenFlags::DIRECTORY).with(OpenFlags::CLOSE_ON_EXEC);
    let src_fd = match openat(CWD, Path::new(src_path), dir_flags, 0) {
        Ok(fd) => fd,
        Err(Errno::NoEnt | Errno::NotDir) => return Ok(false),
        Err(errno) => return Err(TestConfineError::OpenPath { which: "open mirror src", errno }),
    };
    let dst_fd = openat(CWD, Path::new(dst_path), dir_flags, 0)
        .map_err(|errno| TestConfineError::OpenPath { which: "open mirror dst", errno })?;

    let mut dents = [0u8; 4096];
    let mut name_buf = [0u8; 512];
    let mut src_buf = [0u8; 512];
    let mut dst_buf = [0u8; 512];
    let mut link_buf = [0u8; 512];
    let mut link_c_buf = [0u8; 512];
    let mut saw_skip_a = false;
    loop {
        let entries = getdents64(src_fd.fd(), &mut dents)
            .map_err(|errno| TestConfineError::Mount { which: "getdents64 mirror", errno })?;
        let mut any = false;
        for entry in entries {
            any = true;
            let name = entry.name;
            if name == b"." || name == b".." || name == skip_b {
                continue;
            }
            if name == skip_a {
                saw_skip_a = true;
                continue;
            }
            let Some(name_c) = make_prefixed_cstr(&mut name_buf, b"", name) else {
                return Err(TestConfineError::Mount { which: "mirror name too long", errno: Errno::NameTooLong });
            };
            let Some(src_c) = make_prefixed_cstr(&mut src_buf, src_prefix, name) else {
                return Err(TestConfineError::Mount { which: "mirror src too long", errno: Errno::NameTooLong });
            };
            let Some(dst_c) = make_prefixed_cstr(&mut dst_buf, dst_prefix, name) else {
                return Err(TestConfineError::Mount { which: "mirror dst too long", errno: Errno::NameTooLong });
            };
            let kind = match entry.kind {
                EntryKind::Unknown => {
                    let st = statx(src_fd.fd(), Path::new(name_c), AtFlags::NO_FOLLOW, StatxMask::TYPE)
                        .map_err(|errno| TestConfineError::Mount { which: "statx mirror unknown", errno })?;
                    match st.mode & 0o170_000 {
                        0o040_000 => EntryKind::Dir,
                        0o100_000 => EntryKind::Regular,
                        0o120_000 => EntryKind::Link,
                        _ => EntryKind::Other(0),
                    }
                }
                other => other,
            };
            match kind {
                EntryKind::Link => {
                    let target = readlinkat(src_fd.fd(), Path::new(name_c), &mut link_buf)
                        .map_err(|errno| TestConfineError::Mount { which: "readlink mirror", errno })?;
                    let Some(target_c) = make_prefixed_cstr(&mut link_c_buf, b"", target) else {
                        return Err(TestConfineError::Mount { which: "mirror symlink too long", errno: Errno::NameTooLong });
                    };
                    symlinkat(Path::new(target_c), dst_fd.fd(), Path::new(name_c))
                        .map_err(|errno| TestConfineError::Mount { which: "symlink mirror", errno })?;
                }
                EntryKind::Dir => {
                    mkdirat(dst_fd.fd(), Path::new(name_c), 0o755)
                        .map_err(|errno| TestConfineError::Mount { which: "mkdir mirror", errno })?;
                    if name != skip_mount_dir {
                        mount(
                            Some(Path::new(src_c)),
                            Path::new(dst_c),
                            None,
                            MountFlags::BIND.with(MountFlags::RECURSIVE),
                            None,
                        )
                        .map_err(|errno| TestConfineError::Mount { which: "bind mirror dir", errno })?;
                    }
                }
                EntryKind::Regular => {
                    let create_flags = OpenFlags::CREATE.with(OpenFlags::WRITE).with(OpenFlags::CLOSE_ON_EXEC);
                    openat(dst_fd.fd(), Path::new(name_c), create_flags, 0o600)
                        .map_err(|errno| TestConfineError::Mount { which: "create mirror file", errno })?;
                    mount(Some(Path::new(src_c)), Path::new(dst_c), None, MountFlags::BIND, None)
                        .map_err(|errno| TestConfineError::Mount { which: "bind mirror file", errno })?;
                }
                _ => {}
            }
        }
        if !any {
            break;
        }
    }
    Ok(saw_skip_a)
}

/// Switches the sandbox's root mount to a minimal private `tmpfs` mirroring the host's top-level
/// root entries while overlaying ONLY `/target/debug/bin` with read-only `bins_tree` (preserving
/// any existing host `/target` and `/target/debug` entries) so compile-time
/// `env!("CARGO_BIN_EXE_<name>")` paths exist as real, kernel-visible executable files.
#[blaze::noalloc]
#[blaze::nopanic]
fn pivot_root_with_bins(bins_tree: LocalFd) -> Result<(), TestConfineError> {
    let tmpfs_flags = MountFlags::NO_SUID.with(MountFlags::NO_DEV);
    mount(Some(Path::new(c"tmpfs")), Path::new(c"/tmp"), Some(Path::new(c"tmpfs")), tmpfs_flags, Some(c"size=4m,mode=0755"))
        .map_err(|errno| TestConfineError::Mount { which: "mount staging /tmp", errno })?;
    mkdirat(CWD, Path::new(c"/tmp/nr"), 0o755)
        .map_err(|errno| TestConfineError::Mount { which: "mkdir /tmp/nr", errno })?;
    mount(Some(Path::new(c"tmpfs")), Path::new(c"/tmp/nr"), Some(Path::new(c"tmpfs")), tmpfs_flags, Some(c"size=4m,mode=0755"))
        .map_err(|errno| TestConfineError::Mount { which: "mount newroot", errno })?;
    mkdirat(CWD, Path::new(c"/tmp/nr/.old"), 0o755)
        .map_err(|errno| TestConfineError::Mount { which: "mkdir /tmp/nr/.old", errno })?;

    let had_target = mirror_dir_into(c"/", b"/", c"/tmp/nr", b"/tmp/nr/", b"target", b".old", b"tmp")?;
    mkdirat(CWD, Path::new(c"/tmp/nr/target"), 0o755)
        .map_err(|errno| TestConfineError::Mount { which: "mkdir /target", errno })?;
    let had_debug = if had_target {
        mirror_dir_into(c"/target", b"/target/", c"/tmp/nr/target", b"/tmp/nr/target/", b"debug", b"", b"")?
    } else {
        false
    };
    mkdirat(CWD, Path::new(c"/tmp/nr/target/debug"), 0o755)
        .map_err(|errno| TestConfineError::Mount { which: "mkdir /target/debug", errno })?;
    if had_debug {
        let _ = mirror_dir_into(
            c"/target/debug",
            b"/target/debug/",
            c"/tmp/nr/target/debug",
            b"/tmp/nr/target/debug/",
            b"bin",
            b"",
            b"",
        )?;
    }
    mkdirat(CWD, Path::new(c"/tmp/nr/target/debug/bin"), 0o755)
        .map_err(|errno| TestConfineError::Mount { which: "mkdir /target/debug/bin", errno })?;
    move_mount_empty("move_mount /target/debug/bin", &bins_tree, c"/tmp/nr/target/debug/bin")?;
    drop(bins_tree);
    mount_setattr_ro("mount_setattr /target/debug/bin ro", c"/tmp/nr/target/debug/bin", true)?;

    pivot_root(Path::new(c"/tmp/nr"), Path::new(c"/tmp/nr/.old"))
        .map_err(|errno| TestConfineError::Mount { which: "pivot_root", errno })?;
    chdir(Path::new(c"/")).map_err(|errno| TestConfineError::Mount { which: "chdir /", errno })?;
    umount2(Path::new(c"/.old"), UnmountFlags::DETACH)
        .map_err(|errno| TestConfineError::Mount { which: "umount2 /.old", errno })?;
    let _ = unlinkat(CWD, Path::new(c"/.old"), Unlink::Dir);
    Ok(())
}

/// Enters the full Linux test sandbox in the current (single-threaded) process (`test.md`, piece 9):
/// 1. Unshares private user, network, IPC, and mount namespaces (`CLONE_NEWUSER | CLONE_NEWNET |
///    CLONE_NEWIPC | CLONE_NEWNS`) and maps the caller's effective UID/GID 1:1.
/// 2. Brings up loopback (`lo`) in the private network namespace so `127.0.0.1`/`::1` works while
///    all non-loopback network traffic fails with `ENETUNREACH` and host loopback/abstract sockets
///    are unreachable.
/// 3. Isolates the mount namespace: makes `/` recursively `MS_PRIVATE` and `MOUNT_ATTR_RDONLY`
///    (refusing writes AND metadata mutations like `chmod`/`chown`/`utime*`/`*xattr` everywhere
///    outside `$TMPDIR` with `EROFS`), masks `/run`, `/var/tmp`, `/dev/shm`, and `/tmp` with empty
///    read-only `tmpfs` (hiding host daemon pathname UNIX sockets), preserves any `keep_ro`
///    directories (e.g. code store, workspace checkout, or private socket dir when located under
///    `/tmp`) as read-only mounts, bind-mounts `bins_dir` (when provided) read-only at
///    `/target/debug/bin` via a private `pivot_root`, and bind-mounts `temp` (`$TMPDIR`) read-write
///    at a short non-colliding mountpoint under `/tmp` (returned to the caller).
/// 4. Drops all capabilities via `capset(0)`, sets `RLIMIT_CORE = 0`, sets `PR_SET_NO_NEW_PRIVS`,
///    enforces Landlock ABI >= 6 (`LANDLOCK_SCOPE_SIGNAL | LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET`
///    plus filesystem read/exec on `/`, writable `temp`, and `/dev/null`), and installs the
///    seccomp-bpf filter ([`build_test`]).
///
/// Must be called while the process is single-threaded (before spawning any control thread or
/// running guest code). Fails closed on every step (rule 9).
#[blaze::noalloc]
#[blaze::nopanic]
pub fn enter_test_sandbox(
    temp: &CStr,
    keep_ro: &[&CStr],
    cwd: Option<&CStr>,
    bins_dir: Option<&CStr>,
) -> Result<&'static CStr, TestConfineError> {
    let uid = super::host::geteuid();
    let gid = super::host::getegid();
    let temp_mount = pick_temp_mount(keep_ro)?;

    // 1. Enter private user, net, IPC, and mount namespaces.
    unshare(Namespaces::USER.with(Namespaces::NET).with(Namespaces::IPC).with(Namespaces::NS))
        .map_err(TestConfineError::Unshare)?;
    write_proc_bytes("setgroups", c"/proc/self/setgroups", b"deny")?;
    write_id_map("uid_map", c"/proc/self/uid_map", uid)?;
    write_id_map("gid_map", c"/proc/self/gid_map", gid)?;

    // 2. Enable loopback (`lo`) inside the private network namespace.
    bring_up_loopback()?;

    // 3. Isolate mount namespace: `/` recursively read-only, host `/run` & temp dirs masked with
    //    empty read-only tmpfs, `keep_ro` preserved read-only, optional `bins_dir` mounted RO at
    //    `/target/debug/bin`, and `temp` (`$TMPDIR`) mounted RW at `temp_mount`.
    mount(None, Path::new(c"/"), None, MountFlags::RECURSIVE.with(MountFlags::PRIVATE), None)
        .map_err(|errno| TestConfineError::Mount { which: "MS_PRIVATE /", errno })?;
    let temp_tree = open_tree_clone("open_tree temp", temp)?;
    let bins_tree = match bins_dir {
        Some(path) => Some(open_tree_clone("open_tree bins_dir", path)?),
        None => None,
    };

    // Snapshot any `keep_ro` paths (at most 6) that live under a masked directory before overmounting.
    let mut saved_ro: [Option<(LocalFd, usize)>; 6] = [None, None, None, None, None, None];
    for (&path, slot) in keep_ro.iter().zip(saved_ro.iter_mut()) {
        if let Some(prefix_len) = masked_prefix_len(path) {
            let tree = open_tree_clone("open_tree keep_ro", path)?;
            *slot = Some((tree, prefix_len));
        }
    }

    // When `CARGO_BIN_EXE_*` stubs are exposed, pivot to a private root tmpfs so
    // `/target/debug/bin/<package>/<name>` is a real, kernel-visible read-only file.
    if let Some(bins_tree) = bins_tree {
        pivot_root_with_bins(bins_tree)?;
    }

    // Lock all mounts recursively read-only BEFORE overmounting masked dirs so any hidden
    // mounts underneath them are read-only too.
    mount_setattr_ro("mount_setattr / ro", c"/", true)?;

    // Mount fresh empty tmpfs over host runtime/temp directories.
    let mut masked_mounted = [false; 4];
    let tmpfs_flags = MountFlags::NO_SUID.with(MountFlags::NO_DEV);
    for (&(dir, _), mounted) in MASKED_DIRS.iter().zip(masked_mounted.iter_mut()) {
        match mount(Some(Path::new(c"tmpfs")), Path::new(dir), Some(Path::new(c"tmpfs")), tmpfs_flags, Some(c"size=16m,mode=0755")) {
            Ok(()) => *mounted = true,
            Err(Errno::NoEnt) | Err(Errno::NotDir) => {}
            Err(errno) => return Err(TestConfineError::Mount { which: "mount tmpfs mask", errno }),
        }
    }

    // Re-attach any saved `keep_ro` subtrees inside the fresh tmpfs and lock them read-only.
    for (&path, slot) in keep_ro.iter().zip(saved_ro.iter_mut()) {
        if let Some((tree, prefix_len)) = slot.take() {
            mkdir_under_masked("mkdir keep_ro", path, prefix_len)?;
            move_mount_empty("move_mount keep_ro", &tree, path)?;
            drop(tree);
            mount_setattr_ro("mount_setattr keep_ro ro", path, true)?;
        }
    }

    // Create the short, non-colliding `$TMPDIR` mountpoint inside the fresh empty `/tmp` tmpfs.
    mkdir_under_masked("mkdir temp_mount", temp_mount, b"/tmp/".len())?;

    // Lock the fresh empty tmpfs masks themselves read-only (non-recursive so `keep_ro` stays).
    for (&(dir, _), &mounted) in MASKED_DIRS.iter().zip(masked_mounted.iter()) {
        if mounted {
            mount_setattr_ro("mount_setattr mask ro", dir, false)?;
        }
    }

    // Mount the cloned read-write `$TMPDIR` at `temp_mount`.
    move_mount_empty("move_mount temp rw", &temp_tree, temp_mount)?;
    drop(temp_tree);

    // Re-anchor cwd onto the visible read-only mount if requested.
    if let Some(dir) = cwd {
        chdir(Path::new(dir)).map_err(|errno| TestConfineError::Mount { which: "chdir cwd", errno })?;
    }

    // 4. Prepare Landlock ABI >= 6 ruleset (defense-in-depth on top of mount/net namespaces;
    //    scopes signals and abstract Unix sockets to this sandbox).
    let abi = landlock_abi().map_err(TestConfineError::AbiProbe)?;
    if abi < LEAST_TEST_ABI {
        return Err(TestConfineError::AbiTooOld { abi, least: LEAST_TEST_ABI });
    }
    let base_fs = AccessFs::ALL_ABI1.with(AccessFs::REFER).with(AccessFs::TRUNCATE);
    let handled_fs = base_fs.with(AccessFs::IOCTL_DEV);
    let attr = RulesetAttr {
        handled_access_fs: handled_fs.bits(),
        // Network isolation is enforced by the private `CLONE_NEWNET` (loopback-only); leaving
        // Landlock TCP net rights unhandled lets tests bind/connect on sandbox-local loopback.
        handled_access_net: 0,
        scoped: TEST_SCOPES,
    };
    let ruleset: OwnedFd = landlock_create_ruleset(&attr).map_err(TestConfineError::CreateRuleset)?;

    let path_flags = OpenFlags::PATH.with(OpenFlags::CLOSE_ON_EXEC);
    let dir_flags = path_flags.with(OpenFlags::DIRECTORY);

    let add_path = |which: &'static str, path: &CStr, flags: OpenFlags, access: AccessFs| -> Result<(), TestConfineError> {
        let fd = openat(CWD, Path::new(path), flags, 0).map_err(|errno| TestConfineError::OpenPath { which, errno })?;
        let rule = PathBeneath { allowed_access: access.bits(), parent_fd: fd.fd().raw() };
        landlock_add_rule_path(ruleset.fd(), &rule).map_err(|errno| TestConfineError::AddRule { which, errno })
    };

    let read_exec = AccessFs::READ_FILE.with(AccessFs::READ_DIR).with(AccessFs::EXECUTE);
    add_path("/", c"/", dir_flags, read_exec)?;
    let temp_access = AccessFs::from_bits(
        base_fs.bits() & !(AccessFs::MAKE_CHAR.with(AccessFs::MAKE_BLOCK).bits()),
    );
    add_path("temp", temp_mount, dir_flags, temp_access)?;
    let null_access = AccessFs::READ_FILE
        .with(AccessFs::WRITE_FILE)
        .with(AccessFs::TRUNCATE)
        .with(AccessFs::IOCTL_DEV);
    add_path("/dev/null", c"/dev/null", path_flags, null_access)?;

    // 5. Drop all capabilities in the new user namespace before installing no_new_privs/seccomp.
    drop_all_capabilities()?;

    // 6. Install no_new_privs, RLIMIT_CORE=0, Landlock, and seccomp filter.
    let mut filter = [SockFilter { code: 0, jt: 0, jf: 0, k: 0 }; MOST_FILTER];
    let filter_slice = build_test(&mut filter);
    let limits = [(Resource::Core, Rlimit { soft: 0, hard: 0 })];
    let confine = ChildConfine { ruleset: ruleset.fd(), filter: filter_slice, limits: &limits, exec_fd: None };
    enter(&confine).map_err(|(step, errno)| TestConfineError::Enter { step, errno })?;
    Ok(temp_mount)
}

#[cfg(test)]
mod tests {
    use super::{Filter, MOST_FILTER, RET_ALLOW, SockFilter, build, build_test};

    fn check_well_formed(filter: &[SockFilter]) {
        let last = filter.last().copied().expect("a filter");
        assert_eq!((last.code, last.k), (0x06, RET_ALLOW));
        for (at, instruction) in filter.iter().enumerate() {
            if instruction.code & 0x07 == 0x05 {
                assert!(at + 1 + usize::from(instruction.jt.max(instruction.jf)) < filter.len(), "jump at {at} leaves the filter");
            }
        }
    }

    /// The filter ends in its allow, fits its cap both ways, and every jump lands inside it.
    #[test]
    fn the_filter_is_well_formed_with_and_without_processes() {
        for processes in [false, true] {
            let mut out = [SockFilter { code: 0, jt: 0, jf: 0, k: 0 }; MOST_FILTER];
            check_well_formed(build(Filter { processes }, &mut out));
        }
        let mut test_out = [SockFilter { code: 0, jt: 0, jf: 0, k: 0 }; MOST_FILTER];
        check_well_formed(build_test(&mut test_out));
    }
}
