//! What [`super::merge`] says of observations made by hand: an item dead in every
//! configuration, one only the tests use, one only a feature uses, the predicate a set of
//! platforms gets (or none), a finding said by its owner's, and when a fix is sound.

use super::{Atom, Inactive, Item, Key, Levels, Lint, Lvl, Observation, Predicate, merge, narrowest};
use crate::model::{Applicability, Span};

const FILE: &str = "src/lib.rs";

fn warn() -> Levels {
    Levels { dead_code: Lvl::Warn, dead: Lvl::Warn, test_only: Lvl::Warn, cfg_only: Lvl::Warn }
}

fn span(lo: u32, hi: u32) -> Span {
    Span { file: FILE.to_owned(), lo, hi, expansion: None }
}

/// A free function `name` written at line `line`, used or not, as `dead_code` sees it.
fn item(descr: &str, name: &str, line: u32, used: bool) -> Item {
    let lo = line * 100;
    Item {
        key: Some(Key {
            file: FILE.to_owned(),
            lo: lo + 3,
            hi: lo + 3 + name.len() as u32,
            in_expansion: false,
            kind: descr.to_owned(),
            name: name.to_owned(),
            nth: 0,
        }),
        span: Some(span(lo + 3, lo + 3 + name.len() as u32)),
        insert: Some((span(lo, lo), String::new())),
        descr: descr.to_owned(),
        name: name.to_owned(),
        used,
        reported: !used,
        levels: (!used).then(warn),
        parent: None,
        owner: None,
        companions: Vec::new(),
        fixable: true,
    }
}

fn config(label: &str, atoms: &[Atom], items: Vec<Item>) -> Observation {
    let mut atoms = atoms.to_vec();
    atoms.sort();
    Observation {
        label: label.to_owned(),
        unit: 0,
        atoms,
        files: vec![FILE.to_owned()],
        items,
        ..Observation::default()
    }
}

fn test() -> Atom {
    Atom::flag("test")
}

#[test]
fn an_item_no_configuration_uses_is_dead_with_no_fix() {
    let lib = config("lib", &[], vec![item("function", "helper", 1, false)]);
    let tests = config("lib (test)", &[test()], vec![item("function", "helper", 1, false)]);
    let found = merge(&[lib, tests]);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].lint, Lint::Dead);
    assert!(found[0].fix.is_empty());
    assert!(found[0].tests_checked);
    assert_eq!(found[0].diag().message, "function `helper` is never used");
}

#[test]
fn an_item_dead_with_no_test_build_checked_says_to_check_the_tests() {
    let found = merge(&[config("lib", &[], vec![item("function", "helper", 1, false)])]);
    assert_eq!(found[0].lint, Lint::Dead);
    assert!(!found[0].tests_checked);
    let diag = found[0].diag();
    assert!(diag.children.iter().any(|it| it.message.contains("--all-targets")), "{diag:?}");
}

#[test]
fn an_item_only_the_tests_use_gets_cfg_test() {
    let lib = config("lib", &[], vec![item("function", "helper", 1, false)]);
    let tests = config("lib (test)", &[test()], vec![item("function", "helper", 1, true)]);
    let found = merge(&[lib, tests]);
    assert_eq!(found.len(), 1);
    let it = &found[0];
    assert_eq!(it.lint, Lint::TestOnly);
    assert_eq!(it.predicate, Some(Predicate::Atom(test())));
    assert_eq!(it.used_in, ["lib (test)"]);
    assert_eq!(it.unused_in, ["lib"]);
    assert_eq!(it.fix, [(span(100, 100), "#[cfg(test)]\n".to_owned())]);
    assert_eq!(it.applicability, Applicability::MachineApplicable);
    assert_eq!(it.diag().message, "function `helper` is only used by tests");
}

#[test]
fn an_item_used_everywhere_is_said_nothing_of() {
    let lib = config("lib", &[], vec![item("function", "used", 1, true)]);
    let tests = config("lib (test)", &[test()], vec![item("function", "used", 1, true)]);
    assert!(merge(&[lib, tests]).is_empty());
}

#[test]
fn an_item_only_a_feature_uses_gets_cfg_feature() {
    let with = config("lib +a", &[Atom::key_value("feature", "a")], vec![item("function", "gated", 2, true)]);
    let without = config("lib", &[], vec![item("function", "gated", 2, false)]);
    let found = merge(&[with, without]);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].lint, Lint::CfgOnly);
    assert_eq!(found[0].predicate, Some(Predicate::Atom(Atom::key_value("feature", "a"))));
    assert_eq!(found[0].fix[0].1, "#[cfg(feature = \"a\")]\n");
    assert_eq!(found[0].diag().message, "function `gated` is only used when `feature = \"a\"`");
}

fn platform(os: &str, family: &str) -> Observation {
    config(os, &[Atom::key_value("target_os", os), Atom::flag(family), Atom::key_value("target_family", family)], Vec::new())
}

