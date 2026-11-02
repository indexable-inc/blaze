//! The first pass: each instruction's and block's place in the layout, each value's uses,
//! last use and defining block, the constants, and the pure instructions nothing uses.
//!
//! A value whose uses are all in its own block is *local*: its slot is free after its last
//! use. Any other is *global* ([`super::slots::GLOBAL`] as its last use) and keeps its slot
//! for the whole function. A use before the definition in layout order (a block placed
//! before its dominator) is in another block, so it is global.

use cranelift_codegen::ir::{self, InstructionData, Opcode};

use super::{Konst, NONE, Plan, slots::GLOBAL};
use crate::Error;

pub(super) fn run(plan: &mut Plan<'_>, func: &ir::Function) -> Result<(), Error> {
    let insts = func.dfg.num_insts();
    let values = func.dfg.num_values();
    let blocks = func.dfg.num_blocks();
    reset(&mut plan.ord, insts, NONE);
    reset(&mut plan.bix, blocks, NONE);
    reset(&mut plan.uses, values, 0);
    reset(&mut plan.last, values, 0);
    reset(&mut plan.def_block, values, NONE);
    reset(&mut plan.konst, values, Konst::None);
    reset(&mut plan.dead, insts, false);

    // Definitions first: a use may come before its definition in layout order.
    let mut ord = 0u32;
    for (bix, block) in func.layout.blocks().enumerate() {
        plan.bix[block.as_u32() as usize] = bix as u32;
        for &param in func.dfg.block_params(block) {
            plan.def_block[param.as_u32() as usize] = block.as_u32();
        }
        for inst in func.layout.block_insts(block) {
            plan.ord[inst.as_u32() as usize] = ord;
            ord += 1;
            for &result in func.dfg.inst_results(inst) {
                plan.def_block[result.as_u32() as usize] = block.as_u32();
            }
            if let Some(konst) = konst(func, inst) {
                plan.konst[func.dfg.first_result(inst).as_u32() as usize] = konst;
            }
        }
    }
    if ord == NONE {
        return Err(Error::new("a function of 2^32 instructions"));
    }

    for block in func.layout.blocks() {
        for inst in func.layout.block_insts(block) {
            let at = plan.ord[inst.as_u32() as usize];
            each_use(func, inst, |value| {
                let i = value.as_u32() as usize;
                plan.uses[i] += 1;
                let local = plan.def_block[i] == block.as_u32();
                plan.last[i] = plan.last[i].max(if local { at } else { GLOBAL });
            });
        }
    }

    // Dead pure instructions, from the end: a chain of them in one block goes in one walk.
    let mut block = func.layout.last_block();
    while let Some(here) = block {
        block = func.layout.prev_block(here);
        for inst in func.layout.block_insts(here).rev() {
            let results = func.dfg.inst_results(inst);
            if pure(func.dfg.insts[inst].opcode())
                && results.iter().all(|it| plan.uses[it.as_u32() as usize] == 0)
            {
                plan.dead[inst.as_u32() as usize] = true;
                for &arg in func.dfg.inst_args(inst) {
                    let arg = func.dfg.resolve_aliases(arg);
                    plan.uses[arg.as_u32() as usize] -= 1;
                }
            }
        }
    }
    Ok(())
}

/// Clears `column` and sizes it to `len` entries of `fill`, keeping its allocation.
pub(super) fn reset<T: Clone, A: std::alloc::Allocator>(column: &mut Vec<T, A>, len: usize, fill: T) {
    column.clear();
    column.resize(len, fill);
}

/// Calls `f` with every value `inst` reads, aliases resolved, once per operand: its
/// arguments and the values its branches pass to their blocks.
pub(super) fn each_use(func: &ir::Function, inst: ir::Inst, mut f: impl FnMut(ir::Value)) {
    for &arg in func.dfg.inst_args(inst) {
        f(func.dfg.resolve_aliases(arg));
    }
    let data = &func.dfg.insts[inst];
    for call in data.branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables) {
        for arg in call.args(&func.dfg.value_lists) {
            if let ir::BlockArg::Value(value) = arg {
                f(func.dfg.resolve_aliases(value));
            }
        }
    }
}

/// Whether dropping `opcode` when nothing uses its results changes nothing: no trap, no
/// memory access, no other effect, no control.
fn pure(opcode: Opcode) -> bool {
    !(opcode.can_trap()
        || opcode.can_load()
        || opcode.can_store()
        || opcode.other_side_effects()
        || opcode.is_call()
        || opcode.is_branch()
        || opcode.is_terminator()
        || opcode.is_return())
}

/// The constant `inst` makes, if it is a constant.
fn konst(func: &ir::Function, inst: ir::Inst) -> Option<Konst> {
    match func.dfg.insts[inst] {
        InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => {
            let bits = func.dfg.value_type(func.dfg.first_result(inst)).bits();
            Some(Konst::Int(sign_extend(imm.bits(), bits)))
        }
        InstructionData::UnaryIeee16 { opcode: Opcode::F16const, imm } => {
            Some(Konst::F16(imm.bits()))
        }
        InstructionData::UnaryIeee32 { opcode: Opcode::F32const, imm } => {
            Some(Konst::F32(imm.bits()))
        }
        InstructionData::UnaryIeee64 { opcode: Opcode::F64const, imm } => {
            Some(Konst::F64(imm.bits()))
        }
        InstructionData::UnaryConst { opcode: Opcode::Vconst | Opcode::F128const, .. } => {
            Some(Konst::Wide)
        }
        _ => None,
    }
}

/// `value`'s low `bits` bits, sign-extended.
pub(super) const fn sign_extend(value: i64, bits: u32) -> i64 {
    if bits == 0 || bits >= 64 {
        return value;
    }
    let shift = 64 - bits;
    (value << shift) >> shift
}

/// `value`'s low `bits` bits, zero-extended.
pub(super) const fn zero_extend(value: i64, bits: u32) -> i64 {
    if bits == 0 || bits >= 64 {
        return value;
    }
    value & ((1i64 << bits) - 1)
}
