//! The variables a proc macro may read of the crate it is called in, each name one of three
//! truths (piece macro-env-unknown, 2026-11-16; rules 3, 19, 21):
//!
//! - SET, with its value: `std::env::var` gives `Ok(value)` (an empty value is a value);
//! - UNSET: `std::env::var` gives `Err(VarError::NotPresent)` (the pinned library's
//!   `env.rs`: "if the environment variable isn't present in the current process,
//!   `VarError::NotPresent`");
//! - UNKNOWN: cargo sets the name for the crate (the cargo book, "Environment variables Cargo
//!   sets for crates") and this build does not know its value: the read stops the expansion
//!   with [`EnvRefused`], naming the variable and the calling crate, never a guess.
//!
//! The truth of each name is not decided here: its one home is blaze-scope's `CrateEnv`
//! (crate_env.rs), the value `env!` and `option_env!` read in the crate's items and bodies, and
//! the sandbox is given that value whole (blaze-cli proc_env.rs `unit_package`). By that home:
//!
//! | name | truth |
//! |---|---|
//! | `CARGO_CRATE_NAME`, `CARGO_PKG_NAME`, `CARGO_PKG_VERSION` and its `_MAJOR`, `_MINOR`, `_PATCH`, `_PRE` | set (a toolchain crate's `CARGO_PKG_*`: unknown, it has no lock entry) |
//! | `CARGO_MANIFEST_DIR`, `CARGO_MANIFEST_PATH` | set: the root the sandbox serves the files at |
//! | `CARGO_BIN_NAME` | set for a binary; unset for any other crate |
//! | `OUT_DIR` and each `cargo::rustc-env` pair | set once the build script's record is joined; `OUT_DIR` unknown before; unset with no script |
//! | `CARGO` | unset by rule: there is no cargo binary; a sandboxed macro could not run one (`proc-macro-crate` then reads the package's own manifest) |
//! | the `[package]` metadata family (`CARGO_PKG_AUTHORS`, `_DESCRIPTION`, `_HOMEPAGE`, `_LICENSE`, `_LICENSE_FILE`, `_README`, `_REPOSITORY`, `_RUST_VERSION`) | set from the manifest (blaze-manifest layout meta.rs, the one table): authors joined by `:`, an inherited key the workspace's, an unstated one `""` as cargo sets it; unknown for a toolchain crate or a graph that does not carry it |
//! | `CARGO_PRIMARY_PACKAGE` | `1` for a workspace member's crate; unset for a dependency's |
//! | `CARGO_TARGET_TMPDIR` | unknown for an integration test or a bench; unset elsewhere |
//! | `CARGO_BIN_EXE_<name>` | for an integration test or a bench, set for each binary of its package to the guest path of its launcher stub (blaze-scope crate_env.rs `bin_exe`); unset for any other name and elsewhere |
//! | any other name (`PATH`, `HOME`, `DOCS_RS`, `CARGO_CFG_*`, ..) | unset: a build sees no ambient environment (rule 75), the same answer for anyone |
//!
//! The read's answer in a run's read list (`env:NAME`): a set value is the digest of its bytes,
//! unset is `Absent` (no digest), so `""` and unset key two different rows (rule 21). An unknown
//! read is never recorded: the run stops, and a kept answer whose list holds the name is not
//! taken where the name is unknown now (`sandbox::answer_now` has no answer for it).

use core::fmt;

/// What one variable is in a [`PackageEnv`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EnvAnswer<'e> {
    /// Set, with this value (`Ok(value)` for the macro; `""` is a value).
    Set(&'e str),
    /// Not set (`Err(NotPresent)` for the macro).
    Unset,
    /// Set by cargo, its value not known to this build: the read is refused by name.
    Unknown,
}

/// The variables of the calling crate as a proc macro may read them: see the module's doc.
#[derive(Clone, Debug, Default)]
pub struct PackageEnv {
    /// Each set name with its value, sorted by name, each name once.
    set: Vec<(String, String)>,
    /// Each unknown name, sorted, each once, none of them set.
    unknown: Vec<String>,
    /// Name prefixes every name under which is unknown unless set, sorted, each once. No
    /// crate's environment gives one today (blaze-cli proc_env.rs passes none); the tests plant
    /// `CARGO_BIN_EXE_` to prove the mechanism.
    unknown_prefixes: Vec<String>,
}

/// Why a [`PackageEnv`] could not be made: a bug of its maker, named.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PackageEnvError {
    /// One name given two values.
    SetTwice { name: String },
    /// One name given a value and called unknown.
    SetAndUnknown { name: String },
}

impl fmt::Display for PackageEnvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PackageEnvError::SetTwice { name } => write!(f, "the variable `{name}` was given two values for one crate"),
            PackageEnvError::SetAndUnknown { name } => write!(f, "the variable `{name}` was given a value and called unknown for one crate"),
        }
    }
}