#[test]
fn the_narrowest_platform_predicate_is_found() {
    let (linux, macos, windows) = (platform("linux", "unix"), platform("macos", "unix"), platform("windows", "windows"));
    let all = [&linux, &macos, &windows];
    // Both unixes: one atom says it; `target_family = "unix"` and `unix` rank the same, and
    // the first by order is taken.
    let unix = narrowest(&all, &[true, true, false]).expect("a predicate");
    assert_eq!(unix.to_string(), "target_family = \"unix\"");
    assert_eq!(narrowest(&all, &[true, false, false]), Some(Predicate::Atom(Atom::key_value("target_os", "linux"))));
    // Two values of one key, where no one atom is exact.
    let four = platform("freebsd", "unix");
    let any = narrowest(&[&linux, &macos, &four, &windows], &[true, true, false, false]).expect("a predicate");
    assert_eq!(any.to_string(), "any(target_os = \"linux\", target_os = \"macos\")");
    assert_eq!(narrowest(&all, &[false, true, true]), Some(Predicate::Not(Atom::key_value("target_os", "linux"))));
}

#[test]
fn no_predicate_when_no_simple_one_is_exact() {
    let a = config("a", &[Atom::flag("x")], Vec::new());
    let b = config("b", &[Atom::flag("x"), Atom::flag("y")], Vec::new());
    let c = config("c", &[Atom::flag("y")], Vec::new());
    assert_eq!(narrowest(&[&a, &b, &c], &[true, false, true]), None);
}

#[test]
fn a_method_said_by_its_type_is_left_out() {
    let make = |used: bool| {
        let s = item("struct", "S", 1, used);
        let mut m = item("method", "m", 3, used);
        m.owner = Some(0);
        vec![s, m]
    };
    let found = merge(&[config("lib", &[], make(false)), config("lib (test)", &[test()], make(true))]);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].name, "S");
}

#[test]
fn a_fix_named_by_code_that_stays_is_not_machine_applicable() {
    // `helper` only the tests use; `caller`, used nowhere, names it, and stays in the lib.
    let mut lib = config("lib", &[], vec![item("function", "helper", 1, false), item("function", "caller", 2, false)]);
    lib.refs = vec![(1, 0)];
    let tests = config("lib (test)", &[test()], vec![item("function", "helper", 1, true), item("function", "caller", 2, false)]);
    let found = merge(&[lib, tests]);
    let helper = found.iter().find(|it| it.name == "helper").expect("a finding of helper");
    assert_eq!(helper.lint, Lint::TestOnly);
    assert_eq!(helper.applicability, Applicability::MaybeIncorrect);
    assert!(helper.unsure.as_deref().is_some_and(|it| it.contains("`caller`")), "{helper:?}");
}

#[test]
fn a_fix_named_only_by_code_it_also_leaves_out_stays_machine_applicable() {
    // `helper` and `caller` only the tests use, and `caller` names `helper`: both go.
    let mut lib = config("lib", &[], vec![item("function", "helper", 1, false), item("function", "caller", 2, false)]);
    lib.refs = vec![(1, 0)];
    let tests = config("lib (test)", &[test()], vec![item("function", "helper", 1, true), item("function", "caller", 2, true)]);
    let found = merge(&[lib, tests]);
    assert_eq!(found.len(), 2);
    assert!(found.iter().all(|it| it.applicability == Applicability::MachineApplicable), "{found:?}");
}

#[test]
fn a_fix_named_by_code_no_configuration_compiles_is_unsure() {
    let off = Inactive {
        file: FILE.to_owned(),
        lo: 900,
        hi: 950,
        predicate: "windows".to_owned(),
        names: vec!["helper".to_owned()],
    };
    let mut lib = config("lib", &[], vec![item("function", "helper", 1, false)]);
    lib.inactive = vec![off.clone()];
    let mut tests = config("lib (test)", &[test()], vec![item("function", "helper", 1, true)]);
    tests.inactive = vec![off];
    let found = merge(&[lib, tests]);
    assert_eq!(found[0].applicability, Applicability::MaybeIncorrect);
    assert!(found[0].unsure.as_deref().is_some_and(|it| it.contains("windows")));
}

#[test]
fn a_type_and_its_impls_get_the_cfg_together() {
    let make = |used: bool| {
        let mut s = item("struct", "S", 1, used);
        s.companions = vec![1];
        let mut imp = item("implementation", "", 5, used);
        imp.key = None;
        imp.reported = false;
        imp.levels = None;
        vec![s, imp]
    };
    let found = merge(&[config("lib", &[], make(false)), config("lib (test)", &[test()], make(true))]);
    assert_eq!(found.len(), 1);
    let at: Vec<u32> = found[0].fix.iter().map(|it| it.0.lo).collect();
    assert_eq!(at, [100, 500]);
}

#[test]
fn an_item_dead_code_is_allowed_at_is_left_alone() {
    let mut helper = item("function", "helper", 1, false);
    helper.levels = Some(Levels { dead_code: Lvl::Allow, ..warn() });
    assert!(merge(&[config("lib", &[], vec![helper])]).is_empty());
}

#[test]
fn a_denied_lint_is_an_error() {
    let mut helper = item("function", "helper", 1, false);
    helper.levels = Some(Levels { test_only: Lvl::Deny, ..warn() });
    let found = merge(&[config("lib", &[], vec![helper]), config("lib (test)", &[test()], vec![item("function", "helper", 1, true)])]);
    assert_eq!(found[0].level, Lvl::Deny);
    assert!(found[0].diag().is_error());
}

#[test]
fn a_configuration_whose_liveness_is_unknown_gives_nothing() {
    let mut lib = config("lib", &[], vec![item("function", "helper", 1, false)]);
    lib.unported = Some("a `global_asm!` names an item".to_owned());
    assert!(merge(&[lib]).is_empty());
}
