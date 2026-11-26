//! Operands, places and the `Value` variants of an assignment. Overflow is already an `Assert`
//! row of the body, so every operation here is the plain one.

use blaze_own::Vec;
use core::alloc::Allocator;

use blaze_body::{AggregateKind, BinOp, CastKind, Const, ConstKind, Local, Operand, Place, Proj, UnOp, Value as BodyValue};
use blaze_build::data::{DataRecord, DataTarget};
use blaze_build::intrinsics::{lir, read_discriminant, resize_in};
use blaze_build::layout::{Fields, Layout, LayoutCx, LayoutId, Variants};
use blaze_build::vtable::{self, HEADER_WORDS, VtableRef};
use blaze_build::{FnTrait, Instance, ground, reify_shim_instance};
use blaze_lir::builder::{DataReloc, MemAttrs};
use blaze_lir::func::{BlockKind, RefId, RefKind, SlotKind};
use blaze_lir::{Flags, FloatCond, IntCond, Op, Scalar, Ty, Value};
use blaze_typeck::{AdtKind, ExprId, List, PNode, PTy, UintTy};

use crate::{BuildError, Cx, Held, V};

/// Where a place is: a value local's values, or memory.
#[derive(Clone, Copy)]
pub(crate) enum At {
    Val(V),
    /// `off` bytes past `ptr`, of layout `lay`; `meta` the metadata of the wide pointer it was
    /// reached through, when its type is unsized.
    Mem { ptr: Value, off: i32, lay: LayoutId, meta: Option<Value> },
}

/// The offset of a scalar pair's second half.
pub(crate) fn second(l: &Layout) -> i32 {
    let Some((a, b)) = l.pair else { return 0 };
    let align = 1u64 << b.prim.align();
    (a.prim.size().div_ceil(align) * align) as i32
}

/// Above this element count, `[x; N]` emits a counted LIR loop instead of unrolled stores.
const REPEAT_UNROLL_CAP: u64 = 64;

