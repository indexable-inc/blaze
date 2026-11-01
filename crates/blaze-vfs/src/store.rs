//! The tree's nodes, stored once and read from any thread without a lock
//! (docs/v2/plan/design/concurrency.md, docs/v2/plan/design/data.md). The crate's one unsafe
//! module (docs/v2/plan/design/unsafe.md): everything it offers is safe to call.
//!
//! - A [`Table`] is one generation of nodes: append-only columns indexed by a node's `u32`, in
//!   chunks that never move, and an open-addressed index from content ID to node, probed by the
//!   ID's own bits (never hashed again) and grown by adding a level.
//! - Each column publishes its length with a release store after writing the values below it,
//!   and every read is checked against the length it loads with acquire: a read past what is
//!   written panics, it never reads uninitialized memory. A value is pushed before anything
//!   that names it (a node before its index place, entries and children before their
//!   directory), so a node reached through the index or a directory reads whole.
//! - Only the [`Writer`] pushes, through `&mut self`, and there is one per [`Shared`] (made
//!   with it, never cloned); a collection fills a [`Building`] it alone holds. A reader reaches
//!   a table only through a [`Snapshot`], which is in a task ([`blaze_reclaim::enter`]) for as
//!   long as anything it lends lives; a replaced table is freed once every such task has ended.
//! - Names are interned once, as spans of one byte column; a table hands its names on to the
//!   next generation with their numbers.
//! - A table restored from the daemon's image (docs/v2/plan/design/restore.md) starts with a
//!   **base**: its nodes, entries, names and both indexes are the image's own bytes, checked
//!   once ([`Base::check`]) and read in place; nodes made after are appended in the columns
//!   with numbers after the base's, and the index of the appended ones is probed after the
//!   base's. A file of the base is a range of the image's blobs section, checked by its page
//!   groups' CRC32C the first time it is read ([`Mapped::read`]).
//!
//! Why unsafe: the columns are read while the writer appends, with no lock (one acquire load
//! and a compare per read). The safe version, a `RwLock<Vec<_>>` per column or a table copied
//! per commit, costs every read a shared write (the lock's count) or every commit a copy of the
//! tree. A base is borrowed from the mapped image the table holds, for as long as it holds it.
//!
//! Costs: a node is 32 bytes of ID plus an 8-byte [`Shape`], and 4 bytes of index place at
//! most half full; a directory entry is 9 bytes (name, node, mode byte) in three columns; a
//! file's bytes are one allocation shared with every reader (an `Arc<[u8]>`, or the `Vec` the
//! op that wrote them owned, never copied), or a range of the image. Finding
//! a root is one probe of about 1.5 places (two when a base is there and the node is not in
//! it); a path component is a binary search over the directory's name column. A base costs
//! nothing in memory: it is the image's pages, read when something reads them.

use std::{
    marker::PhantomData,
    mem::{MaybeUninit, needs_drop, size_of_val},
    ptr,
    sync::{Arc, OnceLock},
};

use blaze_bytes::Bytes;
use blaze_core::Mapping;
use blaze_ids::ContentId;
use blaze_image::{Corrupt, Image, Section};
use blaze_reclaim::{Guard, Limbo};

use crate::{
    Mode,
    sync::{
        Mutex,
        atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering},
    },
};

/// `bytes` held where they are: their `Vec` becomes what the lent slice is of, so a file written
/// with bytes its op owned (or an edit made) is stored with no copy. One allocation, the `Arc`
/// holding the `Vec`'s three words, whatever the length.
pub(crate) fn adopt(bytes: Vec<u8>) -> Bytes {
    // alloc: one per new file, the owner of its bytes; the bytes themselves are not copied.
    let owner = Arc::new(bytes);
    let lent: *const [u8] = owner.as_slice();
    // SAFETY: a `Vec`'s buffer stays at one address, unwritten, until the `Vec` is grown, written
    // or dropped. `owner` is moved into the `Bytes` as an `Arc<dyn Send + Sync>`, through which
    // nothing can reach the `Vec` to grow or write it, and the `Bytes` hold it for as long as
    // they lend the slice.
    unsafe { Bytes::held(lent, owner) }
}

/// `len` bytes at `at` of `mapped`'s blobs section, once each page group they fall in matches
/// its CRC32C.
fn mapped_bytes(mapped: Arc<Mapped>, at: u64, len: u64) -> Result<Bytes, Corrupt> {
    let bytes: *const [u8] = mapped.read(at, len)?;
    // SAFETY: the bytes are the image's, which `mapped` holds through its `Arc<Image>`: the
    // image's owner (`Mapping`, `Stable`) keeps them at one address, unwritten, for as long as
    // it lives, and the `Bytes` hold `mapped` for as long as they lend them.
    Ok(unsafe { Bytes::held(bytes, mapped) })
}

/// Chunk `k` of a column holds `FIRST << k` values from offset `FIRST * (2^k - 1)`, so an
/// offset's chunk is arithmetic and 25 chunks cover every `u32`.
const FIRST: usize = 1 << 8;
const CHUNKS: usize = 25;

fn chunk_of(at: usize) -> (usize, usize) {
    let biased = at / FIRST + 1;
    let chunk = (usize::BITS - 1 - biased.leading_zeros()) as usize;
    (chunk, at - FIRST * ((1 << chunk) - 1))
}

const fn chunk_start(chunk: usize) -> usize {
    FIRST * ((1 << chunk) - 1)
}

/// The first offset at or after `cursor` where `len` values fit in one chunk, so they can be
/// read as one slice. Skipping to the next chunk wastes at most that chunk's tail.
fn run(cursor: u32, len: u32) -> u32 {
    let mut start = cursor as usize;
    loop {
        let (chunk, place) = chunk_of(start);
        if place + len as usize <= FIRST << chunk {
            assert!(start + len as usize <= u32::MAX as usize, "a column of 2^32 values");
            return start as u32;
        }
        start = chunk_start(chunk + 1);
    }
}

