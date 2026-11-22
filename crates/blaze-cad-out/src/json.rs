//! A JSON emitter (RFC 8259) on blaze-text: maps, lists, keys and values in the order they are
//! called, commas placed by the emitter, no whitespace. Numbers are written in the shortest form
//! that reads back as the same `f64` (blaze-text's `float`: plain decimal, no exponent, `-0`
//! written as `0`); strings are escaped per RFC 8259 (blaze-text's `json`). Nothing goes through
//! `core::fmt`.
//!
//! A misuse (a value where a key is due, a close that does not match, a number that is not
//! finite, nesting past [`MOST_DEPTH`]) is kept as the first [`JsonFault`], everything after it
//! is dropped, and [`Json::finish`] reports it: the caller's bug, typed, never broken text.

#![blaze::sans_io]

use blaze_text::{Put, Writes};

/// How deep maps and lists may nest: one bit of state per level, in a `u64`.
pub(crate) const MOST_DEPTH: u32 = 63;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JsonFault {
    /// A number that is NaN or infinite: JSON has no such value.
    NonFinite,
    /// Nesting past [`MOST_DEPTH`].
    TooDeep,
    /// A key outside a map, a value in a map without its key, or a close that does not match
    /// what is open.
    Misplaced,
    /// Finished with a map or list still open, or a key without its value.
    Unbalanced,
}

pub(crate) struct Json<'w, P: Put> {
    out: &'w mut P,
    depth: u32,
    /// Bit `d`: a value has been written at depth `d`, so the next one there takes a comma.
    written: u64,
    /// Bit `d`: depth `d` is a map.
    maps: u64,
    after_key: bool,
    fault: Option<JsonFault>,
}

impl<'w, P: Put> Json<'w, P> {
    pub(crate) fn new(out: &'w mut P) -> Json<'w, P> {
        Json { out, depth: 0, written: 0, maps: 0, after_key: false, fault: None }
    }

    fn fail(&mut self, fault: JsonFault) {
        if self.fault.is_none() {
            self.fault = Some(fault);
        }
    }

    fn in_map(&self) -> bool {
        (self.maps >> self.depth) & 1 == 1
    }

    /// The comma before a value where one is due; whether the value may be written.
    fn before_value(&mut self) -> bool {
        if self.fault.is_some() {
            return false;
        }
        if self.after_key {
            self.after_key = false;
            return true;
        }
        if self.in_map() {
            self.fail(JsonFault::Misplaced);
            return false;
        }
        let bit = 1u64 << self.depth;
        if self.written & bit != 0 {
            self.out.put_byte(b',');
        }
        self.written |= bit;
        true
    }

    /// `"key":` in the open map.
    pub(crate) fn key(&mut self, key: &str) -> &mut Self {
        if self.fault.is_some() {
            return self;
        }
        if !self.in_map() || self.after_key {
            self.fail(JsonFault::Misplaced);
            return self;
        }
        let bit = 1u64 << self.depth;
        if self.written & bit != 0 {
            self.out.put_byte(b',');
        }
        self.written |= bit;
        self.out.json_str(key).put_byte(b':');
        self.after_key = true;
        self
    }

    fn open(&mut self, map: bool) -> &mut Self {
        if !self.before_value() {
            return self;
        }
        if self.depth >= MOST_DEPTH {
            self.fail(JsonFault::TooDeep);
            return self;
        }
        self.out.put_byte(if map { b'{' } else { b'[' });
        self.depth += 1;
        let bit = 1u64 << self.depth;
        self.written &= !bit;
        if map {
            self.maps |= bit;
        } else {
            self.maps &= !bit;
        }
        self
    }

    fn close(&mut self, map: bool) -> &mut Self {
        if self.fault.is_some() {
            return self;
        }
        if self.depth == 0 || self.in_map() != map || self.after_key {
            self.fail(JsonFault::Misplaced);
            return self;
        }
        self.out.put_byte(if map { b'}' } else { b']' });
        self.depth -= 1;
        self
    }

    pub(crate) fn map(&mut self) -> &mut Self {
        self.open(true)
    }

    pub(crate) fn end_map(&mut self) -> &mut Self {
        self.close(true)
    }

    pub(crate) fn list(&mut self) -> &mut Self {
        self.open(false)
    }

    pub(crate) fn end_list(&mut self) -> &mut Self {
        self.close(false)
    }

    pub(crate) fn str(&mut self, text: &str) -> &mut Self {
        if self.before_value() {
            self.out.json_str(text);
        }
        self
    }

    /// One string written from pieces: `base`, then `suffix`, then `number` when given
    /// (`"hull.lod2"`), each escaped.
    pub(crate) fn str_from(&mut self, base: &str, suffix: &str, number: Option<u32>) -> &mut Self {
        if self.before_value() {
            self.out.put_byte(b'"');
            blaze_text::escape::json(base, &mut *self.out);
            blaze_text::escape::json(suffix, &mut *self.out);
            if let Some(n) = number {
                self.out.int(n);
            }
            self.out.put_byte(b'"');
        }
        self
    }

    pub(crate) fn uint(&mut self, n: u64) -> &mut Self {
        if self.before_value() {
            self.out.int(n);
        }
        self
    }

    pub(crate) fn bool(&mut self, b: bool) -> &mut Self {
        if self.before_value() {
            self.out.bool(b);
        }
        self
    }

    /// `x` in its shortest round-trip form; `-0` as `0`; a NaN or infinity is a fault.
    pub(crate) fn num(&mut self, x: f64) -> &mut Self {
        if self.fault.is_some() {
            return self;
        }
        if !x.is_finite() {
            self.fail(JsonFault::NonFinite);
            return self;
        }
        if self.before_value() {
            self.out.float(if x == 0.0 { 0.0 } else { x });
        }
        self
    }

    /// A list of numbers.
    pub(crate) fn nums(&mut self, xs: &[f64]) -> &mut Self {
        self.list();
        for &x in xs {
            self.num(x);
        }
        self.end_list()
    }

    /// A list of unsigned integers.
    pub(crate) fn uints(&mut self, xs: impl IntoIterator<Item = u64>) -> &mut Self {
        self.list();
        for x in xs {
            self.uint(x);
        }
        self.end_list()
    }

    /// Done: every map and list closed and no fault.
    pub(crate) fn finish(&self) -> Result<(), JsonFault> {
        if let Some(fault) = self.fault {
            return Err(fault);
        }
        if self.depth != 0 || self.after_key {
            return Err(JsonFault::Unbalanced);
        }
        Ok(())
    }
}
