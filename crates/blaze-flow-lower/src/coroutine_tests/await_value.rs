//! The value of an `.await` of an `async fn`'s call, related through the fn's WRITTEN return
//! type (await_call.rs `awaited_shadow`, coroutine.rs `await_done`), never through the future.
//!
//! The rules. The Reference, "Async functions": `async fn open(s: &mut S) -> &'static u8` is
//! "roughly equivalent to" `fn open<'a>(s: &'a mut S) -> impl Future<Output = &'static u8> +
//! 'a`: the future captures the borrow of `s`; its output is the written `&'static u8`, which
//! does not. "Await expressions": the value of `open(&mut s).await` is what `poll` returns in
//! `Ready`, `Future::Output`. So in
//! `async { let s = make(); let v = open(&mut s).await; touch(&mut s); keep(v) }` the second
//! `&mut s` conflicts with nothing: the first borrow ends with the future (rustc accepts it).
//! With `async fn get(s: &mut S) -> &u8` (elision: the output is `&'a u8`, the borrow of `s`)
//! the same body is E0499: `v` holds the first borrow when `touch(&mut s)` takes the second.
//!
//! The cause these tests pin (tempo tip20_channel_reserve.rs:229, `ChannelEnv::open(&mut
//! payer, ..).await` then a use of `payer` while the value is held): the poll's result was
//! related to the future's positions (`unify(&[fut, result])`), and an opaque type is ONE
//! position (lib.rs `npos`) holding every region it captures: the borrow of `s` among them.
//!
//! Control (rule 23): with `await_done`'s relation put back to `self.unify(&[fp, rp])` for
//! every `.await`, the first test fails (the borrow of `s` reaches `v`: one `Invalidated`).
//! NOT YET RUN: written by a write-only lane (2026-11-20); the control is the reasoning above
//! until a run shows it.

use std::format;
use std::string::String;
use std::vec;

use blaze_flow::record::check;
use blaze_own::Alloc;
use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{Capture, Checked, Def, ExprId, InferCtx, ItemKey, LangItem, Mutbl, Name, Needs, Outcome, PNode};

use super::declare;
use crate::region_fixture::{Decls, WithRegions};
use crate::{AdtFacts, CalleeFacts, Facts, Region};

/// Plain structs needing drop, no lifetimes; nothing said of any future.
struct ValueFacts;

impl Facts for ValueFacts {
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
    fn adt(&self, _: ItemKey) -> Option<AdtFacts> {
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: true, lifetimes: 0, union: false, is_enum: false, is_box: false })
    }
    fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: false, self_ref: false })
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

struct World {
    owner: ItemKey,
    open: ItemKey,
}

