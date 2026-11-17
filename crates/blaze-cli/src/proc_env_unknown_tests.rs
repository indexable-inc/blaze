//! A proc macro's `getenv` answers each name as `env!` does in the same crate (piece
//! macro-env-unknown, 2026-11-16; rules 3, 19, 21, 30): set is the value, unset is
//! `NotPresent`, and a name cargo sets whose value this build does not know is refused by
//! name, never answered unset.
//!
//! Rust's rule: the pinned library's `std/src/env.rs`, `var`: "Errors if the environment
//! variable is not present" (`VarError::NotPresent`), so unset is not `Ok("")`; and core's
//! `env!` doc (`macros/mod.rs`): "If the environment variable is not defined, then a
//! compilation error will be emitted." The cargo book, "Environment variables Cargo sets for
//! crates": `CARGO_PRIMARY_PACKAGE`, the `CARGO_PKG_*` metadata family, `OUT_DIR` ("If the
//! package has a build script"), `CARGO_TARGET_TMPDIR` and `CARGO_BIN_EXE_<name>` ("Only set
//! when building integration test or benchmark code"); blaze-scope crate_env.rs says which of
//! them this build knows.
//!
//! The crate: package `app` 0.1.0's integration test `app_it`, with a build script whose record
//! is not joined yet. The truth for each name is blaze-scope's `CrateEnv::get`; the macro's is
//! `PackageEnv::get` of `unit_package` (blaze-cli proc_env.rs), checked equal name by name, and
//! the sandbox's `var` refuses an unknown one naming the variable and the crate.
//!
//! Control (the planted bug, the package as macro-env-per-unit made it: the set pairs alone):
//! `PackageEnv::set_only(unit_env(..))` answers `CARGO_PRIMARY_PACKAGE` unset while `env!`
//! refuses it.
//!
//! Expected (`lanecheck.sh blaze-cli test proc_env_unknown`): "test blaze-cli: exit 0; 1
//! passed, 0 failed", the test printing "macro getenv agrees with env! on 14 names;
//! CARGO_PRIMARY_PACKAGE refused in crate `app_it`; the set-only env answered it unset".

use std::string::String;
use std::sync::Arc;

use blaze_proc_macro::package_env::{EnvAnswer, EnvRefused, PackageEnv};
use blaze_proc_macro::sandbox::{Files, Package, Sandbox};
use blaze_scope::crate_env::{CrateEnv, EnvFacts, SeenRoot};

use crate::proc_env::{unit_env, unit_package};

/// The names asked: each truth of the module's doc at least once.
const NAMES: [&str; 14] = [
    "CARGO_CRATE_NAME",
    "CARGO_PKG_NAME",
    "CARGO_PKG_VERSION_MAJOR",
    "CARGO_PKG_VERSION_PRE",
    "CARGO_MANIFEST_DIR",
    "CARGO",
    "CARGO_PKG_AUTHORS",
    "CARGO_PKG_DESCRIPTION",
    "CARGO_PRIMARY_PACKAGE",
    "OUT_DIR",
    "CARGO_TARGET_TMPDIR",
    "CARGO_BIN_EXE_app",
    "CARGO_BIN_NAME",
    "PATH",
];

fn integration_test() -> CrateEnv<'static> {
    let facts = EnvFacts {
        crate_name: "app_it",
        package: Some(("app", "0.1.0")),
        bin: None,
        seen: SeenRoot::member("app").expect("a plain member dir"),
        read_folder: "/0/app/",
        build_script: true,
        script: None,
        test_or_bench: true,
    };
    CrateEnv::of(facts, blaze_own::testing::heap().alloc()).unwrap_or_else(|e| panic!("the environment of app_it: {e}"))
}

/// `CrateEnv::get`'s answer in `PackageEnv`'s words.
fn truth<'e>(env: &'e CrateEnv<'_>, name: &str) -> EnvAnswer<'e> {
    match env.get(name) {
        Ok(Some(value)) => EnvAnswer::Set(value),
        Ok(None) => EnvAnswer::Unset,
        Err(_) => EnvAnswer::Unknown,
    }
}

#[test]
fn a_macro_s_getenv_answers_each_name_as_env_does_and_refuses_an_unknown_one_by_name() {
    let env = integration_test();
    let package = unit_package(&env, Files::Listed(Arc::from(std::vec::Vec::new()))).unwrap_or_else(|e| panic!("{e}"));
    for name in NAMES {
        let shown = package.env.get(name);
        match name {
            // The one name the sandbox answers its own way: the root it SERVES the files at
            // (proc_env.rs `unit_env`), set in both.
            "CARGO_MANIFEST_DIR" => assert_eq!(shown, EnvAnswer::Set("/package"), "{name}"),
            _ => assert_eq!(shown, truth(&env, name), "{name}: the macro's getenv and env! disagree"),
        }
    }
    // The three truths are all met (the list is not vacuous).
    assert_eq!(package.env.get("CARGO_CRATE_NAME"), EnvAnswer::Set("app_it"));
    assert_eq!(package.env.get("CARGO_PRIMARY_PACKAGE"), EnvAnswer::Unknown);
    assert_eq!(package.env.get("OUT_DIR"), EnvAnswer::Unknown, "a script whose record is not joined");
    assert_eq!(package.env.get("CARGO_BIN_EXE_app"), EnvAnswer::Unknown, "an integration test's binary path");
    assert_eq!(package.env.get("CARGO_BIN_NAME"), EnvAnswer::Unset, "not a binary");
    assert_eq!(package.env.get("PATH"), EnvAnswer::Unset, "no ambient environment (rule 75)");

    // The sandbox: an unknown read is a refusal naming the variable and the calling crate.
    let mut sandbox = Sandbox::new(Arc::new(package));
    assert_eq!(sandbox.var("PATH"), Ok(None), "unset is NotPresent");
    let refused = sandbox.var("CARGO_PRIMARY_PACKAGE");
    assert_eq!(refused, Err(EnvRefused { name: String::from("CARGO_PRIMARY_PACKAGE"), krate: Some(String::from("app_it")) }));

    // The control: the set pairs alone answer the unknown name unset, where env! refuses it.
    let old = Package { files: Files::Listed(Arc::from(std::vec::Vec::new())), env: PackageEnv::set_only(set_pairs(&env)).unwrap_or_else(|e| panic!("{e}")) };
    assert_eq!(old.env.get("CARGO_PRIMARY_PACKAGE"), EnvAnswer::Unset, "the planted set-only env refused an unknown name");
    assert_ne!(old.env.get("CARGO_PRIMARY_PACKAGE"), truth(&env, "CARGO_PRIMARY_PACKAGE"));

    println!(
        "macro getenv agrees with env! on {} names; CARGO_PRIMARY_PACKAGE refused in crate `{}`; the set-only env answered it unset",
        NAMES.len(),
        refused.err().and_then(|e| e.krate).unwrap_or_default()
    );
}

/// The set pairs as macro-env-per-unit's `unit_package` gave them (the control's env).
fn set_pairs(env: &CrateEnv<'_>) -> std::vec::Vec<(String, String)> {
    let files = Files::Listed(Arc::from(std::vec::Vec::new()));
    let mut out = std::vec::Vec::new();
    out.extend(unit_env(env, &files));
    out
}
