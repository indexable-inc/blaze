//! Nounwind bodies (docs/v2/plan design "UNWIND" 3.4, piece P4): a body whose own ABI does not
//! permit unwinding ends a panic that reaches it, through core's `panic_cannot_unwind`.
//!
//! The rule, from the Reference, "Functions", the unwinding table: with "panic=unwind", a
//! "panic-unwind" reaching a "non-unwinding" ABI boundary is "abort"; "The \"unwinding\" ABI
//! category refers to \"Rust\" (the implicit ABI of Rust functions not marked extern),
//! \"C-unwind\", and any other ABI with -unwind in its name. The \"non-unwinding\" ABI category
//! refers to all other ABI strings, including \"C\" and \"stdcall\"." And
//! [items.fn.extern.abort]: "With panic=unwind, when a panic is turned into an abort by a
//! non-unwinding ABI boundary, either no destructors (Drop calls) will run, or all destructors
//! up until the ABI boundary will run. It is unspecified which of those two behaviors will
//! happen." blaze takes the first: no destructor runs.
//!
//! The pinned library's own entry point (core/src/panicking.rs:346-351):
//! `#[lang = "panic_cannot_unwind"] #[rustc_nounwind] fn panic_cannot_unwind() -> !`, "Panics
//! because we cannot unwind out of a function ... called directly by the codegen backend, and
//! must not have any extra arguments"; it calls `panic_nounwind("panic in a function that
//! cannot unwind")`, which aborts.
//!
//! So a NOUNWIND body (its fn's own ABI is non-unwinding, or it is `#[rustc_nounwind]`; the
//! world says which by [`crate::Adts::nounwind`]) gets `FnAttrs::NOUNWIND`, and under
//! [`Panic::Unwind`] every call in it that may unwind is an `invoke` whose unwind edge goes to
//! the body's one `terminate` block: its one parameter the exception, a plain call of
//! `panic_cannot_unwind`, then `unreachable`. No cleanup is built in such a body. The call in
//! the terminate block is cleanup code: the emitter gives it no row (a panic there is the
//! personality's `Terminate`; `panic_cannot_unwind` is itself nounwind and aborts).
//!
//! Why a block and not "no row" for the call: the invoke survives inlining (a callee's missing
//! row would become the caller's continue row, and the panic would cross the boundary) and the
//! LLVM tier (a `landingpad` the inliner keeps).
//!
//! REFUSED BY NAME: an `extern` body (blaze's [`Abi::C`], which stands for both "C" and
//! "C-unwind") built under [`Panic::Unwind`] whose unwind category the world does not say: its
//! panic would either cross a boundary that must abort or abort where it must unwind, and which
//! is not known. A missing `panic_cannot_unwind` is [`BuildError::NoLangFn`], as rustc refuses
//! a program without it.
//!
//! Every drop call in such a body (walk/unwind_ladder.rs `drop_call`: a scope's exit drops, a
//! discarded value's, a temporary's) lands on the same `terminate` block.

use core::alloc::Allocator;

use blaze_code::record::TrapKind;
use blaze_lir::func::{BlockKind, FnAttrs, RefKind, SigId};
use blaze_lir::{Block, Flags, Ty, Value};
use blaze_typeck::{ItemKey, List};

use super::{Callee, Walk};
use crate::{Abi, BuildError, Instance, Panic};