/// `struct S;`, `fn make() -> S`, `fn touch(_: &mut S)`, `fn keep(_: &u8)`, `fn spawn<T>(_:
/// T)`, `fn owner()`, and `async fn open(s: &mut S) -> &'static u8` as the front end files an
/// `async fn` (its output an opaque capturing the input's one lifetime, bounds `Future<Output
/// = &u8>` and `Sized`; its written return type's column `written`: `[Static]` for `open`,
/// `[Param(0)]` for the twin's `get(s: &mut S) -> &u8`; `None`: a head with no written return
/// type, `fn open(s: &mut S) -> impl Future<Output = &u8>`, not an `async fn`). `opaque_out`:
/// the opaque's own head files its `Output = &u8` clause (blaze-ladder heads.rs `opaque_filed`,
/// in `open`'s numbering) with the reference's region `opaque_out`; `None`: no head for it.
fn world(written: Option<Region>, opaque_out: Option<Region>) -> (Fx, World, Decls) {
    let mut fx = Fx::new();
    let (p0, unit, u8_) = (fx.param(0), fx.unit(), fx.u8());
    let sized = fx.tr("Sized", 1, vec::Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let copy = fx.tr("Copy", 1, vec::Vec::new(), &[]);
    fx.lang(LangItem::Copy, copy);
    let future = fx.tr("Future", 1, vec::Vec::new(), &["Output"]);
    fx.lang(LangItem::Future, future);
    let into_future = fx.tr("IntoFuture", 1, vec::Vec::new(), &["Output", "IntoFuture"]);
    fx.lang(LangItem::IntoFuture, into_future);
    let held = fx.alias(into_future, "IntoFuture", &[p0]);
    let f = fx.trait_fn(into_future, "into_future", true, &[p0], held);
    fx.lang_def(LangItem::IntoFutureIntoFuture, Def::Fn(f));
    let fut_out = fx.alias(future, "Output", &[p0]);
    let is_future = fx.bound(future, &[p0]);
    fx.imp(
        "impl<F: Future> IntoFuture for F",
        1,
        p0,
        Some((into_future, &[])),
        &[("Output", fut_out), ("IntoFuture", p0)],
        vec![is_future],
    );
    let s = fx.strukt("S", 0, &[]);
    let s_t = fx.adt(s, &[]);
    let mut_s = fx.r(Mutbl::Mut, s_t);
    let ref_u8 = fx.r(Mutbl::Not, u8_);
    let make = fx.func("make", 0, &[], s_t, vec::Vec::new());
    let touch = fx.func("touch", 0, &[mut_s], unit, vec::Vec::new());
    let keep = fx.func("keep", 0, &[ref_u8], unit, vec::Vec::new());
    let opaque_k = fx.opaque_key("open::{opaque#0}");
    let none = fx.list(&[]);
    let opaque = fx.ty(PNode::Opaque(opaque_k, none));
    let opaque_future = fx.bound(future, &[opaque]);
    let out_ref = fx.proj(future, &[opaque], "Output", ref_u8);
    let opaque_sized = fx.bound(sized, &[opaque]);
    fx.set_opaque_bounds(opaque_k, vec![opaque_future, out_ref, opaque_sized]);
    // The clause `Output = &u8` as a head record lays it: `ExProj { args: [Self], term }`.
    let self_list = fx.list(&[opaque]);
    let out_clause = fx.ty(PNode::ExProj { trait_: future, name: Name::of("Output"), args: self_list, term: ref_u8 });
    let open = fx.func("open", 0, &[mut_s], opaque, vec::Vec::new());
    let spawn = fx.func("spawn", 1, &[p0], unit, vec::Vec::new());
    let owner = fx.func("owner", 0, &[], unit, vec::Vec::new());
    fx.finish();
    // The heads as written: `open`'s input `&'0 mut S`, its opaque capturing `'0` (the
    // Reference, "Async functions": the future captures every input lifetime), its written
    // return type `&'written u8`; `touch` and `keep` one elided input lifetime each.
    let mut decls = Decls::new();
    decls.opaque(opaque_k, 1);
    declare(&mut decls, make, &[], s_t, &[]);
    declare(&mut decls, touch, &[mut_s], unit, &[Region::Param(0)]);
    declare(&mut decls, keep, &[ref_u8], unit, &[Region::Param(0)]);
    declare(&mut decls, open, &[mut_s], opaque, &[Region::Param(0), Region::Param(0)]);
    if let Some(written) = written {
        decls.written_output(open, ref_u8, &[written]);
    }
    if let Some(r) = opaque_out {
        // `<_ as Future>::Output` has no lifetime of its own; the column: `Self`'s capture
        // (`open`'s `'0`), then the term's one region.
        decls.alias(future, "Output", 0);
        decls.opaque_clause(opaque_k, 1, 1, 0, out_clause, &[Region::Param(0), r]);
    }
    declare(&mut decls, spawn, &[p0], unit, &[]);
    declare(&mut decls, owner, &[], unit, &[]);
    (fx, World { owner, open }, decls)
}

/// `let s = make(); let v = open(&mut s).await; touch(&mut s); keep(v)` (the flow checks no
/// binding's mutability; the fixture's bindings are plain). `touch` false: without
/// `touch(&mut s);`, nothing conflicts with what `v` holds under any relation.
fn body(b: &mut Bb, touch: bool) -> ExprId {
    let e0 = b.path(&["make"]);
    let e1 = b.call(e0, &[]);
    let s = b.bind("s");
    let s0 = b.let_(s, None, e1);
    let e2 = b.path(&["open"]);
    let e3 = b.local(s);
    let e4 = b.borrow(Mutbl::Mut, e3);
    let e5 = b.call(e2, &[e4]);
    let e6 = b.await_(e5);
    let v = b.bind("v");
    let s1 = b.let_(v, None, e6);
    // `touch(&mut s);` is BUILT only when it is in the block: an expression the builder made
    // but no statement reaches is malformed (typeck writeback.rs `Malformed::Unreached`; the
    // `touch: false` case once failed there, at the `touch` path, before any lowering ran).
    let mut stmts = vec![s0, s1];
    if touch {
        let e7 = b.path(&["touch"]);
        let e8 = b.local(s);
        let e9 = b.borrow(Mutbl::Mut, e8);
        let e10 = b.call(e7, &[e9]);
        stmts.push(b.semi(e10));
    }
    let e11 = b.path(&["keep"]);
    let e12 = b.local(v);
    let e13 = b.call(e11, &[e12]);
    b.block(&stmts, Some(e13))
}

/// The findings of `fn owner() { spawn(async { body }); }` with `open`'s written return type's
/// one region `written`, each printed.
fn findings(written: Region) -> vec::Vec<String> {
    match lowered(Some(written), None, true) {
        (Ok(found), _) => found,
        (Err(r), _) => panic!("lower_async refused: {r:?}"),
    }
}

/// The same body's findings (with `touch`: [`body`]), or the lowering's refusal, with `open`'s
/// written return type `written` (`None`: no column) and its opaque's `Output` clause region
/// `opaque_out` ([`world`]); and `open`'s key. A borrow finding of a flow that used a stricter
/// relation (`LoweredAsync::coarse`) is that relation's refusal, as blaze-ladder applies it.
fn lowered(written: Option<Region>, opaque_out: Option<Region>, touch: bool) -> (Result<vec::Vec<String>, crate::Refusal>, ItemKey) {
    let alloc: Alloc<'static> = crate::region_fixture::alloc();
    let (fx, w, decls) = world(written, opaque_out);
    let mut b = Bb::new();
    let e0 = b.path(&["spawn"]);
    let inner = body(&mut b, touch);
    let block = b.async_block(inner, Capture::Ref);
    let call = b.call(e0, &[block]);
    let st = b.semi(call);
    let root = b.block(&[st], None);
    let view = b.view(w.owner, root);
    let mut cx = InferCtx::new_in(alloc);
    let mut c: Checked<'static> = match blaze_typeck::check(&view, &fx, &mut cx, &Needs::new_in(alloc), alloc) {
        Ok(Outcome::Done(c)) => c,
        Ok(Outcome::Need(n)) => panic!("the world missed {:?}", n.questions),
        Err(e) => panic!("check refused: {e:?}"),
    };
    assert!(c.diags.is_empty(), "the fixture type-checks clean: {:?}", c.diags);
    assert_eq!(c.awaited.len(), 1, "one `.await`, its held future recorded: {:?}", c.awaited);
    let regions = decls.lay(fx.t.view(), &mut c.types);
    let facts = WithRegions { base: &ValueFacts, regions: &regions };
    let lowered = match crate::oracle::lower_async_checked(&view, &c, block, &facts, alloc) {
        Ok(l) => l,
        Err(r) => {
            std::println!("written {written:?}: refused {r:?}");
            return (Err(r), w.open);
        }
    };
    let rec = check(&lowered.flow, alloc).expect("bounded");
    if let Some(coarse) = lowered.coarse
        && !rec.borrows.is_empty()
    {
        std::println!("written {written:?}: a finding under a stricter relation: refused {coarse:?}");
        return (Err(coarse), w.open);
    }
    let mut found: vec::Vec<String> = vec::Vec::new();
    for m in rec.moves.iter() {
        found.push(format!("move {:?} at {:?}", m.kind, m.at));
    }
    for f in rec.borrows.iter() {
        found.push(format!("borrow {f:?}"));
    }
    for line in found.iter() {
        std::println!("written {written:?}: {line}");
    }
    (Ok(found), w.open)
}

