//! The request writer: one HTTP/1.1 request head, its fields in a fixed order (request line,
//! `Host`, `Range`, the caller's headers as given, `Content-Length`, `Connection`), so equal
//! requests are equal bytes. Every field is checked before a byte is written: a CR or LF in a
//! value would let a caller's data start a header of its own.

#![blaze::sans_io]

use crate::{
    error::{Error, Why},
    out::Out,
};

/// Bytes `first..=last` of a resource, or from `first` to its end.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Range {
    pub first: u64,
    pub last: Option<u64>,
}

/// One request.
#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
    pub method: &'a str,
    /// The path and query: `/index/se/rd/serde`; or, to an HTTP proxy forwarding a plain
    /// `http` request, the absolute form `http://host[:port]/path?query` (RFC 9112 section
    /// 3.2.2).
    pub target: &'a str,
    /// The `Host` header: the host, and the port when it is not the scheme's own.
    pub host: &'a str,
    pub range: Option<Range>,
    /// More headers, written in this order.
    pub headers: &'a [(&'a str, &'a str)],
    /// A body's length, sent as `Content-Length` (the body follows the head, the caller's).
    pub body_len: Option<u64>,
    /// `Connection: close`: one request on the connection, its body ended by the close.
    pub close: bool,
}

/// The headers the writer sets itself, which a caller's list may not hold.
const OWN: [&str; 5] = ["host", "content-length", "transfer-encoding", "range", "connection"];

/// Why a caller's header can't be written as given.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HeaderFault {
    /// Its name is no token.
    NotToken,
    /// Its name is one the writer sets itself (`Host`, `Content-Length`, `Transfer-Encoding`,
    /// `Range`, `Connection`): it frames the body, keeps the connection, or is typed.
    Owned,
    /// Its value has a CR, LF, NUL or other control byte, which would end the head early.
    ControlByte,
}

/// Whether `name: value` can be one of a [`Request`]'s headers: the writer's own check, for a
/// client that refuses a request before it is sent.
pub fn check_header(name: &str, value: &str) -> Result<(), HeaderFault> {
    if !is_token(name) {
        return Err(HeaderFault::NotToken);
    }
    if OWN.iter().any(|own| own.eq_ignore_ascii_case(name)) {
        return Err(HeaderFault::Owned);
    }
    if value.bytes().any(|b| (b < b' ' && b != b'\t') || b == 0x7f) {
        return Err(HeaderFault::ControlByte);
    }
    Ok(())
}

/// `request`'s head, appended to `out`.
pub fn write_request<O: Out + ?Sized>(request: &Request<'_>, out: &mut O) -> Result<(), Error> {
    let fail = |why| Error { at: 0, why };
    if !is_token(request.method) {
        return Err(fail(Why::Method));
    }
    let t = request.target.as_bytes();
    if !(t.first() == Some(&b'/') || t.starts_with(b"http://")) || t.iter().any(|&b| b <= b' ' || b == 0x7f) {
        return Err(fail(Why::Target));
    }
    let h = request.host.as_bytes();
    if h.is_empty() || !h.iter().all(|&b| b.is_ascii_alphanumeric() || b"-.:[]_".contains(&b)) {
        return Err(fail(Why::Host));
    }
    for (i, &(name, value)) in request.headers.iter().enumerate() {
        let index = i.min(usize::from(u16::MAX)) as u16;
        match check_header(name, value) {
            Ok(()) => {}
            Err(HeaderFault::NotToken | HeaderFault::Owned) => return Err(fail(Why::HeaderName { index })),
            Err(HeaderFault::ControlByte) => return Err(fail(Why::HeaderValue { index })),
        }
    }
    if let Some(Range { first, last: Some(last) }) = request.range
        && last < first
    {
        return Err(fail(Why::RangeOrder { first, last }));
    }
    let mut len = request.method.len() + request.target.len() + request.host.len() + 64;
    for (name, value) in request.headers {
        len += name.len() + value.len() + 4;
    }
    out.try_reserve_more(len).map_err(|e| fail(Why::Alloc(e)))?;
    // Within the room just reserved: a failure here is the allocator's after all, and the head
    // is cut short; the caller sees the error of the first.
    let mut failed = None;
    let mut put = |bytes: &[u8]| {
        if failed.is_none()
            && let Err(e) = out.try_put(bytes)
        {
            failed = Some(e);
        }
    };
    put(request.method.as_bytes());
    put(b" ");
    put(t);
    put(b" HTTP/1.1\r\nHost: ");
    put(h);
    put(b"\r\n");
    let mut digits = [0u8; 20];
    if let Some(range) = request.range {
        put(b"Range: bytes=");
        put(decimal(range.first, &mut digits));
        put(b"-");
        if let Some(last) = range.last {
            put(decimal(last, &mut digits));
        }
        put(b"\r\n");
    }
    for (name, value) in request.headers {
        put(name.as_bytes());
        put(b": ");
        put(value.as_bytes());
        put(b"\r\n");
    }
    if let Some(n) = request.body_len {
        put(b"Content-Length: ");
        put(decimal(n, &mut digits));
        put(b"\r\n");
    }
    if request.close {
        put(b"Connection: close\r\n");
    }
    put(b"\r\n");
    match failed {
        None => Ok(()),
        Some(e) => Err(fail(Why::Alloc(e))),
    }
}

/// A token (RFC 9110 section 5.6.2): one or more of the bytes a field name or method may hold.
pub(crate) fn is_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

pub(crate) fn decimal(mut value: u64, buf: &mut [u8; 20]) -> &[u8] {
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            return &buf[at..];
        }
    }
}
