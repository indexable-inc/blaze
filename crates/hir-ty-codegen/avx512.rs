//! x86's AVX-512 intrinsics of LLVM that `core::arch` calls (`llvm.x86.avx512.*`, those of
//! AVX512-FP16 and AVX512-BF16, `llvm.x86.avx512fp16.*` and `llvm.x86.avx512bf16.*`, the VNNI
//! ones of AVX, `llvm.x86.avx2.vpdp*`, and AVX-NE-CONVERT's, `llvm.x86.vcvtne*` and
//! `llvm.x86.vbcstne*`): the masked conversions, compares, estimates, truncations,
//! compressions, complex products and dot products. LLVM defines each as the instruction it
//! selects for it, so that instruction is the lowering that agrees with it in every lane: in
//! how it rounds, saturates and treats a NaN, a denormal or a lane the mask leaves alone, and
//! in the reciprocal estimates, whose tables only the hardware has. [`lower`] says which
//! instruction an intrinsic is; the code generator calls it as a function of its own
//! (`asm::avx512_function`) on registers loaded from where its operands are: argument `i` into
//! `zmm{i+1}` (`ymm`, `xmm` by its size), `k{i+1}` for a mask, `r11` for an address, `rcx`
//! (`ecx`) for an integer. The result is left in `zmm0`, `k0` (`k0` and `k1` for a pair of
//! masks) or `rax`.
//!
//! A mask of all ones, which LLVM drops from the instruction, and a source of zeros, which it
//! makes the `{z}` form, are kept here as a merge into that source under that mask: the same
//! lanes. The rounding a 512-bit or scalar form takes as its last operand is part of the
//! instruction: `_MM_FROUND_CUR_DIRECTION` (4) is the instruction without one, the four
//! directions with `_MM_FROUND_NO_EXC` (8 to 11) are `{rn-sae}` to `{rz-sae}`, and where the
//! operand only suppresses exceptions, 8 (or 12) is `{sae}`. LLVM rejects any other value.

#![forbid(unsafe_code)]

use std::fmt;

use hir_ty::{TargetFeature, TargetFeatureSet, intrinsic::SimdOp};
use smallvec::SmallVec;

/// An operand of the intrinsic as the code generator has it: its size, whether it is an
/// address, and its value where that is a constant integer.
#[derive(Clone, Copy, Debug)]
pub(super) struct Given {
    pub(super) bytes: u32,
    pub(super) pointer: bool,
    pub(super) constant: Option<u128>,
}

/// What the function reads of an argument.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Load {
    /// A vector of this many bytes, into the vector register numbered one past the argument.
    Vector(u32),
    /// A mask of this many bytes, into the mask register numbered one past the argument.
    Mask(u32),
    /// An address, into `r11`.
    Pointer,
    /// An integer of this many bytes (4 or 8), into `ecx` or `rcx`.
    Int(u32),
    /// Nothing: a constant that is part of the instruction.
    Immediate,
}

/// Where the result is left, and its size.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Out {
    /// Nothing: the intrinsic stores to memory.
    Nothing,
    Vector(u32),
    Mask(u32),
    /// In `rax` (`eax`).
    Int(u32),
    /// `k0` and `k1`, of this many bytes each, the second this many bytes after the first.
    Masks(u32, u32),
}

#[derive(Debug)]
pub(super) struct Lowered {
    pub(super) text: String,
    pub(super) loads: Vec<Load>,
    pub(super) out: Out,
}

/// What the last operand of an intrinsic that has one says of the instruction.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Rounding {
    /// A direction of rounding, or the current one.
    Round,
    /// Only whether exceptions are suppressed.
    Sae,
}

/// The vector register `number` of `bytes`.
fn reg(bytes: u32, number: usize) -> Reg {
    Reg { bytes, number }
}

/// The vector register argument `at` is read into.
fn arg(bytes: u32, at: usize) -> Reg {
    reg(bytes, at + 1)
}

/// A vector register, written `zmm{number}`, `ymm{number}` or `xmm{number}` by its size.
#[derive(Clone, Copy)]
struct Reg {
    bytes: u32,
    number: usize,
}

impl fmt::Display for Reg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let class = match self.bytes {
            64 => "zmm",
            32 => "ymm",
            _ => "xmm",
        };
        write!(f, "{class}{}", self.number)
    }
}

/// A mnemonic of up to three parts, written one after another: `v`, `getexp` and `ps`.
#[derive(Clone, Copy)]
struct Concat<'a>([&'a str; 3]);

impl fmt::Display for Concat<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|it| f.write_str(it))
    }
}

/// The size of a memory operand of `bytes`, as Intel's syntax names it.
fn memory(bytes: u32) -> Option<&'static str> {
    Some(match bytes {
        2 => "word",
        4 => "dword",
        8 => "qword",
        16 => "xmmword",
        32 => "ymmword",
        64 => "zmmword",
        _ => return None,
    })
}

/// The bytes of a lane its letter names: `d` of `pmov.dw`, `q` of `compress.q`.
fn lane_bytes(letter: char) -> Option<u32> {
    Some(match letter {
        'b' => 1,
        'w' => 2,
        'd' => 4,
        'q' => 8,
        _ => return None,
    })
}

/// The instructions `name` (`x86.avx512.mask.cmp.ps.512`) is on operands of the shapes in
/// `args`, giving `result` bytes, in a function with the target features `features`. `None` for
/// an intrinsic this does not know or of shapes other than its name says; an error for one it
/// knows with an operand LLVM refuses, as a rounding it has no instruction for.
pub(super) fn lower(
    name: &str,
    args: &[Given],
    result: u32,
    features: TargetFeatureSet,
) -> Option<Result<Lowered, String>> {
    let mut words = name.strip_prefix("x86.")?.split('.').collect::<SmallVec<[&str; 8]>>();
    let width = match words.last() {
        Some(&(last @ ("128" | "256" | "512"))) => {
            words.pop();
            Some(last.parse::<u32>().ok()? / 8)
        }
        _ => None,
    };
    let lowered = match lowered(&words, width, args, result, features)? {
        Ok(it) => it,
        Err(e) => return Some(Err(format!("`llvm.{name}` {e}"))), // alloc: the error's message
    };
    // The width the name says is that of its widest vector.
    let widest = lowered
        .loads
        .iter()
        .filter_map(|it| match it {
            Load::Vector(bytes) => Some(*bytes),
            _ => None,
        })
        .chain(match lowered.out {
            Out::Vector(bytes) => Some(bytes),
            _ => None,
        })
        .max();
    if width.is_some_and(|width| widest != Some(width)) {
        return None;
    }
    Some(Ok(lowered))
}

