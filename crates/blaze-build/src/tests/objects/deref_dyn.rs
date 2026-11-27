//! Dynamic vtable dispatch in `Walk::deref_call` and `Walk::object_reads`:
//! - `trait Sub: Deref<Target = u64>` called via `**r` on `r: &dyn Sub` dispatches through the
//!   vtable slot for `Deref::deref` and runs natively to return the target value.
//! - `crate::object_reads` records `ObjectRead::Call { principal, method: deref, slot }` for an
//!   overloaded deref on `dyn Sub`, and moving `deref`'s slot rekeys the caller.
//! - `#[track_caller]` on `Deref::deref` is refused by `refuse_tracked_slot` when called through
//!   `dyn Sub`.

use blaze_ids::ContentId;
use blaze_own::Alloc;
use blaze_typeck::fixture::Bb;
use blaze_typeck::{Adt, Expr, ExprId, InferCtx, ItemKey, LangItem, Mutbl, Name, Needs, Outcome, PNode, PTy, PatId, PortableTys, UnOp};

use super::super::{Tables, run_native_vt};
use super::{Global, World, ground_of, world};
use crate::layout::{LayoutFacts, Missing, ReprAttrs, ValidRange};
use crate::vtable::{MethodDispatch, ObjectMethod, ObjectRead, ObjectSuper, ObjectTrait};
use crate::{Abi, Adts, BuildError, Built, Instance, Intrinsic, Subst};

struct DerefAdts<'a> {
    w: &'a World,
    lits: &'a [(ExprId, u128)],
    deref_fn: ItemKey,
    deref_mut_fn: ItemKey,
    tracked_deref: bool,
}

impl LayoutFacts for DerefAdts<'_> {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        self.w.fx.adt_of(key)
    }

    fn repr(&self, _: ItemKey) -> Result<ReprAttrs, Missing> {
        Ok(ReprAttrs::RUST)
    }

    fn discriminant(&self, _: ItemKey, _: u32) -> Result<Option<i128>, Missing> {
        Ok(None)
    }

    fn valid_range(&self, _: ItemKey) -> Result<ValidRange, Missing> {
        Ok(ValidRange::default())
    }
}

impl Adts for DerefAdts<'_> {
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        self.w.fx.adt_of(key)
    }

    fn overflow_checks(&self) -> crate::OverflowChecks {
        crate::OverflowChecks::Off
    }

    fn ub_checks(&self) -> crate::UbChecks {
        crate::UbChecks::Off
    }

    fn facts(&self) -> &dyn LayoutFacts {
        self
    }

    fn int_lit(&self, _: ItemKey, expr: ExprId) -> Option<u128> {
        self.lits.iter().find(|(e, _)| *e == expr).map(|&(_, v)| v)
    }

    fn int_pat(&self, _: ItemKey, _: PatId) -> Option<u128> {
        None
    }

    fn intrinsic(&self, item: ItemKey) -> Option<Intrinsic> {
        self.w.intrinsics.iter().find(|(k, _)| *k == item).map(|&(_, i)| i)
    }

    fn abi(&self, item: ItemKey) -> Option<Abi> {
        self.w.fx.sig_of(item).map(|_| Abi::Rust)
    }

    fn drop_impl(&self, _: PTy, _: &mut PortableTys<'_>) -> Result<Option<ItemKey>, BuildError> {
        Ok(None)
    }

    fn deref_fn(&self, mutbl: Mutbl) -> Option<ItemKey> {
        Some(match mutbl {
            Mutbl::Not => self.deref_fn,
            Mutbl::Mut => self.deref_mut_fn,
        })
    }

    fn lang_fn(&self, name: &str) -> Option<ItemKey> {
        (name == "drop_in_place").then_some(self.w.boxes.dip)
    }

    fn lang_adt(&self, item: LangItem) -> Option<ItemKey> {
        (item == LangItem::OwnedBox).then_some(self.w.boxes.box_k)
    }

    fn object_trait(&self, t: ItemKey) -> Option<ObjectTrait<'_>> {
        self.w.object_trait(t)
    }

    fn track_caller(&self, instance: Instance, _: &mut PortableTys<'_>) -> Result<Option<bool>, BuildError> {
        if self.tracked_deref && instance.item == self.deref_fn {
            Ok(Some(true))
        } else {
            Ok(Some(false))
        }
    }
}

