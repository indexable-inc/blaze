//! `asm!` through the walk (walk/asm.rs; INTRINSICS.md 3.4, P10): quanta's counter read on
//! AArch64, `asm!("mrs {}, cntvct_el0", out(reg) t, options(nostack, nomem))` (quanta
//! src/clocks/counter.rs), built as one LIR `asm_block` and one `asm` instruction.
//!
//! The Reference (inline-assembly.html): "Operand type": `out(<reg>) <expr>`: "<expr> must be
//! a (possibly uninitialized) place expression, to which the contents of the register are
//! written when exiting the asm block"; "Register operands": AArch64 `reg` holds an `i64`
//! (`u64` is passed as LIR `i64`); "Options": `nomem` and `nostack` are the block's option
//! bits, and with no `clobber_abi` the block clobbers no ABI's registers. blaze-llvm then
//! prints the one output's constraint as `=&r` (P11, an `out` is early-clobber).
//!
//! A wrong fix fails here: a walk that keeps the placeholder as text, drops the output
//! (no result type, no store to `t`), types the result other than `i64`, or loses the options.
//!
//! `quanta_cntvct` prints
//! `asm "mrs {0}, cntvct_el0": one out(reg) output of i64, options [nomem nostack], stored to t`.

use super::*;
use blaze_lir::{Op, Value};
use blaze_typeck::{AsmArch, AsmKind, AsmOperand as TcOperand, AsmPiece as TcPiece, AsmReg};

const U64: PNode = PNode::Uint(UintTy::U64);

/// `fn main() -> u64 { let mut t = 0u64; asm!("mrs {}, cntvct_el0", out(reg) t,
/// options(nostack, nomem)); t }`, built.
fn cntvct() -> Built<Global> {
    let mut w = TestWorld::new();
    w.func(1, "main", &[], U64);
    let mut b = BodyBuf::new();
    // "mrs " is 0..4, ", cntvct_el0" 4..16, the class name "reg" 16..19.
    b.asm_text = "mrs , cntvct_el0reg";
    b.asm_pieces.extend([
        TcPiece::Text { first: 0, len: 4 },
        TcPiece::Operand { index: 0, modifier: None },
        TcPiece::Text { first: 4, len: 12 },
    ]);
    let t = b.bind("t");
    let zero = b.int(0, Some(IntSuffix::Uint(UintTy::U64)));
    let place = b.local(t);
    b.asm_operands.push(TcOperand { kind: AsmKind::Out, reg: Some(AsmReg::Class { first: 16, len: 3 }), expr: place, out_expr: None });
    let options = blaze_lir::func::AsmRec::NOMEM as u16 | blaze_lir::func::AsmRec::NOSTACK as u16;
    let asm = b.e(Expr::Asm {
        pieces: List { start: 0, len: 3 },
        operands: List { start: 0, len: 1 },
        options,
        clobber_abis: List::EMPTY,
        arch: AsmArch::Aarch64,
    });
    let tail = b.local(t);
    let root = b.block(
        &[Stmt::Let { pat: t, ty: None, init: Some(zero), else_: None }, Stmt::Expr { expr: asm, semi: true }],
        Some(tail),
    );
    // The baseline tier refuses `asm` (INTRINSICS.md 3.3: such an instance goes to the
    // Optimized tier), so it is the one op `emits` may refuse here.
    let mut ground = PortableTys::new_in(Alloc::global());
    build_instance_refusing(&w, &b, MAIN, root, &[], &mut ground, &[Op::Asm]).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn quanta_cntvct() {
    let built = cntvct();
    let view = built.func.view();
    let text = blaze_lir::print(view);
    let asms: Vec<usize, Global> = (0..view.ops.len()).filter(|&i| view.op(i) == Op::Asm).collect();
    assert_eq!(asms.len(), 1, "one `asm` instruction:\n{text}");
    assert_eq!(view.asms.len(), 1, "one `asm!` block:\n{text}");
    // The one output is the instruction's result, an `i64`, with no inputs.
    assert_eq!(view.ty(Value(asms[0] as u32)), Ty::I64, "{text}");
    // Operands are read field by field, as each op's layout (blaze-lir op.rs) lays them out:
    // `asm` is its block's row, then the list of its inputs.
    let mut asm_operands = view.operands(asms[0]);
    asm_operands.asm();
    assert!(asm_operands.values().is_empty(), "no inputs:\n{text}");
    let block = "asm a0 = options [nomem nostack] clobbers \"\" pieces [\"mrs \", {0}, \", cntvct_el0\"] operands [out class \"reg\" 0]";
    assert!(text.contains(block), "the block is not `{block}`:\n{text}");
    // The output is written to `t`: after the `asm`, a store of its result through the
    // address of the slot the tail `t` is loaded from. `store` is value, pointer, offset;
    // `load` is pointer, offset; `slot_addr` holds its slot.
    let slot_of = |ptr: Value| (view.op(ptr.index()) == Op::SlotAddr).then(|| view.operands(ptr.index()).slot());
    let tail = (asms[0] + 1..view.ops.len()).rev().find(|&i| view.op(i) == Op::Load).unwrap_or_else(|| panic!("no load of `t` after the `asm`:\n{text}"));
    let t_slot = slot_of(view.operands(tail).value()).unwrap_or_else(|| panic!("the tail `t` is not loaded from a slot:\n{text}"));
    let stored = (asms[0] + 1..tail).any(|i| {
        let mut o = view.operands(i);
        view.op(i) == Op::Store && o.value() == Value(asms[0] as u32) && slot_of(o.value()) == Some(t_slot)
    });
    assert!(stored, "the output is not stored to `t`:\n{text}");
    println!("asm \"mrs {{0}}, cntvct_el0\": one out(reg) output of i64, options [nomem nostack], stored to t");
}
