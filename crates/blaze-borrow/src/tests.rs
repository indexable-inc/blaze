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
    let clauses = [ClauseIn::TraitOf { trait_ref: ty(clause, P0), binders: 0 }];
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
    let tie = [ClauseIn::TraitOf { trait_ref: ty(clause, P01), binders: 0 }];
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
    let tie = [ClauseIn::TraitOf { trait_ref: ty(clause, P0), binders: 0 }];
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
    let tie = [ClauseIn::TraitOf { trait_ref: ty(clause, P0), binders: 0 }];
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
    let tie = [ClauseIn::TraitOf { trait_ref: ty(clause, P0), binders: 0 }];
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
    let tie = [ClauseIn::TraitOf { trait_ref: ty(other_clause, NONE), binders: 0 }];
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
    let with = [ClauseIn::ProjOf { proj: ty(proj, NONE), binders: 0 }];
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
    let tie = [ClauseIn::TraitOf { trait_ref: ty(marker, NONE), binders: 0 }];
    let sig = Sig { item: METHOD, params: ParamsView::of(&p), inputs: &inputs, output: ty(unit, NONE), clauses: &tie };
    let types = [arg(plain, 0, 0)];
    let generics = GenericArgs { lifetimes: SlotRange::EMPTY, types: &types };
    let (res, rec) = run(&cols, &builtin(T_TR), &Own::default(), 0, Callee::Fn(sig), generics, &[arg(plain, 0, 0)], arg(unit, 0, 0));
    assert_eq!(res, Ok(()), "`Plain: Marker` by a builtin rule");
    assert!(rec.subsets.is_empty() && rec.statics.is_empty(), "nothing tied: {:?}", rec.subsets);

    let p = params(1, &[]);
    let tie = [ClauseIn::TraitOf { trait_ref: ty(visitor, P0), binders: 0 }];
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
    let tie = [ClauseIn::TraitOf { trait_ref: ty(clause_one, L20), binders: 0 }];
    let sig = Sig { item: METHOD, params: ParamsView::of(&p), inputs: &inputs, output: ty(unit, NONE), clauses: &tie };
    let (res, rec) = run(&cols, &impl_of(header_one, L20), &Own::default(), 0, Callee::Fn(sig), generics, &args, arg(unit, 0, 0));
    assert_eq!(res, Ok(()), "the impl's binder 2 and the callee's binder 2 are one binder renamed");
    assert!(rec.subsets.is_empty() && rec.statics.is_empty(), "a renaming emits nothing: {:?}", rec.subsets);

    const L20_L20: &[Region] = &[Region::Late { binder: 2, var: 0 }, Region::Late { binder: 2, var: 0 }];
    const L20_L21: &[Region] = &[Region::Late { binder: 2, var: 0 }, Region::Late { binder: 2, var: 1 }];
    let tie2 = [ClauseIn::TraitOf { trait_ref: ty(clause_two, L20_L20), binders: 0 }];
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
    let clauses = [ClauseIn::TraitOf { trait_ref: ty(callee_clause, P00), binders: 0 }];
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
    let callee_clauses = [ClauseIn::TraitOf { trait_ref: ty(tr_r, P0), binders: 0 }];
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
    let clauses_hr = [ClauseIn::TraitOf { trait_ref: ty(tr_r, L00), binders: 1 }];
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

/// RFC 1214 implied alias outlives from `Owner::sig` (`blaze_own::vec::Index::index` shape):
/// `fn index<'a, T, I: SliceIndex<[T]>>(&'a self, index: I) -> &'a I::Output` calling
/// `<[T] as Index<I>>::index(&'call [T], index) -> &'call I::Output`.
/// - With `Owner::sig` carrying `&'a I::Output`, `I::Output: 'call` is proved via `'a: 'call`
///   without constraining `Param(1)` (`I`) at all.
/// - Negative control (rule 23): without `Owner::sig`, `I::Output: 'call` falls back to
///   demanding `Param(1): 'call` (`ρ_I: 'call`).
#[test]
fn implied_alias_outlives_from_owner_signature() {
    const F_INDEX: ItemKey = ItemKey(0x0211);
    const T_INDEX: ItemKey = ItemKey(0x0311);
    const T_SLICE_INDEX: ItemKey = ItemKey(0x0312);
    const OUTPUT: Name = Name::of("Output");

    let mut t = Tys::new();
    let p0 = t.param(0); // T
    let p1 = t.param(1); // I
    let slice_t = t.n(PNode::Slice(p0));
    let r_slice = t.r(slice_t);
    let idx_out = t.alias(T_INDEX, OUTPUT, &[p0, p1]);
    let slice_idx_out = t.alias(T_SLICE_INDEX, OUTPUT, &[p1, slice_t]);
    let r_idx_out = t.r(idx_out);
    let r_slice_idx_out = t.r(slice_idx_out);
    let sig_list = t.list(&[r_slice, p1, r_slice_idx_out]);
    let fn_sig_ty = t.n(PNode::FnPtr(sig_list, FnPtrHeader::RUST));

    // Impl `<[T] as Index<I>>::Output = <I as SliceIndex<[T]>>::Output`
    let header = t.extrait(T_INDEX, &[slice_t, p1]);
    let sel = Sel {
        rows: SVec::from([(
            T_INDEX,
            Some(OUTPUT),
            Selection::Impl {
                head: ImplHead { imp: IMP, lifetimes: 0, types: 2, trait_ref: ty(header, NONE) },
                value: Some(ImplValue { value: ty(slice_idx_out, NONE), gat_lifetimes: 0 }),
            },
        )]),
    };

    // Callee: `fn index<'0, Self, Idx>(&'0 self, i: Idx) -> &'0 <Self as Index<Idx>>::Output`
    // `'0` is late-bound (Param(0) marked late).
    let p = params(1, &[0]);
    let inputs = [ty(t.r(p0), P0), ty(p1, NONE)];
    let sig = Sig { item: F_INDEX, params: ParamsView::of(&p), inputs: &inputs, output: ty(r_idx_out, P0), clauses: &[] };
    let facts = Facts::default()
        .trait_(T_INDEX, 0)
        .alias(T_INDEX, OUTPUT, 0)
        .trait_(T_SLICE_INDEX, 0)
        .alias(T_SLICE_INDEX, OUTPUT, 0);
    let cols = Columns::new(t.t.view(), &facts, &facts);
    let types = [arg(slice_t, 0, 0), arg(p1, 0, 0)];
    let generics = GenericArgs { lifetimes: SlotRange::EMPTY, types: &types };
    // Caller slots: 0 is `&'a [T]` (owner universal `Origin(10)`), 1 is destination `&'a I::Output`.
    let args = [arg(r_slice, 0, 1), arg(p1, 0, 0)];
    let dest = arg(r_slice_idx_out, 1, 1);

    struct WithSig<'s> {
        sig: Option<Typed<'s>>,
    }
    impl Owner for WithSig<'_> {
        fn item(&self) -> ItemKey { OWNER }
        fn universal(&self, r: Region) -> Option<Origin> {
            (r == Region::Param(0)).then_some(Origin(10))
        }
        fn param(&self, p: u32) -> Option<Origin> { Some(Origin(20 + p)) }
        fn bound(&self, _: u32, _: ItemKey) -> Option<Typed<'_>> { None }
        fn sig(&self) -> Option<Typed<'_>> { self.sig }
        fn body_params(&self, _: PTy) -> Option<&[u32]> { None }
    }

    const SIG_COL: &[Region] = &[Region::Param(0), Region::Param(0)];
    let with_sig = WithSig { sig: Some(ty(fn_sig_ty, SIG_COL)) };
    let (res, rec) = run(&cols, &sel, &with_sig, 2, Callee::Fn(sig), generics, &args, dest);
    assert_eq!(res, Ok(()), "with implied Owner::sig bound: {res:?}");
    assert!(
        !rec.subsets.iter().any(|&(a, _)| a == 21),
        "I (Origin(21)) must NOT be constrained when Owner::sig implies `&'a I::Output`: {:?}",
        rec.subsets,
    );
    assert!(
        rec.subsets.iter().any(|&(a, _)| a == 10),
        "implied `'a` (Origin(10)) must constrain the call's lifetime: {:?}",
        rec.subsets,
    );

    // Negative control: without Owner::sig, `I` (Origin(21)) IS constrained.
    let without_sig = WithSig { sig: None };
    let (res, rec) = run(&cols, &sel, &without_sig, 2, Callee::Fn(sig), generics, &args, dest);
    assert_eq!(res, Ok(()));
    assert!(
        rec.subsets.iter().any(|&(a, _)| a == 21),
        "CONTROL: without Owner::sig, `I` (Origin(21)) must be constrained: {:?}",
        rec.subsets,
    );
}

/// `BinderCtx::Dynamic(Co)` Skolemization (`blaze_sim::check::replace_hook` shape):
/// passing `&dyn Tr<'?1>` to `fn f<'r>(x: &'r (dyn for<'a> Tr<'a> + 'r))` Skolemizes the
/// supertype's higher-ranked `dyn` binder via `Constraints::dyn_placeholder(ordinal)` rather than
/// an unmapped anonymous placeholder, so closures capturing `Box<dyn for<'a> Fn(&'a T)>` map the
/// constraint to `External::DynPlaceholder(ordinal)` while still rejecting caller universals.
#[test]
fn dynamic_co_binder_skolemizes_via_canonical_dyn_placeholder() {
    let mut t = Tys::new();
    let ex = t.extrait(T_TR, &[]);
    let preds = t.list(&[ex]);
    let dyn_ty = t.n(PNode::Dynamic(preds));
    let ref_dyn = t.n(PNode::Ref(Mutbl::Not, dyn_ty));
    let unit = t.unit();
    let facts = Facts::default().trait_(T_TR, 1);
    let cols = Columns::new(t.t.view(), &facts, &facts);

    const COL: &[Region] = &[Region::Param(0), Region::Param(0), Region::Late { binder: 1, var: 0 }];
    let p = params(1, &[]);
    let inputs = [ty(ref_dyn, COL)];
    let sig = Sig { item: METHOD, params: ParamsView::of(&p), inputs: &inputs, output: ty(unit, NONE), clauses: &[] };
    let generics = GenericArgs { lifetimes: range(0, 1), types: &[] };

    struct RecDyn {
        rec: Rec,
        dyn_ph: SVec<Origin>,
    }
    impl Constraints for RecDyn {
        fn subset(&mut self, a: Origin, b: Origin) { self.rec.subset(a, b); }
        fn outlives_static(&mut self, a: Origin) { self.rec.outlives_static(a); }
        fn fresh(&mut self, kind: Kind) -> Origin { self.rec.fresh(kind) }
        fn dyn_placeholder(&mut self, ordinal: u32) -> Origin {
            let idx = ordinal as usize;
            while self.dyn_ph.len() <= idx {
                let o = self.rec.fresh(Kind::Placeholder);
                self.dyn_ph.push(o);
            }
            self.dyn_ph[idx]
        }
    }

    let origins: SVec<Origin> = (0..4).map(Origin).collect();
    let mut out = RecDyn {
        rec: Rec { next: 100, subsets: SVec::new(), statics: SVec::new(), placeholders: SVec::new() },
        dyn_ph: SVec::new(),
    };
    let sel = Sel::default();
    let own = Own::default();
    let res = {
        let mut cx = Cx { types: &cols, sig_types: &cols, selected: &sel, owner: &own, origins: &origins, out: &mut out };
        call(&mut cx, Callee::Fn(sig), generics, &[arg(ref_dyn, 1, 3)], arg(unit, 4, 0), alloc())
    };
    assert_eq!(res, Ok(()), "shared `&dyn for<'a> Tr<'a>` lowers cleanly: {res:?}");
    assert_eq!(out.dyn_ph.len(), 1, "Dynamic(Co) must invoke dyn_placeholder(0)");
    let ph = out.dyn_ph[0].0;
    assert!(reach(&out.rec, 3).contains(&ph) && reach(&out.rec, ph).contains(&3), "slot 3 must equate with dyn_placeholder(0) ({ph}): {:?}", out.rec.subsets);
    assert!(rejects(&out.rec, &[], &[3, ph]), "CONTROL: a caller universal at slot 3 cannot equal rigid dyn_placeholder {ph}");
}

