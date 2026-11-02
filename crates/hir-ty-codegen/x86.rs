//! x86's intrinsics of LLVM that `core::arch` calls and that LLVM selects one instruction
//! (or a few, for a flag made a value) for: the SSE and AVX operations whose NaNs, rounding,
//! saturation and reciprocal estimates are the instruction's, the conversions that read
//! MXCSR, the tests that set flags, the crypto instructions, and the instructions on the
//! machine's state (fences, MXCSR, XSAVE, the time stamp counter, random numbers). That
//! instruction is the lowering that agrees with LLVM's on every input, so [`lower`] says it.
//! The code generator calls it as a function of its own (`asm::x86_function`), which loads
//! each argument into the register [`Lowered::args`] names, runs the text, and writes each
//! register [`Lowered::results`] names where the result goes.
//!
//! The ones SSE2 has and Cranelift says as vector instructions (`minps`, the packs,
//! `pmaddwd`, `pavgb`) are [`FunctionCx::x86_vector`]'s instead, in the function's own code.

#![forbid(unsafe_code)]

use std::fmt;

use cranelift_codegen::ir::{
    Endianness, InstBuilder, MemFlagsData, Type, Value, condcodes::FloatCC, types,
};
use cranelift_frontend::FunctionBuilder;
use hir_ty::mir::Operand;
use smallvec::SmallVec;

use super::{Arch, FunctionCx, MEM, Result};

/// An intrinsic as instructions on the registers `asm::x86_function` loads.
pub(super) struct Lowered {
    pub(super) text: String,
    /// The register each argument is read into, `None` for an immediate, which the text
    /// names as `{imm}`.
    pub(super) args: Vec<Option<&'static str>>,
    /// The registers of the results, one for each field of a result that is a struct.
    pub(super) results: Vec<&'static str>,
    /// The registers the text writes that are neither an argument's nor a result's.
    pub(super) clobbers: Vec<&'static str>,
}

fn lowered(
    text: impl Into<String>,
    args: &[&'static str],
    results: &[&'static str],
    clobbers: &[&'static str],
) -> Option<Lowered> {
    Some(Lowered {
        text: text.into(),
        // alloc: the lowering's registers, the output
        args: args.iter().map(|&it| Some(it)).collect(),
        results: results.to_vec(), // alloc: the lowering's registers, the output
        clobbers: clobbers.to_vec(), // alloc: the lowering's registers, the output
    })
}

/// Words run together: `pmulhrsw` of `pmul.hr.sw`.
#[derive(Clone, Copy)]
struct Undotted<'a>(&'a str);

impl fmt::Display for Undotted<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.split('.').try_for_each(|it| f.write_str(it))
    }
}

/// The words of `name` after the first, run together: `pmulhrsw` of `ssse3.pmul.hr.sw`.
fn tail(name: &str) -> Undotted<'_> {
    Undotted(name.split_once('.').map_or("", |(_, rest)| rest))
}

/// A mnemonic of up to three parts, written one after another.
#[derive(Clone, Copy)]
struct Concat<'a>([&'a str; 3]);

impl fmt::Display for Concat<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|it| f.write_str(it))
    }
}

/// The vector register `xmm`, `ymm` or `zmm` `k` of `bytes`.
fn vector_register(bytes: u32, k: u32) -> &'static str {
    const NAMES: [[&str; 3]; 3] =
        [["xmm0", "xmm1", "xmm2"], ["ymm0", "ymm1", "ymm2"], ["zmm0", "zmm1", "zmm2"]];
    NAMES[match bytes {
        16 => 0,
        32 => 1,
        _ => 2,
    }][k as usize]
}

/// The bytes of a vector of `llvm.x86.*.{width}`: 16 without one.
fn width(name: &str) -> u32 {
    match name.rsplit('.').next() {
        Some("256") => 32,
        Some("512") => 64,
        _ => 16,
    }
}

/// The flag an instruction set made a 32-bit 0 or 1, as LLVM reads it with `setcc`; `eq`
/// and `neq` of a comparison of floats also read the parity flag, which says the two were
/// unordered.
fn flag(condition: &str) -> Flag<'_> {
    Flag(condition)
}

#[derive(Clone, Copy)]
struct Flag<'a>(&'a str);

impl fmt::Display for Flag<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            "eq" => f.write_str("setnp al\nsete cl\nand al, cl\nmovzx eax, al"),
            "neq" => f.write_str("setp al\nsetne cl\nor al, cl\nmovzx eax, al"),
            condition => write!(f, "set{condition} al\nmovzx eax, al"),
        }
    }
}

