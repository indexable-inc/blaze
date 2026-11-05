//! The SPARC instruction table.
//!
//! SPARC has exactly three instruction formats, and every entry here says
//! which one an mnemonic uses and what goes in its opcode fields. The formats
//! are described in [`super::encode`]; in short, bits 31-30 (`op`) pick
//! between them:
//!
//! ```text
//! op=1  format 1   call, and nothing else: a 30-bit displacement
//! op=0  format 2   sethi and the branches: `op2` plus a 22- or 19-bit field
//! op=2  format 3   arithmetic, logic, jumps, the window ops: `op3`
//! op=3  format 3   loads and stores, same shape, different `op3` space
//! ```

use super::reg::FpWidth;

/// What a `%fsr` or `%fq` operand means for a load or store.
///
/// Both name the floating-point state rather than a register, so neither
/// fills a field: the mnemonic and the register together pick a different
/// opcode, and the `rd` field carries what is left of the distinction.
#[derive(Copy, Clone, Debug)]
pub enum StateOp {
    /// `%fsr`, with the `rd` that says how much of it moves: `ld`/`st` carry
    /// the low 32 bits and `ldx`/`stx` all 64.
    Fsr(u8),
    /// `%fq`, the exception queue, which only `std` reads.
    Fq,
}

/// A load or store. `op3` lives in the `op=3` opcode space.
#[derive(Copy, Clone, Debug)]
pub struct MemForm {
    pub op3: u8,
    /// The `op3` to use when the data register is a float register. `ld` and
    /// `st` pick their opcode from the register class rather than the
    /// mnemonic, so `ld [%o0], %f1` is a different instruction from
    /// `ld [%o0], %g1`.
    pub fop3: Option<u8>,
    /// How wide that float form's register is, which is what says whether
    /// `%f32` and above can name it.
    pub fwidth: FpWidth,
    /// True when the data register comes first: `st %g1, [%o0]`.
    pub store: bool,
    /// The data register must be a float register (`ldf`, `stdf`, ...).
    pub float_only: bool,
    /// What `%fsr` or `%fq` means here, for the mnemonics that move the
    /// floating-point state.
    pub state: Option<StateOp>,
}

#[derive(Copy, Clone, Debug)]
pub enum Form {
    /// `op=2` three-operand: `rs1, reg_or_imm, rd`.
    Alu(u8),
    /// A shift. `x` selects the V9 64-bit variant, which sets bit 12 and
    /// widens the count field from five bits to six.
    Shift {
        op3: u8,
        x: bool,
    },
    Mem(MemForm),
    /// A conditional branch. `predicted` forces the V9 `BPcc` form even
    /// without a `%icc` operand.
    Branch {
        cond: u8,
        predicted: bool,
    },
    /// `fb<cc>`: `FBfcc`, or the V9 `FBPfcc` with a `%fccN` operand. The
    /// condition is numbered by [`fcond_code`], not [`cond_code`].
    BranchFloat(u8),
    /// V9 branch on the contents of a register (`BPr`).
    BranchReg(u8),
    Call,
    Sethi,
    Jmpl,
    /// `save` / `restore`: the register-window instructions.
    Window(u8),
    /// V9 `return`, spelled `rett` in V8.
    Return,
    /// `FPop1` with two sources: `frs1, frs2, frd`. The two widths differ
    /// only for `fsmuld` and `fdmulq`, which widen their result.
    FpBin {
        opf: u16,
        src: FpWidth,
        dst: FpWidth,
    },
    /// `FPop1` with one source: `frs2, frd`. The conversions are this form
    /// with two different widths.
    FpUn {
        opf: u16,
        src: FpWidth,
        dst: FpWidth,
    },
    /// `FPop2` compare: `frs1, frs2`, or `%fccN, frs1, frs2` on V9, where the
    /// bank the result lands in rides in the `rd` field.
    FpCmp {
        opf: u16,
        width: FpWidth,
    },
    /// V9 `fmov<s|d|q><cc> %icc, frs2, frd`, or `%fccN`. As with [`MovCc`],
    /// which condition table the name comes from is what the operand picks.
    ///
    /// [`MovCc`]: Form::MovCc
    FpMovCc {
        icc: Option<u8>,
        fcc: Option<u8>,
        width: FpWidth,
    },
    /// V9 `fmovr<s|d|q><cond> rs1, frs2, frd`.
    FpMovReg {
        rcond: u8,
        width: FpWidth,
    },
    /// V9 `mov<cc> %icc, reg_or_imm, rd`, or `%fccN`, which selects the
    /// floating-point condition names instead. The two tables share names
    /// with different values -- `e` is 1 against `%icc` and 9 against a
    /// `%fcc` -- so both codes travel with the form and the operand picks.
    MovCc {
        icc: Option<u8>,
        fcc: Option<u8>,
    },
    /// V9 `movr<cond> rs1, reg_or_imm, rd`.
    MovReg(u8),
    /// `t<cc> software_trap_number`.
    Trap(u8),
    /// `rd %asr, rd`.
    ReadAsr,
    /// `wr rs1, reg_or_imm, %asr`.
    WriteAsr,
    Flush,
    Unimp,
}