/// Reflexive projection equality (`blaze_format_core::Writer::new` -> `Writer::with_buffer` shape):
/// `fn callee<'a, P: Tr<'a, Out = B>, B>()` called with `B = <P as Tr<'b>>::Out` (no impl
/// selected: `P` is a caller type parameter) satisfies `<P as Tr<'a>>::Out == <P as Tr<'b>>::Out`
/// reflexively, equating `'a` and `'b` invariantly (`(0, 1)` and `(1, 0)`).
/// CONTROL: instantiating `B = <P as Tr<'b>>::Err` (a different associated type) is still
/// refused (`Unselected`), never falsely equated.
#[test]
fn reflexive_projection_equality_relates_invariantly() {
    let mut t = Tys::new();
    let unit = t.unit();
    let p0 = t.param(0);
    let p1 = t.param(1);
    let proj_clause = t.exproj(T_TR, OUTPUT, &[p0], p1);
    let alias_out = t.alias(T_TR, OUTPUT, &[p0]);
    let alias_err = t.alias(T_TR, ERR, &[p0]);
    let facts = Facts::default().trait_(T_TR, 1).alias(T_TR, OUTPUT, 1).alias(T_TR, ERR, 1);
    let cols = Columns::new(t.t.view(), &facts, &facts);

    let p = params(1, &[]);
    let clauses = [ClauseIn::ProjOf { proj: ty(proj_clause, P0), binders: 0 }];
    let sig = Sig { item: F, params: ParamsView::of(&p), inputs: &[], output: ty(unit, NONE), clauses: &clauses };

    let types = [arg(p0, 0, 0), arg(alias_out, 1, 1)];
    let generics = GenericArgs { lifetimes: range(0, 1), types: &types };
    let (res, rec) = run(&cols, &Sel::default(), &Own::default(), 2, Callee::Fn(sig), generics, &[], arg(unit, 2, 0));
    assert_eq!(res, Ok(()), "reflexive `<P as Tr<'a>>::Out == <P as Tr<'b>>::Out` must succeed: {res:?}");
    assert_eq!(set(&rec), pairs(&[(0, 1), (1, 0)]), "reflexive projection must equate `'a` and `'b` invariantly");
    assert!(rejects(&rec, &[1], &[0]), "local `'b` cannot satisfy universal `'a`");
    assert!(rejects(&rec, &[0], &[1]), "local `'a` cannot satisfy universal `'b`");

    let mismatch_types = [arg(p0, 0, 0), arg(alias_err, 1, 1)];
    let mismatch_gen = GenericArgs { lifetimes: range(0, 1), types: &mismatch_types };
    let (res, _) = run(&cols, &Sel::default(), &Own::default(), 2, Callee::Fn(sig), mismatch_gen, &[], arg(unit, 2, 0));
    assert!(matches!(res, Err(Refused::Unselected { .. })), "CONTROL: distinct associated types must not equate reflexively: {res:?}");
}

/// Higher-ranked clause Skolemization (`for<'a> Wrap<'a>: Visitor<'a>`):
/// a clause with `binders: 1` Skolemizes its `Late { binder: 0, var: 0 }` slots into a fresh
/// rigid placeholder so `impl<'a> Visitor<'a> for Wrap<'a>` succeeds with zero leaked constraints.
/// NEGATIVE CONTROL: matching the same `for<'a>` clause against `impl Visitor<'static> for
/// Wrap<'static>` demands `placeholder: 'static`, which is rejected (a universally quantified
/// lifetime cannot satisfy `'static`).
#[test]
fn higher_ranked_clause_skolemizes_and_rejects_static_escape() {
    let mut t = Tys::new();
    let unit = t.unit();
    let wrap = t.adt(K_WRAP, &[]);
    let clause_tr = t.extrait(T_VISITOR, &[wrap]);
    let facts = Facts::default().trait_(T_VISITOR, 1).adt(K_WRAP, 1, &[Variance::Co]);
    let cols = Columns::new(t.t.view(), &facts, &facts);

    const L00_L00: &[Region] = &[Region::Late { binder: 0, var: 0 }, Region::Late { binder: 0, var: 0 }];
    const P0_P0: &[Region] = &[Region::Param(0), Region::Param(0)];
    const ST_ST: &[Region] = &[Region::Static, Region::Static];

    let p = params(0, &[]);
    let clauses = [ClauseIn::TraitOf { trait_ref: ty(clause_tr, L00_L00), binders: 1 }];
    let sig = Sig { item: METHOD, params: ParamsView::of(&p), inputs: &[], output: ty(unit, NONE), clauses: &clauses };
    let generics = GenericArgs { lifetimes: SlotRange::EMPTY, types: &[] };

    let impl_poly = Sel {
        rows: SVec::from([(
            T_VISITOR,
            None,
            Selection::Impl { head: ImplHead { imp: IMP, lifetimes: 1, types: 0, trait_ref: ty(clause_tr, P0_P0) }, value: None },
        )]),
    };
    let (res, rec) = run(&cols, &impl_poly, &Own::default(), 0, Callee::Fn(sig), generics, &[], arg(unit, 0, 0));
    assert_eq!(res, Ok(()), "`for<'a> Wrap<'a>: Visitor<'a>` satisfied by `impl<'a> Visitor<'a> for Wrap<'a>`: {res:?}");
    assert!(!rec.placeholders.is_empty(), "clause binder must Skolemize to a rigid placeholder");
    assert!(rec.statics.is_empty(), "polymorphic impl must not demand `'static`: {:?}", rec.statics);

    let impl_static = Sel {
        rows: SVec::from([(
            T_VISITOR,
            None,
            Selection::Impl { head: ImplHead { imp: IMP, lifetimes: 0, types: 0, trait_ref: ty(clause_tr, ST_ST) }, value: None },
        )]),
    };
    let (res, rec) = run(&cols, &impl_static, &Own::default(), 0, Callee::Fn(sig), generics, &[], arg(unit, 0, 0));
    assert_eq!(res, Ok(()), "constraints emitted for borrow checker");
    let ph = rec.placeholders[0];
    assert!(rejects(&rec, &[], &[ph]), "NEGATIVE CONTROL: `for<'a>` placeholder {ph} cannot satisfy `'static` (statics={:?}, subsets={:?})", rec.statics, rec.subsets);
}

/// Associated type item outlives bound (`trait Database { type Error: 'static; }`):
/// calling `fn need_static<T: 'static>()` with `T = <DB as Database>::Error` is proved directly
/// from the associated type's own `Self: 'static` bound without forcing `DB: 'static`.
/// NEGATIVE CONTROL: without the associated type's `'static` bound, `<DB as Database>::Error:
/// 'static` falls back to components and demands `ρ_DB: 'static`.
#[test]
fn assoc_type_item_outlives_bound_proves_alias_static() {
    use blaze_typeck::region_facts::{ClauseView, ColView};
    use blaze_typeck::regions::RegionCol;

    let mut t = Tys::new();
    let unit = t.unit();
    let p0 = t.param(0);
    let alias_err = t.alias(T_TR, ERR, &[p0]);
    let facts = Facts::default().trait_(T_TR, 0).alias(T_TR, ERR, 0);
    let cols = Columns::new(t.t.view(), &facts, &facts);

    let p = params(0, &[]);
    let clauses = [ClauseIn::TypeOutlives { subject: ty(p0, NONE), region: Region::Static }];
    let sig = Sig { item: F, params: ParamsView::of(&p), inputs: &[], output: ty(unit, NONE), clauses: &clauses };
    let types = [arg(alias_err, 0, 0)];
    let generics = GenericArgs { lifetimes: SlotRange::EMPTY, types: &types };

    struct WithAssocOutlives<'a> {
        views: &'a [ClauseView<'a>],
    }
    impl Selected for WithAssocOutlives<'_> {
        fn select(&self, _: Ask) -> Selection<'_> {
            Selection::Not(NotSelected::Unanswered)
        }
        fn assoc_clauses(&self, trait_: ItemKey, name: Name) -> Option<(u32, &[ClauseView<'_>])> {
            (trait_ == T_TR && name == ERR).then_some((0, self.views))
        }
    }

    let col_self = RegionCol::lay(t.t.view(), alias_err, &facts, NONE, alloc()).expect("empty column");
    let views = [ClauseView { nth: 0, binders: 0, col: ColView::of(&col_self), outlives: Some(Region::Static) }];
    let sel_with = WithAssocOutlives { views: &views };

    let (res, rec) = run(&cols, &sel_with, &Own::default(), 0, Callee::Fn(sig), generics, &[], arg(unit, 0, 0));
    assert_eq!(res, Ok(()), "`type Error: 'static` proves `<DB as Tr>::Error: 'static`");
    assert!(rec.statics.is_empty(), "`DB` (param origin 20) must NOT be forced `'static`: {:?}", rec.statics);

    let (res, rec_neg) = run(&cols, &Sel::default(), &Own::default(), 0, Callee::Fn(sig), generics, &[], arg(unit, 0, 0));
    assert_eq!(res, Ok(()), "without item bound, falls back to components");
    assert!(rec_neg.statics.contains(&20), "NEGATIVE CONTROL: without `type Error: 'static`, `ρ_DB` (20) must outlive `'static`: {:?}", rec_neg.statics);
}

