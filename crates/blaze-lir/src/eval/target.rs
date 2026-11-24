//! `target_intrinsic` in the reference evaluator (THREADS.md D1).
//!
//! The four target intrinsics blaze builds (blaze-build `intrinsics::target::Target`, the one
//! home of the set; blaze-lir sits below blaze-build and cannot name it, so the names are
//! listed here and blaze-build's test runs this evaluator on what it builds) are the aarch64
//! barriers and hints: `isb`, `dmb`, `dsb` and `hint`. They order or pace the processor and
//! have no meaning in the evaluator's single-threaded abstract machine: each tier must equal
//! the evaluator in observable results, and none of the four changes a value or a byte of
//! memory. So each runs as no effect. Any other name is refused, never guessed.

use core::alloc::Allocator;

use super::{EvalError, Outcome, Run, Val};
use crate::Op;

#[cfg(test)]
mod tests;

/// The link names the evaluator runs as no effect: blaze-build `Target::link_name` of each of
/// `ALL`.
pub(super) const NO_EFFECT: [&[u8]; 4] = [b"llvm.aarch64.isb", b"llvm.aarch64.dmb", b"llvm.aarch64.dsb", b"llvm.aarch64.hint"];

/// Reflected IEEE 802.3 CRC-32 polynomial (`crc32b`/`crc32x`).
const CRC32_IEEE_POLY: u32 = 0xEDB8_8320;

/// Reflected Castagnoli CRC-32C polynomial (`crc32cb`/`crc32cx`).
const CRC32_CASTAGNOLI_POLY: u32 = 0x82F6_3B78;

/// Exact ARM ARM reflected CRC-32 step over `bits` LSB-first bits of `data`.
fn crc32_step(mut acc: u32, data: u64, bits: u32, poly: u32) -> u32 {
    for i in 0..bits {
        let bit = ((acc ^ u32::try_from((data >> i) & 1).unwrap_or(0)) & 1) != 0;
        acc >>= 1;
        if bit {
            acc ^= poly;
        }
    }
    acc
}

impl<A: Allocator + Clone> Run<'_, A> {
    /// Target intrinsic `inst`: one of [`NO_EFFECT`] does nothing and gives no value;
    /// AArch64 `FEAT_CRC32` intrinsics (`blaze.a64.crc32{,c}{b,x}`) execute exact ARM ARM
    /// reflected CRC-32 semantics; any other name is refused.
    pub(super) fn target_intrinsic(&mut self, inst: usize) -> Result<Result<Option<Val>, Outcome<A>>, EvalError> {
        let mut o = self.view.operands(inst);
        let name = self.view.intrinsic_name(o.intrinsic());
        if NO_EFFECT.contains(&name) {
            // Its operand (the immediate) is read so an unset value is still the verifier's error
            // it always is.
            for value in o.values().iter() {
                self.get(inst, value)?;
            }
            return Ok(Ok(None));
        }

        let crc_spec = match name {
            b"blaze.a64.crc32b" => Some((8u32, CRC32_IEEE_POLY)),
            b"blaze.a64.crc32x" => Some((64u32, CRC32_IEEE_POLY)),
            b"blaze.a64.crc32cb" => Some((8u32, CRC32_CASTAGNOLI_POLY)),
            b"blaze.a64.crc32cx" => Some((64u32, CRC32_CASTAGNOLI_POLY)),
            _ => None,
        };
        if let Some((bits, poly)) = crc_spec {
            let mut vals = o.values().iter();
            let (Some(crc_v), Some(data_v), None) = (vals.next(), vals.next(), vals.next()) else {
                return Err(EvalError::Unsupported {
                    inst,
                    op: Op::TargetIntrinsic,
                    what: "a `blaze.a64.crc32*` intrinsic without exactly 2 integer arguments",
                });
            };
            let (crc_raw, crc_bits) = self.int(inst, crc_v)?;
            let (data_raw, data_bits) = self.int(inst, data_v)?;
            let data_ok = if bits == 8 { data_bits == 8 || data_bits == 32 } else { data_bits == 64 };
            let res_ok = self.width(inst)? == 32;
            if crc_bits != 32 || !data_ok || !res_ok {
                return Err(EvalError::Unsupported {
                    inst,
                    op: Op::TargetIntrinsic,
                    what: "a `blaze.a64.crc32*` intrinsic with unsupported operand or result width",
                });
            }
            let acc = u32::try_from(crc_raw & u128::from(u32::MAX)).unwrap_or(0);
            let data_mask = if bits == 8 { 0xffu128 } else { u128::from(u64::MAX) };
            let data = u64::try_from(data_raw & data_mask).unwrap_or(0);
            let res = crc32_step(acc, data, bits, poly);
            return Ok(Ok(Some(Val::Int(u128::from(res)))));
        }

        Err(EvalError::Unsupported {
            inst,
            op: Op::TargetIntrinsic,
            what: "a target intrinsic other than `llvm.aarch64.{isb,dmb,dsb,hint}` (the instruction's row names it)",
        })
    }
}
