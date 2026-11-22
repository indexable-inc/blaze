//! RFC 8949's appendix A, each deterministic-encoding rule with a refused non-canonical input,
//! the bounds, the field reader, the event stream, and the round trip against `Value` with its
//! planted-bug control.

use core::fmt;
use std::{format, string::String as StdString, string::ToString, vec::Vec as StdVec};

use blaze_mem::{Frame, Tag};

use blaze_data::{Int, Map, Value, value};
use blaze_own::{Alloc, AVec, Erased, String, Vec};
use blaze_sim::{Broken, Config, NoPlant, Planted, Point, Tape, check, controls};

use crate::{
    CborPlant, Decoder, Encoder, Error, Event, Head, Limits, Reader, StreamError, StreamWhat, major,
    read_in, to_vec_in, why, write_into_planted,
};

fn hex<'a>(text: &str, alloc: Alloc<'a>) -> AVec<'a, u8> {
    let digits = Vec::from_iter_in(text.bytes().filter(|b| !b.is_ascii_whitespace()), alloc);
    Vec::from_iter_in(digits.chunks(2).map(|pair| u8::from_str_radix(core::str::from_utf8(pair).unwrap(), 16).unwrap()), alloc)
}

fn read<'a>(bytes: &[u8], alloc: Alloc<'a>) -> Value<Alloc<'a>> {
    match read_in(bytes, Limits::DEFAULT, alloc) {
        Ok(value) => value,
        Err(error) => panic!("{bytes:02x?}: {error}"),
    }
}

fn refused(text: &str, alloc: Alloc<'_>) -> Error {
    let bytes = hex(text, alloc);
    match read_in(&bytes, Limits::DEFAULT, alloc) {
        Ok(value) => panic!("{text} read as {value:?}"),
        Err(error) => error,
    }
}

fn written<'a>(value: &Value<Alloc<'_>>, alloc: Alloc<'a>) -> AVec<'a, u8> {
    to_vec_in(value, alloc)
}

