//! Values to their one encoding, written in place: every item goes straight onto the end of the
//! output as serde hands it over, with no buffer of its own. A map whose keys come in the order
//! of their encoding (a struct declared in that order) is done when its last entry is; one whose
//! keys come out of order has its entries put in order once, at its end, through a scratch
//! buffer the [`Writer`] keeps for the next. A sequence or map that did not say its length up
//! front gets a one-byte head, widened in place at its end in the rare case it holds 24 items
//! or more.

use std::cmp::Ordering;

use serde::ser::{self, Serialize};

use super::{
    ARRAY, BYTES, Error, FALSE, MAP, NEGATIVE, NULL, TEXT, TRUE, UNSIGNED, float, head, head_bytes,
    key_order, name_order,
};

/// The deterministic encoding of `value`.
pub fn to_vec<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, Error> {
    // alloc: the encoding is handed out; `Writer::append` writes into a buffer kept by the caller.
    let mut out = Vec::new();
    Writer::new().append(&mut out, value)?;
    Ok(out)
}

/// `value`'s encoding added to the end of `out`; on an error `out` is left as it was.
pub fn append<T: Serialize + ?Sized>(out: &mut Vec<u8>, value: &T) -> Result<(), Error> {
    Writer::new().append(out, value)
}

/// An encoder kept from one value to the next, so the maps it puts in order reuse its scratch
/// space rather than allocate.
#[derive(Default, Debug)]
pub struct Writer {
    scratch: Scratch,
}

/// The most scratch bytes a [`Writer`] keeps between values: a map of a big file put in order
/// once does not hold its size for good.
const KEPT_SCRATCH: usize = 1 << 20;

#[derive(Default, Debug)]
struct Scratch {
    /// The entries of the maps being put in order, innermost last: where each key starts and
    /// ends in the output, and where its entry ends.
    spans: Vec<Span>,
    /// A map's entries in order, before they go back.
    bytes: Vec<u8>,
    permuted: u64,
}

/// Three words (24 bytes on 64-bit) per entry of a map written out of order; none for one in
/// order.
#[derive(Clone, Copy, Debug)]
struct Span {
    key: usize,
    key_end: usize,
    end: usize,
}

const _: () = assert!(size_of::<Span>() == 3 * size_of::<usize>());

impl Writer {
    pub fn new() -> Writer {
        Writer::default()
    }

    /// `value`'s encoding added to the end of `out`; on an error `out` is left as it was.
    pub fn append<T: Serialize + ?Sized>(
        &mut self,
        out: &mut Vec<u8>,
        value: &T,
    ) -> Result<(), Error> {
        let start = out.len();
        let written = value.serialize(Encoder { out: &mut *out, scratch: &mut self.scratch });
        self.scratch.spans.clear();
        if self.scratch.bytes.capacity() > KEPT_SCRATCH {
            drop(std::mem::take(&mut self.scratch.bytes));
        }
        if written.is_err() {
            out.truncate(start);
        }
        written
    }

    /// How many maps it has put in order since it was made: none for values whose structs are
    /// declared in the order of their keys' encoding.
    pub fn permuted(&self) -> u64 {
        self.scratch.permuted
    }
}

struct Encoder<'a> {
    out: &'a mut Vec<u8>,
    scratch: &'a mut Scratch,
}

fn text(out: &mut Vec<u8>, text: &str) {
    head(out, TEXT, text.len() as u64);
    out.extend_from_slice(text.as_bytes());
}

/// The head at `at`, written saying `said`, made to say `count`: rewritten where it is when it
/// keeps its width, else widened, moving what follows it.
fn rehead(out: &mut Vec<u8>, at: usize, major: u8, said: u64, count: u64) {
    if said == count {
        return;
    }
    let (_, width) = head_bytes(major, said);
    let (bytes, len) = head_bytes(major, count);
    if width == len {
        out[at..at + len].copy_from_slice(&bytes[..len]);
    } else {
        out.splice(at..at + width, bytes[..len].iter().copied());
    }
}

