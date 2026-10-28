use std::collections::BTreeMap;

use rustc_hash::FxHashMap;

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    Message,
    cbor::{Error, check, from_slice, to_vec},
    read_frame, receive, send, write_frame,
};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|it| format!("{it:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
        .collect()
}

/// `value` encodes as `expected`, which the checker passes and which decodes to it again.
#[track_caller]
fn encodes<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(
    value: T,
    expected: &str,
) {
    let bytes = to_vec(&value).unwrap();
    assert_eq!(hex(&bytes), expected, "{value:?}");
    check(&bytes).unwrap();
    assert_eq!(from_slice::<T>(&bytes).unwrap(), value);
}

#[track_caller]
fn refused(bytes: &str, what: &str) {
    match check(&unhex(bytes)) {
        Err(Error::NotDeterministic { what: said, .. }) => assert_eq!(said, what, "{bytes}"),
        other => panic!("{bytes}: {other:?}"),
    }
}

/// RFC 8949 Appendix A, the examples that are in the deterministic encoding.
#[test]
fn the_rfc_examples() {
    for (n, expected) in [
        (0u64, "00"),
        (1, "01"),
        (10, "0a"),
        (23, "17"),
        (24, "1818"),
        (25, "1819"),
        (100, "1864"),
        (1000, "1903e8"),
        (1_000_000, "1a000f4240"),
        (1_000_000_000_000, "1b000000e8d4a51000"),
        (u64::MAX, "1bffffffffffffffff"),
    ] {
        encodes(n, expected);
    }
    encodes(-18_446_744_073_709_551_616i128, "3bffffffffffffffff");
    for (n, expected) in [(-1i64, "20"), (-10, "29"), (-100, "3863"), (-1000, "3903e7")] {
        encodes(n, expected);
    }
    for (x, expected) in [
        (0.0f64, "f90000"),
        (-0.0, "f98000"),
        (1.0, "f93c00"),
        (1.1, "fb3ff199999999999a"),
        (1.5, "f93e00"),
        (65504.0, "f97bff"),
        (100_000.0, "fa47c35000"),
        (3.402_823_466_385_288_6e38, "fa7f7fffff"),
        (1.0e300, "fb7e37e43c8800759c"),
        (5.960_464_477_539_063e-8, "f90001"),
        (0.000_061_035_156_25, "f90400"),
        (-4.0, "f9c400"),
        (-4.1, "fbc010666666666666"),
        (f64::INFINITY, "f97c00"),
        (f64::NEG_INFINITY, "f9fc00"),
    ] {
        let bytes = to_vec(&x).unwrap();
        assert_eq!(hex(&bytes), expected, "{x}");
        check(&bytes).unwrap();
        assert_eq!(from_slice::<f64>(&bytes).unwrap().to_bits(), x.to_bits());
    }
    assert_eq!(hex(&to_vec(&f64::NAN).unwrap()), "f97e00");
    assert!(from_slice::<f64>(&unhex("f97e00")).unwrap().is_nan());
    encodes(false, "f4");
    encodes(true, "f5");
    encodes((), "f6");
    encodes(String::new(), "60");
    encodes("a".to_owned(), "6161");
    encodes("IETF".to_owned(), "6449455446");
    encodes("\"\\".to_owned(), "62225c");
    encodes("\u{fc}".to_owned(), "62c3bc");
    encodes("\u{6c34}".to_owned(), "63e6b0b4");
    encodes("\u{10151}".to_owned(), "64f0908591");
    encodes(Vec::<u8>::new(), "80");
    encodes(vec![1, 2, 3], "83010203");
    encodes((1, (2, 3), (4, 5)), "8301820203820405");
    encodes(
        (1..=25).collect::<Vec<u32>>(),
        "98190102030405060708090a0b0c0d0e0f101112131415161718181819",
    );
    encodes(BTreeMap::<u8, u8>::new(), "a0");
    encodes(BTreeMap::from([(1, 2), (3, 4)]), "a201020304");
    let bytes = to_vec(&serde_bytes(&[1, 2, 3, 4])).unwrap();
    assert_eq!(hex(&bytes), "4401020304");
}