/// Appendix A's vectors that are deterministic: read as the value, and the value written back
/// as the same bytes.
#[test]
fn appendix_a_reads_and_writes_back() {
    let frame = Frame::with_reserve(4usize << 30, Tag::Codegen).expect("reserve");
    let erased = Erased::new(&frame);
    let alloc = erased.handle();
    let ints: &[(&str, Value<Alloc<'_>>)] = &[
        ("00", value!(in alloc; 0)),
        ("01", value!(in alloc; 1)),
        ("0a", value!(in alloc; 10)),
        ("17", value!(in alloc; 23)),
        ("1818", value!(in alloc; 24)),
        ("1819", value!(in alloc; 25)),
        ("1864", value!(in alloc; 100)),
        ("1903e8", value!(in alloc; 1000)),
        ("1a000f4240", value!(in alloc; 1_000_000)),
        ("1b000000e8d4a51000", value!(in alloc; 1_000_000_000_000u64)),
        ("1bffffffffffffffff", value!(in alloc; u64::MAX)),
        ("20", value!(in alloc; -1)),
        ("29", value!(in alloc; -10)),
        ("3863", value!(in alloc; -100)),
        ("3903e7", value!(in alloc; -1000)),
        ("3b7fffffffffffffff", value!(in alloc; i64::MIN)),
        ("f90000", value!(in alloc; 0.0)),
        ("f98000", value!(in alloc; -0.0)),
        ("f93c00", value!(in alloc; 1.0)),
        ("fb3ff199999999999a", value!(in alloc; 1.1)),
        ("f93e00", value!(in alloc; 1.5)),
        ("f97bff", value!(in alloc; 65504.0)),
        ("fa47c35000", value!(in alloc; 100_000.0)),
        ("fa7f7fffff", value!(in alloc; 3.4028234663852886e38)),
        ("fb7e37e43c8800759c", value!(in alloc; 1.0e300)),
        ("f90001", value!(in alloc; 5.960464477539063e-8)),
        ("f90400", value!(in alloc; 0.00006103515625)),
        ("f9c400", value!(in alloc; -4.0)),
        ("fbc010666666666666", value!(in alloc; -4.1)),
        ("f97c00", value!(in alloc; f64::INFINITY)),
        ("f97e00", value!(in alloc; f64::NAN)),
        ("f9fc00", value!(in alloc; f64::NEG_INFINITY)),
        ("f4", value!(in alloc; false)),
        ("f5", value!(in alloc; true)),
        ("f6", value!(in alloc; null)),
        ("60", value!(in alloc; "")),
        ("6161", value!(in alloc; "a")),
        ("6449455446", value!(in alloc; "IETF")),
        ("62225c", value!(in alloc; "\"\\")),
        ("62c3bc", value!(in alloc; "\u{fc}")),
        ("63e6b0b4", value!(in alloc; "\u{6c34}")),
        ("64f0908591", value!(in alloc; "\u{10151}")),
        ("80", value!(in alloc; [])),
        ("83010203", value!(in alloc; [1, 2, 3])),
        ("8301820203820405", value!(in alloc; [1, [2, 3], [4, 5]])),
        (
            "98190102030405060708090a0b0c0d0e0f101112131415161718181819",
            value!(in alloc; [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25]),
        ),
        ("a0", value!(in alloc; {})),
        ("a26161016162820203", value!(in alloc; {"a": 1, "b": [2, 3]})),
        ("826161a161626163", value!(in alloc; ["a", {"b": "c"}])),
        ("a56161614161626142616361436164614461656145", value!(in alloc; {"a": "A", "b": "B", "c": "C", "d": "D", "e": "E"})),
    ];
    for (text, value) in ints {
        let bytes = hex(text, alloc);
        assert_eq!(read(&bytes, alloc), *value, "{text}");
        assert_eq!(written(value, alloc), bytes, "{text}");
    }
    let mut b = Vec::new_in(alloc);
    b.extend_from_slice(&[1, 2, 3, 4]);
    let bytes = Value::<Alloc<'_>>::Bytes(b);
    assert_eq!(read(&hex("4401020304", alloc), alloc), bytes);
    assert_eq!(written(&bytes, alloc), hex("4401020304", alloc));
    assert_eq!(read(&hex("40", alloc), alloc), Value::Bytes(Vec::new_in(alloc)));
}

/// Appendix A's vectors that have no `Value` or break the deterministic rules: refused, by name.
#[test]
fn appendix_a_outside_the_rules_is_refused() {
    let frame = Frame::with_reserve(4usize << 30, Tag::Codegen).expect("reserve");
    let erased = Erased::new(&frame);
    let alloc = erased.handle();
    let cases: &[(&str, usize, &str)] = &[
        ("3bffffffffffffffff", 0, why::NEGATIVE_PAST_I64),
        ("c074323031332d30332d32315432303a30343a30305a", 0, why::TAG),
        ("c11a514b67b0", 0, why::TAG),
        ("f7", 0, why::SIMPLE),
        ("f0", 0, why::SIMPLE),
        ("f818", 0, why::SIMPLE),
        ("f8ff", 0, why::SIMPLE),
        ("a201020304", 1, why::KEY_NOT_TEXT),
        ("fa7f800000", 0, why::FLOAT_NOT_SHORTEST),
        ("fa7fc00000", 0, why::NAN_NOT_CANONICAL),
        ("fa ff800000", 0, why::FLOAT_NOT_SHORTEST),
        ("fb7ff0000000000000", 0, why::FLOAT_NOT_SHORTEST),
        ("fb7ff8000000000000", 0, why::NAN_NOT_CANONICAL),
        ("fbfff0000000000000", 0, why::FLOAT_NOT_SHORTEST),
        ("5f42010243030405ff", 0, why::INDEFINITE),
        ("7f657374726561646d696e67ff", 0, why::INDEFINITE),
        ("9fff", 0, why::INDEFINITE),
        ("9f018202039f0405ffff", 0, why::INDEFINITE),
        ("bf61610161629f0203ffff", 0, why::INDEFINITE),
        ("826161bf61626163ff", 3, why::INDEFINITE),
    ];
    for &(text, at, why) in cases {
        assert_eq!(refused(text, alloc), Error { at, why }, "{text}");
    }
}

/// Each rule of RFC 8949 4.2.1, a canonical input read and a non-canonical one refused.
#[test]
fn each_deterministic_rule_refuses_its_breach() {
    let frame = Frame::with_reserve(4usize << 30, Tag::Codegen).expect("reserve");
    let erased = Erased::new(&frame);
    let alloc = erased.handle();
    // Shortest heads: numbers, and every length's head.
    assert_eq!(read(&hex("17", alloc), alloc), value!(in alloc; 23));
    assert_eq!(refused("1817", alloc), Error { at: 0, why: why::NOT_SHORTEST });
    assert_eq!(refused("190017", alloc), Error { at: 0, why: why::NOT_SHORTEST });
    assert_eq!(refused("1a0000ffff", alloc), Error { at: 0, why: why::NOT_SHORTEST });
    assert_eq!(refused("1b00000000ffffffff", alloc), Error { at: 0, why: why::NOT_SHORTEST });
    assert_eq!(refused("3817", alloc), Error { at: 0, why: why::NOT_SHORTEST });
    assert_eq!(refused("5800", alloc), Error { at: 0, why: why::NOT_SHORTEST });
    assert_eq!(refused("780161", alloc), Error { at: 0, why: why::NOT_SHORTEST });
    assert_eq!(refused("9800", alloc), Error { at: 0, why: why::NOT_SHORTEST });
    assert_eq!(refused("b800", alloc), Error { at: 0, why: why::NOT_SHORTEST });
    assert_eq!(refused("1c", alloc), Error { at: 0, why: why::INDEFINITE });
    // Definite lengths only.
    assert_eq!(refused("5fff", alloc), Error { at: 0, why: why::INDEFINITE });
    assert_eq!(refused("bfff", alloc), Error { at: 0, why: why::INDEFINITE });
    assert_eq!(refused("ff", alloc), Error { at: 0, why: why::INDEFINITE });
    // Keys in their encodings' bytewise order, the shorter first, none twice.
    assert_eq!(read(&hex("a2 6162 01 626161 02", alloc), alloc), value!(in alloc; {"b": 1, "aa": 2}));
    assert_eq!(written(&value!(in alloc; {"aa": 2, "b": 1}), alloc), hex("a2 6162 01 626161 02", alloc));
    assert_eq!(refused("a2 626161 02 6162 01", alloc), Error { at: 5, why: why::KEY_ORDER });
    assert_eq!(refused("a2 6162 01 6161 02", alloc), Error { at: 4, why: why::KEY_ORDER });
    assert_eq!(refused("a2 6161 01 6161 02", alloc), Error { at: 4, why: why::KEY_TWICE });
    // Shortest exact floats; one NaN.
    assert_eq!(refused("fa3f800000", alloc), Error { at: 0, why: why::FLOAT_NOT_SHORTEST });
    assert_eq!(refused("fb3ff0000000000000", alloc), Error { at: 0, why: why::FLOAT_NOT_SHORTEST });
    assert_eq!(refused("fb3ff8000000000000", alloc), Error { at: 0, why: why::FLOAT_NOT_SHORTEST });
    assert_eq!(refused("fb47efffffe0000000", alloc), Error { at: 0, why: why::FLOAT_NOT_SHORTEST });
    assert_eq!(refused("f97e01", alloc), Error { at: 0, why: why::NAN_NOT_CANONICAL });
    assert_eq!(refused("f9fe00", alloc), Error { at: 0, why: why::NAN_NOT_CANONICAL });
    assert_eq!(written(&value!(in alloc; f64::from_bits(0x7ff8_0000_0000_0001)), alloc), hex("f97e00", alloc), "every NaN is one");
    assert_eq!(written(&value!(in alloc; 1.0e-7), alloc), hex("fb3e7ad7f29abcaf48", alloc));
    assert_eq!(written(&value!(in alloc; 0.5f32), alloc), hex("f93800", alloc));
    assert_eq!(written(&value!(in alloc; f64::from(f32::MIN_POSITIVE)), alloc), hex("fa00800000", alloc));
    // One value, then the end.
    assert_eq!(refused("0000", alloc), Error { at: 1, why: why::BYTES_AFTER });
    assert_eq!(refused("", alloc), Error { at: 0, why: why::END_AT_VALUE });
}

#[test]
fn bounds_are_parameters_and_refused() {
    let frame = Frame::with_reserve(4usize << 30, Tag::Codegen).expect("reserve");
    let erased = Erased::new(&frame);
    let alloc = erased.handle();
    let limits = Limits { most_depth: 2, ..Limits::DEFAULT };
    assert!(read_in(&hex("818100", alloc), limits, alloc).is_ok());
    assert_eq!(read_in(&hex("81818100", alloc), limits, alloc).unwrap_err(), Error { at: 2, why: why::TOO_DEEP });
    assert_eq!(read_in(&hex("a1616181a0", alloc), limits, alloc).unwrap_err(), Error { at: 4, why: why::TOO_DEEP });
    let limits = Limits { most_bytes: 2, ..Limits::DEFAULT };
    assert_eq!(read_in(&hex("1903e8", alloc), limits, alloc).unwrap_err(), Error { at: 0, why: why::TOO_LARGE });
    // A length is checked against the input before anything is sized by it.
    assert_eq!(refused("9affffffff00", alloc), Error { at: 0, why: why::ARRAY_PAST_INPUT });
    assert_eq!(refused("baffffffff00", alloc), Error { at: 0, why: why::MAP_PAST_INPUT });
    assert_eq!(refused("a2616100", alloc), Error { at: 0, why: why::MAP_PAST_INPUT });
    assert_eq!(refused("5affffffff00", alloc), Error { at: 0, why: why::BYTES_PAST_INPUT });
    assert_eq!(refused("7b00000001000000000000", alloc), Error { at: 0, why: why::TEXT_PAST_INPUT });
    assert_eq!(refused("62c3", alloc), Error { at: 0, why: why::TEXT_PAST_INPUT });
    assert_eq!(refused("61ff", alloc), Error { at: 0, why: why::NOT_UTF8 });
    assert_eq!(refused("1903", alloc), Error { at: 1, why: why::END_IN_VALUE });
    assert_eq!(refused("8201", alloc), Error { at: 0, why: why::ARRAY_PAST_INPUT });
    assert_eq!(refused("a16161", alloc), Error { at: 3, why: why::END_AT_VALUE });
}

/// `body` on a thread with a 64 KiB stack, where a recursion on depth overflows long before a
/// million levels.
fn small_stack(body: impl FnOnce() + Send + 'static) {
    let thread = std::thread::Builder::new().stack_size(64 << 10).spawn(body).unwrap();
    thread.join().unwrap();
}

/// `n` levels, one-element arrays and one-entry maps (key `"k"`) by turns, around a `0`.
fn nested<'a>(n: usize, alloc: Alloc<'a>) -> AVec<'a, u8> {
    let mut bytes = Vec::new_in(alloc);
    for i in 0..n {
        match i % 2 {
            0 => bytes.push(0x81),
            _ => bytes.extend_from_slice(&[0xa1, 0x61, b'k']),
        }
    }
    bytes.push(0x00);
    bytes
}

