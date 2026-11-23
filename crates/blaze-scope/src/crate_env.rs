//! A crate's environment as it is compiled: what `env!` and `option_env!` read, ONE value per
//! unit, the same for its items and its bodies (census 13, brief 13; decided by the coordinator
//! 2026-11-16).
//!
//! The Reference ("Macros", built-in `env!`; core's `env!` doc in the pinned library,
//! `macros/mod.rs`): "Inspects an environment variable at compile time. ... If the environment
//! variable is not defined, then a compilation error will be emitted." `option_env!`: "If the
//! named environment variable is present at compile time, this will expand into an expression
//! of type `Option<&'static str>` whose value is `Some` of the value ...; if not, `None`."
//!
//! What a crate's environment holds (the cargo book, "Environment variables Cargo sets for
//! crates"), each from a fact of the build, never the machine:
//! - `CARGO_PKG_NAME`, `CARGO_PKG_VERSION` and its `_MAJOR`, `_MINOR`, `_PATCH`, `_PRE` parts:
//!   the package's name and version as the lock and the manifest give them (the unit graph's
//!   `Unit::name`, `Unit::version`);
//! - `CARGO_PKG_AUTHORS`, `_DESCRIPTION`, `_HOMEPAGE`, `_LICENSE`, `_LICENSE_FILE`, `_README`,
//!   `_REPOSITORY`, `_RUST_VERSION`: the package's `[package]` metadata as cargo normalizes it
//!   (blaze-manifest layout meta.rs, the one table: authors joined with `:`, an unstated key
//!   `""`, an inherited one the workspace root's), carried by the unit graph
//!   (`UnitGraph::meta`). `""` for an unstated key is KNOWING, not guessing: cargo sets every
//!   one of them for every crate, empty when the manifest says nothing;
//! - `CARGO_PRIMARY_PACKAGE`: `1` for a workspace member's crate, unset for a dependency's
//!   (every member is selected: blaze builds the workspace as `cargo --workspace` would);
//! - `CARGO_CRATE_NAME`: the crate's name (`-` already `_` in the unit graph's `crate_name`);
//! - `CARGO_BIN_NAME`: a binary's name;
//! - `CARGO_MANIFEST_DIR`, `CARGO_MANIFEST_PATH`: the root the crate SEES (decision D96,
//!   [`SeenRoot`]): `/package` for every dependency, `/workspace/<member dir>` for a member;
//!   never where the checkout is, nor the folder the host reads its files under (that is
//!   [`CrateEnv::read_folder`], the caller's path space: two values, two types, rule 76);
//! - `OUT_DIR` and every `cargo::rustc-env` pair: from the build script's joined outputs record
//!   (blaze-build-script's `OutputsRef`), a later pair of one name winning as cargo gives it;
//! - `CARGO_BIN_EXE_<name>` of an integration test or a bench, one per binary of its package
//!   (the cargo book: "The absolute path to a binary target's executable. This is only set when
//!   building an integration test or benchmark"): the GUEST path [`bin_exe`] gives,
//!   `/target/debug/bin/<package>/<name>`, a constant of the package and the binary, as
//!   `OUT_DIR` is `/target/debug/build/..` (D96: never where the checkout or the home is, so a
//!   key holds nothing of the machine, rule 25). The binaries are the package's `UnitKind::Bin`
//!   units on the target side, which are the ones cargo builds (blaze-unit-graph resolve.rs
//!   skips a target whose `required-features` are off), so `option_env!` of another name is
//!   `None` as under cargo. What is at the path is the RUN side's (docs/v2/plan/design/test.md,
//!   "End-to-end tests", piece 17): a launcher stub for that binary.
//!
//! `CARGO` is unset: there is no cargo binary ([`CARGO_ALWAYS`]'s doc).
//!
//! A name cargo always sets whose value this build does not know (every `CARGO_PKG_*` of a
//! toolchain crate, or of a package whose metadata the graph does not carry; `OUT_DIR` of a
//! unit whose script's record is not joined; `CARGO_TARGET_TMPDIR` of a test or bench) is
//! UNKNOWN: a read of it is
//! refused by name ([`EnvUnknown`]), never answered unset (rule 3: an unset answer would turn a
//! valid program into a false `env!` error, or an `option_env!` into a wrong `None`). Any other
//! name is unset: blaze's builds see no ambient environment (rule 75), so `env!` of it is the
//! Reference's compile error naming the variable (blaze-expand's `Diagnostic::EnvNotDefined`).
//!
//! The reads: the expander records every answer (`ReadKind::Env`, the name, the value) in the
//! expansion's read list (blaze-expand builtin.rs `env`), which the item's or body's read list
//! joins: a changed value reruns exactly its readers.

use core::fmt;

use blaze_build_script::OutputsRef;
/// The root a crate sees (D96): its home is blaze-expand's `seen`, re-exported for the crates
/// that make environments.
pub use blaze_expand::seen::{MemberDirError, SeenRoot};
use blaze_own::{AString, AVec, Alloc, BString, Bump};
use blaze_unit_graph::{PackageMetaVars, UnitGraph, UnitKind};

use crate::units::{Origin, UnitCrates};

/// Every name cargo sets for every crate it compiles (the cargo book's list for crates), sorted:
/// one of these a [`CrateEnv`] holds no value for is [`EnvUnknown`], never unset.
///
/// Not `CARGO` ("Path to the cargo binary performing the build"): UNSET by rule (coordinator,
/// 2026-11-17): there is no cargo binary (rule 1), and a sandboxed macro could not run one, so
/// no value is true and none is unknown. `proc-macro-crate` (jsonrpsee's `#[rpc]`, ..) then
/// skips `cargo locate-project` and reads the calling package's own manifest, as it does in a
/// vendored folder; before, its read of `CARGO` refused every such call.
///
/// Not `CARGO_PRIMARY_PACKAGE`: cargo sets it only for a selected package, so it is `1` for a
/// member's crate and unset for a dependency's, both known (the module's doc).
pub const CARGO_ALWAYS: &[&str] = &[
    "CARGO_CRATE_NAME",
    "CARGO_MANIFEST_DIR",
    "CARGO_MANIFEST_PATH",
    "CARGO_PKG_AUTHORS",
    "CARGO_PKG_DESCRIPTION",
    "CARGO_PKG_HOMEPAGE",
    "CARGO_PKG_LICENSE",
    "CARGO_PKG_LICENSE_FILE",
    "CARGO_PKG_NAME",
    "CARGO_PKG_README",
    "CARGO_PKG_REPOSITORY",
    "CARGO_PKG_RUST_VERSION",
    "CARGO_PKG_VERSION",
    "CARGO_PKG_VERSION_MAJOR",
    "CARGO_PKG_VERSION_MINOR",
    "CARGO_PKG_VERSION_PATCH",
    "CARGO_PKG_VERSION_PRE",
];

