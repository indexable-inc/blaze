//! AVX, AVX2 and FMA: the VEX-encoded families.
//!
//! VEX folds the legacy prefixes and escapes into two or three bytes: `pp`
//! stands in for a `66`/`F3`/`F2` mandatory prefix, `mmmmm` for the `0F`,
//! `0F 38` or `0F 3A` escape, `L` selects 128 or 256 bits, and `vvvv` carries
//! the extra, non-destructive source operand that SSE never had room for.
//!
//! Rows here are written with the *legacy* opcode bytes, escape included, so
//! they read exactly like their SSE counterparts; [`vex`] splits the escape
//! back out into `map`.

use super::mmx::{PACKED_BINARY, SHIFT_IMM};
use super::{Def, ModRm, Op, R_IN_RM, Tbl, Vk, add, d};

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// Splits legacy opcode bytes into a VEX/EVEX map number and the final byte.
pub fn split_escape(bytes: &[u8]) -> (u8, u8) {
    match bytes {
        [0x0f, 0x38, op] => (2, *op),
        [0x0f, 0x3a, op] => (3, *op),
        [0x0f, op] => (1, *op),
        _ => panic!("VEX/EVEX rows are written as 0F, 0F 38 or 0F 3A opcodes"),
    }
}

/// The register class a vector length implies.
pub fn vk(vlen: u16) -> Vk {
    match vlen {
        128 => Vk::Xmm,
        256 => Vk::Ymm,
        _ => Vk::Zmm,
    }
}

/// A VEX row. `w` is `VEX.W`; everything else as for a legacy row.
pub fn vex(ops: Vec<Op>, pfx: u8, esc: &[u8], vlen: u16, w: bool) -> Def {
    let (map, op) = split_escape(esc);
    d(ops, &[op], ModRm::Reg, if w { 64 } else { 0 })
        .pfx(pfx)
        .map(map)
        .vex(vlen)
}

/// `op dst, src1, src2/m`: the three-operand shape most of AVX has.
pub fn nds(k: Vk, memw: u8) -> Vec<Op> {
    vec![Op::V(k), Op::Nds(k), Op::Vm(k, memw)]
}

/// `op dst, src/m`: no `vvvv` operand.
pub fn rm(k: Vk, memw: u8) -> Vec<Op> {
    vec![Op::V(k), Op::Vm(k, memw)]
}

/// `op dst/m, src`: the store direction.
pub fn mr(k: Vk, memw: u8) -> Vec<Op> {
    vec![Op::Vm(k, memw), Op::V(k)]
}

/// Appends a trailing `imm8` to an operand list.
pub fn imm(mut ops: Vec<Op>) -> Vec<Op> {
    ops.push(Op::Imm(1));
    ops
}

const BOTH: &[u16] = &[128, 256];
const X128: &[u16] = &[128];
const Y256: &[u16] = &[256];

/// Adds one row per vector length for a shape that scales with it.
fn each(
    t: &mut Tbl,
    mnem: &'static str,
    lens: &[u16],
    pfx: u8,
    esc: &[u8],
    w: bool,
    shape: fn(Vk, u8) -> Vec<Op>,
) {
    for &l in lens {
        add(t, mnem, vec![vex(shape(vk(l), 0), pfx, esc, l, w)]);
    }
}

