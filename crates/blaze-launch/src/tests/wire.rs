//! The patch protocol's bytes: every message, written by this crate's hand codec, is one item
//! of RFC 8949's core deterministic encoding as blaze-protocol's `cbor::check` holds its own,
//! and reads back as the message it was; and what is not a message of this schema is refused.

use blaze_protocol::cbor;
use hegel::TestCase;
use hegel::generators as gs;

use crate::wire::{
    Arch, Ask, Bind, LONGEST_FRAME, List, Os, ProgramId, RecordId, SCHEMA, Tell, WireError, split,
};
use crate::{CodeId, Slot, Thread, Tier, Txn};

/// A message with what it borrows owned, so a drawn one outlives its draw.
#[derive(Clone, Debug)]
enum Owned {
    Hello { program: [u8; 32], pid: u64, arm: bool, mac: bool, page: u32 },
    Need { slot: u32, thread: u64 },
    Applied { txn: u64, epoch: u64 },
    Failed { txn: u64, why: String },
    Frames { txn: u64, held: Vec<Slot> },
    Ended { domain: u64, marks: Vec<u8>, needs: Option<String> },
    Restarting { txn: u64 },
    Welcome { slots: u32, store: String, statics: u64 },
    Edit { txn: u64, binds: Vec<Bind>, declare: u32, together: bool },
    Refused { slot: u32, diagnostic: String },
    Probe { txn: u64, slots: Vec<Slot> },
    Restart { txn: u64, why: String },
}

enum Message<'a> {
    Ask(Ask<'a>),
    Tell(Tell<'a>),
}

impl Owned {
    fn message(&self) -> Message<'_> {
        match self {
            Owned::Hello { program, pid, arm, mac, page } => Message::Ask(Ask::Hello {
                program: ProgramId(*program),
                pid: *pid,
                arch: if *arm { Arch::Aarch64 } else { Arch::X86_64 },
                os: if *mac { Os::Macos } else { Os::Linux },
                page: *page,
            }),
            Owned::Need { slot, thread } => {
                Message::Ask(Ask::Need { slot: Slot(*slot), thread: Thread(*thread) })
            }
            Owned::Applied { txn, epoch } => {
                Message::Ask(Ask::Applied { txn: Txn(*txn), epoch: *epoch })
            }
            Owned::Failed { txn, why } => Message::Ask(Ask::Failed { txn: Txn(*txn), why }),
            Owned::Frames { txn, held } => {
                Message::Ask(Ask::Frames { txn: Txn(*txn), held: List::of(held) })
            }
            Owned::Ended { domain, marks, needs } => {
                Message::Ask(Ask::Ended { domain: *domain, marks, needs: needs.as_deref() })
            }
            Owned::Restarting { txn } => Message::Ask(Ask::Restarting { txn: Txn(*txn) }),
            Owned::Welcome { slots, store, statics } => {
                Message::Tell(Tell::Welcome { slots: *slots, store, statics: *statics })
            }
            Owned::Edit { txn, binds, declare, together } => Message::Tell(Tell::Edit {
                txn: Txn(*txn),
                binds: List::of(binds),
                declare: *declare,
                together: *together,
            }),
            Owned::Refused { slot, diagnostic } => {
                Message::Tell(Tell::Refused { slot: Slot(*slot), diagnostic })
            }
            Owned::Probe { txn, slots } => {
                Message::Tell(Tell::Probe { txn: Txn(*txn), slots: List::of(slots) })
            }
            Owned::Restart { txn, why } => Message::Tell(Tell::Restart { txn: Txn(*txn), why }),
        }
    }
}

fn pick(tc: &TestCase, max: usize) -> usize {
    tc.draw(gs::integers::<usize>().max_value(max))
}

/// A number at the edge of a CBOR head's size, or any.
fn number(tc: &TestCase) -> u64 {
    const EDGES: [u64; 10] =
        [0, 23, 24, 255, 256, 65_535, 65_536, 0xffff_ffff, 0x1_0000_0000, u64::MAX];
    match pick(tc, 2) {
        0 => EDGES[pick(tc, EDGES.len() - 1)],
        1 => pick(tc, 1000) as u64,
        _ => ((pick(tc, u32::MAX as usize) as u64) << 32) | pick(tc, u32::MAX as usize) as u64,
    }
}

