//! Replace over two crates of the view and two outside it, against the front end: every import
//! form of std's `Mutex` (a `use`, a list, a rename, a glob, a block's glob, a qualified path, a
//! path through a module, a re-export of the view and a glob of it, the prelude, an alias of
//! the dependency's, a macro's input) moves to parking_lot's. The bytes are the hand-written
//! text edits' and the VFS root theirs, and in a front end of the new text every name that
//! named std's lock names parking_lot's, nothing else changes, and nothing is unresolved or
//! mistyped.

use std::collections::BTreeMap;

use base_db::FileId;
use blaze_api::Edit;
use blaze_vfs::Op;
use hir::{Crate, diagnostics::AnyDiagnostic, view::Semantics};
use hir_expand::fixture;
use syntax::{
    SyntaxKind,
    view::{self as ast, AstNode as _, HasName as _},
};

use super::{applied, assert_fresh, db::TestDB, roots};
use crate::{Files, apply, retarget::names_in};

const OUTSIDE: &str = r#"
//- /std.rs crate:std edition:2021
pub mod sync {
    mod mutex {
        pub struct Mutex(pub u32);
        impl Mutex {
            pub fn new(value: u32) -> Mutex { Mutex(value) }
        }
    }
    pub use self::mutex::Mutex;
    pub struct Arc(pub u32);
    pub struct RwLock(pub u32);
    impl RwLock {
        pub fn new(value: u32) -> RwLock { RwLock(value) }
    }
}
pub mod alias {
    pub use crate::sync::Mutex as Lock;
}
pub mod prelude {
    pub mod rust_2021 {
        pub use crate::sync::Mutex;
    }
}
//- /parking_lot.rs crate:parking_lot edition:2021
pub struct Mutex(pub u32);
impl Mutex {
    pub fn new(value: u32) -> Mutex { Mutex(value) }
}
pub struct RwLock(pub u32);
impl RwLock {
    pub fn new(value: u32) -> RwLock { RwLock(value) }
}
"#;

const LIB: &str = r#"pub mod locks;

macro_rules! id {
    ($($t:tt)*) => { $($t)* };
}

mod simple {
    use std::sync::Mutex;
    pub fn f() -> Mutex { Mutex::new(1) }
}
mod nested {
    use std::sync::{Arc, Mutex};
    pub fn f(_: Arc) -> Mutex { Mutex::new(2) }
}
mod renamed {
    use std::sync::Mutex as StdMutex;
    pub fn f() -> StdMutex { StdMutex::new(3) }
}
mod globbed {
    use std::sync::*;
    pub fn f(_: Arc) -> Mutex { Mutex::new(4) }
}
mod qualified {
    pub fn f() -> std::sync::Mutex { std::sync::Mutex::new(5) }
}
mod via_module {
    use std::sync;
    pub fn f() -> sync::Mutex { sync::Mutex::new(6) }
}
mod via_reexport {
    pub fn f() -> crate::locks::Mutex { crate::locks::Mutex::new(7) }
}
mod via_glob_of_ours {
    use crate::locks::*;
    pub fn f() -> Mutex { Mutex::new(8) }
}
mod via_prelude {
    pub fn f() -> Mutex { Mutex::new(9) }
}
mod in_macro {
    pub fn f() -> u32 { id!(std::sync::Mutex::new(10)).0 }
}
mod in_block {
    pub fn f() -> u32 {
        use std::sync::*;
        let _: Arc = Arc(0);
        Mutex::new(11).0
    }
}
mod unrelated {
    pub struct Mutex(pub u8);
    pub fn f() -> Mutex { Mutex(12) }
}
"#;

const LIB_AFTER: &str = r#"pub mod locks;

macro_rules! id {
    ($($t:tt)*) => { $($t)* };
}

mod simple {
    use parking_lot::Mutex;
    pub fn f() -> Mutex { Mutex::new(1) }
}
mod nested {
    use std::sync::Arc;
    use parking_lot::Mutex;
    pub fn f(_: Arc) -> Mutex { Mutex::new(2) }
}
mod renamed {
    use parking_lot::Mutex as StdMutex;
    pub fn f() -> StdMutex { StdMutex::new(3) }
}
mod globbed {
    use std::sync::*;
    use parking_lot::Mutex;
    pub fn f(_: Arc) -> Mutex { Mutex::new(4) }
}
mod qualified {
    pub fn f() -> parking_lot::Mutex { parking_lot::Mutex::new(5) }
}
mod via_module {
    use std::sync;
    pub fn f() -> parking_lot::Mutex { parking_lot::Mutex::new(6) }
}
mod via_reexport {
    pub fn f() -> crate::locks::Mutex { crate::locks::Mutex::new(7) }
}
mod via_glob_of_ours {
    use crate::locks::*;
    pub fn f() -> Mutex { Mutex::new(8) }
}
mod via_prelude {
    use parking_lot::Mutex;

