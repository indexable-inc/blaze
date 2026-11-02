//! Every argument and return shape of the three conventions, each crossing the backends.
//!
//! A *shape* family is a callee (u1) and a caller (u0) of one signature whose first
//! parameter points at a data object: the callee stores each argument it got to its own
//! place in the first half (a `StructArgument`'s bytes whole, then writes over its copy) and
//! returns some of them out of order; the caller forwards every argument to it, then
//! stores what it got itself to the second half and returns the callee's results. Called
//! from the host by a Cranelift trampoline, a mix with a stencil caller and a Cranelift
//! callee (and the reverse) shows any disagreement on where a value goes, in either
//! direction, as a byte of the object or a result that differs from the all-Cranelift link.
//!
//! The limits are families too: returns past the return registers (Cranelift has no
//! `enable_multi_ret_implicit_sret` in the IR builder's flags, so it refuses them, and the
//! stencil backend must as well), and `StructArgument` on AArch64 (a panic in Cranelift).

use cranelift_codegen::ir::{self, AbiParam, ArgumentPurpose, InstBuilder, types};
use cranelift_frontend::FunctionBuilder;

use super::{FUNCS, Family, Mk, call, import, mem, mix, slot, widen};

pub const FAMILIES: &[fn(&Mk) -> Family] = &[
    ints,
    ints_many,
    narrow_stack,
    i128_pairs,
    i128_lone,
    i128_even,
    floats,
    mixed,
    vectors,
    f16_f128,
    rets_3,
    rets_4,
    rets_8,
    rets_9,
    rets_floats_8,
    rets_mixed,
    rets_16,
    rets_17,
    struct_args,
    ext_seen,
    struct_return,
];

use types::{F16, F32, F64, F128, I8, I16, I32, I64, I128};

fn p(ty: ir::Type) -> AbiParam {
    AbiParam::new(ty)
}

fn ps(tys: &[ir::Type]) -> Vec<AbiParam> {
    tys.iter().map(|ty| p(*ty)).collect()
}

/// The bytes one parameter's place takes: 16, or a `StructArgument`'s size rounded up.
fn place(param: &AbiParam) -> u32 {
    match param.purpose {
        ArgumentPurpose::StructArgument(size) => size.next_multiple_of(16),
        _ => 16,
    }
}

/// The length of a shape's data object for `params` (without the area pointer): the
/// callee's half and the caller's.
pub fn area_len(params: &[AbiParam]) -> usize {
    2 * params.iter().map(place).sum::<u32>() as usize
}

/// Stores each of `values` (of `params`) at `base` onward in `area`.
fn record(b: &mut FunctionBuilder<'_>, area: ir::Value, base: u32, params: &[AbiParam], values: &[ir::Value]) {
    let mut at = base;
    for (param, value) in params.iter().zip(values) {
        match param.purpose {
            ArgumentPurpose::StructArgument(size) => {
                for off in (0..size).step_by(8) {
                    let word = b.ins().load(I64, mem(), *value, off as i32);
                    b.ins().store(mem(), word, area, (at + off) as i32);
                }
            }
            _ => {
                b.ins().store(mem(), *value, area, at as i32);
            }
        }
        at += place(param);
    }
}

/// Which parameter each return gives back: of its type, from the last parameter of that
/// type backwards.
fn picks(params: &[AbiParam], rets: &[AbiParam]) -> Vec<usize> {
    let mut used: Vec<(ir::Type, usize)> = Vec::new();
    rets.iter()
        .map(|ret| {
            let ty = ret.value_type;
            let of: Vec<usize> = params
                .iter()
                .enumerate()
                .filter(|(_, param)| param.value_type == ty && param.purpose == ArgumentPurpose::Normal)
                .map(|(k, _)| k)
                .collect();
            assert!(!of.is_empty(), "a return of {ty} and no parameter of it");
            let n = match used.iter_mut().find(|(it, _)| *it == ty) {
                Some((_, n)) => {
                    *n += 1;
                    *n - 1
                }
                None => {
                    used.push((ty, 1));
                    0
                }
            };
            of[of.len() - 1 - n % of.len()]
        })
        .collect()
}

