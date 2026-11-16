//! The one-thread types, and the mark each carries (docs/v2/plan/design/bottom.md, "Fibers and
//! `Send`"): a value meant for one thread checks, in a debug build, that it ends on the thread
//! that made it.
//!
//! Why: rustc checks `Send` across an `.await` and not across a fiber's wait, so a one-thread
//! value held by a task that resumed on another thread would be a race rustc accepts. bstd's
//! answer as rustc builds it is that a started task never changes its thread
//! ([`crate::task`]); this mark is that rule's check on the values it protects. A value that
//! moved threads by plain Rust (a `RefCell` is `Send`) trips it too, which is the point: these
//! types are meant to stay. Built by blaze, a task may move at a wait (blaze refuses a value
//! that is not `Send` live across one), so the mark checks nothing there.
//!
//! The wrappers dereference to the `core::cell` type they hold, so every call of it is theirs.
//! bstd has no thread-local storage: what would be one is a value passed down
//! (std.md, "What replaces a thread-local").

use core::{
    cell::{LazyCell, OnceCell, RefCell},
    fmt,
    ops::Deref,
};

/// Where a one-thread value was made: in a debug build of rustc's, the OS thread, checked when
/// the value ends; otherwise nothing.
pub(crate) struct OneThread {
    #[cfg(all(debug_assertions, not(blaze)))]
    made_on: crate::rt::ThreadMark,
}

impl OneThread {
    /// The calling thread.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn here() -> OneThread {
        OneThread {
            #[cfg(all(debug_assertions, not(blaze)))]
            made_on: crate::rt::thread_mark(),
        }
    }
}

impl Drop for OneThread {
    fn drop(&mut self) {
        #[cfg(all(debug_assertions, not(blaze)))]
        if !crate::rt::panicking() && crate::rt::thread_mark() != self.made_on {
            panic!(
                "bstd::sync a one-thread value ended on another thread than the one that made it \
                 (bottom.md, \"Fibers and `Send`\": a started task never changes its thread)"
            );
        }
    }
}

impl fmt::Debug for OneThread {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OneThread")
    }
}

macro_rules! one_thread {
    ($(#[$doc:meta])* $name:ident<$($p:ident $(= $default:ty)?),*> over $inner:ident) => {
        $(#[$doc])*
        pub struct $name<$($p $(= $default)?),*> {
            cell: $inner<$($p),*>,
            home: OneThread,
        }

        impl<$($p),*> $name<$($p),*> {
            /// The value in its cell, homed on the calling thread.
            pub fn from_cell(cell: $inner<$($p),*>) -> $name<$($p),*> {
                $name { cell, home: OneThread::here() }
            }

            /// The cell, and its value, out of the wrapper: on the thread that made it.
            pub fn into_cell(self) -> $inner<$($p),*> {
                let $name { cell, home } = self;
                drop(home);
                cell
            }
        }

        impl<$($p),*> Deref for $name<$($p),*> {
            type Target = $inner<$($p),*>;
            fn deref(&self) -> &$inner<$($p),*> {
                &self.cell
            }
        }

        impl<$($p),*> fmt::Debug for $name<$($p),*>
        where
            $inner<$($p),*>: fmt::Debug,
        {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Debug::fmt(&self.cell, f)
            }
        }
    };
}

one_thread! {
    /// `core::cell::RefCell`, for one thread: borrowed with `borrow` and `borrow_mut`, which
    /// panic where a `Lock` would wait.
    LocalLock<T> over RefCell
}

one_thread! {
    /// `core::cell::OnceCell`, for one thread: set once, read many.
    LocalOnce<T> over OnceCell
}

one_thread! {
    /// `core::cell::LazyCell`, for one thread: made on first use.
    LocalLazy<T, F = fn() -> T> over LazyCell
}

impl<T> LocalLock<T> {
    pub fn new(value: T) -> LocalLock<T> {
        LocalLock::from_cell(RefCell::new(value))
    }

    /// The value, the lock consumed.
    pub fn into_inner(self) -> T {
        self.into_cell().into_inner()
    }

    /// The value, through the only reference: no borrow flag.
    pub fn get_mut(&mut self) -> &mut T {
        self.cell.get_mut()
    }
}

impl<T> LocalOnce<T> {
    pub fn new() -> LocalOnce<T> {
        LocalOnce::from_cell(OnceCell::new())
    }
}

impl<T> Default for LocalOnce<T> {
    fn default() -> LocalOnce<T> {
        LocalOnce::new()
    }
}

impl<T, F: FnOnce() -> T> LocalLazy<T, F> {
    pub fn new(make: F) -> LocalLazy<T, F> {
        LocalLazy::from_cell(LazyCell::new(make))
    }

    /// The value, made now if it was not yet.
    pub fn force(this: &LocalLazy<T, F>) -> &T {
        LazyCell::force(&this.cell)
    }
}
