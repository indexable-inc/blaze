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

use blaze_own::{AVec, Alloc, planted::NoPlant};
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
    prepare_with_exec(plan, budget, processes, None, None)
}

pub(super) fn prepare_with_exec(
    plan: &Plan<'_>,
    budget: Budget,
    processes: bool,
    exec_fd: Option<Fd>,
    alloc: Option<Alloc<'_>>,
) -> Result<Prepared, Fault> {
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
            grant_nix_elf_closure(ruleset.fd(), path, Some(efd), plan, has_helper_folder, alloc)?;
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
            grant_nix_elf_closure(ruleset.fd(), path, None, plan, has_helper_folder, alloc)?;
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

/// On Linux hosts (NixOS and standard FHS), walks the exact transitive ELF64 runtime closure of
/// `program_path` (bounded by `cx` before every open and 64 KiB read chunk) and feeds the
/// canonical resolved path and file contents of the ELF interpreter, delegated glibc `ld.so` or
/// `/etc/ld.so.cache`, and every transitive shared library into `feed` so callers can bind the
/// host runtime closure into content-addressed keys (AGENTS.md rule 21).
/// Returns `Ok(true)` when a runtime closure was hashed, or `Ok(false)` on non-ELF files.
pub fn feed_nix_runtime_closure(
    cx: &Cx,
    program_path: &str,
    alloc: Alloc<'_>,
    feed: &mut dyn FnMut(&[u8]),
) -> Result<bool, Fault> {
    let reader = super::RootReader::new(cx)?;
    feed_nix_runtime_closure_with(&reader, cx, program_path, alloc, feed)
}

/// Same as [`feed_nix_runtime_closure`], reusing a caller-owned [`super::RootReader`] so
/// cancelled/stalled reads never allocate or leak a fresh async driver per probe.
pub fn feed_nix_runtime_closure_with(
    reader: &super::RootReader,
    cx: &Cx,
    program_path: &str,
    alloc: Alloc<'_>,
    feed: &mut dyn FnMut(&[u8]),
) -> Result<bool, Fault> {
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
    // `HermeticClangProbe` always launches the verified binary via anonymous sealed `memfd`
    // (`execveat(..., AT_EMPTY_PATH)`), so model the exact memfd `$ORIGIN` (`"/"`).
    walk_nix_elf_closure(Some(reader), Some(cx), Some(alloc), None, cstr, None, true, None, false, &mut opt_feed)
}

fn check_nix_cx(cx: Option<&Cx>) -> Result<(), Fault> {
    if let Some(cx) = cx {
        super::remaining_for_cx(cx)
            .map_err(|_| Fault::Host { call: "nix runtime closure (deadline/stopped)", code: 110 })?;
    }
    Ok(())
}

fn read_nix_at(
    reader: Option<&super::RootReader>,
    cx: Option<&Cx>,
    fd: Fd,
    out: &mut [u8],
    offset: u64,
    call: &'static str,
) -> Result<usize, Fault> {
    if let (Some(r), Some(cx)) = (reader, cx) {
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
    alloc: Option<Alloc<'_>>,
) -> Result<(), Fault> {
    let mut no_feed: Option<&mut dyn FnMut(&[u8])> = None;
    let memfd_origin = verified_exec_fd.is_some();
    let _ = walk_nix_elf_closure(None, None, alloc, Some(ruleset), program, verified_exec_fd, memfd_origin, Some(plan), has_helper_folder, &mut no_feed)?;
    Ok(())
}

/// Maximum size of `/etc/ld.so.cache` buffered for SONAME resolution on non-Nix Linux (4 MiB).
const MAX_LD_CACHE_BYTES: usize = 4 << 20;

/// Expands and validates a single colon-separated `DT_RPATH` or `DT_RUNPATH` entry:
/// - Rejects empty entries (e.g. `:/pinned/lib` or `/a::/b`), which glibc interprets as cwd `.`.
/// - Expands `$ORIGIN`/`$LIB`/`$PLATFORM` (and braced forms) against `origin_dir`.
/// - Rejects any expanded relative path (not starting with `/`) so cwd-dependent library search
///   can never diverge from the hashed and Landlock-admitted closure.
pub fn validate_rpath_entry(
    raw_entry: &[u8],
    origin_dir: &[u8],
    lib_token: &[u8],
    platform_token: &[u8],
    out: &mut [u8; 320],
) -> Result<usize, Fault> {
    if raw_entry.is_empty() {
        return Err(Fault::Host { call: "landlock (ELF RPATH/RUNPATH contains empty cwd component)", code: 22 });
    }
    let expanded_len = expand_rpath_tokens(raw_entry, origin_dir, lib_token, platform_token, out)?;
    if expanded_len == 0 {
        return Err(Fault::Host { call: "landlock (ELF RPATH/RUNPATH entry exceeds path cap)", code: 36 });
    }
    if out.first() != Some(&b'/') {
        return Err(Fault::Host { call: "landlock (ELF RPATH/RUNPATH contains relative cwd component)", code: 22 });
    }
    Ok(expanded_len)
}

#[expect(clippy::too_many_arguments, reason = "internal ELF closure walker shared between Landlock admission and strategy hashing")]
fn walk_nix_elf_closure(
    reader: Option<&super::RootReader>,
    cx: Option<&Cx>,
    alloc: Option<Alloc<'_>>,
    ruleset: Option<Fd>,
    program: &CStr,
    verified_exec_fd: Option<Fd>,
    memfd_origin: bool,
    plan: Option<&Plan<'_>>,
    has_helper_folder: bool,
    feed: &mut Option<&mut dyn FnMut(&[u8])>,
) -> Result<bool, Fault> {
    const MAX_LIBS: usize = 128;
    const PATH_CAP: usize = 320;
    const NIX_LD_DIR: &[u8] = b"/run/current-system/sw/share/nix-ld/lib/";
    const NIX_LD_SO: &CStr = c"/run/current-system/sw/share/nix-ld/lib/ld.so";

    check_nix_cx(cx)?;

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
    let Some(hdr) = parse_elf64_meta(reader, cx, prog_fd)? else {
        return Ok(false);
    };
    if hdr.interp_len == 0 {
        return Ok(false);
    }
    let mut interp_buf = [0u8; 512];
    let Some(interp_slice) = interp_buf.get_mut(..hdr.interp_len) else {
        return Ok(false);
    };
    let n = read_nix_at(reader, cx, prog_fd, interp_slice, hdr.interp_off, "pread (nix ELF PT_INTERP)")?;
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
    // Check whether the ELF interpreter symlinks into `/nix/store/` (NixOS).
    // Propagate filesystem/permission errors on `readlinkat` rather than treating them as
    // non-NixOS (only `EINVAL`, a regular non-symlink interpreter file, means non-symlink).
    let mut link_buf = [0u8; 512];
    let (is_nix, is_nix_ld) = match readlinkat(CWD, blaze_sys::Path::new(interp_cstr), &mut link_buf) {
        Ok(target) => {
            let nix = target.starts_with(b"/nix/store/");
            let nix_ld = nix && target.windows(6).any(|w| w == b"nix-ld");
            (nix, nix_ld)
        }
        Err(blaze_sys::Errno::Inval) => (false, false),
        Err(errno) => {
            return Err(Fault::Host { call: "readlinkat (nix ELF PT_INTERP)", code: errno.raw() });
        }
    };
    // Generated helpers inside `Why::HelperFolder` do not exist pre-exec so their transitive
    // Nix-store `DT_NEEDED` closure cannot be computed upfront; refuse explicitly rather than
    // granting `/nix/store` or letting generated helpers fail mid-run (rule 3).
    if is_nix && has_helper_folder {
        return Err(Fault::Host {
            call: "landlock (NixOS minimal ELF closure does not support HelperFolder)",
            code: 38,
        });
    }

    // Refuse Linux hosts with a non-empty `/etc/ld.so.preload` (rule 3/21): the dynamic loader
    // preloads those DSOs into every process before `main`, which would either be denied by
    // Landlock or alter compiler behavior outside the keyed ELF closure. Stream through EOF up to
    // `MAX_PRELOAD_SCAN` bytes so entries past byte 4096 (after long comment headers) are never
    // truncated or misclassified as empty.
    match openat(CWD, blaze_sys::Path::new(c"/etc/ld.so.preload"), OpenFlags::CLOSE_ON_EXEC, 0) {
        Ok(preload_fd) => {
            const MAX_PRELOAD_SCAN: u64 = 64 << 10;
            let mut preload_buf = [0u8; 4096];
            let mut off = 0u64;
            let mut scanner = PreloadScanner::new();
            loop {
                let n = read_nix_at(reader, cx, preload_fd.fd(), &mut preload_buf, off, "pread (/etc/ld.so.preload)")?;
                if n == 0 {
                    break;
                }
                if scanner.feed(preload_buf.get(..n).unwrap_or(&[])) {
                    return Err(Fault::Host {
                        call: "landlock (/etc/ld.so.preload unsupported for hermetic probes)",
                        code: 38,
                    });
                }
                let step = u64::try_from(n).unwrap_or(0);
                off = off.saturating_add(step);
                if off >= MAX_PRELOAD_SCAN {
                    return Err(Fault::Host {
                        call: "landlock (/etc/ld.so.preload exceeds bounded scan limit)",
                        code: 38,
                    });
                }
            }
            if let Some(f) = feed.as_deref_mut() {
                f(b"/etc/ld.so.preload:empty");
            }
        }
        Err(blaze_sys::Errno::NoEnt) => {}
        Err(errno) => {
            return Err(Fault::Host { call: "openat (/etc/ld.so.preload)", code: errno.raw() });
        }
    }

    // Grant single-file READ_FILE | EXECUTE ONLY on the resolved loader (`PT_INTERP`) and, when
    // the loader is NixOS's `nix-ld` shim, its delegated glibc `ld.so` (and feed both — plus
    // `/etc/ld.so.cache` on non-Nix FHS Linux — into any caller hashing the runtime closure).
    // On non-Nix FHS Linux, BOTH confinement (`ruleset.is_some()`) and closure hashing
    // (`feed.is_some()`) also load `/etc/ld.so.cache` so custom configured library directories
    // (e.g. `/opt/vendor` or `glibc-hwcaps`) are both hashed AND granted `READ_FILE` in Landlock.
    if is_nix && let Some(rs) = ruleset {
        add_nix_loader_rule(rs, interp_cstr)?;
        if is_nix_ld {
            add_nix_loader_rule(rs, NIX_LD_SO)?;
        }
    }
    let mut ld_cache_bytes: Option<AVec<'_, u8>> = None;
    if let Some(f) = feed.as_deref_mut() {
        feed_nix_file_by_path(reader, cx, interp_cstr, f)?;
        if is_nix_ld {
            feed_nix_file_by_path(reader, cx, NIX_LD_SO, f)?;
        }
        if !is_nix {
            match openat(CWD, blaze_sys::Path::new(c"/etc/ld.so.cache"), OpenFlags::CLOSE_ON_EXEC, 0) {
                Ok(cache_fd) => {
                    if let Some(a) = alloc {
                        let framing = 4 + b"/etc/ld.so.cache".len() + 8;
                        let mut captured = AVec::new_in(a);
                        let mut overflow = false;
                        feed_nix_fd(reader, cx, b"/etc/ld.so.cache", cache_fd.fd(), &mut |chunk| {
                            f(chunk);
                            if captured.len().saturating_add(chunk.len()) <= MAX_LD_CACHE_BYTES + framing {
                                captured.extend_from_slice(chunk);
                            } else {
                                overflow = true;
                            }
                        })?;
                        // `feed_nix_fd` frames the stream with a 4-byte path len + 16-byte path
                        // prefix and an 8-byte total-length trailer; strip those framing bytes so
                        // `captured` holds ONLY the exact raw `/etc/ld.so.cache` file contents.
                        if overflow {
                            return Err(Fault::Host { call: "read (/etc/ld.so.cache exceeds 4 MiB cap)", code: 27 });
                        }
                        let prefix = 4 + b"/etc/ld.so.cache".len();
                        let trailer = 8;
                        if captured.len() >= prefix + trailer {
                            let body_len = captured.len() - prefix - trailer;
                            let mut raw = AVec::with_capacity_in(body_len, a);
                            if let Some(body) = captured.as_slice().get(prefix..prefix + body_len) {
                                raw.extend_from_slice(body);
                            }
                            ld_cache_bytes = Some(raw);
                        }
                    } else {
                        feed_nix_fd(reader, cx, b"/etc/ld.so.cache", cache_fd.fd(), f)?;
                    }
                }
                Err(blaze_sys::Errno::NoEnt) => {}
                Err(errno) => {
                    return Err(Fault::Host { call: "openat (/etc/ld.so.cache)", code: errno.raw() });
                }
            }
        }
    } else if !is_nix
        && let Some(a) = alloc
    {
        match openat(CWD, blaze_sys::Path::new(c"/etc/ld.so.cache"), OpenFlags::CLOSE_ON_EXEC, 0) {
            Ok(cache_fd) => {
                let mut raw = AVec::new_in(a);
                let mut buf = [0u8; 64 << 10];
                let mut off = 0u64;
                loop {
                    let n = pread(cache_fd.fd(), &mut buf, off)
                        .map_err(|errno| Fault::Host { call: "pread (/etc/ld.so.cache)", code: errno.raw() })?;
                    if n == 0 {
                        break;
                    }
                    if raw.len().saturating_add(n) > MAX_LD_CACHE_BYTES {
                        return Err(Fault::Host { call: "read (/etc/ld.so.cache exceeds 4 MiB cap)", code: 27 });
                    }
                    if let Some(chunk) = buf.get(..n) {
                        raw.extend_from_slice(chunk);
                    }
                    let step = u64::try_from(n).map_err(|_| Fault::Host { call: "pread (/etc/ld.so.cache overflow)", code: 75 })?;
                    off = off.checked_add(step).ok_or(Fault::Host { call: "pread (/etc/ld.so.cache overflow)", code: 75 })?;
                }
                ld_cache_bytes = Some(raw);
            }
            Err(blaze_sys::Errno::NoEnt) => {}
            Err(errno) => {
                return Err(Fault::Host { call: "openat (/etc/ld.so.cache)", code: errno.raw() });
            }
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

    let mut interp_resolved = [0u8; PATH_CAP];
    let active_loader_for_lib: &[u8] = if is_nix_ld {
        glibc_dir.unwrap_or(interp_cstr.to_bytes())
    } else if let Some(rlen) = resolve_symlink_path(interp_cstr.to_bytes(), &mut interp_resolved) {
        interp_resolved.get(..rlen).unwrap_or(interp_cstr.to_bytes())
    } else {
        interp_cstr.to_bytes()
    };
    let lib_token = dl_dst_lib_token(active_loader_for_lib);
    let platform_token = dl_dst_platform_token();
    if let Some(f) = feed.as_deref_mut() {
        f(b"dl-dst:");
        f(lib_token);
        f(b":");
        f(platform_token);
    }

    // Bounded BFS queue of `(canonical ELF path, len, ordered inherited DT_RPATH chain)` seeded
    // ONLY with the executable itself, its `PT_INTERP`, and (for `nix-ld`) `NIX_LD_SO`.
    // For the primary program we always keep `program` (its shelf path) for `$ORIGIN` expansion
    // even when `verified_exec_fd` is an anonymous sealed `memfd`.
    let host_ld_caps = if !is_nix {
        HostLdCaps::detect()?
    } else {
        HostLdCaps { at_hwcap: 0, x86_64_level: 0 }
    };
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
        let Some(meta) = parse_elf64_meta(reader, cx, elf_fd)? else {
            continue;
        };
        if meta.dyn_len == 0 {
            continue;
        }
        let mut dyn_buf = [0u8; 8192];
        let Some(dyn_slice) = dyn_buf.get_mut(..meta.dyn_len) else {
            return Err(Fault::Host { call: "landlock (NixOS ELF PT_DYNAMIC exceeds cap)", code: 7 });
        };
        let dyn_read = read_nix_at(reader, cx, elf_fd, dyn_slice, meta.dyn_off, "pread (nix ELF PT_DYNAMIC)")?;
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
        // - When launching the primary executable (`cur_idx == 0`) from an anonymous sealed memfd
        //   via `execveat(..., AT_EMPTY_PATH)`, glibc derives `$ORIGIN` from `/memfd:...` as `"/"`
        //   (verified with `LD_DEBUG=libs`), NOT the on-disk shelf directory.
        let origin_dir: &[u8] = if cur_idx == 0 && memfd_origin {
            b"/"
        } else {
            cur_path
                .get(..cur_len)
                .and_then(|p| p.iter().rposition(|&b| b == b'/').and_then(|s| p.get(..s)))
                .unwrap_or(&[])
        };
        let mut search_dirs = [([0u8; PATH_CAP], 0usize); 16];
        let mut search_count = 0usize;
        let child_chain = if let Some(rp_off) = runpath_off {
            if let Some(at) = strtab_file_off.checked_add(rp_off) {
                let mut rp_buf = [0u8; 2048];
                let n = read_nix_at(reader, cx, elf_fd, &mut rp_buf, at, "pread (nix ELF DT_RUNPATH)")?;
                let raw = rp_buf.get(..n).unwrap_or(&[]);
                let Some(end) = raw.iter().position(|&b| b == 0) else {
                    return Err(Fault::Host { call: "landlock (NixOS ELF RUNPATH exceeds buffer cap)", code: 7 });
                };
                let rp_raw = raw.get(..end).unwrap_or(&[]);
                if !rp_raw.is_empty() {
                    for entry in rp_raw.split(|&b| b == b':') {
                        let Some(slot) = search_dirs.get_mut(search_count) else {
                            return Err(Fault::Host { call: "landlock (NixOS ELF RUNPATH entries exceed cap)", code: 7 });
                        };
                        let expanded_len = validate_rpath_entry(entry, origin_dir, lib_token, platform_token, &mut slot.0)?;
                        slot.1 = expanded_len;
                        search_count += 1;
                    }
                }
            }
            inherited_chain
        } else {
            let mut eff_chain = RpathChain::EMPTY;
            if let Some(rp_off) = rpath_off
                && let Some(at) = strtab_file_off.checked_add(rp_off)
            {
                let mut rp_buf = [0u8; 2048];
                let n = read_nix_at(reader, cx, elf_fd, &mut rp_buf, at, "pread (nix ELF DT_RPATH)")?;
                let raw = rp_buf.get(..n).unwrap_or(&[]);
                let Some(end) = raw.iter().position(|&b| b == 0) else {
                    return Err(Fault::Host { call: "landlock (NixOS ELF RPATH exceeds buffer cap)", code: 7 });
                };
                let rp_raw = raw.get(..end).unwrap_or(&[]);
                if !rp_raw.is_empty() {
                    for entry in rp_raw.split(|&b| b == b':') {
                        let mut exp = [0u8; PATH_CAP];
                        let expanded_len = validate_rpath_entry(entry, origin_dir, lib_token, platform_token, &mut exp)?;
                        let idx = intern_rpath_dir(exp.get(..expanded_len).unwrap_or(&[]), &mut rpath_pool, &mut rpath_pool_len)?;
                        eff_chain.push(idx)?;
                    }
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
            let n = read_nix_at(reader, cx, elf_fd, &mut name_buf, at, "pread (nix ELF DT_NEEDED)")?;
            let raw = name_buf.get(..n).unwrap_or(&[]);
            let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            let soname = raw.get(..end).unwrap_or(&[]);
            if soname.is_empty() {
                continue;
            }
            if soname.contains(&b'/') {
                let mut expanded = [0u8; PATH_CAP];
                let exp_len = expand_rpath_tokens(soname, origin_dir, lib_token, platform_token, &mut expanded)?;
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
                    reader,
                    cx,
                    ruleset,
                    b"",
                    exp_slice,
                    child_chain,
                    NIX_LD_DIR,
                    is_nix,
                    false,
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
                    reader,
                    cx,
                    ruleset,
                    soname,
                    search_dirs.get(..search_count).unwrap_or(&[]),
                    child_chain,
                    NIX_LD_DIR,
                    glibc_dir,
                    is_nix,
                    ld_cache_bytes.as_deref(),
                    &host_ld_caps,
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
    reader: Option<&super::RootReader>,
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

/// Derives glibc's `$LIB` dynamic-string-token value (`DL_DST_LIB`) from the resolved dynamic
/// loader path (`PT_INTERP`): Debian/Ubuntu multiarch uses `lib/<triplet>`, standard 64-bit FHS
/// uses `lib64`, and NixOS glibc uses `lib`.
pub fn dl_dst_lib_token(loader_path: &[u8]) -> &'static [u8] {
    let dir = match loader_path.iter().rposition(|&b| b == b'/') {
        Some(pos) => loader_path.get(..pos).unwrap_or(&[]),
        None => &[],
    };
    if dir.ends_with(b"/lib/x86_64-linux-gnu") {
        b"lib/x86_64-linux-gnu"
    } else if dir.ends_with(b"/lib/aarch64-linux-gnu") {
        b"lib/aarch64-linux-gnu"
    } else if dir.ends_with(b"/lib64") {
        b"lib64"
    } else {
        b"lib"
    }
}

/// Returns Linux's `AT_PLATFORM` (`ELF_PLATFORM`) dynamic-string-token value for this target.
pub const fn dl_dst_platform_token() -> &'static [u8] {
    #[cfg(target_arch = "aarch64")]
    {
        b"aarch64"
    }
    #[cfg(target_arch = "x86_64")]
    {
        b"x86_64"
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        b""
    }
}

fn dst_boundary(next: Option<u8>) -> bool {
    matches!(next, None | Some(b'/') | Some(b':'))
}

/// Expands glibc's standard ELF dynamic string tokens (`$ORIGIN`/`${ORIGIN}`, `$LIB`/`${LIB}`,
/// `$PLATFORM`/`${PLATFORM}`) in a `DT_RPATH`, `DT_RUNPATH`, or path-bearing `DT_NEEDED` entry.
/// Refuses unknown `$...` tokens with `ENOSYS` rather than passing them through unexpanded.
pub fn expand_rpath_tokens(
    entry: &[u8],
    origin_dir: &[u8],
    lib_token: &[u8],
    platform_token: &[u8],
    out: &mut [u8; 320],
) -> Result<usize, Fault> {
    let mut len = 0usize;
    let mut i = 0usize;
    while i < entry.len() {
        let rest = entry.get(i..).unwrap_or(&[]);
        let (chunk, step) = if rest.starts_with(b"${ORIGIN}") {
            (origin_dir, 9)
        } else if rest.starts_with(b"$ORIGIN") && dst_boundary(rest.get(7).copied()) {
            (origin_dir, 7)
        } else if rest.starts_with(b"${LIB}") {
            (lib_token, 6)
        } else if rest.starts_with(b"$LIB") && dst_boundary(rest.get(4).copied()) {
            (lib_token, 4)
        } else if rest.starts_with(b"${PLATFORM}") && !platform_token.is_empty() {
            (platform_token, 11)
        } else if rest.starts_with(b"$PLATFORM") && !platform_token.is_empty() && dst_boundary(rest.get(9).copied()) {
            (platform_token, 9)
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

/// Whether a glibc `ld.so.cache` entry's `flags` field matches an ELF shared library for the
/// current target architecture (`FLAG_ELF_LIBC6` / `FLAG_ELF` + target `FLAG_ARCH`).
fn matches_target_ld_cache_flags(flags: u32) -> bool {
    let kind = flags & 0x00ff;
    if kind != 0x0003 && kind != 0x0001 {
        return false;
    }
    #[cfg(target_arch = "aarch64")]
    {
        (flags & 0xff00) == 0x0a00
    }
    #[cfg(target_arch = "x86_64")]
    {
        (flags & 0xff00) == 0x0300
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        true
    }
}

/// Host CPU capabilities read from `/proc/self/auxv` (and `/proc/cpuinfo` on x86_64) used to
/// match glibc `ld.so`'s exact `glibc-hwcaps` and legacy `AT_HWCAP` entry selection in
/// `/etc/ld.so.cache`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostLdCaps {
    pub at_hwcap: u64,
    pub x86_64_level: u8,
}

impl HostLdCaps {
    /// Detects host CPU capabilities from `/proc/self/auxv` (and `/proc/cpuinfo` on x86-64).
    /// Fails loudly (AGENTS.md rule 3) when required capability metadata is unreadable instead of
    /// silently guessing baseline capabilities that could diverge from `ld.so`.
    pub fn detect() -> Result<Self, Fault> {
        let at_hwcap = read_proc_auxv_hwcap()?;
        #[cfg(target_arch = "x86_64")]
        let x86_64_level = read_proc_cpuinfo_x86_64_level()?;
        #[cfg(not(target_arch = "x86_64"))]
        let x86_64_level = 0;
        Ok(Self { at_hwcap, x86_64_level })
    }
}

fn read_proc_auxv_hwcap() -> Result<u64, Fault> {
    let fd = openat(CWD, blaze_sys::Path::new(c"/proc/self/auxv"), OpenFlags::CLOSE_ON_EXEC, 0)
        .map_err(|errno| Fault::Host { call: "openat (/proc/self/auxv for ld.so.cache hwcap)", code: errno.raw() })?;
    let mut buf = [0u8; 2048];
    let mut filled = 0usize;
    while filled < buf.len() {
        let Ok(off) = u64::try_from(filled) else {
            break;
        };
        let Some(dst) = buf.get_mut(filled..) else {
            break;
        };
        let n = pread(fd.fd(), dst, off)
            .map_err(|errno| Fault::Host { call: "pread (/proc/self/auxv for ld.so.cache hwcap)", code: errno.raw() })?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    let mut pos = 0usize;
    while pos + 16 <= filled {
        let tag = read_u64_le(&buf, pos);
        let val = read_u64_le(&buf, pos + 8);
        pos += 16;
        if tag == 0 {
            break;
        }
        if tag == 16 {
            // AT_HWCAP
            return Ok(val);
        }
    }
    Ok(0)
}

#[cfg(target_arch = "x86_64")]
fn read_proc_cpuinfo_x86_64_level() -> Result<u8, Fault> {
    let fd = openat(CWD, blaze_sys::Path::new(c"/proc/cpuinfo"), OpenFlags::CLOSE_ON_EXEC, 0)
        .map_err(|errno| Fault::Host { call: "openat (/proc/cpuinfo for x86-64 glibc-hwcaps)", code: errno.raw() })?;
    let mut buf = [0u8; 8192];
    let mut filled = 0usize;
    while filled < buf.len() {
        let Ok(off) = u64::try_from(filled) else {
            break;
        };
        let Some(dst) = buf.get_mut(filled..) else {
            break;
        };
        let n = pread(fd.fd(), dst, off)
            .map_err(|errno| Fault::Host { call: "pread (/proc/cpuinfo for x86-64 glibc-hwcaps)", code: errno.raw() })?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    let text = buf.get(..filled).unwrap_or(&[]);
    let Some(flags_line) = text
        .split(|&b| b == b'\n')
        .find(|line| line.starts_with(b"flags") && line.contains(&b':'))
    else {
        return Err(Fault::Host { call: "parse (/proc/cpuinfo missing flags for x86-64 glibc-hwcaps)", code: 22 });
    };
    let Some(colon) = flags_line.iter().position(|&b| b == b':') else {
        return Err(Fault::Host { call: "parse (/proc/cpuinfo missing flags colon)", code: 22 });
    };
    let flags = flags_line.get(colon + 1..).unwrap_or(&[]);
    let has = |feat: &[u8]| flags.split(|&b| b == b' ' || b == b'\t').any(|tok| tok == feat);
    let v2 = has(b"cx16") && has(b"lahf_lm") && has(b"popcnt") && has(b"sse4_1") && has(b"sse4_2") && has(b"ssse3");
    if !v2 {
        return Ok(1);
    }
    let v3 = has(b"avx")
        && has(b"avx2")
        && has(b"bmi1")
        && has(b"bmi2")
        && has(b"f16c")
        && has(b"fma")
        && (has(b"abm") || has(b"lzcnt"))
        && has(b"movbe")
        && has(b"xsave");
    if !v3 {
        return Ok(2);
    }
    let v4 = has(b"avx512f") && has(b"avx512bw") && has(b"avx512cd") && has(b"avx512dq") && has(b"avx512vl");
    Ok(if v4 { 4 } else { 3 })
}

/// Locates the `cache_extension_tag_glibc_hwcaps` (`tag == 1`) string-offset table inside a
/// `glibc-ld.so.cache1.1` header, returning `(table_file_offset, entry_count)` when present.
/// Per glibc `elf/dl-cache.h`, `extension_offset` and each section's `offset` are file-relative
/// (from offset 0, even in compat `ld.so-1.7.0` + `glibc-ld.so.cache1.1` caches), while the
/// `u32` string offsets stored inside the `glibc-hwcaps` section are relative to `base_off`.
fn find_glibc_hwcaps_extension(cache_bytes: &[u8], base_off: usize) -> Option<(usize, usize)> {
    const EXT_MAGIC: u32 = 0xeaa4_2174;
    const TAG_GLIBC_HWCAPS: u32 = 1;
    let ext_off = usize::try_from(read_u32_le(cache_bytes, base_off + 32)).ok()?;
    if ext_off == 0 {
        return None;
    }
    if ext_off.checked_add(8)? > cache_bytes.len() {
        return None;
    }
    if read_u32_le(cache_bytes, ext_off) != EXT_MAGIC {
        return None;
    }
    let count = usize::try_from(read_u32_le(cache_bytes, ext_off + 4)).ok()?;
    let sections_start = ext_off.checked_add(8)?;
    let sections_end = sections_start.checked_add(count.checked_mul(16)?)?;
    if sections_end > cache_bytes.len() {
        return None;
    }
    for i in 0..count {
        let s_off = sections_start + i * 16;
        if read_u32_le(cache_bytes, s_off) == TAG_GLIBC_HWCAPS {
            let data_off = usize::try_from(read_u32_le(cache_bytes, s_off + 8)).ok()?;
            let data_size = usize::try_from(read_u32_le(cache_bytes, s_off + 12)).ok()?;
            if data_off.checked_add(data_size)? <= cache_bytes.len() {
                return Some((data_off, data_size / 4));
            }
        }
    }
    None
}

/// Returns glibc `ld.so`'s selection rank for a cache entry (`None` if unsupported on `host_caps`,
/// higher `u16` = higher priority: `x86-64-v4` > `x86-64-v3` > `x86-64-v2` > legacy `AT_HWCAP` >
/// baseline `hwcap == 0`).
fn ld_cache_hwcap_rank(
    cache_bytes: &[u8],
    base_off: usize,
    hwcaps_ext: Option<(usize, usize)>,
    hwcap: u64,
    host_caps: &HostLdCaps,
) -> Option<u16> {
    if hwcap == 0 {
        return Some(0);
    }
    let tag = hwcap >> 62;
    if tag == 1 {
        let (table_off, count) = hwcaps_ext?;
        let idx = usize::try_from(hwcap & 0xffff_ffff).ok()?;
        if idx >= count {
            return None;
        }
        let str_rel = usize::try_from(read_u32_le(cache_bytes, table_off + idx * 4)).ok()?;
        let str_off = base_off.checked_add(str_rel)?;
        let tail = cache_bytes.get(str_off..)?;
        let end = tail.iter().position(|&b| b == 0)?;
        let subdir = tail.get(..end).unwrap_or(&[]);
        return match subdir {
            b"x86-64-v2" if host_caps.x86_64_level >= 2 => Some(102),
            b"x86-64-v3" if host_caps.x86_64_level >= 3 => Some(103),
            b"x86-64-v4" if host_caps.x86_64_level >= 4 => Some(104),
            _ => None,
        };
    }
    if tag == 0 {
        // Legacy `AT_HWCAP` entry (bits 0..55; bits 56..61 encode legacy platform id).
        if (hwcap >> 56) == 0 && (hwcap & !host_caps.at_hwcap) == 0 {
            return Some(1);
        }
    }
    None
}

/// Parses glibc's `/etc/ld.so.cache` (`glibc-ld.so.cache1.1`, either standalone at offset 0 or
/// 8-byte-aligned after a compat `ld.so-1.7.0` header), collects all architecture- and
/// `host_caps`-compatible entries for `soname`, ranks them by glibc's runtime priority
/// (`x86-64-v4` > `x86-64-v3` > `x86-64-v2` > legacy `AT_HWCAP` > baseline, breaking ties by
/// original cache order), and stops on the FIRST existing library where `on_path` returns
/// `Ok(true)`.
pub fn parse_ld_so_cache_match(
    cache_bytes: &[u8],
    soname: &[u8],
    host_caps: &HostLdCaps,
    mut on_path: impl FnMut(&[u8]) -> Result<bool, Fault>,
) -> Result<bool, Fault> {
    const NEW_MAGIC: &[u8] = b"glibc-ld.so.cache1.1";
    const OLD_MAGIC: &[u8] = b"ld.so-1.7.0";
    const NEW_HDR_LEN: usize = 48;
    const NEW_ENTRY_LEN: usize = 24;
    const MAX_CANDIDATES: usize = 32;

    let base_off = if cache_bytes.starts_with(NEW_MAGIC) {
        0usize
    } else if cache_bytes.starts_with(OLD_MAGIC) && cache_bytes.len() >= 16 {
        let old_nlibs = usize::try_from(read_u32_le(cache_bytes, 12))
            .map_err(|_| Fault::Host { call: "ld.so.cache (invalid compat nlibs)", code: 22 })?;
        let old_table_end = old_nlibs
            .checked_mul(12)
            .and_then(|n| n.checked_add(16))
            .ok_or(Fault::Host { call: "ld.so.cache (compat table overflow)", code: 22 })?;
        let aligned = old_table_end
            .checked_add(7)
            .map(|n| n & !7)
            .ok_or(Fault::Host { call: "ld.so.cache (compat align overflow)", code: 22 })?;
        if cache_bytes.get(aligned..aligned + NEW_MAGIC.len()) != Some(NEW_MAGIC) {
            return Err(Fault::Host { call: "ld.so.cache (missing glibc-ld.so.cache1.1 header)", code: 22 });
        }
        aligned
    } else {
        return Err(Fault::Host { call: "ld.so.cache (unrecognized magic)", code: 22 });
    };

    let hdr_end = base_off
        .checked_add(NEW_HDR_LEN)
        .ok_or(Fault::Host { call: "ld.so.cache (header overflow)", code: 22 })?;
    if hdr_end > cache_bytes.len() {
        return Err(Fault::Host { call: "ld.so.cache (truncated header)", code: 22 });
    }
    let nlibs = usize::try_from(read_u32_le(cache_bytes, base_off + 20))
        .map_err(|_| Fault::Host { call: "ld.so.cache (invalid nlibs)", code: 22 })?;
    let table_bytes = nlibs
        .checked_mul(NEW_ENTRY_LEN)
        .and_then(|n| hdr_end.checked_add(n))
        .ok_or(Fault::Host { call: "ld.so.cache (table overflow)", code: 22 })?;
    if table_bytes > cache_bytes.len() {
        return Err(Fault::Host { call: "ld.so.cache (truncated entry table)", code: 22 });
    }

    let hwcaps_ext = find_glibc_hwcaps_extension(cache_bytes, base_off);
    // Stack-allocated ranked candidate list: `(rank, original_idx, val_off, val_len)`.
    let mut candidates = [(0u16, 0usize, 0usize, 0usize); MAX_CANDIDATES];
    let mut cand_len = 0usize;
    for idx in 0..nlibs {
        let entry_off = hdr_end + idx * NEW_ENTRY_LEN;
        let flags = read_u32_le(cache_bytes, entry_off);
        if !matches_target_ld_cache_flags(flags) {
            continue;
        }
        let hwcap = read_u64_le(cache_bytes, entry_off + 16);
        let Some(rank) = ld_cache_hwcap_rank(cache_bytes, base_off, hwcaps_ext, hwcap, host_caps) else {
            continue;
        };
        let Ok(key_rel) = usize::try_from(read_u32_le(cache_bytes, entry_off + 4)) else {
            continue;
        };
        let Ok(val_rel) = usize::try_from(read_u32_le(cache_bytes, entry_off + 8)) else {
            continue;
        };
        let (Some(key_off), Some(val_off)) = (base_off.checked_add(key_rel), base_off.checked_add(val_rel)) else {
            continue;
        };
        let Some(key_tail) = cache_bytes.get(key_off..) else {
            continue;
        };
        let Some(key_end) = key_tail.iter().position(|&b| b == 0) else {
            continue;
        };
        if key_tail.get(..key_end) != Some(soname) {
            continue;
        }
        let Some(val_tail) = cache_bytes.get(val_off..) else {
            continue;
        };
        let Some(val_end) = val_tail.iter().position(|&b| b == 0) else {
            continue;
        };
        if val_end == 0 {
            continue;
        }
        if let Some(slot) = candidates.get_mut(cand_len) {
            *slot = (rank, idx, val_off, val_end);
            cand_len += 1;
        } else {
            return Err(Fault::Host { call: "ld.so.cache (matching SONAME alternatives exceed cap)", code: 7 });
        }
    }
    if let Some(used) = candidates.get_mut(..cand_len) {
        // Stable descending rank, ascending cache index on ties.
        used.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        for &(_, _, val_off, val_end) in used.iter() {
            let path = cache_bytes.get(val_off..val_off + val_end).unwrap_or(&[]);
            if !path.is_empty() && on_path(path)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn enqueue_nix_soname<const N: usize>(
    reader: Option<&super::RootReader>,
    cx: Option<&Cx>,
    ruleset: Option<Fd>,
    soname: &[u8],
    search_dirs: &[([u8; 320], usize)],
    child_chain: RpathChain,
    nix_ld_dir: &[u8],
    glibc_dir: Option<&[u8]>,
    is_nix: bool,
    ld_cache: Option<&[u8]>,
    host_caps: &HostLdCaps,
    plan: Option<&Plan<'_>>,
    queue: &mut [([u8; 320], usize, RpathChain); N],
    q_len: &mut usize,
    feed: &mut Option<&mut dyn FnMut(&[u8])>,
) -> Result<(), Fault> {
    for (dir_buf, dir_len) in search_dirs {
        let dir = dir_buf.get(..*dir_len).unwrap_or(&[]);
        if try_grant_and_enqueue_lib(reader, cx, ruleset, dir, soname, child_chain, nix_ld_dir, is_nix, false, plan, queue, q_len, feed)? {
            return Ok(());
        }
    }
    if is_nix {
        if try_grant_and_enqueue_lib(reader, cx, ruleset, nix_ld_dir, soname, child_chain, nix_ld_dir, is_nix, false, plan, queue, q_len, feed)? {
            return Ok(());
        }
        if let Some(dir) = glibc_dir {
            if try_grant_and_enqueue_lib(reader, cx, ruleset, dir, soname, child_chain, nix_ld_dir, is_nix, false, plan, queue, q_len, feed)? {
                return Ok(());
            }
        }
    } else {
        // On non-Nix FHS Linux (in BOTH Landlock confinement AND runtime closure hashing),
        // select ONLY the single active entry in `/etc/ld.so.cache` matching glibc `ld.so`'s
        // priority order and `host_caps` (`glibc-hwcaps` / `AT_HWCAP`), falling back to standard
        // FHS dirs only when no matching cache entry exists.
        if let Some(cache_bytes) = ld_cache
            && !cache_bytes.is_empty()
        {
            let found = parse_ld_so_cache_match(cache_bytes, soname, host_caps, |resolved_path| {
                try_grant_and_enqueue_lib(
                    reader,
                    cx,
                    ruleset,
                    b"",
                    resolved_path,
                    child_chain,
                    nix_ld_dir,
                    is_nix,
                    true,
                    plan,
                    queue,
                    q_len,
                    feed,
                )
            })?;
            if found {
                return Ok(());
            }
        }
        #[cfg(target_arch = "aarch64")]
        let fhs_dirs: &[&[u8]] = &[
            b"/lib/aarch64-linux-gnu/",
            b"/usr/lib/aarch64-linux-gnu/",
            b"/lib64/",
            b"/usr/lib64/",
            b"/lib/",
            b"/usr/lib/",
        ];
        #[cfg(target_arch = "x86_64")]
        let fhs_dirs: &[&[u8]] = &[
            b"/lib/x86_64-linux-gnu/",
            b"/usr/lib/x86_64-linux-gnu/",
            b"/lib64/",
            b"/usr/lib64/",
            b"/lib/",
            b"/usr/lib/",
        ];
        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        let fhs_dirs: &[&[u8]] = &[b"/lib64/", b"/usr/lib64/", b"/lib/", b"/usr/lib/"];
        let hwcap_subdirs = fhs_hwcap_subdirs(host_caps);
        for fhs_dir in fhs_dirs {
            // glibc searches supported `glibc-hwcaps/<subdir>/` entries inside each search dir
            // in descending ISA rank BEFORE falling back to the baseline directory itself.
            for sub in hwcap_subdirs {
                let mut joined = [0u8; 128];
                let jlen = fhs_dir.len().saturating_add(sub.len());
                if jlen <= joined.len() {
                    if let Some(head) = joined.get_mut(..fhs_dir.len()) {
                        head.copy_from_slice(fhs_dir);
                    }
                    if let Some(tail) = joined.get_mut(fhs_dir.len()..jlen) {
                        tail.copy_from_slice(sub);
                    }
                    if let Some(prefix) = joined.get(..jlen)
                        && try_grant_and_enqueue_lib(reader, cx, ruleset, prefix, soname, child_chain, nix_ld_dir, is_nix, false, plan, queue, q_len, feed)?
                    {
                        return Ok(());
                    }
                }
            }
            if try_grant_and_enqueue_lib(reader, cx, ruleset, fhs_dir, soname, child_chain, nix_ld_dir, is_nix, false, plan, queue, q_len, feed)? {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Streaming line-oriented scanner for `/etc/ld.so.preload` chunks: returns `true` as soon as any
/// non-whitespace, non-comment (`#`) entry byte is observed, across arbitrary chunk boundaries.
pub struct PreloadScanner {
    at_line_start: bool,
    in_comment: bool,
}

impl PreloadScanner {
    pub const fn new() -> Self {
        Self { at_line_start: true, in_comment: false }
    }

    pub fn feed(&mut self, chunk: &[u8]) -> bool {
        for &b in chunk {
            if b == b'\n' {
                self.at_line_start = true;
                self.in_comment = false;
            } else if self.in_comment {
                continue;
            } else if self.at_line_start {
                if b.is_ascii_whitespace() {
                    continue;
                }
                if b == b'#' {
                    self.in_comment = true;
                    self.at_line_start = false;
                } else {
                    return true;
                }
            }
        }
        false
    }
}

/// Ranked `glibc-hwcaps` subdirectories searched before baseline FHS directories for a given
/// x86-64 microarchitecture level (4 -> v4,v3,v2; 3 -> v3,v2; 2 -> v2; <=1 -> none).
pub const fn x86_64_fhs_hwcap_subdirs(x86_64_level: u8) -> &'static [&'static [u8]] {
    const ALL: &[&[u8]] = &[
        b"glibc-hwcaps/x86-64-v4/",
        b"glibc-hwcaps/x86-64-v3/",
        b"glibc-hwcaps/x86-64-v2/",
    ];
    match x86_64_level {
        4.. => ALL,
        3 => &[b"glibc-hwcaps/x86-64-v3/", b"glibc-hwcaps/x86-64-v2/"],
        2 => &[b"glibc-hwcaps/x86-64-v2/"],
        _ => &[],
    }
}

fn fhs_hwcap_subdirs(host_caps: &HostLdCaps) -> &'static [&'static [u8]] {
    #[cfg(target_arch = "x86_64")]
    {
        x86_64_fhs_hwcap_subdirs(host_caps.x86_64_level)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = host_caps;
        &[]
    }
}

fn try_grant_and_enqueue_lib<const N: usize>(
    reader: Option<&super::RootReader>,
    cx: Option<&Cx>,
    ruleset: Option<Fd>,
    dir_prefix: &[u8],
    soname: &[u8],
    child_chain: RpathChain,
    nix_ld_dir: &[u8],
    is_nix: bool,
    from_ld_cache: bool,
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
    let fd = match openat(CWD, blaze_sys::Path::new(cstr), OpenFlags::CLOSE_ON_EXEC, 0) {
        Ok(fd) => fd,
        Err(blaze_sys::Errno::NoEnt) => return Ok(false),
        Err(errno) => return Err(Fault::Host { call: "openat (nix runtime file)", code: errno.raw() }),
    };
    // Verify ELF64 class, little-endian data, and target `e_machine` (`EM_AARCH64=183` /
    // `EM_X86_64=62`) on the opened fd so cross-arch libraries in shared FHS dirs are skipped
    // just as glibc `ld.so` skips them.
    let mut ehdr_head = [0u8; 20];
    let head_n = read_nix_at(reader, cx, fd.fd(), &mut ehdr_head, 0, "pread (ELF e_machine check)")?;
    if head_n < 20 || ehdr_head.get(..6) != Some(&[0x7f, b'E', b'L', b'F', 2, 1]) {
        return Ok(false);
    }
    let e_machine = read_u16_le(&ehdr_head, 18);
    #[cfg(target_arch = "aarch64")]
    if e_machine != 183 {
        return Ok(false);
    }
    #[cfg(target_arch = "x86_64")]
    if e_machine != 62 {
        return Ok(false);
    }
    // Ask the kernel for the canonical realpath of the ALREADY-OPENED fd via `/proc/self/fd/<fd>`,
    // resolving every intermediate directory symlink and preventing TOCTOU before authorizing the
    // inode for Landlock or hashing its content.
    let mut resolved = [0u8; 320];
    let res_len = readlink_fd_path(fd.fd(), &mut resolved)?;
    let res_slice = resolved.get(..res_len).unwrap_or(&[]);
    if let Some(p) = plan {
        let authorized = if !is_nix && from_ld_cache {
            // System-configured `/etc/ld.so.cache` targets (e.g. `/opt/vendor` or `glibc-hwcaps`)
            // are authorized for single-file `READ_FILE` (never `EXECUTE`) as long as the resolved
            // path is absolute and traversal-free.
            res_slice.first() == Some(&b'/') && !res_slice.split(|&b| b == b'/').any(|seg| seg == b"..")
        } else {
            is_authorized_nix_dep_path(res_slice, nix_ld_dir, p)
        };
        if !authorized {
            return Err(Fault::Host {
                call: "landlock (NixOS ELF dependency resolved outside authorized roots)",
                code: 13,
            });
        }
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
    reader: Option<&super::RootReader>,
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
    reader: Option<&super::RootReader>,
    cx: Option<&Cx>,
    canonical_path: &[u8],
    fd: Fd,
    feed: &mut dyn FnMut(&[u8]),
) -> Result<(), Fault> {
    let path_len = u32::try_from(canonical_path.len()).unwrap_or(u32::MAX);
    feed(&path_len.to_le_bytes());
    feed(canonical_path);
    let offset = if let (Some(r), Some(cx)) = (reader, cx) {
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
    fn start(&mut self, spawner: &Spawner, cx: &Cx, start: &StartView<'_>, streams: Streams, alloc: Alloc<'_>) -> Result<Started<Run>, Fault> {
        let spec = start.spec;
        let plan = start.plan()?;
        let Program::Stored { path, .. } = spec.program() else {
            return Err(Fault::RecordsNeedExecution);
        };
        let prepared = prepare_with_exec(plan, spec.budget(), spec.starts_programs(), spec.verified_exec_fd(), Some(alloc))?;
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
    use alloc::vec::Vec;

    use super::{Fault, HostLdCaps, RpathChain, enqueue_nix_resolved_path, parse_ld_so_cache_match};

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

    #[test]
    fn ld_so_cache_parser_selects_only_active_entry_in_both_formats() {
        #[cfg(target_arch = "aarch64")]
        let (target_flags, other_flags) = (0x0a03u32, 0x0303u32);
        #[cfg(target_arch = "x86_64")]
        let (target_flags, other_flags) = (0x0303u32, 0x0a03u32);
        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        let (target_flags, other_flags) = (0x0003u32, 0x0000u32);

        let build_cache = |compat_prefix: bool| -> Vec<u8> {
            let mut buf = Vec::new();
            if compat_prefix {
                buf.extend_from_slice(b"ld.so-1.7.0\0");
                buf.extend_from_slice(&1u32.to_le_bytes());
                buf.extend_from_slice(&[0u8; 12]);
                while buf.len() % 8 != 0 {
                    buf.push(0);
                }
            }
            let base_off = buf.len();
            // Ordered v2-first (matching real glibc `ldconfig`), followed by v3, v4, and baseline.
            let entries: &[(u32, u64, &[u8], &[u8])] = &[
                (target_flags, (1u64 << 62) | 0, b"libz.so.1", b"/opt/custom/glibc-hwcaps/x86-64-v2/libz.so.1"),
                (target_flags, (1u64 << 62) | 1, b"libz.so.1", b"/opt/custom/glibc-hwcaps/x86-64-v3/libz.so.1"),
                (target_flags, (1u64 << 62) | 2, b"libz.so.1", b"/opt/custom/glibc-hwcaps/x86-64-v4/libz.so.1"),
                (target_flags, 0, b"libz.so.1", b"/opt/custom/libz.so.1"),
                (other_flags, 0, b"libz.so.1", b"/usr/lib/wrong-arch/libz.so.1"),
            ];
            let str_base = 48 + entries.len() * 24;
            let mut strings = Vec::new();
            let mut offsets = Vec::new();
            for &(flags, hwcap, key, val) in entries {
                let k_off = str_base + strings.len();
                strings.extend_from_slice(key);
                strings.push(0);
                let v_off = str_base + strings.len();
                strings.extend_from_slice(val);
                strings.push(0);
                offsets.push((flags, hwcap, k_off as u32, v_off as u32));
            }
            let mut subdir_offs = [0u32; 3];
            for (slot, name) in subdir_offs.iter_mut().zip([b"x86-64-v2\0".as_slice(), b"x86-64-v3\0", b"x86-64-v4\0"]) {
                *slot = (str_base + strings.len()) as u32;
                strings.extend_from_slice(name);
            }
            buf.extend_from_slice(b"glibc-ld.so.cache1.1");
            buf.extend_from_slice(&(entries.len() as u32).to_le_bytes());
            buf.extend_from_slice(&(strings.len() as u32).to_le_bytes());
            buf.resize(base_off + 48, 0);
            for (flags, hwcap, k_off, v_off) in offsets {
                buf.extend_from_slice(&flags.to_le_bytes());
                buf.extend_from_slice(&k_off.to_le_bytes());
                buf.extend_from_slice(&v_off.to_le_bytes());
                buf.extend_from_slice(&0u32.to_le_bytes());
                buf.extend_from_slice(&hwcap.to_le_bytes());
            }
            buf.extend_from_slice(&strings);
            while buf.len() % 4 != 0 {
                buf.push(0);
            }
            // Per glibc `elf/dl-cache.h`, `extension_offset` and section `offset` are file-relative.
            let ext_off = buf.len() as u32;
            buf[base_off + 32..base_off + 36].copy_from_slice(&ext_off.to_le_bytes());
            let data_off = ext_off + 8 + 16;
            buf.extend_from_slice(&0xeaa4_2174u32.to_le_bytes());
            buf.extend_from_slice(&1u32.to_le_bytes());
            buf.extend_from_slice(&1u32.to_le_bytes());
            buf.extend_from_slice(&0u32.to_le_bytes());
            buf.extend_from_slice(&data_off.to_le_bytes());
            buf.extend_from_slice(&12u32.to_le_bytes());
            for off in subdir_offs {
                buf.extend_from_slice(&off.to_le_bytes());
            }
            buf
        };

        let new_only = build_cache(false);
        let compat = build_cache(true);
        for buf in [&new_only, &compat] {
            for (level, want) in [
                (4u8, b"/opt/custom/glibc-hwcaps/x86-64-v4/libz.so.1".as_slice()),
                (3u8, b"/opt/custom/glibc-hwcaps/x86-64-v3/libz.so.1".as_slice()),
                (2u8, b"/opt/custom/glibc-hwcaps/x86-64-v2/libz.so.1".as_slice()),
                (1u8, b"/opt/custom/libz.so.1".as_slice()),
            ] {
                let caps = HostLdCaps { at_hwcap: 0, x86_64_level: level };
                let mut found: Vec<Vec<u8>> = Vec::new();
                assert!(parse_ld_so_cache_match(buf, b"libz.so.1", &caps, |p| { found.push(p.to_vec()); Ok(true) }).expect("parse"));
                assert_eq!(found, alloc::vec![want.to_vec()]);
            }
        }
        let err = parse_ld_so_cache_match(b"not-an-ld-cache", b"libz.so.1", &HostLdCaps { at_hwcap: 0, x86_64_level: 1 }, |_| Ok(true))
            .expect_err("corrupt ld.so.cache must fail");
        assert!(matches!(err, Fault::Host { code: 22, .. }));
    }
}
