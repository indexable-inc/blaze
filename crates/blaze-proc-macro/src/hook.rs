//! The proc-macro hook of the expansion pipeline: what an [`blaze_expand::ExpandCx`]'s
//! `run_proc_macro` calls, so a derive, attribute or function-like proc macro met while a crate
//! is expanded is run (or found in its record) and blaze-expand checks and places its output.
//!
//! - [`ProcMacros`] holds, for one build, the macro crates a caller registered
//!   ([`ProcMacros::add_crate`]), each with its module record or why it has none, and the
//!   runtime and record store every worker shares. A module is compiled or loaded from its
//!   compiled record once per runtime ([`Runtime::load`]): ahead, by [`ProcMacros::load_all`]
//!   before expansions start, so no call waits on a compile.
//! - [`ProcMacros::run`] is one call: the base key from text, a lookup, and on a miss a run in
//!   the module's sandbox under [`Limits`] and the record kept. Its error is blaze-expand's
//!   [`HostError`]; what it was about is kept as a [`Finding`] naming the macro and the call's
//!   base key, which the caller turns into a finding at the call's place.
//! - A crate with no module is ONE finding, [`Finding::MissingModule`], naming the crate and
//!   its version, however many calls meet it; each call is refused.
//! - A macro's panic and its own diagnostics are in its output ([`Produced::diagnostics`]):
//!   blaze-expand makes them the expansion's diagnostics on the call.
//!
//! Fetching or building a module is not this: a module the store lacks stays missing.

#![expect(
    clippy::disallowed_types,
    reason = "a std Mutex holds the findings across the expansion's threads; blaze_core::lock is not a dependency of this crate"
)]

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use blaze_expand::{
    Edition, ExpandCx, HostAsk, HostError, HostWhy, IncludeAs, Included, MacroDefinition, MacroKind, Place, PlaceOf, ProcCall, ProcKind,
    Produced, TokenStream,
};
use blaze_id::{ContentId, CrateItemKey};
use blaze_own::Alloc;
use bstd::sync::PublishOnce;

use crate::{
    host::{CallError, CompiledFinding, ExpandError, Limits, Loaded, Runtime, RuntimeError},
    module::{Exported, ModuleError, ModuleRecord},
    parts::Part,
    record::{
        DirStore, RecordError, base_key,
        table::{self, CrateResults, Kept, Verdicts},
    },
    sandbox::Package,
};

/// A macro crate a build reaches, as its unit names it: what its module is keyed by, but the
/// lock closure, which [`ProcMacros::add_reached`] reads.
#[derive(Clone, Copy, Debug)]
pub struct Reached<'c> {
    pub name: &'c str,
    pub version: &'c str,
    /// `Registry { checksum }` from the lock; `Git { commit, path }`; for a workspace or path
    /// crate `Path { files_sha256 }`, [`crate::module::files_digest`] of
    /// [`crate::module::package_files`] of its folder as it is now.
    pub source: crate::module::Source<'c>,
    /// The host features the unit graph unified it to.
    pub features: &'c [&'c str],
}

/// A macro crate [`ProcMacros::add_reached`] could not register.
#[derive(Debug)]
pub struct AddError {
    pub name: String,
    pub version: String,
    pub why: AddWhy,
}

#[derive(Debug)]
pub enum AddWhy {
    /// Its dependency closure is not in the lock.
    Closure(crate::module::ClosureError),
    /// Its module is in the store but cannot be read.
    Module(ModuleError),
}

impl std::fmt::Display for AddError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.why {
            AddWhy::Closure(e) => write!(f, "macro crate {} {}: its dependency closure: {e:?}", self.name, self.version),
            AddWhy::Module(e) => write!(f, "macro crate {} {}: {e}", self.name, self.version),
        }
    }
}

impl std::error::Error for AddError {}

/// The key a macro crate's definitions name it by: its module identity's key, hashed into the
/// expander's crate key, so the same crate at the same identity is one crate everywhere.
pub fn crate_key(module_key: &str) -> CrateItemKey {
    CrateItemKey(ContentId::of("blaze proc-macro crate", module_key.as_bytes()))
}

