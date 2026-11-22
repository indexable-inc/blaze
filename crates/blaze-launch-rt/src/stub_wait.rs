//! A stub's wait for its slot's code, with no allocation, no lock and a deadline
//! (THREADS.md design D6).
//!
//! A program thread that calls a function whose code has not come yet runs the slot's stub,
//! which lands in `table::resolve`. That thread may be any thread, at any point of its life:
//! inside a signal handler (signal-hook's registry calls registered closures from one), while
//! its thread-locals are being destroyed, or holding `malloc`'s lock in the frame it
//! interrupted. So the stub's path calls only what POSIX lets a handler call: atomics, the
//! kernel's address wait (`os_sync_wait_on_address_with_deadline` on macOS, `futex` on Linux),
//! `write` and `_exit`. It allocates nothing and takes no lock.
//!
//! **The table.** [`Waiters`] is `most_waiters` [`Row`]s and a ring of row indices, reserved
//! once at the launcher's start with one `mmap`. A stub:
//! 1. claims a free row by compare-and-swap ([`Waiters::claim`]): FREE to WAITING;
//! 2. writes its slot and thread into the row;
//! 3. pushes the row's index onto the ring (a bounded lock-free queue: Vyukov's sequence
//!    cells), whose release publishes the row's fields;
//! 4. rings the control thread (`Shared::ring`: a counter word the control thread waits on);
//! 5. waits on the row's `state` word ([`Waiters::wait`]) to a deadline: the stub bound
//!    ([`STUB_BOUND`], FirstCall's `ask_bound` plus one second, on the monotonic clock);
//! 6. reads the answer and frees the row ([`Waiters::release`]).
//!
//! The control thread pops row indices ([`Waiters::popped`]), and answers each row once
//! ([`Waiters::answer`]): a store of BOUND, REFUSED or LOST and a wake of the word. A refusal's
//! words are said by the control thread itself ([`say_no_code`]) before it answers: the
//! waiter has nothing to read but the state, and on REFUSED or LOST ends the process.
//!
//! **Capacity.** A row is pushed once per claim and popped before it is answered, and it is
//! freed only after its answer: so the ring never holds more indices than there are rows, and
//! a ring of `most_waiters` cells never refuses a push of a claimed row.
//!
//! **Stops.** No free row ([`Full`]) and no answer by the deadline ([`Overran`]) stop the
//! process (rule 18) with one `write` of a line built on the stack and `_exit(101)`.
//!
//! **Proof.** stub_wait_tests.rs (threads on the real kernel wait, the deadline, and the
//! allocation count of the stub's path) and stub_wait_model.rs (the claim, ring and answer
//! under the interleaving checker, `--cfg blaze_check`, with the planted bug
//! [`Plant::PushBeforeSlot`] its oracle catches). Under `blaze_check` the atomics are the
//! checker's and the kernel wait is a yield loop to the same deadline: the model checks the
//! order of the stores and loads; the kernel's compare-and-block is the one step it does not.

#[cfg(blaze_check)]
use blaze_check::sync::atomic::{AtomicU32, AtomicU64, Ordering};
#[cfg(not(blaze_check))]
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use core::{num::NonZeroU32, ptr, time::Duration};

use blaze_core::wait::{Clock, Deadline};
#[cfg(not(blaze_check))]
use blaze_core::wait::{Waited, wait_shared, wake_shared};
use blaze_sys::Fd;
use blaze_sys::posix::io::{MapFlags, Prot, mmap, munmap, write};
use blaze_sys::posix::signal::exit_now;

use crate::sys::SysError;

/// A row no stub holds.
pub const FREE: u64 = 0;
/// A stub waits in the row for its answer.
pub const WAITING: u64 = 1;
/// The slot's word names code: the stub calls through it.
pub const BOUND: u64 = 2;
/// The function does not compile: the control thread said why; the process stops.
pub const REFUSED: u64 = 3;
/// No code can come (the parent or daemon is gone): the control thread said why; the process
/// stops.
pub const LOST: u64 = 4;

