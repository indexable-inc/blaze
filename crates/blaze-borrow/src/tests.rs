//! The one call rule, per callee kind and on tempo's shapes, with exact constraints; and the
//! two planted controls (an adapter that counts an opaque as 1, a variance that is all
//! covariant), each of which must change the verdict (rule 23).
//!
//! The caller's origin column is the identity (`Reg::Slot(i)` is `Origin(i)`), fresh origins
//! are numbered from 100, the caller's own lifetime `Param(i)` is `Origin(10 + i)` and its type
//! parameter `P`'s `ρ_P` is `Origin(20 + p)`. A verdict is read from the constraints by
//! reachability: a call is rejected when a local origin (a borrow of a local) reaches a
//! universal or `'static`, or a universal reaches another universal or `'static`.

use std::collections::BTreeSet;
use std::vec::Vec as SVec;

use blaze_flow::Origin;
use blaze_own::Alloc;
use blaze_typeck::region_facts::{Outlives, ParamsView};
use blaze_typeck::regions::{self, Arity, Count, LifetimeParams, Region};
use blaze_typeck::{FnPtrHeader, ItemKey, List, Mutbl, Name, PNode, PTy, PortableTys, UintTy};

use crate::inst::{Arg, Ask, Callee, ClauseIn, GenericArgs, ImplHead, ImplValue, ImplWhere, NotSelected, Selected, Selection, Sig, Typed, ValueKind};
use crate::relate::{Constraints, Cx, Kind, Owner};
use crate::types::{Columns, Kids, Pos, RegionTypes, Slot, SlotCount, SlotRange, Unknown, Variances, Why};
use crate::{Bad, Refused, Variance, call};

// ---------------------------------------------------------------------------------------------
// Fixture.

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

const P0: &[Region] = &[Region::Param(0)];
const P1: &[Region] = &[Region::Param(1)];
const P01: &[Region] = &[Region::Param(0), Region::Param(1)];
const P00: &[Region] = &[Region::Param(0), Region::Param(0)];
const P012: &[Region] = &[Region::Param(0), Region::Param(1), Region::Param(2)];
const P10: &[Region] = &[Region::Param(1), Region::Param(0)];
const STATIC: &[Region] = &[Region::Static];
const L00: &[Region] = &[Region::Late { binder: 0, var: 0 }];
const L10: &[Region] = &[Region::Late { binder: 1, var: 0 }];
const NONE: &[Region] = &[];

const OWNER: ItemKey = ItemKey(0x0101);
const F: ItemKey = ItemKey(0x0201);
const KEEP: ItemKey = ItemKey(0x0202);
const METHOD: ItemKey = ItemKey(0x0203);
const ADD: ItemKey = ItemKey(0x0204);
const F_RESULT: ItemKey = ItemKey(0x0205);
const F_CF: ItemKey = ItemKey(0x0206);
const F_PIN: ItemKey = ItemKey(0x0207);
const F_OPAQUE: ItemKey = ItemKey(0x0208);
const F_TWO: ItemKey = ItemKey(0x0209);
const F_PUT: ItemKey = ItemKey(0x020a);
const T_DE: ItemKey = ItemKey(0x0301);
const T_VISITOR: ItemKey = ItemKey(0x0302);
const T_ADD: ItemKey = ItemKey(0x0303);
const T_TR: ItemKey = ItemKey(0x0304);
const T_TRY: ItemKey = ItemKey(0x0305);
const T_FUT: ItemKey = ItemKey(0x0306);
const T_CONV: ItemKey = ItemKey(0x0307);
const K_VIS: ItemKey = ItemKey(0x0401);
const K_TWO: ItemKey = ItemKey(0x0402);
const K_TEXT: ItemKey = ItemKey(0x0403);
const K_RESULT: ItemKey = ItemKey(0x0404);
const K_WRAP: ItemKey = ItemKey(0x0405);
const K_CF: ItemKey = ItemKey(0x0406);
const K_PIN: ItemKey = ItemKey(0x0407);
const K_BOX: ItemKey = ItemKey(0x0408);
const K_PLAIN: ItemKey = ItemKey(0x0409);
const K_HOLD: ItemKey = ItemKey(0x040a);
const K_OPAQUE: ItemKey = ItemKey(0x0501);
const IMP: ItemKey = ItemKey(0x0601);

const VALUE: Name = Name::of("Value");
const OUTPUT: Name = Name::of("Output");
const OK: Name = Name::of("Ok");
const ERR: Name = Name::of("Err");
const RESIDUAL: Name = Name::of("Residual");

struct Tys {
    t: PortableTys<'static>,
}

impl Tys {
    fn new() -> Tys {
        Tys { t: PortableTys::new_in(alloc()) }
    }
    fn n(&mut self, node: PNode) -> PTy {
        self.t.intern(node).expect("a test type interns")
    }
    fn list(&mut self, tys: &[PTy]) -> List {
        self.t.intern_list(tys).expect("a test list interns")
    }
    fn u8(&mut self) -> PTy {
        self.n(PNode::Uint(UintTy::U8))
    }
    fn u16(&mut self) -> PTy {
        self.n(PNode::Uint(UintTy::U16))
    }
    fn unit(&mut self) -> PTy {
        self.n(PNode::Tuple(List::EMPTY))
    }
    fn r(&mut self, t: PTy) -> PTy {
        self.n(PNode::Ref(Mutbl::Not, t))
    }
    fn rm(&mut self, t: PTy) -> PTy {
        self.n(PNode::Ref(Mutbl::Mut, t))
    }
    fn param(&mut self, i: u32) -> PTy {
        self.n(PNode::Param(i))
    }
    fn adt(&mut self, k: ItemKey, args: &[PTy]) -> PTy {
        let l = self.list(args);
        self.n(PNode::Adt(k, l))
    }
    fn alias(&mut self, trait_: ItemKey, name: Name, args: &[PTy]) -> PTy {
        let args = self.list(args);
        self.n(PNode::Alias { trait_, name, args })
    }
    fn opaque(&mut self, k: ItemKey, args: &[PTy]) -> PTy {
        let l = self.list(args);
        self.n(PNode::Opaque(k, l))
    }
    fn fnptr(&mut self, inputs_then_output: &[PTy]) -> PTy {
        let l = self.list(inputs_then_output);
        self.n(PNode::FnPtr(l, FnPtrHeader::RUST))
    }
    fn extrait(&mut self, k: ItemKey, args: &[PTy]) -> PTy {
        let l = self.list(args);
        self.n(PNode::ExTrait(k, l))
    }
    fn exproj(&mut self, trait_: ItemKey, name: Name, args: &[PTy], term: PTy) -> PTy {
        let args = self.list(args);
        self.n(PNode::ExProj { trait_, name, args, term })
    }
    fn dynamic(&mut self, preds: &[PTy]) -> PTy {
        self.t.dynamic(preds).expect("a test object interns")
    }
}

/// The counting and variance facts of a test.
#[derive(Default)]
struct Facts {
    adts: SVec<(ItemKey, u32, SVec<Variance>)>,
    opaques: SVec<(ItemKey, u32)>,
    traits: SVec<(ItemKey, u32)>,
    aliases: SVec<(ItemKey, Name, u32)>,
}

impl Facts {
    fn adt(mut self, k: ItemKey, lifetimes: u32, variances: &[Variance]) -> Facts {
        self.adts.push((k, lifetimes, variances.to_vec()));
        self
    }
    fn opaque(mut self, k: ItemKey, captures: u32) -> Facts {
        self.opaques.push((k, captures));
        self
    }
    fn trait_(mut self, k: ItemKey, lifetimes: u32) -> Facts {
        self.traits.push((k, lifetimes));
        self
    }
    fn alias(mut self, trait_: ItemKey, name: Name, lifetimes: u32) -> Facts {
        self.aliases.push((trait_, name, lifetimes));
        self
    }
}

fn known(n: Option<u32>) -> Count {
    n.map_or(Count::NotKnown, Count::Known)
}

impl Arity for Facts {
    fn adt_lifetimes(&self, adt: ItemKey) -> Count {
        known(self.adts.iter().find(|a| a.0 == adt).map(|a| a.1))
    }
    fn opaque_captures(&self, opaque: ItemKey) -> Count {
        known(self.opaques.iter().find(|a| a.0 == opaque).map(|a| a.1))
    }
    fn alias_lifetimes(&self, trait_: ItemKey, name: Name) -> Count {
        known(self.aliases.iter().find(|a| a.0 == trait_ && a.1 == name).map(|a| a.2))
    }
    fn trait_lifetimes(&self, trait_: ItemKey) -> Count {
        known(self.traits.iter().find(|a| a.0 == trait_).map(|a| a.1))
    }
}

impl Variances for Facts {
    fn of(&self, adt: ItemKey) -> Option<&[Variance]> {
        self.adts.iter().find(|a| a.0 == adt).map(|a| a.2.as_slice())
    }
}

/// The type checker's selections at the call, by trait and associated name.
#[derive(Default)]
struct Sel<'s> {
    rows: SVec<(ItemKey, Option<Name>, Selection<'s>)>,
}

impl Selected for Sel<'_> {
    fn select(&self, ask: Ask) -> Selection<'_> {
        self.rows
            .iter()
            .find(|r| r.0 == ask.trait_ && r.1 == ask.name)
            .map_or(Selection::Not(NotSelected::Unanswered), |r| r.2)
    }
}

/// The caller.
#[derive(Default)]
struct Own<'s> {
    bounds: SVec<(u32, ItemKey, Typed<'s>)>,
}

impl Owner for Own<'_> {
    fn item(&self) -> ItemKey {
        OWNER
    }
    fn universal(&self, r: Region) -> Option<Origin> {
        match r {
            Region::Param(i) => Some(Origin(10 + i)),
            _ => None,
        }
    }
    fn param(&self, p: u32) -> Option<Origin> {
        Some(Origin(20 + p))
    }
    fn bound(&self, p: u32, trait_: ItemKey) -> Option<Typed<'_>> {
        self.bounds.iter().find(|b| b.0 == p && b.1 == trait_).map(|b| b.2)
    }
    fn body_params(&self, _: PTy) -> Option<&[u32]> {
        // No body type in these tables.
        None
    }
}

/// The recording double of the emit seam.
struct Rec {
    subsets: SVec<(u32, u32)>,
    statics: SVec<u32>,
    placeholders: SVec<u32>,
    next: u32,
}

impl Constraints for Rec {
    fn subset(&mut self, a: Origin, b: Origin) {
        self.subsets.push((a.0, b.0));
    }
    fn outlives_static(&mut self, a: Origin) {
        self.statics.push(a.0);
    }
    fn fresh(&mut self, kind: Kind) -> Origin {
        let o = Origin(self.next);
        if kind == Kind::Placeholder {
            self.placeholders.push(self.next);
        }
        self.next += 1;
        o
    }
}

fn set(rec: &Rec) -> BTreeSet<(u32, u32)> {
    rec.subsets.iter().copied().collect()
}

fn pairs(p: &[(u32, u32)]) -> BTreeSet<(u32, u32)> {
    p.iter().copied().collect()
}

const STATIC_NODE: u32 = u32::MAX;

fn reach(rec: &Rec, from: u32) -> BTreeSet<u32> {
    let mut seen = BTreeSet::new();
    let mut todo = SVec::from([from]);
    while let Some(o) = todo.pop() {
        for &(a, b) in &rec.subsets {
            if a == o && seen.insert(b) {
                todo.push(b);
            }
        }
        if rec.statics.contains(&o) && seen.insert(STATIC_NODE) {
            todo.push(STATIC_NODE);
        }
    }
    seen
}

/// The verdict: a local reaches a universal or `'static`, or a universal reaches another
/// universal or `'static` (no known edges in these tests).
fn rejects(rec: &Rec, locals: &[u32], universals: &[u32]) -> bool {
    let bad = |from: u32, r: &BTreeSet<u32>| r.contains(&STATIC_NODE) || universals.iter().any(|&u| u != from && r.contains(&u));
    locals.iter().any(|&l| {
        let r = reach(rec, l);
        r.contains(&STATIC_NODE) || universals.iter().any(|u| r.contains(u))
    }) || universals.iter().any(|&u| bad(u, &reach(rec, u)))
}

fn range(start: u32, count: u32) -> SlotRange {
    SlotRange::new(Slot::new(start), SlotCount::new(count)).expect("a test range fits")
}

fn arg(ty: PTy, start: u32, count: u32) -> Arg {
    Arg { ty, at: range(start, count) }
}

fn no_generics() -> GenericArgs<'static> {
    GenericArgs { lifetimes: SlotRange::EMPTY, types: &[] }
}

fn params(count: u32, late: &[u32]) -> LifetimeParams<Alloc<'static>> {
    let mut p = LifetimeParams::new_in(count, alloc());
    for &i in late {
        p.set_late(i).expect("a test parameter exists");
    }
    p
}

