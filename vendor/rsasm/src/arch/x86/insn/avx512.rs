//! AVX-512: the EVEX-encoded foundation set, its 128/256-bit (VL) forms, and
//! the opmask register instructions.
//!
//! EVEX widens VEX in every direction at once: a fifth register bit for each
//! of reg, `vvvv` and a register-direct r/m (`R'`, `V'`, and `X` pressed into
//! service), `L'L` for 512 bits, `aaa` for the writemask, `z` for zeroing, and
//! `b` for broadcast — or, on register-only forms, for embedded rounding, when
//! `L'L` holds the rounding mode instead of the length.
//!
//! Every row that can take a memory operand carries a [`Tuple`], because EVEX
//! scales an 8-bit displacement by it.
//!
//! Where AVX already defines an instruction at 128 or 256 bits, the VEX row in
//! `avx.rs` is installed first and wins for ordinary operands; the EVEX rows
//! here are reached when the operands need EVEX (`xmm16` and up, a mask, a
//! broadcast). See `prefer_evex_when_required`.
//!
//! The opmask instructions (`kmovw` and friends) are VEX-encoded, but they only
//! exist for AVX-512's sake, so they live here too.

use super::avx::{split_escape, vk};
use super::{Def, EVEX_ER, EVEX_SAE, ModRm, NEEDS_MASK, NOMASK, Op, Tbl, Tuple, Vk, add, d};

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// An EVEX row, written with legacy opcode bytes like the VEX rows are.
pub(crate) fn ev(ops: Vec<Op>, pfx: u8, esc: &[u8], vlen: u16, w: bool, tuple: Tuple) -> Def {
    let (map, op) = split_escape(esc);
    d(ops, &[op], ModRm::Reg, if w { 64 } else { 0 })
        .pfx(pfx)
        .map(map)
        .evex(vlen, tuple)
}

pub(crate) const ALL: [u16; 3] = [128, 256, 512];

/// Half the register class, for the widening and narrowing conversions.
pub(crate) fn half(vlen: u16) -> Vk {
    match vlen {
        512 => Vk::Ymm,
        _ => Vk::Xmm,
    }
}

pub(crate) fn nds(k: Vk) -> Vec<Op> {
    vec![Op::V(k), Op::Nds(k), Op::Vm(k, 0)]
}

pub(crate) fn rm(k: Vk) -> Vec<Op> {
    vec![Op::V(k), Op::Vm(k, 0)]
}

pub(crate) fn mr(k: Vk) -> Vec<Op> {
    vec![Op::Vm(k, 0), Op::V(k)]
}

pub(crate) fn with_imm(mut ops: Vec<Op>) -> Vec<Op> {
    ops.push(Op::Imm(1));
    ops
}

/// Rounding control only exists where the register holds a full 512-bit
/// vector or a single scalar; a 128- or 256-bit packed form has no spare
/// `L'L` to put the mode in.
pub(crate) fn rounding_flag(def: Def, flag: u32, vlen: u16) -> Def {
    if vlen == 512 { def.flags(flag) } else { def }
}

