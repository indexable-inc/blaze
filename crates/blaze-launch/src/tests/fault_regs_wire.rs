//! The run wire's `Up::Fault` with the thread's registers and the code around the pc (run
//! schema 2, wire/fault.rs): each architecture's form reads back as written, as deterministic
//! CBOR (blaze-protocol's `cbor::check`, the oracle); every cut is refused; a register list of
//! the wrong length, code past 32 bytes and an unknown architecture are refused by name.

use blaze_protocol::cbor;

use super::run_wire::{Message, encoded};
use crate::wire::fault::{AROUND, Arm64, Around, Fault, Placed, Regs, Signal, Spot, X86_64};
use crate::wire::run::{Symbol, Up};
use crate::wire::{RecordId, WireError, split};

fn arm64() -> Regs {
    let mut x = [0u64; 31];
    for (n, x) in x.iter_mut().enumerate() {
        // Small, middle and full-width words, so every head length of the encoding is met.
        *x = match n % 3 {
            0 => n as u64,
            1 => 0x1_0000 + n as u64,
            _ => u64::MAX - n as u64,
        };
    }
    Regs::Arm64(Arm64 { x, sp: 0x16f5_ff_e0a0, pc: 0x70_02ae_a76c, pstate: 0x6000_0000 })
}

fn x86_64() -> Regs {
    let mut gpr = [0u64; 16];
    for (n, g) in gpr.iter_mut().enumerate() {
        *g = (n as u64) << (4 * n);
    }
    Regs::X86_64(X86_64 { gpr, rip: 0x4010_00, rflags: 0x246 })
}

fn around(len: u8) -> Around {
    let mut bytes = [0u8; AROUND];
    for (i, b) in bytes.iter_mut().enumerate().take(usize::from(len)) {
        *b = 0x40 + i as u8;
    }
    Around { start: 0x70_02ae_a75c, len, bytes }
}

fn fault(code: Option<Around>, regs: Option<Regs>) -> Fault {
    let at = Spot { placed: Placed::Code, symbol: Symbol([0x66; 32]), record: RecordId([0xd9; 32]), offset: 0x3fc };
    Fault { signal: Signal::Trap, pc: Some(0x70_02ae_a76c), at: Some(at), code, regs, address: 0x70_02ae_a76c, touched: Some(at), write: None, callers: None }
}

/// The fault's frame (the file's one maker, `run_wire::encoded`): its item is `frame[4..]`.
fn frame_of(fault: Fault) -> Vec<u8> {
    let frame = encoded(&Message::Up(Up::Fault { fault }), 0);
    let (_, took) = split(&frame).expect("a frame").expect("a whole frame");
    assert_eq!(took, frame.len(), "one frame: {fault:?}");
    frame
}

/// Both architectures' registers, the code around the pc whole, clipped and empty, each with
/// and without the other: deterministic CBOR that reads back as itself; each cut refused; the
/// general register `n` and `sp` read back by `Regs::general` and `Regs::sp`. Prints "run
/// wire: Fault with registers round-trips in N shapes".
#[test]
fn a_fault_with_registers_round_trips() {
    let mut shapes = 0;
    for regs in [None, Some(arm64()), Some(x86_64())] {
        for code in [None, Some(around(32)), Some(around(20)), Some(around(0))] {
            let fault = fault(code, regs);
            let frame = frame_of(fault);
            let item = &frame[4..];
            if let Err(why) = cbor::check(item) {
                panic!("{fault:?} is not deterministic CBOR: {why}");
            }
            assert_eq!(Up::decode(item), Ok((0, Up::Fault { fault })));
            for cut in 0..item.len() {
                assert!(Up::decode(&item[..cut]).is_err(), "a fault cut to {cut} bytes was read");
            }
            shapes += 1;
        }
    }
    let Regs::Arm64(a) = arm64() else { unreachable!("arm64() is an AArch64 state") };
    assert_eq!(arm64().general(9), Some(a.x[9]));
    assert_eq!(arm64().general(30), Some(a.x[30]));
    assert_eq!(arm64().general(31), None, "x31 is no general register (it is sp or xzr)");
    assert_eq!(arm64().sp(), a.sp);
    assert_eq!(x86_64().general(15), Some(15u64 << 60));
    assert_eq!(x86_64().general(16), None);
    assert_eq!(around(20).bytes().len(), 20);
    std::println!("run wire: Fault with registers round-trips in {shapes} shapes");
}

/// An AArch64 state whose `x` list is one short, a code stretch of 33 bytes, and an
/// architecture the schema has not: each refused, by name, never read short.
#[test]
fn a_bad_register_list_is_refused() {
    // The `x` list's head: an array of 31 (`0x98 0x1f`): made 30, the list is one short.
    let mut short = frame_of(fault(None, Some(arm64())));
    let at = short.windows(3).position(|w| w == [0x61, b'x', 0x98]).expect("the frame has the x list");
    assert_eq!(short[at + 3], 31);
    short[at + 3] = 30;
    assert!(matches!(Up::decode(&short[4..]), Err(WireError::Range { what: "x0..x30 are 31 words", .. })), "a list of 30 registers was read");

    let mut other = frame_of(fault(None, Some(arm64())));
    let at = other.windows(5).position(|w| w == *b"Arm64").expect("the frame names the architecture");
    other[at + 4] = b'5';
    assert!(matches!(Up::decode(&other[4..]), Err(WireError::Variant { .. })), "an unknown architecture was read");

    // The code's head (a byte string of 32, `0x58 0x20`, then its first bytes 0x40 0x41) made
    // 33: the string takes one byte of what follows, and is refused as past what a fault
    // carries before anything after it is read.
    let mut long = frame_of(fault(Some(around(32)), None));
    let at = long.windows(4).position(|w| w == [0x58, 32, 0x40, 0x41]).expect("the code's head");
    long[at + 1] = 33;
    assert!(
        matches!(Up::decode(&long[4..]), Err(WireError::Range { what: "the code around the pc is at most 32 bytes", .. })),
        "33 bytes of code were read"
    );
}
