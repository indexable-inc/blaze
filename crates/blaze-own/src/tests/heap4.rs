//! `Heap4` against a sorted `Vec` (the model), under the counting allocator, and the typed
//! control of its one plant (D165): a sift that skips the fourth child must be caught as a pop
//! that gave other than the least.

use core::fmt;
use std::vec::Vec as StdVec;

use blaze_sim::{Broken, Tape, controls};

use super::{Counting, cases};
use crate::{
    heap4::{Heap4, Heap4Plant},
    planted::{NoPlant, Planted},
};

/// The heap's oracle: what it can see broken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeapBroken {
    /// `pop`, `push_pop` or `peek` gave other than the model's least.
    NotLeast { op: &'static str },
    /// The length differs from the model's.
    Len { heap: usize, model: usize },
    /// A block was left live or freed wrong.
    Leaked,
}

impl fmt::Display for HeapBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HeapBroken::NotLeast { op } => write!(f, "`{op}` gave other than the least"),
            HeapBroken::Len { heap, model } => write!(f, "the heap holds {heap}, the model {model}"),
            HeapBroken::Leaked => f.write_str("the counting allocator saw a leak or a wrong free"),
        }
    }
}

// blaze-own's tests link a second blaze-own through blaze-sim, whose `Plant` extends that copy's
// `Plants`: the enum takes both, and `plants!` writes `Plant` over the shipped enum.
impl blaze_sim::Plants for Heap4Plant {}

blaze_sim::plants! {
    impl Heap4Plant breaks HeapBroken {
        None,
        SkipsFourthChild => HeapBroken::NotLeast { .. },
    }
}

fn ensure(holds: bool, broken: HeapBroken) -> Result<(), Broken<HeapBroken>> {
    if holds { Ok(()) } else { Err(Broken::Oracle(broken)) }
}

/// A run of pushes, pops and push-pops against the model, the plant taken through the seam.
fn heap_case(tape: &Tape, plant: impl Planted<Heap4Plant>) -> Result<(), Broken<HeapBroken>> {
    let counted = Counting::new();
    {
        let mut heap: Heap4<u64, &Counting> = Heap4::new_in(&counted);
        let mut model: StdVec<u64> = StdVec::new();
        for _ in 0..tape.below(200, "ops") {
            let value = tape.below(500, "value");
            match tape.below(5, "op") {
                0..=1 => {
                    heap.push(value);
                    model.push(value);
                }
                2..=3 => {
                    model.sort_unstable_by(|a, b| b.cmp(a));
                    ensure(heap.pop_planted(plant) == model.pop(), HeapBroken::NotLeast { op: "pop" })?;
                }
                _ => {
                    model.push(value);
                    model.sort_unstable_by(|a, b| b.cmp(a));
                    let least = model.pop();
                    let got = heap.push_pop_planted(value, plant);
                    ensure(Some(got) == least, HeapBroken::NotLeast { op: "push_pop" })?;
                }
            }
            ensure(heap.len() == model.len(), HeapBroken::Len { heap: heap.len(), model: model.len() })?;
            let least = model.iter().copied().min();
            ensure(heap.peek().copied() == least, HeapBroken::NotLeast { op: "peek" })?;
        }
    }
    counted.check().map_err(|_| Broken::Oracle(HeapBroken::Leaked))
}

/// The refinement: `Heap4` matches a sorted `Vec` under random operations and frees every allocation through the counting allocator.
#[test]
fn heap4_matches_a_sorted_vec() {
    blaze_sim::check(cases(300), |tape: &Tape| {
        heap_case(tape, NoPlant).map_err(|broken| blaze_sim::Broke::Oracle(std::format!("{broken}")))
    });
}

#[test]
#[cfg_attr(miri, ignore = "a control tests the oracle, not the unsafe code; its clean half is `heap4_matches_a_sorted_vec`")]
fn the_oracle_catches_a_sift_that_skips_the_fourth_child() {
    controls(cases(400), |tape, plant: Heap4Plant| heap_case(tape, plant));
}

#[test]
fn a_heap_drains_and_refills_in_its_room() {
    let counted = Counting::new();
    {
        let mut heap: Heap4<u32, &Counting> = Heap4::with_capacity_in(8, &counted);
        for value in [5, 3, 8, 1, 9, 2, 7, 4] {
            heap.push(value);
        }
        let room = heap.capacity();
        let mut drained: StdVec<u32> = heap.drain_unordered().collect();
        drained.sort_unstable();
        assert_eq!(drained, [1, 2, 3, 4, 5, 7, 8, 9]);
        assert!(heap.is_empty());
        assert_eq!(heap.capacity(), room, "the room is kept");
        assert_eq!(heap.push_pop(6), 6, "push_pop of an empty heap gives the value back");
    }
    if let Err(broke) = counted.check() {
        panic!("{broke}");
    }
}
