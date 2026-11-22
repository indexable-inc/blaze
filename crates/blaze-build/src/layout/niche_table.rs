//! The niche table: about fifty types laid out by layout.rs, each row's size, tag place, the
//! value each variant writes and the valid range derived BY HAND from rustc's rules (the pinned
//! toolchain's `rustc_abi`, crates/rustc_abi/src/layout.rs `layout_of_enum` and lib.rs
//! `Niche::reserve`), the reasoning in the comment above the row. A value a variant writes is
//! the one rustc's compiled code (std's, or a `transmute` in the user's) reads, so a wrong one
//! is a miscompile of real code, not a style difference.
//!
//! The rules the comments name: RESERVE (lib.rs `Niche::reserve`): one value, not `bool`'s
//! `0..=1`: the side whose next value is nearer zero as a signed number, BELOW the start on a
//! tie; more values: past the end unless that cannot reach zero cleanly. LARGEST: the untagged
//! variant is the LAST of the largest size. FIT: every other variant ends before the niche's
//! byte or starts after it, at its own alignment. CHOICE: the smaller layout; at equal sizes
//! the one with the larger niche; else tagged. WIDEN: a tagged enum's tag grows to the smallest
//! alignment of the variants' first non-1-ZST field. NECESSARY: an uninhabited variant's
//! discriminant is not in the tag's range. GAP: the tag's range is the smallest wrapping range
//! holding the necessary discriminants. ABSENT: uninhabited and every field a 1-ZST.
//!
//! OFFSETS are blaze's own struct order (decreasing alignment, ties by declaration), which is
//! not rustc's niche-biased sort: where a row's offset differs from rustc's the comment says
//! so (the values never differ). That sort is its own piece (the lane report's NOT DONE).
//!
//! The control (`a_planted_always_tagged_layout_fails_the_table`): with niche filling planted
//! off (`Layouts::niche_off`), the `Option<NonZero<u8>>` row (what a `transmute` to its byte
//! reads) and the other niche rows fail.
//!
//! The tables are in an [`Inline`] buffer on the test thread's own stack (a thread made with a
//! large stack): no heap is conjured for them.

use core::alloc::Allocator;

use blaze_own::{Alloc, Erased, Inline};
use blaze_typeck::{Adt, AdtKind, IntTy, ItemKey, Mutbl, PNode, PTy, UintTy};

use super::tests::{F, G, World};
use super::{IntRepr, LayoutCx, LayoutError, LayoutFacts, LayoutId, Layouts, Missing, ReprAttrs, ValidRange};

const U8: PNode = PNode::Uint(UintTy::U8);
const U16: PNode = PNode::Uint(UintTy::U16);
const U32: PNode = PNode::Uint(UintTy::U32);
const U64: PNode = PNode::Uint(UintTy::U64);
const USIZE: PNode = PNode::Uint(UintTy::Usize);
const MAX64: u128 = u64::MAX as u128;
const HIGH: u128 = 1 << 63;

/// The test world with `rustc_layout_scalar_valid_range_*` bounds on some of its structs.
struct Ranged<'w> {
    world: &'w World,
    ranges: &'w [(ItemKey, ValidRange)],
}

impl LayoutFacts for Ranged<'_> {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        self.world.adt(key)
    }
    fn repr(&self, key: ItemKey) -> Result<ReprAttrs, Missing> {
        self.world.repr(key)
    }
    fn discriminant(&self, key: ItemKey, variant: u32) -> Result<Option<i128>, Missing> {
        self.world.discriminant(key, variant)
    }
    fn valid_range(&self, key: ItemKey) -> Result<ValidRange, Missing> {
        match self.ranges.iter().find(|(k, _)| *k == key) {
            Some((_, r)) => Ok(*r),
            None => self.world.valid_range(key),
        }
    }
}

/// What a row's layout must be.
#[derive(Clone, Copy)]
struct Want {
    size: u64,
    /// In bytes.
    align: u64,
    /// The tag's offset and width in bytes; `None`: no tag (`read_tag` answers none).
    tag: Option<(u64, u64)>,
    /// The tag scalar's valid range (start, end), checked when given.
    tag_valid: Option<(u128, u128)>,
    /// Each (variant, the value `tag_write` stores for it; `None`: nothing stored), and every
    /// stored value must read back as its variant (`variant_of_tag`).
    writes: &'static [(u32, Option<u128>)],
    /// The layout's own scalar's valid range, when it must be one scalar.
    scalar: Option<(u128, u128)>,
    /// (variant, field, offset): a variant's field's offset in the enum (variant 0 of a struct).
    offsets: &'static [(u32, u32, u64)],
}

const W: Want = Want { size: 0, align: 1, tag: None, tag_valid: None, writes: &[], scalar: None, offsets: &[] };

