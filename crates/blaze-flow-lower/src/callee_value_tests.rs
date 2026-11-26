//! Calls through values (lane B8; NEVER COMPILED when written, 2026-11-18): a fn pointer whose
//! result is a borrow, a closure called while a loan of it is held (`FnMut`), an `FnOnce`
//! called twice, and a closure of this body whose result holds its capture (callee_value.rs).
//!
//! Each body is a small fn of the fixture world, type-checked by the real checker, lowered and
//! checked by blaze-flow (tests.rs `findings_laid`: the heads' region facts declared by hand and
//! laid in the body's record, region_fixture.rs, D148). The world's lists of clauses are the
//! fixture's own `std` vectors (`Fx::func` takes one).

use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{ExprId, FnPtrHeader, ItemKey, LangItem, Mutbl, Name, PClause, PNode, PTy, PatId};

use crate::region_fixture::{BOUND1, Decls, FnHead, P0};
use crate::tests::{Declared, Sigs, Wide, findings_laid};
use crate::{AdtFacts, CalleeFacts, Facts, Form, Plant, Refusal, Requirement, UnreadWhy};

/// The fixtures' facts (tests.rs [`Wide`]: closures lowered, the heads' region facts declared)
/// with one closure's requirements ([`Facts::closure_requirements`]).
pub(crate) struct Lane {
    pub(crate) wide: Wide,
    /// The closure expression `reqs` are for.
    pub(crate) closure: Option<ExprId>,
    pub(crate) reqs: &'static [Requirement],
    pub(crate) fn_traits: [Option<ItemKey>; 3],
}

impl Lane {
    /// No head declared.
    pub(crate) fn new() -> Lane {
        Lane { wide: Wide { sigs: Sigs::new(), life: None }, closure: None, reqs: &[], fn_traits: [None; 3] }
    }

    /// The heads of `w`'s `g`, `m` and `h` declared ([`World`]).
    pub(crate) fn of(w: &World) -> Lane {
        let mut lane = Lane::new();
        // `fn g(_: &S)` and `fn m(_: &mut S)`: one elided input lifetime `P0`, late-bound.
        lane.wide.add(w.g, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[w.rs], output: w.unit, slots: &[P0] });
        lane.wide.add(w.m, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[w.rms], output: w.unit, slots: &[P0] });
        // `fn h(_: S)`: no lifetime.
        lane.wide.add(w.h, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[w.st], output: w.unit, slots: &[] });
        lane
    }
}

impl Declared for Lane {
    fn decls(&self) -> &Decls {
        self.wide.decls()
    }
}

impl Facts for Lane {
    // Every other fact not known here, written out: `Facts` has no default answer.
    fn regions(&self) -> &dyn crate::RegionFacts {
        &crate::NoRegions
    }
    fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> {
        None
    }
    fn adt_variances(&self, _: blaze_typeck::ItemKey) -> Option<&[crate::Variance]> {
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
    fn is_fn_trait(&self, k: ItemKey) -> bool {
        self.fn_traits.contains(&Some(k))
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
        self.wide.adt(k)
    }
    fn callee(&self, k: ItemKey) -> Option<CalleeFacts> {
        self.wide.callee(k)
    }
    fn closure_bodies_checked(&self) -> bool {
        true
    }
    fn closure_requirements(&self, closure: ExprId) -> &[Requirement] {
        if self.closure == Some(closure) { self.reqs } else { &[] }
    }
    fn field(&self, adt: ItemKey, variant: u32, name: Name) -> Option<u32> {
        self.wide.field(adt, variant, name)
    }
    fn field_count(&self, adt: ItemKey, variant: u32) -> Option<u32> {
        self.wide.field_count(adt, variant)
    }
    fn param_copy(&self, index: u32) -> bool {
        self.wide.param_copy(index)
    }
}

/// No clauses: the fixture's `func` takes its list as a `std` vector.
pub(crate) fn no_clauses() -> std::vec::Vec<PClause> {
    std::vec::Vec::new()
}

/// `struct S;` with `Copy` known (so `&S` is `Copy` and a `move` of `S` is a move), and
/// `fn g(_: &S)`, `fn m(_: &mut S)`, `fn h(_: S)` (their heads: [`Lane::of`]).
pub(crate) struct World {
    pub(crate) fx: Fx,
    pub(crate) st: PTy,
    pub(crate) rs: PTy,
    pub(crate) rms: PTy,
    pub(crate) unit: PTy,
    pub(crate) g: ItemKey,
    pub(crate) m: ItemKey,
    pub(crate) h: ItemKey,
}

/// The world, with `owners(fx, st, rs, unit)` declaring the owners before it is finished.
pub(crate) fn world_with(owners: impl FnOnce(&mut Fx, PTy, PTy, PTy)) -> World {
    let mut fx = Fx::new();
    let copy = fx.tr("Copy", 1, no_clauses(), &[]);
    fx.lang(LangItem::Copy, copy);
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let rs = fx.r(Mutbl::Not, st);
    let rms = fx.r(Mutbl::Mut, st);
    let unit = fx.unit();
    // core's `impl<T: ?Sized> Copy for &T {}` (core/src/marker.rs): the solver holds no builtin
    // `Copy` for a shared reference (blaze-solve builtin.rs `step_copy_clone`, as rustc's
    // `instantiate_constituent_tys_for_copy_clone_trait`: "impls in core"), so without it a
    // closure's by-value use of `a: &S` was a move and the capture by value, not by
    // reference (B12, the fix of closure_requirements_tests' `Malformed`: the requirement's
    // `Capture(3)` past a two-position closure value). `T: Sized` is implied by the fixture;
    // every `T` here is sized.
    let p0 = fx.param(0);
    let ref_t = fx.r(Mutbl::Not, p0);
    let _ = fx.imp("impl<T: ?Sized> Copy for &T {}", 1, ref_t, Some((copy, &[])), &[], no_clauses());
    let g = fx.func("g", 0, &[rs], unit, no_clauses());
    let m = fx.func("m", 0, &[rms], unit, no_clauses());
    let h = fx.func("h", 0, &[st], unit, no_clauses());
    owners(&mut fx, st, rs, unit);
    fx.finish();
    World { fx, st, rs, rms, unit, g, m, h }
}

/// `name(local)`.
pub(crate) fn call1(b: &mut Bb, name: &str, arg: PatId) -> ExprId {
    let f = b.path(&[name]);
    let a = b.local(arg);
    b.call(f, &[a])
}

/// `name(&local)` or `name(&mut local)`.
pub(crate) fn call_ref(b: &mut Bb, name: &str, m: Mutbl, arg: PatId) -> ExprId {
    let f = b.path(&[name]);
    let a = b.local(arg);
    let r = b.borrow(m, a);
    b.call(f, &[r])
}

/// A fn pointer returning a borrow (the Reference, "Lifetime elision": in a fn pointer type the
/// output takes the one input's lifetime, so `p(&s)` is a loan of `s` carried in the result):
/// `fn f(p: fn(&S) -> &S, s: S) { let r = p(&s); g(r); m(&mut s); }` is right (the result is
/// dead when `s` is borrowed mutably). Its negative, `.. let r = p(&s); m(&mut s); g(r); }`,
/// mutates `s` while the result holds its loan: rustc E0502, here `Invalidated`. Without the
/// owner's head regions the pointer's lifetimes are not known: refused by name (D148:
/// `RegionsUnread { item: f, NoHead }`, where the text's absence was `CalleeValueUnknown`), never
/// accepted.
/// Prints: `fn pointer result late: ["borrow Invalidated ..` and `fn pointer result early: []`.
#[test]
fn a_fn_pointer_result_is_a_loan_used_after_mutation() {
    let mut owner = None;
    let w = world_with(|fx, st, rs, unit| {
        let l = fx.list(&[rs, rs]);
        let fp = fx.ty(PNode::FnPtr(l, FnPtrHeader::RUST));
        owner = Some((fx.func("f", 0, &[fp, st], unit, no_clauses()), fp));
    });
    let (f, fp) = owner.expect("declared");
    // `fn f(p: fn(&S) -> &S, s: S)`: no lifetime of its own; the pointer's input and output are
    // its binder's (binder 1, the signature's root is 0; elision in the pointer's type).
    let f_head = FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[fp, w.st], output: w.unit, slots: &[BOUND1, BOUND1] };
    for late in [false, true] {
        let mut b = Bb::new();
        let (pp, ps) = (b.param("p"), b.param("s"));
        let lp = b.local(pp);
        let ls = b.local(ps);
        let bor = b.borrow(Mutbl::Not, ls);
        let call = b.call(lp, &[bor]);
        let pr = b.bind("r");
        let st1 = b.let_(pr, None, call);
        let use_r = call1(&mut b, "g", pr);
        let st_use = b.semi(use_r);
        let mutate = call_ref(&mut b, "m", Mutbl::Mut, ps);
        let st_mut = b.semi(mutate);
        let root = if late { b.block(&[st1, st_mut, st_use], None) } else { b.block(&[st1, st_use, st_mut], None) };
        let mut facts = Lane::of(&w);
        facts.wide.add(f, f_head);
        let got = findings_laid(&w.fx, &b, f, root, &facts, Plant::None);
        std::println!("fn pointer result {}: {got:?}", if late { "late" } else { "early" });
        let got = got.expect("lowered");
        if late {
            assert!(got.iter().any(|l| l.starts_with("borrow Invalidated")), "{got:?}");
        } else {
            assert!(got.is_empty(), "{got:?}");
        }
        let unread = findings_laid(&w.fx, &b, f, root, &Lane::of(&w), Plant::None);
        assert!(matches!(unread, Err(Refusal { form: Form::RegionsUnread { item, why: UnreadWhy::NoHead }, .. }) if item == f), "{unread:?}");
    }
}

/// An `FnMut` closure called while a shared loan of it is held (the Reference, "Closure
/// types", "Call traits and coercions": `FnMut::call_mut` takes `&mut self`): `fn f(s: S) {
/// let mut c = || m(&mut s); c(); c(); }` is right (each call's mutable borrow ends with it).
/// Its negative, `.. let r = &c; c(); let d = r; }`, calls `c` while `r` borrows it: rustc
/// E0502, here `Invalidated`.
/// Prints: `fn mut while borrowed: ["borrow Invalidated ..` and `fn mut twice: []`.
#[test]
fn an_fn_mut_called_while_borrowed() {
    let mut owner = None;
    let w = world_with(|fx, st, _rs, unit| owner = Some(fx.func("f", 0, &[st], unit, no_clauses())));
    let f = owner.expect("declared");
    for borrowed in [false, true] {
        let mut b = Bb::new();
        let s = b.param("s");
        let inner = call_ref(&mut b, "m", Mutbl::Mut, s);
        let cl = b.closure(&[], inner);
        let pc = b.bind_mut("c");
        let st1 = b.let_(pc, None, cl);
        let root = if borrowed {
            let lc = b.local(pc);
            let r = b.borrow(Mutbl::Not, lc);
            let pr = b.bind("r");
            let st2 = b.let_(pr, None, r);
            let lc2 = b.local(pc);
            let call = b.call(lc2, &[]);
            let st3 = b.semi(call);
            let lr = b.local(pr);
            let pd = b.bind("d");
            let st4 = b.let_(pd, None, lr);
            b.block(&[st1, st2, st3, st4], None)
        } else {
            let (l1, l2) = (b.local(pc), b.local(pc));
            let (c1, c2) = (b.call(l1, &[]), b.call(l2, &[]));
            let (st2, st3) = (b.semi(c1), b.semi(c2));
            b.block(&[st1, st2, st3], None)
        };
        let got = findings_laid(&w.fx, &b, f, root, &Lane::of(&w), Plant::None);
        std::println!("fn mut {}: {got:?}", if borrowed { "while borrowed" } else { "twice" });
        let got = got.expect("lowered");
        if borrowed {
            assert!(got.iter().any(|l| l.starts_with("borrow Invalidated")), "{got:?}");
        } else {
            assert!(got.is_empty(), "{got:?}");
        }
    }
}

