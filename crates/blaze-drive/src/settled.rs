//! The settled table (drive.md, "Scheduling on many cores"): what each name settled this run is,
//! readable by every worker at once while the coordinator steps, so a worker answers a task's
//! `Need` itself when the record already exists and re-enters the task in place.
//!
//! One writer, many readers, no lock: the coordinator's thread is the only writer (it performs
//! [`crate::Effect::Publish`], after the `Put`s before it), and each slot is a seqlock of
//! atomic words, so a reader never sees a torn row and never blocks the writer. A slot is
//! found by open addressing from the name's fingerprint, at most [`PROBES`] slots.
//!
//! Correctness never rests on this table, only throughput: every read a task reports is checked
//! by the coordinator against the name's settled digest when the task's record arrives (a read
//! whose digest is not today's makes the job decide again: `Request::made`). So a fingerprint
//! collision, a row overwritten by a re-settle, or a reader that gave up on a busy slot costs a
//! run, never a wrong answer. The simulation proves the table against a planted early publish.
//!
//! Memory: [`WORDS`] words (88 bytes) a slot, sized by the caller at twice the names it expects
//! this run (load at most a half). A full probe run is [`TableFull`], naming the size (rule 18).

use core::alloc::Allocator;
use core::sync::atomic::{AtomicU64, Ordering, fence};

use blaze_id::ContentId;
use blaze_own::Vec;

use crate::key::{Key, Name};
use crate::request::Input;

/// Words a slot: sequence, fingerprint, live, the record's key (4), its out digest (4).
pub const WORDS: usize = 11;
/// The most slots one lookup or publish probes.
pub const PROBES: usize = 32;
/// The most times a reader re-reads a slot the writer is in before it gives up (rule 18): the
/// writer holds a slot for eleven stores, so a reader that loses this many races is not
/// starved by one write but by a re-settle storm, and the coordinator answers instead.
pub const MOST_RETRIES: u32 = 64;

const SEQ: usize = 0;
const FP: usize = 1;
const LIVE: usize = 2;
const AT: usize = 3;
const OUT: usize = 7;

/// A publish found no free slot in [`PROBES`]: the caller sized the table below the run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TableFull {
    pub slots: u32,
    pub name: Name,
}

impl core::fmt::Display for TableFull {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "the settled table of {} slots has no free slot near {} (rule 18): size it from more names",
            self.slots, self.name
        )
    }
}

impl core::error::Error for TableFull {}

pub struct Settled<A: Allocator> {
    words: Vec<AtomicU64, A>,
    mask: usize,
}

fn fingerprint(name: Name) -> u64 {
    let s = &name.subject.0;
    let fp = u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]) ^ ((name.kind.index() as u64) << 56);
    // Zero marks a slot never written.
    fp | 1
}

fn words_of(id: &ContentId) -> [u64; 4] {
    let b = &id.0;
    let w = |i: usize| u64::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3], b[i + 4], b[i + 5], b[i + 6], b[i + 7]]);
    [w(0), w(8), w(16), w(24)]
}

fn id_of(words: [u64; 4]) -> ContentId {
    let mut out = [0u8; 32];
    for (at, word) in words.iter().enumerate() {
        out[at * 8..at * 8 + 8].copy_from_slice(&word.to_le_bytes());
    }
    ContentId(out)
}

impl<A: Allocator> Settled<A> {
    /// A table of at least `slots` slots (a power of two at or above it, at least 64).
    pub fn new_in(slots: usize, alloc: A) -> Settled<A> {
        let slots = slots.max(64).next_power_of_two();
        let mut words = Vec::with_capacity_in(slots * WORDS, alloc);
        for _ in 0..slots * WORDS {
            words.push(AtomicU64::new(0));
        }
        Settled { words, mask: slots - 1 }
    }

    pub fn slots(&self) -> usize {
        self.mask + 1
    }

    fn word(&self, slot: usize, at: usize) -> &AtomicU64 {
        &self.words[slot * WORDS + at]
    }

    /// The slot holding `fp`, or the first never written, within [`PROBES`].
    fn find(&self, fp: u64) -> Option<usize> {
        let start = (fp >> 7) as usize & self.mask;
        for probe in 0..PROBES {
            let slot = (start + probe) & self.mask;
            let held = self.word(slot, FP).load(Ordering::Acquire);
            if held == fp || held == 0 {
                return Some(slot);
            }
        }
        None
    }

    /// The writer's: `name` is settled at `input`. Only one thread calls this or [`clear`].
    pub fn publish(&self, input: Input) -> Result<(), TableFull> {
        self.write(input.name, Some((input.at, input.out)))
    }

    /// The writer's: `name` is no longer settled (its home dropped it, or it was dirtied).
    pub fn clear(&self, name: Name) -> Result<(), TableFull> {
        self.write(name, None)
    }

    fn write(&self, name: Name, value: Option<(Key, ContentId)>) -> Result<(), TableFull> {
        let fp = fingerprint(name);
        let full = TableFull { slots: u32::try_from(self.slots()).unwrap_or(u32::MAX), name };
        let slot = self.find(fp).ok_or(full)?;
        let seq = self.word(slot, SEQ).load(Ordering::Relaxed);
        // Odd: the slot is being written. The fence keeps the field stores after it.
        self.word(slot, SEQ).store(seq.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        match value {
            Some((at, out)) => {
                for (i, word) in words_of(&at.0).into_iter().enumerate() {
                    self.word(slot, AT + i).store(word, Ordering::Relaxed);
                }
                for (i, word) in words_of(&out).into_iter().enumerate() {
                    self.word(slot, OUT + i).store(word, Ordering::Relaxed);
                }
                self.word(slot, LIVE).store(1, Ordering::Relaxed);
            }
            None => self.word(slot, LIVE).store(0, Ordering::Relaxed),
        }
        self.word(slot, SEQ).store(seq.wrapping_add(2), Ordering::Release);
        // Last: a reader that sees the fingerprint sees a whole first write (Acquire in find).
        self.word(slot, FP).store(fp, Ordering::Release);
        Ok(())
    }

    /// Any thread's: what `name` is settled at, if it is and the slot was read whole within
    /// [`MOST_RETRIES`]. `None` sends the need to the coordinator.
    pub fn get(&self, name: Name) -> Option<Input> {
        let fp = fingerprint(name);
        let slot = self.find(fp)?;
        if self.word(slot, FP).load(Ordering::Acquire) != fp {
            return None;
        }
        for _ in 0..MOST_RETRIES {
            let before = self.word(slot, SEQ).load(Ordering::Acquire);
            if before & 1 == 1 {
                core::hint::spin_loop();
                continue;
            }
            let live = self.word(slot, LIVE).load(Ordering::Relaxed);
            let mut at = [0u64; 4];
            let mut out = [0u64; 4];
            for i in 0..4 {
                at[i] = self.word(slot, AT + i).load(Ordering::Relaxed);
                out[i] = self.word(slot, OUT + i).load(Ordering::Relaxed);
            }
            fence(Ordering::Acquire);
            if self.word(slot, SEQ).load(Ordering::Relaxed) != before {
                continue;
            }
            return (live == 1).then(|| Input { name, at: Key(id_of(at)), out: id_of(out) });
        }
        None
    }
}
