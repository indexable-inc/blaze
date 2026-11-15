//! The read-only endpoint a daemon may open so machines on its network read its store, and
//! what a reader makes of its answers. No server besides the daemons themselves.
//!
//! **Off unless asked for, and never wider than asked.** [`Endpoint::new`] takes the address
//! to bind and the public keys that may read, both written by the user, and refuses an
//! address that is every interface (`0.0.0.0`, `::`), since the user is to name the one they
//! mean, and an empty list of keys (an endpoint nobody may read is a mistake, not a
//! default). Who may read is said by key, not by address: every request carries its
//! caller's public key and that key's signature over the request's path
//! ([`request_message`]), and a request whose key is not listed or whose signature is not
//! the key's is answered 403 with nothing of the store read. So another user of the machine,
//! or a machine on the network, that holds no listed key reads nothing, whatever address it
//! calls from; an address can be borrowed, a key cannot. (The daemon's driver names no
//! caller's address either: `blaze_core::Done::Accepted` carries the connection alone.)
//!
//! **What it serves** (HTTP/1.1, `GET` only, one request a connection):
//!
//! ```text
//! GET /v1/has/<key>[,<key>...]   200: the signed statements of those of the keys it has (at most HAS_MOST keys)
//! GET /v1/record/<key>           206 (Range: bytes=a-b, at most RANGE_MOST bytes) or 200 (a record that short): its bytes   404: none
//! ```
//!
//! It serves by key because a daemon's store is not a pack: what one machine built an hour
//! ago is asked for by the next machine by the key its own plan computed. Many keys in one
//! `has` is how a build asks for every unit's record at once: what the peer lacks then
//! costs no request of its own.
//!
//! **Trust** (see [`crate::head`]). A [`Statement`] is the peer saying "the record under this
//! key has this kind, length and content hash", signed with the peer's key. A reader takes
//! it only when that key is in its [`Trust`], then reads the bytes and checks them against
//! the statement's content hash. The transport is plain HTTP and carries no secret: a
//! statement cannot be forged without the key, bytes cannot be changed without failing the
//! hash, and an old statement played again is still true. A request played again by someone
//! who saw it reads the record the request named, which they saw answered. What plain HTTP
//! does not give is privacy against whoever can read the network between: on a network
//! where that matters the address to bind is a tunnel's (WireGuard, Tailscale).
//!
//! A peer that is not listed is never asked. A peer that is listed and dishonest can make
//! the reader link code that is not what its sources say: listing a key is trusting its
//! holder's builds as one's own.

use alloc::{
    format,
    string::{String, ToString as _},
    vec::Vec,
};
use core::net::SocketAddr;

use crate::{Id, Record, Refused, Signatures, Trust, cbor, manifest};

/// The most bytes of a request's head: a longer one is answered 431 unread.
pub const HEAD_MOST: usize = 8 << 10;
/// The most bytes of a signed statement.
pub const STATEMENT_MOST: u64 = 512;
/// The most keys one `has` asks for: their hex fits a request's head.
pub const HAS_MOST: usize = 64;
/// The most bytes of the answer to one `has`.
pub const STATEMENTS_MOST: u64 = 64 + HAS_MOST as u64 * (STATEMENT_MOST + 8);
/// The most bytes of a record one request is answered with: a longer record is read by
/// ranges, so neither side holds more than this for one answer.
pub const RANGE_MOST: u64 = 4 << 20;

const MAGIC: &str = "blaze pack peer statement";
const LIST_MAGIC: &str = "blaze pack peer statements";
const LAYOUT: u64 = 1;
const DOMAIN: &[u8] = b"blaze pack peer statement signature 1\0";
const REQUEST_DOMAIN: &[u8] = b"blaze pack peer request signature 1\0";
const WHAT: &str = "the peer's statement";
/// The request headers a caller says who it is with: its public key and its signature of
/// [`request_message`], in hex.
pub const CALLER: &str = "blaze-peer";
pub const SIGNATURE: &str = "blaze-peer-signature";

/// What a caller signs to ask for `path`.
pub fn request_message(path: &str) -> Vec<u8> {
    // alloc: the message, once a request made or checked
    let mut out = Vec::with_capacity(REQUEST_DOMAIN.len() + path.len());
    out.extend_from_slice(REQUEST_DOMAIN);
    out.extend_from_slice(path.as_bytes());
    out
}

