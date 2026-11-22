//! Zero-overhead performance primitives in `bstd::mem`: explicit-allocator, `#![no_std]`,
//! zero-`unsafe` data structures that eliminate the most common manual hot-path patterns across
//! Blaze (inline small buffers, O(1)-clear generation-stamped memo tables, `const fn` O(1)
//! discriminant bitsets, and O(log log N) safeguarded interpolation search over uniform digests).

use alloc::vec::Vec;
use core::alloc::Allocator;
use core::cmp::Ordering;
use core::hash::{Hash, Hasher};
use core::ops::{Deref, DerefMut};

use crate::Stopped;

// ---------------------------------------------------------------------------------------------
// SmallVec: inline stack buffer up to N, spilling to explicit allocator A past N.
// ---------------------------------------------------------------------------------------------

enum SmallStorage<T, const N: usize, A: Allocator> {
    Inline { buf: [T; N], len: usize, alloc: A },
    Spilled(Vec<T, A>),
}

/// A vector that stores up to `N` `Copy + Default` elements inline on the stack with zero heap
/// allocations, spilling transparently into the caller's explicit allocator `A` only past `N`.
///
/// Designed for hot-path IDs, type argument lists, AST stacks, and small batches where 95%+ of
/// calls stay below `N` (e.g. `N = 8` or `N = 16`).
pub struct SmallVec<T, const N: usize, A: Allocator> {
    storage: SmallStorage<T, N, A>,
}

impl<T: Copy + Default, const N: usize, A: Allocator + Clone> SmallVec<T, N, A> {
    /// Creates an empty `SmallVec` backed by explicit allocator `alloc` (no allocation occurs
    /// until `len > N`).
    #[inline]
    pub fn new_in(alloc: A) -> Self {
        Self { storage: SmallStorage::Inline { buf: [T::default(); N], len: 0, alloc } }
    }

    /// Number of elements currently stored.
    #[inline]
    pub fn len(&self) -> usize {
        match &self.storage {
            SmallStorage::Inline { len, .. } => *len,
            SmallStorage::Spilled(v) => v.len(),
        }
    }

    /// Whether the vector holds zero elements.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether storage has spilled from the inline stack array to the backing allocator `A`.
    #[inline]
    pub fn is_spilled(&self) -> bool {
        matches!(self.storage, SmallStorage::Spilled(_))
    }

    #[inline]
    pub fn as_slice(&self) -> &[T] {
        match &self.storage {
            SmallStorage::Inline { buf, len, .. } => &buf[..*len],
            SmallStorage::Spilled(v) => v.as_slice(),
        }
    }

    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        match &mut self.storage {
            SmallStorage::Inline { buf, len, .. } => &mut buf[..*len],
            SmallStorage::Spilled(v) => v.as_mut_slice(),
        }
    }

    /// Appends `value`, spilling into `A` if `len == N`.
    #[inline]
    pub fn push(&mut self, value: T) {
        match &mut self.storage {
            SmallStorage::Inline { buf, len, alloc } => {
                if *len < N {
                    buf[*len] = value;
                    *len += 1;
                    return;
                }
                let mut v = Vec::with_capacity_in(N.saturating_mul(2).max(4), alloc.clone());
                v.extend_from_slice(&buf[..*len]);
                v.push(value);
                self.storage = SmallStorage::Spilled(v);
            }
            SmallStorage::Spilled(v) => v.push(value),
        }
    }

    /// Fallibly appends `value`, returning `Stopped::OutOfMemory` if spilling or growing fails.
    pub fn try_push(&mut self, value: T, arena: &'static str) -> Result<(), Stopped> {
        match &mut self.storage {
            SmallStorage::Inline { buf, len, alloc } => {
                if *len < N {
                    buf[*len] = value;
                    *len += 1;
                    return Ok(());
                }
                let want = N.saturating_mul(2).max(4);
                let mut v = Vec::new_in(alloc.clone());
                v.try_reserve(want).map_err(|_| Stopped::OutOfMemory {
                    arena,
                    asked: want.saturating_mul(core::mem::size_of::<T>()),
                })?;
                v.extend_from_slice(&buf[..*len]);
                v.push(value);
                self.storage = SmallStorage::Spilled(v);
                Ok(())
            }
            SmallStorage::Spilled(v) => {
                v.try_reserve(1).map_err(|_| Stopped::OutOfMemory {
                    arena,
                    asked: core::mem::size_of::<T>(),
                })?;
                v.push(value);
                Ok(())
            }
        }
    }

    /// Appends all elements of `slice`.
    pub fn extend_from_slice(&mut self, slice: &[T]) {
        if slice.is_empty() {
            return;
        }
        match &mut self.storage {
            SmallStorage::Inline { buf, len, alloc } => {
                if *len + slice.len() <= N {
                    buf[*len..*len + slice.len()].copy_from_slice(slice);
                    *len += slice.len();
                    return;
                }
                let mut v = Vec::with_capacity_in((*len + slice.len()).max(N * 2), alloc.clone());
                v.extend_from_slice(&buf[..*len]);
                v.extend_from_slice(slice);
                self.storage = SmallStorage::Spilled(v);
            }
            SmallStorage::Spilled(v) => v.extend_from_slice(slice),
        }
    }

    /// Removes and returns the last element, or `None` if empty.
    #[inline]
    pub fn pop(&mut self) -> Option<T> {
        match &mut self.storage {
            SmallStorage::Inline { buf, len, .. } => {
                if *len == 0 {
                    return None;
                }
                *len -= 1;
                Some(buf[*len])
            }
            SmallStorage::Spilled(v) => v.pop(),
        }
    }

    /// Empties the vector while retaining any already-spilled capacity.
    #[inline]
    pub fn clear(&mut self) {
        match &mut self.storage {
            SmallStorage::Inline { len, .. } => *len = 0,
            SmallStorage::Spilled(v) => v.clear(),
        }
    }
}

