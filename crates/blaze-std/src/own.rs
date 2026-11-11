//! Ownership properties derived from a type's shape, and the shared handles built on them
//! (docs/v2/plan/design/ownership.md).
//!
//! - [`reach`]: the one compile-time walk over what a type owns. [`reaches`] answers a
//!   [`Question`]; [`none`] is the same as a `where const` condition whose refusal names the
//!   field path.
//! - [`Acyclic`]: no value of the type can come to own itself. Derived: one blanket impl
//!   over what the [`Cycle`] walk clears, per instance.
//! - [`NoCapability`], and the questions [`AnySecret`] and [`AnyCell`]: the other
//!   reachability assertions, each the same walk with another question.
//! - [`Owns`]: how a container holds its type arguments, the trusted assertion that tells the
//!   walk about a type it cannot see into. bstd states it here for std's and its own.
//! - [`Shared`] and [`LocalShared`]: counted handles. Over an acyclic type they hold a linear
//!   value ([`Shared::holding`]); the handle is then linear, copied with `share` and ended
//!   with `release`, which hands the value back from the last one.
//! - [`Shares`]: a fixed number of owners known as the crate compiles, the fraction in the
//!   type, no count at run time.
//!
//! Which to use: borrow from a scope when the owners' lifetimes nest (the default); `Shares`
//! when their number is fixed; `Shared` over an acyclic type when it is not.

#![expect(
    clippy::disallowed_types,
    reason = "how each of std's containers holds its contents is stated here once, for the \
              names bstd replaces too: code that still holds one is walked the same way"
)]

pub mod raw;
pub mod reach;
mod shared;
pub mod shares;

use alloc::{
    boxed::Box,
    collections::{BTreeMap, BTreeSet, BinaryHeap, LinkedList, VecDeque},
    rc::{self, Rc},
    string::String,
    sync::{self, Arc},
    vec::Vec,
};
use core::{
    alloc::Allocator,
    cell::{Cell, LazyCell, OnceCell, RefCell, UnsafeCell},
    mem::{ManuallyDrop, MaybeUninit},
    sync::atomic::{Atomic, AtomicPrimitive},
};

pub use reach::{
    Acyclic, Ask, Cycle, Find, Inline, Jointly, Mutably, Nothing, Owns, Question, Reason, Solely,
    Through, Verdict, none, reaches,
};
pub use shared::{LocalShared, Released, Shared};
pub use shares::Shares;

// SAFETY: the walk for `Cycle` cleared `T`: from a `T`, through everything it owns, no
// counted pointer that can be set after the value exists leads back to `T`, and nothing on the
// way was a type the walk could not see into. A value owns itself only through such a pointer
// (reach.rs, `Cycle`).
unsafe impl<T: ?Sized> Acyclic for T where const { none::<T, Cycle>() } {}

/// A value that lets its holder reach something outside the program: the disk, the network,
/// the clock. bstd's capabilities implement it; a type that wraps one implements it too.
pub trait Capability {}

/// Is a capability reachable from the type, owned or borrowed?
pub struct AnyCapability;

impl Question for AnyCapability {
    const ASK: Ask = Ask {
        find: Find::Implements(raw::id::<dyn Capability>()),
        through: Through::Borrowed,
        noun: "a capability",
        unseen_refuses: true,
        vouch: None,
    };
}

/// The type holds no capability, anywhere inside: a configuration, a message, a key. Derived
/// as [`Acyclic`] is; `unsafe impl NoCapability for X {}` is the trusted assertion for a
/// type the walk cannot see into.
///
/// # Safety
///
/// An impl promises that no value of the type reaches a [`Capability`].
#[marker]
pub unsafe trait NoCapability {}

// SAFETY: the walk for `AnyCapability` found no type that implements `Capability` in, owned
// by or borrowed by a `T`, and saw into every type on the way.
unsafe impl<T: ?Sized> NoCapability for T where const { none::<T, AnyCapability>() } {}

/// Is a [`Secret`](crate::keys::Secret) reachable from the type? For an event or a log record:
/// `const _: () = assert!(none::<Event, AnySecret>());`.
pub struct AnySecret;

impl Question for AnySecret {
    const ASK: Ask = Ask {
        find: Find::Type(raw::id::<crate::keys::Secret>()),
        through: Through::Borrowed,
        noun: "a secret",
        unseen_refuses: true,
        vouch: None,
    };
}

