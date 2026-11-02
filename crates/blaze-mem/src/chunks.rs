//! The process's chunks: 2 MB blocks of reserved spaces, handed to arenas and frames and taken
//! back for the next (docs/v2/plan/design/memory.md "back to the pool, never to the OS in the warm
//! loop").
//!
//! A space is reserved once ([`SPACE_BYTES`] of address space, nothing committed) and carved
//! in order; a chunk given back goes on its space's lock-free stack and is the next one taken
//! there, warm in cache and already backed by pages. [`trim`] lets the kernel take the pages of
//! cached chunks (they stay committed and come back zeroed), for after a burst. Each chunk is
//! aligned to a huge page, and on Linux the space is advised to use them.
//!
//! There is a space per place: one whose pages the kernel places as it likes (first touch), and
//! one per NUMA node, reserved on that node ([`Region::reserve_huge_on`]), made the first time a
//! thread placed there ([`set_thread_node`]: a pool worker, on its CPU's node) takes a chunk. A
//! chunk taken on a node is the node's for good, and goes back to its own space whichever
//! thread gives it, so a worker's frames are on its node with no system call per chunk.
//!
//! Cost: a take or give is one compare-and-swap on the stack's head; carving is one
//! `fetch_add`, plus a commit system call every quarter of what is committed.

use std::{cell::Cell, ptr::NonNull, sync::OnceLock};

use crate::{
    MemError, Region, Tag,
    stack::{LinkTable, Links, Stack},
    sync::atomic::{AtomicU32, Ordering},
    tag,
};

/// The size of a chunk.
pub const CHUNK: usize = crate::HUGE_PAGE;

/// The address space reserved for the chunks of one place: 256 GB, 131,072 chunks. Free on a
/// 64-bit machine (x86-64 and arm64 give user space 128 TB), even for every place at once.
#[cfg(target_pointer_width = "64")]
pub const SPACE_BYTES: usize = 256 << 30;

/// The address space reserved for chunks on a 32-bit target: 256 MB, 128 chunks. There
/// (wasm32) a reservation is memory taken whole, so it is a budget, not free address space;
/// such a target has no NUMA node, so only the unplaced space is ever made.
#[cfg(not(target_pointer_width = "64"))]
pub const SPACE_BYTES: usize = 256 << 20;

/// The NUMA nodes chunks can be placed on: 0 to 15. Machines blaze targets have at most 8
/// (two sockets of four sub-NUMA clusters); a node past these is an error naming it.
pub const MAX_NODES: u32 = 16;

const COUNT: u32 = (SPACE_BYTES / CHUNK) as u32;

/// Bits of a chunk's index that number it within its space; the bits above name the place.
const SHIFT: u32 = 17;

/// The unplaced space, then one per node.
const PLACES: usize = 1 + MAX_NODES as usize;

const _: () = assert!(COUNT <= 1 << SHIFT);
const _: () = assert!(((PLACES as u64) << SHIFT) < crate::stack::NONE as u64);

struct Space {
    region: Region,
    links: LinkTable,
    carved: AtomicU32,
    /// Chunks given back, their pages resident.
    warm: Stack,
    /// Chunks [`trim`] purged: committed, their pages given to the kernel.
    cold: Stack,
}

static SPACES: [OnceLock<Result<Space, String>>; PLACES] = [const { OnceLock::new() }; PLACES];

thread_local! {
    /// Where the calling thread's chunks are carved: 0 unplaced, `n + 1` NUMA node `n`.
    static PLACE: Cell<u8> = const { Cell::new(0) };
}

/// Places the calling thread's chunks (and so its frames) on NUMA node `node`, or nowhere in
/// particular with `None`: what a pool worker bound to a CPU does as it starts. An error for a
/// node past [`MAX_NODES`].
pub fn set_thread_node(node: Option<u32>) -> Result<(), MemError> {
    let place = match node {
        None => 0,
        Some(node) if node < MAX_NODES => node as u8 + 1,
        Some(node) => {
            return Err(MemError::TooLarge {
                tag: Tag::Chunks,
                what: "a NUMA node number (chunks are placed on nodes 0 to 15)",
                wanted: node as usize,
            });
        }
    };
    PLACE.set(place);
    Ok(())
}

