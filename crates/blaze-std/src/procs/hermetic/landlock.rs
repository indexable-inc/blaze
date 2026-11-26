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
    Fd,
    linux::{
        AccessFs, AccessNet, CWD, OpenFlags, OwnedFd, PathBeneath, RulesetAttr, SockFilter,
        confine::{self, ChildConfine, Filter, MOST_FILTER, Resource, Rlimit},
        landlock_abi, landlock_add_rule_path, landlock_create_ruleset, openat, pread, readlinkat,
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
/// `/lib64` holds x86-64's loader (`/lib64/ld-linux-x86-64.so.2`).
/// Missing optional system paths are skipped in [`prepare`] (`Errno::NoEnt` on `Why::System`
/// rules only); every user root and program path is still strictly required.
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
            Err(blaze_sys::Errno::NotDir) => match openat(CWD, blaze_sys::Path::new(path), flags, 0) {
                Ok(fd) => (fd, false),
                Err(blaze_sys::Errno::NoEnt) if rule.why == Why::System => continue,
                Err(errno) => return Err(Fault::Host { call: "openat (a plan file, O_PATH)", code: errno.raw() }),
            },
            Err(blaze_sys::Errno::NoEnt) if rule.why == Why::System => continue,
            Err(errno) => return Err(Fault::Host { call: "openat (a plan path, O_PATH)", code: errno.raw() }),
        };
        // The system set also gets EXECUTE: `execve` opens the ELF interpreter (the dynamic
        // loader under `/lib` or `/lib64`) for execution, which Landlock checks as EXECUTE.
        let mut granted = rights(rule.access, rule.subtree && folder).bits();
        if rule.why == Why::System {
            granted |= AccessFs::EXECUTE.bits();
        }
        let pb = PathBeneath { allowed_access: granted, parent_fd: fd.fd().raw() };
        landlock_add_rule_path(ruleset.fd(), &pb).map_err(|errno| Fault::Host { call: "landlock_add_rule", code: errno.raw() })?;
        if matches!(rule.why, Why::Program | Why::Helper) {
            grant_nix_elf_closure(ruleset.fd(), path)?;
        }
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

