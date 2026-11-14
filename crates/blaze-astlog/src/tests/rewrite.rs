//! Rewrites against a real front end: the bytes they leave, the paths they write, and the edits
//! they refuse, each test named for the property it pins.

use blaze_vfs::Op;

use super::{View, view};
use crate::edit;

const LIB: &str = r#"
//- /lib.rs crate:lib edition:2021
macro_rules! id {
    ($($t:tt)*) => { $($t)* };
}
pub fn old(x: u32) -> u32 { x }
pub fn new(x: u32) -> u32 { x }
pub fn top() -> u32 { old(1) }
pub mod inner {
    pub fn deep() -> u32 { crate::old(2) }
}
pub fn input() -> u32 { id!(old(3)) }
"#;

/// A call no file writes: `make!` makes it. Its body names `old` too, so blaze-intent's
/// rename refuses this fixture (a macro body's tokens are no reference it can resolve).
const MADE: &str = r#"macro_rules! make {
    () => { old(9) };
}
pub fn made() -> u32 { make!() }
"#;

/// The error a request that must fail gives, shown.
#[track_caller]
fn refused(result: Result<blaze_intent::Applied, crate::Error>) -> String {
    match result {
        Ok(_) => panic!("the request applied"),
        Err(error) => error.to_string(),
    }
}

/// The text of each view file after `ops`.
pub(super) fn after(files: &View, ops: &[Op]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> =
        files.files.iter().map(|(_, path, text)| (path.clone(), text.clone())).collect();
    for op in ops {
        let Op::Edit { at, len, path, bytes } = op else { panic!("a rewrite made {op:?}") };
        let (_, text) = out.iter_mut().find(|it| it.0 == *path).expect("an edited view file");
        let at = *at as usize;
        text.replace_range(at..at + *len as usize, std::str::from_utf8(bytes).unwrap());
    }
    out
}

const MOVE: &str = r#"
(rule (call e x) (expr e "lib::old($x)"))
(rewrite to-new (call e x) (not (in-macro-expansion e)) (replace e "::lib::new($x)"))
"#;

#[test]
fn a_rewrite_writes_each_template_path_as_its_module_names_the_item() {
    let (db, files) = view(LIB, 0, &["lib.rs"]);
    let applied = edit(&db, &files, MOVE, &[]).unwrap();
    let text = &after(&files, &applied.edited.ops)[0].1;
    assert!(text.contains("pub fn top() -> u32 { new(1) }"), "{text}");
    assert!(text.contains("pub fn deep() -> u32 { crate::new(2) }"), "{text}");
    assert!(text.contains("id!(new(3))"), "a macro's input is edited where it is written: {text}");
    assert!(text.contains("pub fn old(x: u32)"), "the definition is no call: {text}");
}

#[test]
fn a_rewrite_is_byte_identical_across_two_runs() {
    let (db, files) = view(LIB, 0, &["lib.rs"]);
    let first = edit(&db, &files, MOVE, &[]).unwrap();
    let second = edit(&db, &files, MOVE, &[]).unwrap();
    assert_eq!(first.edited.ops, second.edited.ops);
    assert_eq!(first.edited.items, second.edited.items);
}

#[test]
fn a_match_the_macro_made_is_refused_at_the_macro_call_unless_excluded() {
    let fixture = format!("{LIB}{MADE}");
    let (db, files) = view(&fixture, 0, &["lib.rs"]);
    let rules = r#"
        (rule (call e x) (expr e "lib::old($x)"))
        (rewrite to-new (call e x) (replace e "::lib::new($x)"))
    "#;
    let error = refused(edit(&db, &files, rules, &[]));
    // The fixture's first two lines (an empty one and the header) are no text of lib.rs.
    let line = fixture.lines().position(|it| it.contains("make!()")).unwrap() - 1;
    let place = format!("lib.rs:{line}:");
    assert!(error.starts_with(&place), "{error}");
    assert!(error.contains("to-new") && error.contains("in-macro-expansion"), "{error}");
    // Excluded, it is no error, and the other calls still move.
    let applied = edit(&db, &files, MOVE, &[]).unwrap();
    assert!(after(&files, &applied.edited.ops)[0].1.contains("make!()"));
}

