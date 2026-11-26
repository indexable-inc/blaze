//! Positive and negative soundness tests for:
//! 1. Unsizing to `dyn Trait<'m, Args..., Assoc = Term> + 'q` via `blaze-borrow` (and its sound
//!    conservative fallback when no impl selection row is present), including `CommandStore<'m, 'r>
//!    -> dyn Store<'m>`, `ScopedBy<'d, 'n> -> dyn MacroNames<'d, Out = &'d S>` with `impl_where`,
//!    and `MadeOnce<'n, [closure]> -> dyn ProcsByNeed<'n>` (closure signature tying).
//! 2. `PNode::Alias` in `closure::lay_written_param` (`|cx: &P::Cx|` with `ClosureParamRegion`).
//! 3. `lower_closure_as_fn_once` / `lower_closure_with_kind` for `FnOnce`-only consumers of
//!    `FnMut`-inferred `move` closures (`move || &mut *bufs_chunk` ok; `move || &bufs_chunk`
//!    rejected as a dangling borrow of an owned local).

use std::vec;

use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{
    ClosureKind, ClosureParamRegion, ExprId, ItemKey, LangItem, List, Mutbl, Name, PNode, PTy, Selected, WrittenRegion,
};

use crate::region_fixture::{self, Decls, FnHead, ImplHead, P0, P1, P2};
use crate::{AdtFacts, CallBounds, CalleeFacts, External, Facts, Need, Plant, Region, Requirement, lower_closure_as_fn_once, lower_closure_external, requirement};

struct UnsizeFacts {
    adts: vec::Vec<(ItemKey, u32, bool)>,
    traits: vec::Vec<(ItemKey, u32)>,
    aliases: vec::Vec<(ItemKey, Name, u32)>,
    fn_traits: [Option<ItemKey>; 3],
    future_trait: Option<ItemKey>,
    closure_bodies_checked: bool,
    async_bodies_checked: bool,
    closure_reqs: vec::Vec<(ExprId, vec::Vec<Requirement>)>,
}

impl UnsizeFacts {
    fn new() -> Self {
        Self {
            adts: vec::Vec::new(),
            traits: vec::Vec::new(),
            aliases: vec::Vec::new(),
            fn_traits: [None; 3],
            future_trait: None,
            closure_bodies_checked: false,
            async_bodies_checked: false,
            closure_reqs: vec::Vec::new(),
        }
    }
}

impl Facts for UnsizeFacts {
    fn adt(&self, k: ItemKey) -> Option<AdtFacts> {
        let (lifetimes, copy) = self.adts.iter().find(|&&(id, _, _)| id == k).map_or((0, false), |&(_, l, c)| (l, c));
        Some(AdtFacts { copy, drop_impl: false, needs_drop: false, lifetimes, union: false, is_enum: false, is_box: false })
    }
    fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: false, self_ref: false, c_variadic: false })
    }
    fn regions(&self) -> &dyn crate::RegionFacts {
        &crate::NoRegions
    }
    fn region_tys(&self) -> Option<blaze_typeck::PTys<'_>> {
        None
    }
    fn adt_variances(&self, k: ItemKey) -> Option<&[crate::Variance]> {
        const CO4: &[crate::Variance] = &[crate::Variance::Co, crate::Variance::Co, crate::Variance::Co, crate::Variance::Co];
        let _ = k;
        Some(CO4)
    }
    fn implied_bounds(&self) -> Option<(&[(u32, Region)], &[(Region, Region)])> {
        None
    }
    fn constant_initializer(&self) -> bool {
        false
    }
    fn fn_binders(&self) -> bool {
        true
    }
    fn closure_bodies_checked(&self) -> bool {
        self.closure_bodies_checked
    }
    fn closure_requirements(&self, c: ExprId) -> &[Requirement] {
        self.closure_reqs.iter().find(|(e, _)| *e == c).map_or(&[], |(_, r)| r.as_slice())
    }
    fn async_bodies_checked(&self) -> bool {
        self.async_bodies_checked
    }
    fn is_future(&self, _: PTy) -> Option<bool> {
        None
    }
    fn shape(&self, _: ItemKey) -> Option<crate::Shape<'_>> {
        None
    }
    fn object_trait_lifetimes(&self, k: ItemKey) -> Option<bool> {
        self.traits.iter().find(|&&(id, _)| id == k).map(|&(_, l)| l > 0)
    }
    fn call_bounds(&self, _: ItemKey) -> Option<CallBounds<'_>> {
        None
    }
    fn trait_lifetimes(&self, k: ItemKey) -> Option<u32> {
        self.traits.iter().find(|&&(id, _)| id == k).map(|&(_, l)| l)
    }
    fn alias_lifetimes(&self, k: ItemKey, n: Name) -> Option<u32> {
        self.aliases.iter().find(|&&(id, nm, _)| id == k && nm == n).map(|&(_, _, l)| l)
    }
    fn is_fn_trait(&self, k: ItemKey) -> bool {
        self.fn_traits.contains(&Some(k))
    }
    fn is_future_trait(&self, k: ItemKey) -> bool {
        self.future_trait == Some(k)
    }
    fn ties(&self, _: ItemKey) -> Option<crate::Ties<'_>> {
        None
    }
    fn field(&self, _: ItemKey, _: u32, _: Name) -> Option<u32> {
        None
    }
    fn field_param(&self, _: ItemKey, _: u32, _: u32) -> Option<u32> {
        None
    }
    fn field_lead(&self, _: ItemKey, _: u32, _: u32) -> Option<crate::FieldLead> {
        None
    }
    fn field_regions(&self, _: ItemKey, _: u32, _: u32) -> Option<&[crate::FieldRegion]> {
        None
    }
    fn copy_args(&self, _: ItemKey) -> Option<&[bool]> {
        None
    }
    fn field_count(&self, _: ItemKey, _: u32) -> Option<u32> {
        None
    }
    fn param_copy(&self, _: u32) -> bool {
        false
    }
}