/// The instructions of the intrinsic `llvm.{name}`, or `None` for one this does not know.
pub(super) fn lower(name: &str) -> Option<Lowered> {
    let op = name.strip_prefix("x86.")?;
    let (x0, x1) = ("xmm0", "xmm1");
    match op {
        // Lane by lane into the first operand, and the lanes above the first of the first
        // operand for the scalar ones: a NaN, the order of the two zeros, the rounding of
        // `cvtsd2ss`, the saturation of `pmulhrsw` and the counts of the shifts past the
        // lane's width are the instruction's. The functions of `core::arch` that call these
        // have SSE's features and not AVX's, for which LLVM selects the SSE encoding.
        "sse.max.ps"
        | "sse.max.ss"
        | "sse.min.ps"
        | "sse.min.ss"
        | "sse2.max.pd"
        | "sse2.max.sd"
        | "sse2.min.pd"
        | "sse2.min.sd"
        | "sse2.cvtsd2ss"
        | "sse2.psll.w"
        | "sse2.psll.d"
        | "sse2.psll.q"
        | "sse2.psra.w"
        | "sse2.psra.d"
        | "sse2.psrl.w"
        | "sse2.psrl.d"
        | "sse2.psrl.q"
        | "sse4a.extrq"
        | "sse4a.insertq"
        | "ssse3.psign.b.128"
        | "ssse3.psign.w.128"
        | "ssse3.psign.d.128"
        | "ssse3.pmul.hr.sw.128"
        | "aesni.aesenc"
        | "aesni.aesenclast"
        | "aesni.aesdec"
        | "aesni.aesdeclast"
        | "vgf2p8mulb.128" => {
            let mnemonic = match op {
                "vgf2p8mulb.128" => Undotted("gf2p8mulb"),
                _ => tail(op.trim_end_matches(".128")),
            };
            // alloc: the stub's text, the output
            lowered(format!("{mnemonic} xmm0, xmm1"), &[x0, x1], &[x0], &[])
        }
        // Of one operand, the scalar ones keeping the lanes above the first.
        "sse.rcp.ps" | "sse.rcp.ss" | "sse.rsqrt.ps" | "sse.rsqrt.ss" | "aesni.aesimc"
        | "sse2.cvtpd2dq" | "sse2.cvtps2dq" | "sse2.cvttpd2dq" | "sse2.cvttps2dq" => {
            // alloc: the stub's text, the output
            lowered(format!("{} xmm0, xmm0", tail(op)), &[x0], &[x0], &[])
        }
        // The first lane to an integer, rounded as MXCSR says (`t` toward zero); past the
        // integer's range or a NaN, its least.
        "sse.cvtss2si" | "sse.cvttss2si" | "sse2.cvtsd2si" | "sse2.cvttsd2si" => {
            let mnemonic = op.split('.').nth(1)?;
            // alloc: the stub's text, the output
            lowered(format!("{mnemonic} eax, xmm0"), &[x0], &["eax"], &[])
        }
        "sse.cvtss2si64" | "sse.cvttss2si64" | "sse2.cvtsd2si64" | "sse2.cvttsd2si64" => {
            let mnemonic = op.split('.').nth(1)?.trim_end_matches("64");
            // alloc: the stub's text, the output
            lowered(format!("{mnemonic} rax, xmm0"), &[x0], &["rax"], &[])
        }
        // The first lanes compared, as LLVM reads the flags `comis[sd]` and `ucomis[sd]`
        // set: `lt` and `le` are `gt` and `ge` of the two the other way round.
        _ if let Some((condition, mnemonic)) = comparison(op) => {
            let (l, r) = if matches!(condition, "lt" | "le") { (x1, x0) } else { (x0, x1) };
            let condition = match condition {
                "gt" | "lt" => "a",
                "ge" | "le" => "ae",
                other => other,
            };
            // alloc: the stub's text, the output
            let text = format!("{mnemonic} {l}, {r}\n{}", flag(condition));
            lowered(text, &[x0, x1], &["eax"], &["ecx"])
        }
        // Bits of the two tested together: `c` is the carry flag (no bit of the second where
        // the first has none), `z` the zero flag (no bit in both), `nzc` neither.
        "sse41.ptestnzc"
        | "avx.ptestnzc.256"
        | "avx.vtestc.pd.256"
        | "avx.vtestc.ps.256"
        | "avx.vtestz.pd.256"
        | "avx.vtestz.ps.256"
        | "avx.vtestnzc.pd"
        | "avx.vtestnzc.ps"
        | "avx.vtestnzc.pd.256"
        | "avx.vtestnzc.ps.256" => {
            let bytes = width(op);
            let mut words = op.split('.');
            let family = words.next()?;
            let test = words.next()?;
            let mnemonic = match (family, words.next()) {
                ("sse41", _) => "ptest",
                (_, Some("pd")) => "vtestpd",
                (_, Some("ps")) => "vtestps",
                _ => "vptest",
            };
            let condition = match test.trim_start_matches("vtest").trim_start_matches("ptest") {
                "c" => "b",
                "z" => "e",
                _ => "a",
            };
            let (a, b) = (vector_register(bytes, 0), vector_register(bytes, 1));
            // alloc: the stub's text, the output
            let text = format!("{mnemonic} {a}, {b}\n{}", flag(condition));
            lowered(text, &[a, b], &["eax"], &[])
        }
        // The first lane of the second operand rounded as the immediate says, the others
        // the first operand's.
        "sse41.round.ss" | "sse41.round.sd" => Some(Lowered {
            // alloc: the stub's text, the output
            text: format!("round{} xmm0, xmm1, {{imm}}", &op[op.len() - 2..]),
            args: vec![Some(x0), Some(x1), None], // alloc: the lowering's registers, the output
            results: vec![x0], // alloc: the lowering's registers, the output
            clobbers: Vec::new(), // alloc: empty, allocates nothing
        }),
        // AVX's and AVX2's, of three operands.
        "avx.max.pd.256"
        | "avx.max.ps.256"
        | "avx.min.pd.256"
        | "avx.min.ps.256"
        | "avx.vpermilvar.pd"
        | "avx.vpermilvar.ps"
        | "avx.vpermilvar.pd.256"
        | "avx.vpermilvar.ps.256"
        | "avx2.psign.b"
        | "avx2.psign.w"
        | "avx2.psign.d"
        | "avx2.pmul.hr.sw"
        | "aesni.aesenc.256"
        | "aesni.aesenclast.256"
        | "aesni.aesdec.256"
        | "aesni.aesdeclast.256"
        | "aesni.aesenc.512"
        | "aesni.aesenclast.512"
        | "aesni.aesdec.512"
        | "aesni.aesdeclast.512"
        | "vgf2p8mulb.256"
        | "vgf2p8mulb.512" => {
            // AVX2's integer forms are of 256 bits without a suffix.
            let bytes = if op.starts_with("avx2.") { 32 } else { width(op) };
            let words = op.split('.').collect::<SmallVec<[&str; 4]>>();
            let mnemonic = match words.as_slice() {
                &["avx", "vpermilvar", lanes, ..] => Concat(["vpermil", lanes, ""]),
                &["avx", op, lanes, ..] => Concat(["v", op, lanes]),
                &["avx2", "psign", lanes] => Concat(["vpsign", lanes, ""]),
                &["avx2", "pmul", "hr", "sw"] => Concat(["vpmulhrsw", "", ""]),
                &["aesni", op, _] => Concat(["v", op, ""]),
                _ => Concat(["vgf2p8mulb", "", ""]),
            };
            let (a, b) = (vector_register(bytes, 0), vector_register(bytes, 1));
            // alloc: the stub's text, the output
            lowered(format!("{mnemonic} {a}, {a}, {b}"), &[a, b], &[a], &[])
        }
        // Each lane shifted by the count in the low 64 bits of the second operand.
        "avx2.psll.w" | "avx2.psll.d" | "avx2.psll.q" | "avx2.psra.w" | "avx2.psra.d"
        | "avx2.psrl.w" | "avx2.psrl.d" | "avx2.psrl.q" => {
            let mnemonic = Undotted(&op["avx2.p".len()..]);
            // alloc: the stub's text, the output
            lowered(format!("vp{mnemonic} ymm0, ymm0, xmm1"), &["ymm0", x1], &["ymm0"], &[])
        }
        // To integers, rounded as MXCSR says (`cvtt` toward zero), of four doubles into a
        // vector of 128 bits or of eight singles.
        "avx.cvt.pd2dq.256" | "avx.cvtt.pd2dq.256" => {
            let mnemonic = Undotted(&op[4..op.len() - 4]);
            // alloc: the stub's text, the output
            lowered(format!("v{mnemonic} xmm0, ymm0"), &["ymm0"], &[x0], &[])
        }
        "avx.cvt.ps2dq.256" | "avx.cvtt.ps2dq.256" | "avx.rcp.ps.256" | "avx.rsqrt.ps.256" => {
            let mnemonic = Undotted(&op[4..op.len() - 4]);
            // alloc: the stub's text, the output
            lowered(format!("v{mnemonic} ymm0, ymm0"), &["ymm0"], &["ymm0"], &[])
        }
        // Zeroes every vector register, which holds nothing of the caller's across a call.
        "avx.vzeroall" => {
            const ALL: [&str; 16] = [
                "xmm0", "xmm1", "xmm2", "xmm3", "xmm4", "xmm5", "xmm6", "xmm7", "xmm8", "xmm9",
                "xmm10", "xmm11", "xmm12", "xmm13", "xmm14", "xmm15",
            ];
            lowered("vzeroall", &[], &[], &ALL)
        }
        // SHA-512, SM3 and SM4 (AVX's encodings), the first operand also the result.
        "vsha512msg1" => lowered("vsha512msg1 ymm0, xmm1", &["ymm0", x1], &["ymm0"], &[]),
        "vsha512msg2" => lowered("vsha512msg2 ymm0, ymm1", &["ymm0", "ymm1"], &["ymm0"], &[]),
        "vsha512rnds2" => {
            lowered("vsha512rnds2 ymm0, ymm1, xmm2", &["ymm0", "ymm1", "xmm2"], &["ymm0"], &[])
        }
        "vsm3msg1" | "vsm3msg2" => {
            // alloc: the stub's text, the output
            lowered(format!("{op} xmm0, xmm1, xmm2"), &[x0, x1, "xmm2"], &[x0], &[])
        }
        "vsm4key4128" | "vsm4rnds4128" | "vsm4key4256" | "vsm4rnds4256" => {
            let bytes = if op.ends_with("256") { 32 } else { 16 };
            let (a, b) = (vector_register(bytes, 0), vector_register(bytes, 1));
            // alloc: the stub's text, the output
            lowered(format!("{} {a}, {a}, {b}", &op[..op.len() - 3]), &[a, b], &[a], &[])
        }
        // Barriers, and the instructions on the machine's state of no operand.
        "sse.sfence" | "sse2.lfence" | "sse2.mfence" => {
            lowered(op.split('.').nth(1)?, &[], &[], &[])
        }
        "tilerelease" | "xend" => lowered(op, &[], &[], &[]),
        // Whether a transaction runs: the zero flag clear.
        // alloc: the stub's text, the output
        "xtest" => lowered(format!("xtest\n{}", flag("ne")), &[], &["eax"], &[]),
        // Starts a transaction: all ones, or where it aborted to (the next instruction) the
        // status of the abort, which the processor leaves in `eax`.
        "xbegin" => lowered("mov eax, -1\nxbegin 2f\n2:", &[], &["eax"], &[]),
        // The time stamp counter, of `edx:eax`, and with `rdtscp` the processor's number.
        "rdtsc" => lowered("rdtsc\nshl rdx, 32\nor rax, rdx", &[], &["rax"], &["rdx"]),
        "rdtscp" => lowered("rdtscp\nshl rdx, 32\nor rax, rdx", &[], &["rax", "ecx"], &["rdx"]),
        // A random number and whether there was one: 1 with the carry flag, otherwise the
        // number, which the processor zeroes then, as LLVM selects it (`cmovb`).
        "rdrand.16" | "rdrand.32" | "rdrand.64" | "rdseed.16" | "rdseed.32" | "rdseed.64" => {
            let (mnemonic, bits) = op.split_once('.')?;
            let (value, low) = match bits {
                "16" => ("ax", "movzx ecx, ax"),
                "32" => ("eax", "mov ecx, eax"),
                _ => ("rax", "mov ecx, eax"),
            };
            // alloc: the stub's text, the output
            let text = format!("{mnemonic} {value}\n{low}\nmov edx, 1\ncmovb ecx, edx");
            lowered(text, &[], &[value, "ecx"], &["edx"])
        }
        // Of the memory at a pointer.
        "sse2.clflush" | "clflushopt" => {
            // alloc: the stub's text, the output
            lowered(format!("{} byte ptr [r8]", op.trim_start_matches("sse2.")), &["r8"], &[], &[])
        }
        "sse.ldmxcsr" | "sse.stmxcsr" => {
            // alloc: the stub's text, the output
            lowered(format!("{} dword ptr [r8]", &op[4..]), &["r8"], &[], &[])
        }
        "fxsave" | "fxsave64" | "fxrstor" | "fxrstor64" | "ldtilecfg" | "sttilecfg" => {
            // alloc: the stub's text, the output
            lowered(format!("{op} [r8]"), &["r8"], &[], &[])
        }
        "prefetchrs" => lowered("prefetchrst2 byte ptr [r8]", &["r8"], &[], &[]),
        "movrsqi" | "movrshi" | "movrssi" | "movrsdi" => {
            let (register, size) = match op {
                "movrsqi" => ("al", "byte"),
                "movrshi" => ("ax", "word"),
                "movrssi" => ("eax", "dword"),
                _ => ("rax", "qword"),
            };
            // alloc: the stub's text, the output
            lowered(format!("movrs {register}, {size} ptr [r8]"), &["r8"], &[register], &[])
        }
        // The state components of the mask in `edx:eax`, saved to or restored from memory.
        "xsave" | "xsave64" | "xsaveopt" | "xsaveopt64" | "xsavec" | "xsavec64" | "xsaves"
        | "xsaves64" | "xrstor" | "xrstor64" | "xrstors" | "xrstors64" => {
            // alloc: the stub's text, the output
            lowered(format!("{op} [r8]"), &["r8", "edx", "eax"], &[], &[])
        }
        "xsetbv" => lowered("xsetbv", &["ecx", "edx", "eax"], &[], &[]),
        // The bytes of the first operand where the second has its top bit set, written to
        // the memory `rdi` points at.
        "sse2.maskmov.dqu" => lowered("maskmovdqu xmm0, xmm1", &[x0, x1, "rdi"], &[], &[]),
        // The sum (difference) of the two and the carry (borrow) in, and the carry out: LLVM
        // makes the carry flag of a carry in that is not 0 by adding 255 to it.
        "addcarry.32" | "addcarry.64" | "subborrow.32" | "subborrow.64" => {
            let (a, b) = if op.ends_with("64") { ("rdx", "r8") } else { ("edx", "r8d") };
            let mnemonic = if op.starts_with("add") { "adc" } else { "sbb" };
            // alloc: the stub's text, the output
            let text = format!("add cl, -1\n{mnemonic} {a}, {b}\nsetb al");
            lowered(text, &["cl", a, b], &["al", a], &[])
        }
        // BMI's: the bits of the first operand the second says.
        "bmi.bextr.32" | "bmi.bzhi.32" | "bmi.pdep.32" | "bmi.pext.32" => {
            let mnemonic = &op[4..op.len() - 3];
            // alloc: the stub's text, the output
            lowered(format!("{mnemonic} eax, ecx, edx"), &["ecx", "edx"], &["eax"], &[])
        }
        "bmi.bextr.64" | "bmi.bzhi.64" | "bmi.pdep.64" | "bmi.pext.64" => {
            let mnemonic = &op[4..op.len() - 3];
            // alloc: the stub's text, the output
            lowered(format!("{mnemonic} rax, rcx, rdx"), &["rcx", "rdx"], &["rax"], &[])
        }
        // Key Locker's AES of one block with the key of a handle, and whether the handle was
        // refused (the zero flag), in which case the block is zeroed.
        "aesenc128kl" | "aesdec128kl" | "aesenc256kl" | "aesdec256kl" => {
            // alloc: the stub's text, the output
            let text = format!("{op} xmm0, [r8]\n{}", flag_byte("e"));
            lowered(text, &[x0, "r8"], &["al", x0], &[])
        }
        // Of eight blocks, `xmm0` to `xmm7`, after the address and the flag.
        "aesencwide128kl" | "aesdecwide128kl" | "aesencwide256kl" | "aesdecwide256kl" => {
            const ARGS: [&str; 9] =
                ["r8", "xmm0", "xmm1", "xmm2", "xmm3", "xmm4", "xmm5", "xmm6", "xmm7"];
            const RESULTS: [&str; 9] =
                ["al", "xmm0", "xmm1", "xmm2", "xmm3", "xmm4", "xmm5", "xmm6", "xmm7"];
            // alloc: the stub's text, the output
            let text = format!("{op} [r8]\n{}", flag_byte("e"));
            lowered(text, &ARGS, &RESULTS, &[])
        }
        // The handle of a key wrapped with the wrapping key, the control word's value, and
        // the registers the instruction zeroes, which LLVM takes as results in the order the
        // instruction defines them: `xmm3` is not one of them for a key of 128 bits.
        "encodekey128" => lowered(
            "encodekey128 eax, ecx",
            &["ecx", x0],
            &["eax", "xmm0", "xmm1", "xmm2", "xmm4", "xmm5", "xmm6"],
            &[],
        ),
        "encodekey256" => lowered(
            "encodekey256 eax, ecx",
            &["ecx", x0, x1],
            &["eax", "xmm0", "xmm1", "xmm2", "xmm3", "xmm4", "xmm5", "xmm6"],
            &[],
        ),
        // The wrapping key, of the integrity key in `xmm0`, the two halves of the key and the
        // control word in `eax`.
        "loadiwkey" => lowered("loadiwkey xmm1, xmm2", &[x0, x1, "xmm2", "eax"], &[], &[]),
        _ => None,
    }
}

