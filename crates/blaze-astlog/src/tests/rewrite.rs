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
fn after(files: &View, ops: &[Op]) -> Vec<(String, String)> {
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
    let rules = r#"
        (rule (call e x) (expr e "lib::old($x)"))
        (rewrite broken (call e x) (not (in-macro-expansion e)) (replace e "old($x"))
    "#;
    let error = refused(edit(&db, &files, rules, &[]));
    assert!(error.contains("unparsable"), "{error}");
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