struct Bytes<'a>(&'a [u8]);

impl Serialize for Bytes<'_> {
    fn serialize<S: serde::Serializer>(&self, to: S) -> Result<S::Ok, S::Error> {
        to.serialize_bytes(self.0)
    }
}

fn serde_bytes(bytes: &[u8]) -> Bytes<'_> {
    Bytes(bytes)
}

/// Keys are sorted by their encoding, not as Rust sorts them: a shorter text first.
#[test]
fn map_keys_go_in_the_order_of_their_bytes() {
    let map = BTreeMap::from([("aa".to_owned(), 1), ("b".to_owned(), 2)]);
    encodes(map, "a261620262616101");
    // A hash map's order is its iteration's, which the encoding does not keep: unsigned
    // before negative, each by its head.
    let hashed: FxHashMap<i64, ()> = (-30..30).map(|it| (it, ())).collect();
    let bytes = to_vec(&hashed).unwrap();
    check(&bytes).unwrap();
    assert_eq!(&hex(&bytes)[..12], "b83c00f601f6");
    assert_eq!(from_slice::<FxHashMap<i64, ()>>(&bytes).unwrap(), hashed);
    // A struct's fields come out sorted too, whatever their order in the type.
    #[derive(Serialize)]
    struct Fields {
        schema: u8,
        id: u8,
        body: u8,
    }
    let fields = to_vec(&Fields { schema: 1, id: 2, body: 3 }).unwrap();
    assert_eq!(hex(&fields), "a36269640264626f64790366736368656d6101");
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
enum Said {
    Started,
    Crate { name: String, done: u32, total: u32 },
    Line(String),
    Pair(u8, i8),
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
struct Record {
    at_ns: u64,
    said: Vec<Said>,
    why: Option<String>,
    path: std::path::PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    newer: Option<u8>,
}

fn record() -> Message<Record> {
    Message::new(
        7,
        Record {
            at_ns: 1_250_000_000,
            said: vec![
                Said::Started,
                Said::Crate { name: "anim".into(), done: 3, total: 12 },
                Said::Line("   Compiling".into()),
                Said::Pair(255, -128),
            ],
            why: None,
            path: "src/main.rs".into(),
            newer: None,
        },
    )
}

/// The encoding of a message is pinned: a change to it is a change of the protocol, and
/// bumps [`crate::SCHEMA`].
#[test]
fn a_message_is_these_bytes() {
    let bytes = to_vec(&record()).unwrap();
    // Keys sort by their encoding: shorter text first, then bytewise.
    let expected = [
        "a3",                                        // the message, a map of three
        "626964 07",                                 // "id": 7
        "64626f6479 a4",                             // "body": a map of four (no `newer`)
        "63776879 f6",                               //   "why": null
        "6470617468 6b 7372632f6d61696e2e7273",      //   "path": "src/main.rs"
        "6473616964 84",                             //   "said": an array of four
        "67 53746172746564",                         //     "Started"
        "a1 654372617465 a3",                        //     {"Crate": a map of three
        "64646f6e65 03",                             //       "done": 3
        "646e616d65 64616e696d",                     //       "name": "anim"
        "65746f74616c 0c",                           //       "total": 12}
        "a1 644c696e65 6c 202020436f6d70696c696e67", //     {"Line": "   Compiling"}
        "a1 6450616972 82 18ff 387f",                //     {"Pair": [255, -128]}
        "6561745f6e73 1a4a817c80",                   //   "at_ns": 1250000000
        "66736368656d61 02",                         // "schema": 2
    ];
    assert_eq!(hex(&bytes), expected.concat().replace(' ', ""));
    assert_eq!(from_slice::<Message<Record>>(&bytes).unwrap(), record());
    // An older reader skips a field it does not know; a newer one fills one it is not given.
    let with_newer = Message { body: Record { newer: Some(1), ..record().body }, ..record() };
    let newer = to_vec(&with_newer).unwrap();
    assert_eq!(from_slice::<Message<Record>>(&newer).unwrap().body.newer, Some(1));
}

/// A build's record is pinned the same way.
#[test]
fn an_event_is_these_bytes() {
    use crate::events::{Cause, CrateState, Event, Fns, NodeKind, Record, Slow, Step};
    let records = [
        Record { at_ns: 0, event: Event::Started { step: Step::LoadWorkspace, of: None } },
        Record {
            at_ns: 38_300_000_000,
            event: Event::Crate {
                name: "anim".into(),
                state: CrateState::Running,
                fns: Fns { total: 5353, done: 12, kept: 2 },
            },
        },
        Record {
            at_ns: 210_000_000,
            event: Event::Rebuilt {
                name: "render".into(),
                krate: "anim".into(),
                cause: Cause::Body,
            },
        },
        Record {
            at_ns: 2_000_000_000,
            event: Event::Progress {
                step: Step::Check,
                done: 3,
                total: 14,
                longest: Some("src/main.rs".into()),
            },
        },
        Record {
            at_ns: 0,
            event: Event::Node { id: 1, kind: NodeKind::Std, name: "std".into(), deps: vec![0] },
        },
        Record { at_ns: 0, event: Event::Cores { busy: 790, of: 8 } },
        Record {
            at_ns: 0,
            event: Event::Compiling {
                functions: vec![Slow {
                    krate: "rg".into(),
                    name: "rg::main".into(),
                    ns: 300_000_000,
                }],
            },
        },
    ];
    let hexes: Vec<String> = records.iter().map(|it| hex(&to_vec(it).unwrap())).collect();
    let expected = [
        // {"at_ns": 0, "event": {"Started": {"of": null, "step": "LoadWorkspace"}}}
        "a2 6561745f6e73 00 656576656e74 a1 6753746172746564 a2 626f66 f6 6473746570 \
         6d 4c6f6164576f726b7370616365",
        // {"at_ns": 38300000000, "event": {"Crate": {"fns": {"done": 12, "kept": 2,
        // "total": 5353}, "name": "anim", "state": "Running"}}}
        "a2 6561745f6e73 1b00000008eadb9f00 656576656e74 a1 654372617465 a3 63666e73 a3 \
         64646f6e65 0c 646b657074 02 65746f74616c 1914e9 646e616d65 64616e696d \
         657374617465 6752756e6e696e67",
        // {"at_ns": 210000000, "event": {"Rebuilt": {"name": "render", "cause": "Body",
        // "krate": "anim"}}}
        "a2 6561745f6e73 1a0c845880 656576656e74 a1 6752656275696c74 a3 646e616d65 \
         6672656e646572 656361757365 64426f6479 656b72617465 64616e696d",
        // {"at_ns": 2000000000, "event": {"Progress": {"done": 3, "step": "Check",
        // "total": 14, "longest": "src/main.rs"}}}
        "a2 6561745f6e73 1a77359400 656576656e74 a1 6850726f6772657373 a4 64646f6e65 03 \
         6473746570 65436865636b 65746f74616c 0e 676c6f6e67657374 6b 7372632f6d61696e2e7273",
        // {"at_ns": 0, "event": {"Node": {"id": 1, "deps": [0], "kind": "Std", "name": "std"}}}
        "a2 6561745f6e73 00 656576656e74 a1 644e6f6465 a4 626964 01 6464657073 81 00 \
         646b696e64 63537464 646e616d65 63737464",
        // {"at_ns": 0, "event": {"Cores": {"of": 8, "busy": 790}}}
        "a2 6561745f6e73 00 656576656e74 a1 65436f726573 a2 626f66 08 6462757379 190316",
        // {"at_ns": 0, "event": {"Compiling": {"functions": [{"ns": 300000000,
        // "name": "rg::main", "krate": "rg"}]}}}
        "a2 6561745f6e73 00 656576656e74 a1 69436f6d70696c696e67 a1 6966756e6374696f6e73 81 \
         a3 626e73 1a11e1a300 646e616d65 6872673a3a6d61696e 656b72617465 627267",
    ];
    for ((hexed, expected), record) in hexes.iter().zip(expected).zip(&records) {
        assert_eq!(hexed, &expected.replace(' ', ""), "{record:?}");
        assert_eq!(&from_slice::<Record>(&to_vec(record).unwrap()).unwrap(), record);
    }
}

#[test]
fn what_is_not_deterministic_is_refused() {
    refused("1817", "an argument in more bytes than it needs");
    refused("190017", "an argument in more bytes than it needs");
    refused("1a0000ffff", "an argument in more bytes than it needs");
    refused("1b00000000ffffffff", "an argument in more bytes than it needs");
    refused("5f42010243030405ff", "an indefinite length");
    refused("9fff", "an indefinite length");
    refused("1c", "a reserved head");
    refused("a2616201616102", "map keys out of order");
    refused("a2616101616102", "a key twice");
    refused("fa3f800000", "a single that a half holds");
    refused("fb3ff0000000000000", "a double that a single holds");
    refused("fb7ff8000000000000", "a double that a single holds");
    refused("f97e01", "a NaN other than f9 7e00");
    refused("c11a514b67b0", "a tag");
    refused("f7", "a simple value other than false, true and null");
    refused("f820", "a simple value other than false, true and null");
    refused("62c328", "text not UTF-8");
    refused("0000", "bytes after the item");
    refused("", "the item is cut short");
    refused("83", "the item is cut short");
    refused("44010203", "a length past the end");
    refused("5bffffffffffffffff00", "a length past the end");
    let mut deep = vec![0x81; 300];
    deep.push(0);
    assert!(matches!(
        check(&deep),
        Err(Error::NotDeterministic { what: "items nested deeper than 256", .. })
    ));
    // The decoder refuses them too, before it reads anything.
    assert!(from_slice::<u8>(&unhex("1817")).is_err());
    assert!(to_vec(&u128::MAX).is_err());
}

/// A small generator: the same values in every run.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn text(&mut self) -> String {
        let len = self.next() % 12;
        (0..len)
            .map(|_| {
                ['a', 'Z', '0', ' ', '\u{e9}', '\u{6c34}', '\u{1f525}'][self.next() as usize % 7]
            })
            .collect()
    }

    fn said(&mut self) -> Said {
        match self.next() % 4 {
            0 => Said::Started,
            1 => Said::Crate {
                name: self.text(),
                done: self.next() as u32,
                total: self.next() as u32,
            },
            2 => Said::Line(self.text()),
            _ => Said::Pair(self.next() as u8, self.next() as i8),
        }
    }
}

