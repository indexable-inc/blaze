//! Layout's rules, each test naming the rule it checks (the module doc), the layouts Rust
//! guarantees by name, and one property test over random types.

use std::alloc::Global;

use blaze_own::Alloc;
use blaze_sim::{Broke, Config, Tape};
use blaze_typeck::{Adt, AdtKind, CtorKind, Field, IntTy, ItemKey, List, Mutbl, Name, PNode, PTy, PortableTys, UintTy, Variant};

use super::{
    DynPrefix, IntRepr, LayoutCx, LayoutFacts, LayoutId, Layouts, Prim, Refusal, ReprAttrs, Scalar, TagEncoding, Tail, ValidRange, Variants,
};

/// One ADT of the test world: its own type table, its variants' field types in it.
pub(crate) struct Def {
    key: ItemKey,
    kind: AdtKind,
    generics: u32,
    variants: Vec<Variant>,
    fields: Vec<Field>,
    types: PortableTys<'static>,
    repr: ReprAttrs,
    pub(crate) discrs: Vec<Option<i128>>,
    valid: ValidRange,
}

#[derive(Default)]
pub(crate) struct World {
    defs: Vec<Def>,
    /// An enum whose explicit discriminants' records are not made yet.
    missing: core::cell::Cell<Option<ItemKey>>,
    /// An ADT whose head is not made yet: its `repr`, valid range and discriminants are
    /// `Missing` (a lazy front end asked before the head, D194).
    unmade: core::cell::Cell<Option<ItemKey>>,
}

impl LayoutFacts for World {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        let d = self.defs.iter().find(|d| d.key == key)?;
        Some(Adt { kind: d.kind, generics: d.generics, variants: &d.variants, fields: &d.fields, types: d.types.view() })
    }
    fn repr(&self, key: ItemKey) -> Result<ReprAttrs, super::Missing> {
        if self.unmade.get() == Some(key) {
            return Err(super::Missing { item: key });
        }
        Ok(self.defs.iter().find(|d| d.key == key).map_or(ReprAttrs::RUST, |d| d.repr))
    }
    fn discriminant(&self, key: ItemKey, variant: u32) -> Result<Option<i128>, super::Missing> {
        if self.missing.get() == Some(key) || self.unmade.get() == Some(key) {
            return Err(super::Missing { item: key });
        }
        Ok(self.defs.iter().find(|d| d.key == key).and_then(|d| d.discrs.get(variant as usize).copied().flatten()))
    }
    fn valid_range(&self, key: ItemKey) -> Result<ValidRange, super::Missing> {
        if self.unmade.get() == Some(key) {
            return Err(super::Missing { item: key });
        }
        Ok(self.defs.iter().find(|d| d.key == key).map_or(ValidRange::default(), |d| d.valid))
    }
}

/// The planted control of `a_repr_asked_before_its_head_caches_nothing`: the old answer to a
/// `repr` asked before the ADT's head, a default `repr(Rust)` where the head is unmade.
struct DefaultsRepr<'w>(&'w World);

impl LayoutFacts for DefaultsRepr<'_> {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        self.0.adt(key)
    }
    fn repr(&self, key: ItemKey) -> Result<ReprAttrs, super::Missing> {
        if self.0.unmade.get() == Some(key) {
            return Ok(ReprAttrs::RUST);
        }
        self.0.repr(key)
    }
    fn discriminant(&self, key: ItemKey, variant: u32) -> Result<Option<i128>, super::Missing> {
        self.0.discriminant(key, variant)
    }
    fn valid_range(&self, key: ItemKey) -> Result<ValidRange, super::Missing> {
        if self.0.unmade.get() == Some(key) {
            return Ok(ValidRange::default());
        }
        self.0.valid_range(key)
    }
}

/// A field type of a definition, written in the definition's own table.
#[derive(Clone, Copy)]
pub(crate) enum F {
    /// Generic parameter `i`.
    P(u32),
    /// A ground-free node with no children (`u8`, `bool`, ...).
    Leaf(PNode),
    /// `*const P(i)`.
    RawP(u32),
    /// Another ADT applied to `P(i)`.
    AdtP(ItemKey, u32),
    /// Another ADT with no arguments.
    Adt0(ItemKey),
}

impl World {
    /// Defines an ADT: `variants` lists each variant's field types.
    pub(crate) fn def(&mut self, kind: AdtKind, generics: u32, variants: &[&[F]], repr: ReprAttrs) -> ItemKey {
        let key = ItemKey(0x1000 + self.defs.len() as u128);
        let mut types = PortableTys::new_in(Alloc::global());
        let mut vs = Vec::new();
        let mut fields = Vec::new();
        for (vi, fs) in variants.iter().enumerate() {
            let start = fields.len() as u32;
            for (fi, f) in fs.iter().enumerate() {
                let ty = match *f {
                    F::P(i) => types.intern(PNode::Param(i)).unwrap(),
                    F::Leaf(n) => types.intern(n).unwrap(),
                    F::RawP(i) => {
                        let p = types.intern(PNode::Param(i)).unwrap();
                        types.intern(PNode::Ptr(Mutbl::Not, p)).unwrap()
                    }
                    F::AdtP(k, i) => {
                        let p = types.intern(PNode::Param(i)).unwrap();
                        let l = types.intern_list(&[p]).unwrap();
                        types.intern(PNode::Adt(k, l)).unwrap()
                    }
                    F::Adt0(k) => types.intern(PNode::Adt(k, List::EMPTY)).unwrap(),
                };
                fields.push(Field { name: Name(fi as u64), ty });
            }
            vs.push(Variant {
                key: ItemKey(key.0 * 64 + vi as u128),
                name: Name(vi as u64),
                ctor: CtorKind::Tuple,
                fields: List { start, len: fs.len() as u32 },
            });
        }
        let discrs = vec![None; variants.len()];
        self.defs.push(Def { key, kind, generics, variants: vs, fields, types, repr, discrs, valid: ValidRange::default() });
        key
    }

    pub(crate) fn strukt(&mut self, generics: u32, fields: &[F], repr: ReprAttrs) -> ItemKey {
        self.def(AdtKind::Struct, generics, &[fields], repr)
    }

    pub(crate) fn def_mut(&mut self, key: ItemKey) -> &mut Def {
        self.defs.iter_mut().find(|d| d.key == key).unwrap()
    }

    /// `enum Option<T> { None, Some(T) }`.
    pub(crate) fn option(&mut self) -> ItemKey {
        self.def(AdtKind::Enum, 1, &[&[], &[F::P(0)]], ReprAttrs::RUST)
    }
}

/// The ground table and its interning, for building test types.
pub(crate) struct G {
    pub(crate) t: PortableTys<'static>,
}

impl G {
    pub(crate) fn new() -> G {
        G { t: PortableTys::new_in(Alloc::global()) }
    }
    pub(crate) fn n(&mut self, n: PNode) -> PTy {
        self.t.intern(n).unwrap()
    }
    /// `[elem; n]`.
    pub(crate) fn array(&mut self, elem: PTy, n: u64) -> PTy {
        self.t.array_n(elem, n).unwrap()
    }
    pub(crate) fn adt(&mut self, k: ItemKey, args: &[PTy]) -> PTy {
        let l = self.t.intern_list(args).unwrap();
        self.n(PNode::Adt(k, l))
    }
    pub(crate) fn tuple(&mut self, args: &[PTy]) -> PTy {
        let l = self.t.intern_list(args).unwrap();
        self.n(PNode::Tuple(l))
    }
    pub(crate) fn r(&mut self, t: PTy) -> PTy {
        self.n(PNode::Ref(Mutbl::Not, t))
    }
}

