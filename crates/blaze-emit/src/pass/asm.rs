//! `asm`: one `asm!` block (blaze-lir func.rs `AsmRec`; blaze-build walk/asm.rs makes it).
//!
//! Every LIR value lives in its cell (lib.rs, "Values") and no register is held across an
//! instruction, so the block owns every caller-saved register while it runs:
//!
//! 1. Each operand gets a register: an explicit one (`in("x0") a`) as named, then each class
//!    operand the first free register of its class, in [`X_POOL`] or [`V_POOL`] order. Every
//!    operand gets its own register (the Reference lets a late output share an input's; it
//!    never requires it). Registers this tier does not save are refused as explicit operands
//!    by name: x18 (the platform's), x19 (LLVM's base pointer, which rustc refuses too), x29
//!    (the frame pointer), x20..x28 and v8..v15 (callee-saved: the prologue saves only x29 and
//!    x30). x30 is free: the prologue saved it and the epilogue restores it.
//! 2. Two more free general registers are the block's scratch: one carries a vector output to
//!    its cell, the other makes a cell's address past the scaled offsets. Neither is any
//!    operand's register, so no operand is overwritten on its way in or out.
//! 3. Inputs (`in`, `inout`, `inlateout`) are loaded from their cells: a general register takes
//!    the cell's 8 low bytes (the value zero-extended, an `f32`/`f64` its bits); a vector
//!    register the cell's 16 bytes (`ldr q`), so the lanes past the value are unspecified, as
//!    the Reference says of a value narrower than its register.
//! 4. The template is read by the assembler (asm.rs): text pieces as they are, each
//!    placeholder as its register's name under its modifier (`reg`: `x`, `w`; `vreg`: `v`, `b`,
//!    `h`, `s`, `d`, `q`). A comment-only template makes no instruction.
//! 5. Outputs are stored to the result's cell (one output) or to the members of the pair it
//!    is (two outputs, lib.rs "Pairs"), each zero-extended to its width.
//!
//! The options need nothing here, and are honoured by that: without `nomem` the block may
//! touch any memory, and no value is cached in a register across it; without
//! `preserves_flags` it may change NZCV, and no flag is live across an instruction; without
//! `nostack` it may push below `sp`, which is the frame's bottom. `noreturn` is the LIR
//! `unreachable` that follows the block. A `sym` operand, a `label` (`asm_br`) and a block of
//! more than two outputs are refused by name.
//! A C ABI clobber needs no caller-save spills here. Live values already have frame cells,
//! and the prologue saved the incoming link register. A literal `bl name` still needs the
//! function reference place to which the call site will be bound.

use alloc::alloc::Allocator;

use blaze_lir::{
    Op, Ty, Value,
    func::{AsmOperandKind, AsmPieceRec},
};

use super::{EmitError, FloatWidth, Pass, unsupported};
use crate::{
    asm::{Assembler, Dialect},
    enc::{self, SP, Size},
};

/// The most operands one block may have here (the class pools hold 18 and 24 registers).
const OPERANDS_MOST: usize = 32;

/// The general registers a class operand or the scratch takes, in order: the caller-saved
/// ones, x16 and x17 last (the linker's veneer registers, free inside one instruction).
const X_POOL: [u32; 18] = [9, 10, 11, 12, 13, 14, 15, 0, 1, 2, 3, 4, 5, 6, 7, 8, 16, 17];

/// The vector registers a class operand takes: all of v0..v7 and v16..v31 are caller-saved
/// (AAPCS64 6.1.2: only the low 64 bits of v8..v15 are callee-saved).
const V_POOL: [u32; 24] = [0, 1, 2, 3, 4, 5, 6, 7, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31];

/// A register bank.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Bank {
    /// `x0..x30`.
    X,
    /// `v0..v31`.
    V,
}

/// One operand, placed.
#[derive(Clone, Copy, Debug)]
struct Placed {
    bank: Bank,
    reg: u32,
    /// A class operand (a placeholder may name it); explicit ones may not be named.
    class: bool,
    /// The value it takes in, for `in`, `inout` and `inlateout`.
    input: Option<Value>,
    /// Its member of the result, for `out`, `lateout`, `inout` and `inlateout`.
    member: Option<u32>,
}

