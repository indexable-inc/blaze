//! Scratch buffers reused per worker (docs/v2/plan/design/memory.md, "Scratch buffers reused
//! per worker"): formatting, hashing, encoding and decompression write into a buffer the
//! worker keeps, grown once to its high-water mark and never freed in the warm loop.
//!
//! [`with_bytes`], [`with_u32s`], [`with_u64s`], [`with_string`] and [`with_path`] lend the
//! calling thread's buffer, empty, for the closure; a call inside a call gets another of the
//! thread's (up to [`DEPTH`] deep, each kept warm). [`lend_bytes`], [`lend_u32s`],
//! [`lend_u64s`], [`lend_string`] and [`lend_path`] lend one past any closure: the [`Lent`]
//! goes back to the shelf of the thread that drops it, so a column built on one worker and read
//! on another still returns warm. [`Scratch<T>`] is the same for a type of the caller's, kept
//! in its worker state.
//!
//! Cost: two thread-local accesses and a `clear`; an allocation only while a buffer grows past
//! the most it ever held.

use std::{
    cell::RefCell,
    fmt, mem,
    ops::{Deref, DerefMut},
    path::PathBuf,
};

/// Buffers kept per thread and kind: the nesting depth whose buffers stay warm. A reparse
/// holds four lexings at once (a node's, and the text around it lexed joined and in parts),
/// two `u32` columns each. A shelf only holds buffers that were out at once, so a deeper one
/// costs a thread nothing it did not already use.
pub const DEPTH: usize = 8;

/// A thread's buffers of one kind, up to `N` (the nesting depth kept warm): what a
/// [`Shelved`] implementation keeps in its `thread_local!`, as
/// `const { RefCell::new(Shelf::new()) }`.
pub struct Shelf<B: Shelved, const N: usize = DEPTH> {
    /// `buffers[..free]` are on the shelf; the rest are `None`, lent out or never made.
    buffers: [Option<B>; N],
    free: usize,
}

impl<B: Shelved, const N: usize> Shelf<B, N> {
    /// An empty shelf: buffers join it as they come back.
    pub const fn new() -> Self {
        Shelf { buffers: [const { None }; N], free: 0 }
    }

    /// The top buffer, if the shelf has one.
    pub fn take(&mut self) -> Option<B> {
        if self.free == 0 {
            return None;
        }
        self.free -= 1;
        self.buffers[self.free].take()
    }

    /// Shelves `buffer` on top. A full shelf (a buffer lent on another thread coming back
    /// here) keeps its `N` largest and frees the smallest.
    pub fn give(&mut self, buffer: B) {
        if self.free < N {
            self.buffers[self.free] = Some(buffer);
            self.free += 1;
            return;
        }
        let held = |slot: &Option<B>| slot.as_ref().map_or(0, Shelved::held);
        let Some(smallest) = (0..N).min_by_key(|&at| held(&self.buffers[at])) else {
            return;
        };
        if buffer.held() > held(&self.buffers[smallest]) {
            self.buffers[smallest] = Some(buffer);
        }
    }
}

impl<B: Shelved, const N: usize> Default for Shelf<B, N> {
    fn default() -> Self {
        Shelf::new()
    }
}

/// A kind of buffer each thread keeps a shelf of.
pub trait Shelved: Default + 'static {
    /// Empties it, keeping its capacity.
    fn empty(&mut self);

    /// What it has room for without growing.
    fn held(&self) -> usize;

    /// One of the calling thread's buffers of this kind, if its shelf has one.
    fn unshelve() -> Option<Self>;

    /// Puts it on the calling thread's shelf; it is freed when the shelf is full or the thread
    /// is ending.
    fn shelve(self);
}

/// Lends the calling thread's buffer of kind `B`, empty, for `f`.
pub fn with<B: Shelved, R>(f: impl FnOnce(&mut B) -> R) -> R {
    let mut buffer = B::unshelve().unwrap_or_default();
    buffer.empty();
    let result = f(&mut buffer);
    buffer.empty();
    buffer.shelve();
    result
}

/// Lends the calling thread's buffer of kind `B`, empty, until the [`Lent`] is dropped.
pub fn lend<B: Shelved>() -> Lent<B> {
    let mut buffer = B::unshelve().unwrap_or_default();
    buffer.empty();
    Lent { buffer }
}

/// A buffer lent from a thread's shelf. Dropping it empties it and puts it on the shelf of the
/// thread it is dropped on. `Send` when the buffer is.
pub struct Lent<B: Shelved> {
    buffer: B,
}

