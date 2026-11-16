//! RFC 8259's strictness case by case with offsets, deep nesting against the bound, big
//! numbers, escapes and surrogate pairs, invalid UTF-8; the writers' exact text; and the round
//! trips with their planted-bug control.

use std::{alloc::Global, format, string::String as StdString, string::ToString, vec::Vec as StdVec};

use blaze_data::{Int, Map, Value, value};
use blaze_own::String;
use blaze_sim::{Broke, Config, Tape};

use crate::{
    BytesAs, Duplicates, Error, Event, Expected, KeyOrder, Limits, Reader, StreamWhat, Style, Unwritable,
    What, WriteOptions, Writer, line_col,
    plant::{self, Plant},
    read_in, to_string_in,
};

fn read(text: &str) -> Value<Global> {
    match read_in(text.as_bytes(), Limits::DEFAULT, Global) {
        Ok(value) => value,
        Err(error) => panic!("{text:?}: {error}"),
    }
}

fn refused(text: &[u8]) -> Error {
    match read_in(text, Limits::DEFAULT, Global) {
        Ok(value) => panic!("{:?} read as {value:?}", StdString::from_utf8_lossy(text)),
        Err(error) => error,
    }
}

fn compact(value: &Value<Global>) -> StdString {
    to_string_in(value, WriteOptions::COMPACT, Global).unwrap().as_str().to_string()
}

