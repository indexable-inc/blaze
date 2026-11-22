//! Piece sig-assoc-projection: a member's `Self::Name` borrows through the impl's own `type
//! Name = ..;`, and the impl's lifetimes are its self type's. Each test gives the heads as the
//! head records file them (D148: a fn's lifetime parameters and its signature column, made by
//! hand from the head the test declares; `Self::Name` normalized through the impl, as the head
//! lowering does), lowers a body checked by the real type checker, and checks it with
//! blaze-flow. What the PRODUCER must give for these heads (the impl's `'a` through `Self::Name`,
//! two lifetimes of one ADT) is stated in blaze-ladder tests/front2_region_rules.rs.
//!
//! Numbering (regions.rs): `Param(i)` is the container's lifetimes, then the item's own in
//! declaration order, then one per elided input; slots in the type's pre-order.
//!
//! Sources: the Reference, "Associated Items", "Associated types" (an impl's `type Name = T;`
//! is that implementation's associated type), "Implementations", "Generic implementations" (a
//! lifetime of the impl appears in its self type or trait: E0207), "Paths", "`Self`" (in an
//! impl, `Self` is the implementing type); "Await expressions": `.await` first calls
//! `IntoFuture::into_future` on its operand, so a builder's future holds what the builder
//! held; "Moved and copied types" and RFC 2094 (a loan is live while the value holding it may
//! still be used): moving the borrowed source while that value is live is E0505.

use std::string::String;
use std::vec;

use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{ExprId, ItemKey, Mutbl, Name, PTy};

use crate::region_fixture::{Decls, FnHead};
use crate::{AdtFacts, CalleeFacts, Facts, Form, Plant, Refusal, Region, Stats, UnreadWhy, Variance};

/// The fixtures' facts: each ADT in `life` declares one lifetime per time it is listed (the
/// count both the columns are laid by, [`Decls::adt`], and the lowering reads); each head's
/// region head ([`Decls`], laid in the checked record's own table); `self_ref` heads take
/// `&self`.
struct Lives {
    life: vec::Vec<ItemKey>,
    self_ref: vec::Vec<ItemKey>,
    decls: Decls,
}

impl Lives {
    fn new(life: &[ItemKey]) -> Lives {
        let mut decls = Decls::new();
        let mut seen: vec::Vec<ItemKey> = vec::Vec::new();
        for &k in life {
            if !seen.contains(&k) {
                seen.push(k);
                decls.adt(k, life.iter().filter(|x| **x == k).count() as u32);
            }
        }
        Lives { life: life.to_vec(), self_ref: vec::Vec::new(), decls }
    }

    fn lifetimes(&self, k: ItemKey) -> u32 {
        self.life.iter().filter(|x| **x == k).count() as u32
    }

    /// `item`'s region head: `count` lifetimes, those in `late` late-bound, its column over
    /// `fn(io[..last]) -> io[last]` `slots`.
    fn add(&mut self, _fx: &mut Fx, item: ItemKey, count: u32, late: &[u32], io: &[PTy], slots: &[Region]) {
        let Some((&output, inputs)) = io.split_last() else { panic!("{item:?}: a head has an output") };
        self.decls.func(item, FnHead { lifetimes: count, late, outlives: &[], inputs, output, slots });
    }
}

impl Facts for Lives {
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
    fn shape(&self, _: blaze_typeck::ItemKey) -> Option<crate::Shape<'_>> {
        None
    }
    fn object_trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<bool> {
        None
    }
    fn call_bounds(&self, _: blaze_typeck::ItemKey) -> Option<crate::CallBounds<'_>> {
        None
    }
    fn trait_lifetimes(&self, _: blaze_typeck::ItemKey) -> Option<u32> {
        None
    }
    fn alias_lifetimes(&self, _: blaze_typeck::ItemKey, _: blaze_typeck::Name) -> Option<u32> {
        None
    }
    fn ties(&self, _: blaze_typeck::ItemKey) -> Option<crate::Ties<'_>> {
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
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: false, lifetimes: self.lifetimes(k), union: false, is_enum: false, is_box: false })
    }
    /// `Builder<'a>`, `Pending<'a>`, `Holder<'a>` and `Two<'a, 'b>` hold `&'a S` (and `&'b
    /// S`): covariant in each lifetime; `S` has no parameter.
    fn adt_variances(&self, k: ItemKey) -> Option<&[Variance]> {
        Some(crate::region_fixture::all_co(self.lifetimes(k)))
    }
    fn callee(&self, k: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: false, self_ref: self.self_ref.contains(&k) })
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

