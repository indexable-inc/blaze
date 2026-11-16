//! blaze's own owning types, in place of Rust's `alloc` crate (docs/v2/plan/design/bottom.md,
//! "Also decided on 2026-11-15": "no alloc! we should have our own").
//!
//! Every owning type takes its allocator as a type parameter with no default, and every
//! constructor takes an allocator value: `Vec::new_in(a)`, `Box::new_in(x, a)`,
//! `Arc::new_in(x, a)`. There is no `Vec::new`, no `Default`, no `FromIterator`, nothing that
//! names the global heap: a caller that wants it passes its own `Global`. So rule 11
//! (bump-allocate: name the allocator in code) is held by the types; an implied allocator
//! cannot be written.
//!
//! | type | what |
//! |---|---|
//! | [`RawBuf`] | an uninitialized buffer of `T` and its allocator: the one place a buffer is allocated, grown, shrunk and freed |
//! | [`Vec`] | a growable array ([`vec_in!`]) |
//! | [`Box`] | one value, sized or not; unsizes to `Box<dyn Trait, A>` and `Box<[T], A>` |
//! | [`Arc`], [`arc::Weak`] | an atomically counted shared value |
//! | [`Rc`], [`rc::Weak`] | the same, counted on one thread |
//! | [`VecDeque`] | a ring buffer |
//! | [`SortedMap`], [`SortedSet`] | ordered by key, a sorted vector searched by halving |
//! | [`Cow`] | borrowed or owned, copied into the allocator the caller names when it must own |
//! | [`Soa`], [`soa!`] | a struct-of-arrays vector: one packed column per field in one allocation; the macro names its rows and columns |
//! | [`String`], [`format_in!`], [`format_into!`] | UTF-8 text over [`Vec<u8, A>`](Vec); `blaze_mem` re-exports it, keeping only `OsString` and `PathBuf` |
//! | [`FixedVec`] | up to `N` values inline, no allocator; a push past `N` is refused with [`Full`], the value handed back |
//! | [`Packed`], [`Pack`] | values of different sizes end to end in one byte buffer, each its own size; iterated, not indexed, with an optional checkpoint every `K` values for seeking |
//! | [`Inline`] | an allocator over a buffer inside itself (`&Inline<BYTES>`), so any of these lives on the stack; refuses when full, never falls back |
//!
//! The cargo feature `std` (off by default; blaze-mem turns it on) adds what only this crate
//! may write by the orphan rule and only std has: `AsRef<OsStr>` and `AsRef<Path>` for
//! [`String`]. Nothing else of std is used. The feature `testing` adds the module `testing`: a leak-
//! and misuse-counting allocator, one that refuses the n-th allocation, and `every_failure`,
//! which runs a closure once per allocation with that one refused (after Zig's
//! `std.testing.allocator` and `checkAllAllocationFailures`).
//!
//! # Allocation failure
//!
//! An infallible call (`push`, `reserve`, `new_in`) that cannot get memory panics with an
//! [`AllocFailure`] naming the type and the layout (`fail`), which fails its own job (rule
//! 20), never an abort; each has a `try_` form that returns the [`AllocFailure`] (or a
//! [`Refused`] with the value handed back) and leaves the value as it was, which the tests
//! hold under `testing::every_failure`. The rule that follows: library code calls the `try_`
//! forms and returns the failure; only a binary's top decides that a refusal stops it. A count
//! that would overflow (a reference count past `isize::MAX`) aborts, as std's does: by then
//! the count is wrong for every owner, which no one job's failure can contain.
//!
//! # Nightly features
//!
//! A nightly bump checks this table: each gate, and what in the crate needs it.
//!
//! | feature | what for |
//! |---|---|
//! | `allocator_ext` | `AllocatorClone`, the bound for sharing one allocator between the owners of an [`Arc`] or [`Rc`] (each may free the block); `StaticAllocator`, the bound for pinning a [`Box`], [`Arc`] or [`Rc`] |
//! | `coerce_unsized` | `Box<T, A>` to `Box<dyn Trait, A>` and `Box<[T; N], A>` to `Box<[T], A>`, the same for [`Arc`], [`Rc`] and their `Weak`s |
//! | `unsize` | the `Unsize` bound of those impls |
//! | `dropck_eyepatch` | `#[may_dangle]` on the `Drop` of every owner, so a `Vec<&'a T, A>` may be dropped after `'a` ends, as std's may |
//! | `fn_traits`, `unboxed_closures` | `FnOnce`, `FnMut` and `Fn` for a [`Box`] of a [`CallOnce`] closure, so a boxed one-shot closure is called as `b(x)` |
//! | `tuple_trait` | the `Tuple` bound on those impls' argument lists |
//! | `register_tool`, `custom_inner_attributes` | `#![blaze::trust]`, blaze's lint attribute; inert under rustc |
//!
//! Stable on the pinned nightly and used without a gate: `core::alloc::Allocator`,
//! `AllocError`, `Layout::for_value_raw`, `core::mem::DropGuard`.
//!
//! # Unsafe code
//!
//! Every `unsafe` block states the invariant it relies on in a `// SAFETY:` comment. Buffers
//! are allocated, grown and freed only in [`RawBuf`] and in the struct-of-arrays core
//! ([`Soa`], whose [`soa!`] wrappers are safe code); [`Box`], [`Arc`] and [`Rc`] allocate their one block each in their own
//! module; [`FixedVec`] keeps its slots inline and [`Inline`] hands out ranges of its own buffer. Panic-safety guards are `core::mem::DropGuard`s; a hand-written guard says why.
//!
//! # Send and Sync
//!
//! Exactly as std's: `Vec`, `Box`, `VecDeque`, the sorted map and set and the
//! struct-of-arrays vector are `Send` when `T: Send, A: Send` and `Sync` when
//! `T: Sync, A: Sync`; `Arc` and `arc::Weak` are both when `T: Send + Sync, A: Send + Sync`;
//! `Rc` and `rc::Weak` are neither.

