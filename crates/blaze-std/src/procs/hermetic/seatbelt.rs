//! [`Level::Inputs`](super::Level::Inputs) on macOS: Seatbelt (a thin driver).
//!
//! The plan (plan.rs) is rendered as an SBPL profile: deny by default; run only the program and
//! the named helpers (literals); read each plan rule (a literal for a file, a subpath for a
//! folder) and the metadata of each rule's ancestor folders (path resolution stats them); write
//! only beneath the output root and `/dev/null`; `sysctl-read` (the C runtime asks the page
//! size and the CPU count at start: it is the one host fact let through, and the CPU's model
//! with it); signals only within the sandbox. No network rule at all (every `network*`
//! operation is denied by the default), no `mach-lookup` (no daemon is reachable: the child
//! cannot ask a host service for anything), and `process-exec` only of the program's and the
//! helpers' literals (and beneath the helper folders); `process-fork` is allowed (a fork runs
//! the confined program again: [`PREAMBLE`] says why). The root folder itself is read (the
//! loader's). The fixed rules are [`PREAMBLE`], each with where it came from.
//!
//! The start. blaze-core's linked start is `posix_spawn` on macOS, so no code of blaze's runs
//! in the child before `execve`, and no `posix_spawn` attribute applies a Seatbelt profile
//! (the private `posix_spawnattr_setmacpolicyinfo_np` takes a MAC policy's opaque data, whose
//! form for the Sandbox policy is undocumented). So the profile is applied by
//! `/usr/bin/sandbox-exec -p <profile> <program> <args>`, which calls the private
//! `sandbox_init_with_parameters` (blaze-sys seatbelt.rs declares it, with its symbol test) and
//! then `execve`s the program confined. Its own refusals (a profile it cannot compile, an
//! `execve` denied) are written on the same standard error the program then has, before the
//! program exists: the run's kept standard error holds them, so an empty one means
//! `sandbox-exec` said nothing. `sandbox-exec` itself is the OS's own binary, run before
//! confinement; the program it runs is the plan's literal. It stays until blaze-core's macOS
//! start runs a step in the child (a `fork` path of its own, decided against for now: the
//! linked start stays `posix_spawn`).
//!
//! The budget is metered, not held: `proc_pid_rusage` of the child every
//! [`super::machine::METER_EVERY`], its group killed (`killpg`) when a cap is passed. The meter
//! reads the group's leader (the program; `sandbox-exec` became it by `execve`); a helper's use
//! is not in it.
//!
//! What this does not hold: the host's clock (a `gettimeofday` is the host's), metadata timing
//! (an mtime it stats under a root is the host file's), the CPU's model (`sysctl`), the kernel's
//! version (`uname` reads `kern.*` sysctls), the pid, and randomness (`getentropy`).

use core::time::Duration;

use blaze_own::{AVec, Alloc, planted::NoPlant};

use super::{
    Access, Fault, HermeticError, HermeticRun, HermeticSpec, Plan, Program, RootFiles, StartView, Usage,
    native::{Confiner, NativePort, run_spec},
    run_through,
};
use crate::{
    clock::Clock,
    cx::Cx,
    procs::{Run, Spawner, Started, Streams},
};

/// The loader and the system libraries, read whole: blaze-build-script's `MACOS_LOADER_READS`
/// verbatim (found by narrowing a `(deny default)` profile on macOS 27, arm64; the profile the
/// old native_sandbox.rs ran tempo's libproc, reth-mdbx-sys, sha3-asm and librocksdb-sys under).
/// `/usr/lib` holds dyld and libSystem; `/System/Library` the frameworks and `dyld/`; the
/// Cryptex the dyld shared cache.
pub const SYSTEM: [&str; 3] = ["/usr/lib", "/System/Library", "/System/Volumes/Preboot/Cryptexes/OS"];

/// What every profile allows before the plan's rules, each from the working profile
/// (blaze-build-script `seatbelt_with`, D65/D69):
/// - `process-fork`: a fork runs only the confined program again, under the same profile;
///   what it may then `execve` is the plan's literals alone (`process-exec`), so a start is
///   still limited to the program and the named helpers. (The working profile allowed it
///   always; the D177 move allowed it only with helpers, untested: put back.)
/// - `sysctl-read`: the C runtime asks the page size and the CPU count at start.
/// - `signal (target same-sandbox)`: a program signals itself and its own helpers (`abort`,
///   make's children); nothing outside the sandbox.
/// - `file-read*` of the root folder ITSELF (a literal: not what is under it): the loader reads
///   `/` before `main`. The D177 move allowed only its metadata; this is the one rule the
///   working profile had that the moved one lacked, and the likely reason a confined test binary
///   printed nothing at all (it never reached `main`).
const PREAMBLE: &[u8] = b"(version 1)\n(deny default)\n(allow process-fork)\n(allow sysctl-read)\n\
(allow signal (target same-sandbox))\n(allow file-read* (literal \"/\"))\n";

