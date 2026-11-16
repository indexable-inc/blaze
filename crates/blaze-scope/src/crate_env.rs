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
//! - `CARGO_CRATE_NAME`: the crate's name (`-` already `_` in the unit graph's `crate_name`);
//! - `CARGO_BIN_NAME`: a binary's name;
//! - `CARGO_MANIFEST_DIR`, `CARGO_MANIFEST_PATH`: the package's STABLE folder, `/package/N`
//!   (the folder its files are read under: units.rs's `shown_from`), never where the checkout
//!   is (rules 21, 25);
//! - `OUT_DIR` and every `cargo::rustc-env` pair: from the build script's joined outputs record
//!   (blaze-build-script's `OutputsRef`), a later pair of one name winning as cargo gives it.
//!
//! A name cargo always sets whose value this build does not know (`CARGO`, the `[package]`
//! metadata family: `CARGO_PKG_AUTHORS`, `_DESCRIPTION`, ..; `CARGO_PRIMARY_PACKAGE`;
//! `OUT_DIR` of a unit whose script's record is not joined) is UNKNOWN: a read of it is
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
use blaze_own::{AString, AVec, Alloc};
use blaze_unit_graph::{UnitGraph, UnitKind};

use crate::units::{Origin, UnitCrates};

/// Every name cargo sets for every crate it compiles (the cargo book's list for crates), sorted:
/// one of these a [`CrateEnv`] holds no value for is [`EnvUnknown`], never unset.
pub const CARGO_ALWAYS: &[&str] = &[
    "CARGO",
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
    "CARGO_PRIMARY_PACKAGE",
];

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
    /// The package's stable folder, as the crate's files are read under it (`/package/N/`, a
    /// trailing `/` allowed): `CARGO_MANIFEST_DIR` without the trailing `/`.
    pub folder: &'p str,
    /// The package has a build script.
    pub build_script: bool,
    /// Its outputs record, once joined (`UnitCrates::join_scripts`).
    pub script: Option<OutputsRef<'p>>,
    /// An integration test or a bench: `CARGO_TARGET_TMPDIR` and `CARGO_BIN_EXE_<name>` are set
    /// for it, to values this build does not know yet.
    pub test_or_bench: bool,
}

/// A crate's environment: see the module's doc.
pub struct CrateEnv<'a> {
    /// Each variable with its value, sorted by name, each name once.
    vars: AVec<'a, (AString<'a>, AString<'a>)>,
    /// The names cargo sets for this crate whose values are not known, sorted.
    unknown: AVec<'a, &'static str>,
    /// `CARGO_BIN_EXE_<name>` is set for this crate (an integration test or a bench) but not
    /// known.
    bin_exe_unknown: bool,
    /// The package's folder with its trailing `/` (`/package/N/`).
    folder: AString<'a>,
    script: Option<OutputsRef<'a>>,
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
        let mut put = |given: &mut AVec<'a, (AString<'a>, AString<'a>, u32)>, name: &str, value: &str| {
            let order = given.len() as u32;
            given.push((AString::from_str_in(name, alloc), AString::from_str_in(value, alloc), order));
        };
        let folder = facts.folder.trim_end_matches('/');
        let mut with_slash = AString::from_str_in(folder, alloc);
        with_slash.push_str("/");
        let mut manifest = AString::from_str_in(folder, alloc);
        manifest.push_str("/Cargo.toml");
        put(&mut given, "CARGO_CRATE_NAME", facts.crate_name);
        put(&mut given, "CARGO_MANIFEST_DIR", folder);
        put(&mut given, "CARGO_MANIFEST_PATH", manifest.as_str());
        if let Some((name, version)) = facts.package {
            let [major, minor, patch, pre] = version_parts(version).ok_or(CrateEnvError::Version { krate: facts.crate_name, version })?;
            put(&mut given, "CARGO_PKG_NAME", name);
            put(&mut given, "CARGO_PKG_VERSION", version);
            put(&mut given, "CARGO_PKG_VERSION_MAJOR", major);
            put(&mut given, "CARGO_PKG_VERSION_MINOR", minor);
            put(&mut given, "CARGO_PKG_VERSION_PATCH", patch);
            put(&mut given, "CARGO_PKG_VERSION_PRE", pre);
        }
        if let Some(bin) = facts.bin {
            put(&mut given, "CARGO_BIN_NAME", bin);
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
        Ok(CrateEnv { vars, unknown, bin_exe_unknown: facts.test_or_bench, folder: with_slash, script: facts.script })
    }

    /// The variable `name`: its value, `None` when the crate is compiled without it (`env!` of
    /// it is the Reference's compile error), or [`EnvUnknown`] when cargo sets it and this
    /// build does not know its value.
    pub fn get(&self, name: &str) -> Result<Option<&str>, EnvUnknown> {
        if let Ok(at) = self.vars.binary_search_by(|v| v.0.as_str().cmp(name)) {
            return Ok(Some(self.vars[at].1.as_str()));
        }
        if self.unknown.binary_search_by(|u| (*u).cmp(name)).is_ok() || (self.bin_exe_unknown && name.starts_with("CARGO_BIN_EXE_")) {
            return Err(EnvUnknown);
        }
        Ok(None)
    }

    /// The package's stable folder, with its trailing `/` (`/package/N/`): where the crate's
    /// files are read, and what `CARGO_MANIFEST_DIR` names.
    pub fn folder(&self) -> &str {
        self.folder.as_str()
    }

    /// The build script's joined outputs record: its `OUT_DIR` files are read from it.
    pub fn script(&self) -> Option<OutputsRef<'a>> {
        self.script
    }

    /// Every variable with its value, sorted by name.
    pub fn vars(&self) -> impl Iterator<Item = (&str, &str)> {
        self.vars.iter().map(|(n, v)| (n.as_str(), v.as_str()))
    }
}

