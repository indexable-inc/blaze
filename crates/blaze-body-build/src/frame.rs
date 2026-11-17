//! The frame: which locals are values and which are memory, the function's signature, its entry
//! block and the parameters of every body block (lib.rs, "The frame" and "Block parameters").

use core::alloc::Allocator;

use blaze_body::{Block as BodyBlock, BlockKind as BodyKind, Local, LocalKind, Place, Proj, StmtKind, Value as BodyValue};
use blaze_build::drop_facts::needs_drop;
use blaze_build::intrinsics::lir;
use blaze_build::layout::{LayoutId, Prim, Tail, Variants};
use blaze_build::{Panic, abi};
use blaze_lir::builder::Variadic;
use blaze_lir::func::{BlockKind, Conv, Ext, SlotKind};
use blaze_lir::{Flags, Ty, Value};
use blaze_typeck::{LangItem, PNode, PTy};

use crate::{BuildError, Cx, Held, Pass, V};

impl<A: Allocator + Clone> Cx<'_, '_, '_, A> {
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
        let mut taken = Vec::new_in(self.alloc.clone());
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

    /// How a value of ground type `g` crosses a call of the Rust ABI: the walk's plan (its
    /// `repr` then `rust_pass`), read from the ground type and the one layout, so a function
    /// this crate builds and one the walk builds call each other. Nothing for `()`, `!`, a fn
    /// item and a zero-sized aggregate that needs no drop; a struct, tuple, array, closure or
    /// wide pointer by the address of the caller's copy (whatever its layout's scalars); every
    /// other scalar in one register, a `bool` and an integer under 32 bits widened by its sign.
    pub(crate) fn pass_of(&mut self, g: PTy) -> Result<Pass, BuildError> {
        let scalar = |ty: Ty, ext: Ext| Pass::Scalar { ty, ext, agg: false };
        // A pattern type is held as its base, as the walk holds it.
        let g = match self.node(g)? {
            PNode::Pat(base, _) => base,
            _ => g,
        };
        Ok(match self.node(g)? {
            PNode::Never | PNode::FnDef(..) => Pass::Ignore,
            PNode::Tuple(l) if l.len == 0 => Pass::Ignore,
            PNode::Adt(..) | PNode::Tuple(_) | PNode::Array(..) | PNode::Closure { .. } | PNode::Coroutine { .. } => {
                let lay = self.layout(g)?;
                let l = *self.layouts.get(lay);
                if l.tail != Tail::Sized {
                    return Err(self.unsupported("a value of an unsized type passed by value"));
                }
                if l.size == 0 && !self.needs_drop_of(g)? { Pass::Ignore } else { Pass::Ref }
            }
            PNode::Ref(..) | PNode::Ptr(..) => {
                let lay = self.layout(g)?;
                if self.layouts.get(lay).pair.is_some() { Pass::Ref } else { scalar(Ty::PTR, Ext::None) }
            }
            PNode::FnPtr(..) => scalar(Ty::PTR, Ext::None),
            PNode::Bool => scalar(Ty::I8, Ext::Zext),
            PNode::Int(_) | PNode::Uint(_) | PNode::Char | PNode::Float(_) => {
                let lay = self.layout(g)?;
                let s = self.layouts.get(lay).scalar.ok_or(self.malformed("a primitive whose layout is not one scalar"))?;
                let (ty, signed) = lir(s.prim);
                let ext = match s.prim {
                    Prim::Int { .. } if ty.bits(crate::PTR_BITS) < 32 => {
                        if signed {
                            Ext::Sext
                        } else {
                            Ext::Zext
                        }
                    }
                    _ => Ext::None,
                };
                scalar(ty, ext)
            }
            _ => return Err(self.malformed("a type with no value of its own passed across a call")),
        })
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
        let mut args: Vec<Option<Local>, A> = Vec::new_in(self.alloc.clone());
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
        self.ret = self.pass_of(self.tys[ret_local.index()])?;
        let mut passes: Vec<Pass, A> = Vec::with_capacity_in(args.len(), self.alloc.clone());
        for a in args.iter() {
            let a = a.ok_or(self.malformed("an argument without its local"))?;
            passes.push(self.pass_of(self.tys[a.index()])?);
        }
        let (params, rets) = abi::sig_of_in(&passes, self.ret, self.alloc.clone());
        let sig = self.b.sig(Conv::C, Variadic::No, &params, &rets);
        self.b.set_signature(sig);
        let entry = self.b.create_block(BlockKind::Normal);
        self.b.begin(entry);
        let mut values: Vec<Value, A> = Vec::with_capacity_in(params.len(), self.alloc.clone());
        for p in params.iter() {
            values.push(self.b.param(p.ty));
        }
        // How each local is held: memory when its address is taken or its layout is not one or
        // two scalars; a parameter in memory at its caller's copy, the result at its pointer.
        let mut decided: Vec<Option<Held>, A> = Vec::new_in(self.alloc.clone());
        decided.resize(locals, None);
        let mut memory: Vec<bool, A> = Vec::with_capacity_in(locals, self.alloc.clone());
        for i in 0..locals {
            memory.push(taken[i] || self.value_held(self.lays[i])?.is_none());
        }
        let mut next = values.iter().copied();
        if self.ret == Pass::Ref {
            let hidden = next.next().ok_or(self.malformed("a signature without its result pointer"))?;
            decided[ret_local.index()] = Some(Held::At(hidden));
        }
        let mut given: Vec<(Local, Pass, Value), A> = Vec::with_capacity_in(args.len(), self.alloc.clone());
        for (n, a) in args.iter().enumerate() {
            let a = a.ok_or(self.malformed("an argument without its local"))?;
            match passes[n] {
                Pass::Ignore => {}
                pass @ (Pass::Scalar { .. } | Pass::Ref) => {
                    let x = next.next().ok_or(self.malformed("a parameter without its register"))?;
                    if pass == Pass::Ref && memory[a.index()] {
                        decided[a.index()] = Some(Held::At(x));
                    } else {
                        given.push((a, pass, x));
                    }
                }
                Pass::Memory { .. } | Pass::Words { .. } => return Err(self.malformed("a C pass of a value of the Rust ABI")),
            }
        }
        self.cur.resize(locals, [None, None]);
        for i in 0..locals {
            let held = match decided[i] {
                Some(held) => held,
                None if memory[i] => {
                    let kind = match self.v.local(Local(i as u32)).kind {
                        LocalKind::Binding(_) => SlotKind::Named,
                        _ => SlotKind::Temp,
                    };
                    Held::Slot(self.new_slot(self.lays[i], kind)?)
                }
                None => self.value_held(self.lays[i])?.ok_or(self.malformed("a value local of no value hold"))?,
            };
            self.held.push(held);
        }
        // The parameters' values put where their locals are.
        for &(a, pass, x) in given.iter() {
            let lay = self.lays[a.index()];
            let value = match pass {
                Pass::Ref => self.load_at(x, 0, lay)?,
                _ => V::One(x),
            };
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
        match self.held[i] {
            Held::None => Ok(()),
            Held::One(_) => {
                let x = match x {
                    V::Mem { ptr, off, lay } => self.load_at(ptr, off, lay)?,
                    x => x,
                };
                match x {
                    V::One(v) => {
                        self.cur[i] = [Some(v), None];
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
                        self.cur[i] = [Some(a), Some(b)];
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
        match self.held[self.ix(l)?] {
            Held::Slot(slot) => Ok(self.b.slot_addr(slot)),
            Held::At(ptr) => Ok(ptr),
            Held::None | Held::One(_) | Held::Two(..) => Err(self.malformed("the address of a value local")),
        }
    }

    /// The values every value local holds now, in local order: what an edge passes. One not
    /// written yet passes `uninit`.
    pub(crate) fn local_args(&mut self) -> Result<Vec<Value, A>, BuildError> {
        let mut out = Vec::with_capacity_in(self.held.len(), self.alloc.clone());
        for i in 0..self.held.len() {
            match self.held[i] {
                Held::One(t) => {
                    let x = match self.cur[i][0] {
                        Some(x) => x,
                        None => self.b.uninit(t),
                    };
                    out.push(x);
                }
                Held::Two(t, u) => {
                    for (k, ty) in [t, u].into_iter().enumerate() {
                        let x = match self.cur[i][k] {
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
        let mut out = Vec::with_capacity_in(self.held.len() + 1, self.alloc.clone());
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
        for i in 0..self.held.len() {
            self.cur[i] = match self.held[i] {
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
            if self.blocks[bi.index()].is_none() {
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
                if let Held::Slot(slot) = self.held[self.ix(l)?] {
                    self.b.slot_live(slot);
                }
                Ok(())
            }
            StmtKind::StorageDead(l) => {
                let i = self.ix(l)?;
                match self.held[i] {
                    Held::Slot(slot) => self.b.slot_dead(slot),
                    Held::One(_) | Held::Two(..) => self.cur[i] = [None, None],
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
