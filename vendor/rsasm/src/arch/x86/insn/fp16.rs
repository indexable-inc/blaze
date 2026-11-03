//! AVX-512FP16: half-precision floating point, the `ph` and `sh` families.
//!
//! FP16 needed room the `0F`, `0F 38` and `0F 3A` maps no longer had, so most
//! of it lives in the EVEX-only maps 5 and 6, where it reuses the familiar
//! opcode numbers: `vaddph` is `58` in map 5 as `vaddps` is `58` in map 1,
//! and `vfmadd132ph` is `98` in map 6 as `vfmadd132ps` is `98` in map 2. The
//! handful that fit in map 3 (`vcmpph`, `vgetmantph`, `vrndscaleph`,
//! `vreduceph`, `vfpclassph`) are the `ps` opcodes with no `66`.
//!
//! The element is a word, so a broadcast repeats two bytes and a scalar reads
//! two, which is what the `Fvw`/`Hvw`/`Qvw` and `T1s16` tuples express.

use super::avx::vk;
use super::avx512::{ALL, half, nds, rm, with_imm};
use super::{
    DISTINCT_DEST, Def, EVEX_ER, EVEX_SAE, ModRm, NOMASK, Op, R_IN_RM, Tbl, Tuple, Vk, add, d,
};

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// An EVEX row in an explicit map.
fn ev(ops: Vec<Op>, pfx: u8, map: u8, op: u8, vlen: u16, w: bool, tuple: Tuple) -> Def {
    d(ops, &[op], ModRm::Reg, if w { 64 } else { 0 })
        .pfx(pfx)
        .map(map)
        .evex(vlen, tuple)
}

/// Rounding or exception control belongs to the 512-bit packed form.
fn at512(def: Def, flag: u32, vlen: u16) -> Def {
    if vlen == 512 { def.flags(flag) } else { def }
}

/// A scalar row, `op xmm, xmm, xmm/m16`.
fn scalar(pfx: u8, map: u8, op: u8, flag: u32) -> Def {
    let x = Vk::Xmm;
    ev(
        vec![Op::V(x), Op::Nds(x), Op::Vm(x, 2)],
        pfx,
        map,
        op,
        128,
        false,
        Tuple::T1s16,
    )
    .flags(flag)
}

