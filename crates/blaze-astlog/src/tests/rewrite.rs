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
fn a_text_rewrite_of_a_file_that_already_has_a_syntax_error_is_refused_naming_it() {
    let fixture = format!("{LIB}pub fn broken() -> u32 {{ let y = ; 2 }}\npub fn other() -> u32 {{ new(5) }}\n");
    let (db, files) = view(&fixture, 0, &["lib.rs"]);
    // One rewrite mends the file's own error and another makes a new one: as many errors as
    // before, and still a broken file.
    let rules = r#"
        (rule (bad s) (syntax s "LET_STMT") (enclosing-fn s f) (def-path f "lib::broken"))
        (rule (new-path p) (path p "lib::new"))
        (rewrite mended (bad s) (replace s "let y = 1;"))
        (rewrite broke (new-path p) (replace p "new("))
    "#;
    let error = refused(edit(&db, &files, rules, &[]));
    let line = fixture.lines().position(|it| it.contains("pub fn broken")).unwrap() - 1;
    assert!(error.starts_with(&format!("lib.rs:{line}:")), "{error}");
    assert!(error.contains("already has a syntax error"), "{error}");
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
fn a_template_past_the_token_cap_is_refused_naming_its_rule() {
    let (db, files) = view(LIB, 0, &["lib.rs"]);
    // `old($x, ` and 1023 more arguments: 2050 tokens.
    let rules = format!(
        "(rule (call e x) (expr e \"lib::old($x)\"))\n\
         (rewrite big (call e x) (not (in-macro-expansion e)) (replace e \"old($x, {}1)\"))",
        "1, ".repeat(1022)
    );
    let error = refused(edit(&db, &files, &rules, &[]));
    assert!(error.contains("`big` (rules:2)") && error.contains("has 2050 tokens"), "{error}");
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
fn a_template_path_inside_a_string_or_a_comment_is_left_as_written() {
    let (db, files) = view(LIB, 0, &["lib.rs"]);
    // `::lib::nwe` names nothing: as code it would be refused, as text it is only text.
    let rules = r#"
        (rule (call e x) (expr e "lib::old($x)"))
        (rewrite to-new (call e x) (not (in-macro-expansion e))
          (replace e "{ let _ = \"::lib::nwe\"; ::lib::new($x) /* was ::lib::new */ }"))
    "#;
    let applied = edit(&db, &files, rules, &[]).unwrap();
    let text = &after(&files, &applied.edited.ops)[0].1;
    assert!(text.contains(r#"{ { let _ = "::lib::nwe"; new(1) /* was ::lib::new */ } }"#), "{text}");
    assert!(text.contains(r#"{ { let _ = "::lib::nwe"; crate::new(2) /* was ::lib::new */ } }"#), "{text}");
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

#[test]
fn a_rename_reached_through_several_references_is_one_rename() {
    let (db, files) = view(LIB, 0, &["lib.rs"]);
    // `old` has three references, so three rows ask for the same rename.
    let through = r#"
        (rule (uses n) (def-path d "lib::old") (references d n))
        (rewrite renamed (uses n) (rename n "older"))
    "#;
    let direct = r#"
        (rule (target d) (def-path d "lib::old"))
        (rewrite renamed (target d) (rename d "older"))
    "#;
    let through = edit(&db, &files, through, &[]).unwrap();
    let direct = edit(&db, &files, direct, &[]).unwrap();
    assert_eq!(through.edited.ops, direct.edited.ops);
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

const MOVED: &str = r#"
//- /lib.rs crate:lib edition:2021
macro_rules! id {
    ($($t:tt)*) => { $($t)* };
}
pub mod m {
    pub fn old(x: u32) -> u32 { x }
    pub fn new(x: u32) -> u32 { x }
}
mod alone {
    use crate::m::old;
    pub fn f() -> u32 { old(1) + old(2) }
}
mod listed {
    use crate::m::{self, old};
    pub fn f() -> u32 { old(1) + m::old(2) }
}
mod still_read {
    use crate::m::old;
    pub fn f() -> u32 { old(1) + id!(old(3)) }
    pub fn keep() -> fn(u32) -> u32 { old }
}
"#;

/// The paths of `lib::m::old` in a fn other than `keep`, or anywhere (`use` paths included).
const IN_FN: &str = r#"
    (rule (old-path p) (path p "lib::m::old"))
    (rule (kept p) (old-path p) (syntax n "FN") (defines n d) (def-name d "keep") (ancestor n p))
    (rule (in-fn p) (old-path p) (syntax n "FN") (ancestor n p))
"#;

/// A bare name the retarget moves came through a `use` of the old item: the new item is
/// imported where that `use` is, and the `use` goes when nothing else reads what it imports,
/// whether or not the rule retargets the `use` too. A read the rules leave keeps the `use`,
/// and the import goes beside it; a retargeted `use` a read still needs is refused.
#[test]
fn a_bare_retarget_imports_the_new_item_and_drops_the_use_nothing_else_reads() {
    let (db, files) = view(MOVED, 0, &["lib.rs"]);
    let run = |only: &str| {
        let rules = format!("{IN_FN}(rewrite moved (old-path p) {only} (retarget p \"lib::m::new\"))");
        edit(&db, &files, &rules, &[])
    };
    let alone = "mod alone {\n    use crate::m::new;\n    pub fn f() -> u32 { new(1) + new(2) }\n}";
    // The import is the path the module names the item by: `m` is imported here.
    let listed = "mod listed {\n    use crate::m::{self};\n    use m::new;\n    \
                  pub fn f() -> u32 { new(1) + m::new(2) }\n}";
    // Only the reads whose syntax is in a fn other than `keep`: no `use` is retargeted, and
    // nor is the path in `id!`'s input, whose syntax is the expansion's.
    let applied = run("(in-fn p) (not (kept p))").unwrap();
    let text = &after(&files, &applied.edited.ops)[0].1;
    assert!(text.contains(alone) && text.contains(listed), "{text}");
    let kept = "mod still_read {\n    use crate::m::old;\n    use crate::m::new;\n    \
                pub fn f() -> u32 { new(1) + id!(old(3)) }\n    \
                pub fn keep() -> fn(u32) -> u32 { old }\n}";
    assert!(text.contains(kept), "{text}");
    // Every path, `use`s included: each `use` goes, the last read with it.
    let applied = run("").unwrap();
    let text = &after(&files, &applied.edited.ops)[0].1;
    assert!(text.contains(alone) && text.contains(listed), "{text}");
    let all = "mod still_read {\n    use crate::m::new;\n    \
               pub fn f() -> u32 { new(1) + id!(new(3)) }\n    \
               pub fn keep() -> fn(u32) -> u32 { new }\n}";
    assert!(text.contains(all), "{text}");
    // The `use` of `still_read`, but not `keep`'s read of it.
    let error = refused(run("(not (kept p))"));
    assert!(
        error.contains("retargets the import `crate::m::old`, which `old` at lib.rs:19 still reads"),
        "{error}"
    );
}

/// The new item's name already means something else where the bare name is: importing it
/// would clash with that or shadow it, so the retarget is refused, naming both.
#[test]
fn a_bare_retarget_whose_new_name_is_taken_is_refused_naming_what_has_it() {
    let fixture = r#"
//- /lib.rs crate:lib edition:2021
pub mod m {
    pub fn old(x: u32) -> u32 { x }
    pub fn new(x: u32) -> u32 { x }
}
mod taken {
    use crate::m::old;
    fn new() {}
    pub fn f() -> u32 { new(); old(1) }
}
"#;
    let (db, files) = view(fixture, 0, &["lib.rs"]);
    let rules = r#"
        (rule (old-path p) (path p "lib::m::old"))
        (rewrite moved (old-path p) (retarget p "lib::m::new"))
    "#;
    let error = refused(edit(&db, &files, rules, &[]));
    assert!(
        error.contains("`new` already names `lib::taken::new` here, so `lib::m::new` can't be imported"),
        "{error}"
    );
}

const PARAMS: &str = r#"
//- /lib.rs crate:lib edition:2021
pub fn mix(a: u32, b: u32, unused: u32) -> u32 { a - b }
pub fn call(x: u32) -> u32 { mix(1, x, 3) + mix(x, 2, x) }
"#;

#[test]
fn remove_param_and_reorder_params_change_the_fn_and_every_call() {
    let (db, files) = view(PARAMS, 0, &["lib.rs"]);
    let rules = r#"
        (rule (mix d) (def-path d "lib::mix"))
        (rewrite drop (mix d) (remove-param d "unused"))
        (rewrite swap (mix d) (reorder-params d "b" "a" "unused"))
    "#;
    // Both rewrites make one `Edit::Params` each for one fn: blaze-intent refuses the pair.
    let both = refused(edit(&db, &files, rules, &[]));
    assert!(both.contains("two changes of `mix`'s parameters"), "{both}");
    let rules = r#"
        (rule (mix d) (def-path d "lib::mix"))
        (rewrite drop (mix d) (remove-param d "unused"))
    "#;
    let applied = edit(&db, &files, rules, &[]).unwrap();
    let text = &after(&files, &applied.edited.ops)[0].1;
    assert!(text.contains("pub fn mix(a: u32, b: u32) -> u32"), "{text}");
    assert!(text.contains("{ mix(1, x) + mix(x, 2) }"), "{text}");
    let rules = r#"
        (rule (mix d) (def-path d "lib::mix"))
        (rewrite swap (mix d) (reorder-params d "unused" "b" "a"))
    "#;
    let applied = edit(&db, &files, rules, &[]).unwrap();
    let text = &after(&files, &applied.edited.ops)[0].1;
    assert!(text.contains("pub fn mix(unused: u32, b: u32, a: u32) -> u32"), "{text}");
    assert!(text.contains("{ mix(3, x, 1) + mix(x, 2, x) }"), "{text}");
}

#[test]
fn a_param_action_that_names_no_parameter_or_not_every_one_is_refused_naming_the_fn() {
    let (db, files) = view(PARAMS, 0, &["lib.rs"]);
    let run = |action: &str| {
        let rules = format!("(rule (mix d) (def-path d \"lib::mix\"))\n(rewrite r (mix d) {action})");
        refused(edit(&db, &files, &rules, &[]))
    };
    let missing = run(r#"(remove-param d "c")"#);
    assert!(missing.contains("`lib::mix` has no parameter `c` (it has `a`, `b`, `unused`)"), "{missing}");
    let partial = run(r#"(reorder-params d "b" "a")"#);
    assert!(partial.contains("names every parameter of `lib::mix` once"), "{partial}");
    let past = run(r#"(add-param d "c: u32" "0" 4)"#);
    assert!(past.contains("index 4 is past `lib::mix`'s 3 parameters"), "{past}");
    let reads = run(r#"(remove-param d "a")"#);
    assert!(reads.contains("removes parameter `a`, which the body still reads"), "{reads}");
    let arity = refused(edit(&db, &files, "(rule (mix d) (def-path d \"lib::mix\"))\n(rewrite r (mix d) (add-param d \"c: u32\"))", &[]));
    assert!(arity.contains("`add-param` takes 3 or 4 arguments, got 2"), "{arity}");
}
