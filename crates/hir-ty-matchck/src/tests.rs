//! Fixture tests, each from a case of rustc's `tests/ui/pattern/usefulness/` (or the directory
//! named) with what its `.stderr` gives: the source of the node each finding is at, and the
//! finding, in the order the check gives them, bodies in source order.

mod db;
mod ui;

use base_db::{Db, all_crates};
use hir_def::{
    DefWithBodyId, ModuleDefId,
    expr_store::Body,
    hir::ExprOrPatId,
    nameres::crate_def_map,
};
use hir_expand::fixture;

use crate::{
    BindingOrigin, IrrefutableLet, LetAt, MatchError, MatchLint, PatternTypo, UnreachablePattern,
    match_check,
};


fn render_lint(lint: &MatchLint) -> (ExprOrPatId, String) {
    match lint {
        MatchLint::UnreachablePattern(UnreachablePattern {
            pat, covered_by, catchall, typo, ..
        }) => {
            let mut text = String::from("unreachable_patterns");
            if covered_by.is_empty() {
                text.push_str(": matches no values");
            }
            if *catchall {
                text.push_str(": catchall");
            }
            match typo {
                Some(PatternTypo::WantedConstant { is_typo, const_path, .. }) => {
                    text.push_str(&format!(", wanted {const_path} typo={is_typo}"))
                }
                Some(PatternTypo::PatternLetBinding(_)) => text.push_str(", let binding"),
                Some(PatternTypo::AccessibleConstant(_)) => text.push_str(", accessible"),
                Some(PatternTypo::InaccessibleConstant(_)) => text.push_str(", inaccessible"),
                None => {}
            }
            ((*pat).into(), text)
        }
        MatchLint::IrrefutableLet(IrrefutableLet { source, at, replacement }) => {
            let at = match *at {
                LetAt::Expr(expr) => expr.into(),
                LetAt::Stmt(pat) => pat.into(),
            };
            let mut text = format!("irrefutable_let_patterns: {source:?}");
            if replacement.is_some() {
                text.push_str(", replace with Some");
            }
            (at, text)
        }
        MatchLint::BindingWithVariantName(lint) => {
            let text = if lint.suggest {
                "bindings_with_variant_name: suggest"
            } else {
                "bindings_with_variant_name"
            };
            (lint.pat.into(), text.to_owned())
        }
        MatchLint::OverlappingRangeEndpoints(lint) => {
            (lint.pat.into(), format!("overlapping_range_endpoints: {}", lint.overlap))
        }
        MatchLint::NonContiguousRangeEndpoints(lint) => (
            lint.pat.into(),
            format!(
                "non_contiguous_range_endpoints: {} ({}, {} after)",
                lint.gap,
                lint.suggestion,
                lint.gapped_with.len()
            ),
        ),
    }
}

fn render_error(error: &MatchError) -> (ExprOrPatId, String) {
    match error {
        MatchError::MissingMatchArms { scrutinee, uncovered, .. } => {
            ((*scrutinee).into(), format!("E0004: {}", uncovered.witnesses.join(" | ")))
        }
        MatchError::NonExhaustiveLet { pat, origin, uncovered, .. } => {
            let origin = match origin {
                BindingOrigin::Let | BindingOrigin::Assignment => "local binding",
                BindingOrigin::FnParam => "function argument",
                BindingOrigin::ClosureParam => "closure argument",
                BindingOrigin::ForLoop => "`for` loop binding",
            };
            ((*pat).into(), format!("E0005 {origin}: {}", uncovered.witnesses.join(" | ")))
        }
        MatchError::EmptyRange { pat, inclusive } => {
            let code = if *inclusive { "E0030" } else { "E0579" };
            ((*pat).into(), code.to_owned())
        }
        MatchError::ComplexityLimit { at } => (*at, "complexity limit".to_owned()),
        MatchError::MixedDerefPatterns { deref_pat, .. } => {
            ((*deref_pat).into(), "mixed deref patterns".to_owned())
        }
        MatchError::ConstPattern { pat, error } => {
            ((*pat).into(), format!("const pattern: {error:?}"))
        }
    }
}

