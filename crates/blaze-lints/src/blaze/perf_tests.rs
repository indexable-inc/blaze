//! What the performance lints (`false_sharing`, `seqcst_unexplained`, `large_move`,
//! `collect_then_consume`, `intern_candidate`, `small_map`, `arc_clone_only_borrowed`) run over
//! in their tests: an `std` whose `Arc`, `Mutex` and `Vec` are known by their diagnostic items,
//! whose `collections` has a `HashMap` and a `HashSet`, whose `sync::atomic` has the atomics and
//! `Ordering`,
//! and a `crossbeam_utils` with `CachePadded` aligned to 128 bytes as the real one is on x86-64
//! and arm64.

use crate::tests::lint;

const MINICORE: &str = "//- minicore: sized, copy, clone, option, fn, iterator, iterators, slice, deref, drop, sync, send, builtin_impls, index\n";

const CRATES: &str = r#"
//- /std/lib.rs crate:std
pub mod prelude {
    pub mod v1 {
        pub use crate::vec::Vec;
        pub use core::prelude::v1::*;
    }
    pub mod rust_2015 { pub use super::v1::*; }
    pub mod rust_2018 { pub use super::v1::*; }
    pub mod rust_2021 { pub use super::v1::*; }
    pub mod rust_2024 { pub use super::v1::*; }
}
pub mod sync {
    #[rustc_diagnostic_item = "Arc"]
    pub struct Arc<T: ?Sized> { ptr: *const T }
    impl<T> Arc<T> {
        pub fn new(value: T) -> Arc<T> { loop {} }
    }
    impl<T: ?Sized> Clone for Arc<T> {
        fn clone(&self) -> Arc<T> { loop {} }
    }
    impl<T: ?Sized> core::ops::Deref for Arc<T> {
        type Target = T;
        fn deref(&self) -> &T { loop {} }
    }
    #[rustc_diagnostic_item = "Mutex"]
    pub struct Mutex<T: ?Sized> { state: u32, value: T }
    impl<T> Mutex<T> {
        pub const fn new(value: T) -> Mutex<T> { Mutex { state: 0, value } }
        pub fn lock(&self) -> &T { loop {} }
    }
    pub mod atomic {
        pub enum Ordering { Relaxed, Release, Acquire, AcqRel, SeqCst }
        pub struct AtomicU64 { v: u64 }
        impl AtomicU64 {
            pub const fn new(v: u64) -> AtomicU64 { AtomicU64 { v } }
            pub fn load(&self, order: Ordering) -> u64 { loop {} }
            pub fn store(&self, v: u64, order: Ordering) {}
            pub fn fetch_add(&self, v: u64, order: Ordering) -> u64 { loop {} }
            pub fn compare_exchange(&self, current: u64, new: u64, success: Ordering, failure: Ordering) {}
        }
        pub struct AtomicBool { v: u8 }
        impl AtomicBool {
            pub const fn new(v: bool) -> AtomicBool { AtomicBool { v: 0 } }
            pub fn load(&self, order: Ordering) -> bool { loop {} }
            pub fn store(&self, v: bool, order: Ordering) {}
        }
    }
}
pub mod collections {
    pub struct HashMap<K, V> { k: K, v: V }
    impl<K, V> HashMap<K, V> {
        pub fn new() -> HashMap<K, V> { loop {} }
        pub fn with_capacity(capacity: usize) -> HashMap<K, V> { loop {} }
        pub fn get(&self, k: &K) -> Option<&V> { None }
        pub fn contains_key(&self, k: &K) -> bool { false }
        pub fn insert(&mut self, k: K, v: V) -> Option<V> { None }
        pub fn len(&self) -> usize { 0 }
    }
    pub struct HashSet<T> { t: T }
    impl<T> HashSet<T> {
        pub fn new() -> HashSet<T> { loop {} }
        pub fn contains(&self, t: &T) -> bool { false }
        pub fn insert(&mut self, t: T) -> bool { false }
    }
}
pub mod vec {
    #[rustc_diagnostic_item = "Vec"]
    pub struct Vec<T> { ptr: *const T, cap: usize, len: usize }
    impl<T> Vec<T> {
        pub const fn new() -> Vec<T> { loop {} }
        pub fn push(&mut self, value: T) {}
        pub fn len(&self) -> usize { self.len }
        pub fn is_empty(&self) -> bool { self.len == 0 }
        pub fn first(&self) -> Option<&T> { None }
        pub fn contains(&self, x: &T) -> bool { false }
        pub fn iter(&self) -> Iter<'_, T> { loop {} }
    }
    pub struct Iter<'a, T> { v: &'a Vec<T> }
    impl<'a, T> core::iter::Iterator for Iter<'a, T> {
        type Item = &'a T;
        fn next(&mut self) -> Option<&'a T> { loop {} }
    }
    impl<T> core::iter::FromIterator<T> for Vec<T> {
        fn from_iter<I: core::iter::IntoIterator<Item = T>>(iter: I) -> Vec<T> { loop {} }
    }
    pub struct IntoIter<T> { ptr: *const T }
    impl<T> core::iter::Iterator for IntoIter<T> {
        type Item = T;
        fn next(&mut self) -> Option<T> { loop {} }
    }
    impl<T> core::iter::IntoIterator for Vec<T> {
        type Item = T;
        type IntoIter = IntoIter<T>;
        fn into_iter(self) -> IntoIter<T> { loop {} }
    }
}
//- /crossbeam_utils/lib.rs crate:crossbeam_utils
#[repr(align(128))]
pub struct CachePadded<T> { value: T }
impl<T> CachePadded<T> {
    pub const fn new(value: T) -> CachePadded<T> { CachePadded { value } }
}
impl<T> core::ops::Deref for CachePadded<T> {
    type Target = T;
    fn deref(&self) -> &T { &self.value }
}
"#;

const MAIN: &str = "//- /src/lib.rs crate:main deps:std,crossbeam_utils\n";

/// The fixture of the crate `main` written `main`, with [`CRATES`].
pub(super) fn fixture(main: &str) -> String {
    format!("{MINICORE}{CRATES}{MAIN}{}", main.trim_start_matches('\n'))
}

/// What `blaze::<lint_name>` alone says of `main`.
pub(super) fn perf_only(lint_name: &str, main: &str) -> String {
    lint(&fixture(main), &format!("-A clippy::all --warn=blaze::{lint_name}"))
}

/// What `blaze::<lint_name>` alone says of `main`, over a minicore of the flags `minicore`.
pub(super) fn perf_only_with(minicore: &str, lint_name: &str, main: &str) -> String {
    let fixture =
        format!("//- minicore: {minicore}\n{CRATES}{MAIN}{}", main.trim_start_matches('\n'));
    lint(&fixture, &format!("-A clippy::all --warn=blaze::{lint_name}"))
}

/// What the lints `flags` turn on say of `main`.
pub(super) fn perf_with(flags: &str, main: &str) -> String {
    lint(&fixture(main), &format!("-A clippy::all {flags}"))
}
