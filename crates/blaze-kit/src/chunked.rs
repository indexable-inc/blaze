//! [`ChunkedBitSet<I, A>`]: a bit set over a fixed domain in chunks of [`CHUNK_BITS`] bits,
//! where an all-zero or all-one chunk is one word and a mixed chunk is a counted block that
//! copies of the set share until one writes it: the dataflow state of a large function body,
//! where most chunks of most states are uniform and states are copied at every block entry
//! (after rustc's `ChunkedBitSet`, with the uniform chunks a word instead of an enum with a
//! counted pointer).
//!
//! # Layout and floor
//!
//! A chunk is one pointer word: null for all zeros, the address 1 (never a block's, which is
//! aligned to 8) for all ones, else a block holding a count of references, the count of bits
//! set, a clone of the allocator that made it and 32 words of bits (264 bytes past the
//! allocator). So a set of uniform chunks is 8 bytes per 2,048 ids (a 256th of a bit set's
//! memory); a mixed chunk costs its block once however many copies share it. `contains` reads
//! the chunk word and at most one bit word: one or two lines. A copy of the set is a copy of
//! its chunk words and one count raised per mixed chunk. A union, difference or intersection
//! visits each chunk once; a uniform chunk on either side is decided by its word alone, and two
//! mixed chunks read their 512 bytes and write 256 at most.
//!
//! A mixed chunk's block is written only when no other copy shares it: a write first copies a
//! shared block (copy on write). The counts are not atomic, so the set is neither `Send` nor
//! `Sync`.
//!
//! # When the plain `BitSet` (blaze-ids) is the right one instead
//!
//! When the domain is small (one chunk or a few: a bit set has no chunk word and no count), or
//! when the set is not copied and its bits are scattered so that every chunk is mixed: then
//! this is a bit set plus a word and a count per 2,048 bits.
//!
//! # Measured (`examples/sets.rs`, release, one Mac run, 2026-11-15)
//!
//! A copy and a union, ns, against a plain bit set: 376 against 2,023 at 262,144 ids, 5,621
//! against 21,955 at 4,194,304. A second run (2026-11-15, one run, this machine): 275 against
//! 682, and 4,233 against 17,908.

use core::{
    alloc::{AllocatorClone, Layout},
    any::type_name,
    cell::Cell,
    marker::PhantomData,
    ptr::{self, NonNull},
};

use blaze_int::Int;
use blaze_own::{AllocFailure, Vec};

use crate::{
    error::{fail, refused},
    id::Id,
    plant::{self, Plant},
};

/// Bits in a chunk.
pub const CHUNK_BITS: usize = 2048;

/// Words in a chunk's block.
const WORDS: usize = CHUNK_BITS / 64;

/// A mixed chunk's bits.
///
/// # Invariants
///
/// - `refs` is the number of chunk words (in any set) that point here, at least 1.
/// - `count` is the number of bits set in `words`, strictly between 0 and the chunk's bits.
/// - Bits past the domain (in a set's last chunk) are 0.
/// - `alloc` made this block, for `Layout::new::<Block<A>>()`, and frees it.
/// - `next` links the block into a list of spare blocks while a combine holds it, else null.
#[repr(C)]
struct Block<A> {
    refs: Cell<u32>,
    count: u32,
    next: *mut Block<A>,
    alloc: A,
    words: [u64; WORDS],
}

/// The chunk word of an all-ones chunk: address 1, never a block's (blocks are aligned to 8).
#[inline(always)]
fn ones<A>() -> *mut Block<A> {
    ptr::without_provenance_mut(1)
}

/// What a chunk word means.
enum Chunk<A> {
    Zeros,
    Ones,
    Mixed(*mut Block<A>),
}

#[inline(always)]
fn chunk<A>(word: *mut Block<A>) -> Chunk<A> {
    if word.is_null() {
        Chunk::Zeros
    } else if word == ones() {
        Chunk::Ones
    } else {
        Chunk::Mixed(word)
    }
}

/// The words of a chunk of `bits` bits that are all set.
fn full_words(bits: usize) -> [u64; WORDS] {
    let mut words = [0u64; WORDS];
    for (at, word) in words.iter_mut().enumerate() {
        let from = at * 64;
        if from + 64 <= bits {
            *word = !0;
        } else if from < bits {
            *word = (1u64 << (bits - from)) - 1;
        }
    }
    words
}

