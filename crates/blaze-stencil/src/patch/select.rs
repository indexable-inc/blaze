//! Selection: what writes each instruction, and for a stencil its *base shape*: every
//! operand that is an `iconst` at a position the op takes an immediate in folded as one
//! ([`Src::Imm`]), every other from its slot, the result to its slot. [`super::slots`] then
//! moves operands and results into the accumulators where the shapes allow it, always able
//! to fall back to the base shape, which exists.
//!
//! The conventions the generator's shapes follow (one definition: stencil.md, "Shapes", and
//! the patcher's report):
//!
//! - operands in IR order (`DataFlowGraph::inst_args`);
//! - `ty` is result 0's type, except for the ops whose meaning is by an operand's type
//!   ([`by_operand`]); ops with no type in the IR take the first type their [`Info`] has;
//! - `aux` for [`Aux::Ty`] is the condition's type for `select`, the result's for
//!   `vhigh_bits`, and operand 0's for conversions and `scalar_to_vector`;
//! - `BrIcmp` is `brif (icmp cc a b)` with operands `[a, b]`; `SelectIcmp` is
//!   `select (icmp cc a b), a, b` with operands `[a, b]`, `select (icmp cc a b), b, a`
//!   being that with the condition's arguments swapped. Either fusion is taken only when its
//!   `icmp` is the instruction right before and has no other use, and only when the fused
//!   shape exists; otherwise the two are written apart.
//! - `LoadConfined`/`StoreConfined` is a load or store at offset 0 of the macro tier's
//!   confined address, `iadd vmctx, uextend.i64 (ireduce.i32 (iadd p, iconst off))`, with
//!   operands `[p, vmctx]` (a store's value first) and `off` wrapped to 32 bits in the
//!   `Offset` hole. It is taken only when each value of the address has that one use and is a
//!   stencil's in the access's block, and only when the fused shape exists; otherwise each
//!   instruction is written apart.
//! - `fma x (fneg y) z` is the `fma` of `aux` 1 with operands `[x, y, z]`, always, as
//!   Cranelift's AArch64 lowering matches it (see [`negate`]).
//!
//! An instruction neither the glue nor a stencil writes is an error naming its opcode and
//! type, never a compile by Cranelift.

use cranelift_codegen::{
    ir::{
        self, InstructionData as Data, Opcode,
        condcodes::{CondCode, FloatCC, IntCC},
        types,
    },
    packed_option::ReservedValue,
};

use super::{
    Kind, Konst, KonstRef, Plan, Sel,
    scan::reset,
    slots::{GLOBAL, has},
};
use crate::{
    Error, Target,
    arch::Glue,
    shape::{Aux, Disp, Dst, FloatCc, Info, IntCc, Op, RmwOp, Shape, Src, Ty},
    table::format::Reader,
};

pub(super) fn run<A: Glue>(
    plan: &mut Plan<'_>,
    func: &ir::Function,
    target: &Target,
    library: &Reader<'_>,
) -> Result<(), Error> {
    reset(&mut plan.sels, func.dfg.num_insts(), Sel::default());
    for block in func.layout.blocks() {
        for inst in func.layout.block_insts(block) {
            if plan.dead[inst.as_u32() as usize] {
                continue;
            }
            plan.sels[inst.as_u32() as usize] = selected::<A>(plan, func, target, library, inst)?;
        }
    }

    // A constant needs a slot when a stencil reads it from one, or when it is of a type the
    // glue and the edges can't materialize.
    reset(&mut plan.kslot, func.dfg.num_values(), false);
    for sel in &plan.sels {
        if sel.kind != Kind::Stencil {
            continue;
        }
        for (n, &arg) in sel.args().iter().enumerate() {
            let at = arg.as_u32() as usize;
            if sel.shape.srcs[n] == Src::Slot && plan.konst[at] != Konst::None {
                plan.kslot[at] = true;
            }
        }
    }
    for (at, konst) in plan.konst.iter().enumerate() {
        if matches!(konst, Konst::F16(_) | Konst::Wide) && plan.uses[at] > 0 {
            plan.kslot[at] = true;
        }
    }
    for block in func.layout.blocks() {
        for inst in func.layout.block_insts(block) {
            let i = inst.as_u32() as usize;
            if plan.sels[i].kind != Kind::Stencil {
                continue;
            }
            let Some(&result) = func.dfg.inst_results(inst).first() else { continue };
            let at = result.as_u32() as usize;
            let is_const = matches!(plan.sels[i].shape.op, Op::Iconst | Op::Fconst | Op::Vconst);
            if is_const && plan.konst[at] != Konst::None && !plan.kslot[at] {
                plan.sels[i].kind = Kind::Skip;
            }
        }
    }
    Ok(())
}

