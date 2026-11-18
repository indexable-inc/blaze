//! Lane RB10, class "a reference argument's loan reaches its own referent through the call's
//! hub" (call relating: RB8's to fix; this file is the repro and the diagnosis only; NEVER
//! COMPILED when written, 2026-11-18).
//!
//! The census's 33 `Invalidated` (bstd `write_keys`, `take_all`, `granted_value`, `is_empty`,
//! simtest `laid`, `refusal`, `member_said`): a call `g(&mut *w)` of a known head that writes
//! no lifetimes, whose type argument carries an origin (`T = W<'static>`, bstd's
//! `String<LaunchAlloc>` with `LaunchAlloc = SharedAlloc<'static>`, grant.rs:81), and no
//! generic signature to relate it by (`Facts::shape` or `Facts::call_bounds` missing, or
//! `call_by_generic_signature` not lining up), goes to `unify_call` (lib.rs). There the
//! reference's top origin `'r` flows INTO the hub (one way) and the hub flows into every other
//! position, the referent's `W<'?>` among them; `borrow` related that referent position to
//! `*w`'s both ways (a `&mut` is invariant in its referent), and `*w`'s is `'static`. So the
//! loan of `*w` reaches `'static`, is live for ever, and the next `&mut *w` conflicts with it.
//!
//! The rule rustc applies (RFC 2094, "Subtyping" and "Reborrow constraints"; the Reference,
//! "Lifetime bounds", implied bounds): `fn g<T>(t: &'r mut T)` relates `'r` to `T` only by
//! `T: 'r` (each of `T`'s regions outlives `'r`: a flow from `T`'s positions INTO `'r`, never
//! out of `'r`). A callee that writes no lifetime cannot store the `&'r mut T` anywhere a
//! region of `T` names (no type of its signature holds `'r` but the argument itself, and the
//! output, by elision, only when it has one input lifetime). FIX (RB8, `unify_call` or a
//! generic path before it): for a known head that writes no lifetimes, a reference argument's
//! top origin goes into the output's elided positions only, and its referent's positions are
//! related to the type arguments' as `call_by_generic_signature` relates them; never through
//! the hub that flows back into the referent.

use std::format;
use std::string::String;
use std::vec;

use blaze_flow::record::check;
use blaze_own::Alloc;
use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{ExprId, InferCtx, ItemKey, Mutbl, Name, Needs, Outcome, PTy, PatId, UnOp};

use crate::{AdtFacts, CalleeFacts, Facts, Plant, Refusal, SigBuf, SigLife, lower_planted, read_signature};

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

/// `struct W<'a>;` (one lifetime), `fn g<T>(t: &mut T)`, the owner `fn f(w: &mut W<'static>)`:
/// heads read from their texts; no shape and no bounds (`unify_call`).
struct World {
    w: ItemKey,
    sigs: vec::Vec<(ItemKey, SigBuf<Alloc<'static>>)>,
}

impl World {
    fn add(&mut self, fx: &Fx, item: ItemKey, text: &str, inputs: &[PTy], output: PTy) {
        let w = self.w;
        let lifetimes = move |k: ItemKey| Some(u32::from(k == w));
        let mut buf = SigBuf::new_in(alloc());
        if let Err(why) = read_signature(text, fx.t.view(), inputs, output, &lifetimes, &mut buf, alloc()) {
            panic!("`{text}` was not read: {why:?}");
        }
        self.sigs.push((item, buf));
    }
}

impl Facts for World {
    fn adt(&self, k: ItemKey) -> Option<AdtFacts> {
        let lifetimes = u32::from(k == self.w);
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: false, lifetimes, union: false, is_enum: false, is_box: false })
    }
    fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: false, self_ref: false })
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

/// `*operand`: the fixture has no deref builder, so it takes the slot of a read of `p`.
fn deref(b: &mut Bb, p: PatId, operand: ExprId) -> ExprId {
    let at = b.local(p);
    b.exprs[at.ix()] = blaze_typeck::Expr::Unary { op: UnOp::Deref, operand };
    at
}

