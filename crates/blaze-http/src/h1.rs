//! HTTP/1.1 on bytes: a request's head written into a buffer, an answer's head parsed with
//! `httparse` once it is whole, and a body read as its head frames it (RFC 9112 section 6.3):
//! `Content-Length`, `chunked` (with its extensions and trailers read and dropped), or until the
//! connection closes. Nothing here does IO or keeps a connection; [`crate::Client`] does both.

#![blaze::sans_io]

use std::io::Write as _;

/// The most bytes an answer's head may be, status line and headers; one longer is refused.
pub const HEAD_MOST: usize = 64 << 10;

/// The most headers an answer's head may have (hyper's default); one with more is refused.
pub const HEADERS_MOST: usize = 100;

/// The most bytes of one chunk-size line, extensions included; one longer is refused.
const CHUNK_LINE_MOST: usize = 4 << 10;

/// The request line and headers of a request for `target` (path and query) of `host` (its
/// `Host`, port included when it is not the scheme's), its `headers` after the client's own,
/// and the `Content-Length` of a body of `body` bytes where one is said: always for a body,
/// and for an empty one of a method that carries one (`POST`, `PUT`, `PATCH`). Through an HTTP
/// proxy, `target` is in absolute form and `proxy_authorization` the proxy's credentials, if
/// it has any.
///
/// A caller's `Host` replaces the client's. Header names are written as given, the client's in
/// lower case, as hyper wrote them.
pub fn write_request(
    out: &mut Vec<u8>,
    method: &http::Method,
    target: &str,
    host: &str,
    user_agent: &str,
    gzip: bool,
    headers: &[(String, String)],
    body: usize,
    proxy_authorization: Option<&str>,
) {
    // Writes into a vector, which cannot fail.
    _ = write!(out, "{method} {target} HTTP/1.1\r\n");
    if !headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("host")) {
        _ = write!(out, "host: {host}\r\n");
    }
    if let Some(credentials) = proxy_authorization {
        _ = write!(out, "proxy-authorization: {credentials}\r\n");
    }
    _ = write!(out, "user-agent: {user_agent}\r\n");
    if gzip {
        out.extend_from_slice(b"accept-encoding: gzip\r\n");
    }
    for (name, value) in headers {
        _ = write!(out, "{name}: {value}\r\n");
    }
    let carries = matches!(*method, http::Method::POST | http::Method::PUT | http::Method::PATCH);
    if body > 0 || carries {
        _ = write!(out, "content-length: {body}\r\n");
    }
    out.extend_from_slice(b"\r\n");
}

/// Why a caller's header can't be sent as given: a name that is not a token, a value with a
/// line break or a NUL in it (which would end the head early), or a header whose say is the
/// client's (it frames the body and keeps the connection itself).
pub fn refuse_header(name: &str, value: &str) -> Option<String> {
    let token = |b: u8| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b);
    if name.is_empty() || !name.bytes().all(token) {
        // alloc: an error's message, made only when it fails.
        return Some(format!("`{name}` is no header name"));
    }
    if value.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0)) {
        // alloc: an error's message, made only when it fails.
        return Some(format!("the value of `{name}` has a line break or a NUL in it"));
    }
    for owned in ["content-length", "transfer-encoding", "connection"] {
        if name.eq_ignore_ascii_case(owned) {
            // alloc: an error's message, made only when it fails.
            return Some(format!(
                "`{name}` is the client's to say: it frames the body and keeps the connection itself"
            ));
        }
    }
    None
}

/// An answer's head, once whole.
#[derive(Debug)]
pub struct Head {
    /// How many bytes it took: its body starts there.
    pub len: usize,
    pub status: u16,
    /// Every header whose value is visible ASCII (as hyper's `to_str` takes them), its name in
    /// lower case, in the order sent.
    pub headers: Vec<(String, String)>,
    /// How its body is framed.
    pub body: Body,
    /// Whether the connection can take another request once the body is read: HTTP/1.1 with
    /// no `Connection: close` (HTTP/1.0 with `Connection: keep-alive`), a body that is not
    /// framed by the close, and not both `Transfer-Encoding` and `Content-Length`.
    pub reusable: bool,
    /// Its `Location`, as sent.
    pub location: Option<String>,
    /// Whether its `Content-Encoding` is `gzip`.
    pub gzip: bool,
    /// Its `Content-Length`, where its body is framed by one.
    pub length: Option<u64>,
}