fn lowered(
    words: &[&str],
    width: Option<u32>,
    args: &[Given],
    result: u32,
    features: TargetFeatureSet,
) -> Option<Result<Lowered, String>> {
    let vector = |at: usize| {
        let it = args.get(at)?;
        (!it.pointer && matches!(it.bytes, 16 | 32 | 64)).then_some(it.bytes)
    };
    let mask = |at: usize| {
        let it = args.get(at)?;
        (!it.pointer && matches!(it.bytes, 1 | 2 | 4 | 8)).then_some(it.bytes)
    };
    let pointer = |at: usize| args.get(at).is_some_and(|it| it.pointer).then_some(());
    let int = |at: usize| {
        let it = args.get(at)?;
        (!it.pointer && matches!(it.bytes, 4 | 8)).then_some(it.bytes)
    };
    let round = |at: usize, kind: Rounding| rounding(args.get(at).and_then(|it| it.constant), kind);
    let out = |bytes: u32| reg(bytes, 0);
    let vector_result = matches!(result, 16 | 32 | 64).then_some(result);
    let ok = |text: String, loads: &[Load], out: Out| {
        // alloc: the stub's loads, the output
        Some(Ok(Lowered { text, loads: loads.to_vec(), out }))
    };
    // An error of the operands, which LLVM would refuse.
    macro_rules! check {
        ($e:expr) => {
            match $e {
                Ok(it) => it,
                Err(e) => return Some(Err(e)),
            }
        };
    }
    let (family, words) = words.split_first()?;
    // AVX512-FP16's complex multiply-adds of pairs of halves: the products of the first two
    // vectors (`vfcmadd` the first by the conjugate of the second) added to the third, which
    // the result is, under the mask of the fourth, the other lanes the third's (`mask`) or
    // zeros (`maskz`); rounded as a fifth says where there is one. `csh` is of the low pair,
    // the others the first vector's.
    if *family == "avx512fp16"
        && let &[how @ ("mask" | "maskz"), op @ ("vfmadd" | "vfcmadd"), lanes @ ("cph" | "csh")] =
            words
    {
        let (a, b, c, k, result) = (vector(0)?, vector(1)?, vector(2)?, mask(3)?, vector_result?);
        let scalar = lanes == "csh";
        if a != result || b != result || c != result || (scalar && result != 16) {
            return None;
        }
        let rounding = match args.len() {
            4 if !scalar => "",
            5 if scalar || width == Some(64) => check!(round(4, Rounding::Round)),
            _ => return None,
        };
        let zeros = if how == "maskz" { " {z}" } else { "" };
        // alloc: the stub's text, the output
        let text = format!(
            "vmovaps {0}, {3}\n{op}c{1} {0} {{k4}}{zeros}, {2}, {4}{rounding}",
            out(result),
            &lanes[1..],
            arg(a, 0),
            arg(c, 2),
            arg(b, 1),
        );
        let loads =
            [Load::Vector(a), Load::Vector(b), Load::Vector(c), Load::Mask(k), Load::Immediate];
        return ok(text, &loads[..args.len()], Out::Vector(result));
    }
    let (masked, words) = match words.split_first() {
        Some((&"mask", rest)) => (true, rest),
        _ => (false, words),
    };
    match (*family, masked, words, args.len()) {
        // Each lane of the mask where the two compare as the predicate says, under the mask of
        // the fourth operand.
        ("avx512", true, &["cmp", lanes @ ("ps" | "pd")], 5) => {
            let (a, k) = (vector(0)?, mask(3)?);
            if vector(1)? != a || result != k {
                return None;
            }
            let predicate = match args[2].constant {
                Some(it @ 0..32) => it,
                // alloc: the error's message
                Some(it) => return Some(Err(format!("of predicate {it}"))),
                // alloc: the error's message
                None => return Some(Err("of a predicate that is not a constant".to_owned())),
            };
            let sae = check!(round(4, Rounding::Sae));
            let (x, y) = (arg(a, 0), arg(a, 1));
            // alloc: the stub's text, the output
            let text = format!("vcmp{lanes} k0 {{k4}}, {x}, {y}{sae}, {predicate}");
            let loads = [
                Load::Vector(a),
                Load::Vector(a),
                Load::Immediate,
                Load::Mask(k),
                Load::Immediate,
            ];
            ok(text, &loads, Out::Mask(k))
        }
        // Whether the low halves compare as the predicate says, as 0 or 1, with whether to
        // suppress exceptions: LLVM compares into a mask register and moves it out, the bits
        // above it zero.
        ("avx512fp16", false, &["vcomi", "sh"], 4) => {
            if vector(0)? != 16 || vector(1)? != 16 || result != 4 {
                return None;
            }
            let predicate = match args[2].constant {
                Some(it @ 0..32) => it,
                // alloc: the error's message
                Some(it) => return Some(Err(format!("of predicate {it}"))),
                // alloc: the error's message
                None => return Some(Err("of a predicate that is not a constant".to_owned())),
            };
            let sae = check!(round(3, Rounding::Sae));
            // alloc: the stub's text, the output
            let text = format!("vcmpsh k0, xmm1, xmm2{sae}, {predicate}\nkmovw eax, k0");
            let loads = [Load::Vector(16), Load::Vector(16), Load::Immediate, Load::Immediate];
            ok(text, &loads, Out::Int(4))
        }
        // An integer of 32 or 64 bits (`si64`), signed or not (`usi`), to the low half, the
        // others the first vector's, rounded as the third operand says.
        (
            "avx512fp16",
            false,
            &[op @ ("vcvtsi2sh" | "vcvtusi2sh" | "vcvtsi642sh" | "vcvtusi642sh")],
            3,
        ) => {
            let (a, bytes) = (vector(0)?, int(1)?);
            if a != 16 || result != 16 || bytes != if op.contains("64") { 8 } else { 4 } {
                return None;
            }
            let rounding = check!(round(2, Rounding::Round));
            let from = if bytes == 8 { "rcx" } else { "ecx" };
            let op = match op.split_once("64") {
                Some((head, rest)) => Concat([head, rest, ""]),
                None => Concat([op, "", ""]),
            };
            // Intel's syntax has the rounding before the integer register.
            // alloc: the stub's text, the output
            let text = format!("{op} xmm0, xmm1{rounding}, {from}");
            ok(text, &[Load::Vector(a), Load::Int(bytes), Load::Immediate], Out::Vector(16))
        }
        // Pairs of single floats to bfloat16s, rounded to nearest even: those of the second
        // vector in the low half, the first's in the high one.
        ("avx512bf16", false, &["cvtne2ps2bf16"], 2) => {
            let (a, result) = (vector(0)?, vector_result?);
            if vector(1)? != a || result != a {
                return None;
            }
            let (r, x, y) = (out(result), arg(a, 0), arg(a, 1));
            // alloc: the stub's text, the output
            let text = format!("vcvtne2ps2bf16 {r}, {x}, {y}");
            ok(text, &[Load::Vector(a), Load::Vector(a)], Out::Vector(result))
        }
        // Single floats to bfloat16s, rounded to nearest even, in a vector of half the size.
        // AVX-NE-CONVERT has the instruction too, in an encoding of its own, so where the
        // function has that as well LLVM's choice is not known here.
        ("avx512bf16", false, &["cvtneps2bf16"], 1) => {
            let a = vector(0)?;
            if a < 32 || result != a / 2 {
                return None;
            }
            if features.contains(TargetFeature::AVXNECONVERT) {
                // alloc: the error's message
                return Some(Err("in a function with AVX-NE-CONVERT too".to_owned()));
            }
            // alloc: the stub's text, the output
            let text = format!("{{evex}} vcvtneps2bf16 {}, {}", out(result), arg(a, 0));
            ok(text, &[Load::Vector(a)], Out::Vector(result))
        }
        // The same of four, merged into the second operand under the mask of the third.
        ("avx512bf16", true, &["cvtneps2bf16"], 3) => {
            let (a, src, k) = (vector(0)?, vector(1)?, mask(2)?);
            if a != 16 || src != 16 || result != 16 {
                return None;
            }
            // alloc: the stub's text, the output
            let text = "vmovaps xmm0, xmm2\nvcvtneps2bf16 xmm0 {k3}, xmm1".to_owned();
            ok(text, &[Load::Vector(a), Load::Vector(src), Load::Mask(k)], Out::Vector(16))
        }
        // The sum of each single float of the first vector and the products of the two pairs
        // of bfloat16s of the other two at it.
        ("avx512bf16", false, &["dpbf16ps"], 3) => {
            let (src, a, b, result) = (vector(0)?, vector(1)?, vector(2)?, vector_result?);
            if src != result || a != result || b != result {
                return None;
            }
            // alloc: the stub's text, the output
            let text = format!(
                "vmovaps {0}, {1}\nvdpbf16ps {0}, {2}, {3}",
                out(result),
                arg(src, 0),
                arg(a, 1),
                arg(b, 2),
            );
            ok(text, &[Load::Vector(src), Load::Vector(a), Load::Vector(b)], Out::Vector(result))
        }
        // AVX-NE-CONVERT's, one word each, `128` or `256` bits of result: a bfloat16 or a
        // half at an address made a single float in every lane (`vbcstne`), those at the even
        // (`vcvtnee`) or odd (`vcvtneo`) places of the vector at an address made single floats,
        // or single floats made bfloat16s rounded to nearest even. LLVM may encode the last by
        // AVX512-BF16's instruction where the function has that too.
        (name, false, &[], 1) if let Some((op, bits)) = no_exceptions_convert(name) => {
            let bytes = bits / 8;
            if op == "vcvtneps2bf16" {
                let a = vector(0)?;
                if a != bytes || result != 16 {
                    return None;
                }
                if features.contains(TargetFeature::AVX512BF16) {
                    // alloc: the error's message
                    return Some(Err("in a function with AVX512-BF16 too".to_owned()));
                }
                // alloc: the stub's text, the output
                let text = format!("{{vex}} vcvtneps2bf16 xmm0, {}", arg(a, 0));
                return ok(text, &[Load::Vector(a)], Out::Vector(16));
            }
            pointer(0)?;
            if result != bytes {
                return None;
            }
            let read = if op.starts_with("vbcst") { "word" } else { memory(bytes)? };
            // alloc: the stub's text, the output
            let text = format!("{op} {}, {read} ptr [r11]", out(result));
            ok(text, &[Load::Pointer], Out::Vector(result))
        }
        // Of the first vector, merged into the second under the mask of the third, rounded as
        // a fourth says where there is one (the 512-bit forms).
        (_, _, _, 3 | 4) if let Some((op, kind)) = unary(family, masked, words) => {
            let (a, src, k, result) = (vector(0)?, vector(1)?, mask(2)?, vector_result?);
            if src != result {
                return None;
            }
            let rounding = match (args.len(), kind) {
                (3, _) => "",
                (_, Some(kind)) if width == Some(64) => check!(round(3, kind)),
                _ => return None,
            };
            // alloc: the stub's text, the output
            let text = format!(
                "vmovaps {}, {}\n{op} {} {{k3}}, {}{rounding}",
                out(result),
                arg(src, 1),
                out(result),
                arg(a, 0),
            );
            let loads = [Load::Vector(a), Load::Vector(src), Load::Mask(k), Load::Immediate];
            ok(text, &loads[..args.len()], Out::Vector(result))
        }
        // Of the first two vectors, merged into the third under the mask of the fourth: on
        // whole vectors (`scalef`), or on the low lane of the second with the others of the
        // first (`ss`, `sd`), rounded as a fifth says where there is one.
        (_, _, _, 4 | 5) if let Some((op, kind, scalar)) = binary(family, masked, words) => {
            let (a, b, src, k, result) =
                (vector(0)?, vector(1)?, vector(2)?, mask(3)?, vector_result?);
            if src != result || (scalar && (a != 16 || b != 16 || result != 16)) {
                return None;
            }
            let rounding = match (args.len(), kind) {
                (4, None) => "",
                (4, _) if !scalar => "",
                (5, Some(kind)) if scalar || width == Some(64) => check!(round(4, kind)),
                _ => return None,
            };
            // alloc: the stub's text, the output
            let text = format!(
                "vmovaps {}, {}\n{op} {} {{k4}}, {}, {}{rounding}",
                out(result),
                arg(src, 2),
                out(result),
                arg(a, 0),
                arg(b, 1),
            );
            let loads = [
                Load::Vector(a),
                Load::Vector(b),
                Load::Vector(src),
                Load::Mask(k),
                Load::Immediate,
            ];
            ok(text, &loads[..args.len()], Out::Vector(result))
        }
        // Lane by lane of two vectors, or (`psll` and the other shifts) of a vector by the
        // count in the low 64 bits of the second; `max` and `min` with whether to suppress
        // exceptions.
        ("avx512" | "avx512fp16", false, _, 2 | 3)
            if let Some(op) = lanewise(words, args.len()) =>
        {
            let (a, b, result) = (vector(0)?, vector(1)?, vector_result?);
            let sae = match args.len() {
                3 => check!(round(2, Rounding::Sae)),
                _ => "",
            };
            let (r, x, y) = (out(result), arg(a, 0), arg(b, 1));
            // alloc: the stub's text, the output
            let text = format!("{op} {r}, {x}, {y}{sae}");
            let loads = [Load::Vector(a), Load::Vector(b), Load::Immediate];
            ok(text, &loads[..args.len()], Out::Vector(result))
        }
        // The bits of each 64-bit lane of the first vector that the bytes of the second name.
        ("avx512", false, &["vpshufbitqmb"], 2) => {
            let (a, b) = (vector(0)?, vector(1)?);
            if result != a / 8 {
                return None;
            }
            // alloc: the stub's text, the output
            let text = format!("vpshufbitqmb k0, {}, {}", arg(a, 0), arg(b, 1));
            ok(text, &[Load::Vector(a), Load::Vector(b)], Out::Mask(result))
        }
        // Each lane's bits of the lanes before it that are equal to it.
        ("avx512", false, &["conflict", lanes @ ("d" | "q")], 1) => {
            let (a, result) = (vector(0)?, vector_result?);
            // alloc: the stub's text, the output
            let text = format!("vpconflict{lanes} {}, {}", out(result), arg(a, 0));
            ok(text, &[Load::Vector(a)], Out::Vector(result))
        }
        // 64-bit integers to floats, and integers of 16, 32 or 64 bits to halves, rounded as
        // the second operand says: in any direction at 512 bits, only the current one below.
        ("avx512", false, &[op @ ("sitofp" | "uitofp"), "round", to, from], 2) => {
            let (a, result) = (vector(0)?, vector_result?);
            let split = ['f', 'i'];
            let ((count, to_bits), (from_count, from_bits)) =
                (to.split_once(split)?, from.split_once(split)?);
            let float = match to_bits {
                "16" if to.contains('f') => "ph",
                "32" if to.contains('f') => "ps",
                "64" if to.contains('f') => "pd",
                _ => return None,
            };
            let int = match (from_bits, float) {
                ("64", _) => "qq",
                ("32", "ph") => "dq",
                ("16", "ph") => "w",
                _ => return None,
            };
            if count != from_count || !from.contains('i') {
                return None;
            }
            let rounding = check!(round(1, Rounding::Round));
            if a != 64 && !rounding.is_empty() {
                // alloc: the error's message
                return Some(Err("of a rounding below 512 bits".to_owned()));
            }
            let unsigned = if op == "uitofp" { "u" } else { "" };
            let (r, x) = (out(result), arg(a, 0));
            // alloc: the stub's text, the output
            let text = format!("vcvt{unsigned}{int}2{float} {r}, {x}{rounding}");
            ok(text, &[Load::Vector(a), Load::Immediate], Out::Vector(result))
        }
        // The sum of each 32-bit lane of the first vector and the products of the bytes
        // (`b`) or words (`w`) of the other two at it, wrapping or saturated (`s`).
        ("avx512" | "avx2", false, &[op], 3) if op.starts_with("vpdp") => {
            let (src, a, b, result) = (vector(0)?, vector(1)?, vector(2)?, vector_result?);
            if src != result || a != result || b != result {
                return None;
            }
            let known = match *family {
                "avx2" => ["vpdpbssd", "vpdpbsud", "vpdpbuud", "vpdpwsud", "vpdpwusd", "vpdpwuud"]
                    .iter()
                    .any(|&it| op == it || op.strip_suffix('s') == Some(it)),
                _ => matches!(op, "vpdpbusd" | "vpdpbusds" | "vpdpwssd" | "vpdpwssds"),
            };
            if !known {
                return None;
            }
            // Below 512 bits the instruction is AVX-VNNI's where the function has it (LLVM
            // takes the shorter encoding) and AVX512-VNNI's otherwise: each runs only on a
            // processor with its extension. AVX-VNNI-INT8's and -INT16's have one encoding.
            let encoding = match (*family, result) {
                ("avx512", 16 | 32) if features.contains(TargetFeature::AVXVNNI) => "{vex} ",
                ("avx512", 16 | 32) => "{evex} ",
                _ => "",
            };
            // alloc: the stub's text, the output
            let text = format!(
                "vmovaps {0}, {1}\n{encoding}{op} {0}, {2}, {3}",
                out(result),
                arg(src, 0),
                arg(a, 1),
                arg(b, 2),
            );
            ok(text, &[Load::Vector(src), Load::Vector(a), Load::Vector(b)], Out::Vector(result))
        }
        // The low lane to an integer of 32 or 64 bits, rounded as the second operand says
        // (`cvtt` toward zero, with whether to suppress exceptions).
        ("avx512" | "avx512fp16", false, &[op], 2)
            if op.starts_with("vcvts") || op.starts_with("cvtts") || op.starts_with("vcvtts") =>
        {
            let op = op.trim_start_matches('v');
            let (op, bits) = match op.strip_suffix("64") {
                Some(op) => (op, 64),
                None => (op.strip_suffix("32").unwrap_or(op), 32),
            };
            let (kind, of) = match op.strip_prefix("cvtt") {
                Some(of) => (Rounding::Sae, of),
                None => (Rounding::Round, op.strip_prefix("cvt")?),
            };
            if !matches!(of, "ss2si" | "sd2si" | "ss2usi" | "sd2usi" | "sh2si" | "sh2usi") {
                return None;
            }
            let a = vector(0)?;
            if a != 16 || result != bits / 8 {
                return None;
            }
            let rounding = check!(round(1, kind));
            let to = if bits == 64 { "rax" } else { "eax" };
            // alloc: the stub's text, the output
            let text = format!("v{op} {to}, xmm1{rounding}");
            ok(text, &[Load::Vector(a), Load::Immediate], Out::Int(result))
        }
        // Stores: the lanes truncated (`pmov.mem`), or those of the mask one after another
        // (`compress.store`), to the address of the first operand.
        ("avx512", true, &[op, lanes, "mem"] | &[op, "store", lanes], 3)
            if matches!(op, "pmov" | "pmovs" | "pmovus" | "compress")
                && (op == "compress") == (words[1] == "store") =>
        {
            pointer(0)?;
            let (a, k) = (vector(1)?, mask(2)?);
            if result != 0 {
                return None;
            }
            let (mnemonic, bytes) = match (op, lanes.as_bytes()) {
                ("compress", [b'd' | b'q' | b'w' | b'b']) => {
                    (Concat(["vpcompress", lanes, ""]), a)
                }
                ("compress", b"ps" | b"pd") => (Concat(["vcompress", lanes, ""]), a),
                ("compress", _) => return None,
                (_, &[from, to]) => {
                    let (from, to) = (lane_bytes(from.into())?, lane_bytes(to.into())?);
                    (Concat(["v", op, lanes]), a / from * to)
                }
                _ => return None,
            };
            let (size, x) = (memory(bytes)?, arg(a, 1));
            // alloc: the stub's text, the output
            let text = format!("{mnemonic} {size} ptr [r11] {{k3}}, {x}");
            ok(text, &[Load::Pointer, Load::Vector(a), Load::Mask(k)], Out::Nothing)
        }
        // A load of as many lanes as the mask has bits set, into those lanes, the others the
        // second operand's.
        ("avx512", true, &["expand", "load", lanes], 3) => {
            pointer(0)?;
            let (src, k, result) = (vector(1)?, mask(2)?, vector_result?);
            if src != result {
                return None;
            }
            let mnemonic = match lanes {
                "d" | "q" | "w" | "b" => Concat(["vpexpand", lanes, ""]),
                "ps" | "pd" => Concat(["vexpand", lanes, ""]),
                _ => return None,
            };
            // alloc: the stub's text, the output
            let text = format!(
                "vmovaps {}, {}\n{mnemonic} {} {{k3}}, {} ptr [r11]",
                out(result),
                arg(src, 1),
                out(result),
                memory(result)?,
            );
            ok(text, &[Load::Pointer, Load::Vector(src), Load::Mask(k)], Out::Vector(result))
        }
        // The lanes of each vector equal to one of the other's, as a pair of masks.
        ("avx512", false, &["vp2intersect", lanes @ ("d" | "q")], 2) => {
            let a = vector(0)?;
            if vector(1)? != a {
                return None;
            }
            let count = a / lane_bytes(lanes.chars().next()?)?;
            let bytes = count.div_ceil(8);
            if result != 2 * bytes {
                return None;
            }
            // alloc: the stub's text, the output
            let text = format!("vp2intersect{lanes} k0, {}, {}", arg(a, 0), arg(a, 1));
            ok(text, &[Load::Vector(a), Load::Vector(a)], Out::Masks(bytes, bytes))
        }
        _ => None,
    }
}