/// The bank and number of explicit register `name` (the Reference's AArch64 register names and
/// their aliases), or why it is refused.
fn explicit(name: &[u8]) -> Result<(Bank, u32), &'static str> {
    match name {
        b"lr" => return Ok((Bank::X, 30)),
        b"fp" => return Err("the frame pointer x29 as an `asm!` operand"),
        b"sp" | b"wsp" => return Err("the stack pointer as an `asm!` operand"),
        b"xzr" | b"wzr" => return Err("the zero register as an `asm!` operand"),
        _ => {}
    }
    let Some((&head, digits)) = name.split_first() else { return Err("an `asm!` operand with an empty register name") };
    let n = match digits {
        [d] if d.is_ascii_digit() => u32::from(d - b'0'),
        [a, b] if a.is_ascii_digit() && b.is_ascii_digit() && *a != b'0' => u32::from(a - b'0') * 10 + u32::from(b - b'0'),
        _ => return Err("a register name the AArch64 `asm!` rules have not"),
    };
    match head {
        b'x' | b'w' => match n {
            18 => Err("x18, reserved by the platform, as an `asm!` operand"),
            19 => Err("x19, LLVM's base pointer, as an `asm!` operand"),
            29 => Err("the frame pointer x29 as an `asm!` operand"),
            20..=28 => Err("a callee-saved register x20..x28 as an `asm!` operand (the baseline tier saves none)"),
            0..=30 => Ok((Bank::X, n)),
            _ => Err("a register name the AArch64 `asm!` rules have not"),
        },
        b'v' | b'q' | b'd' | b's' | b'h' | b'b' => match n {
            8..=15 => Err("a vector register v8..v15 as an `asm!` operand (its low half is callee-saved; the baseline tier saves none)"),
            0..=31 => Ok((Bank::V, n)),
            _ => Err("a register name the AArch64 `asm!` rules have not"),
        },
        _ => Err("a register name the AArch64 `asm!` rules have not"),
    }
}

/// The bank of class `name` and whether it is `vreg_low16`, or why it is refused.
fn class(name: &[u8]) -> Result<(Bank, bool), &'static str> {
    match name {
        b"reg" => Ok((Bank::X, false)),
        b"vreg" => Ok((Bank::V, false)),
        b"vreg_low16" => Ok((Bank::V, true)),
        b"preg" => Err("the `preg` class (SVE predicates are only clobbers)"),
        _ => Err("an `asm!` register class AArch64 has not (`reg`, `vreg`, `vreg_low16`)"),
    }
}

/// Register `p`'s name under template modifier `modifier` (0 for none): at most `w30`/`v31`.
fn render(p: Placed, modifier: u32) -> Result<([u8; 3], usize), &'static str> {
    let letter = match (p.bank, char::from_u32(modifier)) {
        (Bank::X, Some('\0' | 'x')) => b'x',
        (Bank::X, Some('w')) => b'w',
        (Bank::X, _) => return Err("a template modifier the `reg` class has not (`x`, `w`)"),
        (Bank::V, Some('\0' | 'v')) => b'v',
        (Bank::V, Some(m @ ('b' | 'h' | 's' | 'd' | 'q'))) => m as u8,
        (Bank::V, _) => return Err("a template modifier the `vreg` class has not (`v`, `b`, `h`, `s`, `d`, `q`)"),
    };
    let mut out = [letter, 0, 0];
    // `reg` is below 32.
    Ok(match p.reg {
        n @ 0..=9 => {
            out[1] = b'0' + n as u8;
            (out, 2)
        }
        n => {
            out[1] = b'0' + (n / 10) as u8;
            out[2] = b'0' + (n % 10) as u8;
            (out, 3)
        }
    })
}

/// The bits a one-word value of `ty` has in its cell, or `None` for a two-word one
/// (`i128`, `f128`); `Err` for a type no cell of this tier holds as an operand.
fn bits_of(pass_width: Option<u32>, ty: Ty) -> Result<Option<u32>, &'static str> {
    if ty == Ty::I128 {
        return Ok(None);
    }
    if let Some(w) = pass_width {
        return Ok(Some(w));
    }
    match FloatWidth::of(ty) {
        Some(FloatWidth::Half) => Ok(Some(16)),
        Some(FloatWidth::Single) => Ok(Some(32)),
        Some(FloatWidth::Double) => Ok(Some(64)),
        Some(FloatWidth::Quad) => Ok(None),
        None => Err("an `asm!` operand of a type no cell holds as one value (a vector or a compound: legalize did not lower it)"),
    }
}

