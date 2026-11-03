//! The AVX-512 subsets beyond the foundation: BW, DQ, CD and the later
//! single-purpose extensions.
//!
//! Each of these is a small set of EVEX rows in the shapes `avx512.rs`
//! already uses, so the rules there apply unchanged: every length the subset
//! defines gets a row, a memory form carries its [`Tuple`], and a row that
//! cannot be masked says so.
//!
//! What separates the subsets is mostly the element size. BW is the byte and
//! word half of the integer instruction set, which AVX-512F left out because
//! its opmasks were only 16 bits wide; DQ adds the dword/qword operations
//! whose element count needs a 64-bit mask, the quadword conversions and the
//! `32x2`/`32x8`/`64x2` sub-vector shuffles; CD is four instructions for
//! writing a conflict-detecting loop.

use super::avx::{split_escape, vk};
use super::avx512::{ALL, ev, half, nds, rm, with_imm};
use super::{Def, EVEX_ER, EVEX_SAE, ModRm, NOMASK, Op, R_IN_RM, Tbl, Tuple, Vk, add, d};

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// `op k, src1, src2/m`: a comparison writing an opmask.
fn to_mask(k: Vk) -> Vec<Op> {
    vec![Op::V(Vk::K), Op::Nds(k), Op::Vm(k, 0)]
}

/// An EVEX row whose ModRM reg field is a fixed extension, as the
/// shift-by-immediate forms have.
fn ev_ext(ops: Vec<Op>, esc: &[u8], ext: u8, vlen: u16, w: bool, tuple: Tuple) -> Def {
    let (map, op) = split_escape(esc);
    d(ops, &[op], ModRm::Ext(ext), if w { 64 } else { 0 })
        .pfx(0x66)
        .map(map)
        .evex(vlen, tuple)
}

// ---------------------------------------------------------------------------
// AVX-512BW: bytes and words
// ---------------------------------------------------------------------------