/// The NUMA node the calling thread's chunks are placed on, if any.
pub fn thread_node() -> Option<u32> {
    match PLACE.try_with(Cell::get).unwrap_or(0) {
        0 => None,
        place => Some(u32::from(place) - 1),
    }
}

fn space(place: usize) -> Result<&'static Space, MemError> {
    let space = SPACES[place].get_or_init(|| {
        let region = match place {
            0 => Region::reserve_huge(SPACE_BYTES, Tag::Chunks),
            node => Region::reserve_huge_on(SPACE_BYTES, node as u32 - 1, Tag::Chunks),
        };
        let made = region.and_then(|region| {
            Ok(Space {
                region,
                links: LinkTable::new(COUNT, Tag::Chunks)?,
                carved: AtomicU32::new(0),
                warm: Stack::new(),
                cold: Stack::new(),
            })
        });
        made.map_err(|error| error.to_string()) // alloc: once, when a space cannot be reserved
    });
    match space {
        Ok(space) => Ok(space),
        Err(message) => Err(MemError::Reserve {
            tag: Tag::Chunks,
            bytes: SPACE_BYTES,
            os: std::io::Error::other(message.as_str()),
        }),
    }
}

/// The space a chunk exists in: made before the chunk was carved.
fn made(place: usize) -> &'static Space {
    match SPACES[place].get() {
        Some(Ok(space)) => space,
        _ => unreachable!("a chunk outlived its space"),
    }
}

const fn place_of(index: u32) -> usize {
    (index >> SHIFT) as usize
}

const fn local_of(index: u32) -> u32 {
    index & ((1 << SHIFT) - 1)
}

const fn index(place: usize, local: u32) -> u32 {
    ((place as u32) << SHIFT) | local
}

/// A chunk held by one owner: the arena or frame it was taken for.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Chunk(u32);

impl Chunk {
    pub(crate) fn index(&self) -> u32 {
        self.0
    }

    /// The chunk's first byte; the chunk is `CHUNK` bytes, committed, aligned to `CHUNK`.
    pub(crate) fn ptr(&self) -> NonNull<u8> {
        made(place_of(self.0)).region.at(local_of(self.0) as usize * CHUNK)
    }

    /// The chunk `index` names, which the caller holds (it took it and linked it in a list
    /// of its own).
    pub(crate) fn from_index(index: u32) -> Chunk {
        Chunk(index)
    }
}

/// Takes a chunk for `owner`, in the calling thread's place: a warm one if any was given back
/// there, else a purged one, else a new one carved from the place's space.
pub(crate) fn take(owner: Tag) -> Result<Chunk, MemError> {
    let place = usize::from(PLACE.try_with(Cell::get).unwrap_or(0));
    let space = space(place)?;
    let local = match space.warm.pop(&space.links) {
        Some(local) => local,
        None => match space.cold.pop(&space.links) {
            Some(local) => local,
            None => carve(space)?,
        },
    };
    tag::transfer(Tag::Chunks, owner, CHUNK);
    Ok(Chunk(index(place, local)))
}

#[cold]
fn carve(space: &Space) -> Result<u32, MemError> {
    let local = space.carved.fetch_add(1, Ordering::Relaxed);
    if local >= COUNT {
        space.carved.store(COUNT, Ordering::Relaxed);
        return Err(MemError::Exhausted {
            tag: Tag::Chunks,
            reserved: SPACE_BYTES,
            wanted: (local as usize + 1) * CHUNK,
        });
    }
    space.region.ensure((local as usize + 1) * CHUNK)?;
    // SAFETY: `local` came from the `fetch_add` just now, so no other thread has it.
    unsafe { space.links.init(local) }?;
    Ok(local)
}