/// `&'r mut CommandStore<'m, 'r2> -> &'r mut dyn Store<'m>` (3 slots -> 3 slots, `engine_dry.rs` /
/// `engine_check.rs` shape):
/// - POSITIVE 1: two sequential `&mut store` calls to `load_pointer(&mut dyn Store<'_>)` pass
///   cleanly (outer `&mut` borrow is NOT coupled to `'m`/`'r2`).
/// - POSITIVE 2: `read_store(&mut store)` returns `&'m S` when `'m` and `'r2` are unrelated
///   (proving `'r2` is NOT equated with `Store<'m>`).
/// - NEGATIVE 1 (soundness): returning `read_store(&mut store)` as `&'r2 S` MUST FAIL (`'m != 'r2`).
/// - NEGATIVE 2 (soundness, `s.len == d.len == 3`): `CommandStore<'local, 'static>` unsized to
///   `&mut dyn Store<'static>` MUST FAIL (proving slot 1 `'local` is equated with `Store<'static>`,
///   never skipped by positional `relate_by`!).
/// - NEGATIVE 3 (soundness, `T: 'q`): `CommandStore<'static, 'local>` unsized to
///   `&mut (dyn Store<'static> + 'static)` MUST FAIL (`'local` violates `T: 'static`).
/// - NEGATIVE 4 (conservative fallback soundness): `CommandStore<'local, 'static>` -> `&mut dyn
///   Store<'static>` with NO selection row in `ck.selected` MUST STILL FAIL (never falls through
///   to positional `relate_by`!).
#[test]
fn unsize_mut_store_with_trait_lifetime_decouples_borrow_and_checks_trait_arg() {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, vec![], &[]);
    fx.lang(LangItem::Sized, sized);
    let unsize = fx.tr("Unsize", 2, vec![], &[]);
    fx.lang(LangItem::Unsize, unsize);
    let s_k = fx.strukt("S", 0, &[]);
    let st = fx.adt(s_k, &[]);
    let rs = fx.r(Mutbl::Not, st);
    let unit = fx.unit();

    let store_tr = fx.tr("Store", 1, vec![], &[]);
    let cs_k = fx.strukt("CommandStore", 0, &[]);
    let cs_ty = fx.adt(cs_k, &[]);
    let mut_cs = fx.r(Mutbl::Mut, cs_ty);

    let ex_store = fx.ty(PNode::ExTrait(store_tr, List::EMPTY));
    let dyn_store_preds = fx.list(&[ex_store]);
    let dyn_store = fx.ty(PNode::Dynamic(dyn_store_preds));
    let mut_dyn_store = fx.r(Mutbl::Mut, dyn_store);

    let store_impl = fx.imp("impl<'m, 'r> Store<'m> for CommandStore<'m, 'r>", 0, cs_ty, Some((store_tr, &[])), &[], vec![]);

    let load_ptr = fx.func("load_pointer", 0, &[mut_dyn_store], unit, vec![]);
    let read_store = fx.func("read_store", 0, &[mut_dyn_store], rs, vec![]);
    let take_static_store = fx.func("take_static_store", 0, &[mut_dyn_store], unit, vec![]);
    let take_static_obj = fx.func("take_static_obj", 0, &[mut_dyn_store], unit, vec![]);
    let mk_store = fx.func("mk_store", 0, &[rs, rs], cs_ty, vec![]);

    let fn_two_calls = fx.func("two_calls", 0, &[cs_ty], unit, vec![]);
    let fn_ret_m = fx.func("ret_m", 0, &[mut_cs], rs, vec![]);
    let fn_ret_r_bad = fx.func("ret_r_bad", 0, &[mut_cs], rs, vec![]);
    let fn_bad_static = fx.func("bad_static", 0, &[st, rs], unit, vec![]);
    let fn_bad_obj = fx.func("bad_obj", 0, &[st, rs], unit, vec![]);
    fx.finish();

    let mut decls = Decls::new();
    decls.adt(cs_k, 2);
    decls.trait_(store_tr, 1);
    // `impl<'m, 'r> Store<'m> for CommandStore<'m, 'r>`
    decls.imp(store_impl, ImplHead { lifetimes: 2, outlives: &[], self_ty: cs_ty, self_slots: &[P0, P1], trait_ref: Some((store_tr, &[], &[P0])) });
    // `fn load_pointer<'a, 'o, 'm>(store: &'a mut (dyn Store<'m> + 'o))`
    decls.func(load_ptr, FnHead { lifetimes: 3, late: &[0, 1, 2], outlives: &[], inputs: &[mut_dyn_store], output: unit, slots: &[P0, P1, P2] });
    // `fn read_store<'a, 'o, 'm>(store: &'a mut (dyn Store<'m> + 'o)) -> &'m S`
    decls.func(read_store, FnHead { lifetimes: 3, late: &[0, 1, 2], outlives: &[], inputs: &[mut_dyn_store], output: rs, slots: &[P0, P1, P2, P2] });
    // `fn take_static_store<'a, 'o>(store: &'a mut (dyn Store<'static> + 'o))`
    decls.func(
        take_static_store,
        FnHead { lifetimes: 2, late: &[0, 1], outlives: &[], inputs: &[mut_dyn_store], output: unit, slots: &[P0, P1, Region::Static] },
    );
    // `fn take_static_obj<'a>(store: &'a mut (dyn Store<'static> + 'static))`
    decls.func(
        take_static_obj,
        FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[mut_dyn_store], output: unit, slots: &[P0, Region::Static, Region::Static] },
    );
    // `fn mk_store<'m, 'r>(a: &'m S, b: &'r S) -> CommandStore<'m, 'r>`
    decls.func(mk_store, FnHead { lifetimes: 2, late: &[0, 1], outlives: &[], inputs: &[rs, rs], output: cs_ty, slots: &[P0, P1, P0, P1] });

    decls.func(fn_two_calls, FnHead { lifetimes: 2, late: &[], outlives: &[], inputs: &[cs_ty], output: unit, slots: &[P0, P1] });
    decls.func(fn_ret_m, FnHead { lifetimes: 3, late: &[], outlives: &[], inputs: &[mut_cs], output: rs, slots: &[P0, P1, P2, P1] });
    decls.func(fn_ret_r_bad, FnHead { lifetimes: 3, late: &[], outlives: &[], inputs: &[mut_cs], output: rs, slots: &[P0, P1, P2, P2] });
    decls.func(fn_bad_static, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[st, rs], output: unit, slots: &[Region::Static] });
    decls.func(fn_bad_obj, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[st, rs], output: unit, slots: &[Region::Static] });

    let mut facts = UnsizeFacts::new();
    facts.adts.push((cs_k, 2, false));
    facts.traits.push((store_tr, 1));

    // POSITIVE 1: two sequential `load_pointer(&mut store)` calls.
    {
        let mut b = Bb::new();
        let ps = b.param("store");
        let f1 = b.path(&["load_pointer"]);
        let l1 = b.local(ps);
        let r1 = b.borrow(Mutbl::Mut, l1);
        let c1 = b.call(f1, &[r1]);
        let st1 = b.semi(c1);
        let f2 = b.path(&["load_pointer"]);
        let l2 = b.local(ps);
        let r2 = b.borrow(Mutbl::Mut, l2);
        let c2 = b.call(f2, &[r2]);
        let st2 = b.semi(c2);
        let root = b.block(&[st1, st2], None);
        let view = b.view(fn_two_calls, root);
        let mut ck = region_fixture::checked(&fx, &view);
        assert!(
            ck.call_selections(r1).iter().any(|r| r.trait_ == store_tr && matches!(r.selected, Selected::Impl { imp, .. } if imp == store_impl)),
            "blaze-typeck must emit Unsize selection row at r1: {:?}",
            ck.call_selections(r1)
        );
        assert!(
            ck.call_selections(r2).iter().any(|r| r.trait_ == store_tr && matches!(r.selected, Selected::Impl { imp, .. } if imp == store_impl)),
            "blaze-typeck must emit Unsize selection row at r2: {:?}",
            ck.call_selections(r2)
        );
        let got = region_fixture::lower(&fx, &view, &mut ck, &facts, &decls, Plant::None).expect("lower");
        assert!(got.lines.is_empty(), "POSITIVE 1: two `&mut store` unsizings must pass cleanly: {:?}", got.lines);
    }

    // POSITIVE 2: `fn ret_m<'a, 'm, 'r>(store: &'a mut CommandStore<'m, 'r>) -> &'m S { read_store(store) }`
    {
        let mut b = Bb::new();
        let ps = b.param("store");
        let f = b.path(&["read_store"]);
        let ls = b.local(ps);
        let call = b.call(f, &[ls]);
        let root = b.block(&[], Some(call));
        let view = b.view(fn_ret_m, root);
        let mut ck = region_fixture::checked(&fx, &view);
        let got = region_fixture::lower(&fx, &view, &mut ck, &facts, &decls, Plant::None).expect("lower");
        assert!(got.lines.is_empty(), "POSITIVE 2: returning `Store<'m>` as `'m` must pass cleanly: {:?}", got.lines);
    }

    // NEGATIVE 1: `fn ret_r_bad<'a, 'm, 'r>(store: &'a mut CommandStore<'m, 'r>) -> &'r S { read_store(store) }`
    {
        let mut b = Bb::new();
        let ps = b.param("store");
        let f = b.path(&["read_store"]);
        let ls = b.local(ps);
        let call = b.call(f, &[ls]);
        let root = b.block(&[], Some(call));
        let view = b.view(fn_ret_r_bad, root);
        let mut ck = region_fixture::checked(&fx, &view);
        let got = region_fixture::lower(&fx, &view, &mut ck, &facts, &decls, Plant::None).expect("lower");
        assert!(
            !got.lines.is_empty(),
            "NEGATIVE 1: returning `Store<'m>` as unrelated `'r` MUST fail borrow check"
        );
    }

    // NEGATIVE 2 & NEGATIVE 4 (with and without selection row):
    // `fn bad_static(local: S, long: &'static S) { let mut store = mk_store(&local, long); take_static_store(&mut store); }`
    for with_row in [true, false] {
        let mut b = Bb::new();
        let plocal = b.param("local");
        let plong = b.param("long");
        let fmk = b.path(&["mk_store"]);
        let lloc = b.local(plocal);
        let rloc = b.borrow(Mutbl::Not, lloc);
        let llong = b.local(plong);
        let mk_c = b.call(fmk, &[rloc, llong]);
        let pstore = b.bind("store");
        let st1 = b.let_(pstore, None, mk_c);
        let ftake = b.path(&["take_static_store"]);
        let lstore = b.local(pstore);
        let rstore = b.borrow(Mutbl::Mut, lstore);
        let take_c = b.call(ftake, &[rstore]);
        let st2 = b.semi(take_c);
        let root = b.block(&[st1, st2], None);
        let view = b.view(fn_bad_static, root);
        let mut ck = region_fixture::checked(&fx, &view);
        if with_row {
            let got = region_fixture::lower(&fx, &view, &mut ck, &facts, &decls, Plant::None).expect("lower");
            assert!(
                got.lines.iter().any(|l| l.starts_with("borrow Invalidated")),
                "NEGATIVE 2: `CommandStore<'local, 'static>` -> `&mut dyn Store<'static>` MUST fail E0597: {:?}",
                got.lines
            );
        } else {
            ck.selected.clear();
            let regions = decls.lay(fx.t.view(), &mut ck.types);
            let with = region_fixture::WithRegions { base: &facts, regions: &regions };
            let lowered = crate::lower(&view, &ck, &with, region_fixture::alloc()).expect("fallback lowers");
            assert!(
                matches!(lowered.coarse, Some(crate::Refusal { form: crate::Form::Unified { why: crate::Unified::ValueInexact }, .. })),
                "NEGATIVE 4: unresolved unsizing must record coarse inner unify, got {:?}",
                lowered.coarse
            );
            let report = blaze_flow::check(&lowered.flow, region_fixture::alloc()).expect("check");
            assert!(
                report.borrows.iter().any(|f| matches!(f, blaze_flow::BorrowFinding::Invalidated { .. })),
                "NEGATIVE 4 (conservative fallback): `CommandStore<'local, 'static>` -> `&mut dyn Store<'static>` MUST STILL fail E0597: {:?}",
                report.borrows
            );
        }
    }

    // NEGATIVE 3 (`T: 'q` object bound violated):
    // `fn bad_obj(local: S, long: &'static S) { let mut store = mk_store(long, &local); take_static_obj(&mut store); }`
    {
        let mut b = Bb::new();
        let plocal = b.param("local");
        let plong = b.param("long");
        let fmk = b.path(&["mk_store"]);
        let llong = b.local(plong);
        let lloc = b.local(plocal);
        let rloc = b.borrow(Mutbl::Not, lloc);
        let mk_c = b.call(fmk, &[llong, rloc]);
        let pstore = b.bind("store");
        let st1 = b.let_(pstore, None, mk_c);
        let ftake = b.path(&["take_static_obj"]);
        let lstore = b.local(pstore);
        let rstore = b.borrow(Mutbl::Mut, lstore);
        let take_c = b.call(ftake, &[rstore]);
        let st2 = b.semi(take_c);
        let root = b.block(&[st1, st2], None);
        let view = b.view(fn_bad_obj, root);
        let mut ck = region_fixture::checked(&fx, &view);
        let got = region_fixture::lower(&fx, &view, &mut ck, &facts, &decls, Plant::None).expect("lower");
        assert!(
            got.lines.iter().any(|l| l.starts_with("borrow Invalidated")),
            "NEGATIVE 3: `CommandStore<'static, 'local>` -> `&mut (dyn Store<'static> + 'static)` MUST violate `T: 'q`: {:?}",
            got.lines
        );
    }
}