impl core::error::Error for PackageEnvError {}

impl PackageEnv {
    /// The variables `set`, the names `unknown` and the prefixes `unknown_prefixes`; every other
    /// name is unset. Sorted here; a name set twice, or set and unknown, is refused.
    pub fn new(mut set: Vec<(String, String)>, mut unknown: Vec<String>, mut unknown_prefixes: Vec<String>) -> Result<PackageEnv, PackageEnvError> {
        set.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        if let Some(pair) = set.windows(2).find(|pair| pair[0].0 == pair[1].0) {
            return Err(PackageEnvError::SetTwice { name: pair[0].0.clone() });
        }
        unknown.sort_unstable();
        unknown.dedup();
        if let Some(name) = unknown.iter().find(|name| set.binary_search_by(|(it, _)| it.as_str().cmp(name.as_str())).is_ok()) {
            return Err(PackageEnvError::SetAndUnknown { name: name.clone() });
        }
        unknown_prefixes.sort_unstable();
        unknown_prefixes.dedup();
        Ok(PackageEnv { set, unknown, unknown_prefixes })
    }

    /// The variables `set` and no unknown name: every other name is unset. For a caller that
    /// models every name it gives (tests, the examples' hand-made packages).
    pub fn set_only(set: Vec<(String, String)>) -> Result<PackageEnv, PackageEnvError> {
        PackageEnv::new(set, Vec::new(), Vec::new())
    }

    /// What `name` is: set first (a build script's pair may set a name under an unknown
    /// prefix), then unknown, else unset: blaze-scope `CrateEnv::get`'s order.
    pub fn get(&self, name: &str) -> EnvAnswer<'_> {
        if let Ok(at) = self.set.binary_search_by(|(it, _)| it.as_str().cmp(name)) {
            return EnvAnswer::Set(self.set[at].1.as_str());
        }
        if self.unknown.binary_search_by(|it| it.as_str().cmp(name)).is_ok() || self.unknown_prefixes.iter().any(|prefix| name.starts_with(prefix.as_str())) {
            return EnvAnswer::Unknown;
        }
        EnvAnswer::Unset
    }

    /// Every set variable with its value, sorted by name.
    pub fn vars(&self) -> impl Iterator<Item = (&str, &str)> {
        self.set.iter().map(|(name, value)| (name.as_str(), value.as_str()))
    }

    /// The calling crate's name (`CARGO_CRATE_NAME`), when set: what a refusal names it by.
    pub fn crate_name(&self) -> Option<&str> {
        match self.get("CARGO_CRATE_NAME") {
            EnvAnswer::Set(name) => Some(name),
            EnvAnswer::Unset | EnvAnswer::Unknown => None,
        }
    }
}

/// A macro read a variable cargo sets for the calling crate whose value this build does not
/// know: the expansion stops, naming the variable and the crate (rules 3, 19).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct EnvRefused {
    /// The variable read.
    pub name: String,
    /// The calling crate (`CARGO_CRATE_NAME`); `None` when its package gives none.
    pub krate: Option<String>,
}

impl fmt::Display for EnvRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.krate {
            Some(krate) => write!(f, "read the variable `{}` in crate `{krate}`", self.name)?,
            None => write!(f, "read the variable `{}` in a crate that names itself no `CARGO_CRATE_NAME`", self.name)?,
        }
        write!(f, ", which cargo sets for the crate and this build does not know: refused, never answered unset (rule 3; blaze-scope crate_env.rs)")
    }
}

impl core::error::Error for EnvRefused {}

#[cfg(test)]
mod tests {
    //! The three truths through the sandbox's `getenv` (`Sandbox::var`) and its read-list
    //! answer (`answer_now`), for a package whose crate `app` has `EMPTY=""` set by its build
    //! script, `CARGO_PRIMARY_PACKAGE` unknown and `CARGO_BIN_EXE_` unknown (an integration
    //! test), and `GONE` unset.
    //!
    //! Rust's rule (the pinned library, `std/src/env.rs`, `var`): "Errors if the environment
    //! variable is not present." (`VarError::NotPresent`): an unset name is not `Ok("")`.
    //!
    //! Control (the planted bug, the env as it was before this piece: only the set pairs, every
    //! other name answered from them): `PackageEnv::set_only` of the same pairs answers
    //! `CARGO_PRIMARY_PACKAGE` unset, the quiet answer this piece refuses.
    //!
    //! Expected (`lanecheck.sh blaze-proc-macro test package_env`): "test blaze-proc-macro:
    //! exit 0; 2 passed, 0 failed"; the first prints "EMPTY=Some(\"\") GONE=None
    //! CARGO_PRIMARY_PACKAGE refused in crate `app`; \"\" and unset key apart".

    use std::sync::Arc;

    use super::{EnvAnswer, EnvRefused, PackageEnv, PackageEnvError};
    use crate::sandbox::{Files, Package, Sandbox, answer_now};

