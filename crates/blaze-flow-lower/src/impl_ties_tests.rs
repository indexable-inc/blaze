//! Lane B12, piece 1: a callee's bound `V: Visitor<'de>` answered by an IMPL at a call whose
//! argument for `V` is a concrete type (trait_ties.rs rule (d); serde's `deserialize_seq(
//! __Visitor { .. })`, thiserror's `as_display` over a concrete `Self`; before this piece every
//! such call was refused `CallTieUnplaced { why: ImplAnswers }`). Each test declares the heads'
//! region facts by hand (region_fixture.rs, D148: each fn's column and lifetimes, each impl's
//! header columns, as the head lowering lays them), the generic shape read from the erased
//! types, the tie rows as blaze-ladder borrows_ties.rs files them, lowers a body the real type
//! checker checked (its `Checked::selected` rows name the impl), and checks it with blaze-flow.
//! NEVER COMPILED when written (2026-11-18, lane B12; ported off the text readers 2026-11-19).
//!
//! Sources: the Reference, "Implementations", "Generic implementations" (an impl's lifetime is
//! constrained by its self type or its trait reference: one the self type holds is the call's
//! argument's there, one it does not hold only the trait reference constrains); "Subtyping and
//! Variance" (a trait's parameters are invariant); "Lifetime elision" (an elided lifetime of an
//! impl header is a fresh parameter of the impl); RFC 2094 (a loan in a region the body does
//! not choose is live at every point of it: its place dying is `borrow Invalidated`, rustc's
//! E0597).
//!
//! Every allocation of the shapes and tie rows goes to one named buffer per test ([`Inline`]);
//! the fixture's own builders (`Fx::tr`, `Fx::func`) take `std` vectors of clauses, and the
//! region fixture (region_fixture.rs) keeps its declarations in `std` vectors and answers in
//! `std` strings: those are the fixtures', not this file's.

use std::vec;

use blaze_own::{Inline, Vec};
use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{ExprId, ItemKey, Mutbl, Name, PNode, Selected};

use crate::region_fixture::{self, Decls, FnHead, ImplHead, P0, P1};
use crate::{AdtFacts, CallBounds, CalleeFacts, Facts, Form, Plant, Refusal, Region, Shape, ShapeBuf, Tie, TieMiss, Ties, read_shape};

const BYTES: usize = 1 << 18;
type Buf<'x> = &'x Inline<BYTES>;

/// Every head's one type parameter has a trait bound and no type-outlives bound.
const TRAIT_BOUNDED: &[u32] = &[0];

/// An impl of `Visitor` as the world declares it: its text (the world's name for it) and its
/// header's region facts (the numbering: the impl's lifetimes in declaration order).
#[derive(Clone, Copy)]
struct Header {
    text: &'static str,
    lifetimes: u32,
    self_slots: &'static [Region],
    trait_slots: &'static [Region],
}

/// `impl<'de> Visitor<'de> for Vis<'de> { type Value = S; }`: `'de` is `P0`, the self type
/// `Vis<'de>` `[P0]`, the trait reference `Visitor<'de>` `[P0]`.
const VIS_IMPL: Header = Header { text: "impl<'de> Visitor<'de> for Vis<'de> { type Value = S; }", lifetimes: 1, self_slots: &[P0], trait_slots: &[P0] };
/// The same self type, its trait lifetime FREE: rustc accepts `bad` with this impl.
/// `impl<'a, 'de> Visitor<'de> for Vis<'a> { type Value = S; }`: `'a` is `P0`, `'de` `P1`; the
/// self type `[P0]`, the trait reference `[P1]`.
const VIS_FREE_IMPL: Header = Header { text: "impl<'a, 'de> Visitor<'de> for Vis<'a> { type Value = S; }", lifetimes: 2, self_slots: &[P0], trait_slots: &[P1] };
/// `impl<'de> Visitor<'de> for Plain { type Value = S; }`: `'de` is `P0`, in no self type: `[]`,
/// the trait reference `[P0]`.
const PLAIN_IMPL: Header = Header { text: "impl<'de> Visitor<'de> for Plain { type Value = S; }", lifetimes: 1, self_slots: &[], trait_slots: &[P0] };
/// `impl Visitor<'static> for Keep { type Value = S; }`: no lifetime; the trait reference
/// `[Static]`.
const KEEP_IMPL: Header = Header { text: "impl Visitor<'static> for Keep { type Value = S; }", lifetimes: 0, self_slots: &[], trait_slots: &[Region::Static] };

