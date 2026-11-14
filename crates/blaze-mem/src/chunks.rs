//! The process's chunks: 2 MB blocks of reserved spaces, handed to arenas and frames and taken
//! back for the next (docs/v2/plan/design/memory.md "back to the pool, never to the OS in the warm
//! loop").
//!
//! A space is reserved once ([`SPACE_BYTES`] of address space, nothing committed) and carved
//! in order; a chunk given back goes on its space's lock-free stack and is the next one taken
//! there, warm in cache and already backed by pages. A space keeps a burst's chunks warm up to
//! [`WARM_MOST`]; the give that passes it lets the kernel take the pages of the cache's chunks
//! past [`WARM_KEEP`] (they stay committed and come back zeroed), so a burst's peak is not kept
//! resident for the process's life, and a working set under [`WARM_MOST`] never pays for a
//! purge. [`trim`] does that to every cached chunk, for after a burst. Each chunk is
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

/// The warm chunks a space keeps after its cache passed [`WARM_MOST`]: 512 MiB a place, 16 for
/// each of 16 workers (a worker's frames shelve 8 of their own, `frame::SHELF`, before they
/// give to the space). coord-102 (B5): with no bound, 3.3 GiB of the 4.72 GiB of frames a
/// build's peak held stayed resident as warm chunks to the end, with nothing to take them.
pub const WARM_KEEP: u32 = 256;

/// The warm chunks past which a give trims the space's cache down to [`WARM_KEEP`]: twice it,
/// so a trim purges at least [`WARM_KEEP`] chunks at once, and a working set that swings under
/// this purges none.
pub const WARM_MOST: u32 = 2 * WARM_KEEP;

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
    /// Chunks given back, their pages resident: at most `most`, about.
    warm: Stack,
    /// How many chunks are on `warm`, or about to be: counted up before a push and down after a
    /// pop, so never less than what the stack holds.
    warm_count: AtomicU32,
    /// The warm chunks kept after a trim ([`WARM_KEEP`]), and past which a give trims
    /// ([`WARM_MOST`]); a test's own space, smaller.
    keep: u32,
    most: u32,
    /// Chunks [`trim`] purged: committed, their pages given to the kernel. Only a chunk whose
    /// purge gave its pages back is here; one the kernel kept stays on `warm`.
    cold: Stack,
    /// How many chunks are on `cold`, or about to be: counted up before a push and down after a
    /// pop, as `warm_count` is. Their bytes are committed but not resident ([`cached`]).
    cold_count: AtomicU32,
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
                warm_count: AtomicU32::new(0),
                keep: WARM_KEEP,
                most: WARM_MOST,
                cold: Stack::new(),
                cold_count: AtomicU32::new(0),
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
/// there, else a purged one (its pages made the process's again: [`Region::reuse`]), else a
/// new one carved from the place's space.
pub(crate) fn take(owner: Tag) -> Result<Chunk, MemError> {
    let place = usize::from(PLACE.try_with(Cell::get).unwrap_or(0));
    let local = space(place)?.take()?;
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
    space.give(&[local_of(chunk)]);
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
        space.give(&locals[..run]);
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
    tag::transfer(owner, Tag::Chunks, CHUNK);
    space.purge_cold(local_of(chunk));
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
    SPACES.iter().filter_map(|space| space.get()?.as_ref().ok()).map(|space| space.trim_to(0)).sum()
}

/// The chunks no owner holds, over every place, in bytes: `warm` ones keep their pages (resident,
/// free), `cold` ones were purged (committed, counted to [`Tag::Chunks`], but their pages are the
/// kernel's: not resident, out of the process's footprint). What the memory line says of
/// [`Tag::Chunks`], and what [`crate::tag::Snapshot::purged`] takes off committed memory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cached {
    pub warm: u64,
    pub cold: u64,
}

