//! BMI1, BMI2, TBM and LWP: VEX- and XOP-encoded instructions on general
//! registers.
//!
//! These borrowed the vector prefixes for their compact three-operand forms:
//! `vvvv` holds a general register instead of a vector one, `L` is always
//! zero, and `W` selects 64-bit operands where `REX.W` would. So a row here
//! is a VEX row whose operand size, 32 or 64, doubles as its `W`, which also
//! lets an AT&T `l` or `q` suffix select it.
//!
//! AMD's TBM and LWP live in the XOP maps behind `8F`.

use super::{Def, ModRm, Op, Tbl, add, d};

/// A VEX or XOP row on general registers of width `w`, 4 or 8.
fn gpr(ops: Vec<Op>, pfx: u8, map: u8, op: u8, modrm: ModRm, w: u8) -> Def {
    d(ops, &[op], modrm, w * 8).pfx(pfx).map(map).vex(128)
}

pub fn install(t: &mut Tbl) {
    for w in [4u8, 8] {
        let r = Op::R(w);
        let rm = Op::Rm(w);
        let v = Op::NdsR(w);
        // `op dst, src1 (vvvv), src2/m`.
        for (mnem, pfx, op) in [
            ("andn", 0x00u8, 0xf2u8),
            ("mulx", 0xf2, 0xf6),
            ("pdep", 0xf2, 0xf5),
            ("pext", 0xf3, 0xf5),
        ] {
            add(
                t,
                mnem,
                vec![gpr(vec![r, v, rm], pfx, 2, op, ModRm::Reg, w)],
            );
        }
        // `op dst, src/m, count (vvvv)`.
        for (mnem, pfx, op) in [
            ("bextr", 0x00u8, 0xf7u8),
            ("bzhi", 0x00, 0xf5),
            ("sarx", 0xf3, 0xf7),
            ("shlx", 0x66, 0xf7),
            ("shrx", 0xf2, 0xf7),
        ] {
            add(
                t,
                mnem,
                vec![gpr(vec![r, rm, v], pfx, 2, op, ModRm::Reg, w)],
            );
        }
        // `op dst (vvvv), src/m`, the operation in ModRM.reg.
        for (mnem, ext) in [("blsr", 1u8), ("blsmsk", 2), ("blsi", 3)] {
            add(
                t,
                mnem,
                vec![gpr(vec![v, rm], 0x00, 2, 0xf3, ModRm::Ext(ext), w)],
            );
        }
        add(
            t,
            "rorx",
            vec![gpr(vec![r, rm, Op::Imm(1)], 0xf2, 3, 0xf0, ModRm::Reg, w)],
        );

        // TBM, in XOP map 9, with the same `op dst (vvvv), src/m` shape.
        for (mnem, op, ext) in [
            ("blcfill", 0x01u8, 1u8),
            ("blsfill", 0x01, 2),
            ("blcs", 0x01, 3),
            ("tzmsk", 0x01, 4),
            ("blcic", 0x01, 5),
            ("blsic", 0x01, 6),
            ("t1mskc", 0x01, 7),
            ("blcmsk", 0x02, 1),
            ("blci", 0x02, 6),
        ] {
            add(
                t,
                mnem,
                vec![gpr(vec![v, rm], 0x00, 9, op, ModRm::Ext(ext), w)],
            );
        }
        // TBM's `bextr` takes its control as an immediate, in map 10.
        add(
            t,
            "bextr",
            vec![gpr(vec![r, rm, Op::Imm(4)], 0x00, 10, 0x10, ModRm::Reg, w)],
        );

        // LWP: the control block address, and the two event inserts, whose
        // data is always 32 bits whatever the register in `vvvv`.
        for (mnem, ext) in [("llwpcb", 0u8), ("slwpcb", 1)] {
            add(
                t,
                mnem,
                vec![gpr(vec![r], 0x00, 9, 0x12, ModRm::Ext(ext), w)],
            );
        }
        for (mnem, ext) in [("lwpins", 0u8), ("lwpval", 1)] {
            add(
                t,
                mnem,
                vec![gpr(
                    vec![v, Op::Rm(4), Op::Imm(4)],
                    0x00,
                    10,
                    0x12,
                    ModRm::Ext(ext),
                    w,
                )],
            );
        }
    }
}
