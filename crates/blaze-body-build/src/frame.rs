//! The frame: which locals are values and which are memory, the function's signature, its entry
//! block and the parameters of every body block (lib.rs, "The frame" and "Block parameters").

use blaze_own::Vec;
use core::alloc::Allocator;

use blaze_body::{Block as BodyBlock, BlockKind as BodyKind, Local, LocalKind, Operand, Place, Proj, StmtKind, Value as BodyValue};
use blaze_build::cabi::{self, CAbiError, Class};
use blaze_build::drop_facts::needs_drop;
use blaze_build::intrinsics::{lir, resize_in};
use blaze_build::layout::{LayoutCx, LayoutError, LayoutId, Prim, Variants};
use blaze_build::{Abi, Instance, Panic, abi};
use blaze_lir::builder::{MemAttrs, Param, Variadic};
use blaze_lir::func::{BlockKind, Conv, Ext, SlotKind};
use blaze_lir::{Flags, Op, Ty, Value};
use blaze_typeck::{AdtKind, LangItem, PNode, PTy};

use crate::{BuildError, Cx, Held, Pass, Track, V};

/// Integer width for C ABI `Pass::Words` power-of-two chunk loads/stores: log2 in 0..=3 -> 1, 2, 4, 8 bytes.
fn word_width(log: u32) -> Ty {
    match log {
        0 => Ty::I8,
        1 => Ty::I16,
        2 => Ty::I32,
        _ => Ty::I64,
    }
}

