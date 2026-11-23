//! The command's proc-macro runner: the ONE path a function-like, attribute or derive proc
//! macro is expanded by, wherever its call is written (an item, a body): blaze-proc-macro's
//! hook, `ProcMacros::run`, which keys the call by the module's wasm digest, the exported
//! macro, the calling edition and the call's input tokens (`record::base_key`, rule 21), finds
//! its record or runs the module in its Wasmtime sandbox under `Limits::STANDARD` (rules 9, 10)
//! and keeps the answer (rule 51).
//!
//! Why it is here (pm-small, 2026-11-16 20:05, port27/tempo-deps/pm-6.out): `blaze check`
//! never registered a macro crate with the hook, so the names job's `JobTexts` answered
//! `SourceTexts::proc_macro_key` with the trait's default `None`, the macro crate's items
//! record held no runner key, and a body call of `mac::same!` was lowered through
//! `blaze_scope::resolve::definition_of`'s `MacroBody::Proc` arm with `runner: None`: the
//! finding "its source makes no definition: NoDefinition { shape: FnLike, name: \"same\" }".
//! And the body check's `Store::sources` was never set, so even with a key every body proc
//! macro was refused.
//!
//! A macro crate's key is its MODULE IDENTITY's (`hook::crate_key` of `module::module_key`),
//! the identity made by the same function the `MacroModule` task looks the module up by
//! (`blaze_world::procs::identify_files` over the member's `MacroFiles` row, macros.rs), so a
//! lookup, a definition and a run name one module (rule 30). The key is a function of the
//! identity alone (sources, dependency closure, features), never of whether the store holds
//! the module: a crate whose module is missing is registered all the same, each of its calls
//! is refused by the hook, and the `MacroModule` task's failure names the crate and the command
//! that builds its module (rule 3, 19), never `NoDefinition`.
//!
//! A call is routed by the calling crate's index in the names world, and the macro reads that
//! crate's own environment (`CrateEnv`, per unit: a package's lib and bin are two callers),
//! through proc_env.rs (macro-env-per-unit).
//!
//! Made once a command on the main thread, by the first `CrateScope` task after the names
//! world ([`make`], held in `Names`), and lent read-only to the pool's names and body jobs.
//! The calling crate's package (what the macro is shown) is where the names world placed it
//! (proc_callers.rs): a checkout folder, or a fetched package's record read in place.

use blaze_expand::{Edition, HostAsk, HostError, HostWhy, MacroDefinition, ProcCall, Produced};
use blaze_id::CrateItemKey;
use blaze_own::Alloc;

/// What the command gives [`make`]: the lock as read (its packages are what the dependency
/// closure of a module identity is walked over) and the macro crates' place (the home holding the module
/// store, the checkout, the members' folders and their `MacroFiles` rows).
pub(crate) struct MacroSide<'s, 'p, 'r> {
    pub lock: &'s crate::fetch::Lockfile<'s>,
    pub place: crate::macros::Place<'p, 'r>,
    /// The process's heap, handed from `main` (`FetchEnv::heap`): the runtime's.
    pub heap: &'static blaze_own::Heap,
}

/// Why the runner could not be made: names what and where (rule 19).
#[derive(Debug)]
pub(crate) enum MakeError {
    /// The binary was built without the `sandbox` feature: no proc macro can run.
    NoSandbox,
    /// No home: the module store is under it.
    NoHome,
    /// The module store could not be listed (its git folders).
    Store(std::string::String),
    /// The Wasmtime runtime could not be made.
    Runtime(std::string::String),
    /// A macro crate whose module is in the store and could not be read.
    Module { krate: std::string::String, why: std::string::String },
    /// A crate of a unit whose package the names world was never told the place of: the
    /// macro could not be shown its package (proc_callers.rs).
    NotPlaced { krate: std::string::String },
    /// A crate the names world made no environment for (blaze-world makes one per crate): the
    /// macro could not be shown the crate's variables.
    NoEnv { krate: std::string::String },
    /// Two packages made for one crate of the world: a bug of `make`'s loop, named.
    GivenTwice { krate: std::string::String },
    /// The crate's environment named one variable both set and unknown: a bug of blaze-scope's
    /// `CrateEnv`, named.
    EnvList { krate: std::string::String, why: blaze_proc_macro::package_env::PackageEnvError },
    /// A member whose dir no sandbox can mount at `/workspace/<dir>` (D96).
    MemberDir { krate: std::string::String, why: std::string::String },
    /// The crate's environment gives a path variable another value than where its package's
    /// files are served (D96: one mount row; blaze-proc-macro `Package::new`).
    SeenMismatch { krate: std::string::String, why: blaze_proc_macro::sandbox::SeenMismatch },
}