/// The findings of one body, one line each, and the lowering's counts; or the refusal. The
/// heads are laid into the checked record's own table (region_fixture.rs `run`).
fn run(fx: &Fx, b: &Bb, owner: ItemKey, root: ExprId, facts: &Lives) -> Result<(vec::Vec<String>, Stats), Refusal> {
    let r = crate::region_fixture::run(fx, &b.view(owner, root), facts, &facts.decls, Plant::None)?;
    Ok((r.lines, r.stats))
}

/// A builder whose future borrows from what the builder borrows: `Builder<'a>` and `impl<'a>
/// IntoFuture for Builder<'a> { type IntoFuture = Pending<'a>; fn into_future(self) ->
/// Self::IntoFuture }` (`.await`'s first step, here called as the method it is). The impl's
/// `'a` is the container's `Param(0)`, and the output, `Self::IntoFuture`, is `Pending<'a>`
/// through the impl's own `type IntoFuture`: `[Param(0)]` in, `[Param(0)]` out (the producer's
/// half: front2_region_rules.rs). Then `fn f(s: S) { let b = mk(&s); let p = b.into_future();
/// h(s); wait(p); }` moves `s` while `p`, holding its loan, is live (E0505: `borrow
/// Invalidated`); `wait(p); h(s);` is clean, and both are related by the signature
/// (`calls_by_signature` counts `mk`, `into_future`, `h`, `wait`).
/// CONTROL (rule 23): the output column `[Param(0)]` is what carries the loan from the builder
/// to the future: `into_future`'s head with the output laid over a lifetime of its own (a
/// second, early `Param(1)`: the future unrelated to the builder, as if `Self::IntoFuture`
/// were not normalized through the impl) accepts the bad order.
/// Prints `a_builders_future_borrows_what_the_builder_borrows, bad order true: [borrow
/// Invalidated ..]`, `.. bad order false: []`, `.. unrelated output (control): []`.
#[test]
fn a_builders_future_borrows_what_the_builder_borrows() {
    let mut fx = Fx::new();
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let rs = fx.r(Mutbl::Not, st);
    let unit = fx.unit();
    let bk = fx.strukt("Builder", 0, &[]);
    let bt = fx.adt(bk, &[]);
    let pk = fx.strukt("Pending", 0, &[]);
    let pt = fx.adt(pk, &[]);
    let imp = fx.imp("impl Builder", 0, bt, None, &[], vec![]);
    let into = fx.impl_fn(imp, "into_future", true, &[bt], pt);
    let mk = fx.func("mk", 0, &[rs], bt, vec![]);
    let wait = fx.func("wait", 0, &[pt], unit, vec![]);
    let h = fx.func("h", 0, &[st], unit, vec![]);
    let f = fx.func("f", 0, &[st], unit, vec![]);
    fx.finish();
    let p0 = Region::Param(0);
    // `fn mk(s: &S) -> Builder<'_>`: one elided lifetime: `[Param(0), Param(0)]`; `fn wait(p:
    // Pending<'_>)`: `[Param(0)]`; `fn h(s: S)`, `fn f(s: S)`: none.
    let facts_with = |fx: &mut Fx, into_out: Region| {
        let mut facts = Lives::new(&[bk, pk]);
        let count = if into_out == p0 { 1 } else { 2 };
        facts.add(fx, into, count, &[], &[bt, pt], &[p0, into_out]);
        facts.add(fx, mk, 1, &[0], &[rs, bt], &[p0, p0]);
        facts.add(fx, wait, 1, &[0], &[pt, unit], &[p0]);
        facts.add(fx, h, 0, &[], &[st, unit], &[]);
        facts.add(fx, f, 0, &[], &[st, unit], &[]);
        facts
    };
    let body = |bad: bool| -> (Bb, ExprId) {
        let mut b = Bb::new();
        let ps = b.param("s");
        let (vb, vp) = (b.bind("b"), b.bind("p"));
        let ls = b.local(ps);
        let bor = b.borrow(Mutbl::Not, ls);
        let mkp = b.path(&["mk"]);
        let made = b.call(mkp, &[bor]);
        let st1 = b.let_(vb, None, made);
        let lb = b.local(vb);
        let fut = b.method(lb, "into_future", &[]);
        let st2 = b.let_(vp, None, fut);
        let hp = b.path(&["h"]);
        let ls2 = b.local(ps);
        let moved = b.call(hp, &[ls2]);
        let wp = b.path(&["wait"]);
        let lp = b.local(vp);
        let waited = b.call(wp, &[lp]);
        let (m, w) = (b.semi(moved), b.semi(waited));
        let root = if bad { b.block(&[st1, st2, m, w], None) } else { b.block(&[st1, st2, w, m], None) };
        (b, root)
    };
    let facts = facts_with(&mut fx, p0);
    for bad in [true, false] {
        let (b, root) = body(bad);
        let (lines, stats) = run(&fx, &b, f, root, &facts).expect("lowered");
        std::println!("a_builders_future_borrows_what_the_builder_borrows, bad order {bad}: {lines:?}");
        assert_eq!(lines.iter().any(|l| l.starts_with("borrow Invalidated")), bad, "{lines:?}");
        assert_eq!(lines.is_empty(), !bad, "{lines:?}");
        assert_eq!(stats.calls_by_signature, 4, "mk, into_future, h and wait by their signatures: {stats:?}");
    }
    let unrelated = facts_with(&mut fx, Region::Param(1));
    let (b, root) = body(true);
    let (lines, _) = run(&fx, &b, f, root, &unrelated).expect("lowered");
    std::println!("a_builders_future_borrows_what_the_builder_borrows, unrelated output (control): {lines:?}");
    assert!(lines.is_empty(), "CONTROL: a future unrelated to the builder misses E0505: {lines:?}");
}

