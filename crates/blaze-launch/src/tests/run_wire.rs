//! The run wire's bytes ([`crate::wire::run`]): every message is one item of the core
//! deterministic encoding (blaze-protocol's `cbor::check`, the oracle), reads back as the message
//! it was, and every cut of it is a typed refusal; a patch-protocol item, a run item of another
//! schema and an unknown variant are refused by name.

use blaze_protocol::cbor;
use blaze_sim::{Config, Tape};

use crate::wire::run::{Declared, Down, Failure, HookKind, Needed, RUN_SCHEMA, Symbol, Up};
use crate::wire::{Ask, List, RecordId, WireError, split};
use crate::{Slot, Thread, Tier};

/// A message with what it borrows owned, so a drawn one outlives its draw.
#[derive(Clone, Debug)]
enum Owned {
    Need { need: Needed, thread: u64 },
    Declare { pairs: Vec<Declared> },
    Publish { slot: u32, optimized: bool, record: [u8; 32] },
    Positioned { slot: u32, optimized: bool, record: [u8; 32], values: Vec<u8> },
    Hooks { slot: u32, kind: HookKind },
    Promote { slot: u32, record: [u8; 32], inlined: Vec<Slot>, values: Vec<u8> },
    Bytes { record: [u8; 32], at: u64, total: u64, part: Vec<u8> },
    Failed { need: Needed, why: OwnedFailure },
}

#[derive(Clone, Debug)]
enum OwnedFailure {
    Unresolved,
    Refused([u8; 32]),
    Panicked,
    NotBuilt(String),
    Overran(u64),
}

pub(super) enum Message<'a> {
    Up(Up),
    Down(Down<'a>),
}

impl Owned {
    fn message(&self) -> Message<'_> {
        match self {
            Owned::Need { need, thread } => Message::Up(Up::Need { need: *need, thread: Thread(*thread) }),
            Owned::Declare { pairs } => Message::Down(Down::Declare { pairs: List::of(pairs) }),
            Owned::Publish { slot, optimized, record } => Message::Down(Down::Publish {
                slot: Slot(*slot),
                tier: if *optimized { Tier::Optimized } else { Tier::Zero },
                record: RecordId(*record),
            }),
            Owned::Positioned { slot, optimized, record, values } => Message::Down(Down::Positioned {
                slot: Slot(*slot),
                tier: if *optimized { Tier::Optimized } else { Tier::Zero },
                record: RecordId(*record),
                values,
            }),
            Owned::Hooks { slot, kind } => Message::Down(Down::Hooks { slot: Slot(*slot), kind: *kind }),
            Owned::Promote { slot, record, inlined, values } => Message::Down(Down::Promote {
                slot: Slot(*slot),
                record: RecordId(*record),
                inlined: List::of(inlined),
                values,
            }),
            Owned::Bytes { record, at, total, part } => Message::Down(Down::Bytes { record: RecordId(*record), at: *at, total: *total, part }),
            Owned::Failed { need, why } => {
                let why = match why {
                    OwnedFailure::Unresolved => Failure::Unresolved,
                    OwnedFailure::Refused(findings) => Failure::Refused { findings: *findings },
                    OwnedFailure::Panicked => Failure::Panicked,
                    OwnedFailure::NotBuilt(what) => Failure::NotBuilt { what: what.as_str() },
                    OwnedFailure::Overran(bound) => Failure::Overran { bound: *bound },
                };
                Message::Down(Down::Failed { need: *need, why })
            }
        }
    }
}

/// A number from each width a head has, so every head length is written and read.
fn number(tape: &Tape) -> u64 {
    match tape.below(5, "width") {
        0 => tape.below(24, "tiny"),
        1 => tape.range(24..256, "byte"),
        2 => tape.range(256..65_536, "short"),
        3 => tape.range(65_536..(1 << 32), "word"),
        _ => tape.range(1 << 32..u64::MAX, "long"),
    }
}

fn small(tape: &Tape) -> u32 {
    (number(tape) % (u64::from(u32::MAX) + 1)) as u32
}

