//! One expansion in a fresh Wasmtime instance of a macro's module: the call's input written
//! to the module's memory, its macro run, each request its `proc_macro` client makes answered
//! by the [`Server`] through the shim's imports (`guest/shim.rs`), and the WASI calls of its
//! std by `blaze_wasi` over the [`Sandbox`].
//!
//! Nothing outlives a call but the compiled module: each call gets its own store, instance and
//! memory, made from the module as it was built, so no state passes between calls. Fuel bounds
//! its work and a limiter its memory; running past either is [`ProcLimit::Fuel`] or
//! [`ProcLimit::Memory`], naming the macro and the call.
//!
//! # Compiled records
//!
//! A module is compiled to machine code once into a record, and every later process loads it
//! from there: a module is never compiled lazily under a lock while other work waits for it
//! ([`Runtime::load_all`] compiles a build's list ahead on N workers).
//!
//! - Where: the store's one folder of compiled code, `<store>/proc-macro/compiled/`
//!   ([`ModuleRecord::compiled`], module.rs [`crate::module::COMPILED_DIR`]), NOT the module's
//!   own folder: `<code key>` holds the bytes of Wasmtime's `Engine::precompile_module`,
//!   `<code key>.seal` their size and the key's parts. The record is keyed by what the code is
//!   made from (rule 52), not by which module record named it, so a module of any identity
//!   form whose wasm is the same bytes takes the same record, and a home whose module store is
//!   copied from another (the scoreboard's fresh homes) takes every record compiled into it.
//!   (Before, 2026-11-20: records were `compiled-<code key>` in each module's folder; the
//!   form-3 modules of 2026-11-18 had none, every scoreboard home was a fresh copy, and each
//!   first check compiled all 81 modules with Cranelift: the Names stage's CPU went from 50 s
//!   to 153 s.)
//! - The key ([`Runtime::code_key`]) is the SHA-256 of everything that decides whether the
//!   bytes are valid here: the module's wasm SHA-256; [`Settings`], every setting the crate
//!   passes to Wasmtime, read from the one place the engine is built ([`Settings::config`]);
//!   and Wasmtime's `precompile_compatibility_hash`, which hashes the target triple, the
//!   compiler's flags, the host CPU features Wasmtime compiled for (`isa_flags`), its tunables
//!   and wasm features, and Wasmtime's version; and the host's architecture and system.
//! - Writing: the bytes go to a temporary name (process id and a count), are synced, and are
//!   renamed to `<code key>`; only then is the seal written the same way. A seal therefore
//!   never names bytes that are not complete: the seal is the record's proof, made once by the
//!   writer that made the bytes (rule 74). Two processes compiling one module write equal
//!   bytes under one name (Wasmtime's output is a function of the wasm and the key), so the
//!   later rename replaces a file with an equal one, and a process that mapped the earlier one
//!   keeps its own inode.
//! - Reading: in place. The record is opened once, checked through that open file, and mapped
//!   by Wasmtime (`Module::deserialize_open_file`); its bytes are neither copied nor hashed on
//!   a read.
//! - TRUST. Wasmtime's deserialize entry trusts its bytes: they are machine code that runs in
//!   this process. So a compiled record is only ever one this user's blaze wrote into the
//!   user's own store, and it is loaded only after: the compiled folder, the record and the
//!   seal are not writable by group or others (a folder others can write is
//!   [`RuntimeError::OpenStore`] and nothing compiled is read from or written to it); the
//!   record (checked on the open file that is mapped) and the seal have the folder's owner;
//!   the seal's key parts equal this runtime's; and the record's size is the seal's. The
//!   content is not hashed again: only its owner can write it, and the owner can run any code
//!   already. This happens once per process per module (a [`Runtime`] keeps each loaded
//!   module). A record that fails any of these is not loaded: it is removed by name (an
//!   unsealed one is left for the writer that may be between its two renames), kept as a
//!   [`CompiledFinding`], and the module is compiled again.
//! - One runtime, two workers asking for one module at once: one compiles or loads, the other
//!   waits on the module's slot with a deadline ([`LOAD_DEADLINE`]) and a span naming the
//!   module; past it the wait is [`RuntimeError::Wait`], naming the module.
//!
//! An expansion runs on the calling thread, whose stack must hold [`MAX_WASM_STACK`] and the
//! host's frames above it: the engine checks the wasm stack's depth, not the thread's.

#![expect(
    clippy::disallowed_types,
    reason = "a std Mutex and Condvar hold each module's load slot; blaze_core::lock is not a dependency of this crate"
)]

use std::{
    collections::BTreeMap,
    fs::{self, File},
    hash::{Hash as _, Hasher as _},
    io::{ErrorKind, Write as _},
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use blaze_expand::{Edition, HostAsk, HostError, HostWhy, ProcCall, ProcLimit, Produced, ReadAnswer, ReadKind, Reads};
use blaze_own::Alloc;
use blaze_wasi::{Stop, W32};
use wasmtime::{Caller, Config, Engine, InstancePre, Linker, Memory, Module, OperatorCost, OptLevel, ResourceLimiter, Store, Trap, TypedFunc, Val, WasmBacktraceDetails};

use crate::{
    module::{Exported, ModuleRecord, sha256_hex},
    parts::{Clock, Mark, Part, Parts},
    package_env::EnvRefused,
    sandbox::{Package, Sandbox},
    server::{Outcome, ProduceError, Server, ServerError, ServerLimits, Tracked},
};

/// The import module of the shim's calls.
const IMPORTS: &str = "blaze_proc_macro";

/// The deepest a module's stack may grow.
pub const MAX_WASM_STACK: usize = 4 << 20;

/// The most elements a module's tables may hold.
const TABLE_MAX: usize = 1 << 20;

/// What one expansion may spend.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Fuel: about one unit a wasm operation that makes code ([`fuel_cost`]).
    pub fuel: u64,
    /// The most bytes its memory may grow to.
    pub memory: usize,
    /// The most bytes of output it may give.
    pub output: usize,
    pub server: ServerLimits,
}

impl Limits {
    /// A derive of serde's size spends some hundreds of millions of fuel: this is tens of
    /// times that.
    pub const STANDARD: Limits = Limits { fuel: 1 << 34, memory: 1 << 30, output: 1 << 28, server: ServerLimits::STANDARD };
}