impl<T: Copy + Default, const N: usize, A: Allocator + Clone> Deref for SmallVec<T, N, A> {
    type Target = [T];
    #[inline]
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T: Copy + Default, const N: usize, A: Allocator + Clone> DerefMut for SmallVec<T, N, A> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

impl<T: Copy + Default + core::fmt::Debug, const N: usize, A: Allocator + Clone> core::fmt::Debug
    for SmallVec<T, N, A>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries(self.as_slice().iter()).finish()
    }
}

// ---------------------------------------------------------------------------------------------
// GenMap: O(1)-clear generation-stamped flat open-addressed hash table (explicit allocator A).
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct FixedMixer(u64);

impl FixedMixer {
    const SEED: u64 = 0x9e37_79b9_7f4a_7c15;
    #[inline]
    fn finish_u64(mut x: u64) -> u64 {
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        x ^ (x >> 31)
    }
}

impl Hasher for FixedMixer {
    #[inline]
    fn finish(&self) -> u64 {
        Self::finish_u64(self.0)
    }
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut h = self.0;
        let mut i = 0;
        while i + 8 <= bytes.len() {
            let w = u64::from_le_bytes([
                bytes[i],
                bytes[i + 1],
                bytes[i + 2],
                bytes[i + 3],
                bytes[i + 4],
                bytes[i + 5],
                bytes[i + 6],
                bytes[i + 7],
            ]);
            h = (h ^ w).wrapping_mul(0x517c_c1b7_2722_0a95);
            i += 8;
        }
        while i < bytes.len() {
            h = (h ^ u64::from(bytes[i])).wrapping_mul(0x517c_c1b7_2722_0a95);
            i += 1;
        }
        self.0 = h;
    }
}

#[inline]
fn hash_key<K: Hash>(key: &K) -> u64 {
    let mut h = FixedMixer(FixedMixer::SEED);
    key.hash(&mut h);
    h.finish()
}

#[derive(Clone, Copy)]
struct Slot<K, V> {
    epoch: u32,
    key: K,
    val: V,
}

/// An **O(1)-clear**, deterministic, flat open-addressed hash table backed by explicit allocator
/// `A`.
///
/// Calling [`GenMap::clear`] increments a 32-bit generation counter without touching or zeroing
/// any slot memory (only zeroing once every 2^32 clears on wraparound), making per-query,
/// per-unit, and per-cell memo tables dramatically faster than `HashMap::clear()`.
pub struct GenMap<K, V, A: Allocator> {
    slots: Vec<Slot<K, V>, A>,
    mask: usize,
    len: usize,
    epoch: u32,
}

impl<K: Copy + Eq + Hash + Default, V: Copy + Default, A: Allocator + Clone> GenMap<K, V, A> {
    /// Creates an empty `GenMap` (allocates nothing until the first `insert`).
    #[inline]
    pub fn new_in(alloc: A) -> Self {
        Self { slots: Vec::new_in(alloc), mask: 0, len: 0, epoch: 1 }
    }

