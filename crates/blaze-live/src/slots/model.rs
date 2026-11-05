//! Model tests (`--cfg blaze_check`) of the jump table's lock-free protocol. The tables are
//! words of the image and of the switch's pool that the linked stubs load by address, so the
//! real ones are `std`'s atomics, which the model checker can't stand in for; these tests take
//! the same steps with the same orderings on its atomics, each named after the code it follows.
//! The stub's two loads (the cell, then the slot through it) are an address dependency in
//! machine code, which orders them on x86-64 and AArch64 as the acquire load here does.

#![forbid(unsafe_code)]

use blaze_check::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

const SLOTS: usize = 2;
const OLD: [u64; SLOTS] = [0x1000, 0x2000];
const NEW: [u64; SLOTS] = [0x3000, 0x4000];

/// Two tables of `SLOTS` slots and the cell naming one of them (0 or 1).
struct Switched {
    tables: [[AtomicU64; SLOTS]; 2],
    cell: AtomicU64,
}

/// A patch writes its slots into a table the cell doesn't point at (`Switch::retarget`, one
/// relaxed store each), then stores the cell (`Switch::publish`, release): a call that loads
/// the new table sees every slot of the patch, never one without the other.
#[test]
fn model_a_call_sees_all_of_a_patch_or_none() {
    blaze_check::model(|| {
        // alloc: the state the model's two threads share, once an execution
        let switched = Arc::new(Switched {
            tables: [OLD.map(AtomicU64::new), OLD.map(AtomicU64::new)],
            cell: AtomicU64::new(0),
        });
        let patch = {
            // clone: the patching thread's handle on it
            let switched = switched.clone();
            thread::spawn(move || {
                for (slot, new) in switched.tables[1].iter().zip(NEW) {
                    slot.store(new, Ordering::Relaxed);
                }
                switched.cell.store(1, Ordering::Release);
            })
        };
        let on = switched.cell.load(Ordering::Acquire) as usize;
        let seen = switched.tables[on].each_ref().map(|slot| slot.load(Ordering::Relaxed));
        assert_eq!(seen, if on == 0 { OLD } else { NEW }, "a call through table {on}");
        patch.join().unwrap();
    });
}

/// The loader's table, a table of the pool being readied, and whether `Tables` names it yet.
struct Readied {
    loader: [AtomicU64; SLOTS],
    pooled: [AtomicU64; SLOTS],
    registered: AtomicU64,
}

/// Lazy compilation publishes a slot (`Header::publish`: the loader's table, then each table
/// `Tables::each` names, every store and load sequentially consistent) while the switch readies
/// a new table (`Switch::ready`: `Tables::register` names it while zero, then each slot copied
/// from the current table by a compare-and-swap from zero): the new table ends with the
/// published slot, whichever goes first.
#[test]
fn model_a_slot_published_while_a_table_is_readied_is_kept() {
    blaze_check::model(|| {
        // alloc: the state the model's two threads share, once an execution
        let readied = Arc::new(Readied {
            loader: OLD.map(AtomicU64::new),
            pooled: [const { AtomicU64::new(0) }; SLOTS],
            registered: AtomicU64::new(0),
        });
        let lazy = {
            // clone: the publishing thread's handle on it
            let readied = readied.clone();
            thread::spawn(move || {
                readied.loader[1].store(NEW[1], Ordering::SeqCst);
                if readied.registered.load(Ordering::SeqCst) != 0 {
                    readied.pooled[1].store(NEW[1], Ordering::SeqCst);
                }
            })
        };
        readied.registered.store(1, Ordering::SeqCst);
        for (to, from) in readied.pooled.iter().zip(readied.loader.iter()) {
            let value = from.load(Ordering::SeqCst);
            _ = to.compare_exchange(0, value, Ordering::Relaxed, Ordering::Relaxed);
        }
        lazy.join().unwrap();
        let ready = readied.pooled.each_ref().map(|slot| slot.load(Ordering::Relaxed));
        assert_eq!(ready, [OLD[0], NEW[1]], "the readied table keeps the published slot");
    });
}
