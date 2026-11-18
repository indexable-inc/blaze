//! The flows of one body for the move and borrow check (front2 borrows.rs `borrows_of`): the
//! body's own, then each closure's and each async block's body as a flow of its own
//! (blaze-flow-lower `lower_closure`, `lower_async`), so a body holding either is checked whole
//! instead of refused (`Form::Closure`, `Form::AsyncBlock`); an `async fn`'s own body is
//! lowered with its suspensions (`lower_async_fn`) instead of refused (`Form::Coroutine`).
//!
//! The Reference, "Closure expressions" and "Capture modes": a closure's captures are borrowed
//! or moved where the closure is made (the owner's flow: each capture a loan or a move at the
//! closure expression, held in the closure value, so live as long as it is), and its body runs
//! with them as its environment (the closure's flow: the captures as arguments of their modes).
//! "Async blocks": "Async blocks capture variables from their environment using the same
//! capture modes as closures"; "Await expressions": the block is suspended at each `.await`
//! and resumed there, so a loan held across one is live through the suspension.
//!
//! The owner's flow is asked with [`NestedFacts`], which says each closure's and async block's
//! body IS checked: true only because `borrows_of` lowers and checks every one of them, in
//! expression order ([`next_nested`]); a refusal of any is the body's refusal. A nested body's
//! outlives result is a relation its owner must prove: between two positions of the closure's
//! or the future's VALUE (or `'static`) it is a requirement the owner's flow emits at the
//! expression ([`Requirements`], closures and async blocks alike); any other is filed as a
//! refusal by name, never as a finding.

use blaze_flow::Flow;
use blaze_flow_lower::{
    AdtFacts, CallBounds, CalleeFacts, ClosureFlow, Facts, FieldRegion, ParamOrigin, Refusal, Requirement, Shape, SigLife, lower, lower_async,
    lower_async_fn, lower_closure, lower_closure_external,
};
use blaze_own::{AVec, Alloc};
use blaze_typeck as tc;

use super::borrows::BorrowFacts;

/// Which flow of a body: its own, or the body of one of its closures or async blocks (by the
/// expression that makes it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlowOf {
    Owner,
    Closure(tc::ExprId),
    AsyncBlock(tc::ExprId),
}

impl FlowOf {
    /// Whether an outlives result of this flow is the owner's to prove (a nested body's). The
    /// root block of an `async fn` (`view.root` of a body with a coroutine) is not: its
    /// placeholders are the fn's signature's (blaze-flow-lower coroutine.rs `async_fn_root`),
    /// so its outlives result is the fn's own finding, as a fn body's is.
    pub fn is_nested(self, view: &tc::BodyView<'_>) -> bool {
        match self {
            FlowOf::Owner => false,
            FlowOf::AsyncBlock(e) if view.coroutine.is_some() && e == view.root => false,
            FlowOf::Closure(_) | FlowOf::AsyncBlock(_) => true,
        }
    }
}

/// The body's facts, saying that closure and async block bodies are checked on their own
/// (module doc: only `borrows_of`, which checks each, asks with these).
pub struct NestedFacts<'f, 'a> {
    facts: &'f BorrowFacts<'a>,
    /// Each closure's requirements, from its own flow checked first ([`Requirements`]): two
    /// columns sorted by (closure, requirement), `needs[i]` closure `closures[i]`'s.
    closures: &'f [tc::ExprId],
    needs: &'f [Requirement],
}

impl<'f, 'a> NestedFacts<'f, 'a> {
    /// No closure's requirements known: what a caller that checks no closure first asks with.
    pub fn new(facts: &'f BorrowFacts<'a>) -> NestedFacts<'f, 'a> {
        NestedFacts { facts, closures: &[], needs: &[] }
    }

    /// With the requirements of every closure checked so far.
    pub fn with_requirements(facts: &'f BorrowFacts<'a>, requirements: &'f Requirements<'_>) -> NestedFacts<'f, 'a> {
        NestedFacts { facts, closures: &requirements.closures, needs: &requirements.needs }
    }
}

/// The closure requirements of one body (the NLL RFC, "Closures": the closure's creator must
/// prove the relations among the closure's external regions its body needs; an async block's
/// the same, keyed by the block's expression): each closure's and nested async block's flow is
/// checked BEFORE the flow that makes it (innermost first: [`nested_inner_first`]),
/// and each of its outlives results between two positions of the closure VALUE (or `'static`)
/// is mapped through blaze-flow-lower `requirement` into this table, which the making flow's
/// lowering emits at the closure expression (`Facts::closure_requirements`). A result that maps
/// to none (the environment's reference, the closure's signature) stays `NestedOutlives`.
pub struct Requirements<'a> {
    closures: AVec<'a, tc::ExprId>,
    needs: AVec<'a, Requirement>,
}