/// What each operator costs: Wasmtime's table, with locals and constants free (they make no
/// work of their own once registers are allocated). Every loop still costs: a backward edge
/// is a branch, and a call costs one.
pub(crate) fn fuel_cost() -> OperatorCost {
    let mut cost = OperatorCost::new();
    cost.LocalGet = 0;
    cost.LocalSet = 0;
    cost.LocalTee = 0;
    cost.I32Const = 0;
    cost.I64Const = 0;
    cost.F32Const = 0;
    cost.F64Const = 0;
    cost
}

/// Why the runtime could not be made or a module loaded.
#[derive(Debug)]
pub enum RuntimeError {
    /// Wasmtime refused the engine's settings.
    Engine(String),
    /// The linker of the shim's and WASI's calls could not be made.
    Linker(String),
    /// The module does not compile, or does not link against the host's calls.
    Compile { module: String, why: String },
    /// The compiled record could not be read or written.
    Io { path: PathBuf, error: std::io::Error },
    /// The module's folder can be written by group or others: no compiled code is read from
    /// it or written to it.
    OpenStore { dir: PathBuf, mode: u32 },
    /// Another worker of this runtime was loading the module and did not finish by the
    /// deadline.
    Wait { module: String, waited: Duration },
    /// A worker thread of [`Runtime::load_all`] could not be started.
    Spawn(std::io::Error),
    /// The module's wasm, read for its compile, could not be read or is not the bytes its
    /// `wasm-sha256` names (module.rs `ModuleRecord::wasm`).
    Wasm(crate::module::ModuleError),
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RuntimeError::Engine(why) => write!(f, "Wasmtime could not make its engine: {why}"),
            RuntimeError::Linker(why) => write!(f, "the host's calls could not be linked: {why}"),
            RuntimeError::Compile { module, why } => write!(f, "the module of {module} does not load: {why}"),
            RuntimeError::Io { path, error } => write!(f, "{}: {error}", path.display()),
            RuntimeError::OpenStore { dir, mode } => {
                write!(f, "{}: mode {mode:o} lets group or others write it, so no compiled module is read from it or kept in it", dir.display())
            }
            RuntimeError::Wait { module, waited } => write!(f, "the module of {module} was still being loaded by another worker after {waited:?}"),
            RuntimeError::Spawn(error) => write!(f, "a thread to load proc-macro modules could not be started: {error}"),
            RuntimeError::Wasm(error) => write!(f, "a proc-macro module's wasm, read for its compile: {error}"),
        }
    }
}

impl std::error::Error for RuntimeError {}

/// How long a worker waits for another worker of its runtime to load a module: about a
/// hundred times the longest compile measured (a 1.6 MB module in about two seconds).
pub const LOAD_DEADLINE: Duration = Duration::from_secs(300);

/// Every setting the crate passes to Wasmtime. The engine is built from these alone
/// ([`Settings::config`]), and they are hashed whole into every compiled record's key.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Settings {
    /// Cranelift's optimisation level: `Speed` when true, `None` when false.
    pub optimize: bool,
    /// A NaN's bits are the same on every host, so a macro that prints a float gives the same
    /// tokens wherever it ran.
    pub nan_canonicalization: bool,
    pub consume_fuel: bool,
    pub fuel_cost: OperatorCost,
    pub max_wasm_stack: usize,
    /// No call runs async, but the vendored build has the feature on, and the engine refuses a
    /// wasm stack larger than the async one.
    pub async_stack_size: usize,
    pub wasm_backtrace_details: bool,
}

impl Settings {
    /// The settings every build uses.
    pub fn standard() -> Settings {
        Settings {
            optimize: true,
            nan_canonicalization: true,
            consume_fuel: true,
            fuel_cost: fuel_cost(),
            max_wasm_stack: MAX_WASM_STACK,
            async_stack_size: MAX_WASM_STACK,
            wasm_backtrace_details: false,
        }
    }

    /// The engine's configuration: the one place the engine's settings are made.
    pub fn config(&self) -> Config {
        let mut config = Config::new();
        config
            .cranelift_opt_level(if self.optimize { OptLevel::Speed } else { OptLevel::None })
            .cranelift_nan_canonicalization(self.nan_canonicalization)
            .consume_fuel(self.consume_fuel)
            .operator_cost(self.fuel_cost.clone())
            .max_wasm_stack(self.max_wasm_stack)
            .async_stack_size(self.async_stack_size)
            .wasm_backtrace_details(if self.wasm_backtrace_details { WasmBacktraceDetails::Enable } else { WasmBacktraceDetails::Disable });
        config
    }
}

/// Why a compiled record was not loaded. It was removed and the module compiled again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// The record has no seal: its writer stopped between the two renames.
    Unsealed,
    /// A seal with no record.
    NoRecord,
    /// The record or its seal can be written by group or others, or has another owner than
    /// the compiled folder.
    Open { mode: u32, uid: u32, owner: u32 },
    /// The record is not the size its seal names (`held` is the seal's size line).
    Size { held: String, found: u64 },
    /// The seal's key parts are not this runtime's.
    Key { held: String },
}

/// A compiled record that was not loaded, by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledFinding {
    /// `name version`.
    pub module: String,
    pub record: PathBuf,
    pub why: Refused,
}

impl std::fmt::Display for CompiledFinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let record = self.record.display();
        let module = &self.module;
        match &self.why {
            Refused::Unsealed => write!(f, "the compiled record {record} of {module} has no seal (its writer may still be between its two renames); compiled again over it"),
            Refused::NoRecord => write!(f, "the seal of {record} of {module} names no record; removed and compiled again"),
            Refused::Open { mode, uid, owner } => write!(
                f,
                "the compiled record {record} of {module} has mode {mode:o} and owner {uid} (its folder's is {owner}): others could have written it; removed and compiled again"
            ),
            Refused::Size { held, found } => {
                write!(f, "the compiled record {record} of {module} is {found} bytes, its seal says {held:?}; removed and compiled again")
            }
            Refused::Key { held } => write!(f, "the seal of {record} of {module} holds another key ({held:?}); removed and compiled again"),
        }
    }
}

/// One module's load in a runtime: being loaded by one worker, loaded, or failed.
enum SlotState {
    Loading,
    Ready(Arc<Loaded>),
    Failed(String),
}

struct Slot {
    state: Mutex<SlotState>,
    done: Condvar,
}

