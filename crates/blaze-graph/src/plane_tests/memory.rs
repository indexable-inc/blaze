//! The plane's slots (docs/v2/plan/lanes/mem/core/plane.md): against the free lists they
//! replaced over seeds, a stale task handle refused, and a warm request allocating nothing but
//! its one payload.

use blaze_alloc_count::measure;

use blaze_ids::rng::SplitMix64;

use super::{add, built, close, config, done, keyed, open, runs, work};
use crate::{
    machine::{Error, NodeId, Outcome, Spec},
    plane::{Plane, PlaneInput, PlaneOutput, RequestId, Slots, TaskHandle, TaskSlot},
};

/// The slots as they were: a free list of indices, last freed first, and each slot's
/// generation, odd while it is live.
#[derive(Default)]
struct Model {
    free: Vec<u32>,
    generation: Vec<u32>,
}

impl Model {
    fn take(&mut self) -> (u32, u32) {
        let index = self.free.pop().unwrap_or_else(|| {
            self.generation.push(0);
            (self.generation.len() - 1) as u32
        });
        self.generation[index as usize] += 1;
        (index, self.generation[index as usize])
    }

    fn live(&self, (index, generation): (u32, u32)) -> bool {
        generation % 2 == 1 && self.generation.get(index as usize) == Some(&generation)
    }

    fn give(&mut self, it: (u32, u32)) {
        assert!(self.live(it));
        self.generation[it.0 as usize] += 1;
        self.free.push(it.0);
    }
}

fn parts(handle: TaskHandle) -> (u32, u32) {
    (handle.index(), handle.generation())
}

/// Each take names the slot the old free list would have, each handle given back is stale
/// from then on, and a stale one is never live again, whoever holds the slot now.
#[test]
fn the_slots_take_what_the_free_lists_took() {
    for seed in 0..200 {
        let mut rng = SplitMix64::new(seed);
        let mut slots = Slots::<TaskSlot>::new(1 << 10);
        let mut model = Model::default();
        let mut live: Vec<TaskHandle> = Vec::new();
        let mut stale: Vec<TaskHandle> = Vec::new();
        for _ in 0..400 {
            if live.is_empty() || (rng.below(3) != 0 && live.len() < 64) {
                let handle = slots.take();
                assert_eq!(parts(handle), model.take(), "seed {seed}: the slot taken");
                live.push(handle);
            } else {
                let handle = live.swap_remove(rng.below(live.len() as u64) as usize);
                slots.give(handle);
                model.give(parts(handle));
                stale.push(handle);
            }
            assert_eq!(slots.made(), model.generation.len(), "seed {seed}: slots made");
            for &handle in live.iter().chain(&stale) {
                assert_eq!(slots.live(handle), model.live(parts(handle)), "seed {seed}: {handle:?}");
            }
            assert!(stale.iter().all(|&it| !slots.live(it)), "seed {seed}: a stale handle is live");
        }
    }
}

/// A Done for a node whose task ended, its slot since taken by another node's task, is
/// refused: the handle the node held is stale, though the slot's index is the same.
#[test]
fn a_done_for_a_task_whose_slot_was_reused_is_refused() {
    let mut plane = Plane::new(config(1, 1)).unwrap();
    open(&mut plane, 1, 1, true);
    assert_eq!(runs(&add(&mut plane, 1, vec![work(1, &[], 1)])), [1]);
    let first = plane.task_of(NodeId(1)).expect("node 1 runs as a task");
    assert!(runs(&built(&mut plane, 1)).is_empty());
    assert!(plane.task_of(NodeId(1)).is_none() && !plane.task_live(first));

    assert_eq!(runs(&add(&mut plane, 1, vec![work(2, &[], 1)])), [2]);
    let second = plane.task_of(NodeId(2)).expect("node 2 runs as a task");
    assert_eq!(second.index(), first.index(), "node 2's task takes the slot node 1's left");
    assert_ne!(second, first);
    assert!(!plane.task_live(first) && plane.task_live(second));

    let before = plane.stats(RequestId(1));
    assert_eq!(plane.step(done(1, Outcome::Built, Vec::new())), Err(Error::NotRunning(NodeId(1))));
    assert_eq!(plane.stats(RequestId(1)), before, "the refused Done changed nothing");
    assert_eq!(plane.task_of(NodeId(2)), Some(second));

    close(&mut plane, 1);
    let out = built(&mut plane, 2);
    assert!(out.iter().any(|it| matches!(it, PlaneOutput::Finished { ok: true, .. })));
}

/// A request's nodes: a diamond, one of them keyed, and one alone.
fn diamond() -> Vec<Spec> {
    vec![work(1, &[], 3), work(2, &[1], 2), keyed(3, &[1], 2, 7), work(4, &[2, 3], 1), work(5, &[], 1)]
}

/// One request through the plane: opened, its nodes added, closed and released at once, then
/// each node run reported built until it finishes. `out` and `running` keep their room.
fn one_round(plane: &mut Plane, nodes: Vec<Spec>, out: &mut Vec<PlaneOutput>, running: &mut Vec<NodeId>) {
    let request = RequestId(9);
    out.clear();
    let open = PlaneInput::Open { at_ns: 0, request, weight: 1, keep_going: true, idle: false };
    plane.step_into(open, out).unwrap();
    plane.step_into(PlaneInput::Add { at_ns: 1, request, nodes }, out).unwrap();
    plane.step_into(PlaneInput::Close { at_ns: 2, request }, out).unwrap();
    plane.release(request).unwrap();
    let mut at_ns = 3;
    let mut seen = 0;
    loop {
        running.clear();
        let mut finished = false;
        for it in &out[seen..] {
            match it {
                PlaneOutput::Run { id, .. } => running.push(*id),
                PlaneOutput::Finished { ok, .. } => {
                    assert!(ok);
                    finished = true;
                }
                _ => {}
            }
        }
        seen = out.len();
        if finished {
            assert!(running.is_empty());
            return;
        }
        assert!(!running.is_empty(), "a request that neither runs nor finishes");
        for &id in running.iter() {
            let done =
                PlaneInput::Done { at_ns, id, outcome: Outcome::Built, added: Vec::new(), group: None, peak: 0 };
            plane.step_into(done, out).unwrap();
            at_ns += 1;
        }
    }
}

/// A warm plane runs a request, its nodes and their Runs and Dones and its Finished, its slots
/// and buffers the last request left, making one allocation: the path of the request's
/// `Event::Critical`, which is payload. The nodes it is given are made before it is measured.
#[test]
fn a_warm_request_allocates_only_its_critical_path() {
    let mut plane = Plane::new(config(2, 4)).unwrap();
    let mut out = Vec::with_capacity(1 << 10);
    let mut running = Vec::with_capacity(16);
    for _ in 0..8 {
        one_round(&mut plane, diamond(), &mut out, &mut running);
    }
    let made = plane.slots();
    let nodes = diamond();
    let ((), used) = measure(|| one_round(&mut plane, nodes, &mut out, &mut running));
    assert_eq!(used.allocations, 1, "a warm request, its Event::Critical's path aside: {used:?}");
    assert_eq!(plane.slots(), made, "the request took the slots the last one left");
    assert!(plane.stats(RequestId(9)).is_none(), "the request was released");
}
