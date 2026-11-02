//! Places: which results stay in an accumulator for the next stencil, each value's slot, the
//! stack slots and the area above the frame register.
//!
//! - **Accumulators.** A stencil's result 0 goes to its accumulator when the instruction
//!   written next in the block is a stencil reading it at a position its op allows one
//!   ([`Dst::Acc`] when that is its only use, [`Dst::Both`] when it has others). Both shapes
//!   must be in the library; otherwise the two keep their slot shapes. Glue never reads one,
//!   and a block starts with both dead.
//! - **Global slots.** Block parameters and values some other block reads keep a slot of their
//!   own for the whole function, laid out first, never shared.
//! - **Local slots.** Every other value gets a slot at its definition by a linear scan in
//!   layout order: the slot of a value is free after its last use and the next definition of
//!   its size reuses it. Results are placed before the instruction's operands are freed, so
//!   no result shares a slot with an operand of its instruction. A stencil result nothing
//!   reads gets no slot: its hole is patched with a glue scratch slot.
//! - **Area.** The glue's two scratch slots, the patcher's relay slots when some slot is past
//!   [`crate::arch::Glue::NEAR`], the value slots, then the stack slots at their alignment.

use blaze_mem::frame::Frame;
use cranelift_codegen::ir::{self, BlockArg};

use super::{Kind, NONE, Places, Plan, RELAYS, SCRATCH, scan::{each_use, reset}};
use crate::{
    Error,
    shape::{Dst, Shape, Src},
    table::format::Reader,
};

/// The last use of a value another block reads: it keeps its slot for the whole function.
pub(super) const GLOBAL: u32 = u32::MAX;

/// A function past this many values would overflow a 32-bit frame offset.
const MAX_VALUES: usize = 1 << 27;

pub(super) fn run(
    plan: &mut Plan<'_>,
    func: &ir::Function,
    library: &Reader<'_>,
    near: [u32; 2],
) -> Result<(), Error> {
    if func.dfg.num_values() > MAX_VALUES {
        // alloc: the error's message, once per failed compile.
        return Err(Error::new(format!(
            "a function of {} values, more than a frame of 32-bit offsets holds",
            func.dfg.num_values()
        )));
    }
    if !func.dynamic_stack_slots.is_empty() {
        return Err(Error::new("dynamic stack slots, which no IR the builder makes has"));
    }
    accumulators(plan, func, library);
    reset(&mut plan.slot, func.dfg.num_values(), NONE);
    reset(&mut plan.released, func.dfg.num_values(), false);
    reset(&mut plan.places_of, func.dfg.num_insts(), NONE);
    plan.places.clear();
    plan.pseudo.clear();
    for free in &mut plan.free {
        free.clear();
    }

    let mut top = 0u32;
    for block in func.layout.blocks() {
        for &param in func.dfg.block_params(block) {
            if plan.uses[param.as_u32() as usize] > 0 {
                let bytes = bytes_of(func, param);
                plan.slot[param.as_u32() as usize] = bump(&mut top, bytes, &mut plan.free);
            }
        }
        for inst in func.layout.block_insts(block) {
            for (k, &result) in func.dfg.inst_results(inst).iter().enumerate() {
                if wants(plan, inst, k, result) && plan.last[result.as_u32() as usize] == GLOBAL {
                    let bytes = bytes_of(func, result);
                    plan.slot[result.as_u32() as usize] = bump(&mut top, bytes, &mut plan.free);
                }
            }
        }
    }

    for block in func.layout.blocks() {
        for inst in func.layout.block_insts(block) {
            let i = inst.as_u32() as usize;
            let at = plan.ord[i];
            for (k, &result) in func.dfg.inst_results(inst).iter().enumerate() {
                let r = result.as_u32() as usize;
                if wants(plan, inst, k, result) && plan.last[r] != GLOBAL {
                    plan.slot[r] = take(&mut top, bytes_of(func, result), &mut plan.free);
                }
            }
            let pseudo = if plan.sels[i].kind == Kind::Glue {
                places(plan, func, inst, &mut top)
            } else {
                0..0
            };
            each_use(func, inst, |value| release(plan, func, value, at));
            let sel = plan.sels[i];
            if sel.kind == Kind::Stencil {
                for &arg in sel.args() {
                    release(plan, func, arg, at);
                }
            }
            for at in pseudo {
                let slot = plan.pseudo[at as usize];
                if slot != NONE {
                    plan.free[1].push(slot);
                }
            }
        }
    }

    top = top.next_multiple_of(16);
    plan.relays = SCRATCH + top > near[0].min(near[1]);
    plan.base = SCRATCH + if plan.relays { 16 * RELAYS } else { 0 };
    let mut end = u64::from(plan.base) + u64::from(top);
    let mut align = 16u32;
    reset(&mut plan.stack, func.sized_stack_slots.len(), 0);
    for (slot, data) in func.sized_stack_slots.iter() {
        let slot_align = 1u64 << data.align_shift.min(31);
        end = end.next_multiple_of(slot_align);
        plan.stack[slot.as_u32() as usize] = u32::try_from(end).map_err(|_| too_big())?;
        end += u64::from(data.size);
        align = align.max(slot_align as u32);
    }
    let size = u32::try_from(end.next_multiple_of(16)).map_err(|_| too_big())?;
    plan.area = (size, align);
    Ok(())
}

