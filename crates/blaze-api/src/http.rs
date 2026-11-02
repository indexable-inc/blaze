//! HTTP/1.1, sans-IO: requests and responses cut from bytes as they arrive, and written as
//! bytes. For the API's HTTP side ([`crate::server`], [`crate::client`]), CI's webhook and its
//! GitHub client.
//!
//! Strict: a head line over 8 KiB, more than 100 headers, a body over the caller's bound
//! (413), both a length and chunks, another transfer coding, a bad chunk size or a line not
//! ended by CRLF is refused, naming what. Nothing is allocated on the peer's word before its
//! bytes are here.

use std::{fmt, io};

use blaze_core::decimal::decimal;

/// The longest line of a head or of a chunk's size, without its CRLF.
const LONGEST_LINE: usize = 8 << 10;
/// The most header lines of a head, and of a chunked body's trailers.
const MOST_HEADERS: usize = 100;
/// What [`RequestParser::read_from`] reads at once while it does not know how much is coming.
const READ: usize = 16 << 10;
/// The most room [`RequestParser::reset`] keeps for the next connection: one large body is
/// not held by every handler from then on.
const KEPT: usize = 1 << 20;

#[derive(PartialEq, Eq, Debug)]
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    /// The first header named `name`, in any case.
    pub fn header(&self, name: &str) -> Option<&str> {
        header(&self.headers, name)
    }
}

#[derive(PartialEq, Eq, Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// The first header named `name`, in any case.
    pub fn header(&self, name: &str) -> Option<&str> {
        header(&self.headers, name)
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(it, _)| it.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
}

/// A request in place ([`RequestParser::next_ref`]): its head's text and its body, lent from
/// the parser until its next call, with nothing allocated or copied.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HttpRequestRef<'a> {
    pub method: &'a str,
    pub path: &'a str,
    /// The head's header lines, checked, with CRLF between them.
    lines: &'a str,
    pub body: &'a [u8],
}

impl<'a> HttpRequestRef<'a> {
    /// The first header named `name`, in any case.
    pub fn header(&self, name: &str) -> Option<&'a str> {
        self.headers().find(|(it, _)| it.eq_ignore_ascii_case(name)).map(|(_, value)| value)
    }

    /// Every header, in order, its value trimmed.
    pub fn headers(&self) -> impl Iterator<Item = (&'a str, &'a str)> + use<'a> {
        pairs(self.lines)
    }

    /// The owned request, for a caller that keeps it past the parser's next call.
    pub fn to_request(&self) -> HttpRequest {
        let (method, path, headers) = self.owned_head();
        // alloc: the owned request's body, as above.
        HttpRequest { method, path, headers, body: self.body.to_vec() }
    }

    /// Its method, path and headers, owned.
    fn owned_head(&self) -> (String, String, Vec<(String, String)>) {
        // alloc: an owned request, for a caller that keeps it past the parser's next call.
        let owned = |(name, value): (&str, &str)| (name.to_owned(), value.to_owned());
        // alloc: as above.
        (self.method.to_owned(), self.path.to_owned(), self.headers().map(owned).collect())
    }
}

/// The header lines of a head [`check_head`] passed, as names and trimmed values.
fn pairs(lines: &str) -> impl Iterator<Item = (&str, &str)> {
    lines.split("\r\n").filter_map(|line| {
        let (name, value) = line.split_once(':')?;
        Some((name, value.trim_matches([' ', '\t'])))
    })
}

/// Bytes refused: the status a server answers them with, and what was wrong.
// clone: error path; a parser out of step gives its error again on every later call.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HttpError {
    pub status: u16,
    pub what: String,
}

impl HttpError {
    fn new(status: u16, what: impl Into<String>) -> HttpError {
        HttpError { status, what: what.into() }
    }
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HTTP {} {}: {}", self.status, reason(self.status), self.what)
    }
}

impl std::error::Error for HttpError {}

/// Requests cut from a connection's bytes, one after another (pipelined requests wait in it).
/// A request's head stays where it arrived until the request is handed out, checked in place.
#[derive(Debug)]
pub struct RequestParser {
    most_body: usize,
    /// The connection's bytes are `bytes[..end]`. What is past `end` is room a read fills,
    /// zeroed once, when it grew.
    bytes: Vec<u8>,
    end: usize,
    state: RequestState,
    /// A chunked body, decoded; kept from request to request.
    chunked: Vec<u8>,
    /// Empty, with the room of the last buffer `push_vec` left free (up to `KEPT`): what the
    /// next read goes into.
    spare: Vec<u8>,
    broken: Option<HttpError>,
}

