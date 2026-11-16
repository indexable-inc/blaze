//! The one forward pass of [`crate::emit`]: blocks in layout order, each instruction lowered
//! as it comes, branch and PC-relative fields patched once everything is placed from the list
//! of patches the pass kept (no second walk of the input).

use std::alloc::{Allocator, Global};

use blaze_lir::{
    Block, Flags, IntCond, Op, Scalar, Ty, Value, View,
    func::{Conv, DataId, Ext, NONE, ParamKind, ParamRec, RefId, RefKind, SigId, SigRec, SlotId},
    op::Target,
};

use crate::{
    EmitError, Fixup, Via,
    enc::{self, Cond, FP, Field, LR, Reg, SP, Size, X0, X9, X10, X11, X12, X16, ZR},
};

/// `brk` immediates of the traps the pass makes itself; a LIR `trap`'s code is its own `brk`
/// immediate, so codes from here up are refused.
pub(crate) const BRK_DIV_ZERO: u32 = 0xff00;
pub(crate) const BRK_DIV_OVERFLOW: u32 = 0xff01;
pub(crate) const BRK_UNREACHABLE: u32 = 0xff02;

/// The frame's bound: every `sp` offset is reached by at most two `add`s of 12 bits.
const FRAME_LIMIT: u64 = 1 << 24;
/// The most alignment a stack slot may ask: a page.
const MOST_SLOT_ALIGN: u8 = 12;
/// The most alignment a data row may ask: what placement promises the code's first byte.
const MOST_DATA_ALIGN: u32 = 4;

/// What a patched field reaches.
#[derive(Clone, Copy, Debug)]
enum Label {
    Block(u32),
    /// The literal word of `out.fixups[i]`.
    Literal(u32),
    /// The data row placed `i`th.
    Data(u32),
}

#[derive(Clone, Copy, Debug)]
struct Patch {
    at: u32,
    field: Field,
    to: Label,
}

struct Pass<'a, 'c, A: Allocator> {
    view: View<'a>,
    via: &'a [Via],
    code: &'c mut Vec<u8, A>,
    /// One per literal word, in order; `at` is filled when the words are placed.
    fixups: &'c mut Vec<Fixup, A>,
    /// The function's own returns.
    rets: &'a [ParamRec],
    patches: Vec<Patch, Global>,
    /// Per block, its first byte (`NONE` until placed).
    block_at: Vec<u32, Global>,
    /// Per reference row, its literal's index (`NONE` until asked).
    ref_literal: Vec<u32, Global>,
    /// Per data row, its index among `placed` (`NONE` until asked).
    data_index: Vec<u32, Global>,
    /// The data rows `data_addr` reaches, in the order they are placed.
    placed: Vec<u32, Global>,
    /// Per stack slot, its offset from `sp`.
    slot_at: Vec<u32, Global>,
    /// Where the staging words of a block's arguments start, from `sp`.
    staging_at: u32,
    /// The most staging words an edge took.
    staging: u32,
}