const U8: PNode = PNode::Uint(UintTy::U8);
const U16: PNode = PNode::Uint(UintTy::U16);
const U32: PNode = PNode::Uint(UintTy::U32);
const U64: PNode = PNode::Uint(UintTy::U64);

pub(crate) fn lay<A: core::alloc::Allocator + Clone>(
    w: &World,
    g: &mut G,
    ls: &mut Layouts<A>,
    t: PTy,
) -> Result<LayoutId, super::LayoutError> {
    let mut cx = LayoutCx { ground: &mut g.t, facts: w, scratch: Alloc::global() };
    ls.of(t, &mut cx)
}

fn size_align<A: core::alloc::Allocator + Clone>(ls: &Layouts<A>, id: LayoutId) -> (u64, u64) {
    let l = ls.get(id);
    (l.size, l.align_bytes())
}

/// `None`'s write: what Rust guarantees is all-zero bytes at the pointer, for `Option<T>` of
/// the guaranteed types (std's `option` module doc, "Representation").
fn none_is_zero_at(ls: &Layouts<Global>, opt: LayoutId, offset: u64) {
    let w = ls.tag_write(opt, 0).expect("None writes its tag");
    assert_eq!((w.offset, w.value), (offset, 0), "None is zero at the pointer");
    assert_eq!(ls.variant_of_tag(opt, 0), Some(0));
    assert_eq!(ls.variant_of_tag(opt, 0x1000), Some(1), "a non-null pointer is Some");
    assert_eq!(ls.tag_write(opt, 1), None, "Some writes no tag: its pointer is the value");
}

#[test]
fn primitives_have_their_sizes_and_valid_ranges() {
    let w = World::default();
    let mut g = G::new();
    let mut ls = Layouts::new_in(Global);
    for (n, size) in [
        (U8, 1),
        (U16, 2),
        (U32, 4),
        (U64, 8),
        (PNode::Uint(UintTy::U128), 16),
        (PNode::Int(IntTy::Isize), 8),
        (PNode::Bool, 1),
        (PNode::Char, 4),
    ] {
        let t = g.n(n);
        let id = lay(&w, &mut g, &mut ls, t).unwrap();
        assert_eq!(size_align(&ls, id), (size, size), "{n:?}: size and alignment are its width");
    }
    let b = g.n(PNode::Bool);
    let id = lay(&w, &mut g, &mut ls, b).unwrap();
    assert_eq!(ls.get(id).niche.unwrap().available(), 254, "bool: 0 and 1 are valid, 254 values are a niche");
    let never = g.n(PNode::Never);
    let id = lay(&w, &mut g, &mut ls, never).unwrap();
    assert!(ls.get(id).is_zst() && ls.get(id).uninhabited, "`!` is zero-sized and uninhabited");
}

#[test]
fn null_pointer_optimisation_holds_for_every_guaranteed_type() {
    let mut w = World::default();
    let option = w.option();
    // `core`'s shapes: `NonNull<T> { pointer: *const T }` with
    // `rustc_layout_scalar_valid_range_start(1)`; `Unique<T> { pointer: NonNull<T>,
    // _marker: PhantomData<T> }`; `Box<T, A = Global>(Unique<T>, A)`;
    // `NonZero<u32>(NonZeroU32Inner)`, the inner `struct(u32)` with the range start 1.
    let phantom = w.strukt(1, &[], ReprAttrs::RUST);
    let nonnull = w.strukt(1, &[F::RawP(0)], ReprAttrs { transparent: true, ..ReprAttrs::RUST });
    w.def_mut(nonnull).valid = ValidRange { start: Some(1), end: None };
    let unique = w.strukt(1, &[F::AdtP(nonnull, 0), F::AdtP(phantom, 0)], ReprAttrs { transparent: true, ..ReprAttrs::RUST });
    let global = w.strukt(0, &[], ReprAttrs::RUST);
    let boxed = w.strukt(1, &[F::AdtP(unique, 0), F::Adt0(global)], ReprAttrs::RUST);
    let inner = w.strukt(0, &[F::Leaf(U32)], ReprAttrs::RUST);
    w.def_mut(inner).valid = ValidRange { start: Some(1), end: None };
    let nonzero = w.strukt(0, &[F::Adt0(inner)], ReprAttrs { transparent: true, ..ReprAttrs::RUST });

    let mut g = G::new();
    let mut ls = Layouts::new_in(Global);
    let u8_ = g.n(U8);
    let u32_ = g.n(U32);
    let slice = g.n(PNode::Slice(u8_));
    let s = g.n(PNode::Str);
    let unit = g.tuple(&[]);
    let fn_args = g.t.intern_list(&[unit]).unwrap();
    let cases: Vec<(&str, PTy, u64)> = vec![
        ("&T", g.r(u8_), 8),
        ("&mut T", g.n(PNode::Ref(Mutbl::Mut, u32_)), 8),
        ("Box<T>", g.adt(boxed, &[u8_]), 8),
        ("NonNull<T>", g.adt(nonnull, &[u8_]), 8),
        ("fn()", g.n(PNode::FnPtr(fn_args, blaze_typeck::FnPtrHeader::RUST)), 8),
        ("NonZero<u32>", g.adt(nonzero, &[]), 4),
        ("&[u8]", g.r(slice), 16),
        ("&str", g.r(s), 16),
        ("NonNull<[u8]>", g.adt(nonnull, &[slice]), 16),
    ];
    for (name, t, size) in cases {
        let opt = g.adt(option, &[t]);
        let tid = lay(&w, &mut g, &mut ls, t).unwrap();
        let oid = lay(&w, &mut g, &mut ls, opt).unwrap();
        assert_eq!(ls.get(tid).size, size, "{name}: its size");
        assert_eq!(ls.get(oid).size, size, "Option<{name}> has {name}'s size (the guaranteed niche)");
        assert_eq!(ls.get(oid).align, ls.get(tid).align, "Option<{name}> has {name}'s alignment");
        none_is_zero_at(&ls, oid, 0);
    }
    // A raw pointer has no niche: `Option<*const T>` is larger.
    let raw = g.n(PNode::Ptr(Mutbl::Not, u8_));
    let opt = g.adt(option, &[raw]);
    let oid = lay(&w, &mut g, &mut ls, opt).unwrap();
    assert_eq!(ls.get(oid).size, 16, "a raw pointer may be null: Option<*const T> needs a tag");
}

