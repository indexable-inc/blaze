//! The response parser: a sans-IO state machine fed the response in any pieces, its body
//! appended to the caller's vector as it comes. The head is kept (at most
//! [`Limits::most_head`] bytes, [`Limits::most_headers`] lines); the body is bounded by
//! [`Limits::most_body`] before a byte past it is written.
//!
//! **Framing** (RFC 9112 section 6.3): no body for a HEAD request, a 204 or a 304; `chunked`
//! when `Transfer-Encoding` says so; `Content-Length` bytes when it says so; else the body ends
//! when the connection does ([`Response::closed`]). A response with both `Content-Length` and
//! `Transfer-Encoding`, two `Content-Length`s that differ, or a coding other than `chunked` is
//! refused, never guessed at. Interim responses (1xx but 101) are read past.

#![blaze::sans_io]

use core::alloc::Allocator;

use blaze_own::Vec;

use crate::{
    error::{Error, Why},
    plant::{self, Plant},
    request::{Range, is_token},
};

/// How much a response may hold: every field is the caller's choice.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// The most bytes of the head (status line and headers), and of a chunked body's trailer.
    pub most_head: u32,
    /// The most header lines.
    pub most_headers: u16,
    /// The most bytes of the body.
    pub most_body: u64,
}

/// What one [`Response::feed`] did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fed {
    /// Bytes of this call's input taken. Less than it was given only once the response ended.
    pub consumed: usize,
    pub done: bool,
}

/// The range a `206` says it holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ContentRange {
    pub first: u64,
    pub last: u64,
    /// The whole resource's length, unless the server says `*`.
    pub complete: Option<u64>,
}