/// Runs the pass (see [`crate::emit`]).
pub(crate) fn run<A: Allocator>(
    view: View<'_>,
    via: &[Via],
    bytes: &mut Vec<u8, A>,
    fixups: &mut Vec<Fixup, A>,
) -> Result<(), EmitError> {
    if view.head.ptr_bits != 64 {
        return Err(EmitError::Signature { why: "a target whose pointers are not 64 bits" });
    }
    let sig = view.sigs.get(view.head.sig as usize).map(|_| view.sig(SigId(view.head.sig)));
    let Some(sig) = sig else { return Err(EmitError::Signature { why: "no signature row" }) };
    check_sig(view.sigs[view.head.sig as usize], sig.params, sig.rets)
        .map_err(|why| EmitError::Signature { why })?;

    // The frame: a 16-byte cell per value, then each slot at its alignment, then the staging
    // words; `sp` aligned to the most any slot asks.
    let cells = view.len() as u64 * 16;
    let mut at = cells;
    let mut most_align: u64 = 16;
    let mut slot_at = Vec::with_capacity_in(view.slots.len(), Global);
    for slot in view.slots {
        if slot.align > MOST_SLOT_ALIGN {
            return Err(EmitError::TooLarge { what: "a stack slot's alignment (past 4 KiB)" });
        }
        let align = 1u64 << slot.align;
        most_align = most_align.max(align);
        at = at.next_multiple_of(align);
        slot_at.push(at as u32);
        at += u64::from(slot.size);
        if at >= FRAME_LIMIT {
            return Err(EmitError::TooLarge { what: "the frame (16 MiB)" });
        }
    }
    let staging_at = at.next_multiple_of(8);
    if staging_at >= FRAME_LIMIT {
        return Err(EmitError::TooLarge { what: "the frame (16 MiB)" });
    }

    let mut block_at = Vec::with_capacity_in(view.blocks(), Global);
    block_at.resize(view.blocks(), NONE);
    let mut ref_literal = Vec::with_capacity_in(view.refs.len(), Global);
    ref_literal.resize(view.refs.len(), NONE);
    let mut data_index = Vec::with_capacity_in(view.datas.len(), Global);
    data_index.resize(view.datas.len(), NONE);
    let mut pass = Pass {
        view,
        via,
        code: bytes,
        fixups,
        rets: sig.rets,
        patches: Vec::new_in(Global),
        block_at,
        ref_literal,
        data_index,
        placed: Vec::new_in(Global),
        slot_at,
        staging_at: staging_at as u32,
        staging: 0,
    };

    // The prologue; the frame's size is patched in at the end, when the staging is known.
    pass.put(enc::stp_pre(FP, LR, SP, -16));
    pass.put(enc::add_imm(FP, SP, 0, false));
    let frame_at = pass.here();
    pass.put(enc::sub_imm(SP, SP, 0, true));
    pass.put(enc::sub_imm(SP, SP, 0, false));
    if most_align > 16 {
        let log2 = most_align.trailing_zeros();
        pass.put(enc::add_imm(X16, SP, 0, false));
        pass.put(enc::lsr_imm(X16, X16, log2));
        pass.put(enc::lsl_imm(X16, X16, log2));
        pass.put(enc::add_imm(SP, X16, 0, false));
    }
    // The parameters arrive in x0..x7, their bits above the width unspecified.
    for (reg, inst) in view.params(Block(0)).enumerate() {
        let width = pass.width(view.ty(Value(inst as u32))).ok_or(EmitError::Signature {
            why: "an entry parameter that is not an integer or pointer of at most 64 bits",
        })?;
        let reg = reg as Reg;
        match width < 64 {
            true => {
                pass.put(enc::zext(X9, reg, width));
                pass.st(X9, inst);
            }
            false => pass.st(reg, inst),
        }
    }

    for block in 0..view.blocks() {
        pass.block_at[block] = pass.here();
        let next = (block + 1 < view.blocks()).then_some(block as u32 + 1);
        let range = view.block(Block(block as u32));
        for inst in view.params(Block(block as u32)).end..range.end {
            pass.lower(inst, next)?;
        }
    }

    pass.finish(frame_at, most_align)
}

/// Why a signature is outside the subset, if it is.
fn check_sig(rec: SigRec, params: &[ParamRec], rets: &[ParamRec]) -> Result<(), &'static str> {
    if rec.conv != Conv::C as u8 {
        return Err("a calling convention other than the target's C one");
    }
    if rec.bits & SigRec::VARIADIC != 0 {
        return Err("a variadic signature (Apple passes the variable arguments on the stack)");
    }
    if params.len() > 8 {
        return Err("more than 8 parameters (the rest go on the stack)");
    }
    if rets.len() > 1 {
        return Err("more than one return");
    }
    for param in params.iter().chain(rets) {
        if param.kind != ParamKind::Value as u8 {
            return Err("an `sret` or `byval` parameter");
        }
        let ty = Ty(param.ty);
        let fits = ty.is_scalar()
            && matches!(
                ty.elem(),
                Scalar::I1 | Scalar::I8 | Scalar::I16 | Scalar::I32 | Scalar::I64 | Scalar::Ptr
            );
        if !fits {
            return Err("a parameter or return that is not an integer or pointer of at most 64 bits");
        }
    }
    Ok(())
}

fn unsupported(inst: usize, op: Op, why: &'static str) -> EmitError {
    EmitError::Unsupported { inst: inst as u32, op, why }
}

/// The access size of an integer of `width` bits, if it is whole bytes.
fn size_of_width(width: u32) -> Option<Size> {
    match width {
        8 => Some(Size::B),
        16 => Some(Size::H),
        32 => Some(Size::W),
        64 => Some(Size::X),
        _ => None,
    }
}

