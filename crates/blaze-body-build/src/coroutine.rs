//! Coroutine LIR lowering over Typed MIR (`blaze-body`): builds `BuiltCoroutine` (`poll` entry,
//! split `resume`, split `drop`, `StateShape`, `StateNiche`) by emitting the unsplit coroutine
//! body directly from a verified, drop-elaborated [`Verified`] body and delegating state-machine
//! splitting to [`blaze_lir::coroutine`].
//!
//! Layout and ABI match `blaze-build/src/coroutine.rs` 1-to-1:
//! - `resume` (`(AsyncItems::resume, [coroutine])`): parameters `(result: ptr, state: ptr, cx: ptr)`,
//!   upvars addressed inside `[0..prefix_size)` of `state`, all non-ZST locals held in stack slots
//!   (so normal blocks have zero block parameters and `blaze_lir::coroutine::split` places live
//!   locals into the state and overlaps non-conflicting slots), `StmtKind::Suspend` writes
//!   `Poll::Pending` and suspends, `StmtKind::Return` writes `Poll::Ready(_0)` and returns, and
//!   `StmtKind::Cancelled` emits `Op::Cancelled`.
//! - `poll` (`(Adts::future_poll, [coroutine])`): extracts the state pointer from `Pin<&mut Self>`,
//!   calls `resume`, and returns `Poll<Output>` by its Rust ABI pass.
//! - `drop` (`(drop_in_place, [coroutine])`): the pass's tag-switching drop function (dropping
//!   upvars on `UNRESUMED`, active locals per suspend point via each cancel path, and returning
//!   immediately on `RETURNED` / `PANICKED`).

use crate::AllocVec;
use blaze_own::Vec;
use core::alloc::Allocator;

use blaze_body::{Block as BodyBlock, BlockKind as BodyKind, Local, LocalKind, Verified};
use blaze_build::coroutine_state::LaidState;
use blaze_build::layout::{LayoutCx, LayoutId, Layouts, Variants};
use blaze_build::{
    Abi, Adts, AsyncItems, Built, BuiltCoroutine, CoroutineOf, Instance, Panic, StateShape, Subst, abi,
    coroutine_identity, coroutine_of, ground, niche_of,
};
use blaze_lir::builder::{MemAttrs, Param, Variadic};
use blaze_lir::coroutine::{Panics, Region, Shape, SlotName};
use blaze_lir::func::{BlockKind, Conv, Ext, RefId, RefKind, SlotKind};
use blaze_lir::{Block, Builder, Flags, Ty, Value, VerifiedFunc};
use blaze_typeck::{ItemKey, List, Mutbl, PNode, PTy, PTys};

use crate::{BuildError, Cx, Held, PTR_BITS, Pass, Track};

/// Optional pre-laid coroutine state region and per-local saved offsets for [`build_coroutine`]
/// (`blaze_lir::coroutine::split_in`).
#[derive(Clone, Copy)]
pub struct CoroutinePlan<'a> {
    /// Pre-laid state size, alignment, saved region bounds, and tag niche.
    pub laid: LaidState,
    /// `(local, offset)` pairs inside `laid.region` for locals saved across suspension points.
    pub locals: &'a [(Local, u32)],
}

/// Coroutine-body build state (`Some` only while building a coroutine's `resume` body).
#[derive(Clone, Copy)]
pub(crate) struct Co {
    /// Entry parameter 1 (`Shape::state_param == 1`): pointer to the pinned coroutine state.
    pub(crate) state: Value,
    /// Entry parameter 2: current poll's `&mut Context<'_>`, appended to 1-arg `Future::poll` calls.
    pub(crate) cx: Value,
    /// Monotonic suspend point index in emission order.
    pub(crate) points: u32,
    /// Layout of `Poll<Output>`, or `None` when `Poll<Output>` is zero-sized (e.g. `Poll<!>`).
    pub(crate) poll_lay: Option<LayoutId>,
    /// Library async items (`Poll`, `Pin`, `Ready`, `Pending`, `resume`).
    pub(crate) items: AsyncItems,
}

