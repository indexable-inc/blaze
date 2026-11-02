//! `file!()` is the name the build gives the file, and a constant of it is evaluated again when
//! the build names files differently.

use hir_ty::{db::HirDatabase, next_solver::GenericArgs};
use hir_ty_mir::mir::location::name_sources;
use hir_expand::fixture;

use crate::{constant, db::TestDB};

const PROGRAM: &str = r#"
#[rustc_builtin_macro]
macro_rules! file {() => {}}

const GOAL: &str = file!();
"#;

/// `GOAL`'s value: an address and a length, the bytes behind them in the memory map.
#[track_caller]
fn goal(db: &TestDB) -> String {
    hir_ty::attach_db(db, || {
        let value = db
            .const_eval(constant(db, "GOAL"), GenericArgs::empty(), None)
            .unwrap_or_else(|error| panic!("evaluating `GOAL`: {error:?}"));
        let bytes: &[u8] = &value.memory;
        let (address, len) = bytes.split_at(bytes.len() / 2);
        let address = usize::from_le_bytes(address.try_into().unwrap());
        let len = usize::from_le_bytes(len.try_into().unwrap());
        let text = value.memory_map.get(address, len).expect("the string's bytes");
        String::from_utf8(text.to_vec()).unwrap()
    })
}

#[test]
fn file_follows_the_names_the_build_gives() {
    let fx = fixture::db(PROGRAM);
    let _ = fx.only_file();
    let mut db = fx.into_db();
    // Before the build names files, the evaluator refuses: no placeholder for a path.
    let unnamed = hir_ty::attach_db(&db, || {
        db.const_eval(constant(&db, "GOAL"), GenericArgs::empty(), None)
            .map(|_| ())
            .map_err(|error| format!("{error:?}"))
    });
    assert!(unnamed.as_ref().is_err_and(|it| it.contains("name_sources")), "{unnamed:?}");
    name_sources(&mut db, "first", |_, _| Some("src/first.rs".to_owned()));
    assert_eq!(goal(&db), "src/first.rs");
    name_sources(&mut db, "second", |_, _| Some("src/second.rs".to_owned()));
    assert_eq!(goal(&db), "src/second.rs");
}

#[test]
fn the_same_key_is_the_same_naming() {
    let fx = fixture::db(PROGRAM);
    let _ = fx.only_file();
    let mut db = fx.into_db();
    name_sources(&mut db, "build", |_, _| Some("src/lib.rs".to_owned()));
    assert_eq!(goal(&db), "src/lib.rs");
    name_sources(&mut db, "build", |_, _| Some("src/other.rs".to_owned()));
    assert_eq!(goal(&db), "src/lib.rs");
}

#[test]
fn each_file_gets_its_own_name() {
    let fx = fixture::db(PROGRAM);
    let file = fx.only_file();
    let mut db = fx.into_db();
    let named = file.file_id(&db);
    name_sources(&mut db, "build", move |_, file| (file == named).then(|| "src/lib.rs".to_owned()));
    assert_eq!(goal(&db), "src/lib.rs");
}