fn cond_of(cond: IntCond) -> Cond {
    match cond {
        IntCond::Eq => Cond::Eq,
        IntCond::Ne => Cond::Ne,
        IntCond::Ult => Cond::Lo,
        IntCond::Ule => Cond::Ls,
        IntCond::Ugt => Cond::Hi,
        IntCond::Uge => Cond::Hs,
        IntCond::Slt => Cond::Lt,
        IntCond::Sle => Cond::Le,
        IntCond::Sgt => Cond::Gt,
        IntCond::Sge => Cond::Ge,
    }
}

const fn is_signed(cond: IntCond) -> bool {
    matches!(cond, IntCond::Slt | IntCond::Sle | IntCond::Sgt | IntCond::Sge)
}

impl<A: Allocator> Pass<'_, '_, A> {
    fn here(&self) -> u32 {
        self.code.len() as u32
    }

    fn put(&mut self, word: u32) {
        self.code.extend_from_slice(&word.to_le_bytes());
    }

    fn word_at(&self, at: u32) -> u32 {
        let at = at as usize;
        u32::from_le_bytes([self.code[at], self.code[at + 1], self.code[at + 2], self.code[at + 3]])
    }

    fn set(&mut self, at: u32, word: u32) {
        let at = at as usize;
        self.code[at..at + 4].copy_from_slice(&word.to_le_bytes());
    }

    /// `word`, its `field` patched to reach `to` once that is placed.
    fn put_to(&mut self, word: u32, field: Field, to: Label) {
        self.patches.push(Patch { at: self.here(), field, to });
        self.put(word);
    }

    /// Points the forward branch at `at` (an `Imm19` field) here.
    fn land(&mut self, at: u32) -> Result<(), EmitError> {
        let word = Field::Imm19
            .patch(self.word_at(at), i64::from(self.here()) - i64::from(at))
            .ok_or(EmitError::TooLarge { what: "a branch inside one instruction's code (1 MiB)" })?;
        self.set(at, word);
        Ok(())
    }

    /// The width of a value of `ty` in a cell: an integer of at most 64 bits or a pointer.
    fn width(&self, ty: Ty) -> Option<u32> {
        if !ty.is_scalar() {
            return None;
        }
        match ty.elem() {
            Scalar::I1 => Some(1),
            Scalar::I8 => Some(8),
            Scalar::I16 => Some(16),
            Scalar::I32 => Some(32),
            Scalar::I64 => Some(64),
            Scalar::Ptr => Some(self.view.head.ptr_bits),
            Scalar::None | Scalar::I128 | Scalar::F16 | Scalar::F32 | Scalar::F64 | Scalar::F128 => None,
        }
    }

    fn width_of(&self, inst: usize, op: Op, value: Value) -> Result<u32, EmitError> {
        self.width(self.view.ty(value)).ok_or(unsupported(
            inst,
            op,
            "a value that is not an integer or pointer of at most 64 bits (i128, a float, a vector, a compound)",
        ))
    }

    fn result_width(&self, inst: usize, op: Op) -> Result<u32, EmitError> {
        self.width_of(inst, op, Value(inst as u32))
    }

    /// `rd = value`, in the fewest `movz`/`movn` and `movk`.
    fn mov_imm(&mut self, rd: Reg, value: u64) {
        let chunk = |hw: u32| ((value >> (16 * hw)) & 0xffff) as u32;
        let ones = (0..4).filter(|&hw| chunk(hw) == 0xffff).count();
        let zeros = (0..4).filter(|&hw| chunk(hw) == 0).count();
        let (skip, inverted) = match ones > zeros {
            true => (0xffff, true),
            false => (0, false),
        };
        let mut first = true;
        for hw in 0..4 {
            let c = chunk(hw);
            if c == skip {
                continue;
            }
            let word = match (first, inverted) {
                (true, true) => enc::movn(rd, !c & 0xffff, hw),
                (true, false) => enc::movz(rd, c, hw),
                (false, _) => enc::movk(rd, c, hw),
            };
            self.put(word);
            first = false;
        }
        if first {
            self.put(match inverted {
                true => enc::movn(rd, 0, 0),
                false => enc::movz(rd, 0, 0),
            });
        }
    }

    /// `rd = sp + off`, `off` below [`FRAME_LIMIT`].
    fn sp_addr(&mut self, rd: Reg, off: u32) {
        let (hi, lo) = (off >> 12, off & 0xfff);
        match hi {
            0 => self.put(enc::add_imm(rd, SP, lo, false)),
            _ => {
                self.put(enc::add_imm(rd, SP, hi, true));
                if lo != 0 {
                    self.put(enc::add_imm(rd, rd, lo, false));
                }
            }
        }
    }

