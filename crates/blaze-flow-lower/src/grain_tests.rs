//! THE GRAIN behind the seam (D190 step 3, grain.rs): under the one rule a place's origins are
//! its type's region slots (blaze-borrow `Columns`), under the old paths the flow's positions.
//! Each test lowers one body under BOTH rules (oracle.rs `against`, the oracle's comparison of
//! their calls kept) and reads the ONE RULE's findings: what the slot grain decides. Each with
//! its NEGATIVE TWIN (rustc rejects: a finding) and, where a cause is claimed, the control that
//! removes it (rule 23).

use std::format;
use std::string::String;
use std::vec::Vec as SVec;

use blaze_flow::record::check;
use blaze_typeck::fixture::{Bb, Fx};
use blaze_typeck::{ExprId, ItemKey, Name, PTy};

use crate::callee_value_tests::{no_clauses, world_with};
use crate::oracle::lower_checked;
use crate::region_fixture::{self, Decls, FnHead, WithRegions};
use crate::{AdtFacts, CalleeFacts, Facts, Lowered, Refusal, Region, Variance};

type Lines = Result<SVec<String>, Refusal>;

/// A lowering's findings, one line each, as region_fixture.rs `lower` prints them (a stricter
/// relation's coarse refusal standing only on a finding, as there).
fn lines(lowered: Result<Lowered<blaze_own::Alloc<'static>>, Refusal>) -> Lines {
    let lowered = lowered?;
    let rec = check(&lowered.flow, region_fixture::alloc()).expect("bounded");
    if let Some(coarse) = lowered.coarse
        && !rec.borrows.is_empty()
    {
        return Err(coarse);
    }
    let mut out = SVec::new();
    for m in rec.moves.iter() {
        out.push(format!("move {:?} at {:?} span {}..{}", m.kind, m.at, m.span.lo, m.span.hi));
    }
    for f in rec.borrows.iter() {
        out.push(format!("borrow {f:?}"));
    }
    Ok(out)
}

/// `root`, the body of `owner`, lowered under production (`OneRule` + `RegionSlots`) over
/// `decls`' heads.
fn one_rule<F: Facts>(fx: &Fx, b: &Bb, owner: ItemKey, root: ExprId, facts: &F, decls: &Decls) -> Lines {
    let view = b.view(owner, root);
    let mut c = region_fixture::checked(fx, &view);
    let regions = decls.lay(fx.t.view(), &mut c.types);
    let with = WithRegions { base: facts, regions: &regions };
    lines(lower_checked(&view, &c, &with, crate::Plant::None, region_fixture::alloc()))
}

fn found(got: &Lines) -> bool {
    got.as_ref().is_ok_and(|l| l.iter().any(|l| l.starts_with("borrow UnprovenOutlives")))
}

fn clean(got: &Lines) -> bool {
    got.as_ref().is_ok_and(|l| l.is_empty())
}

/// The facts of [`a_box_is_covariant_under_the_slot_grain`]: `Box<T>` the owned box (no
/// lifetime, no variance row: the box's covariance is the language's, one_call.rs
/// `FactsArity`'s `BOX`), every other ADT with no lifetime.
struct Boxed {
    boxed: ItemKey,
    /// The control: the box NOT known as the box (as any one-type-parameter ADT whose
    /// variance row is missing).
    known: bool,
}