fn ty(ty: PTy, col: &'static [Region]) -> Typed<'static> {
    Typed { ty, col }
}

fn run(
    types: &dyn RegionTypes,
    sel: &dyn Selected,
    own: &dyn Owner,
    slots: u32,
    callee: Callee<'_>,
    generics: GenericArgs<'_>,
    args: &[Arg],
    dest: Arg,
) -> (Result<(), Refused>, Rec) {
    run_in(types, types, sel, own, slots, callee, generics, args, dest)
}

/// [`run`] with the declared types in their own table (`Cx::sig_types`).
fn run_in(
    types: &dyn RegionTypes,
    sig_types: &dyn RegionTypes,
    sel: &dyn Selected,
    own: &dyn Owner,
    slots: u32,
    callee: Callee<'_>,
    generics: GenericArgs<'_>,
    args: &[Arg],
    dest: Arg,
) -> (Result<(), Refused>, Rec) {
    let origins: SVec<Origin> = (0..slots).map(Origin).collect();
    let mut rec = Rec { subsets: SVec::new(), statics: SVec::new(), placeholders: SVec::new(), next: 100 };
    let res = {
        let mut cx = Cx { types, sig_types, selected: sel, owner: own, origins: &origins, out: &mut rec };
        call(&mut cx, callee, generics, args, dest, alloc())
    };
    (res, rec)
}

/// CONTROL: an adapter that answers 1 for every opaque (today's flow lowering).
struct Lying<'a>(Columns<'a>);

impl RegionTypes for Lying<'_> {
    fn node(&self, t: PTy) -> Result<PNode, Unknown> {
        self.0.node(t)
    }
    fn own(&self, t: PTy) -> Result<SlotCount, Unknown> {
        match self.0.node(t)? {
            PNode::Opaque(..) => Ok(SlotCount::new(1)),
            _ => self.0.own(t),
        }
    }
    fn kids(&self, t: PTy) -> Result<Kids<'_>, Unknown> {
        self.0.kids(t)
    }
    fn adt_variances(&self, adt: ItemKey) -> Option<&[Variance]> {
        self.0.adt_variances(adt)
    }
}

/// CONTROL: every position covariant (`&mut T` too).
struct AllCo<'a>(Columns<'a>);

impl RegionTypes for AllCo<'_> {
    fn node(&self, t: PTy) -> Result<PNode, Unknown> {
        self.0.node(t)
    }
    fn own(&self, t: PTy) -> Result<SlotCount, Unknown> {
        self.0.own(t)
    }
    fn kids(&self, t: PTy) -> Result<Kids<'_>, Unknown> {
        self.0.kids(t)
    }
    fn adt_variances(&self, adt: ItemKey) -> Option<&[Variance]> {
        self.0.adt_variances(adt)
    }
    fn variance(&self, _: PTy, _: Pos) -> Result<Variance, Unknown> {
        Ok(Variance::Co)
    }
}

// ---------------------------------------------------------------------------------------------
// Per callee kind: one accept, one reject, exact constraints.

/// `fn pick<'a>(x: &'a u8, y: &'a u8) -> &'a u8` ('a late-bound: fresh 100).
#[test]
fn fn_item_accepts_through_a_late_bound_lifetime() {
    let mut t = Tys::new();
    let u8_ = t.u8();
    let r = t.r(u8_);
    let facts = Facts::default();
    let p = params(1, &[0]);
    let inputs = [ty(r, P0), ty(r, P0)];
    let sig = Sig { item: F, params: ParamsView::of(&p), inputs: &inputs, output: ty(r, P0), clauses: &[] };
    let cols = Columns::new(t.t.view(), &facts, &facts);
    let (res, rec) = run(&cols, &Sel::default(), &Own::default(), 3, Callee::Fn(sig), no_generics(), &[arg(r, 0, 1), arg(r, 1, 1)], arg(r, 2, 1));
    assert_eq!(res, Ok(()));
    assert_eq!(set(&rec), pairs(&[(0, 100), (1, 100), (100, 2)]));
    assert!(rec.statics.is_empty());
    assert!(!rejects(&rec, &[0, 1], &[]));
}

/// `fn keep(x: &'static u8)` called with a borrow of a local.
#[test]
fn fn_item_rejects_a_local_where_static_is_wanted() {
    let mut t = Tys::new();
    let u8_ = t.u8();
    let r = t.r(u8_);
    let unit = t.unit();
    let facts = Facts::default();
    let p = params(0, &[]);
    let inputs = [ty(r, STATIC)];
    let sig = Sig { item: KEEP, params: ParamsView::of(&p), inputs: &inputs, output: ty(unit, NONE), clauses: &[] };
    let cols = Columns::new(t.t.view(), &facts, &facts);
    let (res, rec) = run(&cols, &Sel::default(), &Own::default(), 1, Callee::Fn(sig), no_generics(), &[arg(r, 0, 1)], arg(unit, 1, 0));
    assert_eq!(res, Ok(()));
    assert!(rec.subsets.is_empty());
    assert_eq!(rec.statics, [0]);
    assert!(rejects(&rec, &[0], &[]));
}

/// The method shape of `Deserializer<'de>::deserialize_seq<V: Visitor<'de>>(self, v: V) ->
/// V::Value`, called from `fn go<'a, De: Deserializer<'a>>(d: De, v: Vis<'_>)`, with
/// `impl<'x> Visitor<'x> for Vis<'x> { type Value = &'x u8; }` selected. Slots: 0 the call's
/// `'de`, 1 the call's `V = Vis<'1>`, 2 the argument `v`'s, 3 the destination's. The clause
/// `Self: Deserializer<'de>` ties `'de` to the caller's `'a` (Origin 10); `V: Visitor<'de>`
/// ties it to the impl's `'x`, so `v`'s region reaches `'a`.
#[test]
fn method_ties_the_traits_lifetime_to_the_callers_bound_and_the_impl() {
    let mut t = Tys::new();
    let u8_ = t.u8();
    let r = t.r(u8_);
    let p0 = t.param(0);
    let p1 = t.param(1);
    let value = t.alias(T_VISITOR, VALUE, &[p1]);
    let ex_de = t.extrait(T_DE, &[]);
    let ex_vis = t.extrait(T_VISITOR, &[]);
    let vis = t.adt(K_VIS, &[]);
    // The impl's header subject-first, as the head record lays it: `ExTrait(Visitor, [Vis])`
    // over `['x, 'x]` (the trait's `'x`, then `Vis<'x>`'s).
    let header = t.extrait(T_VISITOR, &[vis]);
    let facts = Facts::default().trait_(T_DE, 1).trait_(T_VISITOR, 1).alias(T_VISITOR, VALUE, 1).adt(K_VIS, 1, &[Variance::Co]);
    let head = ImplHead { imp: IMP, lifetimes: 1, types: 0, trait_ref: ty(header, P00) };
    let sel = Sel {
        rows: SVec::from([
            (T_VISITOR, Some(VALUE), Selection::Impl { head, value: Some(ImplValue { value: ty(r, P0), gat_lifetimes: 0 }) }),
            (T_VISITOR, None, Selection::Impl { head, value: None }),
        ]),
    };
    let own = Own { bounds: SVec::from([(0, T_DE, ty(ex_de, P0))]) };
    let p = params(1, &[]);
    let inputs = [ty(p0, NONE), ty(p1, NONE)];
    let both = [
        ClauseIn::Trait { subject: ty(p0, NONE), trait_ref: ty(ex_de, P0) },
        ClauseIn::Trait { subject: ty(p1, NONE), trait_ref: ty(ex_vis, P0) },
    ];
    let types = [arg(p0, 1, 0), arg(vis, 1, 1)];
    let generics = GenericArgs { lifetimes: range(0, 1), types: &types };
    let args = [arg(p0, 2, 0), arg(vis, 2, 1)];
    let cols = Columns::new(t.t.view(), &facts, &facts);

    let sig = Sig { item: METHOD, params: ParamsView::of(&p), inputs: &inputs, output: ty(value, P0), clauses: &both };
    let (res, rec) = run(&cols, &sel, &own, 4, Callee::Method { trait_: T_DE, sig }, generics, &args, arg(r, 3, 1));
    assert_eq!(res, Ok(()));
    assert_eq!(set(&rec), pairs(&[(0, 1), (0, 3), (0, 10), (1, 0), (2, 1), (10, 0)]));
    assert!(!rejects(&rec, &[], &[10]), "accept: nothing local reaches 'a");
    assert!(rejects(&rec, &[2], &[10]), "reject: a local visitor's region reaches 'a");

    // CONTROL: without the `Self: Deserializer<'de>` clause the local is wrongly accepted.
    let sig = Sig { item: METHOD, params: ParamsView::of(&p), inputs: &inputs, output: ty(value, P0), clauses: &both[1..] };
    let (res, rec) = run(&cols, &sel, &own, 4, Callee::Method { trait_: T_DE, sig }, generics, &args, arg(r, 3, 1));
    assert_eq!(res, Ok(()));
    assert!(!rejects(&rec, &[2], &[10]), "the clause is the cause of the rejection");
}

/// `struct Two<'a, 'b>(&'a u8, &'b u8)` built: slots 0, 1 the call's lifetimes, 2, 3 the
/// fields' arguments, 4, 5 the destination's.
#[test]
fn ctor_relates_fields_to_its_lifetimes() {
    let mut t = Tys::new();
    let u8_ = t.u8();
    let r = t.r(u8_);
    let two = t.adt(K_TWO, &[]);
    let facts = Facts::default().adt(K_TWO, 2, &[Variance::Co, Variance::Co]);
    let fields = [ty(r, P0), ty(r, P1)];
    let callee = Callee::Ctor { adt: K_TWO, variant: 0, lifetimes: 2, fields: &fields, output: ty(two, P01) };
    let generics = GenericArgs { lifetimes: range(0, 2), types: &[] };
    let cols = Columns::new(t.t.view(), &facts, &facts);
    let (res, rec) = run(&cols, &Sel::default(), &Own::default(), 6, callee, generics, &[arg(r, 2, 1), arg(r, 3, 1)], arg(two, 4, 2));
    assert_eq!(res, Ok(()));
    assert_eq!(set(&rec), pairs(&[(2, 0), (3, 1), (0, 4), (1, 5)]));
    assert!(!rejects(&rec, &[2, 3], &[]));
    assert!(rejects(&rec, &[2], &[4]), "a local field in a value whose first lifetime is the caller's");
}

/// A closure `for<'r> |x: &'r u8| -> &'r u8`: its binder instantiated fresh (100).
#[test]
fn closure_instantiates_its_binder_and_reads_the_owners_regions() {
    let mut t = Tys::new();
    let u8_ = t.u8();
    let r = t.r(u8_);
    let facts = Facts::default();
    let cols = Columns::new(t.t.view(), &facts, &facts);

    let inputs = [ty(r, L00)];
    let callee = Callee::Closure { owner: OWNER, ordinal: 0, binder: 0, inputs: &inputs, output: ty(r, L00) };
    let (res, rec) = run(&cols, &Sel::default(), &Own::default(), 2, callee, no_generics(), &[arg(r, 0, 1)], arg(r, 1, 1));
    assert_eq!(res, Ok(()));
    assert_eq!(set(&rec), pairs(&[(0, 100), (100, 1)]));
    assert!(!rejects(&rec, &[0], &[]));

    // `|x: &'a u8| -> &u8` with `'a` the owner's: a local argument reaches it.
    let inputs = [ty(r, P0)];
    let callee = Callee::Closure { owner: OWNER, ordinal: 0, binder: 0, inputs: &inputs, output: ty(r, L00) };
    let (res, rec) = run(&cols, &Sel::default(), &Own::default(), 2, callee, no_generics(), &[arg(r, 0, 1)], arg(r, 1, 1));
    assert_eq!(res, Ok(()));
    assert_eq!(set(&rec), pairs(&[(0, 10), (100, 1)]));
    assert!(rejects(&rec, &[0], &[10]));
}

/// A fn pointer `fn(&T) -> &U` (elided: one binder variable), and one whose input is itself a
/// fn pointer with its own binder: higher-ranked, refused by name.
#[test]
fn fn_pointer_accepts_and_refuses_a_nested_binder() {
    let mut t = Tys::new();
    let u8_ = t.u8();
    let u16_ = t.u16();
    let r = t.r(u8_);
    let r16 = t.r(u16_);
    let unit = t.unit();
    let inner = t.fnptr(&[r, unit]);
    let facts = Facts::default();
    let cols = Columns::new(t.t.view(), &facts, &facts);

    let inputs = [ty(r, L00)];
    let callee = Callee::Value { kind: ValueKind::FnPtr, owner: OWNER, binder: 0, inputs: &inputs, output: ty(r16, L00) };
    let (res, rec) = run(&cols, &Sel::default(), &Own::default(), 2, callee, no_generics(), &[arg(r, 0, 1)], arg(r16, 1, 1));
    assert_eq!(res, Ok(()));
    assert_eq!(set(&rec), pairs(&[(0, 100), (100, 1)]));
    assert!(!rejects(&rec, &[0], &[]));

    // `fn(for<'r> fn(&'r u8))`: higher-ranked subtyping Skolemizes `'r` to a Placeholder (100)
    // and emits `100: '0` (contravariant in the inner fn's argument).
    let inputs = [ty(inner, L10)];
    let callee = Callee::Value { kind: ValueKind::FnPtr, owner: OWNER, binder: 0, inputs: &inputs, output: ty(unit, NONE) };
    let (res, rec) = run(&cols, &Sel::default(), &Own::default(), 1, callee, no_generics(), &[arg(inner, 0, 1)], arg(unit, 1, 0));
    assert_eq!(res, Ok(()));
    assert_eq!(set(&rec), pairs(&[(100, 0)]));
    assert!(rec.placeholders.contains(&100));

    // Invariant position (`&mut (for<'r> fn(&'r u8))`): still refused as `HigherRanked`.
    let rmut_inner = t.rm(inner);
    let cols2 = Columns::new(t.t.view(), &facts, &facts);
    const L00_L10: &[Region] = &[Region::Late { binder: 0, var: 0 }, Region::Late { binder: 1, var: 0 }];
    let inputs2 = [ty(rmut_inner, L00_L10)];
    let callee2 = Callee::Value { kind: ValueKind::FnPtr, owner: OWNER, binder: 0, inputs: &inputs2, output: ty(unit, NONE) };
    let (res2, _) = run(&cols2, &Sel::default(), &Own::default(), 2, callee2, no_generics(), &[arg(rmut_inner, 0, 2)], arg(unit, 2, 0));
    assert_eq!(res2, Err(Refused::HigherRanked { item: OWNER, node: r, binder: 1 }));
}

