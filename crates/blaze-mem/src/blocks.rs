//! Blocks of power-of-two sizes, for tables made and dropped whole that grow by a new table
//! twice the last (a solver cache's chunks, shards and probe tables): `Box<[T]>`'s use,
//! without the global allocator.
//!
//! A size class (64 B, 128 B, and so on up to [`LARGEST`]) reserves its own region the first
//! time a block of it is taken ([`CLASS_BYTES`] of address space, nothing committed) and carves
//! it in order; a block given back goes on its class's lock-free stack and is the next one
//! taken there, already backed by pages. Memory is never given back to the kernel, as with
//! [`crate::Pool`]: a class holds as many blocks as were live at once at most.
//!
//! A block is aligned to its size up to a page: a class's region is page-aligned and its
//! blocks are multiples of the size apart. A newly carved block is zero, as the kernel makes
//! fresh pages; a recycled one holds what its last owner left, and is cleared only for a caller
//! that asks ([`Fill::Zeroed`]).
//!
//! Cost: a take or give is one compare-and-swap on the class's stack; carving is one
//! `fetch_add`, plus a commit system call every quarter of what the class has committed.

use std::{fmt, marker::PhantomData, mem, ops::Deref, ptr::NonNull, sync::OnceLock};

use crate::{
    MemError, Region, Tag,
    stack::{LinkTable, Links, Stack},
    sync::atomic::{AtomicU32, Ordering},
    tag,
};

/// The address space a size class reserves: 16 GB, free on a 64-bit machine even for every
/// class at once (about 470 GB of the 128 TB user space).
#[cfg(target_pointer_width = "64")]
pub const CLASS_BYTES: usize = 16 << 30;

/// On a 32-bit target (wasm32) a reservation is memory taken whole, so a class reserves 8 MB,
/// and only the classes used are made.
#[cfg(not(target_pointer_width = "64"))]
pub const CLASS_BYTES: usize = 8 << 20;

/// The smallest block: a cache line.
pub const SMALLEST: usize = 64;

/// The largest block: a class of one.
pub const LARGEST: usize = CLASS_BYTES;

/// The largest alignment a block has for certain: the smallest page of the machines blaze
/// targets.
pub const MAX_ALIGN: usize = 4096;

const MIN_SHIFT: u32 = SMALLEST.trailing_zeros();
const MAX_SHIFT: u32 = LARGEST.trailing_zeros();
const CLASSES: usize = (MAX_SHIFT - MIN_SHIFT + 1) as usize;

const _: () = assert!(SMALLEST.is_power_of_two() && LARGEST.is_power_of_two());
const _: () = assert!((CLASS_BYTES >> MIN_SHIFT) < crate::stack::NONE as usize);

/// What a taken block holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fill {
    /// Whatever it holds: a caller that writes every byte it reads.
    Any,
    /// Zeros.
    Zeroed,
}

struct Class {
    shift: u32,
    region: Region,
    links: LinkTable,
    max: u32,
    carved: AtomicU32,
    /// Blocks given back: committed, holding what their owners left.
    free: Stack,
}

static CLASS: [OnceLock<Result<Class, String>>; CLASSES] = [const { OnceLock::new() }; CLASSES];

impl Class {
    /// A class of `1 << shift`-byte blocks over `space` bytes of address space.
    fn new(shift: u32, space: usize) -> Result<Class, MemError> {
        let max = (space >> shift).max(1) as u32;
        Ok(Class {
            shift,
            region: Region::reserve((max as usize) << shift, Tag::Chunks)?,
            links: LinkTable::new(max, Tag::Chunks)?,
            max,
            carved: AtomicU32::new(0),
            free: Stack::new(),
        })
    }

    fn size(&self) -> usize {
        1 << self.shift
    }

    fn take(&'static self, fill: Fill, tag: Tag) -> Result<Owned, MemError> {
        let at = match self.free.pop(&self.links) {
            Some(index) => {
                let at = self.region.at((index as usize) << self.shift);
                if fill == Fill::Zeroed {
                    // SAFETY: a block on the free stack was carved, so it is committed, and
                    // the pop just now made it this caller's alone: no `Owned` names it.
                    unsafe { at.write_bytes(0, self.size()) };
                }
                at
            }
            None => self.carve()?,
        };
        tag::add(tag, self.size());
        tag::allocated(tag, self.size());
        Ok(Owned { at, class: self, tag })
    }

    /// A block never handed out, committed, so zero.
    fn carve(&self) -> Result<NonNull<u8>, MemError> {
        let index = self.carved.fetch_add(1, Ordering::Relaxed);
        if index >= self.max {
            self.carved.fetch_min(self.max, Ordering::Relaxed);
            return Err(MemError::Exhausted {
                tag: Tag::Chunks,
                reserved: self.region.reserved(),
                wanted: (index as usize + 1) << self.shift,
            });
        }
        let end = (index as usize + 1) << self.shift;
        self.region.ensure(end)?;
        // SAFETY: `index` came from the `fetch_add` just now: no other thread has it.
        unsafe { self.links.init(index) }?;
        Ok(self.region.at((index as usize) << self.shift))
    }

