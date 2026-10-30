//! The locals of a body whose storage never overlaps share memory in the frame, as LLVM's
//! stack colouring has them share it from the lifetime markers rustc writes. Cranelift gives
//! every slot room of its own, so a function that matches on many shapes, each arm with its
//! temporaries, had a frame of all of them at once: hir-ide's `<Ty as HirDisplay>::hir_fmt`
//! had one of 20 KB, and its tests that print a type nested two hundred deep overflowed a
//! test thread's stack of 2 MB.
//!
//! Run on the function as [`promote`](super::promote) leaves it, just before Cranelift or
//! LLVM: a local's slot that nothing addresses any more takes no room at all, and the rest are
//! put together greedily, the largest first, into one slot per group of locals of which no two
//! conflict ([`MirBody::frame_storage_conflicts`](hir_ty::mir::MirBody::frame_storage_conflicts)).

use cranelift_codegen::ir::{self, InstructionData};
use rustc_hash::FxHashMap;
use rustc_index::bit_set::BitMatrix;

use super::fragment::Named;

/// The slots of a body's locals, and which of them may hold a value at the same time.
pub(super) struct FrameLocals {
    /// `None` for a local kept in a register that never needed memory.
    pub(super) slots: Vec<Option<ir::StackSlot>>,
    /// By index in `slots`.
    pub(super) conflicts: BitMatrix<usize, usize>,
    /// Where in `slots` a local kept in a register puts the slot it gets when it needs one.
    pub(super) lazy: FxHashMap<hir_ty::mir::LocalId, usize>,
}

/// Shares the slots of `frame` between locals that never hold storage at once, and has
/// `named` say where each named local now is.
pub(super) fn share(func: &mut ir::Function, frame: &FrameLocals, named: &mut [Named]) {
    let mut slots: Vec<ir::StackSlot> = named.iter().map(|it| it.slot).collect();
    share_slots(func, frame, &mut slots, None);
    for (it, slot) in named.iter_mut().zip(slots) {
        it.slot = slot;
    }
}

/// [`share`] for LLVM, on a copy of `func` with `named` in it: only slots whose address
/// escapes share. LLVM's SROA takes the others apart into values, which a slot shared with
/// one whose address escapes would keep it from; LLVM's own stack colouring shares none,
/// as the IR has no lifetime markers.
pub(super) fn share_escaping(
    func: &ir::Function,
    frame: &FrameLocals,
    named: &mut [ir::StackSlot],
) -> ir::Function {
    let mut shared = func.clone();
    let escaping = escaping(func);
    share_slots(&mut shared, frame, named, Some(&escaping));
    shared
}

/// The slots whose address goes anywhere but to the address of a load or a store, an
/// offset from it, or a `memcpy`, `memmove` or `memset`.
fn escaping(func: &ir::Function) -> rustc_hash::FxHashSet<ir::StackSlot> {
    let dfg = &func.dfg;
    // What each address of a slot, or an offset from one, is of.
    let mut of: FxHashMap<ir::Value, ir::StackSlot> = FxHashMap::default();
    loop {
        let known = of.len();
        for block in func.layout.blocks() {
            for inst in func.layout.block_insts(block) {
                let slot = match dfg.insts[inst] {
                    InstructionData::StackAddr { stack_slot, .. } => Some(stack_slot),
                    InstructionData::Binary { opcode: ir::Opcode::Iadd, args } => {
                        args.iter().find_map(|it| of.get(&dfg.resolve_aliases(*it)).copied())
                    }
                    _ => None,
                };
                if let Some(slot) = slot {
                    of.insert(dfg.first_result(inst), slot);
                }
            }
        }
        if of.len() == known {
            break;
        }
    }
    let mut escaping = rustc_hash::FxHashSet::default();
    for block in func.layout.blocks() {
        for inst in func.layout.block_insts(block) {
            let data = &dfg.insts[inst];
            let followed = match *data {
                InstructionData::Load { .. } | InstructionData::StackAddr { .. } => true,
                InstructionData::Store { args: [value, _], .. } => {
                    if let Some(&slot) = of.get(&dfg.resolve_aliases(value)) {
                        escaping.insert(slot);
                    }
                    true
                }
                InstructionData::Binary { opcode: ir::Opcode::Iadd, .. } => true,
                InstructionData::Call { func_ref, .. } => matches!(
                    dfg.ext_funcs[func_ref].name,
                    ir::ExternalName::LibCall(
                        ir::LibCall::Memcpy | ir::LibCall::Memmove | ir::LibCall::Memset
                    )
                ),
                _ => false,
            };
            if !followed {
                escaping.extend(
                    dfg.inst_values(inst)
                        .filter_map(|it| of.get(&dfg.resolve_aliases(it)).copied()),
                );
            }
        }
    }
    escaping
}