/// Builds coroutine `ordinal` of `owner` from its verified, drop-elaborated body `body` (whose
/// types index `from`) at `subst`'s ground arguments.
///
/// When `plan` is `Some`, saved locals are placed at their type-laid offsets via
/// [`blaze_lir::coroutine::split_in`]; when `plan` is `None`, [`blaze_lir::coroutine::split_with`]
/// computes the state layout directly from the LIR body.
#[expect(clippy::too_many_arguments, reason = "mirrors blaze_build::build_coroutine seam with explicit body and type table")]
pub fn build_coroutine<A: Allocator + Copy>(
    body: Verified<'_>,
    from: PTys<'_>,
    owner: ItemKey,
    ordinal: u32,
    subst: &mut Subst<'_, '_>,
    adts: &dyn Adts,
    plan: Option<CoroutinePlan<'_>>,
    alloc: A,
) -> Result<BuiltCoroutine<A>, BuildError> {
    let items = adts
        .async_items()
        .ok_or(BuildError::Malformed { stmt: None, what: "the world gave no `AsyncItems` for a coroutine build" })?;
    let own_args = subst
        .ground
        .intern_list(subst.args)
        .map_err(|_| BuildError::Malformed { stmt: None, what: "the ground table is full" })?;
    let coro_ty = coroutine_identity(subst.ground, owner, ordinal, own_args)
        .map_err(|e| BuildError::Facts { stmt: None, error: e })?;
    let (env_ty, output_ty) = coroutine_env_output(coro_ty, subst, adts, alloc)?;

    let (func, shape, panics, slot_names, refs, statics, thread_locals, vtables) = {
        let mut cx = Cx {
            v: body,
            from,
            item: owner,
            abi: Abi::Rust,
            track: Track::No,
            subst: &mut *subst,
            adts,
            alloc,
            b: Builder::new_in(PTR_BITS, alloc),
            layouts: Layouts::new_in(alloc),
            panic: adts.panic(),
            co: None,
            tys: Vec::new_in(alloc),
            lays: Vec::new_in(alloc),
            held: Vec::new_in(alloc),
            cur: Vec::new_in(alloc),
            blocks: Vec::new_in(alloc),
            exn: None,
            ret: Pass::Ref,
            sret: None,
            ref_params: Vec::new_in(alloc),
            refs: AllocVec::new_in(alloc),
            ref_ids: Vec::new_in(alloc),
            statics: AllocVec::new_in(alloc),
            static_ids: Vec::new_in(alloc),
            thread_locals: AllocVec::new_in(alloc),
            tls_ids: Vec::new_in(alloc),
            vtables: AllocVec::new_in(alloc),
            vtable_ids: Vec::new_in(alloc),
            stmt: None,
        };
        let (prefix_size, prefix_align, slot_names) = cx.coroutine_frame(env_ty, output_ty, items, plan)?;
        cx.blocks()?;
        let unresumed = cx.coroutine_unresumed_block(env_ty)?;
        let panics = cx.coroutine_panics()?;
        let func = cx.b.finish().map_err(BuildError::Lir)?;
        let shape = Shape { state_param: 1, prefix_size, prefix_align, unresumed };
        (func, shape, panics, slot_names, cx.refs, cx.statics, cx.thread_locals, cx.vtables)
    };

    let split = match plan {
        Some(p) => {
            let l = p.laid;
            let region = Region {
                start: l.region.start,
                end: l.region.end,
                tag_offset: l.shape.tag_offset,
                tag_ty: l.shape.tag_ty,
                size: l.shape.size,
                align: l.shape.align,
            };
            blaze_lir::coroutine::split_in(func.view(), shape, panics, region, &slot_names, alloc)
                .map_err(BuildError::Coroutine)?
        }
        None => blaze_lir::coroutine::split_with(func.view(), shape, panics, alloc)
            .map_err(BuildError::Coroutine)?,
    };

    let (state, niche) = match plan {
        Some(p) => (p.laid.shape, p.laid.niche),
        None => (StateShape::of(&split.layout), niche_of(&split.layout)),
    };

    let poll = build_poll_entry(coro_ty, output_ty, items, subst, adts, alloc)?;

    fn copy_vec<T: Copy, A: Allocator>(slice: &[T], alloc: A) -> AllocVec<T, A> {
        let mut out = AllocVec::with_capacity_in(slice.len(), alloc);
        out.extend_from_slice(slice);
        out
    }

    let resume = Built {
        func: VerifiedFunc::new(split.poll).map_err(|e| BuildError::Lir(blaze_lir::BuildError::Verify(e)))?,
        refs: copy_vec(&refs, alloc),
        statics: copy_vec(&statics, alloc),
        thread_locals: copy_vec(&thread_locals, alloc),
        vtables: copy_vec(&vtables, alloc),
        traps: AllocVec::new_in(alloc),
        copy_insts: 0,
    };
    let drop = Built {
        func: VerifiedFunc::new(split.drop).map_err(|e| BuildError::Lir(blaze_lir::BuildError::Verify(e)))?,
        refs,
        statics,
        thread_locals,
        vtables,
        traps: AllocVec::new_in(alloc),
        copy_insts: 0,
    };

    Ok(BuiltCoroutine { poll, resume, drop, state, niche })
}