#[test]
fn deep_nesting_reads_writes_and_drops_in_a_small_stack() {
    let frame = Frame::with_reserve(4usize << 30, Tag::Codegen).expect("reserve");
    small_stack(move || {
        let erased = Erased::new(&frame);
        let alloc = erased.handle();
        for (n, limits) in [(128, Limits::DEFAULT), (1_000_000, Limits { most_depth: 1_000_000, ..Limits::DEFAULT })] {
            let bytes = nested(n, alloc);
            let value = read_in(&bytes, limits, alloc).unwrap();
            assert_eq!(to_vec_in(&value, alloc).as_slice(), bytes.as_slice(), "write(read(bytes)) is the bytes");
            let copy = value.copy_in(alloc);
            assert_eq!(to_vec_in(&copy, alloc).as_slice(), bytes.as_slice(), "the copy writes the same");
            assert_eq!(read_in(&nested(n + 1, alloc), limits, alloc).unwrap_err().why, why::TOO_DEEP);
            drop(value);
            drop(copy);
        }
    });
}

/// The field reader blaze-pack's and blaze-watch's records are read with, as it was moved.
#[test]
fn the_field_reader_reads_records() {
    let frame = Frame::with_reserve(4usize << 30, Tag::Codegen).expect("reserve");
    let erased = Erased::new(&frame);
    let alloc = erased.handle();
    let mut out = Vec::new_in(alloc);
    crate::array(&mut out, 4);
    crate::uint(&mut out, 500);
    crate::text(&mut out, "name");
    crate::bytes(&mut out, &[7; 32]);
    crate::boolean(&mut out, true);
    let mut r = Reader::new(out.as_slice());
    r.array_of(4).unwrap();
    assert_eq!(r.uint_most(1000, "too many").unwrap(), 500);
    assert_eq!(r.text(3, "a name past three bytes"), Err(Error { at: 4, why: "a name past three bytes" }));
    assert_eq!(r.text(64, "a name past 64 bytes").unwrap(), "name");
    assert_eq!(r.fixed::<32>().unwrap(), [7; 32]);
    assert!(r.boolean().unwrap());
    r.end().unwrap();
    let mut r = Reader::new(out.as_slice());
    assert_eq!(r.array_of(3), Err(Error { at: 0, why: why::RECORD_LENGTH }));
    assert_eq!(Head::new(major::TEXT, 300).as_bytes(), [0x79, 0x01, 0x2c]);
    assert_eq!(crate::head_len(300), 3);
    let mut r = Reader::new(&[0x18, 0x17]);
    assert_eq!(r.uint(), Err(Error { at: 0, why: why::NOT_SHORTEST }));
    assert_eq!(r.at(), 0, "a refusal leaves the reader at the value");
}

