//! Vectors to scalars (INTRINSICS.md 3.2, "scalarizes vector ops into ... unrolled
//! scalars"): a fixed vector's value is its lanes, each an output value, and each vector
//! operation is unrolled lane by lane. Lane `k` of a vector in memory is at byte `k * size`
//! of the lane, and lane 0 holds the lowest bits when a vector is reinterpreted as one integer
//! (LLVM LangRef, "bitcast .. to": "as if the value had been stored to memory and read back
//! as type ty2"; both targets in scope, aarch64 and x86-64, are little-endian).
//!
//! What is refused by name: masked and gathered accesses (their lanes are conditional, and the
//! pass makes no blocks), volatile vector accesses (one access may not become several),
//! vectors of `i1` in memory, a lane index past the end, pointer vectors' arithmetic, and a
//! vector crossing an ABI (a call's argument or result, a `return`, the function's own
//! parameter). An `asm!` operand is the one ABI rewritten: a vector of 64 or 128 bits passes
//! as the integer of its width (`Out::asm`).

use super::{LegalizeError, Out};
use crate::{
    Flags, Op, Ty,
    flags::IntCond,
    ty::Scalar,
};

impl Out<'_, '_, '_> {
    /// Input `inst`, an operation with a vector result or a vector operand, unrolled.
    pub(super) fn vector(&mut self, inst: usize, op: Op, ty: Ty) -> Result<(), LegalizeError> {
        let view = self.view;
        let flags = view.flags[inst];
        let n = ty.lanes();
        let lane = ty.lane();
        match op {
            Op::Uninit => {
                self.begin_lanes(inst);
                for _ in 0..n {
                    let value = self.raw(Op::Uninit, 0, lane, 0, 0);
                    self.push_lane(value);
                }
            }
            Op::Vconst => self.vconst(inst, ty)?,
            Op::Splat => {
                let scalar = self.get(view.data[inst], 0);
                self.begin_lanes(inst);
                for _ in 0..n {
                    self.push_lane(scalar);
                }
            }
            Op::Insert => {
                let (vector, value, at) = (self.word(inst, 0), self.word(inst, 1), self.word(inst, 2));
                if at >= n {
                    return Err(self.refuse(inst, "an `insert` past the vector's last lane"));
                }
                let value = self.get(value, 0);
                self.begin_lanes(inst);
                for k in 0..n {
                    let it = match k == at {
                        true => value,
                        false => self.get(vector, k),
                    };
                    self.push_lane(it);
                }
            }
            Op::Extract => {
                let (vector, at) = (self.word(inst, 0), self.word(inst, 1));
                if at >= self.ty_of(vector).lanes() {
                    return Err(self.refuse(inst, "an `extract` past the vector's last lane"));
                }
                let value = self.get(vector, at);
                self.set(inst, value);
                self.stats.scalarized += 1;
            }
            Op::Shuffle => {
                let start = view.data[inst] as usize;
                let (a, b) = (view.extra[start], view.extra[start + 1]);
                let count = view.extra[start + 2];
                if count != n {
                    return Err(self.refuse(inst, "a `shuffle` whose mask is not as long as its result"));
                }
                let from_a = self.ty_of(a).lanes();
                self.begin_lanes(inst);
                for k in 0..count as usize {
                    let index = view.extra[start + 3 + k];
                    let it = match index {
                        index if index < from_a => self.get(a, index),
                        index if index - from_a < self.ty_of(b).lanes() => self.get(b, index - from_a),
                        _ => return Err(self.refuse(inst, "a `shuffle` lane past both vectors")),
                    };
                    self.push_lane(it);
                }
            }
            Op::Swizzle => self.swizzle(inst, ty)?,
            Op::Load => self.load(inst, flags, ty)?,
            Op::Store => self.store(inst, flags)?,
            Op::Bitcast => self.bitcast(inst, ty)?,
            Op::ReduceAdd
            | Op::ReduceMul
            | Op::ReduceAnd
            | Op::ReduceOr
            | Op::ReduceXor
            | Op::ReduceUmin
            | Op::ReduceUmax
            | Op::ReduceSmin
            | Op::ReduceSmax
            | Op::ReduceFmin
            | Op::ReduceFmax => {
                let vector = self.word(inst, 0);
                let (each, flags) = match op {
                    Op::ReduceAdd => (Op::Add, 0),
                    Op::ReduceMul => (Op::Mul, 0),
                    Op::ReduceAnd => (Op::And, 0),
                    Op::ReduceOr => (Op::Or, 0),
                    Op::ReduceXor => (Op::Xor, 0),
                    Op::ReduceUmin => (Op::Umin, 0),
                    Op::ReduceUmax => (Op::Umax, 0),
                    Op::ReduceSmin => (Op::Smin, 0),
                    Op::ReduceSmax => (Op::Smax, 0),
                    // LLVM LangRef, `llvm.vector.reduce.fmin`: "This instruction has the same
                    // comparison semantics as the 'llvm.minnum.*' intrinsic".
                    Op::ReduceFmin => (Op::FminNum, flags),
                    _ => (Op::FmaxNum, flags),
                };
                let mut acc = self.get(vector, 0);
                for k in 1..self.ty_of(vector).lanes() {
                    let it = self.get(vector, k);
                    acc = self.lower_scalar(inst, each, flags, 0, ty, ty, &[acc, it])?;
                }
                self.set(inst, acc);
                self.stats.scalarized += 1;
            }
            // "The start, then the vector: in lane order, unless `reassoc`" (op.rs): lane order
            // is right with or without it.
            Op::ReduceFadd | Op::ReduceFmul => {
                let (start, vector) = (self.word(inst, 0), self.word(inst, 1));
                let each = match op {
                    Op::ReduceFadd => Op::Fadd,
                    _ => Op::Fmul,
                };
                let mut acc = self.get(start, 0);
                for k in 0..self.ty_of(vector).lanes() {
                    let it = self.get(vector, k);
                    acc = self.lower_scalar(inst, each, flags, 0, ty, ty, &[acc, it])?;
                }
                self.set(inst, acc);
                self.stats.scalarized += 1;
            }
            // "An integer of each `<n x i1>` lane's bit, lane 0 lowest" (op.rs).
            Op::Bitmask => {
                let vector = self.word(inst, 0);
                let lanes = self.ty_of(vector).lanes();
                if ty.bits(self.ptr_bits()) < lanes || !ty.is_int() {
                    return Err(self.refuse(inst, "a `bitmask` whose integer is narrower than its lanes"));
                }
                let mut acc = match ty == Ty::I1 {
                    true => self.get(vector, 0),
                    false => self.iconst(ty, 0),
                };
                for k in 0..lanes {
                    if ty == Ty::I1 {
                        break;
                    }
                    let bit = self.get(vector, k);
                    let mut it = self.emit(Op::Zext, 0, ty, 0, &[bit]);
                    if k > 0 {
                        let shift = self.iconst(ty, u128::from(k));
                        it = self.emit(Op::Shl, 0, ty, 0, &[it, shift]);
                    }
                    acc = self.emit(Op::Or, 0, ty, 0, &[acc, it]);
                }
                self.set(inst, acc);
                self.stats.scalarized += 1;
            }
            Op::MaskedLoad | Op::MaskedStore | Op::Gather | Op::Scatter => {
                return Err(self.refuse(inst, "a masked or gathered vector access (its lanes are conditional; legalize makes no blocks)"));
            }
            Op::Asm => self.asm(inst, ty)?,
            _ if lane_by_lane(op) => self.lane_by_lane(inst, op, flags, ty)?,
            _ => return Err(self.refuse(inst, "a vector operation legalize does not scalarize")),
        }
        Ok(())
    }

    /// An operation done lane by lane: each lane the scalar operation over the operands'
    /// lanes (a scalar operand, a `select`'s `i1` condition, used by every lane).
    fn lane_by_lane(&mut self, inst: usize, op: Op, flags: u8, ty: Ty) -> Result<(), LegalizeError> {
        let view = self.view;
        let mut operands = [0u32; 3];
        let mut count = 0;
        let mut cond = 0;
        match op.info().layout {
            crate::op::Layout::Data(crate::op::Field::Value) => {
                operands[0] = view.data[inst];
                count = 1;
            }
            crate::op::Layout::Extra(fields) => {
                let start = view.data[inst] as usize;
                for (k, &field) in fields.iter().enumerate() {
                    let word = view.extra[start + k];
                    match field {
                        crate::op::Field::IntCond | crate::op::Field::FloatCond => cond = word,
                        crate::op::Field::Value if count < 3 => {
                            operands[count] = word;
                            count += 1;
                        }
                        _ => return Err(self.refuse(inst, "a lane-by-lane operation of an unexpected form")),
                    }
                }
            }
            crate::op::Layout::None | crate::op::Layout::Data(_) => {
                return Err(self.refuse(inst, "a lane-by-lane operation of an unexpected form"));
            }
        }
        let src = match op {
            Op::Select => ty.lane(),
            _ => self.ty_of(operands[0]).lane(),
        };
        self.begin_lanes(inst);
        for k in 0..ty.lanes() {
            let mut args = [0u32; 3];
            for j in 0..count {
                let lane = match self.ty_of(operands[j]).is_vector() {
                    true => k,
                    false => 0,
                };
                args[j] = self.get(operands[j], lane);
            }
            let value = self.lower_scalar(inst, op, flags, cond, ty.lane(), src, &args[..count])?;
            self.push_lane(value);
        }
        Ok(())
    }

    /// A vector constant: lane `k` is bits `k * w ..` of its little-endian words.
    fn vconst(&mut self, inst: usize, ty: Ty) -> Result<(), LegalizeError> {
        let lane = ty.lane();
        if lane.is_ptr() {
            return Err(self.refuse(inst, "a vector constant of pointers"));
        }
        let width = lane.bits(self.ptr_bits());
        let start = self.view.data[inst] as usize;
        let words = &self.view.extra[start..start + ty.const_words() as usize];
        self.begin_lanes(inst);
        for k in 0..ty.lanes() {
            let mut bits = 0u128;
            for i in 0..width {
                let at = k * width + i;
                if (words[(at / 32) as usize] >> (at % 32)) & 1 == 1 {
                    bits |= 1 << i;
                }
            }
            let value = match lane.is_float() {
                true => self.fconst(lane, bits),
                false => self.iconst(lane, bits),
            };
            self.push_lane(value);
        }
        Ok(())
    }

    /// `swizzle table, index` (op.rs: "The lanes of the first vector the second's lanes index
    /// (a table lookup); 0 for one out of range"): per lane, a chain of selects over the
    /// table's lanes, starting from 0.
    fn swizzle(&mut self, inst: usize, ty: Ty) -> Result<(), LegalizeError> {
        let (table, index) = (self.word(inst, 0), self.word(inst, 1));
        let index_lane = self.ty_of(index).lane();
        let lane = ty.lane();
        if !index_lane.is_int() {
            return Err(self.refuse(inst, "a `swizzle` whose indices are not integers"));
        }
        let zero = match (lane.is_float(), lane.is_int()) {
            (true, _) => self.fconst(lane, 0),
            (_, true) => self.iconst(lane, 0),
            _ => return Err(self.refuse(inst, "a `swizzle` of pointers")),
        };
        let entries = self.ty_of(table).lanes();
        self.begin_lanes(inst);
        for k in 0..ty.lanes() {
            let at = self.get(index, k);
            let mut picked = zero;
            for j in 0..entries {
                let j_const = self.iconst(index_lane, u128::from(j));
                let hit = self.icmp(IntCond::Eq, at, j_const);
                let entry = self.get(table, j);
                picked = self.emit(Op::Select, 0, lane, 0, &[hit, entry, picked]);
            }
            self.push_lane(picked);
        }
        Ok(())
    }

    /// The byte size of a lane of `ty` in memory; refused for `i1` lanes (bits, not bytes).
    fn lane_bytes(&self, inst: usize, ty: Ty) -> Result<u32, LegalizeError> {
        match ty.elem() {
            Scalar::I1 => Err(self.refuse(inst, "a load or store of a vector of `i1` (its lanes are bits)")),
            elem => Ok(elem.bits(self.ptr_bits()) / 8),
        }
    }

    /// The offset of lane `k` from an access at `offset`.
    fn lane_offset(&self, inst: usize, offset: u32, k: u32, size: u32) -> Result<(u32, u32), LegalizeError> {
        let from = k * size;
        let Some(at) = i32::try_from(from).ok().and_then(|from| (offset as i32).checked_add(from)) else {
            return Err(self.refuse(inst, "a vector access whose lane offsets do not fit an `i32`"));
        };
        Ok((at as u32, from))
    }

    /// `load <n x T>`: one load per lane.
    fn load(&mut self, inst: usize, flags: u8, ty: Ty) -> Result<(), LegalizeError> {
        if flags & Flags::VOLATILE.0 != 0 {
            return Err(self.refuse(inst, "a volatile vector access (one access may not become several)"));
        }
        let size = self.lane_bytes(inst, ty)?;
        let (ptr, offset) = (self.word(inst, 0), self.word(inst, 1));
        let ptr = self.get(ptr, 0);
        let attrs = self.view.attrs[inst];
        self.begin_lanes(inst);
        for k in 0..ty.lanes() {
            let (at, from) = self.lane_offset(inst, offset, k, size)?;
            let lane_attrs = self.lane_attrs(attrs, from);
            let start = self.extra_at();
            self.push_value(ptr);
            self.push_word(at);
            let value = self.raw(Op::Load, flags, ty.lane(), start, lane_attrs);
            self.push_lane(value);
        }
        Ok(())
    }

    /// `store <n x T>`: one store per lane.
    fn store(&mut self, inst: usize, flags: u8) -> Result<(), LegalizeError> {
        if flags & Flags::VOLATILE.0 != 0 {
            return Err(self.refuse(inst, "a volatile vector access (one access may not become several)"));
        }
        let (value, ptr, offset) = (self.word(inst, 0), self.word(inst, 1), self.word(inst, 2));
        let ty = self.ty_of(value);
        let size = self.lane_bytes(inst, ty)?;
        let ptr = self.get(ptr, 0);
        let attrs = self.view.attrs[inst];
        for k in 0..ty.lanes() {
            let (at, from) = self.lane_offset(inst, offset, k, size)?;
            let lane_attrs = self.lane_attrs(attrs, from);
            let lane = self.get(value, k);
            let start = self.extra_at();
            self.push_value(lane);
            self.push_value(ptr);
            self.push_word(at);
            self.raw(Op::Store, flags, Ty::NONE, start, lane_attrs);
        }
        self.stats.scalarized += 1;
        Ok(())
    }

    /// `bitcast` to or from a vector: lane by lane when both are vectors of one lane count,
    /// else through one integer of the whole width (at most 128 bits), lane 0 lowest.
    fn bitcast(&mut self, inst: usize, ty: Ty) -> Result<(), LegalizeError> {
        let from = self.word(inst, 0);
        let src = self.ty_of(from);
        let ptr_bits = self.ptr_bits();
        if src.is_ptr() || ty.is_ptr() {
            return Err(self.refuse(inst, "a `bitcast` of pointers"));
        }
        if src.is_vector() && ty.is_vector() && src.lanes() == ty.lanes() {
            self.begin_lanes(inst);
            for k in 0..ty.lanes() {
                let it = self.get(from, k);
                let value = match src.lane() == ty.lane() {
                    true => it,
                    false => self.emit(Op::Bitcast, 0, ty.lane(), 0, &[it]),
                };
                self.push_lane(value);
            }
            return Ok(());
        }
        let total = ty.bits(ptr_bits);
        let Some(int) = Ty::int(total) else {
            return Err(self.refuse(inst, "a `bitcast` between vector shapes of more than 128 bits"));
        };
        let whole = match src.is_vector() {
            true => self.pack(inst, from, src, int)?,
            false => {
                let it = self.get(from, 0);
                match src.is_float() {
                    true => self.emit(Op::Bitcast, 0, int, 0, &[it]),
                    false => it,
                }
            }
        };
        if !ty.is_vector() {
            let value = match ty.is_float() {
                true => self.emit(Op::Bitcast, 0, ty, 0, &[whole]),
                false => whole,
            };
            self.set(inst, value);
            self.stats.scalarized += 1;
            return Ok(());
        }
        self.unpack(inst, whole, ty, int)
    }

    /// The lanes of input vector `from` (of type `src`) as one output integer of type `int`,
    /// its whole width: lane 0 lowest, each float lane by its bits.
    fn pack(&mut self, inst: usize, from: u32, src: Ty, int: Ty) -> Result<u32, LegalizeError> {
        let ptr_bits = self.ptr_bits();
        let total = int.bits(ptr_bits);
        let width = src.lane().bits(ptr_bits);
        let Some(lane_int) = Ty::int(width) else {
            return Err(self.refuse(inst, "a vector whose lanes are no integer width, as one integer (a `bitcast`, an `asm!` operand)"));
        };
        let mut acc = None;
        for k in 0..src.lanes() {
            let mut it = self.get(from, k);
            if src.lane().is_float() {
                it = self.emit(Op::Bitcast, 0, lane_int, 0, &[it]);
            }
            if width < total {
                it = self.emit(Op::Zext, 0, int, 0, &[it]);
            }
            if k > 0 {
                let shift = self.iconst(int, u128::from(k * width));
                it = self.emit(Op::Shl, 0, int, 0, &[it, shift]);
            }
            acc = Some(match acc {
                None => it,
                Some(acc) => self.emit(Op::Or, 0, int, 0, &[acc, it]),
            });
        }
        acc.ok_or(self.refuse(inst, "a vector of no lanes as one integer (a `bitcast`, an `asm!` operand)"))
    }

    /// Input `inst`'s result, a vector of type `ty`, as the lanes of output integer `whole` (of
    /// type `int`, the vector's whole width): [`Out::pack`] undone.
    fn unpack(&mut self, inst: usize, whole: u32, ty: Ty, int: Ty) -> Result<(), LegalizeError> {
        let ptr_bits = self.ptr_bits();
        let total = int.bits(ptr_bits);
        let width = ty.lane().bits(ptr_bits);
        let Some(lane_int) = Ty::int(width) else {
            return Err(self.refuse(inst, "a vector whose lanes are no integer width, from one integer (a `bitcast`, an `asm!` output)"));
        };
        self.begin_lanes(inst);
        for k in 0..ty.lanes() {
            let mut it = whole;
            if k > 0 {
                let shift = self.iconst(int, u128::from(k * width));
                it = self.emit(Op::Lshr, 0, int, 0, &[whole, shift]);
            }
            if width < total {
                it = self.emit(Op::Trunc, 0, lane_int, 0, &[it]);
            }
            if ty.lane().is_float() {
                it = self.emit(Op::Bitcast, 0, ty.lane(), 0, &[it]);
            }
            self.push_lane(it);
        }
        Ok(())
    }

    /// Whether `asm` instruction `inst` takes a vector operand (mod.rs `reads_vector`).
    pub(super) fn asm_reads_vector(&self, inst: usize) -> bool {
        let view = self.view;
        let at = view.data[inst] as usize + 1;
        let n = view.extra[at] as usize;
        view.extra[at + 1..at + 1 + n].iter().any(|&value| self.ty_of(value).is_vector())
    }

    /// An `asm` instruction with vector operands or a vector result (AArch64's `vreg` takes
    /// `i8x8`..`f64x2`, constant_time_eq's `uint8x16_t`): the block is the same with each
    /// vector as the integer of its whole width, 64 or 128 bits ([`Out::pack`] before it, lane 0
    /// lowest, as a vector register holds it, and [`Out::unpack`] after). The emitter places
    /// that integer in the vector register (blaze-emit pass/asm.rs: `ldr q` of the cell), so
    /// the block sees the vector's bytes in the register's low bits, as the Reference has it.
    /// A vector of another width, and a result holding a vector among several (mod.rs refuses
    /// it before this), are refused by name.
    fn asm(&mut self, inst: usize, ty: Ty) -> Result<(), LegalizeError> {
        let view = self.view;
        let flags = view.flags[inst];
        let attrs = view.attrs[inst];
        let at = view.data[inst] as usize;
        let id = view.extra[at];
        let n = view.extra[at + 1] as usize;
        let whole = |out: &Self, vector: Ty| match vector.bits(out.ptr_bits()) {
            64 => Ok(Ty::I64),
            128 => Ok(Ty::I128),
            _ => Err(out.refuse(inst, "a vector `asm!` operand of other than 64 or 128 bits (a `vreg` holds a `d` or a `q`)")),
        };
        // The operands first (each pack is instructions of its own), then the block's words.
        let mut renamed = [0u32; 32];
        if n > renamed.len() {
            return Err(self.refuse(inst, "an `asm!` block of more than 32 inputs"));
        }
        for k in 0..n {
            let value = view.extra[at + 2 + k];
            let vector = self.ty_of(value);
            renamed[k] = match vector.is_vector() {
                true => {
                    let int = whole(self, vector)?;
                    self.pack(inst, value, vector, int)?
                }
                false => self.get(value, 0),
            };
        }
        let result = match ty.is_vector() {
            true => whole(self, ty)?,
            false => ty,
        };
        let start = self.extra_at();
        self.push_word(id);
        self.push_word(n as u32);
        for &value in &renamed[..n] {
            self.push_value(value);
        }
        let made = self.raw(Op::Asm, flags, result, start, attrs);
        match ty.is_vector() {
            true => self.unpack(inst, made, ty, result)?,
            false => self.set(inst, made),
        }
        self.stats.scalarized += 1;
        Ok(())
    }
}