/// Every value encodes in bytes the checker passes, which decode to it and encode to the
/// same bytes again.
#[test]
fn random_values_round_trip_byte_for_byte() {
    let mut random = Random(0x2545_f491_4f6c_dd1d);
    for _ in 0..2000 {
        let message = Message::new(
            random.next(),
            Record {
                at_ns: random.next() >> (random.next() % 64),
                said: (0..random.next() % 6).map(|_| random.said()).collect(),
                why: (random.next() % 2 == 0).then(|| random.text()),
                path: random.text().into(),
                newer: (random.next() % 3 == 0).then(|| random.next() as u8),
            },
        );
        let bytes = to_vec(&message).unwrap();
        check(&bytes).unwrap();
        let back: Message<Record> = from_slice(&bytes).unwrap();
        assert_eq!(back, message);
        assert_eq!(to_vec(&back).unwrap(), bytes);
        let x = f64::from_bits(random.next());
        let float = to_vec(&x).unwrap();
        check(&float).unwrap();
        let y: f64 = from_slice(&float).unwrap();
        assert!(y.to_bits() == x.to_bits() || x.is_nan() && y.is_nan(), "{x} {y}");
        let n = random.next() as i64 >> (random.next() % 64);
        assert_eq!(from_slice::<i64>(&to_vec(&n).unwrap()).unwrap(), n);
        let single = f32::from_bits(random.next() as u32);
        let back: f32 = from_slice(&to_vec(&single).unwrap()).unwrap();
        assert!(back.to_bits() == single.to_bits() || single.is_nan() && back.is_nan());
    }
}

