//! [`Level::Inputs`](super::Level::Inputs) on Linux: Landlock, a seccomp filter and
//! no_new_privs (a thin driver over blaze-sys's wrappers).
//!
//! Made in the parent ([`prepare`]): a Landlock ruleset with one rule per plan rule (an
//! `O_PATH` descriptor of the path, the rights of its access: a file gets file rights only), a
//! seccomp filter (blaze-sys `linux::confine::build`: no socket of any family, no ptrace or
//! other process's memory, no mount table or namespace change, no bpf, perf, io_uring or
//! keyring; no new process unless the spec names a helper), and the resource limits (CPU
//! seconds and address space from the budget, no core file). Entered in the child before
//! `execve` by blaze-sys `linux::confine::enter`.
//!
//! Landlock's ABI: 3 or later is required (it handles truncation; older kernels would let a
//! confined child truncate a granted file it may only read); older is
//! [`Fault::LandlockAbi`] (rule 9: no weaker confinement in its place). ABI 4 adds TCP bind and
//! connect to the handled rights (none granted), ABI 6 scopes abstract unix sockets and signals
//! to the sandbox.
//!
//! The start: blaze-core's one linked start (`process/spawn.rs`, `clone(CLONE_VM|CLONE_VFORK)`)
//! carries the prepared confinement as data (`RunLink::confine`) and its child enters it last
//! before `execve` ([`Spawner::start_confined`]); a refused step is a typed error naming it, and
//! the program never runs. The budget is held, not metered: `RLIMIT_CPU` ends the child with
//! `SIGXCPU` (a budget stop), `RLIMIT_AS` fails its allocations.
//!
//! What this does not hold: the host's clock, metadata timing, the CPU's model (`cpuid` needs no
//! call; `/proc/cpuinfo` is outside the plan and refused, but `uname` and `sysinfo` are not
//! filtered), the kernel's version (`uname`), the pid, and randomness (`getrandom`).

use core::ffi::CStr;

use blaze_own::{Alloc, planted::NoPlant};
use blaze_sys::{
    linux::{
        AccessFs, AccessNet, CWD, OpenFlags, OwnedFd, PathBeneath, RulesetAttr, SockFilter,
        confine::{self, ChildConfine, Filter, MOST_FILTER, Resource, Rlimit},
        landlock_abi, landlock_add_rule_path, landlock_create_ruleset, openat,
    },
};

use super::{
    Access, Budget, Fault, HermeticError, HermeticRun, HermeticSpec, Plan, Program, RootFiles, StartView, Usage, Why,
    native::{Confiner, NativePort, run_spec},
    run_through,
};
use crate::{
    clock::Clock,
    cx::Cx,
    procs::{Run, Spawner, Started, Streams},
};

/// The loader and the system libraries: read whole (and the loader run: see `prepare`).
/// `/lib64` holds x86-64's loader (`/lib64/ld-linux-x86-64.so.2`); a path of the set the host
/// lacks is refused by name (`openat`), so each is one every supported host has.
#[cfg(target_arch = "x86_64")]
pub const SYSTEM: [&str; 4] = ["/usr/lib", "/lib", "/lib64", "/etc/ld.so.cache"];
#[cfg(not(target_arch = "x86_64"))]
pub const SYSTEM: [&str; 3] = ["/usr/lib", "/lib", "/etc/ld.so.cache"];

/// The oldest Landlock ABI that confines a child here.
pub const LEAST_ABI: u32 = 3;

/// `LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET | LANDLOCK_SCOPE_SIGNAL` (ABI 6).
const SCOPES: u64 = 0b11;

/// The rights every ruleset handles: everything of ABI 1, refer (2) and truncate (3).
fn handled() -> AccessFs {
    AccessFs::ALL_ABI1.with(AccessFs::REFER).with(AccessFs::TRUNCATE)
}

/// The rights a rule on a non-folder may carry (the kernel refuses folder rights on a file).
fn file_rights() -> AccessFs {
    AccessFs::EXECUTE.with(AccessFs::WRITE_FILE).with(AccessFs::READ_FILE).with(AccessFs::TRUNCATE)
}

fn rights(access: Access, subtree: bool) -> AccessFs {
    let rights = match access {
        Access::Read => AccessFs::READ_FILE.with(AccessFs::READ_DIR).bits(),
        // A folder of programs is listed too (the file rights drop READ_DIR on one file).
        Access::Exec => AccessFs::EXECUTE.with(AccessFs::READ_FILE).with(AccessFs::READ_DIR).bits(),
        // Everything handled but running a file and making devices.
        Access::Write => handled().bits() & !(AccessFs::EXECUTE.with(AccessFs::MAKE_CHAR).with(AccessFs::MAKE_BLOCK).bits()),
    };
    AccessFs::from_bits(if subtree { rights } else { rights & file_rights().bits() })
}

/// A child's confinement, made in the parent.
pub struct Prepared {
    ruleset: OwnedFd,
    filter: [SockFilter; MOST_FILTER],
    filter_len: usize,
    limits: [(Resource, Rlimit); confine::MOST_LIMITS],
}

impl Prepared {
    /// What the child enters (blaze-sys `linux::confine::enter`).
    pub fn confine(&self) -> ChildConfine<'_> {
        ChildConfine { ruleset: self.ruleset.fd(), filter: &self.filter[..self.filter_len], limits: &self.limits }
    }
}

