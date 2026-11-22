//! The call-rule oracle (D156, rule 78): every fixture body lowered under BOTH call rules
//! ([`OldPaths`], [`OneRule`]), compared call site by call site, before production's lowering is
//! handed back. One generic entry, [`lower_checked`], which every fixture helper calls in place
//! of `lower_planted` (region_fixture.rs `lower`, tests.rs `findings_with` and
//! `update_findings`, lib.rs `lowered_apart`).
//!
//! A CALL'S CONSTRAINTS, NORMALIZED. Each rule makes its own intermediate origins (an instance's,
//! a hub), numbered as it goes, so two lowerings' subsets are not compared edge by edge. What
//! both share is what each origin of the call STANDS FOR (builder.rs [`Label`]): an argument's
//! `k`-th slot, the destination's, a caller's universal region, a caller's `ρ_P`. A call's
//! normalized set is every ordered pair of labels `(x, y)`, `x`'s origin reaching `y`'s through
//! the subsets the call emitted (`'x: 'y` derived at the call).
//!
//! `'STATIC`. `'static: y` holds of every `y`: no pair from `'static` is kept, and a label that
//! reaches `'static` keeps the one pair `(x, 'static)`, which DERIVES `(x, y)` for every `y`
//! when the sets are compared ([`compare`]), never spread over the walk's labels: each walk
//! makes its universals and `ρ_P` as it meets them, so the two label sets differ, and a spread
//! pair to a label only the old walk made read as "fewer". blaze-borrow emits nothing for
//! `'static: y` (relate.rs `outlives`), the old paths emit it as an edge; the two are the same
//! constraint.
//!
//! EVERY BODY. A fn body ([`lower_checked`]), a closure's ([`lower_closure_checked`]), an async
//! block's ([`lower_async_checked`]) and an async fn's ([`lower_async_fn_checked`]), each under
//! both rules through the same seam. A refusal of the call rule is per call in a comparing
//! lowering (lib.rs `lower_with`): the walk goes on, so the body's later calls are compared too,
//! and the answer is still the first refusal.
//!
//! THE RULES.
//! (a) The one rule accepts a call the old paths refused: allowed only where the running
//!     fixture names the case (`allowing`), else a failure.
//! (b) Both accept: every pair of the old set is in the one rule's (it never emits fewer).
//!     Except at a call the old paths took through the hub (`unify_call`, path 8, marked by
//!     `Builder::mark_hub`): there fewer is the expected precision, a counted, printed
//!     allowance ("hub: one rule is more precise at N calls"), never silent.
//! (c) The old paths accept and the one rule refuses: reported (printed) with its typed refusal,
//!     never a failure: a gap the flip must close first.
//! A failure prints the callee, the argument of each differing pair, both sets and both
//! answers. The planted control ([`OneRuleDropOne`]: the adapter drops one subset per call) is
//! caught by (b) (test `the_oracle_catches_a_dropped_constraint`).
//!
//! WHAT IT DOES NOT SEE. Calls after a refusal that is not the call rule's (a body form the
//! lowering refuses: the walk stops there); bodies blaze-ladder lowers (borrows_nested.rs calls
//! the public entries, production's rule only: the oracle runs where fixtures lower); edges a
//! path emits other than through `Builder::subset` (known edges, placeholder statements).

use core::alloc::Allocator;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::string::String;
use std::vec;

use blaze_flow::Origin;
use blaze_typeck::regions::Region;
use blaze_typeck::{BodyView, Checked, ExprId, ItemKey};

use crate::builder::{Label, Log, Site};
use crate::closure::{lower_async_fn_with, lower_closure_with};
use crate::coroutine::lower_async_with;
use crate::one_call::{CallRule, OldPaths, OneRule, Production};
use crate::{ClosureFlow, Facts, Form, Lowered, LoweredAsync, Plant, Refusal, lower_planted, lower_with};

