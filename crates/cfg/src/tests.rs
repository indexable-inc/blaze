use expect_test::{Expect, expect};
use hegel::TestCase;
use hegel::generators as gs;
use intern::Symbol;
use syntax::{Edition, SourceFile, view, view::AstNode as _};
use syntax_bridge::{
    DocCommentDesugarMode, call_input,
    dummy_test_span_utils::{DUMMY, DummyTestSpanMap},
};

use crate::{CfgAtom, CfgExpr, CfgOptions, DnfExpr};

/// The first predicate of `input`, read from its token tree and from its view.
/// `clean`: `input` must parse without errors.
#[track_caller]
fn cfgs(input: &str, clean: bool) -> (CfgExpr, CfgExpr) {
    let parse = SourceFile::parse(input, Edition::CURRENT);
    if clean {
        assert!(parse.errors().is_empty(), "{input}");
    }
    let pred = parse.root().descendants().find_map(view::CfgPredicate::cast).unwrap();
    let tt = call_input(pred.syntax(), DummyTestSpanMap, DUMMY, DocCommentDesugarMode::ProcMacro);
    let from_view = CfgExpr::parse_from_view(pred);
    (CfgExpr::parse(&tt), from_view)
}

#[track_caller]
fn assert_parse_result(input: &str, expected: CfgExpr) {
    let (from_tt, from_view) = cfgs(input, false);
    assert_eq!(from_tt, expected);
    assert_eq!(from_view, expected);
}

#[track_caller]
fn check_dnf(input: &str, expect: Expect) {
    let (from_tt, from_view) = cfgs(input, true);
    for cfg in [from_tt, from_view] {
        let actual = format!("#![cfg({})]", DnfExpr::new(&cfg));
        expect.assert_eq(&actual);
    }
}

#[track_caller]
fn check_why_inactive(input: &str, opts: &CfgOptions, expect: Expect) {
    let (from_tt, from_view) = cfgs(input, true);
    for cfg in [from_tt, from_view] {
        let dnf = DnfExpr::new(&cfg);
        let why_inactive = dnf.why_inactive(opts).unwrap().to_string();
        expect.assert_eq(&why_inactive);
    }
}

#[track_caller]
fn check_enable_hints(input: &str, opts: &CfgOptions, expected_hints: &[&str]) {
    let (from_tt, from_view) = cfgs(input, true);
    for cfg in [from_tt, from_view] {
        let dnf = DnfExpr::new(&cfg);
        let hints = dnf.compute_enable_hints(opts).map(|diff| diff.to_string()).collect::<Vec<_>>();
        assert_eq!(hints, expected_hints);
    }
}

#[test]
fn test_cfg_expr_parser() {
    assert_parse_result("#![cfg(foo)]", CfgAtom::Flag(Symbol::intern("foo")).into());
    assert_parse_result("#![cfg(foo,)]", CfgAtom::Flag(Symbol::intern("foo")).into());
    assert_parse_result(
        "#![cfg(not(foo))]",
        CfgExpr::Not(Box::new(CfgAtom::Flag(Symbol::intern("foo")).into())),
    );
    assert_parse_result("#![cfg(foo(bar))]", CfgExpr::Invalid);

    // Only take the first
    assert_parse_result(
        r#"#![cfg(foo, bar = "baz")]"#,
        CfgAtom::Flag(Symbol::intern("foo")).into(),
    );

    assert_parse_result(
        r#"#![cfg(all(foo, bar = "baz"))]"#,
        CfgExpr::All(
            vec![
                CfgAtom::Flag(Symbol::intern("foo")).into(),
                CfgAtom::KeyValue { key: Symbol::intern("bar"), value: Symbol::intern("baz") }
                    .into(),
            ]
            .into_boxed_slice(),
        ),
    );
}