fn install_float(t: &mut Tbl) {
    const FLAVOURS: [(&str, u8, u8); 4] = [
        ("ps", 0x00, 0),
        ("pd", 0x66, 0),
        ("ss", 0xf3, 4),
        ("sd", 0xf2, 8),
    ];
    #[rustfmt::skip]
    const OPS: &[(&str, u8, &[&str])] = &[
        ("add", 0x58, &["ps", "pd", "ss", "sd"]), ("mul", 0x59, &["ps", "pd", "ss", "sd"]),
        ("sub", 0x5c, &["ps", "pd", "ss", "sd"]), ("min", 0x5d, &["ps", "pd", "ss", "sd"]),
        ("div", 0x5e, &["ps", "pd", "ss", "sd"]), ("max", 0x5f, &["ps", "pd", "ss", "sd"]),
        ("and", 0x54, &["ps", "pd"]), ("andn", 0x55, &["ps", "pd"]),
        ("or", 0x56, &["ps", "pd"]), ("xor", 0x57, &["ps", "pd"]),
        // The scalar forms of these take `vvvv`; the packed ones do not.
        ("sqrt", 0x51, &["ps", "pd", "ss", "sd"]),
        ("rsqrt", 0x52, &["ps", "ss"]), ("rcp", 0x53, &["ps", "ss"]),
    ];
    for &(stem, op, which) in OPS {
        let unary = matches!(stem, "sqrt" | "rsqrt" | "rcp");
        for (flav, pfx, memw) in FLAVOURS {
            if !which.contains(&flav) {
                continue;
            }
            let mnem = leak(format!("v{stem}{flav}"));
            if memw != 0 {
                // Scalar: 128 bits only, and `L` is ignored, so it is left 0.
                add(
                    t,
                    mnem,
                    vec![vex(nds(Vk::Xmm, memw), pfx, &[0x0f, op], 128, false)],
                );
            } else if unary {
                each(t, mnem, BOTH, pfx, &[0x0f, op], false, rm);
            } else {
                each(t, mnem, BOTH, pfx, &[0x0f, op], false, nds);
            }
        }
    }

    // Compares with a predicate immediate.
    for (flav, pfx, memw) in FLAVOURS {
        let mnem = leak(format!("vcmp{flav}"));
        if memw != 0 {
            add(
                t,
                mnem,
                vec![vex(imm(nds(Vk::Xmm, memw)), pfx, &[0x0f, 0xc2], 128, false)],
            );
        } else {
            for l in [128, 256] {
                add(
                    t,
                    mnem,
                    vec![vex(imm(nds(vk(l), 0)), pfx, &[0x0f, 0xc2], l, false)],
                );
            }
        }
    }
    for (mnem, pfx, op, memw) in [
        ("vucomiss", 0x00u8, 0x2eu8, 4u8),
        ("vucomisd", 0x66, 0x2e, 8),
        ("vcomiss", 0x00, 0x2f, 4),
        ("vcomisd", 0x66, 0x2f, 8),
    ] {
        add(
            t,
            mnem,
            vec![vex(rm(Vk::Xmm, memw), pfx, &[0x0f, op], 128, false)],
        );
    }

    // SSE3 horizontal arithmetic.
    for (mnem, pfx, op) in [
        ("vaddsubps", 0xf2u8, 0xd0u8),
        ("vaddsubpd", 0x66, 0xd0),
        ("vhaddps", 0xf2, 0x7c),
        ("vhaddpd", 0x66, 0x7c),
        ("vhsubps", 0xf2, 0x7d),
        ("vhsubpd", 0x66, 0x7d),
    ] {
        each(t, mnem, BOTH, pfx, &[0x0f, op], false, nds);
    }

    // Unpacks and shuffles.
    for (mnem, pfx, op) in [
        ("vunpcklps", 0x00u8, 0x14u8),
        ("vunpcklpd", 0x66, 0x14),
        ("vunpckhps", 0x00, 0x15),
        ("vunpckhpd", 0x66, 0x15),
    ] {
        each(t, mnem, BOTH, pfx, &[0x0f, op], false, nds);
    }
    for (mnem, pfx) in [("vshufps", 0x00u8), ("vshufpd", 0x66)] {
        for l in [128, 256] {
            add(
                t,
                mnem,
                vec![vex(imm(nds(vk(l), 0)), pfx, &[0x0f, 0xc6], l, false)],
            );
        }
    }

    // SSE4.1 blends, rounds and dot products.
    for (mnem, op, lens) in [
        ("vblendps", 0x0cu8, BOTH),
        ("vblendpd", 0x0d, BOTH),
        ("vdpps", 0x40, BOTH),
        ("vdppd", 0x41, X128),
    ] {
        for &l in lens {
            add(
                t,
                mnem,
                vec![vex(imm(nds(vk(l), 0)), 0x66, &[0x0f, 0x3a, op], l, false)],
            );
        }
    }
    for (mnem, op) in [("vroundps", 0x08u8), ("vroundpd", 0x09)] {
        for l in [128, 256] {
            add(
                t,
                mnem,
                vec![vex(imm(rm(vk(l), 0)), 0x66, &[0x0f, 0x3a, op], l, false)],
            );
        }
    }
    for (mnem, op, memw) in [("vroundss", 0x0au8, 4u8), ("vroundsd", 0x0b, 8)] {
        add(
            t,
            mnem,
            vec![vex(
                imm(nds(Vk::Xmm, memw)),
                0x66,
                &[0x0f, 0x3a, op],
                128,
                false,
            )],
        );
    }
    // The variable blends name their selector register in an immediate byte.
    for (mnem, op) in [
        ("vblendvps", 0x4au8),
        ("vblendvpd", 0x4b),
        ("vpblendvb", 0x4c),
    ] {
        for l in [128, 256] {
            let k = vk(l);
            add(
                t,
                mnem,
                vec![vex(
                    vec![Op::V(k), Op::Nds(k), Op::Vm(k, 0), Op::Is4(k)],
                    0x66,
                    &[0x0f, 0x3a, op],
                    l,
                    false,
                )],
            );
        }
    }
    add(
        t,
        "vinsertps",
        vec![vex(
            imm(nds(Vk::Xmm, 4)),
            0x66,
            &[0x0f, 0x3a, 0x21],
            128,
            false,
        )],
    );
    add(
        t,
        "vextractps",
        vec![
            vex(
                vec![Op::Rm(4), Op::V(Vk::Xmm), Op::Imm(1)],
                0x66,
                &[0x0f, 0x3a, 0x17],
                128,
                false,
            ),
            vex(
                vec![Op::R(8), Op::V(Vk::Xmm), Op::Imm(1)],
                0x66,
                &[0x0f, 0x3a, 0x17],
                128,
                false,
            )
            .flags(R_IN_RM),
        ],
    );
}