fn too_big() -> Error {
    Error::new("a frame of more than 4 GiB")
}

/// Whether `inst`'s result `k` gets a slot: a stencil's that it writes to a slot and something
/// reads, or a glue instruction's that something reads.
fn wants(plan: &Plan<'_>, inst: ir::Inst, k: usize, result: ir::Value) -> bool {
    let sel = &plan.sels[inst.as_u32() as usize];
    if plan.uses[result.as_u32() as usize] == 0 {
        return false;
    }
    match sel.kind {
        Kind::Stencil => k > 0 || matches!(sel.shape.dst, Dst::Slot | Dst::Both),
        Kind::Glue => true,
        Kind::Skip | Kind::Fused | Kind::Jump => false,
    }
}

/// Bytes of `value`'s slot: 8, or 16 for what is wider than 64 bits.
pub(super) fn bytes_of(func: &ir::Function, value: ir::Value) -> u8 {
    if func.dfg.value_type(value).bytes() > 8 { 16 } else { 8 }
}

/// A new slot of `bytes` at the top of the value slots. A 16-byte slot after an odd number of
/// 8-byte ones leaves an 8-byte gap, which the next 8-byte slot fills.
fn bump(top: &mut u32, bytes: u8, free: &mut [Vec<u32, &Frame>; 2]) -> u32 {
    if bytes == 16 && *top % 16 != 0 {
        free[0].push(*top);
        *top += 8;
    }
    let at = *top;
    *top += u32::from(bytes);
    at
}

/// A free local slot of `bytes`, or a new one.
fn take(top: &mut u32, bytes: u8, free: &mut [Vec<u32, &Frame>; 2]) -> u32 {
    match free[usize::from(bytes == 16)].pop() {
        Some(at) => at,
        None => bump(top, bytes, free),
    }
}

/// Frees `value`'s slot if `at` is its last use.
fn release(plan: &mut Plan<'_>, func: &ir::Function, value: ir::Value, at: u32) {
    let v = value.as_u32() as usize;
    if plan.last[v] != at || plan.released[v] {
        return;
    }
    let slot = plan.slot[v];
    if slot != NONE {
        plan.released[v] = true;
        plan.free[usize::from(bytes_of(func, value) == 16)].push(slot);
    }
}