impl<'a> Requirements<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Requirements<'a> {
        Requirements { closures: AVec::new_in(alloc), needs: AVec::new_in(alloc) }
    }

    /// Files closure `closure`'s requirement `r` (each once, the columns kept sorted).
    pub fn add(&mut self, closure: tc::ExprId, r: Requirement) {
        let (lo, hi) = run_of(&self.closures, closure);
        let at = match self.needs[lo..hi].binary_search(&r) {
            Ok(_) => return,
            Err(i) => lo + i,
        };
        self.closures.insert(at, closure);
        self.needs.insert(at, r);
    }

    /// Closure `closure`'s requirements, in order.
    pub fn of(&self, closure: tc::ExprId) -> &[Requirement] {
        let (lo, hi) = run_of(&self.closures, closure);
        &self.needs[lo..hi]
    }
}

/// Where closure `closure`'s rows are in a sorted column of closures.
fn run_of(closures: &[tc::ExprId], closure: tc::ExprId) -> (usize, usize) {
    (closures.partition_point(|&e| e < closure), closures.partition_point(|&e| e <= closure))
}

/// Every nested flow of the body whose outlives results are its maker's to prove: each closure
/// (not `async`, whose flow is no closure's: `Form::AsyncClosure`) and each async block but an
/// `async fn`'s root ([`FlowOf::is_nested`]), innermost first: by the length of its range,
/// shorter first (one written inside another's body has a range strictly inside it), then by
/// expression. What [`Requirements`] is filled in the order of, so every maker is lowered with
/// the requirements of what it makes (lane B14: async blocks in the one mechanism of closures,
/// rule 30; a closure inside an async block, or an async block inside a closure, either way).
pub fn nested_inner_first<'a>(view: &tc::BodyView<'_>, alloc: Alloc<'a>) -> AVec<'a, FlowOf> {
    let mut out: AVec<'a, (u32, tc::ExprId)> = AVec::new_in(alloc);
    for (i, x) in view.exprs.iter().enumerate() {
        let e = tc::ExprId(i as u32);
        let nested = match x {
            tc::Expr::Closure { is_async: false, .. } => true,
            tc::Expr::AsyncBlock { .. } => FlowOf::AsyncBlock(e).is_nested(view),
            _ => false,
        };
        if nested {
            let len = view.expr_ranges.get(i).map_or(u32::MAX, |r| r.end - r.start);
            out.push((len, e));
        }
    }
    out.sort_unstable();
    let mut flows: AVec<'a, FlowOf> = AVec::new_in(alloc);
    for &(_, e) in out.iter() {
        flows.push(match view.exprs.get(e.ix()) {
            Some(tc::Expr::AsyncBlock { .. }) => FlowOf::AsyncBlock(e),
            _ => FlowOf::Closure(e),
        });
    }
    flows
}

/// A closure's flow with what each of its placeholders stands for in its owner
/// (blaze-flow-lower `lower_closure_external`): its requirements are mapped through these.
pub fn closure_flow_of<'a>(
    view: &tc::BodyView<'_>,
    done: &tc::Checked<'_>,
    closure: tc::ExprId,
    facts: &NestedFacts<'_, '_>,
    alloc: Alloc<'a>,
) -> Result<ClosureFlow<Alloc<'a>>, Refusal> {
    lower_closure_external(view, done, closure, facts, alloc)
}

