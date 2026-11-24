//! Terminators: goto, switch, return through the calling convention, calls (direct by an
//! instance's reference, or through a fn pointer), asserts, drops, resume and unreachable
//! (lib.rs, "Unwinding").

use blaze_own::Vec;
use core::alloc::Allocator;

use blaze_body::{AssertKind, BinOp, Block as BodyBlock, Callee, CalleeId, Operand, Ops, Place, StmtKind, Unwind};
use blaze_build::intrinsics::target::{Target as IntrinsicTarget, float::FloatCall};
use blaze_build::intrinsics::{Arg, Call, GenericArg, IntrinsicCx, Out, Which};
use blaze_build::layout::{LayoutCx, LayoutId};
use blaze_build::vtable::{self, HEADER_WORDS};
use blaze_build::{Abi, Instance, Intrinsic, OverflowChecks, Panic, PanicConst, abi};
use blaze_lir::builder::{MemAttrs, Variadic};
use blaze_lir::func::{BlockKind, Conv, RefId, SigId, SlotKind};
use blaze_lir::{Block, Flags, IntCond, Ty, Value};
use blaze_typeck::{ItemKey, List, Mutbl, PNode, PTy};

use crate::value::At;
use crate::{BuildError, Cx, Pass, V};

/// Maps a fn-pointer's surface ABI onto the calling convention plan (matching `blaze-build`).
fn pointer_abi(abi: blaze_typeck::Abi) -> Option<Abi> {
    match abi {
        blaze_typeck::Abi::Rust
        | blaze_typeck::Abi::RustCold
        | blaze_typeck::Abi::RustPreserveNone
        | blaze_typeck::Abi::RustTail => Some(Abi::Rust),
        blaze_typeck::Abi::C | blaze_typeck::Abi::CUnwind => Some(Abi::C),
        blaze_typeck::Abi::RustCall => Some(Abi::RustCall),
        _ => None,
    }
}

/// What a call calls.
#[derive(Clone, Copy)]
enum Target {
    Direct(RefId),
    /// A fn pointer's value.
    Pointer(Value),
}

/// Where an unwinding call goes.
#[derive(Clone, Copy)]
enum Landing {
    /// The body's cleanup block.
    Cleanup(BodyBlock),
    /// A landing that calls `panic_cannot_unwind`.
    Terminate,
}

/// Where a call's result goes.
#[derive(Clone, Copy)]
enum ResultTo {
    /// Nowhere: a zero-sized result, or a call that does not return.
    Nothing,
    /// The call's one value, to `dest`.
    Value { dest: Place, lay: LayoutId },
    /// The returned data pointer and its metadata, to `dest`.
    Wide { dest: Place, lay: LayoutId },
    /// Already written through the hidden result pointer, at `dest` itself (`temp: None`) or
    /// at a temporary copied to `dest` after the call.
    Memory { dest: Place, lay: LayoutId, temp: Option<Value> },
    /// C composite result of 1..=16 bytes returned in 1 or 2 `i64` registers.
    Words { dest: Place, lay: LayoutId, size: u32 },
}