/// `Box<dyn Fn() -> &'a u8>` and `Box<dyn Fn(&'a u8)>` of the caller (its `'a` is Origin 10).
#[test]
fn dyn_fn_reads_the_objects_regions_as_the_owners() {
    let mut t = Tys::new();
    let u8_ = t.u8();
    let r = t.r(u8_);
    let unit = t.unit();
    let facts = Facts::default();
    let cols = Columns::new(t.t.view(), &facts, &facts);

    let callee = Callee::Value { kind: ValueKind::Dyn, owner: OWNER, binder: 0, inputs: &[], output: ty(r, P0) };
    let (res, rec) = run(&cols, &Sel::default(), &Own::default(), 1, callee, no_generics(), &[], arg(r, 0, 1));
    assert_eq!(res, Ok(()));
    assert_eq!(set(&rec), pairs(&[(10, 0)]));
    assert!(!rejects(&rec, &[0], &[10]));

    let inputs = [ty(r, P0)];
    let callee = Callee::Value { kind: ValueKind::Dyn, owner: OWNER, binder: 0, inputs: &inputs, output: ty(unit, NONE) };
    let (res, rec) = run(&cols, &Sel::default(), &Own::default(), 1, callee, no_generics(), &[arg(r, 0, 1)], arg(unit, 1, 0));
    assert_eq!(res, Ok(()));
    assert_eq!(set(&rec), pairs(&[(0, 10)]));
    assert!(rejects(&rec, &[0], &[10]));
}

/// `F: Fn(&u8) -> &u8` of the caller; then an argument of another type: `Refused::Shape`.
#[test]
fn fn_bound_accepts_and_refuses_a_shape_mismatch() {
    let mut t = Tys::new();
    let u8_ = t.u8();
    let u16_ = t.u16();
    let r = t.r(u8_);
    let r16 = t.r(u16_);
    let facts = Facts::default();
    let cols = Columns::new(t.t.view(), &facts, &facts);
    let inputs = [ty(r, L00)];
    let callee = Callee::Value { kind: ValueKind::Bound, owner: OWNER, binder: 0, inputs: &inputs, output: ty(r, L00) };

    let (res, rec) = run(&cols, &Sel::default(), &Own::default(), 2, callee, no_generics(), &[arg(r, 0, 1)], arg(r, 1, 1));
    assert_eq!(res, Ok(()));
    assert_eq!(set(&rec), pairs(&[(0, 100), (100, 1)]));

    let (res, _) = run(&cols, &Sel::default(), &Own::default(), 2, callee, no_generics(), &[arg(r16, 0, 1)], arg(r, 1, 1));
    assert_eq!(res, Err(Refused::Shape { item: OWNER, a: u16_, b: u8_, why: crate::ShapeWhy::Heads { a: crate::Head::Uint, b: crate::Head::Uint } }));
}

/// `a + b` for `&Text`, `Add::add(self, rhs: Rhs) -> Self::Output` with `impl<'x> Add<&'x Text>
/// for &'x Text { type Output = &'x Text; }` selected. Slots: 0, 1 the arguments, 2 the
/// destination, 3, 4 the call's `Self` and `Rhs`. The header ties 3 and 4.
#[test]
fn operator_normalizes_its_output_through_the_selected_impl() {
    let mut t = Tys::new();
    let text = t.adt(K_TEXT, &[]);
    let rt = t.r(text);
    let p0 = t.param(0);
    let p1 = t.param(1);
    let out = t.alias(T_ADD, OUTPUT, &[p0, p1]);
    // Subject-first: `ExTrait(Add, [&'x Text, &'x Text])` over `['x, 'x]`.
    let header = t.extrait(T_ADD, &[rt, rt]);
    let facts = Facts::default().trait_(T_ADD, 0).alias(T_ADD, OUTPUT, 0).adt(K_TEXT, 0, &[]);
    let head = ImplHead { imp: IMP, lifetimes: 1, types: 0, trait_ref: ty(header, P00) };
    let sel = Sel { rows: SVec::from([(T_ADD, Some(OUTPUT), Selection::Impl { head, value: Some(ImplValue { value: ty(rt, P0), gat_lifetimes: 0 }) })]) };
    let p = params(0, &[]);
    let inputs = [ty(p0, NONE), ty(p1, NONE)];
    let sig = Sig { item: ADD, params: ParamsView::of(&p), inputs: &inputs, output: ty(out, NONE), clauses: &[] };
    let types = [arg(rt, 3, 1), arg(rt, 4, 1)];
    let generics = GenericArgs { lifetimes: SlotRange::EMPTY, types: &types };
    let args = [arg(rt, 0, 1), arg(rt, 1, 1)];
    let cols = Columns::new(t.t.view(), &facts, &facts);

    let (res, rec) = run(&cols, &sel, &Own::default(), 5, Callee::Operator { trait_: T_ADD, sig }, generics, &args, arg(rt, 2, 1));
    assert_eq!(res, Ok(()));
    assert_eq!(set(&rec), pairs(&[(0, 3), (1, 4), (3, 4), (4, 3), (3, 2)]));
    assert!(!rejects(&rec, &[0, 1], &[]));
    assert!(rejects(&rec, &[0], &[2]), "a local operand reaches a destination of the caller's lifetime");

    // The impl not selected (generic at the call): refused by name where normalization is needed.
    let generic = Sel { rows: SVec::from([(T_ADD, Some(OUTPUT), Selection::Not(NotSelected::Generic))]) };
    let (res, _) = run(&cols, &generic, &Own::default(), 5, Callee::Operator { trait_: T_ADD, sig }, generics, &args, arg(rt, 2, 1));
    assert_eq!(res, Err(Refused::Unselected { item: ADD, node: out, trait_: T_ADD, name: Some(OUTPUT), why: NotSelected::Generic }));
}

// ---------------------------------------------------------------------------------------------
// Tempo's shapes.

/// `fn f<T: Tr>(t: T) -> Result<T::Ok, T::Err>` with `impl<'x> Tr for Wrap<'x> { type Ok =
/// &'x u8; type Err = (); }`. Slots: 0 the call's `T = Wrap<'0>`, 1 the argument, 2 the
/// destination's `&u8`.
#[test]
fn result_of_two_projections() {
    let mut t = Tys::new();
    let u8_ = t.u8();
    let r = t.r(u8_);
    let unit = t.unit();
    let p0 = t.param(0);
    let wrap = t.adt(K_WRAP, &[]);
    let ok = t.alias(T_TR, OK, &[p0]);
    let err = t.alias(T_TR, ERR, &[p0]);
    let res_sig = t.adt(K_RESULT, &[ok, err]);
    let res_dest = t.adt(K_RESULT, &[r, unit]);
    let header = t.extrait(T_TR, &[wrap]);
    let facts = Facts::default()
        .trait_(T_TR, 0)
        .alias(T_TR, OK, 0)
        .alias(T_TR, ERR, 0)
        .adt(K_WRAP, 1, &[Variance::Co])
        .adt(K_RESULT, 0, &[Variance::Co, Variance::Co]);
    let head = ImplHead { imp: IMP, lifetimes: 1, types: 0, trait_ref: ty(header, P0) };
    let sel = Sel {
        rows: SVec::from([
            (T_TR, Some(OK), Selection::Impl { head, value: Some(ImplValue { value: ty(r, P0), gat_lifetimes: 0 }) }),
            (T_TR, Some(ERR), Selection::Impl { head, value: Some(ImplValue { value: ty(unit, NONE), gat_lifetimes: 0 }) }),
        ]),
    };
    let p = params(0, &[]);
    let inputs = [ty(p0, NONE)];
    let sig = Sig { item: F_RESULT, params: ParamsView::of(&p), inputs: &inputs, output: ty(res_sig, NONE), clauses: &[] };
    let types = [arg(wrap, 0, 1)];
    let cols = Columns::new(t.t.view(), &facts, &facts);
    let (res, rec) = run(&cols, &sel, &Own::default(), 3, Callee::Fn(sig), GenericArgs { lifetimes: SlotRange::EMPTY, types: &types }, &[arg(wrap, 1, 1)], arg(res_dest, 2, 1));
    assert_eq!(res, Ok(()));
    assert_eq!(set(&rec), pairs(&[(1, 0), (0, 2)]));
}

/// `fn f<R: Try>(r: R) -> ControlFlow<R::Residual, R::Output>` with `impl<'x> Try for Wrap<'x>
/// { type Residual = &'x u16; type Output = &'x u8; }`. Slots: 0 the call's `R`, 1 the
/// argument, 2, 3 the destination's.
#[test]
fn control_flow_of_two_projections() {
    let mut t = Tys::new();
    let u8_ = t.u8();
    let u16_ = t.u16();
    let r = t.r(u8_);
    let r16 = t.r(u16_);
    let p0 = t.param(0);
    let wrap = t.adt(K_WRAP, &[]);
    let residual = t.alias(T_TRY, RESIDUAL, &[p0]);
    let output = t.alias(T_TRY, OUTPUT, &[p0]);
    let cf_sig = t.adt(K_CF, &[residual, output]);
    let cf_dest = t.adt(K_CF, &[r16, r]);
    let header = t.extrait(T_TRY, &[wrap]);
    let facts = Facts::default()
        .trait_(T_TRY, 0)
        .alias(T_TRY, RESIDUAL, 0)
        .alias(T_TRY, OUTPUT, 0)
        .adt(K_WRAP, 1, &[Variance::Co])
        .adt(K_CF, 0, &[Variance::Co, Variance::Co]);
    let head = ImplHead { imp: IMP, lifetimes: 1, types: 0, trait_ref: ty(header, P0) };
    let sel = Sel {
        rows: SVec::from([
            (T_TRY, Some(RESIDUAL), Selection::Impl { head, value: Some(ImplValue { value: ty(r16, P0), gat_lifetimes: 0 }) }),
            (T_TRY, Some(OUTPUT), Selection::Impl { head, value: Some(ImplValue { value: ty(r, P0), gat_lifetimes: 0 }) }),
        ]),
    };
    let p = params(0, &[]);
    let inputs = [ty(p0, NONE)];
    let sig = Sig { item: F_CF, params: ParamsView::of(&p), inputs: &inputs, output: ty(cf_sig, NONE), clauses: &[] };
    let types = [arg(wrap, 0, 1)];
    let cols = Columns::new(t.t.view(), &facts, &facts);
    let (res, rec) = run(&cols, &sel, &Own::default(), 4, Callee::Fn(sig), GenericArgs { lifetimes: SlotRange::EMPTY, types: &types }, &[arg(wrap, 1, 1)], arg(cf_dest, 2, 2));
    assert_eq!(res, Ok(()));
    assert_eq!(set(&rec), pairs(&[(1, 0), (0, 2), (0, 3)]));
}