/// `ScopedBy<'d, 'n> -> dyn MacroNames<'d, Out = &'d S>` with `impl<'d, 'n: 'd> MacroNames<'d> for
/// ScopedBy<'d, 'n> { type Out = &'d S; }` (`front2/body.rs:1669` + `ExProj` shape):
/// - POSITIVE: `'n: 'd` holds (and `'d` does NOT outlive `'n`) -> passes cleanly.
/// - NEGATIVE 1: missing `'n: 'd` bound -> `impl_where` rejects it.
/// - NEGATIVE 2: `dyn MacroNames<'d, Out = &'b S>` with unrelated `'b` -> `ExProj` rejects it.
#[test]
fn unsize_scoped_by_with_impl_where_outlives_and_ex_proj() {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, vec![], &[]);
    fx.lang(LangItem::Sized, sized);
    let unsize = fx.tr("Unsize", 2, vec![], &[]);
    fx.lang(LangItem::Unsize, unsize);
    let s_k = fx.strukt("S", 0, &[]);
    let st = fx.adt(s_k, &[]);
    let rs = fx.r(Mutbl::Not, st);

    let mn_tr = fx.tr("MacroNames", 1, vec![], &["Out"]);
    let sc_k = fx.strukt("ScopedBy", 0, &[]);
    let sc_ty = fx.adt(sc_k, &[]);

    let ex_mn = fx.ty(PNode::ExTrait(mn_tr, List::EMPTY));
    let ex_out = fx.ty(PNode::ExProj { trait_: mn_tr, name: Name::of("Out"), args: List::EMPTY, term: rs });
    let dyn_preds = fx.list(&[ex_mn, ex_out]);
    let dyn_mn = fx.ty(PNode::Dynamic(dyn_preds));
    let ref_dyn_mn = fx.r(Mutbl::Not, dyn_mn);

    let mn_impl = fx.imp(
        "impl<'d, 'n: 'd> MacroNames<'d> for ScopedBy<'d, 'n> { type Out = &'d S; }",
        0,
        sc_ty,
        Some((mn_tr, &[])),
        &[("Out", rs)],
        vec![],
    );

    let use_names = fx.func("use_names", 0, &[ref_dyn_mn], rs, vec![]);
    let use_names_out = fx.func("use_names_out", 0, &[ref_dyn_mn], rs, vec![]);
    let fn_ok = fx.func("ok", 0, &[sc_ty], rs, vec![]);
    let fn_bad_where = fx.func("bad_where", 0, &[sc_ty], rs, vec![]);
    let fn_bad_assoc = fx.func("bad_assoc", 0, &[sc_ty], rs, vec![]);
    fx.finish();

    let mut decls = Decls::new();
    decls.adt(sc_k, 2);
    decls.trait_(mn_tr, 1);
    decls.alias(mn_tr, "Out", 1);
    // `impl<'d, 'n: 'd> MacroNames<'d> for ScopedBy<'d, 'n> { type Out = &'d S; }`
    decls.imp(
        mn_impl,
        ImplHead { lifetimes: 2, outlives: &[(P1, P0)], self_ty: sc_ty, self_slots: &[P0, P1], trait_ref: Some((mn_tr, &[], &[P0])) },
    );
    decls.impl_assoc(mn_impl, "Out", rs, &[P0]);

    // `ref_dyn_mn` slots: `[ref_lt, obj_lt, ex_trait_lt, ex_proj_trait_lt, ex_proj_term_lt]`
    // `fn use_names<'r, 'd>(n: &'r (dyn MacroNames<'d, Out = &'d S> + 'd)) -> &'d S`
    decls.func(
        use_names,
        FnHead { lifetimes: 2, late: &[0, 1], outlives: &[], inputs: &[ref_dyn_mn], output: rs, slots: &[P0, P1, P1, P1, P1, P1] },
    );
    // `fn use_names_out<'r, 'd, 'b>(n: &'r (dyn MacroNames<'d, Out = &'b S> + 'd)) -> &'b S`
    decls.func(
        use_names_out,
        FnHead { lifetimes: 3, late: &[0, 1, 2], outlives: &[], inputs: &[ref_dyn_mn], output: rs, slots: &[P0, P1, P1, P1, P2, P2] },
    );
    // `fn ok<'d, 'n: 'd>(s: ScopedBy<'d, 'n>) -> &'d S`
    decls.func(fn_ok, FnHead { lifetimes: 2, late: &[], outlives: &[(P1, P0)], inputs: &[sc_ty], output: rs, slots: &[P0, P1, P0] });
    // `fn bad_where<'d, 'n>(s: ScopedBy<'d, 'n>) -> &'d S` (missing `'n: 'd`!)
    decls.func(fn_bad_where, FnHead { lifetimes: 2, late: &[], outlives: &[], inputs: &[sc_ty], output: rs, slots: &[P0, P1, P0] });
    // `fn bad_assoc<'d, 'n: 'd, 'b>(s: ScopedBy<'d, 'n>) -> &'b S`
    decls.func(fn_bad_assoc, FnHead { lifetimes: 3, late: &[], outlives: &[(P1, P0)], inputs: &[sc_ty], output: rs, slots: &[P0, P1, P2] });

    let mut facts = UnsizeFacts::new();
    facts.adts.push((sc_k, 2, false));
    facts.traits.push((mn_tr, 1));
    facts.aliases.push((mn_tr, Name::of("Out"), 1));

    let run_case = |owner: ItemKey, callee: &str| {
        let mut b = Bb::new();
        let ps = b.param("s");
        let f = b.path(&[callee]);
        let ls = b.local(ps);
        let rs_e = b.borrow(Mutbl::Not, ls);
        let call = b.call(f, &[rs_e]);
        let root = b.block(&[], Some(call));
        let view = b.view(owner, root);
        let mut ck = region_fixture::checked(&fx, &view);
        assert!(
            ck.call_selections(rs_e).iter().any(|r| r.trait_ == mn_tr && r.name.is_none() && matches!(r.selected, Selected::Impl { imp, .. } if imp == mn_impl)),
            "blaze-typeck must emit ExTrait Unsize row for ScopedBy -> dyn MacroNames: {:?}",
            ck.call_selections(rs_e)
        );
        assert!(
            ck.call_selections(rs_e).iter().any(|r| r.trait_ == mn_tr && r.name == Some(Name::of("Out")) && matches!(r.selected, Selected::Impl { imp, .. } if imp == mn_impl)),
            "blaze-typeck must emit ExProj Unsize row for ScopedBy -> dyn MacroNames<Out = ..>: {:?}",
            ck.call_selections(rs_e)
        );
        region_fixture::lower(&fx, &view, &mut ck, &facts, &decls, Plant::None).expect("lower")
    };

    let ok = run_case(fn_ok, "use_names");
    assert!(ok.lines.is_empty(), "POSITIVE: `ScopedBy<'d, 'n>` with `'n: 'd` unsized to `&dyn MacroNames<'d, Out = &'d S>` must pass: {:?}", ok.lines);

    let bad_where = run_case(fn_bad_where, "use_names");
    assert!(
        !bad_where.lines.is_empty(),
        "NEGATIVE 1: missing `'n: 'd` required by `impl<'d, 'n: 'd>` MUST fail borrow check"
    );

    let bad_assoc = run_case(fn_bad_assoc, "use_names_out");
    assert!(
        !bad_assoc.lines.is_empty(),
        "NEGATIVE 2: `dyn MacroNames<'d, Out = &'b S>` with unrelated `'b` MUST fail `ExProj`"
    );
}

