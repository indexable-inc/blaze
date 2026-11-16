// adapted from makepad/relax (MIT), libs/relax/base/src/git/smart_http.rs
//! pkt-line (git's `gitprotocol-common`): four hex digits of length (the four included), then
//! the payload; `0000` flush, `0001` delimiter, `0002` response end. And the side-band a v2
//! `packfile` section rides in: each line's first byte is its band (1 data, 2 progress,
//! 3 error).

#![blaze::sans_io]

use core::alloc::Allocator;

use blaze_own::{AllocFailure, Vec};

/// One pkt-line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pkt<'a> {
    Data(&'a [u8]),
    Flush,
    Delim,
    End,
}

/// Why a pkt-line stream was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PktWhy {
    /// The length is no four hex digits, or 3, or past the bytes left.
    Length,
    /// The stream ended inside a line, or before the flush it needs.
    Truncated,
    /// A side-band line on band 3: the server's error, at this offset in the stream.
    Remote,
    /// A side-band line on no band 1, 2 or 3.
    Band { band: u8 },
    /// A line not the protocol's at this point.
    Unexpected,
    Alloc(AllocFailure),
}

/// The most payload of one line (65520 less the four length digits).
pub const MOST: usize = 65516;

/// A data line holding `payload`, appended to `out`.
pub fn data<B: Allocator>(payload: &[u8], out: &mut Vec<u8, B>) -> Result<(), PktWhy> {
    if payload.len() > MOST {
        return Err(PktWhy::Length);
    }
    out.try_reserve(payload.len() + 4).map_err(PktWhy::Alloc)?;
    out.extend_from_slice(&hex4(payload.len() + 4));
    out.extend_from_slice(payload);
    Ok(())
}

/// The data line of the parts' concatenation.
pub fn line<B: Allocator>(parts: &[&[u8]], out: &mut Vec<u8, B>) -> Result<(), PktWhy> {
    let len: usize = parts.iter().map(|p| p.len()).sum();
    if len > MOST {
        return Err(PktWhy::Length);
    }
    out.try_reserve(len + 4).map_err(PktWhy::Alloc)?;
    out.extend_from_slice(&hex4(len + 4));
    for part in parts {
        out.extend_from_slice(part);
    }
    Ok(())
}

pub fn flush<B: Allocator>(out: &mut Vec<u8, B>) -> Result<(), PktWhy> {
    out.try_extend_from_slice(b"0000").map_err(PktWhy::Alloc)
}

pub fn delim<B: Allocator>(out: &mut Vec<u8, B>) -> Result<(), PktWhy> {
    out.try_extend_from_slice(b"0001").map_err(PktWhy::Alloc)
}

fn hex4(n: usize) -> [u8; 4] {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    [DIGITS[(n >> 12) & 15], DIGITS[(n >> 8) & 15], DIGITS[(n >> 4) & 15], DIGITS[n & 15]]
}

/// Reads pkt-lines from a whole response.
pub struct Lines<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Lines<'a> {
    pub const fn new(bytes: &'a [u8]) -> Lines<'a> {
        Lines { bytes, pos: 0 }
    }

    /// The offset of the next line.
    pub const fn offset(&self) -> usize {
        self.pos
    }

    /// The next line, or `None` at the stream's end.
    pub fn next_line(&mut self) -> Result<Option<Pkt<'a>>, PktWhy> {
        let rest = &self.bytes[self.pos..];
        if rest.is_empty() {
            return Ok(None);
        }
        let head = rest.get(..4).ok_or(PktWhy::Truncated)?;
        let mut len = 0usize;
        for &b in head {
            len = len * 16 + (b as char).to_digit(16).ok_or(PktWhy::Length)? as usize;
        }
        let pkt = match len {
            0 => Pkt::Flush,
            1 => Pkt::Delim,
            2 => Pkt::End,
            3 => return Err(PktWhy::Length),
            n => Pkt::Data(rest.get(4..n).ok_or(PktWhy::Truncated)?),
        };
        self.pos += len.max(4);
        Ok(Some(pkt))
    }

    /// The next line, which must be there.
    pub fn expect_line(&mut self) -> Result<Pkt<'a>, PktWhy> {
        self.next_line()?.ok_or(PktWhy::Truncated)
    }
}

/// A data line's text, its newline taken off.
pub fn text(payload: &[u8]) -> &[u8] {
    payload.strip_suffix(b"\n").unwrap_or(payload)
}

/// The side-band's band 1 bytes from `lines` up to its flush, appended to `out`; progress is
/// dropped (it is for a terminal), an error refused.
pub fn sideband<B: Allocator>(lines: &mut Lines<'_>, most: u64, out: &mut Vec<u8, B>) -> Result<(), PktWhy> {
    loop {
        match lines.expect_line()? {
            Pkt::Flush | Pkt::End => return Ok(()),
            Pkt::Delim => return Err(PktWhy::Unexpected),
            Pkt::Data([]) => return Err(PktWhy::Unexpected),
            Pkt::Data([1, rest @ ..]) => {
                if out.len() as u64 + rest.len() as u64 > most {
                    return Err(PktWhy::Length);
                }
                out.try_extend_from_slice(rest).map_err(PktWhy::Alloc)?;
            }
            Pkt::Data([2, ..]) => {}
            Pkt::Data([3, ..]) => return Err(PktWhy::Remote),
            Pkt::Data([band, ..]) => return Err(PktWhy::Band { band: *band }),
        }
    }
}