/// One difference the oracle found at a call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Finding {
    /// (a), with no fixture naming it.
    OneAcceptsOldRefused { e: ExprId, callee: Option<ItemKey>, old: Refusal, one: Pairs },
    /// (b): `missing` are the old pairs the one rule lacks.
    Fewer { e: ExprId, callee: Option<ItemKey>, old: Pairs, one: Pairs, missing: Pairs },
    /// (c): reported, not failed.
    OneRefuses { e: ExprId, callee: Option<ItemKey>, old: Pairs, one: Refusal },
    /// THE HUB ALLOWANCE (decided 2026-11-20): (b)'s difference at a call the old paths took
    /// through `unify_call`'s one hub origin (path 8), the over-approximation the one rule
    /// replaces. Counted and printed, never a failure; every other path keeps (b) strict.
    HubMorePrecise { e: ExprId, callee: Option<ItemKey>, old: Pairs, one: Pairs, missing: Pairs },
}

impl Finding {
    fn fails(&self) -> bool {
        !matches!(self, Finding::OneRefuses { .. } | Finding::HubMorePrecise { .. })
    }
}

pub(crate) type Pairs = BTreeSet<(Label, Label)>;

std::thread_local! {
    /// The old refusals the running fixture names as ones the one rule may accept (rule (a)),
    /// by form (its variant).
    static ALLOWED: RefCell<vec::Vec<Form>> = const { RefCell::new(vec::Vec::new()) };
}

/// Run `f` with the old refusals of the forms in `forms` named as ones the one rule may accept.
#[allow(dead_code)]
pub(crate) fn allowing<T>(forms: &[Form], f: impl FnOnce() -> T) -> T {
    let before = ALLOWED.with(|a| a.borrow().len());
    ALLOWED.with(|a| a.borrow_mut().extend_from_slice(forms));
    let out = f();
    ALLOWED.with(|a| a.borrow_mut().truncate(before));
    out
}

fn allowed(form: Form) -> bool {
    ALLOWED.with(|a| a.borrow().iter().any(|f| core::mem::discriminant(f) == core::mem::discriminant(&form)))
}

/// THE ENTRY: `body` lowered under both rules and compared ([`compare`]); a failing finding
/// panics with the message ([`describe`]), a reported one is printed. Production's lowering is
/// the answer. A planted lowering (`plant` not `None`) is production's alone.
pub(crate) fn lower_checked<A: Allocator + Clone, F: Facts>(
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    facts: &F,
    plant: Plant,
    alloc: A,
) -> Result<Lowered<A>, Refusal> {
    if plant != Plant::None {
        return lower_planted(body, ck, facts, plant, alloc);
    }
    let (old, one, findings) = against::<A, F, OneRule>(body, ck, facts, alloc);
    judge(&findings);
    if Production::ONE { one } else { old }
}

/// [`lower_checked`] for the body of closure `closure` ([`crate::lower_closure_external`]).
#[allow(dead_code, reason = "no fixture helper lowers a closure's own body yet (oracle test the_oracle_sees_a_closure_bodys_calls compares one through closure_against)")]
pub(crate) fn lower_closure_checked<A: Allocator + Clone, F: Facts>(
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    closure: ExprId,
    facts: &F,
    alloc: A,
) -> Result<ClosureFlow<A>, Refusal> {
    let (old, one, findings) = closure_against::<A, F, OneRule>(body, ck, closure, facts, alloc);
    judge(&findings);
    if Production::ONE { one } else { old }
}

/// [`lower_checked`] for an `async fn`'s body ([`crate::lower_async_fn`]).
#[allow(dead_code, reason = "no fixture lowers an async fn's own body yet; the entry its first one calls")]
pub(crate) fn lower_async_fn_checked<A: Allocator + Clone, F: Facts>(
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    facts: &F,
    alloc: A,
) -> Result<Lowered<A>, Refusal> {
    let (mut lo, mut ln) = (None, None);
    let old = lower_async_fn_with::<A, F, OldPaths>(body, ck, facts, true, alloc.clone(), &mut lo);
    let one = lower_async_fn_with::<A, F, OneRule>(body, ck, facts, true, alloc, &mut ln);
    judge(&logs(&lo, &ln));
    if Production::ONE { one } else { old }
}