/// Trait objects (codegen.md, "Trait objects: vtables and dynamic calls"; the Reference,
/// "Trait objects" and "Dynamically sized types": a pointer to a trait object is a wide pointer,
/// the data address and the vtable's): `&dyn Tr`, `&mut dyn Tr`, `*const dyn Tr` and
/// `Box<dyn Tr>` are 16 bytes aligned to 8, the vtable word at 8 a full-range address; the
/// niche of `Option<&dyn Tr>` and `Option<Box<dyn Tr>>` is the data word (the guaranteed
/// null-pointer optimisation, std's `option` "Representation"), as a thin pointer's; a raw one
/// has none. `dyn Tr + Send` lays out as `dyn Tr`. `dyn Tr` itself is unsized ([`Tail::Dyn`]):
/// no static size (`size_of_val` is the vtable's); a struct ending in one keeps its prefix's
/// end (`dyn_tails_keep_the_prefix_end_unrounded`).
#[test]
fn trait_object_pointers_are_two_words_with_the_niche_in_the_data_word() {
    let mut w = World::default();
    let option = w.option();
    let phantom = w.strukt(1, &[], ReprAttrs::RUST);
    let nonnull = w.strukt(1, &[F::RawP(0)], ReprAttrs { transparent: true, ..ReprAttrs::RUST });
    w.def_mut(nonnull).valid = ValidRange { start: Some(1), end: None };
    let unique = w.strukt(1, &[F::AdtP(nonnull, 0), F::AdtP(phantom, 0)], ReprAttrs { transparent: true, ..ReprAttrs::RUST });
    let global = w.strukt(0, &[], ReprAttrs::RUST);
    let boxed = w.strukt(1, &[F::AdtP(unique, 0), F::Adt0(global)], ReprAttrs::RUST);
    let tail = w.strukt(1, &[F::Leaf(U8), F::P(0)], ReprAttrs::RUST);
    let mut g = G::new();
    let mut ls = Layouts::new_in(Global);
    let principal = g.n(PNode::ExTrait(ItemKey(0x77), List::EMPTY));
    let send = g.n(PNode::ExAuto(ItemKey(0x78)));
    let l = g.t.intern_list(&[principal]).unwrap();
    let dyn_tr = g.n(PNode::Dynamic(l));
    let l = g.t.intern_list(&[principal, send]).unwrap();
    let dyn_send = g.n(PNode::Dynamic(l));
    let cases: Vec<(&str, PTy, bool)> = vec![
        ("&dyn Tr", g.r(dyn_tr), true),
        ("&mut dyn Tr", g.n(PNode::Ref(Mutbl::Mut, dyn_tr)), true),
        ("&(dyn Tr + Send)", g.r(dyn_send), true),
        ("Box<dyn Tr>", g.adt(boxed, &[dyn_tr]), true),
        ("*const dyn Tr", g.n(PNode::Ptr(Mutbl::Not, dyn_tr)), false),
    ];
    for (name, t, niche) in cases {
        let tid = lay(&w, &mut g, &mut ls, t).unwrap();
        assert_eq!(size_align(&ls, tid), (16, 8), "{name}: two words");
        let (addr, meta) = ls.get(tid).pair.unwrap_or_else(|| panic!("{name}: a pair"));
        assert_eq!(meta, Scalar::full(Prim::Ptr), "{name}: the vtable word is an address");
        assert_eq!(addr.prim, Prim::Ptr, "{name}: the data word is an address");
        let opt = g.adt(option, &[t]);
        let oid = lay(&w, &mut g, &mut ls, opt).unwrap();
        if niche {
            assert_eq!(ls.get(oid).size, 16, "Option<{name}> is {name}'s size");
            none_is_zero_at(&ls, oid, 0);
        } else {
            assert_eq!(ls.get(oid).size, 24, "a raw {name} may be null: Option of it needs a tag");
        }
    }
    let did = lay(&w, &mut g, &mut ls, dyn_tr).unwrap();
    assert_eq!(ls.get(did).tail, Tail::Dyn, "dyn Tr is unsized");
    assert_eq!(ls.size_of_val(did, 0), None, "dyn Tr's size is its vtable's, never the type's");
    assert_eq!(ls.dyn_prefix(did), Some(DynPrefix { tail_at: 0, align: 0 }), "a bare dyn Tr is the empty prefix");
    let ends_in_dyn = g.adt(tail, &[dyn_tr]);
    let r = g.r(ends_in_dyn);
    assert!(lay(&w, &mut g, &mut ls, r).is_ok(), "a pointer to it is still two words: it lays out no tail");
    dyn_tails_keep_the_prefix_end_unrounded(&mut w, &mut g, &mut ls, tail, dyn_tr);
}

/// "Type layout": a struct ending in a trait object (`ArcInner<dyn Tr>`) places its sized fields
/// at static offsets, and the trait object at the prefix's end rounded up to the vtable's
/// alignment, a run-time value (crate::dyn_tail). Its row keeps the end unrounded and the
/// prefix's alignment: `S<dyn Tr> { a: u8, t: dyn Tr }` ends at 1 aligned to 1; `ArcInner<dyn
/// Tr> { strong: u64, weak: u8, data: dyn Tr }` ends at 9 aligned to 8 (not 16: the rounding is
/// the vtable's). A struct whose last field is itself such a struct is a chain of two levels
/// (`dyn_chain`; its run-time size is crate::dyn_tail's, run in
/// tests/objects/nested_dyn_tail.rs); a packed one is refused by name. A wrong version that rounded the prefix's end to its own alignment would
/// give 16 for the second; one that kept the old refusal fails the first `unwrap`.
fn dyn_tails_keep_the_prefix_end_unrounded(w: &mut World, g: &mut G, ls: &mut Layouts<Global>, tail: ItemKey, dyn_tr: PTy) {
    let ends_in_dyn = g.adt(tail, &[dyn_tr]);
    let id = lay(w, g, ls, ends_in_dyn).unwrap();
    assert_eq!(ls.get(id).tail, Tail::Dyn, "S<dyn Tr> ends in a trait object");
    assert_eq!(ls.dyn_prefix(id), Some(DynPrefix { tail_at: 1, align: 0 }), "S<dyn Tr>: the u8 ends at 1");
    assert_eq!(ls.size_of_val(id, 0), None, "its size is the vtable's, read at run time");
    let arc_inner = w.strukt(1, &[F::Leaf(U64), F::Leaf(U8), F::P(0)], ReprAttrs::RUST);
    let t = g.adt(arc_inner, &[dyn_tr]);
    let id = lay(w, g, ls, t).unwrap();
    assert_eq!(ls.field_offset(id, 0), Some(0), "the u64 first");
    assert_eq!(ls.field_offset(id, 1), Some(8), "then the u8");
    assert_eq!(ls.dyn_prefix(id), Some(DynPrefix { tail_at: 9, align: 3 }), "ArcInner<dyn Tr>: ends at 9, aligned to 8");
    let outer = w.strukt(1, &[F::Leaf(U8), F::AdtP(tail, 0)], ReprAttrs::RUST);
    let t = g.adt(outer, &[dyn_tr]);
    let id = lay(w, g, ls, t).unwrap();
    let chain = ls.dyn_chain(id).expect("Outer<dyn Tr> ends in a trait object");
    assert_eq!(
        chain.levels(),
        [DynPrefix { tail_at: 1, align: 0 }, DynPrefix { tail_at: 1, align: 0 }],
        "Outer<dyn Tr>: its S at 1 (S's static alignment 1), then S's own level"
    );
    let packed = w.strukt(1, &[F::Leaf(U8), F::P(0)], ReprAttrs { packed: Some(0), ..ReprAttrs::RUST });
    let t = g.adt(packed, &[dyn_tr]);
    let refused = lay(w, g, ls, t).unwrap_err();
    assert!(matches!(refused.why, Refusal::Unsupported(what) if what.contains("packed")), "{refused:?}");
}

