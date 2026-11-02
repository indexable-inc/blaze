//! Bytes as a program that builds a value of any shape serde hands a serializer (integers past
//! 64 bits, floats of both widths, sequences and maps that say a length that is not theirs or
//! none, keys out of order or twice, structs, every kind of variant), written by
//! blaze-protocol's encoder (`cbor::to_vec`, a kept `cbor::Writer`, `cbor::append`,
//! `blaze_protocol::append_frame`) and by the encoder it replaced (`cbor::reference::to_vec`).
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - The encoder writes the reference's bytes for every value, or refuses it with the
//!   reference's error.
//! - A kept writer appends those bytes after what is in the buffer, again on a second append;
//!   a value it refuses leaves the buffer as it was, and the writer then writes the next value
//!   as a new one would. `cbor::append` writes the same.
//! - What it writes is canonical (`cbor::check` passes it), decodes through the fast decoder
//!   and the reference to the same item, and that item encodes back to the same bytes.
//! - `append_frame` puts the bytes after their big-endian length, which `read_frame` reads
//!   back; a value it refuses leaves the buffer as it was.

#![forbid(unsafe_code)]

use std::{borrow::Cow, fmt, time::Duration};

use blaze_fuzzer::Target;
use blaze_protocol::cbor::{self, Writer, reference};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, MapAccess, SeqAccess, Visitor},
    ser::{
        SerializeMap, SerializeSeq, SerializeStruct, SerializeStructVariant, SerializeTuple,
        SerializeTupleStruct, SerializeTupleVariant,
    },
};

const TARGET: Target = Target {
    name: "api_cbor_encode",
    check,
    seeds,
    dictionary: &[b"\xf0", b"\xfd", b"\xfe", b"\xff", b"\x00\x00\x00\x00", b"\xff\xff\xff\xff"],
    max_len: 1 << 12,
    budget: Duration::from_secs(600),
};

/// Field and variant names, in no order: some in their encoding's, most not.
const NAMES: [&str; 12] =
    ["schema", "id", "body", "a", "b", "aa", "ok", "kind", "mode", "path", "bytes", "packages"];

/// The most values one program builds, past which every value is a leaf.
const MOST_NODES: u32 = 4096;
/// How deep the containers of a program nest.
const DEEPEST: u32 = 8;

fn check(data: &[u8]) {
    let shape = Program { bytes: data, at: 0, nodes: 0 }.shape(DEEPEST);
    let new = cbor::to_vec(&shape);
    assert_eq!(new, reference::to_vec(&shape), "the encoder writes what the reference does, or refuses as it does");

    let mut writer = Writer::new();
    let mut out = vec![0xee];
    let first = writer.append(&mut out, &shape);
    let second = writer.append(&mut out, &shape);
    let mut appended = vec![0xee];
    let alone = cbor::append(&mut appended, &shape);
    let mut frame = vec![1, 2, 3];
    let framed = blaze_protocol::append_frame(&mut writer, &mut frame, &shape);
    match &new {
        Ok(bytes) => {
            assert_eq!((&first, &second, &alone), (&Ok(()), &Ok(()), &Ok(())), "every writer writes what to_vec does");
            assert_eq!(out.len(), 1 + 2 * bytes.len(), "a kept writer appends the value's bytes");
            assert_eq!(out[0], 0xee, "a writer leaves the bytes before it alone");
            assert_eq!(&out[1..1 + bytes.len()], &bytes[..], "a writer appends what to_vec writes");
            assert_eq!(&out[1 + bytes.len()..], &bytes[..], "a kept writer writes the same bytes again");
            assert_eq!(appended[1..], bytes[..], "append writes what to_vec does");
            framed.expect("a value of a few kilobytes has a frame");
            assert_eq!(frame[..3], [1, 2, 3], "append_frame leaves the bytes before it alone");
            assert_eq!(frame[3..7], (bytes.len() as u32).to_be_bytes(), "a frame starts with its length");
            assert_eq!(frame[7..], bytes[..], "a frame's item is what to_vec writes");
            let read = blaze_protocol::read_frame(&mut &frame[3..]).expect("a written frame reads");
            assert_eq!(read.as_ref(), Some(bytes), "read_frame reads back the item append_frame wrote");

            assert_eq!(cbor::check(bytes), Ok(()), "what the encoder writes is canonical");
            let value = cbor::from_slice::<Value<'_>>(bytes).expect("what the encoder writes decodes");
            assert_eq!(
                reference::from_slice::<Value<'_>>(bytes).as_ref(),
                Ok(&value),
                "the two decoders read what the encoder writes alike"
            );
            assert_eq!(cbor::to_vec(&value).as_ref(), Ok(bytes), "what the encoder writes reads back to itself");
        }
        Err(error) => {
            assert_eq!((first.as_ref(), second.as_ref(), alone.as_ref()), (Err(error), Err(error), Err(error)), "every writer refuses what to_vec does");
            assert_eq!(out, [0xee], "a refused value leaves the buffer as it was");
            assert_eq!(appended, [0xee], "a refused value leaves the buffer as it was");
            assert!(framed.is_err(), "append_frame refuses what to_vec does");
            assert_eq!(frame, [1, 2, 3], "a refused frame leaves the buffer as it was");
        }
    }
    let mut next = Vec::new();
    writer.append(&mut next, &NAMES).expect("a list of names has an encoding");
    assert_eq!(Ok(next), cbor::to_vec(&NAMES), "a kept writer writes the next value as a new one");
}