/// The engine, its linker, and each module it loaded in this process.
pub struct Runtime {
    engine: Engine,
    linker: Linker<Host>,
    code_id: String,
    /// By the module's record key.
    slots: Mutex<BTreeMap<String, Arc<Slot>>>,
    /// How many modules this runtime compiled (none loaded from a record).
    compiles: AtomicU64,
    /// Their compiles' time, in nanoseconds, summed over the workers that ran them.
    compile_ns: AtomicU64,
    /// How many modules this runtime took from their compiled records.
    taken: AtomicU64,
    /// Temporary names: one count for the runtime, with the process id.
    temps: AtomicU64,
    findings: Mutex<Vec<CompiledFinding>>,
    /// Where its calls' time went ([`Runtime::timed`] gives the clock).
    parts: Parts,
    /// The heap each call's server lexes in (D102: handed in by the runtime's maker; a `&Heap`,
    /// not an `Alloc`, so the runtime stays `Sync` for its load workers).
    heap: &'static blaze_own::Heap,
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The bytes a `Hash` writes, in order, hashed whole.
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

/// What a runtime's module loads came to ([`Runtime::module_loads`]). Its `Display` is the
/// profile's line: `macro modules: N loaded, M compiled now (S ms), K taken compiled`, where S
/// is the compiles' time summed over the workers that ran them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModuleLoads {
    /// Modules loaded: `compiled + taken`.
    pub loaded: u64,
    /// Modules compiled by this process (no valid compiled record was there).
    pub compiled: u64,
    /// The compiles' time, summed over workers.
    pub compile_time: Duration,
    /// Modules taken from their compiled records, mapped in place.
    pub taken: u64,
}

impl core::fmt::Display for ModuleLoads {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "macro modules: {} loaded, {} compiled now ({} ms), {} taken compiled", self.loaded, self.compiled, self.compile_time.as_millis(), self.taken)
    }
}

/// A module compiled for this host, ready to instantiate per call.
pub struct Loaded {
    pre: InstancePre<Host>,
    /// The crate, `name version`, for errors.
    pub crate_name: String,
    decls: String,
    /// Whether this load compiled it (no valid compiled record was kept).
    pub compiled: bool,
    /// The compiled record's size in bytes.
    pub code_bytes: u64,
    pub load_time: Duration,
}

impl Runtime {
    /// The runtime with the standard [`Settings`].
    pub fn new(heap: &'static blaze_own::Heap) -> Result<Runtime, RuntimeError> {
        Runtime::with_settings(&Settings::standard(), heap)
    }

    /// The runtime with `settings`; its calls' servers lex in `heap`.
    pub fn with_settings(settings: &Settings, heap: &'static blaze_own::Heap) -> Result<Runtime, RuntimeError> {
        let engine = Engine::new(&settings.config()).map_err(|e| RuntimeError::Engine(format!("{e:#}")))?;
        let mut bytes = Bytes::default();
        bytes.write(b"blaze proc-macro host 2\n");
        settings.hash(&mut bytes);
        // The target triple, the compiler's flags, the host CPU features Wasmtime compiled for
        // (`isa_flags`), its tunables and wasm features, and Wasmtime's version.
        engine.precompile_compatibility_hash().hash(&mut bytes);
        bytes.write(std::env::consts::ARCH.as_bytes());
        bytes.write(std::env::consts::OS.as_bytes());
        let code_id = sha256_hex(&bytes.0);
        let mut linker = Linker::new(&engine);
        imports(&mut linker).map_err(|e| RuntimeError::Linker(format!("{e:#}")))?;
        blaze_wasi::link::<W32, Host>(&mut linker).map_err(|e| RuntimeError::Linker(format!("{e:#}")))?;
        Ok(Runtime {
            engine,
            linker,
            code_id,
            slots: Mutex::new(BTreeMap::new()),
            compiles: AtomicU64::new(0),
            compile_ns: AtomicU64::new(0),
            taken: AtomicU64::new(0),
            temps: AtomicU64::new(0),
            findings: Mutex::new(Vec::new()),
            parts: Parts::counted(),
            heap,
        })
    }

    /// The runtime with the standard [`Settings`], its calls' parts timed by `clock` (a
    /// profile's driver gives its own: rule 75).
    pub fn timed(clock: Clock, heap: &'static blaze_own::Heap) -> Result<Runtime, RuntimeError> {
        let mut runtime = Runtime::new(heap)?;
        runtime.parts = Parts::timed(clock);
        Ok(runtime)
    }

    /// Where the calls' time went so far.
    pub fn parts(&self) -> &Parts {
        &self.parts
    }

