//! The smaller vector extensions: F16C, GFNI, the 256-bit VAES and
//! VPCLMULQDQ forms, SHA, SHA512, SM3, SM4, and the VEX-only integer
//! additions that came after AVX2.
//!
//! Two groups need telling apart. Most of these are VEX-only, or have an
//! AVX-512 counterpart that only EVEX-worthy operands should reach, so they
//! are installed here, before the EVEX rows, as the rest of AVX is.
//!
//! The others — AVX-VNNI, AVX-IFMA and AVX-NE-CONVERT's `vcvtneps2bf16` —
//! spell exactly what AVX-512 already spells. Both references give those
//! mnemonics the EVEX encoding unless the source asks for VEX with `{vex}`,
//! so their VEX rows are installed after the EVEX ones by [`install_late`]
//! and only a `{vex}` prefix selects them.

use super::avx::{imm, nds, rm, vex, vk};
use super::{Def, ModRm, Op, Tbl, Vk, add, d};

const BOTH: [u16; 2] = [128, 256];

/// A legacy-encoded SSE-style row: `66`-less unless given, `/r`.
fn legacy(ops: Vec<Op>, pfx: u8, esc: &[u8]) -> Def {
    d(ops, esc, ModRm::Reg, 0).pfx(pfx)
}

fn install_f16c(t: &mut Tbl) {
    // Half-precision floats: four of them fit in eight bytes, so the 128-bit
    // form converts a quadword of memory and the 256-bit form an `xmm`.
    add(
        t,
        "vcvtph2ps",
        vec![
            vex(
                vec![Op::V(Vk::Xmm), Op::Vm(Vk::Xmm, 8)],
                0x66,
                &[0x0f, 0x38, 0x13],
                128,
                false,
            ),
            vex(
                vec![Op::V(Vk::Ymm), Op::Vm(Vk::Xmm, 16)],
                0x66,
                &[0x0f, 0x38, 0x13],
                256,
                false,
            ),
        ],
    );
    add(
        t,
        "vcvtps2ph",
        vec![
            vex(
                vec![Op::Vm(Vk::Xmm, 8), Op::V(Vk::Xmm), Op::Imm(1)],
                0x66,
                &[0x0f, 0x3a, 0x1d],
                128,
                false,
            ),
            vex(
                vec![Op::Vm(Vk::Xmm, 16), Op::V(Vk::Ymm), Op::Imm(1)],
                0x66,
                &[0x0f, 0x3a, 0x1d],
                256,
                false,
            ),
        ],
    );
}

fn install_gfni(t: &mut Tbl) {
    let xm = || vec![Op::V(Vk::Xmm), Op::Vm(Vk::Xmm, 0)];
    add(
        t,
        "gf2p8mulb",
        vec![legacy(xm(), 0x66, &[0x0f, 0x38, 0xcf])],
    );
    for (mnem, op) in [("gf2p8affineqb", 0xceu8), ("gf2p8affineinvqb", 0xcf)] {
        add(t, mnem, vec![legacy(imm(xm()), 0x66, &[0x0f, 0x3a, op])]);
        let vmnem = if op == 0xce {
            "vgf2p8affineqb"
        } else {
            "vgf2p8affineinvqb"
        };
        for l in BOTH {
            add(
                t,
                vmnem,
                vec![vex(imm(nds(vk(l), 0)), 0x66, &[0x0f, 0x3a, op], l, true)],
            );
        }
    }
    for l in BOTH {
        add(
            t,
            "vgf2p8mulb",
            vec![vex(nds(vk(l), 0), 0x66, &[0x0f, 0x38, 0xcf], l, false)],
        );
    }
}

fn install_wide_crypto(t: &mut Tbl) {
    // VAES and VPCLMULQDQ are the 128-bit AVX instructions at 256 bits.
    for (mnem, op) in [
        ("vaesenc", 0xdcu8),
        ("vaesenclast", 0xdd),
        ("vaesdec", 0xde),
        ("vaesdeclast", 0xdf),
    ] {
        add(
            t,
            mnem,
            vec![vex(nds(Vk::Ymm, 0), 0x66, &[0x0f, 0x38, op], 256, false)],
        );
    }
    add(
        t,
        "vpclmulqdq",
        vec![vex(
            imm(nds(Vk::Ymm, 0)),
            0x66,
            &[0x0f, 0x3a, 0x44],
            256,
            false,
        )],
    );
}