/// rustc's `size_and_align_of_dst` for a slice or `str` tail: `round_up(tail_at + len * stride,
/// align)`, where `tail_at` is the tail field's own offset and `align` includes the element's.
/// The row's `size` is that offset, unrounded: `P { a: u64, b: u8, t: [u8] }` keeps 9 (not 16),
/// so lengths 0, 3, 7, 8 are 16, 16, 16, 24; `Q { a: u8, t: [u32] }` has its tail at 4 and length
/// 2 is 12; a bare `[u16]` of 5 is 10. Nesting adds offsets: `Outer { a: u8, p: P<[u8]> }` has
/// `p` at 8 and its tail at 17, so length 3 is 24; a `repr(transparent)` wrapper of `P` keeps
/// 9, so length 3 is 16 (the last field's offset, 0, would give 8). A version that rounded the
/// prefix first gives 24 for `P` of 3. Prints nothing; a failure names the case.
#[test]
fn slice_tails_round_once_from_the_tails_offset() {
    let mut w = World::default();
    let s3 = w.strukt(1, &[F::Leaf(U64), F::Leaf(U8), F::P(0)], ReprAttrs::RUST);
    let s2 = w.strukt(1, &[F::Leaf(U8), F::P(0)], ReprAttrs::RUST);
    let outer = w.strukt(1, &[F::Leaf(U8), F::AdtP(s3, 0)], ReprAttrs::RUST);
    let wrap = w.strukt(1, &[F::AdtP(s3, 0)], ReprAttrs { transparent: true, ..ReprAttrs::RUST });
    let mut g = G::new();
    let mut ls = Layouts::new_in(Global);
    let (u8_, u16_, u32_) = (g.n(U8), g.n(U16), g.n(U32));
    let (bytes, halves, words) = (g.n(PNode::Slice(u8_)), g.n(PNode::Slice(u16_)), g.n(PNode::Slice(u32_)));
    let p = g.adt(s3, &[bytes]);
    let pid = lay(&w, &mut g, &mut ls, p).unwrap();
    assert_eq!((ls.field_offset(pid, 2), ls.get(pid).size, ls.get(pid).align_bytes()), (Some(9), 9, 8), "P: the tail at 9, unrounded, aligned to 8");
    for (len, want) in [(0, 16), (3, 16), (7, 16), (8, 24)] {
        assert_eq!(ls.size_of_val(pid, len), Some(want), "P of length {len}");
    }
    let q = g.adt(s2, &[words]);
    let qid = lay(&w, &mut g, &mut ls, q).unwrap();
    assert_eq!(ls.field_offset(qid, 1), Some(4), "Q: the [u32] at 4");
    assert_eq!(ls.size_of_val(qid, 2), Some(12), "Q of length 2");
    let hid = lay(&w, &mut g, &mut ls, halves).unwrap();
    assert_eq!(ls.size_of_val(hid, 5), Some(10), "[u16] of length 5");
    let o = g.adt(outer, &[bytes]);
    let oid = lay(&w, &mut g, &mut ls, o).unwrap();
    assert_eq!((ls.field_offset(oid, 1), ls.get(oid).size), (Some(8), 17), "Outer: p at 8, its tail at 17");
    assert_eq!(ls.size_of_val(oid, 3), Some(24), "Outer of length 3");
    let t = g.adt(wrap, &[bytes]);
    let tid = lay(&w, &mut g, &mut ls, t).unwrap();
    assert_eq!(ls.size_of_val(tid, 3), Some(16), "transparent P of length 3");
}

/// Pattern types (typeck.md, "Pattern types") narrow their base's valid range, so an enclosing
/// enum finds the niche: core's `NonZero<u32>` holds `u32 is 1..=u32::MAX`, `NonNull<u8>`
/// `*const u8 is !null` (the address `1..=usize::MAX`), and `Option` of either is its size.
/// A signed bound sign-extended past the width (`i8 is -1..=-1`) is kept to the width, and a
/// base with its own valid range (`bool is 1..=1`) is refused rather than widened.
#[test]
fn pattern_types_give_their_range_as_a_niche() {
    let mut w = World::default();
    let option = w.option();
    let mut g = G::new();
    let mut ls = Layouts::new_in(Global);
    let range = |g: &mut G, base: PNode, start: u128, end: u128| {
        let b = g.n(base);
        let s = g.n(PNode::ConstValue { ty: b, bits: start });
        let e = g.n(PNode::ConstValue { ty: b, bits: end });
        let l = g.t.intern_list(&[s, e]).unwrap();
        g.n(PNode::Pat(b, l))
    };
    let nz = range(&mut g, U32, 1, u128::from(u32::MAX));
    let o = g.adt(option, &[nz]);
    let oid = lay(&w, &mut g, &mut ls, o).unwrap();
    assert_eq!(ls.get(oid).size, 4, "Option<u32 is 1..> is four bytes");
    none_is_zero_at(&ls, oid, 0);
    let u8_ = g.n(U8);
    let nn = range(&mut g, PNode::Ptr(Mutbl::Not, u8_), 1, u128::from(u64::MAX));
    let o = g.adt(option, &[nn]);
    let oid = lay(&w, &mut g, &mut ls, o).unwrap();
    assert_eq!(ls.get(oid).size, 8, "Option<*const u8 is !null> is one pointer");
    none_is_zero_at(&ls, oid, 0);
    // `i8 is -1..=-1` with its bound sign-extended to 128 bits: kept to 8 bits, one valid value.
    let minus_one = range(&mut g, PNode::Int(blaze_typeck::IntTy::I8), u128::MAX, u128::MAX);
    let id = lay(&w, &mut g, &mut ls, minus_one).unwrap();
    let s = ls.get(id).scalar.unwrap();
    assert_eq!((s.valid.start, s.valid.end), (0xff, 0xff));
    let b = range(&mut g, PNode::Bool, 1, 1);
    assert!(lay(&w, &mut g, &mut ls, b).is_ok(), "a bool base takes the pattern's range");
}

#[test]
fn niches_of_bool_and_char_and_nesting() {
    let mut w = World::default();
    let option = w.option();
    let mut g = G::new();
    let mut ls = Layouts::new_in(Global);
    let b = g.n(PNode::Bool);
    let ob = g.adt(option, &[b]);
    let id = lay(&w, &mut g, &mut ls, ob).unwrap();
    assert_eq!(ls.get(id).size, 1);
    assert_eq!(ls.tag_write(id, 0).unwrap().value, 2, "Option<bool>::None is the first value past 1");
    let oob = g.adt(option, &[ob]);
    let id2 = lay(&w, &mut g, &mut ls, oob).unwrap();
    assert_eq!(ls.get(id2).size, 1, "Option<Option<bool>> still fits a byte");
    // rustc's `Niche::reserve` for one value: the side nearer zero as a signed number
    // (compiler/rustc_abi, `count == 1`): below 0 is -1, nearer than 3.
    assert_eq!(ls.tag_write(id2, 0).unwrap().value, 0xff);
    assert_eq!(ls.variant_of_tag(id2, 2), Some(1), "2 is Some(None)");
    let c = g.n(PNode::Char);
    let oc = g.adt(option, &[c]);
    let id = lay(&w, &mut g, &mut ls, oc).unwrap();
    assert_eq!(ls.get(id).size, 4);
    assert_eq!(ls.tag_write(id, 0).unwrap().value, 0xFFFF_FFFF, "Option<char>::None is rustc's -1 (next down is nearer zero)");
}

#[test]
fn unsafe_cell_hides_its_niche() {
    let mut w = World::default();
    let option = w.option();
    let cell = w.strukt(1, &[F::P(0)], ReprAttrs { transparent: true, hide_niche: true, ..ReprAttrs::RUST });
    let mut g = G::new();
    let mut ls = Layouts::new_in(Global);
    let u8_ = g.n(U8);
    let r = g.r(u8_);
    let c = g.adt(cell, &[r]);
    let oc = g.adt(option, &[c]);
    let id = lay(&w, &mut g, &mut ls, oc).unwrap();
    assert_eq!(ls.get(id).size, 16, "Option<UnsafeCell<&u8>> uses no niche of its field");
}

