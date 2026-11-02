//! Bytes from a peer through blaze-protocol's deterministic CBOR: the decoder
//! (`cbor::from_slice`), the checker (`cbor::check`) and the decoder it replaced
//! (`cbor::reference::from_slice`), and an item that decodes back out through both encoders
//! (`cbor::to_vec`, a kept `cbor::Writer`, `cbor::reference::to_vec`).
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - `check` allocates nothing; the decoder and the reference ask for at most
//!   `blaze_fuzz::allowed(len)` bytes at once (each item takes a byte at least, so a hint
//!   bounded by the bytes left times a value of at most 64 bytes is inside it).
//! - The bytes decode as an item exactly when `check` passes them, and where it refuses them
//!   the decoder's error is the checker's: what decodes is canonical.
//! - Read as any item, passed over (`IgnoredAny`), or read in part by a struct, a tuple, a
//!   sequence, a map or an enum, the fast decoder and the reference give the same value or
//!   the same error; every typed read that succeeds is of bytes `check` passes, and every one
//!   of bytes it refuses fails with its error.
//! - An item that decodes encodes back to exactly the input bytes, through `to_vec`, a
//!   `Writer` appending twice after other bytes, and the reference encoder.

#![forbid(unsafe_code)]

use std::{borrow::Cow, collections::BTreeMap, fmt, time::Duration};

use blaze_fuzzer::Target;
use blaze_protocol::cbor::{self, Writer, reference};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, DeserializeOwned, IgnoredAny, MapAccess, SeqAccess, Visitor},
    ser::{SerializeMap, SerializeSeq},
};

