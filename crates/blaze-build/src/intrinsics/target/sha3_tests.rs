//! The SHA3 intrinsics (target/sha3.rs): their names as the one set `target::named` asks, and
//! their build, legalized as the Reference and Fast tiers take it (blaze-lir `legalize`), then
//! run by blaze-lir's reference evaluator against the Arm rules computed here on `u64`s:
//! `eor3(a, b, c) = a ^ b ^ c`, `bcax(a, b, c) = a ^ (b & !c)`, `rax1(a, b) = a ^ rotl(b, 1)`,
//! `xar(a, b, imm) = rotr(a ^ b, imm)`.

use std::alloc::Global;

use blaze_lir::builder::{MemAttrs, Param, Variadic};
use blaze_lir::eval::{Outcome, Val, run_in};
use blaze_lir::func::{BlockKind, Conv, Func, RefId, SlotKind};
use blaze_lir::legalize::{QuadRoutines, SoftFloat, legalize};
use blaze_lir::{Builder, Flags, Full, Op, Ty};

use super::neon::IntShape;
use super::sha3::{ImmRefused, Sha3Call, Sha3Fn, all, rotate_amount};
use super::{Target, named};

struct NoRoutines;

impl QuadRoutines for NoRoutines {
    fn routine(&self, _: SoftFloat) -> Option<RefId> {
        None
    }
}

fn call(name: &str) -> Sha3Call {
    match named(name) {
        Ok(Target::Sha3(call)) => call,
        other => panic!("{name} is not a SHA3 call of the set: {other:?}"),
    }
}

/// How the rotate amount reaches `build` in a test: as the walk builds `IMM6 as i64` (an `i32`
/// constant, then `sext`), or as a value that is no constant.
#[derive(Clone, Copy)]
enum Imm {
    Sext(u32),
    Loaded,
}

/// The 8 bytes at `at` of `call` of the 16-byte operands `ops` and rotate amount `imm`.
fn word_of(call: Sha3Call, ops: &[[u8; 16]], imm: Imm, at: i32) -> Result<u64, ImmRefused> {
    let heap = blaze_own::testing::heap();
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let mut loaded = [blaze_lir::Value(0); 3];
    for (k, bytes) in ops.iter().enumerate() {
        let slot = b.slot(16, 4, SlotKind::Temp);
        let addr = b.slot_addr(slot);
        for (i, &byte) in bytes.iter().enumerate() {
            let v = b.iconst(Ty::I8, u128::from(byte));
            b.store(Flags::NONE, v, addr, i as i32, MemAttrs::aligned(0));
        }
        loaded[k] = b.load(call.shape().ty(), Flags::NONE, addr, 0, MemAttrs::aligned(4));
    }
    let loaded = &loaded[..ops.len()];
    let amount = if call.takes_imm() {
        let x = match imm {
            Imm::Sext(n) => {
                let c = b.iconst(Ty::I32, u128::from(n));
                b.convert(Op::Sext, Flags::NONE, Ty::I64, c)
            }
            Imm::Loaded => {
                let slot = b.slot(8, 3, SlotKind::Temp);
                let addr = b.slot_addr(slot);
                b.load(Ty::I64, Flags::NONE, addr, 0, MemAttrs::aligned(3))
            }
        };
        rotate_amount(&b, x)?
    } else {
        0
    };
    let r = super::sha3::build(&mut b, call, loaded, amount).expect("the operand count of the call");
    let slot = b.slot(16, 4, SlotKind::Temp);
    let addr = b.slot_addr(slot);
    b.store(Flags::NONE, r, addr, 0, MemAttrs::aligned(4));
    let w = b.load(Ty::I64, Flags::NONE, addr, at, MemAttrs::aligned(3));
    b.ret(&[w]);
    let f: Func<Global> = b.finish_checked().unwrap_or_else(|e| panic!("{}: the built function verifies: {e:?}", call.link_name()));
    let alloc = heap.alloc();
    let mut legal = f.view().to_func_in(alloc);
    legalize(&mut legal, &NoRoutines, alloc).unwrap_or_else(|e| panic!("{}: the vector operations legalize: {e:?}", call.link_name()));
    Ok(match run_in(legal.view(), &[], 1_000_000, Global).expect("the evaluator models it") {
        Outcome::Returned(v) => match v.first() {
            Some(Val::Int(i)) => u64::try_from(*i).expect("an i64's bits"),
            other => panic!("a non-integer result {other:?}"),
        },
        other => panic!("the run did not return: {other:?}"),
    })
}

fn lanes64(lo: u64, hi: u64) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&lo.to_le_bytes());
    out[8..].copy_from_slice(&hi.to_le_bytes());
    out
}

/// Both 64-bit lanes of the result.
fn run(call: Sha3Call, ops: &[(u64, u64)], imm: Imm) -> Result<(u64, u64), ImmRefused> {
    let mut bytes = [[0u8; 16]; 3];
    for (k, &(lo, hi)) in ops.iter().enumerate() {
        bytes[k] = lanes64(lo, hi);
    }
    let bytes = &bytes[..ops.len()];
    Ok((word_of(call, bytes, imm, 0)?, word_of(call, bytes, imm, 8)?))
}