fn install_float(t: &mut Tbl) {
    // (stem, opcode, rounding flag for the 512-bit and scalar forms)
    #[rustfmt::skip]
    const OPS: &[(&str, u8, u32)] = &[
        ("add", 0x58, EVEX_ER), ("mul", 0x59, EVEX_ER),
        ("sub", 0x5c, EVEX_ER), ("div", 0x5e, EVEX_ER),
        ("min", 0x5d, EVEX_SAE), ("max", 0x5f, EVEX_SAE),
    ];
    for &(stem, op, flag) in OPS {
        for (flav, pfx, w) in [("ps", 0x00u8, false), ("pd", 0x66, true)] {
            let mnem = leak(format!("v{stem}{flav}"));
            for l in ALL {
                let def = ev(nds(vk(l)), pfx, &[0x0f, op], l, w, Tuple::Fv);
                add(t, mnem, vec![rounding_flag(def, flag, l)]);
            }
        }
        for (flav, pfx, w) in [("ss", 0xf3u8, false), ("sd", 0xf2, true)] {
            let mnem = leak(format!("v{stem}{flav}"));
            let memw = if w { 8 } else { 4 };
            add(
                t,
                mnem,
                vec![
                    ev(
                        vec![Op::V(Vk::Xmm), Op::Nds(Vk::Xmm), Op::Vm(Vk::Xmm, memw)],
                        pfx,
                        &[0x0f, op],
                        128,
                        w,
                        Tuple::T1s,
                    )
                    .flags(flag),
                ],
            );
        }
    }
    for (flav, pfx, w) in [("ps", 0x00u8, false), ("pd", 0x66, true)] {
        let mnem = leak(format!("vsqrt{flav}"));
        for l in ALL {
            let def = ev(rm(vk(l)), pfx, &[0x0f, 0x51], l, w, Tuple::Fv);
            add(t, mnem, vec![rounding_flag(def, EVEX_ER, l)]);
        }
    }
    for (flav, pfx, w, memw) in [("ss", 0xf3u8, false, 4u8), ("sd", 0xf2, true, 8)] {
        add(
            t,
            leak(format!("vsqrt{flav}")),
            vec![
                ev(
                    vec![Op::V(Vk::Xmm), Op::Nds(Vk::Xmm), Op::Vm(Vk::Xmm, memw)],
                    pfx,
                    &[0x0f, 0x51],
                    128,
                    w,
                    Tuple::T1s,
                )
                .flags(EVEX_ER),
            ],
        );
    }

    // Compares write a mask register rather than a vector.
    for (flav, pfx, w) in [("ps", 0x00u8, false), ("pd", 0x66, true)] {
        let mnem = leak(format!("vcmp{flav}"));
        for l in ALL {
            let k = vk(l);
            let def = ev(
                vec![Op::V(Vk::K), Op::Nds(k), Op::Vm(k, 0), Op::Imm(1)],
                pfx,
                &[0x0f, 0xc2],
                l,
                w,
                Tuple::Fv,
            );
            add(t, mnem, vec![rounding_flag(def, EVEX_SAE, l)]);
        }
    }
    for (flav, pfx, w, memw) in [("ss", 0xf3u8, false, 4u8), ("sd", 0xf2, true, 8)] {
        add(
            t,
            leak(format!("vcmp{flav}")),
            vec![
                ev(
                    vec![
                        Op::V(Vk::K),
                        Op::Nds(Vk::Xmm),
                        Op::Vm(Vk::Xmm, memw),
                        Op::Imm(1),
                    ],
                    pfx,
                    &[0x0f, 0xc2],
                    128,
                    w,
                    Tuple::T1s,
                )
                .flags(EVEX_SAE),
            ],
        );
    }
    for (mnem, pfx, op, w) in [
        ("vucomiss", 0x00u8, 0x2eu8, false),
        ("vucomisd", 0x66, 0x2e, true),
        ("vcomiss", 0x00, 0x2f, false),
        ("vcomisd", 0x66, 0x2f, true),
    ] {
        let memw = if w { 8 } else { 4 };
        add(
            t,
            mnem,
            vec![
                ev(
                    vec![Op::V(Vk::Xmm), Op::Vm(Vk::Xmm, memw)],
                    pfx,
                    &[0x0f, op],
                    128,
                    w,
                    Tuple::T1s,
                )
                .flags(EVEX_SAE | NOMASK),
            ],
        );
    }

    for (mnem, pfx, op, w) in [
        ("vunpcklps", 0x00u8, 0x14u8, false),
        ("vunpcklpd", 0x66, 0x14, true),
        ("vunpckhps", 0x00, 0x15, false),
        ("vunpckhpd", 0x66, 0x15, true),
        ("vandps", 0x00, 0x54, false),
        ("vandpd", 0x66, 0x54, true),
        ("vandnps", 0x00, 0x55, false),
        ("vandnpd", 0x66, 0x55, true),
        ("vorps", 0x00, 0x56, false),
        ("vorpd", 0x66, 0x56, true),
        ("vxorps", 0x00, 0x57, false),
        ("vxorpd", 0x66, 0x57, true),
    ] {
        for l in ALL {
            add(
                t,
                mnem,
                vec![ev(nds(vk(l)), pfx, &[0x0f, op], l, w, Tuple::Fv)],
            );
        }
    }
    for (mnem, pfx, w) in [("vshufps", 0x00u8, false), ("vshufpd", 0x66, true)] {
        for l in ALL {
            add(
                t,
                mnem,
                vec![ev(
                    with_imm(nds(vk(l))),
                    pfx,
                    &[0x0f, 0xc6],
                    l,
                    w,
                    Tuple::Fv,
                )],
            );
        }
    }
}