/// On a 128-byte line of its own (the pair arm64 and x86 prefetch together), so the writer's
/// length stores do not take the line holding the chunk pointers from readers.
#[repr(align(128))]
struct Padded<T>(T);

/// An append-only column whose values never move: chunks are allocated as it grows and kept
/// until it is dropped. Readers see the prefix below `len`. Its values are `Send` and `Sync`:
/// pushed on the writer's thread through `&self`, read on any, dropped on whichever drops it.
struct Column<T: Send + Sync> {
    len: Padded<AtomicU32>,
    chunks: [AtomicPtr<T>; CHUNKS],
    of: PhantomData<T>,
}

impl<T: Send + Sync> Column<T> {
    fn new() -> Column<T> {
        Column {
            len: Padded(AtomicU32::new(0)),
            chunks: std::array::from_fn(|_| AtomicPtr::new(ptr::null_mut())),
            of: PhantomData,
        }
    }

    /// How many values are written and readable.
    fn len(&self) -> u32 {
        self.len.0.load(Ordering::Acquire)
    }

    /// The value at `at`; panics past what is written.
    fn get(&self, at: u32) -> &T {
        let len = self.len();
        assert!(at < len, "value {at} of a column of {len}");
        let (chunk, place) = chunk_of(at as usize);
        let base = self.chunks[chunk].load(Ordering::Acquire);
        // SAFETY: `at < len`, and `len` was stored with release after `at`'s chunk was
        // allocated (a block of `FIRST << chunk` values, which `place` is below) and its value
        // written: the offset stays in that block.
        let value = unsafe { base.add(place) };
        // SAFETY: as above the value is initialized; it is never written again and its chunk
        // is freed only when the column drops, which `&self` outlives.
        unsafe { &*value }
    }

    /// The chunk, allocated if it is not yet.
    fn chunk(&self, chunk: usize) -> *mut T {
        let at = self.chunks[chunk].load(Ordering::Acquire);
        if !at.is_null() {
            return at;
        }
        let fresh = Box::into_raw(Box::<[T]>::new_uninit_slice(FIRST << chunk)).cast::<T>();
        self.chunks[chunk].store(fresh, Ordering::Release);
        fresh
    }

    /// Writes `value` at the end and publishes it; returns its offset.
    ///
    /// # Safety
    ///
    /// The caller is the column's one writer: no other thread pushes to it at the same time.
    /// Otherwise two pushes write the same slot and one value is lost or torn.
    unsafe fn push(&self, value: T) -> u32 {
        let at = self.len.0.load(Ordering::Relaxed);
        assert!(at < u32::MAX, "a column of 2^32 values");
        let (chunk, place) = chunk_of(at as usize);
        let base = self.chunk(chunk);
        // SAFETY: `base` is a block of `FIRST << chunk` values and `place` is below that.
        let slot = unsafe { base.add(place) };
        // SAFETY: the slot is at `len`, past what any reader may read, and the caller is the
        // only writer, so nothing else touches it.
        unsafe { slot.write(value) };
        self.len.0.store(at + 1, Ordering::Release);
        at
    }
}

impl<T: Copy + Send + Sync> Column<T> {
    /// The `len` values from `start`; panics past what is written or across chunks.
    fn slice(&self, start: u32, len: u32) -> &[T] {
        if len == 0 {
            return &[];
        }
        let written = self.len();
        assert!(
            start.checked_add(len).is_some_and(|end| end <= written),
            "values {start}..+{len} of a column of {written}"
        );
        let (chunk, place) = chunk_of(start as usize);
        assert!(place + len as usize <= FIRST << chunk, "a run across chunks");
        let base = self.chunks[chunk].load(Ordering::Acquire);
        // SAFETY: the run is within one chunk, as checked, and its chunk is allocated (it was
        // before `written` was stored).
        let first = unsafe { base.add(place) };
        // SAFETY: the `len` values from `first` are in one block, written before `written`
        // was stored with release and never written again; the block lives as long as `&self`.
        unsafe { std::slice::from_raw_parts(first, len as usize) }
    }

    /// Pushes the `len` values of `values` in one chunk, padding with `filler` up to where
    /// they fit; returns where they start.
    ///
    /// # Safety
    ///
    /// As [`Column::push`].
    unsafe fn push_run(&self, len: u32, filler: T, values: impl IntoIterator<Item = T>) -> u32 {
        let start = run(self.len.0.load(Ordering::Relaxed), len);
        while self.len.0.load(Ordering::Relaxed) < start {
            // SAFETY: the caller is the one writer.
            unsafe { self.push(filler) };
        }
        let mut pushed = 0;
        for value in values {
            // SAFETY: as above.
            unsafe { self.push(value) };
            pushed += 1;
        }
        assert_eq!(pushed, len, "a run of the length it said");
        start
    }
}

impl<T: Send + Sync> Drop for Column<T> {
    fn drop(&mut self) {
        let len = *self.len.0.get_mut() as usize;
        for (chunk, base) in self.chunks.iter_mut().enumerate() {
            let base = *base.get_mut();
            if base.is_null() {
                continue;
            }
            let size = FIRST << chunk;
            if needs_drop::<T>() {
                let written = len.saturating_sub(chunk_start(chunk)).min(size);
                // SAFETY: chunks fill in order, so the first `written` values of this one are
                // the column's values below `len`: initialized, and dropped once, here.
                unsafe { ptr::drop_in_place(ptr::slice_from_raw_parts_mut(base, written)) };
            }
            let all = ptr::slice_from_raw_parts_mut(base.cast::<MaybeUninit<T>>(), size);
            // SAFETY: `chunk` allocated `base` as a box of exactly `size` values, and nothing
            // reads it any more (`&mut self`).
            drop(unsafe { Box::from_raw(all) });
        }
    }
}

/// A node's place in the columns. 8 bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub(crate) struct Shape {
    /// A directory's first entry; a file's place in `blobs`.
    pub(crate) start: u32,
    /// A directory's number of entries; [`FILE`] for a file.
    pub(crate) len: u32,
}