/// The heads' shapes and tie rows, the ADTs that hold one lifetime, the heads that write
/// lifetimes, and the region facts declared by hand ([`Decls`]).
struct Heads<'x> {
    buf: Buf<'x>,
    shapes: Vec<(ItemKey, ShapeBuf<Buf<'x>>), Buf<'x>>,
    ties: Vec<(ItemKey, Vec<Tie, Buf<'x>>, Vec<Region, Buf<'x>>), Buf<'x>>,
    with_lifetime: Vec<ItemKey, Buf<'x>>,
    writes: Vec<ItemKey, Buf<'x>>,
    decls: Decls,
}

impl<'x> Heads<'x> {
    fn new(buf: Buf<'x>) -> Heads<'x> {
        Heads {
            buf,
            shapes: Vec::new_in(buf),
            ties: Vec::new_in(buf),
            with_lifetime: Vec::new_in(buf),
            writes: Vec::new_in(buf),
            decls: Decls::new(),
        }
    }

    /// `item`'s head (its column `head`), its shape read from the erased types, and per written
    /// trait bound of its type parameter `Param(0)` (`tie`: the trait and the bound's lifetime
    /// arguments, numbered as the head's) a row as blaze-ladder borrows_ties.rs files them (the
    /// old model) and its clause column 0, `ExTrait(trait, [Param(0)])` over the arguments, as
    /// heads_regions.rs lays it (the one model).
    fn func(&mut self, fx: &mut Fx, item: ItemKey, tie: Option<(ItemKey, &[Region])>, head: FnHead<'_>) {
        self.decls.func(item, head);
        let mut shape = ShapeBuf::new_in(self.buf);
        if let Err(why) = read_shape(fx.t.view(), head.inputs, head.output, &self.decls, &mut shape, self.buf) {
            panic!("{item:?} has no shape: {why:?}");
        }
        let (mut rows, mut regions) = (Vec::new_in(self.buf), Vec::new_in(self.buf));
        if let Some((trait_, args)) = tie {
            regions.extend_from_slice(args);
            rows.push(Tie { param: 0, trait_, lo: 0, hi: regions.len() as u32, read: true });
            let subject = fx.param(0);
            let list = fx.list(&[subject]);
            let clause = fx.ty(PNode::ExTrait(trait_, list));
            self.decls.clause(item, 0, 0, clause, args);
        }
        self.ties.push((item, rows, regions));
        self.shapes.push((item, shape));
    }
}

impl Facts for Heads<'_> {
    // Every other fact not known here, written out: `Facts` has no default answer.
    fn regions(&self) -> &dyn crate::RegionFacts {
        &crate::NoRegions
    }
    fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> {
        None
    }
    fn implied_bounds(&self) -> Option<(&[(u32, crate::Region)], &[(crate::Region, crate::Region)])> {
        None
    }
    fn constant_initializer(&self) -> bool {
        false
    }
    fn fn_binders(&self) -> bool {
        true
    }
    fn closure_bodies_checked(&self) -> bool {
        false
    }
    fn closure_requirements(&self, _: blaze_typeck::ExprId) -> &[crate::Requirement] {
        &[]
    }
    fn async_bodies_checked(&self) -> bool {
        false
    }
    fn is_future(&self, _: blaze_typeck::PTy) -> Option<bool> {
        None
    }
    fn object_trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<bool> {
        None
    }
    fn trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<u32> {
        None
    }
    fn alias_lifetimes(&self, _: blaze_typeck::ItemKey, _: blaze_typeck::Name) -> Option<u32> {
        None
    }
    fn field_param(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<u32> {
        None
    }
    fn field_lead(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<crate::FieldLead> {
        None
    }
    fn field_regions(&self, _: blaze_typeck::ItemKey, _: u32, _: u32) -> Option<&[crate::FieldRegion]> {
        None
    }
    fn copy_args(&self, _: blaze_typeck::ItemKey) -> Option<&[bool]> {
        None
    }
    fn adt(&self, k: ItemKey) -> Option<AdtFacts> {
        let lifetimes = u32::from(self.with_lifetime.contains(&k));
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: true, lifetimes, union: false, is_enum: false, is_box: false })
    }
    /// `Vis<'a>` holds `&'a S`: covariant in `'a`; `S`, `Plain` and `Keep` have no parameter.
    fn adt_variances(&self, k: ItemKey) -> Option<&[crate::Variance]> {
        Some(region_fixture::all_co(u32::from(self.with_lifetime.contains(&k))))
    }
    fn callee(&self, item: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: self.writes.contains(&item), self_ref: false })
    }
    fn shape(&self, item: ItemKey) -> Option<Shape<'_>> {
        self.shapes.iter().find(|(k, _)| *k == item).map(|(_, s)| s.shape())
    }
    fn call_bounds(&self, _: ItemKey) -> Option<CallBounds<'_>> {
        Some(CallBounds { outlives: &[], unread_below: 0, trait_bounded: TRAIT_BOUNDED })
    }
    fn ties(&self, item: ItemKey) -> Option<Ties<'_>> {
        self.ties.iter().find(|(k, _, _)| *k == item).map(|(_, rows, regions)| Ties { rows: &rows[..], regions: &regions[..] })
    }
    fn field(&self, _: ItemKey, _: u32, _: Name) -> Option<u32> {
        None
    }
    fn field_count(&self, _: ItemKey, _: u32) -> Option<u32> {
        None
    }
    fn param_copy(&self, _: u32) -> bool {
        false
    }
}