impl<A: Allocator + Copy> Cx<'_, '_, '_, A> {
    /// `ty` of the body's table at this instance, in the ground table.
    /// Grounding also handles types interned into the body's own overlay (`BodyBuilder::intern_ty`)
    /// iteratively up to a fixed depth bound.
    pub(crate) fn ground(&mut self, ty: PTy) -> Result<PTy, BuildError> {
        const MAX_OVERLAY_DEPTH: u32 = 32;
        if !matches!(self.v.own_ty(ty), Ok(Some(_))) {
            return ground(self.from, ty, self.subst.args, self.subst.ground, self.subst.scratch, self.adts.facts()).map_err(|e| self.facts(e));
        }
        enum Frame<A: Allocator> {
            ConstValue(u128),
            Ref(blaze_typeck::Mutbl),
            Ptr(blaze_typeck::Mutbl),
            Slice,
            Array { len: PTy, elem: Option<PTy> },
            List { kind: ListKind, pending: Vec<PTy, A>, done: Vec<PTy, A> },
        }
        enum ListKind {
            Tuple,
            Record(blaze_typeck::RecordSchema),
            Union,
            Adt(blaze_typeck::ItemKey),
            FnDef(blaze_typeck::ItemKey),
            Closure(blaze_typeck::ItemKey, u32),
            Coroutine(blaze_typeck::ItemKey, u32),
        }
        let mut stack: Vec<(u32, Frame<A>), A> = Vec::new_in(self.alloc);
        let mut cur_ty = ty;
        let mut cur_depth: u32 = 0;
        loop {
            let mut resolved = if let Ok(Some(node)) = self.v.own_ty(cur_ty) {
                if cur_depth >= MAX_OVERLAY_DEPTH {
                    return Err(self.unsupported("a deeply nested type of the body's own overlay"));
                }
                match node {
                    PNode::Bool
                    | PNode::Char
                    | PNode::Str
                    | PNode::Never
                    | PNode::Int(_)
                    | PNode::Uint(_)
                    | PNode::Float(_) => self.subst.ground.intern(node).map_err(|_| self.malformed("the ground table is full"))?,
                    PNode::Param(i) | PNode::ConstParam(i) => {
                        let idx = usize::try_from(i).map_err(|_| self.malformed("a generic parameter past the instance's arguments"))?;
                        *self.subst.args.get(idx).ok_or(self.malformed("a generic parameter past the instance's arguments"))?
                    }
                    PNode::ConstValue { ty: cty, bits } => {
                        stack.push((cur_depth, Frame::ConstValue(bits)));
                        cur_ty = cty;
                        cur_depth += 1;
                        continue;
                    }
                    PNode::Ref(m, inner) => {
                        stack.push((cur_depth, Frame::Ref(m)));
                        cur_ty = inner;
                        cur_depth += 1;
                        continue;
                    }
                    PNode::Ptr(m, inner) => {
                        stack.push((cur_depth, Frame::Ptr(m)));
                        cur_ty = inner;
                        cur_depth += 1;
                        continue;
                    }
                    PNode::Slice(inner) => {
                        stack.push((cur_depth, Frame::Slice));
                        cur_ty = inner;
                        cur_depth += 1;
                        continue;
                    }
                    PNode::Array(elem, len) => {
                        stack.push((cur_depth, Frame::Array { len, elem: None }));
                        cur_ty = elem;
                        cur_depth += 1;
                        continue;
                    }
                    PNode::Tuple(list)
                    | PNode::Record(_, list)
                    | PNode::Union(list)
                    | PNode::Adt(_, list)
                    | PNode::FnDef(_, list)
                    | PNode::Closure { args: list, .. }
                    | PNode::Coroutine { args: list, .. } => {
                        let kind = match node {
                            PNode::Tuple(_) => ListKind::Tuple,
                            PNode::Record(s, _) => ListKind::Record(s),
                            PNode::Union(_) => ListKind::Union,
                            PNode::Adt(k, _) => ListKind::Adt(k),
                            PNode::FnDef(k, _) => ListKind::FnDef(k),
                            PNode::Closure { owner, ordinal, .. } => ListKind::Closure(owner, ordinal),
                            PNode::Coroutine { owner, ordinal, .. } => ListKind::Coroutine(owner, ordinal),
                            _ => return Err(self.unsupported("a type of the body's own overlay (not in its record's table)")),
                        };
                        let cap = usize::try_from(list.len).unwrap_or(0);
                        let mut pending: Vec<PTy, A> = Vec::with_capacity_in(cap, self.alloc);
                        let entries = self.v.ty_list(&self.from, list).map_err(|_| self.malformed("an overlay type list past its table"))?;
                        for t in entries.iter() {
                            pending.push(t);
                        }
                        pending.reverse();
                        if let Some(first) = pending.pop() {
                            let done = Vec::with_capacity_in(cap, self.alloc);
                            stack.push((cur_depth, Frame::List { kind, pending, done }));
                            cur_ty = first;
                            cur_depth += 1;
                            continue;
                        }
                        let empty = self.subst.ground.intern_list(&[]).map_err(|_| self.malformed("the ground table is full"))?;
                        let out = match kind {
                            ListKind::Tuple => PNode::Tuple(empty),
                            ListKind::Record(s) => PNode::Record(s, empty),
                            ListKind::Union => PNode::Union(empty),
                            ListKind::Adt(k) => PNode::Adt(k, empty),
                            ListKind::FnDef(k) => PNode::FnDef(k, empty),
                            ListKind::Closure(owner, ordinal) => PNode::Closure { owner, ordinal, args: empty },
                            ListKind::Coroutine(owner, ordinal) => PNode::Coroutine { owner, ordinal, args: empty },
                        };
                        self.subst.ground.intern(out).map_err(|_| self.malformed("the ground table is full"))?
                    }
                    _ => return Err(self.unsupported("a type of the body's own overlay (not in its record's table)")),
                }
            } else {
                ground(self.from, cur_ty, self.subst.args, self.subst.ground, self.subst.scratch, self.adts.facts()).map_err(|e| self.facts(e))?
            };

            loop {
                let Some((frame_depth, frame)) = stack.pop() else {
                    return Ok(resolved);
                };
                match frame {
                    Frame::ConstValue(bits) => {
                        resolved = self.subst.ground.intern(PNode::ConstValue { ty: resolved, bits }).map_err(|_| self.malformed("the ground table is full"))?;
                    }
                    Frame::Ref(m) => {
                        resolved = self.subst.ground.intern(PNode::Ref(m, resolved)).map_err(|_| self.malformed("the ground table is full"))?;
                    }
                    Frame::Ptr(m) => {
                        resolved = self.subst.ground.intern(PNode::Ptr(m, resolved)).map_err(|_| self.malformed("the ground table is full"))?;
                    }
                    Frame::Slice => {
                        resolved = self.subst.ground.intern(PNode::Slice(resolved)).map_err(|_| self.malformed("the ground table is full"))?;
                    }
                    Frame::Array { len, elem: None } => {
                        stack.push((frame_depth, Frame::Array { len, elem: Some(resolved) }));
                        cur_ty = len;
                        cur_depth = frame_depth + 1;
                        break;
                    }
                    Frame::Array { elem: Some(elem), .. } => {
                        resolved = self.subst.ground.intern(PNode::Array(elem, resolved)).map_err(|_| self.malformed("the ground table is full"))?;
                    }
                    Frame::List { kind, mut pending, mut done } => {
                        done.push(resolved);
                        if let Some(next) = pending.pop() {
                            stack.push((frame_depth, Frame::List { kind, pending, done }));
                            cur_ty = next;
                            cur_depth = frame_depth + 1;
                            break;
                        }
                        let interned = self.subst.ground.intern_list(&done).map_err(|_| self.malformed("the ground table is full"))?;
                        let out = match kind {
                            ListKind::Tuple => PNode::Tuple(interned),
                            ListKind::Record(s) => PNode::Record(s, interned),
                            ListKind::Union => PNode::Union(interned),
                            ListKind::Adt(k) => PNode::Adt(k, interned),
                            ListKind::FnDef(k) => PNode::FnDef(k, interned),
                            ListKind::Closure(owner, ordinal) => PNode::Closure { owner, ordinal, args: interned },
                            ListKind::Coroutine(owner, ordinal) => PNode::Coroutine { owner, ordinal, args: interned },
                        };
                        resolved = self.subst.ground.intern(out).map_err(|_| self.malformed("the ground table is full"))?;
                    }
                }
            }
        }
    }

    /// The layout of ground type `g`.
    pub(crate) fn layout(&mut self, g: PTy) -> Result<LayoutId, BuildError> {
        let mut cx = LayoutCx { ground: self.subst.ground, facts: self.adts.facts(), scratch: self.subst.scratch };
        self.layouts.of(g, &mut cx).map_err(|error| BuildError::Layout { stmt: self.stmt, error })
    }

    /// The head node of ground type `g`.
    pub(crate) fn node(&self, g: PTy) -> Result<PNode, BuildError> {
        self.subst.ground.view().node(g).map_err(|_| self.malformed("a ground type past its table"))
    }

    /// Whether integer type `g` is signed (a `bool`, a `char` and a pointer are not).
    fn signed(&self, g: PTy) -> Result<bool, BuildError> {
        Ok(matches!(self.node(g)?, PNode::Int(_)))
    }

    /// Whether ground type `g` is a floating-point scalar.
    fn is_float(&self, g: PTy) -> Result<bool, BuildError> {
        Ok(matches!(self.node(g)?, PNode::Float(_)))
    }

    /// The bits of a ground `ConstValue` (an array's length).
    fn const_bits(&self, g: PTy) -> Result<u128, BuildError> {
        match self.node(g)? {
            PNode::ConstValue { bits, .. } => Ok(bits),
            _ => Err(self.unsupported("an array length that is not a ground value")),
        }
    }

    /// The offset of field `idx` of layout `lay`, an array's element included.
    pub(crate) fn field_off(&self, lay: LayoutId, idx: u32) -> Result<i32, BuildError> {
        let off = match self.layouts.get(lay).fields {
            Fields::Array { stride, .. } => stride.checked_mul(u64::from(idx)),
            _ => self.layouts.field_offset(lay, idx),
        };
        off.and_then(|o| i32::try_from(o).ok()).ok_or(self.malformed("a field past its layout"))
    }

    /// The value of layout `lay` at `ptr + off`: its one or two scalars loaded, an aggregate by
    /// address.
    pub(crate) fn load_at(&mut self, ptr: Value, off: i32, lay: LayoutId) -> Result<V, BuildError> {
        let l = *self.layouts.get(lay);
        if l.is_zst() {
            return Ok(V::None);
        }
        if let Some(s) = l.scalar {
            let ty = lir(s.prim).0;
            return Ok(V::One(self.b.load(ty, Flags::NONE, ptr, off, MemAttrs::aligned(s.prim.align()))));
        }
        if let Some((a, b)) = l.pair {
            let x = self.b.load(lir(a.prim).0, Flags::NONE, ptr, off, MemAttrs::aligned(a.prim.align()));
            let y = self.b.load(lir(b.prim).0, Flags::NONE, ptr, off + second(&l), MemAttrs::aligned(b.prim.align()));
            return Ok(V::Two(x, y));
        }
        Ok(V::Mem { ptr, off, lay })
    }

    /// Stores `x`, a value of layout `lay`, at `ptr + off`.
    pub(crate) fn store_v(&mut self, ptr: Value, off: i32, lay: LayoutId, x: V) -> Result<(), BuildError> {
        let l = *self.layouts.get(lay);
        match x {
            V::None => {}
            V::One(v) => self.b.store(Flags::NONE, v, ptr, off, MemAttrs::aligned(l.align)),
            V::Two(a, b) => {
                let (sa, sb) = l.pair.ok_or(self.malformed("two values stored as a type that is not a pair"))?;
                self.b.store(Flags::NONE, a, ptr, off, MemAttrs::aligned(sa.prim.align()));
                self.b.store(Flags::NONE, b, ptr, off + second(&l), MemAttrs::aligned(sb.prim.align()));
            }
            V::Mem { ptr: from, off: from_off, .. } => {
                if from == ptr && from_off == off {
                    return Ok(());
                }
                let dst = self.addr(ptr, off);
                let src = self.addr(from, from_off);
                let len = self.b.iconst(Ty::I64, u128::from(l.size));
                self.b.bulk(Op::Memcpy, Flags::NONE, dst, src, len, MemAttrs::aligned(l.align));
            }
        }
        Ok(())
    }

    /// `x` in memory: where it is, or a fresh temporary slot it is stored to.
    pub(crate) fn in_memory(&mut self, x: V, lay: LayoutId) -> Result<(Value, i32), BuildError> {
        if let V::Mem { ptr, off, .. } = x {
            return Ok((ptr, off));
        }
        let slot = self.new_slot(lay, SlotKind::Temp)?;
        let ptr = self.b.slot_addr(slot);
        self.store_v(ptr, 0, lay, x)?;
        Ok((ptr, 0))
    }

    /// Where place `p` is, and its ground type: its local, then each projection in turn.
    pub(crate) fn resolve(&mut self, p: Place) -> Result<(At, PTy), BuildError> {
        let depth = self.v.place(p).depth as usize;
        let mut chain: Vec<Proj, A> = Vec::with_capacity_in(depth, self.alloc);
        let mut q = p;
        while chain.len() < depth {
            let row = self.v.place(q);
            chain.push(row.proj.ok_or(self.malformed("a projected place without its projection"))?);
            q = row.parent.ok_or(self.malformed("a projected place without its parent"))?;
        }
        let local = self.v.place(p).local;
        let i = self.ix(local)?;
        let mut g = *self.tys.get(i).ok_or(self.malformed("a local past the frame"))?;
        let mut lay = *self.lays.get(i).ok_or(self.malformed("a local past the frame"))?;
        let held = *self.held.get(i).ok_or(self.malformed("a local past the frame"))?;
        let [c0, c1] = *self.cur.get(i).ok_or(self.malformed("a local past the frame"))?;
        let mut at = match held {
            Held::None => At::Val(V::None),
            Held::One(_) => At::Val(V::One(c0.ok_or(self.malformed("a read of a value local before it is written"))?)),
            Held::Two(..) => {
                let a = c0.ok_or(self.malformed("a read of a value local before it is written"))?;
                let b = c1.ok_or(self.malformed("a read of a value local before it is written"))?;
                At::Val(V::Two(a, b))
            }
            Held::Slot(_) | Held::At(_) | Held::AtOff(..) => {
                let (ptr, off) = self.base_at(local)?;
                At::Mem { ptr, off, lay, meta: None }
            }
        };
        for &proj in chain.iter().rev() {
            match proj {
                Proj::Field { index, ty } => {
                    let fg = self.ground(ty)?;
                    let flay = self.layout(fg)?;
                    let foff = self.field_off(lay, index)?;
                    at = match at {
                        At::Mem { ptr, off, meta, .. } => At::Mem { ptr, off: off + foff, lay: flay, meta },
                        At::Val(x) => At::Val(self.field_of_value(x, lay, foff, flay)?),
                    };
                    (g, lay) = (fg, flay);
                }
                Proj::Downcast(variant) => {
                    let At::Mem { ptr, off, meta, .. } = at else {
                        return Err(self.malformed("a downcast of a value local (an enum with a tag is held in memory)"));
                    };
                    let vlay = match self.layouts.get(lay).variants {
                        Variants::Multiple { .. } => self.layouts.variant(lay, variant).ok_or(self.malformed("a variant past its enum"))?,
                        Variants::Single { .. } => lay,
                    };
                    at = At::Mem { ptr, off, lay: vlay, meta };
                    lay = vlay;
                }
                Proj::Deref => {
                    let pointee = match self.node(g)? {
                        PNode::Ref(_, t) | PNode::Ptr(_, t) => t,
                        _ => return Err(self.unsupported("a dereference of a type that is not a reference or raw pointer (a `Box`)")),
                    };
                    let x = match at {
                        At::Val(x) => x,
                        At::Mem { ptr, off, lay: plain, .. } => self.load_at(ptr, off, plain)?,
                    };
                    let plain = self.layout(pointee)?;
                    at = match x {
                        V::One(p) => At::Mem { ptr: p, off: 0, lay: plain, meta: None },
                        V::Two(p, m) => At::Mem { ptr: p, off: 0, lay: plain, meta: Some(m) },
                        V::None | V::Mem { .. } => return Err(self.malformed("a dereference of a value that is not a pointer")),
                    };
                    (g, lay) = (pointee, plain);
                }
                Proj::Index(l) => {
                    let At::Mem { ptr, off, meta, .. } = at else {
                        return Err(self.malformed("an index into a value local"));
                    };
                    let (elem_g, elem, stride) = self.element(g, lay)?;
                    let (idx, _) = self.read_local(l)?;
                    let idx = resize_in(&mut self.b, idx, Ty::I64, false);
                    let by = self.b.iconst(Ty::I64, u128::from(stride));
                    let bytes = self.b.binary(Op::Mul, Flags::NONE, idx, by);
                    let base = self.addr(ptr, off);
                    let at_elem = self.b.ptr_add(Flags::NONE, base, bytes);
                    at = At::Mem { ptr: at_elem, off: 0, lay: elem, meta };
                    (g, lay) = (elem_g, elem);
                }
                Proj::ConstIndex { offset, from_end, .. } => {
                    let At::Mem { ptr, off, meta, .. } = at else {
                        return Err(self.malformed("an index into a value local"));
                    };
                    let (elem_g, elem, stride) = self.element(g, lay)?;
                    if from_end && matches!(self.node(g)?, PNode::Slice(_)) {
                        let len = meta.ok_or(self.malformed("a slice index from the end without its length metadata"))?;
                        let sub = self.b.iconst(Ty::I64, u128::from(offset));
                        let idx = self.b.binary(Op::Sub, Flags::NONE, len, sub);
                        let by = self.b.iconst(Ty::I64, u128::from(stride));
                        let bytes = self.b.binary(Op::Mul, Flags::NONE, idx, by);
                        let base = self.addr(ptr, off);
                        let at_elem = self.b.ptr_add(Flags::NONE, base, bytes);
                        at = At::Mem { ptr: at_elem, off: 0, lay: elem, meta: None };
                    } else {
                        let k = match (from_end, self.layouts.get(lay).fields) {
                            (false, _) => u64::from(offset),
                            (true, Fields::Array { count, .. }) => {
                                count.checked_sub(u64::from(offset)).ok_or(self.malformed("an index from the end past an array"))?
                            }
                            (true, _) => return Err(self.malformed("a slice index from the end without its length metadata")),
                        };
                        let by = k.checked_mul(stride).and_then(|b| i32::try_from(b).ok()).ok_or(self.malformed("an element past 2 GiB"))?;
                        at = At::Mem { ptr, off: off + by, lay: elem, meta: None };
                    }
                    (g, lay) = (elem_g, elem);
                }
                Proj::DerefBox => {
                    let pointee = match self.node(g)? {
                        PNode::Adt(_, args) => {
                            let list = self.subst.ground.view().list(args).map_err(|_| self.malformed("a Box's arguments past the ground table"))?;
                            *list.first().ok_or(self.malformed("a Box without its element type argument"))?
                        }
                        _ => return Err(self.malformed("a `DerefBox` of a type that is not an ADT")),
                    };
                    let plain = self.layout(pointee)?;
                    let ptr_ty = self.subst.ground.intern(PNode::Ptr(blaze_typeck::Mutbl::Mut, pointee))
                        .map_err(|_| self.malformed("the ground table is full"))?;
                    let ptr_lay = self.layout(ptr_ty)?;
                    let x = match at {
                        At::Val(x) => x,
                        At::Mem { ptr, off, .. } => self.load_at(ptr, off, ptr_lay)?,
                    };
                    at = match x {
                        V::One(p) => At::Mem { ptr: p, off: 0, lay: plain, meta: None },
                        V::Two(p, m) => At::Mem { ptr: p, off: 0, lay: plain, meta: Some(m) },
                        V::None | V::Mem { .. } => return Err(self.malformed("a `Box` dereference of a value that is not a pointer")),
                    };
                    (g, lay) = (pointee, plain);
                }
                Proj::Subslice { from, to, from_end } => {
                    let At::Mem { ptr, off, meta, .. } = at else {
                        return Err(self.malformed("a subslice of a value local"));
                    };
                    let (elem_g, _, stride) = self.element(g, lay)?;
                    let start_by = u64::from(from)
                        .checked_mul(stride)
                        .and_then(|b| i32::try_from(b).ok())
                        .ok_or(self.malformed("a subslice start past 2 GiB"))?;
                    match self.node(g)? {
                        PNode::Array(_, len_ty) => {
                            let n = u64::try_from(self.const_bits(len_ty)?).map_err(|_| self.malformed("an array length past u64"))?;
                            let sub_len = if from_end {
                                n.checked_sub(u64::from(from) + u64::from(to))
                            } else {
                                u64::from(to).checked_sub(u64::from(from))
                            }
                            .ok_or(self.malformed("a subslice range past its array"))?;
                            let usize_ty = self.subst.ground.intern(PNode::Uint(UintTy::Usize))
                                .map_err(|_| self.malformed("the ground table is full"))?;
                            let len_val = self.subst.ground.intern(PNode::ConstValue { ty: usize_ty, bits: u128::from(sub_len) })
                                .map_err(|_| self.malformed("the ground table is full"))?;
                            let sub_g = self.subst.ground.intern(PNode::Array(elem_g, len_val))
                                .map_err(|_| self.malformed("the ground table is full"))?;
                            let sub_lay = self.layout(sub_g)?;
                            at = At::Mem { ptr, off: off + start_by, lay: sub_lay, meta: None };
                            (g, lay) = (sub_g, sub_lay);
                        }
                        PNode::Slice(_) => {
                            let len = meta.ok_or(self.malformed("a subslice of a slice without its length metadata"))?;
                            let new_len = if from_end {
                                let trim = self.b.iconst(Ty::I64, u128::from(u64::from(from) + u64::from(to)));
                                self.b.binary(Op::Sub, Flags::NONE, len, trim)
                            } else {
                                let sub_len = u64::from(to).checked_sub(u64::from(from))
                                    .ok_or(self.malformed("a subslice with `to < from`"))?;
                                self.b.iconst(Ty::I64, u128::from(sub_len))
                            };
                            at = At::Mem { ptr, off: off + start_by, lay, meta: Some(new_len) };
                        }
                        _ => return Err(self.malformed("a subslice of a type that is not an array or slice")),
                    }
                }
            }
        }
        Ok((at, g))
    }

    /// The element type, layout and stride of array, slice, or `str` `g` of layout `lay`.
    fn element(&mut self, g: PTy, lay: LayoutId) -> Result<(PTy, LayoutId, u64), BuildError> {
        let elem_g = match self.node(g)? {
            PNode::Array(t, _) | PNode::Slice(t) => t,
            PNode::Str => self.subst.ground.intern(PNode::Uint(UintTy::U8))
                .map_err(|_| self.malformed("the ground table is full"))?,
            _ => return Err(self.malformed("an index into a type that is not an array or slice")),
        };
        let elem = self.layout(elem_g)?;
        let stride = match self.layouts.get(lay).fields {
            Fields::Array { stride, .. } => stride,
            _ => self.layouts.get(elem).size,
        };
        Ok((elem_g, elem, stride))
    }

    /// The field of layout `flay` at `foff` of value `x` of layout `lay`, held as values.
    fn field_of_value(&mut self, x: V, lay: LayoutId, foff: i32, flay: LayoutId) -> Result<V, BuildError> {
        let f = *self.layouts.get(flay);
        if f.is_zst() {
            return Ok(V::None);
        }
        let sec = second(self.layouts.get(lay));
        match (x, f.scalar.is_some(), f.pair.is_some()) {
            (V::One(v), true, _) if foff == 0 => Ok(V::One(v)),
            (V::Two(a, _), true, _) if foff == 0 => Ok(V::One(a)),
            (V::Two(_, b), true, _) if foff == sec => Ok(V::One(b)),
            (V::Two(a, b), _, true) if foff == 0 => Ok(V::Two(a, b)),
            _ => Err(self.unsupported("a field of a value local that is not one of its scalars")),
        }
    }

    /// Local `l`'s one scalar value and its ground type.
    fn read_local(&mut self, l: Local) -> Result<(Value, PTy), BuildError> {
        let place = self.v.local_place(l);
        let (at, g) = self.resolve(place)?;
        let x = self.read_at(at)?;
        Ok((self.scalar(x)?, g))
    }

    fn read_at(&mut self, at: At) -> Result<V, BuildError> {
        match at {
            At::Val(x) => Ok(x),
            At::Mem { ptr, off, lay, .. } => self.load_at(ptr, off, lay),
        }
    }

    /// Operand `o` and its ground type. A move reads as a copy does: the body's drop
    /// elaboration already says what is dropped.
    pub(crate) fn operand(&mut self, o: Operand) -> Result<(V, PTy), BuildError> {
        match o {
            Operand::Copy(p) | Operand::Move(p) => {
                let (at, g) = self.resolve(p)?;
                Ok((self.read_at(at)?, g))
            }
            Operand::Const(c) => self.constant(c),
        }
    }

    /// A scalar LIR constant of `ty` whose bits are `value` (little-endian): integers, floats,
    /// and exposed-provenance pointers (`0` -> `null`, non-zero -> `from_exposed`).
    pub(crate) fn const_scalar(&mut self, ty: Ty, value: u128) -> Result<Value, BuildError> {
        if !ty.is_scalar() {
            return Err(self.malformed("a scalar constant of a type with no constant form"));
        }
        match ty.elem() {
            Scalar::I1 | Scalar::I8 | Scalar::I16 | Scalar::I32 | Scalar::I64 | Scalar::I128 => {
                Ok(self.b.iconst(ty, value))
            }
            Scalar::Ptr => {
                if value == 0 {
                    return Ok(self.b.null());
                }
                if value > u128::from(u64::MAX) {
                    return Err(self.malformed("a pointer constant's address past 64 bits"));
                }
                let address = self.b.iconst(Ty::I64, value);
                Ok(self.b.convert(Op::FromExposed, Flags::NONE, Ty::PTR, address))
            }
            Scalar::F16 | Scalar::F32 | Scalar::F64 | Scalar::F128 => Ok(self.b.fconst(ty, value)),
            Scalar::None => Err(self.malformed("a scalar constant of a type with no constant form")),
        }
    }

    /// Materializes every allocation of `record` as a LIR data row (targets before referrers),
    /// resolving Local, Static, Fn, and Vtable relocations, and returns the root's address.
    fn data_rows<DA: Allocator>(&mut self, record: &DataRecord<DA>) -> Result<Value, BuildError> {
        const TODO: u8 = 0;
        const OPEN: u8 = 1;
        let stmt = self.row()?;
        let n = record.allocs.len();
        let mut state: Vec<u8, A> = Vec::with_capacity_in(n, self.alloc);
        state.resize(n, TODO);
        let mut rows: Vec<Option<RefId>, A> = Vec::with_capacity_in(n, self.alloc);
        rows.resize(n, None);
        let mut stack: Vec<usize, A> = Vec::new_in(self.alloc);
        stack.push(0usize);
        let mut relocs: Vec<DataReloc, A> = Vec::new_in(self.alloc);
        while let Some(&k) = stack.last() {
            if rows.get(k).copied().flatten().is_some() {
                stack.pop();
                continue;
            }
            let a = record.allocs.get(k).ok_or(self.malformed("a const allocation index past its record"))?;
            if state.get(k).copied() == Some(TODO) {
                if let Some(slot) = state.get_mut(k) {
                    *slot = OPEN;
                }
                for r in a.relocs.iter() {
                    if let DataTarget::Local(t) = r.target {
                        let t = usize::try_from(t).map_err(|_| self.malformed("a const allocation index past usize"))?;
                        match (rows.get(t), state.get(t)) {
                            (Some(Some(_)), _) => {}
                            (Some(None), Some(&OPEN)) => {
                                return Err(self.unsupported("a const whose allocations point at each other in a cycle"));
                            }
                            (Some(None), _) => stack.push(t),
                            (None, _) => return Err(self.malformed("a const's relocation to an allocation past its record")),
                        }
                    }
                }
                continue;
            }
            relocs.clear();
            for r in a.relocs.iter() {
                let off = usize::try_from(r.offset).map_err(|_| self.malformed("a const relocation offset past usize"))?;
                let word = a.bytes.get(off..off + 8).ok_or(self.malformed("a const's relocation past its bytes"))?;
                let addend = i64::from_le_bytes(word.try_into().map_err(|_| self.malformed("a relocation word not of 8 bytes"))?);
                let target = match r.target {
                    DataTarget::Local(t) => {
                        let ti = usize::try_from(t).map_err(|_| self.malformed("a const allocation index past usize"))?;
                        rows.get(ti).copied().flatten().ok_or(self.malformed("a const's row made before its target"))?
                    }
                    DataTarget::Static(s) => self.static_ref(s),
                    DataTarget::Fn(key) => {
                        let instance = self.adts.const_fn_instance(key, self.subst.ground).ok_or(BuildError::NoConstFnInstance { stmt, key })?;
                        let target = self.reify_target(instance)?;
                        self.func_ref(target)
                    }
                    DataTarget::Vtable(key) => {
                        let vt = self.adts.const_vtable_ref(key, self.subst.ground).ok_or(BuildError::NoConstVtable { stmt, key })?;
                        self.vtable_ref(vt)
                    }
                };
                relocs.push(DataReloc { offset: r.offset, target, addend });
            }
            if a.mutable {
                return Err(self.unsupported("a const with a mutable allocation"));
            }
            let mut bytes: Vec<u8, A> = Vec::with_capacity_in(a.bytes.len(), self.alloc);
            bytes.extend_from_slice(&a.bytes);
            for r in relocs.iter() {
                let start = usize::try_from(r.offset).map_err(|_| self.malformed("a const relocation offset past usize"))?;
                if let Some(slice) = bytes.get_mut(start..start + 8) {
                    slice.fill(0);
                }
            }
            let row = self.b.data(&bytes, u32::from(a.align), &relocs);
            if let Some(slot) = rows.get_mut(k) {
                *slot = Some(row);
            }
            stack.pop();
        }
        let root = rows.first().copied().flatten().ok_or(self.malformed("a const's root row not made"))?;
        Ok(self.b.data_addr(root))
    }

    /// Reads an evaluated `DataRecord` as a value of layout `lay`.
    fn const_record_value<DA: Allocator>(&mut self, lay: LayoutId, record: &DataRecord<DA>) -> Result<V, BuildError> {
        record.check().map_err(|_| self.malformed("a const's record that breaks the data form"))?;
        let l = *self.layouts.get(lay);
        if l.is_zst() {
            return Ok(V::None);
        }
        let root = record.allocs.first().ok_or(self.malformed("a const's record with no allocation"))?;
        if let Some(s) = l.scalar
            && root.relocs.is_empty()
        {
            let ty = lir(s.prim).0;
            let width = usize::try_from(ty.bits(crate::PTR_BITS) / 8).map_err(|_| self.malformed("a scalar width past usize"))?;
            let bytes = root.bytes.get(..width).ok_or(self.malformed("a const's bytes shorter than its type"))?;
            let mut bits = 0u128;
            for (i, &byte) in bytes.iter().enumerate() {
                bits |= u128::from(byte) << (8 * i);
            }
            return Ok(V::One(self.const_scalar(ty, bits)?));
        }
        let at = self.data_rows(record)?;
        self.load_at(at, 0, lay)
    }

    /// The scalar bits of a const generic parameter `idx` from `subst.args`.
    fn const_param_bits(&self, idx: u32) -> Result<u128, BuildError> {
        let i = usize::try_from(idx).map_err(|_| self.malformed("a const parameter index past usize"))?;
        let g = *self.subst.args.get(i).ok_or(self.malformed("a const parameter past the instance's arguments"))?;
        match self.node(g)? {
            PNode::ConstValue { bits, .. } => Ok(bits),
            _ => Err(self.malformed("a const parameter's argument that is not a value in the ground table")),
        }
    }

    /// Constant `c` and its ground type.
    fn constant(&mut self, c: Const) -> Result<(V, PTy), BuildError> {
        let decl = self.v.konst(c);
        let g = self.ground(decl.ty)?;
        let lay = self.layout(g)?;
        let l = *self.layouts.get(lay);
        let x = match decl.kind {
            ConstKind::Scalar { bits } => {
                if l.is_zst() {
                    V::None
                } else if let Some(s) = l.scalar {
                    let ty = lir(s.prim).0;
                    V::One(self.const_scalar(ty, bits)?)
                } else {
                    return Err(self.unsupported("a scalar constant of a type that is not one scalar"));
                }
            }
            ConstKind::FnItem { .. } if l.is_zst() => V::None,
            ConstKind::Bytes { .. } => {
                let bytes = self.v.bytes(c).ok_or(self.malformed("a bytes constant past the body's bytes"))?;
                let len = bytes.len() as u128;
                let data = self.b.data(bytes, 0, &[]);
                let ptr = self.b.data_addr(data);
                match (l.scalar.is_some(), l.pair.is_some()) {
                    (true, _) => V::One(ptr),
                    (_, true) => V::Two(ptr, self.b.iconst(Ty::I64, len)),
                    _ => return Err(self.malformed("a bytes constant of a type that is not a pointer")),
                }
            }
            ConstKind::FnItem { .. } => return Err(self.malformed("a fn item constant of a sized type")),
            ConstKind::Item { key, args } => {
                let stmt = self.row()?;
                let from = self.from;
                let v = self.v;
                let tys = v.ty_list(&from, args).map_err(|_| self.malformed("a constant's arguments past the body's tables"))?;
                let mut ground_args: Vec<PTy, A> = Vec::with_capacity_in(tys.len(), self.alloc);
                for t in tys.iter() {
                    ground_args.push(self.ground(t)?);
                }
                let record = self.adts.const_value(key, &ground_args).ok_or(BuildError::NoConstValue { stmt, item: key })?;
                self.const_record_value(lay, record)?
            }
            ConstKind::Promoted(id) => {
                let stmt = self.row()?;
                let expr = ExprId(id);
                let record = self.adts.promoted_value(self.item, expr, self.subst.args)
                    .ok_or(BuildError::NoPromotedValue { stmt, expr })?;
                record.check().map_err(|_| self.malformed("a promoted borrow's record that breaks the data form"))?;
                let at = self.data_rows(record)?;
                V::One(at)
            }
            ConstKind::StaticAddr(item) => {
                let r = self.static_ref(item);
                V::One(self.b.data_addr(r))
            }
            ConstKind::Anon(expr) => {
                let stmt = self.row()?;
                let record = self.adts.const_block_value(self.item, expr, self.subst.args).ok_or(BuildError::NoConstBlock { stmt, expr })?;
                self.const_record_value(lay, record)?
            }
            ConstKind::Param(idx) => {
                let bits = self.const_param_bits(idx)?;
                if l.is_zst() {
                    V::None
                } else if let Some(s) = l.scalar {
                    let ty = lir(s.prim).0;
                    let width = ty.bits(crate::PTR_BITS);
                    let masked = if width >= 128 { bits } else { bits & ((1u128 << width) - 1) };
                    V::One(self.const_scalar(ty, masked)?)
                } else {
                    return Err(self.unsupported("a const generic parameter of a non-scalar type"));
                }
            }
        };
        Ok((x, g))
    }

    /// Writes `x`, of layout `lay`, to place `dest`.
    pub(crate) fn write(&mut self, dest: Place, x: V, lay: LayoutId) -> Result<(), BuildError> {
        let row = self.v.place(dest);
        if row.depth == 0 {
            return self.put(row.local, x, lay);
        }
        match self.resolve(dest)?.0 {
            At::Mem { ptr, off, lay, .. } => self.store_v(ptr, off, lay, x),
            At::Val(_) => Err(self.malformed("a projection written through a value local")),
        }
    }

    /// The layout of place `dest`'s type.
    pub(crate) fn dest_layout(&mut self, dest: Place) -> Result<LayoutId, BuildError> {
        let row = self.v.place(dest);
        if row.depth == 0 {
            let i = self.ix(row.local)?;
            return self.lays.get(i).copied().ok_or(self.malformed("a local past the frame"));
        }
        match self.resolve(dest)?.0 {
            At::Mem { lay, .. } => Ok(lay),
            At::Val(_) => Err(self.malformed("a projection written through a value local")),
        }
    }

    /// Writes the fields `fields` (index, value, the value's layout) of variant `variant` of
    /// `dest`, whose layout is `lay`, and its tag.
    pub(crate) fn write_fields(&mut self, dest: Place, lay: LayoutId, variant: Option<u32>, fields: &[(u32, V, LayoutId)]) -> Result<(), BuildError> {
        if self.layouts.get(lay).is_zst() {
            return Ok(());
        }
        let row = self.v.place(dest);
        let value_local = row.depth == 0 && self.held.get(self.ix(row.local)?).is_some_and(|h| h.is_value());
        if value_local {
            if variant.is_some_and(|v| v != 0) {
                return Err(self.malformed("an enum variant built into a value local"));
            }
            // The fields, at their offsets, make the local's one or two values.
            let mut parts: Vec<(i32, Value), A> = Vec::with_capacity_in(2, self.alloc);
            for &(idx, x, flay) in fields {
                let foff = self.field_off(lay, idx)?;
                let x = match x {
                    V::Mem { ptr, off, lay } => self.load_at(ptr, off, lay)?,
                    x => x,
                };
                match x {
                    V::None => {}
                    V::One(v) => parts.push((foff, v)),
                    V::Two(a, b) => {
                        parts.push((foff, a));
                        parts.push((foff + second(self.layouts.get(flay)), b));
                    }
                    V::Mem { .. } => return Err(self.malformed("an aggregate field of a value local")),
                }
            }
            parts.sort_by_key(|&(off, _)| off);
            let sec = second(self.layouts.get(lay));
            let x = match parts.as_slice() {
                [] => V::None,
                [(0, v)] => V::One(*v),
                [(0, a), (o, b)] if *o == sec => V::Two(*a, *b),
                _ => return Err(self.malformed("an aggregate whose fields do not make its local's values")),
            };
            return self.put(row.local, x, lay);
        }
        let (ptr, off) = match self.v.place(dest).depth {
            0 => (self.base(row.local)?, 0),
            _ => match self.resolve(dest)?.0 {
                At::Mem { ptr, off, .. } => (ptr, off),
                At::Val(_) => return Err(self.malformed("a projection written through a value local")),
            },
        };
        let vlay = match (variant, self.layouts.get(lay).variants) {
            (Some(v), Variants::Multiple { .. }) => self.layouts.variant(lay, v).ok_or(self.malformed("a variant past its enum"))?,
            _ => lay,
        };
        for &(idx, x, flay) in fields {
            let foff = self.field_off(vlay, idx)?;
            self.store_v(ptr, off + foff, flay, x)?;
        }
        match variant {
            Some(v) => self.store_tag(ptr, off, lay, v),
            None => Ok(()),
        }
    }

    /// Writes the tag that makes the value of layout `lay` at `ptr + off` variant `v`, by the
    /// layout's one encoding (`Layouts::tag_write`); nothing when the layout has no tag or `v`
    /// is the niche's untagged variant.
    pub(crate) fn store_tag(&mut self, ptr: Value, off: i32, lay: LayoutId, v: u32) -> Result<(), BuildError> {
        let Some(tag) = self.layouts.tag_write(lay, v) else {
            return Ok(());
        };
        let at = i32::try_from(tag.offset).map_err(|_| self.malformed("a tag past 2 GiB"))?;
        let ty = match lir(tag.scalar.prim).0 {
            Ty::PTR => Ty::I64,
            ty => ty,
        };
        let bits = self.b.iconst(ty, tag.value);
        self.b.store(Flags::NONE, bits, ptr, off + at, MemAttrs::aligned(tag.scalar.prim.align()));
        Ok(())
    }

    /// `SetDiscriminant(place, variant)`: the tag of the enum at `place` written to say
    /// `variant`, its fields untouched. The frame holds the local in memory (frame.rs
    /// `address_taken`), so the place is memory.
    pub(crate) fn set_discriminant(&mut self, place: Place, variant: u32) -> Result<(), BuildError> {
        match self.resolve(place)?.0 {
            At::Mem { ptr, off, lay, .. } => self.store_tag(ptr, off, lay, variant),
            At::Val(_) => Err(self.malformed("a discriminant set on a value local (the frame holds it in memory)")),
        }
    }

    /// `dest = value`.
    pub(crate) fn assign(&mut self, dest: Place, value: BodyValue) -> Result<(), BuildError> {
        let dlay = self.dest_layout(dest)?;
        let x = match value {
            BodyValue::Use(o) => self.operand(o)?.0,
            BodyValue::Borrow { place, .. } | BodyValue::RawAddr { place, .. } => match self.resolve(place)?.0 {
                At::Mem { ptr, off, meta, .. } => {
                    let p = self.addr(ptr, off);
                    match meta {
                        Some(m) if self.layouts.get(dlay).pair.is_some() => V::Two(p, m),
                        _ => V::One(p),
                    }
                }
                At::Val(_) => return Err(self.malformed("a borrow of a value local (the frame holds it in memory)")),
            },
            BodyValue::Binary { op, lhs, rhs } => V::One(self.binary(op, lhs, rhs)?),
            BodyValue::CheckedBinary { op, lhs, rhs } => {
                let (res, overflowed, rlay) = self.checked(op, lhs, rhs)?;
                let bool_lay = self.bool_layout()?;
                return self.write_fields(dest, dlay, None, &[(0, V::One(res), rlay), (1, V::One(overflowed), bool_lay)]);
            }
            BodyValue::Unary { op, operand } => self.unary(op, operand)?,
            BodyValue::Cast { kind, operand, ty } => {
                let to = self.ground(ty)?;
                self.cast(kind, operand, to)?
            }
            BodyValue::Aggregate { kind, ty, operands } => return self.aggregate(dest, kind, ty, operands, dlay),
            BodyValue::Repeat { operand, count } => {
                let (x, g) = self.operand(operand)?;
                let elem = self.layout(g)?;
                let n = match self.v.konst(count).kind {
                    ConstKind::Scalar { bits } => bits,
                    ConstKind::Param(idx) => self.const_param_bits(idx)?,
                    _ => return Err(self.unsupported("a repeat count that is not a scalar or param constant")),
                };
                let n = u64::try_from(n).map_err(|_| self.malformed("a repeat count past u64"))?;
                if n <= REPEAT_UNROLL_CAP {
                    let count_usize = usize::try_from(n).unwrap_or(0);
                    let count_u32 = u32::try_from(n).unwrap_or(0);
                    let mut fields: Vec<(u32, V, LayoutId), A> = Vec::with_capacity_in(count_usize, self.alloc);
                    for i in 0..count_u32 {
                        fields.push((i, x, elem));
                    }
                    return self.write_fields(dest, dlay, None, &fields);
                }
                return self.repeat_loop(dest, dlay, x, elem, n);
            }
            BodyValue::Len(place) => {
                let (at, g) = self.resolve(place)?;
                let ty = self.b.usize_ty();
                match (self.node(g)?, at) {
                    (PNode::Array(_, len), _) => {
                        let n = self.const_bits(len)?;
                        V::One(self.b.iconst(ty, n))
                    }
                    (PNode::Slice(_) | PNode::Str, At::Mem { meta: Some(m), .. }) => V::One(m),
                    _ => return Err(self.unsupported("the length of a place that is not an array or a slice behind a wide pointer")),
                }
            }
            BodyValue::Discriminant(place) => {
                let rty = match self.layouts.get(dlay).scalar {
                    Some(s) => lir(s.prim).0,
                    None => return Err(self.malformed("a discriminant written to a place that is not one scalar")),
                };
                let (at, _) = self.resolve(place)?;
                let (lay, at) = match at {
                    At::Mem { ptr, off, lay, .. } => (lay, Some((ptr, off))),
                    At::Val(_) => {
                        let lay = self.resolve_layout(place)?;
                        (lay, None)
                    }
                };
                V::One(read_discriminant(&mut self.b, &self.layouts, lay, at, rty).ok_or(self.malformed("a discriminant of an enum not in memory"))?)
            }
            BodyValue::SizeOf(ty) | BodyValue::AlignOf(ty) => {
                let g = self.ground(ty)?;
                let lay = self.layout(g)?;
                let l = *self.layouts.get(lay);
                let n = match value {
                    BodyValue::SizeOf(_) => l.size,
                    _ => l.align_bytes(),
                };
                let usize_ty = self.b.usize_ty();
                V::One(self.b.iconst(usize_ty, u128::from(n)))
            }
            BodyValue::ThreadLocalAddr(c) => match self.v.konst(c).kind {
                ConstKind::StaticAddr(item) => {
                    let r = self.tls_ref(item);
                    V::One(self.b.tls_addr(r))
                }
                _ => return Err(self.malformed("a thread-local's address whose constant is not a static")),
            },
        };
        self.write(dest, x, dlay)
    }

    /// Emits a counted LIR loop storing `x` (of layout `elem`) `n` times into `dest` (of layout
    /// `dlay`), used for large `[x; N]` repeats without bloating code size or capping `N`.
    fn repeat_loop(&mut self, dest: Place, dlay: LayoutId, x: V, elem: LayoutId, n: u64) -> Result<(), BuildError> {
        let dl = *self.layouts.get(dlay);
        if dl.is_zst() || n == 0 {
            return Ok(());
        }
        let stride = match dl.fields {
            Fields::Array { stride, .. } => stride,
            _ => self.layouts.get(elem).size,
        };
        let (ptr, off) = match self.v.place(dest).depth {
            0 => (self.base(self.v.place(dest).local)?, 0),
            _ => match self.resolve(dest)?.0 {
                At::Mem { ptr, off, .. } => (ptr, off),
                At::Val(_) => return Err(self.malformed("a large repeat into a value local")),
            },
        };
        let base = self.addr(ptr, off);
        let head = self.b.create_block(BlockKind::Normal);
        let body = self.b.create_block(BlockKind::Normal);
        let done = self.b.create_block(BlockKind::Normal);
        let zero = self.b.iconst(Ty::I64, 0);
        let limit = self.b.iconst(Ty::I64, u128::from(n));
        let stride_val = self.b.iconst(Ty::I64, u128::from(stride));
        let one = self.b.iconst(Ty::I64, 1);
        self.b.jump((head, &[zero]));
        self.b.begin(head);
        let idx = self.b.param(Ty::I64);
        let cond = self.b.icmp(IntCond::Ult, idx, limit);
        self.b.br(Flags::NONE, cond, (body, &[]), (done, &[]));
        self.b.begin(body);
        let byte_off = self.b.binary(Op::Mul, Flags::NONE, idx, stride_val);
        let dst_elem = self.b.ptr_add(Flags::NONE, base, byte_off);
        self.store_v(dst_elem, 0, elem, x)?;
        let next = self.b.binary(Op::Add, Flags::NONE, idx, one);
        self.b.jump((head, &[next]));
        self.b.begin(done);
        Ok(())
    }

    /// The layout of place `p`'s type.
    fn resolve_layout(&mut self, p: Place) -> Result<LayoutId, BuildError> {
        let (_, g) = self.resolve(p)?;
        self.layout(g)
    }

    /// The layout of `bool`.
    fn bool_layout(&mut self) -> Result<LayoutId, BuildError> {
        let g = self.subst.ground.intern(PNode::Bool).map_err(|_| self.malformed("the ground table is full"))?;
        self.layout(g)
    }

    /// An `i1` as a `bool`'s `i8`.
    fn widen_bool(&mut self, c: Value) -> Value {
        self.b.convert(Op::Zext, Flags::NONE, Ty::I8, c)
    }

    /// A shift's amount at the shifted value's type, masked to its width (MIR's `Shl` and
    /// `Shr`: the amount is taken modulo the width).
    fn shift_amount(&mut self, x: Value, y: Value) -> Value {
        let ty = self.b.ty(x);
        let y = resize_in(&mut self.b, y, ty, false);
        let mask = self.b.iconst(ty, u128::from(ty.bits(crate::PTR_BITS) - 1));
        self.b.binary(Op::And, Flags::NONE, y, mask)
    }

    /// `lhs op rhs`, wrapping (or IEEE 754 for floats).
    fn binary(&mut self, op: BinOp, lhs: Operand, rhs: Operand) -> Result<Value, BuildError> {
        let (a, ga) = self.operand(lhs)?;
        let (b, gb) = self.operand(rhs)?;
        let x = self.scalar(a)?;
        let y = self.scalar(b)?;
        if self.is_float(ga)? {
            return Ok(match op {
                BinOp::Add => self.b.binary(Op::Fadd, Flags::NONE, x, y),
                BinOp::Sub => self.b.binary(Op::Fsub, Flags::NONE, x, y),
                BinOp::Mul => self.b.binary(Op::Fmul, Flags::NONE, x, y),
                BinOp::Div => self.b.binary(Op::Fdiv, Flags::NONE, x, y),
                BinOp::Rem => self.b.binary(Op::Frem, Flags::NONE, x, y),
                BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                    let cond = match op {
                        BinOp::Eq => FloatCond::Oeq,
                        BinOp::Ne => FloatCond::Une,
                        BinOp::Lt => FloatCond::Olt,
                        BinOp::Le => FloatCond::Ole,
                        BinOp::Gt => FloatCond::Ogt,
                        _ => FloatCond::Oge,
                    };
                    let c = self.b.fcmp(cond, x, y);
                    self.widen_bool(c)
                }
                _ => return Err(self.malformed("a non-float binary operation on a float")),
            });
        }
        let signed = self.signed(ga)?;
        let pick = |s: Op, u: Op| if signed { s } else { u };
        let cmp = |s: IntCond, u: IntCond| if signed { s } else { u };
        Ok(match op {
            BinOp::Add => self.b.binary(Op::Add, Flags::NONE, x, y),
            BinOp::Sub => self.b.binary(Op::Sub, Flags::NONE, x, y),
            BinOp::Mul => self.b.binary(Op::Mul, Flags::NONE, x, y),
            BinOp::Div => self.b.binary(pick(Op::Sdiv, Op::Udiv), Flags::NONE, x, y),
            BinOp::Rem => self.b.binary(pick(Op::Srem, Op::Urem), Flags::NONE, x, y),
            BinOp::BitXor => self.b.binary(Op::Xor, Flags::NONE, x, y),
            BinOp::BitAnd => self.b.binary(Op::And, Flags::NONE, x, y),
            BinOp::BitOr => self.b.binary(Op::Or, Flags::NONE, x, y),
            BinOp::Shl => {
                let y = self.shift_amount(x, y);
                self.b.binary(Op::Shl, Flags::NONE, x, y)
            }
            BinOp::Shr => {
                let y = self.shift_amount(x, y);
                self.b.binary(pick(Op::Ashr, Op::Lshr), Flags::NONE, x, y)
            }
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                let cond = match op {
                    BinOp::Eq => IntCond::Eq,
                    BinOp::Ne => IntCond::Ne,
                    BinOp::Lt => cmp(IntCond::Slt, IntCond::Ult),
                    BinOp::Le => cmp(IntCond::Sle, IntCond::Ule),
                    BinOp::Gt => cmp(IntCond::Sgt, IntCond::Ugt),
                    _ => cmp(IntCond::Sge, IntCond::Uge),
                };
                let c = self.b.icmp(cond, x, y);
                self.widen_bool(c)
            }
            BinOp::Cmp => {
                // `Ordering` as its `i8`: -1, 0 or 1.
                let lt = self.b.icmp(cmp(IntCond::Slt, IntCond::Ult), x, y);
                let gt = self.b.icmp(cmp(IntCond::Sgt, IntCond::Ugt), x, y);
                let minus = self.b.iconst(Ty::I8, 0xff);
                let zero = self.b.iconst(Ty::I8, 0);
                let one = self.b.iconst(Ty::I8, 1);
                let below = self.b.select(Flags::NONE, lt, minus, zero);
                self.b.select(Flags::NONE, gt, one, below)
            }
            BinOp::Offset => {
                let pointee = match self.node(ga)? {
                    PNode::Ptr(_, t) | PNode::Ref(_, t) => t,
                    _ => return Err(self.malformed("an offset of a value that is not a pointer")),
                };
                let lay = self.layout(pointee)?;
                let size = self.layouts.get(lay).size;
                let count_signed = self.signed(gb)?;
                let n = resize_in(&mut self.b, y, Ty::I64, count_signed);
                let by = self.b.iconst(Ty::I64, u128::from(size));
                let bytes = self.b.binary(Op::Mul, Flags::NONE, n, by);
                self.b.ptr_add(Flags::NONE, x, bytes)
            }
        })
    }

    /// `lhs op rhs` and whether it overflowed (as a `bool`), and the result's layout.
    fn checked(&mut self, op: BinOp, lhs: Operand, rhs: Operand) -> Result<(Value, Value, LayoutId), BuildError> {
        let (a, ga) = self.operand(lhs)?;
        let (b, _) = self.operand(rhs)?;
        if self.is_float(ga)? {
            return Err(self.malformed("a checked operation on a float"));
        }
        let rlay = self.layout(ga)?;
        let x = self.scalar(a)?;
        let y = self.scalar(b)?;
        let signed = self.signed(ga)?;
        let with = match op {
            BinOp::Add => Some(if signed { Op::SaddOverflow } else { Op::UaddOverflow }),
            BinOp::Sub => Some(if signed { Op::SsubOverflow } else { Op::UsubOverflow }),
            BinOp::Mul => Some(if signed { Op::SmulOverflow } else { Op::UmulOverflow }),
            BinOp::Shl | BinOp::Shr => None,
            _ => return Err(self.malformed("a checked operation other than + - * << >>")),
        };
        if let Some(with) = with {
            let pair = self.b.overflow(with, x, y);
            let res = self.b.project(pair, 0);
            let flag = self.b.project(pair, 1);
            return Ok((res, self.widen_bool(flag), rlay));
        }
        // A shift overflows when its amount is at least the width.
        let ty = self.b.ty(x);
        let yty = self.b.ty(y);
        let width = self.b.iconst(yty, u128::from(ty.bits(crate::PTR_BITS)));
        let over = self.b.icmp(IntCond::Uge, y, width);
        let amount = self.shift_amount(x, y);
        let shift = match (op, signed) {
            (BinOp::Shl, _) => Op::Shl,
            (_, true) => Op::Ashr,
            (_, false) => Op::Lshr,
        };
        let res = self.b.binary(shift, Flags::NONE, x, amount);
        Ok((res, self.widen_bool(over), rlay))
    }

    /// `op operand`.
    fn unary(&mut self, op: UnOp, operand: Operand) -> Result<V, BuildError> {
        let (a, g) = self.operand(operand)?;
        match op {
            UnOp::PtrMetadata => match a {
                V::Two(_, m) => Ok(V::One(m)),
                V::One(_) => Ok(V::None),
                _ => Err(self.malformed("the metadata of a value that is not a pointer")),
            },
            UnOp::Not | UnOp::Neg => {
                let x = self.scalar(a)?;
                Ok(V::One(match (op, self.node(g)?) {
                    (UnOp::Not, PNode::Bool) => {
                        let one = self.b.iconst(Ty::I8, 1);
                        self.b.binary(Op::Xor, Flags::NONE, x, one)
                    }
                    (UnOp::Not, PNode::Float(_)) => return Err(self.malformed("bitwise not on a float")),
                    (UnOp::Not, _) => self.b.unary(Op::Not, Flags::NONE, x),
                    (UnOp::Neg, PNode::Float(_)) => self.b.unary(Op::Fneg, Flags::NONE, x),
                    _ => self.b.unary(Op::Neg, Flags::NONE, x),
                }))
            }
        }
    }

    /// `operand as to`.
    fn cast(&mut self, kind: CastKind, operand: Operand, to: PTy) -> Result<V, BuildError> {
        let (a, g) = self.operand(operand)?;
        let tlay = self.layout(to)?;
        let tl = *self.layouts.get(tlay);
        let target = |cx: &Self| tl.scalar.map(|s| lir(s.prim).0).ok_or(cx.malformed("a cast to a type that is not one scalar"));
        Ok(match kind {
            CastKind::IntToInt => {
                let x = self.scalar(a)?;
                let ty = target(self)?;
                let signed = self.signed(g)?;
                V::One(resize_in(&mut self.b, x, ty, signed))
            }
            CastKind::PtrToPtr | CastKind::MutToConstPointer | CastKind::UnsafeFnPointer | CastKind::FnPtrToPtr | CastKind::ArrayToPointer => {
                match (a, tl.pair.is_some()) {
                    (V::Two(p, _), false) => V::One(p),
                    (x, _) => x,
                }
            }
            CastKind::ExposeAddr => {
                let x = self.scalar(a)?;
                let ty = target(self)?;
                V::One(self.b.convert(Op::Expose, Flags::NONE, ty, x))
            }
            CastKind::FromExposedAddr => {
                let x = self.scalar(a)?;
                V::One(self.b.convert(Op::FromExposed, Flags::NONE, Ty::PTR, x))
            }
            CastKind::Transmute => {
                let flay = self.layout(g)?;
                let fl = *self.layouts.get(flay);
                if fl.size != tl.size {
                    return Err(self.malformed("a transmute between types of different sizes"));
                }
                if tl.is_zst() {
                    V::None
                } else if let (V::One(x), Some(s)) = (a, tl.scalar) {
                    let ty = lir(s.prim).0;
                    if self.b.ty(x) == ty { V::One(x) } else { V::One(self.b.convert(Op::Bitcast, Flags::NONE, ty, x)) }
                } else {
                    let (ptr, off) = self.in_memory(a, flay)?;
                    self.load_at(ptr, off, tlay)?
                }
            }
            CastKind::Unsize => self.unsize(a, g, to)?,
            CastKind::ReifyFnPointer => {
                let PNode::FnDef(item, args) = self.node(g)? else {
                    return Err(self.malformed("a fn pointer made from a value that is not a fn item"));
                };
                let target = self.reify_target(Instance { item, args })?;
                let r = self.func_ref(target);
                V::One(self.b.func_addr(r))
            }
            CastKind::IntToFloat => {
                let x = self.scalar(a)?;
                let ty = target(self)?;
                let op = if self.signed(g)? { Op::SitoFp } else { Op::UitoFp };
                V::One(self.b.convert(op, Flags::NONE, ty, x))
            }
            CastKind::FloatToInt => {
                let x = self.scalar(a)?;
                let ty = target(self)?;
                let op = if self.signed(to)? { Op::FptoSi } else { Op::FptoUi };
                V::One(self.b.convert(op, Flags::NONE, ty, x))
            }
            CastKind::FloatToFloat => {
                let x = self.scalar(a)?;
                let ty = target(self)?;
                let from_bits = self.b.ty(x).bits(crate::PTR_BITS);
                let to_bits = ty.bits(crate::PTR_BITS);
                if from_bits == to_bits {
                    V::One(x)
                } else if from_bits < to_bits {
                    V::One(self.b.convert(Op::Fpext, Flags::NONE, ty, x))
                } else {
                    V::One(self.b.convert(Op::Fptrunc, Flags::NONE, ty, x))
                }
            }
            CastKind::ClosureFnPointer => {
                let PNode::Closure { owner, ordinal, args } = self.node(g)? else {
                    return Err(self.malformed("a closure to fn pointer cast of a value that is not a closure"));
                };
                let flay = self.layout(g)?;
                if self.layouts.get(flay).size != 0 {
                    return Err(self.unsupported("a closure with captures coerced to a fn pointer"));
                }
                let c = self.adts.facts().closure(owner, ordinal)
                    .ok_or(self.unsupported("a closure whose facts the world did not answer"))?;
                let mut owner_args: Vec<PTy, A> = Vec::new_in(self.alloc);
                owner_args.extend_from_slice(
                    self.subst.ground.view().list(args)
                        .map_err(|_| self.malformed("a closure's arguments past the ground table"))?,
                );
                let sig = ground(c.types, c.sig, &owner_args, self.subst.ground, self.subst.scratch, self.adts.facts())
                    .map_err(|e| self.facts(e))?;
                let PNode::FnPtr(sig_list, _) = self.node(sig)? else {
                    return Err(self.malformed("a closure's signature that is not a fn pointer"));
                };
                let all = self.subst.ground.view().list(sig_list)
                    .map_err(|_| self.malformed("a closure's signature past the ground table"))?;
                let (_, inputs) = all.split_last().ok_or(self.malformed("a closure's signature without its output"))?;
                let mut in_vec: Vec<PTy, A> = Vec::with_capacity_in(inputs.len(), self.alloc);
                in_vec.extend_from_slice(inputs);
                let in_list = self.subst.ground.intern_list(&in_vec).map_err(|_| self.malformed("the ground table is full"))?;
                let args_tuple = self.subst.ground.intern(PNode::Tuple(in_list)).map_err(|_| self.malformed("the ground table is full"))?;
                let item = self.adts.fn_trait_method(FnTrait::FnOnce)
                    .ok_or(BuildError::NoLangFn { stmt: self.row()?, name: "call_once" })?;
                let inst_args = self.subst.ground.intern_list(&[g, args_tuple])
                    .map_err(|_| self.malformed("the ground table is full"))?;
                let r = self.func_ref(Instance { item, args: inst_args });
                V::One(self.b.func_addr(r))
            }
        })
    }

    /// The target instance for a fn-item reification: `ReifyShim` when `instance` is
    /// `#[track_caller]`, else `instance` itself.
    pub(crate) fn reify_target(&mut self, instance: Instance) -> Result<Instance, BuildError> {
        if self.adts.track_caller(instance, self.subst.ground) == Some(true) {
            return reify_shim_instance(instance, self.subst.ground).map_err(|e| self.facts(e));
        }
        Ok(instance)
    }

    /// `kind ty (operands)` written field by field into `dest`.
    fn aggregate(&mut self, dest: Place, kind: AggregateKind, ty: PTy, operands: blaze_body::Ops, dlay: LayoutId) -> Result<(), BuildError> {
        let mut is_union = false;
        let variant = match kind {
            AggregateKind::Tuple | AggregateKind::Array | AggregateKind::Closure => None,
            AggregateKind::Adt(v) => {
                let g = self.ground(ty)?;
                if let PNode::Adt(key, _) = self.node(g)?
                    && self.adts.adt(key).is_some_and(|adt| adt.kind == AdtKind::Union)
                {
                    is_union = true;
                    None
                } else {
                    Some(v)
                }
            }
            AggregateKind::Coroutine => return self.coroutine_aggregate(dest, ty, operands, dlay),
        };
        let ops: Vec<Operand, A> = {
            let mut out = Vec::with_capacity_in(operands.len() as usize, self.alloc);
            out.extend(self.v.ops(operands));
            out
        };
        if is_union {
            let Some(&o) = ops.first() else {
                return Err(self.malformed("a union aggregate without its active field operand"));
            };
            let (x, g) = self.operand(o)?;
            let flay = self.layout(g)?;
            let (ptr, off) = match self.v.place(dest).depth {
                0 => (self.base(self.v.place(dest).local)?, 0),
                _ => match self.resolve(dest)?.0 {
                    At::Mem { ptr, off, .. } => (ptr, off),
                    At::Val(_) => return Err(self.malformed("a union written to a value local")),
                },
            };
            return self.store_v(ptr, off, flay, x);
        }
        let mut fields: Vec<(u32, V, LayoutId), A> = Vec::with_capacity_in(ops.len(), self.alloc);
        for (i, &o) in ops.iter().enumerate() {
            let (x, g) = self.operand(o)?;
            let flay = self.layout(g)?;
            fields.push((i as u32, x, flay));
        }
        self.write_fields(dest, dlay, variant, &fields)
    }

    /// The principal trait of ground trait object `t` and its arguments after `Self`, `None` for
    /// auto-trait-only `dyn Send`.
    pub(crate) fn principal(&self, t: PTy) -> Result<Option<(blaze_typeck::ItemKey, List)>, BuildError> {
        let view = self.subst.ground.view();
        let PNode::Dynamic(l) = view.node(t).map_err(|_| self.malformed("a type past the ground table"))? else {
            return Err(self.malformed("a trait object's principal asked of another type"));
        };
        let preds = view.list(l).map_err(|_| self.malformed("a trait object's predicates past the ground table"))?;
        let Some(&first) = preds.first() else {
            return Err(self.unsupported("a trait object with no predicates"));
        };
        Ok(match view.node(first).map_err(|_| self.malformed("a type past the ground table"))? {
            PNode::ExTrait(k, args) => Some((k, args)),
            _ => None,
        })
    }

    /// Unsizes reference/raw-pointer value `a` of ground type `from_ty` to `to_ty`:
    /// `&[T; N] -> &[T]`, `&T -> &dyn Tr`, `&dyn Sub -> &dyn Super`, and struct-tail unsizing.
    fn unsize(&mut self, a: V, from_ty: PTy, to_ty: PTy) -> Result<V, BuildError> {
        let pointee_of = |cx: &Self, ty: PTy| match cx.node(ty)? {
            PNode::Ref(_, t) | PNode::Ptr(_, t) => Ok(t),
            _ => Err(cx.unsupported("an unsizing of a value that is not a reference or raw pointer")),
        };
        let mut f = pointee_of(self, from_ty)?;
        let mut t = pointee_of(self, to_ty)?;
        // Peel matching struct tails (`S<[T; N]>` -> `S<[T]>`, `S<T>` -> `S<dyn Tr>`).
        for _ in 0..16 {
            if f == t {
                break;
            }
            let (PNode::Adt(k1, lf), PNode::Adt(k2, lt)) = (self.node(f)?, self.node(t)?) else {
                break;
            };
            if k1 != k2 {
                break;
            }
            let Some(adt) = self.adts.adt(k1) else {
                return Err(self.malformed("an ADT without its definition during unsizing"));
            };
            if adt.kind != AdtKind::Struct || adt.variants.len() != 1 {
                return Err(self.unsupported("an unsizing of a non-struct ADT tail"));
            }
            let Some(fields) = adt.fields.get(adt.variants[0].fields.range()) else {
                return Err(self.malformed("an ADT's fields past their column"));
            };
            let Some(last) = fields.last() else {
                return Err(self.unsupported("an unsizing of an empty struct"));
            };
            let (types, last_ty) = (adt.types, last.ty);
            let mut args_f: Vec<PTy, A> = Vec::new_in(self.alloc);
            args_f.extend_from_slice(self.subst.ground.view().list(lf).map_err(|_| self.malformed("a struct's arguments past the ground table"))?);
            let next_f = ground(types, last_ty, &args_f, self.subst.ground, self.subst.scratch, self.adts.facts()).map_err(|e| self.facts(e))?;
            let mut args_t: Vec<PTy, A> = Vec::new_in(self.alloc);
            args_t.extend_from_slice(self.subst.ground.view().list(lt).map_err(|_| self.malformed("a struct's arguments past the ground table"))?);
            let next_t = ground(types, last_ty, &args_t, self.subst.ground, self.subst.scratch, self.adts.facts()).map_err(|e| self.facts(e))?;
            f = next_f;
            t = next_t;
        }
        let (data, old_meta) = match a {
            V::One(p) => (p, None),
            V::Two(p, m) => (p, Some(m)),
            V::Mem { ptr, off, lay } => match self.load_at(ptr, off, lay)? {
                V::One(p) => (p, None),
                V::Two(p, m) => (p, Some(m)),
                _ => return Err(self.malformed("an unsized pointer operand that is not a pointer")),
            },
            V::None => return Err(self.malformed("an unsizing of a zero-sized value")),
        };
        let meta = match (self.node(f)?, self.node(t)?, old_meta) {
            (PNode::Array(_, len), PNode::Slice(_), None) => {
                let n = self.const_bits(len)?;
                self.b.iconst(Ty::I64, n)
            }
            (PNode::Dynamic(_), PNode::Dynamic(_), Some(vt)) => {
                let (pf, pt) = (self.principal(f)?, self.principal(t)?);
                match (pf, pt) {
                    (_, None) => vt,
                    (Some((a_tr, _)), Some((b_tr, _))) if a_tr == b_tr => vt,
                    (Some((a_tr, _)), Some((b_tr, _))) => {
                        let shape = vtable::Shape::of(a_tr, |tr| self.adts.object_trait(tr), self.subst.scratch)
                            .map_err(|error| BuildError::Vtable { stmt: self.stmt, error })?;
                        match shape.upcast(b_tr) {
                            Some(vtable::Upcast::Prefix) => vt,
                            Some(vtable::Upcast::Load(slot)) => {
                                let off = 8 * (HEADER_WORDS + slot);
                                let off = i32::try_from(off).map_err(|_| self.malformed("a vtable slot past 2^31 bytes"))?;
                                self.b.load(Ty::PTR, Flags::NONE, vt, off, MemAttrs::aligned(3))
                            }
                            None => return Err(self.malformed("an upcast to a trait that is not a supertrait")),
                        }
                    }
                    (None, Some(_)) => return Err(self.malformed("an upcast from a trait object without a principal")),
                }
            }
            (_, PNode::Dynamic(_), None) => {
                let (principal, args) = self.principal(t)?.unwrap_or((vtable::AUTO_ONLY_PRINCIPAL, List::EMPTY));
                self.vtable_addr(VtableRef { concrete: f, principal, args })
            }
            _ => return Err(self.unsupported("an unsizing coercion of an unsupported shape")),
        };
        Ok(V::Two(data, meta))
    }

    /// The vtable reference of `table`, deduplicated into `Built::vtables`.
    pub(crate) fn vtable_ref(&mut self, table: VtableRef) -> RefId {
        if let Some(i) = self.vtables.iter().position(|&t| t == table)
            && let Some(&r) = self.vtable_ids.get(i)
        {
            return r;
        }
        let place = u32::try_from(self.vtables.len()).unwrap_or(u32::MAX);
        let r = self.b.reference(RefKind::Vtable, place);
        self.vtables.push(table);
        self.vtable_ids.push(r);
        r
    }

    /// The address of `table`: `data_addr` of its `Vtable` reference.
    pub(crate) fn vtable_addr(&mut self, table: VtableRef) -> Value {
        let r = self.vtable_ref(table);
        self.b.data_addr(r)
    }

    /// The function reference of `instance`, one per instance.
    pub(crate) fn func_ref(&mut self, instance: Instance) -> blaze_lir::func::RefId {
        if let Some(i) = self.refs.iter().position(|&r| r == instance)
            && let Some(&r) = self.ref_ids.get(i)
        {
            return r;
        }
        let place = self.refs.len() as u32;
        let r = self.b.reference(RefKind::Func, place);
        self.refs.push(instance);
        self.ref_ids.push(r);
        r
    }

    /// The static reference of `item`, one per static item.
    pub(crate) fn static_ref(&mut self, item: blaze_typeck::ItemKey) -> blaze_lir::func::RefId {
        if let Some(i) = self.statics.iter().position(|&k| k == item)
            && let Some(&r) = self.static_ids.get(i)
        {
            return r;
        }
        let place = u32::try_from(self.statics.len()).unwrap_or(u32::MAX);
        let r = self.b.reference(RefKind::Static, place);
        self.statics.push(item);
        self.static_ids.push(r);
        r
    }

    /// The thread-local reference of `item`, one per thread-local static.
    pub(crate) fn tls_ref(&mut self, item: blaze_typeck::ItemKey) -> blaze_lir::func::RefId {
        if let Some(i) = self.thread_locals.iter().position(|&k| k == item)
            && let Some(&r) = self.tls_ids.get(i)
        {
            return r;
        }
        let place = u32::try_from(self.thread_locals.len()).unwrap_or(u32::MAX);
        let r = self.b.reference(RefKind::Tls, place);
        self.thread_locals.push(item);
        self.tls_ids.push(r);
        r
    }
}