/// `bytes` in lower-case hex.
pub fn hex(bytes: &[u8]) -> String {
    use core::fmt::Write as _;
    // alloc: a header's value, once a request
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing to a `String` does not fail.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn unhex<const N: usize>(hex: &str) -> Option<[u8; N]> {
    let hex = hex.as_bytes();
    if hex.len() != N * 2 {
        return None;
    }
    let digit = |byte: u8| match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    };
    let mut out = [0u8; N];
    for (at, pair) in hex.chunks_exact(2).enumerate() {
        out[at] = digit(pair[0])? << 4 | digit(pair[1])?;
    }
    Some(out)
}

/// An endpoint the user asked for: where it listens and whose requests it answers.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Endpoint {
    bind: SocketAddr,
    readers: Trust,
}

impl Endpoint {
    /// The endpoint at `bind` (`10.0.0.4:7420`, `[fd00::4]:7420`) answering the requests
    /// signed by one of `readers`.
    pub fn new(bind: &str, readers: Vec<[u8; 32]>) -> Result<Endpoint, Refused> {
        let untrusted = |what: String| Refused::Untrusted { what };
        let Ok(bind) = bind.parse::<SocketAddr>() else {
            return Err(Refused::Name { what: "the address to serve on", name: bind.chars().take(64).collect() });
        };
        if bind.ip().is_unspecified() {
            // alloc: an error's text
            return Err(untrusted(format!(
                "serving the store on {bind} would answer on every interface; name the one address to serve on"
            )));
        }
        if readers.is_empty() {
            return Err(untrusted(
                "serving the store with no reader's key answers nobody; list the public keys that may read it".to_string(),
            ));
        }
        Ok(Endpoint { bind, readers: Trust::new(readers) })
    }

    pub fn bind(&self) -> SocketAddr {
        self.bind
    }

    /// Whether `request` is answered: its caller's key is listed and signed its path.
    pub fn lets(&self, request: &Request<'_>, signatures: &impl Signatures) -> bool {
        let Some((caller, signature)) = &request.caller else { return false };
        self.readers.trusts(caller) && signatures.verify(caller, &request_message(request.path), signature)
    }
}

/// What a request asks for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Asked {
    /// The signed statements of the records under `keys`, of those it has.
    Has { keys: Vec<Id> },
    /// The record's bytes, all or the range `from..=to`.
    Record { key: Id, range: Option<(u64, u64)> },
}

/// A request's head, read.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Request<'a> {
    pub path: &'a str,
    pub asked: Asked,
    /// The caller's public key and its signature of [`request_message`] of the path, when
    /// the request carries both, well formed.
    pub caller: Option<([u8; 32], [u8; 64])>,
}

/// A request's head, read: what it asks, or the status to answer with and why.
/// `head` is everything up to and including the blank line.
pub fn asked(head: &[u8]) -> Result<Request<'_>, (u16, &'static str)> {
    if head.len() > HEAD_MOST {
        return Err((431, "the request's head is too long"));
    }
    let Ok(head) = core::str::from_utf8(head) else {
        return Err((400, "the request's head is not text"));
    };
    let mut lines = head.split("\r\n");
    let mut words = lines.next().unwrap_or("").split(' ');
    let (Some(method), Some(path), Some(version), None) = (words.next(), words.next(), words.next(), words.next())
    else {
        return Err((400, "the request line is not a method, a path and a version"));
    };
    if !matches!(version, "HTTP/1.1" | "HTTP/1.0") {
        return Err((400, "the request is not HTTP/1.x"));
    }
    if method != "GET" {
        return Err((405, "only GET is served"));
    }
    let (mut range, mut caller, mut signature) = (None, None, None);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        if name.eq_ignore_ascii_case("range") {
            let Some(spec) = value.strip_prefix("bytes=") else {
                return Err((416, "a range that is not of bytes"));
            };
            let parsed = spec.split_once('-').and_then(|(from, to)| {
                Some((from.parse::<u64>().ok()?, to.parse::<u64>().ok()?))
            });
            match parsed {
                Some((from, to)) if from <= to => range = Some((from, to)),
                _ => return Err((416, "a range that is not `bytes=<from>-<to>`")),
            }
        } else if name.eq_ignore_ascii_case(CALLER) {
            caller = unhex::<32>(value);
        } else if name.eq_ignore_ascii_case(SIGNATURE) {
            signature = unhex::<64>(value);
        }
    }
    let caller = caller.zip(signature);
    // A path is one of two fixed prefixes and keys of 64 hex digits: nothing in it is a
    // file name.
    let key = |hex: &str| -> Result<Id, (u16, &'static str)> {
        Id::from_hex(hex).ok_or((404, "no such path"))
    };
    if let Some(list) = path.strip_prefix("/v1/has/") {
        // alloc: the keys asked, at most `HAS_MOST`
        let mut keys = Vec::new();
        for hex in list.split(',') {
            if keys.len() == HAS_MOST {
                return Err((400, "more keys than one `has` takes"));
            }
            keys.push(key(hex)?);
        }
        return Ok(Request { path, asked: Asked::Has { keys }, caller });
    }
    if let Some(hex) = path.strip_prefix("/v1/record/") {
        return Ok(Request { path, asked: Asked::Record { key: key(hex)?, range }, caller });
    }
    Err((404, "no such path"))
}