fn install_bw(t: &mut Tbl) {
    // Three-operand byte/word arithmetic. None of it broadcasts — there is no
    // `{1to64}` — so the tuple is the plain full vector.
    #[rustfmt::skip]
    const BINARY: &[(&str, u8, &[u8])] = &[
        ("vpacksswb", 0x66, &[0x0f, 0x63]),
        ("vpackuswb", 0x66, &[0x0f, 0x67]),
        ("vpmaddubsw", 0x66, &[0x0f, 0x38, 0x04]),
        ("vpmaddwd", 0x66, &[0x0f, 0xf5]),
        ("vpmaxsb", 0x66, &[0x0f, 0x38, 0x3c]),
        ("vpmaxsw", 0x66, &[0x0f, 0xee]),
        ("vpmaxub", 0x66, &[0x0f, 0xde]),
        ("vpmaxuw", 0x66, &[0x0f, 0x38, 0x3e]),
        ("vpminsb", 0x66, &[0x0f, 0x38, 0x38]),
        ("vpminsw", 0x66, &[0x0f, 0xea]),
        ("vpminub", 0x66, &[0x0f, 0xda]),
        ("vpminuw", 0x66, &[0x0f, 0x38, 0x3a]),
        ("vpmulhrsw", 0x66, &[0x0f, 0x38, 0x0b]),
        ("vpmulhuw", 0x66, &[0x0f, 0xe4]),
        ("vpshufb", 0x66, &[0x0f, 0x38, 0x00]),
        ("vpsubsb", 0x66, &[0x0f, 0xe8]),
        ("vpsubsw", 0x66, &[0x0f, 0xe9]),
        ("vpsubusb", 0x66, &[0x0f, 0xd8]),
        ("vpsubusw", 0x66, &[0x0f, 0xd9]),
        ("vpunpckhbw", 0x66, &[0x0f, 0x68]),
        ("vpunpckhwd", 0x66, &[0x0f, 0x69]),
        ("vpunpcklbw", 0x66, &[0x0f, 0x60]),
        ("vpunpcklwd", 0x66, &[0x0f, 0x61]),
    ];
    for &(mnem, pfx, esc) in BINARY {
        for l in ALL {
            add(
                t,
                mnem,
                vec![ev(nds(vk(l)), pfx, esc, l, false, Tuple::Fvm)],
            );
        }
    }
    // `vpsadbw` has no per-element result, so no writemask either.
    for l in ALL {
        add(
            t,
            "vpsadbw",
            vec![ev(nds(vk(l)), 0x66, &[0x0f, 0xf6], l, false, Tuple::Fvm).flags(NOMASK)],
        );
    }
    // The two packs whose source is a dword, and so do broadcast.
    for (mnem, esc) in [
        ("vpackssdw", &[0x0fu8, 0x6b] as &[u8]),
        ("vpackusdw", &[0x0f, 0x38, 0x2b]),
    ] {
        for l in ALL {
            add(
                t,
                mnem,
                vec![ev(nds(vk(l)), 0x66, esc, l, false, Tuple::Fv)],
            );
        }
    }
    for (mnem, op) in [("vpabsb", 0x1cu8), ("vpabsw", 0x1d)] {
        for l in ALL {
            add(
                t,
                mnem,
                vec![ev(rm(vk(l)), 0x66, &[0x0f, 0x38, op], l, false, Tuple::Fvm)],
            );
        }
    }
    for (mnem, esc) in [
        ("vpalignr", &[0x0fu8, 0x3a, 0x0f] as &[u8]),
        ("vdbpsadbw", &[0x0f, 0x3a, 0x42]),
    ] {
        for l in ALL {
            add(
                t,
                mnem,
                vec![ev(with_imm(nds(vk(l))), 0x66, esc, l, false, Tuple::Fvm)],
            );
        }
    }
    for (mnem, pfx) in [("vpshufhw", 0xf3u8), ("vpshuflw", 0xf2)] {
        for l in ALL {
            add(
                t,
                mnem,
                vec![ev(
                    with_imm(rm(vk(l))),
                    pfx,
                    &[0x0f, 0x70],
                    l,
                    false,
                    Tuple::Fvm,
                )],
            );
        }
    }

    // Word shifts. The count is either an immediate, with the destination in
    // `vvvv`, or the low quadword of an `xmm`, which is sixteen bytes of
    // memory whatever the vector length.
    for (mnem, ext, op) in [
        ("vpsllw", 6u8, 0xf1u8),
        ("vpsraw", 4, 0xe1),
        ("vpsrlw", 2, 0xd1),
    ] {
        for l in ALL {
            let k = vk(l);
            add(
                t,
                mnem,
                vec![
                    ev_ext(
                        vec![Op::Nds(k), Op::Vm(k, 0), Op::Imm(1)],
                        &[0x0f, 0x71],
                        ext,
                        l,
                        false,
                        Tuple::Fvm,
                    ),
                    ev(
                        vec![Op::V(k), Op::Nds(k), Op::Vm(Vk::Xmm, 16)],
                        0x66,
                        &[0x0f, op],
                        l,
                        false,
                        Tuple::M128,
                    ),
                ],
            );
        }
    }
    // The whole-register byte shifts write no per-element result and take no
    // writemask.
    for (mnem, ext) in [("vpslldq", 7u8), ("vpsrldq", 3)] {
        for l in ALL {
            let k = vk(l);
            add(
                t,
                mnem,
                vec![
                    ev_ext(
                        vec![Op::Nds(k), Op::Vm(k, 0), Op::Imm(1)],
                        &[0x0f, 0x73],
                        ext,
                        l,
                        false,
                        Tuple::Fvm,
                    )
                    .flags(NOMASK),
                ],
            );
        }
    }
    // Variable shifts, one count per word.
    for (mnem, op) in [("vpsllvw", 0x12u8), ("vpsravw", 0x11), ("vpsrlvw", 0x10)] {
        for l in ALL {
            add(
                t,
                mnem,
                vec![ev(nds(vk(l)), 0x66, &[0x0f, 0x38, op], l, true, Tuple::Fvm)],
            );
        }
    }

    // Byte and word permutes and blends, and the byte/word test-into-mask.
    for (mnem, esc, w) in [
        ("vpermw", &[0x0fu8, 0x38, 0x8d] as &[u8], true),
        ("vpermi2w", &[0x0f, 0x38, 0x75], true),
        ("vpermt2w", &[0x0f, 0x38, 0x7d], true),
        ("vpblendmb", &[0x0f, 0x38, 0x66], false),
        ("vpblendmw", &[0x0f, 0x38, 0x66], true),
    ] {
        for l in ALL {
            add(t, mnem, vec![ev(nds(vk(l)), 0x66, esc, l, w, Tuple::Fvm)]);
        }
    }
    for (mnem, pfx, w) in [
        ("vptestmb", 0x66u8, false),
        ("vptestmw", 0x66, true),
        ("vptestnmb", 0xf3, false),
        ("vptestnmw", 0xf3, true),
    ] {
        for l in ALL {
            add(
                t,
                mnem,
                vec![ev(
                    to_mask(vk(l)),
                    pfx,
                    &[0x0f, 0x38, 0x26],
                    l,
                    w,
                    Tuple::Fvm,
                )],
            );
        }
    }
    // The saturating and truncating word-to-byte narrows.
    for (mnem, op) in [("vpmovwb", 0x30u8), ("vpmovswb", 0x20), ("vpmovuswb", 0x10)] {
        for l in ALL {
            let dst = half(l);
            add(
                t,
                mnem,
                vec![ev(
                    vec![Op::Vm(dst, (l / 16) as u8), Op::V(vk(l))],
                    0xf3,
                    &[0x0f, 0x38, op],
                    l,
                    false,
                    Tuple::Hvm,
                )],
            );
        }
    }
    for (mnem, op) in [("vpmovsxbw", 0x20u8), ("vpmovzxbw", 0x30)] {
        for l in ALL {
            let src = half(l);
            add(
                t,
                mnem,
                vec![ev(
                    vec![Op::V(vk(l)), Op::Vm(src, (l / 16) as u8)],
                    0x66,
                    &[0x0f, 0x38, op],
                    l,
                    false,
                    Tuple::Hvm,
                )],
            );
        }
    }
    // Byte and word compares with a predicate, signed and unsigned.
    for (mnem, op, w) in [
        ("vpcmpb", 0x3fu8, false),
        ("vpcmpw", 0x3f, true),
        ("vpcmpub", 0x3e, false),
        ("vpcmpuw", 0x3e, true),
    ] {
        for l in ALL {
            add(
                t,
                mnem,
                vec![ev(
                    with_imm(to_mask(vk(l))),
                    0x66,
                    &[0x0f, 0x3a, op],
                    l,
                    w,
                    Tuple::Fvm,
                )],
            );
        }
    }
    // Vector to mask and back, one bit per byte or word.
    for (to_m, from_m, w) in [
        ("vpmovb2m", "vpmovm2b", false),
        ("vpmovw2m", "vpmovm2w", true),
    ] {
        add_mask_moves(t, to_m, from_m, 0x28, w);
    }

    // 128-bit inserts and extracts of a byte or word, which EVEX needs only
    // to reach `xmm16` and up.
    add(
        t,
        "vpextrb",
        vec![
            ev(
                vec![Op::R(4), Op::V(Vk::Xmm), Op::Imm(1)],
                0x66,
                &[0x0f, 0x3a, 0x14],
                128,
                false,
                Tuple::T1s8,
            )
            .flags(NOMASK | super::R_IN_RM),
            ev(
                vec![Op::R(8), Op::V(Vk::Xmm), Op::Imm(1)],
                0x66,
                &[0x0f, 0x3a, 0x14],
                128,
                false,
                Tuple::T1s8,
            )
            .flags(NOMASK | super::R_IN_RM),
            ev(
                vec![Op::M(1), Op::V(Vk::Xmm), Op::Imm(1)],
                0x66,
                &[0x0f, 0x3a, 0x14],
                128,
                false,
                Tuple::T1s8,
            )
            .flags(NOMASK),
        ],
    );
    add(
        t,
        "vpextrw",
        vec![
            ev(
                vec![Op::R(4), Op::V(Vk::Xmm), Op::Imm(1)],
                0x66,
                &[0x0f, 0xc5],
                128,
                false,
                Tuple::None,
            )
            .flags(NOMASK),
            ev(
                vec![Op::R(8), Op::V(Vk::Xmm), Op::Imm(1)],
                0x66,
                &[0x0f, 0xc5],
                128,
                false,
                Tuple::None,
            )
            .flags(NOMASK),
            ev(
                vec![Op::M(2), Op::V(Vk::Xmm), Op::Imm(1)],
                0x66,
                &[0x0f, 0x3a, 0x15],
                128,
                false,
                Tuple::T1s16,
            )
            .flags(NOMASK),
        ],
    );
    for (mnem, esc, memw, tuple) in [
        ("vpinsrb", &[0x0fu8, 0x3a, 0x20] as &[u8], 1u8, Tuple::T1s8),
        ("vpinsrw", &[0x0f, 0xc4], 2, Tuple::T1s16),
    ] {
        add(
            t,
            mnem,
            vec![
                ev(
                    vec![Op::V(Vk::Xmm), Op::Nds(Vk::Xmm), Op::R(4), Op::Imm(1)],
                    0x66,
                    esc,
                    128,
                    false,
                    tuple,
                )
                .flags(NOMASK),
                ev(
                    vec![Op::V(Vk::Xmm), Op::Nds(Vk::Xmm), Op::R(8), Op::Imm(1)],
                    0x66,
                    esc,
                    128,
                    false,
                    tuple,
                )
                .flags(NOMASK),
                ev(
                    vec![Op::V(Vk::Xmm), Op::Nds(Vk::Xmm), Op::M(memw), Op::Imm(1)],
                    0x66,
                    esc,
                    128,
                    false,
                    tuple,
                )
                .flags(NOMASK),
            ],
        );
    }
}

