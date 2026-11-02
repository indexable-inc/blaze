//! The generator: from one seed and a size, a function of Cranelift IR that is well typed,
//! free of UB and ends on every input, shaped like what the IR builder makes after ABI
//! lowering.
//!
//! Values live in a pool of frontend variables, a few per type, defined in the entry block
//! from the parameters and constants, so every block may use any of them and the frontend
//! makes the block parameters SSA needs. Each instruction takes its first operand from the
//! one before it half the time (a chain, so the accumulator is used) and the rest from a
//! random variable (a value defined far away, so slots are), and defines a random variable.
//! Control flow is structured, nested two deep: `brif` diamonds (the else edge sometimes going
//! straight to the join with arguments), `br_table`s, and loops whose counter is a header
//! parameter compared against a constant of 1 to 4, so every program ends. The join and
//! header parameters are explicit, and each edge passes them a permutation of the variables
//! they are read back into: swaps and cycles.
//!
//! Nothing traps: a divisor is `bor`'d with 1 and, signed, `select`ed away from `-1` when the
//! dividend is `MIN`; `uadd_overflow_trap` adds halves; `trapz`/`trapnz` test values that are
//! never zero or always zero; conversions to integers are `_sat`; memory is stack slots,
//! zeroed in the entry block and accessed in bounds, atomics aligned.
//!
//! NaN bits are the hardware's (Cranelift leaves them nondeterministic), so wherever a float's
//! bits become something other than a float (a bitcast, a bitwise op, a store, `vhigh_bits`,
//! the sign `fcopysign` takes) its NaNs are made canonical first, as `cranelift-fuzzgen` does
//! for the whole function. Float results are returned as they are: Cranelift's and the stencil
//! code's must still agree bit for bit.
//!
//! The size is how many instructions the generator draws (each structure counts one, its
//! contents their own). It decides only where generation stops, so a smaller size of the same
//! seed is close to a prefix of a larger one: that is what shrinking leans on.

use cranelift_codegen::{
    data_value::DataValue,
    ir::{
        self, BlockArg, InstBuilder, MemFlagsData, Opcode,
        condcodes::{FloatCC, IntCC},
        immediates::{Ieee32, Ieee64},
        types,
    },
    isa::{CallConv, TargetIsa},
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};

use super::table::{self, Candidate, INTS_WIDE, TYS};
use crate::{
    Os, Target,
    shape::{Isa, Op, Ty},
    tests::{
        host,
        rng::{self, Rng},
    },
};

/// Variables per type.
const VARS: usize = 3;

/// Nesting of structures: a loop in a diamond in a loop is as deep as it goes.
const DEPTH: u32 = 2;

/// One generated function.
pub(super) struct Program {
    pub(super) func: ir::Function,
    pub(super) sig: ir::Signature,
    /// How many times each op was drawn, by `Op as usize`.
    pub(super) drawn: Vec<u32>,
    pub(super) features: Features,
}

/// How many of each structure a function has.
#[derive(Clone, Copy, Default, Debug)]
pub(super) struct Features {
    pub(super) brifs: u32,
    /// `brif`s whose else edge goes to the join with arguments.
    pub(super) direct: u32,
    pub(super) tables: u32,
    pub(super) loops: u32,
    /// Edges passing explicit block arguments.
    pub(super) edges: u32,
}

/// The function of `seed` at `size`, for `target`, finished with `isa`'s frontend config.
pub(super) fn generate(seed: u64, size: usize, target: &Target, isa: &dyn TargetIsa) -> Program {
    let arch = isa_of(target);
    let candidates = table::candidates(arch);
    let mut rng = Rng::new(seed);
    let (sig, digest) = signature(&mut rng, target);
    let mut func = ir::Function::with_name_signature(ir::UserFuncName::user(0, 0), sig.clone());
    let mut cx = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut func, &mut cx);
    let mut vars = Vec::with_capacity(TYS.len());
    for ty in TYS {
        let mut of = Vec::with_capacity(VARS);
        for _ in 0..VARS {
            of.push(b.declare_var(ir_ty(ty)));
        }
        vars.push(of);
    }
    let mut g = Gen {
        b,
        rng,
        isa: arch,
        ops: &candidates,
        vars,
        slots: Vec::new(),
        budget: size,
        last: None,
        digest,
        drawn: vec![0; usize::from(Op::COUNT)],
        features: Features::default(),
    };
    g.entry();
    g.region(0, size);
    g.ret();
    let Gen { mut b, drawn, features, .. } = g;
    b.seal_all_blocks();
    b.finalize(isa.frontend_config());
    Program { func, sig, drawn, features }
}

/// Input `k` of the function of `seed`: a value per parameter, biased to its type's edges.
pub(super) fn inputs(seed: u64, k: u64, sig: &ir::Signature) -> Vec<DataValue> {
    let mut rng = Rng::new(seed ^ (k + 1).wrapping_mul(0xa076_1d64_78bd_642f));
    sig.params
        .iter()
        .map(|param| {
            let bytes = rng.value(ty_of(param.value_type));
            DataValue::read_from_slice_le(&bytes, param.value_type)
        })
        .collect()
}

pub(super) fn isa_of(target: &Target) -> Isa {
    match target.arch {
        blaze_codegen::Arch::X86_64 => Isa::X86_64,
        blaze_codegen::Arch::Aarch64 => Isa::Aarch64,
        blaze_codegen::Arch::Wasm32 => panic!("wasm32 has no stencils to compare"),
    }
}

