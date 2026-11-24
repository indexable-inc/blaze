//! Unwind ladders (UNWIND.md 3.6, piece P6): every drop sequence carries its own unwind edges,
//! so a destructor that panics while a sequence runs still drops the rest of it.
//!
//! The rule, from the Reference: "Destructors" says each value of a scope, each field of a
//! value and each element of an array or owned slice "is dropped", in a stated order; "Panic"
//! ([panic.unwind.destruction]): "as the panic traverses Rust frames, live objects in those
//! frames that implement Drop will have their drop methods called". A destructor that panics
//! partway through such a sequence does not end the duty to drop what comes after it.
//!
//! So a sequence of drops is a list of [`DropStep`]s built by [`Walk::drops_with_ladder`].
//! Under [`Panic::Unwind`], outside cleanup code, in a live block, in a body that may unwind,
//! each call made while step k is performed (a drop glue, a user `Drop::drop`, a trait object's
//! glue: every drop call goes through [`Walk::drop_call`]) is an `invoke` whose landing block
//! (kind `Cleanup`, ONE per step, shared by the step's calls) performs what is OWED after step
//! k, as plain calls (`self.cleanup` set: the emitter gives a call in cleanup code no row, so
//! a destructor that panics there ends the process, as Rust's `panic_in_cleanup` does), then
//! `resume`s. What is owed is a stack, [`Walk::owed`], performed top first: the steps after k,
//! then whatever an enclosing sequence owes after the step that holds this one (a `Box`'s free
//! after its contents' glue, the frame's live values after a scope's exit drops).
//!
//! The seams:
//! - a scope's exit ([`Walk::drop_scope`]: walk.rs `drop_from` for `break`, `continue` and
//!   `return`, and the block end): its entries, last declared first, then [`DropStep::Frame`]:
//!   the operands waiting on the value stack (walk/val_stack.rs) and every entry of the scopes
//!   still open below, as `call_unwinding`'s cleanup drops them (state left as it was);
//! - drop glue ([`Walk::drop_impl_then_parts`], walk.rs `glue` and walk/moves/boxes.rs
//!   `drop_box_free`): the type's own `Drop::drop` (its landing drops all the fields), then its
//!   parts; the fields of a struct one step each in declaration order ([`Walk::drop_fields`]'s
//!   ladder); a `Box`'s contents, then its free;
//! - the elements of an array or a slice ([`Walk::elements`], walk/slice_glue.rs and the
//!   array arm of the glue): element i's landing drops elements i+1.. by the same loop.
//!
//! A step whose landing would owe nothing is a plain call (the emitter gives it a row that lets
//! the unwind go on), except in an `async` body, where every drop lands: the cleanup's `resume`
//! is where the coroutine pass stores `Panicked` (walk/unwind.rs). In a body that may not unwind
//! (walk/nounwind.rs) every drop call lands on the body's `terminate` block instead. Under
//! [`Panic::Abort`], in cleanup code and in dead code: plain calls, as before this piece.
//!
//! The granularity is the step: the calls one step makes (a binding dropped by its drop flag,
//! the fields of an arm's rest) share one landing, which owes the steps AFTER the step, not the
//! rest of the step itself.
//!
//! NOT BUILT (a leak on the unwind path, never a double drop): a block's value already
//! computed when one of its locals' drops panics is not dropped (it may be a place of a binding
//! still live below, which [`DropStep::Frame`] drops; dropping it too would drop twice); a drop
//! outside these seams (a discarded value's drop, a temporary's at its statement's end) stays a
//! plain call.
//!
//! The call graph has one cycle, through a landing's fill (a step's call fills its landing,
//! which performs owed steps): it is entered once per nesting of sequences, and the fill runs
//! in cleanup code, which builds no landing, so its depth is a small constant.

use core::alloc::Allocator;

use blaze_lir::func::{BlockKind, SigId};
use blaze_lir::{Block, Flags, IntCond, Op, Ty, Value};
use blaze_own::Vec;
use blaze_typeck::{List, PTy};

