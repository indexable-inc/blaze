//! A build script's run: its `wasm32-wasip1` module (`_start`, built with the process shim
//! `guest/script_process.rs`) in a fresh Wasmtime instance over the [`ScriptSandbox`], within
//! fuel and memory limits, and its outputs kept as a record (docs/v2/plan/design/build-scripts.md).
//!
//! - The record's base is [`blaze_build_script::key::base`]: the module's SHA-256, the whole
//!   environment, the answers it could be given, the C mode and the limits. Its read-list is
//!   what the script read of its workspace (`file:`, `dir:`, `stat:` subjects, valued now by
//!   [`answer_now`]), so a changed file misses exactly the runs that read it.
//! - The answer is [`Outputs`] in canonical bytes: the instructions, the files of `OUT_DIR`
//!   and the C requests deferred.
//! - A failed run is not kept: a script that exits non-zero, traps or is stopped gives a
//!   [`ScriptError`] naming the package.
//!
//! The compiled-for-host module is `<store>/build-script/compiled/<key>/code`; records are
//! under `<store>/build-script/records/{list,answer}/<id>`.

use std::{
    hash::{Hash as _, Hasher as _},
    path::{Path, PathBuf},
    sync::Arc,
};

use blaze_build_script::{
    Env, Outputs, OutputsRef,
    instr::{OutputError, OutputLimits},
    key::{self, KeyInputs, RunnerKey},
    outputs::{FileError, FileLimits},
    probe::Table,
};
use blaze_ids::ContentId;
use blaze_own::Alloc;
use blaze_record::{AnswerKey, Bounds, Digest, Filing, Found, keep_answer, lookup, write_list};
use blaze_wasi::{Answer, Errno, Stop, W32};
use wasmtime::{Caller, Config, Engine, InstancePre, Linker, Memory, Module, OptLevel, ResourceLimiter, Store as WasmStore, Trap, TypedFunc, WasmBacktraceDetails};

use crate::{
    host::{MAX_WASM_STACK, fuel_cost},
    module::sha256_hex,
    record::DirStore,
    script_sandbox::{Read, Refusal, Script, ScriptSandbox, Tree, answer_now},
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
    /// The records could not be read or written.
    Record(RecordError),
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
            ScriptWhy::Record(why) => write!(f, "the build-script records of {package}: {why}"),
        }
    }
}

impl std::error::Error for ScriptError {}

/// Why the build-script records could not be read or written.
#[derive(Debug)]
pub enum RecordError {
    /// The read-list chain or the store under it.
    Chain(blaze_record::Error<crate::record::FilesError>),
    /// The answer under `base` is not a run's outputs.
    NotOutputs { base: ContentId, why: blaze_build_script::outputs::RecordError },
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecordError::Chain(error) => write!(f, "{error}"),
            RecordError::NotOutputs { base, why } => write!(f, "the record of base {:02x?} is not outputs: {why:?}", &base.0[..8]),
        }
    }
}

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

