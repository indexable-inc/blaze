//! The little of HTTP/1.1 the viewer speaks to a page, as functions over bytes: a request
//! parser with a limit on everything, the response heads (CORS for the one origin, Private
//! Network Access for a page on https reaching loopback), and server-sent events.
//!
//! **One request a connection.** Every response says `Connection: close`; bytes after the first
//! request (pipelining) are refused. A stream response is delimited by the close.
//!
//! **Limits** (each refused with its status before anything is sized by it): a head of at most
//! [`MOST_HEAD`] bytes (431), a line of at most [`MOST_LINE`] (431), at most [`MOST_HEADERS`]
//! headers (431), a body of at most [`MOST_BODY`] by `Content-Length` (413); `Transfer-Encoding`,
//! a folded line, a byte outside visible ASCII, a repeated `Host`, `Origin`, `Authorization` or
//! `Content-Length`, a method other than GET, POST and OPTIONS, or a version other than
//! HTTP/1.1 are refused (400, 405, 505). Header names are matched without case.

#![blaze::sans_io]

use core::alloc::Allocator;

use blaze_own::Vec;

pub const MOST_HEAD: usize = 8 << 10;
pub const MOST_LINE: usize = 2 << 10;
pub const MOST_HEADERS: usize = 32;
pub const MOST_BODY: usize = 16 << 10;
/// What a page's connection may send in all.
pub const MOST_REQUEST: usize = MOST_HEAD + 4 + MOST_BODY;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method {
    Get,
    Post,
    Options,
}

/// The parts of a request the viewer reads; every other header is passed over.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Request<'a> {
    pub method: Method,
    pub path: &'a str,
    pub query: &'a str,
    pub host: Option<&'a str>,
    pub origin: Option<&'a str>,
    pub authorization: Option<&'a str>,
    pub last_event_id: Option<&'a str>,
    /// `Access-Control-Request-Private-Network: true`: a preflight from a public page to
    /// loopback.
    pub private_network: bool,
    /// `Access-Control-Request-Method`: present on a CORS preflight.
    pub preflight: Option<&'a str>,
    pub body: &'a [u8],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Parsed<'a> {
    /// The request is not whole yet.
    More,
    Done(Request<'a>),
}

/// A response status.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Ok,
    Accepted,
    NoContent,
    BadRequest,
    Unauthorized,
    Forbidden,
    NotFound,
    MethodNotAllowed,
    Conflict,
    PayloadTooLarge,
    Misdirected,
    HeadersTooLarge,
    Unavailable,
    VersionNotSupported,
}

impl Status {
    pub const fn code(self) -> u16 {
        match self {
            Status::Ok => 200,
            Status::Accepted => 202,
            Status::NoContent => 204,
            Status::BadRequest => 400,
            Status::Unauthorized => 401,
            Status::Forbidden => 403,
            Status::NotFound => 404,
            Status::MethodNotAllowed => 405,
            Status::Conflict => 409,
            Status::PayloadTooLarge => 413,
            Status::Misdirected => 421,
            Status::HeadersTooLarge => 431,
            Status::Unavailable => 503,
            Status::VersionNotSupported => 505,
        }
    }

    pub const fn reason(self) -> &'static str {
        match self {
            Status::Ok => "OK",
            Status::Accepted => "Accepted",
            Status::NoContent => "No Content",
            Status::BadRequest => "Bad Request",
            Status::Unauthorized => "Unauthorized",
            Status::Forbidden => "Forbidden",
            Status::NotFound => "Not Found",
            Status::MethodNotAllowed => "Method Not Allowed",
            Status::Conflict => "Conflict",
            Status::PayloadTooLarge => "Payload Too Large",
            Status::Misdirected => "Misdirected Request",
            Status::HeadersTooLarge => "Request Header Fields Too Large",
            Status::Unavailable => "Service Unavailable",
            Status::VersionNotSupported => "HTTP Version Not Supported",
        }
    }
}

