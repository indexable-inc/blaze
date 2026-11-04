//! Fuel for tests ([`meter`]): a function counts down a per-thread counter at its entry and
//! at the head of each loop, and calls the runtime when it runs out, so a test that loops
//! forever stops at a budget, at the same tick on every run (docs: /tmp handoff of issue
//! #1605 until it moves under `docs/v2/plan`).
//!
//! The pass is Cranelift IR to Cranelift IR, run where the IR is built, so the stencil tier,
//! Cranelift and LLVM all compile the same metered function: a tick is a load, an add, a
//! store, a `brif` and a call, each of which every back end already lowers. No stencil is
//! made for fuel.
//!
//! The counter is the calling OS thread's: `__blaze_fuel_cell()` gives its address, once per
//! call of a metered function, and `__blaze_fuel_out(cell)` is called by a tick that finds it
//! at 0 (the counter then reads `u64::MAX`). The runtime arms a thread on its first tick (a
//! thread starts at 0) and stops the test on a later one. The runtime's own functions
//! (symbols starting with [`PREFIX`]) must not be metered.

use cranelift_codegen::{
    cursor::{Cursor as _, FuncCursor},
    ir::{self, InstBuilder as _, types},
    isa::CallConv,
};
use smallvec::SmallVec;

/// The namespace of the runtime's two functions in a metered function's IR, beside
/// [`crate::tier::COUNTER_NAMESPACE`] (`u32::MAX - 3`) and `hir-ty-codegen`'s own (`MAX`,
/// `MAX - 1`, `MAX - 2` SanitizerCoverage, `MAX - 4` PGO counters); codegen names them by
/// [`symbol`].
pub const NAMESPACE: u32 = u32::MAX - 5;
/// `__blaze_fuel_cell() -> *mut u64`: the calling thread's counter.
pub const CELL: u32 = 0;
/// `__blaze_fuel_out(cell: *mut u64)`: a tick found the counter at 0.
pub const OUT: u32 = 1;
/// What every symbol of the fuel runtime starts with; functions so named are not metered.
pub const PREFIX: &str = "__blaze_fuel_";

/// The symbol of the runtime function `index` of [`NAMESPACE`].
pub fn symbol(index: u32) -> Option<&'static str> {
    match index {
        CELL => Some("__blaze_fuel_cell"),
        OUT => Some("__blaze_fuel_out"),
        _ => None,
    }
}

/// The names a metered function calls for the counter and for running out.
#[derive(Clone, Debug)]
pub struct Runtime {
    pub cell: ir::UserExternalName,
    pub out: ir::UserExternalName,
}

impl Runtime {
    /// blaze's runtime: [`NAMESPACE`]'s [`CELL`] and [`OUT`].
    pub fn blaze() -> Self {
        Self {
            cell: ir::UserExternalName::new(NAMESPACE, CELL),
            out: ir::UserExternalName::new(NAMESPACE, OUT),
        }
    }
}

/// What [`meter`] did to a function.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Metered {
    /// Places that tick: the entry and each loop head.
    pub ticks: u32,
}

