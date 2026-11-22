//! Unwinding in the record form (docs/v2/plan/design/run.md, "End state", "Panics"): `invoke`,
//! `resume` and landing blocks lowered, and the record's unwind rows made of them
//! (blaze-code unwind.rs says what the rows mean).
//!
//! - **The frame.** Every function has pass.rs `run`'s prologue, `stp x29, x30, [sp, #-16]!;
//!   mov x29, sp`, and saves no other register (every value lives in its frame cell), so its
//!   frame word is `Frame::PLAIN`.
//! - **A call's row.** Each `blr` of a `call` or `invoke` (not of a runtime routine: `memcpy`
//!   and the rest never unwind) gets a row covering that one instruction: the personality
//!   routine looks up the return address minus one, which is inside it. An `invoke` lands at its
//!   landing block with the block's kind as the action (cleanup, catch, terminate); a `call`
//!   outside a cleanup lands nowhere (`Continue`: unwinding goes on to the caller). A `call`
//!   inside a cleanup gets no row, so an unwind out of it ends the program (the pinned
//!   library's eh.rs: "Ip is not present in the table. This indicates a nounwind call.", then
//!   `EHAction::Terminate`, "Process should be terminated as the call site does not permit
//!   unwinding"): a destructor that panics while the stack unwinds ends the process, as rustc's
//!   own cleanups do with their terminate edge. The one exception is `resume`'s call of
//!   `_Unwind_Resume`, whose row continues (below): the unwind goes on through it.
//! - **Cleanup code** is every block reached from a `cleanup` or `terminate` landing block by
//!   ordinary edges (a drop flag's test makes ordinary blocks inside a cleanup). A `catch`
//!   landing block's code is not: the unwind has stopped there, and a call in it unwinds as any.
//! - **A landing block** is entered by the unwinder with the exception pointer in `x0` (the
//!   pinned library's gcc.rs: `UNWIND_DATA_REG: (i32, i32) = (0, 1); // R0, R1 / X0, X1` and
//!   `_Unwind_SetGR(context, UNWIND_DATA_REG.0, exception_object.cast())`) and `sp`, `x29` as
//!   at the call (restored from the callee's frame): its one parameter's cell is stored first.
//! - **`resume`** calls `_Unwind_Resume` with the exception; it does not return. Its `blr` has a
//!   `Continue` row, cleanup code or not: `_Unwind_Resume`'s phase 2 asks this frame's
//!   personality routine of that call, and with no row the process aborts.
//!
//! The literal-word form refuses all of it by name: it places no unwind entry.

use alloc::vec::Vec;
use alloc::{alloc::Allocator, alloc::Global};

use blaze_code::record::CodeRecord;
use blaze_code::unwind::{self, Action, Frame, Row};
use blaze_lir::{Block, BlockKind, Op, Value, op::Operands};

use super::{EmitError, Label, Pass, unsupported};
use crate::{
    Routine,
    enc::{self, Field, X0},
};

/// One call's row as the pass sees it: its `blr`'s offset and, for an `invoke`, its landing
/// block (resolved to an offset once every block is placed).
#[derive(Clone, Copy)]
struct Call {
    at: u32,
    landing: Option<Block>,
}

/// The rows a function's calls make, kept in the record form's `Form`.
pub(crate) struct Rows<A: Allocator> {
    calls: Vec<Call, A>,
    /// Per block: whether it is cleanup code (the module's doc).
    cleanup: Vec<bool, A>,
    /// The resolved rows, in the order of their calls.
    rows: Vec<Row, A>,
    /// The block being lowered ([`Pass::block_entry`]).
    current: usize,
}

impl<A: Allocator + Clone> Rows<A> {
    pub(crate) fn new_in(alloc: A) -> Rows<A> {
        Rows { calls: Vec::new_in(alloc.clone()), cleanup: Vec::new_in(alloc.clone()), rows: Vec::new_in(alloc), current: 0 }
    }
}

/// Writes the frame word and the rows into `record` (emit_record's one hook).
pub(crate) fn into_record<A: Allocator>(rows: &Rows<A>, record: &mut CodeRecord<'_>) -> Result<(), EmitError> {
    unwind::fill(record, Frame::PLAIN, &rows.rows).map_err(|_| EmitError::Record { why: "the frame word of the plain frame" })
}

/// Calls `f` with each block `at`, a terminator, leads to by an ordinary edge.
fn successors(view: &blaze_lir::View<'_>, at: usize, mut f: impl FnMut(Block)) {
    let mut o = view.operands(at);
    match view.op(at) {
        Op::Jump => f(o.target().block),
        Op::Br => {
            o.value();
            f(o.target().block);
            f(o.target().block);
        }
        Op::Switch => {
            let scrutinee = o.value();
            f(o.target().block);
            for (_, target) in o.cases(scrutinee) {
                f(target.block);
            }
        }
        Op::Invoke => {
            o.reference();
            o.sig();
            f(o.block());
        }
        Op::InvokeIndirect => {
            o.value();
            o.sig();
            f(o.block());
        }
        _ => {}
    }
}

