//! Frames in and out of a byte stream, in [`blaze_protocol`]'s layout (a four-byte big-endian
//! length, then one item in its deterministic CBOR), cut as the bytes arrive. An item is an
//! [`Envelope`] of [`SCHEMA`]: another schema is refused by name, and a body that does not
//! decode is refused under its request's id.

use std::{
    io::{self, Read},
    ops::Range,
};

use blaze_protocol::cbor;
use serde::{Deserialize, Serialize, de::IgnoredAny};

use crate::{ApiError, Envelope, LONGEST_REQUEST, SCHEMA};

/// Bytes of a stream, cut into frames' items as they arrive.
#[derive(Default, Debug)]
pub struct Frames {
    /// The stream's bytes are `bytes[at..end]`. What is past `end` is room a read fills, zeroed
    /// once when the buffer grew to it rather than on every read.
    bytes: Vec<u8>,
    /// Where the next frame starts in `bytes`.
    at: usize,
    end: usize,
    /// How many bytes the buffer [`Frames::lend`] gave a receive held.
    lent: usize,
    broken: Option<ApiError>,
}

/// The most room [`Frames::lend`] keeps past a frame: one long frame does not hold its size
/// for the connection's life.
const KEPT_ROOM: usize = 1 << 20;

impl Frames {
    pub fn new() -> Frames {
        Frames::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        if self.broken.is_some() {
            return;
        }
        self.compact();
        let room = (self.bytes.len() - self.end).min(bytes.len());
        let (here, rest) = bytes.split_at(room);
        self.bytes[self.end..self.end + room].copy_from_slice(here);
        self.bytes.extend_from_slice(rest);
        self.end += bytes.len();
    }

    /// [`Frames::push`] of a buffer that is the caller's no more: kept as it is, uncopied, when
    /// nothing is held before it.
    pub(crate) fn push_vec(&mut self, bytes: Vec<u8>) {
        if self.broken.is_some() {
            return;
        }
        if self.held() == 0 {
            self.end = bytes.len();
            self.bytes = bytes;
            self.at = 0;
        } else {
            self.push(&bytes);
        }
    }

    /// The buffer, for a receive to append the stream's next bytes to ([`blaze_core`]'s
    /// `Effect::Recv`): what it holds of the frame under way moved to its front, its room kept.
    /// It holds nothing until [`Frames::give_back`], so a warm receive allocates nothing and
    /// its bytes land where they are read.
    pub(crate) fn lend(&mut self) -> Vec<u8> {
        let held = self.held();
        self.bytes.copy_within(self.at..self.end, 0);
        self.bytes.truncate(held);
        self.bytes.shrink_to(KEPT_ROOM);
        self.at = 0;
        self.end = 0;
        self.lent = held;
        std::mem::take(&mut self.bytes)
    }

    /// Takes back the buffer [`Frames::lend`] gave, with what the receive appended to it:
    /// false when that was nothing, the peer having hung up.
    pub(crate) fn give_back(&mut self, bytes: Vec<u8>) -> bool {
        let appended = bytes.len() > self.lent;
        self.lent = 0;
        self.push_vec(bytes);
        appended
    }

    /// The next whole item; `Ok(None)` until one is here. An error for a length over
    /// [`LONGEST_REQUEST`], after which the stream is unusable (the peer is hung up on).
    pub fn next(&mut self) -> Result<Option<Vec<u8>>, ApiError> {
        Ok(self.next_ref()?.map(<[u8]>::to_vec))
    }

    /// [`Frames::next`] in place: the item stays in the buffer until the next push.
    pub fn next_ref(&mut self) -> Result<Option<&[u8]>, ApiError> {
        Ok(self.next_range()?.map(|range| &self.bytes[range]))
    }

    /// Where [`Frames::next_ref`]'s item is, for [`Frames::item`]: so a caller can ask
    /// [`Frames::ready`] while it holds the item.
    pub(crate) fn next_range(&mut self) -> Result<Option<Range<usize>>, ApiError> {
        let Some(len) = self.declared()? else { return Ok(None) };
        let start = self.at + 4;
        let end = start + len;
        if self.end < end {
            return Ok(None);
        }
        self.at = end;
        Ok(Some(start..end))
    }