/// The world of the `Self::Item` tests: `Holder<'a>` with `fn get(&self, k: &S) ->
/// Self::Item` where `type Item = &'a S`; `mk_h(&S) -> Holder<'_>`, `g(&S)`, `h(S)`,
/// `drop_h(Holder<'_>)`, and the owner `f2(s: S, k: S)`.
struct HolderWorld {
    fx: Fx,
    hk: ItemKey,
    get: ItemKey,
    f2: ItemKey,
    st: PTy,
    rs: PTy,
    ht: PTy,
    rht: PTy,
    unit: PTy,
    mk_h: ItemKey,
    g: ItemKey,
    h: ItemKey,
    drop_h: ItemKey,
}

fn holder_world() -> HolderWorld {
    let mut fx = Fx::new();
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let rs = fx.r(Mutbl::Not, st);
    let unit = fx.unit();
    let hk = fx.strukt("Holder", 0, &[]);
    let ht = fx.adt(hk, &[]);
    let rht = fx.r(Mutbl::Not, ht);
    let imp = fx.imp("impl Holder", 0, ht, None, &[], vec![]);
    let get = fx.impl_fn(imp, "get", true, &[rht, rs], rs);
    let mk_h = fx.func("mk_h", 0, &[rs], ht, vec![]);
    let g = fx.func("g", 0, &[rs], unit, vec![]);
    let h = fx.func("h", 0, &[st], unit, vec![]);
    let drop_h = fx.func("drop_h", 0, &[ht], unit, vec![]);
    let f2 = fx.func("f2", 0, &[st, st], unit, vec![]);
    fx.finish();
    HolderWorld { fx, hk, get, f2, st, rs, ht, rht, unit, mk_h, g, h, drop_h }
}

/// `What` is moved between `let r = hh.get(&k);` and `g(r);`, or after both.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Moved {
    SourceBefore,
    SourceAfter,
    HolderBefore,
}