/// A value as the program said it, to be serialized as it says.
#[derive(Debug)]
enum Shape {
    Unsigned(u64),
    Signed(i64),
    Wide(i128),
    Huge(u128),
    Double(f64),
    Single(f32),
    Bool(bool),
    None,
    Unit,
    Char(char),
    Text(String),
    Bytes(Vec<u8>),
    UnitStruct,
    UnitVariant(usize),
    Some(Box<Shape>),
    Seq { said: Said, items: Vec<Shape> },
    Tuple(Vec<Shape>),
    TupleStruct(Vec<Shape>),
    Map { said: Said, entries: Vec<(Shape, Shape)> },
    Struct(Vec<(usize, Shape)>),
    NewtypeStruct(Box<Shape>),
    NewtypeVariant(usize, Box<Shape>),
    TupleVariant(usize, Vec<Shape>),
    StructVariant(usize, Vec<(usize, Shape)>),
}

/// The length a sequence or a map says up front.
#[derive(Clone, Copy, Debug)]
enum Said {
    Nothing,
    Own,
    Other(usize),
}

impl Said {
    fn len(self, own: usize) -> Option<usize> {
        match self {
            Said::Nothing => None,
            Said::Own => Some(own),
            Said::Other(len) => Some(len),
        }
    }
}

impl Serialize for Shape {
    fn serialize<S: Serializer>(&self, to: S) -> Result<S::Ok, S::Error> {
        match self {
            Shape::Unsigned(n) => to.serialize_u64(*n),
            Shape::Signed(n) => to.serialize_i64(*n),
            Shape::Wide(n) => to.serialize_i128(*n),
            Shape::Huge(n) => to.serialize_u128(*n),
            Shape::Double(x) => to.serialize_f64(*x),
            Shape::Single(x) => to.serialize_f32(*x),
            Shape::Bool(value) => to.serialize_bool(*value),
            Shape::None => to.serialize_none(),
            Shape::Unit => to.serialize_unit(),
            Shape::Char(it) => to.serialize_char(*it),
            Shape::Text(text) => to.serialize_str(text),
            Shape::Bytes(bytes) => to.serialize_bytes(bytes),
            Shape::UnitStruct => to.serialize_unit_struct("Unit"),
            Shape::UnitVariant(name) => {
                to.serialize_unit_variant("Shape", *name as u32, NAMES[*name])
            }
            Shape::Some(inner) => to.serialize_some(&**inner),
            Shape::Seq { said, items } => {
                let mut seq = to.serialize_seq(said.len(items.len()))?;
                for item in items {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            Shape::Tuple(items) => {
                let mut tuple = to.serialize_tuple(items.len())?;
                for item in items {
                    tuple.serialize_element(item)?;
                }
                tuple.end()
            }
            Shape::TupleStruct(items) => {
                let mut tuple = to.serialize_tuple_struct("Tuple", items.len())?;
                for item in items {
                    tuple.serialize_field(item)?;
                }
                tuple.end()
            }
            Shape::Map { said, entries } => {
                let mut map = to.serialize_map(said.len(entries.len()))?;
                for (key, value) in entries {
                    map.serialize_entry(key, value)?;
                }
                map.end()
            }
            Shape::Struct(fields) => {
                let mut map = to.serialize_struct("Shape", fields.len())?;
                for (name, value) in fields {
                    map.serialize_field(NAMES[*name], value)?;
                }
                map.end()
            }
            Shape::NewtypeStruct(inner) => to.serialize_newtype_struct("Newtype", &**inner),
            Shape::NewtypeVariant(name, inner) => {
                to.serialize_newtype_variant("Shape", *name as u32, NAMES[*name], &**inner)
            }
            Shape::TupleVariant(name, items) => {
                let mut tuple =
                    to.serialize_tuple_variant("Shape", *name as u32, NAMES[*name], items.len())?;
                for item in items {
                    tuple.serialize_field(item)?;
                }
                tuple.end()
            }
            Shape::StructVariant(name, fields) => {
                let mut map =
                    to.serialize_struct_variant("Shape", *name as u32, NAMES[*name], fields.len())?;
                for (field, value) in fields {
                    map.serialize_field(NAMES[*field], value)?;
                }
                map.end()
            }
        }
    }
}

/// The input read as a program: each value is a byte naming its kind and then what that kind
/// takes. Past the end every byte reads as 0, a leaf.
struct Program<'a> {
    bytes: &'a [u8],
    at: usize,
    nodes: u32,
}

impl<'a> Program<'a> {
    fn byte(&mut self) -> u8 {
        let byte = self.bytes.get(self.at).copied().unwrap_or(0);
        self.at = (self.at + 1).min(self.bytes.len());
        byte
    }

