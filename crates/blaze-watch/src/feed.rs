//! The event channel, both ends. [`Feed`] is a run's side: it collects what its threads emitted
//! (each lane drained from its own cursor, [`crate::ring`]) into a [`Window`] as [`Item`]s with
//! one dense numbering, and a [`Subscriber`] per connection pulls from the window while the
//! connection has credit. [`Follow`] is the reader's side: it takes items and gaps in order and
//! refuses anything else, so a resume can never deliver an item twice or skip one silently.
//!
//! **Resume.** A reader that lost its connection reconnects, hears the run's epoch in its HELLO
//! and, for the same epoch, sends `OPEN{from: last}` (the last item it took or passed in a gap).
//! The subscriber starts after `from`: the items the window still holds are sent, and those it
//! dropped meanwhile are one GAP frame. For another epoch (a new process) the reader starts
//! again from 0. The window is bounded, so a reader away longer than the window holds sees a
//! gap, and nothing grows.

// adapted from makepad/relax (MIT), libs/remote/src/hub.rs (feeds reopened `from=<last seq>`
// after a link drops, a gap marker where the link was down, nothing else); changed: the run
// serves the resume itself, the reader's order checked (`Follow`), an epoch so numbers of
// another process are not resumed.

#![blaze::sans_io]

use core::alloc::Allocator;
use core::fmt;

use blaze_own::Vec;

use crate::cbor::{self, Reader};
use crate::conn::{Conn, ConnError};
use crate::frame::{self, Frame, LENGTH, VERSION};
use crate::ring::{Emitter, Event, Got, Lane, LaneState};
use crate::window::{Window, WindowError};
use crate::{Plant, Seq};

/// One item of a run's stream.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Item {
    /// Entry `n` of `lane`.
    Event { lane: u32, n: u64, event: Event },
    /// Entries `from..from + count` of `lane` were overwritten before the run's drain read
    /// them.
    Lost { lane: u32, from: u64, count: u64 },
}

const EVENT: u64 = 0;
const LOST: u64 = 1;

impl Item {
    /// `[0, lane, n, kind, flags, detail, at, value, key]` (the key empty when zero) or
    /// `[1, lane, from, count]`.
    pub fn write<A: Allocator>(&self, out: &mut Vec<u8, A>) {
        match *self {
            Item::Event { lane, n, event } => {
                cbor::array(out, 9);
                cbor::uint(out, EVENT);
                cbor::uint(out, u64::from(lane));
                cbor::uint(out, n);
                cbor::uint(out, u64::from(event.kind));
                cbor::uint(out, u64::from(event.flags));
                cbor::uint(out, u64::from(event.detail));
                cbor::uint(out, event.at);
                cbor::uint(out, event.value);
                match event.key == [0; 32] {
                    true => cbor::bytes(out, &[]),
                    false => cbor::bytes(out, &event.key),
                }
            }
            Item::Lost { lane, from, count } => {
                cbor::array(out, 4);
                cbor::uint(out, LOST);
                cbor::uint(out, u64::from(lane));
                cbor::uint(out, from);
                cbor::uint(out, count);
            }
        }
    }

    /// An item from untrusted bytes: each field checked against its type, nothing after it.
    pub fn read(bytes: &[u8]) -> Result<Item, cbor::Error> {
        let mut r = Reader::new(bytes);
        let fields = r.array(9, "an item of more fields than any kind has")?;
        let item = match (r.uint()?, fields) {
            (EVENT, 9) => {
                let lane = r.uint_most(u64::from(u32::MAX), "a lane past 32 bits")? as u32;
                let n = r.uint()?;
                let kind = r.uint_most(u64::from(u16::MAX), "a kind past 16 bits")? as u16;
                let flags = r.uint_most(u64::from(u16::MAX), "flags past 16 bits")? as u16;
                let detail = r.uint_most(u64::from(u32::MAX), "a detail past 32 bits")? as u32;
                let at = r.uint()?;
                let value = r.uint()?;
                let raw = r.bytes(32, "a key longer than 32 bytes")?;
                let mut key = [0u8; 32];
                match raw.len() {
                    0 => {}
                    32 => key.copy_from_slice(raw),
                    _ => return Err(cbor::Error { at: r.at(), why: "a key of neither 0 nor 32 bytes" }),
                }
                Item::Event { lane, n, event: Event { kind, flags, detail, at, value, key } }
            }
            (LOST, 4) => {
                let lane = r.uint_most(u64::from(u32::MAX), "a lane past 32 bits")? as u32;
                Item::Lost { lane, from: r.uint()?, count: r.uint()? }
            }
            _ => return Err(cbor::Error { at: 0, why: "an item of unknown kind or length" }),
        };
        r.end()?;
        Ok(item)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FeedError {
    Window(WindowError),
    Conn(ConnError),
    Drain(crate::ring::DrainError),
    Lane(crate::ring::EmitterError),
    /// A reader resumed past the newest item: numbers of another epoch, or its bug.
    Ahead { from: Seq, newest: Seq },
}

impl fmt::Display for FeedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FeedError::Window(error) => write!(f, "the event feed: {error}"),
            FeedError::Conn(error) => write!(f, "the event feed: {error}"),
            FeedError::Drain(error) => write!(f, "the event feed: {error}"),
            FeedError::Lane(error) => write!(f, "the event feed: {error}"),
            FeedError::Ahead { from, newest } => {
                write!(f, "a reader resumed after item {}, past the newest, {}", from.0, newest.0)
            }
        }
    }
}

