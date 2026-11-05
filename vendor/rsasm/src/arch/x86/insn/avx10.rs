//! AVX10.2: the instructions added on top of the converged AVX-512 set.
//!
//! AVX10 folds AVX-512's subsets into one versioned instruction set; its
//! second version adds new operations rather than new encodings, so these are
//! EVEX rows in the shapes the other AVX-512 tables use:
//!
//! - EVEX forms of the VEX-only integer additions (AVX-VNNI-INT8/INT16,
//!   `vmpsadbw`, SM4), installed after the VEX rows so ordinary operands keep
//!   the shorter prefix;
//! - `vminmax`, the IEEE 754-2019 minimum and maximum;
//! - saturating truncations to integers (`vcvttps2dqs` and the `ibs`/`iubs`
//!   byte conversions), which clamp rather than returning the indefinite
//!   value;
//! - `vcomxss` and friends, compares that set the flags from all of the
//!   predicate rather than PF-signalled unordered;
//! - the bfloat16 arithmetic and the 8-bit float (`bf8`, `hf8`) conversions,
//!   in FP16's maps 5 and 6;
//! - `vmovd` and `vmovw` between vector registers, and `vmovrs*`, the moves
//!   that read memory at most once.
//!
//! As elsewhere, embedded rounding is on the 512-bit and scalar forms only.

use super::avx::{split_escape, vk};
use super::avx512::{ALL, half, nds, rm, with_imm};
use super::{Def, EVEX_ER, EVEX_SAE, ModRm, NOMASK, Op, Tbl, Tuple, Vk, add, d};

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// An EVEX row in an explicit map (1-3 as `0F`/`0F 38`/`0F 3A`, or 5 and 6).
fn ev(ops: Vec<Op>, pfx: u8, map: u8, op: u8, vlen: u16, w: bool, tuple: Tuple) -> Def {
    d(ops, &[op], ModRm::Reg, if w { 64 } else { 0 })
        .pfx(pfx)
        .map(map)
        .evex(vlen, tuple)
}

/// The same, with the opcode written with its legacy escape bytes.
fn ev_esc(ops: Vec<Op>, pfx: u8, esc: &[u8], vlen: u16, w: bool, tuple: Tuple) -> Def {
    let (map, op) = split_escape(esc);
    ev(ops, pfx, map, op, vlen, w, tuple)
}

fn at512(def: Def, flag: u32, vlen: u16) -> Def {
    if vlen == 512 { def.flags(flag) } else { def }
}

fn install_integer(t: &mut Tbl) {
    for (mnem, pfx, op) in [
        ("vpdpbuud", 0x00u8, 0x50u8),
        ("vpdpbuuds", 0x00, 0x51),
        ("vpdpbssd", 0xf2, 0x50),
        ("vpdpbssds", 0xf2, 0x51),
        ("vpdpbsud", 0xf3, 0x50),
        ("vpdpbsuds", 0xf3, 0x51),
        ("vpdpwuud", 0x00, 0xd2),
        ("vpdpwuuds", 0x00, 0xd3),
        ("vpdpwusd", 0x66, 0xd2),
        ("vpdpwusds", 0x66, 0xd3),
        ("vpdpwsud", 0xf3, 0xd2),
        ("vpdpwsuds", 0xf3, 0xd3),
        ("vdpphps", 0x00, 0x52),
    ] {
        for l in ALL {
            add(
                t,
                mnem,
                vec![ev_esc(
                    nds(vk(l)),
                    pfx,
                    &[0x0f, 0x38, op],
                    l,
                    false,
                    Tuple::Fv,
                )],
            );
        }
    }
    // SM4's rounds and key expansion take no writemask.
    for (mnem, pfx) in [("vsm4key4", 0xf3u8), ("vsm4rnds4", 0xf2)] {
        for l in ALL {
            add(
                t,
                mnem,
                vec![
                    ev_esc(nds(vk(l)), pfx, &[0x0f, 0x38, 0xda], l, false, Tuple::Fvm)
                        .flags(NOMASK),
                ],
            );
        }
    }
    // `vmpsadbw`'s EVEX form moved from `66` to `F3`.
    for l in ALL {
        add(
            t,
            "vmpsadbw",
            vec![ev_esc(
                with_imm(nds(vk(l))),
                0xf3,
                &[0x0f, 0x3a, 0x42],
                l,
                false,
                Tuple::Fvm,
            )],
        );
    }
}

