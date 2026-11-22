//! The response parser: a sans-IO codec fed the response in any pieces ([`Response::feed`] and
//! [`Response::feed_head`], each answering what it took as a [`Fed`]; [`Response::closed`] is its
//! finish, the connection's end), its body appended to the caller's vector as it comes. The head
//! is kept (at most [`Limits::most_head`] bytes, [`Limits::most_headers`] lines); the body is
//! bounded by [`Limits::most_body`] before a byte past it is written, and a chunk-size line by
//! [`Limits::most_chunk_line`].
//!
//! Its state is one enum ([`State`]): the head being read, the body being read as its framing
//! says, or the response ended; the head's facts (status, version, `Content-Length`) exist only
//! once the head is whole, so nothing stands for "not yet known" by a zero.
//!
//! **Framing** (RFC 9112 section 6.3): no body for a HEAD request, a 204 or a 304; `chunked`
//! when `Transfer-Encoding` says so; `Content-Length` bytes when it says so; else the body ends
//! when the connection does ([`Response::closed`]). A response with both `Content-Length` and
//! `Transfer-Encoding`, two `Content-Length`s that differ, or a coding other than `chunked` is
//! refused, never guessed at. Interim responses (1xx but 101) are read past.

#![blaze::sans_io]

use core::alloc::Allocator;

use blaze_own::{
    Vec,
    planted::{NoPlant, Planted},
};

use crate::{
    error::{Error, Why},
    out::Out,
    plant::H1Plant,
    request::{Range, is_token},
};

/// How much a response may hold (D165: every cap of the parser is a field here): every field is
/// the caller's choice, and [`Limits::SHIPPED`] is what blaze's own clients ship with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// The most bytes of the head (status line and headers), and of a chunked body's trailer.
    pub most_head: u32,
    /// The most header lines.
    pub most_headers: u16,
    /// The most bytes of the body.
    pub most_body: u64,
    /// The most bytes of one chunk-size line, its extensions and CRLF included.
    pub most_chunk_line: u16,
}

impl Limits {
    /// The values blaze's clients ship with: a 64 KiB head of at most 100 lines, a chunk-size
    /// line of at most 4096 bytes, and a body bounded by the caller alone.
    pub const SHIPPED: Limits =
        Limits { most_head: 64 << 10, most_headers: 100, most_body: u64::MAX, most_chunk_line: 4096 };
}

/// The method of the request a response answers, as far as its framing cares (RFC 9112 section
/// 6.3): the answer to a HEAD request has no body, whatever its head says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method {
    /// The request was a HEAD: its answer's body is empty.
    Head,
    /// Any other method: its answer's head frames its body.
    Other,
}

/// What a [`Response`] is made from ([`Response::new`], rule 83).
///
/// A call's named arguments: write it as a literal in the call that takes it (rule 83).
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct ResponseOpts<A> {
    /// The caps the response is held to.
    pub limits: Limits,
    /// The method of the request it answers.
    pub method: Method,
    /// The allocator the head and its header index are kept in.
    pub alloc: A,
}

/// What one [`Response::feed`] did: the codec's progress.
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

/// The `x` of a status line's `HTTP/1.x`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Version {
    Http10,
    Http11,
}

/// The head's facts, known once it is whole.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Whole {
    status: u16,
    version: Version,
    /// The `Content-Length` the head gave, whatever framed the body.
    length: Option<u64>,
}

/// Where a chunk-size line's reader is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SizeAt {
    /// In the hex digits.
    Digits,
    /// In the extensions, after a `;`.
    Extension,
    /// After the CR: the LF ends the line.
    Cr,
}

/// Where a trailer's reader is in its line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LineAt {
    /// At a line's start (or after its CR only): an LF here is the blank line that ends it.
    Start,
    /// Inside a line.
    Inside,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Chunk {
    /// A chunk-size line: the size so far, its digits, where its reader is and its bytes so far.
    Size { value: u64, digits: u8, at: SizeAt, line: u32 },
    Data { left: u64 },
    DataCr,
    DataLf,
    Trailer { at: LineAt, bytes: u32 },
}

/// How a whole head says its body is read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Framing {
    Empty,
    Length { left: u64 },
    Chunked(Chunk),
    Close,
}

/// The parser's one state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// The head is not yet whole; the line being read starts at `line_start` of the head's bytes.
    Head { line_start: usize },
    /// The head is whole, and the body is being read as `framing` says.
    Body { whole: Whole, framing: Framing },
    /// The response has ended; `framing` is how its body was read, where it stopped.
    Ended { whole: Whole, framing: Framing },
}

const NEW_CHUNK: Chunk = Chunk::Size { value: 0, digits: 0, at: SizeAt::Digits, line: 0 };