    fn env() -> PackageEnv {
        let set = vec![("EMPTY".to_owned(), String::new()), ("CARGO_CRATE_NAME".to_owned(), "app".to_owned())];
        PackageEnv::new(set, vec!["CARGO_PRIMARY_PACKAGE".to_owned()], vec!["CARGO_BIN_EXE_".to_owned()]).unwrap_or_else(|e| panic!("{e}"))
    }

    fn package(env: PackageEnv) -> Package {
        Package { files: Files::Listed(Arc::from(Vec::new())), env }
    }

    #[test]
    fn an_unset_variable_is_absent_a_set_one_its_value_an_unknown_one_a_named_refusal() {
        let mut sandbox = Sandbox::new(Arc::new(package(env())));
        let empty = sandbox.var("EMPTY");
        assert_eq!(empty, Ok(Some(String::new())), "a set empty value is a value");
        let gone = sandbox.var("GONE");
        assert_eq!(gone, Ok(None), "an unset name is NotPresent, not empty");
        assert_eq!(sandbox.var("CARGO_CRATE_NAME"), Ok(Some("app".to_owned())));
        let refused = sandbox.var("CARGO_PRIMARY_PACKAGE");
        assert_eq!(refused, Err(EnvRefused { name: "CARGO_PRIMARY_PACKAGE".to_owned(), krate: Some("app".to_owned()) }));
        assert_eq!(sandbox.env_refused.as_ref(), refused.as_ref().err(), "the sandbox keeps the refusal for the run's error");
        assert_eq!(sandbox.var("CARGO_BIN_EXE_node").map_err(|e| e.name), Err("CARGO_BIN_EXE_node".to_owned()), "a name under an unknown prefix");
        let shown = refused.as_ref().err().map(ToString::to_string).unwrap_or_default();
        assert!(shown.contains("`CARGO_PRIMARY_PACKAGE`") && shown.contains("crate `app`"), "the refusal names the variable and the crate: {shown}");
        // Only answered reads are on the run's list; the refused one never is.
        let subjects: Vec<&str> = sandbox.reads.iter().map(|read| read.subject.as_str()).collect();
        assert_eq!(subjects, ["env:EMPTY", "env:GONE", "env:CARGO_CRATE_NAME"]);

        // The key (rule 21): "" and unset answer apart, now and in the run's list.
        let now = package(env());
        let empty_now = answer_now(&now, "env:EMPTY").unwrap_or_else(|e| panic!("{e:?}"));
        let gone_now = answer_now(&now, "env:GONE").unwrap_or_else(|e| panic!("{e:?}"));
        assert!(empty_now.is_some() && gone_now.is_none(), "\"\" is a digest, unset is absent");
        assert_ne!(empty_now, gone_now);
        assert_eq!(sandbox.reads[0].digest, empty_now, "the run recorded what answer_now gives");
        assert_eq!(sandbox.reads[1].digest, None);
        assert!(answer_now(&now, "env:CARGO_PRIMARY_PACKAGE").is_err(), "an unknown name has no answer now: no kept answer is taken");

        // The control: the env as it was (set pairs only) answers the unknown name unset.
        let old = PackageEnv::set_only(env().vars().map(|(n, v)| (n.to_owned(), v.to_owned())).collect()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(old.get("CARGO_PRIMARY_PACKAGE"), EnvAnswer::Unset, "the planted old env refused the unknown name");
        assert_eq!(env().get("CARGO_PRIMARY_PACKAGE"), EnvAnswer::Unknown);

        println!("EMPTY={:?} GONE={:?} CARGO_PRIMARY_PACKAGE refused in crate `app`; \"\" and unset key apart", empty.ok().flatten(), gone.ok().flatten());
    }

    #[test]
    fn a_name_given_twice_or_set_and_unknown_is_refused() {
        let twice = PackageEnv::set_only(vec![("A".to_owned(), "1".to_owned()), ("A".to_owned(), "2".to_owned())]);
        assert_eq!(twice.err(), Some(PackageEnvError::SetTwice { name: "A".to_owned() }));
        let both = PackageEnv::new(vec![("A".to_owned(), "1".to_owned())], vec!["A".to_owned()], Vec::new());
        assert_eq!(both.err(), Some(PackageEnvError::SetAndUnknown { name: "A".to_owned() }));
        // Set wins over an unknown prefix (a build script's `rustc-env` pair), as `CrateEnv::get`.
        let script = PackageEnv::new(vec![("CARGO_BIN_EXE_x".to_owned(), "/bin/x".to_owned())], Vec::new(), vec!["CARGO_BIN_EXE_".to_owned()]).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(script.get("CARGO_BIN_EXE_x"), EnvAnswer::Set("/bin/x"));
        assert_eq!(script.get("CARGO_BIN_EXE_y"), EnvAnswer::Unknown);
    }
}
