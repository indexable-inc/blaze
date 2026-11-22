//! THE GRANTS of one compile-time run: everything it may reach, as one value. Nothing else
//! exists for it: no file outside [`Grants::roots`], no variable outside [`Grants::env`], no
//! clock but [`Clock`], no randomness but the stream of [`Seed`], no network, no thread, no
//! command unless [`Spawns::CMode`]. A proc macro's grants and a build script's are the two
//! constructors of the one type ([`Grants::proc_macro`], [`Grants::build_script`]).

#![blaze::sans_io]

use blaze_roots::{Mounts, Roots};

/// The call's key: the expansion's or script's base key, computed before the run by the code
/// that keys it (blaze-proc-macro record.rs `base_key`, blaze-build-script key.rs). The seed is
/// derived from it, so it must not depend on the run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CallKey(pub [u8; 32]);

/// The random stream's seed: BLAKE3's key derivation of the call's key under the context
/// [`Seed::CONTEXT`]. A macro that asks for randomness gets the same bytes every time (rule 25),
/// and the run is still keepable (today a run that reads randomness is not kept:
/// proc-macros.md "Keys"). The stream is BLAKE3's extendable output of the seed: the bytes at
/// offsets `[n, n + len)` answer the call that asks for `len` after `n` were given.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Seed(pub [u8; 32]);

impl Seed {
    pub const CONTEXT: &'static str = "blaze-comptime 2026-11-20 random stream of a call";

    pub fn of(key: CallKey) -> Seed {
        Seed(blaze_blake3::derive_key(Seed::CONTEXT, &key.0))
    }
}

/// The clock a run reads: fixed values, the same on every run. Both are 0, which is what the
/// wasm host answers today for every clock (blaze-proc-macro sandbox.rs `clock_time_get`,
/// script_sandbox.rs the same), so the two engines agree; a wall clock of 0 is 1970-01-01, the
/// `SOURCE_DATE_EPOCH` reproducible builds use when none is given.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Clock {
    pub wall_ns: u64,
    pub monotonic_ns: u64,
}

impl Clock {
    pub const FIXED: Clock = Clock { wall_ns: 0, monotonic_ns: 0 };
}

/// The run's bound (rule 18; the owner's decision of 2026-11-20). The driver meters the
/// child's RETIRED INSTRUCTIONS where the kernel gives them (macOS: `proc_pid_rusage(V4)`'s
/// `ri_instructions`, which blaze-meter os.rs reads for a process; Linux: a counting
/// `perf_event_open` on the child where permitted) and delivers them as
/// [`crate::Event::Retired`]; where it cannot, the child's CPU time as the kernel accounts it
/// ([`crate::Event::Cpu`]; the kernel's `RLIMIT_CPU` one second above it as the hard wall).
/// Either one past its cap stops the run, as does the count of calls; the wall deadline is
/// [`Deadline`]. Fuel is a bound and never part of a key or a record: a stop by fuel is a typed
/// end, so the meter's inexactness decides only whether a run near its budget finishes, never
/// what a finished run's output is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fuel {
    pub instructions: u64,
    pub cpu_ns: u64,
    pub calls: u32,
}

impl Fuel {
    /// A proc macro: 2^34 instructions (Wasmtime's fuel today, `host.rs:105` of
    /// blaze-proc-macro), else 10 s of CPU, and 2^22 calls.
    pub const MACRO: Fuel = Fuel { instructions: 1 << 34, cpu_ns: 10_000_000_000, calls: 1 << 22 };
    /// A build script: 2^36 instructions (the script host's today), else 60 s of CPU, and
    /// 2^24 calls.
    pub const SCRIPT: Fuel = Fuel { instructions: 1 << 36, cpu_ns: 60_000_000_000, calls: 1 << 24 };
}

/// The wall-clock bound, from the run's start on the request clock (nanoseconds). The wasm path
/// has none today (comptime-native.md, part 1, A4): a guest that blocks in a host call is
/// bounded only by its fuel there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Deadline {
    pub within_ns: u64,
}

