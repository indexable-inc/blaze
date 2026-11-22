//! A build script's run: its `wasm32-wasip1` module (`_start`, built with the process shim
//! `guest/script_process.rs`) in a fresh Wasmtime instance over the [`ScriptSandbox`], within
//! fuel and memory limits (docs/v2/plan/design/build-scripts.md).
//!
//! - The run's base is [`blaze_build_script::key::base`]: the module's SHA-256, the whole
//!   environment, the answers it could be given, the C mode and the limits; it seeds a spawn's
//!   clock and randomness. Its reads are what the script read of its workspace (`file:`, `dir:`,
//!   `stat:` subjects, valued by [`crate::script_sandbox::answer_now`]).
//! - The answer is [`Outputs`]: the instructions, the files of `OUT_DIR` and the C requests
//!   deferred. A failed run gives a [`ScriptError`] naming the package.
//! - Nothing is kept here: the only cache of a run is the command's `Script` task record
//!   (decision D115, docs/v2/plan/design/drive.md, "Two rulings (19:48)"), whose read list the
//!   task makes from [`ScriptRun::reads`].
//!
//! The compiled-for-host module is `<store>/build-script/compiled/<key>/code`.

use std::{
    hash::{Hash as _, Hasher as _},
    path::{Path, PathBuf},
    sync::Arc,
};

use blaze_build_script::{
    Env, Outputs,
    instr::{OutputError, OutputLimits},
    key::{self, KeyInputs, RunnerKey},
    outputs::{FileError, FileLimits},
    probe::Table,
};
use blaze_ids::ContentId;
use blaze_own::Alloc;
use blaze_wasi::{Answer, Errno, Stop, W32};
use wasmtime::{Caller, Config, Engine, InstancePre, Linker, Memory, Module, OptLevel, ResourceLimiter, Store as WasmStore, Trap, TypedFunc, WasmBacktraceDetails};

use crate::{
    host::{MAX_WASM_STACK, fuel_cost},
    module::sha256_hex,
    script_sandbox::{Read, Refusal, Script, ScriptSandbox},
};

/// The import module of the process shim's calls.
const IMPORTS: &str = "blaze_build_script";

/// WASI's module, for the calls `blaze_wasi` leaves out (the writes `OUT_DIR` takes).
const WASI: &str = "wasi_snapshot_preview1";

/// The most elements a module's tables may hold.
const TABLE_MAX: usize = 1 << 20;

/// Which limit stopped a script.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScriptLimit {
    Fuel,
    Memory,
    OutDir,
    Output,
}

/// Why a script gave no record.
#[derive(Debug)]
pub enum ScriptWhy {
    /// It ran past a limit.
    Limit(ScriptLimit, String),
    /// It did what the sandbox refuses: a path out, a spawn blaze does not answer, a wait.
    Refused(Refusal),
    /// It exited with a code other than 0: the last of its standard error.
    Exit { code: u32, stderr: String },
    /// It trapped (a panic is an abort under `panic=abort`): the last of its standard error.
    Trapped { why: String, stderr: String },
    /// It failed (an exit other than 0, or a trap) after asking a native tool `check` defers
    /// (a C compiler, `perl`, `configure`, `pkg-config`: D53), which answered with nothing: it
    /// cannot produce outputs in check mode. `request` is the last such spawn, its argv joined
    /// by spaces; `failure` the exit code or the trap; `stderr` the end of its standard error.
    NeedsDeferred { request: String, failure: String, stderr: String },
    /// Its output is not instructions within the limits.
    Output(OutputError),
    /// A file of `OUT_DIR` the record does not take.
    File { name: String, error: FileError },
    /// The module does not load or link.
    Load(String),
}

/// A [`ScriptWhy`] with the package it is about.
#[derive(Debug)]
pub struct ScriptError {
    /// `name version`.
    pub package: String,
    pub why: ScriptWhy,
}

