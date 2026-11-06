//! `lock_held_across_slow` over a fixture of its own: an `std` whose `fs::File::open` is IO,
//! whose `io::Write` is a stream, and whose `Vec` reaches an allocator marked
//! `#[rustc_allocator]` through `A: Allocator` and `FromIterator` (as `blaze::tests`'s
//! does); a `parking_lot` whose guards are `lock_api`'s; and a `blaze_core` whose `lock::Lock`
//! hands out a `LockGuard` holding one in its field `inner`, as blaze-core's does.

use super::assert_marked;
use crate::tests::lint;

const CRATES: &str = r#"
//- minicore: sized, copy, clone, drop, deref, fn, unsize, coerce_unsized, dispatch_from_dyn, send, sync, option, result, builtin_impls
//- /std/lib.rs crate:std
pub mod prelude {
    pub mod v1 {
        pub use crate::iter::Iterator;
        pub use crate::vec::Vec;
        pub use core::prelude::v1::*;
    }
    pub mod rust_2015 { pub use super::v1::*; }
    pub mod rust_2018 { pub use super::v1::*; }
    pub mod rust_2021 { pub use super::v1::*; }
    pub mod rust_2024 { pub use super::v1::*; }
}
pub mod alloc {
    extern "Rust" {
        #[rustc_allocator]
        fn __rust_alloc(size: usize, align: usize) -> *mut u8;
    }
    pub fn alloc(size: usize) -> *mut u8 {
        unsafe { __rust_alloc(size, 8) }
    }
    pub trait Allocator {
        fn allocate(&self, size: usize) -> *mut u8;
    }
    pub struct Global;
    impl Allocator for Global {
        fn allocate(&self, size: usize) -> *mut u8 {
            alloc(size)
        }
    }
}
pub mod iter {
    pub trait FromIterator<A>: Sized {
        fn from_iter<I: Iterator<Item = A>>(iter: I) -> Self;
    }
    pub trait Iterator {
        type Item;
        fn collect<B: FromIterator<Self::Item>>(self) -> B
        where
            Self: Sized,
        {
            B::from_iter(self)
        }
    }
}
pub mod vec {
    use crate::alloc::{Allocator, Global};
    pub struct Vec<T, A: Allocator = Global> {
        ptr: *mut T,
        cap: usize,
        alloc: A,
    }
    impl<T> Vec<T> {
        pub const fn new() -> Vec<T> {
            Vec { ptr: 0 as *mut T, cap: 0, alloc: Global }
        }
        pub fn with_capacity(cap: usize) -> Vec<T> {
            Vec::with_capacity_in(cap, Global)
        }
    }
    impl<T, A: Allocator> Vec<T, A> {
        pub fn with_capacity_in(cap: usize, alloc: A) -> Vec<T, A> {
            let ptr = alloc.allocate(cap) as *mut T;
            Vec { ptr, cap, alloc }
        }
    }
    impl<T> crate::iter::FromIterator<T> for Vec<T> {
        fn from_iter<I: crate::iter::Iterator<Item = T>>(_iter: I) -> Vec<T> {
            Vec::with_capacity(0)
        }
    }
}
pub mod io {
    pub trait Write {
        fn write(&mut self, buf: &[u8]) -> usize;
    }
}
pub mod fs {
    pub struct File(u32);
    impl File {
        pub fn open(_path: &str) -> File {
            File(0)
        }
    }
    impl crate::io::Write for File {
        fn write(&mut self, _buf: &[u8]) -> usize {
            0
        }
    }
}
//- /lock_api.rs crate:lock_api
pub struct Mutex<R, T>(R, T);
pub struct MutexGuard<'a, R, T>(&'a Mutex<R, T>);
impl<R, T> Mutex<R, T> {
    pub fn lock(&self) -> MutexGuard<'_, R, T> {
        MutexGuard(self)
    }
}
impl<R, T> Drop for MutexGuard<'_, R, T> {
    fn drop(&mut self) {}
}
impl<R, T> core::ops::Deref for MutexGuard<'_, R, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0.1
    }
}
//- /parking_lot.rs crate:parking_lot deps:lock_api
pub struct RawMutex;
pub type Mutex<T> = lock_api::Mutex<RawMutex, T>;
pub type MutexGuard<'a, T> = lock_api::MutexGuard<'a, RawMutex, T>;
//- /blaze_core/lib.rs crate:blaze_core deps:parking_lot,lock_api
pub mod lock {
    pub struct Lock<T> {
        name: &'static str,
        raw: parking_lot::Mutex<T>,
    }
    pub struct LockGuard<'a, T> {
        name: &'static str,
        inner: parking_lot::MutexGuard<'a, T>,
    }
    impl<T> Lock<T> {
        pub fn lock(&self) -> LockGuard<'_, T> {
            LockGuard { name: self.name, inner: self.raw.lock() }
        }
    }
    impl<T> Drop for LockGuard<'_, T> {
        fn drop(&mut self) {}
    }
    impl<T> core::ops::Deref for LockGuard<'_, T> {
        type Target = T;
        fn deref(&self) -> &T {
            &self.inner
        }
    }
}
//- /src/lib.rs crate:main deps:std,parking_lot,blaze_core
"#;