/// Where an integration test or a bench is told binary `name` of its package `package` is
/// (`CARGO_BIN_EXE_<name>`): the guest path `/target/debug/bin/<package>/<name>`. A package's
/// name holds no `/` (the cargo book, "The `name` field": alphanumerics, `-` and `_`), nor
/// does a binary's, so one (package, binary) is one path. The run side serves a launcher stub
/// there (docs/v2/plan/design/test.md piece 17); this is the one place the path is spelled.
pub fn bin_exe<'b>(package: &str, name: &str, alloc: Alloc<'b>) -> AString<'b> {
    let mut out = AString::with_capacity_in(BIN_EXE_ROOT.len() + package.len() + 1 + name.len(), alloc);
    out.push_str(BIN_EXE_ROOT);
    out.push_str(package);
    out.push_str("/");
    out.push_str(name);
    out
}

/// The guest folder under which [`bin_exe`] places each package's binaries.
pub const BIN_EXE_ROOT: &str = "/target/debug/bin/";

/// The names of a package's binaries, sorted, each once, joined by `\n` (empty: none): what an
/// integration test or a bench of it is given a `CARGO_BIN_EXE_<name>` for. Made only by
/// [`BinNames::new`], which checks the form, so every reader may split it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BinNames<'p>(&'p str);

impl<'p> BinNames<'p> {
    /// No binary.
    pub const NONE: BinNames<'static> = BinNames("");

    /// `joined` as binary names: `None` unless each is non-empty, holds no `\n` and is after
    /// the one before it (sorted, each once).
    pub fn new(joined: &'p str) -> Option<BinNames<'p>> {
        if joined.is_empty() {
            return Some(BinNames(joined));
        }
        let mut last: Option<&str> = None;
        for name in joined.split('\n') {
            if name.is_empty() || last.is_some_and(|before| before >= name) {
                return None;
            }
            last = Some(name);
        }
        Some(BinNames(joined))
    }

    /// The names, sorted.
    pub fn names(self) -> impl Iterator<Item = &'p str> {
        self.0.split('\n').filter(|name| !name.is_empty())
    }

    /// The names joined by `\n`, as [`BinNames::new`] took them.
    pub fn joined(self) -> &'p str {
        self.0
    }
}

/// The facts a crate's environment is made from ([`CrateEnv::of`]).
#[derive(Clone, Copy)]
pub struct EnvFacts<'p> {
    /// The crate's name, as `CARGO_CRATE_NAME` gives it.
    pub crate_name: &'p str,
    /// The package's name and version; `None` for a toolchain crate (no lock entry: its
    /// `CARGO_PKG_*` are [`EnvUnknown`]).
    pub package: Option<(&'p str, &'p str)>,
    /// A binary's name (`CARGO_BIN_NAME`).
    pub bin: Option<&'p str>,
    /// The root the crate sees (D96): `CARGO_MANIFEST_DIR`.
    pub seen: SeenRoot<'p>,
    /// The folder the host reads the crate's files under, in the caller's path space
    /// (`/<tree>/<dir>/`, `/package/N/`; a trailing `/` allowed): never shown to user code.
    pub read_folder: &'p str,
    /// The package has a build script.
    pub build_script: bool,
    /// Its outputs record, once joined (`UnitCrates::join_scripts`).
    pub script: Option<OutputsRef<'p>>,
    /// An integration test or a bench: `CARGO_TARGET_TMPDIR` is set for it, to a value this
    /// build does not know yet, and `CARGO_BIN_EXE_<name>` for each of `bin_exes`.
    pub test_or_bench: bool,
    /// The package's binaries (read only with `test_or_bench`): each is a
    /// `CARGO_BIN_EXE_<name>` at [`bin_exe`].
    pub bin_exes: BinNames<'p>,
    /// The package's `[package]` metadata (`CARGO_PKG_AUTHORS`, ..), when the graph carries it
    /// (`UnitGraph::meta_of`); `None`: those names are [`EnvUnknown`]. Read only with `package`.
    pub meta: Option<PackageMetaVars<'p>>,
}

/// A crate's environment: see the module's doc.
pub struct CrateEnv<'a> {
    /// Each variable with its value, sorted by name, each name once.
    vars: AVec<'a, (AString<'a>, AString<'a>)>,
    /// The names cargo sets for this crate whose values are not known, sorted.
    unknown: AVec<'a, &'static str>,
    /// An integration test or a bench ([`EnvFacts::test_or_bench`]).
    test_or_bench: bool,
    /// Its package's binaries, as [`BinNames`] joins them ([`EnvFacts::bin_exes`]).
    bin_exes: AString<'a>,
    /// The root the crate sees (D96).
    seen: SeenRoot<'a>,
    /// The package's name and version as the lock gives them (never a build script's
    /// `rustc-env` pair): what `file!()` names a dependency by. `None`: a toolchain crate.
    package: Option<(AString<'a>, AString<'a>)>,
    /// The folder its files are read under, with its trailing `/` (the caller's path space).
    read_folder: AString<'a>,
    script: Option<OutputsRef<'a>>,
    /// The facts it was made from that its variables do not give back on their own
    /// ([`CrateEnv::facts`]): the crate's name, a binary's name, whether it has a build script.
    crate_name: AString<'a>,
    bin: Option<AString<'a>>,
    build_script: bool,
    /// The metadata it was made from, in [`PackageMetaVars::NAMES`]' order (a script's
    /// `rustc-env` pair may shadow one in `vars`, so it is kept apart for [`CrateEnv::facts`]).
    meta: Option<[AString<'a>; 8]>,
}