type Got = Result<vec::Vec<std::string::String>, Refusal>;

fn clean() -> Got {
    Ok(vec::Vec::new())
}

fn any(got: &Got, prefix: &str) -> bool {
    got.as_ref().is_ok_and(|l| l.iter().any(|l| l.starts_with(prefix)))
}

/// The findings of `root`, the body of `owner`, under `heads` (their region facts laid in the
/// body's record, region_fixture.rs).
fn findings_with(fx: &Fx, b: &Bb, owner: ItemKey, root: ExprId, heads: &Heads<'_>, plant: Plant) -> Got {
    region_fixture::findings(fx, b, owner, root, heads, &heads.decls, plant)
}

/// The world: `S`; `Vis<'a>` (one lifetime), `Plain`, `Keep` (none); `trait Visitor<'de> {
/// type Value; }` and an impl of it for each of the three (`Vis`'s is `vis`); `fn drive<'de, V: Visitor<'de>>(v: V, s: &'de S) -> V::Value` (its projection
/// gives the call a `Checked::selected` row; the output is `(V, V::Value)`) and `fn take<'de, V: Visitor<'de>>(v: V, s: &'de
/// S)` (no projection: its row is the bound `V: Visitor<'de>`'s); the owners `good`, `bad`,
/// `free`, `keep`, `norow`; `plain_impl` is `Plain`'s impl.
struct World {
    fx: Fx,
    good: ItemKey,
    bad: ItemKey,
    free: ItemKey,
    keep: ItemKey,
    norow: ItemKey,
    plain_impl: ItemKey,
}