fn install_moves(t: &mut Tbl) {
    // Every full-width move comes in a load and a store form. Unlike VEX there
    // is no shorter prefix to reach for, so the load form is always used for
    // register-to-register moves.
    for (mnem, pfx, load, store, w) in [
        ("vmovaps", 0x00u8, 0x28u8, 0x29u8, false),
        ("vmovapd", 0x66, 0x28, 0x29, true),
        ("vmovups", 0x00, 0x10, 0x11, false),
        ("vmovupd", 0x66, 0x10, 0x11, true),
        ("vmovdqa32", 0x66, 0x6f, 0x7f, false),
        ("vmovdqa64", 0x66, 0x6f, 0x7f, true),
        ("vmovdqu32", 0xf3, 0x6f, 0x7f, false),
        ("vmovdqu64", 0xf3, 0x6f, 0x7f, true),
        ("vmovdqu8", 0xf2, 0x6f, 0x7f, false),
        ("vmovdqu16", 0xf2, 0x6f, 0x7f, true),
    ] {
        for l in ALL {
            add(
                t,
                mnem,
                vec![
                    ev(rm(vk(l)), pfx, &[0x0f, load], l, w, Tuple::Fvm),
                    ev(mr(vk(l)), pfx, &[0x0f, store], l, w, Tuple::Fvm),
                ],
            );
        }
    }
    for (mnem, pfx, w) in [("vmovntps", 0x00u8, false), ("vmovntpd", 0x66, true)] {
        for l in ALL {
            add(
                t,
                mnem,
                vec![
                    ev(
                        vec![Op::M((l / 8) as u8), Op::V(vk(l))],
                        pfx,
                        &[0x0f, 0x2b],
                        l,
                        w,
                        Tuple::Fvm,
                    )
                    .flags(NOMASK),
                ],
            );
        }
    }
    for l in ALL {
        add(
            t,
            "vmovntdq",
            vec![
                ev(
                    vec![Op::M((l / 8) as u8), Op::V(vk(l))],
                    0x66,
                    &[0x0f, 0xe7],
                    l,
                    false,
                    Tuple::Fvm,
                )
                .flags(NOMASK),
            ],
        );
    }

    for (mnem, pfx, w, memw) in [("vmovss", 0xf3u8, false, 4u8), ("vmovsd", 0xf2, true, 8)] {
        add(
            t,
            mnem,
            vec![
                ev(
                    vec![Op::V(Vk::Xmm), Op::M(memw)],
                    pfx,
                    &[0x0f, 0x10],
                    128,
                    w,
                    Tuple::T1s,
                ),
                ev(
                    vec![Op::M(memw), Op::V(Vk::Xmm)],
                    pfx,
                    &[0x0f, 0x11],
                    128,
                    w,
                    Tuple::T1s,
                )
                .flags(NOMASK),
                ev(
                    vec![Op::V(Vk::Xmm), Op::Nds(Vk::Xmm), Op::V(Vk::Xmm)],
                    pfx,
                    &[0x0f, 0x10],
                    128,
                    w,
                    Tuple::None,
                ),
            ],
        );
    }
    add(
        t,
        "vmovddup",
        vec![
            ev(
                vec![Op::V(Vk::Xmm), Op::Vm(Vk::Xmm, 8)],
                0xf2,
                &[0x0f, 0x12],
                128,
                true,
                Tuple::Dup,
            ),
            ev(rm(Vk::Ymm), 0xf2, &[0x0f, 0x12], 256, true, Tuple::Dup),
            ev(rm(Vk::Zmm), 0xf2, &[0x0f, 0x12], 512, true, Tuple::Dup),
        ],
    );
    for (mnem, op) in [("vmovsldup", 0x12u8), ("vmovshdup", 0x16)] {
        for l in ALL {
            add(
                t,
                mnem,
                vec![ev(rm(vk(l)), 0xf3, &[0x0f, op], l, false, Tuple::Fvm)],
            );
        }
    }
    // `vmovd`/`vmovq` reach `xmm16`-`xmm31` only through these.
    add(
        t,
        "vmovd",
        vec![
            ev(
                vec![Op::V(Vk::Xmm), Op::Rm(4)],
                0x66,
                &[0x0f, 0x6e],
                128,
                false,
                Tuple::T1s,
            )
            .flags(NOMASK),
            ev(
                vec![Op::Rm(4), Op::V(Vk::Xmm)],
                0x66,
                &[0x0f, 0x7e],
                128,
                false,
                Tuple::T1s,
            )
            .flags(NOMASK),
        ],
    );
    add(
        t,
        "vmovq",
        vec![
            ev(
                vec![Op::V(Vk::Xmm), Op::Vm(Vk::Xmm, 8)],
                0xf3,
                &[0x0f, 0x7e],
                128,
                true,
                Tuple::T1s,
            )
            .flags(NOMASK),
            ev(
                vec![Op::Vm(Vk::Xmm, 8), Op::V(Vk::Xmm)],
                0x66,
                &[0x0f, 0xd6],
                128,
                true,
                Tuple::T1s,
            )
            .flags(NOMASK),
            ev(
                vec![Op::V(Vk::Xmm), Op::Rm(8)],
                0x66,
                &[0x0f, 0x6e],
                128,
                true,
                Tuple::T1s,
            )
            .flags(NOMASK),
            ev(
                vec![Op::Rm(8), Op::V(Vk::Xmm)],
                0x66,
                &[0x0f, 0x7e],
                128,
                true,
                Tuple::T1s,
            )
            .flags(NOMASK),
        ],
    );
}

