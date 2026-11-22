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

use super::{SockFilter, nr::PRLIMIT64, raw};
use crate::{Errno, Fd};

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

/// The most instructions a [`Filter`] builds into (the deny list, two each, and the head and
/// tail): a table has a cap (rule 18).
pub const MOST_FILTER: usize = 80;

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

#[cfg(test)]
mod tests {
    use super::{Filter, MOST_FILTER, RET_ALLOW, SockFilter, build};

    /// The filter ends in its allow, fits its cap both ways, and every jump lands inside it.
    #[test]
    fn the_filter_is_well_formed_with_and_without_processes() {
        for processes in [false, true] {
            let mut out = [SockFilter { code: 0, jt: 0, jf: 0, k: 0 }; MOST_FILTER];
            let filter = build(Filter { processes }, &mut out);
            let last = filter.last().copied().expect("a filter");
            assert_eq!((last.code, last.k), (0x06, RET_ALLOW));
            for (at, instruction) in filter.iter().enumerate() {
                if instruction.code & 0x07 == 0x05 {
                    assert!(at + 1 + usize::from(instruction.jt.max(instruction.jf)) < filter.len(), "jump at {at} leaves the filter");
                }
            }
        }
    }
}
