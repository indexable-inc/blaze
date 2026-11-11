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
//! | [`catch_unwind`], [`resume_unwind`] | a task's panic failing only its task; the simulated exit's unwind | bstd's own unwinder entry (on `library/unwind`) |
//! | [`LazyLock`], [`OnceLock`] | `bstd::sync`'s `Lazy` and `Once` | the same two on bstd's one wait |
//! | `Owns` for `HashMap`, `HashSet`, `Mutex`, `RwLock`, `OnceLock`, `LazyLock` | `bstd::own`'s reachability walk, for code that still holds one of Rust std's | nothing: gone with the last use of those types |

extern crate std;

pub use std::{
    panic::{catch_unwind, resume_unwind},
    path::Path as RootPath,
    sync::{LazyLock, OnceLock},
};

/// The process's id.
pub(crate) fn process_id() -> u32 {
    std::process::id()
}

/// Ends the process with `status`.
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
    use std::{
        collections::{HashMap, HashSet},
        sync::{LazyLock, Mutex, OnceLock, RwLock},
    };

    use crate::own::{Mutably, Owns, Solely};

    // SAFETY (all of the impls below): as `own.rs` says of the others. A map keeps each key
    // and value alive behind a pointer it alone owns, its hasher in place; a lock or a
    // once-cell keeps its value in place, changeable through a shared reference; none keeps
    // alive anything else that owns.
    unsafe impl<K, V, S> Owns<Solely> for HashMap<K, V, S> {}
    unsafe impl<T, S> Owns<Solely> for HashSet<T, S> {}
    unsafe impl<T: ?Sized> Owns<Mutably> for Mutex<T> {}
    unsafe impl<T: ?Sized> Owns<Mutably> for RwLock<T> {}
    unsafe impl<T> Owns<Mutably> for OnceLock<T> {}
    unsafe impl<T, F> Owns<Mutably> for LazyLock<T, F> {}
}