const _: () = assert!(size_of::<Shape>() == 8);

pub(crate) const FILE: u32 = u32::MAX;

impl Shape {
    pub(crate) fn is_dir(self) -> bool {
        self.len != FILE
    }
}

/// An entry's mode byte: a file's [`Mode`], or [`DIR`].
pub(crate) const DIR: u8 = 3;

pub(crate) const fn mode_byte(mode: Mode) -> u8 {
    match mode {
        Mode::Regular => 0,
        Mode::Executable => 1,
        Mode::Symlink => 2,
    }
}

/// The mode an entry's byte gives: [`Mode::Regular`] for a directory, as the tree says.
pub(crate) const fn mode_of(byte: u8) -> Mode {
    match byte {
        1 => Mode::Executable,
        2 => Mode::Symlink,
        _ => Mode::Regular,
    }
}

/// A directory's entries, sorted by name bytes: three columns of one range.
#[derive(Clone, Copy)]
pub(crate) struct Entries<'a> {
    pub(crate) names: &'a [u32],
    pub(crate) nodes: &'a [u32],
    pub(crate) modes: &'a [u8],
}

impl Entries<'_> {
    pub(crate) fn len(&self) -> usize {
        self.names.len()
    }

    pub(crate) fn is_dir(&self, at: usize) -> bool {
        self.modes[at] == DIR
    }

    /// The entry named `name`, by binary search of the name column.
    pub(crate) fn find(&self, names: &Names, name: &[u8]) -> Option<usize> {
        self.names.binary_search_by(|&it| names.get(it).cmp(name)).ok()
    }
}

/// The hash a name is found by, in the tree's index of names and in the image's: FxHash's
/// multiply and rotate over little-endian words, then the high bits folded down, since a
/// place is the low bits. Fixed here, not taken from a crate, as the image keeps its places.
pub(crate) fn name_hash(bytes: &[u8]) -> u64 {
    const K: u64 = 0xf135_7aea_2e62_a9c5;
    let mut hash = bytes.len() as u64;
    let mut words = bytes.chunks_exact(8);
    for word in &mut words {
        let word = u64::from_le_bytes(word.try_into().expect("8 bytes"));
        hash = (hash.rotate_left(26) ^ word).wrapping_mul(K);
    }
    let rest = words.remainder();
    if !rest.is_empty() {
        let mut word = [0; 8];
        word[..rest.len()].copy_from_slice(rest);
        hash = (hash.rotate_left(26) ^ u64::from_le_bytes(word)).wrapping_mul(K);
    }
    hash ^ (hash >> 32)
}

/// Every name, interned once: a span of one byte column each, after the base's.
pub(crate) struct Names {
    /// The base's spans, bytes and index of names (by [`name_hash`], a name plus one a place,
    /// 0 empty): the image's, borrowed for as long as the table holding these names holds it.
    base_spans: &'static [[u32; 2]],
    base_bytes: &'static [u8],
    base_index: &'static [u32],
    spans: Column<Span>,
    bytes: Column<u8>,
}

/// 8 bytes.
#[derive(Clone, Copy)]
#[repr(C)]
struct Span {
    start: u32,
    len: u32,
}

const _: () = assert!(size_of::<Span>() == 8);

impl Names {
    fn new() -> Names {
        Names {
            base_spans: &[],
            base_bytes: &[],
            base_index: &[],
            spans: Column::new(),
            bytes: Column::new(),
        }
    }

    fn base_len(&self) -> u32 {
        self.base_spans.len() as u32
    }

    /// How many names there are.
    pub(crate) fn len(&self) -> u32 {
        self.base_len() + self.spans.len()
    }

    /// The bytes of `name`; panics for a name not interned.
    pub(crate) fn get(&self, name: u32) -> &[u8] {
        match name.checked_sub(self.base_len()) {
            None => {
                let [start, len] = self.base_spans[name as usize];
                &self.base_bytes[start as usize..start as usize + len as usize]
            }
            Some(at) => {
                let span = *self.spans.get(at);
                self.bytes.slice(span.start, span.len)
            }
        }
    }

    /// `name` as the `&str` it was interned from.
    pub(crate) fn str(&self, name: u32) -> &str {
        let bytes = self.get(name);
        debug_assert!(std::str::from_utf8(bytes).is_ok());
        // SAFETY: spans are pushed only by `intern`, each right after the bytes of a `&str`,
        // and cover exactly them; pushed values never change. A base's spans were each checked
        // to be UTF-8 by `Base::check` before the base was taken. So these bytes are UTF-8.
        unsafe { std::str::from_utf8_unchecked(bytes) }
    }

    /// The base's name whose bytes are `name` and whose [`name_hash`] is `hash`.
    pub(crate) fn find_base(&self, hash: u64, name: &[u8]) -> Option<u32> {
        let places = self.base_index;
        let mask = places.len().checked_sub(1)?;
        let mut at = hash as usize & mask;
        for _ in 0..places.len() {
            match places[at] {
                0 => return None,
                plus_one if self.get(plus_one - 1) == name => return Some(plus_one - 1),
                _ => at = (at + 1) & mask,
            }
        }
        None
    }

    /// Interns `name`, new; returns its number.
    ///
    /// # Safety
    ///
    /// As [`Column::push`]: the caller is the one writer.
    unsafe fn intern(&self, name: &str) -> u32 {
        let len = u32::try_from(name.len()).expect("a name under 4 GiB");
        // SAFETY: the caller is the one writer.
        let start = unsafe { self.bytes.push_run(len, 0, name.bytes()) };
        // SAFETY: as above.
        let at = unsafe { self.spans.push(Span { start, len }) };
        self.base_len().checked_add(at).expect("fewer than 2^32 names")
    }