/// `MadeOnce<'n, [closure]> -> &dyn ProcsByNeed<'n>` (`engine_export.rs` / `engine_start.rs` shape):
/// `impl<'n, F: Fn() -> &'n S> ProcsByNeed<'n> for MadeOnce<'n, F>`.
/// - POSITIVE: closure `|| r` (with requirement `Capture(0): Sig(0)`) unsized through `MadeOnce`
///   ties its signature via `impl_where` -> NO `ClosureSigErased`, checks CLEAN!
/// - NEGATIVE (soundness): closure `|| &local` (capturing a local borrow with `Capture(0): Sig(0)`)
///   unsized through `MadeOnce` into `dyn ProcsByNeed<'n>` -> MUST FAIL E0597 (`Invalidated`)!
#[test]
fn unsize_made_once_closure_ties_signature_via_impl_where() {
    let mut fx = Fx::new();
    let copy_tr = fx.tr("Copy", 1, vec![], &[]);
    fx.lang(LangItem::Copy, copy_tr);
    let p0 = fx.param(0);
    let ref_p0 = fx.r(Mutbl::Not, p0);
    let _ = fx.imp("impl<T: ?Sized> Copy for &T", 1, ref_p0, Some((copy_tr, &[])), &[], vec![]);
    let sized = fx.tr("Sized", 1, vec![], &[]);
    fx.lang(LangItem::Sized, sized);
    let unsize = fx.tr("Unsize", 2, vec![], &[]);
    fx.lang(LangItem::Unsize, unsize);

    let fn_once = fx.tr("FnOnce", 2, vec![], &["Output"]);
    let fn_mut = fx.tr("FnMut", 2, vec![], &[]);
    let fn_tr = fx.tr("Fn", 2, vec![], &[]);
    fx.lang(LangItem::FnOnce, fn_once);
    fx.lang(LangItem::FnMut, fn_mut);
    fx.lang(LangItem::Fn, fn_tr);

    let s_k = fx.strukt("S", 0, &[]);
    let st = fx.adt(s_k, &[]);
    let rs = fx.r(Mutbl::Not, st);
    let unit_tup = fx.tuple(&[]);

    let procs_tr = fx.tr("ProcsByNeed", 1, vec![], &[]);
    let made_k = fx.strukt("MadeOnce", 1, &[]);
    let made_p0 = fx.adt(made_k, &[p0]);

    let ex_procs = fx.ty(PNode::ExTrait(procs_tr, List::EMPTY));
    let dyn_preds = fx.list(&[ex_procs]);
    let dyn_procs = fx.ty(PNode::Dynamic(dyn_preds));
    let ref_dyn_procs = fx.r(Mutbl::Not, dyn_procs);

    let c_fn = fx.bound(fn_tr, &[p0, unit_tup]);
    let c_out = fx.proj(fn_once, &[p0, unit_tup], "Output", rs);
    let made_impl = fx.imp(
        "impl<'n, F: Fn() -> &'n S> ProcsByNeed<'n> for MadeOnce<'n, F>",
        1,
        made_p0,
        Some((procs_tr, &[])),
        &[],
        vec![c_fn, c_out],
    );

    let make_once = fx.func("make_once", 1, &[p0], made_p0, vec![]);
    let run_procs = fx.func("run_procs", 0, &[ref_dyn_procs], rs, vec![]);
    let fn_ok = fx.func("ok", 0, &[rs], rs, vec![]);
    let fn_bad = fx.func("bad", 0, &[st], rs, vec![]);

    let fn_clause_args = fx.list(&[p0, unit_tup]);
    let ex_imp_fn = fx.ty(PNode::ExTrait(fn_tr, fn_clause_args));
    let ex_imp_out = fx.ty(PNode::ExProj { trait_: fn_once, name: Name::of("Output"), args: fn_clause_args, term: rs });
    fx.finish();

    let mut decls = Decls::new();
    decls.adt(made_k, 1);
    decls.trait_(procs_tr, 1);
    decls.trait_(fn_tr, 0);
    decls.trait_(fn_mut, 0);
    decls.trait_(fn_once, 0);
    decls.alias(fn_once, "Output", 0);
    // `impl<'n, F: Fn() -> &'n S> ProcsByNeed<'n> for MadeOnce<'n, F>`
    decls.imp(made_impl, ImplHead { lifetimes: 1, outlives: &[], self_ty: made_p0, self_slots: &[P0], trait_ref: Some((procs_tr, &[], &[P0])) });
    decls.impl_clause(made_impl, 0, 0, ex_imp_fn, &[]);
    decls.impl_clause(made_impl, 1, 0, ex_imp_out, &[P0]);

    // `fn make_once<'n, F>(f: F) -> MadeOnce<'n, F>`
    decls.func(make_once, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[p0], output: made_p0, slots: &[P0] });
    // `fn run_procs<'r, 'o, 'n>(p: &'r (dyn ProcsByNeed<'n> + 'o)) -> &'n S`
    decls.func(run_procs, FnHead { lifetimes: 3, late: &[0, 1, 2], outlives: &[], inputs: &[ref_dyn_procs], output: rs, slots: &[P0, P1, P2, P2] });
    // `fn ok<'n>(r: &'n S) -> &'n S`
    decls.func(fn_ok, FnHead { lifetimes: 1, late: &[], outlives: &[], inputs: &[rs], output: rs, slots: &[P0, P0] });
    // `fn bad<'n>(local: S) -> &'n S`
    decls.func(fn_bad, FnHead { lifetimes: 1, late: &[], outlives: &[], inputs: &[st], output: rs, slots: &[P0] });

    let run_case = |owner: ItemKey, borrow_local: bool| {
        let mut b = Bb::new();
        let arg = b.param(if borrow_local { "local" } else { "r" });
        let body_expr = if borrow_local {
            let l = b.local(arg);
            b.borrow(Mutbl::Not, l)
        } else {
            b.local(arg)
        };
        let cl = if borrow_local { b.closure(&[], body_expr) } else { b.move_closure(&[], body_expr) };
        let fmk = b.path(&["make_once"]);
        let mk_c = b.call(fmk, &[cl]);
        let pm = b.bind("m");
        let st1 = b.let_(pm, None, mk_c);
        let frun = b.path(&["run_procs"]);
        let lm = b.local(pm);
        let rm = b.borrow(Mutbl::Not, lm);
        let run_c = b.call(frun, &[rm]);
        let root = b.block(&[st1], Some(run_c));

        let view = b.view(owner, root);
        let mut ck = region_fixture::checked(&fx, &view);
        assert!(
            ck.call_selections(rm).iter().any(|r| r.trait_ == procs_tr && matches!(r.selected, Selected::Impl { imp, .. } if imp == made_impl)),
            "blaze-typeck must emit Unsize selection row for MadeOnce -> dyn ProcsByNeed: {:?}",
            ck.call_selections(rm)
        );

        let mut facts = UnsizeFacts::new();
        facts.adts.push((made_k, 1, false));
        facts.traits.push((procs_tr, 1));
        facts.traits.push((fn_tr, 0));
        facts.traits.push((fn_mut, 0));
        facts.traits.push((fn_once, 0));
        facts.aliases.push((fn_once, Name::of("Output"), 0));
        facts.fn_traits = [Some(fn_tr), Some(fn_mut), Some(fn_once)];
        facts.closure_bodies_checked = true;
        // Closure `|| r` / `|| &local` requires its capture slot to outlive its return sig slot:
        // `Need::Capture(0) : Need::Sig(0)` (so `sig_needs` is true and would refuse with
        // `ClosureSigErased` unless `unsize_via_borrow` ties the signature through `impl_where`).
        facts.closure_reqs.push((cl, vec![Requirement { from: Need::Capture(0), into: Need::Sig(0) }]));

        region_fixture::lower(&fx, &view, &mut ck, &facts, &decls, Plant::None)
    };

    let ok = run_case(fn_ok, false).expect("POSITIVE: MadeOnce<'n, [closure]> must tie sig without ClosureSigErased");
    assert!(ok.lines.is_empty(), "POSITIVE: `MadeOnce` with `|| r` must check clean: {:?}", ok.lines);

    let bad = run_case(fn_bad, true).expect("NEGATIVE: must lower and report borrow error, not refuse");
    assert!(
        bad.lines.iter().any(|l| l.starts_with("borrow Invalidated")),
        "NEGATIVE: `MadeOnce` with `|| &local` returning into `'n` MUST fail E0597: {:?}",
        bad.lines
    );
}