/// `&mut *w`.
fn reborrow(b: &mut Bb, w: PatId) -> ExprId {
    let lw = b.local(w);
    let d = deref(b, w, lw);
    b.borrow(Mutbl::Mut, d)
}

/// The findings of `fn f(w: &mut W<'static>) { <body> }`: `held` false, `g(&mut *w); g(&mut
/// *w);`; `held` true, `let r = &mut *w; g(&mut *w); g(r);`.
fn findings(held: bool) -> Result<vec::Vec<String>, Refusal> {
    let mut fx = Fx::new();
    let wk = fx.strukt("W", 0, &[]);
    let wt = fx.adt(wk, &[]);
    let rw = fx.r(Mutbl::Mut, wt);
    let t0 = fx.param(0);
    let rt = fx.r(Mutbl::Mut, t0);
    let unit = fx.unit();
    let g = fx.func("g", 1, &[rt], unit, vec![]);
    let f = fx.func("f", 0, &[rw], unit, vec![]);
    fx.finish();
    let mut w = World { w: wk, sigs: vec::Vec::new() };
    w.add(&fx, g, "fn g<T>(t: &mut T) {}", &[rt], unit);
    w.add(&fx, f, "fn f(w: &mut W<'static>) {}", &[rw], unit);
    let mut b = Bb::new();
    let pw = b.param("w");
    let mut stmts = vec::Vec::new();
    let r = if held {
        let r = b.bind("r");
        let first = reborrow(&mut b, pw);
        stmts.push(b.let_(r, None, first));
        Some(r)
    } else {
        None
    };
    let callee = b.path(&["g"]);
    let arg = reborrow(&mut b, pw);
    let call = b.call(callee, &[arg]);
    stmts.push(b.semi(call));
    let callee = b.path(&["g"]);
    let arg = match r {
        Some(r) => b.local(r),
        None => reborrow(&mut b, pw),
    };
    let call = b.call(callee, &[arg]);
    stmts.push(b.semi(call));
    let root = b.block(&stmts, None);
    let a = alloc();
    let view = b.view(f, root);
    let mut cx = InferCtx::new_in(a);
    let c = match blaze_typeck::check(&view, &fx, &mut cx, &Needs::new_in(a), a) {
        Ok(Outcome::Done(c)) => {
            assert!(c.diags.is_empty(), "the fixture type-checks clean: {:?}", c.diags);
            c
        }
        Ok(Outcome::Need(n)) => panic!("the world missed {:?}", n.questions),
        Err(e) => panic!("{e:?}"),
    };
    let lowered = lower_planted(&view, &c, &w, Plant::None, a)?;
    let rec = check(&lowered.flow, a).expect("bounded");
    let mut out = vec::Vec::new();
    for m in rec.moves.iter() {
        out.push(format!("move {:?} at {:?}", m.kind, m.at));
    }
    for f in rec.borrows.iter() {
        out.push(format!("borrow {f:?}"));
    }
    Ok(out)
}

/// The bstd shape: two reborrows of `*w` one after the other, each loan dead when its call
/// returns; rustc accepts. IGNORED until RB8's fix: today the first loan reaches `'static`
/// through the hub and the second `&mut *w` is `Invalidated` (the census's class).
#[test]
#[ignore = "RB8: unify_call's hub carries a reference argument's loan into its referent's `'static`"]
fn two_reborrows_through_a_generic_call_do_not_conflict() {
    let got = findings(false);
    std::println!("hub_call two reborrows: {got:?}");
    assert_eq!(got, Ok(vec::Vec::new()));
}

/// NEGATIVE (rustc E0499): `let r = &mut *w; g(&mut *w); g(r);`: `r`'s loan is live across
/// the second reborrow. Must stay refused after RB8's fix.
#[test]
fn a_reborrow_held_across_a_second_is_refused() {
    let got = findings(true);
    std::println!("hub_call held reborrow: {got:?}");
    assert!(matches!(got, Ok(ref lines) if lines.iter().any(|l| l.contains("Invalidated"))), "{got:?}");
}
