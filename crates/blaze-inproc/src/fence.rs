//! Fences for running threads: when a retired range of code may be reused.
//!
//! The state is a page of words shared by the daemon and every run process (the control
//! object, mapped [`Access::Shared`](crate::Access::Shared)):
//!
//! - the epoch, written by the daemon only: a range unlinked before [`Fences::start`]
//!   returned grace `g` is posted with `g`, then the epoch becomes `g + 1`;
//! - a table of the ranges being retired, by run-process address, each with its grace;
//! - a slot per thread of the run processes, padded to 128 bytes and written by its thread
//!   only: its process and thread IDs and `seen`, the epoch it was past at its last quiescent
//!   point. A thread is past grace `g` when `seen > g`.
//! - `births`, counting slot claims.
//!
//! At a quiescent point ([`Thread::quiesce`]) a thread reads the epoch `e`, walks its stack,
//! and reports `seen = e`, or the oldest grace of a posted range one of its frames returns
//! into. A range of grace `g` is free once [`Fences::oldest`] is past `g`: every live thread
//! of every process has a slot past it, and no thread without a slot is alive. That last part
//! is what makes the fence sound for threads the runtime never saw: the daemon lists the
//! threads the OS has (`births` read before the list and after the slots, unchanged, so no
//! slot was claimed meanwhile) and requires as many online slots as threads. A thread that
//! never reaches a quiescent point holds every range retired after it began; a thread that
//! died without leaving its slot is reaped by its ID missing from the list.
//!
//! Ordering: the daemon's unlinking stores come before its entry stores (release) and the
//! epoch store (sequentially consistent); a thread's acquire of an epoch past `g` therefore
//! sees the unlinking stores, and every call it makes afterwards goes to the new code. A
//! table entry is a seqlock on its grace word: written as `0`, then start and end, then the
//! grace; a reader that sees the grace change skips it, which is safe since a grace not yet
//! below the epoch it read can't be passed by its report anyway.

use std::sync::atomic::{AtomicU64, Ordering, fence};

/// Words of shared memory: the control object's view, or plain memory in tests.
pub trait Words {
    fn words(&self) -> usize;
    fn word(&self, index: usize) -> &AtomicU64;
}

impl Words for Vec<AtomicU64> {
    fn words(&self) -> usize {
        self.len()
    }

    fn word(&self, index: usize) -> &AtomicU64 {
        &self[index]
    }
}

impl Words for &'static [AtomicU64] {
    fn words(&self) -> usize {
        self.len()
    }

    fn word(&self, index: usize) -> &AtomicU64 {
        &self[index]
    }
}

const MAGIC: u64 = u64::from_le_bytes(*b"bzfence1");
const W_MAGIC: usize = 0;
const W_EPOCH: usize = 1;
const W_BIRTHS: usize = 2;
/// One past the highest table entry in use: what a thread scans.
const W_HIGH: usize = 3;
/// What the daemon announces to every run process ([`Fences::announce`]).
const W_ANNOUNCED: usize = 4;

/// Ranges the table holds at once; more wait in the daemon's limbo for room.
pub const ENTRIES: usize = 256;
const W_ENTRIES: usize = 8;
const ENTRY_WORDS: usize = 3;

/// Threads with a slot at once, over every process; a thread finding none stays without one
/// and holds the fences it could pass.
pub const SLOTS: usize = 1024;
/// 128 bytes: the pair of lines arm64 and x86 prefetch together.
const SLOT_WORDS: usize = 16;
const W_SLOTS: usize = (W_ENTRIES + ENTRIES * ENTRY_WORDS).next_multiple_of(SLOT_WORDS);

pub const CONTROL_WORDS: usize = W_SLOTS + SLOTS * SLOT_WORDS;
pub const CONTROL_BYTES: u64 = (CONTROL_WORDS * 8) as u64;

/// A slot's state word: free, being claimed, or online, with its process in the high half.
const FREE: u64 = 0;
const CLAIMING: u64 = 2;
const ONLINE: u64 = 1;

fn state(pid: u32, low: u64) -> u64 {
    (u64::from(pid) << 32) | low
}

