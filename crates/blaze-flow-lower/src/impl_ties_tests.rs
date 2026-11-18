//! Lane B12, piece 1: a callee's bound `V: Visitor<'de>` answered by an IMPL at a call whose
//! argument for `V` is a concrete type (trait_ties.rs rule (d); serde's `deserialize_seq(
//! __Visitor { .. })`, thiserror's `as_display` over a concrete `Self`; before this piece every
//! such call was refused `CallTieUnplaced { why: ImplAnswers }`). Each test reads the heads from
//! text as the census does (lifetime part, generic shape, tie rows as blaze-ladder
//! borrows_ties.rs files them), the impls' texts as `Facts::impl_source` gives them, lowers a
//! body the real type checker checked (its `Checked::selected` rows name the impl), and checks
//! it with blaze-flow. NEVER COMPILED when written (2026-11-18, lane B12).
//!
//! Sources: the Reference, "Implementations", "Generic implementations" (an impl's lifetime is
//! constrained by its self type or its trait reference: one the self type holds is the call's
//! argument's there, one it does not hold only the trait reference constrains); "Subtyping and
//! Variance" (a trait's parameters are invariant); "Lifetime elision" (an elided lifetime of an
//! impl header is a fresh parameter of the impl); RFC 2094 (a loan in a region the body does
//! not choose is live at every point of it: its place dying is `borrow Invalidated`, rustc's
//! E0597).
//!
//! Every allocation of the heads goes to one named buffer per test ([`Inline`]); the fixture's
//! own builders (`Fx::tr`, `Fx::func`) take `std` vectors of clauses, and `findings_with`
//! answers in `std` strings: those are the fixture's, not this file's.

use std::vec;

use blaze_own::{Inline, Vec};
use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{ExprId, ItemKey, Mutbl, Name, PTy};

use crate::tests::findings_with;
use crate::{
    AdtFacts, AssocRead, CallBounds, CalleeFacts, Facts, Form, ImplSource, ImplText, ImplTrait, ImplTraitWhy, Plant, Refusal, Region, Shape, ShapeBuf, SigBuf,
    SigLife, Tie, TieMiss, Ties, read_impl_trait, read_shape, read_signature,
};

const BYTES: usize = 1 << 18;
type Buf<'x> = &'x Inline<BYTES>;

/// Every head's one type parameter has a trait bound and no type-outlives bound.
const TRAIT_BOUNDED: &[u32] = &[0];

const VIS_IMPL: &str = "impl<'de> Visitor<'de> for Vis<'de> { type Value = S; }";
/// The same self type, its trait lifetime FREE: rustc accepts `bad` with this impl.
const VIS_FREE_IMPL: &str = "impl<'a, 'de> Visitor<'de> for Vis<'a> { type Value = S; }";
const PLAIN_IMPL: &str = "impl<'de> Visitor<'de> for Plain { type Value = S; }";
const KEEP_IMPL: &str = "impl Visitor<'static> for Keep { type Value = S; }";
const ASSOC: &[(&str, &str)] = &[("Value", "type Value = S;")];

