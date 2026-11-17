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
//! Made once a command on the main thread, by the first `CrateScope` task after the names
//! world ([`make`], held in `Names`), and lent read-only to the pool's names and body jobs.
//! The calling crate's package (what the macro is shown) is a checkout crate's folder; a
//! crate of a fetched package (a tree other than the checkout's) has none here yet, and its
//! calls are refused (`HostWhy::Refused`) rather than shown another package.

use blaze_expand::{Edition, HostAsk, HostError, HostWhy, MacroDefinition, ProcCall, Produced};
use blaze_id::CrateItemKey;
use blaze_own::Alloc;

/// What the command gives [`make`]: the lock as read (its text is what the dependency closure
/// of a module identity is read from) and the macro crates' place (the home holding the module
/// store, the checkout, the members' folders and their `MacroFiles` rows).
pub(crate) struct MacroSide<'s, 'p, 'r> {
    pub lock: &'s crate::fetch::Lockfile<'s>,
    pub place: crate::macros::Place<'p, 'r>,
}

/// Why the runner could not be made: names what and where (rule 19).
#[derive(Debug)]
pub(crate) enum MakeError {
    /// The binary was built without the `sandbox` feature: no proc macro can run.
    NoSandbox,
    /// No home: the module store is under it.
    NoHome,
    /// The lock's text is not UTF-8.
    LockText,
    /// The module store could not be listed (its git folders).
    Store(std::string::String),
    /// The Wasmtime runtime could not be made.
    Runtime(std::string::String),
    /// A macro crate whose module is in the store and could not be read.
    Module { krate: std::string::String, why: std::string::String },
}