fn entry(index: usize) -> usize {
    W_ENTRIES + index * ENTRY_WORDS
}

fn slot(index: usize) -> usize {
    W_SLOTS + index * SLOT_WORDS
}

/// The most frames a quiescent point looks at; a deeper stack reports no progress.
pub const MOST_FRAMES: usize = 512;

/// A grace period: ranges posted under it are free once every thread is past it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Grace(pub u64);

impl Grace {
    /// No grace yet: a range posted, its grace about to start.
    pub const NONE: Grace = Grace(0);

    /// Whether every thread is past it, `oldest` being the oldest `seen` of any.
    pub fn passed_by(self, oldest: u64) -> bool {
        self != Grace::NONE && oldest > self.0
    }
}

/// The daemon's side: it alone writes the epoch and the table.
pub struct Fences<W: Words> {
    words: W,
    /// Which table entries are in use.
    used: [u64; ENTRIES / 64],
    high: usize,
}

impl<W: Words> Fences<W> {
    /// Fences over `words`, zero, of at least [`CONTROL_WORDS`].
    pub fn new(words: W) -> Fences<W> {
        assert!(words.words() >= CONTROL_WORDS, "a control object of {} words", words.words());
        words.word(W_EPOCH).store(1, Ordering::Relaxed);
        words.word(W_MAGIC).store(MAGIC, Ordering::Release);
        Fences { words, used: [0; ENTRIES / 64], high: 0 }
    }

    pub fn words(&self) -> &W {
        &self.words
    }

    pub fn epoch(&self) -> u64 {
        self.words.word(W_EPOCH).load(Ordering::Relaxed)
    }

    /// Posts the run-process addresses `start..end` under the grace the next
    /// [`Fences::start`] returns; `None` when the table is full.
    pub fn post(&mut self, start: u64, end: u64) -> Option<u32> {
        let index = self.used.iter().enumerate().find_map(|(word, bits)| {
            (*bits != u64::MAX).then(|| word * 64 + bits.trailing_ones() as usize)
        })?;
        self.used[index / 64] |= 1 << (index % 64);
        let at = entry(index);
        let grace = self.epoch();
        self.words.word(at).store(0, Ordering::Relaxed);
        fence(Ordering::Release);
        self.words.word(at + 1).store(start, Ordering::Relaxed);
        self.words.word(at + 2).store(end, Ordering::Relaxed);
        self.words.word(at).store(grace, Ordering::Release);
        if index + 1 > self.high {
            self.high = index + 1;
            self.words.word(W_HIGH).store(self.high as u64, Ordering::Release);
        }
        Some(index as u32)
    }

    /// Starts the grace of everything posted since the last one. Call it after the stores
    /// that unlinked what it covers.
    pub fn start(&mut self) -> Grace {
        let epoch = self.epoch();
        fence(Ordering::SeqCst);
        self.words.word(W_EPOCH).store(epoch + 1, Ordering::SeqCst);
        Grace(epoch)
    }

    /// Takes the entry `index` out of the table: its range passed and is free.
    pub fn clear(&mut self, index: u32) {
        let index = index as usize;
        if index >= ENTRIES {
            return;
        }
        self.words.word(entry(index)).store(0, Ordering::Release);
        self.used[index / 64] &= !(1 << (index % 64));
        while self.high > 0 && self.used[(self.high - 1) / 64] & (1 << ((self.high - 1) % 64)) == 0 {
            self.high -= 1;
        }
        self.words.word(W_HIGH).store(self.high as u64, Ordering::Release);
    }

    /// Frees every slot of process `pid`, which exited.
    pub fn forget(&self, pid: u32) {
        for index in 0..SLOTS {
            let word = self.words.word(slot(index));
            let state = word.load(Ordering::Acquire);
            if state != FREE && (state >> 32) as u32 == pid {
                let _ = word.compare_exchange(state, FREE, Ordering::AcqRel, Ordering::Relaxed);
            }
        }
    }

    /// Stores `value` where every run process reads it with one load ([`Thread::announced`]),
    /// and wakes the threads sleeping for it ([`Thread::wait_announced`]).
    pub fn announce(&self, value: u64) {
        let word = self.words.word(W_ANNOUNCED);
        word.store(value, Ordering::Release);
        crate::sys::wake(word);
    }