/// The instruction LLVM selects for the generic intrinsic `op` of `core::intrinsics::simd`
/// on a vector of halves of `from` bytes, giving `to` bytes, in a function with the target
/// features `features` (implied ones included): AVX512-FP16's of the lanes for the arithmetic,
/// operands in order, and F16C's `vcvtph2ps` for `simd_cast` to single floats where the
/// function does not have AVX512-FP16, which converts by its own. `None` for any other.
pub(super) fn half_vector(
    op: SimdOp,
    from: u32,
    to: u32,
    features: impl Fn(TargetFeature) -> bool,
) -> Option<Lowered> {
    let fp16 = features(TargetFeature::AVX512FP16)
        && (from == 64 || matches!(from, 16 | 32) && features(TargetFeature::AVX512VL));
    let (text, operands) = match op {
        SimdOp::Add | SimdOp::Sub | SimdOp::Mul | SimdOp::Div if fp16 && from == to => {
            let op = match op {
                SimdOp::Add => "add",
                SimdOp::Sub => "sub",
                SimdOp::Mul => "mul",
                _ => "div",
            };
            let (r, x, y) = (reg(to, 0), arg(from, 0), arg(from, 1));
            // alloc: the stub's text, the output
            (format!("v{op}ph {r}, {x}, {y}"), 2)
        }
        SimdOp::Fsqrt if fp16 && from == to => {
            // alloc: the stub's text, the output
            (format!("vsqrtph {}, {}", reg(to, 0), arg(from, 0)), 1)
        }
        SimdOp::Cast
            if features(TargetFeature::F16C)
                && !features(TargetFeature::AVX512FP16)
                && matches!((from, to), (8, 16) | (16, 32)) =>
        {
            // alloc: the stub's text, the output
            (format!("vcvtph2ps {}, xmm1", reg(to, 0)), 1)
        }
        _ => return None,
    };
    // alloc: the stub's loads, the output
    let loads = vec![Load::Vector(from); operands];
    Some(Lowered { text, loads, out: Out::Vector(to) })
}

