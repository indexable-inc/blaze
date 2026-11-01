//! The locals of a body whose storage never overlaps share memory in the frame, as LLVM's
//! stack colouring has them share it from the lifetime markers rustc writes. Cranelift gives
//! every slot room of its own, so a function that matches on many shapes, each arm with its
//! temporaries, had a frame of all of them at once: hir-ty's `<Ty as HirDisplay>::hir_fmt`
//! had one of 20 KB, and its tests that print a type nested two hundred deep overflowed a
//! test thread's stack of 2 MB.
//!
//! Run on the function as [`promote`](super::promote) leaves it, just before Cranelift or
//! LLVM: a local's slot that nothing addresses any more takes no room at all, and the rest are
//! put together greedily, the largest first, into one slot per group of locals of which no two
//! conflict ([`FrameStorage::frame_storage_conflicts`](hir_ty_mir::mir::FrameStorage::frame_storage_conflicts)).

#![forbid(unsafe_code)]

use blaze_hash::HashMap;
use blaze_ids::bits::BitMatrix;
use blaze_mem::Frame;
use cranelift_codegen::{
    entity::{EntitySet, SecondaryMap, packed_option::PackedOption},
    ir::{self, InstructionData},
};
use smallvec::SmallVec;

use super::fragment::Named;

/// The slots of a body's locals, and which of them may hold a value at the same time.
pub(super) struct FrameLocals {
    /// `None` for a local kept in a register that never needed memory.
    pub(super) slots: Vec<Option<ir::StackSlot>>,
    /// By index in `slots`.
    pub(super) conflicts: BitMatrix<usize>,
    /// Where in `slots` a local kept in a register puts the slot it gets when it needs one.
    pub(super) lazy: HashMap<hir_ty_mir::mir::LocalId, usize>,
}

/// Shares the slots of `frame` between locals that never hold storage at once, and has
/// `named` say where each named local now is.
pub(super) fn share(func: &mut ir::Function, frame: &FrameLocals, named: &mut [Named]) {
    let mut slots: SmallVec<[ir::StackSlot; 16]> = SmallVec::new();
    slots.extend(named.iter().map(|it| it.slot));
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
    // clone: LLVM's copy; the caller keeps `func` unshared for Cranelift should LLVM leave it
    let mut shared = func.clone();
    let escaping = escaping(func);
    share_slots(&mut shared, frame, named, Some(&escaping));
    shared
}