fn install_moves(t: &mut Tbl) {
    // Aligned and unaligned moves. The store opcode is listed as an
    // alternative for register-to-register moves too: when the source is one
    // of `xmm8`-`xmm15` and the destination is not, using it moves the
    // extended register from `B` to `R`, which the two-byte VEX can carry.
    for (mnem, pfx, load, store) in [
        ("vmovaps", 0x00u8, 0x28u8, 0x29u8),
        ("vmovapd", 0x66, 0x28, 0x29),
        ("vmovups", 0x00, 0x10, 0x11),
        ("vmovupd", 0x66, 0x10, 0x11),
        ("vmovdqa", 0x66, 0x6f, 0x7f),
        ("vmovdqu", 0xf3, 0x6f, 0x7f),
    ] {
        each(t, mnem, BOTH, pfx, &[0x0f, load], false, rm);
        each(t, mnem, BOTH, pfx, &[0x0f, store], false, mr);
    }

    // Scalar moves have three shapes: load, store, and a register-only form
    // that merges the upper element from `vvvv`.
    for (mnem, pfx, w) in [("vmovss", 0xf3u8, 4u8), ("vmovsd", 0xf2, 8)] {
        add(
            t,
            mnem,
            vec![
                vex(
                    vec![Op::V(Vk::Xmm), Op::M(w)],
                    pfx,
                    &[0x0f, 0x10],
                    128,
                    false,
                ),
                vex(
                    vec![Op::M(w), Op::V(Vk::Xmm)],
                    pfx,
                    &[0x0f, 0x11],
                    128,
                    false,
                ),
                vex(
                    vec![Op::V(Vk::Xmm), Op::Nds(Vk::Xmm), Op::V(Vk::Xmm)],
                    pfx,
                    &[0x0f, 0x10],
                    128,
                    false,
                ),
                // `11 /r` with the destination in r/m; see the note above.
                d(
                    vec![Op::Vm(Vk::Xmm, w), Op::Nds(Vk::Xmm), Op::V(Vk::Xmm)],
                    &[0x11],
                    ModRm::Reg,
                    0,
                )
                .pfx(pfx)
                .map(1)
                .vex(128),
            ],
        );
    }

    add(
        t,
        "vmovd",
        vec![
            vex(
                vec![Op::V(Vk::Xmm), Op::Rm(4)],
                0x66,
                &[0x0f, 0x6e],
                128,
                false,
            ),
            vex(
                vec![Op::Rm(4), Op::V(Vk::Xmm)],
                0x66,
                &[0x0f, 0x7e],
                128,
                false,
            ),
            vex(
                vec![Op::V(Vk::Xmm), Op::R(8)],
                0x66,
                &[0x0f, 0x6e],
                128,
                true,
            )
            .flags(R_IN_RM),
            vex(
                vec![Op::R(8), Op::V(Vk::Xmm)],
                0x66,
                &[0x0f, 0x7e],
                128,
                true,
            )
            .flags(R_IN_RM),
        ],
    );
    add(
        t,
        "vmovq",
        vec![
            vex(rm(Vk::Xmm, 8), 0xf3, &[0x0f, 0x7e], 128, false),
            vex(mr(Vk::Xmm, 8), 0x66, &[0x0f, 0xd6], 128, false),
            vex(
                vec![Op::V(Vk::Xmm), Op::Rm(8)],
                0x66,
                &[0x0f, 0x6e],
                128,
                true,
            ),
            vex(
                vec![Op::Rm(8), Op::V(Vk::Xmm)],
                0x66,
                &[0x0f, 0x7e],
                128,
                true,
            ),
        ],
    );

    add(
        t,
        "vmovhlps",
        vec![vex(
            vec![Op::V(Vk::Xmm), Op::Nds(Vk::Xmm), Op::V(Vk::Xmm)],
            0x00,
            &[0x0f, 0x12],
            128,
            false,
        )],
    );
    add(
        t,
        "vmovlhps",
        vec![vex(
            vec![Op::V(Vk::Xmm), Op::Nds(Vk::Xmm), Op::V(Vk::Xmm)],
            0x00,
            &[0x0f, 0x16],
            128,
            false,
        )],
    );
    for (mnem, pfx, load) in [
        ("vmovlps", 0x00u8, 0x12u8),
        ("vmovlpd", 0x66, 0x12),
        ("vmovhps", 0x00, 0x16),
        ("vmovhpd", 0x66, 0x16),
    ] {
        add(
            t,
            mnem,
            vec![
                vex(
                    vec![Op::V(Vk::Xmm), Op::Nds(Vk::Xmm), Op::M(8)],
                    pfx,
                    &[0x0f, load],
                    128,
                    false,
                ),
                vex(
                    vec![Op::M(8), Op::V(Vk::Xmm)],
                    pfx,
                    &[0x0f, load + 1],
                    128,
                    false,
                ),
            ],
        );
    }

    for (mnem, pfx, esc) in [
        ("vmovntps", 0x00u8, &[0x0fu8, 0x2b] as &[u8]),
        ("vmovntpd", 0x66, &[0x0f, 0x2b]),
        ("vmovntdq", 0x66, &[0x0f, 0xe7]),
    ] {
        for l in [128, 256] {
            add(
                t,
                mnem,
                vec![vex(
                    vec![Op::M((l / 8) as u8), Op::V(vk(l))],
                    pfx,
                    esc,
                    l,
                    false,
                )],
            );
        }
    }
    for l in [128u16, 256] {
        add(
            t,
            "vmovntdqa",
            vec![vex(
                vec![Op::V(vk(l)), Op::M((l / 8) as u8)],
                0x66,
                &[0x0f, 0x38, 0x2a],
                l,
                false,
            )],
        );
        add(
            t,
            "vlddqu",
            vec![vex(
                vec![Op::V(vk(l)), Op::M((l / 8) as u8)],
                0xf2,
                &[0x0f, 0xf0],
                l,
                false,
            )],
        );
    }

    each(t, "vmovsldup", BOTH, 0xf3, &[0x0f, 0x12], false, rm);
    each(t, "vmovshdup", BOTH, 0xf3, &[0x0f, 0x16], false, rm);
    add(
        t,
        "vmovddup",
        vec![
            vex(rm(Vk::Xmm, 8), 0xf2, &[0x0f, 0x12], 128, false),
            vex(rm(Vk::Ymm, 0), 0xf2, &[0x0f, 0x12], 256, false),
        ],
    );
    add(
        t,
        "vmaskmovdqu",
        vec![vex(
            vec![Op::V(Vk::Xmm), Op::V(Vk::Xmm)],
            0x66,
            &[0x0f, 0xf7],
            128,
            false,
        )],
    );

    for (mnem, pfx, esc) in [
        ("vmovmskps", 0x00u8, &[0x0fu8, 0x50] as &[u8]),
        ("vmovmskpd", 0x66, &[0x0f, 0x50]),
        ("vpmovmskb", 0x66, &[0x0f, 0xd7]),
    ] {
        for l in [128, 256] {
            add(
                t,
                mnem,
                vec![
                    vex(vec![Op::R(4), Op::V(vk(l))], pfx, esc, l, false),
                    vex(vec![Op::R(8), Op::V(vk(l))], pfx, esc, l, false),
                ],
            );
        }
    }
}