impl<A: Allocator + Copy> Cx<'_, '_, '_, A> {
    /// Whether the projection nearest local of place `p` is a dereference: then a use of `p` is
    /// a use of memory the local points to, not of the local's own. `None` for the local itself.
    fn first_proj(&self, p: Place) -> Result<Option<Proj>, BuildError> {
        let mut q = p;
        for _ in 0..=self.v.place(p).depth {
            let row = self.v.place(q);
            if row.depth <= 1 {
                return Ok(row.proj);
            }
            q = row.parent.ok_or(self.malformed("a projected place without its parent"))?;
        }
        Err(self.malformed("a place deeper than its depth says"))
    }

    /// Marks the local of `p` when the use reaches the local's own bytes: the local itself
    /// (`whole`), or a place inside it not behind a dereference.
    fn mark(&self, p: Place, whole: bool, taken: &mut [bool]) -> Result<(), BuildError> {
        let local = self.v.place(p).local.index();
        let own = match self.first_proj(p)? {
            None => whole,
            Some(Proj::Deref) => false,
            Some(_) => true,
        };
        if own {
            *taken.get_mut(local).ok_or(self.malformed("a place of a local past the frame"))? = true;
        }
        Ok(())
    }

    /// Marks `p`'s local when `p` indexes or subslices the local's own bytes (even on reads,
    /// because `Proj::Index`, `ConstIndex`, and `Subslice` compute addresses in memory).
    fn mark_read_place(&self, p: Place, taken: &mut [bool]) -> Result<(), BuildError> {
        if matches!(self.first_proj(p)?, Some(Proj::Index(_) | Proj::ConstIndex { .. } | Proj::Subslice { .. })) {
            let local = self.v.place(p).local.index();
            *taken.get_mut(local).ok_or(self.malformed("a place of a local past the frame"))? = true;
        }
        Ok(())
    }

    fn mark_operand(&self, o: Operand, taken: &mut [bool]) -> Result<(), BuildError> {
        if let Operand::Copy(p) | Operand::Move(p) = o {
            self.mark_read_place(p, taken)?;
        }
        Ok(())
    }

    /// The address-taken locals, by one pass over the rows: a borrow or raw borrow of a place
    /// of the local's own bytes, a projection written through, a drop, or an indexed/subsliced
    /// local.
    pub(crate) fn address_taken(&self) -> Result<Vec<bool, A>, BuildError> {
        let mut taken = Vec::new_in(self.alloc);
        taken.resize(self.v.locals() as usize, false);
        for s in 0..self.v.stmts() {
            match self.v.stmt(blaze_body::Stmt(s)).kind {
                StmtKind::Assign { dest, value } => {
                    self.mark(dest, false, &mut taken)?;
                    match value {
                        BodyValue::Borrow { place, .. } | BodyValue::RawAddr { place, .. } => {
                            self.mark(place, true, &mut taken)?;
                        }
                        BodyValue::Use(o) | BodyValue::Unary { operand: o, .. } | BodyValue::Cast { operand: o, .. } | BodyValue::Repeat { operand: o, .. } => {
                            self.mark_operand(o, &mut taken)?;
                        }
                        BodyValue::Binary { lhs, rhs, .. } | BodyValue::CheckedBinary { lhs, rhs, .. } => {
                            self.mark_operand(lhs, &mut taken)?;
                            self.mark_operand(rhs, &mut taken)?;
                        }
                        BodyValue::Aggregate { operands, .. } => {
                            for o in self.v.ops(operands) {
                                self.mark_operand(o, &mut taken)?;
                            }
                        }
                        BodyValue::Len(place) | BodyValue::Discriminant(place) => {
                            self.mark_read_place(place, &mut taken)?;
                        }
                        BodyValue::SizeOf(_) | BodyValue::AlignOf(_) | BodyValue::ThreadLocalAddr(_) => {}
                    }
                }
                StmtKind::Call { callee, args, dest, .. } => {
                    if let Some(dest) = dest {
                        self.mark(dest, false, &mut taken)?;
                    }
                    if let blaze_body::Callee::Value(o) = self.v.callee(callee) {
                        self.mark_operand(o, &mut taken)?;
                    }
                    for o in self.v.ops(args) {
                        self.mark_operand(o, &mut taken)?;
                    }
                }
                StmtKind::TailCall { callee, args } => {
                    if let blaze_body::Callee::Value(o) = self.v.callee(callee) {
                        self.mark_operand(o, &mut taken)?;
                    }
                    for o in self.v.ops(args) {
                        self.mark_operand(o, &mut taken)?;
                    }
                }
                StmtKind::InlineAsm { asm, operands, .. } => {
                    for o in self.v.ops(operands) {
                        self.mark_operand(o, &mut taken)?;
                    }
                    if asm < self.v.asms() {
                        let row = self.v.asm(asm);
                        for oi in row.ops.start..row.ops.end {
                            if let Some(p) = self.v.asm_op(oi).out_place {
                                self.mark(p, false, &mut taken)?;
                            }
                        }
                    }
                }
                StmtKind::Switch { on, .. } => self.mark_operand(on, &mut taken)?,
                StmtKind::Assert { cond, msg, .. } => {
                    self.mark_operand(cond, &mut taken)?;
                    for o in self.v.ops(msg) {
                        self.mark_operand(o, &mut taken)?;
                    }
                }
                StmtKind::Drop { place, .. } => self.mark(place, true, &mut taken)?,
                // The tag is written in memory, by the layout's one encoding.
                StmtKind::SetDiscriminant { place, .. } => self.mark(place, true, &mut taken)?,
                _ => {}
            }
        }
        Ok(taken)
    }

    /// How a value of layout `lay` and ground type `g` is held when its address is never taken.
    fn value_held(&self, lay: LayoutId, g: PTy) -> Result<Option<Held>, BuildError> {
        let l = *self.layouts.get(lay);
        if l.tail != blaze_build::layout::Tail::Sized {
            return Err(self.unsupported("a local of an unsized type"));
        }
        if let Variants::Multiple { .. } = l.variants {
            return Ok(None);
        }
        if let PNode::Adt(key, _) = self.node(g)?
            && self.adts.adt(key).is_some_and(|adt| adt.kind == AdtKind::Union)
        {
            return Ok(None);
        }
        if l.is_zst() {
            return Ok(Some(Held::None));
        }
        if let Some(s) = l.scalar {
            return Ok(Some(Held::One(lir(s.prim).0)));
        }
        if let Some((a, b)) = l.pair {
            return Ok(Some(Held::Two(lir(a.prim).0, lir(b.prim).0)));
        }
        Ok(None)
    }

    /// A stack slot for a value of layout `lay`.
    pub(crate) fn new_slot(&mut self, lay: LayoutId, kind: SlotKind) -> Result<blaze_lir::func::SlotId, BuildError> {
        let l = *self.layouts.get(lay);
        let size = u32::try_from(l.size).map_err(|_| self.unsupported("a local larger than 4 GiB"))?;
        Ok(self.b.slot(size, l.align, kind))
    }

    /// Use the same layout decision for a parameter, an argument and a return place under the Rust ABI.
    pub(crate) fn pass_of(&mut self, g: PTy) -> Result<Pass, BuildError> {
        let id = self.layout(g)?;
        let layout = *self.layouts.get(id);
        layout.rust_pass(self.subst.ground.view())
            .map_err(|why| BuildError::Layout { stmt: self.stmt, error: LayoutError { ty: g, why } })
    }

    /// How ground type `g` crosses a call of `abi` (`Rust`, `RustCall`, or `C`), matching
    /// `blaze-build`'s `walk.rs` `pass`.
    pub(crate) fn pass_of_abi(&mut self, g: PTy, abi: Abi) -> Result<Pass, BuildError> {
        if matches!(abi, Abi::Rust | Abi::RustCall) {
            return self.pass_of(g);
        }
        let stmt = self.stmt;
        let refuse = |why| BuildError::CAbi { stmt, why };
        let id = self.layout(g)?;
        let class = {
            let mut cx = LayoutCx { ground: &mut *self.subst.ground, facts: self.adts.facts(), scratch: self.subst.scratch };
            cabi::class_of(&mut self.layouts, g, &mut cx, self.alloc)
        };
        let class = class.map_err(|error| match error {
            CAbiError::Layout(error) => BuildError::Layout { stmt, error },
            CAbiError::Refused { why, .. } => refuse(why),
            CAbiError::TooLarge => refuse("more than 2 GiB of stacked arguments"),
        })?;
        let l = *self.layouts.get(id);
        Ok(match class {
            Class::Ignore => Pass::Ignore,
            Class::Int { size: 16, .. } => return Err(refuse("a 128-bit integer (a register pair)")),
            Class::Int { size, signed } => {
                let ext = match (size < 4, signed) {
                    (true, true) => Ext::Sext,
                    (true, false) => Ext::Zext,
                    (false, _) => Ext::None,
                };
                let s = l.scalar.ok_or(self.malformed("a C integer class of a type with no scalar"))?;
                Pass::Scalar { ty: lir(s.prim).0, ext, agg: false }
            }
            Class::Float { .. } => {
                let s = l.scalar.ok_or(self.malformed("a C float class of a type with no scalar"))?;
                Pass::Scalar { ty: lir(s.prim).0, ext: Ext::None, agg: false }
            }
            Class::Hfa { .. } => return Err(refuse("a homogeneous float aggregate (the SIMD registers are not built)")),
            Class::Composite { size, align } if size > 16 => {
                let size = u32::try_from(size).map_err(|_| refuse("a composite past 4 GiB"))?;
                Pass::Memory { size, align }
            }
            Class::Composite { align: 4.., .. } => return Err(refuse("a 16-byte-aligned composite (an even register pair)")),
            Class::Composite { size, .. } => match self.pass_of(g)? {
                wide @ Pass::Wide { .. } => wide,
                _ => {
                    let size = u32::try_from(size).map_err(|_| refuse("a composite past 4 GiB"))?;
                    Pass::Words { size }
                }
            },
        })
    }

    /// Reads `size` (1..=8) bytes at `ptr + off + start` (aligned to `align`) into a zero-extended
    /// `i64` using power-of-two loads that never read past the value.
    pub(crate) fn read_word(&mut self, ptr: Value, off: i32, start: u32, size: u32, align: u8) -> Result<Value, BuildError> {
        let mut word = None;
        let mut pos = 0u32;
        while pos < size {
            let log = (31 - (size - pos).leading_zeros()).min(3);
            let ty = word_width(log);
            let log_u8 = u8::try_from(log).unwrap_or(3);
            let tz_u8 = u8::try_from((start + pos).trailing_zeros().min(7)).unwrap_or(7);
            let a = align.min(log_u8).min(tz_u8);
            let delta = i32::try_from(start + pos).map_err(|_| self.malformed("a C register offset past i32"))?;
            let x = self.b.load(ty, Flags::NONE, ptr, off + delta, MemAttrs::aligned(a));
            let mut x = resize_in(&mut self.b, x, Ty::I64, false);
            if pos > 0 {
                let by = self.b.iconst(Ty::I64, u128::from(pos * 8));
                x = self.b.binary(Op::Shl, Flags::NONE, x, by);
            }
            word = Some(match word {
                None => x,
                Some(w) => self.b.binary(Op::Or, Flags::NONE, w, x),
            });
            pos += 1 << log;
        }
        word.ok_or(self.malformed("a C register of no bytes"))
    }

    /// Writes the low `size` (1..=8) bytes of `i64` `word` at `ptr + off + start` (aligned to
    /// `align`) in power-of-two stores that never write past the value.
    pub(crate) fn write_word(&mut self, word: Value, ptr: Value, off: i32, start: u32, size: u32, align: u8) {
        let mut pos = 0u32;
        while pos < size {
            let log = (31 - (size - pos).leading_zeros()).min(3);
            let ty = word_width(log);
            let log_u8 = u8::try_from(log).unwrap_or(3);
            let tz_u8 = u8::try_from((start + pos).trailing_zeros().min(7)).unwrap_or(7);
            let a = align.min(log_u8).min(tz_u8);
            let mut x = word;
            if pos > 0 {
                let by = self.b.iconst(Ty::I64, u128::from(pos * 8));
                x = self.b.binary(Op::Lshr, Flags::NONE, x, by);
            }
            let x = resize_in(&mut self.b, x, ty, false);
            let delta = i32::try_from(start + pos).unwrap_or(0);
            self.b.store(Flags::NONE, x, ptr, off + delta, MemAttrs::aligned(a));
            pos += 1 << log;
        }
    }

    /// The two typed registers of a wide pointer, whether kept in registers or in a slot.
    pub(crate) fn wide_values(&mut self, value: V) -> Result<[Value; 2], BuildError> {
        let value = match value {
            V::Mem { ptr, off, lay } => self.load_at(ptr, off, lay)?,
            v => v,
        };
        let V::Two(a, b) = value else { return Err(self.malformed("a two-register argument or result without both scalar values")) };
        Ok([a, b])
    }

    /// Whether ground type `g` needs drop (blaze-build's one rule, `drop_facts::needs_drop`).
    pub(crate) fn needs_drop_of(&mut self, g: PTy) -> Result<bool, BuildError> {
        let manually_drop = self.adts.lang_adt(LangItem::ManuallyDrop);
        let adts = self.adts;
        let stmt = self.stmt;
        needs_drop(g, self.subst.ground, adts.facts(), manually_drop, &mut |t, ground| adts.drop_impl(t, ground), self.subst.scratch)
            .map_err(|error| BuildError::NeedsDrop { stmt, error })
    }

    /// The frame (the module doc): every local's ground type, layout and hold; the signature;
    /// the entry block, which takes the parameters and jumps to the body's first block; and
    /// the LIR block of every body block.
    pub(crate) fn frame_inner(&mut self, is_closure: bool) -> Result<(), BuildError> {
        let taken = self.address_taken()?;
        let locals = self.v.locals() as usize;
        let mut args: Vec<Option<Local>, A> = Vec::new_in(self.alloc);
        args.resize(self.v.args() as usize, None);
        let mut ret_local = None;
        for i in 0..locals {
            let l = Local(i as u32);
            let decl = self.v.local(l);
            let g = self.ground(decl.ty)?;
            let lay = self.layout(g)?;
            self.tys.push(g);
            self.lays.push(lay);
            match decl.kind {
                LocalKind::Return => ret_local = Some(l),
                LocalKind::Arg(n) => {
                    *args.get_mut(n as usize).ok_or(self.malformed("an argument local past the argument count"))? = Some(l);
                }
                _ => {}
            }
        }
        let ret_local = ret_local.ok_or(self.malformed("a body without its return place"))?;
        let abi = self.abi;
        let c_variadic = !is_closure && self.adts.c_variadic(self.item).is_some();
        let va_local = if c_variadic {
            let last = args
                .last()
                .copied()
                .flatten()
                .ok_or(self.malformed("a C-variadic function missing its trailing `...` parameter"))?;
            Some(last)
        } else {
            None
        };
        // The signature: a result pointer first when the result is passed indirectly (`Pass::Ref`
        // or C's `Pass::Memory` sret). For `Abi::RustCall`, the last parameter's tuple is spread
        // into its field passes. For C-variadic definitions, the trailing `VaList` local is
        // materialized via `blaze.va_start` rather than taken as a fixed LIR parameter.
        let ret_ty = *self.tys.get(ret_local.index()).ok_or(self.malformed("a return local past the frame"))?;
        self.ret = self.pass_of_abi(ret_ty, abi)?;
        let mut passes: Vec<Pass, A> = Vec::with_capacity_in(args.len(), self.alloc);
        let mut rust_call_spread: Option<(Local, Vec<(u32, LayoutId, Pass), A>)> = None;
        let regular_count = if c_variadic || (abi == Abi::RustCall && !args.is_empty()) {
            args.len().saturating_sub(1)
        } else {
            args.len()
        };
        for a in args.iter().take(regular_count) {
            let a = a.ok_or(self.malformed("an argument without its local"))?;
            let a_ty = *self.tys.get(a.index()).ok_or(self.malformed("an argument local past the frame"))?;
            passes.push(self.pass_of_abi(a_ty, abi)?);
        }
        if !c_variadic
            && abi == Abi::RustCall
            && let Some(last) = args.last()
        {
            let last = last.ok_or(self.malformed("an argument without its local"))?;
            let last_ty = *self.tys.get(last.index()).ok_or(self.malformed("an argument local past the frame"))?;
            let PNode::Tuple(list) = self.node(last_ty)? else {
                return Err(self.malformed("a \"rust-call\" fn whose last parameter is not a tuple"));
            };
            let field_tys: Vec<PTy, A> = {
                let cap = usize::try_from(list.len).unwrap_or(0);
                let mut out = Vec::with_capacity_in(cap, self.alloc);
                let slice = self.subst.ground.view().list(list).map_err(|_| self.malformed("a tuple's fields past the ground table"))?;
                out.extend_from_slice(slice);
                out
            };
            let mut fields_plan: Vec<(u32, LayoutId, Pass), A> = Vec::with_capacity_in(field_tys.len(), self.alloc);
            for (idx, &fty) in field_tys.iter().enumerate() {
                let flay = self.layout(fty)?;
                let fpass = self.pass_of(fty)?;
                passes.push(fpass);
                let idx_u32 = u32::try_from(idx).map_err(|_| self.malformed("too many fields in a \"rust-call\" tuple"))?;
                fields_plan.push((idx_u32, flay, fpass));
            }
            rust_call_spread = Some((last, fields_plan));
        }
        // `#[track_caller]`: a tracked fn takes one hidden last parameter (`&'static Location`).
        let own_tracked = if is_closure {
            self.track = Track::No;
            false
        } else {
            let own_args = self.subst.ground.intern_list(self.subst.args).map_err(|_| self.malformed("the ground table is full"))?;
            let own_inst = Instance { item: self.item, args: own_args };
            match self.adts.track_caller(own_inst, self.subst.ground).map_err(|e| self.facts(e))? {
                Some(true) => {
                    if c_variadic {
                        return Err(self.malformed("a C-variadic fn that is `#[track_caller]` (E0737)"));
                    }
                    passes.push(Pass::Scalar { ty: Ty::PTR, ext: Ext::None, agg: false });
                    true
                }
                Some(false) => {
                    self.track = Track::No;
                    false
                }
                None => {
                    self.track = Track::Unsaid;
                    false
                }
            }
        };
        let (params, rets) = abi::sig_of_in(&passes, self.ret, self.alloc);
        let variadic = if c_variadic { Variadic::Yes } else { Variadic::No };
        let sig = self.b.sig(Conv::C, variadic, &params, &rets);
        self.b.set_signature(sig);
        let entry = self.b.create_block(BlockKind::Normal);
        self.b.begin(entry);
        let mut values: Vec<Value, A> = Vec::with_capacity_in(params.len(), self.alloc);
        for p in params.iter() {
            values.push(self.b.param(p.ty));
        }
        // How each local is held: memory when its address is taken or its layout is not one or
        // two scalars; a parameter in memory at its caller's copy, the result at its pointer.
        let mut decided: Vec<Option<Held>, A> = Vec::new_in(self.alloc);
        decided.resize(locals, None);
        let mut memory: Vec<bool, A> = Vec::with_capacity_in(locals, self.alloc);
        for ((&is_taken, &lay), &g) in taken.iter().zip(self.lays.iter()).zip(self.tys.iter()) {
            memory.push(is_taken || self.value_held(lay, g)?.is_none());
        }
        let mut next = values.iter().copied();
        if matches!(self.ret, Pass::Ref | Pass::Memory { .. }) {
            let hidden = next.next().ok_or(self.malformed("a signature without its result pointer"))?;
            self.sret = Some(hidden);
            *decided.get_mut(ret_local.index()).ok_or(self.malformed("a return local past the frame"))? = Some(Held::At(hidden));
        }
        let mut given: Vec<(Local, V), A> = Vec::with_capacity_in(args.len(), self.alloc);
        let mut given_words: Vec<(Local, u32, [Option<Value>; 2]), A> = Vec::new_in(self.alloc);
        for (a, &pass) in args.iter().take(regular_count).zip(passes.iter()) {
            let a = a.ok_or(self.malformed("an argument without its local"))?;
            match pass {
                Pass::Ignore => {}
                pass @ (Pass::Scalar { .. } | Pass::Ref | Pass::Memory { .. }) => {
                    let x = next.next().ok_or(self.malformed("a parameter without its register"))?;
                    let is_mem = *memory.get(a.index()).ok_or(self.malformed("an argument local past the frame"))?;
                    let indirect = matches!(pass, Pass::Ref | Pass::Memory { .. });
                    let lay = *self.lays.get(a.index()).ok_or(self.malformed("an argument local past the frame"))?;
                    if pass == Pass::Ref {
                        let l = *self.layouts.get(lay);
                        self.ref_params.push((x, l.size, l.align));
                    }
                    if indirect && is_mem {
                        *decided.get_mut(a.index()).ok_or(self.malformed("an argument local past the frame"))? = Some(Held::At(x));
                    } else if indirect {
                        let value = self.load_at(x, 0, lay)?;
                        given.push((a, value));
                    } else {
                        given.push((a, V::One(x)));
                    }
                }
                Pass::Wide { .. } => {
                    let lo = next.next().ok_or(self.malformed("a wide parameter without its address"))?;
                    let hi = next.next().ok_or(self.malformed("a wide parameter without its metadata"))?;
                    given.push((a, V::Two(lo, hi)));
                }
                Pass::Words { size } => {
                    let w0 = next.next().ok_or(self.malformed("a C composite parameter without its first word"))?;
                    let w1 = if size > 8 {
                        Some(next.next().ok_or(self.malformed("a C composite parameter without its second word"))?)
                    } else {
                        None
                    };
                    given_words.push((a, size, [Some(w0), w1]));
                }
            }
        }
        // Unpack `Abi::RustCall` spread tuple fields from the remaining entry parameters.
        let mut spread_values: Option<(Local, Vec<(u32, V, LayoutId), A>)> = None;
        if let Some((last, fields_plan)) = rust_call_spread {
            let mut fvals: Vec<(u32, V, LayoutId), A> = Vec::with_capacity_in(fields_plan.len(), self.alloc);
            for &(idx, flay, fpass) in fields_plan.iter() {
                let v = match fpass {
                    Pass::Ignore => V::None,
                    Pass::Scalar { .. } => V::One(next.next().ok_or(self.malformed("a spread parameter without its register"))?),
                    Pass::Wide { .. } => {
                        let lo = next.next().ok_or(self.malformed("a spread wide parameter without its address"))?;
                        let hi = next.next().ok_or(self.malformed("a spread wide parameter without its metadata"))?;
                        V::Two(lo, hi)
                    }
                    Pass::Ref => {
                        let ptr = next.next().ok_or(self.malformed("a spread indirect parameter without its pointer"))?;
                        self.load_at(ptr, 0, flay)?
                    }
                    Pass::Memory { .. } | Pass::Words { .. } => return Err(self.malformed("a C pass in a \"rust-call\" tuple")),
                };
                fvals.push((idx, v, flay));
            }
            spread_values = Some((last, fvals));
        }
        if own_tracked {
            let loc = next.next().ok_or(self.malformed("a `#[track_caller]` fn without its hidden location parameter"))?;
            self.track = Track::Passed(loc);
        }
        self.cur.resize(locals, [None, None]);
        for i in 0..locals {
            let dec = *decided.get(i).ok_or(self.malformed("a local past the frame"))?;
            let is_mem = *memory.get(i).ok_or(self.malformed("a local past the frame"))?;
            let lay = *self.lays.get(i).ok_or(self.malformed("a local past the frame"))?;
            let g = *self.tys.get(i).ok_or(self.malformed("a local past the frame"))?;
            let held = match dec {
                Some(held) => held,
                None if is_mem => {
                    let kind = match self.v.local(Local(i as u32)).kind {
                        LocalKind::Binding(_) => SlotKind::Named,
                        _ => SlotKind::Temp,
                    };
                    Held::Slot(self.new_slot(lay, kind)?)
                }
                None => self.value_held(lay, g)?.ok_or(self.malformed("a value local of no value hold"))?,
            };
            self.held.push(held);
        }
        // The parameters' values put where their locals are.
        for &(a, value) in given.iter() {
            let lay = *self.lays.get(a.index()).ok_or(self.malformed("an argument local past the frame"))?;
            self.put(a, value, lay)?;
        }
        for &(a, size, [w0, w1]) in given_words.iter() {
            let lay = *self.lays.get(a.index()).ok_or(self.malformed("an argument local past the frame"))?;
            let align = self.layouts.get(lay).align;
            let held = *self.held.get(a.index()).ok_or(self.malformed("an argument local past the frame"))?;
            let (ptr, is_temp) = match held {
                Held::Slot(_) | Held::At(_) | Held::AtOff(..) => (self.base(a)?, false),
                _ => {
                    let slot = self.new_slot(lay, SlotKind::Temp)?;
                    (self.b.slot_addr(slot), true)
                }
            };
            if let Some(w0) = w0 {
                self.write_word(w0, ptr, 0, 0, size.min(8), align);
            }
            if let Some(w1) = w1 {
                self.write_word(w1, ptr, 0, 8, size - 8, align);
            }
            if is_temp {
                let v = self.load_at(ptr, 0, lay)?;
                self.put(a, v, lay)?;
            }
        }
        if let Some((last, fvals)) = spread_values {
            let lay = *self.lays.get(last.index()).ok_or(self.malformed("an argument local past the frame"))?;
            let place = self.v.local_place(last);
            self.write_fields(place, lay, None, &fvals)?;
        }
        if let Some(va_local) = va_local {
            let va_lay = *self.lays.get(va_local.index()).ok_or(self.malformed("a C-variadic local past the frame"))?;
            let va_l = *self.layouts.get(va_lay);
            if va_l.size != 8 || va_l.scalar.map(|s| s.prim) != Some(Prim::Ptr) {
                return Err(self.unsupported(
                    "a C-variadic definition on a target whose `VaList` is not a single pointer (Linux AAPCS64 register save area)",
                ));
            }
            let va_sig = self.b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::PTR)]);
            let va_intr = self.b.intrinsic("blaze.va_start", va_sig);
            let va_ptr = self.b.target_intrinsic(va_intr, &[]);
            self.put(va_local, V::One(va_ptr), va_lay)?;
        }
        // The body's blocks; a cleanup block only when panics unwind.
        for bi in 0..self.v.blocks() {
            let row = self.v.block(BodyBlock(bi));
            let block = match (row.kind, self.panic) {
                (BodyKind::Cleanup, Panic::Abort) => None,
                (BodyKind::Cleanup, Panic::Unwind) => Some(self.b.create_block(BlockKind::Cold)),
                (BodyKind::Normal, _) => Some(self.b.create_block(BlockKind::Normal)),
            };
            self.blocks.push(block);
        }
        let first = self.v.block_of(self.v.entry());
        let to = self.lir_block(first)?;
        let args = self.args_for(first)?;
        self.b.jump((to, &args));
        Ok(())
    }

    /// Writes value `x` of layout `lay` as the whole of local `l`, held either way.
    pub(crate) fn put(&mut self, l: Local, x: V, lay: LayoutId) -> Result<(), BuildError> {
        let i = self.ix(l)?;
        let held = *self.held.get(i).ok_or(self.malformed("a local past the frame"))?;
        match held {
            Held::None => Ok(()),
            Held::One(_) => {
                let x = match x {
                    V::Mem { ptr, off, lay } => self.load_at(ptr, off, lay)?,
                    x => x,
                };
                match x {
                    V::One(v) => {
                        let err = self.malformed("a local past the frame");
                        *self.cur.get_mut(i).ok_or(err)? = [Some(v), None];
                        Ok(())
                    }
                    _ => Err(self.malformed("a value of another shape written to a one-value local")),
                }
            }
            Held::Two(..) => {
                let x = match x {
                    V::Mem { ptr, off, lay } => self.load_at(ptr, off, lay)?,
                    x => x,
                };
                match x {
                    V::Two(a, b) => {
                        let err = self.malformed("a local past the frame");
                        *self.cur.get_mut(i).ok_or(err)? = [Some(a), Some(b)];
                        Ok(())
                    }
                    _ => Err(self.malformed("a value of another shape written to a two-value local")),
                }
            }
            Held::Slot(_) | Held::At(_) | Held::AtOff(..) => {
                let (ptr, off) = self.base_at(l)?;
                self.store_v(ptr, off, lay, x)
            }
        }
    }

    /// Base pointer and constant byte offset of memory-held local `l`.
    pub(crate) fn base_at(&mut self, l: Local) -> Result<(Value, i32), BuildError> {
        let i = self.ix(l)?;
        match *self.held.get(i).ok_or(self.malformed("a local past the frame"))? {
            Held::Slot(slot) => Ok((self.b.slot_addr(slot), 0)),
            Held::At(ptr) => Ok((ptr, 0)),
            Held::AtOff(ptr, off) => Ok((ptr, off)),
            Held::None | Held::One(_) | Held::Two(..) => Err(self.malformed("the address of a value local")),
        }
    }

    /// The address of memory-held local `l`.
    pub(crate) fn base(&mut self, l: Local) -> Result<Value, BuildError> {
        let (ptr, off) = self.base_at(l)?;
        Ok(self.addr(ptr, off))
    }

    /// The values every value local holds now, in local order: what an edge passes. One not
    /// written yet passes `uninit`.
    pub(crate) fn local_args(&mut self) -> Result<Vec<Value, A>, BuildError> {
        let mut out = Vec::with_capacity_in(self.held.len(), self.alloc);
        for (&held, &[c0, c1]) in self.held.iter().zip(self.cur.iter()) {
            match held {
                Held::One(t) => {
                    let x = match c0 {
                        Some(x) => x,
                        None => self.b.uninit(t),
                    };
                    out.push(x);
                }
                Held::Two(t, u) => {
                    for (cur, ty) in [(c0, t), (c1, u)] {
                        let x = match cur {
                            Some(x) => x,
                            None => self.b.uninit(ty),
                        };
                        out.push(x);
                    }
                }
                Held::None | Held::Slot(_) | Held::At(_) | Held::AtOff(..) => {}
            }
        }
        Ok(out)
    }

    /// What an edge to body block `to` passes: the exception first when it is a cleanup block,
    /// then [`Cx::local_args`].
    pub(crate) fn args_for(&mut self, to: BodyBlock) -> Result<Vec<Value, A>, BuildError> {
        let cleanup = self.v.block(to).kind == BodyKind::Cleanup;
        let mut out = Vec::with_capacity_in(self.held.len() + 1, self.alloc);
        if cleanup {
            out.push(self.exn.ok_or(self.malformed("an edge to a cleanup block from outside unwinding"))?);
        }
        out.extend(self.local_args()?.iter().copied());
        Ok(out)
    }

    /// Begins body block `bi`: the exception parameter of a cleanup block, then one parameter
    /// per value local's value, which become the locals' values.
    pub(crate) fn begin_body_block(&mut self, bi: BodyBlock) -> Result<(), BuildError> {
        let block = self.lir_block(bi)?;
        self.b.begin(block);
        self.exn = match self.v.block(bi).kind {
            BodyKind::Cleanup => Some(self.b.param(Ty::PTR)),
            BodyKind::Normal => None,
        };
        for (&held, cur) in self.held.iter().zip(self.cur.iter_mut()) {
            *cur = match held {
                Held::One(t) => [Some(self.b.param(t)), None],
                Held::Two(t, u) => {
                    let a = self.b.param(t);
                    [Some(a), Some(self.b.param(u))]
                }
                Held::None | Held::Slot(_) | Held::At(_) | Held::AtOff(..) => [None, None],
            };
        }
        Ok(())
    }

    /// Builds every body block, its rows in order.
    pub(crate) fn blocks(&mut self) -> Result<(), BuildError> {
        for bi in 0..self.v.blocks() {
            let bi = BodyBlock(bi);
            if self.blocks.get(bi.index()).copied().flatten().is_none() {
                continue;
            }
            self.begin_body_block(bi)?;
            let row = self.v.block(bi);
            for s in row.stmts.range() {
                let s = blaze_body::Stmt(s as u32);
                self.stmt = Some(s);
                self.row_of(self.v.stmt(s).kind)?;
            }
            self.stmt = None;
            if !self.b.is_ended() {
                return Err(self.malformed("a block whose last row is not a terminator"));
            }
        }
        Ok(())
    }

    /// One row: a statement here, a terminator in term.rs.
    fn row_of(&mut self, kind: StmtKind) -> Result<(), BuildError> {
        match kind {
            StmtKind::Assign { dest, value } => self.assign(dest, value),
            StmtKind::StorageLive(l) => {
                let i = self.ix(l)?;
                if let Some(&Held::Slot(slot)) = self.held.get(i) {
                    self.b.slot_live(slot);
                }
                Ok(())
            }
            StmtKind::StorageDead(l) => {
                let i = self.ix(l)?;
                match *self.held.get(i).ok_or(self.malformed("a local past the frame"))? {
                    Held::Slot(slot) => self.b.slot_dead(slot),
                    Held::One(_) | Held::Two(..) => {
                        let err = self.malformed("a local past the frame");
                        *self.cur.get_mut(i).ok_or(err)? = [None, None];
                    }
                    Held::None | Held::At(_) | Held::AtOff(..) => {}
                }
                Ok(())
            }
            StmtKind::SetDiscriminant { place, variant } => self.set_discriminant(place, variant),
            // The place's bytes become uninitialized: nothing a run does (its next write
            // initializes them).
            StmtKind::Deinit(_) => Ok(()),
            // The borrow checker's rows: they say nothing a run does.
            StmtKind::Activate(_) | StmtKind::FakeRead(_) => Ok(()),
            terminator => self.terminator(terminator),
        }
    }

    /// A scalar LIR value of a one-value `x`, read from memory when it is there.
    pub(crate) fn scalar(&mut self, x: V) -> Result<Value, BuildError> {
        match x {
            V::One(v) => Ok(v),
            V::Mem { ptr, off, lay } => match self.load_at(ptr, off, lay)? {
                V::One(v) => Ok(v),
                _ => Err(self.malformed("a scalar operand of an aggregate type")),
            },
            V::None | V::Two(..) => Err(self.malformed("a scalar operand of a value that is not one scalar")),
        }
    }

    /// `ptr + off` as one pointer.
    pub(crate) fn addr(&mut self, ptr: Value, off: i32) -> Value {
        if off == 0 {
            return ptr;
        }
        let by = self.b.iconst(Ty::I64, off as i64 as u64 as u128);
        self.b.ptr_add(Flags::NONE, ptr, by)
    }
}