fn install_sha(t: &mut Tbl) {
    let xm = || vec![Op::V(Vk::Xmm), Op::Vm(Vk::Xmm, 0)];
    for (mnem, op) in [
        ("sha1nexte", 0xc8u8),
        ("sha1msg1", 0xc9),
        ("sha1msg2", 0xca),
        ("sha256rnds2", 0xcb),
        ("sha256msg1", 0xcc),
        ("sha256msg2", 0xcd),
    ] {
        add(t, mnem, vec![legacy(xm(), 0, &[0x0f, 0x38, op])]);
    }
    // `sha256rnds2` reads its round keys from `xmm0`, which the source may
    // also name, as a third operand, as `blendvps` allows.
    add(
        t,
        "sha256rnds2",
        vec![legacy(
            vec![Op::V(Vk::Xmm), Op::Vm(Vk::Xmm, 0), Op::Fixed("xmm0")],
            0,
            &[0x0f, 0x38, 0xcb],
        )],
    );
    add(
        t,
        "sha1rnds4",
        vec![legacy(imm(xm()), 0, &[0x0f, 0x3a, 0xcc])],
    );

    // SHA512 works on `ymm` state, SM3 on `xmm`, and SM4 on either.
    add(
        t,
        "vsha512rnds2",
        vec![vex(
            vec![Op::V(Vk::Ymm), Op::Nds(Vk::Ymm), Op::V(Vk::Xmm)],
            0xf2,
            &[0x0f, 0x38, 0xcb],
            256,
            false,
        )],
    );
    add(
        t,
        "vsha512msg1",
        vec![vex(
            vec![Op::V(Vk::Ymm), Op::V(Vk::Xmm)],
            0xf2,
            &[0x0f, 0x38, 0xcc],
            256,
            false,
        )],
    );
    add(
        t,
        "vsha512msg2",
        vec![vex(
            vec![Op::V(Vk::Ymm), Op::V(Vk::Ymm)],
            0xf2,
            &[0x0f, 0x38, 0xcd],
            256,
            false,
        )],
    );
    add(
        t,
        "vsm3rnds2",
        vec![vex(
            imm(nds(Vk::Xmm, 0)),
            0x66,
            &[0x0f, 0x3a, 0xde],
            128,
            false,
        )],
    );
    for (mnem, pfx) in [("vsm3msg1", 0x00u8), ("vsm3msg2", 0x66)] {
        add(
            t,
            mnem,
            vec![vex(nds(Vk::Xmm, 0), pfx, &[0x0f, 0x38, 0xda], 128, false)],
        );
    }
    for (mnem, pfx) in [("vsm4key4", 0xf3u8), ("vsm4rnds4", 0xf2)] {
        for l in BOTH {
            add(
                t,
                mnem,
                vec![vex(nds(vk(l), 0), pfx, &[0x0f, 0x38, 0xda], l, false)],
            );
        }
    }
}

fn install_vnni_int(t: &mut Tbl) {
    // AVX-VNNI-INT8 and -INT16: the signedness combinations the AVX-512
    // accumulators left out, told apart by the `pp` field alone.
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
    ] {
        for l in BOTH {
            add(
                t,
                mnem,
                vec![vex(nds(vk(l), 0), pfx, &[0x0f, 0x38, op], l, false)],
            );
        }
    }
    // AVX-NE-CONVERT's half-precision and bfloat16 loads, which only read
    // memory.
    for (mnem, pfx, op, memw) in [
        ("vbcstnebf162ps", 0xf3u8, 0xb1u8, 2u8),
        ("vbcstnesh2ps", 0x66, 0xb1, 2),
        ("vcvtneebf162ps", 0xf3, 0xb0, 0),
        ("vcvtneeph2ps", 0x66, 0xb0, 0),
        ("vcvtneobf162ps", 0xf2, 0xb0, 0),
        ("vcvtneoph2ps", 0x00, 0xb0, 0),
    ] {
        for l in BOTH {
            let k = vk(l);
            let mem = if memw != 0 { memw } else { k.width() };
            add(
                t,
                mnem,
                vec![vex(
                    vec![Op::V(k), Op::M(mem)],
                    pfx,
                    &[0x0f, 0x38, op],
                    l,
                    false,
                )],
            );
        }
    }
}

pub fn install(t: &mut Tbl) {
    // The MXCSR load and store, whose VEX forms only exist so AVX code need
    // not mix in a legacy prefix.
    for (mnem, ext) in [("vldmxcsr", 2u8), ("vstmxcsr", 3)] {
        add(
            t,
            mnem,
            vec![
                d(vec![Op::M(4)], &[0xae], ModRm::Ext(ext), 0)
                    .map(1)
                    .vex(128),
            ],
        );
    }
    install_f16c(t);
    install_gfni(t);
    install_wide_crypto(t);
    install_sha(t);
    install_vnni_int(t);
}

/// The VEX spellings of instructions AVX-512 also has, reachable only with
/// `{vex}`. See the module documentation.
pub fn install_late(t: &mut Tbl) {
    for (mnem, pfx, op, w) in [
        ("vpdpbusd", 0x66u8, 0x50u8, false),
        ("vpdpbusds", 0x66, 0x51, false),
        ("vpdpwssd", 0x66, 0x52, false),
        ("vpdpwssds", 0x66, 0x53, false),
        ("vpmadd52luq", 0x66, 0xb4, true),
        ("vpmadd52huq", 0x66, 0xb5, true),
    ] {
        for l in BOTH {
            add(
                t,
                mnem,
                vec![vex(nds(vk(l), 0), pfx, &[0x0f, 0x38, op], l, w)],
            );
        }
    }
    add(
        t,
        "vcvtneps2bf16",
        vec![
            vex(rm(Vk::Xmm, 0), 0xf3, &[0x0f, 0x38, 0x72], 128, false),
            vex(
                vec![Op::V(Vk::Xmm), Op::Vm(Vk::Ymm, 0)],
                0xf3,
                &[0x0f, 0x38, 0x72],
                256,
                false,
            ),
        ],
    );
}