fn bytes32(tape: &Tape) -> [u8; 32] {
    let mut out = [0; 32];
    out[..8].copy_from_slice(&number(tape).to_le_bytes());
    out[31] = tape.below(256, "last byte") as u8;
    out
}

fn need(tape: &Tape) -> Needed {
    match tape.coin("need by symbol") {
        true => Needed::Symbol(Symbol(bytes32(tape))),
        false => Needed::Slot(Slot(small(tape))),
    }
}

fn owned(tape: &Tape) -> Owned {
    match tape.below(8, "message") {
        0 => Owned::Need { need: need(tape), thread: number(tape) },
        1 => {
            let count = tape.below(5, "pairs") as usize;
            let pairs = (0..count).map(|_| Declared { symbol: Symbol(bytes32(tape)), slot: Slot(small(tape)) }).collect();
            Owned::Declare { pairs }
        }
        2 => Owned::Publish { slot: small(tape), optimized: tape.coin("optimized"), record: bytes32(tape) },
        3 => Owned::Positioned {
            slot: small(tape),
            optimized: tape.coin("optimized"),
            record: bytes32(tape),
            values: (0..tape.range(1..8, "position fields")).flat_map(|_| small(tape).max(1).to_le_bytes()).collect(),
        },
        4 => Owned::Hooks {
            slot: small(tape),
            kind: [HookKind::Plain, HookKind::Count, HookKind::Mark, HookKind::Hook, HookKind::Span]
                [tape.index(5, "hook kind")],
        },
        5 => {
            let count = tape.below(5, "inlined") as usize;
            let inlined = (0..count).map(|_| Slot(small(tape))).collect();
            Owned::Promote {
                slot: small(tape),
                record: bytes32(tape),
                inlined,
                values: (0..tape.below(4, "values")).flat_map(|_| small(tape).to_le_bytes()).collect(),
            }
        }
        6 => {
            // A part of shape the decode takes: not empty, within its total, a total within
            // BYTES_RECORD_MOST (wire/bytes.rs).
            let part: Vec<u8> = (0..tape.range(1..40, "part")).map(|_| tape.below(256, "byte") as u8).collect();
            let at = tape.below(1 << 20, "at");
            let total = at + part.len() as u64 + tape.below(1 << 20, "after");
            Owned::Bytes { record: bytes32(tape), at, total, part }
        }
        _ => {
            let why = match tape.below(5, "failure") {
                0 => OwnedFailure::Unresolved,
                1 => OwnedFailure::Refused(bytes32(tape)),
                2 => OwnedFailure::Panicked,
                3 => OwnedFailure::NotBuilt(["", "a", "unsized locals", "ünïcode"][tape.index(4, "what")].to_owned()),
                _ => OwnedFailure::Overran(number(tape)),
            };
            Owned::Failed { need: need(tape), why }
        }
    }
}

pub(super) fn encoded(message: &Message<'_>, id: u64) -> Vec<u8> {
    let mut out = Vec::new();
    match message {
        Message::Up(up) => up.encode(id, &mut out),
        Message::Down(down) => down.encode(id, &mut out),
    }
    .expect("a message this short fits a frame");
    out
}

/// Invariant: every run-wire message is one frame whose item is deterministic CBOR, reads
/// back with its id as the message it was (and not as the other direction's), and EVERY cut of
/// the item is refused, never read as a shorter message. Expected: passes with no output.
#[test]
fn every_run_message_round_trips_and_every_cut_is_refused() {
    blaze_sim::check(Config::cases(400), |tape: &Tape| {
        let owned = owned(tape);
        let id = number(tape);
        let message = owned.message();
        let mut stream = encoded(&message, id);
        let one = stream.len();
        stream.extend_from_slice(&encoded(&message, id));
        let (item, took) = split(&stream).expect("a frame").expect("a whole frame");
        assert_eq!(took, one, "{owned:?}");
        if let Err(why) = cbor::check(item) {
            panic!("{owned:?} is not deterministic CBOR: {why}");
        }
        match message {
            Message::Up(up) => {
                assert_eq!(Up::decode(item), Ok((id, up)));
                assert!(Down::decode(item).is_err(), "an up message read as a down one: {owned:?}");
            }
            Message::Down(down) => {
                assert_eq!(Down::decode(item), Ok((id, down)));
                assert!(Up::decode(item).is_err(), "a down message read as an up one: {owned:?}");
            }
        }
        // Not a patch-protocol message either.
        assert!(Ask::decode(item).is_err(), "a run message read as the patch protocol's: {owned:?}");
        for cut in 0..item.len() {
            assert!(
                Up::decode(&item[..cut]).is_err() && Down::decode(&item[..cut]).is_err(),
                "{owned:?} cut to {cut} of {} bytes was read",
                item.len()
            );
        }
    });
}

