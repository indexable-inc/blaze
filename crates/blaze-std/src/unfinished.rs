//! The rustc build's check that a value which must be finished was finished (std.md, "Values
//! that must be finished").
//!
//! Built by blaze, a must-finish type is `#[blaze::linear]` and leaving one unfinished is a
//! compile error. rustc has no linear types, so there a silent drop would run the value's
//! abandonment path (close without syncing, hang up, drop the bytes held) where the program
//! meant to finish it: the two builders would disagree exactly where data is lost. So each
//! such type holds an [`Unfinished`], and in a debug build of rustc's dropping that guard
//! stops the program, naming the type, where it was made and how it is finished. The type's
//! finishing calls disarm it ([`Unfinished::finished`]).
//!
//! It says nothing while the thread is already unwinding: a panic is the one place the
//! abandonment path is meant to run, and a second panic would abort the process. A release
//! build of rustc's keeps no guard and checks nothing: there an unfinished value still runs its
//! abandonment path silently, as it did before this check. Built by blaze the guard is empty,
//! since the linear check already refused the program.

use core::{fmt, mem::ManuallyDrop};

/// A must-finish value's guard: in a debug build of rustc's, its type and where it was made,
/// checked when it is dropped; otherwise nothing.
pub(crate) struct Unfinished {
    #[cfg(all(not(blaze), debug_assertions))]
    ty: &'static str,
    #[cfg(all(not(blaze), debug_assertions))]
    finished_by: &'static str,
    #[cfg(all(not(blaze), debug_assertions))]
    made_at: &'static core::panic::Location<'static>,
}

impl Unfinished {
    /// The guard of a `ty` made by the caller's caller (each constructor that makes one is
    /// `#[track_caller]` back to the program's call where it can be), finished by
    /// `finished_by` (`"finish(cx)"`).
    #[track_caller]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn new(ty: &'static str, finished_by: &'static str) -> Unfinished {
        #[cfg(not(all(not(blaze), debug_assertions)))]
        let _ = (ty, finished_by);
        Unfinished {
            #[cfg(all(not(blaze), debug_assertions))]
            ty,
            #[cfg(all(not(blaze), debug_assertions))]
            finished_by,
            #[cfg(all(not(blaze), debug_assertions))]
            made_at: core::panic::Location::caller(),
        }
    }

    /// The value was finished: the guard ends without its check. The guard holds no resource
    /// (two `&'static str` and a `&'static Location`), so not running its destructor frees
    /// nothing late.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn finished(self) {
        let _ = ManuallyDrop::new(self);
    }
}

#[cfg(all(not(blaze), debug_assertions))]
impl Drop for Unfinished {
    fn drop(&mut self) {
        if !crate::rt::panicking() {
            panic!(
                "bstd: a `{}` made at {} was dropped unfinished; it is finished with {} \
                 (std.md, \"Values that must be finished\": built by blaze this is a compile \
                 error, and in a debug build of rustc's it stops here)",
                self.ty, self.made_at, self.finished_by
            );
        }
    }
}

/// Every guard is equal: it is no part of its value's meaning (a `ReadWrite` compares equal to
/// another).
impl PartialEq for Unfinished {
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn eq(&self, _: &Unfinished) -> bool {
        true
    }
}

impl Eq for Unfinished {}

impl fmt::Debug for Unfinished {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Unfinished")
    }
}