/// The heads, the ADTs that hold one lifetime, the impls' texts, the heads that write
/// lifetimes.
struct Heads<'x> {
    buf: Buf<'x>,
    sigs: Vec<(ItemKey, SigBuf<Buf<'x>>), Buf<'x>>,
    shapes: Vec<(ItemKey, ShapeBuf<Buf<'x>>), Buf<'x>>,
    ties: Vec<(ItemKey, Vec<Tie, Buf<'x>>, Vec<Region, Buf<'x>>), Buf<'x>>,
    with_lifetime: Vec<ItemKey, Buf<'x>>,
    impls: Vec<(ItemKey, &'static str), Buf<'x>>,
    writes: Vec<ItemKey, Buf<'x>>,
}

impl<'x> Heads<'x> {
    fn new(buf: Buf<'x>) -> Heads<'x> {
        Heads {
            buf,
            sigs: Vec::new_in(buf),
            shapes: Vec::new_in(buf),
            ties: Vec::new_in(buf),
            with_lifetime: Vec::new_in(buf),
            impls: Vec::new_in(buf),
            writes: Vec::new_in(buf),
        }
    }

    fn lifetimes(&self, k: ItemKey) -> Option<u32> {
        Some(u32::from(self.with_lifetime.contains(&k)))
    }

    /// `item`'s lifetime part, shape and a row per written trait bound (its parameter
    /// `Param(0)`, its trait `trait_`), each read from `text`.
    fn func(&mut self, fx: &Fx, item: ItemKey, trait_: Option<ItemKey>, text: &str, inputs: &[PTy], output: PTy) {
        let adt = |k: ItemKey| self.lifetimes(k);
        let mut sig = SigBuf::new_in(self.buf);
        if let Err(why) = read_signature(text, fx.t.view(), inputs, output, &adt, &mut sig, self.buf) {
            panic!("`{text}` was not read: {why:?}");
        }
        let mut shape = ShapeBuf::new_in(self.buf);
        let assoc = |_: ItemKey, _: Name| Some(AssocRead { trait_lifetimes: true, generic: false });
        if let Err(why) = read_shape(fx.t.view(), inputs, output, &adt, &assoc, &mut shape, self.buf) {
            panic!("`{text}` has no shape: {why:?}");
        }
        let (mut rows, mut regions) = (Vec::new_in(self.buf), Vec::new_in(self.buf));
        for tb in sig.trait_bounds.iter().filter(|tb| !tb.binder) {
            let lo = regions.len() as u32;
            regions.extend_from_slice(&sig.trait_bound_regions[tb.lo as usize..tb.hi as usize]);
            rows.push(Tie { param: 0, trait_: trait_.expect("a trait of the fixture"), lo, hi: regions.len() as u32, read: !tb.unread });
        }
        self.ties.push((item, rows, regions));
        self.shapes.push((item, shape));
        self.sigs.push((item, sig));
    }
}

impl Facts for Heads<'_> {
    fn adt(&self, k: ItemKey) -> Option<AdtFacts> {
        let lifetimes = u32::from(self.with_lifetime.contains(&k));
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: true, lifetimes, union: false, is_enum: false, is_box: false })
    }
    fn callee(&self, item: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: self.writes.contains(&item), self_ref: false })
    }
    fn sig(&self, item: ItemKey) -> Option<SigLife<'_>> {
        self.sigs.iter().find(|(k, _)| *k == item).map(|(_, s)| s.life())
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
    fn impl_source(&self, imp: ItemKey) -> Option<ImplSource<'_>> {
        self.impls.iter().find(|(k, _)| *k == imp).map(|&(_, text)| ImplSource { text, assoc: ASSOC })
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

/// The world: `S`; `Vis<'a>` (one lifetime), `Plain`, `Keep` (none); `trait Visitor<'de> {
/// type Value; }` and an impl of it for each of the three (`impls`: `Vis`'s text is
/// `vis_text`); `fn drive<'de, V: Visitor<'de>>(v: V, s: &'de S) -> V::Value` (its projection
/// gives the call a `Checked::selected` row; the output is `(V, V::Value)`) and `fn take<'de, V: Visitor<'de>>(v: V, s: &'de
/// S)` (no projection: no row); the owners `good`, `bad`, `free`, `keep`, `norow`.
struct World {
    fx: Fx,
    good: ItemKey,
    bad: ItemKey,
    free: ItemKey,
    keep: ItemKey,
    norow: ItemKey,
}

fn world<'x>(buf: Buf<'x>, vis_text: &'static str) -> (World, Heads<'x>) {
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
    let vis_impl = fx.imp(vis_text, 0, vt, Some((vis, &[])), &[("Value", st)], vec![]);
    let plain_impl = fx.imp(PLAIN_IMPL, 0, pt, Some((vis, &[])), &[("Value", st)], vec![]);
    let keep_impl = fx.imp(KEEP_IMPL, 0, kt, Some((vis, &[])), &[("Value", st)], vec![]);
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
    heads.impls.push((vis_impl, vis_text));
    heads.impls.push((plain_impl, PLAIN_IMPL));
    heads.impls.push((keep_impl, KEEP_IMPL));
    heads.func(&fx, drive, Some(vis), "fn drive<'de, V: Visitor<'de>>(v: V, s: &'de S) -> (V, V::Value) { todo() }", &[p0, rs], out);
    heads.writes.push(drive);
    heads.func(&fx, take, Some(vis), "fn take<'de, V: Visitor<'de>>(v: V, s: &'de S) {}", &[p0, rs], unit);
    heads.writes.push(take);
    heads.func(&fx, good, None, "fn good<'x>(v: Vis<'x>, s: &'x S) -> (Vis<'x>, S) { drive(v, s) }", &[vt, rs], vo);
    heads.func(&fx, bad, None, "fn bad<'x>(v: Vis<'x>, s: S) -> (Vis<'x>, S) { drive(v, &s) }", &[vt, st], vo);
    heads.func(&fx, free, None, "fn free(p: Plain, s: S) -> (Plain, S) { drive(p, &s) }", &[pt, st], po);
    heads.func(&fx, keep, None, "fn keep(k: Keep, s: S) -> (Keep, S) { drive(k, &s) }", &[kt, st], ko);
    heads.func(&fx, norow, None, "fn norow(p: Plain, s: S) { take(p, &s); }", &[pt, st], unit);
    (World { fx, good, bad, free, keep, norow }, heads)
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
    let got = findings_with(&w.fx, &b, w.bad, root, &heads, Plant::TiesFresh);
    std::println!("impl_answers bad, fresh (control): {got:?}");
    assert_eq!(got, clean(), "the control: a fresh `'de` misses E0597");
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
    let got = findings_with(&w.fx, &b, w.keep, root, &heads, Plant::TiesFresh);
    std::println!("impl_answers keep, fresh (control): {got:?}");
    assert_eq!(got, clean(), "the control: a fresh `'de` misses E0597");
}