impl std::fmt::Display for ScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let package = &self.package;
        match &self.why {
            ScriptWhy::Limit(limit, why) => write!(f, "the build script of {package} ran past its {limit:?} limit: {why}"),
            ScriptWhy::Refused(refusal) => write!(f, "the build script of {package} was stopped: {refusal:?}"),
            ScriptWhy::Exit { code, stderr } => write!(f, "the build script of {package} exited with code {code}; its standard error ends:\n{stderr}"),
            ScriptWhy::Trapped { why, stderr } => write!(f, "the build script of {package} trapped: {why}; its standard error ends:\n{stderr}"),
            ScriptWhy::NeedsDeferred { request, failure, stderr } => write!(
                f,
                "the build script of {package} cannot produce outputs in check mode: after `{request}`, a native tool check defers (D53) whose answer or files it may need, it {failure}; its standard error ends:\n{stderr}"
            ),
            ScriptWhy::Output(error) => write!(f, "the build script of {package} printed what blaze does not take: {error:?}"),
            ScriptWhy::File { name, error } => write!(f, "the build script of {package} left OUT_DIR/{name}, which the record does not take: {error:?}"),
            ScriptWhy::Load(why) => write!(f, "the build script of {package} does not load: {why}"),
        }
    }
}

impl std::error::Error for ScriptError {}

/// Why the runtime could not be made.
#[derive(Debug)]
pub enum RuntimeError {
    /// Wasmtime made no engine of the configuration.
    Engine(wasmtime::Error),
    /// The host's calls did not link.
    Link(wasmtime::Error),
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RuntimeError::Engine(error) => write!(f, "Wasmtime could not make the build-script engine: {error:#}"),
            RuntimeError::Link(error) => write!(f, "the build-script host's calls could not be linked: {error:#}"),
        }
    }
}

impl std::error::Error for RuntimeError {}

/// The engine and linker build scripts run in, and the store their compiled modules are kept in.
pub struct ScriptRuntime {
    engine: Engine,
    linker: Linker<ScriptHost>,
    code_id: String,
    store: PathBuf,
}

/// A script's module compiled for this host.
pub struct LoadedScript {
    pre: InstancePre<ScriptHost>,
    /// The SHA-256 of the module's bytes, which the run's key holds.
    pub sha256: [u8; 32],
    /// Whether this load compiled it.
    pub compiled: bool,
}

/// What one run gave.
pub struct ScriptRun<'a> {
    pub outputs: Outputs<'a>,
    /// The run's base key.
    pub base: ContentId,
    /// What it read.
    pub reads: Vec<Read>,
    pub fuel: u64,
    pub peak_memory: usize,
    pub spawns: u32,
}

/// A store's data: one run's sandbox.
pub struct ScriptHost {
    sandbox: ScriptSandbox,
    memory: Option<Memory>,
    limiter: Limiter,
    stop: Option<Stop>,
}

struct Limiter {
    memory: usize,
    peak: usize,
    over: bool,
}

impl ResourceLimiter for Limiter {
    fn memory_growing(&mut self, _current: usize, desired: usize, _maximum: Option<usize>) -> wasmtime::Result<bool> {
        if desired > self.memory {
            self.over = true;
            return Ok(false);
        }
        self.peak = self.peak.max(desired);
        Ok(true)
    }

    fn table_growing(&mut self, _current: usize, desired: usize, _maximum: Option<usize>) -> wasmtime::Result<bool> {
        Ok(desired <= TABLE_MAX)
    }
}

/// Runs `f` on the module's memory and the run's host; a [`Stop`] it returns is kept and ends
/// the instance.
fn with<R>(caller: &mut Caller<'_, ScriptHost>, f: impl FnOnce(&mut [u8], &mut ScriptHost) -> Result<R, Stop>) -> wasmtime::Result<R> {
    let Some(memory) = caller.data().memory else {
        return Err(wasmtime::Error::msg("the script called the host before its memory was found"));
    };
    let (mem, host) = memory.data_and_store_mut(caller);
    match f(mem, host) {
        Ok(it) => Ok(it),
        Err(stop) => {
            host.stop.get_or_insert(stop);
            Err(wasmtime::Error::msg("a call of the host stopped the build script"))
        }
    }
}

impl blaze_wasi::Embed for ScriptHost {
    fn with<R>(
        caller: &mut Caller<'_, ScriptHost>,
        f: impl FnOnce(&mut [u8], &mut dyn blaze_wasi::System) -> Result<R, Stop>, // dyn: `blaze_wasi`'s calls, shared by every embedder
    ) -> wasmtime::Result<R> {
        with(caller, |mem, host| f(mem, &mut host.sandbox))
    }
}