/// What the check finds in each function of the fixture's one file: the source of the node,
/// and the finding.
#[track_caller]
fn check(#[rust_analyzer::rust_fixture] fixture: &str, expected: &[(&str, &str)]) {
    let fx = fixture::db(fixture);
    let file = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let hir_db: &Db = &db;
        let text = db.file_text(file.file_id(&db)).text(&db);
        let mut bodies = Vec::new();
        for &krate in all_crates(&db).iter() {
            for (_, module) in crate_def_map(&db, krate).modules(&db) {
                for def in module.scope(&db).declarations() {
                    if let ModuleDefId::FunctionId(it) = def {
                        bodies.push(DefWithBodyId::from(it));
                    }
                }
            }
        }
        let mut found = Vec::new();
        for def in bodies {
            let (body, source_map) = Body::with_source_map(&db, def);
            let Ok(root) = source_map.expr_syntax(body.root_expr()) else { continue };
            let root = root.map(|it| it.text_range()).original_node_file_range_rooted(&db);
            if root.file_id != file {
                continue;
            }
            let checked = match_check(hir_db, def);
            assert!(!checked.not_checked, "{def:?} was not checked: type checking failed");
            let rendered = checked
                .lints
                .iter()
                .map(render_lint)
                .chain(checked.errors.iter().map(render_error))
                .map(|(at, finding)| {
                    let node = source_map
                        .expr_or_pat_syntax(at)
                        .unwrap_or_else(|_| panic!("{def:?}: {finding} is not written anywhere"));
                    let range = node.map(|it| it.text_range()).original_node_file_range_rooted(&db);
                    (text[range.range].to_owned(), finding)
                })
                .collect::<Vec<_>>();
            found.push((root.range.start(), rendered));
        }
        found.sort_by_key(|(start, _)| *start);
        let actual: Vec<(String, String)> =
            found.into_iter().flat_map(|(_, rendered)| rendered).collect();
        let expected: Vec<(String, String)> =
            expected.iter().map(|&(at, finding)| (at.to_owned(), finding.to_owned())).collect();
        assert_eq!(actual, expected, "\n{text}");
    });
}

// tests/ui/pattern/usefulness/match-arm-statics.rs
#[test]
fn match_arm_statics() {
    check(
        r#"
//- minicore: option, derive, eq
use self::Direction::{North, East, South, West};

#[derive(PartialEq, Eq)]
struct NewBool(bool);

#[derive(PartialEq, Eq)]
enum Direction {
    North,
    East,
    South,
    West
}

const TRUE_TRUE: (bool, bool) = (true, true);

fn nonexhaustive_1() {
    match (true, false) {
        TRUE_TRUE => (),
        (false, false) => (),
        (false, true) => ()
    }
}

fn unreachable_1() {
    match (true, false) {
        TRUE_TRUE => (),
        (false, false) => (),
        (false, true) => (),
        (true, false) => (),
        (true, true) => ()
    }
}

const NONE: Option<Direction> = None;
const EAST: Direction = East;

fn nonexhaustive_2() {
    match Some(Some(North)) {
        Some(NONE) => (),
        Some(Some(North)) => (),
        Some(Some(EAST)) => (),
        Some(Some(South)) => (),
        None => ()
    }
}

fn unreachable_2() {
    match Some(Some(North)) {
        Some(NONE) => (),
        Some(Some(North)) => (),
        Some(Some(EAST)) => (),
        Some(Some(South)) => (),
        Some(Some(West)) => (),
        Some(Some(East)) => (),
        None => ()
    }
}

const NEW_FALSE: NewBool = NewBool(false);
struct Foo {
    bar: Option<Direction>,
    baz: NewBool
}

fn nonexhaustive_3() {
    match (Foo { bar: Some(North), baz: NewBool(true) }) {
        Foo { bar: None, baz: NewBool(true) } => (),
        Foo { bar: _, baz: NEW_FALSE } => (),
        Foo { bar: Some(West), baz: NewBool(true) } => (),
        Foo { bar: Some(South), .. } => (),
        Foo { bar: Some(EAST), .. } => ()
    }
}

fn unreachable_3() {
    match (Foo { bar: Some(EAST), baz: NewBool(true) }) {
        Foo { bar: None, baz: NewBool(true) } => (),
        Foo { bar: _, baz: NEW_FALSE } => (),
        Foo { bar: Some(West), baz: NewBool(true) } => (),
        Foo { bar: Some(South), .. } => (),
        Foo { bar: Some(EAST), .. } => (),
        Foo { bar: Some(North), baz: NewBool(true) } => (),
        Foo { bar: Some(EAST), baz: NewBool(false) } => ()
    }
}
"#,
        &[
            ("(true, false)", "E0004: (true, false)"),
            ("(true, true)", "unreachable_patterns"),
            ("Some(Some(North))", "E0004: Some(Some(Direction::West))"),
            ("Some(Some(East))", "unreachable_patterns"),
            (
                "(Foo { bar: Some(North), baz: NewBool(true) })",
                "E0004: Foo { bar: Some(Direction::North), baz: NewBool(true) }",
            ),
            ("Foo { bar: Some(EAST), baz: NewBool(false) }", "unreachable_patterns"),
        ],
    );
}