/// Where the item starting at `at` ends, in bytes this encoder wrote (so well formed, and with
/// no indefinite lengths).
fn skip(bytes: &[u8], mut at: usize) -> usize {
    let mut left: u64 = 1;
    while left > 0 {
        left -= 1;
        let first = bytes[at];
        at += 1;
        let n = match first & 0x1f {
            info @ 0..=23 => u64::from(info),
            info => {
                let width = 1 << (info - 24);
                let arg = &bytes[at..at + width];
                at += width;
                arg.iter().fold(0, |n, &byte| (n << 8) | u64::from(byte))
            }
        };
        match first >> 5 {
            BYTES | TEXT => at += n as usize,
            ARRAY => left += n,
            MAP => left += 2 * n,
            _ => {}
        }
    }
    at
}

impl<'a> ser::Serializer for Encoder<'a> {
    type Ok = ();
    type Error = Error;
    type SerializeSeq = Seq<'a>;
    type SerializeTuple = Seq<'a>;
    type SerializeTupleStruct = Seq<'a>;
    type SerializeTupleVariant = Seq<'a>;
    type SerializeMap = Map<'a>;
    type SerializeStruct = Map<'a>;
    type SerializeStructVariant = Map<'a>;

    fn is_human_readable(&self) -> bool {
        false
    }

    fn serialize_bool(self, value: bool) -> Result<(), Error> {
        self.out.push(if value { TRUE } else { FALSE });
        Ok(())
    }

    fn serialize_i8(self, value: i8) -> Result<(), Error> {
        self.serialize_i64(value.into())
    }

    fn serialize_i16(self, value: i16) -> Result<(), Error> {
        self.serialize_i64(value.into())
    }

    fn serialize_i32(self, value: i32) -> Result<(), Error> {
        self.serialize_i64(value.into())
    }

    fn serialize_i64(self, value: i64) -> Result<(), Error> {
        match u64::try_from(value) {
            Ok(n) => head(self.out, UNSIGNED, n),
            // -1 - value, which is what a negative integer's argument is.
            Err(_) => head(self.out, NEGATIVE, !value as u64),
        }
        Ok(())
    }

    fn serialize_i128(self, value: i128) -> Result<(), Error> {
        if let Ok(n) = u64::try_from(value) {
            head(self.out, UNSIGNED, n);
        } else if let Ok(n) = u64::try_from(-1 - value) {
            head(self.out, NEGATIVE, n);
        } else {
            return Err(Error::Unsupported("an integer outside -2^64..2^64"));
        }
        Ok(())
    }

    fn serialize_u8(self, value: u8) -> Result<(), Error> {
        self.serialize_u64(value.into())
    }

    fn serialize_u16(self, value: u16) -> Result<(), Error> {
        self.serialize_u64(value.into())
    }

    fn serialize_u32(self, value: u32) -> Result<(), Error> {
        self.serialize_u64(value.into())
    }

    fn serialize_u64(self, value: u64) -> Result<(), Error> {
        head(self.out, UNSIGNED, value);
        Ok(())
    }

    fn serialize_u128(self, value: u128) -> Result<(), Error> {
        let value = u64::try_from(value)
            .map_err(|_| Error::Unsupported("an integer outside -2^64..2^64"))?;
        self.serialize_u64(value)
    }

    fn serialize_f32(self, value: f32) -> Result<(), Error> {
        float(self.out, value.into());
        Ok(())
    }

    fn serialize_f64(self, value: f64) -> Result<(), Error> {
        float(self.out, value);
        Ok(())
    }

    fn serialize_char(self, value: char) -> Result<(), Error> {
        text(self.out, value.encode_utf8(&mut [0; 4]));
        Ok(())
    }

    fn serialize_str(self, value: &str) -> Result<(), Error> {
        text(self.out, value);
        Ok(())
    }

    fn serialize_bytes(self, value: &[u8]) -> Result<(), Error> {
        head(self.out, BYTES, value.len() as u64);
        self.out.extend_from_slice(value);
        Ok(())
    }

    fn serialize_none(self) -> Result<(), Error> {
        self.out.push(NULL);
        Ok(())
    }

    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<(), Error> {
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<(), Error> {
        self.out.push(NULL);
        Ok(())
    }

    fn serialize_unit_struct(self, _: &'static str) -> Result<(), Error> {
        self.serialize_unit()
    }

    fn serialize_unit_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
    ) -> Result<(), Error> {
        text(self.out, variant);
        Ok(())
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        head(self.out, MAP, 1);
        text(self.out, variant);
        value.serialize(self)
    }

