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
    exec_fd: Option<Fd>,
}

impl Prepared {
    /// What the child enters (blaze-sys `linux::confine::enter`).
    pub fn confine(&self) -> ChildConfine<'_> {
        ChildConfine {
            ruleset: self.ruleset.fd(),
            filter: self.filter.get(..self.filter_len).unwrap_or(&[]),
            limits: &self.limits,
            exec_fd: self.exec_fd,
        }
    }
}

/// The confinement of `plan`, with `budget`'s limits; `processes` lets the child start the
/// spec's helpers. When `exec_fd` is `Some`, the primary program's Landlock rule and NixOS ELF
/// closure use that exact opened descriptor and the child executes it via `execveat(...,
/// AT_EMPTY_PATH)`.
pub fn prepare(plan: &Plan<'_>, budget: Budget, processes: bool) -> Result<Prepared, Fault> {
    prepare_with_exec(plan, budget, processes, None)
}

pub(super) fn prepare_with_exec(plan: &Plan<'_>, budget: Budget, processes: bool, exec_fd: Option<Fd>) -> Result<Prepared, Fault> {
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
    let has_helper_folder = plan.rules().any(|rule| rule.why == Why::HelperFolder);
    let mut name = [0u8; 4096];
    for (at, rule) in plan.rules().enumerate() {
        let bytes = rule.path.as_bytes();
        if bytes.len() + 1 > name.len() {
            return Err(Fault::PathRefused { at: at as u32 });
        }
        name[..bytes.len()].copy_from_slice(bytes);
        name[bytes.len()] = 0;
        let path = CStr::from_bytes_with_nul(&name[..=bytes.len()]).map_err(|_| Fault::PathRefused { at: at as u32 })?;
        if rule.why == Why::Program
            && let Some(efd) = exec_fd
        {
            // `efd` is a kernel-sealed anonymous `memfd` executed via `execveat(..., AT_EMPTY_PATH)`:
            // it lives outside any mounted filesystem (so Landlock does not gate `AT_EMPTY_PATH`
            // exec on it and rejects `memfd` in `PathBeneath` with EBADFD). We therefore omit
            // granting `EXECUTE` on the on-disk program path and walk the Nix ELF closure directly
            // from the sealed `efd`.
            grant_nix_elf_closure(ruleset.fd(), path, Some(efd), plan, has_helper_folder)?;
            continue;
        }
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
            grant_nix_elf_closure(ruleset.fd(), path, None, plan, has_helper_folder)?;
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
    Ok(Prepared { ruleset, filter, filter_len, limits, exec_fd })
}

/// On NixOS hosts where `program_path`'s `PT_INTERP` symlinks into `/nix/store/`, walks the exact
/// same transitive ELF64 runtime closure as [`grant_nix_elf_closure`] (bounded by `cx` before
/// every open and 64 KiB read chunk) and feeds the canonical resolved path and file contents of
/// the ELF interpreter, delegated glibc `ld.so`, and every transitive shared library into `feed` so
/// callers can bind the host Nix runtime closure into content-addressed keys (AGENTS.md rule 21).
/// Returns `Ok(true)` when a NixOS closure was hashed, or `Ok(false)` on non-NixOS hosts or
/// non-ELF files.
pub fn feed_nix_runtime_closure(cx: &Cx, program_path: &str, feed: &mut dyn FnMut(&[u8])) -> Result<bool, Fault> {
    check_nix_cx(Some(cx))?;
    let bytes = program_path.as_bytes();
    if bytes.is_empty() || bytes.contains(&0) || bytes.len() >= 4096 {
        return Err(Fault::Host { call: "nix runtime closure (invalid program path)", code: 22 });
    }
    let mut name = [0u8; 4096];
    if let Some(dst) = name.get_mut(..bytes.len()) {
        dst.copy_from_slice(bytes);
    }
    let Ok(cstr) = CStr::from_bytes_until_nul(name.get(..=bytes.len()).unwrap_or(&[0])) else {
        return Err(Fault::Host { call: "nix runtime closure (invalid program path)", code: 22 });
    };
    let mut opt_feed = Some(feed);
    walk_nix_elf_closure(Some(cx), None, cstr, None, None, false, &mut opt_feed)
}

fn check_nix_cx(cx: Option<&Cx>) -> Result<(), Fault> {
    if let Some(cx) = cx {
        super::remaining_for_cx(cx)
            .map_err(|_| Fault::Host { call: "nix runtime closure (deadline/stopped)", code: 110 })?;
    }
    Ok(())
}

fn read_nix_at(
    reader: &Option<super::RootFdReader>,
    cx: Option<&Cx>,
    fd: Fd,
    out: &mut [u8],
    offset: u64,
    call: &'static str,
) -> Result<usize, Fault> {
    if let (Some(r), Some(cx)) = (reader.as_ref(), cx) {
        r.read_exact_or_eof(cx, fd, out, offset, call)
    } else {
        let mut filled = 0usize;
        while filled < out.len() {
            let step_off = u64::try_from(filled)
                .ok()
                .and_then(|d| offset.checked_add(d))
                .ok_or(Fault::Host { call, code: 75 })?;
            let Some(dst) = out.get_mut(filled..) else {
                break;
            };
            let n = pread(fd, dst, step_off).map_err(|errno| Fault::Host { call, code: errno.raw() })?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        Ok(filled)
    }
}

/// Ordered inherited `DT_RPATH` pool-index chain (current/newest ancestor first, oldest last),
/// preserving glibc ELF lookup order across arbitrary transitive dependency depth.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct RpathChain {
    dirs: [u8; 8],
    len: u8,
}

impl RpathChain {
    const EMPTY: Self = Self { dirs: [0; 8], len: 0 };

    fn as_slice(&self) -> &[u8] {
        self.dirs.get(..usize::from(self.len)).unwrap_or(&[])
    }

    fn push(&mut self, idx: u8) -> Result<(), Fault> {
        if self.as_slice().contains(&idx) {
            return Ok(());
        }
        let at = usize::from(self.len);
        let Some(slot) = self.dirs.get_mut(at) else {
            return Err(Fault::Host { call: "landlock (NixOS ELF inherited DT_RPATH chain exceeds cap)", code: 7 });
        };
        *slot = idx;
        self.len += 1;
        Ok(())
    }
}

/// On NixOS hosts where an ELF program's `PT_INTERP` (e.g. `/lib/ld-linux-aarch64.so.1`) symlinks
/// into `/nix/store/...` (`nix-ld`), walks the full transitive ELF64 dependency closure
/// (`DT_NEEDED`, ordered inherited `DT_RPATH` vs non-transitive `DT_RUNPATH` with `$ORIGIN`
/// expansion, `/run/current-system/sw/share/nix-ld/lib`, and `ld.so`'s resolved glibc `lib/`
/// directory) and grants Landlock single-file `READ_FILE | EXECUTE` ONLY on the loader and
/// `READ_FILE` ONLY on resolved shared library files — never on `/nix/store` as a directory and
/// never granting `EXECUTE` to shared libraries. Does nothing on non-NixOS hosts or non-ELF
/// scripts.
fn grant_nix_elf_closure(
    ruleset: Fd,
    program: &CStr,
    verified_exec_fd: Option<Fd>,
    plan: &Plan<'_>,
    has_helper_folder: bool,
) -> Result<(), Fault> {
    let mut no_feed: Option<&mut dyn FnMut(&[u8])> = None;
    let _ = walk_nix_elf_closure(None, Some(ruleset), program, verified_exec_fd, Some(plan), has_helper_folder, &mut no_feed)?;
    Ok(())
}

fn walk_nix_elf_closure(
    cx: Option<&Cx>,
    ruleset: Option<Fd>,
    program: &CStr,
    verified_exec_fd: Option<Fd>,
    plan: Option<&Plan<'_>>,
    has_helper_folder: bool,
    feed: &mut Option<&mut dyn FnMut(&[u8])>,
) -> Result<bool, Fault> {
    const MAX_LIBS: usize = 128;
    const PATH_CAP: usize = 320;
    const NIX_LD_DIR: &[u8] = b"/run/current-system/sw/share/nix-ld/lib/";
    const NIX_LD_SO: &CStr = c"/run/current-system/sw/share/nix-ld/lib/ld.so";

    check_nix_cx(cx)?;
    let reader = match cx {
        Some(cx) => Some(super::RootFdReader::new(cx)?),
        None => None,
    };

    let opened_prog = match verified_exec_fd {
        Some(_) => None,
        None => match openat(CWD, blaze_sys::Path::new(program), OpenFlags::CLOSE_ON_EXEC, 0) {
            Ok(fd) => Some(fd),
            Err(blaze_sys::Errno::NoEnt) if ruleset.is_none() => return Ok(false),
            Err(errno) if ruleset.is_none() => {
                return Err(Fault::Host { call: "openat (nix runtime closure program)", code: errno.raw() });
            }
            Err(_) => return Ok(false),
        },
    };
    let prog_fd = match (verified_exec_fd, &opened_prog) {
        (Some(fd), _) => fd,
        (None, Some(owned)) => owned.fd(),
        (None, None) => return Ok(false),
    };
    let Some(hdr) = parse_elf64_meta(&reader, cx, prog_fd)? else {
        return Ok(false);
    };
    if hdr.interp_len == 0 {
        return Ok(false);
    }
    let mut interp_buf = [0u8; 512];
    let Some(interp_slice) = interp_buf.get_mut(..hdr.interp_len) else {
        return Ok(false);
    };
    let n = read_nix_at(&reader, cx, prog_fd, interp_slice, hdr.interp_off, "pread (nix ELF PT_INTERP)")?;
    let Some(interp_bytes) = interp_buf.get(..n) else {
        return Ok(false);
    };
    let nul_pos = interp_bytes.iter().position(|&b| b == 0).unwrap_or(interp_bytes.len());
    let mut interp_c = [0u8; 513];
    if let (Some(dst), Some(src)) = (interp_c.get_mut(..nul_pos), interp_bytes.get(..nul_pos)) {
        dst.copy_from_slice(src);
    } else {
        return Ok(false);
    }
    let Ok(interp_cstr) = CStr::from_bytes_until_nul(&interp_c) else {
        return Ok(false);
    };
    // Only engage on NixOS where the ELF interpreter symlinks into `/nix/store/`.
    // Propagate filesystem/permission errors on `readlinkat` rather than treating them as
    // non-NixOS (only `EINVAL`, a regular non-symlink interpreter file, means non-symlink).
    let mut link_buf = [0u8; 512];
    let target = match readlinkat(CWD, blaze_sys::Path::new(interp_cstr), &mut link_buf) {
        Ok(target) => target,
        Err(blaze_sys::Errno::Inval) => return Ok(false),
        Err(errno) => {
            return Err(Fault::Host { call: "readlinkat (nix ELF PT_INTERP)", code: errno.raw() });
        }
    };
    if !target.starts_with(b"/nix/store/") {
        return Ok(false);
    }
    // Generated helpers inside `Why::HelperFolder` do not exist pre-exec so their transitive
    // Nix-store `DT_NEEDED` closure cannot be computed upfront; refuse explicitly rather than
    // granting `/nix/store` or letting generated helpers fail mid-run (rule 3).
    if has_helper_folder {
        return Err(Fault::Host {
            call: "landlock (NixOS minimal ELF closure does not support HelperFolder)",
            code: 38,
        });
    }

    // Grant single-file READ_FILE | EXECUTE ONLY on the resolved loader (`PT_INTERP`) and, when
    // the loader is NixOS's `nix-ld` shim, its delegated glibc `ld.so` (and feed both into any
    // caller hashing the runtime closure).
    let is_nix_ld = target.windows(6).any(|w| w == b"nix-ld");
    if let Some(rs) = ruleset {
        add_nix_loader_rule(rs, interp_cstr)?;
        if is_nix_ld {
            add_nix_loader_rule(rs, NIX_LD_SO)?;
        }
    }
    if let Some(f) = feed.as_deref_mut() {
        feed_nix_file_by_path(&reader, cx, interp_cstr, f)?;
        if is_nix_ld {
            feed_nix_file_by_path(&reader, cx, NIX_LD_SO, f)?;
        }
    }

    // Resolve `ld.so`'s target (`/nix/store/<hash>-glibc-.../lib/ld-linux-*.so.*`) when using
    // `nix-ld` so `DT_NEEDED` names from the ELF closure can resolve glibc libs without granting
    // any library not actually referenced by the ELF graph.
    let mut glibc_buf = [0u8; PATH_CAP];
    let glibc_dir: Option<&[u8]> = if is_nix_ld {
        resolve_symlink_path(NIX_LD_SO.to_bytes(), &mut glibc_buf).and_then(|len| {
            let resolved = glibc_buf.get(..len)?;
            let slash = resolved.iter().rposition(|&b| b == b'/')?;
            resolved.get(..=slash)
        })
    } else {
        None
    };

    // Bounded BFS queue of `(canonical ELF path, len, ordered inherited DT_RPATH chain)` seeded
    // ONLY with the executable itself, its `PT_INTERP`, and (for `nix-ld`) `NIX_LD_SO`.
    // For the primary program we always keep `program` (its shelf path) for `$ORIGIN` expansion
    // even when `verified_exec_fd` is an anonymous sealed `memfd`.
    let mut queue = [([0u8; PATH_CAP], 0usize, RpathChain::EMPTY); MAX_LIBS];
    let mut q_len = 0usize;
    let mut q_head = 0usize;
    let mut rpath_pool = [([0u8; PATH_CAP], 0usize); 32];
    let mut rpath_pool_len = 0usize;

    enqueue_nix_resolved_path(program.to_bytes(), RpathChain::EMPTY, &mut queue, &mut q_len)?;
    enqueue_nix_resolved_path(interp_cstr.to_bytes(), RpathChain::EMPTY, &mut queue, &mut q_len)?;
    if is_nix_ld {
        enqueue_nix_resolved_path(NIX_LD_SO.to_bytes(), RpathChain::EMPTY, &mut queue, &mut q_len)?;
    }

    while q_head < q_len {
        check_nix_cx(cx)?;
        let cur_idx = q_head;
        let mut cur_path = [0u8; PATH_CAP];
        let (cur_len, inherited_chain) = match queue.get(cur_idx) {
            Some((buf, len, chain)) => {
                if let (Some(d), Some(s)) = (cur_path.get_mut(..*len), buf.get(..*len)) {
                    d.copy_from_slice(s);
                }
                (*len, *chain)
            }
            None => break,
        };
        q_head += 1;
        let opened_cur = if cur_idx == 0 && verified_exec_fd.is_some() {
            None
        } else {
            let Ok(cur_cstr) = CStr::from_bytes_until_nul(cur_path.get(..=cur_len).unwrap_or(&[0])) else {
                return Err(Fault::Host { call: "landlock (NixOS ELF invalid queued path)", code: 22 });
            };
            Some(
                openat(CWD, blaze_sys::Path::new(cur_cstr), OpenFlags::CLOSE_ON_EXEC, 0)
                    .map_err(|errno| Fault::Host { call: "openat (nix ELF closure object)", code: errno.raw() })?,
            )
        };
        let elf_fd = match (cur_idx == 0, verified_exec_fd, &opened_cur) {
            (true, Some(fd), _) => fd,
            (_, _, Some(owned)) => owned.fd(),
            _ => continue,
        };
        let Some(meta) = parse_elf64_meta(&reader, cx, elf_fd)? else {
            continue;
        };
        if meta.dyn_len == 0 {
            continue;
        }
        let mut dyn_buf = [0u8; 8192];
        let Some(dyn_slice) = dyn_buf.get_mut(..meta.dyn_len) else {
            return Err(Fault::Host { call: "landlock (NixOS ELF PT_DYNAMIC exceeds cap)", code: 7 });
        };
        let dyn_read = read_nix_at(&reader, cx, elf_fd, dyn_slice, meta.dyn_off, "pread (nix ELF PT_DYNAMIC)")?;
        let mut strtab_vaddr = None;
        let mut runpath_off = None;
        let mut rpath_off = None;
        let mut needed_offs = [0u64; 128];
        let mut needed_count = 0usize;
        let mut pos = 0usize;
        while pos + 16 <= dyn_read {
            let tag = read_u64_le(&dyn_buf, pos);
            let val = read_u64_le(&dyn_buf, pos + 8);
            pos += 16;
            match tag {
                0 => break,                // DT_NULL
                1 => {                     // DT_NEEDED
                    let Some(slot) = needed_offs.get_mut(needed_count) else {
                        return Err(Fault::Host { call: "landlock (NixOS ELF DT_NEEDED exceeds cap)", code: 7 });
                    };
                    *slot = val;
                    needed_count += 1;
                }
                5 => strtab_vaddr = Some(val), // DT_STRTAB
                15 => rpath_off = Some(val),   // DT_RPATH
                29 => runpath_off = Some(val), // DT_RUNPATH
                _ => {}
            }
        }
        let Some(strtab_vaddr) = strtab_vaddr else {
            continue;
        };
        let Some(strtab_file_off) = meta.vaddr_to_offset(strtab_vaddr) else {
            continue;
        };

        // Resolve search directories with exact glibc ELF lookup order:
        // - `DT_RUNPATH` applies ONLY to direct dependencies of this object (non-transitive);
        //   when present, it overrides `DT_RPATH` for direct lookups while leaving ancestor
        //   `DT_RPATH` available to transitive children that lack `DT_RUNPATH`.
        // - `DT_RPATH` (when `DT_RUNPATH` is absent): THIS object's own `DT_RPATH` entries are
        //   searched FIRST (in colon-separated order), followed by inherited ancestor `DT_RPATH`
        //   entries (newest ancestor first), and this ordered chain is inherited by children.
        let origin_dir = cur_path
            .get(..cur_len)
            .and_then(|p| p.iter().rposition(|&b| b == b'/').and_then(|s| p.get(..s)))
            .unwrap_or(&[]);
        let mut search_dirs = [([0u8; PATH_CAP], 0usize); 16];
        let mut search_count = 0usize;
        let child_chain = if let Some(rp_off) = runpath_off {
            if let Some(at) = strtab_file_off.checked_add(rp_off) {
                let mut rp_buf = [0u8; 2048];
                let n = read_nix_at(&reader, cx, elf_fd, &mut rp_buf, at, "pread (nix ELF DT_RUNPATH)")?;
                let raw = rp_buf.get(..n).unwrap_or(&[]);
                let Some(end) = raw.iter().position(|&b| b == 0) else {
                    return Err(Fault::Host { call: "landlock (NixOS ELF RUNPATH exceeds buffer cap)", code: 7 });
                };
                for entry in raw.get(..end).unwrap_or(&[]).split(|&b| b == b':') {
                    if entry.is_empty() {
                        continue;
                    }
                    let Some(slot) = search_dirs.get_mut(search_count) else {
                        return Err(Fault::Host { call: "landlock (NixOS ELF RUNPATH entries exceed cap)", code: 7 });
                    };
                    let expanded_len = expand_origin(entry, origin_dir, &mut slot.0)?;
                    if expanded_len == 0 {
                        return Err(Fault::Host { call: "landlock (NixOS ELF RUNPATH entry exceeds path cap)", code: 36 });
                    }
                    slot.1 = expanded_len;
                    search_count += 1;
                }
            }
            inherited_chain
        } else {
            let mut eff_chain = RpathChain::EMPTY;
            if let Some(rp_off) = rpath_off
                && let Some(at) = strtab_file_off.checked_add(rp_off)
            {
                let mut rp_buf = [0u8; 2048];
                let n = read_nix_at(&reader, cx, elf_fd, &mut rp_buf, at, "pread (nix ELF DT_RPATH)")?;
                let raw = rp_buf.get(..n).unwrap_or(&[]);
                let Some(end) = raw.iter().position(|&b| b == 0) else {
                    return Err(Fault::Host { call: "landlock (NixOS ELF RPATH exceeds buffer cap)", code: 7 });
                };
                for entry in raw.get(..end).unwrap_or(&[]).split(|&b| b == b':') {
                    if entry.is_empty() {
                        continue;
                    }
                    let mut exp = [0u8; PATH_CAP];
                    let expanded_len = expand_origin(entry, origin_dir, &mut exp)?;
                    if expanded_len == 0 {
                        return Err(Fault::Host { call: "landlock (NixOS ELF RPATH entry exceeds path cap)", code: 36 });
                    }
                    let idx = intern_rpath_dir(exp.get(..expanded_len).unwrap_or(&[]), &mut rpath_pool, &mut rpath_pool_len)?;
                    eff_chain.push(idx)?;
                }
            }
            for &anc_idx in inherited_chain.as_slice() {
                eff_chain.push(anc_idx)?;
            }
            for &pool_idx in eff_chain.as_slice() {
                if let Some((src_buf, src_len)) = rpath_pool.get(usize::from(pool_idx))
                    && let Some(slot) = search_dirs.get_mut(search_count)
                {
                    if let (Some(d), Some(s)) = (slot.0.get_mut(..*src_len), src_buf.get(..*src_len)) {
                        d.copy_from_slice(s);
                        slot.1 = *src_len;
                        search_count += 1;
                    }
                }
            }
            eff_chain
        };

        let mut name_buf = [0u8; 320];
        for &str_off in needed_offs.get(..needed_count).unwrap_or(&[]) {
            let Some(at) = strtab_file_off.checked_add(str_off) else {
                continue;
            };
            let n = read_nix_at(&reader, cx, elf_fd, &mut name_buf, at, "pread (nix ELF DT_NEEDED)")?;
            let raw = name_buf.get(..n).unwrap_or(&[]);
            let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            let soname = raw.get(..end).unwrap_or(&[]);
            if soname.is_empty() {
                continue;
            }
            if soname.contains(&b'/') {
                let mut expanded = [0u8; PATH_CAP];
                let exp_len = expand_origin(soname, origin_dir, &mut expanded)?;
                let file_len = if exp_len > 1 && expanded.get(exp_len - 1) == Some(&b'/') {
                    exp_len - 1
                } else {
                    exp_len
                };
                let exp_slice = expanded.get(..file_len).unwrap_or(&[]);
                if exp_slice.first() != Some(&b'/') {
                    return Err(Fault::Host {
                        call: "landlock (NixOS ELF relative path-bearing DT_NEEDED unsupported)",
                        code: 2,
                    });
                }
                if !try_grant_and_enqueue_lib(
                    &reader,
                    cx,
                    ruleset,
                    b"",
                    exp_slice,
                    child_chain,
                    NIX_LD_DIR,
                    plan,
                    &mut queue,
                    &mut q_len,
                    feed,
                )? {
                    return Err(Fault::Host {
                        call: "landlock (NixOS ELF path-bearing DT_NEEDED not found)",
                        code: 2,
                    });
                }
            } else {
                enqueue_nix_soname(
                    &reader,
                    cx,
                    ruleset,
                    soname,
                    search_dirs.get(..search_count).unwrap_or(&[]),
                    child_chain,
                    NIX_LD_DIR,
                    glibc_dir,
                    plan,
                    &mut queue,
                    &mut q_len,
                    feed,
                )?;
            }
        }
    }
    Ok(true)
}

struct Elf64Meta {
    interp_off: u64,
    interp_len: usize,
    dyn_off: u64,
    dyn_len: usize,
    loads: [(u64, u64, u64); 16],
    load_count: usize,
}

impl Elf64Meta {
    fn vaddr_to_offset(&self, vaddr: u64) -> Option<u64> {
        self.loads.get(..self.load_count)?.iter().find_map(|&(pv, po, psz)| {
            let delta = vaddr.checked_sub(pv)?;
            (delta < psz).then(|| po.checked_add(delta))?
        })
    }
}

fn parse_elf64_meta(
    reader: &Option<super::RootFdReader>,
    cx: Option<&Cx>,
    fd: Fd,
) -> Result<Option<Elf64Meta>, Fault> {
    let mut ehdr = [0u8; 64];
    let ehdr_n = read_nix_at(reader, cx, fd, &mut ehdr, 0, "pread (nix ELF header)")?;
    if ehdr_n < 64 {
        return Ok(None);
    }
    if ehdr.get(..6) != Some(&[0x7f, b'E', b'L', b'F', 2, 1]) {
        return Ok(None);
    }
    let e_phoff = read_u64_le(&ehdr, 32);
    let e_phentsize = usize::from(read_u16_le(&ehdr, 54));
    let e_phnum = usize::from(read_u16_le(&ehdr, 56));
    if e_phentsize < 56 || e_phnum == 0 {
        return Ok(None);
    }
    if e_phnum > 64 {
        return Err(Fault::Host { call: "landlock (NixOS ELF program header count exceeds cap)", code: 7 });
    }
    let mut meta = Elf64Meta {
        interp_off: 0,
        interp_len: 0,
        dyn_off: 0,
        dyn_len: 0,
        loads: [(0, 0, 0); 16],
        load_count: 0,
    };
    let mut ph_buf = [0u8; 64];
    for i in 0..e_phnum {
        let Some(off) = u64::try_from(i)
            .ok()
            .and_then(|n| n.checked_mul(u64::try_from(e_phentsize).unwrap_or(0)))
            .and_then(|o| e_phoff.checked_add(o))
        else {
            return Ok(None);
        };
        let Some(ph_slice) = ph_buf.get_mut(..56) else {
            return Ok(None);
        };
        let ph_n = read_nix_at(reader, cx, fd, ph_slice, off, "pread (nix ELF program header)")?;
        if ph_n < 56 {
            break;
        }
        let p_type = read_u32_le(&ph_buf, 0);
        let p_offset = read_u64_le(&ph_buf, 8);
        let p_vaddr = read_u64_le(&ph_buf, 16);
        let p_filesz = read_u64_le(&ph_buf, 32);
        match p_type {
            1 => {
                let Some(slot) = meta.loads.get_mut(meta.load_count) else {
                    return Err(Fault::Host { call: "landlock (NixOS ELF PT_LOAD count exceeds cap)", code: 7 });
                };
                *slot = (p_vaddr, p_offset, p_filesz);
                meta.load_count += 1;
            }
            2 => {
                let Ok(len) = usize::try_from(p_filesz) else {
                    return Err(Fault::Host { call: "landlock (NixOS ELF PT_DYNAMIC exceeds cap)", code: 7 });
                };
                if len > 8192 {
                    return Err(Fault::Host { call: "landlock (NixOS ELF PT_DYNAMIC exceeds cap)", code: 7 });
                }
                meta.dyn_off = p_offset;
                meta.dyn_len = len;
            }
            3 => {
                meta.interp_off = p_offset;
                meta.interp_len = usize::try_from(p_filesz).unwrap_or(0).min(512);
            }
            _ => {}
        }
    }
    Ok(Some(meta))
}

fn expand_origin(entry: &[u8], origin_dir: &[u8], out: &mut [u8; 320]) -> Result<usize, Fault> {
    let mut len = 0usize;
    let mut i = 0usize;
    while i < entry.len() {
        let rest = entry.get(i..).unwrap_or(&[]);
        let (chunk, step) = if rest.starts_with(b"$ORIGIN") {
            (origin_dir, 7)
        } else if rest.starts_with(b"${ORIGIN}") {
            (origin_dir, 9)
        } else if rest.first() == Some(&b'$') {
            return Err(Fault::Host {
                call: "landlock (NixOS ELF unsupported dynamic-loader token in RUNPATH)",
                code: 38,
            });
        } else {
            (rest.get(..1).unwrap_or(&[]), 1)
        };
        if len + chunk.len() + 2 > out.len() {
            return Ok(0);
        }
        if let Some(dst) = out.get_mut(len..len + chunk.len()) {
            dst.copy_from_slice(chunk);
        }
        len += chunk.len();
        i += step;
    }
    if len > 0 && out.get(len - 1) != Some(&b'/') {
        if let Some(slot) = out.get_mut(len) {
            *slot = b'/';
            len += 1;
        }
    }
    Ok(len)
}

/// Resolves symlinks (up to 8 hops) for `path`, writing a NUL-terminated path into `out` and
/// returning its byte length (excluding NUL).
fn resolve_symlink_path(path: &[u8], out: &mut [u8; 320]) -> Option<usize> {
    if path.is_empty() || path.len() >= out.len() {
        return None;
    }
    out.fill(0);
    out.get_mut(..path.len())?.copy_from_slice(path);
    let mut cur_len = path.len();
    let mut link = [0u8; 320];
    for _ in 0..8 {
        let cstr = CStr::from_bytes_until_nul(out.get(..=cur_len)?).ok()?;
        let Ok(target) = readlinkat(CWD, blaze_sys::Path::new(cstr), &mut link) else {
            break;
        };
        if target.is_empty() {
            break;
        }
        let mut next = [0u8; 320];
        let next_len = if target.first() == Some(&b'/') {
            if target.len() >= next.len() {
                return None;
            }
            next.get_mut(..target.len())?.copy_from_slice(target);
            target.len()
        } else {
            let parent_end = out.get(..cur_len)?.iter().rposition(|&b| b == b'/').map_or(0, |s| s + 1);
            let total = parent_end.checked_add(target.len())?;
            if total >= next.len() {
                return None;
            }
            next.get_mut(..parent_end)?.copy_from_slice(out.get(..parent_end)?);
            next.get_mut(parent_end..total)?.copy_from_slice(target);
            total
        };
        out.copy_from_slice(&next);
        cur_len = next_len;
    }
    Some(cur_len)
}

/// Whether resolved path `path` is within an explicitly authorized root (`/nix/store/`,
/// `nix_ld_dir`, or a declared `plan` rule) and contains no `..` traversal components.
fn is_authorized_nix_dep_path(path: &[u8], nix_ld_dir: &[u8], plan: &Plan<'_>) -> bool {
    if path.first() != Some(&b'/') || path.split(|&b| b == b'/').any(|seg| seg == b"..") {
        return false;
    }
    if path.starts_with(b"/nix/store/") || path.starts_with(nix_ld_dir) {
        return true;
    }
    for rule in plan.rules() {
        let root = rule.path.as_bytes();
        if path == root {
            return true;
        }
        if rule.subtree && path.starts_with(root) {
            if root.ends_with(b"/") || path.get(root.len()) == Some(&b'/') {
                return true;
            }
        }
    }
    false
}

fn intern_rpath_dir(dir: &[u8], pool: &mut [([u8; 320], usize); 32], pool_len: &mut usize) -> Result<u8, Fault> {
    for idx in 0..*pool_len {
        if let Some((buf, len)) = pool.get(idx)
            && *len == dir.len()
            && buf.get(..*len) == Some(dir)
        {
            return u8::try_from(idx).map_err(|_| Fault::Host { call: "landlock (NixOS ELF RPATH pool index)", code: 7 });
        }
    }
    let idx = *pool_len;
    let Some(slot) = pool.get_mut(idx) else {
        return Err(Fault::Host { call: "landlock (NixOS ELF inherited DT_RPATH pool exceeds cap)", code: 7 });
    };
    let Some(dst) = slot.0.get_mut(..dir.len()) else {
        return Err(Fault::Host { call: "landlock (NixOS ELF RPATH entry exceeds path cap)", code: 36 });
    };
    dst.copy_from_slice(dir);
    slot.1 = dir.len();
    *pool_len += 1;
    u8::try_from(idx).map_err(|_| Fault::Host { call: "landlock (NixOS ELF RPATH pool index)", code: 7 })
}

fn enqueue_nix_resolved_path<const N: usize>(
    raw: &[u8],
    rpath_chain: RpathChain,
    queue: &mut [([u8; 320], usize, RpathChain); N],
    q_len: &mut usize,
) -> Result<(), Fault> {
    let mut resolved = [0u8; 320];
    let len = resolve_symlink_path(raw, &mut resolved).unwrap_or_else(|| {
        let l = raw.len().min(319);
        if let (Some(d), Some(s)) = (resolved.get_mut(..l), raw.get(..l)) {
            d.copy_from_slice(s);
            l
        } else {
            0
        }
    });
    if len == 0 {
        return Ok(());
    }
    let slice = resolved.get(..len).unwrap_or(&[]);
    enqueue_nix_canonical_slice(slice, rpath_chain, queue, q_len)
}

fn enqueue_nix_canonical_slice<const N: usize>(
    slice: &[u8],
    rpath_chain: RpathChain,
    queue: &mut [([u8; 320], usize, RpathChain); N],
    q_len: &mut usize,
) -> Result<(), Fault> {
    let seen_exact = queue
        .get(..*q_len)
        .is_some_and(|used| used.iter().any(|(b, l, c)| *l == slice.len() && b.get(..*l) == Some(slice) && *c == rpath_chain));
    if seen_exact {
        return Ok(());
    }
    let Some(slot) = queue.get_mut(*q_len) else {
        return Err(Fault::Host { call: "landlock (NixOS ELF transitive closure exceeds cap)", code: 7 });
    };
    let Some(dst) = slot.0.get_mut(..slice.len()) else {
        return Err(Fault::Host { call: "landlock (NixOS ELF library path exceeds cap)", code: 36 });
    };
    dst.copy_from_slice(slice);
    slot.1 = slice.len();
    slot.2 = rpath_chain;
    *q_len += 1;
    Ok(())
}

fn enqueue_nix_soname<const N: usize>(
    reader: &Option<super::RootFdReader>,
    cx: Option<&Cx>,
    ruleset: Option<Fd>,
    soname: &[u8],
    search_dirs: &[([u8; 320], usize)],
    child_chain: RpathChain,
    nix_ld_dir: &[u8],
    glibc_dir: Option<&[u8]>,
    plan: Option<&Plan<'_>>,
    queue: &mut [([u8; 320], usize, RpathChain); N],
    q_len: &mut usize,
    feed: &mut Option<&mut dyn FnMut(&[u8])>,
) -> Result<(), Fault> {
    for (dir_buf, dir_len) in search_dirs {
        let dir = dir_buf.get(..*dir_len).unwrap_or(&[]);
        if try_grant_and_enqueue_lib(reader, cx, ruleset, dir, soname, child_chain, nix_ld_dir, plan, queue, q_len, feed)? {
            return Ok(());
        }
    }
    if try_grant_and_enqueue_lib(reader, cx, ruleset, nix_ld_dir, soname, child_chain, nix_ld_dir, plan, queue, q_len, feed)? {
        return Ok(());
    }
    if let Some(dir) = glibc_dir {
        let _ = try_grant_and_enqueue_lib(reader, cx, ruleset, dir, soname, child_chain, nix_ld_dir, plan, queue, q_len, feed)?;
    }
    Ok(())
}

fn try_grant_and_enqueue_lib<const N: usize>(
    reader: &Option<super::RootFdReader>,
    cx: Option<&Cx>,
    ruleset: Option<Fd>,
    dir_prefix: &[u8],
    soname: &[u8],
    child_chain: RpathChain,
    nix_ld_dir: &[u8],
    plan: Option<&Plan<'_>>,
    queue: &mut [([u8; 320], usize, RpathChain); N],
    q_len: &mut usize,
    feed: &mut Option<&mut dyn FnMut(&[u8])>,
) -> Result<bool, Fault> {
    check_nix_cx(cx)?;
    let mut candidate = [0u8; 320];
    let total = dir_prefix.len().saturating_add(soname.len());
    if total + 1 > candidate.len() {
        return Ok(false);
    }
    if let Some(head) = candidate.get_mut(..dir_prefix.len()) {
        head.copy_from_slice(dir_prefix);
    }
    if let Some(tail) = candidate.get_mut(dir_prefix.len()..total) {
        tail.copy_from_slice(soname);
    }
    let Ok(cstr) = CStr::from_bytes_until_nul(candidate.get(..=total).unwrap_or(&[0])) else {
        return Ok(false);
    };
    let flags = if feed.is_some() {
        OpenFlags::CLOSE_ON_EXEC
    } else {
        OpenFlags::PATH.with(OpenFlags::CLOSE_ON_EXEC)
    };
    let fd = match openat(CWD, blaze_sys::Path::new(cstr), flags, 0) {
        Ok(fd) => fd,
        Err(blaze_sys::Errno::NoEnt) => return Ok(false),
        Err(errno) => return Err(Fault::Host { call: "openat (nix runtime file)", code: errno.raw() }),
    };
    // Ask the kernel for the canonical realpath of the ALREADY-OPENED fd via `/proc/self/fd/<fd>`,
    // resolving every intermediate directory symlink and preventing TOCTOU before authorizing the
    // inode for Landlock or hashing its content.
    let mut resolved = [0u8; 320];
    let res_len = readlink_fd_path(fd.fd(), &mut resolved)?;
    let res_slice = resolved.get(..res_len).unwrap_or(&[]);
    if let Some(p) = plan
        && !is_authorized_nix_dep_path(res_slice, nix_ld_dir, p)
    {
        return Err(Fault::Host {
            call: "landlock (NixOS ELF dependency resolved outside authorized roots)",
            code: 13,
        });
    }
    if let Some(rs) = ruleset {
        // Shared libraries need ONLY `READ_FILE` (never `EXECUTE`, which authorizes `execve`).
        let pb = PathBeneath { allowed_access: AccessFs::READ_FILE.bits(), parent_fd: fd.fd().raw() };
        landlock_add_rule_path(rs, &pb)
            .map_err(|errno| Fault::Host { call: "landlock_add_rule (nix runtime library)", code: errno.raw() })?;
    }
    let first_seen_path = !queue
        .get(..*q_len)
        .is_some_and(|used| used.iter().any(|(b, l, _)| *l == res_slice.len() && b.get(..*l) == Some(res_slice)));
    if first_seen_path && let Some(f) = feed.as_deref_mut() {
        feed_nix_fd(reader, cx, res_slice, fd.fd(), f)?;
    }
    enqueue_nix_canonical_slice(res_slice, child_chain, queue, q_len)?;
    Ok(true)
}

fn feed_nix_file_by_path(
    reader: &Option<super::RootFdReader>,
    cx: Option<&Cx>,
    path: &CStr,
    feed: &mut dyn FnMut(&[u8]),
) -> Result<(), Fault> {
    check_nix_cx(cx)?;
    let fd = openat(CWD, blaze_sys::Path::new(path), OpenFlags::CLOSE_ON_EXEC, 0)
        .map_err(|errno| Fault::Host { call: "openat (nix runtime loader)", code: errno.raw() })?;
    let mut resolved = [0u8; 320];
    let res_len = readlink_fd_path(fd.fd(), &mut resolved)?;
    feed_nix_fd(reader, cx, resolved.get(..res_len).unwrap_or(&[]), fd.fd(), feed)
}

fn feed_nix_fd(
    reader: &Option<super::RootFdReader>,
    cx: Option<&Cx>,
    canonical_path: &[u8],
    fd: Fd,
    feed: &mut dyn FnMut(&[u8]),
) -> Result<(), Fault> {
    let path_len = u32::try_from(canonical_path.len()).unwrap_or(u32::MAX);
    feed(&path_len.to_le_bytes());
    feed(canonical_path);
    let offset = if let (Some(r), Some(cx)) = (reader.as_ref(), cx) {
        r.stream_fd(cx, fd, "bstd::fs read (nix runtime closure hash)", |chunk| feed(chunk))?
    } else {
        let mut buf = [0u8; 64 << 10];
        let mut offset = 0u64;
        loop {
            check_nix_cx(cx)?;
            let n = pread(fd, &mut buf, offset)
                .map_err(|errno| Fault::Host { call: "pread (nix runtime closure hash)", code: errno.raw() })?;
            if n == 0 {
                break;
            }
            if let Some(chunk) = buf.get(..n) {
                feed(chunk);
            }
            let step = u64::try_from(n)
                .map_err(|_| Fault::Host { call: "pread (nix runtime closure overflow)", code: 75 })?;
            offset = offset
                .checked_add(step)
                .ok_or(Fault::Host { call: "pread (nix runtime closure overflow)", code: 75 })?;
        }
        offset
    };
    feed(&offset.to_le_bytes());
    Ok(())
}

/// Reads the kernel-canonicalized path of an opened descriptor via `/proc/self/fd/<fd>`, resolving
/// all intermediate symlinks atomically on the opened inode.
fn readlink_fd_path(fd: Fd, out: &mut [u8; 320]) -> Result<usize, Fault> {
    const PREFIX: &[u8] = b"/proc/self/fd/";
    let mut proc_buf = [0u8; 32];
    if let Some(dst) = proc_buf.get_mut(..PREFIX.len()) {
        dst.copy_from_slice(PREFIX);
    }
    let mut digits = [0u8; 12];
    let mut d_at = digits.len();
    let mut n = u32::try_from(fd.raw()).map_err(|_| Fault::Host { call: "readlinkat (/proc/self/fd)", code: 9 })?;
    loop {
        d_at -= 1;
        if let Some(slot) = digits.get_mut(d_at) {
            *slot = b'0' + u8::try_from(n % 10).unwrap_or(0);
        }
        n /= 10;
        if n == 0 {
            break;
        }
    }
    let d_len = digits.len() - d_at;
    let total = PREFIX.len() + d_len;
    if let (Some(dst), Some(src)) = (proc_buf.get_mut(PREFIX.len()..total), digits.get(d_at..)) {
        dst.copy_from_slice(src);
    }
    let Ok(proc_cstr) = CStr::from_bytes_until_nul(proc_buf.get(..=total).unwrap_or(&[0])) else {
        return Err(Fault::Host { call: "readlinkat (/proc/self/fd)", code: 22 });
    };
    let cap = out.len();
    let len = readlinkat(CWD, blaze_sys::Path::new(proc_cstr), out)
        .map_err(|errno| Fault::Host { call: "readlinkat (/proc/self/fd)", code: errno.raw() })?
        .len();
    if len >= cap {
        return Err(Fault::Host { call: "readlinkat (/proc/self/fd path exceeds cap)", code: 36 });
    }
    Ok(len)
}

fn add_nix_loader_rule(ruleset: Fd, path: &CStr) -> Result<(), Fault> {
    let flags = OpenFlags::PATH.with(OpenFlags::CLOSE_ON_EXEC);
    let fd = match openat(CWD, blaze_sys::Path::new(path), flags, 0) {
        Ok(fd) => fd,
        Err(blaze_sys::Errno::NoEnt) => return Ok(()),
        Err(errno) => return Err(Fault::Host { call: "openat (nix runtime loader, O_PATH)", code: errno.raw() }),
    };
    let granted = AccessFs::READ_FILE.with(AccessFs::EXECUTE).bits();
    let pb = PathBeneath { allowed_access: granted, parent_fd: fd.fd().raw() };
    landlock_add_rule_path(ruleset, &pb).map_err(|errno| Fault::Host { call: "landlock_add_rule (nix runtime loader)", code: errno.raw() })?;
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
        let prepared = prepare_with_exec(plan, spec.budget(), spec.starts_programs(), spec.verified_exec_fd())?;
        // `prepared` (the ruleset descriptor, the filter, the limits, and any verified exec fd)
        // lives until the start returns, by which time the child has exec'd (CLONE_VFORK) and
        // holds its own domain.
        spawner.start_confined(cx, &run_spec(path, spec.args(), start, streams), &prepared.confine()).map_err(Fault::Run)
    }

    /// Nothing is read while it runs: the kernel holds CPU (`RLIMIT_CPU`: `SIGXCPU`, a budget
    /// stop) and address space (`RLIMIT_AS`: the child's allocation fails).
    fn meter(&mut self, _pid: i32) -> Result<Option<Usage>, Fault> {
        Ok(None)
    }
}