/// The response parser: see the module.
pub struct Response<A: Allocator> {
    limits: Limits,
    method: Method,
    head: Vec<u8, A>,
    fields: Vec<Field, A>,
    state: State,
    body: u64,
    taken: u64,
}

impl<A: Allocator + Clone> Response<A> {
    /// The parser `opts` describe: held to `opts.limits`, for the answer to a request of
    /// `opts.method`, its head kept in `opts.alloc`.
    pub fn new(opts: ResponseOpts<A>) -> Response<A> {
        Response {
            limits: opts.limits,
            method: opts.method,
            head: Vec::new_in(opts.alloc.clone()),
            fields: Vec::new_in(opts.alloc),
            state: State::Head { line_start: 0 },
            body: 0,
            taken: 0,
        }
    }

    /// A parser held to `limits`, for the answer to a request whose method was HEAD or not.
    ///
    /// Deprecated (C2: a bool parameter is an enum): write
    /// `Response::new(ResponseOpts { limits, method: Method::Head or Method::Other, alloc })`.
    pub fn new_in(limits: Limits, head_request: bool, alloc: A) -> Response<A> {
        let method = if head_request { Method::Head } else { Method::Other };
        Response::new(ResponseOpts { limits, method, alloc })
    }
}

impl<A: Allocator> Response<A> {
    /// Takes what it can of `input`, appending the body's bytes to `out`.
    pub fn feed<O: Out + ?Sized>(&mut self, input: &[u8], out: &mut O) -> Result<Fed, Error> {
        self.feed_planted(input, out, NoPlant)
    }

    /// [`Response::feed`] with its plant seam (D165): `plant` is [`NoPlant`] where it ships and an
    /// [`H1Plant`] under a control.
    pub fn feed_planted<O: Out + ?Sized>(
        &mut self,
        input: &[u8],
        out: &mut O,
        plant: impl Planted<H1Plant>,
    ) -> Result<Fed, Error> {
        let mut at = 0;
        let ran = self.run(input, &mut at, out, plant);
        let error_at = self.taken + at as u64;
        self.taken += at as u64;
        ran.map_err(|why| Error { at: error_at, why })?;
        Ok(Fed { consumed: at, done: self.is_done() })
    }

    /// Takes the head alone from `input`, interim heads read past, stopping where the body
    /// starts: for a client that reads a body only as fast as its asker takes it. `consumed`
    /// is all of `input` until the head is whole.
    pub fn feed_head(&mut self, input: &[u8]) -> Result<Fed, Error> {
        let mut at = 0;
        let mut ran = Ok(());
        while let State::Head { line_start } = self.state {
            match self.head_line(line_start, input, &mut at) {
                Ok(true) => {}
                Ok(false) => break,
                Err(why) => {
                    ran = Err(why);
                    break;
                }
            }
        }
        let error_at = self.taken + at as u64;
        self.taken += at as u64;
        ran.map_err(|why| Error { at: error_at, why })?;
        Ok(Fed { consumed: at, done: self.is_done() })
    }

    /// The head's facts and the body's framing, once the head is whole.
    fn framed(&self) -> Option<(Whole, Framing)> {
        match self.state {
            State::Head { .. } => None,
            State::Body { whole, framing } | State::Ended { whole, framing } => Some((whole, framing)),
        }
    }

    /// The `Content-Length` that frames the body, once the head is whole; none for a body
    /// framed otherwise or for none (a HEAD request's, a 204's, a 304's).
    pub fn content_length(&self) -> Option<u64> {
        match self.framed() {
            Some((whole, Framing::Length { .. })) => whole.length,
            _ => None,
        }
    }

    /// Whether the connection can take another request once this response has ended: HTTP/1.1
    /// with no `Connection: close`, or HTTP/1.0 with `Connection: keep-alive`, and a body not
    /// ended by the close (RFC 9112 section 9.3).
    pub fn reusable(&self) -> bool {
        let Some((whole, framing)) = self.framed() else {
            return false;
        };
        let (mut close, mut keep_alive) = (false, false);
        for (name, value) in self.headers() {
            if name.eq_ignore_ascii_case(b"connection") {
                for token in value.split(|&b| b == b',') {
                    let token = token.trim_ascii();
                    close |= token.eq_ignore_ascii_case(b"close");
                    keep_alive |= token.eq_ignore_ascii_case(b"keep-alive");
                }
            }
        }
        let keep = match whole.version {
            Version::Http11 => !close,
            Version::Http10 => keep_alive && !close,
        };
        keep && framing != Framing::Close
    }

    /// The connection ended (the codec's finish): the body ends with it when nothing else
    /// frames it, and is cut short otherwise.
    pub fn closed(&mut self) -> Result<(), Error> {
        match self.state {
            State::Ended { .. } => Ok(()),
            State::Body { whole, framing: Framing::Close } => {
                self.state = State::Ended { whole, framing: Framing::Close };
                Ok(())
            }
            State::Head { .. } | State::Body { .. } => {
                Err(Error { at: self.taken, why: Why::Truncated { body: self.body } })
            }
        }
    }

