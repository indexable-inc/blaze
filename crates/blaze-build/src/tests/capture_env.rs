//! A closure environment's LAYOUT alone, under each strategy of the seam (blaze-typeck
//! env_fields.rs `WholeBindings` / `Places`; layout.rs `environment_by`). The Reference,
//! "Closure types": since Rust 2021 a closure captures PLACES (RFC 2229), one field per place,
//! in capture order; a by-reference field is a reference to the place, a by-value field the
//! place's own value. Each rule has its twin (rule 23): the same rows under `WholeBindings`
//! lay one field per binding, the whole binding's type.
//!
//! NOT YET RUN (written 2026-11-21 by a write-only lane).

use blaze_own::Alloc;
use blaze_typeck::{CaptureKind, Checked, ExprId, ItemKey, Mutbl, PNode, PTy, PatId, Places, PortableTys, UintTy, WholeBindings};

use crate::layout::environment_by;
use crate::walk::{empty_record_for_tests as empty_record, empty_view_for_tests as empty_view};

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn ty(t: &mut PortableTys<'_>, n: PNode) -> PTy {
    t.intern(n).unwrap_or_else(|e| panic!("intern {n:?}: {e:?}"))
}

fn tuple(t: &mut PortableTys<'_>, items: &[PTy]) -> PTy {
    let l = t.intern_list(items).unwrap_or_else(|e| panic!("intern a list: {e:?}"));
    ty(t, PNode::Tuple(l))
}

/// The environment of `e` in `c` under both strategies, interned in one table so equal types
/// are equal ids: `(whole, places)`.
fn both(c: &Checked<'_>, e: ExprId, out: &mut PortableTys<'_>) -> (PTy, PTy) {
    let body = empty_view(ItemKey(1), c.types.view());
    let whole = environment_by::<WholeBindings>(&body, c, e, &[], out, alloc()).unwrap_or_else(|err| panic!("whole bindings: {err}"));
    let places = environment_by::<Places>(&body, c, e, &[], out, alloc()).unwrap_or_else(|err| panic!("places: {err}"));
    (whole, places)
}

/// `let x: (u64, u32); let c = || { let _ = &x.0; let y = x.1; };`: two places of one binding,
/// `x.0` by reference and `x.1` by value (a `u32` read is a copy, so rustc captures it by
/// reference; the rows here are written by hand to hold each mode once). The environment is
/// `(&u64, u32)`, in the analysis's order, one field per place. TWIN: the same closure's
/// whole-binding row lays `((u64, u32),)`.
#[test]
fn one_field_per_place_in_analysis_order() {
    let a = alloc();
    let mut c = empty_record(a);
    let u64t = ty(&mut c.types, PNode::Uint(UintTy::U64));
    let u32t = ty(&mut c.types, PNode::Uint(UintTy::U32));
    let pair = tuple(&mut c.types, &[u64t, u32t]);
    let ref_u64 = ty(&mut c.types, PNode::Ref(Mutbl::Not, u64t));
    c.local_ty.push(pair);
    let (x, e) = (PatId(0), ExprId(0));
    c.captures.push((e, x, CaptureKind::ByValue));
    c.capture_projs.push((1, 0, 0, u64t));
    c.capture_projs.push((1, 0, 1, u32t));
    c.capture_places.push((e, x, 0, 1, CaptureKind::ByRef, ref_u64));
    c.capture_places.push((e, x, 1, 1, CaptureKind::ByValue, u32t));
    let mut out = PortableTys::new_in(a);
    let (whole, places) = both(&c, e, &mut out);
    let o_u64 = ty(&mut out, PNode::Uint(UintTy::U64));
    let o_u32 = ty(&mut out, PNode::Uint(UintTy::U32));
    let o_ref = ty(&mut out, PNode::Ref(Mutbl::Not, o_u64));
    let want = tuple(&mut out, &[o_ref, o_u32]);
    assert_eq!(places, want, "one field per place, by-reference `&u64` then by-value `u32`: {:?}", out.view().node(places));
    let o_pair = tuple(&mut out, &[o_u64, o_u32]);
    let twin = tuple(&mut out, &[o_pair]);
    assert_eq!(whole, twin, "TWIN: one field per binding, the whole `(u64, u32)` moved: {:?}", out.view().node(whole));
}