/// The path that asks for the statements of `keys` (at most [`HAS_MOST`]).
pub fn has_path(keys: &[Id]) -> String {
    // alloc: a request's path
    let mut out = String::with_capacity(8 + keys.len() * 65);
    out.push_str("/v1/has/");
    for (at, key) in keys.iter().enumerate() {
        if at > 0 {
            out.push(',');
        }
        out.push_str(&key.to_string());
    }
    out
}

/// The path that asks for the record of `key`.
pub fn record_path(key: &Id) -> String {
    format!("/v1/record/{key}")
}

/// The bytes of a record of `len` bytes a request's `range` names: `from..to` (exclusive),
/// or the status to answer with. No more than [`RANGE_MOST`] bytes a request.
pub fn slice(len: u64, range: Option<(u64, u64)>) -> Result<(u64, u64), (u16, &'static str)> {
    let (from, to) = match range {
        None => (0, len),
        Some((from, to)) if from < len && to < len => (from, to + 1),
        Some(_) => return Err((416, "a range past the record's end")),
    };
    if to - from > RANGE_MOST {
        return Err((416, "more bytes than one request is answered with: ask by ranges"));
    }
    Ok((from, to))
}

/// The head of a response of `status` whose body of `len` bytes follows; `part` is the
/// range served of a whole of that many bytes (`from`, `to` exclusive, `of`).
pub fn response_head(status: u16, len: u64, part: Option<(u64, u64, u64)>) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        206 => "Partial Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        416 => "Range Not Satisfiable",
        431 => "Request Header Fields Too Large",
        _ => "Error",
    };
    let range = match part {
        // alloc: a response's head
        Some((from, to, of)) => format!("Content-Range: bytes {from}-{}/{of}\r\n", to - 1),
        None => String::new(),
    };
    // alloc: a response's head
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/octet-stream\r\n{range}Content-Length: {len}\r\nConnection: close\r\n\r\n"
    )
    .into_bytes()
}

/// The answer to a `has`: the statements, each as its own bytes, in one CBOR array
/// `["blaze pack peer statements", 1, [statement, ...]]`.
pub fn statements(each: &[Vec<u8>]) -> Vec<u8> {
    // alloc: the answer's bytes, which are the output
    let mut out = Vec::with_capacity(64 + each.iter().map(|it| it.len() + 8).sum::<usize>());
    cbor::array(&mut out, 3);
    cbor::text(&mut out, LIST_MAGIC);
    cbor::uint(&mut out, LAYOUT);
    cbor::array(&mut out, each.len() as u64);
    for one in each {
        cbor::bytes(&mut out, one);
    }
    out
}

/// The statements an answer to a `has` holds, each decoded; none is checked yet
/// ([`Statement::vouched`]).
pub fn statements_of(bytes: &[u8]) -> Result<Vec<Statement>, Refused> {
    const LIST: &str = "the peer's statements";
    if bytes.len() as u64 > STATEMENTS_MOST {
        return Err(Refused::TooLarge { what: LIST, len: bytes.len() as u64, most: STATEMENTS_MOST });
    }
    let mut reader = cbor::Reader::new(bytes);
    let malformed = |error: cbor::Error| Refused::Malformed { what: LIST, at: error.at, why: error.why };
    let foreign = || Refused::Layout { what: LIST, found: "not a blaze pack peer's statements".to_string() };
    if reader.array().map_err(|_| foreign())? != 3 {
        return Err(foreign());
    }
    if reader.text().map_err(|_| foreign())? != LIST_MAGIC {
        return Err(foreign());
    }
    let version = reader.uint().map_err(malformed)?;
    if version != LAYOUT {
        // alloc: an error's text
        return Err(Refused::Layout { what: LIST, found: format!("of layout {version}, not {LAYOUT}") });
    }
    let count = reader.array().map_err(malformed)?;
    if count > HAS_MOST as u64 {
        return Err(Refused::TooLarge { what: LIST, len: count, most: HAS_MOST as u64 });
    }
    // alloc: the statements, at most `HAS_MOST`
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        out.push(Statement::decode(reader.bytes().map_err(malformed)?)?);
    }
    reader.end().map_err(malformed)?;
    Ok(out)
}