impl core::fmt::Display for MakeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MakeError::NoSandbox => f.write_str("this blaze was built without the proc-macro sandbox (feature `sandbox`), so no proc macro can run"),
            MakeError::NoHome => f.write_str("no home: the proc-macro module store is under it, and HOME is not set"),
            MakeError::Store(why) => write!(f, "the proc-macro module store: {why}"),
            MakeError::Runtime(why) => write!(f, "the proc-macro runtime (Wasmtime): {why}"),
            MakeError::Module { krate, why } => write!(f, "the proc-macro module of {krate} could not be read: {why}"),
            MakeError::NotPlaced { krate } => write!(f, "{krate}: the names world never placed its package, so a proc macro called in it cannot be shown its files"),
            MakeError::NoEnv { krate } => write!(f, "{krate}: the names world made no environment for this crate, so a proc macro called in it cannot be shown its variables"),
            MakeError::GivenTwice { krate } => write!(f, "{krate}: the proc-macro runner was given two packages for this one crate"),
            MakeError::EnvList { krate, why } => write!(f, "{krate}: the variables a proc macro called in it is shown: {why}"),
            MakeError::MemberDir { krate, why } => write!(f, "{krate}: {why}"),
            MakeError::SeenMismatch { krate, why } => write!(f, "{krate}: {why}"),
        }
    }
}

/// Where [`ProcRunner::keep`] writes: the store [`make`] opens under the home.
pub(crate) const ANSWERS_STORE: &str = "<home>/.blaze/v2/proc-macro/expansions";

/// The expansion records' folder beneath scripts.rs `STORE_UNDER_HOME` ([`make`] joins it; the
/// layout's row, layout.rs, is this joined, a test holds [`ANSWERS_STORE`] to it).
#[cfg_attr(not(any(test, feature = "sandbox")), expect(dead_code, reason = "only the sandboxed `make` opens the expansions; layout.rs's test reads it"))]
pub(crate) const EXPANSIONS: &str = "proc-macro/expansions";

/// The answers this command's proc-macro calls made could not be written to their store
/// ([`ProcRunner::keep`]): names the store and the record that refused (rule 19). The command
/// fails with it, never quietly leaving the next command to run the calls again (rule 3).
#[derive(Debug)]
pub(crate) struct KeepError {
    pub error: blaze_proc_macro::record::RecordError,
}

impl core::fmt::Display for KeepError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "the proc-macro answers of this command could not be written to their store ({ANSWERS_STORE}): {}", self.error)
    }
}

#[cfg(feature = "sandbox")]
pub(crate) use with_sandbox::{ProcRunner, make};
#[cfg(not(feature = "sandbox"))]
pub(crate) use without_sandbox::{ProcRunner, make};

#[cfg(feature = "sandbox")]
mod with_sandbox {
    use super::*;

    use std::alloc::Global;
    use std::sync::Arc;

    use blaze_proc_macro::hook::{ProcMacros, RunRefused, crate_key};
    use blaze_proc_macro::module::{LockedPackages, ModuleError, module_key, read_module};
    use blaze_scope::resolve::SourceTexts;
    use blaze_world::procs::files::{PackageAt, sandbox_files};
    use blaze_world::procs::{git_folders, identify};

    /// The runner: see the module's doc.
    pub(crate) struct ProcRunner<'r> {
        procs: ProcMacros<'r>,
        /// Each macro crate by its world index: its key. Sorted by index.
        keys: std::vec::Vec<(usize, CrateItemKey), Global>,
        /// Each macro crate's package name by its runner key, sorted by key (for live status).
        crate_names: blaze_own::Vec<(CrateItemKey, blaze_own::String<blaze_own::SharedAlloc<'static>>), blaze_own::SharedAlloc<'static>>,
        /// Each crate's package as the macro is shown it, by its index in the names world (proc_env.rs).
        callers: crate::proc_env::Callers,
    }

    impl<'r> ProcRunner<'r> {
        /// The runner key of the proc-macro crate at world index `krate`: what its items record
        /// holds and its definitions carry. `None`: no macro crate at that index (or one with
        /// no module identity, which the `MacroModule` task names).
        pub(crate) fn key_of(&self, krate: usize) -> Option<CrateItemKey> {
            let at = self.keys.binary_search_by_key(&krate, |(k, _)| *k).ok()?;
            Some(self.keys[at].1)
        }

        /// Package name of the proc-macro crate that defines `def` (for live status attribution).
        pub(crate) fn crate_name(&self, def: &MacroDefinition<'_>) -> Option<&str> {
            let blaze_expand::MacroKind::ProcMacro { krate, .. } = def.kind() else {
                return None;
            };
            let at = self.crate_names.binary_search_by_key(&krate, |(key, _)| *key).ok()?;
            self.crate_names.get(at).map(|(_, name)| name.as_str())
        }

