//! The macro's heap in the compiled tier: what `__rust_alloc`, `__rust_dealloc`,
//! `__rust_realloc`, `__rust_alloc_zeroed` and C's `malloc` family give it, as offsets in the
//! region's heap ([`super::region::Layout::heap`]).
//!
//! Its bookkeeping is host memory, never the region's: nothing the macro writes changes what
//! the heap hands out, so a macro that corrupts its own memory still gets blocks inside its
//! heap and nowhere else. Small blocks are powers of two from 16 bytes to 32 KiB, each class
//! carved from 64 KiB chunks and kept on its own free list (last freed, first reused: the
//! block still in cache); larger ones are whole granules, reused by their count. A call is a
//! shift, a pop and a compare. The same calls give the same addresses in every region and
//! run, so an expansion that hashes addresses is as reproducible as one that doesn't.

use blaze_hash::HashMap;

use super::region::{GRANULE, Region};
use crate::guest::Stop;

const MIN_SHIFT: u32 = 4;
const MAX_SHIFT: u32 = 15;
const CLASSES: usize = (MAX_SHIFT - MIN_SHIFT + 1) as usize;
/// The largest small block.
const MAX_SMALL: u64 = 1 << MAX_SHIFT;

const _: () = assert!(MAX_SMALL <= GRANULE as u64, "a chunk holds whole blocks of every class");

pub(crate) struct Heap {
    /// The heap's first and past-last guest addresses: its bound.
    start: u32,
    end: u32,
    /// The first byte no block or chunk has taken: a multiple of [`GRANULE`].
    top: u32,
    /// Per class: freed blocks, last freed last.
    free: [Vec<u32>; CLASSES],
    /// Per class: what is left of the chunk it carves from, `(next, end)`.
    carve: [(u32, u32); CLASSES],
    /// Freed large blocks by their granules.
    large: HashMap<u32, Vec<u32>>,
    /// Size and alignment of each block `malloc` and its family gave, which C's `free` and
    /// `realloc` don't say.
    c_blocks: HashMap<u32, (u64, u64)>,
}

/// Where a block of `size` bytes aligned to `align` comes from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    Small(usize),
    /// Whole granules.
    Large(u32),
}

impl Class {
    fn of(size: u64, align: u64) -> Result<Class, Stop> {
        if !align.is_power_of_two() {
            // alloc: error path
            return Err(Stop(format!("asked for memory aligned to {align}, not a power of two")));
        }
        let need = size.max(align).max(1 << MIN_SHIFT);
        if need <= MAX_SMALL {
            let shift = need.next_power_of_two().trailing_zeros();
            return Ok(Class::Small((shift - MIN_SHIFT) as usize));
        }
        let granules = need.div_ceil(u64::from(GRANULE));
        match u32::try_from(granules) {
            Ok(granules) => Ok(Class::Large(granules)),
            // alloc: error path
            Err(_) => Err(Stop(format!("asked for {size} bytes of memory at once"))),
        }
    }

    fn bytes(self) -> u64 {
        match self {
            Class::Small(class) => 1 << (class as u32 + MIN_SHIFT),
            Class::Large(granules) => u64::from(granules) * u64::from(GRANULE),
        }
    }
}

impl Heap {
    /// A heap over `heap`, a granule-aligned range of guest addresses.
    pub(crate) fn new(heap: std::ops::Range<u32>) -> Heap {
        debug_assert!(heap.start.is_multiple_of(GRANULE));
        Heap {
            start: heap.start,
            end: heap.end,
            top: heap.start,
            free: Default::default(),
            carve: [(0, 0); CLASSES],
            large: HashMap::default(),
            c_blocks: HashMap::default(),
        }
    }

    /// Bytes from the heap's start that any block has reached: what the next expansion is
    /// likely to need mapped.
    pub(crate) fn reached(&self) -> u32 {
        self.top - self.start
    }

    /// Forgets every block, for the next expansion. Keeps the host memory of its lists.
    pub(crate) fn reset(&mut self) {
        self.top = self.start;
        self.free.iter_mut().for_each(Vec::clear);
        self.carve = [(0, 0); CLASSES];
        self.large.values_mut().for_each(Vec::clear);
        self.c_blocks.clear();
    }

    /// `__rust_alloc`: `size` bytes aligned to `align`. Its bytes are what they were.
    pub(crate) fn alloc(&mut self, region: &mut Region, size: u64, align: u64) -> Result<u32, Stop> {
        match Class::of(size, align)? {
            Class::Small(class) => {
                if let Some(at) = self.free[class].pop() {
                    return Ok(at);
                }
                let block = Class::Small(class).bytes() as u32;
                let (next, end) = self.carve[class];
                if next < end {
                    self.carve[class].0 = next + block;
                    return Ok(next);
                }
                let chunk = self.take(region, GRANULE, 1)?;
                self.carve[class] = (chunk + block, chunk + GRANULE);
                Ok(chunk)
            }
            Class::Large(granules) => {
                let aligned = |at: &u32| u64::from(*at).is_multiple_of(align);
                if let Some(free) = self.large.get_mut(&granules)
                    && let Some(i) = free.iter().rposition(aligned)
                {
                    return Ok(free.swap_remove(i));
                }
                let (Some(len), Ok(align)) =
                    (granules.checked_mul(GRANULE), u32::try_from(align.max(u64::from(GRANULE))))
                else {
                    // alloc: error path
                    return Err(Stop(format!("asked for {size} bytes of memory aligned to {align}")));
                };
                self.take(region, len, align)
            }
        }
    }