fn install_conversions(t: &mut Tbl) {
    for (mnem, pfx) in [("vcvtsi2ss", 0xf3u8), ("vcvtsi2sd", 0xf2)] {
        for (w, opsize) in [(4u8, 32u8), (8, 64)] {
            let mut def = vex(
                vec![Op::V(Vk::Xmm), Op::Nds(Vk::Xmm), Op::Rm(w)],
                pfx,
                &[0x0f, 0x2a],
                128,
                w == 8,
            );
            // Keeps the AT&T `l`/`q` suffix able to select the row.
            def.opsize = opsize;
            add(t, mnem, vec![def]);
        }
    }
    for (mnem, pfx, op, memw) in [
        ("vcvttss2si", 0xf3u8, 0x2cu8, 4u8),
        ("vcvttsd2si", 0xf2, 0x2c, 8),
        ("vcvtss2si", 0xf3, 0x2d, 4),
        ("vcvtsd2si", 0xf2, 0x2d, 8),
    ] {
        for (w, opsize) in [(4u8, 32u8), (8, 64)] {
            let mut def = vex(
                vec![Op::R(w), Op::Vm(Vk::Xmm, memw)],
                pfx,
                &[0x0f, op],
                128,
                w == 8,
            );
            def.opsize = opsize;
            add(t, mnem, vec![def]);
        }
    }
    for (mnem, pfx, memw) in [("vcvtss2sd", 0xf3u8, 4u8), ("vcvtsd2ss", 0xf2, 8)] {
        add(
            t,
            mnem,
            vec![vex(nds(Vk::Xmm, memw), pfx, &[0x0f, 0x5a], 128, false)],
        );
    }
    for (mnem, pfx, op) in [
        ("vcvtdq2ps", 0x00u8, 0x5bu8),
        ("vcvtps2dq", 0x66, 0x5b),
        ("vcvttps2dq", 0xf3, 0x5b),
    ] {
        each(t, mnem, BOTH, pfx, &[0x0f, op], false, rm);
    }
    // Widening conversions read half a register's worth of elements.
    for (mnem, pfx, op) in [("vcvtps2pd", 0x00u8, 0x5au8), ("vcvtdq2pd", 0xf3, 0xe6)] {
        add(
            t,
            mnem,
            vec![
                vex(rm(Vk::Xmm, 8), pfx, &[0x0f, op], 128, false),
                vex(
                    vec![Op::V(Vk::Ymm), Op::Vm(Vk::Xmm, 16)],
                    pfx,
                    &[0x0f, op],
                    256,
                    false,
                ),
            ],
        );
    }
    // Narrowing conversions always land in an xmm register.
    for (mnem, pfx, op) in [
        ("vcvtpd2ps", 0x66u8, 0x5au8),
        ("vcvtpd2dq", 0xf2, 0xe6),
        ("vcvttpd2dq", 0x66, 0xe6),
    ] {
        add(
            t,
            mnem,
            vec![
                vex(rm(Vk::Xmm, 0), pfx, &[0x0f, op], 128, false),
                vex(
                    vec![Op::V(Vk::Xmm), Op::Vm(Vk::Ymm, 0)],
                    pfx,
                    &[0x0f, op],
                    256,
                    false,
                ),
            ],
        );
    }
}