/// A variable cargo sets for the crate whose value this build does not know: a read of it is
/// refused by name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EnvUnknown;

/// Why a crate's environment could not be made.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CrateEnvError<'a> {
    /// The version is not `MAJOR.MINOR.PATCH[-PRE][+BUILD]` (the cargo book, "The `version`
    /// field": a SemVer version), so its parts cannot be given.
    Version { krate: &'a str, version: &'a str },
    /// The crate at `at` of the list is not a unit of the graph given (its origin names a unit
    /// past the graph's end, or one of another crate name).
    NotItsUnit { krate: &'a str, at: usize },
    /// No crate at `at`.
    NoCrate { at: usize },
    /// A binary of the package of test or bench `krate` has a name no `CARGO_BIN_EXE_<name>`
    /// can be made of (empty, or holding a line break).
    BinName { krate: &'a str, name: &'a str },
}

impl fmt::Display for CrateEnvError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CrateEnvError::Version { krate, version } => write!(
                f,
                "crate `{krate}`: version `{version}` is not MAJOR.MINOR.PATCH (the cargo book, \"The `version` field\"): CARGO_PKG_VERSION's parts cannot be given"
            ),
            CrateEnvError::NotItsUnit { krate, at } => write!(f, "crate `{krate}` (at {at}): not a unit of the graph given"),
            CrateEnvError::NoCrate { at } => write!(f, "no crate at {at} of the list"),
            CrateEnvError::BinName { krate, name } => write!(f, "crate `{krate}`: its package's binary {name:?} has no name a `CARGO_BIN_EXE_<name>` can be made of (crate_env.rs `package_bins`)"),
        }
    }
}

impl core::error::Error for CrateEnvError<'_> {}

/// `MAJOR`, `MINOR`, `PATCH` and `PRE` of a SemVer version (build metadata after `+` dropped);
/// `None` when it has not three numeric parts.
fn version_parts(version: &str) -> Option<[&str; 4]> {
    let version = version.split_once('+').map_or(version, |(v, _)| v);
    let (core, pre) = version.split_once('-').unwrap_or((version, ""));
    let mut parts = core.split('.');
    let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let (major, minor, patch) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || !numeric(major) || !numeric(minor) || !numeric(patch) {
        return None;
    }
    Some([major, minor, patch, pre])
}

