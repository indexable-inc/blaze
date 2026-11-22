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
//! `PackageEnv::set_only(unit_env(..))` answers `CARGO_TARGET_TMPDIR` unset while `env!`
//! refuses it. (`CARGO_PRIMARY_PACKAGE` is known now: `1` for this member's crate.)
//!
//! Expected (`lanecheck.sh blaze-cli test proc_env_unknown`): "test blaze-cli: exit 0; 1
//! passed, 0 failed", the test printing "macro getenv agrees with env! on 14 names;
//! CARGO_TARGET_TMPDIR refused in crate `app_it`; the set-only env answered it unset".

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
        bin_exes: blaze_scope::crate_env::BinNames::new("app").expect("one name"),
        meta: None,
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
    let package = unit_package(&env, member()).unwrap_or_else(|e| panic!("{e:?}"));
    for name in NAMES {
        // Every name, `CARGO_MANIFEST_DIR` too: the member is served at the root its `CrateEnv`
        // says (D96), so the macro's answer is `env!`'s.
        assert_eq!(package.env.get(name), truth(&env, name), "{name}: the macro's getenv and env! disagree");
    }
    // The three truths are all met (the list is not vacuous).
    assert_eq!(package.env.get("CARGO_CRATE_NAME"), EnvAnswer::Set("app_it"));
    assert_eq!(package.env.get("CARGO_PRIMARY_PACKAGE"), EnvAnswer::Set("1"), "a member's crate is a selected package");
    assert_eq!(package.env.get("CARGO_TARGET_TMPDIR"), EnvAnswer::Unknown);
    assert_eq!(package.env.get("OUT_DIR"), EnvAnswer::Unknown, "a script whose record is not joined");
    assert_eq!(package.env.get("CARGO_BIN_EXE_app"), EnvAnswer::Set("/target/debug/bin/app/app"), "an integration test's binary: the guest path of its launcher stub");
    assert_eq!(package.env.get("CARGO_BIN_EXE_other"), EnvAnswer::Unset, "no binary of that name in the package");
    assert_eq!(package.env.get("CARGO_BIN_NAME"), EnvAnswer::Unset, "not a binary");
    assert_eq!(package.env.get("CARGO"), EnvAnswer::Unset, "no cargo binary: proc-macro-crate reads the package's manifest");
    assert_eq!(package.env.get("PATH"), EnvAnswer::Unset, "no ambient environment (rule 75)");

    // The sandbox: an unknown read is a refusal naming the variable and the calling crate.
    let mut sandbox = Sandbox::new(Arc::new(package));
    assert_eq!(sandbox.var("PATH"), Ok(None), "unset is NotPresent");
    let refused = sandbox.var("CARGO_TARGET_TMPDIR");
    assert_eq!(refused, Err(EnvRefused { name: String::from("CARGO_TARGET_TMPDIR"), krate: Some(String::from("app_it")) }));

    // The control: the set pairs alone answer the unknown name unset, where env! refuses it.
    let old = Package::new(member(), PackageEnv::set_only(set_pairs(&env)).unwrap_or_else(|e| panic!("{e}"))).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(old.env.get("CARGO_TARGET_TMPDIR"), EnvAnswer::Unset, "the planted set-only env refused an unknown name");
    assert_ne!(old.env.get("CARGO_TARGET_TMPDIR"), truth(&env, "CARGO_TARGET_TMPDIR"));

    println!(
        "macro getenv agrees with env! on {} names; CARGO_TARGET_TMPDIR refused in crate `{}`; the set-only env answered it unset",
        NAMES.len(),
        refused.err().and_then(|e| e.krate).unwrap_or_default()
    );
}