impl Deadline {
    pub const MACRO: Deadline = Deadline { within_ns: 60_000_000_000 };
    pub const SCRIPT: Deadline = Deadline { within_ns: 300_000_000_000 };
}

/// One variable's grant.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EnvValue<'g> {
    Set(&'g [u8]),
    /// Known to be unset (`PATH`, `HOME`, `CARGO` for a macro: blaze-proc-macro package_env.rs).
    /// A variable that exists but is not yet known (a macro's `OUT_DIR` before its crate's
    /// script ran) has no form here on purpose: the driver starts the run only once every
    /// granted value is known, so no run answers "unset" for a value it lacked.
    Unset,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EnvGrant<'g> {
    pub name: &'g [u8],
    pub value: EnvValue<'g>,
}

/// Whether the run may run commands.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Spawns {
    /// No command: a spawn is answered "not found" and recorded as a refusal.
    None,
    /// A build script's commands go to the build-script host's routing (blaze-proc-macro
    /// spawn_route.rs: the probe table, `cargo -V`, `git`, derived facts, then the C mode of
    /// D174, which turns a compile into a unit task); nothing runs the machine's programs.
    CMode,
}

/// The output's caps: what the run may hand back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OutputCaps {
    /// A proc macro's output stream (2^28 today, host.rs `Limits.output`) or a build script's
    /// standard output (16 MiB today, build-script key.rs).
    pub most_out: u32,
    /// The standard error kept (its last bytes: 64 KiB today, script_sandbox.rs).
    pub most_log: u32,
    /// Files in the output root (4096 today) and their bytes together (64 MiB today).
    pub most_files: u32,
    pub most_file_bytes: u64,
}

impl OutputCaps {
    pub const MACRO: OutputCaps = OutputCaps { most_out: 1 << 28, most_log: 64 << 10, most_files: 0, most_file_bytes: 0 };
    pub const SCRIPT: OutputCaps = OutputCaps { most_out: 16 << 20, most_log: 64 << 10, most_files: 4096, most_file_bytes: 64 << 20 };
}

/// What the run is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunKind<'g> {
    /// One proc-macro call: `name` the macro, `input` the call's tokens and `attr` an
    /// attribute's, each in the bridge's wire, as the server was given them.
    ProcMacro { name: &'g [u8], input: &'g [u8], attr: Option<&'g [u8]> },
    /// One build script run.
    BuildScript,
}

/// The grants of one compile-time run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Grants<'g> {
    pub kind: RunKind<'g>,
    pub key: CallKey,
    /// What it may read: each root read-only but the one output root (blaze-roots).
    pub roots: Roots<'g>,
    /// The spellings it sees for its roots, and its working directory (never in a key).
    pub mounts: Mounts<'g>,
    /// Every variable it can see, sorted by name bytewise, each once (checked by the
    /// constructors). A name not here is unset to the run, and asking it is a refusal.
    pub env: &'g [EnvGrant<'g>],
    /// Its arguments (a build script: `build-script-build`; a macro: none).
    pub args: &'g [&'g [u8]],
    pub spawns: Spawns,
    pub output: OutputCaps,
    pub fuel: Fuel,
    pub deadline: Deadline,
    pub seed: Seed,
    pub clock: Clock,
    /// The read list's caps.
    pub limits: blaze_roots::Limits,
}

/// Why grants were refused at construction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GrantsError {
    /// The variables are not sorted bytewise, or one is there twice (at index `at`).
    EnvOrder { at: usize },
    /// A variable's name is empty or holds `=` or a NUL, or its value holds a NUL.
    EnvName { at: usize },
    /// A build script was given no output root (`OUT_DIR`).
    NoOutput,
    /// A proc macro was given an output root: a macro writes nothing.
    MacroOutput,
}