#[test]
fn two_rules_editing_overlapping_text_are_refused_naming_both_and_the_place() {
    let (db, files) = view(LIB, 0, &["lib.rs"]);
    let rules = r#"
        (rule (call e x) (expr e "lib::old($x)"))
        (rewrite first (call e x) (not (in-macro-expansion e)) (replace e "::lib::new($x)"))
        (rewrite second (call e x) (not (in-macro-expansion e)) (replace x "0"))
    "#;
    let error = refused(edit(&db, &files, rules, &[]));
    assert!(error.contains("`first` (rules:3)") && error.contains("`second` (rules:4)"), "{error}");
    assert!(error.starts_with("lib.rs:"), "{error}");
}

#[test]
fn the_same_edit_from_two_rules_is_one_edit() {
    let (db, files) = view(LIB, 0, &["lib.rs"]);
    let rules = r#"
        (rule (call e x) (expr e "lib::old($x)"))
        (rewrite first (call e x) (not (in-macro-expansion e)) (replace e "::lib::new($x)"))
        (rewrite again (call e x) (not (in-macro-expansion e)) (replace e "::lib::new($x)"))
    "#;
    let twice = edit(&db, &files, rules, &[]).unwrap();
    let once = edit(&db, &files, MOVE, &[]).unwrap();
    assert_eq!(twice.edited.ops, once.edited.ops);
}

#[test]
fn a_rewrite_that_leaves_a_file_unparsable_fails_the_request() {
    let (db, files) = view(LIB, 0, &["lib.rs"]);
    // A path is no expression, so its template is spliced as text and only the reparse
    // can refuse it.
    let rules = r#"
        (rule (old-path p) (path p "lib::old"))
        (rewrite broken (old-path p) (not (in-macro-expansion p)) (replace p "old("))
    "#;
    let error = refused(edit(&db, &files, rules, &[]));
    assert!(error.contains("unparsable"), "{error}");
}

#[test]
fn a_template_that_is_no_expression_where_it_replaces_one_is_refused_naming_its_rule() {
    let (db, files) = view(LIB, 0, &["lib.rs"]);
    let rules = r#"
        (rule (call e x) (expr e "lib::old($x)"))
        (rewrite broken (call e x) (not (in-macro-expansion e)) (replace e "old($x"))
    "#;
    let error = refused(edit(&db, &files, rules, &[]));
    assert!(error.contains("`broken` (rules:3)") && error.contains("not an expression"), "{error}");
    assert!(error.starts_with("lib.rs:"), "{error}");
}

#[test]
fn a_template_path_from_a_crate_that_names_nothing_is_an_error() {
    let (db, files) = view(LIB, 0, &["lib.rs"]);
    let rules = r#"
        (rule (call e x) (expr e "lib::old($x)"))
        (rewrite typo (call e x) (replace e "::lib::nwe($x)"))
    "#;
    let error = refused(edit(&db, &files, rules, &[]));
    assert!(error.contains("rules:3") && error.contains("lib::nwe"), "{error}");
}

#[test]
fn rename_goes_through_the_front_ends_references() {
    let (db, files) = view(LIB, 0, &["lib.rs"]);
    let rules = r#"
        (rule (target d) (def-path d "lib::old"))
        (rewrite renamed (target d) (rename d "older"))
    "#;
    let applied = edit(&db, &files, rules, &[]).unwrap();
    let text = &after(&files, &applied.edited.ops)[0].1;
    assert!(text.contains("pub fn older(x: u32)"), "{text}");
    assert!(text.contains("{ older(1) }") && text.contains("crate::older(2)"), "{text}");
    assert!(text.contains("id!(older(3))"), "{text}");
}

const PREC: &str = r#"
//- /lib.rs crate:lib edition:2021
pub fn mul(a: u32, b: u32) -> u32 { a * b }
pub fn pair(a: u32, b: u32) -> u32 { a + b }
pub fn size(x: u32) -> u32 { x }
pub fn product(x: u32, y: u32, b: u32) -> u32 { mul(x + y, b) }
pub fn length(a: u32, b: u32) -> u32 { pair(a, b).len() }
pub fn sized(x: u32) -> u32 { size(x) }
pub fn noted(x: u32, y: u32) { let _ = mul(x + y, 1); }
"#;

/// Inlines `mul`, `pair` and `size` as operators and a method: each template splices a
/// bound expression into a new place, or lands where the call's parent reads it.
const INLINE: &str = r#"
(rule (m e a b) (expr e "lib::mul($a, $b)"))
(rule (p e a b) (expr e "lib::pair($a, $b)"))
(rule (s e x) (expr e "lib::size($x)"))
(rewrite as-mul (m e a b) (replace e "$a * $b"))
(rewrite as-add (p e a b) (replace e "$a + $b"))
(rewrite as-len (s e x) (replace e "$x.len()"))
"#;

