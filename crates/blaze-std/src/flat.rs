//! [`Flat`]: a value whose bytes are all of it, so they mean the same in another thread,
//! another process or another machine (std.md, "Concurrency: what to reach for"; the idea is
//! Goose's, github.com/aardappel/goose: threads that share no memory and pass only flat values
//! through typed queues have no data race, no lock and no atomic between them).
//!
//! A flat value holds no [`Address`] anywhere in its own bytes: no raw pointer, reference or
//! function pointer, no owning or counted pointer (`Box`, `Vec`, `Arc`, `Rc`, `Weak`, a
//! `String`, a map), no interior-mutability cell (`Cell`, `RefCell`, an atomic, a lock), no
//! capability ([`crate::own::Capability`]), no allocator handle, no machine-local id, and no
//! `usize` or `isize` (their width is the target's). It has no drop glue either: nothing runs
//! when one is let go, so a copy of its bytes is the value. What it may hold: fixed-width
//! integers, floats, `bool`, `char`, `()`, and structs, enums, tuples, arrays and options of
//! flat values.
//!
//! ```
//! #![feature(allocator_api)]
//! use std::{alloc::Global, num::NonZeroUsize};
//!
//! /// A job, flat: its input is named by a key, not pointed at.
//! struct Job { key: [u8; 32], size: u64, retries: u8 }
//!
//! let jobs = bstd::sync::chan::bounded::<Job, _>("jobs", NonZeroUsize::new(4).unwrap(), Global);
//! # let _ = jobs;
//! ```
//!
//! A message with a reference inside is refused when the channel is made (under rustc, by the
//! `const` assertion in [`crate::sync::chan::bounded`]; built by blaze, at the bound):
//!
//! ```compile_fail
//! #![feature(allocator_api)]
//! use std::{alloc::Global, num::NonZeroUsize};
//!
//! /// A job whose name is borrowed: an address of the sender's memory.
//! struct Job { name: &'static str, size: u64 }
//!
//! let jobs = bstd::sync::chan::bounded::<Job, _>("jobs", NonZeroUsize::new(4).unwrap(), Global);
//! # let _ = jobs;
//! ```
//!
//! **Derived, by the one walk** ([`crate::own::reach`]): the question [`AnyAddress`] finds a
//! type that implements [`Address`] in the value's own bytes, and refuses a type it cannot see
//! into (a union, a closure, a `dyn` type, a foreign type). [`is_flat`] is that walk and the
//! drop-glue check together, for one instance.
//!
//! - Built by blaze (`cfg(blaze)`), `Flat` is one blanket impl bounded by `where const {
//!   is_flat::<T>() }` (`flat/extended.rs`): a bound that fails shows the field path to the
//!   address.
//! - Built by rustc (std.md, "Two builders"), which cannot parse `where const`, `Flat` is held of
//!   every type, and the walk is asserted where it matters, as a `const` of the instance:
//!   [`crate::sync::chan::bounded`] asserts it for its message type. Weaker, as `Acyclic`'s
//!   plain form is: generic code that only names the bound checks nothing, and a trusted
//!   assertion (below) does not stop the use-site assertion from walking the type.
//!
//! `unsafe impl Flat for X {}` is the trusted assertion, for a type the walk cannot see into
//! whose bytes are still all of it (a union of integers, say). `impl Address for X {}` is the
//! other direction, and safe, since it only refuses more: a type that is plain integers but
//! means something only on this machine (an interned id, a slot of a process-local table, a
//! file descriptor) says so, and every type holding one stops being flat. blaze's interned ids
//! should (blaze-memo's `Gid`), once their crates are on bstd.

use core::{alloc::Allocator, ops::FnPtr, ptr::NonNull};

use crate::own::{
    Capability, Jointly, Mutably, Nothing, Owns, Solely, raw,
    reach::{Ask, Find, Question, Through, none},
};

/// The derived impl of [`Flat`], bounded by `where const`, which rustc cannot parse: compiled
/// by blaze only.
#[cfg(blaze)]
mod extended;

