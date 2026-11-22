//! The transport's frames (rule 33: one versioned schema in deterministic CBOR): a 32-bit
//! little-endian length, then one CBOR array `[version, channel, kind, ...fields]`.
//!
//! | kind | fields | what |
//! |---|---|---|
//! | 0 `HELLO` | `role, epoch` | who speaks; the epoch names this process's numbering, so a resume from another process's numbers is not taken for one of these |
//! | 1 `OPEN` | `service, from` | the subscriber has seen everything up to `from` (0: nothing) |
//! | 2 `DATA` | `seq, bytes` | item `seq` of the channel's stream |
//! | 3 `GAP` | `from, to` | items `from..=to` will not come |
//! | 4 `CREDIT` | `bytes` | the receiver may take `bytes` more of the channel's data |
//! | 5 `CLOSE` | `code` | the channel ends |
//! | 6 `PING` / 7 `PONG` | `at` | a liveness probe and its echo |
//! | 8 `REQUEST` | `bytes` | a request to the run's parent (blaze-run's `viewer` wire form: a `Toggle`) |
//!
//! Limits: a frame of at most [`MOST_FRAME`] bytes after its length word, refused from the
//! length word alone, before any byte of it is buffered; data of at most [`MOST_DATA`]; a
//! channel below [`MOST_CHANNELS`]; every number checked against its field's type.

// adapted from makepad/relax (MIT), libs/remote/src/frame.rs (a length-prefixed frame with a
// channel and a kind; OPEN, DATA, CREDIT, CLOSE, PING, PONG); changed: the frame body is
// deterministic CBOR, a sequence number on DATA, GAP as a frame of its own, no continuation
// frames (an item is small; the cap refuses a large one).

#![blaze::sans_io]

use core::alloc::Allocator;
use core::fmt;

use blaze_own::Vec;

use crate::cbor::{self, Reader};
use crate::{Nanos, Seq};

pub const VERSION: u64 = 1;
/// The most bytes of a frame after its length word.
pub const MOST_FRAME: usize = 80 << 10;
/// The most bytes of one DATA item or REQUEST.
pub const MOST_DATA: usize = 64 << 10;
/// Channels are below this.
pub const MOST_CHANNELS: u16 = 16;
/// The length word.
pub const LENGTH: usize = 4;

/// The control channel: HELLO, PING, PONG, REQUEST.
pub const CONTROL: u16 = 0;
/// The event stream's channel.
pub const EVENTS: u16 = 1;

pub mod kind {
    pub const HELLO: u64 = 0;
    pub const OPEN: u64 = 1;
    pub const DATA: u64 = 2;
    pub const GAP: u64 = 3;
    pub const CREDIT: u64 = 4;
    pub const CLOSE: u64 = 5;
    pub const PING: u64 = 6;
    pub const PONG: u64 = 7;
    pub const REQUEST: u64 = 8;
}

/// Who is speaking.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// A blaze command's process, serving its events.
    Run,
    /// The viewer, reading runs or writing to the relay.
    Viewer,
    /// The relay on ix.dev.
    Relay,
}

/// The services a channel can be opened for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Service {
    Events,
}

/// Why a channel closed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Code {
    Normal,
    /// The peer broke the protocol (a duplicate, a skip, a bad frame).
    Protocol,
    /// The peer is not allowed this (a request from the relay).
    Refused,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Frame<'a> {
    Hello { role: Role, epoch: u64 },
    Open { chan: u16, service: Service, from: Seq },
    Data { chan: u16, seq: Seq, bytes: &'a [u8] },
    Gap { chan: u16, from: Seq, to: Seq },
    Credit { chan: u16, bytes: u32 },
    Close { chan: u16, code: Code },
    Ping { at: Nanos },
    Pong { at: Nanos },
    Request { bytes: &'a [u8] },
}

/// Why bytes are not a frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bad {
    /// A length word past [`MOST_FRAME`]: refused before the frame is read.
    TooLong { len: u32 },
    Cbor(cbor::Error),
    Version { found: u64 },
    Kind { found: u64 },
    Channel { found: u64 },
    /// A frame of the control channel's kind on another channel, or the reverse.
    WrongChannel { kind: u64, chan: u16 },
    Field { what: &'static str, found: u64 },
}

impl fmt::Display for Bad {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Bad::TooLong { len } => write!(f, "a frame of {len} bytes, past the cap of {MOST_FRAME}"),
            Bad::Cbor(error) => write!(f, "a frame: {error}"),
            Bad::Version { found } => write!(f, "a frame of schema {found}, where this blaze speaks {VERSION}"),
            Bad::Kind { found } => write!(f, "a frame of unknown kind {found}"),
            Bad::Channel { found } => write!(f, "a frame on channel {found}, past {MOST_CHANNELS}"),
            Bad::WrongChannel { kind, chan } => write!(f, "a frame of kind {kind} on channel {chan}"),
            Bad::Field { what, found } => write!(f, "a frame's {what} {found} is not one"),
        }
    }
}