impl Facts for Boxed {
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
        Some(AdtFacts { copy: false, drop_impl: false, needs_drop: true, lifetimes: 0, union: false, is_enum: false, is_box: self.known && k == self.boxed })
    }
    fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
        Some(CalleeFacts { writes_lifetimes: false, self_ref: false })
    }
    fn adt_variances(&self, _: ItemKey) -> Option<&[Variance]> {
        None
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

const A: Region = Region::Param(0);
const B: Region = Region::Param(1);

/// `fn f<'a, 'b: 'a>(x: Box<&'x S>) -> Box<&'y S> { x }`, with `'x`, `'y` the column `input`,
/// `output`, under the one rule's grain, the box known as the box or not.
fn boxed(input: Region, output: Region, known: bool) -> Lines {
    let mut owner: Option<(ItemKey, ItemKey, PTy)> = None;
    let w = world_with(|fx, _st, rs, _unit| {
        let p0 = fx.param(0);
        let bx = fx.strukt("Box", 1, &[("p", p0)]);
        let t = fx.adt(bx, &[rs]);
        owner = Some((fx.func("f", 0, &[t], t, no_clauses()), bx, t));
    });
    let (f, bx, t) = owner.expect("declared");
    let mut decls = Decls::new();
    decls.adt(bx, 0);
    decls.func(f, FnHead { lifetimes: 2, late: &[], outlives: &[(B, A)], inputs: &[t], output: t, slots: &[input, output] });
    let mut b = Bb::new();
    let px = b.param("x");
    let lx = b.local(px);
    let root = b.block(&[], Some(lx));
    one_rule(&w.fx, &b, f, root, &Boxed { boxed: bx, known }, &decls)
}

/// The Reference, "Subtyping and Variance": `Box<T>` is covariant in `T`. Under the slot grain
/// (variance.rs `variances_by_slots`) `Box<&'b S>` is a `Box<&'a S>` when `'b: 'a`: clean.
/// NEGATIVE TWIN: `Box<&'a S>` to `Box<&'b S>` is "lifetime may not live long enough": found.
/// CONTROL (rule 23): the same ADT not known as the box has no variance row, so its slot is
/// related invariantly (a coarse `VarianceUnknown` note, the stricter answer): the positive
/// is no longer clean.
#[test]
fn a_box_is_covariant_under_the_slot_grain() {
    let one = boxed(B, A, true);
    std::println!("Box<&'b S> -> Box<&'a S>: {one:?}");
    assert!(clean(&one), "covariant: {one:?}");
    let one = boxed(A, B, true);
    std::println!("TWIN Box<&'a S> -> Box<&'b S>: {one:?}");
    assert!(found(&one), "TWIN: the reverse needs 'a: 'b: {one:?}");
    let one = boxed(B, A, false);
    std::println!("CONTROL, not known as the box: {one:?}");
    assert!(!clean(&one), "CONTROL: unknown variance relates both ways: {one:?}");
}

/// `fn f(s: S) { let c = || &s; let r = c(); <use r and mutate s in order> }` under the one
/// rule (callee_value.rs, D190 item 2(b)): the call relates the result with the closure's
/// signature only; the capture reaches the result through the closure's requirement
/// `Capture(0): Sig(0)` (its body returns the capture's borrow into its signature's free
/// region), applied at the closure expression. `g(r); m(&mut s)` is clean; NEGATIVE TWIN `m(&mut
/// s); g(r)` is rustc E0502, here an `Invalidated` finding (no `ClosureValueCall` refusal: the
/// one rule ties no capture to the result). CONTROL (rule 23): the requirement left out, the
/// twin is accepted: the requirement is what carries the loan.
#[test]
fn a_closure_result_holds_its_capture_by_the_signature() {
    use crate::callee_value_tests::{Lane, call_ref, call1};
    use crate::closure::Need;
    use crate::tests::Declared;
    use crate::Requirement;
    use blaze_typeck::Mutbl;
    const REQ: [Requirement; 1] = [Requirement { from: Need::Capture(0), into: Need::Sig(0) }];
    let mut owner = None;
    let w = world_with(|fx, st, _rs, unit| owner = Some(fx.func("f", 0, &[st], unit, no_clauses())));
    let f = owner.expect("declared");
    let run = |late: bool, reqs: &'static [Requirement]| {
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
        let mut lane = Lane::of(&w);
        // The owner's own head, `fn f(s: S)`: no lifetime (as tests.rs declares its `f`). The
        // one rule reads the owner's head at each call it relates (`RegionsUnread { item: f,
        // NoHead }` at `g(r)` and `m(&mut s)` without it: wave-1119 t60 flow-lower.txt, item
        // 1364656135 being `f`, declared after `g`, `m` and `h` in `world_with`); `Lane::of`
        // declares only the world's `g`, `m` and `h`.
        lane.wide.add(f, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[w.st], output: w.unit, slots: &[] });
        lane.closure = Some(cl);
        lane.reqs = reqs;
        let one = one_rule(&w.fx, &b, f, root, &lane, lane.decls());
        std::println!("late {late}, requirements {}: {one:?}", reqs.len());
        one
    };
    let early = run(false, &REQ);
    assert!(clean(&early), "use before the mutation: {early:?}");
    let late = run(true, &REQ);
    assert!(late.as_ref().is_ok_and(|l| l.iter().any(|l| l.starts_with("borrow Invalidated"))), "TWIN: E0502: {late:?}");
    let control = run(true, &[]);
    assert!(clean(&control), "CONTROL: no requirement, nothing carries the loan: {control:?}");
}