/// The most threads waiting in stubs at once: run mode's FirstCall `Config::most_waiters`
/// (`run_mode::MOST_WAITERS`), for the old daemon path's driver too.
pub const MOST_WAITERS: NonZeroU32 = match NonZeroU32::new(crate::run_mode::MOST_WAITERS) {
    Some(most) => most,
    None => panic!("run_mode::MOST_WAITERS is 0: a stub could never wait"),
};

/// How long a stub waits for its code before the process stops (rule 18): FirstCall's
/// `ask_bound` (`run_mode::ASK_BOUND`, after which the control thread fails the ask itself)
/// and one second more, so the control thread's own answer comes first when it can.
pub const STUB_BOUND: Duration = Duration::from_nanos(crate::run_mode::ASK_BOUND.0.saturating_add(1_000_000_000));

/// What [`Waiters::wait`] names as the wait it is (rule 15).
#[cfg(not(blaze_check))]
const WHAT: &str = "a stub's wait for its slot's code (stub_wait::Waiters::wait)";

/// The status the process ends with when a stub stops it.
const STOPPED: i32 = 101;

/// One waiting thread's row: its own cache line, so two stubs' waits never share one.
#[repr(C, align(64))]
pub struct Row {
    /// [`FREE`], [`WAITING`], [`BOUND`], [`REFUSED`] or [`LOST`]. The word the stub's thread
    /// sleeps on (its low half, as the kernel's address wait compares 32 bits).
    state: AtomicU64,
    /// The slot whose code the thread waits for. Written by the claimer before the push.
    slot: AtomicU32,
    /// The launcher's id of the thread (0 for a thread that is not a mutator).
    thread: AtomicU64,
}

impl Row {
    fn new() -> Row {
        Row { state: AtomicU64::new(FREE), slot: AtomicU32::new(0), thread: AtomicU64::new(0) }
    }
}

/// One cell of the ring: Vyukov's bounded queue. `seq` is the position the cell is next
/// written at (free) or that plus one (full); `row` is the index it holds.
struct Cell {
    seq: AtomicU64,
    row: AtomicU32,
}

/// A fixed-capacity lock-free queue of row indices: any thread pushes, the control thread
/// pops (any number may: a pop claims its position by compare-and-swap). No allocation: its
/// cells are in the [`Waiters`]' mapping.
pub struct Ring {
    cells: &'static [Cell],
    /// The next position to pop.
    head: AtomicU64,
    /// The next position to push.
    tail: AtomicU64,
}

/// The most compare-and-swap races one push or pop may lose before it says so (rule 18): each
/// lost race is another thread's finished step, and there are at most `most_waiters` pushers.
const RACES: u32 = 1 << 20;

/// A push the ring could not take: it held as many indices as it has cells, or lost
/// [`RACES`] races. Either is the launcher's own fault (the module's "Capacity").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct RingRefused;

impl Ring {
    fn capacity(&self) -> u64 {
        self.cells.len() as u64
    }

    /// The cell of position `pos`.
    fn cell(&self, pos: u64) -> &Cell {
        // `capacity` is not zero (a `NonZeroU32` of rows), and the remainder is below it.
        &self.cells[(pos % self.capacity()) as usize]
    }

    fn push(&self, row: u32) -> Result<(), RingRefused> {
        let mut pos = self.tail.load(Ordering::Relaxed);
        for _ in 0..RACES {
            let cell = self.cell(pos);
            let seq = cell.seq.load(Ordering::Acquire);
            match (seq as i64).wrapping_sub(pos as i64) {
                0 => match self.tail.compare_exchange_weak(pos, pos + 1, Ordering::Relaxed, Ordering::Relaxed) {
                    Ok(_) => {
                        cell.row.store(row, Ordering::Relaxed);
                        // Publishes the index, and with it every store this thread made before
                        // the push (the row's slot and thread): the pop's acquire of `seq`.
                        cell.seq.store(pos + 1, Ordering::Release);
                        return Ok(());
                    }
                    Err(now) => pos = now,
                },
                // The cell still holds the index pushed a lap ago: the ring is full.
                behind if behind < 0 => return Err(RingRefused),
                // Another pusher took `pos`: look at the tail again.
                _ => pos = self.tail.load(Ordering::Relaxed),
            }
        }
        Err(RingRefused)
    }

