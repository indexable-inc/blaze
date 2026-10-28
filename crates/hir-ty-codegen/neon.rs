//! AArch64's SIMD intrinsics of LLVM that `core::arch` calls: `llvm.aarch64.neon.*`, the
//! crypto ones, the rounding and conversion ones, and the generic ones it calls on vectors
//! (`llvm.roundeven.v4f32`). LLVM defines each of these as the instruction it selects for
//! it, so that instruction is the lowering that agrees with it in every lane: in how it
//! rounds, saturates and treats a NaN, and in the reciprocal estimates, whose tables only
//! the hardware has. [`lower`] says which instructions an intrinsic is; the code generator
//! calls them as a function of their own (`asm::neon_function`), on registers loaded from
//! where its operands are: `v1` up from the first argument, `x1` up for a pointer, the result
//! left in `v0` (and `v1` up for a tuple of vectors).

use std::ops::RangeInclusive;

/// An intrinsic as instructions on the registers [`asm::neon_function`](super::asm) loads.
#[derive(Clone)]
pub(super) struct Lowered {
    /// The target features, as Rust names them, the instructions need of the assembler.
    pub(super) features: Vec<&'static str>,
    pub(super) text: String,
    /// The bytes each argument has in its register, `None` for a pointer: argument `i` is
    /// read into `v{i+1}` (`x{i+1}` for a pointer), counted without the immediate.
    pub(super) args: Vec<Option<u32>>,
    /// The bytes of each result, from `v0` up; none for an intrinsic that only stores.
    pub(super) results: Vec<u32>,
    pub(super) immediate: Option<Immediate>,
}

/// The argument an instruction takes as an immediate, which the text names `{imm}`: a
/// constant `core::arch` passes, which LLVM requires to be in `range`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Immediate {
    /// Which argument it is, of all the intrinsic's.
    pub(super) arg: usize,
    pub(super) range: RangeInclusive<u32>,
    /// The bytes of each lane of the vector it comes splat into (`sqshlu`'s shift), or
    /// `None` for an integer.
    pub(super) splat: Option<u32>,
}

impl Immediate {
    fn of(arg: usize, range: RangeInclusive<u32>) -> Immediate {
        Immediate { arg, range, splat: None }
    }
}

/// `lowered`, which names `immediate` `{imm}`.
fn taking(lowered: Option<Lowered>, immediate: Immediate) -> Option<Lowered> {
    Some(Lowered { immediate: Some(immediate), ..lowered? })
}

/// The values of an intrinsic's signature as its name says them: `v8i8`, `v1f64`, `i32`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Lanes {
    pub(super) count: u32,
    pub(super) bits: u32,
    pub(super) float: bool,
}

impl Lanes {
    fn parse(token: &str) -> Option<Lanes> {
        let (count, rest) = match token.strip_prefix('v') {
            Some(rest) => {
                let digits = rest.find(|c: char| !c.is_ascii_digit())?;
                (rest[..digits].parse().ok()?, &rest[digits..])
            }
            None => (1, token),
        };
        let float = match rest.as_bytes().first()? {
            b'f' => true,
            b'i' => false,
            _ => return None,
        };
        let bits = rest[1..].parse().ok().filter(|it| matches!(it, 8 | 16 | 32 | 64))?;
        // A float of 8 bits is not one LLVM names so.
        (count > 0 && !(float && bits == 8)).then_some(Lanes { count, bits, float })
    }

    pub(super) fn bytes(self) -> u32 {
        self.count * self.bits / 8
    }

    fn with_bits(self, bits: u32) -> Lanes {
        Lanes { bits, ..self }
    }

    fn is_int(self) -> bool {
        !self.float
    }

    /// The operand that names this value in register `k`: a vector by its arrangement, a
    /// value of one lane as the scalar register of its width, as the instructions on one
    /// lane take it (`sqadd s0, s1, s2`, `fabd d0, d1, d2` of a `v1f64`).
    fn reg(self, k: u32) -> String {
        let size = size_letter(self.bits);
        match self.count {
            1 => format!("{size}{k}"),
            count => format!("v{k}.{count}{size}"),
        }
    }

    /// The register `k` in a list of registers, which names a vector of one 64-bit lane as
    /// `v{k}.1d`.
    fn listed(self, k: u32) -> String {
        format!("v{k}.{}{}", self.count, size_letter(self.bits))
    }

    /// Whether this is a whole vector register's 128 bits or its low 64.
    fn is_register(self) -> bool {
        matches!(self.bytes(), 8 | 16)
    }
}

fn size_letter(bits: u32) -> char {
    match bits {
        8 => 'b',
        16 => 'h',
        32 => 's',
        64 => 'd',
        _ => 'q',
    }
}

/// The list `{v{first}.T, ...}` of `n` registers of `lanes`, as the loads, stores and table
/// lookups take them.
fn list(lanes: Lanes, first: u32, n: u32) -> String {
    let regs: Vec<String> = (first..first + n).map(|k| lanes.listed(k)).collect();
    format!("{{{}}}", regs.join(", "))
}

const SIXTEEN: Lanes = Lanes { count: 16, bits: 8, float: false };
const HALF: Lanes = Lanes { count: 1, bits: 16, float: true };

/// The instructions of the intrinsic `llvm.{name}`, or `None` for one this does not know.
pub(super) fn lower(name: &str) -> Option<Lowered> {
    let mut words = Vec::new();
    let mut types = Vec::new();
    let rest = name.strip_prefix("aarch64.");
    for token in rest.unwrap_or(name).split('.') {
        match Lanes::parse(token) {
            Some(lanes) => types.push(lanes),
            // An address space, which is the only one.
            None if token == "p0" => (),
            None => words.push(token),
        }
    }
    let op = words.join(".");
    let lowered = match rest {
        Some(_) => match op.strip_prefix("neon.") {
            Some(neon) => self::neon(neon, &types),
            None => match op.strip_prefix("crypto.") {
                Some(crypto) => self::crypto(crypto, &types),
                None => self::neon(&op, &types),
            },
        },
        None => generic(&op, &types),
    }?;
    Some(finish(lowered, &types))
}

/// A saturating right shift into lanes of half the width. The last operand is an
/// immediate in `1..=max_shift`, returned alongside the lowering; the caller substitutes
/// it for `{shift}` and passes only the first operand to the assembly function.
/// `sqshrun` shifts signed lanes before saturating to unsigned ones. The `r` variants
/// round before narrowing, including when the rounding addition exceeds the input width.
pub(super) fn narrowing_shift(name: &str) -> Option<(Lowered, u32)> {
    let (op, result) = name.strip_prefix("aarch64.neon.")?.split_once('.')?;
    if !matches!(op, "sqshrn" | "uqshrn" | "sqshrun" | "sqrshrn" | "uqrshrn" | "sqrshrun") {
        return None;
    }
    let r = Lanes::parse(result)?;
    if !r.is_int() || !matches!(r.bits, 8 | 16 | 32) || !(r.count == 1 || r.bytes() == 8) {
        return None;
    }
    let a = r.with_bits(2 * r.bits);
    let text = format!("{op} {}, {}, #{{shift}}", r.reg(0), a.reg(1));
    let lowered = instruction(&[], text, [a], [r])?;
    Some((finish(lowered, &[r, a]), r.bits))
}

