//! tempo's `tempo-primitives` and `reth-codecs` in miniature: a member with an optional normal
//! dependency and a dev-dependency on one package, a weak feature on it, two members asking
//! different features of one package, and a build-dependency beside a dev-dependency. Each
//! expectation is derived by hand from the rule it names (lib.rs, "The rules"), never from a
//! stored cargo output (project rule 7).
//!
//! The shape (tempo `crates/primitives/Cargo.toml`, `[workspace.dependencies] reth-codecs = {
//! default-features = false, features = ["account-ext"] }`): `prim` has
//! `codecs = { workspace = true, optional = true }` under `[dependencies]` and
//! `codecs = { workspace = true, features = ["test-utils"] }` under `[dev-dependencies]`; the
//! layout hands both entries inherited (rule 11): no defaults, `account-ext`, and the dev entry's
//! own `test-utils` added. `prim`'s `default = ["codec"]`, `codec = ["dep:codecs",
//! "codecs/alloy"]`, `test-utils = ["codecs?/test-utils"]`. `codecs` has `default = ["std"]`,
//! `std`, `alloy`, `account-ext` and `test-utils = ["std", "alloy"]`, and (in the real crate)
//! an item a dependent's test reaches only where `feature = "test-utils"` holds on `codecs`.

use std::borrow::ToOwned;
use std::string::ToString;
use std::vec;

use super::{Opts, P, a, dep, features_of, lines, ok, opts, pkg, solve, solve_by};
use crate::input::{CfgAtom, Resolver};
use crate::{Plant, UnitGraph, UnitKind};

fn world() -> [P; 2] {
    [
        pkg("prim")
            .member()
            .feature("default", &["codec"])
            .feature("codec", &["dep:codecs", "codecs/alloy"])
            .feature("test-utils", &["codecs?/test-utils"])
            .dep(dep("codecs").optional().no_default().features(&["account-ext"]))
            .dep(dep("codecs").dev().no_default().features(&["account-ext", "test-utils"])),
        pkg("codecs")
            .feature("default", &["std"])
            .feature("std", &[])
            .feature("alloy", &[])
            .feature("account-ext", &[])
            .feature("test-utils", &["std", "alloy"]),
    ]
}

fn graph(world: &[P], o: &Opts) -> UnitGraph<'static> {
    match solve(world, o) {
        Ok(g) => g,
        Err(e) => panic!("{e}"),
    }
}

/// Whether the unit of package `name` and `kind` has the cfg atom `atom`.
fn has_cfg(graph: &UnitGraph<'_>, name: &str, kind: &UnitKind<'_>, atom: &CfgAtom<'_>) -> bool {
    let Some(unit) = graph.units.iter().find(|u| u.name.as_str() == name && u.kind == *kind) else {
        panic!("no `{name}` {kind:?} unit in {:#?}", lines(graph))
    };
    unit.cfg.iter().any(|c| c == atom)
}

fn with(tests: bool, no_default_features: bool, features: &[&str], plant: Plant) -> Opts {
    Opts { tests, no_default_features, features: features.iter().map(|f| (*f).to_owned()).collect(), plant, ..opts(Resolver::V2) }
}

