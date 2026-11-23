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
    /// The lazy caller pack opener could not reserve room on the command heap.
    Trees(&'static str),
}

impl core::fmt::Display for MakeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MakeError::NoSandbox => f.write_str("this blaze was built without the proc-macro sandbox (feature `sandbox`), so no proc macro can run"),
            MakeError::NoHome => f.write_str("no home: the proc-macro module store is under it, and HOME is not set"),
            MakeError::Store(why) => write!(f, "the proc-macro module store: {why}"),
            MakeError::Runtime(why) => write!(f, "the proc-macro runtime (Wasmtime): {why}"),
            MakeError::Module { krate, why } => write!(f, "the proc-macro module of {krate} could not be read: {why}"),
            MakeError::Trees(why) => write!(f, "the proc-macro runner's lazy pack table: {why}"),
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
    use blaze_proc_macro::sandbox::Package;
    use blaze_world::crate_list::{CrateGraph, CrateList};
    use blaze_world::procs::files::{PackageAt, sandbox_files};
    use blaze_world::procs::{git_folders, identify};

    /// Typed error when constructing a calling crate's sandbox package lazily.
    #[derive(Clone, Copy)]
    struct CallerError(&'static str);

    /// Shared sandbox package handle for one calling crate.
    struct CallerPkg(Arc<Package>);

    /// Per-crate lazy sandbox package table: constructs each calling crate's `Package` (files +
    /// `CrateEnv`) on first proc-macro invocation in that crate, instead of eagerly building all
    /// 1,200+ crates up front in `proc_runner::make`.
    struct LazyCallers {
        slots: &'static [bstd::sync::PublishOnce<Result<CallerPkg, CallerError>>],
        list: CrateList<'static>,
        placed: &'static [(blaze_manifest::lock::LockId, blaze_world::Place<'static>)],
        package_of: &'static [Option<blaze_manifest::lock::LockId>],
        checkout: &'static str,
        heap: &'static blaze_own::Heap,
    }

    impl LazyCallers {
        fn new(
            list: CrateList<'static>,
            names: &crate::names::NamesWorld<'_>,
            checkout: &str,
            heap: &'static blaze_own::Heap,
        ) -> Result<LazyCallers, MakeError> {
            let g = heap.alloc();
            let mut slots: blaze_own::AVec<'static, bstd::sync::PublishOnce<Result<CallerPkg, CallerError>>> = blaze_own::AVec::new_in(g);
            if slots.try_reserve_exact(list.len()).is_err() {
                return Err(MakeError::Trees("the heap refused room for the proc-macro callers table"));
            }
            slots.extend((0..list.len()).map(|_| bstd::sync::PublishOnce::new()));
            let graph = names.graph();
            let world = names.world();
            let mut package_of: blaze_own::AVec<'static, Option<blaze_manifest::lock::LockId>> = blaze_own::AVec::new_in(g);
            if package_of.try_reserve_exact(world.crates.len()).is_err() {
                return Err(MakeError::Trees("the heap refused room for the proc-macro caller packages"));
            }
            package_of.extend(world.crates.iter().map(|krate| match krate.origin {
                blaze_world::Origin::Unit(unit) => graph.units.get(unit.index()).map(|u| u.package),
                blaze_world::Origin::Toolchain(_) => None,
            }));
            let checkout: &'static str = &*blaze_own::Box::leak_static(blaze_own::AString::from_str_in(checkout, g).into_boxed_str());
            Ok(LazyCallers { slots: slots.leak_static(), list, placed: names.placed(), package_of: package_of.leak_static(), checkout, heap })
        }

        fn keep_err(&self, words: &str) -> CallerError {
            CallerError(&*blaze_own::Box::leak_static(blaze_own::AString::from_str_in(words, self.heap.alloc()).into_boxed_str()))
        }

        fn of(&self, cx: &bstd::cx::Cx, packs: crate::job_texts::Packs<'_>, krate: usize) -> Result<&CallerPkg, CallerError> {
            let Some(slot) = self.slots.get(krate) else {
                let mut msg = blaze_own::AString::new_in(self.heap.alloc());
                let _ = core::fmt::Write::write_fmt(&mut msg, format_args!("the calling crate (index {krate} of the names world) was given no package, so no proc macro runs for it (proc_env.rs)"));
                return Err(CallerError(&*blaze_own::Box::leak_static(msg.into_boxed_str())));
            };
            let got = match slot.get() {
                Some(got) => got,
                None => {
                    let built = self.build(cx, packs, krate);
                    let _ = slot.set(built);
                    slot.get().expect("just set")
                }
            };
            got.as_ref().map_err(|err| *err)
        }

        fn build(&self, cx: &bstd::cx::Cx, packs: crate::job_texts::Packs<'_>, krate: usize) -> Result<CallerPkg, CallerError> {
            let pkg = self.list.package(krate);
            let kname = CrateGraph::name(&self.list, krate);
            let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other)
                .map_err(|_| self.keep_err("could not reserve scratch frame for proc-macro caller setup"))?;
            let fail = |args: core::fmt::Arguments<'_>| -> CallerError {
                let words = bstd::mem::format_in!(&frame, "{pkg} (crate {kname}): {args}");
                self.keep_err(words.as_str())
            };
            let files = if CrateGraph::toolchain(&self.list, krate) {
                sandbox_files(PackageAt::Toolchain)
            } else if let Some(place) = self.package_of.get(krate).copied().flatten().and_then(|pkg_id| crate::proc_callers::placed_of(self.placed, pkg_id)) {
                crate::proc_callers::shown(place, self.checkout).map_err(|e| fail(format_args!("{e}")))?
            } else if self.list.member(krate) {
                let dir = self.list.shown_from(krate).strip_prefix("/0/").unwrap_or("").trim_end_matches('/');
                crate::proc_callers::shown(blaze_world::Place::Checkout { dir }, self.checkout)
                    .map_err(|e| fail(format_args!("{e}")))?
            } else if let Some(rest) = self.list.shown_from(krate).strip_prefix("/0/") {
                let dir = rest.trim_end_matches('/');
                crate::proc_callers::shown(blaze_world::Place::Path { dir }, self.checkout)
                    .map_err(|e| fail(format_args!("{e}")))?
            } else {
                let shown_from = self.list.shown_from(krate);
                let rest = shown_from.strip_prefix('/').ok_or_else(|| fail(format_args!("invalid shown_from `{shown_from}`")))?;
                let (tree_str, dir_rest) = rest.split_once('/').ok_or_else(|| fail(format_args!("invalid shown_from `{shown_from}`")))?;
                let tree: usize = tree_str.parse().map_err(|_| fail(format_args!("invalid tree index in `{shown_from}`")))?;
                let dir = dir_rest.trim_end_matches('/');
                let view = match packs {
                    crate::job_texts::Packs::SyncLazy(lazy, fs) => lazy.tree(cx, fs, &frame, tree)
                        .ok_or_else(|| match lazy.refused() {
                            Some(why) => fail(format_args!("{why}")),
                            None => fail(format_args!("tree {tree} is not in the crate list")),
                        })?,
                    crate::job_texts::Packs::Shared(shared) => shared.tree(cx, &frame, tree)
                        .ok_or_else(|| match shared.refused() {
                            Some(why) => fail(format_args!("{why}")),
                            None => fail(format_args!("tree {tree} is not in the crate list")),
                        })?,
                    crate::job_texts::Packs::Open(_) | crate::job_texts::Packs::OnFirstRead(_) => {
                        return Err(fail(format_args!("the names world never placed its package, so a proc macro called in it cannot be shown its files")));
                    }
                };
                crate::proc_callers::shown(blaze_world::Place::Pack { view, dir }, self.checkout)
                    .map_err(|e| fail(format_args!("{e}")))?
            };
            let erased = blaze_own::Erased::new(&frame);
            let env = blaze_scope::crate_env::CrateEnv::of(self.list.env_facts(krate), erased.handle())
                .map_err(|e| fail(format_args!("{e}")))?;
            let package = crate::proc_env::unit_package(&env, files).map_err(|error| match error {
                crate::proc_env::UnitPackageError::Env(why) => fail(format_args!("the variables a proc macro called in it is shown: {why}")),
                crate::proc_env::UnitPackageError::Seen(why) => fail(format_args!("{why}")),
            })?;
            Ok(CallerPkg(Arc::new(package)))
        }
    }

    /// The runner: see the module's doc.
    pub(crate) struct ProcRunner<'r> {
        procs: ProcMacros<'r>,
        /// Each macro crate by its world index: its key. Sorted by index.
        keys: std::vec::Vec<(usize, CrateItemKey), Global>,
        /// Each macro crate's package name by its runner key, sorted by key (for live status).
        crate_names: blaze_own::Vec<(CrateItemKey, blaze_own::String<blaze_own::SharedAlloc<'static>>), blaze_own::SharedAlloc<'static>>,
        /// Each crate's package as the macro is shown it, constructed lazily by world index.
        callers: LazyCallers,
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
            cx: &bstd::cx::Cx,
            packs: crate::job_texts::Packs<'_>,
            krate: usize,
            edition: Edition,
            def: &MacroDefinition<'_>,
            call: ProcCall<'_>,
            alloc: Alloc<'a>,
        ) -> Result<Produced<'a>, RunRefused> {
            let Ok(package) = self.callers.of(cx, packs, krate) else {
                return Err(RunRefused { host: HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused }, call: None });
            };
            self.procs.run_why(def, &call, edition, &package.0, alloc)
        }

        /// Why `def` cannot run, called in the crate at index `krate`, in words naming the
        /// macro's crate (blaze-proc-macro `ProcMacros::refusal_of`): a caller with no package
        /// here, or the module the store holds or lacks under the key asked. `None`: nothing
        /// about the module stops it.
        pub(crate) fn unusable(&self, cx: &bstd::cx::Cx, packs: crate::job_texts::Packs<'_>, krate: usize, def: &MacroDefinition<'_>) -> Option<std::string::String> {
            if let Err(CallerError(why)) = self.callers.of(cx, packs, krate) {
                return Some(why.into());
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
    /// expansion records kept under `<home>/.blaze/v2/proc-macro/expansions`, and each calling
    /// crate's sandbox package built lazily on first proc-macro call in that crate.
    pub(crate) fn make(names: &crate::names::NamesWorld<'_>, list: CrateList<'static>, side: &MacroSide<'_, '_, '_>) -> Result<&'static ProcRunner<'static>, MakeError> {
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
        let callers = LazyCallers::new(list, names, side.place.checkout, side.heap)?;
        for (at, krate) in world.crates.iter().enumerate() {
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

        pub(crate) fn run<'a>(&self, _: &bstd::cx::Cx, _: crate::job_texts::Packs<'_>, _: usize, _: Edition, _: &MacroDefinition<'_>, _: ProcCall<'_>, _: Alloc<'a>) -> Result<Produced<'a>, blaze_proc_macro::hook::RunRefused> {
            match self {
                ProcRunner::Never(never, _) => match *never {},
            }
        }

        pub(crate) fn unusable(&self, _: &bstd::cx::Cx, _: crate::job_texts::Packs<'_>, _: usize, _: &MacroDefinition<'_>) -> Option<std::string::String> {
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

    pub(crate) fn make(_: &crate::names::NamesWorld<'_>, _: blaze_world::crate_list::CrateList<'static>, _: &MacroSide<'_, '_, '_>) -> Result<&'static ProcRunner<'static>, MakeError> {
        Err(MakeError::NoSandbox)
    }
}
