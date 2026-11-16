//! [`Uplink`]: the viewer's optional stream to the ix.dev relay, for an organisation that opted
//! in (watch.md, "The relay"). The viewer dials out (the relay never reaches the machine),
//! says HELLO with its epoch, and the relay opens the events channel from the last item it
//! holds; from then the uplink sends the merged window as DATA while the relay's credit lasts,
//! and a GAP for what the window dropped while the relay was slow or away.
//!
//! **What it carries**: the window's page items, the same activity a paired page sees, with
//! every item's key zeroed unless the organisation chose to carry item names (`carry_keys`).
//! **Never**: pairing keys or codes, the history, argument values (no event holds them), and
//! no action: a REQUEST from the relay is refused and closes the uplink, so nothing on the
//! relay's side can toggle a hook.

#![blaze::sans_io]

use core::alloc::Allocator;
use core::fmt;

use blaze_own::Vec;

use crate::conn::{Conn, ConnError, Control, Incoming};
use crate::feed::{Item, write_data};
use crate::frame::{self, Code, Frame, Role, Service};
use crate::viewer::{MOST_ITEM, PageItem};
use crate::window::{Window, WindowError};
use crate::Seq;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UplinkError {
    Conn(ConnError),
    Window(WindowError),
    /// The relay sent what a relay never sends: a request, data, or a hello of another role.
    Unexpected { kind: &'static str },
    /// The relay opened past the newest item, numbers of another epoch.
    Ahead { from: Seq, newest: Seq },
    /// A window item that does not decode: the viewer's own bug.
    Item,
}

impl fmt::Display for UplinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            UplinkError::Conn(error) => write!(f, "the relay uplink: {error}"),
            UplinkError::Window(error) => write!(f, "the relay uplink: {error}"),
            UplinkError::Unexpected { kind } => write!(f, "the relay sent the viewer a {kind}, which it may not"),
            UplinkError::Ahead { from, newest } => {
                write!(f, "the relay resumed after item {}, past the newest, {}", from.0, newest.0)
            }
            UplinkError::Item => write!(f, "the relay uplink found an item of the viewer's window it cannot read"),
        }
    }
}

impl core::error::Error for UplinkError {}

impl From<ConnError> for UplinkError {
    fn from(error: ConnError) -> UplinkError {
        UplinkError::Conn(error)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UplinkState {
    /// HELLO sent; the relay has not opened.
    Greeting,
    /// Streaming: sent up to `sent`.
    Open { sent: Seq },
    /// Refused or broken: the driver closes the socket.
    Closed,
}

pub struct Uplink<A: Allocator> {
    conn: Conn<A>,
    state: UplinkState,
    carry_keys: bool,
    scratch: Vec<u8, A>,
}

impl<A: Allocator + Clone> Uplink<A> {
    /// A new uplink; its HELLO is in `out`.
    pub fn new_in<B: Allocator>(epoch: u64, carry_keys: bool, out: &mut Vec<u8, B>, alloc: A) -> Uplink<A> {
        Frame::Hello { role: Role::Viewer, epoch }.write(out);
        Uplink {
            conn: Conn::new_in(alloc.clone()),
            state: UplinkState::Greeting,
            carry_keys,
            scratch: Vec::with_capacity_in(MOST_ITEM, alloc),
        }
    }
}

impl<A: Allocator> Uplink<A> {
    pub fn state(&self) -> UplinkState {
        self.state
    }

    fn refuse<B: Allocator>(&mut self, error: UplinkError, out: &mut Vec<u8, B>) -> Result<(), UplinkError> {
        Frame::Close { chan: frame::EVENTS, code: Code::Refused }.write(out);
        self.state = UplinkState::Closed;
        Err(error)
    }

    /// A frame from the relay.
    pub fn received<W: Allocator, B: Allocator>(
        &mut self,
        frame: Frame<'_>,
        window: &Window<W>,
        out: &mut Vec<u8, B>,
    ) -> Result<(), UplinkError> {
        if self.state == UplinkState::Closed {
            return Ok(());
        }
        let incoming = match self.conn.received(frame) {
            Ok(incoming) => incoming,
            Err(error) => return self.refuse(UplinkError::Conn(error), out),
        };
        match incoming {
            Incoming::Nothing | Incoming::Pong { .. } => {}
            Incoming::Hello { role: Role::Relay, .. } => {}
            Incoming::Open { chan: frame::EVENTS, service: Service::Events, from } => {
                if from > window.newest() {
                    return self.refuse(UplinkError::Ahead { from, newest: window.newest() }, out);
                }
                self.conn.open(frame::EVENTS, 0);
                self.state = UplinkState::Open { sent: from };
            }
            Incoming::Close { .. } => self.state = UplinkState::Closed,
            Incoming::Hello { .. } => return self.refuse(UplinkError::Unexpected { kind: "hello of another role" }, out),
            Incoming::Request { .. } => return self.refuse(UplinkError::Unexpected { kind: "request" }, out),
            Incoming::Open { .. } => return self.refuse(UplinkError::Unexpected { kind: "open of another channel" }, out),
            Incoming::Data { .. } | Incoming::Gap { .. } => {
                return self.refuse(UplinkError::Unexpected { kind: "data frame" }, out);
            }
        }
        self.conn.write_control(out);
        Ok(())
    }

    /// Writes what the relay's credit allows; returns whether it caught up.
    pub fn pump<W: Allocator, B: Allocator>(&mut self, window: &Window<W>, out: &mut Vec<u8, B>) -> Result<bool, UplinkError> {
        self.conn.write_control(out);
        let UplinkState::Open { mut sent } = self.state else {
            return Ok(false);
        };
        let chan = frame::EVENTS;
        let caught = loop {
            let want = sent.next();
            if want >= window.next() {
                break true;
            }
            if want < window.first() {
                let to = Seq(window.first().0 - 1);
                Frame::Gap { chan, from: want, to }.write(out);
                sent = to;
                continue;
            }
            self.scratch.clear();
            window.copy_into(want, &mut self.scratch).map_err(UplinkError::Window)?;
            if !self.carry_keys {
                strip_key(&mut self.scratch)?;
            }
            let len = Frame::data_len(chan, want, self.scratch.len());
            if !self.conn.credit(chan).fits(len) {
                break false;
            }
            write_data(out, chan, want, &self.scratch, &[]);
            self.conn.spend(chan, len)?;
            sent = want;
        };
        self.state = UplinkState::Open { sent };
        Ok(caught)
    }

    /// Queues a keepalive the relay answers.
    pub fn ping(&mut self, at: crate::Nanos) -> Result<(), UplinkError> {
        Ok(self.conn.queue(Control::Ping { at })?)
    }
}

/// Re-encodes a page item with its key zeroed (an event's key is written empty when zero).
fn strip_key<A: Allocator>(item: &mut Vec<u8, A>) -> Result<(), UplinkError> {
    let page = PageItem::read(item).map_err(|_| UplinkError::Item)?;
    if let PageItem::Run { name, seq, item: Item::Event { lane, n, mut event } } = page
        && event.key != [0; 32]
    {
        event.key = [0; 32];
        item.clear();
        PageItem::Run { name, seq, item: Item::Event { lane, n, event } }.write(item);
    }
    Ok(())
}
