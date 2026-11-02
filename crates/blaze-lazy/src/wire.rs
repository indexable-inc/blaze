//! The fetch protocol's frames, as blaze-inproc's channel carries them between the run
//! process and the daemon: fixed-layout little-endian, each one whole message.
//!
//! - `Ask { request, lazy }`, from the run process: the code of lazy function `lazy`. A
//!   request id is the asker's, unique among its asks, and echoed in the answer; asking
//!   again with the same id (after a lost message) is the same ask.
//! - `Ready { request, lazy, target }`, from the daemon: the function's code is published in
//!   its slot at `target`, and was before this was sent.
//! - `Failed { request, lazy, why }`: it can't be compiled, and why; the run stops.
//!
//! Each travels as the body of a blaze-inproc frame of the kind [`channel_kind`] gives, in
//! the range blaze-inproc keeps for this crate (`frame::kinds::LAZY`); the channel's own
//! sequence number pairs an answer with its ask on the wire.
//!
//! Decoding checks every length and refuses unknown kinds, bytes left over and a reason that
//! isn't UTF-8 or is longer than [`MOST_WHY`]: bytes from the other process never panic it.

use std::fmt;

use blaze_bytes::le::{get_u32 as u32_at, get_u64 as u64_at};
use blaze_inproc::frame::{Kind, kinds};

/// The longest reason a `Failed` frame carries.
pub const MOST_WHY: usize = 64 << 10;

const ASK: u32 = 0x4c5a_0001;
const READY: u32 = 0x4c5a_0002;
const FAILED: u32 = 0x4c5a_0003;

/// The size of an `Ask` frame, a `Ready` frame and a `Failed` frame's header.
pub const ASK_LEN: usize = 16;
pub const READY_LEN: usize = 24;
pub const FAILED_LEN: usize = 24;

/// The blaze-inproc kinds the frames travel as.
pub const ASK_KIND: Kind = Kind(kinds::LAZY.start);
pub const READY_KIND: Kind = Kind(kinds::LAZY.start + 1);
pub const FAILED_KIND: Kind = Kind(kinds::LAZY.start + 2);

/// The blaze-inproc kind `frame` travels as.
pub fn channel_kind(frame: &Frame<'_>) -> Kind {
    match frame {
        Frame::Ask { .. } => ASK_KIND,
        Frame::Ready { .. } => READY_KIND,
        Frame::Failed { .. } => FAILED_KIND,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Frame<'a> {
    Ask { request: u64, lazy: u32 },
    Ready { request: u64, lazy: u32, target: u64 },
    Failed { request: u64, lazy: u32, why: &'a str },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum WireError {
    Short { kind: u32, len: usize, need: usize },
    Kind(u32),
    Trailing { kind: u32, extra: usize },
    LongWhy(usize),
    Utf8,
    /// A `Failed` frame whose four bytes after the reason's length are not zero.
    Padding,
    /// An answer sent to the daemon, which only asks come to.
    Unasked,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::Short { kind, len, need } => {
                write!(f, "a lazy frame of kind {kind:#x} of {len} bytes, which needs {need}")
            }
            WireError::Kind(kind) => write!(f, "a lazy frame of unknown kind {kind:#x}"),
            WireError::Trailing { kind, extra } => {
                write!(f, "a lazy frame of kind {kind:#x} with {extra} bytes left over")
            }
            WireError::LongWhy(len) => write!(f, "a failure reason of {len} bytes, past {MOST_WHY}"),
            WireError::Utf8 => write!(f, "a failure reason that is not UTF-8"),
            WireError::Padding => write!(f, "a failure frame whose padding is not zero"),
            WireError::Unasked => write!(f, "an answer frame from the run process, which only asks"),
        }
    }
}

impl std::error::Error for WireError {}