    /// The heap the runtime was handed ([`Runtime::new`]): what the hook makes the rows a
    /// command runs in (record.rs `table::open`).
    pub fn heap(&self) -> &'static blaze_own::Heap {
        self.heap
    }

    /// What the machine code depends on besides the module: [`Settings`], Wasmtime's
    /// compatibility hash, the host's architecture and system.
    pub fn code_id(&self) -> &str {
        &self.code_id
    }

    /// The compiled record's key for `module`: its wasm's SHA-256 and [`Runtime::code_id`].
    pub fn code_key(&self, module: &ModuleRecord) -> String {
        sha256_hex(format!("blaze compiled module 1\n{}\n{}\n", module.wasm_sha256, self.code_id).as_bytes())
    }

    /// How many modules this runtime compiled.
    pub fn compiles(&self) -> u64 {
        self.compiles.load(Ordering::Relaxed)
    }

    /// What this runtime's module loads came to so far: the profile's `macro modules` line.
    pub fn module_loads(&self) -> ModuleLoads {
        let compiled = self.compiles.load(Ordering::Relaxed);
        let taken = self.taken.load(Ordering::Relaxed);
        ModuleLoads { loaded: compiled + taken, compiled, compile_time: Duration::from_nanos(self.compile_ns.load(Ordering::Relaxed)), taken }
    }

    /// The compiled records refused so far, taken.
    pub fn take_findings(&self) -> Vec<CompiledFinding> {
        std::mem::take(&mut *lock(&self.findings))
    }

    /// `module` compiled for this host, loaded once per runtime: from its compiled record, or
    /// compiled and the record written. A second worker asking while the first loads it waits
    /// for it, up to [`LOAD_DEADLINE`].
    pub fn load(&self, module: &ModuleRecord) -> Result<Arc<Loaded>, RuntimeError> {
        use crate::ShareArc as _;
        let (slot, mine) = {
            let mut slots = lock(&self.slots);
            match slots.get(&module.key) {
                Some(slot) => (slot.share_arc(), false),
                None => {
                    let slot = Arc::new(Slot { state: Mutex::new(SlotState::Loading), done: Condvar::new() });
                    slots.insert(module.key.clone(), slot.share_arc());
                    (slot, true)
                }
            }
        };
        let label = || format!("{} {}", module.name, module.version);
        if mine {
            let loaded = self.load_now(module);
            *lock(&slot.state) = match &loaded {
                Ok(loaded) => SlotState::Ready(loaded.share_arc()),
                Err(error) => SlotState::Failed(error.to_string()),
            };
            slot.done.notify_all();
            return loaded;
        }
        let started = Instant::now();
        let deadline = started + LOAD_DEADLINE;
        let mut state = lock(&slot.state);
        let _span = matches!(*state, SlotState::Loading).then(|| tracing::info_span!("proc_macro_module_wait", module = %label()).entered());
        loop {
            match &*state {
                SlotState::Ready(loaded) => return Ok(loaded.share_arc()),
                SlotState::Failed(why) => return Err(RuntimeError::Compile { module: label(), why: why.clone() }),
                SlotState::Loading => {}
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(RuntimeError::Wait { module: label(), waited: now - started });
            }
            state = slot.done.wait_timeout(state, deadline - now).unwrap_or_else(PoisonError::into_inner).0;
        }
    }

    /// Each of `modules` loaded ([`Runtime::load`]) on `workers` threads, in `modules`' order:
    /// what a build calls before its expansions start, so none waits on a compile.
    pub fn load_all(&self, modules: &[&ModuleRecord], workers: usize) -> Result<Vec<Result<Arc<Loaded>, RuntimeError>>, RuntimeError> {
        let next = AtomicUsize::new(0);
        let worker = || {
            let mut done = Vec::new();
            loop {
                let at = next.fetch_add(1, Ordering::Relaxed);
                let Some(module) = modules.get(at) else { return done };
                done.push((at, self.load(module)));
            }
        };
        let mut all: Vec<(usize, Result<Arc<Loaded>, RuntimeError>)> = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for n in 0..workers.clamp(1, modules.len().max(1)) {
                // A compile's own stack, as an expansion worker's.
                let handle = std::thread::Builder::new().name(format!("proc-macro-load-{n}")).stack_size(MAX_WASM_STACK * 4).spawn_scoped(scope, worker);
                handles.push(handle.map_err(RuntimeError::Spawn)?);
            }
            let mut all = Vec::new();
            for handle in handles {
                all.extend(handle.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic)));
            }
            Ok::<_, RuntimeError>(all)
        })?;
        all.sort_by_key(|(at, _)| *at);
        Ok(all.into_iter().map(|(_, loaded)| loaded).collect())
    }

    /// See [`Runtime::load`]: this worker loads it.
    #[allow(unsafe_code)]
    fn load_now(&self, module: &ModuleRecord) -> Result<Arc<Loaded>, RuntimeError> {
        let started = Instant::now();
        let compile = |why: String| RuntimeError::Compile { module: format!("{} {}", module.name, module.version), why };
        let dir = &module.compiled;
        // The store's compiled folder is made on its first compile (mode from the user's umask;
        // `private_dir` refuses one that group or others can write).
        fs::create_dir_all(dir).map_err(|error| RuntimeError::Io { path: dir.clone(), error })?;
        let owner = private_dir(dir)?;
        let key = self.code_key(module);
        let path = dir.join(&key);
        let seal_path = dir.join(format!("{key}.seal"));
        let (compiled, compiled_now, code_bytes) = match self.open_sealed(module, &key, &path, &seal_path, owner)? {
            Some((file, len)) => {
                // SAFETY: `deserialize_open_file` maps this file and runs its bytes as machine
                // code. It is the record `open_sealed` opened in this user's own store (folder,
                // seal and this open file not writable by group or others, owned by the
                // folder's owner), whose seal, written by the process that made the bytes only
                // after they were synced and renamed whole, names this size under a key holding
                // every setting of this engine. Records are written once, by rename, never in
                // place: a later writer replaces the name, not this inode, so the mapping stays
                // the bytes checked for the life of the module.
                let loaded = unsafe { Module::deserialize_open_file(&self.engine, file) };
                let loaded = loaded.map_err(|e| compile(format!("its compiled record {} does not load: {e:#}", path.display())))?;
                self.taken.fetch_add(1, Ordering::Relaxed);
                (loaded, false, len)
            }
            None => {
                let compiling = Instant::now();
                // The wasm read and checked only here, for this compile, and dropped with it
                // (module.rs `ModuleRecord::wasm`).
                let wasm = module.wasm().map_err(RuntimeError::Wasm)?;
                let code = self.engine.precompile_module(&wasm).map_err(|e| compile(format!("{e:#}")))?;
                drop(wasm);
                let len = u64::try_from(code.len()).unwrap_or(u64::MAX);
                self.write_synced(dir, &path, &code)?;
                self.write_synced(dir, &seal_path, seal(&key, module, &self.code_id, len).as_bytes())?;
                // SAFETY: the bytes this engine just made from the module, in this process's
                // memory.
                let loaded = unsafe { Module::deserialize(&self.engine, &code) };
                let loaded = loaded.map_err(|e| compile(format!("{e:#}")))?;
                self.compiles.fetch_add(1, Ordering::Relaxed);
                self.compile_ns.fetch_add(u64::try_from(compiling.elapsed().as_nanos()).unwrap_or(u64::MAX), Ordering::Relaxed);
                (loaded, true, len)
            }
        };
        let pre = self.linker.instantiate_pre(&compiled).map_err(|e| compile(format!("{e:#}")))?;
        let crate_name = format!("{} {}", module.name, module.version);
        Ok(Arc::new(Loaded { pre, crate_name, decls: module.decls.clone(), compiled: compiled_now, code_bytes, load_time: started.elapsed() }))
    }

    /// `module`'s compiled record at `path`, opened, with its size, when its seal at
    /// `seal_path` names it under `key`; otherwise none, a finding kept and the record and
    /// seal removed (an unsealed record is left: its writer may be between its two renames,
    /// and the compile that follows renames over it).
    fn open_sealed(&self, module: &ModuleRecord, key: &str, path: &Path, seal_path: &Path, owner: u32) -> Result<Option<(File, u64)>, RuntimeError> {
        let io = |path: &Path| {
            let path = path.to_owned();
            move |error| RuntimeError::Io { path, error }
        };
        let held = match fs::read(seal_path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => return Err(io(seal_path)(error)),
        };
        let file = match File::open(path) {
            Ok(file) => Some(file),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => return Err(io(path)(error)),
        };
        let refused = match (held, file) {
            (None, None) => return Ok(None),
            (None, Some(_)) => Refused::Unsealed,
            (Some(_), None) => Refused::NoRecord,
            (Some(held), Some(file)) => {
                // The record's mode, owner and size from the open file that is mapped, not its
                // name: nothing can be swapped in between the check and the map.
                let record = file.metadata().map_err(io(path))?;
                let sealed = fs::metadata(seal_path).map_err(io(seal_path))?;
                let open = [&record, &sealed]
                    .into_iter()
                    .find(|meta| meta.mode() & 0o022 != 0 || meta.uid() != owner)
                    .map(|meta| Refused::Open { mode: meta.mode() & 0o7777, uid: meta.uid(), owner });
                let held = String::from_utf8_lossy(&held).into_owned();
                let expected = seal(key, module, &self.code_id, record.len());
                match open {
                    Some(open) => open,
                    None if held == expected => return Ok(Some((file, record.len()))),
                    None => match (held.split_once('\n'), expected.split_once('\n')) {
                        (Some((held_size, held_key)), Some((_, found_key))) if held_key == found_key => Refused::Size { held: held_size.to_owned(), found: record.len() },
                        _ => Refused::Key { held },
                    },
                }
            }
        };
        if refused != Refused::Unsealed {
            for at in [path, seal_path] {
                match fs::remove_file(at) {
                    Err(error) if error.kind() != ErrorKind::NotFound => return Err(io(at)(error)),
                    _ => {}
                }
            }
        }
        lock(&self.findings).push(CompiledFinding { module: format!("{} {}", module.name, module.version), record: path.to_owned(), why: refused });
        Ok(None)
    }

    /// `bytes` written to a temporary name in `dir`, synced, then renamed to `path`: a name
    /// never holds bytes that are not whole on disk.
    fn write_synced(&self, dir: &Path, path: &Path, bytes: &[u8]) -> Result<(), RuntimeError> {
        let n = self.temps.fetch_add(1, Ordering::Relaxed);
        let name = path.file_name().map(|it| it.to_string_lossy().into_owned()).unwrap_or_default();
        let staging = dir.join(format!(".{name}.{}.{n}.tmp", std::process::id()));
        let at = |error| RuntimeError::Io { path: staging.clone(), error };
        let mut file = File::create(&staging).map_err(at)?;
        file.write_all(bytes).map_err(at)?;
        file.sync_all().map_err(at)?;
        drop(file);
        fs::rename(&staging, path).map_err(|error| RuntimeError::Io { path: path.to_owned(), error })
    }
}