fn popcount(words: &[u64; WORDS]) -> u32 {
    words.iter().map(|w| w.count_ones()).sum()
}

/// Lowers a block's count of references; the last one frees it through its own allocator.
///
/// # Safety
///
/// `block` is a live block (its invariants hold) and the caller gives up one reference.
unsafe fn release<A: AllocatorClone>(block: *mut Block<A>) {
    // SAFETY: `block` is live (the caller's contract).
    let refs = unsafe { (*block).refs.get() };
    if refs > 1 {
        // SAFETY: as above.
        unsafe { (*block).refs.set(refs - 1) };
        return;
    }
    // SAFETY: this was the last reference, so nothing reads the block again: the allocator is
    // moved out once, and frees the block it made with the layout it was made with.
    unsafe {
        let alloc = ptr::read(&raw const (*block).alloc);
        alloc.deallocate(NonNull::new_unchecked(block.cast::<u8>()), Layout::new::<Block<A>>());
    }
}

/// Raises a block's count of references for another chunk word.
///
/// # Safety
///
/// `block` is a live block.
#[track_caller]
unsafe fn share<A>(block: *mut Block<A>) {
    // SAFETY: `block` is live (the caller's contract).
    let refs = unsafe { (*block).refs.get() };
    let Some(more) = refs.checked_add(1) else {
        panic!("blaze_kit: a chunk of a `ChunkedBitSet` shared by more than u32::MAX sets");
    };
    // SAFETY: as above.
    unsafe { (*block).refs.set(more) };
}

/// A fresh block of `words` with `count` bits set and one reference, in a clone of `alloc`.
fn new_block<A: AllocatorClone>(alloc: &A, words: [u64; WORDS], count: u32) -> Result<*mut Block<A>, AllocFailure> {
    let layout = Layout::new::<Block<A>>();
    let block = alloc.allocate(layout).map_err(|_| refused(type_name::<Block<A>>(), layout))?;
    let block = block.cast::<Block<A>>().as_ptr();
    // SAFETY: the block is fresh, of `Block<A>`'s layout.
    unsafe {
        block.write(Block { refs: Cell::new(1), count, next: ptr::null_mut(), alloc: alloc.clone(), words });
    }
    Ok(block)
}

/// Spare blocks a combine allocated up front, linked by `next`.
struct Spares<A: AllocatorClone> {
    head: *mut Block<A>,
}

impl<A: AllocatorClone> Spares<A> {
    /// `n` spare blocks, or the refusal with every one made so far freed.
    fn try_new(alloc: &A, n: usize) -> Result<Spares<A>, AllocFailure> {
        let mut spares = Spares { head: ptr::null_mut() };
        for _ in 0..n {
            let block = new_block(alloc, [0; WORDS], 0)?;
            // SAFETY: fresh and live; it joins the list, which `Drop` frees.
            unsafe { (*block).next = spares.head };
            spares.head = block;
        }
        Ok(spares)
    }

    /// A spare block; the combine counted one for every use.
    #[track_caller]
    fn take(&mut self) -> *mut Block<A> {
        let block = self.head;
        if block.is_null() {
            panic!("blaze_kit: a ChunkedBitSet combine took more spare blocks than it counted");
        }
        // SAFETY: a block on the list is live and its `next` is the rest of the list.
        unsafe {
            self.head = (*block).next;
            (*block).next = ptr::null_mut();
        }
        block
    }
}

impl<A: AllocatorClone> Drop for Spares<A> {
    fn drop(&mut self) {
        while !self.head.is_null() {
            let block = self.take();
            // SAFETY: a spare block is live with one reference, the list's, given up here.
            unsafe { release(block) };
        }
    }
}

/// A combine of two sets, chunk by chunk.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Union,
    Subtract,
    Intersect,
}

/// A set of ids `I` below a domain, in chunks (see the module): 8 bytes a uniform chunk of 2,048 ids, 264 a mixed one shared by copies; a copy costs a word a chunk.
pub struct ChunkedBitSet<I: Id, A: AllocatorClone> {
    chunks: Vec<*mut Block<A>, A>,
    domain: usize,
    _id: PhantomData<fn(I) -> I>,
}