fn small(tc: &TestCase) -> u32 {
    number(tc) as u32
}

fn id(tc: &TestCase) -> [u8; 32] {
    let seed = number(tc).to_le_bytes();
    core::array::from_fn(|at| seed[at % 8] ^ at as u8)
}

fn text(tc: &TestCase) -> String {
    const TEXTS: [&str; 5] =
        ["", "x", "the record of `main` is not in the store", "λ → 走る", "a\u{0}b"];
    let text = TEXTS[pick(tc, TEXTS.len() - 1)];
    // Past the lengths one byte and two bytes of head hold.
    text.repeat(1 + pick(tc, 2) * 150)
}

fn slots(tc: &TestCase) -> Vec<Slot> {
    (0..pick(tc, 30)).map(|_| Slot(small(tc))).collect()
}

fn owned(tc: &TestCase) -> Owned {
    match pick(tc, 11) {
        0 => Owned::Hello {
            program: id(tc),
            pid: number(tc),
            arm: tc.draw(gs::booleans()),
            mac: tc.draw(gs::booleans()),
            page: small(tc),
        },
        1 => Owned::Need { slot: small(tc), thread: number(tc) },
        2 => Owned::Applied { txn: number(tc), epoch: number(tc) },
        3 => Owned::Failed { txn: number(tc), why: text(tc) },
        4 => Owned::Frames { txn: number(tc), held: slots(tc) },
        5 => Owned::Ended {
            domain: number(tc),
            marks: (0..pick(tc, 300)).map(|_| pick(tc, 1) as u8).collect(),
            needs: tc.draw(gs::booleans()).then(|| text(tc)),
        },
        6 => Owned::Restarting { txn: number(tc) },
        7 => Owned::Welcome { slots: small(tc), store: text(tc), statics: number(tc) },
        8 => Owned::Edit {
            txn: number(tc),
            binds: (0..pick(tc, 20))
                .map(|_| Bind {
                    slot: Slot(small(tc)),
                    record: RecordId(id(tc)),
                    code: CodeId(id(tc)),
                    tier: if tc.draw(gs::booleans()) { Tier::Zero } else { Tier::Optimized },
                    len: number(tc),
                    align: small(tc),
                })
                .collect(),
            declare: small(tc),
            together: tc.draw(gs::booleans()),
        },
        9 => Owned::Refused { slot: small(tc), diagnostic: text(tc) },
        10 => Owned::Probe { txn: number(tc), slots: slots(tc) },
        _ => Owned::Restart { txn: number(tc), why: text(tc) },
    }
}

/// The frame of `message` under `id`.
fn encoded(message: &Message<'_>, id: u64) -> Vec<u8> {
    let mut out = Vec::new();
    match message {
        Message::Ask(ask) => ask.encode(id, &mut out),
        Message::Tell(tell) => tell.encode(id, &mut out),
    }
    .expect("a message this short fits a frame");
    out
}

/// The invariant: every message is written as one frame whose item is in the core
/// deterministic encoding (so one message has one encoding), and reads back, with its id, as
/// the message it was; two frames on one stream split where the first ends.
#[hegel::test(test_cases = 400)]
fn every_message_is_deterministic_cbor_and_reads_back(tc: TestCase) {
    let owned = owned(&tc);
    let id = number(&tc);
    let message = owned.message();
    let mut stream = encoded(&message, id);
    let one = stream.len();
    stream.extend_from_slice(&encoded(&message, id));

    let (item, took) = split(&stream).expect("a frame").expect("a whole frame");
    assert_eq!(took, one, "{owned:?}");
    assert_eq!(split(&stream[..one - 1]), Ok(None), "a frame short of its last byte is not here");
    if let Err(why) = cbor::check(item) {
        panic!("{owned:?} is not deterministic CBOR: {why}");
    }
    match message {
        Message::Ask(ask) => {
            assert_eq!(Ask::decode(item), Ok((id, ask)));
            assert!(Tell::decode(item).is_err(), "an ask read as a tell: {owned:?}");
        }
        Message::Tell(tell) => {
            assert_eq!(Tell::decode(item), Ok((id, tell)));
            assert!(Ask::decode(item).is_err(), "a tell read as an ask: {owned:?}");
        }
    }
    // Any cut of the item is refused, never read as a shorter message.
    let cut = pick(&tc, item.len() - 1);
    assert!(Ask::decode(&item[..cut]).is_err() && Tell::decode(&item[..cut]).is_err());
}