    pub fn f() -> Mutex { Mutex::new(9) }
}
mod in_macro {
    pub fn f() -> u32 { id!(parking_lot::Mutex::new(10)).0 }
}
mod in_block {
    pub fn f() -> u32 {
        use std::sync::*;
        use parking_lot::Mutex;
        let _: Arc = Arc(0);
        Mutex::new(11).0
    }
}
mod unrelated {
    pub struct Mutex(pub u8);
    pub fn f() -> Mutex { Mutex(12) }
}
"#;

const LOCKS: &str = "pub use std::sync::Mutex;\n";

const APP: &str = r#"use lib::locks::Mutex;
use std::alias::Lock;

pub fn ours() -> Mutex { Mutex::new(20) }
pub fn theirs() -> std::sync::Mutex { std::sync::Mutex::new(21) }
pub fn aliased() -> Lock { Lock::new(22) }
"#;

const APP_AFTER: &str = r#"use lib::locks::Mutex;
use parking_lot::Mutex as Lock;

pub fn ours() -> Mutex { Mutex::new(20) }
pub fn theirs() -> parking_lot::Mutex { parking_lot::Mutex::new(21) }
pub fn aliased() -> Lock { Lock::new(22) }
"#;

const PATHS: [&str; 3] = ["lib.rs", "locks.rs", "app.rs"];

/// The view's files; std's and parking_lot's are outside it.
struct View {
    files: Vec<(FileId, String, String)>,
}

impl Files for View {
    fn file(&self, path: &str) -> Option<(FileId, &[u8])> {
        self.files.iter().find(|it| it.1 == path).map(|it| (it.0, it.2.as_bytes()))
    }

    fn path(&self, file: FileId) -> Option<&str> {
        self.files.iter().find(|it| it.0 == file).map(|it| &*it.1)
    }
}

/// A front end of `lib` (with `locks`) and `app`, `app` depending on `app_deps`, beside std and
/// parking_lot.
fn view(lib: &str, locks: &str, app: &str, app_deps: &str) -> (TestDB, View) {
    let fixture = format!(
        "{OUTSIDE}//- /lib.rs crate:lib deps:std,parking_lot edition:2021\n{lib}//- /locks.rs\n{locks}\
         //- /app.rs crate:app deps:{app_deps} edition:2021\n{app}"
    );
    let fx = fixture::db(&fixture);
    let ids = fx.files().to_vec();
    let db = fx.into_db();
    assert_eq!(ids.len(), 2 + PATHS.len());
    let files = ids[2..]
        .iter()
        .zip(PATHS)
        .map(|(file, path)| {
            let id = file.file_id(&db);
            (id, path.to_owned(), db.file_text(id).text(&db).to_string())
        })
        .collect();
    (db, View { files })
}

fn texts(view: &View) -> BTreeMap<String, String> {
    view.files.iter().map(|(_, path, text)| (path.clone(), text.clone())).collect()
}

fn replace(from: &str, to: &str) -> Edit {
    Edit::Replace { to: to.to_owned(), from: from.to_owned() }
}

/// The crate each path whose name is `Mutex`, `Lock` or `StdMutex` resolves to, in the view's
/// files and in what their macros expand, by file and module.
fn named(db: &TestDB, view: &View) -> Vec<(String, String)> {
    let sema = Semantics::new(db);
    let mut out = Vec::new();
    for (file, path, _) in &view.files {
        let root = sema.parse(sema.attach_first_edition(*file)).syntax();
        for token in root.descendants_with_tokens().filter_map(|it| it.into_token()) {
            if token.kind() != SyntaxKind::IDENT
                || !matches!(token.text(), "Mutex" | "Lock" | "StdMutex")
            {
                continue;
            }
            for mapped in sema.descend_into_macros_exact(token) {
                let Some(path_node) = mapped
                    .parent()
                    .filter(|it| it.kind() == SyntaxKind::NAME_REF)
                    .and_then(|it| it.parent())
                    .filter(|it| it.kind() == SyntaxKind::PATH_SEGMENT)
                    .and_then(|it| it.parent())
                    .and_then(ast::Path::cast)
                else {
                    continue;
                };
                let [ty, value] = names_in(&sema, path_node);
                let krate = ty
                    .or(value)
                    .and_then(|it| it.module(db))
                    .and_then(|it| it.krate(db).display_name(db))
                    .map_or_else(
                        || format!("unresolved `{}`", path_node.syntax().text()),
                        |it| it.to_string(),
                    );
                let module = token
                    .parent_ancestors()
                    .find_map(ast::Module::cast)
                    .and_then(|it| it.name())
                    .map_or_else(|| "(root)".to_owned(), |it| it.text().to_owned());
                out.push((format!("{path} {module}"), krate));
            }
        }
    }
    out
}