    /// Creates a `GenMap` pre-sized to hold at least `expected` live entries at ≤ 50% load.
    pub fn with_capacity_in(expected: usize, alloc: A) -> Self {
        if expected == 0 {
            return Self::new_in(alloc);
        }
        let cap = (expected.saturating_mul(2)).next_power_of_two().max(16);
        let mut slots = Vec::with_capacity_in(cap, alloc);
        slots.resize(cap, Slot { epoch: 0, key: K::default(), val: V::default() });
        Self { slots, mask: cap - 1, len: 0, epoch: 1 }
    }

    /// Number of entries live in the current generation.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether no entries are live in the current generation.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Total slot capacity of the backing table.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Current generation number.
    #[inline]
    pub fn generation(&self) -> u32 {
        self.epoch
    }

    /// Clears all entries in **O(1)** time by advancing the generation stamp (zero memory writes
    /// except on 32-bit wraparound).
    #[inline]
    pub fn clear(&mut self) {
        self.len = 0;
        if self.epoch == u32::MAX {
            for s in &mut self.slots {
                s.epoch = 0;
            }
            self.epoch = 1;
        } else {
            self.epoch += 1;
        }
    }

    /// Looks up `key` in the current generation.
    #[inline]
    pub fn get(&self, key: &K) -> Option<V> {
        if self.len == 0 {
            return None;
        }
        let mut idx = (hash_key(key) as usize) & self.mask;
        let epoch = self.epoch;
        // Bounded: load factor is <= 50%, so an empty/stale slot is always found within `slots.len()`.
        loop {
            let slot = self.slots[idx];
            if slot.epoch != epoch {
                return None;
            }
            if slot.key == *key {
                return Some(slot.val);
            }
            idx = (idx + 1) & self.mask;
        }
    }

    /// Inserts `(key, val)` in the current generation, returning any previous value for `key`.
    #[inline]
    pub fn insert(&mut self, key: K, val: V) -> Option<V> {
        if self.slots.is_empty() || (self.len + 1) * 2 > self.slots.len() {
            self.grow();
        }
        let mut idx = (hash_key(&key) as usize) & self.mask;
        let epoch = self.epoch;
        loop {
            let slot = &mut self.slots[idx];
            if slot.epoch != epoch {
                *slot = Slot { epoch, key, val };
                self.len += 1;
                return None;
            }
            if slot.key == key {
                let prev = slot.val;
                slot.val = val;
                return Some(prev);
            }
            idx = (idx + 1) & self.mask;
        }
    }

    /// Single-probe lookup-or-insert: computes `make()` only on a miss.
    #[inline]
    pub fn get_or_insert_with(&mut self, key: K, make: impl FnOnce() -> V) -> V {
        if self.slots.is_empty() || (self.len + 1) * 2 > self.slots.len() {
            self.grow();
        }
        let mut idx = (hash_key(&key) as usize) & self.mask;
        let epoch = self.epoch;
        loop {
            let slot = &mut self.slots[idx];
            if slot.epoch != epoch {
                let val = make();
                *slot = Slot { epoch, key, val };
                self.len += 1;
                return val;
            }
            if slot.key == key {
                return slot.val;
            }
            idx = (idx + 1) & self.mask;
        }
    }

    #[cold]
    fn grow(&mut self) {
        let new_cap = (self.slots.len().saturating_mul(2)).max(32);
        let alloc = self.slots.allocator().clone();
        let mut next = Vec::with_capacity_in(new_cap, alloc);
        next.resize(new_cap, Slot { epoch: 0, key: K::default(), val: V::default() });
        let new_mask = new_cap - 1;
        let old_epoch = self.epoch;
        for slot in &self.slots {
            if slot.epoch == old_epoch {
                let mut idx = (hash_key(&slot.key) as usize) & new_mask;
                while next[idx].epoch == 1 {
                    idx = (idx + 1) & new_mask;
                }
                next[idx] = Slot { epoch: 1, key: slot.key, val: slot.val };
            }
        }
        self.slots = next;
        self.mask = new_mask;
        self.epoch = 1;
    }
}

// ---------------------------------------------------------------------------------------------
// KindSet: compile-time `const fn` O(1) bitset for enum discriminants and ASCII byte classes.
// ---------------------------------------------------------------------------------------------

/// A fixed-size `const fn` bitset over `W * 64` dense integer kinds (e.g. `KindSet<4>` covers
/// 256 syntax kinds or all byte values). Replaces 15–20-branch `matches!` chains with one shift
/// and one bit test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KindSet<const W: usize> {
    words: [u64; W],
}

impl<const W: usize> KindSet<W> {
    /// Empty bitset.
    pub const fn empty() -> Self {
        Self { words: [0u64; W] }
    }