/// An `FnOnce` closure called twice (the Reference, "Closure types": a closure that moves out
/// of a capture implements only `FnOnce`, whose `call_once` takes `self` by value): `fn f(s:
/// S) { let c = move || h(s); c(); }` is right; `.. c(); c(); }` uses `c` after its move:
/// rustc E0382, here a `move` finding.
/// Prints: `fn once twice: ["move ..` and `fn once once: []`.
#[test]
fn an_fn_once_called_twice() {
    let mut owner = None;
    let w = world_with(|fx, st, _rs, unit| owner = Some(fx.func("f", 0, &[st], unit, no_clauses())));
    let f = owner.expect("declared");
    for twice in [false, true] {
        let mut b = Bb::new();
        let s = b.param("s");
        let inner = call1(&mut b, "h", s);
        let cl = b.move_closure(&[], inner);
        let pc = b.bind("c");
        let st1 = b.let_(pc, None, cl);
        let l1 = b.local(pc);
        let c1 = b.call(l1, &[]);
        let st2 = b.semi(c1);
        let root = if twice {
            let l2 = b.local(pc);
            let c2 = b.call(l2, &[]);
            let st3 = b.semi(c2);
            b.block(&[st1, st2, st3], None)
        } else {
            b.block(&[st1, st2], None)
        };
        let got = findings_laid(&w.fx, &b, f, root, &Lane::of(&w), Plant::None);
        std::println!("fn once {}: {got:?}", if twice { "twice" } else { "once" });
        let got = got.expect("lowered");
        assert_eq!(got.iter().any(|l| l.starts_with("move ")), twice, "{got:?}");
        if !twice {
            assert!(got.is_empty(), "{got:?}");
        }
    }
}

/// A closure of this body whose result is a borrow of its capture (callee_value.rs): `fn f(s:
/// S) { let c = || &s; let r = c(); g(r); m(&mut s); }` is right. Its negative, `.. let r =
/// c(); m(&mut s); g(r); }`, mutates `s` while `r` holds the capture's loan: rustc E0502, here
/// refused by the stricter relation's name (`ClosureValueCall`), never a pass. CONTROL (rule
/// 23): `Plant::ClosureCallUnrelated` relates nothing into the result, and the negative is
/// accepted. Before this piece the call was refused (`CalleeValueUnknown`: the callee is no
/// body parameter). D190 step 3 means the one rule to relate it by the closure's own
/// signature, so that the negative is a reported `Invalidated` there (not yet tested here).
/// Prints: `closure result late: Err(.. ClosureValueCall ..)`, `closure result early: Ok([])` and
/// `closure result late, planted ClosureCallUnrelated: []`.
#[test]
fn a_closure_result_holds_its_capture() {
    let mut owner = None;
    let w = world_with(|fx, st, _rs, unit| owner = Some(fx.func("f", 0, &[st], unit, no_clauses())));
    let f = owner.expect("declared");
    for late in [false, true] {
        let mut b = Bb::new();
        let s = b.param("s");
        let ls = b.local(s);
        let inner = b.borrow(Mutbl::Not, ls);
        let cl = b.closure(&[], inner);
        let pc = b.bind("c");
        let st1 = b.let_(pc, None, cl);
        let lc = b.local(pc);
        let call = b.call(lc, &[]);
        let pr = b.bind("r");
        let st2 = b.let_(pr, None, call);
        let use_r = call1(&mut b, "g", pr);
        let st_use = b.semi(use_r);
        let mutate = call_ref(&mut b, "m", Mutbl::Mut, s);
        let st_mut = b.semi(mutate);
        let root = if late { b.block(&[st1, st2, st_mut, st_use], None) } else { b.block(&[st1, st2, st_use, st_mut], None) };
        const REQ: [crate::Requirement; 1] = [crate::Requirement { from: crate::closure::Need::Capture(0), into: crate::closure::Need::Sig(0) }];
        let mut lane = Lane::of(&w);
        lane.wide.add(f, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[w.st], output: w.unit, slots: &[] });
        lane.closure = Some(cl);
        lane.reqs = &REQ;
        let got = findings_laid(&w.fx, &b, f, root, &lane, Plant::None);
        std::println!("closure result {}: {got:?}", if late { "late" } else { "early" });
        if late {
            assert!(got.as_ref().is_ok_and(|l| l.iter().any(|s| s.starts_with("borrow Invalidated"))), "{got:?}");
            let planted = findings_laid(&w.fx, &b, f, root, &lane, Plant::NoRequirements);
            std::println!("closure result late, planted NoRequirements: {planted:?}");
            assert!(planted.as_ref().is_ok_and(|p| p.is_empty()), "{planted:?}");
        } else {
            assert_eq!(got, Ok(std::vec::Vec::new()), "{got:?}");
        }
    }
}

/// Target 2 (`precompiles/src/error.rs::add_errors_to_registry` shape): a closure coerced to a
/// higher-ranked trait object `dyn for<'a> Fn(&'a S) -> &'a S` (`SigFrom::Object`) reads the
/// expecting object's `Fn*` predicates and ties its input and output `Late` regions so returning
/// `data` does NOT trigger E0521 (`UnprovenOutlives`).
#[test]
fn object_coerced_hrtb_closure_ties_input_and_output_late_region() {
    use blaze_typeck::regions::Region;

    let mut items = None;
    let mut fn_trait = None;
    let mut fn_once_trait = None;
    let w = world_with(|fx, _st, rs, unit| {
        let sized = fx.tr("Sized", 1, no_clauses(), &[]);
        let unsize = fx.tr("Unsize", 2, no_clauses(), &[]);
        let coerce = fx.tr("CoerceUnsized", 2, no_clauses(), &[]);
        let f_tr = fx.tr("Fn", 2, no_clauses(), &[]);
        let fm_tr = fx.tr("FnMut", 2, no_clauses(), &[]);
        let fo_tr = fx.tr("FnOnce", 2, no_clauses(), &["Output"]);
        fx.lang(LangItem::Sized, sized);
        fx.lang(LangItem::Unsize, unsize);
        fx.lang(LangItem::CoerceUnsized, coerce);
        fx.lang(LangItem::Fn, f_tr);
        fx.lang(LangItem::FnMut, fm_tr);
        fx.lang(LangItem::FnOnce, fo_tr);
        let rs_list = fx.list(&[rs]);
        let tup = fx.ty(PNode::Tuple(rs_list));
        let tup_list = fx.list(&[tup]);
        let ex_fn = fx.ty(PNode::ExTrait(f_tr, tup_list));
        let ex_out = fx.ty(PNode::ExProj { trait_: fo_tr, name: Name::of("Output"), args: tup_list, term: rs });
        let preds = fx.list(&[ex_fn, ex_out]);
        let dyn_fn = fx.ty(PNode::Dynamic(preds));
        let r_dyn = fx.ty(PNode::Ref(Mutbl::Not, dyn_fn));
        let take = fx.func("take", 0, &[r_dyn], unit, no_clauses());
        let f = fx.func("f", 0, &[rs], unit, no_clauses());
        fn_trait = Some(f_tr);
        fn_once_trait = Some(fo_tr);
        items = Some((take, f, r_dyn));
    });
    let (take, f, r_dyn) = items.expect("declared");
    let f_tr = fn_trait.expect("Fn");
    let fo_tr = fn_once_trait.expect("FnOnce");

    // Build `fn f(keep: &S) { take(&|data| data); }`.
    let mut b = Bb::new();
    let _keep = b.param("keep");
    let p_data = b.bind("data");
    let l_data = b.local(p_data);
    let cl = b.closure(&[p_data], l_data);
    let r_cl = b.borrow(Mutbl::Not, cl);
    let callee = b.path(&["take"]);
    let call = b.call(callee, &[r_cl]);
    let st_call = b.semi(call);
    let root = b.block(&[st_call], None);

    let body = b.view(f, root);
    let mut ck = crate::region_fixture::checked(&w.fx, &body);

    // `take`'s declared signature: `fn take(_: &'0 (dyn for<'a> Fn(&'a S) -> &'a S + 'static))`.
    // In `take`'s sig, root `FnPtr` is binder 0, so the `Dynamic` node is binder 1.
    let late1 = Region::Late { binder: 1, var: 0 };
    let mut lane = Lane::of(&w);
    lane.fn_traits = [Some(f_tr), Some(fo_tr), None];
    lane.wide.sigs.decls.trait_(f_tr, 0);
    lane.wide.sigs.decls.trait_(fo_tr, 0);
    lane.wide.sigs.decls.alias(fo_tr, "Output", 0);
    lane.wide.add(
        take,
        FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[r_dyn], output: w.unit, slots: &[P0, Region::Static, late1, late1, late1] },
    );
    lane.wide.add(f, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[w.rs], output: w.unit, slots: &[P0] });
    let regions = lane.decls().lay(w.fx.t.view(), &mut ck.types);
    let with = crate::region_fixture::WithRegions { base: &lane, regions: &regions };
    let cf = crate::lower_closure_external(&body, &ck, cl, &with, crate::region_fixture::alloc()).expect("lower closure");
    let report = blaze_flow::check(&cf.lowered.flow, crate::region_fixture::alloc()).expect("flow check");
    assert!(report.moves.is_empty() && report.borrows.is_empty(), "HRTB dyn Fn closure returning `data` must have zero borrow errors: moves={} borrows={}", report.moves.len(), report.borrows.len());
    assert_eq!(
        cf.externals.iter().filter(|(_, ext)| *ext == crate::External::Late).count(),
        2,
        "both input and output positions must be tied as External::Late: {:?}",
        cf.externals
    );
}

/// Bonus (`retired_root_error_order_and_access_gas` shape): calling a fn-pointer parameter of a
/// closure (`|edit: fn(&mut S)| { edit(&mut s); }`) lowers cleanly instead of refusing with
/// `CalleeValueUnknown`, and still catches an active shared borrow across `edit(&mut s)`.
#[test]
fn closure_fn_ptr_param_call_lowers_cleanly() {
    let mut owner = None;
    let mut fn_ptr_ty = None;
    let w = world_with(|fx, st, _rs, unit| {
        let rms = fx.ty(PNode::Ref(Mutbl::Mut, st));
        let fp_list = fx.list(&[rms, unit]);
        let fp = fx.ty(PNode::FnPtr(fp_list, FnPtrHeader::RUST));
        fn_ptr_ty = Some(fp);
        owner = Some(fx.func("f", 0, &[st], unit, no_clauses()));
    });
    let f = owner.expect("declared");
    let fp = fn_ptr_ty.expect("fn ptr");

    for conflict in [false, true] {
        let mut b = Bb::new();
        let s = b.param("s");
        let p_edit = b.bind("edit");
        let l_edit = b.local(p_edit);
        let l_s = b.local(s);
        let m_s = b.borrow(Mutbl::Mut, l_s);
        let call_edit = b.call(l_edit, &[m_s]);
        let st_call = b.semi(call_edit);
        let inner = if conflict {
            let l_s2 = b.local(s);
            let r_s = b.borrow(Mutbl::Not, l_s2);
            let pr = b.bind("r");
            let st_r = b.let_(pr, None, r_s);
            let use_r = call1(&mut b, "g", pr);
            let st_use = b.semi(use_r);
            b.block(&[st_r, st_call, st_use], None)
        } else {
            b.block(&[st_call], None)
        };
        let cl = b.closure(&[p_edit], inner);
        let pc = b.bind("c");
        let st_let = b.let_(pc, None, cl);
        let root = b.block(&[st_let], None);

        let mut body = b.view(f, root);
        let written = [Some(fp)];
        body.types = w.fx.t.view();
        body.opt_types = &written;
        let mut ck = crate::region_fixture::checked(&w.fx, &body);

        let mut lane = Lane::of(&w);
        lane.wide.add(f, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[w.st], output: w.unit, slots: &[] });
        let regions = lane.decls().lay(w.fx.t.view(), &mut ck.types);
        let with = crate::region_fixture::WithRegions { base: &lane, regions: &regions };
        let cf = crate::lower_closure_external(&body, &ck, cl, &with, crate::region_fixture::alloc()).expect("lower closure");
        let report = blaze_flow::check(&cf.lowered.flow, crate::region_fixture::alloc()).expect("flow check");
        if conflict {
            assert!(!report.borrows.is_empty(), "NEGATIVE CONTROL: shared borrow across `edit(&mut s)` must error");
        } else {
            assert!(report.moves.is_empty() && report.borrows.is_empty(), "clean `edit(&mut s)` must have zero errors: moves={} borrows={}", report.moves.len(), report.borrows.len());
        }
    }
}

