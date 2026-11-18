//! The run wire's `Up::Fault` with the faulting thread's callers (run schema 3, wire/fault.rs
//! `Callers`): none, one, eight (every one named) and the full 32 (eight named), each walk end,
//! read back as written as deterministic CBOR (blaze-protocol's `cbor::check`, the oracle);
//! every cut refused; more spots than return addresses, 33 return addresses and an unknown end
//! refused by name.

use blaze_protocol::cbor;

use super::run_wire::{Message, encoded};
use crate::wire::fault::{Callers, Fault, MOST_CALLERS, NAMED_CALLERS, Placed, Signal, Spot, WalkEnd};
use crate::wire::run::{Symbol, Up};
use crate::wire::{RecordId, WireError, split};

const ENDS: [WalkEnd; 7] = [WalkEnd::Top, WalkEnd::Cap, WalkEnd::OutOfStack, WalkEnd::NotAbove, WalkEnd::Unaligned, WalkEnd::Unread, WalkEnd::NoBounds];

fn spot(byte: u8, offset: u64) -> Spot {
    Spot { placed: Placed::Code, symbol: Symbol([byte; 32]), record: RecordId([byte ^ 0xff; 32]), offset }
}

/// `len` return addresses (small and full-width words), the first eight named, every third of
/// those in no placed code.
fn callers(len: usize, end: WalkEnd) -> Callers {
    let mut c = Callers::none(end);
    c.len = len as u8;
    for i in 0..len {
        c.ret[i] = match i % 2 {
            0 => 0x70_02ae_0000 + (i as u64) * 0x40,
            _ => u64::MAX - i as u64,
        };
        if i < NAMED_CALLERS {
            c.at[i] = (i % 3 != 2).then_some(spot(0x10 + i as u8, 0x54 + i as u64));
        }
    }
    c
}

fn fault(callers: Option<Callers>) -> Fault {
    let at = spot(0x66, 0x3fc);
    Fault { signal: Signal::Trap, pc: Some(0x70_02ae_a76c), at: Some(at), code: None, regs: None, address: 0x70_02ae_a76c, touched: Some(at), write: None, callers }
}

/// The fault's frame (the file's one maker, `run_wire::encoded`): its item is `frame[4..]`.
fn frame_of(fault: Fault) -> Vec<u8> {
    let frame = encoded(&Message::Up(Up::Fault { fault }), 0);
    let (_, took) = split(&frame).expect("a frame").expect("a whole frame");
    assert_eq!(took, frame.len(), "one frame: {fault:?}");
    frame
}

/// Every length class with every end: deterministic CBOR that reads back as itself, each cut
/// refused; `rets` and `spots` the sent prefixes. Prints "run wire: Fault with callers
/// round-trips in N shapes; the full walk's frame is B bytes".
#[test]
fn a_fault_with_callers_round_trips() {
    let mut shapes = 0;
    let mut longest = 0;
    let mut cases = [None; 1 + 4 * ENDS.len()];
    for (k, &end) in ENDS.iter().enumerate() {
        for (j, len) in [0, 1, NAMED_CALLERS, MOST_CALLERS].into_iter().enumerate() {
            cases[1 + 4 * k + j] = Some(callers(len, end));
        }
    }
    for callers in cases {
        let fault = fault(callers);
        let frame = frame_of(fault);
        let item = &frame[4..];
        if let Err(why) = cbor::check(item) {
            panic!("{fault:?} is not deterministic CBOR: {why}");
        }
        assert_eq!(Up::decode(item), Ok((0, Up::Fault { fault })));
        for cut in 0..item.len() {
            assert!(Up::decode(&item[..cut]).is_err(), "a fault cut to {cut} bytes was read");
        }
        if let Some(c) = callers {
            assert_eq!(c.rets().len(), usize::from(c.len));
            assert_eq!(c.spots().len(), usize::from(c.len).min(NAMED_CALLERS));
        }
        longest = longest.max(frame.len());
        shapes += 1;
    }
    // The child encodes the frame in a fixed room on its signal stack (blaze-launch-rt
    // fault_report.rs `FRAME_ROOM`, 4096): the longest frame, with registers and code too,
    // stays well inside it.
    assert!(longest < 3072, "the full walk's frame is {longest} bytes");
    std::println!("run wire: Fault with callers round-trips in {shapes} shapes; the full walk's frame is {longest} bytes");
}

/// A frame that says more spots than return addresses, one of 33 return addresses, and an end
/// the schema has not: each refused by name, never read short.
#[test]
fn bad_callers_are_refused() {
    // The `ret` list's head of 1 (`0x63 "ret" 0x81`) made 0: the one spot names no address.
    let mut more_spots = frame_of(fault(Some(callers(1, WalkEnd::Top))));
    let at = more_spots.windows(5).position(|w| w == [0x63, b'r', b'e', b't', 0x81]).expect("the ret list");
    more_spots[at + 4] = 0x80;
    assert!(
        matches!(Up::decode(&more_spots[4..]), Err(WireError::Range { what: "one spot per return address, up to 8", .. })),
        "a spot with no return address was read"
    );

    // The `ret` list's head of 32 (`0x98 0x20`) made 33.
    let mut long = frame_of(fault(Some(callers(MOST_CALLERS, WalkEnd::Cap))));
    let at = long.windows(6).position(|w| w == [0x63, b'r', b'e', b't', 0x98, 0x20]).expect("the ret list");
    long[at + 5] = 33;
    assert!(
        matches!(Up::decode(&long[4..]), Err(WireError::Range { what: "a fault carries at most 32 return addresses", .. })),
        "33 return addresses were read"
    );

    // `Top` made `Tip`.
    let mut other = frame_of(fault(Some(callers(1, WalkEnd::Top))));
    let at = other.windows(4).position(|w| w == [0x63, b'T', b'o', b'p']).expect("the end's name");
    other[at + 2] = b'i';
    assert!(matches!(Up::decode(&other[4..]), Err(WireError::Variant { .. })), "an unknown walk end was read");
}