/// Opaque ancestor clause elaboration for `PNode::Alias` subjects (`test_tx_iterator_for_payload`
/// shape): `Opaque: SubTrait<'a>` where `SubTrait<'a>` declares `<Self as BaseTrait>::Out:
/// TargetTrait<'a>`. Proving `<Opaque as BaseTrait>::Out: TargetTrait<'b>` walks
/// `alias_self_chain` to `Opaque`, elaborates `Opaque`'s opaque clauses through `SubTrait<'a>`,
/// and equates `'a` with `'b` invariantly.
/// NEGATIVE CONTROL: `<Opaque as BaseTrait>::Err: TargetTrait<'b>` (not bounded by `SubTrait`)
/// is refused `Unselected`.
#[test]
fn opaque_ancestor_clause_elaborates_for_alias_subject() {
    use blaze_typeck::region_facts::{ClauseView, ColView};
    use blaze_typeck::regions::RegionCol;

    const K_OP: ItemKey = ItemKey(0x0901);
    const T_BASE: ItemKey = ItemKey(0x0902);
    const T_SUB: ItemKey = ItemKey(0x0903);
    const T_TARGET: ItemKey = ItemKey(0x0904);

    let mut t = Tys::new();
    let unit = t.unit();
    let p0 = t.param(0);
    let op = t.opaque(K_OP, &[]);
    let op_out = t.alias(T_BASE, OUTPUT, &[op]);
    let op_err = t.alias(T_BASE, ERR, &[op]);
    let self_out = t.alias(T_BASE, OUTPUT, &[p0]);
    let op_is_sub = t.extrait(T_SUB, &[op]);
    let self_out_is_target = t.extrait(T_TARGET, &[self_out]);
    let p0_is_target = t.extrait(T_TARGET, &[p0]);

    let facts = Facts::default()
        .opaque(K_OP, 1)
        .trait_(T_BASE, 0)
        .trait_(T_SUB, 1)
        .trait_(T_TARGET, 1)
        .alias(T_BASE, OUTPUT, 0)
        .alias(T_BASE, ERR, 0);
    let cols = Columns::new(t.t.view(), &facts, &facts);

    struct OpSel<'a> {
        op_views: &'a [ClauseView<'a>],
        sub_views: &'a [ClauseView<'a>],
    }
    impl Selected for OpSel<'_> {
        fn select(&self, _: Ask) -> Selection<'_> {
            Selection::Not(NotSelected::Unanswered)
        }
        fn opaque_clauses(&self, opaque: ItemKey) -> Option<(u32, &[ClauseView<'_>])> {
            (opaque == K_OP).then_some((1, self.op_views))
        }
        fn trait_clauses(&self, trait_: ItemKey) -> Option<(u32, &[ClauseView<'_>])> {
            (trait_ == T_SUB).then_some((1, self.sub_views))
        }
    }

    const P0_P0: &[Region] = &[Region::Param(0), Region::Param(0)];
    let op_col = RegionCol::lay(t.t.view(), op_is_sub, &facts, P0_P0, alloc()).unwrap();
    let sub_col = RegionCol::lay(t.t.view(), self_out_is_target, &facts, P0, alloc()).unwrap();
    let op_views = [ClauseView { nth: 0, binders: 0, col: ColView::of(&op_col), outlives: None }];
    let sub_views = [ClauseView { nth: 0, binders: 0, col: ColView::of(&sub_col), outlives: None }];
    let sel = OpSel { op_views: &op_views, sub_views: &sub_views };

    let p = params(1, &[]);
    let clauses = [ClauseIn::TraitOf { trait_ref: ty(p0_is_target, P0), binders: 0 }];
    let sig = Sig { item: METHOD, params: ParamsView::of(&p), inputs: &[], output: ty(unit, NONE), clauses: &clauses };

    let types_ok = [arg(op_out, 1, 1)];
    let (res, rec) = run(&cols, &sel, &Own::default(), 2, Callee::Fn(sig), GenericArgs { lifetimes: range(0, 1), types: &types_ok }, &[], arg(unit, 2, 0));
    assert_eq!(res, Ok(()), "`<Opaque<'1> as Base>::Out: Target<'0>` proved via `Opaque<'a>: Sub<'a>` elaboration: {res:?}");
    assert_eq!(set(&rec), pairs(&[(0, 1), (1, 0)]), "elaboration must equate `'0` and `'1` invariantly");

    let types_bad = [arg(op_err, 1, 1)];
    let (res, _) = run(&cols, &sel, &Own::default(), 2, Callee::Fn(sig), GenericArgs { lifetimes: range(0, 1), types: &types_bad }, &[], arg(unit, 2, 0));
    assert!(matches!(res, Err(Refused::Unselected { .. })), "NEGATIVE CONTROL: `<Opaque as Base>::Err` has no `Target` bound: {res:?}");
}

/// Higher-ranked clause over `dyn for<'a> Tr<'a>` Skolemizes via canonical `dyn_placeholder(ord)`
/// so passing the same `dyn for<'a> Tr<'a>` to two separate calls shares one placeholder instead
/// of equating two distinct rigid placeholders.
#[test]
fn dyn_clause_skolemization_shares_canonical_placeholder_across_calls() {
    let mut t = Tys::new();
    let unit = t.unit();
    let p0 = t.param(0);
    let pred = t.extrait(T_VISITOR, &[]);
    let dyn_tr = t.dynamic(&[pred]);
    let clause_tr = t.extrait(T_VISITOR, &[p0]);
    let facts = Facts::default().trait_(T_VISITOR, 1);
    let cols = Columns::new(t.t.view(), &facts, &facts);

    const L00: &[Region] = &[Region::Late { binder: 0, var: 0 }];
    let p = params(0, &[]);
    let clauses = [ClauseIn::TraitOf { trait_ref: ty(clause_tr, L00), binders: 1 }];
    let sig = Sig { item: METHOD, params: ParamsView::of(&p), inputs: &[], output: ty(unit, NONE), clauses: &clauses };

    struct RecDyn {
        rec: Rec,
        dyn_ph: SVec<Origin>,
    }
    impl Constraints for RecDyn {
        fn subset(&mut self, a: Origin, b: Origin) { self.rec.subset(a, b); }
        fn outlives_static(&mut self, a: Origin) { self.rec.outlives_static(a); }
        fn fresh(&mut self, kind: Kind) -> Origin { self.rec.fresh(kind) }
        fn dyn_placeholder(&mut self, ordinal: u32) -> Origin {
            let idx = ordinal as usize;
            while self.dyn_ph.len() <= idx {
                let o = self.rec.fresh(Kind::Placeholder);
                self.dyn_ph.push(o);
            }
            self.dyn_ph[idx]
        }
    }

    let origins: SVec<Origin> = (0..2).map(Origin).collect();
    let mut out = RecDyn {
        rec: Rec { next: 100, subsets: SVec::new(), statics: SVec::new(), placeholders: SVec::new() },
        dyn_ph: SVec::new(),
    };
    let sel = Sel::default();
    let own = Own::default();
    let types = [arg(dyn_tr, 0, 2)];
    let generics = GenericArgs { lifetimes: SlotRange::EMPTY, types: &types };
    for call_idx in 0..2 {
        let res = {
            let mut cx = Cx { types: &cols, sig_types: &cols, selected: &sel, owner: &own, origins: &origins, out: &mut out };
            call(&mut cx, Callee::Fn(sig), generics, &[], arg(unit, 2, 0), alloc())
        };
        assert_eq!(res, Ok(()), "call {call_idx} with `dyn for<'a> Tr<'a>` must succeed: {res:?}");
    }
    assert_eq!(out.dyn_ph.len(), 1, "both calls must share one canonical dyn_placeholder(0)");
    assert_eq!(out.rec.placeholders.len(), 1, "no second distinct rigid placeholder should be minted");
}

/// Target 1 (`test_tx_iterator_for_payload` shape): an opaque type `Opaque<'a, 'b>` with an
/// opaque bound `Opaque: SubTrait` whose supertrait is `BaseTrait: 'static` satisfies
/// `Opaque<'a, 'b>: 'static` without forcing its captured lifetimes `'a, 'b` to outlive `'static`.
///
/// NEGATIVE CONTROL: an opaque without any `'static` (direct or supertrait) bound MUST still
/// constrain its captured lifetimes `'a, 'b: 'static`.
#[test]
fn opaque_supertrait_static_outlives_does_not_constrain_captures() {
    use blaze_typeck::region_facts::{ClauseView, ColView};
    use blaze_typeck::regions::RegionCol;
    const K_OP_STATIC: ItemKey = ItemKey(880);
    const K_OP_PLAIN: ItemKey = ItemKey(881);
    const T_SUB: ItemKey = ItemKey(882);
    const T_BASE: ItemKey = ItemKey(883);

    let mut t = Tys::new();
    let unit = t.unit();
    let p0 = t.param(0);
    let op_static = t.opaque(K_OP_STATIC, &[]);
    let op_plain = t.opaque(K_OP_PLAIN, &[]);
    let op_is_sub = t.extrait(T_SUB, &[op_static]);
    let self_is_base = t.extrait(T_BASE, &[p0]);

    let facts = Facts::default()
        .opaque(K_OP_STATIC, 2)
        .opaque(K_OP_PLAIN, 2)
        .trait_(T_SUB, 0)
        .trait_(T_BASE, 0);
    let cols = Columns::new(t.t.view(), &facts, &facts);

    struct OpOutlivesSel<'a> {
        op_views: &'a [ClauseView<'a>],
        sub_views: &'a [ClauseView<'a>],
        base_views: &'a [ClauseView<'a>],
    }
    impl Selected for OpOutlivesSel<'_> {
        fn select(&self, _: Ask) -> Selection<'_> {
            Selection::Not(NotSelected::Unanswered)
        }
        fn opaque_clauses(&self, opaque: ItemKey) -> Option<(u32, &[ClauseView<'_>])> {
            (opaque == K_OP_STATIC).then_some((2, self.op_views))
        }
        fn trait_clauses(&self, trait_: ItemKey) -> Option<(u32, &[ClauseView<'_>])> {
            if trait_ == T_SUB {
                Some((0, self.sub_views))
            } else if trait_ == T_BASE {
                Some((0, self.base_views))
            } else {
                None
            }
        }
    }

    const P0_P1: &[Region] = &[Region::Param(0), Region::Param(1)];
    let op_col = RegionCol::lay(t.t.view(), op_is_sub, &facts, P0_P1, alloc()).unwrap();
    let sub_col = RegionCol::lay(t.t.view(), self_is_base, &facts, NONE, alloc()).unwrap();
    let base_self_col = RegionCol::lay(t.t.view(), p0, &facts, NONE, alloc()).unwrap();
    let op_views = [ClauseView { nth: 0, binders: 0, col: ColView::of(&op_col), outlives: None }];
    let sub_views = [ClauseView { nth: 0, binders: 0, col: ColView::of(&sub_col), outlives: None }];
    let base_views = [ClauseView { nth: 0, binders: 0, col: ColView::of(&base_self_col), outlives: Some(Region::Static) }];
    let sel = OpOutlivesSel { op_views: &op_views, sub_views: &sub_views, base_views: &base_views };

    // Callee: `fn require_static<T: 'static>(_x: T)`
    let p = params(0, &[]);
    let clauses = [ClauseIn::TypeOutlives { subject: ty(p0, NONE), region: Region::Static }];
    let sig = Sig { item: METHOD, params: ParamsView::of(&p), inputs: &[ty(p0, NONE)], output: ty(unit, NONE), clauses: &clauses };

    // Positive: `OpaqueStatic<'0, '1>: 'static` holds via `Sub -> Base: 'static`; ZERO `'static` on `'0`, `'1`.
    let types_ok = [arg(op_static, 0, 2)];
    let (res, rec) = run(&cols, &sel, &Own::default(), 2, Callee::Fn(sig), GenericArgs { lifetimes: SlotRange::EMPTY, types: &types_ok }, &[arg(op_static, 0, 2)], arg(unit, 2, 0));
    assert_eq!(res, Ok(()), "opaque with supertrait `'static` must satisfy `T: 'static`: {res:?}");
    assert!(rec.statics.is_empty(), "captured lifetimes must NOT be forced `'static`: {:?}", rec.statics);

    // Negative control: `OpaquePlain<'0, '1>: 'static` MUST require both `'0: 'static` and `'1: 'static`.
    let types_plain = [arg(op_plain, 0, 2)];
    let (res_plain, rec_plain) = run(&cols, &sel, &Own::default(), 2, Callee::Fn(sig), GenericArgs { lifetimes: SlotRange::EMPTY, types: &types_plain }, &[arg(op_plain, 0, 2)], arg(unit, 2, 0));
    assert_eq!(res_plain, Ok(()));
    let mut statics: SVec<u32> = rec_plain.statics.clone();
    statics.sort_unstable();
    assert_eq!(statics, [0, 1], "NEGATIVE CONTROL: plain opaque must require both captured lifetimes to be `'static`");
}

