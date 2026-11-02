//! Copies and comparisons of a few bytes as loads and stores, not calls of the C library.
//!
//! A body copies a value too large for a register, and `ptr::copy_nonoverlapping` of a known
//! count, with `memcpy` or `memmove`, whose size is known when the function is built or once
//! the callee it is in is built into its caller. LLVM's -O0 (FastISel) makes one of at most
//! 128 bytes loads and stores, and rustc compares a value of at most two words for `raw_eq`
//! as integers: through the library each was a call, and the slots on both sides escaped,
//! which kept [`promote`](super::promote) from taking them out of the frame.

#![forbid(unsafe_code)]

use cranelift_codegen::{
    cursor::{Cursor, FuncCursor},
    ir::{self, InstBuilder, InstructionData, Opcode, Value, condcodes::IntCC, types},
};
use cranelift_frontend::FunctionBuilder;

/// The most bytes a copy of a known size is made loads and stores of: LLVM's FastISel's
/// limit for an inline `memcpy` at -O0.
const MOST: i64 = 128;

/// Makes each call of `memcpy` or `memmove` in `func` whose size is known and at most
/// [`MOST`] bytes loads and stores. Whether any was.
pub(super) fn lower_small(func: &mut ir::Function) -> bool {
    let mut lowered = false;
    let mut cursor = FuncCursor::new(func);
    while cursor.next_block().is_some() {
        while let Some(inst) = cursor.next_inst() {
            let InstructionData::Call { opcode: Opcode::Call, func_ref, .. } =
                cursor.func.dfg.insts[inst]
            else {
                continue;
            };
            let callee = &mut cursor.func.dfg.ext_funcs[func_ref];
            let ir::ExternalName::LibCall(
                call @ (ir::LibCall::Memcpy | ir::LibCall::Memmove | ir::LibCall::Memset),
            ) = callee.name
            else {
                continue;
            };
            // Called in place, as every other function is (`Recorder::declare_func_in_func`).
            callee.colocated = true;
            let overlap = match call {
                ir::LibCall::Memcpy => false,
                ir::LibCall::Memmove => true,
                _ => continue,
            };
            let &[to, from, size] = cursor.func.dfg.inst_args(inst) else { continue };
            let Some(size) = super::tier::known_value(cursor.func, size) else { continue };
            if !(0..=MOST).contains(&size) {
                continue;
            }
            // `memcpy` and `memmove` give back where they copied to, if they are declared to.
            let result = match *cursor.func.dfg.inst_results(inst) {
                [] => None,
                [result] => Some(result),
                _ => continue,
            };
            // What `memmove` copies may overlap where it goes: every byte is read first.
            let mut loaded = [to; CHUNKS];
            for (at, (offset, ty)) in chunks(size as u32).enumerate() {
                let value = cursor.ins().load(ty, super::MEM, from, offset);
                if overlap {
                    loaded[at] = value;
                } else {
                    cursor.ins().store(super::MEM, value, to, offset);
                }
            }
            if overlap {
                for ((offset, _), value) in chunks(size as u32).zip(loaded) {
                    cursor.ins().store(super::MEM, value, to, offset);
                }
            }
            cursor.func.dfg.detach_inst_results(inst);
            if let Some(result) = result {
                cursor.func.dfg.change_to_alias(result, to);
            }
            cursor.remove_inst_and_step_back();
            lowered = true;
        }
    }
    lowered
}

/// The most loads [`chunks`] splits a copy of at most [`MOST`] bytes into: 16 of 8 bytes,
/// then one each of 4, 2 and 1.
const CHUNKS: usize = MOST as usize / 8 + 3;

/// Where each load and store of a copy of `size` bytes is and what type it is of: the widest
/// integers first, as FastISel splits one.
fn chunks(size: u32) -> impl Iterator<Item = (i32, ir::Type)> {
    let mut at = 0;
    [types::I64, types::I32, types::I16, types::I8].into_iter().flat_map(move |ty| {
        let count = (size - at) / ty.bytes();
        let first = at;
        at += count * ty.bytes();
        (0..count).map(move |it| ((first + it * ty.bytes()) as i32, ty))
    })
}

/// Whether the `size` bytes at `a` and at `b` are the same, as an `i8` of 0 or 1, compared
/// as integers where they are at most two words, as rustc's `raw_eq` is; `None` for more.
pub(super) fn equal_bytes(
    builder: &mut FunctionBuilder,
    pointer: ir::Type,
    a: Value,
    b: Value,
    size: u64,
) -> Option<Value> {
    if size > 2 * u64::from(pointer.bytes()) {
        return None;
    }
    let mut differ = None;
    for (at, ty) in chunks(size as u32) {
        let x = builder.ins().load(ty, super::MEM, a, at);
        let y = builder.ins().load(ty, super::MEM, b, at);
        let ne = builder.ins().icmp(IntCC::NotEqual, x, y);
        differ = Some(match differ {
            Some(differ) => builder.ins().bor(differ, ne),
            None => ne,
        });
    }
    Some(match differ {
        Some(differ) => builder.ins().bxor_imm(differ, 1),
        None => builder.ins().iconst(types::I8, 1),
    })
}

#[test]
fn chunks_are_the_widest_first_and_fit_the_array() {
    for size in 0..=MOST as u32 {
        // alloc: a test's
        let mut expected = Vec::new();
        let mut at = 0;
        for ty in [types::I64, types::I32, types::I16, types::I8] {
            while size - at >= ty.bytes() {
                expected.push((at as i32, ty));
                at += ty.bytes();
            }
        }
        // alloc: a test's
        let chunks: Vec<_> = chunks(size).collect();
        assert_eq!(chunks, expected, "{size} bytes");
        assert!(chunks.len() <= CHUNKS);
    }
}