/// Is there interior mutability in the value's own bytes? None means `&T` is truly immutable
/// (`Freeze`): what a key type must be.
pub struct AnyCell;

impl Question for AnyCell {
    const ASK: Ask = Ask {
        find: Find::Implements(raw::id::<dyn Owns<Mutably>>()),
        through: Through::InPlace,
        noun: "interior mutability",
        unseen_refuses: true,
        vouch: None,
    };
}

impl Capability for crate::io::Io {}
impl Capability for crate::fs::Fs {}
impl<#[blaze::linear] P: crate::fs::Rights> Capability for crate::fs::Dir<P> {}
impl<#[blaze::linear] P: crate::fs::Rights> Capability for crate::fs::File<P> {}
impl Capability for crate::net::Net {}
impl Capability for crate::clock::Clock {}
impl Capability for crate::rng::Rng {}
impl Capability for crate::env::Env {}
impl Capability for crate::args::Args {}
impl Capability for crate::host::Host {}
impl Capability for crate::mem::Mem {}
impl Capability for crate::keys::Keys {}

// How core's and alloc's containers hold their type arguments. Each is a trusted assertion
// about a type whose fields the walk would otherwise refuse (a raw pointer inside) or misread.
// The ones only Rust's `std` has (`HashMap`, `HashSet`, `Mutex`, `RwLock`, `OnceLock`,
// `LazyLock`) are stated in `crate::rt`, the one module that names `std` (bstd is `#![no_std]`).
//
// SAFETY (all of the impls below): the type keeps a value of each type argument alive the way
// the impl says and keeps alive nothing else that owns: `Option`, `Result`, `ManuallyDrop` and
// `MaybeUninit` in their own bytes; `Box`, `Vec`, the collections and the maps behind a pointer
// they alone own, their allocator and hasher in place (the walk treats every type argument the
// same way, which only refuses more); `Rc` and `Arc` behind a count other handles share;
// `UnsafeCell` and what is built on it in place, changeable through a shared reference; the
// `Weak`s own nothing, since dropping one never drops the value.
unsafe impl<T> Owns<Inline> for Option<T> {}
unsafe impl<T, E> Owns<Inline> for Result<T, E> {}
unsafe impl<T: ?Sized> Owns<Inline> for ManuallyDrop<T> {}
unsafe impl<T> Owns<Inline> for MaybeUninit<T> {}

unsafe impl<T: ?Sized, A: Allocator> Owns<Solely> for Box<T, A> {}
unsafe impl<T, A: Allocator> Owns<Solely> for Vec<T, A> {}
unsafe impl<T, A: Allocator> Owns<Solely> for VecDeque<T, A> {}
unsafe impl<T, A: Allocator> Owns<Solely> for LinkedList<T, A> {}
unsafe impl<T, A: Allocator> Owns<Solely> for BinaryHeap<T, A> {}
unsafe impl<K, V, A: Allocator + Clone> Owns<Solely> for BTreeMap<K, V, A> {}
unsafe impl<T, A: Allocator + Clone> Owns<Solely> for BTreeSet<T, A> {}
unsafe impl Owns<Solely> for String {}

unsafe impl<T: ?Sized, A: Allocator> Owns<Jointly> for Rc<T, A> {}
unsafe impl<T: ?Sized, A: Allocator> Owns<Jointly> for Arc<T, A> {}

unsafe impl<T: ?Sized> Owns<Mutably> for UnsafeCell<T> {}
unsafe impl<T: ?Sized> Owns<Mutably> for Cell<T> {}
unsafe impl<T: ?Sized> Owns<Mutably> for RefCell<T> {}
unsafe impl<T> Owns<Mutably> for OnceCell<T> {}
unsafe impl<T, F> Owns<Mutably> for LazyCell<T, F> {}
unsafe impl<T: AtomicPrimitive> Owns<Mutably> for Atomic<T> {}
unsafe impl<T: ?Sized> Owns<Mutably> for blaze_core::lock::Lock<T> {}
unsafe impl<T: ?Sized> Owns<Mutably> for blaze_core::lock::ReadLock<T> {}
unsafe impl<T: ?Sized, K> Owns<Mutably> for crate::cell::TokenCell<T, K> {}

unsafe impl<T: ?Sized, A: Allocator> Owns<Nothing> for rc::Weak<T, A> {}
unsafe impl<T: ?Sized, A: Allocator> Owns<Nothing> for sync::Weak<T, A> {}
