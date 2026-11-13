//! The patch protocol's bytes: every message, written by this crate's hand codec, is one item
//! of RFC 8949's core deterministic encoding as blaze-protocol's `cbor::check` holds its own,
//! and reads back as the message it was; and what is not a message of this schema is refused.

use blaze_protocol::cbor;
use hegel::TestCase;
use hegel::generators as gs;

use crate::wire::{
    Arch, Ask, Bind, LONGEST_FRAME, List, Os, ProgramId, RecordId, SCHEMA, StaticDef, Tell,
    WireError, split,
};
use crate::{CodeId, Slot, Thread, Tier, Txn};

/// A message with what it borrows owned, so a drawn one outlives its draw.
#[derive(Clone, Debug)]
enum Owned {
    Hello {
        program: [u8; 32],
        pid: u64,
        arm: bool,
        mac: bool,
        page: u32,
        holds: Option<[u8; 32]>,
        running: bool,
    },
    Need { slot: u32, thread: u64 },
    Applied { txn: u64, epoch: u64 },
    Failed { txn: u64, why: String },
    Frames { txn: u64, held: Vec<Slot> },
    Ended { domain: u64, marks: Vec<u8>, needs: Option<String> },
    Restarting { txn: u64 },
    Welcome { slots: u32 },
    Start { entry: u32, inits: Vec<u32> },
    Record { id: [u8; 32], bytes: Vec<u8> },
    Statics { defs: Vec<(u32, [u8; 32])>, refs: Vec<u32> },
    Edit { txn: u64, binds: Vec<OwnedBind>, refs: Vec<u32>, declare: u32, together: bool },
    Refused { slot: u32, diagnostic: String },
    Probe { txn: u64, slots: Vec<Slot> },
    Restart { txn: u64, why: String },
    Unheld { why: String },
    Image { bytes: Vec<u8> },
}

/// A bind without its refs: every bind of a drawn edit shares the edit's one list.
#[derive(Clone, Copy, Debug)]
struct OwnedBind {
    slot: u32,
    record: [u8; 32],
    code: [u8; 32],
    optimized: bool,
    len: u64,
    align: u32,
}

/// What a message borrows that is not in its [`Owned`] as it is: the lists of structs.
#[derive(Default)]
struct Lists<'a> {
    binds: Vec<Bind<'a>>,
    defs: Vec<StaticDef<'a>>,
}

