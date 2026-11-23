//! The baseline tier's tests: each instruction form against the assembler's word, then
//! functions emitted, run as machine code in this process and compared with the reference
//! evaluator (`blaze_lir::eval`) on the same arguments, hand-built and random.

use std::alloc::Global;

use blaze_lir::{
    Block, Builder, Flags, Full, IntCond, Op, Ty, Value, View,
    builder::{MemAttrs, Param, Variadic},
    eval::{self, Outcome, Val},
    func::{BlockKind, Conv, Ext, Func, RefKind, SlotKind},
};

use crate::{
    Code, EmitError, Isa, Os, Via, emit,
    enc::{self, Cond, Size},
};

/// The host OS the tests emit and run on (`Os::MacOs` on macOS, `Os::Linux` on Linux).
pub(crate) const HOST_OS: Os = if cfg!(target_os = "macos") { Os::MacOs } else { Os::Linux };

/// What the tests emit for: this host (Armv8.1+ with FEAT_LSE).
pub(crate) const ISA: Isa = Isa { lse: true, os: HOST_OS };

/// The forms, each against the word `clang -c -arch arm64` assembles for it (the text in the
/// comment), and the patched fields against the assembler's PC-relative words.
#[test]
fn encodings() {
    let cases: &[(u32, u32, &str)] = &[
        (enc::movz(9, 0x1234, 1), 0xd2a2_4689, "movz x9, #0x1234, lsl #16"),
        (enc::movk(10, 0xbeef, 3), 0xf2f7_ddea, "movk x10, #0xbeef, lsl #48"),
        (enc::movn(11, 5, 0), 0x9280_00ab, "movn x11, #5"),
        (enc::movz_w(0, 42), 0x5280_0540, "mov w0, #42"),
        (enc::add(9, 9, 10), 0x8b0a_0129, "add x9, x9, x10"),
        (enc::sub(9, 9, 10), 0xcb0a_0129, "sub x9, x9, x10"),
        (enc::add_imm(9, 31, 0x123, false), 0x9104_8fe9, "add x9, sp, #0x123"),
        (enc::add_imm(12, 31, 5, true), 0x9140_17ec, "add x12, sp, #5, lsl #12"),
        (enc::add_imm(12, 12, 0x10, false), 0x9100_418c, "add x12, x12, #0x10"),
        (enc::sub_imm(31, 31, 0x10, false), 0xd100_43ff, "sub sp, sp, #0x10"),
        (enc::sub_imm(31, 31, 2, true), 0xd140_0bff, "sub sp, sp, #2, lsl #12"),
        (enc::cmp_imm(9, 7), 0xf100_1d3f, "cmp x9, #7"),
        (enc::cmn_imm(10, 1), 0xb100_055f, "cmn x10, #1"),
        (enc::cmp(9, 10), 0xeb0a_013f, "cmp x9, x10"),
        (enc::adcs(9, 9, 10), 0xba0a_0129, "adcs x9, x9, x10"),
        (enc::mul(9, 9, 10), 0x9b0a_7d29, "mul x9, x9, x10"),
        (enc::sdiv(11, 9, 10), 0x9aca_0d2b, "sdiv x11, x9, x10"),
        (enc::udiv(11, 9, 10), 0x9aca_092b, "udiv x11, x9, x10"),
        (enc::msub(9, 11, 10, 9), 0x9b0a_a569, "msub x9, x11, x10, x9"),
        (enc::and(9, 9, 10), 0x8a0a_0129, "and x9, x9, x10"),
        (enc::orr(9, 9, 10), 0xaa0a_0129, "orr x9, x9, x10"),
        (enc::eor(9, 9, 10), 0xca0a_0129, "eor x9, x9, x10"),
        (enc::lslv(9, 9, 10), 0x9aca_2129, "lsl x9, x9, x10"),
        (enc::lsrv(9, 9, 10), 0x9aca_2529, "lsr x9, x9, x10"),
        (enc::asrv(9, 9, 10), 0x9aca_2929, "asr x9, x9, x10"),
        (enc::rorv(9, 9, 10), 0x9aca_2d29, "ror x9, x9, x10"),
        (enc::mvn(9, 10), 0xaa2a_03e9, "mvn x9, x10"),
        (enc::neg(9, 10), 0xcb0a_03e9, "neg x9, x10"),
        (enc::mov(9, 0), 0xaa00_03e9, "mov x9, x0"),
        (enc::cset(9, Cond::Lt), 0x9a9f_a7e9, "cset x9, lt"),
        (enc::csel(9, 10, 11, Cond::Ne), 0x9a8b_1149, "csel x9, x10, x11, ne"),
        (enc::sxtb(9, 9), 0x9340_1d29, "sxtb x9, w9"),
        (enc::sxth(9, 9), 0x9340_3d29, "sxth x9, w9"),
        (enc::sxtw(9, 9), 0x9340_7d29, "sxtw x9, w9"),
        (enc::uxtb(9, 9), 0xd340_1d29, "ubfx x9, x9, #0, #8"),
        (enc::uxth(9, 9), 0xd340_3d29, "ubfx x9, x9, #0, #16"),
        (enc::zext(9, 9, 32), 0xd340_7d29, "ubfx x9, x9, #0, #32"),
        (enc::zext(10, 10, 1), 0xd340_014a, "ubfx x10, x10, #0, #1"),
        (enc::sext(9, 9, 1), 0x9340_0129, "sbfx x9, x9, #0, #1"),
        (enc::lsr_imm(16, 16, 6), 0xd346_fe10, "lsr x16, x16, #6"),
        (enc::lsl_imm(16, 16, 6), 0xd37a_e610, "lsl x16, x16, #6"),
        (enc::ldr(Size::B, 9, 10, 3), 0x3940_0d49, "ldrb w9, [x10, #3]"),
        (enc::ldr(Size::H, 9, 10, 3), 0x7940_0d49, "ldrh w9, [x10, #6]"),
        (enc::ldr(Size::W, 9, 10, 3), 0xb940_0d49, "ldr w9, [x10, #12]"),
        (enc::ldr(Size::X, 9, 31, 511), 0xf947_ffe9, "ldr x9, [sp, #4088]"),
        (enc::str(Size::B, 9, 10, 3), 0x3900_0d49, "strb w9, [x10, #3]"),
        (enc::str(Size::H, 9, 10, 3), 0x7900_0d49, "strh w9, [x10, #6]"),
        (enc::str(Size::W, 9, 10, 3), 0xb900_0d49, "str w9, [x10, #12]"),
        (enc::str(Size::X, 9, 31, 2), 0xf900_0be9, "str x9, [sp, #16]"),
        (enc::str(Size::X, 31, 31, 4), 0xf900_13ff, "str xzr, [sp, #32]"),
        (enc::ldur(Size::B, 9, 10, -1), 0x385f_f149, "ldurb w9, [x10, #-1]"),
        (enc::ldur(Size::H, 9, 10, -2), 0x785f_e149, "ldurh w9, [x10, #-2]"),
        (enc::ldur(Size::W, 9, 10, -4), 0xb85f_c149, "ldur w9, [x10, #-4]"),
        (enc::ldur(Size::X, 9, 10, -8), 0xf85f_8149, "ldur x9, [x10, #-8]"),
        (enc::stur(Size::B, 9, 10, -1), 0x381f_f149, "sturb w9, [x10, #-1]"),
        (enc::stur(Size::H, 9, 10, -2), 0x781f_e149, "sturh w9, [x10, #-2]"),
        (enc::stur(Size::W, 9, 10, -4), 0xb81f_c149, "stur w9, [x10, #-4]"),
        (enc::stur(Size::X, 9, 10, -8), 0xf81f_8149, "stur x9, [x10, #-8]"),
        (enc::ldr_reg(Size::B, 9, 10, 11), 0x386b_6949, "ldrb w9, [x10, x11]"),
        (enc::ldr_reg(Size::X, 9, 10, 11), 0xf86b_6949, "ldr x9, [x10, x11]"),
        (enc::str_reg(Size::H, 9, 10, 11), 0x782b_6949, "strh w9, [x10, x11]"),
        (enc::str_reg(Size::X, 9, 10, 11), 0xf82b_6949, "str x9, [x10, x11]"),
        (enc::fp_access(enc::ldr(Size::X, 9, 10, 3)), 0xfd40_0d49, "ldr d9, [x10, #24]"),
        (enc::fp_access(enc::ldr(Size::W, 9, 10, 3)), 0xbd40_0d49, "ldr s9, [x10, #12]"),
        (enc::fp_access(enc::str(Size::W, 9, 10, 3)), 0xbd00_0d49, "str s9, [x10, #12]"),
        (enc::fp_access(enc::ldur(Size::X, 9, 10, -8)), 0xfc5f_8149, "ldur d9, [x10, #-8]"),
        (enc::fp_access(enc::stur(Size::W, 9, 10, -4)), 0xbc1f_c149, "stur s9, [x10, #-4]"),
        (enc::fp_access(enc::ldr_reg(Size::X, 9, 10, 11)), 0xfc6b_6949, "ldr d9, [x10, x11]"),
        (enc::fp_access(enc::str_reg(Size::W, 9, 10, 11)), 0xbc2b_6949, "str s9, [x10, x11]"),
        (enc::stp_pre(29, 30, 31, -16), 0xa9bf_7bfd, "stp x29, x30, [sp, #-16]!"),
        (enc::ldp_post(29, 30, 31, 16), 0xa8c1_7bfd, "ldp x29, x30, [sp], #16"),
        (enc::add_imm(29, 31, 0, false), 0x9100_03fd, "mov x29, sp"),
        (enc::add_imm(31, 29, 0, false), 0x9100_03bf, "mov sp, x29"),
        (enc::ldr_lit(16, 4), 0x5800_0090, "ldr x16, . + 16"),
        (enc::adr(9, 20), 0x1000_00a9, "adr x9, . + 20"),
        (enc::b(2), 0x1400_0002, "b . + 8"),
        (enc::b_cond(Cond::Ne, -2), 0x54ff_ffc1, "b.ne . - 8"),
        (enc::cbz(9, 3), 0xb400_0069, "cbz x9, . + 12"),
        (enc::cbnz(9, -1), 0xb5ff_ffe9, "cbnz x9, . - 4"),
        (enc::br(16), 0xd61f_0200, "br x16"),
        (enc::blr(16), 0xd63f_0200, "blr x16"),
        (enc::ret(), 0xd65f_03c0, "ret"),
        (enc::brk(0xff00), 0xd43f_e000, "brk #0xff00"),
    ];
    for &(got, want, text) in cases {
        assert_eq!(got, want, "{text}: {got:#010x}, the assembler's is {want:#010x}");
    }
    let patched: &[(enc::Field, u32, i64, u32)] = &[
        (enc::Field::Imm26, enc::b(0), 8, 0x1400_0002),
        (enc::Field::Imm19, enc::b_cond(Cond::Ne, 0), -8, 0x54ff_ffc1),
        (enc::Field::Imm19, enc::cbnz(9, 0), -4, 0xb5ff_ffe9),
        (enc::Field::Imm19, enc::ldr_lit(16, 0), 16, 0x5800_0090),
        (enc::Field::Adr, enc::adr(9, 0), 20, 0x1000_00a9),
        (enc::Field::Adr, enc::adr(9, 0), -3, enc::adr(9, -3)),
    ];
    for &(field, word, bytes, want) in patched {
        assert_eq!(field.patch(word, bytes), Some(want), "{field:?} by {bytes}");
    }
    assert_eq!(enc::Field::Imm19.patch(enc::cbz(9, 0), 1 << 20), None, "past ±1 MiB");
    assert_eq!(enc::Field::Imm26.patch(enc::b(0), 6), None, "not a whole word");
}