    /// The same names with the same numbers, for the next generation: the base's borrowed
    /// again (the next table holds the same image), the rest copied.
    fn carry(&self) -> Names {
        let next = Names {
            base_spans: self.base_spans,
            base_bytes: self.base_bytes,
            base_index: self.base_index,
            ..Names::new()
        };
        for at in 0..self.spans.len() {
            let span = *self.spans.get(at);
            let bytes = self.bytes.slice(span.start, span.len);
            // SAFETY: `next` is new and only this thread has it.
            let start = unsafe { next.bytes.push_run(span.len, 0, bytes.iter().copied()) };
            // SAFETY: as above.
            unsafe { next.spans.push(Span { start, len: span.len }) };
        }
        next
    }
}

/// The daemon's image a table was restored from: what keeps its base's bytes mapped, and where
/// its files' bytes are. Shared by every generation that has a node of it, and by every
/// [`Bytes`] lent from it.
pub(crate) struct Mapped {
    pub(crate) image: Arc<Image<Mapping>>,
    pub(crate) blobs: Section,
    /// The first group of `blobs` a read found corrupt: what [`crate::Vfs::corrupt`] names.
    corrupt: OnceLock<Corrupt>,
}

impl Mapped {
    pub(crate) fn new(image: Arc<Image<Mapping>>, blobs: Section) -> Mapped {
        Mapped { image, blobs, corrupt: OnceLock::new() }
    }

    /// `len` bytes at `at` of the blobs section, each page group they fall in checked by its
    /// CRC32C the first time it is read; a group that fails is never read, and is kept as the
    /// image's first corruption.
    pub(crate) fn read(&self, at: u64, len: u64) -> Result<&[u8], Corrupt> {
        self.image.range(&self.blobs, at, len).inspect_err(|corrupt| {
            let _ = self.corrupt.set(*corrupt);
        })
    }

    pub(crate) fn corrupt(&self) -> Option<Corrupt> {
        self.corrupt.get().copied()
    }
}

/// The first generation's columns, read in place from a mapped image: what [`Writer::restore`]
/// starts a table with. Node `n` of the base is `ids[n]` and `shapes[n]` (`[start, len]`); a
/// directory's entries are `[start, start + len)` of the entry columns, a file is `files[start]`
/// (where its bytes are in the blobs section, and how many). `index` holds a node plus one at
/// the place its ID's [`home`] probes to, linearly; `name_index` a name plus one by
/// [`name_hash`]; both are a power of two long and at most half full.
#[derive(Clone, Copy, Default)]
pub(crate) struct Base<'a> {
    pub(crate) ids: &'a [ContentId],
    pub(crate) shapes: &'a [[u32; 2]],
    pub(crate) entry_names: &'a [u32],
    pub(crate) entry_nodes: &'a [u32],
    pub(crate) entry_modes: &'a [u8],
    pub(crate) files: &'a [[u64; 2]],
    pub(crate) index: &'a [u32],
    pub(crate) spans: &'a [[u32; 2]],
    pub(crate) name_bytes: &'a [u8],
    pub(crate) name_index: &'a [u32],
}

impl Base<'_> {
    /// Whether every read of the base stays in it and every walk of it ends: each name a path
    /// component of UTF-8 its span holds; each directory's entries in range, sorted by name
    /// and naming only nodes before it, of a mode that is the child's kind; each file's bytes
    /// in the blobs section (`blobs` long); each node found by the index where it is. The
    /// image's CRCs say its bytes are those written; this says the writer wrote a tree.
    pub(crate) fn check(&self, blobs: u64) -> Result<(), &'static str> {
        let (nodes, entries) = (self.ids.len(), self.entry_names.len());
        if nodes == 0 || nodes >= FILE as usize || entries >= FILE as usize {
            return Err("a base of no nodes, or of 2^32");
        }
        if self.shapes.len() != nodes
            || self.entry_nodes.len() != entries
            || self.entry_modes.len() != entries
        {
            return Err("node or entry columns of different lengths");
        }
        let half_full = |places: &[u32], count: usize| {
            places.len().is_power_of_two() && places.len() >= 2 * (count + 1)
        };
        if !half_full(self.index, nodes) || !half_full(self.name_index, self.spans.len()) {
            return Err("an index that is not a power of two at most half full");
        }
        for &[start, len] in self.spans {
            let name = (start as usize)
                .checked_add(len as usize)
                .and_then(|end| self.name_bytes.get(start as usize..end))
                .ok_or("a name past the name bytes")?;
            let name = std::str::from_utf8(name).map_err(|_| "a name that is not UTF-8")?;
            if name.is_empty() || name == "." || name == ".." || name.contains('/') {
                return Err("a name that is not one path component");
            }
        }
        let name = |name: u32| -> &[u8] {
            let [start, len] = self.spans[name as usize];
            &self.name_bytes[start as usize..start as usize + len as usize]
        };
        for (node, &[start, len]) in self.shapes.iter().enumerate() {
            if len == FILE {
                let &[at, len] =
                    self.files.get(start as usize).ok_or("a file past the file column")?;
                if at.checked_add(len).is_none_or(|end| end > blobs) {
                    return Err("a file past the blobs section");
                }
                continue;
            }
            let range = start as usize..start as usize + len as usize;
            if range.end > entries {
                return Err("a directory's entries past the entry columns");
            }
            for at in range.start..range.end {
                let (child, mode, label) =
                    (self.entry_nodes[at], self.entry_modes[at], self.entry_names[at]);
                if child as usize >= node {
                    return Err("an entry naming a node not before its directory");
                }
                if label as usize >= self.spans.len() {
                    return Err("an entry of a name the base does not have");
                }
                let child_is_dir = self.shapes[child as usize][1] != FILE;
                if mode > DIR || (mode == DIR) != child_is_dir {
                    return Err("an entry whose mode is not its node's kind");
                }
                if at > range.start && name(self.entry_names[at - 1]) >= name(label) {
                    return Err("a directory's entries not sorted by name, or a name twice");
                }
            }
        }
        if self.index.iter().any(|&plus_one| plus_one as usize > nodes) {
            return Err("an index place naming a node the base does not have");
        }
        if self.name_index.iter().any(|&plus_one| plus_one as usize > self.spans.len()) {
            return Err("a name index place naming a name the base does not have");
        }
        // At most half full, so a probe for an ID or a name the base lacks ends at an empty
        // place. Where each node and name is placed is the writer's (tested), not checked
        // here: one misplaced is a lookup that misses, never a read out of bounds.
        let used = |places: &[u32]| places.iter().filter(|&&it| it != 0).count();
        if used(self.index) > nodes || used(self.name_index) > self.spans.len() {
            return Err("an index fuller than its nodes or names");
        }
        Ok(())
    }

    /// Whether every slice of the base lies in `bytes`.
    fn within(&self, bytes: &[u8]) -> bool {
        let (start, end) = (bytes.as_ptr() as usize, bytes.as_ptr() as usize + bytes.len());
        let inside = |at: *const u8, len: usize| {
            let at = at as usize;
            len == 0 || (at >= start && at.checked_add(len).is_some_and(|it| it <= end))
        };
        inside(self.ids.as_ptr().cast(), size_of_val(self.ids))
            && inside(self.shapes.as_ptr().cast(), size_of_val(self.shapes))
            && inside(self.entry_names.as_ptr().cast(), size_of_val(self.entry_names))
            && inside(self.entry_nodes.as_ptr().cast(), size_of_val(self.entry_nodes))
            && inside(self.entry_modes.as_ptr(), self.entry_modes.len())
            && inside(self.files.as_ptr().cast(), size_of_val(self.files))
            && inside(self.index.as_ptr().cast(), size_of_val(self.index))
            && inside(self.spans.as_ptr().cast(), size_of_val(self.spans))
            && inside(self.name_bytes.as_ptr(), self.name_bytes.len())
            && inside(self.name_index.as_ptr().cast(), size_of_val(self.name_index))
    }
}