/// The head `buffer` starts with, for a request whose method is `HEAD` when `head_request`
/// (its answer has no body whatever it says): `None` until it is whole. A head of an
/// informational status (1xx) is given like any other, with an empty body; its asker reads the
/// next. An error says what is wrong with it.
pub fn parse_head(buffer: &[u8], head_request: bool) -> Result<Option<Head>, String> {
    let window = &buffer[..buffer.len().min(HEAD_MOST)];
    let mut slots = [httparse::EMPTY_HEADER; HEADERS_MOST];
    let mut response = httparse::Response::new(&mut slots);
    let len = match response.parse(window) {
        Ok(httparse::Status::Complete(len)) => len,
        Ok(httparse::Status::Partial) if buffer.len() >= HEAD_MOST => {
            // alloc: an error's message, made only when it fails.
            return Err(format!("a head longer than the {HEAD_MOST} bytes allowed"));
        }
        Ok(httparse::Status::Partial) => return Ok(None),
        Err(httparse::Error::TooManyHeaders) => {
            // alloc: an error's message, made only when it fails.
            return Err(format!("a head with more than the {HEADERS_MOST} headers allowed"));
        }
        // alloc: an error's message, made only when it fails.
        Err(e) => return Err(format!("not an HTTP/1 answer: {e}")),
    };
    let one_one = response.version == Some(1);
    // alloc: an error's message, made only when it fails.
    let status = response.code.ok_or_else(|| "a status line with no status".to_owned())?;
    let mut length: Option<u64> = None;
    let mut encoded = false;
    let mut chunked = false;
    let mut close = false;
    let mut keep_alive = false;
    let mut location = None;
    let mut gzip = false;
    // alloc: one per answer, its headers handed to the asker.
    let mut headers = Vec::with_capacity(response.headers.len());
    for header in response.headers.iter() {
        let raw = header.value;
        let text = raw.iter().all(|&b| (32..127).contains(&b) || b == b'\t').then(|| {
            // Visible ASCII is UTF-8.
            std::str::from_utf8(raw).unwrap_or_default()
        });
        let name = header.name;
        if name.eq_ignore_ascii_case("content-length") {
            for said in String::from_utf8_lossy(raw).split(',') {
                let said = said.trim();
                if said.is_empty() || !said.bytes().all(|b| b.is_ascii_digit()) {
                    // alloc: an error's message, made only when it fails.
                    return Err(format!("a Content-Length of {said:?}"));
                }
                // alloc: an error's message, made only when it fails.
                let said: u64 =
                    said.parse().map_err(|_| format!("a Content-Length of {said}, past 2^64"))?;
                match length {
                    Some(was) if was != said => {
                        // alloc: an error's message, made only when it fails.
                        return Err(format!("two Content-Lengths that disagree, {was} and {said}"));
                    }
                    _ => length = Some(said),
                }
            }
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            encoded = true;
            // `chunked` frames the body only as the last coding; after another it is the close.
            let last = String::from_utf8_lossy(raw)
                .rsplit(',')
                .next()
                .map(|it| it.trim().to_ascii_lowercase());
            chunked = last.as_deref() == Some("chunked");
        } else if name.eq_ignore_ascii_case("connection") {
            for token in String::from_utf8_lossy(raw).split(',') {
                let token = token.trim();
                close |= token.eq_ignore_ascii_case("close");
                keep_alive |= token.eq_ignore_ascii_case("keep-alive");
            }
        } else if name.eq_ignore_ascii_case("location") && location.is_none() {
            // alloc: one per redirect, where it leads.
            location = text.map(str::to_owned);
        } else if name.eq_ignore_ascii_case("content-encoding") {
            gzip = text.is_some_and(|it| it.trim().eq_ignore_ascii_case("gzip"));
        }
        if let Some(text) = text {
            // alloc: one per header of an answer, handed to the asker.
            headers.push((name.to_ascii_lowercase(), text.to_owned()));
        }
    }
    let bodiless = head_request || (100..200).contains(&status) || status == 204 || status == 304;
    let body = if bodiless {
        Body::Length(0)
    } else if encoded && chunked {
        Body::Chunked(Chunked::default())
    } else if encoded {
        Body::Close
    } else if let Some(length) = length {
        Body::Length(length)
    } else {
        Body::Close
    };
    let keep = if one_one { !close } else { keep_alive && !close };
    let reusable = keep && !matches!(body, Body::Close) && !(encoded && length.is_some());
    let length = if matches!(body, Body::Length(_)) && !bodiless { length } else { None };
    Ok(Some(Head { len, status, headers, body, reusable, location, gzip, length }))
}

/// How a body is framed, and how much of it is left.
#[derive(Debug)]
pub enum Body {
    /// This many bytes more.
    Length(u64),
    Chunked(Chunked),
    /// Everything until the connection closes.
    Close,
}