/// `lowered` with the features every intrinsic needs: NEON, and the half-precision
/// instructions where a value has lanes of `f16`.
fn finish(lowered: Lowered, types: &[Lanes]) -> Lowered {
    let half = types.iter().any(|it| it.float && it.bits == 16);
    let mut features = vec!["neon"];
    features.extend(half.then_some("fp16"));
    features.extend(lowered.features);
    Lowered { features, ..lowered }
}

/// Rounds each lane of a vector of floats to an integral float, as `instruction` does
/// (`frintz`, `frinta`, `frintx`): what LLVM selects on AArch64 for the rounding `simd_*`
/// intrinsics rustc lowers to its `trunc`, `round` and `rint`.
pub(super) fn rounding(mnemonic: &str, lanes: Lanes) -> Option<Lowered> {
    Some(finish(instruction_of(mnemonic, lanes, lanes)?, &[lanes]))
}

/// The generic intrinsic `op` (`simd_add`, `simd_lt`, `simd_cast`, ...) where its operands'
/// lanes `a` or its result's `r` are halves, as AArch64's LLVM selects it in a function with
/// the half-precision instructions: one instruction on the lanes, which rounds once and
/// propagates a NaN as the instruction does. `signed` is whether the integers of a
/// `simd_cast` are.
pub(super) fn half_vector(op: &str, r: Lanes, a: Lanes, signed: bool) -> Option<Lowered> {
    let half = |it: Lanes| it.float && it.bits == 16;
    if r.count != a.count || !r.is_register() || !a.is_register() || !(half(r) || half(a)) {
        return None;
    }
    let lowered = match op {
        "simd_add" | "simd_sub" | "simd_mul" | "simd_div" if r == a => {
            let text = format!("f{} {}, {}, {}", &op[5..], r.reg(0), a.reg(1), a.reg(2));
            instruction(&[], text, [a, a], [r])
        }
        // A true lane is all ones. `le` and `lt` are `ge` and `gt` of the operands swapped.
        "simd_eq" | "simd_ge" | "simd_gt" | "simd_le" | "simd_lt"
            if r.is_int() && r.bits == 16 && half(a) =>
        {
            let (mnemonic, first, second) = match op {
                "simd_eq" => ("fcmeq", 1, 2),
                "simd_ge" => ("fcmge", 1, 2),
                "simd_gt" => ("fcmgt", 1, 2),
                "simd_le" => ("fcmge", 2, 1),
                _ => ("fcmgt", 2, 1),
            };
            let text = format!("{mnemonic} {}, {}, {}", r.reg(0), a.reg(first), a.reg(second));
            instruction(&[], text, [a, a], [r])
        }
        "simd_neg" | "simd_fabs" | "simd_fsqrt" | "simd_floor" | "simd_ceil" if r == a => {
            let mnemonic = match op {
                "simd_neg" => "fneg",
                "simd_fabs" => "fabs",
                "simd_fsqrt" => "fsqrt",
                "simd_floor" => "frintm",
                _ => "frintp",
            };
            instruction_of(mnemonic, r, a)
        }
        // `a * b + c`, rounded once, into the register of `c`. LLVM's pattern names the
        // factors the other way round, which decides which NaN the instruction keeps.
        "simd_fma" if r == a => {
            let text = format!("mov v0.16b, v3.16b\nfmla {}, {}, {}", r.reg(0), a.reg(2), a.reg(1));
            instruction(&[], text, [a, a, a], [r])
        }
        // A float out of the integer's range is undefined for `simd_cast`; the instruction
        // saturates it and makes a NaN 0.
        "simd_cast" => {
            let mnemonic = match (r.float, a.float, r.bits, a.bits) {
                (true, true, 16, 32) => "fcvtn",
                (true, true, 32, 16) => "fcvtl",
                (true, false, 16, 16) if signed => "scvtf",
                (true, false, 16, 16) => "ucvtf",
                (false, true, 16, 16) if signed => "fcvtzs",
                (false, true, 16, 16) => "fcvtzu",
                _ => return None,
            };
            instruction_of(mnemonic, r, a)
        }
        _ => None,
    }?;
    Some(finish(lowered, &[r, a]))
}

