//! The trap codes' one home is blaze-code trap.rs: what a run that stops on a `brk` says is read
//! from there. blaze-lir names its coroutine codes itself (it depends on no crate that could
//! hold them); this holds them equal, and the emitter's own codes are trap.rs's by definition.

use blaze_code::trap::{self, Reason};

/// Each producer's code is the one trap.rs names, and reads back as its reason; no two reasons
/// share a code. Prints "trap codes: one home, 9 codes".
#[test]
fn every_trap_code_is_the_one_homes() {
    assert_eq!(blaze_lir::coroutine::RESUMED_TRAP, trap::ASYNC_RESUMED, "blaze-lir coroutine.rs RESUMED_TRAP");
    assert_eq!(blaze_lir::coroutine::TAG_TRAP, trap::NO_STATE, "blaze-lir coroutine.rs TAG_TRAP");
    assert_eq!(crate::pass::BRK_DIV_ZERO, trap::DIV_ZERO);
    assert_eq!(crate::pass::BRK_DIV_OVERFLOW, trap::DIV_OVERFLOW);
    assert_eq!(crate::pass::BRK_UNREACHABLE, trap::UNREACHABLE);
    let codes = [trap::ABORT, trap::NO_MEMORY, trap::SET_FAILED, trap::BAD_ROW, trap::ASYNC_RESUMED, trap::NO_STATE, trap::DIV_ZERO, trap::DIV_OVERFLOW, trap::UNREACHABLE];
    for (i, &a) in codes.iter().enumerate() {
        assert!(!matches!(Reason::of(a), Reason::Other(_)), "code {a:#x} has no reason");
        assert!(a <= 0xffff, "code {a:#x} is past a `brk` immediate");
        for &b in &codes[i + 1..] {
            assert_ne!(a, b, "two reasons share code {a:#x}");
        }
    }
    // A LIR code is refused from the emitter's first own code up, so the LIR codes sit below.
    for lir in [trap::ABORT, trap::NO_MEMORY, trap::SET_FAILED, trap::BAD_ROW, trap::ASYNC_RESUMED, trap::NO_STATE] {
        assert!(lir < crate::pass::BRK_DIV_ZERO, "LIR code {lir:#x} would be refused by the emitter");
    }
    std::println!("trap codes: one home, {} codes", codes.len());
}