fn install_integer(t: &mut Tbl) {
    // Dword/qword packed operations: `W` picks the element, and both
    // broadcast from a single element.
    #[rustfmt::skip]
    const DQ: &[(&str, &str, u8, &[u8])] = &[
        // (dword name, qword name, prefix, opcode bytes)
        ("vpaddd", "vpaddq", 0x66, &[0x0f, 0xfe]),
        ("vpsubd", "vpsubq", 0x66, &[0x0f, 0xfa]),
        ("vpandd", "vpandq", 0x66, &[0x0f, 0xdb]),
        ("vpandnd", "vpandnq", 0x66, &[0x0f, 0xdf]),
        ("vpord", "vporq", 0x66, &[0x0f, 0xeb]),
        ("vpxord", "vpxorq", 0x66, &[0x0f, 0xef]),
        ("vpminsd", "vpminsq", 0x66, &[0x0f, 0x38, 0x39]),
        ("vpminud", "vpminuq", 0x66, &[0x0f, 0x38, 0x3b]),
        ("vpmaxsd", "vpmaxsq", 0x66, &[0x0f, 0x38, 0x3d]),
        ("vpmaxud", "vpmaxuq", 0x66, &[0x0f, 0x38, 0x3f]),
        ("vpermi2d", "vpermi2q", 0x66, &[0x0f, 0x38, 0x76]),
        ("vpermi2ps", "vpermi2pd", 0x66, &[0x0f, 0x38, 0x77]),
        ("vpermt2d", "vpermt2q", 0x66, &[0x0f, 0x38, 0x7e]),
        ("vpermt2ps", "vpermt2pd", 0x66, &[0x0f, 0x38, 0x7f]),
        ("vpsllvd", "vpsllvq", 0x66, &[0x0f, 0x38, 0x47]),
        ("vpsrlvd", "vpsrlvq", 0x66, &[0x0f, 0x38, 0x45]),
        ("vpsravd", "vpsravq", 0x66, &[0x0f, 0x38, 0x46]),
        ("vprolvd", "vprolvq", 0x66, &[0x0f, 0x38, 0x15]),
        ("vprorvd", "vprorvq", 0x66, &[0x0f, 0x38, 0x14]),
        ("vpunpckldq", "vpunpcklqdq", 0x66, &[0x0f, 0x62]),
        ("vpunpckhdq", "vpunpckhqdq", 0x66, &[0x0f, 0x6a]),
    ];
    for &(dname, qname, pfx, esc) in DQ {
        // Where SSE2 already had separate dword and qword opcodes, EVEX keeps
        // them; only the bitwise group and the AVX-512 newcomers share one
        // opcode and let `W` tell the element sizes apart.
        let qesc: &[u8] = match qname {
            "vpaddq" => &[0x0f, 0xd4],
            "vpsubq" => &[0x0f, 0xfb],
            "vpunpcklqdq" => &[0x0f, 0x6c],
            "vpunpckhqdq" => &[0x0f, 0x6d],
            _ => esc,
        };
        for l in ALL {
            add(
                t,
                dname,
                vec![ev(nds(vk(l)), pfx, esc, l, false, Tuple::Fv)],
            );
            add(
                t,
                qname,
                vec![ev(nds(vk(l)), pfx, qesc, l, true, Tuple::Fv)],
            );
        }
    }
    // A few dword and qword operations whose opcodes are not shared.
    for (mnem, esc, w) in [
        ("vpmulld", &[0x0fu8, 0x38, 0x40] as &[u8], false),
        ("vpmullq", &[0x0f, 0x38, 0x40], true),
        ("vpmuludq", &[0x0f, 0xf4], true),
        ("vpmuldq", &[0x0f, 0x38, 0x28], true),
        ("vpermd", &[0x0f, 0x38, 0x36], false),
        ("vpermq", &[0x0f, 0x38, 0x36], true),
        ("vpermps", &[0x0f, 0x38, 0x16], false),
        ("vpermpd", &[0x0f, 0x38, 0x16], true),
        ("vpermilps", &[0x0f, 0x38, 0x0c], false),
        ("vpermilpd", &[0x0f, 0x38, 0x0d], true),
    ] {
        // `vpermd` and friends index across the whole register, which a
        // 128-bit register is too small for.
        let lens: &[u16] = if mnem.starts_with("vperm") && !mnem.starts_with("vpermil") {
            &[256, 512]
        } else {
            &ALL
        };
        for &l in lens {
            add(t, mnem, vec![ev(nds(vk(l)), 0x66, esc, l, w, Tuple::Fv)]);
        }
    }
    // Byte and word packed operations have no broadcast, and so a plain
    // full-vector-memory tuple.
    for (mnem, op) in [
        ("vpaddb", 0xfcu8),
        ("vpaddw", 0xfd),
        ("vpsubb", 0xf8),
        ("vpsubw", 0xf9),
        ("vpaddsb", 0xec),
        ("vpaddsw", 0xed),
        ("vpaddusb", 0xdc),
        ("vpaddusw", 0xdd),
        ("vpavgb", 0xe0),
        ("vpavgw", 0xe3),
        ("vpmullw", 0xd5),
        ("vpmulhw", 0xe5),
    ] {
        for l in ALL {
            add(
                t,
                mnem,
                vec![ev(nds(vk(l)), 0x66, &[0x0f, op], l, false, Tuple::Fvm)],
            );
        }
    }

    // Immediate permutes and rotates.
    for (mnem, esc, w, lens) in [
        (
            "vpermq",
            &[0x0fu8, 0x3a, 0x00] as &[u8],
            true,
            &[256u16, 512] as &[u16],
        ),
        ("vpermpd", &[0x0f, 0x3a, 0x01], true, &[256, 512]),
        ("vpermilps", &[0x0f, 0x3a, 0x04], false, &ALL),
        ("vpermilpd", &[0x0f, 0x3a, 0x05], true, &ALL),
    ] {
        for &l in lens {
            add(
                t,
                mnem,
                vec![ev(with_imm(rm(vk(l))), 0x66, esc, l, w, Tuple::Fv)],
            );
        }
    }
    add_pshufd(t);

    // Shift and rotate by immediate: `vvvv` is the destination.
    for (mnem, op, ext, w) in [
        ("vpslld", 0x72u8, 6u8, false),
        ("vpsrld", 0x72, 2, false),
        ("vpsrad", 0x72, 4, false),
        ("vpsraq", 0x72, 4, true),
        ("vprold", 0x72, 1, false),
        ("vprolq", 0x72, 1, true),
        ("vprord", 0x72, 0, false),
        ("vprorq", 0x72, 0, true),
        ("vpsllq", 0x73, 6, true),
        ("vpsrlq", 0x73, 2, true),
    ] {
        for l in ALL {
            let k = vk(l);
            add(
                t,
                mnem,
                vec![
                    d(
                        vec![Op::Nds(k), Op::Vm(k, 0), Op::Imm(1)],
                        &[op],
                        ModRm::Ext(ext),
                        if w { 64 } else { 0 },
                    )
                    .pfx(0x66)
                    .map(1)
                    .evex(l, Tuple::Fv),
                ],
            );
        }
    }

    for (mnem, w) in [("vpternlogd", false), ("vpternlogq", true)] {
        for l in ALL {
            add(
                t,
                mnem,
                vec![ev(
                    with_imm(nds(vk(l))),
                    0x66,
                    &[0x0f, 0x3a, 0x25],
                    l,
                    w,
                    Tuple::Fv,
                )],
            );
        }
    }
    for (mnem, op, w) in [("vpabsd", 0x1eu8, false), ("vpabsq", 0x1f, true)] {
        for l in ALL {
            add(
                t,
                mnem,
                vec![ev(rm(vk(l)), 0x66, &[0x0f, 0x38, op], l, w, Tuple::Fv)],
            );
        }
    }

    // Integer compares into a mask register.
    for (mnem, esc, w, tuple) in [
        ("vpcmpeqd", &[0x0fu8, 0x76] as &[u8], false, Tuple::Fv),
        ("vpcmpgtd", &[0x0f, 0x66], false, Tuple::Fv),
        ("vpcmpeqq", &[0x0f, 0x38, 0x29], true, Tuple::Fv),
        ("vpcmpgtq", &[0x0f, 0x38, 0x37], true, Tuple::Fv),
        ("vpcmpeqb", &[0x0f, 0x74], false, Tuple::Fvm),
        ("vpcmpeqw", &[0x0f, 0x75], false, Tuple::Fvm),
        ("vpcmpgtb", &[0x0f, 0x64], false, Tuple::Fvm),
        ("vpcmpgtw", &[0x0f, 0x65], false, Tuple::Fvm),
    ] {
        for l in ALL {
            let k = vk(l);
            add(
                t,
                mnem,
                vec![ev(
                    vec![Op::V(Vk::K), Op::Nds(k), Op::Vm(k, 0)],
                    0x66,
                    esc,
                    l,
                    w,
                    tuple,
                )],
            );
        }
    }
    for (mnem, op, w) in [
        ("vpcmpd", 0x1fu8, false),
        ("vpcmpud", 0x1e, false),
        ("vpcmpq", 0x1f, true),
        ("vpcmpuq", 0x1e, true),
    ] {
        for l in ALL {
            let k = vk(l);
            add(
                t,
                mnem,
                vec![ev(
                    vec![Op::V(Vk::K), Op::Nds(k), Op::Vm(k, 0), Op::Imm(1)],
                    0x66,
                    &[0x0f, 0x3a, op],
                    l,
                    w,
                    Tuple::Fv,
                )],
            );
        }
    }
    for (mnem, pfx, w) in [
        ("vptestmd", 0x66u8, false),
        ("vptestmq", 0x66, true),
        ("vptestnmd", 0xf3, false),
        ("vptestnmq", 0xf3, true),
    ] {
        for l in ALL {
            let k = vk(l);
            add(
                t,
                mnem,
                vec![ev(
                    vec![Op::V(Vk::K), Op::Nds(k), Op::Vm(k, 0)],
                    pfx,
                    &[0x0f, 0x38, 0x27],
                    l,
                    w,
                    Tuple::Fv,
                )],
            );
        }
    }
}