/// A value whose bytes are all of it: they mean the same in another thread, process or
/// machine, and nothing runs when it is let go. See the module's doc for what is flat, how it
/// is derived, and what each builder checks.
///
/// It is a `#[marker]` trait, so the trusted assertion may overlap the derived impl.
///
/// # Safety
///
/// An impl promises, for every value of the type: no [`Address`] is in its bytes (nothing in
/// them names memory, a cell, a capability or a thing of this machine or process), every byte
/// pattern a value of the type has means the same value on every host blaze runs on, and the
/// type has no drop glue. A channel that copies a value's bytes to another process or machine
/// rests on it: an impl that breaks it hands the receiver a dangling address or another
/// process's id.
#[marker]
pub unsafe trait Flat {}

// SAFETY: not a promise under rustc (module doc, "Built by rustc"): each place that relies on
// the property asserts `is_flat::<T>()` in a `const` for its instance as it is compiled, and
// code that needs it of a bound alone is built by blaze.
#[cfg(not(blaze))]
unsafe impl<T: ?Sized> Flat for T {}

/// A type that names a place or a thing of this machine: a value of it means nothing in
/// another process or on another host. What [`AnyAddress`] looks for.
///
/// Implemented here for raw and non-null pointers, references, function pointers, `usize` and
/// `isize`, every type that holds its contents behind a pointer or a cell (an [`Owns`] impl of
/// [`Solely`], [`Jointly`], [`Mutably`] or [`Nothing`]: `Box`, `Vec`, `String`, `Arc`, `Rc`,
/// `Weak`, `Cell`, `RefCell`, the atomics, the locks), every [`Capability`] and every
/// [`Allocator`]. Implementing it for a type of one's own is safe: it only refuses more.
#[marker]
pub trait Address {}

impl<T: ?Sized> Address for *const T {}
impl<T: ?Sized> Address for *mut T {}
impl<T: ?Sized> Address for NonNull<T> {}
impl<T: ?Sized> Address for &T {}
impl<T: ?Sized> Address for &mut T {}
/// A function's address: where this process loaded it.
impl<F: FnPtr> Address for F {}
// The one function pointer the walk asks for every function pointer (`own::raw::implements`
// says why the blanket impl above is not enough under rustc).
impl Address for fn() {}
/// The target's width: a value written on one host is read on another.
impl Address for usize {}
/// The target's width, as `usize`.
impl Address for isize {}
/// An owning pointer: `Box`, `Vec`, `String`, a map.
impl<T: ?Sized + Owns<Solely>> Address for T {}
/// A counted pointer: `Arc`, `Rc`.
impl<T: ?Sized + Owns<Jointly>> Address for T {}
/// A pointer that owns nothing: `Weak`.
impl<T: ?Sized + Owns<Nothing>> Address for T {}
/// Interior mutability: a flat value is copied, and a copy of a cell is not the cell.
impl<T: ?Sized + Owns<Mutably>> Address for T {}
/// Authority over something outside the program, which is this machine's.
impl<T: ?Sized + Capability> Address for T {}
/// A handle to an allocator of this process.
impl<T: ?Sized + Allocator> Address for T {}

/// Is an [`Address`] in the value's own bytes? A type the walk cannot see into is a refusal.
pub struct AnyAddress;

impl Question for AnyAddress {
    const ASK: Ask = Ask {
        find: Find::Implements(raw::id::<dyn Address>()),
        // An address is found where it sits; nothing behind one is part of the value's bytes.
        through: Through::InPlace,
        noun: "an address of this machine (a pointer, a reference, an owning or counted pointer, a cell, a capability, an allocator, a machine-local id or a target-width integer), so the value is not flat",
        unseen_refuses: true,
        vouch: None,
    };
}

/// `true` when `T` is flat: no [`Address`] in its bytes, and no drop glue. Otherwise a panic
/// whose message is the reason: written as `const { assert!(is_flat::<T>()) }` under rustc and as
/// the `where const` bound of the derived impl built by blaze.
#[rustc_comptime]
pub fn is_flat<T: ?Sized>() -> bool {
    none::<T, AnyAddress>() && no_drop_glue::<T>()
}

/// `true` when nothing runs when a `T` is let go; otherwise a panic saying so.
const fn no_drop_glue<T: ?Sized>() -> bool {
    match core::mem::needs_drop::<T>() {
        false => true,
        true => panic!(
            "the type has drop glue (a `Drop` impl in it), so the value is not flat: a flat value is its bytes, and nothing runs when one is let go"
        ),
    }
}