/// A `for` LOOP'S CALL (census wave-1119 t67: every `for` desugaring's call and two of a `?`'s
/// were refused `Malformed` once the impls' heads were read). The Reference, "Iterator loops":
/// `for x in v` is `IntoIterator::into_iter(v)`, `fn into_iter(self) -> <Self as
/// IntoIterator>::IntoIter`; with `v: &'a [u8]` the type checker selects `impl<'x> IntoIterator
/// for &'x [u8] { type IntoIter = Iter<'x>; }`. The head record lays the impl's header
/// SUBJECT-FIRST (blaze-ladder heads_regions.rs `header_trait`: `ExTrait(IntoIterator, [&'x
/// [u8]])` over `['x]`), as the alias is (`<Self as IntoIterator>::IntoIter`, children
/// `[Self]`), and the rule normalizes through it child for child (relate.rs `normalize`).
/// Slots: 0 the call's `Self = &'0 [u8]`, 1 the argument, 2 the destination's `Iter<'2>`: the
/// argument's region reaches the iterator's, `(1, 0), (0, 2)`. `looped_ref`: nothing local
/// reaches a universal: accepted.
/// TWIN (`looped_ref_bad`, rustc "lifetime may not live long enough"): the destination's region
/// a universal of the caller's (the `'static` it returns) and the argument a local's borrow:
/// rejected.
/// CONTROL (rule 23, the cause put back): the header laid without its subject, the form the rule
/// read before (`ExTrait(IntoIterator, [])`): refused `Malformed` naming why, `HeaderKids`
/// (the alias's one child, the header's none), never related by a guess.
#[test]
fn a_for_loops_into_iter_normalizes_through_a_subject_first_header() {
    const F_INTO: ItemKey = ItemKey(0x020b);
    const T_INTO: ItemKey = ItemKey(0x0308);
    const K_ITER: ItemKey = ItemKey(0x040c);
    const INTO_ITER: Name = Name::of("IntoIter");
    let mut t = Tys::new();
    let u8_ = t.u8();
    let slice = t.n(PNode::Slice(u8_));
    let rs = t.r(slice);
    let p0 = t.param(0);
    let iter = t.adt(K_ITER, &[]);
    let alias = t.alias(T_INTO, INTO_ITER, &[p0]);
    let header = t.extrait(T_INTO, &[rs]);
    let without_self = t.extrait(T_INTO, &[]);
    let facts = Facts::default().trait_(T_INTO, 0).alias(T_INTO, INTO_ITER, 0).adt(K_ITER, 1, &[Variance::Co]);
    let impl_of = |trait_ref: Typed<'static>| Sel {
        rows: SVec::from([(
            T_INTO,
            Some(INTO_ITER),
            Selection::Impl { head: ImplHead { imp: IMP, lifetimes: 1, types: 0, trait_ref }, value: Some(ImplValue { value: ty(iter, P0), gat_lifetimes: 0 }) },
        )]),
    };
    let p = params(0, &[]);
    let inputs = [ty(p0, NONE)];
    let sig = Sig { item: F_INTO, params: ParamsView::of(&p), inputs: &inputs, output: ty(alias, NONE), clauses: &[] };
    let types = [arg(rs, 0, 1)];
    let generics = GenericArgs { lifetimes: SlotRange::EMPTY, types: &types };
    let cols = Columns::new(t.t.view(), &facts, &facts);

    let (res, rec) = run(&cols, &impl_of(ty(header, P0)), &Own::default(), 3, Callee::Fn(sig), generics, &[arg(rs, 1, 1)], arg(iter, 2, 1));
    assert_eq!(res, Ok(()));
    assert_eq!(set(&rec), pairs(&[(1, 0), (0, 2)]));
    assert!(!rejects(&rec, &[1], &[]), "looped_ref: nothing local reaches a universal");
    assert!(rejects(&rec, &[1], &[2]), "TWIN looped_ref_bad: a local's borrow reaches the caller's region it is returned at");

    let (res, _) = run(&cols, &impl_of(ty(without_self, NONE)), &Own::default(), 3, Callee::Fn(sig), generics, &[arg(rs, 1, 1)], arg(iter, 2, 1));
    assert_eq!(res, Err(Refused::Malformed { item: F_INTO, node: alias, what: Bad::HeaderKids { alias: 1, header: 0 } }), "CONTROL: the header without its subject");
}

/// `fn f<'a>(x: &'a u8) -> Pin<Box<dyn Future<Output = &'a u8> + 'a>>` ('a late: 100). The
/// object's own region is covariant (1); its `Output` term invariant (2).
#[test]
fn pin_box_dyn_future() {
    let mut t = Tys::new();
    let u8_ = t.u8();
    let r = t.r(u8_);
    let principal = t.extrait(T_FUT, &[]);
    let proj = t.exproj(T_FUT, OUTPUT, &[], r);
    let obj = t.dynamic(&[principal, proj]);
    let bx = t.adt(K_BOX, &[obj]);
    let pin = t.adt(K_PIN, &[bx]);
    let facts = Facts::default().trait_(T_FUT, 0).alias(T_FUT, OUTPUT, 0).adt(K_BOX, 0, &[Variance::Co]).adt(K_PIN, 0, &[Variance::Co]);
    let p = params(1, &[0]);
    let inputs = [ty(r, P0)];
    let sig = Sig { item: F_PIN, params: ParamsView::of(&p), inputs: &inputs, output: ty(pin, P00), clauses: &[] };
    let cols = Columns::new(t.t.view(), &facts, &facts);
    let (res, rec) = run(&cols, &Sel::default(), &Own::default(), 3, Callee::Fn(sig), no_generics(), &[arg(r, 0, 1)], arg(pin, 1, 2));
    assert_eq!(res, Ok(()));
    assert_eq!(set(&rec), pairs(&[(0, 100), (100, 1), (100, 2), (2, 100)]));
}

/// `fn f<'a, 'b>(x: &'a u8, y: &'b u16) -> impl Iterator<Item = &'a u8> + use<'a, 'b>`: the
/// opaque has TWO own slots (its captures), invariant. CONTROL: an adapter that answers 1 for
/// an opaque makes the call refuse; an opaque whose captures are not filed is `Count`.
#[test]
fn opaque_has_its_captures_as_slots() {
    let mut t = Tys::new();
    let u8_ = t.u8();
    let u16_ = t.u16();
    let r = t.r(u8_);
    let r16 = t.r(u16_);
    let op = t.opaque(K_OPAQUE, &[]);
    let facts = Facts::default().opaque(K_OPAQUE, 2);
    let p = params(2, &[0, 1]);
    let inputs = [ty(r, P0), ty(r16, P1)];
    let sig = Sig { item: F_OPAQUE, params: ParamsView::of(&p), inputs: &inputs, output: ty(op, P01), clauses: &[] };
    let args = [arg(r, 0, 1), arg(r16, 1, 1)];
    let cols = Columns::new(t.t.view(), &facts, &facts);
    let (res, rec) = run(&cols, &Sel::default(), &Own::default(), 4, Callee::Fn(sig), no_generics(), &args, arg(op, 2, 2));
    assert_eq!(res, Ok(()));
    assert_eq!(set(&rec), pairs(&[(0, 100), (1, 101), (100, 2), (2, 100), (101, 3), (3, 101)]));

    let lying = Lying(cols);
    let (res, _) = run(&lying, &Sel::default(), &Own::default(), 4, Callee::Fn(sig), no_generics(), &args, arg(op, 2, 2));
    assert_eq!(res, Err(Refused::Malformed { item: F_OPAQUE, node: op, what: Bad::ColumnLength { want: SlotCount::new(1), got: SlotCount::new(2) } }));

    let unfiled = Facts::default();
    let cols = Columns::new(t.t.view(), &unfiled, &unfiled);
    let (res, _) = run(&cols, &Sel::default(), &Own::default(), 4, Callee::Fn(sig), no_generics(), &args, arg(op, 2, 2));
    assert_eq!(res, Err(Refused::Count { item: F_OPAQUE, unknown: Unknown { node: op, why: Why::Fact(regions::Unknown::OpaqueCaptures(K_OPAQUE)) } }));
}

/// `fn get<'s, 'a, 'b>(x: &'s Two<'a, 'b>) -> &'a u8` (all late: 100, 101, 102).
#[test]
fn adt_with_two_lifetimes_inside_a_reference() {
    let mut t = Tys::new();
    let u8_ = t.u8();
    let r = t.r(u8_);
    let two = t.adt(K_TWO, &[]);
    let rtwo = t.r(two);
    let facts = Facts::default().adt(K_TWO, 2, &[Variance::Co, Variance::Co]);
    let p = params(3, &[0, 1, 2]);
    let inputs = [ty(rtwo, P012)];
    let sig = Sig { item: F_TWO, params: ParamsView::of(&p), inputs: &inputs, output: ty(r, P1), clauses: &[] };
    let cols = Columns::new(t.t.view(), &facts, &facts);
    let (res, rec) = run(&cols, &Sel::default(), &Own::default(), 4, Callee::Fn(sig), no_generics(), &[arg(rtwo, 0, 3)], arg(r, 3, 1));
    assert_eq!(res, Ok(()));
    // The input's well-formedness at the call: `&'s Two<'a, 'b>` makes `'a: 's`, `'b: 's`.
    assert_eq!(set(&rec), pairs(&[(0, 100), (1, 101), (2, 102), (101, 3), (101, 100), (102, 100)]));
}

/// `fn put<'a, 'b>(slot: &'b mut &'a u8, v: &'a u8)` with the slot's referent of the caller's
/// lifetime (slot 1) and `v` a local (slot 2): rejected, because `&mut T` is invariant in `T`.
/// CONTROL: an all-covariant variance wrongly accepts it.
#[test]
fn mut_reference_is_invariant() {
    let mut t = Tys::new();
    let u8_ = t.u8();
    let r = t.r(u8_);
    let rmr = t.rm(r);
    let unit = t.unit();
    let facts = Facts::default();
    let p = params(2, &[0, 1]);
    let inputs = [ty(rmr, P10), ty(r, P0)];
    let sig = Sig { item: F_PUT, params: ParamsView::of(&p), inputs: &inputs, output: ty(unit, NONE), clauses: &[] };
    let args = [arg(rmr, 0, 2), arg(r, 2, 1)];
    let cols = Columns::new(t.t.view(), &facts, &facts);
    let (res, rec) = run(&cols, &Sel::default(), &Own::default(), 3, Callee::Fn(sig), no_generics(), &args, arg(unit, 3, 0));
    assert_eq!(res, Ok(()));
    // (100, 101): the slot's well-formedness, `&'b mut &'a u8` makes `'a: 'b`.
    assert_eq!(set(&rec), pairs(&[(0, 101), (1, 100), (100, 1), (2, 100), (100, 101)]));
    assert!(rejects(&rec, &[2], &[1]));

    let all_co = AllCo(cols);
    let (res, rec) = run(&all_co, &Sel::default(), &Own::default(), 3, Callee::Fn(sig), no_generics(), &args, arg(unit, 3, 0));
    assert_eq!(res, Ok(()));
    assert_eq!(set(&rec), pairs(&[(0, 101), (1, 100), (2, 100), (100, 101)]));
    assert!(!rejects(&rec, &[2], &[1]), "the invariance is the cause of the rejection");
}

// ---------------------------------------------------------------------------------------------
// The contract's extensions for the switch (D156): two tables, well-formedness at a call, an
// impl whose header was not read.

/// `fn f<'a, 'b>(x: &'a &'b u8)` ('a 100, 'b 101, both late): the instance's well-formedness
/// makes `'b: 'a` (101, 100), the implied bound the callee was promised. CONTROL (rule 23):
/// the same lifetimes not nested (`fn g<'a, 'b>(x: &'a u8, y: &'b u8)`) make no edge between
/// them: the nesting is the cause.
#[test]
fn a_nested_reference_is_well_formed_at_the_call() {
    let mut t = Tys::new();
    let u8_ = t.u8();
    let r = t.r(u8_);
    let rr = t.r(r);
    let unit = t.unit();
    let facts = Facts::default();
    let p = params(2, &[0, 1]);
    let inputs = [ty(rr, P01)];
    let sig = Sig { item: F, params: ParamsView::of(&p), inputs: &inputs, output: ty(unit, NONE), clauses: &[] };
    let cols = Columns::new(t.t.view(), &facts, &facts);
    let (res, rec) = run(&cols, &Sel::default(), &Own::default(), 2, Callee::Fn(sig), no_generics(), &[arg(rr, 0, 2)], arg(unit, 2, 0));
    assert_eq!(res, Ok(()));
    assert_eq!(set(&rec), pairs(&[(0, 100), (1, 101), (101, 100)]));

    let apart = [ty(r, P0), ty(r, P1)];
    let sig = Sig { item: F, params: ParamsView::of(&p), inputs: &apart, output: ty(unit, NONE), clauses: &[] };
    let (res, rec) = run(&cols, &Sel::default(), &Own::default(), 2, Callee::Fn(sig), no_generics(), &[arg(r, 0, 1), arg(r, 1, 1)], arg(unit, 2, 0));
    assert_eq!(res, Ok(()));
    assert_eq!(set(&rec), pairs(&[(0, 100), (1, 101)]), "CONTROL: no nesting, no implied bound");
}

/// `fn pick<'a>(x: &'a u8) -> &'a u8` declared in the heads' table, called with values of the
/// body's table, which numbers its types otherwise: each side is read in its own table. CONTROL:
/// the declared types read in the body's table are other types (`&u16` where `&u8` was
/// declared), refused as a shape mismatch, never related.
#[test]
fn each_side_is_read_in_its_own_table() {
    let facts = Facts::default();
    let mut body = Tys::new();
    let u8_ = body.u8();
    let r = body.r(u8_);
    let u16_ = body.u16();
    let _r16 = body.r(u16_);
    let mut heads = Tys::new();
    let h16 = heads.u16();
    let _hr16 = heads.r(h16);
    let h8 = heads.u8();
    let hr = heads.r(h8);
    let p = params(1, &[0]);
    let inputs = [ty(hr, P0)];
    let sig = Sig { item: F, params: ParamsView::of(&p), inputs: &inputs, output: ty(hr, P0), clauses: &[] };
    let body_cols = Columns::new(body.t.view(), &facts, &facts);
    let head_cols = Columns::new(heads.t.view(), &facts, &facts);
    let (res, rec) = run_in(&body_cols, &head_cols, &Sel::default(), &Own::default(), 2, Callee::Fn(sig), no_generics(), &[arg(r, 0, 1)], arg(r, 1, 1));
    assert_eq!(res, Ok(()));
    assert_eq!(set(&rec), pairs(&[(0, 100), (100, 1)]));

    let (res, _) = run(&body_cols, &Sel::default(), &Own::default(), 2, Callee::Fn(sig), no_generics(), &[arg(r, 0, 1)], arg(r, 1, 1));
    assert!(matches!(res, Err(Refused::Shape { item: F, .. })), "CONTROL: one table for both sides: {res:?}");
}