fn add_pshufd(t: &mut Tbl) {
    for l in ALL {
        add(
            t,
            "vpshufd",
            vec![ev(
                with_imm(rm(vk(l))),
                0x66,
                &[0x0f, 0x70],
                l,
                false,
                Tuple::Fv,
            )],
        );
    }
}

fn install_broadcasts(t: &mut Tbl) {
    // From a scalar, whose tuple is its own element size.
    for (mnem, op, memw, w, tuple, lens) in [
        (
            "vpbroadcastb",
            0x78u8,
            1u8,
            false,
            Tuple::T1s8,
            &ALL as &[u16],
        ),
        ("vpbroadcastw", 0x79, 2, false, Tuple::T1s16, &ALL),
        ("vpbroadcastd", 0x58, 4, false, Tuple::T1s, &ALL),
        ("vpbroadcastq", 0x59, 8, true, Tuple::T1s, &ALL),
        ("vbroadcastss", 0x18, 4, false, Tuple::T1s, &ALL),
        ("vbroadcastsd", 0x19, 8, true, Tuple::T1s, &[256, 512]),
    ] {
        for &l in lens {
            add(
                t,
                mnem,
                vec![ev(
                    vec![Op::V(vk(l)), Op::Vm(Vk::Xmm, memw)],
                    0x66,
                    &[0x0f, 0x38, op],
                    l,
                    w,
                    tuple,
                )],
            );
        }
    }
    // From a general-purpose register, which only EVEX can do.
    for (mnem, op, gpr, w) in [
        ("vpbroadcastb", 0x7au8, 4u8, false),
        ("vpbroadcastw", 0x7b, 4, false),
        ("vpbroadcastd", 0x7c, 4, false),
        ("vpbroadcastq", 0x7c, 8, true),
    ] {
        for l in ALL {
            add(
                t,
                mnem,
                vec![ev(
                    vec![Op::V(vk(l)), Op::R(gpr)],
                    0x66,
                    &[0x0f, 0x38, op],
                    l,
                    w,
                    Tuple::None,
                )],
            );
        }
    }
    // Sub-vector broadcasts.
    for (mnem, op, sub, w, lens) in [
        (
            "vbroadcastf32x4",
            0x1au8,
            Vk::Xmm,
            false,
            &[256u16, 512] as &[u16],
        ),
        ("vbroadcasti32x4", 0x5a, Vk::Xmm, false, &[256, 512]),
        ("vbroadcastf64x4", 0x1b, Vk::Ymm, true, &[512]),
        ("vbroadcasti64x4", 0x5b, Vk::Ymm, true, &[512]),
    ] {
        for &l in lens {
            add(
                t,
                mnem,
                vec![ev(
                    vec![Op::V(vk(l)), Op::M(sub.width())],
                    0x66,
                    &[0x0f, 0x38, op],
                    l,
                    w,
                    Tuple::T4,
                )],
            );
        }
    }

    // 256-bit halves of a 512-bit register, and 128-bit quarters.
    for (stem, fop, iop, sub, w) in [
        ("32x4", 0x18u8, 0x38u8, Vk::Xmm, false),
        ("64x4", 0x1a, 0x3a, Vk::Ymm, true),
    ] {
        let lens: &[u16] = if sub == Vk::Xmm { &[256, 512] } else { &[512] };
        for (kind, op) in [("f", fop), ("i", iop)] {
            let ins = leak(format!("vinsert{kind}{stem}"));
            let ext = leak(format!("vextract{kind}{stem}"));
            for &l in lens {
                add(
                    t,
                    ins,
                    vec![ev(
                        vec![Op::V(vk(l)), Op::Nds(vk(l)), Op::Vm(sub, 0), Op::Imm(1)],
                        0x66,
                        &[0x0f, 0x3a, op],
                        l,
                        w,
                        Tuple::T4,
                    )],
                );
                add(
                    t,
                    ext,
                    vec![ev(
                        vec![Op::Vm(sub, 0), Op::V(vk(l)), Op::Imm(1)],
                        0x66,
                        &[0x0f, 0x3a, op + 1],
                        l,
                        w,
                        Tuple::T4,
                    )],
                );
            }
        }
    }
}