/// Every instruction of `func` that selection refuses, each with its [`crate::Gap`], where
/// [`run`] stops at the first: the coverage survey's view of one function.
pub(super) fn survey<A: Glue>(
    plan: &mut Plan<'_>,
    func: &ir::Function,
    target: &Target,
    library: &Reader<'_>,
    out: &mut Vec<Error>,
) {
    reset(&mut plan.sels, func.dfg.num_insts(), Sel::default());
    for block in func.layout.blocks() {
        for inst in func.layout.block_insts(block) {
            if plan.dead[inst.as_u32() as usize] {
                continue;
            }
            match selected::<A>(plan, func, target, library, inst) {
                Ok(sel) => plan.sels[inst.as_u32() as usize] = sel,
                Err(error) => out.push(error),
            }
        }
    }
}

/// What writes `inst` with its fusions taken; a refusal carries the instruction's gap.
fn selected<A: Glue>(
    plan: &mut Plan<'_>,
    func: &ir::Function,
    target: &Target,
    library: &Reader<'_>,
    inst: ir::Inst,
) -> Result<Sel, Error> {
    let chosen = one::<A>(plan, func, target, library, inst).and_then(|mut sel| {
        if sel.kind == Kind::Stencil {
            fuse(plan, func, library, inst, &mut sel);
            confine(plan, func, library, inst, &mut sel);
            negate(plan, func, library, inst, &mut sel)?;
        }
        Ok(sel)
    });
    chosen.map_err(|mut error| {
        error.gap.get_or_insert(crate::Gap { opcode: func.dfg.insts[inst].opcode(), ty: inst_type(func, inst) });
        error
    })
}

/// What writes `inst`, and its base shape if a stencil does.
fn one<A: Glue>(
    plan: &Plan<'_>,
    func: &ir::Function,
    target: &Target,
    library: &Reader<'_>,
    inst: ir::Inst,
) -> Result<Sel, Error> {
    let data = func.dfg.insts[inst];
    let opcode = data.opcode();
    match opcode {
        Opcode::Nop | Opcode::SequencePoint => return Ok(Sel::default()),
        Opcode::Jump => return Ok(Sel { kind: Kind::Jump, ..Sel::default() }),
        Opcode::Brif => {}
        _ if A::takes(func, target, inst) => return Ok(Sel { kind: Kind::Glue, ..Sel::default() }),
        _ => {}
    }
    let Some(op) = op_of(opcode) else {
        return Err(Error::at(
            inst,
            // alloc: the error's message, once per failed compile.
            format!("`{opcode}` of {}: no stencil is of it and the glue does not write it", type_name(func, inst)),
        ));
    };
    let info = op.info();
    let mut sel = Sel { kind: Kind::Stencil, ..Sel::default() };

    let args = func.dfg.inst_args(inst);
    let results = func.dfg.inst_results(inst);
    if args.len() != usize::from(info.args) || results.len() != usize::from(info.results) {
        return Err(Error::at(
            inst,
            // alloc: the error's message, once per failed compile.
            format!(
                "`{opcode}` with {} operands and {} results, where the `{}` stencils take {} and give {}",
                args.len(),
                results.len(),
                info.name,
                info.args,
                info.results
            ),
        ));
    }
    for (n, &arg) in args.iter().enumerate() {
        sel.args[n] = func.dfg.resolve_aliases(arg);
    }
    sel.nargs = info.args;

    let ty = match ty_value(func, inst, op) {
        Some(ty) => ty_of(ty).ok_or_else(|| no_type(inst, opcode, ty))?,
        None => info.tys.iter().next().ok_or_else(|| {
            // alloc: the error's message, once per failed compile.
            Error::at(inst, format!("the `{}` stencils are of no type", info.name))
        })?,
    };

    let mut aux = 0u8;
    match data {
        Data::Load { flags, offset, .. } | Data::Store { flags, offset, .. } => {
            little_endian(func, inst, flags)?;
            sel.offset = i64::from(i32::from(offset));
        }
        Data::LoadNoOffset { flags, .. }
        | Data::StoreNoOffset { flags, .. }
        | Data::AtomicCas { flags, .. } => little_endian(func, inst, flags)?,
        Data::AtomicRmw { flags, op: rmw_op, .. } => {
            little_endian(func, inst, flags)?;
            aux = rmw(rmw_op) as u8;
        }
        Data::StackAddr { stack_slot, offset, .. } => {
            sel.stack = Some(stack_slot);
            sel.offset = i64::from(i32::from(offset));
        }
        Data::IntCompare { cond, .. } => aux = int_cc(cond) as u8,
        Data::FloatCompare { cond, .. } => aux = float_cc(cond) as u8,
        Data::BinaryImm8 { imm, .. } | Data::TernaryImm8 { imm, .. } => aux = imm.into(),
        Data::UnaryConst { constant_handle, .. } => sel.konst = KonstRef::Pool(constant_handle),
        Data::Shuffle { imm, .. } => sel.konst = KonstRef::Immediate(imm),
        _ => {}
    }
    if let Aux::Ty(_) = info.aux {
        let source = match op {
            Op::VhighBits => results.first(),
            _ => args.first(),
        };
        let source = source.ok_or_else(|| {
            // alloc: the error's message, once per failed compile.
            Error::at(inst, format!("`{opcode}` has no value its stencil's `aux` type is of"))
        })?;
        let source_ty = func.dfg.value_type(*source);
        aux = ty_of(source_ty).ok_or_else(|| no_type(inst, opcode, source_ty))? as u8;
    }

    sel.trap = match op {
        Op::Udiv | Op::Sdiv | Op::Urem | Op::Srem => Some(ir::TrapCode::INTEGER_DIVISION_BY_ZERO),
        Op::FcvtToUint | Op::FcvtToSint => Some(ir::TrapCode::BAD_CONVERSION_TO_INTEGER),
        _ => data.trap_code(),
    };
    if info.traps && sel.trap.is_none() {
        // alloc: the error's message, once per failed compile.
        return Err(Error::at(inst, format!("the `{}` stencils trap, and `{opcode}` has no trap code", info.name)));
    }

    let dst = if results.is_empty() { Dst::None } else { Dst::Slot };
    let shape = match op {
        Op::Iconst | Op::Fconst => {
            let konst = plan.konst[results[0].as_u32() as usize];
            let (imm, wide) = immediate(konst);
            sel.imm = imm;
            let auxes: [u8; 2] = if wide { [1, 0] } else { [0, 1] };
            auxes.into_iter().map(|aux| Shape { op, ty, aux, srcs: [Src::None; 3], dst }).find(|it| has(library, *it))
        }
        _ => base(plan, func, library, info, Shape { op, ty, aux, srcs: [Src::None; 3], dst }, sel.args()),
    };
    sel.shape = shape.ok_or_else(|| {
        // alloc: the error's message, once per failed compile.
        Error::at(inst, format!("`{opcode}` of {}: no stencil of `{}` takes its operands", ty.name(), info.name))
    })?;
    Ok(sel)
}