/// `PNode::Alias` in `closure::lay_written_param` (`crates/blaze-engine/src/shell.rs:283` shape:
/// `|cx: &P::Cx| use_cx(cx)` where `trait Pool<'a> { type Cx; }` and `fn use_cx<'a, P: Pool<'a>>(cx: &P::Cx)`):
/// - POSITIVE: `ClosureParamRegion` marks `<P as Pool<'a>>::Cx`'s own trait lifetime as
///   `WrittenRegion::Named` -> laid as `SigSlot::Owner(0)` -> passing `cx` to `use_cx` checks CLEAN.
/// - NEGATIVE (soundness): without `WrittenRegion::Named` (an elided lifetime inside the alias,
///   e.g. `|cx: &P::Cx<'_>|`), the slot is laid as `SigSlot::Late(None)` (rigid universal) ->
///   passing `cx` to `use_cx` requiring owner `'a` MUST FAIL borrow check (E0521)!
#[test]
fn lay_written_param_reads_alias_closure_param_regions() {
    let mut fx = Fx::new();
    let pool_tr = fx.tr("Pool", 1, vec![], &["Cx"]);
    let p0 = fx.param(0);
    let cx_alias = fx.alias(pool_tr, "Cx", &[p0]);
    let ref_cx = fx.r(Mutbl::Not, cx_alias);
    let unit = fx.unit();

    let b_pool = fx.bound(pool_tr, &[p0]);
    let use_cx = fx.func("use_cx", 1, &[ref_cx], unit, vec![b_pool]);
    let owner = fx.func("run", 1, &[p0], unit, vec![b_pool]);

    let pool_args = fx.list(&[p0]);
    let ex_pool = fx.ty(PNode::ExTrait(pool_tr, pool_args));
    fx.finish();

    let mut decls = Decls::new();
    decls.trait_(pool_tr, 1);
    decls.alias(pool_tr, "Cx", 1);
    // `fn use_cx<'a, 'r, P: Pool<'a>>(cx: &'r <P as Pool<'a>>::Cx)`
    decls.func(use_cx, FnHead { lifetimes: 2, late: &[1], outlives: &[], inputs: &[ref_cx], output: unit, slots: &[P1, P0] });
    decls.clause(use_cx, 0, 0, ex_pool, &[P0]);
    // `fn run<'a, P: Pool<'a>>(p: P)`
    decls.func(owner, FnHead { lifetimes: 1, late: &[], outlives: &[], inputs: &[p0], output: unit, slots: &[] });
    decls.clause(owner, 0, 0, ex_pool, &[P0]);

    let mut facts = UnsizeFacts::new();
    facts.traits.push((pool_tr, 1));
    facts.aliases.push((pool_tr, Name::of("Cx"), 1));

    let run_case = |mark_named: bool| {
        let mut b = Bb::new();
        let _pp = b.param("p");
        let pcx = b.bind("cx");
        let p0_b = b.ty(PNode::Param(0));
        let f = b.path_args("use_cx", &[p0_b]);
        let lcx = b.local(pcx);
        let call = b.call(f, &[lcx]);
        let cl = b.closure(&[pcx], call);
        let pwork = b.bind("work");
        let st1 = b.let_(pwork, None, cl);
        let root = b.block(&[st1], None);

        let mut view = b.view(owner, root);
        let mut ck = region_fixture::checked(&fx, &view);
        // Set closure param 0's type to `&<P as Pool>::Cx` (matching an explicit `|cx: &P::Cx|`)
        // and `SigFrom::Written` so `sig_slots` calls `lay_written_param`.
        let p0_body = ck.types.intern(PNode::Param(0)).expect("intern");
        let args_body = ck.types.intern_list(&[p0_body]).expect("intern");
        let alias_body = ck.types.intern(PNode::Alias { trait_: pool_tr, name: Name::of("Cx"), args: args_body }).expect("intern");
        let ref_cx_body = ck.types.intern(PNode::Ref(Mutbl::Not, alias_body)).expect("intern");
        let unit_body = ck.types.intern(PNode::Tuple(List::EMPTY)).expect("intern");
        let sig_list = ck.types.intern_list(&[ref_cx_body, unit_body]).expect("intern");
        let sig_ty = ck.types.intern(PNode::FnPtr(sig_list, blaze_typeck::FnPtrHeader::RUST)).expect("intern");
        let row = ck.closures.column::<0>().iter().position(|&c| c == cl).expect("closure row");
        ck.closures.column_mut::<2>()[row] = sig_ty;
        ck.closures.column_mut::<4>()[row] = blaze_typeck::SigFrom::Written;
        ck.local_ty[pcx.ix()] = ref_cx_body;
        ck.expr_ty[lcx.ix()] = ref_cx_body;

        let step0 = ClosureParamRegion::push_step(ClosureParamRegion::ROOT, 0).expect("step");
        let written = [ClosureParamRegion { closure: cl, param: 0, path: step0, own: 0, kind: WrittenRegion::Named }];
        if mark_named {
            view.closure_param_regions = &written;
        }

        let regions = decls.lay(fx.t.view(), &mut ck.types);
        let with = region_fixture::WithRegions { base: &facts, regions: &regions };
        let cf = lower_closure_external(&view, &ck, cl, &with, region_fixture::alloc()).expect("lower closure");
        let report = blaze_flow::check(&cf.lowered.flow, region_fixture::alloc()).expect("check");
        (cf, report)
    };

    // POSITIVE: `|cx: &P::Cx|` with `WrittenRegion::Named` -> alias slot is `SigOwner(0)` (not `Late`).
    let (cf_ok, rep_ok) = run_case(true);
    assert!(
        cf_ok.externals.iter().any(|&(_, w)| matches!(w, External::SigOwner(0))),
        "POSITIVE: named alias lifetime must lower to `External::SigOwner(0)`: {:?}",
        cf_ok.externals
    );
    // Any finding against `use_cx`'s `'a` must be a propagatable `UnprovenOutlives` between
    // `SigOwner(0)` and `OwnerParam(0)` (NOT `Invalidated` and NOT E0521 `Late` outlives!).
    for f in rep_ok.borrows.iter() {
        match *f {
            blaze_flow::BorrowFinding::UnprovenOutlives { from, into, .. } => {
                assert!(
                    requirement(&cf_ok.externals, from, into).is_some(),
                    "POSITIVE: outlives ({from:?} -> {into:?}) must propagate as owner Requirement, not E0521; externals={:?}",
                    cf_ok.externals
                );
            }
            other => panic!("POSITIVE: unexpected borrow finding inside `|cx: &P::Cx| use_cx(cx)`: {other:?}"),
        }
    }

    // NEGATIVE (soundness): without `WrittenRegion::Named` -> alias slot is `External::Late` ->
    // `use_cx` demands `Late: OwnerParam(0)` -> `requirement` is `None` (E0521)!
    let (cf_bad, rep_bad) = run_case(false);
    assert!(
        !cf_bad.externals.iter().any(|&(_, w)| matches!(w, External::SigOwner(_))),
        "NEGATIVE: elided alias lifetime must stay `External::Late`: {:?}",
        cf_bad.externals
    );
    assert!(
        rep_bad.borrows.iter().any(|f| matches!(
            *f,
            blaze_flow::BorrowFinding::UnprovenOutlives { from, into, .. }
                if requirement(&cf_bad.externals, from, into).is_none()
        )),
        "NEGATIVE: elided alias lifetime passed to `use_cx<'a>` MUST fail E0521 (unpropagatable Late outlives): borrows={:?} externals={:?}",
        rep_bad.borrows,
        cf_bad.externals
    );
}

/// `lower_closure_as_fn_once` (`crates/blaze-cli/src/store/take.rs:826` shape):
/// `bufs_chunk: &'w mut S` captured `ByValue` by a `move` closure that `blaze-typeck` inferred as
/// `ClosureKind::FnMut`:
/// - CONTROL (`lower_closure_external` as `FnMut`): `move || reborrow(bufs_chunk)` wraps
///   `bufs_chunk` in `&'env mut &'w mut S`, so returning `&'w mut S` fails with an unpropagatable
///   `External::Env` outlives error!
/// - POSITIVE (`lower_closure_as_fn_once`): the SAME `move || reborrow(bufs_chunk)` owns
///   `bufs_chunk` directly (`call_once(self)`), so reborrowing `bufs_chunk` has NO `External::Env`
///   and propagates cleanly as `Requirement { from: Capture(0), into: Sig(0) }`!
/// - NEGATIVE (soundness): `move || &bufs_chunk` (returning `&'r &'w mut S`, borrowing the owned
///   capture local itself!) lowered via `lower_closure_as_fn_once` MUST STILL BE REJECTED by
///   `blaze_flow::check` (borrow of local variable invalidated at `StorageDead`)!
#[test]
fn lower_closure_as_fn_once_owns_by_value_captures_and_rejects_returning_local_ref() {
    let mut fx = Fx::new();
    let s_k = fx.strukt("S", 0, &[]);
    let st = fx.adt(s_k, &[]);
    let rms = fx.r(Mutbl::Mut, st);
    let unit = fx.unit();
    let reborrow_fn = fx.func("reborrow", 0, &[rms], rms, vec![]);
    let owner = fx.func("take_lan", 0, &[rms], unit, vec![]);
    fx.finish();

    let mut decls = Decls::new();
    decls.func(reborrow_fn, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[rms], output: rms, slots: &[P0, P0] });
    decls.func(owner, FnHead { lifetimes: 1, late: &[], outlives: &[], inputs: &[rms], output: unit, slots: &[P0] });
    let facts = UnsizeFacts::new();

    // Case A: `move || reborrow(bufs_chunk)` (reborrows `&mut S` through `reborrow`), with
    // `ck.closures` marked `FnMut`.
    {
        let mut b = Bb::new();
        let pbuf = b.param("bufs_chunk");
        let f = b.path(&["reborrow"]);
        let lbuf = b.local(pbuf);
        let call = b.call(f, &[lbuf]);
        let cl = b.move_closure(&[], call);
        let pc = b.bind("c");
        let st1 = b.let_(pc, None, cl);
        let root = b.block(&[st1], None);

        let view = b.view(owner, root);
        let mut ck = region_fixture::checked(&fx, &view);
        let row = ck.closures.column::<0>().iter().position(|&c| c == cl).expect("closure row");
        // Simulate `blaze-typeck` inferring `FnMut` even though the consumer is `FnOnce`-only.
        ck.closures.column_mut::<1>()[row] = ClosureKind::FnMut;

        let regions = decls.lay(fx.t.view(), &mut ck.types);
        let with = region_fixture::WithRegions { base: &facts, regions: &regions };

        // CONTROL: lowered as `FnMut` -> `External::Env` blocks returning `reborrow(bufs_chunk)`.
        let cf_mut = lower_closure_external(&view, &ck, cl, &with, region_fixture::alloc()).expect("lower FnMut");
        let rep_mut = blaze_flow::check(&cf_mut.lowered.flow, region_fixture::alloc()).expect("check FnMut");
        assert!(
            cf_mut.externals.iter().any(|&(_, w)| w == External::Env),
            "CONTROL: `FnMut` must wrap `ByValue` capture in `External::Env`"
        );
        assert!(
            rep_mut.borrows.iter().any(|f| matches!(
                *f,
                blaze_flow::BorrowFinding::UnprovenOutlives { from, into, .. }
                    if requirement(&cf_mut.externals, from, into).is_none()
            )),
            "CONTROL: `FnMut` reborrow of `&mut` capture must fail with `External::Env` outlives: {:?}",
            rep_mut.borrows
        );

        // POSITIVE: lowered via `lower_closure_as_fn_once` -> no `External::Env`, clean owner req!
        let cf_once = lower_closure_as_fn_once(&view, &ck, cl, &with, region_fixture::alloc()).expect("lower FnOnce");
        let rep_once = blaze_flow::check(&cf_once.lowered.flow, region_fixture::alloc()).expect("check FnOnce");
        assert!(
            !cf_once.externals.iter().any(|&(_, w)| w == External::Env),
            "POSITIVE: `lower_closure_as_fn_once` must NOT wrap `ByValue` capture in `External::Env`"
        );
        assert!(
            rep_once.moves.is_empty(),
            "POSITIVE: `move || reborrow(bufs_chunk)` as `FnOnce` must have zero move errors: {:?}",
            rep_once.moves
        );
        for f in rep_once.borrows.iter() {
            match *f {
                blaze_flow::BorrowFinding::UnprovenOutlives { from, into, .. } => {
                    assert_eq!(
                        requirement(&cf_once.externals, from, into),
                        Some(Requirement { from: Need::Capture(0), into: Need::Sig(0) }),
                        "POSITIVE: only owner requirement should be `Capture(0): Sig(0)`, got ({from:?} -> {into:?})"
                    );
                }
                other => panic!("POSITIVE: unexpected borrow finding under `lower_closure_as_fn_once`: {other:?}"),
            }
        }
    }

    // Case B (NEGATIVE soundness): `move || &bufs_chunk` (returning `&'ret &'w mut S`, borrowing
    // the owned `bufs_chunk` local itself!) lowered via `lower_closure_as_fn_once` MUST BE REJECTED!
    {
        let mut b = Bb::new();
        let pbuf = b.param("bufs_chunk");
        let lbuf = b.local(pbuf);
        let bor_local = b.borrow(Mutbl::Not, lbuf);
        let cl = b.move_closure(&[], bor_local);
        let pc = b.bind("c");
        let st1 = b.let_(pc, None, cl);
        let root = b.block(&[st1], None);

        let view = b.view(owner, root);
        let mut ck = region_fixture::checked(&fx, &view);
        let row = ck.closures.column::<0>().iter().position(|&c| c == cl).expect("closure row");
        ck.closures.column_mut::<1>()[row] = ClosureKind::FnMut;

        let regions = decls.lay(fx.t.view(), &mut ck.types);
        let with = region_fixture::WithRegions { base: &facts, regions: &regions };
        let cf_bad = lower_closure_as_fn_once(&view, &ck, cl, &with, region_fixture::alloc()).expect("lower bad FnOnce");
        let rep_bad = blaze_flow::check(&cf_bad.lowered.flow, region_fixture::alloc()).expect("check bad FnOnce");
        assert!(
            rep_bad.borrows.iter().any(|f| matches!(f, blaze_flow::BorrowFinding::Invalidated { .. })),
            "NEGATIVE: `move || &bufs_chunk` under `lower_closure_as_fn_once` MUST be rejected as returning borrow of local capture! borrows={:?}",
            rep_bad.borrows
        );
    }
}