// ---------------------------------------------------------------------------
// AVX-512DQ: quadword integers, and the sub-vector shuffles
// ---------------------------------------------------------------------------

fn install_dq(t: &mut Tbl) {
    // Conversions between packed floats and 64-bit integers, signed and not.
    for (mnem, pfx, op, w, flag) in [
        ("vcvtpd2qq", 0x66u8, 0x7bu8, true, EVEX_ER),
        ("vcvtpd2uqq", 0x66, 0x79, true, EVEX_ER),
        ("vcvttpd2qq", 0x66, 0x7a, true, EVEX_SAE),
        ("vcvttpd2uqq", 0x66, 0x78, true, EVEX_SAE),
        ("vcvtqq2pd", 0xf3, 0xe6, true, EVEX_ER),
        ("vcvtuqq2pd", 0xf3, 0x7a, true, EVEX_ER),
    ] {
        for l in ALL {
            let def = ev(rm(vk(l)), pfx, &[0x0f, op], l, w, Tuple::Fv);
            add(t, mnem, vec![rounding(def, flag, l)]);
        }
    }
    // Widening: a half-register source of dwords becomes qwords.
    for (mnem, pfx, op, flag) in [
        ("vcvtps2qq", 0x66u8, 0x7bu8, EVEX_ER),
        ("vcvtps2uqq", 0x66, 0x79, EVEX_ER),
        ("vcvttps2qq", 0x66, 0x7a, EVEX_SAE),
        ("vcvttps2uqq", 0x66, 0x78, EVEX_SAE),
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
            add(t, mnem, vec![rounding(def, flag, l)]);
        }
    }
    // Narrowing: qwords to a half register of floats.
    for (mnem, pfx, op) in [("vcvtqq2ps", 0x00u8, 0x5bu8), ("vcvtuqq2ps", 0xf2, 0x7a)] {
        for l in ALL {
            let def = ev(
                vec![Op::V(half(l)), Op::Vm(vk(l), 0)],
                pfx,
                &[0x0f, op],
                l,
                true,
                Tuple::Fv,
            );
            add(t, mnem, vec![rounding(def, EVEX_ER, l)]);
        }
    }

    // `vfpclass` and `vrange`/`vreduce`, DQ's own float operations.
    for (stem, w) in [("ps", false), ("pd", true)] {
        let mnem = leak(format!("vfpclass{stem}"));
        for l in ALL {
            add(
                t,
                mnem,
                vec![ev(
                    vec![Op::V(Vk::K), Op::Vm(vk(l), 0), Op::Imm(1)],
                    0x66,
                    &[0x0f, 0x3a, 0x66],
                    l,
                    w,
                    Tuple::Fv,
                )],
            );
        }
    }
    for (stem, w, memw) in [("ss", false, 4u8), ("sd", true, 8)] {
        add(
            t,
            leak(format!("vfpclass{stem}")),
            vec![ev(
                vec![Op::V(Vk::K), Op::Vm(Vk::Xmm, memw), Op::Imm(1)],
                0x66,
                &[0x0f, 0x3a, 0x67],
                128,
                w,
                Tuple::T1s,
            )],
        );
    }
    for (stem, pop, sop) in [("range", 0x50u8, 0x51u8), ("reduce", 0x56, 0x57)] {
        for (flav, w) in [("ps", false), ("pd", true)] {
            let mnem = leak(format!("v{stem}{flav}"));
            for l in ALL {
                // `vrange` combines two vectors; `vreduce` works on one.
                let ops = if stem == "range" {
                    nds(vk(l))
                } else {
                    rm(vk(l))
                };
                let def = ev(with_imm(ops), 0x66, &[0x0f, 0x3a, pop], l, w, Tuple::Fv);
                add(t, mnem, vec![rounding(def, EVEX_SAE, l)]);
            }
        }
        for (flav, w, memw) in [("ss", false, 4u8), ("sd", true, 8)] {
            add(
                t,
                leak(format!("v{stem}{flav}")),
                vec![
                    ev(
                        vec![
                            Op::V(Vk::Xmm),
                            Op::Nds(Vk::Xmm),
                            Op::Vm(Vk::Xmm, memw),
                            Op::Imm(1),
                        ],
                        0x66,
                        &[0x0f, 0x3a, sop],
                        128,
                        w,
                        Tuple::T1s,
                    )
                    .flags(EVEX_SAE),
                ],
            );
        }
    }

    // Mask-to-vector and vector-to-mask for dwords and qwords.
    for (to_m, from_m, w) in [
        ("vpmovd2m", "vpmovm2d", false),
        ("vpmovq2m", "vpmovm2q", true),
    ] {
        add_mask_moves(t, to_m, from_m, 0x38, w);
    }

    // Sub-vector broadcasts, inserts and extracts DQ adds to the `32x4`/`64x4`
    // pair AVX-512F already had: two dwords or qwords, and eight dwords.
    for (mnem, op, sub, w, tuple, lens) in [
        (
            "vbroadcasti32x2",
            0x59u8,
            Vk::Xmm,
            false,
            Tuple::T2,
            &[128u16, 256, 512] as &[u16],
        ),
        (
            "vbroadcastf32x2",
            0x19,
            Vk::Xmm,
            false,
            Tuple::T2,
            &[256, 512],
        ),
        (
            "vbroadcastf64x2",
            0x1a,
            Vk::Xmm,
            true,
            Tuple::T2,
            &[256, 512],
        ),
        (
            "vbroadcasti64x2",
            0x5a,
            Vk::Xmm,
            true,
            Tuple::T2,
            &[256, 512],
        ),
        ("vbroadcastf32x8", 0x1b, Vk::Ymm, false, Tuple::T8, &[512]),
        ("vbroadcasti32x8", 0x5b, Vk::Ymm, false, Tuple::T8, &[512]),
    ] {
        // Two or eight elements of the row's width.
        let elem = if w { 8 } else { 4 };
        let memw = elem * if tuple == Tuple::T2 { 2 } else { 8 };
        for &l in lens {
            add(
                t,
                mnem,
                vec![ev(
                    vec![Op::V(vk(l)), Op::Vm(sub, memw)],
                    0x66,
                    &[0x0f, 0x38, op],
                    l,
                    w,
                    tuple,
                )],
            );
        }
    }
    for (stem, fop, iop, sub, w, tuple) in [
        ("64x2", 0x18u8, 0x38u8, Vk::Xmm, true, Tuple::T2),
        ("32x8", 0x1a, 0x3a, Vk::Ymm, false, Tuple::T8),
    ] {
        let lens: &[u16] = if sub == Vk::Ymm { &[512] } else { &[256, 512] };
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
                        tuple,
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
                        tuple,
                    )],
                );
            }
        }
    }
}