/// Emitting refuses what is outside the subset, naming the instruction.
#[test]
fn refuses_what_it_does_not_emit() {
    // `i128` division and `frem` are not refused: each is a call of the runtime's routine
    // (pass/runtime.rs), named by the one literal word placement fills.
    let calls = |code: &Code<Global>, r: crate::Routine| code.relocs.iter().any(|it| it.to == crate::RelocTo::Routine(r));
    let wide = one(&[Ty::I128, Ty::I128], Ty::I128, |b, v| b.binary(Op::Udiv, Flags::NONE, v[0], v[1]));
    let code = emitted(wide.view(), &[]).unwrap_or_else(|e| panic!("{e}"));
    assert!(calls(&code, crate::Routine::Udivti3), "`udiv` at `i128` calls `__udivti3`");
    let float = one(&[Ty::F64, Ty::F64], Ty::F64, |b, v| b.binary(Op::Frem, Flags::NONE, v[0], v[1]));
    let code = emitted(float.view(), &[]).unwrap_or_else(|e| panic!("{e}"));
    assert!(calls(&code, crate::Routine::Fmod), "`frem` at `f64` calls `fmod`");

    // `fminnum` is emitted since INTRINSICS P7b (pass/float/math.rs: one `fminnm`). Judge-changed
    // 2026-11-16 by the builder integrator: it was "still refused by name" before P7b-2.
    let min = one(&[Ty::F64, Ty::F64], Ty::F64, |b, v| b.binary(Op::FminNum, Flags::NONE, v[0], v[1]));
    let emitted_min = emitted(min.view(), &[]);
    assert!(emitted_min.is_ok(), "{:?}", emitted_min.err());

    let (caller, _) = caller_of(Ty::I32);
    assert_eq!(emitted(caller.view(), &[]).err(), Some(EmitError::NoVia { place: 0 }));
    assert_eq!(func_addr(Via::Slot), None, "a slot function's address is its slot stub's");
}

fn func_addr(via: Via) -> Option<EmitError> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::PTR)]);
    b.set_signature(sig);
    let callee = b.reference(RefKind::Func, 0);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let at = b.func_addr(callee);
    b.ret(&[at]);
    let func = b.finish_checked().expect("a hand-built function builds");
    emitted(func.view(), &[via]).err()
}

// Running emitted code.

/// Instructions an evaluated run may take.
pub(crate) const FUEL: u64 = 1_000_000;

pub(crate) fn emitted(view: View<'_>, via: &[Via]) -> Result<Code<Global>, EmitError> {
    emitted_for(view, via, ISA)
}

pub(crate) fn emitted_for(view: View<'_>, via: &[Via], isa: Isa) -> Result<Code<Global>, EmitError> {
    let mut code = Code::new_in(Global);
    emit(blaze_lir::verify(view).expect("the test's function is LIR"), via, isa, &mut code)?;
    // Every function the tests emit is also built as a code record and checked: no literal
    // word left, a round trip, equal bytes twice (record_tests.rs).
    crate::record_tests::holds(view, isa);
    Ok(code)
}

/// `f(params) = body(params)`, returning the one value `body` gives, of type `ret`.
pub(crate) trait IntoRet {
    fn into_ret(self) -> Param;
}