/// `fn f<'a, T: Tr<'a>>(t: T)` ('a early: slot 0) called with `T = Wrap<'1>` (slot 1), the
/// argument's `Wrap` slot 2, `impl<'x> Tr<'x> for Wrap<'x>` selected. The clause is given as
/// its column lays it, subject first (`ExTrait(Tr, [T])` over `['a]`): the impl ties `'a` to
/// the call's `T`'s region both ways. CONTROL: the same node handed as the impl's form
/// (`ClauseIn::Trait`, which reads no subject off it) does not walk in step with the header.
#[test]
fn a_subject_first_clause_ties_through_the_impl() {
    let mut t = Tys::new();
    let unit = t.unit();
    let p0 = t.param(0);
    let wrap = t.adt(K_WRAP, &[]);
    let clause = t.extrait(T_TR, &[p0]);
    let header = t.extrait(T_TR, &[wrap]);
    let facts = Facts::default().trait_(T_TR, 1).adt(K_WRAP, 1, &[Variance::Co]);
    let head = ImplHead { imp: IMP, lifetimes: 1, types: 0, trait_ref: ty(header, P00) };
    let sel = Sel { rows: SVec::from([(T_TR, None, Selection::Impl { head, value: None })]) };
    let p = params(1, &[]);
    let inputs = [ty(p0, NONE)];
    let clauses = [ClauseIn::TraitOf { trait_ref: ty(clause, P0) }];
    let sig = Sig { item: F, params: ParamsView::of(&p), inputs: &inputs, output: ty(unit, NONE), clauses: &clauses };
    let types = [arg(wrap, 1, 1)];
    let generics = GenericArgs { lifetimes: range(0, 1), types: &types };
    let cols = Columns::new(t.t.view(), &facts, &facts);
    let (res, rec) = run(&cols, &sel, &Own::default(), 3, Callee::Fn(sig), generics, &[arg(wrap, 2, 1)], arg(unit, 3, 0));
    assert_eq!(res, Ok(()));
    assert_eq!(set(&rec), pairs(&[(2, 1), (0, 1), (1, 0)]));

    let as_impl_form = [ClauseIn::Trait { subject: ty(p0, NONE), trait_ref: ty(clause, P0) }];
    let sig = Sig { item: F, params: ParamsView::of(&p), inputs: &inputs, output: ty(unit, NONE), clauses: &as_impl_form };
    let (res, _) = run(&cols, &sel, &Own::default(), 3, Callee::Fn(sig), generics, &[arg(wrap, 2, 1)], arg(unit, 3, 0));
    assert!(matches!(res, Err(Refused::Shape { item: F, .. })), "CONTROL: the subject-first node read as the impl's form: {res:?}");
}

/// An alias whose impl was selected but whose header regions were not read is refused naming
/// why ([`NotSelected::HeadUnread`]), never normalized by a guess.
#[test]
fn an_unread_impl_header_is_refused_by_name() {
    let mut t = Tys::new();
    let u8_ = t.u8();
    let r = t.r(u8_);
    let p0 = t.param(0);
    let wrap = t.adt(K_WRAP, &[]);
    let ok = t.alias(T_TR, OK, &[p0]);
    let facts = Facts::default().trait_(T_TR, 0).alias(T_TR, OK, 0).adt(K_WRAP, 1, &[Variance::Co]);
    let sel = Sel { rows: SVec::from([(T_TR, Some(OK), Selection::Not(NotSelected::HeadUnread))]) };
    let p = params(0, &[]);
    let inputs = [ty(p0, NONE)];
    let sig = Sig { item: F_RESULT, params: ParamsView::of(&p), inputs: &inputs, output: ty(ok, NONE), clauses: &[] };
    let types = [arg(wrap, 0, 1)];
    let cols = Columns::new(t.t.view(), &facts, &facts);
    let generics = GenericArgs { lifetimes: SlotRange::EMPTY, types: &types };
    let (res, _) = run(&cols, &sel, &Own::default(), 3, Callee::Fn(sig), generics, &[arg(wrap, 1, 1)], arg(r, 2, 1));
    assert_eq!(res, Err(Refused::Unselected { item: F_RESULT, node: ok, trait_: T_TR, name: Some(OK), why: NotSelected::HeadUnread }));
}

// ---------------------------------------------------------------------------------------------
// The shapes the flow lowering's oracle found (D156, wave-1119 t13), each as the contract reads
// it: a trait method's `Self: Trait<'params>` clause laid subject-first, the caller's bound or
// the selected impl proving it.

/// `trait Conv<'a, 'b> { fn conv(self, x: &'a u8) -> &'b u8; }` called as `t.conv(x)` from
/// `fn good<'a, 'b, T: Conv<'a, 'b>>(t: T, x: &'a u8) -> &'b u8` (the flow lowering's
/// trait_ties_tests.rs `a_trait_lifetime_is_the_callers`). Slots: 0, 1 the call's `'a`, `'b`; 2
/// `x`'s; 3 the destination's. The clause `Self: Conv<'a, 'b>` ties the call's `'a`, `'b` to
/// the caller's (Origins 10, 11) in the bound's order: `x` reaches `'a` only. With the bound
/// `T: Conv<'b, 'a>` (`swapped`) `x` reaches `'b`, which `x: &'a u8` does not give (rustc:
/// "lifetime may not live long enough").
/// CONTROL (rule 23): without the clause the call's lifetimes are free and `x` reaches neither.
#[test]
fn a_trait_methods_lifetimes_are_the_callers_bounds() {
    let mut t = Tys::new();
    let u8_ = t.u8();
    let r = t.r(u8_);
    let p0 = t.param(0);
    let clause = t.extrait(T_CONV, &[p0]);
    let facts = Facts::default().trait_(T_CONV, 2);
    let p = params(2, &[]);
    let inputs = [ty(p0, NONE), ty(r, P0)];
    let tie = [ClauseIn::TraitOf { trait_ref: ty(clause, P01) }];
    let types = [arg(p0, 2, 0)];
    let generics = GenericArgs { lifetimes: range(0, 2), types: &types };
    let args = [arg(p0, 2, 0), arg(r, 2, 1)];
    let cols = Columns::new(t.t.view(), &facts, &facts);
    let sig = Sig { item: METHOD, params: ParamsView::of(&p), inputs: &inputs, output: ty(r, P1), clauses: &tie };

    let good = Own { bounds: SVec::from([(0, T_CONV, ty(clause, P01))]) };
    let (res, rec) = run(&cols, &Sel::default(), &good, 4, Callee::Fn(sig), generics, &args, arg(r, 3, 1));
    assert_eq!(res, Ok(()));
    assert_eq!(set(&rec), pairs(&[(0, 10), (10, 0), (1, 11), (11, 1), (2, 0), (1, 3)]));
    let from_x = reach(&rec, 2);
    assert!(from_x.contains(&10) && !from_x.contains(&11), "good: `x` reaches `'a` only: {from_x:?}");

    let swapped = Own { bounds: SVec::from([(0, T_CONV, ty(clause, P10))]) };
    let (res, rec) = run(&cols, &Sel::default(), &swapped, 4, Callee::Fn(sig), generics, &args, arg(r, 3, 1));
    assert_eq!(res, Ok(()));
    assert_eq!(set(&rec), pairs(&[(0, 11), (11, 0), (1, 10), (10, 1), (2, 0), (1, 3)]));
    assert!(reach(&rec, 2).contains(&11), "swapped: `x` must outlive `'b`");

    let bare = Sig { clauses: &[], ..sig };
    let (res, rec) = run(&cols, &Sel::default(), &swapped, 4, Callee::Fn(bare), generics, &args, arg(r, 3, 1));
    assert_eq!(res, Ok(()));
    assert_eq!(set(&rec), pairs(&[(2, 0), (1, 3)]), "CONTROL: the clause is the cause of the tie");
}

/// Serde's shape: `trait Visitor<'de> { fn take(self, s: &'de u8); }` called as `v.take(&s)`
/// from `fn local<'de, V: Visitor<'de>>(v: V, s: u8)` (trait_ties_tests.rs
/// `a_visitors_lifetime_is_the_callers`). Slots: 0 the call's `'de`, 1 the borrow of the local
/// `s`. The caller writes `'de` only in its bound, so the borrow reaches the caller's `'de`
/// (Origin 10): rejected (E0597).
/// CONTROL (rule 23): without the clause the borrow reaches only the call's own `'de`.
#[test]
fn a_lifetime_the_caller_writes_only_in_its_bound_is_tied() {
    let mut t = Tys::new();
    let u8_ = t.u8();
    let r = t.r(u8_);
    let unit = t.unit();
    let p0 = t.param(0);
    let clause = t.extrait(T_VISITOR, &[p0]);
    let facts = Facts::default().trait_(T_VISITOR, 1);
    let p = params(1, &[]);
    let inputs = [ty(p0, NONE), ty(r, P0)];
    let tie = [ClauseIn::TraitOf { trait_ref: ty(clause, P0) }];
    let types = [arg(p0, 1, 0)];
    let generics = GenericArgs { lifetimes: range(0, 1), types: &types };
    let args = [arg(p0, 1, 0), arg(r, 1, 1)];
    let cols = Columns::new(t.t.view(), &facts, &facts);
    let own = Own { bounds: SVec::from([(0, T_VISITOR, ty(clause, P0))]) };
    let sig = Sig { item: METHOD, params: ParamsView::of(&p), inputs: &inputs, output: ty(unit, NONE), clauses: &tie };

    let (res, rec) = run(&cols, &Sel::default(), &own, 2, Callee::Fn(sig), generics, &args, arg(unit, 2, 0));
    assert_eq!(res, Ok(()));
    assert_eq!(set(&rec), pairs(&[(0, 10), (10, 0), (1, 0)]));
    assert!(rejects(&rec, &[1], &[10]), "the local's borrow reaches the caller's `'de`");

    let bare = Sig { clauses: &[], ..sig };
    let (res, rec) = run(&cols, &Sel::default(), &own, 2, Callee::Fn(bare), generics, &args, arg(unit, 2, 0));
    assert_eq!(res, Ok(()));
    assert!(!rejects(&rec, &[1], &[10]), "CONTROL: the clause is the cause of the rejection");
}

/// The tie has no answer: the subject is the caller's own `V` and the caller states no `V:
/// Visitor<..>` its regions are known for (trait_ties_tests.rs `a_tie_with_no_answer_is_refused`),
/// or the subject is a concrete `Plain` and the type checker filed no selection for the call
/// (impl_ties_tests.rs `no_selected_row_is_refused`). Each is refused by name, never related
/// with a fresh lifetime.
/// CONTROLS (rule 23): the caller's bound given, the call relates; the impl `impl
/// Visitor<'static> for Plain` selected, the call relates and the local's borrow must be
/// `'static`.
#[test]
fn a_tie_with_no_answer_is_refused_by_name() {
    let mut t = Tys::new();
    let u8_ = t.u8();
    let r = t.r(u8_);
    let unit = t.unit();
    let p0 = t.param(0);
    let plain = t.adt(K_PLAIN, &[]);
    let clause = t.extrait(T_VISITOR, &[p0]);
    let header = t.extrait(T_VISITOR, &[plain]);
    let facts = Facts::default().trait_(T_VISITOR, 1).adt(K_PLAIN, 0, &[]);
    let p = params(1, &[]);
    let inputs = [ty(p0, NONE), ty(r, P0)];
    let tie = [ClauseIn::TraitOf { trait_ref: ty(clause, P0) }];
    let cols = Columns::new(t.t.view(), &facts, &facts);
    let sig = Sig { item: METHOD, params: ParamsView::of(&p), inputs: &inputs, output: ty(unit, NONE), clauses: &tie };

    // The caller's own `V`, no bound.
    let types = [arg(p0, 1, 0)];
    let generics = GenericArgs { lifetimes: range(0, 1), types: &types };
    let args = [arg(p0, 1, 0), arg(r, 1, 1)];
    let (res, _) = run(&cols, &Sel::default(), &Own::default(), 2, Callee::Fn(sig), generics, &args, arg(unit, 2, 0));
    assert_eq!(res, Err(Refused::Unselected { item: METHOD, node: clause, trait_: T_VISITOR, name: None, why: NotSelected::NoBound }));
    let own = Own { bounds: SVec::from([(0, T_VISITOR, ty(clause, P0))]) };
    let (res, _) = run(&cols, &Sel::default(), &own, 2, Callee::Fn(sig), generics, &args, arg(unit, 2, 0));
    assert_eq!(res, Ok(()), "CONTROL: the bound given");

    // A concrete `Plain`, no selection.
    let types = [arg(plain, 1, 0)];
    let generics = GenericArgs { lifetimes: range(0, 1), types: &types };
    let args = [arg(plain, 1, 0), arg(r, 1, 1)];
    let (res, _) = run(&cols, &Sel::default(), &Own::default(), 2, Callee::Fn(sig), generics, &args, arg(unit, 2, 0));
    assert_eq!(res, Err(Refused::Unselected { item: METHOD, node: clause, trait_: T_VISITOR, name: None, why: NotSelected::Unanswered }));
    let head = ImplHead { imp: IMP, lifetimes: 0, types: 0, trait_ref: ty(header, STATIC) };
    let sel = Sel { rows: SVec::from([(T_VISITOR, None, Selection::Impl { head, value: None })]) };
    let (res, rec) = run(&cols, &sel, &Own::default(), 2, Callee::Fn(sig), generics, &args, arg(unit, 2, 0));
    assert_eq!(res, Ok(()), "CONTROL: the impl selected");
    assert_eq!(rec.statics, [0], "`Visitor<'static>`: the call's `'de` is `'static`");
    assert!(rejects(&rec, &[1], &[]), "the local's borrow must be `'static`");
}

