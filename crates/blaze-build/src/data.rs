//! Constant data and statics: the one form of a value's initial bytes ([`DataRecord`], agreed
//! with the constant evaluator, crates/blaze-consteval, which makes them for every initialiser
//! that is not a plain literal), and the image a program's statics are placed in
//! ([`StaticImage`]).
//!
//! # The form
//!
//! A record is a list of allocations; the first is the value itself, the rest what it points
//! to (a `&[&str]`'s slice and strings, a `&'static T`'s target). Each allocation is its bytes
//! (padding and uninitialised bytes written 0), its alignment, whether it may be written (a
//! `static mut`, or a static with interior mutability), and its relocations: at `offset`, an
//! 8-byte little-endian word that holds the *addend* and becomes the address of the target plus
//! that addend. A target is another allocation of the record, a `static` (by item: statics are
//! never generic), or a fn instance (by [`InstanceKey`]). Pointers are 8 bytes, little-endian
//! (AArch64 and x86-64).
//!
//! A string or byte string literal is a record of one allocation of its bytes, alignment 1, no
//! relocation ([`DataRecord::literal`]); a function using one keeps it in its own data row
//! (blaze-lir's `data`, placed after its code by blaze-emit) and builds `&str` as (address,
//! length) per [`crate::layout`]'s wide-pointer rule.
//!
//! # Statics
//!
//! A program's statics are placed in one image ([`StaticImage::new`]): sorted by item key (a
//! deterministic order, rule 25), each record's allocations one after another, each at its
//! alignment. A relocation to a static or to an allocation of the image is resolved at once to
//! an offset in the image; one to a fn is left for placement. [`StaticImage::fill`] writes the
//! image at the address it is placed at: every relocation word becomes an absolute address.
//! The image is one writable block: immutable statics in read-only pages is a later hardening,
//! not a meaning (writing an immutable static is UB already).

use core::alloc::Allocator;
use core::fmt;

use blaze_typeck::ItemKey;

use crate::InstanceKey;

/// The most alignment an allocation may ask: a page.
pub const MOST_ALIGN: u8 = 12;

/// What a relocation word becomes the address of.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum DataTarget {
    /// Allocation `n` of the same record.
    Local(u32),
    /// The static item's first byte.
    Static(ItemKey),
    /// The fn instance's entry.
    Fn(InstanceKey),
    /// A vtable's first word (crate::vtable): a supertrait's table from a table, or a
    /// `&dyn Tr` in a constant.
    Vtable(crate::vtable::VtableKey),
}

/// At `offset`, an 8-byte word holding the addend, to become the target's address plus it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct DataReloc {
    pub offset: u32,
    pub target: DataTarget,
}

/// One allocation of a record.
pub struct DataAlloc<A: Allocator> {
    pub bytes: Vec<u8, A>,
    /// log2 of the alignment.
    pub align: u8,
    pub mutable: bool,
    /// Sorted by offset, none overlapping.
    pub relocs: Vec<DataReloc, A>,
}

/// A value's initial bytes and what they point to: `allocs[0]` is the value.
pub struct DataRecord<A: Allocator> {
    pub allocs: Vec<DataAlloc<A>, A>,
}

/// A record or an image that breaks the form, named.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DataError {
    /// A record with no allocation.
    Empty,
    /// Allocation `alloc` asks an alignment past [`MOST_ALIGN`].
    Align { alloc: u32 },
    /// Relocation `reloc` of allocation `alloc` is past its end, overlaps the one before, or
    /// is out of order.
    Reloc { alloc: u32, reloc: u32 },
    /// A `Local` target past the record's allocations.
    Local { alloc: u32, target: u32 },
    /// A relocation names a static the image does not hold.
    NoStatic { static_: ItemKey },
    /// Two records for one static.
    Twice { static_: ItemKey },
    /// The image passed 4 GiB.
    TooLarge,
    /// Placement found no address for a fn instance a static points to.
    NoFn { key: InstanceKey },
    /// `fill` was given a buffer of another length, or a base not aligned to the image.
    Placement,
    /// A static points to a vtable: the image places no vtables yet.
    Vtable { key: crate::vtable::VtableKey },
}