fn find(input: &[u8], needle: &[u8]) -> Option<usize> {
    input.windows(needle.len()).position(|window| window == needle)
}

fn visible(byte: u8) -> bool {
    (0x20..0x7f).contains(&byte) || byte == b'\t'
}

fn token(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// A header that may come once: a second one is ambiguous, and refused.
fn once<'a>(slot: &mut Option<&'a str>, value: &'a str) -> Result<(), Status> {
    match slot.replace(value) {
        None => Ok(()),
        Some(_) => Err(Status::BadRequest),
    }
}

/// The request at the start of `input`, or what it breaks.
pub fn parse(input: &[u8]) -> Result<Parsed<'_>, Status> {
    let scan = &input[..input.len().min(MOST_HEAD + 4)];
    let Some(end) = find(scan, b"\r\n\r\n") else {
        return match input.len() >= MOST_HEAD {
            true => Err(Status::HeadersTooLarge),
            false => Ok(Parsed::More),
        };
    };
    let head = &input[..end];
    if !head.iter().all(|b| visible(*b) || *b == b'\r' || *b == b'\n') {
        return Err(Status::BadRequest);
    }
    let Ok(head) = core::str::from_utf8(head) else {
        return Err(Status::BadRequest);
    };
    let mut lines = head.split("\r\n");
    let line = lines.next().unwrap_or("");
    if line.len() > MOST_LINE {
        return Err(Status::HeadersTooLarge);
    }
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) = (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Status::BadRequest);
    };
    if version != "HTTP/1.1" {
        return Err(match version.starts_with("HTTP/") {
            true => Status::VersionNotSupported,
            false => Status::BadRequest,
        });
    }
    let method = match method {
        "GET" => Method::Get,
        "POST" => Method::Post,
        "OPTIONS" => Method::Options,
        name if token(name) => return Err(Status::MethodNotAllowed),
        _ => return Err(Status::BadRequest),
    };
    if !target.starts_with('/') {
        return Err(Status::BadRequest);
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut request = Request {
        method,
        path,
        query,
        host: None,
        origin: None,
        authorization: None,
        last_event_id: None,
        private_network: false,
        preflight: None,
        body: &[],
    };
    let mut length: Option<usize> = None;
    let mut count = 0;
    for line in lines {
        count += 1;
        if count > MOST_HEADERS || line.len() > MOST_LINE {
            return Err(Status::HeadersTooLarge);
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            // A folded line (obsolete; RFC 9112 5.2).
            return Err(Status::BadRequest);
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(Status::BadRequest);
        };
        if !token(name) {
            return Err(Status::BadRequest);
        }
        let value = value.trim_matches(|c| c == ' ' || c == '\t');
        if name.eq_ignore_ascii_case("host") {
            once(&mut request.host, value)?;
        } else if name.eq_ignore_ascii_case("origin") {
            once(&mut request.origin, value)?;
        } else if name.eq_ignore_ascii_case("authorization") {
            once(&mut request.authorization, value)?;
        } else if name.eq_ignore_ascii_case("last-event-id") {
            once(&mut request.last_event_id, value)?;
        } else if name.eq_ignore_ascii_case("access-control-request-method") {
            once(&mut request.preflight, value)?;
        } else if name.eq_ignore_ascii_case("access-control-request-private-network") {
            request.private_network = value.eq_ignore_ascii_case("true");
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(Status::BadRequest);
        } else if name.eq_ignore_ascii_case("content-length") {
            if length.is_some() {
                return Err(Status::BadRequest);
            }
            let Some(n) = decimal(value) else {
                return Err(Status::BadRequest);
            };
            if n > MOST_BODY as u64 {
                return Err(Status::PayloadTooLarge);
            }
            length = Some(n as usize);
        }
    }
    let length = length.unwrap_or(0);
    if length > 0 && method != Method::Post {
        return Err(Status::BadRequest);
    }
    let body_at = end + 4;
    if input.len() < body_at + length {
        return Ok(Parsed::More);
    }
    if input.len() > body_at + length {
        return Err(Status::BadRequest);
    }
    request.body = &input[body_at..];
    Ok(Parsed::Done(request))
}