/// [`lower_checked`] for async block `block` ([`crate::lower_async`]).
pub(crate) fn lower_async_checked<A: Allocator + Clone, F: Facts>(
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    block: ExprId,
    facts: &F,
    alloc: A,
) -> Result<LoweredAsync<A>, Refusal> {
    let (mut lo, mut ln) = (None, None);
    let old = lower_async_with::<A, F, OldPaths>(body, ck, block, facts, true, alloc.clone(), &mut lo);
    let one = lower_async_with::<A, F, OneRule>(body, ck, block, facts, true, alloc.clone(), &mut ln);
    judge(&logs(&lo, &ln));
    let production = if Production::ONE { one } else { old };
    // W1's oracle (typeck.md "Witness records"): wherever production's lowering succeeds, the
    // witness lowering succeeds too, with equal statements and equal saved locals.
    if let Ok(p) = &production {
        let w = match crate::coroutine::lower_async_witness(body, ck, block, facts, alloc.clone()) {
            Ok(w) => w,
            Err(r) => panic!("the witness lowering refused {r:?} where production's lowered async block {block:?}"),
        };
        if let Some(d) = witness_differs(p, w.lowered_for_oracle(), alloc) {
            panic!("the witness of async block {block:?} differs from production's lowering: {d}");
        }
    }
    production
}

/// The first difference between production's lowering `p` and the witness's `w` of one async
/// block, origins dropped: the locals and their types, wraps and identities, the statements,
/// blocks, operands, successors, places, loans (place, kind, statement) and suspensions, and
/// the saved bits. `None`: equal.
pub(crate) fn witness_differs<A: Allocator + Clone>(p: &LoweredAsync<A>, w: &LoweredAsync<A>, alloc: A) -> Option<String> {
    let (pf, wf) = (&p.flow, &w.flow);
    if (pf.locals, pf.args) != (wf.locals, wf.args) {
        return Some(std::format!("locals/args {:?} vs {:?}", (pf.locals, pf.args), (wf.locals, wf.args)));
    }
    if p.local_ty[..] != w.local_ty[..] || p.local_wrap[..] != w.local_wrap[..] || p.identity[..] != w.identity[..] {
        return Some(String::from("a local's type, wrap or identity"));
    }
    if p.point_of[..] != w.point_of[..] {
        return Some(String::from("the suspensions"));
    }
    if pf.stmts.len() != wf.stmts.len() {
        return Some(std::format!("{} statements vs {}", pf.stmts.len(), wf.stmts.len()));
    }
    if let Some(i) = (0..pf.stmts.len()).find(|&i| pf.stmts[i] != wf.stmts[i]) {
        return Some(std::format!("statement {i}: {:?} vs {:?}", pf.stmts[i], wf.stmts[i]));
    }
    if pf.blocks[..] != wf.blocks[..] || pf.operands[..] != wf.operands[..] || pf.succ[..] != wf.succ[..] {
        return Some(String::from("the blocks, operands or successors"));
    }
    if pf.places[..] != wf.places[..] || pf.local_places[..] != wf.local_places[..] {
        return Some(String::from("the places"));
    }
    if pf.loans.len() != wf.loans.len()
        || pf.loans.iter().zip(wf.loans.iter()).any(|(a, b)| (a.place, a.kind, a.at) != (b.place, b.kind, b.at))
    {
        return Some(String::from("the loans"));
    }
    let (ps, ws) = match (blaze_flow::saved(pf, alloc.clone()), blaze_flow::saved(wf, alloc)) {
        (Ok(a), Ok(b)) => (a, b),
        _ => return Some(String::from("the saved analysis past its bound")),
    };
    if ps.bits[..] != ws.bits[..] || ps.suspends != ws.suspends {
        return Some(std::format!("saved {:?} vs {:?}", &ps.bits[..], &ws.bits[..]));
    }
    None
}

/// A failing finding panics with its message ([`describe`]); a reported one is printed; the
/// hub allowance is counted and printed.
fn judge(findings: &[Finding]) {
    for f in findings.iter() {
        let text = describe(f);
        if f.fails() {
            panic!("call-rule oracle (D156): {text}");
        }
        std::eprintln!("call-rule oracle (D156), reported: {text}");
    }
    let hub = findings.iter().filter(|f| matches!(f, Finding::HubMorePrecise { .. })).count();
    if hub > 0 {
        std::eprintln!("call-rule oracle (D156): hub: one rule is more precise at {hub} calls");
    }
}

/// Two lowerings' logs compared, when both kept one.
fn logs<A: Allocator>(old: &Option<Log<A>>, one: &Option<Log<A>>) -> vec::Vec<Finding> {
    match (old, one) {
        (Some(o), Some(n)) => compare(o, n),
        _ => vec::Vec::new(),
    }
}

