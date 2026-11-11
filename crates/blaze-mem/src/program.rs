//! The program arena and [`Ref32`] (docs/v2/plan/design/facts.md "Ref32"): memory that lives
//! until the process exits, in one reserved range, named by 32-bit references.
//!
//! A [`Ref32<T>`] is four bytes: the offset of a `T` from the range's base, in units of
//! [`UNIT`] bytes, never zero, so `Option<Ref32<T>>` is four bytes too. It is `Copy` and
//! derefs to `&'static T`. Half the size of a `&'static T` or a `Box<T>`, a third of an
//! `Arc<T>`'s pointer and its two counts: what a syntax tree's child links and an interner's
//! entries are held as.
//!
//! The scale. [`UNIT`] is 8 bytes: a reference reaches `2^32 * 8` bytes, 32 GiB, which is
//! [`MAX_RESERVED`]. Every value starts on a multiple of 8 (of its own alignment, if that is
//! more), so a value of alignment 1 to 8 wastes at most 7 bytes before it, which the types
//! this is for (nodes with pointer-sized fields) never do. A unit of 4 would reach 16 GiB and
//! a unit of 16 would waste a word before most nodes; 8 is the alignment of the values held.
//!
//! The base. [`ProgramArena::establish`] reserves the range once for the process and
//! publishes its base address; a second call is [`ProgramError::Established`], whoever makes
//! it. Allocation needs the handle `establish` returned: there is no global to allocate
//! from, and the handle is neither `Clone` nor reachable from a static here. Reading needs a
//! `Ref32`, which only the handle's [`ProgramArena::alloc`] makes. The published base is not
//! ambient authority: it is the address of memory, as a `static`'s is, and gives nothing to
//! code that holds neither the handle nor a reference; no function here takes a number and
//! returns a reference.
//!
//! Nothing is freed and nothing moves ([`crate::Region`]): the handle has no `Drop`, the
//! range is never unmapped, and a value is written once before its reference exists. So
//! every `Ref32` is valid from when it is made until the process exits, which is what
//! `'static` says.

use std::{
    fmt,
    marker::PhantomData,
    mem::{ManuallyDrop, align_of, size_of},
    num::NonZeroU32,
    ops::Deref,
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::{MemError, Region, Tag};

/// What one step of a [`Ref32`]'s offset is, in bytes.
pub const UNIT: usize = 8;

/// The most a program arena reserves: what a 32-bit offset in [`UNIT`]s reaches.
pub const MAX_RESERVED: usize = (u32::MAX as usize + 1) * UNIT;

/// The base address of the process's program arena; 0 until one is established.
static BASE: AtomicUsize = AtomicUsize::new(0);

/// A type whose instances live until the program exits: no `Drop` anywhere inside, every
/// field plain data, a `&'static` reference or program-lifetime itself, and instances built
/// only into the program arena.
///
/// # Safety
///
/// The type is declared `#[blaze::program_lifetime]`, whose check (`blaze::program_lifetime`)
/// holds exactly that; an impl for a type without the attribute is refused by the same lint.
/// Once reflection's phase 2 is on main the shape half is a blanket impl over
/// `core::mem::type_info` and this trait is no longer implemented by hand.
pub unsafe trait ProgramLifetime: Sized + 'static {}

macro_rules! plain {
    ($($ty:ty),*) => { $(
        // SAFETY: plain data: nothing to drop, nothing borrowed.
        unsafe impl ProgramLifetime for $ty {}
    )* };
}

plain!(u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize, f32, f64, bool, char, ());

// SAFETY: a reference into the arena is four bytes of plain data.
unsafe impl<T: ProgramLifetime> ProgramLifetime for Ref32<T> {}
// SAFETY: as its payload; `None` is plain.
unsafe impl<T: ProgramLifetime> ProgramLifetime for Option<T> {}
// SAFETY: as its elements.
unsafe impl<T: ProgramLifetime, const N: usize> ProgramLifetime for [T; N] {}
// SAFETY: what it points to is not part of the value and lives as long as the program.
unsafe impl<T: ?Sized + 'static> ProgramLifetime for &'static T {}

/// Why the program arena could not be established.
#[derive(Debug)]
pub enum ProgramError {
    /// The process has one already: the range is reserved once, and a second would give
    /// references two bases.
    Established,
    /// More than a 32-bit offset reaches, or the system refused the reservation.
    Mem(MemError),
}

impl fmt::Display for ProgramError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProgramError::Established => f.write_str(
                "the program arena is established already: `ProgramArena::establish` is called \
                 once, and its handle passed to what allocates",
            ),
            ProgramError::Mem(error) => write!(f, "the program arena: {error}"),
        }
    }
}