#[test]
fn repr_rust_orders_by_decreasing_alignment_and_repr_c_by_declaration() {
    let mut w = World::default();
    let fs = [F::Leaf(U8), F::Leaf(U32), F::Leaf(U16)];
    let rust = w.strukt(0, &fs, ReprAttrs::RUST);
    let c = w.strukt(0, &fs, ReprAttrs { c: true, ..ReprAttrs::RUST });
    let packed = w.strukt(0, &fs, ReprAttrs { packed: Some(0), ..ReprAttrs::RUST });
    let packed2 = w.strukt(0, &fs, ReprAttrs { packed: Some(1), ..ReprAttrs::RUST });
    let aligned = w.strukt(0, &fs, ReprAttrs { align: Some(4), ..ReprAttrs::RUST });
    let mut g = G::new();
    let mut ls = Layouts::new_in(Global);
    let offs = |ls: &Layouts<Global>, id| [0, 1, 2].map(|f| ls.field_offset(id, f).unwrap());

    let t = g.adt(rust, &[]);
    let id = lay(&w, &mut g, &mut ls, t).unwrap();
    assert_eq!((offs(&ls, id), size_align(&ls, id)), ([6, 0, 4], (8, 4)), "repr(Rust): u32, u16, u8");
    let t = g.adt(c, &[]);
    let id = lay(&w, &mut g, &mut ls, t).unwrap();
    assert_eq!((offs(&ls, id), size_align(&ls, id)), ([0, 4, 8], (12, 4)), "repr(C): declaration order, padded");
    let t = g.adt(packed, &[]);
    let id = lay(&w, &mut g, &mut ls, t).unwrap();
    assert_eq!((offs(&ls, id), size_align(&ls, id)), ([0, 1, 5], (7, 1)), "repr(packed): no padding");
    let t = g.adt(packed2, &[]);
    let id = lay(&w, &mut g, &mut ls, t).unwrap();
    assert_eq!((offs(&ls, id), size_align(&ls, id)), ([0, 2, 6], (8, 2)), "repr(packed(2)): alignment capped at 2");
    let t = g.adt(aligned, &[]);
    let id = lay(&w, &mut g, &mut ls, t).unwrap();
    assert_eq!(size_align(&ls, id), (16, 16), "repr(align(16)) raises the alignment and rounds the size");
}

#[test]
fn unions_arrays_tuples_and_zero_sized_types() {
    let mut w = World::default();
    let un = w.def(AdtKind::Union, 0, &[&[F::Leaf(U8), F::Leaf(U64)]], ReprAttrs::RUST);
    let unit = w.strukt(0, &[], ReprAttrs::RUST);
    let mut g = G::new();
    let mut ls = Layouts::new_in(Global);
    let t = g.adt(un, &[]);
    let id = lay(&w, &mut g, &mut ls, t).unwrap();
    assert_eq!((size_align(&ls, id), ls.field_offset(id, 1)), ((8, 8), Some(0)), "a union: fields at 0, the largest size");
    assert!(ls.get(id).niche.is_none(), "a union has no niche");
    let u16_ = g.n(U16);
    let arr = g.array(u16_, 3);
    let id = lay(&w, &mut g, &mut ls, arr).unwrap();
    assert_eq!((size_align(&ls, id), ls.field_offset(id, 2)), ((6, 2), Some(4)), "[u16; 3]: stride 2");
    let empty = g.array(u16_, 0);
    let id = lay(&w, &mut g, &mut ls, empty).unwrap();
    assert_eq!(size_align(&ls, id), (0, 2), "[u16; 0] is zero-sized with u16's alignment");
    let u = g.adt(unit, &[]);
    let t0 = g.tuple(&[]);
    for t in [u, t0] {
        let id = lay(&w, &mut g, &mut ls, t).unwrap();
        assert!(ls.get(id).is_zst(), "a unit struct and () are zero-sized");
    }
    let u8_ = g.n(U8);
    let u64_ = g.n(U64);
    let tup = g.tuple(&[u8_, u64_, u8_]);
    let id = lay(&w, &mut g, &mut ls, tup).unwrap();
    assert_eq!(size_align(&ls, id), (16, 8), "(u8, u64, u8): reordered, 16 bytes");
    let slice = g.n(PNode::Slice(u64_));
    let id = lay(&w, &mut g, &mut ls, slice).unwrap();
    assert_eq!(ls.get(id).tail, Tail::Slice { stride: 8 });
    assert_eq!(ls.size_of_val(id, 3), Some(24), "size_of_val of [u64] of length 3");
}

#[test]
fn enum_tags_follow_their_rules() {
    let mut w = World::default();
    let option = w.option();
    let three = w.def(AdtKind::Enum, 0, &[&[], &[], &[]], ReprAttrs::RUST);
    let ordering = w.def(AdtKind::Enum, 0, &[&[], &[], &[]], ReprAttrs { int: Some(IntRepr::Signed(IntTy::I8)), ..ReprAttrs::RUST });
    w.def_mut(ordering).discrs = vec![Some(-1), Some(0), Some(1)];
    let c_less = w.def(AdtKind::Enum, 0, &[&[], &[]], ReprAttrs { c: true, ..ReprAttrs::RUST });
    let u8_fields = w.def(AdtKind::Enum, 0, &[&[F::Leaf(U32)], &[F::Leaf(U8)]], ReprAttrs { int: Some(IntRepr::Unsigned(UintTy::U8)), ..ReprAttrs::RUST });
    let c_fields = w.def(AdtKind::Enum, 0, &[&[F::Leaf(U8)], &[F::Leaf(U64)]], ReprAttrs { c: true, ..ReprAttrs::RUST });
    let rust_fields = w.def(AdtKind::Enum, 0, &[&[F::Leaf(U8), F::Leaf(U64)], &[F::Leaf(U16)]], ReprAttrs::RUST);
    let result_never = w.def(AdtKind::Enum, 0, &[&[F::Leaf(U64)], &[F::Leaf(PNode::Never)]], ReprAttrs::RUST);
    let mut g = G::new();
    let mut ls = Layouts::new_in(Global);

    let t = g.adt(three, &[]);
    let id = lay(&w, &mut g, &mut ls, t).unwrap();
    assert_eq!(ls.get(id).size, 1, "a field-less enum of 3 variants: a u8 tag");
    assert_eq!(ls.tag_write(id, 2).unwrap().value, 2);
    let t = g.adt(ordering, &[]);
    let id = lay(&w, &mut g, &mut ls, t).unwrap();
    assert_eq!(ls.tag_write(id, 0).unwrap().value, 0xff, "repr(i8) -1 is 0xff");
    assert_eq!(ls.discriminant(id, 0), Some(-1));
    let o = g.adt(option, &[t]);
    let oid = lay(&w, &mut g, &mut ls, o).unwrap();
    assert_eq!(ls.get(oid).size, 1, "Option<Ordering> uses the tag's niche");
    assert_eq!(ls.tag_write(oid, 0).unwrap().value, 0xfe, "None is -2, the tie below");
    let t = g.adt(c_less, &[]);
    let id = lay(&w, &mut g, &mut ls, t).unwrap();
    assert_eq!(size_align(&ls, id), (4, 4), "a field-less repr(C) enum is C's int");
    let t = g.adt(u8_fields, &[]);
    let id = lay(&w, &mut g, &mut ls, t).unwrap();
    let a = ls.variant(id, 0).unwrap();
    assert_eq!((size_align(&ls, id), ls.field_offset(a, 0)), ((8, 4), Some(4)), "repr(u8): each variant a repr(C) struct after the tag");
    let t = g.adt(c_fields, &[]);
    let id = lay(&w, &mut g, &mut ls, t).unwrap();
    let a = ls.variant(id, 0).unwrap();
    assert_eq!((size_align(&ls, id), ls.field_offset(a, 0)), ((16, 8), Some(8)), "repr(C): tag, then the union at its alignment");
    let t = g.adt(rust_fields, &[]);
    let id = lay(&w, &mut g, &mut ls, t).unwrap();
    let a = ls.variant(id, 0).unwrap();
    assert_eq!(
        (size_align(&ls, id), ls.field_offset(a, 0), ls.field_offset(a, 1)),
        ((16, 8), Some(1), Some(8)),
        "repr(Rust) tagged: after the tag, increasing alignment"
    );
    let t = g.adt(result_never, &[]);
    let id = lay(&w, &mut g, &mut ls, t).unwrap();
    assert_eq!((ls.get(id).size, ls.get(id).variants), (8, Variants::Single { index: 0 }), "an absent variant takes no tag");
}