impl fmt::Display for DataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DataError::Empty => write!(f, "a data record with no allocation"),
            DataError::Align { alloc } => write!(f, "allocation {alloc}: alignment past 4 KiB"),
            DataError::Reloc { alloc, reloc } => {
                write!(f, "allocation {alloc}, relocation {reloc}: past the end, overlapping or out of order")
            }
            DataError::Local { alloc, target } => write!(f, "allocation {alloc}: a relocation to allocation {target}, which the record lacks"),
            DataError::NoStatic { static_ } => write!(f, "a relocation to static {:#x}, which the program does not place", static_.0),
            DataError::Twice { static_ } => write!(f, "static {:#x} has two records", static_.0),
            DataError::TooLarge => write!(f, "the statics' image passes 4 GiB"),
            DataError::NoFn { key } => write!(f, "a static points to fn instance {:?}, which placement has no address for", key.0),
            DataError::Placement => write!(f, "the image was placed in a buffer of another length or alignment"),
            DataError::Vtable { key } => write!(f, "a static points to vtable {:?}: the statics' image places no vtables yet", key.0),
        }
    }
}

impl core::error::Error for DataError {}

impl<A: Allocator + Copy> DataRecord<A> {
    /// A literal's bytes: one allocation, alignment 1, immutable, no relocation.
    pub fn literal(bytes: &[u8], alloc: A) -> DataRecord<A> {
        let mut b = Vec::with_capacity_in(bytes.len(), alloc);
        b.extend_from_slice(bytes);
        let mut allocs = Vec::with_capacity_in(1, alloc);
        allocs.push(DataAlloc { bytes: b, align: 0, mutable: false, relocs: Vec::new_in(alloc) });
        DataRecord { allocs }
    }
}

impl<A: Allocator> DataRecord<A> {
    /// Checks the form: an allocation at least, alignments, relocations in bounds, sorted and
    /// apart, `Local` targets in range.
    pub fn check(&self) -> Result<(), DataError> {
        if self.allocs.is_empty() {
            return Err(DataError::Empty);
        }
        for (a, alloc) in self.allocs.iter().enumerate() {
            let a = a as u32;
            if alloc.align > MOST_ALIGN {
                return Err(DataError::Align { alloc: a });
            }
            let mut next = 0u64;
            for (r, reloc) in alloc.relocs.iter().enumerate() {
                let at = u64::from(reloc.offset);
                if at < next || at + 8 > alloc.bytes.len() as u64 {
                    return Err(DataError::Reloc { alloc: a, reloc: r as u32 });
                }
                next = at + 8;
                if let DataTarget::Local(t) = reloc.target
                    && t as usize >= self.allocs.len()
                {
                    return Err(DataError::Local { alloc: a, target: t });
                }
            }
        }
        Ok(())
    }
}

/// Where an image's relocation points.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImageTarget {
    /// A byte of the image.
    Image(u32),
    Fn(InstanceKey),
}

/// An absolute address word of the image: at `at`, the target plus `addend`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ImageReloc {
    pub at: u32,
    pub target: ImageTarget,
    pub addend: i64,
}

/// A program's statics laid out in one block.
pub struct StaticImage<A: Allocator> {
    /// The initial bytes, each relocation word 0 until [`StaticImage::fill`].
    pub bytes: Vec<u8, A>,
    /// log2 of the block's alignment.
    pub align: u8,
    pub relocs: Vec<ImageReloc, A>,
    /// Each static's first byte, sorted by key.
    pub statics: Vec<(ItemKey, u32), A>,
}

fn align_up(at: u64, align: u8) -> u64 {
    let a = 1u64 << align;
    (at + a - 1) & !(a - 1)
}