impl Body {
    /// Appends to `out` the body's bytes at the start of `input`: how many bytes of `input` it
    /// took, and whether the body has ended (what follows is not its).
    pub fn decode(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<(usize, bool), String> {
        match self {
            Body::Length(left) => {
                let take = usize::try_from(*left).unwrap_or(usize::MAX).min(input.len());
                out.extend_from_slice(&input[..take]);
                *left -= take as u64;
                Ok((take, *left == 0))
            }
            Body::Chunked(chunked) => chunked.decode(input, out),
            Body::Close => {
                out.extend_from_slice(input);
                Ok((input.len(), false))
            }
        }
    }

    /// Whether the connection closing now ends the body whole: only a body framed by the close.
    pub fn closed(&self) -> Result<(), String> {
        match self {
            Body::Length(0) | Body::Close => Ok(()),
            // alloc: an error's message, made only when it fails.
            Body::Length(left) => {
                Err(format!("the connection closed with {left} bytes of the body unsent"))
            }
            Body::Chunked(chunked) if chunked.done() => Ok(()),
            Body::Chunked(_) => {
                Err("the connection closed in the middle of a chunked body".to_owned())
            }
        }
    }
}

/// Where a `chunked` body's reading is (RFC 9112 section 7.1).
#[derive(Debug, Default)]
pub struct Chunked {
    state: ChunkState,
    /// The size of the chunk being read, as its line has said so far.
    size: u64,
    /// Bytes of the chunk-size line or the trailer section read so far, for their bounds.
    line: usize,
    /// Hex digits of the chunk-size line read so far.
    digits: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ChunkState {
    /// In a chunk's size, its hex digits.
    #[default]
    Size,
    /// After the size: whitespace and extensions, up to the line's CR.
    Extension,
    /// The LF that ends a chunk-size line.
    SizeLf,
    /// This many bytes of chunk data left.
    Data(u64),
    /// The CR after a chunk's data.
    DataCr,
    /// The LF after a chunk's data.
    DataLf,
    /// At the start of a trailer line, or of the blank line that ends the body.
    Trailer,
    /// In a trailer line, up to its CR.
    TrailerLine,
    /// The LF that ends a trailer line.
    TrailerLf,
    /// The LF of the blank line that ends the body.
    EndLf,
    Done,
}

impl Chunked {
    fn done(&self) -> bool {
        self.state == ChunkState::Done
    }

    fn decode(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<(usize, bool), String> {
        let mut at = 0;
        while at < input.len() {
            let byte = input[at];
            match self.state {
                ChunkState::Done => break,
                ChunkState::Data(left) => {
                    let take = usize::try_from(left).unwrap_or(usize::MAX).min(input.len() - at);
                    out.extend_from_slice(&input[at..at + take]);
                    at += take;
                    let left = left - take as u64;
                    self.state =
                        if left == 0 { ChunkState::DataCr } else { ChunkState::Data(left) };
                    continue;
                }
                ChunkState::Size | ChunkState::Extension => {
                    self.line += 1;
                    if self.line > CHUNK_LINE_MOST {
                        // alloc: an error's message, made only when it fails.
                        return Err(format!(
                            "a chunk-size line longer than the {CHUNK_LINE_MOST} bytes allowed"
                        ));
                    }
                    match (self.state, byte) {
                        (ChunkState::Size, b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F') => {
                            let digit = u64::from((byte as char).to_digit(16).unwrap_or(0));
                            self.size = self
                                .size
                                .checked_mul(16)
                                .and_then(|it| it.checked_add(digit))
                                .ok_or_else(|| "a chunk size past 2^64".to_owned())?;
                            self.digits += 1;
                        }
                        (_, b'\r') if self.digits > 0 => self.state = ChunkState::SizeLf,
                        (_, b';' | b' ' | b'\t') if self.digits > 0 => {
                            self.state = ChunkState::Extension
                        }
                        (ChunkState::Extension, b'\n') => {
                            return Err("a chunk-size line ended by LF alone, not CRLF".to_owned());
                        }
                        (ChunkState::Extension, _) => {}
                        (_, _) => {
                            // alloc: an error's message, made only when it fails.
                            return Err(format!("a chunk size that is not hex: byte {byte:#04x}"));
                        }
                    }
                }
                ChunkState::SizeLf => {
                    if byte != b'\n' {
                        return Err("a chunk-size line not ended by CRLF".to_owned());
                    }
                    self.state = if self.size == 0 {
                        ChunkState::Trailer
                    } else {
                        ChunkState::Data(self.size)
                    };
                    self.size = 0;
                    self.line = 0;
                    self.digits = 0;
                }
                ChunkState::DataCr => {
                    if byte != b'\r' {
                        return Err("a chunk's data longer than its size said".to_owned());
                    }
                    self.state = ChunkState::DataLf;
                }
                ChunkState::DataLf => {
                    if byte != b'\n' {
                        return Err("a chunk's data not ended by CRLF".to_owned());
                    }
                    self.state = ChunkState::Size;
                }
                ChunkState::Trailer
                | ChunkState::TrailerLine
                | ChunkState::TrailerLf
                | ChunkState::EndLf => {
                    self.line += 1;
                    if self.line > HEAD_MOST {
                        // alloc: an error's message, made only when it fails.
                        return Err(format!("trailers longer than the {HEAD_MOST} bytes allowed"));
                    }
                    self.state = match (self.state, byte) {
                        (ChunkState::Trailer, b'\r') => ChunkState::EndLf,
                        (ChunkState::Trailer | ChunkState::TrailerLine, b'\r') => {
                            ChunkState::TrailerLf
                        }
                        (ChunkState::Trailer | ChunkState::TrailerLine, _) => {
                            ChunkState::TrailerLine
                        }
                        (ChunkState::TrailerLf, b'\n') => ChunkState::Trailer,
                        (ChunkState::EndLf, b'\n') => ChunkState::Done,
                        (_, _) => return Err("a trailer line not ended by CRLF".to_owned()),
                    };
                }
            }
            at += 1;
        }
        Ok((at, self.done()))
    }
}