fn world<'x>(buf: Buf<'x>, vis_header: Header) -> (World, Heads<'x>) {
    let mut fx = Fx::new();
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let rs = fx.r(Mutbl::Not, st);
    let unit = fx.unit();
    let vis = fx.tr("Visitor", 1, vec![], &["Value"]);
    let v = fx.strukt("Vis", 0, &[]);
    let vt = fx.adt(v, &[]);
    let pl = fx.strukt("Plain", 0, &[]);
    let pt = fx.adt(pl, &[]);
    let kp = fx.strukt("Keep", 0, &[]);
    let kt = fx.adt(kp, &[]);
    let vis_impl = fx.imp(vis_header.text, 0, vt, Some((vis, &[])), &[("Value", st)], vec![]);
    let plain_impl = fx.imp(PLAIN_IMPL.text, 0, pt, Some((vis, &[])), &[("Value", st)], vec![]);
    let keep_impl = fx.imp(KEEP_IMPL.text, 0, kt, Some((vis, &[])), &[("Value", st)], vec![]);
    let p0 = fx.param(0);
    let value = fx.alias(vis, "Value", &[p0]);
    // `(V, V::Value)`: the visitor comes back out, so its positions flow to the caller's
    // result whatever `Vis`'s variance (a covariant `Vis<'x>` passed in alone could shrink to
    // the call, and rustc would accept `bad`).
    let out = fx.tuple(&[p0, value]);
    let db = fx.bound(vis, &[p0]);
    let drive = fx.func("drive", 1, &[p0, rs], out, vec![db]);
    let tb = fx.bound(vis, &[p0]);
    let take = fx.func("take", 1, &[p0, rs], unit, vec![tb]);
    let (vo, po, ko) = (fx.tuple(&[vt, st]), fx.tuple(&[pt, st]), fx.tuple(&[kt, st]));
    let good = fx.func("good", 0, &[vt, rs], vo, vec![]);
    let bad = fx.func("bad", 0, &[vt, st], vo, vec![]);
    let free = fx.func("free", 0, &[pt, st], po, vec![]);
    let keep = fx.func("keep", 0, &[kt, st], ko, vec![]);
    let norow = fx.func("norow", 0, &[pt, st], unit, vec![]);
    fx.finish();
    let mut heads = Heads::new(buf);
    heads.with_lifetime.push(v);
    // `Vis<'a>` declares one lifetime (as `Facts::adt` says), `Visitor<'de>` one, and
    // `<_ as Visitor<'de>>::Value` the trait's one (no GAT).
    heads.decls.adt(v, 1);
    heads.decls.trait_(vis, 1);
    heads.decls.alias(vis, "Value", 1);
    for (imp, header, self_ty) in [(vis_impl, vis_header, vt), (plain_impl, PLAIN_IMPL, pt), (keep_impl, KEEP_IMPL, kt)] {
        let head = ImplHead { lifetimes: header.lifetimes, outlives: &[], self_ty, self_slots: header.self_slots, trait_ref: Some((vis, &[], header.trait_slots)) };
        heads.decls.imp(imp, head);
        // `type Value = S;`: no lifetime.
        heads.decls.impl_assoc(imp, "Value", st, &[]);
    }
    // `fn drive<'de, V: Visitor<'de>>(v: V, s: &'de S) -> (V, V::Value)`: `'de` is `P0`,
    // early-bound (the bound names it); `V` holds no slot, `&'de S` and `V::Value` (the trait's
    // `'de`) one each: `[P0, P0]`. Its row: `V: Visitor<'de>`, `[P0]`.
    heads.func(&mut fx, drive, Some((vis, &[P0])), FnHead { lifetimes: 1, late: &[], outlives: &[], inputs: &[p0, rs], output: out, slots: &[P0, P0] });
    heads.writes.push(drive);
    // `fn take<'de, V: Visitor<'de>>(v: V, s: &'de S)`: `[P0]`, early; its row `[P0]`.
    heads.func(&mut fx, take, Some((vis, &[P0])), FnHead { lifetimes: 1, late: &[], outlives: &[], inputs: &[p0, rs], output: unit, slots: &[P0] });
    heads.writes.push(take);
    // `fn good<'x>(v: Vis<'x>, s: &'x S) -> (Vis<'x>, S)`: `'x` is `P0`, late-bound: `[P0, P0, P0]`.
    heads.func(&mut fx, good, None, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[vt, rs], output: vo, slots: &[P0, P0, P0] });
    // `fn bad<'x>(v: Vis<'x>, s: S) -> (Vis<'x>, S)`: `[P0, P0]`, late-bound.
    heads.func(&mut fx, bad, None, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[vt, st], output: vo, slots: &[P0, P0] });
    // `fn free(p: Plain, s: S) -> (Plain, S)`, `fn keep(k: Keep, s: S) -> (Keep, S)` and `fn
    // norow(p: Plain, s: S)`: no lifetime.
    heads.func(&mut fx, free, None, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[pt, st], output: po, slots: &[] });
    heads.func(&mut fx, keep, None, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[kt, st], output: ko, slots: &[] });
    heads.func(&mut fx, norow, None, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[pt, st], output: unit, slots: &[] });
    (World { fx, good, bad, free, keep, norow, plain_impl }, heads)
}