impl std::error::Error for ProgramError {}

/// The handle that allocates in the process's program arena. One exists; it is not `Clone`.
/// `alloc` takes `&mut self`, so there is one writer and no concurrent bump: code that
/// allocates from several threads holds the handle in a lock it names.
pub struct ProgramArena {
    /// Never dropped: the range outlives every reference into it.
    region: ManuallyDrop<Region>,
    /// The first free byte's offset: a multiple of [`UNIT`], at least [`UNIT`] (offset 0 is
    /// the niche).
    next: usize,
}

impl ProgramArena {
    /// Reserves `bytes` of address space (at most [`MAX_RESERVED`]) as the process's program
    /// arena, counted to `tag`, and publishes its base. Pages are committed as it fills.
    pub fn establish(bytes: usize, tag: Tag) -> Result<ProgramArena, ProgramError> {
        if bytes > MAX_RESERVED || bytes < 2 * UNIT {
            return Err(ProgramError::Mem(MemError::TooLarge {
                tag,
                what: "a program arena, which a 32-bit offset of 8-byte units must reach",
                wanted: bytes,
            }));
        }
        let region = Region::reserve(bytes, tag).map_err(ProgramError::Mem)?;
        let base = region.base().as_ptr() as usize;
        // The one write of the base. A thread that loses gives its range back (`region`
        // drops here) and is told; no reference into the loser's range was ever made.
        if BASE.compare_exchange(0, base, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return Err(ProgramError::Established);
        }
        Ok(ProgramArena { region: ManuallyDrop::new(region), next: UNIT })
    }

    /// The bytes reserved.
    pub fn reserved(&self) -> usize {
        self.region.reserved()
    }

    /// The bytes used, the unused first unit included.
    pub fn used(&self) -> usize {
        self.next
    }

    /// Moves `value` into the arena, for good. [`MemError::Exhausted`] when the reservation
    /// is used up: nothing is written, and the arena stays as it was.
    pub fn alloc<T: ProgramLifetime>(&mut self, value: T) -> Result<Ref32<T>, MemError> {
        let align = align_of::<T>().max(UNIT);
        let exhausted = |wanted| MemError::Exhausted {
            tag: self.region.tag(),
            reserved: self.region.reserved(),
            wanted,
        };
        let start = self.next.checked_next_multiple_of(align).ok_or_else(|| exhausted(usize::MAX))?;
        let end = start.checked_add(size_of::<T>()).ok_or_else(|| exhausted(usize::MAX))?;
        // A value of no size at the very end would be named by an offset one past the range.
        if end > self.region.reserved() || start >= self.region.reserved() {
            return Err(exhausted(end));
        }
        self.region.ensure(end)?;
        // SAFETY: `[start, end)` is committed (`ensure`), inside the reservation, aligned for
        // `T` (`start` is a multiple of its alignment and the base is page-aligned), and
        // nothing else names it: `next` only grows, and `&mut self` is the one writer.
        unsafe { self.region.at(start).as_ptr().cast::<T>().write(value) };
        self.next = end.next_multiple_of(UNIT);
        // `start / UNIT` fits: `start < reserved <= MAX_RESERVED`; it is not zero: `start >=
        // UNIT`.
        let units = NonZeroU32::new((start / UNIT) as u32).expect("an offset past the first unit");
        Ok(Ref32 { units, _of: PhantomData })
    }
}

/// A reference to a `T` in the program arena: four bytes, `Copy`, valid until the process
/// exits. `Option<Ref32<T>>` is four bytes.
#[repr(transparent)]
pub struct Ref32<T: ProgramLifetime> {
    units: NonZeroU32,
    _of: PhantomData<&'static T>,
}

impl<T: ProgramLifetime> Ref32<T> {
    /// The value, for as long as the program runs.
    #[inline]
    pub fn get(self) -> &'static T {
        // A reference exists only once `establish` published the base and `alloc` wrote the
        // value, on the thread that made it; whatever gave it to this thread ordered this
        // load after that store.
        let base = BASE.load(Ordering::Relaxed);
        debug_assert!(base != 0, "a `Ref32` with no program arena");
        let address = base + self.units.get() as usize * UNIT;
        // SAFETY: only `ProgramArena::alloc` makes a `Ref32<T>`, after writing a `T` at this
        // offset of the one range whose base `BASE` holds; the range is never unmapped
        // (`ManuallyDrop`), never moves and the value is never written again or dropped.
        unsafe { &*(address as *const T) }
    }

    /// The offset in [`UNIT`]s: the reference as a number, for a table's key. Nothing turns
    /// a number back into a reference.
    #[inline]
    pub fn units(self) -> u32 {
        self.units.get()
    }
}