// tests/ui/pattern/usefulness/top-level-alternation.rs
#[test]
fn top_level_alternation() {
    check(
        r#"
//- minicore: option, index, slice, range
fn main() {
    while let 0..=2 | 1 = 0 {}
    if let 0..=2 | 1 = 0 {}

    match 0u8 {
        0
            | 0 => {}
        _ => {}
    }
    match Some(0u8) {
        Some(0)
            | Some(0) => {}
        _ => {}
    }
    match (0u8, 0u8) {
        (0, _) | (_, 0) => {}
        (0, 0) => {}
        (1, 1) => {}
        _ => {}
    }
    match ((0u8,),) {
        ((0,) | (1,),) => {}
        ((1..=2,),) => {}
        _ => {}
    }
    match (0u8,) {
        (1
            | 1,) => {}
        _ => {}
    }
    match &[][..] {
        [0] => {}
        [0, _] => {}
        [0, _, _] => {}
        [1, ..] => {}
        [1
            | 2, ..] => {}
        _ => {}
    }
    match &[][..] {
        [true] => {}
        [true | false, ..] => {}
        _ => {}
    }
    match &[][..] {
        [false] => {}
        [true, ..] => {}
        [true
            | false, ..] => {}
        _ => {}
    }
    match None {
        Some(false) => {}
        None | Some(true
                | false) => {}
    }
}
"#,
        &[
            ("1", "unreachable_patterns"),
            ("1", "unreachable_patterns"),
            ("0", "unreachable_patterns"),
            ("Some(0)", "unreachable_patterns"),
            ("(0, 0)", "unreachable_patterns"),
            ("1", "unreachable_patterns"),
            ("1", "unreachable_patterns"),
            ("true", "unreachable_patterns"),
            ("false", "unreachable_patterns"),
        ],
    );
}

// tests/ui/pattern/usefulness/integer-ranges/reachability.rs, with the cases its macro writes
// spelled out, and the range lints it allows.
#[test]
fn integer_ranges_reachability() {
    check(
        r#"
fn main() {
    match 0u8 { 42 => {} 41 => {} _ => {} }
    match 0u8 { 42 => {} 42 => {} _ => {} }
    match 0u8 { 20..=30 => {} 19 => {} _ => {} }
    match 0u8 { 20..=30 => {} 20 => {} _ => {} }
    match 0u8 { 20..=30 => {} 25 => {} _ => {} }
    match 0u8 { 20..=30 => {} 30 => {} _ => {} }
    match 0u8 { 20..=30 => {} 31 => {} _ => {} }
    match 0u8 { 20..30 => {} 29 => {} _ => {} }
    match 0u8 { 20..30 => {} 30 => {} _ => {} }
    match 0u8 { 20..=30 => {} 20..=30 => {} _ => {} }
    match 0u8 { 20..=30 => {} 30..=31 => {} _ => {} }
    match 0u8 { 0..10 => {} 11..20 => {} _ => {} }
    match 0u8 { 0..=255 => {} _ => {} }
    match 'a' { 'a'..='z' => {} 'q' => {} _ => {} }
    match 0i8 { -128..=0 => {} 1..=127 => {} }
    match 0i8 { -128..=0 => {} 1..=126 => {} }
}
"#,
        &[
            ("42", "unreachable_patterns"),
            ("20", "unreachable_patterns"),
            ("25", "unreachable_patterns"),
            ("30", "unreachable_patterns"),
            ("29", "unreachable_patterns"),
            ("20..=30", "unreachable_patterns"),
            ("30..=31", "overlapping_range_endpoints: 30_u8"),
            ("0..10", "non_contiguous_range_endpoints: 10_u8 (0_u8..=10_u8, 1 after)"),
            ("_", "unreachable_patterns"),
            ("'q'", "unreachable_patterns"),
            ("0i8", "E0004: i8::MAX"),
        ],
    );
}