/// [`against`] for the body of closure `closure`.
pub(crate) fn closure_against<A: Allocator + Clone, F: Facts, R: CallRule>(
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    closure: ExprId,
    facts: &F,
    alloc: A,
) -> (Result<ClosureFlow<A>, Refusal>, Result<ClosureFlow<A>, Refusal>, vec::Vec<Finding>) {
    let (mut lo, mut ln) = (None, None);
    let old = lower_closure_with::<A, F, OldPaths>(body, ck, closure, facts, true, alloc.clone(), &mut lo);
    let one = lower_closure_with::<A, F, R>(body, ck, closure, facts, true, alloc, &mut ln);
    let findings = logs(&lo, &ln);
    (old, one, findings)
}

/// `body` lowered under the old paths and under `R`, both logged, and their findings.
pub(crate) fn against<A: Allocator + Clone, F: Facts, R: CallRule>(
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    facts: &F,
    alloc: A,
) -> (Result<Lowered<A>, Refusal>, Result<Lowered<A>, Refusal>, vec::Vec<Finding>) {
    let (old, old_log) = lower_with::<A, F, OldPaths>(body, ck, facts, Plant::None, true, alloc.clone());
    let (one, one_log) = lower_with::<A, F, R>(body, ck, facts, Plant::None, true, alloc);
    let findings = logs(&old_log, &one_log);
    (old, one, findings)
}

/// The two logs compared per call site, paired by expression and occurrence.
pub(crate) fn compare<A: Allocator>(old: &Log<A>, one: &Log<A>) -> vec::Vec<Finding> {
    let mut out = vec::Vec::new();
    let mut seen: BTreeMap<ExprId, usize> = BTreeMap::new();
    for o in old.sites.iter() {
        let nth = seen.entry(o.e).or_insert(0);
        let k = *nth;
        *nth += 1;
        let Some(n) = one.sites.iter().filter(|s| s.e == o.e).nth(k) else { continue };
        let callee = o.callee;
        match (o.outcome, n.outcome) {
            (Err(_), Err(_)) => {}
            (Err(old_r), Ok(())) => {
                if !allowed(old_r.form) {
                    out.push(Finding::OneAcceptsOldRefused { e: o.e, callee, old: old_r, one: pairs(one, n) });
                }
            }
            (Ok(()), Err(one_r)) => out.push(Finding::OneRefuses { e: o.e, callee, old: pairs(old, o), one: one_r }),
            (Ok(()), Ok(())) => {
                let a = pairs(old, o);
                let b = pairs(one, n);
                // `x: 'static` in the one rule's set derives `x: y` for every `y` ([`pairs`]).
                let missing: Pairs = a.iter().filter(|p| !b.contains(p) && !b.contains(&(p.0, Label::Uni(Region::Static)))).copied().collect();
                match (missing.is_empty(), o.hub) {
                    (true, _) => {}
                    (false, true) => out.push(Finding::HubMorePrecise { e: o.e, callee, old: a, one: b, missing }),
                    (false, false) => out.push(Finding::Fewer { e: o.e, callee, old: a, one: b, missing }),
                }
            }
        }
    }
    out
}

/// The most origins one call's reach visits (rule 18).
const MOST_REACH: usize = 1 << 16;

/// A site's normalized set: each labeled origin's reach over the call's own subsets, closed
/// under `'static`'s meaning: `'static: y` holds of every `y`, so no pair from `'static` is
/// kept (a rule that emits it adds nothing: blaze-borrow relate.rs `outlives` emits nothing for
/// it), and `x` reaching `'static` is the pair `(x, 'static)`, from which [`compare`] derives
/// `x: y` for every `y`.
fn pairs<A: Allocator>(log: &Log<A>, s: &Site<A>) -> Pairs {
    let edges = log.edges.get(s.first as usize..s.end as usize).unwrap_or(&[]);
    let mut next: BTreeMap<Origin, vec::Vec<Origin>> = BTreeMap::new();
    for &(a, b) in edges.iter() {
        next.entry(a).or_default().push(b);
    }
    let mut out = Pairs::new();
    for &(from, x) in s.labels.iter() {
        if x == Label::Uni(Region::Static) {
            continue;
        }
        let mut reached: BTreeSet<Origin> = BTreeSet::new();
        let mut stack = vec![from];
        while let Some(o) = stack.pop() {
            assert!(reached.len() < MOST_REACH, "call-rule oracle: a call's reach passed {MOST_REACH} origins");
            for &t in next.get(&o).map(|v| v.as_slice()).unwrap_or(&[]) {
                if reached.insert(t) {
                    stack.push(t);
                }
            }
        }
        for &(to, y) in s.labels.iter() {
            if x != y && to != from && reached.contains(&to) {
                out.insert((x, y));
            }
        }
    }
    out
}

