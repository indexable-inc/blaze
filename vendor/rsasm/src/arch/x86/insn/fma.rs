//! FMA: the fused multiply-add family, VEX- and EVEX-encoded.
//!
//! The mnemonics spell out which operand is multiplied by which and which is
//! added, and the opcode encodes exactly that: a base byte per operation
//! (`88` add, `8A` sub, `8C` negated add, `8E` negated sub, `86` alternating
//! add/sub, `87` the other way round), `+1` for the scalar form, and
//! `+0x10`, `+0x20` or `+0x30` for the `132`, `213` and `231` operand orders.
//! `W` picks single or double precision. So the whole family is one loop.
//!
//! Every form exists twice, VEX at 128 and 256 bits and EVEX at all three,
//! and the VEX rows are installed first so ordinary operands take them.

use super::avx::vex;
use super::avx512::ev;
use super::{Def, EVEX_ER, Op, Tbl, Tuple, Vk, add};

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// (stem, base opcode, whether the operation also has scalar forms)
#[rustfmt::skip]
const OPS: &[(&str, u8, bool)] = &[
    ("fmaddsub", 0x86, false),
    ("fmsubadd", 0x87, false),
    ("fmadd",    0x88, true),
    ("fmsub",    0x8a, true),
    ("fnmadd",   0x8c, true),
    ("fnmsub",   0x8e, true),
];

/// The three operand orders, and what each adds to the base opcode.
const ORDERS: [(&str, u8); 3] = [("132", 0x10), ("213", 0x20), ("231", 0x30)];

pub fn install(t: &mut Tbl) {
    for &(stem, base, scalar) in OPS {
        for (order, bump) in ORDERS {
            for (flav, w) in [("ps", false), ("pd", true)] {
                let mnem = leak(format!("v{stem}{order}{flav}"));
                let op = base | bump;
                let ops = |k: Vk| vec![Op::V(k), Op::Nds(k), Op::Vm(k, 0)];
                add(
                    t,
                    mnem,
                    vec![
                        vex(ops(Vk::Xmm), 0x66, &[0x0f, 0x38, op], 128, w),
                        vex(ops(Vk::Ymm), 0x66, &[0x0f, 0x38, op], 256, w),
                    ],
                );
                for (l, k) in [(128, Vk::Xmm), (256, Vk::Ymm), (512, Vk::Zmm)] {
                    let def = ev(ops(k), 0x66, &[0x0f, 0x38, op], l, w, Tuple::Fv);
                    add(t, mnem, vec![er(def, l == 512)]);
                }
            }
            if !scalar {
                continue;
            }
            for (flav, w, memw) in [("ss", false, 4u8), ("sd", true, 8)] {
                let mnem = leak(format!("v{stem}{order}{flav}"));
                let op = base | 1 | bump;
                let ops = vec![Op::V(Vk::Xmm), Op::Nds(Vk::Xmm), Op::Vm(Vk::Xmm, memw)];
                add(
                    t,
                    mnem,
                    vec![
                        vex(ops.clone(), 0x66, &[0x0f, 0x38, op], 128, w),
                        ev(ops, 0x66, &[0x0f, 0x38, op], 128, w, Tuple::T1s).flags(EVEX_ER),
                    ],
                );
            }
        }
    }
}

/// Rounding control exists on the 512-bit packed forms and on the scalars;
/// a 128- or 256-bit packed form has no spare `L'L` to hold the mode.
fn er(def: Def, yes: bool) -> Def {
    if yes { def.flags(EVEX_ER) } else { def }
}
