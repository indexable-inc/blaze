//! Asks from a run process to the daemon: any number at once, from any thread of the run, from
//! TLS destructors and `atexit` handlers, and from a signal handler that interrupted its own
//! thread in the middle of an ask.
//!
//! Each launch has an object of its own for them, the asks object (object 2 of the grant),
//! mapped shared and writable by the daemon and the run process: a header, then [`SLOTS`] slots
//! of 4 KiB. A slot's state word is its generation (24 bits) and its phase:
//!
//! - The run process ([`crate::runtime::Attached::request_into`], [`request`]) claims a slot (one
//!   compare-and-swap on the header's bitmap), writes the ask's kind, length and body into it,
//!   stores `ASKED` and rings the doorbell: one byte on the channel (Unix), an event the daemon
//!   made (Windows). It sleeps on the slot's state word (a futex, `__ulock_wait` on macOS, both
//!   shared across processes; the slot's event on Windows) until `ANSWERED`, copies the answer
//!   into the caller's buffer, and frees the slot under the next generation.
//! - The daemon's [`Reader`] wakes on the doorbell, takes each `ASKED` slot (`TAKEN`, one
//!   compare-and-swap) and hands it out as a frame whose sequence number names the slot and its
//!   generation. A [`Sender`], on any thread, takes the slot to `ANSWERING` (compare-and-swap: an
//!   ask is answered once, never one of another generation), writes the answer, stores
//!   `ANSWERED` and wakes the slot.
//!
//! Nothing is locked: an asker holds only its slot, and each call it makes (atomics, `send`,
//! `futex`, `__ulock_wait`, `poll`, `SetEvent`, `WaitForSingleObject`) is async-signal-safe, so
//! a signal handler that asks while its thread is inside an ask takes another slot and is
//! answered on its own. Nothing is allocated: bodies move between the caller's buffers and the
//! slot. A thread that finds every slot claimed sleeps on the header's count of frees until one
//! comes back; other threads' slots come back as they are answered, so only asks nested
//! [`SLOTS`] deep on interrupted threads could wait for ever. `ask/model.rs` checks the
//! protocol under every interleaving.
//!
//! Before it sleeps, an asker spins on its state word for about one cross-process wake-up
//! ([`ASKER_SPIN`]), what the sleep and wake would cost it, and the reader rescans the slots for
//! [`READER_SPIN`] after the last ask it took, so a burst of first calls finds neither asleep.
//! A spin only reads the words the sleep compares, and the clock (`clock_gettime`,
//! `QueryPerformanceCounter`), so the asker's side stays async-signal-safe.
//!
//! The run process is not trusted: the daemon reads each length once and bounds it, and a slot
//! in a state it did not expect is skipped or refused. A run process whose daemon is gone wakes
//! every second and finds its channel hung up.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::channel::Channel;
use crate::fence::Words;
use crate::frame::{Frame, Kind};
use crate::{Error, sys};

#[cfg(test)]
mod model;
#[cfg(all(test, target_os = "linux"))]
mod tests;

/// The asks one run process has in flight at most.
pub const SLOTS: usize = 64;
/// The longest body of an ask or an answer.
pub const MOST_BODY: usize = (SLOT_WORDS - S_BODY) * 8;

const MAGIC: u64 = u64::from_le_bytes(*b"bzasks01");
const W_MAGIC: usize = 0;
/// Bit `i` set: slot `i` is claimed by an asker.
const W_USED: usize = 1;
/// Counts slots given back, for a thread waiting for one.
const W_FREES: usize = 2;
/// The threads waiting for a slot.
const W_WAITING: usize = 3;
/// What the run process names the doorbell by, then each slot's bell: on Windows the handles of
/// the events the daemon duplicated into it, 0 elsewhere.
const W_BELLS: usize = 8;
const SLOT_WORDS: usize = 512;
const W_SLOTS: usize = (W_BELLS + 1 + SLOTS).next_multiple_of(SLOT_WORDS);
const S_STATE: usize = 0;
/// The ask's kind and length: `kind | len << 16`.
const S_ASK: usize = 1;
/// The answer's, likewise.
const S_ANSWER: usize = 2;
const S_BODY: usize = 4;
pub(crate) const WORDS: usize = W_SLOTS + SLOTS * SLOT_WORDS;
pub(crate) const BYTES: u64 = (WORDS * 8) as u64;

const FREE: u64 = 0;
const ASKED: u64 = 1;
const TAKEN: u64 = 2;
const ANSWERING: u64 = 3;
const ANSWERED: u64 = 4;
const GENERATIONS: u64 = 1 << 24;