#[derive(Copy, Clone, Debug)]
pub struct Def {
    pub form: Form,
    /// Only exists on V9; refused with a clear message on a V8 target.
    pub v9: bool,
}

const fn v8(form: Form) -> Def {
    Def { form, v9: false }
}

const fn v9(form: Form) -> Def {
    Def { form, v9: true }
}

/// The four-bit integer condition field, shared by `Bicc`, `BPcc`, `MOVcc`
/// and `Tcc`. The bottom three bits name the test and bit 3 inverts it, which
/// is why `be` (1) and `bne` (9) differ by eight.
pub fn cond_code(name: &str) -> Option<u8> {
    Some(match name {
        "n" => 0,
        "e" | "eq" | "z" => 1,
        "le" => 2,
        "l" | "lt" => 3,
        "leu" => 4,
        "cs" | "lu" => 5,
        "neg" => 6,
        "vs" => 7,
        "a" => 8,
        "ne" | "nz" => 9,
        "g" | "gt" => 10,
        "ge" => 11,
        "gu" => 12,
        "cc" | "geu" => 13,
        "pos" => 14,
        "vc" => 15,
        _ => return None,
    })
}

/// The four-bit condition field as the *floating-point* condition codes
/// number it, which `MOVcc` and `FMOVcc` use when the tested register is a
/// `%fccN`. Several names appear in both tables with different values, and
/// eight -- `lg`, `ul`, `ug`, `u`, `ue`, `uge`, `ule`, `o` -- only here.
pub fn fcond_code(name: &str) -> Option<u8> {
    Some(match name {
        "n" => 0,
        "ne" | "nz" => 1,
        "lg" => 2,
        "ul" => 3,
        "l" => 4,
        "ug" => 5,
        "g" => 6,
        "u" => 7,
        "a" => 8,
        "e" | "z" => 9,
        "ue" => 10,
        "ge" => 11,
        "uge" => 12,
        "le" => 13,
        "ule" => 14,
        "o" => 15,
        _ => return None,
    })
}

/// The three-bit `rcond` field of `BPr` and `MOVr`, which tests a whole
/// register against zero rather than the condition codes. `e` and `ne` are
/// the spellings GNU's disassembler prints; `z` and `nz` mean the same.
fn rcond_code(name: &str) -> Option<u8> {
    Some(match name {
        "z" | "e" => 1,
        "lez" => 2,
        "lz" => 3,
        "nz" | "ne" => 5,
        "gz" => 6,
        "gez" => 7,
        _ => return None,
    })
}

fn mem(op3: u8, store: bool) -> Def {
    v8(Form::Mem(MemForm {
        op3,
        fop3: None,
        fwidth: FpWidth::Single,
        store,
        float_only: false,
        state: None,
    }))
}

/// `ld`, `st`, `ldd` and `std`, whose opcode depends on the register class.
fn mem_either(op3: u8, fop3: u8, fwidth: FpWidth, store: bool, state: Option<StateOp>) -> Def {
    v8(Form::Mem(MemForm {
        op3,
        fop3: Some(fop3),
        fwidth,
        store,
        float_only: false,
        state,
    }))
}