/// `crates/blaze-sim/src/check.rs::replace_hook` shape:
/// `let hook = MyBox::new(move |info: &Info<'_>| on_panic(&before, info)); set_hook(hook);`
/// where `before: MyBox<dyn for<'a, 'b> Fn(&'a Info<'b>) + 'static>` and `set_hook` expects
/// `MyBox<dyn for<'a, 'b> Fn(&'a Info<'b>) + 'static>`.
///
/// RFC 1214 outlives components for `dyn Tr + 'o`: `dyn Tr + 'o: 'r` iff `'o: 'r` (predicate slots
/// `1..` hold rigid `dyn_bound_placeholders` that must NOT be forced to outlive `'static`).
/// - POSITIVE: `before` has object lifetime `'static` -> passes cleanly with zero findings.
/// - NEGATIVE: `before` has non-`'static` object lifetime `'short` -> MUST fail borrow check
///   (`UnprovenOutlives`) because `'short: 'static` does not hold!
#[test]
fn unsize_box_closure_capturing_hrtb_dyn_fn_ignores_predicate_placeholders() {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, vec![], &[]);
    fx.lang(LangItem::Sized, sized);
    let unsize = fx.tr("Unsize", 2, vec![], &[]);
    fx.lang(LangItem::Unsize, unsize);
    let coerce_unsized = fx.tr("CoerceUnsized", 2, vec![], &[]);
    fx.lang(LangItem::CoerceUnsized, coerce_unsized);
    let fn_once = fx.tr("FnOnce", 2, vec![], &["Output"]);
    let fn_mut = fx.tr("FnMut", 2, vec![], &[]);
    let fn_tr = fx.tr("Fn", 2, vec![], &[]);
    fx.lang(LangItem::FnOnce, fn_once);
    fx.lang(LangItem::FnMut, fn_mut);
    fx.lang(LangItem::Fn, fn_tr);

    let info_k = fx.strukt("Info", 0, &[]);
    let info_ty = fx.adt(info_k, &[]);
    let ref_info = fx.r(Mutbl::Not, info_ty);
    let arg_tup = fx.tuple(&[ref_info]);
    let unit = fx.unit();

    let arg_list = fx.list(&[arg_tup]);
    let ex_fn = fx.ty(PNode::ExTrait(fn_tr, arg_list));
    let ex_out = fx.ty(PNode::ExProj { trait_: fn_once, name: Name::of("Output"), args: arg_list, term: unit });
    let dyn_preds = fx.list(&[ex_fn, ex_out]);
    let dyn_hook = fx.ty(PNode::Dynamic(dyn_preds));

    let p0 = fx.param(0);
    let p1 = fx.param(1);
    let box_k = fx.strukt("MyBox", 1, &[]);
    let box_p0 = fx.adt(box_k, &[p0]);
    let box_p1 = fx.adt(box_k, &[p1]);
    let box_dyn = fx.adt(box_k, &[dyn_hook]);
    let ref_box_dyn = fx.r(Mutbl::Not, box_dyn);

    let b_unsize = fx.bound(unsize, &[p0, p1]);
    let coerce_imp = fx.imp("impl<T, U> CoerceUnsized<MyBox<U>> for MyBox<T> where T: Unsize<U>", 2, box_p0, Some((coerce_unsized, &[box_p1])), &[], vec![b_unsize]);
    fx.unsized_param(coerce_imp, 0);
    fx.unsized_param(coerce_imp, 1);

    let box_new = fx.func("box_new", 1, &[p0], box_p0, vec![]);
    let on_panic = fx.func("on_panic", 0, &[ref_box_dyn, ref_info], unit, vec![]);
    let set_hook = fx.func("set_hook", 0, &[box_dyn], unit, vec![]);
    let fn_ok = fx.func("ok", 0, &[box_dyn], unit, vec![]);
    let fn_bad = fx.func("bad", 0, &[box_dyn], unit, vec![]);
    fx.finish();

    let late1_0 = Region::Late { binder: 1, var: 0 };
    let late1_1 = Region::Late { binder: 1, var: 1 };

    let mut decls = Decls::new();
    decls.adt(info_k, 1);
    decls.adt(box_k, 0);
    decls.trait_(fn_tr, 0);
    decls.trait_(fn_mut, 0);
    decls.trait_(fn_once, 0);
    decls.alias(fn_once, "Output", 0);

    // `box_new<T>(x: T) -> MyBox<T>` (0 lifetime params; generic T's slots pass through)
    decls.func(box_new, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[p0], output: box_p0, slots: &[] });
    // `on_panic<'r, 'o, 'i, 'b>(_: &'r MyBox<dyn for<'a, 'c> Fn(&'a Info<'c>) + 'o>, _: &'i Info<'b>)`
    decls.func(
        on_panic,
        FnHead {
            lifetimes: 4,
            late: &[0, 1, 2, 3],
            outlives: &[],
            inputs: &[ref_box_dyn, ref_info],
            output: unit,
            slots: &[P0, P1, late1_0, late1_1, late1_0, late1_1, P2, Region::Param(3)],
        },
    );
    // `set_hook(_: MyBox<dyn for<'a, 'b> Fn(&'a Info<'b>) + 'static>)`
    decls.func(
        set_hook,
        FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[box_dyn], output: unit, slots: &[Region::Static, late1_0, late1_1, late1_0, late1_1] },
    );
    // POSITIVE: `fn ok(before: MyBox<dyn for<'a, 'b> Fn(&'a Info<'b>) + 'static>)`
    decls.func(
        fn_ok,
        FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[box_dyn], output: unit, slots: &[Region::Static, late1_0, late1_1, late1_0, late1_1] },
    );
    // NEGATIVE: `fn bad<'short>(before: MyBox<dyn for<'a, 'b> Fn(&'a Info<'b>) + 'short>)`
    decls.func(
        fn_bad,
        FnHead { lifetimes: 1, late: &[], outlives: &[], inputs: &[box_dyn], output: unit, slots: &[P0, late1_0, late1_1, late1_0, late1_1] },
    );

    let mut facts = UnsizeFacts::new();
    facts.adts.push((info_k, 1, false));
    facts.adts.push((box_k, 0, false));
    facts.traits.push((fn_tr, 0));
    facts.traits.push((fn_mut, 0));
    facts.traits.push((fn_once, 0));
    facts.aliases.push((fn_once, Name::of("Output"), 0));
    facts.fn_traits = [Some(fn_tr), Some(fn_mut), Some(fn_once)];
    facts.closure_bodies_checked = true;

    let run = |owner: ItemKey| {
        let mut b = Bb::new();
        let p_before = b.param("before");
        let p_info = b.bind("info");
        let l_before = b.local(p_before);
        let r_before = b.borrow(Mutbl::Not, l_before);
        let l_info = b.local(p_info);
        let f_on = b.path(&["on_panic"]);
        let call_on = b.call(f_on, &[r_before, l_info]);
        let cl = b.move_closure(&[p_info], call_on);
        let f_new = b.path(&["box_new"]);
        let boxed = b.call(f_new, &[cl]);
        let f_set = b.path(&["set_hook"]);
        let call_set = b.call(f_set, &[boxed]);
        let st = b.semi(call_set);
        let root = b.block(&[st], None);
        let view = b.view(owner, root);
        let mut ck = region_fixture::checked(&fx, &view);
        region_fixture::lower(&fx, &view, &mut ck, &facts, &decls, Plant::None).expect("lower")
    };

    let ok = run(fn_ok);
    assert!(ok.lines.is_empty(), "POSITIVE: Box<Closure> capturing `Box<dyn for<'a,'b> Fn(&'a Info<'b>) + 'static>` must pass cleanly: {:?}", ok.lines);
    let bad = run(fn_bad);
    assert!(
        bad.lines.iter().any(|l| l.starts_with("borrow UnprovenOutlives")),
        "NEGATIVE: Box<Closure> capturing `Box<dyn Fn + 'short>` coerced to `+ 'static` MUST fail borrow check: {:?}",
        bad.lines
    );
}