/// What is wrong with layout `id` against `w`, each line naming the row.
fn wrong<A: Allocator + Clone>(name: &str, ls: &Layouts<A>, id: LayoutId, w: &Want) -> Vec<String> {
    let mut out = Vec::new();
    let l = ls.get(id);
    if (l.size, l.align_bytes()) != (w.size, w.align) {
        out.push(format!("{name}: size and alignment {:?}, want {:?}", (l.size, l.align_bytes()), (w.size, w.align)));
    }
    let tag = ls.read_tag(id);
    let place = tag.map(|(at, s)| (at, s.prim.size()));
    if place != w.tag {
        out.push(format!("{name}: the tag at {place:?} (offset, bytes), want {:?}", w.tag));
    }
    if let (Some(want), Some((_, s))) = (w.tag_valid, tag)
        && (s.valid.start, s.valid.end) != want
    {
        out.push(format!("{name}: the tag's valid range {:?}, want {want:?}", (s.valid.start, s.valid.end)));
    }
    for &(v, value) in w.writes {
        let written = ls.tag_write(id, v).map(|t| t.value);
        if written != value {
            out.push(format!("{name}: variant {v} writes {written:x?}, want {value:x?}"));
        }
        if let Some(bits) = written
            && ls.variant_of_tag(id, bits) != Some(v)
        {
            out.push(format!("{name}: variant {v}'s value {bits:#x} reads as {:?}", ls.variant_of_tag(id, bits)));
        }
    }
    if let Some(want) = w.scalar {
        let got = l.scalar.map(|s| (s.valid.start, s.valid.end));
        if got != Some(want) {
            out.push(format!("{name}: the scalar's valid range {got:x?}, want {want:x?}"));
        }
    }
    for &(v, f, off) in w.offsets {
        let got = ls.variant(id, v).and_then(|row| ls.field_offset(row, f));
        if got != Some(off) {
            out.push(format!("{name}: variant {v} field {f} at {got:?}, want {off}"));
        }
    }
    out
}

fn lay_in<A: Allocator + Clone>(facts: &dyn LayoutFacts, g: &mut G, ls: &mut Layouts<A>, scratch: Alloc<'_>, t: PTy) -> Result<LayoutId, LayoutError> {
    let mut cx = LayoutCx { ground: &mut g.t, facts, scratch };
    ls.of(t, &mut cx)
}

/// `base is start..=end`, the bounds as bits of the base.
fn pat(g: &mut G, base: PTy, start: u128, end: u128) -> PTy {
    let s = g.n(PNode::ConstValue { ty: base, bits: start });
    let e = g.n(PNode::ConstValue { ty: base, bits: end });
    let l = g.t.intern_list(&[s, e]).expect("the pattern's bounds intern");
    g.n(PNode::Pat(base, l))
}

/// Runs `f` on a thread with a 64 MiB stack (the [`Inline`] buffers live there), its panic
/// carried out whole.
fn on_a_large_stack<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    std::thread::scope(|s| {
        let thread = std::thread::Builder::new().stack_size(64 << 20).spawn_scoped(s, f).expect("the test's thread starts");
        thread.join().unwrap_or_else(|p| std::panic::resume_unwind(p))
    })
}

