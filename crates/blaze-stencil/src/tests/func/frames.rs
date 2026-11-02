//! Frames: stack slots of several sizes and alignments (up to 64) written, passed by
//! `stack_addr` to a callee of the other backend that writes through it, and read back;
//! enough values live across blocks that their slots go past `Glue::NEAR` on AArch64
//! (32 KiB of 8-byte slots, 64 KiB of 16-byte ones), used by stencils, by edges and by a
//! call; and frames larger than a page, whose prologue probes each page.
//!
//! Cranelift aligns a stack slot only within its 16-byte-aligned frame, so its addresses of
//! a slot asking for 32 or 64 are 16-aligned; the stencil backend realigns its frame
//! register to the largest alignment a slot asks for (stencil.md, "What a compile is"), so
//! its addresses are aligned as asked. The slots family returns the bits each address
//! misses and the test holds each backend to its own rule.

use cranelift_codegen::ir::{self, InstBuilder, types};
use cranelift_frontend::FunctionBuilder;

use super::{FUNCS, Family, Mk, call, i128_const, import, mem, mix, slot};
use crate::arch::{Glue, aarch64::Aarch64};

pub const FAMILIES: &[fn(&Mk) -> Family] = &[slots, far, far_wide, probe];

use types::{I8, I32, I64, I128};

/// `(p, v)`: writes four words derived from `v` at `p`.
fn writer(mk: &Mk, index: u32) -> ir::Function {
    mk.func(index, mk.sig_of(&[I64, I64], &[]), |b, v| {
        let tripled = b.ins().imul_imm(v[1], 3);
        let one = b.ins().iconst(I64, 1);
        let rotated = b.ins().rotl(v[1], one);
        let flipped = b.ins().bnot(v[1]);
        for (k, word) in [v[1], tripled, rotated, flipped].into_iter().enumerate() {
            b.ins().store(mem(), word, v[0], (k * 8) as i32);
        }
        b.ins().return_(&[]);
    })
}

/// The slots of [`slots`]: `(size, align_shift)`.
pub const SLOTS: [(u32, u8); 6] = [(1, 0), (8, 3), (24, 4), (48, 5), (100, 6), (16, 4)];

/// u0 fills six stack slots, has u1 write into two of them, and returns a mix of every byte
/// it reads back and the OR of the low bits each slot's address misses of its alignment.
pub fn slots(mk: &Mk) -> Family {
    let entry = mk.func(0, mk.sig_of(&[I64, I64], &[I64, I64]), |b, v| {
        let (a, bb) = (v[0], v[1]);
        let writer = import(b, FUNCS, 1, mk.sig_of(&[I64, I64], &[]), false);
        let s: Vec<ir::StackSlot> = SLOTS.iter().map(|(size, shift)| slot(b, *size, *shift)).collect();
        let byte = b.ins().ireduce(I8, a);
        b.ins().stack_store(I64, byte, s[0], 0);
        b.ins().stack_store(I64, a, s[1], 0);
        let wide = b.ins().iconcat(a, bb);
        b.ins().stack_store(I64, wide, s[2], 0);
        let x = b.ins().bxor(a, bb);
        b.ins().stack_store(I64, x, s[2], 16);
        for k in 0..6 {
            let word = b.ins().iadd_imm(bb, k * 0x101);
            b.ins().stack_store(I64, word, s[3], (k * 8) as i32);
        }
        for k in 0..12 {
            let word = b.ins().imul_imm(a, 2 * k + 1);
            b.ins().stack_store(I64, word, s[4], (k * 8) as i32);
        }
        let low = b.ins().ireduce(I32, bb);
        b.ins().stack_store(I64, low, s[4], 96);
        let lanes = b.ins().splat(types::I32X4, low);
        b.ins().stack_store(I64, lanes, s[5], 0);

        let inside = b.ins().stack_addr(I64, s[3], 8);
        call(b, writer, &[inside, a]);
        let inside = b.ins().stack_addr(I64, s[4], 40);
        call(b, writer, &[inside, bb]);

        let mut acc = b.ins().iconst(I64, 0x0123_4567);
        let mut k = 0;
        let mut take = |b: &mut FunctionBuilder<'_>, acc: ir::Value, value: ir::Value| {
            k += 1;
            let value = super::widen(b, value, false);
            mix(b, acc, value, k)
        };
        let byte = b.ins().stack_load(I64, I8, s[0], 0);
        acc = take(b, acc, byte);
        let word = b.ins().stack_load(I64, I64, s[1], 0);
        acc = take(b, acc, word);
        for off in [0, 8, 16] {
            let word = b.ins().stack_load(I64, I64, s[2], off);
            acc = take(b, acc, word);
        }
        for off in (0..48).step_by(8) {
            let word = b.ins().stack_load(I64, I64, s[3], off);
            acc = take(b, acc, word);
        }
        for off in (0..96).step_by(8) {
            let word = b.ins().stack_load(I64, I64, s[4], off);
            acc = take(b, acc, word);
        }
        let low = b.ins().stack_load(I64, I32, s[4], 96);
        acc = take(b, acc, low);
        for off in [0, 8] {
            let word = b.ins().stack_load(I64, I64, s[5], off);
            acc = take(b, acc, word);
        }

        let mut missed = b.ins().iconst(I64, 0);
        for (at, (_, shift)) in s.iter().zip(SLOTS) {
            let addr = b.ins().stack_addr(I64, *at, 0);
            let bits = b.ins().band_imm(addr, (1i64 << shift) - 1);
            missed = b.ins().bor(missed, bits);
        }
        b.ins().return_(&[acc, missed]);
    });
    Family::new("frames/slots", vec![entry, writer(mk, 1)])
}