/// One header: its name's and value's offsets in the head.
#[derive(Clone, Copy)]
struct Field {
    name: (u32, u32),
    value: (u32, u32),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Chunk {
    Size { value: u64, digits: u8, ext: bool, cr: bool, line: u16 },
    Data { left: u64 },
    DataCr,
    DataLf,
    Trailer { blank: bool, bytes: u32 },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Framing {
    /// The head is not yet whole.
    Head,
    Empty,
    Length { left: u64 },
    Chunked(Chunk),
    Close,
}

const LINE_MOST: u16 = 4096;
const NEW_CHUNK: Chunk = Chunk::Size { value: 0, digits: 0, ext: false, cr: false, line: 0 };

/// The response parser: see the module.
pub struct Response<A: Allocator> {
    limits: Limits,
    head_request: bool,
    head: Vec<u8, A>,
    fields: Vec<Field, A>,
    line_start: usize,
    status: u16,
    length: Option<u64>,
    framing: Framing,
    body: u64,
    taken: u64,
    done: bool,
}

impl<A: Allocator + Clone> Response<A> {
    /// A parser held to `limits`, for the answer to a request whose method was HEAD or not.
    pub fn new_in(limits: Limits, head_request: bool, alloc: A) -> Response<A> {
        Response {
            limits,
            head_request,
            head: Vec::new_in(alloc.clone()),
            fields: Vec::new_in(alloc),
            line_start: 0,
            status: 0,
            length: None,
            framing: Framing::Head,
            body: 0,
            taken: 0,
            done: false,
        }
    }
}

impl<A: Allocator> Response<A> {
    /// Takes what it can of `input`, appending the body's bytes to `out`.
    pub fn feed<B: Allocator>(&mut self, input: &[u8], out: &mut Vec<u8, B>) -> Result<Fed, Error> {
        let mut at = 0;
        let ran = self.run(input, &mut at, out);
        let error_at = self.taken + at as u64;
        self.taken += at as u64;
        ran.map_err(|why| Error { at: error_at, why })?;
        Ok(Fed { consumed: at, done: self.done })
    }

    /// The connection ended: the body ends with it when nothing else frames it, and is cut
    /// short otherwise.
    pub fn closed(&mut self) -> Result<(), Error> {
        match self.framing {
            _ if self.done => Ok(()),
            Framing::Close => {
                self.done = true;
                Ok(())
            }
            _ => Err(Error { at: self.taken, why: Why::Truncated { body: self.body } }),
        }
    }

    /// Whether the response has ended.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Whether the head is whole (so [`Response::status`] and the headers are known).
    pub fn has_head(&self) -> bool {
        self.framing != Framing::Head
    }

    pub fn status(&self) -> u16 {
        self.status
    }

    /// The first header named `name` (any case), its value trimmed.
    pub fn header(&self, name: &str) -> Option<&[u8]> {
        self.headers().find(|(n, _)| n.eq_ignore_ascii_case(name.as_bytes())).map(|(_, v)| v)
    }

    /// Every header, in order.
    pub fn headers(&self) -> impl Iterator<Item = (&[u8], &[u8])> {
        let head = &self.head[..];
        self.fields.iter().map(move |f| {
            (&head[f.name.0 as usize..f.name.1 as usize], &head[f.value.0 as usize..f.value.1 as usize])
        })
    }

    /// Bytes of body so far.
    pub fn body_len(&self) -> u64 {
        self.body
    }

    /// The range this response holds, checked against the range `asked` for: a 206 whose
    /// `Content-Range` starts where asked, ends where asked or at the resource's end, and
    /// whose `Content-Length` (if any) is its length.
    pub fn ranged(&self, asked: Range) -> Result<ContentRange, Error> {
        let fail = |why| Error { at: 0, why };
        if self.status != 206 {
            return Err(fail(Why::RangeStatus { status: self.status }));
        }
        let got = self.header("content-range").and_then(content_range).ok_or(fail(Why::ContentRange))?;
        let mismatch = fail(Why::RangeMismatch { first: got.first, last: got.last });
        let ends_right = match (asked.last, got.complete) {
            (Some(last), _) if got.last == last => true,
            (Some(last), Some(complete)) => got.last < last && got.last + 1 == complete,
            (Some(_), None) => false,
            (None, Some(complete)) => got.last + 1 == complete,
            (None, None) => true,
        };
        if got.first != asked.first || !ends_right {
            return Err(mismatch);
        }
        if self.length.is_some_and(|len| len != got.last - got.first + 1) {
            return Err(mismatch);
        }
        Ok(got)
    }

    fn run<B: Allocator>(&mut self, input: &[u8], at: &mut usize, out: &mut Vec<u8, B>) -> Result<(), Why> {
        while !self.done {
            let rest = &input[*at..];
            match self.framing {
                Framing::Head => {
                    let Some(end) = rest.iter().position(|&b| b == b'\n') else {
                        self.head_bytes(rest)?;
                        *at += rest.len();
                        return Ok(());
                    };
                    self.head_bytes(&rest[..=end])?;
                    *at += end + 1;
                    let line = &self.head[self.line_start..];
                    if line == b"\n" || line == b"\r\n" {
                        self.parse_head()?;
                    } else {
                        self.line_start = self.head.len();
                    }
                }
                Framing::Empty => self.done = true,
                Framing::Length { left } => {
                    if rest.is_empty() {
                        return Ok(());
                    }
                    let n = left.min(rest.len() as u64) as usize;
                    self.emit(&rest[..n], out)?;
                    *at += n;
                    self.framing = Framing::Length { left: left - n as u64 };
                    self.done = n as u64 == left;
                }
                Framing::Close => {
                    self.emit(rest, out)?;
                    *at += rest.len();
                    return Ok(());
                }
                Framing::Chunked(chunk) => {
                    if rest.is_empty() {
                        return Ok(());
                    }
                    let fresh = *at == 0;
                    *at += self.chunk(chunk, rest, fresh, out)?;
                }
            }
        }
        Ok(())
    }

    fn head_bytes(&mut self, bytes: &[u8]) -> Result<(), Why> {
        if self.head.len() + bytes.len() > self.limits.most_head as usize {
            return Err(Why::HeadOver { most: self.limits.most_head });
        }
        self.head.try_extend_from_slice(bytes).map_err(Why::Alloc)
    }

    fn emit<B: Allocator>(&mut self, bytes: &[u8], out: &mut Vec<u8, B>) -> Result<(), Why> {
        if self.body + bytes.len() as u64 > self.limits.most_body {
            return Err(Why::BodyOver { most: self.limits.most_body });
        }
        out.try_extend_from_slice(bytes).map_err(Why::Alloc)?;
        self.body += bytes.len() as u64;
        Ok(())
    }

    /// One step of a chunked body over `rest`; returns the bytes it took.
    fn chunk<B: Allocator>(&mut self, chunk: Chunk, rest: &[u8], fresh: bool, out: &mut Vec<u8, B>) -> Result<usize, Why> {
        if let Chunk::Data { left } = chunk {
            let n = left.min(rest.len() as u64) as usize;
            self.emit(&rest[..n], out)?;
            self.framing = Framing::Chunked(match n as u64 == left {
                true => Chunk::DataCr,
                false => Chunk::Data { left: left - n as u64 },
            });
            return Ok(n);
        }
        let b = rest[0];
        let next = match chunk {
            Chunk::Size { .. } if fresh && plant::on(Plant::SizeLineRestarts) => NEW_CHUNK,
            other => other,
        };
        let next = match next {
            Chunk::Size { value, digits, ext, cr, line } => {
                let line = line + 1;
                if line > LINE_MOST {
                    return Err(Why::ChunkLineOver);
                }
                match b {
                    b'\n' if cr => match (digits, value) {
                        (0, _) => return Err(Why::ChunkSize),
                        (_, 0) => Chunk::Trailer { blank: true, bytes: 0 },
                        (_, value) => Chunk::Data { left: value },
                    },
                    _ if cr => return Err(Why::ChunkSize),
                    b'\r' => Chunk::Size { value, digits, ext, cr: true, line },
                    _ if ext => {
                        if b < b' ' && b != b'\t' {
                            return Err(Why::ChunkSize);
                        }
                        Chunk::Size { value, digits, ext, cr, line }
                    }
                    b';' => Chunk::Size { value, digits, ext: true, cr, line },
                    _ => {
                        let d = (b as char).to_digit(16).ok_or(Why::ChunkSize)?;
                        let value = value.checked_mul(16).and_then(|v| v.checked_add(u64::from(d))).ok_or(Why::ChunkSize)?;
                        Chunk::Size { value, digits: digits.saturating_add(1), ext, cr, line }
                    }
                }
            }
            Chunk::DataCr if b == b'\r' => Chunk::DataLf,
            Chunk::DataLf if b == b'\n' => NEW_CHUNK,
            Chunk::DataCr | Chunk::DataLf => return Err(Why::ChunkEnd),
            Chunk::Trailer { blank, bytes } => {
                let bytes = bytes + 1;
                if bytes > self.limits.most_head {
                    return Err(Why::HeadOver { most: self.limits.most_head });
                }
                match b {
                    b'\n' if blank => {
                        self.done = true;
                        Chunk::Trailer { blank, bytes }
                    }
                    b'\n' => Chunk::Trailer { blank: true, bytes },
                    b'\r' => Chunk::Trailer { blank, bytes },
                    _ => Chunk::Trailer { blank: false, bytes },
                }
            }
            Chunk::Data { .. } => unreachable!("handled above"),
        };
        self.framing = Framing::Chunked(next);
        Ok(1)
    }

    /// The whole head parsed, and the body's framing chosen.
    fn parse_head(&mut self) -> Result<(), Why> {
        self.fields.clear();
        let head = &self.head[..];
        let mut lines = head.split(|&b| b == b'\n').map(|l| l.strip_suffix(b"\r").unwrap_or(l));
        let status = status_line(lines.next().unwrap_or(b""))?;
        let mut offset = head.iter().position(|&b| b == b'\n').map_or(head.len(), |i| i + 1);
        for line in lines {
            let start = offset;
            offset += line.len() + 1;
            if head.get(start + line.len()) == Some(&b'\r') {
                offset += 1;
            }
            if line.is_empty() {
                break;
            }
            if self.fields.len() >= usize::from(self.limits.most_headers) {
                return Err(Why::HeadersOver { most: self.limits.most_headers });
            }
            let colon = line.iter().position(|&b| b == b':').ok_or(Why::HeaderLine)?;
            let name = &line[..colon];
            if !core::str::from_utf8(name).is_ok_and(is_token) {
                return Err(Why::HeaderLine);
            }
            let raw = &line[colon + 1..];
            if raw.iter().any(|&b| (b < b' ' && b != b'\t') || b == 0x7f) {
                return Err(Why::HeaderLine);
            }
            let lead = raw.iter().take_while(|&&b| b == b' ' || b == b'\t').count();
            let trail = raw[lead..].iter().rev().take_while(|&&b| b == b' ' || b == b'\t').count();
            let v0 = start + colon + 1 + lead;
            let v1 = start + colon + 1 + raw.len() - trail;
            self.fields
                .try_push(Field { name: (start as u32, (start + colon) as u32), value: (v0 as u32, v1 as u32) })
                .map_err(|refused| Why::Alloc(refused.why))?;
        }
        self.status = status;
        if status == 101 {
            return Err(Why::Switching);
        }
        if (100..200).contains(&status) {
            // An interim response: the real one follows.
            self.head.clear();
            self.fields.clear();
            self.line_start = 0;
            return Ok(());
        }
        self.framing = self.framing()?;
        Ok(())
    }

    fn framing(&mut self) -> Result<Framing, Why> {
        let mut coding: Option<&[u8]> = None;
        let mut length: Option<u64> = None;
        for (name, value) in self.headers() {
            if name.eq_ignore_ascii_case(b"transfer-encoding") {
                if coding.is_some() {
                    return Err(Why::TransferEncoding);
                }
                coding = Some(value);
            } else if name.eq_ignore_ascii_case(b"content-length") {
                let n = digits(value).ok_or(Why::ContentLength)?;
                if length.is_some_and(|l| l != n) {
                    return Err(Why::ContentLength);
                }
                length = Some(n);
            }
        }
        let empty = self.head_request || self.status == 204 || self.status == 304;
        let framing = match (coding, length) {
            (Some(_), Some(_)) => return Err(Why::LengthAndChunked),
            _ if empty => Framing::Empty,
            (Some(coding), None) if coding.eq_ignore_ascii_case(b"chunked") => Framing::Chunked(NEW_CHUNK),
            (Some(_), None) => return Err(Why::TransferEncoding),
            (None, Some(n)) => {
                if n > self.limits.most_body {
                    return Err(Why::BodyOver { most: self.limits.most_body });
                }
                Framing::Length { left: n }
            }
            (None, None) => Framing::Close,
        };
        self.length = length;
        if framing == (Framing::Length { left: 0 }) {
            self.done = true;
        }
        Ok(framing)
    }
}

/// `HTTP/1.x NNN reason`: the status.
fn status_line(line: &[u8]) -> Result<u16, Why> {
    let rest = line.strip_prefix(b"HTTP/1.").ok_or(Why::StatusLine)?;
    match rest {
        [b'0' | b'1', b' ', a, b, c, tail @ ..] if [a, b, c].iter().all(|d| d.is_ascii_digit()) => {
            if !(tail.is_empty() || tail[0] == b' ') || tail.iter().any(|&b| b < b' ' && b != b'\t') {
                return Err(Why::StatusLine);
            }
            Ok(u16::from(a - b'0') * 100 + u16::from(b - b'0') * 10 + u16::from(c - b'0'))
        }
        _ => Err(Why::StatusLine),
    }
}

/// Decimal digits, at least one and at most 19 (so no overflow), none else.
fn digits(value: &[u8]) -> Option<u64> {
    if value.is_empty() || value.len() > 19 || !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(value.iter().fold(0u64, |n, &b| n * 10 + u64::from(b - b'0')))
}

/// `bytes first-last/complete` (or `/*`).
fn content_range(value: &[u8]) -> Option<ContentRange> {
    let rest = value.strip_prefix(b"bytes ")?;
    let dash = rest.iter().position(|&b| b == b'-')?;
    let slash = rest.iter().position(|&b| b == b'/')?;
    if slash < dash {
        return None;
    }
    let first = digits(&rest[..dash])?;
    let last = digits(&rest[dash + 1..slash])?;
    let complete = match &rest[slash + 1..] {
        b"*" => None,
        n => Some(digits(n)?),
    };
    if last < first || complete.is_some_and(|c| last >= c) {
        return None;
    }
    Some(ContentRange { first, last, complete })
}