/// A decimal of at most 19 digits (no sign, no leading `+`).
pub fn decimal(text: &str) -> Option<u64> {
    if text.is_empty() || text.len() > 19 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(text.bytes().fold(0u64, |n, b| n * 10 + u64::from(b - b'0')))
}

/// Appends `n` in decimal.
pub fn write_decimal<A: Allocator>(out: &mut Vec<u8, A>, mut n: u64) {
    let mut digits = [0u8; 20];
    let mut at = digits.len();
    loop {
        at -= 1;
        digits[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(&digits[at..]);
}

/// Whether `host` names the viewer on loopback: `127.0.0.1`, `localhost` or `[::1]`, with the
/// viewer's port. A request through any other name is a DNS-rebinding page and is refused.
pub fn loopback_host(host: &str, port: u16) -> bool {
    let Some((name, given)) = host.rsplit_once(':') else {
        return false;
    };
    matches!(name, "127.0.0.1" | "localhost" | "[::1]") && decimal(given) == Some(u64::from(port))
}

/// The cross-origin headers of a response.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cors<'a> {
    /// The one origin let in, sent only to a request from it.
    pub origin: Option<&'a str>,
    /// Answer a Private Network Access preflight.
    pub private_network: bool,
    /// A preflight: name the methods and headers allowed.
    pub preflight: bool,
}

impl Cors<'_> {
    pub const NONE: Cors<'static> = Cors { origin: None, private_network: false, preflight: false };
}

/// What a response carries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Body<'a> {
    Empty,
    Cbor(&'a [u8]),
    Script(&'a [u8]),
    /// Server-sent events follow until the connection closes.
    Stream,
}

fn line<A: Allocator>(out: &mut Vec<u8, A>, parts: &[&[u8]]) {
    for part in parts {
        out.extend_from_slice(part);
    }
    out.extend_from_slice(b"\r\n");
}

/// Appends a whole response (or, for a stream, its head). The headers are always in this
/// order, so equal answers are equal bytes.
pub fn respond<A: Allocator>(out: &mut Vec<u8, A>, status: Status, cors: Cors<'_>, body: Body<'_>) {
    out.extend_from_slice(b"HTTP/1.1 ");
    write_decimal(out, u64::from(status.code()));
    out.push(b' ');
    line(out, &[status.reason().as_bytes()]);
    let (kind, bytes): (Option<&[u8]>, &[u8]) = match body {
        Body::Empty => (None, &[]),
        Body::Cbor(bytes) => (Some(b"application/cbor"), bytes),
        Body::Script(bytes) => (Some(b"text/javascript; charset=utf-8"), bytes),
        Body::Stream => (Some(b"text/event-stream"), &[]),
    };
    if let Some(kind) = kind {
        line(out, &[b"Content-Type: ", kind]);
    }
    if body != Body::Stream {
        out.extend_from_slice(b"Content-Length: ");
        write_decimal(out, bytes.len() as u64);
        out.extend_from_slice(b"\r\n");
    }
    line(out, &[b"Cache-Control: no-store"]);
    line(out, &[b"X-Content-Type-Options: nosniff"]);
    if let Body::Script(_) = body {
        // The ix.dev page loads the script with a <script> tag from another origin.
        line(out, &[b"Cross-Origin-Resource-Policy: cross-origin"]);
    }
    if let Some(origin) = cors.origin {
        line(out, &[b"Access-Control-Allow-Origin: ", origin.as_bytes()]);
        line(out, &[b"Vary: Origin"]);
    }
    if cors.preflight {
        line(out, &[b"Access-Control-Allow-Methods: GET, POST"]);
        line(out, &[b"Access-Control-Allow-Headers: authorization, content-type, last-event-id"]);
        line(out, &[b"Access-Control-Max-Age: 600"]);
    }
    if cors.private_network {
        line(out, &[b"Access-Control-Allow-Private-Network: true"]);
    }
    line(out, &[b"Connection: close"]);
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(bytes);
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Appends `bytes` in standard base64 with padding: what an event's `data:` line carries.
pub fn base64<A: Allocator>(out: &mut Vec<u8, A>, bytes: &[u8]) {
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(BASE64[(n >> 18) as usize & 63]);
        out.push(BASE64[(n >> 12) as usize & 63]);
        out.push(match chunk.len() > 1 {
            true => BASE64[(n >> 6) as usize & 63],
            false => b'=',
        });
        out.push(match chunk.len() > 2 {
            true => BASE64[n as usize & 63],
            false => b'=',
        });
    }
}

/// The bytes base64 of `len` bytes takes.
pub const fn base64_len(len: usize) -> usize {
    len.div_ceil(3) * 4
}

fn url_digit(c: u8) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some(u32::from(c - b'A')),
        b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
        b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
        b'-' => Some(62),
        b'_' => Some(63),
        _ => None,
    }
}