/// `get`'s column through the impl's `type Item = &'a S`: the impl's `'a` `Param(0)`, the
/// elided `&self` `Param(1)` and `k` `Param(2)`: `&Holder<'a>` `[Param(1), Param(0)]`, `&S`
/// `[Param(2)]`, out `[Param(0)]`.
const GET_THROUGH_IMPL: [Region; 4] = [Region::Param(1), Region::Param(0), Region::Param(2), Region::Param(0)];

/// The column of a reading that does not normalize `Self::Item` and elides the output from
/// `&self` instead (the coarse path before this piece): out `[Param(1)]`, the `&self` loan.
const GET_FROM_SELF: [Region; 4] = [Region::Param(1), Region::Param(0), Region::Param(2), Region::Param(1)];

/// `fn f2(s: S, k: S) { let hh = mk_h(&s); let r = hh.get(&k); <move>; g(r); }` (or the move
/// after `g(r)`), with `get`'s head of column `get_col`, or no head of `get` at all (`None`).
fn holder_body(w: &mut HolderWorld, moved: Moved, get_col: Option<[Region; 4]>) -> Result<(vec::Vec<String>, Stats), Refusal> {
    let mut facts = Lives::new(&[w.hk]);
    facts.self_ref.push(w.get);
    let p0 = Region::Param(0);
    let (st, rs, ht, rht, unit) = (w.st, w.rs, w.ht, w.rht, w.unit);
    if let Some(col) = get_col {
        facts.add(&mut w.fx, w.get, 3, &[1, 2], &[rht, rs, rs], &col);
    }
    // `fn mk_h(s: &S) -> Holder<'_>`: `[Param(0), Param(0)]`; `fn g(r: &S)`: `[Param(0)]`;
    // `fn h(s: S)`: none; `fn drop_h(h: Holder<'_>)`: `[Param(0)]`; `fn f2(s: S, k: S)`: none.
    facts.add(&mut w.fx, w.mk_h, 1, &[0], &[rs, ht], &[p0, p0]);
    facts.add(&mut w.fx, w.g, 1, &[0], &[rs, unit], &[p0]);
    facts.add(&mut w.fx, w.h, 0, &[], &[st, unit], &[]);
    facts.add(&mut w.fx, w.drop_h, 1, &[0], &[ht, unit], &[p0]);
    facts.add(&mut w.fx, w.f2, 0, &[], &[st, st, unit], &[]);
    let mut b = Bb::new();
    let (ps, pk) = (b.param("s"), b.param("k"));
    let (vh, vr) = (b.bind("hh"), b.bind("r"));
    let ls = b.local(ps);
    let bor_s = b.borrow(Mutbl::Not, ls);
    let mkp = b.path(&["mk_h"]);
    let made = b.call(mkp, &[bor_s]);
    let st1 = b.let_(vh, None, made);
    let lh = b.local(vh);
    let lk = b.local(pk);
    let bor_k = b.borrow(Mutbl::Not, lk);
    let got = b.method(lh, "get", &[bor_k]);
    let st2 = b.let_(vr, None, got);
    let mv = match moved {
        Moved::SourceBefore | Moved::SourceAfter => {
            let hp = b.path(&["h"]);
            let ls2 = b.local(ps);
            b.call(hp, &[ls2])
        }
        Moved::HolderBefore => {
            let dp = b.path(&["drop_h"]);
            let lh2 = b.local(vh);
            b.call(dp, &[lh2])
        }
    };
    let gp = b.path(&["g"]);
    let lr = b.local(vr);
    let used = b.call(gp, &[lr]);
    let (m, u) = (b.semi(mv), b.semi(used));
    let root = if moved == Moved::SourceAfter { b.block(&[st1, st2, u, m], None) } else { b.block(&[st1, st2, m, u], None) };
    run(&w.fx, &b, w.f2, root, &facts)
}