impl<A: Allocator> Pass<'_, '_, A> {
    /// Before any block: finds the cleanup code. A function with a landing block is refused in
    /// the literal-word form.
    pub(super) fn unwind_begin(&mut self) -> Result<(), EmitError> {
        let view = self.view;
        let blocks = view.blocks();
        let landing = (0..blocks).find(|&b| view.block_kind(Block(b as u32)).is_landing());
        let Some(rows) = self.record.as_deref_mut().map(|form| &mut form.unwind) else {
            return match landing {
                None => Ok(()),
                Some(b) => {
                    let inst = view.block(Block(b as u32)).start;
                    Err(unsupported(inst, view.op(inst), "unwinding in the literal-word form (the record form carries landing pads)"))
                }
            };
        };
        rows.cleanup.clear();
        rows.cleanup.resize(blocks, false);
        let mut todo: Vec<Block, Global> = Vec::new_in(Global);
        for b in 0..blocks {
            if matches!(view.block_kind(Block(b as u32)), BlockKind::Cleanup | BlockKind::Terminate) {
                rows.cleanup[b] = true;
                todo.push(Block(b as u32));
            }
        }
        while let Some(block) = todo.pop() {
            successors(&view, view.terminator(block), |next| {
                if !rows.cleanup[next.index()] {
                    rows.cleanup[next.index()] = true;
                    todo.push(next);
                }
            });
        }
        Ok(())
    }

    /// At each block's first byte: the block is noted for its calls' rows; a landing block's one
    /// parameter, the exception pointer the unwinder put in `x0`, is stored to its cell.
    pub(super) fn block_entry(&mut self, block: Block) {
        let view = self.view;
        if let Some(form) = self.record.as_deref_mut() {
            form.unwind.current = block.index();
        }
        if view.block_kind(block).is_landing() {
            // `sp` as at the call, lower when it had stacked arguments: put back first
            // (pass/stack.rs); `x0` is untouched by it.
            self.restore_sp();
            let param = view.params(block).start;
            self.st(X0, param);
        }
    }

    /// Reads an `invoke`'s two blocks between its signature and its arguments (`call` reads the
    /// rest); `None` for a `call`.
    pub(super) fn invoke_blocks(&mut self, op: Op, o: &mut Operands<'_>) -> Option<(Block, Block)> {
        match op {
            Op::Invoke | Op::InvokeIndirect => Some((o.block(), o.block())),
            _ => None,
        }
    }

    /// The `blr` at `at` of instruction `inst` was just put: its row (none inside cleanup code,
    /// none in the literal-word form, which refused landing blocks already).
    pub(super) fn call_row(&mut self, at: u32, blocks: Option<(Block, Block)>) {
        let Some(form) = self.record.as_deref_mut() else { return };
        let rows = &mut form.unwind;
        match blocks {
            Some((_, landing)) => rows.calls.push(Call { at, landing: Some(landing) }),
            None if rows.cleanup.get(rows.current).copied().unwrap_or(false) => {}
            None => rows.calls.push(Call { at, landing: None }),
        }
    }

    /// After an `invoke`'s call and its results: the edge to the block it returns to.
    pub(super) fn invoke_normal(&mut self, inst: usize, op: Op, next: Option<u32>) {
        let mut o = self.view.operands(inst);
        match op {
            Op::Invoke => {
                let _ = o.reference();
            }
            _ => {
                let _ = o.value();
            }
        }
        o.sig();
        if let Some((normal, _)) = self.invoke_blocks(op, &mut o)
            && next != Some(normal.0)
        {
            self.put_to(enc::b(0), Field::Imm26, Label::Block(normal.0));
        }
    }

    /// `resume exception`: `_Unwind_Resume(exception)`, which does not return. Its `blr` gets a
    /// row that lands nowhere (`Continue`), though it is in cleanup code: `_Unwind_Resume` goes
    /// on with phase 2 from its caller, this frame at this call, and asks the personality
    /// routine of it. With no row the pinned library's eh.rs gives `EHAction::Terminate` ("Ip is
    /// not present in the table. This indicates a nounwind call."), gcc.rs answers phase 2's
    /// call with `_URC_FATAL_PHASE2_ERROR`, and libunwind's `_Unwind_Resume` stops the process:
    /// "libunwind: _Unwind_Resume - _Unwind_Resume() can't return" and SIGABRT, after every
    /// panic whose unwind passed a cleanup (tempo's tests, t1500, 2026-11-21). LLVM gives its
    /// `_Unwind_Resume` call a call-site entry with no landing pad, which is this row.
    pub(super) fn resume(&mut self, inst: usize) {
        let exception: Value = self.view.operands(inst).value();
        self.ld(X0, exception);
        self.call_routine(Routine::UnwindResume);
        // `call_routine` ends with the `blr`.
        let at = self.here() - unwind::INST;
        if let Some(form) = self.record.as_deref_mut() {
            form.unwind.calls.push(Call { at, landing: None });
        }
    }

    /// Once every block is placed: each call's row, its landing an offset.
    pub(super) fn unwind_end(&mut self) -> Result<(), EmitError> {
        let view = self.view;
        let Some(form) = self.record.as_deref_mut() else { return Ok(()) };
        let rows = &mut form.unwind;
        rows.rows.clear();
        for call in rows.calls.iter() {
            let row = match call.landing {
                None => Row { start: call.at, len: unwind::INST, landing: 0, action: Action::Continue },
                Some(block) => {
                    let action = match view.block_kind(block) {
                        BlockKind::Cleanup => Action::Cleanup,
                        BlockKind::Catch => Action::Catch,
                        BlockKind::Terminate => Action::Terminate,
                        BlockKind::Normal | BlockKind::Cold => {
                            return Err(EmitError::Record { why: "an unwind edge to a block that is not a landing block" });
                        }
                    };
                    let landing = self.block_at[block.index()];
                    if landing == blaze_lir::func::NONE {
                        return Err(EmitError::Record { why: "an unwind edge to a block never placed" });
                    }
                    Row { start: call.at, len: unwind::INST, landing, action }
                }
            };
            rows.rows.push(row);
        }
        Ok(())
    }
}
