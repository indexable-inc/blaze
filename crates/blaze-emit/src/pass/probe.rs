//! Stack probes (THREADS.md D3): a frame larger than a guard page must not step over it.
//!
//! The rule, from the pinned library (std/src/sys/pal/unix/stack_overflow/handler_signal.rs,
//! 19-20): "We've got guard pages (unmapped pages) at the end of every thread's stack, so if a
//! thread ends up running into the guard page it'll trigger"; and 109: "Even for
//! panic=immediate-abort, installing the guard pages is important for soundness." A function
//! whose `sp` drops by more than a guard before it writes below can land its first store in
//! whatever mapping lies under the guard: no Rust program may do that.
//!
//! The bound is [`PROBE_STEP`], 4096 bytes: the smallest page among the hosts (Linux arm64
//! with 4 KiB pages; macOS's is 16 KiB), so never wider than any guard.
//!
//! # How far `sp` drops
//!
//! The prologue's `stp fp, lr, [sp, #-16]!` writes at `entry - 16`; the two words at
//! `frame_at` then take `F` (the frame) off `sp`; a function whose slots ask more than 16 bytes
//! of alignment (`most_align`) then rounds `sp` down, which drops it by up to `most_align - 16`
//! more (`entry` and `F` are multiples of 16). So the drop below the `stp`'s write is at most
//! `D = F + (most_align - 16)`, and a function with `16 + D <= PROBE_STEP` pays nothing: its two
//! `sub`s stay, no word is added anywhere. (The first version counted `F` alone: a 4080-byte
//! frame with a 1 KiB-aligned slot then dropped up to 5088 bytes untouched.)
//!
//! # Where the probe code lives
//!
//! The frame's size is final when the last instruction is lowered: it depends on the staging
//! words and the `byval` copies, which grow while lowering, and on nothing placed after the
//! code. So [`Pass::place_frame`] runs first in `finish`, before any literal word or data row:
//! a larger frame's two words become `b tail; nop`, and the tail is placed right after the
//! function's last instruction. Every instruction then lies inside the instructions the record
//! form counts (`form.insts`, set by `finish_record` after this; record_check.rs
//! `no_address_word` and record_tests.rs `holds` check the first `insts` bytes and refuse a
//! byte after them that is not a data row). The first version placed the tail in `settle`,
//! after the data, and broke that invariant.
//!
//! ```text
//! tail: movz x17, #(F & 0xffff)
//!       movk x17, #(F >> 16), lsl #16
//! loop: sub  sp, sp, #1, lsl #12      // one step down          } only when F >= 4096: the
//!       str  xzr, [sp]                // touch it: a guard faults } loop runs at least once
//!       sub  x17, x17, #1, lsl #12    //                           } and must not take x17
//!       cmp  x17, #1, lsl #12         //                           } below zero
//!       b.hs loop                     // another whole step left   }
//!       sub  sp, sp, x17, uxtx        // the rest, under one step
//!       str  xzr, [sp]                // only when most_align > 16: the rounding may drop more
//!       b    frame_at + 8
//! ```
//!
//! Untouched stretches: between the `stp` and the first touch, 4096 - 8 bytes; between two
//! touches, 4096 - 8; below the last loop touch, `x17 < 4096` bytes. With `most_align > 16`
//! the closing `str` touches the frame's bottom before the rounding, so the stretch under it is
//! the rounding's drop, at most `most_align - 16 <= 4080` (`MOST_SLOT_ALIGN` is 4 KiB). The loop
//! runs while `x17 >= 4096` (`b.hs`). THREADS.md D3 writes `b.hi` (`x17 > 4096`); that leaves a
//! whole 4096 bytes untouched below the last touch when `F` is a multiple of 4096, which at a
//! page-aligned last touch is exactly a 4 KiB guard, stepped over by the next write: `b.hs` is
//! the smallest change that keeps every stretch under a page (src/probe_tests.rs,
//! `a_1_mib_frame_touches_each_4096_bytes`, runs the tail on an 8 KiB frame and fails on `b.hi`).
//!
//! `x16`/`x17` are free at the prologue (AAPCS64 6.1.1: intra-procedure-call scratch, and no
//! parameter lives there); `lr` is already saved; the frame's address rows are `fp`-based from
//! `add fp, sp, #0` on, which runs before `frame_at`, so the tail needs no unwind row of its
//! own, and it makes no call, so no call-site row covers it. The tail is placed before every
//! patch is resolved (`settle`) and before any literal or data row, so no patch, literal or data
//! offset moves.