#[test]
fn a_type_that_holds_itself_by_value_is_refused() {
    let mut w = World::default();
    let key = ItemKey(0x1000);
    let s = w.strukt(0, &[F::Leaf(U8), F::Adt0(key)], ReprAttrs::RUST);
    assert_eq!(s, key);
    let by_ref = w.def(AdtKind::Struct, 0, &[&[F::Leaf(U8)]], ReprAttrs::RUST);
    let mut g = G::new();
    let mut ls = Layouts::new_in(Global);
    let t = g.adt(s, &[]);
    let error = lay(&w, &mut g, &mut ls, t).unwrap_err();
    assert!(matches!(error.why, Refusal::Recursive), "{error}");
    let t = g.adt(by_ref, &[]);
    assert!(lay(&w, &mut g, &mut ls, t).is_ok());
}

/// A random ground type from a pool of shapes, and the checks every layout must pass.
fn random_types(tape: &Tape) -> Result<(), Broke> {
    let mut w = World::default();
    let option = w.option();
    let reprs = [
        ReprAttrs::RUST,
        ReprAttrs { c: true, ..ReprAttrs::RUST },
        ReprAttrs { packed: Some(0), ..ReprAttrs::RUST },
        ReprAttrs { packed: Some(1), ..ReprAttrs::RUST },
        ReprAttrs { align: Some(4), ..ReprAttrs::RUST },
    ];
    // Structs of 1..=3 parameter fields in every repr; enums of 2..=3 variants, each with up to
    // two parameter fields; a union of two.
    let mut structs = Vec::new();
    for n in 1..=3u32 {
        let fs: Vec<F> = (0..n).map(F::P).collect();
        for r in reprs {
            structs.push((w.strukt(n, &fs, r), n));
        }
    }
    let mut enums = Vec::new();
    for r in [ReprAttrs::RUST, ReprAttrs { c: true, ..ReprAttrs::RUST }, ReprAttrs { int: Some(IntRepr::Unsigned(UintTy::U16)), ..ReprAttrs::RUST }] {
        enums.push((w.def(AdtKind::Enum, 3, &[&[F::P(0)], &[], &[F::P(1), F::P(2)]], r), 3));
        enums.push((w.def(AdtKind::Enum, 2, &[&[F::P(0), F::P(1)], &[F::P(1)]], r), 2));
    }
    let union = w.def(AdtKind::Union, 2, &[&[F::P(0), F::P(1)]], ReprAttrs::RUST);
    let mut g = G::new();
    let leaves = [U8, U16, U32, U64, PNode::Bool, PNode::Char, PNode::Uint(UintTy::U128)];
    let mut pool: Vec<PTy> = leaves.iter().map(|&n| g.n(n)).collect();
    let steps = tape.int_in(1..=12usize, "steps");
    for _ in 0..steps {
        let pick = |tape: &Tape, pool: &Vec<PTy>| pool[tape.index(pool.len(), "part")];
        let t = match tape.index(7, "shape") {
            0 => {
                let n = tape.int_in(0..=3usize, "tuple len");
                let parts: Vec<PTy> = (0..n).map(|_| pick(tape, &pool)).collect();
                g.tuple(&parts)
            }
            1 => {
                let e = pick(tape, &pool);
                g.array(e, tape.int_in(0..=3u64, "array len"))
            }
            2 => {
                let e = pick(tape, &pool);
                g.r(e)
            }
            3 => {
                let (k, n) = structs[tape.index(structs.len(), "struct")];
                let args: Vec<PTy> = (0..n).map(|_| pick(tape, &pool)).collect();
                g.adt(k, &args)
            }
            4 => {
                let (k, n) = enums[tape.index(enums.len(), "enum")];
                let args: Vec<PTy> = (0..n).map(|_| pick(tape, &pool)).collect();
                g.adt(k, &args)
            }
            5 => {
                let (a, b) = (pick(tape, &pool), pick(tape, &pool));
                g.adt(union, &[a, b])
            }
            _ => {
                let e = pick(tape, &pool);
                g.adt(option, &[e])
            }
        };
        pool.push(t);
    }
    let mut ls = Layouts::new_in(Global);
    let oracle = |s: String| Broke::Oracle(s);
    for &t in pool.iter() {
        let id = lay(&w, &mut g, &mut ls, t).map_err(|e| oracle(format!("{e}")))?;
        let l = *ls.get(id);
        if l.size % l.align_bytes() != 0 {
            return Err(oracle(format!("type {t:?}: size {} is not a multiple of alignment {}", l.size, l.align_bytes())));
        }
        // The fields of each variant: inside the size, aligned, and never overlapping each
        // other or the tag.
        let node = g.t.view().node(t).unwrap();
        let (parts, packed, adt_kind): (Vec<PTy>, Option<u8>, Option<AdtKind>) = match node {
            PNode::Tuple(lst) => (g.t.view().list(lst).unwrap().to_vec(), None, None),
            PNode::Adt(k, lst) => {
                let args = g.t.view().list(lst).unwrap().to_vec();
                let adt = w.adt(k).unwrap();
                let mut ps = Vec::new();
                for f in adt.fields {
                    ps.push(crate::ground(adt.types, f.ty, &args, &mut g.t, Alloc::global(), &w).unwrap());
                }
                (ps, w.repr(k).expect("every head of the random world is made").packed, Some(adt.kind))
            }
            _ => continue,
        };
        if adt_kind == Some(AdtKind::Union) {
            continue;
        }
        let variant_fields: Vec<(LayoutId, Vec<usize>)> = match (node, l.variants) {
            (PNode::Adt(k, _), Variants::Multiple { count, .. }) => {
                let adt = w.adt(k).unwrap();
                (0..count).map(|v| (ls.variant(id, v).unwrap(), adt.variants[v as usize].fields.range().collect())).collect()
            }
            (PNode::Adt(k, _), Variants::Single { index }) if adt_kind == Some(AdtKind::Enum) => {
                let adt = w.adt(k).unwrap();
                if adt.variants.is_empty() {
                    continue;
                }
                vec![(id, adt.variants[index as usize].fields.range().collect())]
            }
            _ => vec![(id, (0..parts.len()).collect())],
        };
        let tag = ls.read_tag(id).map(|(o, s)| (o, o + s.prim.size()));
        for (row, fields) in variant_fields {
            let mut spans = Vec::new();
            if let Some(tag) = tag
                && adt::kind(&l) == adt::Kind::Direct
            {
                spans.push(tag);
            }
            for (i, &f) in fields.iter().enumerate() {
                let fid = lay(&w, &mut g, &mut ls, parts[f]).unwrap();
                let fl = *ls.get(fid);
                let off = ls.field_offset(row, i as u32).ok_or_else(|| oracle(format!("type {t:?}: no offset for field {i}")))?;
                let want = packed.map_or(fl.align, |p| fl.align.min(p));
                if off % (1u64 << want) != 0 {
                    return Err(oracle(format!("type {t:?}: field {i} at {off} breaks its alignment {}", 1u64 << want)));
                }
                if off + fl.size > l.size {
                    return Err(oracle(format!("type {t:?}: field {i} at {off} of size {} is past the size {}", fl.size, l.size)));
                }
                if fl.size > 0 {
                    spans.push((off, off + fl.size));
                }
            }
            spans.sort();
            for pair in spans.windows(2) {
                if pair[0].1 > pair[1].0 {
                    return Err(oracle(format!("type {t:?}: fields overlap: {:?} and {:?}", pair[0], pair[1])));
                }
            }
        }
        // A niche-carrying T: Option<T> has T's size, and every variant round-trips its tag.
        let o = g.adt(option, &[t]);
        let oid = lay(&w, &mut g, &mut ls, o).map_err(|e| oracle(format!("{e}")))?;
        if l.niche.is_some() && !l.uninhabited && ls.get(oid).size != l.size {
            return Err(oracle(format!("type {t:?} has a niche but Option of it is {} bytes, not {}", ls.get(oid).size, l.size)));
        }
        for v in 0..2 {
            if let Some(write) = ls.tag_write(oid, v) {
                let back = ls.variant_of_tag(oid, write.value);
                if back != Some(v) {
                    return Err(oracle(format!("Option<{t:?}>: variant {v} writes {} which reads as {back:?}", write.value)));
                }
            }
        }
    }
    Ok(())
}

