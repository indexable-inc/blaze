//! Both ends of a connection as machines: numbered channels over one byte stream, each with
//! credit flow control, and a writer that sends in priority order.
//!
//! **Credit.** A sender may have at most [`INITIAL_CREDIT`] bytes of a channel's data frames
//! unanswered; the receiver grants them back as its consumer takes them, in grants of at least
//! [`GRANT_AT`]. A grant that would raise the credit past the initial amount is refused (a peer
//! cannot make a sender hold more than the window), and data past the credit is refused (a peer
//! cannot make a receiver hold more). So each side's buffer for a channel is bounded by the
//! window, whatever the other side does.
//!
//! **Priority.** Control frames (hello, credit, pong, close) are never held by credit and go
//! first; then channels in the order of their priority (lower first), each sending only while
//! it has credit. The data itself is not queued here: a channel's source (a [`crate::feed::Feed`]
//! or the viewer's pages) is pulled when the writer has room, so nothing grows while a peer is
//! slow; the source keeps a cursor and turns what it could not send in time into a gap.
//!
//! No app thread is involved: app threads write only to their rings ([`crate::ring`]); this runs
//! on the process's drain thread.

// adapted from makepad/relax (MIT), libs/remote/src/conn.rs (credit per channel, INITIAL_CREDIT,
// GRANT_AT, a writer that drains channels in priority order, sending data only while it has
// credit); changed: no queue per channel (sources are pulled), grants and spends checked, both
// ends sans-IO.

#![blaze::sans_io]

use core::alloc::Allocator;
use core::fmt;

use blaze_own::{Vec, VecDeque};

use crate::frame::{Code, Frame, MOST_CHANNELS, Role, Service};
use crate::{Nanos, Seq};

/// The bytes of data a sender may have unanswered on a channel.
pub const INITIAL_CREDIT: u32 = 256 << 10;
/// A receiver grants once its consumer has taken this much.
pub const GRANT_AT: u32 = 64 << 10;
/// Control frames queued at once; past it the peer is flooding (pings) and is refused.
pub const MOST_CONTROL: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConnError {
    /// A grant that would raise a channel's credit past the window.
    Overgranted { chan: u16, credit: u32, grant: u32 },
    /// A send past the channel's credit: the writer's own bug.
    Overspent { chan: u16, credit: u32, len: usize },
    /// Data past the credit the receiver gave.
    Overrun { chan: u16, held: u32, len: usize },
    ControlFull,
    /// A channel not opened.
    NotOpen { chan: u16 },
}

impl fmt::Display for ConnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ConnError::Overgranted { chan, credit, grant } => write!(
                f,
                "channel {chan} was granted {grant} bytes on top of {credit}, past its window of {INITIAL_CREDIT}"
            ),
            ConnError::Overspent { chan, credit, len } => {
                write!(f, "a send of {len} bytes on channel {chan}, which has credit for {credit}")
            }
            ConnError::Overrun { chan, held, len } => write!(
                f,
                "the peer sent {len} bytes on channel {chan} with {held} of {INITIAL_CREDIT} already unanswered"
            ),
            ConnError::ControlFull => {
                write!(f, "the peer asked for more than {MOST_CONTROL} control answers at once")
            }
            ConnError::NotOpen { chan } => write!(f, "data on channel {chan}, which is not open"),
        }
    }
}

impl core::error::Error for ConnError {}

/// The sender's view of one channel: the bytes it may still send.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Credit {
    left: u32,
}

impl Credit {
    pub const fn new() -> Credit {
        Credit { left: INITIAL_CREDIT }
    }

    pub const fn left(self) -> u32 {
        self.left
    }

    pub const fn fits(self, len: usize) -> bool {
        len <= self.left as usize
    }

    pub fn spend(&mut self, chan: u16, len: usize) -> Result<(), ConnError> {
        if !self.fits(len) {
            return Err(ConnError::Overspent { chan, credit: self.left, len });
        }
        self.left -= len as u32;
        Ok(())
    }

    pub fn grant(&mut self, chan: u16, bytes: u32) -> Result<(), ConnError> {
        match self.left.checked_add(bytes) {
            Some(left) if left <= INITIAL_CREDIT => {
                self.left = left;
                Ok(())
            }
            _ => Err(ConnError::Overgranted { chan, credit: self.left, grant: bytes }),
        }
    }
}

/// The receiver's view of one channel: what it holds unanswered, and what its consumer took
/// since its last grant.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Grant {
    held: u32,
    taken: u32,
}

impl Grant {
    pub const fn held(self) -> u32 {
        self.held
    }

    /// A data frame of `len` bytes arrived.
    pub fn received(&mut self, chan: u16, len: usize) -> Result<(), ConnError> {
        if self.held as usize + len > INITIAL_CREDIT as usize {
            return Err(ConnError::Overrun { chan, held: self.held, len });
        }
        self.held += len as u32;
        Ok(())
    }

    /// The consumer took `len` bytes; the grant to send, once [`GRANT_AT`] was taken. No
    /// stall: a sender out of credit has more than `INITIAL_CREDIT - MOST_FRAME` held here,
    /// past `GRANT_AT`, so a consumer that took it all has granted.
    pub fn consumed(&mut self, len: usize) -> Option<u32> {
        let len = (len as u32).min(self.held - self.taken);
        self.taken += len;
        if self.taken < GRANT_AT {
            return None;
        }
        let grant = self.taken;
        self.held -= grant;
        self.taken = 0;
        Some(grant)
    }
}