#[derive(Debug)]
enum RequestState {
    Head(HeadScan),
    /// The head, checked, is `bytes[start..end]`; its body is being read.
    Body { start: usize, end: usize, body: BodyRead },
    /// A request was lent out of `bytes[..used]` (and `chunked`): dropped at the next call.
    Lent { used: usize },
}

#[derive(Debug)]
enum BodyRead {
    /// This many bytes, from the head's end.
    Length(usize),
    /// Chunks decoded into the parser's `chunked`, the bytes read up to `at`.
    Chunked { chunks: Chunks, at: usize },
}

/// A request read whole: its head at `start..end`, its body at `body` or in `chunked`.
#[derive(Clone, Copy)]
struct Whole {
    start: usize,
    end: usize,
    body: Option<(usize, usize)>,
}

impl RequestParser {
    pub fn new(most_body: usize) -> RequestParser {
        // alloc: none until a push; the room then grows once and `reset` keeps it.
        let (bytes, chunked, spare) = (Vec::new(), Vec::new(), Vec::new());
        let state = RequestState::Head(HeadScan::default());
        RequestParser { most_body, bytes, end: 0, state, chunked, spare, broken: None }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        if self.broken.is_some() {
            return;
        }
        self.release();
        let room = (self.bytes.len() - self.end).min(bytes.len());
        let (here, rest) = bytes.split_at(room);
        self.bytes[self.end..self.end + room].copy_from_slice(here);
        self.bytes.extend_from_slice(rest);
        self.end += bytes.len();
    }

