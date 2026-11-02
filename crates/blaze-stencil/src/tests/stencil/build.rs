//! The one function per shape: IR the patcher selects exactly that shape for (stencil.md,
//! "Shapes", "the accumulator"; `patch/select.rs` and `patch/slots.rs` for the rules).
//!
//! - An operand from a slot is a function parameter. Every type is one: the targets'
//!   conventions pass `i128`, `f16`, `f128` and vectors with LLVM's ABI extensions.
//! - An immediate is an `iconst` of the operand's type ([`Baked::imms`]), a function per value.
//! - An operand from the accumulator is the result of the instruction right before, a `bnot`
//!   (integers, integer vectors) or `fneg` (floats, float vectors) of a parameter the test
//!   passes inverted or negated ([`Xform`]), so the operand's bits are exactly the input's.
//!   An `fma`'s operand 1 is a `bitcast` of the integer parameter of the same bits instead:
//!   an `fneg` there is fused into the `fma` (`patch/select.rs`).
//! - A result to the accumulator has the next instruction, the same `bnot`/`fneg`, as its one
//!   use; to both, it is returned as well; to a slot, it is returned.
//! - Memory goes through a stack slot the function first stores its input to, since the
//!   interpreter has no other memory; a branch returns which way it went.
//!
//! What may sit right before the instruction is what decides its accumulator shapes, so every
//! other instruction is placed where it can't chain into it: before a store (which has no
//! result), or behind a separator (`bnot` of a parameter of its own).

use cranelift_codegen::{
    ir::{
        self, AtomicRmwOp, BlockArg, ConstantData, Endianness, InstBuilder, MemFlagsData,
        condcodes::{FloatCC, IntCC},
        immediates::{Ieee16, Ieee32, Ieee64},
        types,
    },
    isa::TargetIsa,
    packed_option::ReservedValue,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

use super::super::host;
use crate::{
    Target,
    shape::{Disp, Dst, FloatCc, IntCc, Op, RmwOp, Shape, Src, Ty},
};

/// How a parameter is made from the input it carries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Xform {
    Plain,
    /// Every bit flipped: the function's `bnot` gives the input back.
    Invert,
    /// Each lane's sign flipped: the function's `fneg` gives the input back.
    Negate,
    /// The same bits as the integer type of the float's width: the function's `bitcast`
    /// gives the input back.
    Bits,
}

impl Xform {
    /// The type of the parameter carrying an input of `ty`.
    pub fn param(self, ty: Ty) -> Ty {
        match (self, ty) {
            (Xform::Bits, Ty::F32) => Ty::I32,
            (Xform::Bits, Ty::F64) => Ty::I64,
            (Xform::Bits, it) => it.int_lanes(),
            (_, it) => it,
        }
    }

    /// The parameter's bits for an input of `ty`.
    pub fn apply(self, ty: Ty, mut bytes: [u8; 16]) -> [u8; 16] {
        let width = usize::from(ty.bytes());
        match self {
            Xform::Plain | Xform::Bits => {}
            Xform::Invert => bytes[..width].iter_mut().for_each(|it| *it = !*it),
            Xform::Negate => {
                let lane = usize::from(ty.lane().bytes());
                for at in (lane - 1..width).step_by(lane) {
                    bytes[at] ^= 0x80;
                }
            }
        }
        bytes
    }
}

/// One parameter of a shape's function: the input it carries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Input {
    pub ty: Ty,
    pub xform: Xform,
    /// For a shift's or rotate's amount, the lane type it shifts: its edges are that width's.
    pub amount_of: Option<Ty>,
}

/// What a function is made with besides its parameters: each [`Src::Imm`] operand's value,
/// an `iconst`'s or `fconst`'s bits (in `imms[0]`), a confined access's offset (in
/// `imms[OFFSET]`), a `vconst`'s bytes or a shuffle's mask.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Baked {
    pub imms: [i64; 3],
    pub bytes: [u8; 16],
}

/// Where a confined access's `off` is in [`Baked::imms`]: operand 2, a load's none and a
/// store's `vmctx`, never an immediate.
pub const OFFSET: usize = 2;

/// A shape's function, ready for both backends and the interpreter.
pub struct Built {
    pub func: ir::Function,
    pub sig: ir::Signature,
    /// One per parameter, in order.
    pub inputs: Vec<Input>,
    pub rets: Vec<ir::Type>,
}

/// The function that selects `shape` for `target`, compiled with `isa`'s frontend config. An
/// error names why no IR can select it.
pub fn build(shape: Shape, target: &Target, isa: &dyn TargetIsa, baked: &Baked) -> Result<Built, String> {
    let sig = ir::Signature::new(host::call_conv(target));
    let mut func = ir::Function::with_name_signature(ir::UserFuncName::user(0, 0), sig);
    let mut fcx = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut func, &mut fcx);
    let entry = b.create_block();
    b.switch_to_block(entry);
    let mut cx = Cx { b, entry, shape, baked: *baked, inputs: Vec::new(), returns: false, ss: None };
    cx.body()?;
    let Cx { mut b, inputs, .. } = cx;
    b.seal_all_blocks();
    b.finalize(isa.frontend_config());
    let rets = func.signature.returns.iter().map(|it| it.value_type).collect();
    Ok(Built { sig: func.signature.clone(), func, inputs, rets })
}