    fn give(&self, at: NonNull<u8>) {
        let offset = at.as_ptr() as usize - self.region.base().as_ptr() as usize;
        self.free.push((offset >> self.shift) as u32, &self.links);
    }

    #[cfg(test)]
    fn carved(&self) -> u32 {
        self.carved.load(Ordering::Relaxed).min(self.max)
    }
}

/// The class of the smallest block that holds `bytes` aligned to `align`.
fn shift_of(bytes: usize, align: usize, tag: Tag) -> Result<u32, MemError> {
    if !align.is_power_of_two() || align > MAX_ALIGN {
        return Err(MemError::TooLarge { tag, what: "a block's alignment", wanted: align });
    }
    let size = bytes.max(align).max(SMALLEST);
    if size > LARGEST {
        return Err(MemError::TooLarge { tag, what: "a block", wanted: bytes });
    }
    Ok(size.next_power_of_two().trailing_zeros())
}

fn class(shift: u32) -> Result<&'static Class, MemError> {
    let class = CLASS[(shift - MIN_SHIFT) as usize].get_or_init(|| {
        // alloc: once, when a class cannot be reserved
        Class::new(shift, CLASS_BYTES).map_err(|error| error.to_string())
    });
    match class {
        Ok(class) => Ok(class),
        Err(message) => Err(MemError::Reserve {
            tag: Tag::Chunks,
            bytes: CLASS_BYTES,
            os: std::io::Error::other(message.as_str()),
        }),
    }
}

/// A block of at least `bytes`, aligned to `align` (a power of two up to [`MAX_ALIGN`]),
/// counted to `tag` while it lives.
pub fn take(bytes: usize, align: usize, fill: Fill, tag: Tag) -> Result<Owned, MemError> {
    class(shift_of(bytes, align, tag)?)?.take(fill, tag)
}

/// A block of its own, given back to its class when dropped.
pub struct Owned {
    at: NonNull<u8>,
    class: &'static Class,
    tag: Tag,
}

// SAFETY: an `Owned` is the one name of its bytes (a block is on its class's free stack or in
// exactly one `Owned`, never both), and its class is shared through atomics and a lock-free
// stack, so it may move to and be read from any thread.
unsafe impl Send for Owned {}
// SAFETY: as for `Send`: `&Owned` hands out only the address and the length.
unsafe impl Sync for Owned {}

impl Owned {
    /// The first byte, aligned to the alignment it was taken with.
    pub fn as_ptr(&self) -> NonNull<u8> {
        self.at
    }

    /// Its bytes: the size of its class, at least what was asked.
    pub fn len(&self) -> usize {
        self.class.size()
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn tag(&self) -> Tag {
        self.tag
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        tag::sub(self.tag, self.class.size());
        self.class.give(self.at);
    }
}

impl fmt::Debug for Owned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Owned").field("at", &self.at).field("len", &self.len()).finish()
    }
}

/// `Box<[T]>` in a block: `len` values made once, dropped with it, its block given back.
pub struct Slice<T> {
    block: Owned,
    len: usize,
    _owns: PhantomData<T>,
}

// SAFETY: a `Slice` owns its values as a `Box<[T]>` does: moving it moves them.
unsafe impl<T: Send> Send for Slice<T> {}
// SAFETY: `&Slice` hands out `&[T]` only.
unsafe impl<T: Sync> Sync for Slice<T> {}

impl<T> Slice<T> {
    /// `len` values, the one at `at` made by `make(at)`, counted to `tag`.
    pub fn new_with(len: usize, tag: Tag, mut make: impl FnMut(usize) -> T) -> Result<Slice<T>, MemError> {
        let bytes = len
            .checked_mul(mem::size_of::<T>())
            .ok_or(MemError::TooLarge { tag, what: "a block's values", wanted: len })?;
        let block = take(bytes, mem::align_of::<T>(), Fill::Any, tag)?;
        let first = block.as_ptr().cast::<T>();
        let mut slice = Slice { block, len: 0, _owns: PhantomData };
        for at in 0..len {
            let value = make(at);
            // SAFETY: the block holds `len` values of `T` aligned (taken with `T`'s size times
            // `len` and its alignment), `at < len`, and the place is not yet a value: `len`
            // counts the values made so far, so a panic in `make` drops those alone.
            unsafe { first.as_ptr().wrapping_add(at).write(value) };
            slice.len = at + 1;
        }
        Ok(slice)
    }

    fn first(&self) -> NonNull<T> {
        self.block.as_ptr().cast::<T>()
    }
}

impl<T> Deref for Slice<T> {
    type Target = [T];

    #[inline]
    fn deref(&self) -> &[T] {
        // SAFETY: the first `len` places of the block are values (`new_with`), aligned, and
        // live until the slice drops; `&self` lends them shared.
        unsafe { std::slice::from_raw_parts(self.first().as_ptr(), self.len) }
    }
}

impl<T> Drop for Slice<T> {
    fn drop(&mut self) {
        // SAFETY: the first `len` places are values no one else drops; the block, given back
        // after this, is not read again by this slice.
        unsafe { std::ptr::slice_from_raw_parts_mut(self.first().as_ptr(), self.len).drop_in_place() };
    }
}

impl<T: fmt::Debug> fmt::Debug for Slice<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

#[cfg(all(test, not(blaze_check)))]
mod tests;