impl<'a> CrateEnv<'a> {
    /// The environment of a crate with these facts.
    pub fn of(facts: EnvFacts<'a>, alloc: Alloc<'a>) -> Result<CrateEnv<'a>, CrateEnvError<'a>> {
        // (name, value, the order it was given in): a later pair of one name wins.
        let mut given: AVec<'a, (AString<'a>, AString<'a>, u32)> = AVec::new_in(alloc);
        let put = |given: &mut AVec<'a, (AString<'a>, AString<'a>, u32)>, name: &str, value: &str| {
            let order = given.len() as u32;
            given.push((AString::from_str_in(name, alloc), AString::from_str_in(value, alloc), order));
        };
        let mut read_folder = AString::from_str_in(facts.read_folder.trim_end_matches('/'), alloc);
        read_folder.push_str("/");
        let mut dir = AString::new_in(alloc);
        // An arena string takes any length: writing cannot fail.
        let _ = fmt::write(&mut Into(&mut dir), format_args!("{}", facts.seen));
        let mut manifest = AString::from_str_in(dir.as_str(), alloc);
        manifest.push_str("/Cargo.toml");
        put(&mut given, "CARGO_CRATE_NAME", facts.crate_name);
        put(&mut given, "CARGO_MANIFEST_DIR", dir.as_str());
        put(&mut given, "CARGO_MANIFEST_PATH", manifest.as_str());
        if let Some((name, version)) = facts.package {
            let [major, minor, patch, pre] = version_parts(version).ok_or(CrateEnvError::Version { krate: facts.crate_name, version })?;
            put(&mut given, "CARGO_PKG_NAME", name);
            put(&mut given, "CARGO_PKG_VERSION", version);
            put(&mut given, "CARGO_PKG_VERSION_MAJOR", major);
            put(&mut given, "CARGO_PKG_VERSION_MINOR", minor);
            put(&mut given, "CARGO_PKG_VERSION_PATCH", patch);
            put(&mut given, "CARGO_PKG_VERSION_PRE", pre);
            if let Some(meta) = facts.meta {
                for (name, value) in meta.pairs() {
                    put(&mut given, name, value);
                }
            }
        }
        if let SeenRoot::Member { .. } = facts.seen {
            put(&mut given, "CARGO_PRIMARY_PACKAGE", "1");
        }
        if let Some(bin) = facts.bin {
            put(&mut given, "CARGO_BIN_NAME", bin);
        }
        // A toolchain crate (no package) has no test or bench of a package to be given these.
        if let (true, Some((package, _))) = (facts.test_or_bench, facts.package) {
            for name in facts.bin_exes.names() {
                let mut var = AString::with_capacity_in("CARGO_BIN_EXE_".len() + name.len(), alloc);
                var.push_str("CARGO_BIN_EXE_");
                var.push_str(name);
                put(&mut given, var.as_str(), bin_exe(package, name, alloc).as_str());
            }
        }
        if let Some(script) = facts.script {
            put(&mut given, "OUT_DIR", script.out_dir());
            for (name, value) in script.env() {
                put(&mut given, name, value);
            }
        }
        given.sort_unstable_by(|a, b| a.0.as_str().cmp(b.0.as_str()).then(a.2.cmp(&b.2)));
        let mut vars: AVec<'a, (AString<'a>, AString<'a>)> = AVec::with_capacity_in(given.len(), alloc);
        for (name, value, _) in given {
            match vars.last_mut() {
                Some(last) if last.0.as_str() == name.as_str() => last.1 = value,
                _ => vars.push((name, value)),
            }
        }
        let has = |vars: &AVec<'a, (AString<'a>, AString<'a>)>, name: &str| vars.binary_search_by(|v| v.0.as_str().cmp(name)).is_ok();
        let mut unknown: AVec<'a, &'static str> = AVec::new_in(alloc);
        for &name in CARGO_ALWAYS {
            if !has(&vars, name) {
                unknown.push(name);
            }
        }
        if facts.build_script && facts.script.is_none() {
            unknown.push("OUT_DIR");
        }
        if facts.test_or_bench && !has(&vars, "CARGO_TARGET_TMPDIR") {
            unknown.push("CARGO_TARGET_TMPDIR");
        }
        unknown.sort_unstable();
        unknown.dedup();
        let package = facts.package.map(|(name, version)| (AString::from_str_in(name, alloc), AString::from_str_in(version, alloc)));
        let meta = facts.package.and(facts.meta).map(|meta| meta.values().map(|value| AString::from_str_in(value, alloc)));
        Ok(CrateEnv {
            vars,
            unknown,
            test_or_bench: facts.test_or_bench,
            bin_exes: AString::from_str_in(facts.bin_exes.joined(), alloc),
            seen: facts.seen,
            package,
            read_folder,
            script: facts.script,
            crate_name: AString::from_str_in(facts.crate_name, alloc),
            bin: facts.bin.map(|bin| AString::from_str_in(bin, alloc)),
            build_script: facts.build_script,
            meta,
        })
    }

    /// The kept metadata as the table's borrowed values.
    fn meta_vars(&self) -> Option<PackageMetaVars<'_>> {
        self.meta.as_ref().map(|values| PackageMetaVars::from_values(values.each_ref().map(|value| value.as_str())))
    }

    /// The kept metadata copied into `alloc`.
    fn meta_in<'b>(&self, alloc: Alloc<'b>) -> Option<[AString<'b>; 8]> {
        self.meta.as_ref().map(|values| values.each_ref().map(|value| AString::from_str_in(value.as_str(), alloc)))
    }

    /// The facts this environment was made from: [`CrateEnv::of`] of them makes it again, equal
    /// in every answer. What a reader with no world (a body job, blaze-cli body_types.rs) is
    /// handed through the crate list (blaze-world crate_list.rs), so a body's `env!` is
    /// answered by the ONE construction an item's is (rule 30), never a second table.
    pub fn facts(&self) -> EnvFacts<'_> {
        EnvFacts {
            crate_name: self.crate_name.as_str(),
            package: self.package.as_ref().map(|(name, version)| (name.as_str(), version.as_str())),
            bin: self.bin.as_ref().map(|bin| bin.as_str()),
            seen: self.seen,
            read_folder: self.read_folder.as_str(),
            build_script: self.build_script,
            script: self.script,
            test_or_bench: self.test_or_bench,
            // Made from a `BinNames` in `of`, copied byte for byte since.
            bin_exes: BinNames(self.bin_exes.as_str()),
            meta: self.meta_vars(),
        }
    }

    /// This environment copied whole into `alloc`, borrowing nothing of `self`: a reader that
    /// outlives the world's borrow (a pool body job's frame) holds its crates' environments so.
    /// The build script's outputs record is copied byte for byte and opened again; an error
    /// from that second open says the record's bytes are not what opened before. The copy
    /// borrows nothing, so it has no file area (`OutputsRef::area`): [`EnvLent::copy_in`],
    /// whose copy lives inside the lender's borrow, attaches it again.
    pub fn clone_in<'b>(&self, alloc: Bump<'b>) -> Result<CrateEnv<'b>, blaze_build_script::outputs::RecordError> {
        let a = alloc.alloc();
        let text = |s: &str| AString::from_str_in(s, a);
        let mut vars: AVec<'b, (AString<'b>, AString<'b>)> = AVec::with_capacity_in(self.vars.len(), a);
        vars.extend(self.vars.iter().map(|(n, v)| (text(n.as_str()), text(v.as_str()))));
        let mut unknown: AVec<'b, &'static str> = AVec::with_capacity_in(self.unknown.len(), a);
        unknown.extend(self.unknown.iter().copied());
        let seen = match self.seen {
            SeenRoot::Package => SeenRoot::Package,
            SeenRoot::Member { dir } => SeenRoot::Member { dir: alloc.alloc_str(dir) },
        };
        let package = self.package.as_ref().map(|(name, version)| (text(name.as_str()), text(version.as_str())));
        let script = match self.script {
            None => None,
            Some(record) => Some(OutputsRef::open(alloc.alloc_slice_copy(record.bytes()))?),
        };
        Ok(CrateEnv {
            vars,
            unknown,
            test_or_bench: self.test_or_bench,
            bin_exes: text(self.bin_exes.as_str()),
            seen,
            package,
            read_folder: text(self.read_folder.as_str()),
            script,
            crate_name: text(self.crate_name.as_str()),
            bin: self.bin.as_ref().map(|bin| text(bin.as_str())),
            build_script: self.build_script,
            meta: self.meta_in(a),
        })
    }

    /// This environment with its crate's build-script outputs `script` joined (decision D115:
    /// a script's outputs are read by the crate's task, never made into the world): `OUT_DIR`
    /// and each `cargo::rustc-env` pair given after cargo's own, a later pair of one name
    /// winning as in [`CrateEnv::of`] (so the result equals `of` with `script` in its facts);
    /// `OUT_DIR` no longer unknown; `script` the record its `OUT_DIR` files are read from.
    /// Copied whole into `alloc`, borrowing nothing of `self`.
    pub fn joined<'b>(&self, script: OutputsRef<'b>, alloc: Bump<'b>) -> CrateEnv<'b> {
        let a = alloc.alloc();
        let text = |s: &str| AString::from_str_in(s, a);
        // (name, value, the order it was given in): cargo's pairs first, then the script's.
        let mut given: AVec<'b, (AString<'b>, AString<'b>, u32)> = AVec::with_capacity_in(self.vars.len() + 1, a);
        for (name, value) in self.vars.iter() {
            let order = given.len() as u32;
            given.push((text(name.as_str()), text(value.as_str()), order));
        }
        let order = given.len() as u32;
        given.push((text("OUT_DIR"), text(script.out_dir()), order));
        for (name, value) in script.env() {
            let order = given.len() as u32;
            given.push((text(name), text(value), order));
        }
        given.sort_unstable_by(|a, b| a.0.as_str().cmp(b.0.as_str()).then(a.2.cmp(&b.2)));
        let mut vars: AVec<'b, (AString<'b>, AString<'b>)> = AVec::with_capacity_in(given.len(), a);
        for (name, value, _) in given {
            match vars.last_mut() {
                Some(last) if last.0.as_str() == name.as_str() => last.1 = value,
                _ => vars.push((name, value)),
            }
        }
        let mut unknown: AVec<'b, &'static str> = AVec::with_capacity_in(self.unknown.len(), a);
        unknown.extend(self.unknown.iter().copied().filter(|name| *name != "OUT_DIR"));
        let seen = match self.seen {
            SeenRoot::Package => SeenRoot::Package,
            SeenRoot::Member { dir } => SeenRoot::Member { dir: alloc.alloc_str(dir) },
        };
        let package = self.package.as_ref().map(|(name, version)| (text(name.as_str()), text(version.as_str())));
        CrateEnv {
            vars,
            unknown,
            test_or_bench: self.test_or_bench,
            bin_exes: text(self.bin_exes.as_str()),
            seen,
            package,
            read_folder: text(self.read_folder.as_str()),
            script: Some(script),
            crate_name: text(self.crate_name.as_str()),
            bin: self.bin.as_ref().map(|bin| text(bin.as_str())),
            build_script: self.build_script,
            meta: self.meta_in(a),
        }
    }

    /// The variable `name`: its value, `None` when the crate is compiled without it (`env!` of
    /// it is the Reference's compile error), or [`EnvUnknown`] when cargo sets it and this
    /// build does not know its value.
    pub fn get(&self, name: &str) -> Result<Option<&str>, EnvUnknown> {
        if let Ok(at) = self.vars.binary_search_by(|v| v.0.as_str().cmp(name)) {
            return Ok(Some(self.vars[at].1.as_str()));
        }
        if self.unknown.binary_search_by(|u| (*u).cmp(name)).is_ok() {
            return Err(EnvUnknown);
        }
        Ok(None)
    }

    /// The folder the crate's files are read under, with its trailing `/`, in the caller's path
    /// space: an include's read bound. Never what user code sees ([`CrateEnv::seen`]).
    pub fn read_folder(&self) -> &str {
        self.read_folder.as_str()
    }

    /// The root the crate sees (D96): what `CARGO_MANIFEST_DIR` names, and the root an
    /// absolute include path is taken under.
    pub fn seen(&self) -> SeenRoot<'a> {
        self.seen
    }

    /// What `file!()` gives for the file at `path` in the crate's package (package-relative,
    /// `src/lib.rs`), D96 (4): a dependency's `<name>-<version>/src/lib.rs`, a member's
    /// workspace-relative `crates/app/src/lib.rs` ([`SeenRoot::write_shown`], the one form both
    /// hosts give, so one call's read is answered byte-equal in the item and the body layer).
    /// `None` for a toolchain crate (no package in the lock, so no name and version to show):
    /// the host refuses the read then, never answers a path without them (rule 3).
    pub fn shown_file<'b>(&self, path: &str, alloc: Alloc<'b>) -> Option<AString<'b>> {
        let (name, version) = self.package.as_ref()?;
        let mut out = AString::new_in(alloc);
        // An arena string takes any length: writing cannot fail.
        let _ = self.seen.write_shown((name.as_str(), version.as_str()), path, &mut Into(&mut out));
        Some(out)
    }

    /// The build script's joined outputs record: its `OUT_DIR` files are read from it.
    pub fn script(&self) -> Option<OutputsRef<'a>> {
        self.script
    }

    /// Every variable with its value, sorted by name.
    pub fn vars(&self) -> impl Iterator<Item = (&str, &str)> {
        self.vars.iter().map(|(n, v)| (n.as_str(), v.as_str()))
    }

    /// Every name cargo sets for this crate whose value this build does not know, sorted: what
    /// [`CrateEnv::get`] answers [`EnvUnknown`] for. A proc macro's sandbox is given these, so
    /// its `getenv` of one is refused by name too (blaze-proc-macro package_env.rs,
    /// macro-env-unknown). No name is unknown by a prefix: each `CARGO_BIN_EXE_<name>` is set.
    pub fn unknown(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.unknown.iter().copied()
    }
}