/// `Need { need: Slot(3), thread: 7 }` under id 1, byte for byte: `{id, run, body}` with its
/// keys in the order of their encodings.
const NEED: [u8; 46] = [
    0, 0, 0, 42, // the frame's length
    0xa3, // a map of 3
    0x62, b'i', b'd', 0x01, //
    0x63, b'r', b'u', b'n', 0x06, // the run schema
    0x64, b'b', b'o', b'd', b'y', //
    0xa1, 0x64, b'N', b'e', b'e', b'd', //
    0xa2, //
    0x64, b'n', b'e', b'e', b'd', //
    0xa1, 0x64, b'S', b'l', b'o', b't', 0x03, //
    0x66, b't', b'h', b'r', b'e', b'a', b'd', 0x07,
];

/// The encoding is pinned: a need is these bytes, which the oracle takes as deterministic.
/// Expected: passes with no output.
#[test]
fn a_need_by_slot_is_these_bytes() {
    assert_eq!(RUN_SCHEMA, 6, "a schema step changes the pinned bytes: update NEED");
    let need = Up::Need { need: Needed::Slot(Slot(3)), thread: Thread(7) };
    assert_eq!(encoded(&Message::Up(need), 1), NEED);
    assert_eq!(cbor::check(&NEED[4..]).map_err(|it| it.to_string()), Ok(()));
    assert_eq!(Up::decode(&NEED[4..]), Ok((1, need)));
}

/// What is not a message of this wire is refused by name: a run item of another schema, a
/// patch-protocol item, an unknown variant, a symbol not of 32 bytes. Expected: passes with no
/// output.
#[test]
fn what_is_not_a_run_message_is_refused_by_name() {
    let item = &NEED[4..];
    // Schema 1 (the one before `Up::Fault` carried registers) in the `run` key's value (byte 9
    // of the item).
    let mut other = item.to_vec();
    other[9] = 0x01;
    assert_eq!(Up::decode(&other), Err(WireError::RunSchema { said: 1 }));
    assert!(WireError::RunSchema { said: 1 }.to_string().contains("schema 1"));
    other[9] = 0x03;
    assert_eq!(Down::decode(&other), Err(WireError::RunSchema { said: 3 }));

    // The patch protocol's `Need`: `{id, body, schema}` is refused at its second key.
    let mut patch = Vec::new();
    Ask::Need { slot: Slot(3), thread: Thread(7) }.encode(1, &mut patch).expect("a frame");
    assert_eq!(Up::decode(&patch[4..]), Err(WireError::Key { at: 5, want: "run" }));

    // `Nead`: a variant this wire has not, named at its byte (the text's head, byte 16).
    let mut unknown = item.to_vec();
    unknown[18] = b'a';
    assert_eq!(Up::decode(&unknown), Err(WireError::Variant { at: 16 }));

    // A need by a symbol of 31 bytes.
    let mut short = Vec::new();
    Up::Need { need: Needed::Symbol(Symbol([9; 32])), thread: Thread(7) }.encode(1, &mut short).expect("a frame");
    let item = &short[4..];
    let at = item.windows(2).position(|it| it == [0x58, 32]).expect("the symbol's head");
    let mut cut_symbol = item[..at].to_vec();
    cut_symbol.extend_from_slice(&[0x58, 31]);
    cut_symbol.extend_from_slice(&item[at + 2..at + 2 + 31]);
    cut_symbol.extend_from_slice(&item[at + 2 + 32..]);
    assert_eq!(Up::decode(&cut_symbol), Err(WireError::Range { at, what: "a symbol is 32 bytes" }));
}