/// The WASI errno of an answer with no value.
fn errno(answer: Answer<()>) -> Result<i32, Stop> {
    Ok(match answer? {
        Ok(()) => 0,
        Err(Errno(code)) => i32::from(code),
    })
}

fn imports(l: &mut Linker<ScriptHost>) -> wasmtime::Result<()> {
    // The shim's lookup before a spawn (guest/script_process.rs `Command::spawn`): 1 when the
    // machine has the program, 0 for the OS's `NotFound` (D192). A module built before the
    // shim asked does not import it, and the linker's unused definition is nothing to it.
    l.func_wrap(IMPORTS, "found", |mut c: Caller<'_, ScriptHost>, program: u32, len: u32| {
        with(&mut c, |mem, host| {
            let program = blaze_wasi::bytes(mem, u64::from(program), u64::from(len))?.to_vec();
            Ok(u32::from(host.sandbox.found(&program)?))
        })
    })?;
    l.func_wrap(IMPORTS, "spawn", |mut c: Caller<'_, ScriptHost>, request: u32, len: u32| {
        with(&mut c, |mem, host| {
            let request = blaze_wasi::bytes(mem, u64::from(request), u64::from(len))?.to_vec();
            host.sandbox.spawn(&request)?;
            Ok(host.sandbox.reply.len() as u32)
        })
    })?;
    l.func_wrap(IMPORTS, "reply", |mut c: Caller<'_, ScriptHost>, into: u32| {
        with(&mut c, |mem, host| {
            blaze_wasi::put(mem, u64::from(into), &host.sandbox.reply)?;
            host.sandbox.reply.clear();
            Ok(())
        })
    })?;
    l.func_wrap(WASI, "path_create_directory", |mut c: Caller<'_, ScriptHost>, fd: u32, path: u32, len: u32| {
        with(&mut c, |mem, host| {
            let path = blaze_wasi::bytes(mem, u64::from(path), u64::from(len))?.to_vec();
            errno(host.sandbox.create_dir(fd, &path))
        })
    })?;
    // `fd_sync` and `fd_datasync` (cc syncs the files it writes): a script's files are the
    // sandbox's memory, which no crash tears, so a sync of an open descriptor is done once it is
    // known to be one. Scripts only: a proc macro writes nothing and is refused these.
    for name in ["fd_sync", "fd_datasync"] {
        l.func_wrap(WASI, name, |mut c: Caller<'_, ScriptHost>, fd: u32| {
            with(&mut c, |_, host| errno(blaze_wasi::System::fd_filetype(&mut host.sandbox, fd).map(|it| it.map(|_| ()))))
        })?;
    }
    // `path_link` (std's `fs::hard_link`, which cc imports): `OUT_DIR` holds files, not links,
    // so the sandbox answers ENOTSUP, a WASI errno the script sees and handles as on a file
    // system without hard links (cc then copies).
    l.func_wrap(WASI, "path_link", |_: Caller<'_, ScriptHost>, _: u32, _: u32, _: u32, _: u32, _: u32, _: u32, _: u32| -> i32 {
        i32::from(Errno::NOTSUP.0)
    })?;
    for (name, dir) in [("path_unlink_file", false), ("path_remove_directory", true)] {
        l.func_wrap(WASI, name, move |mut c: Caller<'_, ScriptHost>, fd: u32, path: u32, len: u32| {
            with(&mut c, |mem, host| {
                let path = blaze_wasi::bytes(mem, u64::from(path), u64::from(len))?.to_vec();
                errno(host.sandbox.remove(fd, &path, dir))
            })
        })?;
    }
    l.func_wrap(
        WASI,
        "path_rename",
        |mut c: Caller<'_, ScriptHost>, fd: u32, from: u32, from_len: u32, to_fd: u32, to: u32, to_len: u32| {
            with(&mut c, |mem, host| {
                let from = blaze_wasi::bytes(mem, u64::from(from), u64::from(from_len))?.to_vec();
                let to = blaze_wasi::bytes(mem, u64::from(to), u64::from(to_len))?.to_vec();
                errno(host.sandbox.rename(fd, &from, to_fd, &to))
            })
        },
    )?;
    l.func_wrap(WASI, "fd_filestat_set_size", |mut c: Caller<'_, ScriptHost>, fd: u32, size: u64| {
        with(&mut c, |_, host| errno(host.sandbox.set_size(fd, size)))
    })?;
    Ok(())
}

