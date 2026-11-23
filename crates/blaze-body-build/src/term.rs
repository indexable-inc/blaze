//! Terminators: goto, switch, return through the calling convention, calls (direct by an
//! instance's reference, or through a fn pointer), asserts, drops, resume and unreachable
//! (lib.rs, "Unwinding").

use blaze_own::Vec;
use core::alloc::Allocator;

use blaze_body::{AssertKind, BinOp, Block as BodyBlock, Callee, CalleeId, Operand, Ops, Place, StmtKind, Unwind};
use blaze_build::intrinsics::target::{Target as IntrinsicTarget, float::FloatCall};
use blaze_build::layout::LayoutId;
use blaze_build::{Instance, Intrinsic, Panic, PanicConst, abi};
use blaze_lir::builder::Variadic;
use blaze_lir::func::{BlockKind, Conv, RefId, SigId, SlotKind};
use blaze_lir::{Block, Flags, IntCond, Ty, Value};
use blaze_typeck::{ItemKey, List, Mutbl, PNode};

use crate::value::At;
use crate::{BuildError, Cx, Pass, V};

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

    /// Return the registers of `_0`. An ignored or indirect result has no registers.
    fn ret(&mut self) -> Result<(), BuildError> {
        match self.ret {
            Pass::Ignore | Pass::Ref => self.b.ret(&[]),
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
            Pass::Memory { .. } | Pass::Words { .. } => return Err(self.malformed("a C pass of a value of the Rust ABI")),
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
    /// wide pointer, or the address of a copy the callee owns. Moving an indirect argument
    /// from memory transfers the place itself.
    fn pass_arg(&mut self, x: V, lay: LayoutId, pass: Pass, moved: bool, out: &mut Vec<Value, A>) -> Result<(), BuildError> {
        match pass {
            Pass::Ignore => {}
            Pass::Scalar { .. } => out.push(self.scalar(x)?),
            Pass::Wide { .. } => out.extend(self.wide_values(x)?),
            Pass::Ref => {
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
            Pass::Memory { .. } | Pass::Words { .. } => return Err(self.malformed("a C pass of a value of the Rust ABI")),
        }
        Ok(())
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

    fn call(&mut self, callee: CalleeId, args: Ops, dest: Option<Place>, target: Option<BodyBlock>, unwind: Unwind) -> Result<(), BuildError> {
        let (to, may_unwind) = match self.v.callee(callee) {
            Callee::FnItem { key, args: fn_args } => {
                if let Some(Intrinsic::Target(IntrinsicTarget::Float(fc))) = self.adts.intrinsic(key) {
                    return self.float_intrinsic(fc, args, dest, target);
                }
                match self.adts.abi(key) {
                    Some(blaze_build::Abi::Rust) => {}
                    Some(blaze_build::Abi::C | blaze_build::Abi::RustCall) => {
                        return Err(self.unsupported("a call of a fn of the C or \"rust-call\" ABI"));
                    }
                    None => return Err(BuildError::NoAbi { item: key }),
                }
                let instance = self.instance(key, fn_args)?;
                if self.adts.track_caller(instance, self.subst.ground) == Some(true) {
                    return Err(BuildError::TrackCaller { stmt: self.row()?, name: "the callee" });
                }
                (Target::Direct(self.func_ref(instance)), self.adts.nounwind(key) != Some(true))
            }
            Callee::Value(o) => {
                let (x, _) = self.operand(o)?;
                (Target::Pointer(self.scalar(x)?), true)
            }
            Callee::Method { .. } => return Err(self.unsupported("a trait method call (its impl's fn is not resolved here yet)")),
            Callee::Intrinsic { key, .. } => {
                if let Some(Intrinsic::Target(IntrinsicTarget::Float(fc))) = self.adts.intrinsic(key) {
                    return self.float_intrinsic(fc, args, dest, target);
                }
                return Err(self.unsupported("an intrinsic call"));
            }
        };
        let ops: Vec<Operand, A> = {
            let mut out = Vec::with_capacity_in(args.len() as usize, self.alloc);
            out.extend(self.v.ops(args));
            out
        };
        let mut passes: Vec<Pass, A> = Vec::with_capacity_in(ops.len(), self.alloc);
        let mut values: Vec<Value, A> = Vec::with_capacity_in(ops.len() + 1, self.alloc);
        // The result: through a hidden first pointer, or as the call's value.
        let (rpass, result) = match dest {
            None => (Pass::Ignore, ResultTo::Nothing),
            Some(dest) => {
                let lay = self.dest_layout(dest)?;
                let g = self.place_ty(dest)?;
                let rpass = self.pass_of(g)?;
                let result = match rpass {
                    Pass::Ignore => ResultTo::Nothing,
                    Pass::Scalar { .. } => ResultTo::Value { dest, lay },
                    Pass::Wide { .. } => ResultTo::Wide { dest, lay },
                    Pass::Ref => {
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
                    Pass::Memory { .. } | Pass::Words { .. } => return Err(self.malformed("a C pass of a value of the Rust ABI")),
                };
                (rpass, result)
            }
        };
        for &o in ops.iter() {
            let (x, g) = self.operand(o)?;
            let lay = self.layout(g)?;
            let pass = self.pass_of(g)?;
            self.pass_arg(x, lay, pass, matches!(o, Operand::Move(_)), &mut values)?;
            passes.push(pass);
        }
        let (params, rets) = abi::sig_of_in(&passes, rpass, self.alloc);
        let sig = self.b.sig(Conv::C, Variadic::No, &params, &rets);
        let landing = self.landing(unwind, may_unwind);
        self.emit(to, sig, &values, landing, result, target)
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
    /// the panic entry `kind` names with the row's unwind edge.
    fn assert(&mut self, cond: Operand, expected: bool, kind: AssertKind, msg: Ops, target: BodyBlock, unwind: Unwind) -> Result<(), BuildError> {
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