/// The shape of `shape`'s op with as many of `args` folded as immediates as one of the set
/// takes, and every other operand from its slot.
fn base(
    plan: &Plan<'_>,
    func: &ir::Function,
    library: &Reader<'_>,
    info: &Info,
    shape: Shape,
    args: &[ir::Value],
) -> Option<Shape> {
    let mut foldable = 0u8;
    for (n, &arg) in args.iter().enumerate() {
        if info.imm & (1 << n) != 0 && folds(plan, func, arg) {
            foldable |= 1 << n;
        }
    }
    let mut imms = foldable;
    loop {
        let mut it = shape;
        for n in 0..args.len() {
            it.srcs[n] = if imms & (1 << n) != 0 { Src::Imm } else { Src::Slot };
        }
        if has(library, it) {
            return Some(it);
        }
        if imms == 0 {
            return None;
        }
        imms = (imms - 1) & foldable;
    }
}

/// Whether `value` is an `iconst` a stencil can take as its 32-bit immediate.
fn folds(plan: &Plan<'_>, func: &ir::Function, value: ir::Value) -> bool {
    match plan.konst[value.as_u32() as usize] {
        Konst::Int(it) => func.dfg.value_type(value).bits() <= 32 || i32::try_from(it).is_ok(),
        _ => false,
    }
}

/// An `Iconst`/`Fconst` stencil's `Imm0`, and whether it needs all 64 bits.
pub(super) fn immediate(konst: Konst) -> (i64, bool) {
    match konst {
        Konst::Int(it) => (it, i32::try_from(it).is_err()),
        Konst::F16(bits) => (i64::from(bits as i16), false),
        Konst::F32(bits) => (i64::from(bits as i32), false),
        Konst::F64(bits) => (bits as i64, i32::try_from(bits as i64).is_err()),
        Konst::None | Konst::Wide => (0, false),
    }
}