const TARGET: Target = Target {
    name: "api_cbor",
    check,
    seeds,
    dictionary: &[
        b"\x18",
        b"\x19",
        b"\x1a",
        b"\x1b",
        b"\x1c",
        b"\x1f",
        b"\x9f",
        b"\xbf",
        b"\xff",
        b"\xc1",
        b"\xf4",
        b"\xf5",
        b"\xf6",
        b"\xf7",
        b"\xf8\x20",
        b"\xf9\x7e\x00",
        b"\xf9\x7c\x00",
        b"\xf9\x80\x00",
        b"\xfa\x3f\x80\x00\x00",
        b"\xfb\x3f\xf0\x00\x00\x00\x00\x00\x00",
        b"\x3b\xff\xff\xff\xff\xff\xff\xff\xff",
        b"\x5b\xff\xff\xff\xff\xff\xff\xff\xff",
        b"\x7b\xff\xff\xff\xff\xff\xff\xff\xff",
        b"\x9b\xff\xff\xff\xff\xff\xff\xff\xff",
        b"\xbb\xff\xff\xff\xff\xff\xff\xff\xff",
        b"\x62id",
        b"\x64body",
        b"\x64kind",
        b"\x66schema",
        b"\x64Unit",
        b"\x67Newtype",
        b"\x65Tuple",
        b"\x66Struct",
        b"\x61a",
        b"\x61b",
        b"\xc3\x28",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(600),
};

/// The typed reads, one a run (`blaze_fuzzer::case`): each decodes the whole input twice and
/// most fail with a formatted message. The plain test takes every one of each seed.
const TYPED: usize = 7;

fn check(data: &[u8]) {
    check_case(data, blaze_fuzzer::case(data, TYPED));
}

fn check_case(data: &[u8], case: usize) {
    let allowed = blaze_fuzz::allowed(data.len());
    let (most, checked) = blaze_fuzz::largest_allocation(|| cbor::check(data));
    assert_eq!(most, 0, "check allocates nothing");

    let (most, read) = blaze_fuzz::largest_allocation(|| cbor::from_slice::<Value<'_>>(data));
    assert!(most <= allowed, "decoding {} bytes asked for {most} at once", data.len());
    assert_eq!(
        read.as_ref().err(),
        checked.as_ref().err(),
        "the bytes decode as an item exactly when check passes them, with its error"
    );
    let (most, old) = blaze_fuzz::largest_allocation(|| reference::from_slice::<Value<'_>>(data));
    assert!(most <= allowed, "the reference decoding {} bytes asked for {most}", data.len());
    assert_eq!(read, old, "the fast decoder and the reference read any item alike");

    match case {
        0 => same::<IgnoredAny>(data, &checked),
        1 => same::<Typed>(data, &checked),
        2 => same::<(IgnoredAny, Option<u8>)>(data, &checked),
        3 => same::<Vec<Option<bool>>>(data, &checked),
        4 => same::<BTreeMap<String, i64>>(data, &checked),
        5 => same::<Variant>(data, &checked),
        _ => same::<Vec<Variant>>(data, &checked),
    }

    if let Ok(value) = &read {
        assert_eq!(
            cbor::to_vec(value).as_deref(),
            Ok(data),
            "an item that decodes encodes back to its bytes"
        );
        assert_eq!(
            reference::to_vec(value).as_deref(),
            Ok(data),
            "an item that decodes encodes back to its bytes through the reference"
        );
        let mut writer = Writer::new();
        let mut out = vec![0xee];
        writer.append(&mut out, value).expect("an item that decodes has an encoding");
        writer.append(&mut out, value).expect("an item that decodes has an encoding");
        assert_eq!(out.len(), 1 + 2 * data.len(), "a kept writer appends the item's bytes");
        assert_eq!(out[0], 0xee, "a writer leaves the bytes before it alone");
        assert_eq!(&out[1..1 + data.len()], data, "a writer appends the item's bytes");
        assert_eq!(&out[1 + data.len()..], data, "a kept writer appends the same bytes again");
    }
}

/// The two decoders read `data` as a `T` alike, and only canonical bytes as one.
#[track_caller]
fn same<T: DeserializeOwned + PartialEq + fmt::Debug>(
    data: &[u8],
    checked: &Result<(), cbor::Error>,
) {
    let name = std::any::type_name::<T>();
    let fast = cbor::from_slice::<T>(data);
    assert_eq!(fast, reference::from_slice::<T>(data), "the two decoders read a {name} alike");
    if fast.is_ok() {
        assert_eq!(checked, &Ok(()), "a {name} decoded from bytes check refuses");
    }
    if let Err(error) = checked {
        assert_eq!(fast.as_ref().err(), Some(error), "a {name} of bytes check refuses fails with its error");
    }
}

/// Reads some fields of a map and passes over the rest.
#[derive(Deserialize, Debug, PartialEq)]
struct Typed {
    id: Option<u64>,
    kind: Option<String>,
    body: Option<IgnoredAny>,
}

/// Every way an enum is read: a name, or a map of one entry from the name to its contents.
#[derive(Deserialize, Debug, PartialEq)]
enum Variant {
    Unit,
    Newtype(i64),
    Tuple(u8, Option<String>),
    Struct { a: bool, b: Option<u32> },
}

/// Any item, as `deserialize_any` gives it, borrowing its strings where it can.
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

const _: () = assert!(size_of::<Value<'static>>() <= 64);

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
        let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0));
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value<'de>, A::Error> {
        let mut entries = Vec::with_capacity(map.size_hint().unwrap_or(0));
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

/// Items of every kind at the edges of their heads, written by the encoder.
fn seeds() -> Vec<Vec<u8>> {
    let text = |text: &'static str| Value::Text(Cow::Borrowed(text));
    let mut values: Vec<Value<'static>> = Vec::new();
    for n in [0, 23, 24, 255, 256, 65_535, 65_536, 0xffff_ffff, 1 << 32, u64::MAX] {
        values.push(Value::Unsigned(n));
    }
    for n in [-1, -24, -25, -256, -257, -65_537, i64::MIN] {
        values.push(Value::Signed(n));
    }
    for n in [i128::from(i64::MIN) - 1, -(1 << 64)] {
        values.push(Value::Wide(n));
    }
    for x in [
        0.0,
        -0.0,
        1.0,
        1.5,
        65_504.0,
        65_536.0,
        5.960_464_477_539_063e-8,
        1.0e-7,
        0.1,
        f64::from(f32::MIN_POSITIVE) / 2.0,
        f64::from(f32::MAX),
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NAN,
        1.0e300,
    ] {
        values.push(Value::Float(x.to_bits()));
    }
    values.extend([Value::Bool(false), Value::Bool(true), Value::Null]);
    values.extend([text(""), text("src/lib.rs"), text("\u{e9}\u{6c34}\u{1f525}")]);
    values.push(Value::Text(Cow::Owned("x".repeat(300))));
    values.push(Value::Bytes(Cow::Borrowed(&[0, 1, 0xff])));
    values.push(Value::Bytes(Cow::Owned(vec![7; 24])));
    values.push(Value::Array((0..24).map(Value::Unsigned).collect()));
    values.push(Value::Map(vec![
        (Value::Unsigned(1), Value::Null),
        (Value::Signed(-1), Value::Bool(true)),
        (text("a"), Value::Unsigned(1)),
        (text("b"), Value::Unsigned(2)),
        (text("aa"), Value::Unsigned(3)),
        (Value::Bytes(Cow::Borrowed(b"id")), Value::Null),
        (Value::Array(vec![Value::Unsigned(0)]), Value::Array(Vec::new())),
    ]));
    values.push(Value::Map(vec![
        (text("id"), Value::Unsigned(7)),
        (text("kind"), text("build")),
        (text("body"), Value::Map(vec![(text("ok"), Value::Bool(true))])),
    ]));
    values.extend([
        text("Unit"),
        Value::Map(vec![(text("Newtype"), Value::Signed(-3))]),
        Value::Map(vec![(
            text("Tuple"),
            Value::Array(vec![Value::Unsigned(1), text("one")]),
        )]),
        Value::Map(vec![(
            text("Struct"),
            Value::Map(vec![(text("a"), Value::Bool(true)), (text("b"), Value::Null)]),
        )]),
    ]);
    // A null inside 256 arrays is as deep as a peer may nest; inside 257 it is refused.
    for depth in [255, 256, 257] {
        let mut deep = Value::Null;
        for _ in 0..depth {
            deep = Value::Array(vec![deep]);
        }
        values.push(deep);
    }
    let mut seeds: Vec<Vec<u8>> = values
        .iter()
        .map(|value| cbor::to_vec(value).expect("a seed has an encoding"))
        .collect();
    seeds.push(
        cbor::to_vec(&blaze_protocol::Message::new(7, Value::Array(vec![text("Status")])))
            .expect("a message has an encoding"),
    );
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

    #[test]
    fn every_case() {
        blaze_fuzzer::every_case(&super::TARGET, super::TYPED, super::check_case);
    }
}