    fn pop(&self) -> Option<u32> {
        let mut pos = self.head.load(Ordering::Relaxed);
        for _ in 0..RACES {
            let cell = self.cell(pos);
            let seq = cell.seq.load(Ordering::Acquire);
            match (seq as i64).wrapping_sub(pos.wrapping_add(1) as i64) {
                0 => match self.head.compare_exchange_weak(pos, pos + 1, Ordering::Relaxed, Ordering::Relaxed) {
                    Ok(_) => {
                        let row = cell.row.load(Ordering::Relaxed);
                        // The cell is free for the push a lap later.
                        cell.seq.store(pos + self.capacity(), Ordering::Release);
                        return Some(row);
                    }
                    Err(now) => pos = now,
                },
                // Not yet published (a pusher between its tail and its `seq` store), or empty:
                // the pusher rings the control thread after it publishes.
                behind if behind < 0 => return None,
                _ => pos = self.head.load(Ordering::Relaxed),
            }
        }
        // A pop that lost every race: others popped; this one finds nothing now.
        None
    }
}

/// A row the control thread popped: which, and what its thread waits for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Waiting {
    pub row: u32,
    pub slot: u32,
    pub thread: u64,
}

/// What a stub's wait ended with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Answer {
    /// The slot's word names code.
    Bound,
    /// The function does not compile; the control thread said why.
    Refused,
    /// No code can come; the control thread said why.
    Lost,
}

impl Answer {
    fn state(self) -> u64 {
        match self {
            Answer::Bound => BOUND,
            Answer::Refused => REFUSED,
            Answer::Lost => LOST,
        }
    }
}

/// No free row: more than `most` threads wait for code at once.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Full {
    pub slot: u32,
    pub most: u32,
}

impl Full {
    /// Ends the process, saying why: one `write` and `_exit` (async-signal-safe).
    pub fn stop(&self) -> ! {
        let mut line = Line::new();
        line.text("blaze: the function of slot ")
            .number(u64::from(self.slot))
            .text(" was called and more than ")
            .number(u64::from(self.most))
            .text(" threads wait for code at once (most_waiters, rule 18)\n");
        line.stop()
    }
}

/// A stub's wait that ended with no answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Overran {
    /// The deadline passed: `bound` is how long the wait was given.
    Deadline { slot: u32, bound: Duration },
    /// The kernel's wait failed, with its `errno` when it gave one.
    Failed { slot: u32, errno: Option<i32> },
}

impl Overran {
    /// Ends the process, saying why: one `write` and `_exit` (async-signal-safe).
    pub fn stop(&self) -> ! {
        let mut line = Line::new();
        match *self {
            Overran::Deadline { slot, bound } => {
                line.text("blaze: the function of slot ")
                    .number(u64::from(slot))
                    .text(" was called and its code did not come within ")
                    .number(u64::try_from(bound.as_millis()).unwrap_or(u64::MAX))
                    .text(" ms (rule 18)\n");
            }
            Overran::Failed { slot, errno } => {
                line.text("blaze: the function of slot ")
                    .number(u64::from(slot))
                    .text(" was called and its wait for the code failed in the kernel");
                if let Some(errno) = errno {
                    line.text(" with errno ").number(u64::from(errno.unsigned_abs()));
                }
                line.text(" (rule 18)\n");
            }
        }
        line.stop()
    }
}

/// Where a stub's claim is wrong on purpose: the model test's control (rule 53).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// The row's index is pushed before its slot and thread are written: the control thread
    /// can pop it and answer BOUND for a slot the row does not hold yet.
    #[cfg_attr(not(all(test, blaze_check)), expect(dead_code, reason = "the claim's planted-bug control (stub_wait_model.rs)"))]
    PushBeforeSlot,
}

/// The rows and the ring: see the module's documentation.
pub struct Waiters {
    rows: &'static [Row],
    ring: Ring,
}

