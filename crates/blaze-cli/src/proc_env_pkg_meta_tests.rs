//! A proc macro's `getenv` of the `[package]` metadata (`CARGO_PKG_AUTHORS`, ..) is answered
//! from the manifest, as `env!` is, through the seam a real call takes: the crate's
//! `CrateEnv` (blaze-scope crate_env.rs, made with the unit graph's `PackageMeta`), the
//! sandbox's package (proc_env.rs `unit_package`), the sandbox's `var`. The failure this
//! answers: tempo's `xtask` `#[derive(clap::Parser)]` stopped on "read the variable
//! CARGO_PKG_AUTHORS in crate tempo_xtask, which cargo sets ... refused".
//!
//! The cargo book, "Environment variables Cargo sets for crates": "CARGO_PKG_AUTHORS — Colon
//! separated list of authors from the manifest of your package." A name cargo does not set is
//! unset (`NotPresent`; blaze-scope crate_env.rs: a build sees no ambient environment), and a
//! name cargo sets whose value is not known is refused by name.
//!
//! The grain (rule 23): a run's read list holds `env:CARGO_PKG_AUTHORS` with its value's
//! digest (sandbox.rs `var`), and a kept answer is taken only where each read's answer now
//! (`answer_now`, table.rs `list_holds`) equals it: an authors edit moves that answer, so the
//! call that read it runs again; a description edit moves no answer the call read, so its
//! kept answer is taken. The read is the grain: one variable of one crate.
//!
//! Expected (`cargo test -p blaze-cli --bin blaze -- proc_env_pkg_meta`): 2 passed, printing
//! "CARGO_PKG_AUTHORS=`A <a@x>:B` in crate `tempo_xtask`; NOT_A_CARGO_VAR unset;
//! CARGO_TARGET_TMPDIR refused by name; an authors edit re-enters, a description edit does not".

use std::string::String;
use std::sync::Arc;

use blaze_proc_macro::package_env::EnvRefused;
use blaze_proc_macro::sandbox::{Files, Sandbox, answer_now};
use blaze_scope::crate_env::{CrateEnv, EnvFacts, SeenRoot};
use blaze_unit_graph::PackageMetaVars;

use crate::proc_env::unit_package;

/// tempo's `xtask` member: a binary whose manifest states authors and a description.
fn xtask(meta: PackageMetaVars<'static>, test_or_bench: bool) -> CrateEnv<'static> {
    let facts = EnvFacts {
        crate_name: "tempo_xtask",
        package: Some(("tempo-xtask", "0.1.0")),
        bin: (!test_or_bench).then_some("tempo-xtask"),
        seen: SeenRoot::member("xtask").unwrap_or_else(|e| panic!("{e}")),
        read_folder: "/0/xtask/",
        build_script: false,
        script: None,
        test_or_bench,
        bin_exes: blaze_scope::crate_env::BinNames::NONE,
        meta: Some(meta),
    };
    CrateEnv::of(facts, blaze_own::testing::heap().alloc()).unwrap_or_else(|e| panic!("the environment of tempo_xtask: {e}"))
}

const WRITTEN: PackageMetaVars<'static> = PackageMetaVars {
    authors: "A <a@x>:B",
    description: "the xtask",
    homepage: "",
    license: "MIT OR Apache-2.0",
    license_file: "",
    readme: "",
    repository: "https://example.org/tempo",
    rust_version: "1.88",
};

/// The member `xtask` as the runner serves it (D96 (3)).
fn member() -> Files {
    let root = blaze_proc_macro::script_sandbox::ScriptRoot::member("xtask").unwrap_or_else(|e| panic!("{e}"));
    Files::Member { workspace: std::path::PathBuf::from("/nonexistent-ws"), root }
}

