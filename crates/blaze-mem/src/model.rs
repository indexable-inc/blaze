//! Model tests (docs/v2/plan/design/model-checker.md) of the tagged lock-free stack every pool
//! and the chunk cache move batches through: 2 or 3 threads under every interleaving to the
//! preemption bound and every value C11 lets a load read, against a sequential model (the
//! multiset of indices on the stack or held). The memory gate's waits and gives-way are the
//! budget's (`budget::tests`, every interleaving of its sans-IO steps) and the gate's own
//! tests (`gate::tests`).

use std::sync::Arc;

use blaze_check::{model, thread};

use crate::{
    stack::{Links, NONE, Stack},
    sync::atomic::AtomicU32,
};

struct Table(Vec<AtomicU32>);

impl Links for Table {
    fn link(&self, index: u32) -> &AtomicU32 {
        &self.0[index as usize]
    }
}

fn table(n: u32) -> Arc<Table> {
    Arc::new(Table((0..n).map(|_| AtomicU32::new(NONE)).collect()))
}

fn drain(stack: &Stack, table: &Table) -> Vec<u32> {
    let mut items = Vec::new();
    while let Some(item) = stack.pop(table) {
        items.push(item);
    }
    items.sort_unstable();
    items
}

/// Two threads each pop one and push it back with a third pushed: nothing is lost or popped
/// twice, whatever the interleaving.
#[test]
fn model_pops_and_pushes_from_two_threads_lose_nothing() {
    model(|| {
        let links = table(4);
        let stack = Arc::new(Stack::new());
        stack.push(0, &*links);
        stack.push(1, &*links);
        let spawn = |extra: u32| {
            let (stack, links) = (Arc::clone(&stack), Arc::clone(&links));
            thread::spawn(move || {
                let got = stack.pop(&*links);
                if let Some(item) = got {
                    stack.push(item, &*links);
                }
                stack.push(extra, &*links);
                got
            })
        };
        let a = spawn(2);
        let b = spawn(3);
        let (a, b) = (a.join().unwrap_or(None), b.join().unwrap_or(None));
        assert!(a.is_some() && b.is_some(), "two items were on the stack for two pops");
        assert_eq!(drain(&stack, &links), vec![0, 1, 2, 3]);
    });
}

/// The ABA case: one thread pops while another pops two and pushes the first back. The first
/// thread's compare-and-swap must not link the stale next, so every item is held exactly once.
#[test]
fn model_a_stale_top_does_not_link_a_popped_next() {
    model(|| {
        let links = table(3);
        let stack = Arc::new(Stack::new());
        stack.push(2, &*links);
        stack.push(1, &*links);
        stack.push(0, &*links);
        let popper = {
            let (stack, links) = (Arc::clone(&stack), Arc::clone(&links));
            thread::spawn(move || stack.pop(&*links))
        };
        let shuffler = {
            let (stack, links) = (Arc::clone(&stack), Arc::clone(&links));
            thread::spawn(move || {
                let first = stack.pop(&*links);
                let second = stack.pop(&*links);
                if let Some(first) = first {
                    stack.push(first, &*links);
                }
                second
            })
        };
        let popped = popper.join().unwrap_or(None);
        let kept = shuffler.join().unwrap_or(None);
        let mut held: Vec<u32> = drain(&stack, &links);
        held.extend(popped);
        held.extend(kept);
        held.sort_unstable();
        assert_eq!(held, vec![0, 1, 2], "each item is on the stack or held by one thread");
    });
}

/// A pop of up to three, racing a thread that pops two and pushes them back as one chain:
/// the walk of a stale chain never commits, so every item is on the stack or held once.
#[test]
fn model_a_pop_of_some_racing_a_chain_push_holds_each_item_once() {
    model(|| {
        let links = table(4);
        let stack = Arc::new(Stack::new());
        stack.push_all(&[0, 1, 2, 3], &*links);
        let popper = {
            let (stack, links) = (Arc::clone(&stack), Arc::clone(&links));
            thread::spawn(move || {
                let mut out = [NONE; 3];
                let n = stack.pop_some(&*links, &mut out);
                out[..n].to_vec()
            })
        };
        let shuffler = {
            let (stack, links) = (Arc::clone(&stack), Arc::clone(&links));
            thread::spawn(move || {
                let first = stack.pop(&*links);
                let second = stack.pop(&*links);
                let back: Vec<u32> = second.into_iter().chain(first).collect();
                stack.push_all(&back, &*links);
            })
        };
        let mut held = popper.join().unwrap_or_default();
        shuffler.join().unwrap_or(());
        held.extend(drain(&stack, &links));
        held.sort_unstable();
        assert_eq!(held, vec![0, 1, 2, 3], "each item is on the stack or held by one thread");
    });
}

/// What a pusher wrote before pushing (a batch's contents) is seen by the thread that pops it.
#[test]
fn model_a_pop_sees_what_its_pusher_wrote() {
    model(|| {
        let links = table(1);
        let stack = Arc::new(Stack::new());
        let payload = Arc::new(AtomicU32::new(0));
        let pusher = {
            let (stack, links, payload) =
                (Arc::clone(&stack), Arc::clone(&links), Arc::clone(&payload));
            thread::spawn(move || {
                payload.store(7, crate::sync::atomic::Ordering::Relaxed);
                stack.push(0, &*links);
            })
        };
        let popper = {
            let (stack, links, payload) =
                (Arc::clone(&stack), Arc::clone(&links), Arc::clone(&payload));
            thread::spawn(move || {
                stack.pop(&*links).map(|_| payload.load(crate::sync::atomic::Ordering::Relaxed))
            })
        };
        pusher.join().ok();
        if let Ok(Some(seen)) = popper.join() {
            assert_eq!(seen, 7, "the push's release orders the payload before the pop");
        }
    });
}