    /// One read from `from` into the room after what it holds: the rest of a body whose length
    /// is known, else up to 16 KiB, or all the room it has if that is more. `Ok(0)` is the end
    /// of the stream. Once the stream is out of step ([`RequestParser::next`]'s error) it
    /// reads nothing and gives `Ok(0)`.
    pub fn read_from(&mut self, from: &mut impl io::Read) -> io::Result<usize> {
        if self.broken.is_some() {
            return Ok(0);
        }
        self.release();
        let rest = match self.state {
            RequestState::Body { end, body: BodyRead::Length(len), .. } => {
                (end + len).saturating_sub(self.end)
            }
            _ => 0,
        };
        let want = if rest > 0 { rest } else { READ };
        if self.bytes.len() < self.end + want {
            // alloc: room grows to the largest request a connection sends, kept by `reset`.
            self.bytes.resize(self.end + want, 0);
        }
        let got = loop {
            match from.read(&mut self.bytes[self.end..]) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                got => break got?,
            }
        };
        self.end += got;
        Ok(got)
    }

    /// Back to the start of a connection, with the room it reads into kept (up to 1 MiB), so a
    /// handler keeps one parser for every connection it serves.
    pub fn reset(&mut self) {
        if self.bytes.len() > KEPT {
            drop(std::mem::take(&mut self.bytes));
        }
        self.end = 0;
        self.state = RequestState::Head(HeadScan::default());
        self.chunked.clear();
        self.broken = None;
    }

    /// [`RequestParser::push`] of a buffer that is the caller's no more: kept as it is,
    /// uncopied, when nothing is held before it. The buffer this leaves free (the one the
    /// parser held, or `bytes` once copied) is kept for [`RequestParser::lend_spare`].
    pub(crate) fn push_vec(&mut self, bytes: Vec<u8>) {
        let free = match self.broken.is_some() {
            true => bytes,
            false => {
                self.release();
                match self.end == 0 {
                    true => {
                        let free = std::mem::replace(&mut self.bytes, bytes);
                        self.end = self.bytes.len();
                        free
                    }
                    false => {
                        self.push(&bytes);
                        bytes
                    }
                }
            }
        };
        self.spare = free;
        self.spare.clear();
        self.spare.shrink_to(KEPT);
    }

    /// An empty buffer with the room the last [`RequestParser::push_vec`] left free, for the
    /// next read to fill and give back through `push_vec`: warm reads allocate nothing.
    pub(crate) fn lend_spare(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.spare)
    }

    /// The next whole request (Content-Length or chunked body), `Ok(None)` until one is here.
    /// After an error every call gives it again: the connection is not in step any more.
    pub fn next(&mut self) -> Result<Option<HttpRequest>, HttpError> {
        let Some(whole) = self.whole()? else { return Ok(None) };
        // The owned request (the webhook's); the API server reads in place (`next_ref`).
        let (method, path, headers) = self.lend(whole).owned_head();
        let body = match whole.body {
            Some((from, to)) => {
                self.bytes.truncate(self.end);
                let rest = self.bytes.split_off(to);
                let mut body = std::mem::replace(&mut self.bytes, rest);
                body.drain(..from);
                self.end = self.bytes.len();
                body
            }
            None => {
                let body = std::mem::take(&mut self.chunked);
                self.release();
                body
            }
        };
        self.state = RequestState::Head(HeadScan::default());
        Ok(Some(HttpRequest { method, path, headers, body }))
    }

    /// [`RequestParser::next`] in place: the request's head and body stay in the parser,
    /// lent until its next call.
    pub fn next_ref(&mut self) -> Result<Option<HttpRequestRef<'_>>, HttpError> {
        let Some(whole) = self.whole()? else { return Ok(None) };
        Ok(Some(self.lend(whole)))
    }

    /// How many bytes it holds: of the request it is reading and of any after it.
    pub(crate) fn held(&self) -> usize {
        match self.state {
            RequestState::Lent { used } => self.end - used,
            _ => self.end,
        }
    }

    fn whole(&mut self) -> Result<Option<Whole>, HttpError> {
        if let Some(error) = &self.broken {
            // clone: error path, a connection out of step.
            return Err(error.clone());
        }
        self.release();
        self.step().inspect_err(|error| {
            // clone: error path, once per connection out of step.
            self.broken = Some(error.clone());
            drop(std::mem::take(&mut self.bytes));
            self.end = 0;
        })
    }

    /// Drops the request lent out last.
    fn release(&mut self) {
        if let RequestState::Lent { used } = self.state {
            self.bytes.copy_within(used..self.end, 0);
            self.end -= used;
            self.chunked.clear();
            self.state = RequestState::Head(HeadScan::default());
        }
    }

    fn lend(&self, whole: Whole) -> HttpRequestRef<'_> {
        let head = checked(&self.bytes[whole.start..whole.end]);
        let body = match whole.body {
            Some((from, to)) => &self.bytes[from..to],
            None => &self.chunked,
        };
        HttpRequestRef { method: head.method, path: head.path, lines: head.lines, body }
    }

    fn step(&mut self) -> Result<Option<Whole>, HttpError> {
        loop {
            match &mut self.state {
                RequestState::Lent { .. } => unreachable!("a lent request is released first"),
                RequestState::Head(scan) => {
                    let Some(end) = scan.scan(&self.bytes[..self.end])? else { return Ok(None) };
                    let start = scan.start;
                    let head = check_head(&self.bytes[start..end])?;
                    let body = match framing(pairs(head.lines), self.most_body)? {
                        Framing::Length(len) => BodyRead::Length(len),
                        Framing::Chunked => {
                            self.chunked.clear();
                            BodyRead::Chunked { chunks: Chunks::Size, at: end }
                        }
                        Framing::None => BodyRead::Length(0),
                    };
                    self.state = RequestState::Body { start, end, body };
                }
                RequestState::Body { start, end, body: BodyRead::Length(len) } => {
                    let (start, end, len) = (*start, *end, *len);
                    if self.end - end < len {
                        return Ok(None);
                    }
                    self.state = RequestState::Lent { used: end + len };
                    return Ok(Some(Whole { start, end, body: Some((end, end + len)) }));
                }
                RequestState::Body { start, end, body: BodyRead::Chunked { chunks, at } } => {
                    let mut decoded = self.chunked.len();
                    let most = self.most_body;
                    let bytes = &self.bytes[..self.end];
                    if !chunks.decode(bytes, at, &mut self.chunked, &mut decoded, most)? {
                        return Ok(None);
                    }
                    let (whole, used) = (Whole { start: *start, end: *end, body: None }, *at);
                    self.state = RequestState::Lent { used };
                    return Ok(Some(whole));
                }
            }
        }
    }
}