    /// Loads the 8 bytes at `sp + off` (a multiple of 8).
    fn ld_sp(&mut self, rt: Reg, off: u32) {
        match off / 8 < 4096 {
            true => self.put(enc::ldr(Size::X, rt, SP, off / 8)),
            false => {
                self.sp_addr(X12, off);
                self.put(enc::ldr(Size::X, rt, X12, 0));
            }
        }
    }

    /// Stores `rt` (`xzr` for 31) to the 8 bytes at `sp + off` (a multiple of 8).
    fn st_sp(&mut self, rt: Reg, off: u32) {
        match off / 8 < 4096 {
            true => self.put(enc::str(Size::X, rt, SP, off / 8)),
            false => {
                self.sp_addr(X12, off);
                self.put(enc::str(Size::X, rt, X12, 0));
            }
        }
    }

    /// Loads `value`'s cell.
    fn ld(&mut self, rt: Reg, value: Value) {
        self.ld_sp(rt, value.0 * 16);
    }

    /// Stores `rt` to instruction `inst`'s cell. `rt` holds the value as the cell invariant
    /// says (lib.rs, "Values").
    fn st(&mut self, rt: Reg, inst: usize) {
        self.st_sp(rt, inst as u32 * 16);
    }

    /// Zero-extends the low `width` bits of `reg` over it: the cell invariant.
    fn narrow(&mut self, reg: Reg, width: u32) {
        if width < 64 {
            self.put(enc::zext(reg, reg, width));
        }
    }

    /// Sign-extends the low `width` bits of `reg` over it.
    fn widen_signed(&mut self, reg: Reg, width: u32) {
        if width < 64 {
            self.put(enc::sext(reg, reg, width));
        }
    }

    /// Traps unless `reg` is non-zero.
    fn trap_if_zero(&mut self, reg: Reg) {
        self.put(enc::cbnz(reg, 2));
        self.put(enc::brk(BRK_DIV_ZERO));
    }

    /// A load (`load`) or store of `size` between `rt` and `rn + off`.
    fn mem(&mut self, load: bool, size: Size, rt: Reg, rn: Reg, off: i32) {
        let bytes = 1i32 << size as u32;
        if (-256..256).contains(&off) {
            self.put(match load {
                true => enc::ldur(size, rt, rn, off),
                false => enc::stur(size, rt, rn, off),
            });
        } else if off >= 0 && off % bytes == 0 && off / bytes < 4096 {
            let imm = (off / bytes) as u32;
            self.put(match load {
                true => enc::ldr(size, rt, rn, imm),
                false => enc::str(size, rt, rn, imm),
            });
        } else {
            self.mov_imm(X11, i64::from(off) as u64);
            self.put(enc::add(X11, rn, X11));
            self.put(match load {
                true => enc::ldr(size, rt, X11, 0),
                false => enc::str(size, rt, X11, 0),
            });
        }
    }

    /// The literal word of function reference `r`, made once per place, and how it is called.
    fn literal(&mut self, inst: usize, op: Op, r: RefId) -> Result<(u32, Via), EmitError> {
        let rec = self.view.reference(r);
        if rec.kind != RefKind::Func as u32 {
            return Err(unsupported(inst, op, "a reference to a static or thread-local (only functions are reached by literal words)"));
        }
        let place = rec.place;
        let via = *self.via.get(place as usize).ok_or(EmitError::NoVia { place })?;
        let known = self.ref_literal[r.index()];
        if known != NONE {
            return Ok((known, via));
        }
        let index = match self.fixups.iter().position(|it| it.place == place) {
            Some(it) => it as u32,
            None => {
                self.fixups.push(Fixup { at: 0, place });
                self.fixups.len() as u32 - 1
            }
        };
        self.ref_literal[r.index()] = index;
        Ok((index, via))
    }