/// A plain method returning `Self::Item`, borrowed: `type Item = &'a S` makes the result
/// borrow what the holder borrows (`'a`, the impl's `Param(0)`), not the `&self` loan and not
/// `k`: [`GET_THROUGH_IMPL`]. So moving the source `s` while `r` is live is E0505 (`borrow
/// Invalidated`), and after `g(r)` it is clean. Before this piece the text `Self::Item`
/// against the erased `&S` was `Unaligned`.
/// Prints `a_methods_self_item_borrows_through_the_impls_lifetime, source moved before: [borrow
/// Invalidated ..]`, `.. after: []`.
#[test]
fn a_methods_self_item_borrows_through_the_impls_lifetime() {
    let mut w = holder_world();
    let (before, stats) = holder_body(&mut w, Moved::SourceBefore, Some(GET_THROUGH_IMPL)).expect("lowered");
    std::println!("a_methods_self_item_borrows_through_the_impls_lifetime, source moved before: {before:?}");
    assert!(before.iter().any(|l| l.starts_with("borrow Invalidated")), "{before:?}");
    assert_eq!(stats.calls_by_signature, 4, "mk_h, get, h and g by their signatures: {stats:?}");
    let (after, _) = holder_body(&mut w, Moved::SourceAfter, Some(GET_THROUGH_IMPL)).expect("lowered");
    std::println!("a_methods_self_item_borrows_through_the_impls_lifetime, source moved after: {after:?}");
    assert!(after.is_empty(), "{after:?}");
}

/// The control (rule 23): the same `get` with `Self::Item` NOT normalized through the impl
/// ([`GET_FROM_SELF`]: the output elided from the receiver, the coarse answer a head took
/// before this piece) gives a different, over-approximate answer. Moving the HOLDER `hh`
/// while `r` is live is right (`r` borrows `'a`, what `hh` borrows, never `hh` itself: `type
/// Item = &'a S`), and clean through the impl; the planted column puts the `&self` loan in `r`
/// and reports `borrow Invalidated`. And with no head of `get` at all, the call is refused by
/// name (`RegionsUnread { item: get, why: NoHead }`), never related by elision (D148).
/// Prints `the_coarse_path_over_approximates_self_item: read []; from self [borrow Invalidated
/// ..]; no head Err(.. RegionsUnread ..)`.
#[test]
fn the_coarse_path_over_approximates_self_item() {
    let mut w = holder_world();
    let (read, stats) = holder_body(&mut w, Moved::HolderBefore, Some(GET_THROUGH_IMPL)).expect("lowered");
    let (coarse, coarse_stats) = holder_body(&mut w, Moved::HolderBefore, Some(GET_FROM_SELF)).expect("lowered");
    let none = holder_body(&mut w, Moved::HolderBefore, None);
    std::println!("the_coarse_path_over_approximates_self_item: read {read:?}; from self {coarse:?}; no head {none:?}");
    assert!(read.is_empty(), "through the impl's `type Item = &'a S`, `r` does not borrow `hh`: {read:?}");
    assert!(coarse.iter().any(|l| l.starts_with("borrow Invalidated")), "the coarse column ties `r` to the `&self` loan: {coarse:?}");
    assert_eq!(stats.calls_by_signature, 4, "mk_h, get, drop_h and g: {stats:?}");
    assert_eq!(coarse_stats.calls_by_signature, 4, "every call by its column: {coarse_stats:?}");
    let get = w.get;
    assert!(matches!(none, Err(Refusal { form: Form::RegionsUnread { item, why: UnreadWhy::NoHead }, .. }) if item == get), "{none:?}");
}