    /// `__rust_alloc_zeroed`.
    pub(crate) fn alloc_zeroed(&mut self, region: &mut Region, size: u64, align: u64) -> Result<u32, Stop> {
        let at = self.alloc(region, size, align)?;
        region.fill(u64::from(at), size as usize, 0)?;
        Ok(at)
    }

    /// `__rust_dealloc` of a block [`Heap::alloc`] gave for `size` and `align`.
    pub(crate) fn dealloc(&mut self, at: u64, size: u64, align: u64) -> Result<(), Stop> {
        let class = Class::of(size, align)?;
        let at = self.block(at, class)?;
        match class {
            Class::Small(class) => self.free[class].push(at),
            Class::Large(granules) => self.large.entry(granules).or_default().push(at),
        }
        Ok(())
    }

    /// `__rust_realloc`: the block at `at`, of `old` bytes aligned to `align`, as one of `new`
    /// bytes holding its first `min(old, new)` bytes.
    pub(crate) fn realloc(
        &mut self,
        region: &mut Region,
        at: u64,
        old: u64,
        align: u64,
        new: u64,
    ) -> Result<u32, Stop> {
        let class = Class::of(old, align)?;
        let at = self.block(at, class)?;
        if Class::of(new, align)? == class {
            return Ok(at);
        }
        let moved = self.alloc(region, new, align)?;
        region.copy_within(u64::from(at), u64::from(moved), old.min(new) as usize)?;
        self.dealloc(u64::from(at), old, align)?;
        Ok(moved)
    }

    /// C's `malloc`, `calloc` (`zeroed`), `aligned_alloc` and `posix_memalign`: a block
    /// whose size the heap keeps for `free`.
    pub(crate) fn c_alloc(&mut self, region: &mut Region, size: u64, align: u64, zeroed: bool) -> Result<u32, Stop> {
        let at = if zeroed { self.alloc_zeroed(region, size, align)? } else { self.alloc(region, size, align)? };
        self.c_blocks.insert(at, (size, align));
        Ok(at)
    }

    /// C's `free`; null is nothing.
    pub(crate) fn c_free(&mut self, at: u64) -> Result<(), Stop> {
        if at == 0 {
            return Ok(());
        }
        let (size, align) = self.c_block(at)?;
        self.c_blocks.remove(&(at as u32));
        self.dealloc(at, size, align)
    }

    /// C's `realloc`; null is `malloc`.
    pub(crate) fn c_realloc(&mut self, region: &mut Region, at: u64, new: u64) -> Result<u32, Stop> {
        if at == 0 {
            return self.c_alloc(region, new, 16, false);
        }
        let (size, align) = self.c_block(at)?;
        self.c_blocks.remove(&(at as u32));
        let moved = self.realloc(region, at, size, align, new)?;
        self.c_blocks.insert(moved, (new, align));
        Ok(moved)
    }

    fn c_block(&self, at: u64) -> Result<(u64, u64), Stop> {
        let block = u32::try_from(at).ok().and_then(|at| self.c_blocks.get(&at));
        // alloc: error path
        block.copied().ok_or_else(|| Stop(format!("freed {at:#x}, which malloc did not give it")))
    }

    /// `at` as a block of `class`: inside what the heap has handed out and aligned as its
    /// class's blocks are. Anything else was never given, and is refused rather than reused.
    fn block(&self, at: u64, class: Class) -> Result<u32, Stop> {
        let inside = u64::from(self.start) <= at && at < u64::from(self.top);
        let aligned = at.is_multiple_of(class.bytes().min(u64::from(GRANULE)));
        if inside && aligned {
            return Ok(at as u32);
        }
        // alloc: error path
        Err(Stop(format!("freed {at:#x}, which the allocator did not give it")))
    }

    /// `len` fresh bytes at the top, aligned to `align` (a multiple of [`GRANULE`] or 1),
    /// mapped.
    fn take(&mut self, region: &mut Region, len: u32, align: u32) -> Result<u32, Stop> {
        let at = u64::from(self.top).next_multiple_of(u64::from(align.max(1)));
        let end = at + u64::from(len);
        if end > u64::from(self.end) {
            // alloc: error path
            return Err(Stop(format!("used more than its {} bytes of heap", self.end - self.start)));
        }
        region.grow_heap((end - u64::from(self.start)) as u32)?;
        self.top = end as u32;
        Ok(at as u32)
    }
}

#[cfg(test)]
mod tests;
