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
//! outlives result is a relation its owner would have to prove (the closure requirements, not
//! built): the caller files it as a refusal by name, never as a finding.

use blaze_flow::Flow;
use blaze_flow_lower::{
    AdtFacts, CallBounds, CalleeFacts, Facts, ParamOrigin, Refusal, Shape, SigLife, lower, lower_async, lower_async_fn, lower_closure,
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
    /// Whether an outlives result of this flow is the owner's to prove (a nested body's).
    pub fn is_nested(self) -> bool {
        self != FlowOf::Owner
    }
}

/// The body's facts, saying that closure and async block bodies are checked on their own
/// (module doc: only `borrows_of`, which checks each, asks with these).
pub struct NestedFacts<'f, 'a> {
    facts: &'f BorrowFacts<'a>,
}

impl<'f, 'a> NestedFacts<'f, 'a> {
    pub fn new(facts: &'f BorrowFacts<'a>) -> NestedFacts<'f, 'a> {
        NestedFacts { facts }
    }
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
    fn param_copy(&self, index: u32) -> bool {
        self.facts.param_copy(index)
    }
    fn shape(&self, item: tc::ItemKey) -> Option<Shape<'_>> {
        self.facts.shape(item)
    }
    fn call_bounds(&self, item: tc::ItemKey) -> Option<CallBounds<'_>> {
        self.facts.call_bounds(item)
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
