//! What a proc macro called in one crate is shown of that crate's environment, and which crate
//! a call is routed to: per UNIT (one crate target: a package's lib, each bin, each test), never
//! per package (piece macro-env-per-unit, 2026-11-16; rules 21, 30, 54).
//!
//! The bug this removes: the runner (proc_runner.rs, pieces pm-call-expand and -deps) made each
//! caller's variables with its own builder (`package_env`) and kept the callers BY THE FOLDER
//! the world placed their package at (`CrateInput::shown_from`). A package's lib and its bins
//! are placed at one folder (blaze-world list.rs: `shown_from` is the package's base), so the
//! two callers had one key in a sorted list and `binary_search` gave either for both: a macro
//! called in the bin could read the lib's `CARGO_CRATE_NAME` (and lack `CARGO_BIN_NAME`), or the
//! reverse. The class (rule 54): a per-unit fact looked up by a per-package key. The fix: the
//! callers are kept by the crate's index in the names world ([`Callers`]), the index every
//! `SourceTexts` call already names a crate by, and the variables come from the ONE home of a
//! crate's environment, blaze-scope's `CrateEnv` (made per unit by blaze-world, the same value
//! `env!` and `option_env!` read in the crate's items and bodies, rule 30), never a second list.
//!
//! The expansion's key (rule 21) needs nothing new: the hook (blaze-proc-macro hook.rs `run`)
//! keys a call by `base_key(wasm digest, export, edition, input tokens)` and keeps each answer
//! with the READ LIST of its run (sandbox.rs `getenv` records `env:NAME` with the value's digest,
//! or absent); a kept answer is taken for a caller only when each read is answered the same in
//! the caller's package now (table.rs `find`/`list_holds` over `sandbox::answer_now`). So the
//! key covers exactly the variables the macro READ: a macro that reads `CARGO_CRATE_NAME` gets one
//! row for the lib and one for the bin; a macro that reads nothing, or only `CARGO_PKG_*`,
//! shares one row between them (work once, rule 77). What was wrong was the value it was
//! handed, which this file makes the unit's.
//!
//! `CARGO_MANIFEST_DIR` and `CARGO_MANIFEST_PATH` name the folder the sandbox SERVES the
//! package at, its mount row (`Files::seen`, D96): a member is served as `Files::Member` at
//! `/workspace/<dir>`, the value its `CrateEnv` gives, so a macro that opens
//! `$CARGO_MANIFEST_DIR/abi/x.json` (`sol!`) finds the member's file. The crate's values pass
//! through unchanged; `Package::new` refuses one that differs from the row (never overwritten).

use std::alloc::Global;
use std::string::String;
use std::sync::Arc;
use std::vec::Vec;

use blaze_proc_macro::package_env::{PackageEnv, PackageEnvError};
use blaze_proc_macro::sandbox::{Files, Package, SeenMismatch};
use blaze_scope::crate_env::CrateEnv;

/// The variables a proc macro called in the crate whose environment is `env` may read: every
/// variable `env` holds (cargo's `CARGO_*` for crates, the crate's own `CARGO_CRATE_NAME` and
/// `CARGO_BIN_NAME`, `OUT_DIR` and the build script's `rustc-env` pairs), unchanged, sorted by
/// name as `CrateEnv::vars` gives them: the values `env!` reads in the crate. The path ones are
/// checked against where the files are served by `Package::new` in [`unit_package`].
pub(crate) fn unit_env(env: &CrateEnv<'_>) -> Vec<(String, String), Global> {
    let mut out = Vec::new_in(Global);
    for (name, value) in env.vars() {
        out.push((String::from(name), String::from(value)));
    }
    out
}

/// Why a calling crate's package could not be made ([`unit_package`]).
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum UnitPackageError {
    /// The crate's environment named one variable both set and unknown (`PackageEnv::new`).
    Env(PackageEnvError),
    /// A path variable of the crate's environment is not where its files are served (D96).
    Seen(SeenMismatch),
}

/// The package a proc macro called in the crate with environment `env` is shown: `files`, the
/// variables [`unit_env`] gives, and the names `env` holds no value for that cargo sets
/// (`CrateEnv::unknown`): the macro's `getenv` of one is refused by name,
/// as `env!` of it is, never answered unset (macro-env-unknown, rule 3). Every other name is
/// unset. An error names a bug of `env` (a name both set and unknown), or a path variable that
/// is not where `files` are served (D96).
pub(crate) fn unit_package(env: &CrateEnv<'_>, files: Files) -> Result<Package, UnitPackageError> {
    let vars = unit_env(env);
    // `PackageEnv` takes blaze-proc-macro's prelude `Vec`: moved over, element by element.
    let mut set = std::vec::Vec::with_capacity(vars.len());
    set.extend(vars);
    let unknown = env.unknown().map(String::from).collect();
    // No name of a crate's environment is unknown by a prefix: each `CARGO_BIN_EXE_<name>` of
    // a test or bench is set (blaze-scope crate_env.rs).
    let env = PackageEnv::new(set, unknown, Vec::new()).map_err(UnitPackageError::Env)?;
    // D96: the path variables equal where `files` are served, or the package is refused.
    Package::new(files, env).map_err(UnitPackageError::Seen)
}

