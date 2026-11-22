//! RFC 8259's strictness case by case with offsets, deep nesting against the bound, big
//! numbers, escapes and surrogate pairs, invalid UTF-8; the writers' exact text; the round
//! trips with their planted-bug controls; and the reader's caps, each drawn and reached.

use core::fmt;
use std::{format, string::String as StdString, string::ToString, vec::Vec as StdVec};

use blaze_data::{Int, Map, Value, value};
use blaze_own::{Alloc, String};
use blaze_sim::{Broken, Config, NoPlant, Planted, Point, Tape, check, controls};

use crate::{
    BytesAs, Duplicates, Error, Event, Expected, JsonPlant, KeyOrder, Limits, Reader, StreamError, StreamWhat, Style,
    Unwritable, What, WriteOptions, Writer, line_col, read_in, to_string_in, write_into_planted,
};

/// The test's allocator (D102): the process heap's handle, never `Global`.
fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

/// A value in the test's allocator.
type V = Value<Alloc<'static>>;

/// A plain struct for the typed shortcuts.
struct Run<A: core::alloc::Allocator> {
    suite: String<A>,
    timeout: u32,
    ids: Option<blaze_own::Vec<u64, A>>,
}
blaze_data::fields!(for<A> Run<A> { suite, timeout; ids });