/// The bytes a `Hash` writes, in order.
#[derive(Default)]
struct Bytes(Vec<u8>);

impl std::hash::Hasher for Bytes {
    fn finish(&self) -> u64 {
        let digest = blaze_sha256::digest(&self.0);
        let mut word = [0; 8];
        word.copy_from_slice(&digest[..8]);
        u64::from_le_bytes(word)
    }

    fn write(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }
}

impl ScriptRuntime {
    /// The runtime keeping compiled modules under `store` (`build-script/compiled`).
    pub fn new(store: &Path) -> Result<ScriptRuntime, RuntimeError> {
        let mut config = Config::new();
        config
            .cranelift_opt_level(OptLevel::Speed)
            // A NaN's bits are the same on every host, so a script that prints a float gives
            // the same output wherever it ran.
            .cranelift_nan_canonicalization(true)
            .consume_fuel(true)
            .operator_cost(fuel_cost())
            .max_wasm_stack(MAX_WASM_STACK)
            .async_stack_size(MAX_WASM_STACK)
            .wasm_backtrace_details(WasmBacktraceDetails::Disable);
        let engine = Engine::new(&config).map_err(RuntimeError::Engine)?;
        let mut bytes = Bytes::default();
        bytes.write(b"blaze build-script host 1\n");
        engine.precompile_compatibility_hash().hash(&mut bytes);
        bytes.write(std::env::consts::ARCH.as_bytes());
        bytes.write(std::env::consts::OS.as_bytes());
        let code_id = sha256_hex(&bytes.0);
        let mut linker = Linker::new(&engine);
        imports(&mut linker).map_err(RuntimeError::Link)?;
        blaze_wasi::link::<W32, ScriptHost>(&mut linker).map_err(RuntimeError::Link)?;
        Ok(ScriptRuntime { engine, linker, code_id, store: store.to_owned() })
    }

    /// `wasm`, the module of `package`'s script, compiled for this host: from its compiled
    /// record, or compiled and the record written.
    #[allow(unsafe_code)]
    pub fn load(&self, package: &str, wasm: &[u8]) -> Result<LoadedScript, ScriptError> {
        let failed = |why: String| ScriptError { package: package.to_owned(), why: ScriptWhy::Load(why) };
        let sha256 = blaze_sha256::digest(wasm);
        let key = sha256_hex(format!("{}\n{}\n", sha256_hex(wasm), self.code_id).as_bytes());
        let dir = self.store.join("build-script/compiled").join(key);
        let path = dir.join("code");
        let (compiled, compiled_now) = if path.exists() {
            // SAFETY: `deserialize_file` runs the file as machine code, checking only the
            // engine's version and settings. The file is this host's own compiled record: only
            // `load` writes it (below), from `Module::serialize` of this engine, under a key
            // holding the engine's code id; the store's folder is the user's own.
            let module = unsafe { Module::deserialize_file(&self.engine, &path) };
            (module.map_err(|e| failed(format!("its compiled record {} does not load: {e:#}", path.display())))?, false)
        } else {
            let compiled = Module::new(&self.engine, wasm).map_err(|e| failed(format!("{e:#}")))?;
            let code = compiled.serialize().map_err(|e| failed(format!("{e:#}")))?;
            let io = |path: &Path, error: std::io::Error| failed(format!("{}: {error}", path.display()));
            // A temporary name of its own a write, then a rename (blaze-core fs.rs
            // `write_unsynced`): two scripts of one process loading the same module side by
            // side (lane CX25) each write their own file, where `code.<pid>.tmp` was one name
            // for both and a reader could map a file another thread was still writing.
            blaze_core::fs::write_unsynced(&path, &code).map_err(|e| io(&path, e))?;
            (compiled, true)
        };
        let pre = self.linker.instantiate_pre(&compiled).map_err(|e| failed(format!("{e:#}")))?;
        Ok(LoadedScript { pre, sha256, compiled: compiled_now })
    }

