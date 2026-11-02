//! What the lints of held guards and shared state run over in their tests
//! (`significant_drop_in_scrutinee`, `significant_drop_tightening`, `await_holding_lock`,
//! `await_holding_refcell_ref`, `blaze::shared_collection_lock`, `blaze::open_wait_loop`): an
//! `std` whose guards, locks, maps, `Arc` and `Vec` are known as std's are, by their diagnostic
//! items, and drop as std's do, `RefCell`'s borrows, `lock_api`'s guards behind `parking_lot`'s
//! alias, std's sleeps, spins, clock and lazy cells, `blaze_graph::par`, `papaya`, `hashbrown`,
//! `indexmap`, `once_cell`, and a run of some lints alone.

use crate::tests::lint;

const MINICORE: &str = "//- minicore: sized, copy, clone, option, result, fn, future, send, sync, drop, iterators, deref, deref_mut, builtin_impls, unary_ops, eq, ord, add, panic\n";

const CRATES: &str = r#"
//- /alloc/lib.rs crate:alloc
pub mod sync {
    #[rustc_diagnostic_item = "Arc"]
    pub struct Arc<T: ?Sized> { ptr: *const T }
    impl<T> Arc<T> {
        pub fn new(value: T) -> Arc<T> { loop {} }
    }
    impl<T: ?Sized> core::ops::Deref for Arc<T> {
        type Target = T;
        fn deref(&self) -> &T { loop {} }
    }
    impl<T: ?Sized> Clone for Arc<T> {
        fn clone(&self) -> Arc<T> { loop {} }
    }
}
pub mod collections {
    #[rustc_diagnostic_item = "BTreeMap"]
    pub struct BTreeMap<K, V> { k: *const K, v: *const V }
    #[rustc_diagnostic_item = "BTreeSet"]
    pub struct BTreeSet<T> { t: *const T }
}
pub mod vec {
    #[rustc_diagnostic_item = "Vec"]
    pub struct Vec<T> { ptr: *const T, len: usize }
    impl<T> Vec<T> {
        pub const fn new() -> Vec<T> { loop {} }
        pub fn push(&mut self, value: T) {}
        pub fn pop(&mut self) -> Option<T> { None }
        pub fn len(&self) -> usize { self.len }
        pub fn first(&self) -> Option<&T> { None }
    }
}
//- /std/lib.rs crate:std deps:alloc
pub mod prelude {
    pub mod v1 {
        pub use alloc::vec::Vec;
        pub use core::prelude::v1::*;
    }
    pub mod rust_2015 { pub use super::v1::*; }
    pub mod rust_2018 { pub use super::v1::*; }
    pub mod rust_2021 { pub use super::v1::*; }
    pub mod rust_2024 { pub use super::v1::*; }
}
pub mod mem {
    pub fn drop<T>(_x: T) {}
}
pub mod hash {
    pub struct RandomState;
}
pub mod collections {
    #[rustc_diagnostic_item = "HashMap"]
    pub struct HashMap<K, V, S = crate::hash::RandomState> { k: *const K, v: *const V, s: S }
    #[rustc_diagnostic_item = "HashSet"]
    pub struct HashSet<T, S = crate::hash::RandomState> { t: *const T, s: S }
    pub use alloc::collections::{BTreeMap, BTreeSet};
}
pub mod time {
    pub struct Duration(u64);
    impl Duration {
        pub const fn from_millis(ms: u64) -> Duration { Duration(ms) }
    }
    impl core::cmp::PartialEq for Duration {
        fn eq(&self, other: &Duration) -> bool { self.0 == other.0 }
    }
    impl core::cmp::PartialOrd for Duration {
        fn partial_cmp(&self, other: &Duration) -> Option<core::cmp::Ordering> { None }
    }
    pub struct Instant(u64);
    impl Instant {
        pub fn now() -> Instant { Instant(0) }
        pub fn elapsed(&self) -> Duration { Duration(0) }
    }
}
pub mod thread {
    pub fn sleep(duration: crate::time::Duration) {}
    pub fn yield_now() {}
}
pub mod hint {
    pub fn spin_loop() {}
}
pub mod sync {
    pub use alloc::sync::Arc;
    pub struct LockResult<T>(T);
    impl<T> LockResult<T> {
        pub fn unwrap(self) -> T { self.0 }
    }
    #[rustc_diagnostic_item = "Mutex"]
    pub struct Mutex<T: ?Sized> { value: T }
    impl<T> Mutex<T> {
        pub const fn new(value: T) -> Mutex<T> { Mutex { value } }
        pub fn lock(&self) -> LockResult<MutexGuard<'_, T>> { loop {} }
    }
    #[rustc_diagnostic_item = "MutexGuard"]
    pub struct MutexGuard<'a, T: ?Sized> { lock: &'a Mutex<T> }
    impl<T: ?Sized> core::ops::Deref for MutexGuard<'_, T> {
        type Target = T;
        fn deref(&self) -> &T { loop {} }
    }
    impl<T: ?Sized> core::ops::DerefMut for MutexGuard<'_, T> {
        fn deref_mut(&mut self) -> &mut T { loop {} }
    }
    impl<T: ?Sized> core::ops::Drop for MutexGuard<'_, T> {
        fn drop(&mut self) {}
    }
    #[rustc_diagnostic_item = "RwLock"]
    pub struct RwLock<T: ?Sized> { value: T }
    impl<T> RwLock<T> {
        pub const fn new(value: T) -> RwLock<T> { RwLock { value } }
        pub fn read(&self) -> LockResult<RwLockReadGuard<'_, T>> { loop {} }
        pub fn write(&self) -> LockResult<RwLockWriteGuard<'_, T>> { loop {} }
    }
    #[rustc_diagnostic_item = "RwLockWriteGuard"]
    pub struct RwLockWriteGuard<'a, T: ?Sized> { lock: &'a RwLock<T> }
    impl<T: ?Sized> core::ops::Deref for RwLockWriteGuard<'_, T> {
        type Target = T;
        fn deref(&self) -> &T { loop {} }
    }
    impl<T: ?Sized> core::ops::DerefMut for RwLockWriteGuard<'_, T> {
        fn deref_mut(&mut self) -> &mut T { loop {} }
    }
    impl<T: ?Sized> core::ops::Drop for RwLockWriteGuard<'_, T> {
        fn drop(&mut self) {}
    }
    pub struct LazyLock<T, F = fn() -> T> { value: T, f: F }
    impl<T, F> LazyLock<T, F> {
        pub const fn new(f: F) -> LazyLock<T, F> { loop {} }
    }
    pub struct OnceLock<T> { value: T }
    impl<T> OnceLock<T> {
        pub const fn new() -> OnceLock<T> { loop {} }
    }
    #[rustc_diagnostic_item = "RwLockReadGuard"]
    pub struct RwLockReadGuard<'a, T: ?Sized> { lock: &'a RwLock<T> }
    impl<T: ?Sized> core::ops::Deref for RwLockReadGuard<'_, T> {
        type Target = T;
        fn deref(&self) -> &T { loop {} }
    }
    impl<T: ?Sized> core::ops::Drop for RwLockReadGuard<'_, T> {
        fn drop(&mut self) {}
    }
}
pub mod cell {
    pub struct RefCell<T: ?Sized> { value: T }
    impl<T> RefCell<T> {
        pub const fn new(value: T) -> RefCell<T> { RefCell { value } }
        pub fn borrow(&self) -> Ref<'_, T> { loop {} }
        pub fn borrow_mut(&self) -> RefMut<'_, T> { loop {} }
    }
    #[rustc_diagnostic_item = "RefCellRef"]
    pub struct Ref<'b, T: ?Sized> { value: &'b T }
    impl<T: ?Sized> core::ops::Deref for Ref<'_, T> {
        type Target = T;
        fn deref(&self) -> &T { self.value }
    }
    impl<T: ?Sized> core::ops::Drop for Ref<'_, T> {
        fn drop(&mut self) {}
    }
    #[rustc_diagnostic_item = "RefCellRefMut"]
    pub struct RefMut<'b, T: ?Sized> { value: &'b mut T }
    impl<T: ?Sized> core::ops::Deref for RefMut<'_, T> {
        type Target = T;
        fn deref(&self) -> &T { loop {} }
    }
    impl<T: ?Sized> core::ops::DerefMut for RefMut<'_, T> {
        fn deref_mut(&mut self) -> &mut T { loop {} }
    }
    impl<T: ?Sized> core::ops::Drop for RefMut<'_, T> {
        fn drop(&mut self) {}
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
impl<R, T: ?Sized> core::ops::Drop for MutexGuard<'_, R, T> {
    fn drop(&mut self) {}
}
//- /parking_lot/lib.rs crate:parking_lot deps:lock_api
pub struct RawMutex;
pub type Mutex<T> = lock_api::Mutex<RawMutex, T>;
//- /hashbrown/lib.rs crate:hashbrown
pub struct HashMap<K, V> { k: *const K, v: *const V }
pub struct HashSet<T> { t: *const T }
//- /indexmap/lib.rs crate:indexmap
pub struct IndexMap<K, V> { k: *const K, v: *const V }
pub struct IndexSet<T> { t: *const T }
//- /once_cell/lib.rs crate:once_cell
pub mod sync {
    pub struct Lazy<T, F = fn() -> T> { value: T, f: F }
}
//- /papaya/lib.rs crate:papaya
pub struct HashMap<K, V> { k: *const K, v: *const V }
pub struct HashSet<T> { t: *const T }
//- /blaze_graph/lib.rs crate:blaze_graph deps:alloc
pub mod par {
    pub fn for_each<T, F: Fn(&T) + Sync>(items: &[T], f: F) {}
    pub fn map<T, R, F: Fn(&T) -> R + Sync>(items: &[T], f: F) -> alloc::vec::Vec<R> { loop {} }
}
"#;

const MAIN: &str =
    "//- /src/lib.rs crate:main deps:std,alloc,lock_api,parking_lot,hashbrown,indexmap,once_cell,papaya,blaze_graph\n";

/// The fixture of the crate `main` written `main`, with [`CRATES`].
pub(crate) fn fixture(main: &str) -> String {
    format!("{MINICORE}{CRATES}{MAIN}{}", main.trim_start_matches('\n'))
}

/// What `clippy::<lint>` of each of `lints` alone say of `main`, each diagnostic as rustc
/// writes it.
pub(crate) fn lint_only(lints: &[&str], main: &str) -> String {
    let flags: String = lints.iter().map(|it| format!(" -W clippy::{it}")).collect();
    lint(&fixture(main), &format!("-A clippy::all{flags}"))
}

/// What `blaze::<lint>` alone says of `main`.
pub(crate) fn blaze_only(lint_name: &str, main: &str) -> String {
    lint(&fixture(main), &format!("-A clippy::all --warn=blaze::{lint_name}"))
}