/// Whether a tagged layout's tag occupies bytes no field of a variant may share (a direct
/// tag), or lives inside the untagged variant's own field (a niche).
mod field_ground;

mod adt {
    use super::super::{Layout, TagEncoding, Variants};

    #[derive(PartialEq, Eq)]
    pub enum Kind {
        Direct,
        NicheOrNone,
    }

    pub fn kind(l: &Layout) -> Kind {
        match l.variants {
            Variants::Multiple { encoding: TagEncoding::Direct, .. } => Kind::Direct,
            _ => Kind::NicheOrNone,
        }
    }
}

#[test]
fn every_layout_keeps_fields_apart_aligned_and_sized() {
    blaze_sim::check(Config::cases(400), random_types);
}

/// A discriminant whose record is not made refuses with `Missing` and caches nothing: the same
/// `Layouts` asked again once it exists lays the enum (and the struct holding it) out with it.
#[test]
fn a_missing_discriminant_caches_nothing() {
    let mut w = World::default();
    let e = w.def(AdtKind::Enum, 0, &[&[], &[]], ReprAttrs::RUST);
    w.def_mut(e).discrs = vec![Some(300), None];
    let mut g = G::new();
    let et = g.adt(e, &[]);
    let holder = w.def(AdtKind::Struct, 1, &[&[F::P(0)]], ReprAttrs::RUST);
    let ht = g.adt(holder, &[et]);
    let mut ls = Layouts::new_in(Global);
    w.missing.set(Some(e));
    let err = lay(&w, &mut g, &mut ls, ht).unwrap_err();
    assert!(matches!(err.why, super::Refusal::Missing(super::Missing { item }) if item == e), "{err}");
    w.missing.set(None);
    let id = lay(&w, &mut g, &mut ls, ht).expect("laid out once the record exists");
    assert_eq!(size_align(&ls, id), (2, 2), "300 and 301 need a 2-byte tag");
    let eid = lay(&w, &mut g, &mut ls, et).unwrap();
    assert_eq!(ls.discriminant(eid, 1), Some(301));
}

/// D194: a `repr` asked before the ADT's head is made (`Missing`) refuses as an unanswered head
/// (`Refusal::NoAdt`, which every driver makes and asks again) and caches nothing, so the same `Layouts` asked again once the head exists lays the packed struct (and
/// the struct holding it) out packed: the answer does not depend on which was asked first.
/// The planted control answers the unmade head with a default `repr(Rust)` (the old reading):
/// the holder is then laid out padded, and the order would change the layout.
#[test]
fn a_repr_asked_before_its_head_caches_nothing() {
    let mut w = World::default();
    let packed = w.strukt(0, &[F::Leaf(U8), F::Leaf(U32)], ReprAttrs { packed: Some(0), ..ReprAttrs::RUST });
    let mut g = G::new();
    let pt = g.adt(packed, &[]);
    let holder = w.strukt(1, &[F::P(0)], ReprAttrs::RUST);
    let ht = g.adt(holder, &[pt]);
    let heap = blaze_own::testing::heap();

    // Asked first, before the head: refused by name, nothing kept.
    let mut ls = Layouts::new_in(heap.alloc());
    w.unmade.set(Some(packed));
    let mut cx = LayoutCx { ground: &mut g.t, facts: &w, scratch: heap.alloc() };
    let err = ls.of(ht, &mut cx).unwrap_err();
    assert!(matches!(err.why, Refusal::NoAdt(item) if item == packed), "{err}");
    w.unmade.set(None);
    let mut cx = LayoutCx { ground: &mut g.t, facts: &w, scratch: heap.alloc() };
    let id = ls.of(ht, &mut cx).expect("laid out once the head exists");
    let asked_first = size_align(&ls, id);
    assert_eq!(asked_first, (5, 1), "repr(packed) u8, u32: no padding, held unpadded");

    // Asked after the head: the same answer.
    let mut fresh = Layouts::new_in(heap.alloc());
    let mut cx = LayoutCx { ground: &mut g.t, facts: &w, scratch: heap.alloc() };
    let id = fresh.of(ht, &mut cx).unwrap();
    assert_eq!(size_align(&fresh, id), asked_first, "the order of asking changes nothing");

    // Planted: the unmade head read as `repr(Rust)` lays the holder out padded.
    w.unmade.set(Some(packed));
    let planted = DefaultsRepr(&w);
    let mut wrong = Layouts::new_in(heap.alloc());
    let mut cx = LayoutCx { ground: &mut g.t, facts: &planted, scratch: heap.alloc() };
    let id = wrong.of(ht, &mut cx).expect("the planted reading answers");
    assert_eq!(size_align(&wrong, id), (8, 4), "the planted default is caught: a padded layout");
}

#[test]
fn niche_encoding_decodes_every_value() {
    // The untagged variant's valid values decode to it; the reserved ones to their variant.
    let mut w = World::default();
    let five = w.def(AdtKind::Enum, 0, &[&[], &[F::Leaf(PNode::Bool)], &[], &[]], ReprAttrs::RUST);
    let mut g = G::new();
    let mut ls = Layouts::new_in(Global);
    let t = g.adt(five, &[]);
    let id = lay(&w, &mut g, &mut ls, t).unwrap();
    let Variants::Multiple { encoding: TagEncoding::Niche { untagged, .. }, .. } = ls.get(id).variants else {
        panic!("expected a niche layout, got {:?}", ls.get(id).variants)
    };
    assert_eq!((untagged, ls.get(id).size), (1, 1));
    assert_eq!(ls.variant_of_tag(id, 0), Some(1));
    assert_eq!(ls.variant_of_tag(id, 1), Some(1));
    for v in [0, 2, 3] {
        let wr = ls.tag_write(id, v).unwrap();
        assert_eq!(ls.variant_of_tag(id, wr.value), Some(v));
    }
}

