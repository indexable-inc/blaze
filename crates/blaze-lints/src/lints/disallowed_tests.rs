//! `blaze_lints::fix` over `clippy.toml`'s `disallowed-*` entries with a replacement: for each import
//! form of std's locks and channels and of `rustc_hash`'s maps, the fixed file is the text the
//! test writes, front-end clean, each name that named a disallowed item names its replacement,
//! and linting it again finds nothing.

use base_db::FileId;
use blaze_hash::HashMap;
use hir::{attach_db, view::Semantics};
use hir_expand::fixture;
use syntax::view::{self as ast, AstNode as _};

use crate::{
    check::{Setup, check},
    conf::Conf,
    fix::{FixError, fix},
    level::CommandLine,
};

const FLAGS: &str =
    "-A clippy::all --warn=clippy::disallowed_types --warn=clippy::disallowed_methods --warn=clippy::disallowed_macros";

/// The workspace's moves (`.clippy.toml`), as entries of the fixture's crates.
const CLIPPY_TOML: &str = r#"
disallowed-types = [
    { path = "std::sync::Mutex", replacement = "parking_lot::Mutex", reason = "no poisoning to handle" },
    { path = "std::sync::RwLock", replacement = "parking_lot::RwLock" },
    { path = "std::sync::Condvar", replacement = "parking_lot::Condvar" },
    { path = "std::sync::mpsc::Sender", replacement = "crossbeam_channel::Sender" },
    { path = "std::sync::mpsc::Receiver", replacement = "crossbeam_channel::Receiver" },
    { path = "rustc_hash::FxHashMap", replacement = "blaze_hash::HashMap" },
]
disallowed-methods = [
    { path = "std::sync::mpsc::channel", replacement = "crossbeam_channel::unbounded" },
]
"#;