    /// Moves `to`'s arguments into its block's parameters (all read before any is written)
    /// and branches there, unless it is `next`, the block placed after this one.
    fn edge(&mut self, to: Target<'_>, next: Option<u32>) -> Result<(), EmitError> {
        let start = self.view.params(to.block).start;
        let n = to.args.len();
        // Parameter `j` is written at step `j`; argument `k` read at step `k` sees it overwritten
        // when it is parameter `j < k`.
        let clobbered = to.args.iter().enumerate().any(|(k, arg)| {
            let at = arg.index();
            at >= start && at < start + n && at - start < k
        });
        match clobbered {
            false => {
                for (k, arg) in to.args.iter().enumerate() {
                    if arg.index() != start + k {
                        self.ld(X9, arg);
                        self.st(X9, start + k);
                    }
                }
            }
            true => {
                let words = u32::try_from(n).map_err(|_| EmitError::TooLarge { what: "a block's arguments" })?;
                if u64::from(self.staging_at) + 8 * u64::from(words) >= FRAME_LIMIT {
                    return Err(EmitError::TooLarge { what: "the frame (16 MiB)" });
                }
                self.staging = self.staging.max(words);
                for (k, arg) in to.args.iter().enumerate() {
                    self.ld(X9, arg);
                    self.st_sp(X9, self.staging_at + 8 * k as u32);
                }
                for k in 0..n {
                    self.ld_sp(X9, self.staging_at + 8 * k as u32);
                    self.st(X9, start + k);
                }
            }
        }
        if next != Some(to.block.0) {
            self.put_to(enc::b(0), Field::Imm26, Label::Block(to.block.0));
        }
        Ok(())
    }

