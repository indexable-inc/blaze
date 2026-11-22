//! The crash handler's frame-pointer walk (fault_report.rs `walk_frames`, `callers_from`) over a
//! PLANTED stack: words at made-up addresses, read by a reader that panics on any address
//! outside the stack it was given, so a wild read in the handler is a failed test here. Three
//! frames then a bad frame pointer: three return addresses, and the walk ends by name (out of
//! the stack, not above the last, off 16 bytes, a zero fp, the cap) without a read past the
//! stack. The return addresses are named by the placement rows (the byte before each, so a
//! call that is a function's last instruction is its own function's), and the frame with the
//! full walk fits the handler's room.

use blaze_code::record::Symbol as CodeSymbol;
use blaze_launch::wire::RecordId;
use blaze_launch::wire::fault::{Arm64, Around, Fault, MOST_CALLERS, NAMED_CALLERS, Placed, Regs, Signal, WalkEnd, AROUND};
use blaze_launch::wire::run::{Symbol, Up};
use blaze_launch::wire::split;

use super::fault_report::{Kind, Row, callers_from, fault_frame, return_spot, walk_frames};

/// The planted stack's lowest address (16-byte aligned, as a thread's stack is).
const BASE: u64 = 0x16f0_0000_0000;
/// Its words.
const WORDS: usize = 2 * (MOST_CALLERS + 4);
/// The code area the return addresses point into.
const CODE: u64 = 0x70_0200_0000;

/// A reader of `stack` planted at [`BASE`]: panics on an address outside it or off 8 bytes.
fn reader(stack: &[u64; WORDS]) -> impl FnMut(u64) -> Option<u64> + '_ {
    move |at| {
        assert!(at >= BASE && at < BASE + 8 * WORDS as u64, "the walk read {at:#x}, outside the planted stack");
        assert_eq!(at % 8, 0, "the walk read {at:#x}, off 8 bytes");
        Some(stack[((at - BASE) / 8) as usize])
    }
}

/// The stack with frame records at words 0, 4 and 8 (each `[next fp, return address]`), the
/// third's next fp `bad`.
fn three_frames(bad: u64) -> [u64; WORDS] {
    let mut stack = [0u64; WORDS];
    let at = |word: usize| BASE + 8 * word as u64;
    stack[0] = at(4);
    stack[1] = CODE + 0x154;
    stack[4] = at(8);
    stack[5] = CODE + 0x1000 + 0x1c0;
    stack[8] = bad;
    stack[9] = CODE + 0x2000 + 0x8;
    stack
}

const BOUNDS: Option<(u64, u64)> = Some((BASE, BASE + 8 * WORDS as u64));

/// Three frames, then each bad fp: three return addresses innermost first, the end named, no
/// read past the planted stack. Prints "fault walk: three frames; ends <end> for each bad fp".
#[test]
fn the_walk_takes_three_frames_and_ends_at_a_bad_fp_without_a_wild_read() {
    let below = BASE; // the first frame's own record: not above the last
    let ends = [
        (BASE + 8 * WORDS as u64 + 0x100, WalkEnd::OutOfStack),
        (0x10, WalkEnd::OutOfStack),
        (below, WalkEnd::NotAbove),
        (BASE + 8 * 4, WalkEnd::NotAbove),
        (BASE + 8 * 12 + 8, WalkEnd::Unaligned),
        (0, WalkEnd::Top),
        (u64::MAX - 7, WalkEnd::Unaligned),
        (u64::MAX & !15, WalkEnd::OutOfStack),
    ];
    for (bad, want) in ends {
        let stack = three_frames(bad);
        let (len, rets, end) = walk_frames(BASE, BOUNDS, &mut reader(&stack));
        assert_eq!((len, end), (3, want), "a third frame whose next fp is {bad:#x}");
        assert_eq!(&rets[..3], &[CODE + 0x154, CODE + 0x1000 + 0x1c0, CODE + 0x2000 + 0x8]);
        assert!(rets[3..].iter().all(|&r| r == 0), "nothing past the walk is said");
    }
    // A first fp outside the stack, unaligned, or zero: nothing read at all.
    let stack = three_frames(0);
    let mut never = |at: u64| -> Option<u64> { panic!("read {at:#x} though the first fp is bad") };
    assert_eq!(walk_frames(BASE - 16, BOUNDS, &mut never).2, WalkEnd::OutOfStack);
    assert_eq!(walk_frames(BASE + 8, BOUNDS, &mut never).2, WalkEnd::Unaligned);
    assert_eq!(walk_frames(0, BOUNDS, &mut never).2, WalkEnd::Top);
    assert_eq!(walk_frames(BASE, None, &mut never), (0, [0; MOST_CALLERS], WalkEnd::NoBounds));
    // A record whose second word is past the top: out of the stack, its first word unread.
    let top = BASE + 8 * WORDS as u64;
    assert_eq!(walk_frames((top - 8) & !15,Some((BASE, top - 8)), &mut reader(&stack)).2, WalkEnd::OutOfStack);
    // A word the reader cannot read: the walk ends there.
    let mut unread = |_at: u64| -> Option<u64> { None };
    assert_eq!(walk_frames(BASE, BOUNDS, &mut unread), (0, [0; MOST_CALLERS], WalkEnd::Unread));
    // A zero return address: the chain's end.
    let mut zero_ret = three_frames(0);
    zero_ret[5] = 0;
    assert_eq!(walk_frames(BASE, BOUNDS, &mut reader(&zero_ret)).0, 1);
    std::println!("fault walk: three frames; ends {:?} for each bad fp", ends.map(|(_, end)| end.name()));
}

