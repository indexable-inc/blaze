//! AArch64's SIMD intrinsics of LLVM that `core::arch` calls: `llvm.aarch64.neon.*`, the
//! crypto ones, the rounding and conversion ones, and the generic ones it calls on vectors
//! (`llvm.roundeven.v4f32`). LLVM defines each of these as the instruction it selects for
//! it, so that instruction is the lowering that agrees with it in every lane: in how it
//! rounds, saturates and treats a NaN, and in the reciprocal estimates, whose tables only
//! the hardware has. [`lower`] says which instructions an intrinsic is; the code generator
//! calls them as a function of their own (`asm::neon_function`), on registers loaded from
//! where its operands are: `v1` up from the first argument, `x1` up for a pointer, the result
//! left in `v0` (and `v1` up for a tuple of vectors).

/// An intrinsic as instructions on the registers [`asm::neon_function`](super::asm) loads.
pub(super) struct Lowered {
    /// The target features, as Rust names them, the instructions need of the assembler.
    pub(super) features: Vec<&'static str>,
    pub(super) text: String,
    /// The bytes each argument has in its register, `None` for a pointer: argument `i` is
    /// read into `v{i+1}` (`x{i+1}` for a pointer).
    pub(super) args: Vec<Option<u32>>,
    /// The bytes of each result, from `v0` up; none for an intrinsic that only stores.
    pub(super) results: Vec<u32>,
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
        // To single precision, rounded to odd.
        ("fcvtxn", &[r, a]) if r.float && a.float && r.count == a.count && a.bits == 64 => {
            instruction(&[], format!("fcvtxn {}, {}", r.reg(0), a.reg(1)), [a], [r])
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
            Some(Lowered { features: vec!["aes"], text, args: vec![Some(8); 2], results: vec![16] })
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
            })
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
            Some(Lowered { features: Vec::new(), text, args, results: Vec::new() })
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
";
        assert_eq!(described, expected);
    }
}