/// `|| r.0` with `r: &(u64, u32)`: the place `(*r).0` through a reference, captured by
/// reference: its field is `&u64`, a reference to the place, not to `r`. TWIN: whole binding
/// `r` by reference is `&&(u64, u32)`.
#[test]
fn a_by_reference_field_points_at_the_place() {
    let a = alloc();
    let mut c = empty_record(a);
    let u64t = ty(&mut c.types, PNode::Uint(UintTy::U64));
    let u32t = ty(&mut c.types, PNode::Uint(UintTy::U32));
    let pair = tuple(&mut c.types, &[u64t, u32t]);
    let r_ty = ty(&mut c.types, PNode::Ref(Mutbl::Not, pair));
    let ref_u64 = ty(&mut c.types, PNode::Ref(Mutbl::Not, u64t));
    c.local_ty.push(r_ty);
    let (r, e) = (PatId(0), ExprId(0));
    c.captures.push((e, r, CaptureKind::ByRef));
    c.capture_projs.push((0, 0, 0, pair));
    c.capture_projs.push((1, 0, 0, u64t));
    c.capture_places.push((e, r, 0, 2, CaptureKind::ByRef, ref_u64));
    let mut out = PortableTys::new_in(a);
    let (whole, places) = both(&c, e, &mut out);
    let o_u64 = ty(&mut out, PNode::Uint(UintTy::U64));
    let o_u32 = ty(&mut out, PNode::Uint(UintTy::U32));
    let o_ref = ty(&mut out, PNode::Ref(Mutbl::Not, o_u64));
    assert_eq!(places, tuple(&mut out, &[o_ref]), "the field is `&u64`: {:?}", out.view().node(places));
    let o_pair = tuple(&mut out, &[o_u64, o_u32]);
    let o_r = ty(&mut out, PNode::Ref(Mutbl::Not, o_pair));
    let o_rr = ty(&mut out, PNode::Ref(Mutbl::Not, o_r));
    assert_eq!(whole, tuple(&mut out, &[o_rr]), "TWIN: the whole binding by reference: {:?}", out.view().node(whole));
}

/// An owner the analysis did not decide has no place row: under `Places` it is laid out from
/// its whole-binding rows, as under `WholeBindings`. Control of the strategy's fallback to the
/// rows that exist (it is the record's own choice per owner, not a second route).
#[test]
fn an_undecided_owner_keeps_its_whole_bindings() {
    let a = alloc();
    let mut c = empty_record(a);
    let u64t = ty(&mut c.types, PNode::Uint(UintTy::U64));
    let ref_u64 = ty(&mut c.types, PNode::Ref(Mutbl::Not, u64t));
    c.local_ty.push(u64t);
    c.local_ty.push(u64t);
    let (decided, undecided) = (ExprId(0), ExprId(1));
    c.captures.push((decided, PatId(0), CaptureKind::ByRef));
    c.captures.push((undecided, PatId(1), CaptureKind::ByMutRef));
    c.capture_places.push((decided, PatId(0), 0, 0, CaptureKind::ByRef, ref_u64));
    let mut out = PortableTys::new_in(a);
    let (whole, places) = both(&c, undecided, &mut out);
    assert_eq!(whole, places, "an undecided owner lays the same environment under both strategies");
    let o_u64 = ty(&mut out, PNode::Uint(UintTy::U64));
    let o_mut = ty(&mut out, PNode::Ref(Mutbl::Mut, o_u64));
    assert_eq!(places, tuple(&mut out, &[o_mut]), "its whole binding by `&mut`: {:?}", out.view().node(places));
    let (w0, p0) = both(&c, decided, &mut out);
    let o_ref = ty(&mut out, PNode::Ref(Mutbl::Not, o_u64));
    assert_eq!((w0, p0), (tuple(&mut out, &[o_ref]), tuple(&mut out, &[o_ref])), "a whole-binding place lays as its row does");
}