    /// Returns a copy with `bit` set. Panics (compile error in `const`) if `bit >= W * 64`.
    pub const fn with(mut self, bit: usize) -> Self {
        assert!(bit < W * 64, "KindSet bit index out of range");
        self.words[bit >> 6] |= 1u64 << (bit & 63);
        self
    }

    /// Returns a copy with every bit in `lo..=hi` set.
    pub const fn with_range(mut self, lo: usize, hi: usize) -> Self {
        assert!(lo <= hi && hi < W * 64, "KindSet range out of bounds");
        let mut b = lo;
        while b <= hi {
            self.words[b >> 6] |= 1u64 << (b & 63);
            b += 1;
        }
        self
    }

    /// Bitwise union of two sets.
    pub const fn union(mut self, other: Self) -> Self {
        let mut i = 0;
        while i < W {
            self.words[i] |= other.words[i];
            i += 1;
        }
        self
    }

    /// O(1) membership test (returns `false` for indices `>= W * 64`).
    #[inline(always)]
    pub const fn contains(&self, bit: usize) -> bool {
        let word = bit >> 6;
        if word >= W {
            return false;
        }
        (self.words[word] & (1u64 << (bit & 63))) != 0
    }

    /// Number of bits set.
    pub const fn count(&self) -> u32 {
        let mut total = 0u32;
        let mut i = 0;
        while i < W {
            total += self.words[i].count_ones();
            i += 1;
        }
        total
    }
}

// ---------------------------------------------------------------------------------------------
// Safeguarded Interpolation Search: O(log log N) on uniform hash/BLAKE3 columns, O(log N) worst.
// ---------------------------------------------------------------------------------------------

/// Safeguarded interpolation search over a sorted slice whose keys have a roughly uniform `u64`
/// big-endian prefix (such as BLAKE3 `ContentId` or `Digest` columns).
///
/// - Average complexity on uniform hashes: **`O(log log N)`** (~1–2 cache misses on a 64 KiB page).
/// - Worst-case complexity on adversarial/skewed keys: strictly **`O(log N)`** (falls back to
///   binary search whenever `span <= 8`, prefixes collide, or 3 consecutive probes fail to halve).
/// - Returns `Ok(idx)` when found or `Err(insert_pos)` matching `slice::binary_search_by`.
pub fn interp_search_by_prefix<T>(
    slice: &[T],
    target_prefix: u64,
    mut prefix_of: impl FnMut(&T) -> u64,
    mut cmp: impl FnMut(&T) -> Ordering,
) -> Result<usize, usize> {
    let mut lo = 0usize;
    let mut hi = slice.len();
    let mut stale = 0u8;

    while lo < hi {
        let span = hi - lo;
        let mid = if span <= 8 || stale >= 3 {
            lo + span / 2
        } else {
            let lo_p = prefix_of(&slice[lo]);
            let hi_p = prefix_of(&slice[hi - 1]);
            if target_prefix < lo_p {
                return Err(lo);
            }
            if target_prefix > hi_p {
                return Err(hi);
            }
            if hi_p == lo_p {
                lo + span / 2
            } else {
                let num = u128::from(target_prefix - lo_p) * u128::from((span - 1) as u64);
                let den = u128::from(hi_p - lo_p);
                let guess = lo + (num / den) as usize;
                guess.clamp(lo, hi - 1)
            }
        };

        match cmp(&slice[mid]) {
            Ordering::Equal => return Ok(mid),
            Ordering::Less => {
                let next_lo = mid + 1;
                if hi - next_lo > span / 2 {
                    stale = stale.saturating_add(1);
                } else {
                    stale = 0;
                }
                lo = next_lo;
            }
            Ordering::Greater => {
                if mid - lo > span / 2 {
                    stale = stale.saturating_add(1);
                } else {
                    stale = 0;
                }
                hi = mid;
            }
        }
    }
    Err(lo)
}

// ---- Portable zero-unsafe SIMD primitives (NEON on Apple Silicon / Graviton, AVX2/AVX-512 on EPYC)

use core::simd::{Select as _, Simd, cmp::SimdPartialEq as _};

