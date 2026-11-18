//! Piece sig-assoc-projection: a member's `Self::Name` is read through the impl's own `type
//! Name = ..;` text, and the impl's lifetimes through its self type (sig/impl_life.rs). Each
//! test reads the signatures from text as the census does (blaze-ladder front2 borrows.rs),
//! lowers a body checked by the real type checker, and checks it with blaze-flow.
//!
//! Sources: the Reference, "Associated Items", "Associated types" (an impl's `type Name = T;`
//! is that implementation's associated type), "Implementations", "Generic implementations" (a
//! lifetime of the impl appears in its self type or trait: E0207), "Paths", "`Self`" (in an
//! impl, `Self` is the implementing type); "Await expressions": `.await` first calls
//! `IntoFuture::into_future` on its operand, so a builder's future holds what the builder
//! held; "Moved and copied types" and RFC 2094 (a loan is live while the value holding it may
//! still be used): moving the borrowed source while that value is live is E0505.

use std::format;
use std::string::String;
use std::vec;

use blaze_flow::record::check;
use blaze_own::Alloc;
use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{ExprId, InferCtx, ItemKey, Mutbl, Name, Needs, Outcome, PTy};

use crate::{
    AdtFacts, AssocWhy, CalleeFacts, Facts, ImplText, Plant, Refusal, Region, SigBuf, SigLife, SigRefusal, Stats, lower_planted, read_impl_signature,
    read_signature,
};

pub(crate) fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

/// The fixtures' facts: each ADT in `life` declares one lifetime per time it is listed; each head's lifetime part is
/// read from its text (with its impl's text for a member); `self_ref` heads take `&self`.
struct Lives {
    life: vec::Vec<ItemKey>,
    self_ref: vec::Vec<ItemKey>,
    sigs: vec::Vec<(ItemKey, SigBuf<Alloc<'static>>)>,
}

impl Lives {
    fn new(life: &[ItemKey]) -> Lives {
        Lives { life: life.to_vec(), self_ref: vec::Vec::new(), sigs: vec::Vec::new() }
    }

    fn read(&self, fx: &Fx, text: &str, imp: Option<ImplText<'_>>, inputs: &[PTy], output: PTy) -> Result<SigBuf<Alloc<'static>>, SigRefusal> {
        let life = &self.life;
        let adt = |k: ItemKey| Some(life.iter().filter(|x| **x == k).count() as u32);
        let mut buf = SigBuf::new_in(alloc());
        match imp {
            Some(i) => read_impl_signature(text, i, fx.t.view(), inputs, output, &adt, &mut buf, alloc())?,
            None => read_signature(text, fx.t.view(), inputs, output, &adt, &mut buf, alloc())?,
        }
        Ok(buf)
    }

    fn add(&mut self, fx: &Fx, item: ItemKey, text: &str, imp: Option<ImplText<'_>>, inputs: &[PTy], output: PTy) {
        match self.read(fx, text, imp, inputs, output) {
            Ok(buf) => self.sigs.push((item, buf)),
            Err(why) => panic!("`{text}` was not read: {why:?}"),
        }
    }
}

impl Facts for Lives {
    fn adt(&self, k: ItemKey) -> Option<AdtFacts> {
        let lifetimes = self.life.iter().filter(|x| **x == k).count() as u32;
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: false, lifetimes, union: false, is_enum: false, is_box: false })
    }
    fn callee(&self, k: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: false, self_ref: self.self_ref.contains(&k) })
    }
    fn sig(&self, item: ItemKey) -> Option<SigLife<'_>> {
        self.sigs.iter().find(|(k, _)| *k == item).map(|(_, s)| s.life())
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

/// The findings of one body, one line each, and the lowering's counts; or the refusal.
fn run(fx: &Fx, b: &Bb, owner: ItemKey, root: ExprId, facts: &Lives) -> Result<(vec::Vec<String>, Stats), Refusal> {
    let a = alloc();
    let view = b.view(owner, root);
    let mut cx = InferCtx::new_in(a);
    let c = match blaze_typeck::check(&view, fx, &mut cx, &Needs::new_in(a), a) {
        Ok(Outcome::Done(c)) => {
            assert!(c.diags.is_empty(), "the fixture type-checks clean: {:?}", c.diags);
            c
        }
        Ok(Outcome::Need(n)) => panic!("the world missed {:?}", n.questions),
        Err(e) => panic!("{e:?}"),
    };
    let lowered = lower_planted(&view, &c, facts, Plant::None, a)?;
    let rec = check(&lowered.flow, a).expect("bounded");
    let mut out = vec::Vec::new();
    for m in rec.moves.iter() {
        out.push(format!("move {:?} at {:?}", m.kind, m.at));
    }
    for f in rec.borrows.iter() {
        out.push(format!("borrow {f:?}"));
    }
    Ok((out, lowered.stats))
}