fn install_conversions(t: &mut Tbl) {
    for (mnem, pfx, flag) in [
        ("vcvtdq2ps", 0x00u8, EVEX_ER),
        ("vcvtps2dq", 0x66, EVEX_ER),
        ("vcvttps2dq", 0xf3, EVEX_SAE),
    ] {
        for l in ALL {
            let def = ev(rm(vk(l)), pfx, &[0x0f, 0x5b], l, false, Tuple::Fv);
            add(t, mnem, vec![rounding_flag(def, flag, l)]);
        }
    }
    // Widening: a half-register source.
    for (mnem, pfx, op, flag) in [
        ("vcvtdq2pd", 0xf3u8, 0xe6u8, 0u32),
        ("vcvtps2pd", 0x00, 0x5a, EVEX_SAE),
    ] {
        for l in ALL {
            let def = ev(
                vec![Op::V(vk(l)), Op::Vm(half(l), (l / 16) as u8)],
                pfx,
                &[0x0f, op],
                l,
                false,
                Tuple::Hv,
            );
            add(t, mnem, vec![rounding_flag(def, flag, l)]);
        }
    }
    // Narrowing: a half-register destination.
    for (mnem, pfx, op, flag) in [
        ("vcvtpd2ps", 0x66u8, 0x5au8, EVEX_ER),
        ("vcvtpd2dq", 0xf2, 0xe6, EVEX_ER),
        ("vcvttpd2dq", 0x66, 0xe6, EVEX_SAE),
    ] {
        for l in ALL {
            let def = ev(
                vec![Op::V(half(l)), Op::Vm(vk(l), 0)],
                pfx,
                &[0x0f, op],
                l,
                true,
                Tuple::Fv,
            );
            add(t, mnem, vec![rounding_flag(def, flag, l)]);
        }
    }
    for (mnem, pfx) in [("vcvtsi2ss", 0xf3u8), ("vcvtsi2sd", 0xf2)] {
        for (w, opsize) in [(4u8, 32u8), (8, 64)] {
            let mut def = ev(
                vec![Op::V(Vk::Xmm), Op::Nds(Vk::Xmm), Op::Rm(w)],
                pfx,
                &[0x0f, 0x2a],
                128,
                w == 8,
                Tuple::T1s,
            )
            .flags(EVEX_ER | NOMASK);
            def.opsize = opsize;
            add(t, mnem, vec![def]);
        }
    }
    for (mnem, pfx, memw) in [("vcvtss2sd", 0xf3u8, 4u8), ("vcvtsd2ss", 0xf2, 8)] {
        add(
            t,
            mnem,
            vec![
                ev(
                    vec![Op::V(Vk::Xmm), Op::Nds(Vk::Xmm), Op::Vm(Vk::Xmm, memw)],
                    pfx,
                    &[0x0f, 0x5a],
                    128,
                    memw == 8,
                    Tuple::T1s,
                )
                .flags(if memw == 4 { EVEX_SAE } else { EVEX_ER }),
            ],
        );
    }

    // Truncating moves to a narrower element: the destination is a fraction
    // of the source register, which is what the Half/Quarter/Eighth tuples
    // describe.
    for (stem, op, tuple, dst) in [
        ("vpmovqd", 0x35u8, Tuple::Hvm, 2u16),
        ("vpmovdw", 0x33, Tuple::Hvm, 2),
        ("vpmovdb", 0x31, Tuple::Qvm, 4),
        ("vpmovqw", 0x34, Tuple::Qvm, 4),
        ("vpmovqb", 0x32, Tuple::Ovm, 8),
    ] {
        for l in ALL {
            let dst_bytes = (l / 8 / dst) as u8;
            // The register destination is the smallest class that holds it.
            let dk = if dst_bytes > 16 { Vk::Ymm } else { Vk::Xmm };
            add(
                t,
                stem,
                vec![ev(
                    vec![Op::Vm(dk, dst_bytes), Op::V(vk(l))],
                    0xf3,
                    &[0x0f, 0x38, op],
                    l,
                    false,
                    tuple,
                )],
            );
        }
    }
    // And the widening loads, which are the same shapes the other way round.
    #[rustfmt::skip]
    const PMOVX: &[(&str, u8, Tuple, u16)] = &[
        ("vpmovzxbd", 0x31, Tuple::Qvm, 4), ("vpmovsxbd", 0x21, Tuple::Qvm, 4),
        ("vpmovzxbq", 0x32, Tuple::Ovm, 8), ("vpmovsxbq", 0x22, Tuple::Ovm, 8),
        ("vpmovzxwd", 0x33, Tuple::Hvm, 2), ("vpmovsxwd", 0x23, Tuple::Hvm, 2),
        ("vpmovzxwq", 0x34, Tuple::Qvm, 4), ("vpmovsxwq", 0x24, Tuple::Qvm, 4),
        ("vpmovzxdq", 0x35, Tuple::Hvm, 2), ("vpmovsxdq", 0x25, Tuple::Hvm, 2),
    ];
    for &(mnem, op, tuple, frac) in PMOVX {
        for l in ALL {
            let src_bytes = (l / 8 / frac) as u8;
            let sk = if src_bytes > 16 { Vk::Ymm } else { Vk::Xmm };
            add(
                t,
                mnem,
                vec![ev(
                    vec![Op::V(vk(l)), Op::Vm(sk, src_bytes)],
                    0x66,
                    &[0x0f, 0x38, op],
                    l,
                    false,
                    tuple,
                )],
            );
        }
    }
}