/// The lang item a nounwind body's terminate block calls (core/src/panicking.rs:348).
const CANNOT_UNWIND_FN: &str = "panic_cannot_unwind";

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// Whether the body being built may not unwind: its fn's own ABI is non-unwinding, or it is
    /// `#[rustc_nounwind]` (set once by [`Self::mark_nounwind`] before the body is walked).
    pub(super) fn is_nounwind(&self) -> bool {
        self.nounwind
    }

    /// Decides [`Self::is_nounwind`] for the body of `owner`, declared with `abi` (`body`:
    /// whether it is the fn's written body; drop glue and closures are of the Rust ABI and
    /// unwind), and marks the LIR function `nounwind` when it is. Called once, after the
    /// signature is set and before the entry block.
    ///
    /// The world's answer ([`crate::Adts::nounwind`]) decides. With none: a Rust or
    /// "rust-call" body unwinds (its ABI is of the unwinding category; only the library's
    /// `#[rustc_nounwind]` fns differ, which a world reports); an `extern` body under
    /// [`Panic::Unwind`] is refused by name, because blaze's [`Abi::C`] is both "C" (abort) and
    /// "C-unwind" (unwind); under [`Panic::Abort`] nothing unwinds, so it is left unmarked.
    pub(super) fn mark_nounwind(&mut self, owner: ItemKey, abi: Abi, body: bool) -> Result<(), BuildError> {
        let nounwind = if !body {
            false
        } else {
            match (self.adts.nounwind(owner), abi) {
                (Some(answer), _) => answer,
                (None, Abi::Rust | Abi::RustCall) => false,
                (None, Abi::C) => match self.adts.panic() {
                    Panic::Abort => false,
                    Panic::Unwind => {
                        return Err(BuildError::UnsupportedSig {
                            what: "an `extern` fn's body under `panic = unwind` whose ABI's unwind category (\"C\": a panic aborts at the boundary; \"C-unwind\": it unwinds through) the build's `Adts::nounwind` does not say",
                        });
                    }
                },
            }
        };
        self.nounwind = nounwind;
        let mut attrs = if body { self.adts.fn_attrs(owner) } else { FnAttrs::default() };
        if nounwind {
            attrs.0 |= FnAttrs::NOUNWIND;
        }
        if attrs.0 != 0 {
            self.b.set_attrs(attrs);
        }
        Ok(())
    }

    /// The body's one `terminate` landing block, made at its first need: its block id only.
    /// Its code (the exception parameter, the `panic_cannot_unwind` call, `unreachable`) is
    /// filled by [`Self::call_terminating`] right after the first invoke that lands on it, when
    /// the builder's current block has its terminator. Refuses a program without the lang fn.
    pub(super) fn terminate_block(&mut self) -> Result<Block, BuildError> {
        if let Some(block) = self.terminate {
            return Ok(block);
        }
        if self.adts.lang_fn(CANNOT_UNWIND_FN).is_none() {
            return Err(BuildError::NoLangFn { expr: None, name: CANNOT_UNWIND_FN });
        }
        let block = self.b.create_block(BlockKind::Terminate);
        self.reached.push(false);
        self.terminate = Some(block);
        Ok(block)
    }

    /// The call `call_unwinding` makes in a nounwind body under [`Panic::Unwind`]: an `invoke`
    /// (`invoke_indirect` for a pointer) whose unwind edge is the body's terminate block, and
    /// whose normal edge is a new block the walk goes on in. Gives the call's value. Builds no
    /// cleanup ([items.fn.extern.abort] allows "no destructors").
    pub(super) fn call_terminating(&mut self, callee: Callee, sig: SigId, values: &[Value]) -> Result<Value, BuildError> {
        let first = self.terminate.is_none();
        let pad = self.terminate_block()?;
        let normal = self.create();
        self.reach(normal);
        self.reach(pad);
        let r = match callee {
            Callee::Direct(reference) => self.b.invoke(Flags::NONE, reference, sig, values, normal, pad),
            Callee::Pointer(p) => self.b.invoke_indirect(Flags::NONE, p, sig, values, normal, pad),
        };
        if first {
            self.fill_terminate(pad)?;
        }
        self.begin(normal);
        Ok(r)
    }

    /// Fills terminate block `pad` (once per body): its exception parameter, a plain call of
    /// `panic_cannot_unwind` (no arguments, never returns: core/src/panicking.rs:348), and
    /// `unreachable`. The builder's current block must have its terminator.
    fn fill_terminate(&mut self, pad: Block) -> Result<(), BuildError> {
        let item = self.adts.lang_fn(CANNOT_UNWIND_FN).ok_or(BuildError::NoLangFn { expr: None, name: CANNOT_UNWIND_FN })?;
        self.begin(pad);
        let _exception = self.b.param(Ty::PTR);
        // One pad serves the whole body, so no expression is its place: a tracked entry point
        // is refused by name (walk/track_caller.rs); the pinned one is not tracked.
        let callee = Instance { item, args: List::EMPTY };
        let (sig, values) = self.lang_call_sig(None, callee, &[], &[])?;
        let place = self.ref_place(callee);
        let reference = self.b.reference(RefKind::Func, place);
        self.b.call(Flags::NONE, reference, sig, &values);
        self.unreachable_of(TrapKind::AfterPanic, None);
        Ok(())
    }
}