/// The confinement of `plan`, with `budget`'s limits; `processes` lets the child start the
/// spec's helpers.
pub fn prepare(plan: &Plan<'_>, budget: Budget, processes: bool) -> Result<Prepared, Fault> {
    let abi = landlock_abi().map_err(|errno| Fault::Host { call: "landlock_create_ruleset (version)", code: errno.raw() })?;
    if abi < LEAST_ABI {
        return Err(Fault::LandlockAbi { abi, least: LEAST_ABI });
    }
    let attr = RulesetAttr {
        handled_access_fs: handled().bits(),
        handled_access_net: if abi >= 4 { AccessNet::BIND_TCP.with(AccessNet::CONNECT_TCP).bits() } else { 0 },
        scoped: if abi >= 6 { SCOPES } else { 0 },
    };
    let ruleset = landlock_create_ruleset(&attr).map_err(|errno| Fault::Host { call: "landlock_create_ruleset", code: errno.raw() })?;
    let mut name = [0u8; 4096];
    for (at, rule) in plan.rules().enumerate() {
        let bytes = rule.path.as_bytes();
        if bytes.len() + 1 > name.len() {
            return Err(Fault::PathRefused { at: at as u32 });
        }
        name[..bytes.len()].copy_from_slice(bytes);
        name[bytes.len()] = 0;
        let path = CStr::from_bytes_with_nul(&name[..=bytes.len()]).map_err(|_| Fault::PathRefused { at: at as u32 })?;
        // A folder rule is opened as a folder; a path that is a file (a whole-subtree rule on a
        // file, as `/etc/ld.so.cache`) is then a file rule: the kernel takes only file rights
        // on a file. The kind is asked of the kernel, never guessed from the spelling.
        let flags = OpenFlags::PATH.with(OpenFlags::CLOSE_ON_EXEC);
        let (fd, folder) = match openat(CWD, blaze_sys::Path::new(path), flags.with(OpenFlags::DIRECTORY), 0) {
            Ok(fd) => (fd, true),
            Err(blaze_sys::Errno::NotDir) => {
                let fd = openat(CWD, blaze_sys::Path::new(path), flags, 0)
                    .map_err(|errno| Fault::Host { call: "openat (a plan file, O_PATH)", code: errno.raw() })?;
                (fd, false)
            }
            Err(errno) => return Err(Fault::Host { call: "openat (a plan path, O_PATH)", code: errno.raw() }),
        };
        // The system set also gets EXECUTE: `execve` opens the ELF interpreter (the dynamic
        // loader under `/lib` or `/lib64`) for execution, which Landlock checks as EXECUTE.
        let mut granted = rights(rule.access, rule.subtree && folder).bits();
        if rule.why == Why::System {
            granted |= AccessFs::EXECUTE.bits();
        }
        let rule = PathBeneath { allowed_access: granted, parent_fd: fd.fd().raw() };
        landlock_add_rule_path(ruleset.fd(), &rule).map_err(|errno| Fault::Host { call: "landlock_add_rule", code: errno.raw() })?;
    }
    let mut filter = [SockFilter { code: 0, jt: 0, jf: 0, k: 0 }; MOST_FILTER];
    let filter_len = confine::build(Filter { processes }, &mut filter).len();
    let seconds = budget.cpu.as_secs() + u64::from(budget.cpu.subsec_nanos() > 0);
    let limits = [
        (Resource::Cpu, Rlimit { soft: seconds.max(1), hard: seconds.max(1) + 1 }),
        (Resource::AddressSpace, Rlimit { soft: budget.memory, hard: budget.memory }),
        (Resource::Core, Rlimit { soft: 0, hard: 0 }),
    ];
    Ok(Prepared { ruleset, filter, filter_len, limits })
}

/// `spec` run confined (module docs).
pub(super) fn run<'a>(
    spawner: &Spawner,
    clock: &Clock,
    cx: &Cx,
    spec: &'a HermeticSpec<'a>,
    files: &dyn RootFiles,
    alloc: Alloc<'a>,
) -> Result<HermeticRun<'a>, HermeticError<'a>> {
    let mut port = NativePort::new(spawner, clock, cx, alloc, Landlock);
    run_through(spec, &mut port, files, &SYSTEM, alloc, NoPlant)
}

/// The Linux confiner: the plan prepared here, entered by blaze-core's start in the child.
struct Landlock;

impl Confiner for Landlock {
    fn start(&mut self, spawner: &Spawner, cx: &Cx, start: &StartView<'_>, streams: Streams, _alloc: Alloc<'_>) -> Result<Started<Run>, Fault> {
        let spec = start.spec;
        let plan = start.plan()?;
        let Program::Stored { path, .. } = spec.program() else {
            return Err(Fault::RecordsNeedExecution);
        };
        let prepared = prepare(plan, spec.budget(), spec.starts_programs())?;
        // `prepared` (the ruleset descriptor, the filter, the limits) lives until the start
        // returns, by which time the child has exec'd (CLONE_VFORK) and holds its own domain.
        spawner.start_confined(cx, &run_spec(path, spec.args(), start, streams), &prepared.confine()).map_err(Fault::Run)
    }

    /// Nothing is read while it runs: the kernel holds CPU (`RLIMIT_CPU`: `SIGXCPU`, a budget
    /// stop) and address space (`RLIMIT_AS`: the child's allocation fails).
    fn meter(&mut self, _pid: i32) -> Result<Option<Usage>, Fault> {
        Ok(None)
    }
}