    /// The item [`Frames::next_range`] said is at `range`, until the next push or fill.
    pub(crate) fn item(&self, range: Range<usize>) -> &[u8] {
        &self.bytes[range]
    }

    /// Whether [`Frames::next_ref`] answers without another byte: a whole item is held, or the
    /// stream is refused.
    pub fn ready(&self) -> bool {
        if self.broken.is_some() {
            return true;
        }
        match self.length() {
            Some(len) if len > LONGEST_REQUEST => true,
            Some(len) => self.held() >= 4 + len as usize,
            None => false,
        }
    }

    /// How many bytes are held that are not yet a whole item.
    pub(crate) fn held(&self) -> usize {
        self.end - self.at
    }

    /// The length the frame under way declares, once its four bytes are here.
    fn length(&self) -> Option<u32> {
        let head = self.bytes[..self.end].get(self.at..self.at + 4)?;
        Some(u32::from_be_bytes(head.try_into().ok()?))
    }

    /// How many bytes to read next: what the frame under way still lacks, but no more than
    /// have arrived of it, so a buffer grows on the peer's word only as fast as its bytes come;
    /// and never fewer than `least`.
    pub fn wanted(&self, least: usize) -> usize {
        let held = self.held();
        let Some(len) = self.length() else { return least };
        if len > LONGEST_REQUEST {
            return least;
        }
        let lacking = (4 + len as usize).saturating_sub(held);
        lacking.min(held).max(least)
    }

    /// One read of at most `most` bytes from `from`, straight into the buffer; 0 once the peer
    /// has hung up.
    pub fn fill(&mut self, from: &mut impl Read, most: usize) -> io::Result<usize> {
        self.compact();
        let end = self.end;
        if self.bytes.len() < end + most {
            self.bytes.resize(end + most, 0);
        }
        let read = loop {
            match from.read(&mut self.bytes[end..end + most]) {
                Ok(read) => break Ok(read),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => break Err(error),
            }
        };
        if let Ok(got) = &read {
            self.end += got;
        }
        read
    }

    /// The length the next frame declares, once its head is here.
    fn declared(&mut self) -> Result<Option<usize>, ApiError> {
        if let Some(error) = &self.broken {
            // clone: error path, a stream hung up on refuses every later read.
            return Err(error.clone());
        }
        let Some(len) = self.length() else { return Ok(None) };
        if len > LONGEST_REQUEST {
            // alloc: error path, a peer's frame too long; the stream is hung up on.
            let said = format!("a frame of {len} bytes, longer than {LONGEST_REQUEST}");
            let error = ApiError::new(said, None);
            // clone: error path, once per stream hung up on.
            self.broken = Some(error.clone());
            drop(std::mem::take(&mut self.bytes));
            self.at = 0;
            self.end = 0;
            return Err(error);
        }
        Ok(Some(len as usize))
    }

    /// Drops the items handed out, once they are at least half of what is held.
    fn compact(&mut self) {
        if self.at > 0 && self.at >= self.end / 2 {
            self.bytes.copy_within(self.at..self.end, 0);
            self.end -= self.at;
            self.at = 0;
        }
    }
}

/// Why an item was refused, with the envelope's id when it could be read.
#[derive(PartialEq, Eq, Debug)]
pub struct Refused {
    pub id: Option<u64>,
    pub error: ApiError,
}

/// An item as an envelope of [`SCHEMA`]: another schema refused by name ("schema 2, this
/// daemon speaks 4"), whether or not its body is one of this schema's; non-deterministic CBOR
/// refused naming the offence. An item that decodes is read in one pass; only a refused one
/// has its head read apart. A `T` that borrows (a [`crate::ResponseRef`]) borrows from `item`.
pub fn decode<'a, T: Deserialize<'a>>(item: &'a [u8]) -> Result<Envelope<T>, Refused> {
    let error = match cbor::from_slice::<Envelope<T>>(item) {
        Ok(envelope) if envelope.schema == SCHEMA => return Ok(envelope),
        Ok(envelope) => return Err(wrong_schema(envelope.schema, envelope.id)),
        Err(error) => error,
    };
    match head(item)? {
        (schema, id) if schema != SCHEMA => Err(wrong_schema(schema, id)),
        (_, id) => Err(Refused { id: Some(id), error: undecodable(&error) }),
    }
}

