//! Materialising a constant into a register — the `li` pseudo-instruction.
//!
//! RISC-V can only build a constant twelve or twenty bits at a time, so `li`
//! is a whole code sequence. Which sequence is a genuine optimisation problem
//! (LLVM's `RISCVMatInt` runs to several hundred lines), and assemblers do not
//! agree on the answer for every value. This follows LLVM's choices, minus the
//! paths that need bit-manipulation extensions rsasm does not assemble, so
//! that the differential test against `llvm-mc` has a chance of matching.

/// One step of a materialisation sequence. Every step but the first reads the
/// destination register back, and the first reads `x0`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Op {
    Lui(i64),
    Addi(i64),
    Addiw(i64),
    Slli(u32),
    Srli(u32),
    /// Always `xori rd, rd, -1`: the tail of an inverted sequence.
    Not,
}

fn fits_i32(v: i64) -> bool {
    v as i32 as i64 == v
}

fn fits_i12(v: i64) -> bool {
    (-(1 << 11)..(1 << 11)).contains(&v)
}

fn fits_i6(v: i64) -> bool {
    (-32..32).contains(&v)
}

fn sext12(v: i64) -> i64 {
    ((v as u64) << 52) as i64 >> 52
}

fn sext32(v: u64) -> i64 {
    v as u32 as i32 as i64
}

/// The base sequence: peel twelve bits off the bottom, shift, and recurse
/// until what is left fits in `lui`+`addi`.
fn seq(val: i64, rv64: bool, res: &mut Vec<Op>) {
    if fits_i32(val) {
        let hi20 = (((val as u64).wrapping_add(0x800) >> 12) & 0xf_ffff) as i64;
        let lo12 = sext12(val);
        if hi20 != 0 {
            res.push(Op::Lui(hi20));
        }
        if lo12 != 0 || hi20 == 0 {
            // `addiw` is only needed when the 32-bit add would overflow, which
            // is also what keeps RV32 and RV64 sequences looking alike.
            let overflows = rv64 && hi20 != 0 && !fits_i32(sext32((hi20 as u64) << 12) + lo12);
            res.push(if overflows {
                Op::Addiw(lo12)
            } else {
                Op::Addi(lo12)
            });
        }
        return;
    }

    let lo12 = sext12(val);
    let mut rest = (val as u64).wrapping_sub(lo12 as u64) as i64;
    let mut shift = 0u32;
    if !fits_i32(rest) {
        shift = (rest as u64).trailing_zeros();
        rest >>= shift;
        // A sparse constant can shift so far that what is left no longer fits
        // an `addi`; giving twelve of those bits back lets `lui` carry them.
        if shift > 12 && !fits_i12(rest) && fits_i32(((rest as u64) << 12) as i64) {
            shift -= 12;
            rest = ((rest as u64) << 12) as i64;
        }
    }

    seq(rest, rv64, res);
    if shift != 0 {
        res.push(Op::Slli(shift));
    }
    if lo12 != 0 {
        res.push(Op::Addi(lo12));
    }
}

fn mask_trailing_ones(n: u32) -> u64 {
    if n >= 64 { u64::MAX } else { (1u64 << n) - 1 }
}

/// Builds the value shifted all the way left, then shifts it back down.
///
/// This wins for constants with a long run of ones in the middle: the run
/// becomes an `addi -1` once it reaches the top of the register.
fn leading_zeros_seq(val: i64, rv64: bool, res: &mut Vec<Op>) {
    if val <= 0 {
        return;
    }
    let lz = (val as u64).leading_zeros();
    let filled = ((val as u64) << lz) | mask_trailing_ones(lz);

    let better = |candidate: u64, res: &mut Vec<Op>| {
        let mut tmp = Vec::new();
        seq(candidate as i64, rv64, &mut tmp);
        if tmp.len() + 1 < res.len() || (res.is_empty() && tmp.len() < 8) {
            tmp.push(Op::Srli(lz));
            *res = tmp;
        }
    };
    better(filled, res);
    // The other half of the trick: zeros in the shifted-out bits instead.
    better(filled & !mask_trailing_ones(lz), res);
}

/// The instruction sequence that loads `val` on an `xlen`-bit machine.
pub fn generate(val: i64, xlen: u8) -> Vec<Op> {
    let rv64 = xlen == 64;
    let mut res = Vec::new();
    seq(val, rv64, &mut res);

    // A constant with trailing zeros can often be built small and shifted up.
    // Preferring that even when it is no shorter buys two compressible
    // instructions in place of two that are not.
    if val & 0xfff != 0 && val & 1 == 0 && res.len() >= 2 {
        let tz = (val as u64).trailing_zeros();
        let shifted = val >> tz;
        let mut tmp = Vec::new();
        seq(shifted, rv64, &mut tmp);
        if tmp.len() + 1 < res.len() || fits_i6(shifted) {
            tmp.push(Op::Slli(tz));
            res = tmp;
        }
    }

    if res.len() <= 2 {
        return res;
    }

    // Rounding the low bits up to the next multiple of 0x800 can leave more
    // trailing zeros for the recursion, at the price of a final `addi`.
    if val & 0xfff != 0 && val & 0x1800 == 0x1000 {
        let imm12 = -(0x800 - (val & 0xfff));
        let mut tmp = Vec::new();
        seq(val - imm12, rv64, &mut tmp);
        if tmp.len() + 1 < res.len() {
            tmp.push(Op::Addi(imm12));
            res = tmp;
        }
    }

    if val > 0 && res.len() > 2 {
        leading_zeros_seq(val, rv64, &mut res);
    }

    // A negative constant may have a much cheaper complement.
    if val < 0 && res.len() > 3 {
        let mut tmp = Vec::new();
        leading_zeros_seq(!val, rv64, &mut tmp);
        if !tmp.is_empty() && tmp.len() + 1 < res.len() {
            tmp.push(Op::Not);
            res = tmp;
        }
    }

    res
}

#[cfg(test)]
mod tests {
    use super::Op::*;
    use super::*;

    #[test]
    fn small_constants_are_one_instruction() {
        assert_eq!(generate(0, 64), vec![Addi(0)]);
        assert_eq!(generate(5, 64), vec![Addi(5)]);
        assert_eq!(generate(-1, 64), vec![Addi(-1)]);
        assert_eq!(generate(2047, 64), vec![Addi(2047)]);
        assert_eq!(generate(0x1000, 64), vec![Lui(1)]);
    }

    #[test]
    fn thirty_two_bit_constants_split_into_lui_and_addi() {
        assert_eq!(generate(0x1234_5678, 64), vec![Lui(0x12345), Addi(0x678)]);
        // The low half is negative as a 12-bit value, so `lui` is biased up
        // by one to compensate.
        assert_eq!(generate(0x1234_5fff, 64), vec![Lui(0x12346), Addi(-1)]);
    }

    #[test]
    fn shifted_small_constants_beat_lui_plus_addi() {
        // 2048 is `li 1` shifted left eleven, which compresses to four bytes.
        assert_eq!(generate(2048, 64), vec![Addi(1), Slli(11)]);
    }
}