/// Rule 6 (resolver 2, "Feature resolver version 2": "Dev-dependencies do not activate features
/// unless building a target that needs them"), rule 8 and rule 11.
///
/// Without tests: the dev entry takes no part. `default` -> `codec` -> `dep:codecs` (the
/// optional entry on: no defaults, `account-ext`) and `codecs/alloy`: `codecs` is
/// `[account-ext, alloy]`; `prim`'s library links it.
///
/// With tests (`blaze check`, `cargo check --workspace --all-targets`): the dev entry counts for
/// the whole build: `account-ext` and `test-utils` -> `std`, `alloy`. One `codecs` unit,
/// `[account-ext, alloy, std, test-utils]`, read by the library and by its test harness alike;
/// the harness has `test` and `feature = "codec"` in its cfg, so tempo's
/// `cfg_attr(all(test, feature = "reth-codec"), reth_codecs::add_arbitrary_tests(..))` holds in
/// that unit and reads `codecs`' names under `feature = "test-utils"`.
///
/// Prints nothing; fails naming the lines.
#[test]
fn a_test_unit_sees_the_dev_dependencys_features_and_the_library_build_does_not() {
    let w = world();
    assert_eq!(
        ok(&w, &with(false, false, &[], Plant::None)),
        vec!["codecs 1.0.0 lib target [account-ext,alloy] -> ".to_string(), "prim 1.0.0 lib target [codec,default] -> codecs=codecs".to_string()]
    );
    let tests = with(true, false, &[], Plant::None);
    let want = vec![
        "codecs 1.0.0 lib target [account-ext,alloy,std,test-utils] -> ".to_string(),
        "prim 1.0.0 lib target [codec,default] -> codecs=codecs".to_string(),
        "prim 1.0.0 test target [codec,default] -> codecs=codecs".to_string(),
    ];
    assert_eq!(ok(&w, &tests), want);
    // The fixpoint is the same whichever order the work is taken in.
    let first = match solve_by(&w, &tests, &mut |_| 0) {
        Ok(g) => lines(&g),
        Err(e) => panic!("{e}"),
    };
    assert_eq!(first, want, "the order of the work changed the graph");
    let g = graph(&w, &tests);
    let test = UnitKind::Test { bin: None };
    assert!(has_cfg(&g, "prim", &test, &CfgAtom::flag("test", a())), "the harness has `test`: {:#?}", lines(&g));
    assert!(has_cfg(&g, "prim", &test, &CfgAtom::pair("feature", "codec", a())), "the harness has `feature = \"codec\"`: {:#?}", lines(&g));
    assert!(!has_cfg(&g, "prim", &UnitKind::Lib, &CfgAtom::flag("test", a())), "the library has no `test`: {:#?}", lines(&g));
    assert!(has_cfg(&g, "codecs", &UnitKind::Lib, &CfgAtom::pair("feature", "test-utils", a())), "`codecs` has `feature = \"test-utils\"`: {:#?}", lines(&g));
}

/// The optional normal entry off (`--no-default-features`: no `codec`): without tests `prim`'s
/// library has no `codecs` at all; with tests the harness links `codecs` through the dev entry
/// (not optional), with `test-utils`, and the library still does not (its only entry is the
/// optional one, off): two units of `prim` with different dependencies.
///
/// Prints nothing; fails naming the lines.
#[test]
fn the_optional_entry_off_leaves_the_library_without_it_and_the_harness_with_it() {
    let w = world();
    assert_eq!(ok(&w, &with(false, true, &[], Plant::None)), vec!["prim 1.0.0 lib target [] -> ".to_string()]);
    assert_eq!(
        ok(&w, &with(true, true, &[], Plant::None)),
        vec![
            "codecs 1.0.0 lib target [account-ext,alloy,std,test-utils] -> ".to_string(),
            "prim 1.0.0 lib target [] -> ".to_string(),
            "prim 1.0.0 test target [] -> codecs=codecs".to_string(),
        ]
    );
}

/// Rule 1: `codecs?/test-utils` turns nothing on alone (`codecs` off: `--no-default-features
/// --features test-utils`, no tests: no `codecs` unit), and turns `test-utils` on once `codecs`
/// is on for another reason (`default` -> `codec` -> `dep:codecs`), whichever of the two values
/// the fixpoint takes first.
///
/// Prints nothing; fails naming the lines.
#[test]
fn a_weak_feature_waits_for_dep_colon() {
    let w = world();
    assert_eq!(ok(&w, &with(false, true, &["test-utils"], Plant::None)), vec!["prim 1.0.0 lib target [test-utils] -> ".to_string()]);
    let on = with(false, false, &["test-utils"], Plant::None);
    let want = vec![
        "codecs 1.0.0 lib target [account-ext,alloy,std,test-utils] -> ".to_string(),
        "prim 1.0.0 lib target [codec,default,test-utils] -> codecs=codecs".to_string(),
    ];
    assert_eq!(ok(&w, &on), want);
    let first = match solve_by(&w, &on, &mut |_| 0) {
        Ok(g) => lines(&g),
        Err(e) => panic!("{e}"),
    };
    assert_eq!(first, want, "the order of the work changed the graph");
}