impl core::error::Error for Bad {}

impl From<cbor::Error> for Bad {
    fn from(error: cbor::Error) -> Bad {
        Bad::Cbor(error)
    }
}

const fn role_code(role: Role) -> u64 {
    match role {
        Role::Run => 0,
        Role::Viewer => 1,
        Role::Relay => 2,
    }
}

const fn code_code(code: Code) -> u64 {
    match code {
        Code::Normal => 0,
        Code::Protocol => 1,
        Code::Refused => 2,
    }
}

impl Frame<'_> {
    pub fn chan(&self) -> u16 {
        match *self {
            Frame::Hello { .. } | Frame::Ping { .. } | Frame::Pong { .. } | Frame::Request { .. } => CONTROL,
            Frame::Open { chan, .. }
            | Frame::Data { chan, .. }
            | Frame::Gap { chan, .. }
            | Frame::Credit { chan, .. }
            | Frame::Close { chan, .. } => chan,
        }
    }

    /// Appends the frame, its length word first.
    pub fn write<A: Allocator>(&self, out: &mut Vec<u8, A>) {
        let start = out.len();
        out.extend_from_slice(&[0; LENGTH]);
        let fields = |out: &mut Vec<u8, A>, kind: u64, chan: u16, more: u64| {
            cbor::array(out, 3 + more);
            cbor::uint(out, VERSION);
            cbor::uint(out, u64::from(chan));
            cbor::uint(out, kind);
        };
        let chan = self.chan();
        match *self {
            Frame::Hello { role, epoch } => {
                fields(out, kind::HELLO, chan, 2);
                cbor::uint(out, role_code(role));
                cbor::uint(out, epoch);
            }
            Frame::Open { service, from, .. } => {
                fields(out, kind::OPEN, chan, 2);
                cbor::uint(out, match service {
                    Service::Events => 0,
                });
                cbor::uint(out, from.0);
            }
            Frame::Data { seq, bytes, .. } => {
                fields(out, kind::DATA, chan, 2);
                cbor::uint(out, seq.0);
                cbor::bytes(out, bytes);
            }
            Frame::Gap { from, to, .. } => {
                fields(out, kind::GAP, chan, 2);
                cbor::uint(out, from.0);
                cbor::uint(out, to.0);
            }
            Frame::Credit { bytes, .. } => {
                fields(out, kind::CREDIT, chan, 1);
                cbor::uint(out, u64::from(bytes));
            }
            Frame::Close { code, .. } => {
                fields(out, kind::CLOSE, chan, 1);
                cbor::uint(out, code_code(code));
            }
            Frame::Ping { at } => {
                fields(out, kind::PING, chan, 1);
                cbor::uint(out, at.0);
            }
            Frame::Pong { at } => {
                fields(out, kind::PONG, chan, 1);
                cbor::uint(out, at.0);
            }
            Frame::Request { bytes } => {
                fields(out, kind::REQUEST, chan, 1);
                cbor::bytes(out, bytes);
            }
        }
        let len = (out.len() - start - LENGTH) as u32;
        out[start..start + LENGTH].copy_from_slice(&len.to_le_bytes());
    }

    /// The bytes [`Frame::write`] appends for a DATA frame of `len` bytes at `seq`: what a
    /// sender checks against its credit before it writes.
    pub const fn data_len(chan: u16, seq: Seq, len: usize) -> usize {
        LENGTH
            + 1
            + cbor::head_len(VERSION)
            + cbor::head_len(chan as u64)
            + cbor::head_len(kind::DATA)
            + cbor::head_len(seq.0)
            + cbor::head_len(len as u64)
            + len
    }
}