/// The flag an instruction set as a byte that is 0 or 1.
fn flag_byte(condition: &str) -> Concat<'_> {
    Concat(["set", condition, " al"])
}

/// `{sse.,sse2.}{comi,ucomi}{eq,neq,lt,le,gt,ge}.{ss,sd}` as its condition and the
/// instruction that compares.
fn comparison(op: &str) -> Option<(&str, Concat<'_>)> {
    let (family, rest) = op.split_once('.')?;
    if !matches!(family, "sse" | "sse2") {
        return None;
    }
    let (compare, lanes) = rest.split_once('.')?;
    if !matches!((family, lanes), ("sse", "ss") | ("sse2", "sd")) {
        return None;
    }
    let (mnemonic, condition) = match compare.strip_prefix("ucomi") {
        Some(condition) => ("ucomis", condition),
        None => ("comis", compare.strip_prefix("comi")?),
    };
    if !matches!(condition, "eq" | "neq" | "lt" | "le" | "gt" | "ge") {
        return None;
    }
    Some((condition, Concat([mnemonic, &lanes[1..], ""])))
}

/// What [`FunctionCx::x86_vector`] lowers, on each 16 bytes of its two operands.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
enum Vector {
    /// `minps`: the first where it is less than the second, else the second (a NaN or two
    /// zeros give the second).
    Min,
    /// `maxps`: the first where it is greater than the second, else the second.
    Max,
    /// `packsswb`: each lane of the first then the second, halved with signed saturation.
    PackSigned,
    /// `packuswb`: the same with unsigned saturation of the signed lanes.
    PackUnsigned,
    /// `pmaddwd`: the wrapping sum of each two neighboring products of 16-bit lanes.
    MultiplyAddPairs,
    /// `pavgb`: `(a + b + 1) >> 1` without overflow.
    RoundedMean,
}