/// `fn keep(h: Hold<'static>)` with `Hold<'a>` invariant (sig_alias_tests.rs
/// `an_alias_input_demands_static_of_its_argument`: the alias laid as `['static]`): relating the
/// argument's `'0` invariantly with `'static` is `'0: 'static` and `'static: '0`; the second
/// holds of every region, so the rule emits only the first (relate.rs `outlives`, `(At::Static,
/// _)`), and the local's borrow must be `'static`.
/// CONTROL (rule 23): `Hold` contravariant, only `'static: '0` is wanted, nothing is emitted and
/// the borrow is accepted: the variance is the cause.
#[test]
fn static_in_an_invariant_input_is_one_outlives_static() {
    let mut t = Tys::new();
    let unit = t.unit();
    let hold = t.adt(K_HOLD, &[]);
    let p = params(0, &[]);
    let inputs = [ty(hold, STATIC)];
    let sig = Sig { item: KEEP, params: ParamsView::of(&p), inputs: &inputs, output: ty(unit, NONE), clauses: &[] };

    let facts = Facts::default().adt(K_HOLD, 1, &[Variance::In]);
    let cols = Columns::new(t.t.view(), &facts, &facts);
    let (res, rec) = run(&cols, &Sel::default(), &Own::default(), 1, Callee::Fn(sig), no_generics(), &[arg(hold, 0, 1)], arg(unit, 1, 0));
    assert_eq!(res, Ok(()));
    assert!(rec.subsets.is_empty(), "`'static: '0` is emitted as nothing: {:?}", rec.subsets);
    assert_eq!(rec.statics, [0]);
    assert!(rejects(&rec, &[0], &[]));

    let facts = Facts::default().adt(K_HOLD, 1, &[Variance::Contra]);
    let cols = Columns::new(t.t.view(), &facts, &facts);
    let (res, rec) = run(&cols, &Sel::default(), &Own::default(), 1, Callee::Fn(sig), no_generics(), &[arg(hold, 0, 1)], arg(unit, 1, 0));
    assert_eq!(res, Ok(()));
    assert!(rec.statics.is_empty() && rec.subsets.is_empty(), "CONTROL: contravariant, nothing wanted of the argument");
}

/// The stub's rows, and one selected impl's where-clauses ([`ImplWhere`]).
struct WhereSel<'s> {
    sel: Sel<'s>,
    imp: ItemKey,
    wh: ImplWhere<'s>,
}

impl Selected for WhereSel<'_> {
    fn select(&self, ask: Ask) -> Selection<'_> {
        self.sel.select(ask)
    }
    fn impl_where(&self, imp: ItemKey) -> Result<ImplWhere<'_>, NotSelected> {
        Ok(if imp == self.imp { self.wh } else { ImplWhere::NONE })
    }
}

/// THE IMPL'S PROOF (D190 step 2 (iv)): `impl<'x> Visitor<'x> for Plain where 'x: 'static`
/// proves `Plain: Visitor<'de>` only where `'de: 'static` (the Reference, "Implementations":
/// an impl's where-clauses must hold where it is used), so the local's borrow passed as
/// `&'de u8` must be `'static`: rejected (E0597). Slots: 0 the call's `'de`, 1 the borrow.
/// TWIN (rule 23): the same impl with no where-clause relates `'de` to nothing and the borrow
/// is accepted: the where-clause is the cause.
#[test]
fn an_impls_where_clause_is_related_at_the_call() {
    let mut t = Tys::new();
    let u8_ = t.u8();
    let r = t.r(u8_);
    let unit = t.unit();
    let p0 = t.param(0);
    let plain = t.adt(K_PLAIN, &[]);
    let clause = t.extrait(T_VISITOR, &[p0]);
    let header = t.extrait(T_VISITOR, &[plain]);
    let facts = Facts::default().trait_(T_VISITOR, 1).adt(K_PLAIN, 0, &[]);
    let p = params(1, &[]);
    let inputs = [ty(p0, NONE), ty(r, P0)];
    let tie = [ClauseIn::TraitOf { trait_ref: ty(clause, P0) }];
    let cols = Columns::new(t.t.view(), &facts, &facts);
    let sig = Sig { item: METHOD, params: ParamsView::of(&p), inputs: &inputs, output: ty(unit, NONE), clauses: &tie };
    let types = [arg(plain, 1, 0)];
    let generics = GenericArgs { lifetimes: range(0, 1), types: &types };
    let args = [arg(plain, 1, 0), arg(r, 1, 1)];
    let head = ImplHead { imp: IMP, lifetimes: 1, types: 0, trait_ref: ty(header, P0) };
    let rows = || Sel { rows: SVec::from([(T_VISITOR, None, Selection::Impl { head, value: None })]) };

    // As heads_regions.rs `fill_clauses` files `'x: 'static`: in the parameters' outlives and
    // in the head's `outlives` (related once).
    const X_STATIC: &[(Region, Region)] = &[(Region::Param(0), Region::Static)];
    const X_STATIC_ROWS: &[Outlives] = &[Outlives::Region { long: Region::Param(0), short: Region::Static }];
    let bounded = WhereSel { sel: rows(), imp: IMP, wh: ImplWhere { params: X_STATIC, outlives: X_STATIC_ROWS, views: &[] } };
    let (res, rec) = run(&cols, &bounded, &Own::default(), 2, Callee::Fn(sig), generics, &args, arg(unit, 2, 0));
    assert_eq!(res, Ok(()));
    assert_eq!(rec.statics, [0], "`'x: 'static`, `'x` the call's `'de`: `'de: 'static`, once");
    assert!(rejects(&rec, &[1], &[]), "the local's borrow must be `'static`");

    let free = WhereSel { sel: rows(), imp: IMP, wh: ImplWhere::NONE };
    let (res, rec) = run(&cols, &free, &Own::default(), 2, Callee::Fn(sig), generics, &args, arg(unit, 2, 0));
    assert_eq!(res, Ok(()));
    assert!(rec.statics.is_empty(), "TWIN: no where-clause, nothing asked of `'de`: {:?}", rec.statics);
    assert!(!rejects(&rec, &[1], &[]), "TWIN: the borrow is accepted");
}

/// THE EMPTY-COLUMN RULE (relate.rs `inert`, `impl_where_in`). Rust's rule (the Reference,
/// "Implementations": an impl's where-clauses must hold for the instance it is used at; rustc
/// confirms the impl and proves its where-clauses there): `impl<A: Marker, B> Other for Wrap<A,
/// B>` with `impl Marker for &'static u8` proves `Wrap<&'x u8, u8>: Other` only where `'x:
/// 'static`. The clause `A: Marker` WRITES no region (its column is empty), yet its instance
/// holds `'x`: it was skipped for the empty column, so `need(Wrap(&local, 0))` was accepted
/// where rustc rejects it (E0597, "borrowed value does not live long enough"): LOOSER.
/// Reduced from the census's empty-column skip (front2 call_census.rs D190 step 2), slots: 0 the
/// local's borrow.
/// TWIN (rustc accepts): `Wrap<u8, &'x u8>`: `A = u8` names no region, so `u8: Marker` asks
/// nothing (no row needed) and the borrow is free.
/// NEGATIVE TWIN of the row: `Wrap<&'x u8, u8>` with no `Marker` row filed is refused by name
/// (`Unanswered`), never accepted.
/// CONTROL (rule 23): the impl's where-clause removed ([`ImplWhere::NONE`]), the borrow is
/// accepted again: the clause is the cause.
#[test]
fn an_impls_empty_column_clause_is_tied_where_it_names_a_region() {
    const T_MARKER: ItemKey = ItemKey(0x0309);
    const T_OTHER: ItemKey = ItemKey(0x030a);
    const IMP_MARKER: ItemKey = ItemKey(0x0602);
    let mut t = Tys::new();
    let u8_ = t.u8();
    let r = t.r(u8_);
    let unit = t.unit();
    let p0 = t.param(0);
    let p1 = t.param(1);
    let wrap_ab = t.adt(K_WRAP, &[p0, p1]);
    let wrap_ru = t.adt(K_WRAP, &[r, u8_]);
    let wrap_ur = t.adt(K_WRAP, &[u8_, r]);
    let other_clause = t.extrait(T_OTHER, &[p0]);
    let other_header = t.extrait(T_OTHER, &[wrap_ab]);
    let marker_clause = t.extrait(T_MARKER, &[p0]);
    let marker_header = t.extrait(T_MARKER, &[r]);
    let facts = Facts::default().trait_(T_OTHER, 0).trait_(T_MARKER, 0).adt(K_WRAP, 0, &[Variance::Co, Variance::Co]);
    let marker_col = blaze_typeck::regions::RegionCol::lay(t.t.view(), marker_clause, &facts, NONE, alloc()).expect("`A: Marker`'s column is empty");
    assert_eq!(marker_col.len(), 0, "`A: Marker` writes no region");
    let views = [blaze_typeck::region_facts::ClauseView { nth: 0, binders: 0, col: blaze_typeck::region_facts::ColView::of(&marker_col), outlives: None }];
    let where_marker = ImplWhere { params: &[], outlives: &[], views: &views };
    let cols = Columns::new(t.t.view(), &facts, &facts);

    // `fn need<T: Other>(t: T)`.
    let p = params(0, &[]);
    let inputs = [ty(p0, NONE)];
    let tie = [ClauseIn::TraitOf { trait_ref: ty(other_clause, NONE) }];
    let sig = Sig { item: METHOD, params: ParamsView::of(&p), inputs: &inputs, output: ty(unit, NONE), clauses: &tie };
    let other = Selection::Impl { head: ImplHead { imp: IMP, lifetimes: 0, types: 2, trait_ref: ty(other_header, NONE) }, value: None };
    let marker = Selection::Impl { head: ImplHead { imp: IMP_MARKER, lifetimes: 0, types: 0, trait_ref: ty(marker_header, STATIC) }, value: None };
    let both = || Sel { rows: SVec::from([(T_OTHER, None, other), (T_MARKER, None, marker)]) };
    let call = |sel: &dyn Selected, w: PTy| {
        let types = [arg(w, 0, 1)];
        let generics = GenericArgs { lifetimes: SlotRange::EMPTY, types: &types };
        run(&cols, sel, &Own::default(), 1, Callee::Fn(sig), generics, &[arg(w, 0, 1)], arg(unit, 1, 0))
    };

    let (res, rec) = call(&WhereSel { sel: both(), imp: IMP, wh: where_marker }, wrap_ru);
    assert_eq!(res, Ok(()));
    assert_eq!(rec.statics, [0], "`&'x u8: Marker` by `impl Marker for &'static u8`: `'x: 'static`");
    assert!(rejects(&rec, &[0], &[]), "the local's borrow must be `'static` (rustc E0597)");

    let only_other = Sel { rows: SVec::from([(T_OTHER, None, other)]) };
    let (res, rec) = call(&WhereSel { sel: only_other, imp: IMP, wh: where_marker }, wrap_ur);
    assert_eq!(res, Ok(()), "TWIN: `u8: Marker` names no region and asks no row");
    assert!(rec.statics.is_empty() && !rejects(&rec, &[0], &[]), "TWIN: the borrow in `B` is free: {:?}", rec.statics);

    let only_other = Sel { rows: SVec::from([(T_OTHER, None, other)]) };
    let (res, _) = call(&WhereSel { sel: only_other, imp: IMP, wh: where_marker }, wrap_ru);
    assert!(
        matches!(res, Err(Refused::Unselected { trait_, why: NotSelected::Unanswered, .. }) if trait_ == T_MARKER),
        "NEGATIVE TWIN: `&'x u8: Marker` unanswered is refused by name, never skipped: {res:?}"
    );

    let (res, rec) = call(&WhereSel { sel: both(), imp: IMP, wh: ImplWhere::NONE }, wrap_ru);
    assert_eq!(res, Ok(()));
    assert!(rec.statics.is_empty() && !rejects(&rec, &[0], &[]), "CONTROL: no where-clause, nothing asked of `'x`: {:?}", rec.statics);
}