/// Fuses `inst` (a `brif` or `select`) with the `icmp` right before it that only it uses,
/// when the fused shape exists.
fn fuse(plan: &mut Plan<'_>, func: &ir::Function, library: &Reader<'_>, inst: ir::Inst, sel: &mut Sel) {
    let op = sel.shape.op;
    if !matches!(op, Op::Brif | Op::Select) {
        return;
    }
    let cond = sel.args[0];
    let ir::ValueDef::Result(icmp, 0) = func.dfg.value_def(cond) else { return };
    if func.layout.prev_inst(inst) != Some(icmp) || plan.uses[cond.as_u32() as usize] != 1 {
        return;
    }
    let Data::IntCompare { opcode: Opcode::Icmp, cond: cc, .. } = func.dfg.insts[icmp] else { return };
    if plan.sels[icmp.as_u32() as usize].kind != Kind::Stencil {
        return;
    }
    let [a, b] = [0, 1].map(|n| plan.sels[icmp.as_u32() as usize].args[n]);
    let Some(compared) = ty_of(func.dfg.value_type(a)) else { return };
    let (fused, args, cc) = match op {
        Op::Brif => {
            let shape =
                Shape { op: Op::BrIcmp, ty: compared, aux: int_cc(cc) as u8, srcs: [Src::None; 3], dst: Dst::None };
            (shape, [a, b], cc)
        }
        _ => {
            let (x, y) = (sel.args[1], sel.args[2]);
            let (args, cc) = if x == a && y == b {
                ([a, b], cc)
            } else if x == b && y == a {
                ([b, a], cc.swap_args())
            } else {
                return;
            };
            let aux = int_cc(cc) as u8 | (compared as u8) << 4;
            let shape = Shape { op: Op::SelectIcmp, ty: compared, aux, srcs: [Src::None; 3], dst: Dst::Slot };
            (shape, args, cc)
        }
    };
    let Some(shape) = base(plan, func, library, fused.op.info(), fused, &args) else { return };
    let at = plan.ord[inst.as_u32() as usize];
    for value in args {
        let last = &mut plan.last[value.as_u32() as usize];
        if *last != GLOBAL {
            *last = (*last).max(at);
        }
    }
    plan.sels[icmp.as_u32() as usize].kind = Kind::Fused;
    sel.shape = shape;
    sel.args = [args[0], args[1], ir::Value::reserved_value()];
    sel.nargs = 2;
    sel.cc = cc;
}

/// Fuses `inst`, a load or store at offset 0, with the confinement of its address,
/// `iadd vmctx, uextend.i64 (ireduce.i32 (iadd p, iconst off))` (either `iadd` in either
/// operand order; without the inner one `off` is 0), when each of those values has that one
/// use, each is a stencil's in `inst`'s block, and the fused shape exists.
fn confine(plan: &mut Plan<'_>, func: &ir::Function, library: &Reader<'_>, inst: ir::Inst, sel: &mut Sel) {
    let access = sel.shape.op;
    if !confinable(access) || sel.offset != 0 {
        return;
    }
    let Some((op, _)) = Shape::confining(access, Disp::Zero) else { return };
    let block = func.layout.inst_block(inst);
    // The instruction of `opcode` that defines `value` of `ty`, a stencil's in `block`, when
    // `value` has one use.
    let def = |value: ir::Value, opcode: Opcode, ty: ir::Type| {
        let ir::ValueDef::Result(it, 0) = func.dfg.value_def(value) else { return None };
        let ok = func.dfg.insts[it].opcode() == opcode
            && func.dfg.value_type(value) == ty
            && plan.uses[value.as_u32() as usize] == 1
            && func.layout.inst_block(it) == block
            && plan.sels[it.as_u32() as usize].kind == Kind::Stencil;
        ok.then_some(it)
    };
    let arg = |of: ir::Inst, n: usize| func.dfg.resolve_aliases(func.dfg.inst_args(of)[n]);
    let store = op == Op::StoreConfined;
    let Some(add) = def(sel.args[usize::from(store)], Opcode::Iadd, types::I64) else { return };
    let Some((vmctx, extend)) = [(0, 1), (1, 0)].into_iter().find_map(|(v, e)| {
        Some((arg(add, v), def(arg(add, e), Opcode::Uextend, types::I64)?))
    }) else {
        return;
    };
    let Some(reduce) = def(arg(extend, 0), Opcode::Ireduce, types::I32) else { return };
    let wrapped = arg(reduce, 0);
    if func.dfg.value_type(wrapped) != types::I64 {
        return;
    }
    let sum = def(wrapped, Opcode::Iadd, types::I64).and_then(|sum| {
        [(0, 1), (1, 0)].into_iter().find_map(|(p, k)| match plan.konst[arg(sum, k).as_u32() as usize] {
            Konst::Int(off) => Some((sum, arg(sum, p), off as i32)),
            _ => None,
        })
    });
    let (p, off) = sum.map_or((wrapped, 0), |(_, p, off)| (p, off));
    let disps: &[Disp] = match off {
        0 => &[Disp::Zero],
        1..4096 => &[Disp::Lo12, Disp::Imm32],
        _ => &[Disp::Imm32],
    };
    let (args, nargs) =
        if store { ([sel.args[0], p, vmctx], 3) } else { ([p, vmctx, ir::Value::reserved_value()], 2) };
    let Some(shape) = disps.iter().find_map(|&disp| {
        let (op, aux) = Shape::confining(access, disp)?;
        let shape = Shape { op, aux, srcs: [Src::None; 3], ..sel.shape };
        base(plan, func, library, op.info(), shape, &args[..nargs])
    }) else {
        return;
    };
    let at = plan.ord[inst.as_u32() as usize];
    for value in [p, vmctx] {
        let last = &mut plan.last[value.as_u32() as usize];
        if *last != GLOBAL {
            *last = (*last).max(at);
        }
    }
    for fused in [Some(add), Some(extend), Some(reduce), sum.map(|it| it.0)].into_iter().flatten() {
        plan.sels[fused.as_u32() as usize].kind = Kind::Fused;
    }
    sel.shape = shape;
    sel.args = args;
    sel.nargs = nargs as u8;
    sel.offset = i64::from(off);
}