/// Resolves `(env_ty, output_ty)` of ground coroutine type `coro_ty` from `LayoutFacts::coroutine`.
fn coroutine_env_output<A: Allocator + Copy>(
    coro_ty: PTy,
    subst: &mut Subst<'_, '_>,
    adts: &dyn Adts,
    alloc: A,
) -> Result<(PTy, PTy), BuildError> {
    let of: CoroutineOf = coroutine_of(subst.ground.view(), coro_ty)
        .ok_or(BuildError::Malformed { stmt: None, what: "a coroutine type `coroutine_of` does not read" })?;
    let facts = adts.facts();
    let c = facts
        .coroutine(of.owner, of.ordinal)
        .ok_or(BuildError::Malformed { stmt: None, what: "a coroutine whose facts the world did not answer (`LayoutFacts::coroutine`)" })?;
    let mut argv: Vec<PTy, A> = Vec::new_in(alloc);
    let args_slice = subst
        .ground
        .view()
        .list(of.owner_args)
        .map_err(|_| BuildError::Malformed { stmt: None, what: "a coroutine's arguments past the ground table" })?;
    argv.extend_from_slice(args_slice);
    let env = ground(c.types, c.env, &argv, subst.ground, subst.scratch, facts)
        .map_err(|error| BuildError::Facts { stmt: None, error })?;
    let output = ground(c.types, c.output, &argv, subst.ground, subst.scratch, facts)
        .map_err(|error| BuildError::Facts { stmt: None, error })?;
    Ok((env, output))
}