/// The seal of a compiled record: its size on the first line, then the key's parts. Written
/// by the process that made the bytes, after they are synced and renamed whole: the proof a
/// read takes instead of hashing them again (rule 74).
fn seal(key: &str, module: &ModuleRecord, code_id: &str, len: u64) -> String {
    format!("size {len}\ncode-key {key}\nwasm-sha256 {}\ncode-id {code_id}\n", module.wasm_sha256)
}

/// The owner of `dir`, the store's compiled folder, when group and others cannot write it.
fn private_dir(dir: &Path) -> Result<u32, RuntimeError> {
    let meta = fs::metadata(dir).map_err(|error| RuntimeError::Io { path: dir.to_owned(), error })?;
    if meta.mode() & 0o022 != 0 {
        return Err(RuntimeError::OpenStore { dir: dir.to_owned(), mode: meta.mode() & 0o7777 });
    }
    Ok(meta.uid())
}

/// A store's data: one call's server and sandbox.
pub struct Host {
    server: Server,
    sandbox: Sandbox,
    memory: Option<Memory>,
    limiter: Limiter,
    output_limit: usize,
    /// The reply to the last request, which the module reads with `reply`.
    reply: Vec<u8>,
    output: Option<Vec<u8>>,
    /// Why a call of the host stopped the module.
    stop: Option<Stop>,
    server_error: Option<ServerError>,
    over_output: bool,
    requests: u64,
    /// The profile's clock, and the time the bridge's requests took by it.
    clock: Option<Clock>,
    bridge_nanos: u64,
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

fn stopped() -> wasmtime::Error {
    wasmtime::Error::msg("a call of the host stopped the expansion")
}

/// Runs `f` on the module's memory and the call's [`Host`]; a [`Stop`] it returns is kept and
/// ends the instance.
fn with<R>(caller: &mut Caller<'_, Host>, f: impl FnOnce(&mut [u8], &mut Host) -> Result<R, Stop>) -> wasmtime::Result<R> {
    let Some(memory) = caller.data().memory else {
        return Err(wasmtime::Error::msg("the module called the host before its memory was found"));
    };
    let (mem, host) = memory.data_and_store_mut(caller);
    match f(mem, host) {
        Ok(it) => Ok(it),
        Err(stop) => {
            host.stop.get_or_insert(stop);
            Err(stopped())
        }
    }
}

impl blaze_wasi::Embed for Host {
    fn with<R>(
        caller: &mut Caller<'_, Host>,
        f: impl FnOnce(&mut [u8], &mut dyn blaze_wasi::System) -> Result<R, Stop>, // dyn: `blaze_wasi`'s calls, shared by every embedder
    ) -> wasmtime::Result<R> {
        with(caller, |mem, host| f(mem, &mut host.sandbox))
    }
}

/// Writes `value` at `into` when it fits in `capacity`: its length.
fn give(mem: &mut [u8], into: u32, capacity: u32, value: &[u8]) -> Result<i32, Stop> {
    let Ok(n) = i32::try_from(value.len()) else {
        return Err(Stop(format!("was given {} bytes, more than a 32-bit memory holds", value.len())));
    };
    if value.len() <= capacity as usize {
        blaze_wasi::put(mem, u64::from(into), value)?;
    }
    Ok(n)
}

fn imports(l: &mut Linker<Host>) -> wasmtime::Result<()> {
    l.func_wrap(IMPORTS, "dispatch", |mut c: Caller<'_, Host>, request: u32, len: u32| {
        c.data_mut().requests += 1;
        let clock = c.data().clock;
        let at = clock.map_or(0, |clock| clock());
        with(&mut c, |mem, host| {
            let request = blaze_wasi::bytes(mem, u64::from(request), u64::from(len))?;
            let served = host.server.dispatch(request, &mut host.reply);
            if let Some(clock) = clock {
                host.bridge_nanos += clock().saturating_sub(at);
            }
            match served {
                Ok(()) => Ok(host.reply.len() as u32),
                Err(error) => {
                    host.server_error = Some(error);
                    Err(Stop(error.to_string()))
                }
            }
        })
    })?;
    l.func_wrap(IMPORTS, "reply", |mut c: Caller<'_, Host>, into: u32| {
        with(&mut c, |mem, host| {
            blaze_wasi::put(mem, u64::from(into), &host.reply)?;
            host.reply.clear();
            Ok(())
        })
    })?;
    l.func_wrap(IMPORTS, "output", |mut c: Caller<'_, Host>, bytes: u32, len: u32| {
        with(&mut c, |mem, host| {
            if host.output.is_some() {
                return Err(Stop("gave its output twice".to_owned()));
            }
            if len as usize > host.output_limit {
                host.over_output = true;
                return Err(Stop(format!("gave {len} bytes of output, more than its {}", host.output_limit)));
            }
            host.output = Some(blaze_wasi::bytes(mem, u64::from(bytes), u64::from(len))?.to_vec());
            Ok(())
        })
    })?;
    l.func_wrap(IMPORTS, "env_var", |mut c: Caller<'_, Host>, name: u32, len: u32, into: u32, capacity: u32| {
        with(&mut c, |mem, host| {
            let name = blaze_wasi::bytes(mem, u64::from(name), u64::from(len))?;
            let Ok(name) = std::str::from_utf8(name) else {
                return Err(Stop(format!("read the variable {:?}, whose name is not UTF-8", String::from_utf8_lossy(name))));
            };
            let name = name.to_owned();
            match host.sandbox.var(&name) {
                Ok(Some(value)) => give(mem, into, capacity, value.as_bytes()),
                Ok(None) => Ok(-1),
                // Kept in `sandbox.env_refused`: `stopped_by` gives it as the call's error.
                Err(refused) => Err(Stop(refused.to_string())),
            }
        })
    })?;
    l.func_wrap(IMPORTS, "current_dir", |mut c: Caller<'_, Host>, into: u32, capacity: u32| {
        with(&mut c, |mem, host| {
            let dir = host.sandbox.package().cwd();
            give(mem, into, capacity, dir.as_bytes())
        })
    })?;
    refused_wasi(l)
}