#[test]
fn a_macro_reads_cargo_pkg_authors_as_env_does_and_an_unknown_name_is_refused_by_name() {
    let env = xtask(WRITTEN, false);
    // `env!`'s answer (blaze-expand reads `CrateEnv::get`).
    assert_eq!(env.get("CARGO_PKG_AUTHORS"), Ok(Some("A <a@x>:B")));
    assert_eq!(env.get("CARGO_PKG_HOMEPAGE"), Ok(Some("")), "an unstated key is cargo's empty value");
    let mut sandbox = Sandbox::new(Arc::new(unit_package(&env, member()).unwrap_or_else(|e| panic!("{e:?}"))));
    let authors = sandbox.var("CARGO_PKG_AUTHORS");
    assert_eq!(authors, Ok(Some(String::from("A <a@x>:B"))), "the derive of clap::Parser reads the manifest's authors");
    assert_eq!(sandbox.var("CARGO_PKG_DESCRIPTION"), Ok(Some(String::from("the xtask"))));
    assert_eq!(sandbox.var("CARGO_PKG_RUST_VERSION"), Ok(Some(String::from("1.88"))));
    assert_eq!(sandbox.var("CARGO_PRIMARY_PACKAGE"), Ok(Some(String::from("1"))), "a member's crate");
    // A name cargo never sets is unset, not refused: no ambient environment (rule 75).
    assert_eq!(sandbox.var("NOT_A_CARGO_VAR"), Ok(None));
    // A name cargo sets whose value this build does not know is refused by name.
    let mut test = Sandbox::new(Arc::new(unit_package(&xtask(WRITTEN, true), member()).unwrap_or_else(|e| panic!("{e:?}"))));
    assert_eq!(
        test.var("CARGO_TARGET_TMPDIR"),
        Err(EnvRefused { name: String::from("CARGO_TARGET_TMPDIR"), krate: Some(String::from("tempo_xtask")) })
    );
    // CONTROL: the graph carrying no metadata leaves the old refusal in place, named.
    let unknown = CrateEnv::of(EnvFacts { meta: None, ..env.facts() }, blaze_own::testing::heap().alloc()).unwrap_or_else(|e| panic!("{e}"));
    let mut old = Sandbox::new(Arc::new(unit_package(&unknown, member()).unwrap_or_else(|e| panic!("{e:?}"))));
    assert_eq!(old.var("CARGO_PKG_AUTHORS"), Err(EnvRefused { name: String::from("CARGO_PKG_AUTHORS"), krate: Some(String::from("tempo_xtask")) }));
    println!(
        "CARGO_PKG_AUTHORS=`{}` in crate `tempo_xtask`; NOT_A_CARGO_VAR unset; CARGO_TARGET_TMPDIR refused by name",
        authors.ok().flatten().unwrap_or_default()
    );
}

/// Rule 23: the read `env:CARGO_PKG_AUTHORS` a call recorded is answered otherwise after an
/// authors edit (the call re-enters) and alike after a description edit (it does not).
#[test]
fn an_authors_edit_re_enters_its_reader_and_a_description_edit_does_not() {
    let read = "env:CARGO_PKG_AUTHORS";
    let before = unit_package(&xtask(WRITTEN, false), member()).unwrap_or_else(|e| panic!("{e:?}"));
    let recorded = answer_now(&before, read).unwrap_or_else(|e| panic!("{e:?}"));
    assert!(recorded.is_some(), "a set value is recorded by its digest");
    let authors_edited = unit_package(&xtask(PackageMetaVars { authors: "A <a@x>", ..WRITTEN }, false), member()).unwrap_or_else(|e| panic!("{e:?}"));
    assert_ne!(answer_now(&authors_edited, read).unwrap_or_else(|e| panic!("{e:?}")), recorded, "an authors edit must move the read's answer");
    let described = unit_package(&xtask(PackageMetaVars { description: "another", ..WRITTEN }, false), member()).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(answer_now(&described, read).unwrap_or_else(|e| panic!("{e:?}")), recorded, "a description edit moved the authors' answer");
    assert_ne!(
        answer_now(&described, "env:CARGO_PKG_DESCRIPTION").unwrap_or_else(|e| panic!("{e:?}")),
        answer_now(&before, "env:CARGO_PKG_DESCRIPTION").unwrap_or_else(|e| panic!("{e:?}")),
        "CONTROL: the description's own read moves"
    );
    println!("an authors edit re-enters, a description edit does not");
}