    /// Instruction `inst`, not a parameter. `next` is the block placed after this one.
    fn lower(&mut self, inst: usize, next: Option<u32>) -> Result<(), EmitError> {
        let view = self.view;
        let op = view.op(inst);
        let flags = view.flags(inst);
        // Promises and hints only allow UB or say nothing, so they are dropped; a meaning must
        // be honoured. `volatile` on a load or store is: each is exactly one access of its
        // width, never merged, split or removed.
        let honoured = match op {
            Op::Load | Op::Store => Flags::VOLATILE.0,
            _ => 0,
        };
        if op.info().flags.meanings_only(flags).0 & !honoured != 0 {
            return Err(unsupported(inst, op, "a flag that is a meaning (an atomic ordering, `volatile` on a bulk operation)"));
        }
        let mut o = view.operands(inst);
        match op {
            Op::Iconst => {
                let width = self.result_width(inst, op)?;
                let word = o.imm();
                let value = match width {
                    ..=32 => u64::from(word) & mask(width),
                    _ => i64::from(word as i32) as u64,
                };
                self.constant(inst, value);
            }
            Op::IconstWide => {
                let width = self.result_width(inst, op)?;
                let words = o.constant();
                if width != 64 || words.len() != 2 {
                    return Err(unsupported(inst, op, "a wide constant of other than 64 bits"));
                }
                self.constant(inst, u64::from(words[0]) | (u64::from(words[1]) << 32));
            }
            Op::Null => {
                self.result_width(inst, op)?;
                self.st(ZR, inst);
            }
            Op::Add | Op::Sub | Op::Mul | Op::And | Op::Or | Op::Xor => {
                let width = self.result_width(inst, op)?;
                let (a, b) = (o.value(), o.value());
                self.ld(X9, a);
                self.ld(X10, b);
                self.put(match op {
                    Op::Add => enc::add(X9, X9, X10),
                    Op::Sub => enc::sub(X9, X9, X10),
                    Op::Mul => enc::mul(X9, X9, X10),
                    Op::And => enc::and(X9, X9, X10),
                    Op::Or => enc::orr(X9, X9, X10),
                    _ => enc::eor(X9, X9, X10),
                });
                if matches!(op, Op::Add | Op::Sub | Op::Mul) {
                    self.narrow(X9, width);
                }
                self.st(X9, inst);
            }
            Op::Neg | Op::Not => {
                let width = self.result_width(inst, op)?;
                self.ld(X9, o.value());
                self.put(match op {
                    Op::Neg => enc::neg(X9, X9),
                    _ => enc::mvn(X9, X9),
                });
                self.narrow(X9, width);
                self.st(X9, inst);
            }
            Op::Udiv | Op::Urem => {
                self.result_width(inst, op)?;
                let (a, b) = (o.value(), o.value());
                self.ld(X9, a);
                self.ld(X10, b);
                self.trap_if_zero(X10);
                self.put(enc::udiv(X11, X9, X10));
                if op == Op::Urem {
                    self.put(enc::msub(X11, X11, X10, X9));
                }
                self.st(X11, inst);
            }
            Op::Sdiv | Op::Srem => {
                let width = self.result_width(inst, op)?;
                let (a, b) = (o.value(), o.value());
                self.ld(X9, a);
                self.ld(X10, b);
                self.trap_if_zero(X10);
                self.widen_signed(X9, width);
                self.widen_signed(X10, width);
                if op == Op::Sdiv {
                    // `MIN / -1` traps, as LIR says; `MIN % -1` is 0, which `msub` gives.
                    self.put(enc::cmn_imm(X10, 1));
                    let not_minus_one = self.here();
                    self.put(enc::b_cond(Cond::Ne, 0));
                    let min = match width {
                        64 => i64::MIN,
                        width => -(1i64 << (width - 1)),
                    };
                    self.mov_imm(X11, min as u64);
                    self.put(enc::cmp(X9, X11));
                    let not_min = self.here();
                    self.put(enc::b_cond(Cond::Ne, 0));
                    self.put(enc::brk(BRK_DIV_OVERFLOW));
                    self.land(not_minus_one)?;
                    self.land(not_min)?;
                }
                self.put(enc::sdiv(X11, X9, X10));
                if op == Op::Srem {
                    self.put(enc::msub(X11, X11, X10, X9));
                }
                self.narrow(X11, width);
                self.st(X11, inst);
            }
            Op::Shl | Op::Lshr | Op::Ashr => {
                let width = self.result_width(inst, op)?;
                let (a, b) = (o.value(), o.value());
                self.width_of(inst, op, b)?;
                self.ld(X9, a);
                // The amount is taken modulo the width, a power of two; at 1 bit it is 0.
                if width > 1 {
                    self.ld(X10, b);
                    self.put(enc::zext(X10, X10, width.trailing_zeros()));
                    match op {
                        Op::Shl => self.put(enc::lslv(X9, X9, X10)),
                        Op::Lshr => self.put(enc::lsrv(X9, X9, X10)),
                        _ => {
                            self.widen_signed(X9, width);
                            self.put(enc::asrv(X9, X9, X10));
                        }
                    }
                    self.narrow(X9, width);
                }
                self.st(X9, inst);
            }
            Op::Icmp => {
                self.result_width(inst, op)?;
                let cond = o.int_cond();
                let (a, b) = (o.value(), o.value());
                let width = self.width_of(inst, op, a)?;
                self.ld(X9, a);
                self.ld(X10, b);
                if is_signed(cond) {
                    self.widen_signed(X9, width);
                    self.widen_signed(X10, width);
                }
                self.put(enc::cmp(X9, X10));
                self.put(enc::cset(X9, cond_of(cond)));
                self.st(X9, inst);
            }
            Op::Select => {
                self.result_width(inst, op)?;
                let (cond, a, b) = (o.value(), o.value(), o.value());
                self.width_of(inst, op, cond)?;
                self.ld(X9, cond);
                self.ld(X10, a);
                self.ld(X11, b);
                self.put(enc::cmp_imm(X9, 0));
                self.put(enc::csel(X9, X10, X11, Cond::Ne));
                self.st(X9, inst);
            }
            Op::Zext | Op::Freeze | Op::PtrAddr => {
                self.result_width(inst, op)?;
                let a = o.value();
                self.width_of(inst, op, a)?;
                self.ld(X9, a);
                self.st(X9, inst);
            }
            Op::Sext | Op::Trunc => {
                let width = self.result_width(inst, op)?;
                let a = o.value();
                let from = self.width_of(inst, op, a)?;
                self.ld(X9, a);
                if op == Op::Sext {
                    self.widen_signed(X9, from);
                }
                self.narrow(X9, width);
                self.st(X9, inst);
            }
            Op::SlotAddr => {
                self.result_width(inst, op)?;
                let slot: SlotId = o.slot();
                self.sp_addr(X9, self.slot_at[slot.index()]);
                self.st(X9, inst);
            }
            // A slot's bytes are its own from the prologue to the epilogue.
            Op::SlotLive | Op::SlotDead => {}
            Op::PtrAdd => {
                self.result_width(inst, op)?;
                let (ptr, by) = (o.value(), o.value());
                let width = self.width_of(inst, op, by)?;
                self.ld(X9, ptr);
                self.ld(X10, by);
                self.widen_signed(X10, width);
                self.put(enc::add(X9, X9, X10));
                self.st(X9, inst);
            }
            Op::Load => {
                let width = self.result_width(inst, op)?;
                let size = size_of_width(width)
                    .ok_or(unsupported(inst, op, "a load of an `i1` (memory holds whole bytes)"))?;
                let (ptr, off) = (o.value(), o.offset());
                self.ld(X9, ptr);
                self.mem(true, size, X9, X9, off);
                self.st(X9, inst);
            }
            Op::Store => {
                let (value, ptr, off) = (o.value(), o.value(), o.offset());
                let width = self.width_of(inst, op, value)?;
                let size = size_of_width(width)
                    .ok_or(unsupported(inst, op, "a store of an `i1` (memory holds whole bytes)"))?;
                self.ld(X9, value);
                self.ld(X10, ptr);
                self.mem(false, size, X9, X10, off);
            }
            Op::DataAddr => {
                self.result_width(inst, op)?;
                let rec = view.reference(o.reference());
                if rec.kind != RefKind::Data as u32 {
                    return Err(unsupported(inst, op, "the address of a static (only the function's own data is placed with its code)"));
                }
                let row = rec.place;
                let data = view.datas[row as usize];
                if data.relocs_len != 0 {
                    return Err(unsupported(inst, op, "data with addresses in it (relocations placement would fill)"));
                }
                if data.align > MOST_DATA_ALIGN {
                    return Err(unsupported(inst, op, "data aligned past 16 bytes (placement aligns the code to 16)"));
                }
                let index = match self.data_index[row as usize] {
                    NONE => {
                        self.placed.push(row);
                        let index = self.placed.len() as u32 - 1;
                        self.data_index[row as usize] = index;
                        index
                    }
                    index => index,
                };
                self.put_to(enc::adr(X9, 0), Field::Adr, Label::Data(index));
                self.st(X9, inst);
            }
            Op::FuncAddr => {
                self.result_width(inst, op)?;
                let (literal, via) = self.literal(inst, op, o.reference())?;
                if via == Via::Slot {
                    return Err(unsupported(
                        inst,
                        op,
                        "the address of a function called through a launcher slot (its literal holds the slot word's address, not the function's)",
                    ));
                }
                self.put_to(enc::ldr_lit(X9, 0), Field::Imm19, Label::Literal(literal));
                self.st(X9, inst);
            }
            Op::Call => self.call(inst, op)?,
            Op::Jump => self.edge(o.target(), next)?,
            Op::Br => {
                let cond = o.value();
                let (then, otherwise) = (o.target(), o.target());
                self.ld(X9, cond);
                match then.args.is_empty() {
                    true => self.put_to(enc::cbnz(X9, 0), Field::Imm19, Label::Block(then.block.0)),
                    false => {
                        let at = self.here();
                        self.put(enc::cbz(X9, 0));
                        self.edge(then, None)?;
                        self.land(at)?;
                    }
                }
                self.edge(otherwise, next)?;
            }
            Op::Switch => {
                let scrutinee = o.value();
                self.width_of(inst, op, scrutinee)?;
                let default = o.target();
                self.ld(X9, scrutinee);
                for (words, to) in o.cases(scrutinee) {
                    let value = words.iter().rev().fold(0u64, |it, &word| (it << 32) | u64::from(word));
                    match value < 4096 {
                        true => self.put(enc::cmp_imm(X9, value as u32)),
                        false => {
                            self.mov_imm(X10, value);
                            self.put(enc::cmp(X9, X10));
                        }
                    }
                    match to.args.is_empty() {
                        true => self.put_to(enc::b_cond(Cond::Eq, 0), Field::Imm19, Label::Block(to.block.0)),
                        false => {
                            let at = self.here();
                            self.put(enc::b_cond(Cond::Ne, 0));
                            self.edge(to, None)?;
                            self.land(at)?;
                        }
                    }
                }
                self.edge(default, next)?;
            }
            Op::Return => {
                let values = o.values();
                if values.len() != self.rets.len() {
                    return Err(unsupported(inst, op, "a return of other than the signature's values"));
                }
                if let (Some(value), Some(ret)) = (values.iter().next(), self.rets.first()) {
                    let width = self.width_of(inst, op, value)?;
                    self.ld(X0, value);
                    // Apple's convention: a sub-32-bit return is extended as the signature says.
                    if ret.ext == Ext::Sext as u8 {
                        self.widen_signed(X0, width);
                    }
                }
                self.put(enc::add_imm(SP, FP, 0, false));
                self.put(enc::ldp_post(FP, LR, SP, 16));
                self.put(enc::ret());
            }
            Op::Trap => {
                let code = o.imm();
                if code >= BRK_DIV_ZERO {
                    return Err(unsupported(inst, op, "a trap code from 0xff00 up (the emitter's own `brk` codes)"));
                }
                self.put(enc::brk(code));
            }
            Op::Unreachable => self.put(enc::brk(BRK_UNREACHABLE)),
            Op::Param => return Err(unsupported(inst, op, "a parameter after a block's first instruction")),
            _ => return Err(unsupported(inst, op, "an operation outside the baseline tier's integer subset")),
        }
        Ok(())
    }

