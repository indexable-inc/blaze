//! `asm!` in the walk (INTRINSICS.md 3.4, P10): the checked `Expr::Asm` built as one LIR
//! `asm_block` and an `asm` (or, with `label` operands, an `asm_br`) instruction.
//!
//! The rules are the Reference's (inline-assembly.html):
//! - "Operand type": "Operand expressions are evaluated from left to right, just like function
//!   call arguments. After the `asm!` has executed, outputs are written to in left to right
//!   order." So every operand is evaluated (an input's value, an output's place) before the
//!   block, and the outputs are stored after it, in operand order.
//! - `inout(<reg>) <expr>`: "<expr> is a mutable place expression, which is read on entry and
//!   written on exit": the place is evaluated once, read before the block and written after.
//!   `inout(<reg>) <in> => <out>` evaluates `<in>` as a value and `<out>` as a place (or `_`).
//! - `out(<reg>) _`: "the contents of the register are discarded at the end of the asm code":
//!   the operand stays an output (the register is still written), its member never stored.
//! - `const <expr>`: "the value of the expression is formatted as a string and substituted
//!   directly into the asm template string": the integer in decimal, a `-` before a negative
//!   one of a signed type, merged into the text around it; the operand is no LIR operand. Its
//!   value comes from where the walk already reads constants: an integer literal (negated or
//!   not), a `const` item's record, a `const { .. }` block's record, or a const generic
//!   parameter's argument ([`ConstText`]). Any other expression, a type other than an
//!   integer, a value holding an address and a template modifier on its placeholder are
//!   refused by name.
//! - `sym <path>`: "<path> must refer to a fn or static": a `Func` or `Static` reference of
//!   the function, the operand's `aux` its row of `refs`.
//! - `label <block>`: "If the assembly code jumps to the label, the block is executed": the
//!   instruction is an `asm_br`; the outputs are written on the fall-through edge only, each
//!   label block runs in a block of its own and joins the code after the `asm!`.
//! - `options(noreturn)`: "the asm code never returns ... the asm! block has type `!`": an
//!   `unreachable` follows.
//!
//! Template text, register and class names and `clobber_abi` names come from the body's
//! `asm_text` (blaze-lower's `to_typeck` cooks the escapes and `{{`/`}}`); the option bits are
//! blaze-lir's `AsmRec` bits (blaze-lower's `AsmForm` has the same). Only AArch64 reaches here
//! (the checker's `AsmArch`); the constraint strings are blaze-llvm's (lower_target.rs).
//!
//! An operand held as an aggregate is passed as a LIR vector when it is a `repr(simd)` type
//! (AArch64's `vreg`/`vreg_low16` take `i8x8`..`f64x2`): loaded from its place before the
//! block and stored to its place after it. Any other aggregate is refused by name.

use core::alloc::Allocator;

use blaze_code::record::TrapKind;
use blaze_lir::builder::{AsmOperand, AsmPiece, MemAttrs, RegKind};
use blaze_lir::func::{AsmOperandKind, AsmRec, RefKind};
use blaze_lir::{Block, Flags, Scalar, Ty, Value};
use blaze_typeck::{AsmArch, AsmKind, AsmPiece as TcPiece, AsmReg, BodyView, Def, Expr, ExprId, List, PNode, UnOp};

use super::{PTR_BITS, Repr, Task, V, Walk, layout_err, malformed, prim_ty, unsupported};
use crate::data::DataRecord;
use crate::layout::LayoutCx;
use crate::{BuildError, Instance};

/// `const` operands' text ([`ConstText::write`]).
#[cfg(test)]
mod const_tests;

/// A `const` operand's value as the template's text takes it: its bits at its type's width
/// and whether the type is signed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ConstText {
    bits: u128,
    width: u32,
    signed: bool,
}