/// On NixOS hosts where an ELF program's `PT_INTERP` (e.g. `/lib/ld-linux-aarch64.so.1`) symlinks
/// into `/nix/store/...` (`nix-ld`), grants Landlock single-file `READ_FILE | EXECUTE` ONLY on the
/// exact loader binary and the shared libraries referenced by that binary's `DT_NEEDED` entries
/// (plus core libc/libm/libgcc_s/ld.so under `/run/current-system/sw/share/nix-ld/lib`), never on
/// `/nix/store` as a directory. Does nothing on non-NixOS hosts or non-ELF scripts.
fn grant_nix_elf_closure(ruleset: Fd, program: &CStr) -> Result<(), Fault> {
    let Ok(file) = openat(CWD, blaze_sys::Path::new(program), OpenFlags::CLOSE_ON_EXEC, 0) else {
        return Ok(());
    };
    let mut ehdr = [0u8; 64];
    if pread(file.fd(), &mut ehdr, 0).unwrap_or(0) < 64 {
        return Ok(());
    }
    // ELF64 little-endian magic check.
    if ehdr.get(..6) != Some(&[0x7f, b'E', b'L', b'F', 2, 1]) {
        return Ok(());
    }
    let e_phoff = read_u64_le(&ehdr, 32);
    let e_phentsize = usize::from(read_u16_le(&ehdr, 54));
    let e_phnum = usize::from(read_u16_le(&ehdr, 56));
    if e_phentsize < 56 || e_phnum == 0 || e_phnum > 64 {
        return Ok(());
    }
    let mut interp_off = 0u64;
    let mut interp_len = 0usize;
    let mut dyn_off = 0u64;
    let mut dyn_len = 0usize;
    // Track up to 16 PT_LOAD segments to map DT_STRTAB vaddr -> file offset.
    let mut loads = [(0u64, 0u64, 0u64); 16]; // (p_vaddr, p_offset, p_filesz)
    let mut load_count = 0usize;
    let mut ph_buf = [0u8; 64];
    for i in 0..e_phnum {
        let Some(off) = u64::try_from(i).ok().and_then(|n| n.checked_mul(u64::try_from(e_phentsize).unwrap_or(0))).and_then(|o| e_phoff.checked_add(o)) else {
            break;
        };
        if pread(file.fd(), &mut ph_buf[..56], off).unwrap_or(0) < 56 {
            break;
        }
        let p_type = read_u32_le(&ph_buf, 0);
        let p_offset = read_u64_le(&ph_buf, 8);
        let p_vaddr = read_u64_le(&ph_buf, 16);
        let p_filesz = read_u64_le(&ph_buf, 32);
        match p_type {
            1 => {
                // PT_LOAD
                if let Some(slot) = loads.get_mut(load_count) {
                    *slot = (p_vaddr, p_offset, p_filesz);
                    load_count += 1;
                }
            }
            3 => {
                // PT_INTERP
                interp_off = p_offset;
                interp_len = usize::try_from(p_filesz).unwrap_or(0).min(512);
            }
            2 => {
                // PT_DYNAMIC
                dyn_off = p_offset;
                dyn_len = usize::try_from(p_filesz).unwrap_or(0).min(8192);
            }
            _ => {}
        }
    }
    if interp_len == 0 {
        return Ok(());
    }
    let mut interp_buf = [0u8; 512];
    let Some(interp_slice) = interp_buf.get_mut(..interp_len) else {
        return Ok(());
    };
    let n = pread(file.fd(), interp_slice, interp_off).unwrap_or(0);
    let Some(interp_bytes) = interp_buf.get(..n) else {
        return Ok(());
    };
    let nul_pos = interp_bytes.iter().position(|&b| b == 0).unwrap_or(interp_bytes.len());
    let mut interp_c = [0u8; 513];
    let Some(dst) = interp_c.get_mut(..nul_pos) else {
        return Ok(());
    };
    let Some(src) = interp_bytes.get(..nul_pos) else {
        return Ok(());
    };
    dst.copy_from_slice(src);
    let Ok(interp_cstr) = CStr::from_bytes_until_nul(&interp_c) else {
        return Ok(());
    };
    // Only engage on NixOS where the ELF interpreter symlinks into `/nix/store/`.
    let mut link_buf = [0u8; 512];
    let Ok(target) = readlinkat(CWD, blaze_sys::Path::new(interp_cstr), &mut link_buf) else {
        return Ok(());
    };
    if !target.starts_with(b"/nix/store/") {
        return Ok(());
    }
    // Grant single-file READ_FILE | EXECUTE on the resolved loader (`nix-ld`) and glibc `ld.so`.
    add_nix_file_rule(ruleset, interp_cstr)?;
    const NIX_LD_DIR: &[u8] = b"/run/current-system/sw/share/nix-ld/lib/";
    const NIX_LD_SO: &CStr = c"/run/current-system/sw/share/nix-ld/lib/ld.so";
    add_nix_file_rule(ruleset, NIX_LD_SO)?;
    // Resolve `ld.so`'s target (`/nix/store/<hash>-glibc-.../lib/ld-linux-*.so.*`) to find the
    // exact glibc `lib/` prefix where `libc.so.6` and `libm.so.6` live (never granting `/nix/store`).
    let mut glibc_buf = [0u8; 512];
    let glibc_dir: Option<&[u8]> = readlinkat(CWD, blaze_sys::Path::new(NIX_LD_SO), &mut glibc_buf)
        .ok()
        .and_then(|resolved| {
            let slash = resolved.iter().rposition(|&b| b == b'/')?;
            resolved.get(..=slash)
        });
    for base in [b"libc.so.6".as_slice(), b"libm.so.6", b"libgcc_s.so.1", b"libpthread.so.0", b"libdl.so.2", b"librt.so.1"] {
        add_nix_lib_rule(ruleset, NIX_LD_DIR, base)?;
        if let Some(dir) = glibc_dir {
            add_nix_lib_rule(ruleset, dir, base)?;
        }
    }
    if dyn_len == 0 {
        return Ok(());
    }
    // Read PT_DYNAMIC to locate DT_STRTAB and DT_NEEDED sonames.
    let mut dyn_buf = [0u8; 8192];
    let Some(dyn_slice) = dyn_buf.get_mut(..dyn_len) else {
        return Ok(());
    };
    let dyn_read = pread(file.fd(), dyn_slice, dyn_off).unwrap_or(0);
    let mut strtab_vaddr = None;
    let mut needed_offs = [0u64; 64];
    let mut needed_count = 0usize;
    let mut pos = 0usize;
    while pos + 16 <= dyn_read {
        let tag = read_u64_le(&dyn_buf, pos);
        let val = read_u64_le(&dyn_buf, pos + 8);
        pos += 16;
        if tag == 0 {
            break; // DT_NULL
        } else if tag == 5 {
            strtab_vaddr = Some(val); // DT_STRTAB
        } else if tag == 1 {
            // DT_NEEDED
            if let Some(slot) = needed_offs.get_mut(needed_count) {
                *slot = val;
                needed_count += 1;
            }
        }
    }
    let Some(strtab_vaddr) = strtab_vaddr else {
        return Ok(());
    };
    let strtab_file_off = loads.get(..load_count).and_then(|ls| {
        ls.iter().find_map(|&(vaddr, foff, filesz)| {
            let delta = strtab_vaddr.checked_sub(vaddr)?;
            (delta < filesz).then(|| foff.checked_add(delta))?
        })
    });
    let Some(strtab_file_off) = strtab_file_off else {
        return Ok(());
    };
    let mut name_buf = [0u8; 192];
    for &str_off in needed_offs.get(..needed_count).unwrap_or(&[]) {
        let Some(at) = strtab_file_off.checked_add(str_off) else {
            continue;
        };
        let n = pread(file.fd(), &mut name_buf, at).unwrap_or(0);
        let Some(raw) = name_buf.get(..n) else {
            continue;
        };
        let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
        let Some(soname) = raw.get(..end) else {
            continue;
        };
        if !soname.is_empty() && !soname.contains(&b'/') {
            add_nix_lib_rule(ruleset, NIX_LD_DIR, soname)?;
            if let Some(dir) = glibc_dir {
                add_nix_lib_rule(ruleset, dir, soname)?;
            }
        }
    }
    Ok(())
}