/// `vpmovb2m`/`vpmovm2b` and their relatives: one mask bit per element. The
/// byte and dword directions share opcodes with the word and qword ones and
/// are split by `W`: `29`/`28` for BW's pair, `39`/`38` for DQ's.
fn add_mask_moves(t: &mut Tbl, to_m: &'static str, from_m: &'static str, base: u8, w: bool) {
    for l in ALL {
        let k = vk(l);
        add(
            t,
            to_m,
            vec![
                ev(
                    vec![Op::V(Vk::K), Op::V(k)],
                    0xf3,
                    &[0x0f, 0x38, base + 1],
                    l,
                    w,
                    Tuple::None,
                )
                .flags(NOMASK),
            ],
        );
        add(
            t,
            from_m,
            vec![
                ev(
                    vec![Op::V(k), Op::V(Vk::K)],
                    0xf3,
                    &[0x0f, 0x38, base],
                    l,
                    w,
                    Tuple::None,
                )
                .flags(NOMASK),
            ],
        );
    }
}

/// Rounding or exception control exists on a 512-bit packed form, never on a
/// shorter one.
fn rounding(def: Def, flag: u32, vlen: u16) -> Def {
    if vlen == 512 { def.flags(flag) } else { def }
}

// ---------------------------------------------------------------------------
// AVX-512CD and the single-purpose extensions
// ---------------------------------------------------------------------------

fn install_cd(t: &mut Tbl) {
    for (stem, op) in [("vplzcnt", 0x44u8), ("vpconflict", 0xc4)] {
        for (flav, w) in [("d", false), ("q", true)] {
            let mnem = leak(format!("{stem}{flav}"));
            for l in ALL {
                add(
                    t,
                    mnem,
                    vec![ev(rm(vk(l)), 0x66, &[0x0f, 0x38, op], l, w, Tuple::Fv)],
                );
            }
        }
    }
    for (mnem, op, w) in [
        ("vpbroadcastmb2q", 0x2au8, true),
        ("vpbroadcastmw2d", 0x3a, false),
    ] {
        for l in ALL {
            add(
                t,
                mnem,
                vec![
                    ev(
                        vec![Op::V(vk(l)), Op::V(Vk::K)],
                        0xf3,
                        &[0x0f, 0x38, op],
                        l,
                        w,
                        Tuple::None,
                    )
                    .flags(NOMASK),
                ],
            );
        }
    }
}