/// Has `func` (Cranelift IR after ABI lowering, of a 64-bit target) tick its thread's fuel
/// counter at its entry and at the start of each block a back edge enters (a branch to a
/// block at or before it in layout order, as [`crate::tier::instrument`] counts them). Every
/// cycle of the control flow graph has such an edge, so metered code can't run without
/// ticking. `call_conv` is the convention of the runtime's functions (the target's C one).
pub fn meter(
    func: &mut ir::Function,
    runtime: &Runtime,
    call_conv: CallConv,
) -> Result<Metered, String> {
    let name = func.name.clone();
    // alloc: an error
    let fail = |what: &str| format!("metering {name} for fuel: {what}");
    let Some(entry) = func.layout.entry_block() else {
        return Err(fail("a function with no blocks"));
    };
    let heads = blaze_mem::frame::task(
        blaze_mem::Tag::Codegen,
        |temp| -> Result<SmallVec<[ir::Block; 8]>, String> {
            let order = temp.alloc_slice_fill_with(func.dfg.num_blocks(), |_| u32::MAX);
            for (at, block) in func.layout.blocks().enumerate() {
                order[block.as_u32() as usize] = at as u32;
            }
            let head = temp.alloc_slice_fill_with(func.dfg.num_blocks(), |_| false);
            for block in func.layout.blocks() {
                let Some(last) = func.layout.last_inst(block) else { continue };
                let from = order[block.as_u32() as usize];
                let data = &func.dfg.insts[last];
                for call in
                    data.branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                {
                    let target = call.block(&func.dfg.value_lists);
                    if target == entry {
                        return Err(fail("a branch to the entry block"));
                    }
                    if order[target.as_u32() as usize] <= from {
                        head[target.as_u32() as usize] = true;
                    }
                }
            }
            // alloc: inline up to 8 loop heads
            Ok(func.layout.blocks().filter(|block| head[block.as_u32() as usize]).collect())
        },
    )?;

    let mut cell_sig = ir::Signature::new(call_conv);
    cell_sig.returns.push(ir::AbiParam::new(types::I64));
    let cell_sig = func.import_signature(cell_sig);
    let mut out_sig = ir::Signature::new(call_conv);
    out_sig.params.push(ir::AbiParam::new(types::I64));
    let out_sig = func.import_signature(out_sig);
    let cell_name = func.declare_imported_user_function(runtime.cell.clone());
    let out_name = func.declare_imported_user_function(runtime.out.clone());
    let cell_fn = func.import_function(ir::ExtFuncData {
        name: ir::ExternalName::User(cell_name),
        signature: cell_sig,
        colocated: false,
        patchable: false,
    });
    let out_fn = func.import_function(ir::ExtFuncData {
        name: ir::ExternalName::User(out_name),
        signature: out_sig,
        colocated: false,
        patchable: false,
    });

    let Some(first) = func.layout.first_inst(entry) else {
        return Err(fail("an entry block with no instructions"));
    };
    let srcloc = func.srcloc(first);
    let rest = func.dfg.make_block();
    func.layout.split_block(rest, first);
    let mut cursor = FuncCursor::new(func).at_bottom(entry).with_srcloc(srcloc);
    let call = cursor.ins().call(cell_fn, &[]);
    let cell = cursor.func.dfg.inst_results(call)[0];
    tick(&mut cursor, entry, rest, cell, out_fn);

    for &head in &heads {
        let Some(first) = func.layout.first_inst(head) else {
            return Err(fail("a loop head with no instructions"));
        };
        let srcloc = func.srcloc(first);
        let rest = func.dfg.make_block();
        func.layout.split_block(rest, first);
        let mut cursor = FuncCursor::new(func).at_bottom(head).with_srcloc(srcloc);
        tick(&mut cursor, head, rest, cell, out_fn);
    }
    Ok(Metered { ticks: 1 + heads.len() as u32 })
}

/// Ends `block` (empty of its own instructions, which moved to `rest`) with a tick:
/// `c = *cell; *cell = c - 1; brif c, rest, out`, where `out` (at the end of the layout)
/// calls the runtime's `out(cell)` and goes on to `rest`.
fn tick(
    cursor: &mut FuncCursor<'_>,
    block: ir::Block,
    rest: ir::Block,
    cell: ir::Value,
    out_fn: ir::FuncRef,
) {
    let flags = ir::MemFlagsData::trusted();
    cursor.goto_bottom(block);
    let count = cursor.ins().load(types::I64, flags, cell, 0);
    let left = cursor.ins().iadd_imm(count, -1);
    cursor.ins().store(flags, left, cell, 0);
    let out = cursor.func.dfg.make_block();
    cursor.func.layout.append_block(out);
    cursor.ins().brif(count, rest, &[], out, &[]);
    cursor.goto_bottom(out);
    cursor.ins().call(out_fn, &[cell]);
    cursor.ins().jump(rest, &[]);
}