    /// Stores the constant `value` (the cell invariant's bits) to `inst`'s cell.
    fn constant(&mut self, inst: usize, value: u64) {
        match value {
            0 => self.st(ZR, inst),
            _ => {
                self.mov_imm(X9, value);
                self.st(X9, inst);
            }
        }
    }

    /// A `call`: AAPCS64 with Apple's rule that the caller extends a sub-32-bit argument as
    /// its parameter's `ext` says (a cell's value is already zero-extended to 64 bits).
    fn call(&mut self, inst: usize, op: Op) -> Result<(), EmitError> {
        let view = self.view;
        let mut o = view.operands(inst);
        let (callee, sig_id, args) = (o.reference(), o.sig(), o.values());
        let sig = view.sig(sig_id);
        check_sig(view.sigs[sig_id.index()], sig.params, sig.rets).map_err(|why| unsupported(inst, op, why))?;
        if args.len() != sig.params.len() {
            return Err(unsupported(inst, op, "arguments other than the signature's parameters"));
        }
        let (literal, via) = self.literal(inst, op, callee)?;
        for (reg, (arg, param)) in args.iter().zip(sig.params).enumerate() {
            let reg = reg as Reg;
            self.ld(reg, arg);
            if param.ext == Ext::Sext as u8 {
                let width = self.width_of(inst, op, arg)?;
                self.widen_signed(reg, width);
            }
        }
        self.put_to(enc::ldr_lit(X16, 0), Field::Imm19, Label::Literal(literal));
        if via == Via::Slot {
            self.put(enc::ldr(Size::X, X16, X16, 0));
        }
        self.put(enc::blr(X16));
        if let Some(ret) = sig.rets.first() {
            let width = self.width(Ty(ret.ty)).ok_or(unsupported(inst, op, "a return wider than 64 bits"))?;
            self.narrow(X0, width);
            self.st(X0, inst);
        }
        Ok(())
    }