impl IntoRet for Ty {
    fn into_ret(self) -> Param {
        Param::of(self)
    }
}

impl IntoRet for Param {
    fn into_ret(self) -> Param {
        self
    }
}

pub(crate) fn one(
    params: &[Ty],
    ret: impl IntoRet,
    body: impl FnOnce(&mut Builder<Global, Full>, &[Value]) -> Value,
) -> Func<Global> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig_params: Vec<Param> = params.iter().map(|&ty| Param::of(ty)).collect();
    let sig = b.sig(Conv::C, Variadic::No, &sig_params, &[ret.into_ret()]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let values: Vec<Value> = params.iter().map(|&ty| b.param(ty)).collect();
    let value = body(&mut b, &values);
    b.ret(&[value]);
    b.finish_checked().expect("a hand-built function builds")
}

pub(crate) const fn mask(bits: u32) -> u64 {
    match bits {
        64.. => u64::MAX,
        bits => (1 << bits) - 1,
    }
}

#[cfg(target_arch = "aarch64")]
pub(crate) mod run {
    use std::alloc::Global;

    use blaze_inproc::exec::Executable;

    use crate::{Code, Fixup, RelocTo};

    /// `code` placed, each literal word filled by `fill`.
    pub(crate) fn place(code: &Code<Global>, fill: impl Fn(Fixup) -> u64) -> Executable {
        place_all(code, fill, |to| panic!("an unexpected relocation {to:?}"))
    }

    /// `code` placed, each fixup's word filled by `fill`, each relocation's by `reloc` (but
    /// `Code`, its own bytes) plus its addend.
    pub(crate) fn place_all(
        code: &Code<Global>,
        fill: impl Fn(Fixup) -> u64,
        reloc: impl Fn(RelocTo) -> u64,
    ) -> Executable {
        Executable::written(code.bytes.len(), 0, |out, _| {
            let base = out.as_ptr() as u64;
            out.copy_from_slice(&code.bytes);
            for &fixup in code.fixups.iter() {
                let at = fixup.at as usize;
                out[at..at + 8].copy_from_slice(&fill(fixup).to_le_bytes());
            }
            for r in code.relocs.iter() {
                let to = match r.to {
                    RelocTo::Code(off) => base + u64::from(off),
                    other => reloc(other),
                };
                let at = r.at as usize;
                out[at..at + 8].copy_from_slice(&to.wrapping_add_signed(r.addend).to_le_bytes());
            }
        })
        .map(|exec| {
            // Placement honours what the code asks (lib.rs `Code::align`): the mapping starts
            // on a page, which is at least the most a code asks (4 KiB).
            let align = 1u64 << code.align;
            assert_eq!(exec.code_address() % align, 0, "the code placed off the 2^{} it asks", code.align);
            exec
        })
        .expect("the code maps")
    }

    /// Calls the function at `exec`'s first byte with four integer arguments (a function of
    /// fewer parameters ignores the rest: AAPCS64 passes each in its own register).
    pub(crate) fn call(exec: &Executable, args: [u64; 4]) -> u64 {
        // SAFETY: every caller's code is an emitted function of the C convention whose
        // parameters are at most four integers and whose return is at most one integer,
        // mapped executable for as long as `exec` lives; its literal words were filled with
        // addresses of live functions or slot words.
        let f = unsafe {
            std::mem::transmute::<usize, extern "C" fn(u64, u64, u64, u64) -> u64>(exec.code_address() as usize)
        };
        f(args[0], args[1], args[2], args[3])
    }
}

/// `func` run as machine code and in the evaluator on `args` (each its parameter's bits): the
/// machine's return (of width `ret_bits`) and the evaluator's, or `None` where the evaluator
/// traps or calls the run undefined. `garbage` goes in the arguments' bits above their widths.
#[cfg(target_arch = "aarch64")]
pub(crate) fn both(func: &Func<Global>, args: &[u128], garbage: u64) -> Option<(u64, u64)> {
    let view = func.view();
    let vals: Vec<Val> = args.iter().map(|&it| Val::Int(it)).collect();
    let reference = eval::run_in(view, &vals, FUEL, Global).expect("the evaluator runs it");
    let want = match reference {
        Outcome::Returned(values) => match values.first() {
            Some(Val::Int(it)) => *it as u64,
            other => panic!("a return the tests do not compare: {other:?}"),
        },
        Outcome::Trapped(_) | Outcome::Undefined { .. } | Outcome::OutOfFuel => return None,
    };
    let code = emitted(view, &[]).unwrap_or_else(|why| panic!("{why}\n{}", blaze_lir::print(view)));
    let exec = run::place(&code, |_| unreachable!("no literal words"));
    let entry = view.params(Block(0));
    let mut machine = [0u64; 4];
    for (at, (inst, &arg)) in entry.zip(args).enumerate() {
        let bits = view.ty(Value(inst as u32)).bits(64);
        machine[at] = arg as u64 | (garbage & !mask(bits));
    }
    let ret_bits = view.sig(blaze_lir::func::SigId(view.head.sig)).rets[0].ty;
    let got = run::call(&exec, machine) & mask(Ty(ret_bits).bits(64));
    Some((got, want))
}

#[cfg(target_arch = "aarch64")]
pub(crate) fn agrees(func: &Func<Global>, args: &[u128]) {
    for garbage in [0, u64::MAX, 0x5a5a_a5a5_0f0f_f0f0] {
        if let Some((got, want)) = both(func, args, garbage) {
            assert_eq!(got, want, "args {args:x?} (garbage {garbage:#x})\n{}", blaze_lir::print(func.view()));
        }
    }
}

/// Edge values of a width, as unsigned bits.
pub(crate) fn edges(bits: u32) -> Vec<u128> {
    let m = u128::from(mask(bits));
    let mut out: Vec<u128> =
        [0, 1, 2, 3, 7, m, m - 1, m >> 1, (m >> 1) + 1, 0x5a5a_5a5a_5a5a_5a5a, 0x8000, 100].map(|it| it & m).into();
    out.sort();
    out.dedup();
    out
}

pub(crate) const WIDTHS: [Ty; 5] = [Ty::I1, Ty::I8, Ty::I16, Ty::I32, Ty::I64];

