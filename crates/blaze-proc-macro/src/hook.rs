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

use crate::{
    host::{CallError, CompiledFinding, ExpandError, Limits, Runtime, RuntimeError},
    module::{Exported, ModuleError, ModuleRecord},
    record::{DirStore, RecordError, base_key, find, keep},
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
}

/// The hook: see the module's doc.
pub struct ProcMacros<'r> {
    runtime: &'r Runtime,
    store: &'r DirStore,
    limits: Limits,
    crates: Vec<MacroCrate>,
    findings: Mutex<Vec<Finding>>,
}

impl<'r> ProcMacros<'r> {
    pub fn new(runtime: &'r Runtime, store: &'r DirStore, limits: Limits) -> ProcMacros<'r> {
        ProcMacros { runtime, store, limits, crates: Vec::new(), findings: Mutex::new(Vec::new()) }
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
        let with: Vec<&MacroCrate> = self.crates.iter().filter(|it| it.module.is_ok()).collect();
        let modules: Vec<&ModuleRecord> = with.iter().filter_map(|it| it.module.as_ref().ok()).collect();
        let before = self.runtime.compiles();
        for (macro_crate, loaded) in with.iter().zip(self.runtime.load_all(&modules, workers)?) {
            if let Err(error) = loaded
                && !macro_crate.reported.swap(true, Ordering::Relaxed)
            {
                self.report(Finding::Unloadable { krate: macro_crate.label.clone(), error });
            }
        }
        Ok(self.runtime.compiles() - before)
    }

    fn report(&self, finding: Finding) {
        self.findings.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(finding);
    }

    /// Runs the proc macro `def` on `call`, expanded in `package` at `edition`: from its record,
    /// or run and kept. Its error is what blaze-expand takes; the finding it made is kept.
    pub fn run<'a>(&self, def: &MacroDefinition<'_>, call: &ProcCall<'_>, edition: Edition, package: &Arc<Package>, alloc: Alloc<'a>) -> Result<Produced<'a>, HostError> {
        let refused = HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused };
        let failed = HostError { ask: HostAsk::ProcMacro, why: HostWhy::Failed };
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
        let base = base_key(&module.wasm_sha256, export, edition, call);
        match find(self.store, &base, package, edition, alloc) {
            Ok(Some(produced)) => return Ok(produced),
            Ok(None) => {}
            Err(error) => {
                self.report(Finding::Record { base, error });
                return Err(failed);
            }
        }
        let loaded = match self.runtime.load(module) {
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
                if let Err(error) = keep(self.store, base, &ran.produced) {
                    self.report(Finding::Record { base, error });
                }
                Ok(ran.produced)
            }
            Err(error) => {
                let host = error.error.host_error();
                self.report(Finding::Call { base, error });
                Err(host)
            }
        }
    }
}

/// Whether `error` is a limit, a refusal or a failure: [`CallError::host_error`]'s `why`.
pub fn cause(error: &CallError) -> HostWhy {
    error.host_error().why
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
