//! Legalize (port27/tempo-run/INTRINSICS.md 3.2 and 3.5): what the Reference and Fast tiers
//! take is LIR with no vector, no `f16` arithmetic and no `f128` arithmetic. The Optimized tier
//! keeps all three for LLVM and never runs this pass.
//!
//! - **Vectors to scalars** ([`vector`]): every fixed-vector value becomes its lanes, each a
//!   scalar value, and every vector operation the same operation lane by lane, unrolled (a lane
//!   count is at most 64, [`crate::ty::VECTOR_BITS`] bits). A vector block parameter becomes
//!   one parameter per lane and every edge to it passes the lanes. Scalable vectors are
//!   refused ([`LegalizeError::Scalable`]).
//! - **`f16` widened** ([`half`]): `+ - * /`, `sqrt`, `%`, min/max and rounding to an integer
//!   are done in `f32` and truncated back, which is exact: `f32` has 24 >= 2 * 11 + 2
//!   significand bits, so the double rounding is innocuous (Figueroa, "When is double rounding
//!   innocuous?", 1995). Sign operations work on the bits.
//! - **`f128` to calls** ([`quad`]): every `f128` operation becomes a call to the pinned
//!   compiler_builtins' `#[no_mangle]` routine for it ([`SoftFloat`]), bound by the caller
//!   ([`QuadRoutines`]); a missing one is [`LegalizeError::NoRoutine`].
//!
//! What the tiers must still take after this pass: `f16` and `f128` values that are only moved
//! (loaded, stored, selected, passed, bitcast), `fpext f16 -> f32` and `fptrunc f32 -> f16`,
//! and `i128` bit operations.
//!
//! The pass is linear: one walk over the instructions, then one walk over the uses it could
//! not name yet (a use before its definition in layout order, which dominance allows), then the
//! verifier. No recursion; every buffer is in the caller's allocator.
//!
//! A change to what this pass emits for an unchanged input is a change of [`VERSION`], which
//! the Reference and Fast tiers' record keys hold.

use core::fmt;

use blaze_own::{Alloc, Vec};

use crate::{
    Func, Op, Ty, View, VerifyError,
    flags::{FloatCond, IntCond},
    func::{Block, Conv, Ext, MemAttrsRec, NONE, ParamKind, ParamRec, RefId, SigId, SigRec},
    op::{Field, Layout},
    ty::{Form, Scalar},
    verify,
};

mod half;
mod quad;
mod vector;

#[cfg(test)]
mod tests;

pub use quad::SoftFloat;

/// The version of what [`legalize`] emits: part of the key of every Reference and Fast tier
/// record made from its output.
pub const VERSION: u32 = 1;

/// Where the program's own compiler_builtins routines are: the function's `refs` row of each
/// one the caller bound (`builtins_shim`, INTRINSICS.md 3.5), `None` for one it did not.
pub trait QuadRoutines {
    fn routine(&self, r: SoftFloat) -> Option<RefId>;
}

/// What [`legalize`] did: for a tier's profile span and for the binder of routines.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Legalized {
    /// Vector instructions made scalar.
    pub scalarized: u32,
    /// `f16` operations done in `f32` or on their bits.
    pub widened: u32,
    /// Routine calls made.
    pub calls: u32,
    /// The routines called: bit [`SoftFloat::bit`] of each.
    pub routines: u32,
}

