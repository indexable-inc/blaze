//! Compiler intrinsics evaluated over the one body (`Machine::intrinsic`), matching
//! `blaze-consteval`'s full intrinsic set plus memory copy/write intrinsics (`copy`,
//! `copy_nonoverlapping`, `write_via_move`, `forget`, `size_of_val`, `align_of_val`, etc.).

use blaze_body::{BinOp, Operand, Ops};
use blaze_consteval::arith::{self, Plant};
use blaze_consteval::mem::{Origin, PTR, Pointer, Scalar};
use blaze_consteval::{Intrinsic, Layout, LayoutCx, Op, PanicPart, Refusal};
use blaze_own::{String, Vec};
use blaze_typeck::{AdtKind, List, Mutbl, Name, PNode, PTy};

use crate::error::Fault;
use crate::eval::{Machine, Opnd, Pl, R, Src, Stop, TypeIdTarget, layout_stop, malformed, unsupported};
use crate::world::{BodyWorld, DropGlue};

fn smin(bits: u32) -> i128 {
    if bits >= 128 { i128::MIN } else { -(1i128 << (bits - 1)) }
}

fn smax(bits: u32) -> i128 {
    if bits >= 128 { i128::MAX } else { (1i128 << (bits - 1)) - 1 }
}

fn umax(bits: u32) -> u128 {
    arith::mask(u128::MAX, bits)
}

fn i128_to_u128(v: i128) -> u128 {
    u128::from_le_bytes(v.to_le_bytes())
}