/// A `parking_lot` guard held across `File::open` (IO), across a `collect` into a `Vec` (the
/// global heap) and across a write to a `dyn Write`, and a `blaze_core::lock::LockGuard` held
/// across `File::open`, are findings; a guard dropped before the call, and one held across a
/// call that is neither, are none.
#[test]
fn lock_held_across_slow() {
    let source = r#"
use parking_lot::Mutex;
use std::iter::Iterator;

pub struct Cache {
    table: Mutex<u32>,
    named: blaze_core::lock::Lock<u32>,
}

pub struct Count(u32);

impl Iterator for Count {
    type Item = u32;
}

pub fn plain(x: u32) -> u32 {
    x
}

pub fn opens(cache: &Cache) -> u32 {
    let guard = cache.table.lock();
    let _file = std::fs::File::open("x"); //~ lock_held_across_slow
    *guard
}

pub fn collects(cache: &Cache, items: Count) -> u32 {
    let guard = cache.table.lock();
    let all: std::vec::Vec<u32> = items.collect(); //~ lock_held_across_slow
    let _ = all;
    *guard
}

pub fn streams(cache: &Cache, out: &mut dyn std::io::Write) -> u32 {
    let guard = cache.table.lock();
    let _ = out.write(b"x"); //~ lock_held_across_slow
    *guard
}

pub fn blaze_core_guard(cache: &Cache) -> u32 {
    let held = cache.named.lock();
    let _file = std::fs::File::open("x"); //~ lock_held_across_slow
    *held
}

pub fn dropped_first(cache: &Cache) -> u32 {
    let n = {
        let guard = cache.table.lock();
        *guard
    };
    let _file = std::fs::File::open("x");
    n
}

pub fn neither(cache: &Cache) -> u32 {
    let guard = cache.table.lock();
    let p = plain(1);
    p + *guard
}
"#
    .trim_start_matches('\n');
    let fixture = format!("{CRATES}{source}");
    let flags = "-A clippy::all --deny=blaze::lock_held_across_slow";
    let out = lint(&fixture, flags);
    assert_marked(&out, source, flags);
    assert!(
        out.contains(
            "is held across `File::open`, which reaches the file system through `std::fs::File::open`"
        ),
        "the call and its IO root are named:\n{out}"
    );
    assert!(out.contains("the lock guard `guard` (`"), "the guard is named with its type:\n{out}");
    assert!(out.contains("which allocates on the global heap"), "{out}");
    assert!(out.contains("through `std::io::Write` on a receiver it cannot see into"), "{out}");
    assert!(out.contains("the lock guard `held` (`LockGuard<"), "blaze-core's guard by name:\n{out}");
    assert!(
        out.contains(
            "a lock is held only for a field update: rule standard-structures; docs/v2/plan/design/named-locks.md"
        ),
        "the rule:\n{out}"
    );
}
