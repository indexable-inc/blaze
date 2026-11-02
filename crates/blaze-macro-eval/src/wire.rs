//! The bytes of the pinned bridge's protocol (`library/proc_macro/src/bridge/rpc.rs`), read
//! and written by blaze's server as the client writes and reads them. Little-endian; a
//! `usize` is the client's pointer width ([`Width`]: `rpc_encode_decode!(le usize)` writes
//! `to_le_bytes`, so four bytes from a wasm32 module and eight from the evaluator's 64-bit
//! targets); a handle is a nonzero `u32`; a string is its length then its bytes; `Option`'s
//! tag is `Some` 0 and `None` 1, `Result`'s `Ok` 0 and `Err` 1, `Bound`'s `Included` 0,
//! `Excluded` 1 and `Unbounded` 2, each a byte. [`protocol`](crate::protocol) checks that the
//! bridge in the database is the one this is.

use std::ops::Bound;

use blaze_format::cursor::{self, Put as _};

use crate::guest::Stop;

/// The width of the client's `usize`, which every length, count and `usize` argument of the
/// protocol is written in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Width {
    /// A wasm32 module's client.
    W32,
    /// The evaluator's client, on a 64-bit target.
    W64,
}

impl Width {
    pub(crate) const fn bytes(self) -> usize {
        match self {
            Width::W32 => 4,
            Width::W64 => 8,
        }
    }
}

/// A request, read from its start.
pub(crate) struct Reader<'a> {
    bytes: cursor::Reader<'a>,
    width: Width,
}

fn short(what: &str) -> Stop {
    // alloc: error path
    Stop(format!("sent the server a request cut short in {what}, which the pinned bridge never does"))
}

fn bad(what: &str, value: u8) -> Stop {
    // alloc: error path
    Stop(format!("sent the server {what} {value}, which the pinned bridge never sends"))
}

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8], width: Width) -> Reader<'a> {
        Reader { bytes: cursor::Reader::new(bytes), width }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.bytes.is_done()
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
        match self.width {
            Width::W32 => self.bytes.u32().map(u64::from),
            Width::W64 => self.bytes.u64(),
        }
        .map_err(|_| short("a usize"))
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
    pub(crate) width: Width,
}

impl Writer<'_> {
    /// Writes `value` over the `usize` written at `at`: a count known only once what it
    /// counts is written.
    pub(crate) fn set_usize(&mut self, at: usize, value: u64) {
        let len = self.width.bytes();
        self.out[at..at + len].copy_from_slice(&value.to_le_bytes()[..len]);
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

    /// `value` in the client's width; one that does not fit a wasm32 `usize` is a length no
    /// wasm32 client can hold, which the server never writes for it (each is of what the
    /// client sent or a stream in its memory).
    pub(crate) fn usize(&mut self, value: u64) {
        match self.width {
            Width::W32 => self.out.put_u32(value as u32),
            Width::W64 => self.out.put_u64(value),
        }
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
        let mut w = Writer { out: &mut out, width: Width::W64 };
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
        let mut r = Reader::new(&out, Width::W64);
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
        assert!(Reader::new(&[1, 2], Width::W64).u32().is_err());
        assert!(Reader::new(&[0, 0, 0, 0], Width::W64).handle().is_err());
        assert!(Reader::new(&[2], Width::W64).bool().is_err());
        assert!(Reader::new(&[3], Width::W64).bound().is_err());
        let mut long = 100u64.to_le_bytes().to_vec();
        long.push(0);
        assert!(Reader::new(&long, Width::W64).count().is_err());
        assert!(Reader::new(&long, Width::W64).str().is_err());
    }

    #[test]
    fn bounds_are_read_in_the_bridges_order() {
        let mut out = vec![0];
        out.extend_from_slice(&5u64.to_le_bytes());
        out.push(1);
        out.extend_from_slice(&9u64.to_le_bytes());
        out.push(2);
        let mut r = Reader::new(&out, Width::W64);
        assert_eq!(r.bound().unwrap(), Bound::Included(5));
        assert_eq!(r.bound().unwrap(), Bound::Excluded(9));
        assert_eq!(r.bound().unwrap(), Bound::Unbounded);
    }

    /// A wasm32 client writes a `usize`, and so a string's length, in four bytes.
    #[test]
    fn a_wasm32_clients_usize_is_four_bytes_and_a_count_is_patched_in_its_width() {
        let mut out = Vec::new();
        let mut w = Writer { out: &mut out, width: Width::W32 };
        w.usize(0);
        w.str("ab");
        w.set_usize(0, 9);
        assert_eq!(out, [9, 0, 0, 0, 2, 0, 0, 0, b'a', b'b']);
        let mut r = Reader::new(&out, Width::W32);
        assert_eq!(r.usize().unwrap(), 9);
        assert_eq!(r.str().unwrap(), "ab");
        assert!(r.is_empty());
    }
}