// tests/ui/pattern/usefulness/uninhabited.rs and empty-types.rs: an empty type's values,
// read where the place is valid and where it may not be.
#[test]
fn uninhabited() {
    check(
        r#"
//- minicore: result
enum Void {}

fn by_value(x: Void) {
    match x {}
}

fn result_by_value(x: Result<u32, Void>) {
    let Ok(_y) = x;
    match x {
        Ok(_) => {}
    }
    match x {
        Ok(_) => {}
        Err(_) => {}
    }
}

fn through_a_reference(x: &Void, r: &Result<u32, Void>) {
    match *x {}
    let Ok(_y) = *r;
}
"#,
        // rustc_pattern_analysis `compute_exhaustiveness_and_usefulness`: without
        // `exhaustive_patterns` an empty arm is unreachable only at the top level of a `!` or
        // empty-enum scrutinee (rust-lang/rust#129103), so `Err(_)` is not linted; and that
        // top-level exception omits empty arms whatever the place, so `match *x {}` is
        // exhaustive.
        &[("Ok(_y)", "E0005 local binding: Err(_)")],
    );
}

// tests/ui/pattern/usefulness/bindings-same-as-variants.rs (E0170): a binding named like a unit
// variant of its type, suggested a path where the pattern is refutable or the enum has one
// variant.
#[test]
fn bindings_same_as_variants() {
    check(
        r#"
pub enum E { One, Two }
pub enum Single { Only }

fn in_a_match(e: E) {
    match e {
        One => {}
        Two => {}
    }
}

fn in_a_let(e: E, s: Single) {
    let One = e;
    let Only = s;
}
"#,
        &[
            ("One", "bindings_with_variant_name: suggest"),
            ("Two", "bindings_with_variant_name: suggest"),
            ("Two", "unreachable_patterns: catchall"),
            ("One", "bindings_with_variant_name"),
            ("Only", "bindings_with_variant_name: suggest"),
        ],
    );
}

// tests/ui/rfcs/rfc-2294-if-let-guard/warns.rs
#[test]
fn if_let_guard() {
    check(
        r#"
//- minicore: option
fn irrefutable_let_guard() {
    match Some(()) {
        Some(x) if let () = x => {}
        _ => {}
    }
}

fn unreachable_pattern() {
    match Some(()) {
        x if let None | None = x => {}
        _ => {}
    }
}
"#,
        &[
            ("let () = x", "irrefutable_let_patterns: IfLetGuard"),
            ("None", "unreachable_patterns"),
        ],
    );
}

// tests/ui/let-else/let-else-irrefutable.rs, and `check_irrefutable_option_some`'s case.
#[test]
fn let_else_irrefutable() {
    check(
        r#"
//- minicore: option
fn main() {
    let x = 1 else { return };
}

fn wrapped(o: Option<u8>) {
    let y = Some(o) else { return };
}
"#,
        &[
            ("x", "irrefutable_let_patterns: LetElse"),
            ("y", "irrefutable_let_patterns: LetElse, replace with Some"),
        ],
    );
}

// tests/ui/pattern/usefulness/refutable-pattern-in-fn-arg.rs and refutable-pattern-errors.rs:
// the bindings `check_binding_is_irrefutable` checks besides `let`.
#[test]
fn refutable_bindings() {
    check(
        r#"
//- minicore: option, fn
fn param((1, _): (u8, u8)) {}

fn closure() {
    let f = |3: isize| {};
}

fn without_initializer() {
    let Some(x): Option<u8>;
}
"#,
        &[
            ("(1, _)", "E0005 function argument: (0_u8, _) | (2_u8..=u8::MAX, _)"),
            ("3", "E0005 closure argument: ..=2_isize | 4_isize.."),
            ("Some(x)", "E0005 local binding: None"),
        ],
    );
}