/// Each calling crate's package, by the crate's index in the names world: the key a call is
/// routed by. One entry per crate; a lib and a bin of one package are two.
pub(crate) struct Callers {
    /// `(world index, package)`, sorted by index, each index once.
    by_crate: Vec<(usize, Arc<Package>), Global>,
}

/// Two packages given for one crate: the world lists each crate once, so this names a bug in
/// the caller's loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GivenTwice {
    pub krate: usize,
}

impl Callers {
    pub(crate) fn new() -> Callers {
        Callers { by_crate: Vec::new_in(Global) }
    }

    /// The package of the crate at world index `krate`. Kept sorted; a second package for one
    /// index is refused.
    pub(crate) fn put(&mut self, krate: usize, package: Arc<Package>) -> Result<(), GivenTwice> {
        let at = self.by_crate.partition_point(|(k, _)| *k < krate);
        if self.by_crate.get(at).is_some_and(|(k, _)| *k == krate) {
            return Err(GivenTwice { krate });
        }
        self.by_crate.insert(at, (krate, package));
        Ok(())
    }

    /// The package a macro called in the crate at world index `krate` is shown; `None`: that
    /// crate was given none.
    pub(crate) fn of(&self, krate: usize) -> Option<&Arc<Package>> {
        let at = self.by_crate.partition_point(|(k, _)| *k < krate);
        self.by_crate.get(at).filter(|(k, _)| *k == krate).map(|(_, package)| package)
    }
}

#[cfg(test)]
mod tests {
    //! A package `app` 0.1.0 with a lib (crate `app`) and a bin `app-cli` (crate `app_cli`), as
    //! cargo makes them (the cargo book, "Environment variables Cargo sets for crates":
    //! `CARGO_CRATE_NAME` "The name of the crate that is currently being compiled. It is the name
    //! of the Cargo target with `-` converted to `_`"; `CARGO_BIN_NAME` "The name of the binary
    //! that is currently being compiled", set for binaries only). A fn-like macro that emits
    //! `env!("CARGO_CRATE_NAME")`'s value, read through the sandbox's `getenv` at expansion,
    //! returns what its package's `env` holds for the name (sandbox.rs `getenv`), and its kept
    //! answer is taken for another caller only when `answer_now` of `env:CARGO_CRATE_NAME` is
    //! equal there (table.rs `list_holds`). So: the lib's call must read `app`, the bin's
    //! `app_cli`, under different answers; a read of `CARGO_PKG_NAME` alone is shared.
    //!
    //! Control: a planted copy of the old routing (callers by `shown_from`, the package's one
    //! folder, `binary_search`) hands both crates ONE package, so both read the same name.
    //!
    //! Expected (`lanecheck.sh blaze-cli test proc_env`): "test blaze-cli: exit 0; 1 passed, 0
    //! failed", the test printing "lib reads CARGO_CRATE_NAME=app, bin reads
    //! CARGO_CRATE_NAME=app_cli; by folder both read the same".

    use std::alloc::Global;
    use std::string::String;
    use std::sync::Arc;
    use std::vec::Vec;

    use blaze_proc_macro::sandbox::{Files, Package, answer_now};
    use blaze_scope::crate_env::{CrateEnv, EnvFacts, SeenRoot};

    use super::{Callers, GivenTwice, unit_package};

    /// The lib's and the bin's world indices (any two; a world lists the toolchain first).
    const LIB: usize = 7;
    const BIN: usize = 8;
    /// The folder the world placed package `app` at: both crates' `shown_from`.
    const FOLDER: &str = "/0/app/";