/// What a call's run found, for the caller to report at the call's place.
#[derive(Debug)]
pub enum Finding {
    /// The macro crate has no module: reported once per crate.
    MissingModule { name: String, version: String, why: String },
    /// The module did not compile or load: reported once per crate.
    Unloadable { krate: String, error: RuntimeError },
    /// A compiled record was not loaded (removed, and its module compiled again).
    Compiled(CompiledFinding),
    /// The call named a crate the caller did not register.
    UnknownCrate { krate: CrateItemKey },
    /// The module exports no macro of that name and kind.
    NoSuchMacro { krate: String, name: String, kind: ProcKind },
    /// The run stopped: a limit, a refusal or a failure, naming the macro.
    Call { base: ContentId, error: ExpandError },
    /// The expansion ran but its record could not be read or kept.
    Record { base: ContentId, error: RecordError },
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Finding::MissingModule { why, .. } => write!(f, "{why}"),
            Finding::Unloadable { krate, error } => write!(f, "the proc-macro module of {krate} does not load: {error}"),
            Finding::Compiled(finding) => write!(f, "{finding}"),
            Finding::UnknownCrate { krate } => write!(f, "a proc macro of the crate {:x}, which this build did not register", krate.0),
            Finding::NoSuchMacro { krate, name, kind } => write!(f, "{krate} exports no {kind:?} macro `{name}`"),
            Finding::Call { base, error } => write!(f, "{error} (call {base:x})"),
            Finding::Record { base, error } => write!(f, "{error} (call {base:x})"),
        }
    }
}

/// One registered macro crate.
struct MacroCrate {
    key: CrateItemKey,
    /// `name version`.
    label: String,
    module: Result<ModuleRecord, ModuleError>,
    /// Whether its missing or unloadable module was reported.
    reported: AtomicBool,
    /// Its module as the runtime loaded it, for the command: asked of the runtime once (its
    /// compiled record found by the wasm's digest and the engine's settings), then one read
    /// of this cell a call. Empty while no call has loaded it, and after a load that failed
    /// (the runtime keeps the failure: the next call is told it again).
    loaded: PublishOnce<Arc<Loaded>>,
}

impl MacroCrate {
    /// See [`MacroCrate::loaded`].
    fn loaded(&self, runtime: &Runtime, module: &ModuleRecord) -> Result<Arc<Loaded>, RuntimeError> {
        if let Some(loaded) = self.loaded.get() {
            // clone: the handle, a refcount.
            return Ok(Arc::clone(loaded));
        }
        let loaded = runtime.load(module)?;
        // clone: the handle, a refcount. A worker that published first published the same
        // handle (the runtime loads a module once), so the one given back is dropped.
        drop(self.loaded.set(Arc::clone(&loaded)));
        Ok(loaded)
    }
}

/// The hook: see the module's doc.
pub struct ProcMacros<'r> {
    runtime: &'r Runtime,
    store: &'r DirStore,
    limits: Limits,
    crates: Vec<MacroCrate>,
    findings: Mutex<Vec<Finding>>,
    /// Each package a call was expanded in, with its crate name's results and its own
    /// verdicts on them: sorted by the package's address, which a slot keeps alive, so a
    /// package is found by identity with no hash of it a call.
    /// DEBT: prelude `Vec`s here and in the table's rows, not blaze-own's with an allocator
    /// passed: `ProcMacros::new` takes none (its callers are other lanes' drivers).
    slots: Mutex<Vec<Arc<CrateSlot>>>,
    /// Each crate name's results as this command found them, with the rows it ran
    /// ([`crate::record::table`]): sorted by the name, one for every package of that name
    /// (two packages with a table each would each write it, the second without the first's
    /// rows).
    tables: Mutex<Vec<Arc<CrateResults>>>,
}

/// One package's results: see [`ProcMacros::slots`].
struct CrateSlot {
    package: Arc<Package>,
    results: Arc<CrateResults>,
    /// This package's verdict on each read list of `results`' table.
    verdicts: Verdicts,
}

/// What [`ProcMacros::keep_all`] wrote.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct KeptAll {
    /// Crates whose table was written again.
    pub tables: usize,
    /// The rows those tables hold, and their bytes with the seals.
    pub rows: usize,
    pub bytes: usize,
}

