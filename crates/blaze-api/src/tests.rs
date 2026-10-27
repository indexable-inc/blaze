use serde::{Deserialize, Serialize};

use crate::{Bytes, Error, Value, frame, from_slice, json, to_vec};

fn hex(text: &str) -> Vec<u8> {
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
}

fn text(it: &str) -> Value {
    Value::Text(it.to_owned())
}

#[test]
fn values_encode_as_rfc_8949_appendix_a_has_them() {
    let cases = [
        (Value::Uint(0), "00"),
        (Value::Uint(23), "17"),
        (Value::Uint(24), "1818"),
        (Value::Uint(100), "1864"),
        (Value::Uint(1000), "1903e8"),
        (Value::Uint(1_000_000), "1a000f4240"),
        (Value::Uint(1_000_000_000_000), "1b000000e8d4a51000"),
        (Value::Uint(u64::MAX), "1bffffffffffffffff"),
        (Value::Nint(0), "20"),
        (Value::Nint(9), "29"),
        (Value::Nint(99), "3863"),
        (Value::Nint(999), "3903e7"),
        (text(""), "60"),
        (text("a"), "6161"),
        (text("IETF"), "6449455446"),
        (text("\u{00fc}"), "62c3bc"),
        (Value::Bytes(vec![]), "40"),
        (Value::Bytes(vec![1, 2, 3, 4]), "4401020304"),
        (Value::Array(vec![]), "80"),
        (Value::Array(vec![Value::Uint(1), Value::Uint(2), Value::Uint(3)]), "83010203"),
        (Value::Map(vec![]), "a0"),
        (Value::Bool(false), "f4"),
        (Value::Bool(true), "f5"),
        (Value::Null, "f6"),
    ];
    for (value, encoded) in cases {
        assert_eq!(value.encode(), hex(encoded), "{value:?}");
        assert_eq!(Value::decode(&hex(encoded)), Ok(value));
    }
    let map = Value::map(vec![(Value::Uint(3), Value::Uint(4)), (Value::Uint(1), Value::Uint(2))])
        .unwrap();
    assert_eq!(map.encode(), hex("a201020304"));
    let nested = Value::map(vec![
        (text("b"), Value::Array(vec![Value::Uint(2), Value::Uint(3)])),
        (text("a"), Value::Uint(1)),
    ])
    .unwrap();
    assert_eq!(nested.encode(), hex("a26161016162820203"));
}

#[test]
fn map_keys_sort_bytewise_by_their_encodings() {
    // Deterministic CBOR (RFC 8949 4.2.1) sorts by the bytes alone: "b" (61 62) before "aa"
    // (62 61 61), where the length-first order of RFC 7049's canonical CBOR had them reversed.
    let map = Value::map(vec![(text("aa"), Value::Null), (text("b"), Value::Null)]).unwrap();
    assert_eq!(map.encode(), hex("a26162f6626161f6"));
    let Value::Map(entries) = &map else { unreachable!() };
    assert_eq!(entries[0].0, text("b"));
    assert_eq!(
        Value::map(vec![(Value::Uint(1), Value::Null), (Value::Uint(1), Value::Null)]),
        Err(Value::Uint(1))
    );
}