    fn take(&mut self, len: usize) -> &'a [u8] {
        let bytes = self.bytes;
        let end = (self.at + len).min(bytes.len());
        let taken = &bytes[self.at..end];
        self.at = end;
        taken
    }

    /// An integer of 0, 1, 2, 4 or 8 bytes, as the byte before it says.
    fn uint(&mut self) -> u64 {
        let width = [0, 1, 2, 4, 8][usize::from(self.byte() % 5)];
        self.take(width).iter().fold(0, |n, &byte| (n << 8) | u64::from(byte))
    }

    fn wide(&mut self) -> i128 {
        const EDGES: [i128; 8] = [
            i128::MIN,
            -(1 << 64) - 1,
            -(1 << 64),
            -(1 << 64) + 1,
            -(1 << 63) - 1,
            (1 << 64) - 1,
            1 << 64,
            i128::MAX,
        ];
        match self.byte() % 10 {
            pick @ 0..=7 => EDGES[usize::from(pick)],
            _ => i128::from(self.uint() as i64) * 3,
        }
    }

    fn huge(&mut self) -> u128 {
        match self.byte() % 4 {
            0 => u128::from(u64::MAX),
            1 => 1 << 64,
            2 => u128::MAX,
            _ => u128::from(self.uint()),
        }
    }

    fn double(&mut self) -> f64 {
        const EDGES: [f64; 10] = [
            0.0,
            -0.0,
            f64::NAN,
            f64::INFINITY,
            65_504.0,
            5.960_464_477_539_063e-8,
            1.0e-7,
            f32::MAX as f64,
            1.5,
            0.1,
        ];
        match self.byte() % 12 {
            pick @ 0..=9 => EDGES[usize::from(pick)],
            10 => f64::from(self.uint() as u16) / 8.0,
            _ => f64::from_bits(self.uint()),
        }
    }

    fn text(&mut self) -> String {
        const CHARS: [char; 8] = ['a', 'Z', '0', ' ', '\u{e9}', '\u{6c34}', '\u{1f525}', '\0'];
        let len = self.byte() % 16;
        (0..len).map(|_| CHARS[usize::from(self.byte() % 8)]).collect()
    }

    fn name(&mut self) -> usize {
        usize::from(self.byte()) % NAMES.len()
    }

    /// Mostly a few, now and then just past a head's one-byte and two-byte widths; none once
    /// the program has built its most.
    fn count(&mut self) -> usize {
        if self.nodes > MOST_NODES {
            return 0;
        }
        match self.byte() {
            byte @ 0..=239 => usize::from(byte % 6),
            byte @ 240..=253 => 23 + usize::from(byte - 240),
            254 => 255,
            _ => 256,
        }
    }

    fn said(&mut self) -> Said {
        match self.byte() % 4 {
            0 => Said::Nothing,
            1 | 2 => Said::Own,
            _ => Said::Other(usize::from(self.byte())),
        }
    }

