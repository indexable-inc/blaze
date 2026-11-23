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
//! The budget is metered, not held: `proc_listpgrppids` enumerates the live processes in the
//! child's process group every [`super::machine::METER_EVERY`], `proc_pid_rusage` sums each
//! member's self + reaped-child user/system CPU time and physical footprint (treating `ESRCH` on
//! an individual PID as a benign exit race), and its group is killed (`killpg`) when a cap is
//! passed.
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

/// The loader and the system libraries, read whole (from `blaze_roots::seatbelt::MACOS_SYSTEM`).
pub const SYSTEM: [&str; 3] = blaze_roots::seatbelt::MACOS_SYSTEM;

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
    let mut port = NativePort::new(
        spawner,
        clock,
        cx,
        alloc,
        Seatbelt { timebase: blaze_sys::macos::timebase(), peak_cpu: Duration::ZERO },
    );
    run_through(spec, &mut port, files, &SYSTEM, alloc, NoPlant)
}

/// The SBPL profile of `plan` (module docs), NUL-ended text in `alloc`.
pub fn profile<'a>(plan: &Plan<'_>, alloc: Alloc<'a>) -> Result<AVec<'a, u8>, Fault> {
    use blaze_roots::seatbelt::{SbplAccess, SbplError, SbplRule, render_sbpl};
    let rules = plan.rules().map(|rule| SbplRule {
        path: rule.path,
        subtree: rule.subtree,
        access: match rule.access {
            Access::Read => SbplAccess::Read,
            Access::Write => SbplAccess::Write,
            Access::Exec => SbplAccess::Exec,
        },
    });
    render_sbpl(rules, alloc).map_err(|err| match err {
        SbplError::OutOfMemory => Fault::OutOfMemory,
        SbplError::BadPath { at } => Fault::PathRefused { at },
    })
}

/// The macOS confiner: `sandbox-exec` with the plan's profile, and the process-group rusage meter.
struct Seatbelt {
    timebase: (u32, u32),
    /// Highest group CPU time observed so far (monotonic even if a reparented helper exits).
    peak_cpu: Duration,
}

/// Maximum live PIDs sampled per process group in one meter tick (bounded stack buffer, rule 18).
const GROUP_PID_CAP: usize = 256;

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
        let mut pids = [0i32; GROUP_PID_CAP];
        let count = match blaze_sys::seatbelt::group_pids(pid, &mut pids) {
            Ok(count) => count,
            // Benign exit race: the process group disappeared before metering ran.
            Err(blaze_sys::Errno::Srch) => 0,
            Err(errno) => return Err(Fault::Host { call: "proc_listpgrppids", code: errno.raw() }),
        };
        let listed = &pids[..count];
        let mut total_ticks: u64 = 0;
        let mut total_memory: u64 = 0;
        let mut leader_seen = false;

        for &member in listed {
            if member == pid {
                leader_seen = true;
            }
            match blaze_sys::seatbelt::child_usage(member) {
                Ok(info) => {
                    total_ticks = total_ticks.saturating_add(info.total_cpu_ticks());
                    total_memory = total_memory.saturating_add(info.footprint());
                }
                // Benign exit race: a process exited between enumeration and rusage query.
                Err(blaze_sys::Errno::Srch) => continue,
                Err(errno) => return Err(Fault::Host { call: "proc_pid_rusage", code: errno.raw() }),
            }
        }
        if !leader_seen {
            match blaze_sys::seatbelt::child_usage(pid) {
                Ok(info) => {
                    total_ticks = total_ticks.saturating_add(info.total_cpu_ticks());
                    total_memory = total_memory.saturating_add(info.footprint());
                }
                Err(blaze_sys::Errno::Srch) => {}
                Err(errno) => return Err(Fault::Host { call: "proc_pid_rusage", code: errno.raw() }),
            }
        }

        let (numer, denom) = self.timebase;
        let nanos = u128::from(total_ticks) * u128::from(numer) / u128::from(denom.max(1));
        // Saturated: a CPU time past u64 nanoseconds (584 years) is past every budget.
        let cpu = Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX));
        self.peak_cpu = self.peak_cpu.max(cpu);
        Ok(Some(Usage { cpu: self.peak_cpu, memory: total_memory, steps: None }))
    }
}
