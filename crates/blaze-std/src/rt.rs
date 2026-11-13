//! What bstd still takes from Rust's `std`, all of it, in one module.
//!
//! bstd is `#![no_std]`: every other module names only `core`, `alloc` and blaze's crates. The
//! few things Rust's `std` alone provides today are named here and nowhere else, and this
//! module is a row of S4's ban list (`harness/v2/nostd.json`): it shrinks as `bstd::rt` (the
//! process's entry, unwinding and exit on `blaze-sys`) is built, and is deleted with the row
//! (docs/v2/plan/design/sys.md, "Runtime" and "Migration").
//!
//! | here | used by | replaced by |
//! |---|---|---|
//! | [`RootPath`] | `NativeWorld::root`, which blaze-core's drivers take as a `std::path::Path` | a `Dir` handle from the start block, when the drivers open beneath a handle |
//! | [`process_id`] | `Host`'s pid on a native `Io` | `blaze_sys`'s `getpid` |
//! | [`exit`] | `Host::exit` on a native `Io` | `blaze_sys`'s `exit_group` |
//! | [`DriverError`] | `io::OpenWhy::Driver`, what `blaze_core::native` returns | the driver's own error, when it has one |
//! | [`catch_unwind`], [`resume_unwind`] | a task's panic failing only its task; the simulated exit's unwind | bstd's own unwinder entry (on `library/unwind`) |
//! | [`LazyLock`], [`OnceLock`] | `bstd::sync`'s `Lazy` and `Once` | the same two on bstd's one wait |
//! | `Owns` for `HashMap`, `HashSet`, `Mutex`, `RwLock`, `OnceLock`, `LazyLock` | `bstd::own`'s reachability walk, for code that still holds one of Rust std's | nothing: gone with the last use of those types |

extern crate std;

pub use std::{
    io::Error as DriverError,
    panic::{catch_unwind, resume_unwind},
    path::Path as RootPath,
    sync::{LazyLock, OnceLock},
};

/// The process's id.
pub(crate) fn process_id() -> u32 {
    std::process::id()
}

/// Ends the process with `status`.
#[expect(
    clippy::disallowed_methods,
    reason = "`Host::exit` is the capability to end the process: a program asks for it by name, \
              and this is std's one call for it until `blaze_sys`'s `exit_group` replaces it"
)]
pub(crate) fn exit(status: i32) -> ! {
    std::process::exit(status)
}

// How the containers only Rust's `std` has hold their type arguments, for `bstd::own`'s walk
// (`own.rs` states core's and alloc's, with the rule): the maps behind a pointer they alone
// own, the locks and the once-cells in place, changeable through a shared reference.
#[expect(
    clippy::disallowed_types,
    reason = "how each of std's containers holds its contents is stated once, for the names \
              bstd replaces too: code that still holds one is walked the same way"
)]
#[expect(unsafe_code, reason = "the trusted `Owns` assertions for std's own containers, with their SAFETY")]
mod owns {
    // `super::std`: bstd is `#![no_std]`, so `std` is a name only where `rt` declares it.
    use super::std::{
        collections::{HashMap, HashSet},
        sync::{LazyLock, Mutex, OnceLock, RwLock},
    };

    use crate::own::{Mutably, Owns, Solely};

    // As `own.rs` says of the others: each keeps a value of each type argument alive the way
    // its impl says, and keeps alive nothing else that owns.

    // SAFETY: a `HashMap` keeps each key and value in a table behind a pointer it alone
    // owns, and its hasher `S` in place (walked as solely owned too, which only refuses more).
    unsafe impl<K, V, S> Owns<Solely> for HashMap<K, V, S> {}
    // SAFETY: a `HashSet` is a `HashMap<T, (), S>`: its values behind a pointer it alone owns.
    unsafe impl<T, S> Owns<Solely> for HashSet<T, S> {}
    // SAFETY: a `Mutex` holds its value in place in an `UnsafeCell`; `lock` through a shared
    // reference gives `&mut T`, so the value is changeable after it exists.
    unsafe impl<T: ?Sized> Owns<Mutably> for Mutex<T> {}
    // SAFETY: as `Mutex`: the value in place in an `UnsafeCell`, `write` through a shared
    // reference gives `&mut T`.
    unsafe impl<T: ?Sized> Owns<Mutably> for RwLock<T> {}
    // SAFETY: a `OnceLock` holds its value in place and `set` writes it through a shared
    // reference, once, after the cell exists.
    unsafe impl<T> Owns<Mutably> for OnceLock<T> {}
    // SAFETY: a `LazyLock` holds its initializer `F` and then its value `T` in place, the
    // one replaced by the other through a shared reference on first use.
    unsafe impl<T, F> Owns<Mutably> for LazyLock<T, F> {}
}