impl<A: Allocator + Copy> Cx<'_, '_, '_, A> {
    /// The terminator `kind`, the last row of its block.
    pub(crate) fn terminator(&mut self, kind: StmtKind) -> Result<(), BuildError> {
        match kind {
            StmtKind::Goto(to) => self.goto(to),
            StmtKind::Switch { on, cases, otherwise } => self.switch(on, cases, otherwise),
            StmtKind::Return => self.ret(),
            StmtKind::Call { callee, args, dest, target, unwind } => self.call(callee, args, dest, target, unwind),
            StmtKind::Drop { place, target, unwind } => self.drop_place(place, target, unwind),
            StmtKind::Assert { cond, expected, kind, msg, target, unwind } => self.assert(cond, expected, kind, msg, target, unwind),
            StmtKind::Unreachable => {
                self.b.unreachable();
                Ok(())
            }
            StmtKind::Resume => {
                let exn = self.exn.ok_or(self.malformed("a resume outside a cleanup block"))?;
                self.b.resume(exn);
                Ok(())
            }
            StmtKind::Terminate => self.cannot_unwind(),
            StmtKind::Suspend { .. } => Err(self.unsupported("a suspension (a coroutine's body is split first)")),
            StmtKind::InlineAsm { .. } => Err(self.unsupported("an `asm!` block")),
            StmtKind::Assign { .. }
            | StmtKind::StorageLive(_)
            | StmtKind::StorageDead(_)
            | StmtKind::Activate(_)
            | StmtKind::FakeRead(_)
            | StmtKind::SetDiscriminant { .. }
            | StmtKind::Deinit(_) => Err(self.malformed("a statement as a block's terminator")),
        }
    }

    fn goto(&mut self, to: BodyBlock) -> Result<(), BuildError> {
        let block = self.lir_block(to)?;
        let args = self.args_for(to)?;
        self.b.jump((block, &args));
        Ok(())
    }

    fn switch(&mut self, on: Operand, cases: Ops, otherwise: BodyBlock) -> Result<(), BuildError> {
        let (x, _) = self.operand(on)?;
        let x = self.scalar(x)?;
        let default = (self.lir_block(otherwise)?, self.args_for(otherwise)?);
        let mut targets: Vec<(u128, Block, Vec<Value, A>), A> = Vec::with_capacity_in(cases.len() as usize, self.alloc);
        let rows: Vec<(u128, BodyBlock), A> = {
            let mut out = Vec::with_capacity_in(cases.len() as usize, self.alloc);
            out.extend(self.v.cases(cases));
            out
        };
        for &(value, to) in rows.iter() {
            targets.push((value, self.lir_block(to)?, self.args_for(to)?));
        }
        let mut arms: Vec<(u128, (Block, &[Value])), A> = Vec::with_capacity_in(targets.len(), self.alloc);
        for (value, block, args) in targets.iter() {
            arms.push((*value, (*block, args.as_slice())));
        }
        self.b.switch(x, (default.0, &default.1), &arms);
        Ok(())
    }

    /// Return the registers of `_0`. An ignored or indirect (`Pass::Ref` / `sret`) result has no
    /// return registers; a C composite of 1..=16 bytes returns 1 or 2 `i64` words.
    fn ret(&mut self) -> Result<(), BuildError> {
        match self.ret {
            Pass::Ignore | Pass::Ref | Pass::Memory { .. } => self.b.ret(&[]),
            Pass::Scalar { .. } => {
                let place = self.v.local_place(blaze_body::Local(0));
                let (x, _) = self.operand(Operand::Move(place))?;
                let x = self.scalar(x)?;
                self.b.ret(&[x]);
            }
            Pass::Wide { .. } => {
                let place = self.v.local_place(blaze_body::Local(0));
                let (x, _) = self.operand(Operand::Move(place))?;
                let values = self.wide_values(x)?;
                self.b.ret(&values);
            }
            Pass::Words { size } => {
                let place = self.v.local_place(blaze_body::Local(0));
                let (x, _) = self.operand(Operand::Move(place))?;
                let lay = *self.lays.first().ok_or(self.malformed("a body without its return local"))?;
                let align = self.layouts.get(lay).align;
                let (ptr, off) = self.in_memory(x, lay)?;
                let w0 = self.read_word(ptr, off, 0, size.min(8), align)?;
                if size > 8 {
                    let w1 = self.read_word(ptr, off, 8, size - 8, align)?;
                    self.b.ret(&[w0, w1]);
                } else {
                    self.b.ret(&[w0]);
                }
            }
        }
        Ok(())
    }

    /// The landing an unwinding call takes, if it is an `invoke`: only when panics unwind, the
    /// callee may unwind and the row names somewhere to go.
    fn landing(&self, unwind: Unwind, may_unwind: bool) -> Option<Landing> {
        if self.panic == Panic::Abort || !may_unwind {
            return None;
        }
        match unwind {
            Unwind::Cleanup(block) => Some(Landing::Cleanup(block)),
            Unwind::Terminate => Some(Landing::Terminate),
            Unwind::Continue | Unwind::Unreachable => None,
        }
    }

    /// The instance of fn item `item` at the body's arguments `args` (of the body's table),
    /// grounded.
    fn instance(&mut self, item: ItemKey, args: List) -> Result<Instance, BuildError> {
        let from = self.from;
        let v = self.v;
        let tys = v.ty_list(&from, args).map_err(|_| self.malformed("a callee's arguments past the body's tables"))?;
        let mut ground: Vec<blaze_typeck::PTy, A> = Vec::with_capacity_in(tys.len(), self.alloc);
        for t in tys.iter() {
            ground.push(self.ground(t)?);
        }
        let args = self.subst.ground.intern_list(&ground).map_err(|_| self.malformed("the ground table is full"))?;
        Ok(Instance { item, args })
    }

    /// The instance of lang fn `name` at `args` (ground), refusing a `#[track_caller]` one.
    fn lang_instance(&mut self, name: &'static str, args: &[blaze_typeck::PTy]) -> Result<(Instance, bool), BuildError> {
        let stmt = self.row()?;
        let item = self.adts.lang_fn(name).ok_or(BuildError::NoLangFn { stmt, name })?;
        let args = self.subst.ground.intern_list(args).map_err(|_| self.malformed("the ground table is full"))?;
        let instance = Instance { item, args };
        if self.adts.track_caller(instance, self.subst.ground) == Some(true) {
            return Err(BuildError::TrackCaller { stmt, name });
        }
        Ok((instance, self.adts.nounwind(item) != Some(true)))
    }

    /// Append the registers for argument `x` of layout `lay`: a scalar, the two words of a
    /// wide pointer, the address of a copy (`Pass::Ref` / C `Pass::Memory`), or 1-2 `i64` words
    /// for a small C composite (`Pass::Words`).
    fn pass_arg(&mut self, x: V, lay: LayoutId, pass: Pass, moved: bool, out: &mut Vec<Value, A>) -> Result<(), BuildError> {
        match pass {
            Pass::Ignore => {}
            Pass::Scalar { .. } => out.push(self.scalar(x)?),
            Pass::Wide { .. } => out.extend(self.wide_values(x)?),
            Pass::Ref | Pass::Memory { .. } => {
                let (ptr, off) = match (x, moved) {
                    (V::Mem { ptr, off, .. }, true) => (ptr, off),
                    (V::Mem { .. }, false) => {
                        let slot = self.new_slot(lay, SlotKind::Temp)?;
                        let to = self.b.slot_addr(slot);
                        self.store_v(to, 0, lay, x)?;
                        (to, 0)
                    }
                    (x, _) => self.in_memory(x, lay)?,
                };
                out.push(self.addr(ptr, off));
            }
            Pass::Words { size } => {
                let align = self.layouts.get(lay).align;
                let (ptr, off) = self.in_memory(x, lay)?;
                let w0 = self.read_word(ptr, off, 0, size.min(8), align)?;
                out.push(w0);
                if size > 8 {
                    let w1 = self.read_word(ptr, off, 8, size - 8, align)?;
                    out.push(w1);
                }
            }
        }
        Ok(())
    }

    /// Lowers a compiler intrinsic (`Intrinsic::Table(which)`) directly via
    /// `blaze_build::intrinsics::build`, sharing all 100+ intrinsics with zero duplication.
    fn table_intrinsic(
        &mut self,
        which: Which,
        fn_args: List,
        args: Ops,
        dest: Option<Place>,
        target: Option<BodyBlock>,
    ) -> Result<(), BuildError> {
        let stmt = self.row()?;
        let from = self.from;
        let v = self.v;
        let generic_tys = v.ty_list(&from, fn_args).map_err(|_| self.malformed("an intrinsic's generic arguments past the body's tables"))?;
        let mut generics: Vec<GenericArg, A> = Vec::with_capacity_in(generic_tys.len(), self.alloc);
        for t in generic_tys.iter() {
            let g = self.ground(t)?;
            let arg = match self.node(g)? {
                PNode::ConstValue { ty, bits } => match self.node(ty)? {
                    PNode::Bool | PNode::Int(_) | PNode::Uint(_) | PNode::Char => GenericArg::Const(bits),
                    PNode::Adt(adt, _) => {
                        let variant = u32::try_from(bits).map_err(|_| self.malformed("an enum const variant past u32"))?;
                        let d = self.adts.facts().discriminant(adt, variant)
                            .map_err(|_| self.malformed("an enum const generic without its discriminant"))?
                            .unwrap_or(i128::from(variant));
                        GenericArg::Const(d.cast_unsigned())
                    }
                    _ => return Err(self.unsupported("a const generic argument of an unsupported type in an intrinsic")),
                },
                PNode::ConstParam(_) => return Err(self.malformed("a const parameter left in an intrinsic's grounded arguments")),
                _ => GenericArg::Ty(g),
            };
            generics.push(arg);
        }
        let ops: Vec<Operand, A> = {
            let cap = usize::try_from(args.len()).unwrap_or(0);
            let mut out = Vec::with_capacity_in(cap, self.alloc);
            out.extend(self.v.ops(args));
            out
        };
        let mut cargs: Vec<Arg, A> = Vec::with_capacity_in(ops.len(), self.alloc);
        for &o in ops.iter() {
            let (x, g) = self.operand(o)?;
            let lay = self.layout(g)?;
            let l = *self.layouts.get(lay);
            let arg = if l.is_zst() {
                Arg::None
            } else if l.scalar.is_some() {
                Arg::Scalar(self.scalar(x)?)
            } else {
                let (ptr, off) = self.in_memory(x, lay)?;
                Arg::Place(self.addr(ptr, off))
            };
            cargs.push(arg);
        }
        let ret = match dest {
            Some(dest) => self.place_ty(dest)?,
            None => self.subst.ground.intern(PNode::Never).map_err(|_| self.malformed("the ground table is full"))?,
        };
        let answer = match (which, generics.first()) {
            (Which::NeedsDrop, Some(&GenericArg::Ty(t0))) => self.needs_drop_of(t0)?,
            _ => false,
        };
        let needs_drop = move |_: PTy| answer;
        let call = Call { which, generics: &generics, args: &cargs, ret, site: None };
        let adts = self.adts;
        let out = {
            let mut lcx = LayoutCx { ground: &mut *self.subst.ground, facts: adts.facts(), scratch: self.subst.scratch };
            let mut icx = IntrinsicCx {
                layouts: &mut self.layouts,
                cx: &mut lcx,
                needs_drop: &needs_drop,
                ub_checks: adts.ub_checks(),
                overflow_checks: adts.overflow_checks(),
            };
            blaze_build::intrinsics::build(&mut self.b, &call, &mut icx)
        }
        .map_err(|error| BuildError::Intrinsic { stmt, error })?;
        match out {
            Out::Diverges => {
                self.b.unreachable();
                return Ok(());
            }
            Out::None => {}
            Out::Scalar(val) => {
                if let Some(dest) = dest {
                    let lay = self.dest_layout(dest)?;
                    self.write(dest, V::One(val), lay)?;
                }
            }
            Out::Pair(a, b) => {
                if let Some(dest) = dest {
                    let dlay = self.dest_layout(dest)?;
                    let dl = *self.layouts.get(dlay);
                    if dl.pair.is_some() {
                        self.write(dest, V::Two(a, b), dlay)?;
                    } else {
                        let dty = self.place_ty(dest)?;
                        let PNode::Tuple(flist) = self.node(dty)? else {
                            return Err(self.malformed("an intrinsic pair result written to a non-pair place"));
                        };
                        let &[t0, t1] = self.subst.ground.view().list(flist).map_err(|_| self.malformed("a tuple's fields past the ground table"))? else {
                            return Err(self.malformed("an intrinsic pair result written to a non-2-tuple"));
                        };
                        let (l0, l1) = (self.layout(t0)?, self.layout(t1)?);
                        self.write_fields(dest, dlay, None, &[(0, V::One(a), l0), (1, V::One(b), l1)])?;
                    }
                }
            }
            Out::At(addr) => {
                if let Some(dest) = dest {
                    let lay = self.dest_layout(dest)?;
                    let v = self.load_at(addr, 0, lay)?;
                    self.write(dest, v, lay)?;
                }
            }
        }
        match target {
            Some(to) => self.goto(to),
            None => {
                self.b.unreachable();
                Ok(())
            }
        }
    }

    /// Lowers a scalar floating-point intrinsic (`llvm.fma.*`, `llvm.sqrt.*`, `llvm.fabs.*`,
    /// etc.) directly to its native hardware LIR instruction, with zero call overhead.
    fn float_intrinsic(&mut self, fc: FloatCall, args: Ops, dest: Option<Place>, target: Option<BodyBlock>) -> Result<(), BuildError> {
        if fc.shape.lanes().is_some() {
            return Err(self.unsupported("a vector float intrinsic call"));
        }
        let arity = fc.func.arity();
        if usize::try_from(args.len()).ok() != Some(arity) {
            return Err(self.malformed("a float intrinsic call with the wrong number of arguments"));
        }
        let ops: Vec<Operand, A> = {
            let mut out = Vec::with_capacity_in(arity, self.alloc);
            out.extend(self.v.ops(args));
            out
        };
        let mut vals = [Value(0); 3];
        let elem = fc.shape.elem();
        for (slot, &o) in vals.iter_mut().zip(ops.iter()) {
            let (x, _) = self.operand(o)?;
            let v = self.scalar(x)?;
            if self.b.ty(v) != elem {
                return Err(self.malformed("a float intrinsic argument of the wrong scalar type"));
            }
            *slot = v;
        }
        let [v0, v1, v2] = vals;
        let op = fc.func.op();
        let res = match arity {
            1 => self.b.unary(op, Flags::NONE, v0),
            2 => self.b.binary(op, Flags::NONE, v0, v1),
            3 => self.b.ternary(op, Flags::NONE, v0, v1, v2),
            _ => return Err(self.malformed("a float intrinsic of unsupported arity")),
        };
        if let Some(dest) = dest {
            let lay = self.dest_layout(dest)?;
            self.write(dest, V::One(res), lay)?;
        }
        match target {
            Some(to) => self.goto(to),
            None => {
                self.b.unreachable();
                Ok(())
            }
        }
    }

    /// Checks whether trait method `instance` dispatches through a `dyn Trait` vtable: returns
    /// `Some(slot)` when `Self` (its first generic argument) is `PNode::Dynamic(_)` and the
    /// method has a vtable slot on the object's principal/supertrait tree.
    fn dyn_vtable_slot(&self, instance: Instance) -> Result<Option<u32>, BuildError> {
        let view = self.subst.ground.view();
        let Some(&self_ty) = view.list(instance.args).map_err(|_| self.malformed("a callee's arguments past the ground table"))?.first() else {
            return Ok(None);
        };
        if !matches!(self.node(self_ty)?, PNode::Dynamic(_)) {
            return Ok(None);
        }
        let Some((principal, _)) = self.principal(self_ty)? else {
            return Ok(None);
        };
        let shape = vtable::Shape::of(principal, |t| self.adts.object_trait(t), self.subst.scratch)
            .map_err(|error| BuildError::Vtable { stmt: self.stmt, error })?;
        if let Some((slot, _by_value)) = shape.slot_info(instance.item) {
            return Ok(Some(slot));
        }
        let traits = shape.prefix.iter().copied().chain(shape.words.iter().filter_map(|w| match *w {
            vtable::Word::Super { trait_, .. } => Some(trait_),
            vtable::Word::Method { .. } => None,
        }));
        for t in traits {
            let o = self.adts.object_trait(t).ok_or(BuildError::Vtable { stmt: self.stmt, error: vtable::VtableError::NoTrait(t) })?;
            if o.methods.iter().any(|m| m.item == instance.item) {
                return Err(self.unsupported("a method of the object's own trait with no vtable slot"));
            }
        }
        Ok(None)
    }

    fn call(&mut self, callee: CalleeId, args: Ops, dest: Option<Place>, target: Option<BodyBlock>, unwind: Unwind) -> Result<(), BuildError> {
        let ops: Vec<Operand, A> = {
            let mut out = Vec::with_capacity_in(args.len() as usize, self.alloc);
            out.extend(self.v.ops(args));
            out
        };
        let mut dyn_slot: Option<u32> = None;
        let mut variadic = Variadic::No;
        let (to, call_abi, may_unwind) = match self.v.callee(callee) {
            Callee::FnItem { key, args: fn_args } => {
                match self.adts.intrinsic(key) {
                    Some(Intrinsic::Target(IntrinsicTarget::Float(fc))) => {
                        return self.float_intrinsic(fc, args, dest, target);
                    }
                    Some(Intrinsic::Table(which)) => {
                        return self.table_intrinsic(which, fn_args, args, dest, target);
                    }
                    Some(Intrinsic::Target(_)) => {
                        return Err(self.unsupported("a target-specific non-float intrinsic call"));
                    }
                    None => {}
                }
                let abi = self.adts.abi(key).ok_or(BuildError::NoAbi { item: key })?;
                if self.adts.c_variadic(key).is_some() {
                    variadic = Variadic::Yes;
                }
                let instance = self.instance(key, fn_args)?;
                if self.adts.track_caller(instance, self.subst.ground) == Some(true) {
                    return Err(BuildError::TrackCaller { stmt: self.row()?, name: "the callee" });
                }
                (Target::Direct(self.func_ref(instance)), abi, self.adts.nounwind(key) != Some(true))
            }
            Callee::Method { key, args: fn_args } => {
                let abi = self.adts.abi(key).ok_or(BuildError::NoAbi { item: key })?;
                let instance = self.instance(key, fn_args)?;
                if self.adts.track_caller(instance, self.subst.ground) == Some(true) {
                    return Err(BuildError::TrackCaller { stmt: self.row()?, name: "the callee" });
                }
                let may_unwind = self.adts.nounwind(key) != Some(true);
                if let Some(slot) = self.dyn_vtable_slot(instance)? {
                    dyn_slot = Some(slot);
                    // Placeholder target; replaced below after evaluating receiver's `(data, vtable)`.
                    (Target::Direct(RefId(0)), abi, may_unwind)
                } else {
                    (Target::Direct(self.func_ref(instance)), abi, may_unwind)
                }
            }
            Callee::Value(o) => {
                let (x, g) = self.operand(o)?;
                let abi = match self.node(g)? {
                    PNode::FnPtr(_, header) => {
                        pointer_abi(header.abi).ok_or(self.unsupported("a fn pointer of an unsupported ABI"))?
                    }
                    _ => Abi::Rust,
                };
                (Target::Pointer(self.scalar(x)?), abi, true)
            }
            Callee::Intrinsic { key, args: fn_args } => match self.adts.intrinsic(key) {
                Some(Intrinsic::Target(IntrinsicTarget::Float(fc))) => {
                    return self.float_intrinsic(fc, args, dest, target);
                }
                Some(Intrinsic::Table(which)) => {
                    return self.table_intrinsic(which, fn_args, args, dest, target);
                }
                _ => return Err(self.unsupported("an intrinsic call")),
            },
        };
        let mut passes: Vec<Pass, A> = Vec::with_capacity_in(ops.len(), self.alloc);
        let mut values: Vec<Value, A> = Vec::with_capacity_in(ops.len() + 1, self.alloc);
        // The result: through a hidden first pointer (`Pass::Ref` or C `sret`), as 1-2 `i64`
        // words (`Pass::Words`), or as the call's value.
        let (rpass, result) = match dest {
            None => (Pass::Ignore, ResultTo::Nothing),
            Some(dest) => {
                let lay = self.dest_layout(dest)?;
                let g = self.place_ty(dest)?;
                let rpass = self.pass_of_abi(g, call_abi)?;
                let result = match rpass {
                    Pass::Ignore => ResultTo::Nothing,
                    Pass::Scalar { .. } => ResultTo::Value { dest, lay },
                    Pass::Wide { .. } => ResultTo::Wide { dest, lay },
                    Pass::Ref | Pass::Memory { .. } => {
                        let (ptr, temp) = match self.dest_memory(dest)? {
                            Some(ptr) => (ptr, None),
                            None => {
                                let slot = self.new_slot(lay, SlotKind::Temp)?;
                                let ptr = self.b.slot_addr(slot);
                                (ptr, Some(ptr))
                            }
                        };
                        values.push(ptr);
                        ResultTo::Memory { dest, lay, temp }
                    }
                    Pass::Words { size } => ResultTo::Words { dest, lay, size },
                };
                (rpass, result)
            }
        };
        let mut final_target = to;
        let regular_ops = if call_abi == Abi::RustCall && !ops.is_empty() { ops.len() - 1 } else { ops.len() };
        for (idx, &o) in ops.iter().take(regular_ops).enumerate() {
            let (x, g) = self.operand(o)?;
            if idx == 0
                && let Some(slot) = dyn_slot
            {
                let [data, vt] = self.wide_values(x)?;
                let off = 8 * (HEADER_WORDS + slot);
                let off = i32::try_from(off).map_err(|_| self.malformed("a vtable slot past 2^31 bytes"))?;
                let fn_ptr = self.b.load(Ty::PTR, Flags::NONE, vt, off, MemAttrs::aligned(3));
                final_target = Target::Pointer(fn_ptr);
                let ptr_pass = Pass::Scalar { ty: Ty::PTR, ext: blaze_lir::func::Ext::None, agg: false };
                values.push(data);
                passes.push(ptr_pass);
                continue;
            }
            let lay = self.layout(g)?;
            let pass = self.pass_of_abi(g, call_abi)?;
            self.pass_arg(x, lay, pass, matches!(o, Operand::Move(_)), &mut values)?;
            passes.push(pass);
        }
        // `Abi::RustCall`: spread the last tuple argument's fields as individual Rust ABI arguments.
        if call_abi == Abi::RustCall
            && let Some(&last_op) = ops.last()
        {
            let (x, g) = self.operand(last_op)?;
            let tlay = self.layout(g)?;
            let PNode::Tuple(flist) = self.node(g)? else {
                return Err(self.malformed("a \"rust-call\" call whose last argument is not a tuple"));
            };
            let field_tys: Vec<PTy, A> = {
                let cap = usize::try_from(flist.len).unwrap_or(0);
                let mut out = Vec::with_capacity_in(cap, self.alloc);
                let slice = self.subst.ground.view().list(flist).map_err(|_| self.malformed("a tuple's fields past the ground table"))?;
                out.extend_from_slice(slice);
                out
            };
            if !field_tys.is_empty() {
                let (ptr, off) = self.in_memory(x, tlay)?;
                for (i, &fty) in field_tys.iter().enumerate() {
                    let flay = self.layout(fty)?;
                    let idx_u32 = u32::try_from(i).map_err(|_| self.malformed("a spread tuple field index past u32"))?;
                    let foff = self.layouts.field_offset(tlay, idx_u32)
                        .and_then(|o| i32::try_from(o).ok())
                        .ok_or(self.malformed("a spread tuple field past its layout"))?;
                    let fv = self.load_at(ptr, off + foff, flay)?;
                    let fpass = self.pass_of(fty)?;
                    self.pass_arg(fv, flay, fpass, matches!(last_op, Operand::Move(_)), &mut values)?;
                    passes.push(fpass);
                }
            }
        }
        let (params, rets) = abi::sig_of_in(&passes, rpass, self.alloc);
        let sig = self.b.sig(Conv::C, variadic, &params, &rets);
        let landing = self.landing(unwind, may_unwind);
        self.emit(final_target, sig, &values, landing, result, target)
    }

    /// The address of `dest` when it is memory already (`None`: a value local).
    fn dest_memory(&mut self, dest: Place) -> Result<Option<Value>, BuildError> {
        let row = self.v.place(dest);
        if row.depth == 0 {
            let i = self.ix(row.local)?;
            return match *self.held.get(i).ok_or(self.malformed("a local past the frame"))? {
                crate::Held::Slot(_) | crate::Held::At(_) => Ok(Some(self.base(row.local)?)),
                _ => Ok(None),
            };
        }
        match self.resolve(dest)?.0 {
            At::Mem { ptr, off, .. } => Ok(Some(self.addr(ptr, off))),
            At::Val(_) => Err(self.malformed("a projection written through a value local")),
        }
    }

    /// The ground type of place `p`.
    fn place_ty(&mut self, p: Place) -> Result<blaze_typeck::PTy, BuildError> {
        let row = self.v.place(p);
        if row.depth == 0 {
            let i = self.ix(row.local)?;
            return self.tys.get(i).copied().ok_or(self.malformed("a local past the frame"));
        }
        Ok(self.resolve(p)?.1)
    }

    /// The call itself, then its result and its edge on: a plain call, or an `invoke` whose
    /// normal block writes the result and whose landing goes to the cleanup.
    fn emit(&mut self, to: Target, sig: SigId, values: &[Value], landing: Option<Landing>, result: ResultTo, target: Option<BodyBlock>) -> Result<(), BuildError> {
        let r = match landing {
            None => match to {
                Target::Direct(r) => self.b.call(Flags::NONE, r, sig, values),
                Target::Pointer(p) => self.b.call_indirect(Flags::NONE, p, sig, values),
            },
            Some(landing) => {
                // What the cleanup block takes, as the call's block holds it.
                let carried = match landing {
                    Landing::Cleanup(_) => self.local_args()?,
                    Landing::Terminate => Vec::new_in(self.alloc),
                };
                let normal = self.b.create_block(BlockKind::Normal);
                let pad = self.b.create_block(match landing {
                    Landing::Cleanup(_) => BlockKind::Cleanup,
                    Landing::Terminate => BlockKind::Terminate,
                });
                let r = match to {
                    Target::Direct(r) => self.b.invoke(Flags::NONE, r, sig, values, normal, pad),
                    Target::Pointer(p) => self.b.invoke_indirect(Flags::NONE, p, sig, values, normal, pad),
                };
                self.b.begin(pad);
                let exn = self.b.param(Ty::PTR);
                match landing {
                    Landing::Cleanup(block) => {
                        let lir = self.lir_block(block)?;
                        let mut args: Vec<Value, A> = Vec::with_capacity_in(carried.len() + 1, self.alloc);
                        args.push(exn);
                        args.extend(carried.iter().copied());
                        self.b.jump((lir, &args));
                    }
                    Landing::Terminate => self.cannot_unwind()?,
                }
                self.b.begin(normal);
                r
            }
        };
        match result {
            ResultTo::Nothing => {}
            ResultTo::Value { dest, lay } => self.write(dest, V::One(r), lay)?,
            ResultTo::Wide { dest, lay } => {
                let data = self.b.project(r, 0);
                let metadata = self.b.project(r, 1);
                self.write(dest, V::Two(data, metadata), lay)?;
            }
            ResultTo::Memory { temp: None, .. } => {}
            ResultTo::Memory { dest, lay, temp: Some(ptr) } => {
                let x = self.load_at(ptr, 0, lay)?;
                self.write(dest, x, lay)?;
            }
            ResultTo::Words { dest, lay, size } => {
                let align = self.layouts.get(lay).align;
                let (ptr, is_temp) = match self.dest_memory(dest)? {
                    Some(ptr) => (ptr, false),
                    None => {
                        let slot = self.new_slot(lay, SlotKind::Temp)?;
                        (self.b.slot_addr(slot), true)
                    }
                };
                if size > 8 {
                    let w0 = self.b.project(r, 0);
                    let w1 = self.b.project(r, 1);
                    self.write_word(w0, ptr, 0, 0, 8, align);
                    self.write_word(w1, ptr, 0, 8, size - 8, align);
                } else {
                    self.write_word(r, ptr, 0, 0, size, align);
                }
                if is_temp {
                    let x = self.load_at(ptr, 0, lay)?;
                    self.write(dest, x, lay)?;
                }
            }
        }
        match target {
            Some(t) => self.goto(t),
            None => {
                self.b.unreachable();
                Ok(())
            }
        }
    }

    /// A call of `panic_cannot_unwind`, which does not return: a `Terminate` row or landing.
    fn cannot_unwind(&mut self) -> Result<(), BuildError> {
        let (instance, _) = self.lang_instance("panic_cannot_unwind", &[])?;
        let r = self.func_ref(instance);
        let sig = self.b.sig(Conv::C, Variadic::No, &[], &[]);
        self.b.call(Flags::NONE, r, sig, &[]);
        self.b.unreachable();
        Ok(())
    }

    /// `drop place`: a call of the type's drop glue (`drop_in_place::<T>`) with the row's unwind
    /// edge; a jump when the type needs no drop.
    fn drop_place(&mut self, place: Place, target: BodyBlock, unwind: Unwind) -> Result<(), BuildError> {
        let (at, g) = self.resolve(place)?;
        if !self.needs_drop_of(g)? {
            return self.goto(target);
        }
        let At::Mem { ptr, off, meta, .. } = at else {
            return Err(self.malformed("a drop of a value local (the frame holds it in memory)"));
        };
        let (instance, may_unwind) = self.lang_instance("drop_in_place", &[g])?;
        let r = self.func_ref(instance);
        // The argument: a `*mut T`, wide when `T` is unsized.
        let pointer = self.subst.ground.intern(PNode::Ptr(Mutbl::Mut, g)).map_err(|_| self.malformed("the ground table is full"))?;
        let plain = self.layout(pointer)?;
        let pass = self.pass_of(pointer)?;
        let address = self.addr(ptr, off);
        let x = match meta {
            Some(m) => V::Two(address, m),
            None => V::One(address),
        };
        let mut values: Vec<Value, A> = Vec::with_capacity_in(1, self.alloc);
        self.pass_arg(x, plain, pass, false, &mut values)?;
        let (params, rets) = abi::sig_of_in(&[pass], Pass::Ignore, self.alloc);
        let sig = self.b.sig(Conv::C, Variadic::No, &params, &rets);
        let landing = self.landing(unwind, may_unwind);
        self.emit(Target::Direct(r), sig, &values, landing, ResultTo::Nothing, Some(target))
    }

    /// `assert cond == expected`: a compare, a branch to `target`, and a cold block that calls
    /// the panic entry `kind` names with the row's unwind edge. When `OverflowChecks::Off`,
    /// arithmetic overflow asserts for `+ - * << >>` and unary `-` are elided (division/rem
    /// overflow, division/rem by zero, and bounds checks always stay checked, per Rust).
    fn assert(&mut self, cond: Operand, expected: bool, kind: AssertKind, msg: Ops, target: BodyBlock, unwind: Unwind) -> Result<(), BuildError> {
        if self.adts.overflow_checks() == OverflowChecks::Off
            && matches!(
                kind,
                AssertKind::Overflow(BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Shl | BinOp::Shr)
                    | AssertKind::OverflowNeg
            )
        {
            return self.goto(target);
        }
        let (c, _) = self.operand(cond)?;
        let c = self.scalar(c)?;
        let zero = self.b.iconst(self.b.ty(c), 0);
        let holds = self.b.icmp(if expected { IntCond::Ne } else { IntCond::Eq }, c, zero);
        let to = self.lir_block(target)?;
        let args = self.args_for(target)?;
        let cold = self.b.create_block(BlockKind::Cold);
        self.b.br(Flags::NONE, holds, (to, &args), (cold, &[]));
        self.b.begin(cold);
        let panic = match kind {
            AssertKind::Overflow(op) => match op {
                BinOp::Add => PanicConst::AddOverflow,
                BinOp::Sub => PanicConst::SubOverflow,
                BinOp::Mul => PanicConst::MulOverflow,
                BinOp::Div => PanicConst::DivOverflow,
                BinOp::Rem => PanicConst::RemOverflow,
                BinOp::Shl => PanicConst::ShlOverflow,
                BinOp::Shr => PanicConst::ShrOverflow,
                _ => return Err(self.malformed("an overflow assert of an operator that cannot overflow")),
            },
            AssertKind::OverflowNeg => PanicConst::NegOverflow,
            AssertKind::DivisionByZero => PanicConst::DivByZero,
            AssertKind::RemainderByZero => PanicConst::RemByZero,
            AssertKind::Bounds => return self.bounds_panic(msg, unwind),
            AssertKind::ResumedAfterReturn | AssertKind::ResumedAfterPanic => {
                return Err(self.unsupported("a coroutine's resumed-after assert"));
            }
        };
        let (instance, may_unwind) = self.lang_instance(panic.lang(), &[])?;
        let r = self.func_ref(instance);
        let sig = self.b.sig(Conv::C, Variadic::No, &[], &[]);
        let landing = self.landing(unwind, may_unwind);
        self.emit(Target::Direct(r), sig, &[], landing, ResultTo::Nothing, None)
    }

    /// The cold path of a bounds assert: `panic_bounds_check(index, len)`; the row's operands
    /// are the length, then the index.
    fn bounds_panic(&mut self, msg: Ops, unwind: Unwind) -> Result<(), BuildError> {
        let ops: Vec<Operand, A> = {
            let mut out = Vec::with_capacity_in(2, self.alloc);
            out.extend(self.v.ops(msg));
            out
        };
        let [len, index] = ops.as_slice() else {
            return Err(self.malformed("a bounds assert without its length and index"));
        };
        let (len, _) = self.operand(*len)?;
        let (index, _) = self.operand(*index)?;
        let values = [self.scalar(index)?, self.scalar(len)?];
        let (instance, may_unwind) = self.lang_instance("panic_bounds_check", &[])?;
        let r = self.func_ref(instance);
        let usize_ty = self.b.usize_ty();
        let param = blaze_lir::builder::Param::of(usize_ty);
        let sig = self.b.sig(Conv::C, Variadic::No, &[param, param], &[]);
        let landing = self.landing(unwind, may_unwind);
        self.emit(Target::Direct(r), sig, &values, landing, ResultTo::Nothing, None)
    }
}