#[test]
fn smoke() {
    check_dnf("#![cfg(test)]", expect![[r#"#![cfg(test)]"#]]);
    check_dnf("#![cfg(not(test))]", expect![[r#"#![cfg(not(test))]"#]]);
    check_dnf("#![cfg(not(not(test)))]", expect![[r#"#![cfg(test)]"#]]);

    check_dnf("#![cfg(all(a, b))]", expect![[r#"#![cfg(all(a, b))]"#]]);
    check_dnf("#![cfg(any(a, b))]", expect![[r#"#![cfg(any(a, b))]"#]]);

    check_dnf("#![cfg(not(a))]", expect![[r#"#![cfg(not(a))]"#]]);
}

#[test]
fn distribute() {
    check_dnf("#![cfg(all(any(a, b), c))]", expect![[r#"#![cfg(any(all(a, c), all(b, c)))]"#]]);
    check_dnf("#![cfg(all(c, any(a, b)))]", expect![[r#"#![cfg(any(all(c, a), all(c, b)))]"#]]);
    check_dnf(
        "#![cfg(all(any(a, b), any(c, d)))]",
        expect![[r#"#![cfg(any(all(a, c), all(a, d), all(b, c), all(b, d)))]"#]],
    );

    check_dnf(
        "#![cfg(all(any(a, b, c), any(d, e, f), g))]",
        expect![[
            r#"#![cfg(any(all(a, d, g), all(a, e, g), all(a, f, g), all(b, d, g), all(b, e, g), all(b, f, g), all(c, d, g), all(c, e, g), all(c, f, g)))]"#
        ]],
    );
}

#[test]
fn demorgan() {
    check_dnf("#![cfg(not(all(a, b)))]", expect![[r#"#![cfg(any(not(a), not(b)))]"#]]);
    check_dnf("#![cfg(not(any(a, b)))]", expect![[r#"#![cfg(all(not(a), not(b)))]"#]]);

    check_dnf("#![cfg(not(all(not(a), b)))]", expect![[r#"#![cfg(any(a, not(b)))]"#]]);
    check_dnf("#![cfg(not(any(a, not(b))))]", expect![[r#"#![cfg(all(not(a), b))]"#]]);
}

#[test]
fn nested() {
    check_dnf("#![cfg(all(any(a), not(all(any(b)))))]", expect![[r#"#![cfg(all(a, not(b)))]"#]]);

    check_dnf("#![cfg(any(any(a, b)))]", expect![[r#"#![cfg(any(a, b))]"#]]);
    check_dnf("#![cfg(not(any(any(a, b))))]", expect![[r#"#![cfg(all(not(a), not(b)))]"#]]);
    check_dnf("#![cfg(all(all(a, b)))]", expect![[r#"#![cfg(all(a, b))]"#]]);
    check_dnf("#![cfg(not(all(all(a, b))))]", expect![[r#"#![cfg(any(not(a), not(b)))]"#]]);
}

#[test]
fn regression() {
    check_dnf("#![cfg(all(not(not(any(any(any()))))))]", expect![[r##"#![cfg(any())]"##]]);
    check_dnf("#![cfg(all(any(all(any()))))]", expect![[r##"#![cfg(any())]"##]]);
    check_dnf("#![cfg(all(all(any())))]", expect![[r##"#![cfg(any())]"##]]);

    check_dnf("#![cfg(all(all(any(), x)))]", expect![[r##"#![cfg(any())]"##]]);
    check_dnf("#![cfg(all(all(any()), x))]", expect![[r##"#![cfg(any())]"##]]);
    check_dnf("#![cfg(all(all(any(x))))]", expect![[r##"#![cfg(x)]"##]]);
    check_dnf("#![cfg(all(all(any(x), x)))]", expect![[r##"#![cfg(all(x, x))]"##]]);
}

#[test]
fn hints() {
    let mut opts = CfgOptions::default();

    check_enable_hints("#![cfg(test)]", &opts, &["enable test"]);
    check_enable_hints("#![cfg(not(test))]", &opts, &[]);

    check_enable_hints("#![cfg(any(a, b))]", &opts, &["enable a", "enable b"]);
    check_enable_hints("#![cfg(any(b, a))]", &opts, &["enable b", "enable a"]);

    check_enable_hints("#![cfg(all(a, b))]", &opts, &["enable a and b"]);

    opts.insert_atom(Symbol::intern("test"));

    check_enable_hints("#![cfg(test)]", &opts, &[]);
    check_enable_hints("#![cfg(not(test))]", &opts, &["disable test"]);

    opts.insert_atom(Symbol::intern("a"));
    opts.insert_atom(Symbol::intern("b"));
    check_enable_hints("#![cfg(all(not(a), not(b)))]", &opts, &["disable a and b"]);
}

/// Tests that we don't suggest hints for cfgs that express an inconsistent formula.
#[test]
fn hints_impossible() {
    let mut opts = CfgOptions::default();

    check_enable_hints("#![cfg(all(test, not(test)))]", &opts, &[]);

    opts.insert_atom(Symbol::intern("test"));

    check_enable_hints("#![cfg(all(test, not(test)))]", &opts, &[]);
}

#[test]
fn why_inactive() {
    let mut opts = CfgOptions::default();
    opts.insert_atom(Symbol::intern("test"));
    opts.insert_atom(Symbol::intern("test2"));

    check_why_inactive("#![cfg(a)]", &opts, expect![["a is disabled"]]);
    check_why_inactive("#![cfg(not(test))]", &opts, expect![["test is enabled"]]);

    check_why_inactive(
        "#![cfg(all(not(test), not(test2)))]",
        &opts,
        expect![["test and test2 are enabled"]],
    );
    check_why_inactive("#![cfg(all(a, b))]", &opts, expect![["a and b are disabled"]]);
    check_why_inactive(
        "#![cfg(all(not(test), a))]",
        &opts,
        expect![["test is enabled and a is disabled"]],
    );
    check_why_inactive(
        "#![cfg(all(not(test), test2, a))]",
        &opts,
        expect![["test is enabled and a is disabled"]],
    );
    check_why_inactive(
        "#![cfg(all(not(test), not(test2), a))]",
        &opts,
        expect![["test and test2 are enabled and a is disabled"]],
    );
}

/// `fold` answers each variant: an atom by the query, `all`/`any`/`not` by their predicates, and
/// `Invalid` (or any predicate under it) with no answer. `check` folds with the enabled set.
#[test]
fn fold_answers_every_variant() {
    let mut opts = CfgOptions::default();
    opts.insert_atom(Symbol::intern("on"));
    let cases = [
        ("#![cfg(on)]", Some(true)),
        ("#![cfg(off)]", Some(false)),
        ("#![cfg(all(on, off))]", Some(false)),
        ("#![cfg(all(on, on))]", Some(true)),
        ("#![cfg(any(off, on))]", Some(true)),
        ("#![cfg(any(off, off))]", Some(false)),
        ("#![cfg(not(off))]", Some(true)),
        ("#![cfg(all())]", Some(true)),
        ("#![cfg(any())]", Some(false)),
    ];
    for (input, expected) in cases {
        let (from_tt, from_view) = cfgs(input, true);
        for cfg in [from_tt, from_view] {
            assert_eq!(opts.check(&cfg), expected, "{input}");
            assert_eq!(cfg.fold(&|atom| opts.check_atom(atom)), expected, "{input}");
        }
    }
    assert_eq!(CfgExpr::Invalid.fold(&|_| true), None);
    let under_not = CfgExpr::Not(Box::new(CfgExpr::Invalid));
    assert_eq!(under_not.fold(&|_| true), None);
    let on: CfgExpr = CfgAtom::Flag(Symbol::intern("on")).into();
    let under_all = CfgExpr::All(vec![on, CfgExpr::Invalid].into());
    assert_eq!(under_all.fold(&|_| true), None);
}

/// A name from few letters, so atoms repeat across an expression.
fn name(tc: &TestCase) -> Symbol {
    Symbol::intern(&tc.draw(gs::text().alphabet("abc").max_size(3)))
}

fn cfg_expr(tc: &TestCase, depth: u32) -> CfgExpr {
    let pick: u8 = tc.draw(gs::integers().max_value(if depth == 0 { 1 } else { 4 }));
    let exprs = || -> Box<[CfgExpr]> {
        let len: usize = tc.draw(gs::integers().max_value(4));
        (0..len).map(|_| cfg_expr(tc, depth - 1)).collect()
    };
    match pick {
        0 => CfgExpr::Invalid,
        1 if tc.draw(gs::booleans()) => CfgAtom::Flag(name(tc)).into(),
        1 => CfgAtom::KeyValue { key: name(tc), value: name(tc) }.into(),
        2 => CfgExpr::All(exprs()),
        3 => CfgExpr::Any(exprs()),
        _ => CfgExpr::Not(Box::new(cfg_expr(tc, depth - 1))),
    }
}

#[hegel::test(test_cases = 512)]
fn any_expr_has_a_dnf(tc: TestCase) {
    DnfExpr::new(&cfg_expr(&tc, 5));
}