pub fn ir_ty(ty: Ty) -> ir::Type {
    match ty {
        Ty::I8 => types::I8,
        Ty::I16 => types::I16,
        Ty::I32 => types::I32,
        Ty::I64 => types::I64,
        Ty::I128 => types::I128,
        Ty::F16 => types::F16,
        Ty::F32 => types::F32,
        Ty::F64 => types::F64,
        Ty::F128 => types::F128,
        Ty::I8x16 => types::I8X16,
        Ty::I16x8 => types::I16X8,
        Ty::I32x4 => types::I32X4,
        Ty::I64x2 => types::I64X2,
        Ty::F32x4 => types::F32X4,
        Ty::F64x2 => types::F64X2,
    }
}

/// The type of the memory a load `op` of a `ty` result reads.
fn loaded(op: Op, ty: Ty) -> Ty {
    match op {
        Op::Uload8 | Op::Sload8 => Ty::I8,
        Op::Uload16 | Op::Sload16 => Ty::I16,
        Op::Uload32 | Op::Sload32 => Ty::I32,
        Op::Uload8x8 | Op::Sload8x8 | Op::Uload16x4 | Op::Sload16x4 | Op::Uload32x2 | Op::Sload32x2 => {
            Ty::I64
        }
        _ => ty,
    }
}

fn int_cc(cc: u8) -> IntCC {
    match IntCc::from_u8(cc & 0xf).expect("a shape's condition") {
        IntCc::Equal => IntCC::Equal,
        IntCc::NotEqual => IntCC::NotEqual,
        IntCc::SignedLessThan => IntCC::SignedLessThan,
        IntCc::SignedGreaterThanOrEqual => IntCC::SignedGreaterThanOrEqual,
        IntCc::SignedGreaterThan => IntCC::SignedGreaterThan,
        IntCc::SignedLessThanOrEqual => IntCC::SignedLessThanOrEqual,
        IntCc::UnsignedLessThan => IntCC::UnsignedLessThan,
        IntCc::UnsignedGreaterThanOrEqual => IntCC::UnsignedGreaterThanOrEqual,
        IntCc::UnsignedGreaterThan => IntCC::UnsignedGreaterThan,
        IntCc::UnsignedLessThanOrEqual => IntCC::UnsignedLessThanOrEqual,
    }
}

fn float_cc(cc: u8) -> FloatCC {
    match FloatCc::from_u8(cc).expect("a shape's condition") {
        FloatCc::Ordered => FloatCC::Ordered,
        FloatCc::Unordered => FloatCC::Unordered,
        FloatCc::Equal => FloatCC::Equal,
        FloatCc::NotEqual => FloatCC::NotEqual,
        FloatCc::OrderedNotEqual => FloatCC::OrderedNotEqual,
        FloatCc::UnorderedOrEqual => FloatCC::UnorderedOrEqual,
        FloatCc::LessThan => FloatCC::LessThan,
        FloatCc::LessThanOrEqual => FloatCC::LessThanOrEqual,
        FloatCc::GreaterThan => FloatCC::GreaterThan,
        FloatCc::GreaterThanOrEqual => FloatCC::GreaterThanOrEqual,
        FloatCc::UnorderedOrLessThan => FloatCC::UnorderedOrLessThan,
        FloatCc::UnorderedOrLessThanOrEqual => FloatCC::UnorderedOrLessThanOrEqual,
        FloatCc::UnorderedOrGreaterThan => FloatCC::UnorderedOrGreaterThan,
        FloatCc::UnorderedOrGreaterThanOrEqual => FloatCC::UnorderedOrGreaterThanOrEqual,
    }
}

fn rmw_op(op: u8) -> AtomicRmwOp {
    match RmwOp::from_u8(op).expect("a shape's read-modify-write") {
        RmwOp::Add => AtomicRmwOp::Add,
        RmwOp::Sub => AtomicRmwOp::Sub,
        RmwOp::And => AtomicRmwOp::And,
        RmwOp::Nand => AtomicRmwOp::Nand,
        RmwOp::Or => AtomicRmwOp::Or,
        RmwOp::Xor => AtomicRmwOp::Xor,
        RmwOp::Xchg => AtomicRmwOp::Xchg,
        RmwOp::Umin => AtomicRmwOp::Umin,
        RmwOp::Umax => AtomicRmwOp::Umax,
        RmwOp::Smin => AtomicRmwOp::Smin,
        RmwOp::Smax => AtomicRmwOp::Smax,
    }
}

/// The trap code of `trapz`, `trapnz` and `uadd_overflow_trap`.
fn user_trap() -> ir::TrapCode {
    ir::TrapCode::user(1).expect("user trap code 1")
}

fn flags() -> MemFlagsData {
    MemFlagsData::trusted()
}

/// Where the stored input sits in the function's 32-byte stack slot: past an offset, so the
/// `Offset` hole is patched with something.
const AT: i32 = 16;

