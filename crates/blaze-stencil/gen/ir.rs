//! Each stencil's LLVM IR, as text (stencil.md, "Stencils' calling convention and pinned
//! registers"): a `preserve_none` function `(ptr frame, i64 acc, <2 x i64> facc)` of one
//! block, in a section of its own, that reads its operands, computes, writes its results and
//! ends in a `musttail` call of `hole.next`.
//!
//! How a hole is made, per instruction set:
//!
//! - x86-64: every number hole is an external global with an `!absolute_symbol` range,
//!   `[0, 2^31)` for slots and `[-2^31, 2^31)` for immediates and offsets, so LLVM folds it
//!   into the instruction that uses it (a `disp32`, an `imm32` sign-extended, an `imm8` where
//!   the operand is a byte) and the relocation it leaves is that field. A 64-bit constant is
//!   inline assembly (`movabsq`), whose field is all 64 bits.
//! - AArch64: LLVM would address any symbol PC-relatively, so every number hole is inline
//!   assembly: `ldr`/`str [x20, #:lo12:hole]` (8 or 16 bytes, so the imm12 reaches 32 KB or
//!   64 KB of slots), and `movz`/`movk` of the 16-bit groups of an immediate: one for a byte
//!   or a halfword, two for a word, four for 64 bits.
//! - Both: a conditional exit (a trap, a branch's target) is inline assembly branching to
//!   the hole's symbol, so the stencil stays one block and its last instruction is the jump
//!   to `Next`, which the patcher drops when the next code follows.

use std::{collections::BTreeMap, fmt::Write};

use crate::{
    format::HoleKind,
    shape::{Acc, Dst, IntCc, Isa, Op, Shape, Src, Ty},
};

/// LLVM's type of a [`Ty`]: `f16` and `f128` are their bits, as stencils only move them.
pub fn ir(ty: Ty) -> &'static str {
    match ty {
        Ty::I8 => "i8",
        Ty::I16 | Ty::F16 => "i16",
        Ty::I32 => "i32",
        Ty::I64 => "i64",
        Ty::I128 | Ty::F128 => "i128",
        Ty::F32 => "float",
        Ty::F64 => "double",
        Ty::I8x16 => "<16 x i8>",
        Ty::I16x8 => "<8 x i16>",
        Ty::I32x4 => "<4 x i32>",
        Ty::I64x2 => "<2 x i64>",
        Ty::F32x4 => "<4 x float>",
        Ty::F64x2 => "<2 x double>",
    }
}

/// The intrinsic name suffix of a [`Ty`] (`i64`, `f32`, `v4i32`).
pub fn mangle(ty: Ty) -> &'static str {
    match ty {
        Ty::I8 => "i8",
        Ty::I16 | Ty::F16 => "i16",
        Ty::I32 => "i32",
        Ty::I64 => "i64",
        Ty::I128 | Ty::F128 => "i128",
        Ty::F32 => "f32",
        Ty::F64 => "f64",
        Ty::I8x16 => "v16i8",
        Ty::I16x8 => "v8i16",
        Ty::I32x4 => "v4i32",
        Ty::I64x2 => "v2i64",
        Ty::F32x4 => "v4f32",
        Ty::F64x2 => "v2f64",
    }
}

/// The integer type of the same bits: what bitwise ops on floats work in.
pub fn bits(ty: Ty) -> Ty {
    match ty {
        Ty::F16 => Ty::I16,
        Ty::F32 => Ty::I32,
        Ty::F64 => Ty::I64,
        Ty::F128 => Ty::I128,
        it => it.int_lanes(),
    }
}

/// Bits of an integer type or of one lane of an integer vector.
pub fn lane_bits(ty: Ty) -> u32 {
    u32::from(ty.lane().bytes()) * 8
}

/// A float constant LLVM reads exactly: the hex of its `f64` bits, which LLVM takes for
/// `float` too when the value is exact in it.
pub fn float(value: f64) -> String {
    format!("0x{:016X}", value.to_bits())
}

/// The vector of `lanes` lanes of `bits`-bit integers.
pub fn vec_of(lanes: u8, bits: u32) -> String {
    format!("<{lanes} x i{bits}>")
}