    fn shape(&mut self, depth: u32) -> Shape {
        self.nodes += 1;
        let mut kind = self.byte() % 24;
        if (depth == 0 || self.nodes > MOST_NODES) && kind >= 14 {
            kind %= 14;
        }
        match kind {
            0 => Shape::Unsigned(self.uint()),
            1 => Shape::Signed(self.uint() as i64),
            2 => Shape::Wide(self.wide()),
            3 => Shape::Huge(self.huge()),
            4 => Shape::Double(self.double()),
            5 => Shape::Single(f32::from_bits(self.uint() as u32)),
            6 => Shape::Bool(self.byte() & 1 == 1),
            7 => Shape::None,
            8 => Shape::Unit,
            9 => Shape::Char(char::from_u32(self.uint() as u32).unwrap_or('\u{fffd}')),
            10 => Shape::Text(self.text()),
            11 => {
                let len = usize::from(self.byte() % 40);
                Shape::Bytes(self.take(len).to_vec())
            }
            12 => Shape::UnitStruct,
            13 => Shape::UnitVariant(self.name()),
            14 => Shape::Some(Box::new(self.shape(depth - 1))),
            15 => {
                let said = self.said();
                Shape::Seq { said, items: self.items(depth) }
            }
            16 => Shape::Tuple(self.items(depth)),
            17 => Shape::TupleStruct(self.items(depth)),
            18 => {
                let said = self.said();
                Shape::Map { said, entries: self.entries(depth) }
            }
            19 => Shape::Struct(self.fields(depth)),
            20 => Shape::NewtypeStruct(Box::new(self.shape(depth - 1))),
            21 => {
                let name = self.name();
                Shape::NewtypeVariant(name, Box::new(self.shape(depth - 1)))
            }
            22 => {
                let name = self.name();
                Shape::TupleVariant(name, self.items(depth))
            }
            _ => {
                let name = self.name();
                Shape::StructVariant(name, self.fields(depth))
            }
        }
    }

    fn items(&mut self, depth: u32) -> Vec<Shape> {
        let count = self.count();
        (0..count).map(|_| self.shape(depth - 1)).collect()
    }

    /// Keys distinct and scrambled (integers of every head width, or texts), or keys of any
    /// shape, which may come twice.
    fn entries(&mut self, depth: u32) -> Vec<(Shape, Shape)> {
        let count = self.count();
        let mode = self.byte() % 4;
        let salt = self.uint() as u32;
        (0..count)
            .map(|at| {
                // An odd multiplier is a bijection of u32, and so is the xor.
                let distinct = (at as u32).wrapping_mul(0x9e37_79b9) ^ salt;
                let key = match mode {
                    0 => Shape::Unsigned(u64::from(distinct)),
                    1 => Shape::Text(format!("{distinct:x}")),
                    _ => self.shape(depth - 1),
                };
                (key, self.shape(depth - 1))
            })
            .collect()
    }

    /// Names in any order, now and then one twice.
    fn fields(&mut self, depth: u32) -> Vec<(usize, Shape)> {
        let count = self.count();
        (0..count).map(|_| (self.name(), self.shape(depth - 1))).collect()
    }
}