enum Message<'a> {
    Ask(Ask<'a>),
    Tell(Tell<'a>),
}

impl Owned {
    /// The lists of structs `message` borrows.
    fn lists(&self) -> Lists<'_> {
        match self {
            Owned::Edit { binds, refs, .. } => Lists {
                binds: binds
                    .iter()
                    .map(|bind| Bind {
                        slot: Slot(bind.slot),
                        record: RecordId(bind.record),
                        code: CodeId(bind.code),
                        tier: if bind.optimized { Tier::Optimized } else { Tier::Zero },
                        len: bind.len,
                        align: bind.align,
                        refs: List::of(refs),
                    })
                    .collect(),
                defs: Vec::new(),
            },
            Owned::Statics { defs, refs } => Lists {
                binds: Vec::new(),
                defs: defs
                    .iter()
                    .map(|(number, record)| StaticDef {
                        number: *number,
                        record: RecordId(*record),
                        refs: List::of(refs),
                    })
                    .collect(),
            },
            _ => Lists::default(),
        }
    }

    fn message<'a>(&'a self, lists: &'a Lists<'a>) -> Message<'a> {
        match self {
            Owned::Hello { program, pid, arm, mac, page, holds, running } => {
                Message::Ask(Ask::Hello {
                    program: ProgramId(*program),
                    pid: *pid,
                    arch: if *arm { Arch::Aarch64 } else { Arch::X86_64 },
                    os: if *mac { Os::Macos } else { Os::Linux },
                    page: *page,
                    holds: *holds,
                    running: *running,
                })
            }
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
            Owned::Welcome { slots } => Message::Tell(Tell::Welcome { slots: *slots }),
            Owned::Start { entry, inits } => {
                Message::Tell(Tell::Start { entry: Slot(*entry), inits: List::of(inits) })
            }
            Owned::Record { id, bytes } => {
                Message::Tell(Tell::Record { id: RecordId(*id), bytes })
            }
            Owned::Statics { .. } => {
                Message::Tell(Tell::Statics { defs: List::of(&lists.defs) })
            }
            Owned::Edit { txn, declare, together, .. } => Message::Tell(Tell::Edit {
                txn: Txn(*txn),
                binds: List::of(&lists.binds),
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
            Owned::Unheld { why } => Message::Tell(Tell::Unheld { why }),
            Owned::Image { bytes } => Message::Tell(Tell::Image { bytes }),
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
    match pick(tc, 16) {
        16 => Owned::Image { bytes: (0..pick(tc, 300)).map(|at| at as u8).collect() },
        0 => Owned::Hello {
            program: id(tc),
            pid: number(tc),
            arm: tc.draw(gs::booleans()),
            mac: tc.draw(gs::booleans()),
            page: small(tc),
            holds: tc.draw(gs::booleans()).then(|| id(tc)),
            running: tc.draw(gs::booleans()),
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
        7 => Owned::Welcome { slots: small(tc) },
        14 => Owned::Unheld { why: text(tc) },
        13 => Owned::Start {
            entry: small(tc),
            inits: (0..pick(tc, 30)).map(|_| small(tc)).collect(),
        },
        11 => Owned::Record {
            id: id(tc),
            bytes: (0..pick(tc, 300)).map(|at| at as u8).collect(),
        },
        12 => Owned::Statics {
            defs: (0..pick(tc, 20)).map(|_| (small(tc), id(tc))).collect(),
            refs: (0..pick(tc, 30)).map(|_| small(tc)).collect(),
        },
        8 => Owned::Edit {
            txn: number(tc),
            binds: (0..pick(tc, 20))
                .map(|_| OwnedBind {
                    slot: small(tc),
                    record: id(tc),
                    code: id(tc),
                    optimized: tc.draw(gs::booleans()),
                    len: number(tc),
                    align: small(tc),
                })
                .collect(),
            refs: (0..pick(tc, 30)).map(|_| small(tc)).collect(),
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
    let lists = owned.lists();
    let message = owned.message(&lists);
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
    0x66, b's', b'c', b'h', b'e', b'm', b'a', 0x03,
];

#[test]
fn a_need_is_these_bytes() {
    let need = Ask::Need { slot: Slot(3), thread: Thread(7) };
    let mut out = Vec::new();
    need.encode(1, &mut out).expect("encoded");
    assert_eq!(out, NEED);
    assert_eq!(SCHEMA, 3, "the bytes above say schema 3");
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
    let other = need_with(38, &[0x01]);
    assert_eq!(Ask::decode(&other), Err(WireError::Schema { said: 1 }));
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

/// A `Hello` of these fields under id 1, its item as the launcher writes it.
fn hello_item(holds: Option<[u8; 32]>, running: bool) -> Vec<u8> {
    let hello = Ask::Hello {
        program: ProgramId([5; 32]),
        pid: 9,
        arch: Arch::X86_64,
        os: Os::Linux,
        page: 4096,
        holds,
        running,
    };
    let mut out = Vec::new();
    hello.encode(1, &mut out).expect("a hello fits a frame");
    out[4..].to_vec()
}

/// The invariant: a `Hello` reads back as itself with a start image's check word and without
/// one, running or not, in the deterministic encoding, its seven keys in the order of their
/// encodings. The control: a check word of 31 bytes is refused by name, never read as `None`.
#[test]
fn a_hello_round_trips_with_and_without_what_it_holds() {
    for holds in [None, Some([0xab; 32])] {
        for running in [false, true] {
            let item = hello_item(holds, running);
            assert_eq!(cbor::check(&item).map_err(|it| it.to_string()), Ok(()), "{holds:?} {running}");
            let Ok((1, Ask::Hello { holds: read, running: ran, program, .. })) = Ask::decode(&item) else {
                panic!("{holds:?} {running}: {:?}", Ask::decode(&item));
            };
            assert_eq!((read, ran, program), (holds, running, ProgramId([5; 32])));
        }
    }
    // The control: the check word's byte string one byte short. Its head is `0x58 0x20`
    // (a byte string of 32), found by the bytes that follow it.
    let item = hello_item(Some([0xab; 32]), false);
    let at = item.windows(34).position(|it| it[..2] == [0x58, 0x20] && it[2..] == [0xab; 32]).expect("the check word");
    let mut short = item[..at].to_vec();
    short.extend_from_slice(&[0x58, 0x1f]);
    short.extend_from_slice(&item[at + 2..at + 33]);
    short.extend_from_slice(&item[at + 34..]);
    assert_eq!(
        Ask::decode(&short),
        Err(WireError::Range { at, what: "a start image's check word is 32 bytes" })
    );
}

/// The invariant: a `Hello` of the five fields a launcher said before `holds` and `running`
/// is read as holding nothing and not running, so a daemon answers it as it answered every
/// launcher before. The control: a map of 6 is neither, and is refused.
#[test]
fn a_hello_of_five_fields_holds_nothing_and_is_not_running() {
    let five = |fields: usize| {
        let mut out = Vec::new();
        let mut to = crate::wire::cbor::Writer::new(&mut out);
        to.map(3);
        to.text("id");
        to.uint(1);
        to.text("body");
        to.map(1);
        to.text("Hello");
        to.map(fields);
        to.text("os");
        to.text("Linux");
        to.text("pid");
        to.uint(9);
        to.text("arch");
        to.text("X86_64");
        to.text("page");
        to.uint(4096);
        to.text("program");
        to.bytes(&[5; 32]);
        to.text("schema");
        to.uint(u64::from(SCHEMA));
        out
    };
    let item = five(5);
    assert_eq!(cbor::check(&item).map_err(|it| it.to_string()), Ok(()));
    assert_eq!(
        Ask::decode(&item),
        Ok((
            1,
            Ask::Hello {
                program: ProgramId([5; 32]),
                pid: 9,
                arch: Arch::X86_64,
                os: Os::Linux,
                page: 4096,
                holds: None,
                running: false,
            }
        ))
    );
    assert_eq!(item, five(5), "one encoding");
    // The control.
    assert!(Ask::decode(&five(6)).is_err(), "a hello of 6 fields was read");
}
