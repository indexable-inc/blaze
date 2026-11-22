//! [`FirstCall`]: a function's first call in the program, in the program's control thread
//! (run.md, "End state", "First call"; bottom.md section 11: "A function not yet compiled is a
//! stub: its first call asks the run's parent for the code record (found by key, or made then),
//! which is published in the slot").
//!
//! The child numbers its slots itself, as symbols first appear: in the run record it was started
//! with, in an offer from the parent, or among the targets of a record it places
//! ([`FirstCall::slot_of`]). A run has no list of functions made before it starts. Each slot is
//! one of:
//!
//! - **Stub**: its word names its stub; nothing is known of its code.
//! - **Known**: its word names its stub; its record is known (a row of the run record, or an
//!   offer the parent sent unasked), so a call places it here with no round trip.
//! - **Asked**: a thread called it while nothing was known; the parent was asked once.
//! - **Publishing**: its record is being placed and published ([`Do::Publish`], carried by
//!   `slots::Slots::publish`, the one writer of words).
//! - **Bound**: its word names its body.
//! - **Failed**: it has no code; every caller is told why (rule 20: that call fails, nothing else).
//!
//! What it promises, which the simulation (`sim/first_call.rs`) checks with a planted bug each:
//! however many threads call a slot before it is bound, the parent is asked at most once and the
//! record is published at most once; every waiting thread is woken exactly once, and woken to go
//! again ([`Woken::Again`]) only after the publish has been carried, so the word it loads again
//! names the body; a thread that fires a stub whose slot is already bound (it loaded the word
//! before the store) is sent back at once. An ask has a deadline on the driver's clock
//! ([`Config::ask_bound`], rule 18): past it the waiters fail with [`Why::Overran`], and a late
//! answer still publishes the record for later callers.
//!
//! Sans-IO: time is an event's field, every table is in the allocator its maker passes, and the
//! effects go, in the order the driver must carry them, into the caller's buffer.

#![blaze::sans_io]

use core::fmt;

use blaze_hash::{FixedState, Map, MapOpts};
use blaze_own::{AVec, Alloc, Vec};

use crate::{CodeRef, Nanos, SlotId, ThreadId, Why};

/// A function instance as a code record names its target: the item's stable id and its type
/// arguments by content. Becomes `blaze_code::record::Symbol`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Symbol(pub [u8; 32]);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    /// The most slots the run's reservation holds words and stubs for.
    pub most_slots: u32,
    /// The most threads waiting in stubs at once.
    pub most_waiters: u32,
    /// How long an ask may go unanswered on the driver's clock before its waiters fail.
    pub ask_bound: Nanos,
}

/// Where a slot stands. See the module's documentation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Stub,
    Known(CodeRef),
    Asked { since: Nanos },
    Publishing(CodeRef),
    Bound(CodeRef),
    Failed(Why),
}

/// What a woken thread does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Woken {
    /// Load the slot's word again and jump through it: it names the body now.
    Again,
    /// The call fails on this thread with this reason (a panic carrying it); the program goes on.
    Fail(Why),
}

/// One thing for the driver to do, in the order given.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Do {
    /// A slot is new: its words are made to name its stub (`Slots` grown to cover it) before any
    /// placed code can name it.
    Stub { slot: SlotId },
    /// Ask the parent for the code record of `symbol` (the wire's `Need`); answered by
    /// [`FirstCall::answered`].
    Ask { slot: SlotId, symbol: Symbol },
    /// Place `code` and publish it in `slot` (`Slots::publish` at the fast tier, its writes
    /// carried in order: the copy, the cache maintenance, the unwind entry, then the word's
    /// release store); then call [`FirstCall::published`].
    Publish { slot: SlotId, code: CodeRef },
    /// Wake `thread`, which waits in `slot`'s stub.
    Wake { thread: ThreadId, slot: SlotId, woken: Woken },
}

/// Why a call was refused. Each names the slot; none changes the machine's state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// A slot this machine never numbered.
    NoSlot { slot: SlotId, slots: u32 },
    /// More symbols than the reservation holds slots for.
    Full { most: u32 },
    /// More threads waiting in stubs than the bound.
    Waiters { most: u32 },
    /// The parent answered a slot nobody asked about.
    Unasked { slot: SlotId, state: State },
    /// A publish reported for a slot that was not publishing.
    NotPublishing { slot: SlotId, state: State },
    /// Another record offered or answered for a slot whose first record is placed or being
    /// placed: a later tier goes through `Slots::publish`, not through a first call.
    NotFirst { slot: SlotId, first: CodeRef, other: CodeRef },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Error::NoSlot { slot, slots } => write!(f, "slot {} is not one of the {slots} this run numbered", slot.0),
            Error::Full { most } => {
                write!(f, "the program names more than {most} functions, the slots its reservation holds (run.md, \"Slot table\")")
            }
            Error::Waiters { most } => write!(f, "more than {most} threads wait in stubs at once (rule 18)"),
            Error::Unasked { slot, state } => write!(
                f,
                "the parent answered slot {}, which was not asked (it is {}): the wire is out of step",
                slot.0,
                state.name()
            ),
            Error::NotPublishing { slot, state } => {
                write!(f, "slot {} was reported published while {}: the driver is out of step", slot.0, state.name())
            }
            Error::NotFirst { slot, first, other } => {
                write!(f, "slot {} was given record ", slot.0)?;
                prefix(f, &other)?;
                f.write_str(" while its first record ")?;
                prefix(f, &first)?;
                f.write_str(" is placed: a later tier is published through `Slots`")
            }
        }
    }
}