fn add_nix_lib_rule(ruleset: Fd, dir_prefix: &[u8], soname: &[u8]) -> Result<(), Fault> {
    let mut path_buf = [0u8; 256];
    let total = dir_prefix.len().saturating_add(soname.len());
    if total + 1 > path_buf.len() {
        return Ok(());
    }
    if let Some(head) = path_buf.get_mut(..dir_prefix.len()) {
        head.copy_from_slice(dir_prefix);
    }
    if let Some(tail) = path_buf.get_mut(dir_prefix.len()..total) {
        tail.copy_from_slice(soname);
    }
    if let Ok(cstr) = CStr::from_bytes_until_nul(&path_buf) {
        add_nix_file_rule(ruleset, cstr)?;
    }
    Ok(())
}

fn add_nix_file_rule(ruleset: Fd, path: &CStr) -> Result<(), Fault> {
    let flags = OpenFlags::PATH.with(OpenFlags::CLOSE_ON_EXEC);
    let fd = match openat(CWD, blaze_sys::Path::new(path), flags, 0) {
        Ok(fd) => fd,
        Err(blaze_sys::Errno::NoEnt) => return Ok(()),
        Err(errno) => return Err(Fault::Host { call: "openat (nix runtime file, O_PATH)", code: errno.raw() }),
    };
    let granted = AccessFs::READ_FILE.with(AccessFs::EXECUTE).bits();
    let pb = PathBeneath { allowed_access: granted, parent_fd: fd.fd().raw() };
    landlock_add_rule_path(ruleset, &pb).map_err(|errno| Fault::Host { call: "landlock_add_rule (nix runtime file)", code: errno.raw() })?;
    Ok(())
}

fn read_u16_le(buf: &[u8], at: usize) -> u16 {
    buf.get(at..at + 2).and_then(|s| s.try_into().ok()).map_or(0, u16::from_le_bytes)
}

fn read_u32_le(buf: &[u8], at: usize) -> u32 {
    buf.get(at..at + 4).and_then(|s| s.try_into().ok()).map_or(0, u32::from_le_bytes)
}

fn read_u64_le(buf: &[u8], at: usize) -> u64 {
    buf.get(at..at + 8).and_then(|s| s.try_into().ok()).map_or(0, u64::from_le_bytes)
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
