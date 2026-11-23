//! THE ONE HOME of the trap codes blaze's generated code carries (rule 30), and what each means
//! when a run dies on one (rule 19: a crash names the thing).
//!
//! A trap in a code record is an AArch64 `brk #imm16`: LIR's `trap code` is its own immediate
//! (blaze-emit pass.rs refuses a LIR code at or past [`DIV_ZERO`]), and the emitter's own traps
//! (`unreachable`, a division's checks) take the codes from [`DIV_ZERO`] up. So the reason a
//! program stopped on a trap is read from the instruction it stopped on. WHICH construct an
//! `unreachable` ends is not in the instruction: the record's trap table holds it (record.rs
//! `TrapRow`, data only, the key unchanged by it). What a code means is [`Reason`]; what a trap
//! at an offset of a record is, with the call it follows when it ends a call to a fn that never
//! returns and its trap row, is [`at`].
//!
//! Every producer names its code from here: blaze-emit's `BRK_*`, blaze-build's
//! `ABORT_TRAP`, `NO_MEMORY_TRAP`, `SET_FAILED_TRAP` and `BAD_ROW_TRAP`. blaze-lir's coroutine
//! codes (`RESUMED_TRAP`, `TAG_TRAP`) are held equal to [`ASYNC_RESUMED`] and [`NO_STATE`] by
//! blaze-emit's trap_codes_tests.rs (blaze-lir depends on no crate that could hold them).

use crate::record::{CodeView, SiteKind, Space, Symbol, TrapRow};

/// `core::intrinsics::abort` (and `abort_immediate`), and a zero-sized `offset_from`.
pub const ABORT: u32 = 0xfe00;
/// A thread's thread-local table or copy could not be allocated (blaze-build tls.rs).
pub const NO_MEMORY: u32 = 0xfe01;
/// `pthread_setspecific` failed in the thread-local getter (blaze-build tls/index.rs).
pub const SET_FAILED: u32 = 0xfe02;
/// A thread-local descriptor's row whose initial bytes exceed its size (blaze-build
/// tls/index.rs).
pub const BAD_ROW: u32 = 0xfe03;
/// `pthread_key_create` failed during thread-local startup (blaze-export start.rs).
pub const KEY_CREATE_FAILED: u32 = 0xfe04;
/// A coroutine's `poll` of a state that returned or panicked (blaze-lir coroutine.rs
/// `RESUMED_TRAP`).
pub const ASYNC_RESUMED: u32 = 0xfe10;
/// A coroutine's tag that names no state (blaze-lir coroutine.rs `TAG_TRAP`).
pub const NO_STATE: u32 = 0xfe11;
/// A division or remainder by zero (the emitter's check).
pub const DIV_ZERO: u32 = 0xff00;
/// A signed division of the least value by -1 (the emitter's check).
pub const DIV_OVERFLOW: u32 = 0xff01;
/// LIR's `unreachable`: a point the compiler was told is never reached.
pub const UNREACHABLE: u32 = 0xff02;

/// What a trap code means.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reason {
    Abort,
    NoMemory,
    SetFailed,
    BadRow,
    KeyCreateFailed,
    AsyncResumed,
    NoState,
    DivZero,
    DivOverflow,
    Unreachable,
    /// A code no producer of this tree names: a LIR `trap` written with a code of its own.
    Other(u32),
}

impl Reason {
    pub fn of(code: u32) -> Reason {
        match code {
            ABORT => Reason::Abort,
            NO_MEMORY => Reason::NoMemory,
            SET_FAILED => Reason::SetFailed,
            BAD_ROW => Reason::BadRow,
            KEY_CREATE_FAILED => Reason::KeyCreateFailed,
            ASYNC_RESUMED => Reason::AsyncResumed,
            NO_STATE => Reason::NoState,
            DIV_ZERO => Reason::DivZero,
            DIV_OVERFLOW => Reason::DivOverflow,
            UNREACHABLE => Reason::Unreachable,
            other => Reason::Other(other),
        }
    }

    /// The code itself.
    pub fn code(self) -> u32 {
        match self {
            Reason::Abort => ABORT,
            Reason::NoMemory => NO_MEMORY,
            Reason::SetFailed => SET_FAILED,
            Reason::BadRow => BAD_ROW,
            Reason::KeyCreateFailed => KEY_CREATE_FAILED,
            Reason::AsyncResumed => ASYNC_RESUMED,
            Reason::NoState => NO_STATE,
            Reason::DivZero => DIV_ZERO,
            Reason::DivOverflow => DIV_OVERFLOW,
            Reason::Unreachable => UNREACHABLE,
            Reason::Other(code) => code,
        }
    }

    /// What it says in a report.
    pub fn says(self) -> &'static str {
        match self {
            Reason::Abort => "an abort (`core::intrinsics::abort`: a panic with no unwinder, a `#[panic_handler]` that aborts, or `std::process::abort`)",
            Reason::NoMemory => "a thread's thread-local table could not be allocated",
            Reason::SetFailed => "`pthread_setspecific` failed in the thread-local getter",
            Reason::BadRow => "a thread-local descriptor row is malformed (its initial bytes exceed its size)",
            Reason::KeyCreateFailed => "`pthread_key_create` failed during thread-local startup",
            Reason::AsyncResumed => "a coroutine (`async fn`) resumed after it completed or panicked",
            Reason::NoState => "a coroutine's state tag names no state",
            Reason::DivZero => "a division or remainder by zero",
            Reason::DivOverflow => "a signed division overflowed (the least value by -1)",
            Reason::Unreachable => "an `unreachable` reached (code the compiler holds is never run)",
            Reason::Other(_) => "a `trap` with a code no producer of blaze names",
        }
    }
}