fn install_float(t: &mut Tbl) {
    // (flavour, prefix, W, tuple) for single, double and half precision.
    let flavours = [
        ("ps", 0x66u8, false, Tuple::Fv),
        ("pd", 0x66, true, Tuple::Fv),
        ("ph", 0x00, false, Tuple::Fvw),
    ];
    for (flav, pfx, w, tuple) in flavours {
        let mnem = leak(format!("vminmax{flav}"));
        for l in ALL {
            add(
                t,
                mnem,
                vec![at512(
                    ev_esc(with_imm(nds(vk(l))), pfx, &[0x0f, 0x3a, 0x52], l, w, tuple),
                    EVEX_SAE,
                    l,
                )],
            );
        }
    }
    let x = Vk::Xmm;
    for (flav, pfx, w, memw, tuple) in [
        ("ss", 0x66u8, false, 4u8, Tuple::T1s),
        ("sd", 0x66, true, 8, Tuple::T1s),
        ("sh", 0x00, false, 2, Tuple::T1s16),
    ] {
        add(
            t,
            leak(format!("vminmax{flav}")),
            vec![
                ev_esc(
                    vec![Op::V(x), Op::Nds(x), Op::Vm(x, memw), Op::Imm(1)],
                    pfx,
                    &[0x0f, 0x3a, 0x53],
                    128,
                    w,
                    tuple,
                )
                .flags(EVEX_SAE),
            ],
        );
    }
    // The compares that report every predicate through the flags.
    for (stem, op) in [("vcomx", 0x2fu8), ("vucomx", 0x2e)] {
        for (flav, pfx, map, w, memw, tuple) in [
            ("ss", 0xf3u8, 1u8, false, 4u8, Tuple::T1s),
            ("sd", 0xf2, 1, true, 8, Tuple::T1s),
            ("sh", 0xf3, 5, false, 2, Tuple::T1s16),
        ] {
            add(
                t,
                leak(format!("{stem}{flav}")),
                vec![
                    ev(vec![Op::V(x), Op::Vm(x, memw)], pfx, map, op, 128, w, tuple)
                        .flags(EVEX_SAE | NOMASK),
                ],
            );
        }
    }
    // `vcvt2ps2phx` converts two single-precision vectors into one of halves.
    for l in ALL {
        add(
            t,
            "vcvt2ps2phx",
            vec![at512(
                ev_esc(nds(vk(l)), 0x66, &[0x0f, 0x38, 0x67], l, false, Tuple::Fv),
                EVEX_ER,
                l,
            )],
        );
    }
}