/// The argument index a pair is about, if one end is an argument.
fn arg_of(p: &(Label, Label)) -> Option<u32> {
    match p {
        (Label::Arg { arg, .. }, _) | (_, Label::Arg { arg, .. }) => Some(*arg),
        _ => None,
    }
}

/// The failure message: the callee, the argument(s) the difference is about, both sets and
/// both answers.
pub(crate) fn describe(f: &Finding) -> String {
    match f {
        Finding::OneAcceptsOldRefused { e, callee, old, one } => std::format!(
            "(a) the one rule accepts a call the old paths refused, and no fixture names it: callee {callee:?} at expr {e:?}; old: refused {old:?}; one rule: accepted with {one:?}"
        ),
        Finding::Fewer { e, callee, old, one, missing } => {
            let args: BTreeSet<u32> = missing.iter().filter_map(arg_of).collect();
            std::format!(
                "(b) the one rule emits fewer constraints: callee {callee:?} at expr {e:?}, argument(s) {args:?}; missing {missing:?}; old (accepted): {old:?}; one rule (accepted): {one:?}"
            )
        }
        Finding::OneRefuses { e, callee, old, one } => std::format!(
            "(c) the old paths accept and the one rule refuses: callee {callee:?} at expr {e:?}; old (accepted): {old:?}; one rule: refused {one:?}"
        ),
        Finding::HubMorePrecise { e, callee, old, one, missing } => {
            let args: BTreeSet<u32> = missing.iter().filter_map(arg_of).collect();
            std::format!(
                "hub allowance: the old paths related callee {callee:?} at expr {e:?} through one hub origin; the one rule drops {missing:?} (argument(s) {args:?}); old: {old:?}; one rule: {one:?}"
            )
        }
    }
}

#[cfg(test)]
mod checks {
    use std::vec;

    use blaze_typeck::fixture::{Bb, Fx};
    use blaze_typeck::{ItemKey, Mutbl, Name};

    use super::{Finding, against, closure_against};
    use crate::one_call::{CallRule, OneRule, OneRuleDropOne};
    use crate::region_fixture::{self, Decls, FnHead, P0, WithRegions};
    use crate::{AdtFacts, CallBounds, CalleeFacts, Facts};

