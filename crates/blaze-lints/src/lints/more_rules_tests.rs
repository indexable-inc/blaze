//! What the lints of `no-fallback-front-lints-clippy` run over in their tests: an `std` (and an
//! `alloc`, `lock_api` and `parking_lot`) with the shapes those lints look at, a run of one lint
//! alone, and the fix of one lint applied and checked clean (`blaze_lints::fix`).

use base_db::FileId;
use blaze_hash::HashMap;
use hir::attach_db;
use hir_expand::fixture;

use crate::{
    check::{Setup, check},
    conf::Conf,
    fix::fix,
    level::CommandLine,
    tests::lint,
};

const MINICORE: &str = "//- minicore: sized, copy, clone, option, result, fn, future, send, sync, drop, iterators, try, deref, deref_mut, builtin_impls, unary_ops, eq, add, panic\n";

/// Locks that poison (std's) and that don't (`lock_api`'s, behind `parking_lot`'s alias), `Rc`
/// and `Arc` in `alloc` with std's re-exports, hash maps and their iterators where std defines
/// them (`collections::hash::{map, set}`), `Result`'s `ok`, `map_err`, `expect` and `unwrap`
/// (minicore's `Result` has none without `fmt`), and the panicking macros.
pub(super) const CRATES: &str = r#"
//- /alloc/lib.rs crate:alloc
pub mod rc {
    #[rustc_diagnostic_item = "Rc"]
    pub struct Rc<T: ?Sized> { ptr: *const T }
    impl<T> Rc<T> {
        pub fn new(value: T) -> Rc<T> { loop {} }
    }
}
pub mod sync {
    #[rustc_diagnostic_item = "Arc"]
    pub struct Arc<T: ?Sized> { ptr: *const T }
    unsafe impl<T: ?Sized + core::marker::Send + core::marker::Sync> core::marker::Send for Arc<T> {}
    unsafe impl<T: ?Sized + core::marker::Send + core::marker::Sync> core::marker::Sync for Arc<T> {}
    impl<T> Arc<T> {
        pub fn new(value: T) -> Arc<T> { loop {} }
    }
}
//- /std/lib.rs crate:std deps:alloc
pub mod prelude {
    pub mod v1 {
        pub use crate::{assert, assert_eq, assert_ne, panic, todo};
        pub use crate::result::{OptionExt, ResultExt};
        pub use core::prelude::v1::*;
    }
    pub mod rust_2015 { pub use super::v1::*; }
    pub mod rust_2018 { pub use super::v1::*; }
    pub mod rust_2021 { pub use super::v1::*; }
    pub mod rust_2024 { pub use super::v1::*; }
}
#[macro_export]
macro_rules! panic { ($($t:tt)*) => { loop {} }; }
#[macro_export]
macro_rules! assert { ($c:expr $(, $($t:tt)*)?) => { if !$c { loop {} } }; }
#[macro_export]
macro_rules! assert_eq { ($a:expr, $b:expr $(, $($t:tt)*)?) => { if $a != $b { loop {} } }; }
#[macro_export]
macro_rules! assert_ne { ($a:expr, $b:expr $(, $($t:tt)*)?) => { if $a == $b { loop {} } }; }
#[macro_export]
macro_rules! todo { () => { loop {} }; }
pub mod result {
    pub trait ResultExt<T, E> {
        fn ok(self) -> Option<T>;
        fn map_err<F, O: FnOnce(E) -> F>(self, op: O) -> Result<T, F>;
        fn expect(self, msg: &str) -> T;
        fn unwrap(self) -> T;
    }
    impl<T, E> ResultExt<T, E> for Result<T, E> {
        fn ok(self) -> Option<T> { loop {} }
        fn map_err<F, O: FnOnce(E) -> F>(self, op: O) -> Result<T, F> { loop {} }
        fn expect(self, msg: &str) -> T { loop {} }
        fn unwrap(self) -> T { loop {} }
    }
    pub trait OptionExt<T> {
        fn expect(self, msg: &str) -> T;
    }
    impl<T> OptionExt<T> for Option<T> {
        fn expect(self, msg: &str) -> T { loop {} }
    }
}
pub mod process {
    pub fn exit(code: i32) -> ! { loop {} }
}
pub mod rc {
    pub use alloc::rc::Rc;
}
pub mod sync {
    pub use alloc::sync::Arc;
    pub struct LockResult<T>(T);
    impl<T> LockResult<T> {
        pub fn unwrap(self) -> T { self.0 }
        pub fn expect(self, msg: &str) -> T { self.0 }
    }
    #[rustc_diagnostic_item = "Mutex"]
    pub struct Mutex<T: ?Sized> { value: T }
    unsafe impl<T: ?Sized + core::marker::Send> core::marker::Send for Mutex<T> {}
    unsafe impl<T: ?Sized + core::marker::Send> core::marker::Sync for Mutex<T> {}
    impl<T> Mutex<T> {
        pub const fn new(value: T) -> Mutex<T> { Mutex { value } }
        pub fn lock(&self) -> LockResult<MutexGuard<'_, T>> { loop {} }
    }
    pub struct MutexGuard<'a, T: ?Sized> { lock: &'a Mutex<T> }
    impl<T: ?Sized> core::ops::Deref for MutexGuard<'_, T> {
        type Target = T;
        fn deref(&self) -> &T { loop {} }
    }
    impl<T: ?Sized> core::ops::DerefMut for MutexGuard<'_, T> {
        fn deref_mut(&mut self) -> &mut T { loop {} }
    }
    pub mod atomic {
        pub struct AtomicBool(bool);
        pub struct AtomicU8(u8);
        pub struct AtomicU32(u32);
        pub struct AtomicUsize(usize);
        pub enum Ordering { Relaxed, SeqCst }
        impl AtomicBool {
            pub const fn new(v: bool) -> AtomicBool { AtomicBool(v) }
            pub fn load(&self, order: Ordering) -> bool { self.0 }
            pub fn store(&self, v: bool, order: Ordering) {}
        }
        impl AtomicU8 {
            pub const fn new(v: u8) -> AtomicU8 { AtomicU8(v) }
            pub fn load(&self, order: Ordering) -> u8 { self.0 }
            pub fn store(&self, v: u8, order: Ordering) {}
        }
        impl AtomicU32 {
            pub const fn new(v: u32) -> AtomicU32 { AtomicU32(v) }
            pub fn load(&self, order: Ordering) -> u32 { self.0 }
            pub fn store(&self, v: u32, order: Ordering) {}
        }
        impl AtomicUsize {
            pub const fn new(v: usize) -> AtomicUsize { AtomicUsize(v) }
            pub fn load(&self, order: Ordering) -> usize { self.0 }
            pub fn store(&self, v: usize, order: Ordering) {}
        }
    }
}
pub mod hash {
    pub struct RandomState;
}
pub mod collections {
    mod hash {
        pub mod map {
            #[rustc_diagnostic_item = "HashMap"]
            pub struct HashMap<K, V, S = crate::hash::RandomState> { k: *const K, v: *const V, s: S }
            impl<K, V> HashMap<K, V> {
                pub fn new() -> HashMap<K, V> { loop {} }
            }
            impl<K, V, S> HashMap<K, V, S> {
                pub fn iter(&self) -> Iter<'_, K, V> { loop {} }
                pub fn keys(&self) -> Keys<'_, K, V> { loop {} }
            }
            #[rustc_diagnostic_item = "hashmap_iter_ty"]
            pub struct Iter<'a, K, V> { m: &'a (K, V) }
            impl<'a, K, V> core::iter::Iterator for Iter<'a, K, V> {
                type Item = (&'a K, &'a V);
                fn next(&mut self) -> Option<(&'a K, &'a V)> { None }
            }
            #[rustc_diagnostic_item = "hashmap_keys_ty"]
            pub struct Keys<'a, K, V> { m: &'a (K, V) }
            impl<'a, K, V> core::iter::Iterator for Keys<'a, K, V> {
                type Item = &'a K;
                fn next(&mut self) -> Option<&'a K> { None }
            }
            impl<'a, K, V, S> core::iter::IntoIterator for &'a HashMap<K, V, S> {
                type Item = (&'a K, &'a V);
                type IntoIter = Iter<'a, K, V>;
                fn into_iter(self) -> Iter<'a, K, V> { loop {} }
            }
        }
        pub mod set {
            #[rustc_diagnostic_item = "HashSet"]
            pub struct HashSet<T, S = crate::hash::RandomState> { t: *const T, s: S }
            impl<T> HashSet<T> {
                pub fn new() -> HashSet<T> { loop {} }
            }
            impl<T, S> HashSet<T, S> {
                pub fn iter(&self) -> Iter<'_, T> { loop {} }
            }
            #[rustc_diagnostic_item = "hashset_iter_ty"]
            pub struct Iter<'a, T> { t: &'a T }
            impl<'a, T> core::iter::Iterator for Iter<'a, T> {
                type Item = &'a T;
                fn next(&mut self) -> Option<&'a T> { None }
            }
        }
    }
    pub use self::hash::{map::HashMap, set::HashSet};
    pub mod hash_map { pub use super::hash::map::*; }
    pub mod hash_set { pub use super::hash::set::*; }
    pub struct BTreeMap<K, V> { k: *const K, v: *const V }
    impl<K, V> BTreeMap<K, V> {
        pub fn iter(&self) -> BTreeIter<'_, K, V> { loop {} }
    }
    pub struct BTreeIter<'a, K, V> { m: &'a (K, V) }
    impl<'a, K, V> core::iter::Iterator for BTreeIter<'a, K, V> {
        type Item = (&'a K, &'a V);
        fn next(&mut self) -> Option<(&'a K, &'a V)> { None }
    }
}
//- /lock_api/lib.rs crate:lock_api
pub struct Mutex<R, T: ?Sized> { raw: R, value: T }
impl<R, T> Mutex<R, T> {
    pub const fn new(value: T) -> Mutex<R, T> { loop {} }
    pub fn lock(&self) -> MutexGuard<'_, R, T> { loop {} }
}
pub struct MutexGuard<'a, R, T: ?Sized> { lock: &'a Mutex<R, T> }
impl<R, T: ?Sized> core::ops::Deref for MutexGuard<'_, R, T> {
    type Target = T;
    fn deref(&self) -> &T { loop {} }
}
impl<R, T: ?Sized> core::ops::DerefMut for MutexGuard<'_, R, T> {
    fn deref_mut(&mut self) -> &mut T { loop {} }
}
//- /parking_lot/lib.rs crate:parking_lot deps:lock_api
pub struct RawMutex;
pub type Mutex<T> = lock_api::Mutex<RawMutex, T>;
"#;