/// A 32-byte key in base64url without padding (43 characters), as a page sends it.
pub fn key_from_base64url(text: &str) -> Option<[u8; 32]> {
    let text = text.as_bytes();
    if text.len() != 43 {
        return None;
    }
    let mut out = [0u8; 32];
    let mut bits = 0u32;
    let mut held = 0u32;
    let mut at = 0;
    for c in text {
        bits = (bits << 6) | url_digit(*c)?;
        held += 6;
        if held >= 8 {
            held -= 8;
            out[at] = (bits >> held) as u8;
            at += 1;
            bits &= (1 << held) - 1;
        }
    }
    // 43 * 6 = 258 bits: 32 bytes and two bits, which must be zero (the canonical form).
    (at == 32 && bits == 0).then_some(out)
}

/// The head of an event stream's response.
pub fn sse_head<A: Allocator>(out: &mut Vec<u8, A>, origin: &str) {
    respond(out, Status::Ok, Cors { origin: Some(origin), private_network: false, preflight: false }, Body::Stream);
}

/// One server-sent event: `id: <epoch>-<seq>`, its name, and `payload` as base64 on one `data:`
/// line (base64 has no newline, so the payload cannot end the event early).
pub fn sse_event<A: Allocator>(out: &mut Vec<u8, A>, epoch: u64, seq: u64, name: &str, payload: &[u8]) {
    out.extend_from_slice(b"id: ");
    write_decimal(out, epoch);
    out.push(b'-');
    write_decimal(out, seq);
    out.extend_from_slice(b"\nevent: ");
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(b"\ndata: ");
    base64(out, payload);
    out.extend_from_slice(b"\n\n");
}

/// The bytes [`sse_event`] appends at most for a payload of `len` bytes and a name of `name`
/// bytes: two 20-digit numbers.
pub const fn sse_event_most(name: usize, len: usize) -> usize {
    4 + 20 + 1 + 20 + 8 + name + 7 + base64_len(len) + 2
}

/// A comment line: keeps a quiet stream open through proxies and tells the page it lives.
pub fn sse_comment<A: Allocator>(out: &mut Vec<u8, A>) {
    out.extend_from_slice(b": k\n\n");
}

/// The `<epoch>-<seq>` of a `Last-Event-ID`.
pub fn event_id(text: &str) -> Option<(u64, u64)> {
    let (epoch, seq) = text.split_once('-')?;
    Some((decimal(epoch)?, decimal(seq)?))
}

/// Bytes in lowercase hex, two characters a byte, from `text`.
pub fn hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    let text = text.as_bytes();
    if text.len() != 2 * N {
        return None;
    }
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = (digit(text[2 * i])? << 4) | digit(text[2 * i + 1])?;
    }
    Some(out)
}