#[test]
fn events_stream_both_ways() {
    let frame = Frame::with_reserve(4usize << 30, Tag::Codegen).expect("reserve");
    let erased = Erased::new(&frame);
    let alloc = erased.handle();
    let bytes = hex("a2 6161 01 6162 82 f6 43010203", alloc);
    let mut decoder = Decoder::new_in(&bytes, Limits::DEFAULT, alloc).unwrap();
    let mut events = Vec::new_in(alloc);
    while let Some(item) = decoder.next().unwrap() {
        events.push((item.at, item.event));
    }
    assert_eq!(
        events,
        [
            (0, Event::StartMap { len: 2 }),
            (1, Event::Key("a")),
            (3, Event::Int(Int::from_u64(1))),
            (4, Event::Key("b")),
            (6, Event::StartList { len: 2 }),
            (7, Event::Null),
            (8, Event::Bytes(&[1, 2, 3])),
            (12, Event::EndList),
            (12, Event::EndMap),
        ]
    );
    let mut out = Vec::new_in(alloc);
    let mut encoder = Encoder::new_in(&mut out, alloc);
    for (_, event) in &events {
        encoder.event(event).unwrap();
    }
    encoder.finish().unwrap();
    assert_eq!(out.as_slice(), bytes.as_slice());

    let mut out = Vec::new_in(alloc);
    let mut e = Encoder::new_in(&mut out, alloc);
    assert_eq!(e.event(&Event::Key("a")).unwrap_err().what, StreamWhat::KeyNotDue);
    e.event(&Event::StartMap { len: 2 }).unwrap();
    assert_eq!(e.event(&Event::Null).unwrap_err().what, StreamWhat::KeyDue);
    e.event(&Event::Key("b")).unwrap();
    e.event(&Event::Null).unwrap();
    assert_eq!(e.event(&Event::Key("a")).unwrap_err().what, StreamWhat::KeyOrder, "refused, not moved");
    assert_eq!(e.event(&Event::Key("b")).unwrap_err().what, StreamWhat::KeyTwice);
    assert_eq!(e.event(&Event::EndMap).unwrap_err().what, StreamWhat::TooFew);
    assert_eq!(e.event(&Event::EndList).unwrap_err().what, StreamWhat::Unbalanced);
    e.event(&Event::Key("aa")).unwrap();
    e.event(&Event::Bool(true)).unwrap();
    e.event(&Event::EndMap).unwrap();
    assert_eq!(e.event(&Event::Null).unwrap_err().what, StreamWhat::AfterEnd);
    e.finish().unwrap();
    assert_eq!(out.as_slice(), hex("a2 6162 f6 626161 f5", alloc).as_slice());
    let mut out = Vec::new_in(alloc);
    let mut e = Encoder::new_in(&mut out, alloc);
    e.event(&Event::StartList { len: 0 }).unwrap_or_else(|error| panic!("{error}"));
    let mut out2 = Vec::new_in(alloc);
    let mut e2 = Encoder::new_in(&mut out2, alloc);
    e2.event(&Event::StartList { len: 1 }).unwrap();
    assert_eq!(e2.finish().unwrap_err().what, StreamWhat::Unfinished);
    e.event(&Event::EndList).unwrap();
    e.finish().unwrap();
}