    /// The record's base key of `script` run by `loaded` with `env`.
    pub fn base(&self, loaded: &LoadedScript, script: &Script, env: &Env<'_>) -> ContentId {
        // This runner is the wasm one: a native script never reaches it (D65).
        let inputs = KeyInputs {
            module: loaded.sha256,
            env,
            // The checker's identity when it answers the probes (script_probes.rs), else the
            // pinned table's: what the run's probe answers are a function of (rule 21).
            answers: match &script.probes {
                Some(probes) => probes.id().0,
                None => Table { rows: &script.table }.id(),
            },
            c_mode: script.c_mode,
            limits: script.limits,
            runner: RunnerKey::Wasm,
        };
        let mut scratch = blaze_own::Vec::new_in(Alloc::global());
        // Brief S1: the `links` dependencies' files the sandbox serves are not text of this
        // script; their content is in the key (`DepViews::key`: the base itself with none).
        script.deps.key(key::base(&inputs, &mut scratch))
    }

    /// `script` run by `loaded`: its outputs and reads. `env` is the environment `script.env`
    /// was made from. Nothing is looked up or kept (the `Script` task's record is the cache).
    pub fn run<'a>(&self, loaded: &LoadedScript, script: Arc<Script>, env: &Env<'_>, alloc: Alloc<'a>) -> Result<ScriptRun<'a>, ScriptError> {
        let base = self.base(loaded, &script, env);
        self.fresh(loaded, script, base, alloc)
    }

    /// `script` run by `loaded` under `base`: its outputs and reads.
    pub(crate) fn fresh<'a>(&self, loaded: &LoadedScript, script: Arc<Script>, base: ContentId, alloc: Alloc<'a>) -> Result<ScriptRun<'a>, ScriptError> {
        // A spawn's clock and randomness are seeded by the run's base key (W2): the copy is
        // made only when the caller's `Script` does not hold it yet.
        let mut script = script;
        if script.step != base {
            Arc::make_mut(&mut script).step = base;
        }
        let package = script.package.clone();
        let named = |why: ScriptWhy| ScriptError { package: package.clone(), why };
        let limits = script.limits;
        let out_dir = script.out_dir().to_owned();
        let host = ScriptHost {
            sandbox: ScriptSandbox::new(Arc::clone(&script)),
            memory: None,
            limiter: Limiter { memory: usize::try_from(limits.memory).unwrap_or(usize::MAX), peak: 0, over: false },
            stop: None,
        };
        let mut store = WasmStore::new(&self.engine, host);
        store.limiter(|host| &mut host.limiter);
        store.set_fuel(limits.fuel).map_err(|e| named(ScriptWhy::Load(format!("{e:#}"))))?;
        let ran = start(&mut store, loaded);
        let fuel = limits.fuel.saturating_sub(store.get_fuel().unwrap_or(0));
        let mut host = store.into_data();
        if let Err(error) = ran {
            stopped_by(&mut host, &error, fuel >= limits.fuel).map_err(named)?;
        }
        let sandbox = &host.sandbox;
        let output_limits = OutputLimits { line: limits.line as usize, output: limits.output as usize };
        let mut outputs = Outputs::read(&sandbox.stdout, &out_dir, output_limits, alloc).map_err(|e| named(ScriptWhy::Output(e)))?;
        let file_limits = FileLimits { bytes: limits.out_dir_bytes, files: limits.out_dir_files as usize };
        for (name, bytes) in sandbox.out_files() {
            outputs.add_file(name, bytes, file_limits, alloc).map_err(|error| named(ScriptWhy::File { name: name.to_owned(), error }))?;
        }
        c_inputs(sandbox, &mut outputs, alloc);
        let reads = std::mem::take(&mut host.sandbox.reads);
        Ok(ScriptRun { outputs, base, reads, fuel, peak_memory: host.limiter.peak, spawns: host.sandbox.spawns })
    }
}