/// CONTROL (rule 23): with dev-dependency unification planted off, the harness still links
/// `codecs` but with the library build's features only: no `test-utils` in its cfg, so the
/// miniature's item behind `feature = "test-utils"` is not in the names the harness reads, and
/// tempo's attribute would be unresolved there. With the plant gone (the test above) it is.
///
/// Prints nothing; fails naming the lines.
#[test]
fn control_without_dev_unification_the_harness_loses_the_feature() {
    let w = world();
    let planted = with(true, false, &[], Plant::DevIgnored);
    assert_eq!(
        ok(&w, &planted),
        vec![
            "codecs 1.0.0 lib target [account-ext,alloy] -> ".to_string(),
            "prim 1.0.0 lib target [codec,default] -> codecs=codecs".to_string(),
            "prim 1.0.0 test target [codec,default] -> codecs=codecs".to_string(),
        ]
    );
    let g = graph(&w, &planted);
    assert!(!has_cfg(&g, "codecs", &UnitKind::Lib, &CfgAtom::pair("feature", "test-utils", a())), "the plant took no effect: {:#?}", lines(&g));
    let fixed = graph(&w, &with(true, false, &[], Plant::None));
    assert!(has_cfg(&fixed, "codecs", &UnitKind::Lib, &CfgAtom::pair("feature", "test-utils", a())), "unplanted, `test-utils` is on: {:#?}", lines(&fixed));
}

/// Rule 5: two members asking different features of one package unify in one workspace build:
/// one `s` unit with `a` and `b`, read by both.
///
/// Prints nothing; fails naming the lines.
#[test]
fn two_members_unify_one_dependency() {
    let w = [
        pkg("m1").member().dep(dep("s").features(&["a"])),
        pkg("m2").member().dep(dep("s").features(&["b"])),
        pkg("s").feature("a", &[]).feature("b", &[]).feature("c", &[]),
    ];
    assert_eq!(
        ok(&w, &opts(Resolver::V2)),
        vec!["m1 1.0.0 lib target [] -> s=s".to_string(), "m2 1.0.0 lib target [] -> s=s".to_string(), "s 1.0.0 lib target [a,b] -> ".to_string()]
    );
}

/// Rule 6 and 12 (resolver 2): a build-dependency's features stay on the host's `s` and a
/// dev-dependency's on the target's: `app` has `s` normal, `s` as a build-dependency with
/// `host-only` and `s` as a dev-dependency with `dev`. With tests: the target's `s` is `[dev]`
/// (normal and dev entries), the host's `[host-only]` (the build entry alone); the build script
/// is one unit, shared by the library and the harness.
///
/// Prints nothing; fails naming the lines.
#[test]
fn build_and_dev_dependency_features_do_not_cross() {
    let w = [
        pkg("app").member().build().dep(dep("s")).dep(dep("s").build().features(&["host-only"])).dep(dep("s").dev().features(&["dev"])),
        pkg("s").feature("host-only", &[]).feature("dev", &[]),
    ];
    assert_eq!(
        ok(&w, &Opts { tests: true, ..opts(Resolver::V2) }),
        vec![
            "app 1.0.0 lib target [] -> s=s".to_string(),
            "app 1.0.0 build host [] -> s=s".to_string(),
            "app 1.0.0 test target [] -> s=s".to_string(),
            "s 1.0.0 lib target [dev] -> ".to_string(),
            "s 1.0.0 lib host [host-only] -> ".to_string(),
        ]
    );
}

/// Rule 7 for a dev-dependency: `[target.'cfg(windows)'.dev-dependencies] s = { features =
/// ["win"] }` takes no part on the Mac, tests or not; the harness reads the normal `s`.
///
/// Prints nothing; fails naming the lines.
#[test]
fn a_dev_dependency_for_another_platform_takes_no_part() {
    let w = [pkg("app").member().dep(dep("s")).dep(dep("s").dev().on("cfg(windows)").features(&["win"])), pkg("s").feature("win", &[])];
    assert_eq!(
        features_of(&ok(&w, &Opts { tests: true, ..opts(Resolver::V2) }), "s "),
        vec!["s 1.0.0 lib target [] -> ".to_string()]
    );
}
