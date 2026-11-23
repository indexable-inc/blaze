//! Scopes, the drop schedule and the unwind chain.
//!
//! The schedule (`Lower::sched`) is a stack of what is live, in declaration order: parameters
//! at the bottom, then each scope's bindings and temporaries as they are made. A scope is a
//! mark into it (`Lower::marks`). The Reference, "Destructors": "When control flow leaves a
//! drop scope all variables associated to that scope are dropped in reverse order of
//! declaration"; temporaries of a statement are dropped at its end (its temporary scope).
//!
//! - Normal exit, `break`, `continue` and `return` ([`Lower::emit_exit`]): each entry above the
//!   mark, last first: a `Drop` row when its type needs drop (un-elaborated: "if initialized"),
//!   then `StorageDead`. Nothing is popped: the lexical scope goes on for the other paths.
//! - Unwind ([`Lower::unwind_here`]): a call, drop or assert made while anything droppable is
//!   live unwinds to a cleanup block that drops exactly what a return at that point would drop,
//!   last declared first, each `Drop` with `unwind terminate`, then `resume` (blaze-build
//!   walk/unwind.rs). With nothing droppable live, the edge is `unwind continue`.
//!
//! A cleanup chain is shared: the block that drops entry `i` goes on to the one that drops the
//! droppable entry below it, so each entry has at most one cleanup block while it is live. A
//! block's rows are contiguous in the body, so cleanup blocks are made when first needed and
//! filled after the body (`Pending`).

use core::alloc::Allocator;

use blaze_body::{Block, BlockKind, Local, Operand, Place, Span, StmtKind, Unwind, Value};
use blaze_own::Vec;
use blaze_typeck::{ExprId, PTy};

use crate::{BodyFacts, Lower, LowerError};

/// One live local of the schedule.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Entry {
    pub(crate) local: Local,
    pub(crate) place: Place,
    /// Its type needs drop: a `Drop` row at each exit and in each cleanup chain.
    pub(crate) drops: bool,
    /// It had a `StorageLive` (a parameter has none, and no `StorageDead`).
    pub(crate) storage: bool,
    /// The cleanup block that drops it, then what is below it.
    pub(crate) cleanup: Option<Block>,
}

/// A cleanup block made and not yet filled.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Pending {
    /// `drop place then next unwind terminate`.
    Drop { block: Block, place: Place, next: Block },
    /// `dest = value; goto next`: an assignment's new value written on its drop's unwind edge
    /// (the Reference, "Assignment expressions": the old value is dropped, then the new one
    /// written; when that drop unwinds, the new value is still owned by the place).
    Assign { block: Block, dest: Place, value: Operand, next: Block },
}

/// An enclosing loop or labelled block: where `break` and `continue` go.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Target {
    pub(crate) expr: ExprId,
    /// Where `break` goes.
    pub(crate) exit: Block,
    /// Where `continue` goes (a loop's head); `None` for a block.
    pub(crate) head: Option<Block>,
    /// Where a `break`'s value is written.
    pub(crate) dest: Place,
    /// The schedule's length when it was entered: a `break` drops everything above.
    pub(crate) mark: usize,
}

/// A binding made by a pattern and scheduled once its statement's temporaries are dropped.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Binding {
    pub(crate) local: Local,
    pub(crate) place: Place,
    pub(crate) ty: PTy,
    pub(crate) drops: bool,
}

