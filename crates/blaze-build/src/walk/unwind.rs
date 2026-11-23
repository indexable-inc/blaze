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
//! - **A waiting operand the walk cannot ground.** The Reference, "Destructors": "Temporaries
//!   are also created to hold the result of operands to an expression while the other operands
//!   are evaluated ... dropping them has no effect unless one of the operands to an expression
//!   breaks out of the expression, returns, or panics." Beside each waiting value the walk keeps
//!   its ground type when it owns it (walk/val_stack.rs): an owned one that needs drop
//!   (`g(s, f())`: the caller's copy of `s`) is dropped by the cleanup first, top of the stack
//!   first, before `drop_from(0)`; a borrow (`v.push(f())`), a scalar or a binding's place is
//!   not owned and passes. An aggregate literal being built (`S { a: String::new(), b: f() }`)
//!   is owned part by part: the cleanup drops each part stored so far, the last stored first.
//!   Refused: an owned waiting value whose type has no layout row, or not held in memory.
//!
//! A drop at a scope's normal exit, and each drop of a drop glue, carries its own unwind
//! ladder (walk/unwind_ladder.rs): a destructor that panics there still drops the rest.
//!
//! - In an `async` body every call made through [`Walk::call_unwinding`] lands, with or
//!   without something to drop: its cleanup's `resume` is where the coroutine pass stores
//!   `Panicked` (blaze-lir coroutine_unwind.rs), so a poll that unwinds leaves its state
//!   poisoned, as rustc's transform poisons every unwind edge of a coroutine.
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

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// A block of `kind` (a landing block), noted as not yet reached.
    pub(super) fn landing_block(&mut self, kind: BlockKind) -> Block {
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
        // An operand waiting on the value stack (walk/val_stack.rs): one the walk owns whose type
        // needs drop joins the cleanup; one whose ownership it could not ground is refused there.
        let waiting = self.waiting_owned()?;
        // In an `async` body every call lands, even with nothing to drop: the cleanup's
        // `resume` is where the coroutine pass stores `Panicked` (ASYNC.md D-A5).
        Ok(self.co.is_some() || waiting || self.pending_from(0))
    }

    /// The call `emit_call` makes: an `invoke` whose cleanup drops what is live when
    /// [`Self::needs_cleanup`] says so, else a plain call. Gives the call's value.
    pub(super) fn call_unwinding(&mut self, callee: Callee, sig: SigId, values: &[Value]) -> Result<Value, BuildError> {
        // A body whose ABI does not permit unwinding: no cleanup, the unwind ends in
        // `panic_cannot_unwind` (walk/nounwind.rs; the Reference, [items.fn.extern.abort]).
        if self.adts.panic() == Panic::Unwind && self.live && !self.cleanup && self.is_nounwind() {
            return self.call_terminating(callee, sig, values);
        }
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
        // The waiting operands first (the innermost temporary scope), top of the stack first,
        // then every open scope (walk/val_stack.rs).
        let dropped = self.drop_waiting().and_then(|()| self.drop_from(0));
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
        self.truncate_vals(base);
        let try_fn = self.scalar(try_fn, Ty::PTR)?;
        let data = self.scalar(data, Ty::PTR)?;
        let catch_fn = self.scalar(catch_fn, Ty::PTR)?;
        let one = self.b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[]);
        let two = self.b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::PTR)], &[]);
        if self.adts.panic() == Panic::Abort || !self.live {
            self.b.call_indirect(Flags::NONE, try_fn, one, &[data]);
            let no = self.b.iconst(Ty::I8, 0);
            self.push_val(V::Scalar(no), None);
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
        self.push_val(V::Scalar(caught), None);
        Ok(())
    }
}