fn install_arith(t: &mut Tbl) {
    // (stem, map, packed opcode, packed takes vvvv, control, scalar opcode)
    #[rustfmt::skip]
    const OPS: &[(&str, u8, u8, bool, u32)] = &[
        ("vadd", 5, 0x58, true, EVEX_ER),
        ("vmul", 5, 0x59, true, EVEX_ER),
        ("vsub", 5, 0x5c, true, EVEX_ER),
        ("vdiv", 5, 0x5e, true, EVEX_ER),
        ("vmin", 5, 0x5d, true, EVEX_SAE),
        ("vmax", 5, 0x5f, true, EVEX_SAE),
        ("vsqrt", 5, 0x51, false, EVEX_ER),
    ];
    for &(stem, map, op, two, flag) in OPS {
        let ph = leak(format!("{stem}ph"));
        for l in ALL {
            let ops = if two { nds(vk(l)) } else { rm(vk(l)) };
            add(
                t,
                ph,
                vec![at512(ev(ops, 0x00, map, op, l, false, Tuple::Fvw), flag, l)],
            );
        }
        add(
            t,
            leak(format!("{stem}sh")),
            vec![scalar(0xf3, map, op, flag)],
        );
    }
    // Map 6's `66`-prefixed operations, with the scalar one opcode up.
    #[rustfmt::skip]
    const MAP6: &[(&str, u8, bool, u32)] = &[
        ("vscalef", 0x2c, true, EVEX_ER),
        ("vgetexp", 0x42, false, EVEX_SAE),
        ("vrcp", 0x4c, false, 0),
        ("vrsqrt", 0x4e, false, 0),
    ];
    for &(stem, op, two, flag) in MAP6 {
        let ph = leak(format!("{stem}ph"));
        for l in ALL {
            let ops = if two { nds(vk(l)) } else { rm(vk(l)) };
            add(
                t,
                ph,
                vec![at512(ev(ops, 0x66, 6, op, l, false, Tuple::Fvw), flag, l)],
            );
        }
        add(
            t,
            leak(format!("{stem}sh")),
            vec![scalar(0x66, 6, op + 1, flag)],
        );
    }
    // Map 3, where the `ps` opcodes are reused without a prefix.
    for (stem, op, flag) in [
        ("vgetmant", 0x26u8, EVEX_SAE),
        ("vrndscale", 0x08, EVEX_SAE),
        ("vreduce", 0x56, EVEX_SAE),
    ] {
        let ph = leak(format!("{stem}ph"));
        for l in ALL {
            add(
                t,
                ph,
                vec![at512(
                    ev(with_imm(rm(vk(l))), 0x00, 3, op, l, false, Tuple::Fvw),
                    flag,
                    l,
                )],
            );
        }
        // `vrndscalesh` skips an opcode, as `vrndscaless` does.
        let sop = if op == 0x08 { 0x0a } else { op + 1 };
        let mut sh = scalar(0x00, 3, sop, flag);
        sh.ops.push(Op::Imm(1));
        add(t, leak(format!("{stem}sh")), vec![sh]);
    }
    for l in ALL {
        let k = vk(l);
        add(
            t,
            "vcmpph",
            vec![at512(
                ev(
                    vec![Op::V(Vk::K), Op::Nds(k), Op::Vm(k, 0), Op::Imm(1)],
                    0x00,
                    3,
                    0xc2,
                    l,
                    false,
                    Tuple::Fvw,
                ),
                EVEX_SAE,
                l,
            )],
        );
        add(
            t,
            "vfpclassph",
            vec![ev(
                vec![Op::V(Vk::K), Op::Vm(k, 0), Op::Imm(1)],
                0x00,
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
        "vcmpsh",
        vec![
            ev(
                vec![Op::V(Vk::K), Op::Nds(x), Op::Vm(x, 2), Op::Imm(1)],
                0xf3,
                3,
                0xc2,
                128,
                false,
                Tuple::T1s16,
            )
            .flags(EVEX_SAE),
        ],
    );
    add(
        t,
        "vfpclasssh",
        vec![ev(
            vec![Op::V(Vk::K), Op::Vm(x, 2), Op::Imm(1)],
            0x00,
            3,
            0x67,
            128,
            false,
            Tuple::T1s16,
        )],
    );
    for (mnem, op) in [("vucomish", 0x2eu8), ("vcomish", 0x2f)] {
        add(
            t,
            mnem,
            vec![
                ev(
                    vec![Op::V(x), Op::Vm(x, 2)],
                    0x00,
                    5,
                    op,
                    128,
                    false,
                    Tuple::T1s16,
                )
                .flags(EVEX_SAE | NOMASK),
            ],
        );
    }

    // Fused multiply-add, in map 6 at the FMA opcodes.
    for (stem, base, scalar_too) in [
        ("fmaddsub", 0x96u8, false),
        ("fmsubadd", 0x97, false),
        ("fmadd", 0x98, true),
        ("fmsub", 0x9a, true),
        ("fnmadd", 0x9c, true),
        ("fnmsub", 0x9e, true),
    ] {
        for (order, bump) in [("132", 0x00u8), ("213", 0x10), ("231", 0x20)] {
            let ph = leak(format!("v{stem}{order}ph"));
            for l in ALL {
                add(
                    t,
                    ph,
                    vec![at512(
                        ev(nds(vk(l)), 0x66, 6, base + bump, l, false, Tuple::Fvw),
                        EVEX_ER,
                        l,
                    )],
                );
            }
            if scalar_too {
                add(
                    t,
                    leak(format!("v{stem}{order}sh")),
                    vec![scalar(0x66, 6, base + bump + 1, EVEX_ER)],
                );
            }
        }
    }
    // Complex multiplication and multiply-add: pairs of halves, read as
    // dwords, whose destination must differ from both sources.
    for (mnem, pfx, op) in [
        ("vfmaddc", 0xf3u8, 0x56u8),
        ("vfcmaddc", 0xf2, 0x56),
        ("vfmulc", 0xf3, 0xd6),
        ("vfcmulc", 0xf2, 0xd6),
    ] {
        let ph = leak(format!("{mnem}ph"));
        for l in ALL {
            add(
                t,
                ph,
                vec![at512(
                    ev(nds(vk(l)), pfx, 6, op, l, false, Tuple::Fv).flags(DISTINCT_DEST),
                    EVEX_ER,
                    l,
                )],
            );
        }
        add(
            t,
            leak(format!("{mnem}sh")),
            vec![
                ev(
                    vec![Op::V(x), Op::Nds(x), Op::Vm(x, 4)],
                    pfx,
                    6,
                    op + 1,
                    128,
                    false,
                    Tuple::T1s,
                )
                .flags(EVEX_ER | DISTINCT_DEST),
            ],
        );
    }
}

fn install_moves(t: &mut Tbl) {
    let x = Vk::Xmm;
    add(
        t,
        "vmovsh",
        vec![
            ev(
                vec![Op::V(x), Op::M(2)],
                0xf3,
                5,
                0x10,
                128,
                false,
                Tuple::T1s16,
            ),
            ev(
                vec![Op::M(2), Op::V(x)],
                0xf3,
                5,
                0x11,
                128,
                false,
                Tuple::T1s16,
            ),
            ev(
                vec![Op::V(x), Op::Nds(x), Op::V(x)],
                0xf3,
                5,
                0x10,
                128,
                false,
                Tuple::None,
            ),
        ],
    );
    add(
        t,
        "vmovw",
        vec![
            ev(
                vec![Op::V(x), Op::M(2)],
                0x66,
                5,
                0x6e,
                128,
                false,
                Tuple::T1s16,
            )
            .flags(NOMASK),
            ev(
                vec![Op::M(2), Op::V(x)],
                0x66,
                5,
                0x7e,
                128,
                false,
                Tuple::T1s16,
            )
            .flags(NOMASK),
            ev(
                vec![Op::V(x), Op::R(4)],
                0x66,
                5,
                0x6e,
                128,
                false,
                Tuple::None,
            )
            .flags(NOMASK),
            ev(
                vec![Op::R(4), Op::V(x)],
                0x66,
                5,
                0x7e,
                128,
                false,
                Tuple::None,
            )
            .flags(NOMASK | R_IN_RM),
        ],
    );
}

fn install_conversions(t: &mut Tbl) {
    // Word-integer to half and back, element for element.
    for (mnem, pfx, op, flag) in [
        ("vcvtw2ph", 0xf3u8, 0x7du8, EVEX_ER),
        ("vcvtuw2ph", 0xf2, 0x7d, EVEX_ER),
        ("vcvtph2w", 0x66, 0x7d, EVEX_ER),
        ("vcvtph2uw", 0x00, 0x7d, EVEX_ER),
        ("vcvttph2w", 0x66, 0x7c, EVEX_SAE),
        ("vcvttph2uw", 0x00, 0x7c, EVEX_SAE),
    ] {
        for l in ALL {
            add(
                t,
                mnem,
                vec![at512(
                    ev(rm(vk(l)), pfx, 5, op, l, false, Tuple::Fvw),
                    flag,
                    l,
                )],
            );
        }
    }
    // Widening: half the register of halves to dwords or singles, a quarter
    // of it to qwords or doubles.
    for (mnem, pfx, map, op, flag, quarter) in [
        ("vcvtph2dq", 0x66u8, 5u8, 0x5bu8, EVEX_ER, false),
        ("vcvtph2udq", 0x00, 5, 0x79, EVEX_ER, false),
        ("vcvttph2dq", 0xf3, 5, 0x5b, EVEX_SAE, false),
        ("vcvttph2udq", 0x00, 5, 0x78, EVEX_SAE, false),
        ("vcvtph2psx", 0x66, 6, 0x13, EVEX_SAE, false),
        ("vcvtph2qq", 0x66, 5, 0x7b, EVEX_ER, true),
        ("vcvtph2uqq", 0x66, 5, 0x79, EVEX_ER, true),
        ("vcvttph2qq", 0x66, 5, 0x7a, EVEX_SAE, true),
        ("vcvttph2uqq", 0x66, 5, 0x78, EVEX_SAE, true),
        ("vcvtph2pd", 0x00, 5, 0x5a, EVEX_SAE, true),
    ] {
        for l in ALL {
            let (src, bytes, tuple) = if quarter {
                (Vk::Xmm, (l / 32) as u8, Tuple::Qvw)
            } else {
                (half(l), (l / 16) as u8, Tuple::Hvw)
            };
            add(
                t,
                mnem,
                vec![at512(
                    ev(
                        vec![Op::V(vk(l)), Op::Vm(src, bytes)],
                        pfx,
                        map,
                        op,
                        l,
                        false,
                        tuple,
                    ),
                    flag,
                    l,
                )],
            );
        }
    }
    // Narrowing: dwords or singles to half the register of halves, qwords or
    // doubles to a quarter.
    for (mnem, pfx, op, w, quarter) in [
        ("vcvtdq2ph", 0x00u8, 0x5bu8, false, false),
        ("vcvtudq2ph", 0xf2, 0x7a, false, false),
        ("vcvtps2phx", 0x66, 0x1d, false, false),
        ("vcvtqq2ph", 0x00, 0x5b, true, true),
        ("vcvtuqq2ph", 0xf2, 0x7a, true, true),
        ("vcvtpd2ph", 0x66, 0x5a, true, true),
    ] {
        for l in ALL {
            let dst = if quarter { Vk::Xmm } else { half(l) };
            add(
                t,
                mnem,
                vec![at512(
                    ev(
                        vec![Op::V(dst), Op::Vm(vk(l), 0)],
                        pfx,
                        5,
                        op,
                        l,
                        w,
                        Tuple::Fv,
                    ),
                    EVEX_ER,
                    l,
                )],
            );
        }
    }

    // Scalar conversions.
    let x = Vk::Xmm;
    for (mnem, pfx, map, op, memw, w, flag) in [
        ("vcvtsh2sd", 0xf3u8, 5u8, 0x5au8, 2u8, false, EVEX_SAE),
        ("vcvtsh2ss", 0x00, 6, 0x13, 2, false, EVEX_SAE),
        ("vcvtsd2sh", 0xf2, 5, 0x5a, 8, true, EVEX_ER),
        ("vcvtss2sh", 0x00, 5, 0x1d, 4, false, EVEX_ER),
    ] {
        let tuple = if memw == 2 { Tuple::T1s16 } else { Tuple::T1s };
        add(
            t,
            mnem,
            vec![
                ev(
                    vec![Op::V(x), Op::Nds(x), Op::Vm(x, memw)],
                    pfx,
                    map,
                    op,
                    128,
                    w,
                    tuple,
                )
                .flags(flag),
            ],
        );
    }
    for (mnem, op, flag) in [
        ("vcvtsh2si", 0x2du8, EVEX_ER),
        ("vcvtsh2usi", 0x79, EVEX_ER),
        ("vcvttsh2si", 0x2c, EVEX_SAE),
        ("vcvttsh2usi", 0x78, EVEX_SAE),
    ] {
        for gpr in [4u8, 8] {
            let mut def = ev(
                vec![Op::R(gpr), Op::Vm(x, 2)],
                0xf3,
                5,
                op,
                128,
                gpr == 8,
                Tuple::T1s16,
            )
            .flags(flag | NOMASK);
            def.opsize = gpr * 8;
            add(t, mnem, vec![def]);
        }
    }
    for (mnem, op) in [("vcvtsi2sh", 0x2au8), ("vcvtusi2sh", 0x7b)] {
        for gpr in [4u8, 8] {
            let mut def = ev(
                vec![Op::V(x), Op::Nds(x), Op::Rm(gpr)],
                0xf3,
                5,
                op,
                128,
                gpr == 8,
                Tuple::T1s,
            )
            .flags(EVEX_ER | NOMASK);
            def.opsize = gpr * 8;
            add(t, mnem, vec![def]);
        }
    }
}

pub fn install(t: &mut Tbl) {
    install_arith(t);
    install_moves(t);
    install_conversions(t);
}