fn install_integer(t: &mut Tbl) {
    // The shifts whose count is another register take that count as an xmm
    // at every vector length: only its low quadword matters.
    const SHIFT_BY_XMM: [u8; 8] = [0xd1, 0xd2, 0xd3, 0xe1, 0xe2, 0xf1, 0xf2, 0xf3];
    let mut packed: Vec<(&'static str, u8)> = PACKED_BINARY.to_vec();
    packed.push(("punpcklqdq", 0x6c));
    packed.push(("punpckhqdq", 0x6d));
    for (mnem, op) in packed {
        let v = leak(format!("v{mnem}"));
        for l in [128u16, 256] {
            let k = vk(l);
            let ops = if SHIFT_BY_XMM.contains(&op) {
                vec![Op::V(k), Op::Nds(k), Op::Vm(Vk::Xmm, 16)]
            } else {
                nds(k, 0)
            };
            add(t, v, vec![vex(ops, 0x66, &[0x0f, op], l, false)]);
        }
    }
    // Shift by immediate: `vvvv` is the destination and r/m the source.
    let mut shifts: Vec<(&'static str, u8, u8)> = SHIFT_IMM.to_vec();
    shifts.push(("psrldq", 0x73, 3));
    shifts.push(("pslldq", 0x73, 7));
    for (mnem, op, ext) in shifts {
        let v = leak(format!("v{mnem}"));
        for l in [128u16, 256] {
            let k = vk(l);
            let (map, byte) = split_escape(&[0x0f, op]);
            add(
                t,
                v,
                vec![
                    d(
                        vec![Op::Nds(k), Op::V(k), Op::Imm(1)],
                        &[byte],
                        ModRm::Ext(ext),
                        0,
                    )
                    .pfx(0x66)
                    .map(map)
                    .vex(l),
                ],
            );
        }
    }

    for (mnem, pfx) in [("vpshufd", 0x66u8), ("vpshuflw", 0xf2), ("vpshufhw", 0xf3)] {
        for l in [128, 256] {
            add(
                t,
                mnem,
                vec![vex(imm(rm(vk(l), 0)), pfx, &[0x0f, 0x70], l, false)],
            );
        }
    }

    #[rustfmt::skip]
    const NDS_38: &[(&str, u8)] = &[
        ("vpshufb", 0x00), ("vphaddw", 0x01), ("vphaddd", 0x02), ("vphaddsw", 0x03),
        ("vpmaddubsw", 0x04), ("vphsubw", 0x05), ("vphsubd", 0x06), ("vphsubsw", 0x07),
        ("vpsignb", 0x08), ("vpsignw", 0x09), ("vpsignd", 0x0a), ("vpmulhrsw", 0x0b),
        ("vpmuldq", 0x28), ("vpcmpeqq", 0x29), ("vpackusdw", 0x2b), ("vpcmpgtq", 0x37),
        ("vpminsb", 0x38), ("vpminsd", 0x39), ("vpminuw", 0x3a), ("vpminud", 0x3b),
        ("vpmaxsb", 0x3c), ("vpmaxsd", 0x3d), ("vpmaxuw", 0x3e), ("vpmaxud", 0x3f),
        ("vpmulld", 0x40),
    ];
    for &(mnem, op) in NDS_38 {
        each(t, mnem, BOTH, 0x66, &[0x0f, 0x38, op], false, nds);
    }
    for (mnem, op) in [
        ("vpabsb", 0x1cu8),
        ("vpabsw", 0x1d),
        ("vpabsd", 0x1e),
        ("vptest", 0x17),
    ] {
        each(t, mnem, BOTH, 0x66, &[0x0f, 0x38, op], false, rm);
    }
    for (mnem, op) in [("vtestps", 0x0eu8), ("vtestpd", 0x0f)] {
        each(t, mnem, BOTH, 0x66, &[0x0f, 0x38, op], false, rm);
    }
    add(
        t,
        "vphminposuw",
        vec![vex(rm(Vk::Xmm, 0), 0x66, &[0x0f, 0x38, 0x41], 128, false)],
    );

    // Sign- and zero-extending loads: the source is always an xmm register,
    // and a memory source holds only as many elements as the result needs.
    #[rustfmt::skip]
    const PMOVX: &[(&str, u8, u8)] = &[
        ("vpmovsxbw", 0x20, 8), ("vpmovsxbd", 0x21, 4), ("vpmovsxbq", 0x22, 2),
        ("vpmovsxwd", 0x23, 8), ("vpmovsxwq", 0x24, 4), ("vpmovsxdq", 0x25, 8),
        ("vpmovzxbw", 0x30, 8), ("vpmovzxbd", 0x31, 4), ("vpmovzxbq", 0x32, 2),
        ("vpmovzxwd", 0x33, 8), ("vpmovzxwq", 0x34, 4), ("vpmovzxdq", 0x35, 8),
    ];
    for &(mnem, op, memw) in PMOVX {
        add(
            t,
            mnem,
            vec![
                vex(rm(Vk::Xmm, memw), 0x66, &[0x0f, 0x38, op], 128, false),
                vex(
                    vec![Op::V(Vk::Ymm), Op::Vm(Vk::Xmm, memw * 2)],
                    0x66,
                    &[0x0f, 0x38, op],
                    256,
                    false,
                ),
            ],
        );
    }

    for (mnem, op, lens) in [
        ("vpblendw", 0x0eu8, BOTH),
        ("vpalignr", 0x0f, BOTH),
        ("vmpsadbw", 0x42, BOTH),
        ("vpclmulqdq", 0x44, X128),
    ] {
        for &l in lens {
            add(
                t,
                mnem,
                vec![vex(imm(nds(vk(l), 0)), 0x66, &[0x0f, 0x3a, op], l, false)],
            );
        }
    }
    for (mnem, op) in [
        ("vpcmpestrm", 0x60u8),
        ("vpcmpestri", 0x61),
        ("vpcmpistrm", 0x62),
        ("vpcmpistri", 0x63),
        ("vaeskeygenassist", 0xdf),
    ] {
        add(
            t,
            mnem,
            vec![vex(
                imm(rm(Vk::Xmm, 0)),
                0x66,
                &[0x0f, 0x3a, op],
                128,
                false,
            )],
        );
    }
    for (mnem, op) in [
        ("vaesenc", 0xdcu8),
        ("vaesenclast", 0xdd),
        ("vaesdec", 0xde),
        ("vaesdeclast", 0xdf),
    ] {
        add(
            t,
            mnem,
            vec![vex(nds(Vk::Xmm, 0), 0x66, &[0x0f, 0x38, op], 128, false)],
        );
    }
    add(
        t,
        "vaesimc",
        vec![vex(rm(Vk::Xmm, 0), 0x66, &[0x0f, 0x38, 0xdb], 128, false)],
    );

    // Element insert and extract.
    add(
        t,
        "vpextrb",
        vec![
            vex(
                vec![Op::R(4), Op::V(Vk::Xmm), Op::Imm(1)],
                0x66,
                &[0x0f, 0x3a, 0x14],
                128,
                false,
            )
            .flags(R_IN_RM),
            vex(
                vec![Op::R(8), Op::V(Vk::Xmm), Op::Imm(1)],
                0x66,
                &[0x0f, 0x3a, 0x14],
                128,
                false,
            )
            .flags(R_IN_RM),
            vex(
                vec![Op::M(1), Op::V(Vk::Xmm), Op::Imm(1)],
                0x66,
                &[0x0f, 0x3a, 0x14],
                128,
                false,
            ),
        ],
    );
    add(
        t,
        "vpextrw",
        vec![
            vex(
                vec![Op::R(4), Op::V(Vk::Xmm), Op::Imm(1)],
                0x66,
                &[0x0f, 0xc5],
                128,
                false,
            ),
            vex(
                vec![Op::R(8), Op::V(Vk::Xmm), Op::Imm(1)],
                0x66,
                &[0x0f, 0xc5],
                128,
                false,
            ),
            vex(
                vec![Op::M(2), Op::V(Vk::Xmm), Op::Imm(1)],
                0x66,
                &[0x0f, 0x3a, 0x15],
                128,
                false,
            ),
        ],
    );
    for (mnem, w) in [("vpextrd", 4u8), ("vpextrq", 8)] {
        add(
            t,
            mnem,
            vec![vex(
                vec![Op::Rm(w), Op::V(Vk::Xmm), Op::Imm(1)],
                0x66,
                &[0x0f, 0x3a, 0x16],
                128,
                w == 8,
            )],
        );
    }
    for (mnem, esc, reg, mem) in [
        (
            "vpinsrb",
            &[0x0fu8, 0x3a, 0x20] as &[u8],
            Op::R(4),
            Op::M(1),
        ),
        ("vpinsrw", &[0x0f, 0xc4], Op::R(4), Op::M(2)),
    ] {
        // A 64-bit register is as good as a 32-bit one, and takes no `W`.
        for src in [reg, Op::R(8), mem] {
            add(
                t,
                mnem,
                vec![vex(
                    vec![Op::V(Vk::Xmm), Op::Nds(Vk::Xmm), src, Op::Imm(1)],
                    0x66,
                    esc,
                    128,
                    false,
                )],
            );
        }
    }
    for (mnem, w) in [("vpinsrd", 4u8), ("vpinsrq", 8)] {
        add(
            t,
            mnem,
            vec![vex(
                vec![Op::V(Vk::Xmm), Op::Nds(Vk::Xmm), Op::Rm(w), Op::Imm(1)],
                0x66,
                &[0x0f, 0x3a, 0x22],
                128,
                w == 8,
            )],
        );
    }
}

fn install_avx_only(t: &mut Tbl) {
    // The same opcode twice: `L` alone decides whether only the upper halves
    // or the whole registers are cleared.
    for (mnem, l) in [("vzeroupper", 128u16), ("vzeroall", 256)] {
        add(
            t,
            mnem,
            vec![d(vec![], &[0x77], ModRm::None, 0).map(1).vex(l)],
        );
    }

    // Broadcasts from a scalar or a sub-vector.
    add(
        t,
        "vbroadcastss",
        vec![
            vex(
                vec![Op::V(Vk::Xmm), Op::Vm(Vk::Xmm, 4)],
                0x66,
                &[0x0f, 0x38, 0x18],
                128,
                false,
            ),
            vex(
                vec![Op::V(Vk::Ymm), Op::Vm(Vk::Xmm, 4)],
                0x66,
                &[0x0f, 0x38, 0x18],
                256,
                false,
            ),
        ],
    );
    add(
        t,
        "vbroadcastsd",
        vec![vex(
            vec![Op::V(Vk::Ymm), Op::Vm(Vk::Xmm, 8)],
            0x66,
            &[0x0f, 0x38, 0x19],
            256,
            false,
        )],
    );
    for (mnem, op) in [("vbroadcastf128", 0x1au8), ("vbroadcasti128", 0x5a)] {
        add(
            t,
            mnem,
            vec![vex(
                vec![Op::V(Vk::Ymm), Op::M(16)],
                0x66,
                &[0x0f, 0x38, op],
                256,
                false,
            )],
        );
    }
    for (mnem, op, memw) in [
        ("vpbroadcastb", 0x78u8, 1u8),
        ("vpbroadcastw", 0x79, 2),
        ("vpbroadcastd", 0x58, 4),
        ("vpbroadcastq", 0x59, 8),
    ] {
        for l in [128, 256] {
            add(
                t,
                mnem,
                vec![vex(
                    vec![Op::V(vk(l)), Op::Vm(Vk::Xmm, memw)],
                    0x66,
                    &[0x0f, 0x38, op],
                    l,
                    false,
                )],
            );
        }
    }

    // 128-bit halves of a 256-bit register.
    for (mnem, op) in [("vinsertf128", 0x18u8), ("vinserti128", 0x38)] {
        add(
            t,
            mnem,
            vec![vex(
                vec![
                    Op::V(Vk::Ymm),
                    Op::Nds(Vk::Ymm),
                    Op::Vm(Vk::Xmm, 0),
                    Op::Imm(1),
                ],
                0x66,
                &[0x0f, 0x3a, op],
                256,
                false,
            )],
        );
    }
    for (mnem, op) in [("vextractf128", 0x19u8), ("vextracti128", 0x39)] {
        add(
            t,
            mnem,
            vec![vex(
                vec![Op::Vm(Vk::Xmm, 0), Op::V(Vk::Ymm), Op::Imm(1)],
                0x66,
                &[0x0f, 0x3a, op],
                256,
                false,
            )],
        );
    }
    for (mnem, op) in [("vperm2f128", 0x06u8), ("vperm2i128", 0x46)] {
        add(
            t,
            mnem,
            vec![vex(
                imm(nds(Vk::Ymm, 0)),
                0x66,
                &[0x0f, 0x3a, op],
                256,
                false,
            )],
        );
    }

    // Permutes.
    for (mnem, var, fixed) in [("vpermilps", 0x0cu8, 0x04u8), ("vpermilpd", 0x0d, 0x05)] {
        each(t, mnem, BOTH, 0x66, &[0x0f, 0x38, var], false, nds);
        for l in [128, 256] {
            add(
                t,
                mnem,
                vec![vex(imm(rm(vk(l), 0)), 0x66, &[0x0f, 0x3a, fixed], l, false)],
            );
        }
    }
    for (mnem, op) in [("vpermd", 0x36u8), ("vpermps", 0x16)] {
        each(t, mnem, Y256, 0x66, &[0x0f, 0x38, op], false, nds);
    }
    for (mnem, op) in [("vpermq", 0x00u8), ("vpermpd", 0x01)] {
        add(
            t,
            mnem,
            vec![vex(imm(rm(Vk::Ymm, 0)), 0x66, &[0x0f, 0x3a, op], 256, true)],
        );
    }

    // Variable shifts, where `W` picks dword or qword elements.
    for (mnem, op, w) in [
        ("vpsllvd", 0x47u8, false),
        ("vpsllvq", 0x47, true),
        ("vpsrlvd", 0x45, false),
        ("vpsrlvq", 0x45, true),
        ("vpsravd", 0x46, false),
    ] {
        each(t, mnem, BOTH, 0x66, &[0x0f, 0x38, op], w, nds);
    }
    for l in [128, 256] {
        add(
            t,
            "vpblendd",
            vec![vex(imm(nds(vk(l), 0)), 0x66, &[0x0f, 0x3a, 0x02], l, false)],
        );
    }

    // Masked moves: the mask is `vvvv` in both directions.
    for (mnem, load, store, w) in [
        ("vmaskmovps", 0x2cu8, 0x2eu8, false),
        ("vmaskmovpd", 0x2d, 0x2f, false),
        ("vpmaskmovd", 0x8c, 0x8e, false),
        ("vpmaskmovq", 0x8c, 0x8e, true),
    ] {
        for l in [128u16, 256] {
            let k = vk(l);
            let mw = (l / 8) as u8;
            add(
                t,
                mnem,
                vec![
                    vex(
                        vec![Op::V(k), Op::Nds(k), Op::M(mw)],
                        0x66,
                        &[0x0f, 0x38, load],
                        l,
                        w,
                    ),
                    vex(
                        vec![Op::M(mw), Op::Nds(k), Op::V(k)],
                        0x66,
                        &[0x0f, 0x38, store],
                        l,
                        w,
                    ),
                ],
            );
        }
    }

    // Gathers. The index register's class depends on how many indices are
    // needed, which is not always the destination's class: gathering four
    // doubles by dword index uses an xmm index into a ymm destination.
    for (stem, op, w) in [
        ("vgatherdps", 0x92u8, false),
        ("vgatherqps", 0x93, false),
        ("vgatherdpd", 0x92, true),
        ("vgatherqpd", 0x93, true),
        ("vpgatherdd", 0x90, false),
        ("vpgatherqd", 0x91, false),
        ("vpgatherdq", 0x90, true),
        ("vpgatherqq", 0x91, true),
    ] {
        let qword_index = op & 1 == 1;
        for l in [128u16, 256] {
            // Destination and mask class, then index class.
            let (dst, index) = match (w, qword_index, l) {
                // Dword elements by dword index: all three the same width.
                (false, false, _) => (vk(l), vk(l)),
                // Dword elements by qword index: half as many results.
                (false, true, _) => (Vk::Xmm, vk(l)),
                // Qword elements by dword index: half as many indices.
                (true, false, _) => (vk(l), Vk::Xmm),
                (true, true, _) => (vk(l), vk(l)),
            };
            add(
                t,
                stem,
                vec![vex(
                    vec![Op::V(dst), Op::Vsib(index), Op::Nds(dst)],
                    0x66,
                    &[0x0f, 0x38, op],
                    l,
                    w,
                )],
            );
        }
    }
}

fn install_fma(t: &mut Tbl) {
    // Each operation exists in three operand orders, named for which operands
    // are multiplied and which added: 132, 213 and 231. The packed and scalar
    // opcodes are adjacent, and `W` selects double precision.
    #[rustfmt::skip]
    const FMA: &[(&str, u8, bool)] = &[
        // (stem, 132 packed opcode, has scalar forms)
        ("fmadd", 0x98, true), ("fmsub", 0x9a, true),
        ("fnmadd", 0x9c, true), ("fnmsub", 0x9e, true),
        ("fmaddsub", 0x96, false), ("fmsubadd", 0x97, false),
    ];
    for &(stem, base, scalar) in FMA {
        for (order, delta) in [("132", 0x00u8), ("213", 0x10), ("231", 0x20)] {
            let op = base + delta;
            for (flav, w) in [("ps", false), ("pd", true)] {
                let mnem = leak(format!("v{stem}{order}{flav}"));
                each(t, mnem, BOTH, 0x66, &[0x0f, 0x38, op], w, nds);
            }
            if scalar {
                for (flav, w, memw) in [("ss", false, 4u8), ("sd", true, 8)] {
                    let mnem = leak(format!("v{stem}{order}{flav}"));
                    add(
                        t,
                        mnem,
                        vec![vex(nds(Vk::Xmm, memw), 0x66, &[0x0f, 0x38, op + 1], 128, w)],
                    );
                }
            }
        }
    }
}

pub fn install(t: &mut Tbl) {
    install_float(t);
    install_moves(t);
    install_conversions(t);
    install_integer(t);
    install_avx_only(t);
    install_fma(t);
}
