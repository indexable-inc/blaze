//! The model checker's tests of the two protocols other threads take part in: a free into
//! another thread's heap ([`Remote`]) and the adoption of an ended thread's heap ([`Registry`]).
//! Run with `RUSTFLAGS="--cfg blaze_check" cargo test -p blaze-malloc --release model_`: every
//! interleaving to the preemption bound and every value C11 lets a load read, each block's link
//! word a tracked place. Each protocol is run with [`Plant::None`] and with each planted bug,
//! which the model must catch.
//!
//! The blocks are `Box`es of the test process's allocator, not the allocator's own: the
//! protocols take any block whose first word is free to link, and the real heap's spans need
//! a page source a model execution would make and unmake thousands of times.

use std::{boxed::Box, vec::Vec};

use blaze_check::{cell::UnsafeCell, model, sync::Arc, thread};

use crate::{
    adopt::{Claimed, Registry},
    remote::{Block, Freed, Plant, Remote},
};

/// A block the model hands around, by address.
fn block() -> *mut Block {
    Box::into_raw(Box::new(Block { next: core::ptr::null_mut() }))
}

/// A raw block moved into a model thread.
#[derive(Clone, Copy)]
struct Sent(*mut Block);

// SAFETY: the block is given up by the sender: one thread holds it at a time.
unsafe impl Send for Sent {}

/// Takes `block` into `got`, failing if it was handed out already.
fn take(got: &mut Vec<usize>, block: *mut Block) {
    assert!(!got.contains(&block.addr()), "a block handed out twice");
    got.push(block.addr());
}

/// Two threads free a block each into the owner's queue while the owner takes what is there
/// once; after both end it takes the rest. Each block reaches the owner once.
fn two_frees_and_a_take(plant: Plant) {
    model(move || {
        let remote = Arc::new(Remote::new());
        let blocks = [Sent(block()), Sent(block())];
        let freers: Vec<_> = blocks
            .iter()
            .map(|&sent| {
                let remote = Arc::clone(&remote);
                thread::spawn(move || {
                    let sent = sent;
                    // SAFETY: the block was given to this thread, which gives it up.
                    unsafe { remote.push_with(sent.0, Freed::of(sent.0), plant) };
                })
            })
            .collect();
        let mut got = Vec::new();
        // SAFETY: this thread is the queue's one consumer.
        unsafe { remote.drain_with(plant, &mut |b| take(&mut got, b)) };
        for freer in freers {
            freer.join().unwrap();
        }
        // SAFETY: as above.
        unsafe { remote.drain_with(plant, &mut |b| take(&mut got, b)) };
        assert_eq!(got.len(), 2, "a block freed into the queue was lost");
        for sent in blocks {
            // SAFETY: every block came back once and is dropped once.
            drop(unsafe { Box::from_raw(sent.0) });
        }
    });
}

#[test]
fn model_remote_frees_reach_the_owner_once() {
    two_frees_and_a_take(Plant::None);
}

/// The control: a push by a plain store loses one of two racing frees.
#[test]
#[should_panic(expected = "lost")]
fn model_remote_store_push_loses_a_block() {
    two_frees_and_a_take(Plant::StorePush);
}

/// The control: handing out the peeked head before taking the list hands it out twice.
#[test]
#[should_panic(expected = "twice")]
fn model_remote_peek_take_hands_out_twice() {
    two_frees_and_a_take(Plant::PeekTake);
}

/// A heap as the adoption protocol sees it: its queue, and a word its owner writes last.
struct Fake {
    remote: Remote,
    last: UnsafeCell<u32>,
}

// SAFETY: `last` is the owner's (or, after the registry's acquire, the adopter's) alone, which is
// what the model checks; `remote` is atomics.
unsafe impl Sync for Fake {}
// SAFETY: as above: nothing in it is bound to the thread that made it.
unsafe impl Send for Fake {}

/// The owner of heap 0 writes its last word and ends (abandons it) while another thread frees a
/// block of it into its queue and a new thread claims a heap. Whoever adopts heap 0 reads the
/// ended owner's word in order, and the late free reaches it once.
fn adoption_meets_a_late_free(plant: Plant) {
    model(move || {
        let registry = Arc::new(Registry::<2>::new());
        let heaps = Arc::new([
            Fake { remote: Remote::new(), last: UnsafeCell::new(0) },
            Fake { remote: Remote::new(), last: UnsafeCell::new(0) },
        ]);
        let first = registry.claim_with(plant, |at| &heaps[at as usize].remote, |_| {});
        assert_eq!(first, Some(Claimed::Fresh(0)));
        let late = Sent(block());
        let owner = {
            let (registry, heaps) = (Arc::clone(&registry), Arc::clone(&heaps));
            thread::spawn(move || {
                // SAFETY: the owner's own word, written before it abandons the heap.
                heaps[0].last.with_mut(|it| unsafe { *it = 7 });
                registry.abandon_with(0, plant);
            })
        };
        let freer = {
            let heaps = Arc::clone(&heaps);
            thread::spawn(move || {
                let late = late;
                // SAFETY: the block was given to this thread, which gives it up.
                unsafe { heaps[0].remote.push(late.0, Freed::of(late.0)) };
            })
        };
        let adopter = {
            let (registry, heaps) = (Arc::clone(&registry), Arc::clone(&heaps));
            thread::spawn(move || {
                let mut got = Vec::new();
                let claimed =
                    registry.claim_with(plant, |at| &heaps[at as usize].remote, |b| take(&mut got, b));
                if claimed == Some(Claimed::Adopted(0)) {
                    // SAFETY: the adopter owns the heap from the claim on.
                    let last = heaps[0].last.with(|it| unsafe { *it });
                    assert_eq!(last, 7, "the adopter missed the ended owner's last write");
                }
                (claimed, got)
            })
        };
        owner.join().unwrap();
        freer.join().unwrap();
        let (claimed, mut got) = adopter.join().unwrap();
        if claimed != Some(Claimed::Adopted(0)) {
            // Heap 0 is abandoned by now: this thread adopts it.
            let again = registry.claim_with(plant, |at| &heaps[at as usize].remote, |b| take(&mut got, b));
            assert_eq!(again, Some(Claimed::Adopted(0)));
        }
        // The adopter's owner, whoever it is, takes what came after the adoption.
        // SAFETY: every thread that took part has ended, so this is the one consumer.
        unsafe { heaps[0].remote.drain(|b| take(&mut got, b)) };
        assert_eq!(got, [late.0.addr()], "a late free into an adopted heap was dropped");
        // SAFETY: the block came back once.
        drop(unsafe { Box::from_raw(late.0) });
    });
}

#[test]
fn model_adoption_takes_late_frees() {
    adoption_meets_a_late_free(Plant::None);
}

/// The control: an adopter that starts the queue afresh drops a free that came before it.
#[test]
#[should_panic(expected = "dropped")]
fn model_adoption_dropping_the_queue_loses_a_free() {
    adoption_meets_a_late_free(Plant::DropAtAdoption);
}

/// The control: a relaxed abandon leaves the adopter's read of the owner's last write racing it.
#[test]
#[should_panic(expected = "data race")]
fn model_adoption_relaxed_abandon_races() {
    adoption_meets_a_late_free(Plant::RelaxedAbandon);
}