impl<B: Shelved> Lent<B> {
    /// Keeps the buffer: it goes back to no shelf.
    pub fn into_inner(mut self) -> B {
        mem::take(&mut self.buffer)
    }
}

impl<B: Shelved> Deref for Lent<B> {
    type Target = B;

    fn deref(&self) -> &B {
        &self.buffer
    }
}

impl<B: Shelved> DerefMut for Lent<B> {
    fn deref_mut(&mut self) -> &mut B {
        &mut self.buffer
    }
}

impl<B: Shelved> Drop for Lent<B> {
    fn drop(&mut self) {
        let mut buffer = mem::take(&mut self.buffer);
        // One with no room (taken by `into_inner`) is not worth a place on the shelf.
        if buffer.held() != 0 {
            buffer.empty();
            buffer.shelve();
        }
    }
}

impl<B: Shelved + fmt::Debug> fmt::Debug for Lent<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.buffer, f)
    }
}

macro_rules! shelves {
    ($($(#[$doc:meta])* $with:ident, $lend:ident, $shelf:ident: $buffer:ty;)*) => {
        thread_local! {
            $(
                static $shelf: RefCell<Shelf<$buffer>> = const {
                    RefCell::new(Shelf::new())
                };
            )*
        }

        $(
            impl Shelved for $buffer {
                fn empty(&mut self) {
                    self.clear();
                }

                fn held(&self) -> usize {
                    self.capacity()
                }

                fn unshelve() -> Option<Self> {
                    $shelf.try_with(|shelf| shelf.borrow_mut().take()).ok().flatten()
                }

                fn shelve(self) {
                    // A thread that is ending: the buffer is freed.
                    let _ = $shelf.try_with(|shelf| shelf.borrow_mut().give(self));
                }
            }

            $(#[$doc])*
            pub fn $with<R>(f: impl FnOnce(&mut $buffer) -> R) -> R {
                with::<$buffer, R>(f)
            }

            $(#[$doc])*
            ///
            /// Until the returned [`Lent`] is dropped, on any thread.
            pub fn $lend() -> Lent<$buffer> {
                lend::<$buffer>()
            }
        )*
    };
}

shelves! {
    /// Lends the thread's byte buffer, empty.
    with_bytes, lend_bytes, BYTES: Vec<u8>;
    /// Lends the thread's `u32` buffer, empty.
    with_u32s, lend_u32s, U32S: Vec<u32>;
    /// Lends the thread's `u64` buffer, empty.
    with_u64s, lend_u64s, U64S: Vec<u64>;
    /// Lends the thread's string buffer, empty: formatting at the edge.
    with_string, lend_string, STRINGS: String;
}

thread_local! {
    static PATHS: RefCell<Shelf<PathBuf>> =
        const { RefCell::new(Shelf::new()) };
}

impl Shelved for PathBuf {
    fn empty(&mut self) {
        self.as_mut_os_string().clear();
    }

    fn held(&self) -> usize {
        self.capacity()
    }

    fn unshelve() -> Option<Self> {
        PATHS.try_with(|shelf| shelf.borrow_mut().take()).ok().flatten()
    }

    fn shelve(self) {
        // A thread that is ending: the buffer is freed.
        let _ = PATHS.try_with(|shelf| shelf.borrow_mut().give(self));
    }
}

/// Lends the thread's path buffer, empty: paths joined to probe or stat.
pub fn with_path<R>(f: impl FnOnce(&mut PathBuf) -> R) -> R {
    with::<PathBuf, R>(f)
}

/// Lends the thread's path buffer, empty, until the returned [`Lent`] is dropped.
pub fn lend_path() -> Lent<PathBuf> {
    lend::<PathBuf>()
}

/// A buffer of `T`s a worker keeps across calls: [`Scratch::take`] lends it empty, keeping
/// what it grew to.
#[derive(Default)]
pub struct Scratch<T> {
    buffer: Vec<T>,
}

impl<T> Scratch<T> {
    pub const fn new() -> Scratch<T> {
        Scratch { buffer: Vec::new() } // alloc: none, an empty vector
    }

    /// The buffer, emptied.
    pub fn take(&mut self) -> &mut Vec<T> {
        self.buffer.clear();
        &mut self.buffer
    }

    /// The most elements it has room for without growing.
    pub fn high_water(&self) -> usize {
        self.buffer.capacity()
    }

    /// Frees the buffer: after a burst, so it grows again only to the next working set.
    pub fn shrink(&mut self, keep: usize) {
        self.buffer.clear();
        self.buffer.shrink_to(keep);
    }
}
