//! The bodies of the global cache's HTTP requests and responses: fixed-width, little-endian,
//! each starting with a magic of its own and the version.
//!
//! - **fetch** (`POST /v1/fetch`): `BLZGFQ`, then the number of keys as a `u32` (at most
//!   [`BATCH_MOST`]) and the keys. Answered by `BLZGFA` and one answer a key, in the order
//!   asked: `0` and the key (missing), `1` and the key (deferred: the response was full; ask
//!   again), or `2`, the [`Signed`] record and the value as pieces.
//! - **publish** (`POST /v1/publish`): `BLZGPQ`, the number of values, and each value's signed
//!   record and pieces. Answered by `BLZGPA` and, for each, its key and `0` (kept) or `1` and
//!   why, as a `u16` length and at most [`REASON_MOST`] bytes of UTF-8.
//! - **bundle** (`POST /v1/bundle`): `BLZGBQ` and a [`crate::BundleId`]; answered by `BLZGBA`
//!   and `0` (missing) or `2` and its [`crate::Manifest`]. **bundle publish**
//!   (`POST /v1/bundle/publish`): `BLZGBP` and the manifest; answered as a publish of one.
//!
//! **Pieces**: a value is its count of pieces as a `u32`, then each piece: `0`, its length as a
//! `u32` (at most [`PIECE_MOST`]) and its bytes, or `1` and the sequence number of a piece sent
//! before in the same direction of the same connection. A value of [`CHUNKED_FROM`] bytes or
//! more is cut into the store's content-defined chunks ([`blaze_cas::chunk::chunks`]); a
//! shorter one is one piece. Each side keeps the last [`WINDOW`] bytes of pieces sent or
//! received in each direction, oldest out first, by the same rule ([`SendWindow`],
//! [`RecvWindow`]), so the sender refers only to pieces the receiver still holds.
//!
//! **Bounds**: every count and length is checked against what the body still holds or a
//! limit before anything is allocated for it. A value's length is a record's, and a record is
//! believed only once its signature is checked, so nothing is allocated for a value of a
//! record no pinned publisher signed.

use std::collections::VecDeque;

use blaze_cas::Key;
use blaze_format::cursor::{self, Put as _, Reader};
use blaze_ids::{ContentId, Domain};

use crate::{
    ByHash,
    sign::{SIGNED_LEN, Signed},
};

/// Whether a batch holding `bytes` of values takes a next of `len`.
pub(crate) fn takes(bytes: u64, len: u64) -> bool {
    bytes == 0 || bytes + len <= BATCH_BYTES
}

pub const VERSION: u16 = 1;

/// The most bytes of one value: the remote tier's bound.
pub const LONGEST_VALUE: u64 = blaze_cas::remote::LONGEST_VALUE;
/// The most keys one fetch names.
pub const BATCH_MOST: usize = 4096;
/// A fetch's answer, or a publish, carries values of about this many bytes: it takes its
/// first value whatever its length, then the next while they are no more than this. Four
/// in flight on a connection are more than its receive window holds, so the pipe stays full,
/// and a client of [`crate::client::CONNECTIONS`] buffers a bounded amount.
pub const BATCH_BYTES: u64 = 8 << 20;
/// The most bytes of a body: a batch whose first value is the longest, and its framing.
pub const BODY_MOST: u64 = LONGEST_VALUE + BATCH_BYTES + (8 << 20);
/// The most bytes of one piece: more than any chunk the store cuts.
pub const PIECE_MOST: usize = 1 << 20;
/// Values from this long on travel as content-defined chunks.
pub const CHUNKED_FROM: usize = 64 << 10;
/// The bytes of pieces each direction of a connection keeps.
pub const WINDOW: u64 = 32 << 20;
pub const REASON_MOST: usize = 1024;

pub(crate) const FETCH_ASK: &[u8; 6] = b"BLZGFQ";
pub(crate) const FETCH_ANSWER: &[u8; 6] = b"BLZGFA";
pub(crate) const PUBLISH_ASK: &[u8; 6] = b"BLZGPQ";
pub(crate) const PUBLISH_ANSWER: &[u8; 6] = b"BLZGPA";
pub(crate) const BUNDLE_ASK: &[u8; 6] = b"BLZGBQ";
pub(crate) const BUNDLE_ANSWER: &[u8; 6] = b"BLZGBA";
pub(crate) const BUNDLE_PUBLISH: &[u8; 6] = b"BLZGBP";