/// A record key's first four bytes in hex: how an error names one.
fn prefix(f: &mut fmt::Formatter<'_>, code: &CodeRef) -> fmt::Result {
    for byte in &code.0[..4] {
        write!(f, "{byte:02x}")?;
    }
    Ok(())
}

impl State {
    /// The state's name in an error.
    pub fn name(self) -> &'static str {
        match self {
            State::Stub => "a stub",
            State::Known(_) => "known",
            State::Asked { .. } => "asked",
            State::Publishing(_) => "publishing",
            State::Bound(_) => "bound",
            State::Failed(_) => "failed",
        }
    }
}

impl core::error::Error for Error {}

/// A safeguard taken out, for the simulation's planted-bug controls (rule 43).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(not(test), allow(dead_code, reason = "only the tests plant a bug"))]
pub(crate) enum Slip {
    None,
    /// A second thread firing an asked slot asks the parent again.
    AskTwice,
    /// The waiters are woken when the publish is asked for, before it is carried.
    WakeBeforeStore,
}

#[derive(Clone, Copy, Debug)]
struct Row {
    symbol: Symbol,
    state: State,
}

/// The first calls of one program. See the module's documentation.
pub struct FirstCall<'a> {
    config: Config,
    rows: AVec<'a, Row>,
    by_symbol: Map<Symbol, SlotId, FixedState, Alloc<'a>>,
    /// Threads waiting in a stub, each with its slot; at most `config.most_waiters`.
    waiters: AVec<'a, (SlotId, ThreadId)>,
    pub(crate) slip: Slip,
}