/// Any item, as `deserialize_any` gives it.
#[derive(Debug, PartialEq)]
enum Value<'a> {
    Unsigned(u64),
    Signed(i64),
    Wide(i128),
    /// Its bits, so that NaN is equal to itself.
    Float(u64),
    Bool(bool),
    Null,
    Text(Cow<'a, str>),
    Bytes(Cow<'a, [u8]>),
    Array(Vec<Value<'a>>),
    Map(Vec<(Value<'a>, Value<'a>)>),
}

impl<'de> Deserialize<'de> for Value<'de> {
    fn deserialize<D: Deserializer<'de>>(from: D) -> Result<Value<'de>, D::Error> {
        from.deserialize_any(Values)
    }
}

struct Values;

impl<'de> Visitor<'de> for Values {
    type Value = Value<'de>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any CBOR item")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Value<'de>, E> {
        Ok(Value::Bool(value))
    }

    fn visit_u64<E: de::Error>(self, n: u64) -> Result<Value<'de>, E> {
        Ok(Value::Unsigned(n))
    }

    fn visit_i64<E: de::Error>(self, n: i64) -> Result<Value<'de>, E> {
        Ok(Value::Signed(n))
    }

    fn visit_i128<E: de::Error>(self, n: i128) -> Result<Value<'de>, E> {
        Ok(Value::Wide(n))
    }

    fn visit_f64<E: de::Error>(self, x: f64) -> Result<Value<'de>, E> {
        Ok(Value::Float(x.to_bits()))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value<'de>, E> {
        Ok(Value::Null)
    }

    fn visit_borrowed_str<E: de::Error>(self, text: &'de str) -> Result<Value<'de>, E> {
        Ok(Value::Text(Cow::Borrowed(text)))
    }

    fn visit_str<E: de::Error>(self, text: &str) -> Result<Value<'de>, E> {
        Ok(Value::Text(Cow::Owned(text.to_owned())))
    }

    fn visit_borrowed_bytes<E: de::Error>(self, bytes: &'de [u8]) -> Result<Value<'de>, E> {
        Ok(Value::Bytes(Cow::Borrowed(bytes)))
    }

    fn visit_bytes<E: de::Error>(self, bytes: &[u8]) -> Result<Value<'de>, E> {
        Ok(Value::Bytes(Cow::Owned(bytes.to_vec())))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value<'de>, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value<'de>, A::Error> {
        let mut entries = Vec::new();
        while let Some(entry) = map.next_entry()? {
            entries.push(entry);
        }
        Ok(Value::Map(entries))
    }
}

impl Serialize for Value<'_> {
    fn serialize<S: Serializer>(&self, to: S) -> Result<S::Ok, S::Error> {
        match self {
            Value::Unsigned(n) => to.serialize_u64(*n),
            Value::Signed(n) => to.serialize_i64(*n),
            Value::Wide(n) => to.serialize_i128(*n),
            Value::Float(bits) => to.serialize_f64(f64::from_bits(*bits)),
            Value::Bool(value) => to.serialize_bool(*value),
            Value::Null => to.serialize_unit(),
            Value::Text(text) => to.serialize_str(text),
            Value::Bytes(bytes) => to.serialize_bytes(bytes),
            Value::Array(items) => {
                let mut seq = to.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            Value::Map(entries) => {
                let mut map = to.serialize_map(Some(entries.len()))?;
                for (key, value) in entries {
                    map.serialize_entry(key, value)?;
                }
                map.end()
            }
        }
    }
}

/// Programs for the shapes the encoder handles apart: maps put in order, heads widened at the
/// end, refusals inside what is written in place.
fn seeds() -> Vec<Vec<u8>> {
    const SEQ: u8 = 15;
    const MAP: u8 = 18;
    const STRUCT: u8 = 19;
    const STRUCT_VARIANT: u8 = 23;
    const WIDE: u8 = 2;
    // Said: 0 nothing, 1 its own, 3 another (the byte after).
    const UNIT: u8 = 8;
    let mut seeds = vec![
        Vec::new(),
        // A map of 36 integer keys out of order, its length unsaid.
        vec![MAP, 0, 253, 0, 4, 0x12, 0x34, 0x56, 0x78],
        // The same with text keys and its own length said.
        vec![MAP, 1, 253, 1, 1, 0x5a],
        // A map of 5 keys of any shape, saying 200.
        vec![MAP, 3, 200, 5, 2, 0, 0, 1, 7, 0, 10, 1, 3, 0, 1, 3, 0, 0, 0, 0, 1, 1, 9],
        // A sequence of 256 items, its length unsaid: a head widened by two bytes.
        vec![SEQ, 0, 255, 7],
        // A sequence that says 3 and holds 2.
        vec![SEQ, 3, 3, 2, 0, 1, 7, 0, 1, 8],
        // A struct of id, schema, body, id: out of order, and id twice but not in a row.
        vec![STRUCT, 4, 1, UNIT, 0, UNIT, 2, UNIT, 1, UNIT],
        // A struct variant whose second field is an integer past 64 bits.
        vec![STRUCT_VARIANT, 2, 2, 1, 0, 0, 2, WIDE, 0],
    ];
    // Sequences and maps nested in each other, each map out of order.
    let mut nested = Vec::new();
    for _ in 0..4 {
        nested.extend([SEQ, 0, 2, MAP, 0, 4, 0, 1, 0xa5]);
    }
    seeds.push(nested);
    seeds
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    #[test]
    fn seeds() {
        blaze_fuzzer::seeds(&super::TARGET);
    }
}
