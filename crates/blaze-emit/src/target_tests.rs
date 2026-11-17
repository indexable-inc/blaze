//! Target intrinsics emitted and run on this Mac (THREADS.md D1): core's
//! `hint::spin_loop` is `__isb(SY)` on aarch64 (core/src/hint.rs), stdarch
//! `arm_shared/barrier/mod.rs` `link_name = "llvm.aarch64.isb"` `fn isb(_: i32)` with
//! `SY = 15`; `__yield` is `hint(HINT_YIELD)`, `link_name = "llvm.aarch64.hint"`, with
//! `HINT_YIELD = 1` (arm_shared/hints.rs).
//!
//! `isb_and_hint_encode_and_run` prints "isb #15 encodes as 0xd5033fdf" and "hint #1 encodes
//! as 0xd503203f". Without pass/target.rs the emitter returns `EmitError::Unsupported` "a target
//! intrinsic" and every test here fails.

#![cfg(target_arch = "aarch64")]

use std::alloc::Global;

use blaze_lir::{
    Builder, Full, Op, Ty,
    builder::{Param, Variadic},
    func::{BlockKind, Conv, Func},
};

use crate::{
    EmitError, Isa, Os,
    tests::{emitted_for, run},
};

/// `f(x) = { <name>(imm); x }`.
fn calling(name: &str, imm: u128) -> Func<Global> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let x = b.param(Ty::I64);
    let void = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I32)], &[]);
    let id = b.intrinsic(name, void);
    let imm = b.iconst(Ty::I32, imm);
    b.target_intrinsic(id, &[imm]);
    b.ret(&[x]);
    b.finish_checked().expect("a hand-built function builds")
}

fn words(bytes: &[u8]) -> impl Iterator<Item = u32> + '_ {
    bytes.chunks_exact(4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
}

const ISA: Isa = Isa { lse: true, os: Os::MacOs };

#[test]
fn isb_and_hint_encode_and_run() {
    for (name, imm, want, text) in [
        ("llvm.aarch64.isb", 15u128, 0xd503_3fdfu32, "isb #15"),
        ("llvm.aarch64.hint", 1, 0xd503_203f, "hint #1"),
        ("llvm.aarch64.dmb", 11, 0xd503_3bbf, "dmb #11"),
        ("llvm.aarch64.dsb", 15, 0xd503_3f9f, "dsb #15"),
    ] {
        let func = calling(name, imm);
        let code = emitted_for(func.view(), &[], ISA).expect("a target intrinsic emits");
        assert_eq!(words(&code.bytes).filter(|&w| w == want).count(), 1, "{text}: one word {want:#010x}");
        // The instruction runs here and changes no register the function returns.
        let exec = run::place(&code, |_| unreachable!());
        assert_eq!(run::call(&exec, [42, 0, 0, 0]), 42, "{text} ran");
        println!("{text} encodes as {want:#010x}");
    }
}

fn refused(func: &Func<Global>) -> &'static str {
    match emitted_for(func.view(), &[], ISA) {
        Err(EmitError::Unsupported { op: Op::TargetIntrinsic, why, .. }) => why,
        Err(other) => panic!("refused for another reason: {other}"),
        Ok(_) => panic!("emitted"),
    }
}

/// Planted controls: a name with no encoding and an immediate past its field are refused by
/// name, never encoded with the bits cut.
#[test]
fn an_unknown_name_or_a_wide_immediate_is_refused() {
    assert_eq!(refused(&calling("llvm.aarch64.neon.tbl1", 0)), "a target intrinsic blaze has no encoding for");
    let past = "a target intrinsic's immediate past its field (4 bits for `isb`, `dmb`, `dsb`; 7 for `hint`)";
    assert_eq!(refused(&calling("llvm.aarch64.isb", 16)), past);
    assert_eq!(refused(&calling("llvm.aarch64.hint", 128)), past);
    // The field's edge is accepted.
    emitted_for(calling("llvm.aarch64.hint", 127).view(), &[], ISA).expect("hint #127 emits");
}