impl<A: Allocator + Copy> StaticImage<A> {
    /// Lays out `statics` (each a static item and its checked record).
    pub fn new<B: Allocator>(statics: &[(ItemKey, &DataRecord<B>)], alloc: A) -> Result<StaticImage<A>, DataError> {
        let mut order: Vec<usize, A> = Vec::with_capacity_in(statics.len(), alloc);
        order.extend(0..statics.len());
        order.sort_by_key(|&i| statics[i].0);
        for w in order.windows(2) {
            if statics[w[0]].0 == statics[w[1]].0 {
                return Err(DataError::Twice { static_: statics[w[0]].0 });
            }
        }
        // Pass 1: every allocation's offset; the statics' roots.
        let mut at_of: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(statics.len(), alloc);
        at_of.resize_with(statics.len(), || Vec::new_in(alloc));
        let mut roots: Vec<(ItemKey, u32), A> = Vec::with_capacity_in(statics.len(), alloc);
        let mut end = 0u64;
        let mut align = 0u8;
        for &i in order.iter() {
            let record = statics[i].1;
            record.check()?;
            for a in record.allocs.iter() {
                let at = align_up(end, a.align);
                end = at + a.bytes.len() as u64;
                if end > u64::from(u32::MAX) {
                    return Err(DataError::TooLarge);
                }
                align = align.max(a.align);
                at_of[i].push(at as u32);
            }
            roots.push((statics[i].0, at_of[i][0]));
        }
        let root_of = |key: ItemKey| roots.binary_search_by_key(&key, |r| r.0).ok().map(|k| roots[k].1);
        // Pass 2: the bytes and the relocations.
        let mut bytes = Vec::with_capacity_in(end as usize, alloc);
        bytes.resize(end as usize, 0);
        let mut relocs = Vec::new_in(alloc);
        for &i in order.iter() {
            let record = statics[i].1;
            for (k, a) in record.allocs.iter().enumerate() {
                let at = at_of[i][k] as usize;
                bytes[at..at + a.bytes.len()].copy_from_slice(&a.bytes);
                for r in a.relocs.iter() {
                    let word = at + r.offset as usize;
                    let mut addend = [0u8; 8];
                    addend.copy_from_slice(&bytes[word..word + 8]);
                    bytes[word..word + 8].fill(0);
                    let target = match r.target {
                        DataTarget::Local(t) => ImageTarget::Image(at_of[i][t as usize]),
                        DataTarget::Static(key) => ImageTarget::Image(root_of(key).ok_or(DataError::NoStatic { static_: key })?),
                        DataTarget::Fn(key) => ImageTarget::Fn(key),
                        DataTarget::Vtable(key) => return Err(DataError::Vtable { key }),
                    };
                    relocs.push(ImageReloc { at: word as u32, target, addend: i64::from_le_bytes(addend) });
                }
            }
        }
        let mut sorted = Vec::with_capacity_in(roots.len(), alloc);
        sorted.extend_from_slice(&roots);
        Ok(StaticImage { bytes, align, relocs, statics: sorted })
    }

    /// Static `key`'s offset in the image.
    pub fn static_at(&self, key: ItemKey) -> Option<u32> {
        self.statics.binary_search_by_key(&key, |r| r.0).ok().map(|k| self.statics[k].1)
    }

