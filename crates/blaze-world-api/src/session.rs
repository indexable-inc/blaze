//! One connection, sans IO (rule 14): bytes in as they arrive, at most one [`Request`] out;
//! one [`Response`] in, its bytes out. A message is one CBOR item with every length definite,
//! so it says where it ends: no frame is put around it, and the bytes over a Unix socket are
//! the bytes of an HTTP POST's body. Bytes past the first message are a second request,
//! refused by name ([`WireCode::SecondRequest`]).
//!
//! The scanner finds the message's end as bytes come, each byte looked at once, its stack the
//! schema's depth: a message is decoded once, when whole.

use blaze_own::{AVec, Alloc};

use crate::{Limits, MOST_DEPTH, Request, Response, SessionError, WireCode, WireError, enc::wide};

/// Where the scan of the first item is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Scan {
    /// Bytes scanned: the items before this offset are whole.
    at: usize,
    /// Items still to come at each open level; the root's level holds the one message.
    left: [u64; MOST_DEPTH + 1],
    depth: usize,
}

/// What one head says, if its bytes are all in.
enum Head {
    /// More bytes are needed.
    Partial,
    /// A head and the bytes of its string, if any, are in: `len` bytes taken, and how many items
    /// it opens (an array's or a map's).
    Whole { len: usize, opens: u64 },
}

impl Scan {
    fn new() -> Scan {
        let mut left = [0; MOST_DEPTH + 1];
        left[0] = 1;
        Scan { at: 0, left, depth: 1 }
    }

    fn done(&self) -> bool {
        self.depth == 0
    }

    /// The head at `self.at` of `bytes`.
    fn head(&self, bytes: &[u8], limits: &Limits) -> Result<Head, WireError> {
        let at = self.at;
        let Some(&first) = bytes.get(at) else { return Ok(Head::Partial) };
        let (major, info) = (first >> 5, first & 0x1f);
        if major == 6 {
            return Err(WireError { cbor: blaze_cbor::why::TAG, ..WireError::new(at, WireCode::Cbor, "message", 0) });
        }
        if major == 7 {
            return match first {
                0xf4..=0xf6 => Ok(Head::Whole { len: 1, opens: 0 }),
                _ => Err(WireError { cbor: blaze_cbor::why::SIMPLE, ..WireError::new(at, WireCode::Cbor, "message", 0) }),
            };
        }
        let extra = match info {
            0..=23 => 0,
            24 => 1,
            25 => 2,
            26 => 4,
            27 => 8,
            _ => return Err(WireError::new(at, WireCode::Indefinite, "message", 0)),
        };
        let Some(raw) = bytes.get(at + 1..at + 1 + extra) else { return Ok(Head::Partial) };
        let value = match extra {
            0 => u64::from(info),
            _ => raw.iter().fold(0u64, |n, byte| (n << 8) | u64::from(*byte)),
        };
        let len = 1 + extra;
        match major {
            // A string: its bytes must fit the message, whole, before any wait for them.
            2 | 3 => {
                let total = wide(at + len).saturating_add(value);
                if total > wide(limits.most_bytes) {
                    return Err(WireError::new(at, WireCode::TooLarge, "message", wide(limits.most_bytes)));
                }
                let end = at + len + usize::try_from(value).unwrap_or_else(|_| unreachable!("at most `most_bytes`"));
                match end <= bytes.len() {
                    true => Ok(Head::Whole { len: end - at, opens: 0 }),
                    false => Ok(Head::Partial),
                }
            }
            4 => Ok(Head::Whole { len, opens: value }),
            5 => Ok(Head::Whole { len, opens: value.saturating_mul(2) }),
            _ => Ok(Head::Whole { len, opens: 0 }),
        }
    }