/// Registers `Sized`, `FnOnce`, `FnMut`, and `Fn` lang traits (with `rust-call` methods) into `fx`.
fn add_fn_traits(fx: &mut Fx) -> (ItemKey, ItemKey, ItemKey) {
    let sized = fx.tr("Sized", 1, no_clauses(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (p0, p1) = (fx.param(0), fx.param(1));
    let rust_call = FnPtrHeader { abi: blaze_typeck::Abi::RustCall, safety: blaze_typeck::Safety::Safe, c_variadic: false };

    let fn_once = fx.tr("FnOnce", 2, no_clauses(), &["Output"]);
    fx.lang(LangItem::FnOnce, fn_once);
    let out = fx.alias(fn_once, "Output", &[p0, p1]);
    let call_once = fx.trait_fn(fn_once, "call_once", true, &[p0, p1], out);
    fx.set_header(call_once, rust_call);

    let sup = fx.bound(fn_once, &[p0, p1]);
    let fn_mut = fx.tr("FnMut", 2, std::vec![sup], &[]);
    fx.lang(LangItem::FnMut, fn_mut);
    let m_self = fx.r(Mutbl::Mut, p0);
    let call_mut = fx.trait_fn(fn_mut, "call_mut", true, &[m_self, p1], out);
    fx.set_header(call_mut, rust_call);

    let sup = fx.bound(fn_mut, &[p0, p1]);
    let fn_ = fx.tr("Fn", 2, std::vec![sup], &[]);
    fx.lang(LangItem::Fn, fn_);
    let r_self = fx.r(Mutbl::Not, p0);
    let call = fx.trait_fn(fn_, "call", true, &[r_self, p1], out);
    fx.set_header(call, rust_call);
    (fn_, fn_mut, fn_once)
}

/// `dyn Fn(inputs..) -> output` in `fx`.
fn dyn_fn_ty(fx: &mut Fx, fn_tr: ItemKey, fn_once: ItemKey, inputs: &[PTy], output: PTy) -> PTy {
    let tup = fx.tuple(inputs);
    let args_l = fx.list(&[tup]);
    let ex_fn = fx.ty(PNode::ExTrait(fn_tr, args_l));
    let ex_out = fx.ty(PNode::ExProj { trait_: fn_once, name: Name::of("Output"), args: args_l, term: output });
    let preds = fx.list(&[ex_fn, ex_out]);
    fx.ty(PNode::Dynamic(preds))
}

/// `base.name` built in `b` using a local slot for `p`.
fn field_access(b: &mut Bb, p: PatId, base: ExprId, name: &str) -> ExprId {
    let at = b.local(p);
    b.exprs[at.ix()] = blaze_typeck::Expr::Field { base, name: Name::of(name) };
    at
}

/// `lhs = rhs` built in `b` using a local slot for `p`.
fn assign_expr(b: &mut Bb, p: PatId, lhs: ExprId, rhs: ExprId) -> ExprId {
    let at = b.local(p);
    b.exprs[at.ix()] = blaze_typeck::Expr::Assign { lhs, rhs };
    at
}

/// Extended fixture facts with custom ADT metadata (lifetimes, variances, field names, field
/// counts, and `FieldRegion` rows) for testing struct-field and container `dyn Fn*` calls.
struct DynFacts {
    lane: Lane,
    adts: std::vec::Vec<(ItemKey, AdtFacts, &'static [crate::Variance])>,
    fields: std::vec::Vec<(ItemKey, u32, &'static str, u32, &'static [crate::FieldRegion])>,
    field_counts: std::vec::Vec<(ItemKey, u32, u32)>,
}

impl DynFacts {
    fn of(w: &World, fn_traits: (ItemKey, ItemKey, ItemKey)) -> DynFacts {
        let mut lane = Lane::of(w);
        lane.fn_traits = [Some(fn_traits.0), Some(fn_traits.1), Some(fn_traits.2)];
        lane.wide.sigs.decls.trait_(fn_traits.0, 0);
        lane.wide.sigs.decls.trait_(fn_traits.1, 0);
        lane.wide.sigs.decls.trait_(fn_traits.2, 0);
        lane.wide.sigs.decls.alias(fn_traits.2, "Output", 0);
        DynFacts { lane, adts: std::vec::Vec::new(), fields: std::vec::Vec::new(), field_counts: std::vec::Vec::new() }
    }
}

impl Declared for DynFacts {
    fn decls(&self) -> &Decls {
        self.lane.decls()
    }
}

impl Facts for DynFacts {
    fn regions(&self) -> &dyn crate::RegionFacts {
        &crate::NoRegions
    }
    fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> {
        None
    }
    fn adt_variances(&self, k: ItemKey) -> Option<&[crate::Variance]> {
        if let Some(&(_, _, v)) = self.adts.iter().find(|&&(a, ..)| a == k) {
            return Some(v);
        }
        self.lane.adt_variances(k)
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
    fn async_bodies_checked(&self) -> bool {
        false
    }
    fn is_future(&self, _: PTy) -> Option<bool> {
        None
    }
    fn shape(&self, _: ItemKey) -> Option<crate::Shape<'_>> {
        None
    }
    fn object_trait_lifetimes(&self, _: ItemKey) -> Option<bool> {
        None
    }
    fn call_bounds(&self, _: ItemKey) -> Option<crate::CallBounds<'_>> {
        None
    }
    fn trait_lifetimes(&self, _: ItemKey) -> Option<u32> {
        None
    }
    fn alias_lifetimes(&self, _: ItemKey, _: Name) -> Option<u32> {
        None
    }
    fn is_fn_trait(&self, k: ItemKey) -> bool {
        self.lane.is_fn_trait(k)
    }
    fn ties(&self, _: ItemKey) -> Option<crate::Ties<'_>> {
        None
    }
    fn field_param(&self, _: ItemKey, _: u32, _: u32) -> Option<u32> {
        None
    }
    fn field_lead(&self, _: ItemKey, _: u32, _: u32) -> Option<crate::FieldLead> {
        None
    }
    fn field_regions(&self, adt: ItemKey, variant: u32, field: u32) -> Option<&[crate::FieldRegion]> {
        self.fields.iter().find(|&&(a, v, _, f, _)| a == adt && v == variant && f == field).map(|&(_, _, _, _, r)| r)
    }
    fn copy_args(&self, _: ItemKey) -> Option<&[bool]> {
        None
    }
    fn adt(&self, k: ItemKey) -> Option<AdtFacts> {
        if let Some(&(_, f, _)) = self.adts.iter().find(|&&(a, ..)| a == k) {
            return Some(f);
        }
        self.lane.adt(k)
    }
    fn callee(&self, k: ItemKey) -> Option<CalleeFacts> {
        self.lane.callee(k)
    }
    fn closure_bodies_checked(&self) -> bool {
        true
    }
    fn closure_requirements(&self, closure: ExprId) -> &[Requirement] {
        self.lane.closure_requirements(closure)
    }
    fn field(&self, adt: ItemKey, variant: u32, name: Name) -> Option<u32> {
        self.fields.iter().find(|&&(a, v, n, _, _)| a == adt && v == variant && Name::of(n) == name).map(|&(_, _, _, f, _)| f)
    }
    fn field_count(&self, adt: ItemKey, variant: u32) -> Option<u32> {
        self.field_counts.iter().find(|&&(a, v, _)| a == adt && v == variant).map(|&(_, _, c)| c)
    }
    fn param_copy(&self, index: u32) -> bool {
        self.lane.param_copy(index)
    }
}

/// Struct field `&'a dyn Fn*` on a lifetime-bearing struct (`Mapped<'s>::map`,
/// `Standing<'r>::sdk_resolver`, `CommandFront<'h, 'c>::names`, `Declared<'f, 't>::make` shapes):
/// - Positive 1 (object lifetime `'a` in `slots[0]`, HRTB input `for<'x> Fn(&'x S)`):
///   `fn use_hrtb<'a>(h: Holder<'a>, s: S) { (h.hrtb)(&s); m(&mut s); }` -> clean.
/// - Positive 2 (free lifetime `'a` in `slots[1..]` output `&'a dyn Fn() -> &'a S`):
///   `fn use_make<'a>(h: Holder<'a>) -> &'a S { (h.make)() }` -> clean.
/// - Negative 1 (soundness: seeded input lifetime `'a` rejects local borrow):
///   `fn bad_sink<'a>(h: Holder<'a>, s: S) { (h.sink)(&s); }` -> borrow error.
/// - Negative 2 (soundness: seeded output lifetime `'a` cannot be returned as unrelated `'b`):
///   `fn bad_make<'a, 'b>(h: Holder<'a>) -> &'b S { (h.make)() }` -> `UnprovenOutlives`.
#[test]
fn dyn_fn_field_with_struct_lifetime_and_seeded_param() {
    use crate::region_fixture::P1;
    use crate::{FieldRegion, Variance};

    let mut decls_out = None;
    let w = world_with(|fx, st, rs, unit| {
        let fns = add_fn_traits(fx);
        let d_in = dyn_fn_ty(fx, fns.0, fns.2, &[rs], unit);
        let r_in = fx.r(Mutbl::Not, d_in);
        let d_out = dyn_fn_ty(fx, fns.0, fns.2, &[], rs);
        let r_out = fx.r(Mutbl::Not, d_out);
        let holder = fx.strukt("Holder", 0, &[("hrtb", r_in), ("sink", r_in), ("make", r_out)]);
        let ht = fx.adt(holder, &[]);
        let use_hrtb = fx.func("use_hrtb", 0, &[ht, st], unit, no_clauses());
        let use_make = fx.func("use_make", 0, &[ht], rs, no_clauses());
        let bad_sink = fx.func("bad_sink", 0, &[ht, st], unit, no_clauses());
        let bad_make = fx.func("bad_make", 0, &[ht], rs, no_clauses());
        decls_out = Some((fns, holder, ht, r_in, r_out, use_hrtb, use_make, bad_sink, bad_make));
    });
    let (fns, holder, ht, r_in, r_out, use_hrtb, use_make, bad_sink, bad_make) = decls_out.expect("declared");

    const HRTB_FR: [FieldRegion; 4] = [FieldRegion::Lifetime(0), FieldRegion::Lifetime(0), FieldRegion::Bound(0), FieldRegion::Bound(0)];
    const SINK_FR: [FieldRegion; 4] = [FieldRegion::Lifetime(0), FieldRegion::Lifetime(0), FieldRegion::Lifetime(0), FieldRegion::Lifetime(0)];
    const MAKE_FR: [FieldRegion; 3] = [FieldRegion::Lifetime(0), FieldRegion::Lifetime(0), FieldRegion::Lifetime(0)];
    const INV: [Variance; 1] = [Variance::In];

    let mut facts = DynFacts::of(&w, fns);
    facts.adts.push((
        holder,
        AdtFacts { copy: false, drop_impl: false, needs_drop: false, lifetimes: 1, union: false, is_enum: false, is_box: false },
        &INV,
    ));
    facts.fields.push((holder, 0, "hrtb", 0, &HRTB_FR));
    facts.fields.push((holder, 0, "sink", 1, &SINK_FR));
    facts.fields.push((holder, 0, "make", 2, &MAKE_FR));
    facts.lane.wide.sigs.decls.adt(holder, 1);
    facts.lane.wide.sigs.decls.field(holder, 0, 0, r_in, &[P0, P0, BOUND1, BOUND1]);
    facts.lane.wide.sigs.decls.field(holder, 0, 1, r_in, &[P0, P0, P0, P0]);
    facts.lane.wide.sigs.decls.field(holder, 0, 2, r_out, &[P0, P0, P0]);

    facts.lane.wide.add(use_hrtb, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[ht, w.st], output: w.unit, slots: &[P0] });
    facts.lane.wide.add(use_make, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[ht], output: w.rs, slots: &[P0, P0] });
    facts.lane.wide.add(bad_sink, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[ht, w.st], output: w.unit, slots: &[P0] });
    facts.lane.wide.add(bad_make, FnHead { lifetimes: 2, late: &[0, 1], outlives: &[], inputs: &[ht], output: w.rs, slots: &[P0, P1] });

    // Positive 1: `(h.hrtb)(&s); m(&mut s);`
    {
        let mut b = Bb::new();
        let (ph, ps) = (b.param("h"), b.param("s"));
        let lh = b.local(ph);
        let fh = field_access(&mut b, ph, lh, "hrtb");
        let ls = b.local(ps);
        let rs_e = b.borrow(Mutbl::Not, ls);
        let call = b.call(fh, &[rs_e]);
        let st1 = b.semi(call);
        let mut_s = call_ref(&mut b, "m", Mutbl::Mut, ps);
        let st2 = b.semi(mut_s);
        let root = b.block(&[st1, st2], None);
        let got = findings_laid(&w.fx, &b, use_hrtb, root, &facts, Plant::None);
        assert_eq!(got, Ok(std::vec::Vec::new()), "HRTB dyn Fn field on lifetime struct must pass: {got:?}");
    }

    // Positive 2: `(h.make)()` returned as `&'a S`
    {
        let mut b = Bb::new();
        let ph = b.param("h");
        let lh = b.local(ph);
        let fm = field_access(&mut b, ph, lh, "make");
        let call = b.call(fm, &[]);
        let root = b.block(&[], Some(call));
        let got = findings_laid(&w.fx, &b, use_make, root, &facts, Plant::None);
        assert_eq!(got, Ok(std::vec::Vec::new()), "seeded output lifetime on dyn Fn field must pass: {got:?}");
    }

    // Negative 1: `(h.sink)(&s)` passes local `&s` into `&'a S`
    {
        let mut b = Bb::new();
        let (ph, ps) = (b.param("h"), b.param("s"));
        let lh = b.local(ph);
        let fs = field_access(&mut b, ph, lh, "sink");
        let ls = b.local(ps);
        let rs_e = b.borrow(Mutbl::Not, ls);
        let call = b.call(fs, &[rs_e]);
        let st1 = b.semi(call);
        let root = b.block(&[st1], None);
        let got = findings_laid(&w.fx, &b, bad_sink, root, &facts, Plant::None).expect("lowered");
        assert!(got.iter().any(|l| l.starts_with("borrow ")), "NEGATIVE: local borrow into &'a S field sink must error: {got:?}");
    }

    // Negative 2: `(h.make)()` returns `&'a S` where `&'b S` is expected
    {
        let mut b = Bb::new();
        let ph = b.param("h");
        let lh = b.local(ph);
        let fm = field_access(&mut b, ph, lh, "make");
        let call = b.call(fm, &[]);
        let root = b.block(&[], Some(call));
        let got = findings_laid(&w.fx, &b, bad_make, root, &facts, Plant::None).expect("lowered");
        assert!(got.iter().any(|l| l.contains("UnprovenOutlives")), "NEGATIVE: &'a S returned as &'b S must fail outlives: {got:?}");
    }
}