impl Facts for NestedFacts<'_, '_> {
    fn adt(&self, adt: tc::ItemKey) -> Option<AdtFacts> {
        self.facts.adt(adt)
    }
    fn callee(&self, item: tc::ItemKey) -> Option<CalleeFacts> {
        self.facts.callee(item)
    }
    fn sig(&self, item: tc::ItemKey) -> Option<SigLife<'_>> {
        self.facts.sig(item)
    }
    fn own_text(&self) -> Option<&str> {
        self.facts.own_text()
    }
    fn fn_binders(&self) -> bool {
        self.facts.fn_binders()
    }
    fn closure_bodies_checked(&self) -> bool {
        true
    }
    fn async_bodies_checked(&self) -> bool {
        true
    }
    fn is_future(&self, ty: tc::PTy) -> Option<bool> {
        self.facts.is_future(ty)
    }
    fn field(&self, adt: tc::ItemKey, variant: u32, name: tc::Name) -> Option<u32> {
        self.facts.field(adt, variant, name)
    }
    fn field_count(&self, adt: tc::ItemKey, variant: u32) -> Option<u32> {
        self.facts.field_count(adt, variant)
    }
    fn field_param(&self, adt: tc::ItemKey, variant: u32, field: u32) -> Option<u32> {
        self.facts.field_param(adt, variant, field)
    }
    fn field_lead(&self, adt: tc::ItemKey, variant: u32, field: u32) -> Option<blaze_flow_lower::FieldLead> {
        self.facts.field_lead(adt, variant, field)
    }
    fn param_copy(&self, index: u32) -> bool {
        self.facts.param_copy(index)
    }
    fn shape(&self, item: tc::ItemKey) -> Option<Shape<'_>> {
        self.facts.shape(item)
    }
    fn call_bounds(&self, item: tc::ItemKey) -> Option<CallBounds<'_>> {
        self.facts.call_bounds(item)
    }
    fn object_trait_lifetimes(&self, trait_: tc::ItemKey) -> Option<bool> {
        self.facts.object_trait_lifetimes(trait_)
    }
    // Every fact the body's facts answer, forwarded (a nested flow asks what the owner's does).
    fn ties(&self, item: tc::ItemKey) -> Option<blaze_flow_lower::Ties<'_>> {
        self.facts.ties(item)
    }
    fn impl_source(&self, imp: tc::ItemKey) -> Option<blaze_flow_lower::ImplSource<'_>> {
        self.facts.impl_source(imp)
    }
    fn adt_variances(&self, adt: tc::ItemKey) -> Option<&[blaze_flow_lower::Variance]> {
        self.facts.adt_variances(adt)
    }
    fn field_regions(&self, adt: tc::ItemKey, variant: u32, field: u32) -> Option<&[FieldRegion]> {
        self.facts.field_regions(adt, variant, field)
    }
    fn copy_args(&self, adt: tc::ItemKey) -> Option<&[bool]> {
        self.facts.copy_args(adt)
    }
    fn closure_requirements(&self, closure: tc::ExprId) -> &[Requirement] {
        let (lo, hi) = run_of(self.closures, closure);
        self.needs.get(lo..hi).unwrap_or(&[])
    }
}

/// The item-relative range of the closure or async block `of` is the flow of, when it is a
/// nested one (`FlowOf::is_nested`); `None` for the owner's flow: where a refusal of that flow
/// is (borrows.rs `BorrowVerdict::within`).
pub fn nested_range(view: &tc::BodyView<'_>, of: FlowOf) -> Option<(u32, u32)> {
    match of {
        FlowOf::Closure(e) | FlowOf::AsyncBlock(e) if of.is_nested(view) => view.expr_ranges.get(e.ix()).map(|r| (r.start, r.end)),
        _ => None,
    }
}

/// The flow after `at` in the body's order: the owner's first, then each closure and async block
/// in expression order. `None` after the last. One pass over the expressions in all.
pub fn next_nested(view: &tc::BodyView<'_>, at: FlowOf) -> Option<FlowOf> {
    let from = match at {
        FlowOf::Owner => 0,
        FlowOf::Closure(e) | FlowOf::AsyncBlock(e) => e.ix() + 1,
    };
    for i in from..view.exprs.len() {
        let e = tc::ExprId(i as u32);
        match view.exprs[i] {
            tc::Expr::Closure { .. } => return Some(FlowOf::Closure(e)),
            tc::Expr::AsyncBlock { .. } => return Some(FlowOf::AsyncBlock(e)),
            _ => {}
        }
    }
    None
}

/// Lowers flow `of` of the body: the owner's (an `async fn`'s with its suspensions), a closure's
/// or an async block's.
pub fn flow_of<'a>(
    of: FlowOf,
    view: &tc::BodyView<'_>,
    done: &tc::Checked<'_>,
    facts: &NestedFacts<'_, '_>,
    alloc: Alloc<'a>,
) -> Result<(Flow<Alloc<'a>>, AVec<'a, ParamOrigin>), Refusal> {
    // With each flow, its placeholders for the body's type parameters (blaze-flow-lower
    // param_outlives.rs): an `UnprovenOutlives` from one is E0310 or a refusal by name. An async
    // block's lowering makes none.
    match of {
        FlowOf::Owner if view.coroutine.is_some() => lower_async_fn(view, done, facts, alloc).map(|l| (l.flow, l.params)),
        FlowOf::Owner => lower(view, done, facts, alloc).map(|l| (l.flow, l.params)),
        FlowOf::Closure(e) => lower_closure(view, done, e, facts, alloc).map(|l| (l.flow, l.params)),
        FlowOf::AsyncBlock(e) => lower_async(view, done, e, facts, alloc).map(|l| (l.flow, AVec::new_in(alloc))),
    }
}