    /// Scans what `bytes` adds; stops at the first item's end.
    fn feed(&mut self, bytes: &[u8], limits: &Limits) -> Result<(), WireError> {
        while !self.done() {
            let (len, opens) = match self.head(bytes, limits)? {
                Head::Partial => return Ok(()),
                Head::Whole { len, opens } => (len, opens),
            };
            let at = self.at;
            self.at += len;
            self.left[self.depth - 1] -= 1;
            if opens > 0 {
                if self.depth == MOST_DEPTH + 1 {
                    return Err(WireError::new(at, WireCode::TooDeep, "message", wide(MOST_DEPTH)));
                }
                self.left[self.depth] = opens;
                self.depth += 1;
            }
            while self.depth > 0 && self.left[self.depth - 1] == 0 {
                self.depth -= 1;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// Bytes are coming.
    Reading,
    /// The request was handed out; its response is owed.
    Asked,
    /// The bytes were refused; the refusal is owed.
    Refused,
    /// The response was written.
    Answered,
}

/// The server's side of one connection.
pub struct Session<'a> {
    bytes: AVec<'a, u8>,
    scan: Scan,
    state: State,
    limits: Limits,
    alloc: Alloc<'a>,
}

impl<'a> Session<'a> {
    pub fn new(limits: Limits, alloc: Alloc<'a>) -> Session<'a> {
        Session { bytes: AVec::new_in(alloc), scan: Scan::new(), state: State::Reading, limits, alloc }
    }

    /// Takes bytes as they come. Gives the request once its last byte is in (`Ok(None)` until
    /// then); bytes after it, now or later, are [`WireCode::SecondRequest`], and the one answer
    /// owed is then that refusal. Bytes after a refusal are refused again, never dropped.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Option<Request<'a>>, SessionFeed> {
        match self.state {
            State::Reading => {}
            State::Asked | State::Refused => {
                let at = self.bytes.len();
                return Err(self.refuse(WireError::new(at, WireCode::SecondRequest, "message", 0)));
            }
            State::Answered => return Err(SessionFeed::Session(SessionError::Closed)),
        }
        if bytes.len() > self.limits.most_bytes - self.bytes.len() {
            let most = wide(self.limits.most_bytes);
            return Err(self.refuse(WireError::new(self.limits.most_bytes, WireCode::TooLarge, "message", most)));
        }
        self.bytes.extend_from_slice(bytes);
        if let Err(error) = self.scan.feed(&self.bytes, &self.limits) {
            return Err(self.refuse(error));
        }
        if !self.scan.done() {
            return Ok(None);
        }
        let end = self.scan.at;
        if end < self.bytes.len() {
            return Err(self.refuse(WireError::new(end, WireCode::SecondRequest, "message", 0)));
        }
        match Request::decode(&self.bytes[..end], self.limits, self.alloc) {
            Ok(request) => {
                self.state = State::Asked;
                Ok(Some(request))
            }
            Err(error) => Err(self.refuse(error)),
        }
    }

    /// The peer closed its side: a message not yet whole is [`WireCode::Short`].
    pub fn closed(&mut self) -> Result<(), SessionFeed> {
        match self.state {
            State::Reading => {
                let at = self.bytes.len();
                Err(self.refuse(WireError::new(at, WireCode::Short, "message", 0)))
            }
            State::Asked | State::Refused | State::Answered => Ok(()),
        }
    }

    fn refuse(&mut self, error: WireError) -> SessionFeed {
        self.state = State::Refused;
        SessionFeed::Wire(error)
    }

    /// The one response, its bytes appended to `out`: after a request, or (as a refusal) after
    /// bytes were refused.
    pub fn answer(&mut self, response: &Response<'_>, out: &mut AVec<'_, u8>) -> Result<(), SessionError> {
        match self.state {
            State::Reading => Err(SessionError::NotAsked),
            State::Answered => Err(SessionError::Answered),
            State::Asked | State::Refused => {
                response.encode(out);
                self.state = State::Answered;
                Ok(())
            }
        }
    }
}

/// Why [`Session::feed`] gave no request: the bytes were refused (answer with
/// [`WireError::refusal`]), or the session was driven out of its order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SessionFeed {
    Wire(WireError),
    Session(SessionError),
}