impl core::fmt::Display for MakeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            MakeError::NoSandbox => f.write_str("this blaze was built without the proc-macro sandbox (feature `sandbox`), so no proc macro can run"),
            MakeError::NoHome => f.write_str("no home: the proc-macro module store is under it, and HOME is not set"),
            MakeError::LockText => f.write_str("Cargo.lock is not UTF-8"),
            MakeError::Store(why) => write!(f, "the proc-macro module store: {why}"),
            MakeError::Runtime(why) => write!(f, "the proc-macro runtime (Wasmtime): {why}"),
            MakeError::Module { krate, why } => write!(f, "the proc-macro module of {krate} could not be read: {why}"),
        }
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
    use std::path::PathBuf;
    use std::sync::Arc;

    use blaze_proc_macro::hook::{ProcMacros, crate_key};
    use blaze_proc_macro::module::{LockedPackages, ModuleError, module_key, read_module};
    use blaze_proc_macro::sandbox::Package;
    use blaze_world::procs::files::{PackageAt, sandbox_files};
    use blaze_world::procs::{git_folders, identify_files};

    /// The runner: see the module's doc.
    pub(crate) struct ProcRunner<'r> {
        procs: ProcMacros<'r>,
        /// Each macro crate by its world index: its key. Sorted by index.
        keys: std::vec::Vec<(usize, CrateItemKey), Global>,
        /// Each checkout crate's package as the macro is shown it, by `shown_from`, sorted.
        callers: std::vec::Vec<(std::string::String, Arc<Package>), Global>,
    }

    impl<'r> ProcRunner<'r> {
        /// The runner key of the proc-macro crate at world index `krate`: what its items record
        /// holds and its definitions carry. `None`: no macro crate at that index (or one with
        /// no module identity, which the `MacroModule` task names).
        pub(crate) fn key_of(&self, krate: usize) -> Option<CrateItemKey> {
            let at = self.keys.binary_search_by_key(&krate, |(k, _)| *k).ok()?;
            Some(self.keys[at].1)
        }

        /// Runs the proc macro `def` on `call`, written in the crate whose package the world
        /// placed at `shown_from`, at `edition`: through `ProcMacros::run` alone. A caller with
        /// no package here is refused (the macro is never shown another package).
        pub(crate) fn run<'a>(
            &self,
            shown_from: &str,
            edition: Edition,
            def: &MacroDefinition<'_>,
            call: ProcCall<'_>,
            alloc: Alloc<'a>,
        ) -> Result<Produced<'a>, HostError> {
            let Ok(at) = self.callers.binary_search_by(|(from, _)| from.as_str().cmp(shown_from)) else {
                return Err(HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused });
            };
            self.procs.run(def, &call, edition, &self.callers[at].1, alloc)
        }
    }

    /// The runner for `names`' world: each proc-macro crate of a unit registered by its module
    /// identity, its module read from `<home>/.blaze/v2` (a missing one kept for its calls),
    /// expansion records kept under `<home>/.blaze/v2/proc-macro/expansions`, and each crate of
    /// the checkout (tree 0) given its folder as its package. Leaked: it lives to the end of
    /// the command, as the names world does.
    pub(crate) fn make(names: &crate::names::Names<'_>, side: &MacroSide<'_, '_, '_>) -> Result<&'static ProcRunner<'static>, MakeError> {
        let home = side.place.home.ok_or(MakeError::NoHome)?;
        let modules = std::path::Path::new(home).join(".blaze/v2");
        let text = core::str::from_utf8(side.lock.text).map_err(|_| MakeError::LockText)?;
        let locked = LockedPackages::read(text);
        let folders = git_folders(&modules).map_err(|e| MakeError::Store(e.to_string()))?;
        // D102: blaze-cli's entry does not hand its heap down yet (program.rs says the same);
        // the runtime takes the process heap, named here at the edge.
        let runtime = blaze_proc_macro::host::Runtime::new(blaze_own::Heap::process()).map_err(|e| MakeError::Runtime(e.to_string()))?;
        let runtime: &'static blaze_proc_macro::host::Runtime = std::boxed::Box::leak(std::boxed::Box::new_in(runtime, Global));
        let records = blaze_proc_macro::record::DirStore::new(&modules.join("proc-macro/expansions"));
        let records: &'static blaze_proc_macro::record::DirStore = std::boxed::Box::leak(std::boxed::Box::new_in(records, Global));
        let mut procs = ProcMacros::new(runtime, records, blaze_proc_macro::host::Limits::STANDARD);
        let world = names.world();
        let graph = names.graph();
        let mut keys = std::vec::Vec::new_in(Global);
        let mut callers = std::vec::Vec::new_in(Global);
        for (at, krate) in world.crates.iter().enumerate() {
            if let Some(inner) = krate.input.shown_from.strip_prefix("/0/").or_else(|| (krate.input.shown_from == "/0").then_some("")) {
                let dir: PathBuf = std::path::Path::new(side.place.checkout).join(inner);
                let files = sandbox_files(PackageAt::Disk(&dir));
                let env = package_env(krate.package, krate.version, krate.input.name);
                callers.push((std::string::String::from(krate.input.shown_from), Arc::new(Package { files, env })));
            }
            let blaze_world::Origin::Unit(unit) = krate.origin else { continue };
            if !krate.proc_macro {
                continue;
            }
            let unit = &graph.units[unit.0 as usize];
            let identity = identify_files(side.lock.lock, &locked, &folders, unit, |id| side.place.member_files(id)).map_err(|e| MakeError::Store(e.to_string()))?;
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
        }
        callers.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let runner = ProcRunner { procs, keys, callers };
        Ok(std::boxed::Box::leak(std::boxed::Box::new_in(runner, Global)))
    }

    /// The `CARGO_*` variables a proc macro may read of the package it is expanded in, paths
    /// under the sandbox's package folder (cargo's "Environment variables Cargo sets for
    /// crates"); the version parts as cargo splits a semver version.
    fn package_env(package: &str, version: &str, crate_name: &str) -> std::vec::Vec<(std::string::String, std::string::String)> {
        let (core, pre) = version.split_once('-').unwrap_or((version, ""));
        let core = core.split_once('+').map_or(core, |(c, _)| c);
        let mut parts = core.splitn(3, '.');
        let (major, minor, patch) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        let pre = pre.split_once('+').map_or(pre, |(p, _)| p);
        let pairs: [(&str, std::string::String); 9] = [
            ("CARGO_CRATE_NAME", crate_name.replace('-', "_")),
            ("CARGO_MANIFEST_DIR", std::string::String::from(blaze_proc_macro::sandbox::PACKAGE)),
            ("CARGO_PKG_NAME", std::string::String::from(package)),
            ("CARGO_PKG_VERSION", std::string::String::from(version)),
            ("CARGO_PKG_VERSION_MAJOR", std::string::String::from(major)),
            ("CARGO_PKG_VERSION_MINOR", std::string::String::from(minor)),
            ("CARGO_PKG_VERSION_PATCH", std::string::String::from(patch)),
            ("CARGO_PKG_VERSION_PRE", std::string::String::from(pre)),
            ("CARGO_MANIFEST_PATH", std::format!("{}/Cargo.toml", blaze_proc_macro::sandbox::PACKAGE)),
        ];
        pairs.into_iter().map(|(k, v)| (std::string::String::from(k), v)).collect()
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

        pub(crate) fn run<'a>(&self, _: &str, _: Edition, _: &MacroDefinition<'_>, _: ProcCall<'_>, _: Alloc<'a>) -> Result<Produced<'a>, HostError> {
            match self {
                ProcRunner::Never(never, _) => match *never {},
            }
        }
    }

    pub(crate) fn make(_: &crate::names::Names<'_>, _: &MacroSide<'_, '_, '_>) -> Result<&'static ProcRunner<'static>, MakeError> {
        Err(MakeError::NoSandbox)
    }
}