pub(crate) const MISSING: u8 = 0;
pub(crate) const DEFERRED: u8 = 1;
pub(crate) const FOUND: u8 = 2;
pub(crate) const KEPT: u8 = 0;
pub(crate) const REFUSED: u8 = 1;
const NEW: u8 = 0;
const SEEN: u8 = 1;

const PIECE_DOMAIN: &str = "blaze v2 global piece 1";

pub(crate) fn header(magic: &[u8; 6], out: &mut Vec<u8>) {
    out.put_bytes(magic);
    out.put_u16(VERSION);
}

/// A read past the end of a body, in the words the wire's errors use.
pub(crate) fn truncated(error: cursor::Error) -> String {
    // alloc: an error path.
    format!("truncated: {error}")
}

/// The wire's own fields, read with the byte cursor.
pub(crate) trait Fields {
    fn key(&mut self) -> Result<Key, String>;
    fn signed(&mut self) -> Result<Signed, String>;
    /// The magic and version a body starts with.
    fn header(&mut self, magic: &[u8; 6], what: &str) -> Result<(), String>;
    /// A `u32` count of at most `most` items of at least `each` bytes, checked against what is
    /// left before anything is allocated for them.
    fn count(&mut self, each: usize, most: usize, what: &str) -> Result<usize, String>;
    fn end(&self, what: &str) -> Result<(), String>;
}

impl Fields for Reader<'_> {
    fn key(&mut self) -> Result<Key, String> {
        Ok(Key(ContentId(*self.array().map_err(truncated)?)))
    }

    fn signed(&mut self) -> Result<Signed, String> {
        Signed::decode(self.array::<SIGNED_LEN>().map_err(truncated)?)
    }

    fn header(&mut self, magic: &[u8; 6], what: &str) -> Result<(), String> {
        let found = self.array::<6>().map_err(truncated)?;
        if found != magic {
            // alloc: an error's message, made only when it fails.
            return Err(format!("not {what}: its magic is {:?}", found.escape_ascii().to_string()));
        }
        let version = self.u16().map_err(truncated)?;
        if version != VERSION {
            // alloc: an error's message, made only when it fails.
            return Err(format!("{what} of version {version}; this blaze speaks version {VERSION}"));
        }
        Ok(())
    }

    fn count(&mut self, each: usize, most: usize, what: &str) -> Result<usize, String> {
        let mut peek = *self;
        let count = peek.u32().map_err(truncated)? as usize;
        if count > most {
            // alloc: an error's message, made only when it fails.
            return Err(format!("{count} {what}, more than the {most} allowed"));
        }
        self.count_u32(each).map_err(truncated)
    }

    fn end(&self, what: &str) -> Result<(), String> {
        match self.left() {
            0 => Ok(()),
            // alloc: an error's message, made only when it fails.
            left => Err(format!("{left} trailing bytes after {what}")),
        }
    }
}

/// `f` of the pieces `value` travels as: none when empty, the whole of it under
/// [`CHUNKED_FROM`] bytes, else blaze-cas's chunks.
pub(crate) fn with_pieces<R>(value: &[u8], f: impl FnOnce(&[std::ops::Range<usize>]) -> R) -> R {
    if value.is_empty() {
        f(&[])
    } else if value.len() < CHUNKED_FROM {
        f(&[0..value.len()])
    } else {
        // alloc: blaze-cas's chunk list, once per value of `CHUNKED_FROM` bytes or more.
        f(&blaze_cas::chunk::chunks(value))
    }
}

static PIECE: Domain = Domain::new(PIECE_DOMAIN);

/// The pieces one direction of a connection sent, as the receiver holds them.
#[derive(Default)]
pub(crate) struct SendWindow {
    by_hash: ByHash<ContentId, u64>,
    held: VecDeque<(ContentId, u32)>,
    first: u64,
    bytes: u64,
}

impl SendWindow {
    /// `value` as its pieces: each one held a reference, the rest their bytes, then held.
    pub(crate) fn write(&mut self, value: &[u8], out: &mut Vec<u8>) {
        with_pieces(value, |pieces| self.write_pieces(value, pieces, out))
    }