// E0030 and E0579 (tests/ui/error-codes/E0030.rs, tests/ui/match/match-range-fail-2.rs).
#[test]
fn empty_ranges() {
    check(
        r#"
fn main() {
    match 5u32 {
        1000..=5 => {}
        _ => {}
    }
    match 5u32 {
        5..5 => {}
        _ => {}
    }
}
"#,
        &[("1000..=5", "E0030"), ("5..5", "E0579")],
    );
}

#[test]
fn a_body_without_a_match_computes_nothing() {
    let fx = fixture::db(
        r#"
fn plain(a: u8) -> u8 { let b = a; b }
"#,
    );
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let hir_db: &Db = &db;
        for &krate in all_crates(&db).iter() {
            for (_, module) in crate_def_map(&db, krate).modules(&db) {
                for def in module.scope(&db).declarations() {
                    let ModuleDefId::FunctionId(it) = def else { continue };
                    let def = DefWithBodyId::from(it);
                    let body = Body::of(&db, def);
                    let infer = hir_ty::InferenceResult::of(hir_db, def);
                    assert!(!super::visit::needs_analysis(&body, infer));
                    let checked = match_check(hir_db, def);
                    assert!(checked.lints.is_empty() && checked.errors.is_empty());
                    assert!(!checked.failed && !checked.not_checked);
                }
            }
        }
    });
}

/// The shortcuts of a body with nothing to analyze give what the scan and the walk they skip
/// give, and a body the binding scan clears takes no allocation.
#[test]
fn shortcuts_match_the_walk() {
    let fx = fixture::db(
        r#"
//- minicore: option, fn
pub enum E { One, Two }
pub enum Single { Only }
struct S { a: u8 }

fn plain(a: u8) -> u8 { let b = a; b }
fn named(e: E, s: &Single) { let One = e; let Only = s; }
fn named_param(One: E) {}
fn closure() -> u8 { let f = |x: u8, _| x; f(1, 2) }
fn closure_named() { let f = |Two: E| {}; }
fn assign(mut s: S) { let x; x = 1; _ = x; s.a = 2; }
fn with_match(o: Option<u8>) -> u8 { match o { Some(x) => x, None => 0 } }
fn with_let(o: Option<u8>) { if let Some(_) = o {} }
fn let_else() { let x = 1 else { return }; }
fn refutable_param((1, _): (u8, u8)) {}
"#,
    );
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let hir_db: &Db = &db;
        for &krate in all_crates(&db).iter() {
            for (_, module) in crate_def_map(&db, krate).modules(&db) {
                for def in module.scope(&db).declarations() {
                    let ModuleDefId::FunctionId(it) = def else { continue };
                    let def = DefWithBodyId::from(it);
                    let body = Body::of(&db, def);
                    let infer = hir_ty::InferenceResult::of(hir_db, def);
                    let needs = super::visit::needs_analysis(&body, infer);
                    assert_eq!(needs, super::visit::slow::needs_analysis(&body, infer), "{def:?}");
                    if needs {
                        continue;
                    }
                    let walked = super::visit::slow::walk(hir_db, def, infer);
                    if !super::visit::may_name_a_variant(hir_db, &body, infer) {
                        assert_eq!(walked, crate::MatchCheck::default(), "{def:?}");
                        blaze_alloc_count::assert_at_most(0, "a body with nothing to check", || {
                            assert_eq!(super::visit::check_body(hir_db, def, infer), walked);
                        });
                    }
                    assert_eq!(super::visit::check_body(hir_db, def, infer), walked, "{def:?}");
                }
            }
        }
    });
}

#[test]
fn erroneous_bodies_are_not_checked() {
    let fx = fixture::db(
        r#"
fn mismatched() {
    let x: u8 = "not a u8";
    match x { 1 => {} }
}
"#,
    );
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let hir_db: &Db = &db;
        for &krate in all_crates(&db).iter() {
            for (_, module) in crate_def_map(&db, krate).modules(&db) {
                for def in module.scope(&db).declarations() {
                    let ModuleDefId::FunctionId(it) = def else { continue };
                    let checked = match_check(hir_db, it.into());
                    assert!(checked.not_checked);
                    assert!(checked.errors.is_empty());
                }
            }
        }
    });
}