/// `<callee>(<first>, <s or &s>)`, the body's tail (`tail`) or its one statement.
fn call_body(b: &mut Bb, first: &str, callee: &str, borrow: bool, tail: bool) -> ExprId {
    let (pf, ps) = (b.param(first), b.param("s"));
    let (lf, ls) = (b.local(pf), b.local(ps));
    let arg = if borrow { b.borrow(Mutbl::Not, ls) } else { ls };
    let f = b.path(&[callee]);
    let call = b.call(f, &[lf, arg]);
    if tail {
        b.block(&[], Some(call))
    } else {
        let st = b.semi(call);
        b.block(&[st], None)
    }
}

/// SERDE'S SHAPE. With `impl<'de> Visitor<'de> for Vis<'de>`, `drive(v, ..)` for `v: Vis<'x>`
/// answers `V: Visitor<'de>` through that impl, so the callee's `'de` IS `v`'s `'x` (the impl's
/// `'de` is the self type's position). `fn good<'x>(v: Vis<'x>, s: &'x S) -> (Vis<'x>, S) {
/// drive(v, s) }` is right. `fn bad<'x>(v: Vis<'x>, s: S) -> (Vis<'x>, S) { drive(v, &s) }` is
/// E0597 (rustc: "`s` does not live long enough"): the returned `V` is `Vis<'x>`, so `'de`
/// outlives `'x` and `&s` must too, a region the body does not choose. The one
/// constraint tested: the callee's `Named(0)` (trait `'de`) equals position 0 of the argument
/// for `V`.
/// CONTROLS (rule 23): `Plant::TiesFresh` (no tie: the trait lifetime fresh) ACCEPTS `bad`;
/// and with the header `impl<'a, 'de> Visitor<'de> for Vis<'a>` (the trait's `'de` free) the
/// same `bad` is clean, as rustc accepts it: the relation is the header's, not assumed.
/// Prints `impl_answers good: Ok([])`, `.. bad: Ok(["borrow Invalidated ..])`, `.. bad, fresh
/// (control): Ok([])`, `.. bad, free header (control): Ok([])`.
#[test]
fn an_impl_ties_the_trait_lifetime_to_the_argument() {
    let inline = Inline::<BYTES>::new();
    let (w, heads) = world(&inline, VIS_IMPL);
    let mut b = Bb::new();
    let root = call_body(&mut b, "v", "drive", false, true);
    let got = findings_with(&w.fx, &b, w.good, root, &heads, Plant::None);
    std::println!("impl_answers good: {got:?}");
    assert_eq!(got, clean(), "`'de` is `v`'s `'x`, which `s` outlives: clean");
    let mut b = Bb::new();
    let root = call_body(&mut b, "v", "drive", true, true);
    let got = findings_with(&w.fx, &b, w.bad, root, &heads, Plant::None);
    std::println!("impl_answers bad: {got:?}");
    assert!(any(&got, "borrow Invalidated"), "E0597: {got:?}");
    let inline = Inline::<BYTES>::new();
    let (w, heads) = world(&inline, VIS_FREE_IMPL);
    let got = findings_with(&w.fx, &b, w.bad, root, &heads, Plant::None);
    std::println!("impl_answers bad, free header (control): {got:?}");
    assert_eq!(got, clean(), "the control: the header leaves `'de` free, so rustc accepts it");
}