/// Target 4 (`validator.rs::validate_each` shape): an assumed higher-ranked GAT projection
/// supertrait clause `for<'a> <Self::Factory as Factory>::Evm<'a> = Evm<'a>` instantiates its
/// `for<'a>` binder to an existential origin so normalizing `EvmFor<'x, Self>` to `Evm<'x>`
/// leaves no raw `Reg::Bound { binder: 0, .. }` when walked outside `BinderCtx::Clause`.
#[test]
fn higher_ranked_gat_projection_supertrait_clause_normalizes_cleanly() {
    use blaze_typeck::region_facts::{ClauseView, ColView};
    use blaze_typeck::regions::RegionCol;
    use crate::inst::SelfHead;
    const T_CONF: ItemKey = ItemKey(890);
    const T_BASE: ItemKey = ItemKey(891);
    const T_FAC: ItemKey = ItemKey(892);
    const K_EVM: ItemKey = ItemKey(893);
    let fac_name = Name::of("Factory");
    let evm_name = Name::of("Evm");

    let mut t = Tys::new();
    let p0 = t.param(0);
    let self_fac = t.alias(T_BASE, fac_name, &[p0]);
    let self_evm = t.alias(T_FAC, evm_name, &[self_fac]);
    let evm_adt = t.adt(K_EVM, &[]);
    let self_is_conf = t.extrait(T_CONF, &[p0]);
    let proj_clause = t.exproj(T_FAC, evm_name, &[self_fac], evm_adt);

    let facts = Facts::default()
        .adt(K_EVM, 1, &[Variance::In])
        .trait_(T_CONF, 0)
        .trait_(T_BASE, 0)
        .trait_(T_FAC, 0)
        .alias(T_BASE, fac_name, 0)
        .alias(T_FAC, evm_name, 1);
    let cols = Columns::new(t.t.view(), &facts, &facts);

    const L00_L00: &[Region] = &[Region::Late { binder: 0, var: 0 }, Region::Late { binder: 0, var: 0 }];
    let owner_col = RegionCol::lay(t.t.view(), self_is_conf, &facts, NONE, alloc()).unwrap();
    let conf_proj_col = RegionCol::lay(t.t.view(), proj_clause, &facts, L00_L00, alloc()).unwrap();
    let owner_clauses = [ClauseView { nth: 0, binders: 0, col: ColView::of(&owner_col), outlives: None }];
    let conf_views = [ClauseView { nth: 0, binders: 1, col: ColView::of(&conf_proj_col), outlives: None }];

    struct HrGatSel<'a>(&'a [ClauseView<'a>]);
    impl Selected for HrGatSel<'_> {
        fn select(&self, _: Ask) -> Selection<'_> {
            Selection::Not(NotSelected::GenericSelf(SelfHead::Alias))
        }
        fn trait_clauses(&self, trait_: ItemKey) -> Option<(u32, &[ClauseView<'_>])> {
            (trait_ == T_CONF).then_some((0, self.0))
        }
    }
    struct HrGatOwn<'a>(&'a [ClauseView<'a>]);
    impl Owner for HrGatOwn<'_> {
        fn item(&self) -> ItemKey { OWNER }
        fn universal(&self, r: Region) -> Option<Origin> {
            match r {
                Region::Param(i) => Some(Origin(50 + i)),
                _ => None,
            }
        }
        fn param(&self, _p: u32) -> Option<Origin> { None }
        fn bound(&self, _p: u32, _trait_: ItemKey) -> Option<Typed<'_>> { None }
        fn body_params(&self, _body: PTy) -> Option<&[u32]> { None }
        fn clauses(&self) -> &[ClauseView<'_>] {
            self.0
        }
    }

    // Callee: `fn evm_with_env<'a, C: Conf>() -> <<C as Base>::Factory as Factory>::Evm<'a>`
    // Caller receives `Evm<'1>` (`origin 1`) with `'a` = `origin 0`.
    let p = params(1, &[]);
    let sig = Sig { item: METHOD, params: ParamsView::of(&p), inputs: &[], output: ty(self_evm, P0), clauses: &[] };
    let types = [arg(p0, 2, 0)];
    let (res, rec) = run(
        &cols,
        &HrGatSel(&conf_views),
        &HrGatOwn(&owner_clauses),
        2,
        Callee::Fn(sig),
        GenericArgs { lifetimes: range(0, 1), types: &types },
        &[],
        arg(evm_adt, 1, 1),
    );
    assert_eq!(res, Ok(()), "higher-ranked GAT projection supertrait clause must normalize cleanly: {res:?}");
    // Existential origin `o` for `'a` must tie `0` and `1` invariantly (`0 == o == 1`).
    assert!(!rec.subsets.is_empty(), "must emit region constraints tying call lifetime `'0` to `Evm<'1>`");
}

/// Repeated calls through [`crate::call_in`] reuse `CallScratch`'s flattened environment pools
/// and `impl_seen` buffer without leaking bump-frame memory across calls.
#[test]
fn call_in_reuses_scratch_in_bump_frame() {
    let mut t = Tys::new();
    let u8_ = t.u8();
    let r_u8 = t.r(u8_);
    let p0 = t.param(0);
    let r_p0 = t.r(p0);
    let alias_val = t.alias(T_TR, VALUE, &[p0]);
    let impl_self = t.r(p0);
    let impl_trait = t.extrait(T_TR, &[impl_self]);
    let facts = Facts::default().trait_(T_TR, 0).alias(T_TR, VALUE, 0);
    let cols = Columns::new(t.t.view(), &facts, &facts);

    let head = ImplHead { imp: IMP, lifetimes: 1, types: 1, trait_ref: ty(impl_trait, P0) };
    let val = ImplValue { value: ty(r_p0, P0), gat_lifetimes: 0 };
    let mut sel = Sel::default();
    sel.rows.push((T_TR, Some(VALUE), Selection::Impl { head, value: Some(val) }));
    let own = Own::default();

    // `fn f<'a, T>(x: T) -> <T as Tr>::Value` called with `T = &'0 u8`, `dest = &'1 u8`.
    let p = params(1, &[0]);
    let inputs = [ty(p0, NONE)];
    let sig = Sig { item: F, params: ParamsView::of(&p), inputs: &inputs, output: ty(alias_val, NONE), clauses: &[] };
    let types = [arg(r_u8, 0, 1)];
    let args = [arg(r_u8, 0, 1)];
    let dest = arg(r_u8, 1, 1);
    let origins = [Origin(0), Origin(1)];

    let frame = std::boxed::Box::new(blaze_own::Inline::<{ 256 * 1024 }>::new());
    let mut scratch = crate::CallScratch::new_in(&*frame);
    let mut used_after_first = 0usize;
    for iter in 0..500usize {
        let mut rec = Rec { subsets: SVec::new(), statics: SVec::new(), placeholders: SVec::new(), next: 100 };
        let res = {
            let mut cx = Cx { types: &cols, sig_types: &cols, selected: &sel, owner: &own, origins: &origins, out: &mut rec };
            crate::call_in(&mut cx, Callee::Fn(sig), GenericArgs { lifetimes: SlotRange::EMPTY, types: &types }, &args, dest, &mut scratch)
        };
        assert_eq!(res, Ok(()), "iter {iter} failed: {res:?}");
        assert!(!rec.subsets.is_empty());
        if iter == 0 {
            used_after_first = frame.used();
        }
    }
    let used_after_all = frame.used();
    assert!(
        used_after_all <= used_after_first + 4096 && used_after_all < 32 * 1024,
        "CallScratch leaked into bump frame: after_first={used_after_first}B, after_500={used_after_all}B",
    );
}

/// Flattened `Envs::truncate` reclaims `regs`, `late`, and `types` pool space across speculative
/// environment probes instead of leaking per-`Env` `Vec`s into a bump frame.
#[test]
fn envs_truncate_reclaims_pool_capacity_in_bump_frame() {
    let frame = std::boxed::Box::new(blaze_own::Inline::<{ 64 * 1024 }>::new());
    let mut envs: crate::inst::Envs<'static, _> = crate::inst::Envs::new_in(&*frame);
    let base = envs.impl_env(2, 2).expect("base env");
    let mark = envs.len();
    for i in 0..2_000u32 {
        let e1 = envs.impl_env(4, 4).expect("speculative impl env");
        let _e2 = envs.skolemize_clause(e1, 3, || Origin(1000 + i)).expect("skolemize env");
        envs.truncate(mark);
    }
    assert_eq!(envs.len(), mark);
    assert_eq!(mark, 1);
    let _ = base;
    let used = frame.used();
    assert!(used < 16 * 1024, "speculative Envs::truncate leaked into bump frame: used={used}B");
}

/// Higher-ranked impl projection where-clause (`impl<P, R> World for Program<'_, P> where
/// for<'a> P: FnOnce<(&'a u8,), Output = R>, R: 'static`) binds `R` inside the skolemized
/// environment and MUST propagate `R` back to the impl's base environment so subsequent
/// clauses (`R: 'static`) see `R` instead of `Bad::Unconstrained` and still reject non-static `R`.
#[test]
fn higher_ranked_impl_projection_propagates_bound_type_param() {
    use blaze_typeck::region_facts::{ClauseView, ColView};
    use blaze_typeck::regions::RegionCol;

    const T_WORLD: ItemKey = ItemKey(0x9001);
    const T_FN_ONCE: ItemKey = ItemKey(0x9002);
    const K_PROG: ItemKey = ItemKey(0x9003);
    const IMP_WORLD: ItemKey = ItemKey(0x9004);
    const RUN_WORLD: ItemKey = ItemKey(0x9005);

    let mut t = Tys::new();
    let u8_ = t.u8();
    let unit = t.unit();
    let r_u8 = t.r(u8_);
    let p0 = t.param(0);
    let p1 = t.param(1);
    let prog_p0 = t.adt(K_PROG, &[p0]);
    let args_list = t.list(&[r_u8]);
    let args_tup = t.n(PNode::Tuple(args_list));
    let impl_header = t.extrait(T_WORLD, &[prog_p0]);
    let impl_proj = t.exproj(T_FN_ONCE, OUTPUT, &[p0, args_tup], p1);
    let call_clause = t.extrait(T_WORLD, &[p0]);

    // Caller closure type `fn(&'b u8) -> &'0 u8` (its output carries caller slot `Origin(0)`).
    let fn_ret_ref = t.fnptr(&[r_u8, r_u8]);
    let prog_fn = t.adt(K_PROG, &[fn_ret_ref]);

    let facts = Facts::default()
        .adt(K_PROG, 1, &[Variance::Co, Variance::In])
        .trait_(T_WORLD, 0)
        .trait_(T_FN_ONCE, 0)
        .alias(T_FN_ONCE, OUTPUT, 0);
    let cols = Columns::new(t.t.view(), &facts, &facts);

    let proj_col = RegionCol::lay(t.t.view(), impl_proj, &facts, L00, alloc()).unwrap();
    let r_subj_col = RegionCol::lay(t.t.view(), p1, &facts, NONE, alloc()).unwrap();
    let impl_views = [
        ClauseView { nth: 0, binders: 1, col: ColView::of(&proj_col), outlives: None },
        ClauseView { nth: 1, binders: 0, col: ColView::of(&r_subj_col), outlives: None },
    ];
    let impl_outlives = [Outlives::Type { clause: 1, region: Region::Static }];

    struct WorldSel<'a> {
        head: ImplHead<'a>,
        views: &'a [ClauseView<'a>],
        outlives: &'a [Outlives],
    }
    impl Selected for WorldSel<'_> {
        fn select(&self, ask: Ask) -> Selection<'_> {
            if ask.trait_ == T_WORLD && ask.name.is_none() {
                Selection::Impl { head: self.head, value: None }
            } else {
                Selection::Not(NotSelected::Unanswered)
            }
        }
        fn is_fn_trait(&self, trait_: ItemKey) -> bool {
            trait_ == T_FN_ONCE
        }
        fn impl_where(&self, imp: ItemKey) -> Result<ImplWhere<'_>, NotSelected> {
            if imp == IMP_WORLD {
                Ok(ImplWhere { params: &[], outlives: self.outlives, views: self.views })
            } else {
                Ok(ImplWhere::NONE)
            }
        }
    }

    let sel = WorldSel {
        head: ImplHead { imp: IMP_WORLD, lifetimes: 1, types: 2, trait_ref: ty(impl_header, P0) },
        views: &impl_views,
        outlives: &impl_outlives,
    };
    let own = Own::default();

    // `fn run_world<W: World>(w: W)` called with `W = Program<'0, fn(&u8) -> &'1 u8>`.
    // Slot 0 = Program's lifetime, slot 1 = fn_ret_ref's return lifetime (`R`).
    let p = params(0, &[]);
    let inputs = [ty(p0, NONE)];
    let clauses = [ClauseIn::TraitOf { trait_ref: ty(call_clause, NONE), binders: 0 }];
    let sig = Sig { item: RUN_WORLD, params: ParamsView::of(&p), inputs: &inputs, output: ty(unit, NONE), clauses: &clauses };

    // Positive: `fn(&'b u8) -> &'1 u8` has 2 slots (input binder + output); give caller slot `1`
    // for the return reference so `R` binds to `&'1 u8`, and `R: 'static` constrains `1: 'static`.
    let mut t2 = Tys::new();
    let _ = &mut t2;
    let fn_arg = arg(prog_fn, 0, 3); // slots: 0 (Program's 'p), 1 (fn arg), 2 (fn ret)
    let types = [fn_arg];
    let args = [fn_arg];
    let (res, rec) = run(
        &cols,
        &sel,
        &own,
        3,
        Callee::Fn(sig),
        GenericArgs { lifetimes: SlotRange::EMPTY, types: &types },
        &args,
        arg(unit, 0, 0),
    );
    assert_eq!(res, Ok(()), "higher-ranked impl projection must propagate bound R to impl env: {res:?}");
    // Soundness check (negative control): `R: 'static` must constrain the return slot `Origin(2)`
    // to outlive `'static` (a local borrow in `R` is rejected).
    assert!(
        reach(&rec, 2).contains(&STATIC_NODE),
        "propagated R = &'2 u8 must still satisfy impl's `R: 'static` bound; statics={:?} subsets={:?}",
        rec.statics,
        rec.subsets,
    );
}