struct DerefWorld {
    w: World,
    deref_tr: ItemKey,
    deref_fn: ItemKey,
    deref_mut_fn: ItemKey,
    sub_deref: ItemKey,
    wrap_k: ItemKey,
    wrap_ty: PTy,
    wrap_deref_impl: ItemKey,
}

fn deref_world() -> DerefWorld {
    let mut w = world();
    let fx = &mut w.fx;
    let deref_tr = fx.key_of("Deref");
    let p0 = fx.param(0);
    let r_self = fx.r(Mutbl::Not, p0);
    let p0_list = fx.t.intern_list(&[p0]).expect("list");
    let target_proj = fx.t.intern(PNode::Alias {
        trait_: deref_tr,
        args: p0_list,
        name: Name::of("Target"),
    }).expect("proj");
    let r_target = fx.r(Mutbl::Not, target_proj);
    let deref_fn = fx.trait_fn(deref_tr, "deref", true, &[r_self], r_target);
    let deref_bound = fx.bound(deref_tr, &[p0]);
    let deref_mut_tr = fx.tr("DerefMut", 1, [deref_bound].into(), &[]);
    fx.lang(LangItem::DerefMut, deref_mut_tr);
    let mr_self = fx.r(Mutbl::Mut, p0);
    let mr_target = fx.r(Mutbl::Mut, target_proj);
    let deref_mut_fn = fx.trait_fn(deref_mut_tr, "deref_mut", true, &[mr_self], mr_target);

    let u64_ = w.u64_;
    let deref_u64 = fx.proj(deref_tr, &[p0], "Target", u64_);
    let sub_deref = fx.tr("SubDeref", 1, [deref_bound, deref_u64].into(), &[]);

    let wrap_k = fx.strukt("Wrap", 0, &[("n", u64_)]);
    let wrap_ty = fx.adt(wrap_k, &[]);
    let r_wrap = fx.r(Mutbl::Not, wrap_ty);
    let r_u64 = fx.r(Mutbl::Not, u64_);
    let imp_d = fx.imp("deref_wrap", 0, wrap_ty, Some((deref_tr, &[])), &[("Target", u64_)], [].into());
    let wrap_deref_impl = fx.impl_fn(imp_d, "deref", true, &[r_wrap], r_u64);
    fx.imp("sub_deref_wrap", 0, wrap_ty, Some((sub_deref, &[])), &[], [].into());
    fx.finish();

    let method = |item| ObjectMethod { item, dispatch: MethodDispatch::Direct };
    w.traits.push((deref_tr, [].into(), [method(deref_fn)].into()));
    w.traits.push((deref_mut_tr, [ObjectSuper::plain(deref_tr)].into(), [method(deref_mut_fn)].into()));
    w.traits.push((sub_deref, [ObjectSuper::plain(deref_tr)].into(), [].into()));
    DerefWorld { w, deref_tr, deref_fn, deref_mut_fn, sub_deref, wrap_k, wrap_ty, wrap_deref_impl }
}

fn star(b: &mut Bb, operand: ExprId) -> ExprId {
    let at = b.int(None);
    b.exprs[at.ix()] = Expr::Unary { op: UnOp::Deref, operand };
    at
}

fn field(b: &mut Bb, base: ExprId, name: &str) -> ExprId {
    let at = b.int(None);
    b.exprs[at.ix()] = Expr::Field { base, name: Name::of(name) };
    at
}