/// Every row is found by its own link name and no other; the set's sizes.
#[test]
fn every_sha3_name_round_trips() {
    let mut seen = 0;
    for c in all() {
        let name = c.link_name();
        assert_eq!(named(name), Ok(Target::Sha3(c)), "{name}");
        assert_eq!(Target::Sha3(c).link_name(), name);
        assert_eq!(c.shape().bytes(), 16, "{name}: a Q register");
        seen += 1;
    }
    assert_eq!(seen, 18);
    let xar = call("llvm.aarch64.crypto.xar");
    assert_eq!((xar.func(), xar.shape(), xar.vectors(), xar.takes_imm()), (Sha3Fn::Xar, IntShape::V2I64, 2, true));
    let bcax = call("llvm.aarch64.crypto.bcaxs.v4i32");
    assert_eq!((bcax.func(), bcax.shape(), bcax.vectors(), bcax.takes_imm()), (Sha3Fn::Bcax, IntShape::V4I32, 3, false));
    assert!(named("llvm.aarch64.crypto.eor3u.v1i64").is_err(), "eor3 is declared at the four Q shapes only");
    assert!(matches!(named("llvm.aarch64.crypto.sha512h"), Ok(Target::Sha2(_))), "SHA-512 hashing is target/sha2.rs's set");
    println!("{seen} SHA3 names, each its own");
}

/// Each function against its rule on `u64` lanes, at several inputs. `bcax` with `b & c` in
/// place of `b & !c`, `rax1` rotating `a` in place of `b` or rotating right, and `xar` rotating
/// left each fail at least one of these.
#[test]
fn sha3_functions_follow_the_arm_rules() {
    let inputs: [(u64, u64, u64); 4] = [
        (0, 0, 0),
        (u64::MAX, 0x0123_4567_89ab_cdef, 0xf0f0_f0f0_0f0f_0f0f),
        (0x8000_0000_0000_0001, 0x7fff_ffff_ffff_fffe, 0xdead_beef_cafe_babe),
        (0x5555_5555_aaaa_aaaa, 0x8000_0000_0000_0000, u64::MAX),
    ];
    for &(a, b, c) in &inputs {
        let (a2, b2, c2) = (a.rotate_left(7), !b, c ^ 0x1111);
        let ops3 = [(a, a2), (b, b2), (c, c2)];
        for (name, rule) in [
            ("llvm.aarch64.crypto.eor3u.v2i64", (a ^ b ^ c, a2 ^ b2 ^ c2)),
            ("llvm.aarch64.crypto.eor3s.v16i8", (a ^ b ^ c, a2 ^ b2 ^ c2)),
            ("llvm.aarch64.crypto.bcaxu.v2i64", (a ^ (b & !c), a2 ^ (b2 & !c2))),
            ("llvm.aarch64.crypto.bcaxs.v8i16", (a ^ (b & !c), a2 ^ (b2 & !c2))),
            ("llvm.aarch64.crypto.bcaxu.v4i32", (a ^ (b & !c), a2 ^ (b2 & !c2))),
        ] {
            assert_eq!(run(call(name), &ops3, Imm::Sext(0)), Ok(rule), "{name} of {a:#x} {b:#x} {c:#x}");
        }
        let ops2 = [(a, a2), (b, b2)];
        let rax1 = (a ^ b.rotate_left(1), a2 ^ b2.rotate_left(1));
        assert_eq!(run(call("llvm.aarch64.crypto.rax1"), &ops2, Imm::Sext(0)), Ok(rax1), "rax1 of {a:#x} {b:#x}");
        for imm in [0, 1, 14, 63] {
            let xar = ((a ^ b).rotate_right(imm), (a2 ^ b2).rotate_right(imm));
            assert_eq!(run(call("llvm.aarch64.crypto.xar"), &ops2, Imm::Sext(imm)), Ok(xar), "xar of {a:#x} {b:#x} by {imm}");
        }
    }
    let one = run(call("llvm.aarch64.crypto.xar"), &[(1, 2), (0, 0)], Imm::Sext(1)).expect("a constant amount");
    assert_eq!(one, (1 << 63, 1));
    println!("eor3, bcax, rax1, xar follow the Arm rules at 4 inputs; xar(1, 0) by 1 = {:#x}", one.0);
}

/// `xar`'s amount must be a constant below 64: 64 and a negative `i32` are refused, as is a
/// loaded value.
#[test]
fn xar_refuses_an_amount_it_cannot_encode() {
    let xar = call("llvm.aarch64.crypto.xar");
    let ops = [(1, 1), (0, 0)];
    assert_eq!(run(xar, &ops, Imm::Sext(64)), Err(ImmRefused::PastSixBits));
    assert_eq!(run(xar, &ops, Imm::Sext(u32::MAX)), Err(ImmRefused::PastSixBits));
    assert_eq!(run(xar, &ops, Imm::Loaded), Err(ImmRefused::NotConstant));
}