    /// Writes the image into `out`, which is placed at address `base` (aligned to the image),
    /// every relocation word made absolute; `fn_at` gives each fn instance's address.
    pub fn fill(&self, base: u64, out: &mut [u8], mut fn_at: impl FnMut(InstanceKey) -> Option<u64>) -> Result<(), DataError> {
        if out.len() != self.bytes.len() || base & ((1u64 << self.align) - 1) != 0 {
            return Err(DataError::Placement);
        }
        out.copy_from_slice(&self.bytes);
        for r in self.relocs.iter() {
            let to = match r.target {
                ImageTarget::Image(off) => base + u64::from(off),
                ImageTarget::Fn(key) => fn_at(key).ok_or(DataError::NoFn { key })?,
            };
            let at = r.at as usize;
            out[at..at + 8].copy_from_slice(&to.wrapping_add_signed(r.addend).to_le_bytes());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::alloc::Global;

    use blaze_ids::ContentId;
    use blaze_typeck::ItemKey;

    use super::{DataAlloc, DataError, DataRecord, DataReloc, DataTarget, StaticImage};
    use crate::InstanceKey;

    fn alloc(bytes: &[u8], align: u8, relocs: &[DataReloc]) -> DataAlloc<Global> {
        DataAlloc { bytes: bytes.to_vec_in(Global), align, mutable: false, relocs: relocs.to_vec_in(Global) }
    }

    fn record(allocs: Vec<DataAlloc<Global>>) -> DataRecord<Global> {
        let mut v = Vec::new_in(Global);
        v.extend(allocs);
        DataRecord { allocs: v }
    }

    /// `static A: &[u8] = b"hey";` (a wide pointer to a local allocation), `static B: &&[u8] =
    /// &A` with addend, and `static F: fn() = f`: each word becomes base + offset + addend.
    #[test]
    fn statics_point_at_each_other_their_allocations_and_fns() {
        let (a, b, f) = (ItemKey(3), ItemKey(1), ItemKey(2));
        let mut wide = [0u8; 16];
        wide[8..].copy_from_slice(&3u64.to_le_bytes());
        let ra = record(vec![alloc(&wide, 3, &[DataReloc { offset: 0, target: DataTarget::Local(1) }]), alloc(b"hey", 0, &[])]);
        let mut word = [0u8; 8];
        word.copy_from_slice(&4i64.to_le_bytes());
        let rb = record(vec![alloc(&word, 3, &[DataReloc { offset: 0, target: DataTarget::Static(a) }])]);
        let key = InstanceKey(ContentId::of("test fn", b"f"));
        let rf = record(vec![alloc(&[0; 8], 3, &[DataReloc { offset: 0, target: DataTarget::Fn(key) }])]);
        let image = StaticImage::new(&[(a, &ra), (b, &rb), (f, &rf)], Global).expect("lays out");
        // Sorted by key: B (1) at 0, F (2) at 8, A (3) at 16, A's text at 32.
        assert_eq!((image.static_at(b), image.static_at(f), image.static_at(a)), (Some(0), Some(8), Some(16)));
        let base = 0x1_0000u64;
        let mut out = vec![0u8; image.bytes.len()];
        image.fill(base, &mut out, |k| (k == key).then_some(0xF00D)).expect("fills");
        let word = |at: usize| u64::from_le_bytes(out[at..at + 8].try_into().unwrap());
        assert_eq!(word(0), base + 16 + 4, "B: A's address plus the addend 4");
        assert_eq!(word(8), 0xF00D, "F: the fn's address");
        assert_eq!(word(16), base + 32, "A's pointer: its text");
        assert_eq!(word(24), 3, "A's length is kept");
        assert_eq!(&out[32..35], b"hey");
    }

    #[test]
    fn broken_forms_are_refused_by_name() {
        let r = record(vec![alloc(&[0; 12], 0, &[DataReloc { offset: 8, target: DataTarget::Local(0) }])]);
        assert_eq!(r.check(), Err(DataError::Reloc { alloc: 0, reloc: 0 }), "a word past the end");
        let r = record(vec![alloc(&[0; 16], 0, &[DataReloc { offset: 0, target: DataTarget::Local(0) }, DataReloc { offset: 4, target: DataTarget::Local(0) }])]);
        assert_eq!(r.check(), Err(DataError::Reloc { alloc: 0, reloc: 1 }), "overlapping words");
        let r = record(vec![alloc(&[0; 8], 0, &[DataReloc { offset: 0, target: DataTarget::Local(5) }])]);
        assert_eq!(r.check(), Err(DataError::Local { alloc: 0, target: 5 }));
        let r = record(vec![alloc(&[0; 8], 0, &[DataReloc { offset: 0, target: DataTarget::Static(ItemKey(9)) }])]);
        assert!(matches!(StaticImage::new(&[(ItemKey(1), &r)], Global), Err(DataError::NoStatic { .. })));
        let lit = DataRecord::literal(b"x", Global);
        assert!(matches!(StaticImage::new(&[(ItemKey(1), &lit), (ItemKey(1), &lit)], Global), Err(DataError::Twice { .. })));
    }
}
