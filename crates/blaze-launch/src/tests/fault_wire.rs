//! The run wire's `Up::Fault` (wire/fault.rs): deterministic CBOR (blaze-protocol's
//! `cbor::check`, the oracle), read back as written with every optional part present and
//! absent, every cut refused, an unknown signal refused by name.

use blaze_protocol::cbor;

use super::run_wire::{Message, encoded};
use crate::wire::fault::{Fault, Placed, Signal, Spot};
use crate::wire::run::{Down, Symbol, Up};
use crate::wire::{RecordId, WireError, split};

fn spot(placed: Placed, byte: u8, offset: u64) -> Spot {
    Spot { placed, symbol: Symbol([byte; 32]), record: RecordId([byte ^ 0xff; 32]), offset }
}

/// Each signal and placement, with the pc, the spots and the access each there and not: one
/// frame of deterministic CBOR that reads back as itself (and never as a down message); each
/// cut refused. Prints "run wire: Fault round-trips in N shapes; cuts and an unknown signal
/// refused".
#[test]
fn a_fault_round_trips_and_a_cut_is_refused() {
    let signals = [Signal::Segv, Signal::Bus, Signal::Ill, Signal::Trap];
    let placed = [Placed::Code, Placed::Relro, Placed::ReadOnlyDatum, Placed::WritableDatum];
    let mut shapes = 0;
    for (i, &signal) in signals.iter().enumerate() {
        for (j, &kind) in placed.iter().enumerate() {
            for with in [false, true] {
                let fault = Fault {
                    signal,
                    pc: with.then_some(0x70_02ae_a76c),
                    at: with.then_some(spot(Placed::Code, 0x66, 0x3fc)),
                    code: None,
                    regs: None,
                    address: 0x70_0000_6048 + j as u64,
                    touched: (with || i % 2 == 0).then_some(spot(kind, 0x51, 8)),
                    write: if with { Some(i % 2 == 1) } else { None },
                    callers: None,
                };
                let up = Up::Fault { fault };
                let frame = encoded(&Message::Up(up), 0);
                let (item, took) = split(&frame).expect("a frame").expect("a whole frame");
                assert_eq!(took, frame.len(), "one frame: {fault:?}");
                if let Err(why) = cbor::check(item) {
                    panic!("{fault:?} is not deterministic CBOR: {why}");
                }
                assert_eq!(Up::decode(item), Ok((0, up)));
                assert!(Down::decode(item).is_err(), "a fault read as a down message");
                for cut in 0..item.len() {
                    assert!(Up::decode(&item[..cut]).is_err(), "a fault cut to {cut} bytes was read");
                }
                shapes += 1;
            }
        }
    }
    // The signal's text changed to one of the same length the schema has not: refused.
    let fault = Fault { signal: Signal::Trap, pc: None, at: None, code: None, regs: None, address: 0, touched: None, write: None, callers: None };
    let frame = encoded(&Message::Up(Up::Fault { fault }), 0);
    let (item, _) = split(&frame).expect("a frame").expect("a whole frame");
    let mut bad = item.to_vec();
    let at = bad.windows(7).position(|w| w == b"SIGTRAP").expect("the frame names the signal");
    bad[at + 6] = b'X';
    assert!(matches!(Up::decode(&bad), Err(WireError::Variant { .. })), "an unknown signal was read");
    std::println!("run wire: Fault round-trips in {shapes} shapes; cuts and an unknown signal refused");
}