/// `ldx` and `stx`, which move no float register but do move `%fsr`.
fn mem_state(op3: u8, store: bool, state: Option<StateOp>) -> Def {
    v8(Form::Mem(MemForm {
        op3,
        fop3: None,
        fwidth: FpWidth::Single,
        store,
        float_only: false,
        state,
    }))
}

/// `ldf`, `stdf` and friends: GNU as spellings that pin the float form.
fn mem_float(op3: u8, fwidth: FpWidth, store: bool) -> Def {
    v8(Form::Mem(MemForm {
        op3,
        fop3: Some(op3),
        fwidth,
        store,
        float_only: true,
        state: None,
    }))
}

pub fn lookup(name: &str) -> Option<Def> {
    if let Some(d) = alu(name) {
        return Some(d);
    }
    if let Some(d) = memory(name) {
        return Some(d);
    }
    if let Some(d) = control(name) {
        return Some(d);
    }
    if let Some(d) = float(name) {
        return Some(d);
    }
    None
}

#[rustfmt::skip]
fn alu(name: &str) -> Option<Def> {
    use Form::*;
    Some(match name {
        // The `cc` suffix is bit 4 of op3: `add` 0x00, `addcc` 0x10.
        "add"    => v8(Alu(0x00)), "addcc"    => v8(Alu(0x10)),
        // V8 calls the carry-propagating forms `addx`/`subx`; V9 renamed them
        // `addc`/`subc`. Both spellings assemble to the same opcode.
        "addx" | "addc"     => v8(Alu(0x08)),
        "addxcc" | "addccc" => v8(Alu(0x18)),
        "sub"    => v8(Alu(0x04)), "subcc"    => v8(Alu(0x14)),
        "subx" | "subc"     => v8(Alu(0x0c)),
        "subxcc" | "subccc" => v8(Alu(0x1c)),
        "and"    => v8(Alu(0x01)), "andcc"    => v8(Alu(0x11)),
        "andn"   => v8(Alu(0x05)), "andncc"   => v8(Alu(0x15)),
        "or"     => v8(Alu(0x02)), "orcc"     => v8(Alu(0x12)),
        "orn"    => v8(Alu(0x06)), "orncc"    => v8(Alu(0x16)),
        "xor"    => v8(Alu(0x03)), "xorcc"    => v8(Alu(0x13)),
        "xnor"   => v8(Alu(0x07)), "xnorcc"   => v8(Alu(0x17)),
        "umul"   => v8(Alu(0x0a)), "umulcc"   => v8(Alu(0x1a)),
        "smul"   => v8(Alu(0x0b)), "smulcc"   => v8(Alu(0x1b)),
        "udiv"   => v8(Alu(0x0e)), "udivcc"   => v8(Alu(0x1e)),
        "sdiv"   => v8(Alu(0x0f)), "sdivcc"   => v8(Alu(0x1f)),
        "mulscc" => v8(Alu(0x24)),
        "taddcc" => v8(Alu(0x20)), "tsubcc"   => v8(Alu(0x21)),
        "taddcctv" => v8(Alu(0x22)), "tsubcctv" => v8(Alu(0x23)),
        "mulx"   => v9(Alu(0x09)),
        "udivx"  => v9(Alu(0x0d)), "sdivx"    => v9(Alu(0x2d)),

        "sll"  => v8(Shift { op3: 0x25, x: false }),
        "srl"  => v8(Shift { op3: 0x26, x: false }),
        "sra"  => v8(Shift { op3: 0x27, x: false }),
        "sllx" => v9(Shift { op3: 0x25, x: true }),
        "srlx" => v9(Shift { op3: 0x26, x: true }),
        "srax" => v9(Shift { op3: 0x27, x: true }),

        // `save` allocates a fresh register window: the caller's `%o`
        // registers become the callee's `%i` registers and `%l` is fresh, so
        // a leaf's locals never touch memory. `restore` rotates it back. Both
        // also add their operands, which is how the stack pointer is bumped
        // in the same instruction: `save %sp, -96, %sp`.
        "save"    => v8(Window(0x3c)),
        "restore" => v8(Window(0x3d)),

        "rd" => v8(ReadAsr),
        "wr" => v8(WriteAsr),
        _ => return None,
    })
}