/// Shares the slots of `frame`, those in `only` alone when it is given, and moves the slots
/// of `named` along.
fn share_slots(
    func: &mut ir::Function,
    frame: &FrameLocals,
    named: &mut [ir::StackSlot],
    only: Option<&rustc_hash::FxHashSet<ir::StackSlot>>,
) {
    if frame.slots.is_empty() {
        return;
    }
    // An address of a slot that nothing uses is what `promote` left of its loads and stores.
    let mut used = rustc_hash::FxHashSet::default();
    for block in func.layout.blocks() {
        for inst in func.layout.block_insts(block) {
            used.extend(func.dfg.inst_values(inst).map(|it| func.dfg.resolve_aliases(it)));
        }
    }
    let mut addressed = vec![false; func.sized_stack_slots.len()];
    for block in func.layout.blocks() {
        for inst in func.layout.block_insts(block) {
            let Some(slot) = slot_of(&func.dfg.insts[inst]) else { continue };
            if used.contains(&func.dfg.first_result(inst)) {
                addressed[slot.as_u32() as usize] = true;
            }
        }
    }
    for slot in named.iter() {
        addressed[slot.as_u32() as usize] = true;
    }
    let mut order: Vec<usize> = Vec::new();
    for (at, slot) in frame.slots.iter().enumerate() {
        let Some(slot) = *slot else { continue };
        if !addressed[slot.as_u32() as usize] {
            func.sized_stack_slots[slot].size = 0;
        } else if only.is_none_or(|only| only.contains(&slot)) {
            order.push(at);
        }
    }
    let slot = |at: usize| frame.slots[at].expect("a local's slot");
    let size = |func: &ir::Function, at: usize| func.sized_stack_slots[slot(at)].size;
    order.sort_by_key(|&at| (std::cmp::Reverse(size(func, at)), at));
    // Each group's members, the first of them the slot the group keeps.
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for at in order {
        let fits = |group: &Vec<usize>| group.iter().all(|&it| !frame.conflicts.contains(it, at));
        match groups.iter_mut().find(|group| fits(group)) {
            Some(group) => group.push(at),
            None => groups.push(vec![at]),
        }
    }
    let mut moved: FxHashMap<ir::StackSlot, ir::StackSlot> = FxHashMap::default();
    for group in groups.iter().filter(|it| it.len() > 1) {
        let kept = slot(group[0]);
        for &at in &group[1..] {
            let slot = slot(at);
            let (size, align_shift) =
                (func.sized_stack_slots[slot].size, func.sized_stack_slots[slot].align_shift);
            let into = &mut func.sized_stack_slots[kept];
            into.size = into.size.max(size);
            into.align_shift = into.align_shift.max(align_shift);
            func.sized_stack_slots[slot].size = 0;
            moved.insert(slot, kept);
        }
    }
    if moved.is_empty() {
        return;
    }
    let blocks: Vec<ir::Block> = func.layout.blocks().collect();
    for block in blocks {
        let insts: Vec<ir::Inst> = func.layout.block_insts(block).collect();
        for inst in insts {
            if let Some(slot) = slot_mut(&mut func.dfg.insts[inst])
                && let Some(&kept) = moved.get(slot)
            {
                *slot = kept;
            }
        }
    }
    for slot in named.iter_mut() {
        if let Some(&kept) = moved.get(slot) {
            *slot = kept;
        }
    }
}

/// The slot an instruction addresses: only `stack_addr` names one, `stack_load` and
/// `stack_store` being built of it.
fn slot_of(inst: &InstructionData) -> Option<ir::StackSlot> {
    match *inst {
        InstructionData::StackAddr { stack_slot, .. } => Some(stack_slot),
        _ => None,
    }
}

fn slot_mut(inst: &mut InstructionData) -> Option<&mut ir::StackSlot> {
    match inst {
        InstructionData::StackAddr { stack_slot, .. } => Some(stack_slot),
        _ => None,
    }
}