/// An impl header whose `SelfTy` (child 0) contains a projection over a type parameter `T` bound
/// by a later trait argument (child 1) — e.g. `impl<'a, T> From<&'a mut T> for StoreContextMut<'a,
/// <T as AsContext>::Data>` — must walk the non-alias argument first so `T` is bound before
/// `<T as AsContext>::Data` normalizes, and still relate `'a` invariantly.
#[test]
fn impl_header_walks_non_alias_arg_before_alias_arg() {
    const T_FROM: ItemKey = ItemKey(0x9010);
    const T_ASCX: ItemKey = ItemKey(0x9011);
    const K_STORE: ItemKey = ItemKey(0x9012);
    const K_CALLER: ItemKey = ItemKey(0x9013);
    const K_HOST: ItemKey = ItemKey(0x9014);
    const IMP_FROM: ItemKey = ItemKey(0x9015);
    const IMP_ASCX: ItemKey = ItemKey(0x9016);
    const F_CONVERT: ItemKey = ItemKey(0x9017);
    const DATA: Name = Name::of("Data");

    let mut t = Tys::new();
    let unit = t.unit();
    let p0 = t.param(0);
    let p1 = t.param(1);
    let host = t.adt(K_HOST, &[]);
    let caller = t.adt(K_CALLER, &[]);
    let mut_p0 = t.rm(p0);
    let mut_caller = t.rm(caller);
    let p0_data = t.alias(T_ASCX, DATA, &[p0]);
    let store_data = t.adt(K_STORE, &[p0_data]);
    let store_host = t.adt(K_STORE, &[host]);
    // Header of `impl<'a, T> From<&'a mut T> for StoreContextMut<'a, <T as AsContext>::Data>`:
    // child 0 = `StoreContextMut<'a, T::Data>` (has Alias!), child 1 = `&'a mut T` (binds `T`!).
    let from_header = t.extrait(T_FROM, &[store_data, mut_p0]);
    let ascx_header = t.extrait(T_ASCX, &[caller]);
    let call_clause = t.extrait(T_FROM, &[p0, p1]);

    let facts = Facts::default()
        .adt(K_STORE, 1, &[Variance::In, Variance::In])
        .adt(K_CALLER, 0, &[])
        .adt(K_HOST, 0, &[])
        .trait_(T_FROM, 0)
        .trait_(T_ASCX, 0)
        .alias(T_ASCX, DATA, 0);
    let cols = Columns::new(t.t.view(), &facts, &facts);

    // Require exact `Keyed::Same` match for `AsContext::Data` so an unbound `T` (`Keyed::Unknown`)
    // would fail with `NotSelected::Unanswered` — reproducing `body_rows` in `one_call.rs`.
    struct StrictSel<'a> {
        from_head: ImplHead<'a>,
        ascx_head: ImplHead<'a>,
        ascx_val: ImplValue<'a>,
        caller_ty: PTy,
    }
    impl Selected for StrictSel<'_> {
        fn select(&self, _: Ask) -> Selection<'_> {
            Selection::Not(NotSelected::Unanswered)
        }
        fn select_keyed(&self, ask: Ask, args: &dyn crate::inst::AskArgs) -> Selection<'_> {
            if ask.trait_ == T_FROM && ask.name.is_none() {
                Selection::Impl { head: self.from_head, value: None }
            } else if ask.trait_ == T_ASCX && ask.name == Some(DATA) {
                if args.compare(0, self.caller_ty) == crate::inst::Keyed::Same {
                    Selection::Impl { head: self.ascx_head, value: Some(self.ascx_val) }
                } else {
                    Selection::Not(NotSelected::Unanswered)
                }
            } else {
                Selection::Not(NotSelected::Unanswered)
            }
        }
    }

    let sel = StrictSel {
        from_head: ImplHead { imp: IMP_FROM, lifetimes: 1, types: 1, trait_ref: ty(from_header, P00) },
        ascx_head: ImplHead { imp: IMP_ASCX, lifetimes: 0, types: 0, trait_ref: ty(ascx_header, NONE) },
        ascx_val: ImplValue { value: ty(host, NONE), gat_lifetimes: 0 },
        caller_ty: caller,
    };
    let own = Own::default();

    // `fn convert<U, V>(v: V) -> U where U: From<V>` called with
    // `U = StoreContextMut<'1, Host>`, `V = &'0 mut Caller`.
    let p = params(0, &[]);
    let inputs = [ty(p1, NONE)];
    let clauses = [ClauseIn::TraitOf { trait_ref: ty(call_clause, NONE), binders: 0 }];
    let sig = Sig { item: F_CONVERT, params: ParamsView::of(&p), inputs: &inputs, output: ty(p0, NONE), clauses: &clauses };
    let types = [arg(store_host, 1, 1), arg(mut_caller, 0, 1)];
    let args = [arg(mut_caller, 0, 1)];
    let dest = arg(store_host, 1, 1);
    let _ = unit;
    let (res, rec) = run(
        &cols,
        &sel,
        &own,
        2,
        Callee::Fn(sig),
        GenericArgs { lifetimes: SlotRange::EMPTY, types: &types },
        &args,
        dest,
    );
    assert_eq!(res, Ok(()), "deferred alias header kid must normalize after T is bound: {res:?}");
    // Soundness check: `'0` (`&'0 mut Caller`) and `'1` (`StoreContextMut<'1, Host>`) must be tied
    // invariantly (`0 <: 1` and `1 <: 0`).
    assert!(reach(&rec, 0).contains(&1), "must constrain '0 <: '1: {:?}", rec.subsets);
    assert!(reach(&rec, 1).contains(&0), "must constrain '1 <: '0: {:?}", rec.subsets);
}

/// Builtin `<dyn Tr + 't as Pointee>::Metadata` relating against `DynMetadata<dyn Tr + 'o>`
/// must succeed without `NotSelected::Builtin`, enforce exact object-lifetime variance (`'0 <: '1`),
/// and reject mismatched trait objects (`dyn OtherTr`).
#[test]
fn pointee_dyn_metadata_relates_dyn_object_lifetimes_soundly() {
    const T_POINTEE: ItemKey = ItemKey(0x9020);
    const T_REFLECT: ItemKey = ItemKey(0x9021);
    const T_OTHER: ItemKey = ItemKey(0x9022);
    const K_DYN_META: ItemKey = ItemKey(0x9023);
    const F_FROM_RAW: ItemKey = ItemKey(0x9024);
    const METADATA: Name = Name::of("Metadata");

    let mut t = Tys::new();
    let p0 = t.param(0);
    let ex_reflect = t.extrait(T_REFLECT, &[]);
    let ex_other = t.extrait(T_OTHER, &[]);
    let dyn_reflect = t.dynamic(&[ex_reflect]);
    let dyn_other = t.dynamic(&[ex_other]);
    let dyn_meta_reflect = t.adt(K_DYN_META, &[dyn_reflect]);
    let dyn_meta_other = t.adt(K_DYN_META, &[dyn_other]);
    let p0_meta = t.alias(T_POINTEE, METADATA, &[p0]);
    let ptr_p0 = t.n(PNode::Ptr(Mutbl::Not, p0));
    let ptr_dyn_reflect = t.n(PNode::Ptr(Mutbl::Not, dyn_reflect));

    let facts = Facts::default()
        .adt(K_DYN_META, 0, &[Variance::Co])
        .trait_(T_POINTEE, 0)
        .trait_(T_REFLECT, 0)
        .trait_(T_OTHER, 0)
        .alias(T_POINTEE, METADATA, 0);
    let cols = Columns::new(t.t.view(), &facts, &facts);

    struct PointeeSel;
    impl Selected for PointeeSel {
        fn select(&self, _: Ask) -> Selection<'_> {
            Selection::Not(NotSelected::Builtin)
        }
        fn is_zero_slot_builtin_assoc(&self, trait_: ItemKey, name: Name) -> bool {
            trait_ == T_POINTEE && name == METADATA
        }
    }

    let own = Own::default();
    // `fn from_raw_parts<T: ?Sized>(meta: <T as Pointee>::Metadata) -> *const T`
    let p = params(0, &[]);
    let inputs = [ty(p0_meta, NONE)];
    let sig = Sig { item: F_FROM_RAW, params: ParamsView::of(&p), inputs: &inputs, output: ty(ptr_p0, NONE), clauses: &[] };

    // Positive: `T = dyn Reflect + '2`, arg = `DynMetadata<dyn Reflect + '0>`, dest = `*const (dyn Reflect + '1)`.
    let types = [arg(dyn_reflect, 2, 1)];
    let args = [arg(dyn_meta_reflect, 0, 1)];
    let dest = arg(ptr_dyn_reflect, 1, 1);
    let (res, rec) = run(
        &cols,
        &PointeeSel,
        &own,
        3,
        Callee::Fn(sig),
        GenericArgs { lifetimes: SlotRange::EMPTY, types: &types },
        &args,
        dest,
    );
    assert_eq!(res, Ok(()), "Pointee::Metadata for dyn Trait must relate cleanly: {res:?}");
    assert!(reach(&rec, 0).contains(&1), "must relate DynMetadata's '0 <: dest *const dyn '1: {:?}", rec.subsets);
    assert!(!reach(&rec, 1).contains(&0), "DynMetadata is covariant, not invariant: {:?}", rec.subsets);

    // Negative soundness control: mismatched `DynMetadata<dyn Other>` against `<dyn Reflect as Pointee>::Metadata` MUST fail with `Shape`.
    let bad_args = [arg(dyn_meta_other, 0, 1)];
    let (bad_res, _) = run(
        &cols,
        &PointeeSel,
        &own,
        3,
        Callee::Fn(sig),
        GenericArgs { lifetimes: SlotRange::EMPTY, types: &types },
        &bad_args,
        dest,
    );
    assert!(matches!(bad_res, Err(Refused::Shape { .. })), "mismatched dyn trait in DynMetadata must be refused with Shape, got: {bad_res:?}");
}