#[test]
fn what_is_not_deterministic_is_refused_with_why() {
    let refused = [
        ("1817", "a number not in its shortest form"),
        ("190017", "a number not in its shortest form"),
        ("1a0000ffff", "a number not in its shortest form"),
        ("1b00000000ffffffff", "a number not in its shortest form"),
        ("5801ff", "a number not in its shortest form"),
        ("9fff", "an indefinite length"),
        ("5fff", "an indefinite length"),
        ("a203040102", "map keys out of order"),
        ("a201020103", "a map key that is there twice"),
        ("f93c00", "a float"),
        ("fb3ff199999999999a", "a float"),
        ("c06161", "a tag"),
        ("f7", "undefined"),
        ("f0", "a simple value"),
        ("1c", "a reserved head"),
        ("0000", "bytes after the value"),
        ("61ff", "text that is not UTF-8"),
        ("1903", "the value ends early"),
        ("5bffffffffffffffff", "a length longer than the message"),
        ("83", "a length longer than the message"),
    ];
    for (bytes, why) in refused {
        let offence = Value::decode(&hex(bytes)).expect_err(bytes);
        assert_eq!(offence.what, why, "{bytes}");
    }
    let deep = [vec![0x81; 100], vec![0x00]].concat();
    assert_eq!(Value::decode(&deep).unwrap_err().what, "arrays or maps nested too deep");
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
enum Op {
    Write { path: String, bytes: Bytes },
    Delete(String),
    Rename(String, String),
    Nothing,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
struct Transaction {
    base: Option<Bytes>,
    ops: Vec<Op>,
    sequence: u64,
    offset: i64,
    wide: i128,
    flags: (bool, char),
    #[serde(default)]
    missing: Vec<u8>,
}

fn transaction() -> Transaction {
    Transaction {
        base: Some(Bytes(vec![0xab; 32])),
        ops: vec![
            Op::Write { path: "src/main.rs".into(), bytes: Bytes(b"fn main() {}\n".to_vec()) },
            Op::Delete("src/old.rs".into()),
            Op::Rename("a.rs".into(), "b.rs".into()),
            Op::Nothing,
        ],
        sequence: 1 << 40,
        offset: -5,
        wide: -(1 << 64),
        flags: (true, 'z'),
        missing: vec![1, 2],
    }
}

#[test]
fn messages_round_trip_byte_for_byte() {
    let message = transaction();
    let bytes = to_vec(&message).unwrap();
    let back: Transaction = from_slice(&bytes).unwrap();
    assert_eq!(back, message);
    assert_eq!(to_vec(&back).unwrap(), bytes);
    // The bytes are a byte string, not an array of numbers.
    let Value::Map(entries) = Value::decode(&bytes).unwrap() else { panic!() };
    let base = entries.iter().find(|it| it.0 == text("base")).unwrap();
    assert_eq!(base.1, Value::Bytes(vec![0xab; 32]));
    // A field left out is its default.
    let mut without = Value::decode(&bytes).unwrap();
    if let Value::Map(entries) = &mut without {
        entries.retain(|it| it.0 != text("missing"));
    }
    let back: Transaction = from_slice(&without.encode()).unwrap();
    assert_eq!(back.missing, Vec::<u8>::new());
}

#[test]
fn floats_and_wrong_shapes_are_refused() {
    assert!(matches!(to_vec(&1.5f64), Err(Error::Schema(_))));
    assert!(matches!(from_slice::<Transaction>(&hex("a0")), Err(Error::Schema(_))));
    assert!(matches!(from_slice::<u8>(&hex("190100")), Err(Error::Schema(_))));
    assert!(matches!(from_slice::<Op>(&hex("6161")), Err(Error::Schema(_))));
    assert!(matches!(from_slice::<u64>(&hex("1817")), Err(Error::Encoding(_))));
}

#[test]
fn random_values_round_trip() {
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }
    fn number(rng: &mut Rng) -> u64 {
        match rng.below(5) {
            0 => rng.below(24),
            1 => rng.below(0x100),
            2 => rng.below(0x1_0000),
            3 => rng.below(0x1_0000_0000),
            _ => rng.next(),
        }
    }
    fn value(rng: &mut Rng, depth: u32) -> Value {
        match rng.below(if depth > 3 { 6 } else { 8 }) {
            0 => Value::Uint(number(rng)),
            1 => Value::Nint(number(rng)),
            2 => Value::Bytes((0..rng.below(40)).map(|_| rng.next() as u8).collect()),
            3 => Value::Text(
                (0..rng.below(12)).map(|_| char::from(b'a' + rng.below(26) as u8)).collect(),
            ),
            4 => Value::Bool(rng.below(2) == 0),
            5 => Value::Null,
            6 => Value::Array((0..rng.below(5)).map(|_| value(rng, depth + 1)).collect()),
            _ => {
                let entries =
                    (0..rng.below(5)).map(|_| (value(rng, depth + 1), value(rng, depth + 1)));
                let mut entries: Vec<_> = entries.collect();
                entries.sort_by_key(|it| it.0.encode());
                entries.dedup_by(|a, b| a.0 == b.0);
                Value::map(entries).unwrap()
            }
        }
    }
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    for _ in 0..20_000 {
        let it = value(&mut rng, 0);
        let bytes = it.encode();
        assert_eq!(Value::decode(&bytes).as_ref(), Ok(&it));
        // Any prefix is refused.
        if bytes.len() > 1 {
            let cut = rng.below(bytes.len() as u64) as usize;
            assert!(Value::decode(&bytes[..cut]).is_err());
        }
    }
}

#[test]
fn frames_carry_messages_and_refuse_long_ones() {
    let mut stream = Vec::new();
    frame::write(&mut stream, b"one").unwrap();
    frame::write(&mut stream, b"").unwrap();
    assert_eq!(stream[0], 0);
    let mut reading = &stream[..];
    assert_eq!(frame::read(&mut reading).unwrap(), Some(b"one".to_vec()));
    assert_eq!(frame::read(&mut reading).unwrap(), Some(vec![]));
    assert_eq!(frame::read(&mut reading).unwrap(), None);
    let mut long = &[0x01, 0, 0, 1][..];
    assert!(frame::read(&mut long).is_err());
    let mut cut = &[0, 0][..];
    assert!(frame::read(&mut cut).is_err());
}

#[test]
fn json_renders_for_people() {
    let value = crate::to_value(&transaction()).unwrap();
    let rendered = json::render(&value);
    assert!(rendered.starts_with("{\"ops\":"), "{rendered}");
    assert!(rendered.contains("\"base\":\"abab"), "{rendered}");
    assert!(rendered.contains("\"wide\":-18446744073709551616"), "{rendered}");
    assert!(rendered.contains("{\"Delete\":\"src/old.rs\"}"), "{rendered}");
    assert_eq!(json::render(&text("a\"\n\u{1}")), "\"a\\\"\\n\\u0001\"");
}