    fn write_pieces(&mut self, value: &[u8], pieces: &[std::ops::Range<usize>], out: &mut Vec<u8>) {
        out.put_u32(pieces.len() as u32);
        for range in pieces.iter().cloned() {
            let piece = &value[range];
            let hash = ContentId::of_domain(&PIECE, piece);
            if let Some(&seq) = self.by_hash.get(&hash) {
                out.put_u8(SEEN);
                out.put_u64(seq);
                continue;
            }
            out.put_u8(NEW);
            out.put_bytes_u32(piece);
            let seq = self.first + self.held.len() as u64;
            self.by_hash.insert(hash, seq);
            self.held.push_back((hash, piece.len() as u32));
            self.bytes += piece.len() as u64;
            while self.bytes > WINDOW {
                let (old, len) = self.held.pop_front().expect("a window over its bytes holds a piece");
                self.by_hash.remove(&old);
                self.bytes -= u64::from(len);
                self.first += 1;
            }
        }
    }
}

/// The pieces one direction of a connection received.
#[derive(Default)]
pub(crate) struct RecvWindow {
    /// The pieces held, oldest first, end to end; `kept[0]` is byte `base` of all the pieces
    /// this side received.
    kept: Vec<u8>,
    base: u64,
    /// Each piece held: where it starts among all received, and its length.
    held: VecDeque<(u64, u32)>,
    first: u64,
    bytes: u64,
}

impl RecvWindow {
    /// A value of `len` bytes from its pieces: put together when `keep`, else only read past
    /// (a value refused still moves the window as its sender's moved). `len` is a signed
    /// record's when `keep`, so the value's buffer is allocated once at its length.
    pub(crate) fn read(&mut self, reader: &mut Reader<'_>, len: u64, keep: bool) -> Result<Option<Vec<u8>>, String> {
        if len > LONGEST_VALUE {
            // alloc: an error's message, made only when it fails.
            return Err(format!("a value of {len} bytes, longer than the {LONGEST_VALUE} allowed"));
        }
        let count = reader.count(2, usize::MAX, "pieces")?;
        if count as u64 > len {
            // alloc: an error's message, made only when it fails.
            return Err(format!("{count} pieces of a value of {len} bytes"));
        }
        let mut value = keep.then(|| Vec::with_capacity(len as usize));
        let mut total = 0u64;
        for _ in 0..count {
            let piece: &[u8] = match reader.u8().map_err(truncated)? {
                NEW => {
                    let n = reader.u32().map_err(truncated)? as usize;
                    if n == 0 || n > PIECE_MOST {
                        // alloc: an error's message, made only when it fails.
                        return Err(format!("a piece of {n} bytes (1 to {PIECE_MOST} are allowed)"));
                    }
                    let bytes = reader.take(n).map_err(truncated)?;
                    self.hold(bytes);
                    bytes
                }
                SEEN => {
                    let seq = reader.u64().map_err(truncated)?;
                    let at = seq.checked_sub(self.first).filter(|at| *at < self.held.len() as u64).ok_or_else(|| {
                        // alloc: an error's message, made only when it fails.
                        format!(
                            "a reference to piece {seq}, outside the {} to {} this side holds",
                            self.first,
                            self.first + self.held.len() as u64
                        )
                    })?;
                    let (start, len) = self.held[at as usize];
                    &self.kept[(start - self.base) as usize..][..len as usize]
                }
                // alloc: an error's message, made only when it fails.
                other => return Err(format!("an unknown piece {other} (0 is new, 1 seen)")),
            };
            total += piece.len() as u64;
            if total > len {
                // alloc: an error's message, made only when it fails.
                return Err(format!("its pieces are more than the {len} bytes of its record"));
            }
            if let Some(value) = &mut value {
                value.extend_from_slice(piece);
            }
        }
        if total != len {
            // alloc: an error's message, made only when it fails.
            return Err(format!("its pieces are {total} bytes, not the {len} of its record"));
        }
        Ok(value)
    }

    fn hold(&mut self, bytes: &[u8]) {
        let end = self.base + self.kept.len() as u64;
        self.kept.extend_from_slice(bytes);
        self.held.push_back((end, bytes.len() as u32));
        self.bytes += bytes.len() as u64;
        while self.bytes > WINDOW {
            let (_, len) = self.held.pop_front().expect("a window over its bytes holds a piece");
            self.bytes -= u64::from(len);
            self.first += 1;
        }
        // The pieces dropped are the buffer's front: cut them once they are half of it, so a
        // byte moves at most once per byte received and the buffer stays under twice the window.
        let live = self.held.front().map_or(self.base + self.kept.len() as u64, |it| it.0);
        let dead = (live - self.base) as usize;
        if dead * 2 > self.kept.len() {
            self.kept.drain(..dead);
            self.base = live;
        }
    }
}