/// The instruction of an AVX-NE-CONVERT intrinsic named `name` (`vcvtneebf162ps128`), and the
/// bits of its result.
fn no_exceptions_convert(name: &str) -> Option<(&str, u32)> {
    let (op, bits) = match name.split_at_checked(name.len().checked_sub(3)?)? {
        (op, "128") => (op, 128),
        (op, "256") => (op, 256),
        _ => return None,
    };
    let known = [
        "vbcstnebf162ps",
        "vbcstnesh2ps",
        "vcvtneebf162ps",
        "vcvtneeph2ps",
        "vcvtneobf162ps",
        "vcvtneoph2ps",
        "vcvtneps2bf16",
    ];
    known.contains(&op).then_some((op, bits))
}

/// The instruction of an intrinsic of two vectors and nothing else, of `count` operands.
fn lanewise<'a>(words: &[&'a str], count: usize) -> Option<Concat<'a>> {
    Some(match (words, count) {
        (&[op @ ("max" | "min"), lanes @ ("ps" | "pd")], 3) => Concat(["v", op, lanes]),
        (&[op @ ("max" | "min"), "ph"], 2 | 3) => Concat(["v", op, "ph"]),
        (&[op @ ("psll" | "psrl" | "psra"), lanes @ ("d" | "q" | "w")], 2) => {
            Concat(["v", op, lanes])
        }
        (&["vpermilvar", lanes @ ("ps" | "pd")], 2) => Concat(["vpermil", lanes, ""]),
        (&["pmul", "hr", "sw"], 2) => Concat(["vpmulhrsw", "", ""]),
        (&["pmultishift", "qb"], 2) => Concat(["vpmultishiftqb", "", ""]),
        _ => return None,
    })
}