impl<I: Id, A: AllocatorClone> ChunkedBitSet<I, A> {
    /// An empty set of ids below `domain`: one word per chunk.
    pub fn try_new_empty_in(domain: u32, alloc: A) -> Result<ChunkedBitSet<I, A>, AllocFailure> {
        ChunkedBitSet::try_filled(domain, alloc, ptr::null_mut())
    }

    /// A set of every id below `domain`.
    pub fn try_new_full_in(domain: u32, alloc: A) -> Result<ChunkedBitSet<I, A>, AllocFailure> {
        ChunkedBitSet::try_filled(domain, alloc, ones())
    }

    /// [`ChunkedBitSet::try_new_empty_in`], panicking with the refusal.
    pub fn new_empty_in(domain: u32, alloc: A) -> ChunkedBitSet<I, A> {
        match ChunkedBitSet::try_new_empty_in(domain, alloc) {
            Ok(set) => set,
            Err(failure) => fail(failure),
        }
    }

    fn try_filled(domain: u32, alloc: A, word: *mut Block<A>) -> Result<ChunkedBitSet<I, A>, AllocFailure> {
        let domain = blaze_int::idx(domain);
        let n = domain.div_ceil(CHUNK_BITS);
        let mut chunks = Vec::try_with_capacity_in(n, alloc)?;
        chunks.resize(n, word);
        Ok(ChunkedBitSet { chunks, domain, _id: PhantomData })
    }

    /// The ids it can hold: `0..domain`.
    pub fn domain(&self) -> usize {
        self.domain
    }

    /// The bits of chunk `k`: [`CHUNK_BITS`], or fewer for the last.
    #[inline]
    fn bits(&self, k: usize) -> usize {
        (self.domain - k * CHUNK_BITS).min(CHUNK_BITS)
    }

    /// The chunk and the bit within it of `id`; an id outside the domain panics.
    #[inline]
    #[track_caller]
    fn place(&self, id: I) -> (usize, usize) {
        let at = id.index();
        if at >= self.domain {
            panic!("blaze_kit: id {at} is outside the domain of `{}`, {}", type_name::<Self>(), self.domain);
        }
        (at / CHUNK_BITS, at % CHUNK_BITS)
    }

    #[inline]
    #[track_caller]
    pub fn contains(&self, id: I) -> bool {
        let (k, bit) = self.place(id);
        match chunk(self.chunks[k]) {
            Chunk::Zeros => false,
            Chunk::Ones => true,
            // SAFETY: a mixed chunk word points at a live block.
            Chunk::Mixed(block) => unsafe { (*block).words[bit / 64] >> (bit % 64) & 1 != 0 },
        }
    }

    /// Chunk `k`'s block, unshared: copied first when another chunk word shares it. Refused
    /// with nothing changed.
    fn writable(&mut self, k: usize) -> Result<*mut Block<A>, AllocFailure> {
        let block = self.chunks[k];
        // SAFETY: chunk `k` is mixed (the caller's), so `block` is live.
        let (refs, words, count) = unsafe { ((*block).refs.get(), (*block).words, (*block).count) };
        if refs == 1 || plant::on(Plant::ChunkWritesShared) {
            return Ok(block);
        }
        let copy = new_block(self.chunks.allocator(), words, count)?;
        // SAFETY: this chunk word gives up its reference to the shared block (`refs > 1`, so it
        // stays live for the others).
        unsafe { release(block) };
        self.chunks[k] = copy;
        Ok(copy)
    }

    /// Adds `id`; whether it was new. Refused with nothing changed when a block cannot be had.
    #[track_caller]
    pub fn try_insert(&mut self, id: I) -> Result<bool, AllocFailure> {
        let (k, bit) = self.place(id);
        let bits = self.bits(k);
        let (word, mask) = (bit / 64, 1u64 << (bit % 64));
        match chunk(self.chunks[k]) {
            Chunk::Ones => Ok(false),
            Chunk::Zeros if bits == 1 => {
                self.chunks[k] = ones();
                Ok(true)
            }
            Chunk::Zeros => {
                let mut words = [0u64; WORDS];
                words[word] = mask;
                self.chunks[k] = new_block(self.chunks.allocator(), words, 1)?;
                Ok(true)
            }
            Chunk::Mixed(block) => {
                // SAFETY: a mixed chunk word points at a live block.
                if unsafe { (*block).words[word] } & mask != 0 {
                    return Ok(false);
                }
                let block = self.writable(k)?;
                // SAFETY: `block` is live and this chunk's alone (or the plant's shared one).
                let count = unsafe {
                    (*block).words[word] |= mask;
                    (*block).count += 1;
                    (*block).count
                };
                if blaze_int::idx(count) == bits {
                    // SAFETY: the chunk word gives up its block for the ones word.
                    unsafe { release(block) };
                    self.chunks[k] = ones();
                }
                Ok(true)
            }
        }
    }