    /// The count of slot claims so far: read before listing the processes' threads for
    /// [`Fences::oldest`].
    pub fn births(&self) -> u64 {
        self.words.word(W_BIRTHS).load(Ordering::SeqCst)
    }

    /// The oldest `seen` of every live thread of `processes` (each with its threads, listed
    /// by the OS after `births` was read), or `None` when that can't be known now: a thread
    /// has no slot, or one was claimed while looking. Slots of threads no longer listed are
    /// reaped. `u64::MAX` when no process has a thread.
    pub fn oldest(&self, births: u64, processes: &[(u32, &[u64])]) -> Option<u64> {
        fence(Ordering::SeqCst);
        let mut counts = [0usize; 64];
        let mut many: Vec<usize> = Vec::new();
        if processes.len() > counts.len() {
            many = vec![0; processes.len()];
        }
        let mut dead = [0usize; 32];
        let mut dead_len = 0;
        let mut oldest = u64::MAX;
        for index in 0..SLOTS {
            let at = slot(index);
            let state = self.words.word(at).load(Ordering::SeqCst);
            if state == FREE {
                continue;
            }
            let pid = (state >> 32) as u32;
            let Some(process) = processes.iter().position(|it| it.0 == pid) else { continue };
            if state & 3 != ONLINE {
                continue;
            }
            let tid = self.words.word(at + 1).load(Ordering::SeqCst);
            let seen = self.words.word(at + 2).load(Ordering::SeqCst);
            if !processes[process].1.contains(&tid) {
                if dead_len == dead.len() {
                    return None;
                }
                dead[dead_len] = index;
                dead_len += 1;
                continue;
            }
            match many.get_mut(process) {
                Some(count) => *count += 1,
                None => counts[process] += 1,
            }
            oldest = oldest.min(seen);
        }
        if self.words.word(W_BIRTHS).load(Ordering::SeqCst) != births {
            return None;
        }
        for (index, (_, threads)) in processes.iter().enumerate() {
            let count = many.get(index).copied().unwrap_or_else(|| counts.get(index).copied().unwrap_or(0));
            if count != threads.len() {
                return None;
            }
        }
        for &index in &dead[..dead_len] {
            let word = self.words.word(slot(index));
            let state = word.load(Ordering::Acquire);
            if state & 3 == ONLINE {
                let _ = word.compare_exchange(state, FREE, Ordering::AcqRel, Ordering::Relaxed);
            }
        }
        Some(oldest)
    }
}

/// A thread's side, over the same words. It holds no state of its own: the runtime keeps the
/// slot index and the last epoch reported in a thread-local.
pub struct Thread;

impl Thread {
    /// Whether `words` were set up by [`Fences::new`].
    pub fn ready(words: &impl Words) -> bool {
        words.words() >= CONTROL_WORDS && words.word(W_MAGIC).load(Ordering::Acquire) == MAGIC
    }

    /// What the daemon last announced ([`Fences::announce`]).
    pub fn announced(words: &impl Words) -> u64 {
        words.word(W_ANNOUNCED).load(Ordering::Acquire)
    }

    /// Sleeps while what the daemon announced is `seen`, until [`Fences::announce`] wakes it or
    /// `timeout` passes: what is announced then, which may still be `seen` (a timeout, or a
    /// wake for another word on the same futex). The wait compares the word's low 32 bits.
    pub fn wait_announced(words: &impl Words, seen: u64, timeout: std::time::Duration) -> u64 {
        let word = words.word(W_ANNOUNCED);
        let now = word.load(Ordering::Acquire);
        if now != seen {
            return now;
        }
        crate::sys::wait(word, seen as u32, timeout);
        word.load(Ordering::Acquire)
    }

    /// The epoch now.
    pub fn epoch(words: &impl Words) -> u64 {
        words.word(W_EPOCH).load(Ordering::Acquire)
    }