impl core::error::Error for FeedError {}

impl From<WindowError> for FeedError {
    fn from(error: WindowError) -> FeedError {
        FeedError::Window(error)
    }
}

impl From<ConnError> for FeedError {
    fn from(error: ConnError) -> FeedError {
        FeedError::Conn(error)
    }
}

/// A run's event stream: the window its subscribers read, and where each lane was drained to.
pub struct Feed<A: Allocator> {
    window: Window<A>,
    cursors: Vec<u64, A>,
    scratch: Vec<u8, A>,
}

impl<A: Allocator + Clone> Feed<A> {
    pub fn new_in(lanes: usize, bytes: usize, items: usize, alloc: A) -> Result<Feed<A>, FeedError> {
        let mut cursors = Vec::with_capacity_in(lanes, alloc.clone());
        cursors.resize(lanes, 0);
        Ok(Feed {
            window: Window::new_in(bytes, items, alloc.clone())?,
            cursors,
            scratch: Vec::with_capacity_in(128, alloc),
        })
    }
}

impl<A: Allocator> Feed<A> {
    pub fn window(&self) -> &Window<A> {
        &self.window
    }

    /// Pushes one item; returns its number.
    pub fn push(&mut self, item: &Item) -> Result<Seq, FeedError> {
        self.scratch.clear();
        item.write(&mut self.scratch);
        Ok(self.window.push(&self.scratch)?)
    }

    /// Drains every lane of `emitter` from where it was drained to, in lane order, and frees
    /// each lane whose thread had ended before the drain began (so the drain saw all it wrote).
    pub fn collect<B: Allocator>(&mut self, emitter: &Emitter<B>) -> Result<(), FeedError> {
        for index in 0..emitter.lanes().min(self.cursors.len()) {
            let lane = Lane(index as u32);
            let state = emitter.state(lane);
            if state == LaneState::Free && emitter.head(lane) == self.cursors[index] {
                continue;
            }
            let mut failed = None;
            let from = self.cursors[index];
            let next = emitter
                .drain(lane, from, |got| {
                    if failed.is_some() {
                        return;
                    }
                    let item = match got {
                        Got::Event { n, event } => Item::Event { lane: lane.0, n, event },
                        Got::Lost { from, count } => Item::Lost { lane: lane.0, from, count },
                    };
                    if let Err(error) = self.push(&item) {
                        failed = Some(error);
                    }
                })
                .map_err(FeedError::Drain)?;
            if let Some(error) = failed {
                return Err(error);
            }
            self.cursors[index] = next;
            if state == LaneState::Ended {
                emitter.free(lane).map_err(FeedError::Lane)?;
            }
        }
        Ok(())
    }
}

/// One connection's reading of a window: the last item it was sent or passed in a gap.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Subscriber {
    pub chan: u16,
    sent: Seq,
}

impl Subscriber {
    /// A reader that has seen everything up to `from`.
    pub fn open<A: Allocator>(chan: u16, from: Seq, window: &Window<A>) -> Result<Subscriber, FeedError> {
        Subscriber::open_planted(chan, from, window, Plant::None)
    }