impl<'r> ProcMacros<'r> {
    pub fn new(runtime: &'r Runtime, store: &'r DirStore, limits: Limits) -> ProcMacros<'r> {
        ProcMacros {
            runtime,
            store,
            limits,
            crates: Vec::new(),
            findings: Mutex::new(Vec::new()),
            slots: Mutex::new(Vec::new()),
            tables: Mutex::new(Vec::new()),
        }
    }

    /// The results filed under the crate name `krate`: opened at the name's first call (one
    /// read of its table, proved once), shared after.
    fn results(&self, krate: blaze_ids::ContentId) -> Result<Arc<CrateResults>, RecordError> {
        let held = |tables: &Vec<Arc<CrateResults>>| {
            let at = tables.partition_point(|it| it.krate().0 < krate.0);
            // clone: the table, a refcount.
            (at, tables.get(at).filter(|it| it.krate() == krate).map(Arc::clone))
        };
        if let (_, Some(results)) = held(&crate::host::lock(&self.tables)) {
            return Ok(results);
        }
        // Opened outside the lock: another worker's calls do not wait on this crate's read.
        let made = Arc::new(table::open(self.store, krate, self.runtime.heap().shared())?);
        let mut tables = crate::host::lock(&self.tables);
        match held(&tables) {
            // Another worker opened it meanwhile: its table is the name's, this one is dropped.
            (_, Some(results)) => Ok(results),
            (at, None) => {
                // clone: the table, a refcount.
                tables.insert(at, Arc::clone(&made));
                Ok(made)
            }
        }
    }

    /// The results of the crate `package` is: opened at the package's first call (one read
    /// of the crate's table, proved once), found by the package's identity after.
    fn slot(&self, package: &Arc<Package>) -> Result<Arc<CrateSlot>, RecordError> {
        let address = |slot: &Arc<CrateSlot>| Arc::as_ptr(&slot.package).addr();
        let wanted = Arc::as_ptr(package).addr();
        {
            let slots = crate::host::lock(&self.slots);
            let at = slots.partition_point(|slot| address(slot) < wanted);
            if let Some(slot) = slots.get(at).filter(|slot| address(slot) == wanted) {
                // clone: the slot, a refcount.
                return Ok(Arc::clone(slot));
            }
        }
        let results = self.results(table::crate_name(package))?;
        let verdicts = Verdicts::of(&results);
        // clone: the package, a refcount the slot holds so its address stays this package's.
        let made = Arc::new(CrateSlot { package: Arc::clone(package), results, verdicts });
        let mut slots = crate::host::lock(&self.slots);
        let at = slots.partition_point(|slot| address(slot) < wanted);
        if let Some(slot) = slots.get(at).filter(|slot| address(slot) == wanted) {
            // Another worker opened it meanwhile: its slot is the crate's, this one is dropped.
            return Ok(Arc::clone(slot));
        }
        slots.insert(at, Arc::clone(&made));
        Ok(made)
    }

    /// Writes the results table of every crate this command ran a new call in: each whole,
    /// under a temporary name, renamed onto the crate's. A driver calls it when its crates
    /// are expanded (and may again: a crate with nothing new since is not written).
    pub fn keep_all(&self) -> Result<KeptAll, RecordError> {
        let mut mark = self.runtime.parts().start();
        // clone: the slots and tables, a refcount each: no table is written under a lock.
        let slots: Vec<Arc<CrateSlot>> = crate::host::lock(&self.slots).clone();
        let tables: Vec<Arc<CrateResults>> = crate::host::lock(&self.tables).clone();
        let mut kept = KeptAll::default();
        for results in &tables {
            let verdicts: Vec<&Verdicts> = slots.iter().filter(|slot| Arc::ptr_eq(&slot.results, results)).map(|slot| &slot.verdicts).collect();
            if let Kept::Written { rows, bytes } = results.keep(self.store, &verdicts)? {
                kept.tables += 1;
                kept.rows += rows;
                kept.bytes += bytes;
            }
        }
        mark.lap(Part::Keep);
        Ok(kept)
    }

    /// Registers a macro crate `name version` with its module (as [`crate::module::read_module`]
    /// gave it) under `key`, the crate key its definitions carry. A crate with no module is
    /// registered all the same: its calls are refused, and it is reported once.
    pub fn add_crate(&mut self, key: CrateItemKey, name: &str, version: &str, module: Result<ModuleRecord, ModuleError>) {
        self.crates.push(MacroCrate {
            key,
            label: format!("{name} {version}"),
            module,
            reported: AtomicBool::new(false),
            loaded: PublishOnce::new(),
        });
    }

    /// Registers the macro crate `reached` names, its module read from `store` (the module
    /// store's root: `proc-macro/modules/<key>` under it) by the identity the bootstrap builder
    /// keys it by: its source, its dependency closure in `lock` (the workspace's `Cargo.lock`
    /// text) and its host features. Gives the crate key its definitions must carry. A missing
    /// module is registered all the same (one finding at its first call); a crate the lock
    /// does not hold, or a module that cannot be read, is an error naming it.
    pub fn add_reached(&mut self, store: &std::path::Path, lock: &str, reached: &Reached<'_>) -> Result<CrateItemKey, AddError> {
        let named = |why| AddError { name: reached.name.to_owned(), version: reached.version.to_owned(), why };
        let deps = crate::module::lock_closure(lock, reached.name, reached.version).map_err(|e| named(AddWhy::Closure(e)))?;
        let id = crate::module::CrateId { name: reached.name, version: reached.version, source: reached.source, deps: &deps, features: reached.features };
        let key = crate_key(&crate::module::module_key(&id));
        match crate::module::read_module(store, &id) {
            Err(e) if !matches!(e, ModuleError::Missing { .. }) => Err(named(AddWhy::Module(e))),
            module => {
                self.add_crate(key, reached.name, reached.version, module);
                Ok(key)
            }
        }
    }

    /// The macros `key`'s module exports; none when it has no module.
    pub fn exports(&self, key: CrateItemKey) -> &[Exported] {
        match self.crates.iter().find(|it| it.key == key).map(|it| &it.module) {
            Some(Ok(module)) => &module.macros,
            _ => &[],
        }
    }

    /// Each registered crate: its key, `name version`, and whether it has a module.
    pub fn crates(&self) -> impl Iterator<Item = (CrateItemKey, &str, bool)> {
        self.crates.iter().map(|it| (it.key, it.label.as_str(), it.module.is_ok()))
    }

    /// The findings so far, taken, with the compiled records the runtime refused.
    pub fn take_findings(&self) -> Vec<Finding> {
        let mut findings = std::mem::take(&mut *self.findings.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
        findings.extend(self.runtime.take_findings().into_iter().map(Finding::Compiled));
        findings
    }

    /// Every registered crate's module loaded on `workers` threads before any expansion
    /// starts ([`Runtime::load_all`]), so no call waits on a compile. A module that does not
    /// load is reported once, as its first call would. Gives how many modules were compiled
    /// (the rest came from their compiled records).
    pub fn load_all(&self, workers: usize) -> Result<u64, RuntimeError> {
        // The crates with a module, in the order `modules` lists theirs: the loads come back so.
        let modules: Vec<&ModuleRecord> = self.crates.iter().filter_map(|it| it.module.as_ref().ok()).collect();
        let before = self.runtime.compiles();
        let with = self.crates.iter().filter(|it| it.module.is_ok());
        for (macro_crate, loaded) in with.zip(self.runtime.load_all(&modules, workers)?) {
            if let Err(error) = loaded
                && !macro_crate.reported.swap(true, Ordering::Relaxed)
            {
                self.report(Finding::Unloadable { krate: macro_crate.label.clone(), error });
            }
        }
        Ok(self.runtime.compiles() - before)
    }

    /// Where the calls' time went so far ([`crate::parts`]).
    pub fn parts(&self) -> &crate::parts::Parts {
        self.runtime.parts()
    }

    fn report(&self, finding: Finding) {
        self.findings.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(finding);
    }

    /// Why `def`'s crate cannot run it, when that is why a call of it was refused: the same
    /// finding [`ProcMacros::run`] files for it (the crate unregistered, its module missing under
    /// the key asked or unreadable, no such export, a module that does not load), so the caller
    /// can name the reason at the call (blaze-scope call_refusal.rs). `None`: the module can run
    /// it (a refusal was the run's own), or `def` is no proc macro. Asks nothing new: a load is
    /// the runtime's kept answer.
    pub fn refusal_of(&self, def: &MacroDefinition<'_>) -> Option<Finding> {
        let MacroKind::ProcMacro { krate, kind } = def.kind() else { return None };
        let Some(macro_crate) = self.crates.iter().find(|it| it.key == krate) else { return Some(Finding::UnknownCrate { krate }) };
        let module = match &macro_crate.module {
            Ok(module) => module,
            Err(error) => {
                let (name, version) = macro_crate.label.split_once(' ').unwrap_or((&macro_crate.label, ""));
                return Some(Finding::MissingModule { name: name.to_owned(), version: version.to_owned(), why: error.to_string() });
            }
        };
        let name = def.proc_name().unwrap_or("");
        if module.find(kind, name).is_none() {
            return Some(Finding::NoSuchMacro { krate: macro_crate.label.clone(), name: name.to_owned(), kind });
        }
        match macro_crate.loaded(self.runtime, module) {
            Ok(_) => None,
            Err(error) => Some(Finding::Unloadable { krate: macro_crate.label.clone(), error }),
        }
    }

    /// Runs the proc macro `def` on `call`, expanded in `package` at `edition`: from its record,
    /// or run and kept. Its error is what blaze-expand takes; the finding it made is kept.
    pub fn run<'a>(&self, def: &MacroDefinition<'_>, call: &ProcCall<'_>, edition: Edition, package: &Arc<Package>, alloc: Alloc<'a>) -> Result<Produced<'a>, HostError> {
        self.run_why(def, call, edition, package, alloc).map_err(|refused| refused.host)
    }

    /// [`ProcMacros::run`], whose error also holds, when the run itself stopped, why
    /// ([`RunRefused::call`]): what a caller that keeps a refused call's reason (blaze-cli's
    /// names jobs, into the names record's row) names it by (rule 19).
    pub fn run_why<'a>(&self, def: &MacroDefinition<'_>, call: &ProcCall<'_>, edition: Edition, package: &Arc<Package>, alloc: Alloc<'a>) -> Result<Produced<'a>, RunRefused> {
        let refused = RunRefused::bare(HostWhy::Refused);
        let failed = RunRefused::bare(HostWhy::Failed);
        let MacroKind::ProcMacro { krate, kind } = def.kind() else { return Err(refused) };
        let Some(macro_crate) = self.crates.iter().find(|it| it.key == krate) else {
            self.report(Finding::UnknownCrate { krate });
            return Err(refused);
        };
        let module = match &macro_crate.module {
            Ok(module) => module,
            Err(error) => {
                if !macro_crate.reported.swap(true, Ordering::Relaxed) {
                    let (name, version) = macro_crate.label.split_once(' ').unwrap_or((&macro_crate.label, ""));
                    self.report(Finding::MissingModule { name: name.to_owned(), version: version.to_owned(), why: error.to_string() });
                }
                return Err(refused);
            }
        };
        let name = def.proc_name().unwrap_or("");
        let Some(export) = module.find(kind, name) else {
            self.report(Finding::NoSuchMacro { krate: macro_crate.label.clone(), name: name.to_owned(), kind });
            return Err(refused);
        };
        let parts = self.runtime.parts();
        let mut mark = parts.start();
        let base = base_key(&module.wasm_sha256, export, edition, call);
        mark.lap(Part::Key);
        let found = self.slot(package).and_then(|slot| Ok((slot.results.find(self.store, &base, package, &slot.verdicts, edition, alloc)?, slot)));
        mark.lap(Part::Find);
        let slot = match found {
            Ok((Some(produced), _)) => return Ok(produced),
            Ok((None, slot)) => slot,
            Err(error) => {
                self.report(Finding::Record { base, error });
                return Err(failed);
            }
        };
        let loaded = macro_crate.loaded(self.runtime, module);
        mark.lap(Part::Load);
        let loaded = match loaded {
            Ok(loaded) => loaded,
            Err(error) => {
                if !macro_crate.reported.swap(true, Ordering::Relaxed) {
                    self.report(Finding::Unloadable { krate: macro_crate.label.clone(), error });
                }
                return Err(failed);
            }
        };
        match self.runtime.expand(&loaded, export, call, edition, package.clone(), self.limits, alloc) {
            Ok(ran) => {
                let mut mark = parts.start();
                let kept = slot.results.insert(self.store, base, &ran.produced);
                mark.lap(Part::Keep);
                if let Err(error) = kept {
                    self.report(Finding::Record { base, error });
                }
                Ok(ran.produced)
            }
            Err(error) => {
                let refused = RunRefused { host: error.error.host_error(), call: Some(error.error.clone()) };
                self.report(Finding::Call { base, error });
                Err(refused)
            }
        }
    }
}