/// The errors of the view's crates that say a name names nothing or a value has the wrong
/// type, by kind.
fn errors(db: &TestDB) -> Vec<&'static str> {
    let mut all = Vec::new();
    for krate in Crate::all(db) {
        let name = krate.display_name(db).map(|it| it.to_string());
        if !matches!(name.as_deref(), Some("lib" | "app")) {
            continue;
        }
        for module in krate.modules(db) {
            module.diagnostics(db, &mut all, false);
        }
    }
    all.iter()
        .filter_map(|it| match it {
            AnyDiagnostic::UnresolvedImport(_) => Some("unresolved import"),
            AnyDiagnostic::UnresolvedPath(_) => Some("unresolved path"),
            AnyDiagnostic::UnresolvedIdent(_) => Some("unresolved ident"),
            AnyDiagnostic::UnresolvedMacroCall(_) => Some("unresolved macro call"),
            AnyDiagnostic::UnresolvedModule(_) => Some("unresolved module"),
            AnyDiagnostic::UnresolvedAssocItem(_) => Some("unresolved assoc item"),
            AnyDiagnostic::UnresolvedMethodCall(_) => Some("unresolved method call"),
            AnyDiagnostic::UnresolvedField(_) => Some("unresolved field"),
            AnyDiagnostic::TypeMismatch(_) => Some("type mismatch"),
            _ => None,
        })
        .collect()
}

#[test]
fn every_import_form_of_std_s_mutex_moves_to_parking_lot_s_and_resolves_there() {
    let (db, view) = view(LIB, LOCKS, APP, "lib,std,parking_lot");
    let before = texts(&view);
    assert_eq!(
        (before["lib.rs"].as_str(), before["locks.rs"].as_str(), before["app.rs"].as_str()),
        (LIB, LOCKS, APP)
    );
    assert_eq!(errors(&db), Vec::<&str>::new(), "the fixture is clean before the replace");
    let was = named(&db, &view);
    assert!(
        was.iter().all(|(at, krate)| if at.ends_with(" unrelated") {
            krate == "lib"
        } else {
            krate == "std"
        }),
        "{was:#?}"
    );

    let made = apply(&db, &view, &[replace("std::sync::Mutex", "parking_lot::Mutex")]).unwrap();
    let after = applied(&before, &made.edited.ops);
    assert_eq!(after["lib.rs"], LIB_AFTER);
    assert_eq!(after["locks.rs"], "pub use parking_lot::Mutex;\n");
    assert_eq!(after["app.rs"], APP_AFTER);
    let (edited, written) = roots(&before, &made.edited.ops, &after);
    assert_eq!(edited, written, "the edit's root is the text edits'");
    assert!(
        made.edited.ops.iter().all(|it| matches!(it, Op::Edit { .. })),
        "{:?}",
        made.edited.ops
    );
    for seed in made.seeds() {
        let (_, text) = after
            .iter()
            .find(|(path, _)| view.path(seed.file) == Some(path.as_str()))
            .expect("a seed is of a file of the view");
        assert_fresh(&seed.parse, text);
    }
    let touched: Vec<&str> = made.edited.items.iter().map(|it| it.file.as_str()).collect();
    for path in PATHS {
        assert!(touched.contains(&path), "{path} is touched: {:?}", made.edited.items);
    }

    let (next, next_view) =
        self::view(&after["lib.rs"], &after["locks.rs"], &after["app.rs"], "lib,std,parking_lot");
    assert_eq!(errors(&next), Vec::<&str>::new(), "the new text compiles");
    let now = named(&next, &next_view);
    assert!(
        now.iter().all(|(at, krate)| if at.ends_with(" unrelated") {
            krate == "lib"
        } else {
            krate == "parking_lot"
        }),
        "every name that named std's lock names parking_lot's: {now:#?}"
    );
    let again =
        apply(&next, &next_view, &[replace("std::sync::Mutex", "parking_lot::Mutex")]).unwrap();
    assert!(
        again.edited.ops.is_empty(),
        "a replaced view has nothing left to move: {:?}",
        again.edited.ops
    );
}

