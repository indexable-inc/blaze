//! The frame: which locals are values and which are memory, the function's signature, its entry
//! block and the parameters of every body block (lib.rs, "The frame" and "Block parameters").

use blaze_own::Vec;
use core::alloc::Allocator;

use blaze_body::{Block as BodyBlock, BlockKind as BodyKind, Local, LocalKind, Place, Proj, StmtKind, Value as BodyValue};
use blaze_build::drop_facts::needs_drop;
use blaze_build::layout::{LayoutError, LayoutId, Variants};
use blaze_build::{Panic, abi};
use blaze_lir::builder::Variadic;
use blaze_lir::func::{BlockKind, Conv, SlotKind};
use blaze_lir::{Flags, Ty, Value};
use blaze_typeck::{LangItem, PTy};

use crate::{BuildError, Cx, Held, Pass, V};

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

    /// The address-taken locals, by one pass over the rows: a borrow or raw borrow of a place
    /// of the local's own bytes, a projection written through, a drop.
    fn address_taken(&self) -> Result<Vec<bool, A>, BuildError> {
        let mut taken = Vec::new_in(self.alloc);
        taken.resize(self.v.locals() as usize, false);
        for s in 0..self.v.stmts() {
            match self.v.stmt(blaze_body::Stmt(s)).kind {
                StmtKind::Assign { dest, value } => {
                    self.mark(dest, false, &mut taken)?;
                    if let BodyValue::Borrow { place, .. } | BodyValue::RawAddr { place, .. } = value {
                        self.mark(place, true, &mut taken)?;
                    }
                }
                StmtKind::Call { dest: Some(dest), .. } => self.mark(dest, false, &mut taken)?,
                StmtKind::Drop { place, .. } => self.mark(place, true, &mut taken)?,
                // The tag is written in memory, by the layout's one encoding.
                StmtKind::SetDiscriminant { place, .. } => self.mark(place, true, &mut taken)?,
                _ => {}
            }
        }
        Ok(taken)
    }

    /// How a value of layout `lay` is held when its address is never taken.
    fn value_held(&self, lay: LayoutId) -> Result<Option<Held>, BuildError> {
        let l = *self.layouts.get(lay);
        if l.tail != blaze_build::layout::Tail::Sized {
            return Err(self.unsupported("a local of an unsized type"));
        }
        if let Variants::Multiple { .. } = l.variants {
            return Ok(None);
        }
        if l.is_zst() {
            return Ok(Some(Held::None));
        }
        if let Some(s) = l.scalar {
            return Ok(Some(Held::One(blaze_build::intrinsics::lir(s.prim).0)));
        }
        if let Some((a, b)) = l.pair {
            return Ok(Some(Held::Two(blaze_build::intrinsics::lir(a.prim).0, blaze_build::intrinsics::lir(b.prim).0)));
        }
        Ok(None)
    }

    /// A stack slot for a value of layout `lay`.
    pub(crate) fn new_slot(&mut self, lay: LayoutId, kind: SlotKind) -> Result<blaze_lir::func::SlotId, BuildError> {
        let l = *self.layouts.get(lay);
        let size = u32::try_from(l.size).map_err(|_| self.unsupported("a local larger than 4 GiB"))?;
        Ok(self.b.slot(size, l.align, kind))
    }

    /// Use the same layout decision for a parameter, an argument and a return place.
    pub(crate) fn pass_of(&mut self, g: PTy) -> Result<Pass, BuildError> {
        let id = self.layout(g)?;
        let layout = *self.layouts.get(id);
        layout.rust_pass(self.subst.ground.view())
            .map_err(|why| BuildError::Layout { stmt: self.stmt, error: LayoutError { ty: g, why } })
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
    pub(crate) fn frame(&mut self) -> Result<(), BuildError> {
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
        // The signature: a result pointer first when the result is passed by reference.
        let ret_ty = *self.tys.get(ret_local.index()).ok_or(self.malformed("a return local past the frame"))?;
        self.ret = self.pass_of(ret_ty)?;
        let mut passes: Vec<Pass, A> = Vec::with_capacity_in(args.len(), self.alloc);
        for a in args.iter() {
            let a = a.ok_or(self.malformed("an argument without its local"))?;
            let a_ty = *self.tys.get(a.index()).ok_or(self.malformed("an argument local past the frame"))?;
            passes.push(self.pass_of(a_ty)?);
        }
        let (params, rets) = abi::sig_of_in(&passes, self.ret, self.alloc);
        let sig = self.b.sig(Conv::C, Variadic::No, &params, &rets);
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
        for (&is_taken, &lay) in taken.iter().zip(self.lays.iter()) {
            memory.push(is_taken || self.value_held(lay)?.is_none());
        }
        let mut next = values.iter().copied();
        if self.ret == Pass::Ref {
            let hidden = next.next().ok_or(self.malformed("a signature without its result pointer"))?;
            *decided.get_mut(ret_local.index()).ok_or(self.malformed("a return local past the frame"))? = Some(Held::At(hidden));
        }
        let mut given = Vec::with_capacity_in(args.len(), self.alloc);
        for (a, &pass) in args.iter().zip(passes.iter()) {
            let a = a.ok_or(self.malformed("an argument without its local"))?;
            match pass {
                Pass::Ignore => {}
                pass @ (Pass::Scalar { .. } | Pass::Ref) => {
                    let x = next.next().ok_or(self.malformed("a parameter without its register"))?;
                    let is_mem = *memory.get(a.index()).ok_or(self.malformed("an argument local past the frame"))?;
                    if pass == Pass::Ref && is_mem {
                        *decided.get_mut(a.index()).ok_or(self.malformed("an argument local past the frame"))? = Some(Held::At(x));
                    } else if pass == Pass::Ref {
                        let lay = *self.lays.get(a.index()).ok_or(self.malformed("an argument local past the frame"))?;
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
                Pass::Memory { .. } | Pass::Words { .. } => return Err(self.malformed("a C pass of a value of the Rust ABI")),
            }
        }
        self.cur.resize(locals, [None, None]);
        for i in 0..locals {
            let dec = *decided.get(i).ok_or(self.malformed("a local past the frame"))?;
            let is_mem = *memory.get(i).ok_or(self.malformed("a local past the frame"))?;
            let lay = *self.lays.get(i).ok_or(self.malformed("a local past the frame"))?;
            let held = match dec {
                Some(held) => held,
                None if is_mem => {
                    let kind = match self.v.local(Local(i as u32)).kind {
                        LocalKind::Binding(_) => SlotKind::Named,
                        _ => SlotKind::Temp,
                    };
                    Held::Slot(self.new_slot(lay, kind)?)
                }
                None => self.value_held(lay)?.ok_or(self.malformed("a value local of no value hold"))?,
            };
            self.held.push(held);
        }
        // The parameters' values put where their locals are.
        for &(a, value) in given.iter() {
            let lay = *self.lays.get(a.index()).ok_or(self.malformed("an argument local past the frame"))?;
            self.put(a, value, lay)?;
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
            Held::Slot(_) | Held::At(_) => {
                let base = self.base(l)?;
                self.store_v(base, 0, lay, x)
            }
        }
    }

    /// The address of memory-held local `l`.
    pub(crate) fn base(&mut self, l: Local) -> Result<Value, BuildError> {
        let i = self.ix(l)?;
        match *self.held.get(i).ok_or(self.malformed("a local past the frame"))? {
            Held::Slot(slot) => Ok(self.b.slot_addr(slot)),
            Held::At(ptr) => Ok(ptr),
            Held::None | Held::One(_) | Held::Two(..) => Err(self.malformed("the address of a value local")),
        }
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
                Held::None | Held::Slot(_) | Held::At(_) => {}
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
                Held::None | Held::Slot(_) | Held::At(_) => [None, None],
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
                    Held::None | Held::At(_) => {}
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