fn install_saturating(t: &mut Tbl) {
    // Element-for-element saturating conversions to integers, and to bytes
    // (`ibs`, `iubs`), from singles, halves and bfloat16s.
    for (mnem, pfx, op, w, tuple, flag) in [
        ("vcvttps2dqs", 0x00u8, 0x6du8, false, Tuple::Fv, EVEX_SAE),
        ("vcvttps2udqs", 0x00, 0x6c, false, Tuple::Fv, EVEX_SAE),
        ("vcvttpd2qqs", 0x66, 0x6d, true, Tuple::Fv, EVEX_SAE),
        ("vcvttpd2uqqs", 0x66, 0x6c, true, Tuple::Fv, EVEX_SAE),
        ("vcvtps2ibs", 0x66, 0x69, false, Tuple::Fv, EVEX_ER),
        ("vcvtps2iubs", 0x66, 0x6b, false, Tuple::Fv, EVEX_ER),
        ("vcvttps2ibs", 0x66, 0x68, false, Tuple::Fv, EVEX_SAE),
        ("vcvttps2iubs", 0x66, 0x6a, false, Tuple::Fv, EVEX_SAE),
        ("vcvtph2ibs", 0x00, 0x69, false, Tuple::Fvw, EVEX_ER),
        ("vcvtph2iubs", 0x00, 0x6b, false, Tuple::Fvw, EVEX_ER),
        ("vcvttph2ibs", 0x00, 0x68, false, Tuple::Fvw, EVEX_SAE),
        ("vcvttph2iubs", 0x00, 0x6a, false, Tuple::Fvw, EVEX_SAE),
        ("vcvtbf162ibs", 0xf2, 0x69, false, Tuple::Fvw, 0),
        ("vcvtbf162iubs", 0xf2, 0x6b, false, Tuple::Fvw, 0),
        ("vcvttbf162ibs", 0xf2, 0x68, false, Tuple::Fvw, 0),
        ("vcvttbf162iubs", 0xf2, 0x6a, false, Tuple::Fvw, 0),
    ] {
        for l in ALL {
            add(
                t,
                mnem,
                vec![at512(ev(rm(vk(l)), pfx, 5, op, l, w, tuple), flag, l)],
            );
        }
    }
    // Doubles to dwords narrow to half the register.
    for (mnem, op) in [("vcvttpd2dqs", 0x6du8), ("vcvttpd2udqs", 0x6c)] {
        for l in ALL {
            add(
                t,
                mnem,
                vec![at512(
                    ev(
                        vec![Op::V(half(l)), Op::Vm(vk(l), 0)],
                        0x00,
                        5,
                        op,
                        l,
                        true,
                        Tuple::Fv,
                    ),
                    EVEX_SAE,
                    l,
                )],
            );
        }
    }
    // Singles to qwords widen from half the register.
    for (mnem, op) in [("vcvttps2qqs", 0x6du8), ("vcvttps2uqqs", 0x6c)] {
        for l in ALL {
            add(
                t,
                mnem,
                vec![at512(
                    ev(
                        vec![Op::V(vk(l)), Op::Vm(half(l), (l / 16) as u8)],
                        0x66,
                        5,
                        op,
                        l,
                        false,
                        Tuple::Hv,
                    ),
                    EVEX_SAE,
                    l,
                )],
            );
        }
    }
    // Scalars to a general register.
    let x = Vk::Xmm;
    for (mnem, pfx, op, memw) in [
        ("vcvttss2sis", 0xf3u8, 0x6du8, 4u8),
        ("vcvttss2usis", 0xf3, 0x6c, 4),
        ("vcvttsd2sis", 0xf2, 0x6d, 8),
        ("vcvttsd2usis", 0xf2, 0x6c, 8),
    ] {
        let tuple = if memw == 4 {
            Tuple::T1s32
        } else {
            Tuple::T1s64
        };
        for gpr in [4u8, 8] {
            let mut def = ev(
                vec![Op::R(gpr), Op::Vm(x, memw)],
                pfx,
                5,
                op,
                128,
                gpr == 8,
                tuple,
            )
            .flags(EVEX_SAE | NOMASK);
            def.opsize = gpr * 8;
            add(t, mnem, vec![def]);
        }
    }
}

fn install_bf16(t: &mut Tbl) {
    // bfloat16 arithmetic, in FP16's opcode positions with a different
    // prefix, and no rounding control.
    for (stem, pfx, map, op, two) in [
        ("vadd", 0x66u8, 5u8, 0x58u8, true),
        ("vmul", 0x66, 5, 0x59, true),
        ("vsub", 0x66, 5, 0x5c, true),
        ("vdiv", 0x66, 5, 0x5e, true),
        ("vmin", 0x66, 5, 0x5d, true),
        ("vmax", 0x66, 5, 0x5f, true),
        ("vsqrt", 0x66, 5, 0x51, false),
        ("vscalef", 0x00, 6, 0x2c, true),
        ("vgetexp", 0x00, 6, 0x42, false),
        ("vrcp", 0x00, 6, 0x4c, false),
        ("vrsqrt", 0x00, 6, 0x4e, false),
    ] {
        let mnem = leak(format!("{stem}bf16"));
        for l in ALL {
            let ops = if two { nds(vk(l)) } else { rm(vk(l)) };
            add(t, mnem, vec![ev(ops, pfx, map, op, l, false, Tuple::Fvw)]);
        }
    }
    for (stem, base) in [
        ("fmadd", 0x98u8),
        ("fmsub", 0x9a),
        ("fnmadd", 0x9c),
        ("fnmsub", 0x9e),
    ] {
        for (order, bump) in [("132", 0x00u8), ("213", 0x10), ("231", 0x20)] {
            let mnem = leak(format!("v{stem}{order}bf16"));
            for l in ALL {
                add(
                    t,
                    mnem,
                    vec![ev(nds(vk(l)), 0x00, 6, base + bump, l, false, Tuple::Fvw)],
                );
            }
        }
    }
    for (stem, op) in [("vgetmant", 0x26u8), ("vreduce", 0x56), ("vrndscale", 0x08)] {
        let mnem = leak(format!("{stem}bf16"));
        for l in ALL {
            add(
                t,
                mnem,
                vec![ev(with_imm(rm(vk(l))), 0xf2, 3, op, l, false, Tuple::Fvw)],
            );
        }
    }
    for l in ALL {
        let k = vk(l);
        add(
            t,
            "vminmaxbf16",
            vec![ev(with_imm(nds(k)), 0xf2, 3, 0x52, l, false, Tuple::Fvw)],
        );
        add(
            t,
            "vcmpbf16",
            vec![ev(
                vec![Op::V(Vk::K), Op::Nds(k), Op::Vm(k, 0), Op::Imm(1)],
                0xf2,
                3,
                0xc2,
                l,
                false,
                Tuple::Fvw,
            )],
        );
        add(
            t,
            "vfpclassbf16",
            vec![ev(
                vec![Op::V(Vk::K), Op::Vm(k, 0), Op::Imm(1)],
                0xf2,
                3,
                0x66,
                l,
                false,
                Tuple::Fvw,
            )],
        );
    }
    let x = Vk::Xmm;
    add(
        t,
        "vcomisbf16",
        vec![
            ev(
                vec![Op::V(x), Op::Vm(x, 2)],
                0x66,
                5,
                0x2f,
                128,
                false,
                Tuple::T1s16,
            )
            .flags(NOMASK),
        ],
    );
}