/// The instruction of an intrinsic of one vector merged into a source under a mask, and what
/// a fourth operand says where there is one.
fn unary<'a>(
    family: &str,
    masked: bool,
    words: &[&'a str],
) -> Option<(Concat<'a>, Option<Rounding>)> {
    if family == "avx512fp16" {
        return Some(match (masked, words) {
            // Between halves and integers or other floats: rounded as the operand says, or
            // toward zero (`vcvtt`) and widening, which are exact, with whether to suppress
            // exceptions.
            (
                true,
                &[
                    op @ ("vcvtph2w" | "vcvtph2uw" | "vcvtph2dq" | "vcvtph2udq" | "vcvtph2qq"
                    | "vcvtph2uqq" | "vcvtps2phx" | "vcvtpd2ph" | "vcvtdq2ph" | "vcvtudq2ph"
                    | "vcvtqq2ph" | "vcvtuqq2ph"),
                ],
            ) => (Concat([op, "", ""]), Some(Rounding::Round)),
            (
                true,
                &[
                    op @ ("vcvttph2w" | "vcvttph2uw" | "vcvttph2dq" | "vcvttph2udq" | "vcvttph2qq"
                    | "vcvttph2uqq" | "vcvtph2psx" | "vcvtph2pd"),
                ],
            ) => (Concat([op, "", ""]), Some(Rounding::Sae)),
            (true, &["getexp", "ph"]) => (Concat(["vgetexpph", "", ""]), Some(Rounding::Sae)),
            (true, &[op @ ("rcp" | "rsqrt"), "ph"]) => (Concat(["v", op, "ph"]), None),
            _ => return None,
        });
    }
    if family != "avx512" {
        return None;
    }
    Some(match (masked, words) {
        (false, &[op @ ("rcp14" | "rsqrt14"), lanes @ ("ps" | "pd")]) => {
            (Concat(["v", op, lanes]), None)
        }
        (true, &["getexp", lanes @ ("ps" | "pd")]) => {
            (Concat(["vgetexp", lanes, ""]), Some(Rounding::Sae))
        }
        (
            true,
            &[
                op @ ("cvtps2dq" | "cvtps2udq" | "cvtpd2dq" | "cvtpd2udq" | "cvtpd2ps" | "cvtpd2qq"
                | "cvtps2qq" | "cvtpd2uqq" | "cvtps2uqq"),
            ],
        ) => (Concat(["v", op, ""]), Some(Rounding::Round)),
        (
            true,
            &[
                op @ ("cvttps2dq" | "cvttps2udq" | "cvttpd2dq" | "cvttpd2udq" | "cvttpd2qq"
                | "cvttps2qq" | "cvttpd2uqq" | "cvttps2uqq" | "cvtps2pd" | "vcvtph2ps"),
            ],
        ) => (Concat(["v", op.trim_start_matches('v'), ""]), Some(Rounding::Sae)),
        (true, &[op @ ("cvtqq2ps" | "cvtuqq2ps")]) => (Concat(["v", op, ""]), None),
        // Truncations to narrower lanes: wrapping, or saturated as signed (`s`) or unsigned
        // (`us`) integers.
        (
            true,
            &[
                op @ ("pmov" | "pmovs" | "pmovus"),
                lanes @ ("dw" | "db" | "qw" | "qb" | "qd" | "wb"),
            ],
        ) => (Concat(["v", op, lanes]), None),
        // The lanes the mask has set, one after another (`compress`), or the first lanes put
        // where it has bits set (`expand`).
        (true, &[op @ ("compress" | "expand"), lanes]) => match lanes {
            "d" | "q" | "w" | "b" => (Concat(["vp", op, lanes]), None),
            "ps" | "pd" => (Concat(["v", op, lanes]), None),
            _ => return None,
        },
        _ => return None,
    })
}