impl<'a> FirstCall<'a> {
    pub fn new(config: Config, alloc: Alloc<'a>) -> FirstCall<'a> {
        FirstCall {
            config,
            rows: Vec::new_in(alloc),
            by_symbol: Map::new(MapOpts { hasher: FixedState, alloc: alloc, capacity: None }),
            waiters: Vec::new_in(alloc),
            slip: Slip::None,
        }
    }

    /// The slots numbered so far.
    pub fn slots(&self) -> u32 {
        self.rows.len() as u32
    }

    pub fn state(&self, slot: SlotId) -> Result<State, Error> {
        Ok(self.row(slot)?.state)
    }

    /// The threads waiting in stubs now.
    pub fn waiting(&self) -> usize {
        self.waiters.len()
    }

    fn row(&self, slot: SlotId) -> Result<&Row, Error> {
        let slots = self.slots();
        self.rows.get(slot.index()).ok_or(Error::NoSlot { slot, slots })
    }

    fn row_mut(&mut self, slot: SlotId) -> Result<&mut Row, Error> {
        let slots = self.slots();
        self.rows.get_mut(slot.index()).ok_or(Error::NoSlot { slot, slots })
    }

    /// The slot of `symbol`; a symbol seen for the first time is numbered next, and its words
    /// are made to name its stub ([`Do::Stub`]) before anything that names it is published.
    pub fn slot_of(&mut self, symbol: Symbol, out: &mut AVec<'_, Do>) -> Result<SlotId, Error> {
        if let Some(&slot) = self.by_symbol.get(&symbol) {
            return Ok(slot);
        }
        if self.slots() >= self.config.most_slots {
            return Err(Error::Full { most: self.config.most_slots });
        }
        let slot = SlotId(self.slots());
        self.rows.push(Row { symbol, state: State::Stub });
        self.by_symbol.insert(symbol, slot);
        out.push(Do::Stub { slot });
        Ok(slot)
    }

    /// `symbol`'s record is `code`: a row of the run record the run started with, or an offer the
    /// parent sent unasked (a callee it compiled behind a publish). Placed at once when a thread
    /// already waits for it; else kept for the first call. Speculation places offers at once:
    /// `place_now` (an offer compiled behind a call, likely to run next) publishes it now.
    pub fn know(&mut self, symbol: Symbol, code: CodeRef, place_now: bool, out: &mut AVec<'_, Do>) -> Result<SlotId, Error> {
        let slot = self.slot_of(symbol, out)?;
        let state = self.row(slot)?.state;
        match state {
            State::Stub | State::Known(_) if place_now => self.publish(slot, code, out)?,
            State::Stub | State::Known(_) => self.row_mut(slot)?.state = State::Known(code),
            // The offer crossed the ask on the wire: it is the answer.
            State::Asked { .. } => self.publish(slot, code, out)?,
            State::Publishing(first) | State::Bound(first) if first == code => {}
            State::Publishing(first) | State::Bound(first) => {
                return Err(Error::NotFirst { slot, first, other: code });
            }
            // A late record after an overrun serves later callers; another failure stands.
            State::Failed(Why::Overran { .. }) => self.publish(slot, code, out)?,
            State::Failed(_) => {}
        }
        Ok(slot)
    }

    /// `thread` called `slot` and found its stub. It waits until a [`Do::Wake`] names it.
    pub fn fired(&mut self, thread: ThreadId, slot: SlotId, now: Nanos, out: &mut AVec<'_, Do>) -> Result<(), Error> {
        let row = *self.row(slot)?;
        match row.state {
            // It loaded the word before the store: the word names the body now.
            State::Bound(_) => {
                out.push(Do::Wake { thread, slot, woken: Woken::Again });
                return Ok(());
            }
            State::Failed(why) => {
                out.push(Do::Wake { thread, slot, woken: Woken::Fail(why) });
                return Ok(());
            }
            State::Stub | State::Known(_) | State::Asked { .. } | State::Publishing(_) => {}
        }
        if self.waiters.len() as u64 >= u64::from(self.config.most_waiters) {
            return Err(Error::Waiters { most: self.config.most_waiters });
        }
        self.waiters.push((slot, thread));
        match row.state {
            State::Stub => {
                self.row_mut(slot)?.state = State::Asked { since: now };
                out.push(Do::Ask { slot, symbol: row.symbol });
            }
            State::Known(code) => self.publish(slot, code, out)?,
            State::Asked { .. } if self.slip == Slip::AskTwice => out.push(Do::Ask { slot, symbol: row.symbol }),
            // Single flight: the ask or the publish under way answers this thread too.
            State::Asked { .. } | State::Publishing(_) => {}
            State::Bound(_) | State::Failed(_) => unreachable!("answered above"),
        }
        Ok(())
    }

    /// The parent's answer to [`Do::Ask`] for `slot`: the record (found by key, or made then), or
    /// why the function has no code.
    pub fn answered(&mut self, slot: SlotId, answer: Result<CodeRef, Why>, out: &mut AVec<'_, Do>) -> Result<(), Error> {
        let state = self.row(slot)?.state;
        match (state, answer) {
            (State::Asked { .. }, Ok(code)) => self.publish(slot, code, out),
            (State::Asked { .. }, Err(why)) => {
                self.row_mut(slot)?.state = State::Failed(why);
                self.wake(slot, Woken::Fail(why), out);
                Ok(())
            }
            // An offer arrived before the answer it crossed: the same record is nothing new.
            (State::Publishing(first) | State::Bound(first), Ok(code)) if first == code => Ok(()),
            (State::Publishing(first) | State::Bound(first), Ok(code)) => {
                Err(Error::NotFirst { slot, first, other: code })
            }
            (State::Publishing(_) | State::Bound(_), Err(_)) => Ok(()),
            (State::Failed(Why::Overran { .. }), Ok(code)) => self.publish(slot, code, out),
            (State::Failed(Why::Overran { .. }), Err(why)) => {
                self.row_mut(slot)?.state = State::Failed(why);
                Ok(())
            }
            (State::Stub | State::Known(_) | State::Failed(_), _) => Err(Error::Unasked { slot, state }),
        }
    }

    /// The driver carried [`Do::Publish`] for `slot`: the word names the body. Every thread
    /// waiting in its stub goes again.
    pub fn published(&mut self, slot: SlotId, out: &mut AVec<'_, Do>) -> Result<(), Error> {
        let state = self.row(slot)?.state;
        let State::Publishing(code) = state else {
            return Err(Error::NotPublishing { slot, state });
        };
        self.row_mut(slot)?.state = State::Bound(code);
        if self.slip != Slip::WakeBeforeStore {
            self.wake(slot, Woken::Again, out);
        }
        Ok(())
    }

    /// The driver's clock reads `now`: each ask unanswered past [`Config::ask_bound`] fails its
    /// waiters with [`Why::Overran`].
    pub fn tick(&mut self, now: Nanos, out: &mut AVec<'_, Do>) {
        let bound = self.config.ask_bound;
        for index in 0..self.rows.len() {
            let State::Asked { since } = self.rows[index].state else { continue };
            if since.after(bound) > now {
                continue;
            }
            let why = Why::Overran { bound };
            self.rows[index].state = State::Failed(why);
            self.wake(SlotId(index as u32), Woken::Fail(why), out);
        }
    }

    fn publish(&mut self, slot: SlotId, code: CodeRef, out: &mut AVec<'_, Do>) -> Result<(), Error> {
        self.row_mut(slot)?.state = State::Publishing(code);
        out.push(Do::Publish { slot, code });
        if self.slip == Slip::WakeBeforeStore {
            self.wake(slot, Woken::Again, out);
        }
        Ok(())
    }

    /// Wakes every thread waiting in `slot`'s stub, in the order they came.
    fn wake(&mut self, slot: SlotId, woken: Woken, out: &mut AVec<'_, Do>) {
        for &(at, thread) in self.waiters.iter() {
            if at == slot {
                out.push(Do::Wake { thread, slot, woken });
            }
        }
        self.waiters.retain(|&(at, _)| at != slot);
    }
}