    fn serialize_seq(self, len: Option<usize>) -> Result<Seq<'a>, Error> {
        Ok(Seq::new(self, len))
    }

    fn serialize_tuple(self, len: usize) -> Result<Seq<'a>, Error> {
        Ok(Seq::new(self, Some(len)))
    }

    fn serialize_tuple_struct(self, _: &'static str, len: usize) -> Result<Seq<'a>, Error> {
        Ok(Seq::new(self, Some(len)))
    }

    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Seq<'a>, Error> {
        head(self.out, MAP, 1);
        text(self.out, variant);
        Ok(Seq::new(self, Some(len)))
    }

    fn serialize_map(self, len: Option<usize>) -> Result<Map<'a>, Error> {
        Ok(Map::new(self, len))
    }

    fn serialize_struct(self, _: &'static str, len: usize) -> Result<Map<'a>, Error> {
        Ok(Map::new(self, Some(len)))
    }

    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Map<'a>, Error> {
        head(self.out, MAP, 1);
        text(self.out, variant);
        Ok(Map::new(self, Some(len)))
    }
}

/// An array, written as it comes. One whose length was not said has a head saying none,
/// fixed at its end.
pub(crate) struct Seq<'a> {
    out: &'a mut Vec<u8>,
    scratch: &'a mut Scratch,
    head: usize,
    said: Option<usize>,
    count: usize,
}

impl<'a> Seq<'a> {
    fn new(to: Encoder<'a>, len: Option<usize>) -> Seq<'a> {
        let Encoder { out, scratch } = to;
        let at = out.len();
        head(out, ARRAY, len.unwrap_or(0) as u64);
        Seq { out, scratch, head: at, said: len, count: 0 }
    }

    fn item<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        value.serialize(Encoder { out: &mut *self.out, scratch: &mut *self.scratch })?;
        self.count += 1;
        Ok(())
    }

    fn end(self) -> Result<(), Error> {
        match self.said {
            Some(said) if said != self.count => {
                Err(Error::Unsupported("a sequence of another length than it said"))
            }
            Some(_) => Ok(()),
            None => {
                rehead(self.out, self.head, ARRAY, 0, self.count as u64);
                Ok(())
            }
        }
    }
}

impl ser::SerializeSeq for Seq<'_> {
    type Ok = ();
    type Error = Error;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.item(value)
    }

    fn end(self) -> Result<(), Error> {
        Seq::end(self)
    }
}

impl ser::SerializeTuple for Seq<'_> {
    type Ok = ();
    type Error = Error;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.item(value)
    }

    fn end(self) -> Result<(), Error> {
        Seq::end(self)
    }
}

impl ser::SerializeTupleStruct for Seq<'_> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.item(value)
    }

    fn end(self) -> Result<(), Error> {
        Seq::end(self)
    }
}

impl ser::SerializeTupleVariant for Seq<'_> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.item(value)
    }

    fn end(self) -> Result<(), Error> {
        Seq::end(self)
    }
}

/// A map, its entries written where they go while their keys come in order. Its head says the
/// length it was given (none when it gave none) and is fixed at its end, since the encoding
/// counts the entries there are.
pub(crate) struct Map<'a> {
    out: &'a mut Vec<u8>,
    scratch: &'a mut Scratch,
    head: usize,
    said: u64,
    /// Where its first entry starts.
    entries: usize,
    count: usize,
    /// The last key, where it is in the output, while every key so far came in order.
    last: (usize, usize),
    /// The last key of a struct, by name.
    last_name: &'static str,
    /// Where its entries' spans start in the scratch, once a key came out of order.
    spans: Option<usize>,
    /// A key came twice in a row.
    twice: bool,
}