/// Builtin `Fn*` proof peels `&mut` / `&` wrappers around a caller closure (`&mut each` in
/// `Iterator::try_for_each(&mut each)`), tying the closure's input/output regions while still
/// enforcing any `F: 'static` bound on the outer `&mut` reference itself.
#[test]
fn builtin_fn_trait_peels_mut_ref_closure_and_enforces_regions() {
    const T_FN_MUT: ItemKey = ItemKey(0x9030);
    const T_FN_ONCE: ItemKey = ItemKey(0x9031);
    const F_APPLY: ItemKey = ItemKey(0x9032);

    let mut t = Tys::new();
    let u8_ = t.u8();
    let r_u8 = t.r(u8_);
    let p0 = t.param(0); // F
    let p1 = t.param(1); // R
    let closure_ty = t.n(PNode::Closure { owner: OWNER, ordinal: 0, args: List::EMPTY });
    let mut_closure = t.rm(closure_ty);
    let closure_sig_ty = t.fnptr(&[r_u8, r_u8]);
    let args_list = t.list(&[r_u8]);
    let args_tup = t.n(PNode::Tuple(args_list));
    let fn_mut_clause = t.extrait(T_FN_MUT, &[p0, args_tup]);
    let fn_out_clause = t.exproj(T_FN_ONCE, OUTPUT, &[p0, args_tup], p1);

    let facts = Facts::default().trait_(T_FN_MUT, 0).trait_(T_FN_ONCE, 0).alias(T_FN_ONCE, OUTPUT, 0);
    struct BodyCols<'a> {
        inner: Columns<'a>,
        closure_ty: PTy,
    }
    impl RegionTypes for BodyCols<'_> {
        fn count(&self, t: PTy) -> Result<SlotCount, Unknown> {
            if t == self.closure_ty {
                Ok(SlotCount::new(1))
            } else if let Ok(PNode::Ref(_, inner)) = self.inner.node(t)
                && inner == self.closure_ty
            {
                Ok(SlotCount::new(2))
            } else {
                self.inner.count(t)
            }
        }
        fn node(&self, t: PTy) -> Result<PNode, Unknown> { self.inner.node(t) }
        fn own(&self, t: PTy) -> Result<SlotCount, Unknown> {
            if t == self.closure_ty { Ok(SlotCount::new(1)) } else { self.inner.own(t) }
        }
        fn kids(&self, t: PTy) -> Result<Kids<'_>, Unknown> {
            if t == self.closure_ty { Ok(Kids::None) } else { self.inner.kids(t) }
        }
        fn variance(&self, t: PTy, at: Pos) -> Result<Variance, Unknown> {
            if t == self.closure_ty { Ok(Variance::In) } else { self.inner.variance(t, at) }
        }
        fn adt_variances(&self, adt: ItemKey) -> Option<&[Variance]> {
            self.inner.adt_variances(adt)
        }
    }
    let cols = BodyCols { inner: Columns::new(t.t.view(), &facts, &facts), closure_ty };

    struct FnSel;
    impl Selected for FnSel {
        fn select(&self, _: Ask) -> Selection<'_> { Selection::Not(NotSelected::Unanswered) }
        fn is_fn_trait(&self, trait_: ItemKey) -> bool { trait_ == T_FN_MUT || trait_ == T_FN_ONCE }
    }
    struct ClosureOwn {
        sig_ty: PTy,
    }
    impl Owner for ClosureOwn {
        fn item(&self) -> ItemKey { OWNER }
        fn universal(&self, _: Region) -> Option<Origin> { None }
        fn param(&self, _: u32) -> Option<Origin> { None }
        fn bound(&self, _: u32, _: ItemKey) -> Option<Typed<'_>> { None }
        fn body_params(&self, _: PTy) -> Option<&[u32]> { Some(&[]) }
        fn closure_sig(&self, ordinal: u32) -> Option<crate::ClosureSig<'_>> {
            (ordinal == 0).then_some(crate::ClosureSig { sig_ty: self.sig_ty, ncap: 0, nsig: 1, col: P00 })
        }
    }

    // `fn apply<'a, F: FnMut(&'a u8) -> R, R>(f: F, x: &'a u8) -> R`
    // Called with `F = &'0 mut Closure('1)` (2 slots: '0 for &mut, '1 for closure sig),
    // `x: &'2 u8`, `dest: &'3 u8`.
    let p = params(1, &[0]);
    let inputs = [ty(p0, NONE), ty(r_u8, P0)];
    let clauses = [
        ClauseIn::TraitOf { trait_ref: ty(fn_mut_clause, P0), binders: 0 },
        ClauseIn::ProjOf { proj: ty(fn_out_clause, P0), binders: 0 },
    ];
    let sig = Sig { item: F_APPLY, params: ParamsView::of(&p), inputs: &inputs, output: ty(p1, NONE), clauses: &clauses };
    let f_arg = arg(mut_closure, 0, 2);
    let types = [f_arg, arg(r_u8, 3, 1)];
    let args = [f_arg, arg(r_u8, 2, 1)];
    let dest = arg(r_u8, 3, 1);

    let origins = [Origin(0), Origin(1), Origin(2), Origin(3)];
    let mut rec = Rec { subsets: SVec::new(), statics: SVec::new(), placeholders: SVec::new(), next: 100 };
    let own = ClosureOwn { sig_ty: closure_sig_ty };
    let res = {
        let mut cx = Cx { types: &cols, sig_types: &cols, selected: &FnSel, owner: &own, origins: &origins, out: &mut rec };
        call(&mut cx, Callee::Fn(sig), GenericArgs { lifetimes: SlotRange::EMPTY, types: &types }, &args, dest, alloc())
    };
    assert_eq!(res, Ok(()), "`&mut closure` must satisfy builtin FnMut + FnOnce::Output: {res:?}");
    assert!(reach(&rec, 2).contains(&3), "closure input '2 must flow to output '3: {:?}", rec.subsets);
    assert!(!reach(&rec, 0).contains(&STATIC_NODE), "plain FnMut must not require &'0 mut to be 'static");

    // Negative soundness control: adding `F: 'static` MUST constrain the outer `&'0 mut` reference
    // to `'static` even though `builtin_fn_trait` peels `&mut`.
    let static_clauses = [
        ClauseIn::TraitOf { trait_ref: ty(fn_mut_clause, P0), binders: 0 },
        ClauseIn::ProjOf { proj: ty(fn_out_clause, P0), binders: 0 },
        ClauseIn::TypeOutlives { subject: ty(p0, NONE), region: Region::Static },
    ];
    let static_sig = Sig { item: F_APPLY, params: ParamsView::of(&p), inputs: &inputs, output: ty(p1, NONE), clauses: &static_clauses };
    let mut rec2 = Rec { subsets: SVec::new(), statics: SVec::new(), placeholders: SVec::new(), next: 100 };
    let res2 = {
        let mut cx = Cx { types: &cols, sig_types: &cols, selected: &FnSel, owner: &own, origins: &origins, out: &mut rec2 };
        call(&mut cx, Callee::Fn(static_sig), GenericArgs { lifetimes: SlotRange::EMPTY, types: &types }, &args, dest, alloc())
    };
    assert_eq!(res2, Ok(()));
    assert!(reach(&rec2, 0).contains(&STATIC_NODE), "`F: 'static` must still constrain outer `&'0 mut` ref to 'static");
    assert!(reach(&rec2, 1).contains(&STATIC_NODE), "`F: 'static` must still constrain inner closure slot '1 to 'static");
}