/// Fuses `inst`, an `fma` whose operand 1 is an `fneg`, into the `fma` of `aux` 1 of the
/// `fneg`'s operand: Cranelift's AArch64 `fmadd x (fneg y) z` is `fmsub x y z` (lower.isle),
/// which flips `x`'s sign where the `fneg` flipped `y`'s, so the NaN of a NaN `x` differs
/// unless the stencils fuse it too. Cranelift matches the `fneg` in any block and whatever its
/// other uses, so this fusion has no condition; the `fneg` stays for its other uses, or is
/// dropped. The other `fneg` operands Cranelift folds (`x`, and `z` of a scalar) give the same
/// bits fused or not.
fn negate(
    plan: &mut Plan<'_>,
    func: &ir::Function,
    library: &Reader<'_>,
    inst: ir::Inst,
    sel: &mut Sel,
) -> Result<(), Error> {
    if sel.shape.op != Op::Fma {
        return Ok(());
    }
    let negated = sel.args[1];
    let ir::ValueDef::Result(fneg, 0) = func.dfg.value_def(negated) else { return Ok(()) };
    let Data::Unary { opcode: Opcode::Fneg, arg } = func.dfg.insts[fneg] else { return Ok(()) };
    let y = func.dfg.resolve_aliases(arg);
    let args = [sel.args[0], y, sel.args[2]];
    let fused = Shape { aux: 1, srcs: [Src::None; 3], ..sel.shape };
    let shape = base(plan, func, library, Op::Fma.info(), fused, &args).ok_or_else(|| {
        // alloc: the error's message, once per failed compile.
        Error::at(inst, format!("`fma` of {} by an `fneg`: no `fmsub` stencil takes its operands", sel.shape.ty.name()))
    })?;

    let (n, v) = (negated.as_u32() as usize, y.as_u32() as usize);
    plan.uses[n] -= 1;
    if plan.uses[n] == 0 {
        plan.dead[fneg.as_u32() as usize] = true;
        plan.sels[fneg.as_u32() as usize].kind = Kind::Fused;
    } else {
        plan.uses[v] += 1;
    }
    let block = func.layout.inst_block(inst).expect("a selected instruction is in a block");
    let at = plan.ord[inst.as_u32() as usize];
    let last = &mut plan.last[v];
    if *last != GLOBAL {
        *last = if plan.def_block[v] == block.as_u32() { (*last).max(at) } else { GLOBAL };
    }
    sel.shape = shape;
    sel.args = args;
    Ok(())
}

/// Whether `op` is an access some confined op fuses.
fn confinable(op: Op) -> bool {
    matches!(
        op,
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
            | Op::AtomicLoad
            | Op::Store
            | Op::Istore8
            | Op::Istore16
            | Op::Istore32
            | Op::AtomicStore
    )
}

/// The value whose type a stencil of `op` is named by: result 0, or for the ops whose
/// meaning is by an operand's type, operand 0. `None` for an op with neither.
fn ty_value(func: &ir::Function, inst: ir::Inst, op: Op) -> Option<ir::Type> {
    let args = func.dfg.inst_args(inst);
    let results = func.dfg.inst_results(inst);
    let value = if by_operand(op) { args.first() } else { results.first().or(args.first()) };
    value.map(|it| func.dfg.value_type(*it))
}

