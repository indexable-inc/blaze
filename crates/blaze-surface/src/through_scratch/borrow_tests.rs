//! A [`ThroughScratch`] kept across walks borrows what it walks and answers as a fresh one
//! (lane S4, 2026-11-18):
//!
//! - `a_held_scratch_answers_as_a_fresh_one`: one scratch walks several worlds, forward and then
//!   backward, and every answer equals [`crate::through`]'s (a fresh scratch per lookup, the
//!   oracle); the control plants a scratch whose values are NOT cleared between walks
//!   ([`ThroughScratch::plant_stale_values`]), and the oracle must catch it.
//! - `the_scratch_copies_no_edge_and_no_chain`: over a counting allocator, a walk of nodes with
//!   chains of 64 modules holds exactly the bytes of the same walk with chains of 1 (no chain
//!   copied), a walk with four times the edges holds more only by the edges' slots (no edge
//!   copied), and a warm walk allocates nothing; the scratch outlives each world it walked.

use std::{format, println};

use blaze_own::testing::Counting;
use blaze_own::{AVec, Alloc, Erased, Vec};
use blaze_sim::{Broke, Config, Tape};

use super::tests::{K1, Plant, Reader, Spec, draw, heap, item, reference};
use super::{ThroughScratch, through_in};
use crate::kind::{BindingState, Def, Visibility};
use crate::module_scope::GlobEdge;
use crate::through::{Passed, through};

/// The scratch as the test holds it: cleared per walk (the code as it is), or the plant.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Held {
    Cleared,
    StaleValues,
}

/// One case: two to four worlds, each walked through one held scratch, forward then backward,
/// each answer against a fresh scratch's (and the reference walk's, so the oracle is itself
/// checked).
fn held_agrees(tape: &Tape, held: Held) -> Result<(), Broke> {
    let count = 2 + tape.below(3, "worlds") as usize;
    let mut worlds: AVec<'static, AVec<'static, Spec>> = Vec::new_in(heap());
    for _ in 0..count {
        worlds.push(draw(tape));
    }
    let root = Def::module(item(0));
    let mut scratch = ThroughScratch::new_in(heap());
    if held == Held::StaleValues {
        scratch.plant_stale_values();
    }
    for pass in 0..2 {
        for k in 0..count {
            let at = if pass == 0 { k } else { count - 1 - k };
            let graph = worlds[at].as_slice();
            let fresh = through(&mut Reader::new(graph), root, heap());
            let old = reference(&mut Reader::new(graph), root, heap(), Plant::None);
            if fresh != old {
                return Err(Broke::Harness(format!("the fresh scratch differs from the reference walk: world {at}: {fresh:?} against {old:?}")));
            }
            let got = through_in(&mut Reader::new(graph), root, &mut scratch);
            if got != fresh {
                return Err(Broke::Oracle(format!(
                    "the held scratch differs from a fresh one: world {at} of {count}, pass {pass}: {got:?} against {fresh:?}"
                )));
            }
        }
    }
    Ok(())
}

/// Every answer through a held scratch equals a fresh scratch's, in both orders of walking; the
/// oracle catches a held scratch whose values are not cleared between walks. Expected:
/// `test through_scratch::borrow_tests::a_held_scratch_answers_as_a_fresh_one ... ok`, the
/// control's observe block naming `StaleValues` caught.
#[test]
fn a_held_scratch_answers_as_a_fresh_one() {
    blaze_sim::control(Config::cases(300), Held::Cleared, Held::StaleValues, "differs from a fresh one", held_agrees);
}