    /// What a thread at a quiescent point reports, having read `epoch` and walked its stack
    /// to `frames` (return addresses, or `None` for a walk that did not finish): `epoch`, or
    /// the oldest grace of a posted range one of its frames returns into.
    pub fn seen(words: &impl Words, epoch: u64, frames: Option<&[u64]>) -> u64 {
        let high = (words.word(W_HIGH).load(Ordering::Acquire) as usize).min(ENTRIES);
        let mut seen = epoch;
        for index in 0..high {
            let at = entry(index);
            let grace = words.word(at).load(Ordering::Acquire);
            if grace == 0 || grace >= seen {
                continue;
            }
            let start = words.word(at + 1).load(Ordering::Relaxed);
            let end = words.word(at + 2).load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            if words.word(at).load(Ordering::Relaxed) != grace {
                continue;
            }
            let inside = match frames {
                // A return address is one past its call: the frame is in the range when the
                // call is.
                Some(frames) => frames.iter().any(|&it| it > start && it <= end),
                None => true,
            };
            if inside {
                seen = grace;
            }
        }
        seen
    }

    /// Claims a slot for thread `tid` of process `pid`, reporting `seen`; `None` when every
    /// slot is taken.
    pub fn claim(words: &impl Words, pid: u32, tid: u64, seen: u64) -> Option<usize> {
        Thread::begin_claim(words);
        Thread::finish_claim(words, pid, tid, seen)
    }

    /// The first half of [`Thread::claim`]: counts the birth, so a daemon looking meanwhile
    /// knows not to trust its count of slots.
    pub fn begin_claim(words: &impl Words) {
        words.word(W_BIRTHS).fetch_add(1, Ordering::SeqCst);
    }

    /// The second half of [`Thread::claim`]. A slot online under this thread's own IDs was
    /// left by a dead thread whose ID the OS gave this one: it is adopted, so the daemon never
    /// counts two slots for one listed thread.
    pub fn finish_claim(words: &impl Words, pid: u32, tid: u64, seen: u64) -> Option<usize> {
        for index in 0..SLOTS {
            let at = slot(index);
            let word = words.word(at);
            if word.load(Ordering::Relaxed) != state(pid, ONLINE) || words.word(at + 1).load(Ordering::SeqCst) != tid {
                continue;
            }
            if word
                .compare_exchange(state(pid, ONLINE), state(pid, CLAIMING), Ordering::AcqRel, Ordering::Relaxed)
                .is_err()
            {
                continue;
            }
            if words.word(at + 1).load(Ordering::SeqCst) != tid {
                word.store(state(pid, ONLINE), Ordering::SeqCst);
                continue;
            }
            words.word(at + 2).store(seen, Ordering::SeqCst);
            word.store(state(pid, ONLINE), Ordering::SeqCst);
            return Some(index);
        }
        for index in 0..SLOTS {
            let at = slot(index);
            let word = words.word(at);
            if word.load(Ordering::Relaxed) != FREE {
                continue;
            }
            if word.compare_exchange(FREE, state(pid, CLAIMING), Ordering::AcqRel, Ordering::Relaxed).is_err() {
                continue;
            }
            words.word(at + 1).store(tid, Ordering::SeqCst);
            words.word(at + 2).store(seen, Ordering::SeqCst);
            word.store(state(pid, ONLINE), Ordering::SeqCst);
            return Some(index);
        }
        None
    }

    /// Reports `seen` in the thread's slot `index`.
    pub fn report(words: &impl Words, index: usize, seen: u64) {
        if index < SLOTS {
            words.word(slot(index) + 2).store(seen, Ordering::SeqCst);
        }
    }

    /// Whether the slot `index` is still the thread's: the daemon reaps a slot whose thread it
    /// did not find listed, and a thread then claims another.
    pub fn holds(words: &impl Words, index: usize, pid: u32, tid: u64) -> bool {
        index < SLOTS
            && words.word(slot(index)).load(Ordering::Acquire) == state(pid, ONLINE)
            && words.word(slot(index) + 1).load(Ordering::Acquire) == tid
    }

    /// Gives the slot `index` back: the thread is ending with no frame in a posted range.
    pub fn release(words: &impl Words, index: usize) {
        if index < SLOTS {
            words.word(slot(index)).store(FREE, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod model;
#[cfg(test)]
mod tests;