fn install_gather_scatter(t: &mut Tbl) {
    // (gather, scatter, gather opcode, qword index, qword element)
    for (gather, scatter, op, qidx, w) in [
        ("vgatherdps", "vscatterdps", 0x92u8, false, false),
        ("vgatherqps", "vscatterqps", 0x93, true, false),
        ("vgatherdpd", "vscatterdpd", 0x92, false, true),
        ("vgatherqpd", "vscatterqpd", 0x93, true, true),
        ("vpgatherdd", "vpscatterdd", 0x90, false, false),
        ("vpgatherqd", "vpscatterqd", 0x91, true, false),
        ("vpgatherdq", "vpscatterdq", 0x90, false, true),
        ("vpgatherqq", "vpscatterqq", 0x91, true, true),
    ] {
        for l in ALL {
            // As with VEX, element and index counts differ when one is a
            // dword and the other a qword: the narrower of the two needs a
            // register half the size.
            let (data, index) = match (qidx, w) {
                (false, false) | (true, true) => (vk(l), vk(l)),
                (true, false) => (half(l), vk(l)),
                (false, true) => (vk(l), half(l)),
            };
            add(
                t,
                gather,
                vec![
                    ev(
                        vec![Op::V(data), Op::Vsib(index)],
                        0x66,
                        &[0x0f, 0x38, op],
                        l,
                        w,
                        Tuple::T1s,
                    )
                    .flags(NEEDS_MASK),
                ],
            );
            add(
                t,
                scatter,
                vec![
                    ev(
                        vec![Op::Vsib(index), Op::V(data)],
                        0x66,
                        &[0x0f, 0x38, op + 0x10],
                        l,
                        w,
                        Tuple::T1s,
                    )
                    .flags(NEEDS_MASK),
                ],
            );
        }
    }
}

