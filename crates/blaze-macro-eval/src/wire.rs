//! The bytes of the pinned bridge's protocol (`library/proc_macro/src/bridge/rpc.rs`), read
//! and written by blaze's server as the client writes and reads them. Little-endian; a
//! `usize` is eight bytes on every target (a 64-bit client's `to_le_bytes`, a wasm32 module's
//! padded to `MAX_USIZE_SIZE`, of which it reads the first four); a handle is a nonzero `u32`; a string is its length then its bytes; `Option`'s
//! tag is `Some` 0 and `None` 1, `Result`'s `Ok` 0 and `Err` 1, `Bound`'s `Included` 0,
//! `Excluded` 1 and `Unbounded` 2, each a byte. [`protocol`](crate::protocol) checks that the
//! bridge in the database is the one this is.

use std::ops::Bound;

use blaze_format::cursor::{self, Put as _};

use crate::guest::Stop;

/// A request, read from its start.
pub(crate) struct Reader<'a> {
    bytes: cursor::Reader<'a>,
}

fn short(what: &str) -> Stop {
    // alloc: error path
    Stop(format!(
        "sent the server a request cut short in {what}, which the pinned bridge never does"
    ))
}

fn bad(what: &str, value: u8) -> Stop {
    // alloc: error path
    Stop(format!("sent the server {what} {value}, which the pinned bridge never sends"))
}

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Reader<'a> {
        Reader { bytes: cursor::Reader::new(bytes) }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.bytes.is_done()
    }

    /// The bytes not read yet.
    pub(crate) fn rest(&self) -> &'a [u8] {
        self.bytes.remaining()
    }

    pub(crate) fn u8(&mut self) -> Result<u8, Stop> {
        self.bytes.u8().map_err(|_| short("a byte"))
    }

    pub(crate) fn bool(&mut self) -> Result<bool, Stop> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(bad("the bool", other)),
        }
    }

    pub(crate) fn u32(&mut self) -> Result<u32, Stop> {
        self.bytes.u32().map_err(|_| short("a u32"))
    }

    pub(crate) fn usize(&mut self) -> Result<u64, Stop> {
        self.bytes.u64().map_err(|_| short("a usize"))
    }

    pub(crate) fn handle(&mut self) -> Result<u32, Stop> {
        match self.u32()? {
            0 => Err(Stop("sent the server the handle 0, which no handle is".to_owned())), // alloc: error path
            handle => Ok(handle),
        }
    }

    pub(crate) fn str(&mut self) -> Result<&'a str, Stop> {
        let len = self.usize()?;
        let len = usize::try_from(len).map_err(|_| short("a string"))?;
        let bytes = self.bytes.take(len).map_err(|_| short("a string"))?;
        std::str::from_utf8(bytes)
            .map_err(|_| Stop("sent the server a string that is not UTF-8".to_owned())) // alloc: error path
    }

    /// The tag of an `Option`: whether it is `Some`.
    pub(crate) fn some(&mut self) -> Result<bool, Stop> {
        match self.u8()? {
            0 => Ok(true),
            1 => Ok(false),
            other => Err(bad("the Option tag", other)),
        }
    }

    pub(crate) fn bound(&mut self) -> Result<Bound<u64>, Stop> {
        match self.u8()? {
            0 => Ok(Bound::Included(self.usize()?)),
            1 => Ok(Bound::Excluded(self.usize()?)),
            2 => Ok(Bound::Unbounded),
            other => Err(bad("the Bound tag", other)),
        }
    }

    /// The length of a `Vec`.
    pub(crate) fn count(&mut self) -> Result<usize, Stop> {
        let len = self.usize()?;
        // Each element is at least a byte: a length past the bytes left is no request.
        match usize::try_from(len) {
            Ok(len) if len <= self.bytes.left() => Ok(len),
            _ => Err(short("a list")),
        }
    }
}

/// A reply, written into a buffer the server reuses.
pub(crate) struct Writer<'a> {
    pub(crate) out: &'a mut Vec<u8>,
}

