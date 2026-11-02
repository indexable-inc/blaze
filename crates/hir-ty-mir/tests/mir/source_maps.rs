//! MIR reads a body's source map only where it emits a position: lowering a body, checking
//! its macro calls and evaluating a constant without `file!` or `concat!` build no map
//! (`line!` and `column!` expand to their values), and the places those macros give are where
//! they are written.

use hir_ty::{db::HirDatabase, next_solver::GenericArgs};
use hir_ty_mir::mir::{MirOptLevel, MirOptions};
use hir_expand::fixture;

use crate::{constant, db::TestDB, function};

const PLAIN: &str = r#"
macro_rules! twice {
    ($e:expr) => { $e * 2 };
}

fn add(a: u32, b: u32) -> u32 {
    let c = a + b;
    if c > 3 { twice!(c) } else { b }
}

const GOAL: u32 = { let x = 3; twice!(x) };
"#;

/// `column!()` counts a tab before it as one column, as rustc's `lookup_char_pos` does, where
/// a caller location counts it as four.
const PLACED: &str = "
#[rustc_builtin_macro]
macro_rules! line {() => {}}
#[rustc_builtin_macro]
macro_rules! column {() => {}}
#[rustc_builtin_macro]
macro_rules! concat {($($t:tt)*) => {}}

const LINE: u32 = line!();
const COLUMN: u32 =\t/* \u{e9} */ column!();
const BOTH: &str = concat!(line!(), \":\",\tcolumn!());
";

/// The queries that build a body's source map.
const MAP_QUERIES: [&str; 2] = ["Body::relative", "Body::with_source_map"];

#[track_caller]
fn assert_no_map(executed: &[String]) {
    for query in MAP_QUERIES {
        assert!(
            !executed.iter().any(|it| it == query),
            "`{query}` ran, so a source map was built: {executed:#?}"
        );
    }
}

/// The value of the constant `name`, as its bytes.
#[track_caller]
fn bytes(db: &TestDB, name: &str) -> Vec<u8> {
    let value = db
        .const_eval(constant(db, name), GenericArgs::empty(), None)
        .unwrap_or_else(|error| panic!("evaluating `{name}`: {error:?}"));
    value.memory.to_vec()
}

/// The value of the `&str` constant `name`.
#[track_caller]
fn text(db: &TestDB, name: &str) -> String {
    let value = db
        .const_eval(constant(db, name), GenericArgs::empty(), None)
        .unwrap_or_else(|error| panic!("evaluating `{name}`: {error:?}"));
    let bytes: &[u8] = &value.memory;
    let (address, len) = bytes.split_at(bytes.len() / 2);
    let address = usize::from_le_bytes(address.try_into().unwrap());
    let len = usize::from_le_bytes(len.try_into().unwrap());
    let text = value.memory_map.get(address, len).expect("the string's bytes");
    String::from_utf8(text.to_vec()).unwrap()
}

/// rustc's line and column (1-based, in characters) of the first `needle` in `text`.
fn place_of(text: &str, needle: &str) -> (u32, u32) {
    let at = text.find(needle).expect("the fixture has it");
    let before = &text[..at];
    let line = before.matches('\n').count() as u32 + 1;
    let line_start = before.rfind('\n').map_or(0, |it| it + 1);
    (line, text[line_start..at].chars().count() as u32 + 1)
}

#[test]
fn lowering_a_body_builds_no_source_map() {
    let fx = fixture::db(PLAIN);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let func = function(&db, "add");
        let executed = db.log_executed(|| {
            db.mir_body(func.into()).unwrap_or_else(|error| panic!("the MIR of `add`: {error:?}"));
            db.optimized_mir_body(func.into(), MirOptions::at(MirOptLevel::Debug))
                .unwrap_or_else(|error| panic!("the optimized MIR of `add`: {error:?}"));
        });
        assert!(executed.iter().any(|it| it == "Body::of"), "{executed:#?}");
        assert_no_map(&executed);
    });
}

#[test]
fn evaluating_a_constant_without_location_macros_builds_no_source_map() {
    let fx = fixture::db(PLAIN);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let executed = db.log_executed(|| {
            assert_eq!(bytes(&db, "GOAL"), 6u32.to_le_bytes());
        });
        assert_no_map(&executed);
    });
}

#[test]
fn evaluating_line_and_column_builds_no_source_map() {
    let fx = fixture::db(PLACED);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let executed = db.log_executed(|| {
            bytes(&db, "LINE");
            bytes(&db, "COLUMN");
        });
        assert_no_map(&executed);
    });
}

#[test]
fn location_macros_are_where_they_are_written() {
    let fx = fixture::db(PLACED);
    let file = fx.only_file();
    let db = fx.into_db();
    let written = file.file_id(&db);
    let source = db.file_text(written).text(&db).to_string();
    let (line, _) = place_of(&source, "line!();");
    let (_, column) = place_of(&source, "column!();");
    let (both_line, _) = place_of(&source, "concat!(line!()");
    let (_, both_column) = place_of(&source, "column!())");
    hir_ty::attach_db(&db, || {
        assert_eq!(bytes(&db, "LINE"), line.to_le_bytes());
        assert_eq!(bytes(&db, "COLUMN"), column.to_le_bytes());
        assert_eq!(text(&db, "BOTH"), format!("{both_line}:{both_column}"));
    });
}