#![no_std]
#![feature(allocator_ext, coerce_unsized, unsize, dropck_eyepatch)]
#![feature(fn_traits, unboxed_closures, tuple_trait)]
// blaze's lints (`blaze::trust`) name their tool; inert under rustc.
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::trust(
    unsafe,
    reason = "the owning types themselves: a buffer's allocation and its initialized prefix (`RawBuf`, `soa::RawSoa`), a box's block, a counted block's header and value; each block names the invariant it relies on"
)]

// Tests compare against std's types and use its threads; the crate itself sees `std` only for
// the two impls of the `std` feature.
#[cfg(any(test, feature = "std"))]
extern crate std;

// `macro_use`: the crate's own code names `vec_in!` and `soa!` by textual scope; a path
// (`crate::vec_in!`) to an exported macro is refused in a crate whose root carries a tool
// attribute, as blaze-mem's `text` found.
#[macro_use]
mod macros;

pub mod arc;
pub mod boxed;
pub mod cow;
pub mod deque;
mod error;
pub mod fixed;
pub mod packed;
pub mod raw;
pub mod rc;
#[macro_use]
pub mod soa;
pub mod sorted;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod text;
pub mod vec;

// A test-only switch that plants a known bug, for the oracles' controls (rule 53).
#[cfg(test)]
mod plant;

pub use arc::Arc;
pub use boxed::{Box, CallOnce};
pub use cow::{Cow, ToOwnedIn};
pub use deque::VecDeque;
pub use error::AllocFailure;
pub use fixed::{FixedString, FixedVec, Full, Inline};
pub use packed::{Pack, Packed};
pub use raw::RawBuf;
pub use rc::Rc;
pub use soa::Soa;
pub use sorted::{SortedMap, SortedSet};
pub use text::{FromUtf8Error, String};
pub use vec::{CollectIn, Refused, Vec};

#[cfg(test)]
mod tests;