/// A response cut from a connection's bytes as they arrive: its head, then its body as it
/// comes (chunks decoded). A push of no bytes is the end of the stream, which ends a body
/// that has neither a length nor chunks. Interim responses (1xx but 101) are skipped. A
/// response to `HEAD` is not told apart: it is read as having the body its head names.
#[derive(Debug)]
pub struct ResponseParser {
    most_body: usize,
    bytes: Vec<u8>,
    scan: HeadScan,
    head: Option<(u16, Vec<(String, String)>)>,
    body: ResponseBody,
    out: Vec<u8>,
    decoded: usize,
    ended: bool,
    broken: Option<HttpError>,
}

#[derive(Debug)]
enum ResponseBody {
    Length(usize),
    Chunked(Chunks),
    UntilEnd,
    Done,
}

impl ResponseParser {
    pub fn new(most_body: usize) -> ResponseParser {
        // alloc: none until a push; once per response, a client's.
        let (bytes, out) = (Vec::new(), Vec::new());
        let (scan, body) = (HeadScan::default(), ResponseBody::Done);
        ResponseParser { most_body, bytes, scan, head: None, body, out, decoded: 0, ended: false, broken: None }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<(), HttpError> {
        if let Some(error) = &self.broken {
            // clone: error path, a response out of step.
            return Err(error.clone());
        }
        match bytes.is_empty() {
            true => self.ended = true,
            false => self.bytes.extend_from_slice(bytes),
        }
        self.step().inspect_err(|error| {
            // clone: error path, once per response out of step.
            self.broken = Some(error.clone());
            drop(std::mem::take(&mut self.bytes));
        })
    }

    pub fn head(&self) -> Option<(u16, &[(String, String)])> {
        self.head.as_ref().map(|(status, headers)| (*status, headers.as_slice()))
    }

    /// The head, moved out for a caller done with the parser.
    fn take_head(&mut self) -> Option<(u16, Vec<(String, String)>)> {
        self.head.take()
    }

    /// Body bytes decoded since the last call.
    pub fn take_body(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    pub fn finished(&self) -> bool {
        self.head.is_some() && matches!(self.body, ResponseBody::Done)
    }

    /// The bytes after the end of the response, once it is finished: the next response's.
    pub(crate) fn rest(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.bytes)
    }

    fn step(&mut self) -> Result<(), HttpError> {
        while self.head.is_none() {
            let Some(end) = self.scan.scan(&self.bytes)? else {
                if self.ended {
                    return Err(HttpError::new(502, "the stream ended inside a response's head"));
                }
                return Ok(());
            };
            let (status, headers) = response_head(&self.bytes[self.scan.start..end])?;
            self.bytes.drain(..end);
            self.scan = HeadScan::default();
            if (100..200).contains(&status) && status != 101 {
                continue;
            }
            self.body = match status {
                101 | 204 | 304 => ResponseBody::Done,
                _ => match framing(
                    headers.iter().map(|(name, value)| (name.as_str(), value.as_str())),
                    self.most_body,
                )? {
                    Framing::Length(0) => ResponseBody::Done,
                    Framing::Length(len) => ResponseBody::Length(len),
                    Framing::Chunked => ResponseBody::Chunked(Chunks::Size),
                    Framing::None => ResponseBody::UntilEnd,
                },
            };
            self.head = Some((status, headers));
        }
        let cut_short = || HttpError::new(502, "the stream ended inside a response's body");
        match &mut self.body {
            ResponseBody::Length(left) => {
                let here = self.bytes.len().min(*left);
                match here == self.bytes.len() && self.out.is_empty() {
                    true => std::mem::swap(&mut self.out, &mut self.bytes),
                    false => self.out.extend(self.bytes.drain(..here)),
                }
                *left -= here;
                if *left == 0 {
                    self.body = ResponseBody::Done;
                } else if self.ended {
                    return Err(cut_short());
                }
            }
            ResponseBody::Chunked(chunks) => {
                let mut at = 0;
                let done = chunks.decode(
                    &self.bytes,
                    &mut at,
                    &mut self.out,
                    &mut self.decoded,
                    self.most_body,
                );
                self.bytes.drain(..at);
                if done? {
                    self.body = ResponseBody::Done;
                } else if self.ended {
                    return Err(cut_short());
                }
            }
            ResponseBody::UntilEnd => {
                self.decoded += self.bytes.len();
                if self.decoded > self.most_body {
                    // alloc: error path, a body past the bound.
                    let said = format!("a body of more than {} bytes", self.most_body);
                    return Err(HttpError::new(413, said));
                }
                match self.out.is_empty() {
                    true => std::mem::swap(&mut self.out, &mut self.bytes),
                    false => self.out.append(&mut self.bytes),
                }
                if self.ended {
                    self.body = ResponseBody::Done;
                }
            }
            ResponseBody::Done => {}
        }
        Ok(())
    }
}

/// Finds where a head ends as its bytes arrive, holding its lines to the limits. Blank lines
/// before the start line are skipped (RFC 9112 section 2.2).
#[derive(Default, Debug)]
struct HeadScan {
    /// How far the bytes have been looked at.
    at: usize,
    line_start: usize,
    /// Where the start line begins.
    start: usize,
    lines: usize,
}

impl HeadScan {
    /// Where the head ends in `bytes` (past its blank line), once it is whole.
    fn scan(&mut self, bytes: &[u8]) -> Result<Option<usize>, HttpError> {
        let too_long = || HttpError::new(431, "a head line longer than 8 KiB");
        loop {
            let Some(found) = memchr::memchr(b'\n', &bytes[self.at..]) else {
                self.at = bytes.len();
                if self.at - self.line_start > LONGEST_LINE + 1 {
                    return Err(too_long());
                }
                return Ok(None);
            };
            let end = self.at + found;
            if end - self.line_start > LONGEST_LINE + 1 {
                return Err(too_long());
            }
            self.at = end + 1;
            let line = &bytes[self.line_start..end];
            let Some(line) = line.strip_suffix(b"\r") else {
                return Err(HttpError::new(400, "a head line ended by LF without CR"));
            };
            let empty = line.is_empty();
            self.line_start = self.at;
            match (empty, self.lines) {
                (true, 0) => self.start = self.at,
                (true, _) => return Ok(Some(self.at)),
                (false, _) => {
                    self.lines += 1;
                    if self.lines > MOST_HEADERS + 1 {
                        return Err(HttpError::new(431, "more than 100 headers"));
                    }
                }
            }
        }
    }
}

/// A whole head's text, its blank line left off: its start line, then its header lines with
/// CRLF between them.
fn text(head: &[u8]) -> Result<(&str, &str), HttpError> {
    let head = head.strip_suffix(b"\r\n\r\n").unwrap_or(head);
    let text =
        std::str::from_utf8(head).map_err(|_| HttpError::new(400, "a head that is not UTF-8"))?;
    // A CR inside a line is one that does not begin a CRLF.
    if memchr::memchr_iter(b'\r', head).any(|at| head.get(at + 1) != Some(&b'\n')) {
        return Err(HttpError::new(400, "a CR inside a head line"));
    }
    Ok(text.split_once("\r\n").unwrap_or((text, "")))
}

/// A request's head, checked, in place.
struct Head<'a> {
    method: &'a str,
    path: &'a str,
    lines: &'a str,
}