impl<F: BodyFacts, A: Allocator + Copy> Lower<'_, '_, '_, F, A> {
    /// Opens a scope; its mark is the schedule's length.
    pub(crate) fn open(&mut self) -> usize {
        let m = self.sched.len();
        self.marks.push(m as u32);
        m
    }

    /// Drops what the innermost scope holds, on the normal path, and closes it.
    pub(crate) fn close(&mut self, span: Span) -> Result<(), LowerError> {
        let Some(&m) = self.marks.last() else { return self.refuse(crate::Construct::Malformed, self.body.root) };
        self.emit_exit(m as usize, span)?;
        self.pop_scope();
        Ok(())
    }

    /// Closes the innermost scope without writing rows: each path out of it already wrote its
    /// exit ([`Self::emit_exit`]).
    pub(crate) fn pop_scope(&mut self) {
        if let Some(m) = self.marks.pop() {
            self.sched.truncate(m as usize);
        }
    }

    /// A new temporary's storage begins and it joins the innermost scope; a temporary of an
    /// extending borrow's operand joins the scope of the `let` being lowered instead (the
    /// Reference, "Temporary lifetime extension"), below that statement's own temporaries.
    pub(crate) fn declare(&mut self, local: Local, place: Place, drops: bool, e: ExprId, span: Span) {
        self.b.push(StmtKind::StorageLive(local), span);
        let entry = Entry { local, place, drops, storage: true, cleanup: None };
        if let Some(d) = self.ext_mark
            && self.extended.scan_for(&e).is_some()
            && let Some(&at) = self.marks.get(d)
        {
            let at = at as usize;
            self.sched.insert_shift(at, entry);
            for m in self.marks.iter_mut().skip(d) {
                *m += 1;
            }
            self.invalidate_from(at);
            return;
        }
        self.sched.push(entry);
    }

    /// Bindings made by a pattern join the innermost scope, in the order the pattern made them.
    pub(crate) fn schedule(&mut self, out: &[Binding]) {
        for x in out {
            self.sched.push(Entry { local: x.local, place: x.place, drops: x.drops, storage: true, cleanup: None });
        }
    }

    /// Entries at `at` and above lost their cleanup blocks: what is below them changed.
    pub(crate) fn invalidate_from(&mut self, at: usize) {
        for x in self.sched.iter_mut().skip(at) {
            x.cleanup = None;
        }
    }

    /// The exit rows from here down to schedule length `to`: each entry above it, last first,
    /// dropped when its type needs drop (that drop unwinding to the chain below it), then its
    /// storage ended. The schedule is not changed.
    pub(crate) fn emit_exit(&mut self, to: usize, span: Span) -> Result<(), LowerError> {
        let mut i = self.sched.len();
        while i > to {
            i -= 1;
            let Some(&x) = self.sched.get(i) else { return self.refuse(crate::Construct::Malformed, self.body.root) };
            if x.drops {
                let unwind = self.chain(i);
                let next = self.b.new_block(BlockKind::Normal);
                self.b.terminate(StmtKind::Drop { place: x.place, target: next, unwind }, span);
                self.b.start(next);
            }
            if x.storage {
                self.b.push(StmtKind::StorageDead(x.local), span);
            }
        }
        Ok(())
    }

    /// The 2021 tail rule: the entries `sched[from..to]` (the block's own) are dropped and ended
    /// on the normal path, and the ones above `to` (its tail's temporaries) stay live in the
    /// enclosing scope.
    pub(crate) fn exit_keeping_tail(&mut self, from: usize, to: usize, span: Span) -> Result<(), LowerError> {
        let mut i = to;
        while i > from {
            i -= 1;
            let Some(&x) = self.sched.get(i) else { return self.refuse(crate::Construct::Malformed, self.body.root) };
            if x.drops {
                let unwind = self.chain(i);
                let next = self.b.new_block(BlockKind::Normal);
                self.b.terminate(StmtKind::Drop { place: x.place, target: next, unwind }, span);
                self.b.start(next);
            }
            if x.storage {
                self.b.push(StmtKind::StorageDead(x.local), span);
            }
        }
        for _ in from..to {
            self.sched.remove_shift(from);
        }
        self.invalidate_from(from);
        Ok(())
    }

    /// The unwind edge of a row added now: the chain that drops every droppable live entry.
    pub(crate) fn unwind_here(&mut self) -> Unwind {
        let top = self.sched.len();
        self.chain(top)
    }

    /// The unwind edge that drops every droppable entry below schedule index `top`.
    pub(crate) fn chain(&mut self, top: usize) -> Unwind {
        let mut todo: Vec<usize, A> = Vec::new_in(self.alloc);
        let mut below: Option<Block> = None;
        let mut j = top.min(self.sched.len());
        while j > 0 {
            j -= 1;
            let Some(&x) = self.sched.get(j) else { continue };
            if !x.drops {
                continue;
            }
            if let Some(b) = x.cleanup {
                below = Some(b);
                break;
            }
            todo.push(j);
        }
        // `todo` holds the entries without a block, highest first: make them lowest first.
        while let Some(i) = todo.pop() {
            let Some(place) = self.sched.get(i).map(|x| x.place) else { continue };
            let next = match below {
                Some(b) => b,
                None => self.resume_block(),
            };
            let block = self.b.new_block(BlockKind::Cleanup);
            self.pending.push(Pending::Drop { block, place, next });
            if let Some(entry) = self.sched.get_mut(i) {
                entry.cleanup = Some(block);
            }
            below = Some(block);
        }
        match below {
            Some(b) => Unwind::Cleanup(b),
            None => Unwind::Continue,
        }
    }

    /// The one cleanup block that ends unwinding out of the body: `resume`.
    pub(crate) fn resume_block(&mut self) -> Block {
        if let Some(b) = self.resume {
            return b;
        }
        let b = self.b.new_block(BlockKind::Cleanup);
        self.resume = Some(b);
        b
    }

    /// The block a cleanup chain continues to: `u`'s, or `resume`'s when nothing is left.
    pub(crate) fn unwind_block(&mut self, u: Unwind) -> Block {
        match u {
            Unwind::Cleanup(b) => b,
            _ => self.resume_block(),
        }
    }

    /// Fills each cleanup block made, after the body's last normal block.
    pub(crate) fn fill_cleanups(&mut self, span: Span) {
        let mut i = 0;
        while let Some(&pending) = self.pending.get(i) {
            match pending {
                Pending::Drop { block, place, next } => {
                    self.b.start(block);
                    self.b.terminate(StmtKind::Drop { place, target: next, unwind: Unwind::Terminate }, span);
                }
                Pending::Assign { block, dest, value, next } => {
                    self.b.start(block);
                    self.b.assign(dest, Value::Use(value), span);
                    self.b.terminate(StmtKind::Goto(next), span);
                }
            }
            i += 1;
        }
        if let Some(b) = self.resume {
            self.b.start(b);
            self.b.terminate(StmtKind::Resume, span);
        }
    }

    /// After a row that does not return (`goto`, `return`, a diverging call), the code that
    /// follows is lowered into a block nothing reaches.
    pub(crate) fn dead_block(&mut self) {
        let b = self.b.new_block(BlockKind::Normal);
        self.b.start(b);
    }
}