impl ConstText {
    /// Appends the value in decimal (the Reference's "formatted as a string": `Display`):
    /// a signed type's value with its sign, at most 40 bytes.
    fn write<A: Allocator>(self, out: &mut Vec<u8, A>) {
        let mask = match self.width {
            128.. => u128::MAX,
            w => (1u128 << w) - 1,
        };
        let bits = self.bits & mask;
        let negative = self.signed && self.width > 0 && (bits >> (self.width - 1)) & 1 == 1;
        let mut magnitude = match negative {
            true => (!bits).wrapping_add(1) & mask,
            false => bits,
        };
        if negative {
            out.push(b'-');
        }
        // u128::MAX has 39 digits.
        let mut digits = [0u8; 39];
        let mut n = 0;
        loop {
            digits[n] = b'0' + (magnitude % 10) as u8;
            n += 1;
            magnitude /= 10;
            if magnitude == 0 {
                break;
            }
        }
        out.extend(digits[..n].iter().rev());
    }
}

/// The bits of a scalar constant's record at `width` bits: its root's bytes, little-endian
/// (as walk.rs `const_record_value_as` reads a scalar record); a record whose root holds an
/// address is refused, as no address is known when the template is written.
fn record_bits<B: Allocator>(e: ExprId, record: &DataRecord<B>, width: u32) -> Result<u128, BuildError> {
    let bad = |why| BuildError::Malformed { what: why };
    record.check().map_err(|_| bad("a const's record that breaks the data form"))?;
    let root = record.allocs.first().ok_or(bad("a const's record with no allocation"))?;
    if !root.relocs.is_empty() {
        return Err(unsupported(e, "a `const` operand of `asm!` whose value holds an address"));
    }
    let bytes = root.bytes.get(..(width / 8) as usize).ok_or(bad("a const's bytes shorter than its type"))?;
    Ok(bytes.iter().enumerate().fold(0u128, |value, (i, &byte)| value | (u128::from(byte) << (8 * i))))
}

/// How an operand's value is held around the block.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Held {
    /// A LIR scalar, kept as the walk keeps it.
    Scalar(Ty),
    /// A `repr(simd)` value: in memory in the walk, a LIR vector of this type for the block;
    /// log2 of its alignment for the load and the store.
    Vector(Ty, u8),
}

impl Held {
    const fn ty(self) -> Ty {
        match self {
            Held::Scalar(ty) | Held::Vector(ty, _) => ty,
        }
    }
}

/// An output to store after the block: its member's place among the result's members, its
/// place, how it is held and its type's repr.
#[derive(Clone, Copy)]
struct Store {
    member: u32,
    at: (Value, i32),
    held: Held,
    repr: Repr,
}

/// A piece of the template while it is assembled: text as a range of the scratch buffer, or
/// a register operand by its LIR index.
#[derive(Clone, Copy)]
enum Raw {
    Text(usize, usize),
    Operand { index: u32, modifier: Option<char> },
}