/// A constant of `ty` (an integer or integer vector) with every lane `value`.
pub fn splat(ty: Ty, value: i128) -> String {
    if ty.is_vector() { format!("splat ({} {value})", ir(ty.lane())) } else { value.to_string() }
}

/// The value of `ty` that is all zero bits.
pub fn zero(ty: Ty) -> &'static str {
    if ty.is_vector() { "zeroinitializer" } else if ty.is_float() { "0.0" } else { "0" }
}

/// One stencil being written: its text, a counter of values, and the intrinsics it calls.
pub struct B<'a> {
    pub isa: Isa,
    out: &'a mut String,
    n: u32,
    decls: &'a mut BTreeMap<String, String>,
}

impl B<'_> {
    pub fn fresh(&mut self) -> String {
        self.n += 1;
        format!("%v{}", self.n)
    }

    /// `%v = <rhs>`; the new value.
    pub fn val(&mut self, rhs: &str) -> String {
        let v = self.fresh();
        let _ = writeln!(self.out, "  {v} = {rhs}");
        v
    }

    pub fn stmt(&mut self, text: &str) {
        let _ = writeln!(self.out, "  {text}");
    }

    /// A call of the intrinsic `name` returning `ret`, declared once per module.
    pub fn call(&mut self, ret: &str, name: &str, args: &[(&str, &str)]) -> String {
        let params = args.iter().map(|(ty, _)| *ty).collect::<Vec<_>>().join(", ");
        self.decls
            .entry(name.to_owned())
            .or_insert_with(|| format!("declare {ret} @{name}({params})"));
        let list = args.iter().map(|(ty, v)| format!("{ty} {v}")).collect::<Vec<_>>().join(", ");
        self.val(&format!("call {ret} @{name}({list})"))
    }

    /// `ptrtoint` of a hole symbol: the number the patcher writes (x86-64's absolute holes).
    pub fn hole_value(&mut self, kind: HoleKind) -> String {
        self.val(&format!("ptrtoint ptr @{} to i64", kind.symbol()))
    }

    /// The frame plus a byte offset.
    pub fn frame_at(&mut self, offset: &str) -> String {
        self.val(&format!("getelementptr i8, ptr %f, i64 {offset}"))
    }

    /// The value in the slot of `kind`, of type `ty`.
    pub fn slot_load(&mut self, kind: HoleKind, ty: Ty) -> String {
        let t = ir(ty);
        let sym = kind.symbol();
        match self.isa {
            Isa::X86_64 => {
                let offset = self.hole_value(kind);
                let at = self.frame_at(&offset);
                self.val(&format!("load {t}, ptr {at}, align {}", ty.slot()))
            }
            Isa::Aarch64 => match ty {
                Ty::F32 | Ty::F64 => self.val(&format!(
                    "call {t} asm \"ldr ${{0:d}}, [${{1:x}}, #:lo12:{sym}]\", \"=w,r,~{{memory}}\"(ptr %f)"
                )),
                _ if ty.bytes() == 16 => {
                    let v = self.val(&format!(
                        "call <2 x i64> asm \"ldr ${{0:q}}, [${{1:x}}, #:lo12:{sym}]\", \"=w,r,~{{memory}}\"(ptr %f)"
                    ));
                    self.cast("bitcast", Ty::I64x2, &v, ty)
                }
                _ => self.val(&format!(
                    "call {t} asm \"ldr ${{0:x}}, [${{1:x}}, #:lo12:{sym}]\", \"=r,r,~{{memory}}\"(ptr %f)"
                )),
            },
        }
    }

    /// Writes `v` (of type `ty`) to the slot of `kind`.
    pub fn slot_store(&mut self, kind: HoleKind, ty: Ty, v: &str) {
        let t = ir(ty);
        let sym = kind.symbol();
        match self.isa {
            Isa::X86_64 => {
                let offset = self.hole_value(kind);
                let at = self.frame_at(&offset);
                self.stmt(&format!("store {t} {v}, ptr {at}, align {}", ty.slot()));
            }
            Isa::Aarch64 => match ty {
                Ty::F32 | Ty::F64 => self.stmt(&format!(
                    "call void asm sideeffect \"str ${{0:d}}, [${{1:x}}, #:lo12:{sym}]\", \"w,r,~{{memory}}\"({t} {v}, ptr %f)"
                )),
                _ if ty.bytes() == 16 => {
                    let w = self.cast("bitcast", ty, v, Ty::I64x2);
                    self.stmt(&format!(
                        "call void asm sideeffect \"str ${{0:q}}, [${{1:x}}, #:lo12:{sym}]\", \"w,r,~{{memory}}\"(<2 x i64> {w}, ptr %f)"
                    ));
                }
                _ => self.stmt(&format!(
                    "call void asm sideeffect \"str ${{0:x}}, [${{1:x}}, #:lo12:{sym}]\", \"r,r,~{{memory}}\"({t} {v}, ptr %f)"
                )),
            },
        }
    }

    /// The immediate of `kind`, an integer of `ty` (up to 64 bits): its value sign-extended
    /// from 32 bits, or all 64 when `wide`. The patcher writes the constant sign-extended
    /// from `ty`'s width, or its zero-extension where a field of a narrower `ty` refuses that.
    pub fn imm(&mut self, kind: HoleKind, ty: Ty, wide: bool) -> String {
        let sym = kind.symbol();
        match self.isa {
            Isa::X86_64 if wide => {
                self.val(&format!("call i64 asm \"movabsq $${sym}, $0\", \"=r\"()"))
            }
            // Folded into a 16-bit instruction it would be an `R_X86_64_16`, which no form is.
            Isa::X86_64 if ty.bytes() == 2 => {
                let v = self.val(&format!("call i32 asm \"movl $${sym}, $0\", \"=r\"()"));
                self.cast("trunc", Ty::I32, &v, ty)
            }
            Isa::X86_64 => {
                let v = self.hole_value(kind);
                self.cast("trunc", Ty::I64, &v, ty)
            }
            Isa::Aarch64 => match ty.bytes() {
                1 | 2 => {
                    let v = self.val(&format!("call i32 asm \"movz ${{0:w}}, #:abs_g0:{sym}\", \"=r\"()"));
                    self.cast("trunc", Ty::I32, &v, ty)
                }
                4 => self.val(&format!(
                    "call i32 asm \"movz ${{0:w}}, #:abs_g1:{sym}\\0Amovk ${{0:w}}, #:abs_g0_nc:{sym}\", \"=r\"()"
                )),
                _ => self.val(&format!(
                    "call i64 asm \"movz ${{0:x}}, #:abs_g3:{sym}\\0Amovk ${{0:x}}, #:abs_g2_nc:{sym}\\0Amovk ${{0:x}}, #:abs_g1_nc:{sym}\\0Amovk ${{0:x}}, #:abs_g0_nc:{sym}\", \"=r\"()"
                )),
            },
        }
    }

    /// A byte offset (the `Offset` hole), any `i32` or frame offset, as an `i64`.
    pub fn offset(&mut self) -> String {
        self.imm(HoleKind::Offset, Ty::I64, false)
    }

    /// The `i64` `v` plus the low 12 bits of the hole `kind`: AArch64's `add #:lo12:`.
    pub fn add_lo12(&mut self, kind: HoleKind, v: &str) -> String {
        assert_eq!(self.isa, Isa::Aarch64, "an `add #:lo12:` on {:?}", self.isa);
        let sym = kind.symbol();
        self.val(&format!("call i64 asm \"add ${{0:x}}, ${{1:x}}, #:lo12:{sym}\", \"=r,r\"(i64 {v})"))
    }

    /// `<op> <from> v to <to>`, or `v` when the types are LLVM's same type.
    pub fn cast(&mut self, op: &str, from: Ty, v: &str, to: Ty) -> String {
        if ir(from) == ir(to) {
            return v.to_owned();
        }
        self.val(&format!("{op} {} {v} to {}", ir(from), ir(to)))
    }

    /// The accumulator's value as a value of `ty`.
    pub fn acc_read(&mut self, ty: Ty) -> String {
        match ty.acc() {
            Some(Acc::Int) => self.cast("trunc", Ty::I64, "%acc", ty),
            Some(Acc::Float) if ty.is_vector() => self.cast("bitcast", Ty::I64x2, "%facc", ty),
            Some(Acc::Float) => {
                let lanes = if ty == Ty::F32 { Ty::F32x4 } else { Ty::F64x2 };
                let v = self.cast("bitcast", Ty::I64x2, "%facc", lanes);
                self.val(&format!("extractelement {} {v}, i64 0", ir(lanes)))
            }
            None => unreachable!("{} has no accumulator", ty.name()),
        }
    }

    /// `v` (of `ty`) as the accumulator's `i64` or `<2 x i64>`.
    pub fn acc_write(&mut self, ty: Ty, v: &str) -> String {
        match ty.acc() {
            Some(Acc::Int) => self.cast("zext", ty, v, Ty::I64),
            Some(Acc::Float) if ty.is_vector() => self.cast("bitcast", ty, v, Ty::I64x2),
            Some(Acc::Float) => {
                let lanes = if ty == Ty::F32 { Ty::F32x4 } else { Ty::F64x2 };
                let w = self.val(&format!("insertelement {} poison, {} {v}, i64 0", ir(lanes), ir(ty)));
                self.cast("bitcast", lanes, &w, Ty::I64x2)
            }
            None => unreachable!("{} has no accumulator", ty.name()),
        }
    }

    /// Operand `n` of `shape`, from where its source says.
    pub fn arg(&mut self, shape: Shape, n: usize) -> String {
        let ty = shape.arg(n);
        match shape.srcs[n] {
            Src::Slot => self.slot_load(HoleKind::slot(n), ty),
            Src::Acc => self.acc_read(ty),
            Src::Imm => self.imm(HoleKind::imm(n), ty, false),
            Src::None => unreachable!("{shape} has no operand {n}"),
        }
    }

    /// Leaves for the hole `kind` (a trap stub, the branch's target) when `cond` (an `i1`)
    /// holds; goes on otherwise.
    pub fn exit_if(&mut self, kind: HoleKind, cond: &str) {
        let sym = kind.symbol();
        match self.isa {
            Isa::X86_64 => {
                let c = self.val(&format!("zext i1 {cond} to i8"));
                self.stmt(&format!(
                    "call void asm sideeffect \"testb $0, $0\\0Ajne {sym}\", \"r,~{{dirflag}},~{{fpsr}},~{{flags}},~{{memory}}\"(i8 {c})"
                ));
            }
            Isa::Aarch64 => {
                let c = self.val(&format!("zext i1 {cond} to i32"));
                self.stmt(&format!(
                    "call void asm sideeffect \"cbnz ${{0:w}}, {sym}\", \"r,~{{memory}}\"(i32 {c})"
                ));
            }
        }
    }

    /// Leaves for `kind` when the integer `v` of `ty` is zero (`zero`) or not.
    pub fn exit_test(&mut self, kind: HoleKind, ty: Ty, v: &str, zero: bool) {
        let sym = kind.symbol();
        let (v, ty) = if ty == Ty::I128 {
            let lo = self.val(&format!("trunc i128 {v} to i64"));
            let high = self.val(&format!("lshr i128 {v}, 64"));
            let hi = self.val(&format!("trunc i128 {high} to i64"));
            (self.val(&format!("or i64 {lo}, {hi}")), Ty::I64)
        } else {
            (v.to_owned(), ty)
        };
        match self.isa {
            Isa::X86_64 => {
                let jump = if zero { "je" } else { "jne" };
                self.stmt(&format!(
                    "call void asm sideeffect \"test $0, $0\\0A{jump} {sym}\", \"r,~{{dirflag}},~{{fpsr}},~{{flags}},~{{memory}}\"({} {v})",
                    ir(ty)
                ));
            }
            Isa::Aarch64 => {
                let branch = if zero { "cbz" } else { "cbnz" };
                let (v, reg, t) = match ty {
                    Ty::I64 => (v, "x", "i64"),
                    Ty::I32 => (v, "w", "i32"),
                    _ => (self.cast("zext", ty, &v, Ty::I32), "w", "i32"),
                };
                self.stmt(&format!(
                    "call void asm sideeffect \"{branch} ${{0:{reg}}}, {sym}\", \"r,~{{memory}}\"({t} {v})"
                ));
            }
        }
    }

    /// Leaves for `kind` when `a cc b` (integers of `ty`): one compare and one branch.
    pub fn exit_cmp(&mut self, kind: HoleKind, cc: IntCc, ty: Ty, a: &str, b: &str) {
        let sym = kind.symbol();
        if ty == Ty::I128 {
            let c = self.val(&format!("icmp {} i128 {a}, {b}", icmp(cc)));
            return self.exit_if(kind, &c);
        }
        match self.isa {
            Isa::X86_64 => {
                let jump = match cc {
                    IntCc::Equal => "je",
                    IntCc::NotEqual => "jne",
                    IntCc::SignedLessThan => "jl",
                    IntCc::SignedGreaterThanOrEqual => "jge",
                    IntCc::SignedGreaterThan => "jg",
                    IntCc::SignedLessThanOrEqual => "jle",
                    IntCc::UnsignedLessThan => "jb",
                    IntCc::UnsignedGreaterThanOrEqual => "jae",
                    IntCc::UnsignedGreaterThan => "ja",
                    IntCc::UnsignedLessThanOrEqual => "jbe",
                };
                let t = ir(ty);
                self.stmt(&format!(
                    "call void asm sideeffect \"cmp $1, $0\\0A{jump} {sym}\", \"r,r,~{{dirflag}},~{{fpsr}},~{{flags}},~{{memory}}\"({t} {a}, {t} {b})"
                ));
            }
            Isa::Aarch64 => {
                let cond = match cc {
                    IntCc::Equal => "eq",
                    IntCc::NotEqual => "ne",
                    IntCc::SignedLessThan => "lt",
                    IntCc::SignedGreaterThanOrEqual => "ge",
                    IntCc::SignedGreaterThan => "gt",
                    IntCc::SignedLessThanOrEqual => "le",
                    IntCc::UnsignedLessThan => "lo",
                    IntCc::UnsignedGreaterThanOrEqual => "hs",
                    IntCc::UnsignedGreaterThan => "hi",
                    IntCc::UnsignedLessThanOrEqual => "ls",
                };
                let (a, b, reg, t) = match ty {
                    Ty::I64 => (a.to_owned(), b.to_owned(), "x", "i64"),
                    Ty::I32 => (a.to_owned(), b.to_owned(), "w", "i32"),
                    _ => {
                        let ext = if cc.signed() { "sext" } else { "zext" };
                        (self.cast(ext, ty, a, Ty::I32), self.cast(ext, ty, b, Ty::I32), "w", "i32")
                    }
                };
                self.stmt(&format!(
                    "call void asm sideeffect \"cmp ${{0:{reg}}}, ${{1:{reg}}}\\0Ab.{cond} {sym}\", \"r,r,~{{cc}},~{{memory}}\"({t} {a}, {t} {b})"
                ));
            }
        }
    }
}