    fn env_of(crate_name: &'static str, bin: Option<&'static str>) -> CrateEnv<'static> {
        let facts = EnvFacts {
            crate_name,
            package: Some(("app", "0.1.0")),
            bin,
            seen: SeenRoot::member("app").expect("a plain member dir"),
            read_folder: FOLDER,
            build_script: false,
            script: None,
            test_or_bench: false,
            bin_exes: blaze_scope::crate_env::BinNames::NONE,
            meta: None,
        };
        CrateEnv::of(facts, blaze_own::testing::heap().alloc()).unwrap_or_else(|e| panic!("the environment of {crate_name}: {e}"))
    }

    /// The member `app` as the runner serves it (D96 (3)): its workspace at `/workspace`, the
    /// member at `app` in it, the same dir its `CrateEnv` was made with.
    fn served() -> Files {
        let root = blaze_proc_macro::script_sandbox::ScriptRoot::member("app").unwrap_or_else(|e| panic!("{e}"));
        Files::Member { workspace: std::path::PathBuf::from("/nonexistent-ws"), root }
    }

    /// What the macro's `getenv` returns for `name` in `package` (sandbox.rs: the same lookup).
    fn read<'p>(package: &'p Package, name: &str) -> Option<&'p str> {
        match package.env.get(name) {
            blaze_proc_macro::package_env::EnvAnswer::Set(value) => Some(value),
            blaze_proc_macro::package_env::EnvAnswer::Unset => None,
            blaze_proc_macro::package_env::EnvAnswer::Unknown => panic!("{name} is unknown in this package"),
        }
    }

    /// The old routing (proc_runner.rs `run` before this piece): callers by folder, sorted,
    /// found by `binary_search`.
    fn by_folder<'c>(callers: &'c [(String, Arc<Package>)], shown_from: &str) -> &'c Arc<Package> {
        let at = callers.binary_search_by(|(from, _)| from.as_str().cmp(shown_from)).unwrap_or_else(|_| panic!("no caller at {shown_from}"));
        &callers[at].1
    }

    #[test]
    fn a_lib_and_a_bin_of_one_package_read_their_own_crate_name() {
        let lib = Arc::new(unit_package(&env_of("app", None), served()).unwrap_or_else(|e| panic!("{e:?}")));
        let bin = Arc::new(unit_package(&env_of("app_cli", Some("app-cli")), served()).unwrap_or_else(|e| panic!("{e:?}")));
        // D96: the member served as a plain listing at `/package` while its crate says
        // `/workspace/app` is refused by name, never overwritten.
        let at_package = unit_package(&env_of("app", None), Files::Listed(Arc::from(Vec::new())));
        let Err(super::UnitPackageError::Seen(at_package)) = at_package else { panic!("a member served at /package was taken: {at_package:?}") };
        assert_eq!((at_package.name.as_str(), at_package.given.as_str(), at_package.row.as_str()), ("CARGO_MANIFEST_DIR", "/workspace/app", "/package"));
        let mut callers = Callers::new();
        callers.put(LIB, Arc::clone(&lib)).expect("the lib, once");
        callers.put(BIN, Arc::clone(&bin)).expect("the bin, once");
        assert_eq!(callers.put(BIN, Arc::clone(&bin)), Err(GivenTwice { krate: BIN }), "a crate given two packages");

        let lib_seen = callers.of(LIB).expect("the lib's package");
        let bin_seen = callers.of(BIN).expect("the bin's package");
        assert_eq!(read(lib_seen, "CARGO_CRATE_NAME"), Some("app"), "the lib's call reads the lib's crate name");
        assert_eq!(read(bin_seen, "CARGO_CRATE_NAME"), Some("app_cli"), "the bin's call reads the bin's crate name");
        assert_eq!(read(lib_seen, "CARGO_BIN_NAME"), None, "a lib has no CARGO_BIN_NAME");
        assert_eq!(read(bin_seen, "CARGO_BIN_NAME"), Some("app-cli"), "the bin's call reads its binary name");
        // The manifest's folder is where the files are served: the member's row, the value
        // its `CrateEnv` (and `env!`) gives.
        assert_eq!(read(lib_seen, "CARGO_MANIFEST_DIR"), Some("/workspace/app"));
        assert_eq!(read(bin_seen, "CARGO_MANIFEST_PATH"), Some("/workspace/app/Cargo.toml"));
        assert!(callers.of(LIB + 100).is_none(), "a crate never given a package has one");

        // The key: a kept answer that read `CARGO_CRATE_NAME` holds for one crate only; one
        // that read `CARGO_PKG_NAME` alone holds for both (shared, worked once).
        let now = |p: &Package, subject: &str| answer_now(p, subject).unwrap_or_else(|e| panic!("{subject}: {e:?}"));
        assert_ne!(now(lib_seen, "env:CARGO_CRATE_NAME"), now(bin_seen, "env:CARGO_CRATE_NAME"), "lib and bin answer one read the same: one kept answer would serve both");
        assert_eq!(now(lib_seen, "env:CARGO_PKG_NAME"), now(bin_seen, "env:CARGO_PKG_NAME"), "a package-wide read differs between the package's crates");

        // The control: the old routing by folder gives the two crates one package.
        let mut old: Vec<(String, Arc<Package>), Global> = Vec::new_in(Global);
        old.push((String::from(FOLDER), Arc::clone(&lib)));
        old.push((String::from(FOLDER), Arc::clone(&bin)));
        old.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let (old_lib, old_bin) = (by_folder(&old, FOLDER), by_folder(&old, FOLDER));
        assert_eq!(read(old_lib, "CARGO_CRATE_NAME"), read(old_bin, "CARGO_CRATE_NAME"), "the planted old routing told the two crates apart");

        println!(
            "lib reads CARGO_CRATE_NAME={}, bin reads CARGO_CRATE_NAME={}; by folder both read the same",
            read(lib_seen, "CARGO_CRATE_NAME").unwrap_or(""),
            read(bin_seen, "CARGO_CRATE_NAME").unwrap_or("")
        );
    }
}
