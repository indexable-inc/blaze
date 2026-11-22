//! Each case `tobstd` meets, one function or item apiece; `expected/` holds what each mode makes
//! of it, and `expected/<mode>.json` the summary with its walls.

use core::fmt;
use core::mem::take;
use std::{cell::Cell, mem::replace};

/// rename: a path inline, and in a `use` (above); the grouped `use` stays (ast-grep sees no
/// path there).
pub fn renamed(x: &mut u32) -> u32 {
    let _ = core::mem::size_of::<u64>();
    let old = core::mem::replace(x, 1);
    let _ = take(x);
    let _ = replace(x, 2);
    let _ = Cell::new(0);
    old
}

/// add-arg: a function with no allocator gains `alloc: blaze_own::Alloc<'_>` (no generic); its
/// types follow.
pub fn words(n: usize, alloc: blaze_own::Alloc<'_>) -> blaze_own::Vec<u32, blaze_own::Alloc<'_>> {
    let mut v = blaze_own::Vec::with_capacity_in(n, alloc);
    v.push(1);
    v
}

/// add-arg: no parameters at all.
pub fn greeting(alloc: blaze_own::Alloc<'_>) -> blaze_own::String<blaze_own::Alloc<'_>> {
    let name = blaze_own::String::from_str_in("world", alloc);
    let owned = blaze_own::ToOwnedIn::to_owned_in("x", alloc);
    blaze_own::format_in!(alloc, "hello {name}{owned}")
}

/// add-arg: generics of its own, the allocator's added after them.
pub fn listed<T: Copy>(items: &[T], alloc: blaze_own::Alloc<'_>) -> blaze_own::Vec<T, blaze_own::Alloc<'_>> {
    let mut out = blaze_own::Vec::new_in(alloc);
    out.extend_from_slice(items);
    out
}

/// add-arg: an `alloc` already in scope: used, no parameter added.
pub fn with_alloc(alloc: blaze_own::Alloc<'_>) -> blaze_own::AVec<'_, u8> {
    blaze_own::vec_in![alloc; 1, 2, 3]
}

/// add-arg: a collect whose turbofish names the target.
pub fn evens(n: u32, alloc: blaze_own::Alloc<'_>) -> usize {
    blaze_own::CollectIn::collect_in((0..n).filter(|x| x % 2 == 0), alloc).len()
}

/// add-arg: a method on a type that holds its allocator: `self.alloc`.
pub struct Pool<'a> {
    pub alloc: blaze_own::Alloc<'a>,
}

impl<'a> Pool<'a> {
    pub fn fresh(&self) -> usize {
        let b = blaze_own::Box::new_in(5u8, self.alloc);
        usize::from(*b)
    }
}

/// wall `trait-method`: `Default::default` takes no argument; and wall `needs-lifetime`: `Bag`
/// owns a `Vec`.
pub struct Bag {
    pub items: Vec<u8>,
}

impl Default for Bag {
    fn default() -> Bag {
        Bag { items: Vec::new() }
    }
}

/// wall `drop`.
impl Drop for Bag {
    fn drop(&mut self) {
        let _note = String::new();
    }
}

/// wall `static-init`.
pub static NAMES: std::sync::LazyLock<Vec<u8>> = std::sync::LazyLock::new(|| vec![1]);

/// wall `ambiguous`: two functions named `make`, one with a site.
pub mod twins {
    pub fn make() -> usize {
        let v: Vec<u8> = Vec::new();
        v.len()
    }
}

pub fn make() -> usize {
    3
}

/// A generic already named `A`: no clash, the parameter's type is concrete.
pub fn taken<A: Copy>(a: A, alloc: blaze_own::Alloc<'_>) -> A {
    let _ = blaze_own::String::new_in(alloc);
    a
}

/// wall `fn-pointer`: `words` gains a parameter, and is passed as a value here (listed, not
/// attempted).
pub fn lengths(ns: &[usize]) -> usize {
    ns.iter().copied().map(words).count()
}

#[cfg(test)]
mod tests {
    /// A test passes `Alloc::global()`.
    #[test]
    fn a_test() {
        let v = blaze_own::vec_in![blaze_own::Alloc::global(); 1u8];
        assert_eq!(v.len(), 1);
    }
}

/// `entry`: `main` passes `Alloc::global()` (in a library this is only a function named `main`).
pub fn main() {
    let s = blaze_own::format_in!(blaze_own::Alloc::global(), "{}", 1);
    let _ = fmt::Debug::fmt;
    drop(s);
}