/// Builds the `Future::poll` entry shim `(Pin<&mut Self>, &mut Context<'_>) -> Poll<Output>`
/// that extracts the state pointer from `Pin` and forwards `(result, state, cx)` to `RESUME`.
fn build_poll_entry<A: Allocator + Copy>(
    coro_ty: PTy,
    output_ty: PTy,
    items: AsyncItems,
    subst: &mut Subst<'_, '_>,
    adts: &dyn Adts,
    alloc: A,
) -> Result<Built<A>, BuildError> {
    let full = |_| BuildError::Malformed { stmt: None, what: "the ground table is full" };
    let poll_args = subst.ground.intern_list(&[output_ty]).map_err(full)?;
    let poll_ty = subst.ground.intern(PNode::Adt(items.poll, poll_args)).map_err(full)?;
    let none = subst.ground.intern_list(&[]).map_err(full)?;
    let unit = subst.ground.intern(PNode::Tuple(none)).map_err(full)?;
    let mut_unit = subst.ground.intern(PNode::Ref(Mutbl::Mut, unit)).map_err(full)?;
    let pin_args = subst.ground.intern_list(&[mut_unit]).map_err(full)?;
    let pin_ty = subst.ground.intern(PNode::Adt(items.pin, pin_args)).map_err(full)?;

    let mut layouts = Layouts::new_in(alloc);
    let mut layout_of = |ty: PTy| -> Result<LayoutId, BuildError> {
        let mut lcx = LayoutCx { ground: subst.ground, facts: adts.facts(), scratch: subst.scratch };
        layouts.of(ty, &mut lcx).map_err(|error| BuildError::Layout { stmt: None, error })
    };
    let pin_lay = layout_of(pin_ty)?;
    let poll_lay = layout_of(poll_ty)?;
    let pin_off = layouts
        .field_offset(pin_lay, 0)
        .and_then(|o| i32::try_from(o).ok())
        .ok_or(BuildError::Malformed { stmt: None, what: "`Pin` layout without its pointer field" })?;
    let pin_pass = layouts
        .get(pin_lay)
        .rust_pass(subst.ground.view())
        .map_err(|why| BuildError::Layout { stmt: None, error: blaze_build::layout::LayoutError { ty: pin_ty, why } })?;
    let ret_pass = layouts
        .get(poll_lay)
        .rust_pass(subst.ground.view())
        .map_err(|why| BuildError::Layout { stmt: None, error: blaze_build::layout::LayoutError { ty: poll_ty, why } })?;
    let cx_pass = Pass::Scalar { ty: Ty::PTR, ext: Ext::None, agg: false };

    let mut b = Builder::new_in(PTR_BITS, alloc);
    let (params, rets) = abi::sig_of_in(&[pin_pass, cx_pass], ret_pass, alloc);
    let sig = b.sig(Conv::C, Variadic::No, &params, &rets);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let mut vals: Vec<Value, A> = Vec::with_capacity_in(params.len(), alloc);
    for p in params.iter() {
        vals.push(b.param(p.ty));
    }

    let (result, pin, cx) = match (ret_pass, vals.as_slice()) {
        (Pass::Ref, &[result, pin, cx]) => (result, pin, cx),
        (Pass::Ignore | Pass::Scalar { .. } | Pass::Wide { .. }, &[pin, cx]) => {
            let l = *layouts.get(poll_lay);
            let size = u32::try_from(l.size)
                .map_err(|_| BuildError::Malformed { stmt: None, what: "`Poll` layout larger than 4 GiB" })?;
            let slot = b.slot(size, l.align, SlotKind::Temp);
            (b.slot_addr(slot), pin, cx)
        }
        _ => {
            return Err(BuildError::Malformed {
                stmt: None,
                what: "a `Future::poll` entry without its receiver, context or result",
            });
        }
    };

    let state = match pin_pass {
        Pass::Scalar { ty: Ty::PTR, .. } if pin_off == 0 => pin,
        Pass::Ref => b.load(Ty::PTR, Flags::NONE, pin, pin_off, MemAttrs::aligned(3)),
        _ => {
            return Err(BuildError::Malformed {
                stmt: None,
                what: "a `Pin<&mut Self>` receiver without its thin state pointer",
            });
        }
    };

    let resume_args = subst.ground.intern_list(&[coro_ty]).map_err(full)?;
    let mut refs = AllocVec::new_in(alloc);
    refs.push(Instance { item: items.resume, args: resume_args });
    let r = b.reference(RefKind::Func, 0);
    let resume_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR); 3], &[]);
    b.call(Flags::NONE, r, resume_sig, &[result, state, cx]);

    match ret_pass {
        Pass::Ignore | Pass::Ref => b.ret(&[]),
        Pass::Scalar { ty, .. } => {
            let l = *layouts.get(poll_lay);
            let align = l.scalar.map_or(l.align, |s| s.prim.align());
            let v = b.load(ty, Flags::NONE, result, 0, MemAttrs::aligned(align));
            b.ret(&[v]);
        }
        Pass::Wide { metadata } => {
            let l = *layouts.get(poll_lay);
            let (sa, sb) = l.pair.ok_or(BuildError::Malformed {
                stmt: None,
                what: "a wide `Poll` return layout without its scalar pair",
            })?;
            let sec = crate::value::second(&l);
            let a = b.load(Ty::PTR, Flags::NONE, result, 0, MemAttrs::aligned(sa.prim.align()));
            let bval = b.load(metadata, Flags::NONE, result, sec, MemAttrs::aligned(sb.prim.align()));
            b.ret(&[a, bval]);
        }
        Pass::Memory { .. } | Pass::Words { .. } => {
            return Err(BuildError::Malformed {
                stmt: None,
                what: "a C ABI pass returned for Rust `Future::poll`",
            });
        }
    }

    let func = b.finish_verified().map_err(BuildError::Lir)?;
    Ok(Built {
        func,
        refs,
        statics: AllocVec::new_in(alloc),
        thread_locals: AllocVec::new_in(alloc),
        traps: AllocVec::new_in(alloc),
        vtables: AllocVec::new_in(alloc),
        copy_insts: 0,
    })
}