/// The places of a glue instruction's values ([`Places`]): its results' slots, or for a
/// `try_call` a 16-byte slot for each `TryCallRet` and `TryCallExn` an edge reads. The range of
/// [`Plan::pseudo`] to free after the instruction: a `try_call`'s, whose values only its edges
/// read.
fn places(plan: &mut Plan<'_>, func: &ir::Function, inst: ir::Inst, top: &mut u32) -> std::ops::Range<u32> {
    let first = plan.pseudo.len() as u32;
    let data = &func.dfg.insts[inst];
    let Some(table) = data.exception_table() else {
        let results = func.dfg.inst_results(inst);
        if results.is_empty() {
            return 0..0;
        }
        for &result in results {
            let slot = plan.slot[result.as_u32() as usize];
            plan.pseudo.push(slot);
        }
        plan.places_of[inst.as_u32() as usize] = plan.places.len() as u32;
        plan.places.push(Places { first, rets: results.len() as u32, exns: 0 });
        return 0..0;
    };
    let pool = &func.dfg.value_lists;
    let calls = func.dfg.exception_tables[table].all_branches();
    let (mut rets, mut exns) = (0u32, 0u32);
    for call in calls {
        for arg in call.args(pool) {
            match arg {
                BlockArg::TryCallRet(k) => rets = rets.max(k + 1),
                BlockArg::TryCallExn(k) => exns = exns.max(k + 1),
                BlockArg::Value(_) => {}
            }
        }
    }
    plan.pseudo.resize((first + rets + exns) as usize, NONE);
    for call in calls {
        let params = func.dfg.block_params(call.block(pool));
        for (arg, &param) in call.args(pool).zip(params) {
            let at = match arg {
                BlockArg::TryCallRet(k) => first + k,
                BlockArg::TryCallExn(k) => first + rets + k,
                BlockArg::Value(_) => continue,
            };
            if plan.uses[param.as_u32() as usize] > 0 && plan.pseudo[at as usize] == NONE {
                plan.pseudo[at as usize] = take(top, 16, &mut plan.free);
            }
        }
    }
    plan.places_of[inst.as_u32() as usize] = plan.places.len() as u32;
    plan.places.push(Places { first, rets, exns });
    first..first + rets + exns
}

/// Keeps results in the accumulators, block by block.
fn accumulators(plan: &mut Plan<'_>, func: &ir::Function, library: &Reader<'_>) {
    for block in func.layout.blocks() {
        let mut prev = None;
        for inst in func.layout.block_insts(block) {
            match plan.sels[inst.as_u32() as usize].kind {
                Kind::Skip | Kind::Fused => continue,
                Kind::Glue | Kind::Jump => prev = None,
                Kind::Stencil => {
                    if let Some(prev) = prev {
                        chain(plan, func, library, prev, inst);
                    }
                    prev = Some(inst);
                }
            }
        }
    }
}

/// Passes `from`'s result 0 to `to` in the accumulator, if both shapes are in the library.
fn chain(plan: &mut Plan<'_>, func: &ir::Function, library: &Reader<'_>, from: ir::Inst, to: ir::Inst) {
    let Some(&value) = func.dfg.inst_results(from).first() else { return };
    let (f, t) = (from.as_u32() as usize, to.as_u32() as usize);
    let producer = plan.sels[f].shape;
    let consumer = plan.sels[t].shape;
    if producer.dst != Dst::Slot || !producer.op.info().dst_acc {
        return;
    }
    let info = consumer.op.info();
    let Some(n) = plan.sels[t].args().iter().enumerate().position(|(n, &arg)| {
        arg == value && consumer.srcs[n] == Src::Slot && info.acc & (1 << n) != 0
    }) else {
        return;
    };
    let mut reads = consumer;
    reads.srcs[n] = Src::Acc;
    if !has(library, reads) {
        return;
    }
    let only = plan.uses[value.as_u32() as usize] == 1;
    for dst in [Dst::Acc, Dst::Both] {
        if dst == Dst::Acc && !only {
            continue;
        }
        let gives = Shape { dst, ..producer };
        if has(library, gives) {
            plan.sels[f].shape = gives;
            plan.sels[t].shape = reads;
            return;
        }
    }
}

#[inline]
pub(super) fn has(library: &Reader<'_>, shape: Shape) -> bool {
    library.find(shape.key()).is_some()
}