/// `async fn open(s: &mut S) -> &'static u8`: the awaited value is `'static`; the borrow of
/// `s` ends with the future, so `touch(&mut s)` while `v` is held is accepted (module doc).
/// Prints "await value by its written type: 0 findings".
#[test]
fn an_awaited_value_does_not_hold_the_futures_borrow() {
    let found = findings(Region::Static);
    assert!(found.is_empty(), "the value of `open(&mut s).await` holds no borrow of `s`: {found:?}");
    std::println!("await value by its written type: {} findings", found.len());
}

/// TWIN: `async fn get(s: &mut S) -> &u8`: the written output is the input's lifetime, so `v`
/// holds the borrow of `s` and `touch(&mut s)` before `keep(v)` is E0499 (module doc).
/// Prints "await value borrowing its argument: N findings" with N at least 1.
#[test]
fn an_awaited_value_that_borrows_its_argument_conflicts() {
    let found = findings(Region::Param(0));
    assert!(found.iter().any(|l| l.starts_with("borrow")), "`v` holds the borrow of `s` across `touch(&mut s)`: {found:?}");
    std::println!("await value borrowing its argument: {} findings", found.len());
}

/// `fn open(s: &mut S) -> impl Future<Output = &u8>`, no written return type column (not an
/// `async fn`) and no head filed for its opaque: what the value may hold is the opaque's
/// `Output = ..` to say, and nothing read says it. The value is made equal to the future's
/// one position, which holds the borrow of `s`: STRICTER than Rust's (noted
/// `AwaitValueUnrelated { why: OpaqueOutputUnread { opaque }, callee: Some(open) }`,
/// await_call.rs `opaque_output`, `LoweredAsync::coarse`). Under it `touch(&mut s)` while `v`
/// is held conflicts, a finding that may be the relation's artifact: the body is REFUSED by
/// that name, the finding dropped. STRICT RELATIONS REFUSE ONLY ON A FINDING: the same body
/// without `touch(&mut s)` has no finding under the stricter relation and stands CHECKED
/// clean. NEGATIVE TWIN on the precise path: `an_opaque_futures_value_is_its_output_binding`'s
/// `Param(0)` case (the `Output` clause read) reports the real E0499. The control for "a value
/// with no position needs nothing" is await_operand.rs's `fetch(&x).await`.
/// Prints "await value with no written type: refused ..." and "... no touch: Ok([])".
#[test]
fn an_awaited_value_with_no_written_type_is_refused_by_name() {
    let (got, open) = lowered(None, None, true);
    std::println!("await value with no written type: {got:?}");
    let refused = match &got {
        Err(crate::Refusal { form: crate::Form::AwaitValueUnrelated { why: crate::AwaitUnrelated::OpaqueOutputUnread { .. }, callee }, .. }) => *callee == Some(open),
        _ => false,
    };
    assert!(refused, "refused by name, the opaque's `Output` unread: {got:?}");
    let (got, _) = lowered(None, None, false);
    std::println!("await value with no written type, no touch: {got:?}");
    assert_eq!(got, Ok(vec::Vec::new()), "clean under the stricter relation: checked, not refused");
}