use super::{Callee, Held, Owned, Repr, Shape, Walk, malformed};
use crate::{BuildError, Instance, Panic};

/// One drop of a sequence: performed on the normal path in order, owed by a landing on the
/// unwind path.
#[derive(Clone, Copy)]
pub(super) enum DropStep {
    /// A drop-scope entry: walk.rs `drop_held` (`kill` only on the normal path).
    Held(Held),
    /// The drop glue of ground type `g` on the value at `at`: walk.rs `drop_at`.
    Field { g: PTy, at: (Value, i32) },
    /// A type's own `Drop::drop` (`instance`) on the value at `p`.
    Impl { instance: Instance, p: Value },
    /// The fields, live variant's fields or elements of the value of layout `l` at `p`: its
    /// glue after its own `Drop` ([`Walk::parts`]).
    Parts { l: u32, p: Value },
    /// The contents of the `Box` at `p` (`list` its arguments): walk.rs `drop_box_contents`.
    BoxContents { list: List, p: Value },
    /// The `Box` of ground type `g` at `p` without its contents: its `Drop` (the free), then its
    /// own fields (walk/moves/boxes.rs `drop_box_free`).
    BoxFree { g: PTy, p: Value },
    /// Elements `from..len` of type `elem` at `data`, `stride` bytes apart, first to last.
    Elements { elem: PTy, data: Value, from: Value, len: Value, stride: u32 },
    /// What the frame owes below a scope exit's entries: the waiting operands (top first), then
    /// the drop-scope entries `below - 1 ..= 0`. Owed only, never performed on a normal path.
    Frame { below: usize },
}