/// How many 8-byte values [`far`] keeps live across a block: past AArch64's `NEAR[0]`.
pub fn far_count() -> usize {
    (Aarch64::NEAR[0] / 8) as usize + 256
}

/// How many 16-byte ones [`far_wide`] keeps: past `NEAR[1]`.
pub fn far_wide_count() -> usize {
    (Aarch64::NEAR[1] / 16) as usize + 256
}

/// `count` values made in the entry block from `seed`, all used in the next one, which
/// mixes them in order and passes the last two and the mix to a third block by its
/// parameters, which calls u1 with two of them.
fn live_across(mk: &Mk, name: &'static str, ty: ir::Type, count: usize) -> Family {
    let entry = mk.func(0, mk.sig_of(&[I64, I64], &[ty]), |b, v| {
        let callee = import(b, FUNCS, 1, mk.sig_of(&[ty, ty], &[ty]), false);
        let seed = if ty == I128 { b.ins().iconcat(v[0], v[1]) } else { b.ins().bxor(v[0], v[1]) };
        let mut live = Vec::with_capacity(count);
        for k in 0..count {
            let k = (k as i64).wrapping_mul(0x9e37_79b9_7f4a_7c15u64 as i64) | 1;
            let k = if ty == I128 { i128_const(b, k) } else { b.ins().iconst(ty, k) };
            live.push(b.ins().bxor(seed, k));
        }
        let mixing = b.create_block();
        let last = b.create_block();
        let x = b.append_block_param(last, ty);
        let y = b.append_block_param(last, ty);
        let z = b.append_block_param(last, ty);
        b.ins().jump(mixing, &[]);

        b.switch_to_block(mixing);
        let mut acc = seed;
        let one = b.ins().iconst(I64, 1);
        for value in &live {
            let rotated = b.ins().rotl(acc, one);
            acc = b.ins().bxor(rotated, *value);
        }
        b.ins().jump(last, &super::args(&[acc, live[count - 1], live[count - 2]]));

        b.switch_to_block(last);
        let r = call(b, callee, &[live[count - 3], x])[0];
        let r = b.ins().bxor(r, y);
        let out = b.ins().iadd(r, z);
        b.ins().return_(&[out]);
    });
    let callee = mk.func(1, mk.sig_of(&[ty, ty], &[ty]), |b, v| {
        let thirteen = b.ins().iconst(I64, 13);
        let rotated = b.ins().rotl(v[1], thirteen);
        let out = b.ins().bxor(v[0], rotated);
        b.ins().return_(&[out]);
    });
    Family::new(name, vec![entry, callee])
}

pub fn far(mk: &Mk) -> Family {
    live_across(mk, "frames/far", I64, far_count())
}

pub fn far_wide(mk: &Mk) -> Family {
    live_across(mk, "frames/far_wide", I128, far_wide_count())
}

/// Frame sizes of [`probe`]: u0's two slots and u1's one, each past a page.
pub const PROBED: [u32; 3] = [3 * 4096 + 8, 100_000, 5000];

