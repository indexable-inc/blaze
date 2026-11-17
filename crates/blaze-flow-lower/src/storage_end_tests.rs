//! Piece storage-end-loans (SOUNDNESS): a local's storage ends at the end of its scope, and a
//! parameter's at the function's end, WHATEVER ITS TYPE; the end invalidates every loan of that
//! place still live; `needs_drop` decides only whether a `Drop` runs there.
//!
//! The program: `fn keep(x: &'static S) {}` and `fn f(s: S) { keep(&s); }`, `S` with no drop
//! glue. The Reference, "Lifetime bounds": `'static` "outlives all other lifetimes", so the
//! borrow of `s` must live for ever; "Function parameters" / "Temporary scopes" ("Function
//! parameters are dropped after the function body", "the entire function body scope"): `s`'s
//! storage ends when `f` returns, drop glue or not. E0597 ("borrowed value does not live long
//! enough", `borrow Invalidated` here) at that end.
//!
//! Before the fix (lib.rs `exit_drops`), a PARAMETER got a `Drop` only when its type needs one,
//! and NO `StorageDead`: with `S` not `Drop`, nothing ended `s`, and the program was ACCEPTED. A
//! `let` local got its `StorageDead` from `drop_local` whatever its type (so the `let` case is
//! expected to be found already; the run before the fix says).
//! CONTROL (rule 23): `Plant::StorageEndTiedToDrop` (storage ends only where a drop runs, the
//! form before the fix and its generalization to `let`) accepts all three.

use std::format;
use std::string::String;
use std::vec;

use blaze_flow::record::check;
use blaze_own::Alloc;
use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{ExprId, InferCtx, ItemKey, Mutbl, Name, Needs, Outcome, PTy};

use crate::{AdtFacts, CalleeFacts, Facts, Plant, Refusal, SigBuf, SigLife, lower_planted, read_signature};

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

/// `S` has no drop glue; `copy`: it is `Copy` too. Heads read from their text; `keep` writes
/// `'static`.
struct World {
    copy: bool,
    keep: ItemKey,
    sigs: vec::Vec<(ItemKey, SigBuf<Alloc<'static>>)>,
}

impl World {
    fn add(&mut self, fx: &Fx, item: ItemKey, text: &str, inputs: &[PTy], output: PTy) {
        let adt = |_: ItemKey| Some(0);
        let mut buf = SigBuf::new_in(alloc());
        if let Err(why) = read_signature(text, fx.t.view(), inputs, output, &adt, &mut buf, alloc()) {
            panic!("`{text}` was not read: {why:?}");
        }
        self.sigs.push((item, buf));
    }
}

impl Facts for World {
    fn adt(&self, _: ItemKey) -> Option<AdtFacts> {
        Some(AdtFacts { copy: self.copy, drop_impl: false, needs_drop: false, lifetimes: 0, union: false, is_enum: false, is_box: false })
    }
    fn callee(&self, k: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: k == self.keep, self_ref: false })
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

fn run(fx: &Fx, b: &Bb, owner: ItemKey, root: ExprId, facts: &World, plant: Plant) -> Result<vec::Vec<String>, Refusal> {
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
    let lowered = lower_planted(&view, &c, facts, plant, a)?;
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

/// The three bodies, each of owner `f(t: S)` or `f(s: S)`:
/// - `param`: `fn f(s: S) { keep(&s); }`;
/// - `let`: `fn f(t: S) { let s = t; keep(&s); }`;
/// - `fine`: `fn f(s: S) { look(&s); }` with `fn look(x: &S) {}`: no finding (the parameter's
///   storage end does not invalidate a loan no live origin holds).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Body {
    Param,
    Let,
    Fine,
}

/// The findings of `body` with `S` `Copy` or not, under `plant`.
fn findings(body: Body, copy: bool, plant: Plant) -> Result<vec::Vec<String>, Refusal> {
    let mut fx = Fx::new();
    let s = fx.strukt("S", 0, &[]);
    let st = fx.adt(s, &[]);
    let rs = fx.r(Mutbl::Not, st);
    let unit = fx.unit();
    let keep = fx.func("keep", 0, &[rs], unit, vec![]);
    let look = fx.func("look", 0, &[rs], unit, vec![]);
    let f = fx.func("f", 0, &[st], unit, vec![]);
    fx.finish();
    let mut w = World { copy, keep, sigs: vec::Vec::new() };
    w.add(&fx, keep, "fn keep(x: &'static S) {}", &[rs], unit);
    w.add(&fx, look, "fn look(x: &S) {}", &[rs], unit);
    let own = match body {
        Body::Param => "fn f(s: S) { keep(&s); }",
        Body::Let => "fn f(t: S) { let s = t; keep(&s); }",
        Body::Fine => "fn f(s: S) { look(&s); }",
    };
    w.add(&fx, f, own, &[st], unit);
    let mut b = Bb::new();
    let callee = b.path(&[if body == Body::Fine { "look" } else { "keep" }]);
    let root = match body {
        Body::Param | Body::Fine => {
            let ps = b.param("s");
            let ls = b.local(ps);
            let bor = b.borrow(Mutbl::Not, ls);
            let call = b.call(callee, &[bor]);
            let st1 = b.semi(call);
            b.block(&[st1], None)
        }
        Body::Let => {
            let pt = b.param("t");
            let vs = b.bind("s");
            let lt = b.local(pt);
            let st1 = b.let_(vs, None, lt);
            let ls = b.local(vs);
            let bor = b.borrow(Mutbl::Not, ls);
            let call = b.call(callee, &[bor]);
            let st2 = b.semi(call);
            b.block(&[st1, st2], None)
        }
    };
    run(&fx, &b, f, root, &w, plant)
}

fn found(got: &Result<vec::Vec<String>, Refusal>) -> bool {
    matches!(got, Ok(lines) if lines.iter().any(|l| l.starts_with("borrow Invalidated")))
}

/// Prints, one line per case, `storage_end <case> copy <bool>: [..]`; each of `param`, `let`
/// (S not `Copy`) and `param` with S `Copy` gives `["borrow Invalidated ..`; `fine` gives `[]`.
/// The CONTROL lines `.. planted: []` follow (storage ended only where a drop runs).
#[test]
fn a_local_s_storage_end_invalidates_its_static_loan_whatever_its_type() {
    let mut missed = vec::Vec::new();
    for (body, copy) in [(Body::Param, false), (Body::Let, false), (Body::Param, true), (Body::Let, true)] {
        let got = findings(body, copy, Plant::None);
        std::println!("storage_end {body:?} copy {copy}: {got:?}");
        if !found(&got) {
            missed.push(format!("{body:?} copy {copy}"));
        }
    }
    for copy in [false, true] {
        let got = findings(Body::Fine, copy, Plant::None);
        std::println!("storage_end Fine copy {copy}: {got:?}");
        assert_eq!(got, Ok(vec::Vec::new()), "a loan no live origin holds is not invalidated by the end of `s`");
    }
    assert!(missed.is_empty(), "E0597 missed: {missed:?}");
    for (body, copy) in [(Body::Param, false), (Body::Let, false), (Body::Param, true)] {
        let got = findings(body, copy, Plant::StorageEndTiedToDrop);
        std::println!("storage_end {body:?} copy {copy} planted: {got:?}");
        assert!(!found(&got), "the control accepts: {got:?}");
    }
}