impl Writer<'_> {
    /// Writes `value` over the `usize` written at `at`: a count known only once what it
    /// counts is written.
    pub(crate) fn set_usize(&mut self, at: usize, value: u64) {
        self.out[at..at + 8].copy_from_slice(&value.to_le_bytes());
    }

    pub(crate) fn u8(&mut self, value: u8) {
        self.out.put_u8(value);
    }

    pub(crate) fn bool(&mut self, value: bool) {
        self.out.put_u8(value as u8);
    }

    pub(crate) fn u32(&mut self, value: u32) {
        self.out.put_u32(value);
    }

    /// `value` in eight bytes; a wasm32 client reads the low four, and the server never writes
    /// it a length past them (each is of what the client sent or a stream in its memory).
    pub(crate) fn usize(&mut self, value: u64) {
        self.out.put_u64(value);
    }

    pub(crate) fn handle(&mut self, handle: u32) {
        self.u32(handle);
    }

    pub(crate) fn str(&mut self, value: &str) {
        self.usize(value.len() as u64);
        self.out.put_bytes(value.as_bytes());
    }

    pub(crate) fn some(&mut self, some: bool) {
        self.out.put_u8(if some { 0 } else { 1 });
    }

    pub(crate) fn ok(&mut self, ok: bool) {
        self.out.put_u8(if ok { 0 } else { 1 });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_written_is_read_back_in_the_bridges_encoding() {
        let mut out = Vec::new();
        let mut w = Writer { out: &mut out };
        w.u8(7);
        w.bool(true);
        w.handle(3);
        w.usize(1 << 40);
        w.str("héllo");
        w.some(false);
        w.some(true);
        w.ok(false);
        // `Option::Some` is tag 0, as `compound_traits!(enum Option<T> { Some(t), None })` makes it.
        assert_eq!(&out[out.len() - 3..], &[1, 0, 1]);
        let mut r = Reader::new(&out);
        assert_eq!(r.u8().unwrap(), 7);
        assert!(r.bool().unwrap());
        assert_eq!(r.handle().unwrap(), 3);
        assert_eq!(r.usize().unwrap(), 1 << 40);
        assert_eq!(r.str().unwrap(), "héllo");
        assert!(!r.some().unwrap());
        assert!(r.some().unwrap());
        assert_eq!(r.u8().unwrap(), 1);
        assert!(r.is_empty());
    }

    #[test]
    fn a_request_cut_short_or_out_of_range_is_refused() {
        assert!(Reader::new(&[1, 2]).u32().is_err());
        assert!(Reader::new(&[0, 0, 0, 0]).handle().is_err());
        assert!(Reader::new(&[2]).bool().is_err());
        assert!(Reader::new(&[3]).bound().is_err());
        let mut long = 100u64.to_le_bytes().to_vec();
        long.push(0);
        assert!(Reader::new(&long).count().is_err());
        assert!(Reader::new(&long).str().is_err());
    }

    #[test]
    fn bounds_are_read_in_the_bridges_order() {
        let mut out = vec![0];
        out.extend_from_slice(&5u64.to_le_bytes());
        out.push(1);
        out.extend_from_slice(&9u64.to_le_bytes());
        out.push(2);
        let mut r = Reader::new(&out);
        assert_eq!(r.bound().unwrap(), Bound::Included(5));
        assert_eq!(r.bound().unwrap(), Bound::Excluded(9));
        assert_eq!(r.bound().unwrap(), Bound::Unbounded);
    }

    /// Every client writes a `usize`, and so a string's length, in eight bytes: the pinned
    /// bridge pads a wasm32 one (`MAX_USIZE_SIZE`), as the request a wasm32 module sent for
    /// `TokenStream::from_str("42")` shows.
    #[test]
    fn a_usize_is_eight_bytes_from_every_client_and_a_count_is_patched_in_place() {
        let mut out = Vec::new();
        let mut w = Writer { out: &mut out };
        w.usize(0);
        w.str("42");
        w.set_usize(0, 9);
        assert_eq!(out, [9, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, b'4', b'2']);
        let mut r = Reader::new(&[2, 0, 0, 0, 0, 0, 0, 0, 0x34, 0x32]);
        assert_eq!(r.str().unwrap(), "42");
        assert!(r.is_empty());
    }
}