/// Wave 5 Cluster A (`blaze-typeck` `import`, `import_list`, `import_witness_goal`,
/// `import_instance_future`): when an associated type declares `type Error: From<CheckError>` and
/// a `?` / `FromResidual` call proves `<S as ClosureSource>::Error: From<<S as ClosureSource>::Error>`,
/// `match_trait_clause` MUST NOT falsely match `CheckError` against the rigid non-inert alias
/// `<S as ClosureSource>::Error` under `allow_alias = true` (which previously aborted with
/// `Unselected(GenericSelf(Param))` before reaching blanket `impl<T> From<T> for T`).
///
/// Also verifies:
/// - `S::Error: From<CheckError>` still proves via `assoc_clauses`;
/// - multiple clauses `T: From<ErrA>` + `T: From<ErrB>` against `T: From<X::Assoc>` (where
///   `X::Assoc -> ErrB`) pick `From<ErrB>` via `d >= 0` normalization instead of committing to `ErrA`;
/// - NEGATIVE CONTROL 1 (rule 4 / rule 23): without blanket `impl<T> From<T> for T`,
///   `S::Error: From<S::Error>` is refused `Unselected` (never falsely satisfied by `From<CheckError>`);
/// - NEGATIVE CONTROL 2 (rule 4 / rule 23): a clause `T: Tr<S::Error>` MUST NOT match a non-inert
///   goal `T: Tr<&'a u8>`.
#[test]
fn assoc_from_bound_does_not_shadow_reflexive_from_impl() {
    use blaze_typeck::region_facts::{ClauseView, ColView};

    const T_SRC: ItemKey = ItemKey(0x9040);
    const T_FROM: ItemKey = ItemKey(0x9041);
    const T_WRAP: ItemKey = ItemKey(0x9042);
    const K_CHECK_ERR: ItemKey = ItemKey(0x9043);
    const K_OTHER_ERR: ItemKey = ItemKey(0x9044);
    const K_PROVIDER: ItemKey = ItemKey(0x9045);
    const IMP_FROM_REFL: ItemKey = ItemKey(0x9046);
    const IMP_WRAP: ItemKey = ItemKey(0x9047);
    const F_CONVERT: ItemKey = ItemKey(0x9048);
    const ERROR: Name = Name::of("Error");
    const ASSOC: Name = Name::of("Assoc");

    let mut t = Tys::new();
    let u8_ = t.u8();
    let r_u8 = t.r(u8_);
    let p0 = t.param(0);
    let p1 = t.param(1);
    let check_err = t.adt(K_CHECK_ERR, &[]);
    let other_err = t.adt(K_OTHER_ERR, &[]);
    let provider = t.adt(K_PROVIDER, &[]);
    let s_err = t.alias(T_SRC, ERROR, &[p0]);
    let prov_assoc = t.alias(T_WRAP, ASSOC, &[provider]);

    // Assoc item bound: `<Self as ClosureSource>::Error: From<CheckError>`
    let assoc_from_check = t.extrait(T_FROM, &[s_err, check_err]);
    // Callee clause: `F: From<E>` (with `F = Param(0)`, `E = Param(1)`)
    let call_from = t.extrait(T_FROM, &[p0, p1]);
    // Blanket `impl<T> From<T> for T`: header `ExTrait(From, [Param(0), Param(0)])`
    let from_refl_header = t.extrait(T_FROM, &[p0, p0]);
    // `impl Wrap for Provider { type Assoc = CheckError; }`
    let wrap_header = t.extrait(T_WRAP, &[provider]);
    // Additional owner clauses for multi-clause normalization & non-inert negative control
    let owner_from_other = t.extrait(T_FROM, &[p0, other_err]);
    let owner_from_check = t.extrait(T_FROM, &[p0, check_err]);
    let owner_from_s_err = t.extrait(T_FROM, &[p0, s_err]);

    let facts = Facts::default()
        .adt(K_CHECK_ERR, 0, &[])
        .adt(K_OTHER_ERR, 0, &[])
        .adt(K_PROVIDER, 0, &[])
        .trait_(T_SRC, 0)
        .trait_(T_FROM, 0)
        .trait_(T_WRAP, 0)
        .alias(T_SRC, ERROR, 0)
        .alias(T_WRAP, ASSOC, 0);
    let cols = Columns::new(t.t.view(), &facts, &facts);

    let assoc_col = blaze_typeck::regions::RegionCol::lay(t.t.view(), assoc_from_check, &facts, NONE, alloc()).expect("assoc bound laid");
    let assoc_cv = [ClauseView { nth: 0, binders: 0, col: ColView::of(&assoc_col), outlives: None }];

    struct Sel<'s> {
        assoc_cv: &'s [ClauseView<'s>],
        from_refl: Option<ImplHead<'s>>,
        wrap_head: ImplHead<'s>,
        wrap_val: ImplValue<'s>,
    }
    impl Selected for Sel<'_> {
        fn select(&self, ask: Ask) -> Selection<'_> {
            if ask.trait_ == T_FROM && ask.name.is_none() {
                match self.from_refl {
                    Some(head) => Selection::Impl { head, value: None },
                    None => Selection::Not(NotSelected::GenericSelf(crate::inst::SelfHead::Alias)),
                }
            } else if ask.trait_ == T_SRC && ask.name == Some(ERROR) {
                Selection::Not(NotSelected::GenericSelf(crate::inst::SelfHead::Param))
            } else if ask.trait_ == T_WRAP && ask.name == Some(ASSOC) {
                Selection::Impl { head: self.wrap_head, value: Some(self.wrap_val) }
            } else {
                Selection::Not(NotSelected::Unanswered)
            }
        }
        fn assoc_clauses(&self, trait_: ItemKey, name: Name) -> Option<(u32, &[ClauseView<'_>])> {
            (trait_ == T_SRC && name == ERROR).then_some((0, self.assoc_cv))
        }
    }

    let from_head = ImplHead { imp: IMP_FROM_REFL, lifetimes: 0, types: 1, trait_ref: ty(from_refl_header, NONE) };
    let wrap_head = ImplHead { imp: IMP_WRAP, lifetimes: 0, types: 0, trait_ref: ty(wrap_header, NONE) };
    let wrap_val = ImplValue { value: ty(check_err, NONE), gat_lifetimes: 0 };

    let p = params(0, &[]);
    let inputs = [ty(p1, NONE)];
    let clauses = [ClauseIn::TraitOf { trait_ref: ty(call_from, NONE), binders: 0 }];
    let sig = Sig { item: F_CONVERT, params: ParamsView::of(&p), inputs: &inputs, output: ty(p0, NONE), clauses: &clauses };

    // Positive 1: `S::Error: From<S::Error>` skips non-matching `From<CheckError>` assoc bound and
    // proves via reflexive `impl<T> From<T> for T`.
    let sel_with_refl = Sel { assoc_cv: &assoc_cv, from_refl: Some(from_head), wrap_head, wrap_val };
    let types_self = [arg(s_err, 0, 0), arg(s_err, 0, 0)];
    let (res, _) = run(
        &cols, &sel_with_refl, &Own::default(), 0,
        Callee::Fn(sig), GenericArgs { lifetimes: SlotRange::EMPTY, types: &types_self },
        &[arg(s_err, 0, 0)], arg(s_err, 0, 0),
    );
    assert_eq!(res, Ok(()), "S::Error: From<S::Error> must reach reflexive From impl: {res:?}");

    // Negative control 1: WITHOUT reflexive `impl<T> From<T> for T`, `S::Error: From<S::Error>`
    // MUST be refused `Unselected` on `T_FROM` (never falsely proved by `From<CheckError>`, and
    // never aborting with `T_SRC` `GenericSelf(Param)`!).
    let sel_no_refl = Sel { assoc_cv: &assoc_cv, from_refl: None, wrap_head, wrap_val };
    let (res_no_refl, _) = run(
        &cols, &sel_no_refl, &Own::default(), 0,
        Callee::Fn(sig), GenericArgs { lifetimes: SlotRange::EMPTY, types: &types_self },
        &[arg(s_err, 0, 0)], arg(s_err, 0, 0),
    );
    assert!(
        matches!(res_no_refl, Err(Refused::Unselected { trait_: T_FROM, .. })),
        "CONTROL: S::Error: From<S::Error> without From impl must fail on T_FROM, got: {res_no_refl:?}",
    );

    // Positive 2: `S::Error: From<CheckError>` still proves directly via `assoc_clauses` even
    // without any `From` impl.
    let types_check = [arg(s_err, 0, 0), arg(check_err, 0, 0)];
    let (res_check, _) = run(
        &cols, &sel_no_refl, &Own::default(), 0,
        Callee::Fn(sig), GenericArgs { lifetimes: SlotRange::EMPTY, types: &types_check },
        &[arg(check_err, 0, 0)], arg(s_err, 0, 0),
    );
    assert_eq!(res_check, Ok(()), "S::Error: From<CheckError> must still prove via assoc_clauses: {res_check:?}");

    // Positive 3: two caller clauses `P0: From<OtherErr>` + `P0: From<CheckError>` against goal
    // `P0: From<<Provider as Wrap>::Assoc>` (where `Provider::Assoc = CheckError`) must select the
    // second clause via top-level alias normalization instead of committing to `OtherErr` and failing `Shape`.
    let c_other = blaze_typeck::regions::RegionCol::lay(t.t.view(), owner_from_other, &facts, NONE, alloc()).expect("laid");
    let c_check = blaze_typeck::regions::RegionCol::lay(t.t.view(), owner_from_check, &facts, NONE, alloc()).expect("laid");
    let c_s_err = blaze_typeck::regions::RegionCol::lay(t.t.view(), owner_from_s_err, &facts, NONE, alloc()).expect("laid");
    struct ClauseOwn<'s> {
        clauses: &'s [ClauseView<'s>],
    }
    impl Owner for ClauseOwn<'_> {
        fn item(&self) -> ItemKey { OWNER }
        fn universal(&self, _: Region) -> Option<Origin> { None }
        fn param(&self, _: u32) -> Option<Origin> { None }
        fn bound(&self, _: u32, _: ItemKey) -> Option<Typed<'_>> { None }
        fn clauses(&self) -> &[ClauseView<'_>] { self.clauses }
        fn body_params(&self, _: PTy) -> Option<&[u32]> { None }
    }
    let multi_cv = [
        ClauseView { nth: 0, binders: 0, col: ColView::of(&c_other), outlives: None },
        ClauseView { nth: 1, binders: 0, col: ColView::of(&c_check), outlives: None },
    ];
    let multi_own = ClauseOwn { clauses: &multi_cv };
    let types_alias_arg = [arg(p0, 0, 0), arg(prov_assoc, 0, 0)];
    let (res_multi, _) = run(
        &cols, &sel_no_refl, &multi_own, 0,
        Callee::Fn(sig), GenericArgs { lifetimes: SlotRange::EMPTY, types: &types_alias_arg },
        &[arg(prov_assoc, 0, 0)], arg(p0, 0, 0),
    );
    assert_eq!(res_multi, Ok(()), "normalizable alias trait arg must pick matching second clause: {res_multi:?}");

    // Negative control 2: caller clause `P0: From<S::Error>` MUST NOT match non-inert goal
    // `P0: From<&'0 u8>`.
    let bad_cv = [ClauseView { nth: 0, binders: 0, col: ColView::of(&c_s_err), outlives: None }];
    let bad_own = ClauseOwn { clauses: &bad_cv };
    let types_ref_arg = [arg(p0, 0, 0), arg(r_u8, 0, 1)];
    let (res_ref, _) = run(
        &cols, &sel_no_refl, &bad_own, 1,
        Callee::Fn(sig), GenericArgs { lifetimes: SlotRange::EMPTY, types: &types_ref_arg },
        &[arg(r_u8, 0, 1)], arg(p0, 0, 0),
    );
    assert!(
        matches!(res_ref, Err(Refused::Unselected { trait_: T_FROM, .. })),
        "CONTROL: P0: From<S::Error> must not match non-inert P0: From<&'0 u8>, got: {res_ref:?}",
    );
}