/// The ops whose stencils are named by operand 0's type, not result 0's.
fn by_operand(op: Op) -> bool {
    matches!(
        op,
        Op::Icmp
            | Op::Fcmp
            | Op::Brif
            | Op::Trapz
            | Op::Trapnz
            | Op::VanyTrue
            | Op::VallTrue
            | Op::VhighBits
            | Op::Extractlane
            | Op::Isplit
            | Op::Store
            | Op::Istore8
            | Op::Istore16
            | Op::Istore32
            | Op::AtomicStore
    )
}

fn little_endian(func: &ir::Function, inst: ir::Inst, flags: ir::MemFlags) -> Result<(), Error> {
    match func.dfg.mem_flags[flags].explicit_endianness() {
        Some(ir::Endianness::Big) => Err(Error::at(
            inst,
            // alloc: the error's message, once per failed compile.
            format!("`{}` of big-endian memory: every stencil is little-endian", func.dfg.insts[inst].opcode()),
        )),
        _ => Ok(()),
    }
}

fn no_type(inst: ir::Inst, opcode: Opcode, ty: ir::Type) -> Error {
    // alloc: the error's message, once per failed compile.
    Error::at(inst, format!("`{opcode}` of {ty}: no stencil is of that type"))
}

/// `inst`'s controlling type: its first result's, else its first operand's, else
/// `types::INVALID`.
fn inst_type(func: &ir::Function, inst: ir::Inst) -> ir::Type {
    match func.dfg.inst_results(inst).first().or(func.dfg.inst_args(inst).first()) {
        Some(value) => func.dfg.value_type(*value),
        None => types::INVALID,
    }
}

fn type_name(func: &ir::Function, inst: ir::Inst) -> String {
    match inst_type(func, inst) {
        types::INVALID => "no type".to_owned(), // alloc: part of an error's message, once per failed compile.
        ty => ty.to_string(), // alloc: as above.
    }
}

/// The stencils' type of a Cranelift type.
pub(super) fn ty_of(ty: ir::Type) -> Option<Ty> {
    Some(match ty {
        types::I8 => Ty::I8,
        types::I16 => Ty::I16,
        types::I32 => Ty::I32,
        types::I64 => Ty::I64,
        types::I128 => Ty::I128,
        types::F16 => Ty::F16,
        types::F32 => Ty::F32,
        types::F64 => Ty::F64,
        types::F128 => Ty::F128,
        types::I8X16 => Ty::I8x16,
        types::I16X8 => Ty::I16x8,
        types::I32X4 => Ty::I32x4,
        types::I64X2 => Ty::I64x2,
        types::F32X4 => Ty::F32x4,
        types::F64X2 => Ty::F64x2,
        _ => return None,
    })
}

pub(super) fn int_cc(cc: IntCC) -> IntCc {
    match cc {
        IntCC::Equal => IntCc::Equal,
        IntCC::NotEqual => IntCc::NotEqual,
        IntCC::SignedLessThan => IntCc::SignedLessThan,
        IntCC::SignedGreaterThanOrEqual => IntCc::SignedGreaterThanOrEqual,
        IntCC::SignedGreaterThan => IntCc::SignedGreaterThan,
        IntCC::SignedLessThanOrEqual => IntCc::SignedLessThanOrEqual,
        IntCC::UnsignedLessThan => IntCc::UnsignedLessThan,
        IntCC::UnsignedGreaterThanOrEqual => IntCc::UnsignedGreaterThanOrEqual,
        IntCC::UnsignedGreaterThan => IntCc::UnsignedGreaterThan,
        IntCC::UnsignedLessThanOrEqual => IntCc::UnsignedLessThanOrEqual,
    }
}

fn float_cc(cc: FloatCC) -> FloatCc {
    match cc {
        FloatCC::Ordered => FloatCc::Ordered,
        FloatCC::Unordered => FloatCc::Unordered,
        FloatCC::Equal => FloatCc::Equal,
        FloatCC::NotEqual => FloatCc::NotEqual,
        FloatCC::OrderedNotEqual => FloatCc::OrderedNotEqual,
        FloatCC::UnorderedOrEqual => FloatCc::UnorderedOrEqual,
        FloatCC::LessThan => FloatCc::LessThan,
        FloatCC::LessThanOrEqual => FloatCc::LessThanOrEqual,
        FloatCC::GreaterThan => FloatCc::GreaterThan,
        FloatCC::GreaterThanOrEqual => FloatCc::GreaterThanOrEqual,
        FloatCC::UnorderedOrLessThan => FloatCc::UnorderedOrLessThan,
        FloatCC::UnorderedOrLessThanOrEqual => FloatCc::UnorderedOrLessThanOrEqual,
        FloatCC::UnorderedOrGreaterThan => FloatCc::UnorderedOrGreaterThan,
        FloatCC::UnorderedOrGreaterThanOrEqual => FloatCc::UnorderedOrGreaterThanOrEqual,
    }
}

