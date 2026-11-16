//! Cleanup paths (docs/v2/plan/design/run.md, "End state", "Panics"; piece 11): what a call that
//! may unwind leaves behind it, when the build unwinds ([`crate::Panic::Unwind`]).
//!
//! The rule, from the Reference: "Panic" says the strategy `unwind` "unwinds the stack and is
//! potentially recoverable", and that "if the panic handler is set to abort, panicking will
//! always terminate the process without destructors being run"; its "Unwinding" section: "as
//! the panic traverses Rust frames, live objects in those frames that implement Drop will have
//! their drop methods called. Thus, when normal execution resumes, no-longer-accessible objects
//! will have been \"cleaned up\" just as if they had gone out of scope normally."
//!
//! So a call made while anything of this frame needs drop is an `invoke` whose landing block
//! (a `cleanup` block) drops exactly what a `return` at that point would drop, by the same code:
//! `drop_from(0)`, every open scope's entries last declared first, parameters last, each by its
//! static state or its drop flag, leaving the state as it was (`kill` false); then `resume`s.
//! A call with nothing to drop stays a plain `call` (the emitter gives it a row that lets the
//! unwind go on). Under [`crate::Panic::Abort`] nothing here is built.
//!
//! The calls inside a cleanup (the drops) are plain: the emitter gives a call in cleanup code
//! no row, so a destructor that panics while the stack unwinds ends the process.
//!
//! REFUSED BY NAME ([`BuildError::NoCleanup`]), because the cleanup would be inexact:
//!
//! - **An operand waiting on the value stack.** The Reference, "Destructors": "Temporaries are
//!   also created to hold the result of operands to an expression while the other operands are
//!   evaluated ... dropping them has no effect unless one of the operands to an expression
//!   breaks out of the expression, returns, or panics." A call made while an earlier operand's
//!   value waits (`g(s, f())`, `v.push(f())`) would have to drop that value if it is owned and
//!   needs drop; the walk keeps no type beside each waiting value, so it cannot tell `&mut v`
//!   from a moved `String`. A waiting value that is an integer or float scalar, an unsized
//!   place (only ever borrowed) or nothing passes; any other is refused. MISSING: the type of
//!   each waiting value (a row beside `vals`), with which a reference or a `Copy` value passes
//!   and an owned one joins the cleanup.
//! - **A call in an `async` body with values to drop.** The coroutine pass refuses landing
//!   blocks (blaze-lir coroutine.rs, "Unwinding ... is not built").
//!
//! NOT BUILT, and not refused because it is no wrong answer of the kind a cleanup could give:
//! a drop at a scope's normal exit is a plain call, so a destructor that panics there skips the
//! scope's remaining drops (Rust drops them); MISSING: an unwind edge from each exit drop to a
//! cleanup of the entries before it.
//!
//! `catch_unwind` (the intrinsic; pinned core intrinsics/mod.rs: "Rust's \"try catch\"
//! construct for unwinding. Invokes the function pointer `try_fn` with the data pointer `data`,
//! and calls `catch_fn` if unwinding occurs while `try_fn` runs. Returns `true` if unwinding
//! occurred and `catch_fn` was called; returns `false` otherwise.") is an `invoke` of `try_fn`
//! whose landing block is a `catch` block calling `catch_fn(data, exception)`.

use core::alloc::Allocator;

use blaze_lir::builder::{Param, Variadic};
use blaze_lir::func::{BlockKind, Conv, SigId};
use blaze_lir::{Block, Flags, Ty, Value};
use blaze_typeck::ExprId;

use super::{Callee, V, Walk, malformed};
use crate::{BuildError, Panic};