/// Builds `let x = Wrap { n: 77 }; let r: &dyn SubDeref = &x; **r` as `main`.
fn build_sub_deref_main(dw: &DerefWorld) -> (Bb, ExprId, [(ExprId, u128); 1]) {
    let mut b = Bb::new();
    let x = b.bind("x");
    let lit = b.int(None);
    let wrap_init = b.struct_lit(&["Wrap"], &[("n", lit)], None);
    let s0 = b.let_(x, None, wrap_init);
    let r = b.bind("r");
    let u64_ty = b.ty(PNode::Uint(blaze_typeck::UintTy::U64));
    let ex = b.ty(PNode::ExTrait(dw.sub_deref, blaze_typeck::List::EMPTY));
    let ex_proj = b.ty(PNode::ExProj {
        trait_: dw.deref_tr,
        name: Name::of("Target"),
        args: blaze_typeck::List::EMPTY,
        term: u64_ty,
    });
    let dyn_sub = b.t.dynamic(&[ex, ex_proj]).expect("dyn");
    let r_ty = b.ty(PNode::Ref(Mutbl::Not, dyn_sub));
    let xe = b.local(x);
    let borrow = b.borrow(Mutbl::Not, xe);
    let s1 = b.let_(r, Some(r_ty), borrow);
    let re = b.local(r);
    let d1 = star(&mut b, re);
    let d2 = star(&mut b, d1);
    let root = b.block(&[s0, s1], Some(d2));
    (b, root, [(lit, 77)])
}

/// Dynamic `Deref::deref` through `&dyn SubDeref`:
/// 1. Executes natively and returns `77` through the vtable slot.
/// 2. `object_reads` records `ObjectRead::Call { principal: SubDeref, method: Deref::deref, slot: 0 }`.
/// 3. Inserting a method before `Deref::deref` moves the slot from 0 to 1 and rekeys `main`.
#[test]
fn dyn_deref_dispatches_via_vtable_and_records_object_read() {
    let mut dw = deref_world();
    let mut ground = PortableTys::new_in(Alloc::global());

    // Build `impl Deref for Wrap { fn deref(&self) -> &u64 { &self.n } }`.
    let mut ib = Bb::new();
    let s = ib.param("self");
    let se = ib.local(s);
    let fld = field(&mut ib, se, "n");
    let ref_n = ib.borrow(Mutbl::Not, fld);
    let iroot = ib.block(&[], Some(ref_n));
    let iview = ib.view(dw.wrap_deref_impl, iroot);
    let mut ity = InferCtx::new_in(Alloc::global());
    let Ok(Outcome::Done(ichecked)) = blaze_typeck::check(&iview, &dw.w.fx, &mut ity, &Needs::new_in(Alloc::global()), Alloc::global()) else {
        panic!("the check of wrap_deref_impl failed");
    };
    assert!(ichecked.diags.is_empty(), "diagnostics: {:?}", ichecked.diags);
    let isig = dw.w.fx.sig_of(dw.wrap_deref_impl).expect("impl sig");
    let iadts = DerefAdts { w: &dw.w, lits: &[], deref_fn: dw.deref_fn, deref_mut_fn: dw.deref_mut_fn, tracked_deref: false };
    let mut isubst = Subst { args: &[], ground: &mut ground, scratch: Alloc::global() };
    let impl_built: Built<Global> = crate::build(&iview, &ichecked, &isig, &mut isubst, &iadts, Global).expect("build impl");
    super::super::emits(&impl_built, &[]);

    let (b, root, lits) = build_sub_deref_main(&dw);
    let view = b.view(dw.w.main, root);
    let mut ty = InferCtx::new_in(Alloc::global());
    let Ok(Outcome::Done(checked)) = blaze_typeck::check(&view, &dw.w.fx, &mut ty, &Needs::new_in(Alloc::global()), Alloc::global()) else {
        panic!("the check of main failed");
    };
    assert!(checked.diags.is_empty(), "diagnostics: {:?}", checked.diags);
    let sig = dw.w.fx.sig_of(dw.w.main).expect("main sig");
    let adts = DerefAdts { w: &dw.w, lits: &lits, deref_fn: dw.deref_fn, deref_mut_fn: dw.deref_mut_fn, tracked_deref: false };

    let mut rsubst = Subst { args: &[], ground: &mut ground, scratch: Alloc::global() };
    let reads_before = crate::object_reads(&view, &checked, &mut rsubst, &adts, Global).expect("object_reads before");
    assert_eq!(
        reads_before.as_slice(),
        &[ObjectRead::Call { principal: dw.sub_deref, method: dw.deref_fn, slot: 0 }],
        "overloaded deref on dyn SubDeref must record its vtable slot read"
    );
    let code = ContentId::of("deref_dyn test code", &dw.w.main.0.to_le_bytes());
    let inst_key = crate::instance_key(code, blaze_typeck::List::EMPTY, ground.view(), Alloc::global()).expect("instance key");
    let key_before = crate::vtable::record_key(inst_key, &reads_before);

    let mut msubst = Subst { args: &[], ground: &mut ground, scratch: Alloc::global() };
    let main_built = crate::build(&view, &checked, &sig, &mut msubst, &adts, Global).expect("build main");
    super::super::emits(&main_built, &[]);
    assert!(main_built.refs.is_empty(), "dynamic deref must not emit a direct fn ref to Deref::deref");

    let gwrap = ground_of(&dw.w.fx, &mut ground, dw.wrap_ty);
    let impls = [(dw.deref_fn, gwrap, dw.wrap_deref_impl)];
    let sizes = [(gwrap, 8, 8, None)];
    let facts = |t| dw.w.object_trait(t);
    let mut tables = Tables { facts: &facts, impls: &impls, sizes: &sizes, ground: &mut ground, placed: Default::default() };
    let got = run_native_vt(&[], &[(dw.wrap_deref_impl, None, &impl_built), (dw.w.main, None, &main_built)], Some(&mut tables));
    assert_eq!(got, 77, "dynamic Deref::deref through &dyn SubDeref must return Wrap.n");

    // Moving `Deref::deref` from slot 0 to slot 1 must update `object_reads` and rekey `main`.
    let dummy = dw.w.boxes.get;
    let methods = &mut dw.w.traits.iter_mut().find(|(k, _, _)| *k == dw.deref_tr).expect("Deref").2;
    methods.insert(0, ObjectMethod { item: dummy, dispatch: MethodDispatch::Direct });
    let adts_after = DerefAdts { w: &dw.w, lits: &lits, deref_fn: dw.deref_fn, deref_mut_fn: dw.deref_mut_fn, tracked_deref: false };
    let mut rsubst2 = Subst { args: &[], ground: &mut ground, scratch: Alloc::global() };
    let reads_after = crate::object_reads(&view, &checked, &mut rsubst2, &adts_after, Global).expect("object_reads after");
    assert_eq!(
        reads_after.as_slice(),
        &[ObjectRead::Call { principal: dw.sub_deref, method: dw.deref_fn, slot: 1 }],
        "slot shift must be reflected in object_reads"
    );
    let key_after = crate::vtable::record_key(inst_key, &reads_after);
    assert_ne!(key_before, key_after, "moving Deref::deref's slot must rekey the caller");
    let _ = dw.wrap_k;
}