/// LLVM's `icmp` predicate of a condition.
pub fn icmp(cc: IntCc) -> &'static str {
    match cc {
        IntCc::Equal => "eq",
        IntCc::NotEqual => "ne",
        IntCc::SignedLessThan => "slt",
        IntCc::SignedGreaterThanOrEqual => "sge",
        IntCc::SignedGreaterThan => "sgt",
        IntCc::SignedLessThanOrEqual => "sle",
        IntCc::UnsignedLessThan => "ult",
        IntCc::UnsignedGreaterThanOrEqual => "uge",
        IntCc::UnsignedGreaterThan => "ugt",
        IntCc::UnsignedLessThanOrEqual => "ule",
    }
}

/// The symbol and section of stencil `index`.
pub fn name(index: u32) -> String {
    format!("s{index}")
}

/// The function of stencil `index` (`shape`), appended to `out`.
fn stencil(out: &mut String, decls: &mut BTreeMap<String, String>, isa: Isa, index: u32, shape: Shape) {
    let attrs = if isa == Isa::X86_64 && shape.op == Op::AtomicCas && shape.ty == Ty::I128 {
        "#1"
    } else {
        "#0"
    };
    let name = name(index);
    let _ = writeln!(out, "; {shape}");
    let _ = writeln!(
        out,
        "define preserve_nonecc void @{name}(ptr %f, i64 %acc, <2 x i64> %facc) {attrs} section \".text.{name}\" {{"
    );
    let mut b = B { isa, out, n: 0, decls };
    let args = usize::from(shape.op.info().args);
    let args: Vec<String> = (0..args).map(|n| b.arg(shape, n)).collect();
    let results = crate::generate::ops::body(&mut b, shape, &args);
    assert_eq!(
        results.len(),
        usize::from(shape.op.info().results),
        "the generator gave {shape} the wrong number of results"
    );
    let (mut acc, mut facc) = (None, None);
    if let Some(first) = results.first() {
        let ty = shape.result(0);
        if matches!(shape.dst, Dst::Slot | Dst::Both) {
            b.slot_store(HoleKind::Result0, ty, first);
        }
        if matches!(shape.dst, Dst::Acc | Dst::Both) {
            let v = b.acc_write(ty, first);
            match ty.acc() {
                Some(Acc::Int) => acc = Some(v),
                _ => facc = Some(v),
            }
        }
    }
    if let Some(second) = results.get(1) {
        b.slot_store(HoleKind::Result1, shape.result(1), second);
    }
    next(&mut b, acc.as_deref(), facc.as_deref());
    let _ = writeln!(b.out, "}}");
}