impl Waiters {
    /// The bytes of the mapping for `most` rows and as many cells.
    fn bytes(most: usize) -> usize {
        most * size_of::<Row>() + most * size_of::<Cell>()
    }

    /// Reserves `most` rows and a ring of `most` cells with one anonymous `mmap`: what the
    /// launcher does once, at its start.
    ///
    /// # Errors
    ///
    /// The `mmap` failed, with its `errno`.
    pub fn reserve(most: NonZeroU32) -> Result<Waiters, SysError> {
        let count = most.get() as usize;
        let len = Waiters::bytes(count);
        // SAFETY: an anonymous private mapping of `len` bytes, at an address the kernel picks;
        // nothing else is named or changed.
        let at = unsafe { mmap(None, len, Prot::READ | Prot::WRITE, MapFlags::PRIVATE | MapFlags::ANONYMOUS, None, 0) }
            .map_err(|error| SysError { call: "mmap (the stub waiters' rows)", errno: i32::from(error.raw()), at: 0, len })?
            .as_ptr();
        let rows = at.cast::<Row>();
        // The cells follow the rows: `count * size_of::<Row>()` is a multiple of 64 (a row's
        // alignment), so the cells' start is aligned for a `Cell` (at most 8).
        // SAFETY: inside the mapping: `count` rows come first, then `count` cells (`bytes`).
        let cells = unsafe { at.cast::<u8>().add(count * size_of::<Row>()) }.cast::<Cell>();
        for index in 0..count {
            // SAFETY: `index` < `count`: each write is to a row's or a cell's own bytes in the
            // fresh mapping (page aligned, so aligned for both), which holds no value yet.
            unsafe {
                rows.add(index).write(Row::new());
                cells.add(index).write(Cell { seq: AtomicU64::new(index as u64), row: AtomicU32::new(0) });
            }
        }
        // SAFETY: `count` initialized rows and cells, written above, in a mapping only this
        // `Waiters` unmaps (`Drop`), and only lent out through `&self`: no reference made
        // from these slices outlives the `Waiters`, though the type says `'static`.
        let (rows, cells) = unsafe {
            (core::slice::from_raw_parts(rows, count), core::slice::from_raw_parts(cells.cast_const(), count))
        };
        Ok(Waiters { rows, ring: Ring { cells, head: AtomicU64::new(0), tail: AtomicU64::new(0) } })
    }

    /// How many rows there are: `most_waiters`.
    pub fn most(&self) -> u32 {
        // At most a `NonZeroU32`'s value (`reserve`).
        self.rows.len() as u32
    }

    /// Claims a free row for a thread that waits for `slot`'s code, writes the slot and the
    /// thread into it and pushes its index onto the ring. The caller then rings the control
    /// thread (`Shared::ring`) and [`Waiters::wait`]s. Allocates nothing, takes no lock.
    ///
    /// # Errors
    ///
    /// Every row is claimed: [`Full`], which the stub stops the process with.
    pub fn claim(&self, slot: u32, thread: u64) -> Result<u32, Full> {
        self.claim_with(slot, thread, Plant::None)
    }

    /// [`Waiters::claim`], with `plant` for the model test's control.
    pub(crate) fn claim_with(&self, slot: u32, thread: u64, plant: Plant) -> Result<u32, Full> {
        let full = Full { slot, most: self.most() };
        for (index, row) in self.rows.iter().enumerate() {
            // Acquire: the row's last holder released it after reading its answer.
            if row.state.compare_exchange(FREE, WAITING, Ordering::Acquire, Ordering::Relaxed).is_err() {
                continue;
            }
            // Below `most`, a `u32`.
            let index = index as u32;
            let pushed = match plant {
                Plant::None => {
                    row.slot.store(slot, Ordering::Relaxed);
                    row.thread.store(thread, Ordering::Relaxed);
                    self.ring.push(index)
                }
                Plant::PushBeforeSlot => {
                    let pushed = self.ring.push(index);
                    row.slot.store(slot, Ordering::Relaxed);
                    row.thread.store(thread, Ordering::Relaxed);
                    pushed
                }
            };
            // A ring that refuses a claimed row's index is past the capacity the module's
            // "Capacity" proves: as full as no free row, and named the same way.
            return match pushed {
                Ok(()) => Ok(index),
                Err(RingRefused) => Err(full),
            };
        }
        Err(full)
    }