/// The children an `asm!` gives the flag scan (walk.rs, the drop-flag pass): every operand
/// expression the walk evaluates (inputs, output places), and each `label` block as a
/// conditional child (it runs only when the assembly jumps to it). `const` and `sym` operands
/// are not evaluated here.
pub(super) fn asm_kids<A: Allocator>(body: &BodyView<'_>, operands: List, kids: &mut Vec<(ExprId, bool), A>) -> Result<(), BuildError> {
    let ops = body.asm_operands.get(operands.range()).ok_or(malformed("an `asm!`'s operands past `asm_operands`"))?;
    for op in ops {
        match op.kind {
            AsmKind::In | AsmKind::Out | AsmKind::LateOut => kids.push((op.expr, false)),
            AsmKind::InOut | AsmKind::InLateOut => {
                kids.push((op.expr, false));
                kids.extend(op.out_expr.map(|o| (o, false)));
            }
            AsmKind::Label => kids.push((op.expr, true)),
            AsmKind::Sym | AsmKind::Const => {}
        }
    }
    Ok(())
}

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// `asm!` `e`: its operands' values and places, in operand order, then `Task::Asm`.
    pub(super) fn asm_operands(&mut self, e: ExprId) -> Result<(), BuildError> {
        let Expr::Asm { operands, arch, .. } = self.expr(e)? else { return Err(malformed("an `asm!`'s walk on another expression")) };
        match arch {
            AsmArch::Aarch64 => {}
            AsmArch::X86_64 => return Err(unsupported(e, "inline assembly for x86-64 (the builder builds AArch64's `asm!` only)")),
        }
        let body = self.body;
        let ops = body.asm_operands.get(operands.range()).ok_or(malformed("an `asm!`'s operands past `asm_operands`"))?;
        self.work.push(Task::Asm(e));
        // The work list runs last-pushed first: operands in reverse, each one's tasks in
        // reverse.
        for op in ops.iter().rev() {
            match op.kind {
                AsmKind::In => self.work.push(Task::Eval(op.expr)),
                AsmKind::Out | AsmKind::LateOut => {
                    if !self.is_underscore(op.expr)? {
                        self.work.push(Task::Place(op.expr));
                    }
                }
                AsmKind::InOut | AsmKind::InLateOut => match op.out_expr {
                    // One place, read on entry and written on exit.
                    None => self.work.push(Task::Place(op.expr)),
                    Some(out) => {
                        if !self.is_underscore(out)? {
                            self.work.push(Task::Place(out));
                        }
                        self.work.push(Task::Eval(op.expr));
                    }
                },
                AsmKind::Sym | AsmKind::Const | AsmKind::Label => {}
            }
        }
        Ok(())
    }

    /// `asm!` `e`, its operands' values and places on top (as [`Self::asm_operands`] pushed
    /// them): the block, the instruction, the outputs' stores, and for `label` operands the
    /// label blocks' tasks.
    pub(super) fn asm_done(&mut self, e: ExprId) -> Result<(), BuildError> {
        let Expr::Asm { pieces, operands, options, clobber_abis, .. } = self.expr(e)? else {
            return Err(malformed("an `asm!`'s task on another expression"));
        };
        let body = self.body;
        let ops = body.asm_operands.get(operands.range()).ok_or(malformed("an `asm!`'s operands past `asm_operands`"))?;
        let tpl = body.asm_pieces.get(pieces.range()).ok_or(malformed("an `asm!`'s pieces past `asm_pieces`"))?;
        let abis = body.asm_clobbers.get(clobber_abis.range()).ok_or(malformed("an `asm!`'s `clobber_abi` names past `asm_clobbers`"))?;
        let options = u32::from(options);
        let noreturn = options & AsmRec::NORETURN != 0;

        // How many values the operands left on the stack.
        let mut count = 0usize;
        for op in ops {
            count += match op.kind {
                AsmKind::In => 1,
                AsmKind::Out | AsmKind::LateOut => usize::from(!self.is_underscore(op.expr)?),
                AsmKind::InOut | AsmKind::InLateOut => match op.out_expr {
                    None => 1,
                    Some(out) => 1 + usize::from(!self.is_underscore(out)?),
                },
                AsmKind::Sym | AsmKind::Const | AsmKind::Label => 0,
            };
        }
        let base = self.vals.len().checked_sub(count).ok_or(malformed("an `asm!`'s operands missing from the value stack (a build bug)"))?;

        // Per operand of the source, its index among the LIR block's operands (`const`s are
        // formatted into the text and have none).
        let mut lir_index: Vec<u32, A> = Vec::with_capacity_in(ops.len(), self.alloc);
        let mut lir_ops: Vec<AsmOperand<'_>, A> = Vec::with_capacity_in(ops.len(), self.alloc);
        let mut inputs: Vec<Value, A> = Vec::new_in(self.alloc);
        let mut members: Vec<Ty, A> = Vec::new_in(self.alloc);
        let mut stores: Vec<Store, A> = Vec::new_in(self.alloc);
        let mut labels: Vec<(Block, ExprId), A> = Vec::new_in(self.alloc);
        // Per operand of the source, its value when it is a `const`.
        let mut consts: Vec<Option<ConstText>, A> = Vec::with_capacity_in(ops.len(), self.alloc);
        let mut next = base;
        for op in ops {
            lir_index.push(lir_ops.len() as u32);
            let (reg, reg_kind) = match op.reg {
                Some(AsmReg::Class { first, len }) => (asm_str(body, first, len)?, RegKind::Class),
                Some(AsmReg::Explicit { first, len }) => (asm_str(body, first, len)?, RegKind::Reg),
                None => ("", RegKind::Class),
            };
            let (kind, aux) = match op.kind {
                AsmKind::In => {
                    let v = self.vals[next];
                    next += 1;
                    let held = self.asm_held(op.expr, true)?;
                    let x = self.asm_input(v, held, op.expr, true)?;
                    inputs.push(x);
                    (AsmOperandKind::In, 0)
                }
                AsmKind::Out | AsmKind::LateOut => {
                    let held = match self.is_underscore(op.expr)? {
                        true => discarded(e, reg, reg_kind)?,
                        false => {
                            let v = self.vals[next];
                            next += 1;
                            let held = self.asm_held(op.expr, false)?;
                            let repr = self.expr_repr(op.expr)?;
                            let at = self.place(v, repr)?;
                            stores.push(Store { member: members.len() as u32, at, held, repr });
                            held
                        }
                    };
                    members.push(held.ty());
                    let kind = if op.kind == AsmKind::Out { AsmOperandKind::Out } else { AsmOperandKind::LateOut };
                    (kind, 0)
                }
                AsmKind::InOut | AsmKind::InLateOut => {
                    let held = match op.out_expr {
                        // The one place: read now, written after.
                        None => {
                            let v = self.vals[next];
                            next += 1;
                            let held = self.asm_held(op.expr, false)?;
                            let repr = self.expr_repr(op.expr)?;
                            let at = self.place(v, repr)?;
                            let x = match held {
                                Held::Scalar(_) => match self.load(repr, at) {
                                    V::Scalar(x) => x,
                                    _ => return Err(malformed("an `inout` place of a scalar that loads no scalar")),
                                },
                                Held::Vector(ty, align) => self.b.load(ty, Flags::NONE, at.0, at.1, MemAttrs::aligned(align)),
                            };
                            inputs.push(x);
                            stores.push(Store { member: members.len() as u32, at, held, repr });
                            held
                        }
                        Some(out) => {
                            let v = self.vals[next];
                            next += 1;
                            let held = self.asm_held(op.expr, true)?;
                            let x = self.asm_input(v, held, op.expr, true)?;
                            inputs.push(x);
                            if !self.is_underscore(out)? {
                                let w = self.vals[next];
                                next += 1;
                                let out_held = self.asm_held(out, false)?;
                                if out_held != held {
                                    return Err(unsupported(e, "an `inout(..) a => b` whose input and output are held as different LIR types"));
                                }
                                let repr = self.expr_repr(out)?;
                                let at = self.place(w, repr)?;
                                stores.push(Store { member: members.len() as u32, at, held, repr });
                            }
                            held
                        }
                    };
                    members.push(held.ty());
                    let kind = if op.kind == AsmKind::InOut { AsmOperandKind::InOut } else { AsmOperandKind::InLateOut };
                    (kind, 0)
                }
                AsmKind::Sym => (AsmOperandKind::Sym, self.asm_sym(op.expr)?),
                AsmKind::Label => {
                    let block = self.create();
                    labels.push((block, op.expr));
                    (AsmOperandKind::Label, (labels.len() - 1) as u32)
                }
                // Its value is formatted into the text below, with no LIR operand.
                AsmKind::Const => {
                    let value = self.asm_const(e, op.expr)?;
                    consts.push(Some(value));
                    continue;
                }
            };
            consts.push(None);
            lir_ops.push(AsmOperand { kind, reg, reg_kind, aux });
        }
        if next != self.vals.len() {
            return Err(malformed("an `asm!`'s operands not all taken from the value stack (a build bug)"));
        }
        self.truncate_vals(base);
        if noreturn && !stores.is_empty() {
            return Err(malformed("an `asm!` with `options(noreturn)` and an output (the lowering's refusal)"));
        }

        // The template: text pieces from `asm_text`, `const` values in decimal, register
        // operands by their LIR index; adjacent text merged.
        let mut text: Vec<u8, A> = Vec::new_in(self.alloc);
        let mut raws: Vec<Raw, A> = Vec::with_capacity_in(tpl.len(), self.alloc);
        for piece in tpl {
            let start = text.len();
            match *piece {
                TcPiece::Text { first, len } => text.extend_from_slice(asm_str(body, first, len)?.as_bytes()),
                // The Reference, "Template string arguments": the strings "concatenated with a
                // \n between them".
                TcPiece::Line => text.push(b'\n'),
                TcPiece::Operand { index, modifier } => match consts.get(index as usize).copied().flatten() {
                    // The Reference, "Template modifiers": a modifier applies to a register
                    // operand; rustc refuses one on a `const`.
                    Some(_) if modifier.is_some() => {
                        return Err(unsupported(e, "a template modifier on a `const` operand of `asm!`"));
                    }
                    Some(value) => value.write(&mut text),
                    None => {
                        let at = *lir_index.get(index as usize).ok_or(malformed("an `asm!` placeholder past its operands"))?;
                        raws.push(Raw::Operand { index: at, modifier });
                        continue;
                    }
                },
            }
            match raws.last_mut() {
                Some(Raw::Text(_, end)) if *end == start => *end = text.len(),
                _ => raws.push(Raw::Text(start, text.len())),
            }
        }
        let mut clobbers: Vec<u8, A> = Vec::new_in(self.alloc);
        for (i, &(first, len)) in abis.iter().enumerate() {
            if i > 0 {
                clobbers.push(b',');
            }
            clobbers.extend_from_slice(asm_str(body, first, len)?.as_bytes());
        }
        let text = core::str::from_utf8(&text).map_err(|_| malformed("an `asm!` template that is not UTF-8"))?;
        let clobbers = core::str::from_utf8(&clobbers).map_err(|_| malformed("a `clobber_abi` name that is not UTF-8"))?;
        let mut lir_pieces: Vec<AsmPiece<'_>, A> = Vec::with_capacity_in(raws.len(), self.alloc);
        for raw in raws.iter() {
            lir_pieces.push(match *raw {
                Raw::Text(s, end) => AsmPiece::Text(text.get(s..end).ok_or(malformed("an `asm!` text piece off a character boundary"))?),
                Raw::Operand { index, modifier } => AsmPiece::Operand { index, modifier },
            });
        }
        let id = self.b.asm_block(&lir_pieces, &lir_ops, options, clobbers);

        // The result: none, the one output's type, or a compound in operand order.
        let ty = match members.as_slice() {
            [] => Ty::NONE,
            &[one] => one,
            many => self.b.compound(many),
        };
        let single = members.len() == 1;
        if labels.is_empty() {
            let v = self.b.asm(id, ty, &inputs);
            if noreturn {
                self.unreachable_of(TrapKind::NoreturnAsm, Some(e));
                self.dead_block();
            } else {
                self.asm_stores(v, single, &stores)?;
            }
            self.push_val(V::None, None);
            return Ok(());
        }
        let fall = self.create();
        let join = self.create();
        let mut blocks: Vec<Block, A> = Vec::with_capacity_in(labels.len(), self.alloc);
        blocks.extend(labels.iter().map(|&(b, _)| b));
        self.reach(fall);
        for &b in blocks.iter() {
            self.reach(b);
        }
        let v = self.b.asm_br(id, ty, fall, &blocks, &inputs);
        self.begin(fall);
        if noreturn {
            self.unreachable_of(TrapKind::NoreturnAsm, Some(e));
        } else {
            self.asm_stores(v, single, &stores)?;
            self.jump(join, &[]);
        }
        // Label 0 runs first: pushed last.
        self.work.push(Task::AsmJoin(join));
        for &(block, body) in labels.iter().rev() {
            self.work.push(Task::AsmLabel { block, body, join });
        }
        Ok(())
    }

    /// Label block `block` of an `asm!`: its body, then a jump to `join`.
    pub(super) fn asm_label(&mut self, block: Block, body: ExprId, join: Block) {
        self.begin(block);
        self.work.push(Task::AsmLabelEnd(join));
        self.work.push(Task::Eval(body));
    }

    /// The end of a label block: its value (`()` or `!`) dropped, a jump to `join`.
    pub(super) fn asm_label_end(&mut self, join: Block) -> Result<(), BuildError> {
        self.pop()?;
        self.jump(join, &[]);
        Ok(())
    }

    /// After every label block: the code after the `asm!`, its value `()`.
    pub(super) fn asm_join(&mut self, join: Block) {
        self.begin(join);
        self.push_val(V::None, None);
    }

    /// Stores each output's member of result `v` (the value itself when `single`) to its place.
    fn asm_stores(&mut self, v: Value, single: bool, stores: &[Store]) -> Result<(), BuildError> {
        for s in stores {
            let x = match single {
                true => v,
                false => self.b.project(v, s.member),
            };
            match s.held {
                Held::Scalar(_) => self.store(V::Scalar(x), s.repr, s.at)?,
                Held::Vector(_, align) => self.b.store(Flags::NONE, x, s.at.0, s.at.1, MemAttrs::aligned(align)),
            }
        }
        Ok(())
    }

    /// An input's value `v` of operand expression `x`, held as `held`.
    fn asm_input(&mut self, v: V, held: Held, x: ExprId, adjusted: bool) -> Result<Value, BuildError> {
        Ok(match held {
            Held::Scalar(ty) => self.scalar(v, ty)?,
            Held::Vector(ty, align) => {
                let repr = match adjusted {
                    true => self.adjusted_repr(x)?,
                    false => self.expr_repr(x)?,
                };
                let at = self.place(v, repr)?;
                self.b.load(ty, Flags::NONE, at.0, at.1, MemAttrs::aligned(align))
            }
        })
    }

    /// How operand expression `x` (its type after adjustments when `adjusted`, an input's; its
    /// own type otherwise, a place's) is held around the block.
    fn asm_held(&mut self, x: ExprId, adjusted: bool) -> Result<Held, BuildError> {
        let t = match adjusted {
            true => self.adjusted_ty(x)?,
            false => self.expr_ty(x)?,
        };
        let repr = self.repr_at(x, t)?;
        if let Some(ty) = repr.scalar() {
            return Ok(Held::Scalar(ty));
        }
        let Repr::Agg(_) = repr else { return Err(unsupported(x, "an `asm!` operand of a type with no value (`()`, `!`, a zero-sized type)")) };
        let g = self.ground_ty(t).map_err(|error| Self::ty_err(x, error))?;
        let adts = self.adts;
        let lanes = {
            let mut cx = LayoutCx { ground: &mut *self.ground, facts: adts.facts(), scratch: self.scratch };
            self.ls.lanes(g, &mut cx).map_err(|error| Self::ty_err(x, layout_err(error)))?
        };
        let lanes = lanes.ok_or(unsupported(x, "an `asm!` operand of an aggregate that is not a `repr(simd)` vector"))?;
        let elem: Scalar = prim_ty(lanes.elem).elem();
        let count = u32::try_from(lanes.count).map_err(|_| unsupported(x, "an `asm!` vector operand of more than 2^32 lanes"))?;
        let ty = Ty::vector(elem, count).ok_or(unsupported(x, "an `asm!` vector operand LIR has no vector type for"))?;
        Ok(Held::Vector(ty, lanes.align))
    }

    /// The value of `const` operand `expr` of `asm!` `e` (parentheses peeled), from the four
    /// places the walk reads constants: an integer literal (`lit`'s `int_lit`), `-` of one,
    /// a `const` item's record ([`Self::const_value`]'s), a `const { .. }` block's record, and
    /// a const generic parameter's argument ([`Self::const_param_value`]'s). The Reference:
    /// "must be an integer constant expression".
    fn asm_const(&mut self, e: ExprId, expr: ExprId) -> Result<ConstText, BuildError> {
        let mut x = expr;
        while let Expr::Paren(inner) = self.expr(x)? {
            x = inner;
        }
        let Repr::Int { ty, signed } = self.expr_repr(x)? else {
            return Err(unsupported(e, "a `const` operand of `asm!` of a type other than an integer"));
        };
        let width = ty.bits(PTR_BITS);
        let adts = self.adts;
        let owner = self.body.owner;
        let bits = match self.expr(x)? {
            Expr::Lit(_) => adts.int_lit(owner, x).ok_or(BuildError::NoLiteralValue { expr: x })?,
            Expr::Unary { op: UnOp::Neg, operand } if matches!(self.expr(operand)?, Expr::Lit(_)) => {
                adts.int_lit(owner, operand).ok_or(BuildError::NoLiteralValue { expr: operand })?.wrapping_neg()
            }
            Expr::ConstBlock { .. } => {
                let record = adts.const_block_value(owner, x, self.args).ok_or(BuildError::NoConstBlockValue { expr: x, owner })?;
                record_bits(e, record, width)?
            }
            Expr::Path(_) => match self.expr_def(x) {
                Some(Def::Const(item)) => {
                    let args = self.const_path_args(x)?;
                    let record = adts.const_value(item, &args[..]).ok_or(BuildError::NoConstValue { expr: x, item })?;
                    record_bits(e, record, width)?
                }
                Some(Def::TyParam(index)) => {
                    let g = *self.args.get(index as usize).ok_or(malformed("a const parameter past the instance's arguments"))?;
                    let PNode::ConstValue { bits, .. } = self.ground_node(g).map_err(|error| Self::ty_err(x, error))? else {
                        return Err(malformed("a const parameter's argument that is not a value in the ground table"));
                    };
                    bits
                }
                _ => return Err(unsupported(e, "a `const` operand of `asm!` naming a path that is no constant or const parameter")),
            },
            _ => {
                return Err(unsupported(
                    e,
                    "a `const` operand of `asm!` other than an integer literal, a constant, a `const { .. }` block or a const parameter (an arithmetic expression is not evaluated here)",
                ));
            }
        };
        Ok(ConstText { bits, width, signed })
    }

    /// `sym` operand path `x`: the row of `refs` of the fn instance or static it names.
    fn asm_sym(&mut self, x: ExprId) -> Result<u32, BuildError> {
        let t = self.expr_ty(x)?;
        if let PNode::FnDef(item, args) = self.node_at(x, t)? {
            let target = self.reify_target(Instance { item, args })?;
            let place = self.ref_place(target);
            return Ok(self.b.reference(RefKind::Func, place).0);
        }
        let Some(Def::Static(item, _)) = self.expr_def(x) else {
            return Err(malformed("a `sym` operand naming other than a fn or a static (the check's refusal)"));
        };
        let s = self.adts.static_item(item).ok_or(BuildError::NoStatic { expr: x, item })?;
        if s.thread_local {
            return Err(unsupported(x, "a `sym` operand naming a `#[thread_local]` static"));
        }
        let place = self.static_place(item, false)?;
        Ok(self.b.reference(RefKind::Static, place).0)
    }

    /// Whether `x` is `_` (a discarded output).
    fn is_underscore(&self, x: ExprId) -> Result<bool, BuildError> {
        Ok(matches!(self.expr(x)?, Expr::Underscore))
    }
}