pub(super) fn ir_ty(ty: Ty) -> ir::Type {
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

fn ty_of(ty: ir::Type) -> Ty {
    TYS.into_iter().find(|it| ir_ty(*it) == ty).unwrap_or_else(|| panic!("{ty} is not a type the generator makes"))
}

/// The signature: one to six parameters, and results that fit the convention's return
/// registers (Cranelift refuses more without an explicit return area): on x64 two integer and
/// two float registers (an `i128` takes both integer ones), one of each on Windows, whose
/// signatures are scalars of 64 bits or less. With `true`, result 0 is the digest
/// ([`Gen::digest`]).
fn signature(rng: &mut Rng, target: &Target) -> (ir::Signature, bool) {
    const SCALARS: [Ty; 6] = [Ty::I8, Ty::I16, Ty::I32, Ty::I64, Ty::F32, Ty::F64];
    let conv = host::call_conv(target);
    let windows = target.os == Os::Windows;
    let tys: &[Ty] = if windows { &SCALARS } else { &TYS };
    let mut sig = ir::Signature::new(conv);
    for _ in 0..1 + rng.below(6) {
        let ty = tys[rng.below(tys.len() as u64) as usize];
        sig.params.push(ir::AbiParam::new(ir_ty(ty)));
    }
    let (mut ints, mut floats) = match (target.arch, conv) {
        (blaze_codegen::Arch::Aarch64, _) => (4u32, 4u32),
        (_, CallConv::WindowsFastcall) => (1, 1),
        _ => (2, 2),
    };
    let digest = rng.below(4) != 0;
    if digest {
        sig.returns.push(ir::AbiParam::new(types::I64));
        ints -= 1;
    }
    for _ in 0..1 + rng.below(3) {
        let ty = tys[rng.below(tys.len() as u64) as usize];
        let (budget, cost) = match ty {
            Ty::I128 => (&mut ints, 2),
            _ if ty.is_int() => (&mut ints, 1),
            _ => (&mut floats, 1),
        };
        if *budget >= cost {
            *budget -= cost;
            sig.returns.push(ir::AbiParam::new(ir_ty(ty)));
        }
    }
    if sig.returns.is_empty() {
        sig.returns.push(ir::AbiParam::new(types::I64));
    }
    (sig, digest)
}

struct Gen<'f, 'c> {
    b: FunctionBuilder<'f>,
    rng: Rng,
    isa: Isa,
    ops: &'c [Candidate],
    /// By [`table::index`].
    vars: Vec<Vec<Variable>>,
    /// Each stack slot and its size.
    slots: Vec<(ir::StackSlot, u32)>,
    /// Instructions left to draw.
    budget: usize,
    /// The result of the instruction before, in this block.
    last: Option<ir::Value>,
    digest: bool,
    drawn: Vec<u32>,
    features: Features,
}

impl Gen<'_, '_> {
    fn below(&mut self, n: usize) -> usize {
        self.rng.below(n as u64) as usize
    }

    fn chance(&mut self, one_in: u64) -> bool {
        self.rng.below(one_in) == 0
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len())]
    }

    fn var(&mut self, ty: Ty) -> Variable {
        let k = self.below(VARS);
        self.vars[table::index(ty)][k]
    }

    /// An operand of type `ty`: the instruction before's result half the time it has the
    /// type, a variable otherwise.
    fn arg(&mut self, ty: Ty) -> ir::Value {
        if let Some(last) = self.last
            && self.b.func.dfg.value_type(last) == ir_ty(ty)
            && self.chance(2)
        {
            return last;
        }
        let var = self.var(ty);
        self.b.use_var(var)
    }

    /// [`Gen::arg`], or for an integer of 64 bits or less a third of the time a constant
    /// (which the stencil backend folds into an `Imm` operand where the shape takes one).
    fn arg_imm(&mut self, ty: Ty) -> ir::Value {
        if ty.is_narrow_int() && self.chance(3) { self.imm(ty) } else { self.arg(ty) }
    }

    /// Defines a random variable of `ty` as `value`, which the next instruction may chain on.
    fn def(&mut self, ty: Ty, value: ir::Value) {
        let var = self.var(ty);
        self.b.def_var(var, value);
        self.last = Some(value);
    }

    fn switch_to(&mut self, block: ir::Block) {
        self.b.switch_to_block(block);
        self.last = None;
    }

    fn entry(&mut self) {
        let entry = self.b.create_block();
        self.b.append_block_params_for_function_params(entry);
        self.switch_to(entry);
        for _ in 0..1 + self.below(3) {
            let size = self.pick(&[16u32, 32, 64]);
            let slot = self
                .b
                .create_sized_stack_slot(ir::StackSlotData::new(ir::StackSlotKind::ExplicitSlot, size, 4));
            let addr = self.b.ins().stack_addr(types::I64, slot, 0);
            let zero = self.b.ins().iconst(types::I64, 0);
            for at in (0..size).step_by(8) {
                self.b.ins().store(MemFlagsData::trusted(), zero, addr, at as i32);
            }
            self.slots.push((slot, size));
        }
        let params = self.b.block_params(entry).to_vec();
        let mut next = [0usize; TYS.len()];
        for param in params {
            let at = table::index(ty_of(self.b.func.dfg.value_type(param)));
            if next[at] < VARS {
                self.b.def_var(self.vars[at][next[at]], param);
                next[at] += 1;
            }
        }
        for (at, ty) in TYS.into_iter().enumerate() {
            for k in next[at]..VARS {
                let value = self.konst(ty);
                self.b.def_var(self.vars[at][k], value);
            }
        }
    }

    /// Draws up to `n` instructions, or what is left of the budget.
    fn region(&mut self, depth: u32, n: usize) {
        let stop = self.budget.saturating_sub(n);
        while self.budget > stop {
            let room = self.budget - stop;
            let structures = depth < DEPTH && room > 4;
            match if structures { self.below(10) } else { 0 } {
                7 => self.diamond(depth, room),
                8 => self.table(depth, room),
                9 => self.looped(depth, room),
                _ => {
                    for _ in 0..(1 + self.below(8)).min(room) {
                        self.op();
                    }
                }
            }
        }
    }

    /// The share of `room` a structure's region gets, at least 1.
    fn share(&mut self, room: usize) -> usize {
        1 + self.below(room.min(48))
    }

    /// Variables a join or loop header takes as explicit parameters: up to three.
    fn carried(&mut self) -> Vec<(Ty, Variable)> {
        let n = self.below(4);
        (0..n)
            .map(|_| {
                let ty = self.pick(&TYS);
                (ty, self.var(ty))
            })
            .collect()
    }

    fn params(&mut self, block: ir::Block, carried: &[(Ty, Variable)]) -> Vec<ir::Value> {
        carried.iter().map(|(ty, _)| self.b.append_block_param(block, ir_ty(*ty))).collect()
    }

    /// The arguments an edge passes `carried`'s parameters: for each, half the time the
    /// variable of another parameter of its type (a swap, a cycle or a duplicate), else any
    /// variable or the result before.
    fn edge_args(&mut self, carried: &[(Ty, Variable)]) -> Vec<BlockArg> {
        if !carried.is_empty() {
            self.features.edges += 1;
        }
        let mut args = Vec::with_capacity(carried.len() + 1);
        for (ty, _) in carried {
            let value = if self.chance(2) {
                let same: Vec<Variable> = carried.iter().filter(|(it, _)| it == ty).map(|(_, var)| *var).collect();
                let var = self.pick(&same);
                self.b.use_var(var)
            } else {
                self.arg(*ty)
            };
            args.push(BlockArg::Value(value));
        }
        args
    }

    /// Reads a join's or header's parameters back into their variables.
    fn joined(&mut self, carried: &[(Ty, Variable)], params: &[ir::Value]) {
        for ((_, var), param) in carried.iter().zip(params) {
            self.b.def_var(*var, *param);
        }
    }

    /// Fills `block` with a region of `n` and jumps to `join`.
    fn arm(&mut self, block: ir::Block, depth: u32, n: usize, join: ir::Block, carried: &[(Ty, Variable)]) {
        self.switch_to(block);
        self.region(depth + 1, n);
        let args = self.edge_args(carried);
        self.b.ins().jump(join, &args);
    }

    /// A branch condition: an integer of any width, or an `icmp` right before the branch,
    /// which the stencil backend fuses with it when the branch is its one use.
    fn cond(&mut self) -> ir::Value {
        if self.chance(3) {
            let ty = self.pick(&INTS_WIDE);
            return self.arg(ty);
        }
        let ty = self.pick(&INTS_WIDE);
        let x = self.arg(ty);
        let y = self.arg_imm(ty);
        let cc = self.pick(IntCC::all());
        let cond = self.b.ins().icmp(cc, x, y);
        if self.chance(4) {
            let var = self.var(Ty::I8);
            self.b.def_var(var, cond);
        }
        cond
    }

    fn diamond(&mut self, depth: u32, room: usize) {
        self.budget -= 1;
        self.features.brifs += 1;
        let then = self.b.create_block();
        let join = self.b.create_block();
        let carried = self.carried();
        let params = self.params(join, &carried);
        let direct = self.chance(3);
        let n = self.share(room.saturating_sub(1)) / 2 + 1;
        let cond = self.cond();
        if direct {
            self.features.direct += 1;
            let args = self.edge_args(&carried);
            self.b.ins().brif(cond, then, &[], join, &args);
            self.arm(then, depth, n, join, &carried);
        } else {
            let otherwise = self.b.create_block();
            self.b.ins().brif(cond, then, &[], otherwise, &[]);
            self.arm(then, depth, n, join, &carried);
            self.arm(otherwise, depth, n, join, &carried);
        }
        self.switch_to(join);
        self.joined(&carried, &params);
    }

    fn table(&mut self, depth: u32, room: usize) {
        self.budget -= 1;
        self.features.tables += 1;
        let join = self.b.create_block();
        let carried = self.carried();
        let params = self.params(join, &carried);
        let mut index = self.arg(Ty::I32);
        if self.chance(2) {
            let mask = self.b.ins().iconst(types::I32, self.rng.below(8) as i64);
            index = self.b.ins().band(index, mask);
        }
        let cases: Vec<ir::Block> = (0..1 + self.below(4)).map(|_| self.b.create_block()).collect();
        let default = self.b.create_block();
        let mut entries = cases.clone();
        for _ in 0..self.below(3) {
            let at = self.below(entries.len() + 1);
            let case = self.pick(&cases);
            entries.insert(at, case);
        }
        let calls: Vec<ir::BlockCall> = entries.iter().map(|case| self.b.func.dfg.block_call(*case, &[])).collect();
        let default_call = self.b.func.dfg.block_call(default, &[]);
        let jt = self.b.create_jump_table(ir::JumpTableData::new(default_call, &calls));
        self.b.ins().br_table(index, jt);
        let n = self.share(room.saturating_sub(1)) / (cases.len() + 1) + 1;
        for case in cases.into_iter().chain([default]) {
            self.arm(case, depth, n, join, &carried);
        }
        self.switch_to(join);
        self.joined(&carried, &params);
    }

    /// A loop of 1 to 4 trips: the header takes the counter and the carried variables, and
    /// the back edge is `brif (icmp ult (counter + 1), trips)`.
    fn looped(&mut self, depth: u32, room: usize) {
        self.budget -= 1;
        self.features.loops += 1;
        let header = self.b.create_block();
        let exit = self.b.create_block();
        let counted = if self.chance(2) { Ty::I32 } else { Ty::I64 };
        let counter = self.b.append_block_param(header, ir_ty(counted));
        let carried = self.carried();
        let params = self.params(header, &carried);
        let zero = self.b.ins().iconst(ir_ty(counted), 0);
        let mut args = vec![BlockArg::Value(zero)];
        args.extend(self.edge_args(&carried));
        self.b.ins().jump(header, &args);
        self.switch_to(header);
        self.joined(&carried, &params);
        let n = self.share(room.saturating_sub(1));
        self.region(depth + 1, n);
        let one = self.b.ins().iconst(ir_ty(counted), 1);
        let next = self.b.ins().iadd(counter, one);
        let trips = self.b.ins().iconst(ir_ty(counted), 1 + self.rng.below(4) as i64);
        let more = self.b.ins().icmp(IntCC::UnsignedLessThan, next, trips);
        let mut back = vec![BlockArg::Value(next)];
        back.extend(self.edge_args(&carried));
        self.b.ins().brif(more, header, &back, exit, &[]);
        self.switch_to(exit);
    }

    fn ret(&mut self) {
        let tys: Vec<ir::Type> = self.b.func.signature.returns.iter().map(|ret| ret.value_type).collect();
        let mut values = Vec::with_capacity(tys.len());
        for (k, ty) in tys.into_iter().enumerate() {
            let value = if k == 0 && self.digest { self.digest() } else { self.arg(ty_of(ty)) };
            values.push(value);
        }
        self.b.ins().return_(&values);
    }

    /// An `i64` mixing the bits of twelve random variables (NaNs canonical), so a wrong value
    /// no result returns still shows.
    fn digest(&mut self) -> ir::Value {
        let mut digest = self.b.ins().iconst(types::I64, 0x5bd1_e995);
        for _ in 0..12 {
            let ty = self.pick(&TYS);
            let var = self.var(ty);
            let value = self.b.use_var(var);
            let bits = self.bits64(ty, value);
            let by = self.b.ins().iconst(types::I8, 7);
            let turned = self.b.ins().rotl(digest, by);
            digest = self.b.ins().bxor(turned, bits);
        }
        digest
    }

    /// `value`'s bits folded to an `i64`, NaNs canonical.
    fn bits64(&mut self, ty: Ty, value: ir::Value) -> ir::Value {
        match ty {
            Ty::I8 | Ty::I16 | Ty::I32 => self.b.ins().uextend(types::I64, value),
            Ty::I64 => value,
            Ty::I128 => {
                let (lo, hi) = self.b.ins().isplit(value);
                self.b.ins().bxor(lo, hi)
            }
            Ty::F32 => {
                let value = self.canon(ty, value);
                let bits = self.b.ins().bitcast(types::I32, MemFlagsData::new(), value);
                self.b.ins().uextend(types::I64, bits)
            }
            Ty::F64 => {
                let value = self.canon(ty, value);
                self.b.ins().bitcast(types::I64, MemFlagsData::new(), value)
            }
            _ => {
                let ints = if matches!(ty, Ty::F32x4 | Ty::F64x2) { self.canon_bits(ty, value) } else { value };
                let wide = if ty.lanes() == 2 {
                    ints
                } else {
                    self.b.ins().bitcast(types::I64X2, little(), ints)
                };
                let lo = self.b.ins().extractlane(wide, 0);
                let hi = self.b.ins().extractlane(wide, 1);
                self.b.ins().bxor(lo, hi)
            }
        }
    }

    /// `value` of `ty` with every NaN (lane) made the canonical quiet NaN; anything not a
    /// float as it is.
    fn canon(&mut self, ty: Ty, value: ir::Value) -> ir::Value {
        match ty {
            Ty::F32 | Ty::F64 => {
                let nan = self.b.ins().fcmp(FloatCC::Unordered, value, value);
                let canonical = if ty == Ty::F32 {
                    self.b.ins().f32const(Ieee32::with_bits(0x7fc0_0000))
                } else {
                    self.b.ins().f64const(Ieee64::with_bits(0x7ff8_0000_0000_0000))
                };
                self.b.ins().select(nan, canonical, value)
            }
            Ty::F32x4 | Ty::F64x2 => {
                let bits = self.canon_bits(ty, value);
                self.b.ins().bitcast(ir_ty(ty), MemFlagsData::new(), bits)
            }
            _ => value,
        }
    }

    /// A float vector's bits as the integer vector of its lanes, NaN lanes canonical.
    fn canon_bits(&mut self, ty: Ty, value: ir::Value) -> ir::Value {
        let ints = ty.int_lanes();
        let nan = self.b.ins().fcmp(FloatCC::Unordered, value, value);
        let bits = self.b.ins().bitcast(ir_ty(ints), MemFlagsData::new(), value);
        let canonical: i64 = if ty == Ty::F32x4 { 0x7fc0_0000 } else { 0x7ff8_0000_0000_0000 };
        let lane = self.b.ins().iconst(ir_ty(ints.lane()), canonical);
        let canonical = self.b.ins().splat(ir_ty(ints), lane);
        self.b.ins().bitselect(nan, canonical, bits)
    }

    /// [`Gen::arg`] with its NaNs canonical, for an operand whose bits become other than a
    /// float's.
    fn arg_bits(&mut self, ty: Ty) -> ir::Value {
        let value = self.arg(ty);
        self.canon(ty, value)
    }

    /// An integer of `ty`'s width: an edge a third of the time, a small one (-16 to 16) a
    /// third, random bits the rest.
    fn int_value(&mut self, ty: Ty) -> u128 {
        let raw = match self.below(3) {
            0 => {
                let edges = rng::edges(ty);
                let edge = &edges[self.below(edges.len())];
                let mut wide = [0u8; 16];
                wide[..edge.len()].copy_from_slice(edge);
                u128::from_le_bytes(wide)
            }
            1 => (self.rng.below(33) as i128 - 16) as u128,
            _ => u128::from_le_bytes(self.rng.value(ty)),
        };
        raw & mask(ty)
    }

    /// An `iconst` of `bits` (an `iconcat` of two for `i128`).
    fn iconst(&mut self, ty: Ty, bits: u128) -> ir::Value {
        let bits = bits & mask(ty);
        if ty == Ty::I128 {
            let lo = self.b.ins().iconst(types::I64, bits as u64 as i64);
            let hi = self.b.ins().iconst(types::I64, (bits >> 64) as u64 as i64);
            self.b.ins().iconcat(lo, hi)
        } else {
            self.b.ins().iconst(ir_ty(ty), bits as u64 as i64)
        }
    }

    fn imm(&mut self, ty: Ty) -> ir::Value {
        let bits = self.int_value(ty);
        self.iconst(ty, bits)
    }

    /// A constant of any type the generator makes.
    fn konst(&mut self, ty: Ty) -> ir::Value {
        match ty {
            Ty::F32 => {
                let bits = u32::from_le_bytes(self.rng.value(ty)[..4].try_into().expect("4 bytes"));
                self.b.ins().f32const(Ieee32::with_bits(bits))
            }
            Ty::F64 => {
                let bits = u64::from_le_bytes(self.rng.value(ty)[..8].try_into().expect("8 bytes"));
                self.b.ins().f64const(Ieee64::with_bits(bits))
            }
            _ if ty.is_vector() => {
                let bytes = self.rng.value(ty);
                let constant = self.b.func.dfg.constants.insert(ir::ConstantData::from(&bytes[..]));
                self.b.ins().vconst(ir_ty(ty), constant)
            }
            _ => self.imm(ty),
        }
    }

    /// A place in a stack slot for an access of `bytes`: its address (a `stack_addr`, never a
    /// variable, so no address reaches a result), the access's own offset, and its flags.
    /// Half the accesses are aligned (all atomics, which take no offset); every one is in
    /// bounds.
    fn place(&mut self, bytes: u32, atomic: bool) -> (ir::Value, i32, MemFlagsData) {
        let k = self.below(self.slots.len());
        let (slot, size) = self.slots[k];
        let aligned = atomic || self.chance(2);
        let room = size - bytes;
        let at = if aligned {
            self.rng.below(u64::from(room / bytes) + 1) as u32 * bytes
        } else {
            self.rng.below(u64::from(room) + 1) as u32
        };
        let base = if atomic { at } else { self.rng.below(u64::from(at) + 1) as u32 };
        let addr = self.b.ins().stack_addr(types::I64, slot, base as i32);
        let flags = if aligned { MemFlagsData::trusted() } else { MemFlagsData::new().with_notrap() };
        (addr, (at - base) as i32, flags)
    }

    /// Draws one op of the candidates and writes it.
    fn op(&mut self) {
        self.budget -= 1;
        let ops = self.ops;
        let candidate = &ops[self.below(ops.len())];
        let ty = self.pick(&candidate.tys);
        self.drawn[candidate.op as usize] += 1;
        self.emit(candidate.op, ty);
    }

    fn unary(&mut self, opcode: Opcode, ty: Ty, x: ir::Value) {
        let (inst, dfg) = self.b.ins().Unary(opcode, ir_ty(ty), x);
        let value = dfg.first_result(inst);
        self.def(ty, value);
    }

    fn binary(&mut self, opcode: Opcode, ty: Ty, x: ir::Value, y: ir::Value) {
        let (inst, dfg) = self.b.ins().Binary(opcode, ir_ty(ty), x, y);
        let value = dfg.first_result(inst);
        self.def(ty, value);
    }

    /// A binary op of two operands of `ty`, the second maybe a constant.
    fn bin(&mut self, opcode: Opcode, ty: Ty, imm: bool) {
        let x = self.arg(ty);
        let y = if imm { self.arg_imm(ty) } else { self.arg(ty) };
        self.binary(opcode, ty, x, y);
    }

    /// A bitwise op, which reads a float's bits.
    fn bits(&mut self, opcode: Opcode, ty: Ty) {
        let x = self.arg_bits(ty);
        let y = if ty.is_narrow_int() { self.arg_imm(ty) } else { self.arg_bits(ty) };
        self.binary(opcode, ty, x, y);
    }

    fn emit(&mut self, op: Op, ty: Ty) {
        let t = ir_ty(ty);
        match op {
            Op::Iconst => {
                let value = self.imm(ty);
                self.def(ty, value);
            }
            Op::Fconst | Op::Vconst => {
                let value = self.konst(ty);
                self.def(ty, value);
            }
            Op::Iadd => self.bin(Opcode::Iadd, ty, true),
            Op::Isub => self.bin(Opcode::Isub, ty, true),
            Op::Imul => self.bin(Opcode::Imul, ty, true),
            Op::Umulhi => self.bin(Opcode::Umulhi, ty, true),
            Op::Smulhi => self.bin(Opcode::Smulhi, ty, true),
            Op::Smin => self.bin(Opcode::Smin, ty, true),
            Op::Umin => self.bin(Opcode::Umin, ty, true),
            Op::Smax => self.bin(Opcode::Smax, ty, true),
            Op::Umax => self.bin(Opcode::Umax, ty, true),
            Op::SqmulRoundSat => self.bin(Opcode::SqmulRoundSat, ty, false),
            Op::AvgRound => self.bin(Opcode::AvgRound, ty, false),
            Op::UaddSat => self.bin(Opcode::UaddSat, ty, false),
            Op::SaddSat => self.bin(Opcode::SaddSat, ty, false),
            Op::UsubSat => self.bin(Opcode::UsubSat, ty, false),
            Op::SsubSat => self.bin(Opcode::SsubSat, ty, false),
            Op::IaddPairwise => self.bin(Opcode::IaddPairwise, ty, false),
            Op::Fadd => self.bin(Opcode::Fadd, ty, false),
            Op::Fsub => self.bin(Opcode::Fsub, ty, false),
            Op::Fmul => self.bin(Opcode::Fmul, ty, false),
            Op::Fdiv => self.bin(Opcode::Fdiv, ty, false),
            Op::Fmin => self.bin(Opcode::Fmin, ty, false),
            Op::Fmax => self.bin(Opcode::Fmax, ty, false),
            Op::Fcopysign => {
                let x = self.arg(ty);
                let sign = self.arg_bits(ty);
                self.binary(Opcode::Fcopysign, ty, x, sign);
            }
            Op::Band => self.bits(Opcode::Band, ty),
            Op::Bor => self.bits(Opcode::Bor, ty),
            Op::Bxor => self.bits(Opcode::Bxor, ty),
            Op::Bnot => {
                let x = self.arg_bits(ty);
                self.unary(Opcode::Bnot, ty, x);
            }
            Op::Rotl | Op::Rotr | Op::Ishl | Op::Ushr | Op::Sshr => {
                let opcode = match op {
                    Op::Rotl => Opcode::Rotl,
                    Op::Rotr => Opcode::Rotr,
                    Op::Ishl => Opcode::Ishl,
                    Op::Ushr => Opcode::Ushr,
                    _ => Opcode::Sshr,
                };
                let x = self.arg(ty);
                let by = self.pick(table::amounts(ty));
                let amount = self.arg_imm(by);
                self.binary(opcode, ty, x, amount);
            }
            Op::Ineg | Op::Iabs | Op::Bitrev | Op::Clz | Op::Cls | Op::Ctz | Op::Bswap | Op::Popcnt => {
                let opcode = match op {
                    Op::Ineg => Opcode::Ineg,
                    Op::Iabs => Opcode::Iabs,
                    Op::Bitrev => Opcode::Bitrev,
                    Op::Clz => Opcode::Clz,
                    Op::Cls => Opcode::Cls,
                    Op::Ctz => Opcode::Ctz,
                    Op::Bswap => Opcode::Bswap,
                    _ => Opcode::Popcnt,
                };
                let x = self.arg(ty);
                self.unary(opcode, ty, x);
            }
            Op::Sqrt | Op::Fneg | Op::Fabs | Op::Ceil | Op::Floor | Op::Trunc | Op::Nearest => {
                let opcode = match op {
                    Op::Sqrt => Opcode::Sqrt,
                    Op::Fneg => Opcode::Fneg,
                    Op::Fabs => Opcode::Fabs,
                    Op::Ceil => Opcode::Ceil,
                    Op::Floor => Opcode::Floor,
                    Op::Trunc => Opcode::Trunc,
                    _ => Opcode::Nearest,
                };
                let x = self.arg(ty);
                self.unary(opcode, ty, x);
            }
            Op::Udiv | Op::Urem | Op::Sdiv | Op::Srem => self.divide(op, ty),
            Op::UaddOverflow
            | Op::SaddOverflow
            | Op::UsubOverflow
            | Op::SsubOverflow
            | Op::UmulOverflow
            | Op::SmulOverflow => {
                let x = self.arg(ty);
                let y = self.arg_imm(ty);
                let ins = self.b.ins();
                let (value, flag) = match op {
                    Op::UaddOverflow => ins.uadd_overflow(x, y),
                    Op::SaddOverflow => ins.sadd_overflow(x, y),
                    Op::UsubOverflow => ins.usub_overflow(x, y),
                    Op::SsubOverflow => ins.ssub_overflow(x, y),
                    Op::UmulOverflow => ins.umul_overflow(x, y),
                    _ => ins.smul_overflow(x, y),
                };
                let var = self.var(Ty::I8);
                self.b.def_var(var, flag);
                self.def(ty, value);
            }
            Op::UaddOverflowTrap => {
                // Two halves never carry out.
                let x = self.arg(ty);
                let one = self.iconst(ty, 1);
                let x = self.b.ins().ushr(x, one);
                let y = if self.chance(2) {
                    let small = u128::from(self.rng.below(1 << 30));
                    self.iconst(ty, small)
                } else {
                    let y = self.arg(ty);
                    let one = self.iconst(ty, 1);
                    self.b.ins().ushr(y, one)
                };
                let value = self.b.ins().uadd_overflow_trap(x, y, ir::TrapCode::INTEGER_OVERFLOW);
                self.def(ty, value);
            }
            Op::Bitselect => {
                let c = self.arg_bits(ty);
                let x = self.arg_bits(ty);
                let y = self.arg_bits(ty);
                let value = self.b.ins().bitselect(c, x, y);
                self.def(ty, value);
            }
            Op::Bmask => {
                let from = self.source(op, ty);
                let x = self.arg(from);
                let value = self.b.ins().bmask(t, x);
                self.def(ty, value);
            }
            Op::Icmp => {
                let cc = self.pick(IntCC::all());
                let x = self.arg(ty);
                let y = self.arg_imm(ty);
                let value = self.b.ins().icmp(cc, x, y);
                self.def(if ty.is_vector() { ty } else { Ty::I8 }, value);
            }
            Op::Fcmp => {
                let ccs = table::float_ccs(self.isa, ty.is_vector());
                let cc = self.pick(&ccs);
                let x = self.arg(ty);
                let y = self.arg(ty);
                let value = self.b.ins().fcmp(cc, x, y);
                self.def(if ty.is_vector() { ty.int_lanes() } else { Ty::I8 }, value);
            }
            Op::Select => {
                let cond = self.source(op, ty);
                let c = self.arg(cond);
                let x = self.arg_imm(ty);
                let y = self.arg_imm(ty);
                let value = self.b.ins().select(c, x, y);
                self.def(ty, value);
            }
            Op::SelectIcmp => {
                // `select (icmp cc a b), a, b` or `..., b, a`: a min or max of any condition.
                let cc = self.pick(IntCC::all());
                let a = self.arg(ty);
                let b = self.arg_imm(ty);
                let c = self.b.ins().icmp(cc, a, b);
                let value = if self.chance(2) { self.b.ins().select(c, a, b) } else { self.b.ins().select(c, b, a) };
                self.def(ty, value);
            }
            Op::Fma => {
                let x = self.arg(ty);
                let y = self.arg(ty);
                let z = self.arg(ty);
                let value = self.b.ins().fma(x, y, z);
                self.def(ty, value);
            }
            Op::Bitcast => {
                let from = self.source(op, ty);
                let x = self.arg_bits(from);
                let flags = if from.lanes() == ty.lanes() { MemFlagsData::new() } else { little() };
                let value = self.b.ins().bitcast(t, flags, x);
                self.def(ty, value);
            }
            Op::Ireduce | Op::Uextend | Op::Sextend => {
                let from = self.source(op, ty);
                let x = self.arg(from);
                let value = match op {
                    Op::Ireduce => self.b.ins().ireduce(t, x),
                    Op::Uextend => self.b.ins().uextend(t, x),
                    _ => self.b.ins().sextend(t, x),
                };
                self.def(ty, value);
            }
            Op::Fpromote => {
                let x = self.arg(Ty::F32);
                let value = self.b.ins().fpromote(t, x);
                self.def(ty, value);
            }
            Op::Fdemote => {
                let x = self.arg(Ty::F64);
                let value = self.b.ins().fdemote(t, x);
                self.def(ty, value);
            }
            Op::Fvdemote => {
                let x = self.arg(Ty::F64x2);
                let value = self.b.ins().fvdemote(x);
                self.def(ty, value);
            }
            Op::FvpromoteLow => {
                let x = self.arg(Ty::F32x4);
                let value = self.b.ins().fvpromote_low(x);
                self.def(ty, value);
            }
            Op::FcvtToUintSat | Op::FcvtToSintSat | Op::FcvtFromUint | Op::FcvtFromSint => {
                let from = self.source(op, ty);
                let x = self.arg(from);
                let value = match op {
                    Op::FcvtToUintSat => self.b.ins().fcvt_to_uint_sat(t, x),
                    Op::FcvtToSintSat => self.b.ins().fcvt_to_sint_sat(t, x),
                    Op::FcvtFromUint => self.b.ins().fcvt_from_uint(t, x),
                    _ => self.b.ins().fcvt_from_sint(t, x),
                };
                self.def(ty, value);
            }
            Op::Isplit => {
                let x = self.arg(Ty::I128);
                let (lo, hi) = self.b.ins().isplit(x);
                let var = self.var(Ty::I64);
                self.b.def_var(var, hi);
                self.def(Ty::I64, lo);
            }
            Op::Iconcat => {
                let lo = self.arg(Ty::I64);
                let hi = self.arg(Ty::I64);
                let value = self.b.ins().iconcat(lo, hi);
                self.def(ty, value);
            }
            Op::Snarrow | Op::Unarrow | Op::Uunarrow => {
                let wide = ty.wider().expect("a narrowed vector has a wider one");
                let x = self.arg(wide);
                let y = self.arg(wide);
                let value = match op {
                    Op::Snarrow => self.b.ins().snarrow(x, y),
                    Op::Unarrow => self.b.ins().unarrow(x, y),
                    _ => self.b.ins().uunarrow(x, y),
                };
                self.def(ty, value);
            }
            Op::SwidenLow | Op::SwidenHigh | Op::UwidenLow | Op::UwidenHigh => {
                let narrow = ty.narrower().expect("a widened vector has a narrower one");
                let x = self.arg(narrow);
                let value = match op {
                    Op::SwidenLow => self.b.ins().swiden_low(x),
                    Op::SwidenHigh => self.b.ins().swiden_high(x),
                    Op::UwidenLow => self.b.ins().uwiden_low(x),
                    _ => self.b.ins().uwiden_high(x),
                };
                self.def(ty, value);
            }
            Op::ScalarToVector => {
                let x = self.arg_imm(ty.lane());
                let value = self.b.ins().scalar_to_vector(t, x);
                self.def(ty, value);
            }
            Op::Splat => {
                let x = self.arg_imm(ty.lane());
                let value = self.b.ins().splat(t, x);
                self.def(ty, value);
            }
            Op::Swizzle => {
                let x = self.arg(ty);
                let y = self.arg(ty);
                let value = self.b.ins().swizzle(x, y);
                self.def(ty, value);
            }
            Op::Shuffle => {
                let x = self.arg(ty);
                let y = self.arg(ty);
                let mut lanes = [0u8; 16];
                for lane in &mut lanes {
                    *lane = self.rng.below(32) as u8;
                }
                let mask = self.b.func.dfg.immediates.push(ir::ConstantData::from(&lanes[..]));
                let value = self.b.ins().shuffle(x, y, mask);
                self.def(ty, value);
            }
            Op::Insertlane => {
                let v = self.arg(ty);
                let x = self.arg(ty.lane());
                let lane = self.rng.below(u64::from(ty.lanes())) as u8;
                let value = self.b.ins().insertlane(v, x, lane);
                self.def(ty, value);
            }
            Op::Extractlane => {
                let v = self.arg(ty);
                let lane = self.rng.below(u64::from(ty.lanes())) as u8;
                let value = self.b.ins().extractlane(v, lane);
                self.def(ty.lane(), value);
            }
            Op::VanyTrue | Op::VallTrue => {
                let v = self.arg(ty);
                let value = if op == Op::VanyTrue { self.b.ins().vany_true(v) } else { self.b.ins().vall_true(v) };
                self.def(Ty::I8, value);
            }
            Op::VhighBits => {
                let to = self.source(op, ty);
                let v = self.arg_bits(ty);
                let value = self.b.ins().vhigh_bits(ir_ty(to), v);
                self.def(to, value);
            }
            Op::Load => {
                let (addr, offset, flags) = self.place(u32::from(ty.bytes()), false);
                let value = self.b.ins().load(t, flags, addr, offset);
                self.def(ty, value);
            }
            Op::Store => {
                let x = self.arg_bits(ty);
                let (addr, offset, flags) = self.place(u32::from(ty.bytes()), false);
                self.b.ins().store(flags, x, addr, offset);
            }
            Op::Uload8 | Op::Sload8 | Op::Uload16 | Op::Sload16 | Op::Uload32 | Op::Sload32 => {
                let bytes = match op {
                    Op::Uload8 | Op::Sload8 => 1,
                    Op::Uload16 | Op::Sload16 => 2,
                    _ => 4,
                };
                let (addr, offset, flags) = self.place(bytes, false);
                let ins = self.b.ins();
                let value = match op {
                    Op::Uload8 => ins.uload8(t, flags, addr, offset),
                    Op::Sload8 => ins.sload8(t, flags, addr, offset),
                    Op::Uload16 => ins.uload16(t, flags, addr, offset),
                    Op::Sload16 => ins.sload16(t, flags, addr, offset),
                    Op::Uload32 => ins.uload32(flags, addr, offset),
                    _ => ins.sload32(flags, addr, offset),
                };
                self.def(ty, value);
            }
            Op::Istore8 | Op::Istore16 | Op::Istore32 => {
                let x = self.arg(ty);
                let bytes = match op {
                    Op::Istore8 => 1,
                    Op::Istore16 => 2,
                    _ => 4,
                };
                let (addr, offset, flags) = self.place(bytes, false);
                let ins = self.b.ins();
                match op {
                    Op::Istore8 => ins.istore8(flags, x, addr, offset),
                    Op::Istore16 => ins.istore16(flags, x, addr, offset),
                    _ => ins.istore32(flags, x, addr, offset),
                };
            }
            Op::AtomicLoad => {
                let (addr, _, flags) = self.place(u32::from(ty.bytes()), true);
                let value = self.b.ins().atomic_load(t, flags, addr);
                self.def(ty, value);
            }
            Op::AtomicStore => {
                let x = self.arg(ty);
                let (addr, _, flags) = self.place(u32::from(ty.bytes()), true);
                self.b.ins().atomic_store(flags, x, addr);
            }
            Op::AtomicRmw => {
                let rmw = self.pick(ir::AtomicRmwOp::all());
                let x = self.arg_imm(ty);
                let (addr, _, flags) = self.place(u32::from(ty.bytes()), true);
                let value = self.b.ins().atomic_rmw(t, flags, rmw, addr, x);
                self.def(ty, value);
            }
            Op::AtomicCas => {
                let expected = self.arg(ty);
                let x = self.arg(ty);
                let (addr, _, flags) = self.place(u32::from(ty.bytes()), true);
                let value = self.b.ins().atomic_cas(flags, addr, expected, x);
                self.def(ty, value);
            }
            Op::Fence => {
                self.b.ins().fence();
            }
            Op::Trapz => {
                // `x | 1` is never zero.
                let x = self.arg(ty);
                let one = self.iconst(ty, 1);
                let c = self.b.ins().bor(x, one);
                self.b.ins().trapz(c, ir::TrapCode::INTEGER_OVERFLOW);
            }
            Op::Trapnz => {
                // `x & 0` always is.
                let x = self.arg(ty);
                let zero = self.iconst(ty, 0);
                let c = self.b.ins().band(x, zero);
                self.b.ins().trapnz(c, ir::TrapCode::INTEGER_OVERFLOW);
            }
            Op::Copy
            | Op::Jump
            | Op::Brif
            | Op::BrIcmp
            | Op::StackAddr
            | Op::UaddOverflowCin
            | Op::SaddOverflowCin
            | Op::UsubOverflowBin
            | Op::SsubOverflowBin
            | Op::FcvtToUint
            | Op::FcvtToSint
            | Op::Uload8x8
            | Op::Sload8x8
            | Op::Uload16x4
            | Op::Sload16x4
            | Op::Uload32x2
            | Op::Sload32x2
            | Op::Blendv
            | Op::X86Pshufb
            | Op::X86Pmulhrsw
            | Op::X86Pmaddubsw
            | Op::X86Cvtt2dq
            | Op::LoadConfined
            | Op::StoreConfined => panic!("the generator never draws `{}`", op.info().name),
        }
    }

    /// A division or remainder that can't trap: an immediate divisor that is neither 0 nor
    /// (signed) -1, or a variable `bor`'d with 1 and, signed, replaced by 1 when it is -1 and
    /// the dividend is `MIN`.
    fn divide(&mut self, op: Op, ty: Ty) {
        let signed = matches!(op, Op::Sdiv | Op::Srem);
        let opcode = match op {
            Op::Udiv => Opcode::Udiv,
            Op::Urem => Opcode::Urem,
            Op::Sdiv => Opcode::Sdiv,
            _ => Opcode::Srem,
        };
        let x = self.arg(ty);
        let all = mask(ty);
        let y = if self.chance(3) {
            let mut bits = self.int_value(ty);
            if bits == 0 || (signed && bits == all) {
                bits = 3;
            }
            self.iconst(ty, bits)
        } else {
            let y = self.arg(ty);
            let one = self.iconst(ty, 1);
            let y = self.b.ins().bor(y, one);
            if signed {
                let min = self.iconst(ty, (all >> 1) + 1);
                let minus_one = self.iconst(ty, all);
                let is_min = self.b.ins().icmp(IntCC::Equal, x, min);
                let is_minus_one = self.b.ins().icmp(IntCC::Equal, y, minus_one);
                let overflows = self.b.ins().band(is_min, is_minus_one);
                let one = self.iconst(ty, 1);
                self.b.ins().select(overflows, one, y)
            } else {
                y
            }
        };
        self.binary(opcode, ty, x, y);
    }

    /// The other type of `op` at `ty` ([`table::sources`]).
    fn source(&mut self, op: Op, ty: Ty) -> Ty {
        let sources = table::sources(self.isa, op, ty);
        self.pick(&sources)
    }
}

/// The bits of `ty`'s width.
fn mask(ty: Ty) -> u128 {
    match ty.lane().bytes() {
        16 => u128::MAX,
        bytes => (1u128 << (u32::from(bytes) * 8)) - 1,
    }
}

/// Flags of a bitcast between vectors of different lane counts, which must say the order.
fn little() -> MemFlagsData {
    MemFlagsData::new().with_endianness(ir::Endianness::Little)
}

#[test]
fn masks_are_of_the_type_width() {
    assert_eq!(mask(Ty::I8), 0xff);
    assert_eq!(mask(Ty::I64), u128::from(u64::MAX));
    assert_eq!(mask(Ty::I128), u128::MAX);
    for ty in TYS {
        assert_eq!(ty_of(ir_ty(ty)), ty);
    }
}
