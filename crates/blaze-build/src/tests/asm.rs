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

/// A `#[unsafe(naked)]` function with parameters and `naked_asm!("... bl {0} ...", sym target)`:
/// - type-checks with ZERO `E0133 RequiresUnsafe` diagnostics without an `unsafe {}` block;
/// - builds with ZERO parameter `slot_addr`/`store` instructions and ZERO `TrapSource`s;
/// - emits bare AArch64 instructions via `blaze_emit::emit_record` with `frame == 0` and one
///   `SiteKind::Call` site;
/// - CONTROL: the same `Expr::Asm` without `ASM_NAKED` fails with `DiagCode::RequiresUnsafe`.
#[test]
fn naked_asm_with_params_and_sym_checks_builds_and_emits_bare_code() {
    use blaze_code::record::{SiteKind, Symbol};
    use blaze_emit::{Isa, Named, Names, Os, Outside, emit_record};
    use blaze_lir::func::AsmRec;
    use blaze_typeck::DiagCode;

    let mut w = TestWorld::new();
    w.func(1, "resolver", &[U64, U64], U64);
    w.func(2, "target", &[U64, U64], U64);
    w.naked.push(MAIN);

    let mut b = BodyBuf::new();
    let p0 = b.bind("a");
    let p1 = b.bind("b");
    b.params.extend([p0, p1]);
    let pre = "stp x29, x30, [sp, #-16]!\nmov x29, sp\nbl ";
    let post = "\ncbz x0, 2f\nstr x0, [sp, #8]\n2:\nldp x29, x30, [sp], #16\nret";
    let joined: &'static str = Box::leak(format!("{pre}{post}").into_boxed_str());
    b.asm_text = joined;
    b.asm_pieces.extend([
        TcPiece::Text { first: 0, len: pre.len() as u32 },
        TcPiece::Operand { index: 0, modifier: None },
        TcPiece::Text { first: pre.len() as u32, len: post.len() as u32 },
    ]);
    let path = b.path("target");
    let target_expr = b.e(Expr::Path(path));
    b.asm_operands.push(TcOperand { kind: AsmKind::Sym, reg: None, expr: target_expr, out_expr: None });
    let naked_opts = (AsmRec::NAKED | AsmRec::NORETURN) as u16;
    let asm = b.e(Expr::Asm {
        pieces: List { start: 0, len: 3 },
        operands: List { start: 0, len: 1 },
        options: naked_opts,
        clobber_abis: List::EMPTY,
        arch: AsmArch::Aarch64,
    });
    let root = b.block(&[], Some(asm));
    // Clear `BodyBuf`'s default unsafe-block markers so `blaze_typeck::unsafety` runs in a safe
    // context and proves `ASM_NAKED` bypasses `E0133 RequiresUnsafe`.
    b.unsafe_blocks.clear();

    let mut ground = PortableTys::new_in(Alloc::global());
    let built = build_instance(&w, &b, MAIN, root, &[], &mut ground)
        .unwrap_or_else(|e| panic!("naked_asm! with params & sym failed: {e}"));
    let view = built.func.view();
    let text = blaze_lir::print(view);
    assert!(
        !(0..view.ops.len()).any(|i| matches!(view.op(i), Op::SlotAddr | Op::Store)),
        "naked fn must not emit parameter slot_addr/store:\n{text}"
    );
    assert!(built.traps.is_empty(), "naked fn must not emit TrapSource rows: {:?}", built.traps);

    struct OneTarget;
    impl Names for OneTarget {
        fn name(&self, of: Outside) -> Option<Named<'_>> {
            match of {
                Outside::Func(0) => Some(Named::Symbol(Symbol([0x22; 32]))),
                _ => None,
            }
        }
    }
    let heap = blaze_own::testing::heap();
    let isa = Isa { lse: true, os: if cfg!(target_os = "macos") { Os::MacOs } else { Os::Linux } };
    let (record, _called) = emit_record(
        built.func.verified(),
        &OneTarget,
        &blaze_emit::RoutinePlaces::NONE,
        isa,
        heap.alloc(),
    )
    .unwrap_or_else(|e| panic!("emit_record refused naked fn: {e}"));
    assert_eq!(record.code.len(), 36, "2 + 3 (bl sym) + 4 words = 36 bytes");
    assert_eq!(record.sites.len(), 1);
    assert_eq!(record.sites[0].at, 8);
    assert_eq!(record.sites[0].kind, SiteKind::Call);
    assert_eq!(record.frame, 0);

    // Control: regular `asm!` without `ASM_NAKED` outside an `unsafe {}` block still fails E0133.
    let mut b_ctrl = BodyBuf::new();
    let ca = b_ctrl.bind("a");
    let cb = b_ctrl.bind("b");
    b_ctrl.params.extend([ca, cb]);
    b_ctrl.asm_text = "nop";
    b_ctrl.asm_pieces.push(TcPiece::Text { first: 0, len: 3 });
    let safe_asm = b_ctrl.e(Expr::Asm {
        pieces: List { start: 0, len: 1 },
        operands: List::EMPTY,
        options: AsmRec::NORETURN as u16,
        clobber_abis: List::EMPTY,
        arch: AsmArch::Aarch64,
    });
    let ctrl_root = b_ctrl.block(&[], Some(safe_asm));
    b_ctrl.unsafe_blocks.clear();
    let view_ctrl = b_ctrl.view(MAIN, ctrl_root);
    let mut ty_ctrl = InferCtx::new_in(Alloc::global());
    let checked_ctrl = match blaze_typeck::check(&view_ctrl, &w, &mut ty_ctrl, &Needs::new_in(Alloc::global()), Alloc::global()) {
        Ok(Outcome::Done(c)) => c,
        Ok(Outcome::Need(n)) => panic!("control check needed {:?}", n.questions),
        Err(error) => panic!("control check failed: {error:?}"),
    };
    assert!(
        checked_ctrl.diags.iter().any(|d| d.code == DiagCode::RequiresUnsafe),
        "control: regular asm! outside unsafe block must fail E0133, got {:?}",
        checked_ctrl.diags
    );
}