#[rustfmt::skip]
fn memory(name: &str) -> Option<Def> {
    use FpWidth::{Double, Single};
    // `ld`/`st` and `ldx`/`stx` also move `%fsr`. That pair of opcodes is the
    // same for all four mnemonics; `rd` is the only thing that says whether
    // the low 32 bits move or all 64.
    let fsr32 = Some(StateOp::Fsr(0));
    let fsr64 = Some(StateOp::Fsr(1));
    Some(match name {
        "ld"   => mem_either(0x00, 0x20, Single, false, fsr32),
        "lduw" => mem_either(0x00, 0x20, Single, false, fsr32),
        "ldub" => mem(0x01, false),
        "lduh" => mem(0x02, false),
        "ldd"  => mem_either(0x03, 0x23, Double, false, None),
        "ldsb" => mem(0x09, false),
        "ldsh" => mem(0x0a, false),
        "st" | "stw" => mem_either(0x04, 0x24, Single, true, fsr32),
        "stb"  => mem(0x05, true),
        "sth"  => mem(0x06, true),
        "std"  => mem_either(0x07, 0x27, Double, true, Some(StateOp::Fq)),
        "ldf"  => mem_float(0x20, Single, false),
        "lddf" => mem_float(0x23, Double, false),
        "stf"  => mem_float(0x24, Single, true),
        "stdf" => mem_float(0x27, Double, true),
        // V9 widened the integer registers to 64 bits and added the opcodes
        // that move all of them.
        // Read-modify-write: both exchange with memory and hand the old
        // value back, so the register is written last like a load's.
        "ldstub" => mem(0x0d, false),
        "swap" => mem(0x0f, false),
        "ldsw" => Def { v9: true, ..mem(0x08, false) },
        "ldx"  => Def { v9: true, ..mem_state(0x0b, false, fsr64) },
        "stx"  => Def { v9: true, ..mem_state(0x0e, true, fsr64) },
        _ => return None,
    })
}

fn control(name: &str) -> Option<Def> {
    use Form::*;
    match name {
        "call" => return Some(v8(Call)),
        "sethi" => return Some(v8(Sethi)),
        "jmpl" => return Some(v8(Jmpl)),
        "flush" => return Some(v8(Flush)),
        "unimp" | "illtrap" => return Some(v8(Unimp)),
        "rett" => return Some(v8(Return)),
        // `b` on its own is `ba`: the branch whose condition is "always",
        // which is how GNU's disassembler prints it.
        "b" => {
            return Some(v8(Branch {
                cond: 8,
                predicted: false,
            }));
        }
        // `fb` on its own is `fba`, the same way `b` is `ba`.
        "fb" => return Some(v8(BranchFloat(8))),
        "return" => return Some(v9(Return)),
        _ => {}
    }
    // `b<cc>` and the V9 `bp<cc>`, `br<cond>`, `mov<cc>`, `movr<cond>` and
    // `t<cc>` families are all "opcode plus condition name", so they are
    // decoded from the mnemonic rather than listed one by one.
    //
    // The floating-point moves come first because `fmovs` is both the plain
    // move in `float` below and the stem of `fmovse`; a condition name is
    // what tells them apart, and an empty one is not a condition.
    if let Some(rest) = name.strip_prefix("fmovr")
        && let Some((width, cond)) = width_prefix(rest)
        && let Some(rcond) = rcond_code(cond)
    {
        return Some(v9(FpMovReg { rcond, width }));
    }
    if let Some(rest) = name.strip_prefix("fmov")
        && let Some((width, cond)) = width_prefix(rest)
        && let (icc, fcc) = (cond_code(cond), fcond_code(cond))
        && (icc.is_some() || fcc.is_some())
    {
        return Some(v9(FpMovCc { icc, fcc, width }));
    }
    if let Some(rest) = name.strip_prefix("fb")
        && let Some(cond) = fcond_code(rest)
    {
        return Some(v8(BranchFloat(cond)));
    }
    if let Some(rest) = name.strip_prefix("movr")
        && let Some(rcond) = rcond_code(rest)
    {
        return Some(v9(MovReg(rcond)));
    }
    if let Some(rest) = name.strip_prefix("mov")
        && let (icc, fcc) = (cond_code(rest), fcond_code(rest))
        && (icc.is_some() || fcc.is_some())
    {
        return Some(v9(MovCc { icc, fcc }));
    }
    if let Some(rest) = name.strip_prefix("br")
        && let Some(rcond) = rcond_code(rest)
    {
        return Some(v9(BranchReg(rcond)));
    }
    if let Some(rest) = name.strip_prefix("bp")
        && let Some(cond) = cond_code(rest)
    {
        return Some(v9(Branch {
            cond,
            predicted: true,
        }));
    }
    if let Some(rest) = name.strip_prefix("b")
        && let Some(cond) = cond_code(rest)
    {
        return Some(v8(Branch {
            cond,
            predicted: false,
        }));
    }
    if let Some(rest) = name.strip_prefix("t")
        && let Some(cond) = cond_code(rest)
    {
        return Some(v8(Trap(cond)));
    }
    None
}