/// `sandbox-exec`, the OS's own.
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// `spec` run under Seatbelt.
pub(super) fn run<'a>(
    spawner: &Spawner,
    clock: &Clock,
    cx: &Cx,
    spec: &'a HermeticSpec<'a>,
    files: &dyn RootFiles,
    alloc: Alloc<'a>,
) -> Result<HermeticRun<'a>, HermeticError<'a>> {
    let mut port = NativePort::new(spawner, clock, cx, alloc, Seatbelt { timebase: blaze_sys::macos::timebase() });
    run_through(spec, &mut port, files, &SYSTEM, alloc, NoPlant)
}

/// The SBPL profile of `plan` (module docs), NUL-ended text in `alloc`.
pub fn profile<'a>(plan: &Plan<'_>, alloc: Alloc<'a>) -> Result<AVec<'a, u8>, Fault> {
    let mut out = AVec::new_in(alloc);
    let mut put = |bytes: &[u8]| out.try_extend_from_slice(bytes).map_err(|_| Fault::OutOfMemory);
    put(PREAMBLE)?;
    for rule in plan.rules() {
        let path = rule.path.as_bytes();
        let filter: &[u8] = if rule.subtree { b"subpath" } else { b"literal" };
        let ops: &[&[u8]] = match rule.access {
            Access::Exec => &[b"process-exec", b"file-read*"],
            Access::Read => &[b"file-read*"],
            Access::Write => &[b"file-read*", b"file-write*"],
        };
        for op in ops {
            put(b"(allow ")?;
            put(op)?;
            put(b" (")?;
            put(filter)?;
            put(b" \"")?;
            put(path)?;
            put(b"\"))\n")?;
        }
        // Each ancestor folder's metadata: resolving the path stats every folder above it.
        let mut end = 0;
        while let Some(at) = path[end + 1..].iter().position(|&b| b == b'/') {
            end += 1 + at;
            put(b"(allow file-read-metadata (literal \"")?;
            put(&path[..end])?;
            put(b"\"))\n")?;
        }
    }
    put(&[0])?;
    Ok(out)
}

/// The macOS confiner: `sandbox-exec` with the plan's profile, and the rusage meter.
struct Seatbelt {
    timebase: (u32, u32),
}

impl Confiner for Seatbelt {
    fn start(&mut self, spawner: &Spawner, cx: &Cx, start: &StartView<'_>, streams: Streams, alloc: Alloc<'_>) -> Result<Started<Run>, Fault> {
        let spec = start.spec;
        let plan = start.plan()?;
        let Program::Stored { path, .. } = spec.program() else {
            return Err(Fault::RecordsNeedExecution);
        };
        let profile = profile(plan, alloc)?;
        // The NUL ends the C string sandbox-exec is given; the argument is the text before it.
        let text = core::str::from_utf8(&profile[..profile.len() - 1]).map_err(|_| Fault::PathRefused { at: 0 })?;
        let mut args: AVec<'_, &str> = AVec::new_in(alloc);
        // No `--`: the working start was `sandbox-exec -p <profile> <program> <args>`, and the
        // program is an absolute path (the plan checks it), never read as an option.
        for arg in ["-p", text, path] {
            args.try_push(arg).map_err(|_| Fault::OutOfMemory)?;
        }
        for arg in spec.args() {
            args.try_push(arg).map_err(|_| Fault::OutOfMemory)?;
        }
        spawner.start(cx, &run_spec(SANDBOX_EXEC, &args, start, streams)).map_err(Fault::Run)
    }

    fn meter(&mut self, pid: i32) -> Result<Option<Usage>, Fault> {
        let info = blaze_sys::seatbelt::child_usage(pid).map_err(|errno| Fault::Host { call: "proc_pid_rusage", code: errno.raw() })?;
        let ticks = info.words[blaze_sys::macos::RusageInfoV4::USER].saturating_add(info.words[blaze_sys::macos::RusageInfoV4::SYSTEM]);
        let (numer, denom) = self.timebase;
        let nanos = u128::from(ticks) * u128::from(numer) / u128::from(denom.max(1));
        // Saturated: a CPU time past u64 nanoseconds (584 years) is past every budget.
        let cpu = Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX));
        let memory = info.words[blaze_sys::macos::RusageInfoV4::FOOTPRINT];
        Ok(Some(Usage { cpu, memory, steps: None }))
    }
}