/// A request head [`check_head`] passed, cut again: only the UTF-8 is looked at again, which
/// `&str` needs.
fn checked(head: &[u8]) -> Head<'_> {
    let head = head.strip_suffix(b"\r\n\r\n").unwrap_or(head);
    let text = std::str::from_utf8(head).expect("a checked head is UTF-8");
    let (start, lines) = text.split_once("\r\n").unwrap_or((text, ""));
    let mut parts = start.split(' ');
    let method = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or_default();
    Head { method, path, lines }
}

/// A request's head checked: its start line, then each header line, the first offence
/// named. Nothing is allocated.
fn check_head(head: &[u8]) -> Result<Head<'_>, HttpError> {
    let (start, lines) = text(head)?;
    let mut parts = start.split(' ');
    let (Some(method), Some(path), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(HttpError::new(400, "a request line that is not METHOD PATH VERSION"));
    };
    if method.is_empty() || !method.bytes().all(is_tchar) {
        return Err(HttpError::new(400, "a method that is not a token"));
    }
    if path.is_empty() {
        return Err(HttpError::new(400, "an empty request target"));
    }
    match version {
        "HTTP/1.1" | "HTTP/1.0" => {}
        _ if version.starts_with("HTTP/") => {
            // alloc: error path, another HTTP version.
            let said = format!("{version}; this speaks HTTP/1.1");
            return Err(HttpError::new(505, said));
        }
        _ => return Err(HttpError::new(400, "a request line whose version is not HTTP/1.x")),
    }
    for line in header_lines(lines) {
        header_line(line)?;
    }
    Ok(Head { method, path, lines })
}

