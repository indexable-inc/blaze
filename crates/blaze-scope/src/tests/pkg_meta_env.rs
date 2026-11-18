//! A crate's environment holds every variable cargo sets from its manifest (crate_env.rs): the
//! version's parts, the `[package]` metadata from the one table (blaze-manifest layout
//! meta.rs) and `CARGO_PRIMARY_PACKAGE` for a member; what it is made from round-trips through
//! `facts` (the crate list carries it to a body job), and a build script's `rustc-env` pair of
//! a metadata name is given after cargo's, as cargo gives it.
//!
//! Expected (`cargo test -p blaze-scope --lib -- tests::pkg_meta_env`): 2 passed.

use blaze_expand::seen::SeenRoot;
use blaze_unit_graph::PackageMetaVars;

use crate::crate_env::{CrateEnv, EnvFacts, EnvUnknown};

const META: PackageMetaVars<'static> = PackageMetaVars {
    authors: "A <a@x>:B",
    description: "",
    homepage: "https://example.org",
    license: "MIT",
    license_file: "",
    readme: "README.md",
    repository: "",
    rust_version: "1.80",
};

/// The tests' allocator, as this crate's other test files make theirs (tests/refused.rs): on
/// the sweep's list of ambient-heap lines (runner/global-debt.md), to come from the test's top.
fn a() -> blaze_own::Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn facts(seen: SeenRoot<'static>, meta: Option<PackageMetaVars<'static>>) -> EnvFacts<'static> {
    EnvFacts {
        crate_name: "tempo_node",
        package: Some(("tempo-node", "1.2.3-rc.1")),
        bin: None,
        seen,
        read_folder: "/0/node/",
        build_script: false,
        script: None,
        test_or_bench: false,
        meta,
    }
}

/// The table's values, the version's parts and `CARGO_PRIMARY_PACKAGE`; a dependency's
/// `CARGO_PRIMARY_PACKAGE` is unset; without metadata the names stay refused by name.
#[test]
fn a_crate_s_env_holds_its_manifest_s_metadata_and_version_parts() {
    let alloc = a();
    let member = SeenRoot::member("node").unwrap_or_else(|e| panic!("{e}"));
    let env = CrateEnv::of(facts(member, Some(META)), alloc).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(env.get("CARGO_PKG_VERSION_MAJOR"), Ok(Some("1")));
    assert_eq!(env.get("CARGO_PKG_VERSION_MINOR"), Ok(Some("2")));
    assert_eq!(env.get("CARGO_PKG_VERSION_PATCH"), Ok(Some("3")));
    assert_eq!(env.get("CARGO_PKG_VERSION_PRE"), Ok(Some("rc.1")));
    for (name, value) in META.pairs() {
        assert_eq!(env.get(name), Ok(Some(value)), "{name}");
    }
    assert_eq!(env.get("CARGO_PKG_DESCRIPTION"), Ok(Some("")), "unstated is cargo's empty value, known");
    assert_eq!(env.get("CARGO_PRIMARY_PACKAGE"), Ok(Some("1")));
    assert!(env.unknown().all(|name| !name.starts_with("CARGO_PKG_")), "no CARGO_PKG_ name left unknown");
    let dependency = CrateEnv::of(facts(SeenRoot::Package, Some(META)), alloc).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(dependency.get("CARGO_PRIMARY_PACKAGE"), Ok(None), "a dependency is not a selected package");
    // CONTROL: no metadata carried, the names are refused, never `""`.
    let unknown = CrateEnv::of(facts(member, None), alloc).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(unknown.get("CARGO_PKG_AUTHORS"), Err(EnvUnknown));
    // A toolchain crate's metadata is unknown whatever is handed in.
    let toolchain = CrateEnv::of(EnvFacts { package: None, ..facts(SeenRoot::Package, Some(META)) }, alloc).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(toolchain.get("CARGO_PKG_AUTHORS"), Err(EnvUnknown));
}

/// `of(facts())` answers as the original, the metadata kept apart from a script pair that
/// shadows one of its names; `clone_in` keeps it too.
#[test]
fn the_metadata_round_trips_through_facts_and_a_script_pair_shadows_it() {
    let alloc = a();
    let printed = "cargo:rustc-env=CARGO_PKG_AUTHORS=from-script\n";
    let out = blaze_build_script::Outputs::read(printed.as_bytes(), "/out", blaze_build_script::instr::OutputLimits::DEFAULT, alloc).unwrap_or_else(|e| panic!("{e:?}"));
    let mut bytes = blaze_own::AVec::new_in(alloc);
    out.encode(&mut bytes);
    let record = blaze_build_script::OutputsRef::open(bytes.leak()).unwrap_or_else(|e| panic!("{e:?}"));
    let env = CrateEnv::of(EnvFacts { build_script: true, script: Some(record), ..facts(SeenRoot::Package, Some(META)) }, alloc).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(env.get("CARGO_PKG_AUTHORS"), Ok(Some("from-script")), "the script's pair is given after cargo's");
    assert_eq!(env.facts().meta, Some(META), "the facts keep the manifest's value, not the script's");
    let again = CrateEnv::of(env.facts(), alloc).unwrap_or_else(|e| panic!("{e}"));
    let copy = env.clone_in(alloc).unwrap_or_else(|e| panic!("{e:?}"));
    for (name, _) in META.pairs() {
        assert_eq!(again.get(name), env.get(name), "{name}");
        assert_eq!(copy.get(name), env.get(name), "{name}");
    }
    assert_eq!(copy.facts().meta, Some(META));
}