/// Gives back a chunk `owner` held; its contents are dead. It goes back to its own space.
#[expect(
    clippy::needless_pass_by_value,
    reason = "the chunk is given up: taking it ends its holder's hold, as `Chunk` is not `Copy` so it is held once"
)]
pub(crate) fn give(chunk: Chunk, owner: Tag) {
    let Chunk(chunk) = chunk;
    let space = made(place_of(chunk));
    tag::transfer(owner, Tag::Chunks, CHUNK);
    space.warm.push(local_of(chunk), &space.links);
}

/// Chunks [`give_all`] moves to a space at once.
pub(crate) const GIVE_RUN: usize = 64;

/// As [`give`] for every chunk of `chunks` (at most [`GIVE_RUN`] of them): each run of one
/// place goes back to its space with one compare-and-swap, its bytes counted with one transfer.
pub(crate) fn give_all(chunks: &[u32], owner: Tag) {
    debug_assert!(chunks.len() <= GIVE_RUN);
    let mut locals = [0u32; GIVE_RUN];
    let mut at = 0;
    while at < chunks.len() {
        let place = place_of(chunks[at]);
        let mut run = 0;
        while at < chunks.len() && place_of(chunks[at]) == place {
            locals[run] = local_of(chunks[at]);
            run += 1;
            at += 1;
        }
        let space = made(place);
        tag::transfer(owner, Tag::Chunks, run * CHUNK);
        space.warm.push_all(&locals[..run], &space.links);
    }
}

/// As [`give`], and lets the kernel take its pages first: for a chunk an owner trims after a
/// burst rather than keeps warm.
#[expect(
    clippy::needless_pass_by_value,
    reason = "the chunk is given up: taking it ends its holder's hold, as `Chunk` is not `Copy` so it is held once"
)]
pub(crate) fn give_cold(chunk: Chunk, owner: Tag) {
    let Chunk(chunk) = chunk;
    let space = made(place_of(chunk));
    // SAFETY: the owner gives the chunk up, so no one reads its contents before writing them.
    unsafe { space.region.purge(local_of(chunk) as usize * CHUNK, CHUNK) };
    tag::transfer(owner, Tag::Chunks, CHUNK);
    space.cold.push(local_of(chunk), &space.links);
}

/// The link of a held chunk, for its owner's list of the chunks it holds (a chunk is on one
/// list at a time: an owner's or its space's cache).
pub(crate) fn link(chunk: u32) -> &'static AtomicU32 {
    made(place_of(chunk)).links.link(local_of(chunk))
}

/// Lets the kernel take the pages of every warm cached chunk of every place; they stay
/// committed and come back zeroed. Returns how many it purged. For after a burst, never in
/// the warm loop.
pub fn trim() -> usize {
    let mut purged = 0;
    for space in SPACES.iter().filter_map(|space| space.get()?.as_ref().ok()) {
        while let Some(local) = space.warm.pop(&space.links) {
            // SAFETY: the chunk was popped off the cache, so no owner holds it and no one reads
            // its contents before writing them.
            unsafe { space.region.purge(local as usize * CHUNK, CHUNK) };
            space.cold.push(local, &space.links);
            purged += 1;
        }
    }
    purged
}

/// Bytes of chunks carved so far (held or cached), over every place.
pub fn carved_bytes() -> usize {
    SPACES.iter().filter_map(|space| space.get()?.as_ref().ok()).map(Space::carved_bytes).sum()
}

/// Bytes of chunks carved so far on NUMA node `node`, or in the unplaced space with `None`.
pub fn carved_on(node: Option<u32>) -> usize {
    let place = match node {
        None => 0,
        Some(node) if node < MAX_NODES => node as usize + 1,
        Some(_) => return 0,
    };
    SPACES[place].get().and_then(|space| space.as_ref().ok()).map_or(0, Space::carved_bytes)
}

impl Space {
    fn carved_bytes(&self) -> usize {
        self.carved.load(Ordering::Relaxed).min(COUNT) as usize * CHUNK
    }
}