#[cfg(target_arch = "aarch64")]
#[test]
fn arithmetic_agrees_with_the_evaluator() {
    let binary = [
        Op::Add,
        Op::Sub,
        Op::Mul,
        Op::Udiv,
        Op::Sdiv,
        Op::Urem,
        Op::Srem,
        Op::And,
        Op::Or,
        Op::Xor,
        Op::Shl,
        Op::Lshr,
        Op::Ashr,
    ];
    for ty in WIDTHS {
        let values = edges(ty.bits(64));
        for op in binary {
            let func = one(&[ty, ty], ty, |b, v| b.binary(op, Flags::NONE, v[0], v[1]));
            for &a in &values {
                for &b in &values {
                    agrees(&func, &[a, b]);
                }
            }
        }
        for op in [Op::Neg, Op::Not, Op::Freeze] {
            let func = one(&[ty], ty, |b, v| b.unary(op, Flags::NONE, v[0]));
            for &a in &values {
                agrees(&func, &[a]);
            }
        }
        for cond in IntCond::ALL {
            let func = one(&[ty, ty], Ty::I1, |b, v| b.icmp(cond, v[0], v[1]));
            for &a in &values {
                for &b in &values {
                    agrees(&func, &[a, b]);
                }
            }
        }
        let func = one(&[Ty::I1, ty, ty], ty, |b, v| b.select(Flags::NONE, v[0], v[1], v[2]));
        for &a in &values {
            agrees(&func, &[0, a, 3 & u128::from(mask(ty.bits(64)))]);
            agrees(&func, &[1, a, 0]);
        }
        for to in WIDTHS {
            let (from_bits, to_bits) = (ty.bits(64), to.bits(64));
            let ops: &[Op] = match to_bits.cmp(&from_bits) {
                core::cmp::Ordering::Less => &[Op::Trunc],
                core::cmp::Ordering::Equal => &[],
                core::cmp::Ordering::Greater => &[Op::Zext, Op::Sext],
            };
            for &op in ops {
                let func = one(&[ty], to, |b, v| b.convert(op, Flags::NONE, to, v[0]));
                for &a in &values {
                    agrees(&func, &[a]);
                }
            }
        }
        // Constants of every shape `mov_imm` takes.
        for value in [0u128, 1, 0xffff, 0x1_0000, 0xffff_0000_ffff, u128::from(u64::MAX), 0x8000_0000_0000_0001, 0x1234_5678_9abc_def0] {
            let value = value & u128::from(mask(ty.bits(64)));
            let func = one(&[], ty, |b, _| b.iconst(ty, value));
            agrees(&func, &[]);
        }
    }
}

/// `sum(n) = 1 + 2 + .. + n`, a loop through block parameters.
#[cfg(target_arch = "aarch64")]
/// `uninit` is no code: its cell is left as it is (the walk makes one for a match arm with no
/// value, as in `from_residual`'s uninhabited `Ok(Infallible)`).
#[cfg(target_arch = "aarch64")]
#[test]
fn uninit_emits_nothing() {
    let with = one(&[Ty::I64], Ty::I64, |b, v| {
        let _ = b.uninit(Ty::I64);
        let _ = b.uninit(Ty::I8);
        let one = b.iconst(Ty::I64, 1);
        b.binary(Op::Add, Flags::NONE, v[0], one)
    });
    let without = one(&[Ty::I64], Ty::I64, |b, v| {
        let one = b.iconst(Ty::I64, 1);
        b.binary(Op::Add, Flags::NONE, v[0], one)
    });
    let a = emitted(with.view(), &[]).expect("uninit emits");
    let b = emitted(without.view(), &[]).expect("emits");
    // The frame differs by the two cells only: compare the code after the prologue's size.
    assert_eq!(a.bytes.len(), b.bytes.len(), "no instruction for either `uninit`");
    let exec = run::place(&a, |_| unreachable!("no literal words"));
    assert_eq!(run::call(&exec, [41, 0, 0, 0]), 42);
}

#[test]
fn a_loop_sums() {
    let func = sum_func();
    for n in [0u128, 1, 2, 10, 1000] {
        agrees(&func, &[n]);
    }
    let code = emitted(func.view(), &[]).expect("emits");
    let exec = run::place(&code, |_| unreachable!());
    assert_eq!(run::call(&exec, [100, 0, 0, 0]), 5050);
}

/// The cost of a stack cell per value (codegen.md, "The baseline's cost"): `sum(n)` as this
/// tier emits it against the same loop with every value in a register (the floor a register
/// allocator reaches: `cmp; b.hi; add; add; b`, five instructions an iteration). Prints the
/// code size and the time an iteration of each; asserts only that both agree.
#[cfg(target_arch = "aarch64")]
#[test]
fn the_cell_baseline_against_registers() {
    let code = emitted(sum_func().view(), &[]).expect("emits");
    let exec = run::place(&code, |_| unreachable!());
    let n: u64 = 100_000_000;
    let floor = |n: u64| -> u64 {
        let (mut i, mut acc) = (1u64, 0u64);
        // SAFETY: register arithmetic and branches within the block only; no memory.
        unsafe {
            core::arch::asm!(
                "2:", "cmp {i}, {n}", "b.hi 3f", "add {acc}, {acc}, {i}", "add {i}, {i}, #1", "b 2b", "3:",
                i = inout(reg) i, acc = inout(reg) acc, n = in(reg) n, options(nomem, nostack),
            );
        }
        let _ = i;
        acc
    };
    let time = |f: &dyn Fn() -> u64| {
        let mut best = u128::MAX;
        let mut out = 0;
        for _ in 0..3 {
            let start = std::time::Instant::now();
            out = core::hint::black_box(f());
            best = best.min(start.elapsed().as_nanos());
        }
        (out, best)
    };
    let (cells, cells_ns) = time(&|| run::call(&exec, [core::hint::black_box(n), 0, 0, 0]));
    let (regs, regs_ns) = time(&|| floor(core::hint::black_box(n)));
    assert_eq!(cells, regs);
    println!(
        "cell baseline: sum({n}) code {} bytes ({} instructions); cells {:.3} ns/iter, registers {:.3} ns/iter, ratio {:.2}",
        code.bytes.len(),
        code.bytes.len() / 4,
        cells_ns as f64 / n as f64,
        regs_ns as f64 / n as f64,
        cells_ns as f64 / regs_ns as f64,
    );
}

/// `sum(n) = 1 + 2 + .. + n` through block parameters: entry, a head testing `i > n`, a body
/// adding, the exit returning.
pub(crate) fn sum_func() -> Func<Global> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    let (entry, head, body, exit) = (
        b.create_block(BlockKind::Normal),
        b.create_block(BlockKind::Normal),
        b.create_block(BlockKind::Normal),
        b.create_block(BlockKind::Normal),
    );
    b.begin(entry);
    let n = b.param(Ty::I64);
    let (one_, zero) = (b.iconst(Ty::I64, 1), b.iconst(Ty::I64, 0));
    b.jump((head, &[one_, zero]));
    b.begin(head);
    let (i, acc) = (b.param(Ty::I64), b.param(Ty::I64));
    let done = b.icmp(IntCond::Ugt, i, n);
    b.br(Flags::NONE, done, (exit, &[acc]), (body, &[]));
    b.begin(body);
    let acc2 = b.binary(Op::Add, Flags::NONE, acc, i);
    let one_ = b.iconst(Ty::I64, 1);
    let i2 = b.binary(Op::Add, Flags::NONE, i, one_);
    b.jump((head, &[i2, acc2]));
    b.begin(exit);
    let out = b.param(Ty::I64);
    b.ret(&[out]);
    b.finish_checked().expect("builds")
}

/// A loop whose back edge swaps its parameters: the arguments are all read before any
/// parameter is written.
#[cfg(target_arch = "aarch64")]
#[test]
fn a_back_edge_swaps_its_parameters() {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let params = [Param::of(Ty::I32), Param::of(Ty::I32), Param::of(Ty::I8)];
    let sig = b.sig(Conv::C, Variadic::No, &params, &[Param::of(Ty::I32)]);
    b.set_signature(sig);
    let (entry, head, exit) =
        (b.create_block(BlockKind::Normal), b.create_block(BlockKind::Normal), b.create_block(BlockKind::Normal));
    b.begin(entry);
    let (x, y, n) = (b.param(Ty::I32), b.param(Ty::I32), b.param(Ty::I8));
    b.jump((head, &[x, y, n]));
    b.begin(head);
    let (p, q, k) = (b.param(Ty::I32), b.param(Ty::I32), b.param(Ty::I8));
    let zero = b.iconst(Ty::I8, 0);
    let more = b.icmp(IntCond::Ne, k, zero);
    let one_ = b.iconst(Ty::I8, 1);
    let k1 = b.binary(Op::Sub, Flags::NONE, k, one_);
    // (p, q) <- (q + p, p): reads p after p is the first parameter written.
    let sum = b.binary(Op::Add, Flags::NONE, q, p);
    b.br(Flags::NONE, more, (head, &[q, p, k1]), (exit, &[sum]));
    b.begin(exit);
    let out = b.param(Ty::I32);
    b.ret(&[out]);
    let func = b.finish_checked().expect("builds");
    for (x, y, n) in [(1u128, 2u128, 0u128), (1, 2, 1), (1, 2, 2), (5, 1000, 7), (0xffff_ffff, 3, 255)] {
        agrees(&func, &[x, y, n]);
    }
}