/// A value from the tape, `depth` levels at most; floats finite or the one NaN.
fn draw_value<'a>(tape: &Tape, depth: u32, alloc: Alloc<'a>) -> Value<Alloc<'a>> {
    let kinds: u8 = if depth == 0 { 5 } else { 7 };
    match tape.int_in(0u8..=kinds, "kind") {
        0 => Value::Null,
        1 => Value::Bool(tape.coin("bool")),
        2 => match tape.coin("signed") {
            true => Value::Int(Int::from_i64(tape.int_in(i64::MIN..=i64::MAX, "i64"))),
            false => Value::Int(Int::from_u64(tape.int_in(0..=u64::MAX, "u64"))),
        },
        3 => Value::Float(match tape.int_in(0u8..=3, "float kind") {
            0 => f64::NAN,
            1 => f64::from(f32::from_bits(tape.int_in(0..=u32::MAX, "f32 bits"))),
            _ => tape.f64_finite("float"),
        }),
        4 => Value::Text(String::from_str_in(&tape.text(0..=8, "text"), alloc)),
        5 => {
            let mut bytes = Vec::new_in(alloc);
            bytes.extend_from_slice(&tape.bytes(0..=8, "bytes"));
            Value::Bytes(bytes)
        }
        6 => {
            let mut list = Vec::new_in(alloc);
            for item in tape.vec(0..=4, "list", |t| draw_value(t, depth - 1, alloc)) {
                list.push(item);
            }
            Value::List(list)
        }
        _ => {
            let mut map = Map::new_in(alloc);
            for (key, value) in tape.vec(0..=4, "map", |t| (t.text_from("ab", 0..=3, "key"), draw_value(t, depth - 1, alloc))) {
                map.insert(String::from_str_in(&key, alloc), value);
            }
            Value::Map(map)
        }
    }
}