impl<'a> UnitCrates<'a> {
    /// The environment of the crate at `at` of the list ([`CrateEnv`]), from its unit in
    /// `graph` (the graph [`crate::units::unit_crates`] made the list from): its package's name
    /// and version, its crate and binary name, its package's stable folder
    /// ([`crate::units::UnitCrate::shown_from`]) and its build script's joined record. Call it
    /// after `join_scripts`: a unit with a script and no record joined has `OUT_DIR` unknown.
    /// A toolchain crate's `CARGO_PKG_*` are unknown (it has no lock entry).
    pub fn env_of(&'a self, at: usize, graph: &'a UnitGraph<'_>, alloc: Alloc<'a>) -> Result<CrateEnv<'a>, CrateEnvError<'a>> {
        let c = self.crates.get(at).ok_or(CrateEnvError::NoCrate { at })?;
        let facts = match c.origin {
            Origin::Unit(id) => {
                let unit = graph.units.get(id.index()).ok_or(CrateEnvError::NotItsUnit { krate: c.name, at })?;
                if unit.crate_name.as_str() != c.name {
                    return Err(CrateEnvError::NotItsUnit { krate: c.name, at });
                }
                EnvFacts {
                    crate_name: c.name,
                    package: Some((unit.name.as_str(), unit.version.as_str())),
                    bin: match &unit.kind {
                        UnitKind::Bin(name) => Some(name.as_str()),
                        _ => None,
                    },
                    folder: c.shown_from.as_str(),
                    build_script: c.build_script,
                    script: c.script,
                    test_or_bench: matches!(unit.kind, UnitKind::IntegrationTest(_) | UnitKind::Bench(_)),
                }
            }
            Origin::Toolchain { .. } => EnvFacts {
                crate_name: c.name,
                package: None,
                bin: None,
                folder: c.shown_from.as_str(),
                build_script: false,
                script: None,
                test_or_bench: false,
            },
        };
        CrateEnv::of(facts, alloc)
    }
}

#[cfg(test)]
mod tests {
    use blaze_own::Alloc;

    use super::{CrateEnv, EnvFacts, EnvUnknown};

    fn facts(version: &'static str) -> EnvFacts<'static> {
        EnvFacts {
            crate_name: "tempo_node",
            package: Some(("tempo-node", version)),
            bin: None,
            folder: "/package/7/",
            build_script: false,
            script: None,
            test_or_bench: false,
        }
    }

    /// CLEAN: `env!("CARGO_PKG_NAME")` and the version's parts are the package's, the folder
    /// is the stable `/package/N`, never a machine path. Prints nothing; passes.
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
        assert_eq!(env.get("CARGO_MANIFEST_DIR"), Ok(Some("/package/7")));
        assert_eq!(env.get("CARGO_MANIFEST_PATH"), Ok(Some("/package/7/Cargo.toml")));
        assert_eq!(env.folder(), "/package/7/");
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

    /// A toolchain crate has no package: its `CARGO_PKG_NAME` is unknown, never unset.
    #[test]
    fn a_toolchain_crate_s_package_variables_are_unknown() {
        let f = EnvFacts { package: None, folder: "/toolchain/core/", ..facts("1.0.0") };
        let env = CrateEnv::of(f, Alloc::global()).expect("no version to read");
        assert_eq!(env.get("CARGO_PKG_NAME"), Err(EnvUnknown));
        assert_eq!(env.get("CARGO_MANIFEST_DIR"), Ok(Some("/toolchain/core")));
    }
}