/// Level `k` of the index has `PLACES << k` places; a place holds a node plus one, 0 empty.
const PLACES: usize = 64;
const LEVELS: usize = 28;

struct Index {
    level: AtomicUsize,
    levels: [AtomicPtr<AtomicU32>; LEVELS],
}

fn places(len: usize) -> *mut AtomicU32 {
    Box::into_raw((0..len).map(|_| AtomicU32::new(0)).collect::<Box<[AtomicU32]>>()).cast()
}

/// Where `id`'s probe starts: its own first 64 bits, uniform as a BLAKE3 hash is.
pub(crate) fn home(id: &ContentId) -> usize {
    let mut word = [0; 8];
    word.copy_from_slice(&id.0[..8]);
    u64::from_le_bytes(word) as usize
}

impl Index {
    /// An index whose first level holds `count` nodes at most half full.
    fn new(count: usize) -> Index {
        let mut level = 0;
        while (count + 1) * 2 > PLACES << level {
            level += 1;
        }
        let index = Index {
            level: AtomicUsize::new(level),
            levels: std::array::from_fn(|_| AtomicPtr::new(ptr::null_mut())),
        };
        index.levels[level].store(places(PLACES << level), Ordering::Release);
        index
    }

    fn places(&self, level: usize) -> &[AtomicU32] {
        let at = self.levels[level].load(Ordering::Acquire);
        assert!(!at.is_null(), "index level {level} is not allocated");
        // SAFETY: a level is allocated by `places` as `PLACES << level` atomics before
        // anything names it, never moved, and freed only when the index drops, which `&self`
        // outlives.
        unsafe { std::slice::from_raw_parts(at, PLACES << level) }
    }
}

impl Drop for Index {
    fn drop(&mut self) {
        for (level, at) in self.levels.iter_mut().enumerate() {
            let at = *at.get_mut();
            if !at.is_null() {
                let all = ptr::slice_from_raw_parts_mut(at, PLACES << level);
                // SAFETY: allocated by `places` with exactly this many places; nothing reads
                // it any more (`&mut self`).
                drop(unsafe { Box::from_raw(all) });
            }
        }
    }
}

/// Where a file node made after the base keeps its bytes: its own (an `Arc<[u8]>`, or the
/// `Vec` an op wrote, [`adopt`]ed), or the range of the image's blobs section a base file had
/// before a collection renumbered it.
enum Blob {
    Heap(Bytes),
    Mapped { at: u64, len: u64 },
}

/// Where [`Table::blob_at`] found a file's bytes: a base file's place, or the column's value.
enum BlobAt<'a> {
    Base(&'a u64, &'a u64),
    Column(&'a Blob),
}

/// Where a file's bytes are, without reading them: for the image's writer, which copies them.
pub(crate) enum Source<'a> {
    Heap(&'a [u8]),
    /// `len` bytes at `at` of the blobs section of [`Table::mapped`].
    Mapped { at: u64, len: u64 },
}

/// One generation of nodes: a base (empty unless restored from an image), then the columns.
/// Node `n` is the base's below `base.ids.len()`, else the columns' `n - base.ids.len()`; a
/// shape's `start` counts the base's entries (or files) first the same way.
pub(crate) struct Table {
    /// Borrowed from `mapped`'s image, which this table holds for as long as it lives.
    base: Base<'static>,
    names: Names,
    ids: Column<ContentId>,
    shapes: Column<Shape>,
    blobs: Column<Blob>,
    entry_names: Column<u32>,
    entry_nodes: Column<u32>,
    entry_modes: Column<u8>,
    /// The nodes after the base's.
    index: Index,
    mapped: Option<Arc<Mapped>>,
}

impl Table {
    /// A table whose index holds `count` nodes without growing.
    fn new(count: usize) -> Table {
        Table {
            base: Base::default(),
            names: Names::new(),
            ids: Column::new(),
            shapes: Column::new(),
            blobs: Column::new(),
            entry_names: Column::new(),
            entry_nodes: Column::new(),
            entry_modes: Column::new(),
            index: Index::new(count),
            mapped: None,
        }
    }

    fn base_nodes(&self) -> u32 {
        self.base.ids.len() as u32
    }