/// Every row laid out in `ls` (niche filling planted off when `niche_off`): the failures, and
/// whether the `Option<NonZero<u8>>` row (the transmute row) failed.
fn table(niche_off: bool) -> (Vec<String>, bool) {
    let inline = Inline::<{ 8 << 20 }>::new();
    let erased = Erased::new(&inline);
    let mut ls = Layouts::new_in(&inline);
    ls.niche_off = niche_off;
    let rust = ReprAttrs::RUST;
    let transparent = ReprAttrs { transparent: true, ..ReprAttrs::RUST };
    let mut w = World::default();
    let option = w.option();
    let result = w.def(AdtKind::Enum, 2, &[&[F::P(0)], &[F::P(1)]], rust);
    // `#[repr(transparent)] struct N<T>(T)`: core's `NonZero<T>(T::NonZeroInner)`,
    // `NonNull { pointer: *const T is !null }` (here over the pattern type itself),
    // `Cap(UsizeNoHighBit)`, `Alignment(AlignmentEnum)` alike.
    let newtype = w.strukt(1, &[F::P(0)], transparent);
    let unit = w.strukt(0, &[], rust);
    let phantom = w.strukt(1, &[], rust);
    let unique = w.strukt(1, &[F::AdtP(newtype, 0), F::AdtP(phantom, 0)], transparent);
    let three_fields = w.strukt(3, &[F::P(0), F::P(1), F::P(2)], rust);
    let two_fields = w.strukt(2, &[F::P(0), F::P(1)], rust);
    let three = w.def(AdtKind::Enum, 0, &[&[], &[], &[]], rust);
    let gap = w.def(AdtKind::Enum, 0, &[&[], &[], &[]], rust);
    w.def_mut(gap).discrs = vec![Some(0), Some(200), Some(100)];
    let neg = w.def(AdtKind::Enum, 0, &[&[], &[], &[]], rust);
    w.def_mut(neg).discrs = vec![Some(-1), Some(0), Some(1)];
    let ordering = w.def(AdtKind::Enum, 0, &[&[], &[], &[]], ReprAttrs { int: Some(IntRepr::Signed(IntTy::I8)), ..ReprAttrs::RUST });
    w.def_mut(ordering).discrs = vec![Some(-1), Some(0), Some(1)];
    let tie = w.def(AdtKind::Enum, 0, &[&[F::Leaf(U32), F::Leaf(PNode::Bool)], &[F::Leaf(U16)]], rust);
    let before = w.def(AdtKind::Enum, 0, &[&[F::Leaf(U32), F::Leaf(U16), F::Leaf(PNode::Bool), F::Leaf(U8)], &[F::Leaf(U16)]], rust);
    let fit = w.def(AdtKind::Enum, 1, &[&[F::P(0), F::Leaf(U64)], &[F::Leaf(U32)]], rust);
    let wide = w.def(AdtKind::Enum, 0, &[&[F::Leaf(U32)], &[F::Leaf(U16)]], rust);
    let big = w.def(AdtKind::Enum, 0, &[&[F::Leaf(U64)], &[F::Leaf(U8)]], rust);
    let wide8 = w.def(AdtKind::Enum, 0, &[&[F::Leaf(U64)], &[F::Leaf(U32)]], rust);
    let absent = w.def(AdtKind::Enum, 0, &[&[F::Leaf(U32)], &[F::Leaf(PNode::Never)], &[]], rust);
    let repr_u8 = w.def(AdtKind::Enum, 0, &[&[F::Leaf(U32)], &[F::Leaf(U8)]], ReprAttrs { int: Some(IntRepr::Unsigned(UintTy::U8)), ..ReprAttrs::RUST });
    let c_less = w.def(AdtKind::Enum, 0, &[&[], &[]], ReprAttrs { c: true, ..ReprAttrs::RUST });
    // core's `#[repr(usize)] enum AlignmentEnum { _Align1Shl0 = 1 << 0, .., _Align1Shl63 = 1 << 63 }`.
    let no_fields: Vec<&[F]> = vec![&[]; 64];
    let align_enum = w.def(AdtKind::Enum, 0, &no_fields, ReprAttrs { int: Some(IntRepr::Unsigned(UintTy::Usize)), ..ReprAttrs::RUST });
    w.def_mut(align_enum).discrs = (0..64).map(|n| Some(1i128 << n)).collect();
    let alignment = w.strukt(0, &[F::Adt0(align_enum)], transparent);
    // core's `struct Layout { size: usize, align: Alignment }`.
    let layout = w.strukt(0, &[F::Leaf(USIZE), F::Adt0(alignment)], rust);
    let nonzero_attr = w.strukt(0, &[F::Leaf(USIZE)], rust);
    let below_attr = w.strukt(0, &[F::Leaf(U8)], rust);
    let ranges = [(nonzero_attr, ValidRange { start: Some(1), end: None }), (below_attr, ValidRange { start: None, end: Some(254) })];
    let facts = Ranged { world: &w, ranges: &ranges };

    let mut g = G::new();
    let u8_ = g.n(U8);
    let u32_ = g.n(U32);
    let u64_ = g.n(U64);
    let usize_ = g.n(USIZE);
    let i32_ = g.n(PNode::Int(IntTy::I32));
    let bool_ = g.n(PNode::Bool);
    let char_ = g.n(PNode::Char);
    let never = g.n(PNode::Never);
    let u128_ = g.n(PNode::Uint(UintTy::U128));
    let unit_t = g.tuple(&[]);
    let nz8_inner = pat(&mut g, u8_, 1, 0xFF);
    let nz8 = g.adt(newtype, &[nz8_inner]);
    let nz32_inner = pat(&mut g, u32_, 1, 0xFFFF_FFFF);
    let nz32 = g.adt(newtype, &[nz32_inner]);
    let raw_u8 = g.n(PNode::Ptr(Mutbl::Not, u8_));
    let not_null = pat(&mut g, raw_u8, 1, MAX64);
    let nonnull = g.adt(newtype, &[not_null]);
    let ref_u8 = g.r(u8_);
    let slice = g.n(PNode::Slice(u8_));
    let ref_slice = g.r(slice);
    let char_inner = pat(&mut g, char_, 1, 0x10_FFFF);
    let nz_char = g.adt(newtype, &[char_inner]);
    let no_high_bit = pat(&mut g, usize_, 0, HIGH - 1);
    let cap = g.adt(newtype, &[no_high_bit]);
    let not_all_ones = pat(&mut g, i32_, 0, 0xFFFF_FFFE);
    let opt = |g: &mut G, t: PTy| g.adt(option, &[t]);

    let mut rows: Vec<(&'static str, PTy, Want)> = Vec::new();
    let mut row = |name: &'static str, t: PTy, want: Want| rows.push((name, t, want));

    // A full `u8` has no niche: tagged. NECESSARY 0, 1: a `u8` tag; WIDEN: `Some`'s `u8` has
    // alignment 1, no wider. The tag at 0, `Some`'s byte at 1.
    let t = opt(&mut g, u8_);
    row("Option<u8>", t, Want { size: 2, tag: Some((0, 1)), tag_valid: Some((0, 1)), writes: &[(0, Some(0)), (1, Some(1))], offsets: &[(1, 0, 1)], ..W });
    // `bool` 0..=1, one value: RESERVE's first branch skips `bool`; 0 is not above `count`, so
    // past the end: `None` is 2.
    let t = opt(&mut g, bool_);
    row("Option<bool>", t, Want { size: 1, tag: Some((0, 1)), writes: &[(0, Some(2)), (1, None)], scalar: Some((0, 2)), ..W });
    // `Option<bool>` is 0..=2, not `bool`'s range: next up 3, next down -1 (0xff): below, so
    // `None` is 0xff and 2 still reads as `Some(None)`.
    let ob = opt(&mut g, bool_);
    let t = opt(&mut g, ob);
    row("Option<Option<bool>>", t, Want { size: 1, tag: Some((0, 1)), writes: &[(0, Some(0xFF)), (1, None)], scalar: Some((0xFF, 2)), ..W });
    // `char` 0..=0x10FFFF: next up 0x110000, next down -1: below, `None` is 0xFFFF_FFFF (the
    // comment in rustc: "matches clang's WEOF").
    let oc = opt(&mut g, char_);
    row("Option<char>", oc, Want { size: 4, align: 4, tag: Some((0, 4)), writes: &[(0, Some(0xFFFF_FFFF)), (1, None)], scalar: Some((0xFFFF_FFFF, 0x10_FFFF)), ..W });
    // Its range 0xFFFF_FFFF..=0x10FFFF: next up 0x110000, next down -2: below again.
    let t = opt(&mut g, oc);
    row("Option<Option<char>>", t, Want { size: 4, align: 4, tag: Some((0, 4)), writes: &[(0, Some(0xFFFF_FFFE)), (1, None)], scalar: Some((0xFFFF_FFFE, 0x10_FFFF)), ..W });
    // `u8 is 1..=255`: next up 0, next down 0, the tie below: `None` is 0, and the byte's range
    // is then full. THE TRANSMUTE ROW: `transmute::<Option<NonZero<u8>>, u8>(None)` is 0.
    let onz8 = opt(&mut g, nz8);
    row("Option<NonZero<u8>>", onz8, Want { size: 1, tag: Some((0, 1)), writes: &[(0, Some(0)), (1, None)], scalar: Some((0, 0xFF)), ..W });
    // The inner range is full: no niche left, tagged (a `u8` tag, the payload at 1).
    let t = opt(&mut g, onz8);
    row("Option<Option<NonZero<u8>>>", t, Want { size: 2, tag: Some((0, 1)), writes: &[(0, Some(0)), (1, Some(1))], offsets: &[(1, 0, 1)], ..W });
    // `u32 is 1..`: `None` is 0.
    let t = opt(&mut g, nz32);
    row("Option<NonZero<u32>>", t, Want { size: 4, align: 4, tag: Some((0, 4)), writes: &[(0, Some(0)), (1, None)], scalar: Some((0, 0xFFFF_FFFF)), ..W });
    // `(u8, bool)`: blaze puts the `bool` at 1 (declaration order at one alignment; rustc's
    // sort puts the larger niche first, at 0). The niche is the `bool`'s: `None` is 2 there;
    // tagged would be 3 bytes.
    let t = g.tuple(&[u8_, bool_]);
    let t = opt(&mut g, t);
    row("Option<(u8, bool)>", t, Want { size: 2, tag: Some((1, 1)), writes: &[(0, Some(2)), (1, None)], ..W });
    // An array's niche is its first element's: `None` is 0 at offset 0; tagged would be 3.
    let arr = g.array(nz8, 2);
    let oarr = opt(&mut g, arr);
    row("Option<[NonZero<u8>; 2]>", oarr, Want { size: 2, tag: Some((0, 1)), writes: &[(0, Some(0)), (1, None)], ..W });
    // The first element's range is now full: tagged, 3 bytes.
    let t = opt(&mut g, oarr);
    row("Option<Option<[NonZero<u8>; 2]>>", t, Want { size: 3, tag: Some((0, 1)), writes: &[(0, Some(0)), (1, Some(1))], offsets: &[(1, 0, 1)], ..W });
    // TWO DATAFUL VARIANTS, `enum Tie { A(u32, bool), B(u16) }`: niche: A is u32@0 bool@4 (8
    // bytes), B (2 bytes) fits before the bool; 8 bytes, its niche 0..=2 (253 left). Tagged: a
    // `u8` tag (WIDEN: A's first field by offset is the bool, alignment 1), A bool@1 u32@4, B
    // u16@2: 8 bytes, the tag's niche 254 left. CHOICE: equal sizes, tagged's niche is larger:
    // TAGGED.
    let t = g.adt(tie, &[]);
    row("Tie (two dataful, equal sizes)", t, Want { size: 8, align: 4, tag: Some((0, 1)), writes: &[(0, Some(0)), (1, Some(1))], offsets: &[(0, 0, 4), (0, 1, 1), (1, 0, 2)], ..W });
    // TWO DATAFUL VARIANTS, FIT BEFORE: `enum Before { A(u32, u16, bool, u8), B(u16) }`: A is
    // u32@0 u16@4 bool@6 u8@7 (8 bytes), B (2 bytes) ends before the bool at 6; `B` is the bool's
    // 2. Tagged is 12 bytes. (rustc's niche-biased sort may put the bool at 7: the same values.)
    let t = g.adt(before, &[]);
    row("Before (fit before the niche)", t, Want { size: 8, align: 4, tag: Some((6, 1)), writes: &[(0, None), (1, Some(2))], offsets: &[(0, 2, 6), (1, 0, 0)], ..W });
    // TWO DATAFUL VARIANTS, FIT AFTER: `enum Fit { A(&u8, u64), B(u32) }`: A is &u8@0 u64@8 (16
    // bytes), B's u32 is moved past the pointer, to 8; `B` is the null pointer. Tagged would be
    // 24 (WIDEN: min(8, 4) gives a `u32` tag).
    let t = g.adt(fit, &[ref_u8]);
    row("Fit (fit after the niche)", t, Want { size: 16, align: 8, tag: Some((0, 8)), writes: &[(0, None), (1, Some(0))], offsets: &[(0, 0, 0), (0, 1, 8), (1, 0, 8)], ..W });
    // WIDEN: `enum Wide { A(u32), B(u16) }`: the first fields' alignments are 4 and 2: a `u16`
    // tag; A's u32 at 4, B's u16 at 2.
    let wide_t = g.adt(wide, &[]);
    row("Wide (a u16 tag)", wide_t, Want { size: 8, align: 4, tag: Some((0, 2)), tag_valid: Some((0, 1)), writes: &[(0, Some(0)), (1, Some(1))], offsets: &[(0, 0, 4), (1, 0, 2)], ..W });
    // Its `u16` tag 0..=1 is not `bool`'s byte: next up 2, next down -1: `None` is 0xFFFF.
    let t = opt(&mut g, wide_t);
    row("Option<Wide>", t, Want { size: 8, align: 4, tag: Some((0, 2)), writes: &[(0, Some(0xFFFF)), (1, None)], ..W });
    // WIDEN: min(8, 1) is 1: a `u8` tag.
    let t = g.adt(big, &[]);
    row("Big (A(u64), B(u8))", t, Want { size: 16, align: 8, tag: Some((0, 1)), writes: &[(0, Some(0)), (1, Some(1))], offsets: &[(0, 0, 8), (1, 0, 1)], ..W });
    // WIDEN: min(8, 4) is 4: a `u32` tag.
    let t = g.adt(wide8, &[]);
    row("Wide8 (A(u64), B(u32))", t, Want { size: 16, align: 8, tag: Some((0, 4)), writes: &[(0, Some(0)), (1, Some(1))], offsets: &[(0, 0, 8), (1, 0, 4)], ..W });
    // WIDEN: `Some`'s u64 makes the tag a `u64`.
    let ou64 = opt(&mut g, u64_);
    row("Option<u64>", ou64, Want { size: 16, align: 8, tag: Some((0, 8)), writes: &[(0, Some(0)), (1, Some(1))], offsets: &[(1, 0, 8)], ..W });
    // WIDEN to 16 bytes (`Integer::for_align(16)` is `i128`, aligned to 16 here).
    let t = opt(&mut g, u128_);
    row("Option<u128>", t, Want { size: 32, align: 16, tag: Some((0, 16)), writes: &[(0, Some(0)), (1, Some(1))], offsets: &[(1, 0, 16)], ..W });
    // Its tag `u64` 0..=1: next up 2, next down -1: `None` is u64::MAX; tagged would be 24.
    let t = opt(&mut g, ou64);
    row("Option<Option<u64>>", t, Want { size: 16, align: 8, tag: Some((0, 8)), writes: &[(0, Some(MAX64)), (1, None)], ..W });
    // `Option<u8>`'s `u8` tag 0..=1 IS `bool`'s range at one byte: past the end, 2.
    let ou8 = opt(&mut g, u8_);
    let t = opt(&mut g, ou8);
    row("Option<Option<u8>>", t, Want { size: 2, tag: Some((0, 1)), writes: &[(0, Some(2)), (1, None)], ..W });
    // Both variants zero-sized; LARGEST is the last (`Some`), which has no niche: tagged, the tag
    // its whole layout.
    let t = opt(&mut g, unit_t);
    row("Option<()>", t, Want { size: 1, tag: Some((0, 1)), writes: &[(0, Some(0)), (1, Some(1))], scalar: Some((0, 1)), ..W });
    // ABSENT: `Err(!)` is uninhabited and a 1-ZST: one present variant, no tag.
    let t = g.adt(result, &[u32_, never]);
    row("Result<u32, !>", t, Want { size: 4, align: 4, ..W });
    // NOT absent: `([u64; 0], !)` is uninhabited but aligned to 8. NECESSARY: only `Ok`'s 0, so
    // the tag's range is 0..=0; WIDEN: min(4, 8) is a `u32` tag; 8 bytes aligned to 8.
    let empty = g.array(u64_, 0);
    let void = g.tuple(&[empty, never]);
    let t = g.adt(result, &[u32_, void]);
    row("Result<u32, ([u64; 0], !)>", t, Want { size: 8, align: 8, tag: Some((0, 4)), tag_valid: Some((0, 0)), writes: &[(0, Some(0))], offsets: &[(0, 0, 4)], ..W });
    // `enum Absent { A(u32), B(!), C }`: B is ABSENT; NECESSARY 0 and 2; WIDEN: A's u32 (B's `!`
    // is a 1-ZST, C has no field): a `u32` tag 0..=2.
    let absent_t = g.adt(absent, &[]);
    row("Absent { A(u32), B(!), C }", absent_t, Want { size: 8, align: 4, tag: Some((0, 4)), tag_valid: Some((0, 2)), writes: &[(0, Some(0)), (2, Some(2))], offsets: &[(0, 0, 4)], ..W });
    // Its tag 0..=2 at 32 bits: next up 3, next down -1: `None` is 0xFFFF_FFFF.
    let t = opt(&mut g, absent_t);
    row("Option<Absent>", t, Want { size: 8, align: 4, tag: Some((0, 4)), writes: &[(0, Some(0xFFFF_FFFF)), (1, None)], ..W });
    // GAP: discriminants 0, 200, 100: `0..=200` spans more than half; the wrap-around ranges
    // `100..=0` and `200..=100` are both 157 values, the first kept.
    let gap_t = g.adt(gap, &[]);
    row("Gap { A = 0, B = 200, C = 100 }", gap_t, Want { size: 1, tag: Some((0, 1)), tag_valid: Some((100, 0)), writes: &[(0, Some(0)), (1, Some(200)), (2, Some(100))], scalar: Some((100, 0)), ..W });
    // `100..=0`: next up 1, next down 99: past the end, `None` is 1.
    let t = opt(&mut g, gap_t);
    row("Option<Gap>", t, Want { size: 1, tag: Some((0, 1)), writes: &[(0, Some(1)), (1, None)], scalar: Some((100, 1)), ..W });
    // A negative discriminant without a repr (`Ordering`'s values): a signed `i8` tag whose range
    // is the wrap-around `0xff..=1`.
    let neg_t = g.adt(neg, &[]);
    row("Neg { A = -1, B, C }", neg_t, Want { size: 1, tag: Some((0, 1)), tag_valid: Some((0xFF, 1)), writes: &[(0, Some(0xFF)), (1, Some(0)), (2, Some(1))], ..W });
    // `0xff..=1`: next up 2, next down -2, the tie below: `None` is 0xfe (-2).
    let oneg = opt(&mut g, neg_t);
    row("Option<Neg>", oneg, Want { size: 1, tag: Some((0, 1)), writes: &[(0, Some(0xFE)), (1, None)], scalar: Some((0xFE, 1)), ..W });
    // `0xfe..=1`: next up 2, next down -3: past the end, 2.
    let t = opt(&mut g, oneg);
    row("Option<Option<Neg>>", t, Want { size: 1, tag: Some((0, 1)), writes: &[(0, Some(2)), (1, None)], scalar: Some((0xFE, 2)), ..W });
    // `#[repr(i8)] Ordering`: the same niche as `Neg`'s, `None` is -2.
    let ord = g.adt(ordering, &[]);
    let t = opt(&mut g, ord);
    row("Option<Ordering>", t, Want { size: 1, tag: Some((0, 1)), writes: &[(0, Some(0xFE)), (1, None)], ..W });
    // A field-less enum of three: tag 0..=2, next up 3, next down -1: `None` is 0xff.
    let three_t = g.adt(three, &[]);
    let t = opt(&mut g, three_t);
    row("Option<Three>", t, Want { size: 1, tag: Some((0, 1)), writes: &[(0, Some(0xFF)), (1, None)], scalar: Some((0xFF, 2)), ..W });
    // `#[repr(u8)] enum R { A(u32), B(u8) }`: a union of `repr(C)` structs each starting with the
    // tag (RFC 2195): A's u32 at 4, B's u8 at 1.
    let r = g.adt(repr_u8, &[]);
    row("repr(u8) R { A(u32), B(u8) }", r, Want { size: 8, align: 4, tag: Some((0, 1)), tag_valid: Some((0, 1)), writes: &[(0, Some(0)), (1, Some(1))], offsets: &[(0, 0, 4), (1, 0, 1)], ..W });
    // An outer `Option` uses that tag's niche (`bool`'s range at a byte: past the end, 2).
    let t = opt(&mut g, r);
    row("Option<repr(u8) R>", t, Want { size: 8, align: 4, tag: Some((0, 1)), writes: &[(0, Some(2)), (1, None)], ..W });
    // A field-less `repr(C)` enum is C's `int`.
    let t = g.adt(c_less, &[]);
    row("repr(C) CLess { A, B }", t, Want { size: 4, align: 4, tag: Some((0, 4)), writes: &[(0, Some(0)), (1, Some(1))], ..W });
    // `Result<(), NonZero<u8>>`: LARGEST is `Err`; `Ok` is the tie below, 0.
    let rnz = g.adt(result, &[unit_t, nz8]);
    row("Result<(), NonZero<u8>>", rnz, Want { size: 1, tag: Some((0, 1)), writes: &[(0, Some(0)), (1, None)], scalar: Some((0, 0xFF)), ..W });
    // Its byte's range is full: the outer `Option` is tagged.
    let t = opt(&mut g, rnz);
    row("Option<Result<(), NonZero<u8>>>", t, Want { size: 2, tag: Some((0, 1)), writes: &[(0, Some(0)), (1, Some(1))], offsets: &[(1, 0, 1)], ..W });
    // REAL CORE: `Alignment(AlignmentEnum)`, a `repr(usize)` tag of 1 << 0 ..= 1 << 63.
    let align_t = g.adt(alignment, &[]);
    row("core Alignment", align_t, Want { size: 8, align: 8, scalar: Some((1, HIGH)), ..W });
    // 1..=2^63: next up 2^63 + 1 (-(2^63 - 1)), next down 0: below, `None` is 0.
    let t = opt(&mut g, align_t);
    row("Option<core Alignment>", t, Want { size: 8, align: 8, tag: Some((0, 8)), writes: &[(0, Some(0)), (1, None)], scalar: Some((0, HIGH)), ..W });
    // REAL CORE: `Layout { size: usize, align: Alignment }`: blaze puts `align` at 8 (rustc at
    // 0, its niche first).
    let layout_t = g.adt(layout, &[]);
    row("core Layout", layout_t, Want { size: 16, align: 8, offsets: &[(0, 0, 0), (0, 1, 8)], ..W });
    // REAL CORE: `Result<Layout, LayoutError>`: `Err` (a unit struct) is the align word's 0.
    let layout_error = g.adt(unit, &[]);
    let t = g.adt(result, &[layout_t, layout_error]);
    row("Result<core Layout, LayoutError>", t, Want { size: 16, align: 8, tag: Some((8, 8)), writes: &[(0, None), (1, Some(0))], ..W });
    // REAL CORE: `Option<(NonNull<u8>, Layout)>` (`RawVecInner::current_memory`): the tuple's
    // largest niche is the align word's (2^63 left against the pointer's 1), at 16 here.
    let pair = g.tuple(&[nonnull, layout_t]);
    let t = opt(&mut g, pair);
    row("Option<(NonNull<u8>, core Layout)>", t, Want { size: 24, align: 8, tag: Some((16, 8)), writes: &[(0, Some(0)), (1, None)], ..W });
    // REAL CORE: `NonZeroCharInner`, `char is '\u{1}'..='\u{10FFFF}'` (its base has its own
    // range, which the pattern replaces): next down 0, `None` is 0.
    let t = opt(&mut g, nz_char);
    row("Option<NonZero<char>>", t, Want { size: 4, align: 4, tag: Some((0, 4)), writes: &[(0, Some(0)), (1, None)], scalar: Some((0, 0x10_FFFF)), ..W });
    // REAL CORE: `Cap(UsizeNoHighBit)`, `usize is 0..=isize::MAX`: next up 2^63 (-2^63), next
    // down -1: below, `None` is usize::MAX (not 2^63).
    let t = opt(&mut g, cap);
    row("Option<Cap>", t, Want { size: 8, align: 8, tag: Some((0, 8)), writes: &[(0, Some(MAX64)), (1, None)], scalar: Some((MAX64, HIGH - 1)), ..W });
    // REAL CORE: `RawVecInner { ptr: Unique<u8>, cap: Cap, alloc: Global }`: the largest niche
    // is `cap`'s at 8 (rustc: at 0).
    let uniq = g.adt(unique, &[not_null]);
    let global = g.adt(unit, &[]);
    let inner = g.adt(three_fields, &[uniq, cap, global]);
    let t = opt(&mut g, inner);
    row("Option<RawVecInner>", t, Want { size: 16, align: 8, tag: Some((8, 8)), writes: &[(0, Some(MAX64)), (1, None)], ..W });
    // `Vec`'s shape, `{ buf: RawVecInner, len: usize }`: the same niche, 24 bytes.
    let vec = g.adt(two_fields, &[inner, usize_]);
    let t = opt(&mut g, vec);
    row("Option<Vec<u8>>", t, Want { size: 24, align: 8, tag: Some((8, 8)), writes: &[(0, Some(MAX64)), (1, None)], ..W });
    // A wide reference: the address's niche, `None` is 0.
    let t = opt(&mut g, ref_slice);
    row("Option<&[u8]>", t, Want { size: 16, align: 8, tag: Some((0, 8)), writes: &[(0, Some(0)), (1, None)], ..W });
    // `i32 is ..-1 | 0..` (`I32NotAllOnes`, canonically 0..=0xFFFF_FFFE): next up -1, next down
    // -1, the tie below: `None` is -1, the range then full.
    let not_all_ones_t = g.adt(newtype, &[not_all_ones]);
    let t = opt(&mut g, not_all_ones_t);
    row("Option<I32NotAllOnes>", t, Want { size: 4, align: 4, tag: Some((0, 4)), writes: &[(0, Some(0xFFFF_FFFF)), (1, None)], scalar: Some((0xFFFF_FFFF, 0xFFFF_FFFE)), ..W });
    // `rustc_layout_scalar_valid_range_start(1)` on `struct S(usize)`: `None` is 0.
    let s = g.adt(nonzero_attr, &[]);
    let t = opt(&mut g, s);
    row("Option<#[valid_range_start(1)] S(usize)>", t, Want { size: 8, align: 8, tag: Some((0, 8)), writes: &[(0, Some(0)), (1, None)], scalar: Some((0, MAX64)), ..W });
    // `rustc_layout_scalar_valid_range_end(254)` on `struct S(u8)`: 0..=254, next up -1, next
    // down -1, the tie below: `None` is 0xff.
    let s = g.adt(below_attr, &[]);
    let t = opt(&mut g, s);
    row("Option<#[valid_range_end(254)] S(u8)>", t, Want { size: 1, tag: Some((0, 1)), writes: &[(0, Some(0xFF)), (1, None)], scalar: Some((0xFF, 254)), ..W });

    let mut failures = Vec::new();
    let mut transmute_row_failed = false;
    for (name, t, want) in rows {
        let found = match lay_in(&facts, &mut g, &mut ls, erased.handle(), t) {
            Ok(id) => wrong(name, &ls, id, &want),
            Err(e) => vec![format!("{name}: refused: {e}")],
        };
        if name == "Option<NonZero<u8>>" && !found.is_empty() {
            transmute_row_failed = true;
        }
        failures.extend(found);
    }
    (failures, transmute_row_failed)
}

#[test]
fn every_row_is_rustcs_layout() {
    let (failures, _) = on_a_large_stack(|| table(false));
    assert!(failures.is_empty(), "{} wrong:\n{}", failures.len(), failures.join("\n"));
}

/// The control: niche filling planted off, the transmute row (`Option<NonZero<u8>>` is one
/// byte, `None` 0) fails, as every niche row does.
#[test]
fn a_planted_always_tagged_layout_fails_the_table() {
    let (failures, transmute_row_failed) = on_a_large_stack(|| table(true));
    assert!(transmute_row_failed, "with niche filling off the Option<NonZero<u8>> row passed: the table does not see the niche; failures: {failures:?}");
    assert!(failures.len() > 20, "with niche filling off only {} checks failed: {failures:?}", failures.len());
}