impl<'a> Map<'a> {
    fn new(to: Encoder<'a>, len: Option<usize>) -> Map<'a> {
        let Encoder { out, scratch } = to;
        let at = out.len();
        let said = len.unwrap_or(0) as u64;
        head(out, MAP, said);
        let entries = out.len();
        Map {
            out,
            scratch,
            head: at,
            said,
            entries,
            count: 0,
            last: (entries, entries),
            last_name: "",
            spans: None,
            twice: false,
        }
    }

    fn key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), Error> {
        let start = self.out.len();
        key.serialize(Encoder { out: &mut *self.out, scratch: &mut *self.scratch })?;
        let end = self.out.len();
        self.count += 1;
        match self.spans {
            Some(_) => self.open(start, end),
            None if self.count == 1 => self.last = (start, end),
            None => {
                let last = &self.out[self.last.0..self.last.1];
                match key_order(last, &self.out[start..end]) {
                    Ordering::Less => self.last = (start, end),
                    Ordering::Equal => self.twice = true,
                    Ordering::Greater => self.out_of_order(start, end),
                }
            }
        }
        Ok(())
    }

    /// A struct's field name as its key, ordered against the field before by the names
    /// themselves: the same order as their encodings', with nothing read back from the output.
    fn name(&mut self, name: &'static str) {
        let start = self.out.len();
        text(self.out, name);
        let end = self.out.len();
        self.count += 1;
        match self.spans {
            Some(_) => self.open(start, end),
            None if self.count == 1 => {}
            None => match name_order(self.last_name, name) {
                Ordering::Less => {}
                Ordering::Equal => self.twice = true,
                Ordering::Greater => self.out_of_order(start, end),
            },
        }
        self.last_name = name;
    }

    fn value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        value.serialize(Encoder { out: &mut *self.out, scratch: &mut *self.scratch })
    }

    /// The entry whose key is at `key..key_end`, which ends the one before it.
    fn open(&mut self, key: usize, key_end: usize) {
        if let Some(last) = self.scratch.spans.last_mut() {
            last.end = key;
        }
        self.scratch.spans.push(Span { key, key_end, end: key_end });
    }

    /// The key at `start..end` came before the last: the spans of the entries before it are
    /// found again in what was written, and kept from now on.
    fn out_of_order(&mut self, start: usize, end: usize) {
        self.spans = Some(self.scratch.spans.len());
        let mut at = self.entries;
        while at < start {
            let key_end = skip(self.out, at);
            let entry_end = skip(self.out, key_end);
            self.scratch.spans.push(Span { key: at, key_end, end: entry_end });
            at = entry_end;
        }
        self.open(start, end);
    }

    fn end(self) -> Result<(), Error> {
        let Map { out, scratch, head, said, entries, count, spans, twice, .. } = self;
        if twice {
            return Err(Error::Unsupported("a map with one key twice"));
        }
        if let Some(base) = spans {
            let region_end = out.len();
            let written = out.as_slice();
            let spans = &mut scratch.spans[base..];
            if let Some(last) = spans.last_mut() {
                last.end = region_end;
            }
            let key = |span: &Span| &written[span.key..span.key_end];
            spans.sort_unstable_by(|a, b| key(a).cmp(key(b)));
            if spans.windows(2).any(|pair| key(&pair[0]) == key(&pair[1])) {
                return Err(Error::Unsupported("a map with one key twice"));
            }
            scratch.bytes.clear();
            scratch.bytes.reserve(region_end - entries);
            for span in spans.iter() {
                scratch.bytes.extend_from_slice(&written[span.key..span.end]);
            }
            out[entries..region_end].copy_from_slice(&scratch.bytes);
            scratch.spans.truncate(base);
            scratch.permuted += 1;
        }
        rehead(out, head, MAP, said, count as u64);
        Ok(())
    }
}

impl ser::SerializeMap for Map<'_> {
    type Ok = ();
    type Error = Error;

    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), Error> {
        self.key(key)
    }

    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.value(value)
    }

    fn end(self) -> Result<(), Error> {
        Map::end(self)
    }
}

impl ser::SerializeStruct for Map<'_> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        self.name(key);
        self.value(value)
    }

    fn end(self) -> Result<(), Error> {
        Map::end(self)
    }
}

impl ser::SerializeStructVariant for Map<'_> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        self.name(key);
        self.value(value)
    }

    fn end(self) -> Result<(), Error> {
        Map::end(self)
    }
}