/// The WASI module a proc macro imports from (wasm32-wasip1: 32-bit pointers).
const WASI: &str = <W32 as blaze_wasi::Width>::MODULE;

/// The preview 1 calls a proc macro may import but the sandbox permits none of (rule 9: it has
/// no writable file system, no socket and no signal): each is defined, so a module that links
/// one loads, and a call answers `EPERM`, which the macro sees as any refused call. napi-derive
/// 3.5.2 (turso, 2026-11-21) imports `path_unlink_file` (its type-definition file) and did not
/// load at all ("unknown import"). With [`blaze_wasi::FUNCTIONS`] and [`UNSERVED_WASI`] these
/// are the whole of preview 1 (tests.rs checks it), so no import of it fails a load.
pub(crate) const REFUSED_WASI: &[&str] = &[
    "fd_allocate",
    "fd_datasync",
    "fd_fdstat_set_rights",
    "fd_filestat_set_size",
    "fd_filestat_set_times",
    "fd_pwrite",
    "fd_sync",
    "path_create_directory",
    "path_filestat_set_times",
    "path_link",
    "path_remove_directory",
    "path_rename",
    "path_symlink",
    "path_unlink_file",
    "proc_raise",
    "sock_accept",
    "sock_recv",
    "sock_send",
    "sock_shutdown",
];

/// The preview 1 calls the sandbox permits in kind but does not serve: a positioned read
/// (`fd_pread`: the sandbox's reads are `fd_read`), a renumbered descriptor and an advice. A
/// call answers `ENOTSUP`, a named errno, never a guess at what the call would have done.
pub(crate) const UNSERVED_WASI: &[&str] = &["fd_advise", "fd_pread", "fd_renumber"];

/// Defines [`REFUSED_WASI`] and [`UNSERVED_WASI`] with preview 1's signatures (`witx`): a wrong
/// signature would fail the load as the missing import did.
fn refused_wasi(l: &mut Linker<Host>) -> wasmtime::Result<()> {
    let perm = i32::from(blaze_wasi::Errno::PERM.0);
    let notsup = i32::from(blaze_wasi::Errno::NOTSUP.0);
    // (fd)
    for name in ["fd_datasync", "fd_sync", "proc_raise"] {
        l.func_wrap(WASI, name, move |_: u32| -> i32 { perm })?;
    }
    // (fd, to) and (fd, how)
    for (name, errno) in [("fd_renumber", notsup), ("sock_shutdown", perm)] {
        l.func_wrap(WASI, name, move |_: u32, _: u32| -> i32 { errno })?;
    }
    // (fd, path, path_len) and (fd, flags, fd_out)
    for name in ["path_create_directory", "path_remove_directory", "path_unlink_file", "sock_accept"] {
        l.func_wrap(WASI, name, move |_: u32, _: u32, _: u32| -> i32 { perm })?;
    }
    // (fd, size)
    l.func_wrap(WASI, "fd_filestat_set_size", move |_: u32, _: u64| -> i32 { perm })?;
    // (fd, offset, len) and (fd, base, inheriting)
    for name in ["fd_allocate", "fd_fdstat_set_rights"] {
        l.func_wrap(WASI, name, move |_: u32, _: u64, _: u64| -> i32 { perm })?;
    }
    // (fd, offset, len, advice) and (fd, atim, mtim, fst_flags)
    for (name, errno) in [("fd_advise", notsup), ("fd_filestat_set_times", perm)] {
        l.func_wrap(WASI, name, move |_: u32, _: u64, _: u64, _: u32| -> i32 { errno })?;
    }
    // (fd, iovs, iovs_len, offset, out)
    for (name, errno) in [("fd_pread", notsup), ("fd_pwrite", perm)] {
        l.func_wrap(WASI, name, move |_: u32, _: u32, _: u32, _: u64, _: u32| -> i32 { errno })?;
    }
    // (fd, flags, path, path_len, atim, mtim, fst_flags)
    l.func_wrap(WASI, "path_filestat_set_times", move |_: u32, _: u32, _: u32, _: u32, _: u64, _: u64, _: u32| -> i32 { perm })?;
    // (old_fd, old_flags, old_path, old_len, new_fd, new_path, new_len)
    l.func_wrap(WASI, "path_link", move |_: u32, _: u32, _: u32, _: u32, _: u32, _: u32, _: u32| -> i32 { perm })?;
    // (fd, old_path, old_len, new_fd, new_path, new_len) and (fd, data, data_len, flags, len_out, flags_out)
    for name in ["path_rename", "sock_recv"] {
        l.func_wrap(WASI, name, move |_: u32, _: u32, _: u32, _: u32, _: u32, _: u32| -> i32 { perm })?;
    }
    // (old_path, old_len, fd, new_path, new_len) and (fd, data, data_len, flags, len_out)
    for name in ["path_symlink", "sock_send"] {
        l.func_wrap(WASI, name, move |_: u32, _: u32, _: u32, _: u32, _: u32| -> i32 { perm })?;
    }
    Ok(())
}