impl Frame<'_> {
    /// Appends the frame to `out`, which grows at most once.
    pub fn encode(&self, out: &mut Vec<u8>) {
        match *self {
            Frame::Ask { request, lazy } => {
                out.reserve(ASK_LEN);
                head(out, ASK, lazy, request);
            }
            Frame::Ready { request, lazy, target } => {
                out.reserve(READY_LEN);
                head(out, READY, lazy, request);
                out.extend_from_slice(&target.to_le_bytes());
            }
            Frame::Failed { request, lazy, why } => {
                let why = &why.as_bytes()[..floor(why, MOST_WHY)];
                out.reserve(FAILED_LEN + why.len());
                head(out, FAILED, lazy, request);
                out.extend_from_slice(&(why.len() as u32).to_le_bytes());
                out.extend_from_slice(&[0; 4]);
                out.extend_from_slice(why);
            }
        }
    }

    /// The frame `bytes` are, read in place.
    pub fn decode(bytes: &[u8]) -> Result<Frame<'_>, WireError> {
        let kind = u32_at(bytes, 0).ok_or(WireError::Short { kind: 0, len: bytes.len(), need: 4 })?;
        let need = match kind {
            ASK => ASK_LEN,
            READY => READY_LEN,
            FAILED => FAILED_LEN,
            other => return Err(WireError::Kind(other)),
        };
        if bytes.len() < need {
            return Err(WireError::Short { kind, len: bytes.len(), need });
        }
        let lazy = u32_at(bytes, 4).expect("checked");
        let request = u64_at(bytes, 8).expect("checked");
        let exact = |len: usize| {
            if bytes.len() == len { Ok(()) } else { Err(WireError::Trailing { kind, extra: bytes.len() - len }) }
        };
        match kind {
            ASK => {
                exact(ASK_LEN)?;
                Ok(Frame::Ask { request, lazy })
            }
            READY => {
                exact(READY_LEN)?;
                Ok(Frame::Ready { request, lazy, target: u64_at(bytes, 16).expect("checked") })
            }
            _ => {
                let len = u32_at(bytes, 16).expect("checked") as usize;
                if u32_at(bytes, 20) != Some(0) {
                    return Err(WireError::Padding);
                }
                if len > MOST_WHY {
                    return Err(WireError::LongWhy(len));
                }
                let end = FAILED_LEN + len;
                if bytes.len() < end {
                    return Err(WireError::Short { kind, len: bytes.len(), need: end });
                }
                exact(end)?;
                let why = std::str::from_utf8(&bytes[FAILED_LEN..end]).map_err(|_| WireError::Utf8)?;
                Ok(Frame::Failed { request, lazy, why })
            }
        }
    }
}

/// An `Ask` frame, on the stack: the hook allocates nothing for it.
pub fn ask(request: u64, lazy: u32) -> [u8; ASK_LEN] {
    let mut out = [0; ASK_LEN];
    out[0..4].copy_from_slice(&ASK.to_le_bytes());
    out[4..8].copy_from_slice(&lazy.to_le_bytes());
    out[8..16].copy_from_slice(&request.to_le_bytes());
    out
}

/// A `Ready` frame, on the stack: an ask of a published function allocates nothing.
pub fn ready(request: u64, lazy: u32, target: u64) -> [u8; READY_LEN] {
    let mut out = [0; READY_LEN];
    out[0..4].copy_from_slice(&READY.to_le_bytes());
    out[4..8].copy_from_slice(&lazy.to_le_bytes());
    out[8..16].copy_from_slice(&request.to_le_bytes());
    out[16..24].copy_from_slice(&target.to_le_bytes());
    out
}

/// The frames' fields are written and read by these three helpers, not by
/// `blaze_format::cursor`: the run process links this module without the `daemon` feature,
/// with only blaze-inproc behind it, and `blaze-format` would bring zstd and serde into every
/// running program.
fn head(out: &mut Vec<u8>, kind: u32, lazy: u32, request: u64) {
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&lazy.to_le_bytes());
    out.extend_from_slice(&request.to_le_bytes());
}

/// The longest prefix of `text` of at most `most` bytes that ends on a character boundary.
fn floor(text: &str, most: usize) -> usize {
    if text.len() <= most {
        return text.len();
    }
    (0..=most).rev().find(|it| text.is_char_boundary(*it)).unwrap_or(0)
}