impl<T: ProgramLifetime> Clone for Ref32<T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: ProgramLifetime> Copy for Ref32<T> {}

impl<T: ProgramLifetime> Deref for Ref32<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        self.get()
    }
}

/// Identity: two references are equal when they name one value.
impl<T: ProgramLifetime> PartialEq for Ref32<T> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.units == other.units
    }
}

impl<T: ProgramLifetime> Eq for Ref32<T> {}

impl<T: ProgramLifetime> std::hash::Hash for Ref32<T> {
    #[inline]
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.units.hash(state);
    }
}

impl<T: ProgramLifetime + fmt::Debug> fmt::Debug for Ref32<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.get().fmt(f)
    }
}

#[cfg(all(test, not(blaze_check)))]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct Node {
        kind: u16,
        parent: Option<Ref32<Node>>,
        next: Option<Ref32<Node>>,
        span: (u32, u32),
    }

    // SAFETY: plain fields and references into the arena; no `Drop`.
    unsafe impl ProgramLifetime for Node {}

    #[repr(align(16))]
    #[derive(Debug, PartialEq, Clone, Copy)]
    struct Wide(u8);

    // SAFETY: plain data.
    unsafe impl ProgramLifetime for Wide {}

    const _: () = assert!(size_of::<Ref32<Node>>() == 4);
    const _: () = assert!(size_of::<Option<Ref32<Node>>>() == 4, "the niche");
    const _: () = assert!(size_of::<Option<Ref32<u64>>>() == 4);
    const _: () = assert!(align_of::<Ref32<Node>>() == 4);
    // Two links and a parent in 12 bytes, where three `&'static Node` are 24.
    const _: () = assert!(size_of::<Node>() == 20);

    /// The process has one program arena, so its tests are one test: a round trip, the
    /// alignment of a wide type, the niche, a second `establish`, and exhaustion last (it
    /// uses the arena up).
    #[test]
    fn a_ref32_round_trips_and_exhaustion_is_a_typed_error() {
        let reserved = 4 * crate::page_size();
        let mut arena = ProgramArena::establish(reserved, Tag::Intern).expect("the first arena");
        assert!(matches!(
            ProgramArena::establish(reserved, Tag::Intern),
            Err(ProgramError::Established)
        ));
        assert!(matches!(
            ProgramArena::establish(MAX_RESERVED + 1, Tag::Intern),
            Err(ProgramError::Mem(MemError::TooLarge { .. }))
        ));

        let root = arena.alloc(Node { kind: 1, parent: None, next: None, span: (0, 9) }).unwrap();
        let child =
            arena.alloc(Node { kind: 2, parent: Some(root), next: None, span: (2, 5) }).unwrap();
        assert_eq!(root.units(), 1, "offset 0 is the niche: the first value is one unit in");
        assert_eq!(child.kind, 2);
        assert_eq!(child.parent, Some(root));
        assert_eq!(child.parent.unwrap().span, (0, 9));
        let kept: &'static Node = child.get();
        assert_eq!(kept.span, (2, 5));
        // A copy names the same value.
        let copy = child;
        assert!(std::ptr::eq(copy.get(), child.get()));
        assert_ne!(root, child);

        let byte = arena.alloc(7u8).unwrap();
        let wide = arena.alloc(Wide(9)).unwrap();
        assert_eq!((*byte, *wide), (7, Wide(9)));
        assert_eq!(wide.get() as *const Wide as usize % 16, 0, "aligned for its type");
        assert_eq!(byte.get() as *const u8 as usize % UNIT, 0);

        // Exhaustion: a typed error naming the sizes, and the arena as it was.
        let mut made = 0usize;
        let error = loop {
            match arena.alloc([made as u64; 32]) {
                Ok(it) => {
                    assert_eq!(it[31], made as u64);
                    made += 1;
                }
                Err(error) => break error,
            }
            assert!(made <= arena.reserved(), "the arena never fills");
        };
        assert!(made > 0);
        let used = arena.used();
        match error {
            MemError::Exhausted { tag, reserved: of, wanted } => {
                assert_eq!((tag, of), (Tag::Intern, arena.reserved()));
                assert!(wanted > of);
            }
            other => panic!("exhaustion is `MemError::Exhausted`: {other}"),
        }
        assert_eq!(arena.used(), used, "a refused allocation changes nothing");
        // What was allocated before is still there.
        assert_eq!(root.span, (0, 9));
        assert_eq!(child.parent, Some(root));
    }
}