/// Wave 5 Cluster C (`blaze-text/src/debug.rs` `form`, `reflect_of`): builtin
/// `core::any::TryAsDynCompatible<'a>` (`#[lang = "try_as_dyn"]`, `#[rustc_deny_explicit_impl]`)
/// ties `'a` invariantly to the trait object's lifetime `'o` on `dyn Trait + 'o`.
///
/// - Positive 1 (`TypeId::trait_info_of::<dyn DebugForm>()`): late-bound `'a` ties to `'o` cleanly.
/// - Positive 2 (`try_as_dyn<'a, &'t u8, dyn DebugForm + 'u>`): enforces `'t: 'a == 'u` -> `'t: 'u`.
/// - Negative soundness control 1: `try_as_dyn<'a, &'local u8, dyn DebugForm + 'static>` MUST
///   constrain `'local: 'static`.
/// - Negative soundness control 2: non-`Dynamic` subject `u32: TryAsDynCompatible<'a>` MUST be
///   refused `Unselected`.
#[test]
fn try_as_dyn_compatible_ties_object_lifetime_invariantly() {
    const T_TRY_AS_DYN: ItemKey = ItemKey(0x9050);
    const T_DEBUG_FORM: ItemKey = ItemKey(0x9051);
    const F_TRAIT_INFO: ItemKey = ItemKey(0x9052);
    const F_TRY_AS_DYN: ItemKey = ItemKey(0x9053);

    let mut t = Tys::new();
    let unit = t.unit();
    let u8_ = t.u8();
    let r_u8 = t.r(u8_);
    let p0 = t.param(0);
    let p1 = t.param(1);
    let ex_debug = t.extrait(T_DEBUG_FORM, &[]);
    let dyn_debug = t.dynamic(&[ex_debug]);
    let try_clause_p0 = t.extrait(T_TRY_AS_DYN, &[p0]);
    let try_clause_p1 = t.extrait(T_TRY_AS_DYN, &[p1]);

    let facts = Facts::default()
        .trait_(T_TRY_AS_DYN, 1)
        .trait_(T_DEBUG_FORM, 0);
    let cols = Columns::new(t.t.view(), &facts, &facts);

    struct TryDynSel;
    impl Selected for TryDynSel {
        fn select(&self, _: Ask) -> Selection<'_> {
            Selection::Not(NotSelected::GenericSelf(crate::inst::SelfHead::Object))
        }
        fn is_try_as_dyn_trait(&self, trait_: ItemKey) -> bool {
            trait_ == T_TRY_AS_DYN
        }
    }

    // Positive 1: `fn trait_info_of<'a, T: TryAsDynCompatible<'a> + ?Sized>() -> ()`
    // Called with `T = dyn DebugForm + '0` (`'a` late-bound).
    let p_late = params(1, &[0]);
    let clauses_info = [ClauseIn::TraitOf { trait_ref: ty(try_clause_p0, P0), binders: 0 }];
    let sig_info = Sig { item: F_TRAIT_INFO, params: ParamsView::of(&p_late), inputs: &[], output: ty(unit, NONE), clauses: &clauses_info };
    let (res_info, rec_info) = run(
        &cols, &TryDynSel, &Own::default(), 1,
        Callee::Fn(sig_info), GenericArgs { lifetimes: SlotRange::EMPTY, types: &[arg(dyn_debug, 0, 1)] },
        &[], arg(unit, 0, 0),
    );
    assert_eq!(res_info, Ok(()), "trait_info_of::<dyn DebugForm>() must prove TryAsDynCompatible: {res_info:?}");
    // Fresh late-bound origin (100) must be tied invariantly to slot 0 (`'o`).
    assert!(reach(&rec_info, 100).contains(&0) && reach(&rec_info, 0).contains(&100), "must tie 'a == 'o: {:?}", rec_info.subsets);

    // Positive 2: `fn try_as_dyn<'a, T: 'a, U: TryAsDynCompatible<'a>>(_: T) -> ()`
    // Called with `T = &'0 u8`, `U = dyn DebugForm + '1` -> must constrain `'0: '1`.
    let clauses_try = [
        ClauseIn::TypeOutlives { subject: ty(p0, NONE), region: Region::Param(0) },
        ClauseIn::TraitOf { trait_ref: ty(try_clause_p1, P0), binders: 0 },
    ];
    let inputs_try = [ty(p0, NONE)];
    let sig_try = Sig { item: F_TRY_AS_DYN, params: ParamsView::of(&p_late), inputs: &inputs_try, output: ty(unit, NONE), clauses: &clauses_try };
    let (res_try, rec_try) = run(
        &cols, &TryDynSel, &Own::default(), 2,
        Callee::Fn(sig_try), GenericArgs { lifetimes: SlotRange::EMPTY, types: &[arg(r_u8, 0, 1), arg(dyn_debug, 1, 1)] },
        &[arg(r_u8, 0, 1)], arg(unit, 0, 0),
    );
    assert_eq!(res_try, Ok(()), "try_as_dyn with dyn DebugForm + '1 must succeed: {res_try:?}");
    assert!(reach(&rec_try, 0).contains(&1), "T: 'a + U: TryAsDynCompatible<'a> must enforce '0: '1: {:?}", rec_try.subsets);
    assert!(!reach(&rec_try, 1).contains(&0), "T: 'a is one-way, must not force '1: '0: {:?}", rec_try.subsets);

    // Negative soundness control 1: `try_as_dyn<'a, &'0 u8, dyn DebugForm + 'static>` MUST
    // constrain `'0: 'static` via `'0: 'a == 'u: 'static`.
    let p_early = params(1, &[]);
    let mut rec_static = Rec { subsets: SVec::new(), statics: SVec::new(), placeholders: SVec::new(), next: 100 };
    let clauses_static_dyn = [
        ClauseIn::TypeOutlives { subject: ty(p0, NONE), region: Region::Param(0) },
        ClauseIn::TraitOf { trait_ref: ty(try_clause_p1, &[Region::Param(0)]), binders: 0 },
        ClauseIn::TypeOutlives { subject: ty(p1, NONE), region: Region::Static },
    ];
    let sig_static_dyn = Sig { item: F_TRY_AS_DYN, params: ParamsView::of(&p_early), inputs: &inputs_try, output: ty(unit, NONE), clauses: &clauses_static_dyn };
    let origins = [Origin(0), Origin(1), Origin(2)];
    let res_static = {
        let mut cx = Cx { types: &cols, sig_types: &cols, selected: &TryDynSel, owner: &Own::default(), origins: &origins, out: &mut rec_static };
        call(
            &mut cx,
            Callee::Fn(sig_static_dyn),
            GenericArgs { lifetimes: range(1, 1), types: &[arg(r_u8, 0, 1), arg(dyn_debug, 2, 1)] },
            &[arg(r_u8, 0, 1)],
            arg(unit, 0, 0),
            alloc(),
        )
    };
    assert_eq!(res_static, Ok(()));
    assert!(
        reach(&rec_static, 0).contains(&STATIC_NODE),
        "CONTROL: &'local u8 -> dyn DebugForm + 'static MUST constrain 'local: 'static: subsets={:?}, statics={:?}",
        rec_static.subsets,
        rec_static.statics,
    );

    // Negative soundness control 2: non-`Dynamic` subject `u8: TryAsDynCompatible<'a>` MUST be
    // refused `Unselected`.
    let (res_bad, _) = run(
        &cols, &TryDynSel, &Own::default(), 0,
        Callee::Fn(sig_info), GenericArgs { lifetimes: SlotRange::EMPTY, types: &[arg(u8_, 0, 0)] },
        &[], arg(unit, 0, 0),
    );
    assert!(
        matches!(res_bad, Err(Refused::Unselected { trait_: T_TRY_AS_DYN, .. })),
        "CONTROL: non-Dynamic subject u8 must be refused Unselected for TryAsDynCompatible: {res_bad:?}",
    );
}

/// Wave 5 Cluster B soundness (`wasmtime::Memory::data_and_store_mut` / `From<&'a mut T> for
/// StoreContextMut<'a, T::Data>` with `Caller<'b, Host>` carrying lifetime `'b`):
/// - Positive: when `AsContext::Data` is selected for `Caller<'b, Host>`, `'a` ties invariantly
///   AND `AsContext`'s impl where-clause (`where 'b: 'static`) IS enforced on `'b`!
/// - Negative soundness control: when `AsContext::Data` is `Unanswered` (not recorded by typeck),
///   `blaze-borrow` MUST refuse `Unselected(Unanswered)` rather than unsoundly skipping the alias
///   (which would miss `'b: 'static`).
#[test]
fn impl_header_alias_enforces_assoc_impl_where_clauses() {
    use blaze_typeck::region_facts::Outlives;

    const T_FROM: ItemKey = ItemKey(0x9060);
    const T_ASCX: ItemKey = ItemKey(0x9061);
    const K_STORE: ItemKey = ItemKey(0x9062);
    const K_CALLER: ItemKey = ItemKey(0x9063);
    const K_HOST: ItemKey = ItemKey(0x9064);
    const IMP_FROM: ItemKey = ItemKey(0x9065);
    const IMP_ASCX: ItemKey = ItemKey(0x9066);
    const F_CONVERT: ItemKey = ItemKey(0x9067);
    const DATA: Name = Name::of("Data");

    let mut t = Tys::new();
    let p0 = t.param(0);
    let p1 = t.param(1);
    let host = t.adt(K_HOST, &[]);
    let caller = t.adt(K_CALLER, &[host]);
    let mut_p0 = t.rm(p0);
    let mut_caller = t.rm(caller);
    let p0_data = t.alias(T_ASCX, DATA, &[p0]);
    let store_data = t.adt(K_STORE, &[p0_data]);
    let store_host = t.adt(K_STORE, &[host]);
    let from_header = t.extrait(T_FROM, &[store_data, mut_p0]);
    let ascx_header = t.extrait(T_ASCX, &[caller]);
    let call_clause = t.extrait(T_FROM, &[p0, p1]);

    // `Caller<'b, T>` has 1 lifetime (`'b`) and 1 type parameter (`T`).
    let facts = Facts::default()
        .adt(K_STORE, 1, &[Variance::In, Variance::In])
        .adt(K_CALLER, 1, &[Variance::In, Variance::In])
        .adt(K_HOST, 0, &[])
        .trait_(T_FROM, 0)
        .trait_(T_ASCX, 0)
        .alias(T_ASCX, DATA, 0);
    let cols = Columns::new(t.t.view(), &facts, &facts);

    let ascx_outlives = [Outlives::Region { long: Region::Param(0), short: Region::Static }];
    struct WasmSel<'a> {
        from_head: ImplHead<'a>,
        ascx: Option<(ImplHead<'a>, ImplValue<'a>)>,
        ascx_where: crate::inst::ImplWhere<'a>,
    }
    impl Selected for WasmSel<'_> {
        fn select(&self, ask: Ask) -> Selection<'_> {
            if ask.trait_ == T_FROM && ask.name.is_none() {
                Selection::Impl { head: self.from_head, value: None }
            } else if ask.trait_ == T_ASCX && ask.name == Some(DATA) {
                match self.ascx {
                    Some((head, value)) => Selection::Impl { head, value: Some(value) },
                    None => Selection::Not(NotSelected::Unanswered),
                }
            } else {
                Selection::Not(NotSelected::Unanswered)
            }
        }
        fn impl_where(&self, imp: ItemKey) -> Result<crate::inst::ImplWhere<'_>, NotSelected> {
            if imp == IMP_ASCX { Ok(self.ascx_where) } else { Ok(crate::inst::ImplWhere::NONE) }
        }
    }

    let from_head = ImplHead { imp: IMP_FROM, lifetimes: 1, types: 1, trait_ref: ty(from_header, P00) };
    let ascx_head = ImplHead { imp: IMP_ASCX, lifetimes: 1, types: 0, trait_ref: ty(ascx_header, P0) };
    let ascx_val = ImplValue { value: ty(host, NONE), gat_lifetimes: 0 };
    let ascx_where = crate::inst::ImplWhere { params: &[], outlives: &ascx_outlives, views: &[] };

    let p = params(0, &[]);
    let inputs = [ty(p1, NONE)];
    let clauses = [ClauseIn::TraitOf { trait_ref: ty(call_clause, NONE), binders: 0 }];
    let sig = Sig { item: F_CONVERT, params: ParamsView::of(&p), inputs: &inputs, output: ty(p0, NONE), clauses: &clauses };

    // Slots: 0 = `'a` on `&'0 mut Caller<'1, Host>`, 1 = `'b` on `Caller<'1, Host>`, 2 = `'s` on `StoreContextMut<'2, Host>`.
    let types = [arg(store_host, 2, 1), arg(mut_caller, 0, 2)];
    let sel_with = WasmSel { from_head, ascx: Some((ascx_head, ascx_val)), ascx_where };
    let (res, rec) = run(
        &cols, &sel_with, &Own::default(), 3,
        Callee::Fn(sig), GenericArgs { lifetimes: SlotRange::EMPTY, types: &types },
        &[arg(mut_caller, 0, 2)], arg(store_host, 2, 1),
    );
    assert_eq!(res, Ok(()), "Caller<'b, Host> with AsContext selected must succeed: {res:?}");
    assert!(reach(&rec, 0).contains(&2) && reach(&rec, 2).contains(&0), "'0 and '2 must tie invariantly: {:?}", rec.subsets);
    assert!(
        reach(&rec, 1).contains(&STATIC_NODE),
        "AsContext impl where-clause `'b: 'static` MUST be enforced on slot 1: subsets={:?}, statics={:?}",
        rec.subsets,
        rec.statics,
    );

    // Negative soundness control: when `AsContext::Data` is `Unanswered`, MUST refuse `Unselected`
    // rather than unsoundly skipping normalization and dropping `'b: 'static`.
    let sel_missing = WasmSel { from_head, ascx: None, ascx_where };
    let (res_missing, _) = run(
        &cols, &sel_missing, &Own::default(), 3,
        Callee::Fn(sig), GenericArgs { lifetimes: SlotRange::EMPTY, types: &types },
        &[arg(mut_caller, 0, 2)], arg(store_host, 2, 1),
    );
    assert_eq!(
        res_missing,
        Err(Refused::Unselected { item: F_CONVERT, node: p0_data, trait_: T_ASCX, name: Some(DATA), why: NotSelected::Unanswered }),
        "CONTROL: missing AsContext::Data row must refuse Unanswered, never unsoundly skip",
    );
}