/// `fn open(s: &mut S) -> impl Future<Output = &'static u8> + '_` (not an `async fn`): the
/// awaited value is the opaque's `Output`, `&'static u8`, read from its `Output = ..` clause
/// (await_call.rs `opaque_output`): the borrow of `s` ends with the future, so `touch(&mut s)`
/// while `v` is held is accepted, as rustc accepts it (tempo's jsonrpsee `RpcResult<T>` outputs,
/// `ErrorObject<'static>`, are this case). TWIN, `-> impl Future<Output = &u8> + '_` (the
/// output borrows `s`, `Param(0)`): E0499, a borrow finding.
/// Prints "opaque output Static: N findings" and "opaque output Param(0): N findings".
#[test]
fn an_opaque_futures_value_is_its_output_binding() {
    let mut wrong = vec::Vec::new();
    for (r, want_found) in [(Region::Static, false), (Region::Param(0), true)] {
        let (got, _) = lowered(None, Some(r), true);
        let found = match got {
            Ok(found) => found,
            Err(e) => panic!("opaque output {r:?}: refused, not checked: {e:?}"),
        };
        std::println!("opaque output {r:?}: {} findings: {found:?}", found.len());
        if found.iter().any(|l| l.starts_with("borrow")) != want_found {
            wrong.push(format!("{r:?} want a borrow finding {want_found}: {found:?}"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:?}");
}