    /// The next row a stub pushed, with its slot and thread: the control thread's look.
    pub fn popped(&self) -> Option<Waiting> {
        let row = self.ring.pop()?;
        let at = self.row(row);
        // Relaxed: the ring's acquire of the cell's `seq` ordered these after the claimer's
        // stores (`Ring::push`).
        Some(Waiting { row, slot: at.slot.load(Ordering::Relaxed), thread: at.thread.load(Ordering::Relaxed) })
    }

    /// Sleeps until row `row` is answered or `deadline` passes on `clock`: the stub's one
    /// wait, on the kernel's address wait of the row's state word (blaze-core's direct
    /// driver on Linux: the launcher chooses it at its start). Allocates nothing on its way
    /// to an answer or the deadline.
    ///
    /// # Errors
    ///
    /// [`Overran`]: no answer by the deadline, or the kernel's wait failed.
    pub fn wait(&self, row: u32, deadline: Deadline, clock: &impl Clock) -> Result<Answer, Overran> {
        let at = self.row(row);
        let slot = at.slot.load(Ordering::Relaxed);
        let bound = deadline.left(clock.now());
        #[cfg(not(blaze_check))]
        loop {
            // Acquire: the control thread's answer released what it did before (the word's
            // store, for BOUND).
            let state = at.state.load(Ordering::Acquire);
            if state != WAITING {
                return Ok(answered(row, state));
            }
            match wait_shared(WHAT, &at.state, WAITING as u32, deadline, clock) {
                // Woken, or the word moved: look again.
                Ok(Waited::Ready(())) => {}
                Ok(Waited::Deadline) => {
                    // An answer stored as the deadline passed is still the answer.
                    let state = at.state.load(Ordering::Acquire);
                    return match state {
                        WAITING => Err(Overran::Deadline { slot, bound }),
                        state => Ok(answered(row, state)),
                    };
                }
                Err(why) => {
                    let errno = why.io_error().and_then(std::io::Error::raw_os_error);
                    return Err(Overran::Failed { slot, errno });
                }
            }
        }
        #[cfg(blaze_check)]
        loop {
            let state = at.state.load(Ordering::Acquire);
            if state != WAITING {
                return Ok(answered(row, state));
            }
            if deadline.passed(clock.now()) {
                return Err(Overran::Deadline { slot, bound });
            }
            blaze_check::thread::yield_now();
        }
    }

    /// Answers row `row`: stores the answer, then wakes its thread. The control thread's call,
    /// once per pushed row; for [`Answer::Refused`] and [`Answer::Lost`] after it said why
    /// ([`say_no_code`]). Allocates nothing.
    pub fn answer(&self, row: u32, answer: Answer) {
        let at = self.row(row);
        // Release: the waiter's acquire sees what the control thread did before (the slot's
        // word stored, for BOUND).
        if let Err(was) = at.state.compare_exchange(WAITING, answer.state(), Ordering::Release, Ordering::Relaxed) {
            fault(row, "was answered while no thread waited in it", was);
        }
        #[cfg(not(blaze_check))]
        wake_shared(&at.state);
    }

    /// Frees row `row` once its thread read the answer: the stub's last step.
    pub fn release(&self, row: u32) {
        let at = self.row(row);
        match at.state.load(Ordering::Relaxed) {
            BOUND | REFUSED | LOST => at.state.store(FREE, Ordering::Release),
            was => fault(row, "was released before it was answered", was),
        }
    }

    /// Row `row`: one the table has, or the process stops naming it.
    fn row(&self, row: u32) -> &Row {
        match self.rows.get(row as usize) {
            Some(at) => at,
            None => fault(row, "is past the table's end", u64::from(self.most())),
        }
    }
}