impl<A: Allocator> Pass<'_, '_, A> {
    /// `asm` instruction `inst` (see the module's comment).
    pub(super) fn asm(&mut self, inst: usize, op: Op) -> Result<(), EmitError> {
        let view = self.view;
        let mut o = view.operands(inst);
        let id = o.asm();
        let inputs = o.values();
        let operands = view.asm_operands_of(id);
        if operands.len() > OPERANDS_MOST {
            return Err(unsupported(inst, op, "an `asm!` block of more than 32 operands"));
        }

        // The result's members: none, the one output's type, or a pair.
        let result = view.ty(Value(inst as u32));
        let outputs = operands.iter().filter(|it| AsmOperandKind::from_byte(it.kind).is_some_and(AsmOperandKind::gives)).count();
        let pair = match outputs {
            0 | 1 => None,
            2 => {
                let members = view.members(result);
                match (members.first(), members.get(1)) {
                    (Some(&a), Some(&b)) => Some([Ty(a), Ty(b)]),
                    _ => return Err(unsupported(inst, op, "an `asm!` block of two outputs whose result is not a pair")),
                }
            }
            _ => return Err(unsupported(inst, op, "an `asm!` block of more than two outputs (a cell holds a pair at most)")),
        };

        // 1. Registers: explicit ones first, then classes.
        let mut placed: [Option<Placed>; OPERANDS_MOST] = [None; OPERANDS_MOST];
        let (mut x_used, mut v_used) = (0u32, 0u32);
        let (mut next_input, mut next_member) = (0usize, 0u32);
        for (i, rec) in operands.iter().enumerate() {
            let kind = AsmOperandKind::from_byte(rec.kind).ok_or(unsupported(inst, op, "an `asm!` operand of no known kind"))?;
            match kind {
                AsmOperandKind::Sym => {
                    return Err(unsupported(inst, op, "a `sym` operand of `asm!` (a symbol in the template is not assembled yet)"));
                }
                AsmOperandKind::Label => return Err(unsupported(inst, op, "a `label` operand of `asm!` outside an `asm_br`")),
                _ => {}
            }
            let input = match kind.takes() {
                true => {
                    let v = inputs.get(next_input);
                    next_input += 1;
                    Some(v)
                }
                false => None,
            };
            let member = match kind.gives() {
                true => {
                    next_member += 1;
                    Some(next_member - 1)
                }
                false => None,
            };
            if rec.class != 0 {
                placed[i] = Some(Placed { bank: Bank::X, reg: u32::MAX, class: true, input, member });
                continue;
            }
            let name = view.asm_str(rec.reg_first, rec.reg_len);
            let (bank, reg) = explicit(name).map_err(|why| unsupported(inst, op, why))?;
            let used = match bank {
                Bank::X => &mut x_used,
                Bank::V => &mut v_used,
            };
            if *used & (1 << reg) != 0 {
                return Err(unsupported(inst, op, "two `asm!` operands in one explicit register"));
            }
            *used |= 1 << reg;
            placed[i] = Some(Placed { bank, reg, class: false, input, member });
        }
        for (i, rec) in operands.iter().enumerate() {
            let Some(p) = placed[i].as_mut() else { continue };
            if !p.class {
                continue;
            }
            let name = view.asm_str(rec.reg_first, rec.reg_len);
            let (bank, low16) = class(name).map_err(|why| unsupported(inst, op, why))?;
            let (pool, used): (&[u32], &mut u32) = match bank {
                Bank::X => (&X_POOL[..], &mut x_used),
                Bank::V => (&V_POOL[..], &mut v_used),
            };
            let free = pool.iter().copied().find(|&r| *used & (1 << r) == 0 && (!low16 || r < 16));
            let reg = free.ok_or(unsupported(inst, op, "an `asm!` block of more class operands than the class has free registers"))?;
            *used |= 1 << reg;
            p.bank = bank;
            p.reg = reg;
        }

        // 2. The scratch pair, never an operand's.
        let mut spare = X_POOL.iter().copied().filter(|&r| x_used & (1 << r) == 0);
        let (Some(carry), Some(far)) = (spare.next(), spare.next()) else {
            return Err(unsupported(inst, op, "an `asm!` block that leaves fewer than two general registers free"));
        };

        // 3. Inputs.
        for p in placed.iter().flatten() {
            let Some(v) = p.input else { continue };
            let ty = view.ty(v);
            let bits = bits_of(self.width(ty), ty).map_err(|why| unsupported(inst, op, why))?;
            let off = v.0 * 16;
            match (p.bank, bits) {
                (Bank::X, Some(_)) => self.asm_word(true, p.reg, off, far),
                (Bank::X, None) => return Err(unsupported(inst, op, "an `i128` or `f128` `asm!` operand in a general register")),
                (Bank::V, _) => self.asm_q(true, p.reg, off, far),
            }
        }

        // 4. The template.
        let mut reader = Assembler::new(Dialect::of(self.isa.os), self.code, self.scratch);
        let refused = |e: crate::asm::AsmError| EmitError::Asm { inst: inst as u32, line: e.line, why: e.why, token: e.token };
        for piece in view.asm_pieces_of(id) {
            match piece.kind {
                AsmPieceRec::TEXT => {
                    let text = view.asm_str(piece.a, piece.b);
                    reader.feed(text).map_err(refused)?;
                }
                AsmPieceRec::OPERAND => {
                    let p = placed
                        .get(piece.a as usize)
                        .copied()
                        .flatten()
                        .ok_or(unsupported(inst, op, "an `asm!` placeholder past its operands"))?;
                    if !p.class {
                        return Err(unsupported(inst, op, "an `asm!` placeholder naming an explicit register operand"));
                    }
                    let (name, len) = render(p, piece.b).map_err(|why| unsupported(inst, op, why))?;
                    reader.feed(&name[..len]).map_err(refused)?;
                }
                _ => return Err(unsupported(inst, op, "an `asm!` template piece of no known kind")),
            }
        }
        reader.finish().map_err(refused)?;

        // 5. Outputs.
        for p in placed.iter().flatten() {
            let Some(member) = p.member else { continue };
            let (ty, off) = match pair {
                None => (result, inst as u32 * 16),
                Some(tys) => (tys[member as usize], self.value_word_at(inst, member)),
            };
            let bits = bits_of(self.width(ty), ty).map_err(|why| unsupported(inst, op, why))?;
            match (p.bank, bits) {
                (Bank::X, Some(bits)) => {
                    self.narrow(p.reg, bits);
                    self.asm_word(false, p.reg, off, far);
                }
                (Bank::X, None) => return Err(unsupported(inst, op, "an `i128` or `f128` `asm!` output in a general register")),
                (Bank::V, Some(bits)) => {
                    self.put(match bits {
                        64 => enc::fmov_from_d(carry, p.reg),
                        _ => enc::fmov_from_s(carry, p.reg),
                    });
                    self.narrow(carry, bits);
                    self.asm_word(false, carry, off, far);
                }
                (Bank::V, None) if pair.is_none() => self.asm_q(false, p.reg, off, far),
                (Bank::V, None) => return Err(unsupported(inst, op, "an `i128` or `f128` `asm!` output beside another output")),
            }
        }
        Ok(())
    }

    /// `ldr` (`load`) or `str` of `rt` and the 8 bytes at `sp + off` (a multiple of 8), past
    /// the scaled offset through `far`, which is no operand's register.
    fn asm_word(&mut self, load: bool, rt: u32, off: u32, far: u32) {
        let (base, imm) = match off / 8 < 4096 {
            true => (SP, off / 8),
            false => {
                self.sp_addr(far, off);
                (far, 0)
            }
        };
        self.put(match load {
            true => enc::ldr(Size::X, rt, base, imm),
            false => enc::str(Size::X, rt, base, imm),
        });
    }

    /// `ldr q<v>` (`load`) or `str q<v>` of the 16 bytes at `sp + off` (a cell), past the
    /// scaled offset through `far`.
    fn asm_q(&mut self, load: bool, v: u32, off: u32, far: u32) {
        let (base, imm) = match off / 16 < 4096 {
            true => (SP, off / 16),
            false => {
                self.sp_addr(far, off);
                (far, 0)
            }
        };
        self.put(match load {
            true => enc::ldr_q(v, base, imm),
            false => enc::str_q(v, base, imm),
        });
    }
}