        /// Runs the proc macro `def` on `call`, written in the crate at index `krate` of the
        /// names world, at `edition`: through `ProcMacros::run` alone, shown that crate's own
        /// package and variables (a lib's and a bin's of one package are two). A caller with no
        /// package here is refused (the macro is never shown another crate's). A run that
        /// stopped holds its error, naming what stopped it ([`RunRefused`]).
        pub(crate) fn run<'a>(
            &self,
            krate: usize,
            edition: Edition,
            def: &MacroDefinition<'_>,
            call: ProcCall<'_>,
            alloc: Alloc<'a>,
        ) -> Result<Produced<'a>, RunRefused> {
            let Some(package) = self.callers.of(krate) else {
                return Err(RunRefused { host: HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused }, call: None });
            };
            self.procs.run_why(def, &call, edition, package, alloc)
        }

        /// Why `def` cannot run, called in the crate at index `krate`, in words naming the
        /// macro's crate (blaze-proc-macro `ProcMacros::refusal_of`): a caller with no package
        /// here, or the module the store holds or lacks under the key asked. `None`: nothing
        /// about the module stops it.
        pub(crate) fn unusable(&self, krate: usize, def: &MacroDefinition<'_>) -> Option<std::string::String> {
            if self.callers.of(krate).is_none() {
                return Some(std::format!("the calling crate (index {krate} of the names world) was given no package, so no proc macro runs for it (proc_env.rs)"));
            }
            self.procs.refusal_of(def).map(|finding| finding.to_string())
        }

        /// The answers this command's calls ran, written to the answers store
        /// ([`super::ANSWERS_STORE`]) once every stage that expands (names, heads, bodies) is
        /// done: each crate whose calls ran something new has its results table written again,
        /// whole (blaze-proc-macro `ProcMacros::keep_all`), so the next command finds each call
        /// by its key and reads its answer instead of running the macro (rule 77). A crate with
        /// nothing new is not written. Before lane M14 nothing called it: every command expanded
        /// every proc macro again.
        pub(crate) fn keep(&self) -> Result<blaze_proc_macro::hook::KeptAll, super::KeepError> {
            self.procs.keep_all().map_err(|error| super::KeepError { error })
        }

        /// The command's macro-module loads so far: the profile's `macro modules` line.
        pub(crate) fn module_loads(&self) -> blaze_proc_macro::host::ModuleLoads {
            self.procs.module_loads()
        }
    }

    /// The runner for `names`' world: each proc-macro crate of a unit registered by its module
    /// identity, its module read from `<home>/.blaze/v2` (a missing one kept for its calls),
    /// expansion records kept under `<home>/.blaze/v2/proc-macro/expansions`, and each crate of
    /// the checkout (tree 0) given its folder as its package. Leaked: it lives to the end of
    /// the command, as the names world does.
    pub(crate) fn make(names: &crate::names::NamesWorld<'_>, side: &MacroSide<'_, '_, '_>) -> Result<&'static ProcRunner<'static>, MakeError> {
        let home = side.place.home.ok_or(MakeError::NoHome)?;
        let modules = std::path::Path::new(home).join(".blaze/v2");
        let locked = LockedPackages::of(side.lock.lock);
        let folders = git_folders(&modules).map_err(|e| MakeError::Store(e.to_string()))?;
        // The process's heap, handed from `main` through the side (D102, D120).
        let runtime = blaze_proc_macro::host::Runtime::new(side.heap).map_err(|e| MakeError::Runtime(e.to_string()))?;
        let runtime: &'static blaze_proc_macro::host::Runtime = std::boxed::Box::leak(std::boxed::Box::new_in(runtime, Global));
        let records = blaze_proc_macro::record::DirStore::new(&modules.join(super::EXPANSIONS));
        let records: &'static blaze_proc_macro::record::DirStore = std::boxed::Box::leak(std::boxed::Box::new_in(records, Global));
        let mut procs = ProcMacros::new(runtime, records, blaze_proc_macro::host::Limits::STANDARD);
        let world = names.world();
        let graph = names.graph();
        let mut keys = std::vec::Vec::new_in(Global);
        let mut crate_names = blaze_own::Vec::new_in(side.heap.shared());
        let mut callers = crate::proc_env::Callers::new();
        let placed = names.placed();
        for (at, krate) in world.crates.iter().enumerate() {
            // The package the macro is shown: where the world placed the crate's package, by the
            // one path that read it (proc_callers.rs): a checkout folder, a fetched package's
            // record read in place, a toolchain crate's nothing (blaze-world procs/files.rs).
            // A member is its workspace at `/workspace` with the member at its dir (D96, as build
            // scripts get it); a fetched package its record at `/package`.
            let files = match krate.origin {
                blaze_world::Origin::Toolchain(_) => sandbox_files(PackageAt::Toolchain),
                blaze_world::Origin::Unit(unit) => match placed.of(graph.units[unit.0 as usize].package) {
                    Some(place) => crate::proc_callers::shown(place, side.place.checkout)
                        .map_err(|e| MakeError::MemberDir { krate: std::format!("{} {}", krate.package, krate.version), why: e.to_string() })?,
                    None => return Err(MakeError::NotPlaced { krate: std::format!("{} {}", krate.package, krate.version) }),
                },
            };
            // The crate's OWN environment, the one `env!` reads (blaze-world made it per unit):
            // a lib and a bin of one package are two callers (proc_env.rs, rule 21).
            let named = || std::format!("{} {} (crate {})", krate.package, krate.version, krate.input.name);
            let env = world.texts.crate_env(at).ok_or_else(|| MakeError::NoEnv { krate: named() })?;
            let package = crate::proc_env::unit_package(env, files).map_err(|error| match error {
                crate::proc_env::UnitPackageError::Env(why) => MakeError::EnvList { krate: named(), why },
                crate::proc_env::UnitPackageError::Seen(why) => MakeError::SeenMismatch { krate: named(), why },
            })?;
            callers.put(at, Arc::new(package)).map_err(|_| MakeError::GivenTwice { krate: named() })?;
            let blaze_world::Origin::Unit(unit) = krate.origin else { continue };
            if !krate.proc_macro {
                continue;
            }
            let unit = &graph.units[unit.0 as usize];
            let identity = identify(side.lock.lock, &locked, &folders, unit, |id| Ok(side.place.member_files(id))).map_err(|e| MakeError::Store(e.to_string()))?;
            // No identity: the `MacroModule` task's failure names the crate; it gets no key.
            let Ok(identity) = identity else { continue };
            let (key, module) = identity.with_id(|id| (crate_key(&module_key(id)), read_module(&modules, id)));
            if let Err(error) = &module
                && !matches!(error, ModuleError::Missing { .. })
            {
                return Err(MakeError::Module { krate: std::format!("{} {}", identity.package, identity.version), why: error.to_string() });
            }
            procs.add_crate(key, &identity.package, &identity.version, module);
            keys.push((at, key));
            crate_names.push((key, blaze_own::String::from_str_in(identity.package.as_str(), side.heap.shared())));
        }
        crate_names.sort_unstable_by_key(|(key, _)| *key);
        crate_names.dedup_by_key(|(key, _)| *key);
        let runner = ProcRunner { procs, keys, crate_names, callers };
        Ok(std::boxed::Box::leak(std::boxed::Box::new_in(runner, Global)))
    }

}