/// Bytes from a peer never panic the reader nor have it allocate what a length claims.
#[test]
fn no_bytes_break_the_reader() {
    use blaze_fuzz as fuzz;
    let seeds = vec![
        to_vec(&record()).unwrap(),
        unhex("a201020304"),
        unhex("9b00ffffffffffffff"),
        Vec::new(),
    ];
    let (mut whole, mut cut) = (0, 0);
    for bytes in fuzz::inputs(concat!(env!("CARGO_MANIFEST_DIR"), "/corpus/message"), seeds) {
        let (largest, read) = fuzz::largest_allocation(|| from_slice::<Message<Record>>(&bytes));
        assert!(largest <= fuzz::allowed(bytes.len()), "{largest} bytes for {}", bytes.len());
        let checked = check(&bytes);
        match read {
            Ok(message) => {
                whole += 1;
                assert!(checked.is_ok());
                // Unknown keys and an explicit null for `newer` are dropped; the value is
                // what its own encoding reads back as.
                let again = to_vec(&message).unwrap();
                assert_eq!(from_slice::<Message<Record>>(&again).unwrap(), message);
            }
            Err(_) => cut += 1,
        }
    }
    assert!(whole > 0 && cut > 0, "{whole} read, {cut} refused");
}

#[test]
fn frames_carry_one_item_each() {
    let mut stream = Vec::new();
    send(&mut stream, &record()).unwrap();
    send(&mut stream, &Message::new(8, Said::Started)).unwrap();
    let item = to_vec(&record()).unwrap();
    assert_eq!(stream[..4], (item.len() as u32).to_be_bytes());
    let mut from = stream.as_slice();
    assert_eq!(receive::<Message<Record>>(&mut from).unwrap(), Some(record()));
    assert_eq!(receive::<Message<Said>>(&mut from).unwrap(), Some(Message::new(8, Said::Started)));
    assert_eq!(receive::<Message<Said>>(&mut from).unwrap(), None);
    // One cut short is an error, not the end.
    let mut cut = &stream[..stream.len() - 1];
    receive::<Message<Record>>(&mut cut).unwrap();
    assert!(receive::<Message<Said>>(&mut cut).is_err());
    assert!(read_frame(&mut &[0, 0][..]).is_err());
    // A length past the limit is refused before anything is read.
    let mut long = &(crate::LONGEST_FRAME + 1).to_be_bytes()[..];
    assert_eq!(read_frame(&mut long).unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    // So is an item not in the deterministic encoding.
    let mut sent = Vec::new();
    write_frame(&mut sent, &unhex("1817")).unwrap();
    let refused = receive::<u8>(&mut sent.as_slice()).unwrap_err();
    assert!(refused.to_string().contains("more bytes than it needs"), "{refused}");
}

/// A record of the functions a build put in its program takes their names and some 100 bytes
/// more for each (the names of the fields among them): a cold build of anim says some 80 000.
#[test]
fn a_record_of_functions_is_small() {
    use crate::events::{Event, Function, How, Record};
    let fns: Vec<Function> = (0..1000u32)
        .map(|id| Function {
            id,
            key: u64::from(id).wrapping_mul(0x9e37_79b9_7f4a_7c15),
            krate: "anim".into(),
            name: format!("anim::scene::render_{id}"),
            calls: vec![id + 1, id + 2, id / 2],
            how: if id % 3 == 0 { How::Unchanged } else { How::Compiled { ns: 125_000 } },
        })
        .collect();
    let names: usize = fns.iter().map(|it| it.name.len()).sum();
    let record =
        Record { at_ns: 1, event: Event::Functions { object: 2, whole: true, functions: fns } };
    let bytes = to_vec(&record).unwrap();
    check(&bytes).unwrap();
    assert_eq!(from_slice::<Record>(&bytes).unwrap(), record);
    assert!(bytes.len() < names + 1000 * 128, "{} bytes for {names} of names", bytes.len());
}