/// `crates/blaze-cli/src/engine_check.rs::outlines_phase` shape:
/// `Pool<'a>::run(&self, f: &(dyn Fn(&Self::Cx, &mut Buf<'a>)))` invoked on `MyPool` where
/// `type Cx = Cx` (0 lifetimes, whereas `<Self as Pool<'a>>::Cx` has 1 lifetime slot `'a`!).
/// `find_unique_dyn_with_binder` must NOT reject the `dyn Fn` expectation because `want_slots`
/// (from the unnormalized alias) differs from the body `dyn Fn`'s normalized slot count.
/// - POSITIVE: `|cx: &Cx, buf: &mut Buf<'_>| take_buf_a(buf)` gets `SigSlot::Owner(0)` for `Buf<'_>`
///   and checks clean.
/// - NEGATIVE: `|cx: &Cx, buf: &mut Buf<'_>| take_cx_a(cx)` keeps `&Cx` as late-bound `SigSlot::Late`
///   and MUST fail borrow check when passed where `&'a Cx` is required!
#[test]
fn closure_expectation_matches_dyn_fn_with_associated_type_lifetime() {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, vec![], &[]);
    fx.lang(LangItem::Sized, sized);
    let unsize = fx.tr("Unsize", 2, vec![], &[]);
    fx.lang(LangItem::Unsize, unsize);
    let fn_once = fx.tr("FnOnce", 2, vec![], &["Output"]);
    let fn_mut = fx.tr("FnMut", 2, vec![], &[]);
    let fn_tr = fx.tr("Fn", 2, vec![], &[]);
    fx.lang(LangItem::FnOnce, fn_once);
    fx.lang(LangItem::FnMut, fn_mut);
    fx.lang(LangItem::Fn, fn_tr);

    let cx_k = fx.strukt("Cx", 0, &[]);
    let cx_ty = fx.adt(cx_k, &[]);
    let ref_cx = fx.r(Mutbl::Not, cx_ty);
    let buf_k = fx.strukt("Buf", 0, &[]);
    let buf_ty = fx.adt(buf_k, &[]);
    let mut_buf = fx.r(Mutbl::Mut, buf_ty);
    let pool_k = fx.strukt("MyPool", 0, &[]);
    let pool_ty = fx.adt(pool_k, &[]);
    let ref_pool = fx.r(Mutbl::Not, pool_ty);
    let unit = fx.unit();

    let pool_tr = fx.tr("Pool", 1, vec![], &["Cx"]);
    let p0 = fx.param(0);
    let self_cx = fx.alias(pool_tr, "Cx", &[p0]);
    let ref_self_cx = fx.r(Mutbl::Not, self_cx);
    let _pool_imp = fx.imp("impl<'a> Pool<'a> for MyPool { type Cx = Cx; }", 0, pool_ty, Some((pool_tr, &[])), &[("Cx", cx_ty)], vec![]);

    let arg_tup_decl = fx.tuple(&[ref_self_cx, mut_buf]);
    let arg_decl_list = fx.list(&[arg_tup_decl]);
    let ex_fn_decl = fx.ty(PNode::ExTrait(fn_tr, arg_decl_list));
    let ex_out_decl = fx.ty(PNode::ExProj { trait_: fn_once, name: Name::of("Output"), args: arg_decl_list, term: unit });
    let dyn_decl_preds = fx.list(&[ex_fn_decl, ex_out_decl]);
    let dyn_decl = fx.ty(PNode::Dynamic(dyn_decl_preds));
    let ref_dyn_decl = fx.r(Mutbl::Not, dyn_decl);

    let b_pool = fx.bound(pool_tr, &[p0]);
    let ref_p0 = fx.r(Mutbl::Not, p0);
    let run_fn = fx.func("run_pool", 1, &[ref_p0, ref_dyn_decl], unit, vec![b_pool]);
    let tie_buf_a = fx.func("tie_buf_a", 0, &[ref_pool, mut_buf], unit, vec![]);
    let tie_cx_a = fx.func("tie_cx_a", 0, &[ref_pool, ref_cx], unit, vec![]);
    let owner = fx.func("owner", 0, &[ref_pool], unit, vec![]);
    fx.finish();

    let late1_0 = Region::Late { binder: 1, var: 0 };
    let late1_1 = Region::Late { binder: 1, var: 1 };

    let mut decls = Decls::new();
    decls.adt(cx_k, 0);
    decls.adt(buf_k, 1);
    decls.adt(pool_k, 0);
    decls.trait_(pool_tr, 1);
    decls.alias(pool_tr, "Cx", 1);
    decls.trait_(fn_tr, 0);
    decls.trait_(fn_mut, 0);
    decls.trait_(fn_once, 0);
    decls.alias(fn_once, "Output", 0);

    // `run_pool<'a, 'r, 'o, P: Pool<'a>>(&'r P, &'r (dyn for<'c, 'b> Fn(&'c <P as Pool<'a>>::Cx, &'b mut Buf<'a>) + 'o))`
    // Note: `<P as Pool<'a>>::Cx` adds 1 slot (`P0`) in the declared sig (8 predicate slots inside `dyn`),
    // while in the body `Cx` has 0 slots (6 predicate slots inside `dyn`)!
    decls.func(
        run_fn,
        FnHead {
            lifetimes: 3,
            late: &[1, 2],
            outlives: &[],
            inputs: &[ref_p0, ref_dyn_decl],
            output: unit,
            slots: &[P1, P1, P2, late1_0, P0, late1_1, P0, late1_0, P0, late1_1, P0],
        },
    );
    // `tie_buf_a<'a, 'b>(_: &'a MyPool, _: &'b mut Buf<'a>)` (forces `Buf`'s lifetime to equal `'a`!)
    decls.func(tie_buf_a, FnHead { lifetimes: 2, late: &[1], outlives: &[], inputs: &[ref_pool, mut_buf], output: unit, slots: &[P0, P1, P0] });
    // `tie_cx_a<'a>(_: &'a MyPool, _: &'a Cx)` where `Cx` must outlive `'a` — wait: `&'a Cx` is
    // covariant so both inputs could shorten to a common shorter `'x` unless `'a` is invariant!
    // Use `fn tie_cx_static(_: &'static Cx)` for the negative so late-bound `&Cx` MUST fail `'late: 'static`.
    decls.func(tie_cx_a, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[ref_pool, ref_cx], output: unit, slots: &[P0, Region::Static] });
    decls.func(owner, FnHead { lifetimes: 1, late: &[], outlives: &[], inputs: &[ref_pool], output: unit, slots: &[P0] });

    let mut facts = UnsizeFacts::new();
    facts.adts.push((cx_k, 0, false));
    facts.adts.push((buf_k, 1, false));
    facts.adts.push((pool_k, 0, false));
    facts.traits.push((pool_tr, 1));
    facts.aliases.push((pool_tr, Name::of("Cx"), 1));
    facts.traits.push((fn_tr, 0));
    facts.traits.push((fn_mut, 0));
    facts.traits.push((fn_once, 0));
    facts.aliases.push((fn_once, Name::of("Output"), 0));
    facts.fn_traits = [Some(fn_tr), Some(fn_mut), Some(fn_once)];

    for use_cx_bad in [false, true] {
        let mut b = Bb::new();
        let p_pool = b.param("pool");
        let p_cx = b.bind("cx");
        let p_buf = b.bind("buf");
        let body = if use_cx_bad {
            let f = b.path(&["tie_cx_a"]);
            let lp = b.local(p_pool);
            let l = b.local(p_cx);
            b.call(f, &[lp, l])
        } else {
            let f = b.path(&["tie_buf_a"]);
            let lp = b.local(p_pool);
            let l = b.local(p_buf);
            b.call(f, &[lp, l])
        };
        let cl = b.closure(&[p_cx, p_buf], body);
        let r_cl = b.borrow(Mutbl::Not, cl);
        let l_pool = b.local(p_pool);
        let f_run = b.path(&["run_pool"]);
        let call_run = b.call(f_run, &[l_pool, r_cl]);
        let st = b.semi(call_run);
        let root = b.block(&[st], None);

        let view = b.view(owner, root);
        let mut ck = region_fixture::checked(&fx, &view);
        let regions = decls.lay(fx.t.view(), &mut ck.types);
        let with = region_fixture::WithRegions { base: &facts, regions: &regions };
        let cf = lower_closure_external(&view, &ck, cl, &with, region_fixture::alloc()).expect("lower closure");
        let rep = blaze_flow::check(&cf.lowered.flow, region_fixture::alloc()).expect("check");
        if use_cx_bad {
            assert!(
                rep.borrows.iter().any(|f| matches!(
                    *f,
                    blaze_flow::BorrowFinding::UnprovenOutlives { from, into, .. }
                        if requirement(&cf.externals, from, into).is_none()
                )),
                "NEGATIVE: passing late-bound `&Cx` as `&'static Cx` MUST fail borrow check inside closure: {:?}",
                rep.borrows
            );
        } else {
            assert!(
                cf.externals.iter().any(|&(_, w)| w == External::SigOwner(0)),
                "POSITIVE: `Buf<'_>` matched to `dyn Fn(&Self::Cx, &mut Buf<'a>)` must receive External::SigOwner(0): {:?}",
                cf.externals
            );
            assert!(
                rep.moves.is_empty()
                    && rep.borrows.iter().all(|f| matches!(
                        *f,
                        blaze_flow::BorrowFinding::UnprovenOutlives { from, into, .. }
                            if requirement(&cf.externals, from, into).is_some()
                    )),
                "POSITIVE: `&mut Buf<'_>` tied to `pool: &'a MyPool` must have zero un-exportable borrow errors: moves={:?} borrows={:?} externals={:?}",
                rep.moves,
                rep.borrows,
                cf.externals
            );
        }
    }
}