    fn base_entries(&self) -> u32 {
        self.base.entry_names.len() as u32
    }

    fn base_files(&self) -> u32 {
        self.base.files.len() as u32
    }

    /// How many nodes it holds.
    pub(crate) fn len(&self) -> u32 {
        self.base_nodes() + self.ids.len()
    }

    /// How many of its nodes are the base's: those below are read from the image.
    pub(crate) fn base_len(&self) -> u32 {
        self.base_nodes()
    }

    pub(crate) fn names(&self) -> &Names {
        &self.names
    }

    /// The image its base and its mapped files are in.
    pub(crate) fn mapped(&self) -> Option<&Arc<Mapped>> {
        self.mapped.as_ref()
    }

    /// The node whose content ID is `id`.
    pub(crate) fn find(&self, id: &ContentId) -> Option<u32> {
        self.probe(id).ok()
    }

    /// The node whose content ID is `id`; else its free place in the level in use.
    fn probe(&self, id: &ContentId) -> Result<u32, usize> {
        if let Some(node) = self.probe_base(id) {
            return Ok(node);
        }
        let places = self.index.places(self.index.level.load(Ordering::Acquire));
        self.probe_in(places, id)
    }

    /// The base's node whose content ID is `id`: plain loads of the image's places, which
    /// [`Base::check`] found at most half full, so the probe meets an empty one.
    fn probe_base(&self, id: &ContentId) -> Option<u32> {
        let places = self.base.index;
        let mask = places.len().checked_sub(1)?;
        let mut at = home(id) & mask;
        for _ in 0..places.len() {
            match places[at] {
                0 => return None,
                plus_one if self.base.ids[plus_one as usize - 1] == *id => {
                    return Some(plus_one - 1);
                }
                _ => at = (at + 1) & mask,
            }
        }
        None
    }

    fn probe_in(&self, places: &[AtomicU32], id: &ContentId) -> Result<u32, usize> {
        let mask = places.len() - 1;
        let mut at = home(id) & mask;
        loop {
            match places[at].load(Ordering::Acquire) {
                0 => return Err(at),
                plus_one if self.id(plus_one - 1) == id => return Ok(plus_one - 1),
                _ => at = (at + 1) & mask,
            }
        }
    }

    pub(crate) fn id(&self, node: u32) -> &ContentId {
        match node.checked_sub(self.base_nodes()) {
            None => &self.base.ids[node as usize],
            Some(at) => self.ids.get(at),
        }
    }

    pub(crate) fn shape(&self, node: u32) -> Shape {
        match node.checked_sub(self.base_nodes()) {
            None => {
                let [start, len] = self.base.shapes[node as usize];
                Shape { start, len }
            }
            Some(at) => *self.shapes.get(at),
        }
    }