/// A crate's environment lent to another thread to be COPIED there ([`CrateEnv::clone_in`]),
/// and nothing else: a pool worker's task (blaze-cli names.rs, a `CrateScope` off the main
/// thread) makes its own in its own frame. `CrateEnv` itself is not `Sync`: its collections
/// hold [`Alloc`] handles of an allocator that may be one thread's frame.
#[derive(Clone, Copy)]
pub struct EnvLent<'e, 'a>(&'e CrateEnv<'a>);

// SAFETY: the one way through an `EnvLent` is `copy_in`, which calls `clone_in`: that reads the
// env's vectors and strings through shared borrows (their buffers only) and allocates through
// the `alloc` it is given, never through an `Alloc` handle the env's own collections hold, so
// no allocator of the lending thread is called from another. The shared borrow `'e` means no
// thread mutates or drops the env while it is lent. An `OutputsRef` is plain borrowed bytes and
// at most a borrowed file area, which is `Sync` (`StoredBytes: Sync`).
unsafe impl Sync for EnvLent<'_, '_> {}
// SAFETY: as for `Sync`: the value is one shared borrow, used only as described there.
unsafe impl Send for EnvLent<'_, '_> {}

impl<'e, 'a> EnvLent<'e, 'a> {
    pub fn new(env: &'e CrateEnv<'a>) -> EnvLent<'e, 'a> {
        EnvLent(env)
    }

    /// The lent environment copied whole into `alloc` ([`CrateEnv::clone_in`]), its script
    /// record's file area (where an `include!` of a stored file reads it) attached again: the
    /// copy lives inside the lend (`'a: 'b`).
    pub fn copy_in<'b>(self, alloc: Bump<'b>) -> Result<CrateEnv<'b>, blaze_build_script::outputs::RecordError>
    where
        'a: 'b,
    {
        let mut copy = self.0.clone_in(alloc)?;
        if let (Some(record), Some(area)) = (copy.script, self.0.script.and_then(|lent| lent.area())) {
            copy.script = Some(record.with_area(area));
        }
        Ok(copy)
    }
}