fn regions(buf: &SigBuf<Alloc<'static>>) -> (vec::Vec<Region>, vec::Vec<u32>, vec::Vec<(Region, Region)>) {
    (buf.regions.iter().copied().collect(), buf.ends.iter().copied().collect(), buf.bounds.iter().copied().collect())
}

const BUILDER_IMPL: &str = "impl<'a> IntoFuture for Builder<'a> {\n    type Output = ();\n    type IntoFuture = Pending<'a>;\n    fn into_future(self) -> Self::IntoFuture { Pending(self.0) }\n}";
const BUILDER_ASSOC: &[(&str, &str)] = &[("Output", "type Output = ();"), ("IntoFuture", "type IntoFuture = Pending<'a>;")];
const INTO_FUTURE: &str = "fn into_future(self) -> Self::IntoFuture { Pending(self.0) }";

/// A builder whose future borrows from what the builder borrows: `Builder<'a>` and `impl<'a>
/// IntoFuture for Builder<'a> { type IntoFuture = Pending<'a>; fn into_future(self) ->
/// Self::IntoFuture }` (`.await`'s first step, here called as the method it is). The impl's
/// `'a` is the receiver's position (`SelfTy(0)`), and the output, `Self::IntoFuture`, is
/// `Pending<'a>` by the impl's own member text: `[SelfTy(0)]` in, `[SelfTy(0)]` out. Before this
/// piece the path text was lined up as an ADT path and its elided lifetime refused
/// (`ElisionUndecided`); without the impl's text it is refused by name (`AssocWhy::NoImpl`).
/// Then `fn f(s: S) { let b = mk(&s); let p = b.into_future(); h(s); wait(p); }` moves `s` while
/// `p`, holding its loan, is live (E0505: `borrow Invalidated`); `wait(p); h(s);` is clean, and
/// both are related by the signature (`calls_by_signature` counts `mk`, `into_future`, `h`,
/// `wait`).
/// Prints `a_builders_future_borrows_what_the_builder_borrows: [SelfTy(0), SelfTy(0)]; bad
/// order [borrow Invalidated ..]; good order []`.
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
    let it = ImplText { text: BUILDER_IMPL, self_ty: bt, assoc: BUILDER_ASSOC };
    let mut facts = Lives::new(&[bk, pk]);
    // The read itself: the impl's `'a` is the receiver's position, in and out.
    let read = facts.read(&fx, INTO_FUTURE, Some(it), &[bt], pt).expect("read through the impl");
    let got = regions(&read);
    std::println!("a_builders_future_borrows_what_the_builder_borrows: {:?}", got.0);
    assert_eq!(got, (vec![Region::SelfTy(0), Region::SelfTy(0)], vec![1], vec![]));
    let without = facts.read(&fx, INTO_FUTURE, None, &[bt], pt).map(|b| regions(&b));
    assert!(matches!(without, Err(SigRefusal::Assoc { why: AssocWhy::NoImpl, .. })), "{without:?}");
    facts.add(&fx, into, INTO_FUTURE, Some(it), &[bt], pt);
    facts.add(&fx, mk, "fn mk(s: &S) -> Builder<'_> { Builder(s) }", None, &[rs], bt);
    facts.add(&fx, wait, "fn wait(p: Pending<'_>) {}", None, &[pt], unit);
    facts.add(&fx, h, "fn h(s: S) {}", None, &[st], unit);
    for bad in [true, false] {
        let own = if bad {
            "fn f(s: S) { let b = mk(&s); let p = b.into_future(); h(s); wait(p); }"
        } else {
            "fn f(s: S) { let b = mk(&s); let p = b.into_future(); wait(p); h(s); }"
        };
        let mut facts_f = Lives::new(&[bk, pk]);
        facts_f.sigs = core::mem::take(&mut facts.sigs);
        facts_f.add(&fx, f, own, None, &[st], unit);
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
        let (lines, stats) = run(&fx, &b, f, root, &facts_f).expect("lowered");
        std::println!("a_builders_future_borrows_what_the_builder_borrows, bad order {bad}: {lines:?}");
        assert_eq!(lines.iter().any(|l| l.starts_with("borrow Invalidated")), bad, "{lines:?}");
        assert_eq!(lines.is_empty(), !bad, "{lines:?}");
        assert_eq!(stats.calls_by_signature, 4, "mk, into_future, h and wait by their signatures: {stats:?}");
        facts.sigs = core::mem::take(&mut facts_f.sigs);
        facts.sigs.retain(|(k, _)| *k != f);
    }
}