/// `FPop1` with both operand widths the same, which is all of the arithmetic
/// but the two widening multiplies.
const fn fp_bin(opf: u16, w: FpWidth) -> Form {
    Form::FpBin {
        opf,
        src: w,
        dst: w,
    }
}

const fn fp_un(opf: u16, src: FpWidth, dst: FpWidth) -> Form {
    Form::FpUn { opf, src, dst }
}

#[rustfmt::skip]
fn float(name: &str) -> Option<Def> {
    use FpWidth::{Double, Quad, Single};
    use Form::FpCmp;
    Some(match name {
        "fadds" => v8(fp_bin(0x41, Single)), "faddd" => v8(fp_bin(0x42, Double)), "faddq" => v8(fp_bin(0x43, Quad)),
        "fsubs" => v8(fp_bin(0x45, Single)), "fsubd" => v8(fp_bin(0x46, Double)), "fsubq" => v8(fp_bin(0x47, Quad)),
        "fmuls" => v8(fp_bin(0x49, Single)), "fmuld" => v8(fp_bin(0x4a, Double)), "fmulq" => v8(fp_bin(0x4b, Quad)),
        "fdivs" => v8(fp_bin(0x4d, Single)), "fdivd" => v8(fp_bin(0x4e, Double)), "fdivq" => v8(fp_bin(0x4f, Quad)),
        // The two multiplies that widen: the product needs twice the bits the
        // factors have, so the destination is a register pair or quad.
        "fsmuld" => v8(Form::FpBin { opf: 0x69, src: Single, dst: Double }),
        "fdmulq" => v8(Form::FpBin { opf: 0x6e, src: Double, dst: Quad }),

        "fmovs" => v8(fp_un(0x01, Single, Single)), "fnegs" => v8(fp_un(0x05, Single, Single)),
        "fabss" => v8(fp_un(0x09, Single, Single)),
        // The double and quad register-to-register moves only exist on V9;
        // on V8 they are written as two or four `fmovs`.
        "fmovd" => v9(fp_un(0x02, Double, Double)), "fnegd" => v9(fp_un(0x06, Double, Double)),
        "fabsd" => v9(fp_un(0x0a, Double, Double)),
        "fmovq" => v9(fp_un(0x03, Quad, Quad)), "fnegq" => v9(fp_un(0x07, Quad, Quad)),
        "fabsq" => v9(fp_un(0x0b, Quad, Quad)),

        "fsqrts" => v8(fp_un(0x29, Single, Single)), "fsqrtd" => v8(fp_un(0x2a, Double, Double)),
        "fsqrtq" => v8(fp_un(0x2b, Quad, Quad)),
        // The conversions are the same form with the two widths apart. A
        // 32-bit integer sits in a single register and a 64-bit one in a
        // double, whatever the value in it means.
        "fitos" => v8(fp_un(0xc4, Single, Single)), "fitod" => v8(fp_un(0xc8, Single, Double)),
        "fitoq" => v8(fp_un(0xcc, Single, Quad)),
        "fstoi" => v8(fp_un(0xd1, Single, Single)), "fdtoi" => v8(fp_un(0xd2, Double, Single)),
        "fqtoi" => v8(fp_un(0xd3, Quad, Single)),
        "fstod" => v8(fp_un(0xc9, Single, Double)), "fstoq" => v8(fp_un(0xcd, Single, Quad)),
        "fdtos" => v8(fp_un(0xc6, Double, Single)), "fdtoq" => v8(fp_un(0xce, Double, Quad)),
        "fqtos" => v8(fp_un(0xc7, Quad, Single)), "fqtod" => v8(fp_un(0xcb, Quad, Double)),
        // V9 added the conversions to and from a 64-bit integer, which needs
        // a register pair to hold it whatever the other side is.
        "fxtos" => v9(fp_un(0x84, Double, Single)), "fxtod" => v9(fp_un(0x88, Double, Double)),
        "fxtoq" => v9(fp_un(0x8c, Double, Quad)),
        "fstox" => v9(fp_un(0x81, Single, Double)), "fdtox" => v9(fp_un(0x82, Double, Double)),
        "fqtox" => v9(fp_un(0x83, Quad, Double)),

        "fcmps" => v8(FpCmp { opf: 0x51, width: Single }), "fcmpd" => v8(FpCmp { opf: 0x52, width: Double }),
        "fcmpq" => v8(FpCmp { opf: 0x53, width: Quad }),
        "fcmpes" => v8(FpCmp { opf: 0x55, width: Single }), "fcmped" => v8(FpCmp { opf: 0x56, width: Double }),
        "fcmpeq" => v8(FpCmp { opf: 0x57, width: Quad }),
        _ => return None,
    })
}