/// Why [`legalize`] refused a function. Every refusal names the form; the caller names the
/// instance.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LegalizeError {
    /// The routine an `f128` (or `f16`) operation needs was not bound by the caller.
    NoRoutine(SoftFloat),
    /// A scalable vector (SVE): the Reference and Fast tiers have no form for one
    /// (INTRINSICS.md 3.6: "SVE: refused").
    Scalable,
    /// Instruction `inst` of the input, of operation `op`, is a form this pass does not take:
    /// `what` says which.
    Refused { inst: usize, op: Op, what: &'static str },
    /// The output would not fit LIR's columns.
    Full { table: &'static str },
    /// The output is not LIR: a bug of this pass, named by the verifier's rule.
    Verify(VerifyError),
}

impl fmt::Display for LegalizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LegalizeError::NoRoutine(r) => {
                write!(f, "legalize: the routine `{}` is not bound (compiler_builtins of the program)", r.symbol())
            }
            LegalizeError::Scalable => f.write_str("legalize: a scalable vector (refused before the Optimized tier)"),
            LegalizeError::Refused { inst, op, what } => {
                write!(f, "legalize: instruction v{inst} (`{}`): {what}", op.name())
            }
            LegalizeError::Full { table } => write!(f, "legalize: the output's {table} is full"),
            LegalizeError::Verify(error) => write!(f, "legalize: its output is not LIR: {error}"),
        }
    }
}

impl core::error::Error for LegalizeError {}

/// Rewrites `func` in place so it holds no vector, `f16` arithmetic or `f128` arithmetic (see
/// the module), its new columns in `alloc`, verified. A function with none of them is left
/// as it is.
pub fn legalize<'a>(
    func: &mut Func<Alloc<'a>>,
    routines: &dyn QuadRoutines,
    alloc: Alloc<'a>,
) -> Result<Legalized, LegalizeError> {
    let made = {
        let view = func.view();
        if !needs(&view)? {
            return Ok(Legalized::default());
        }
        let mut out = Out::new(view, routines, alloc);
        out.run()?;
        out.finish()?
    };
    let (new, stats) = made;
    verify(new.view()).map_err(LegalizeError::Verify)?;
    *func = new;
    Ok(stats)
}

/// Whether [`legalize`] would rewrite `view` (it holds a vector, or an `f16` or `f128` value):
/// a caller that keeps its input when nothing needs rewriting (the record emitter) asks this
/// before copying the function. A scalable vector is refused here.
pub fn needed(view: &View<'_>) -> Result<bool, LegalizeError> {
    needs(view)
}

/// Whether any instruction's type is a vector, an `f16` or `f128` scalar, or a compound: the
/// forms the pass rewrites or must refuse. A scalable vector is refused here.
fn needs(view: &View<'_>) -> Result<bool, LegalizeError> {
    let mut any = false;
    for &bits in view.tys {
        let ty = Ty(bits);
        match ty.form() {
            Form::Scalable => return Err(LegalizeError::Scalable),
            Form::Vector => any = true,
            Form::Compound => {
                for &member in view.members(ty) {
                    match Ty(member).form() {
                        Form::Scalable => return Err(LegalizeError::Scalable),
                        Form::Vector => any = true,
                        Form::Scalar | Form::Compound => {}
                    }
                }
            }
            Form::Scalar => any |= matches!(ty.elem(), Scalar::F16 | Scalar::F128),
        }
    }
    Ok(any)
}

/// A use not yet named: the high bit of a value word marks it, the rest is its row of
/// `pending`. A real value is below it ([`Out::finish`] refuses a longer output).
const PENDING: u32 = 1 << 31;

/// Where a word holding a [`PENDING`] value sits in the output.
#[derive(Clone, Copy)]
struct Hole {
    /// In `data` (else in `extra`).
    data: bool,
    at: u32,
}

const _: () = {
    // `cond as u32` is the word `IntCond::from_word`/`FloatCond::from_word` read: both index
    // `ALL`, which must list the variants in declaration order.
    let mut k = 0;
    while k < IntCond::ALL.len() {
        assert!(IntCond::ALL[k] as usize == k);
        k += 1;
    }
    let mut k = 0;
    while k < FloatCond::ALL.len() {
        assert!(FloatCond::ALL[k] as usize == k);
        k += 1;
    }
};