use core::alloc::Allocator;

use super::{FRAME_LIMIT, Pass};
use crate::{
    EmitError,
    enc::{self, Cond, Field, SP, Size, ZR},
    enc_probe::{self, X17},
};

/// The step the probes walk down by, and the most `sp` may drop below the prologue's saved
/// `fp`/`lr` (those 16 bytes included) and pay nothing: the smallest page among the hosts. The
/// Fast tier (compose/emit.rs, `prologue`) probes a frame past it by this same bound, with this
/// file's sequence as the table's `glue::PROBE` template (table_gen/arm64_glue.rs).
pub(crate) const PROBE_STEP: u32 = 4096;

impl<A: Allocator> Pass<'_, '_, A> {
    /// Where the `byval` copies' stretch starts from `sp`: after the staging words, at 16.
    /// Final once the last instruction is lowered.
    pub(super) fn byval_at(&self) -> u64 {
        (u64::from(self.staging_at) + 8 * u64::from(self.staging)).next_multiple_of(16)
    }

    /// The frame's size: the cells, slots, staging words and `byval` copies, rounded to 16,
    /// plus the room the rounding of `sp` to `most_align` takes. Final once the last
    /// instruction is lowered; refused past 16 MiB.
    pub(super) fn frame_bytes(&self, most_align: u64) -> Result<u32, EmitError> {
        let too_large = EmitError::TooLarge { what: "the frame (16 MiB)" };
        let used = self.byval_at() + u64::from(self.byval_most);
        if used >= FRAME_LIMIT {
            return Err(too_large);
        }
        let frame = used.next_multiple_of(16) + (most_align - 16);
        if frame >= FRAME_LIMIT {
            return Err(too_large);
        }
        Ok(frame as u32)
    }

    /// Writes the frame's adjustment at `frame_at` (the two words the prologue reserved after
    /// `add fp, sp, #0`): the two `sub`s when `sp`'s whole drop stays within [`PROBE_STEP`],
    /// else `b tail; nop` and the probing tail placed right after the last instruction (module
    /// doc). Called once, first in `finish`, after the last instruction is lowered and before
    /// any literal word or data row is placed.
    pub(super) fn place_frame(&mut self, frame_at: u32, most_align: u64) -> Result<(), EmitError> {
        let frame = self.frame_bytes(most_align)?;
        // `frame < 16 MiB` and `most_align <= 4 KiB`: no sum below wraps.
        let drop = u64::from(frame) + (most_align - 16);
        if 16 + drop <= u64::from(PROBE_STEP) {
            self.set(frame_at, enc::sub_imm(SP, SP, frame >> 12, true));
            self.set(frame_at + 4, enc::sub_imm(SP, SP, frame & 0xfff, false));
            return Ok(());
        }
        let too_far = EmitError::TooLarge { what: "the stack-probe tail past a `b`'s reach (128 MiB)" };
        if self.code.len() % 4 != 0 {
            return Err(EmitError::Record { why: "the instructions do not end on a whole word before the stack-probe tail" });
        }
        let tail = self.here();
        self.put(enc::movz(X17, frame & 0xffff, 0));
        self.put(enc::movk(X17, frame >> 16, 1));
        let step = PROBE_STEP >> 12;
        if frame >= PROBE_STEP {
            let probe_loop = self.here();
            self.put(enc::sub_imm(SP, SP, step, true));
            self.put(enc::str(Size::X, ZR, SP, 0));
            self.put(enc::sub_imm(X17, X17, step, true));
            self.put(enc_probe::cmp_imm_lsl12(X17, step));
            let back = Field::Imm19
                .patch(enc::b_cond(Cond::Hs, 0), i64::from(probe_loop) - i64::from(self.here()))
                .ok_or(too_far)?;
            self.put(back);
        }
        self.put(enc_probe::sub_ext_uxtx(SP, SP, X17));
        if most_align > 16 {
            self.put(enc::str(Size::X, ZR, SP, 0));
        }
        let resume = Field::Imm26.patch(enc::b(0), i64::from(frame_at + 8) - i64::from(self.here())).ok_or(too_far)?;
        self.put(resume);
        let enter = Field::Imm26.patch(enc::b(0), i64::from(tail) - i64::from(frame_at)).ok_or(too_far)?;
        self.set(frame_at, enter);
        self.set(frame_at + 4, enc_probe::NOP);
        Ok(())
    }
}