/// A `switch` with small and large case values, one case passing an argument.
#[cfg(target_arch = "aarch64")]
#[test]
fn a_switch_picks_its_case() {
    for ty in [Ty::I8, Ty::I32, Ty::I64] {
        let mut b = Builder::<Global, Full>::new_in(64, Global);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(ty)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let blocks: Vec<Block> = (0..5).map(|_| b.create_block(BlockKind::Normal)).collect();
        b.begin(blocks[0]);
        let x = b.param(ty);
        let wide = match ty.bits(64) {
            64 => x,
            _ => b.convert(Op::Zext, Flags::NONE, Ty::I64, x),
        };
        let big = match ty.bits(64) {
            8 => 200,
            32 => 0xdead_beef,
            _ => 0x1234_5678_9abc_def0,
        };
        let cases = [(0u128, (blocks[1], &[][..])), (5, (blocks[2], &[][..])), (big, (blocks[3], &[wide][..]))];
        b.switch(x, (blocks[4], &[]), &cases);
        for (at, value) in [(1usize, 10u128), (2, 20)] {
            b.begin(blocks[at]);
            let value = b.iconst(Ty::I64, value);
            b.ret(&[value]);
        }
        b.begin(blocks[3]);
        let got = b.param(Ty::I64);
        let one_ = b.iconst(Ty::I64, 1);
        let out = b.binary(Op::Add, Flags::NONE, got, one_);
        b.ret(&[out]);
        b.begin(blocks[4]);
        let value = b.iconst(Ty::I64, 40);
        b.ret(&[value]);
        let func = b.finish_checked().expect("builds");
        for x in [0u128, 5, big, 1, 6, 4096] {
            agrees(&func, &[x & u128::from(mask(ty.bits(64)))]);
        }
    }
}

/// Values stored to a stack slot at offsets of every access form come back.
#[cfg(target_arch = "aarch64")]
#[test]
fn a_stack_slot_round_trips() {
    let func = one(&[Ty::I64, Ty::I32, Ty::I8, Ty::I16], Ty::I64, |b, v| {
        let slot = b.slot(8400, 4, SlotKind::Temp);
        let at = b.slot_addr(slot);
        let none = MemAttrs::default();
        // Unscaled, scaled and materialized offsets.
        b.store(Flags::NONE, v[0], at, 0, none);
        b.store(Flags::NONE, v[1], at, 8, none);
        b.store(Flags::NONE, v[2], at, 13, none);
        b.store(Flags::NONE, v[3], at, 1000, none);
        b.store(Flags::NONE, v[0], at, 8192, none);
        b.store(Flags::NONE, v[1], at, 8196 + 3, none);
        let a = b.load(Ty::I64, Flags::NONE, at, 0, none);
        let c = b.load(Ty::I32, Flags::NONE, at, 8, none);
        let d = b.load(Ty::I8, Flags::NONE, at, 13, none);
        let e = b.load(Ty::I16, Flags::NONE, at, 1000, none);
        let f = b.load(Ty::I64, Flags::NONE, at, 8192, none);
        let g = b.load(Ty::I32, Flags::NONE, at, 8199, none);
        // Through `ptr_add` with a negative `i8` offset too.
        let further = b.iconst(Ty::I64, 20);
        let p = b.ptr_add(Flags::NONE, at, further);
        let back = b.iconst(Ty::I8, 0xf4);
        let p = b.ptr_add(Flags::NONE, p, back);
        let h = b.load(Ty::I32, Flags::NONE, p, 0, none);
        let mut acc = a;
        for value in [c, d, e, g, h] {
            let wide = b.convert(Op::Zext, Flags::NONE, Ty::I64, value);
            let shift = b.iconst(Ty::I64, 7);
            let rolled = b.binary(Op::Shl, Flags::NONE, acc, shift);
            acc = b.binary(Op::Xor, Flags::NONE, rolled, wide);
        }
        b.binary(Op::Sub, Flags::NONE, acc, f)
    });
    for args in [[1u128, 2, 3, 4], [u128::from(u64::MAX), 0xffff_ffff, 0xff, 0xffff], [0x0123_4567_89ab_cdef, 0x8000_0001, 0x80, 0x7fff]] {
        agrees(&func, &args);
    }
}

/// `data_addr` reaches the function's own data after the code.
#[cfg(target_arch = "aarch64")]
#[test]
fn data_addr_reads_its_data() {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I8)]);
    b.set_signature(sig);
    let small = b.data(&[1, 2, 3, 0xfe, 9], 0, &[]);
    let aligned = b.data(&[0x11; 32], 4, &[]);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let i = b.param(Ty::I64);
    let at = b.data_addr(small);
    let at = b.ptr_add(Flags::NONE, at, i);
    let byte = b.load(Ty::I8, Flags::NONE, at, 0, MemAttrs::default());
    let other = b.data_addr(aligned);
    let other = b.load(Ty::I8, Flags::NONE, other, 31, MemAttrs::default());
    let sum = b.binary(Op::Add, Flags::NONE, byte, other);
    b.ret(&[sum]);
    let func = b.finish_checked().expect("builds");
    let code = emitted(func.view(), &[]).expect("emits");
    let exec = run::place(&code, |_| unreachable!());
    for (i, want) in [(0u64, 0x12u64), (3, 0x0f), (4, 0x1a)] {
        assert_eq!(run::call(&exec, [i, 0, 0, 0]) & 0xff, want, "byte {i}");
    }
    let data = code.bytes.len() - 32;
    assert_eq!(data % 16, 0, "the 16-aligned row is placed 16-aligned");
}

/// More values than an `ldr`'s scaled offset reaches from `sp`: cells far up the frame.
#[cfg(target_arch = "aarch64")]
#[test]
fn a_frame_past_the_scaled_offsets() {
    let func = one(&[Ty::I64], Ty::I64, |b, v| {
        let mut acc = v[0];
        let three = b.iconst(Ty::I64, 3);
        for _ in 0..3000 {
            acc = b.binary(Op::Mul, Flags::NONE, acc, three);
            acc = b.binary(Op::Add, Flags::NONE, acc, v[0]);
        }
        acc
    });
    agrees(&func, &[7]);
    agrees(&func, &[u128::from(u64::MAX)]);
}

/// A slot aligned past 16 bytes is aligned at run time.
#[cfg(target_arch = "aarch64")]
#[test]
fn a_slot_aligned_past_the_stack() {
    let func = one(&[], Ty::I64, |b, _| {
        let _small = b.slot(3, 0, SlotKind::Temp);
        let slot = b.slot(64, 6, SlotKind::Temp);
        let at = b.slot_addr(slot);
        b.convert(Op::PtrAddr, Flags::NONE, Ty::I64, at)
    });
    let code = emitted(func.view(), &[]).expect("emits");
    let exec = run::place(&code, |_| unreachable!());
    for _ in 0..3 {
        assert_eq!(run::call(&exec, [0; 4]) % 64, 0);
    }
}