fn install_late(t: &mut Tbl) {
    // VBMI: byte permutes, and the multi-shift gather.
    for (mnem, esc, w) in [
        ("vpermb", &[0x0fu8, 0x38, 0x8d] as &[u8], false),
        ("vpermi2b", &[0x0f, 0x38, 0x75], false),
        ("vpermt2b", &[0x0f, 0x38, 0x7d], false),
    ] {
        for l in ALL {
            add(t, mnem, vec![ev(nds(vk(l)), 0x66, esc, l, w, Tuple::Fvm)]);
        }
    }
    for l in ALL {
        add(
            t,
            "vpmultishiftqb",
            vec![ev(
                nds(vk(l)),
                0x66,
                &[0x0f, 0x38, 0x83],
                l,
                true,
                Tuple::Fv,
            )],
        );
    }

    // IFMA: the 52-bit multiply-accumulate halves.
    for (mnem, op) in [("vpmadd52luq", 0xb4u8), ("vpmadd52huq", 0xb5)] {
        for l in ALL {
            add(
                t,
                mnem,
                vec![ev(nds(vk(l)), 0x66, &[0x0f, 0x38, op], l, true, Tuple::Fv)],
            );
        }
    }

    // VNNI: the dot-product accumulators, with and without saturation.
    for (mnem, op) in [
        ("vpdpbusd", 0x50u8),
        ("vpdpbusds", 0x51),
        ("vpdpwssd", 0x52),
        ("vpdpwssds", 0x53),
    ] {
        for l in ALL {
            add(
                t,
                mnem,
                vec![ev(nds(vk(l)), 0x66, &[0x0f, 0x38, op], l, false, Tuple::Fv)],
            );
        }
    }

    // VBMI2: byte and word compress/expand, whose tuple is one element.
    for (mnem, op, w, tuple) in [
        ("vpcompressb", 0x63u8, false, Tuple::T1s8),
        ("vpcompressw", 0x63, true, Tuple::T1s16),
        ("vpexpandb", 0x62, false, Tuple::T1s8),
        ("vpexpandw", 0x62, true, Tuple::T1s16),
    ] {
        for l in ALL {
            let k = vk(l);
            let ops = if mnem.contains("expand") {
                vec![Op::V(k), Op::Vm(k, 0)]
            } else {
                vec![Op::Vm(k, 0), Op::V(k)]
            };
            add(t, mnem, vec![ev(ops, 0x66, &[0x0f, 0x38, op], l, w, tuple)]);
        }
    }
    // The double shifts: the immediate forms on the `0F 3A` map and the
    // variable ones eight opcodes lower on `0F 38`. The word forms sit one
    // opcode below the dword/qword pair, which share theirs and split on `W`.
    for (stem, wop, dop) in [("vpshld", 0x70u8, 0x71u8), ("vpshrd", 0x72, 0x73)] {
        for (flav, op, w, tuple) in [
            ("w", wop, true, Tuple::Fvm),
            ("d", dop, false, Tuple::Fv),
            ("q", dop, true, Tuple::Fv),
        ] {
            let imm_name = leak(format!("{stem}{flav}"));
            let var_name = leak(format!("{stem}v{flav}"));
            for l in ALL {
                add(
                    t,
                    imm_name,
                    vec![ev(
                        with_imm(nds(vk(l))),
                        0x66,
                        &[0x0f, 0x3a, op],
                        l,
                        w,
                        tuple,
                    )],
                );
                add(
                    t,
                    var_name,
                    vec![ev(nds(vk(l)), 0x66, &[0x0f, 0x38, op], l, w, tuple)],
                );
            }
        }
    }

    // BITALG and VPOPCNTDQ: population counts, and the bit gather into a mask.
    for (flav, op, w, tuple) in [
        ("b", 0x54u8, false, Tuple::Fvm),
        ("w", 0x54, true, Tuple::Fvm),
        ("d", 0x55, false, Tuple::Fv),
        ("q", 0x55, true, Tuple::Fv),
    ] {
        let mnem = leak(format!("vpopcnt{flav}"));
        for l in ALL {
            add(
                t,
                mnem,
                vec![ev(rm(vk(l)), 0x66, &[0x0f, 0x38, op], l, w, tuple)],
            );
        }
    }
    for l in ALL {
        add(
            t,
            "vpshufbitqmb",
            vec![ev(
                to_mask(vk(l)),
                0x66,
                &[0x0f, 0x38, 0x8f],
                l,
                false,
                Tuple::Fvm,
            )],
        );
    }

    // VP2INTERSECT writes a pair of masks, named by the first of the two.
    for (mnem, w) in [("vp2intersectd", false), ("vp2intersectq", true)] {
        for l in ALL {
            add(
                t,
                mnem,
                vec![ev(to_mask(vk(l)), 0xf2, &[0x0f, 0x38, 0x68], l, w, Tuple::Fv).flags(NOMASK)],
            );
        }
    }

    // BF16.
    for l in ALL {
        add(
            t,
            "vcvtne2ps2bf16",
            vec![ev(
                nds(vk(l)),
                0xf2,
                &[0x0f, 0x38, 0x72],
                l,
                false,
                Tuple::Fv,
            )],
        );
        add(
            t,
            "vdpbf16ps",
            vec![ev(
                nds(vk(l)),
                0xf3,
                &[0x0f, 0x38, 0x52],
                l,
                false,
                Tuple::Fv,
            )],
        );
        add(
            t,
            "vcvtneps2bf16",
            vec![ev(
                vec![Op::V(half(l)), Op::Vm(vk(l), 0)],
                0xf3,
                &[0x0f, 0x38, 0x72],
                l,
                false,
                Tuple::Fv,
            )],
        );
    }
}

// ---------------------------------------------------------------------------
// The rest of AVX-512F, and the extensions riding on its encodings
// ---------------------------------------------------------------------------

/// A scalar row: `op xmm, xmm, xmm/m{4,8}`, sized by `W`.
fn scalar(pfx: u8, esc: &[u8], w: bool) -> Def {
    let memw = if w { 8 } else { 4 };
    ev(
        vec![Op::V(Vk::Xmm), Op::Nds(Vk::Xmm), Op::Vm(Vk::Xmm, memw)],
        pfx,
        esc,
        128,
        w,
        Tuple::T1s,
    )
}