    /// [`ChunkedBitSet::try_insert`], panicking with the refusal.
    #[track_caller]
    pub fn insert(&mut self, id: I) -> bool {
        match self.try_insert(id) {
            Ok(new) => new,
            Err(failure) => fail(failure),
        }
    }

    /// Takes `id` out; whether it was in. Refused with nothing changed when a block cannot be
    /// had (taking one id out of an all-ones chunk makes it mixed).
    #[track_caller]
    pub fn try_remove(&mut self, id: I) -> Result<bool, AllocFailure> {
        let (k, bit) = self.place(id);
        let bits = self.bits(k);
        let (word, mask) = (bit / 64, 1u64 << (bit % 64));
        match chunk(self.chunks[k]) {
            Chunk::Zeros => Ok(false),
            Chunk::Ones if bits == 1 => {
                self.chunks[k] = ptr::null_mut();
                Ok(true)
            }
            Chunk::Ones => {
                let mut words = full_words(bits);
                words[word] &= !mask;
                // `bits - 1` fits 32 bits: a chunk has at most 2,048.
                self.chunks[k] = new_block(self.chunks.allocator(), words, (bits - 1).truncate_to::<u32>())?;
                Ok(true)
            }
            Chunk::Mixed(block) => {
                // SAFETY: a mixed chunk word points at a live block.
                if unsafe { (*block).words[word] } & mask == 0 {
                    return Ok(false);
                }
                let block = self.writable(k)?;
                // SAFETY: as in `try_insert`.
                let count = unsafe {
                    (*block).words[word] &= !mask;
                    (*block).count -= 1;
                    (*block).count
                };
                if count == 0 {
                    // SAFETY: the chunk word gives up its block for the zeros word.
                    unsafe { release(block) };
                    self.chunks[k] = ptr::null_mut();
                }
                Ok(true)
            }
        }
    }

    /// [`ChunkedBitSet::try_remove`], panicking with the refusal.
    #[track_caller]
    pub fn remove(&mut self, id: I) -> bool {
        match self.try_remove(id) {
            Ok(was) => was,
            Err(failure) => fail(failure),
        }
    }

    /// Sets every chunk to `word`, giving up every block.
    fn fill(&mut self, word: *mut Block<A>) {
        for slot in self.chunks.iter_mut() {
            if let Chunk::Mixed(block) = chunk(*slot) {
                // SAFETY: the chunk word gives up its live block.
                unsafe { release(block) };
            }
            *slot = word;
        }
    }

    /// Empties the set.
    pub fn clear(&mut self) {
        self.fill(ptr::null_mut());
    }

    /// Puts every id of the domain in.
    pub fn insert_all(&mut self) {
        self.fill(ones());
    }

    /// How many ids are in.
    pub fn count(&self) -> usize {
        let mut total = 0;
        for (k, &word) in self.chunks.iter().enumerate() {
            total += match chunk(word) {
                Chunk::Zeros => 0,
                Chunk::Ones => self.bits(k),
                // SAFETY: a mixed chunk word points at a live block.
                Chunk::Mixed(block) => blaze_int::idx(unsafe { (*block).count }),
            };
        }
        total
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.iter().all(|word| word.is_null())
    }

    /// How many chunks are mixed, each a block (for a cost check).
    pub fn mixed_chunks(&self) -> usize {
        self.chunks.iter().filter(|&&word| matches!(chunk(word), Chunk::Mixed(_))).count()
    }