/// What `lower` builds a [`Lowered`] of.
fn instruction(
    features: &[&'static str],
    text: String,
    args: impl IntoIterator<Item = Lanes>,
    results: impl IntoIterator<Item = Lanes>,
) -> Option<Lowered> {
    Some(Lowered {
        features: features.to_vec(),
        text,
        args: args.into_iter().map(|it| Some(it.bytes())).collect(),
        results: results.into_iter().map(Lanes::bytes).collect(),
        immediate: None,
    })
}

/// The instruction that writes its first operand from the others, which the intrinsic takes
/// first: `v0` starts as the first argument, `v1`.
const ACCUMULATE: &str = "mov v0.16b, v1.16b\n";

/// `llvm.aarch64.neon.{op}.{types}`, and the few of `llvm.aarch64.` that are instructions of
/// the same kind (`frint32x.f64`, `fjcvtzs`).
fn neon(op: &str, types: &[Lanes]) -> Option<Lowered> {
    match (op, types) {
        // Lane by lane, of two operands of the type of the result. A float's NaN, a signed
        // zero, saturation and the rounding of the halving and shifting ones are the
        // instruction's.
        (
            "fabd" | "fmax" | "fmin" | "fmaxnm" | "fminnm" | "fmaxp" | "fminp" | "fmaxnmp"
            | "fminnmp" | "faddp" | "fmulx" | "frecps" | "frsqrts" | "famax" | "famin"
            | "fp8.fscale",
            &[t],
        ) if t.float => {
            let features: &[&str] = match op {
                "famax" | "famin" => &["faminmax"],
                "fp8.fscale" => &["fp8"],
                _ => &[],
            };
            let op = op.trim_start_matches("fp8.");
            let text = format!("{op} {}, {}, {}", t.reg(0), t.reg(1), t.reg(2));
            // `fscale`'s second operand is the vector of integers of the same arrangement.
            instruction(features, text, [t, t], [t])
        }
        (
            "sabd" | "uabd" | "shadd" | "uhadd" | "shsub" | "uhsub" | "srhadd" | "urhadd"
            | "sqrdmulh" | "sqadd" | "uqadd" | "sqsub" | "uqsub" | "sshl" | "ushl" | "srshl"
            | "urshl" | "sqshl" | "uqshl" | "sqrshl" | "uqrshl" | "pmul",
            &[t],
        ) if t.is_int() => {
            let text = format!("{op} {}, {}, {}", t.reg(0), t.reg(1), t.reg(2));
            instruction(&[], text, [t, t], [t])
        }
        // The absolute comparisons: all ones in a lane where it holds.
        ("facge" | "facgt", &[r, a]) if r.is_int() && a.float && r.bytes() == a.bytes() => {
            let text = format!("{op} {}, {}, {}", r.reg(0), a.reg(1), a.reg(2));
            instruction(&[], text, [a, a], [r])
        }
        // Lane by lane, of one operand.
        ("cls" | "sqabs" | "sqneg" | "abs" | "urecpe" | "ursqrte", &[t]) if t.is_int() => {
            instruction(&[], format!("{op} {}, {}", t.reg(0), t.reg(1)), [t], [t])
        }
        (
            "frecpe" | "frsqrte" | "frecpx" | "frint32x" | "frint32z" | "frint64x" | "frint64z",
            &[t],
        ) if t.float => {
            let features: &[&str] = if op.starts_with("frint") { &["frintts"] } else { &[] };
            instruction(features, format!("{op} {}, {}", t.reg(0), t.reg(1)), [t], [t])
        }
        // To integers of the width of the float, rounded as the name says (`a` to nearest
        // with ties away, `m` down, `n` to nearest with ties to even, `p` up), saturated,
        // a NaN 0.
        (
            "fcvtas" | "fcvtau" | "fcvtms" | "fcvtmu" | "fcvtns" | "fcvtnu" | "fcvtps" | "fcvtpu",
            &[r, a],
        ) if r.is_int() && a.float && r.count == a.count && r.bits == a.bits => {
            instruction(&[], format!("{op} {}, {}", r.reg(0), a.reg(1)), [a], [r])
        }
        // A half to an integer of 32 or 64 bits, which LLVM converts into a general register.
        (
            "fcvtas" | "fcvtau" | "fcvtms" | "fcvtmu" | "fcvtns" | "fcvtnu" | "fcvtps" | "fcvtpu",
            &[r, a],
        ) if r.is_int() && r.count == 1 && matches!(r.bits, 32 | 64) && a == HALF => {
            let (general, vector) = if r.bits == 32 { ('w', 's') } else { ('x', 'd') };
            let text = format!("{op} {general}9, h1\nfmov {vector}0, {general}9");
            instruction(&[], text, [a], [r])
        }
        // Of two halves, into 32 bits: the instruction writes 16 and clears the rest.
        ("facge" | "facgt", &[r, a]) if r == Lanes::single(32) && a == HALF => {
            instruction(&[], format!("{op} h0, h1, h2"), [a, a], [r])
        }
        // To single precision, rounded to odd.
        ("fcvtxn", &[r, a]) if r.float && a.float && r.count == a.count && a.bits == 64 => {
            instruction(&[], format!("fcvtxn {}, {}", r.reg(0), a.reg(1)), [a], [r])
        }
        // Between floats and fixed-point integers of `#imm` fraction bits; to an integer
        // rounded toward zero, saturated, a NaN 0. A half and an integer of 32 or 64 bits
        // are converted in registers of halves, as LLVM selects them: of the integer's low 16
        // bits, to a result whose bits above 16 are 0.
        ("vcvtfxs2fp" | "vcvtfxu2fp" | "vcvtfp2fxs" | "vcvtfp2fxu", &[r, a])
            if r.float != a.float && r.count == a.count && r.is_scalar_or_register() =>
        {
            let mnemonic = match op {
                "vcvtfxs2fp" => "scvtf",
                "vcvtfxu2fp" => "ucvtf",
                "vcvtfp2fxs" => "fcvtzs",
                _ => "fcvtzu",
            };
            let (float, int) = if r.float { (r, a) } else { (a, r) };
            let text = if r.bits == a.bits {
                format!("{mnemonic} {}, {}, #{{imm}}", r.reg(0), a.reg(1))
            } else if float == HALF && matches!(int.bits, 32 | 64) {
                format!("{mnemonic} h0, h1, #{{imm}}")
            } else {
                return None;
            };
            taking(instruction(&[], text, [a], [r]), Immediate::of(1, 1..=float.bits))
        }
        // The second operand shifted left by `#imm` into the first, which keeps the bits
        // below the shift.
        ("vsli", &[t]) if t.is_int() && t.is_scalar_or_register() => {
            let text = format!("{ACCUMULATE}sli {}, {}, #{{imm}}", t.reg(0), t.reg(2));
            taking(instruction(&[], text, [t, t], [t]), Immediate::of(2, 0..=t.bits - 1))
        }
        // Signed lanes shifted left by `#imm` to unsigned ones, saturated. `core::arch`
        // passes the shift as a vector each lane of which is it.
        ("sqshlu", &[t]) if t.is_int() && t.is_scalar_or_register() => {
            let text = format!("sqshlu {}, {}, #{{imm}}", t.reg(0), t.reg(1));
            let shift = Immediate { splat: Some(t.bits / 8), ..Immediate::of(1, 0..=t.bits - 1) };
            taking(instruction(&[], text, [t], [t]), shift)
        }
        // JavaScript's conversion of a double to a 32-bit integer, modulo 2^32.
        ("fjcvtzs", &[]) => {
            let (double, int) = (Lanes { count: 1, bits: 64, float: true }, Lanes::single(32));
            let text = "fjcvtzs w9, d1\nfmov s0, w9".to_owned();
            instruction(&["jsconv"], text, [double], [int])
        }
        // Across the lanes. LLVM takes a pair with the pairwise instruction's form for a
        // scalar, and adds four singles as two pairwise additions: (0 + 1) + (2 + 3).
        ("fmaxv" | "fminv" | "fmaxnmv" | "fminnmv" | "faddv", &[r, a])
            if r.float && r.count == 1 && a.float && a.bits == r.bits =>
        {
            let text = match (op, a.count) {
                (_, 2) => format!("{}p {}, {}", &op[..op.len() - 1], r.reg(0), a.reg(1)),
                ("faddv", 4) if a.bits == 32 => {
                    format!("faddp v1.4s, v1.4s, v1.4s\nfaddp {}, v1.2s", r.reg(0))
                }
                ("faddv", _) => return None,
                (_, 4 | 8) if a.bits <= 32 && a.count * a.bits <= 128 => {
                    format!("{op} {}, {}", r.reg(0), a.reg(1))
                }
                _ => return None,
            };
            instruction(&[], text, [a], [r])
        }
        // Into the first operand.
        ("sqrdmlah" | "sqrdmlsh", &[t]) if t.is_int() => {
            let text = format!("{ACCUMULATE}{op} {}, {}, {}", t.reg(0), t.reg(2), t.reg(3));
            instruction(&["rdm"], text, [t, t, t], [t])
        }
        ("suqadd" | "usqadd", &[t]) if t.is_int() => {
            let text = format!("{ACCUMULATE}{op} {}, {}", t.reg(0), t.reg(2));
            instruction(&[], text, [t, t], [t])
        }
        // Each 32-bit lane plus the dot product of four bytes of each operand.
        ("sdot" | "udot" | "usdot", &[r, a])
            if r.is_int() && r.bits == 32 && a.bits == 8 && a.count == 4 * r.count =>
        {
            let feature = if op == "usdot" { "i8mm" } else { "dotprod" };
            let text = format!("{ACCUMULATE}{op} {}, {}, {}", r.reg(0), a.reg(2), a.reg(3));
            instruction(&[feature], text, [r, a, a], [r])
        }
        // The 2x2 matrix of 32-bit lanes plus the product of a 2x8 by an 8x2 of bytes.
        ("smmla" | "ummla" | "usmmla", &[r, a]) if r.bytes() == 16 && a == SIXTEEN => {
            let text = format!("{ACCUMULATE}{op} v0.4s, v2.16b, v3.16b");
            instruction(&["i8mm"], text, [r, a, a], [r])
        }
        // Each single plus (or minus) the product of a pair of halves widened: of the low
        // halves of the operands, or (`2`) the high ones.
        ("fmlal" | "fmlal2" | "fmlsl" | "fmlsl2", &[r, a])
            if r.float && r.bits == 32 && a.float && a.bits == 16 && a.count == 2 * r.count =>
        {
            let halves = r.count;
            let text = format!("{ACCUMULATE}{op} {}, v2.{halves}h, v3.{halves}h", r.reg(0));
            instruction(&["fhm"], text, [r, a, a], [r])
        }
        // Complex numbers as pairs of lanes, the second rotated as the name says.
        ("vcmla.rot0" | "vcmla.rot90" | "vcmla.rot180" | "vcmla.rot270", &[t])
            if t.float && t.count >= 2 =>
        {
            let rotation = &op["vcmla.rot".len()..];
            let text =
                format!("{ACCUMULATE}fcmla {}, {}, {}, #{rotation}", t.reg(0), t.reg(2), t.reg(3));
            instruction(&["fcma"], text, [t, t, t], [t])
        }
        ("vcadd.rot90" | "vcadd.rot270", &[t]) if t.float && t.count >= 2 => {
            let rotation = &op["vcadd.rot".len()..];
            let text = format!("fcadd {}, {}, {}, #{rotation}", t.reg(0), t.reg(1), t.reg(2));
            instruction(&["fcma"], text, [t, t], [t])
        }
        // To lanes of half the width, saturated (`sqxtun` a signed lane to an unsigned one),
        // or the high half of the sum or difference, rounded.
        ("sqxtn" | "uqxtn" | "sqxtun", &[r]) if r.is_int() && r.bytes() == 8 => {
            let a = r.with_bits(2 * r.bits);
            instruction(&[], format!("{op} {}, {}", r.reg(0), a.reg(1)), [a], [r])
        }
        ("scalar.sqxtn" | "scalar.uqxtn" | "scalar.sqxtun", &[r, a])
            if r.is_int() && a.is_int() && r.count == 1 && a.count == 1 && a.bits == 2 * r.bits =>
        {
            let op = &op["scalar.".len()..];
            instruction(&[], format!("{op} {}, {}", r.reg(0), a.reg(1)), [a], [r])
        }
        // Shifted right by `#imm`, rounded, into lanes of half the width.
        ("rshrn", &[r]) if r.is_int() && r.bytes() == 8 => {
            let a = r.with_bits(2 * r.bits);
            let text = format!("rshrn {}, {}, #{{imm}}", r.reg(0), a.reg(1));
            taking(instruction(&[], text, [a], [r]), Immediate::of(1, 1..=r.bits))
        }
        ("raddhn" | "rsubhn", &[r]) if r.is_int() && r.bytes() == 8 => {
            let a = r.with_bits(2 * r.bits);
            let text = format!("{op} {}, {}, {}", r.reg(0), a.reg(1), a.reg(2));
            instruction(&[], text, [a, a], [r])
        }
        // Products without carries, of bytes into 16 bits and of 64 bits into 128.
        ("pmull", &[r]) if r == (Lanes { count: 8, bits: 16, float: false }) => {
            let a = r.with_bits(8);
            let text = format!("pmull {}, {}, {}", r.reg(0), a.reg(1), a.reg(2));
            instruction(&[], text, [a, a], [r])
        }
        ("pmull64", &[]) => {
            let text = "pmull v0.1q, v1.1d, v2.1d".to_owned();
            Some(Lowered {
                features: vec!["aes"],
                text,
                args: vec![Some(8); 2],
                results: vec![16],
                immediate: None,
            })
        }
        // Twice the product of two 32-bit integers in 64 bits, saturated.
        ("sqdmulls.scalar", &[]) => {
            let text = "sqdmull d0, s1, s2".to_owned();
            instruction(&[], text, [Lanes::single(32), Lanes::single(32)], [Lanes::single(64)])
        }
        // A lane is the byte its index names in the tables taken as one, 0 past their end
        // (`tbl`) or the lane of the first operand (`tbx`).
        ("tbl2" | "tbl3" | "tbl4", &[t]) if t.bits == 8 && t.is_register() => {
            let n = u32::from(op.as_bytes()[3] - b'0');
            let text = format!("tbl {}, {}, {}", t.reg(0), list(SIXTEEN, 1, n), t.reg(n + 1));
            let tables = std::iter::repeat_n(SIXTEEN, n as usize);
            instruction(&[], text, tables.chain([t]), [t])
        }
        ("tbx2" | "tbx3" | "tbx4", &[t]) if t.bits == 8 && t.is_register() => {
            let n = u32::from(op.as_bytes()[3] - b'0');
            let text =
                format!("{ACCUMULATE}tbx {}, {}, {}", t.reg(0), list(SIXTEEN, 2, n), t.reg(n + 2));
            let tables = std::iter::repeat_n(SIXTEEN, n as usize);
            instruction(&[], text, [t].into_iter().chain(tables).chain([t]), [t])
        }
        // Loads of `n` vectors at once: interleaved (`ld2`, which LLVM takes as `ld1` for
        // vectors of one lane, where the two are the same), each lane of each register the
        // one element (`ld2r`), or consecutive (`ld1x2`).
        ("ld2" | "ld3" | "ld4" | "ld2r" | "ld3r" | "ld4r" | "ld1x2" | "ld1x3" | "ld1x4", &[t])
            if t.is_register() =>
        {
            let n = u32::from(op.as_bytes()[if op.starts_with("ld1x") { 4 } else { 2 }] - b'0');
            let mnemonic = match op {
                _ if op.starts_with("ld1x") => "ld1",
                _ if op.ends_with('r') => op,
                _ if t.count == 1 => "ld1",
                _ => op,
            };
            let text = format!("{mnemonic} {}, [x1]", list(t, 0, n));
            Some(Lowered {
                features: Vec::new(),
                text,
                args: vec![None],
                results: vec![t.bytes(); n as usize],
                immediate: None,
            })
        }
        // Lane `#imm` of each of `n` vectors loaded from consecutive elements, the others
        // those the vectors come in with (from `v1` up, out of `v0` up).
        ("ld2lane" | "ld3lane" | "ld4lane", &[t]) if t.is_register() => {
            let n = u32::from(op.as_bytes()[2] - b'0');
            let moves: String = (0..n).map(|k| format!("mov v{k}.16b, v{}.16b\n", k + 1)).collect();
            let size = size_letter(t.bits);
            let lanes: Vec<String> = (0..n).map(|k| format!("v{k}.{size}")).collect();
            let text = format!("{moves}ld{n} {{{}}}[{{imm}}], [x{}]", lanes.join(", "), n + 1);
            let mut args = vec![Some(t.bytes()); n as usize];
            args.push(None);
            let results = vec![t.bytes(); n as usize];
            let lowered = Lowered { features: Vec::new(), text, args, results, immediate: None };
            taking(Some(lowered), Immediate::of(n as usize, 0..=t.count - 1))
        }
        ("st2lane" | "st3lane" | "st4lane", &[t]) if t.is_register() => {
            let n = u32::from(op.as_bytes()[2] - b'0');
            let size = size_letter(t.bits);
            let lanes: Vec<String> = (1..=n).map(|k| format!("v{k}.{size}")).collect();
            let text = format!("st{n} {{{}}}[{{imm}}], [x{}]", lanes.join(", "), n + 1);
            let mut args = vec![Some(t.bytes()); n as usize];
            args.push(None);
            let lowered =
                Lowered { features: Vec::new(), text, args, results: Vec::new(), immediate: None };
            taking(Some(lowered), Immediate::of(n as usize, 0..=t.count - 1))
        }
        // Stores of `n` vectors, the address last: consecutive (`st1x2`) or interleaved.
        ("st1x2" | "st1x3" | "st1x4" | "st2" | "st3" | "st4", &[t]) if t.is_register() => {
            let n = u32::from(op.as_bytes()[op.len() - 1] - b'0');
            let mnemonic = match op {
                _ if op.starts_with("st1x") || t.count == 1 => "st1",
                _ => op,
            };
            let text = format!("{mnemonic} {}, [x{}]", list(t, 1, n), n + 1);
            let mut args = vec![Some(t.bytes()); n as usize];
            args.push(None);
            Some(Lowered { features: Vec::new(), text, args, results: Vec::new(), immediate: None })
        }
        _ => None,
    }
}

/// `llvm.aarch64.crypto.{op}.{types}` beyond the AES and SHA-2 ones the code generator has.
fn crypto(op: &str, types: &[Lanes]) -> Option<Lowered> {
    let vector = SIXTEEN;
    let word = Lanes::single(32);
    match (op, types) {
        // `eor3`: the three xored. `bcax`: the first xored with the second and not the third.
        ("eor3u" | "eor3s" | "bcaxu" | "bcaxs", &[t]) if t.is_int() && t.bytes() == 16 => {
            let text = format!("{} v0.16b, v1.16b, v2.16b, v3.16b", &op[..op.len() - 1]);
            instruction(&["sha3"], text, [t, t, t], [t])
        }
        ("rax1", &[]) => {
            instruction(&["sha3"], "rax1 v0.2d, v1.2d, v2.2d".to_owned(), [vector; 2], [vector])
        }
        // The two xored, each 64-bit lane rotated right by `#imm`.
        ("xar", &[]) => {
            let text = "xar v0.2d, v1.2d, v2.2d, #{imm}".to_owned();
            taking(instruction(&["sha3"], text, [vector; 2], [vector]), Immediate::of(2, 0..=63))
        }
        // A round of SM3 of the state, the next words and word `#imm` of the third operand.
        ("sm3tt1a" | "sm3tt1b" | "sm3tt2a" | "sm3tt2b", &[]) => {
            let text = format!("{ACCUMULATE}{op} v0.4s, v2.4s, v3.s[{{imm}}]");
            taking(instruction(&["sm4"], text, [vector; 3], [vector]), Immediate::of(3, 0..=3))
        }
        // Four rounds of SHA-1 of the hash, its fifth word and the next four words.
        ("sha1c" | "sha1m" | "sha1p", &[]) => {
            let text = format!("{ACCUMULATE}{op} q0, s2, v3.4s");
            instruction(&["sha2"], text, [vector, word, vector], [vector])
        }
        ("sha1h", &[]) => instruction(&["sha2"], "sha1h s0, s1".to_owned(), [word], [word]),
        ("sm3ss1", &[]) => {
            let text = "sm3ss1 v0.4s, v1.4s, v2.4s, v3.4s".to_owned();
            instruction(&["sm4"], text, [vector; 3], [vector])
        }
        ("sm3partw1" | "sm3partw2", &[]) => {
            let text = format!("{ACCUMULATE}{op} v0.4s, v2.4s, v3.4s");
            instruction(&["sm4"], text, [vector; 3], [vector])
        }
        ("sm4e", &[]) => {
            let text = format!("{ACCUMULATE}sm4e v0.4s, v2.4s");
            instruction(&["sm4"], text, [vector; 2], [vector])
        }
        ("sm4ekey", &[]) => {
            instruction(&["sm4"], "sm4ekey v0.4s, v1.4s, v2.4s".to_owned(), [vector; 2], [vector])
        }
        _ => None,
    }
}

/// The generic intrinsics `core::arch` calls on AArch64's vectors and floats, as AArch64's
/// LLVM selects them: rounding to an integral float in the current mode (`nearbyint`) or to
/// even (`roundeven`), and to integers saturated, a NaN 0 (`fptosi.sat`).
fn generic(op: &str, types: &[Lanes]) -> Option<Lowered> {
    match (op, types) {
        ("roundeven" | "nearbyint", &[t]) if t.float && t.is_scalar_or_register() => {
            let mnemonic = if op == "roundeven" { "frintn" } else { "frinti" };
            instruction_of(mnemonic, t, t)
        }
        ("fptosi.sat" | "fptoui.sat", &[r, a])
            if r.is_int() && a.float && r.count == a.count && r.bits == a.bits =>
        {
            let mnemonic = if op == "fptosi.sat" { "fcvtzs" } else { "fcvtzu" };
            instruction_of(mnemonic, r, a)
        }
        _ => None,
    }
}

/// `{mnemonic} {r}, {a}` on one operand.
fn instruction_of(mnemonic: &str, r: Lanes, a: Lanes) -> Option<Lowered> {
    if !(r.is_scalar_or_register() && a.is_scalar_or_register()) {
        return None;
    }
    instruction(&[], format!("{mnemonic} {}, {}", r.reg(0), a.reg(1)), [a], [r])
}

impl Lanes {
    /// An integer of `bits`, alone.
    fn single(bits: u32) -> Lanes {
        Lanes { count: 1, bits, float: false }
    }

    /// Whether this is a scalar or a whole vector register (or its low half).
    fn is_scalar_or_register(self) -> bool {
        self.count == 1 || self.is_register()
    }

    /// The lanes of a vector of `count` floats of `bits`, as a SIMD type's layout says them.
    pub(super) fn floats(bits: u32, count: u32) -> Lanes {
        Lanes { count, bits, float: true }
    }
}

/// A fused multiply-add of `core::arch`'s NEON, `vfm{a,s}[s|d|h][q][_n|_lane|_laneq]_f{16,32,64}`
/// (`a + b * c`, `a - b * c`), as its name and `LANE` say it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Fused {
    subtracts: bool,
    /// Whether the name says a scalar (`vfmad_lane_f64`), which the body takes a lane of
    /// `c` for, where a vector's (`vfma_lane_f64` too) splats that lane.
    scalar: bool,
    /// The lanes of `a`, `b` and the result.
    lanes: Lanes,
    /// What `c` is: the same vector, a scalar (`_n`), or a vector of 64 or 128 bits one lane
    /// of which is the factor (`_lane`, `_laneq`).
    c: Factor,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Factor {
    Each,
    Scalar,
    Lane { bytes: u32, lane: u32 },
}

impl Fused {
    fn parse(name: &str, lane: Option<u32>) -> Option<Fused> {
        let rest = name.strip_prefix("vfm")?;
        let subtracts = match rest.as_bytes().first()? {
            b'a' => false,
            b's' => true,
            _ => return None,
        };
        let rest = &rest[1..];
        let (scalar, rest) = match rest.as_bytes().first()? {
            b's' | b'd' | b'h' => (true, &rest[1..]),
            _ => (false, rest),
        };
        let (quad, rest) = match rest.strip_prefix('q') {
            Some(rest) => (true, rest),
            None => (false, rest),
        };
        let rest = rest.strip_prefix('_')?;
        let (form, element) = rest.rsplit_once('_').unwrap_or(("", rest));
        let bits: u32 = element.strip_prefix('f')?.parse().ok()?;
        if !matches!(bits, 16 | 32 | 64) || scalar && quad {
            return None;
        }
        let count = if scalar { 1 } else { (if quad { 128 } else { 64 }) / bits };
        let c = match (form, lane) {
            ("", None) => Factor::Each,
            ("n", None) if !scalar => Factor::Scalar,
            ("lane", Some(lane)) => Factor::Lane { bytes: 8, lane },
            ("laneq", Some(lane)) => Factor::Lane { bytes: 16, lane },
            _ => return None,
        };
        if let Factor::Lane { bytes, lane } = c
            && lane >= bytes * 8 / bits
        {
            return None;
        }
        Some(Fused { subtracts, scalar, lanes: Lanes::floats(bits, count), c })
    }

    /// Whether the body splats `c` with `vdup_n_f32` or `vdupq_n_f64` and the like, whose
    /// `floatNxM_t::splat` is `#[inline(always)]`. The halves' `vdup_n_f16` is a call of its
    /// own at every opt-level 0 build.
    fn splats_c(self) -> bool {
        !self.scalar && self.c != Factor::Each && self.lanes.bits != 16
    }

    /// Whether the instruction takes `c` as its first factor, which it negates when it
    /// subtracts, and `b` as its second. That decides which of two NaNs comes out, and which
    /// one's sign the negation flips.
    fn c_first(self) -> bool {
        match (self.lanes.count, self.c) {
            // `fmla a, c, b`, as `simd_fma` of vectors is. The halves' bodies still call
            // `vget_lane_f16` and `vdup_n_f16`, so LLVM sees vectors there too.
            (2.., Factor::Each) => true,
            (2.., _) => self.lanes.bits == 16,
            // Lane 0 of a 128-bit vector, subtracted from a scalar, is `fmsub` with `c` first;
            // lane 1 is `fmls d0, d1, v2.d[1]`, `b` first.
            (_, Factor::Lane { bytes: 16, lane: 0 }) => self.subtracts && self.lanes.bits != 16,
            _ => false,
        }
    }

    /// The instruction on `a` in `v1`, `b` in `v2` and `c` in `v3`, which leaves the result
    /// in `v0`.
    fn instruction(self) -> Lowered {
        let t = self.lanes;
        let size = size_letter(t.bits);
        let bytes = |lanes: Lanes| Some(lanes.bytes());
        let c_bytes = match self.c {
            Factor::Each => t.bytes(),
            Factor::Scalar => t.bits / 8,
            Factor::Lane { bytes, .. } => bytes,
        };
        let lane = match self.c {
            Factor::Lane { lane, .. } => lane,
            _ => 0,
        };
        let text = if t.count > 1 {
            let mnemonic = if self.subtracts { "fmls" } else { "fmla" };
            let (r, b) = (t.reg(0), t.reg(2));
            match (self.c, self.c_first()) {
                (Factor::Each, _) => format!("{ACCUMULATE}{mnemonic} {r}, {}, {b}", t.reg(3)),
                (_, true) => format!(
                    "dup {}, v3.{size}[{lane}]\n{ACCUMULATE}{mnemonic} {r}, {}, {b}",
                    t.reg(4),
                    t.reg(4)
                ),
                (_, false) => format!("{ACCUMULATE}{mnemonic} {r}, {b}, v3.{size}[{lane}]"),
            }
        } else if lane == 0 {
            let mnemonic = if self.subtracts { "fmsub" } else { "fmadd" };
            let (first, second) = if self.c_first() { (3, 2) } else { (2, 3) };
            format!("{mnemonic} {size}0, {size}{first}, {size}{second}, {size}1")
        } else {
            let mnemonic = if self.subtracts { "fmls" } else { "fmla" };
            format!("{ACCUMULATE}{mnemonic} {size}0, {size}2, v3.{size}[{lane}]")
        };
        let lowered = Lowered {
            features: Vec::new(),
            text,
            args: vec![bytes(t), bytes(t), Some(c_bytes)],
            results: vec![t.bytes()],
            immediate: None,
        };
        finish(lowered, &[t])
    }
}

/// The body of `core::arch`'s NEON fused multiply-add `name` (`vfmaq_laneq_f64`), of `LANE`
/// `lane`, as the instruction LLVM selects for it (`neon-fma-llvm.txt`). rustc compiles core's
/// optimized MIR of it, with what it calls built in, so LLVM sees the negation and the splat
/// of `c` and picks the instruction by them: which factor comes first, and which one `fmls`
/// or `fmsub` negates. Built of its calls, as ra-jit's MIR has it, it keeps another NaN or
/// flips the sign of another.
pub(super) fn fused(name: &str, lane: Option<u32>) -> Option<Lowered> {
    Some(Fused::parse(name, lane)?.instruction())
}

/// Whether the body of the fused multiply-add `name` of `LANE` `lane` ([`fused`]) splats
/// `c` with an `#[inline(always)]` function. The instruction is of the splat built in, and
/// rustc builds it into every codegen unit, except where `-C link-dead-code` keeps one copy
/// in one unit: LLVM sees the splat only in that unit, and elsewhere picks the instruction
/// for a vector it knows nothing of (`fmla v0.4s, v1.4s, v2.4s` of the splat's result).
pub(super) fn splats_c(name: &str, lane: Option<u32>) -> bool {
    Fused::parse(name, lane).is_some_and(Fused::splats_c)
}

/// `Lowered::text` for a list of names, for a test: each name and what it lowers to, or that
/// it does not.
#[cfg(test)]
fn describe(names: &[&str]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for name in names {
        match lower(name) {
            Some(it) => {
                _ = writeln!(out, "{name}: {} {:?}", it.text.replace('\n', "; "), it.features)
            }
            None => _ = writeln!(out, "{name}: none"),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrowing_shifts_keep_the_signedness_rounding_and_lane_width() {
        for op in ["sqshrn", "uqshrn", "sqshrun", "sqrshrn", "uqrshrn", "sqrshrun"] {
            for (ty, result, input, bytes, limit) in [
                ("v8i8", "v0.8b", "v1.8h", 8, 8),
                ("v4i16", "v0.4h", "v1.4s", 8, 16),
                ("v2i32", "v0.2s", "v1.2d", 8, 32),
                ("i8", "b0", "h1", 1, 8),
                ("i16", "h0", "s1", 2, 16),
                ("i32", "s0", "d1", 4, 32),
            ] {
                let name = format!("aarch64.neon.{op}.{ty}");
                let (lowered, max_shift) = narrowing_shift(&name).unwrap();
                assert_eq!(lowered.text, format!("{op} {result}, {input}, #{{shift}}"));
                assert_eq!(lowered.args, [Some(2 * bytes)]);
                assert_eq!(lowered.results, [bytes]);
                assert_eq!(lowered.features, ["neon"]);
                assert_eq!(max_shift, limit);
            }
        }
        for name in [
            "aarch64.neon.sqshrun.v16i8",
            "aarch64.neon.sqshrun.v4i32",
            "aarch64.neon.sqshrun.v1i64",
            "aarch64.neon.sqshrun.v4f16",
            "aarch64.neon.sqshrun.v4i16.i32",
            "aarch64.neon.sqshl.v4i16",
        ] {
            assert!(narrowing_shift(name).is_none(), "{name}");
        }
    }

    #[test]
    fn the_lanes_a_name_says_are_the_registers_the_instruction_names() {
        let described = describe(&[
            "aarch64.neon.fmaxnmv.f32.v2f32",
            "aarch64.neon.faddv.f32.v4f32",
            "aarch64.neon.fminv.f32.v4f32",
            "aarch64.neon.sqadd.i32",
            "aarch64.neon.fabd.v1f64",
            "aarch64.neon.facge.v4i16.v4f16",
            "aarch64.neon.sqxtn.v8i8",
            "aarch64.neon.sdot.v4i32.v16i8",
            "aarch64.neon.fmlal2.v2f32.v4f16",
            "aarch64.neon.tbx3.v8i8",
            "aarch64.neon.ld2.v1i64.p0",
            "aarch64.neon.ld4r.v8i8.p0.p0",
            "aarch64.neon.st1x3.v2f64.p0",
            "aarch64.neon.vcmla.rot270.v4f32",
            "aarch64.crypto.bcaxs.v8i16",
            "fptoui.sat.v1i64.v1f64",
            "roundeven.v8f16",
            "aarch64.neon.faddv.f64.v4f64",
            "aarch64.neon.fcvtps.i64.f16",
            "aarch64.neon.fcvtau.i32.f16",
            "aarch64.neon.facgt.i32.f16",
        ]);
        let expected = "\
aarch64.neon.fmaxnmv.f32.v2f32: fmaxnmp s0, v1.2s [\"neon\"]
aarch64.neon.faddv.f32.v4f32: faddp v1.4s, v1.4s, v1.4s; faddp s0, v1.2s [\"neon\"]
aarch64.neon.fminv.f32.v4f32: fminv s0, v1.4s [\"neon\"]
aarch64.neon.sqadd.i32: sqadd s0, s1, s2 [\"neon\"]
aarch64.neon.fabd.v1f64: fabd d0, d1, d2 [\"neon\"]
aarch64.neon.facge.v4i16.v4f16: facge v0.4h, v1.4h, v2.4h [\"neon\", \"fp16\"]
aarch64.neon.sqxtn.v8i8: sqxtn v0.8b, v1.8h [\"neon\"]
aarch64.neon.sdot.v4i32.v16i8: mov v0.16b, v1.16b; sdot v0.4s, v2.16b, v3.16b [\"neon\", \"dotprod\"]
aarch64.neon.fmlal2.v2f32.v4f16: mov v0.16b, v1.16b; fmlal2 v0.2s, v2.2h, v3.2h [\"neon\", \"fp16\", \"fhm\"]
aarch64.neon.tbx3.v8i8: mov v0.16b, v1.16b; tbx v0.8b, {v2.16b, v3.16b, v4.16b}, v5.8b [\"neon\"]
aarch64.neon.ld2.v1i64.p0: ld1 {v0.1d, v1.1d}, [x1] [\"neon\"]
aarch64.neon.ld4r.v8i8.p0.p0: ld4r {v0.8b, v1.8b, v2.8b, v3.8b}, [x1] [\"neon\"]
aarch64.neon.st1x3.v2f64.p0: st1 {v1.2d, v2.2d, v3.2d}, [x4] [\"neon\"]
aarch64.neon.vcmla.rot270.v4f32: mov v0.16b, v1.16b; fcmla v0.4s, v2.4s, v3.4s, #270 [\"neon\", \"fcma\"]
aarch64.crypto.bcaxs.v8i16: bcax v0.16b, v1.16b, v2.16b, v3.16b [\"neon\", \"sha3\"]
fptoui.sat.v1i64.v1f64: fcvtzu d0, d1 [\"neon\"]
roundeven.v8f16: frintn v0.8h, v1.8h [\"neon\", \"fp16\"]
aarch64.neon.faddv.f64.v4f64: none
aarch64.neon.fcvtps.i64.f16: fcvtps x9, h1; fmov d0, x9 [\"neon\", \"fp16\"]
aarch64.neon.fcvtau.i32.f16: fcvtau w9, h1; fmov s0, w9 [\"neon\", \"fp16\"]
aarch64.neon.facgt.i32.f16: facgt h0, h1, h2 [\"neon\", \"fp16\"]
";
        assert_eq!(described, expected);
    }

    #[test]
    fn a_generic_intrinsic_on_halves_is_one_instruction() {
        let halves = Lanes::floats(16, 4);
        let shorts = Lanes { count: 4, bits: 16, float: false };
        let singles = Lanes::floats(32, 4);
        let text = |op, r, a, signed| half_vector(op, r, a, signed).map(|it| it.text);
        assert_eq!(text("simd_div", halves, halves, true).unwrap(), "fdiv v0.4h, v1.4h, v2.4h");
        assert_eq!(text("simd_lt", shorts, halves, true).unwrap(), "fcmgt v0.4h, v2.4h, v1.4h");
        assert_eq!(
            text("simd_fma", Lanes::floats(16, 8), Lanes::floats(16, 8), true).unwrap(),
            "mov v0.16b, v3.16b\nfmla v0.8h, v2.8h, v1.8h"
        );
        assert_eq!(text("simd_cast", halves, singles, true).unwrap(), "fcvtn v0.4h, v1.4s");
        assert_eq!(text("simd_cast", halves, shorts, false).unwrap(), "ucvtf v0.4h, v1.4h");
        assert_eq!(text("simd_cast", shorts, halves, true).unwrap(), "fcvtzs v0.4h, v1.4h");
        assert_eq!(text("simd_cast", Lanes::floats(16, 8), Lanes::floats(32, 8), true), None);
        assert_eq!(text("simd_ne", shorts, halves, true), None);
    }

    /// Each lane of each of NEON's fused multiply-adds is what LLVM's instruction for it
    /// computes (`neon-fma-llvm.txt`: `rustc --emit asm -C opt-level=0` of each instance for
    /// aarch64-unknown-linux-gnu, nightly-2026-10-25, each lane of the result read off the
    /// instructions as the addend, the first factor, negated or not, and the second).
    #[test]
    fn fused_multiply_adds_keep_the_factors_llvm_keeps() {
        let mut failed = Vec::new();
        for line in include_str!("neon-fma-llvm.txt").lines() {
            let [name, lane, llvm] = line.split('\t').collect::<Vec<_>>()[..] else {
                panic!("{line}")
            };
            let lane = (!lane.is_empty()).then(|| lane.parse().unwrap());
            let Some(fused) = Fused::parse(name, lane) else {
                failed.push(format!("{name} {lane:?}: none"));
                continue;
            };
            let minus = if fused.subtracts { "-" } else { "" };
            let ours: Vec<String> = (0..fused.lanes.count)
                .map(|i| {
                    let c = match fused.c {
                        Factor::Each => i,
                        Factor::Scalar => 0,
                        Factor::Lane { lane, .. } => lane,
                    };
                    let (b, c) = (format!("b{i}"), format!("c{c}"));
                    let (first, second) = if fused.c_first() { (c, b) } else { (b, c) };
                    format!("(a{i},{minus}{first},{second})")
                })
                .collect();
            if ours.join(" ") != llvm {
                failed.push(format!("{name} {lane:?}: LLVM {llvm}, ours {}", ours.join(" ")));
            }
        }
        assert!(failed.is_empty(), "{}", failed.join("\n"));
    }

    #[test]
    fn a_fused_multiply_add_is_the_instruction_of_its_factors() {
        let text = |name, lane| fused(name, lane).map(|it| it.text.replace('\n', "; "));
        let expected = [
            ("vfmsq_f64", None, "mov v0.16b, v1.16b; fmls v0.2d, v3.2d, v2.2d"),
            ("vfmaq_laneq_f64", Some(1), "mov v0.16b, v1.16b; fmla v0.2d, v2.2d, v3.d[1]"),
            ("vfmsq_n_f32", None, "mov v0.16b, v1.16b; fmls v0.4s, v2.4s, v3.s[0]"),
            (
                "vfms_lane_f16",
                Some(3),
                "dup v4.4h, v3.h[3]; mov v0.16b, v1.16b; fmls v0.4h, v4.4h, v2.4h",
            ),
            ("vfmsd_laneq_f64", Some(0), "fmsub d0, d3, d2, d1"),
            ("vfmsd_laneq_f64", Some(1), "mov v0.16b, v1.16b; fmls d0, d2, v3.d[1]"),
            ("vfma_n_f64", None, "fmadd d0, d2, d3, d1"),
            ("vfmah_lane_f16", Some(3), "mov v0.16b, v1.16b; fmla h0, h2, v3.h[3]"),
        ];
        for (name, lane, want) in expected {
            assert_eq!(text(name, lane).as_deref(), Some(want), "{name} {lane:?}");
        }
        assert_eq!(fused("vfmsq_laneq_f64", Some(2)).map(|it| it.text), None);
        assert_eq!(fused("vfmaq_laneq_f64", None).map(|it| it.text), None);
        assert_eq!(fused("vfmlalq_low_f16", None).map(|it| it.text), None);
        assert_eq!(fused("vfmaq_f64", None).unwrap().args, [Some(16), Some(16), Some(16)]);
        assert_eq!(fused("vfmaq_lane_f32", Some(1)).unwrap().args, [Some(16), Some(16), Some(8)]);
        assert_eq!(fused("vfmah_f16", None).unwrap().features, ["neon", "fp16"]);
    }

    /// The ones whose body calls `floatNxM_t::splat`, as core has them: a vector's `c` of
    /// `_n`, `_lane` or `_laneq` of 32 or 64 bits, one lane of 64 bits too.
    #[test]
    fn the_bodies_that_splat_c_are_known() {
        let splats = [
            ("vfmaq_laneq_f64", Some(1)),
            ("vfmsq_lane_f32", Some(0)),
            ("vfms_n_f32", None),
            ("vfma_n_f64", None),
            ("vfms_laneq_f64", Some(1)),
        ];
        for (name, lane) in splats {
            assert!(splats_c(name, lane), "{name} {lane:?}");
        }
        let others = [
            ("vfmaq_f64", None),
            ("vfmsq_f32", None),
            ("vfmad_laneq_f64", Some(1)),
            ("vfmss_lane_f32", Some(1)),
            ("vfmaq_laneq_f16", Some(7)),
            ("vfms_n_f16", None),
            ("vfmsq_laneq_f64", Some(2)),
        ];
        for (name, lane) in others {
            assert!(!splats_c(name, lane), "{name} {lane:?}");
        }
    }

    /// As `rustc --emit asm` has each (aarch64-unknown-linux-gnu, nightly-2026-10-27): the
    /// instruction, its immediate the argument it is of all the intrinsic's, in the range
    /// LLVM selects.
    #[test]
    fn an_immediate_is_named_in_the_text_and_not_loaded() {
        let cases = [
            ("aarch64.neon.vcvtfxs2fp.v4f32.v4i32", "scvtf v0.4s, v1.4s, #{imm}", 1, 1..=32),
            ("aarch64.neon.vcvtfp2fxu.i64.f64", "fcvtzu d0, d1, #{imm}", 1, 1..=64),
            ("aarch64.neon.vcvtfxs2fp.f16.i32", "scvtf h0, h1, #{imm}", 1, 1..=16),
            ("aarch64.neon.vcvtfp2fxs.i64.f16", "fcvtzs h0, h1, #{imm}", 1, 1..=16),
            ("aarch64.neon.vcvtfp2fxs.v8i16.v8f16", "fcvtzs v0.8h, v1.8h, #{imm}", 1, 1..=16),
            ("aarch64.neon.vsli.v1i64", "mov v0.16b, v1.16b; sli d0, d2, #{imm}", 2, 0..=63),
            ("aarch64.neon.rshrn.v8i8", "rshrn v0.8b, v1.8h, #{imm}", 1, 1..=8),
            ("aarch64.neon.sqshlu.v4i32", "sqshlu v0.4s, v1.4s, #{imm}", 1, 0..=31),
            ("aarch64.crypto.xar", "xar v0.2d, v1.2d, v2.2d, #{imm}", 2, 0..=63),
            (
                "aarch64.crypto.sm3tt2b",
                "mov v0.16b, v1.16b; sm3tt2b v0.4s, v2.4s, v3.s[{imm}]",
                3,
                0..=3,
            ),
            (
                "aarch64.neon.ld3lane.v8i16.p0",
                "mov v0.16b, v1.16b; mov v1.16b, v2.16b; mov v2.16b, v3.16b; \
                 ld3 {v0.h, v1.h, v2.h}[{imm}], [x4]",
                3,
                0..=7,
            ),
            ("aarch64.neon.st2lane.v1f64.p0", "st2 {v1.d, v2.d}[{imm}], [x3]", 2, 0..=0),
        ];
        for (name, text, arg, range) in cases {
            let lowered = lower(name).unwrap_or_else(|| panic!("{name}"));
            assert_eq!(lowered.text.replace('\n', "; "), text, "{name}");
            let immediate = lowered.immediate.unwrap();
            assert_eq!((immediate.arg, immediate.range), (arg, range), "{name}");
        }
        let sqshlu = lower("aarch64.neon.sqshlu.v4i32").unwrap();
        assert_eq!(sqshlu.immediate.unwrap().splat, Some(4));
        assert_eq!(sqshlu.args, [Some(16)]);
        let half = lower("aarch64.neon.vcvtfxs2fp.f16.i32").unwrap();
        assert_eq!(
            (half.args, half.results, half.features),
            (vec![Some(4)], vec![2], vec!["neon", "fp16"])
        );
        let load = lower("aarch64.neon.ld3lane.v8i16.p0").unwrap();
        assert_eq!(
            (load.args, load.results),
            (vec![Some(16), Some(16), Some(16), None], vec![16, 16, 16])
        );
        assert_eq!(lower("aarch64.crypto.xar").unwrap().features, ["neon", "sha3"]);
        assert!(lower("aarch64.neon.vcvtfxs2fp.v2f64.v2i32").is_none());
        assert!(lower("aarch64.neon.vsli.v4f32").is_none());
    }
}
