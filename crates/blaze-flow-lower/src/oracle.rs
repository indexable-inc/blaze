//! Fixture lowering helpers (D156, post-flip): every fixture body lowers under production's
//! single call rule (`OneRule`), and every async block is cross-checked against its region-erased
//! witness (`lower_async_witness`, `witness_differs`).

use core::alloc::Allocator;
use std::string::String;

use blaze_typeck::{BodyView, Checked, ExprId};

use crate::closure::{lower_async_fn, lower_closure_external};
use crate::coroutine::lower_async;
use crate::{ClosureFlow, Facts, Form, Lowered, LoweredAsync, Plant, Refusal, lower_planted};

/// No-op compatibility wrapper kept for fixture tests that historically annotated old-path
/// refusals accepted by `OneRule`.
#[allow(dead_code)]
pub(crate) fn allowing<T>(_forms: &[Form], f: impl FnOnce() -> T) -> T {
    f()
}

/// Lowers `body` under production's `OneRule` call lowering.
pub(crate) fn lower_checked<A: Allocator + Copy, F: Facts>(
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    facts: &F,
    plant: Plant,
    alloc: A,
) -> Result<Lowered<A>, Refusal> {
    lower_planted(body, ck, facts, plant, alloc)
}

/// [`lower_checked`] for the body of closure `closure` ([`crate::lower_closure_external`]).
#[allow(dead_code)]
pub(crate) fn lower_closure_checked<A: Allocator + Copy, F: Facts>(
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    closure: ExprId,
    facts: &F,
    alloc: A,
) -> Result<ClosureFlow<A>, Refusal> {
    lower_closure_external(body, ck, closure, facts, alloc)
}

/// [`lower_checked`] for an `async fn`'s body ([`crate::lower_async_fn`]).
#[allow(dead_code)]
pub(crate) fn lower_async_fn_checked<A: Allocator + Copy, F: Facts>(
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    facts: &F,
    alloc: A,
) -> Result<Lowered<A>, Refusal> {
    lower_async_fn(body, ck, facts, alloc)
}

/// [`lower_checked`] for async block `block` ([`crate::lower_async`]), cross-checked against
/// the region-erased witness lowering ([`crate::coroutine::lower_async_witness`]).
pub(crate) fn lower_async_checked<A: Allocator + Copy, F: Facts>(
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    block: ExprId,
    facts: &F,
    alloc: A,
) -> Result<LoweredAsync<A>, Refusal> {
    let production = lower_async(body, ck, block, facts, alloc);
    if let Ok(p) = &production {
        let w = match crate::coroutine::lower_async_witness(body, ck, block, facts, alloc) {
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
pub(crate) fn witness_differs<A: Allocator + Copy>(p: &LoweredAsync<A>, w: &LoweredAsync<A>, alloc: A) -> Option<String> {
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
    let (ps, ws) = match (blaze_flow::saved(pf, alloc), blaze_flow::saved(wf, alloc)) {
        (Ok(a), Ok(b)) => (a, b),
        _ => return Some(String::from("the saved analysis past its bound")),
    };
    if ps.bits[..] != ws.bits[..] || ps.suspends != ws.suspends {
        return Some(std::format!("saved {:?} vs {:?}", &ps.bits[..], &ws.bits[..]));
    }
    None
}