/// The shape family of `params` and `rets`, refused by Cranelift on its target when
/// `refused`.
fn shape(mk: &Mk, name: &'static str, params: &[AbiParam], rets: &[AbiParam], refused: bool) -> Family {
    let mut all = vec![p(I64)];
    all.extend_from_slice(params);
    let sig = mk.sig(&all, rets);
    let half = (area_len(params) / 2) as u32;
    let chosen = picks(params, rets);
    let callee = mk.func(1, sig.clone(), |b, v| {
        record(b, v[0], 0, params, &v[1..]);
        for (param, value) in params.iter().zip(&v[1..]) {
            if let ArgumentPurpose::StructArgument(_) = param.purpose {
                let marker = b.ins().iconst(I64, -1);
                b.ins().store(mem(), marker, *value, 0);
            }
        }
        let out: Vec<ir::Value> = chosen.iter().map(|k| v[1 + k]).collect();
        b.ins().return_(&out);
    });
    let caller = mk.func(0, sig.clone(), |b, v| {
        let callee = import(b, FUNCS, 1, sig.clone(), false);
        let out = call(b, callee, v);
        record(b, v[0], half, params, &v[1..]);
        b.ins().return_(&out);
    });
    Family::new(name, vec![caller, callee]).refused(refused)
}

/// Integers of each width, with and without `uext`/`sext`, and an extended narrow return.
pub fn ints(mk: &Mk) -> Family {
    let params = [
        p(I8),
        p(I16),
        p(I32),
        p(I64),
        p(I8).uext(),
        p(I16).sext(),
        p(I32).uext(),
        p(I32).sext(),
        p(I64),
        p(I8),
    ];
    shape(mk, "abi/ints", &params, &[p(I64), p(I8).uext(), p(I16).sext()], mk.windows())
}

/// Sixteen integers, most of them past the registers, and three integer returns: `rcx` is
/// System V's third under LLVM's extensions; Windows has two.
pub fn ints_many(mk: &Mk) -> Family {
    let params: Vec<AbiParam> = (0..16).map(|k| if k % 3 == 1 { p(I32) } else { p(I64) }).collect();
    shape(mk, "abi/ints_many", &params, &ps(&[I64, I64, I32]), mk.windows())
}

/// Narrow integers on the stack, after the registers are used: each takes 8 bytes, but
/// Apple's AArch64 packs them at their own size.
pub fn narrow_stack(mk: &Mk) -> Family {
    let mut params = ps(&[I64; 8]);
    params.extend([p(I8), p(I16), p(I8), p(I32), p(I8).sext(), p(I16).uext(), p(I8), p(I64)]);
    shape(mk, "abi/narrow_stack", &params, &ps(&[I64, I8]), false)
}

/// `i128`s as register pairs, then on the stack once the pairs run out.
pub fn i128_pairs(mk: &Mk) -> Family {
    shape(mk, "abi/i128_pairs", &ps(&[I64, I128, I64, I128, I128, I128]), &ps(&[I128]), false)
}

/// System V's `i128` that finds one register left (`r9`): it goes on the stack and the
/// register to nobody, so the `i64` after it is on the stack too. Returns `rax:rdx` and
/// `rcx`.
pub fn i128_lone(mk: &Mk) -> Family {
    shape(mk, "abi/i128_lone", &ps(&[I64, I64, I64, I64, I128, I64]), &ps(&[I128, I64]), mk.windows())
}

/// AArch64's `i128` at an even register (AAPCS64 skips `x1`, Apple doesn't), until they
/// spill; two `i128` returns take four registers, which x86-64 doesn't have.
pub fn i128_even(mk: &Mk) -> Family {
    shape(mk, "abi/i128_even", &ps(&[I128, I64, I128, I64, I128, I128]), &ps(&[I128, I128]), mk.x64())
}

/// Floats past the eight vector registers (Windows: four, by position).
pub fn floats(mk: &Mk) -> Family {
    let params = ps(&[F32, F64, F32, F64, F64, F32, F64, F32, F64, F32, F64]);
    shape(mk, "abi/floats", &params, &ps(&[F64, F32]), false)
}

/// Integers and floats interleaved, each class running out on its own.
pub fn mixed(mk: &Mk) -> Family {
    let params = [
        p(I32),
        p(F64),
        p(I64),
        p(F32),
        p(I8).sext(),
        p(F64),
        p(I128),
        p(F32),
        p(I16).uext(),
        p(F64),
        p(I64),
        p(F32),
        p(I64),
        p(F64),
        p(I32),
        p(F32),
        p(I64),
        p(F64),
    ];
    shape(mk, "abi/mixed", &params, &ps(&[I64, F64]), false)
}

