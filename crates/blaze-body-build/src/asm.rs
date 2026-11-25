//! Inline assembly (`StmtKind::InlineAsm`) lowering into LIR `asm_block` + `asm` / `asm_br`,
//! matching `blaze-build`'s `walk/asm.rs` semantics on AArch64.

use blaze_own::Vec;
use core::alloc::Allocator;

use blaze_body::{AsmArch, AsmOpKind, AsmPiece as BodyAsmPiece, AsmRegKind, Block as BodyBlock, ConstKind, Operand, Ops, Place};
use blaze_build::data::DataRecord;
use blaze_build::intrinsics::lir;
use blaze_build::layout::{LayoutCx, LayoutId};
use blaze_build::Instance;
use blaze_lir::builder::{AsmOperand, AsmPiece, MemAttrs, RegKind};
use blaze_lir::func::{AsmOperandKind, AsmRec, BlockKind};
use blaze_lir::{Block, Flags, Scalar, Ty, Value};
use blaze_typeck::{PNode, PTy};

use crate::{BuildError, Cx, PTR_BITS, V};

/// A `const` operand's value formatted in decimal into the `asm!` template.
#[derive(Clone, Copy)]
struct ConstText {
    bits: u128,
    width: u32,
    signed: bool,
}

impl ConstText {
    fn write<A: Allocator>(self, out: &mut Vec<u8, A>) {
        let mask = match self.width {
            128.. => u128::MAX,
            w => (1u128 << w) - 1,
        };
        let bits = self.bits & mask;
        let negative = self.signed && self.width > 0 && ((bits >> (self.width - 1)) & 1) == 1;
        let mut magnitude = if negative { (!bits).wrapping_add(1) & mask } else { bits };
        if negative {
            out.push(b'-');
        }
        let mut digits = [0u8; 39];
        let mut n = 0usize;
        loop {
            if let Some(slot) = digits.get_mut(n) {
                *slot = b'0' + u8::try_from(magnitude % 10).unwrap_or(0);
            }
            n += 1;
            magnitude /= 10;
            if magnitude == 0 {
                break;
            }
        }
        if let Some(slice) = digits.get(..n) {
            for &d in slice.iter().rev() {
                out.push(d);
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AsmHeld {
    Scalar(Ty),
    Vector(Ty, u8),
}

impl AsmHeld {
    const fn ty(self) -> Ty {
        match self {
            AsmHeld::Scalar(ty) | AsmHeld::Vector(ty, _) => ty,
        }
    }
}

#[derive(Clone, Copy)]
struct AsmStore {
    member: u32,
    place: Place,
    lay: LayoutId,
    held: AsmHeld,
}

#[derive(Clone, Copy)]
enum RawPiece {
    Text(usize, usize),
    Operand { index: u32, modifier: Option<char> },
}

fn discarded_output(reg: &str, kind: RegKind) -> Option<AsmHeld> {
    let v128 = Ty::vector(Scalar::I64, 2)?;
    let wide = match kind {
        RegKind::Class => match reg {
            "reg" => false,
            "vreg" | "vreg_low16" => true,
            _ => return None,
        },
        RegKind::Reg => match reg.as_bytes().first() {
            _ if reg == "lr" => false,
            Some(b'x' | b'w') => false,
            Some(b'v' | b'q' | b'd' | b's' | b'h' | b'b') => true,
            _ => return None,
        },
    };
    Some(if wide { AsmHeld::Vector(v128, 4) } else { AsmHeld::Scalar(Ty::I64) })
}

impl<A: Allocator + Copy> Cx<'_, '_, '_, A> {
    fn asm_held(&mut self, g: PTy) -> Result<AsmHeld, BuildError> {
        let lay = self.layout(g)?;
        let l = *self.layouts.get(lay);
        if let Some(s) = l.scalar {
            return Ok(AsmHeld::Scalar(lir(s.prim).0));
        }
        if l.is_zst() {
            return Err(self.unsupported("an `asm!` operand of a type with no value (`()`, `!`, a zero-sized type)"));
        }
        let adts = self.adts;
        let lanes = {
            let mut lcx = LayoutCx { ground: &mut *self.subst.ground, facts: adts.facts(), scratch: self.subst.scratch };
            self.layouts.lanes(g, &mut lcx).map_err(|error| BuildError::Layout { stmt: self.stmt, error })?
        };
        let lanes = lanes.ok_or(self.unsupported("an `asm!` operand of an aggregate that is not a `repr(simd)` vector"))?;
        let elem = lir(lanes.elem).0.elem();
        let count = u32::try_from(lanes.count).map_err(|_| self.unsupported("an `asm!` vector operand of more than 2^32 lanes"))?;
        let ty = Ty::vector(elem, count).ok_or(self.unsupported("an `asm!` vector operand LIR has no vector type for"))?;
        Ok(AsmHeld::Vector(ty, lanes.align))
    }

    fn asm_input_val(&mut self, v: V, lay: LayoutId, held: AsmHeld) -> Result<Value, BuildError> {
        match held {
            AsmHeld::Scalar(_) => self.scalar(v),
            AsmHeld::Vector(ty, align) => {
                let (ptr, off) = self.in_memory(v, lay)?;
                Ok(self.b.load(ty, Flags::NONE, ptr, off, MemAttrs::aligned(align)))
            }
        }
    }

    fn asm_record_bits<DA: Allocator>(&self, record: &DataRecord<DA>, width: u32) -> Result<u128, BuildError> {
        record.check().map_err(|_| self.malformed("a const's record that breaks the data form"))?;
        let root = record.allocs.first().ok_or(self.malformed("a const's record with no allocation"))?;
        if !root.relocs.is_empty() {
            return Err(self.unsupported("a `const` operand of `asm!` whose value holds an address"));
        }
        let nbytes = usize::try_from(width / 8).map_err(|_| self.malformed("a const width past usize"))?;
        let bytes = root.bytes.get(..nbytes).ok_or(self.malformed("a const's bytes shorter than its type"))?;
        let mut value = 0u128;
        for (i, &byte) in bytes.iter().enumerate() {
            value |= u128::from(byte) << (8 * i);
        }
        Ok(value)
    }

    fn asm_const(&mut self, op: Operand) -> Result<ConstText, BuildError> {
        let Operand::Const(c) = op else {
            return Err(self.malformed("a `const` operand of `asm!` that is not a constant"));
        };
        let stmt = self.row()?;
        let decl = self.v.konst(c);
        let g = self.ground(decl.ty)?;
        let (signed, ty) = match self.node(g)? {
            PNode::Int(_) => {
                let lay = self.layout(g)?;
                let s = self.layouts.get(lay).scalar.ok_or(self.malformed("an integer without a scalar layout"))?;
                (true, lir(s.prim).0)
            }
            PNode::Uint(_) => {
                let lay = self.layout(g)?;
                let s = self.layouts.get(lay).scalar.ok_or(self.malformed("an integer without a scalar layout"))?;
                (false, lir(s.prim).0)
            }
            _ => return Err(self.unsupported("a `const` operand of `asm!` of a type other than an integer")),
        };
        let width = ty.bits(PTR_BITS);
        let bits = match decl.kind {
            ConstKind::Scalar { bits } => bits,
            ConstKind::Param(idx) => {
                let i = usize::try_from(idx).map_err(|_| self.malformed("a const parameter index past usize"))?;
                let arg = *self.subst.args.get(i).ok_or(self.malformed("a const parameter past the instance's arguments"))?;
                match self.node(arg)? {
                    PNode::ConstValue { bits, .. } => bits,
                    _ => return Err(self.malformed("a const parameter's argument that is not a value")),
                }
            }
            ConstKind::Item { key, args } => {
                let from = self.from;
                let v = self.v;
                let tys = v.ty_list(&from, args).map_err(|_| self.malformed("a constant's arguments past the body's tables"))?;
                let mut ground_args: Vec<PTy, A> = Vec::with_capacity_in(tys.len(), self.alloc);
                for t in tys.iter() {
                    ground_args.push(self.ground(t)?);
                }
                let record = self.adts.const_value(key, &ground_args).ok_or(BuildError::NoConstValue { stmt, item: key })?;
                self.asm_record_bits(record, width)?
            }
            ConstKind::Anon(expr) => {
                let record = self.adts.const_block_value(self.item, expr, self.subst.args).ok_or(BuildError::NoConstBlock { stmt, expr })?;
                self.asm_record_bits(record, width)?
            }
            _ => return Err(self.unsupported("an unsupported `const` operand form in `asm!`")),
        };
        Ok(ConstText { bits, width, signed })
    }

    fn asm_sym(&mut self, op: Operand) -> Result<u32, BuildError> {
        let Operand::Const(c) = op else {
            return Err(self.malformed("a `sym` operand of `asm!` that is not a constant"));
        };
        let decl = self.v.konst(c);
        match decl.kind {
            ConstKind::FnItem { key, args } => {
                let from = self.from;
                let v = self.v;
                let tys = v.ty_list(&from, args).map_err(|_| self.malformed("a `sym` fn's args past the body's tables"))?;
                let mut gargs: Vec<PTy, A> = Vec::with_capacity_in(tys.len(), self.alloc);
                for t in tys.iter() {
                    gargs.push(self.ground(t)?);
                }
                let list = self.subst.ground.intern_list(&gargs).map_err(|_| self.malformed("the ground table is full"))?;
                let target = self.reify_target(Instance { item: key, args: list })?;
                Ok(self.func_ref(target).0)
            }
            ConstKind::StaticAddr(item) => {
                if let Some(s) = self.adts.static_item(item)
                    && s.thread_local
                {
                    return Err(self.unsupported("a `sym` operand naming a `#[thread_local]` static"));
                }
                Ok(self.static_ref(item).0)
            }
            _ => Err(self.malformed("a `sym` operand naming other than a fn or a static")),
        }
    }

    fn asm_store_outputs(&mut self, v: Value, single: bool, stores: &[AsmStore]) -> Result<(), BuildError> {
        for s in stores {
            let x = if single { v } else { self.b.project(v, s.member) };
            match s.held {
                AsmHeld::Scalar(_) => self.write(s.place, V::One(x), s.lay)?,
                AsmHeld::Vector(_, align) => {
                    let ptr = self
                        .dest_memory(s.place)?
                        .ok_or(self.malformed("an `asm!` SIMD vector output place not held in memory"))?;
                    self.b.store(Flags::NONE, x, ptr, 0, MemAttrs::aligned(align));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn inline_asm(&mut self, asm: u32, operands: Ops, target: Option<BodyBlock>) -> Result<(), BuildError> {
        if asm >= self.v.asms() {
            return Err(self.unsupported("an `asm!` block without descriptor metadata"));
        }
        let row = self.v.asm(asm);
        if row.arch != AsmArch::Aarch64 {
            return Err(self.unsupported("inline assembly for x86-64 (the builder builds AArch64's `asm!` only)"));
        }
        let options = u32::from(row.options);
        let noreturn = (options & AsmRec::NORETURN) != 0;
        let in_ops: Vec<Operand, A> = {
            let cap = usize::try_from(operands.len()).unwrap_or(0);
            let mut out = Vec::with_capacity_in(cap, self.alloc);
            out.extend(self.v.ops(operands));
            out
        };
        let n_desc = usize::try_from(row.ops.len()).unwrap_or(0);
        let mut lir_index: Vec<u32, A> = Vec::with_capacity_in(n_desc, self.alloc);
        let mut lir_ops: Vec<AsmOperand<'_>, A> = Vec::with_capacity_in(n_desc, self.alloc);
        let mut inputs: Vec<Value, A> = Vec::new_in(self.alloc);
        let mut members: Vec<Ty, A> = Vec::new_in(self.alloc);
        let mut stores: Vec<AsmStore, A> = Vec::new_in(self.alloc);
        let mut labels: Vec<(Block, BodyBlock), A> = Vec::new_in(self.alloc);
        let mut consts: Vec<Option<ConstText>, A> = Vec::with_capacity_in(n_desc, self.alloc);

        for oi in row.ops.start..row.ops.end {
            let desc = self.v.asm_op(oi);
            let cur_lir = u32::try_from(lir_ops.len()).map_err(|_| self.malformed("too many `asm!` operands"))?;
            lir_index.push(cur_lir);
            let (reg, reg_kind) = match desc.reg_kind {
                AsmRegKind::Class => (self.v.asm_str(desc.reg_start, desc.reg_len), RegKind::Class),
                AsmRegKind::Explicit => (self.v.asm_str(desc.reg_start, desc.reg_len), RegKind::Reg),
                AsmRegKind::None => ("", RegKind::Class),
            };
            let (kind, aux) = match desc.kind {
                AsmOpKind::In => {
                    let idx = desc.in_op.and_then(|i| usize::try_from(i).ok()).ok_or(self.malformed("an `in` operand missing its input"))?;
                    let &op = in_ops.get(idx).ok_or(self.malformed("an `in` operand index past operands"))?;
                    let (v, g) = self.operand(op)?;
                    let lay = self.layout(g)?;
                    let held = self.asm_held(g)?;
                    let x = self.asm_input_val(v, lay, held)?;
                    inputs.push(x);
                    (AsmOperandKind::In, 0)
                }
                AsmOpKind::Out | AsmOpKind::LateOut => {
                    let held = if desc.discard_out {
                        discarded_output(reg, reg_kind).ok_or(self.unsupported("a discarded `asm!` output of an unsupported register or class"))?
                    } else {
                        let place = desc.out_place.ok_or(self.malformed("an `out` operand missing its place"))?;
                        let g = self.place_ty(place)?;
                        let lay = self.dest_layout(place)?;
                        let held = self.asm_held(g)?;
                        let member = u32::try_from(members.len()).map_err(|_| self.malformed("too many `asm!` outputs"))?;
                        stores.push(AsmStore { member, place, lay, held });
                        held
                    };
                    members.push(held.ty());
                    let k = if desc.kind == AsmOpKind::Out { AsmOperandKind::Out } else { AsmOperandKind::LateOut };
                    (k, 0)
                }
                AsmOpKind::InOut | AsmOpKind::InLateOut => {
                    let held = match desc.in_op {
                        None => {
                            let place = desc.out_place.ok_or(self.malformed("an `inout` operand missing its place"))?;
                            let (v, g) = self.operand(Operand::Copy(place))?;
                            let lay = self.dest_layout(place)?;
                            let held = self.asm_held(g)?;
                            let x = self.asm_input_val(v, lay, held)?;
                            inputs.push(x);
                            let member = u32::try_from(members.len()).map_err(|_| self.malformed("too many `asm!` outputs"))?;
                            stores.push(AsmStore { member, place, lay, held });
                            held
                        }
                        Some(idx_u32) => {
                            let idx = usize::try_from(idx_u32).map_err(|_| self.malformed("an `inout` operand index past usize"))?;
                            let &op = in_ops.get(idx).ok_or(self.malformed("an `inout` operand index past operands"))?;
                            let (v, g) = self.operand(op)?;
                            let lay = self.layout(g)?;
                            let held = self.asm_held(g)?;
                            let x = self.asm_input_val(v, lay, held)?;
                            inputs.push(x);
                            if !desc.discard_out {
                                let place = desc.out_place.ok_or(self.malformed("an `inout` operand missing its output place"))?;
                                let og = self.place_ty(place)?;
                                let olay = self.dest_layout(place)?;
                                let oheld = self.asm_held(og)?;
                                if oheld != held {
                                    return Err(self.unsupported("an `inout(..) a => b` whose input and output are held as different LIR types"));
                                }
                                let member = u32::try_from(members.len()).map_err(|_| self.malformed("too many `asm!` outputs"))?;
                                stores.push(AsmStore { member, place, lay: olay, held });
                            }
                            held
                        }
                    };
                    members.push(held.ty());
                    let k = if desc.kind == AsmOpKind::InOut { AsmOperandKind::InOut } else { AsmOperandKind::InLateOut };
                    (k, 0)
                }
                AsmOpKind::Sym => {
                    let idx = desc.in_op.and_then(|i| usize::try_from(i).ok()).ok_or(self.malformed("a `sym` operand missing its constant"))?;
                    let &op = in_ops.get(idx).ok_or(self.malformed("a `sym` operand index past operands"))?;
                    (AsmOperandKind::Sym, self.asm_sym(op)?)
                }
                AsmOpKind::Label => {
                    let body_lbl = desc.label.ok_or(self.malformed("a `label` operand missing its target block"))?;
                    let bridge = self.b.create_block(BlockKind::Normal);
                    let lbl_idx = u32::try_from(labels.len()).map_err(|_| self.malformed("too many `asm!` labels"))?;
                    labels.push((bridge, body_lbl));
                    (AsmOperandKind::Label, lbl_idx)
                }
                AsmOpKind::Const => {
                    let idx = desc.in_op.and_then(|i| usize::try_from(i).ok()).ok_or(self.malformed("a `const` operand missing its constant"))?;
                    let &op = in_ops.get(idx).ok_or(self.malformed("a `const` operand index past operands"))?;
                    consts.push(Some(self.asm_const(op)?));
                    continue;
                }
            };
            consts.push(None);
            lir_ops.push(AsmOperand { kind, reg, reg_kind, aux });
        }

        if noreturn && !stores.is_empty() {
            return Err(self.malformed("an `asm!` with `options(noreturn)` and an output"));
        }

        let mut text: Vec<u8, A> = Vec::new_in(self.alloc);
        let n_pieces = usize::try_from(row.pieces.len()).unwrap_or(0);
        let mut raws: Vec<RawPiece, A> = Vec::with_capacity_in(n_pieces, self.alloc);
        for pi in row.pieces.start..row.pieces.end {
            let start = text.len();
            match self.v.asm_piece(pi) {
                BodyAsmPiece::Text { start: s, len } => text.extend_from_slice(self.v.asm_str(s, len).as_bytes()),
                BodyAsmPiece::Line => text.push(b'\n'),
                BodyAsmPiece::Operand { index, modifier } => {
                    let idx = usize::try_from(index).map_err(|_| self.malformed("an `asm!` placeholder index past usize"))?;
                    match consts.get(idx).copied().flatten() {
                        Some(_) if modifier.is_some() => {
                            return Err(self.unsupported("a template modifier on a `const` operand of `asm!`"));
                        }
                        Some(val) => val.write(&mut text),
                        None => {
                            let at = *lir_index.get(idx).ok_or(self.malformed("an `asm!` placeholder past its operands"))?;
                            raws.push(RawPiece::Operand { index: at, modifier });
                            continue;
                        }
                    }
                }
            }
            match raws.last_mut() {
                Some(RawPiece::Text(_, end)) if *end == start => *end = text.len(),
                _ => raws.push(RawPiece::Text(start, text.len())),
            }
        }
        let text_str = core::str::from_utf8(&text).map_err(|_| self.malformed("an `asm!` template that is not UTF-8"))?;
        let mut lir_pieces: Vec<AsmPiece<'_>, A> = Vec::with_capacity_in(raws.len(), self.alloc);
        for raw in raws.iter() {
            lir_pieces.push(match *raw {
                RawPiece::Text(s, e) => AsmPiece::Text(text_str.get(s..e).ok_or(self.malformed("an `asm!` text piece off a char boundary"))?),
                RawPiece::Operand { index, modifier } => AsmPiece::Operand { index, modifier },
            });
        }
        let clobbers = self.v.asm_str(row.clobbers_start, row.clobbers_len);
        let id = self.b.asm_block(&lir_pieces, &lir_ops, options, clobbers);
        let ty = match members.as_slice() {
            [] => Ty::NONE,
            &[one] => one,
            many => self.b.compound(many),
        };
        let single = members.len() == 1;

        if labels.is_empty() {
            let v = self.b.asm(id, ty, &inputs);
            if noreturn {
                self.b.unreachable();
                return Ok(());
            }
            self.asm_store_outputs(v, single, &stores)?;
            return match target {
                Some(to) => self.goto(to),
                None => {
                    self.b.unreachable();
                    Ok(())
                }
            };
        }

        // Capture pre-output value locals for the label branches (outputs are written on fall-through only).
        let lbl_args = self.local_args()?;
        let fall = self.b.create_block(BlockKind::Normal);
        let mut bridge_blocks: Vec<Block, A> = Vec::with_capacity_in(labels.len(), self.alloc);
        for &(bridge, _) in labels.iter() {
            bridge_blocks.push(bridge);
        }
        let v = self.b.asm_br(id, ty, fall, &bridge_blocks, &inputs);
        for &(bridge, body_lbl) in labels.iter() {
            self.b.begin(bridge);
            let target_lir = self.lir_block(body_lbl)?;
            self.b.jump((target_lir, &lbl_args));
        }
        self.b.begin(fall);
        if noreturn {
            self.b.unreachable();
            return Ok(());
        }
        self.asm_store_outputs(v, single, &stores)?;
        match target {
            Some(to) => self.goto(to),
            None => {
                self.b.unreachable();
                Ok(())
            }
        }
    }
}