/// The frame at the start of `input`, and the bytes it took; `None` while the input holds less
/// than a whole frame. A length past the cap is refused from its word alone.
pub fn read(input: &[u8]) -> Result<Option<(Frame<'_>, usize)>, Bad> {
    if input.len() < LENGTH {
        return Ok(None);
    }
    let len = u32::from_le_bytes([input[0], input[1], input[2], input[3]]);
    if len as usize > MOST_FRAME {
        return Err(Bad::TooLong { len });
    }
    let end = LENGTH + len as usize;
    if input.len() < end {
        return Ok(None);
    }
    let body = &input[LENGTH..end];
    let mut r = Reader::new(body);
    let fields = r.array(8, "a frame of more fields than any kind has")?;
    let version = r.uint()?;
    if version != VERSION {
        return Err(Bad::Version { found: version });
    }
    let chan = r.uint()?;
    if chan >= u64::from(MOST_CHANNELS) {
        return Err(Bad::Channel { found: chan });
    }
    let chan = chan as u16;
    let kind = r.uint()?;
    let want = |n: usize| match fields == 3 + n {
        true => Ok(()),
        false => Err(Bad::Field { what: "field count", found: fields as u64 }),
    };
    let control = matches!(kind, kind::HELLO | kind::PING | kind::PONG | kind::REQUEST);
    if control != (chan == CONTROL) && kind <= kind::REQUEST {
        return Err(Bad::WrongChannel { kind, chan });
    }
    let frame = match kind {
        kind::HELLO => {
            want(2)?;
            let role = match r.uint()? {
                0 => Role::Run,
                1 => Role::Viewer,
                2 => Role::Relay,
                found => return Err(Bad::Field { what: "role", found }),
            };
            Frame::Hello { role, epoch: r.uint()? }
        }
        kind::OPEN => {
            want(2)?;
            let service = match r.uint()? {
                0 => Service::Events,
                found => return Err(Bad::Field { what: "service", found }),
            };
            Frame::Open { chan, service, from: Seq(r.uint()?) }
        }
        kind::DATA => {
            want(2)?;
            let seq = Seq(r.uint()?);
            if seq == Seq::NONE {
                return Err(Bad::Field { what: "sequence number", found: 0 });
            }
            Frame::Data { chan, seq, bytes: r.bytes(MOST_DATA, "an item past the data cap")? }
        }
        kind::GAP => {
            want(2)?;
            let (from, to) = (r.uint()?, r.uint()?);
            if from == 0 || to < from {
                return Err(Bad::Field { what: "gap end", found: to });
            }
            Frame::Gap { chan, from: Seq(from), to: Seq(to) }
        }
        kind::CREDIT => {
            want(1)?;
            Frame::Credit { chan, bytes: r.uint_most(u64::from(u32::MAX), "a credit past 32 bits")? as u32 }
        }
        kind::CLOSE => {
            want(1)?;
            let code = match r.uint()? {
                0 => Code::Normal,
                1 => Code::Protocol,
                2 => Code::Refused,
                found => return Err(Bad::Field { what: "close code", found }),
            };
            Frame::Close { chan, code }
        }
        kind::PING => {
            want(1)?;
            Frame::Ping { at: Nanos(r.uint()?) }
        }
        kind::PONG => {
            want(1)?;
            Frame::Pong { at: Nanos(r.uint()?) }
        }
        kind::REQUEST => {
            want(1)?;
            Frame::Request { bytes: r.bytes(MOST_DATA, "a request past the data cap")? }
        }
        found => return Err(Bad::Kind { found }),
    };
    r.end()?;
    Ok(Some((frame, end)))
}