/// A confined access's address ([`Cx::confined`]): `vmctx`, the offset `p` is made with, and
/// the address once made.
#[derive(Clone, Copy)]
struct Confined {
    vmctx: ir::Value,
    off: i64,
    addr: Option<ir::Value>,
}

struct Cx<'a> {
    b: FunctionBuilder<'a>,
    entry: ir::Block,
    shape: Shape,
    baked: Baked,
    inputs: Vec<Input>,
    /// Whether the signature's returns are set (by the first `return`).
    returns: bool,
    ss: Option<ir::StackSlot>,
}

impl Cx<'_> {
    /// A new parameter carrying an input of `ty`.
    fn param(&mut self, ty: Ty, xform: Xform, amount_of: Option<Ty>) -> ir::Value {
        // The entry block's parameters are appended as inputs are found, after its first
        // instructions: no variable is ever declared, so SSA construction never reads them.
        let carried = ir_ty(xform.param(ty));
        let value = self.b.func.dfg.append_block_param(self.entry, carried);
        self.b.func.signature.params.push(ir::AbiParam::new(carried));
        self.inputs.push(Input { ty, xform, amount_of });
        value
    }

    fn amount_of(&self, n: usize) -> Option<Ty> {
        let shift = matches!(self.shape.op, Op::Ishl | Op::Ushr | Op::Sshr | Op::Rotl | Op::Rotr);
        (shift && n == 1).then(|| self.shape.ty.lane())
    }

    /// An `iconst` of `ty` with `value`'s low bits, zero-extended as the verifier wants.
    fn iconst(&mut self, ty: Ty, value: i64) -> ir::Value {
        let bits = u32::from(ty.bytes()) * 8;
        let value = if bits >= 64 { value } else { value & ((1i64 << bits) - 1) };
        self.b.ins().iconst(ir_ty(ty), value)
    }

    /// The `bnot` or `fneg` of `value`: what hands an accumulator on.
    fn invert(&mut self, value: ir::Value) -> ir::Value {
        let ty = self.b.func.dfg.value_type(value);
        if ty.lane_type().is_float() { self.b.ins().fneg(value) } else { self.b.ins().bnot(value) }
    }

    /// Operand `n` from where the shape says. An accumulator operand writes its producer
    /// here, so it is asked for last, right before the instruction.
    fn operand(&mut self, n: usize) -> ir::Value {
        let ty = self.shape.arg(n);
        match self.shape.srcs[n] {
            Src::Slot => self.param(ty, Xform::Plain, self.amount_of(n)),
            Src::Imm => self.iconst(ty, self.baked.imms[n]),
            Src::Acc if self.shape.op == Op::Fma && n == 1 => {
                let param = self.param(ty, Xform::Bits, None);
                self.b.ins().bitcast(ir_ty(ty), MemFlagsData::new(), param)
            }
            Src::Acc => {
                let xform = if ty.lane().is_float() { Xform::Negate } else { Xform::Invert };
                let param = self.param(ty, xform, self.amount_of(n));
                self.invert(param)
            }
            Src::None => panic!("{}: operand {n} is none", self.shape),
        }
    }

    /// Every operand of the op, the accumulator's last.
    fn operands(&mut self) -> [ir::Value; 3] {
        let mut values = [ir::Value::reserved_value(); 3];
        let args = usize::from(self.shape.op.info().args);
        for n in 0..args {
            if self.shape.srcs[n] != Src::Acc {
                values[n] = self.operand(n);
            }
        }
        for n in 0..args {
            if self.shape.srcs[n] == Src::Acc {
                values[n] = self.operand(n);
            }
        }
        values
    }

    /// A stencil of its own between two instructions, so neither hands the other an
    /// accumulator: a `bnot` of a parameter of its own, returned.
    fn separator(&mut self) -> ir::Value {
        let param = self.param(Ty::I64, Xform::Plain, None);
        self.b.ins().bnot(param)
    }

    fn ret(&mut self, values: &[ir::Value]) {
        if !self.returns {
            for &value in values {
                let ty = self.b.func.dfg.value_type(value);
                self.b.func.signature.returns.push(ir::AbiParam::new(ty));
            }
            self.returns = true;
        }
        self.b.ins().return_(values);
    }

    /// Returns `results` with result 0 where the shape's destination says.
    fn finish(&mut self, results: &[ir::Value]) {
        let mut rets = Vec::with_capacity(results.len() + 1);
        match self.shape.dst {
            Dst::None | Dst::Slot => rets.extend_from_slice(results),
            Dst::Acc => {
                rets.push(self.invert(results[0]));
                rets.extend_from_slice(&results[1..]);
            }
            Dst::Both => {
                let inverted = self.invert(results[0]);
                rets.extend_from_slice(&[results[0], inverted]);
                rets.extend_from_slice(&results[1..]);
            }
        }
        self.ret(&rets);
    }

    /// An op of pure operands and results: `f` writes it.
    fn pure(&mut self, f: impl FnOnce(&mut FunctionBuilder<'_>, [ir::Value; 3]) -> Vec<ir::Value>) {
        let values = self.operands();
        let results = f(&mut self.b, values);
        self.finish(&results);
    }

    /// A constant `c` made by the shape's instruction: a stencil reads it from its slot
    /// (or the accumulator), or the glue would materialize it and no stencil would be its.
    fn constant(&mut self, c: ir::Value) {
        match self.shape.dst {
            Dst::Slot if self.shape.ty.acc().is_some() => {
                let sep = self.separator();
                let read = self.invert(c);
                self.ret(&[sep, read]);
            }
            _ => self.finish(&[c]),
        }
    }

    fn slot(&mut self) -> ir::StackSlot {
        if let Some(ss) = self.ss {
            return ss;
        }
        let data = ir::StackSlotData::new(ir::StackSlotKind::ExplicitSlot, 32, 4);
        let ss = self.b.create_sized_stack_slot(data);
        self.ss = Some(ss);
        ss
    }

    fn stack_addr(&mut self, offset: i32) -> ir::Value {
        let ss = self.slot();
        self.b.ins().stack_addr(types::I64, ss, offset)
    }

    /// A load `op` from memory holding one input of `mem`: at `base + AT`, or at `confined`
    /// (that place, reached through a confined address) with no offset.
    fn load(&mut self, op: Op, mem: Ty, confined: Option<Confined>) {
        let t = ir_ty(self.shape.ty);
        let atomic = op == Op::AtomicLoad;
        let base = self.stack_addr(0);
        let at16 = if atomic && confined.is_none() { Some(self.stack_addr(AT)) } else { None };
        let input = self.param(mem, Xform::Plain, None);
        self.b.ins().store(flags(), input, base, AT);
        let (addr, at) = match (confined, self.shape.srcs[0], at16) {
            (Some(confined), _, _) => (self.address(confined), 0),
            (None, Src::Acc, _) => (self.stack_addr(0), AT),
            (None, _, Some(at16)) => (at16, AT),
            (None, _, None) => (base, AT),
        };
        let f = flags();
        let ins = self.b.ins();
        let value = match op {
            Op::Load => ins.load(t, f, addr, at),
            Op::Uload8 => ins.uload8(t, f, addr, at),
            Op::Sload8 => ins.sload8(t, f, addr, at),
            Op::Uload16 => ins.uload16(t, f, addr, at),
            Op::Sload16 => ins.sload16(t, f, addr, at),
            Op::Uload32 => ins.uload32(f, addr, at),
            Op::Sload32 => ins.sload32(f, addr, at),
            Op::Uload8x8 => ins.uload8x8(f, addr, at),
            Op::Sload8x8 => ins.sload8x8(f, addr, at),
            Op::Uload16x4 => ins.uload16x4(f, addr, at),
            Op::Sload16x4 => ins.sload16x4(f, addr, at),
            Op::Uload32x2 => ins.uload32x2(f, addr, at),
            Op::Sload32x2 => ins.sload32x2(f, addr, at),
            Op::AtomicLoad => ins.atomic_load(t, f, addr),
            other => panic!("{other:?} is no load"),
        };
        self.finish(&[value]);
    }

    /// A confined access (`LoadConfined`, `StoreConfined`): the plain access its `aux` names at
    /// `vmctx + uextend.i64(ireduce.i32(p + off))` with no offset, `vmctx` the stack slot and
    /// `p + off` wrapping (in 32 bits, for the `Imm32` offset) to `AT`, so the interpreter and
    /// both backends read and write the same place. `off` is 0, a 12-bit or a 32-bit offset, as
    /// the shape's `Disp` says.
    fn confined(&mut self) -> Result<(), String> {
        let (access, disp) = self.shape.confined().ok_or("a confined shape naming no access")?;
        let off = if disp == Disp::Zero { 0 } else { self.baked.imms[OFFSET] };
        let vmctx = self.stack_addr(0);
        let load = self.shape.op == Op::LoadConfined;
        // An accumulator `p` is made right before the access, after the memory is filled, so
        // its stencil hands it on; any other is made first, so the filling keeps them apart.
        let mut confined = Confined { vmctx, off, addr: None };
        if self.shape.srcs[usize::from(!load)] != Src::Acc {
            confined.addr = Some(self.address(confined));
        }
        if load {
            self.load(access, loaded(access, self.shape.ty), Some(confined));
        } else {
            self.store(access, Some(confined));
        }
        Ok(())
    }

    /// The confined address `vmctx + uextend.i64(ireduce.i32(p + off))`, unless made already.
    fn address(&mut self, confined: Confined) -> ir::Value {
        if let Some(addr) = confined.addr {
            return addr;
        }
        let Confined { vmctx, off, .. } = confined;
        // An `iadd` of an `iconst`, which `confine` fuses (it fuses no `iadd_imm`), its
        // `iconst` before `p`'s: `p`'s stencil is the last before the access.
        let k = (off != 0).then(|| self.b.ins().iconst(types::I64, off));
        let p = self.b.ins().iconst(types::I64, (i64::from(AT) - off) & 0xffff_ffff);
        let sum = match k {
            Some(k) => self.b.ins().iadd(p, k),
            None => p,
        };
        let low = self.b.ins().ireduce(types::I32, sum);
        let index = self.b.ins().uextend(types::I64, low);
        self.b.ins().iadd(vmctx, index)
    }

    /// A store `op` into prefilled memory, read back as two `i64`s: at `base + AT`, or at
    /// `confined` (that place, reached through a confined address) with no offset.
    fn store(&mut self, op: Op, confined: Option<Confined>) {
        let atomic = op == Op::AtomicStore;
        let base = self.stack_addr(0);
        let at16 = if atomic && confined.is_none() { Some(self.stack_addr(AT)) } else { None };
        let fill = self.iconst(Ty::I64, 0x5a5a_5a5a);
        self.b.ins().store(flags(), fill, base, AT);
        self.b.ins().store(flags(), fill, base, AT + 8);
        let (value, addr, at) = if let Some(confined) = confined {
            let value = self.operand(0);
            (value, self.address(confined), 0)
        } else if self.shape.srcs[1] == Src::Acc {
            let value = self.operand(0);
            (value, self.stack_addr(0), AT)
        } else {
            (self.operand(0), at16.unwrap_or(base), AT)
        };
        let f = flags();
        let ins = self.b.ins();
        match op {
            Op::Store => ins.store(f, value, addr, at),
            Op::Istore8 => ins.istore8(f, value, addr, at),
            Op::Istore16 => ins.istore16(f, value, addr, at),
            Op::Istore32 => ins.istore32(f, value, addr, at),
            Op::AtomicStore => ins.atomic_store(f, value, addr),
            other => panic!("{other:?} is no store"),
        };
        let lo = self.b.ins().load(types::I64, flags(), base, AT);
        let hi = self.b.ins().load(types::I64, flags(), base, AT + 8);
        self.ret(&[lo, hi]);
    }

    /// `atomic_rmw` or `atomic_cas` on memory holding an input: the old value and the memory
    /// after.
    fn atomic(&mut self) {
        let ty = self.shape.ty;
        let t = ir_ty(ty);
        let base = self.stack_addr(0);
        let at16 = self.stack_addr(AT);
        let memory = self.param(ty, Xform::Plain, None);
        self.b.ins().store(flags(), memory, base, AT);
        let old = match self.shape.op {
            Op::AtomicRmw => {
                let x = self.operand(1);
                let op = rmw_op(self.shape.aux);
                self.b.ins().atomic_rmw(t, flags(), op, at16, x)
            }
            _ => {
                let expected = self.operand(1);
                let x = self.operand(2);
                self.b.ins().atomic_cas(flags(), at16, expected, x)
            }
        };
        let after = self.b.ins().load(t, flags(), base, AT);
        self.ret(&[old, after]);
    }

    /// Blocks for a branch: `else` placed right after the branch's block and `then` after it,
    /// so the taken edge is never the code that follows (which would complement a `BrIcmp`).
    fn branch(&mut self, cond: ir::Value) {
        let then = self.b.create_block();
        let other = self.b.create_block();
        self.b.ins().brif(cond, then, &[], other, &[]);
        self.b.switch_to_block(other);
        let two = self.b.ins().iconst(types::I32, 2);
        self.ret(&[two]);
        self.b.switch_to_block(then);
        let one = self.b.ins().iconst(types::I32, 1);
        self.ret(&[one]);
    }

    fn body(&mut self) -> Result<(), String> {
        let shape = self.shape;
        let t = ir_ty(shape.ty);
        let aux = shape.aux;
        let aux_t = ir_ty(shape.aux_ty());
        match shape.op {
            Op::Copy => {
                if shape.srcs[0] != Src::Slot || shape.dst != Dst::Slot {
                    return Err(format!(
                        "no IR selects {shape}: the patcher writes `copy` only from a slot to a slot \
                         (`patch/emit.rs`'s `copy`), for block arguments"
                    ));
                }
                let input = self.param(shape.ty, Xform::Plain, None);
                let next = self.b.create_block();
                let param = self.b.append_block_param(next, t);
                self.b.ins().jump(next, &[BlockArg::Value(input)]);
                self.b.switch_to_block(next);
                self.ret(&[param]);
            }
            Op::Iconst => {
                let c = self.iconst(shape.ty, self.baked.imms[0]);
                self.constant(c);
            }
            Op::Fconst => {
                let bits = self.baked.imms[0];
                let c = match shape.ty {
                    Ty::F16 => self.b.ins().f16const(Ieee16::with_bits(bits as u16)),
                    Ty::F32 => self.b.ins().f32const(Ieee32::with_bits(bits as u32)),
                    _ => self.b.ins().f64const(Ieee64::with_bits(bits as u64)),
                };
                self.constant(c);
            }
            Op::Vconst => {
                let data = ConstantData::from(&self.baked.bytes[..]);
                let handle = self.b.func.dfg.constants.insert(data);
                let c = if shape.ty == Ty::F128 {
                    self.b.ins().f128const(handle)
                } else {
                    self.b.ins().vconst(t, handle)
                };
                self.constant(c);
            }
            Op::Iadd => self.pure(|b, [x, y, _]| vec![b.ins().iadd(x, y)]),
            Op::Isub => self.pure(|b, [x, y, _]| vec![b.ins().isub(x, y)]),
            Op::Ineg => self.pure(|b, [x, _, _]| vec![b.ins().ineg(x)]),
            Op::Iabs => self.pure(|b, [x, _, _]| vec![b.ins().iabs(x)]),
            Op::Imul => self.pure(|b, [x, y, _]| vec![b.ins().imul(x, y)]),
            Op::Umulhi => self.pure(|b, [x, y, _]| vec![b.ins().umulhi(x, y)]),
            Op::Smulhi => self.pure(|b, [x, y, _]| vec![b.ins().smulhi(x, y)]),
            Op::Udiv => self.pure(|b, [x, y, _]| vec![b.ins().udiv(x, y)]),
            Op::Sdiv => self.pure(|b, [x, y, _]| vec![b.ins().sdiv(x, y)]),
            Op::Urem => self.pure(|b, [x, y, _]| vec![b.ins().urem(x, y)]),
            Op::Srem => self.pure(|b, [x, y, _]| vec![b.ins().srem(x, y)]),
            Op::SqmulRoundSat => self.pure(|b, [x, y, _]| vec![b.ins().sqmul_round_sat(x, y)]),
            Op::AvgRound => self.pure(|b, [x, y, _]| vec![b.ins().avg_round(x, y)]),
            Op::UaddSat => self.pure(|b, [x, y, _]| vec![b.ins().uadd_sat(x, y)]),
            Op::SaddSat => self.pure(|b, [x, y, _]| vec![b.ins().sadd_sat(x, y)]),
            Op::UsubSat => self.pure(|b, [x, y, _]| vec![b.ins().usub_sat(x, y)]),
            Op::SsubSat => self.pure(|b, [x, y, _]| vec![b.ins().ssub_sat(x, y)]),
            Op::Smin => self.pure(|b, [x, y, _]| vec![b.ins().smin(x, y)]),
            Op::Umin => self.pure(|b, [x, y, _]| vec![b.ins().umin(x, y)]),
            Op::Smax => self.pure(|b, [x, y, _]| vec![b.ins().smax(x, y)]),
            Op::Umax => self.pure(|b, [x, y, _]| vec![b.ins().umax(x, y)]),
            Op::UaddOverflow => self.pure(|b, [x, y, _]| pair(b.ins().uadd_overflow(x, y))),
            Op::SaddOverflow => self.pure(|b, [x, y, _]| pair(b.ins().sadd_overflow(x, y))),
            Op::UsubOverflow => self.pure(|b, [x, y, _]| pair(b.ins().usub_overflow(x, y))),
            Op::SsubOverflow => self.pure(|b, [x, y, _]| pair(b.ins().ssub_overflow(x, y))),
            Op::UmulOverflow => self.pure(|b, [x, y, _]| pair(b.ins().umul_overflow(x, y))),
            Op::SmulOverflow => self.pure(|b, [x, y, _]| pair(b.ins().smul_overflow(x, y))),
            Op::UaddOverflowCin => self.pure(|b, [x, y, c]| pair(b.ins().uadd_overflow_cin(x, y, c))),
            Op::SaddOverflowCin => self.pure(|b, [x, y, c]| pair(b.ins().sadd_overflow_cin(x, y, c))),
            Op::UsubOverflowBin => self.pure(|b, [x, y, c]| pair(b.ins().usub_overflow_bin(x, y, c))),
            Op::SsubOverflowBin => self.pure(|b, [x, y, c]| pair(b.ins().ssub_overflow_bin(x, y, c))),
            Op::UaddOverflowTrap => {
                self.pure(|b, [x, y, _]| vec![b.ins().uadd_overflow_trap(x, y, user_trap())])
            }
            Op::Band => self.pure(|b, [x, y, _]| vec![b.ins().band(x, y)]),
            Op::Bor => self.pure(|b, [x, y, _]| vec![b.ins().bor(x, y)]),
            Op::Bxor => self.pure(|b, [x, y, _]| vec![b.ins().bxor(x, y)]),
            Op::Bnot => self.pure(|b, [x, _, _]| vec![b.ins().bnot(x)]),
            Op::Rotl => self.pure(|b, [x, y, _]| vec![b.ins().rotl(x, y)]),
            Op::Rotr => self.pure(|b, [x, y, _]| vec![b.ins().rotr(x, y)]),
            Op::Ishl => self.pure(|b, [x, y, _]| vec![b.ins().ishl(x, y)]),
            Op::Ushr => self.pure(|b, [x, y, _]| vec![b.ins().ushr(x, y)]),
            Op::Sshr => self.pure(|b, [x, y, _]| vec![b.ins().sshr(x, y)]),
            Op::Bitrev => self.pure(|b, [x, _, _]| vec![b.ins().bitrev(x)]),
            Op::Clz => self.pure(|b, [x, _, _]| vec![b.ins().clz(x)]),
            Op::Cls => self.pure(|b, [x, _, _]| vec![b.ins().cls(x)]),
            Op::Ctz => self.pure(|b, [x, _, _]| vec![b.ins().ctz(x)]),
            Op::Bswap => self.pure(|b, [x, _, _]| vec![b.ins().bswap(x)]),
            Op::Popcnt => self.pure(|b, [x, _, _]| vec![b.ins().popcnt(x)]),
            Op::Bitselect => self.pure(|b, [c, x, y]| vec![b.ins().bitselect(c, x, y)]),
            Op::Bmask => self.pure(|b, [x, _, _]| vec![b.ins().bmask(t, x)]),
            Op::Icmp => self.pure(|b, [x, y, _]| vec![b.ins().icmp(int_cc(aux), x, y)]),
            Op::Fcmp => self.pure(|b, [x, y, _]| vec![b.ins().fcmp(float_cc(aux), x, y)]),
            Op::Select => self.pure(|b, [c, x, y]| vec![b.ins().select(c, x, y)]),
            Op::SelectIcmp => self.pure(|b, [x, y, _]| {
                let c = b.ins().icmp(int_cc(aux), x, y);
                vec![b.ins().select(c, x, y)]
            }),
            Op::Fadd => self.pure(|b, [x, y, _]| vec![b.ins().fadd(x, y)]),
            Op::Fsub => self.pure(|b, [x, y, _]| vec![b.ins().fsub(x, y)]),
            Op::Fmul => self.pure(|b, [x, y, _]| vec![b.ins().fmul(x, y)]),
            Op::Fdiv => self.pure(|b, [x, y, _]| vec![b.ins().fdiv(x, y)]),
            Op::Sqrt => self.pure(|b, [x, _, _]| vec![b.ins().sqrt(x)]),
            Op::Fma if shape.aux == 1 => self.pure(|b, [x, y, z]| {
                let negated = b.ins().fneg(y);
                vec![b.ins().fma(x, negated, z)]
            }),
            Op::Fma => self.pure(|b, [x, y, z]| vec![b.ins().fma(x, y, z)]),
            Op::Fneg => self.pure(|b, [x, _, _]| vec![b.ins().fneg(x)]),
            Op::Fabs => self.pure(|b, [x, _, _]| vec![b.ins().fabs(x)]),
            Op::Fcopysign => self.pure(|b, [x, y, _]| vec![b.ins().fcopysign(x, y)]),
            Op::Fmin => self.pure(|b, [x, y, _]| vec![b.ins().fmin(x, y)]),
            Op::Fmax => self.pure(|b, [x, y, _]| vec![b.ins().fmax(x, y)]),
            Op::Ceil => self.pure(|b, [x, _, _]| vec![b.ins().ceil(x)]),
            Op::Floor => self.pure(|b, [x, _, _]| vec![b.ins().floor(x)]),
            Op::Trunc => self.pure(|b, [x, _, _]| vec![b.ins().trunc(x)]),
            Op::Nearest => self.pure(|b, [x, _, _]| vec![b.ins().nearest(x)]),
            Op::Bitcast => {
                let le = MemFlagsData::new().with_endianness(Endianness::Little);
                self.pure(|b, [x, _, _]| vec![b.ins().bitcast(t, le, x)]);
            }
            Op::Ireduce => self.pure(|b, [x, _, _]| vec![b.ins().ireduce(t, x)]),
            Op::Uextend => self.pure(|b, [x, _, _]| vec![b.ins().uextend(t, x)]),
            Op::Sextend => self.pure(|b, [x, _, _]| vec![b.ins().sextend(t, x)]),
            Op::Fpromote => self.pure(|b, [x, _, _]| vec![b.ins().fpromote(t, x)]),
            Op::Fdemote => self.pure(|b, [x, _, _]| vec![b.ins().fdemote(t, x)]),
            Op::Fvdemote => self.pure(|b, [x, _, _]| vec![b.ins().fvdemote(x)]),
            Op::FvpromoteLow => self.pure(|b, [x, _, _]| vec![b.ins().fvpromote_low(x)]),
            Op::FcvtToUint => self.pure(|b, [x, _, _]| vec![b.ins().fcvt_to_uint(t, x)]),
            Op::FcvtToSint => self.pure(|b, [x, _, _]| vec![b.ins().fcvt_to_sint(t, x)]),
            Op::FcvtToUintSat => self.pure(|b, [x, _, _]| vec![b.ins().fcvt_to_uint_sat(t, x)]),
            Op::FcvtToSintSat => self.pure(|b, [x, _, _]| vec![b.ins().fcvt_to_sint_sat(t, x)]),
            Op::FcvtFromUint => self.pure(|b, [x, _, _]| vec![b.ins().fcvt_from_uint(t, x)]),
            Op::FcvtFromSint => self.pure(|b, [x, _, _]| vec![b.ins().fcvt_from_sint(t, x)]),
            Op::Isplit => self.pure(|b, [x, _, _]| pair(b.ins().isplit(x))),
            Op::Iconcat => self.pure(|b, [lo, hi, _]| vec![b.ins().iconcat(lo, hi)]),
            Op::Snarrow => self.pure(|b, [x, y, _]| vec![b.ins().snarrow(x, y)]),
            Op::Unarrow => self.pure(|b, [x, y, _]| vec![b.ins().unarrow(x, y)]),
            Op::Uunarrow => self.pure(|b, [x, y, _]| vec![b.ins().uunarrow(x, y)]),
            Op::SwidenLow => self.pure(|b, [x, _, _]| vec![b.ins().swiden_low(x)]),
            Op::SwidenHigh => self.pure(|b, [x, _, _]| vec![b.ins().swiden_high(x)]),
            Op::UwidenLow => self.pure(|b, [x, _, _]| vec![b.ins().uwiden_low(x)]),
            Op::UwidenHigh => self.pure(|b, [x, _, _]| vec![b.ins().uwiden_high(x)]),
            Op::IaddPairwise => self.pure(|b, [x, y, _]| vec![b.ins().iadd_pairwise(x, y)]),
            Op::ScalarToVector => self.pure(|b, [x, _, _]| vec![b.ins().scalar_to_vector(t, x)]),
            Op::Splat => self.pure(|b, [x, _, _]| vec![b.ins().splat(t, x)]),
            Op::Swizzle => self.pure(|b, [x, y, _]| vec![b.ins().swizzle(x, y)]),
            Op::Shuffle => {
                let mask = self.b.func.dfg.immediates.push(ConstantData::from(&self.baked.bytes[..]));
                self.pure(|b, [x, y, _]| vec![b.ins().shuffle(x, y, mask)]);
            }
            Op::Insertlane => self.pure(|b, [x, y, _]| vec![b.ins().insertlane(x, y, aux)]),
            Op::Extractlane => self.pure(|b, [x, _, _]| vec![b.ins().extractlane(x, aux)]),
            Op::VanyTrue => self.pure(|b, [x, _, _]| vec![b.ins().vany_true(x)]),
            Op::VallTrue => self.pure(|b, [x, _, _]| vec![b.ins().vall_true(x)]),
            Op::VhighBits => self.pure(|b, [x, _, _]| vec![b.ins().vhigh_bits(aux_t, x)]),
            Op::Blendv => self.pure(|b, [c, x, y]| vec![b.ins().blendv(c, x, y)]),
            Op::X86Pshufb => self.pure(|b, [x, y, _]| vec![b.ins().x86_pshufb(x, y)]),
            Op::X86Pmulhrsw => self.pure(|b, [x, y, _]| vec![b.ins().x86_pmulhrsw(x, y)]),
            Op::X86Pmaddubsw => self.pure(|b, [x, y, _]| vec![b.ins().x86_pmaddubsw(x, y)]),
            Op::X86Cvtt2dq => self.pure(|b, [x, _, _]| vec![b.ins().x86_cvtt2dq(t, x)]),
            Op::Load
            | Op::Uload8
            | Op::Sload8
            | Op::Uload16
            | Op::Sload16
            | Op::Uload32
            | Op::Sload32
            | Op::Uload8x8
            | Op::Sload8x8
            | Op::Uload16x4
            | Op::Sload16x4
            | Op::Uload32x2
            | Op::Sload32x2
            | Op::AtomicLoad => self.load(shape.op, loaded(shape.op, shape.ty), None),
            Op::Store | Op::Istore8 | Op::Istore16 | Op::Istore32 | Op::AtomicStore => {
                self.store(shape.op, None)
            }
            Op::LoadConfined | Op::StoreConfined => self.confined()?,
            Op::StackAddr => {
                let input = self.param(Ty::I64, Xform::Plain, None);
                let base = self.stack_addr(0);
                self.b.ins().store(flags(), input, base, 8);
                let addr = self.stack_addr(8);
                match shape.dst {
                    Dst::Acc => {
                        let read = self.b.ins().load(types::I64, flags(), addr, 0);
                        self.ret(&[read]);
                    }
                    Dst::Both => {
                        let read = self.b.ins().load(types::I64, flags(), addr, 0);
                        let again = self.b.ins().load(types::I64, flags(), addr, 0);
                        self.ret(&[read, again]);
                    }
                    _ => {
                        let sep = self.separator();
                        let read = self.b.ins().load(types::I64, flags(), addr, 0);
                        self.ret(&[sep, read]);
                    }
                }
            }
            Op::AtomicRmw | Op::AtomicCas => self.atomic(),
            Op::Fence => {
                self.b.ins().fence();
                self.ret(&[]);
            }
            Op::Jump => {
                let skipped = self.b.create_block();
                let target = self.b.create_block();
                self.b.ins().jump(target, &[]);
                self.b.switch_to_block(skipped);
                let one = self.b.ins().iconst(types::I32, 1);
                self.ret(&[one]);
                self.b.switch_to_block(target);
                let two = self.b.ins().iconst(types::I32, 2);
                self.ret(&[two]);
            }
            Op::Brif => {
                let [cond, _, _] = self.operands();
                self.branch(cond);
            }
            Op::BrIcmp => {
                let [x, y, _] = self.operands();
                let cond = self.b.ins().icmp(int_cc(aux), x, y);
                self.branch(cond);
            }
            Op::Trapz | Op::Trapnz => {
                let [cond, _, _] = self.operands();
                if shape.op == Op::Trapz {
                    self.b.ins().trapz(cond, user_trap());
                } else {
                    self.b.ins().trapnz(cond, user_trap());
                }
                let zero = self.b.ins().iconst(types::I32, 0);
                self.ret(&[zero]);
            }
        }
        Ok(())
    }
}

fn pair((a, b): (ir::Value, ir::Value)) -> Vec<ir::Value> {
    vec![a, b]
}