/// The immediate of `brk #imm16`, when `word` is one.
pub fn brk(word: u32) -> Option<u32> {
    (word & 0xffe0_001f == 0xd420_0000).then_some((word >> 5) & 0xffff)
}

/// Whether `word` is `blr xN`.
pub fn is_blr(word: u32) -> bool {
    word & 0xffff_fc1f == 0xd63f_0000
}

/// A trap found at an offset of a record's code.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TrapAt {
    pub reason: Reason,
    /// The fn whose call ends just before the trap: the call returned, and the code after it
    /// is a trap (the callee's type says it never returns, or the call is an abort's). `None`
    /// when the instruction before the trap is not one of the record's call sites.
    pub after_call: Option<Symbol>,
    /// The record's trap row for the `brk` (WHICH construct it ends), when it has one.
    pub construct: Option<TrapRow>,
}

/// What the instruction at `offset` of `view`'s code is, when it is a trap: `None` when the
/// offset is not inside the code, not 4-aligned, or not a `brk`.
pub fn at(view: &CodeView<'_>, offset: u64) -> Option<TrapAt> {
    let code = view.code();
    let at = usize::try_from(offset).ok()?;
    if at % 4 != 0 {
        return None;
    }
    let word = u32::from_le_bytes(*code.get(at..)?.first_chunk::<4>()?);
    let reason = Reason::of(brk(word)?);
    // A call site is `adrp`, `ldr`, `blr x16`: it ends 12 bytes after its start.
    let mut after_call = None;
    for i in 0..view.sites() {
        let site = view.site(i);
        if site.kind == SiteKind::Call && u64::from(site.at) + u64::from(SiteKind::Call.len()) == offset {
            let target = view.target(site.target as usize);
            if target.space == Space::Func || target.space == Space::Outside {
                after_call = Some(target.symbol);
            }
            break;
        }
    }
    // `at` is inside the code, whose length a u32 counts (record.rs `seal`).
    let construct = u32::try_from(at).ok().and_then(|at| view.trap_at(at));
    Some(TrapAt { reason, after_call, construct })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `brk #imm` and `blr xN` are told from other words; each named code reads back as itself
    /// and an unnamed one as `Other`.
    #[test]
    fn trap_codes_read_back_from_their_instructions() {
        for code in [ABORT, NO_MEMORY, SET_FAILED, BAD_ROW, KEY_CREATE_FAILED, ASYNC_RESUMED, NO_STATE, DIV_ZERO, DIV_OVERFLOW, UNREACHABLE, 7] {
            let word = 0xd420_0000 | (code << 5);
            assert_eq!(brk(word), Some(code), "brk #{code:#x}");
            assert_eq!(Reason::of(code).code(), code);
        }
        assert_eq!(Reason::of(7), Reason::Other(7));
        assert_eq!(Reason::of(UNREACHABLE), Reason::Unreachable);
        // `ret` and `nop` are no traps.
        assert_eq!(brk(0xd65f_03c0), None);
        assert_eq!(brk(0xd503_201f), None);
        assert!(is_blr(0xd63f_0200), "blr x16");
        assert!(!is_blr(0xd65f_03c0), "ret is no call");
    }

    /// The shape the coordinator's std-hello run stopped on (2026-11-17 21:42): a call of a fn
    /// that never returns, then the `unreachable` after it. A trap right after a call site
    /// names the callee; a trap elsewhere names none; a word that is no `brk` is no trap.
    #[test]
    fn a_trap_after_a_call_names_the_call_it_ends() {
        use crate::record::{CodeRecord, Site, Target};
        use blaze_own::{Erased, Inline, Vec};

        let inline = Inline::<16384>::new();
        let erased = Erased::new(&inline);
        let alloc = erased.handle();
        let callee = Symbol([0x5a; 32]);
        let mut record = CodeRecord::new_in(alloc);
        // adrp x16; ldr x16, [x16]; blr x16; brk #0xff02; nop; brk #0xfe00
        for word in [0x9000_0010u32, 0xf940_0210, 0xd63f_0200, 0xd420_0000 | (UNREACHABLE << 5), 0xd503_201f, 0xd420_0000 | (ABORT << 5)] {
            record.code.extend_from_slice(&word.to_le_bytes());
        }
        let target = record.target(Target { space: Space::Func, symbol: callee });
        record.sites.push(Site { at: 0, kind: SiteKind::Call, target, addend: 0 });
        let sealed = record.seal().expect("the record seals");
        let mut bytes = Vec::new_in(alloc);
        sealed.encode(&mut bytes);
        let view = CodeView::read(&bytes).expect("the record reads back");
        assert_eq!(at(&view, 12), Some(TrapAt { reason: Reason::Unreachable, after_call: Some(callee), construct: None }));
        assert_eq!(at(&view, 20), Some(TrapAt { reason: Reason::Abort, after_call: None, construct: None }));
        assert_eq!(at(&view, 16), None, "a nop is no trap");
        assert_eq!(at(&view, 14), None, "an offset inside an instruction");
        assert_eq!(at(&view, 24), None, "past the code");
    }
}