const _: () = assert!(size_of::<Vector>() == 1);

/// The [`Vector`] of `llvm.{name}`, the lanes of its operands and how many 16-byte halves
/// they have, for the ones SSE2 has (and AVX2's of two halves, each the same).
fn vector_op(name: &str) -> Option<(Vector, Type, i32)> {
    use Vector::*;
    use types::{F32X4, F64X2, I8X16, I16X8, I32X4};
    Some(match name.strip_prefix("x86.")? {
        "sse.min.ps" => (Min, F32X4, 1),
        "sse.max.ps" => (Max, F32X4, 1),
        "sse2.min.pd" => (Min, F64X2, 1),
        "sse2.max.pd" => (Max, F64X2, 1),
        "avx.min.ps.256" => (Min, F32X4, 2),
        "avx.max.ps.256" => (Max, F32X4, 2),
        "avx.min.pd.256" => (Min, F64X2, 2),
        "avx.max.pd.256" => (Max, F64X2, 2),
        "sse2.packsswb.128" => (PackSigned, I16X8, 1),
        "sse2.packuswb.128" => (PackUnsigned, I16X8, 1),
        "sse2.packssdw.128" => (PackSigned, I32X4, 1),
        "sse41.packusdw" => (PackUnsigned, I32X4, 1),
        "avx2.packsswb" => (PackSigned, I16X8, 2),
        "avx2.packuswb" => (PackUnsigned, I16X8, 2),
        "avx2.packssdw" => (PackSigned, I32X4, 2),
        "avx2.packusdw" => (PackUnsigned, I32X4, 2),
        "sse2.pmadd.wd" => (MultiplyAddPairs, I16X8, 1),
        "avx2.pmadd.wd" => (MultiplyAddPairs, I16X8, 2),
        "sse2.pavg.b" => (RoundedMean, I8X16, 1),
        "sse2.pavg.w" => (RoundedMean, I16X8, 1),
        "avx2.pavg.b" => (RoundedMean, I8X16, 2),
        "avx2.pavg.w" => (RoundedMean, I16X8, 2),
        _ => return None,
    })
}

