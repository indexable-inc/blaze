//! Size classes: 8, then every 16 bytes to 128, then eight to each doubling (12.5% apart) to
//! 64 KiB, 81 in all. A request's waste is under 12.5% past 128 bytes; below it a class is at
//! most 15 bytes over. Every class past 8 is a multiple of 16, and each doubling ends on its power
//! of two, so an alignment up to the class's size is met by stepping to a class that divides by
//! it (at most to the next power of two).
//!
//! The lookup is a table for sizes to 1 KiB (one load) and a leading-zeros count past it: the
//! table is the fast path's, the arithmetic the rest's. A class of at most [`SMALL_MAX`] bytes is
//! served from small spans (64 KiB); a larger one from medium spans (512 KiB), so each span holds
//! at least eight blocks.

/// The classes in all.
pub(crate) const CLASSES: usize = 81;

/// The largest class: past it a request is large, its own mapping.
pub(crate) const MAX: usize = 64 << 10;

/// The largest class served from small spans.
pub(crate) const SMALL_MAX: usize = 8 << 10;

/// The block size of class `class`.
pub(crate) const fn size(class: usize) -> usize {
    if class == 0 {
        8
    } else if class <= 8 {
        class * 16
    } else {
        let k = (class - 9) / 8 + 7;
        let j = (class - 9) % 8 + 1;
        (1 << k) + j * (1 << (k - 3))
    }
}

/// Every class's block size, for the hot paths that read it.
pub(crate) static SIZES: [u32; CLASSES] = {
    let mut sizes = [0u32; CLASSES];
    let mut class = 0;
    while class < CLASSES {
        sizes[class] = size(class) as u32;
        class += 1;
    }
    sizes
};

/// The class of sizes `8 * i - 7 ..= 8 * i` for `i` to 128 (sizes to 1 KiB).
static BY_EIGHTS: [u8; 129] = {
    let mut table = [0u8; 129];
    let mut i = 0;
    while i < 129 {
        table[i] = by_arithmetic(if i == 0 { 1 } else { i * 8 }) as u8;
        i += 1;
    }
    table
};

/// The least class at least `size` bytes, by arithmetic.
const fn by_arithmetic(size: usize) -> usize {
    if size <= 8 {
        0
    } else if size <= 128 {
        size.div_ceil(16)
    } else {
        let k = (usize::BITS - 1 - (size - 1).leading_zeros()) as usize;
        let step = 1 << (k - 3);
        let j = (size - (1 << k)).div_ceil(step);
        9 + (k - 7) * 8 + (j - 1)
    }
}

/// The least class at least `size` bytes; `size` at most [`MAX`].
#[inline(always)]
pub(crate) fn of(size: usize) -> usize {
    debug_assert!(size <= MAX);
    if size <= 1024 { BY_EIGHTS[size.div_ceil(8)] as usize } else { by_arithmetic(size) }
}

/// The least class at least `size` bytes whose blocks are `align`-aligned (a power of two), or
/// `None` when that passes [`MAX`]. A span starts at a multiple of its own size, so a block is
/// aligned as its offset `i * size` is: to the largest power of two dividing the class size.
#[inline]
pub(crate) fn of_aligned(size: usize, align: usize) -> Option<usize> {
    if align <= 8 {
        return (size <= MAX).then(|| of(size));
    }
    let size = size.max(align);
    if size > MAX {
        return None;
    }
    let mut class = of(size);
    // Ends: each doubling's last class is its power of two, a multiple of every smaller one.
    while SIZES[class] as usize % align != 0 {
        class += 1;
    }
    Some(class)
}

/// Whether class `class` is served from small spans.
#[inline(always)]
pub(crate) fn is_small(class: usize) -> bool {
    SIZES[class] as usize <= SMALL_MAX
}