fn install_mask_ops(t: &mut Tbl) {
    // The width suffix picks `W` and `pp` together: `b` is `66 W0`, `w` is no
    // prefix and `W0`, `d` is `66 W1` and `q` is no prefix and `W1`.
    for (sfx, pfx, w, width) in [
        ("b", 0x66u8, false, 1u8),
        ("w", 0x00, false, 2),
        ("d", 0x66, true, 4),
        ("q", 0x00, true, 8),
    ] {
        let vexk = |ops: Vec<Op>, pfx: u8, op: u8, l: u16, w: bool| {
            d(ops, &[op], ModRm::Reg, if w { 64 } else { 0 })
                .pfx(pfx)
                .map(1)
                .vex(l)
        };
        // Loading from and storing to a GPR use `F2` for dword and qword
        // masks, because `66` was already taken by the byte form.
        let (gpfx, gw, gpr) = match sfx {
            "b" => (0x66u8, false, 4u8),
            "w" => (0x00, false, 4),
            "d" => (0xf2, false, 4),
            _ => (0xf2, true, 8),
        };
        add(
            t,
            leak(format!("kmov{sfx}")),
            vec![
                vexk(vec![Op::V(Vk::K), Op::Vm(Vk::K, width)], pfx, 0x90, 128, w),
                vexk(vec![Op::M(width), Op::V(Vk::K)], pfx, 0x91, 128, w),
                vexk(vec![Op::V(Vk::K), Op::R(gpr)], gpfx, 0x92, 128, gw),
                vexk(vec![Op::R(gpr), Op::V(Vk::K)], gpfx, 0x93, 128, gw),
            ],
        );
        // Two-source logic, with the result in reg and the sources in `vvvv`
        // and r/m. The prefix is a VEX.L1 form even though no vector is
        // involved: `L` is part of what distinguishes these opcodes.
        for (stem, op) in [
            ("and", 0x41u8),
            ("andn", 0x42),
            ("or", 0x45),
            ("xnor", 0x46),
            ("xor", 0x47),
            ("add", 0x4a),
        ] {
            add(
                t,
                leak(format!("k{stem}{sfx}")),
                vec![vexk(
                    vec![Op::V(Vk::K), Op::Nds(Vk::K), Op::V(Vk::K)],
                    pfx,
                    op,
                    256,
                    w,
                )],
            );
        }
        for (stem, op) in [("not", 0x44u8), ("ortest", 0x98), ("test", 0x99)] {
            add(
                t,
                leak(format!("k{stem}{sfx}")),
                vec![vexk(vec![Op::V(Vk::K), Op::V(Vk::K)], pfx, op, 128, w)],
            );
        }
        // Shifts take their count as an immediate on the `0F 3A` map, with the
        // byte/word and dword/qword pairs sharing one opcode and split by `W`.
        let narrow = matches!(sfx, "b" | "w");
        for (stem, op) in [("shiftr", 0x30u8), ("shiftl", 0x32)] {
            add(
                t,
                leak(format!("k{stem}{sfx}")),
                vec![
                    d(
                        vec![Op::V(Vk::K), Op::V(Vk::K), Op::Imm(1)],
                        &[if narrow { op } else { op + 1 }],
                        ModRm::Reg,
                        if matches!(sfx, "w" | "q") { 64 } else { 0 },
                    )
                    .pfx(0x66)
                    .map(3)
                    .vex(128),
                ],
            );
        }
    }
    for (mnem, pfx, w) in [
        ("kunpckbw", 0x66u8, false),
        ("kunpckwd", 0x00, false),
        ("kunpckdq", 0x00, true),
    ] {
        add(
            t,
            mnem,
            vec![
                d(
                    vec![Op::V(Vk::K), Op::Nds(Vk::K), Op::V(Vk::K)],
                    &[0x4b],
                    ModRm::Reg,
                    if w { 64 } else { 0 },
                )
                .pfx(pfx)
                .map(1)
                .vex(256),
            ],
        );
    }
}

pub fn install(t: &mut Tbl) {
    install_float(t);
    install_moves(t);
    install_integer(t);
    install_broadcasts(t);
    install_conversions(t);
    install_gather_scatter(t);
    install_mask_ops(t);
}