/// A peer saying what the record under a key is.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Statement {
    pub record: Record,
    pub signer: [u8; 32],
    pub signature: [u8; 64],
}

impl Statement {
    /// What `signer` signs to state `record`.
    pub fn message(record: &Record, signer: &[u8; 32]) -> Vec<u8> {
        // alloc: the message, once a statement made or checked
        let mut out = Vec::with_capacity(DOMAIN.len() + 160);
        out.extend_from_slice(DOMAIN);
        Statement::write(record, signer, &[], &mut out);
        out
    }

    fn write(record: &Record, signer: &[u8; 32], signature: &[u8], out: &mut Vec<u8>) {
        cbor::array(out, 8);
        cbor::text(out, MAGIC);
        cbor::uint(out, LAYOUT);
        cbor::bytes(out, &record.key.0);
        cbor::uint(out, u64::from(record.kind));
        cbor::uint(out, record.len);
        cbor::bytes(out, &record.content.0);
        cbor::bytes(out, signer);
        cbor::bytes(out, signature);
    }

    pub fn encode(&self) -> Vec<u8> {
        // alloc: the statement's bytes, which are the output
        let mut out = Vec::with_capacity(256);
        Statement::write(&self.record, &self.signer, &self.signature, &mut out);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Statement, Refused> {
        if bytes.len() as u64 > STATEMENT_MOST {
            return Err(Refused::TooLarge { what: WHAT, len: bytes.len() as u64, most: STATEMENT_MOST });
        }
        let mut reader = cbor::Reader::new(bytes);
        let malformed = |error: cbor::Error| Refused::Malformed { what: WHAT, at: error.at, why: error.why };
        let foreign = || Refused::Layout { what: WHAT, found: "not a blaze pack peer statement".to_string() };
        if reader.array().map_err(|_| foreign())? != 8 {
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
        let key = Id(reader.fixed::<32>().map_err(malformed)?);
        let at = reader.at();
        let Ok(kind) = u8::try_from(reader.uint().map_err(malformed)?) else {
            return Err(Refused::Malformed { what: WHAT, at, why: "the record's kind is not a byte" });
        };
        let len = reader.uint().map_err(malformed)?;
        if len > manifest::RECORD_MOST {
            return Err(Refused::TooLarge { what: "the record a peer states", len, most: manifest::RECORD_MOST });
        }
        let content = Id(reader.fixed::<32>().map_err(malformed)?);
        let signer = reader.fixed::<32>().map_err(malformed)?;
        let signature = reader.fixed::<64>().map_err(malformed)?;
        reader.end().map_err(malformed)?;
        Ok(Statement { record: Record { key, kind, len, content }, signer, signature })
    }

    /// The record this states, when it is about `key`, a key of `trust` signed it and the
    /// signature is that key's; else why it is not taken.
    pub fn vouched(&self, key: &Id, trust: &Trust, signatures: &impl Signatures) -> Result<Record, Refused> {
        let untrusted = |what: String| Refused::Untrusted { what };
        if self.record.key != *key {
            // alloc: an error's text
            return Err(untrusted(format!(
                "a peer asked for key {key} answered with a statement about key {}",
                self.record.key
            )));
        }
        if !trust.trusts(&self.signer) {
            // alloc: an error's text
            return Err(untrusted(format!(
                "the statement about key {key} is signed by {}, which is not among the keys you trust",
                Id(self.signer)
            )));
        }
        if !signatures.verify(&self.signer, &Statement::message(&self.record, &self.signer), &self.signature) {
            // alloc: an error's text
            return Err(untrusted(format!(
                "the signature on the statement about key {key} is not {}'s",
                Id(self.signer)
            )));
        }
        Ok(self.record)
    }
}