/// Container lookup bound via `let Some(program) = self.programs.take() else { return; }; program(..)`
/// (`crates/blaze-core/src/sim.rs::run` shape):
/// - Positive: HRTB `dyn Fn(&mut S)` extracted via `let ... else` checks clean (`program(&mut s); m(&mut s);`).
/// - Negative 1: conflicting shared borrow `let r = &s; program(&mut s); g(r);` catches `borrow Invalidated`.
/// - Negative 2: container holding `dyn Fn(&'static S)` rejects local `&s` (`borrow ` error).
/// - Negative 3: reassigned local `program = other; program(&mut s);` is refused (`CalleeValueUnknown`),
///   never unsoundly inheriting the initializer's signature.
#[test]
fn container_let_else_dyn_fn_call_checks_borrows() {
    use blaze_typeck::regions::Region;
    use crate::{FieldRegion, Variance};

    let mut decls_out = None;
    let w = world_with(|fx, st, rs, unit| {
        let fns = add_fn_traits(fx);
        let rms = fx.r(Mutbl::Mut, st);
        let p0 = fx.param(0);
        let (option, _) = fx.enum_("Option", 1, &[("None", &[]), ("Some", &[p0])]);
        let map = fx.strukt("Map", 1, &[]);
        let map_v = fx.adt(map, &[p0]);
        let rm_map_v = fx.r(Mutbl::Mut, map_v);
        let opt_v = fx.adt(option, &[p0]);
        let imp_map = fx.imp("impl<V> Map<V> {}", 1, map_v, None, &[], no_clauses());
        let take = fx.impl_fn(imp_map, "take", true, &[rm_map_v], opt_v);

        let d_hrtb = dyn_fn_ty(fx, fns.0, fns.2, &[rms], unit);
        let r_hrtb = fx.r(Mutbl::Not, d_hrtb);
        let map_hrtb = fx.adt(map, &[r_hrtb]);

        let d_stat = dyn_fn_ty(fx, fns.0, fns.2, &[rs], unit);
        let r_stat = fx.r(Mutbl::Not, d_stat);
        let map_stat = fx.adt(map, &[r_stat]);

        let sim = fx.strukt("Sim", 0, &[("programs", map_hrtb), ("static_programs", map_stat)]);
        let sim_t = fx.adt(sim, &[]);
        let rm_sim = fx.r(Mutbl::Mut, sim_t);

        let run_hrtb = fx.func("run_hrtb", 0, &[rm_sim, st], unit, no_clauses());
        let run_stat = fx.func("run_stat", 0, &[rm_sim, st], unit, no_clauses());
        let run_reassigned = fx.func("run_reassigned", 0, &[rm_sim, r_hrtb, st], unit, no_clauses());
        decls_out = Some((fns, option, map, sim, rm_map_v, opt_v, take, map_hrtb, map_stat, rm_sim, r_hrtb, run_hrtb, run_stat, run_reassigned));
    });
    let (fns, option, map, sim, rm_map_v, opt_v, take, map_hrtb, map_stat, rm_sim, r_hrtb, run_hrtb, run_stat, run_reassigned) =
        decls_out.expect("declared");

    const HRTB_FR: [FieldRegion; 4] = [FieldRegion::Static, FieldRegion::Static, FieldRegion::Bound(0), FieldRegion::Bound(0)];
    const STAT_FR: [FieldRegion; 4] = [FieldRegion::Static, FieldRegion::Static, FieldRegion::Static, FieldRegion::Static];
    const ARG0_FR: [FieldRegion; 1] = [FieldRegion::Arg(0)];
    const CO1: [Variance; 1] = [Variance::Co];

    let mut facts = DynFacts::of(&w, fns);
    facts.adts.push((
        option,
        AdtFacts { copy: false, drop_impl: false, needs_drop: false, lifetimes: 0, union: false, is_enum: true, is_box: false },
        &CO1,
    ));
    facts.adts.push((
        map,
        AdtFacts { copy: false, drop_impl: false, needs_drop: false, lifetimes: 0, union: false, is_enum: false, is_box: false },
        &CO1,
    ));
    facts.adts.push((
        sim,
        AdtFacts { copy: false, drop_impl: false, needs_drop: false, lifetimes: 0, union: false, is_enum: false, is_box: false },
        &[],
    ));
    facts.field_counts.push((option, 0, 0));
    facts.field_counts.push((option, 1, 1));
    facts.fields.push((option, 1, "0", 0, &ARG0_FR));
    facts.fields.push((sim, 0, "programs", 0, &HRTB_FR));
    facts.fields.push((sim, 0, "static_programs", 1, &STAT_FR));
    facts.lane.wide.sigs.decls.field(sim, 0, 0, map_hrtb, &[Region::Static, Region::Static, BOUND1, BOUND1]);
    facts.lane.wide.sigs.decls.field(sim, 0, 1, map_stat, &[Region::Static, Region::Static, Region::Static, Region::Static]);
    facts.lane.wide.add(take, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[rm_map_v], output: opt_v, slots: &[P0] });
    facts.lane.wide.add(run_hrtb, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[rm_sim, w.st], output: w.unit, slots: &[P0] });
    facts.lane.wide.add(run_stat, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[rm_sim, w.st], output: w.unit, slots: &[P0] });
    facts.lane.wide.add(
        run_reassigned,
        FnHead {
            lifetimes: 1,
            late: &[0],
            outlives: &[],
            inputs: &[rm_sim, r_hrtb, w.st],
            output: w.unit,
            slots: &[P0, Region::Static, Region::Static, BOUND1, BOUND1],
        },
    );

    // Positive + Negative 1 (conflicting borrow across `program(&mut s)`) via `let Some(program) = sim.programs.take() else { return; };`
    for conflict in [false, true] {
        let mut b = Bb::new();
        let (psim, ps) = (b.param("sim"), b.param("s"));
        let lsim = b.local(psim);
        let fprogs = field_access(&mut b, psim, lsim, "programs");
        let call_take = b.method(fprogs, "take", &[]);
        let pprog = b.bind("program");
        let some_prog = b.variant_pat(&["Option", "Some"], &[pprog]);
        let ret_e = b.ret(None);
        let st_let = b.let_(some_prog, None, call_take);
        let lprog = b.local(pprog);
        let ls = b.local(ps);
        let mut_s = b.borrow(Mutbl::Mut, ls);
        let call_prog = b.call(lprog, &[mut_s]);
        let st_call = b.semi(call_prog);
        let root = if conflict {
            let ls2 = b.local(ps);
            let r_s = b.borrow(Mutbl::Not, ls2);
            let pr = b.bind("r");
            let st_r = b.let_(pr, None, r_s);
            let use_r = call1(&mut b, "g", pr);
            let st_use = b.semi(use_r);
            b.block(&[st_let, st_r, st_call, st_use], None)
        } else {
            let mut_after = call_ref(&mut b, "m", Mutbl::Mut, ps);
            let st_after = b.semi(mut_after);
            b.block(&[st_let, st_call, st_after], None)
        };
        let mut view = b.view(run_hrtb, root);
        let mut stmts_owned = view.stmts.to_vec();
        if let blaze_typeck::Stmt::Let { else_, .. } = &mut stmts_owned[st_let.ix()] {
            *else_ = Some(ret_e);
        }
        view.stmts = &stmts_owned;
        let got = crate::region_fixture::run(&w.fx, &view, &facts, facts.decls(), Plant::None).map(|r| r.lines);
        if conflict {
            let lines = got.expect("lowered");
            assert!(lines.iter().any(|l| l.starts_with("borrow Invalidated")), "NEGATIVE: shared borrow across `program(&mut s)` must error: {lines:?}");
        } else {
            assert_eq!(got, Ok(std::vec::Vec::new()), "POSITIVE: `let Some(program) = sim.programs.take() else {{ return; }}; program(&mut s);` must pass: {got:?}");
        }
    }

    // Negative 2: container holds `&'static dyn Fn(&'static S)` -> passing `&s` must report a borrow error.
    {
        let mut b = Bb::new();
        let (psim, ps) = (b.param("sim"), b.param("s"));
        let lsim = b.local(psim);
        let fprogs = field_access(&mut b, psim, lsim, "static_programs");
        let call_take = b.method(fprogs, "take", &[]);
        let pprog = b.bind("program");
        let some_prog = b.variant_pat(&["Option", "Some"], &[pprog]);
        let ret_e = b.ret(None);
        let st_let = b.let_(some_prog, None, call_take);
        let lprog = b.local(pprog);
        let ls = b.local(ps);
        let ref_s = b.borrow(Mutbl::Not, ls);
        let call_prog = b.call(lprog, &[ref_s]);
        let st_call = b.semi(call_prog);
        let root = b.block(&[st_let, st_call], None);
        let mut view = b.view(run_stat, root);
        let mut stmts_owned = view.stmts.to_vec();
        if let blaze_typeck::Stmt::Let { else_, .. } = &mut stmts_owned[st_let.ix()] {
            *else_ = Some(ret_e);
        }
        view.stmts = &stmts_owned;
        let got = crate::region_fixture::run(&w.fx, &view, &facts, facts.decls(), Plant::None).map(|r| r.lines).expect("lowered");
        assert!(got.iter().any(|l| l.starts_with("borrow ")), "NEGATIVE: local `&s` into `&'static S` container dyn Fn must error: {got:?}");
    }

    // Negative 3: reassigned local is refused (`CalleeValueUnknown`), never unsoundly assuming initializer's signature.
    {
        let mut b = Bb::new();
        let (psim, pother, ps) = (b.param("sim"), b.param("other"), b.param("s"));
        let lsim = b.local(psim);
        let fprogs = field_access(&mut b, psim, lsim, "programs");
        let call_take = b.method(fprogs, "take", &[]);
        let pprog = b.bind_mut("program");
        let some_prog = b.variant_pat(&["Option", "Some"], &[pprog]);
        let ret_e = b.ret(None);
        let st_let = b.let_(some_prog, None, call_take);
        let lhs = b.local(pprog);
        let rhs = b.local(pother);
        let asgn = assign_expr(&mut b, pprog, lhs, rhs);
        let st_asgn = b.semi(asgn);
        let lprog = b.local(pprog);
        let ls = b.local(ps);
        let mut_s = b.borrow(Mutbl::Mut, ls);
        let call_prog = b.call(lprog, &[mut_s]);
        let st_call = b.semi(call_prog);
        let root = b.block(&[st_let, st_asgn, st_call], None);
        let mut view = b.view(run_reassigned, root);
        let mut stmts_owned = view.stmts.to_vec();
        if let blaze_typeck::Stmt::Let { else_, .. } = &mut stmts_owned[st_let.ix()] {
            *else_ = Some(ret_e);
        }
        view.stmts = &stmts_owned;
        let got = crate::region_fixture::run(&w.fx, &view, &facts, facts.decls(), Plant::None);
        assert!(
            matches!(got, Err(Refusal { form: Form::CalleeValueUnknown, .. })),
            "NEGATIVE: reassigned local callable must refuse rather than trusting initializer: {got:?}"
        );
    }
}