/// The engine and linker build scripts run in, and the store they are kept in.
pub struct ScriptRuntime {
    engine: Engine,
    linker: Linker<ScriptHost>,
    code_id: String,
    store: PathBuf,
    records: DirStore,
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
    /// The record's base key.
    pub base: ContentId,
    /// Whether the record answered it (nothing ran).
    pub from_record: bool,
    /// What it read (empty when the record answered).
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
    /// The runtime keeping compiled modules under `store` and its records in `records`
    /// ([`DirStore::build_scripts`] over the same folder, through the caller's world).
    pub fn new(store: &Path, records: DirStore) -> Result<ScriptRuntime, RuntimeError> {
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
        Ok(ScriptRuntime { engine, linker, code_id, store: store.to_owned(), records })
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
            std::fs::create_dir_all(&dir).map_err(|e| io(&dir, e))?;
            let staging = dir.join(format!("code.{}.tmp", std::process::id()));
            std::fs::write(&staging, &code).map_err(|e| io(&staging, e))?;
            std::fs::rename(&staging, &path).map_err(|e| io(&path, e))?;
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
            answers: Table { rows: &script.table }.id(),
            c_mode: script.c_mode,
            limits: script.limits,
            runner: RunnerKey::Wasm,
        };
        let mut scratch = blaze_own::Vec::new_in(Alloc::global());
        key::base(&inputs, &mut scratch)
    }

    /// `script`'s outputs: from its record when one answers under what its reads are now, else
    /// run by `loaded` and kept. `env` is the environment `script.env` was made from.
    pub fn run<'a>(&self, loaded: &LoadedScript, script: Arc<Script>, env: &Env<'_>, alloc: Alloc<'a>) -> Result<ScriptRun<'a>, ScriptError> {
        let package = script.package.clone();
        let named = |why: ScriptWhy| ScriptError { package: package.clone(), why };
        let base = self.base(loaded, &script, env);
        if let Some(outputs) = self.find(&base, &script.tree, alloc).map_err(|why| named(ScriptWhy::Record(why)))? {
            return Ok(ScriptRun { outputs, base, from_record: true, reads: Vec::new(), fuel: 0, peak_memory: 0, spawns: 0 });
        }
        let ran = self.fresh(loaded, script, base, alloc)?;
        self.keep(base, &ran.reads, &ran.outputs).map_err(|why| named(ScriptWhy::Record(why)))?;
        Ok(ran)
    }

    /// `script` run by `loaded`, nothing looked up or kept: its outputs and reads.
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
        for request in &sandbox.c_requests {
            outputs.c_requests.push(blaze_own::String::from_str_in(request, alloc));
        }
        let reads = std::mem::take(&mut host.sandbox.reads);
        Ok(ScriptRun { outputs, base, from_record: false, reads, fuel, peak_memory: host.limiter.peak, spawns: host.sandbox.spawns })
    }

    /// The outputs kept under `base` for what its reads are in `tree` now.
    pub fn find<'a>(&self, base: &ContentId, tree: &Tree, alloc: Alloc<'a>) -> Result<Option<Outputs<'a>>, RecordError> {
        let found = lookup(&self.records, Bounds::STANDARD, base, |name| {
            let subject = std::str::from_utf8(name).ok()?;
            let now = answer_now(tree, subject).ok()?;
            Some(now.map_or(Digest::Nothing, Digest::Is))
        })
        .map_err(RecordError::Chain)?;
        match found {
            Found::Hit { answer, .. } => {
                let opened = OutputsRef::open(&answer).map_err(|why| RecordError::NotOutputs { base: *base, why })?;
                Ok(Some(opened.to_owned(alloc)))
            }
            Found::Miss { .. } => Ok(None),
        }
    }

    pub(crate) fn keep(&self, base: ContentId, reads: &[Read], outputs: &Outputs<'_>) -> Result<(), RecordError> {
        self.keep_as(base, reads, outputs, blaze_build_script::host_tools::RUN_KIND)
    }

    /// [`Self::keep`] with the read list filed under `kind`: a native run granted a host tool
    /// passes [`blaze_build_script::host_tools::RUN_KIND_HOST_TOOL`], the `host-tool` mark the
    /// shared-cache origin refuses to seal (D71).
    pub(crate) fn keep_as(&self, base: ContentId, reads: &[Read], outputs: &Outputs<'_>, kind: &[u8]) -> Result<(), RecordError> {
        let mut list = Vec::new();
        write_list(&mut list, &base, kind, reads.iter().map(|read| read.subject.as_bytes()));
        let mut key = AnswerKey::new(&list);
        for read in reads {
            key.value(read.digest.map_or(Digest::Nothing, Digest::Is));
        }
        let key = key.finish();
        let mut filing = Filing::new(base, Bounds::STANDARD);
        filing.place(&self.records, &list).map_err(RecordError::Chain)?;
        let mut bytes = blaze_own::Vec::new_in(Alloc::global());
        outputs.encode(&mut bytes);
        keep_answer(&self.records, key, &bytes).map_err(RecordError::Chain)?;
        Ok(())
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
        host.sandbox.c_requests.last().map(|request| ScriptWhy::NeedsDeferred { request: request.replace('\0', " "), failure, stderr: stderr() })
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