/// A callee whose signature names no projection of the bound's trait gives the call no
/// `Checked::selected` row: which impl answers is not in the record, so the call is REFUSED by
/// name (`ImplAnswers`), never related with a fresh `'de`.
/// Prints `impl_answers norow: Err(Refusal { form: CallTieUnplaced { .., param: 0, why:
/// ImplAnswers }, .. })`.
#[test]
fn no_selected_row_is_refused() {
    let inline = Inline::<BYTES>::new();
    let (w, heads) = world(&inline, VIS_IMPL);
    let mut b = Bb::new();
    let root = call_body(&mut b, "p", "take", true, false);
    let got = findings_with(&w.fx, &b, w.norow, root, &heads, Plant::None);
    std::println!("impl_answers norow: {got:?}");
    assert!(matches!(got, Err(Refusal { form: Form::CallTieUnplaced { param: 0, why: TieMiss::ImplAnswers, .. }, .. })), "{got:?}");
}

/// The reader's half (sig/impl_life.rs `read_impl_trait`), one header at a time, the trait
/// declaring one lifetime (`two` for the count case): what each trait lifetime is, the bounds,
/// the type-outlives, the refusals.
/// Prints `read_impl_trait: ok` after the asserts.
#[test]
fn impl_headers_are_read_for_their_trait_reference() {
    let inline = Inline::<BYTES>::new();
    let mut fx = Fx::new();
    let v = fx.strukt("Vis", 0, &[]);
    let vt = fx.adt(v, &[]);
    let u = fx.strukt("U", 0, &[]);
    let ut = fx.adt(u, &[]);
    let ru = fx.r(Mutbl::Not, ut);
    let pl = fx.strukt("Plain", 0, &[]);
    let pt = fx.adt(pl, &[]);
    fx.finish();
    let adt = |k: ItemKey| Some(u32::from(k == v));
    let read = |text: &'static str, self_ty: PTy, lifetimes: u32| {
        let mut out = ImplTrait::new_in(&inline);
        let got = read_impl_trait(ImplText { text, self_ty, assoc: &[] }, fx.t.view(), &adt, lifetimes, &mut out, &inline);
        (got, out)
    };
    // The self type's lifetime: position 0.
    let (got, it) = read(VIS_IMPL, vt, 1);
    assert_eq!(got, Ok(()));
    assert_eq!((&it.args[..], it.free, it.positions), (&[Region::SelfTy(0)][..], 0, 1));
    // thiserror's `AsDisplay`: `'a` free, outlived by `T`, which the self type `&T` writes.
    let (got, it) = read("impl<'a, T> AsDisplay<'a> for &T where T: Display + ?Sized + 'a { type Target = &'a T; }", ru, 1);
    assert_eq!(got, Ok(()));
    assert_eq!((&it.args[..], &it.outlived[..], it.free, it.positions), (&[Region::Named(0)][..], &[Region::Named(0)][..], 1, 1));
    // A declared bound from a free lifetime to the self type's: `'de: 'a`.
    let (got, it) = read("impl<'a, 'de: 'a> Visitor<'de> for Vis<'a> {}", vt, 1);
    assert_eq!(got, Ok(()));
    assert_eq!(&it.args[..], &[Region::Named(0)][..]);
    assert!(it.bounds.contains(&(Region::Named(0), Region::SelfTy(0))), "{:?}", &it.bounds[..]);
    // Elided in the header: a fresh lifetime of the impl.
    let (got, it) = read("impl Visitor for Plain {}", pt, 1);
    assert_eq!(got, Ok(()));
    assert_eq!((&it.args[..], it.free), (&[Region::Named(0)][..], 1));
    // `'static`.
    let (got, it) = read(KEEP_IMPL, pt, 1);
    assert_eq!(got, Ok(()));
    assert_eq!(&it.args[..], &[Region::Static][..]);
    // Not read: a trait bound's lifetime argument in the header; another count; no trait.
    let (got, _) = read("impl<'de, T> Visitor<'de> for Vis<'de> where T: Deserialize<'de> {}", vt, 1);
    assert_eq!(got, Err(ImplTraitWhy::Predicates));
    let (got, _) = read("impl<'a> Visitor<'a, 'a> for Plain {}", pt, 1);
    assert_eq!(got, Err(ImplTraitWhy::Count { want: 1, have: 2 }));
    let (got, _) = read("impl<'a> Vis<'a> {}", vt, 1);
    assert_eq!(got, Err(ImplTraitWhy::Inherent));
    std::println!("read_impl_trait: ok");
}