/// 128-bit vectors of each lane type, past the registers.
pub fn vectors(mk: &Mk) -> Family {
    use types::{F32X4, F64X2, I8X16, I16X8, I32X4, I64X2};
    let params = ps(&[I32X4, F64X2, I8X16, F32X4, I64X2, I16X8, I32X4, F64X2, I8X16, F32X4]);
    shape(mk, "abi/vectors", &params, &ps(&[I32X4, F64X2]), false)
}

/// `f16` and `f128`, which both architectures pass in vector registers (and on the stack
/// at their own size on Apple's AArch64).
pub fn f16_f128(mk: &Mk) -> Family {
    let params = ps(&[F16, F128, F16, F128, F16, F16, F128, F16, F16, F128, F16]);
    shape(mk, "abi/f16_f128", &params, &ps(&[F16, F128]), false)
}

pub fn rets_3(mk: &Mk) -> Family {
    shape(mk, "abi/rets_3", &ps(&[I64; 4]), &ps(&[I64; 3]), mk.windows())
}

/// Four integer returns: past x86-64's registers, within AArch64's.
pub fn rets_4(mk: &Mk) -> Family {
    shape(mk, "abi/rets_4", &ps(&[I64; 4]), &ps(&[I64; 4]), mk.x64())
}

/// Eight: all of AArch64's `x0` to `x7`.
pub fn rets_8(mk: &Mk) -> Family {
    shape(mk, "abi/rets_8", &ps(&[I64; 8]), &ps(&[I64; 8]), mk.x64())
}

/// Nine: past every convention's registers.
pub fn rets_9(mk: &Mk) -> Family {
    shape(mk, "abi/rets_9", &ps(&[I64; 9]), &ps(&[I64; 9]), true)
}

pub fn rets_floats_8(mk: &Mk) -> Family {
    shape(mk, "abi/rets_floats_8", &ps(&[F64; 8]), &ps(&[F64; 8]), mk.x64())
}

/// Two of each class: `rax`, `rdx`, `xmm0`, `xmm1` (on Windows too since Cranelift 0.137).
pub fn rets_mixed(mk: &Mk) -> Family {
    let tys = [I64, F64, I32, F32];
    shape(mk, "abi/rets_mixed", &ps(&tys), &ps(&tys), false)
}

/// Sixteen: AArch64's eight of each class, its limit of all registers together.
pub fn rets_16(mk: &Mk) -> Family {
    let mut tys = vec![I64; 8];
    tys.extend([F64; 8]);
    shape(mk, "abi/rets_16", &ps(&tys), &ps(&tys), mk.x64())
}

/// Seventeen: one past it.
pub fn rets_17(mk: &Mk) -> Family {
    let mut tys = vec![I64; 8];
    tys.extend([F64; 8]);
    tys.push(I32);
    shape(mk, "abi/rets_17", &ps(&tys), &ps(&tys), true)
}

/// `StructArgument`s, copied by the caller onto the stack among the other arguments; x86-64
/// only (Cranelift's AArch64 refuses them).
pub fn struct_args(mk: &Mk) -> Family {
    let params = [
        AbiParam::special(I64, ArgumentPurpose::StructArgument(24)),
        p(I64),
        AbiParam::special(I64, ArgumentPurpose::StructArgument(8)),
        p(F64),
        AbiParam::special(I64, ArgumentPurpose::StructArgument(40)),
        p(I32),
    ];
    shape(mk, "abi/struct_args", &params, &ps(&[I64, F64]), !mk.x64())
}