/// How long an asker sleeps before it looks whether the daemon is still there.
const LOOK: Duration = Duration::from_secs(1);

/// How long an asker spins for its answer before each sleep: about one cross-process wake-up.
const ASKER_SPIN: Duration = Duration::from_micros(3);

/// How long the reader rescans the slots after the last ask it took before it sleeps on the
/// doorbell: one daemon core, after a burst.
const READER_SPIN: Duration = Duration::from_micros(50);

fn state(generation: u64, phase: u64) -> u64 {
    generation << 8 | phase
}

fn base(index: usize) -> usize {
    W_SLOTS + index * SLOT_WORDS
}

fn seq(index: usize, generation: u64) -> u32 {
    (generation << 8) as u32 | index as u32
}

fn put(words: &impl Words, at: usize, bytes: &[u8]) {
    for (index, chunk) in bytes.chunks(8).enumerate() {
        let mut word = [0; 8];
        word[..chunk.len()].copy_from_slice(chunk);
        words.word(at + index).store(u64::from_le_bytes(word), Ordering::Relaxed);
    }
}

fn get(words: &impl Words, at: usize, out: &mut [u8]) {
    for (index, chunk) in out.chunks_mut(8).enumerate() {
        let word = words.word(at + index).load(Ordering::Relaxed).to_le_bytes();
        chunk.copy_from_slice(&word[..chunk.len()]);
    }
}

/// A new asks object, zero, set up for its run process.
pub(crate) fn prepare(words: &impl Words) {
    words.word(W_MAGIC).store(MAGIC, Ordering::Release);
}

/// Writes what the run process names its bells by ([`sys::Bells::new`]): the doorbell's, then
/// each slot's. Before the start hook can ask anything.
pub(crate) fn name_bells(words: &impl Words, names: &[u64]) {
    for (index, name) in names.iter().take(SLOTS + 1).enumerate() {
        words.word(W_BELLS + index).store(*name, Ordering::Release);
    }
}

/// Whether `words` is an asks object the daemon set up.
pub(crate) fn ready(words: &impl Words) -> bool {
    words.words() >= WORDS && words.word(W_MAGIC).load(Ordering::Acquire) == MAGIC
}

/// A free slot, claimed: its index, or `None` when every slot is claimed.
fn claim(words: &impl Words) -> Option<usize> {
    let used = words.word(W_USED);
    let mut seen = used.load(Ordering::SeqCst);
    loop {
        if seen == u64::MAX {
            return None;
        }
        let index = (!seen).trailing_zeros() as usize;
        match used.compare_exchange_weak(seen, seen | 1 << index, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return Some(index),
            Err(now) => seen = now,
        }
    }
}

/// Sleeps until a slot may have come back, or `timeout`: the caller found every slot claimed.
fn wait_for_a_slot(words: &impl Words, timeout: Duration) {
    let waiting = words.word(W_WAITING);
    waiting.fetch_add(1, Ordering::SeqCst);
    let frees = words.word(W_FREES).load(Ordering::SeqCst);
    if words.word(W_USED).load(Ordering::SeqCst) == u64::MAX {
        sys::wait(words.word(W_FREES), frees as u32, timeout);
    }
    waiting.fetch_sub(1, Ordering::SeqCst);
}

/// Puts an ask of `kind` with `body` (at most [`MOST_BODY`] bytes) in claimed slot `index` for
/// the daemon: its generation.
fn ask(words: &impl Words, index: usize, kind: Kind, body: &[u8]) -> u64 {
    let slot = base(index);
    let generation = words.word(slot + S_STATE).load(Ordering::Acquire) >> 8;
    put(words, slot + S_BODY, body);
    words.word(slot + S_ASK).store(u64::from(kind.0) | (body.len() as u64) << 16, Ordering::Relaxed);
    words.word(slot + S_STATE).store(state(generation, ASKED), Ordering::Release);
    generation
}

/// Whether slot `index`'s ask of `generation` was answered; when not, the state word as it was,
/// to sleep on.
fn answered(words: &impl Words, index: usize, generation: u64) -> Result<(), u64> {
    let now = words.word(base(index) + S_STATE).load(Ordering::Acquire);
    if now == state(generation, ANSWERED) { Ok(()) } else { Err(now) }
}

/// [`answered`], spinning for up to `spin` while it is not.
fn answered_within(words: &impl Words, index: usize, generation: u64, spin: Duration) -> Result<(), u64> {
    let start = Instant::now();
    let mut relax = blaze_spin::SpinWait::new();
    loop {
        let seen = answered(words, index, generation);
        if seen.is_ok() || start.elapsed() >= spin {
            return seen;
        }
        relax.relax();
    }
}