/// `proc-macro-crate` 3.5.0's `crate_name` (jsonrpsee's `#[rpc]`, tempo's 28 "no
/// `XApiServer`"): it reads `CARGO_MANIFEST_DIR`, then `CARGO` (set: it runs `cargo
/// locate-project`), then `$CARGO_MANIFEST_DIR/Cargo.toml`. A fetched git package (reth's
/// `reth-rpc-api`, its manifest as written in the repository, `workspace = true` keys and all)
/// is served from its record at `/package`: `CARGO` is unset by rule (no cargo binary), and the
/// manifest is the package's own bytes. CONTROL: the env as it was (`CARGO` among the unknown
/// names) refuses the read, which stopped every such call before the macro looked.
///
/// Expected: `test proc_env_unknown_tests::a_git_package_s_macro_finds_cargo_unset_and_reads_its_own_manifest ... ok`.
#[test]
fn a_git_package_s_macro_finds_cargo_unset_and_reads_its_own_manifest() {
    use blaze_wasi::{Open, ROOT_FD, System};

    const MANIFEST: &[u8] = b"[package]\nname = \"reth-rpc-api\"\nversion.workspace = true\n\n[dependencies]\njsonrpsee = { workspace = true, features = [\"server\", \"macros\"] }\n";
    let facts = EnvFacts {
        crate_name: "reth_rpc_api",
        package: Some(("reth-rpc-api", "1.9.3")),
        bin: None,
        seen: SeenRoot::Package,
        read_folder: "/1/crates/rpc/rpc-api/",
        build_script: false,
        script: None,
        test_or_bench: false,
        bin_exes: blaze_scope::crate_env::BinNames::NONE,
        meta: None,
    };
    let env = CrateEnv::of(facts, blaze_own::testing::heap().alloc()).unwrap_or_else(|e| panic!("the environment of reth_rpc_api: {e}"));
    let files = || {
        let listed: std::vec::Vec<(String, &'static [u8])> = std::vec![(String::from("Cargo.toml"), MANIFEST), (String::from("src/lib.rs"), &b""[..])];
        Files::Listed(Arc::from(listed))
    };
    let package = unit_package(&env, files()).unwrap_or_else(|e| panic!("{e:?}"));
    let mut sandbox = Sandbox::new(Arc::new(package));
    let dir = sandbox.var("CARGO_MANIFEST_DIR").unwrap_or_else(|e| panic!("{e:?}")).unwrap_or_else(|| panic!("CARGO_MANIFEST_DIR unset"));
    assert_eq!(dir, "/package");
    // asks blaze: asserts that the sandboxed proc-macro env leaves CARGO unset
    assert_eq!(sandbox.var("CARGO"), Ok(None), "CARGO is unset by rule: proc-macro-crate skips `cargo locate-project`");
    const READ_ONLY: Open = Open { dirflags: 1, oflags: 0, rights_base: 1 << 1, rights_inheriting: 0, fdflags: 0 };
    let path = std::format!("{dir}/Cargo.toml");
    let fd = match sandbox.open(ROOT_FD, path.as_bytes(), READ_ONLY) {
        Ok(Ok(fd)) => fd,
        Ok(Err(errno)) => panic!("{path}: errno {errno:?}"),
        Err(stop) => panic!("{path}: {}", stop.0),
    };
    let mut read = std::vec::Vec::new();
    let mut buf = [0u8; 64];
    loop {
        match sandbox.read(fd, &mut buf) {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => read.extend_from_slice(&buf[..n]),
            Ok(Err(errno)) => panic!("{path}: read errno {errno:?}"),
            Err(stop) => panic!("{path}: {}", stop.0),
        }
    }
    assert_eq!(read, MANIFEST, "the macro reads the package's manifest as the repository has it");

    // The control: `CARGO` among the unknown names, as crate_env.rs `CARGO_ALWAYS` had it.
    let mut old_unknown: std::vec::Vec<String> = env.unknown().map(String::from).collect();
    old_unknown.push(String::from("CARGO"));
    let mut set = std::vec::Vec::new();
    set.extend(unit_env(&env));
    let old_env = PackageEnv::new(set, old_unknown, std::vec::Vec::new()).unwrap_or_else(|e| panic!("{e}"));
    let mut old = Sandbox::new(Arc::new(Package::new(files(), old_env).unwrap_or_else(|e| panic!("{e}"))));
    // asks blaze: asserts that the planted old env refused CARGO
    assert!(old.var("CARGO").is_err(), "the planted old env answered CARGO");
    println!("git package's macro: CARGO unset, read {} manifest bytes at {path}; the old env refused CARGO", read.len());
}

/// The set pairs as macro-env-per-unit's `unit_package` gave them (the control's env).
fn set_pairs(env: &CrateEnv<'_>) -> std::vec::Vec<(String, String)> {
    let mut out = std::vec::Vec::new();
    out.extend(unit_env(env));
    out
}

/// The member `app` as the runner serves it (D96 (3)): its workspace at `/workspace`, the
/// member at `app`, the dir `integration_test`'s `CrateEnv` was made with.
fn member() -> Files {
    let root = blaze_proc_macro::script_sandbox::ScriptRoot::member("app").unwrap_or_else(|e| panic!("{e}"));
    Files::Member { workspace: std::path::PathBuf::from("/nonexistent-ws"), root }
}