/// A `#[track_caller]` `Deref::deref` dispatched through a vtable is refused by
/// `refuse_tracked_slot`, matching `Walk::call`.
#[test]
fn tracked_dyn_deref_slot_is_refused() {
    let dw = deref_world();
    let mut ground = PortableTys::new_in(Alloc::global());
    let (b, root, lits) = build_sub_deref_main(&dw);
    let view = b.view(dw.w.main, root);
    let mut ty = InferCtx::new_in(Alloc::global());
    let Ok(Outcome::Done(checked)) = blaze_typeck::check(&view, &dw.w.fx, &mut ty, &Needs::new_in(Alloc::global()), Alloc::global()) else {
        panic!("the check of main failed");
    };
    let sig = dw.w.fx.sig_of(dw.w.main).expect("main sig");
    let adts = DerefAdts { w: &dw.w, lits: &lits, deref_fn: dw.deref_fn, deref_mut_fn: dw.deref_mut_fn, tracked_deref: true };
    let mut subst = Subst { args: &[], ground: &mut ground, scratch: Alloc::global() };
    match crate::build(&view, &checked, &sig, &mut subst, &adts, Global) {
        Err(BuildError::TrackCaller { item, .. }) => assert_eq!(item, dw.deref_fn),
        Err(other) => panic!("expected BuildError::TrackCaller, got {other}"),
        Ok(_) => panic!("tracked Deref::deref on dyn SubDeref must be refused"),
    }
}