/// The instruction of an intrinsic of two vectors merged into a source under a mask, what a
/// fifth operand says where there is one, and whether it is of the low lane alone.
fn binary<'a>(
    family: &str,
    masked: bool,
    words: &[&'a str],
) -> Option<(Concat<'a>, Option<Rounding>, bool)> {
    let words = match words.split_last() {
        Some((&"round", rest)) => rest,
        _ => words,
    };
    if family == "avx512fp16" {
        return Some(match (masked, words) {
            (true, &["scalef", "ph"]) => {
                (Concat(["vscalefph", "", ""]), Some(Rounding::Round), false)
            }
            (true, &[op @ ("max" | "min" | "getexp"), "sh"]) => {
                (Concat(["v", op, "sh"]), Some(Rounding::Sae), true)
            }
            (true, &[op @ ("sqrt" | "scalef"), "sh"]) => {
                (Concat(["v", op, "sh"]), Some(Rounding::Round), true)
            }
            (true, &[op @ ("rcp" | "rsqrt"), "sh"]) => (Concat(["v", op, "sh"]), None, true),
            (true, &[op @ ("vcvtss2sh" | "vcvtsd2sh")]) => {
                (Concat([op, "", ""]), Some(Rounding::Round), true)
            }
            (true, &[op @ ("vcvtsh2ss" | "vcvtsh2sd")]) => {
                (Concat([op, "", ""]), Some(Rounding::Sae), true)
            }
            // Products of pairs of halves as complex numbers (`vfcmul` by the conjugate of
            // the second), each pair one lane of the mask.
            (true, &[op @ ("vfmul" | "vfcmul"), "cph"]) => {
                (Concat([op, "cph", ""]), Some(Rounding::Round), false)
            }
            (true, &[op @ ("vfmul" | "vfcmul"), "csh"]) => {
                (Concat([op, "csh", ""]), Some(Rounding::Round), true)
            }
            _ => return None,
        });
    }
    if family != "avx512" {
        return None;
    }
    Some(match (masked, words) {
        (true, &["scalef", lanes @ ("ps" | "pd")]) => {
            (Concat(["vscalef", lanes, ""]), Some(Rounding::Round), false)
        }
        (true, &[op @ ("max" | "min" | "getexp"), lanes @ ("ss" | "sd")]) => {
            (Concat(["v", op, lanes]), Some(Rounding::Sae), true)
        }
        (true, &[op @ ("sqrt" | "scalef"), lanes @ ("ss" | "sd")]) => {
            (Concat(["v", op, lanes]), Some(Rounding::Round), true)
        }
        (true, &["cvtss2sd"]) => (Concat(["vcvtss2sd", "", ""]), Some(Rounding::Sae), true),
        (true, &["cvtsd2ss"]) => (Concat(["vcvtsd2ss", "", ""]), Some(Rounding::Round), true),
        (false, &[op @ ("rcp14" | "rsqrt14"), lanes @ ("ss" | "sd")]) => {
            (Concat(["v", op, lanes]), None, true)
        }
        _ => return None,
    })
}