/// The header lines of a head's text: none when it has only its start line.
fn header_lines(lines: &str) -> impl Iterator<Item = &str> {
    lines.split("\r\n").filter(|line| !line.is_empty())
}

fn response_head(head: &[u8]) -> Result<(u16, Vec<(String, String)>), HttpError> {
    let (start, lines) = text(head)?;
    let bad = || HttpError::new(502, "a status line that is not HTTP/1.x STATUS REASON");
    let (version, rest) = start.split_once(' ').ok_or_else(bad)?;
    if !matches!(version, "HTTP/1.1" | "HTTP/1.0") {
        return Err(bad());
    }
    let status = rest.split_once(' ').map_or(rest, |(status, _)| status);
    if status.len() != 3 || !status.bytes().all(|it| it.is_ascii_digit()) {
        return Err(bad());
    }
    let status = status.parse().map_err(|_| bad())?;
    let headers = header_lines(lines)
        // alloc: the head `ResponseParser::head` hands out, once per response, a client's.
        .map(|line| header_line(line).map(|(name, value)| (name.to_owned(), value.to_owned())))
        .collect::<Result<_, _>>()?;
    Ok((status, headers))
}

fn header_line(line: &str) -> Result<(&str, &str), HttpError> {
    if line.starts_with([' ', '\t']) {
        return Err(HttpError::new(400, "a folded header line"));
    }
    let Some((name, value)) = line.split_once(':') else {
        return Err(HttpError::new(400, "a header line without a colon"));
    };
    if name.is_empty() || !name.bytes().all(is_tchar) {
        return Err(HttpError::new(400, "a header name that is not a token"));
    }
    Ok((name, value.trim_matches([' ', '\t'])))
}

fn is_tchar(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

enum Framing {
    Length(usize),
    Chunked,
    None,
}

fn framing<'a>(
    headers: impl IntoIterator<Item = (&'a str, &'a str)>,
    most_body: usize,
) -> Result<Framing, HttpError> {
    let mut length = None;
    let mut chunked = false;
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("content-length") {
            if length.is_some() {
                return Err(HttpError::new(400, "more than one Content-Length"));
            }
            let number = !value.is_empty() && value.bytes().all(|it| it.is_ascii_digit());
            let len = value.parse::<u64>().ok().filter(|_| number);
            let len = len.ok_or_else(|| HttpError::new(400, "a Content-Length that is not a number"))?;
            length = Some(len);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            if chunked {
                return Err(HttpError::new(400, "more than one Transfer-Encoding"));
            }
            if !value.eq_ignore_ascii_case("chunked") {
                // alloc: error path, another transfer coding.
                let said = format!("the transfer coding {value:?}; only chunked is read");
                return Err(HttpError::new(501, said));
            }
            chunked = true;
        }
    }
    match (length, chunked) {
        (Some(_), true) => {
            Err(HttpError::new(400, "both Content-Length and Transfer-Encoding: chunked"))
        }
        (Some(len), false) => match usize::try_from(len).ok().filter(|&len| len <= most_body) {
            Some(len) => Ok(Framing::Length(len)),
            None => {
                // alloc: error path, a body past the bound.
                let said = format!("a body of {len} bytes, more than {most_body}");
                Err(HttpError::new(413, said))
            }
        },
        (None, true) => Ok(Framing::Chunked),
        (None, false) => Ok(Framing::None),
    }
}

/// Where a chunked body's decoding is.
#[derive(Debug)]
enum Chunks {
    Size,
    Data(usize),
    /// The CRLF after a chunk's data.
    DataEnd,
    /// Trailer lines read so far, after the last chunk.
    Trailers(usize),
    Done,
}

