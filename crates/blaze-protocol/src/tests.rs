use std::collections::BTreeMap;

use blaze_hash::HashMap;
use blaze_ids::rng::SplitMix64;

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
    let hashed: HashMap<i64, ()> = (-30..30).map(|it| (it, ())).collect();
    let bytes = to_vec(&hashed).unwrap();
    check(&bytes).unwrap();
    assert_eq!(&hex(&bytes)[..12], "b83c00f601f6");
    assert_eq!(from_slice::<HashMap<i64, ()>>(&bytes).unwrap(), hashed);
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

/// Values of the protocol from a seed, the same in every run.
struct Random(SplitMix64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0.next_u64()
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
    let mut random = Random(SplitMix64::new(0x2545_f491_4f6c_dd1d));
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

/// `send` and `receive` keep their frame and encoder on the thread: warm, they allocate
/// nothing for a value that owns nothing, and give the bytes a fresh buffer would. A frame past
/// what is kept is sent and read whole.
#[test]
fn a_warm_send_and_receive_allocate_nothing() {
    let message = Message::new(9, 1_250_000_000u64);
    let mut stream = Vec::with_capacity(4096);
    send(&mut stream, &message).unwrap();
    let mut fresh = Vec::new();
    crate::append_frame(&mut crate::cbor::Writer::new(), &mut fresh, &message).unwrap();
    assert_eq!(stream, fresh);
    blaze_alloc_count::assert_at_most(0, "a warm send", || {
        for _ in 0..15 {
            send(&mut stream, &message).unwrap();
        }
    });
    assert_eq!(stream, fresh.repeat(16));
    let mut from = stream.as_slice();
    assert_eq!(receive::<Message<u64>>(&mut from).unwrap(), Some(message.clone()));
    let read = blaze_alloc_count::assert_at_most(0, "a warm receive", || {
        let mut read = 0;
        while let Some(back) = receive::<Message<u64>>(&mut from).unwrap() {
            assert_eq!(back, message);
            read += 1;
        }
        read
    });
    assert_eq!(read, 15);
    let big = vec![7u8; 2 << 20];
    let mut sent = Vec::new();
    send(&mut sent, &serde_bytes(&big)).unwrap();
    send(&mut sent, &message).unwrap();
    let mut from = sent.as_slice();
    assert_eq!(receive::<crate::bytes::Buf>(&mut from).unwrap().map(|it| it.0), Some(big));
    assert_eq!(receive::<Message<u64>>(&mut from).unwrap(), Some(message));
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

/// A writer that takes one byte per write, and counts its writes.
struct Trickle {
    bytes: Vec<u8>,
    writes: usize,
}

impl std::io::Write for Trickle {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.writes += 1;
        match bytes.first() {
            Some(&byte) => {
                self.bytes.push(byte);
                Ok(1)
            }
            None => Ok(0),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A frame is its length and its item however few bytes each write takes; into a `Vec` it is
/// one write; one encoded in place is the frame `send` writes; a kept buffer reads them all.
#[test]
fn frames_are_written_in_place_and_read_into_one_buffer() {
    let item = to_vec(&record()).unwrap();
    let mut framed = (item.len() as u32).to_be_bytes().to_vec();
    framed.extend_from_slice(&item);
    let mut trickle = Trickle { bytes: Vec::new(), writes: 0 };
    write_frame(&mut trickle, &item).unwrap();
    assert_eq!(trickle.bytes, framed);
    assert_eq!(trickle.writes, framed.len());
    let mut whole = Vec::new();
    write_frame(&mut whole, &item).unwrap();
    assert_eq!(whole, framed);
    let mut sent = Vec::new();
    send(&mut sent, &record()).unwrap();
    assert_eq!(sent, framed);
    let mut writer = crate::cbor::Writer::new();
    let mut appended = vec![9];
    crate::append_frame(&mut writer, &mut appended, &record()).unwrap();
    assert_eq!(appended[1..], framed);
    // An item with no encoding leaves the buffer as it was.
    assert!(crate::append_frame(&mut writer, &mut appended, &u128::MAX).is_err());
    assert_eq!(appended[1..], framed);
    let mut stream = framed.clone();
    stream.extend_from_slice(&framed);
    let mut from = stream.as_slice();
    let mut buffer = Vec::new();
    for _ in 0..2 {
        assert!(crate::read_frame_into(&mut from, &mut buffer).unwrap());
        assert_eq!(buffer, item);
    }
    assert!(!crate::read_frame_into(&mut from, &mut buffer).unwrap());
    assert!(buffer.is_empty());
}

#[test]
fn a_cancelled_node_is_settled_and_encodes_as_its_name() {
    use crate::events::NodeState;
    encodes(NodeState::Cancelled, "6943616e63656c6c6564");
    assert!(NodeState::Cancelled.settled());
    assert!(!NodeState::Ready.settled());
}

/// Columns of an [`crate::events::Event::Nodes`] built from nodes given as an `Event::Node`
/// says each, the ids from `first` on.
fn nodes_record(
    first: u32,
    nodes: &[(crate::events::NodeKind, &str, &[u32])],
) -> crate::events::Event {
    let mut kinds = Vec::new();
    let (mut names, mut name_at) = (String::new(), vec![0]);
    let (mut deps, mut deps_at) = (Vec::new(), vec![0]);
    for &(kind, name, on) in nodes {
        kinds.push(kind);
        names.push_str(name);
        name_at.push(names.len() as u32);
        deps.extend_from_slice(on);
        deps_at.push(deps.len() as u32);
    }
    crate::events::Event::Nodes { first, kinds, names, name_at, deps, deps_at }
}

/// A record of nodes is pinned like the others, and goes through the codec unchanged.
#[test]
fn a_record_of_nodes_is_these_bytes() {
    use crate::events::{NodeKind, Record};
    let event = nodes_record(1, &[(NodeKind::Std, "std", &[0]), (NodeKind::Names, "ab", &[1])]);
    let record = Record { at_ns: 0, event };
    let expected = [
        "a2 6561745f6e73 00 656576656e74 a1 654e6f646573 a6", // {"at_ns": 0, "event": {"Nodes":
        "64 64657073 82 00 01",                                //   "deps": [0, 1]
        "65 6669727374 01",                                    //   "first": 1
        "65 6b696e6473 82 63537464 654e616d6573",              //   "kinds": ["Std", "Names"]
        "65 6e616d6573 65 7374646162",                         //   "names": "stdab"
        "67 646570735f6174 83 00 01 02",                       //   "deps_at": [0, 1, 2]
        "67 6e616d655f6174 83 00 03 05",                       //   "name_at": [0, 3, 5]}}
    ];
    let bytes = to_vec(&record).unwrap();
    assert_eq!(hex(&bytes), expected.concat().replace(' ', ""));
    check(&bytes).unwrap();
    assert_eq!(from_slice::<Record>(&bytes).unwrap(), record);
}

/// A record of nodes reads as the `Event::Node` of each, ids from `first` on, in the order
/// given; its order puts each after those of it that it waits for, the first given first
/// among those that can go, at no cost when each already comes after them.
#[test]
fn a_record_of_nodes_reads_as_its_nodes() {
    use crate::events::{Event, NodeKind, Order};
    let mut order = Order::default();
    let event = nodes_record(
        10,
        &[
            (NodeKind::Codegen, "a\u{e9}", &[3, 12]),
            (NodeKind::Link, "", &[]),
            (NodeKind::Test, "c", &[11, 3, 11]),
        ],
    );
    let batch = event.nodes(&mut order).unwrap().unwrap();
    let each: Vec<Event> = batch.iter().map(|it| it.event()).collect();
    let node = |id, kind, name: &str, deps: &[u32]| Event::Node {
        id,
        kind,
        name: name.to_owned(),
        deps: deps.to_vec(),
    };
    assert_eq!(
        each,
        [
            node(10, NodeKind::Codegen, "a\u{e9}", &[3, 12]),
            node(11, NodeKind::Link, "", &[]),
            node(12, NodeKind::Test, "c", &[11, 3, 11]),
        ]
    );
    // 10 waits for 12, which waits for 11.
    assert_eq!(order.places(), [1, 2, 0]);
    assert_eq!((batch.first(), batch.len(), batch.is_empty()), (10, 3, false));
    let in_order = nodes_record(0, &[(NodeKind::Std, "std", &[]), (NodeKind::Names, "x", &[0])]);
    in_order.nodes(&mut order).unwrap().unwrap();
    assert_eq!(order.places(), [0, 1]);
    let empty = nodes_record(u32::MAX, &[]);
    assert!(empty.nodes(&mut order).unwrap().unwrap().is_empty());
    assert!(order.places().is_empty());
    assert!(Event::Unlisted { count: 1 }.nodes(&mut order).is_none());
}

/// The columns of an `Event::Nodes`, to edit in place.
struct Columns<'a> {
    first: &'a mut u32,
    name_at: &'a mut Vec<u32>,
    deps: &'a mut Vec<u32>,
    deps_at: &'a mut Vec<u32>,
}

/// Columns that disagree, or nodes that wait for each other, are refused with why, never a
/// panic, and leave no order.
#[test]
fn a_malformed_record_of_nodes_is_refused() {
    use crate::events::{Event, NodeKind, Order};
    // Ids 5 and 6: "std" waiting for 1, said before, and "é" waiting for 5.
    let good = || {
        nodes_record(5, &[(NodeKind::Std, "std", &[1]), (NodeKind::Names, "\u{e9}", &[5])])
    };
    let mut order = Order::default();
    assert!(good().nodes(&mut order).unwrap().is_ok());
    assert_eq!(order.places(), [0, 1]);
    let cases: [(fn(Columns<'_>), &str); 11] = [
        (|it| it.name_at.clear(), "name_at of 0 offsets for 2 nodes and 5 entries"),
        (|it| _ = it.name_at.pop(), "name_at of 2 offsets for 2 nodes"),
        (|it| it.name_at[0] = 1, "name_at of 3 offsets for 2 nodes"),
        (|it| it.name_at[2] = 4, "name_at of 3 offsets for 2 nodes and 5 entries"),
        (|it| it.name_at[1] = 6, "name_at goes back at node 1"),
        (|it| it.name_at[1] = 4, "name_at cuts a character at node 1"),
        (|it| it.deps_at[2] = 1, "deps_at of 3 offsets for 2 nodes and 2 entries"),
        (|it| it.deps_at[1] = 3, "deps_at goes back at node 1"),
        (|it| *it.first = u32::MAX - 1, "2 ids from 4294967294 past u32"),
        (|it| it.deps[0] = 6, "nodes 5, 6 wait for each other"),
        (|it| it.deps[1] = 6, "nodes 6 wait for each other"),
    ];
    for (edit, what) in cases {
        let mut event = good();
        let Event::Nodes { first, name_at, deps, deps_at, .. } = &mut event else {
            unreachable!("an Event::Nodes")
        };
        edit(Columns { first, name_at, deps, deps_at });
        good().nodes(&mut order).unwrap().unwrap();
        let why = event.nodes(&mut order).unwrap().unwrap_err().to_string();
        assert!(why.contains(what), "{why}, not {what}");
        assert!(why.starts_with("a record of nodes is malformed: "), "{why}");
        assert!(order.places().is_empty(), "{why}");
    }
}

/// A cycle of more nodes than are shown names the first eight and counts the rest, as the
/// message built from a list of the stuck ones did.
#[test]
fn a_long_cycle_names_eight_nodes() {
    use crate::events::{NodeKind, Order};
    let deps: Vec<[u32; 1]> = (0..11).map(|at| [(at + 1) % 11]).collect();
    let nodes: Vec<(NodeKind, &str, &[u32])> =
        deps.iter().map(|dep| (NodeKind::Codegen, "n", &dep[..])).collect();
    let why = nodes_record(0, &nodes).nodes(&mut Order::default()).unwrap().unwrap_err().0;
    assert_eq!(why, "nodes 0, 1, 2, 3, 4, 5, 6, 7 and 3 more wait for each other");
}

/// `Cause::shown` writes what `Cause::short` says, each the words they said before.
#[test]
fn a_cause_is_said_in_a_few_words() {
    use crate::events::Cause;
    let name = || "serde".to_owned();
    let cases = [
        (Cause::Cold, "not kept"),
        (Cause::Edited { file: "src/lib.rs".into() }, "src/lib.rs edited"),
        (Cause::Body, "body edited"),
        (Cause::Signature, "signature changed"),
        (Cause::Callee { name: name() }, "calls serde"),
        (Cause::Uses, "uses what changed"),
        (Cause::Dependency { name: name() }, "serde changed"),
        (Cause::Flags { what: "RUSTFLAGS".to_owned() }, "RUSTFLAGS changed"),
    ];
    for (cause, said) in cases {
        assert_eq!(cause.short(), said);
        assert_eq!(format!("({})", cause.shown()), format!("({said})"));
    }
}

/// Random columns, many of them malformed, are read or refused but never panic the reader;
/// of those read, the order is the one a plain search for the first node that can go gives.
#[test]
fn random_records_of_nodes_are_read_or_refused() {
    use crate::events::{Event, NodeKind, Order};
    let mut random = Random(SplitMix64::new(0x9e37_79b9_7f4a_7c15));
    let mut order = Order::default();
    let (mut read, mut refused, mut out_of_order) = (0, 0, 0);
    for _ in 0..4000 {
        let len = (random.next() % 7) as usize;
        let first = match random.next() % 50 {
            0 => u32::MAX - 3,
            _ => (random.next() % 20) as u32,
        };
        let names: String = (0..len).map(|_| random.text()).collect();
        let below = |random: &mut Random, end: usize| (random.next() % (end as u64 + 1)) as u32;
        let mut name_at: Vec<u32> = (0..=len).map(|_| below(&mut random, names.len())).collect();
        // Deps from two before `first` to six after it: said before, of the batch, or past it.
        let mut deps: Vec<u32> = (0..random.next() % 10)
            .map(|_| first.wrapping_add((random.next() % 9) as u32).wrapping_sub(2))
            .collect();
        if random.next() % 3 == 0 {
            deps.retain(|&it| it.wrapping_sub(first) >= len as u32);
        }
        let mut deps_at: Vec<u32> = (0..=len).map(|_| below(&mut random, deps.len())).collect();
        // Mostly offsets that agree, so that cycles and orders are reached.
        if random.next() % 4 != 0 {
            for (at, end) in [(&mut name_at, names.len()), (&mut deps_at, deps.len())] {
                at.sort_unstable();
                at[0] = 0;
                at[len] = end as u32;
            }
        }
        let kinds = vec![NodeKind::Codegen; len];
        let event = Event::Nodes { first, kinds, names, name_at, deps, deps_at };
        match event.nodes(&mut order).unwrap() {
            Err(_) => refused += 1,
            Ok(batch) => {
                read += 1;
                let nodes: Vec<(u32, Vec<u32>)> =
                    batch.iter().map(|it| (it.id, it.deps.to_vec())).collect();
                let want = plain_order(&nodes);
                assert_eq!(order.places(), want, "{event:?}");
                let moved = want.iter().enumerate().any(|(at, &it)| it as usize != at);
                out_of_order += usize::from(moved);
            }
        }
    }
    assert!(
        read > 500 && refused > 500 && out_of_order > 20,
        "{read} read, {refused} refused, {out_of_order} out of order"
    );
}

/// The places of `nodes` (each its id and deps), each after those of them it waits for, by
/// looking each time for the first that can go.
fn plain_order(nodes: &[(u32, Vec<u32>)]) -> Vec<u32> {
    let mut placed = vec![false; nodes.len()];
    let mut order = Vec::new();
    while order.len() < nodes.len() {
        let can = (0..nodes.len()).find(|&at| {
            !placed[at]
                && nodes[at].1.iter().all(|dep| {
                    nodes.iter().position(|it| it.0 == *dep).is_none_or(|it| placed[it])
                })
        });
        let at = can.expect("a batch that was read has no cycle");
        placed[at] = true;
        order.push(at as u32);
    }
    order
}

/// One record of every event, each field set.
fn every_event() -> Vec<crate::events::Record> {
    use crate::events::{
        Cache, Cause, CrateState, Event, Fns, Function, How, Level, NodeKind, NodeState, Record,
        Slow, Step, Summary, Took,
    };
    let fns = Fns { total: 5353, done: 12, kept: 2 };
    let slow = || vec![Slow { krate: "rg".into(), name: "rg::main".into(), ns: 300_000_000 }];
    let cache = Cache { found: 7, fetched: 2, compiled: 1, cold: Some("no store".into()) };
    let summary = Summary {
        took_ns: 9_000_000_000,
        crates: 31,
        fns,
        rebuilt: vec![("render".into(), Cause::Callee { name: "draw".into() })],
        more: 4,
        error: Some("anim failed".into()),
        cache: Some(cache.clone()),
        slowest: vec![Took { step: Step::Codegen, of: Some("anim".into()), took_ns: 5 }],
    };
    let function = |id: u32, how: How| Function {
        id,
        key: u64::from(id).wrapping_mul(0x9e37_79b9_7f4a_7c15),
        krate: "anim".into(),
        name: format!("anim::scene::render_{id}"),
        calls: vec![id + 1, id / 2],
        how,
    };
    let events = [
        Event::Began { build: 3, command: "build".into(), root: "/w".into() },
        Event::Started { step: Step::FrontEnd, of: Some("anim".into()) },
        Event::Ended { step: Step::Link, of: None },
        Event::Progress { step: Step::Check, done: 3, total: 14, longest: Some("a.rs".into()) },
        Event::Crate { name: "anim".into(), state: CrateState::Running, fns },
        Event::Rebuilt { name: "render".into(), krate: "anim".into(), cause: Cause::Body },
        Event::Unlisted { count: 9 },
        Event::Cache { cache },
        Event::Refused { name: "asm".into(), why: "global_asm! of x86".into() },
        Event::Said { level: Level::Warning, text: "unused".into() },
        Event::Finished { ok: false, summary },
        Event::Node { id: 1, kind: NodeKind::Std, name: "std".into(), deps: vec![0] },
        Event::Nodes {
            first: 2,
            kinds: vec![NodeKind::Names, NodeKind::Bodies],
            names: "animanim".into(),
            name_at: vec![0, 4, 8],
            deps: vec![1, 2],
            deps_at: vec![0, 1, 2],
        },
        Event::Moved { id: 2, state: NodeState::Failed { why: "E0308".into() } },
        Event::Critical { path: vec![0, 1, 3] },
        Event::Cores { busy: 790, of: 8 },
        Event::Compiling { functions: slow() },
        Event::Slowest { functions: slow() },
        Event::Functions {
            object: 2,
            whole: true,
            functions: vec![
                function(0, How::Compiled { ns: 125_000 }),
                function(1, How::Unit),
                function(2, How::Unchanged),
                function(3, How::Stored),
            ],
        },
    ];
    events.into_iter().enumerate().map(|(at, event)| Record { at_ns: at as u64, event }).collect()
}

/// Every record is written as it comes, no map of it put in order, to the bytes the encoding
/// gives any order of its fields; the same entries as JSON has.
#[test]
fn every_record_is_written_in_its_keys_order() {
    let mut writer = crate::cbor::Writer::new();
    let mut out = Vec::new();
    for record in every_event() {
        let message = Message::new(3, &record);
        out.clear();
        writer.append(&mut out, &message).unwrap();
        assert_eq!(writer.permuted(), 0, "{record:?}");
        assert_eq!(out, crate::cbor::reference::to_vec(&message).unwrap(), "{record:?}");
        check(&out).unwrap();
        let read: Message<crate::events::Record> = from_slice(&out).unwrap();
        assert_eq!(read.body, record);
        let entries: serde_json::Value = from_slice(&out).unwrap();
        assert_eq!(entries, serde_json::to_value(&message).unwrap(), "{record:?}");
    }
}

/// JSON shows the fields in the order the types declare them, as serde's derive wrote them.
#[test]
fn json_keeps_the_declared_order() {
    use crate::events::{Event, Function, How, Summary};
    let function = Function {
        id: 1,
        key: 2,
        krate: "anim".into(),
        name: "render".into(),
        calls: vec![0],
        how: How::Compiled { ns: 5 },
    };
    assert_eq!(
        serde_json::to_string(&function).unwrap(),
        r#"{"id":1,"key":2,"krate":"anim","name":"render","calls":[0],"how":{"Compiled":{"ns":5}}}"#
    );
    let began = Event::Began { build: 1, command: "build".into(), root: "/w".into() };
    assert_eq!(
        serde_json::to_string(&Message::new(4, &began)).unwrap(),
        r#"{"schema":2,"id":4,"body":{"Began":{"build":1,"command":"build","root":"/w"}}}"#
    );
    assert_eq!(
        serde_json::to_string(&Summary::default()).unwrap(),
        concat!(
            r#"{"took_ns":0,"crates":0,"fns":{"total":0,"done":0,"kept":0},"rebuilt":[],"#,
            r#""more":0,"error":null,"cache":null,"slowest":[]}"#
        )
    );
}