/// The sandbox's deferred requests and the `OUT_DIR` states they saw, into `outputs`, after
/// its files (blaze-build-script outputs.rs, THE C INPUTS): a state's file whose bytes are the
/// record's own file of that name is [`Snap::Final`], any other held once by digest.
pub(crate) fn c_inputs<'a>(sandbox: &ScriptSandbox, outputs: &mut Outputs<'a>, alloc: Alloc<'a>) {
    use blaze_build_script::outputs::{CRequest, CState, CTree, Snap, snap_digest};
    for (argv, state) in &sandbox.c_requests {
        outputs.c_requests.push(CRequest { argv: blaze_own::String::from_str_in(argv, alloc), state: *state });
    }
    // Each process tree's folder and environment, by ascending request (outputs.rs `CTree`).
    for (request, cwd, env) in &sandbox.c_trees {
        let mut owned = blaze_own::Vec::with_capacity_in(env.len(), alloc);
        for (name, value) in env {
            owned.push((blaze_own::String::from_str_in(name, alloc), blaze_own::String::from_str_in(value, alloc)));
        }
        outputs.c_trees.push(CTree { request: *request, cwd: blaze_own::String::from_str_in(cwd, alloc), env: owned });
    }
    // Each final file's digest, taken once and only for a name a state holds.
    let mut finals: std::collections::BTreeMap<&str, Option<[u8; 32]>> = std::collections::BTreeMap::new();
    for state in &sandbox.c_states {
        let mut entries = blaze_own::Vec::with_capacity_in(state.len(), alloc);
        for (name, digest) in state {
            let snap = match digest {
                None => Snap::Folder,
                Some(digest) => {
                    let last = *finals.entry(name.as_str()).or_insert_with(|| outputs.file(name).map(snap_digest));
                    if last == Some(*digest) {
                        Snap::Final
                    } else {
                        let bytes = sandbox.c_held.get(digest).expect("c_inputs: snap_out holds every digest a state names");
                        outputs.hold_c_bytes(*digest, bytes, alloc);
                        Snap::Held(*digest)
                    }
                }
            };
            entries.push((blaze_own::String::from_str_in(name, alloc), snap));
        }
        outputs.c_states.push(CState { entries });
    }
}

/// Instantiates the module and runs `_start`.
fn start(store: &mut WasmStore<ScriptHost>, loaded: &LoadedScript) -> wasmtime::Result<()> {
    let instance = loaded.pre.instantiate(&mut *store)?;
    let Some(memory) = instance.get_memory(&mut *store, "memory") else {
        return Err(wasmtime::Error::msg("the module exports no memory"));
    };
    store.data_mut().memory = Some(memory);
    let start: TypedFunc<(), ()> = instance.get_typed_func(&mut *store, "_start")?;
    start.call(&mut *store, ())
}

/// Why an instance that ended in `error` gave no record; `Ok` for `proc_exit(0)`.
fn stopped_by(host: &mut ScriptHost, error: &wasmtime::Error, out_of_fuel: bool) -> Result<(), ScriptWhy> {
    let stderr = || String::from_utf8_lossy(&host.sandbox.stderr).into_owned();
    if let Some(refusal) = host.sandbox.refused.take() {
        let why = host.stop.take().map_or_else(String::new, |it| it.0);
        return Err(match refusal {
            Refusal::OutDir(_) => ScriptWhy::Limit(ScriptLimit::OutDir, why),
            Refusal::Output(_) => ScriptWhy::Limit(ScriptLimit::Output, why),
            refusal => ScriptWhy::Refused(refusal),
        });
    }
    let deferred = |failure: String| {
        host.sandbox.c_requests.last().map(|(request, _)| ScriptWhy::NeedsDeferred { request: request.replace('\0', " "), failure, stderr: stderr() })
    };
    match host.sandbox.exit {
        Some(0) => return Ok(()),
        Some(code) => return Err(deferred(format!("exited with code {code}")).unwrap_or_else(|| ScriptWhy::Exit { code, stderr: stderr() })),
        None => {}
    }
    let trap = error.downcast_ref::<Trap>().copied();
    if trap == Some(Trap::OutOfFuel) || out_of_fuel {
        return Err(ScriptWhy::Limit(ScriptLimit::Fuel, "ran out of fuel: it may loop forever".to_owned()));
    }
    if host.limiter.over {
        return Err(ScriptWhy::Limit(ScriptLimit::Memory, format!("asked for more than {} bytes of memory: {error:#}", host.limiter.memory)));
    }
    let why = host.stop.take().map_or_else(|| format!("{error:#}"), |stop| stop.0);
    Err(deferred(format!("trapped: {why}")).unwrap_or_else(|| ScriptWhy::Trapped { why, stderr: stderr() }))
}