impl<'db> FunctionCx<'_, 'db> {
    /// Lowers `llvm.{name}` to Cranelift's vector instructions if it is one of [`Vector`]'s on
    /// x86-64, each the instruction LLVM selects (Cranelift's x64 rules give `minps`, `maxps`,
    /// `packsswb`, `packuswb`, `packssdw`, `pmaddwd`, `pavgb` and `pavgw` with SSE2 alone),
    /// storing the result at `dst`. False for any other, which the caller lowers.
    pub(super) fn x86_vector(
        &mut self,
        name: &str,
        args: &[Operand],
        dst: Value,
    ) -> Result<bool> {
        if self.cg.target.arch != Arch::X86_64 {
            return Ok(false);
        }
        let Some((op, lanes, halves)) = vector_op(name) else { return Ok(false) };
        let [a, b] = args else { return Ok(false) };
        let (a, _) = self.operand(a)?;
        let (b, _) = self.operand(b)?;
        for half in 0..halves {
            let at = half * 16;
            let x = self.builder.ins().load(lanes, MEM, a, at);
            let y = self.builder.ins().load(lanes, MEM, b, at);
            let value = vector(&mut self.builder, op, lanes, x, y);
            self.builder.ins().store(MEM, value, dst, at);
        }
        Ok(true)
    }
}