/// The `_in` forms: `from_slice`, `to_string` and `to_string_pretty` take `A::default()`, which
/// no allocator but `Global` has, so they are the same calls with the allocator named here.
#[test]
fn typed_shortcuts_read_strictly_and_write() {
    use crate::{FromJsonError, from_slice_in, to_string_of_in};

    let read = |bytes: &[u8]| from_slice_in::<Run<Alloc<'static>>, Alloc<'static>>(bytes, Limits::SHIPPED, heap());
    let run = read(br#"{"suite": "unit", "timeout": 30}"#).unwrap();
    assert_eq!((run.suite.as_str(), run.timeout, run.ids.is_none()), ("unit", 30, true));
    assert_eq!(to_string_of_in(&run, WriteOptions::COMPACT, heap()).unwrap().as_str(), r#"{"suite":"unit","timeout":30}"#);
    assert_eq!(
        to_string_of_in(&run, WriteOptions::PRETTY, heap()).unwrap().as_str(),
        "{\n  \"suite\": \"unit\",\n  \"timeout\": 30\n}"
    );

    let unknown = read(br#"{"suite": "u", "timeout": 1, "timout": 2}"#);
    let Err(FromJsonError::Shape(error)) = unknown else { panic!("an unknown field is refused") };
    assert!(error.to_string().contains("unknown field \"timout\""), "{error}");
    let syntax = read(b"{\"suite\": ");
    assert!(matches!(syntax, Err(FromJsonError::Syntax(Error { at: 10, .. }))));
    let kind = read(br#"{"suite": "u", "timeout": "30"}"#);
    let Err(FromJsonError::Shape(error)) = kind else { panic!("text is no integer") };
    assert_eq!(error.to_string(), "timeout: expected integer, found text");
}

fn read(text: &str) -> V {
    match read_in(text.as_bytes(), Limits::SHIPPED, heap()) {
        Ok(value) => value,
        Err(error) => panic!("{text:?}: {error}"),
    }
}

fn refused(text: &[u8]) -> Error {
    match read_in(text, Limits::SHIPPED, heap()) {
        Ok(value) => panic!("{:?} read as {value:?}", StdString::from_utf8_lossy(text)),
        Err(error) => error,
    }
}

fn compact(value: &V) -> StdString {
    to_string_in(value, WriteOptions::COMPACT, heap()).unwrap().as_str().to_string()
}

#[test]
fn values_read_as_written() {
    let v = read(r#" {"a": [1, -2, 2.5, "x", true, false, null], "b": {}, "c": []} "#);
    assert_eq!(v, value!(in heap(); {"a": [1, -2, 2.5, "x", true, false, null], "b": {}, "c": []}));
    let keys: StdVec<&str> = v.as_map().unwrap().iter().map(|(k, _)| k).collect();
    assert_eq!(keys, ["a", "b", "c"], "the text's order");
    assert_eq!(read("0"), V::Int(Int::from_u64(0)));
    assert_eq!(read("-0.0").as_f64().map(f64::to_bits), Some((-0.0f64).to_bits()));
    assert_eq!(read("1E+2").as_f64(), Some(100.0));
    assert_eq!(read("1e-400").as_f64(), Some(0.0), "underflow is zero, as IEEE has it");
}

#[test]
fn big_numbers_are_exact_or_refused() {
    assert_eq!(read("18446744073709551615").as_u64(), Some(u64::MAX));
    assert_eq!(read("-9223372036854775808").as_i64(), Some(i64::MIN));
    assert_eq!(refused(b"18446744073709551616"), Error { at: 0, what: What::IntegerOutOfRange });
    assert_eq!(refused(b"[-9223372036854775809]"), Error { at: 1, what: What::IntegerOutOfRange });
    assert_eq!(refused(b"1e400"), Error { at: 0, what: What::FloatOutOfRange });
    assert_eq!(read("9007199254740993").as_u64(), Some(9007199254740993), "never through a float");
    let long = format!("1{}", "0".repeat(100));
    assert_eq!(refused(long.as_bytes()).what, What::NumberTooLong { most: 100 });
}

#[test]
fn escapes_and_surrogate_pairs() {
    assert_eq!(read(r#""\"\\\/\b\f\n\r\té\u0000""#), value!(in heap(); "\"\\/\u{8}\u{c}\n\r\té\u{0}"));
    assert_eq!(read(r#""😀""#), value!(in heap(); "\u{1F600}"));
    assert_eq!(read(r#""😀""#), value!(in heap(); "\u{1F600}"));
    assert_eq!(read("\"é😀\""), value!(in heap(); "é😀"));
    assert_eq!(refused(br#""\ud800""#), Error { at: 1, what: What::LoneSurrogate });
    assert_eq!(refused(br#""\ud800A""#), Error { at: 1, what: What::LoneSurrogate });
    assert_eq!(refused(br#""\udc00""#), Error { at: 1, what: What::LoneSurrogate });
    assert_eq!(refused(br#""\x""#), Error { at: 2, what: What::BadEscape { found: 'x' } });
    assert_eq!(refused(br#""\u12g4""#), Error { at: 3, what: What::Unexpected { found: '1', expected: Expected::Hex } });
    assert_eq!(refused(b"\"a\nb\""), Error { at: 2, what: What::Control { byte: b'\n' } });
    assert_eq!(refused(b"\"abc"), Error { at: 4, what: What::End { expected: Expected::Quote } });
}

#[test]
fn invalid_utf8_is_refused_where_it_starts() {
    assert_eq!(refused(b"\"ab\xff\""), Error { at: 3, what: What::Utf8 });
    assert_eq!(refused(b"[\"\xc3\"]"), Error { at: 2, what: What::Utf8 }, "a cut sequence");
    assert_eq!(refused(b"\"\xed\xa0\x80\""), Error { at: 1, what: What::Utf8 }, "an encoded surrogate");
}

#[test]
fn what_rfc_8259_leaves_out_is_refused() {
    let at = |text: &str, at: usize, what: What| assert_eq!(refused(text.as_bytes()), Error { at, what }, "{text:?}");
    let unexpected = |found, expected| What::Unexpected { found, expected };
    at("", 0, What::End { expected: Expected::Value });
    at("   ", 3, What::End { expected: Expected::Value });
    at("[1,]", 3, unexpected(']', Expected::Value));
    at("{\"a\":1,}", 7, unexpected('}', Expected::Key));
    at("[1 2]", 3, unexpected('2', Expected::CommaOrEndList));
    at("{\"a\" 1}", 5, unexpected('1', Expected::Colon));
    at("{a:1}", 1, unexpected('a', Expected::KeyOrEndMap));
    at("// c\n1", 0, unexpected('/', Expected::Value));
    at("[1] /* c */", 4, unexpected('/', Expected::End));
    at("'a'", 0, unexpected('\'', Expected::Value));
    at("01", 0, What::LeadingZero);
    at("-01", 1, What::LeadingZero);
    at("+1", 0, unexpected('+', Expected::Value));
    at("1.", 2, What::End { expected: Expected::Digit });
    at(".5", 0, unexpected('.', Expected::Value));
    at("1e", 2, What::End { expected: Expected::Digit });
    at("-", 1, What::End { expected: Expected::Digit });
    at("NaN", 0, unexpected('N', Expected::Value));
    at("Infinity", 0, unexpected('I', Expected::Value));
    at("tru", 3, What::End { expected: Expected::Literal("true") });
    at("nul1", 3, unexpected('1', Expected::Literal("null")));
    at("1 2", 2, unexpected('2', Expected::End));
    at("{\"a\":1,\"a\":2}", 7, What::DuplicateKey);
    at("[1", 2, What::End { expected: Expected::CommaOrEndList });
    at("{\"a\":1", 6, What::End { expected: Expected::CommaOrEndMap });
    let last = Limits { duplicates: Duplicates::LastWins, ..Limits::SHIPPED };
    let v = read_in(b"{\"a\":1,\"b\":0,\"a\":2}", last, heap()).unwrap();
    assert_eq!(v.get("a").and_then(V::as_u64), Some(2));
    assert_eq!(v.as_map().unwrap().iter().next().map(|(k, _)| k), Some("a"), "the first place is kept");
}

#[test]
fn nesting_is_bounded() {
    let deep = |n: usize| format!("{}{}", "[".repeat(n), "]".repeat(n));
    assert!(read_in(deep(128).as_bytes(), Limits::SHIPPED, heap()).is_ok());
    assert_eq!(refused(deep(129).as_bytes()), Error { at: 128, what: What::TooDeep { most: 128 } });
    let small = Limits { most_bytes: 3, ..Limits::SHIPPED };
    assert_eq!(read_in(b"[1] ", small, heap()).unwrap_err().what, What::TooLarge { len: 4, most: 3 });
}

/// `body` on a thread with a 64 KiB stack, where a recursion on depth overflows long before a
/// million levels. The allocator handle is not `Send`, so `body` takes its own from `heap()`.
fn small_stack(body: impl FnOnce() + Send + 'static) {
    let thread = std::thread::Builder::new().stack_size(64 << 10).spawn(body).unwrap();
    thread.join().unwrap();
}

/// `n` levels, lists and maps by turns, around a `0`.
fn nested_text(n: usize) -> StdString {
    let mut text = StdString::new();
    for i in 0..n {
        text.push_str(if i % 2 == 0 { "[" } else { "{\"k\":" });
    }
    text.push('0');
    for i in (0..n).rev() {
        text.push(if i % 2 == 0 { ']' } else { '}' });
    }
    text
}

#[test]
fn deep_nesting_reads_writes_and_drops_in_a_small_stack() {
    for (n, limits) in [(128, Limits::SHIPPED), (1_000_000, Limits { most_depth: 1_000_000, ..Limits::SHIPPED })] {
        small_stack(move || {
            let alloc = heap();
            let text = nested_text(n);
            let value = read_in(text.as_bytes(), limits, alloc).unwrap();
            let written = to_string_in(&value, WriteOptions::COMPACT, alloc).unwrap();
            assert_eq!(written.as_str(), text, "write(read(text)) is the text");
            // Pretty text indents each level, so it is quadratic in the depth: the default only.
            let options = if n <= 128 { WriteOptions::PRETTY } else { WriteOptions::COMPACT };
            let pretty = to_string_in(&value, options, alloc).unwrap();
            // Compared as text: `==` on values recurses, the one walk here that may.
            let again = read_in(pretty.as_bytes(), limits, alloc).unwrap().copy_in(alloc);
            assert_eq!(to_string_in(&again, WriteOptions::COMPACT, alloc).unwrap().as_str(), text);
            let mut out = String::new_in(alloc);
            let mut writer = Writer::new_in(&mut out, alloc);
            let mut reader = Reader::new_in(text.as_bytes(), limits, alloc).unwrap();
            while let Some(item) = reader.next().unwrap() {
                writer.event(&item.event).unwrap();
            }
            writer.finish().unwrap();
            assert_eq!(out.as_str(), text, "the event writer at the same depth");
            let past = nested_text(n + 1);
            assert_eq!(read_in(past.as_bytes(), limits, alloc).unwrap_err().what, What::TooDeep { most: n });
            drop(value);
        });
    }
}

#[test]
fn errors_carry_line_and_column() {
    let text = "{\n  \"é\": [1,\n  ]\n}";
    let error = refused(text.as_bytes());
    assert_eq!(error.line_col(text.as_bytes()), (3, 3));
    assert_eq!(line_col(b"ab\ncd", 4), (2, 2));
    assert_eq!(line_col("é\nx".as_bytes(), 2), (1, 2), "columns count characters");
    assert!(error.to_string().contains("byte"), "{error}");
}

#[test]
fn the_writer_is_deterministic() {
    let v = value!(in heap(); {"b": 1, "a": {"d": [1, 2.5, 1e100, 0.1], "c": "q\"\\\n\u{1}é"}, "": null});
    assert_eq!(compact(&v), r#"{"":null,"a":{"c":"q\"\\\n\u0001é","d":[1,2.5,1e100,0.1]},"b":1}"#);
    let insertion = WriteOptions { keys: KeyOrder::Insertion, ..WriteOptions::COMPACT };
    assert_eq!(
        to_string_in(&v, insertion, heap()).unwrap().as_str(),
        r#"{"b":1,"a":{"d":[1,2.5,1e100,0.1],"c":"q\"\\\n\u0001é"},"":null}"#
    );
    let pretty = to_string_in(&value!(in heap(); {"a": [1, {}], "b": []}), WriteOptions::PRETTY, heap()).unwrap();
    assert_eq!(pretty.as_str(), "{\n  \"a\": [\n    1,\n    {}\n  ],\n  \"b\": []\n}");
    let one = WriteOptions { style: Style::Pretty { indent: 1 }, ..WriteOptions::PRETTY };
    assert_eq!(to_string_in(&value!(in heap(); []), one, heap()).unwrap().as_str(), "[]");
    assert_eq!(compact(&V::Float(1.0)), "1.0");
    assert_eq!(compact(&V::Float(-0.0)), "-0.0");
}

#[test]
fn what_json_cannot_say_is_refused_by_path() {
    let mut bytes = blaze_own::Vec::new_in(heap());
    bytes.extend_from_slice(b"hi");
    let mut v = value!(in heap(); {"a": [0, null]});
    v.get_mut("a").unwrap().as_list_mut().unwrap()[1] = Value::Bytes(bytes);
    let error = to_string_in(&v, WriteOptions::COMPACT, heap()).unwrap_err();
    assert_eq!((error.path.as_str(), error.what), ("a[1]", Unwritable::Bytes));
    let base64 = WriteOptions { bytes: BytesAs::Base64(blaze_base64::STANDARD), ..WriteOptions::COMPACT };
    assert_eq!(to_string_in(&v, base64, heap()).unwrap().as_str(), r#"{"a":[0,"aGk="]}"#);
    let nan = value!(in heap(); {"x y": f64::NAN});
    let error = to_string_in(&nan, WriteOptions::COMPACT, heap()).unwrap_err();
    assert_eq!(error.to_string(), r#""x y": NaN has no JSON form"#);
}

#[test]
fn the_reader_streams_events() {
    let text = br#"{"a": [1, "x\n"], "b": "y"}"#;
    let mut reader = Reader::new_in(text, Limits::SHIPPED, heap()).unwrap();
    let mut seen = StdVec::new();
    while let Some(item) = reader.next().unwrap() {
        let shown = match item.event {
            Event::Key(k) | Event::Text(k) => format!("{}:{}{}", item.at, k.raw(), if k.as_plain().is_some() { "" } else { "*" }),
            other => format!("{}:{other:?}", item.at),
        };
        seen.push(shown);
    }
    assert_eq!(
        seen,
        ["0:StartMap", "1:a", "6:StartList", "7:Int(Int(NonNegative(1)))", "10:x\\n*", "15:EndList", "18:b", "23:y", "26:EndMap"]
    );
}

#[test]
fn the_event_writer_checks_its_order() {
    let mut out = String::new_in(heap());
    let mut writer = Writer::new_in(&mut out, heap());
    let a = crate::Str::plain("a");
    assert_eq!(writer.event(&Event::Key(a)).unwrap_err().what, StreamWhat::KeyHere);
    writer.event(&Event::StartMap).unwrap();
    assert_eq!(writer.event(&Event::Null).unwrap_err().what, StreamWhat::ValueWithoutKey);
    writer.event(&Event::Key(a)).unwrap();
    assert_eq!(writer.event(&Event::EndMap).unwrap_err().what, StreamWhat::Unbalanced);
    writer.event(&Event::Float(f64::INFINITY)).unwrap_err();
    writer.event(&Event::Bool(true)).unwrap();
    writer.event(&Event::EndMap).unwrap();
    assert_eq!(writer.event(&Event::Null).unwrap_err().what, StreamWhat::AfterEnd);
    writer.finish().unwrap();
    assert_eq!(out.as_str(), r#"{"a":true}"#);
}

/// A value from the tape, `depth` levels at most.
fn draw_value(tape: &Tape, depth: u32) -> V {
    let kinds: u8 = if depth == 0 { 4 } else { 6 };
    match tape.int_in(0u8..=kinds, "kind") {
        0 => Value::Null,
        1 => Value::Bool(tape.coin("bool")),
        2 => match tape.coin("signed") {
            true => Value::Int(Int::from_i64(tape.int_in(i64::MIN..=i64::MAX, "i64"))),
            false => Value::Int(Int::from_u64(tape.int_in(0..=u64::MAX, "u64"))),
        },
        3 => Value::Float(tape.f64_finite("float")),
        4 => Value::Text(String::from_str_in(&tape.text(0..=8, "text"), heap())),
        5 => {
            let items = tape.vec(0..=4, "list", |t| draw_value(t, depth - 1));
            let mut list = blaze_own::Vec::new_in(heap());
            for item in items {
                list.push(item);
            }
            Value::List(list)
        }
        _ => {
            let mut map = Map::new_in(heap());
            for (key, value) in tape.vec(0..=4, "map", |t| (t.text_from("ab\"\\\u{1}é", 0..=3, "key"), draw_value(t, depth - 1))) {
                map.insert(String::from_str_in(&key, heap()), value);
            }
            Value::Map(map)
        }
    }
}

/// `value` as JSON in `options`, through the planted writer.
fn written(value: &V, options: WriteOptions, plant: impl Planted<JsonPlant>) -> Result<StdString, Broken<TripBroken>> {
    let mut out = String::new_in(heap());
    write_into_planted(value, &mut out, options, plant).map_err(|e| Broken::Harness(e.to_string()))?;
    Ok(out.as_str().to_string())
}

/// read(write(v)) == v in both styles, and the events copied through the writer give the
/// compact text back.
fn round_trip(tape: &Tape, plant: impl Planted<JsonPlant> + Copy) -> Result<(), Broken<TripBroken>> {
    let v = draw_value(tape, 3);
    for options in [WriteOptions::COMPACT, WriteOptions::PRETTY] {
        let text = written(&v, options, plant)?;
        match read_in(text.as_bytes(), Limits::SHIPPED, heap()) {
            Ok(back) if back == v => {}
            Ok(back) => {
                return Err(Broken::Oracle(TripBroken::Differs { value: format!("{v:?}"), back: format!("{back:?}") }));
            }
            Err(error) => return Err(Broken::Oracle(TripBroken::Unread { value: format!("{v:?}"), text, error })),
        }
    }
    let text = written(&v, WriteOptions::COMPACT, plant)?;
    let mut out = String::new_in(heap());
    let mut writer = Writer::new_in(&mut out, heap());
    let mut reader =
        Reader::new_in(text.as_bytes(), Limits::SHIPPED, heap()).map_err(|e| Broken::Harness(e.to_string()))?;
    while let Some(item) = reader.next().map_err(|error| Broken::Oracle(TripBroken::Reader { error }))? {
        writer.event(&item.event).map_err(|error| Broken::Oracle(TripBroken::Writer { error }))?;
    }
    writer.finish().map_err(|error| Broken::Oracle(TripBroken::Writer { error }))?;
    if out.as_str() != text.as_str() {
        return Err(Broken::Oracle(TripBroken::Events { events: out.as_str().to_string(), text }));
    }
    Ok(())
}

/// The round trip's invariants, each its own value (D165: a control names one by pattern).
#[derive(Clone, PartialEq, Debug)]
enum TripBroken {
    /// The text written for a value was refused by the reader.
    Unread { value: StdString, text: StdString, error: Error },
    /// The value read back is not the one written.
    Differs { value: StdString, back: StdString },
    /// The event reader refused text `read_in` took.
    Reader { error: Error },
    /// The event writer refused the reader's events.
    Writer { error: StreamError },
    /// The reader's events through the writer gave other text.
    Events { events: StdString, text: StdString },
}

impl fmt::Display for TripBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TripBroken::Unread { value, text, error } => write!(f, "the round trip of {value} wrote {text:?}: {error}"),
            TripBroken::Differs { value, back } => write!(f, "the round trip of {value} gave {back}"),
            TripBroken::Reader { error } => write!(f, "the round trip's reader: {error}"),
            TripBroken::Writer { error } => write!(f, "the round trip's writer: {error:?}"),
            TripBroken::Events { events, text } => write!(f, "the round trip of events gave {events:?}, not {text:?}"),
        }
    }
}

blaze_sim::plants! {
    /// The bugs planted in the writer, each with the invariant that must catch it.
    enum TripPlant breaks TripBroken {
        /// The writer as it is.
        None,
        /// A bare `"` ends its string early: the rest is refused, or (when the rest of the text
        /// reads as more JSON, `a","b` in a map's value) read as another value.
        QuoteUnescaped => TripBroken::Unread { .. } | TripBroken::Differs { .. },
    }
}

impl TripPlant {
    /// The writer's plant this control plants.
    fn planted(self) -> JsonPlant {
        match self {
            TripPlant::None => JsonPlant::None,
            TripPlant::QuoteUnescaped => JsonPlant::QuoteUnescaped,
        }
    }
}

#[test]
fn the_round_trip_holds() {
    check(Config::cases(400), |tape| round_trip(tape, NoPlant));
}

#[test]
fn every_writer_plant_is_caught_by_its_invariant() {
    controls(Config::cases(400), |tape, plant: TripPlant| round_trip(tape, plant.planted()));
}

// The reader's caps (D165): drawn down to their least legal values, each reached.

impl blaze_sim::Limits for Limits {
    const SHIPPED: Limits = Limits::SHIPPED;

    fn draw(tape: &Tape) -> Limits {
        Limits {
            most_bytes: tape.limit(Limits::SHIPPED.most_bytes, Limits::SMALLEST.most_bytes, "most bytes"),
            most_depth: tape.limit(Limits::SHIPPED.most_depth, Limits::SMALLEST.most_depth, "most depth"),
            most_number_len: tape.limit(
                Limits::SHIPPED.most_number_len,
                Limits::SMALLEST.most_number_len,
                "most number length",
            ),
            duplicates: Limits::SHIPPED.duplicates,
        }
    }
}

blaze_sim::points! {
    /// Where the reader's limits simulation has to get.
    enum CapPoint {
        /// A text was refused for passing `most_bytes`.
        TooLarge,
        /// A list or map was refused for passing `most_depth`.
        TooDeep,
        /// A number was refused for passing `most_number_len`.
        NumberTooLong,
    }
}

/// A read held to its drawn limits: refused for a cap only past that cap, and otherwise read.
#[derive(Clone, PartialEq, Eq, Debug)]
enum CapBroken {
    /// Refused as too large at or under `most_bytes`.
    LargeUnder { len: usize, most: usize },
    /// Refused as too deep at or under `most_depth`.
    DeepUnder { depth: usize, most: usize },
    /// Refused as a number too long when none is past `most_number_len`.
    NumberUnder { longest: usize, most: usize },
    /// Read although past a cap, or refused for another reason within all three.
    Wrong { len: usize, depth: usize, longest: usize, limits: Limits, got: StdString },
}

impl fmt::Display for CapBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CapBroken::LargeUnder { len, most } => write!(f, "{len} bytes refused as past a cap of {most}"),
            CapBroken::DeepUnder { depth, most } => write!(f, "{depth} deep refused as past a cap of {most}"),
            CapBroken::NumberUnder { longest, most } => {
                write!(f, "a number refused as past a cap of {most} when the longest is {longest} bytes")
            }
            CapBroken::Wrong { len, depth, longest, limits, got } => {
                write!(f, "{len} bytes {depth} deep, numbers up to {longest} bytes, under {limits:?}: {got}")
            }
        }
    }
}

/// How deeply `v`'s lists and maps nest: 0 for a scalar.
fn depth_of(v: &V) -> usize {
    match v {
        Value::List(list) => 1 + list.iter().map(depth_of).max().unwrap_or(0),
        Value::Map(map) => 1 + map.iter().map(|(_, v)| depth_of(v)).max().unwrap_or(0),
        _ => 0,
    }
}

/// The bytes of the longest number in `v` as the writer writes it: 0 for none.
fn longest_number(v: &V) -> usize {
    match v {
        Value::Int(_) | Value::Float(_) => compact(v).len(),
        Value::List(list) => list.iter().map(longest_number).max().unwrap_or(0),
        Value::Map(map) => map.iter().map(|(_, v)| longest_number(v)).max().unwrap_or(0),
        _ => 0,
    }
}

fn caps(tape: &Tape) -> Result<(), Broken<CapBroken>> {
    let limits: Limits = tape.limits();
    let v = draw_value(tape, 3);
    let text = compact(&v);
    let (len, depth, longest) = (text.len(), depth_of(&v), longest_number(&v));
    let wrong = |got: StdString| Broken::Oracle(CapBroken::Wrong { len, depth, longest, limits, got });
    match read_in(text.as_bytes(), limits, heap()) {
        Err(Error { what: What::TooLarge { .. }, .. }) => {
            tape.reach(CapPoint::TooLarge);
            match len > limits.most_bytes {
                true => Ok(()),
                false => Err(Broken::Oracle(CapBroken::LargeUnder { len, most: limits.most_bytes })),
            }
        }
        Err(Error { what: What::TooDeep { .. }, .. }) => {
            tape.reach(CapPoint::TooDeep);
            match depth > limits.most_depth {
                true => Ok(()),
                false => Err(Broken::Oracle(CapBroken::DeepUnder { depth, most: limits.most_depth })),
            }
        }
        Err(Error { what: What::NumberTooLong { .. }, .. }) => {
            tape.reach(CapPoint::NumberTooLong);
            match longest > limits.most_number_len {
                true => Ok(()),
                false => Err(Broken::Oracle(CapBroken::NumberUnder { longest, most: limits.most_number_len })),
            }
        }
        Err(error) => Err(wrong(error.to_string())),
        Ok(_) if len > limits.most_bytes || depth > limits.most_depth || longest > limits.most_number_len => {
            Err(wrong(StdString::from("read")))
        }
        Ok(_) => Ok(()),
    }
}

#[test]
fn the_reader_holds_each_drawn_cap_and_reaches_it() {
    check(Config::cases(400).must_reach_points(<CapPoint as Point>::IDS), caps);
}