/// An `std` whose locks poison and whose `Condvar` takes the guard, and the crates the ports
/// move to, with the shapes of the real ones.
const CRATES: &str = r#"
//- minicore: sized, copy, clone, fn
//- /std/lib.rs crate:std
pub mod prelude {
    pub mod rust_2015 { pub use core::prelude::rust_2015::*; }
    pub mod rust_2018 { pub use core::prelude::rust_2018::*; }
    pub mod rust_2021 { pub use core::prelude::rust_2021::*; }
    pub mod rust_2024 { pub use core::prelude::rust_2024::*; }
}
pub mod sync {
    pub struct Arc<T>(T);
    pub struct LockResult<T>(T);
    impl<T> LockResult<T> {
        pub fn unwrap(self) -> T { self.0 }
        pub fn expect(self, _: &str) -> T { self.0 }
        pub fn is_poisoned_result(&self) -> bool { false }
    }
    pub struct Mutex<T>(T);
    impl<T> Mutex<T> {
        pub const fn new(value: T) -> Mutex<T> { Mutex(value) }
        pub fn lock(&self) -> LockResult<MutexGuard<'_, T>> { LockResult(MutexGuard(&self.0)) }
        pub fn into_inner(self) -> LockResult<T> { LockResult(self.0) }
    }
    pub struct MutexGuard<'a, T>(&'a T);
    pub struct RwLock<T>(T);
    impl<T> RwLock<T> {
        pub const fn new(value: T) -> RwLock<T> { RwLock(value) }
        pub fn read(&self) -> LockResult<RwLockReadGuard<'_, T>> { LockResult(RwLockReadGuard(&self.0)) }
        pub fn write(&self) -> LockResult<RwLockWriteGuard<'_, T>> { LockResult(RwLockWriteGuard(&self.0)) }
    }
    pub struct RwLockReadGuard<'a, T>(&'a T);
    pub struct RwLockWriteGuard<'a, T>(&'a T);
    pub struct Condvar;
    impl Condvar {
        pub const fn new() -> Condvar { Condvar }
        pub fn wait<'a, T>(&self, guard: MutexGuard<'a, T>) -> LockResult<MutexGuard<'a, T>> { LockResult(guard) }
        pub fn notify_one(&self) {}
    }
    pub mod mpsc {
        pub struct Sent;
        impl Sent {
            pub fn unwrap(self) {}
        }
        pub struct Sender<T>(T);
        impl<T> Sender<T> {
            pub fn send(&self, _: T) -> Sent { Sent }
        }
        pub struct Receiver<T>(T);
        pub fn channel<T>() -> (Sender<T>, Receiver<T>) { loop {} }
    }
}
pub mod collections {
    pub struct HashMap<K, V, S>(K, V, S);
}
//- /parking_lot/lib.rs crate:parking_lot
pub struct Mutex<T>(T);
impl<T> Mutex<T> {
    pub const fn new(value: T) -> Mutex<T> { Mutex(value) }
    pub fn lock(&self) -> MutexGuard<'_, T> { MutexGuard(&self.0) }
    pub fn into_inner(self) -> T { self.0 }
}
pub struct MutexGuard<'a, T>(&'a T);
pub struct RwLock<T>(T);
impl<T> RwLock<T> {
    pub const fn new(value: T) -> RwLock<T> { RwLock(value) }
    pub fn read(&self) -> RwLockReadGuard<'_, T> { RwLockReadGuard(&self.0) }
    pub fn write(&self) -> RwLockWriteGuard<'_, T> { RwLockWriteGuard(&self.0) }
}
pub struct RwLockReadGuard<'a, T>(&'a T);
pub struct RwLockWriteGuard<'a, T>(&'a T);
pub struct Condvar;
impl Condvar {
    pub const fn new() -> Condvar { Condvar }
    pub fn wait<T>(&self, _: &mut MutexGuard<'_, T>) {}
    pub fn notify_one(&self) {}
}
//- /crossbeam_channel/lib.rs crate:crossbeam_channel
pub struct Sent;
impl Sent {
    pub fn unwrap(self) {}
}
pub struct Sender<T>(T);
impl<T> Sender<T> {
    pub fn send(&self, _: T) -> Sent { Sent }
}
pub struct Receiver<T>(T);
pub fn unbounded<T>() -> (Sender<T>, Receiver<T>) { loop {} }
//- /rustc_hash/lib.rs crate:rustc_hash deps:std
pub struct FxBuildHasher;
pub type FxHashMap<K, V> = std::collections::HashMap<K, V, FxBuildHasher>;
//- /blaze_hash/lib.rs crate:blaze_hash deps:std
pub struct FixedState;
pub type HashMap<K, V> = std::collections::HashMap<K, V, FixedState>;
"#;

const MAIN: &str = "//- /src/lib.rs crate:main deps:std,parking_lot,crossbeam_channel,rustc_hash,blaze_hash\n";

/// The ported items' names, and the crates the fixed file must resolve them to.
const MOVED: [(&str, &str); 9] = [
    ("Mutex", "parking_lot"),
    ("M", "parking_lot"),
    ("RwLock", "parking_lot"),
    ("Condvar", "parking_lot"),
    ("Sender", "crossbeam_channel"),
    ("Receiver", "crossbeam_channel"),
    ("unbounded", "crossbeam_channel"),
    ("HashMap", "blaze_hash"),
    ("FxHashMap", "blaze_hash"),
];

fn setup() -> Setup {
    let flags: Vec<String> = FLAGS.split_whitespace().map(str::to_owned).collect();
    let table = CLIPPY_TOML.parse().expect("the configuration reads");
    Setup { command_line: CommandLine::parse(&flags).expect("the flags read"), conf: Conf { table, ..Conf::default() } }
}

/// `src/lib.rs` of `main` as the fix leaves it, after checking it is front-end clean, names
/// what it moved by the counterparts, and lints clean again.
fn fixed(main: &str) -> Result<String, FixError> {
    let fx = fixture::db(&format!("{CRATES}{MAIN}{}", main.trim_start_matches('\n')));
    let file = *fx.files().last().expect("src/lib.rs");
    let mut db = fx.into_db();
    let krate = attach_db(&db, || {
        hir::Crate::all(&db)
            .into_iter()
            .find(|it| it.display_name(&db).is_some_and(|it| it.to_string() == "main"))
            .expect("the fixture has `main`")
    });
    let file = file.file_id(&db);
    let mut names: HashMap<FileId, String> = HashMap::default();
    names.insert(file, "src/lib.rs".to_owned());
    let name_of = |file: FileId| names.get(&file).cloned();
    let setup = setup();
    let made = fix(&mut db, krate, &setup, &name_of)?;
    let text = db.file_text(file).text(&db).to_string();
    assert!(made.files.iter().all(|(it, new)| *it == file && *new == text), "the fix names the file it changed");
    attach_db(&db, || {
        let mut acc = Vec::new();
        for module in krate.modules(&db) {
            module.diagnostics(&db, &mut acc, false);
        }
        let errors: Vec<String> = acc.iter().map(|it| format!("{it:?}")).filter(|it| !it.starts_with("InactiveCode")).collect();
        assert!(errors.is_empty(), "the fixed file is front-end clean:\n{text}\n{errors:#?}");
        let found = check(&db, krate, &setup, &name_of).expect("the file is named");
        let left: Vec<(&str, Vec<&str>)> = found
            .iter()
            .map(|it| (it.diagnostic.message.as_str(), it.diagnostic.children.iter().map(|it| it.message.as_str()).collect()))
            .collect();
        assert!(left.is_empty(), "the fixed file lints clean:\n{text}\n{left:#?}");
        let sema = Semantics::new(&db);
        let editioned = sema.attach_first_edition(file);
        let root = sema.parse(editioned).syntax();
        for path in root.descendants().filter_map(ast::Path::cast) {
            let Some(name) = path.segment().and_then(|it| it.name_ref()) else { continue };
            let Some(&(_, want)) = MOVED.iter().find(|(it, _)| *it == name.text()) else { continue };
            let [ty, value] = blaze_intent::retarget::names_in(&sema, path);
            let krate = ty.or(value).and_then(|it| it.module(&db)).and_then(|it| it.krate(&db).display_name(&db));
            assert_eq!(krate.map(|it| it.to_string()).as_deref(), Some(want), "`{}` in:\n{text}", path.syntax().text());
        }
    });
    Ok(text)
}

fn ports(main: &str, want: &str) {
    let after = fixed(main).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(after, want.trim_start_matches('\n'));
    assert_eq!(fixed(&after).unwrap(), after, "a fixed file has nothing left to fix");
}

#[test]
fn a_single_import_of_a_lock_and_its_unwrapped_lock_move_to_parking_lot() {
    ports(
        r#"
use std::sync::Mutex;
pub fn f(m: &Mutex<u8>) -> u8 {
    let _g = m.lock().unwrap();
    let _h = m.lock().expect("held");
    0
}
"#,
        r#"
use parking_lot::Mutex;
pub fn f(m: &Mutex<u8>) -> u8 {
    let _g = m.lock();
    let _h = m.lock();
    0
}
"#,
    );
}

#[test]
fn a_split_import_keeps_what_does_not_move() {
    ports(
        r#"
use std::sync::{Arc, Mutex};
pub fn f(_: Arc<Mutex<u8>>) {}
"#,
        r#"
use std::sync::Arc;
use parking_lot::Mutex;
pub fn f(_: Arc<Mutex<u8>>) {}
"#,
    );
}

#[test]
fn a_renamed_import_keeps_its_name() {
    ports(
        r#"
use std::sync::Mutex as M;
pub fn f(m: M<u8>) -> u8 {
    m.into_inner().unwrap()
}
"#,
        r#"
use parking_lot::Mutex as M;
pub fn f(m: M<u8>) -> u8 {
    m.into_inner()
}
"#,
    );
}

#[test]
fn a_glob_import_gets_the_moved_names_beside_it() {
    ports(
        r#"
use std::sync::*;
pub fn f(_: Mutex<u8>, _: Arc<u8>) {}
"#,
        r#"
use std::sync::*;
use parking_lot::Mutex;
pub fn f(_: Mutex<u8>, _: Arc<u8>) {}
"#,
    );
}

#[test]
fn qualified_paths_and_paths_through_a_module_move() {
    ports(
        r#"
pub static LOCK: std::sync::Mutex<u8> = std::sync::Mutex::new(0);
mod through {
    use std::sync;
    pub fn f(l: &sync::RwLock<u8>) {
        let _r = l.read().unwrap();
        let _w = l.write().unwrap();
    }
}
"#,
        r#"
pub static LOCK: parking_lot::Mutex<u8> = parking_lot::Mutex::new(0);
mod through {
    use std::sync;
    pub fn f(l: &parking_lot::RwLock<u8>) {
        let _r = l.read();
        let _w = l.write();
    }
}
"#,
    );
}

#[test]
fn a_nested_tree_with_attributes_splits_into_one_line_each() {
    ports(
        r#"
#[cfg(all())]
pub(crate) use std::{sync::{Arc, Condvar, Mutex}, sync::RwLock};
pub fn f(_: Arc<u8>, _: Condvar, _: Mutex<u8>, _: RwLock<u8>) {}
"#,
        r#"
#[cfg(all())]
pub(crate) use std::sync::Arc;
#[cfg(all())]
pub(crate) use parking_lot::Condvar;
#[cfg(all())]
pub(crate) use parking_lot::Mutex;
#[cfg(all())]
pub(crate) use parking_lot::RwLock;
pub fn f(_: Arc<u8>, _: Condvar, _: Mutex<u8>, _: RwLock<u8>) {}
"#,
    );
}

#[test]
fn a_condvar_wait_borrows_the_guard() {
    ports(
        r#"
use std::sync::{Condvar, Mutex};
pub fn f(m: &Mutex<bool>, cv: &Condvar) {
    let mut g = m.lock().unwrap();
    g = cv.wait(g).unwrap();
    cv.notify_one();
    let _ = g;
}
"#,
        r#"
use parking_lot::Condvar;
use parking_lot::Mutex;
pub fn f(m: &Mutex<bool>, cv: &Condvar) {
    let mut g = m.lock();
    cv.wait(&mut g);
    cv.notify_one();
    let _ = g;
}
"#,
    );
}

#[test]
fn std_channels_move_to_crossbeam_channel() {
    ports(
        r#"
use std::sync::mpsc::{Receiver, Sender, channel};
pub fn f() -> (Sender<u8>, Receiver<u8>) {
    let (tx, rx) = channel();
    tx.send(1).unwrap();
    (tx, rx)
}
"#,
        r#"
use crossbeam_channel::Receiver;
use crossbeam_channel::Sender;
use crossbeam_channel::unbounded as channel;
pub fn f() -> (Sender<u8>, Receiver<u8>) {
    let (tx, rx) = channel();
    tx.send(1).unwrap();
    (tx, rx)
}
"#,
    );
}

#[test]
fn rustc_hash_maps_move_to_blaze_hash() {
    ports(
        r#"
use rustc_hash::FxHashMap;
pub fn f(_: FxHashMap<u8, u8>, _: rustc_hash::FxHashMap<u8, u8>) {}
"#,
        r#"
use blaze_hash::HashMap as FxHashMap;
pub fn f(_: FxHashMap<u8, u8>, _: blaze_hash::HashMap<u8, u8>) {}
"#,
    );
}

#[test]
fn fixes_that_leave_an_error_are_refused_naming_it_and_the_text_kept() {
    let main = r#"
use std::sync::Mutex;
pub fn f(m: &Mutex<u8>) -> bool {
    m.lock().is_poisoned_result()
}
"#;
    let fx = fixture::db(&format!("{CRATES}{MAIN}{}", main.trim_start_matches('\n')));
    let file = *fx.files().last().expect("src/lib.rs");
    let mut db = fx.into_db();
    let krate = attach_db(&db, || {
        hir::Crate::all(&db)
            .into_iter()
            .find(|it| it.display_name(&db).is_some_and(|it| it.to_string() == "main"))
            .expect("the fixture has `main`")
    });
    let file = file.file_id(&db);
    let names: HashMap<FileId, String> = [(file, "src/lib.rs".to_owned())].into_iter().collect();
    let name_of = |file: FileId| names.get(&file).cloned();
    let error = fix(&mut db, krate, &setup(), &name_of).unwrap_err();
    let FixError::Broke { at, by, .. } = &error else { panic!("{error}") };
    assert_eq!(at, "src/lib.rs:3", "{error}");
    assert!(!by.is_empty(), "{error}");
    assert_eq!(db.file_text(file).text(&db).as_str(), main.trim_start_matches('\n'), "the text is as it was");
}
