//! What `--check-cfg` rustc refuses, and how it says so: `invalid-arguments.rs`'s revisions,
//! each `.stderr` as it is.

use syntax::Edition;

use crate::{
    emit::{Sources, Style},
    lints::CheckCfg,
};

/// rustc's stderr refusing the `--check-cfg` `spec`, or `None` when it takes it.
fn refused(spec: &str) -> Option<String> {
    let mut check = CheckCfg::default();
    let diag = check.add_diag(spec, Edition::Edition2015).err()?;
    assert_eq!(check, CheckCfg::default(), "`{spec}` is refused and changed nothing");
    let style = Style { ui_testing: true, ..Style::default() };
    Some(crate::emit::written(&diag, &Sources::of(std::iter::empty()), style).unwrap())
}

fn error(spec: &str, notes: &[&str]) -> String {
    let mut out = format!("error: invalid `--check-cfg` argument: `{spec}`\n   |\n");
    for note in notes {
        out.push_str(&format!("   = note: {note}\n"));
    }
    out.push_str("   = note: visit <https://doc.rust-lang.org/nightly/rustc/check-cfg.html> for more details\n\n");
    out
}

const EXPECTED: &str = r#"expected `cfg(name, values("value1", "value2", ... "valueN"))`"#;
const CFG_ARGS: &str = "`cfg()` arguments must be simple identifiers, `any()` or `values(...)`";
const VALUES_ARGS: &str = "`values()` arguments must be string literals, `none()` or `any()`";
const BEFORE_NAMES: &str = "`values()` cannot be specified before the names";

#[test]
fn a_spec_that_is_no_cfg_is_refused() {
    for spec in ["anything_else(...)", "abc()", "cfg(...)", "cfg(", "unsafe(cfg(foo))"] {
        assert_eq!(refused(spec).as_deref(), Some(error(spec, &[EXPECTED]).as_str()), "{spec}");
    }
}

#[test]
fn values_before_names_are_refused() {
    for spec in ["cfg(values(),true)", "cfg(any(),values())", "cfg(any(),values(any()))", "cfg(values(any()),foo)"] {
        assert_eq!(refused(spec).as_deref(), Some(error(spec, &[BEFORE_NAMES]).as_str()), "{spec}");
    }
}

#[test]
fn an_argument_that_is_not_one_is_named() {
    let cases: &[(&str, &[&str])] = &[
        (r#"cfg("NOT_IDENT")"#, &[r#"`"NOT_IDENT"` is a string literal"#, CFG_ARGS]),
        (r#"cfg(foo,"NOT_IDENT",bar)"#, &[r#"`"NOT_IDENT"` is a string literal"#, CFG_ARGS]),
        ("cfg(any(foo))", &["`any(foo)` is invalid", "`any()` takes no argument"]),
        ("cfg(foo,values(any(bar)))", &["`any(bar)` is invalid", "`any()` in `values()` takes no argument"]),
        ("cfg(foo,values(any(),any()))", &["`any()` is invalid", "`any()` in `values()` cannot be specified multiple times"]),
        ("cfg(foo,values(bar))", &["`bar` is invalid", VALUES_ARGS]),
        (r#"cfg(foo,values("bar",bar,"bar"))"#, &["`bar` is invalid", VALUES_ARGS]),
        ("cfg(foo,test())", &["`test()` is invalid", CFG_ARGS]),
        ("cfg(foo,values(test()))", &["`test()` is invalid", VALUES_ARGS]),
        (r#"cfg(foo,values(none("test")))"#, &[r#"`none("test")` is invalid"#, "`none()` in `values()` takes no argument"]),
        ("cfg(none())", &["`none()` is invalid", CFG_ARGS]),
    ];
    for &(spec, notes) in cases {
        assert_eq!(refused(spec).as_deref(), Some(error(spec, notes).as_str()), "{spec}");
    }
}

#[test]
fn what_may_be_given_once_is_refused_twice() {
    let cases: &[(&str, &str)] = &[
        ("cfg(any(),any())", "`any()` cannot be specified multiple times"),
        ("cfg(foo,values(),values())", "`values()` cannot be specified multiple times"),
        (r#"cfg(foo,values("bar",any()))"#, "`values()` arguments cannot specify string literals and `any()` at the same time"),
        ("cfg(foo,any())", "`cfg(any())` can only be provided in isolation"),
    ];
    for &(spec, note) in cases {
        assert_eq!(refused(spec).as_deref(), Some(error(spec, &[note]).as_str()), "{spec}");
    }
}

#[test]
fn names_after_values_and_keywords_are_refused() {
    let spec = "cfg(foo,values(),bar)";
    assert_eq!(refused(spec).as_deref(), Some(error(spec, &["`cfg()` names cannot be after values"]).as_str()));
    let spec = "cfg(self)";
    assert_eq!(
        refused(spec).as_deref(),
        Some(error(spec, &["malformed `cfg` input, expected a valid identifier"]).as_str())
    );
}

#[test]
fn what_rustc_takes_is_taken() {
    for spec in [
        "cfg()",
        "cfg(any())",
        "cfg(foo)",
        "cfg(foo, bar)",
        "cfg(true)",
        "cfg(r#async)",
        r#"cfg(foo, values("a", "b", none()))"#,
        r#"cfg(foo, values(r"raw", "a\"b"))"#,
        "cfg(foo, values(any()))",
        "cfg(foo, values())",
        "cfg(a::b)",
    ] {
        let taken = refused(spec);
        if spec == "cfg(a::b)" {
            assert!(taken.is_some(), "a path of two segments is no name");
        } else {
            assert_eq!(taken, None, "{spec}");
        }
    }
}

#[test]
fn names_and_values_add_up_as_rustc_adds_them() {
    let mut check = CheckCfg::default();
    assert!(!check.is_given());
    check.add(r#"cfg(foo, values("a"))"#).unwrap();
    check.add(r#"cfg(foo, values("b", "a"))"#).unwrap();
    check.add("cfg(bar, values(any()))").unwrap();
    check.add(r#"cfg(bar, values("c"))"#).unwrap();
    check.add("cfg(baz)").unwrap();
    check.add(r#"cfg(baz, values(any()))"#).unwrap();
    assert!(check.is_given());
    assert!(check.exhaustive_names());
    let expecteds: Vec<String> = check.expecteds().iter().map(|(name, values)| format!("{name}: {values:?}")).collect();
    assert_eq!(expecteds, [r#"foo: Some([Some("a"), Some("b")])"#, "bar: Any", "baz: Any"]);
    check.add("cfg(any())").unwrap();
    assert!(!check.exhaustive_names());
}