impl<A: Allocator + Copy> Cx<'_, '_, '_, A> {
    /// Sets up a coroutine body's RESUME frame: signature `(result: ptr, state: ptr, cx: ptr)`,
    /// upvars bound at their offsets in `state`, all other non-ZST locals in stack slots (zero
    /// block parameters on normal blocks), and optional `StatePlan` slot names for `split_in`.
    pub(crate) fn coroutine_frame(
        &mut self,
        env_ty: PTy,
        output_ty: PTy,
        items: AsyncItems,
        plan: Option<CoroutinePlan<'_>>,
    ) -> Result<(u32, u8, Vec<SlotName, A>), BuildError> {
        let taken = self.address_taken()?;
        let poll_args = self
            .subst
            .ground
            .intern_list(&[output_ty])
            .map_err(|_| self.malformed("the ground table is full"))?;
        let poll_ty = self
            .subst
            .ground
            .intern(PNode::Adt(items.poll, poll_args))
            .map_err(|_| self.malformed("the ground table is full"))?;
        let poll_lay = self.layout(poll_ty)?;
        let poll_opt = (!self.layouts.get(poll_lay).is_zst()).then_some(poll_lay);

        let env_lay = self.layout(env_ty)?;
        let env_l = *self.layouts.get(env_lay);
        let prefix_size = u32::try_from(env_l.size)
            .map_err(|_| self.malformed("a coroutine's upvars tuple larger than 4 GiB"))?;
        let prefix_align = env_l.align;

        let sig = self.b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR); 3], &[]);
        self.b.set_signature(sig);
        let entry = self.b.create_block(BlockKind::Normal);
        self.b.begin(entry);
        let result = self.b.param(Ty::PTR);
        let state = self.b.param(Ty::PTR);
        let cx = self.b.param(Ty::PTR);
        self.sret = Some(result);
        self.ret = Pass::Ref;
        self.co = Some(Co { state, cx, points: 0, poll_lay: poll_opt, items });

        let locals = usize::try_from(self.v.locals()).map_err(|_| self.malformed("too many locals"))?;
        self.cur.resize(locals, [None, None]);
        let mut slot_names: Vec<SlotName, A> = Vec::new_in(self.alloc);
        let mut used_offsets: Vec<u32, A> = Vec::new_in(self.alloc);

        for i in 0..locals {
            let l = Local(u32::try_from(i).map_err(|_| self.malformed("too many locals"))?);
            let decl = self.v.local(l);
            let g = self.ground(decl.ty)?;
            let lay = self.layout(g)?;
            self.tys.push(g);
            self.lays.push(lay);

            let is_taken = taken.get(i).copied().unwrap_or(false);
            let held = match decl.kind {
                LocalKind::Arg(idx) => {
                    let off = self.field_off(env_lay, idx)?;
                    Held::AtOff(state, off)
                }
                LocalKind::Return => {
                    if self.layouts.get(lay).is_zst() && !is_taken {
                        Held::None
                    } else {
                        Held::Slot(self.new_slot(lay, SlotKind::Temp)?)
                    }
                }
                kind @ (LocalKind::Binding(_) | LocalKind::Temp(_, _) | LocalKind::DropFlag(_)) => {
                    if self.layouts.get(lay).is_zst() && !is_taken {
                        Held::None
                    } else {
                        let sk = if matches!(kind, LocalKind::Binding(_)) {
                            SlotKind::Named
                        } else {
                            SlotKind::Temp
                        };
                        let slot = self.new_slot(lay, sk)?;
                        if let Some(p) = plan
                            && let Some(&(_, offset)) = p.locals.iter().find(|&&(loc, _)| loc == l)
                            && used_offsets.scan_for(&offset).is_none()
                        {
                            used_offsets.push(offset);
                            slot_names.push(SlotName { slot, offset });
                        }
                        Held::Slot(slot)
                    }
                }
            };
            self.held.push(held);
        }

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
        self.b.jump((to, &[]));
        Ok((prefix_size, prefix_align, slot_names))
    }

    /// Writes `Poll::Ready(_0)` into `sret` and returns `[]` for `StmtKind::Return` inside a coroutine.
    pub(crate) fn coroutine_return(&mut self) -> Result<(), BuildError> {
        let co = self.co.ok_or(self.malformed("coroutine return outside a coroutine body"))?;
        if let Some(poll_lay) = co.poll_lay {
            let sret = self.sret.ok_or(self.malformed("coroutine `Poll` returned without its sret pointer"))?;
            let ret_lay = *self.lays.first().ok_or(self.malformed("a body without its return local"))?;
            if !self.layouts.get(ret_lay).is_zst() {
                let vlay = match self.layouts.get(poll_lay).variants {
                    Variants::Multiple { .. } => self
                        .layouts
                        .variant(poll_lay, co.items.ready)
                        .ok_or(self.malformed("`Poll::Ready` variant past `Poll` layout"))?,
                    Variants::Single { .. } => poll_lay,
                };
                if let Some(off) = self.layouts.field_offset(vlay, 0) {
                    let foff = i32::try_from(off).map_err(|_| self.malformed("`Poll::Ready` field offset past i32"))?;
                    let ret_place = self.v.local_place(Local(0));
                    let (val, _) = self.operand(blaze_body::Operand::Move(ret_place))?;
                    self.store_v(sret, foff, ret_lay, val)?;
                }
            }
            self.store_tag(sret, 0, poll_lay, co.items.ready)?;
        }
        self.b.ret(&[]);
        Ok(())
    }

    /// Writes `Poll::Pending` into `sret` and emits `Op::Suspend` for `StmtKind::Suspend`.
    pub(crate) fn coroutine_suspend(&mut self, resume: BodyBlock, cancel: BodyBlock) -> Result<(), BuildError> {
        let mut co = self.co.ok_or(self.unsupported("a suspension outside a coroutine build"))?;
        if let Some(poll_lay) = co.poll_lay {
            let sret = self.sret.ok_or(self.malformed("coroutine `Poll` suspended without its sret pointer"))?;
            self.store_tag(sret, 0, poll_lay, co.items.pending)?;
        }
        let point = co.points;
        co.points = point
            .checked_add(1)
            .ok_or(self.malformed("a coroutine with more suspend points than a `u32` counts"))?;
        self.co = Some(co);
        let r = self.lir_block(resume)?;
        let c = self.lir_block(cancel)?;
        self.b.suspend(point, r, c, &[]);
        Ok(())
    }

    /// Emits `Op::Cancelled` for `StmtKind::Cancelled` at the end of a coroutine cancel path.
    pub(crate) fn coroutine_cancelled(&mut self) -> Result<(), BuildError> {
        if self.co.is_none() {
            return Err(self.unsupported("`cancelled` outside a coroutine build"));
        }
        self.b.cancelled();
        Ok(())
    }

    /// Emits the unreachable `unresumed` drop block (`Shape::unresumed`) that drops the coroutine's
    /// upvars tuple at `(state, 0)` when a never-polled future is dropped, ending in `cancelled`.
    fn coroutine_unresumed_block(&mut self, env_ty: PTy) -> Result<Block, BuildError> {
        let co = self.co.ok_or(self.malformed("`unresumed` block outside a coroutine build"))?;
        let unresumed = self.b.create_block(BlockKind::Normal);
        self.b.begin(unresumed);
        if self.needs_drop_of(env_ty)? {
            let drop_item = self
                .adts
                .lang_fn("drop_in_place")
                .ok_or(BuildError::Malformed { stmt: None, what: "coroutine unresumed drop needs `drop_in_place`" })?;
            let args = self
                .subst
                .ground
                .intern_list(&[env_ty])
                .map_err(|_| self.malformed("the ground table is full"))?;
            let inst = Instance { item: drop_item, args };
            let r: RefId = self.func_ref(inst);
            let sig = self.b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[]);
            self.b.call(Flags::NONE, r, sig, &[co.state]);
        }
        self.b.cancelled();
        Ok(unresumed)
    }

    /// Emits the two finished-state panic blocks (`Panics { returned, panicked }`) for
    /// `blaze_lir::coroutine::split_with` / `split_in` when the library declares those lang items.
    fn coroutine_panics(&mut self) -> Result<Panics, BuildError> {
        let returned = self.finished_panic_block("panic_const_async_fn_resumed")?;
        let panicked = self.finished_panic_block("panic_const_async_fn_resumed_panic")?;
        Ok(Panics { returned, panicked })
    }

    fn finished_panic_block(&mut self, name: &'static str) -> Result<Option<Block>, BuildError> {
        let Some(item) = self.adts.lang_fn(name) else {
            return Ok(None);
        };
        let block = self.b.create_block(BlockKind::Cold);
        self.b.begin(block);
        let inst = Instance { item, args: List::EMPTY };
        let loc = if self.adts.track_caller(inst, self.subst.ground) == Some(true) {
            self.stmt = Some(self.v.entry());
            let loc = self.caller_arg(inst, name)?;
            self.stmt = None;
            loc
        } else {
            None
        };
        let r = self.func_ref(inst);
        let mut ps: Vec<Param, A> = Vec::with_capacity_in(1, self.alloc);
        let mut vs: Vec<Value, A> = Vec::with_capacity_in(1, self.alloc);
        if let Some(loc) = loc {
            ps.push(Param::of(Ty::PTR));
            vs.push(loc);
        }
        let sig = self.b.sig(Conv::C, Variadic::No, &ps, &[]);
        self.b.call(Flags::NONE, r, sig, &vs);
        self.b.unreachable();
        Ok(Some(block))
    }

    /// Builds `AggregateKind::Coroutine` into `dest`: stores upvar operands into the state's
    /// prefix (`env` tuple at offset 0) and writes `UNRESUMED` (0) to `tag_offset`.
    pub(crate) fn coroutine_aggregate(
        &mut self,
        dest: blaze_body::Place,
        ty: PTy,
        operands: blaze_body::Ops,
        dlay: LayoutId,
    ) -> Result<(), BuildError> {
        let g = self.ground(ty)?;
        let of = coroutine_of(self.subst.ground.view(), g)
            .ok_or(self.malformed("a coroutine aggregate whose ground type is not `PNode::Coroutine`"))?;
        let shape = {
            let view = self.subst.ground.view();
            let args = view
                .list(of.owner_args)
                .map_err(|_| self.malformed("a coroutine's arguments past the ground table"))?;
            match self.adts.facts().coroutine_state(of.owner, of.ordinal, args, view) {
                Ok(Some(shape)) => shape,
                Ok(None) => return Err(self.unsupported("a coroutine whose state `LayoutFacts::coroutine_state` did not answer")),
                Err(missing) => return Err(self.facts(blaze_build::BuildError::MissingRecord { item: missing.item })),
            }
        };
        let (ptr, off) = match self.v.place(dest).depth {
            0 => self.base_at(self.v.place(dest).local)?,
            _ => match self.resolve(dest)?.0 {
                crate::value::At::Mem { ptr, off, .. } => (ptr, off),
                crate::value::At::Val(_) => return Err(self.malformed("a coroutine written to a value local")),
            },
        };
        let ops: Vec<blaze_body::Operand, A> = {
            let cap = usize::try_from(operands.len()).unwrap_or(0);
            let mut out = Vec::with_capacity_in(cap, self.alloc);
            out.extend(self.v.ops(operands));
            out
        };
        if !ops.is_empty() {
            let (env_ty, _) = coroutine_env_output(g, self.subst, self.adts, self.alloc)?;
            let env_lay = self.layout(env_ty)?;
            let base_off = self.field_off(dlay, 0)?;
            for (i, &o) in ops.iter().enumerate() {
                let (x, og) = self.operand(o)?;
                let flay = self.layout(og)?;
                let idx = u32::try_from(i).map_err(|_| self.malformed("too many coroutine upvars"))?;
                let foff = self.field_off(env_lay, idx)?;
                self.store_v(ptr, off + base_off + foff, flay, x)?;
            }
        }
        let tag = self.b.iconst(shape.tag_ty, u128::from(blaze_lir::coroutine::UNRESUMED));
        let tag_off = i32::try_from(shape.tag_offset).map_err(|_| self.malformed("a coroutine tag offset past i32"))?;
        self.b.store(Flags::NONE, tag, ptr, off + tag_off, MemAttrs::aligned(0));
        Ok(())
    }
}
