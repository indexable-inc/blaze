//! What the crash handler tells the parent (fault_report.rs `fault_of`, `fault_frame`): the
//! place the pc and the address are in, as rows of the run's placements, encoded on a stack
//! buffer the size the handler has, and read back by the parent's decoder as the same fault.

use blaze_code::record::Symbol as CodeSymbol;
use blaze_launch::wire::fault::{Fault, Placed, Signal, Spot};
use blaze_launch::wire::run::{Symbol, Up};
use blaze_launch::wire::{RecordId, split};
use blaze_sys::posix::signal::{SIGABRT, SIGSEGV, SIGTRAP};

use super::fault_report::{Kind, Row, Struck, fault_frame, fault_of};

const AREA: u64 = 0x70_0000_6000;

/// The coordinator's stop of 21:42 in its shape: a SIGTRAP whose pc is +0x3fc in a function's
/// code. The pc's spot names the function, its record and the offset; an address in no row has
/// no spot; a signal the handler does not report gives no fault. The frame fits the handler's
/// room and decodes as the same fault. Prints the decoded fault.
#[test]
fn a_trap_is_told_to_the_parent_as_the_place_it_struck() {
    let f = Row::new(Kind::Code, AREA + 0x100, 0x800, CodeSymbol([0x66; 32]), RecordId([0x2d; 32]));
    let s = Row::new(Kind::WritableDatum, AREA + 0x40, 0x10, CodeSymbol([0x51; 32]), RecordId([0x77; 32]));
    let pc = AREA + 0x100 + 0x3fc;
    let struck = Struck { sig: SIGTRAP, pc: Some(pc), address: pc, write: None };
    let fault = fault_of(struck, &[s, f]).expect("a SIGTRAP is reported");
    let at = Spot { placed: Placed::Code, symbol: Symbol([0x66; 32]), record: RecordId([0x2d; 32]), offset: 0x3fc };
    assert_eq!(fault, Fault { signal: Signal::Trap, pc: Some(pc), at: Some(at), code: None, regs: None, address: pc, touched: Some(at), write: None, callers: None });

    let segv = fault_of(Struck { sig: SIGSEGV, pc: Some(pc), address: AREA + 0x48, write: Some(true) }, &[s, f]).expect("a SIGSEGV is reported");
    assert_eq!(segv.touched.map(|spot| (spot.placed, spot.offset)), Some((Placed::WritableDatum, 8)));
    assert_eq!(fault_of(Struck { sig: SIGSEGV, pc: None, address: 0x10, write: None }, &[s, f]).map(|it| (it.at, it.touched)), Some((None, None)));
    assert_eq!(fault_of(Struck { sig: SIGABRT, pc: Some(pc), address: 0, write: None }, &[f]), None, "SIGABRT is not the handler's");

    let room = blaze_own::Inline::<1024>::new();
    let mut frame = std::vec::Vec::with_capacity_in(1024, &room);
    fault_frame(&fault, &mut frame).expect("the frame fits the handler's room");
    let (item, took) = split(&frame).expect("a frame").expect("a whole frame");
    assert_eq!(took, frame.len());
    assert_eq!(Up::decode(item), Ok((0, Up::Fault { fault })));
    std::println!("{fault:?}");
}