const MAIN: &str = "//- /src/lib.rs crate:main deps:std,alloc,lock_api,parking_lot\n";

/// The fixture of the crate `main` written `main`, with [`CRATES`].
pub(super) fn fixture(main: &str) -> String {
    format!("{MINICORE}{CRATES}{MAIN}{}", main.trim_start_matches('\n'))
}

fn only(lint: &str) -> String {
    format!("-A clippy::all -W clippy::{lint}")
}

/// What `clippy::<lint>` alone says of `main`, each diagnostic as rustc writes it.
pub(super) fn lint_only(lint_name: &str, main: &str) -> String {
    lint(&fixture(main), &only(lint_name))
}

/// `src/lib.rs` of `main` as the fix of `clippy::<lint>` alone leaves it, after checking
/// that the fix changed it and that linting it again finds nothing.
pub(super) fn fixed(lint_name: &str, main: &str) -> String {
    let fx = fixture::db(&fixture(main));
    let file = *fx.files().last().expect("src/lib.rs");
    let mut db = fx.into_db();
    let krate = attach_db(&db, || crate::tests::main_crate(&db));
    let file = file.file_id(&db);
    let before = db.file_text(file).text(&db).to_string();
    let mut names: HashMap<FileId, String> = HashMap::default();
    names.insert(file, "src/lib.rs".to_owned());
    let name_of = |file: FileId| names.get(&file).cloned();
    let flags: Vec<String> = only(lint_name).split_whitespace().map(str::to_owned).collect();
    let setup = Setup { command_line: CommandLine::parse(&flags).expect("the flags read"), conf: Conf::default() };
    let made = fix(&mut db, krate, &setup, &name_of).unwrap_or_else(|error| panic!("{error}"));
    let after = db.file_text(file).text(&db).to_string();
    assert_ne!(after, before, "the fix of `{lint_name}` changed the file");
    assert!(made.applied > 0, "the fix of `{lint_name}` made a suggestion");
    attach_db(&db, || {
        let found = check(&db, krate, &setup, &name_of).expect("the file is named");
        let left: Vec<&str> = found.iter().map(|it| it.diagnostic.message.as_str()).collect();
        assert!(left.is_empty(), "the fixed file lints clean:\n{after}\n{left:#?}");
    });
    after
}