/// The schema and id of the envelope `item` is, its body unread.
pub(crate) fn head(item: &[u8]) -> Result<(u32, u64), Refused> {
    let envelope: Envelope<IgnoredAny> =
        cbor::from_slice(item).map_err(|error| Refused { id: None, error: undecodable(&error) })?;
    Ok((envelope.schema, envelope.id))
}

fn wrong_schema(schema: u32, id: u64) -> Refused {
    // alloc: error path, an envelope of another schema.
    let error = ApiError::new(format!("schema {schema}, this daemon speaks {SCHEMA}"), None);
    Refused { id: Some(id), error }
}

fn undecodable(error: &cbor::Error) -> ApiError {
    // alloc: error path, an envelope that does not decode.
    ApiError::new(format!("an envelope that does not decode: {error}"), None)
}

/// `envelope` in deterministic CBOR, framed (4-byte big-endian length, then the item).
pub fn encode<T: Serialize>(envelope: &Envelope<T>) -> Result<Vec<u8>, ApiError> {
    // alloc: the frame is handed out; `encode_into` writes onto a buffer the caller keeps.
    let mut frame = Vec::new();
    encode_into(&mut cbor::Writer::new(), &mut frame, envelope)?;
    Ok(frame)
}

/// [`encode`] onto the end of `out`, a buffer the caller reuses, the item written where it goes
/// and its length put before it after; on an error `out` is left as it was.
pub fn encode_into<T: Serialize>(
    writer: &mut cbor::Writer,
    out: &mut Vec<u8>,
    envelope: &Envelope<T>,
) -> Result<(), ApiError> {
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    if let Err(error) = item_into(writer, out, envelope) {
        out.truncate(start);
        return Err(error);
    }
    let len = (out.len() - start - 4) as u32;
    out[start..start + 4].copy_from_slice(&len.to_be_bytes());
    Ok(())
}

/// An envelope's keys as they are encoded, in the order of their encoding (the order
/// [`cbor`] sorts a map's keys in), the first after the head of a map of three.
const ENVELOPE_ID: [u8; 4] = [0xa3, 0x62, b'i', b'd'];
const ENVELOPE_BODY: [u8; 5] = [0x64, b'b', b'o', b'd', b'y'];
const ENVELOPE_SCHEMA: [u8; 7] = [0x66, b's', b'c', b'h', b'e', b'm', b'a'];

/// `envelope`'s item, unframed, onto the end of `out`: the bytes [`cbor::to_vec`] gives it,
/// written in order with nothing to sort. On an error `out` is left as it was.
pub(crate) fn item_into<T: Serialize>(
    writer: &mut cbor::Writer,
    out: &mut Vec<u8>,
    envelope: &Envelope<T>,
) -> Result<(), ApiError> {
    let start = out.len();
    let written = (|| {
        out.extend_from_slice(&ENVELOPE_ID);
        writer.append(out, &envelope.id)?;
        out.extend_from_slice(&ENVELOPE_BODY);
        writer.append(out, &envelope.body)?;
        out.extend_from_slice(&ENVELOPE_SCHEMA);
        writer.append(out, &envelope.schema)
    })();
    if let Err(error) = written {
        out.truncate(start);
        // alloc: error path, a value with no encoding.
        return Err(ApiError::new(format!("an envelope with no encoding: {error}"), None));
    }
    let len = out.len() - start;
    if len > blaze_protocol::LONGEST_FRAME as usize {
        out.truncate(start);
        // alloc: error path, a frame too long to send.
        let said = format!("a frame of {len} bytes, longer than {}", blaze_protocol::LONGEST_FRAME);
        return Err(ApiError::new(said, None));
    }
    Ok(())
}

#[cfg(test)]
mod identity;
#[cfg(test)]
mod tests;