/// Generic `F: Fn*(..)` referencing an enclosing fn's lifetime parameter `'a`, called inside a
/// nested closure (`MadeOnce<'n, F>::procs` in `job_texts.rs:290` shape):
/// - Positive: `fn ok<'a, F: Fn() -> &'a S>(f: F) { let _c = || { let r = f(); g(r); }; }` lowers
///   cleanly (previously refused with `CalleeValueUnknown` because `self.in_nested` rejected
///   `Region::Param(0)`) with zero findings.
/// - Negative (soundness): `fn bad<'a, F: Fn(&'a S)>(f: F) { let _c = || { let s = mk(); f(&s); }; }`
///   lowers cleanly AND catches the local borrow `&s` escaping into `'a` (`!report.borrows.is_empty()`).
#[test]
fn generic_fn_param_called_inside_closure_checks_owner_lifetime() {
    let mut items = None;
    let w = world_with(|fx, st, rs, unit| {
        let fns = add_fn_traits(fx);
        let p0 = fx.param(0);
        let unit_tup = fx.tuple(&[]);
        let rs_tup = fx.tuple(&[rs]);
        let c_ok_fn = fx.bound(fns.0, &[p0, unit_tup]);
        let c_ok_out = fx.proj(fns.2, &[p0, unit_tup], "Output", rs);
        let f_ok = fx.func("ok", 1, &[p0], unit, std::vec![c_ok_fn, c_ok_out]);
        let c_bad_fn = fx.bound(fns.0, &[p0, rs_tup]);
        let c_bad_out = fx.proj(fns.2, &[p0, rs_tup], "Output", unit);
        let f_bad = fx.func("bad", 1, &[p0], unit, std::vec![c_bad_fn, c_bad_out]);
        let mk = fx.func("mk", 0, &[], st, no_clauses());

        let ok_args = fx.list(&[p0, unit_tup]);
        let ex_ok_fn = fx.ty(PNode::ExTrait(fns.0, ok_args));
        let ex_ok_out = fx.ty(PNode::ExProj { trait_: fns.2, name: Name::of("Output"), args: ok_args, term: rs });
        let bad_args = fx.list(&[p0, rs_tup]);
        let ex_bad_fn = fx.ty(PNode::ExTrait(fns.0, bad_args));
        let ex_bad_out = fx.ty(PNode::ExProj { trait_: fns.2, name: Name::of("Output"), args: bad_args, term: unit });
        items = Some((fns, p0, mk, f_ok, f_bad, ex_ok_fn, ex_ok_out, ex_bad_fn, ex_bad_out));
    });
    let (fns, p0, mk, f_ok, f_bad, ex_ok_fn, ex_ok_out, ex_bad_fn, ex_bad_out) = items.expect("declared");

    let mut facts = DynFacts::of(&w, fns);
    facts.lane.wide.add(mk, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[], output: w.st, slots: &[] });
    facts.lane.wide.add(f_ok, FnHead { lifetimes: 1, late: &[], outlives: &[], inputs: &[p0], output: w.unit, slots: &[] });
    facts.lane.wide.sigs.decls.clause(f_ok, 0, 0, ex_ok_fn, &[]);
    facts.lane.wide.sigs.decls.clause(f_ok, 1, 0, ex_ok_out, &[P0]);
    facts.lane.wide.add(f_bad, FnHead { lifetimes: 1, late: &[], outlives: &[], inputs: &[p0], output: w.unit, slots: &[] });
    facts.lane.wide.sigs.decls.clause(f_bad, 0, 0, ex_bad_fn, &[P0]);
    facts.lane.wide.sigs.decls.clause(f_bad, 1, 0, ex_bad_out, &[P0]);

    // Positive: `|| { let r = f(); g(r); }`
    {
        let mut b = Bb::new();
        let pf = b.param("f");
        let lf = b.local(pf);
        let call_f = b.call(lf, &[]);
        let pr = b.bind("r");
        let st1 = b.let_(pr, None, call_f);
        let use_r = call1(&mut b, "g", pr);
        let st2 = b.semi(use_r);
        let inner = b.block(&[st1, st2], None);
        let cl = b.closure(&[], inner);
        let pc = b.bind("c");
        let st_let = b.let_(pc, None, cl);
        let root = b.block(&[st_let], None);
        let body = b.view(f_ok, root);
        let mut ck = crate::region_fixture::checked(&w.fx, &body);
        let regions = facts.decls().lay(w.fx.t.view(), &mut ck.types);
        let with = crate::region_fixture::WithRegions { base: &facts, regions: &regions };
        let cf = crate::lower_closure_external(&body, &ck, cl, &with, crate::region_fixture::alloc()).expect("lower closure");
        let report = blaze_flow::check(&cf.lowered.flow, crate::region_fixture::alloc()).expect("flow check");
        assert!(
            report.moves.is_empty() && report.borrows.is_empty(),
            "POSITIVE: `|| {{ let r = f(); g(r); }}` must check clean: moves={} borrows={}",
            report.moves.len(),
            report.borrows.len()
        );
    }

    // Negative: `|| { let s = mk(); f(&s); }` passes local `&s` into owner lifetime `'a`
    {
        let mut b = Bb::new();
        let pf = b.param("f");
        let pmk = b.path(&["mk"]);
        let call_mk = b.call(pmk, &[]);
        let ps = b.bind("s");
        let st1 = b.let_(ps, None, call_mk);
        let lf = b.local(pf);
        let ls = b.local(ps);
        let rs_e = b.borrow(Mutbl::Not, ls);
        let call_f = b.call(lf, &[rs_e]);
        let st2 = b.semi(call_f);
        let inner = b.block(&[st1, st2], None);
        let cl = b.closure(&[], inner);
        let pc = b.bind("c");
        let st_let = b.let_(pc, None, cl);
        let root = b.block(&[st_let], None);
        let body = b.view(f_bad, root);
        let mut ck = crate::region_fixture::checked(&w.fx, &body);
        let regions = facts.decls().lay(w.fx.t.view(), &mut ck.types);
        let with = crate::region_fixture::WithRegions { base: &facts, regions: &regions };
        let cf = crate::lower_closure_external(&body, &ck, cl, &with, crate::region_fixture::alloc()).expect("lower closure");
        let report = blaze_flow::check(&cf.lowered.flow, crate::region_fixture::alloc()).expect("flow check");
        assert!(!report.borrows.is_empty(), "NEGATIVE: local `&s` passed to `F: Fn(&'a S)` inside closure must fail borrow check");
    }
}

/// An enum variant field holding a `dyn Fn*` destructured in an outer `match` and captured into
/// a nested closure (`match sorter { Sorter::ByName(cmp) => { let _c = move |x: &S| cmp(x); } }`,
/// the `ignore::walk::WalkBuilder::build` shape):
/// - Positive: higher-ranked `dyn for<'x> Fn(&'x S) + 'static` captured into nested closure
///   lowers cleanly with zero findings (previously `CalleeValueUnknown` because nested closures
///   did not inherit `local_field_source`).
/// - Negative (soundness): `dyn Fn(&'static S) + 'static` captured into nested closure rejects
///   passing a local borrow `&local` (`!report.borrows.is_empty()`).
#[test]
fn enum_variant_dyn_fn_captured_in_nested_closure() {
    use crate::FieldRegion;

    let mut items = None;
    let w = world_with(|fx, st, rs, unit| {
        let fns = add_fn_traits(fx);
        let d_in = dyn_fn_ty(fx, fns.0, fns.2, &[rs], unit);
        let r_in = fx.r(Mutbl::Not, d_in);
        let (sorter, _) = fx.enum_("Sorter", 0, &[("ByName", &[r_in]), ("Sink", &[r_in])]);
        let sorter_ty = fx.adt(sorter, &[]);
        let mk = fx.func("mk", 0, &[], st, no_clauses());
        let f_ok = fx.func("ok", 0, &[sorter_ty], unit, no_clauses());
        let f_bad = fx.func("bad", 0, &[sorter_ty], unit, no_clauses());
        items = Some((fns, sorter, sorter_ty, r_in, mk, f_ok, f_bad));
    });
    let (fns, sorter, sorter_ty, r_in, mk, f_ok, f_bad) = items.expect("declared");

    const HRTB_FR: [FieldRegion; 4] = [FieldRegion::Static, FieldRegion::Static, FieldRegion::Bound(0), FieldRegion::Bound(0)];
    const SINK_FR: [FieldRegion; 4] = [FieldRegion::Static, FieldRegion::Static, FieldRegion::Static, FieldRegion::Static];

    let mut facts = DynFacts::of(&w, fns);
    facts.adts.push((
        sorter,
        AdtFacts { copy: false, drop_impl: false, needs_drop: false, lifetimes: 0, union: false, is_enum: true, is_box: false },
        &[],
    ));
    facts.field_counts.push((sorter, 0, 1));
    facts.field_counts.push((sorter, 1, 1));
    facts.fields.push((sorter, 0, "0", 0, &HRTB_FR));
    facts.fields.push((sorter, 1, "0", 0, &SINK_FR));
    const ST: crate::Region = crate::Region::Static;
    facts.lane.wide.sigs.decls.adt(sorter, 0);
    facts.lane.wide.sigs.decls.field(sorter, 0, 0, r_in, &[ST, ST, BOUND1, BOUND1]);
    facts.lane.wide.sigs.decls.field(sorter, 1, 0, r_in, &[ST, ST, ST, ST]);
    facts.lane.wide.add(mk, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[], output: w.st, slots: &[] });
    facts.lane.wide.add(f_ok, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[sorter_ty], output: w.unit, slots: &[] });
    facts.lane.wide.add(f_bad, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[sorter_ty], output: w.unit, slots: &[] });

    // Positive: `match s { Sorter::ByName(cmp) => { let _c = || { let x = mk(); cmp(&x); }; }, _ => {} }`
    {
        let mut b = Bb::new();
        let ps = b.param("s");
        let p_cmp = b.bind("cmp");
        let p_by_name = b.variant_pat(&["Sorter", "ByName"], &[p_cmp]);
        let p_wild = b.wild();

        let pmk = b.path(&["mk"]);
        let call_mk = b.call(pmk, &[]);
        let px = b.bind("x");
        let st1 = b.let_(px, None, call_mk);
        let l_cmp = b.local(p_cmp);
        let l_x = b.local(px);
        let ref_x = b.borrow(Mutbl::Not, l_x);
        let call_cmp = b.call(l_cmp, &[ref_x]);
        let st2 = b.semi(call_cmp);
        let inner = b.block(&[st1, st2], None);
        let cl = b.closure(&[], inner);
        let pc = b.bind("c");
        let st_let = b.let_(pc, None, cl);
        let arm_body = b.block(&[st_let], None);
        let wild_body = b.block(&[], None);
        let scrut = b.local(ps);
        let m = b.match_(scrut, &[(p_by_name, arm_body), (p_wild, wild_body)]);
        let root = b.block(&[], Some(m));

        let body = b.view(f_ok, root);
        let mut ck = crate::region_fixture::checked(&w.fx, &body);
        let regions = facts.decls().lay(w.fx.t.view(), &mut ck.types);
        let with = crate::region_fixture::WithRegions { base: &facts, regions: &regions };
        let cf = crate::lower_closure_external(&body, &ck, cl, &with, crate::region_fixture::alloc())
            .expect("nested closure capturing enum-variant dyn Fn must lower");
        let report = blaze_flow::check(&cf.lowered.flow, crate::region_fixture::alloc()).expect("flow check");
        assert!(
            report.moves.is_empty() && report.borrows.is_empty(),
            "POSITIVE: HRTB enum-variant dyn Fn captured into nested closure must check clean: moves={} borrows={}",
            report.moves.len(),
            report.borrows.len()
        );
    }

    // Negative: `match s { Sorter::Sink(cmp) => { let _c = || { let x = mk(); cmp(&x); }; }, _ => {} }`
    // where `Sorter::Sink` expects `&'static S` — local borrow `&x` must fail borrow check!
    {
        let mut b = Bb::new();
        let ps = b.param("s");
        let p_cmp = b.bind("cmp");
        let p_sink = b.variant_pat(&["Sorter", "Sink"], &[p_cmp]);
        let p_wild = b.wild();

        let pmk = b.path(&["mk"]);
        let call_mk = b.call(pmk, &[]);
        let px = b.bind("x");
        let st1 = b.let_(px, None, call_mk);
        let l_cmp = b.local(p_cmp);
        let l_x = b.local(px);
        let ref_x = b.borrow(Mutbl::Not, l_x);
        let call_cmp = b.call(l_cmp, &[ref_x]);
        let st2 = b.semi(call_cmp);
        let inner = b.block(&[st1, st2], None);
        let cl = b.closure(&[], inner);
        let pc = b.bind("c");
        let st_let = b.let_(pc, None, cl);
        let arm_body = b.block(&[st_let], None);
        let wild_body = b.block(&[], None);
        let scrut = b.local(ps);
        let m = b.match_(scrut, &[(p_sink, arm_body), (p_wild, wild_body)]);
        let root = b.block(&[], Some(m));

        let body = b.view(f_bad, root);
        let mut ck = crate::region_fixture::checked(&w.fx, &body);
        let regions = facts.decls().lay(w.fx.t.view(), &mut ck.types);
        let with = crate::region_fixture::WithRegions { base: &facts, regions: &regions };
        let cf = crate::lower_closure_external(&body, &ck, cl, &with, crate::region_fixture::alloc())
            .expect("nested closure capturing enum-variant sink dyn Fn must lower");
        let report = blaze_flow::check(&cf.lowered.flow, crate::region_fixture::alloc()).expect("flow check");
        assert!(!report.borrows.is_empty(), "NEGATIVE: local `&x` passed to `&'static S` enum-variant dyn Fn must fail borrow check");
    }
}