/// The text a rounding operand of `value` adds after the last register of the instruction.
fn rounding(value: Option<u128>, kind: Rounding) -> Result<&'static str, String> {
    let Some(value) = value else {
        // alloc: the error's message
        return Err("of a rounding that is not a constant".to_owned());
    };
    Ok(match (kind, value) {
        (_, 4) => "",
        (Rounding::Sae, 8 | 12) => ", {sae}",
        (Rounding::Round, 8) => ", {rn-sae}",
        (Rounding::Round, 9) => ", {rd-sae}",
        (Rounding::Round, 10) => ", {ru-sae}",
        (Rounding::Round, 11) => ", {rz-sae}",
        _ => return Err(format!("of rounding {value}")), // alloc: the error's message
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Given`s from a list: a number of bytes, `p` for an address, `=v` for a constant.
    fn given(shape: &str) -> Vec<Given> {
        shape
            .split_whitespace()
            .map(|it| match it {
                "p" => Given { bytes: 8, pointer: true, constant: None },
                _ => match it.split_once('=') {
                    Some((bytes, value)) => Given {
                        bytes: bytes.parse().unwrap(),
                        pointer: false,
                        constant: Some(value.parse().unwrap()),
                    },
                    None => Given { bytes: it.parse().unwrap(), pointer: false, constant: None },
                },
            })
            .collect()
    }

    fn describe(cases: &[(&str, &str, u32, &[&str])]) -> String {
        let mut out = String::new();
        for &(name, shape, result, features) in cases {
            let features = features.iter().map(|it| TargetFeature::from_name(it).unwrap()).collect();
            let text = match lower(name, &given(shape), result, features) {
                Some(Ok(it)) => format!("{} {:?}", it.text.replace('\n', "; "), it.out),
                Some(Err(e)) => e,
                None => "none".to_owned(),
            };
            out += &format!("{name}: {text}\n");
        }
        out
    }

    #[test]
    fn each_intrinsic_is_the_instruction_llvm_selects() {
        let described = describe(&[
            ("x86.avx512.mask.cmp.ps.512", "64 64 4=1 2 4=8", 2, &[]),
            ("x86.avx512.mask.cmp.pd.512", "64 64 4=31 1 4=4", 1, &[]),
            ("x86.avx512.mask.cmp.ps.512", "64 64 4=32 2 4=4", 2, &[]),
            ("x86.avx512.rcp14.ps.512", "64 64 2", 64, &[]),
            ("x86.avx512.mask.cvtps2dq.512", "64 64 2 4=10", 64, &[]),
            ("x86.avx512.mask.cvtps2dq.512", "64 64 2 4=12", 64, &[]),
            ("x86.avx512.mask.cvttpd2udq.512", "64 32 1 4=8", 32, &[]),
            ("x86.avx512.mask.cvtps2pd.512", "32 64 1 4=4", 64, &[]),
            ("x86.avx512.mask.cvtpd2dq.128", "16 16 1", 16, &[]),
            ("x86.avx512.mask.pmovus.qb.512", "64 16 1", 16, &[]),
            ("x86.avx512.mask.scalef.ps.512", "64 64 64 2 4=9", 64, &[]),
            ("x86.avx512.mask.scalef.pd.256", "32 32 32 1", 32, &[]),
            ("x86.avx512.mask.sqrt.sd", "16 16 16 1 4=11", 16, &[]),
            ("x86.avx512.mask.max.ss.round", "16 16 16 1 4=8", 16, &[]),
            ("x86.avx512.rsqrt14.sd", "16 16 16 1", 16, &[]),
            ("x86.avx512.max.pd.512", "64 64 4=8", 64, &[]),
            ("x86.avx512.psra.q.128", "16 16", 16, &[]),
            ("x86.avx512.pmul.hr.sw.512", "64 64", 64, &[]),
            ("x86.avx512.conflict.q.256", "32", 32, &[]),
            ("x86.avx512.sitofp.round.v8f64.v8i64", "64 4=9", 64, &[]),
            ("x86.avx512.uitofp.round.v4f32.v4i64", "32 4=4", 16, &[]),
            ("x86.avx512.uitofp.round.v4f32.v4i64", "32 4=8", 16, &[]),
            ("x86.avx512.vpdpbusd.256", "32 32 32", 32, &["avxvnni"]),
            ("x86.avx512.vpdpbusd.256", "32 32 32", 32, &["avx512vnni", "avx512vl"]),
            ("x86.avx512.vpdpwssds.512", "64 64 64", 64, &[]),
            ("x86.avx2.vpdpbsuds.128", "16 16 16", 16, &[]),
            ("x86.avx512.vcvtsd2usi64", "16 4=8", 8, &[]),
            ("x86.avx512.cvttss2si", "16 4=8", 4, &[]),
            ("x86.avx512.mask.pmovs.dw.mem.256", "p 32 1", 0, &[]),
            ("x86.avx512.mask.compress.store.ps.512", "p 64 2", 0, &[]),
            ("x86.avx512.mask.expand.load.b.128", "p 16 2", 16, &[]),
            ("x86.avx512.mask.compress.w.512", "64 64 4", 64, &[]),
            ("x86.avx512.vpshufbitqmb.512", "64 64", 8, &[]),
            ("x86.avx512.vp2intersect.d.512", "64 64", 4, &[]),
            ("x86.avx512.vp2intersect.q.128", "16 16", 2, &[]),
            ("x86.avx512.rcp14.ps.512", "32 32 1", 32, &[]),
            ("x86.avx512fp16.vcomi.sh", "16 16 4=1 4=8", 4, &[]),
            ("x86.avx512fp16.vcvtsi642sh", "16 8 4=11", 16, &[]),
            ("x86.avx512fp16.vcvtusi2sh", "16 4 4=4", 16, &[]),
            ("x86.avx512fp16.vcvttsh2usi32", "16 4=8", 4, &[]),
            ("x86.avx512fp16.vcvtsh2si64", "16 4=10", 8, &[]),
            ("x86.avx512.sitofp.round.v16f16.v16i32", "64 4=10", 32, &[]),
            ("x86.avx512.uitofp.round.v8f16.v8i16", "16 4=4", 16, &[]),
            ("x86.avx512.sitofp.round.v4f32.v4i32", "16 4=4", 16, &[]),
            ("x86.avx512fp16.mask.vfmadd.cph.512", "64 64 64 2 4=9", 64, &[]),
            ("x86.avx512fp16.maskz.vfcmadd.csh", "16 16 16 1 4=10", 16, &[]),
            ("x86.avx512fp16.mask.vfmul.cph.256", "32 32 32 1", 32, &[]),
            ("x86.avx512fp16.mask.vfcmul.csh", "16 16 16 1 4=8", 16, &[]),
            ("x86.avx512fp16.max.ph.128", "16 16", 16, &[]),
            ("x86.avx512fp16.min.ph.512", "64 64 4=8", 64, &[]),
            ("x86.avx512fp16.mask.vcvtph2w.512", "64 64 4 4=9", 64, &[]),
            ("x86.avx512fp16.mask.vcvttph2qq.256", "16 32 1", 32, &[]),
            ("x86.avx512fp16.mask.vcvtpd2ph.512", "64 16 1 4=11", 16, &[]),
            ("x86.avx512fp16.mask.vcvtph2psx.512", "32 64 2 4=8", 64, &[]),
            ("x86.avx512fp16.mask.rsqrt.ph.256", "32 32 2", 32, &[]),
            ("x86.avx512fp16.mask.getexp.sh", "16 16 16 1 4=8", 16, &[]),
            ("x86.avx512fp16.mask.sqrt.sh", "16 16 16 1 4=9", 16, &[]),
            ("x86.avx512fp16.mask.vcvtss2sh.round", "16 16 16 1 4=11", 16, &[]),
            ("x86.avx512fp16.mask.vcvtsh2sd.round", "16 16 16 1 4=8", 16, &[]),
            ("x86.avx512fp16.mask.scalef.ph.512", "64 64 64 4 4=10", 64, &[]),
            ("x86.avx512bf16.cvtne2ps2bf16.256", "32 32", 32, &[]),
            ("x86.avx512bf16.cvtneps2bf16.512", "64", 32, &[]),
            ("x86.avx512bf16.cvtneps2bf16.256", "32", 16, &["avxneconvert"]),
            ("x86.avx512bf16.mask.cvtneps2bf16.128", "16 16 1", 16, &[]),
            ("x86.avx512bf16.dpbf16ps.128", "16 16 16", 16, &[]),
            ("x86.vcvtneps2bf16256", "32", 16, &[]),
            ("x86.vcvtneps2bf16128", "16", 16, &["avx512bf16"]),
            ("x86.vcvtneoph2ps256", "p", 32, &[]),
            ("x86.vbcstnebf162ps128", "p", 16, &[]),
        ]);
        let expected = "\
x86.avx512.mask.cmp.ps.512: vcmpps k0 {k4}, zmm1, zmm2, {sae}, 1 Mask(2)
x86.avx512.mask.cmp.pd.512: vcmppd k0 {k4}, zmm1, zmm2, 31 Mask(1)
x86.avx512.mask.cmp.ps.512: `llvm.x86.avx512.mask.cmp.ps.512` of predicate 32
x86.avx512.rcp14.ps.512: vmovaps zmm0, zmm2; vrcp14ps zmm0 {k3}, zmm1 Vector(64)
x86.avx512.mask.cvtps2dq.512: vmovaps zmm0, zmm2; vcvtps2dq zmm0 {k3}, zmm1, {ru-sae} Vector(64)
x86.avx512.mask.cvtps2dq.512: `llvm.x86.avx512.mask.cvtps2dq.512` of rounding 12
x86.avx512.mask.cvttpd2udq.512: vmovaps ymm0, ymm2; vcvttpd2udq ymm0 {k3}, zmm1, {sae} Vector(32)
x86.avx512.mask.cvtps2pd.512: vmovaps zmm0, zmm2; vcvtps2pd zmm0 {k3}, ymm1 Vector(64)
x86.avx512.mask.cvtpd2dq.128: vmovaps xmm0, xmm2; vcvtpd2dq xmm0 {k3}, xmm1 Vector(16)
x86.avx512.mask.pmovus.qb.512: vmovaps xmm0, xmm2; vpmovusqb xmm0 {k3}, zmm1 Vector(16)
x86.avx512.mask.scalef.ps.512: vmovaps zmm0, zmm3; vscalefps zmm0 {k4}, zmm1, zmm2, {rd-sae} Vector(64)
x86.avx512.mask.scalef.pd.256: vmovaps ymm0, ymm3; vscalefpd ymm0 {k4}, ymm1, ymm2 Vector(32)
x86.avx512.mask.sqrt.sd: vmovaps xmm0, xmm3; vsqrtsd xmm0 {k4}, xmm1, xmm2, {rz-sae} Vector(16)
x86.avx512.mask.max.ss.round: vmovaps xmm0, xmm3; vmaxss xmm0 {k4}, xmm1, xmm2, {sae} Vector(16)
x86.avx512.rsqrt14.sd: vmovaps xmm0, xmm3; vrsqrt14sd xmm0 {k4}, xmm1, xmm2 Vector(16)
x86.avx512.max.pd.512: vmaxpd zmm0, zmm1, zmm2, {sae} Vector(64)
x86.avx512.psra.q.128: vpsraq xmm0, xmm1, xmm2 Vector(16)
x86.avx512.pmul.hr.sw.512: vpmulhrsw zmm0, zmm1, zmm2 Vector(64)
x86.avx512.conflict.q.256: vpconflictq ymm0, ymm1 Vector(32)
x86.avx512.sitofp.round.v8f64.v8i64: vcvtqq2pd zmm0, zmm1, {rd-sae} Vector(64)
x86.avx512.uitofp.round.v4f32.v4i64: vcvtuqq2ps xmm0, ymm1 Vector(16)
x86.avx512.uitofp.round.v4f32.v4i64: `llvm.x86.avx512.uitofp.round.v4f32.v4i64` of a rounding below 512 bits
x86.avx512.vpdpbusd.256: vmovaps ymm0, ymm1; {vex} vpdpbusd ymm0, ymm2, ymm3 Vector(32)
x86.avx512.vpdpbusd.256: vmovaps ymm0, ymm1; {evex} vpdpbusd ymm0, ymm2, ymm3 Vector(32)
x86.avx512.vpdpwssds.512: vmovaps zmm0, zmm1; vpdpwssds zmm0, zmm2, zmm3 Vector(64)
x86.avx2.vpdpbsuds.128: vmovaps xmm0, xmm1; vpdpbsuds xmm0, xmm2, xmm3 Vector(16)
x86.avx512.vcvtsd2usi64: vcvtsd2usi rax, xmm1, {rn-sae} Int(8)
x86.avx512.cvttss2si: vcvttss2si eax, xmm1, {sae} Int(4)
x86.avx512.mask.pmovs.dw.mem.256: vpmovsdw xmmword ptr [r11] {k3}, ymm2 Nothing
x86.avx512.mask.compress.store.ps.512: vcompressps zmmword ptr [r11] {k3}, zmm2 Nothing
x86.avx512.mask.expand.load.b.128: vmovaps xmm0, xmm2; vpexpandb xmm0 {k3}, xmmword ptr [r11] Vector(16)
x86.avx512.mask.compress.w.512: vmovaps zmm0, zmm2; vpcompressw zmm0 {k3}, zmm1 Vector(64)
x86.avx512.vpshufbitqmb.512: vpshufbitqmb k0, zmm1, zmm2 Mask(8)
x86.avx512.vp2intersect.d.512: vp2intersectd k0, zmm1, zmm2 Masks(2, 2)
x86.avx512.vp2intersect.q.128: vp2intersectq k0, xmm1, xmm2 Masks(1, 1)
x86.avx512.rcp14.ps.512: none
x86.avx512fp16.vcomi.sh: vcmpsh k0, xmm1, xmm2, {sae}, 1; kmovw eax, k0 Int(4)
x86.avx512fp16.vcvtsi642sh: vcvtsi2sh xmm0, xmm1, {rz-sae}, rcx Vector(16)
x86.avx512fp16.vcvtusi2sh: vcvtusi2sh xmm0, xmm1, ecx Vector(16)
x86.avx512fp16.vcvttsh2usi32: vcvttsh2usi eax, xmm1, {sae} Int(4)
x86.avx512fp16.vcvtsh2si64: vcvtsh2si rax, xmm1, {ru-sae} Int(8)
x86.avx512.sitofp.round.v16f16.v16i32: vcvtdq2ph ymm0, zmm1, {ru-sae} Vector(32)
x86.avx512.uitofp.round.v8f16.v8i16: vcvtuw2ph xmm0, xmm1 Vector(16)
x86.avx512.sitofp.round.v4f32.v4i32: none
x86.avx512fp16.mask.vfmadd.cph.512: vmovaps zmm0, zmm3; vfmaddcph zmm0 {k4}, zmm1, zmm2, {rd-sae} Vector(64)
x86.avx512fp16.maskz.vfcmadd.csh: vmovaps xmm0, xmm3; vfcmaddcsh xmm0 {k4} {z}, xmm1, xmm2, {ru-sae} Vector(16)
x86.avx512fp16.mask.vfmul.cph.256: vmovaps ymm0, ymm3; vfmulcph ymm0 {k4}, ymm1, ymm2 Vector(32)
x86.avx512fp16.mask.vfcmul.csh: vmovaps xmm0, xmm3; vfcmulcsh xmm0 {k4}, xmm1, xmm2, {rn-sae} Vector(16)
x86.avx512fp16.max.ph.128: vmaxph xmm0, xmm1, xmm2 Vector(16)
x86.avx512fp16.min.ph.512: vminph zmm0, zmm1, zmm2, {sae} Vector(64)
x86.avx512fp16.mask.vcvtph2w.512: vmovaps zmm0, zmm2; vcvtph2w zmm0 {k3}, zmm1, {rd-sae} Vector(64)
x86.avx512fp16.mask.vcvttph2qq.256: vmovaps ymm0, ymm2; vcvttph2qq ymm0 {k3}, xmm1 Vector(32)
x86.avx512fp16.mask.vcvtpd2ph.512: vmovaps xmm0, xmm2; vcvtpd2ph xmm0 {k3}, zmm1, {rz-sae} Vector(16)
x86.avx512fp16.mask.vcvtph2psx.512: vmovaps zmm0, zmm2; vcvtph2psx zmm0 {k3}, ymm1, {sae} Vector(64)
x86.avx512fp16.mask.rsqrt.ph.256: vmovaps ymm0, ymm2; vrsqrtph ymm0 {k3}, ymm1 Vector(32)
x86.avx512fp16.mask.getexp.sh: vmovaps xmm0, xmm3; vgetexpsh xmm0 {k4}, xmm1, xmm2, {sae} Vector(16)
x86.avx512fp16.mask.sqrt.sh: vmovaps xmm0, xmm3; vsqrtsh xmm0 {k4}, xmm1, xmm2, {rd-sae} Vector(16)
x86.avx512fp16.mask.vcvtss2sh.round: vmovaps xmm0, xmm3; vcvtss2sh xmm0 {k4}, xmm1, xmm2, {rz-sae} Vector(16)
x86.avx512fp16.mask.vcvtsh2sd.round: vmovaps xmm0, xmm3; vcvtsh2sd xmm0 {k4}, xmm1, xmm2, {sae} Vector(16)
x86.avx512fp16.mask.scalef.ph.512: vmovaps zmm0, zmm3; vscalefph zmm0 {k4}, zmm1, zmm2, {ru-sae} Vector(64)
x86.avx512bf16.cvtne2ps2bf16.256: vcvtne2ps2bf16 ymm0, ymm1, ymm2 Vector(32)
x86.avx512bf16.cvtneps2bf16.512: {evex} vcvtneps2bf16 ymm0, zmm1 Vector(32)
x86.avx512bf16.cvtneps2bf16.256: `llvm.x86.avx512bf16.cvtneps2bf16.256` in a function with AVX-NE-CONVERT too
x86.avx512bf16.mask.cvtneps2bf16.128: vmovaps xmm0, xmm2; vcvtneps2bf16 xmm0 {k3}, xmm1 Vector(16)
x86.avx512bf16.dpbf16ps.128: vmovaps xmm0, xmm1; vdpbf16ps xmm0, xmm2, xmm3 Vector(16)
x86.vcvtneps2bf16256: {vex} vcvtneps2bf16 xmm0, ymm1 Vector(16)
x86.vcvtneps2bf16128: `llvm.x86.vcvtneps2bf16128` in a function with AVX512-BF16 too
x86.vcvtneoph2ps256: vcvtneoph2ps ymm0, ymmword ptr [r11] Vector(32)
x86.vbcstnebf162ps128: vbcstnebf162ps xmm0, word ptr [r11] Vector(16)
";
        assert_eq!(described, expected);
    }

    #[test]
    fn half_vectors_by_simd_op() {
        let all = |_: TargetFeature| true;
        let text = |op, from, to, features: &dyn Fn(TargetFeature) -> bool| {
            half_vector(op, from, to, features).map(|it| it.text)
        };
        for (op, mnemonic) in [
            (SimdOp::Add, "vaddph"),
            (SimdOp::Sub, "vsubph"),
            (SimdOp::Mul, "vmulph"),
            (SimdOp::Div, "vdivph"),
        ] {
            assert_eq!(text(op, 16, 16, &all).unwrap(), format!("{mnemonic} xmm0, xmm1, xmm2"));
        }
        assert_eq!(text(SimdOp::Fsqrt, 64, 64, &all).unwrap(), "vsqrtph zmm0, zmm1");
        let f16c = |it: TargetFeature| it == TargetFeature::F16C;
        assert_eq!(text(SimdOp::Cast, 8, 16, &f16c).unwrap(), "vcvtph2ps xmm0, xmm1");
        assert_eq!(text(SimdOp::Cast, 8, 16, &all), None);
        assert_eq!(text(SimdOp::Neg, 16, 16, &all), None);
        assert_eq!(text(SimdOp::Add, 16, 16, &f16c), None);
    }
}