/// The text `asm_text[first..first + len]` of `body`.
fn asm_str<'b>(body: &BodyView<'b>, first: u32, len: u32) -> Result<&'b str, BuildError> {
    let (s, end) = (first as usize, first as usize + len as usize);
    body.asm_text.get(s..end).ok_or(malformed("an `asm!` text range past `asm_text`"))
}

/// The type a discarded output (`out(<reg>) _`) is given in the block of `asm!` `e`: the whole
/// register, so all of it counts as written. AArch64 (the Reference, "Register operands" and
/// "Register names"): `reg` and `x`/`w` registers (and `lr`) are 64 bits, `vreg`,
/// `vreg_low16` and `v`/`q`/`d`/`s`/`h`/`b` registers 128. Any other class or name is
/// refused by name.
fn discarded(e: ExprId, reg: &str, kind: RegKind) -> Result<Held, BuildError> {
    let v128 = Ty::vector(Scalar::I64, 2).ok_or(malformed("LIR has no `<2 x i64>`"))?;
    let wide = match kind {
        RegKind::Class => match reg {
            "reg" => false,
            "vreg" | "vreg_low16" => true,
            _ => return Err(unsupported(e, "a discarded output (`out(<class>) _`) of a class other than `reg`, `vreg` or `vreg_low16`")),
        },
        RegKind::Reg => match reg.as_bytes().first() {
            _ if reg == "lr" => false,
            Some(b'x' | b'w') => false,
            Some(b'v' | b'q' | b'd' | b's' | b'h' | b'b') => true,
            _ => return Err(unsupported(e, "a discarded output (`out(\"<reg>\") _`) of a register not named `x`/`w`/`lr` or `v`/`q`/`d`/`s`/`h`/`b`")),
        },
    };
    Ok(match wide {
        true => Held::Vector(v128, 4),
        false => Held::Scalar(Ty::I64),
    })
}