/// `Need { slot: 3, thread: 7 }` under id 1, byte for byte: the map `{id, body, schema}` with
/// its keys in the order of their encodings, the body a map of the variant to its fields.
const NEED: [u8; 43] = [
    0, 0, 0, 39, // the frame's length
    0xa3, // a map of 3
    0x62, b'i', b'd', 0x01, //
    0x64, b'b', b'o', b'd', b'y', //
    0xa1, 0x64, b'N', b'e', b'e', b'd', //
    0xa2, //
    0x64, b's', b'l', b'o', b't', 0x03, //
    0x66, b't', b'h', b'r', b'e', b'a', b'd', 0x07, //
    0x66, b's', b'c', b'h', b'e', b'm', b'a', 0x01,
];

#[test]
fn a_need_is_these_bytes() {
    let need = Ask::Need { slot: Slot(3), thread: Thread(7) };
    let mut out = Vec::new();
    need.encode(1, &mut out).expect("encoded");
    assert_eq!(out, NEED);
    assert_eq!(SCHEMA, 1, "the bytes above say schema 1");
    assert_eq!(Ask::decode(&NEED[4..]), Ok((1, need)));
}

/// `NEED`'s item with the bytes from `at` replaced by `with`.
fn need_with(at: usize, with: &[u8]) -> Vec<u8> {
    let mut item = NEED[4..].to_vec();
    drop(item.splice(at..at + with.len(), with.iter().copied()));
    item
}

#[test]
fn what_is_not_a_message_of_this_schema_is_refused_by_name() {
    // Another schema, though the body is one this schema could read.
    let other = need_with(38, &[0x02]);
    assert_eq!(Ask::decode(&other), Err(WireError::Schema { said: 2 }));
    // A variant this schema has not.
    let nope = need_with(12, b"Nope");
    assert_eq!(Ask::decode(&nope), Err(WireError::Variant { at: 11 }));
    // Bytes after the item.
    let mut more = NEED[4..].to_vec();
    more.push(0);
    assert_eq!(Ask::decode(&more), Err(WireError::Left { at: 39 }));
    // A length past what a frame carries is refused before the item is waited for.
    let long = (LONGEST_FRAME + 1).to_be_bytes();
    assert_eq!(split(&long), Err(WireError::Long { len: u64::from(LONGEST_FRAME) + 1 }));
    assert_eq!(split(&[0, 0]), Ok(None));
}

/// The control of the invariant above: bytes that differ from a message only in what the
/// deterministic encoding forbids (a number in a longer head than it needs, two keys in the
/// other order) are refused by blaze-protocol's check, the oracle, and by this crate's reader.
#[test]
fn the_oracle_catches_a_longer_head_and_keys_out_of_order() {
    assert_eq!(cbor::check(&NEED[4..]).map_err(|it| it.to_string()), Ok(()));

    // The slot, 3, in a two-byte head.
    let mut long = NEED[4..].to_vec();
    drop(long.splice(22..23, [0x18, 0x03]));
    assert!(cbor::check(&long).is_err(), "the oracle took a number in a longer head");
    assert_eq!(Ask::decode(&long), Err(WireError::NotShortest { at: 22 }));

    // `thread` before `slot`.
    let item = &NEED[4..];
    let mut swapped = item[..17].to_vec();
    swapped.extend_from_slice(&item[23..31]);
    swapped.extend_from_slice(&item[17..23]);
    swapped.extend_from_slice(&item[31..]);
    assert_eq!(swapped.len(), 39);
    assert!(cbor::check(&swapped).is_err(), "the oracle took keys out of order");
    assert_eq!(Ask::decode(&swapped), Err(WireError::Key { at: 17, want: "slot" }));
}