fn rmw(op: ir::AtomicRmwOp) -> RmwOp {
    match op {
        ir::AtomicRmwOp::Add => RmwOp::Add,
        ir::AtomicRmwOp::Sub => RmwOp::Sub,
        ir::AtomicRmwOp::And => RmwOp::And,
        ir::AtomicRmwOp::Nand => RmwOp::Nand,
        ir::AtomicRmwOp::Or => RmwOp::Or,
        ir::AtomicRmwOp::Xor => RmwOp::Xor,
        ir::AtomicRmwOp::Xchg => RmwOp::Xchg,
        ir::AtomicRmwOp::Umin => RmwOp::Umin,
        ir::AtomicRmwOp::Umax => RmwOp::Umax,
        ir::AtomicRmwOp::Smin => RmwOp::Smin,
        ir::AtomicRmwOp::Smax => RmwOp::Smax,
    }
}

/// The op of the stencils of `opcode`; `None` for the opcodes only the glue writes (calls,
/// returns, addresses of symbols, jump tables, traps) and those no IR the builder makes has.
fn op_of(opcode: Opcode) -> Option<Op> {
    Some(match opcode {
        Opcode::Iconst => Op::Iconst,
        Opcode::F16const | Opcode::F32const | Opcode::F64const => Op::Fconst,
        Opcode::F128const | Opcode::Vconst => Op::Vconst,
        Opcode::Iadd => Op::Iadd,
        Opcode::Isub => Op::Isub,
        Opcode::Ineg => Op::Ineg,
        Opcode::Iabs => Op::Iabs,
        Opcode::Imul => Op::Imul,
        Opcode::Umulhi => Op::Umulhi,
        Opcode::Smulhi => Op::Smulhi,
        Opcode::Udiv => Op::Udiv,
        Opcode::Sdiv => Op::Sdiv,
        Opcode::Urem => Op::Urem,
        Opcode::Srem => Op::Srem,
        Opcode::SqmulRoundSat => Op::SqmulRoundSat,
        Opcode::AvgRound => Op::AvgRound,
        Opcode::UaddSat => Op::UaddSat,
        Opcode::SaddSat => Op::SaddSat,
        Opcode::UsubSat => Op::UsubSat,
        Opcode::SsubSat => Op::SsubSat,
        Opcode::Smin => Op::Smin,
        Opcode::Umin => Op::Umin,
        Opcode::Smax => Op::Smax,
        Opcode::Umax => Op::Umax,
        Opcode::UaddOverflow => Op::UaddOverflow,
        Opcode::SaddOverflow => Op::SaddOverflow,
        Opcode::UsubOverflow => Op::UsubOverflow,
        Opcode::SsubOverflow => Op::SsubOverflow,
        Opcode::UmulOverflow => Op::UmulOverflow,
        Opcode::SmulOverflow => Op::SmulOverflow,
        Opcode::UaddOverflowCin => Op::UaddOverflowCin,
        Opcode::SaddOverflowCin => Op::SaddOverflowCin,
        Opcode::UsubOverflowBin => Op::UsubOverflowBin,
        Opcode::SsubOverflowBin => Op::SsubOverflowBin,
        Opcode::UaddOverflowTrap => Op::UaddOverflowTrap,
        Opcode::Band => Op::Band,
        Opcode::Bor => Op::Bor,
        Opcode::Bxor => Op::Bxor,
        Opcode::Bnot => Op::Bnot,
        Opcode::Rotl => Op::Rotl,
        Opcode::Rotr => Op::Rotr,
        Opcode::Ishl => Op::Ishl,
        Opcode::Ushr => Op::Ushr,
        Opcode::Sshr => Op::Sshr,
        Opcode::Bitrev => Op::Bitrev,
        Opcode::Clz => Op::Clz,
        Opcode::Cls => Op::Cls,
        Opcode::Ctz => Op::Ctz,
        Opcode::Bswap => Op::Bswap,
        Opcode::Popcnt => Op::Popcnt,
        Opcode::Bitselect => Op::Bitselect,
        Opcode::Bmask => Op::Bmask,
        Opcode::Icmp => Op::Icmp,
        Opcode::Fcmp => Op::Fcmp,
        Opcode::Select | Opcode::SelectSpectreGuard => Op::Select,
        Opcode::Fadd => Op::Fadd,
        Opcode::Fsub => Op::Fsub,
        Opcode::Fmul => Op::Fmul,
        Opcode::Fdiv => Op::Fdiv,
        Opcode::Sqrt => Op::Sqrt,
        Opcode::Fma => Op::Fma,
        Opcode::Fneg => Op::Fneg,
        Opcode::Fabs => Op::Fabs,
        Opcode::Fcopysign => Op::Fcopysign,
        Opcode::Fmin => Op::Fmin,
        Opcode::Fmax => Op::Fmax,
        Opcode::Ceil => Op::Ceil,
        Opcode::Floor => Op::Floor,
        Opcode::Trunc => Op::Trunc,
        Opcode::Nearest => Op::Nearest,
        Opcode::Bitcast => Op::Bitcast,
        Opcode::Ireduce => Op::Ireduce,
        Opcode::Uextend => Op::Uextend,
        Opcode::Sextend => Op::Sextend,
        Opcode::Fpromote => Op::Fpromote,
        Opcode::Fdemote => Op::Fdemote,
        Opcode::Fvdemote => Op::Fvdemote,
        Opcode::FvpromoteLow => Op::FvpromoteLow,
        Opcode::FcvtToUint => Op::FcvtToUint,
        Opcode::FcvtToSint => Op::FcvtToSint,
        Opcode::FcvtToUintSat => Op::FcvtToUintSat,
        Opcode::FcvtToSintSat => Op::FcvtToSintSat,
        Opcode::FcvtFromUint => Op::FcvtFromUint,
        Opcode::FcvtFromSint => Op::FcvtFromSint,
        Opcode::Isplit => Op::Isplit,
        Opcode::Iconcat => Op::Iconcat,
        Opcode::Snarrow => Op::Snarrow,
        Opcode::Unarrow => Op::Unarrow,
        Opcode::Uunarrow => Op::Uunarrow,
        Opcode::SwidenLow => Op::SwidenLow,
        Opcode::SwidenHigh => Op::SwidenHigh,
        Opcode::UwidenLow => Op::UwidenLow,
        Opcode::UwidenHigh => Op::UwidenHigh,
        Opcode::IaddPairwise => Op::IaddPairwise,
        Opcode::ScalarToVector => Op::ScalarToVector,
        Opcode::Splat => Op::Splat,
        Opcode::Swizzle => Op::Swizzle,
        Opcode::Shuffle => Op::Shuffle,
        Opcode::Insertlane => Op::Insertlane,
        Opcode::Extractlane => Op::Extractlane,
        Opcode::VanyTrue => Op::VanyTrue,
        Opcode::VallTrue => Op::VallTrue,
        Opcode::VhighBits => Op::VhighBits,
        Opcode::Blendv => Op::Blendv,
        Opcode::X86Pshufb => Op::X86Pshufb,
        Opcode::X86Pmulhrsw => Op::X86Pmulhrsw,
        Opcode::X86Pmaddubsw => Op::X86Pmaddubsw,
        Opcode::X86Cvtt2dq => Op::X86Cvtt2dq,
        Opcode::Load => Op::Load,
        Opcode::Store => Op::Store,
        Opcode::Uload8 => Op::Uload8,
        Opcode::Sload8 => Op::Sload8,
        Opcode::Istore8 => Op::Istore8,
        Opcode::Uload16 => Op::Uload16,
        Opcode::Sload16 => Op::Sload16,
        Opcode::Istore16 => Op::Istore16,
        Opcode::Uload32 => Op::Uload32,
        Opcode::Sload32 => Op::Sload32,
        Opcode::Istore32 => Op::Istore32,
        Opcode::Uload8x8 => Op::Uload8x8,
        Opcode::Sload8x8 => Op::Sload8x8,
        Opcode::Uload16x4 => Op::Uload16x4,
        Opcode::Sload16x4 => Op::Sload16x4,
        Opcode::Uload32x2 => Op::Uload32x2,
        Opcode::Sload32x2 => Op::Sload32x2,
        Opcode::StackAddr => Op::StackAddr,
        Opcode::AtomicLoad => Op::AtomicLoad,
        Opcode::AtomicStore => Op::AtomicStore,
        Opcode::AtomicRmw => Op::AtomicRmw,
        Opcode::AtomicCas => Op::AtomicCas,
        Opcode::Fence => Op::Fence,
        Opcode::Jump => Op::Jump,
        Opcode::Brif => Op::Brif,
        Opcode::Trapz => Op::Trapz,
        Opcode::Trapnz => Op::Trapnz,
        _ => return None,
    })
}

/// `cc` complemented, for a `BrIcmp` that falls through to its taken block.
pub(super) fn complement(cc: IntCC) -> IntCC {
    cc.complement()
}
