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
    AccessFs, AccessNet, CWD, OpenFlags, OwnedFd, PathBeneath, RulesetAttr, SockFilter,
    landlock_abi, landlock_add_rule_path, landlock_create_ruleset, nr::PRLIMIT64, openat, raw,
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
pub const MOST_FILTER: usize = 128;

/// What a hermetic child's seccomp filter refuses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Filter {
    /// Whether the program may start other programs (a helper the spec names: Landlock's
    /// `EXECUTE` right names which). Without it, `clone` without `CLONE_THREAD` is `EPERM` (a
    /// thread is still made), `clone3` is `ENOSYS` (glibc and musl then use `clone`), and on
    /// x86-64 `fork` and `vfork` are `EPERM`.
    pub processes: bool,
}

/// What the parent made for one child: the ruleset, the filter's instructions and the limits.
/// Borrowed by the child between `clone` and `execve`: it is the parent's memory (`CLONE_VM`)
/// and lives until the start returns.
pub struct ChildConfine<'a> {
    pub ruleset: Fd,
    pub filter: &'a [SockFilter],
    pub limits: &'a [(Resource, Rlimit)],
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
/// `struct seccomp_data`: `nr` at 0, `arch` at 4, `args[0]`'s low word at 16 (little-endian).
const DATA_NR: u32 = 0;
const DATA_ARCH: u32 = 4;
const DATA_ARG0: u32 = 16;
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
    /// `socketpair`: in the test sandbox restricted to `AF_UNIX` (anonymous pairs only; `socket`
    /// itself is in `EPERM` so pathname UNIX and network sockets cannot be opened).
    #[cfg(target_arch = "x86_64")]
    pub const SOCKETPAIR: u32 = 53;
    #[cfg(target_arch = "aarch64")]
    pub const SOCKETPAIR: u32 = 199;
    /// Extra syscalls denied with `EPERM` in the test sandbox on top of [`EPERM`]:
    /// - remaining mount API: `fsconfig` (431), `fspick` (433), `mount_setattr` (442);
    /// - kill-domain escape after setup: `setpgid`, `setsid`;
    /// - metadata mutation not mediated by Landlock: `chmod`/`fchmod`/`fchmodat`/`fchmodat2`,
    ///   `chown`/`lchown`/`fchown`/`fchownat`, `*setxattr`, `*removexattr`,
    ///   `utime`/`utimes`/`futimesat`/`utimensat`.
    #[cfg(target_arch = "x86_64")]
    pub const TEST_EXTRA_EPERM: [u32; 23] = [
        431, 433, 442,
        109, 112,
        90, 91, 268, 452,
        92, 94, 93, 260,
        188, 189, 190, 197, 198, 199,
        132, 235, 261, 280,
    ];
    #[cfg(target_arch = "aarch64")]
    pub const TEST_EXTRA_EPERM: [u32; 17] = [
        431, 433, 442,
        154, 157,
        52, 53, 452,
        55, 54,
        5, 6, 7, 14, 15, 16,
        88,
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
/// All Linux `CLONE_NEW*` namespace bits (`NEWNS | NEWCGROUP | NEWUTS | NEWIPC | NEWUSER | NEWPID | NEWNET | NEWTIME`).
const CLONE_NEW_MASK: u32 = 0x7E02_0080;

/// Seccomp filter for a sandboxed test child (`docs/v2/plan/design/test.md`, piece 9):
/// - Denies all syscalls in [`deny::EPERM`] (including `socket`, `ptrace`, mounts, namespaces,
///   `bpf`, `perf_event_open`, `io_uring_setup`, keyrings) and [`deny::TEST_EXTRA_EPERM`]
///   (`fsconfig`/`fspick`/`mount_setattr`, `setpgid`/`setsid`, and metadata-mutating `chmod`/
///   `chown`/`*xattr`/`utime*` syscalls not mediated by Landlock) with `EPERM`.
/// - Denies `clone3` with `ENOSYS` (glibc/musl fall back to `clone`) and rejects any `clone`
///   call carrying a `CLONE_NEW*` namespace flag with `EPERM`, while permitting ordinary threads
///   and helper subprocesses.
/// - Restricts `socketpair(domain, ..)` to `AF_UNIX` (anonymous in-sandbox IPC only; blocking
///   `socket()` prevents both off-machine network access and pathname `AF_UNIX` connections to
///   host daemons outside `$TMPDIR`).
#[blaze::noalloc]
#[blaze::nopanic]
pub fn build_test(out: &mut [SockFilter; MOST_FILTER]) -> &[SockFilter] {
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
    for nr in deny::TEST_EXTRA_EPERM {
        put(jump(JEQ_K, nr, 0, 1));
        put(stmt(RET_K, RET_ERRNO | EPERM));
    }
    put(jump(JEQ_K, deny::CLONE3, 0, 1));
    put(stmt(RET_K, RET_ERRNO | ENOSYS));
    // `socketpair(domain, ..)`: allow only anonymous AF_UNIX pairs; deny other families with EPERM.
    put(jump(JEQ_K, deny::SOCKETPAIR, 0, 4));
    put(stmt(LD_W_ABS, DATA_ARG0));
    put(jump(JEQ_K, AF_UNIX, 0, 1));
    put(stmt(RET_K, RET_ALLOW));
    put(stmt(RET_K, RET_ERRNO | EPERM));
    // `clone(flags, ..)`: allow ordinary threads and helpers; deny any CLONE_NEW* flag with EPERM.
    put(jump(JEQ_K, deny::CLONE, 0, 3));
    put(stmt(LD_W_ABS, DATA_ARG0));
    put(jump(JSET_K, CLONE_NEW_MASK, 0, 1));
    put(stmt(RET_K, RET_ERRNO | EPERM));
    put(stmt(RET_K, RET_ALLOW));
    &out[..at]
}

/// Minimum Landlock ABI required to confine a Linux test process: ABI 6 adds signal and abstract
/// Unix socket scoping (`LANDLOCK_SCOPE_SIGNAL | LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET`), ensuring
/// a test cannot signal the blaze runner or other same-UID processes outside its sandbox (rule 9).
pub const LEAST_TEST_ABI: u32 = 6;

/// `LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET | LANDLOCK_SCOPE_SIGNAL` (ABI 6).
const TEST_SCOPES: u64 = 0b11;

/// Why a Linux test sandbox could not be prepared (rule 9: never falls back to unsandboxed).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TestConfineError {
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
}