/// Where the convention extends narrow values, what a caller passes for a `uext`/`sext`
/// parameter and what a callee returns for one, seen whole: u0 calls u1 as
/// `(i8 uext, i8 sext, i16 uext, i16 sext) -> i64` and u2 as `(i64) -> i64`, which are
/// `(i64, i64, i64, i64) -> i64` and `(i64) -> i8 uext`, u3 as u2 for `(i64) -> i16 sext`.
/// AAPCS64 extends nothing, so there only the low bits are looked at.
pub fn ext_seen(mk: &Mk) -> Family {
    let whole = mk.extends();
    let narrow_args = mk.sig(
        &[p(I8).uext(), p(I8).sext(), p(I16).uext(), p(I16).sext()],
        &[p(I64)],
    );
    let wide = mk.sig_of(&[I64], &[I64]);
    let entry = mk.func(0, mk.sig_of(&[I8, I8, I16, I16, I64], &[I64, I64]), |b, v| {
        let raw = import(b, FUNCS, 1, narrow_args.clone(), false);
        let byte = import(b, FUNCS, 2, wide.clone(), false);
        let half = import(b, FUNCS, 3, wide.clone(), false);
        let r0 = call(b, raw, &v[..4])[0];
        let mut r1 = call(b, byte, &[v[4]])[0];
        let mut r2 = call(b, half, &[v[4]])[0];
        if !whole {
            r1 = b.ins().band_imm(r1, 0xff);
            r2 = b.ins().band_imm(r2, 0xffff);
        }
        let thirty_two = b.ins().iconst(I64, 32);
        let r1 = b.ins().rotl(r1, thirty_two);
        let r12 = b.ins().bxor(r1, r2);
        b.ins().return_(&[r0, r12]);
    });
    let raw = mk.func(1, mk.sig_of(&[I64; 4], &[I64]), |b, v| {
        let mut acc = b.ins().iconst(I64, 0);
        for (k, (value, mask)) in v.iter().zip([0xff, 0xff, 0xffff, 0xffff]).enumerate() {
            let value = if whole { *value } else { b.ins().band_imm(*value, mask) };
            acc = mix(b, acc, value, k as i64);
        }
        b.ins().return_(&[acc]);
    });
    let byte = mk.func(2, mk.sig(&[p(I64)], &[p(I8).uext()]), |b, v| {
        let out = b.ins().ireduce(I8, v[0]);
        b.ins().return_(&[out]);
    });
    let half = mk.func(3, mk.sig(&[p(I64)], &[p(I16).sext()]), |b, v| {
        let flipped = b.ins().bxor_imm(v[0], 0x8000);
        let out = b.ins().ireduce(I16, flipped);
        b.ins().return_(&[out]);
    });
    Family::new("abi/ext_seen", vec![entry, raw, byte, half])
}

/// A `StructReturn` first parameter, as the IR builder passes an indirect return: u0 gives
/// u1 a stack slot's address for it, then copies the slot to its own return area. Neither
/// returns a value besides: Cranelift refuses one next to a `StructReturn` parameter.
pub fn struct_return(mk: &Mk) -> Family {
    let sret = AbiParam::special(I64, ArgumentPurpose::StructReturn);
    let params = [sret, p(I64), p(F64), p(I128)];
    let callee_sig = mk.sig(&params, &[]);
    let entry = mk.func(0, mk.sig(&params, &[]), |b, v| {
        let callee = import(b, FUNCS, 1, callee_sig.clone(), false);
        let place = slot(b, STRUCT_RETURN as u32, 4);
        let at = b.ins().stack_addr(I64, place, 0);
        call(b, callee, &[at, v[1], v[2], v[3]]);
        for off in (0..STRUCT_RETURN as i32).step_by(8) {
            let word = b.ins().stack_load(I64, I64, place, off);
            b.ins().store(mem(), word, v[0], off);
        }
        b.ins().return_(&[]);
    });
    let callee = mk.func(1, callee_sig.clone(), |b, v| {
        b.ins().store(mem(), v[1], v[0], 0);
        b.ins().store(mem(), v[2], v[0], 8);
        b.ins().store(mem(), v[3], v[0], 16);
        let wide = widen(b, v[3], false);
        let tripled = b.ins().imul_imm(v[1], 3);
        let last = b.ins().bxor(tripled, wide);
        b.ins().store(mem(), last, v[0], 32);
        let bits = b.ins().bitcast(I64, ir::MemFlagsData::new(), v[2]);
        b.ins().store(mem(), bits, v[0], 40);
        b.ins().return_(&[]);
    });
    Family::new("abi/struct_return", vec![entry, callee])
}

/// The bytes of [`struct_return`]'s return area.
pub const STRUCT_RETURN: usize = 48;

#[cfg(all(unix, any(target_arch = "x86_64", target_arch = "aarch64")))]
mod run {
    use cranelift_codegen::ir::{AbiParam, ArgumentPurpose};

    use super::super::{
        Family, Mk, random,
        world::{In, Opts, check, host_mk},
    };
    use crate::tests::rng::Rng;

