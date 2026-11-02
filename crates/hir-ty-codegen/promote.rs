//! Values in place of the slots of the frame that are only ever written whole and read whole.
//!
//! A local of a body is given a slot when the function is built, whether or not anything takes
//! its address, and most of what a body computes goes through a temporary that is written
//! once and read once. After the function is built, a part of a slot that one store writes,
//! and that nothing but loads of the same type reads, is the stored value where the store
//! dominates the load: the load and then the store are removed.

#![forbid(unsafe_code)]

use std::cell::RefCell;

use cranelift_codegen::{
    dominator_tree::DominatorTree,
    entity::{EntityRef, EntitySet, SecondaryMap, packed_option::PackedOption},
    flowgraph::ControlFlowGraph,
    ir::{self, InstructionData, Opcode, StackSlot, Value},
};

/// A load of a part of a slot, or a store to one.
#[derive(Clone, Copy)]
struct Access {
    slot: StackSlot,
    at: i32,
    ty: ir::Type,
    store: bool,
    inst: ir::Inst,
}

/// What [`promote`] works in, kept by the thread from one function to the next: indexed by
/// value and by slot, as Cranelift numbers them densely from 0, so that a lookup is a load.
struct Scratch {
    /// The slot each `stack_addr` of offset 0 is the address of.
    slot_of: SecondaryMap<Value, PackedOption<StackSlot>>,
    /// The slots something given an address of them may read or write.
    escaped: EntitySet<StackSlot>,
    /// The slots with an address of offset 0, which are the ones promoted.
    addressed: EntitySet<StackSlot>,
    /// The accesses, in the order of the layout and then by slot and offset.
    accesses: Vec<Access>,
}

thread_local! {
    static SCRATCH: RefCell<Scratch> = RefCell::new(Scratch {
        slot_of: SecondaryMap::new(),
        escaped: EntitySet::new(),
        addressed: EntitySet::new(),
        // alloc: empty, allocates nothing; grown once and reused by the thread's functions
        accesses: Vec::new(),
    });
}

/// Removes the loads of `func` that read what a single store wrote to a slot whose address
/// goes nowhere else, and the stores nothing reads after that. A slot of `kept` is left as
/// it is: something outside the function reads it.
pub(super) fn promote(func: &mut ir::Function, kept: impl Iterator<Item = StackSlot>) {
    SCRATCH.with_borrow_mut(|scratch| {
        let Scratch { slot_of, escaped, addressed, accesses } = scratch;
        slot_of.clear();
        escaped.clear();
        addressed.clear();
        accesses.clear();
        promote_in(func, kept, slot_of, escaped, addressed, accesses);
    });
}

fn promote_in(
    func: &mut ir::Function,
    kept: impl Iterator<Item = StackSlot>,
    slot_of: &mut SecondaryMap<Value, PackedOption<StackSlot>>,
    escaped: &mut EntitySet<StackSlot>,
    addressed: &mut EntitySet<StackSlot>,
    accesses: &mut Vec<Access>,
) {
    for slot in kept {
        escaped.insert(slot);
    }
    let mut any = false;
    for block in func.layout.blocks() {
        for inst in func.layout.block_insts(block) {
            if let InstructionData::StackAddr { stack_slot, offset, .. } = func.dfg.insts[inst] {
                if i32::from(offset) == 0 {
                    slot_of[func.dfg.first_result(inst)] = stack_slot.into();
                    addressed.insert(stack_slot);
                    any = true;
                } else {
                    escaped.insert(stack_slot);
                }
            }
        }
    }
    if !any {
        return;
    }
    let slot = |func: &ir::Function, value: Value| slot_of[func.dfg.resolve_aliases(value)].expand();
    for block in func.layout.blocks() {
        for inst in func.layout.block_insts(block) {
            match func.dfg.insts[inst] {
                InstructionData::Load { opcode: Opcode::Load, arg, offset, .. } => {
                    let Some(slot) = slot(func, arg) else { continue };
                    let ty = func.dfg.value_type(func.dfg.first_result(inst));
                    accesses.push(Access { slot, at: offset.into(), ty, store: false, inst });
                }
                InstructionData::Store {
                    opcode: Opcode::Store,
                    args: [stored, to],
                    offset,
                    ..
                } => {
                    if let Some(slot) = slot(func, stored) {
                        escaped.insert(slot);
                    }
                    let Some(slot) = slot(func, to) else { continue };
                    let ty = func.dfg.value_type(stored);
                    accesses.push(Access { slot, at: offset.into(), ty, store: true, inst });
                }
                _ => {
                    for value in func.dfg.inst_values(inst) {
                        if let Some(slot) = slot(func, value) {
                            escaped.insert(slot);
                        }
                    }
                }
            }
        }
    }
    // Stable, so that the accesses of a part stay in the order of the layout.
    blaze_ids::radix::sort_copy_by_key(accesses, |it| {
        blaze_ids::radix::pack_32(it.slot.as_u32(), blaze_ids::radix::signed_32(it.at))
    });
    let mut dominators = None;
    let mut of_slots = accesses.chunk_by(|a, b| a.slot == b.slot).peekable();
    for slot in (0..func.sized_stack_slots.len()).map(StackSlot::new) {
        let of = of_slots.next_if(|it| it[0].slot == slot).unwrap_or(&[]);
        if !addressed.contains(slot) || escaped.contains(slot) {
            continue;
        }
        if promote_slot(func, of, &mut dominators) {
            // Cranelift reserves even unreferenced explicit slots in the machine frame.
            let data = &mut func.sized_stack_slots[slot];
            data.size = 0;
            data.align_shift = 0;
        }
    }
}