/// The jump to the continuation, passing the frame and the accumulators.
fn next(b: &mut B<'_>, acc: Option<&str>, facc: Option<&str>) {
    let acc = acc.unwrap_or("poison");
    let facc = facc.unwrap_or("poison");
    b.stmt(&format!(
        "musttail call preserve_nonecc void @{}(ptr %f, i64 {acc}, <2 x i64> {facc})",
        HoleKind::Next.symbol()
    ));
    b.stmt("ret void");
}

/// The probe's symbol and section.
pub const PROBE: &str = "probe";

/// The probe: it stores `frame` to `Result0`, `acc` to `Result1` and `facc` to `Slot0`, which
/// shows the registers LLVM passes them in.
fn probe(out: &mut String, decls: &mut BTreeMap<String, String>, isa: Isa) {
    let _ = writeln!(
        out,
        "define preserve_nonecc void @{PROBE}(ptr %f, i64 %acc, <2 x i64> %facc) #0 section \".text.{PROBE}\" {{"
    );
    let mut b = B { isa, out, n: 0, decls };
    let frame = b.val("ptrtoint ptr %f to i64");
    b.slot_store(HoleKind::Result0, Ty::I64, &frame);
    b.slot_store(HoleKind::Result1, Ty::I64, "%acc");
    b.slot_store(HoleKind::Slot0, Ty::I64x2, "%facc");
    next(&mut b, None, None);
    let _ = writeln!(b.out, "}}");
}