/// read(write(v)) == v (a NaN read back as the one NaN), the bytes written twice equal, and
/// the decoder's events through the encoder give the same bytes.
fn round_trip(tape: &Tape, alloc: Alloc<'_>, plant: impl Planted<CborPlant>) -> Result<(), Broken<TripBroken>> {
    let v = draw_value(tape, 3, alloc);
    let mut bytes = Vec::new_in(alloc);
    write_into_planted(&v, &mut bytes, alloc, plant);
    let back = read_in(bytes.as_slice(), Limits::SHIPPED, alloc).map_err(|error| {
        Broken::Oracle(TripBroken::Unread { value: format!("{v:?}"), bytes: bytes.as_slice().to_vec(), error })
    })?;
    let canonical = |x: &Value<Alloc<'_>>| to_vec_in(x, alloc);
    if canonical(&back) != bytes.as_slice() || (!has_nan(&v) && back != v) {
        return Err(Broken::Oracle(TripBroken::Differs { value: format!("{v:?}"), back: format!("{back:?}") }));
    }
    let mut out = Vec::new_in(alloc);
    let mut encoder = Encoder::new_in(&mut out, alloc);
    let mut decoder =
        Decoder::new_in(bytes.as_slice(), Limits::SHIPPED, alloc).map_err(|e| Broken::Harness(e.to_string()))?;
    while let Some(item) = decoder.next().map_err(|error| Broken::Oracle(TripBroken::Decoder { error }))? {
        encoder.event(&item.event).map_err(|error| Broken::Oracle(TripBroken::Encoder { error }))?;
    }
    encoder.finish().map_err(|error| Broken::Oracle(TripBroken::Encoder { error }))?;
    if out.as_slice() != bytes.as_slice() {
        return Err(Broken::Oracle(TripBroken::Events { events: out.as_slice().to_vec(), bytes: bytes.as_slice().to_vec() }));
    }
    Ok(())
}

