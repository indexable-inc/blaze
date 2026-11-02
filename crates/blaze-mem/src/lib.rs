//! Memory the way a game engine manages it (docs/v2/plan/design/memory.md): every byte comes
//! from an allocator chosen by how long it lives, and the warm loop never calls the
//! general-purpose allocator.
//!
//! | lifetime | allocator |
//! |---|---|
//! | a task (one query run, one codegen unit, one stencil emit) | [`Frame`]: a bump pointer over a chain of 2 MB chunks and spans, a frame per task (an inner task starts in the rest of its outer task's chunk), with [`Marker`]s and reset; [`frame::task`] hands each task one |
//! | a request (an edit, a build, a test run) | [`Arena`]: chunked, shared by the request's tasks, each bumping in blocks of its own through an [`ArenaLocal`] |
//! | a view's revision | [`Revisions`]: two arenas, the next revision built while the current one is read, the old one reused once no task can still read it ([`blaze_reclaim`]) |
//! | fixed-size objects that come and go across workers | [`Pool`]: a slab per type with free lists, a worker's [`Cache`] in front, refilled and returned 64 at a time |
//! | objects one owner names by ID (plane tasks, I/O operations, timers, watch slots) | [`Slab`] with generational [`Handle`]s: a stale handle is detected, not dereferenced |
//! | chunks shared by several owners, copied on write | [`pool::Shared`]: a pooled `Arc`, the last owner returns the slot |
//! | objects retired while readers may hold them | [`pool::Limbo`]: kept in their slots until their grace passes, then back to the pool |
//! | tables, interners and columns | [`RegionVec`] (one owner), [`AppendVec`] (one writer, lock-free readers) and [`Column`] (any number of appenders): address space reserved up front, pages committed as they grow, never moved; the big, randomly read ones on huge pages ([`huge`]) |
//! | per-call scratch | [`scratch`]: buffers per worker, grown once to their high-water mark, lent for a closure or until dropped |
//! | tables made and dropped whole, grown by a new one twice the last | [`blocks`]: power-of-two blocks from a region per size class, recycled through lock-free stacks; [`blocks::Slice`] is `Box<[T]>` in one |
//!
//! Every allocator is tagged with its subsystem ([`Tag`]); [`tag`] keeps live bytes, the
//! high-water mark and the bytes handed out per tag, which memory admission and the agent API
//! read ([`tag::snapshot`]), against a budget per tag ([`tag::set_budget`]).
//!
//! The allocators that hand out raw memory implement [`std::alloc::Allocator`], so
//! `Vec<T, &Frame>` and `Vec<T, &ArenaLocal<'_>>` work. Their memory comes from the operating
//! system through [`Region`]s and the [`chunks`] cache, never from the global allocator: a
//! warm frame, arena, pool or slab makes no call to it (the allocation-budget tests hold them
//! to zero).
//!
//! Rounding to power-of-two alignments is [`align`]'s, one copy for the whole tree; the
//! SwissTable group probe every interner shares is [`swiss`]'s.
//!
//! Unsafe code is confined to [`os`] (the kernel's virtual memory), the few lines in each
//! allocator that turn a committed address into a typed place, and [`lock`]'s guard (a lock
//! that never allocates, which `std`'s does on macOS); every block says which
//! invariant holds and where it is upheld (docs/v2/plan/design/unsafe.md).

#![feature(allocator_api, allocator_ext)]

pub mod align;
pub mod append;
pub mod arena;
pub mod blocks;
pub mod chunks;
pub mod column;
mod error;
pub mod frame;
pub mod handle;
pub mod huge;
pub mod lock;
mod os;
pub mod pool;
pub mod region;
pub mod region_vec;
pub mod revision;
pub mod scratch;
mod stack;
pub mod swiss;
mod sync;
pub mod tag;

pub use append::AppendVec;
pub use arena::{Arena, ArenaLocal};
pub use column::{Block, Column};
pub use error::MemError;
pub use frame::{Frame, Marker};
pub use handle::{Handle, Slab};
pub use pool::{Cache, Pool, Pooled, Shared};
pub use region::Region;
pub use region_vec::RegionVec;
pub use revision::{Built, Revisions};
pub use tag::Tag;

/// The size of a huge page on the machines blaze targets (x86-64's and arm64's 2 MB pages
/// with 4 KB base pages). Every Unix reservation is aligned to it, and a region of huge pages
/// commits whole ones past its first ([`huge`]).
pub const HUGE_PAGE: usize = 2 << 20;

/// The base page size of this machine (4 KB on x86-64 and most arm64 Linux, 16 KB on Apple
/// silicon). Regions commit in multiples of it.
pub fn page_size() -> usize {
    os::page_size()
}

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

#[cfg(all(test, not(blaze_check)))]
mod tests;

#[cfg(all(test, blaze_check))]
mod model;