/// A call [`ProcMacros::run_why`] gave no expansion: the error blaze-expand takes, and, when the
/// module ran and its run stopped (a limit, the sandbox refusing a path, a variable this build
/// does not know, a failure), that run's error, whose words name what stopped it as the macro
/// saw it. `None`: the run never started (the crate unregistered, its module missing or not
/// loading, no such export, its record unreadable): the finding kept, and the module's
/// refusal ([`ProcMacros::refusal_of`]), name why.
#[derive(Clone, Debug)]
pub struct RunRefused {
    pub host: HostError,
    pub call: Option<CallError>,
}

impl RunRefused {
    fn bare(why: HostWhy) -> RunRefused {
        RunRefused { host: HostError { ask: HostAsk::ProcMacro, why }, call: None }
    }
}

/// Whether `error` is a limit, a refusal or a failure: [`CallError::host_error`]'s `why`.
pub fn cause(error: &CallError) -> HostWhy {
    error.host_error().why
}

/// A kept answer's sandbox read asked again: what the read `subject` (a
/// [`blaze_expand::ReadKind::ProcMacro`] read of a [`Produced`]'s reads: `env:NAME`,
/// `file:path`, `dir:path` or `stat:path`, paths from the package's root) is answered by now
/// in `package`, in the form the run recorded it: [`blaze_expand::ReadAnswer::Absent`], or
/// [`blaze_expand::ReadAnswer::Digest`] of the variable's value, the file's bytes, the
/// folder's sorted listing, or the kind and length. A kept answer is today's exactly when
/// each of its reads is answered now as its list says. `Err`: a subject of no known form,
/// which no run records.
pub fn read_now(package: &Package, subject: &str) -> Result<blaze_expand::ReadAnswer, crate::sandbox::UnknownSubject> {
    crate::sandbox::answer_now(package, subject).map(|now| now.map_or(blaze_expand::ReadAnswer::Absent, blaze_expand::ReadAnswer::Digest))
}

/// An expansion's outside for one crate: `inner`'s environment, cfg, includes and places, and
/// its proc macros run by `procs` in `package` at `edition`.
pub struct ProcCx<'p, 'r, C> {
    pub inner: C,
    pub procs: &'p ProcMacros<'r>,
    pub package: Arc<Package>,
    pub edition: Edition,
}

impl<C: ExpandCx> ExpandCx for ProcCx<'_, '_, C> {
    fn env(&self, name: &str) -> Result<Option<&str>, HostError> {
        self.inner.env(name)
    }

    fn cfg(&self, predicate: &TokenStream<'_>) -> Result<bool, HostError> {
        self.inner.cfg(predicate)
    }

    fn include(&self, path: &str, how: IncludeAs) -> Result<Included<'_>, HostError> {
        self.inner.include(path, how)
    }

    fn place(&self, what: PlaceOf) -> Result<Place<'_>, HostError> {
        self.inner.place(what)
    }

    fn run_proc_macro<'a>(&self, def: &MacroDefinition<'_>, call: ProcCall<'_>, alloc: Alloc<'a>) -> Result<Produced<'a>, HostError> {
        self.procs.run(def, &call, self.edition, &self.package, alloc)
    }
}