impl Chunks {
    /// Decodes what of `bytes[*at..]` it can into `out`, moving `at` past it; true once the
    /// last chunk and its trailers are read. `decoded` counts the body's bytes, which may be
    /// at most `most`. Trailers are read and dropped.
    fn decode(
        &mut self,
        bytes: &[u8],
        at: &mut usize,
        out: &mut Vec<u8>,
        decoded: &mut usize,
        most: usize,
    ) -> Result<bool, HttpError> {
        loop {
            match *self {
                Chunks::Size => {
                    let Some((line, next)) = line_at(bytes, *at)? else { return Ok(false) };
                    let size = chunk_size(line)?;
                    *at = next;
                    *self = match size {
                        0 => Chunks::Trailers(0),
                        _ => match usize::try_from(size)
                            .ok()
                            .filter(|&size| size <= most.saturating_sub(*decoded))
                        {
                            Some(size) => Chunks::Data(size),
                            None => {
                                // alloc: error path, a chunked body past the bound.
                                let said = format!("a chunked body of more than {most} bytes");
                                return Err(HttpError::new(413, said));
                            }
                        },
                    };
                }
                Chunks::Data(left) => {
                    let here = (bytes.len() - *at).min(left);
                    if here == 0 {
                        return Ok(false);
                    }
                    out.extend_from_slice(&bytes[*at..*at + here]);
                    *at += here;
                    *decoded += here;
                    *self = match left - here {
                        0 => Chunks::DataEnd,
                        left => Chunks::Data(left),
                    };
                }
                Chunks::DataEnd => match bytes.get(*at..*at + 2) {
                    None => return Ok(false),
                    Some(b"\r\n") => {
                        *at += 2;
                        *self = Chunks::Size;
                    }
                    Some(_) => return Err(HttpError::new(400, "a chunk not ended by CRLF")),
                },
                Chunks::Trailers(count) => {
                    let Some((line, next)) = line_at(bytes, *at)? else { return Ok(false) };
                    *at = next;
                    if line.is_empty() {
                        *self = Chunks::Done;
                        return Ok(true);
                    }
                    if count == MOST_HEADERS {
                        return Err(HttpError::new(431, "more than 100 trailers"));
                    }
                    *self = Chunks::Trailers(count + 1);
                }
                Chunks::Done => return Ok(true),
            }
        }
    }
}

/// The line starting at `from`, without its CRLF, and where the next starts; `None` until
/// its end is here.
fn line_at(bytes: &[u8], from: usize) -> Result<Option<(&[u8], usize)>, HttpError> {
    let rest = &bytes[from..];
    match rest.iter().take(LONGEST_LINE + 2).position(|&it| it == b'\n') {
        Some(end) => match rest[..end].strip_suffix(b"\r") {
            Some(line) => Ok(Some((line, from + end + 1))),
            None => Err(HttpError::new(400, "a chunk line ended by LF without CR")),
        },
        None if rest.len() > LONGEST_LINE + 1 => {
            Err(HttpError::new(431, "a chunk line longer than 8 KiB"))
        }
        None => Ok(None),
    }
}

/// A chunk's size line: hexadecimal digits, then any extensions, which are dropped.
fn chunk_size(line: &[u8]) -> Result<u64, HttpError> {
    let digits = line.split(|&it| it == b';').next().unwrap_or_default();
    let digits = digits.trim_ascii_end();
    if digits.is_empty() || digits.len() > 16 || !digits.iter().all(u8::is_ascii_hexdigit) {
        return Err(HttpError::new(400, "a bad chunk size"));
    }
    let digit = |it: u8| u64::from((it as char).to_digit(16).unwrap_or_default());
    Ok(digits.iter().fold(0, |size, &it| (size << 4) | digit(it)))
}

fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        101 => "Switching Protocols",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Content",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        505 => "HTTP Version Not Supported",
        _ => "",
    }
}