    struct World;

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
            Some(AdtFacts { copy: false, drop_impl: false, needs_drop: false, lifetimes: 0, union: false, is_enum: false, is_box: false })
        }
        fn callee(&self, _: ItemKey) -> Option<CalleeFacts> {
            Some(CalleeFacts { writes_lifetimes: false, self_ref: false })
        }
        fn call_bounds(&self, _: ItemKey) -> Option<CallBounds<'_>> {
            Some(CallBounds { outlives: &[], unread_below: 0, trait_bounded: &[] })
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

    /// `fn pick(x: &S) -> &S` called as `fn f(s: &S) -> &S { pick(s) }`, compared against `R`.
    fn findings_of<R: CallRule>() -> vec::Vec<Finding> {
        let mut fx = Fx::new();
        let s = fx.strukt("S", 0, &[]);
        let st = fx.adt(s, &[]);
        let rs = fx.r(Mutbl::Not, st);
        let pick = fx.func("pick", 0, &[rs], rs, vec![]);
        let f = fx.func("f", 0, &[rs], rs, vec![]);
        fx.finish();
        let mut decls = Decls::new();
        decls.func(pick, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[rs], output: rs, slots: &[P0, P0] });
        decls.func(f, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[rs], output: rs, slots: &[P0, P0] });
        let mut b = Bb::new();
        let ps = b.param("s");
        let callee = b.path(&["pick"]);
        let arg = b.local(ps);
        let call = b.call(callee, &[arg]);
        let root = b.block(&[], Some(call));
        let view = b.view(f, root);
        let mut c = region_fixture::checked(&fx, &view);
        let regions = decls.lay(fx.t.view(), &mut c.types);
        let with = WithRegions { base: &World, regions: &regions };
        let (old, one, findings) = against::<_, _, R>(&view, &c, &with, region_fixture::alloc());
        assert!(old.is_ok(), "the old paths lower the fixture: {:?}", old.err());
        assert!(one.is_ok(), "the one rule lowers the fixture: {:?}", one.err());
        findings
    }

    /// THE PLANTED CONTROL (rule 23): the adapter variant that drops one subset per call loses
    /// `s`'s reach into the result, and the oracle names it under (b). CONTROL: the real one
    /// rule over the same body has no finding.
    #[test]
    fn the_oracle_catches_a_dropped_constraint() {
        let planted = findings_of::<OneRuleDropOne>();
        std::println!("oracle planted: {planted:?}");
        assert!(planted.iter().any(|f| matches!(f, Finding::Fewer { .. })), "CONTROL MISSED: a dropped constraint passed the oracle: {planted:?}");
        let real = findings_of::<OneRule>();
        std::println!("oracle real: {real:?}");
        assert!(real.is_empty(), "the one rule differs from the old paths on a plain elided call: {real:?}");
    }

    /// `fn pick(x: &S) -> &S` called in a closure's body, `fn f(s: S) { let c = || { pick(&s);
    /// }; }`, the closure's body lowered under both rules ([`closure_against`]) and compared
    /// against `R`.
    fn closure_findings_of<R: CallRule>() -> vec::Vec<Finding> {
        let mut fx = Fx::new();
        let s = fx.strukt("S", 0, &[]);
        let st = fx.adt(s, &[]);
        let rs = fx.r(Mutbl::Not, st);
        let unit = fx.unit();
        let pick = fx.func("pick", 0, &[rs], rs, vec![]);
        let f = fx.func("f", 0, &[st], unit, vec![]);
        fx.finish();
        let mut decls = Decls::new();
        decls.func(pick, FnHead { lifetimes: 1, late: &[0], outlives: &[], inputs: &[rs], output: rs, slots: &[P0, P0] });
        decls.func(f, FnHead { lifetimes: 0, late: &[], outlives: &[], inputs: &[st], output: unit, slots: &[] });
        let mut b = Bb::new();
        let ps = b.param("s");
        let callee = b.path(&["pick"]);
        let ls = b.local(ps);
        let arg = b.borrow(Mutbl::Not, ls);
        let call = b.call(callee, &[arg]);
        let st1 = b.semi(call);
        let inner = b.block(&[st1], None);
        let cl = b.closure(&[], inner);
        let pc = b.bind("c");
        let made = b.let_(pc, None, cl);
        let root = b.block(&[made], None);
        let view = b.view(f, root);
        let mut c = region_fixture::checked(&fx, &view);
        let regions = decls.lay(fx.t.view(), &mut c.types);
        let with = WithRegions { base: &World, regions: &regions };
        let (old, one, findings) = closure_against::<_, _, R>(&view, &c, cl, &with, region_fixture::alloc());
        assert!(old.is_ok(), "the old paths lower the closure's body: {:?}", old.err().map(|r| r.form));
        assert!(one.is_ok(), "the one rule lowers the closure's body: {:?}", one.err().map(|r| r.form));
        findings
    }

    /// A CLOSURE'S BODY IS COMPARED: the planted control (one subset dropped per call) is caught
    /// at the call inside the closure under (b); the real one rule there has no finding.
    #[test]
    fn the_oracle_sees_a_closure_bodys_calls() {
        let planted = closure_findings_of::<OneRuleDropOne>();
        std::println!("oracle closure planted: {planted:?}");
        assert!(planted.iter().any(|f| matches!(f, Finding::Fewer { .. })), "CONTROL MISSED: a closure body's call was not compared: {planted:?}");
        let real = closure_findings_of::<OneRule>();
        std::println!("oracle closure real: {real:?}");
        assert!(real.is_empty(), "the one rule differs from the old paths in a closure's body: {real:?}");
    }
}