/// The round trip's invariants, each its own value (D165: a control names one by pattern).
#[derive(Clone, PartialEq, Debug)]
enum TripBroken {
    /// The bytes written for a value were refused by the reader.
    Unread { value: StdString, bytes: StdVec<u8>, error: Error },
    /// The value read back is not the one written, or writes other bytes.
    Differs { value: StdString, back: StdString },
    /// The decoder refused bytes the reader took.
    Decoder { error: Error },
    /// The encoder refused the decoder's events.
    Encoder { error: StreamError },
    /// The decoder's events through the encoder gave other bytes.
    Events { events: StdVec<u8>, bytes: StdVec<u8> },
}

impl fmt::Display for TripBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TripBroken::Unread { value, bytes, error } => {
                write!(f, "the round trip of {value} wrote {bytes:02x?}: {error}")
            }
            TripBroken::Differs { value, back } => write!(f, "the round trip of {value} gave {back}"),
            TripBroken::Decoder { error } => write!(f, "the decoder: {error}"),
            TripBroken::Encoder { error } => write!(f, "the encoder: {error}"),
            TripBroken::Events { events, bytes } => write!(f, "events gave {events:02x?}, not {bytes:02x?}"),
        }
    }
}

blaze_sim::plants! {
    /// The bugs planted in the writer, each with the invariant that must catch it.
    enum TripPlant breaks TripBroken {
        /// The writer as it is.
        None,
        /// Keys written in their bytes' order: the reader refuses the order of their encodings.
        KeysBytewise => TripBroken::Unread { error: Error { why: why::KEY_ORDER, .. }, .. },
    }
}

impl TripPlant {
    /// The writer's plant this control plants.
    fn planted(self) -> CborPlant {
        match self {
            TripPlant::None => CborPlant::None,
            TripPlant::KeysBytewise => CborPlant::KeysBytewise,
        }
    }
}

fn has_nan(v: &Value<Alloc<'_>>) -> bool {
    match v {
        Value::Float(x) => x.is_nan(),
        Value::List(list) => list.iter().any(has_nan),
        Value::Map(map) => map.iter().any(|(_, v)| has_nan(v)),
        _ => false,
    }
}

#[test]
fn the_round_trip_holds() {
    let frame = Frame::with_reserve(4usize << 30, Tag::Codegen).expect("reserve");
    let erased = Erased::new(&frame);
    let alloc = erased.handle();
    check(Config::cases(400), |tape| round_trip(tape, alloc, NoPlant));
}

#[test]
fn every_writer_plant_is_caught_by_its_invariant() {
    let frame = Frame::with_reserve(4usize << 30, Tag::Codegen).expect("reserve");
    let erased = Erased::new(&frame);
    let alloc = erased.handle();
    controls(Config::cases(400), |tape, plant: TripPlant| round_trip(tape, alloc, plant.planted()));
}