    /// The entries of a directory's shape; panics for a file's.
    pub(crate) fn entries(&self, shape: Shape) -> Entries<'_> {
        assert!(shape.is_dir(), "a file has no entries");
        match shape.start.checked_sub(self.base_entries()) {
            // A base directory's entries are all in the base, as `Base::check` found.
            None => {
                let start = shape.start as usize;
                let end = start + shape.len as usize;
                Entries {
                    names: &self.base.entry_names[start..end],
                    nodes: &self.base.entry_nodes[start..end],
                    modes: &self.base.entry_modes[start..end],
                }
            }
            Some(start) => Entries {
                names: self.entry_names.slice(start, shape.len),
                nodes: self.entry_nodes.slice(start, shape.len),
                modes: self.entry_modes.slice(start, shape.len),
            },
        }
    }

    /// The bytes of a file's shape, read in place; panics for a directory's. A file of the
    /// image is read only once the page groups it is in match their CRC32C.
    pub(crate) fn blob(&self, shape: Shape) -> Result<&[u8], Corrupt> {
        match self.source(shape) {
            Source::Heap(bytes) => Ok(bytes),
            Source::Mapped { at, len } => self.mapped_ref().read(at, len),
        }
    }

    /// The bytes of a file's shape as [`Table::blob`] reads them, held: one count of the
    /// file's `Arc` or of the image's.
    pub(crate) fn bytes(&self, shape: Shape) -> Result<Bytes, Corrupt> {
        match self.blob_at(shape) {
            BlobAt::Base(&at, &len) | BlobAt::Column(&Blob::Mapped { at, len }) => {
                // clone: one count of the image, which the bytes lend from.
                mapped_bytes(self.mapped_ref().clone(), at, len)
            }
            // clone: one count of the file's owner, never its bytes.
            BlobAt::Column(Blob::Heap(bytes)) => Ok(bytes.clone()),
        }
    }

    /// Where a file's bytes are, read or not; panics for a directory.
    pub(crate) fn source(&self, shape: Shape) -> Source<'_> {
        match self.blob_at(shape) {
            BlobAt::Base(&at, &len) | BlobAt::Column(&Blob::Mapped { at, len }) => {
                Source::Mapped { at, len }
            }
            BlobAt::Column(Blob::Heap(bytes)) => Source::Heap(&bytes[..]),
        }
    }

    /// How many bytes a file has, without reading them.
    pub(crate) fn file_len(&self, shape: Shape) -> u64 {
        match self.source(shape) {
            Source::Heap(bytes) => bytes.len() as u64,
            Source::Mapped { len, .. } => len,
        }
    }

    fn blob_at(&self, shape: Shape) -> BlobAt<'_> {
        assert!(!shape.is_dir(), "a directory has no bytes");
        match shape.start.checked_sub(self.base_files()) {
            None => {
                let [at, len] = &self.base.files[shape.start as usize];
                BlobAt::Base(at, len)
            }
            Some(at) => BlobAt::Column(self.blobs.get(at)),
        }
    }

    fn mapped_ref(&self) -> &Arc<Mapped> {
        self.mapped.as_ref().expect("a table with a file of an image holds the image")
    }

    /// The node of the file `id` whose bytes are `blob`, added if it is new.
    ///
    /// # Safety
    ///
    /// As [`Column::push`]: the caller is the table's one writer.
    unsafe fn add_file(&self, id: ContentId, blob: Blob) -> u32 {
        let place = match self.probe(&id) {
            Ok(node) => return node,
            Err(place) => place,
        };
        if matches!(blob, Blob::Mapped { .. }) {
            assert!(self.mapped.is_some(), "a file of an image in a table without it");
        }
        // SAFETY: the caller is the one writer.
        let at = unsafe { self.blobs.push(blob) };
        let start = self.base_files().checked_add(at).filter(|&it| it != FILE);
        let start = start.expect("fewer than 2^32 - 1 files");
        // SAFETY: as above.
        unsafe { self.add(id, Shape { start, len: FILE }, place) }
    }

    /// The node of the directory `id` with `entries` (name, node, mode byte), added if it is
    /// new.
    ///
    /// # Safety
    ///
    /// As [`Column::push`].
    unsafe fn add_dir(&self, id: ContentId, entries: &[(u32, u32, u8)]) -> u32 {
        let place = match self.probe(&id) {
            Ok(node) => return node,
            Err(place) => place,
        };
        let len = u32::try_from(entries.len()).ok().filter(|&it| it != FILE);
        let len = len.expect("fewer than 2^32 - 1 entries");
        // SAFETY: the caller is the one writer.
        let start = unsafe { self.entry_names.push_run(len, 0, entries.iter().map(|it| it.0)) };
        // SAFETY: as above.
        let nodes = unsafe { self.entry_nodes.push_run(len, 0, entries.iter().map(|it| it.1)) };
        // SAFETY: as above.
        let modes = unsafe { self.entry_modes.push_run(len, 0, entries.iter().map(|it| it.2)) };
        assert!(start == nodes && start == modes, "the entry columns are pushed together");
        let start = self.base_entries().checked_add(start);
        let start = start.filter(|it| it.checked_add(len).is_some()).expect("2^32 entries");
        // SAFETY: as above.
        unsafe { self.add(id, Shape { start, len }, place) }
    }

    /// Adds a node whose blob or entries are pushed, then indexes it at `place` (from
    /// [`Table::probe`]), which publishes it.
    ///
    /// # Safety
    ///
    /// As [`Column::push`].
    unsafe fn add(&self, id: ContentId, shape: Shape, place: usize) -> u32 {
        // SAFETY: the caller is the one writer.
        let at = unsafe { self.shapes.push(shape) };
        // SAFETY: as above.
        let pushed = unsafe { self.ids.push(id) };
        assert_eq!(at, pushed, "the node columns are pushed together");
        let node = self.base_nodes().checked_add(at).expect("fewer than 2^32 nodes");
        self.index(&id, node, place);
        node
    }

    /// Stores `node` at `place` of the level in use, after growing the index if the nodes
    /// after the base would fill it past half. The one writer's.
    fn index(&self, id: &ContentId, node: u32, place: usize) {
        let mut level = self.index.level.load(Ordering::Relaxed);
        let mut place = place;
        if ((node - self.base_nodes()) as usize + 1) * 2 > PLACES << level {
            level = self.grow(level);
            let Err(free) = self.probe_in(self.index.places(level), id) else {
                unreachable!("a node indexed twice")
            };
            place = free;
        }
        self.index.places(level)[place].store(node + 1, Ordering::Release);
    }

    /// Copies every place into a level twice the size and makes it the one in use. A reader
    /// probing the old level still finds every node indexed before; the old level is freed
    /// with the table.
    fn grow(&self, level: usize) -> usize {
        assert!(level + 1 < LEVELS, "an index of 2^32 nodes");
        let next = level + 1;
        self.index.levels[next].store(places(PLACES << next), Ordering::Release);
        let new = self.index.places(next);
        let mask = new.len() - 1;
        for place in self.index.places(level) {
            let plus_one = place.load(Ordering::Relaxed);
            if plus_one == 0 {
                continue;
            }
            let mut at = home(self.id(plus_one - 1)) & mask;
            while new[at].load(Ordering::Relaxed) != 0 {
                at = (at + 1) & mask;
            }
            new[at].store(plus_one, Ordering::Relaxed);
        }
        self.index.level.store(next, Ordering::Release);
        next
    }
}

/// What the tree and every reader share. Made only by [`Writer::new`] and [`Writer::restore`].
pub(crate) struct Shared {
    table: AtomicPtr<Table>,
    /// The tables replaced by collections, until no reader's task can be in them. Locked by
    /// the writer alone (readers never touch it), so never contended.
    retired: Mutex<Limbo>,
}

impl Shared {
    /// The tree as it is now, for reading from this thread.
    pub(crate) fn read(&self) -> Snapshot<'_> {
        let task = blaze_reclaim::enter();
        let table = self.table.load(Ordering::Acquire);
        // SAFETY: `table` is the published `Box`. A replaced one is freed only after a grace
        // started once it was unlinked has passed, which waits for this task (entered before
        // the load) to end; the `Snapshot` holds the task, and lends the table only for as
        // long as it lives.
        let table = unsafe { &*table };
        Snapshot { table, _task: task }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        let table = *self.table.get_mut();
        // SAFETY: the last owner: no reader or writer is left, so nobody reads the published
        // `Box` (the limbo drops the retired tables with it, for the same reason).
        drop(unsafe { Box::from_raw(table) });
    }
}

/// The tree as it was when [`Shared::read`] was called. Not `Send`: its task is the thread's.
pub(crate) struct Snapshot<'a> {
    table: &'a Table,
    _task: Guard,
}

impl Snapshot<'_> {
    pub(crate) fn table(&self) -> &Table {
        self.table
    }

    pub(crate) fn names(&self) -> &Names {
        &self.table.names
    }
}

/// The one writer of a [`Shared`]: made with it and never cloned, so every push goes through
/// its `&mut self`.
pub(crate) struct Writer {
    shared: Arc<Shared>,
}