/// Why one call gave no expansion. Its words ([`std::fmt::Display`]) name what stopped it as
/// the macro saw it (a path from the tree it is served, `/workspace/..` or `/package/..`, a
/// variable's name): never a path on this machine, so they may stand in a record (rule 25).
#[derive(Clone, Debug)]
pub enum CallError {
    /// A limit it ran past.
    Limit { limit: ProcLimit, why: String },
    /// A question the host refuses (a position in a file).
    Refused(String),
    /// A variable read whose value this build does not know (package_env.rs): refused by name,
    /// never answered unset.
    EnvUnknown(EnvRefused),
    /// The module broke the protocol, trapped or stopped otherwise.
    Failed(String),
}

impl CallError {
    /// The error blaze-expand takes.
    pub fn host_error(&self) -> HostError {
        let why = match self {
            CallError::Limit { limit, .. } => HostWhy::Limit(*limit),
            CallError::Refused(_) | CallError::EnvUnknown(_) => HostWhy::Refused,
            CallError::Failed(_) => HostWhy::Failed,
        };
        HostError { ask: HostAsk::ProcMacro, why }
    }
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // The sandbox's own refusal (rule 9): its words name the path as the macro named it.
            CallError::Limit { limit: ProcLimit::Paths, why } => write!(f, "was refused a path by its sandbox: {why}"),
            CallError::Limit { limit, why } => write!(f, "ran past its {limit:?} limit: {why}"),
            CallError::Refused(why) => write!(f, "asked what blaze does not answer: {why}"),
            CallError::EnvUnknown(refused) => write!(f, "{refused}"),
            CallError::Failed(why) => write!(f, "failed: {why}"),
        }
    }
}

/// A [`CallError`] with the macro and the call it is about.
#[derive(Debug)]
pub struct ExpandError {
    /// `crate version`.
    pub krate: String,
    /// The macro's name and kind.
    pub name: String,
    pub error: CallError,
}

impl std::fmt::Display for ExpandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the proc macro `{}` of {} {}", self.name, self.krate, self.error)
    }
}

impl std::error::Error for ExpandError {}

/// What one call gave, with what it spent.
pub struct Ran<'a> {
    pub produced: Produced<'a>,
    pub fuel: u64,
    pub peak_memory: usize,
    pub requests: u64,
}

impl Runtime {
    /// Runs `export` of `loaded` on `call`, in `package`, within `limits`.
    pub fn expand<'a>(
        &self,
        loaded: &Loaded,
        export: &Exported,
        call: &ProcCall<'_>,
        edition: Edition,
        package: Arc<Package>,
        limits: Limits,
        alloc: Alloc<'a>,
    ) -> Result<Ran<'a>, ExpandError> {
        let named = |error| ExpandError { krate: loaded.crate_name.clone(), name: format!("{} ({:?})", export.name, export.kind), error };
        let failed = |why: String| named(CallError::Failed(why));
        let mut mark = self.parts.start();
        let mut run_input = Vec::new();
        let server = Server::new(edition, limits.server, call, self.heap.alloc(), &mut run_input).map_err(|error| match error {
            ServerError::Limit(limit) => named(CallError::Limit { limit, why: error.to_string() }),
            _ => failed(format!("its input could not be served: {error}")),
        })?;
        mark.lap(Part::Serve);
        let host = Host {
            server,
            sandbox: Sandbox::new(package),
            memory: None,
            limiter: Limiter { memory: limits.memory, peak: 0, over: false },
            output_limit: limits.output,
            reply: Vec::new(),
            output: None,
            stop: None,
            server_error: None,
            over_output: false,
            requests: 0,
            clock: self.parts.clock(),
            bridge_nanos: 0,
        };
        let mut store = Store::new(&self.engine, host);
        store.limiter(|host| &mut host.limiter);
        store.set_fuel(limits.fuel).map_err(|e| failed(format!("{e:#}")))?;
        mark.lap(Part::Setup);
        let ran = run(&mut store, loaded, export.index, &run_input, &mut mark);
        // The entry's call, or the part an error stopped in: the bridge's share of it is the
        // host's, the rest the macro's own code.
        let in_call = mark.split();
        let bridge = store.data().bridge_nanos.min(in_call);
        self.parts.add(Part::Guest, in_call - bridge);
        self.parts.add_many(Part::Bridge, bridge, store.data().requests);
        let left = store.get_fuel().unwrap_or(0);
        let fuel = limits.fuel.saturating_sub(left);
        let mut host = store.into_data();
        mark.lap(Part::Free);
        let outcome = match ran {
            Ok(()) => match host.output.take() {
                Some(bytes) => host.server.outcome(&bytes).map_err(|e| failed(format!("its output is not the bridge's: {e}")))?,
                None => return Err(failed("returned without giving an output".to_owned())),
            },
            Err(error) => stopped_by(&mut host, &error, fuel >= limits.fuel).map_err(named)?,
        };
        mark.lap(Part::Outcome);
        // What it read: the sandbox's reads and the variables and paths it tracked.
        for tracked in std::mem::take(&mut host.server.tracked) {
            match tracked {
                Tracked::Env(name) => {
                    if let Err(refused) = host.sandbox.var(&name) {
                        return Err(named(CallError::EnvUnknown(refused)));
                    }
                }
                Tracked::Path(path) => {
                    if host.sandbox.track_path(&path).is_err()
                        && let Some((limit, why)) = host.sandbox.refused.take()
                    {
                        return Err(named(CallError::Limit { limit, why }));
                    }
                }
            }
        }
        let mut reads = Reads::new_in(alloc);
        for read in &host.sandbox.reads {
            let answer = read.digest.map_or(ReadAnswer::Absent, ReadAnswer::Digest);
            reads.push(ReadKind::ProcMacro, &read.subject, answer).map_err(|e| failed(format!("its reads do not fit: {e:?}")))?;
        }
        // `Span::file` answered (server.rs, only where the call's file is known): the call's
        // file is a read of the run, last, so a kept answer is taken only for a call whose
        // file is shown the same (table.rs `list_holds`). Moving the call to another file
        // runs it again; an edit elsewhere in its file does not.
        if let (true, Ok(file)) = (host.server.asked_file, call.file) {
            let answer = ReadAnswer::Digest(crate::sandbox::call_file_digest(file));
            reads.push(ReadKind::ProcMacro, crate::sandbox::CALL_FILE, answer).map_err(|e| failed(format!("its reads do not fit: {e:?}")))?;
        }
        mark.lap(Part::Reads);
        let (handle, panicked) = match &outcome {
            Outcome::Tokens(handle) => (*handle, None),
            Outcome::Panicked(message) => (None, Some(message.as_str())),
        };
        let produced = host.server.produce(handle, panicked, reads, alloc).map_err(|error| match error {
            ProduceError::Server(ServerError::Limit(limit)) => named(CallError::Limit { limit, why: format!("{error:?}") }),
            _ => failed(format!("its output does not make tokens: {error:?}")),
        })?;
        mark.lap(Part::Produce);
        Ok(Ran { produced, fuel, peak_memory: host.limiter.peak, requests: host.requests })
    }
}