/// The tail expression of fn `name` in `text`: its kind, and its operator when it is binary.
fn tail(text: &str, name: &str) -> (syntax::SyntaxKind, Option<syntax::view::BinaryOp>) {
    use syntax::{Edition, SourceFile, SyntaxKind, view::{self as ast, AstNode as _}};
    let parse = SourceFile::parse(text, Edition::CURRENT);
    assert!(parse.errors_with_bodies().is_empty(), "{text}");
    let root = parse.root();
    let function = root
        .descendants()
        .find(|it| {
            it.kind() == SyntaxKind::FN
                && it.children().any(|it| it.kind() == SyntaxKind::NAME && it.text() == name)
        })
        .expect("the fn");
    let list = function.descendants().find(|it| it.kind() == SyntaxKind::STMT_LIST).expect("a body");
    let tail = list.children().last().expect("a tail expression");
    (tail.kind(), ast::BinExpr::cast(tail).and_then(|it| it.op_kind()))
}

fn mul_op() -> Option<syntax::view::BinaryOp> {
    Some(syntax::view::BinaryOp::ArithOp(syntax::view::ArithOp::Mul))
}

#[test]
fn a_bound_expression_spliced_into_a_tighter_operator_keeps_its_meaning() {
    let (db, files) = view(PREC, 0, &["lib.rs"]);
    let text = &after(&files, &edit(&db, &files, INLINE, &[]).unwrap().edited.ops)[0].1;
    assert!(text.contains("{ (x + y) * b }"), "{text}");
    assert_eq!(tail(text, "product"), (syntax::SyntaxKind::BIN_EXPR, mul_op()), "{text}");
}

#[test]
fn a_replacement_its_parent_binds_tighter_than_keeps_its_meaning() {
    let (db, files) = view(PREC, 0, &["lib.rs"]);
    let text = &after(&files, &edit(&db, &files, INLINE, &[]).unwrap().edited.ops)[0].1;
    assert!(text.contains("{ (a + b).len() }"), "{text}");
    assert_eq!(tail(text, "length").0, syntax::SyntaxKind::METHOD_CALL_EXPR, "{text}");
}

#[test]
fn a_splice_that_needs_no_parentheses_gets_none() {
    let (db, files) = view(PREC, 0, &["lib.rs"]);
    let text = &after(&files, &edit(&db, &files, INLINE, &[]).unwrap().edited.ops)[0].1;
    assert!(text.contains("{ x.len() }"), "{text}");
    assert!(!text.contains("(x)"), "{text}");
}

#[test]
fn a_bound_expression_in_an_inserted_statement_keeps_its_meaning() {
    let (db, files) = view(PREC, 0, &["lib.rs"]);
    let rules = r#"
        (rule (noted s a) (stmt s "let _ = lib::mul($a, 1);"))
        (rewrite doubled (noted s a) (insert-before s "let _ = $a * 2; "))
    "#;
    let text = &after(&files, &edit(&db, &files, rules, &[]).unwrap().edited.ops)[0].1;
    assert!(text.contains("{ let _ = (x + y) * 2; let _ = mul(x + y, 1); }"), "{text}");
}

// Causal: the same rewrites with precedence off reparse, and read as other code.
#[test]
fn without_precedence_the_same_rewrites_parse_as_other_code() {
    let (db, files) = view(PREC, 0, &["lib.rs"]);
    let applied = crate::rewrite::edit_with(&db, &files, INLINE, &[], crate::splice::Parens::Off).unwrap();
    let text = &after(&files, &applied.edited.ops)[0].1;
    assert!(text.contains("{ x + y * b }") && text.contains("{ a + b.len() }"), "{text}");
    assert_ne!(tail(text, "product").1, mul_op(), "the sum is now the outer operator: {text}");
    assert_eq!(tail(text, "length").0, syntax::SyntaxKind::BIN_EXPR, "{text}");
}

#[test]
fn retarget_writes_the_new_item_as_the_paths_module_names_it() {
    let (db, files) = view(LIB, 0, &["lib.rs"]);
    let rules = r#"
        (rule (old-path p) (path p "lib::old"))
        (rewrite moved (old-path p) (not (in-macro-expansion p)) (retarget p "lib::new"))
    "#;
    let applied = edit(&db, &files, rules, &[]).unwrap();
    let text = &after(&files, &applied.edited.ops)[0].1;
    assert!(text.contains("{ new(1) }") && text.contains("{ crate::new(2) }"), "{text}");
}