#[test]
fn a_crate_without_a_dependency_on_the_new_item_s_crate_is_an_error() {
    let (db, view) = view(LIB, LOCKS, APP, "lib,std");
    let error = apply(&db, &view, &[replace("std::sync::Mutex", "parking_lot::Mutex")])
        .err()
        .expect("an error");
    assert!(error.what.contains("app.rs"), "{}", error.what);
    assert!(error.what.contains("no dependency on `parking_lot`"), "{}", error.what);
}

#[test]
fn a_name_in_a_macro_rules_body_is_an_error_naming_its_line() {
    let lib = "pub mod locks;\n\nmacro_rules! make {\n    () => { std::sync::Mutex::new(0) };\n}\n";
    let (db, view) = view(lib, LOCKS, "", "lib,std,parking_lot");
    let error = apply(&db, &view, &[replace("std::sync::Mutex", "parking_lot::Mutex")])
        .err()
        .expect("an error");
    assert!(error.what.starts_with("lib.rs:4: "), "{}", error.what);
    assert!(error.what.contains("macro_rules!"), "{}", error.what);
}

#[test]
fn a_def_path_that_names_nothing_or_a_module_is_an_error() {
    let (db, view) = view(LIB, LOCKS, APP, "lib,std,parking_lot");
    for (from, to, wanted) in [
        ("std::sync::Nothing", "parking_lot::Mutex", "names nothing in crate `std`"),
        ("std::sync", "parking_lot::Mutex", "names a module"),
        ("nowhere::Mutex", "parking_lot::Mutex", "no crate of the build is named `nowhere`"),
        ("std::sync::Mutex", "std::sync::Mutex", "name the same item"),
        ("crate::locks::Mutex", "parking_lot::Mutex", "starts at `crate`"),
    ] {
        let error = apply(&db, &view, &[replace(from, to)]).err().expect("an error");
        assert!(error.what.contains(wanted), "{from} -> {to}: {}", error.what);
    }
}

#[test]
fn the_replaces_of_one_request_share_a_use_list_and_are_made_in_one_pass() {
    let lib = "pub mod locks;\n\nuse std::sync::{Arc, Mutex, RwLock};\n\npub fn f(_: Arc) -> (Mutex, RwLock) {\n    (Mutex::new(1), std::sync::RwLock::new(2))\n}\n";
    let (db, view) = view(lib, LOCKS, "", "lib,std,parking_lot");
    let before = texts(&view);
    let both = [
        replace("std::sync::Mutex", "parking_lot::Mutex"),
        replace("std::sync::RwLock", "parking_lot::RwLock"),
    ];
    let made = apply(&db, &view, &both).unwrap();
    let after = applied(&before, &made.edited.ops);
    assert_eq!(
        after["lib.rs"],
        "pub mod locks;\n\nuse std::sync::Arc;\nuse parking_lot::Mutex;\nuse parking_lot::RwLock;\n\npub fn f(_: Arc) -> (Mutex, RwLock) {\n    (Mutex::new(1), parking_lot::RwLock::new(2))\n}\n"
    );
    assert_eq!(after["locks.rs"], "pub use parking_lot::Mutex;\n");
    let (next, next_view) =
        self::view(&after["lib.rs"], &after["locks.rs"], "", "lib,std,parking_lot");
    assert_eq!(errors(&next), Vec::<&str>::new(), "the new text compiles");
    let again = apply(&next, &next_view, &both).unwrap();
    assert!(again.edited.ops.is_empty(), "nothing left to move: {:?}", again.edited.ops);
}

#[test]
fn an_item_replaced_twice_or_a_chain_of_replaces_is_an_error() {
    let (db, view) = view(LIB, LOCKS, APP, "lib,std,parking_lot");
    for (edits, wanted) in [
        (
            [
                replace("std::sync::Mutex", "parking_lot::Mutex"),
                replace("std::alias::Lock", "parking_lot::RwLock"),
            ],
            "replaced twice in one request",
        ),
        (
            [
                replace("std::sync::Mutex", "parking_lot::Mutex"),
                replace("parking_lot::Mutex", "std::sync::RwLock"),
            ],
            "chain",
        ),
    ] {
        let error = apply(&db, &view, &edits).err().expect("an error");
        assert!(error.what.contains(wanted), "{edits:?}: {}", error.what);
    }
}