/// The module of `stencils` (each with its index in the variant's order) and the probe.
pub fn module(isa: Isa, stencils: &[(u32, Shape)]) -> String {
    let mut out = String::with_capacity(stencils.len() * 1024 + 4096);
    let mut decls = BTreeMap::new();
    if isa == Isa::X86_64 {
        for kind in [HoleKind::Slot0, HoleKind::Slot1, HoleKind::Slot2, HoleKind::Result0, HoleKind::Result1] {
            let _ = writeln!(out, "@{} = external hidden global i8, !absolute_symbol !0", kind.symbol());
        }
        for kind in [HoleKind::Imm0, HoleKind::Imm1, HoleKind::Imm2, HoleKind::Offset] {
            let _ = writeln!(out, "@{} = external hidden global i8, !absolute_symbol !1", kind.symbol());
        }
    }
    let _ = writeln!(out, "@{} = external hidden global [16 x i8], align 16", HoleKind::Const.symbol());
    let _ = writeln!(
        out,
        "declare hidden preserve_nonecc void @{}(ptr, i64, <2 x i64>)\n",
        HoleKind::Next.symbol()
    );
    probe(&mut out, &mut decls, isa);
    for (index, shape) in stencils {
        stencil(&mut out, &mut decls, isa, *index, *shape);
    }
    out.push('\n');
    for decl in decls.values() {
        out.push_str(decl);
        out.push('\n');
    }
    out.push_str("\nattributes #0 = { nounwind \"frame-pointer\"=\"none\" }\n");
    out.push_str(
        "attributes #1 = { nounwind \"frame-pointer\"=\"none\" \"target-cpu\"=\"x86-64\" \"target-features\"=\"+cx16\" }\n",
    );
    out.push_str("!0 = !{i64 0, i64 2147483648}\n");
    out.push_str("!1 = !{i64 -2147483648, i64 2147483648}\n");
    out
}
