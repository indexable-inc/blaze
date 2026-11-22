//! One seqlocked row: the action recorded for one owned signal (its handler word, its flags and
//! its mask), written by a thread's `sigaction` and read by the fault handler on any thread.
//!
//! The protocol (H.-J. Boehm, "Can seqlocks get along with programming language memory
//! models?", 2012, the fence form):
//!
//! - **A writer** claims the row by moving `seq` from an even `s` to `s + 1` with a
//!   compare-exchange (two writers never both win one even value, so writes are serialized),
//!   issues a `Release` fence, stores the fields `Relaxed`, and publishes by storing `s + 2` with
//!   `Release`. The claim is bounded: `tries` attempts, then [`Busy`].
//! - **A reader** loads `seq` with `Acquire`; an odd value is a write in progress (it spins); it
//!   loads the fields `Relaxed`, issues an `Acquire` fence and loads `seq` again `Relaxed`: the
//!   same even value means no write touched the fields between, so they are one write's whole
//!   value. Bounded the same way. A field load that read a store of a later write
//!   synchronizes, through the two fences, with that write's claim, so the second load sees the
//!   claim and the read is retried, never returned torn.
//!
//! No lock, no allocation, atomics only: async-signal-safe. A handler on the WRITER's own thread
//! cannot interrupt a write (signals.rs blocks the owned signals on the writing thread around
//! it); a handler on another thread spins at most `tries` times.
//!
//! Every atomic here, the fences and `spin_loop` are blaze-check's under `--cfg blaze_check`
//! (the `use` lines below), so the interleaving checker sees each load, store, fence and retry
//! as one step. Proven by the checker (`row_model.rs`) with two planted bugs it must catch:
//! [`Plant::BumpOnce`] (a writer with no odd claim) and [`Plant::NoRecheck`] (a reader that does
//! not load the sequence again).

#[cfg(blaze_check)]
use blaze_check::hint::spin_loop;
#[cfg(blaze_check)]
use blaze_check::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering, fence};
#[cfg(not(blaze_check))]
use core::hint::spin_loop;
#[cfg(not(blaze_check))]
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering, fence};

/// One recorded action, as plain words: `handler` is the `sa_sigaction` word (`SIG_DFL` is 0),
/// `flags` the `sa_flags` bits, `mask` the `sa_mask` bytes in `W` words.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Value<const W: usize> {
    pub handler: usize,
    pub flags: u32,
    pub mask: [u64; W],
}

/// The row's sequence did not settle within the tries given: a writer held it (or many writers
/// took turns) for that long. The caller stops the process naming the signal (rule 18).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Busy;

/// How a write or a read is carried out. `None` in every build; the other two are the
/// interleaving test's planted bugs (row_model.rs), each one half of the protocol removed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plant {
    None,
    /// A writer that bumps the sequence once: no odd claim and no fence, the fields stored
    /// first and `seq + 2` stored `Release` after. A reader can return a half-written value
    /// and two writers can interleave. [`Row::read_with`] ignores it.
    #[cfg_attr(
        not(all(test, blaze_check)),
        expect(dead_code, reason = "the seqlock's planted writer bug (signals/row_model.rs)")
    )]
    BumpOnce,
    /// A reader that returns the fields after its first even sequence load, with no fence and
    /// no second load: a write between its loads goes unseen. [`Row::replace`] ignores it.
    #[cfg_attr(
        not(all(test, blaze_check)),
        expect(dead_code, reason = "the seqlock's planted reader bug (signals/row_model.rs)")
    )]
    NoRecheck,
}

/// The seqlocked row: a sequence and the fields of one [`Value`].
pub struct Row<const W: usize> {
    seq: AtomicU64,
    handler: AtomicUsize,
    flags: AtomicU32,
    mask: [AtomicU64; W],
}

impl<const W: usize> Row<W> {
    /// A row holding SIG_DFL (handler word 0), no flags and an empty mask, for a `static`.
    pub const fn new() -> Row<W> {
        Row {
            seq: AtomicU64::new(0),
            handler: AtomicUsize::new(0),
            flags: AtomicU32::new(0),
            mask: [const { AtomicU64::new(0) }; W],
        }
    }

    /// Writes `value` and answers the value it replaced. `tries` bounds the claim.
    pub fn replace(&self, value: &Value<W>, tries: u32, plant: Plant) -> Result<Value<W>, Busy> {
        if let Plant::BumpOnce = plant {
            // The planted bug: no claim, fields first, the sequence after.
            let before = self.fields();
            self.store(value);
            let seq = self.seq.load(Ordering::Relaxed);
            self.seq.store(seq.wrapping_add(2), Ordering::Release);
            return Ok(before);
        }
        let mut tried = 0u32;
        let claimed = loop {
            let seq = self.seq.load(Ordering::Relaxed);
            // Acquire on success: this writer's loads of the fields below see the previous
            // writer's stores, whose publish (`Release`) this claim read.
            if seq & 1 == 0
                && self.seq.compare_exchange(seq, seq | 1, Ordering::Acquire, Ordering::Relaxed).is_ok()
            {
                break seq;
            }
            tried += 1;
            if tried >= tries {
                return Err(Busy);
            }
            spin_loop();
        };
        // Release fence: orders the claim (odd) before every field store below, so a reader
        // whose field load sees one of them also sees the claim on its second sequence load.
        fence(Ordering::Release);
        // The claim makes this the only writer: its relaxed loads read the last published value.
        let before = self.fields();
        self.store(value);
        // Release: publishes the field stores to a reader that acquires `claimed + 2`.
        self.seq.store(claimed.wrapping_add(2), Ordering::Release);
        Ok(before)
    }

    /// Reads one whole value. `tries` bounds the attempts.
    pub fn read(&self, tries: u32) -> Result<Value<W>, Busy> {
        self.read_with(tries, Plant::None).map(|(value, _attempts)| value)
    }

    /// [`Row::read`], answering also the attempts it took (1: the first settled; more: the
    /// sequence was odd or moved between the two loads, and the read went again). The model
    /// test counts retries with it and passes [`Plant::NoRecheck`] as its control.
    pub fn read_with(&self, tries: u32, plant: Plant) -> Result<(Value<W>, u32), Busy> {
        let mut tried = 0u32;
        loop {
            let first = self.seq.load(Ordering::Acquire);
            if first & 1 == 0 {
                let value = self.fields();
                if let Plant::NoRecheck = plant {
                    // The planted bug: no fence, no second load.
                    return Ok((value, tried + 1));
                }
                // Acquire fence: a field load that read a later write's store synchronizes with
                // that write's Release fence, so the load below sees its claim.
                fence(Ordering::Acquire);
                if self.seq.load(Ordering::Relaxed) == first {
                    return Ok((value, tried + 1));
                }
            }
            tried += 1;
            if tried >= tries {
                return Err(Busy);
            }
            spin_loop();
        }
    }

    fn fields(&self) -> Value<W> {
        let mut mask = [0u64; W];
        for (into, word) in mask.iter_mut().zip(self.mask.iter()) {
            *into = word.load(Ordering::Relaxed);
        }
        Value { handler: self.handler.load(Ordering::Relaxed), flags: self.flags.load(Ordering::Relaxed), mask }
    }

    fn store(&self, value: &Value<W>) {
        self.handler.store(value.handler, Ordering::Relaxed);
        self.flags.store(value.flags, Ordering::Relaxed);
        for (word, from) in self.mask.iter().zip(value.mask.iter()) {
            word.store(*from, Ordering::Relaxed);
        }
    }
}