/// A FREE trait lifetime and a `'static` one. `impl<'de> Visitor<'de> for Plain`: `'de` is in
/// no self type, so the call answers it: `fn free(p: Plain, s: S) -> (Plain, S) { drive(p,
/// &s) }` is right (rustc infers `'de` local). `impl Visitor<'static> for Keep`: `fn keep(k:
/// Keep, s: S) -> (Keep, S) { drive(k, &s) }` is E0597 (`&s` must be `'static`).
/// CONTROL (rule 23): `Plant::TiesFresh` ACCEPTS `keep`.
/// Prints `impl_answers free: Ok([])`, `.. keep: Ok(["borrow Invalidated ..])`, `.. keep,
/// fresh (control): Ok([])`.
#[test]
fn a_free_or_static_impl_lifetime() {
    let inline = Inline::<BYTES>::new();
    let (w, heads) = world(&inline, VIS_IMPL);
    let mut b = Bb::new();
    let root = call_body(&mut b, "p", "drive", true, true);
    let got = findings_with(&w.fx, &b, w.free, root, &heads, Plant::None);
    std::println!("impl_answers free: {got:?}");
    assert_eq!(got, clean(), "a free `'de` is the call's own: clean");
    let mut b = Bb::new();
    let root = call_body(&mut b, "k", "drive", true, true);
    let got = findings_with(&w.fx, &b, w.keep, root, &heads, Plant::None);
    std::println!("impl_answers keep: {got:?}");
    assert!(any(&got, "borrow Invalidated"), "E0597 through `Visitor<'static>`: {got:?}");
}

/// A call with NO `Checked::selected` row for a bound an impl answers: which impl answers is
/// not in the record, so the call is REFUSED by name (`ImplAnswers`), never related with a
/// fresh `'de`. `fn norow(p: Plain, s: S) { take(p, &s); }`: `take` names no projection, so
/// its one row is the bound `Plain: Visitor<'de>`'s (typeck selected.rs, "Bound rows": every
/// clause of the callee whose instance may hold a region; an ADT may), the impl for `Plain`.
/// With it the call is related: `impl<'de> Visitor<'de> for Plain` leaves `'de` free, so the
/// call picks it local and `&s` is fine: CLEAN, as rustc accepts it (and as `free` through
/// `drive`, above). The record with its rows emptied is the case this test is named for.
/// Prints `impl_answers norow rows: [..]`, `.. norow: Ok([])`, `.. norow without rows:
/// Err(Refusal { form: CallTieUnplaced { .., param: 0, why: ImplAnswers }, .. })`.
#[test]
fn no_selected_row_is_refused() {
    let inline = Inline::<BYTES>::new();
    let (w, heads) = world(&inline, VIS_IMPL);
    let mut b = Bb::new();
    let root = call_body(&mut b, "p", "take", true, false);
    let view = b.view(w.norow, root);
    let mut c = region_fixture::checked(&w.fx, &view);
    let rows: vec::Vec<(Option<Name>, Selected)> = c.selected.iter().map(|r| (r.name, r.selected)).collect();
    std::println!("impl_answers norow rows: {rows:?}");
    assert!(
        matches!(rows[..], [(None, Selected::Impl { imp, .. })] if imp == w.plain_impl),
        "the checker's one row: the bound `Plain: Visitor<'de>`'s, the impl for `Plain`: {rows:?}"
    );
    let got = region_fixture::lower(&w.fx, &view, &mut c, &heads, &heads.decls, Plant::None).map(|r| r.lines);
    std::println!("impl_answers norow: {got:?}");
    assert_eq!(got, clean(), "a free `'de` is the call's own: clean, as rustc accepts it");
    let mut c = region_fixture::checked(&w.fx, &view);
    c.selected.clear();
    let got = region_fixture::lower(&w.fx, &view, &mut c, &heads, &heads.decls, Plant::None).map(|r| r.lines);
    std::println!("impl_answers norow without rows: {got:?}");
    assert!(matches!(got, Err(Refusal { form: Form::OneRule { why: crate::OneRuleWhy::Unselected { .. }, .. }, .. })), "{got:?}");
}