/// Struct-destructured `&'a dyn Fn() -> &'a S` (`Edge { opts, .. }` in `engine_check.rs::serve`)
/// captured into a nested closure:
/// - Positive: `let Edge { opts } = edge; let _c = || { let r = opts(); g(r); };` lowers cleanly
///   inside the nested closure without `CalleeValueUnknown`.
/// - Negative (soundness): `let Edge { opts } = edge; let _c = || { let s = mk(); opts(&s); };`
///   where `opts: &'a dyn Fn(&'a S)` catches local `&s` escaping into `'a`.
#[test]
fn destructured_struct_dyn_fn_field_called_in_nested_closure() {
    use crate::{FieldRegion, Variance};

    let mut items = None;
    let w = world_with(|fx, st, rs, unit| {
        let fns = add_fn_traits(fx);
        let dyn_ret = dyn_fn_ty(fx, fns.0, fns.2, &[], rs);
        let ref_dyn_ret = fx.r(Mutbl::Not, dyn_ret);
        let edge_ret = fx.strukt("EdgeRet", 0, &[("opts", ref_dyn_ret)]);
        let edge_ret_ty = fx.adt(edge_ret, &[]);

        let dyn_take = dyn_fn_ty(fx, fns.0, fns.2, &[rs], unit);
        let ref_dyn_take = fx.r(Mutbl::Not, dyn_take);
        let edge_take = fx.strukt("EdgeTake", 0, &[("opts", ref_dyn_take)]);
        let edge_take_ty = fx.adt(edge_take, &[]);

        let f_ok = fx.func("ok", 0, &[edge_ret_ty], unit, no_clauses());
        let f_bad = fx.func("bad", 0, &[edge_take_ty], unit, no_clauses());
        let mk = fx.func("mk", 0, &[], st, no_clauses());
        items = Some((fns, edge_ret, edge_ret_ty, ref_dyn_ret, edge_take, edge_take_ty, ref_dyn_take, f_ok, f_bad, mk));
    });
    let (fns, edge_ret, edge_ret_ty, ref_dyn_ret, edge_take, edge_take_ty, ref_dyn_take, f_ok, f_bad, mk) = items.expect("declared");

    const INV: [Variance; 1] = [Variance::In];
    const RET_FR: [FieldRegion; 3] = [FieldRegion::Lifetime(0), FieldRegion::Lifetime(0), FieldRegion::Lifetime(0)];
    const TAKE_FR: [FieldRegion; 4] = [FieldRegion::Lifetime(0), FieldRegion::Lifetime(0), FieldRegion::Lifetime(0), FieldRegion::Lifetime(0)];

    let mut facts = DynFacts::of(&w, fns);
    facts.adts.push((edge_ret, AdtFacts { copy: false, drop_impl: false, needs_drop: false, lifetimes: 1, union: false, is_enum: false, is_box: false }, &INV));
    facts.adts.push((edge_take, AdtFacts { copy: false, drop_impl: false, needs_drop: false, lifetimes: 1, union: false, is_enum: false, is_box: false }, &INV));
    facts.fields.push((edge_ret, 0, "opts", 0, &RET_FR));
    facts.fields.push((edge_take, 0, "opts", 0, &TAKE_FR));
    facts.lane.wide.sigs.decls.adt(edge_ret, 1);
    facts.lane.wide.sigs.decls.adt(edge_take, 1);
    facts.lane.wide.sigs.decls.field(edge_ret, 0, 0, ref_dyn_ret, &[P0, P0, P0]);
    facts.lane.wide.sigs.decls.field(edge_take, 0, 0, ref_dyn_take, &[P0, P0, P0, P0]);
    facts.lane.wide.add(mk, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[], output: w.st, slots: &[] });
    facts.lane.wide.add(f_ok, FnHead { lifetimes: 1, late: &[], outlives: &[], inputs: &[edge_ret_ty], output: w.unit, slots: &[P0] });
    facts.lane.wide.add(f_bad, FnHead { lifetimes: 1, late: &[], outlives: &[], inputs: &[edge_take_ty], output: w.unit, slots: &[P0] });

    // Positive: `let EdgeRet { opts } = edge; let _c = || { let r = opts(); g(r); };`
    {
        let mut b = Bb::new();
        let pedge = b.param("edge");
        let popts = b.bind("opts");
        let pat_edge = b.variant_pat(&["EdgeRet"], &[popts]);
        let ledge = b.local(pedge);
        let st_destr = b.let_(pat_edge, None, ledge);
        let lopts = b.local(popts);
        let call_opts = b.call(lopts, &[]);
        let pr = b.bind("r");
        let st1 = b.let_(pr, None, call_opts);
        let use_r = call1(&mut b, "g", pr);
        let st2 = b.semi(use_r);
        let inner = b.block(&[st1, st2], None);
        let cl = b.closure(&[], inner);
        let pc = b.bind("c");
        let st_c = b.let_(pc, None, cl);
        let root = b.block(&[st_destr, st_c], None);
        let mut body = b.view(f_ok, root);
        let mut pats_owned = body.pats.to_vec();
        let path = match pats_owned[pat_edge.ix()] {
            blaze_typeck::Pat::TupleStruct { path, .. } => path,
            _ => unreachable!(),
        };
        let field_pats = [blaze_typeck::FieldPat { name: Name::of("opts"), pat: popts }];
        pats_owned[pat_edge.ix()] = blaze_typeck::Pat::Struct { path, fields: blaze_typeck::List { start: 0, len: 1 }, rest: false };
        body.pats = &pats_owned;
        body.field_pats = &field_pats;
        let mut ck = crate::region_fixture::checked(&w.fx, &body);
        let regions = facts.decls().lay(w.fx.t.view(), &mut ck.types);
        let with = crate::region_fixture::WithRegions { base: &facts, regions: &regions };
        let cf = crate::lower_closure_external(&body, &ck, cl, &with, crate::region_fixture::alloc()).expect("lower nested closure");
        let report = blaze_flow::check(&cf.lowered.flow, crate::region_fixture::alloc()).expect("flow check");
        assert!(
            report.moves.is_empty() && report.borrows.is_empty(),
            "POSITIVE: destructured struct `&'a dyn Fn() -> &'a S` in nested closure must check clean: {:?}",
            report.borrows
        );
    }

    // Negative: `let EdgeTake { opts } = edge; let _c = || { let s = mk(); opts(&s); };`
    {
        let mut b = Bb::new();
        let pedge = b.param("edge");
        let popts = b.bind("opts");
        let pat_edge = b.variant_pat(&["EdgeTake"], &[popts]);
        let ledge = b.local(pedge);
        let st_destr = b.let_(pat_edge, None, ledge);
        let pmk = b.path(&["mk"]);
        let call_mk = b.call(pmk, &[]);
        let ps = b.bind("s");
        let st1 = b.let_(ps, None, call_mk);
        let lopts = b.local(popts);
        let ls = b.local(ps);
        let rs_e = b.borrow(Mutbl::Not, ls);
        let call_opts = b.call(lopts, &[rs_e]);
        let st2 = b.semi(call_opts);
        let inner = b.block(&[st1, st2], None);
        let cl = b.closure(&[], inner);
        let pc = b.bind("c");
        let st_c = b.let_(pc, None, cl);
        let root = b.block(&[st_destr, st_c], None);
        let mut body = b.view(f_bad, root);
        let mut pats_owned = body.pats.to_vec();
        let path = match pats_owned[pat_edge.ix()] {
            blaze_typeck::Pat::TupleStruct { path, .. } => path,
            _ => unreachable!(),
        };
        let field_pats = [blaze_typeck::FieldPat { name: Name::of("opts"), pat: popts }];
        pats_owned[pat_edge.ix()] = blaze_typeck::Pat::Struct { path, fields: blaze_typeck::List { start: 0, len: 1 }, rest: false };
        body.pats = &pats_owned;
        body.field_pats = &field_pats;
        let mut ck = crate::region_fixture::checked(&w.fx, &body);
        let regions = facts.decls().lay(w.fx.t.view(), &mut ck.types);
        let with = crate::region_fixture::WithRegions { base: &facts, regions: &regions };
        let cf = crate::lower_closure_external(&body, &ck, cl, &with, crate::region_fixture::alloc()).expect("lower nested closure");
        let report = blaze_flow::check(&cf.lowered.flow, crate::region_fixture::alloc()).expect("flow check");
        assert!(!report.borrows.is_empty(), "NEGATIVE: local `&s` passed to destructured `&'a dyn Fn(&'a S)` in nested closure must fail borrowck");
    }
}