/// [`Cached`] now: two loads a place made.
pub fn cached() -> Cached {
    SPACES.iter().filter_map(|space| space.get()?.as_ref().ok()).map(Space::cached).fold(
        Cached::default(),
        |sum, it| Cached { warm: sum.warm + it.warm, cold: sum.cold + it.cold },
    )
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

    /// This space's part of [`cached`].
    fn cached(&self) -> Cached {
        Cached {
            warm: u64::from(self.warm_count.load(Ordering::Relaxed)) * CHUNK as u64,
            cold: u64::from(self.cold_count.load(Ordering::Relaxed)) * CHUNK as u64,
        }
    }

    /// A chunk of the space: a warm one, else a cold one made the process's again
    /// ([`Region::reuse`]), else one carved.
    fn take(&self) -> Result<u32, MemError> {
        if let Some(local) = self.warm.pop(&self.links) {
            self.warm_count.fetch_sub(1, Ordering::Relaxed);
            return Ok(local);
        }
        match self.cold.pop(&self.links) {
            Some(local) => {
                if let Err(error) = self.region.reuse(local as usize * CHUNK, CHUNK) {
                    self.cold.push(local, &self.links);
                    return Err(error);
                }
                self.cold_count.fetch_sub(1, Ordering::Relaxed);
                Ok(local)
            }
            None => carve(self),
        }
    }

    /// Caches `locals`, chunks no owner holds, warm (one compare-and-swap); past `most` warm,
    /// trims the cache down to `keep`.
    fn give(&self, locals: &[u32]) {
        let given = locals.len() as u32;
        let now = self.warm_count.fetch_add(given, Ordering::Relaxed) + given;
        self.warm.push_all(locals, &self.links);
        self.trim_past_most(now);
    }

    /// After a give left `now` warm: past `most`, the cache trimmed down to `keep`.
    fn trim_past_most(&self, now: u32) {
        if now > self.most {
            self.trim_to(self.keep);
        }
    }

    /// Purges warm chunks to the cold stack until `keep` are left (or the stack is empty, or
    /// the kernel keeps a chunk's pages: then it would keep the next one's too); says how many
    /// it purged.
    fn trim_to(&self, keep: u32) -> usize {
        let mut purged = 0;
        while self.warm_count.load(Ordering::Relaxed) > keep {
            let Some(local) = self.warm.pop(&self.links) else { break };
            self.warm_count.fetch_sub(1, Ordering::Relaxed);
            if !self.purge_cold(local) {
                break;
            }
            purged += 1;
        }
        purged
    }

    /// Gives the pages of a chunk no owner holds back to the kernel and caches it cold; says
    /// whether they went ([`Space::cache_purged`]).
    fn purge_cold(&self, local: u32) -> bool {
        // SAFETY: the chunk was given up (by its owner, or popped off the warm cache), so no one
        // holds it or reads its contents before writing them; it is touched again only through
        // a take, which `reuse`s a cold chunk first.
        let given = unsafe { self.region.purge(local as usize * CHUNK, CHUNK) };
        self.cache_purged(local, given)
    }

    /// Caches chunk `local` after a purge gave `given` of its bytes back: cold when it gave
    /// any, else warm. memcap3 review F1: a purge that gave nothing (hugetlb pool pages under
    /// [`crate::huge::Pages::Reserved`], a refused advice, a target with no purge) leaves the pages
    /// resident, and a chunk counted cold is taken off the footprint ([`cached`],
    /// [`crate::tag::Snapshot::footprint`]), so counting it cold would leave out resident
    /// memory. Says whether it went cold.
    fn cache_purged(&self, local: u32, given: usize) -> bool {
        // A chunk is aligned to a huge page and committed whole, so a purge gives back all of it
        // or none. A part given back would still go cold, never warm: a warm take skips `reuse`,
        // which the given pages need (Windows decommitted them).
        debug_assert!(
            given == 0 || given == CHUNK,
            "a purge of a {CHUNK}-byte chunk gave back {given} bytes"
        );
        if given == 0 {
            self.warm_count.fetch_add(1, Ordering::Relaxed);
            self.warm.push(local, &self.links);
            return false;
        }
        self.cold_count.fetch_add(1, Ordering::Relaxed);
        self.cold.push(local, &self.links);
        true
    }
}

#[cfg(all(test, not(blaze_check)))]
mod tests {
    use super::*;