impl Drop for Waiters {
    fn drop(&mut self) {
        let count = self.rows.len();
        let at = ptr::NonNull::from(self.rows).cast::<Row>();
        for index in 0..count {
            // SAFETY: the rows and cells `reserve` wrote, each dropped once, here, with no
            // reference to them left (they are lent only through `&self`, and this is `&mut`).
            unsafe {
                ptr::drop_in_place(at.as_ptr().add(index));
                ptr::drop_in_place(self.ring.cells.as_ptr().cast_mut().add(index));
            }
        }
        // SAFETY: the one mapping `reserve` made, of `bytes(count)` bytes at the rows' start;
        // nothing refers into it any more. Its failure leaves the pages mapped, which only
        // holds memory: there is nothing else to undo.
        let _ = unsafe { munmap(at.cast(), Waiters::bytes(count)) };
    }
}

/// The answer a state word holds, once it is not WAITING.
fn answered(row: u32, state: u64) -> Answer {
    match state {
        BOUND => Answer::Bound,
        REFUSED => Answer::Refused,
        LOST => Answer::Lost,
        was => fault(row, "was waited on while no stub held it", was),
    }
}

/// The launcher's own fault in the table's use: stops the process, naming the row.
fn fault(row: u32, what: &str, state: u64) -> ! {
    let mut line = Line::new();
    line.text("blaze: the stub waiters' row ")
        .number(u64::from(row))
        .text(" ")
        .text(what)
        .text(" (state ")
        .number(state)
        .text("): the launcher's own fault (stub_wait.rs)\n");
    line.stop()
}

/// What the control thread says before it answers a slot's rows REFUSED or LOST: the line
/// the process ends with. The control thread is no signal handler: it may lock stderr.
pub fn say_no_code(slot: u32, why: &str) {
    use std::io::Write;

    let mut to = std::io::stderr().lock();
    // The process ends after this whether or not the words could be written.
    drop(writeln!(to, "blaze: the function of slot {slot} was called and has no code: {why}"));
}

/// Ends the process after the control thread said why a slot has no code ([`say_no_code`]):
/// the waiter's end on [`Answer::Refused`] or [`Answer::Lost`]. `_exit`, as a handler may.
pub fn stop_said() -> ! {
    exit_now(STOPPED)
}

/// A line of text built on the stack, for a stop from a stub: no allocation, no lock, no
/// formatting machinery. Text past its end is cut at a marked end ("...\n").
pub(crate) struct Line {
    bytes: [u8; 512],
    len: usize,
}

impl Line {
    pub(crate) const fn new() -> Line {
        Line { bytes: [0; 512], len: 0 }
    }

    pub(crate) fn text(&mut self, text: &str) -> &mut Line {
        for &byte in text.as_bytes() {
            if self.len == self.bytes.len() {
                let end = self.bytes.len() - 4;
                self.bytes[end..].copy_from_slice(b"...\n");
                break;
            }
            self.bytes[self.len] = byte;
            self.len += 1;
        }
        self
    }

    pub(crate) fn number(&mut self, value: u64) -> &mut Line {
        let mut digits = [0u8; 20];
        let mut at = digits.len();
        let mut left = value;
        loop {
            at -= 1;
            digits[at] = b'0' + (left % 10) as u8;
            left /= 10;
            if left == 0 {
                break;
            }
        }
        // The digits are ASCII.
        let text = core::str::from_utf8(&digits[at..]).unwrap_or("?");
        self.text(text)
    }

    /// Writes the line to descriptor 2 and ends the process with status 101.
    pub(crate) fn stop(&self) -> ! {
        let mut done = 0;
        while done < self.len {
            // A failed write is not tried again: the process ends either way.
            match write(Fd::STDERR, &self.bytes[done..self.len]) {
                Ok(0) | Err(_) => break,
                Ok(wrote) => done += wrote,
            }
        }
        exit_now(STOPPED)
    }
}

#[cfg(all(test, not(blaze_check)))]
#[path = "stub_wait_tests.rs"]
mod tests;

#[cfg(all(test, blaze_check))]
#[path = "stub_wait_model.rs"]
mod model;