#[cfg(not(feature = "sandbox"))]
mod without_sandbox {
    use super::*;

    /// No sandbox in this binary: never made ([`make`] refuses), so it has no value.
    pub(crate) enum ProcRunner<'r> {
        #[expect(dead_code, reason = "uninhabited stand-in: the lifetime of the real runner")]
        Never(core::convert::Infallible, core::marker::PhantomData<&'r ()>),
    }

    impl ProcRunner<'_> {
        pub(crate) fn key_of(&self, _krate: usize) -> Option<CrateItemKey> {
            match self {
                ProcRunner::Never(never, _) => match *never {},
            }
        }

        pub(crate) fn crate_name(&self, _def: &MacroDefinition<'_>) -> Option<&str> {
            match self {
                ProcRunner::Never(never, _) => match *never {},
            }
        }

        pub(crate) fn run<'a>(&self, _: usize, _: Edition, _: &MacroDefinition<'_>, _: ProcCall<'_>, _: Alloc<'a>) -> Result<Produced<'a>, blaze_proc_macro::hook::RunRefused> {
            match self {
                ProcRunner::Never(never, _) => match *never {},
            }
        }

        pub(crate) fn unusable(&self, _: usize, _: &MacroDefinition<'_>) -> Option<std::string::String> {
            match self {
                ProcRunner::Never(never, _) => match *never {},
            }
        }

        pub(crate) fn keep(&self) -> Result<blaze_proc_macro::hook::KeptAll, super::KeepError> {
            match self {
                ProcRunner::Never(never, _) => match *never {},
            }
        }

        pub(crate) fn module_loads(&self) -> blaze_proc_macro::host::ModuleLoads {
            match self {
                ProcRunner::Never(never, _) => match *never {},
            }
        }
    }

    pub(crate) fn make(_: &crate::names::NamesWorld<'_>, _: &MacroSide<'_, '_, '_>) -> Result<&'static ProcRunner<'static>, MakeError> {
        Err(MakeError::NoSandbox)
    }
}