/// The slots whose address goes anywhere but to the address of a load or a store, an
/// offset from it, or a `memcpy`, `memmove` or `memset`.
fn escaping(func: &ir::Function) -> EntitySet<ir::StackSlot> {
    let dfg = &func.dfg;
    // What each address of a slot, or an offset from one, is of, indexed by value.
    let mut of: SecondaryMap<ir::Value, PackedOption<ir::StackSlot>> = SecondaryMap::new();
    let address_of = |of: &SecondaryMap<ir::Value, PackedOption<ir::StackSlot>>, value| {
        of[dfg.resolve_aliases(value)].expand()
    };
    let mut known = 0;
    loop {
        let before = known;
        for block in func.layout.blocks() {
            for inst in func.layout.block_insts(block) {
                let slot = match dfg.insts[inst] {
                    InstructionData::StackAddr { stack_slot, .. } => Some(stack_slot),
                    InstructionData::Binary { opcode: ir::Opcode::Iadd, args } => {
                        args.iter().find_map(|&it| address_of(&of, it))
                    }
                    _ => None,
                };
                if let Some(slot) = slot {
                    let value = dfg.first_result(inst);
                    known += usize::from(of[value].is_none());
                    of[value] = slot.into();
                }
            }
        }
        if known == before {
            break;
        }
    }
    let mut escaping = EntitySet::with_capacity(func.sized_stack_slots.len());
    for block in func.layout.blocks() {
        for inst in func.layout.block_insts(block) {
            let data = &dfg.insts[inst];
            let followed = match *data {
                InstructionData::Load { .. } | InstructionData::StackAddr { .. } => true,
                InstructionData::Store { args: [value, _], .. } => {
                    if let Some(slot) = address_of(&of, value) {
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
                for slot in dfg.inst_values(inst).filter_map(|it| address_of(&of, it)) {
                    escaping.insert(slot);
                }
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
    only: Option<&EntitySet<ir::StackSlot>>,
) {
    if frame.slots.is_empty() {
        return;
    }
    blaze_mem::frame::task(blaze_mem::Tag::Codegen, |scratch| {
        share_slots_in(func, frame, named, only, scratch)
    });
}

/// [`share_slots`], its lists of the function's slots in `scratch`.
fn share_slots_in(
    func: &mut ir::Function,
    frame: &FrameLocals,
    named: &mut [ir::StackSlot],
    only: Option<&EntitySet<ir::StackSlot>>,
    scratch: &Frame,
) {
    // An address of a slot that nothing uses is what `promote` left of its loads and stores.
    let mut used = EntitySet::with_capacity(func.dfg.num_values());
    for block in func.layout.blocks() {
        for inst in func.layout.block_insts(block) {
            for value in func.dfg.inst_values(inst) {
                used.insert(func.dfg.resolve_aliases(value));
            }
        }
    }
    let mut addressed = Vec::with_capacity_in(func.sized_stack_slots.len(), scratch);
    addressed.resize(func.sized_stack_slots.len(), false);
    for block in func.layout.blocks() {
        for inst in func.layout.block_insts(block) {
            let Some(slot) = slot_of(&func.dfg.insts[inst]) else { continue };
            if used.contains(func.dfg.first_result(inst)) {
                addressed[slot.as_u32() as usize] = true;
            }
        }
    }
    for slot in named.iter() {
        addressed[slot.as_u32() as usize] = true;
    }
    let mut members: Vec<(usize, u64), &Frame> = Vec::with_capacity_in(frame.slots.len(), scratch);
    for (at, slot) in frame.slots.iter().enumerate() {
        let Some(slot) = *slot else { continue };
        if !addressed[slot.as_u32() as usize] {
            func.sized_stack_slots[slot].size = 0;
        } else if only.is_none_or(|only| only.contains(slot)) {
            members.push((at, u64::from(func.sized_stack_slots[slot].size)));
        }
    }
    let slot = |at: usize| frame.slots[at].expect("a local's slot");
    let shared = hir_ty_mir::mir::share_greedily(&members, &frame.conflicts, scratch);
    let mut moved: SecondaryMap<ir::StackSlot, PackedOption<ir::StackSlot>> = SecondaryMap::new();
    let mut any = false;
    for (member, &(at, _)) in members.iter().enumerate() {
        let first = shared.first[shared.group_of[member] as usize] as usize;
        if first == member {
            continue;
        }
        let (kept, slot) = (slot(members[first].0), slot(at));
        let (size, align_shift) =
            (func.sized_stack_slots[slot].size, func.sized_stack_slots[slot].align_shift);
        let into = &mut func.sized_stack_slots[kept];
        into.size = into.size.max(size);
        into.align_shift = into.align_shift.max(align_shift);
        func.sized_stack_slots[slot].size = 0;
        moved[slot] = kept.into();
        any = true;
    }
    if !any {
        return;
    }
    let stencil = &mut func.stencil;
    for block in stencil.layout.blocks() {
        for inst in stencil.layout.block_insts(block) {
            if let Some(slot) = slot_mut(&mut stencil.dfg.insts[inst])
                && let Some(kept) = moved[*slot].expand()
            {
                *slot = kept;
            }
        }
    }
    for slot in named.iter_mut() {
        if let Some(kept) = moved[*slot].expand() {
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

#[cfg(test)]
mod tests {
    use blaze_hash::HashSet;
    use blaze_ids::rng::SplitMix64;
    use cranelift_codegen::ir::{
        InstBuilder, StackSlotData, StackSlotKind,
        types::{I32, I64},
    };
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

    use super::*;

    /// [`escaping`] as it was, with hash maps.
    fn escaping_model(func: &ir::Function) -> HashSet<ir::StackSlot> {
        let dfg = &func.dfg;
        let mut of: HashMap<ir::Value, ir::StackSlot> = HashMap::default();
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
        let mut escaping = HashSet::default();
        for block in func.layout.blocks() {
            for inst in func.layout.block_insts(block) {
                let followed = match dfg.insts[inst] {
                    InstructionData::Load { .. } | InstructionData::StackAddr { .. } => true,
                    InstructionData::Store { args: [value, _], .. } => {
                        if let Some(&slot) = of.get(&dfg.resolve_aliases(value)) {
                            escaping.insert(slot);
                        }
                        true
                    }
                    InstructionData::Binary { opcode: ir::Opcode::Iadd, .. } => true,
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

    /// [`share_slots`] as it was: groups as lists of their members, each tried member by
    /// member.
    fn share_model(
        func: &mut ir::Function,
        frame: &FrameLocals,
        named: &mut [ir::StackSlot],
        only: Option<&HashSet<ir::StackSlot>>,
    ) {
        if frame.slots.is_empty() {
            return;
        }
        let mut used = HashSet::default();
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
        let mut groups: Vec<Vec<usize>> = Vec::new();
        for at in order {
            let fits =
                |group: &Vec<usize>| group.iter().all(|&it| !frame.conflicts.contains(it, at));
            match groups.iter_mut().find(|group| fits(group)) {
                Some(group) => group.push(at),
                None => groups.push(vec![at]),
            }
        }
        let mut moved: HashMap<ir::StackSlot, ir::StackSlot> = HashMap::default();
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

    /// A function with slots of sizes and alignments `seed` picks, whose addresses are loaded
    /// through, offset, stored, returned or left unused; the frame over them, some of its
    /// locals in registers, with conflicts `seed` picks; and the slots of named locals.
    fn frame(seed: u64) -> (ir::Function, FrameLocals, Vec<ir::StackSlot>) {
        let mut random = SplitMix64::new(seed);
        let mut func = ir::Function::new();
        func.signature.returns.push(ir::AbiParam::new(I64));
        let mut context = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut func, &mut context);
        let block = builder.create_block();
        builder.switch_to_block(block);
        let count = 1 + random.below(24) as usize;
        let slots: Vec<_> = (0..count)
            .map(|_| {
                let size = 8 * (1 + random.below(8) as u32);
                let align_shift = random.below(5) as u8;
                let data = StackSlotData::new(StackSlotKind::ExplicitSlot, size, align_shift);
                builder.create_sized_stack_slot(data)
            })
            .collect();
        let mut returned = None;
        for &slot in &slots {
            let addr = builder.ins().stack_addr(I64, slot, 0);
            match random.below(6) {
                0 => {}
                1 => {
                    let at = builder.ins().iadd_imm(addr, 4);
                    let _ = builder.ins().load(I32, super::super::MEM, at, 0);
                }
                2 => {
                    let other = builder.ins().stack_addr(I64, slots[0], 0);
                    builder.ins().store(super::super::MEM, addr, other, 0);
                }
                3 => returned = Some(addr),
                _ => {
                    let _ = builder.ins().load(I64, super::super::MEM, addr, 0);
                }
            }
        }
        let returned = returned.unwrap_or_else(|| builder.ins().iconst(I64, 0));
        builder.ins().return_(&[returned]);
        builder.seal_all_blocks();
        builder.finalize(super::super::target().unwrap().frontend_config());
        let registers = random.below(4) as usize;
        let frame_slots: Vec<_> = slots
            .iter()
            .map(|&it| (random.below(5) != 0).then_some(it))
            .chain((0..registers).map(|_| None))
            .collect();
        let locals = frame_slots.len();
        let mut conflicts = BitMatrix::new(locals, locals);
        for a in 0..locals {
            for b in a + 1..locals {
                if random.below(3) == 0 {
                    conflicts.insert(a, b);
                    conflicts.insert(b, a);
                }
            }
        }
        let named = slots.iter().copied().filter(|_| random.below(4) == 0).collect();
        let lazy = HashMap::default();
        (func, FrameLocals { slots: frame_slots, conflicts, lazy }, named)
    }

    #[test]
    fn shared_slots_are_those_of_the_member_lists() {
        for seed in 0..500 {
            let (func, frame, named) = frame(seed);
            for escaping_only in [false, true] {
                let (mut dense, mut listed) = (func.clone(), func.clone());
                let (mut dense_named, mut listed_named) = (named.clone(), named.clone());
                let dense_escaping = escaping(&func);
                let model = escaping_model(&func);
                let ours: Vec<_> =
                    func.sized_stack_slots.keys().filter(|&it| dense_escaping.contains(it)).collect();
                let mut theirs: Vec<_> = model.iter().copied().collect();
                theirs.sort();
                assert_eq!(ours, theirs, "seed {seed}");
                let (only, only_model) =
                    if escaping_only { (Some(&dense_escaping), Some(&model)) } else { (None, None) };
                share_slots(&mut dense, &frame, &mut dense_named, only);
                share_model(&mut listed, &frame, &mut listed_named, only_model);
                let (dense, listed) = (dense.display().to_string(), listed.display().to_string());
                assert_eq!(dense, listed, "seed {seed}");
                assert_eq!(dense_named, listed_named, "seed {seed}");
            }
        }
    }
}