impl<'a> UnitCrates<'a> {
    /// The environment of the crate at `at` of the list ([`CrateEnv`]), from its unit in
    /// `graph` (the graph [`crate::units::unit_crates`] made the list from): its package's name
    /// and version, its crate and binary name, its package's stable folder
    /// ([`crate::units::UnitCrate::shown_from`]) and its build script's joined record. Call it
    /// after `join_scripts`: a unit with a script and no record joined has `OUT_DIR` unknown.
    /// A toolchain crate's `CARGO_PKG_*` are unknown (it has no lock entry).
    pub fn env_of(&'a self, at: usize, graph: &'a UnitGraph<'_>, alloc: Bump<'a>) -> Result<CrateEnv<'a>, CrateEnvError<'a>> {
        let c = self.crates.get(at).ok_or(CrateEnvError::NoCrate { at })?;
        let facts = match c.origin {
            Origin::Unit(id) => {
                let unit = graph.units.get(id.index()).ok_or(CrateEnvError::NotItsUnit { krate: c.name, at })?;
                if unit.crate_name.as_str() != c.name {
                    return Err(CrateEnvError::NotItsUnit { krate: c.name, at });
                }
                let test_or_bench = matches!(unit.kind, UnitKind::IntegrationTest(_) | UnitKind::Bench(_));
                let bin_exes = match test_or_bench {
                    true => package_bins(graph, unit.package, c.name, alloc)?,
                    false => BinNames::NONE,
                };
                EnvFacts {
                    crate_name: c.name,
                    package: Some((unit.name.as_str(), unit.version.as_str())),
                    bin: match &unit.kind {
                        UnitKind::Bin(name) => Some(name.as_str()),
                        _ => None,
                    },
                    seen: c.seen,
                    read_folder: c.shown_from.as_str(),
                    build_script: c.build_script,
                    script: c.script,
                    test_or_bench,
                    bin_exes,
                    meta: graph.meta_of(unit.package).map(|meta| meta.vars()),
                }
            }
            Origin::Toolchain { .. } => EnvFacts {
                crate_name: c.name,
                package: None,
                bin: None,
                seen: c.seen,
                read_folder: c.shown_from.as_str(),
                build_script: false,
                script: None,
                test_or_bench: false,
                bin_exes: BinNames::NONE,
                meta: None,
            },
        };
        CrateEnv::of(facts, alloc.alloc())
    }
}

/// The binaries of package `package` in `graph`: its target-side `UnitKind::Bin` units (a
/// binary whose `required-features` are off has no unit: blaze-unit-graph resolve.rs skips
/// it, as cargo builds it not), sorted, each once. `krate`: the test or bench asking, named
/// by the refusal of a name that cannot be one of a list ([`CrateEnvError::BinName`]). The one
/// source of [`EnvFacts::bin_exes`] for a unit (this file's `env_of`, blaze-world list.rs).
pub fn package_bins<'a>(graph: &UnitGraph<'_>, package: blaze_manifest::lock::LockId, krate: &'a str, alloc: Bump<'a>) -> Result<BinNames<'a>, CrateEnvError<'a>> {
    let mut names: AVec<'_, &str> = AVec::new_in(alloc.alloc());
    for unit in graph.units.iter().filter(|unit| unit.package == package && !unit.for_host) {
        if let UnitKind::Bin(name) = &unit.kind {
            names.push(name.as_str());
        }
    }
    names.sort_unstable();
    names.dedup();
    let mut joined = BString::new_in(alloc);
    for name in names.iter() {
        if name.is_empty() || name.contains('\n') {
            let name = alloc.alloc_str(name);
            return Err(CrateEnvError::BinName { krate, name });
        }
        if !joined.is_empty() {
            joined.push_str("\n");
        }
        joined.push_str(name);
    }
    // Sorted, each once, none empty or holding a `\n`: `BinNames::new`'s form, checked above.
    Ok(BinNames(joined.into_bump_str()))
}

/// `fmt::Write` into an arena string.
struct Into<'s, 'a>(&'s mut AString<'a>);

