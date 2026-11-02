//! The parts of the inliner that need no database: the walk of the call graph, where a
//! `#[track_caller]` span is reported, what an intrinsic call costs and which ABIs unwind.
//! What it inlines from fixtures is `fixtures.rs`, an integration test.

use blaze_hash::HashSet;
use hir_def::hir::ExprId;
use la_arena::RawIdx;
use rustc_abi::ExternAbi;
use smallvec::SmallVec;

use crate::{
    intrinsic::Intrinsic,
    mir::{Inlined, InlinedScope, InlinedSpan, MirSpan, optimize::abi_can_unwind},
};

use super::{
    cost::{Call, classify},
    cycle::{Walked, walk},
    spans::{Site, caller_site},
};

/// The walk over `graph`, a node's callees by its index, every node with MIR and its own body.
fn walk_from(
    graph: &[&[u32]],
    root: u32,
    local: impl Fn(u32) -> bool,
) -> Option<Walked<u32>> {
    let callees = |node: u32| -> SmallVec<[(u32, bool); 8]> {
        graph[node as usize].iter().map(|&it| (it, true)).collect()
    };
    walk(root, root, 16, |node| node, callees, local)
}

fn set(of: &[u32]) -> HashSet<u32> {
    of.iter().copied().collect()
}

#[test]
fn a_recursive_pair_stops_and_is_cyclic() {
    // 0 calls 1, and 1 and 2 call each other.
    let graph: &[&[u32]] = &[&[1], &[2], &[1]];
    let from_1 = walk_from(graph, 1, |_| true).expect("the walk ends");
    assert_eq!(from_1.cyclic, set(&[2]));
    assert_eq!(from_1.reached, set(&[2]));
    let from_0 = walk_from(graph, 0, |_| true).expect("the walk ends");
    assert_eq!(from_0.cyclic, set(&[]));
    assert_eq!(from_0.reached, set(&[1, 2]));
}

#[test]
fn a_body_met_again_says_what_it_reached() {
    // 0 calls 1 and 2, both call 3, and 3 calls 0: the second path to 3 reuses the first.
    let graph: &[&[u32]] = &[&[1, 2], &[3], &[3], &[0]];
    let walked = walk_from(graph, 0, |_| true).expect("the walk ends");
    assert_eq!(walked.cyclic, set(&[1, 2, 3]));
    let walked = walk_from(graph, 0, |it| it != 3).expect("the walk ends");
    assert_eq!(walked.cyclic, set(&[1, 2]), "a body of another crate is not kept");
}

#[test]
fn a_callee_without_mir_is_not_followed() {
    let callees = |node: u32| -> SmallVec<[(u32, bool); 8]> {
        match node {
            0 => [(1, false)].into_iter().collect(),
            _ => [(0, true)].into_iter().collect(),
        }
    };
    let walked = walk(0, 0, 16, |node| node, callees, |_| true).expect("the walk ends");
    assert_eq!(walked.cyclic, set(&[]));
    assert_eq!(walked.reached, set(&[]));
}

#[test]
fn polymorphic_recursion_ends_the_walk_at_the_limit() {
    // The body 1 calls itself with ever new arguments, as `f::<T>` calling `f::<Vec<T>>`.
    let callees = |(body, depth): (u32, u32)| -> SmallVec<[((u32, u32), bool); 8]> {
        match body {
            0 => [((1, 0), true)].into_iter().collect(),
            _ => [((1, depth + 1), true)].into_iter().collect(),
        }
    };
    assert_eq!(walk((0, 0), 0, 16, |node| node.0, callees, |_| true), None);
}

fn expr(index: u32) -> MirSpan {
    MirSpan::ExprId(ExprId::from_raw(RawIdx::from_u32(index)))
}

/// A body that inlined the callee 0 at `expr(1)`, which had inlined the callee 1 at
/// `expr(2)`, whose code at `expr(3)` is the span `MirSpan::Inlined(0)`.
fn nested() -> Inlined<'static> {
    Inlined {
        scopes: vec![
            InlinedScope { callee: 0, call_site: expr(1) },
            InlinedScope { callee: 1, call_site: MirSpan::Inlined(1) },
        ],
        spans: vec![
            InlinedSpan { scope: 1, span: expr(3) },
            InlinedSpan { scope: 0, span: expr(2) },
        ],
        ..Inlined::default()
    }
}

#[test]
fn a_span_of_an_inlined_callee_is_the_callee_s() {
    let site = caller_site(&nested(), MirSpan::Inlined(0), |_| false);
    assert_eq!(site, Site::Callee(1, expr(3)));
}

#[test]
fn track_caller_reports_the_call_it_was_inlined_at() {
    let inlined = nested();
    assert_eq!(caller_site(&inlined, MirSpan::Inlined(0), |it| it == 1), Site::Callee(0, expr(2)));
    assert_eq!(caller_site(&inlined, MirSpan::Inlined(0), |_| true), Site::Own(expr(1)));
    assert_eq!(caller_site(&inlined, expr(4), |_| true), Site::Own(expr(4)));
}

#[test]
fn lowered_intrinsics_cost_what_rustc_s_statements_do() {
    assert_eq!(classify(None), Call::Function);
    assert_eq!(classify(Some(Intrinsic::UbChecks)), Call::UbChecks);
    assert_eq!(classify(Some(Intrinsic::Unreachable)), Call::Unreachable);
    assert_eq!(classify(Some(Intrinsic::Assume)), Call::Lowered { cost: 5, statements: 1 });
    let copy = Call::Lowered { cost: 25, statements: 1 };
    assert_eq!(classify(Some(Intrinsic::CopyNonoverlapping)), copy);
    let write_box = Call::Lowered { cost: 10, statements: 2 };
    assert_eq!(classify(Some(Intrinsic::WriteBoxViaMove)), write_box);
    assert_eq!(classify(Some(Intrinsic::SizeOf)), Call::Intrinsic);
}

#[test]
fn only_unwinding_abis_unwind() {
    assert!(abi_can_unwind(ExternAbi::Rust));
    assert!(abi_can_unwind(ExternAbi::RustCall));
    assert!(abi_can_unwind(ExternAbi::C { unwind: true }));
    assert!(!abi_can_unwind(ExternAbi::C { unwind: false }));
    assert!(!abi_can_unwind(ExternAbi::System { unwind: false }));
}
