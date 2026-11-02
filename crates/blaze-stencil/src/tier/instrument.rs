//! Counting in Cranelift code ([`instrument`]): the same two cells and the same rule for a
//! back edge as tier 0's glue, written into the IR before Cranelift compiles it, so a warm
//! function keeps counting toward [`super::Policy::hot`].

use cranelift_codegen::{
    cursor::{Cursor as _, FuncCursor},
    ir::{self, InstBuilder as _, immediates::Imm64, types},
};
use smallvec::SmallVec;

use super::{BACK_EDGES, CALLS, COUNTER_NAMESPACE};
use crate::Error;

/// A back edge to count.
#[derive(Clone, Copy)]
enum Edge {
    /// Before this terminator: a `jump`, whose one edge it is, or a `try_call`, whose edges
    /// carry its results and can't be moved, so its count is of the call (an unwind out of
    /// it counts too, which is fine for a heuristic).
    Before(ir::Inst),
    /// On successor `index` of a `brif` or `br_table`, through a block of its own that counts
    /// and jumps on with the edge's arguments.
    Split { inst: ir::Inst, index: u32 },
}

/// Has `func` (Cranelift IR after ABI lowering, of a 64-bit target) count into its cells:
/// one add to [`CALLS`] at its entry and one to [`BACK_EDGES`] on each branch to a block at
/// or before the branch in layout order. The cells are the symbol
/// `UserExternalName { namespace: COUNTER_NAMESPACE, index: 0 }`, colocated: codegen resolves
/// it to the unit's `Target::Counter`.
pub fn instrument(func: &mut ir::Function) -> Result<(), Error> {
    let Some(entry) = func.layout.entry_block() else {
        return Err(Error::new("counting the calls of a function with no blocks"));
    };
    let name = func.declare_imported_user_function(ir::UserExternalName::new(COUNTER_NAMESPACE, 0));
    let cells = func.create_global_value(ir::GlobalValueData::Symbol {
        name: ir::ExternalName::User(name),
        offset: Imm64::new(0),
        colocated: true,
        tls: false,
    });
    blaze_mem::frame::task(blaze_mem::Tag::Codegen, |temp| -> Result<(), Error> {
        let order = temp.alloc_slice_fill_with(func.dfg.num_blocks(), |_| u32::MAX);
        for (at, block) in func.layout.blocks().enumerate() {
            order[block.as_u32() as usize] = at as u32;
        }
        let mut edges: SmallVec<[Edge; 16]> = SmallVec::new();
        for block in func.layout.blocks() {
            let Some(last) = func.layout.last_inst(block) else { continue };
            let data = &func.dfg.insts[last];
            let from = order[block.as_u32() as usize];
            let mut before = false;
            let dests = data.branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables);
            for (index, call) in dests.iter().enumerate() {
                if order[call.block(&func.dfg.value_lists).as_u32() as usize] > from {
                    continue;
                }
                match data.opcode() {
                    ir::Opcode::Brif | ir::Opcode::BrTable => {
                        edges.push(Edge::Split { inst: last, index: index as u32 });
                    }
                    _ if !before => {
                        before = true;
                        edges.push(Edge::Before(last));
                    }
                    _ => {}
                }
            }
        }
        for edge in edges {
            match edge {
                Edge::Before(inst) => {
                    let srcloc = func.srcloc(inst);
                    let mut cursor = FuncCursor::new(func).at_inst(inst).with_srcloc(srcloc);
                    add(&mut cursor, cells, BACK_EDGES);
                }
                Edge::Split { inst, index } => split(func, cells, inst, index as usize)?,
            }
        }
        let srcloc = func.layout.first_inst(entry).map(|it| func.srcloc(it)).unwrap_or_default();
        let mut cursor = FuncCursor::new(func).at_first_insertion_point(entry).with_srcloc(srcloc);
        add(&mut cursor, cells, CALLS);
        Ok(())
    })
}

/// Moves successor `index` of `inst` to a new block right after `inst`'s that counts a back
/// edge and jumps to the successor with its arguments: plain values, which dominate the new
/// block as they do the branch.
fn split(func: &mut ir::Function, cells: ir::GlobalValue, inst: ir::Inst, index: usize) -> Result<(), Error> {
    let Some(block) = func.layout.inst_block(inst) else {
        return Err(Error::at(inst, "a branch outside the layout"));
    };
    let call = func.dfg.insts[inst].branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)[index];
    let target = call.block(&func.dfg.value_lists);
    // alloc: inline up to 8 arguments; the pool is borrowed while the jump is built.
    let args: SmallVec<[ir::BlockArg; 8]> = call.args(&func.dfg.value_lists).collect();
    let edge = func.dfg.make_block();
    func.layout.insert_block_after(edge, block);
    let srcloc = func.srcloc(inst);
    let mut cursor = FuncCursor::new(func).at_bottom(edge).with_srcloc(srcloc);
    add(&mut cursor, cells, BACK_EDGES);
    cursor.ins().jump(target, &args);
    let dfg = &mut func.dfg;
    let moved = ir::BlockCall::new(edge, std::iter::empty::<ir::BlockArg>(), &mut dfg.value_lists);
    dfg.insts[inst].branch_destination_mut(&mut dfg.jump_tables, &mut dfg.exception_tables)[index] = moved;
    Ok(())
}

/// `cells[offset] += 1`, as a plain load and store.
fn add(cursor: &mut FuncCursor<'_>, cells: ir::GlobalValue, offset: i64) {
    let flags = ir::MemFlagsData::trusted();
    let at = cursor.ins().symbol_value(types::I64, cells);
    let count = cursor.ins().load(types::I64, flags, at, offset as i32);
    let count = cursor.ins().iadd_imm(count, 1);
    cursor.ins().store(flags, count, at, offset as i32);
}