/// A proc macro's grants (the derive's case: `roots` and `mounts` empty, the env its package's;
/// `sol!`'s case: the package's root, so files it includes by relative path resolve under the
/// working directory, which is the package's mount).
pub struct ProcMacroOpts<'g> {
    pub key: CallKey,
    pub name: &'g [u8],
    pub input: &'g [u8],
    pub attr: Option<&'g [u8]>,
    pub roots: Roots<'g>,
    pub mounts: Mounts<'g>,
    pub env: &'g [EnvGrant<'g>],
    pub fuel: Fuel = Fuel::MACRO,
    pub deadline: Deadline = Deadline::MACRO,
    pub output: OutputCaps = OutputCaps::MACRO,
    pub limits: blaze_roots::Limits = blaze_roots::Limits::SHIPPED,
}

/// A build script's grants: its package tree (and its `links` dependencies' views) as roots,
/// `OUT_DIR` as the output root, the variables cargo defines (blaze-build-script env.rs), its
/// one argument, and the C mode's spawns.
pub struct BuildScriptOpts<'g> {
    pub key: CallKey,
    pub roots: Roots<'g>,
    pub mounts: Mounts<'g>,
    pub env: &'g [EnvGrant<'g>],
    pub args: &'g [&'g [u8]] = &[b"build-script-build"],
    pub spawns: Spawns = Spawns::CMode,
    pub fuel: Fuel = Fuel::SCRIPT,
    pub deadline: Deadline = Deadline::SCRIPT,
    pub output: OutputCaps = OutputCaps::SCRIPT,
    pub limits: blaze_roots::Limits = blaze_roots::Limits::SHIPPED,
}

impl<'g> Grants<'g> {
    pub fn proc_macro(opts: ProcMacroOpts<'g>) -> Result<Grants<'g>, GrantsError> {
        check_env(opts.env)?;
        if opts.roots.output().is_some() {
            return Err(GrantsError::MacroOutput);
        }
        Ok(Grants {
            kind: RunKind::ProcMacro { name: opts.name, input: opts.input, attr: opts.attr },
            key: opts.key,
            roots: opts.roots,
            mounts: opts.mounts,
            env: opts.env,
            args: &[],
            spawns: Spawns::None,
            output: opts.output,
            fuel: opts.fuel,
            deadline: opts.deadline,
            seed: Seed::of(opts.key),
            clock: Clock::FIXED,
            limits: opts.limits,
        })
    }

    pub fn build_script(opts: BuildScriptOpts<'g>) -> Result<Grants<'g>, GrantsError> {
        check_env(opts.env)?;
        if opts.roots.output().is_none() {
            return Err(GrantsError::NoOutput);
        }
        Ok(Grants {
            kind: RunKind::BuildScript,
            key: opts.key,
            roots: opts.roots,
            mounts: opts.mounts,
            env: opts.env,
            args: opts.args,
            spawns: opts.spawns,
            output: opts.output,
            fuel: opts.fuel,
            deadline: opts.deadline,
            seed: Seed::of(opts.key),
            clock: Clock::FIXED,
            limits: opts.limits,
        })
    }

    /// The grant of the variable `name`, if any (binary search: the table is sorted).
    pub fn env_of(&self, name: &[u8]) -> Option<EnvValue<'g>> {
        self.env.binary_search_by(|grant| grant.name.cmp(name)).ok().map(|at| self.env[at].value)
    }

    pub fn is_macro(&self) -> bool {
        matches!(self.kind, RunKind::ProcMacro { .. })
    }
}

fn check_env(env: &[EnvGrant<'_>]) -> Result<(), GrantsError> {
    for (at, grant) in env.iter().enumerate() {
        if grant.name.is_empty() || grant.name.contains(&b'=') || grant.name.contains(&0) {
            return Err(GrantsError::EnvName { at });
        }
        if let EnvValue::Set(value) = grant.value
            && value.contains(&0)
        {
            return Err(GrantsError::EnvName { at });
        }
        if at > 0 && env[at - 1].name >= grant.name {
            return Err(GrantsError::EnvOrder { at });
        }
    }
    Ok(())
}
