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
    EmitError, Fixup, Isa, Reloc, RelocTo, Via,
    enc::{self, Cond, FP, Field, LR, Reg, SP, Size, X0, X1, X8, X9, X10, X11, X12, X16, ZR},
};

mod atomic;
mod bulk;
mod float;
mod int;
mod runtime;
pub(crate) mod sites;
mod tls;
mod wide;

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
    /// The literal word placement fills for `words[i]`.
    Word(u32),
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
    isa: Isa,
    /// The most bytes a bulk operation of a constant length is inline.
    inline_most: u64,
    code: &'c mut Vec<u8, A>,
    /// One per literal word, in order; `at` is filled when the words are placed.
    fixups: &'c mut Vec<Fixup, A>,
    /// The absolute address words, filled as the literal words and data are placed.
    relocs: &'c mut Vec<Reloc, A>,
    /// The literal words placement fills, other than the fixups': each once, in order.
    words: Vec<RelocTo, Global>,
    /// The `add`s that make an address in the stretch of the frame where a call's `byval`
    /// copies go, placed after the staging words once their count is known: (the first of
    /// the two words, its register, the offset in the stretch).
    byval_fixes: Vec<(u32, Reg, u32), Global>,
    /// The most bytes one call's `byval` copies took.
    byval_most: u32,
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
    /// The record form's rows (pass/sites.rs): `Some` makes every outside reference an
    /// import site and places no address word; `None` is the literal-word form.
    record: Option<&'c mut sites::Form<A>>,
}