#[test]
fn record_and_union_layouts() {
    use crate::abi::Pass;
    use crate::intrinsics::validity::{Check, permits};
    use blaze_lir::{Ty, func::Ext};

    let w = World::default();
    let mut g = G::new();
    let h = blaze_own::testing::heap();
    let mut ls = Layouts::new_in(h.alloc());
    let u64_t = g.n(PNode::Uint(UintTy::U64));
    let u8_t = g.n(U8);
    let bool_t = g.n(PNode::Bool);
    let char_t = g.n(PNode::Char);
    let unit_t = g.tuple(&[]);
    let never_t = g.n(PNode::Never);
    let ref_u64_t = g.r(u64_t);
    let ref_u8_t = g.r(u8_t);

    // PNode::Record([u64, u8]) -> size 16, align 8
    let rec_list = g.t.intern_list(&[u64_t, u8_t]).unwrap();
    let rec_ty = g.n(PNode::Record(blaze_typeck::RecordSchema::EMPTY, rec_list));
    let rec_id = lay(&w, &mut g, &mut ls, rec_ty).unwrap();
    assert_eq!(size_align(&ls, rec_id), (16, 8));
    assert!(!ls.get(rec_id).uninhabited);

    // PNode::Union([&u64, ()]) -> size 8, align 8 (pointer niche optimization)
    let u_ref_unit_list = g.t.intern_list(&[ref_u64_t, unit_t]).unwrap();
    let u_ref_unit_ty = g.n(PNode::Union(u_ref_unit_list));
    let u_ref_unit_id = lay(&w, &mut g, &mut ls, u_ref_unit_ty).unwrap();
    assert_eq!(size_align(&ls, u_ref_unit_id), (8, 8));
    assert!(!ls.get(u_ref_unit_id).uninhabited);
    assert!(ls.get(u_ref_unit_id).scalar.is_some());
    assert_eq!(
        ls.get(u_ref_unit_id).rust_pass(g.t.view()).unwrap(),
        Pass::Scalar { ty: Ty::PTR, ext: Ext::None, agg: true }
    );
    assert!(matches!(
        ls.get(u_ref_unit_id).variants,
        Variants::Multiple { tag_offset: 0, encoding: TagEncoding::Niche { untagged: 0, first: 1, last: 1, start: 0 }, .. }
    ));
    assert_eq!(ls.tag_write(u_ref_unit_id, 0), None);
    let unit_write = ls.tag_write(u_ref_unit_id, 1).unwrap();
    assert_eq!((unit_write.offset, unit_write.value), (0, 0));
    assert_eq!(ls.variant_of_tag(u_ref_unit_id, 0), Some(1));
    assert_eq!(ls.variant_of_tag(u_ref_unit_id, 0x1000), Some(0));

    // PNode::Union([bool, ()]) -> size 1, align 1 (bool niche optimization)
    let u_bool_unit_list = g.t.intern_list(&[bool_t, unit_t]).unwrap();
    let u_bool_unit_ty = g.n(PNode::Union(u_bool_unit_list));
    let u_bool_unit_id = lay(&w, &mut g, &mut ls, u_bool_unit_ty).unwrap();
    assert_eq!(size_align(&ls, u_bool_unit_id), (1, 1));
    assert!(!ls.get(u_bool_unit_id).uninhabited);
    assert_eq!(ls.tag_write(u_bool_unit_id, 0), None);
    let bool_unit_write = ls.tag_write(u_bool_unit_id, 1).unwrap();
    assert_eq!((bool_unit_write.offset, bool_unit_write.value), (0, 2));
    assert_eq!(ls.variant_of_tag(u_bool_unit_id, 0), Some(0));
    assert_eq!(ls.variant_of_tag(u_bool_unit_id, 1), Some(0));
    assert_eq!(ls.variant_of_tag(u_bool_unit_id, 2), Some(1));

    // PNode::Union([char, ()]) -> size 4, align 4 (char niche optimization)
    let u_char_unit_list = g.t.intern_list(&[char_t, unit_t]).unwrap();
    let u_char_unit_ty = g.n(PNode::Union(u_char_unit_list));
    let u_char_unit_id = lay(&w, &mut g, &mut ls, u_char_unit_ty).unwrap();
    assert_eq!(size_align(&ls, u_char_unit_id), (4, 4));
    assert!(!ls.get(u_char_unit_id).uninhabited);
    assert_eq!(ls.tag_write(u_char_unit_id, 0), None);
    let char_unit_write = ls.tag_write(u_char_unit_id, 1).unwrap();
    assert_eq!(char_unit_write.offset, 0);
    assert_eq!(ls.variant_of_tag(u_char_unit_id, char_unit_write.value), Some(1));
    assert_eq!(ls.variant_of_tag(u_char_unit_id, u128::from(u32::from('a'))), Some(0));

    // PNode::Union([u64, !]) -> size 8, align 8 (uninhabited member collapsed)
    let u_64_never_list = g.t.intern_list(&[u64_t, never_t]).unwrap();
    let u_64_never_ty = g.n(PNode::Union(u_64_never_list));
    let u_64_never_id = lay(&w, &mut g, &mut ls, u_64_never_ty).unwrap();
    assert_eq!(size_align(&ls, u_64_never_id), (8, 8));
    assert!(!ls.get(u_64_never_id).uninhabited);
    assert_eq!(ls.get(u_64_never_id).variants, Variants::Single { index: 0 });
    assert_eq!(ls.tag_write(u_64_never_id, 0), None);
    assert_eq!(ls.variant_of_tag(u_64_never_id, 0), Some(0));

    // PNode::Union([u64, u8]) -> size 16, align 8 (direct tagged, round-trip both variants)
    let u_64_8_list = g.t.intern_list(&[u64_t, u8_t]).unwrap();
    let u_64_8_ty = g.n(PNode::Union(u_64_8_list));
    let u_64_8_id = lay(&w, &mut g, &mut ls, u_64_8_ty).unwrap();
    assert_eq!(size_align(&ls, u_64_8_id), (16, 8));
    assert!(!ls.get(u_64_8_id).uninhabited);
    for v in 0..2u32 {
        let wr = ls.tag_write(u_64_8_id, v).unwrap();
        assert_eq!(ls.variant_of_tag(u_64_8_id, wr.value), Some(v));
    }

    // PNode::Union([u8, bool]) -> size 2, align 1 (direct tagged, round-trip both variants)
    let u_8_bool_list = g.t.intern_list(&[u8_t, bool_t]).unwrap();
    let u_8_bool_ty = g.n(PNode::Union(u_8_bool_list));
    let u_8_bool_id = lay(&w, &mut g, &mut ls, u_8_bool_ty).unwrap();
    assert_eq!(size_align(&ls, u_8_bool_id), (2, 1));
    assert!(!ls.get(u_8_bool_id).uninhabited);
    for v in 0..2u32 {
        let wr = ls.tag_write(u_8_bool_id, v).unwrap();
        assert_eq!(ls.variant_of_tag(u_8_bool_id, wr.value), Some(v));
    }

    // PNode::Union([u64]) -> size 8, align 8 (transparent)
    let u_64_list = g.t.intern_list(&[u64_t]).unwrap();
    let u_64_ty = g.n(PNode::Union(u_64_list));
    let u_64_id = lay(&w, &mut g, &mut ls, u_64_ty).unwrap();
    assert_eq!(size_align(&ls, u_64_id), (8, 8));
    assert!(!ls.get(u_64_id).uninhabited);
    assert_eq!(ls.get(u_64_id).variants, Variants::Single { index: 0 });
    assert_eq!(ls.field_offset(u_64_id, 0), Some(0));

    // PNode::Union([]) -> size 0, uninhabited
    let u_empty_ty = g.n(PNode::Union(List::EMPTY));
    let u_empty_id = lay(&w, &mut g, &mut ls, u_empty_ty).unwrap();
    assert_eq!(ls.get(u_empty_id).size, 0);
    assert!(ls.get(u_empty_id).uninhabited);

    // Zero-validity and inhabitedness checks on niche-tagged vs non-zero-valid unions
    let u_refs_list = g.t.intern_list(&[ref_u64_t, ref_u8_t]).unwrap();
    let u_refs_ty = g.n(PNode::Union(u_refs_list));
    let u_nevers_list = g.t.intern_list(&[never_t, never_t]).unwrap();
    let u_nevers_ty = g.n(PNode::Union(u_nevers_list));
    for (check, ty, want) in [
        (Check::Zero, u_ref_unit_ty, true),
        (Check::Zero, u_refs_ty, false),
        (Check::Ones, u_ref_unit_ty, false),
        (Check::Zero, u_bool_unit_ty, true),
        (Check::Ones, u_bool_unit_ty, true),
        (Check::Inhabited, u_64_never_ty, true),
        (Check::Inhabited, u_nevers_ty, false),
        (Check::Inhabited, u_empty_ty, false),
    ] {
        let mut cx = LayoutCx { ground: &mut g.t, facts: &w, scratch: h.alloc() };
        assert_eq!(permits(check, ty, &mut ls, &mut cx).unwrap(), want, "{check:?} of union type {}", ty.0);
    }
}