/// Immutable local fn pointer initialized by a `match` of `FnDef` arms
/// (`TopEntryPoint::parse_with`, `PrefixEntryPoint::parse`, `cmp_dotted`):
/// - Positive: `let f: fn(&S) = match flag { true => take_any, false => take_any2 }; f(&local);` passes.
/// - Negative: `let f: fn(&S) = match flag { true => take_any, false => take_static }; f(&local);`
///   enforces EVERY leaf arm's contract and rejects `&local` escaping into `'static`.
#[test]
fn match_initialized_fndef_local_enforces_every_arm() {
    let mut items = None;
    let w = world_with(|fx, st, rs, unit| {
        let bool_ty = fx.ty(PNode::Bool);
        let fn_list = fx.list(&[rs, unit]);
        let fn_ty = fx.ty(PNode::FnPtr(fn_list, blaze_typeck::FnPtrHeader::RUST));
        let take_any = fx.func("take_any", 0, &[rs], unit, no_clauses());
        let take_any2 = fx.func("take_any2", 0, &[rs], unit, no_clauses());
        let take_static = fx.func("take_static", 0, &[rs], unit, no_clauses());
        let mk = fx.func("mk", 0, &[], st, no_clauses());
        let f_ok = fx.func("ok", 0, &[bool_ty], unit, no_clauses());
        let f_bad = fx.func("bad", 0, &[bool_ty], unit, no_clauses());
        items = Some((bool_ty, fn_ty, take_any, take_any2, take_static, mk, f_ok, f_bad));
    });
    let (bool_ty, fn_ty, take_any, take_any2, take_static, mk, f_ok, f_bad) = items.expect("declared");
    let mut facts = Lane::of(&w);
    facts.wide.add(take_any, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[w.rs], output: w.unit, slots: &[P0] });
    facts.wide.add(take_any2, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[w.rs], output: w.unit, slots: &[P0] });
    facts.wide.add(take_static, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[w.rs], output: w.unit, slots: &[crate::Region::Static] });
    facts.wide.add(mk, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[], output: w.st, slots: &[] });
    facts.wide.add(f_ok, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[bool_ty], output: w.unit, slots: &[] });
    facts.wide.add(f_bad, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[bool_ty], output: w.unit, slots: &[] });

    let run = |owner: ItemKey, second_arm: &str| {
        let mut b = Bb::new();
        let pflag = b.param("flag");
        let lflag = b.local(pflag);
        let pat_t = b.wild();
        let pat_f = b.wild();
        let e1 = b.path(&["take_any"]);
        let e2 = b.path(&[second_arm]);
        let m = b.match_(lflag, &[(pat_t, e1), (pat_f, e2)]);
        let pf = b.bind("f");
        let st_f = b.let_(pf, Some(fn_ty), m);
        let pmk = b.path(&["mk"]);
        let call_mk = b.call(pmk, &[]);
        let ps = b.bind("s");
        let st_s = b.let_(ps, None, call_mk);
        let lf = b.local(pf);
        let ls = b.local(ps);
        let rs_e = b.borrow(Mutbl::Not, ls);
        let call_f = b.call(lf, &[rs_e]);
        let st_c = b.semi(call_f);
        let root = b.block(&[st_f, st_s, st_c], None);
        let mut view = b.view(owner, root);
        view.types = w.fx.t.view();
        let mut pats_owned = view.pats.to_vec();
        pats_owned[pat_t.ix()] = blaze_typeck::Pat::Lit(blaze_typeck::Lit::Bool(true));
        pats_owned[pat_f.ix()] = blaze_typeck::Pat::Lit(blaze_typeck::Lit::Bool(false));
        view.pats = &pats_owned;
        crate::region_fixture::run(&w.fx, &view, &facts, facts.decls(), Plant::None).map(|r| r.lines)
    };

    assert_eq!(run(f_ok, "take_any2"), Ok(std::vec::Vec::new()), "POSITIVE: match of higher-ranked FnDefs must pass");
    let bad = run(f_bad, "take_static").expect("bad lowers cleanly");
    assert!(bad.iter().any(|l| l.starts_with("borrow ")), "NEGATIVE: match arm requiring `&'static S` must reject `&local`: {bad:?}");
}

/// Callables unwrapped through `?` (`match Try::branch(x) { Continue(v) => v, Break(r) => return .. }`),
/// covering `heap_pages`, `alt_paths`, `current_file`, and `const_holds`:
/// - Positive: `let f = opt_any()?; f(&local);` passes cleanly.
/// - Negative: `let f = opt_static()?; f(&local);` enforces `'static` and rejects `&local`.
#[test]
fn try_unwrapped_callable_traces_source_signature_soundly() {
    let mut items = None;
    let w = world_with(|fx, st, rs, unit| {
        let p0 = fx.param(0);
        let p1 = fx.param(1);
        let fn_list = fx.list(&[rs, unit]);
        let fn_ty = fx.ty(PNode::FnPtr(fn_list, blaze_typeck::FnPtrHeader::RUST));
        let (opt, _) = fx.enum_("Option", 1, &[("Some", &[p0]), ("None", &[])]);
        let (cf, _) = fx.enum_("ControlFlow", 2, &[("Continue", &[p1]), ("Break", &[p0])]);
        let opt_p0 = fx.adt(opt, &[p0]);
        let cf_unit_p0 = fx.adt(cf, &[unit, p0]);
        let opt_fn = fx.adt(opt, &[fn_ty]);
        let opt_unit = fx.adt(opt, &[unit]);
        let branch = fx.func("branch", 1, &[opt_p0], cf_unit_p0, no_clauses());
        let none_fn = fx.func("none_out", 0, &[unit], opt_unit, no_clauses());
        let mk = fx.func("mk", 0, &[], st, no_clauses());
        let f_ok = fx.func("ok", 0, &[opt_fn], opt_unit, no_clauses());
        let f_bad = fx.func("bad", 0, &[opt_fn], opt_unit, no_clauses());
        items = Some((opt, cf, opt_p0, cf_unit_p0, opt_fn, opt_unit, branch, none_fn, mk, f_ok, f_bad));
    });
    let (opt, cf, opt_p0, cf_unit_p0, opt_fn, opt_unit, branch, none_fn, mk, f_ok, f_bad) = items.expect("declared");
    let mut facts = DynFacts::of(&w, (ItemKey(9001), ItemKey(9002), ItemKey(9003)));
    facts.adts.push((opt, AdtFacts { copy: true, drop_impl: false, needs_drop: false, lifetimes: 0, union: false, is_enum: true, is_box: false }, &[crate::Variance::Co]));
    facts.adts.push((cf, AdtFacts { copy: true, drop_impl: false, needs_drop: false, lifetimes: 0, union: false, is_enum: true, is_box: false }, &[crate::Variance::Co, crate::Variance::Co]));
    facts.field_counts.push((opt, 0, 1));
    facts.field_counts.push((opt, 1, 0));
    facts.field_counts.push((cf, 0, 1));
    facts.field_counts.push((cf, 1, 1));
    facts.lane.wide.sigs.decls.adt(opt, 0);
    facts.lane.wide.sigs.decls.adt(cf, 0);
    facts.lane.wide.add(mk, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[], output: w.st, slots: &[] });
    facts.lane.wide.add(none_fn, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[w.unit], output: opt_unit, slots: &[] });
    facts.lane.wide.add(branch, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[opt_p0], output: cf_unit_p0, slots: &[] });
    // `ok(cb: Option<fn(&S)>) -> Option<()>` (higher-ranked `fn(&S)` under binder 1)
    facts.lane.wide.add(f_ok, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[opt_fn], output: opt_unit, slots: &[BOUND1] });
    // `bad(cb: Option<fn(&'static S)>) -> Option<()>` (`'static`)
    facts.lane.wide.add(f_bad, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[opt_fn], output: opt_unit, slots: &[crate::Region::Static] });

    let run = |owner: ItemKey| {
        let mut b = Bb::new();
        let pcb = b.param("cb");
        let lcb = b.local(pcb);
        let pbranch = b.path(&["branch"]);
        let scr = b.call(pbranch, &[lcb]);
        let pv = b.bind("v");
        let pcont = b.variant_pat(&["ControlFlow", "Continue"], &[pv]);
        let lv = b.local(pv);
        let pr = b.bind("r");
        let pbrk = b.variant_pat(&["ControlFlow", "Break"], &[pr]);
        let lr = b.local(pr);
        let pnone = b.path(&["none_out"]);
        let call_none = b.call(pnone, &[lr]);
        let ret_brk = b.ret(Some(call_none));
        let try_m = b.match_(scr, &[(pcont, lv), (pbrk, ret_brk)]);
        let pf = b.bind("f");
        let st_f = b.let_(pf, None, try_m);
        let pmk = b.path(&["mk"]);
        let call_mk = b.call(pmk, &[]);
        let ps = b.bind("s");
        let st_s = b.let_(ps, None, call_mk);
        let lf = b.local(pf);
        let ls = b.local(ps);
        let rs_e = b.borrow(Mutbl::Not, ls);
        let call_f = b.call(lf, &[rs_e]);
        let st_c = b.semi(call_f);
        let pnone2 = b.path(&["none_out"]);
        let u = b.tuple(&[]);
        let tail = b.call(pnone2, &[u]);
        let root = b.block(&[st_f, st_s, st_c], Some(tail));
        let view = b.view(owner, root);
        crate::region_fixture::run(&w.fx, &view, &facts, facts.decls(), Plant::None).map(|r| r.lines)
    };

    assert_eq!(run(f_ok), Ok(std::vec::Vec::new()), "POSITIVE: `?`-unwrapped `Option<fn(&S)>` must pass");
    let bad = run(f_bad).expect("bad lowers cleanly");
    assert!(bad.iter().any(|l| l.starts_with("borrow ")), "NEGATIVE: `?`-unwrapped `Option<fn(&'static S)>` must reject `&local`: {bad:?}");
}

/// Closure parameter typed as `&dyn Fn(&S)` (`closing` in `blaze-report/src/frame/mod.rs`):
/// - Positive: `|draw: &dyn Fn(&S)| { let s = mk(); draw(&s); }` with no explicit `'static`/named
///   regions in `closure_param_regions` checks cleanly.
/// - Negative: if `closure_param_regions` records an explicit `'static` on `draw`, it is refused
///   (`CalleeValueUnknown`) rather than unsoundly treating `&'static S` as higher-ranked.
#[test]
fn closure_dyn_fn_param_higher_ranked_positive_and_static_negative() {
    let mut items = None;
    let w = world_with(|fx, st, rs, unit| {
        let fns = add_fn_traits(fx);
        let dyn_fn = dyn_fn_ty(fx, fns.0, fns.2, &[rs], unit);
        let ref_dyn = fx.r(Mutbl::Not, dyn_fn);
        let mk = fx.func("mk", 0, &[], st, no_clauses());
        let f = fx.func("f", 0, &[], unit, no_clauses());
        items = Some((fns, ref_dyn, mk, f));
    });
    let (fns, ref_dyn, mk, f) = items.expect("declared");
    let mut facts = DynFacts::of(&w, fns);
    facts.lane.wide.add(mk, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[], output: w.st, slots: &[] });
    facts.lane.wide.add(f, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[], output: w.unit, slots: &[] });

    let run = |with_static_region: bool| {
        let mut b = Bb::new();
        let pdraw = b.bind("draw");
        let pmk = b.path(&["mk"]);
        let call_mk = b.call(pmk, &[]);
        let ps = b.bind("s");
        let st1 = b.let_(ps, None, call_mk);
        let ldraw = b.local(pdraw);
        let ls = b.local(ps);
        let rs_e = b.borrow(Mutbl::Not, ls);
        let call_draw = b.call(ldraw, &[rs_e]);
        let st2 = b.semi(call_draw);
        let inner = b.block(&[st1, st2], None);
        let cl = b.closure(&[pdraw], inner);
        let pc = b.bind("c");
        let st_c = b.let_(pc, None, cl);
        let root = b.block(&[st_c], None);
        let mut body = b.view(f, root);
        let opt_tys = [Some(ref_dyn)];
        body.types = w.fx.t.view();
        body.opt_types = &opt_tys;
        let written = [blaze_typeck::ClosureParamRegion {
            closure: cl,
            param: 0,
            path: blaze_typeck::ClosureParamRegion::ROOT,
            own: 0,
            kind: blaze_typeck::WrittenRegion::Static,
        }];
        if with_static_region {
            body.closure_param_regions = &written;
        }
        let mut ck = crate::region_fixture::checked(&w.fx, &body);
        let regions = facts.decls().lay(w.fx.t.view(), &mut ck.types);
        let with = crate::region_fixture::WithRegions { base: &facts, regions: &regions };
        crate::lower_closure_external(&body, &ck, cl, &with, crate::region_fixture::alloc())
    };

    let ok = run(false).expect("POSITIVE: higher-ranked `&dyn Fn(&S)` closure param must lower cleanly");
    let report = blaze_flow::check(&ok.lowered.flow, crate::region_fixture::alloc()).expect("flow check");
    assert!(report.moves.is_empty() && report.borrows.is_empty(), "POSITIVE: expected clean check: {:?}", report.borrows);

    let bad = run(true);
    assert!(
        matches!(bad, Err(Refusal { form: Form::CalleeValueUnknown, .. })),
        "NEGATIVE: explicit `'static` on `&dyn Fn` closure param must be refused, got {:?}",
        bad.err()
    );
}

