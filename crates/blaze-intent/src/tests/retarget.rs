//! Retargeting std's locks to parking_lot's, against the front end: each import form is
//! rewritten so that, in a front end of the new text, every name that named std's lock names
//! parking_lot's, and nothing else changes.

use hir::{ModuleDef, view::Semantics};
use hir_expand::fixture;
use syntax::view::{self as ast, AstNode as _};

use super::db::TestDB;
use crate::retarget::{Retarget, Rewrite, names_in};

const LOCKS: &str = r#"
//- /std.rs crate:std
pub mod sync {
    pub struct Arc<T>(T);
    pub struct Mutex<T>(T);
    impl<T> Mutex<T> {
        pub const fn new(value: T) -> Mutex<T> { Mutex(value) }
    }
    pub struct RwLock<T>(T);
    pub struct Condvar;
}
//- /parking_lot.rs crate:parking_lot
pub struct Mutex<T>(T);
impl<T> Mutex<T> {
    pub const fn new(value: T) -> Mutex<T> { Mutex(value) }
}
pub struct RwLock<T>(T);
pub struct Condvar;
"#;

const MAIN: &str = "//- /main.rs crate:main deps:std,parking_lot\n";

/// The file `main.rs` of `fixture`, retargeted from std's locks to parking_lot's.
fn retargeted(main: &str) -> Result<String, String> {
    let fx = fixture::db(&format!("{LOCKS}{MAIN}{main}"));
    let file = *fx.files().last().expect("main.rs");
    let db: TestDB = fx.into_db();
    let sema = Semantics::new(&db);
    let editioned = sema.attach_first_edition(file.file_id(&db));
    let root = sema.parse(editioned).syntax();
    let to = |def: ModuleDef| {
        let krate = def.module(&db)?.krate(&db).display_name(&db)?;
        let name = def.name(&db)?;
        (krate.to_string() == "std" && matches!(name.as_str(), "Mutex" | "RwLock" | "Condvar"))
            .then(|| format!("parking_lot::{}", name.as_str()))
    };
    let resolve = |path| names_in(&sema, path);
    let retarget = Retarget { db: &db, resolve: &resolve, to: &to };
    let rewrites = retarget.file(root)?;
    let mut text = root.text().to_owned();
    for Rewrite { range, text: insert } in rewrites.iter().rev() {
        text.replace_range(usize::from(range.start())..usize::from(range.end()), insert);
    }
    Ok(text)
}

/// The crate each path of `main` named `Mutex`, `RwLock`, `Condvar` or `M` resolves to, in a
/// front end of it.
fn crates_named(main: &str) -> Vec<String> {
    let fx = fixture::db(&format!("{LOCKS}{MAIN}{main}"));
    let file = *fx.files().last().expect("main.rs");
    let db: TestDB = fx.into_db();
    let sema = Semantics::new(&db);
    let editioned = sema.attach_first_edition(file.file_id(&db));
    let root = sema.parse(editioned).syntax();
    let mut crates = Vec::new();
    for path in root.descendants().filter_map(ast::Path::cast) {
        let Some(name) = path.segment().and_then(|it| it.name_ref()) else { continue };
        if !matches!(name.text(), "Mutex" | "RwLock" | "Condvar" | "M") {
            continue;
        }
        let [ty, value] = names_in(&sema, path);
        let krate = ty
            .or(value)
            .and_then(|it| it.module(&db))
            .and_then(|it| it.krate(&db).display_name(&db));
        crates.push(
            krate.map_or_else(
                || format!("unresolved `{}`", path.syntax().text()),
                |it| it.to_string(),
            ),
        );
    }
    crates
}

#[test]
fn every_import_form_of_a_std_lock_moves_to_parking_lot_and_resolves_there() {
    let main = r#"
mod split {
    use std::sync::{Arc, Mutex};
    pub fn f(_: Arc<Mutex<u8>>) {}
}
mod renamed {
    use std::sync::Mutex as M;
    pub fn f(_: M<u8>) {}
}
mod glob {
    use std::sync::*;
    pub fn f(_: Mutex<u8>, _: Arc<u8>) {}
}
mod qualified {
    pub fn f(_: std::sync::Mutex<u8>) -> std::sync::Mutex<u8> {
        std::sync::Mutex::new(1)
    }
}
mod through_module {
    use std::sync;
    pub fn f(_: sync::RwLock<u8>) {}
}
mod nested {
    use std::{sync::{Arc, Condvar, Mutex}, sync::RwLock};
    pub fn f(_: Arc<u8>, _: Condvar, _: Mutex<u8>, _: RwLock<u8>) {}
}
mod public {
    #[cfg(all())]
    pub(crate) use std::sync::Mutex;
}
"#;
    let main = main.trim_start_matches('\n');
    let after = retargeted(main).unwrap();
    let want = r#"mod split {
    use std::sync::Arc;
    use parking_lot::Mutex;
    pub fn f(_: Arc<Mutex<u8>>) {}
}
mod renamed {
    use parking_lot::Mutex as M;
    pub fn f(_: M<u8>) {}
}
mod glob {
    use std::sync::*;
    use parking_lot::Mutex;
    pub fn f(_: Mutex<u8>, _: Arc<u8>) {}
}
mod qualified {
    pub fn f(_: parking_lot::Mutex<u8>) -> parking_lot::Mutex<u8> {
        parking_lot::Mutex::new(1)
    }
}
mod through_module {
    use std::sync;
    pub fn f(_: parking_lot::RwLock<u8>) {}
}
mod nested {
    use std::sync::Arc;
    use parking_lot::Condvar;
    use parking_lot::Mutex;
    use parking_lot::RwLock;
    pub fn f(_: Arc<u8>, _: Condvar, _: Mutex<u8>, _: RwLock<u8>) {}
}
mod public {
    #[cfg(all())]
    pub(crate) use parking_lot::Mutex;
}
"#;
    assert_eq!(after, want);
    let before = crates_named(main);
    assert!(before.iter().all(|it| it == "std"), "{before:?}");
    let now = crates_named(&after);
    assert!(
        !now.is_empty() && now.iter().all(|it| it == "parking_lot"),
        "every lock names parking_lot's: {now:?}"
    );
    assert_eq!(retargeted(&after).unwrap(), after, "a retargeted file has nothing left to move");
}

#[test]
fn a_comment_inside_a_use_tree_it_would_rewrite_is_an_error() {
    let error =
        retargeted("use std::sync::{\n    // the lock\n    Mutex,\n};\nfn f(_: Mutex<u8>) {}\n")
            .unwrap_err();
    assert!(error.contains("has a comment inside its tree"), "{error}");
}