/// Where a drop call made now goes ([`Walk::drop_call`]).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Rung {
    /// No ladder step is being built: a plain call (a drop outside the seams).
    Off,
    /// A step is being built and none of its calls has asked for its landing yet.
    Unasked,
    /// A step whose landing would owe nothing: plain calls.
    Plain,
    /// A step whose calls land in this `cleanup` block.
    Lands(Block),
}

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// Whether drops built now get unwind ladders: the build unwinds, the block is live, it is
    /// not cleanup code, and the body may unwind.
    fn ladder_applies(&self) -> bool {
        self.adts.panic() == Panic::Unwind && self.live && !self.cleanup && !self.is_nounwind()
    }

    /// Performs `steps` in order; under a ladder each step's calls land in a block owing the
    /// steps after it, then what [`Self::owed`] holds already. `kill`: a scope ends with these
    /// entries (walk.rs `drop_held`), on the normal path only.
    pub(super) fn drops_with_ladder(&mut self, steps: &[DropStep], kill: bool) -> Result<(), BuildError> {
        if !self.ladder_applies() {
            for &step in steps {
                self.perform(step, kill)?;
            }
            return Ok(());
        }
        let base = self.owed.len();
        // Top first: steps[0] on top, then steps[1], ..., the last step deepest.
        for &step in steps.iter().rev() {
            self.owed.push(step);
        }
        let mut result = Ok(());
        for &step in steps {
            // The step itself leaves the stack: what is owed now is the steps after it.
            self.owed.pop();
            result = self.ladder_step(step, kill);
            if result.is_err() {
                break;
            }
        }
        self.owed.truncate(base);
        result
    }

    /// Performs `step` with `after` owed on its unwind path (and not performed here): one
    /// step of a loop whose remaining iterations are `after`.
    fn drop_owing(&mut self, step: DropStep, after: DropStep) -> Result<(), BuildError> {
        if !self.ladder_applies() {
            return self.perform(step, false);
        }
        let base = self.owed.len();
        self.owed.push(after);
        let result = self.ladder_step(step, false);
        self.owed.truncate(base);
        result
    }

    /// Performs one step of a ladder, its calls sharing one landing made at the first of them.
    fn ladder_step(&mut self, step: DropStep, kill: bool) -> Result<(), BuildError> {
        let saved = core::mem::replace(&mut self.rung, Rung::Unasked);
        let result = self.perform(step, kill);
        self.rung = saved;
        result
    }

    /// Performs `step` at the builder's current block.
    fn perform(&mut self, step: DropStep, kill: bool) -> Result<(), BuildError> {
        match step {
            DropStep::Held(h) => self.drop_held(h, kill),
            DropStep::Field { g, at } => self.drop_at(g, at),
            DropStep::Impl { instance, p } => self.call_ptr(instance, p),
            DropStep::Parts { l, p } => self.drop_parts(l as usize, p),
            DropStep::BoxContents { list, p } => self.drop_box_contents(list, p),
            DropStep::BoxFree { g, p } => self.drop_box_free(g, p),
            DropStep::Elements { elem, data, from, len, stride } => self.elements(elem, data, from, len, stride),
            DropStep::Frame { below } => {
                if !self.cleanup {
                    return Err(malformed("a frame's owed drops performed outside a landing block (a build bug)"));
                }
                if below > self.scope_pats.len() {
                    return Err(malformed("a frame's owed drops past the open drop scopes"));
                }
                self.drop_waiting()?;
                for j in (0..below).rev() {
                    let h = self.scope_pats[j];
                    self.drop_held(h, false)?;
                }
                Ok(())
            }
        }
    }

    /// Whether drop-scope entry `h` may be dropped here (walk.rs `pending_from`, per entry).
    pub(super) fn held_pending(&self, h: Held) -> bool {
        match h {
            Held::Rest { .. } | Held::Moved { .. } | Held::Temp { .. } => true,
            Held::EnvPlace(j) => self.env_place_pending(j),
            Held::Pat(p) => match self.owned.get(p.ix()).copied().and_then(super::LocalOwn::as_drop) {
                Some(Owned { flag: Some(_), .. }) => true,
                Some(Owned { flag: None, .. }) => self.init[p.ix()],
                None => false,
            },
        }
    }

    /// Whether the parts of layout `l` (its glue after its own `Drop`) make any call.
    fn parts_need_drop(&self, l: usize) -> bool {
        let layout = &self.layouts[l];
        match layout.shape {
            Shape::Fields { .. } if layout.is_union => false,
            Shape::Fields { first, len } => self.fields_need_drop(first, len),
            Shape::Enum { first, len, .. } => (first..first + len).any(|v| {
                let (ff, fl) = self.layout_variants[v as usize];
                self.fields_need_drop(ff, fl)
            }),
            Shape::Array { elem, .. } => self.repr_needs_drop(elem),
        }
    }

    /// Whether a landing made now would do anything: drop something owed, or (in an `async`
    /// body) poison the coroutine at its `resume`. Refuses, as `call_unwinding` does, a waiting
    /// operand whose ownership the walk could not ground (walk/val_stack.rs `waiting_owned`).
    fn owes_anything(&self) -> Result<bool, BuildError> {
        if self.co.is_some() {
            return Ok(true);
        }
        for i in 0..self.owed.len() {
            let owes = match self.owed[i] {
                DropStep::Held(h) => self.held_pending(h),
                DropStep::Parts { l, .. } => self.parts_need_drop(l as usize),
                DropStep::Frame { below } => {
                    let below = self.scope_pats.get(..below).ok_or(malformed("a frame's owed drops past the open drop scopes"))?;
                    self.waiting_owned()? || below.iter().any(|&h| self.held_pending(h))
                }
                DropStep::Field { .. }
                | DropStep::Impl { .. }
                | DropStep::BoxContents { .. }
                | DropStep::BoxFree { .. }
                | DropStep::Elements { .. } => true,
            };
            if owes {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// A drop call (a fn of one pointer parameter and no result): an `invoke` to the current
    /// step's landing when the ladder says so, to the `terminate` block in a nounwind body,
    /// else a plain call.
    pub(super) fn drop_call(&mut self, callee: Callee, sig: SigId, values: &[Value]) -> Result<(), BuildError> {
        let unwinds = self.adts.panic() == Panic::Unwind && self.live && !self.cleanup;
        if unwinds && self.is_nounwind() {
            self.call_terminating(callee, sig, values)?;
            return Ok(());
        }
        let (pad, fresh) = match (unwinds, self.rung) {
            (true, Rung::Lands(pad)) => (Some(pad), false),
            (true, Rung::Unasked) => {
                if self.owes_anything()? {
                    let pad = self.landing_block(BlockKind::Cleanup);
                    self.rung = Rung::Lands(pad);
                    (Some(pad), true)
                } else {
                    self.rung = Rung::Plain;
                    (None, false)
                }
            }
            _ => (None, false),
        };
        let Some(pad) = pad else {
            match callee {
                Callee::Direct(reference) => self.b.call(Flags::NONE, reference, sig, values),
                Callee::Pointer(p) => self.b.call_indirect(Flags::NONE, p, sig, values),
            };
            return Ok(());
        };
        let normal = self.create();
        self.reach(normal);
        self.reach(pad);
        match callee {
            Callee::Direct(reference) => self.b.invoke(Flags::NONE, reference, sig, values, normal, pad),
            Callee::Pointer(p) => self.b.invoke_indirect(Flags::NONE, p, sig, values, normal, pad),
        };
        if fresh {
            self.fill_ladder_pad(pad)?;
        }
        self.begin(normal);
        Ok(())
    }

    /// Fills landing block `pad`: its exception, every owed step top first as cleanup code
    /// (plain calls, state left as it was), then `resume`. The builder's current block must
    /// have its terminator.
    fn fill_ladder_pad(&mut self, pad: Block) -> Result<(), BuildError> {
        self.begin(pad);
        let exception = self.b.param(Ty::PTR);
        self.cleanup = true;
        let mut result = Ok(());
        // Cleanup code builds no ladder, so nothing pushes onto `owed` while it is read.
        for i in (0..self.owed.len()).rev() {
            let step = self.owed[i];
            result = self.perform(step, false);
            if result.is_err() {
                break;
            }
        }
        self.cleanup = false;
        result?;
        self.b.resume(exception);
        Ok(())
    }

    /// The exit of the drop scopes from `start` of `scope_pats` on: each entry, last declared
    /// first, a step of a ladder whose bottom is [`DropStep::Frame`] (the frame below `start`).
    /// `kill`: the scopes end (a block's end); `false` for an early exit (`break`, `continue`,
    /// `return`), which leaves the state of the code after it as it was.
    pub(super) fn drop_scope(&mut self, start: usize, kill: bool) -> Result<(), BuildError> {
        let n = self.scope_pats.len();
        if start > n {
            return Err(malformed("a scope exit past the open drop scopes"));
        }
        if !self.ladder_applies() {
            for i in (start..n).rev() {
                let h = self.scope_pats[i];
                self.drop_held(h, kill)?;
            }
            return Ok(());
        }
        let mut steps = Vec::with_capacity_in(n - start, self.alloc);
        for i in (start..n).rev() {
            steps.push(DropStep::Held(self.scope_pats[i]));
        }
        let base = self.owed.len();
        self.owed.push(DropStep::Frame { below: start });
        let result = self.drops_with_ladder(&steps, kill);
        self.owed.truncate(base);
        result
    }

    /// The drop glue of ground type `g` (layout `l`) at `p` after a `Box`'s contents, or of
    /// any other type: its own `Drop::drop`, whose landing drops all its parts, then its parts.
    pub(super) fn drop_impl_then_parts(&mut self, g: PTy, l: usize, p: Value) -> Result<(), BuildError> {
        let mut steps = Vec::with_capacity_in(2, self.alloc);
        if let Some(item) = self.layouts[l].drop_fn {
            let args = self.ground.intern_list(&[g]).map_err(|_| malformed("the ground table is full"))?;
            steps.push(DropStep::Impl { instance: Instance { item, args }, p });
        }
        let l = u32::try_from(l).map_err(|_| malformed("a layout index past u32"))?;
        steps.push(DropStep::Parts { l, p });
        self.drops_with_ladder(&steps, false)
    }

    /// The parts of the value of layout `l` at `p`: a struct's fields in declaration order
    /// (none of a union), the live variant's fields (a `switch` on the discriminant, as a
    /// `match` reads it, to each variant with a field that needs drop; every other variant and
    /// a niche's `None` drop nothing), an array's elements first to last.
    fn drop_parts(&mut self, l: usize, p: Value) -> Result<(), BuildError> {
        match self.layouts[l].shape {
            Shape::Fields { .. } if self.layouts[l].is_union => {}
            Shape::Fields { first, len } => self.drop_fields(p, first, len)?,
            Shape::Enum { id, first, len } => {
                let exit = self.create();
                let mut cases = Vec::with_capacity_in(len as usize, self.alloc);
                let mut bodies = Vec::with_capacity_in(len as usize, self.alloc);
                for v in 0..len {
                    let (ff, fl) = self.layout_variants[(first + v) as usize];
                    if !self.fields_need_drop(ff, fl) {
                        continue;
                    }
                    let block = self.create();
                    cases.push((self.discriminant_case(id, v)?, block));
                    bodies.push((block, ff, fl));
                }
                let d = self.read_discriminant(id, (p, 0))?;
                self.switch(d, exit, &cases);
                for &(block, ff, fl) in bodies.iter() {
                    self.begin(block);
                    self.drop_fields(p, ff, fl)?;
                    self.jump(exit, &[]);
                }
                self.begin(exit);
            }
            Shape::Array { elem: Repr::Agg(el), len, stride } if self.layouts[el as usize].needs_drop => {
                let et = self.layouts[el as usize].ty;
                let data = self.address((p, 0));
                let zero = self.b.iconst(Ty::I64, 0);
                let n = self.b.iconst(Ty::I64, u128::from(len));
                self.elements(et, data, zero, n, stride)?;
            }
            Shape::Array { .. } => {}
        }
        Ok(())
    }

    /// Drops each of `layout_fields[first..first + len]` that needs drop, at its offset from
    /// `p`, in declaration order, each a step of a ladder (field k's landing drops the fields
    /// after it).
    pub(super) fn drop_fields(&mut self, p: Value, first: u32, len: u32) -> Result<(), BuildError> {
        let mut steps = Vec::with_capacity_in(len as usize, self.alloc);
        for i in first..first + len {
            let (_, off, repr) = self.layout_fields[i as usize];
            if let Repr::Agg(fl) = repr
                && self.layouts[fl as usize].needs_drop
            {
                let off = i32::try_from(off).map_err(|_| malformed("a field offset past i32"))?;
                steps.push(DropStep::Field { g: self.layouts[fl as usize].ty, at: (p, off) });
            }
        }
        self.drops_with_ladder(&steps, false)
    }

    /// A LIR loop over elements `from..len` (`i64`s) of ground type `elem` at `data`, `stride`
    /// bytes apart, first to last: each element's glue, whose landing drops the elements after
    /// it by this same loop (from `i + 1`). Ends in the block after the loop.
    pub(super) fn elements(&mut self, elem: PTy, data: Value, from: Value, len: Value, stride: u32) -> Result<(), BuildError> {
        let header = self.create();
        let body = self.create();
        let exit = self.create();
        self.jump(header, &[from]);
        self.begin(header);
        let i = self.b.param(Ty::I64);
        let more = self.b.icmp(IntCond::Ult, i, len);
        self.br(more, (body, &[]), (exit, &[]));
        self.begin(body);
        let s = self.b.iconst(Ty::I64, u128::from(stride));
        let off = self.b.binary(Op::Mul, Flags::NONE, i, s);
        let ep = self.b.ptr_add(Flags::NONE, data, off);
        let one = self.b.iconst(Ty::I64, 1);
        let next = self.b.binary(Op::Add, Flags::NONE, i, one);
        self.drop_owing(DropStep::Field { g: elem, at: (ep, 0) }, DropStep::Elements { elem, data, from: next, len, stride })?;
        self.jump(header, &[next]);
        self.begin(exit);
        Ok(())
    }
}