fn state_word(words: &impl Words, index: usize) -> &AtomicU64 {
    words.word(base(index) + S_STATE)
}

/// What the run process rings the daemon with.
fn doorbell(words: &impl Words) -> u64 {
    words.word(W_BELLS).load(Ordering::Acquire)
}

/// What the run process sleeps on for slot `index`'s answer, besides its state word.
fn bell(words: &impl Words, index: usize) -> u64 {
    words.word(W_BELLS + 1 + index).load(Ordering::Acquire)
}

/// The answer in slot `index`: its kind and whole length, with as much of its body as fits put
/// in `out`.
fn answer(words: &impl Words, index: usize, out: &mut [u8]) -> Result<(Kind, usize), Error> {
    let slot = base(index);
    let word = words.word(slot + S_ANSWER).load(Ordering::Relaxed);
    let (kind, len) = (Kind(word as u16), (word >> 16) as usize);
    if len > MOST_BODY {
        return Err(Error::new(format!("the daemon's answer in slot {index} is {len} bytes, past the {MOST_BODY} a slot holds")));
    }
    let fits = len.min(out.len());
    get(words, slot + S_BODY, &mut out[..fits]);
    Ok((kind, len))
}

/// Gives back slot `index`, its ask of `generation` answered and read.
fn release(words: &impl Words, index: usize, generation: u64) {
    words.word(base(index) + S_STATE).store(state((generation + 1) % GENERATIONS, FREE), Ordering::Release);
    words.word(W_USED).fetch_and(!(1 << index), Ordering::SeqCst);
    words.word(W_FREES).fetch_add(1, Ordering::SeqCst);
    if words.word(W_WAITING).load(Ordering::SeqCst) != 0 {
        sys::wake(words.word(W_FREES));
    }
}

/// The run process's ask on `words`, the doorbell `stream`: [`crate::runtime::Attached::request_into`].
pub(crate) fn request(
    words: &impl Words,
    stream: &sys::Stream,
    kind: Kind,
    body: &[u8],
    out: &mut [u8],
) -> Result<(Kind, usize), Error> {
    if body.len() > MOST_BODY {
        return Err(Error::new(format!("an ask of {} bytes, past the {MOST_BODY} a slot holds", body.len())));
    }
    let _errno = sys::Errno::keep();
    let index = loop {
        if let Some(index) = claim(words) {
            break index;
        }
        wait_for_a_slot(words, LOOK);
        if sys::hung_up(stream) {
            return Err(Error::new("the daemon hung up while every ask slot was taken"));
        }
    };
    let generation = ask(words, index, kind, body);
    if let Err(error) = sys::ring(stream, doorbell(words)) {
        // The slot stays claimed: the daemon may yet take the ask and write into it.
        return Err(error);
    }
    let bell = bell(words, index);
    while let Err(seen) = answered_within(words, index, generation, ASKER_SPIN) {
        if !sys::wait_answer(state_word(words, index), seen as u32, bell, LOOK) && sys::hung_up(stream) {
            return Err(Error::new("the daemon hung up before it answered"));
        }
    }
    let answered = answer(words, index, out);
    release(words, index, generation);
    answered
}

/// The next asked slot from `from` on, taken: its index, generation and kind, its body put in
/// `body`.
fn take(words: &impl Words, from: usize, body: &mut Vec<u8>) -> Result<Option<(usize, u64, Kind)>, Error> {
    for step in 0..SLOTS {
        let index = (from + step) % SLOTS;
        let slot = base(index);
        let now = words.word(slot + S_STATE).load(Ordering::Acquire);
        if now & 0xff != ASKED {
            continue;
        }
        let taken = state(now >> 8, TAKEN);
        if words.word(slot + S_STATE).compare_exchange(now, taken, Ordering::AcqRel, Ordering::Acquire).is_err() {
            continue;
        }
        let ask = words.word(slot + S_ASK).load(Ordering::Relaxed);
        let len = (ask >> 16) as usize;
        if len > MOST_BODY {
            return Err(Error::new(format!("an ask in slot {index} of {len} bytes, past the {MOST_BODY} a slot holds")));
        }
        body.resize(len, 0);
        get(words, slot + S_BODY, body);
        return Ok(Some((index, now >> 8, Kind(ask as u16))));
    }
    Ok(None)
}

