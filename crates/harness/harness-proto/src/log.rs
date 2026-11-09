//! A chat's log, `objects/<id>/chat`: [`MAGIC`], then one [`crate::frame::T_EVENT`] frame per event, exactly the bytes
//! the socket carried, appended and never rewritten. Reading it back ([`read`]) gives each record's number (from 1), its
//! frame's bytes and its event. A frame whose header is whole but whose payload is short is the torn write a crash
//! mid-append leaves: it can only be the last, [`Log::torn`] says where it starts, and the daemon cuts it off before it
//! appends again (so the next frame is reachable). A whole frame whose payload does not decode is reported and skipped;
//! the records after it stand.
use crate::event::Event;
use crate::frame::{HEAD, T_EVENT};

/// The first bytes of every chat log: its format and the schema it holds.
pub const MAGIC: &[u8] = b"harness-chat/1\n";

/// One record of a log.
#[derive(Debug)]
pub struct Record<'a> {
    /// Its place in the log, from 1.
    pub n: usize,
    /// Its frame's bytes (header and payload), as written.
    pub frame: &'a [u8],
    /// Its event, or why its payload is not one.
    pub event: Result<Event, String>,
}

/// A log's records, and the torn tail if a crash left one.
#[derive(Debug)]
pub struct Log<'a> {
    pub records: Vec<Record<'a>>,
    /// Where a frame the log ends inside of starts, and how many bytes of it are there.
    pub torn: Option<(usize, usize)>,
    /// The log's length up to its last whole frame (where an append must start).
    pub end: usize,
}

/// Why bytes are not a chat log of this schema.
#[derive(Debug, PartialEq, Eq)]
pub enum NotALog {
    /// The NDJSON log of before schema 1 (the daemon converts these at start: [`crate::migrate`]).
    Ndjson,
    /// Anything else: what its first bytes are.
    Unknown(String),
}

impl std::fmt::Display for NotALog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ndjson => f.write_str("an NDJSON chat log from before harness schema 1 (the daemon converts these when it starts)"),
            Self::Unknown(head) => write!(f, "not a harness chat log (it starts {head}, not {MAGIC:?})"),
        }
    }
}

/// Whether a log needs its [`MAGIC`] written: it is empty, or a crash cut it inside the magic.
#[must_use]
pub fn unstarted(bytes: &[u8]) -> bool {
    bytes.len() < MAGIC.len() && MAGIC.starts_with(bytes)
}

/// A log's records.
///
/// # Errors
/// Bytes that do not start with [`MAGIC`] (an empty or [`unstarted`] log is an empty one).
pub fn read(bytes: &[u8]) -> Result<Log<'_>, NotALog> {
    if unstarted(bytes) {
        return Ok(Log { records: Vec::new(), torn: None, end: bytes.len() });
    }
    let Some(mut rest) = bytes.strip_prefix(MAGIC) else {
        let first = bytes.iter().copied().find(|b| !b.is_ascii_whitespace());
        return Err(if first == Some(b'{') {
            NotALog::Ndjson
        } else {
            NotALog::Unknown(format!("{:?}", String::from_utf8_lossy(&bytes[..bytes.len().min(16)])))
        });
    };
    let mut at = MAGIC.len();
    let mut records = Vec::new();
    let mut torn = None;
    while !rest.is_empty() {
        let n = records.len() + 1;
        let Some(h) = rest.first_chunk::<HEAD>() else {
            torn = Some((at, rest.len()));
            break;
        };
        let len = u32::from_be_bytes([h[1], h[2], h[3], h[4]]) as usize;
        if h[0] != T_EVENT || len > crate::frame::MAX_FRAME {
            // Not a frame boundary: nothing after it can be found. Only a torn header can look like this (a crash
            // never writes bytes that are not a frame's), so it is the tail.
            torn = Some((at, rest.len()));
            break;
        }
        let Some(frame) = rest.get(..HEAD + len) else {
            torn = Some((at, rest.len()));
            break;
        };
        let event = crate::cbor::decode::<Event>(&frame[HEAD..]).map_err(|e| e.to_string());
        records.push(Record { n, frame, event });
        at += frame.len();
        rest = &rest[frame.len()..];
    }
    Ok(Log { records, torn, end: at })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Body;

    #[test]
    fn records_and_a_torn_tail() {
        let a = Event { ts: 1, body: Body::User { text: "hi".into(), images: Vec::new() } }.frame();
        let b = Event { ts: 2, body: Body::Busy { busy: true } }.frame();
        let mut log = MAGIC.to_vec();
        log.extend_from_slice(&a);
        log.extend_from_slice(&b);
        let whole = read(&log).unwrap();
        assert_eq!((whole.records.len(), whole.torn, whole.end), (2, None, log.len()));
        assert_eq!(whole.records[1].frame, &b[..]);
        let cut = &log[..log.len() - 1];
        let torn = read(cut).unwrap();
        assert_eq!((torn.records.len(), torn.torn, torn.end), (1, Some((MAGIC.len() + a.len(), b.len() - 1)), MAGIC.len() + a.len()));
        assert_eq!(read(b"{\"ev\":\"user\"}\n").unwrap_err(), NotALog::Ndjson);
        assert!(read(&MAGIC[..4]).unwrap().records.is_empty());
    }
}