/// The `s`, `d` or `q` that ends a mnemonic's stem, with the rest after it:
/// `fmovd` + `e` in `fmovde`, `fmovr` + `s` + `gz` in `fmovrsgz`.
fn width_prefix(rest: &str) -> Option<(FpWidth, &str)> {
    let (letter, tail) = rest.split_at_checked(1)?;
    let w = match letter {
        "s" => FpWidth::Single,
        "d" => FpWidth::Double,
        "q" => FpWidth::Quad,
        _ => return None,
    };
    Some((w, tail))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn condition_suffixes_do_not_swallow_whole_mnemonics() {
        // `bpos` is "branch on positive", not the predicted branch `bp` with
        // an `os` condition; `movrz` is MOVr, not MOVcc with an `rz` cond.
        assert!(matches!(
            lookup("bpos"),
            Some(Def {
                form: Form::Branch {
                    cond: 14,
                    predicted: false
                },
                ..
            })
        ));
        assert!(matches!(
            lookup("movrz"),
            Some(Def {
                form: Form::MovReg(1),
                ..
            })
        ));
        assert!(matches!(
            lookup("bpe"),
            Some(Def {
                form: Form::Branch {
                    cond: 1,
                    predicted: true
                },
                ..
            })
        ));
    }

    /// A condition name is what makes a branch or a trap; the prefix on its
    /// own is not one. `b` is the exception: it is `ba`, and both GNU as and
    /// llvm-mc assemble it.
    #[test]
    fn unknown_mnemonics_are_not_invented() {
        for bad in [
            "", "t", "mov", "movr", "br", "bp", "zzz", "ldq", "fmov", "fmovr", "fmovrs", "fmovx",
        ] {
            assert!(lookup(bad).is_none(), "`{bad}` should not resolve");
        }
        // `b` is `ba` and `fb` is `fba`; both are spellings the disassembler
        // prints.
        assert!(lookup("b").is_some());
        assert!(lookup("fb").is_some());
    }

    /// The floating-point moves are read as a stem, a width letter and a
    /// condition, which the plain `fmovs`, `fmovd` and `fmovq` must survive.
    #[test]
    fn a_width_letter_alone_is_not_a_conditional_move() {
        assert!(matches!(
            lookup("fmovs"),
            Some(Def {
                form: Form::FpUn { opf: 0x01, .. },
                ..
            })
        ));
        assert!(matches!(
            lookup("fmovse"),
            Some(Def {
                form: Form::FpMovCc {
                    icc: Some(1),
                    fcc: Some(9),
                    ..
                },
                ..
            })
        ));
        assert!(matches!(
            lookup("fmovrsgz"),
            Some(Def {
                form: Form::FpMovReg { rcond: 6, .. },
                ..
            })
        ));
        // `fbne` is 1 in the floating-point table where the integer `bne` is
        // 9, so the two prefixes really do read different tables.
        assert!(matches!(
            lookup("fbne"),
            Some(Def {
                form: Form::BranchFloat(1),
                ..
            })
        ));
    }
}