/// A fetch's body.
pub(crate) fn fetch_ask(keys: &[Key], out: &mut Vec<u8>) {
    fetch_ask_with(keys.len(), keys.iter().copied(), out);
}

/// A fetch's body naming the `count` keys of `keys`.
pub(crate) fn fetch_ask_with(count: usize, keys: impl Iterator<Item = Key>, out: &mut Vec<u8>) {
    out.reserve(12 + 32 * count);
    header(FETCH_ASK, out);
    out.put_u32(count as u32);
    for key in keys {
        out.put_bytes(&key.0.0);
    }
}

pub(crate) fn parse_fetch_ask(body: &[u8]) -> Result<Vec<Key>, String> {
    parse_fetch_ask_in(body, std::alloc::Global)
}

/// [`parse_fetch_ask`] into `alloc`: the server's on the request's frame.
pub(crate) fn parse_fetch_ask_in<A: std::alloc::Allocator>(body: &[u8], alloc: A) -> Result<Vec<Key, A>, String> {
    let mut reader = Reader::new(body);
    reader.header(FETCH_ASK, "a fetch")?;
    let count = reader.count(32, BATCH_MOST, "keys")?;
    let mut keys = Vec::with_capacity_in(count, alloc);
    for _ in 0..count {
        keys.push(reader.key()?);
    }
    reader.end("a fetch")?;
    Ok(keys)
}

pub(crate) fn reason(why: &str, out: &mut Vec<u8>) {
    let mut end = why.len().min(REASON_MOST);
    while !why.is_char_boundary(end) {
        end -= 1;
    }
    out.put_u16(end as u16);
    out.put_bytes(&why.as_bytes()[..end]);
}

pub(crate) fn read_reason(reader: &mut Reader<'_>) -> Result<String, String> {
    let len = reader.u16().map_err(truncated)? as usize;
    if len > REASON_MOST {
        // alloc: an error's message, made only when it fails.
        return Err(format!("a reason of {len} bytes, more than the {REASON_MOST} allowed"));
    }
    let bytes = reader.take(len).map_err(truncated)?;
    // alloc: an error's message, made only when it fails.
    String::from_utf8(bytes.to_vec()).map_err(|_| "a reason that is not UTF-8".to_owned())
}

/// What a publish answered for each value, in order.
pub(crate) fn parse_publish_answer(body: &[u8], most: usize) -> Result<Vec<(Key, Result<(), String>)>, String> {
    let mut reader = Reader::new(body);
    reader.header(PUBLISH_ANSWER, "a publish's answer")?;
    let count = reader.count(33, most, "statuses")?;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let key = reader.key()?;
        let status = match reader.u8().map_err(truncated)? {
            KEPT => Ok(()),
            REFUSED => Err(read_reason(&mut reader)?),
            // alloc: an error's message, made only when it fails.
            other => return Err(format!("an unknown status {other} (0 is kept, 1 refused)")),
        };
        out.push((key, status));
    }
    reader.end("a publish's answer")?;
    Ok(out)
}