/// Regression & soundness test for `crates/blaze-report/src/frame/mod.rs::closing`:
/// `|pen: &mut Pen<'_, '_>, draw: &dyn Fn(&mut Pen<'_, '_>)| { draw(pen); }`
/// `front2::body::closure_param_regions` records a shared `WrittenRegion::Elided(0)` pair at
/// `ROOT` (outer `&`) and `step(ROOT, 0)` (`dyn`'s default `+ '_` object bound) on every
/// `&dyn Fn(...)` parameter.
/// - POSITIVE: that wrapper-only `Elided(0)` pair lowers cleanly and passes `blaze_flow::check`.
/// - NEGATIVE 1 (borrow soundness): an active shared borrow of `pen` across `draw(pen)` is still
///   caught as a borrow error.
/// - NEGATIVE 2: `WrittenRegion::Named` on the wrapper is refused (`CalleeValueUnknown`).
/// - NEGATIVE 3: `WrittenRegion::Elided(0)` deeper than the peeled wrapper chain (inside `dyn Fn`
///   inputs) is refused (`CalleeValueUnknown`).
/// - NEGATIVE 4: `WrittenRegion::Elided(0)` at `own > 0` on `dyn` is refused (`CalleeValueUnknown`).
/// - NEGATIVE 5: `WrittenRegion::Elided(0)` on a `fn(...)` pointer param (not `dyn`) is refused
///   (`CalleeValueUnknown`).
#[test]
fn closure_dyn_fn_param_elided_wrapper_positive_and_soundness_negatives() {
    use crate::Variance;
    use blaze_typeck::{ClosureParamRegion, WrittenRegion};

    let mut items = None;
    let w = world_with(|fx, st, _rs, unit| {
        let fns = add_fn_traits(fx);
        let pen_adt = fx.strukt("Pen", 0, &[]);
        let pen = fx.adt(pen_adt, &[]);
        let rm_pen = fx.r(Mutbl::Mut, pen);
        let r_rm_pen = fx.r(Mutbl::Not, rm_pen);
        let dyn_fn = dyn_fn_ty(fx, fns.0, fns.2, &[rm_pen], unit);
        let ref_dyn = fx.r(Mutbl::Not, dyn_fn);
        let fn_list = fx.list(&[rm_pen, unit]);
        let fn_ptr = fx.ty(PNode::FnPtr(fn_list, FnPtrHeader::RUST));
        let use_pen = fx.func("use_pen", 0, &[r_rm_pen], unit, no_clauses());
        let f = fx.func("f", 0, &[], unit, no_clauses());
        let _ = st;
        items = Some((fns, pen_adt, r_rm_pen, rm_pen, ref_dyn, fn_ptr, use_pen, f));
    });
    let (fns, pen_adt, r_rm_pen, rm_pen, ref_dyn, fn_ptr, use_pen, f) = items.expect("declared");
    const PEN_VAR: [Variance; 2] = [Variance::In, Variance::In];
    let mut facts = DynFacts::of(&w, fns);
    facts.adts.push((
        pen_adt,
        AdtFacts { copy: false, drop_impl: false, needs_drop: false, lifetimes: 2, union: false, is_enum: false, is_box: false },
        &PEN_VAR,
    ));
    facts.lane.wide.sigs.decls.adt(pen_adt, 2);
    facts.lane.wide.add(f, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[], output: w.unit, slots: &[] });
    facts.lane.wide.add(
        use_pen,
        FnHead {
            lifetimes: 4,
            late: &[0, 1, 2, 3],
            outlives: &[],
            inputs: &[r_rm_pen],
            output: w.unit,
            slots: &[P0, crate::region_fixture::P1, crate::region_fixture::P2, crate::Region::Param(3)],
        },
    );

    let root_path = ClosureParamRegion::ROOT;
    let step1 = ClosureParamRegion::push_step(root_path, 0).expect("step1");
    let step2 = ClosureParamRegion::push_step(step1, 0).expect("step2");

    let run_dyn = |conflict_borrow: bool, regions_fn: &dyn Fn(ExprId) -> std::vec::Vec<ClosureParamRegion>| {
        let mut b = Bb::new();
        let ppen = b.bind("pen");
        let pdraw = b.bind("draw");
        let inner = if conflict_borrow {
            let pr = b.bind("r");
            let lpen1 = b.local(ppen);
            let bor_pen = b.borrow(Mutbl::Not, lpen1);
            let st1 = b.let_(pr, None, bor_pen);
            let ldraw = b.local(pdraw);
            let lpen2 = b.local(ppen);
            let call_draw = b.call(ldraw, &[lpen2]);
            let st2 = b.semi(call_draw);
            let puse = b.path(&["use_pen"]);
            let lr = b.local(pr);
            let call_use = b.call(puse, &[lr]);
            let st3 = b.semi(call_use);
            b.block(&[st1, st2, st3], None)
        } else {
            let ldraw = b.local(pdraw);
            let lpen = b.local(ppen);
            let call_draw = b.call(ldraw, &[lpen]);
            let st = b.semi(call_draw);
            b.block(&[st], None)
        };
        let cl = b.closure(&[ppen, pdraw], inner);
        let pc = b.bind("c");
        let st_c = b.let_(pc, None, cl);
        let root = b.block(&[st_c], None);
        let mut body = b.view(f, root);
        let opt_tys = [Some(rm_pen), Some(ref_dyn)];
        body.types = w.fx.t.view();
        body.opt_types = &opt_tys;
        let mut written = regions_fn(cl);
        written.sort_unstable();
        body.closure_param_regions = &written;
        let mut ck = crate::region_fixture::checked(&w.fx, &body);
        let regions = facts.decls().lay(w.fx.t.view(), &mut ck.types);
        let with = crate::region_fixture::WithRegions { base: &facts, regions: &regions };
        crate::lower_closure_external(&body, &ck, cl, &with, crate::region_fixture::alloc())
    };

    // POSITIVE: exact front2 `&dyn Fn(&mut Pen<'_, '_>)` wrapper-only `Elided(0)` pair checks cleanly.
    let ok = run_dyn(false, &|cl| {
        std::vec![
            ClosureParamRegion { closure: cl, param: 1, path: root_path, own: 0, kind: WrittenRegion::Elided(0) },
            ClosureParamRegion { closure: cl, param: 1, path: step1, own: 0, kind: WrittenRegion::Elided(0) },
        ]
    })
    .expect("POSITIVE: `&dyn Fn(&mut Pen<'_, '_>)` with wrapper-only `Elided(0)` must lower cleanly");
    let report = blaze_flow::check(&ok.lowered.flow, crate::region_fixture::alloc()).expect("flow check");
    assert!(report.moves.is_empty() && report.borrows.is_empty(), "POSITIVE: expected clean check: {:?}", report.borrows);

    // NEGATIVE 1: active shared borrow across `draw(pen)` is still caught.
    let conflict = run_dyn(true, &|cl| {
        std::vec![
            ClosureParamRegion { closure: cl, param: 1, path: root_path, own: 0, kind: WrittenRegion::Elided(0) },
            ClosureParamRegion { closure: cl, param: 1, path: step1, own: 0, kind: WrittenRegion::Elided(0) },
        ]
    })
    .expect("NEGATIVE 1: conflicting borrow body lowers cleanly");
    let conflict_report = blaze_flow::check(&conflict.lowered.flow, crate::region_fixture::alloc()).expect("flow check");
    assert!(
        !conflict_report.borrows.is_empty(),
        "NEGATIVE 1: active `&*pen` across `draw(pen)` must report a borrow conflict"
    );

    // NEGATIVE 2: `WrittenRegion::Named` on wrapper is refused.
    let bad_named = run_dyn(false, &|cl| {
        std::vec![ClosureParamRegion { closure: cl, param: 1, path: step1, own: 0, kind: WrittenRegion::Named }]
    });
    assert!(
        matches!(bad_named, Err(Refusal { form: Form::CalleeValueUnknown, .. })),
        "NEGATIVE 2: `WrittenRegion::Named` on `&dyn Fn` param must be refused, got {:?}",
        bad_named.err()
    );

    // NEGATIVE 3: `WrittenRegion::Elided(0)` reaching inside `dyn Fn` inputs (depth 2 > wrapper depth 1) is refused.
    let bad_deep = run_dyn(false, &|cl| {
        std::vec![
            ClosureParamRegion { closure: cl, param: 1, path: root_path, own: 0, kind: WrittenRegion::Elided(0) },
            ClosureParamRegion { closure: cl, param: 1, path: step1, own: 0, kind: WrittenRegion::Elided(0) },
            ClosureParamRegion { closure: cl, param: 1, path: step2, own: 0, kind: WrittenRegion::Elided(0) },
        ]
    });
    assert!(
        matches!(bad_deep, Err(Refusal { form: Form::CalleeValueUnknown, .. })),
        "NEGATIVE 3: `WrittenRegion::Elided` inside `dyn Fn` inputs must be refused, got {:?}",
        bad_deep.err()
    );

    // NEGATIVE 4: `WrittenRegion::Elided(0)` at `own > 0` on `dyn` is refused.
    let bad_own = run_dyn(false, &|cl| {
        std::vec![ClosureParamRegion { closure: cl, param: 1, path: step1, own: 1, kind: WrittenRegion::Elided(0) }]
    });
    assert!(
        matches!(bad_own, Err(Refusal { form: Form::CalleeValueUnknown, .. })),
        "NEGATIVE 4: `WrittenRegion::Elided` at `own > 0` on `dyn` must be refused, got {:?}",
        bad_own.err()
    );

    // NEGATIVE 5: `WrittenRegion::Elided(0)` on a `fn(...)` pointer param (not `dyn`) is refused.
    let mut b = Bb::new();
    let ppen = b.bind("pen");
    let pcb = b.bind("cb");
    let lcb = b.local(pcb);
    let lpen = b.local(ppen);
    let call_cb = b.call(lcb, &[lpen]);
    let st = b.semi(call_cb);
    let inner = b.block(&[st], None);
    let cl = b.closure(&[ppen, pcb], inner);
    let pc = b.bind("c");
    let st_c = b.let_(pc, None, cl);
    let root = b.block(&[st_c], None);
    let mut body = b.view(f, root);
    let opt_tys = [Some(rm_pen), Some(fn_ptr)];
    body.types = w.fx.t.view();
    body.opt_types = &opt_tys;
    let written_fp = [
        ClosureParamRegion { closure: cl, param: 1, path: root_path, own: 0, kind: WrittenRegion::Elided(0) },
        ClosureParamRegion { closure: cl, param: 1, path: step1, own: 0, kind: WrittenRegion::Elided(0) },
    ];
    body.closure_param_regions = &written_fp;
    let mut ck = crate::region_fixture::checked(&w.fx, &body);
    let regions = facts.decls().lay(w.fx.t.view(), &mut ck.types);
    let with = crate::region_fixture::WithRegions { base: &facts, regions: &regions };
    let bad_fnptr = crate::lower_closure_external(&body, &ck, cl, &with, crate::region_fixture::alloc());
    assert!(
        matches!(bad_fnptr, Err(Refusal { form: Form::CalleeValueUnknown, .. })),
        "NEGATIVE 5: `WrittenRegion::Elided` on a `fn(...)` pointer param must be refused, got {:?}",
        bad_fnptr.err()
    );
}