/// What an instance that ended in `error` gave: the panic its hook gave before it aborted,
/// or why it stopped.
fn stopped_by(host: &mut Host, error: &wasmtime::Error, out_of_fuel: bool) -> Result<Outcome, CallError> {
    if let Some(refused) = host.sandbox.env_refused.take() {
        return Err(CallError::EnvUnknown(refused));
    }
    if let Some((limit, why)) = host.sandbox.refused.take() {
        return Err(CallError::Limit { limit, why });
    }
    if let Some(server_error) = host.server_error.take() {
        return Err(match server_error {
            ServerError::Limit(limit) => CallError::Limit { limit, why: server_error.to_string() },
            ServerError::Position { .. } | ServerError::FileUnknown { .. } | ServerError::FileOfSite | ServerError::FileOfMadeToken => {
                CallError::Refused(server_error.to_string())
            }
            _ => CallError::Failed(format!("its request was not the bridge's: {server_error}")),
        });
    }
    if host.over_output {
        let why = host.stop.take().map_or_else(String::new, |it| it.0);
        return Err(CallError::Limit { limit: ProcLimit::Output, why });
    }
    if let Some(stop) = host.stop.take() {
        return Err(CallError::Failed(stop.0));
    }
    let trap = error.downcast_ref::<Trap>().copied();
    if trap == Some(Trap::OutOfFuel) || out_of_fuel {
        return Err(CallError::Limit { limit: ProcLimit::Fuel, why: "ran out of fuel: it may loop forever".to_owned() });
    }
    if host.limiter.over {
        return Err(CallError::Limit { limit: ProcLimit::Memory, why: format!("asked for more than {} bytes of memory: {error:#}", host.limiter.memory) });
    }
    if let Some(bytes) = host.output.take() {
        return match host.server.outcome(&bytes) {
            Ok(Outcome::Panicked(message)) => Ok(Outcome::Panicked(message)),
            Ok(Outcome::Tokens(_)) => Err(CallError::Failed(format!("trapped after it gave its output: {error:#}"))),
            Err(e) => Err(CallError::Failed(format!("its output is not the bridge's: {e}"))),
        };
    }
    Err(CallError::Failed(format!("trapped: {error:#}")))
}

/// The instance's part of [`Runtime::expand`].
fn run(store: &mut Store<Host>, loaded: &Loaded, index: u32, input: &[u8], mark: &mut Mark<'_>) -> wasmtime::Result<()> {
    let instance = loaded.pre.instantiate(&mut *store)?;
    let Some(memory) = instance.get_memory(&mut *store, "memory") else {
        return Err(wasmtime::Error::msg("the module exports no memory"));
    };
    store.data_mut().memory = Some(memory);
    mark.lap(Part::Instantiate);
    // A reactor's constructors and wasi-libc's start-up.
    let init: TypedFunc<(), ()> = instance.get_typed_func(&mut *store, "_initialize")?;
    init.call(&mut *store, ())?;
    mark.lap(Part::Initialize);
    let Some(global) = instance.get_global(&mut *store, &loaded.decls) else {
        return Err(wasmtime::Error::msg(format!("the module exports no `{}`", loaded.decls)));
    };
    let Val::I32(decls) = global.get(&mut *store) else {
        return Err(wasmtime::Error::msg(format!("`{}` is not an i32", loaded.decls)));
    };
    let alloc: TypedFunc<u32, u32> = instance.get_typed_func(&mut *store, "blaze_proc_macro_alloc")?;
    let expand: TypedFunc<(u32, u32, u32, u32), u32> = instance.get_typed_func(&mut *store, "blaze_proc_macro_expand")?;
    let Ok(len) = u32::try_from(input.len()) else {
        return Err(wasmtime::Error::msg("its input is more than a 32-bit memory holds"));
    };
    mark.lap(Part::Exports);
    let at = alloc.call(&mut *store, len)?;
    memory.write(&mut *store, at as usize, input)?;
    mark.lap(Part::Input);
    match expand.call(&mut *store, (decls as u32, index, at, len))? {
        0 => Ok(()),
        1 => Err(wasmtime::Error::msg(format!("the module has no macro at index {index}"))),
        status if status & 0xff == 2 => {
            let errno = status >> 8;
            let name = u16::try_from(errno).ok().and_then(|it| blaze_wasi::Errno(it).name()).unwrap_or("?");
            Err(wasmtime::Error::msg(format!("the shim could not enter its package's folder: {name} (WASI errno {errno})")))
        }
        status => Err(wasmtime::Error::msg(format!("the module returned status {status}, which the shim has not"))),
    }
}