/// THE BOUND of the count's recursion through captures (grain.rs `body_slots_in`, rule 18):
/// `fn f(s: S) { let c0 = || {}; let c1 = || { c0; }; ...; let cN = || { c(N-1); }; }`, each
/// closure capturing the one before, so counting `cN`'s slots walks N+1 body types deep
/// (`region_slots` → `body_slots` → `captured_slots` → `region_slots` of the capture's type).
/// Rust accepts every depth (each closure holds `&c(i-1)`; the Reference, "Closure types").
/// [`crate::grain::MOST_BODY_NEST`] bodies deep: clean. NEGATIVE TWIN, one deeper: refused
/// `Form::Bound` by name, never a recursion without end (stricter than Rust, a refusal: never
/// looser). CONTROL (rule 23): as many closures, none capturing another (no nesting): clean
/// at the depth the twin refuses, so the nesting is what the bound counts.
#[test]
fn the_capture_count_walk_is_bounded() {
    use crate::callee_value_tests::Lane;
    use crate::grain::MOST_BODY_NEST;
    use crate::tests::Declared;
    let mut owner = None;
    let w = world_with(|fx, st, _rs, unit| owner = Some(fx.func("f", 0, &[st], unit, no_clauses())));
    let f = owner.expect("declared");
    let run = |closures: usize, nested: bool| {
        let mut b = Bb::new();
        let _s = b.param("s");
        let mut stmts = SVec::new();
        let mut prev = None;
        for i in 0..closures {
            let body = match prev {
                Some(p) if nested => {
                    let used = b.local(p);
                    let st = b.semi(used);
                    b.block(&[st], None)
                }
                _ => b.block(&[], None),
            };
            let cl = b.closure(&[], body);
            let pc = b.bind(&format!("c{i}"));
            stmts.push(b.let_(pc, None, cl));
            prev = Some(pc);
        }
        let root = b.block(&stmts, None);
        let mut lane = Lane::of(&w);
        lane.wide.add(f, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[w.st], output: w.unit, slots: &[] });
        let one = one_rule(&w.fx, &b, f, root, &lane, lane.decls());
        std::println!("{closures} closures, nested {nested}: {one:?}");
        one
    };
    let at_bound = run(MOST_BODY_NEST, true);
    assert!(clean(&at_bound), "{MOST_BODY_NEST} bodies deep: {at_bound:?}");
    let past = run(MOST_BODY_NEST + 1, true);
    assert!(matches!(&past, Err(r) if r.form == crate::Form::Bound), "TWIN: one deeper is refused by name: {past:?}");
    let control = run(MOST_BODY_NEST + 1, false);
    assert!(clean(&control), "CONTROL: as many closures, none nested: {control:?}");
}