    /// coord-102 (B5): the warm chunk cache had no bound, so the frames of a build's peak (4.72
    /// GiB) stayed resident as warm chunks (3.3 GiB) to the end. A space's cache grows to its
    /// `most` and the give that passes it trims to its `keep`, through one chunk at a time and
    /// through runs, and the chunks purged are taken again, not carved. On a space of the test's
    /// own, so no other test's chunks or `trim` reach it.
    #[test]
    fn a_space_trims_its_warm_chunks_to_keep_once_they_pass_most() {
        const CHUNKS: u32 = 8;
        let space = Space {
            region: Region::reserve_huge(CHUNKS as usize * CHUNK, Tag::Solver)
                .expect("a test's space"),
            links: LinkTable::new(CHUNKS, Tag::Solver).expect("its links"),
            carved: AtomicU32::new(0),
            warm: Stack::new(),
            warm_count: AtomicU32::new(0),
            keep: 2,
            most: 4,
            cold: Stack::new(),
            cold_count: AtomicU32::new(0),
        };
        let warm = || space.warm_count.load(Ordering::Relaxed);
        let taken: Vec<u32> = (0..CHUNKS).map(|_| space.take().expect("a chunk")).collect();
        let mut seen = Vec::new();
        for &local in &taken {
            space.give(&[local]);
            seen.push(warm());
        }
        assert_eq!(
            seen,
            [1, 2, 3, 4, 2, 3, 4, 2],
            "one at a time: up to most, then trimmed to keep"
        );
        let cold = || space.cold_count.load(Ordering::Relaxed);
        assert_eq!(
            cold(),
            6,
            "each purged chunk counts cold, so the memory line can say it is not resident"
        );
        let again: Vec<u32> = (0..CHUNKS).map(|_| space.take().expect("a chunk")).collect();
        assert_eq!(
            space.carved.load(Ordering::Relaxed),
            CHUNKS,
            "the purged chunks are taken again, none carved"
        );
        assert_eq!(warm(), 0);
        assert_eq!(cold(), 0, "a cold chunk taken again counts cold no more");
        space.give(&again);
        assert_eq!(warm(), 2, "a run past most is trimmed to keep");
        assert_eq!(space.trim_to(0), 2);
        assert_eq!(warm(), 0);
    }

    /// memcap3 review F1: a chunk whose purge gave nothing back (hugetlb pool pages under
    /// `Pages::Reserved`, a refused advice, a target with no purge) keeps its pages resident,
    /// so it is cached warm and stays in the footprint: none of its bytes count as purged
    /// (`cached().cold`, what `Snapshot::footprint` takes off committed memory). A chunk whose
    /// purge gave its bytes back goes cold. On a space of the test's own, as above.
    #[test]
    fn a_chunk_whose_purge_gave_nothing_back_stays_warm_and_in_the_footprint() {
        const CHUNKS: u32 = 4;
        let space = Space {
            region: Region::reserve_huge(CHUNKS as usize * CHUNK, Tag::Solver)
                .expect("a test's space"),
            links: LinkTable::new(CHUNKS, Tag::Solver).expect("its links"),
            carved: AtomicU32::new(0),
            warm: Stack::new(),
            warm_count: AtomicU32::new(0),
            keep: 0,
            most: CHUNKS,
            cold: Stack::new(),
            cold_count: AtomicU32::new(0),
        };
        let kept = space.take().expect("a chunk");
        let given = space.take().expect("a chunk");
        assert!(
            !space.cache_purged(kept, 0),
            "a purge that gave nothing back leaves the chunk warm"
        );
        assert_eq!(
            space.cached(),
            Cached { warm: CHUNK as u64, cold: 0 },
            "its pages are resident: none of its bytes come off the footprint"
        );
        assert!(space.cache_purged(given, CHUNK), "a purge that gave the chunk back makes it cold");
        assert_eq!(space.cached(), Cached { warm: CHUNK as u64, cold: CHUNK as u64 });
        assert_eq!(
            space.take().expect("a chunk"),
            kept,
            "the warm chunk is taken first, with no reuse"
        );
        assert_eq!(
            space.take().expect("a chunk"),
            given,
            "then the cold one, made the process's again"
        );
        assert_eq!(space.cached(), Cached::default());
        assert_eq!(space.carved.load(Ordering::Relaxed), 2, "both were taken again, none carved");
    }
}