/// `crates/blaze-cli/src/test_drive.rs::ask` shape:
/// `Pin<Box<async move { mk_out(runs, heap) }>>` coerced to `Pin<Box<dyn Future<Output = Out<'static>> + 'b>>`
/// where `runs: &'b R` and `heap: &'static H`.
/// - POSITIVE: `async move` captures `(&'b R, &'static H)` and produces `Out<'static>` ->
///   passes cleanly with `coarse.is_none()` (no `ValueInexact` and no false `'b: 'static`!).
/// - NEGATIVE 1: `async move` produces `Out<'b>` (output tied to `runs`) coerced to `Output = Out<'static>` ->
///   MUST fail borrow check (`UnprovenOutlives`).
/// - NEGATIVE 2: `async move` capturing `runs: &'b R` coerced to `dyn Future<Output = Out<'static>> + 'static` ->
///   MUST fail borrow check (`UnprovenOutlives` for `'b: 'static`).
#[test]
fn unsize_pin_box_async_move_separates_captures_from_output() {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, vec![], &[]);
    fx.lang(LangItem::Sized, sized);
    let unsize = fx.tr("Unsize", 2, vec![], &[]);
    fx.lang(LangItem::Unsize, unsize);
    let coerce_unsized = fx.tr("CoerceUnsized", 2, vec![], &[]);
    fx.lang(LangItem::CoerceUnsized, coerce_unsized);
    let fut_tr = fx.tr("Future", 1, vec![], &["Output"]);
    fx.lang(LangItem::Future, fut_tr);

    let r_k = fx.strukt("R", 0, &[]);
    let h_k = fx.strukt("H", 0, &[]);
    let out_k = fx.strukt("Out", 0, &[]);
    let r_ty = fx.adt(r_k, &[]);
    let h_ty = fx.adt(h_k, &[]);
    let out_ty = fx.adt(out_k, &[]);
    let ref_r = fx.r(Mutbl::Not, r_ty);
    let ref_h = fx.r(Mutbl::Not, h_ty);

    let p0 = fx.param(0);
    let p1 = fx.param(1);
    let box_k = fx.strukt("MyBox", 1, &[]);
    let pin_k = fx.strukt("MyPin", 1, &[]);
    let box_p0 = fx.adt(box_k, &[p0]);
    let box_p1 = fx.adt(box_k, &[p1]);
    let pin_p0 = fx.adt(pin_k, &[p0]);
    let pin_p1 = fx.adt(pin_k, &[p1]);

    let b_unsize = fx.bound(unsize, &[p0, p1]);
    let box_coerce = fx.imp("impl<T, U> CoerceUnsized<MyBox<U>> for MyBox<T> where T: Unsize<U>", 2, box_p0, Some((coerce_unsized, &[box_p1])), &[], vec![b_unsize]);
    fx.unsized_param(box_coerce, 0);
    fx.unsized_param(box_coerce, 1);
    let b_coerce = fx.bound(coerce_unsized, &[p0, p1]);
    let _pin_coerce = fx.imp("impl<P, Q> CoerceUnsized<MyPin<Q>> for MyPin<P> where P: CoerceUnsized<Q>", 2, pin_p0, Some((coerce_unsized, &[pin_p1])), &[], vec![b_coerce]);

    let ex_fut = fx.ty(PNode::ExTrait(fut_tr, List::EMPTY));
    let ex_out = fx.ty(PNode::ExProj { trait_: fut_tr, name: Name::of("Output"), args: List::EMPTY, term: out_ty });
    let dyn_fut_preds = fx.list(&[ex_fut, ex_out]);
    let dyn_fut = fx.ty(PNode::Dynamic(dyn_fut_preds));
    let box_dyn_fut = fx.adt(box_k, &[dyn_fut]);
    let pin_box_dyn = fx.adt(pin_k, &[box_dyn_fut]);
    let pin_box_p0 = fx.adt(pin_k, &[box_p0]);

    let pin_box = fx.func("pin_box", 1, &[p0], pin_box_p0, vec![]);
    let mk_out_static = fx.func("mk_out_static", 0, &[ref_r, ref_h], out_ty, vec![]);
    let mk_out_from_r = fx.func("mk_out_from_r", 0, &[ref_r, ref_h], out_ty, vec![]);
    let fn_ok = fx.func("ask_ok", 0, &[ref_r, ref_h], pin_box_dyn, vec![]);
    let fn_bad_out = fx.func("ask_bad_out", 0, &[ref_r, ref_h], pin_box_dyn, vec![]);
    let fn_bad_obj = fx.func("ask_bad_obj", 0, &[ref_r, ref_h], pin_box_dyn, vec![]);
    fx.finish();

    let mut decls = Decls::new();
    decls.adt(r_k, 0);
    decls.adt(h_k, 0);
    decls.adt(out_k, 1);
    decls.adt(box_k, 0);
    decls.adt(pin_k, 0);
    decls.trait_(fut_tr, 0);
    decls.alias(fut_tr, "Output", 0);

    decls.func(pin_box, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[p0], output: pin_box_p0, slots: &[] });
    // `mk_out_static<'b, 'h>(_: &'b R, _: &'h H) -> Out<'h>`
    decls.func(mk_out_static, FnHead { lifetimes: 2, late: &[0, 1], outlives: &[], inputs: &[ref_r, ref_h], output: out_ty, slots: &[P0, P1, P1] });
    // `mk_out_from_r<'b, 'h>(_: &'b R, _: &'h H) -> Out<'b>`
    decls.func(mk_out_from_r, FnHead { lifetimes: 2, late: &[0, 1], outlives: &[], inputs: &[ref_r, ref_h], output: out_ty, slots: &[P0, P1, P0] });
    // `ask_ok<'b>(runs: &'b R, heap: &'static H) -> MyPin<MyBox<dyn Future<Output = Out<'static>> + 'b>>`
    decls.func(fn_ok, FnHead { lifetimes: 1, late: &[], outlives: &[], inputs: &[ref_r, ref_h], output: pin_box_dyn, slots: &[P0, Region::Static, P0, Region::Static] });
    // `ask_bad_out<'b>(runs: &'b R, heap: &'static H) -> MyPin<MyBox<dyn Future<Output = Out<'static>> + 'b>>`
    decls.func(fn_bad_out, FnHead { lifetimes: 1, late: &[], outlives: &[], inputs: &[ref_r, ref_h], output: pin_box_dyn, slots: &[P0, Region::Static, P0, Region::Static] });
    // `ask_bad_obj<'b>(runs: &'b R, heap: &'static H) -> MyPin<MyBox<dyn Future<Output = Out<'static>> + 'static>>`
    decls.func(fn_bad_obj, FnHead { lifetimes: 1, late: &[], outlives: &[], inputs: &[ref_r, ref_h], output: pin_box_dyn, slots: &[P0, Region::Static, Region::Static, Region::Static] });

    let run = |owner: ItemKey, tie_out_to_runs: bool| {
        let mut b = Bb::new();
        let p_r = b.param("runs");
        let p_h = b.param("heap");
        let l_r = b.local(p_r);
        let l_h = b.local(p_h);
        let f_mk = b.path(&[if tie_out_to_runs { "mk_out_from_r" } else { "mk_out_static" }]);
        let call_mk = b.call(f_mk, &[l_r, l_h]);
        let ab = b.async_block(call_mk, blaze_typeck::Capture::Move);
        let f_pb = b.path(&["pin_box"]);
        let pinned = b.call(f_pb, &[ab]);
        let root = b.block(&[], Some(pinned));

        let view = b.view(owner, root);
        let mut ck = region_fixture::checked(&fx, &view);
        let regions = decls.lay(fx.t.view(), &mut ck.types);

        let mut facts = UnsizeFacts::new();
        facts.adts.push((r_k, 0, false));
        facts.adts.push((h_k, 0, false));
        facts.adts.push((out_k, 1, false));
        facts.adts.push((box_k, 0, false));
        facts.adts.push((pin_k, 0, false));
        facts.traits.push((fut_tr, 0));
        facts.aliases.push((fut_tr, Name::of("Output"), 0));
        facts.future_trait = Some(fut_tr);
        facts.async_bodies_checked = true;
        // First lower the async block itself to harvest its exact `Capture(k) -> Sig(0)` requirements,
        // mirroring `front2/borrows_nested.rs`!
        let with0 = region_fixture::WithRegions { base: &facts, regions: &regions };
        let af = crate::lower_async(&view, &ck, ab, &with0, region_fixture::alloc()).expect("lower async");
        let arep = blaze_flow::check(&af.flow, region_fixture::alloc()).expect("check async");
        let mut reqs = vec::Vec::new();
        for f in arep.borrows.iter() {
            if let blaze_flow::BorrowFinding::UnprovenOutlives { from, into, .. } = *f {
                if let Some(req) = requirement(&af.externals, from, into) {
                    reqs.push(req);
                }
            }
        }
        facts.closure_reqs.push((ab, reqs));
        let with = region_fixture::WithRegions { base: &facts, regions: &regions };
        let lowered = crate::lower(&view, &ck, &with, region_fixture::alloc()).expect("lower owner");
        let rep = blaze_flow::check(&lowered.flow, region_fixture::alloc()).expect("check owner");
        (lowered.coarse, rep.borrows.len(), rep.moves.len())
    };

    let (ok_coarse, ok_b, ok_m) = run(fn_ok, false);
    assert!(ok_coarse.is_none(), "POSITIVE: Pin<Box<async move>> must not record coarse ValueInexact: {ok_coarse:?}");
    assert_eq!((ok_b, ok_m), (0, 0), "POSITIVE: `ask_ok` must pass cleanly without false `'b: 'static`");

    let (_, bad_out_b, _) = run(fn_bad_out, true);
    assert!(bad_out_b > 0, "NEGATIVE 1: async move returning `Out<'b>` as `Output = Out<'static>` MUST fail borrow check");

    let (_, bad_obj_b, _) = run(fn_bad_obj, false);
    assert!(bad_obj_b > 0, "NEGATIVE 2: async move capturing `&'b R` as `+ 'static` MUST fail borrow check");
}