impl<A: Allocator + Clone> Walk<'_, '_, '_, A> {
    /// A block of `kind` (a landing block), noted as not yet reached.
    fn landing_block(&mut self, kind: BlockKind) -> Block {
        let block = self.b.create_block(kind);
        self.reached.push(false);
        block
    }

    /// Whether a call made now must land somewhere: the build unwinds, the call is not inside
    /// a cleanup, and something of this frame needs drop. Refuses what cannot be exact.
    fn needs_cleanup(&mut self) -> Result<bool, BuildError> {
        if self.adts.panic() == Panic::Abort || self.cleanup || !self.live {
            return Ok(false);
        }
        for &v in self.vals.iter() {
            let plain = match v {
                V::None | V::Unsized(..) => true,
                V::Scalar(x) => matches!(self.b.ty(x), Ty::I1 | Ty::I8 | Ty::I16 | Ty::I32 | Ty::I64 | Ty::I128 | Ty::F32 | Ty::F64),
                V::Place(..) => false,
            };
            if !plain {
                return Err(BuildError::NoCleanup {
                    what: "an operand evaluated before this call waits on the value stack, and the walk keeps no type for it, so whether the unwind path must drop it is not known",
                });
            }
        }
        if !self.pending_from(0) {
            return Ok(false);
        }
        if self.co.is_some() {
            return Err(BuildError::NoCleanup { what: "a call with values to drop inside an `async` body: the coroutine pass does not carry landing blocks" });
        }
        Ok(true)
    }

    /// The call `emit_call` makes: an `invoke` whose cleanup drops what is live when
    /// [`Self::needs_cleanup`] says so, else a plain call. Gives the call's value.
    pub(super) fn call_unwinding(&mut self, callee: Callee, sig: SigId, values: &[Value]) -> Result<Value, BuildError> {
        if !self.needs_cleanup()? {
            return Ok(match callee {
                Callee::Direct(reference) => self.b.call(Flags::NONE, reference, sig, values),
                Callee::Pointer(p) => self.b.call_indirect(Flags::NONE, p, sig, values),
            });
        }
        let normal = self.create();
        let pad = self.landing_block(BlockKind::Cleanup);
        self.reach(normal);
        self.reach(pad);
        let r = match callee {
            Callee::Direct(reference) => self.b.invoke(Flags::NONE, reference, sig, values, normal, pad),
            Callee::Pointer(p) => self.b.invoke_indirect(Flags::NONE, p, sig, values, normal, pad),
        };
        self.begin(pad);
        let exception = self.b.param(Ty::PTR);
        self.cleanup = true;
        let dropped = self.drop_from(0);
        self.cleanup = false;
        dropped?;
        self.b.resume(exception);
        self.begin(normal);
        Ok(r)
    }

    /// `catch_unwind(try_fn, data, catch_fn)`, its three arguments on top of the value stack:
    /// pushes the `bool` (an `i8`) of whether `try_fn` unwound. Under `panic = abort` nothing
    /// unwinds: `try_fn(data)`, then `false`.
    pub(super) fn catch_unwind(&mut self, e: ExprId, n: usize) -> Result<(), BuildError> {
        if n != 3 {
            return Err(malformed("a `catch_unwind` call not of three arguments"));
        }
        let base = self.vals.len().checked_sub(3).ok_or(malformed("an intrinsic's arguments missing from the value stack (a build bug)"))?;
        let (try_fn, data, catch_fn) = (self.vals[base], self.vals[base + 1], self.vals[base + 2]);
        self.vals.truncate(base);
        let try_fn = self.scalar(try_fn, Ty::PTR)?;
        let data = self.scalar(data, Ty::PTR)?;
        let catch_fn = self.scalar(catch_fn, Ty::PTR)?;
        let one = self.b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[]);
        let two = self.b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::PTR)], &[]);
        if self.adts.panic() == Panic::Abort || !self.live {
            self.b.call_indirect(Flags::NONE, try_fn, one, &[data]);
            let no = self.b.iconst(Ty::I8, 0);
            self.vals.push(V::Scalar(no));
            return Ok(());
        }
        if self.co.is_some() {
            return Err(BuildError::Unsupported { expr: e, what: "`catch_unwind` inside an `async` body: the coroutine pass does not carry landing blocks" });
        }
        let normal = self.create();
        let pad = self.landing_block(BlockKind::Catch);
        let join = self.create();
        self.reach(normal);
        self.reach(pad);
        self.b.invoke_indirect(Flags::NONE, try_fn, one, &[data], normal, pad);
        self.begin(normal);
        let no = self.b.iconst(Ty::I8, 0);
        self.jump(join, &[no]);
        self.begin(pad);
        let exception = self.b.param(Ty::PTR);
        self.b.call_indirect(Flags::NONE, catch_fn, two, &[data, exception]);
        let yes = self.b.iconst(Ty::I8, 1);
        self.jump(join, &[yes]);
        self.begin(join);
        let caught = self.b.param(Ty::I8);
        self.vals.push(V::Scalar(caught));
        Ok(())
    }
}