#[test]
fn values_read_as_written() {
    let v = read(r#" {"a": [1, -2, 2.5, "x", true, false, null], "b": {}, "c": []} "#);
    assert_eq!(v, value!(in Global; {"a": [1, -2, 2.5, "x", true, false, null], "b": {}, "c": []}));
    let keys: StdVec<&str> = v.as_map().unwrap().iter().map(|(k, _)| k).collect();
    assert_eq!(keys, ["a", "b", "c"], "the text's order");
    assert_eq!(read("0"), Value::<Global>::Int(Int::from_u64(0)));
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
    assert_eq!(read(r#""\"\\\/\b\f\n\r\té\u0000""#), value!(in Global; "\"\\/\u{8}\u{c}\n\r\té\u{0}"));
    assert_eq!(read(r#""😀""#), value!(in Global; "\u{1F600}"));
    assert_eq!(read(r#""😀""#), value!(in Global; "\u{1F600}"));
    assert_eq!(read("\"é😀\""), value!(in Global; "é😀"));
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
    let last = Limits { duplicates: Duplicates::LastWins, ..Limits::DEFAULT };
    let v = read_in(b"{\"a\":1,\"b\":0,\"a\":2}", last, Global).unwrap();
    assert_eq!(v.get("a").and_then(Value::as_u64), Some(2));
    assert_eq!(v.as_map().unwrap().iter().next().map(|(k, _)| k), Some("a"), "the first place is kept");
}

#[test]
fn nesting_is_bounded() {
    let deep = |n: usize| format!("{}{}", "[".repeat(n), "]".repeat(n));
    assert!(read_in(deep(128).as_bytes(), Limits::DEFAULT, Global).is_ok());
    assert_eq!(refused(deep(129).as_bytes()), Error { at: 128, what: What::TooDeep { most: 128 } });
    let small = Limits { most_bytes: 3, ..Limits::DEFAULT };
    assert_eq!(read_in(b"[1] ", small, Global).unwrap_err().what, What::TooLarge { len: 4, most: 3 });
}

/// `body` on a thread with a 64 KiB stack, where a recursion on depth overflows long before a
/// million levels.
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
    for (n, limits) in [(128, Limits::DEFAULT), (1_000_000, Limits { most_depth: 1_000_000, ..Limits::DEFAULT })] {
        small_stack(move || {
            let text = nested_text(n);
            let value = read_in(text.as_bytes(), limits, Global).unwrap();
            let written = to_string_in(&value, WriteOptions::COMPACT, Global).unwrap();
            assert_eq!(written.as_str(), text, "write(read(text)) is the text");
            // Pretty text indents each level, so it is quadratic in the depth: the default only.
            let options = if n <= 128 { WriteOptions::PRETTY } else { WriteOptions::COMPACT };
            let pretty = to_string_in(&value, options, Global).unwrap();
            // Compared as text: `==` on values recurses, the one walk here that may.
            let again = read_in(pretty.as_bytes(), limits, Global).unwrap().copy_in(Global);
            assert_eq!(to_string_in(&again, WriteOptions::COMPACT, Global).unwrap().as_str(), text);
            let mut out = String::new_in(Global);
            let mut writer = Writer::new_in(&mut out, Global);
            let mut reader = Reader::new_in(text.as_bytes(), limits, Global).unwrap();
            while let Some(item) = reader.next().unwrap() {
                writer.event(&item.event).unwrap();
            }
            writer.finish().unwrap();
            assert_eq!(out.as_str(), text, "the event writer at the same depth");
            let past = nested_text(n + 1);
            assert_eq!(read_in(past.as_bytes(), limits, Global).unwrap_err().what, What::TooDeep { most: n });
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
    let v = value!(in Global; {"b": 1, "a": {"d": [1, 2.5, 1e100, 0.1], "c": "q\"\\\n\u{1}é"}, "": null});
    assert_eq!(compact(&v), r#"{"":null,"a":{"c":"q\"\\\n\u0001é","d":[1,2.5,1e100,0.1]},"b":1}"#);
    let insertion = WriteOptions { keys: KeyOrder::Insertion, ..WriteOptions::COMPACT };
    assert_eq!(
        to_string_in(&v, insertion, Global).unwrap().as_str(),
        r#"{"b":1,"a":{"d":[1,2.5,1e100,0.1],"c":"q\"\\\n\u0001é"},"":null}"#
    );
    let pretty = to_string_in(&value!(in Global; {"a": [1, {}], "b": []}), WriteOptions::PRETTY, Global).unwrap();
    assert_eq!(pretty.as_str(), "{\n  \"a\": [\n    1,\n    {}\n  ],\n  \"b\": []\n}");
    let one = WriteOptions { style: Style::Pretty { indent: 1 }, ..WriteOptions::PRETTY };
    assert_eq!(to_string_in(&value!(in Global; []), one, Global).unwrap().as_str(), "[]");
    assert_eq!(compact(&Value::Float(1.0)), "1.0");
    assert_eq!(compact(&Value::Float(-0.0)), "-0.0");
}

#[test]
fn what_json_cannot_say_is_refused_by_path() {
    let mut bytes = blaze_own::Vec::new_in(Global);
    bytes.extend_from_slice(b"hi");
    let mut v = value!(in Global; {"a": [0, null]});
    v.get_mut("a").unwrap().as_list_mut().unwrap()[1] = Value::Bytes(bytes);
    let error = to_string_in(&v, WriteOptions::COMPACT, Global).unwrap_err();
    assert_eq!((error.path.as_str(), error.what), ("a[1]", Unwritable::Bytes));
    let base64 = WriteOptions { bytes: BytesAs::Base64(blaze_base64::STANDARD), ..WriteOptions::COMPACT };
    assert_eq!(to_string_in(&v, base64, Global).unwrap().as_str(), r#"{"a":[0,"aGk="]}"#);
    let nan = value!(in Global; {"x y": f64::NAN});
    let error = to_string_in(&nan, WriteOptions::COMPACT, Global).unwrap_err();
    assert_eq!(error.to_string(), r#""x y": NaN has no JSON form"#);
}

#[test]
fn the_reader_streams_events() {
    let text = br#"{"a": [1, "x\n"], "b": "y"}"#;
    let mut reader = Reader::new_in(text, Limits::DEFAULT, Global).unwrap();
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
    let mut out = String::new_in(Global);
    let mut writer = Writer::new_in(&mut out, Global);
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
fn draw_value(tape: &Tape, depth: u32) -> Value<Global> {
    let kinds: u8 = if depth == 0 { 4 } else { 6 };
    match tape.int_in(0u8..=kinds, "kind") {
        0 => Value::Null,
        1 => Value::Bool(tape.coin("bool")),
        2 => match tape.coin("signed") {
            true => Value::Int(Int::from_i64(tape.int_in(i64::MIN..=i64::MAX, "i64"))),
            false => Value::Int(Int::from_u64(tape.int_in(0..=u64::MAX, "u64"))),
        },
        3 => Value::Float(tape.f64_finite("float")),
        4 => Value::Text(String::from_str_in(&tape.text(0..=8, "text"), Global)),
        5 => {
            let items = tape.vec(0..=4, "list", |t| draw_value(t, depth - 1));
            let mut list = blaze_own::Vec::new_in(Global);
            for item in items {
                list.push(item);
            }
            Value::List(list)
        }
        _ => {
            let mut map = Map::new_in(Global);
            for (key, value) in tape.vec(0..=4, "map", |t| (t.text_from("ab\"\\\u{1}é", 0..=3, "key"), draw_value(t, depth - 1))) {
                map.insert(String::from_str_in(&key, Global), value);
            }
            Value::Map(map)
        }
    }
}

/// read(write(v)) == v in both styles, and the events copied through the writer give the
/// compact text back.
fn round_trip(tape: &Tape) -> Result<(), Broke> {
    let v = draw_value(tape, 3);
    for options in [WriteOptions::COMPACT, WriteOptions::PRETTY] {
        let text = to_string_in(&v, options, Global).map_err(|e| Broke::Harness(e.to_string()))?;
        match read_in(text.as_bytes(), Limits::DEFAULT, Global) {
            Ok(back) if back == v => {}
            Ok(back) => return Err(Broke::Oracle(format!("the round trip of {v:?} gave {back:?}"))),
            Err(error) => return Err(Broke::Oracle(format!("the round trip of {v:?} wrote {:?}: {error}", text.as_str()))),
        }
    }
    let text = to_string_in(&v, WriteOptions::COMPACT, Global).map_err(|e| Broke::Harness(e.to_string()))?;
    let mut out = String::new_in(Global);
    let mut writer = Writer::new_in(&mut out, Global);
    let mut reader =
        Reader::new_in(text.as_bytes(), Limits::DEFAULT, Global).map_err(|e| Broke::Harness(e.to_string()))?;
    while let Some(item) = reader.next().map_err(|e| Broke::Oracle(format!("the round trip's reader: {e}")))? {
        writer.event(&item.event).map_err(|e| Broke::Oracle(format!("the round trip's writer: {e:?}")))?;
    }
    writer.finish().map_err(|e| Broke::Oracle(format!("the round trip's writer: {e:?}")))?;
    if out.as_str() != text.as_str() {
        return Err(Broke::Oracle(format!("the round trip of events gave {:?}, not {:?}", out.as_str(), text.as_str())));
    }
    Ok(())
}

#[test]
fn the_round_trip_holds() {
    blaze_sim::check(Config::cases(400), round_trip);
}

#[test]
fn the_oracle_catches_an_unescaped_quote() {
    let run = |tape: &Tape, p: Plant| plant::with(p, || round_trip(tape));
    blaze_sim::control(Config::cases(400), Plant::Clean, Plant::QuoteUnescaped, "round trip", run);
}