const LITTLE: MemFlagsData = MemFlagsData::new().with_endianness(Endianness::Little);

/// `op` of 16 bytes `x` and `y` of `lanes`, in the form Cranelift's x64 rules select the one
/// instruction for (`minps` is `bitselect(bitcast(fcmp lt x y), x, y)` with the operands in
/// that order).
fn vector(builder: &mut FunctionBuilder<'_>, op: Vector, lanes: Type, x: Value, y: Value) -> Value {
    match op {
        Vector::Min | Vector::Max => {
            let (less, more) = if op == Vector::Min { (x, y) } else { (y, x) };
            let mask = builder.ins().fcmp(FloatCC::LessThan, less, more);
            let mask = builder.ins().bitcast(lanes, LITTLE, mask);
            builder.ins().bitselect(mask, x, y)
        }
        Vector::PackSigned => builder.ins().snarrow(x, y),
        Vector::PackUnsigned => builder.ins().unarrow(x, y),
        Vector::MultiplyAddPairs => {
            let (x_low, y_low) = (builder.ins().swiden_low(x), builder.ins().swiden_low(y));
            let (x_high, y_high) = (builder.ins().swiden_high(x), builder.ins().swiden_high(y));
            let low = builder.ins().imul(x_low, y_low);
            let high = builder.ins().imul(x_high, y_high);
            builder.ins().iadd_pairwise(low, high)
        }
        Vector::RoundedMean => builder.ins().avg_round(x, y),
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
                _ = writeln!(
                    out,
                    "{name}: {} {:?} -> {:?}",
                    it.text.replace('\n', "; "),
                    it.args,
                    it.results
                )
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
    fn each_name_is_the_instruction_llvm_selects_for_it() {
        let described = describe(&[
            "x86.sse.max.ss",
            "x86.sse2.psra.w",
            "x86.ssse3.pmul.hr.sw.128",
            "x86.ssse3.psign.d.128",
            "x86.aesni.aesdeclast",
            "x86.vgf2p8mulb.128",
            "x86.sse.rsqrt.ss",
            "x86.sse2.cvttpd2dq",
            "x86.sse.cvttss2si64",
            "x86.sse2.cvtsd2si",
            "x86.sse.comilt.ss",
            "x86.sse2.ucomineq.sd",
            "x86.sse41.ptestnzc",
            "x86.avx.vtestc.ps.256",
            "x86.avx.vtestnzc.pd",
            "x86.sse41.round.sd",
            "x86.avx.vpermilvar.ps.256",
            "x86.avx2.pmul.hr.sw",
            "x86.aesni.aesenc.512",
            "x86.avx2.psra.d",
            "x86.avx.cvtt.pd2dq.256",
            "x86.avx.rsqrt.ps.256",
            "x86.vsm4rnds4256",
            "x86.rdseed.16",
            "x86.sse.stmxcsr",
            "x86.movrshi",
            "x86.subborrow.64",
            "x86.bmi.pdep.32",
            "x86.sse.comilt.sd",
            "x86.avx512.max.ps.512",
        ]);
        let expected = "\
x86.sse.max.ss: maxss xmm0, xmm1 [Some(\"xmm0\"), Some(\"xmm1\")] -> [\"xmm0\"]
x86.sse2.psra.w: psraw xmm0, xmm1 [Some(\"xmm0\"), Some(\"xmm1\")] -> [\"xmm0\"]
x86.ssse3.pmul.hr.sw.128: pmulhrsw xmm0, xmm1 [Some(\"xmm0\"), Some(\"xmm1\")] -> [\"xmm0\"]
x86.ssse3.psign.d.128: psignd xmm0, xmm1 [Some(\"xmm0\"), Some(\"xmm1\")] -> [\"xmm0\"]
x86.aesni.aesdeclast: aesdeclast xmm0, xmm1 [Some(\"xmm0\"), Some(\"xmm1\")] -> [\"xmm0\"]
x86.vgf2p8mulb.128: gf2p8mulb xmm0, xmm1 [Some(\"xmm0\"), Some(\"xmm1\")] -> [\"xmm0\"]
x86.sse.rsqrt.ss: rsqrtss xmm0, xmm0 [Some(\"xmm0\")] -> [\"xmm0\"]
x86.sse2.cvttpd2dq: cvttpd2dq xmm0, xmm0 [Some(\"xmm0\")] -> [\"xmm0\"]
x86.sse.cvttss2si64: cvttss2si rax, xmm0 [Some(\"xmm0\")] -> [\"rax\"]
x86.sse2.cvtsd2si: cvtsd2si eax, xmm0 [Some(\"xmm0\")] -> [\"eax\"]
x86.sse.comilt.ss: comiss xmm1, xmm0; seta al; movzx eax, al [Some(\"xmm0\"), Some(\"xmm1\")] -> [\"eax\"]
x86.sse2.ucomineq.sd: ucomisd xmm0, xmm1; setp al; setne cl; or al, cl; movzx eax, al [Some(\"xmm0\"), Some(\"xmm1\")] -> [\"eax\"]
x86.sse41.ptestnzc: ptest xmm0, xmm1; seta al; movzx eax, al [Some(\"xmm0\"), Some(\"xmm1\")] -> [\"eax\"]
x86.avx.vtestc.ps.256: vtestps ymm0, ymm1; setb al; movzx eax, al [Some(\"ymm0\"), Some(\"ymm1\")] -> [\"eax\"]
x86.avx.vtestnzc.pd: vtestpd xmm0, xmm1; seta al; movzx eax, al [Some(\"xmm0\"), Some(\"xmm1\")] -> [\"eax\"]
x86.sse41.round.sd: roundsd xmm0, xmm1, {imm} [Some(\"xmm0\"), Some(\"xmm1\"), None] -> [\"xmm0\"]
x86.avx.vpermilvar.ps.256: vpermilps ymm0, ymm0, ymm1 [Some(\"ymm0\"), Some(\"ymm1\")] -> [\"ymm0\"]
x86.avx2.pmul.hr.sw: vpmulhrsw ymm0, ymm0, ymm1 [Some(\"ymm0\"), Some(\"ymm1\")] -> [\"ymm0\"]
x86.aesni.aesenc.512: vaesenc zmm0, zmm0, zmm1 [Some(\"zmm0\"), Some(\"zmm1\")] -> [\"zmm0\"]
x86.avx2.psra.d: vpsrad ymm0, ymm0, xmm1 [Some(\"ymm0\"), Some(\"xmm1\")] -> [\"ymm0\"]
x86.avx.cvtt.pd2dq.256: vcvttpd2dq xmm0, ymm0 [Some(\"ymm0\")] -> [\"xmm0\"]
x86.avx.rsqrt.ps.256: vrsqrtps ymm0, ymm0 [Some(\"ymm0\")] -> [\"ymm0\"]
x86.vsm4rnds4256: vsm4rnds4 ymm0, ymm0, ymm1 [Some(\"ymm0\"), Some(\"ymm1\")] -> [\"ymm0\"]
x86.rdseed.16: rdseed ax; movzx ecx, ax; mov edx, 1; cmovb ecx, edx [] -> [\"ax\", \"ecx\"]
x86.sse.stmxcsr: stmxcsr dword ptr [r8] [Some(\"r8\")] -> []
x86.movrshi: movrs ax, word ptr [r8] [Some(\"r8\")] -> [\"ax\"]
x86.subborrow.64: add cl, -1; sbb rdx, r8; setb al [Some(\"cl\"), Some(\"rdx\"), Some(\"r8\")] -> [\"al\", \"rdx\"]
x86.bmi.pdep.32: pdep eax, ecx, edx [Some(\"ecx\"), Some(\"edx\")] -> [\"eax\"]
x86.sse.comilt.sd: none
x86.avx512.max.ps.512: none
";
        assert_eq!(described, expected);
    }

    /// Each name of [`vector_op`] with the one SSE2 instruction LLVM selects for it.
    const VECTORS: [(&str, &str); 22] = [
        ("x86.sse.min.ps", "minps"),
        ("x86.sse.max.ps", "maxps"),
        ("x86.sse2.min.pd", "minpd"),
        ("x86.sse2.max.pd", "maxpd"),
        ("x86.avx.min.ps.256", "minps"),
        ("x86.avx.max.ps.256", "maxps"),
        ("x86.avx.min.pd.256", "minpd"),
        ("x86.avx.max.pd.256", "maxpd"),
        ("x86.sse2.packsswb.128", "packsswb"),
        ("x86.sse2.packuswb.128", "packuswb"),
        ("x86.sse2.packssdw.128", "packssdw"),
        ("x86.sse41.packusdw", ""),
        ("x86.avx2.packsswb", "packsswb"),
        ("x86.avx2.packuswb", "packuswb"),
        ("x86.avx2.packssdw", "packssdw"),
        ("x86.avx2.packusdw", ""),
        ("x86.sse2.pmadd.wd", "pmaddwd"),
        ("x86.avx2.pmadd.wd", "pmaddwd"),
        ("x86.sse2.pavg.b", "pavgb"),
        ("x86.sse2.pavg.w", "pavgw"),
        ("x86.avx2.pavg.b", "pavgb"),
        ("x86.avx2.pavg.w", "pavgw"),
    ];

    #[test]
    fn vector_ops_by_name() {
        for (name, _) in VECTORS {
            let (_, lanes, halves) = vector_op(name).expect(name);
            assert_eq!(lanes.bytes(), 16, "{name}");
            assert_eq!(halves, if name.starts_with("x86.avx") { 2 } else { 1 }, "{name}");
        }
        for name in ["x86.sse.min.ss", "x86.sse2.pavg.d", "x86.avx512.pavg.b.512", "sse.min.ps"] {
            assert_eq!(vector_op(name), None, "{name}");
        }
    }

    /// What [`vector`] builds is the instruction LLVM selects once Cranelift lowers it for
    /// x86-64 with SSE2 alone, as blaze's ISA is: one `minps`, not a compare and a blend.
    /// `packusdw` is SSE4.1's, so without it Cranelift's own sequence of SSE2 stands in.
    #[test]
    fn each_vector_is_the_instruction_of_sse2() {
        use cranelift_codegen::{
            Context,
            control::ControlPlane,
            ir::{AbiParam, Function, Signature, UserFuncName},
            isa::{self, CallConv},
            settings::{self, Configurable},
        };
        use cranelift_frontend::FunctionBuilderContext;

        let triple = target_lexicon::triple!("x86_64-unknown-linux-gnu");
        let mut features = isa::lookup(triple).unwrap();
        for feature in ["has_ssse3", "has_sse41", "has_sse42", "has_avx", "has_avx2"] {
            features.set(feature, "false").unwrap();
        }
        let isa = features.finish(settings::Flags::new(settings::builder())).unwrap();
        for (name, instruction) in VECTORS {
            let (op, lanes, _) = vector_op(name).unwrap();
            let mut signature = Signature::new(CallConv::SystemV);
            signature.params.extend([AbiParam::new(types::I64); 3]);
            let mut func = Function::with_name_signature(UserFuncName::default(), signature);
            let mut context = FunctionBuilderContext::new();
            let mut builder = FunctionBuilder::new(&mut func, &mut context);
            let block = builder.create_block();
            builder.append_block_params_for_function_params(block);
            builder.switch_to_block(block);
            let [a, b, dst] = *builder.block_params(block) else { unreachable!() };
            let x = builder.ins().load(lanes, MEM, a, 0);
            let y = builder.ins().load(lanes, MEM, b, 0);
            let value = vector(&mut builder, op, lanes, x, y);
            builder.ins().store(MEM, value, dst, 0);
            builder.ins().return_(&[]);
            builder.seal_all_blocks();
            builder.finalize(isa.frontend_config());

            let mut context = Context::for_function(func);
            context.set_disasm(true);
            let code = context.compile(&*isa, &mut ControlPlane::default()).expect(name);
            let code = code.vcode.as_deref().expect("its instructions");
            let ops = code.lines().filter_map(|line| line.split_whitespace().next());
            let ops = ops.collect::<Vec<_>>();
            assert!(!ops.iter().any(|op| op.starts_with("call")), "{name}: {code}");
            if !instruction.is_empty() {
                assert!(ops.contains(&instruction), "{name}: {code}");
                let blend = |op: &&str| op.starts_with("cmp") || op.starts_with("pand");
                assert!(!ops.iter().any(blend), "{name}: {code}");
            }
        }
    }
}