    /// Checks a shape family on `count` random argument lists, its data object fresh.
    fn shape(make: fn(&Mk) -> Family, seed: u64, count: usize) {
        let Some(mk) = host_mk() else { return };
        let family = make(&mk);
        assert_eq!(family.refused, Some(false), "{} is refused on the host", family.name);
        let params: Vec<AbiParam> = family.funcs[0].signature.params[1..].to_vec();
        let area = vec![0u8; super::area_len(&params)];
        let mut rng = Rng::new(seed);
        let mut structs: Vec<Vec<u8>> = Vec::new();
        for param in &params {
            if let ArgumentPurpose::StructArgument(size) = param.purpose {
                let mut bytes = vec![0u8; size as usize];
                rng.fill(&mut bytes);
                structs.push(bytes);
            }
        }
        let mut inputs = Vec::with_capacity(count);
        for _ in 0..count {
            let mut input = vec![In::Data(0)];
            let mut s = 0;
            for param in &params {
                match param.purpose {
                    ArgumentPurpose::StructArgument(_) => {
                        s += 1;
                        input.push(In::Data(s));
                    }
                    _ => input.push(In::V(random(&mut rng, param.value_type))),
                }
            }
            inputs.push(input);
        }
        let mut data: Vec<&[u8]> = vec![area.as_slice()];
        data.extend(structs.iter().map(|it| it.as_slice()));
        check(&mk, &family, &inputs, &Opts { data: &data, host: &[], forked: false, interp: false });
    }

    #[test]
    fn integers_of_each_width_and_extension() {
        shape(super::ints, 10, 24);
    }

    #[test]
    fn narrow_integers_on_the_stack() {
        shape(super::narrow_stack, 11, 24);
    }

    #[test]
    fn i128_pairs_then_the_stack() {
        shape(super::i128_pairs, 12, 24);
    }

    #[test]
    fn floats_past_the_registers() {
        shape(super::floats, 13, 24);
    }

    #[test]
    fn integers_and_floats_interleaved() {
        shape(super::mixed, 14, 24);
    }

    #[test]
    fn vectors_past_the_registers() {
        shape(super::vectors, 15, 24);
    }

    #[test]
    fn f16_and_f128() {
        shape(super::f16_f128, 16, 24);
    }

    #[test]
    fn three_integer_returns() {
        shape(super::rets_3, 17, 16);
    }

    #[test]
    fn two_returns_of_each_class() {
        shape(super::rets_mixed, 18, 16);
    }

    #[test]
    fn integers_past_the_registers() {
        shape(super::ints_many, 19, 16);
    }

    #[test]
    fn extension_is_what_cranelift_does() {
        let Some(mk) = host_mk() else { return };
        let family = super::ext_seen(&mk);
        let mut rng = Rng::new(20);
        let tys = [
            cranelift_codegen::ir::types::I8,
            cranelift_codegen::ir::types::I8,
            cranelift_codegen::ir::types::I16,
            cranelift_codegen::ir::types::I16,
            cranelift_codegen::ir::types::I64,
        ];
        let inputs: Vec<Vec<In>> =
            (0..32).map(|_| tys.iter().map(|ty| In::V(random(&mut rng, *ty))).collect()).collect();
        check(&mk, &family, &inputs, &Opts::plain());
    }

    #[test]
    fn a_struct_return_through_a_stack_slot() {
        let Some(mk) = host_mk() else { return };
        let family = super::struct_return(&mk);
        let area = [0u8; super::STRUCT_RETURN];
        let mut rng = Rng::new(21);
        let params = family.funcs[0].signature.params.clone();
        let inputs: Vec<Vec<In>> = (0..16)
            .map(|_| {
                let mut input = vec![In::Data(0)];
                input.extend(params[1..].iter().map(|param| In::V(random(&mut rng, param.value_type))));
                input
            })
            .collect();
        check(&mk, &family, &inputs, &Opts { data: &[&area[..]], host: &[], forked: false, interp: false });
    }

    /// x86-64 only: Cranelift's AArch64 has no `StructArgument`.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn struct_arguments_are_copies() {
        shape(super::struct_args, 22, 16);
    }

    /// x86-64 only: System V's `i128` with one register left.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn an_i128_with_one_register_left_goes_on_the_stack() {
        shape(super::i128_lone, 23, 24);
    }

    /// AArch64 only: an `i128` starts at an even register (AAPCS64) or the next (Apple).
    #[cfg(target_arch = "aarch64")]
    #[test]
    fn i128s_at_even_registers_and_two_returned() {
        shape(super::i128_even, 24, 24);
    }

    /// AArch64 only: eight returns of a class, and sixteen of both.
    #[cfg(target_arch = "aarch64")]
    #[test]
    fn every_return_register() {
        shape(super::rets_4, 25, 16);
        shape(super::rets_8, 26, 16);
        shape(super::rets_floats_8, 27, 16);
        shape(super::rets_16, 28, 16);
    }
}