/// Promotes the parts of one slot that nothing else reads or writes, given its accesses by
/// offset: whether no access of it is left. Each access is of one part, so the order the
/// parts are taken in changes nothing.
fn promote_slot(
    func: &mut ir::Function,
    accesses: &[Access],
    dominators: &mut Option<DominatorTree>,
) -> bool {
    // A part is its offset and type: two types at one offset, or a part reaching into the
    // next, overlap.
    let mut end = i32::MIN;
    for part in accesses.chunk_by(|a, b| a.at == b.at) {
        let ty = part[0].ty;
        if part[0].at < end || part.iter().any(|it| it.ty != ty) {
            return false;
        }
        end = part[0].at + ty.bytes() as i32;
    }
    let mut unused = true;
    for part in accesses.chunk_by(|a, b| a.at == b.at) {
        let mut stores = part.iter().filter(|it| it.store);
        let (Some(store), None) = (stores.next(), stores.next()) else {
            unused = false;
            continue;
        };
        let store = store.inst;
        let InstructionData::Store { args: [stored, _], .. } = func.dfg.insts[store] else {
            continue;
        };
        let dominators = dominators.get_or_insert_with(|| dominator_tree(func));
        let mut read = false;
        for load in part.iter().filter(|it| !it.store).map(|it| it.inst) {
            if !dominators.dominates(store, load, &func.layout) {
                read = true;
                continue;
            }
            let loaded = func.dfg.first_result(load);
            func.dfg.detach_inst_results(load);
            func.dfg.change_to_alias(loaded, stored);
            func.layout.remove_inst(load);
        }
        if read {
            unused = false;
        } else {
            func.layout.remove_inst(store);
        }
    }
    unused
}

/// What dominates what in `func`. Cranelift times working it out, and `wasm32-unknown-unknown`
/// has no clock to read: there, the thread is given a profiler that does not look at one.
fn dominator_tree(func: &ir::Function) -> DominatorTree {
    #[cfg(all(target_family = "wasm", target_os = "unknown"))]
    {
        use cranelift_codegen::timing::{Pass, Profiler, set_thread_profiler};
        struct Untimed;
        impl Profiler for Untimed {
            // dyn: cranelift's `Profiler::start_pass` returns `Box<dyn Any>`; a `()` is no allocation
            fn start_pass(&self, _: Pass) -> Box<dyn std::any::Any> {
                // alloc: a `Box` of `()`, which allocates nothing
                Box::new(())
            }
        }
        // alloc: none, `Untimed` has no size; once per thread, on wasm only
        thread_local!(static UNTIMED: () = drop(set_thread_profiler(Box::new(Untimed))));
        UNTIMED.with(|_| ());
    }
    DominatorTree::with_function(func, &ControlFlowGraph::with_function(func))
}