    /// Whether the response has ended.
    pub fn is_done(&self) -> bool {
        matches!(self.state, State::Ended { .. })
    }

    /// Whether the head is whole (so [`Response::status`] and the headers are known).
    pub fn has_head(&self) -> bool {
        !matches!(self.state, State::Head { .. })
    }

    /// The status, once the head is whole ([`Response::has_head`]); 0 before.
    pub fn status(&self) -> u16 {
        self.framed().map_or(0, |(whole, _)| whole.status)
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
        let (status, length) = self.framed().map_or((0, None), |(whole, _)| (whole.status, whole.length));
        if status != 206 {
            return Err(fail(Why::RangeStatus { status }));
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
        if length.is_some_and(|len| len != got.last - got.first + 1) {
            return Err(mismatch);
        }
        Ok(got)
    }

    /// One line of the head from `input[*at..]`, the line being read starting at `line_start`
    /// of the head's bytes, the head parsed at its blank line: false when the input ran out
    /// before the line's end.
    fn head_line(&mut self, line_start: usize, input: &[u8], at: &mut usize) -> Result<bool, Why> {
        let rest = &input[*at..];
        let Some(end) = rest.iter().position(|&b| b == b'\n') else {
            self.head_bytes(rest)?;
            *at += rest.len();
            return Ok(false);
        };
        self.head_bytes(&rest[..=end])?;
        *at += end + 1;
        let line = &self.head[line_start..];
        if line == b"\n" || line == b"\r\n" {
            self.parse_head()?;
        } else {
            self.state = State::Head { line_start: self.head.len() };
        }
        Ok(true)
    }

    fn run<O: Out + ?Sized>(
        &mut self,
        input: &[u8],
        at: &mut usize,
        out: &mut O,
        plant: impl Planted<H1Plant>,
    ) -> Result<(), Why> {
        loop {
            let rest = &input[*at..];
            match self.state {
                State::Ended { .. } => return Ok(()),
                State::Head { line_start } => {
                    if !self.head_line(line_start, input, at)? {
                        return Ok(());
                    }
                }
                State::Body { whole, framing: Framing::Empty } => {
                    self.state = State::Ended { whole, framing: Framing::Empty };
                }
                State::Body { whole, framing: Framing::Length { left } } => {
                    if rest.is_empty() {
                        return Ok(());
                    }
                    let n = left.min(rest.len() as u64) as usize;
                    self.emit(&rest[..n], out)?;
                    *at += n;
                    let framing = Framing::Length { left: left - n as u64 };
                    self.state = match n as u64 == left {
                        true => State::Ended { whole, framing },
                        false => State::Body { whole, framing },
                    };
                }
                State::Body { framing: Framing::Close, .. } => {
                    self.emit(rest, out)?;
                    *at += rest.len();
                    return Ok(());
                }
                State::Body { whole, framing: Framing::Chunked(chunk) } => {
                    if rest.is_empty() {
                        return Ok(());
                    }
                    let fresh = *at == 0;
                    *at += self.chunk(whole, chunk, rest, fresh, out, plant)?;
                }
            }
        }
    }

    fn head_bytes(&mut self, bytes: &[u8]) -> Result<(), Why> {
        if self.head.len() + bytes.len() > self.limits.most_head as usize {
            return Err(Why::HeadOver { most: self.limits.most_head });
        }
        self.head.try_extend_from_slice(bytes).map_err(Why::Alloc)
    }

    fn emit<O: Out + ?Sized>(&mut self, bytes: &[u8], out: &mut O) -> Result<(), Why> {
        if self.body + bytes.len() as u64 > self.limits.most_body {
            return Err(Why::BodyOver { most: self.limits.most_body });
        }
        out.try_put(bytes).map_err(Why::Alloc)?;
        self.body += bytes.len() as u64;
        Ok(())
    }

    /// One step of a chunked body over `rest`, the head being `whole`; returns the bytes it took.
    fn chunk<O: Out + ?Sized>(
        &mut self,
        whole: Whole,
        chunk: Chunk,
        rest: &[u8],
        fresh: bool,
        out: &mut O,
        plant: impl Planted<H1Plant>,
    ) -> Result<usize, Why> {
        let b = rest[0];
        let chunk = match chunk {
            Chunk::Size { .. } if fresh && plant.has(H1Plant::SizeLineRestarts) => NEW_CHUNK,
            other => other,
        };
        let mut ended = false;
        let next = match chunk {
            Chunk::Data { left } => {
                let n = left.min(rest.len() as u64) as usize;
                self.emit(&rest[..n], out)?;
                let next = match n as u64 == left {
                    true => Chunk::DataCr,
                    false => Chunk::Data { left: left - n as u64 },
                };
                self.state = State::Body { whole, framing: Framing::Chunked(next) };
                return Ok(n);
            }
            Chunk::Size { value, digits, at, line } => {
                let line = line + 1;
                if line > u32::from(self.limits.most_chunk_line) {
                    return Err(Why::ChunkLineOver { most: self.limits.most_chunk_line });
                }
                match (at, b) {
                    (SizeAt::Cr, b'\n') => match (digits, value) {
                        (0, _) => return Err(Why::ChunkSize),
                        (_, 0) => Chunk::Trailer { at: LineAt::Start, bytes: 0 },
                        (_, value) => Chunk::Data { left: value },
                    },
                    (SizeAt::Cr, _) => return Err(Why::ChunkSize),
                    (_, b'\r') => Chunk::Size { value, digits, at: SizeAt::Cr, line },
                    (SizeAt::Extension, _) => {
                        if b < b' ' && b != b'\t' {
                            return Err(Why::ChunkSize);
                        }
                        Chunk::Size { value, digits, at: SizeAt::Extension, line }
                    }
                    (SizeAt::Digits, b';') => Chunk::Size { value, digits, at: SizeAt::Extension, line },
                    (SizeAt::Digits, _) => {
                        let d = (b as char).to_digit(16).ok_or(Why::ChunkSize)?;
                        let value =
                            value.checked_mul(16).and_then(|v| v.checked_add(u64::from(d))).ok_or(Why::ChunkSize)?;
                        Chunk::Size { value, digits: digits.saturating_add(1), at: SizeAt::Digits, line }
                    }
                }
            }
            Chunk::DataCr if b == b'\r' => Chunk::DataLf,
            Chunk::DataLf if b == b'\n' => NEW_CHUNK,
            Chunk::DataCr | Chunk::DataLf => return Err(Why::ChunkEnd),
            Chunk::Trailer { at, bytes } => {
                let bytes = bytes + 1;
                if bytes > self.limits.most_head {
                    return Err(Why::HeadOver { most: self.limits.most_head });
                }
                match (at, b) {
                    (LineAt::Start, b'\n') => {
                        ended = true;
                        Chunk::Trailer { at, bytes }
                    }
                    (LineAt::Inside, b'\n') => Chunk::Trailer { at: LineAt::Start, bytes },
                    (_, b'\r') => Chunk::Trailer { at, bytes },
                    _ => Chunk::Trailer { at: LineAt::Inside, bytes },
                }
            }
        };
        let framing = Framing::Chunked(next);
        self.state = match ended {
            true => State::Ended { whole, framing },
            false => State::Body { whole, framing },
        };
        Ok(1)
    }

    /// The whole head parsed, and the body's framing chosen.
    fn parse_head(&mut self) -> Result<(), Why> {
        self.fields.clear();
        let head = &self.head[..];
        let mut lines = head.split(|&b| b == b'\n').map(|l| l.strip_suffix(b"\r").unwrap_or(l));
        let (version, status) = status_line(lines.next().unwrap_or(b""))?;
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
        if status == 101 {
            return Err(Why::Switching);
        }
        if (100..200).contains(&status) {
            // An interim response: the real one follows.
            self.head.clear();
            self.fields.clear();
            self.state = State::Head { line_start: 0 };
            return Ok(());
        }
        let (framing, length) = self.framing(status)?;
        let whole = Whole { status, version, length };
        self.state = match framing {
            Framing::Empty | Framing::Length { left: 0 } => State::Ended { whole, framing },
            _ => State::Body { whole, framing },
        };
        Ok(())
    }

    /// How the body of a response of `status` is framed, and the `Content-Length` its head gave.
    fn framing(&self, status: u16) -> Result<(Framing, Option<u64>), Why> {
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
        let empty = self.method == Method::Head || status == 204 || status == 304;
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
        Ok((framing, length))
    }
}

/// `HTTP/1.x NNN reason`: the version, and the status.
fn status_line(line: &[u8]) -> Result<(Version, u16), Why> {
    let rest = line.strip_prefix(b"HTTP/1.").ok_or(Why::StatusLine)?;
    match rest {
        [minor @ (b'0' | b'1'), b' ', a, b, c, tail @ ..] if [a, b, c].iter().all(|d| d.is_ascii_digit()) => {
            if !(tail.is_empty() || tail[0] == b' ') || tail.iter().any(|&b| b < b' ' && b != b'\t') {
                return Err(Why::StatusLine);
            }
            let version = if *minor == b'1' { Version::Http11 } else { Version::Http10 };
            Ok((version, u16::from(a - b'0') * 100 + u16::from(b - b'0') * 10 + u16::from(c - b'0')))
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