/// The output being made.
pub(super) struct Out<'v, 'r, 'a> {
    view: View<'v>,
    routines: &'r dyn QuadRoutines,
    func: Func<Alloc<'a>>,
    /// Per input instruction: its output value (a scalar) or the row of `lanes` where its
    /// lanes start (a vector); [`NONE`] until made. A scalar may be a [`PENDING`] word (an
    /// `extract` of a lane not yet made).
    map: Vec<u32, Alloc<'a>>,
    /// The lanes of the vectors, each an output value or a [`PENDING`] word.
    lanes: Vec<u32, Alloc<'a>>,
    /// Per pending use: the input value and the lane.
    pending: Vec<(u32, u32), Alloc<'a>>,
    holes: Vec<Hole, Alloc<'a>>,
    /// The `locs` word of the input instruction being rewritten.
    loc: u32,
    stats: Legalized,
}

impl<'v, 'r, 'a> Out<'v, 'r, 'a> {
    fn new(view: View<'v>, routines: &'r dyn QuadRoutines, alloc: Alloc<'a>) -> Out<'v, 'r, 'a> {
        let mut func = view.to_func_in(alloc);
        func.ops.clear();
        func.flags.clear();
        func.tys.clear();
        func.data.clear();
        func.extra.clear();
        func.attrs.clear();
        func.locs.clear();
        func.block_starts.clear();
        func.block_kinds.clear();
        let mut map = Vec::with_capacity_in(view.len(), alloc);
        for _ in 0..view.len() {
            map.push(NONE);
        }
        Out {
            view,
            routines,
            func,
            map,
            lanes: Vec::new_in(alloc),
            pending: Vec::new_in(alloc),
            holes: Vec::new_in(alloc),
            loc: 0,
            stats: Legalized::default(),
        }
    }

    fn run(&mut self) -> Result<(), LegalizeError> {
        let view = self.view;
        for b in 0..view.blocks() {
            self.func.block_starts.push(self.func.ops.len() as u32);
            self.func.block_kinds.push(view.block_kinds[b]);
            for inst in view.block(Block(b as u32)) {
                self.loc = view.locs[inst];
                self.inst(b, inst)?;
            }
        }
        Ok(())
    }

    /// Names every pending use, and hands back the output.
    fn finish(mut self) -> Result<(Func<Alloc<'a>>, Legalized), LegalizeError> {
        if self.func.ops.len() >= PENDING as usize || self.func.extra.len() > u32::MAX as usize {
            return Err(LegalizeError::Full { table: "instructions" });
        }
        for h in 0..self.holes.len() {
            let hole = self.holes[h];
            let word = match hole.data {
                true => self.func.data[hole.at as usize],
                false => self.func.extra[hole.at as usize],
            };
            let value = self.resolve(word)?;
            match hole.data {
                true => self.func.data[hole.at as usize] = value,
                false => self.func.extra[hole.at as usize] = value,
            }
        }
        Ok((self.func, self.stats))
    }

    /// The output value a [`PENDING`] word stands for: a chain of aliases (an `extract` of a
    /// `splat` of a later value) followed at most once per pending row.
    fn resolve(&self, mut word: u32) -> Result<u32, LegalizeError> {
        let mut steps = 0usize;
        while word & PENDING != 0 && word != NONE {
            if steps > self.pending.len() {
                return Err(self.refuse(0, "a use that names itself (a cycle of aliases)"));
            }
            steps += 1;
            let (old, lane) = self.pending[(word & !PENDING) as usize];
            let at = self.map[old as usize];
            if at == NONE {
                return Err(self.refuse(old as usize, "a use of a value that is never defined"));
            }
            word = match Ty(self.view.tys[old as usize]).is_vector() {
                true => self.lanes[at as usize + lane as usize],
                false => at,
            };
        }
        match word {
            NONE => Err(self.refuse(0, "a use of a value that is never defined")),
            it => Ok(it),
        }
    }

    pub(super) fn refuse(&self, inst: usize, what: &'static str) -> LegalizeError {
        let op = match inst < self.view.len() {
            true => self.view.op(inst),
            false => Op::Param,
        };
        LegalizeError::Refused { inst, op, what }
    }

    // Reading the input.

    pub(super) fn ptr_bits(&self) -> u32 {
        self.view.head.ptr_bits
    }

    /// The type of input value `value`.
    pub(super) fn ty_of(&self, value: u32) -> Ty {
        Ty(self.view.tys[value as usize])
    }

    /// The `k`th operand word of input instruction `inst`, for an operation whose layout is
    /// one `Value` in `data` (`k` = 0) or whose operands start with `k + 1` single words.
    pub(super) fn word(&self, inst: usize, k: usize) -> u32 {
        match self.view.op(inst).info().layout {
            Layout::Data(_) => self.view.data[inst],
            Layout::None | Layout::Extra(_) => self.view.extra[self.view.data[inst] as usize + k],
        }
    }

    /// Lane `lane` of input value `old` as an output value word: the value itself for a
    /// scalar, its lane for a vector; a [`PENDING`] word when it is not made yet.
    pub(super) fn get(&mut self, old: u32, lane: u32) -> u32 {
        let at = self.map[old as usize];
        if at != NONE {
            return match self.ty_of(old).is_vector() {
                true => self.lanes[at as usize + lane as usize],
                false => at,
            };
        }
        let id = self.pending.len() as u32;
        self.pending.push((old, lane));
        PENDING | id
    }

    /// Records input `inst`'s output: a scalar value.
    pub(super) fn set(&mut self, inst: usize, value: u32) {
        self.map[inst] = value;
    }

    /// Starts input `inst`'s output, a vector: its lanes follow by [`Out::push_lane`], in
    /// order, with no other vector's between.
    pub(super) fn begin_lanes(&mut self, inst: usize) {
        self.map[inst] = self.lanes.len() as u32;
        self.stats.scalarized += 1;
    }

    pub(super) fn push_lane(&mut self, value: u32) {
        self.lanes.push(value);
    }

    // Writing the output.

    /// Appends one instruction; its value.
    pub(super) fn raw(&mut self, op: Op, flags: u8, ty: Ty, data: u32, attrs: u32) -> u32 {
        let at = self.func.ops.len() as u32;
        self.func.ops.push(op as u8);
        self.func.flags.push(flags);
        self.func.tys.push(ty.0);
        self.func.data.push(data);
        self.func.attrs.push(attrs);
        self.func.locs.push(self.loc);
        at
    }

    /// Appends a value word to `extra`, noting it when it is pending.
    pub(super) fn push_value(&mut self, value: u32) {
        if value & PENDING != 0 {
            self.holes.push(Hole { data: false, at: self.func.extra.len() as u32 });
        }
        self.func.extra.push(value);
    }

    pub(super) fn extra_at(&self) -> u32 {
        self.func.extra.len() as u32
    }

    pub(super) fn push_word(&mut self, word: u32) {
        self.func.extra.push(word);
    }

    /// An instruction of a plain operation over `args`: one `Value` in `data`, or `extra`
    /// fields of values and at most one condition (`cond`, its word).
    pub(super) fn emit(&mut self, op: Op, flags: u8, ty: Ty, cond: u32, args: &[u32]) -> u32 {
        match op.info().layout {
            Layout::Data(Field::Value) => {
                let at = self.func.ops.len() as u32;
                if args[0] & PENDING != 0 {
                    self.holes.push(Hole { data: true, at });
                }
                self.raw(op, flags, ty, args[0], 0)
            }
            Layout::None | Layout::Data(_) => self.raw(op, flags, ty, 0, 0),
            Layout::Extra(fields) => {
                let start = self.extra_at();
                let mut k = 0;
                for &field in fields {
                    match field {
                        Field::IntCond | Field::FloatCond => self.push_word(cond),
                        _ => {
                            self.push_value(args[k]);
                            k += 1;
                        }
                    }
                }
                self.raw(op, flags, ty, start, 0)
            }
        }
    }

    /// An integer constant of scalar type `ty`, encoded as [`crate::Builder::iconst`] does.
    pub(super) fn iconst(&mut self, ty: Ty, value: u128) -> u32 {
        let bits = ty.bits(self.ptr_bits());
        let value = match bits {
            128.. => value,
            bits => value & ((1u128 << bits) - 1),
        };
        if bits <= 32 {
            return self.raw(Op::Iconst, 0, ty, value as u32, 0);
        }
        let shift = 128 - bits;
        let signed = ((value << shift) as i128) >> shift;
        match i32::try_from(signed) {
            Ok(small) => self.raw(Op::Iconst, 0, ty, small as u32, 0),
            Err(_) => {
                let start = self.extra_at();
                for word in 0..ty.const_words() {
                    self.push_word((value >> (32 * word)) as u32);
                }
                self.raw(Op::IconstWide, 0, ty, start, 0)
            }
        }
    }

    /// A float constant of scalar type `ty` of the bits `bits`, as [`crate::Builder::fconst`]
    /// encodes one.
    pub(super) fn fconst(&mut self, ty: Ty, bits: u128) -> u32 {
        let words = match ty.elem() {
            Scalar::F16 => return self.raw(Op::Fconst, 0, ty, bits as u16 as u32, 0),
            Scalar::F32 => return self.raw(Op::Fconst, 0, ty, bits as u32, 0),
            Scalar::F64 => 2,
            _ => 4,
        };
        let start = self.extra_at();
        for word in 0..words {
            self.push_word((bits >> (32 * word)) as u32);
        }
        self.raw(Op::FconstWide, 0, ty, start, 0)
    }

    /// `icmp cond a, b`.
    pub(super) fn icmp(&mut self, cond: IntCond, a: u32, b: u32) -> u32 {
        self.emit(Op::Icmp, 0, Ty::I1, cond as u32, &[a, b])
    }

    /// A call of routine `r` with `args`: its one result.
    pub(super) fn routine(&mut self, inst: usize, r: SoftFloat, args: &[u32]) -> Result<u32, LegalizeError> {
        let Some(callee) = self.routines.routine(r) else {
            return Err(LegalizeError::NoRoutine(r));
        };
        if callee.index() >= self.func.refs.len() {
            return Err(self.refuse(inst, "a routine bound to a reference the function does not have"));
        }
        let (params, ret) = r.signature();
        if params.len() != args.len() {
            return Err(self.refuse(inst, "a routine called with another number of arguments than it takes"));
        }
        let sig = self.sig_of(params, ret)?;
        let start = self.extra_at();
        self.push_word(callee.0);
        self.push_word(sig.0);
        self.push_word(args.len() as u32);
        for &arg in args {
            self.push_value(arg);
        }
        self.stats.calls += 1;
        self.stats.routines |= r.bit();
        Ok(self.raw(Op::Call, 0, ret, start, 0))
    }

    /// The row of the C signature `(params) -> (ret)`, made once.
    fn sig_of(&mut self, params: &[Ty], ret: Ty) -> Result<SigId, LegalizeError> {
        let rec = |ty: Ty| ParamRec { facts: 0, ty: ty.0, ext: Ext::None as u8, kind: ParamKind::Value as u8 };
        for (row, have) in self.func.sigs.iter().enumerate() {
            if have.conv != Conv::C as u8 || have.bits != 0 || have.params as usize != params.len() || have.rets != 1 {
                continue;
            }
            let recs = &self.func.sig_params[have.first as usize..][..params.len() + 1];
            let same = params.iter().zip(recs).all(|(&ty, have)| *have == rec(ty)) && recs[params.len()] == rec(ret);
            if same {
                return Ok(SigId(row as u32));
            }
        }
        let row = u32::try_from(self.func.sigs.len()).map_err(|_| LegalizeError::Full { table: "signatures" })?;
        let first =
            u32::try_from(self.func.sig_params.len()).map_err(|_| LegalizeError::Full { table: "signature parameters" })?;
        for &ty in params {
            self.func.sig_params.push(rec(ty));
        }
        self.func.sig_params.push(rec(ret));
        self.func.sigs.push(SigRec {
            first,
            params: params.len() as u16,
            rets: 1,
            conv: Conv::C as u8,
            bits: 0,
            pad: [0; 2],
        });
        Ok(SigId(row))
    }

    /// The `attrs` word of lane access at byte `offset` from a vector access with `attrs`:
    /// its alignment claim lowered to what the lane's address keeps, its range dropped (a
    /// promise of the whole vector's scalar, which a lane does not have; dropping a promise
    /// never makes code wrong), the rest kept (each holds of every lane).
    pub(super) fn lane_attrs(&mut self, attrs: u32, offset: u32) -> u32 {
        let Some(rec) = attrs.checked_sub(1).map(|row| self.view.mem_attrs[row as usize]) else {
            return 0;
        };
        let align = match offset {
            0 => rec.align,
            offset => rec.align.min(offset.trailing_zeros() as u8),
        };
        let lane = MemAttrsRec { range: 0, align, src_align: 0, pointee_align: rec.pointee_align, bits: rec.bits };
        // A row that claims nothing is written as none (verify.rs, `mem_attrs`): a byte lane
        // at an odd offset keeps no alignment, and may keep nothing else.
        if lane == MemAttrsRec::default() {
            return 0;
        }
        if let Some(row) = self.func.mem_attrs.iter().position(|have| *have == lane) {
            return row as u32 + 1;
        }
        self.func.mem_attrs.push(lane);
        self.func.mem_attrs.len() as u32
    }

    // The instructions.

    fn inst(&mut self, block: usize, inst: usize) -> Result<(), LegalizeError> {
        let view = self.view;
        let op = view.op(inst);
        let ty = Ty(view.tys[inst]);
        if let Form::Compound = ty.form() {
            for &member in view.members(ty) {
                if Ty(member).is_vector() {
                    return Err(self.refuse(inst, "a result holding a vector among several (an overflow operation on vectors, a call's results)"));
                }
            }
        }
        if op == Op::Param {
            if !ty.is_vector() {
                let value = self.raw(Op::Param, view.flags[inst], ty, 0, 0);
                self.set(inst, value);
                return Ok(());
            }
            if block == 0 {
                return Err(self.refuse(inst, "a vector parameter of the function (its signature is its ABI)"));
            }
            self.begin_lanes(inst);
            for _ in 0..ty.lanes() {
                let lane = self.raw(Op::Param, 0, ty.lane(), 0, 0);
                self.push_lane(lane);
            }
            return Ok(());
        }
        if ty.is_vector() || self.reads_vector(inst, op) {
            return self.vector(inst, op, ty);
        }
        self.scalar_inst(inst, op, ty)
    }

    /// Whether `op` at `inst` has a scalar result made from a vector operand: the forms
    /// [`Out::vector`] takes beside those with a vector result.
    fn reads_vector(&self, inst: usize, op: Op) -> bool {
        let first = |k: usize| self.ty_of(self.word(inst, k)).is_vector();
        match op {
            Op::Extract | Op::Bitmask | Op::Bitcast | Op::Store | Op::MaskedStore | Op::Scatter => first(0),
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
            | Op::ReduceFmax => first(0),
            Op::ReduceFadd | Op::ReduceFmul => first(1),
            // An `asm!` block with a vector operand (lane T13: vector.rs `Out::asm` packs it).
            Op::Asm => self.asm_reads_vector(inst),
            _ => false,
        }
    }

    /// A scalar instruction: an `f16` or `f128` operation lowered, anything else copied with
    /// its operands renamed.
    fn scalar_inst(&mut self, inst: usize, op: Op, ty: Ty) -> Result<(), LegalizeError> {
        if let Some((cond, args, n)) = self.plain_args(inst, op) {
            let src = match n {
                0 => ty,
                _ => self.ty_of(self.word(inst, usize::from(matches!(op, Op::Icmp | Op::Fcmp)))),
            };
            let flags = self.view.flags[inst];
            let value = self.lower_scalar(inst, op, flags, cond, ty, src, &args[..n])?;
            self.set(inst, value);
            return Ok(());
        }
        self.copy(inst, op, ty)
    }

    /// For an operation that may be an `f16` or `f128` one (arithmetic, comparison,
    /// conversion): its condition word, its operands renamed and their count; `None` for
    /// another operation.
    fn plain_args(&mut self, inst: usize, op: Op) -> Option<(u32, [u32; 3], usize)> {
        if !float_kind(op) {
            return None;
        }
        let mut args = [0u32; 3];
        let mut n = 0;
        let mut cond = 0;
        match op.info().layout {
            Layout::Data(Field::Value) => {
                args[0] = self.get(self.view.data[inst], 0);
                n = 1;
            }
            Layout::Extra(fields) => {
                let start = self.view.data[inst] as usize;
                for (k, &field) in fields.iter().enumerate() {
                    let word = self.view.extra[start + k];
                    match field {
                        Field::IntCond | Field::FloatCond => cond = word,
                        _ => {
                            args[n] = self.get(word, 0);
                            n += 1;
                        }
                    }
                }
            }
            Layout::None | Layout::Data(_) => return None,
        }
        Some((cond, args, n))
    }

    /// One scalar operation `op` of result type `ty` over `args` (the first of type `src`):
    /// through [`quad`] when it touches `f128`, [`half`] when it touches `f16`, else as is.
    pub(super) fn lower_scalar(
        &mut self,
        inst: usize,
        op: Op,
        flags: u8,
        cond: u32,
        ty: Ty,
        src: Ty,
        args: &[u32],
    ) -> Result<u32, LegalizeError> {
        let touches = |s: Scalar| ty.elem() == s || src.elem() == s;
        if touches(Scalar::F128) {
            if let Some(value) = self.quad(inst, op, cond, ty, src, args)? {
                return Ok(value);
            }
        } else if touches(Scalar::F16) {
            if let Some(value) = self.half(inst, op, flags, cond, ty, src, args)? {
                return Ok(value);
            }
        }
        Ok(self.emit(op, flags, ty, cond, args))
    }

    /// Copies a scalar instruction with its operands renamed; a vector argument of a block
    /// edge becomes its lanes, any other vector operand is refused.
    fn copy(&mut self, inst: usize, op: Op, ty: Ty) -> Result<(), LegalizeError> {
        let view = self.view;
        let flags = view.flags[inst];
        let attrs = view.attrs[inst];
        let value = match op.info().layout {
            Layout::None => self.raw(op, flags, ty, 0, attrs),
            Layout::Data(Field::Value) => {
                let operand = view.data[inst];
                if self.ty_of(operand).is_vector() {
                    return Err(self.refuse(inst, "a vector operand of an operation legalize does not scalarize"));
                }
                let renamed = self.get(operand, 0);
                let at = self.func.ops.len() as u32;
                if renamed & PENDING != 0 {
                    self.holes.push(Hole { data: true, at });
                }
                self.raw(op, flags, ty, renamed, attrs)
            }
            Layout::Data(_) => self.raw(op, flags, ty, view.data[inst], attrs),
            Layout::Extra(fields) => {
                let start = self.extra_at();
                self.rewrite(inst, fields)?;
                self.raw(op, flags, ty, start, attrs)
            }
        };
        self.set(inst, value);
        Ok(())
    }

    /// Appends the operands `fields` of `inst`, renamed: blocks keep their numbers (the pass
    /// adds none), a block edge's vector argument becomes its lanes.
    fn rewrite(&mut self, inst: usize, fields: &[Field]) -> Result<(), LegalizeError> {
        let view = self.view;
        let words = view.extra;
        let mut at = view.data[inst] as usize;
        let mut scrutinee = None;
        for &field in fields {
            match field {
                Field::Value => {
                    let value = words[at];
                    if self.ty_of(value).is_vector() {
                        return Err(self.refuse(inst, "a vector operand of an operation legalize does not scalarize"));
                    }
                    scrutinee = Some(value);
                    let renamed = self.get(value, 0);
                    self.push_value(renamed);
                    at += 1;
                }
                Field::Imm
                | Field::Offset
                | Field::IntCond
                | Field::FloatCond
                | Field::Rmw
                | Field::Ref
                | Field::Sig
                | Field::Slot
                | Field::Intrinsic
                | Field::Asm
                | Field::Block => {
                    self.push_word(words[at]);
                    at += 1;
                }
                Field::Target => {
                    let n = words[at + 1] as usize;
                    self.target(words[at], &words[at + 2..at + 2 + n])?;
                    at += 2 + n;
                }
                Field::Values => {
                    let n = words[at] as usize;
                    self.push_word(n as u32);
                    for k in 0..n {
                        let value = words[at + 1 + k];
                        if self.ty_of(value).is_vector() {
                            return Err(self.refuse(inst, "a vector argument or result of a call, `return`, `asm!` or target intrinsic (its ABI)"));
                        }
                        let renamed = self.get(value, 0);
                        self.push_value(renamed);
                    }
                    at += 1 + n;
                }
                Field::Blocks | Field::Mask => {
                    let n = words[at] as usize;
                    for k in 0..=n {
                        self.push_word(words[at + k]);
                    }
                    at += 1 + n;
                }
                Field::Const => {
                    let n = Ty(view.tys[inst]).const_words() as usize;
                    for k in 0..n {
                        self.push_word(words[at + k]);
                    }
                    at += n;
                }
                Field::Cases => {
                    let width = scrutinee.map_or(0, |it| self.ty_of(it).const_words()) as usize;
                    let n = words[at] as usize;
                    self.push_word(n as u32);
                    at += 1;
                    for _ in 0..n {
                        for k in 0..width {
                            self.push_word(words[at + k]);
                        }
                        at += width;
                        let args = words[at + 1] as usize;
                        self.target(words[at], &words[at + 2..at + 2 + args])?;
                        at += 2 + args;
                    }
                }
            }
        }
        Ok(())
    }

    /// A block edge: the block, the count of its arguments, then each, a vector's lanes in
    /// place of it (its parameters were split the same way).
    fn target(&mut self, block: u32, args: &[u32]) -> Result<(), LegalizeError> {
        self.push_word(block);
        let count_at = self.func.extra.len();
        self.push_word(0);
        let mut count = 0u32;
        for &value in args {
            let ty = self.ty_of(value);
            let lanes = match ty.form() {
                Form::Vector => ty.lanes(),
                Form::Scalable => return Err(LegalizeError::Scalable),
                Form::Scalar | Form::Compound => 1,
            };
            for lane in 0..lanes {
                let renamed = self.get(value, lane);
                self.push_value(renamed);
            }
            count += lanes;
        }
        self.func.extra[count_at] = count;
        Ok(())
    }
}

/// Whether `op` is one [`Out::lower_scalar`] may rewrite for `f16` or `f128`: float
/// arithmetic, comparison and conversion.
fn float_kind(op: Op) -> bool {
    matches!(
        op,
        Op::Fadd
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
            | Op::Fcmp
            | Op::Fpext
            | Op::Fptrunc
            | Op::FptoUi
            | Op::FptoSi
            | Op::UitoFp
            | Op::SitoFp
    )
}