#[test]
fn only_fully_promoted_slots_release_their_storage() {
    use cranelift_codegen::ir::{AbiParam, InstBuilder, StackSlotData, StackSlotKind, types::I64};
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

    let mut func = ir::Function::new();
    func.signature.returns.push(AbiParam::new(I64));
    let mut context = FunctionBuilderContext::new();
    let mut builder = FunctionBuilder::new(&mut func, &mut context);
    let block = builder.create_block();
    builder.switch_to_block(block);
    builder.seal_block(block);
    let slots = [(); 3].map(|()| {
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 64, 4))
    });
    let addrs = slots.map(|slot| builder.ins().stack_addr(I64, slot, 0));
    let value = builder.ins().iconst(I64, 7);
    for addr in addrs {
        builder.ins().store(super::MEM, value, addr, 0);
    }
    let loaded = builder.ins().load(I64, super::MEM, addrs[0], 0);
    builder.ins().store(super::MEM, loaded, addrs[2], 0);
    builder.ins().return_(&[loaded]);
    builder.finalize(super::target().unwrap().frontend_config());

    let [promoted, debugger, multiple_stores] = slots;
    promote(&mut func, [debugger].into_iter());
    assert_eq!(func.sized_stack_slots[promoted].size, 0);
    assert_eq!(func.sized_stack_slots[debugger].size, 64);
    assert_eq!(func.sized_stack_slots[multiple_stores].size, 64);
}

#[cfg(test)]
mod tests {
    use blaze_hash::{HashMap, HashSet};
    use blaze_ids::rng::SplitMix64;
    use cranelift_codegen::ir::{
        AbiParam, InstBuilder, StackSlotData, StackSlotKind,
        types::{F64, I32, I64},
    };
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

    use super::*;

    /// [`promote`] as it was with a hash map of the parts of each slot, which the dense one
    /// has to agree with on every function.
    fn promote_model(func: &mut ir::Function, kept: impl Iterator<Item = StackSlot>) {
        #[derive(Default)]
        struct Part {
            stores: Vec<ir::Inst>,
            loads: Vec<ir::Inst>,
        }
        let mut escaped: HashSet<StackSlot> = kept.collect();
        let mut slot_of: HashMap<Value, StackSlot> = HashMap::default();
        let mut parts: HashMap<StackSlot, HashMap<(i32, ir::Type), Part>> = HashMap::default();
        for block in func.layout.blocks() {
            for inst in func.layout.block_insts(block) {
                if let InstructionData::StackAddr { stack_slot, offset, .. } = func.dfg.insts[inst]
                {
                    if i32::from(offset) == 0 {
                        slot_of.insert(func.dfg.first_result(inst), stack_slot);
                        parts.entry(stack_slot).or_default();
                    } else {
                        escaped.insert(stack_slot);
                    }
                }
            }
        }
        if slot_of.is_empty() {
            return;
        }
        let slot = |func: &ir::Function, value: Value| {
            slot_of.get(&func.dfg.resolve_aliases(value)).copied()
        };
        for block in func.layout.blocks() {
            for inst in func.layout.block_insts(block) {
                match func.dfg.insts[inst] {
                    InstructionData::Load { opcode: Opcode::Load, arg, offset, .. } => {
                        let Some(slot) = slot(func, arg) else { continue };
                        let ty = func.dfg.value_type(func.dfg.first_result(inst));
                        let part = parts.entry(slot).or_default();
                        part.entry((offset.into(), ty)).or_default().loads.push(inst);
                    }
                    InstructionData::Store {
                        opcode: Opcode::Store,
                        args: [stored, to],
                        offset,
                        ..
                    } => {
                        if let Some(slot) = slot(func, stored) {
                            escaped.insert(slot);
                        }
                        let Some(slot) = slot(func, to) else { continue };
                        let ty = func.dfg.value_type(stored);
                        let part = parts.entry(slot).or_default();
                        part.entry((offset.into(), ty)).or_default().stores.push(inst);
                    }
                    _ => {
                        for value in func.dfg.inst_values(inst) {
                            if let Some(slot) = slot(func, value) {
                                escaped.insert(slot);
                            }
                        }
                    }
                }
            }
        }
        let mut dominators = None;
        for (slot, parts) in parts {
            if escaped.contains(&slot) {
                continue;
            }
            let mut spans: Vec<_> =
                parts.keys().map(|&(at, ty)| (at, at + ty.bytes() as i32)).collect();
            spans.sort_unstable();
            if spans.windows(2).any(|it| it[0].1 > it[1].0) {
                continue;
            }
            let mut unused = true;
            for part in parts.into_values() {
                let [store] = *part.stores.as_slice() else {
                    unused = false;
                    continue;
                };
                let InstructionData::Store { args: [stored, _], .. } = func.dfg.insts[store]
                else {
                    continue;
                };
                let dominators = dominators.get_or_insert_with(|| dominator_tree(func));
                let mut read = false;
                for load in part.loads {
                    if !dominators.dominates(store, load, &func.layout) {
                        read = true;
                        continue;
                    }
                    let loaded = func.dfg.first_result(load);
                    func.dfg.detach_inst_results(load);
                    func.dfg.change_to_alias(loaded, stored);
                    func.layout.remove_inst(load);
                }
                if read {
                    unused = false;
                } else {
                    func.layout.remove_inst(store);
                }
            }
            if unused {
                let data = &mut func.sized_stack_slots[slot];
                data.size = 0;
                data.align_shift = 0;
            }
        }
    }