// Calls.

/// `callee(a: i32, b: i8 signext) -> i32 = a * 3 + sext(b)`.
pub(crate) fn callee() -> Func<Global> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let params = [Param::of(Ty::I32), Param { ext: Ext::Sext, ..Param::of(Ty::I8) }];
    let sig = b.sig(Conv::C, Variadic::No, &params, &[Param::of(Ty::I32)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let (a, c) = (b.param(Ty::I32), b.param(Ty::I8));
    let three = b.iconst(Ty::I32, 3);
    let a3 = b.binary(Op::Mul, Flags::NONE, a, three);
    let c = b.convert(Op::Sext, Flags::NONE, Ty::I32, c);
    let out = b.binary(Op::Add, Flags::NONE, a3, c);
    b.ret(&[out]);
    b.finish_checked().expect("builds")
}

/// `caller(x: i32, y: i8) -> i32 = callee(x, y) + 1`, the callee at place 0; returns `ret`.
pub(crate) fn caller_of(ret: Ty) -> (Func<Global>, ()) {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I32), Param::of(Ty::I8)], &[Param::of(ret)]);
    b.set_signature(sig);
    let params = [Param::of(Ty::I32), Param { ext: Ext::Sext, ..Param::of(Ty::I8) }];
    let callee_sig = b.sig(Conv::C, Variadic::No, &params, &[Param::of(Ty::I32)]);
    let callee = b.reference(RefKind::Func, 0);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let (x, y) = (b.param(Ty::I32), b.param(Ty::I8));
    let got = b.call(Flags::NONE, callee, callee_sig, &[x, y]);
    let one_ = b.iconst(Ty::I32, 1);
    let out = b.binary(Op::Add, Flags::NONE, got, one_);
    b.ret(&[out]);
    (b.finish_checked().expect("builds"), ())
}

#[cfg(target_arch = "aarch64")]
#[test]
fn calls_through_an_address_and_a_slot() {
    let callee_code = emitted(callee().view(), &[]).expect("the callee emits");
    let callee_exec = run::place(&callee_code, |_| unreachable!());
    let callee_at = callee_exec.code_address();
    let (caller, ()) = caller_of(Ty::I32);
    let want = |x: u32, y: u8| (x.wrapping_mul(3).wrapping_add(y as i8 as i32 as u32)).wrapping_add(1);
    let cases = [(5u32, 2u8), (5, 0xfe), (0xffff_ffff, 0x80), (1000, 0x7f)];

    let direct = emitted(caller.view(), &[Via::Address]).expect("the caller emits");
    assert_eq!(direct.fixups.len(), 1);
    assert_eq!(direct.fixups[0].place, 0);
    assert_eq!(direct.fixups[0].at % 8, 0);
    let exec = run::place(&direct, |_| callee_at);
    for (x, y) in cases {
        let got = run::call(&exec, [u64::from(x) | 0xabcd << 40, u64::from(y) | 0xff00, 0, 0]) as u32;
        assert_eq!(got, want(x, y), "direct: {x:#x}, {y:#x}");
    }

    let slot_word = Box::new_in(callee_at, Global);
    let slot_at = &*slot_word as *const u64 as u64;
    let through_slot = emitted(caller.view(), &[Via::Slot]).expect("the caller emits");
    let exec = run::place(&through_slot, |_| slot_at);
    for (x, y) in cases {
        let got = run::call(&exec, [u64::from(x), u64::from(y), 0, 0]) as u32;
        assert_eq!(got, want(x, y), "through a slot: {x:#x}, {y:#x}");
    }
    drop(slot_word);

    // `func_addr` of a directly called function is its literal's value.
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::PTR)]);
    b.set_signature(sig);
    let reference = b.reference(RefKind::Func, 0);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let at = b.func_addr(reference);
    b.ret(&[at]);
    let func = b.finish_checked().expect("builds");
    let code = emitted(func.view(), &[Via::Address]).expect("emits");
    let exec = run::place(&code, |_| callee_at);
    assert_eq!(run::call(&exec, [0; 4]), callee_at);
}

// Traps: run in a child process of this test binary, which the `brk` stops with SIGTRAP.

const TRAP_CHILD: &str = "BLAZE_EMIT_TRAP_CHILD";

/// The function that traps for `which`, and its arguments.
fn trapping(which: &str) -> (Func<Global>, [u64; 4]) {
    let divide = |op: Op, ty: Ty| one(&[ty, ty], ty, |b, v| b.binary(op, Flags::NONE, v[0], v[1]));
    match which {
        "udiv by zero" => (divide(Op::Udiv, Ty::I32), [7, 0, 0, 0]),
        "srem by zero" => (divide(Op::Srem, Ty::I8), [7, 0x100, 0, 0]),
        "sdiv i64 MIN by -1" => (divide(Op::Sdiv, Ty::I64), [1 << 63, u64::MAX, 0, 0]),
        "sdiv i16 MIN by -1" => (divide(Op::Sdiv, Ty::I16), [0x8000, 0xffff, 0, 0]),
        "trap" => {
            let mut b = Builder::<Global, Full>::new_in(64, Global);
            let sig = b.sig(Conv::C, Variadic::No, &[], &[]);
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            b.trap(3);
            (b.finish_checked().expect("builds"), [0; 4])
        }
        _ => panic!("no trapping case {which}"),
    }
}

const TRAPS: [&str; 5] = ["udiv by zero", "srem by zero", "sdiv i64 MIN by -1", "sdiv i16 MIN by -1", "trap"];