    /// Places the literal words and the data after the code, patches every field and the
    /// frame's size.
    fn finish(mut self, frame_at: u32, most_align: u64) -> Result<(), EmitError> {
        let too_far = EmitError::TooLarge { what: "a branch or PC-relative reference past its reach" };
        self.pad_to(8);
        let literals_at = self.here();
        for at in 0..self.fixups.len() {
            self.fixups[at].at = self.here();
            self.code.extend_from_slice(&[0; 8]);
        }
        let mut data_at = Vec::with_capacity_in(self.placed.len(), Global);
        for at in 0..self.placed.len() {
            let row = self.placed[at];
            let rec = self.view.datas[row as usize];
            self.pad_to(8usize.max(1 << rec.align));
            data_at.push(self.here());
            self.code.extend_from_slice(self.view.data_bytes_of(DataId(row)));
        }
        if self.code.len() > u32::MAX as usize / 2 {
            return Err(EmitError::TooLarge { what: "the code (2 GiB)" });
        }
        let patches = core::mem::replace(&mut self.patches, Vec::new_in(Global));
        for patch in patches.iter() {
            let to = match patch.to {
                Label::Block(block) => self.block_at[block as usize],
                Label::Literal(index) => literals_at + 8 * index,
                Label::Data(index) => data_at[index as usize],
            };
            let word = patch
                .field
                .patch(self.word_at(patch.at), i64::from(to) - i64::from(patch.at))
                .ok_or(too_far)?;
            let at = patch.at as usize;
            self.code[at..at + 4].copy_from_slice(&word.to_le_bytes());
        }
        let used = u64::from(self.staging_at) + 8 * u64::from(self.staging);
        let frame = used.next_multiple_of(16) + (most_align - 16);
        if frame >= FRAME_LIMIT {
            return Err(EmitError::TooLarge { what: "the frame (16 MiB)" });
        }
        let frame = frame as u32;
        self.set(frame_at, enc::sub_imm(SP, SP, frame >> 12, true));
        self.set(frame_at + 4, enc::sub_imm(SP, SP, frame & 0xfff, false));
        Ok(())
    }

    fn pad_to(&mut self, align: usize) {
        let len = self.code.len().next_multiple_of(align);
        self.code.resize(len, 0);
    }
}

const fn mask(width: u32) -> u64 {
    match width {
        64.. => u64::MAX,
        width => (1 << width) - 1,
    }
}
