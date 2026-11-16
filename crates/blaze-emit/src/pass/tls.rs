//! `tls_addr` (lib.rs, "Thread-locals").

use std::alloc::Allocator;

use blaze_lir::{Op, func::RefKind};

use super::{EmitError, Pass, unsupported};
use crate::{
    Os, RelocTo,
    enc::{self, X0, X9, X10, X11, X16},
};

impl<A: Allocator> Pass<'_, '_, A> {
    pub(super) fn tls_addr(&mut self, inst: usize, op: Op) -> Result<(), EmitError> {
        self.result_width(inst, op)?;
        let rec = self.view.reference(self.view.operands(inst).reference());
        if rec.kind != RefKind::Tls as u32 {
            return Err(unsupported(inst, op, "`tls_addr` of other than a thread-local"));
        }
        match self.isa.os {
            Os::MacOs => {
                // macOS private ABI (rule 45): libpthread's `_pthread_getspecific_direct` reads
                // the thread's specific-data slots at `tpidrro_el0 & ~7`, and a key from
                // `pthread_key_create` is its slot's index. Pinned by the test
                // `the_direct_tsd_read_equals_pthread_getspecific`.
                self.ld_word(X9, RelocTo::TlsDesc);
                self.put(enc::ldr(enc::Size::X, X10, X9, 0));
                self.put(enc::mrs_tpidrro_el0(X11));
                // `and x11, x11, #~7`: 61 ones rotated right by 61.
                self.put(enc::and_imm(X11, X11, 1, 61, 60));
                self.put(enc::ldr_lsl3(X0, X11, X10));
                let found = self.here();
                self.put(enc::cbnz(X0, 0));
                // The thread's first access: the getter makes its block.
                self.put(enc::mov(X0, X9));
                self.ld_word(X16, RelocTo::TlsGetter);
                self.put(enc::blr(X16));
                self.land(found)?;
            }
            // The portable twin: the getter, which asks `pthread_getspecific`.
            Os::Linux => {
                self.ld_word(X0, RelocTo::TlsDesc);
                self.ld_word(X16, RelocTo::TlsGetter);
                self.put(enc::blr(X16));
            }
        }
        self.ld_word(X10, RelocTo::TlsOffset(rec.place));
        self.put(enc::add(X9, X0, X10));
        self.st(X9, inst);
        Ok(())
    }
}