/// A head onto the end of `out`: its start line, `first`, `headers`, `last`, the blank line.
fn head_into(
    out: &mut Vec<u8>,
    start: &[&str],
    first: Option<(&str, &str)>,
    headers: &[(&str, &str)],
    last: (&str, &str),
) {
    for part in start {
        out.extend_from_slice(part.as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    for (name, value) in first.iter().chain(headers).chain([&last]) {
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
}

/// A response's start line, `HTTP/1.1 200 OK`, as the parts [`head_into`] writes.
fn status_line(status: u16, digits: &mut [u8; 20]) -> [&str; 4] {
    ["HTTP/1.1 ", decimal(u64::from(status), digits), " ", reason(status)]
}

/// The most bytes of a head that are not its headers: the longest start line, the length or
/// coding header, CRLFs.
const HEAD_FRAME: usize = 96;

/// What `headers` add to a head.
fn headers_len(headers: &[(&str, &str)]) -> usize {
    headers.iter().map(|(name, value)| name.len() + value.len() + 4).sum()
}

/// A whole response, with its Content-Length, in one allocation of its size.
pub fn response(status: u16, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    // alloc: the response handed out; `response_into` writes onto a buffer the caller keeps.
    let mut out = Vec::with_capacity(HEAD_FRAME + headers_len(headers) + body.len());
    response_into(&mut out, status, headers, body);
    out
}

/// [`response`] onto the end of `out`.
pub fn response_into(out: &mut Vec<u8>, status: u16, headers: &[(&str, &str)], body: &[u8]) {
    let (mut digits, mut len) = ([0; 20], [0; 20]);
    let start = status_line(status, &mut digits);
    let length = ("Content-Length", decimal(body.len() as u64, &mut len));
    head_into(out, &start, None, headers, length);
    out.extend_from_slice(body);
}

/// The head of a response whose body comes in [`chunk`]s, ended by [`last_chunk`].
pub fn chunked(status: u16, headers: &[(&str, &str)]) -> Vec<u8> {
    // alloc: the head handed out; `chunked_into` writes onto a buffer the caller keeps.
    let mut out = Vec::with_capacity(HEAD_FRAME + headers_len(headers));
    chunked_into(&mut out, status, headers);
    out
}

/// [`chunked`] onto the end of `out`.
pub fn chunked_into(out: &mut Vec<u8>, status: u16, headers: &[(&str, &str)]) {
    let mut digits = [0; 20];
    let start = status_line(status, &mut digits);
    head_into(out, &start, None, headers, ("Transfer-Encoding", "chunked"));
}

/// `bytes` as one chunk; no bytes give no output, never the last chunk.
pub fn chunk(bytes: &[u8]) -> Vec<u8> {
    // alloc: the chunk handed out (sized by `chunk_into`, once); it writes onto a kept buffer.
    let mut out = Vec::new();
    chunk_into(&mut out, bytes);
    out
}

/// [`chunk`] onto the end of `out`.
pub fn chunk_into(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = bytes.len();
    if len == 0 {
        return;
    }
    let digits = (usize::BITS - len.leading_zeros()).div_ceil(4);
    out.reserve(digits as usize + len + 4);
    for digit in (0..digits).rev() {
        out.push(b"0123456789abcdef"[(len >> (digit * 4)) & 0xf]);
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(bytes);
    out.extend_from_slice(b"\r\n");
}

/// The chunk that ends a chunked body, with no trailers.
pub const LAST_CHUNK: &[u8] = b"0\r\n\r\n";

pub fn last_chunk() -> Vec<u8> {
    // alloc: the chunk handed out; `LAST_CHUNK` is the same bytes, unallocated.
    LAST_CHUNK.to_vec()
}

/// A whole request to `host`, with its Content-Length, in one allocation of its size.
pub fn request(
    method: &str,
    host: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Vec<u8> {
    let named = method.len() + path.len() + host.len() + headers_len(headers);
    // alloc: the request handed out, once per request, a client's.
    let mut out = Vec::with_capacity(HEAD_FRAME + named + body.len());
    request_head_into(&mut out, method, host, path, headers, body.len());
    out.extend_from_slice(body);
    out
}

/// The head of [`request`] of a body of `len` bytes onto the end of `out`, for the body to be
/// written after it from where it lies.
pub(crate) fn request_head_into(
    out: &mut Vec<u8>,
    method: &str,
    host: &str,
    path: &str,
    headers: &[(&str, &str)],
    len: usize,
) {
    let mut digits = [0; 20];
    let start = [method, " ", path, " HTTP/1.1"];
    let length = ("Content-Length", decimal(len as u64, &mut digits));
    head_into(out, &start, Some(("Host", host)), headers, length);
}

/// Blocking: one whole response from `from`. What `from` gives past the response's end is
/// dropped, so the peer must send nothing more until asked (one request per connection).
pub fn read_response(from: &mut impl io::Read, most_body: usize) -> io::Result<HttpResponse> {
    let mut parser = ResponseParser::new(most_body);
    // alloc: the body handed out, once per response, a client's.
    let mut body = Vec::new();
    let mut buffer = [0; READ];
    while !parser.finished() {
        let read = match from.read(&mut buffer) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        parser.push(&buffer[..read]).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        // The parser's first piece is taken whole, not copied.
        match body.is_empty() {
            true => body = parser.take_body(),
            false => body.extend_from_slice(&parser.take_body()),
        }
        if read == 0 && !parser.finished() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
    }
    let Some((status, headers)) = parser.take_head() else {
        return Err(io::ErrorKind::UnexpectedEof.into());
    };
    Ok(HttpResponse { status, headers, body })
}

#[cfg(test)]
mod tests;