fn install_f_rest(t: &mut Tbl) {
    // Float operations with a packed pair (`ps`/`pd`, split by `W`) and a
    // scalar pair one opcode up. `vvvv` is used by the packed form only where
    // the operation has two inputs.
    #[rustfmt::skip]
    const FLOAT: &[(&str, &[u8], bool, bool, u32)] = &[
        // (stem, packed opcode, packed takes vvvv, has immediate, control)
        ("vgetexp",   &[0x0f, 0x38, 0x42], false, false, EVEX_SAE),
        ("vscalef",   &[0x0f, 0x38, 0x2c], true,  false, EVEX_ER),
        ("vrcp14",    &[0x0f, 0x38, 0x4c], false, false, 0),
        ("vrsqrt14",  &[0x0f, 0x38, 0x4e], false, false, 0),
        ("vgetmant",  &[0x0f, 0x3a, 0x26], false, true,  EVEX_SAE),
        ("vfixupimm", &[0x0f, 0x3a, 0x54], true,  true,  EVEX_SAE),
    ];
    for &(stem, esc, two, has_imm, ctl) in FLOAT {
        let (map_esc, op) = esc.split_at(esc.len() - 1);
        for (flav, w) in [("ps", false), ("pd", true)] {
            let mnem = leak(format!("{stem}{flav}"));
            for l in ALL {
                let mut ops = if two { nds(vk(l)) } else { rm(vk(l)) };
                if has_imm {
                    ops.push(Op::Imm(1));
                }
                let def = ev(ops, 0x66, esc, l, w, Tuple::Fv);
                add(t, mnem, vec![rounding(def, ctl, l)]);
            }
        }
        for (flav, w) in [("ss", false), ("sd", true)] {
            let mnem = leak(format!("{stem}{flav}"));
            let sesc = [map_esc, &[op[0] + 1]].concat();
            let mut def = scalar(0x66, &sesc, w).flags(ctl);
            if has_imm {
                def.ops.push(Op::Imm(1));
            }
            add(t, mnem, vec![def]);
        }
    }
    // `vrndscale` gives each of its four forms an opcode of its own.
    for (mnem, op, w) in [("vrndscaleps", 0x08u8, false), ("vrndscalepd", 0x09, true)] {
        for l in ALL {
            let def = ev(
                with_imm(rm(vk(l))),
                0x66,
                &[0x0f, 0x3a, op],
                l,
                w,
                Tuple::Fv,
            );
            add(t, mnem, vec![rounding(def, EVEX_SAE, l)]);
        }
    }
    for (mnem, op, w) in [("vrndscaless", 0x0au8, false), ("vrndscalesd", 0x0b, true)] {
        let mut def = scalar(0x66, &[0x0f, 0x3a, op], w).flags(EVEX_SAE);
        def.ops.push(Op::Imm(1));
        add(t, mnem, vec![def]);
    }

    // Blends by mask, aligning rotates, and the 128-bit-lane shuffles.
    for (mnem, esc, w) in [
        ("vblendmps", &[0x0fu8, 0x38, 0x65] as &[u8], false),
        ("vblendmpd", &[0x0f, 0x38, 0x65], true),
        ("vpblendmd", &[0x0f, 0x38, 0x64], false),
        ("vpblendmq", &[0x0f, 0x38, 0x64], true),
    ] {
        for l in ALL {
            add(t, mnem, vec![ev(nds(vk(l)), 0x66, esc, l, w, Tuple::Fv)]);
        }
    }
    for (mnem, w) in [("valignd", false), ("valignq", true)] {
        for l in ALL {
            add(
                t,
                mnem,
                vec![ev(
                    with_imm(nds(vk(l))),
                    0x66,
                    &[0x0f, 0x3a, 0x03],
                    l,
                    w,
                    Tuple::Fv,
                )],
            );
        }
    }
    for (mnem, op, w) in [
        ("vshuff32x4", 0x23u8, false),
        ("vshuff64x2", 0x23, true),
        ("vshufi32x4", 0x43, false),
        ("vshufi64x2", 0x43, true),
    ] {
        // A lane shuffle needs at least two lanes.
        for l in [256u16, 512] {
            add(
                t,
                mnem,
                vec![ev(
                    with_imm(nds(vk(l))),
                    0x66,
                    &[0x0f, 0x3a, op],
                    l,
                    w,
                    Tuple::Fv,
                )],
            );
        }
    }

    // Compress and expand: the memory side holds as many elements as were
    // selected, so its tuple is a single element.
    for (stem, op) in [("vcompress", 0x8au8), ("vpcompress", 0x8b)] {
        for (flav, w) in [
            (if op == 0x8a { "ps" } else { "d" }, false),
            (if op == 0x8a { "pd" } else { "q" }, true),
        ] {
            let mnem = leak(format!("{stem}{flav}"));
            for l in ALL {
                add(
                    t,
                    mnem,
                    vec![ev(
                        vec![Op::Vm(vk(l), 0), Op::V(vk(l))],
                        0x66,
                        &[0x0f, 0x38, op],
                        l,
                        w,
                        Tuple::T1s,
                    )],
                );
            }
        }
    }
    for (stem, op) in [("vexpand", 0x88u8), ("vpexpand", 0x89)] {
        for (flav, w) in [
            (if op == 0x88 { "ps" } else { "d" }, false),
            (if op == 0x88 { "pd" } else { "q" }, true),
        ] {
            let mnem = leak(format!("{stem}{flav}"));
            for l in ALL {
                add(
                    t,
                    mnem,
                    vec![ev(rm(vk(l)), 0x66, &[0x0f, 0x38, op], l, w, Tuple::T1s)],
                );
            }
        }
    }

    // Shifts by the count in an `xmm`. The immediate forms are in
    // `avx512.rs`; `vpsraq` had no AVX2 form to inherit, so it is here whole.
    for (mnem, op, w) in [
        ("vpslld", 0xf2u8, false),
        ("vpsrld", 0xd2, false),
        ("vpsrad", 0xe2, false),
        ("vpsllq", 0xf3, true),
        ("vpsrlq", 0xd3, true),
        ("vpsraq", 0xe2, true),
    ] {
        for l in ALL {
            let k = vk(l);
            add(
                t,
                mnem,
                vec![ev(
                    vec![Op::V(k), Op::Nds(k), Op::Vm(Vk::Xmm, 16)],
                    0x66,
                    &[0x0f, op],
                    l,
                    w,
                    Tuple::M128,
                )],
            );
        }
    }

    // Unsigned integer conversions.
    for (mnem, pfx, op, w, ctl) in [
        ("vcvtps2udq", 0x00u8, 0x79u8, false, EVEX_ER),
        ("vcvttps2udq", 0x00, 0x78, false, EVEX_SAE),
        ("vcvtudq2ps", 0xf2, 0x7a, false, EVEX_ER),
    ] {
        for l in ALL {
            let def = ev(rm(vk(l)), pfx, &[0x0f, op], l, w, Tuple::Fv);
            add(t, mnem, vec![rounding(def, ctl, l)]);
        }
    }
    for (mnem, op, ctl) in [
        ("vcvtpd2udq", 0x79u8, EVEX_ER),
        ("vcvttpd2udq", 0x78, EVEX_SAE),
    ] {
        for l in ALL {
            let def = ev(
                vec![Op::V(half(l)), Op::Vm(vk(l), 0)],
                0x00,
                &[0x0f, op],
                l,
                true,
                Tuple::Fv,
            );
            add(t, mnem, vec![rounding(def, ctl, l)]);
        }
    }
    for l in ALL {
        add(
            t,
            "vcvtudq2pd",
            vec![ev(
                vec![Op::V(vk(l)), Op::Vm(half(l), (l / 16) as u8)],
                0xf3,
                &[0x0f, 0x7a],
                l,
                false,
                Tuple::Hv,
            )],
        );
    }
    // Scalar to and from an unsigned GPR.
    for (mnem, pfx, op, ctl) in [
        ("vcvtss2usi", 0xf3u8, 0x79u8, EVEX_ER),
        ("vcvtsd2usi", 0xf2, 0x79, EVEX_ER),
        ("vcvttss2usi", 0xf3, 0x78, EVEX_SAE),
        ("vcvttsd2usi", 0xf2, 0x78, EVEX_SAE),
    ] {
        let memw = if pfx == 0xf3 { 4 } else { 8 };
        for gpr in [4u8, 8] {
            let mut def = ev(
                vec![Op::R(gpr), Op::Vm(Vk::Xmm, memw)],
                pfx,
                &[0x0f, op],
                128,
                gpr == 8,
                Tuple::T1s,
            )
            .flags(ctl | NOMASK);
            // The tuple follows the scalar, not the register `W` selects.
            def.tuple = if memw == 4 {
                Tuple::T1s32
            } else {
                Tuple::T1s64
            };
            def.opsize = gpr * 8;
            add(t, mnem, vec![def]);
        }
    }
    for (mnem, pfx) in [("vcvtusi2ss", 0xf3u8), ("vcvtusi2sd", 0xf2)] {
        for (w, opsize) in [(4u8, 32u8), (8, 64)] {
            let mut def = ev(
                vec![Op::V(Vk::Xmm), Op::Nds(Vk::Xmm), Op::Rm(w)],
                pfx,
                &[0x0f, 0x7b],
                128,
                w == 8,
                Tuple::T1s,
            )
            .flags(EVEX_ER | NOMASK);
            def.opsize = opsize;
            add(t, mnem, vec![def]);
        }
    }

    // Saturating narrows. As with the truncating ones in `avx512.rs`, the
    // destination is a fraction of the source register.
    for (mnem, op, tuple, frac) in [
        ("vpmovsdb", 0x21u8, Tuple::Qvm, 4u16),
        ("vpmovusdb", 0x11, Tuple::Qvm, 4),
        ("vpmovsdw", 0x23, Tuple::Hvm, 2),
        ("vpmovusdw", 0x13, Tuple::Hvm, 2),
        ("vpmovsqb", 0x22, Tuple::Ovm, 8),
        ("vpmovusqb", 0x12, Tuple::Ovm, 8),
        ("vpmovsqd", 0x25, Tuple::Hvm, 2),
        ("vpmovusqd", 0x15, Tuple::Hvm, 2),
        ("vpmovsqw", 0x24, Tuple::Qvm, 4),
        ("vpmovusqw", 0x14, Tuple::Qvm, 4),
    ] {
        for l in ALL {
            let dst_bytes = (l / 8 / frac) as u8;
            let dk = if dst_bytes > 16 { Vk::Ymm } else { Vk::Xmm };
            add(
                t,
                mnem,
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

    // The 128-bit AVX instructions that exist as EVEX only to reach
    // `xmm16`-`xmm31`. None takes a writemask.
    let x = Vk::Xmm;
    add(
        t,
        "vinsertps",
        vec![
            ev(
                vec![Op::V(x), Op::Nds(x), Op::Vm(x, 4), Op::Imm(1)],
                0x66,
                &[0x0f, 0x3a, 0x21],
                128,
                false,
                Tuple::T1s32,
            )
            .flags(NOMASK),
        ],
    );
    for gpr in [Op::Rm(4), Op::R(8)] {
        add(
            t,
            "vextractps",
            vec![
                ev(
                    vec![gpr, Op::V(x), Op::Imm(1)],
                    0x66,
                    &[0x0f, 0x3a, 0x17],
                    128,
                    false,
                    Tuple::T1s32,
                )
                .flags(NOMASK | R_IN_RM),
            ],
        );
    }
    for (mnem, w) in [("vpextrd", 4u8), ("vpextrq", 8)] {
        add(
            t,
            mnem,
            vec![
                ev(
                    vec![Op::Rm(w), Op::V(x), Op::Imm(1)],
                    0x66,
                    &[0x0f, 0x3a, 0x16],
                    128,
                    w == 8,
                    Tuple::T1s,
                )
                .flags(NOMASK),
            ],
        );
    }
    for (mnem, w) in [("vpinsrd", 4u8), ("vpinsrq", 8)] {
        add(
            t,
            mnem,
            vec![
                ev(
                    vec![Op::V(x), Op::Nds(x), Op::Rm(w), Op::Imm(1)],
                    0x66,
                    &[0x0f, 0x3a, 0x22],
                    128,
                    w == 8,
                    Tuple::T1s,
                )
                .flags(NOMASK),
            ],
        );
    }
    // Scalar to a signed GPR; `W` sizes the register, the tuple the scalar.
    for (mnem, pfx, op, ctl) in [
        ("vcvtss2si", 0xf3u8, 0x2du8, EVEX_ER),
        ("vcvtsd2si", 0xf2, 0x2d, EVEX_ER),
        ("vcvttss2si", 0xf3, 0x2c, EVEX_SAE),
        ("vcvttsd2si", 0xf2, 0x2c, EVEX_SAE),
    ] {
        let tuple = if pfx == 0xf3 {
            Tuple::T1s32
        } else {
            Tuple::T1s64
        };
        let memw = if pfx == 0xf3 { 4 } else { 8 };
        for gpr in [4u8, 8] {
            let mut def = ev(
                vec![Op::R(gpr), Op::Vm(x, memw)],
                pfx,
                &[0x0f, op],
                128,
                gpr == 8,
                tuple,
            )
            .flags(ctl | NOMASK);
            def.opsize = gpr * 8;
            add(t, mnem, vec![def]);
        }
    }
    // The half-register moves: a quadword of memory in or out, or the two
    // register-only halves.
    // Eight bytes either way: two singles or one double.
    for (stem, pfx, w, tuple) in [
        ("ps", 0x00u8, false, Tuple::T2),
        ("pd", 0x66, true, Tuple::T1s),
    ] {
        for (dir, load) in [("h", 0x16u8), ("l", 0x12)] {
            add(
                t,
                leak(format!("vmov{dir}{stem}")),
                vec![
                    ev(
                        vec![Op::V(x), Op::Nds(x), Op::M(8)],
                        pfx,
                        &[0x0f, load],
                        128,
                        w,
                        tuple,
                    )
                    .flags(NOMASK),
                    ev(
                        vec![Op::M(8), Op::V(x)],
                        pfx,
                        &[0x0f, load + 1],
                        128,
                        w,
                        tuple,
                    )
                    .flags(NOMASK),
                ],
            );
        }
    }
    for (mnem, op) in [("vmovhlps", 0x12u8), ("vmovlhps", 0x16)] {
        add(
            t,
            mnem,
            vec![
                ev(
                    vec![Op::V(x), Op::Nds(x), Op::V(x)],
                    0x00,
                    &[0x0f, op],
                    128,
                    false,
                    Tuple::None,
                )
                .flags(NOMASK),
            ],
        );
    }
    for l in ALL {
        add(
            t,
            "vmovntdqa",
            vec![
                ev(
                    vec![Op::V(vk(l)), Op::M((l / 8) as u8)],
                    0x66,
                    &[0x0f, 0x38, 0x2a],
                    l,
                    false,
                    Tuple::Fvm,
                )
                .flags(NOMASK),
            ],
        );
    }

    // F16C at every length.
    for l in ALL {
        let (half_k, half_bytes) = (half(l), (l / 16) as u8);
        let ctl = if l == 512 { EVEX_SAE } else { 0 };
        add(
            t,
            "vcvtph2ps",
            vec![
                ev(
                    vec![Op::V(vk(l)), Op::Vm(half_k, half_bytes)],
                    0x66,
                    &[0x0f, 0x38, 0x13],
                    l,
                    false,
                    Tuple::Hvm,
                )
                .flags(ctl),
            ],
        );
        add(
            t,
            "vcvtps2ph",
            vec![
                ev(
                    vec![Op::Vm(half_k, half_bytes), Op::V(vk(l)), Op::Imm(1)],
                    0x66,
                    &[0x0f, 0x3a, 0x1d],
                    l,
                    false,
                    Tuple::Hvm,
                )
                .flags(ctl),
            ],
        );
    }

    // GFNI, VAES and VPCLMULQDQ at every length. The crypto rows take no
    // writemask.
    for (mnem, op) in [("vgf2p8affineqb", 0xceu8), ("vgf2p8affineinvqb", 0xcf)] {
        for l in ALL {
            add(
                t,
                mnem,
                vec![ev(
                    with_imm(nds(vk(l))),
                    0x66,
                    &[0x0f, 0x3a, op],
                    l,
                    true,
                    Tuple::Fv,
                )],
            );
        }
    }
    for l in ALL {
        add(
            t,
            "vgf2p8mulb",
            vec![ev(
                nds(vk(l)),
                0x66,
                &[0x0f, 0x38, 0xcf],
                l,
                false,
                Tuple::Fvm,
            )],
        );
        for (mnem, op) in [
            ("vaesenc", 0xdcu8),
            ("vaesenclast", 0xdd),
            ("vaesdec", 0xde),
            ("vaesdeclast", 0xdf),
        ] {
            add(
                t,
                mnem,
                vec![ev(nds(vk(l)), 0x66, &[0x0f, 0x38, op], l, false, Tuple::Fvm).flags(NOMASK)],
            );
        }
        add(
            t,
            "vpclmulqdq",
            vec![
                ev(
                    with_imm(nds(vk(l))),
                    0x66,
                    &[0x0f, 0x3a, 0x44],
                    l,
                    false,
                    Tuple::Fvm,
                )
                .flags(NOMASK),
            ],
        );
    }

    // AVX-512ER, the Xeon Phi exponent and high-precision reciprocals: 512
    // bits and scalars only, with exception suppression everywhere.
    for (stem, op, packed_only) in [
        ("vexp2", 0xc8u8, true),
        ("vrcp28", 0xca, false),
        ("vrsqrt28", 0xcc, false),
    ] {
        for (flav, w) in [("ps", false), ("pd", true)] {
            add(
                t,
                leak(format!("{stem}{flav}")),
                vec![ev(rm(Vk::Zmm), 0x66, &[0x0f, 0x38, op], 512, w, Tuple::Fv).flags(EVEX_SAE)],
            );
        }
        if packed_only {
            continue;
        }
        for (flav, w) in [("ss", false), ("sd", true)] {
            add(
                t,
                leak(format!("{stem}{flav}")),
                vec![scalar(0x66, &[0x0f, 0x38, op + 1], w).flags(EVEX_SAE)],
            );
        }
    }
    // AVX-512PF, the Xeon Phi gather and scatter prefetches: a VSIB memory
    // operand alone, and the writemask the gathers already insist on.
    for (stem, ext) in [
        ("vgatherpf0", 1u8),
        ("vgatherpf1", 2),
        ("vscatterpf0", 5),
        ("vscatterpf1", 6),
    ] {
        for (flav, op, index, w) in [
            ("dps", 0xc6u8, Vk::Zmm, false),
            ("qps", 0xc7, Vk::Zmm, false),
            ("dpd", 0xc6, Vk::Ymm, true),
            ("qpd", 0xc7, Vk::Zmm, true),
        ] {
            add(
                t,
                leak(format!("{stem}{flav}")),
                vec![
                    d(
                        vec![Op::Vsib(index)],
                        &[op],
                        ModRm::Ext(ext),
                        if w { 64 } else { 0 },
                    )
                    .pfx(0x66)
                    .map(2)
                    .evex(512, Tuple::T1s)
                    .flags(super::NEEDS_MASK),
                ],
            );
        }
    }
}

pub fn install(t: &mut Tbl) {
    install_bw(t);
    install_dq(t);
    install_cd(t);
    install_late(t);
    install_f_rest(t);
}