/// Runs the pass (see [`crate::emit`]).
pub(crate) fn run<A: Allocator>(
    view: View<'_>,
    via: &[Via],
    isa: Isa,
    inline_most: u64,
    bytes: &mut Vec<u8, A>,
    fixups: &mut Vec<Fixup, A>,
    relocs: &mut Vec<Reloc, A>,
    record: Option<&mut sites::Form<A>>,
) -> Result<(), EmitError> {
    if view.head.ptr_bits != 64 {
        return Err(EmitError::Signature { why: "a target whose pointers are not 64 bits" });
    }
    let sig = view.sigs.get(view.head.sig as usize).map(|_| view.sig(SigId(view.head.sig)));
    let Some(sig) = sig else { return Err(EmitError::Signature { why: "no signature row" }) };
    check_sig(view, isa, view.sigs[view.head.sig as usize], sig.params, sig.rets)
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
        isa,
        inline_most,
        code: bytes,
        fixups,
        relocs,
        words: Vec::new_in(Global),
        byval_fixes: Vec::new_in(Global),
        byval_most: 0,
        rets: sig.rets,
        patches: Vec::new_in(Global),
        block_at,
        ref_literal,
        data_index,
        placed: Vec::new_in(Global),
        slot_at,
        staging_at: staging_at as u32,
        staging: 0,
        record,
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
    // The parameters arrive in x0..x7, their bits above the width unspecified; an `sret`
    // pointer in x8, a `byval` one as the next integer parameter.
    // A float in the next `v` register, an `i128` in an even pair (`Regs`).
    let mut regs = Regs::new(isa.os);
    for (inst, param) in view.params(Block(0)).zip(sig.params) {
        let ty = view.ty(Value(inst as u32));
        let reg = match param.kind == ParamKind::Sret as u8 {
            true => X8,
            false => match regs.next(ty) {
                Some(Loc::X(r)) => r,
                Some(Loc::Pair(r)) => {
                    pass.st2(r, r + 1, inst);
                    continue;
                }
                Some(Loc::V(v)) => {
                    pass.float_in(v, inst, ty == Ty::F64);
                    continue;
                }
                None => return Err(EmitError::Signature { why: "an entry parameter past the registers" }),
            },
        };
        let width = pass.width(ty).ok_or(EmitError::Signature {
            why: "an entry parameter that is not an integer, a pointer, an `f32`, an `f64` or an `i128`",
        })?;
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
fn check_sig(view: View<'_>, isa: Isa, rec: SigRec, params: &[ParamRec], rets: &[ParamRec]) -> Result<(), &'static str> {
    if rec.conv != Conv::C as u8 {
        return Err("a calling convention other than the target's C one");
    }
    if rec.bits & SigRec::VARIADIC != 0 {
        return Err("a variadic signature (Apple passes the variable arguments on the stack)");
    }
    let srets = params.iter().filter(|it| it.kind == ParamKind::Sret as u8).count();
    if srets > 1 {
        return Err("more than one `sret` parameter");
    }
    let mut regs = Regs::new(isa.os);
    for param in params.iter().filter(|it| it.kind != ParamKind::Sret as u8) {
        if regs.next(Ty(param.ty)).is_none() {
            return Err("more than 8 integer or 8 float parameters past an `sret` (the rest go on the stack)");
        }
    }
    let mut regs = Regs::new(isa.os);
    for ret in rets {
        match regs.next(Ty(ret.ty)) {
            Some(Loc::X(r)) if r < 2 => {}
            Some(Loc::Pair(0)) => {}
            Some(Loc::V(v)) if v < 4 => {}
            _ => return Err("returns past `x0:x1` and `v0..v3` (a composite past 16 bytes returns through `sret`)"),
        }
    }
    for param in params {
        match ParamKind::from_byte(param.kind) {
            Some(ParamKind::Value) => {}
            Some(ParamKind::Sret) if Ty(param.ty) == Ty::PTR => {}
            Some(ParamKind::Byval) if Ty(param.ty) == Ty::PTR => {
                byval_of(view, param)?;
            }
            _ => return Err("an `sret` or `byval` parameter that is not a pointer"),
        }
    }
    if rets.iter().any(|it| it.kind != ParamKind::Value as u8) {
        return Err("a return passed other than by value");
    }
    for param in params.iter().chain(rets) {
        let ty = Ty(param.ty);
        let fits = ty.is_scalar()
            && matches!(
                ty.elem(),
                Scalar::I1
                    | Scalar::I8
                    | Scalar::I16
                    | Scalar::I32
                    | Scalar::I64
                    | Scalar::I128
                    | Scalar::Ptr
                    | Scalar::F32
                    | Scalar::F64
            );
        if !fits {
            return Err("a parameter or return that is not an integer, a pointer, an `f32` or an `f64`");
        }
    }
    Ok(())
}

/// Where one parameter or return goes (AAPCS64 C.1..C.16): the next `x` register, an `i128`'s
/// even-numbered pair of `x` registers (C.8: "rounded up to the next even number"), or the
/// next vector register for a float (C.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Loc {
    X(Reg),
    Pair(Reg),
    V(u32),
}

/// The next general and vector registers of one signature walk.
pub(crate) struct Regs {
    x: Reg,
    v: u32,
    /// AAPCS64 C.8 rounds an `i128`'s first register up to even; Apple's variant does not
    /// (measured: num_tests.rs, the host's `extern "C" fn(u128, u64, u128)` passes the second
    /// `u128` in `x3:x4` on macOS).
    even: bool,
}

impl Regs {
    pub(crate) fn new(os: crate::Os) -> Regs {
        Regs { x: 0, v: 0, even: os != crate::Os::MacOs }
    }

    /// The place of the next value of `ty`, or `None` past `x7` or `v7` (the stack).
    pub(crate) fn next(&mut self, ty: Ty) -> Option<Loc> {
        if ty.is_scalar() && matches!(ty.elem(), Scalar::F32 | Scalar::F64) {
            let v = self.v;
            self.v += 1;
            return (v < 8).then_some(Loc::V(v));
        }
        if ty == Ty::I128 {
            let x = if self.even { self.x.next_multiple_of(2) } else { self.x };
            self.x = x + 2;
            return (x + 1 < 8).then_some(Loc::Pair(x));
        }
        let x = self.x;
        self.x += 1;
        (x < 8).then_some(Loc::X(x))
    }
}

/// The most alignment a `byval` copy may ask: what `sp` has.
const MOST_BYVAL_ALIGN: u8 = 4;

/// A `byval` parameter's pointee: its size and log2 alignment, from its facts.
fn byval_of(view: View<'_>, param: &ParamRec) -> Result<(u32, u8), &'static str> {
    let Some(facts) = view.facts(param) else { return Err("a `byval` parameter whose size is not told") };
    if facts.align > MOST_BYVAL_ALIGN {
        return Err("a `byval` parameter aligned past 16 bytes");
    }
    Ok((facts.size, facts.align))
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

    /// Data row `row`'s index among the rows placed after the code, placed once.
    fn place_data(&mut self, row: u32) -> u32 {
        match self.data_index[row as usize] {
            NONE => {
                self.placed.push(row);
                let index = self.placed.len() as u32 - 1;
                self.data_index[row as usize] = index;
                index
            }
            index => index,
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

    /// `ldr rt, <the literal word placement fills for to>`, the word made once per `to`.
    fn ld_word(&mut self, rt: Reg, to: RelocTo) {
        if self.in_record() {
            // The record form: the word is `to`'s import word, loaded by a site.
            return self.import_load(rt, to);
        }
        let index = match self.words.iter().position(|&it| it == to) {
            Some(it) => it as u32,
            None => {
                self.words.push(to);
                self.words.len() as u32 - 1
            }
        };
        self.put_to(enc::ldr_lit(rt, 0), Field::Imm19, Label::Word(index));
    }

    /// Calls the system routine `routine` with its arguments already in their registers
    /// (`x0..x3`, `d0`/`d1`); it may take every caller-saved register, which nothing holds
    /// across an instruction (every value lives in its cell).
    fn call_routine(&mut self, routine: crate::Routine) {
        match self.in_record() {
            // The record form: a call site, which the `blr` completes.
            true => self.import_callee(RelocTo::Routine(routine)),
            false => self.ld_word(X16, RelocTo::Routine(routine)),
        }
        self.put(enc::blr(X16));
    }

    /// Whether a value of `ty` is a pair: a compound of two integers or pointers of at most
    /// 64 bits (lib.rs, "Pairs").
    fn is_pair(&self, ty: Ty) -> bool {
        let members = self.view.members(ty);
        ty.compound_row().is_some() && members.len() == 2 && members.iter().all(|&it| self.width(Ty(it)).is_some())
    }

    /// How many 8-byte words of its cell a value of `ty` takes: 2 for a pair, else 1.
    fn words_of(&self, value: Value) -> u32 {
        let ty = self.view.ty(value);
        match ty.compound_row() {
            Some(_) => 2,
            None if Self::is_wide(ty) => 2,
            None => 1,
        }
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
        // A pair moves both of its words.
        match clobbered {
            false => {
                for (k, arg) in to.args.iter().enumerate() {
                    if arg.index() != start + k {
                        for w in 0..self.words_of(arg) {
                            self.ld_sp(X9, arg.0 * 16 + 8 * w);
                            self.st_sp(X9, (start + k) as u32 * 16 + 8 * w);
                        }
                    }
                }
            }
            true => {
                let words = u32::try_from(2 * n).map_err(|_| EmitError::TooLarge { what: "a block's arguments" })?;
                if u64::from(self.staging_at) + 8 * u64::from(words) >= FRAME_LIMIT {
                    return Err(EmitError::TooLarge { what: "the frame (16 MiB)" });
                }
                self.staging = self.staging.max(words);
                for (k, arg) in to.args.iter().enumerate() {
                    for w in 0..self.words_of(arg) {
                        self.ld_sp(X9, arg.0 * 16 + 8 * w);
                        self.st_sp(X9, self.staging_at + 16 * k as u32 + 8 * w);
                    }
                }
                for (k, arg) in to.args.iter().enumerate() {
                    for w in 0..self.words_of(arg) {
                        self.ld_sp(X9, self.staging_at + 16 * k as u32 + 8 * w);
                        self.st_sp(X9, (start + k) as u32 * 16 + 8 * w);
                    }
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
        // An atomic's ordering is honoured (raised where the machine has no weaker form), a
        // weak `cmpxchg` made strong, a single-thread fence widened to nothing more than
        // program order (lib.rs, "Atomics").
        const ORDERING: u8 = 0x7;
        const ORDERINGS: u8 = 0x3f;
        let honoured = match op {
            Op::Load | Op::Store => Flags::VOLATILE.0,
            Op::AtomicLoad | Op::AtomicStore | Op::AtomicRmw => ORDERING | Flags::VOLATILE.0,
            Op::Cmpxchg => ORDERINGS | Flags::WEAK.0 | Flags::VOLATILE.0,
            Op::Fence => ORDERING | Flags::SINGLE_THREAD.0,
            _ => 0,
        };
        if op.info().flags.meanings_only(flags).0 & !honoured != 0 {
            return Err(unsupported(inst, op, "a flag that is a meaning it does not honour (`volatile` on a bulk operation)"));
        }
        // Floats (pass/float.rs) and `i128` (pass/wide.rs), by the type they compute in.
        let result = view.ty(Value(inst as u32));
        let first = |view: View<'_>| view.operands(inst).value();
        match op {
            Op::Fconst
            | Op::FconstWide
            | Op::Fadd
            | Op::Fsub
            | Op::Fmul
            | Op::Fdiv
            | Op::Frem
            | Op::Fneg
            | Op::Fcmp
            | Op::Fpext
            | Op::Fptrunc
            | Op::FptoUi
            | Op::FptoSi
            | Op::UitoFp
            | Op::SitoFp
            | Op::Bitcast => return self.float(inst, op),
            Op::Load if Self::float_ty(result).is_some() => return self.float(inst, op),
            Op::Store if Self::float_ty(view.ty(first(view))).is_some() => return self.float(inst, op),
            Op::Load
            | Op::Iconst
            | Op::IconstWide
            | Op::Add
            | Op::Sub
            | Op::Mul
            | Op::And
            | Op::Or
            | Op::Xor
            | Op::Neg
            | Op::Not
            | Op::Shl
            | Op::Lshr
            | Op::Ashr
            | Op::Udiv
            | Op::Sdiv
            | Op::Urem
            | Op::Srem
            | Op::Zext
            | Op::Sext
            | Op::Freeze
            | Op::Select
                if Self::is_wide(result) =>
            {
                return self.wide(inst, op);
            }
            Op::Store | Op::Trunc if Self::is_wide(view.ty(first(view))) => return self.wide(inst, op),
            Op::Icmp => {
                let mut ops = view.operands(inst);
                ops.int_cond();
                if Self::is_wide(view.ty(ops.value())) {
                    return self.wide(inst, op);
                }
            }
            _ => {}
        }
        let mut o = view.operands(inst);
        match op {
            // Unspecified bits: the value's cell is left as it is. No code, and correct
            // whatever the type is: any bits are a valid `uninit`.
            Op::Uninit => {}
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
            Op::Zext | Op::Freeze | Op::PtrAddr | Op::Expose | Op::FromExposed => {
                self.result_width(inst, op)?;
                let a = o.value();
                self.width_of(inst, op, a)?;
                self.ld(X9, a);
                self.st(X9, inst);
            }
            Op::PtrWithAddr => {
                self.result_width(inst, op)?;
                let (_, addr) = (o.value(), o.value());
                self.ld(X9, addr);
                self.st(X9, inst);
            }
            Op::Project => {
                let (pair, k) = (o.value(), o.imm());
                self.result_width(inst, op)?;
                if !self.is_pair(view.ty(pair)) || k > 1 {
                    return Err(unsupported(inst, op, "a member of a compound that is not a pair of integers or pointers"));
                }
                self.ld_sp(X9, pair.0 * 16 + 8 * k);
                self.st(X9, inst);
            }
            Op::Rotl
            | Op::Rotr
            | Op::Umulhi
            | Op::Smulhi
            | Op::UaddSat
            | Op::SaddSat
            | Op::UsubSat
            | Op::SsubSat
            | Op::Umin
            | Op::Umax
            | Op::Smin
            | Op::Smax => self.int_binary(inst, op)?,
            Op::UaddOverflow
            | Op::SaddOverflow
            | Op::UsubOverflow
            | Op::SsubOverflow
            | Op::UmulOverflow
            | Op::SmulOverflow => self.overflow(inst, op)?,
            Op::Clz | Op::Ctz | Op::Popcnt | Op::Bswap | Op::Bitrev => self.bits(inst, op)?,
            Op::AtomicLoad | Op::AtomicStore | Op::AtomicRmw | Op::Cmpxchg | Op::Fence => self.atomic(inst, op)?,
            Op::Memcpy | Op::Memmove | Op::Memset => self.bulk(inst, op)?,
            Op::TlsAddr => self.tls_addr(inst, op)?,
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
                if rec.kind == RefKind::Static as u32 {
                    // A static's address is a literal word placement fills (`Reloc`).
                    self.ld_word(X9, RelocTo::Static(rec.place));
                    self.st(X9, inst);
                    return Ok(());
                }
                if rec.kind == RefKind::Vtable as u32 {
                    // A vtable's address is a literal word placement fills, as a static's.
                    self.ld_word(X9, RelocTo::Vtable(rec.place));
                    self.st(X9, inst);
                    return Ok(());
                }
                if rec.kind != RefKind::Data as u32 {
                    return Err(unsupported(inst, op, "`data_addr` of other than a static, a vtable or the function's own data"));
                }
                let row = rec.place;
                let data = view.datas[row as usize];
                if data.align > MOST_DATA_ALIGN {
                    return Err(unsupported(inst, op, "data aligned past 16 bytes (placement aligns the code to 16)"));
                }
                let index = self.place_data(row);
                if self.in_record() && !view.data_relocs_of(DataId(row)).is_empty() {
                    // The record form: a row holding an address lives in the relro part.
                    self.relro_addr(X9, index);
                    self.st(X9, inst);
                    return Ok(());
                }
                self.put_to(enc::adr(X9, 0), Field::Adr, Label::Data(index));
                self.st(X9, inst);
            }
            Op::FuncAddr => {
                self.result_width(inst, op)?;
                let reference = o.reference();
                let place = view.reference(reference).place;
                match (self.in_record(), self.via.get(place as usize)) {
                    // The record form: the function's import word holds its address.
                    (true, _) => {
                        let place = self.func_place(inst, op, reference)?;
                        self.import_load(X9, RelocTo::Func(place));
                    }
                    // A slot function's address is its slot stub's, stable across patches.
                    (false, Some(Via::Slot)) => self.ld_word(X9, RelocTo::SlotStub(place)),
                    (false, _) => {
                        let (literal, _) = self.literal(inst, op, reference)?;
                        self.put_to(enc::ldr_lit(X9, 0), Field::Imm19, Label::Literal(literal));
                    }
                }
                self.st(X9, inst);
            }
            Op::Call | Op::CallIndirect => self.call(inst, op)?,
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
                let rets = self.rets;
                let mut regs = Regs::new(self.isa.os);
                for (value, ret) in values.iter().zip(rets) {
                    let reg = match regs.next(Ty(ret.ty)) {
                        Some(Loc::X(reg)) => reg,
                        Some(Loc::Pair(reg)) => {
                            self.ld2(reg, reg + 1, value);
                            continue;
                        }
                        Some(Loc::V(v)) => {
                            self.float_out(v, value, Ty(ret.ty) == Ty::F64);
                            continue;
                        }
                        None => return Err(unsupported(inst, op, "a return past the registers")),
                    };
                    let width = self.width_of(inst, op, value)?;
                    self.ld(reg, value);
                    // Apple's convention: a sub-32-bit return is extended as the signature says.
                    if ret.ext == Ext::Sext as u8 {
                        self.widen_signed(reg, width);
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
            // A promise only allows UB, so it is dropped (as `Freeze`'s flags are).
            Op::Assume => {}
            // Every op is named here, never by `_`, so an op added to LIR is a compile error
            // in this match until it has an emit or a named refusal (the class: the walk making
            // an instruction the emitter cannot lower, found three crates up; rule 54).
            Op::Fconst
            | Op::FconstWide
            | Op::Fadd
            | Op::Fsub
            | Op::Fmul
            | Op::Fdiv
            | Op::Frem
            | Op::FminNum
            | Op::FmaxNum
            | Op::Fminimum
            | Op::Fmaximum
            | Op::Fcopysign
            | Op::Fneg
            | Op::Fabs
            | Op::Sqrt
            | Op::Floor
            | Op::Ceil
            | Op::Ftrunc
            | Op::Nearest
            | Op::Fround
            | Op::Fma
            | Op::Fmuladd
            | Op::Fcmp
            | Op::Fpext
            | Op::Fptrunc
            | Op::FptoUi
            | Op::FptoSi
            | Op::UitoFp
            | Op::SitoFp
            | Op::Bitcast => return Err(unsupported(inst, op, "a float operation past the router (pass/float.rs)")),
            Op::Vconst
            | Op::Splat
            | Op::Extract
            | Op::Insert
            | Op::Shuffle
            | Op::Swizzle
            | Op::ReduceAdd
            | Op::ReduceMul
            | Op::ReduceAnd
            | Op::ReduceOr
            | Op::ReduceXor
            | Op::ReduceUmin
            | Op::ReduceUmax
            | Op::ReduceSmin
            | Op::ReduceSmax
            | Op::ReduceFadd
            | Op::ReduceFmul
            | Op::ReduceFmin
            | Op::ReduceFmax
            | Op::Bitmask
            | Op::MaskedLoad
            | Op::MaskedStore
            | Op::Gather
            | Op::Scatter => return Err(unsupported(inst, op, "a vector operation")),
            Op::Invoke | Op::InvokeIndirect | Op::Resume => return Err(unsupported(inst, op, "unwinding")),
            Op::TailCall | Op::TailCallIndirect => return Err(unsupported(inst, op, "a tail call")),
            Op::Asm | Op::AsmBr => return Err(unsupported(inst, op, "inline assembly")),
            // A coroutine body is split by `blaze_lir::coroutine::split` before any backend
            // (decision D73); its two outputs hold neither operation.
            Op::Suspend | Op::Cancelled => {
                return Err(unsupported(inst, op, "a suspend reached the emitter: the coroutine pass did not run"));
            }
            Op::TargetIntrinsic => return Err(unsupported(inst, op, "a target intrinsic")),
            Op::FrameAddress | Op::ReturnAddress | Op::StackPointer => {
                return Err(unsupported(inst, op, "a machine register read (frame or return address, stack pointer)"));
            }
            Op::Memcmp => return Err(unsupported(inst, op, "`memcmp`")),
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

    /// A `call` or `call_indirect`: AAPCS64 with Apple's rule that the caller extends a
    /// sub-32-bit argument as its parameter's `ext` says (a cell's value is already
    /// zero-extended to 64 bits). An `sret` argument goes in x8; a `byval` one is copied into
    /// the frame first (a copy may itself call `memcpy`, so every copy is made before any
    /// argument register is loaded) and its copy's address passed.
    fn call(&mut self, inst: usize, op: Op) -> Result<(), EmitError> {
        let view = self.view;
        let mut o = view.operands(inst);
        let (direct, indirect) = match op {
            Op::Call => (Some(o.reference()), None),
            _ => (None, Some(o.value())),
        };
        let (sig_id, args) = (o.sig(), o.values());
        let sig = view.sig(sig_id);
        let isa = self.isa;
        check_sig(view, isa, view.sigs[sig_id.index()], sig.params, sig.rets).map_err(|why| unsupported(inst, op, why))?;
        if args.len() != sig.params.len() {
            return Err(unsupported(inst, op, "arguments other than the signature's parameters"));
        }
        // The record form calls through the callee's import word; the literal-word form through
        // its literal.
        let imported = match direct {
            Some(reference) if self.in_record() => Some(self.func_place(inst, op, reference)?),
            _ => None,
        };
        let callee = match direct {
            Some(reference) if imported.is_none() => Some(self.literal(inst, op, reference)?),
            _ => None,
        };
        // The copies, each at its alignment in this call's stretch of the frame.
        let mut copies: Vec<u32, Global> = Vec::new_in(Global);
        let mut area = 0u32;
        for (arg, param) in args.iter().zip(sig.params) {
            if param.kind != ParamKind::Byval as u8 {
                continue;
            }
            let (size, align) = byval_of(view, param).map_err(|why| unsupported(inst, op, why))?;
            area = area.next_multiple_of(1 << align);
            copies.push(area);
            self.byval_addr(X9, area);
            self.ld(X10, arg);
            self.copy(X9, X10, u64::from(size));
            area = area.checked_add(size).ok_or(EmitError::TooLarge { what: "a call's `byval` copies" })?;
        }
        self.byval_most = self.byval_most.max(area);
        let mut regs = Regs::new(isa.os);
        let mut copy = copies.iter();
        for (arg, param) in args.iter().zip(sig.params) {
            if param.kind == ParamKind::Sret as u8 {
                self.ld(X8, arg);
                continue;
            }
            let at = regs.next(Ty(param.ty)).ok_or(unsupported(inst, op, "an argument past the registers"))?;
            match (ParamKind::from_byte(param.kind), at) {
                (Some(ParamKind::Byval), Loc::X(gpr)) => {
                    let Some(&at) = copy.next() else {
                        return Err(unsupported(inst, op, "a `byval` argument without its copy"));
                    };
                    self.byval_addr(gpr, at);
                }
                (_, Loc::X(gpr)) => {
                    self.ld(gpr, arg);
                    if param.ext == Ext::Sext as u8 {
                        let width = self.width_of(inst, op, arg)?;
                        self.widen_signed(gpr, width);
                    }
                }
                (_, Loc::Pair(gpr)) => self.ld2(gpr, gpr + 1, arg),
                (_, Loc::V(v)) => self.float_out(v, arg, Ty(param.ty) == Ty::F64),
            }
        }
        match (callee, indirect) {
            (Some((literal, via)), _) => {
                self.put_to(enc::ldr_lit(X16, 0), Field::Imm19, Label::Literal(literal));
                if via == Via::Slot {
                    self.put(enc::ldr(Size::X, X16, X16, 0));
                }
            }
            (None, Some(pointer)) => self.ld(X16, pointer),
            (None, None) => match imported {
                // A call site, which the `blr` below completes.
                Some(place) => self.import_callee(RelocTo::Func(place)),
                None => return Err(unsupported(inst, op, "a call that names no callee")),
            },
        }
        self.put(enc::blr(X16));
        // Two returns are a pair in one cell (lib.rs, "Pairs"); one `i128` or float is the cell.
        let mut regs = Regs::new(isa.os);
        for (k, ret) in sig.rets.iter().enumerate() {
            let ty = Ty(ret.ty);
            match regs.next(ty).ok_or(unsupported(inst, op, "a return past the registers"))? {
                Loc::X(reg) => {
                    let width = self.width(ty).ok_or(unsupported(inst, op, "a return wider than 64 bits"))?;
                    self.narrow(reg, width);
                    self.st_sp(reg, inst as u32 * 16 + 8 * k as u32);
                }
                Loc::Pair(reg) if sig.rets.len() == 1 => self.st2(reg, reg + 1, inst),
                Loc::V(v) if sig.rets.len() == 1 => self.float_in(v, inst, ty == Ty::F64),
                _ => return Err(unsupported(inst, op, "an `i128` or float return beside another return")),
            }
        }
        Ok(())
    }

    /// `rd = ` the address of byte `off` of the frame's stretch for `byval` copies, two `add`s
    /// patched once the stretch's place is known.
    fn byval_addr(&mut self, rd: Reg, off: u32) {
        self.byval_fixes.push((self.here(), rd, off));
        self.put(enc::add_imm(rd, SP, 0, true));
        self.put(enc::add_imm(rd, rd, 0, false));
    }

    /// Places the literal words and the data after the code, patches every field and the
    /// frame's size.
    fn finish(mut self, frame_at: u32, most_align: u64) -> Result<(), EmitError> {
        if self.in_record() {
            // The record form places no literal word (pass/sites.rs).
            return self.finish_record(frame_at, most_align);
        }
        self.pad_to(8);
        let literals_at = self.here();
        for at in 0..self.fixups.len() {
            self.fixups[at].at = self.here();
            self.code.extend_from_slice(&[0; 8]);
        }
        let words_at = self.here();
        for (i, &to) in self.words.iter().enumerate() {
            self.relocs.push(Reloc { at: words_at + 8 * i as u32, to, addend: 0 });
        }
        self.code.resize(self.code.len() + 8 * self.words.len(), 0);
        // Data rows, then the rows their relocations reach (the list grows as it is walked).
        let mut data_at = Vec::with_capacity_in(self.placed.len(), Global);
        // (word, index of the data row it reaches, addend), resolved once every row is placed.
        let mut inner: Vec<(u32, u32, i64), Global> = Vec::new_in(Global);
        let mut at = 0;
        while at < self.placed.len() {
            let row = self.placed[at];
            let rec = self.view.datas[row as usize];
            if rec.align > MOST_DATA_ALIGN {
                return Err(EmitError::Data { row, why: "data aligned past 16 bytes (placement aligns the code to 16)" });
            }
            self.pad_to(8usize.max(1 << rec.align));
            let start = self.here();
            data_at.push(start);
            self.code.extend_from_slice(self.view.data_bytes_of(DataId(row)));
            for r in self.view.data_relocs_of(DataId(row)) {
                if u64::from(r.offset) + 8 > u64::from(rec.bytes_len) {
                    return Err(EmitError::Data { row, why: "a relocation past its row's end" });
                }
                let word = start + r.offset;
                let target = self.view.reference(RefId(r.target));
                let to = match RefKind::from_word(target.kind) {
                    Some(RefKind::Static) => RelocTo::Static(target.place),
                    Some(RefKind::Vtable) => RelocTo::Vtable(target.place),
                    Some(RefKind::Data) => {
                        let index = self.place_data(target.place);
                        inner.push((word, index, r.addend));
                        continue;
                    }
                    Some(RefKind::Func) => match self.via.get(target.place as usize) {
                        Some(Via::Address) => RelocTo::Func(target.place),
                        // A function pointer to program code is its slot stub's address.
                        Some(Via::Slot) => RelocTo::SlotStub(target.place),
                        None => return Err(EmitError::NoVia { place: target.place }),
                    },
                    Some(RefKind::Tls) | None => {
                        return Err(EmitError::Data { row, why: "the address of a thread-local in data (it differs per thread)" });
                    }
                };
                self.relocs.push(Reloc { at: word, to, addend: r.addend });
            }
            at += 1;
        }
        for &(word, index, addend) in inner.iter() {
            self.relocs.push(Reloc { at: word, to: RelocTo::Code(data_at[index as usize]), addend });
        }
        self.settle(frame_at, most_align, literals_at, words_at, &data_at)
    }

    /// Patches every field and the frame's size once everything after the instructions is
    /// placed: the literal words from `literals_at` and `words_at`, data row `i` of the placed
    /// at `data_at[i]` (the record form has no literal word and patches none).
    fn settle(&mut self, frame_at: u32, most_align: u64, literals_at: u32, words_at: u32, data_at: &[u32]) -> Result<(), EmitError> {
        let too_far = EmitError::TooLarge { what: "a branch or PC-relative reference past its reach" };
        if self.code.len() > u32::MAX as usize / 2 {
            return Err(EmitError::TooLarge { what: "the code (2 GiB)" });
        }
        let patches = core::mem::replace(&mut self.patches, Vec::new_in(Global));
        for patch in patches.iter() {
            let to = match patch.to {
                Label::Block(block) => self.block_at[block as usize],
                Label::Literal(index) => literals_at + 8 * index,
                Label::Data(index) => data_at[index as usize],
                Label::Word(index) => words_at + 8 * index,
            };
            let word = patch
                .field
                .patch(self.word_at(patch.at), i64::from(to) - i64::from(patch.at))
                .ok_or(too_far)?;
            let at = patch.at as usize;
            self.code[at..at + 4].copy_from_slice(&word.to_le_bytes());
        }
        // The `byval` copies' stretch, after the staging words.
        let byval_at = (u64::from(self.staging_at) + 8 * u64::from(self.staging)).next_multiple_of(16);
        let used = byval_at + u64::from(self.byval_most);
        if used >= FRAME_LIMIT {
            return Err(EmitError::TooLarge { what: "the frame (16 MiB)" });
        }
        let fixes = core::mem::replace(&mut self.byval_fixes, Vec::new_in(Global));
        for &(at, rd, off) in fixes.iter() {
            let to = byval_at as u32 + off;
            self.set(at, enc::add_imm(rd, SP, to >> 12, true));
            self.set(at + 4, enc::add_imm(rd, rd, to & 0xfff, false));
        }
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