/// Prepared Linux test sandbox (Landlock ruleset + seccomp filter), ready to hand to
/// [`enter`] in the child before `execve` (or after `fork` in a zygote).
pub struct TestConfine {
    ruleset: OwnedFd,
    filter: [SockFilter; MOST_FILTER],
    filter_len: usize,
}

impl TestConfine {
    /// Builds the Linux test sandbox (`docs/v2/plan/design/test.md`, piece 9):
    /// - Requires Landlock ABI >= 6 (`TRUNCATE`, `BIND_TCP|CONNECT_TCP`, `IOCTL_DEV`, and
    ///   `LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET | LANDLOCK_SCOPE_SIGNAL`).
    /// - Read + execute anywhere under `/` (so checkout fixtures, system libraries, and the code
    ///   store are readable, and helper binaries can exec).
    /// - Write + create + remove + rename + socket + exec beneath `temp` (`$TMPDIR`), with no
    ///   char/block device creation and no writable paths anywhere outside `temp` and `/dev/null`.
    /// - File-only read + write + truncate + ioctl on `/dev/null`.
    /// - All TCP bind/connect denied by Landlock (zero net rules); seccomp further restricts
    ///   `socket()` to `AF_UNIX` only, blocks `setpgid`/`setsid`, rejects `clone3` (`ENOSYS`),
    ///   and rejects any `CLONE_NEW*` flag on `clone`.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn prepare(temp: &CStr) -> Result<TestConfine, TestConfineError> {
        let abi = landlock_abi().map_err(TestConfineError::AbiProbe)?;
        if abi < LEAST_TEST_ABI {
            return Err(TestConfineError::AbiTooOld { abi, least: LEAST_TEST_ABI });
        }
        let base_fs = AccessFs::ALL_ABI1.with(AccessFs::REFER).with(AccessFs::TRUNCATE);
        let handled_fs = base_fs.with(AccessFs::IOCTL_DEV);
        let handled_net = AccessNet::BIND_TCP.with(AccessNet::CONNECT_TCP);
        let attr = RulesetAttr {
            handled_access_fs: handled_fs.bits(),
            handled_access_net: handled_net.bits(),
            scoped: TEST_SCOPES,
        };
        let ruleset = landlock_create_ruleset(&attr).map_err(TestConfineError::CreateRuleset)?;

        let path_flags = OpenFlags::PATH.with(OpenFlags::CLOSE_ON_EXEC);
        let dir_flags = path_flags.with(OpenFlags::DIRECTORY);

        let add_path = |which: &'static str, path: &CStr, flags: OpenFlags, access: AccessFs| -> Result<(), TestConfineError> {
            let fd = openat(CWD, Path::new(path), flags, 0).map_err(|errno| TestConfineError::OpenPath { which, errno })?;
            let rule = PathBeneath { allowed_access: access.bits(), parent_fd: fd.fd().raw() };
            landlock_add_rule_path(ruleset.fd(), &rule).map_err(|errno| TestConfineError::AddRule { which, errno })
        };

        // 1. Read + execute anywhere under `/`.
        let read_exec = AccessFs::READ_FILE.with(AccessFs::READ_DIR).with(AccessFs::EXECUTE);
        add_path("/", c"/", dir_flags, read_exec)?;

        // 2. Full writable subtree under the test's own `$TMPDIR` (no char/block device creation).
        let temp_access = AccessFs::from_bits(
            base_fs.bits() & !(AccessFs::MAKE_CHAR.with(AccessFs::MAKE_BLOCK).bits()),
        );
        add_path("temp", temp, dir_flags, temp_access)?;

        // 3. `/dev/null`: file-only read + write + truncate + ioctl.
        let null_access = AccessFs::READ_FILE
            .with(AccessFs::WRITE_FILE)
            .with(AccessFs::TRUNCATE)
            .with(AccessFs::IOCTL_DEV);
        add_path("/dev/null", c"/dev/null", path_flags, null_access)?;

        let mut filter = [SockFilter { code: 0, jt: 0, jf: 0, k: 0 }; MOST_FILTER];
        let filter_len = build_test(&mut filter).len();
        Ok(TestConfine { ruleset, filter, filter_len })
    }

    /// Borrows this prepared confinement for [`enter`] or `RunLink::confine`.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn confine(&self) -> ChildConfine<'_> {
        ChildConfine { ruleset: self.ruleset.fd(), filter: &self.filter[..self.filter_len], limits: &[] }
    }
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