    /// A function of four blocks, an entry branching to two that join, whose blocks store to,
    /// load from and pass on the addresses of a few slots, as `seed` picks.
    fn function(seed: u64) -> (ir::Function, Vec<StackSlot>) {
        let mut random = SplitMix64::new(seed);
        let mut func = ir::Function::new();
        func.signature.params.push(AbiParam::new(I32));
        let mut context = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut func, &mut context);
        let blocks = [(); 4].map(|()| builder.create_block());
        builder.append_block_params_for_function_params(blocks[0]);
        builder.switch_to_block(blocks[0]);
        let slots: Vec<_> = (0..1 + random.below(5))
            .map(|_| {
                let data = StackSlotData::new(StackSlotKind::ExplicitSlot, 32, 3);
                builder.create_sized_stack_slot(data)
            })
            .collect();
        let addrs: Vec<_> = slots
            .iter()
            .map(|&slot| {
                let offset = if random.below(8) == 0 { 8 } else { 0 };
                builder.ins().stack_addr(I64, slot, offset)
            })
            .collect();
        let ops = |builder: &mut FunctionBuilder, random: &mut SplitMix64| {
            for _ in 0..random.below(10) {
                let addr = addrs[random.below(addrs.len() as u64) as usize];
                let at = [0, 4, 8, 16][random.below(4) as usize];
                let ty = [I32, I64, F64][random.below(3) as usize];
                match random.below(9) {
                    0..4 => {
                        let value = if ty == F64 {
                            builder.ins().f64const(1.5)
                        } else {
                            builder.ins().iconst(ty, random.below(100) as i64)
                        };
                        builder.ins().store(super::super::MEM, value, addr, at);
                    }
                    4..8 => {
                        let _ = builder.ins().load(ty, super::super::MEM, addr, at);
                    }
                    _ => {
                        let other = addrs[random.below(addrs.len() as u64) as usize];
                        if random.below(2) == 0 {
                            let _ = builder.ins().iadd_imm(addr, 1);
                        } else {
                            let _ = builder.ins().store(super::super::MEM, addr, other, 0);
                        }
                    }
                }
            }
        };
        ops(&mut builder, &mut random);
        let cond = builder.block_params(blocks[0])[0];
        builder.ins().brif(cond, blocks[1], &[], blocks[2], &[]);
        for &block in &blocks[1..3] {
            builder.switch_to_block(block);
            ops(&mut builder, &mut random);
            builder.ins().jump(blocks[3], &[]);
        }
        builder.switch_to_block(blocks[3]);
        ops(&mut builder, &mut random);
        builder.ins().return_(&[]);
        builder.seal_all_blocks();
        builder.finalize(super::super::target().unwrap().frontend_config());
        let kept = slots.into_iter().filter(|_| random.below(6) == 0).collect();
        (func, kept)
    }

    #[test]
    fn dense_promotion_is_the_hashed_one() {
        for seed in 0..500 {
            let (func, kept) = function(seed);
            let (mut dense, mut hashed) = (func.clone(), func);
            promote(&mut dense, kept.iter().copied());
            promote_model(&mut hashed, kept.into_iter());
            assert_eq!(dense.display().to_string(), hashed.display().to_string(), "seed {seed}");
        }
    }
}