fn install_fp8(t: &mut Tbl) {
    // Half precision to 8-bit floats: one vector in, half the register out;
    // or two in (`vcvt2ph2bf8`) and one of the same length out; or with a
    // bias vector (`vcvtbiasph2bf8`), half the register out.
    for (stem, map, op) in [
        ("bf8", 2u8, 0x74u8),
        ("bf8s", 5, 0x74),
        ("hf8", 5, 0x18),
        ("hf8s", 5, 0x1b),
    ] {
        let one = leak(format!("vcvtph2{stem}"));
        let two = leak(format!("vcvt2ph2{stem}"));
        let bias = leak(format!("vcvtbiasph2{stem}"));
        for l in ALL {
            let k = vk(l);
            add(
                t,
                one,
                vec![ev(
                    vec![Op::V(half(l)), Op::Vm(k, 0)],
                    0xf3,
                    map,
                    op,
                    l,
                    false,
                    Tuple::Fvw,
                )],
            );
            add(
                t,
                two,
                vec![ev(nds(k), 0xf2, map, op, l, false, Tuple::Fvw)],
            );
            add(
                t,
                bias,
                vec![ev(
                    vec![Op::V(half(l)), Op::Nds(k), Op::Vm(k, 0)],
                    0x00,
                    map,
                    op,
                    l,
                    false,
                    Tuple::Fvw,
                )],
            );
        }
    }
    // And back: bytes widen to twice their register.
    for l in ALL {
        add(
            t,
            "vcvthf82ph",
            vec![ev(
                vec![Op::V(vk(l)), Op::Vm(half(l), (l / 16) as u8)],
                0xf2,
                5,
                0x1e,
                l,
                false,
                Tuple::Hvm,
            )],
        );
    }
}

fn install_moves(t: &mut Tbl) {
    let x = Vk::Xmm;
    // Low dword or word to another vector register, zero-extended.
    add(
        t,
        "vmovd",
        vec![
            ev(
                vec![Op::V(x), Op::Vm(x, 4)],
                0xf3,
                1,
                0x7e,
                128,
                false,
                Tuple::T1s32,
            )
            .flags(NOMASK),
            ev(
                vec![Op::Vm(x, 4), Op::V(x)],
                0x66,
                1,
                0xd6,
                128,
                false,
                Tuple::T1s32,
            )
            .flags(NOMASK),
        ],
    );
    add(
        t,
        "vmovw",
        vec![
            ev(
                vec![Op::V(x), Op::Vm(x, 2)],
                0xf3,
                5,
                0x6e,
                128,
                false,
                Tuple::T1s16,
            )
            .flags(NOMASK),
            ev(
                vec![Op::Vm(x, 2), Op::V(x)],
                0xf3,
                5,
                0x7e,
                128,
                false,
                Tuple::T1s16,
            )
            .flags(NOMASK),
        ],
    );
    for (mnem, pfx, w) in [
        ("vmovrsb", 0xf2u8, false),
        ("vmovrsw", 0xf2, true),
        ("vmovrsd", 0xf3, false),
        ("vmovrsq", 0xf3, true),
    ] {
        for l in ALL {
            add(
                t,
                mnem,
                vec![ev(
                    vec![Op::V(vk(l)), Op::M((l / 8) as u8)],
                    pfx,
                    5,
                    0x6f,
                    l,
                    w,
                    Tuple::Fvm,
                )],
            );
        }
    }
}

pub fn install(t: &mut Tbl) {
    install_integer(t);
    install_float(t);
    install_saturating(t);
    install_bf16(t);
    install_fp8(t);
    install_moves(t);
}