/// `Two<'a, 'b>` holds two loans apart: `fn mk<'a, 'b>(x: &'a S, y: &'b S) -> Two<'a, 'b>`,
/// `fn first<'a, 'b>(t: Two<'a, 'b>) -> &'a S`, and `fn f(s: S, k: S) { let t = mk(&s, &k);
/// let r = first(t); <move>; g(r); }`. `r` borrows what `'a` holds, `s`, and not `k`: moving `s`
/// while `r` is live is E0505 (`borrow Invalidated`), moving `k` is clean. The second is what
/// one slot per lifetime buys: under one slot per ADT, `'a` and `'b` were one origin and
/// `first`'s result held `k`'s loan too.
/// Heads: `mk` `[Param(0), Param(1), Param(0), Param(1)]`, `first` `[Param(0), Param(1),
/// Param(0)]` (both lifetimes late: in the inputs, in no bound), `g` `[Param(0)]`.
/// CONTROL (rule 23): `first`'s output on `'b` (`[Param(0), Param(1), Param(1)]`) makes the
/// `k` move the finding and the `s` move clean: the slots, not the ADT, decide.
/// Prints `two_lifetimes_of_one_adt_stay_apart: s moved [borrow Invalidated ..]; k moved []`,
/// then the control's pair.
#[test]
fn two_lifetimes_of_one_adt_stay_apart() {
    let mut fx = Fx::new();
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let rs = fx.r(Mutbl::Not, st);
    let unit = fx.unit();
    let tk = fx.strukt("Two", 0, &[]);
    let tt = fx.adt(tk, &[]);
    let mk = fx.func("mk", 0, &[rs, rs], tt, vec![]);
    let first = fx.func("first", 0, &[tt], rs, vec![]);
    let g = fx.func("g", 0, &[rs], unit, vec![]);
    let h = fx.func("h", 0, &[st], unit, vec![]);
    let f = fx.func("f", 0, &[st, st], unit, vec![]);
    fx.finish();
    let (a, b_) = (Region::Param(0), Region::Param(1));
    let lines_of = |fx: &mut Fx, first_out: Region| -> [vec::Vec<String>; 2] {
        let mut facts = Lives::new(&[tk, tk]);
        facts.add(fx, mk, 2, &[0, 1], &[rs, rs, tt], &[a, b_, a, b_]);
        facts.add(fx, first, 2, &[0, 1], &[tt, rs], &[a, b_, first_out]);
        facts.add(fx, g, 1, &[0], &[rs, unit], &[a]);
        facts.add(fx, h, 0, &[], &[st, unit], &[]);
        facts.add(fx, f, 0, &[], &[st, st, unit], &[]);
        let mut out = [vec::Vec::new(), vec::Vec::new()];
        for (i, move_s) in [true, false].into_iter().enumerate() {
            let mut b = Bb::new();
            let (ps, pk) = (b.param("s"), b.param("k"));
            let (vt, vr) = (b.bind("t"), b.bind("r"));
            let ls = b.local(ps);
            let bs = b.borrow(Mutbl::Not, ls);
            let lk = b.local(pk);
            let bk = b.borrow(Mutbl::Not, lk);
            let mkp = b.path(&["mk"]);
            let made = b.call(mkp, &[bs, bk]);
            let st1 = b.let_(vt, None, made);
            let fp = b.path(&["first"]);
            let lt = b.local(vt);
            let got = b.call(fp, &[lt]);
            let st2 = b.let_(vr, None, got);
            let hp = b.path(&["h"]);
            let moved = b.local(if move_s { ps } else { pk });
            let mv = b.call(hp, &[moved]);
            let gp = b.path(&["g"]);
            let lr = b.local(vr);
            let used = b.call(gp, &[lr]);
            let (m, u) = (b.semi(mv), b.semi(used));
            let root = b.block(&[st1, st2, m, u], None);
            let (lines, stats) = run(fx, &b, f, root, &facts).expect("lowered");
            assert_eq!(stats.calls_by_signature, 4, "mk, first, h and g by their signatures: {stats:?}");
            out[i] = lines;
        }
        out
    };
    let [s_moved, k_moved] = lines_of(&mut fx, a);
    std::println!("two_lifetimes_of_one_adt_stay_apart: s moved {s_moved:?}; k moved {k_moved:?}");
    assert!(s_moved.iter().any(|l| l.starts_with("borrow Invalidated")), "{s_moved:?}");
    assert!(k_moved.is_empty(), "`r` holds `'a`, `s`'s loan only: {k_moved:?}");
    let [s_moved, k_moved] = lines_of(&mut fx, b_);
    std::println!("two_lifetimes_of_one_adt_stay_apart, output on 'b (control): s moved {s_moved:?}; k moved {k_moved:?}");
    assert!(s_moved.is_empty(), "CONTROL: `r` holds `'b` only: {s_moved:?}");
    assert!(k_moved.iter().any(|l| l.starts_with("borrow Invalidated")), "CONTROL: `k`'s loan is in `r`: {k_moved:?}");
}