    pub(crate) fn open_planted<A: Allocator>(
        chan: u16,
        from: Seq,
        window: &Window<A>,
        plant: Plant,
    ) -> Result<Subscriber, FeedError> {
        if from > window.newest() {
            return Err(FeedError::Ahead { from, newest: window.newest() });
        }
        let sent = match plant {
            Plant::ResumeInclusive if from > Seq::NONE => Seq(from.0 - 1),
            _ => from,
        };
        Ok(Subscriber { chan, sent })
    }

    pub fn sent(&self) -> Seq {
        self.sent
    }

    /// Writes the frames the window has for this reader, while `conn` has credit and `out`
    /// holds less than `most` bytes: a GAP for what the window dropped first, then DATA in
    /// order. Returns whether it caught up.
    pub fn pump<A: Allocator, C: Allocator, B: Allocator>(
        &mut self,
        window: &Window<A>,
        conn: &mut Conn<C>,
        out: &mut Vec<u8, B>,
        most: usize,
    ) -> Result<bool, FeedError> {
        loop {
            let want = self.sent.next();
            if want >= window.next() {
                return Ok(true);
            }
            if out.len() >= most {
                return Ok(false);
            }
            if want < window.first() {
                let to = Seq(window.first().0 - 1);
                Frame::Gap { chan: self.chan, from: want, to }.write(out);
                self.sent = to;
                continue;
            }
            let (a, b) = window.get(want)?;
            let len = a.len() + b.len();
            let frame_len = Frame::data_len(self.chan, want, len);
            if !conn.credit(self.chan).fits(frame_len) {
                return Ok(false);
            }
            write_data(out, self.chan, want, a, b);
            conn.spend(self.chan, frame_len)?;
            self.sent = want;
        }
    }
}

/// A DATA frame whose bytes are in two parts, as a window holds them.
pub fn write_data<B: Allocator>(out: &mut Vec<u8, B>, chan: u16, seq: Seq, a: &[u8], b: &[u8]) {
    let start = out.len();
    out.extend_from_slice(&[0; LENGTH]);
    cbor::array(out, 5);
    cbor::uint(out, VERSION);
    cbor::uint(out, u64::from(chan));
    cbor::uint(out, frame::kind::DATA);
    cbor::uint(out, seq.0);
    cbor::bytes_head(out, a.len() + b.len());
    out.extend_from_slice(a);
    out.extend_from_slice(b);
    let len = (out.len() - start - LENGTH) as u32;
    out[start..start + LENGTH].copy_from_slice(&len.to_le_bytes());
}

/// Why a reader refused what came.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FollowError {
    /// An item at or before the last one taken: a duplicate.
    Replayed { seq: Seq, last: Seq },
    /// An item after a hole no gap announced.
    Skipped { seq: Seq, last: Seq },
    /// A gap that does not start right after the last item taken.
    BadGap { from: Seq, to: Seq, last: Seq },
}

impl fmt::Display for FollowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            FollowError::Replayed { seq, last } => {
                write!(f, "item {} came again after item {} was taken", seq.0, last.0)
            }
            FollowError::Skipped { seq, last } => {
                write!(f, "item {} came after item {} with no gap between", seq.0, last.0)
            }
            FollowError::BadGap { from, to, last } => write!(
                f,
                "a gap of items {} to {} came after item {}",
                from.0, to.0, last.0
            ),
        }
    }
}

impl core::error::Error for FollowError {}

/// A reader's place in one stream: the last item it took or passed in a gap, and the epoch
/// the numbers are of.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Follow {
    last: Seq,
    epoch: Option<u64>,
}

impl Follow {
    pub fn last(&self) -> Seq {
        self.last
    }

    /// The peer said its epoch: the `from` to open with (0 for an epoch not seen before).
    pub fn hello(&mut self, epoch: u64) -> Seq {
        if self.epoch != Some(epoch) {
            self.epoch = Some(epoch);
            self.last = Seq::NONE;
        }
        self.last
    }

    pub fn data(&mut self, seq: Seq) -> Result<(), FollowError> {
        if seq <= self.last {
            return Err(FollowError::Replayed { seq, last: self.last });
        }
        if seq != self.last.next() {
            return Err(FollowError::Skipped { seq, last: self.last });
        }
        self.last = seq;
        Ok(())
    }

    pub fn gap(&mut self, from: Seq, to: Seq) -> Result<(), FollowError> {
        if from != self.last.next() || to < from {
            return Err(FollowError::BadGap { from, to, last: self.last });
        }
        self.last = to;
        Ok(())
    }
}