    /// The ids in, ascending.
    pub fn iter(&self) -> impl Iterator<Item = I> + '_ {
        self.chunks.iter().enumerate().flat_map(move |(k, &word)| {
            let base = k * CHUNK_BITS;
            let (range, words): (core::ops::Range<usize>, Option<&[u64; WORDS]>) = match chunk(word) {
                Chunk::Zeros => (0..0, None),
                Chunk::Ones => (base..base + self.bits(k), None),
                // SAFETY: a mixed chunk word points at a live block, kept live by `&self`.
                Chunk::Mixed(block) => (0..0, Some(unsafe { &(*block).words })),
            };
            let ones = range.map(|at| I::from_u32(at.truncate_to::<u32>()));
            let mixed = words.into_iter().flat_map(move |words| {
                words.iter().enumerate().flat_map(move |(w, &bits)| {
                    let mut left = bits;
                    core::iter::from_fn(move || {
                        if left == 0 {
                            return None;
                        }
                        let bit = blaze_int::idx(left.trailing_zeros());
                        left &= left - 1;
                        Some(I::from_u32((base + w * 64 + bit).truncate_to::<u32>()))
                    })
                })
            });
            ones.chain(mixed)
        })
    }

    /// Adds every id of `other`; whether any was new. Refused with nothing changed.
    #[track_caller]
    pub fn try_union(&mut self, other: &ChunkedBitSet<I, A>) -> Result<bool, AllocFailure> {
        self.combine(other, Op::Union)
    }

    /// Takes every id of `other` out; whether any was in. Refused with nothing changed.
    #[track_caller]
    pub fn try_subtract(&mut self, other: &ChunkedBitSet<I, A>) -> Result<bool, AllocFailure> {
        self.combine(other, Op::Subtract)
    }

    /// Keeps only the ids `other` has too; whether any went. Refused with nothing changed.
    #[track_caller]
    pub fn try_intersect(&mut self, other: &ChunkedBitSet<I, A>) -> Result<bool, AllocFailure> {
        self.combine(other, Op::Intersect)
    }

    /// [`ChunkedBitSet::try_union`], panicking with the refusal.
    #[track_caller]
    pub fn union(&mut self, other: &ChunkedBitSet<I, A>) -> bool {
        match self.try_union(other) {
            Ok(changed) => changed,
            Err(failure) => fail(failure),
        }
    }

    /// Whether chunk `k`'s block (mixed) has other owners, so writing it needs a spare.
    fn shared(word: *mut Block<A>) -> bool {
        // SAFETY: called for mixed chunk words only, which point at live blocks.
        unsafe { (*word).refs.get() > 1 }
    }

    /// The two passes of a combine: count the spare blocks it could need and make them, so
    /// nothing can be refused half way; then each chunk once.
    #[track_caller]
    fn combine(&mut self, other: &ChunkedBitSet<I, A>, op: Op) -> Result<bool, AllocFailure> {
        if self.domain != other.domain {
            panic!(
                "blaze_kit: a combine of `{}`s of domains {} and {}",
                type_name::<Self>(),
                self.domain,
                other.domain
            );
        }
        let mut need = 0usize;
        for (&s, &o) in self.chunks.iter().zip(other.chunks.iter()) {
            need += usize::from(match (chunk(s), chunk(o)) {
                (Chunk::Ones, Chunk::Mixed(_)) => op == Op::Subtract,
                (Chunk::Mixed(a), Chunk::Mixed(b)) => a != b && ChunkedBitSet::<I, A>::shared(a),
                _ => false,
            });
        }
        let mut spares = Spares::try_new(self.chunks.allocator(), need)?;
        let mut changed = false;
        for k in 0..self.chunks.len() {
            let (s, o) = (self.chunks[k], other.chunks[k]);
            let bits = self.bits(k);
            let next = match (op, chunk(s), chunk(o)) {
                // Decided by a uniform word.
                (Op::Union, _, Chunk::Zeros) | (Op::Union, Chunk::Ones, _) => s,
                (Op::Union, _, Chunk::Ones) => ones(),
                (Op::Subtract, Chunk::Zeros, _) | (Op::Subtract, _, Chunk::Zeros) => s,
                (Op::Subtract, _, Chunk::Ones) => ptr::null_mut(),
                (Op::Intersect, Chunk::Zeros, _) | (Op::Intersect, _, Chunk::Ones) => s,
                (Op::Intersect, _, Chunk::Zeros) => ptr::null_mut(),
                // Takes the other's block, shared.
                (Op::Union, Chunk::Zeros, Chunk::Mixed(b)) | (Op::Intersect, Chunk::Ones, Chunk::Mixed(b)) => {
                    // SAFETY: `b` is live; this chunk word takes a reference.
                    unsafe { share(b) };
                    b
                }
                // The complement of the other's block, in a spare.
                (Op::Subtract, Chunk::Ones, Chunk::Mixed(b)) => {
                    let full = full_words(bits);
                    let fresh = spares.take();
                    // SAFETY: `b` is live; `fresh` is a spare, live and unshared.
                    unsafe {
                        for w in 0..WORDS {
                            (*fresh).words[w] = full[w] & !(*b).words[w];
                        }
                        // `bits` fits 32 bits: a chunk has at most 2,048.
                        (*fresh).count = bits.truncate_to::<u32>() - (*b).count;
                    }
                    fresh
                }
                (_, Chunk::Mixed(a), Chunk::Mixed(b)) if a == b => {
                    if op == Op::Subtract { ptr::null_mut() } else { s }
                }
                (_, Chunk::Mixed(a), Chunk::Mixed(b)) => {
                    let mut out = [0u64; WORDS];
                    // SAFETY: both blocks are live; read only.
                    let (aw, bw) = unsafe { (&(*a).words, &(*b).words) };
                    for w in 0..WORDS {
                        out[w] = match op {
                            Op::Union => aw[w] | bw[w],
                            Op::Subtract => aw[w] & !bw[w],
                            Op::Intersect => aw[w] & bw[w],
                        };
                    }
                    if out == *aw {
                        s
                    } else {
                        let count = popcount(&out);
                        if count == 0 {
                            ptr::null_mut()
                        } else if blaze_int::idx(count) == bits {
                            ones()
                        } else if ChunkedBitSet::<I, A>::shared(a) {
                            let fresh = spares.take();
                            // SAFETY: `fresh` is a spare, live and unshared.
                            unsafe {
                                (*fresh).words = out;
                                (*fresh).count = count;
                            }
                            fresh
                        } else {
                            // SAFETY: `a` is live and this chunk's alone: written in place.
                            unsafe {
                                (*a).words = out;
                                (*a).count = count;
                            }
                            changed = true;
                            s
                        }
                    }
                }
            };
            if next != s {
                if let Chunk::Mixed(block) = chunk(s) {
                    // SAFETY: the chunk word gives up its live block for `next`.
                    unsafe { release(block) };
                }
                self.chunks[k] = next;
                changed = true;
            }
        }
        Ok(changed)
    }

    /// A copy in a clone of the allocator, its mixed chunks shared; refused with nothing made.
    pub fn try_clone(&self) -> Result<ChunkedBitSet<I, A>, AllocFailure> {
        let mut chunks = Vec::try_with_capacity_in(self.chunks.len(), self.chunks.allocator().clone())?;
        for &word in self.chunks.iter() {
            if let Chunk::Mixed(block) = chunk(word) {
                // SAFETY: `block` is live; the copy's chunk word takes a reference.
                unsafe { share(block) };
            }
            chunks.push(word);
        }
        Ok(ChunkedBitSet { chunks, domain: self.domain, _id: PhantomData })
    }
}

impl<I: Id, A: AllocatorClone> Clone for ChunkedBitSet<I, A> {
    fn clone(&self) -> Self {
        match self.try_clone() {
            Ok(copy) => copy,
            Err(failure) => fail(failure),
        }
    }
}

impl<I: Id, A: AllocatorClone> PartialEq for ChunkedBitSet<I, A> {
    fn eq(&self, other: &Self) -> bool {
        self.domain == other.domain
            && self.chunks.iter().zip(other.chunks.iter()).all(|(&s, &o)| match (chunk(s), chunk(o)) {
                (Chunk::Zeros, Chunk::Zeros) | (Chunk::Ones, Chunk::Ones) => true,
                // SAFETY: mixed chunk words point at live blocks.
                (Chunk::Mixed(a), Chunk::Mixed(b)) => a == b || unsafe { (*a).words == (*b).words },
                _ => false,
            })
    }
}

impl<I: Id, A: AllocatorClone> Eq for ChunkedBitSet<I, A> {}

impl<I: Id, A: AllocatorClone> Drop for ChunkedBitSet<I, A> {
    fn drop(&mut self) {
        self.clear();
    }
}