const HOLDER_IMPL: &str = "impl<'a> Get for Holder<'a> {\n    type Item = &'a S;\n    fn get(&self, k: &S) -> Self::Item { self.0 }\n}";
const HOLDER_ASSOC: &[(&str, &str)] = &[("Item", "type Item = &'a S;")];
const GET: &str = "fn get(&self, k: &S) -> Self::Item { self.0 }";

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

/// `fn f2(s: S, k: S) { let hh = mk_h(&s); let r = hh.get(&k); <move>; g(r); }` (or the move
/// after `g(r)`), with `get` read through its impl (`known`) or not read at all (the coarse
/// path: a known head with no lifetime part).
fn holder_body(w: &HolderWorld, moved: Moved, known: bool) -> Result<(vec::Vec<String>, Stats), Refusal> {
    let mut facts = Lives::new(&[w.hk]);
    facts.self_ref.push(w.get);
    let it = ImplText { text: HOLDER_IMPL, self_ty: w.ht, assoc: HOLDER_ASSOC };
    if known {
        facts.add(&w.fx, w.get, GET, Some(it), &[w.rht, w.rs], w.rs);
    }
    facts.add(&w.fx, w.mk_h, "fn mk_h(s: &S) -> Holder<'_> { Holder(s) }", None, &[w.rs], w.ht);
    facts.add(&w.fx, w.g, "fn g(r: &S) {}", None, &[w.rs], w.unit);
    facts.add(&w.fx, w.h, "fn h(s: S) {}", None, &[w.st], w.unit);
    facts.add(&w.fx, w.drop_h, "fn drop_h(h: Holder<'_>) {}", None, &[w.ht], w.unit);
    facts.add(&w.fx, w.f2, "fn f2(s: S, k: S) {}", None, &[w.st, w.st], w.unit);
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
/// borrow what the holder borrows (`'a`, the receiver's `SelfTy(0)`), not the `&self` loan
/// and not `k`: regions `[Anon(0), SelfTy(0)]`, `[Anon(1)]`, out `[SelfTy(0)]`. So moving the
/// source `s` while `r` is live is E0505 (`borrow Invalidated`), and after `g(r)` it is clean.
/// Before this piece the text `Self::Item` against the erased `&S` was `Unaligned`.
/// Prints `a_methods_self_item_borrows_through_the_impls_lifetime: [Anon(0), SelfTy(0),
/// Anon(1), SelfTy(0)]; source moved before [borrow Invalidated ..]; after []`.
#[test]
fn a_methods_self_item_borrows_through_the_impls_lifetime() {
    let w = holder_world();
    let facts = Lives::new(&[w.hk]);
    let it = ImplText { text: HOLDER_IMPL, self_ty: w.ht, assoc: HOLDER_ASSOC };
    let read = facts.read(&w.fx, GET, Some(it), &[w.rht, w.rs], w.rs).expect("read through the impl");
    let got = regions(&read);
    std::println!("a_methods_self_item_borrows_through_the_impls_lifetime: {:?}", got.0);
    assert_eq!(got, (vec![Region::Anon(0), Region::SelfTy(0), Region::Anon(1), Region::SelfTy(0)], vec![2, 3], vec![]));
    let (before, stats) = holder_body(&w, Moved::SourceBefore, true).expect("lowered");
    std::println!("a_methods_self_item_borrows_through_the_impls_lifetime, source moved before: {before:?}");
    assert!(before.iter().any(|l| l.starts_with("borrow Invalidated")), "{before:?}");
    assert_eq!(stats.calls_by_signature, 4, "mk_h, get, h and g by their signatures: {stats:?}");
    let (after, _) = holder_body(&w, Moved::SourceAfter, true).expect("lowered");
    std::println!("a_methods_self_item_borrows_through_the_impls_lifetime, source moved after: {after:?}");
    assert!(after.is_empty(), "{after:?}");
}

/// The control (rule 23): the same `get` with its lifetime part NOT read (the coarse path a
/// head took before this piece: `Self::Item` unread, the call related by elision from the
/// receiver) gives a different, over-approximate answer. Moving the HOLDER `hh` while `r` is
/// live is right (`r` borrows `'a`, what `hh` borrows, never `hh` itself: `type Item = &'a S`),
/// and clean through the impl; the coarse path puts the `&self` loan in `r` and reports
/// `borrow Invalidated`. Prints `the_coarse_path_over_approximates_self_item: read [];
/// coarse [borrow Invalidated ..]`.
#[test]
fn the_coarse_path_over_approximates_self_item() {
    let w = holder_world();
    let (read, stats) = holder_body(&w, Moved::HolderBefore, true).expect("lowered");
    let (coarse, coarse_stats) = holder_body(&w, Moved::HolderBefore, false).expect("lowered");
    std::println!("the_coarse_path_over_approximates_self_item: read {read:?}; coarse {coarse:?}");
    assert!(read.is_empty(), "through the impl's `type Item = &'a S`, `r` does not borrow `hh`: {read:?}");
    assert!(coarse.iter().any(|l| l.starts_with("borrow Invalidated")), "the coarse path ties `r` to the `&self` loan: {coarse:?}");
    assert_eq!(stats.calls_by_signature, 4, "mk_h, get, drop_h and g: {stats:?}");
    assert_eq!(coarse_stats.calls_by_signature, 3, "get went by elision: {coarse_stats:?}");
    assert_eq!(coarse_stats.calls_elided, 1, "{coarse_stats:?}");
}

/// The forms refused by name, never guessed (rule 3): a generic associated type's arguments,
/// a `<Self as Other>::Name` of another trait, a `type Name` whose text the impl lacks, a
/// member lifetime the self type does not hold, a member's elided lifetime. And a projection
/// the front end left unresolved (the erased type an `Alias`) stays `Hidden`, counted, not
/// refused. Prints `self_assoc_forms_are_refused_by_name: 5 refusals, 1 hidden`.
#[test]
fn self_assoc_forms_are_refused_by_name() {
    let w = holder_world();
    let facts = Lives::new(&[w.hk]);
    let fx = &w.fx;
    let refused = |text: &str, assoc: &[(&str, &str)], imp_text: &str| -> Option<AssocWhy> {
        let it = ImplText { text: imp_text, self_ty: w.ht, assoc };
        match facts.read(fx, text, Some(it), &[w.rht, w.rs], w.rs) {
            Err(SigRefusal::Assoc { why, .. }) => Some(why),
            _ => None,
        }
    };
    let gat = refused("fn get(&self, k: &S) -> Self::Item<u8> { self.0 }", HOLDER_ASSOC, HOLDER_IMPL);
    assert_eq!(gat, Some(AssocWhy::Gat));
    let other = refused("fn get(&self, k: &S) -> <Self as Other>::Item { self.0 }", HOLDER_ASSOC, HOLDER_IMPL);
    assert_eq!(other, Some(AssocWhy::OtherTrait));
    let none = refused(GET, &[], HOLDER_IMPL);
    assert_eq!(none, Some(AssocWhy::NoText));
    let trait_only = "impl<'a, 'b> Get<'b> for Holder<'a> {\n    type Item = &'b S;\n}";
    let lifetime = refused(GET, &[("Item", "type Item = &'b S;")], trait_only);
    assert_eq!(lifetime, Some(AssocWhy::Lifetime));
    let elided = refused(GET, &[("Item", "type Item = &S;")], HOLDER_IMPL);
    assert_eq!(elided, Some(AssocWhy::Elided));
    // The same `Self::Item` whose erased type is a projection: left to the `Hidden` arm.
    let mut fx2 = Fx::new();
    let s = fx2.strukt("S", 0, &[]);
    let st = fx2.adt(s, &[]);
    let rs = fx2.r(Mutbl::Not, st);
    let hk = fx2.strukt("Holder", 0, &[]);
    let ht = fx2.adt(hk, &[]);
    let rht = fx2.r(Mutbl::Not, ht);
    let tr = fx2.tr("Other", 0, vec![], &["Item"]);
    let proj = fx2.alias(tr, "Item", &[ht]);
    fx2.finish();
    let hidden_facts = Lives::new(&[hk]);
    let it = ImplText { text: HOLDER_IMPL, self_ty: ht, assoc: HOLDER_ASSOC };
    let read = hidden_facts.read(&fx2, GET, Some(it), &[rht, rs], proj).expect("a hidden position is read, not refused");
    assert!(read.life().holds_hidden(), "{:?}", regions(&read));
    std::println!("self_assoc_forms_are_refused_by_name: 5 refusals, 1 hidden");
}

/// bstd's hello world calls `impl<'g> Given<'g> { fn checking(&self) -> Checking<'_, 'g, ()> }`
/// (blaze-std grant/made.rs). An ADT has one origin position per lifetime parameter, in
/// declared order (`AdtFacts::lifetimes`, the Reference, "Generic parameters": each is a
/// parameter of its own), so the head is read: `&self` is `[Anon(0), SelfTy(0)]`, the output
/// `[Anon(0), SelfTy(0)]`. Before, one position stood for all of an ADT's lifetimes and the
/// text was refused (`TwoLifetimesOneOrigin`), so hello world's borrows were not checked.
/// CONTROL (rule 23): told `Checking` declares ONE lifetime, the same text is `Unaligned`.
/// Prints `a_head_naming_two_lifetimes_of_one_adt_is_read: [Anon(0), SelfTy(0), Anon(0),
/// SelfTy(0)]; one-position control Err(Unaligned ..)`.
#[test]
fn a_head_naming_two_lifetimes_of_one_adt_is_read() {
    let mut fx = Fx::new();
    let unit = fx.unit();
    let gk = fx.strukt("Given", 0, &[]);
    let gt = fx.adt(gk, &[]);
    let rgt = fx.r(Mutbl::Not, gt);
    let ck = fx.strukt("Checking", 1, &[]);
    let ct = fx.adt(ck, &[unit]);
    fx.finish();
    const GIVEN_IMPL: &str = "impl<'g> Given<'g> {\n    pub fn checking(&self) -> Checking<'_, 'g, ()> { Checking::new(self) }\n}";
    const CHECKING: &str = "pub fn checking(&self) -> Checking<'_, 'g, ()> { Checking::new(self) }";
    let it = ImplText { text: GIVEN_IMPL, self_ty: gt, assoc: &[] };
    let facts = Lives::new(&[gk, ck, ck]);
    let read = facts.read(&fx, CHECKING, Some(it), &[rgt], ct).expect("two lifetimes of one ADT are two positions");
    let got = regions(&read);
    std::println!("a_head_naming_two_lifetimes_of_one_adt_is_read: {:?}", got.0);
    assert_eq!(got.0, vec![Region::Anon(0), Region::SelfTy(0), Region::Anon(0), Region::SelfTy(0)]);
    let one = Lives::new(&[gk, ck]);
    let control = one.read(&fx, CHECKING, Some(it), &[rgt], ct).map(|b| regions(&b));
    std::println!("a_head_naming_two_lifetimes_of_one_adt_is_read, one-position control: {control:?}");
    assert!(matches!(control, Err(SigRefusal::Unaligned { .. })), "{control:?}");
}

/// `Two<'a, 'b>` holds two loans apart: `fn mk<'a, 'b>(x: &'a S, y: &'b S) -> Two<'a, 'b>`,
/// `fn first<'a, 'b>(t: Two<'a, 'b>) -> &'a S`, and `fn f(s: S, k: S) { let t = mk(&s, &k);
/// let r = first(t); <move>; g(r); }`. `r` borrows what `'a` holds, `s`, and not `k`: moving `s`
/// while `r` is live is E0505 (`borrow Invalidated`), moving `k` is clean. The second is what
/// one position per lifetime buys: under one position per ADT, `'a` and `'b` were one origin
/// and `first`'s result held `k`'s loan too. Prints `two_lifetimes_of_one_adt_stay_apart: s
/// moved [borrow Invalidated ..]; k moved []`.
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
    let mut facts = Lives::new(&[tk, tk]);
    facts.add(&fx, mk, "fn mk<'a, 'b>(x: &'a S, y: &'b S) -> Two<'a, 'b> { Two(x, y) }", None, &[rs, rs], tt);
    facts.add(&fx, first, "fn first<'a, 'b>(t: Two<'a, 'b>) -> &'a S { t.0 }", None, &[tt], rs);
    facts.add(&fx, g, "fn g(r: &S) {}", None, &[rs], unit);
    facts.add(&fx, h, "fn h(s: S) {}", None, &[st], unit);
    facts.add(&fx, f, "fn f(s: S, k: S) {}", None, &[st, st], unit);
    let mut lines_of = [vec::Vec::new(), vec::Vec::new()];
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
        let (lines, stats) = run(&fx, &b, f, root, &facts).expect("lowered");
        assert_eq!(stats.calls_by_signature, 4, "mk, first, h and g by their signatures: {stats:?}");
        lines_of[i] = lines;
    }
    let [s_moved, k_moved] = lines_of;
    std::println!("two_lifetimes_of_one_adt_stay_apart: s moved {s_moved:?}; k moved {k_moved:?}");
    assert!(s_moved.iter().any(|l| l.starts_with("borrow Invalidated")), "{s_moved:?}");
    assert!(k_moved.is_empty(), "`r` holds `'a`, `s`'s loan only: {k_moved:?}");
}