impl Writer {
    pub(crate) fn new() -> Writer {
        Writer::publish(Table::new(0))
    }

    fn publish(table: Table) -> Writer {
        // alloc: once per store made, the first published table.
        let table = Box::into_raw(Box::new(table));
        let shared = Shared { table: AtomicPtr::new(table), retired: Mutex::new(Limbo::new()) };
        // alloc: once per store made, what readers share.
        Writer { shared: Arc::new(shared) }
    }

    /// A tree whose first generation is `base`, read in place from `mapped`'s image. `base`
    /// must lie in the image's bytes (else `Err`, and nothing is borrowed) and be a tree
    /// ([`Base::check`]).
    pub(crate) fn restore(mapped: Arc<Mapped>, base: Base<'_>) -> Result<Writer, &'static str> {
        if !base.within(mapped.image.owner().bytes()) {
            return Err("a base that is not in its image's bytes");
        }
        base.check(mapped.blobs.len)?;
        // SAFETY: every slice of `base` lies in the image's bytes (checked above), which stay
        // at one address, unwritten, for as long as the image lives (`Stable`'s contract). The
        // table holds the image through `mapped` for its whole life, and lends the base only
        // through `&self`, so no borrow outlives it.
        let base: Base<'static> = unsafe { std::mem::transmute::<Base<'_>, Base<'static>>(base) };
        let names = Names {
            base_spans: base.spans,
            base_bytes: base.name_bytes,
            base_index: base.name_index,
            ..Names::new()
        };
        let table = Table { base, names, mapped: Some(mapped), ..Table::new(0) };
        Ok(Writer::publish(table))
    }

    /// A handle for readers.
    pub(crate) fn shared(&self) -> Arc<Shared> {
        // clone: one count per reader handle made, never per read.
        self.shared.clone()
    }

    /// The table in use.
    pub(crate) fn table(&self) -> &Table {
        let table = self.shared.table.load(Ordering::Acquire);
        // SAFETY: only `replace`, through `&mut self`, unlinks the published table, so it
        // outlives this borrow of `self`.
        unsafe { &*table }
    }

    pub(crate) fn names(&self) -> &Names {
        &self.table().names
    }

    /// Interns `name`, which is not interned yet; returns its number.
    pub(crate) fn intern(&mut self, name: &str) -> u32 {
        // SAFETY: `&mut self`: the one writer.
        unsafe { self.table().names.intern(name) }
    }

    /// The node of the file `id` with `bytes`, added if it is new.
    pub(crate) fn file(&mut self, id: ContentId, bytes: impl Into<Bytes>) -> u32 {
        // SAFETY: `&mut self`: the one writer.
        unsafe { self.table().add_file(id, Blob::Heap(bytes.into())) }
    }

    /// The node of the directory `id` with `entries` (name, node, mode byte), added if new.
    pub(crate) fn dir(&mut self, id: ContentId, entries: &[(u32, u32, u8)]) -> u32 {
        // SAFETY: `&mut self`: the one writer.
        unsafe { self.table().add_dir(id, entries) }
    }

    /// Publishes `next` in place of the table in use, retiring that one; returns how many
    /// retired tables are not freed yet.
    pub(crate) fn replace(&mut self, next: Building) -> usize {
        // alloc: once per collection or repair, the table published.
        let next = Box::into_raw(Box::new(next.0));
        let old = self.shared.table.swap(next, Ordering::AcqRel);
        // SAFETY: `old` was the published `Box`, now unlinked; the limbo becomes its only
        // owner and frees it once every task that could have loaded it has ended.
        let old = unsafe { Box::from_raw(old) };
        let mut retired = self.shared.retired.lock();
        retired.retire(old);
        retired.collect(); // alloc: none, the limbo's sweep, not an iterator's collect
        retired.len()
    }

    /// Frees each retired table no reader's task can be in; returns how many are left.
    pub(crate) fn reclaim(&mut self) -> usize {
        let mut retired = self.shared.retired.lock();
        retired.collect(); // alloc: none, the limbo's sweep, not an iterator's collect
        retired.len()
    }
}

/// The next generation, filled by a collection before [`Writer::replace`] publishes it.
pub(crate) struct Building(Table);

impl Building {
    /// A table for `count` nodes.
    #[cfg(test)]
    pub(crate) fn new(count: usize) -> Building {
        Building(Table::new(count))
    }

    /// A table for `count` nodes after `from`: its names with their numbers, and its image, so
    /// a file of the image stays where it is ([`Building::file_of`]).
    pub(crate) fn after(from: &Table, count: usize) -> Building {
        // clone: one count of the image, kept by the next generation.
        let (names, mapped) = (from.names.carry(), from.mapped.clone());
        Building(Table { names, mapped, ..Table::new(count) })
    }

    pub(crate) fn file(&mut self, id: ContentId, bytes: impl Into<Bytes>) -> u32 {
        // SAFETY: `&mut self` on a table nobody else has yet.
        unsafe { self.0.add_file(id, Blob::Heap(bytes.into())) }
    }

    /// The file `id` whose bytes are those of the file `shape` of `from`, the table this one
    /// is [`Building::after`]: the same `Arc`, or the same range of the image, unread.
    pub(crate) fn file_of(&mut self, id: ContentId, from: &Table, shape: Shape) -> u32 {
        let blob = match from.blob_at(shape) {
            BlobAt::Base(&at, &len) | BlobAt::Column(&Blob::Mapped { at, len }) => {
                Blob::Mapped { at, len }
            }
            // clone: one count of the file's owner, never its bytes.
            BlobAt::Column(Blob::Heap(bytes)) => Blob::Heap(bytes.clone()),
        };
        // SAFETY: as above.
        unsafe { self.0.add_file(id, blob) }
    }

    pub(crate) fn dir(&mut self, id: ContentId, entries: &[(u32, u32, u8)]) -> u32 {
        // SAFETY: as above.
        unsafe { self.0.add_dir(id, entries) }
    }
}

#[cfg(test)]
mod tests;