/// A chain longer than the cap: 32 return addresses, ended as the cap, the 33rd record unread.
#[test]
fn the_walk_stops_at_the_cap() {
    let mut stack = [0u64; WORDS];
    for f in 0..MOST_CALLERS + 1 {
        let at = 2 * f;
        stack[at] = BASE + 8 * (at as u64 + 2);
        stack[at + 1] = CODE + 4 * (f as u64 + 1);
    }
    let mut reads = 0usize;
    let mut planted = reader(&stack);
    let mut counted = |at: u64| {
        reads += 1;
        planted(at)
    };
    let (len, rets, end) = walk_frames(BASE, BOUNDS, &mut counted);
    assert_eq!((usize::from(len), end), (MOST_CALLERS, WalkEnd::Cap));
    assert_eq!(rets[MOST_CALLERS - 1], CODE + 4 * MOST_CALLERS as u64);
    assert_eq!(reads, 2 * MOST_CALLERS, "the record past the cap is never read");
}

/// The return addresses named by the rows: each the code holding the byte BEFORE it, with the
/// return address's own offset; one at a function's first byte is its predecessor's (the call
/// was that one's last instruction); one in no row and one in a datum are not named. The full
/// walk with registers and code fits the handler's 4 KiB room and decodes as itself.
#[test]
fn the_callers_are_named_by_the_rows_and_fit_the_room() {
    let f = Row::new(Kind::Code, CODE, 0x1000, CodeSymbol([0xa1; 32]), RecordId([0x1a; 32]));
    let g = Row::new(Kind::Code, CODE + 0x1000, 0x1000, CodeSymbol([0xb2; 32]), RecordId([0x2b; 32]));
    let d = Row::new(Kind::ReadOnlyDatum, CODE + 0x3000, 0x100, CodeSymbol([0xc3; 32]), RecordId([0x3c; 32]));
    let rows = [f, g, d];
    let named = return_spot(CODE + 0x154, &rows).expect("in f");
    assert_eq!((named.symbol, named.offset, named.placed), (Symbol([0xa1; 32]), 0x154, Placed::Code));
    let last = return_spot(CODE + 0x1000, &rows).expect("f's last call");
    assert_eq!((last.symbol, last.offset), (Symbol([0xa1; 32]), 0x1000));
    assert_eq!(return_spot(CODE + 0x1000 + 0x1c0, &rows).map(|s| (s.symbol, s.offset)), Some((Symbol([0xb2; 32]), 0x1c0)));
    assert_eq!(return_spot(CODE + 0x3010, &rows), None, "a datum is no caller");
    assert_eq!(return_spot(CODE + 0x9000, &rows), None);
    assert_eq!(return_spot(0, &rows), None);

    let stack = three_frames(0);
    let callers = callers_from(walk_frames(BASE, BOUNDS, &mut reader(&stack)), &rows);
    assert_eq!(callers.spots().len(), 3);
    assert_eq!(callers.spots()[0].map(|s| s.symbol), Some(Symbol([0xa1; 32])));
    assert_eq!(callers.spots()[1].map(|s| s.symbol), Some(Symbol([0xb2; 32])));
    assert_eq!(callers.spots()[2], None, "CODE + 0x2008 is in no row");

    // The longest frame: every register word full width, 32 code bytes, 32 return addresses,
    // eight named.
    let mut full = [0u64; WORDS];
    for k in 0..MOST_CALLERS + 1 {
        full[2 * k] = BASE + 8 * (2 * k as u64 + 2);
        full[2 * k + 1] = CODE + 0x10 + k as u64;
    }
    let walked = callers_from(walk_frames(BASE, BOUNDS, &mut reader(&full)), &rows);
    assert_eq!(walked.spots().len(), NAMED_CALLERS);
    let at = walked.spots()[0];
    let regs = Regs::Arm64(Arm64 { x: [u64::MAX; 31], sp: u64::MAX, pc: u64::MAX, pstate: u64::MAX });
    let code = Around { start: u64::MAX, len: AROUND as u8, bytes: [0xff; AROUND] };
    let fault = Fault { signal: Signal::Trap, pc: Some(u64::MAX), at, code: Some(code), regs: Some(regs), address: u64::MAX, touched: at, write: Some(true), callers: Some(walked) };
    let room = blaze_own::Inline::<4096>::new();
    let mut frame = std::vec::Vec::with_capacity_in(4096, &room);
    fault_frame(&fault, &mut frame).expect("the longest frame fits the handler's room");
    let (item, _) = split(&frame).expect("a frame").expect("a whole frame");
    assert_eq!(Up::decode(item), Ok((0, Up::Fault { fault })));
    std::println!("fault walk: the longest frame is {} bytes", frame.len());
}