/// u0 has a frame of more than 24 pages, writes both ends of a 3-page slot, and passes the
/// other slot to u1 (itself more than a page of frame), which writes across its pages.
pub fn probe(mk: &Mk) -> Family {
    let entry = mk.func(0, mk.sig_of(&[I64, I64], &[I64]), |b, v| {
        let callee = import(b, FUNCS, 1, mk.sig_of(&[I64, I64], &[I64]), false);
        let small = slot(b, PROBED[0], 4);
        let large = slot(b, PROBED[1], 4);
        let end = PROBED[0] as i32 - 8;
        b.ins().stack_store(I64, v[0], small, 0);
        b.ins().stack_store(I64, v[1], small, end);
        let at = b.ins().stack_addr(I64, large, 0);
        let r = call(b, callee, &[at, v[0]])[0];
        let mut acc = r;
        for (place, off) in [(small, 0), (small, end), (large, 0), (large, 50_000), (large, PROBED[1] as i32 - 8)] {
            let word = b.ins().stack_load(I64, I64, place, off);
            acc = mix(b, acc, word, i64::from(off));
        }
        b.ins().return_(&[acc]);
    });
    let callee = mk.func(1, mk.sig_of(&[I64, I64], &[I64]), |b, v| {
        let own = slot(b, PROBED[2], 3);
        let tripled = b.ins().imul_imm(v[1], 3);
        b.ins().stack_store(I64, tripled, own, PROBED[2] as i32 - 8);
        for (k, off) in [0, 50_000, PROBED[1] as i32 - 8].into_iter().enumerate() {
            let word = b.ins().iadd_imm(v[1], k as i64);
            b.ins().store(mem(), word, v[0], off);
        }
        let back = b.ins().stack_load(I64, I64, own, PROBED[2] as i32 - 8);
        b.ins().return_(&[back]);
    });
    Family::new("frames/probe", vec![entry, callee])
}

#[cfg(all(unix, any(target_arch = "x86_64", target_arch = "aarch64")))]
mod run {
    use cranelift_codegen::data_value::DataValue;

    use super::super::world::{Built, Opts, check, host_mk, i64s, plain, random_inputs};
    use crate::tests::interp::{Nans, same};

    #[test]
    fn stack_slots_of_each_alignment() {
        let Some(mk) = host_mk() else { return };
        let family = super::slots(&mk);
        let built = Built::new(&mk, &family);
        let inputs: Vec<Vec<DataValue>> = random_inputs(40, &family, 12)
            .iter()
            .map(|input| plain(input).expect("values"))
            .collect();
        let reference = built.link(0, &[], &[]);
        let want: Vec<Vec<DataValue>> = inputs.iter().map(|input| reference.values(0, input)).collect();
        for mix in 0..built.mixes() {
            let exec = built.link(mix, &[], &[]);
            for (input, want) in inputs.iter().zip(&want) {
                let got = exec.values(0, input);
                assert!(
                    same(cranelift_codegen::ir::types::I64, &got[0], &want[0], Nans::Exact),
                    "frames/slots with {}: on {input:?}, all Cranelift reads back {:?}, this mix {:?}\n{}",
                    built.mix_name(mix),
                    want[0],
                    got[0],
                    family.clif()
                );
                let DataValue::I64(missed) = got[1] else { panic!("an i64 result") };
                let asked = if Built::stencil(mix, 0) { -1 } else { 15 };
                assert_eq!(
                    missed & asked,
                    0,
                    "frames/slots with {}: a stack slot's address misses its alignment by {missed:#x}\n{}",
                    built.mix_name(mix),
                    family.clif()
                );
            }
        }
    }

    /// On AArch64 the last of the values are past `NEAR` and relayed; on x86-64 nothing is
    /// far and this is a large frame of slots.
    #[test]
    fn values_past_the_near_slots() {
        let Some(mk) = host_mk() else { return };
        let mut inputs = vec![i64s(&[0, 0]), i64s(&[-1, 1])];
        let family = super::far(&mk);
        inputs.extend(random_inputs(41, &family, 4));
        check(&mk, &family, &inputs, &Opts::interp());
        let family = super::far_wide(&mk);
        check(&mk, &family, &inputs, &Opts::interp());
    }

    #[test]
    fn frames_larger_than_a_page_are_probed() {
        let Some(mk) = host_mk() else { return };
        let family = super::probe(&mk);
        let mut inputs = vec![i64s(&[0, 0]), i64s(&[i64::MIN, i64::MAX])];
        inputs.extend(random_inputs(42, &family, 6));
        check(&mk, &family, &inputs, &Opts::plain());
    }
}

#[test]
fn far_values_reach_past_near() {
    assert!(far_count() * 8 > Aarch64::NEAR[0] as usize);
    assert!(far_wide_count() * 16 > Aarch64::NEAR[1] as usize);
    assert!(PROBED.iter().all(|size| *size > 4096));
}