impl fmt::Write for Into<'_, '_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0.push_str(s);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use blaze_expand::seen::SeenRoot;
    use blaze_own::Alloc;

    use super::{BinNames, CrateEnv, EnvFacts, EnvUnknown};

    fn facts(version: &'static str) -> EnvFacts<'static> {
        EnvFacts {
            crate_name: "tempo_node",
            package: Some(("tempo-node", version)),
            bin: None,
            seen: SeenRoot::Package,
            read_folder: "/7/",
            build_script: false,
            script: None,
            test_or_bench: false,
            bin_exes: BinNames::NONE,
            meta: None,
        }
    }

    /// turso cli/tests/non_interactive_exit_code.rs reads `env!("CARGO_BIN_EXE_tursodb")`; the
    /// check refused it (`Host(HostError { ask: Env, why: Refused })`, 28 notes). The cargo
    /// book, "Environment variables Cargo sets for crates": "CARGO_BIN_EXE_<name> — The
    /// absolute path to a binary target's executable. This is only set when building an
    /// integration test or benchmark." THE CAUSE REMOVED: an integration test is given one
    /// variable per binary of its package, each the guest path `/target/debug/bin/<package>/
    /// <name>`, so `env!` of it is answered. TWIN: the crate's facts given back answer it
    /// equal (a body job's environment, from the crate list). CONTROL: a name no binary has is
    /// unset (`option_env!` gives `None`, as cargo does), and the package's library (not a
    /// test) has none, though its binaries are named.
    #[test]
    fn an_integration_test_is_told_where_each_binary_of_its_package_is() {
        let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
        let erased = blaze_own::Erased::new(&frame);
        let bump = erased.bump();
        let alloc = bump.alloc();
        let bins = BinNames::new("tursodb\ntursodb-mcp").expect("sorted names");
        let test = CrateEnv::of(EnvFacts { test_or_bench: true, bin_exes: bins, package: Some(("turso_cli", "0.4.0")), ..facts("0.4.0") }, alloc).expect("a SemVer version");
        assert_eq!(test.get("CARGO_BIN_EXE_tursodb"), Ok(Some("/target/debug/bin/turso_cli/tursodb")));
        assert_eq!(test.get("CARGO_BIN_EXE_tursodb-mcp"), Ok(Some("/target/debug/bin/turso_cli/tursodb-mcp")));
        assert_eq!(super::bin_exe("turso_cli", "tursodb", alloc).as_str(), "/target/debug/bin/turso_cli/tursodb", "the one spelling");
        let again = CrateEnv::of(test.facts(), alloc).expect("a SemVer version");
        assert_eq!(again.get("CARGO_BIN_EXE_tursodb"), test.get("CARGO_BIN_EXE_tursodb"), "TWIN: its facts answer it equal");
        assert_eq!(test.clone_in(bump).expect("no script record").get("CARGO_BIN_EXE_tursodb-mcp"), test.get("CARGO_BIN_EXE_tursodb-mcp"), "TWIN: a copy answers it equal");
        assert_eq!(test.get("CARGO_BIN_EXE_sqlite3"), Ok(None), "CONTROL: no binary of that name: unset, never unknown");
        let lib = CrateEnv::of(EnvFacts { bin_exes: bins, ..facts("0.4.0") }, alloc).expect("a SemVer version");
        assert_eq!(lib.get("CARGO_BIN_EXE_tursodb"), Ok(None), "CONTROL: a library is no test: none set");
        assert!(BinNames::new("b\na").is_none() && BinNames::new("a\na").is_none() && BinNames::new("a\n").is_none(), "unsorted, twice, or an empty name is refused");
        assert_eq!(BinNames::new("").map(|names| names.names().count()), Some(0));
    }

    /// CLEAN: `env!("CARGO_PKG_NAME")` and the version's parts are the package's;
    /// `CARGO_MANIFEST_DIR` is the one root every dependency sees, `/package` (D96), never the
    /// folder the host reads the files under (`/7/` here) nor a machine path. Prints nothing.
    /// The cargo book, "Environment variables Cargo sets for crates": "CARGO_PKG_VERSION_PRE —
    /// The pre-release version of your package."
    #[test]
    fn a_crate_s_env_holds_its_package_s_name_version_and_stable_folder() {
        let env = CrateEnv::of(facts("1.20.3-rc.1+build.5"), Alloc::global()).expect("a SemVer version");
        assert_eq!(env.get("CARGO_PKG_NAME"), Ok(Some("tempo-node")));
        assert_eq!(env.get("CARGO_CRATE_NAME"), Ok(Some("tempo_node")));
        assert_eq!(env.get("CARGO_PKG_VERSION"), Ok(Some("1.20.3-rc.1+build.5")));
        assert_eq!(env.get("CARGO_PKG_VERSION_MAJOR"), Ok(Some("1")));
        assert_eq!(env.get("CARGO_PKG_VERSION_MINOR"), Ok(Some("20")));
        assert_eq!(env.get("CARGO_PKG_VERSION_PATCH"), Ok(Some("3")));
        assert_eq!(env.get("CARGO_PKG_VERSION_PRE"), Ok(Some("rc.1")));
        assert_eq!(env.get("CARGO_MANIFEST_DIR"), Ok(Some("/package")));
        assert_eq!(env.get("CARGO_MANIFEST_PATH"), Ok(Some("/package/Cargo.toml")));
        assert_eq!(env.read_folder(), "/7/");
        let member = CrateEnv::of(EnvFacts { seen: SeenRoot::member("crates/node").expect("a plain dir"), ..facts("1.0.0") }, Alloc::global()).expect("a SemVer version");
        assert_eq!(member.get("CARGO_MANIFEST_DIR"), Ok(Some("/workspace/crates/node")));
    }

    /// REFUSED, two ways. A name the crate is compiled without is unset (`env!` of it is the
    /// Reference's error: "If the environment variable is not defined, then a compilation
    /// error will be emitted"); a name cargo sets whose value is not known (the `[package]`
    /// metadata, `OUT_DIR` of a script whose record is not joined) is [`EnvUnknown`], never
    /// unset. Prints nothing; passes.
    #[test]
    fn an_unset_name_is_none_and_a_name_cargo_sets_unknown_is_refused() {
        let mut f = facts("0.4.0");
        f.build_script = true;
        let env = CrateEnv::of(f, Alloc::global()).expect("a SemVer version");
        assert_eq!(env.get("TEMPO_NOT_SET"), Ok(None));
        assert_eq!(env.get("CARGO_PKG_VERSION_PRE"), Ok(Some("")));
        assert_eq!(env.get("CARGO_PKG_AUTHORS"), Err(EnvUnknown));
        assert_eq!(env.get("OUT_DIR"), Err(EnvUnknown));
        assert_eq!(env.get("CARGO_BIN_EXE_tempo"), Ok(None));
        assert!(CrateEnv::of(facts("1.2"), Alloc::global()).is_err(), "a version without three parts is refused by name");
    }

    /// `facts` gives back what `of` was given: `of` of them answers every read as the
    /// original (a body job's environment is made so from the crate list, blaze-world
    /// crate_list.rs `env_facts`). CONTROL: facts with another version answer
    /// `CARGO_PKG_VERSION` otherwise. Prints nothing; passes.
    #[test]
    fn of_its_facts_answers_as_the_original() {
        let alloc = Alloc::global();
        let bin_exes = BinNames::new("tempo\nx").expect("sorted names");
        let given = EnvFacts { bin: Some("tempo"), seen: SeenRoot::member("bin/tempo").expect("a plain dir"), build_script: true, test_or_bench: true, bin_exes, ..facts("1.2.3-rc.1") };
        let env = CrateEnv::of(given, alloc).expect("a SemVer version");
        let again = CrateEnv::of(env.facts(), alloc).expect("a SemVer version");
        for name in super::CARGO_ALWAYS.iter().copied().chain(["CARGO_BIN_NAME", "OUT_DIR", "CARGO_TARGET_TMPDIR", "CARGO_BIN_EXE_x", "UNSET"]) {
            assert_eq!(again.get(name), env.get(name), "{name}");
        }
        assert_eq!(again.seen(), env.seen());
        assert_eq!(again.read_folder(), env.read_folder());
        let other = CrateEnv::of(EnvFacts { package: Some(("tempo-node", "1.2.4")), ..env.facts() }, alloc).expect("a SemVer version");
        assert_ne!(other.get("CARGO_PKG_VERSION"), env.get("CARGO_PKG_VERSION"), "CONTROL: another version");
    }

    /// D96 (4): `file!()` names a dependency's file `<name>-<version>/<path>` and a member's
    /// workspace-relative (`crates/node/src/lib.rs`), never the package-relative `src/lib.rs`
    /// every crate shared before; a toolchain crate (no package) gets no answer, so the host
    /// refuses the read. A build script's `rustc-env` cannot rename it. Prints nothing; passes.
    #[test]
    fn file_names_a_dependency_by_package_and_a_member_from_its_workspace() {
        let alloc = Alloc::global();
        let dependency = CrateEnv::of(facts("1.2.0"), alloc).expect("a SemVer version");
        assert_eq!(dependency.shown_file("src/lib.rs", alloc).as_ref().map(|s| s.as_str()), Some("tempo-node-1.2.0/src/lib.rs"));
        let member = CrateEnv::of(EnvFacts { seen: SeenRoot::member("crates/node").expect("a plain dir"), ..facts("1.2.0") }, alloc).expect("a SemVer version");
        assert_eq!(member.shown_file("src/lib.rs", alloc).as_ref().map(|s| s.as_str()), Some("crates/node/src/lib.rs"));
        let toolchain = CrateEnv::of(EnvFacts { package: None, ..facts("1.0.0") }, alloc).expect("no version to read");
        assert!(toolchain.shown_file("src/lib.rs", alloc).is_none());
    }

    /// `clone_in` gives an environment that answers every read as the original does and
    /// borrows nothing of it: the facts' member dir is a string dropped before the copy is
    /// read (a pool body job holds its crates' environments so, past the world's borrow).
    /// A script's outputs joined at task time (D115) give what [`CrateEnv::of`] gives with the
    /// record in its facts: `OUT_DIR` and the `rustc-env` pairs set, a script pair of a name
    /// cargo also sets winning (cargo's documentation of `cargo::rustc-env`: the variable is set
    /// when the package is compiled, after cargo's own), and `OUT_DIR` no longer unknown.
    /// Expected line: `test crate_env::tests::a_joined_env_gives_the_scripts_pairs_after_cargos ... ok`.
    /// Control: keep "OUT_DIR" in `unknown` in `joined`: "OUT_DIR is still unknown after the join".
    #[test]
    fn a_joined_env_gives_the_scripts_pairs_after_cargos() {
        let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
        let erased = blaze_own::Erased::new(&frame);
        let bump = erased.bump();
        let alloc = bump.alloc();
        let printed = "cargo:rustc-env=VERSION=7\ncargo:rustc-env=CARGO_PKG_NAME=renamed\n";
        let out = blaze_build_script::Outputs::read(printed.as_bytes(), "/out", blaze_build_script::instr::OutputLimits::DEFAULT, alloc).expect("the instructions read");
        let mut bytes = blaze_own::AVec::new_in(alloc);
        out.encode(&mut bytes);
        let record = blaze_build_script::OutputsRef::open(bump.alloc_slice_copy(&bytes)).expect("an outputs record");
        let bare = CrateEnv::of(EnvFacts { build_script: true, ..facts("1.0.0") }, alloc).expect("a SemVer version");
        assert_eq!(bare.get("OUT_DIR"), Err(EnvUnknown), "OUT_DIR is unknown before the join");
        let joined = bare.joined(record, bump);
        assert_eq!(joined.get("OUT_DIR"), Ok(Some("/out")), "OUT_DIR is still unknown after the join");
        assert_eq!(joined.get("VERSION"), Ok(Some("7")));
        assert_eq!(joined.get("CARGO_PKG_NAME"), Ok(Some("renamed")), "the script's pair is given after cargo's");
        let whole = CrateEnv::of(EnvFacts { build_script: true, script: Some(record), ..facts("1.0.0") }, alloc).expect("a SemVer version");
        assert_eq!(joined.vars().collect::<std::vec::Vec<_>>(), whole.vars().collect::<std::vec::Vec<_>>(), "the join differs from `of` with the record");
        assert!(joined.script().is_some());
    }

    /// Expected line: `test crate_env::tests::a_copied_environment_answers_alone ... ok`.
    #[test]
    fn a_copied_environment_answers_alone() {
        let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
        let erased = blaze_own::Erased::new(&frame);
        let bump = erased.bump();
        let alloc = bump.alloc();
        let copy = {
            let dir = std::string::String::from("crates/node");
            let facts = EnvFacts { seen: SeenRoot::member(dir.as_str()).expect("a plain dir"), test_or_bench: true, bin_exes: BinNames::new("node").expect("one name"), ..facts("1.2.0") };
            let env = CrateEnv::of(facts, alloc).expect("a SemVer version");
            env.clone_in(bump).expect("no script record to open")
        };
        assert_eq!(copy.get("CARGO_MANIFEST_DIR"), Ok(Some("/workspace/crates/node")));
        assert_eq!(copy.get("CARGO_PKG_VERSION_MINOR"), Ok(Some("2")));
        assert_eq!(copy.get("CARGO_BIN_EXE_node"), Ok(Some("/target/debug/bin/tempo-node/node")));
        assert_eq!(copy.shown_file("src/lib.rs", alloc).as_ref().map(|s| s.as_str()), Some("crates/node/src/lib.rs"));
        assert_eq!(copy.read_folder(), "/7/");
    }

    /// A toolchain crate has no package: its `CARGO_PKG_NAME` is unknown, never unset.
    #[test]
    fn a_toolchain_crate_s_package_variables_are_unknown() {
        let f = EnvFacts { package: None, read_folder: "/toolchain/core/", ..facts("1.0.0") };
        let env = CrateEnv::of(f, Alloc::global()).expect("no version to read");
        assert_eq!(env.get("CARGO_PKG_NAME"), Err(EnvUnknown));
        assert_eq!(env.get("CARGO_MANIFEST_DIR"), Ok(Some("/package")));
    }
}