/// Whether `op` on vectors is the scalar operation on each lane (op.rs: arithmetic,
/// bitwise operations, shifts, comparisons, `select` and conversions are "lane by lane").
fn lane_by_lane(op: Op) -> bool {
    matches!(
        op,
        Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Neg
            | Op::Udiv
            | Op::Sdiv
            | Op::Urem
            | Op::Srem
            | Op::And
            | Op::Or
            | Op::Xor
            | Op::Not
            | Op::Shl
            | Op::Lshr
            | Op::Ashr
            | Op::Rotl
            | Op::Rotr
            | Op::Umulhi
            | Op::Smulhi
            | Op::UaddSat
            | Op::SaddSat
            | Op::UsubSat
            | Op::SsubSat
            | Op::Umin
            | Op::Umax
            | Op::Smin
            | Op::Smax
            | Op::Clz
            | Op::Ctz
            | Op::Popcnt
            | Op::Bswap
            | Op::Bitrev
            | Op::Icmp
            | Op::Fcmp
            | Op::Select
            | Op::Zext
            | Op::Sext
            | Op::Trunc
            | Op::Fpext
            | Op::Fptrunc
            | Op::FptoUi
            | Op::FptoSi
            | Op::UitoFp
            | Op::SitoFp
            | Op::Fadd
            | Op::Fsub
            | Op::Fmul
            | Op::Fdiv
            | Op::Frem
            | Op::FminNum
            | Op::FmaxNum
            | Op::Fminimum
            | Op::Fmaximum
            | Op::Fcopysign
            | Op::Fneg
            | Op::Fabs
            | Op::Sqrt
            | Op::Floor
            | Op::Ceil
            | Op::Ftrunc
            | Op::Nearest
            | Op::Fround
            | Op::Fma
            | Op::Fmuladd
            | Op::Freeze
    )
}