// The decoder's caps (D165): drawn down to their least legal values, each reached.

impl blaze_sim::Limits for Limits {
    const SHIPPED: Limits = Limits::SHIPPED;

    fn draw(tape: &Tape) -> Limits {
        Limits {
            most_bytes: tape.limit(Limits::SHIPPED.most_bytes, Limits::SMALLEST.most_bytes, "most bytes"),
            most_depth: tape.limit(Limits::SHIPPED.most_depth, Limits::SMALLEST.most_depth, "most depth"),
        }
    }
}

blaze_sim::points! {
    /// Where the decoder's limits simulation has to get.
    enum CapPoint {
        /// An input was refused for passing `most_bytes`.
        TooLarge,
        /// A container was refused for passing `most_depth`.
        TooDeep,
    }
}

/// A read held to its drawn limits: refused for a cap only past that cap, and otherwise read.
#[derive(Clone, PartialEq, Eq, Debug)]
enum CapBroken {
    /// Refused as too large at or under `most_bytes`.
    LargeUnder { len: usize, most: usize },
    /// Refused as too deep at or under `most_depth`.
    DeepUnder { depth: usize, most: usize },
    /// Read although past a cap, or refused for another reason within both.
    Wrong { len: usize, depth: usize, limits: Limits, got: StdString },
}

impl fmt::Display for CapBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CapBroken::LargeUnder { len, most } => write!(f, "{len} bytes refused as past a cap of {most}"),
            CapBroken::DeepUnder { depth, most } => write!(f, "{depth} deep refused as past a cap of {most}"),
            CapBroken::Wrong { len, depth, limits, got } => {
                write!(f, "{len} bytes {depth} deep under {limits:?}: {got}")
            }
        }
    }
}

/// How deeply `v`'s lists and maps nest: 0 for a scalar.
fn depth_of(v: &Value<Alloc<'_>>) -> usize {
    match v {
        Value::List(list) => 1 + list.iter().map(depth_of).max().unwrap_or(0),
        Value::Map(map) => 1 + map.iter().map(|(_, v)| depth_of(v)).max().unwrap_or(0),
        _ => 0,
    }
}

fn caps(tape: &Tape, alloc: Alloc<'_>) -> Result<(), Broken<CapBroken>> {
    let limits: Limits = tape.limits();
    let v = draw_value(tape, 3, alloc);
    let bytes = to_vec_in(&v, alloc);
    let (len, depth) = (bytes.len(), depth_of(&v));
    let wrong = |got: StdString| Broken::Oracle(CapBroken::Wrong { len, depth, limits, got });
    match read_in(bytes.as_slice(), limits, alloc) {
        Err(Error { why: why::TOO_LARGE, .. }) => {
            tape.reach(CapPoint::TooLarge);
            match len > limits.most_bytes {
                true => Ok(()),
                false => Err(Broken::Oracle(CapBroken::LargeUnder { len, most: limits.most_bytes })),
            }
        }
        Err(Error { why: why::TOO_DEEP, .. }) => {
            tape.reach(CapPoint::TooDeep);
            match depth > limits.most_depth {
                true => Ok(()),
                false => Err(Broken::Oracle(CapBroken::DeepUnder { depth, most: limits.most_depth })),
            }
        }
        Err(error) => Err(wrong(error.to_string())),
        Ok(_) if len > limits.most_bytes || depth > limits.most_depth => Err(wrong(StdString::from("read"))),
        Ok(_) => Ok(()),
    }
}

#[test]
fn the_decoder_holds_each_drawn_cap_and_reaches_it() {
    let frame = Frame::with_reserve(4usize << 30, Tag::Codegen).expect("reserve");
    let erased = Erased::new(&frame);
    let alloc = erased.handle();
    check(Config::cases(400).must_reach_points(<CapPoint as Point>::IDS), |tape| caps(tape, alloc));
}
