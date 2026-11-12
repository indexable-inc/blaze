//! A pack's body compressed for transfer: the body cut into spans of a fixed length, each
//! compressed alone (zstd, by the driver), the frames one after another in one file, and
//! this index of the frames' lengths beside it:
//!
//! ```text
//! [ "blaze pack body spans", 1, span, body_len, [frame_len, ...] ]
//! ```
//!
//! A reader that wants bytes `at..at+len` of the body reads the frames of the spans they are
//! in ([`Spans::covering`], [`Spans::frame`]), decompresses each to at most `span` bytes and
//! takes its part. The index and the frames are under no hash of their own and need none:
//! what comes out is checked record by record against the manifest, as a raw body is, so a
//! wrong index or frame can only make a record fail its check. What the index *can* do is
//! ask for memory, so its numbers are bounded as it is read: a span is at most
//! [`SPAN_MOST`] (the most a frame is decompressed to), a frame at most a span and
//! [`FRAME_SLACK`], and there are exactly as many frames as the body has spans.
//!
//! The store keeps cold records compressed, but a pack's body holds each record as the
//! store reads it out (uncompressed), so this is the one compression of the bytes that
//! travel, not a second.

use alloc::{format, string::ToString as _, vec::Vec};

use crate::{Refused, cbor, manifest};

const MAGIC: &str = "blaze pack body spans";
pub const LAYOUT: u64 = 1;
/// The most bytes of one span: what one frame is decompressed to at most.
pub const SPAN_MOST: u64 = 16 << 20;
/// How much longer than its span a frame may be (data that does not compress).
pub const FRAME_SLACK: u64 = 64 << 10;
/// The most bytes of an index.
pub const INDEX_MOST: u64 = 4 << 20;
const WHAT: &str = "the pack's span index";

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Spans {
    span: u64,
    body_len: u64,
    /// Where each frame starts in the frames' file, and after the last, its length.
    starts: Vec<u64>,
}

impl Spans {
    /// The index of a body of `body_len` bytes cut into spans of `span`, whose frames are
    /// `frames` bytes long, in order.
    pub fn new(span: u64, body_len: u64, frames: &[u64]) -> Result<Spans, Refused> {
        if span == 0 || span > SPAN_MOST {
            return Err(Refused::TooLarge { what: "a span of the pack's body", len: span, most: SPAN_MOST });
        }
        if body_len > manifest::BODY_MOST {
            return Err(Refused::TooLarge { what: "the pack's body", len: body_len, most: manifest::BODY_MOST });
        }
        if frames.len() as u64 != body_len.div_ceil(span) {
            return Err(Refused::Malformed {
                what: WHAT,
                at: 0,
                why: "it has another number of frames than its body has spans",
            });
        }
        // alloc: once an index, a word a frame
        let mut starts = Vec::with_capacity(frames.len() + 1);
        let mut at = 0u64;
        for len in frames {
            if *len > span + FRAME_SLACK {
                return Err(Refused::TooLarge { what: "a frame of the pack's body", len: *len, most: span + FRAME_SLACK });
            }
            starts.push(at);
            at += len;
        }
        starts.push(at);
        Ok(Spans { span, body_len, starts })
    }

    pub fn span(&self) -> u64 {
        self.span
    }

    pub fn body_len(&self) -> u64 {
        self.body_len
    }

    /// The spans the bytes `at..at + len` of the body are in: the first and one past the
    /// last. Empty for no bytes or bytes past the body's end.
    pub fn covering(&self, at: u64, len: u64) -> (u64, u64) {
        let end = at.saturating_add(len).min(self.body_len);
        if len == 0 || at >= end {
            return (0, 0);
        }
        (at / self.span, end.div_ceil(self.span))
    }

    /// Where the frame of span `index` is in the frames' file and how long, and how many
    /// bytes it decompresses to.
    pub fn frame(&self, index: u64) -> (u64, u64, u64) {
        let at = self.starts[index as usize];
        let plain = (self.body_len - index * self.span).min(self.span);
        (at, self.starts[index as usize + 1] - at, plain)
    }

    pub fn encode(&self) -> Vec<u8> {
        // alloc: the index's bytes, which are the output
        let mut out = Vec::with_capacity(64 + self.starts.len() * 5);
        cbor::array(&mut out, 5);
        cbor::text(&mut out, MAGIC);
        cbor::uint(&mut out, LAYOUT);
        cbor::uint(&mut out, self.span);
        cbor::uint(&mut out, self.body_len);
        cbor::array(&mut out, self.starts.len() as u64 - 1);
        for pair in self.starts.windows(2) {
            cbor::uint(&mut out, pair[1] - pair[0]);
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Spans, Refused> {
        if bytes.len() as u64 > INDEX_MOST {
            return Err(Refused::TooLarge { what: WHAT, len: bytes.len() as u64, most: INDEX_MOST });
        }
        let mut reader = cbor::Reader::new(bytes);
        let malformed = |error: cbor::Error| Refused::Malformed { what: WHAT, at: error.at, why: error.why };
        let foreign = || Refused::Layout { what: WHAT, found: "not a blaze pack span index".to_string() };
        if reader.array().map_err(|_| foreign())? != 5 {
            return Err(foreign());
        }
        if reader.text().map_err(|_| foreign())? != MAGIC {
            return Err(foreign());
        }
        let version = reader.uint().map_err(malformed)?;
        if version != LAYOUT {
            // alloc: an error's text
            return Err(Refused::Layout { what: WHAT, found: format!("of layout {version}, not {LAYOUT}") });
        }
        let span = reader.uint().map_err(malformed)?;
        let body_len = reader.uint().map_err(malformed)?;
        // No more frames than the input has bytes (`Reader::array`), each a number.
        let count = reader.array().map_err(malformed)?;
        // alloc: the frames' lengths, no more than the input has bytes
        let mut frames = Vec::with_capacity(count as usize);
        for _ in 0..count {
            frames.push(reader.uint().map_err(malformed)?);
        }
        reader.end().map_err(malformed)?;
        Spans::new(span, body_len, &frames)
    }
}