/// A PROJECTION CLAUSE WITH AN EMPTY COLUMN (relate.rs `proj_of`). Rust's rule (the Reference,
/// "Associated types"): `T: Tr<Output = U>` holds where `<T as Tr>::Output` normalizes to `U`;
/// with `U = &'x u8` the selected impl's value fixes `'x` (`I: Iterator<Item = U>` at `I =
/// Iter<'a, u8>` makes `'x` = `'a`), so the clause relates `U`'s region and was skipped for its
/// empty column: LOOSER. Now refused by name ([`NotSelected::WhereProjection`]) until the rule
/// relates it. TWIN: `U = u8` names no region: the equation relates nothing, accepted. CONTROL
/// (rule 23): the clause removed, `U = &'x u8` is accepted: the clause is the cause.
#[test]
fn a_projection_clause_naming_a_region_is_never_skipped() {
    let mut t = Tys::new();
    let u8_ = t.u8();
    let r = t.r(u8_);
    let unit = t.unit();
    let p0 = t.param(0);
    let p1 = t.param(1);
    let plain = t.adt(K_PLAIN, &[]);
    let proj = t.exproj(T_TR, OUTPUT, &[p0], p1);
    let facts = Facts::default().trait_(T_TR, 0).alias(T_TR, OUTPUT, 0).adt(K_PLAIN, 0, &[]);
    let cols = Columns::new(t.t.view(), &facts, &facts);
    // `fn need<T, U>(t: T, u: U) where T: Tr<Output = U>` (its parent `T: Tr` laid apart).
    let p = params(0, &[]);
    let inputs = [ty(p0, NONE), ty(p1, NONE)];
    let with = [ClauseIn::ProjOf { proj: ty(proj, NONE) }];
    let call = |u: PTy, n: u32, clauses: &[ClauseIn<'static>]| {
        let sig = Sig { item: METHOD, params: ParamsView::of(&p), inputs: &inputs, output: ty(unit, NONE), clauses };
        let types = [arg(plain, 0, 0), arg(u, 0, n)];
        let generics = GenericArgs { lifetimes: SlotRange::EMPTY, types: &types };
        run(&cols, &Sel::default(), &Own::default(), n, Callee::Fn(sig), generics, &[arg(plain, 0, 0), arg(u, 0, n)], arg(unit, n, 0))
    };
    let (res, _) = call(r, 1, &with);
    assert_eq!(res, Err(Refused::Unselected { item: METHOD, node: proj, trait_: T_TR, name: Some(OUTPUT), why: NotSelected::Unanswered }));
    let (res, rec) = call(u8_, 0, &with);
    assert_eq!(res, Ok(()), "TWIN: `U = u8` names no region");
    assert!(rec.subsets.is_empty() && rec.statics.is_empty());
    let (res, _) = call(r, 1, &[]);
    assert_eq!(res, Ok(()), "CONTROL: without the clause the call relates");
}

/// A BUILTIN PROOF TIES NOTHING: `V: Marker` (no lifetime, no argument past `Self`) proved by a
/// builtin rule ([`NotSelected::Builtin`]: ground, no impl, the caller assumes only `Sized`)
/// relates nothing and is not refused (the Reference, "Special types and traits": a builtin
/// impl puts no region requirement on its subject).
/// TWIN: `V: Visitor<'de>` answered `Builtin` has a lifetime no builtin rule binds: refused by
/// name, never related as if free.
#[test]
fn a_builtin_proof_ties_only_what_names_nothing() {
    let mut t = Tys::new();
    let unit = t.unit();
    let p0 = t.param(0);
    let plain = t.adt(K_PLAIN, &[]);
    let marker = t.extrait(T_TR, &[p0]);
    let visitor = t.extrait(T_VISITOR, &[p0]);
    let facts = Facts::default().trait_(T_TR, 0).trait_(T_VISITOR, 1).adt(K_PLAIN, 0, &[]);
    let cols = Columns::new(t.t.view(), &facts, &facts);
    let builtin = |k: ItemKey| Sel { rows: SVec::from([(k, None, Selection::Not(NotSelected::Builtin))]) };

    let p = params(0, &[]);
    let inputs = [ty(p0, NONE)];
    let tie = [ClauseIn::TraitOf { trait_ref: ty(marker, NONE) }];
    let sig = Sig { item: METHOD, params: ParamsView::of(&p), inputs: &inputs, output: ty(unit, NONE), clauses: &tie };
    let types = [arg(plain, 0, 0)];
    let generics = GenericArgs { lifetimes: SlotRange::EMPTY, types: &types };
    let (res, rec) = run(&cols, &builtin(T_TR), &Own::default(), 0, Callee::Fn(sig), generics, &[arg(plain, 0, 0)], arg(unit, 0, 0));
    assert_eq!(res, Ok(()), "`Plain: Marker` by a builtin rule");
    assert!(rec.subsets.is_empty() && rec.statics.is_empty(), "nothing tied: {:?}", rec.subsets);

    let p = params(1, &[]);
    let tie = [ClauseIn::TraitOf { trait_ref: ty(visitor, P0) }];
    let sig = Sig { item: METHOD, params: ParamsView::of(&p), inputs: &inputs, output: ty(unit, NONE), clauses: &tie };
    let types = [arg(plain, 1, 0)];
    let generics = GenericArgs { lifetimes: range(0, 1), types: &types };
    let (res, _) = run(&cols, &builtin(T_VISITOR), &Own::default(), 1, Callee::Fn(sig), generics, &[arg(plain, 1, 0)], arg(unit, 1, 0));
    assert_eq!(res, Err(Refused::Unselected { item: METHOD, node: visitor, trait_: T_VISITOR, name: None, why: NotSelected::Builtin }), "TWIN");
}

/// ALPHA RENAMING (D190 step 2 (iii)): `impl Tr<for<'a> fn(&'a u8)> for Plain` proves `V:
/// Tr<for<'b> fn(&'b u8)>`. The header and the clause, both laid subject-first, each number
/// their binder 2 (the binding node's pre-order index in its own column), but in two
/// environments (the impl's, the callee's: `BoundAt` is keyed by both), so they are two
/// binders; the two are one type (the Reference, "Subtyping and Variance"): related, nothing
/// emitted.
/// TWIN: `for<'a, 'b> fn(&'a u8, &'b u8)` against `for<'c> fn(&'c u8, &'c u8)` is no renaming
/// (`'c` met with two partners): refused by name.
#[test]
fn bound_regions_pair_by_renaming_not_by_index() {
    let mut t = Tys::new();
    let u8_ = t.u8();
    let r = t.r(u8_);
    let unit = t.unit();
    let p0 = t.param(0);
    let plain = t.adt(K_PLAIN, &[]);
    let one = t.fnptr(&[r, unit]);
    let two = t.fnptr(&[r, r, unit]);
    let clause_one = t.extrait(T_TR, &[p0, one]);
    let header_one = t.extrait(T_TR, &[plain, one]);
    let clause_two = t.extrait(T_TR, &[p0, two]);
    let header_two = t.extrait(T_TR, &[plain, two]);
    let facts = Facts::default().trait_(T_TR, 0).adt(K_PLAIN, 0, &[]);
    let cols = Columns::new(t.t.view(), &facts, &facts);
    let p = params(0, &[]);
    let inputs = [ty(p0, NONE)];
    let types = [arg(plain, 0, 0)];
    let generics = GenericArgs { lifetimes: SlotRange::EMPTY, types: &types };
    let args = [arg(plain, 0, 0)];
    let impl_of = |header: PTy, col: &'static [Region]| Sel {
        rows: SVec::from([(T_TR, None, Selection::Impl { head: ImplHead { imp: IMP, lifetimes: 0, types: 0, trait_ref: ty(header, col) }, value: None })]),
    };

    const L20: &[Region] = &[Region::Late { binder: 2, var: 0 }];
    let tie = [ClauseIn::TraitOf { trait_ref: ty(clause_one, L20) }];
    let sig = Sig { item: METHOD, params: ParamsView::of(&p), inputs: &inputs, output: ty(unit, NONE), clauses: &tie };
    let (res, rec) = run(&cols, &impl_of(header_one, L20), &Own::default(), 0, Callee::Fn(sig), generics, &args, arg(unit, 0, 0));
    assert_eq!(res, Ok(()), "the impl's binder 2 and the callee's binder 2 are one binder renamed");
    assert!(rec.subsets.is_empty() && rec.statics.is_empty(), "a renaming emits nothing: {:?}", rec.subsets);

    const L20_L20: &[Region] = &[Region::Late { binder: 2, var: 0 }, Region::Late { binder: 2, var: 0 }];
    const L20_L21: &[Region] = &[Region::Late { binder: 2, var: 0 }, Region::Late { binder: 2, var: 1 }];
    let tie2 = [ClauseIn::TraitOf { trait_ref: ty(clause_two, L20_L20) }];
    let sig2 = Sig { clauses: &tie2, ..sig };
    let (res, _) = run(&cols, &impl_of(header_two, L20_L21), &Own::default(), 0, Callee::Fn(sig2), generics, &args, arg(unit, 0, 0));
    assert!(matches!(res, Err(Refused::HigherRanked { item: METHOD, .. })), "TWIN: `'c` met with two partners: {res:?}");
}

/// A CONST generic argument relates nothing (the census: every `Shape` refusal on tempo was a
/// const value or const parameter against another, 40,394 calls). `Arr<&'0 u8, 32>` flows
/// into `Arr<&'1 u8, N>` (`struct Arr<T, const N: usize>`, covariant in `T`): only `'0: '1`,
/// and two const values with different bits (the instance's `32` against a head's `64` when
/// one table interned them apart) are no refusal either: the type checker proved the
/// arguments equal. NEGATIVE TWIN: a const against a TYPE (`Arr<&u8, 32>` against `Arr<&u8,
/// u8>`, a kind mismatch no checked program has) is still `Shape` with the heads named.
#[test]
fn a_const_argument_relates_nothing() {
    const K_ARR: ItemKey = ItemKey(0x040b);
    let mut t = Tys::new();
    let u8_ = t.u8();
    let r = t.r(u8_);
    let c32 = t.n(PNode::ConstValue { ty: u8_, bits: 32 });
    let c64 = t.n(PNode::ConstValue { ty: u8_, bits: 64 });
    let cn = t.n(PNode::ConstParam(0));
    let a = t.adt(K_ARR, &[r, c32]);
    let b_param = t.adt(K_ARR, &[r, cn]);
    let b_other = t.adt(K_ARR, &[r, c64]);
    let b_type = t.adt(K_ARR, &[r, u8_]);
    let facts = Facts::default().adt(K_ARR, 0, &[Variance::Co, Variance::In]);
    let cols = Columns::new(t.t.view(), &facts, &facts);
    let origins: SVec<Origin> = (0..2).map(Origin).collect();
    let go = |b: PTy| {
        let mut rec = Rec { subsets: SVec::new(), statics: SVec::new(), placeholders: SVec::new(), next: 100 };
        let res = {
            let own = Own::default();
            let sel = Sel::default();
            let mut cx = Cx { types: &cols, sig_types: &cols, selected: &sel, owner: &own, origins: &origins, out: &mut rec };
            crate::relate::relate(&mut cx, arg(a, 0, 1), arg(b, 1, 1), Variance::Co, OWNER, alloc())
        };
        (res, rec)
    };
    for (b, what) in [(b_param, "a const parameter"), (b_other, "another const value")] {
        let (res, rec) = go(b);
        assert_eq!(res, Ok(()), "{what}");
        assert_eq!(set(&rec), pairs(&[(0, 1)]), "{what}: only the reference's region");
        assert!(rec.statics.is_empty());
    }
    let (res, _) = go(b_type);
    assert_eq!(res, Err(Refused::Shape { item: OWNER, a: c32, b: u8_, why: crate::ShapeWhy::Heads { a: crate::Head::ConstValue, b: crate::Head::Uint } }), "TWIN: a const against a type");
}

/// `same_alias` requires erased-equal subjects (the `Iterator::chain` shape): `<Wrap<'0> as
/// Tr>::Ok` against `<Wrap<'1> as Tr>::Ok` relates `'0` and `'1` directly, while `<Wrap<'0> as
/// Tr>::Ok` against `<Plain as Tr>::Ok` (distinct subjects) is NOT treated as the same alias and
/// must normalize through the selected impls instead (refusing `Unselected` when none is given).
#[test]
fn same_alias_requires_erased_equal_subjects() {
    let mut t = Tys::new();
    let wrap = t.adt(K_WRAP, &[]);
    let plain = t.adt(K_PLAIN, &[]);
    let a_wrap = t.alias(T_TR, OK, &[wrap]);
    let a_plain = t.alias(T_TR, OK, &[plain]);
    let facts = Facts::default()
        .trait_(T_TR, 0)
        .alias(T_TR, OK, 0)
        .adt(K_WRAP, 1, &[Variance::Co])
        .adt(K_PLAIN, 0, &[]);
    let cols = Columns::new(t.t.view(), &facts, &facts);
    let origins: SVec<Origin> = (0..2).map(Origin).collect();
    let go = |lhs: Arg, rhs: Arg| {
        let mut rec = Rec { subsets: SVec::new(), statics: SVec::new(), placeholders: SVec::new(), next: 100 };
        let res = {
            let own = Own::default();
            let sel = Sel::default();
            let mut cx = Cx { types: &cols, sig_types: &cols, selected: &sel, owner: &own, origins: &origins, out: &mut rec };
            crate::relate::relate(&mut cx, lhs, rhs, Variance::Co, OWNER, alloc())
        };
        (res, rec)
    };

    let (res, rec) = go(arg(a_wrap, 0, 1), arg(a_wrap, 1, 1));
    assert_eq!(res, Ok(()), "identical alias subjects relate directly");
    assert_eq!(set(&rec), pairs(&[(0, 1), (1, 0)]), "alias args are invariant");

    let (res, _) = go(arg(a_wrap, 0, 1), arg(a_plain, 1, 0));
    assert!(
        matches!(res, Err(Refused::Unselected { .. })),
        "CONTROL: distinct subjects must NOT be conflated by same_alias: {res:?}"
    );
}