#[cfg(all(target_arch = "aarch64", unix))]
#[test]
fn traps_stop_with_sigtrap() {
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    if let Ok(which) = std::env::var(TRAP_CHILD) {
        let (func, args) = trapping(&which);
        let code = emitted(func.view(), &[]).expect("emits");
        let exec = run::place(&code, |_| unreachable!());
        run::call(&exec, args);
        panic!("{which} returned instead of trapping");
    }
    let exe = std::env::current_exe().expect("the test binary's path");
    for which in TRAPS {
        let (func, args) = trapping(which);
        let view = func.view();
        let vals: Vec<Val> = view
            .params(Block(0))
            .zip(args)
            .map(|(inst, it)| Val::Int(u128::from(it & mask(view.ty(Value(inst as u32)).bits(64)))))
            .collect();
        let reference = eval::run_in(func.view(), &vals, FUEL, Global).expect("the evaluator runs it");
        assert!(matches!(reference, Outcome::Trapped(_)), "{which}: the evaluator says {reference:?}");
        let mut child = Command::new(&exe)
            .args(["--exact", "tests::traps_stop_with_sigtrap", "--test-threads=1"])
            .env(TRAP_CHILD, which)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the child starts");
        let deadline = Instant::now() + Duration::from_secs(60);
        let status = loop {
            if let Some(status) = child.try_wait().expect("the child is waited on") {
                break status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("{which}: the child ran past its deadline");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.signal(), Some(5), "{which}: the child ended {status:?}, not by SIGTRAP");
    }
}

// Random programs.

pub(crate) mod random {
    use std::alloc::Global;

    use blaze_lir::{
        Block, Builder, Flags, Full, IntCond, Op, Ty, Value,
        builder::{BuildError, MemAttrs, Param, Variadic},
        eval::Val,
        func::{BlockKind, Conv, Func, SlotKind},
    };
    use blaze_sim::Tape;

    const INTS: [Ty; 5] = [Ty::I1, Ty::I8, Ty::I16, Ty::I32, Ty::I64];
    const STORED: [Ty; 4] = [Ty::I8, Ty::I16, Ty::I32, Ty::I64];
    const BINARY: [Op; 13] = [
        Op::Add,
        Op::Sub,
        Op::Mul,
        Op::Udiv,
        Op::Sdiv,
        Op::Urem,
        Op::Srem,
        Op::And,
        Op::Or,
        Op::Xor,
        Op::Shl,
        Op::Lshr,
        Op::Ashr,
    ];
    const DATA: u32 = 32;
    const MIX: u128 = 0x9E37_79B9_7F4A_7C15;

    /// Builds one random program: blaze-lir's tier generator (src/tiers.rs) cut to the
    /// baseline tier's subset. Up to 6 blocks of up to 12 operations, forward edges plus
    /// counted self-loops, every value mixed into an accumulator the last block returns.
    struct Gen {
        b: Builder<Global, Full>,
        blocks: Vec<Block>,
        block_params: Vec<Vec<Ty>>,
        loops: Vec<bool>,
        entry: Vec<(Value, Ty)>,
        pool: Vec<(Value, Ty)>,
        made: Vec<Value>,
        reached: Vec<bool>,
        data: Value,
        acc: Value,
        counters: Value,
    }

    fn ty(tape: &Tape, label: &'static str) -> Ty {
        *tape.one_of(&INTS, label)
    }

    fn log2(bytes: u32) -> u8 {
        bytes.trailing_zeros() as u8
    }

    impl Gen {
        fn keep(&mut self, value: Value) -> Value {
            let ty = self.b.ty(value);
            self.pool.push((value, ty));
            self.made.push(value);
            value
        }

        fn constant(&mut self, tape: &Tape, ty: Ty) -> Value {
            let value = tape.u128("constant");
            let value = self.b.iconst(ty, value);
            self.keep(value)
        }

        fn of(&mut self, tape: &Tape, ty: Ty) -> Value {
            let found: Vec<Value> = self.pool.iter().filter(|it| it.1 == ty).map(|it| it.0).collect();
            match found.is_empty() || tape.below(8, "a new constant") == 0 {
                true => self.constant(tape, ty),
                false => found[tape.below(found.len() as u64, "which value") as usize],
            }
        }

        fn any(&mut self, tape: &Tape) -> Value {
            match self.pool.is_empty() {
                true => {
                    let ty = ty(tape, "type");
                    self.constant(tape, ty)
                }
                false => self.pool[tape.below(self.pool.len() as u64, "which value") as usize].0,
            }
        }

        fn op(&mut self, tape: &Tape) {
            match tape.below(9, "operation") {
                0 => {
                    let ty = ty(tape, "constant type");
                    self.constant(tape, ty);
                }
                1 => {
                    let op = *tape.one_of(&BINARY, "binary");
                    let a = self.any(tape);
                    let ty = self.b.ty(a);
                    let mut b = self.of(tape, ty);
                    let divides = matches!(op, Op::Udiv | Op::Sdiv | Op::Urem | Op::Srem);
                    if divides && tape.below(8, "a zero divisor may stay") != 0 {
                        let one = self.b.iconst(ty, 1);
                        b = self.b.binary(Op::Or, Flags::NONE, b, one);
                    }
                    let value = self.b.binary(op, Flags::NONE, a, b);
                    self.keep(value);
                }
                2 => {
                    let op = *tape.one_of(&[Op::Neg, Op::Not, Op::Freeze], "unary");
                    let a = self.any(tape);
                    let value = self.b.unary(op, Flags::NONE, a);
                    self.keep(value);
                }
                3 => {
                    let a = self.any(tape);
                    let b = self.of(tape, self.b.ty(a));
                    let cond = *tape.one_of(&IntCond::ALL, "condition");
                    let value = self.b.icmp(cond, a, b);
                    self.keep(value);
                }
                4 => {
                    let cond = self.of(tape, Ty::I1);
                    let a = self.any(tape);
                    let b = self.of(tape, self.b.ty(a));
                    let value = self.b.select(Flags::NONE, cond, a, b);
                    self.keep(value);
                }
                5 => {
                    let a = self.any(tape);
                    let (from, to) = (self.b.ty(a), ty(tape, "conversion type"));
                    let op = match (to.bits(64), from.bits(64)) {
                        (to, from) if to == from => return,
                        (to, from) if to < from => Op::Trunc,
                        _ if tape.bool("sign") => Op::Sext,
                        _ => Op::Zext,
                    };
                    let value = self.b.convert(op, Flags::NONE, to, a);
                    self.keep(value);
                }
                6 | 7 => {
                    let ty = *tape.one_of(&STORED, "load type");
                    let size = ty.bits(64) / 8;
                    let at = size * tape.below(u64::from(DATA / size), "load offset") as u32;
                    let value = self.b.load(ty, Flags::NONE, self.data, at as i32, MemAttrs::aligned(log2(size)));
                    self.keep(value);
                }
                _ => {
                    let ty = *tape.one_of(&STORED, "store type");
                    let size = ty.bits(64) / 8;
                    let value = self.of(tape, ty);
                    let at = size * tape.below(u64::from(DATA / size), "store offset") as u32;
                    self.b.store(Flags::NONE, value, self.data, at as i32, MemAttrs::aligned(log2(size)));
                }
            }
        }

        fn mix(&mut self) -> Value {
            let attrs = MemAttrs::aligned(3);
            let mut acc = self.b.load(Ty::I64, Flags::NONE, self.acc, 0, attrs);
            let mix = self.b.iconst(Ty::I64, MIX);
            for value in std::mem::take(&mut self.made) {
                let wide = match self.b.ty(value).bits(64) {
                    64 => value,
                    _ => self.b.convert(Op::Zext, Flags::NONE, Ty::I64, value),
                };
                let xored = self.b.binary(Op::Xor, Flags::NONE, acc, wide);
                acc = self.b.binary(Op::Mul, Flags::NONE, xored, mix);
            }
            self.b.store(Flags::NONE, acc, self.acc, 0, attrs);
            acc
        }

        fn args(&mut self, tape: &Tape, to: usize) -> Vec<Value> {
            let tys = self.block_params[to].clone();
            tys.into_iter().map(|ty| self.of(tape, ty)).collect()
        }

        fn forward(&mut self, tape: &Tape, from: usize, first: bool) -> usize {
            let to = match first && !self.reached[from + 1] {
                true => from + 1,
                false => from + 1 + tape.below((self.blocks.len() - from - 1) as u64, "target") as usize,
            };
            self.reached[to] = true;
            to
        }

        fn block(&mut self, tape: &Tape, at: usize) {
            let last = at + 1 == self.blocks.len();
            let counted = match self.loops[at] {
                true => {
                    let attrs = MemAttrs::aligned(3);
                    let offset = 8 * at as i32;
                    let count = self.b.load(Ty::I64, Flags::NONE, self.counters, offset, attrs);
                    let zero = self.b.iconst(Ty::I64, 0);
                    let again = self.b.icmp(IntCond::Ne, count, zero);
                    let one = self.b.iconst(Ty::I64, 1);
                    let less = self.b.binary(Op::Sub, Flags::NONE, count, one);
                    self.b.store(Flags::NONE, less, self.counters, offset, attrs);
                    Some(again)
                }
                false => None,
            };
            let ops = tape.int_in(0usize..=12, "operations");
            for _ in 0..ops {
                self.op(tape);
            }
            let acc = self.mix();
            if last {
                self.b.ret(&[acc]);
                return;
            }
            if let Some(again) = counted {
                // The back edge passes the block's own parameters, shuffled: what a swap is.
                let (back, on) = (self.args(tape, at), self.forward(tape, at, true));
                let forth = self.args(tape, on);
                let (this, next) = (self.blocks[at], self.blocks[on]);
                self.b.br(Flags::NONE, again, (this, &back), (next, &forth));
                return;
            }
            match tape.below(8, "terminator") {
                0 if self.reached[at + 1] => self.b.trap(tape.int_in(0u32..=3, "trap code")),
                0..=3 => {
                    let on = self.forward(tape, at, true);
                    let args = self.args(tape, on);
                    self.b.jump((self.blocks[on], &args));
                }
                4..=5 => {
                    let cond = self.of(tape, Ty::I1);
                    let (a, b) = (self.forward(tape, at, true), self.forward(tape, at, false));
                    let (then, otherwise) = (self.args(tape, a), self.args(tape, b));
                    let to = (self.blocks[a], then.as_slice());
                    self.b.br(Flags::NONE, cond, to, (self.blocks[b], &otherwise));
                }
                _ => {
                    let ty = *tape.one_of(&STORED, "scrutinee type");
                    let scrutinee = self.of(tape, ty);
                    let default = self.forward(tape, at, true);
                    let default_args = self.args(tape, default);
                    let cases = tape.int_in(0usize..=3, "cases");
                    let mut targets = Vec::new();
                    for case in 0..cases {
                        let on = self.forward(tape, at, false);
                        targets.push((case as u128 * 3 + tape.below(3, "case") as u128, on, self.args(tape, on)));
                    }
                    let cases: Vec<(u128, (Block, &[Value]))> =
                        targets.iter().map(|(value, on, args)| (*value, (self.blocks[*on], args.as_slice()))).collect();
                    self.b.switch(scrutinee, (self.blocks[default], &default_args), &cases);
                }
            }
        }
    }

    /// A random program and its arguments, each its parameter's bits.
    pub(crate) fn program(tape: &Tape) -> Result<(Func<Global>, Vec<Val>), BuildError> {
        let params: Vec<Ty> = tape.vec(0..=4, "parameters", |tape| ty(tape, "parameter"));
        let n = tape.int_in(1usize..=6, "blocks");
        let mut b = Builder::<Global, Full>::new_in(64, Global);
        let sig_params: Vec<Param> = params.iter().map(|&ty| Param::of(ty)).collect();
        let sig = b.sig(Conv::C, Variadic::No, &sig_params, &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let data_slot = b.slot(DATA, 4, SlotKind::Temp);
        let acc_slot = b.slot(8, 3, SlotKind::Temp);
        let counter_slot = b.slot(8 * n as u32, 3, SlotKind::Temp);
        let blocks: Vec<Block> = (0..n).map(|_| b.create_block(BlockKind::Normal)).collect();
        let mut block_params = vec![params.clone()];
        block_params.extend((1..n).map(|_| tape.vec(0..=3, "block parameters", |tape| ty(tape, "block parameter"))));
        let loops: Vec<bool> = (0..n).map(|at| at > 0 && at + 1 < n && tape.bool("loops")).collect();
        let args: Vec<Val> = params
            .iter()
            .map(|ty| {
                let bits = ty.bits(64);
                let value = tape.u128("argument");
                Val::Int(value & ((1 << bits) - 1))
            })
            .collect();
        b.begin(blocks[0]);
        let entry: Vec<(Value, Ty)> = params.iter().map(|&ty| (b.param(ty), ty)).collect();
        let (data, acc, counters) = (b.slot_addr(data_slot), b.slot_addr(acc_slot), b.slot_addr(counter_slot));
        let zero = b.iconst(Ty::I64, 0);
        for at in 0..DATA / 8 {
            b.store(Flags::NONE, zero, data, 8 * at as i32, MemAttrs::aligned(3));
        }
        b.store(Flags::NONE, zero, acc, 0, MemAttrs::aligned(3));
        for at in 0..n {
            let count = b.iconst(Ty::I64, u128::from(tape.int_in(0u64..=5, "loop count")));
            b.store(Flags::NONE, count, counters, 8 * at as i32, MemAttrs::aligned(3));
        }
        let mut g = Gen {
            b,
            blocks,
            block_params,
            loops,
            entry: entry.clone(),
            pool: entry,
            made: Vec::new(),
            reached: (0..n).map(|at| at == 0).collect(),
            data,
            acc,
            counters,
        };
        g.made.extend(g.pool.iter().map(|it| it.0));
        g.block(tape, 0);
        g.entry = g.pool.clone();
        for at in 1..n {
            g.b.begin(g.blocks[at]);
            g.pool = g.entry.clone();
            g.made.clear();
            let params = g.block_params[at].clone();
            for ty in params {
                let value = g.b.param(ty);
                g.pool.push((value, ty));
                g.made.push(value);
            }
            g.block(tape, at);
        }
        Ok((g.b.finish_checked()?, args))
    }
}

/// Random programs of the subset run as machine code as the evaluator runs them, their
/// arguments' bits above their widths garbage (the callee zero-extends what it is given).
#[cfg(target_arch = "aarch64")]
fn machine_agrees(tape: &blaze_sim::Tape) -> Result<(), blaze_sim::Broke> {
    use blaze_sim::Broke;

    let (func, args) = random::program(tape).map_err(|why| Broke::Harness(format!("a generated program does not build: {why:?}")))?;
    let view = func.view();
    blaze_lir::verify(view).map_err(|why| Broke::Harness(format!("a generated program does not verify: {why}")))?;
    let reference = eval::run_in(view, &args, FUEL, Global)
        .map_err(|why| Broke::Harness(format!("the evaluator refused a generated program: {why}")))?;
    let code = emitted(view, &[])
        .map_err(|why| Broke::Oracle(format!("the emitter refused a program of its subset: {why}\n{}", blaze_lir::print(view))))?;
    let want = match reference {
        Outcome::Returned(values) => match values.first() {
            Some(Val::Int(it)) => *it as u64,
            other => return Err(Broke::Harness(format!("a return of other than an integer: {other:?}"))),
        },
        // A trap stops this process; traps are run in a child (`traps_stop_with_sigtrap`).
        Outcome::Trapped(_) | Outcome::Undefined { .. } | Outcome::OutOfFuel => return Ok(()),
    };
    let garbage = tape.next_u64("garbage above the arguments' widths");
    let mut machine = [0u64; 4];
    for (at, (inst, arg)) in view.params(Block(0)).zip(&args).enumerate() {
        let Val::Int(bits) = *arg else { return Err(Broke::Harness("a pointer argument".to_owned())) };
        let width = view.ty(Value(inst as u32)).bits(64);
        machine[at] = bits as u64 | (garbage & !mask(width));
    }
    let exec = run::place(&code, |_| unreachable!("no literal words"));
    let got = run::call(&exec, machine);
    match got == want {
        true => Ok(()),
        false => Err(Broke::Oracle(format!(
            "the machine code returned {got:#x}, the evaluator {want:#x}, on {args:x?}\n{}",
            blaze_lir::print(view)
        ))),
    }
}

#[cfg(target_arch = "aarch64")]
#[test]
fn random_programs_agree_with_the_evaluator() {
    blaze_sim::check(blaze_sim::Config::cases(1000), machine_agrees);
}