/// Nine glob nodes, node `i` globbing nodes `i + 1` and `i + 2` (mod 9), node 8 also the value
/// node 9; every edge written `repeat` times (the same source again: the same reached set), every
/// chain `chain` modules long (the node, then modules no visibility names).
fn ring(chain: u32, repeat: u32) -> AVec<'static, Spec> {
    const NODES: u32 = 9;
    let mut graph: AVec<'static, Spec> = Vec::new_in(heap());
    for i in 0..NODES {
        let mut edges: AVec<'static, GlobEdge> = Vec::new_in(heap());
        let mut targets: AVec<'static, u32> = Vec::new_in(heap());
        targets.push((i + 1) % NODES);
        targets.push((i + 2) % NODES);
        if i == NODES - 1 {
            targets.push(NODES);
        }
        for _ in 0..repeat {
            for &to in targets.iter() {
                edges.push(GlobEdge { source: Def::module(item(to)), vis: Visibility::Public, expansion: 0 });
            }
        }
        let mut modules: AVec<'static, blaze_id::ItemKey> = Vec::new_in(heap());
        modules.push(item(i));
        for j in 1..chain {
            modules.push(item(500_000 + j));
        }
        graph.push(Spec::Edges { edges, krate: K1, chain: modules });
    }
    graph.push(Spec::Value(Some(found())));
    graph
}

fn found() -> Passed {
    Passed { def: Def::module(item(300_000)), vis: Visibility::Public, state: BindingState::Determined }
}

/// What one cold walk of `graph` leaves held in a fresh scratch over a counting allocator: the
/// live bytes, the edge slots' capacity, and the answer.
fn cold(graph: &[Spec]) -> (usize, usize, bool) {
    let counting = Erased::new(Counting::new(heap()));
    let alloc: Alloc<'_> = counting.handle();
    let mut scratch = ThroughScratch::new_in(alloc);
    let got = through_in(&mut Reader::new(graph), Def::module(item(0)), &mut scratch);
    let live = counting.inner().live_bytes();
    let slots = scratch.capacities()[3];
    (live, slots, got == Ok(Some(found())))
}

/// Floor: 0 bytes of a node's edges or chain copied into the scratch; 0 allocations on a warm
/// walk. Expected: `test through_scratch::borrow_tests::the_scratch_copies_no_edge_and_no_chain
/// ... ok`, printing `through scratch: walks 4, scratches made 1, bytes copied 0 (chains of 64
/// hold 0 bytes more than chains of 1; 4x the edges hold only their slots' bytes more), growth
/// on the warm pass 0`.
#[test]
fn the_scratch_copies_no_edge_and_no_chain() {
    let short = ring(1, 1);
    let long = ring(64, 1);
    let many = ring(1, 4);
    let (short_bytes, short_slots, short_ok) = cold(&short);
    let (long_bytes, _, long_ok) = cold(&long);
    let (many_bytes, many_slots, many_ok) = cold(&many);
    assert!(short_ok && long_ok && many_ok, "every ring's walk reaches the value");
    let chain_bytes = long_bytes as i64 - short_bytes as i64;
    assert_eq!(chain_bytes, 0, "a chain 63 modules longer per node holds {chain_bytes} bytes more: a chain is copied");
    let edge_bytes = many_bytes as i64 - short_bytes as i64;
    let slot_bytes = (many_slots as i64 - short_slots as i64) * 4;
    assert_eq!(edge_bytes, slot_bytes, "four times the edges hold {edge_bytes} bytes more, their slots {slot_bytes}: an edge is copied");

    // One scratch kept across worlds that do not outlive it, and a warm walk allocates nothing.
    let counting = Erased::new(Counting::new(heap()));
    let mut scratch = ThroughScratch::new_in(counting.handle());
    let root = Def::module(item(0));
    {
        let world = ring(64, 4);
        assert_eq!(through_in(&mut Reader::new(&world), root, &mut scratch), Ok(Some(found())));
    }
    let warm_from = counting.inner().allocations();
    let caps = scratch.capacities();
    for (chain, repeat) in [(1, 1), (64, 1), (1, 4)] {
        let world = ring(chain, repeat);
        assert_eq!(through_in(&mut Reader::new(&world), root, &mut scratch), Ok(Some(found())), "ring ({chain}, {repeat})");
    }
    let growth = counting.inner().allocations() - warm_from;
    assert_eq!(growth, 0, "warm walks of worlds no larger than the first allocated {growth} times");
    assert_eq!(scratch.capacities(), caps, "the scratch grew on a warm pass");
    println!(
        "through scratch: walks {}, scratches made 1, bytes copied {chain_bytes} (chains of 64 hold {chain_bytes} bytes more than chains of 1; 4x the edges hold only their slots' {slot_bytes} bytes more), growth on the warm pass {growth}",
        scratch.walks()
    );
}