/// Associated-type item bounds (`trait Pattern { type Searcher<'a>: Searcher<'a>; }`) supplied via
/// `Selected::assoc_clauses` prove a trait clause on an alias `<P as Pattern>::Searcher<'a>` and
/// tie the trait's lifetime parameter `'a`.
/// NEGATIVE CONTROL (rule 23): without `assoc_clauses`, the alias has no proof and is refused
/// `Unselected`.
#[test]
fn assoc_item_bounds_prove_alias_trait_clauses() {
    use blaze_typeck::region_facts::{ClauseView, ColView};

    const T_PAT: ItemKey = ItemKey(0x0501);
    const T_SEARCH: ItemKey = ItemKey(0x0502);
    const SEARCHER: Name = Name::of("Searcher");

    let mut t = Tys::new();
    let u8_ = t.u8();
    let r = t.r(u8_);
    let p0 = t.param(0);
    let searcher_alias = t.alias(T_PAT, SEARCHER, &[p0]);
    let callee_clause = t.extrait(T_SEARCH, &[searcher_alias]);
    // Inside `trait Pattern { type Searcher<'a>: Searcher<'a>; }`, the item bound's subject is
    // `<Self as Pattern>::Searcher<'a>` (with `Self = Param(0)`, `'a = Region::Param(0)`).
    let item_bound_ty = callee_clause;
    let facts = Facts::default()
        .trait_(T_PAT, 0)
        .trait_(T_SEARCH, 1)
        .alias(T_PAT, SEARCHER, 1);
    let cols = Columns::new(t.t.view(), &facts, &facts);

    struct AssocSel<'s> {
        bound: Option<(u32, [ClauseView<'s>; 1])>,
    }
    impl Selected for AssocSel<'_> {
        fn select(&self, _: Ask) -> Selection<'_> {
            Selection::Not(NotSelected::GenericSelf(crate::inst::SelfHead::Alias))
        }
        fn assoc_clauses(&self, trait_: ItemKey, name: Name) -> Option<(u32, &[ClauseView<'_>])> {
            if trait_ == T_PAT && name == SEARCHER {
                self.bound.as_ref().map(|(n, c)| (*n, &c[..]))
            } else {
                None
            }
        }
    }

    // `fn run<'a, P: Pattern>(s: P::Searcher<'a>, x: &'a u8) where P::Searcher<'a>: Searcher<'a>`
    // Called with `P = Param(0)`, `'a = slot 0`, `s: P::Searcher<'1>`, `x: &'2 u8`.
    let p = params(1, &[]);
    let inputs = [ty(searcher_alias, P0), ty(r, P0)];
    let clauses = [ClauseIn::TraitOf { trait_ref: ty(callee_clause, P00) }];
    let sig = Sig { item: METHOD, params: ParamsView::of(&p), inputs: &inputs, output: ty(r, P0), clauses: &clauses };
    let types = [arg(p0, 0, 0)];
    let generics = GenericArgs { lifetimes: range(0, 1), types: &types };
    let args = [arg(searcher_alias, 1, 1), arg(r, 2, 1)];
    let dest = arg(r, 3, 1);

    let item_col = blaze_typeck::regions::RegionCol::lay(t.t.view(), item_bound_ty, &facts, P00, alloc()).expect("item bound laid");
    let cv = ClauseView { nth: 0, binders: 0, col: ColView::of(&item_col), outlives: None };
    let sel_with = AssocSel { bound: Some((1, [cv])) };
    let (res, rec) = run(&cols, &sel_with, &Own::default(), 4, Callee::Fn(sig), generics, &args, dest);
    assert_eq!(res, Ok(()), "assoc item bound proves P::Searcher<'a>: Searcher<'a>: {res:?}");
    assert!(reach(&rec, 1).contains(&0) && reach(&rec, 0).contains(&1), "slot 1 tied to 'a (slot 0): {:?}", rec.subsets);

    let sel_without = AssocSel { bound: None };
    let (res, _) = run(&cols, &sel_without, &Own::default(), 4, Callee::Fn(sig), generics, &args, dest);
    assert!(matches!(res, Err(Refused::Unselected { trait_: T_SEARCH, .. })), "CONTROL: without assoc_clauses, refused Unselected: {res:?}");
}

/// Higher-ranked clause instantiation (`BinderCtx::Clause`):
/// - Positive: a caller with `T: for<'a> Tr<&'a u8>` proves a callee's `T: Tr<&'x u8>` by
///   instantiating the caller's `for<'a>` binder with a fresh existential tied to `'x`.
/// - Negative control (rule 23): a caller with `T: Tr<&'y u8>` (a fixed universal `'y`, no binder)
///   proves `T: for<'b> Tr<&'b u8>` ONLY by forcing a fresh placeholder `'b` to relate to `'y`,
///   which the caller rejects (`rejects` is true: placeholder reaches universal `'y`).
#[test]
fn higher_ranked_clause_instantiation_and_negative_control() {
    use blaze_typeck::region_facts::{ClauseView, ColView};

    let mut t = Tys::new();
    let u8_ = t.u8();
    let r = t.r(u8_);
    let unit = t.unit();
    let p0 = t.param(0);
    let tr_r = t.extrait(T_TR, &[p0, r]);
    let facts = Facts::default().trait_(T_TR, 0);
    let cols = Columns::new(t.t.view(), &facts, &facts);

    const L00: &[Region] = &[Region::Late { binder: 0, var: 0 }];

    struct OwnClauses<'s> {
        views: [ClauseView<'s>; 1],
    }
    impl Owner for OwnClauses<'_> {
        fn item(&self) -> ItemKey { OWNER }
        fn universal(&self, r: Region) -> Option<Origin> {
            match r { Region::Param(i) => Some(Origin(10 + i)), _ => None }
        }
        fn param(&self, p: u32) -> Option<Origin> { Some(Origin(20 + p)) }
        fn bound(&self, _: u32, _: ItemKey) -> Option<Typed<'_>> { None }
        fn clauses(&self) -> &[ClauseView<'_>] { &self.views }
        fn body_params(&self, _: PTy) -> Option<&[u32]> { None }
    }

    // Positive: caller has `Param(0): for<'a> Tr<&'a u8>`, callee demands `Param(0): Tr<&'x u8>`
    // where `'x` is slot 0 (instantiated by caller's local `arg(r, 1, 1)`).
    let p = params(1, &[]);
    let inputs = [ty(p0, NONE), ty(r, P0)];
    let callee_clauses = [ClauseIn::TraitOf { trait_ref: ty(tr_r, P0) }];
    let sig = Sig { item: METHOD, params: ParamsView::of(&p), inputs: &inputs, output: ty(unit, NONE), clauses: &callee_clauses };
    let types = [arg(p0, 0, 0)];
    let generics = GenericArgs { lifetimes: range(0, 1), types: &types };
    let args = [arg(p0, 0, 0), arg(r, 1, 1)];
    let hrtb_col = blaze_typeck::regions::RegionCol::lay(t.t.view(), tr_r, &facts, L00, alloc()).expect("hrtb clause laid");
    let own_hrtb = OwnClauses { views: [ClauseView { nth: 0, binders: 1, col: ColView::of(&hrtb_col), outlives: None }] };
    let (res, rec) = run(&cols, &Sel::default(), &own_hrtb, 2, Callee::Fn(sig), generics, &args, arg(unit, 2, 0));
    assert_eq!(res, Ok(()), "for<'a> Tr<&'a u8> proves Tr<&'x u8>: {res:?}");
    assert!(!rejects(&rec, &[1], &[10]), "local slot 1 does not reach any universal: {:?}", rec.subsets);

    // Negative control: caller only has `Param(0): Tr<&'y u8>` (`P0` -> universal `Origin(10)`),
    // while callee demands `Param(0): for<'b> Tr<&'b u8>` (`L00` -> rigid placeholder!).
    let p0_none = params(0, &[]);
    let inputs_hr = [ty(p0, NONE)];
    let clauses_hr = [ClauseIn::TraitOf { trait_ref: ty(tr_r, L00) }];
    let sig_hr = Sig { item: METHOD, params: ParamsView::of(&p0_none), inputs: &inputs_hr, output: ty(unit, NONE), clauses: &clauses_hr };
    let generics_hr = GenericArgs { lifetimes: SlotRange::EMPTY, types: &types };
    let args_hr = [arg(p0, 0, 0)];
    let fixed_col = blaze_typeck::regions::RegionCol::lay(t.t.view(), tr_r, &facts, P0, alloc()).expect("fixed clause laid");
    let own_fixed = OwnClauses { views: [ClauseView { nth: 0, binders: 0, col: ColView::of(&fixed_col), outlives: None }] };
    let (res, rec) = run(&cols, &Sel::default(), &own_fixed, 1, Callee::Fn(sig_hr), generics_hr, &args_hr, arg(unit, 1, 0));
    assert_eq!(res, Ok(()), "clause Skolemization emits placeholder constraint: {res:?}");
    assert!(!rec.placeholders.is_empty(), "callee's for<'b> must create a rigid placeholder");
    let ph = rec.placeholders[0];
    assert!(rejects(&rec, &[], &[ph, 10]), "CONTROL: fixed `'y` (Origin(10)) cannot satisfy `for<'b>` (placeholder {ph}): {:?}", rec.subsets);
}

/// `BinderCtx::Dynamic(In)` Skolemization: a `dyn for<'a> Tr<'a>` in an invariant position
/// (`&mut dyn for<'a> Tr<'a>`) Skolemizes the signature's higher-ranked binder to a rigid
/// placeholder when matched against a caller's `dyn Tr<'?0>` slot:
/// - Positive: an unconstrained existential slot on the caller side is accepted (no universal reached).
/// - Negative control (rule 23): a caller universal `'x` (`Origin(10)`) matched against the
///   Skolemized placeholder is rejected (universal reaches placeholder).
#[test]
fn dynamic_in_binder_skolemizes_rigidly_against_caller() {
    let mut t = Tys::new();
    let ex = t.extrait(T_TR, &[]);
    let preds = t.list(&[ex]);
    let dyn_ty = t.n(PNode::Dynamic(preds));
    let mut_dyn = t.n(PNode::Ref(Mutbl::Mut, dyn_ty));
    let unit = t.unit();
    let facts = Facts::default().trait_(T_TR, 1);
    let cols = Columns::new(t.t.view(), &facts, &facts);

    // Callee: `fn f<'r>(x: &'r mut (dyn for<'a> Tr<'a> + 'r))`
    // Slots on `mut_dyn`: `[outer_ref, dyn_lifetime, ex_trait_lifetime]`.
    const COL: &[Region] = &[Region::Param(0), Region::Param(0), Region::Late { binder: 1, var: 0 }];
    let p = params(1, &[]);
    let inputs = [ty(mut_dyn, COL)];
    let sig = Sig { item: METHOD, params: ParamsView::of(&p), inputs: &inputs, output: ty(unit, NONE), clauses: &[] };
    let generics = GenericArgs { lifetimes: range(0, 1), types: &[] };

    // Caller passes `&mut dyn Tr<'?2>` with slots `[1, 2, 3]` (all local existentials).
    let (res, rec) = run(&cols, &Sel::default(), &Own::default(), 4, Callee::Fn(sig), generics, &[arg(mut_dyn, 1, 3)], arg(unit, 4, 0));
    assert_eq!(res, Ok(()), "dyn for<'a> Tr<'a> under In variance lowers cleanly: {res:?}");
    assert!(!rec.placeholders.is_empty(), "Dynamic(In) must Skolemize Sig binder to a rigid placeholder");
    let ph = rec.placeholders[0];
    // Slot 3 (caller's predicate slot) is equated with the rigid placeholder `ph`:
    // if slot 3 were a caller universal (not existential), `rejects` catches it!
    assert!(reach(&rec, 3).contains(&ph) && reach(&rec, ph).contains(&3), "slot 3 must equate with placeholder {ph}: {:?}", rec.subsets);
    assert!(rejects(&rec, &[], &[3, ph]), "CONTROL: a caller universal at slot 3 cannot equal rigid placeholder {ph}");
}
