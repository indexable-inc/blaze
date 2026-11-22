//! `tls_addr` by index (port27/tempo-run THREADS.md, D2; the forms are blaze-build's
//! `tls/index.rs`): the thread's table found in line from the run-wide descriptor, the copy of
//! thread-local `place` read from the table's slot at its index, and the getter called only when
//! the thread has no table, a table too short for the index, or no copy yet.
//!
//! ```text
//! ldr  x9, =DESC                 ; the descriptor
//! ldr  x10, [x9, #8]             ; root
//! and  x10, x10, #~(1 << 63)     ; the flag cleared
//! mrs  x11, tpidrro_el0          ; macOS: the specific-data slots,
//! and  x11, x11, #~7             ;   root = key * 8 (rule 45, as pass/tls.rs)
//! mrs  x11, tpidr_el0            ; Linux: the thread pointer, root = &ROOT - tpidr_el0
//! add  x11, x11, x10             ; the root's address
//! ldr  x12, [x11]                ; the table
//! ldr  x13, =INDEX
//! cbz  x12, slow
//! ldr  x14, [x12]                ; len
//! cmp  x13, x14
//! b.hs slow
//! add  x14, x12, #32             ; the slots
//! ldr  x0, [x14, x13, lsl #3]
//! cbnz x0, done
//! slow:
//! mov  x0, x9; mov x1, x13; mov x2, x11
//! ldr  x16, =GETTER; blr x16
//! done:
//! ```
//!
//! The slow path follows the fast one in line, skipped by the taken `cbnz`: the pass is one
//! forward walk with no out-of-line stretch to put cold code in, so D2's `cbz x0, slow` ... `b
//! done` becomes `cbnz x0, done` with the slow path after it: the same 13 instructions on the
//! fast path (macOS), no extra branch. Every value lives in its cell, so the getter's call may
//! take every caller-saved register (as pass/tls.rs's call does).

use alloc::alloc::Allocator;

use blaze_lir::{Op, func::RefKind};

use super::{EmitError, Pass, unsupported};
use crate::{
    Os, RelocTo,
    enc::{self, Cond, Size, X0, X1, X2, X9, X10, X11, X12, X13, X14, X16},
    enc_tls,
};

/// The descriptor's `root`, in 8-byte units for `ldr`'s scaled offset (the forms' one home is
/// blaze-code's `tls`; blaze-build's `tls::index_tests` runs this sequence against its getter).
pub(crate) const DESC_ROOT_WORDS: u32 = blaze_code::tls::DESC_ROOT as u32 / 8;
/// The table's `len`, in 8-byte units.
pub(crate) const TABLE_LEN_WORDS: u32 = blaze_code::tls::TABLE_LEN as u32 / 8;
/// The table's first slot, in bytes.
pub(crate) const TABLE_SLOTS: u32 = blaze_code::tls::TABLE_SLOTS as u32;

impl<A: Allocator> Pass<'_, '_, A> {
    pub(super) fn tls_addr_indexed(&mut self, inst: usize, op: Op) -> Result<(), EmitError> {
        self.result_width(inst, op)?;
        let rec = self.view.reference(self.view.operands(inst).reference());
        if rec.kind != RefKind::Tls as u32 {
            return Err(unsupported(inst, op, "`tls_addr` of other than a thread-local"));
        }
        self.ld_word(X9, RelocTo::TlsDesc);
        self.put(enc::ldr(Size::X, X10, X9, DESC_ROOT_WORDS));
        self.put(enc_tls::and_clear_bit63(X10, X10));
        match self.isa.os {
            Os::MacOs => {
                // macOS private ABI (rule 45): libpthread's `_pthread_getspecific_direct` reads
                // the thread's specific-data slots at `tpidrro_el0 & ~7`, and a key is its
                // slot's index (the root is `key * 8`). Pinned by the test
                // `the_direct_tsd_read_equals_pthread_getspecific`.
                self.put(enc::mrs_tpidrro_el0(X11));
                // `and x11, x11, #~7`: 61 ones rotated right by 61.
                self.put(enc::and_imm(X11, X11, 1, 61, 60));
            }
            // The launcher is the executable, so its local-exec `ROOT` is at one offset from
            // `tpidr_el0` on every thread: the descriptor's root.
            Os::Linux => self.put(enc_tls::mrs_tpidr_el0(X11)),
        }
        self.put(enc::add(X11, X11, X10));
        self.put(enc::ldr(Size::X, X12, X11, 0));
        self.ld_word(X13, RelocTo::TlsIndex(rec.place));
        let no_table = self.here();
        self.put(enc::cbz(X12, 0));
        self.put(enc::ldr(Size::X, X14, X12, TABLE_LEN_WORDS));
        self.put(enc::cmp(X13, X14));
        let too_short = self.here();
        self.put(enc::b_cond(Cond::Hs, 0));
        self.put(enc::add_imm(X14, X12, TABLE_SLOTS, false));
        self.put(enc::ldr_lsl3(X0, X14, X13));
        let found = self.here();
        self.put(enc::cbnz(X0, 0));
        // slow: the getter makes the table, grows it or makes the copy.
        self.land(no_table)?;
        self.land(too_short)?;
        self.put(enc::mov(X0, X9));
        self.put(enc::mov(X1, X13));
        self.put(enc::mov(X2, X11));
        self.ld_word(X16, RelocTo::TlsGetter);
        self.put(enc::blr(X16));
        self.land(found)?;
        self.st(X0, inst);
        Ok(())
    }
}
