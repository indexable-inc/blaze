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

use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{ExprId, ItemKey, Mutbl, Name, PTy};

use crate::region_fixture::{Decls, FnHead};
use crate::{AdtFacts, CalleeFacts, Facts, Plant, Refusal, Region};

/// The head records of the world's fns (D148), declared with the shared fixture and laid in
/// the checked record's own table (region_fixture.rs `run`): the toy's one ADT, `S`, declares
/// no lifetime.
struct Heads(Decls);

impl Heads {
    /// `item`'s head: `count` lifetimes, all late-bound (none is in a bound), its column over
    /// `fn(io[..last]) -> io[last]` `slots`.
    fn add(&mut self, _fx: &mut Fx, item: ItemKey, count: u32, io: &[PTy], slots: &[Region]) {
        let Some((&output, inputs)) = io.split_last() else { panic!("{item:?}: a head has an output") };
        let late: vec::Vec<u32> = (0..count).collect();
        self.0.func(item, FnHead { lifetimes: count, late: &late, outlives: &[], inputs, output, slots });
    }
}

/// `S` has no drop glue; `copy`: it is `Copy` too. Heads from their region records; `keep`
/// writes `'static`.
struct World {
    copy: bool,
    keep: ItemKey,
    heads: Heads,
}

impl Facts for World {
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
        Some(AdtFacts { copy: self.copy, drop_impl: false, needs_drop: false, lifetimes: 0, union: false, is_enum: false, is_box: false })
    }
    fn callee(&self, k: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: k == self.keep, self_ref: false })
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

/// The findings of one body, the heads laid into the checked record's own table.
fn run(fx: &Fx, b: &Bb, owner: ItemKey, root: ExprId, facts: &World, plant: Plant) -> Result<vec::Vec<String>, Refusal> {
    crate::region_fixture::run(fx, &b.view(owner, root), facts, &facts.heads.0, plant).map(|r| r.lines)
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
    // `fn keep(x: &'static S)`: `['static]`; `fn look(x: &S)`: one elided input lifetime,
    // `Param(0)`; `fn f(s: S)` (or `t: S`): no lifetime.
    let mut heads = Heads(Decls::new());
    heads.add(&mut fx, keep, 0, &[rs, unit], &[Region::Static]);
    heads.add(&mut fx, look, 1, &[rs, unit], &[Region::Param(0)]);
    heads.add(&mut fx, f, 0, &[st, unit], &[]);
    let w = World { copy, keep, heads };
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