pub(crate) fn publish_answer(statuses: &[(Key, Result<(), String>)], out: &mut Vec<u8>) {
    header(PUBLISH_ANSWER, out);
    out.put_u32(statuses.len() as u32);
    for (key, status) in statuses {
        out.put_bytes(&key.0.0);
        match status {
            Ok(()) => out.put_u8(KEPT),
            Err(why) => {
                out.put_u8(REFUSED);
                reason(why, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use blaze_ids::rng::SplitMix64 as Rng;

    use super::*;

    fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut rng = Rng::new(seed);
        (0..len).map(|_| rng.next_u64() as u8).collect()
    }

    fn values() -> Vec<Vec<u8>> {
        let big = noise(3 << 20, 1);
        let mut near = big.clone();
        near[1 << 20] ^= 0xff;
        near.splice(2 << 20..2 << 20, noise(100, 2));
        vec![Vec::new(), b"x".to_vec(), noise(CHUNKED_FROM - 1, 3), big.clone(), near, big, noise(70 << 10, 4)]
    }

    #[test]
    fn values_round_trip_through_the_windows() {
        let (mut send, mut recv) = (SendWindow::default(), RecvWindow::default());
        let mut wire = Vec::new();
        for value in values() {
            send.write(&value, &mut wire);
        }
        let mut reader = Reader::new(&wire);
        for value in values() {
            assert_eq!(recv.read(&mut reader, value.len() as u64, true).unwrap(), Some(value));
        }
        reader.end("the values").unwrap();
    }

    #[test]
    fn near_equal_values_share_their_pieces() {
        let values = values();
        let mut alone = 0;
        for value in &values {
            let mut out = Vec::new();
            SendWindow::default().write(value, &mut out);
            alone += out.len();
        }
        let mut shared = Vec::new();
        let mut send = SendWindow::default();
        for value in &values {
            send.write(value, &mut shared);
        }
        let big = 3 << 20;
        assert!(shared.len() < alone - big - big / 2, "{} of {alone}", shared.len());
    }

    #[test]
    fn a_window_forgets_as_its_peer_does() {
        let (mut send, mut recv) = (SendWindow::default(), RecvWindow::default());
        let mut wire = Vec::new();
        let first = noise(1 << 20, 9);
        let values: Vec<Vec<u8>> = std::iter::once(first.clone())
            .chain((0..40).map(|n| noise(1 << 20, 100 + n)))
            .chain(std::iter::once(first))
            .collect();
        for value in &values {
            send.write(value, &mut wire);
        }
        let mut reader = Reader::new(&wire);
        for value in &values {
            assert_eq!(recv.read(&mut reader, value.len() as u64, true).unwrap().as_ref(), Some(value));
        }
        assert!(recv.bytes <= WINDOW && recv.first > 0 && recv.first == send.first);
    }

    #[test]
    fn damaged_pieces_are_refused() {
        let value = noise(CHUNKED_FROM * 4, 5);
        let mut wire = Vec::new();
        let mut send = SendWindow::default();
        send.write(&value, &mut wire);
        send.write(&value, &mut wire);
        let first = wire.len() - (4 + 9 * with_pieces(&value, <[_]>::len));
        let read = |bytes: &[u8], len: u64| {
            let mut recv = RecvWindow::default();
            let mut reader = Reader::new(bytes);
            recv.read(&mut reader, len, true).map(|_| ())
        };
        assert!(read(&wire[..first], value.len() as u64 + 1).unwrap_err().contains("not the"));
        assert!(read(&wire[..first], value.len() as u64 - 1).unwrap_err().contains("more than"));
        for cut in [0, 3, 4, 5, 9, first - 1] {
            assert!(read(&wire[..cut], value.len() as u64).unwrap_err().contains("truncated"), "{cut}");
        }
        let mut unknown = wire[..first].to_vec();
        unknown[4] = 7;
        assert!(read(&unknown, value.len() as u64).unwrap_err().contains("unknown piece 7"));
        let mut huge = wire[..first].to_vec();
        huge[5..9].copy_from_slice(&((PIECE_MOST + 1) as u32).to_le_bytes());
        assert!(read(&huge, value.len() as u64).unwrap_err().contains("allowed"));
        let mut recv = RecvWindow::default();
        assert!(recv.read(&mut Reader::new(&wire[first..]), value.len() as u64, true).unwrap_err().contains("outside"));
        let mut many = Vec::new();
        many.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(read(&many, 5).unwrap_err().contains("truncated"));
        assert!(read(&[0; 4], LONGEST_VALUE + 1).unwrap_err().contains("longer than"));
    }

    #[test]
    fn a_value_read_past_moves_the_window_as_one_kept() {
        let values = values();
        let mut wire = Vec::new();
        let mut send = SendWindow::default();
        for value in &values {
            send.write(value, &mut wire);
        }
        let mut recv = RecvWindow::default();
        let mut reader = Reader::new(&wire);
        for (n, value) in values.iter().enumerate() {
            let read = recv.read(&mut reader, value.len() as u64, n % 2 == 0).unwrap();
            assert_eq!(read.is_some(), n % 2 == 0);
        }
        reader.end("the values").unwrap();
    }

    #[test]
    fn every_chunk_the_store_cuts_is_a_piece_the_wire_takes() {
        for seed in 0..4 {
            let value = noise(4 << 20, 50 + seed);
            for range in with_pieces(&value, <[_]>::to_vec) {
                assert!(range.len() <= PIECE_MOST && !range.is_empty());
            }
        }
    }

    #[test]
    fn a_value_the_window_holds_travels_with_no_allocation() {
        let value = vec![7u8; 1000];
        let (mut send, mut recv) = (SendWindow::default(), RecvWindow::default());
        let mut wire = Vec::new();
        send.write(&value, &mut wire);
        recv.read(&mut Reader::new(&wire), value.len() as u64, false).unwrap();
        wire.clear();
        let ((), used) = blaze_alloc_count::measure(|| send.write(&value, &mut wire));
        assert_eq!(used, blaze_alloc_count::Used::default(), "a piece the receiver holds goes as its number");
        let (read, used) =
            blaze_alloc_count::measure(|| recv.read(&mut Reader::new(&wire), value.len() as u64, false).unwrap());
        assert_eq!(read, None);
        assert_eq!(used, blaze_alloc_count::Used::default(), "a value refused is read past with no copy");
        let (read, used) =
            blaze_alloc_count::measure(|| recv.read(&mut Reader::new(&wire), value.len() as u64, true).unwrap());
        assert_eq!(read.as_deref(), Some(&value[..]));
        assert_eq!(used.allocations, 1, "a value kept is one buffer of its length");
    }

    #[test]
    fn a_warm_window_holds_new_pieces_with_no_allocation() {
        // Under `CHUNKED_FROM`, so each value is one piece; each distinct, so each is new.
        const LEN: usize = 60 << 10;
        let piece = |n: u64| {
            let mut it = vec![0u8; LEN];
            it[..8].copy_from_slice(&n.to_le_bytes());
            it
        };
        let (mut send, mut recv) = (SendWindow::default(), RecvWindow::default());
        let mut wire = Vec::new();
        // Four windows' worth: the buffer has cut its front more than once and reached its
        // steady size (under twice the window and a piece).
        let rounds = 4 * (WINDOW as usize / LEN) + 2;
        for n in 0..rounds as u64 {
            wire.clear();
            send.write(&piece(n), &mut wire);
            recv.read(&mut Reader::new(&wire), LEN as u64, false).unwrap();
        }
        wire.clear();
        send.write(&piece(u64::MAX), &mut wire);
        let (read, used) =
            blaze_alloc_count::measure(|| recv.read(&mut Reader::new(&wire), LEN as u64, false).unwrap());
        assert_eq!(read, None);
        assert_eq!(used, blaze_alloc_count::Used::default(), "a new piece in a warm window");
    }

    #[test]
    fn a_fetch_parsed_onto_a_frame_allocates_nothing() {
        let keys: Vec<Key> = (0..64u8).map(|i| Key(ContentId([i; 32]))).collect();
        let mut body = Vec::new();
        fetch_ask(&keys, &mut body);
        blaze_mem::frame::task(blaze_mem::Tag::Store, |temp| parse_fetch_ask_in(&body, temp).map(drop)).unwrap();
        let (parsed, used) = blaze_alloc_count::measure(|| {
            blaze_mem::frame::task(blaze_mem::Tag::Store, |temp| parse_fetch_ask_in(&body, temp).map(|it| it.to_vec()))
        });
        assert_eq!(parsed.unwrap(), keys);
        assert_eq!(used.allocations, 1, "only the test's copy out of the frame");
    }

    #[test]
    fn fetch_asks_and_publish_answers_round_trip_and_refuse_what_they_are_not() {
        let keys: Vec<Key> = (0..5).map(|n| Key(ContentId([n; 32]))).collect();
        let mut body = Vec::new();
        fetch_ask(&keys, &mut body);
        assert_eq!(parse_fetch_ask(&body).unwrap(), keys);
        for cut in 0..body.len() {
            assert!(parse_fetch_ask(&body[..cut]).is_err());
        }
        let mut trailing = body.clone();
        trailing.push(0);
        assert!(parse_fetch_ask(&trailing).unwrap_err().contains("trailing"));
        let mut many = Vec::new();
        header(FETCH_ASK, &mut many);
        many.extend_from_slice(&((BATCH_MOST + 1) as u32).to_le_bytes());
        assert!(parse_fetch_ask(&many).unwrap_err().contains("more than"));
        let mut version = body.clone();
        version[6] = 9;
        assert!(parse_fetch_ask(&version).unwrap_err().contains("version 9"));

        let long = "é".repeat(REASON_MOST);
        let statuses = vec![(keys[0], Ok(())), (keys[1], Err("no".to_owned())), (keys[2], Err(long.clone()))];
        let mut answer = Vec::new();
        publish_answer(&statuses, &mut answer);
        let parsed = parse_publish_answer(&answer, 3).unwrap();
        assert_eq!(parsed[..2], statuses[..2]);
        let Err(cut) = &parsed[2].1 else { panic!() };
        assert!(cut.len() <= REASON_MOST && long.starts_with(cut.as_str()));
        assert!(parse_publish_answer(&answer, 2).unwrap_err().contains("more than"));
    }
}