#[cfg(test)]
mod nix_closure_tests {
    use super::{Fault, RpathChain, enqueue_nix_resolved_path};

    #[test]
    fn nix_elf_closure_queue_cap_refuses_instead_of_truncating() {
        let mut queue = [([0u8; 320], 0usize, RpathChain::EMPTY); 2];
        let mut q_len = 0usize;
        let mut chain = RpathChain::EMPTY;
        chain.push(2).expect("local rpath first");
        chain.push(0).expect("inherited rpath second");
        assert_eq!(chain.as_slice(), &[2, 0]);
        enqueue_nix_resolved_path(b"/nix/store/aaa-liba/lib/liba.so", chain, &mut queue, &mut q_len).expect("slot 0");
        enqueue_nix_resolved_path(b"/nix/store/bbb-libb/lib/libb.so", chain, &mut queue, &mut q_len).expect("slot 1");
        // Exact duplicate `(path, chain)` is deduplicated without consuming another slot.
        enqueue_nix_resolved_path(b"/nix/store/aaa-liba/lib/liba.so", chain, &mut queue, &mut q_len).expect("dup");
        assert_eq!(q_len, 2);
        // Distinct path past cap MUST fail with named Fault::Host rather than silently dropping.
        let err = enqueue_nix_resolved_path(b"/nix/store/ccc-libc/lib/libc.so", RpathChain::EMPTY, &mut queue, &mut q_len)
            .expect_err("must refuse when queue cap is exceeded");
        assert!(matches!(err, Fault::Host { code: 7, .. }));
    }
}