/// Finds the first index in `haystack` equal to any of the four delimiter bytes `(b0, b1, b2, b3)`
/// using 32-byte portable SIMD vectors (`core::simd::Simd<u8, 32>`). Zero `unsafe`.
#[inline]
pub fn simd_find_byte_any4(haystack: &[u8], b0: u8, b1: u8, b2: u8, b3: u8) -> Option<usize> {
    const LANES: usize = 32;
    let v0 = Simd::<u8, LANES>::splat(b0);
    let v1 = Simd::<u8, LANES>::splat(b1);
    let v2 = Simd::<u8, LANES>::splat(b2);
    let v3 = Simd::<u8, LANES>::splat(b3);

    let chunks = haystack.chunks_exact(LANES);
    let rem_start = haystack.len() - chunks.remainder().len();
    for (idx, chunk) in chunks.enumerate() {
        let v = Simd::<u8, LANES>::from_slice(chunk);
        let mask = v.simd_eq(v0) | v.simd_eq(v1) | v.simd_eq(v2) | v.simd_eq(v3);
        let bits = mask.to_bitmask();
        if bits != 0 {
            return Some(idx * LANES + bits.trailing_zeros() as usize);
        }
    }
    for (off, &byte) in haystack[rem_start..].iter().enumerate() {
        if byte == b0 || byte == b1 || byte == b2 || byte == b3 {
            return Some(rem_start + off);
        }
    }
    None
}

/// Counts occurrences of `needle` in `haystack` using 32-byte SIMD with 255-block vector
/// accumulation (amortizing horizontal reductions to once per 8,160 bytes). Zero `unsafe`.
#[inline]
pub fn simd_count_byte(haystack: &[u8], needle: u8) -> usize {
    const LANES: usize = 32;
    let target = Simd::<u8, LANES>::splat(needle);
    let ones = Simd::<u8, LANES>::splat(1);
    let zeros = Simd::<u8, LANES>::splat(0);

    let mut total = 0usize;
    let mut chunks = haystack.chunks_exact(LANES);
    loop {
        let mut acc = zeros;
        let mut block = 0usize;
        while block < 255 {
            let Some(chunk) = chunks.next() else {
                break;
            };
            let v = Simd::<u8, LANES>::from_slice(chunk);
            acc += v.simd_eq(target).select(ones, zeros);
            block += 1;
        }
        if block == 0 {
            break;
        }
        // Horizontal sum of 32 lanes (each lane <= 255, total <= 8160).
        let arr = acc.to_array();
        let mut s = 0usize;
        for &x in &arr {
            s += usize::from(x);
        }
        total += s;
        if block < 255 {
            break;
        }
    }
    for &b in chunks.remainder() {
        total += usize::from(b == needle);
    }
    total
}

/// Returns the index of the first differing byte between `a` and `b` (or `a.len().min(b.len())`
/// if one is a prefix of the other) using 32-byte SIMD vectors. Zero `unsafe`.
#[inline]
pub fn simd_mismatch(a: &[u8], b: &[u8]) -> usize {
    const LANES: usize = 32;
    let len = a.len().min(b.len());
    let mut i = 0usize;
    while i + LANES <= len {
        let va = Simd::<u8, LANES>::from_slice(&a[i..i + LANES]);
        let vb = Simd::<u8, LANES>::from_slice(&b[i..i + LANES]);
        let diff = va.simd_ne(vb).to_bitmask();
        if diff != 0 {
            return i + diff.trailing_zeros() as usize;
        }
        i += LANES;
    }
    while i < len {
        if a[i] != b[i] {
            return i;
        }
        i += 1;
    }
    len
}

/// In-place inclusive wrapping prefix sum over `values` using 8-wide SIMD Hillis-Steele
/// in-register scans (for building CSR offset tables at 8 `u32`s per vector step). Zero `unsafe`.
#[inline]
pub fn simd_prefix_sum_u32(values: &mut [u32]) {
    const LANES: usize = 8;
    let mut carry = 0u32;
    let mut chunks = values.chunks_exact_mut(LANES);
    for chunk in &mut chunks {
        let mut v = Simd::<u32, LANES>::from_slice(chunk);
        // Step 1: shift by 1
        let s1 = Simd::<u32, LANES>::from_array([0, v[0], v[1], v[2], v[3], v[4], v[5], v[6]]);
        v += s1;
        // Step 2: shift by 2
        let s2 = Simd::<u32, LANES>::from_array([0, 0, v[0], v[1], v[2], v[3], v[4], v[5]]);
        v += s2;
        // Step 3: shift by 4
        let s4 = Simd::<u32, LANES>::from_array([0, 0, 0, 0, v[0], v[1], v[2], v[3]]);
        v += s4;
        v += Simd::<u32, LANES>::splat(carry);
        carry = v[LANES - 1];
        chunk.copy_from_slice(&v.to_array());
    }
    for slot in chunks.into_remainder() {
        carry = carry.wrapping_add(*slot);
        *slot = carry;
    }
}