/// A control frame waiting to be written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Control {
    Hello { role: Role, epoch: u64 },
    Open { chan: u16, service: Service, from: Seq },
    Credit { chan: u16, bytes: u32 },
    Close { chan: u16, code: Code },
    Ping { at: Nanos },
    Pong { at: Nanos },
}

impl Control {
    fn frame(self) -> Frame<'static> {
        match self {
            Control::Hello { role, epoch } => Frame::Hello { role, epoch },
            Control::Open { chan, service, from } => Frame::Open { chan, service, from },
            Control::Credit { chan, bytes } => Frame::Credit { chan, bytes },
            Control::Close { chan, code } => Frame::Close { chan, code },
            Control::Ping { at } => Frame::Ping { at },
            Control::Pong { at } => Frame::Pong { at },
        }
    }
}

/// What a received frame means for the owner, once the connection has handled what is its own
/// (credit, pings).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Incoming<'a> {
    /// Handled here; nothing for the owner.
    Nothing,
    Hello { role: Role, epoch: u64 },
    Open { chan: u16, service: Service, from: Seq },
    Data { chan: u16, seq: Seq, bytes: &'a [u8] },
    Gap { chan: u16, from: Seq, to: Seq },
    Close { chan: u16, code: Code },
    Request { bytes: &'a [u8] },
    Pong { at: Nanos },
}

/// One end of a connection: the credit of each channel it sends on, the grants of each it
/// receives on, the control frames it owes, and the order its channels are written in.
pub struct Conn<A: Allocator> {
    control: VecDeque<Control, A>,
    credit: [Credit; MOST_CHANNELS as usize],
    grant: [Grant; MOST_CHANNELS as usize],
    open: [bool; MOST_CHANNELS as usize],
    priority: [u8; MOST_CHANNELS as usize],
}

impl<A: Allocator> Conn<A> {
    pub fn new_in(alloc: A) -> Conn<A> {
        Conn {
            control: VecDeque::with_capacity_in(MOST_CONTROL, alloc),
            credit: [Credit::new(); MOST_CHANNELS as usize],
            grant: [Grant::default(); MOST_CHANNELS as usize],
            open: [false; MOST_CHANNELS as usize],
            priority: [u8::MAX; MOST_CHANNELS as usize],
        }
    }

    /// Opens `chan` at `priority` (lower is written first), with a fresh window both ways.
    pub fn open(&mut self, chan: u16, priority: u8) {
        let at = chan as usize;
        self.open[at] = true;
        self.priority[at] = priority;
        self.credit[at] = Credit::new();
        self.grant[at] = Grant::default();
    }

    pub fn is_open(&self, chan: u16) -> bool {
        self.open[chan as usize]
    }

    pub fn close(&mut self, chan: u16) {
        self.open[chan as usize] = false;
    }

    pub fn queue(&mut self, control: Control) -> Result<(), ConnError> {
        if self.control.len() == MOST_CONTROL {
            return Err(ConnError::ControlFull);
        }
        self.control.push_back(control);
        Ok(())
    }

    /// Writes every control frame owed: never held by credit.
    pub fn write_control<B: Allocator>(&mut self, out: &mut Vec<u8, B>) {
        while let Some(control) = self.control.pop_front() {
            control.frame().write(out);
        }
    }

    /// The open channels, in the order their data is written.
    pub fn order(&self) -> ([u16; MOST_CHANNELS as usize], usize) {
        let mut order = [0u16; MOST_CHANNELS as usize];
        let mut len = 0;
        for chan in 0..MOST_CHANNELS {
            if self.open[chan as usize] {
                order[len] = chan;
                len += 1;
            }
        }
        // Keys are distinct (the channel breaks ties), so the unstable sort is deterministic.
        order[..len].sort_unstable_by_key(|chan| (self.priority[*chan as usize], *chan));
        (order, len)
    }

    pub fn credit(&self, chan: u16) -> Credit {
        self.credit[chan as usize]
    }

    /// A data frame of `len` bytes is written on `chan`.
    pub fn spend(&mut self, chan: u16, len: usize) -> Result<(), ConnError> {
        self.credit[chan as usize].spend(chan, len)
    }

    /// The consumer took `len` bytes of `chan`'s data: queues the grant when one is due.
    pub fn consumed(&mut self, chan: u16, len: usize) -> Result<(), ConnError> {
        if let Some(bytes) = self.grant[chan as usize].consumed(len) {
            self.queue(Control::Credit { chan, bytes })?;
        }
        Ok(())
    }

    /// A frame from the peer.
    pub fn received<'a>(&mut self, frame: Frame<'a>) -> Result<Incoming<'a>, ConnError> {
        Ok(match frame {
            Frame::Hello { role, epoch } => Incoming::Hello { role, epoch },
            Frame::Open { chan, service, from } => Incoming::Open { chan, service, from },
            Frame::Data { chan, seq, bytes } => {
                if !self.is_open(chan) {
                    return Err(ConnError::NotOpen { chan });
                }
                self.grant[chan as usize].received(chan, Frame::data_len(chan, seq, bytes.len()))?;
                Incoming::Data { chan, seq, bytes }
            }
            Frame::Gap { chan, from, to } => Incoming::Gap { chan, from, to },
            Frame::Credit { chan, bytes } => {
                self.credit[chan as usize].grant(chan, bytes)?;
                Incoming::Nothing
            }
            Frame::Close { chan, code } => {
                self.close(chan);
                Incoming::Close { chan, code }
            }
            Frame::Ping { at } => {
                self.queue(Control::Pong { at })?;
                Incoming::Nothing
            }
            Frame::Pong { at } => Incoming::Pong { at },
            Frame::Request { bytes } => Incoming::Request { bytes },
        })
    }
}