fn u128_low_u64(v: u128) -> u64 {
    let b = v.to_le_bytes();
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

impl<'w, 'g, 't, 'a, W: BodyWorld, L: Layout> Machine<'w, 'g, 't, 'a, W, L> {
    pub(crate) fn intrinsic(&mut self, i: Intrinsic, iargs: List, args: Ops, d: Option<Pl>) -> R<'a, ()> {
        let body = self.top()?.body;
        let mut ops = body.ops(args);
        let op0 = ops.next();
        let op1 = ops.next();
        let op2 = ops.next();
        let (t0, t1) = {
            let list = self.ground.view().list(iargs).map_err(|_| malformed("an intrinsic's arguments"))?;
            (list.first().copied(), list.get(1).copied())
        };
        let need_dest = |d: Option<Pl>| d.ok_or(Stop::from(malformed("an intrinsic call with no destination")));
        let one = || op0.ok_or(Stop::from(malformed("a one-operand intrinsic without its operand")));
        let two = || match (op0, op1) {
            (Some(a), Some(b)) => Ok((a, b)),
            _ => Err(Stop::from(malformed("a two-operand intrinsic without two operands"))),
        };
        let three = || match (op0, op1, op2) {
            (Some(a), Some(b), Some(c)) => Ok((a, b, c)),
            _ => Err(Stop::from(malformed("a three-operand intrinsic without three operands"))),
        };
        match i {
            Intrinsic::SizeOf | Intrinsic::AlignOf => {
                let t = t0.ok_or(Stop::from(malformed("`size_of` with no type")))?;
                let (size, align) = self.size_align(t)?;
                let v = if i == Intrinsic::SizeOf { u128::from(size) } else { 1u128 << align };
                self.put(need_dest(d)?, v)
            }
            Intrinsic::SizeOfVal | Intrinsic::AlignOfVal => {
                let ptr_op = one()?;
                let d = need_dest(d)?;
                let (size, align_log2) = self.size_align_of_val(ptr_op)?;
                let v = if i == Intrinsic::SizeOfVal { u128::from(size) } else { 1u128 << align_log2 };
                self.put(d, v)
            }
            Intrinsic::VariantCount => {
                let t = t0.ok_or(Stop::from(malformed("`variant_count` with no type")))?;
                let n = match self.node(t)? {
                    PNode::Adt(key, _) => {
                        let adt = self
                            .world
                            .adt(key)
                            .ok_or(Stop::from(Refusal::Missing { what: "ADT definition", item: key }))?;
                        match adt.kind {
                            AdtKind::Enum => u128::try_from(adt.variants.len()).unwrap_or(0),
                            AdtKind::Struct | AdtKind::Union => 0,
                        }
                    }
                    _ => 0,
                };
                self.put(need_dest(d)?, n)
            }
            Intrinsic::Transmute | Intrinsic::TransmuteUnchecked => {
                let a = one()?;
                let d = need_dest(d)?;
                let o = self.operand(a)?;
                let res = self.transmute_into(d, o);
                self.release(o);
                res
            }
            Intrinsic::UbChecks | Intrinsic::OverflowChecks => self.put(need_dest(d)?, 1),
            Intrinsic::Likely | Intrinsic::Unlikely | Intrinsic::BlackBox => {
                self.operand_into(one()?, need_dest(d)?.ptr)
            }
            Intrinsic::Forget | Intrinsic::ColdPath => Ok(()),
            Intrinsic::Assume => {
                if self.int(one()?)? == 0 {
                    return Err(Refusal::Unreachable.into());
                }
                Ok(())
            }
            Intrinsic::Unreachable => Err(Refusal::Unreachable.into()),
            Intrinsic::AbortImmediate => Err(Stop::Fault(Fault::Abort)),
            Intrinsic::WrappingAdd | Intrinsic::WrappingSub | Intrinsic::WrappingMul => {
                let (a, b) = two()?;
                let op = match i {
                    Intrinsic::WrappingAdd => BinOp::Add,
                    Intrinsic::WrappingSub => BinOp::Sub,
                    _ => BinOp::Mul,
                };
                let (s, _) = self.binary(op, a, b, true)?;
                let d = need_dest(d)?;
                let size = self.size(d.ty)?;
                self.mem_write(d.ptr, size, s)
            }
            Intrinsic::AddWithOverflow | Intrinsic::SubWithOverflow | Intrinsic::MulWithOverflow => {
                let (a, b) = two()?;
                let op = match i {
                    Intrinsic::AddWithOverflow => BinOp::Add,
                    Intrinsic::SubWithOverflow => BinOp::Sub,
                    _ => BinOp::Mul,
                };
                let (s, o) = self.binary(op, a, b, true)?;
                self.write_checked(need_dest(d)?, s, o)
            }
            Intrinsic::UncheckedAdd
            | Intrinsic::UncheckedSub
            | Intrinsic::UncheckedMul
            | Intrinsic::UncheckedDiv
            | Intrinsic::UncheckedRem
            | Intrinsic::UncheckedShl
            | Intrinsic::UncheckedShr
            | Intrinsic::ExactDiv => {
                let (a_op, b_op) = two()?;
                let d = need_dest(d)?;
                let (a_val, a_ty, b_val, b_ty) = self.read_two_ints(a_op, b_op)?;
                let (bits, signed) = self.int_info_of(a_ty)?;
                let rhs = self.int_info_of(b_ty)?;
                let op = match i {
                    Intrinsic::UncheckedAdd => blaze_typeck::BinOp::Add,
                    Intrinsic::UncheckedSub => blaze_typeck::BinOp::Sub,
                    Intrinsic::UncheckedMul => blaze_typeck::BinOp::Mul,
                    Intrinsic::UncheckedDiv | Intrinsic::ExactDiv => blaze_typeck::BinOp::Div,
                    Intrinsic::UncheckedRem => blaze_typeck::BinOp::Rem,
                    Intrinsic::UncheckedShl => blaze_typeck::BinOp::Shl,
                    _ => blaze_typeck::BinOp::Shr,
                };
                if i == Intrinsic::ExactDiv
                    && b_val != 0
                    && arith::int_binop(blaze_typeck::BinOp::Rem, a_val, b_val, bits, signed, rhs, Plant::None)
                        .is_ok_and(|r| r != 0)
                {
                    return Err(Refusal::Overflow { op: Op::Div }.into());
                }
                let r = arith::int_binop(op, a_val, b_val, bits, signed, rhs, Plant::None)?;
                self.put(d, r)
            }
            Intrinsic::SaturatingAdd | Intrinsic::SaturatingSub => {
                let (a_op, b_op) = two()?;
                let d = need_dest(d)?;
                let (bits, signed) = self.int_info_of(d.ty)?;
                let (a_val, _, b_val, _) = self.read_two_ints(a_op, b_op)?;
                let op = if i == Intrinsic::SaturatingAdd {
                    blaze_typeck::BinOp::Add
                } else {
                    blaze_typeck::BinOp::Sub
                };
                let r = match arith::int_binop(op, a_val, b_val, bits, signed, (bits, signed), Plant::None) {
                    Ok(v) => v,
                    Err(_) if signed => {
                        let up = if op == blaze_typeck::BinOp::Add {
                            arith::sext(b_val, bits) > 0
                        } else {
                            arith::sext(b_val, bits) < 0
                        };
                        arith::mask(i128_to_u128(if up { smax(bits) } else { smin(bits) }), bits)
                    }
                    Err(_) => {
                        if op == blaze_typeck::BinOp::Add {
                            umax(bits)
                        } else {
                            0
                        }
                    }
                };
                self.put(d, r)
            }
            Intrinsic::RotateLeft | Intrinsic::RotateRight => {
                let (a_op, n_op) = two()?;
                let d = need_dest(d)?;
                let (bits, _) = self.int_info_of(d.ty)?;
                let (a_val, _, n_raw, _) = self.read_two_ints(a_op, n_op)?;
                let n = u32::try_from(n_raw % u128::from(bits)).unwrap_or(0);
                let shift = if i == Intrinsic::RotateLeft { n } else { (bits - n) % bits };
                let r = if shift == 0 {
                    a_val
                } else {
                    arith::mask((a_val << shift) | (a_val >> (bits - shift)), bits)
                };
                self.put(d, r)
            }
            Intrinsic::Ctpop
            | Intrinsic::Ctlz
            | Intrinsic::CtlzNonzero
            | Intrinsic::Cttz
            | Intrinsic::CttzNonzero
            | Intrinsic::Bswap
            | Intrinsic::Bitreverse => {
                let a_op = one()?;
                let d = need_dest(d)?;
                let o = self.operand(a_op)?;
                let s = self.read(o);
                self.release(o);
                let a_val = self.uint(s?)?;
                let (bits, _) = self.int_info_of(o.ty)?;
                if a_val == 0 && matches!(i, Intrinsic::CtlzNonzero | Intrinsic::CttzNonzero) {
                    let name = if i == Intrinsic::CtlzNonzero { "ctlz_nonzero" } else { "cttz_nonzero" };
                    return Err(Refusal::NonzeroOnZero { name }.into());
                }
                let r = match i {
                    Intrinsic::Ctpop => u128::from(a_val.count_ones()),
                    Intrinsic::Ctlz | Intrinsic::CtlzNonzero => u128::from(a_val.leading_zeros() - (128 - bits)),
                    Intrinsic::Cttz | Intrinsic::CttzNonzero => u128::from(a_val.trailing_zeros().min(bits)),
                    Intrinsic::Bswap => a_val.swap_bytes() >> (128 - bits),
                    _ => a_val.reverse_bits() >> (128 - bits),
                };
                self.put(d, r)
            }
            Intrinsic::ThreeWayCompare => {
                let (a_op, b_op) = two()?;
                let d = need_dest(d)?;
                let (s, _) = self.binary(BinOp::Cmp, a_op, b_op, false)?;
                let size = self.size(d.ty)?;
                self.mem_write(d.ptr, size, s)
            }
            Intrinsic::FloatToIntUnchecked => {
                let a_op = one()?;
                let d = need_dest(d)?;
                let o = self.operand(a_op)?;
                let s = self.read(o);
                self.release(o);
                let v = self.uint(s?)?;
                let r = arith::num_cast(self.node(o.ty)?, self.node(d.ty)?, v)?;
                self.put(d, r)
            }
            Intrinsic::Offset | Intrinsic::ArithOffset => {
                let (p_op, c_op) = two()?;
                let d = need_dest(d)?;
                self.intrinsic_offset(i == Intrinsic::ArithOffset, p_op, c_op, d)
            }
            Intrinsic::PtrOffsetFrom | Intrinsic::PtrOffsetFromUnsigned => {
                let (a_op, b_op) = two()?;
                let d = need_dest(d)?;
                self.intrinsic_ptr_offset_from(i == Intrinsic::PtrOffsetFromUnsigned, a_op, b_op, d)
            }
            Intrinsic::PtrMetadata => {
                let a = one()?;
                let d = need_dest(d)?;
                let o = self.operand(a)?;
                let res = self.ptr_metadata_into(d, o);
                self.release(o);
                res
            }
            Intrinsic::AggregateRawPtr => {
                let (data_op, meta_op) = two()?;
                let d = need_dest(d)?;
                self.intrinsic_aggregate_raw_ptr(data_op, meta_op, d)
            }
            Intrinsic::PtrGuaranteedCmp => {
                let (a_op, b_op) = two()?;
                let d = need_dest(d)?;
                self.intrinsic_ptr_guaranteed_cmp(a_op, b_op, d)
            }
            Intrinsic::DiscriminantValue => {
                let a_op = one()?;
                let d = need_dest(d)?;
                self.intrinsic_discriminant_value(a_op, d)
            }
            Intrinsic::NeedsDrop => {
                let t = t0.ok_or(Stop::from(malformed("`needs_drop` with no type")))?;
                let d = need_dest(d)?;
                let needs = match self.world.drop_glue(t, &mut *self.ground, self.alloc)? {
                    DropGlue::Nothing => 0u128,
                    DropGlue::Call { .. } => 1u128,
                };
                self.put(d, needs)
            }
            Intrinsic::TypeName => {
                let t = t0.ok_or(Stop::from(malformed("`type_name` with no type")))?;
                let d = need_dest(d)?;
                self.intrinsic_type_name(t, d)
            }
            Intrinsic::TypeId => {
                let t = t0.ok_or(Stop::from(malformed("`type_id` with no type")))?;
                let d = need_dest(d)?;
                let bits = self.record_type_id(t)?;
                self.write_type_id(bits, d)
            }
            Intrinsic::TypeIdTypeOf => {
                let d = need_dest(d)?;
                let tid = self.read_type_id_op(one()?)?;
                let ty = self.lookup_type_id_ty(tid)?;
                self.write_type_info(ty, d)
            }
            Intrinsic::TypeIdVariants => {
                let d = need_dest(d)?;
                let tid = self.read_type_id_op(one()?)?;
                let ty = self.lookup_type_id_ty(tid)?;
                let n: usize = match self.node(ty)? {
                    PNode::Adt(k, _) => {
                        let adt = self
                            .world
                            .adt(k)
                            .ok_or(Stop::from(Refusal::Missing { what: "ADT definition", item: k }))?;
                        match adt.kind {
                            AdtKind::Enum => adt.variants.len(),
                            AdtKind::Struct | AdtKind::Union => 1,
                        }
                    }
                    PNode::Tuple(_) | PNode::Record(..) => 1,
                    _ => 0,
                };
                self.put(d, u128::try_from(n).unwrap_or(0))
            }
            Intrinsic::TypeIdFields => {
                let (id_op, v_op) = two()?;
                let d = need_dest(d)?;
                let tid = self.read_type_id_op(id_op)?;
                let ty = self.lookup_type_id_ty(tid)?;
                let v_ix = usize::try_from(self.int(v_op)?).map_err(|_| malformed("`type_id_fields` variant index overflow"))?;
                let n: u32 = match self.node(ty)? {
                    PNode::Adt(k, _) => {
                        let adt = self
                            .world
                            .adt(k)
                            .ok_or(Stop::from(Refusal::Missing { what: "ADT definition", item: k }))?;
                        let v = adt.variants.get(v_ix).ok_or(Stop::from(Refusal::OutOfBounds {
                            offset: u64::try_from(v_ix).unwrap_or(u64::MAX),
                            size: 1,
                            len: u64::try_from(adt.variants.len()).unwrap_or(u64::MAX),
                        }))?;
                        v.fields.len
                    }
                    PNode::Tuple(l) | PNode::Record(_, l) => {
                        if v_ix != 0 {
                            return Err(Refusal::OutOfBounds {
                                offset: u64::try_from(v_ix).unwrap_or(u64::MAX),
                                size: 1,
                                len: 1,
                            }
                            .into());
                        }
                        l.len
                    }
                    _ => {
                        if v_ix != 0 {
                            return Err(Refusal::OutOfBounds {
                                offset: u64::try_from(v_ix).unwrap_or(u64::MAX),
                                size: 1,
                                len: 1,
                            }
                            .into());
                        }
                        0
                    }
                };
                self.put(d, u128::from(n))
            }
            Intrinsic::TypeIdFieldRepresentingType => {
                let (id_op, v_op, f_op) = three()?;
                let d = need_dest(d)?;
                let tid = self.read_type_id_op(id_op)?;
                let base = self.lookup_type_id_ty(tid)?;
                let variant = u32::try_from(self.int(v_op)?).map_err(|_| malformed("`type_id_field_representing_type` variant overflow"))?;
                let field = u32::try_from(self.int(f_op)?).map_err(|_| malformed("`type_id_field_representing_type` field overflow"))?;
                let field_count = match self.node(base)? {
                    PNode::Adt(k, _) => {
                        let adt = self
                            .world
                            .adt(k)
                            .ok_or(Stop::from(Refusal::Missing { what: "ADT definition", item: k }))?;
                        let v_ix = usize::try_from(variant).map_err(|_| malformed("variant index"))?;
                        let v = adt.variants.get(v_ix).ok_or(Stop::from(Refusal::OutOfBounds {
                            offset: u64::from(variant),
                            size: 1,
                            len: u64::try_from(adt.variants.len()).unwrap_or(u64::MAX),
                        }))?;
                        v.fields.len
                    }
                    PNode::Tuple(l) | PNode::Record(_, l) => {
                        if variant != 0 {
                            return Err(Refusal::OutOfBounds { offset: u64::from(variant), size: 1, len: 1 }.into());
                        }
                        l.len
                    }
                    _ => return Err(Refusal::OutOfBounds { offset: u64::from(field), size: 1, len: 0 }.into()),
                };
                if field >= field_count {
                    return Err(Refusal::OutOfBounds {
                        offset: u64::from(field),
                        size: 1,
                        len: u64::from(field_count),
                    }
                    .into());
                }
                let frt_bits = frt_hash(tid, variant, field);
                if !self.type_ids.iter().any(|&(k, _)| k == frt_bits) {
                    self.type_ids.push((frt_bits, TypeIdTarget::Field { base, variant, field }));
                }
                self.write_type_id(frt_bits, d)
            }
            Intrinsic::FieldRepresentingTypeActualTypeId => {
                let d = need_dest(d)?;
                let frt = self.read_type_id_op(one()?)?;
                let (base, variant, field) = self.lookup_type_id_field(frt)?;
                let ft = self.field_ty(base, variant, field)?;
                let bits = self.record_type_id(ft)?;
                self.write_type_id(bits, d)
            }
            Intrinsic::FieldRepresentingTypeOffset => {
                let d = need_dest(d)?;
                let frt = self.read_type_id_op(one()?)?;
                let (base, variant, field) = self.lookup_type_id_field(frt)?;
                let off = self.field_offset(base, variant, field)?;
                self.put(d, u128::from(off))
            }
            Intrinsic::TypeIdIsSigned => {
                let d = need_dest(d)?;
                let tid = self.read_type_id_op(one()?)?;
                let ty = self.lookup_type_id_ty(tid)?;
                self.put(d, u128::from(matches!(self.node(ty)?, PNode::Int(_))))
            }
            Intrinsic::TypeIdArrayLen => {
                let d = need_dest(d)?;
                let tid = self.read_type_id_op(one()?)?;
                let ty = self.lookup_type_id_ty(tid)?;
                let len = match self.node(ty)? {
                    PNode::Array(_, n) => u128::from(
                        self.ground
                            .view()
                            .value_u64(n)
                            .ok_or(Stop::from(malformed("an array length that is no value")))?,
                    ),
                    _ => 0,
                };
                self.put(d, len)
            }
            Intrinsic::TypeIdElementTy => {
                let d = need_dest(d)?;
                let tid = self.read_type_id_op(one()?)?;
                let ty = self.lookup_type_id_ty(tid)?;
                let elem = match self.node(ty)? {
                    PNode::Array(elem, _) | PNode::Slice(elem) => Some(elem),
                    _ => None,
                };
                self.write_option_type_id(d, elem)
            }
            Intrinsic::TypeIdSizeOf => {
                let d = need_dest(d)?;
                let tid = self.read_type_id_op(one()?)?;
                let ty = self.lookup_type_id_ty(tid)?;
                if self.is_unsized(ty)? {
                    self.write_option_u64(d, None)
                } else {
                    let (sz, _) = self.size_align(ty)?;
                    self.write_option_u64(d, Some(sz))
                }
            }
            Intrinsic::TypeIdPointsTo => {
                let d = need_dest(d)?;
                let tid = self.read_type_id_op(one()?)?;
                let ty = self.lookup_type_id_ty(tid)?;
                let pointee = match self.node(ty)? {
                    PNode::Ref(_, p) | PNode::Ptr(_, p) => Some(p),
                    _ => None,
                };
                self.write_option_type_id(d, pointee)
            }
            Intrinsic::TypeIdPointsMutably => {
                let d = need_dest(d)?;
                let tid = self.read_type_id_op(one()?)?;
                let ty = self.lookup_type_id_ty(tid)?;
                self.put(d, u128::from(matches!(self.node(ty)?, PNode::Ref(Mutbl::Mut, _) | PNode::Ptr(Mutbl::Mut, _))))
            }
            Intrinsic::AssertInhabited | Intrinsic::AssertZeroValid | Intrinsic::AssertMemUninitializedValid => {
                let t = t0.ok_or(Stop::from(malformed("validity assertion type argument")))?;
                let check = blaze_build::intrinsics::validity::Check::of(i)
                    .ok_or(Stop::from(malformed("validity assertion")))?;
                let ok = {
                    let mut cx = LayoutCx { ground: &mut *self.ground, facts: self.world, scratch: self.alloc };
                    self.layout.permits(check, t, &mut cx).map_err(layout_stop)?
                };
                if !ok {
                    let (pre, post) = check.message();
                    let mut text = Vec::with_capacity_in(pre.len() + post.len(), self.alloc);
                    text.extend_from_slice(pre.as_bytes());
                    text.extend_from_slice(post.as_bytes());
                    let mut parts = Vec::with_capacity_in(1, self.alloc);
                    parts.push(PanicPart::Str(text));
                    return Err(Refusal::Panic { parts }.into());
                }
                Ok(())
            }
            Intrinsic::BlazeIntCast => {
                let (from, to) = (
                    t0.ok_or(Stop::from(malformed("`blaze_int_cast` types")))?,
                    t1.ok_or(Stop::from(malformed("`blaze_int_cast` types")))?,
                );
                let ((fb, fs), (tb, _)) = (self.int_info_of(from)?, self.int_info_of(to)?);
                let v = self.int(one()?)?;
                self.put(need_dest(d)?, blaze_build::intrinsics::int_cast_bits(v, fb, fs, tb))
            }
            Intrinsic::BlazeFieldCount => {
                let t = t0.ok_or(Stop::from(malformed("`blaze_field_count` with no type")))?;
                let count = blaze_build::intrinsics::field_count(t, self.ground.view(), self.world).map_err(|e| match e {
                    blaze_build::intrinsics::FieldCountError::MissingType => malformed("`blaze_field_count` of a type not in the ground table"),
                    blaze_build::intrinsics::FieldCountError::NoAdt(item) => Refusal::Missing { what: "ADT definition", item },
                })?;
                self.put(need_dest(d)?, u128::from(count))
            }
            Intrinsic::BlazeRecordsDisjoint => {
                let (a, b) = (
                    t0.ok_or(Stop::from(malformed("`blaze_records_disjoint` types")))?,
                    t1.ok_or(Stop::from(malformed("`blaze_records_disjoint` types")))?,
                );
                let disjoint = blaze_build::intrinsics::records_disjoint(a, b, self.ground.view()).map_err(|e| match e {
                    blaze_build::intrinsics::DisjointError::MissingType => malformed("`blaze_records_disjoint` of a type not in the ground table"),
                    blaze_build::intrinsics::DisjointError::MissingRecordNames => malformed("`blaze_records_disjoint` of a record without registered schema field names"),
                })?;
                self.put(need_dest(d)?, u128::from(disjoint))
            }
            Intrinsic::BlazeRecordSchemaId => {
                let t = t0.ok_or(Stop::from(malformed("`blaze_record_schema_id` with no type")))?;
                let id = blaze_build::intrinsics::record_schema_id(t, self.ground.view())
                    .ok_or(Stop::from(malformed("`blaze_record_schema_id` of a type not in the ground table")))?;
                self.put(need_dest(d)?, u128::from(id))
            }
            Intrinsic::BlazeIsEnum => {
                let t = t0.ok_or(Stop::from(malformed("`blaze_is_enum` with no type")))?;
                let yes = blaze_build::intrinsics::is_enum(t, self.ground.view(), self.world).map_err(|e| match e {
                    blaze_build::intrinsics::FieldCountError::MissingType => malformed("`blaze_is_enum` of a type not in the ground table"),
                    blaze_build::intrinsics::FieldCountError::NoAdt(item) => Refusal::Missing { what: "ADT definition", item },
                })?;
                self.put(need_dest(d)?, u128::from(yes))
            }
            Intrinsic::BlazeIsUnitEnum => {
                let t = t0.ok_or(Stop::from(malformed("`blaze_is_unit_enum` with no type")))?;
                let yes = blaze_build::intrinsics::is_unit_enum(t, self.ground.view(), self.world).map_err(|e| match e {
                    blaze_build::intrinsics::FieldCountError::MissingType => malformed("`blaze_is_unit_enum` of a type not in the ground table"),
                    blaze_build::intrinsics::FieldCountError::NoAdt(item) => Refusal::Missing { what: "ADT definition", item },
                })?;
                self.put(need_dest(d)?, u128::from(yes))
            }
            Intrinsic::BlazeVariantCount => {
                let t = t0.ok_or(Stop::from(malformed("`blaze_variant_count` with no type")))?;
                let count = blaze_build::intrinsics::variant_count(t, self.ground.view(), self.world).map_err(|e| match e {
                    blaze_build::intrinsics::FieldCountError::MissingType => malformed("`blaze_variant_count` of a type not in the ground table"),
                    blaze_build::intrinsics::FieldCountError::NoAdt(item) => Refusal::Missing { what: "ADT definition", item },
                })?;
                self.put(need_dest(d)?, u128::from(count))
            }
            Intrinsic::BlazeVariantName => {
                let t = t0.ok_or(Stop::from(malformed("`blaze_variant_name` with no type")))?;
                let raw_idx = self.int(one()?)?;
                let d = need_dest(d)?;
                self.intrinsic_blaze_variant_name(t, raw_idx, d)
            }
            Intrinsic::RawEq | Intrinsic::CompareBytes => {
                let (a_op, b_op) = two()?;
                let d = need_dest(d)?;
                let n = match i {
                    Intrinsic::RawEq => self.size(t0.ok_or(Stop::from(malformed("`raw_eq` with no type")))?)?,
                    _ => {
                        let c_op = op2.ok_or(Stop::from(malformed("`compare_bytes` without count")))?;
                        u64::try_from(self.int(c_op)?).map_err(|_| malformed("a `usize` past 64 bits"))?
                    }
                };
                self.intrinsic_byte_cmp(i == Intrinsic::RawEq, a_op, b_op, n, d)
            }
            Intrinsic::ReadViaCopy => {
                let t = t0.ok_or(Stop::from(malformed("`read_via_copy` with no type")))?;
                let ptr_op = one()?;
                let d = need_dest(d)?;
                self.intrinsic_read_via_copy(t, ptr_op, d)
            }
            Intrinsic::WriteViaMove => {
                let t = t0.ok_or(Stop::from(malformed("`write_via_move` with no type")))?;
                let (ptr_op, val_op) = two()?;
                self.intrinsic_write_via_move(t, ptr_op, val_op)
            }
            Intrinsic::WriteBytes => {
                let t = t0.ok_or(Stop::from(malformed("`write_bytes` with no type")))?;
                let (dst_op, val_op, count_op) = three()?;
                self.intrinsic_write_bytes(t, dst_op, val_op, count_op)
            }
            Intrinsic::Copy | Intrinsic::CopyNonoverlapping => {
                let t = t0.ok_or(Stop::from(malformed("`copy` with no type")))?;
                let (src_op, dst_op, count_op) = three()?;
                self.intrinsic_copy(t, i == Intrinsic::CopyNonoverlapping, src_op, dst_op, count_op)
            }
            Intrinsic::SliceGetUnchecked => {
                let (slice_op, idx_op) = two()?;
                let d = need_dest(d)?;
                self.intrinsic_slice_get_unchecked(slice_op, idx_op, d)
            }
            other => Err(unsupported(other.entry().name).into()),
        }
    }

    fn int_info_of(&self, t: PTy) -> R<'a, (u32, bool)> {
        arith::int_info(self.node(t)?).ok_or(Stop::from(malformed("an intrinsic of a type that is not an integer")))
    }

    fn read_two_ints(&mut self, a_op: Operand, b_op: Operand) -> R<'a, (u128, PTy, u128, PTy)> {
        let a = self.operand(a_op)?;
        let b = match self.operand(b_op) {
            Ok(b) => b,
            Err(e) => {
                self.release(a);
                return Err(e);
            }
        };
        let ra = match self.read(a) {
            Ok(s) => s,
            Err(e) => {
                self.release(a);
                self.release(b);
                return Err(e);
            }
        };
        let rb = self.read(b);
        self.release(a);
        self.release(b);
        Ok((self.uint(ra)?, a.ty, self.uint(rb?)?, b.ty))
    }

    fn read_ptr_scalar(&mut self, op: Operand) -> R<'a, (Scalar, PTy)> {
        let o = self.operand(op)?;
        let res = (|| -> R<'a, Scalar> {
            match o.src {
                Src::At(p) => self.mem_read(p, PTR),
                Src::Temp(id) => self.mem_read(Pointer { alloc: id, offset: 0 }, PTR),
                Src::Imm(b) => Ok(Scalar::Int(b)),
            }
        })();
        self.release(o);
        Ok((res?, o.ty))
    }

    fn read_ptr_pair(&mut self, op: Operand) -> R<'a, (Scalar, Option<Scalar>, PTy)> {
        let o = self.operand(op)?;
        let res = (|| -> R<'a, (Scalar, Option<Scalar>)> {
            let (PNode::Ref(_, t) | PNode::Ptr(_, t)) = self.node(o.ty)? else {
                return Err(malformed("expected a pointer operand").into());
            };
            let unsized_ = self.is_unsized(t)?;
            match o.src {
                Src::At(p) => {
                    let first = self.mem_read(p, PTR)?;
                    let second = if unsized_ { Some(self.mem_read(p.at(PTR), PTR)?) } else { None };
                    Ok((first, second))
                }
                Src::Temp(id) => {
                    let p = Pointer { alloc: id, offset: 0 };
                    let first = self.mem_read(p, PTR)?;
                    let second = if unsized_ { Some(self.mem_read(p.at(PTR), PTR)?) } else { None };
                    Ok((first, second))
                }
                Src::Imm(b) if !unsized_ => Ok((Scalar::Int(b), None)),
                Src::Imm(_) => Err(malformed("a wide pointer as an immediate").into()),
            }
        })();
        self.release(o);
        let (first, second) = res?;
        Ok((first, second, o.ty))
    }

    fn size_align_of_val(&mut self, ptr_op: Operand) -> R<'a, (u64, u8)> {
        let (_, meta, ptr_ty) = self.read_ptr_pair(ptr_op)?;
        let (PNode::Ref(_, pointee) | PNode::Ptr(_, pointee)) = self.node(ptr_ty)? else {
            return Err(malformed("`size_of_val`/`align_of_val` of a non-pointer").into());
        };
        if !self.is_unsized(pointee)? {
            return self.size_align(pointee);
        }
        match self.node(pointee)? {
            PNode::Str => {
                let m = meta.ok_or(Stop::from(malformed("a `&str` without length metadata")))?;
                let len = u64::try_from(self.uint(m)?).map_err(|_| malformed("a `str` length past u64"))?;
                Ok((len, 0))
            }
            PNode::Slice(elem) => {
                let m = meta.ok_or(Stop::from(malformed("a `&[T]` without length metadata")))?;
                let len = u64::try_from(self.uint(m)?).map_err(|_| malformed("a slice length past u64"))?;
                let (esize, ealign) = self.size_align(elem)?;
                let total = len.checked_mul(esize).ok_or(Stop::from(Refusal::OutOfBounds {
                    offset: u64::MAX,
                    size: esize,
                    len,
                }))?;
                Ok((total, ealign))
            }
            PNode::Dynamic(_) => {
                let m = meta.ok_or(Stop::from(malformed("a `&dyn Trait` without vtable metadata")))?;
                let Scalar::Ptr(vp) = m else {
                    return Err(malformed("a `&dyn Trait` whose metadata is not a vtable pointer").into());
                };
                let Origin::Vtable(vkey) = self.mem.get(vp.alloc)?.origin else {
                    return Err(malformed("a `&dyn Trait` whose metadata does not point to a vtable").into());
                };
                let Some(vt) = self.vtables.iter().find(|&&(k, _, _)| k == vkey).and_then(|&(_, v, _)| v) else {
                    return Err(unsupported("`size_of_val`/`align_of_val` of an unregistered vtable").into());
                };
                self.size_align(vt.vtable.concrete)
            }
            _ => Err(unsupported("`size_of_val`/`align_of_val` of a custom DST struct").into()),
        }
    }

    fn intrinsic_offset(&mut self, wrapping: bool, p_op: Operand, c_op: Operand, d: Pl) -> R<'a, ()> {
        let (pv, pt) = self.read_ptr_scalar(p_op)?;
        let c_opnd = self.operand(c_op)?;
        let c_scalar = self.read(c_opnd);
        self.release(c_opnd);
        let count = self.uint(c_scalar?)?;
        let (ctbits, csigned) = self.int_info_of(c_opnd.ty)?;
        let (PNode::Ptr(_, pointee) | PNode::Ref(_, pointee)) = self.node(pt)? else {
            return Err(malformed("`offset` of a value that is not a pointer").into());
        };
        let (size, _) = self.size_align(pointee)?;
        let count_signed = if csigned {
            arith::sext(count, ctbits)
        } else {
            i128::try_from(count).map_err(|_| Stop::from(Refusal::OutOfBounds { offset: u64::MAX, size: 0, len: 0 }))?
        };
        let Scalar::Ptr(p) = pv else {
            if wrapping {
                let delta = u128_low_u64(i128_to_u128(count_signed)).wrapping_mul(size);
                let base = u128_low_u64(self.uint(pv)?);
                return self.mem_write(d.ptr, PTR, Scalar::Int(u128::from(base.wrapping_add(delta))));
            }
            return Err(Refusal::IntAsPointer.into());
        };
        if wrapping {
            let delta = u128_low_u64(i128_to_u128(count_signed)).wrapping_mul(size);
            return self.mem_write(d.ptr, PTR, Scalar::Ptr(p.at(delta)));
        }
        if count_signed == 0 && self.is_mut_static_alloc(p.alloc) {
            return self.mem_write(d.ptr, PTR, Scalar::Ptr(p));
        }
        let len = self.alloc_len(p.alloc)?;
        let Some(delta) = count_signed.checked_mul(i128::from(size)) else {
            return Err(Refusal::OutOfBounds { offset: u64::MAX, size: 0, len }.into());
        };
        if delta.unsigned_abs() > u128::try_from(isize::MAX).unwrap_or(u128::MAX) {
            return Err(Refusal::OutOfBounds { offset: u64::MAX, size: 0, len }.into());
        }
        let Some(off) = i128::from(p.offset).checked_add(delta) else {
            return Err(Refusal::OutOfBounds { offset: u64::MAX, size: 0, len }.into());
        };
        if off < 0 || off > i128::from(len) {
            let clamped = u64::try_from(off).unwrap_or(if off < 0 { 0 } else { u64::MAX });
            return Err(Refusal::OutOfBounds { offset: clamped, size: 0, len }.into());
        }
        let new_off = u64::try_from(off).unwrap_or(0);
        self.mem_write(d.ptr, PTR, Scalar::Ptr(Pointer { alloc: p.alloc, offset: new_off }))
    }

    fn intrinsic_ptr_offset_from(&mut self, unsigned: bool, a_op: Operand, b_op: Operand, d: Pl) -> R<'a, ()> {
        let (sa, ta) = self.read_ptr_scalar(a_op)?;
        let (sb, _) = self.read_ptr_scalar(b_op)?;
        let (PNode::Ptr(_, pointee) | PNode::Ref(_, pointee)) = self.node(ta)? else {
            return Err(malformed("`ptr_offset_from` of a value that is not a pointer").into());
        };
        let size = self.size(pointee)?;
        if size == 0 {
            return Err(Refusal::DivByZero.into());
        }
        let (a_off, b_off) = match (sa, sb) {
            (Scalar::Ptr(p), Scalar::Ptr(q)) => {
                if p.alloc != q.alloc {
                    return Err(Refusal::PointerCompare.into());
                }
                (p.offset, q.offset)
            }
            (Scalar::Int(a), Scalar::Int(b)) => (u128_low_u64(a), u128_low_u64(b)),
            _ => return Err(Refusal::PointerCompare.into()),
        };
        if unsigned {
            let Some(diff) = a_off.checked_sub(b_off) else {
                return Err(Refusal::Overflow { op: Op::Sub }.into());
            };
            if diff % size != 0 {
                return Err(Refusal::Overflow { op: Op::Div }.into());
            }
            self.put(d, u128::from(diff / size))
        } else {
            let diff = i128::from(a_off) - i128::from(b_off);
            let sz = i128::from(size);
            if diff % sz != 0 {
                return Err(Refusal::Overflow { op: Op::Div }.into());
            }
            self.put(d, i128_to_u128(diff / sz))
        }
    }

    fn intrinsic_aggregate_raw_ptr(&mut self, data_op: Operand, meta_op: Operand, d: Pl) -> R<'a, ()> {
        let (data_s, _) = self.read_ptr_scalar(data_op)?;
        let m = self.operand(meta_op)?;
        let res = (|| -> R<'a, ()> {
            let msize = self.size(m.ty)?;
            self.mem_write(d.ptr, PTR, data_s)?;
            if msize != 0 {
                let ms = self.read(m)?;
                self.mem_write(d.ptr.at(PTR), PTR, ms)?;
            }
            Ok(())
        })();
        self.release(m);
        res
    }

    fn intrinsic_ptr_guaranteed_cmp(&mut self, a_op: Operand, b_op: Operand, d: Pl) -> R<'a, ()> {
        let (x, _) = self.read_ptr_scalar(a_op)?;
        let (y, _) = self.read_ptr_scalar(b_op)?;
        let ans = match (x, y) {
            (Scalar::Int(i), Scalar::Int(j)) => u128::from(i == j),
            (Scalar::Ptr(p), Scalar::Int(i)) | (Scalar::Int(i), Scalar::Ptr(p)) => {
                if self.is_mut_static_alloc(p.alloc) {
                    if i == 0 && p.offset == 0 { 0 } else { 2 }
                } else {
                    let len = self.alloc_len(p.alloc)?;
                    if i == 0 && p.offset <= len { 0 } else { 2 }
                }
            }
            (Scalar::Ptr(p), Scalar::Ptr(q)) if p.alloc == q.alloc => u128::from(p.offset == q.offset),
            (Scalar::Ptr(p), Scalar::Ptr(q)) => {
                if self.is_mut_static_alloc(p.alloc) || self.is_mut_static_alloc(q.alloc) {
                    2
                } else {
                    let plen = self.alloc_len(p.alloc)?;
                    let qlen = self.alloc_len(q.alloc)?;
                    let pa = self.mem.get(p.alloc)?;
                    let qa = self.mem.get(q.alloc)?;
                    let p_own = matches!(pa.origin, Origin::Local | Origin::Static(_));
                    let q_own = matches!(qa.origin, Origin::Local | Origin::Static(_));
                    let inside = p.offset < plen && q.offset < qlen;
                    if inside && (p_own || q_own) { 0 } else { 2 }
                }
            }
        };
        self.put(d, ans)
    }

    fn intrinsic_discriminant_value(&mut self, a_op: Operand, d: Pl) -> R<'a, ()> {
        let (first, meta, pt) = self.read_ptr_pair(a_op)?;
        let (PNode::Ref(_, pointee) | PNode::Ptr(_, pointee)) = self.node(pt)? else {
            return Err(malformed("`discriminant_value` of a value that is not a reference").into());
        };
        let Scalar::Ptr(ptr) = first else {
            return Err(Refusal::IntAsPointer.into());
        };
        let pl = Pl { ptr, meta, ty: pointee, variant: 0 };
        let v = match self.node(pointee)? {
            PNode::Adt(adt_key, _) => {
                let adt = self
                    .world
                    .adt(adt_key)
                    .ok_or(Stop::from(Refusal::Missing { what: "ADT definition", item: adt_key }))?;
                if adt.kind == AdtKind::Enum {
                    i128_to_u128(self.discriminant(pl)?)
                } else {
                    0
                }
            }
            PNode::Union(_) => i128_to_u128(self.discriminant(pl)?),
            _ => 0,
        };
        self.put(d, v)
    }

    fn intrinsic_type_name(&mut self, t: PTy, d: Pl) -> R<'a, ()> {
        let (id, len) = match self.type_names.iter().find(|&&(ty, _, _)| ty == t) {
            Some(&(_, id, len)) => (id, len),
            None => {
                let names = self
                    .world
                    .type_names()
                    .ok_or(Stop::from(unsupported("`type_name` in a world that names no items (`ConstWorld::type_names`)")))?;
                let text = blaze_build::type_name::type_name_text(self.ground.view(), t, names, self.alloc).map_err(|error| match error {
                    blaze_build::type_name::TypeNameError::NoPath { item } => {
                        Refusal::Missing { what: "the path of an item `type_name` prints", item }
                    }
                    other => unsupported(other.what()),
                })?;
                let len = u64::try_from(text.len()).map_err(|_| malformed("type_name length overflow"))?;
                let id = self.mem.allocate(len, 0, false, Origin::Data)?;
                self.mem.fill(id, text.as_bytes(), &[])?;
                self.type_names.push((t, id, len));
                (id, len)
            }
        };
        self.mem_write(d.ptr, PTR, Scalar::Ptr(Pointer { alloc: id, offset: 0 }))?;
        self.mem_write(d.ptr.at(PTR), PTR, Scalar::Int(u128::from(len)))
    }

    fn intrinsic_blaze_variant_name(&mut self, t: PTy, raw_idx: u128, d: Pl) -> R<'a, ()> {
        let mut text = String::new_in(self.alloc);
        if let PNode::Adt(adt_key, _) = self.node(t)?
            && let Ok(v_idx) = u32::try_from(raw_idx)
        {
            let adt = self
                .world
                .adt(adt_key)
                .ok_or(Stop::from(Refusal::Missing { what: "ADT definition", item: adt_key }))?;
            if adt.kind == AdtKind::Enum && usize::try_from(v_idx).is_ok_and(|ix| ix < adt.variants.len()) {
                let names = self.world.type_names().ok_or(Stop::from(unsupported(
                    "`blaze_variant_name` in a world that names no items (`ConstWorld::type_names`)",
                )))?;
                let _ = names.variant_name(adt_key, v_idx, &mut |s| text.push_str(s));
            }
        }
        let len = u64::try_from(text.len()).unwrap_or(0);
        let id = self.mem.allocate(len, 0, false, Origin::Data)?;
        if len > 0 {
            self.mem.fill(id, text.as_bytes(), &[])?;
        }
        self.mem_write(d.ptr, PTR, Scalar::Ptr(Pointer { alloc: id, offset: 0 }))?;
        self.mem_write(d.ptr.at(PTR), PTR, Scalar::Int(u128::from(len)))
    }

    fn intrinsic_byte_cmp(&mut self, eq_only: bool, a_op: Operand, b_op: Operand, n: u64, d: Pl) -> R<'a, ()> {
        let (sa, _) = self.read_ptr_scalar(a_op)?;
        let (sb, _) = self.read_ptr_scalar(b_op)?;
        let (Scalar::Ptr(pa), Scalar::Ptr(pb)) = (sa, sb) else {
            if n == 0 {
                return self.put(d, if eq_only { 1 } else { 0 });
            }
            return Err(Refusal::IntAsPointer.into());
        };
        let left = self.mem_read_bytes(pa, n)?;
        let right = self.mem_read_bytes(pb, n)?;
        let order = blaze_build::intrinsics::byte_order(left, right);
        if eq_only {
            self.put(d, u128::from(order == 0))
        } else {
            self.put(d, u128::from(u32::from_le_bytes(order.to_le_bytes())))
        }
    }

    fn check_nonnull_aligned_int(&self, addr: u128, align_log2: u8) -> R<'a, ()> {
        let align = 1u64 << align_log2;
        let off = u64::try_from(addr).map_err(|_| Stop::from(Refusal::IntAsPointer))?;
        if off == 0 || !off.is_multiple_of(align) {
            return Err(Refusal::IntAsPointer.into());
        }
        Ok(())
    }

    fn intrinsic_read_via_copy(&mut self, t: PTy, ptr_op: Operand, d: Pl) -> R<'a, ()> {
        let (ps, _) = self.read_ptr_scalar(ptr_op)?;
        let (size, align_log2) = self.size_align(t)?;
        let ptr = match ps {
            Scalar::Ptr(p) => p,
            Scalar::Int(addr) => {
                if size == 0 {
                    self.check_nonnull_aligned_int(addr, align_log2)?;
                    return Ok(());
                }
                return Err(Refusal::IntAsPointer.into());
            }
        };
        if size == 0 {
            self.ensure_loaded(ptr.alloc)?;
            let _ = self.mem.read_bytes(ptr, 0)?;
            return Ok(());
        }
        self.mem_copy(ptr, d.ptr, size)?;
        if matches!(self.node(t)?, PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_) | PNode::Float(_)) {
            let s = self.mem_read(d.ptr, size)?;
            self.check_scalar_valid(t, s)?;
        }
        Ok(())
    }

    fn intrinsic_write_via_move(&mut self, t: PTy, ptr_op: Operand, val_op: Operand) -> R<'a, ()> {
        let (ps, _) = self.read_ptr_scalar(ptr_op)?;
        let (size, align_log2) = self.size_align(t)?;
        let ptr = match ps {
            Scalar::Ptr(p) => p,
            Scalar::Int(addr) => {
                if size == 0 {
                    self.check_nonnull_aligned_int(addr, align_log2)?;
                    let v = self.operand(val_op)?;
                    self.release(v);
                    return Ok(());
                }
                return Err(Refusal::IntAsPointer.into());
            }
        };
        if size == 0 {
            self.ensure_loaded(ptr.alloc)?;
            let v = self.operand(val_op)?;
            self.release(v);
            return Ok(());
        }
        self.operand_into(val_op, ptr)
    }

    fn intrinsic_write_bytes(&mut self, t: PTy, dst_op: Operand, val_op: Operand, count_op: Operand) -> R<'a, ()> {
        let (ps, _) = self.read_ptr_scalar(dst_op)?;
        let byte = u8::try_from(self.int(val_op)? & 0xff).unwrap_or(0);
        let count = u64::try_from(self.int(count_op)?).map_err(|_| malformed("`write_bytes` count past 64 bits"))?;
        let (size, align_log2) = self.size_align(t)?;
        let total = size.checked_mul(count).ok_or(Stop::from(Refusal::OutOfBounds {
            offset: u64::MAX,
            size,
            len: 0,
        }))?;
        match ps {
            Scalar::Int(addr) => {
                if total == 0 {
                    self.check_nonnull_aligned_int(addr, align_log2)?;
                    return Ok(());
                }
                Err(Refusal::IntAsPointer.into())
            }
            Scalar::Ptr(p) => {
                if total == 0 {
                    return Ok(());
                }
                self.mem_memset(p, total, byte)
            }
        }
    }

    fn intrinsic_copy(
        &mut self,
        t: PTy,
        nonoverlapping: bool,
        src_op: Operand,
        dst_op: Operand,
        count_op: Operand,
    ) -> R<'a, ()> {
        let (ss, _) = self.read_ptr_scalar(src_op)?;
        let (ds, _) = self.read_ptr_scalar(dst_op)?;
        let count = u64::try_from(self.int(count_op)?).map_err(|_| malformed("`copy` count past 64 bits"))?;
        let (size, align_log2) = self.size_align(t)?;
        let total = size.checked_mul(count).ok_or(Stop::from(Refusal::OutOfBounds {
            offset: u64::MAX,
            size,
            len: 0,
        }))?;
        if total == 0 {
            if let Scalar::Int(addr) = ss {
                self.check_nonnull_aligned_int(addr, align_log2)?;
            }
            if let Scalar::Int(addr) = ds {
                self.check_nonnull_aligned_int(addr, align_log2)?;
            }
            return Ok(());
        }
        let (Scalar::Ptr(sp), Scalar::Ptr(dp)) = (ss, ds) else {
            return Err(Refusal::IntAsPointer.into());
        };
        if nonoverlapping && sp.alloc == dp.alloc {
            let s_end = sp.offset.saturating_add(total);
            let d_end = dp.offset.saturating_add(total);
            if sp.offset < d_end && dp.offset < s_end {
                return Err(Refusal::Unreachable.into());
            }
        }
        self.mem_copy(sp, dp, total)
    }

    fn intrinsic_slice_get_unchecked(&mut self, slice_op: Operand, idx_op: Operand, d: Pl) -> R<'a, ()> {
        let (first, meta, pt) = self.read_ptr_pair(slice_op)?;
        let idx = u64::try_from(self.int(idx_op)?).map_err(|_| malformed("`slice_get_unchecked` index past u64"))?;
        let (PNode::Ref(_, pointee) | PNode::Ptr(_, pointee)) = self.node(pt)? else {
            return Err(malformed("`slice_get_unchecked` on a non-pointer").into());
        };
        let PNode::Slice(elem) = self.node(pointee)? else {
            return Err(malformed("`slice_get_unchecked` on a non-slice pointer").into());
        };
        let m = meta.ok_or(Stop::from(malformed("`slice_get_unchecked` without slice length")))?;
        let len = u64::try_from(self.uint(m)?).map_err(|_| malformed("slice length past u64"))?;
        if idx > len {
            return Err(Refusal::IndexOutOfBounds { index: idx, len }.into());
        }
        let Scalar::Ptr(p) = first else {
            return Err(Refusal::IntAsPointer.into());
        };
        let stride = self.size(elem)?;
        let off = idx.checked_mul(stride).ok_or(Stop::from(Refusal::OutOfBounds {
            offset: u64::MAX,
            size: stride,
            len,
        }))?;
        self.mem_write(d.ptr, PTR, Scalar::Ptr(p.at(off)))
    }

    // ---- TypeId reflection helpers ----

    fn record_type_id(&mut self, t: PTy) -> R<'a, u128> {
        let bits = blaze_build::instance::type_id_hash(t, self.ground.view(), self.alloc).map_err(|_| {
            unsupported("`type_id` of a type that is not ground (an alias, parameter or inference variable left)")
        })?;
        if !self.type_ids.iter().any(|&(k, _)| k == bits) {
            self.type_ids.push((bits, TypeIdTarget::Ty(t)));
        }
        Ok(bits)
    }

    fn write_type_id(&mut self, bits: u128, d: Pl) -> R<'a, ()> {
        let sz = self.size(d.ty)?;
        if sz != 16 {
            return Err(malformed("a `type_id` result that is not `TypeId`'s 16 bytes").into());
        }
        self.mem_write(d.ptr, 8, Scalar::Int(bits & u128::from(u64::MAX)))?;
        self.mem_write(d.ptr.at(8), 8, Scalar::Int(bits >> 64))
    }

    fn read_type_id_op(&mut self, op: Operand) -> R<'a, u128> {
        let o = self.operand(op)?;
        let res = self.read_type_id_opnd(o);
        self.release(o);
        res
    }

    fn read_type_id_opnd(&self, o: Opnd) -> R<'a, u128> {
        let p = match o.src {
            Src::At(p) => p,
            Src::Temp(id) => Pointer { alloc: id, offset: 0 },
            Src::Imm(b) => return Ok(b),
        };
        let bytes = self.mem_read_bytes(p, 16)?;
        let arr: [u8; 16] = bytes.try_into().map_err(|_| malformed("a `TypeId` not 16 bytes"))?;
        Ok(u128::from_le_bytes(arr))
    }

    fn lookup_type_id_ty(&self, bits: u128) -> R<'a, PTy> {
        match self.type_ids.iter().find(|&&(k, _)| k == bits).map(|&(_, v)| v) {
            Some(TypeIdTarget::Ty(ty)) => Ok(ty),
            Some(TypeIdTarget::Field { .. }) => Err(malformed("expected a type's `TypeId`, found a `FieldRepresentingType`").into()),
            None => Err(unsupported("a `TypeId` whose type was not produced in this constant evaluation").into()),
        }
    }

    fn lookup_type_id_field(&self, bits: u128) -> R<'a, (PTy, u32, u32)> {
        match self.type_ids.iter().find(|&&(k, _)| k == bits).map(|&(_, v)| v) {
            Some(TypeIdTarget::Field { base, variant, field }) => Ok((base, variant, field)),
            Some(TypeIdTarget::Ty(_)) => Err(malformed("expected a `FieldRepresentingType` `TypeId`, found a type").into()),
            None => Err(unsupported("a `FieldRepresentingType` `TypeId` not produced in this constant evaluation").into()),
        }
    }

    fn option_variants(&self, rt: PTy) -> R<'a, (u32, u32)> {
        let PNode::Adt(opt_key, _) = self.node(rt)? else {
            return Err(malformed("an `Option` reflection result that is not an ADT").into());
        };
        let adt = self
            .world
            .adt(opt_key)
            .ok_or(Stop::from(Refusal::Missing { what: "ADT definition", item: opt_key }))?;
        let none_v = adt
            .variants
            .iter()
            .position(|v| v.name == Name::of("None"))
            .or_else(|| adt.variants.iter().position(|v| v.fields.len == 0))
            .ok_or(Stop::from(malformed("`Option` without a `None` variant")))?;
        let some_v = adt
            .variants
            .iter()
            .position(|v| v.name == Name::of("Some"))
            .or_else(|| adt.variants.iter().position(|v| v.fields.len == 1))
            .ok_or(Stop::from(malformed("`Option` without a `Some` variant")))?;
        let none_v = u32::try_from(none_v).map_err(|_| malformed("variant index overflow"))?;
        let some_v = u32::try_from(some_v).map_err(|_| malformed("variant index overflow"))?;
        Ok((none_v, some_v))
    }

    fn write_option_u64(&mut self, d: Pl, val: Option<u64>) -> R<'a, ()> {
        let (none_v, some_v) = self.option_variants(d.ty)?;
        let v_idx = if let Some(v) = val {
            let ft = self.field_ty(d.ty, some_v, 0)?;
            let off = self.field_offset(d.ty, some_v, 0)?;
            self.put(Pl { ptr: d.ptr.at(off), meta: None, ty: ft, variant: 0 }, u128::from(v))?;
            some_v
        } else {
            none_v
        };
        self.set_tag(d.ptr, d.ty, v_idx)
    }

    fn write_option_type_id(&mut self, d: Pl, inner: Option<PTy>) -> R<'a, ()> {
        let (none_v, some_v) = self.option_variants(d.ty)?;
        let v_idx = if let Some(t) = inner {
            let ft = self.field_ty(d.ty, some_v, 0)?;
            let off = self.field_offset(d.ty, some_v, 0)?;
            let bits = self.record_type_id(t)?;
            self.write_type_id(bits, Pl { ptr: d.ptr.at(off), meta: None, ty: ft, variant: 0 })?;
            some_v
        } else {
            none_v
        };
        self.set_tag(d.ptr, d.ty, v_idx)
    }

    fn write_type_info(&mut self, ty: PTy, d: Pl) -> R<'a, ()> {
        let want = match self.node(ty)? {
            PNode::Tuple(_) => Name::of("Tuple"),
            PNode::Array(..) => Name::of("Array"),
            PNode::Slice(_) => Name::of("Slice"),
            PNode::Dynamic(_) => Name::of("DynTrait"),
            PNode::Adt(k, _) => {
                let adt = self
                    .world
                    .adt(k)
                    .ok_or(Stop::from(Refusal::Missing { what: "ADT definition", item: k }))?;
                match adt.kind {
                    AdtKind::Struct => Name::of("Struct"),
                    AdtKind::Enum => Name::of("Enum"),
                    AdtKind::Union => Name::of("Union"),
                }
            }
            PNode::Bool => Name::of("Bool"),
            PNode::Char => Name::of("Char"),
            PNode::Int(_) | PNode::Uint(_) => Name::of("Int"),
            PNode::Float(_) => Name::of("Float"),
            PNode::Str => Name::of("Str"),
            PNode::Ref(..) => Name::of("Reference"),
            PNode::Ptr(..) => Name::of("Pointer"),
            PNode::FnPtr(..) => Name::of("FnPtr"),
            _ => Name::of("Other"),
        };
        let kind_ty = self.field_ty(d.ty, 0, 0)?;
        let kind_off = self.field_offset(d.ty, 0, 0)?;
        let PNode::Adt(kind_key, _) = self.node(kind_ty)? else {
            return Err(malformed("`Type::kind` is not an ADT").into());
        };
        let kind_adt = self
            .world
            .adt(kind_key)
            .ok_or(Stop::from(Refusal::Missing { what: "ADT definition", item: kind_key }))?;
        let v_idx = kind_adt
            .variants
            .iter()
            .position(|v| v.name == want)
            .ok_or(Stop::from(malformed("`TypeKind` lacks the expected variant")))?;
        let v_idx = u32::try_from(v_idx).map_err(|_| malformed("`TypeKind` variant overflow"))?;
        let kp = d.ptr.at(kind_off);
        if want == Name::of("DynTrait") {
            let dyn_ty = self.field_ty(kind_ty, v_idx, 0)?;
            let dyn_off = self.field_offset(kind_ty, v_idx, 0)?;
            let slice_off = self.field_offset(dyn_ty, 0, 0)?;
            let empty_id = self.mem.allocate(0, 3, false, Origin::Data)?;
            self.mem_write(kp.at(dyn_off + slice_off), PTR, Scalar::Ptr(Pointer { alloc: empty_id, offset: 0 }))?;
            self.mem_write(kp.at(dyn_off + slice_off + PTR), PTR, Scalar::Int(0))?;
        }
        self.set_tag(kp, kind_ty, v_idx)
    }
}

/// Deterministic 128-bit identifier for a `FieldRepresentingType` `(base_type_id, variant, field)`
/// during constant evaluation (`type_id_field_representing_type`).
fn frt_hash(base: u128, variant: u32, field: u32) -> u128 {
    let mix = |mut z: u64| -> u64 {
        z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    };
    let [b0, b1, b2, b3, b4, b5, b6, b7, b8, b9, b10, b11, b12, b13, b14, b15] = base.to_le_bytes();
    let lo_in = u64::from_le_bytes([b0, b1, b2, b3, b4, b5, b6, b7]);
    let hi_in = u64::from_le_bytes([b8, b9, b10, b11, b12, b13, b14, b15]);
    let vf = (u64::from(variant) << 32) | u64::from(field);
    let lo = mix(lo_in ^ vf ^ 0x6672_745f_6c6f_3031);
    let hi = mix(hi_in ^ lo ^ vf.rotate_left(17) ^ 0x6672_745f_6869_3032);
    u128::from(lo) | (u128::from(hi) << 64)
}