/// Puts the answer to ask `seq` in its slot: the slot's index.
fn answer_into(words: &impl Words, seq: u32, kind: Kind, body: &[u8]) -> Result<usize, Error> {
    let index = (seq & 0xff) as usize;
    let generation = u64::from(seq >> 8);
    if index >= SLOTS {
        return Err(Error::new(format!("an answer to ask {seq:#x}, which names no slot")));
    }
    if body.len() > MOST_BODY {
        return Err(Error::new(format!("an answer of {} bytes, past the {MOST_BODY} a slot holds", body.len())));
    }
    let slot = base(index);
    let word = words.word(slot + S_STATE);
    if let Err(now) =
        word.compare_exchange(state(generation, TAKEN), state(generation, ANSWERING), Ordering::Acquire, Ordering::Relaxed)
    {
        return Err(Error::new(format!("no ask {seq:#x} waits for an answer: its slot is in state {now:#x}")));
    }
    put(words, slot + S_BODY, body);
    words.word(slot + S_ANSWER).store(u64::from(kind.0) | (body.len() as u64) << 16, Ordering::Relaxed);
    word.store(state(generation, ANSWERED), Ordering::Release);
    Ok(index)
}

/// The daemon's side of one run process's asks: its view of the asks object, its bells, and
/// what ends the process.
pub(crate) struct Run {
    words: sys::View,
    bells: sys::Bells,
    remote: sys::Remote,
    pid: u32,
    killed: Mutex<Option<String>>,
}

impl Run {
    pub(crate) fn new(words: sys::View, bells: sys::Bells, remote: sys::Remote, pid: u32) -> Run {
        Run { words, bells, remote, pid, killed: Mutex::new(None) }
    }

    /// Ends the process, `why` its run's error (the first one, when ended twice). Only while it
    /// has not been reaped: on macOS the process ID is all the daemon holds of it.
    pub(crate) fn kill(&self, why: &str) -> Result<(), Error> {
        self.killed.lock().get_or_insert_with(|| why.to_owned());
        self.remote.kill().map_err(|it| Error::new(format!("ending process {}: {it}", self.pid)))
    }

    pub(crate) fn killed(&self) -> Option<String> {
        self.killed.lock().clone()
    }
}

/// Where the daemon reads one run process's asks, on a thread of its own.
pub struct Reader {
    channel: Channel,
    run: Arc<Run>,
    next: usize,
    body: Vec<u8>,
    /// When it last returned an ask, for [`READER_SPIN`].
    last: Option<Instant>,
}

impl Reader {
    pub(crate) fn new(channel: Channel, run: Arc<Run>) -> Reader {
        Reader { channel, run, next: 0, body: Vec::new(), last: None }
    }

    /// The next ask, as a frame whose sequence number is what [`Sender::send`] answers it by;
    /// it lives until the next call. Blocks until one comes. An error when the process hung up
    /// (it ended) or wrote an ask no slot holds (end it).
    pub fn recv(&mut self) -> Result<Frame<'_>, Error> {
        let mut relax = blaze_spin::SpinWait::new();
        loop {
            if let Some((index, generation, kind)) = take(&self.run.words, self.next, &mut self.body)? {
                self.next = (index + 1) % SLOTS;
                self.last = Some(Instant::now());
                return Ok(Frame { kind, seq: seq(index, generation), body: &self.body });
            }
            // The rings of asks taken while spinning are drained by the next `wait_ring`.
            if self.last.is_some_and(|last| last.elapsed() < READER_SPIN) {
                relax.relax();
                continue;
            }
            if !self.run.bells.wait_ring(self.channel.stream())? {
                return Err(Error::new(format!("process {} hung up", self.run.pid)));
            }
        }
    }
}

/// Answers one run process's asks, from any thread.
#[derive(Clone)]
pub struct Sender {
    run: Arc<Run>,
}

impl Sender {
    pub(crate) fn new(run: Arc<Run>) -> Sender {
        Sender { run }
    }

    pub fn pid(&self) -> u32 {
        self.run.pid
    }

    /// Answers the ask of sequence number `seq` with `kind` and `body` (at most [`MOST_BODY`]
    /// bytes). Once: another answer to it, or one to an ask that is not waiting, is an error.
    pub fn send(&self, kind: Kind, seq: u32, body: &[u8]) -> Result<(), Error> {
        let index = answer_into(&self.run.words, seq, kind, body)?;
        self.run.bells.wake(index, state_word(&self.run.words, index));
        Ok(())
    }

    /// Ends the run process, `why` its run's error, as `host::Session::kill` does.
    pub fn kill(&self, why: &str) -> Result<(), Error> {
        self.run.kill(why)
    }
}
