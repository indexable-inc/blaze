//! Synthetic LIR shims for `blaze-body-build`, independent of legacy `blaze-build`'s AST walk:
//!
//! - [`build_drop_glue`]: `core::ptr::drop_in_place::<T>` for sized types (structs, tuples,
//!   closures, enums, arrays, unions, `Box<T>`), slices `[U]` / `str`, `dyn Trait`, and custom
//!   DST structs/tuples ending in an unsized tail, with full `Panic::Unwind` unwind ladders and
//!   `Panic::Abort` direct calls.
//! - [`build_closure_shim`]: thin `FnMut::call_mut` / `FnOnce::call_once` entry over a closure
//!   whose own kind is stronger (`Fn` -> `FnMut`/`FnOnce`, `FnMut` -> `FnOnce`), dropping the
//!   closure value on normal return and unwind for `FnOnce`.
//! - [`build_fn_shim`]: `Fn::call` / `FnMut::call_mut` / `FnOnce::call_once` shim for a fn item
//!   (`FnDef`) or fn pointer (`FnPtr`), bridging Rust ABI and C ABI plus `#[track_caller]`.
//! - [`build_ctor_fn`]: constructor function for a tuple struct or tuple enum variant.
//! - [`build_reify_shim`]: plain fn-pointer entry point (`ReifyShim`) for a `#[track_caller]` fn.
//! - [`build_vtable_shim`]: by-value `self` `VTableShim` taking `(data_ptr, args..)` from a
//!   dynamic call (e.g. `Box<dyn FnOnce>`).
//! - [`build_builtin_clone`]: language-provided `Clone::clone` for tuples, records, closures,
//!   structs, structural unions, fn items, and fn pointers.
//! - [`build_builtin_from`]: language-provided `From::from` for structural error unions.
//! - [`build_test_report_shim`]: `#[test]` `Termination::report` C `main` wrapper.

use crate::AllocVec;
use blaze_own::Vec;
use core::alloc::Allocator;

use blaze_build::cabi::{self, CAbiError, Class};
use blaze_build::drop_facts::needs_drop;
use blaze_build::intrinsics::{Call, IntrinsicCx, Out, Site, Which, lir, read_discriminant, resize_in};
use blaze_build::layout::{DynChain, Fields, LayoutCx, LayoutError, LayoutId, Layouts, Tail, Variants, mask};
use blaze_build::vtable::VtableRef;
use blaze_build::{Abi, Adts, Built, CallerSite, FnTrait, Instance, Panic, Subst, abi, ground};
use blaze_lir::builder::{MemAttrs, Param, Variadic};
use blaze_lir::func::{BlockKind, Conv, Ext, RefId, RefKind, SigId, SlotId, SlotKind};
use blaze_lir::{Builder, Flags, Full, IntCond, Op, Ty, Value};
use blaze_typeck::{AdtKind, FnSig, ItemKey, LangItem, List, Mutbl, PNode, PTy};

use crate::term::pointer_abi;
use crate::value::second;
use crate::{BuildError, PTR_BITS, Pass, V};

/// What a shim call targets.
#[derive(Clone, Copy)]
enum ShimTarget {
    Direct(RefId),
    Pointer(Value),
}

/// One step in a drop-glue unwind ladder.
#[derive(Clone, Copy)]
enum DropStep {
    /// Call `<T as Drop>::drop(p)` (thin pointer when `meta` is `None`, wide when `Some`).
    DropImpl { instance: Instance, p: Value, meta: Option<(Value, Ty)>, may_unwind: bool },
    /// Call `drop_in_place::<ty>` on `p + off` (sized).
    DropSized { ty: PTy, p: Value, off: i32 },
    /// Call `drop_in_place::<ty>` on wide pointer `(data, meta)` (unsized).
    DropWide { ty: PTy, data: Value, meta: Value, meta_ty: Ty },
    /// Drop the contents of `Box<T>` at `p` (`list` is `Box`'s generic args `[T, A]`).
    BoxContents { list: List, p: Value },
    /// Drop the fields/variants/elements of `ty` (of layout `lay`) at `p`.
    Parts { ty: PTy, lay: LayoutId, p: Value },
}

/// State for building one synthetic LIR function without a body.
struct ShimCx<'a, 'g, 't, A: Allocator + Copy> {
    subst: &'a mut Subst<'g, 't>,
    adts: &'a dyn Adts,
    alloc: A,
    b: Builder<A, Full>,
    layouts: Layouts<A>,
    panic: Panic,
    refs: AllocVec<Instance, A>,
    ref_ids: Vec<RefId, A>,
    statics: AllocVec<ItemKey, A>,
    thread_locals: AllocVec<ItemKey, A>,
    vtables: AllocVec<VtableRef, A>,
}

impl<'a, 'g, 't, A: Allocator + Copy> ShimCx<'a, 'g, 't, A> {
    fn new(subst: &'a mut Subst<'g, 't>, adts: &'a dyn Adts, alloc: A) -> Self {
        Self {
            subst,
            adts,
            alloc,
            b: Builder::new_in(PTR_BITS, alloc),
            layouts: Layouts::new_in(alloc),
            panic: adts.panic(),
            refs: AllocVec::new_in(alloc),
            ref_ids: Vec::new_in(alloc),
            statics: AllocVec::new_in(alloc),
            thread_locals: AllocVec::new_in(alloc),
            vtables: AllocVec::new_in(alloc),
        }
    }

    fn finish(self) -> Result<Built<A>, BuildError> {
        let func = self.b.finish_verified().map_err(BuildError::Lir)?;
        Ok(Built {
            func,
            refs: self.refs,
            statics: self.statics,
            thread_locals: self.thread_locals,
            traps: AllocVec::new_in(self.alloc),
            vtables: self.vtables,
            copy_insts: 0,
        })
    }

    fn malformed(&self, what: &'static str) -> BuildError {
        BuildError::Malformed { stmt: None, what }
    }

    fn node(&self, g: PTy) -> Result<PNode, BuildError> {
        self.subst.ground.view().node(g).map_err(|_| self.malformed("a ground type past its table"))
    }

    fn layout(&mut self, g: PTy) -> Result<LayoutId, BuildError> {
        let mut cx = LayoutCx { ground: self.subst.ground, facts: self.adts.facts(), scratch: self.subst.scratch };
        self.layouts.of(g, &mut cx).map_err(|error| BuildError::Layout { stmt: None, error })
    }

    fn needs_drop_of(&mut self, g: PTy) -> Result<bool, BuildError> {
        let manually_drop = self.adts.lang_adt(LangItem::ManuallyDrop);
        let adts = self.adts;
        needs_drop(g, self.subst.ground, adts.facts(), manually_drop, &mut |t, ground| adts.drop_impl(t, ground), self.subst.scratch)
            .map_err(|error| BuildError::NeedsDrop { stmt: None, error })
    }

    fn pass_of(&mut self, g: PTy) -> Result<Pass, BuildError> {
        let id = self.layout(g)?;
        let layout = *self.layouts.get(id);
        layout
            .rust_pass(self.subst.ground.view())
            .map_err(|why| BuildError::Layout { stmt: None, error: LayoutError { ty: g, why } })
    }

    fn pass_of_abi(&mut self, g: PTy, abi: Abi) -> Result<Pass, BuildError> {
        if matches!(abi, Abi::Rust | Abi::RustCall) {
            return self.pass_of(g);
        }
        let refuse = |why| BuildError::CAbi { stmt: None, why };
        let id = self.layout(g)?;
        let class = {
            let mut cx = LayoutCx { ground: &mut *self.subst.ground, facts: self.adts.facts(), scratch: self.subst.scratch };
            cabi::class_of(&mut self.layouts, g, &mut cx, self.alloc)
        };
        let class = class.map_err(|error| match error {
            CAbiError::Layout(error) => BuildError::Layout { stmt: None, error },
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

    fn func_ref(&mut self, instance: Instance) -> RefId {
        if let Some(i) = self.refs.iter().position(|&r| r == instance)
            && let Some(&r) = self.ref_ids.get(i)
        {
            return r;
        }
        let place = u32::try_from(self.refs.len()).unwrap_or(u32::MAX);
        let r = self.b.reference(RefKind::Func, place);
        self.refs.push(instance);
        self.ref_ids.push(r);
        r
    }

    fn new_slot(&mut self, lay: LayoutId, kind: SlotKind) -> Result<SlotId, BuildError> {
        let l = *self.layouts.get(lay);
        let size = u32::try_from(l.size).map_err(|_| self.malformed("a shim slot larger than 4 GiB"))?;
        Ok(self.b.slot(size, l.align, kind))
    }

    fn addr(&mut self, ptr: Value, off: i32) -> Value {
        if off == 0 {
            return ptr;
        }
        let delta = self.b.iconst(Ty::I64, u128::from(i64::from(off).cast_unsigned()));
        self.b.ptr_add(Flags::NONE, ptr, delta)
    }

    fn load_at(&mut self, ptr: Value, off: i32, lay: LayoutId) -> Result<V, BuildError> {
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

    fn store_v(&mut self, ptr: Value, off: i32, lay: LayoutId, x: V) -> Result<(), BuildError> {
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

    fn in_memory(&mut self, x: V, lay: LayoutId) -> Result<(Value, i32), BuildError> {
        if let V::Mem { ptr, off, .. } = x {
            return Ok((ptr, off));
        }
        let slot = self.new_slot(lay, SlotKind::Temp)?;
        let ptr = self.b.slot_addr(slot);
        self.store_v(ptr, 0, lay, x)?;
        Ok((ptr, 0))
    }

    fn scalar(&self, x: V) -> Result<Value, BuildError> {
        match x {
            V::One(v) => Ok(v),
            _ => Err(self.malformed("a scalar expected in a shim")),
        }
    }

    fn wide_values(&mut self, x: V) -> Result<[Value; 2], BuildError> {
        let x = match x {
            V::Mem { ptr, off, lay } => self.load_at(ptr, off, lay)?,
            v => v,
        };
        let V::Two(a, b) = x else {
            return Err(self.malformed("a wide value expected in a shim"));
        };
        Ok([a, b])
    }

    fn read_word(&mut self, ptr: Value, off: i32, start: u32, size: u32, align: u8) -> Result<Value, BuildError> {
        let mut word = None;
        let mut pos = 0u32;
        while pos < size {
            let log = (31 - (size - pos).leading_zeros()).min(3);
            let ty = match log {
                0 => Ty::I8,
                1 => Ty::I16,
                2 => Ty::I32,
                _ => Ty::I64,
            };
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

    fn write_word(&mut self, word: Value, ptr: Value, off: i32, start: u32, size: u32, align: u8) {
        let mut pos = 0u32;
        while pos < size {
            let log = (31 - (size - pos).leading_zeros()).min(3);
            let ty = match log {
                0 => Ty::I8,
                1 => Ty::I16,
                2 => Ty::I32,
                _ => Ty::I64,
            };
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

    /// Reads one parameter value from the function's entry parameter iterator according to `pass`.
    fn take_param(&mut self, lay: LayoutId, pass: Pass, next: &mut impl Iterator<Item = Value>) -> Result<V, BuildError> {
        match pass {
            Pass::Ignore => Ok(V::None),
            Pass::Scalar { .. } => Ok(V::One(next.next().ok_or(self.malformed("a shim parameter missing its register"))?)),
            Pass::Wide { .. } => {
                let lo = next.next().ok_or(self.malformed("a wide shim parameter missing its data"))?;
                let hi = next.next().ok_or(self.malformed("a wide shim parameter missing its meta"))?;
                Ok(V::Two(lo, hi))
            }
            Pass::Ref | Pass::Memory { .. } => {
                let ptr = next.next().ok_or(self.malformed("an indirect shim parameter missing its pointer"))?;
                Ok(V::Mem { ptr, off: 0, lay })
            }
            Pass::Words { size } => {
                let w0 = next.next().ok_or(self.malformed("a C composite shim parameter missing its first word"))?;
                let w1 = if size > 8 {
                    Some(next.next().ok_or(self.malformed("a C composite shim parameter missing its second word"))?)
                } else {
                    None
                };
                let slot = self.new_slot(lay, SlotKind::Temp)?;
                let ptr = self.b.slot_addr(slot);
                let align = self.layouts.get(lay).align;
                self.write_word(w0, ptr, 0, 0, size.min(8), align);
                if let Some(w1) = w1 {
                    self.write_word(w1, ptr, 0, 8, size - 8, align);
                }
                self.load_at(ptr, 0, lay)
            }
        }
    }

    /// Appends call-argument registers for `x` of layout `lay` under `pass`.
    fn pass_arg(&mut self, x: V, lay: LayoutId, pass: Pass, out: &mut Vec<Value, A>) -> Result<(), BuildError> {
        match pass {
            Pass::Ignore => {}
            Pass::Scalar { .. } => {
                let x = match x {
                    V::Mem { ptr, off, lay } => self.load_at(ptr, off, lay)?,
                    v => v,
                };
                out.push(self.scalar(x)?);
            }
            Pass::Wide { .. } => out.extend(self.wide_values(x)?),
            Pass::Ref | Pass::Memory { .. } => {
                let (ptr, off) = self.in_memory(x, lay)?;
                out.push(self.addr(ptr, off));
            }
            Pass::Words { size } => {
                let align = self.layouts.get(lay).align;
                let (ptr, off) = self.in_memory(x, lay)?;
                let w0 = self.read_word(ptr, off, 0, size.min(8), align)?;
                out.push(w0);
                if size > 8 {
                    let w1 = self.read_word(ptr, off, 8, size - 8, align)?;
                    out.push(w1);
                }
            }
        }
        Ok(())
    }

    /// Returns `x` (of layout `lay`) via `ret_pass` and optional hidden `sret` pointer.
    fn ret_value(&mut self, x: V, lay: LayoutId, ret_pass: Pass, sret: Option<Value>) -> Result<(), BuildError> {
        match ret_pass {
            Pass::Ignore => self.b.ret(&[]),
            Pass::Scalar { .. } => {
                let x = match x {
                    V::Mem { ptr, off, lay } => self.load_at(ptr, off, lay)?,
                    v => v,
                };
                let v = self.scalar(x)?;
                self.b.ret(&[v]);
            }
            Pass::Wide { .. } => {
                let [a, b] = self.wide_values(x)?;
                self.b.ret(&[a, b]);
            }
            Pass::Ref | Pass::Memory { .. } => {
                let dst = sret.ok_or(self.malformed("an indirect return without its result pointer"))?;
                self.store_v(dst, 0, lay, x)?;
                self.b.ret(&[]);
            }
            Pass::Words { size } => {
                let align = self.layouts.get(lay).align;
                let (ptr, off) = self.in_memory(x, lay)?;
                let w0 = self.read_word(ptr, off, 0, size.min(8), align)?;
                if size > 8 {
                    let w1 = self.read_word(ptr, off, 8, size - 8, align)?;
                    self.b.ret(&[w0, w1]);
                } else {
                    self.b.ret(&[w0]);
                }
            }
        }
        Ok(())
    }

    /// Emits a call to `target` with `arg_vals`, `arg_lays`, `arg_passes`, returning the result
    /// value `V` (of `ret_lay`, `ret_pass`). Uses `sret_reuse` when the shim's own sret matches
    /// the callee's indirect return.
    fn call_value(
        &mut self,
        target: ShimTarget,
        arg_vals: &[V],
        arg_lays: &[LayoutId],
        arg_passes: &[Pass],
        ret_lay: LayoutId,
        ret_pass: Pass,
        sret_reuse: Option<Value>,
        extra_loc: Option<Value>,
    ) -> Result<V, BuildError> {
        let mut passes: Vec<Pass, A> = Vec::with_capacity_in(arg_passes.len() + usize::from(extra_loc.is_some()), self.alloc);
        let mut values: Vec<Value, A> = Vec::with_capacity_in(arg_passes.len() + 2, self.alloc);
        let sret_slot = match ret_pass {
            Pass::Ref | Pass::Memory { .. } => {
                let ptr = match sret_reuse {
                    Some(p) => p,
                    None => {
                        let slot = self.new_slot(ret_lay, SlotKind::Temp)?;
                        self.b.slot_addr(slot)
                    }
                };
                values.push(ptr);
                Some(ptr)
            }
            _ => None,
        };
        for ((&v, &lay), &pass) in arg_vals.iter().zip(arg_lays.iter()).zip(arg_passes.iter()) {
            self.pass_arg(v, lay, pass, &mut values)?;
            passes.push(pass);
        }
        if let Some(loc) = extra_loc {
            values.push(loc);
            passes.push(Pass::Scalar { ty: Ty::PTR, ext: Ext::None, agg: false });
        }
        let (params, rets) = abi::sig_of_in(&passes, ret_pass, self.alloc);
        let sig = self.b.sig(Conv::C, Variadic::No, &params, &rets);
        let r = match target {
            ShimTarget::Direct(rf) => self.b.call(Flags::NONE, rf, sig, &values),
            ShimTarget::Pointer(p) => self.b.call_indirect(Flags::NONE, p, sig, &values),
        };
        self.unpack_call_result(r, ret_lay, ret_pass, sret_slot)
    }

    fn unpack_call_result(&mut self, r: Value, ret_lay: LayoutId, ret_pass: Pass, sret_slot: Option<Value>) -> Result<V, BuildError> {
        match ret_pass {
            Pass::Ignore => Ok(V::None),
            Pass::Scalar { .. } => Ok(V::One(r)),
            Pass::Wide { .. } => {
                let a = self.b.project(r, 0);
                let b = self.b.project(r, 1);
                Ok(V::Two(a, b))
            }
            Pass::Ref | Pass::Memory { .. } => {
                let ptr = sret_slot.ok_or(self.malformed("an indirect call without its sret pointer"))?;
                Ok(V::Mem { ptr, off: 0, lay: ret_lay })
            }
            Pass::Words { size } => {
                let slot = self.new_slot(ret_lay, SlotKind::Temp)?;
                let ptr = self.b.slot_addr(slot);
                let align = self.layouts.get(ret_lay).align;
                if size > 8 {
                    let w0 = self.b.project(r, 0);
                    let w1 = self.b.project(r, 1);
                    self.write_word(w0, ptr, 0, 0, 8, align);
                    self.write_word(w1, ptr, 0, 8, size - 8, align);
                } else {
                    self.write_word(r, ptr, 0, 0, size, align);
                }
                self.load_at(ptr, 0, ret_lay)
            }
        }
    }

    /// Materializes a `&'static Location<'static>` for `site_item`'s definition site when `callee`
    /// is `#[track_caller]`.
    fn def_site_caller_arg(&mut self, callee: Instance, site_item: ItemKey) -> Result<Option<Value>, BuildError> {
        if self.adts.track_caller(callee, self.subst.ground) != Some(true) {
            return Ok(None);
        }
        let loc_adt = self.adts.location_adt().ok_or(self.malformed("`#[track_caller]` shim without `panic_location` lang item"))?;
        let loc_ty = self.subst.ground.intern(PNode::Adt(loc_adt, List::EMPTY)).map_err(|_| self.malformed("the ground table is full"))?;
        let ret = self.subst.ground.intern(PNode::Ref(Mutbl::Not, loc_ty)).map_err(|_| self.malformed("the ground table is full"))?;
        let def = self.adts.def_site(site_item).map_err(|_| self.malformed("`#[track_caller]` shim without `def_site`"))?;
        let here = Site::Here(CallerSite { file: def.file, item: site_item, offset: def.offset });
        let call = Call { which: Which::CallerLocation, generics: &[], args: &[], ret, site: Some((here, loc_ty)) };
        let adts = self.adts;
        let needs_drop_cb = |_: PTy| false;
        let out = {
            let mut lcx = LayoutCx { ground: &mut *self.subst.ground, facts: adts.facts(), scratch: self.subst.scratch };
            let mut icx = IntrinsicCx {
                layouts: &mut self.layouts,
                cx: &mut lcx,
                needs_drop: &needs_drop_cb,
                ub_checks: adts.ub_checks(),
                overflow_checks: adts.overflow_checks(),
            };
            blaze_build::intrinsics::build(&mut self.b, &call, &mut icx)
        }
        .map_err(|_| self.malformed("failed to materialize `Location` in shim"))?;
        match out {
            Out::Scalar(v) => Ok(Some(v)),
            _ => Err(self.malformed("a `Location` row whose address is not one pointer")),
        }
    }

    fn lang_drop_in_place(&mut self, ty: PTy) -> Result<(Instance, bool), BuildError> {
        let item = self.adts.lang_fn("drop_in_place").ok_or(self.malformed("program declares no `drop_in_place` lang fn"))?;
        let args = self.subst.ground.intern_list(&[ty]).map_err(|_| self.malformed("the ground table is full"))?;
        let may_unwind = self.adts.nounwind(item) != Some(true);
        Ok((Instance { item, args }, may_unwind))
    }

    fn emit_cannot_unwind(&mut self) -> Result<(), BuildError> {
        let item = self.adts.lang_fn("panic_cannot_unwind").ok_or(self.malformed("program declares no `panic_cannot_unwind` lang fn"))?;
        let inst = Instance { item, args: List::EMPTY };
        let loc = self.def_site_caller_arg(inst, item)?;
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
        Ok(())
    }

    /// Emits a void call/invoke to `target` with `sig` and `args`. When `unwinding_pad` is `Some`,
    /// an unwind edge branches to that cleanup/terminate block; when `None`, emits a plain call.
    fn emit_void_call(&mut self, target: ShimTarget, sig: SigId, args: &[Value], may_unwind: bool, unwinding: bool) -> Result<Option<blaze_lir::Block>, BuildError> {
        if self.panic == Panic::Abort || !may_unwind {
            match target {
                ShimTarget::Direct(r) => {
                    self.b.call(Flags::NONE, r, sig, args);
                }
                ShimTarget::Pointer(p) => {
                    self.b.call_indirect(Flags::NONE, p, sig, args);
                }
            }
            return Ok(None);
        }
        let normal = self.b.create_block(BlockKind::Normal);
        let pad = self.b.create_block(if unwinding { BlockKind::Terminate } else { BlockKind::Cleanup });
        match target {
            ShimTarget::Direct(r) => {
                self.b.invoke(Flags::NONE, r, sig, args, normal, pad);
            }
            ShimTarget::Pointer(p) => {
                self.b.invoke_indirect(Flags::NONE, p, sig, args, normal, pad);
            }
        }
        if unwinding {
            self.b.begin(pad);
            let _exn = self.b.param(Ty::PTR);
            self.emit_cannot_unwind()?;
            self.b.begin(normal);
            return Ok(None);
        }
        self.b.begin(normal);
        Ok(Some(pad))
    }

    /// Emits `drop_in_place::<ty>` on `p + off` (thin pointer). Returns the cleanup pad if an
    /// invoke was emitted in normal mode (caller must fill the pad and `resume`).
    fn emit_drop_sized_call(&mut self, ty: PTy, p: Value, off: i32, unwinding: bool) -> Result<Option<blaze_lir::Block>, BuildError> {
        let (inst, may_unwind) = self.lang_drop_in_place(ty)?;
        let r = self.func_ref(inst);
        let ptr = self.addr(p, off);
        let sig = self.b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[]);
        self.emit_void_call(ShimTarget::Direct(r), sig, &[ptr], may_unwind, unwinding)
    }

    /// Emits `drop_in_place::<ty>` on wide pointer `(data, meta)` (for an unsized type `ty`).
    fn emit_drop_wide_call(
        &mut self,
        ty: PTy,
        data: Value,
        meta: Value,
        meta_ty: Ty,
        unwinding: bool,
    ) -> Result<Option<blaze_lir::Block>, BuildError> {
        let (inst, may_unwind) = self.lang_drop_in_place(ty)?;
        let r = self.func_ref(inst);
        let sig = self.b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(meta_ty)], &[]);
        self.emit_void_call(ShimTarget::Direct(r), sig, &[data, meta], may_unwind, unwinding)
    }

    /// Computes the runtime byte offset of a custom DST struct's `dyn` tail field from its
    /// [`DynChain`] and vtable pointer (matching `blaze_build::dyn_tail::tail_offset`).
    fn emit_dyn_tail_offset(&mut self, chain: DynChain, vtable: Value) -> Result<Value, BuildError> {
        const ALIGN_WORD: i32 = 16;
        let levels = chain.levels();
        let outer = levels.first().ok_or(self.malformed("a `dyn` chain with no levels"))?;
        let mut a = self.b.load(Ty::I64, Flags::NONE, vtable, ALIGN_WORD, MemAttrs::aligned(3));
        if let Some(tail) = levels.get(1) {
            let sized = self.b.iconst(Ty::I64, 1u128 << tail.align);
            a = self.b.binary(Op::Umax, Flags::NONE, sized, a);
        }
        let at = self.b.iconst(Ty::I64, u128::from(outer.tail_at));
        let one = self.b.iconst(Ty::I64, 1);
        let m = self.b.binary(Op::Sub, Flags::NONE, a, one);
        let up = self.b.binary(Op::Add, Flags::NONE, at, m);
        let zero = self.b.iconst(Ty::I64, 0);
        let keep = self.b.binary(Op::Sub, Flags::NONE, zero, a);
        Ok(self.b.binary(Op::And, Flags::NONE, up, keep))
    }

    /// Emits `dyn` trait object drop glue: loads vtable slot 0 and calls it with `data` if non-zero.
    fn emit_dyn_glue(&mut self, data: Value, vt: Value, unwinding: bool) -> Result<(), BuildError> {
        let word = self.b.load(Ty::I64, Flags::NONE, vt, 0, MemAttrs::aligned(3));
        let zero = self.b.iconst(Ty::I64, 0);
        let set = self.b.icmp(IntCond::Ne, word, zero);
        let yes = self.b.create_block(BlockKind::Normal);
        let next = self.b.create_block(BlockKind::Normal);
        self.b.br(Flags::NONE, set, (yes, &[]), (next, &[]));
        self.b.begin(yes);
        let f = self.b.load(Ty::PTR, Flags::NONE, vt, 0, MemAttrs::aligned(3));
        let sig = self.b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[]);
        if let Some(pad) = self.emit_void_call(ShimTarget::Pointer(f), sig, &[data], true, unwinding)? {
            let after = self.b.create_block(BlockKind::Normal);
            self.b.jump((after, &[]));
            self.b.begin(pad);
            let exn = self.b.param(Ty::PTR);
            self.b.resume(exn);
            self.b.begin(after);
        }
        self.b.jump((next, &[]));
        self.b.begin(next);
        Ok(())
    }

    /// Emits a loop dropping elements `from..len` of type `elem` (stride bytes apart) at `data`.
    /// When `!unwinding` and `Panic::Unwind`, an unwinding element `i` enters an unwinding loop
    /// dropping `i + 1..len` and then resumes `exn` (or executes `tail_after_unwind` first).
    fn emit_elements_loop(&mut self, elem: PTy, data: Value, from: Value, len: Value, stride: u64, unwinding: bool) -> Result<Option<blaze_lir::Block>, BuildError> {
        if !self.needs_drop_of(elem)? {
            return Ok(None);
        }
        let header = self.b.create_block(BlockKind::Normal);
        let body = self.b.create_block(BlockKind::Normal);
        let exit = self.b.create_block(BlockKind::Normal);
        self.b.jump((header, &[from]));
        self.b.begin(header);
        let i = self.b.param(Ty::I64);
        let more = self.b.icmp(IntCond::Ult, i, len);
        self.b.br(Flags::NONE, more, (body, &[]), (exit, &[]));
        self.b.begin(body);
        let s = self.b.iconst(Ty::I64, u128::from(stride));
        let off = self.b.binary(Op::Mul, Flags::NONE, i, s);
        let ep = self.b.ptr_add(Flags::NONE, data, off);
        let one = self.b.iconst(Ty::I64, 1);
        let next = self.b.binary(Op::Add, Flags::NONE, i, one);
        let pad = self.emit_drop_sized_call(elem, ep, 0, unwinding)?;
        self.b.jump((header, &[next]));

        let unwind_exit = if let Some(pad) = pad {
            // Unwinding loop for `next..len`, passing `(exn, next)` to a dedicated cleanup exit.
            let u_header = self.b.create_block(BlockKind::Cold);
            let u_body = self.b.create_block(BlockKind::Cold);
            let u_exit = self.b.create_block(BlockKind::Cold);
            self.b.begin(pad);
            let exn = self.b.param(Ty::PTR);
            self.b.jump((u_header, &[next, exn]));
            self.b.begin(u_header);
            let ui = self.b.param(Ty::I64);
            let uexn = self.b.param(Ty::PTR);
            let umore = self.b.icmp(IntCond::Ult, ui, len);
            self.b.br(Flags::NONE, umore, (u_body, &[]), (u_exit, &[uexn]));
            self.b.begin(u_body);
            let us = self.b.iconst(Ty::I64, u128::from(stride));
            let uoff = self.b.binary(Op::Mul, Flags::NONE, ui, us);
            let uep = self.b.ptr_add(Flags::NONE, data, uoff);
            let uone = self.b.iconst(Ty::I64, 1);
            let unext = self.b.binary(Op::Add, Flags::NONE, ui, uone);
            let _ = self.emit_drop_sized_call(elem, uep, 0, true)?;
            self.b.jump((u_header, &[unext, uexn]));
            Some(u_exit)
        } else {
            None
        };

        self.b.begin(exit);
        Ok(unwind_exit)
    }

    /// Emits drop glue for slice `[U]` or `str` given `(data, len)`.
    fn emit_slice_glue(&mut self, g: PTy, data: Value, len: Value, unwinding: bool) -> Result<Option<blaze_lir::Block>, BuildError> {
        let elem = match self.node(g)? {
            PNode::Slice(elem) => elem,
            PNode::Str => return Ok(None),
            _ => return Err(self.malformed("slice drop glue asked of non-slice type")),
        };
        if !self.needs_drop_of(elem)? {
            return Ok(None);
        }
        let elay = self.layout(elem)?;
        let stride = self.layouts.get(elay).size;
        let zero = self.b.iconst(Ty::I64, 0);
        self.emit_elements_loop(elem, data, zero, len, stride, unwinding)
    }

    /// Emits `Box<T>` contents drop (`p` points to the `Box<T>` value).
    fn emit_box_contents(&mut self, list: List, p: Value, unwinding: bool) -> Result<Option<blaze_lir::Block>, BuildError> {
        let t = *self
            .subst
            .ground
            .view()
            .list(list)
            .map_err(|_| self.malformed("a `Box`'s arguments past the ground table"))?
            .first()
            .ok_or(self.malformed("a `Box` without its type argument"))?;
        match self.node(t)? {
            PNode::Dynamic(_) => {
                let data = self.b.load(Ty::PTR, Flags::NONE, p, 0, MemAttrs::aligned(3));
                let vt = self.b.load(Ty::PTR, Flags::NONE, p, 8, MemAttrs::aligned(3));
                let word = self.b.load(Ty::I64, Flags::NONE, vt, 0, MemAttrs::aligned(3));
                let zero = self.b.iconst(Ty::I64, 0);
                let set = self.b.icmp(IntCond::Ne, word, zero);
                let yes = self.b.create_block(BlockKind::Normal);
                let next = self.b.create_block(BlockKind::Normal);
                self.b.br(Flags::NONE, set, (yes, &[]), (next, &[]));
                self.b.begin(yes);
                let f = self.b.load(Ty::PTR, Flags::NONE, vt, 0, MemAttrs::aligned(3));
                let sig = self.b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[]);
                let pad = self.emit_void_call(ShimTarget::Pointer(f), sig, &[data], true, unwinding)?;
                self.b.jump((next, &[]));
                self.b.begin(next);
                Ok(pad)
            }
            PNode::Slice(_) | PNode::Str => {
                let data = self.b.load(Ty::PTR, Flags::NONE, p, 0, MemAttrs::aligned(3));
                let len = self.b.load(Ty::I64, Flags::NONE, p, 8, MemAttrs::aligned(3));
                self.emit_slice_glue(t, data, len, unwinding)
            }
            _ => {
                if self.needs_drop_of(t)? {
                    let t_lay = self.layout(t)?;
                    let data = self.b.load(Ty::PTR, Flags::NONE, p, 0, MemAttrs::aligned(3));
                    match self.layouts.get(t_lay).tail {
                        Tail::Sized => self.emit_drop_sized_call(t, data, 0, unwinding),
                        Tail::Slice { .. } => {
                            let len = self.b.load(Ty::I64, Flags::NONE, p, 8, MemAttrs::aligned(3));
                            self.emit_drop_wide_call(t, data, len, Ty::I64, unwinding)
                        }
                        Tail::Dyn => {
                            let vt = self.b.load(Ty::PTR, Flags::NONE, p, 8, MemAttrs::aligned(3));
                            self.emit_drop_wide_call(t, data, vt, Ty::PTR, unwinding)
                        }
                    }
                } else {
                    Ok(None)
                }
            }
        }
    }

    /// Collects the `(offset, field_ty)` pairs for variant `variant` of ground type `ty` (of layout
    /// `lay`) that need drop, in declaration order.
    fn collect_variant_drop_fields(&mut self, ty: PTy, lay: LayoutId, variant: u32) -> Result<Vec<(i32, PTy), A>, BuildError> {
        let mut field_tys: Vec<PTy, A> = Vec::new_in(self.alloc);
        match self.node(ty)? {
            PNode::Tuple(l) | PNode::Record(_, l) => {
                let slice = self.subst.ground.view().list(l).map_err(|_| self.malformed("a tuple's fields past the ground table"))?;
                field_tys.extend_from_slice(slice);
            }
            PNode::Closure { owner, ordinal, args } => {
                let facts = self.adts.facts();
                let c = facts.closure(owner, ordinal).ok_or(self.malformed("a closure without facts in drop glue"))?;
                let mut argv: Vec<PTy, A> = Vec::new_in(self.alloc);
                argv.extend_from_slice(self.subst.ground.view().list(args).map_err(|_| self.malformed("a closure's args past the ground table"))?);
                let env = ground(c.types, c.env, &argv, self.subst.ground, self.subst.scratch, facts).map_err(|e| BuildError::Facts { stmt: None, error: e })?;
                if let PNode::Tuple(el) = self.node(env)? {
                    let slice = self.subst.ground.view().list(el).map_err(|_| self.malformed("a closure env past the ground table"))?;
                    field_tys.extend_from_slice(slice);
                }
            }
            PNode::Adt(key, args) => {
                let adt = self.adts.adt(key).ok_or(self.malformed("an ADT missing in drop glue"))?;
                if adt.kind == AdtKind::Union {
                    return Ok(Vec::new_in(self.alloc));
                }
                let v_idx = usize::try_from(variant).map_err(|_| self.malformed("a variant past its ADT"))?;
                let v = adt.variants.get(v_idx).ok_or(self.malformed("a variant past its ADT"))?;
                let mut argv: Vec<PTy, A> = Vec::new_in(self.alloc);
                argv.extend_from_slice(self.subst.ground.view().list(args).map_err(|_| self.malformed("an ADT's args past the ground table"))?);
                let start = usize::try_from(v.fields.start).map_err(|_| self.malformed("a field range past usize"))?;
                let len = usize::try_from(v.fields.len).map_err(|_| self.malformed("a field range past usize"))?;
                let fields = adt.fields.get(start..start + len).ok_or(self.malformed("a variant's fields past its ADT"))?;
                for f in fields {
                    let ft = ground(adt.types, f.ty, &argv, self.subst.ground, self.subst.scratch, self.adts.facts())
                        .map_err(|e| BuildError::Facts { stmt: None, error: e })?;
                    field_tys.push(ft);
                }
            }
            _ => {}
        }
        let vlay = match self.layouts.get(lay).variants {
            Variants::Multiple { .. } => self.layouts.variant(lay, variant).ok_or(self.malformed("a variant past its layout"))?,
            Variants::Single { .. } => lay,
        };
        let mut out: Vec<(i32, PTy), A> = Vec::new_in(self.alloc);
        for (i, &ft) in field_tys.iter().enumerate() {
            if !self.needs_drop_of(ft)? {
                continue;
            }
            let idx = u32::try_from(i).map_err(|_| self.malformed("a field index past u32"))?;
            let off = self
                .layouts
                .field_offset(vlay, idx)
                .and_then(|o| i32::try_from(o).ok())
                .ok_or(self.malformed("a field offset past i32"))?;
            out.push((off, ft));
        }
        Ok(out)
    }

    /// Emits drop glue for the parts (fields / live enum variant / array elements) of `ty` at `p`.
    /// Because each part calls `drop_in_place::<field_ty>`, this never recurses on nested types.
    fn emit_parts(&mut self, ty: PTy, lay: LayoutId, p: Value, unwinding: bool) -> Result<(), BuildError> {
        match self.node(ty)? {
            PNode::Array(elem, _) => {
                if !self.needs_drop_of(elem)? {
                    return Ok(());
                }
                let (stride, count) = match self.layouts.get(lay).fields {
                    Fields::Array { stride, count, .. } => (stride, count),
                    _ => return Err(self.malformed("an array without an array layout")),
                };
                if count == 0 {
                    return Ok(());
                }
                let zero = self.b.iconst(Ty::I64, 0);
                let len = self.b.iconst(Ty::I64, u128::from(count));
                if let Some(u_exit) = self.emit_elements_loop(elem, p, zero, len, stride, unwinding)? {
                    let after = self.b.create_block(BlockKind::Normal);
                    self.b.jump((after, &[]));
                    self.b.begin(u_exit);
                    let exn = self.b.param(Ty::PTR);
                    self.b.resume(exn);
                    self.b.begin(after);
                }
                return Ok(());
            }
            PNode::Adt(key, _) if self.adts.adt(key).is_some_and(|a| a.kind == AdtKind::Union) => return Ok(()),
            _ => {}
        }

        match self.layouts.get(lay).variants {
            Variants::Single { index } => {
                let fields = self.collect_variant_drop_fields(ty, lay, index)?;
                let mut steps: Vec<DropStep, A> = Vec::with_capacity_in(fields.len(), self.alloc);
                for &(off, ft) in fields.iter() {
                    steps.push(DropStep::DropSized { ty: ft, p, off });
                }
                self.emit_drop_sequence(&steps, unwinding)
            }
            Variants::Multiple { tag, count, .. } => {
                let rty = match lir(tag.prim).0 {
                    Ty::I128 => Ty::I128,
                    _ => Ty::I64,
                };
                let dmask = mask(rty.bits(PTR_BITS));
                let mut arms: Vec<(u128, blaze_lir::Block, Vec<(i32, PTy), A>), A> = Vec::new_in(self.alloc);
                for v in 0..count {
                    let fields = self.collect_variant_drop_fields(ty, lay, v)?;
                    if fields.is_empty() {
                        continue;
                    }
                    let d = self.layouts.discriminant(lay, v).unwrap_or(i128::from(v)).cast_unsigned() & dmask;
                    let blk = self.b.create_block(BlockKind::Normal);
                    arms.push((d, blk, fields));
                }
                if arms.is_empty() {
                    return Ok(());
                }
                let exit = self.b.create_block(BlockKind::Normal);
                let discr = read_discriminant(&mut self.b, &self.layouts, lay, Some((p, 0)), rty)
                    .ok_or(self.malformed("an enum without a readable discriminant in drop glue"))?;
                let mut cases: Vec<(u128, (blaze_lir::Block, &[Value])), A> = Vec::with_capacity_in(arms.len(), self.alloc);
                for (d, blk, _) in arms.iter() {
                    cases.push((*d, (*blk, &[])));
                }
                self.b.switch(discr, (exit, &[]), &cases);
                for (_, blk, fields) in arms.iter() {
                    self.b.begin(*blk);
                    let mut steps: Vec<DropStep, A> = Vec::with_capacity_in(fields.len(), self.alloc);
                    for &(off, ft) in fields.iter() {
                        steps.push(DropStep::DropSized { ty: ft, p, off });
                    }
                    self.emit_drop_sequence(&steps, unwinding)?;
                    self.b.jump((exit, &[]));
                }
                self.b.begin(exit);
                Ok(())
            }
        }
    }

    /// Emits a single `DropStep` in either normal or unwinding mode, returning its cleanup pad
    /// when in normal mode under `Panic::Unwind`.
    fn emit_one_step(&mut self, step: DropStep, unwinding: bool) -> Result<Option<blaze_lir::Block>, BuildError> {
        match step {
            DropStep::DropImpl { instance, p, meta, may_unwind } => {
                let r = self.func_ref(instance);
                match meta {
                    None => {
                        let sig = self.b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[]);
                        self.emit_void_call(ShimTarget::Direct(r), sig, &[p], may_unwind, unwinding)
                    }
                    Some((m, mty)) => {
                        let sig = self.b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(mty)], &[]);
                        self.emit_void_call(ShimTarget::Direct(r), sig, &[p, m], may_unwind, unwinding)
                    }
                }
            }
            DropStep::DropSized { ty, p, off } => self.emit_drop_sized_call(ty, p, off, unwinding),
            DropStep::DropWide { ty, data, meta, meta_ty } => self.emit_drop_wide_call(ty, data, meta, meta_ty, unwinding),
            DropStep::BoxContents { list, p } => self.emit_box_contents(list, p, unwinding),
            DropStep::Parts { ty, lay, p } => {
                self.emit_parts(ty, lay, p, unwinding)?;
                Ok(None)
            }
        }
    }

    /// Emits `steps[0..]` in order. In normal mode under `Panic::Unwind`, if step `i` unwinds,
    /// its landing pad executes `steps[i+1..]` in unwinding mode (double-panic terminates) and
    /// then `resume(exn)`.
    fn emit_drop_sequence(&mut self, steps: &[DropStep], unwinding: bool) -> Result<(), BuildError> {
        let mut pads: Vec<(usize, blaze_lir::Block), A> = Vec::new_in(self.alloc);
        for (i, &step) in steps.iter().enumerate() {
            if let Some(pad) = self.emit_one_step(step, unwinding)? {
                pads.push((i, pad));
            }
        }
        if pads.is_empty() {
            return Ok(());
        }
        let after = self.b.create_block(BlockKind::Normal);
        self.b.jump((after, &[]));
        for &(i, pad) in pads.iter() {
            self.b.begin(pad);
            let exn = self.b.param(Ty::PTR);
            for &rem in steps.iter().skip(i + 1) {
                let _ = self.emit_one_step(rem, true)?;
            }
            self.b.resume(exn);
        }
        self.b.begin(after);
        Ok(())
    }

    /// Drops a custom DST struct or tuple `g` (whose last field is unsized: `[T]`, `str`,
    /// `dyn Tr`, or a nested custom DST) at wide pointer `(data, meta)`:
    /// 1. `<g as Drop>::drop((data, meta))` if `g` implements `Drop`;
    /// 2. Each sized prefix field `0..n-1` that needs drop, in declaration order;
    /// 3. The unsized tail field `n-1` via `drop_in_place::<tail>(tail_ptr, meta)` where
    ///    `tail_ptr` uses the static offset for `Tail::Slice` or `emit_dyn_tail_offset` for
    ///    `Tail::Dyn`.
    fn emit_custom_dst_drop(&mut self, g: PTy, lay: LayoutId, data: Value, meta: Value, meta_ty: Ty) -> Result<(), BuildError> {
        let mut field_tys: Vec<PTy, A> = Vec::new_in(self.alloc);
        match self.node(g)? {
            PNode::Tuple(l) | PNode::Record(_, l) => {
                let slice = self.subst.ground.view().list(l).map_err(|_| self.malformed("a DST tuple's fields past the ground table"))?;
                field_tys.extend_from_slice(slice);
            }
            PNode::Adt(key, args) => {
                let adt = self.adts.adt(key).ok_or(self.malformed("a DST ADT missing in drop glue"))?;
                if adt.kind != AdtKind::Struct || adt.variants.len() != 1 {
                    return Err(self.malformed("an unsized ADT that is not a single-variant struct"));
                }
                let v = adt.variants.first().ok_or(self.malformed("a DST struct without its variant"))?;
                let mut argv: Vec<PTy, A> = Vec::new_in(self.alloc);
                argv.extend_from_slice(self.subst.ground.view().list(args).map_err(|_| self.malformed("a DST struct's args past the ground table"))?);
                let start = usize::try_from(v.fields.start).map_err(|_| self.malformed("a field range past usize"))?;
                let len = usize::try_from(v.fields.len).map_err(|_| self.malformed("a field range past usize"))?;
                let fields = adt.fields.get(start..start + len).ok_or(self.malformed("a DST struct's fields past its ADT"))?;
                for f in fields {
                    let ft = ground(adt.types, f.ty, &argv, self.subst.ground, self.subst.scratch, self.adts.facts())
                        .map_err(|e| BuildError::Facts { stmt: None, error: e })?;
                    field_tys.push(ft);
                }
            }
            _ => return Err(self.malformed("a custom unsized type that is neither a struct nor a tuple")),
        }
        let (&tail_ty, prefix_tys) = field_tys.split_last().ok_or(self.malformed("an unsized struct or tuple with no fields"))?;
        let drop_fn = self.adts.drop_impl(g, self.subst.ground).map_err(|e| BuildError::Facts { stmt: None, error: e })?;
        let mut steps: Vec<DropStep, A> = Vec::with_capacity_in(field_tys.len() + 1, self.alloc);
        if let Some(item) = drop_fn {
            let args = self.subst.ground.intern_list(&[g]).map_err(|_| self.malformed("the ground table is full"))?;
            let may_unwind = self.adts.nounwind(item) != Some(true);
            steps.push(DropStep::DropImpl { instance: Instance { item, args }, p: data, meta: Some((meta, meta_ty)), may_unwind });
        }
        for (i, &ft) in prefix_tys.iter().enumerate() {
            if !self.needs_drop_of(ft)? {
                continue;
            }
            let idx = u32::try_from(i).map_err(|_| self.malformed("a DST field index past u32"))?;
            let off = self
                .layouts
                .field_offset(lay, idx)
                .and_then(|o| i32::try_from(o).ok())
                .ok_or(self.malformed("a DST prefix field offset past i32"))?;
            steps.push(DropStep::DropSized { ty: ft, p: data, off });
        }
        if self.needs_drop_of(tail_ty)? {
            let tail_ptr = match self.layouts.get(lay).tail {
                Tail::Dyn => {
                    let chain = self.layouts.dyn_chain(lay).ok_or(self.malformed("a `dyn`-tailed DST without its `DynChain`"))?;
                    let off = self.emit_dyn_tail_offset(chain, meta)?;
                    self.b.ptr_add(Flags::NONE, data, off)
                }
                Tail::Slice { .. } => {
                    let last_idx = u32::try_from(prefix_tys.len()).map_err(|_| self.malformed("a DST tail index past u32"))?;
                    let off = self
                        .layouts
                        .field_offset(lay, last_idx)
                        .and_then(|o| i32::try_from(o).ok())
                        .ok_or(self.malformed("a DST slice tail offset past i32"))?;
                    self.addr(data, off)
                }
                Tail::Sized => return Err(self.malformed("custom DST drop glue asked of a sized layout")),
            };
            steps.push(DropStep::DropWide { ty: tail_ty, data: tail_ptr, meta, meta_ty });
        }
        self.emit_drop_sequence(&steps, false)
    }

    fn run_drop_glue(&mut self) -> Result<(), BuildError> {
        let g = *self.subst.args.first().ok_or(self.malformed("`drop_in_place` without its type argument"))?;
        let ptr_ty = self.subst.ground.intern(PNode::Ptr(Mutbl::Mut, g)).map_err(|_| self.malformed("the ground table is full"))?;
        let ptr_pass = self.pass_of(ptr_ty)?;
        let (params, rets) = abi::sig_of_in(&[ptr_pass], Pass::Ignore, self.alloc);
        let sig = self.b.sig(Conv::C, Variadic::No, &params, &rets);
        self.b.set_signature(sig);
        let entry = self.b.create_block(BlockKind::Normal);
        self.b.begin(entry);
        let mut vals: Vec<Value, A> = Vec::with_capacity_in(params.len(), self.alloc);
        for p in params.iter() {
            vals.push(self.b.param(p.ty));
        }

        if !self.needs_drop_of(g)? {
            self.b.ret(&[]);
            return Ok(());
        }

        match self.node(g)? {
            PNode::Coroutine { .. } => {
                return Err(self.malformed(
                    "a coroutine's `drop_in_place` glue is built by `build_coroutine` (`BuiltCoroutine::drop`), not `build_drop_glue`",
                ));
            }
            PNode::Dynamic(_) => {
                let &[data, vt] = vals.as_slice() else {
                    return Err(self.malformed("`drop_in_place::<dyn T>` without wide pointer parameters"));
                };
                self.emit_dyn_glue(data, vt, false)?;
                self.b.ret(&[]);
                return Ok(());
            }
            PNode::Slice(_) | PNode::Str => {
                let &[data, len] = vals.as_slice() else {
                    return Err(self.malformed("`drop_in_place::<[T]>` without wide pointer parameters"));
                };
                if let Some(u_exit) = self.emit_slice_glue(g, data, len, false)? {
                    let after = self.b.create_block(BlockKind::Normal);
                    self.b.jump((after, &[]));
                    self.b.begin(u_exit);
                    let exn = self.b.param(Ty::PTR);
                    self.b.resume(exn);
                    self.b.begin(after);
                }
                self.b.ret(&[]);
                return Ok(());
            }
            _ => {}
        }

        let lay = self.layout(g)?;
        if self.layouts.get(lay).tail != Tail::Sized {
            let &[data, meta] = vals.as_slice() else {
                return Err(self.malformed("`drop_in_place::<Dst>` without wide pointer parameters"));
            };
            let Pass::Wide { metadata: meta_ty } = ptr_pass else {
                return Err(self.malformed("an unsized `*mut Dst` not passed as a wide pointer"));
            };
            self.emit_custom_dst_drop(g, lay, data, meta, meta_ty)?;
            self.b.ret(&[]);
            return Ok(());
        }
        let &[p] = vals.as_slice() else {
            return Err(self.malformed("`drop_in_place::<T>` without its thin pointer parameter"));
        };

        let drop_fn = self.adts.drop_impl(g, self.subst.ground).map_err(|e| BuildError::Facts { stmt: None, error: e })?;

        // `Box<T>`: drop contents first, then run `Box`'s own `Drop::drop` (which frees) and fields.
        if let PNode::Adt(k, list) = self.node(g)?
            && self.adts.lang_adt(LangItem::OwnedBox) == Some(k)
        {
            let mut steps: Vec<DropStep, A> = Vec::with_capacity_in(3, self.alloc);
            steps.push(DropStep::BoxContents { list, p });
            if let Some(item) = drop_fn {
                let args = self.subst.ground.intern_list(&[g]).map_err(|_| self.malformed("the ground table is full"))?;
                let may_unwind = self.adts.nounwind(item) != Some(true);
                steps.push(DropStep::DropImpl { instance: Instance { item, args }, p, meta: None, may_unwind });
            }
            steps.push(DropStep::Parts { ty: g, lay, p });
            self.emit_drop_sequence(&steps, false)?;
            self.b.ret(&[]);
            return Ok(());
        }

        let mut steps: Vec<DropStep, A> = Vec::with_capacity_in(2, self.alloc);
        if let Some(item) = drop_fn {
            let args = self.subst.ground.intern_list(&[g]).map_err(|_| self.malformed("the ground table is full"))?;
            let may_unwind = self.adts.nounwind(item) != Some(true);
            steps.push(DropStep::DropImpl { instance: Instance { item, args }, p, meta: None, may_unwind });
        }
        steps.push(DropStep::Parts { ty: g, lay, p });
        self.emit_drop_sequence(&steps, false)?;
        self.b.ret(&[]);
        Ok(())
    }

    fn run_closure_shim(&mut self, closure_ty: PTy, which: FnTrait) -> Result<(), BuildError> {
        let PNode::Closure { owner, ordinal, args } = self.node(closure_ty)? else {
            return Err(self.malformed("a closure shim asked of a non-closure type"));
        };
        let facts = self.adts.facts();
        let c = facts.closure(owner, ordinal).ok_or(self.malformed("a closure without facts in closure shim"))?;
        let mut owner_args: Vec<PTy, A> = Vec::new_in(self.alloc);
        owner_args.extend_from_slice(self.subst.ground.view().list(args).map_err(|_| self.malformed("a closure's args past the ground table"))?);
        let sig_ty = ground(c.types, c.sig, &owner_args, self.subst.ground, self.subst.scratch, facts).map_err(|e| BuildError::Facts { stmt: None, error: e })?;
        let PNode::FnPtr(fl, _) = self.node(sig_ty)? else {
            return Err(self.malformed("a closure's signature is not a fn pointer"));
        };
        let mut all_sig: Vec<PTy, A> = Vec::new_in(self.alloc);
        all_sig.extend_from_slice(self.subst.ground.view().list(fl).map_err(|_| self.malformed("a closure's sig list past the ground table"))?);
        let (&output, inputs) = all_sig.split_last().ok_or(self.malformed("a closure's signature without its output"))?;
        let args_tuple = {
            let l = self.subst.ground.intern_list(inputs).map_err(|_| self.malformed("the ground table is full"))?;
            self.subst.ground.intern(PNode::Tuple(l)).map_err(|_| self.malformed("the ground table is full"))?
        };
        let target_kind = c.kind;
        let target_item = self
            .adts
            .fn_trait_method(target_kind)
            .ok_or(self.malformed("closure shim without target `Fn*` lang trait method"))?;
        let target_args = self.subst.ground.intern_list(&[closure_ty, args_tuple]).map_err(|_| self.malformed("the ground table is full"))?;
        let target_inst = Instance { item: target_item, args: target_args };

        let env_lay = self.layout(closure_ty)?;
        let ptr_ty = self.subst.ground.intern(PNode::Ref(Mutbl::Not, closure_ty)).map_err(|_| self.malformed("the ground table is full"))?;
        let ptr_lay = self.layout(ptr_ty)?;
        let ptr_pass = self.pass_of(ptr_ty)?;
        let self_lay = match which {
            FnTrait::Fn | FnTrait::FnMut => ptr_lay,
            FnTrait::FnOnce => env_lay,
        };
        let self_pass = match which {
            FnTrait::Fn | FnTrait::FnMut => ptr_pass,
            FnTrait::FnOnce => self.pass_of(closure_ty)?,
        };

        let mut in_lays: Vec<LayoutId, A> = Vec::with_capacity_in(inputs.len(), self.alloc);
        let mut in_passes: Vec<Pass, A> = Vec::with_capacity_in(inputs.len(), self.alloc);
        let mut sig_passes: Vec<Pass, A> = Vec::with_capacity_in(inputs.len() + 1, self.alloc);
        sig_passes.push(self_pass);
        for &gi in inputs {
            let l = self.layout(gi)?;
            let p = self.pass_of(gi)?;
            in_lays.push(l);
            in_passes.push(p);
            sig_passes.push(p);
        }
        let ret_lay = self.layout(output)?;
        let ret_pass = self.pass_of(output)?;
        let (params, rets) = abi::sig_of_in(&sig_passes, ret_pass, self.alloc);
        let own_sig = self.b.sig(Conv::C, Variadic::No, &params, &rets);
        self.b.set_signature(own_sig);
        let entry = self.b.create_block(BlockKind::Normal);
        self.b.begin(entry);
        let mut vals: Vec<Value, A> = Vec::with_capacity_in(params.len(), self.alloc);
        for p in params.iter() {
            vals.push(self.b.param(p.ty));
        }
        let mut next = vals.iter().copied();
        let sret = if matches!(ret_pass, Pass::Ref | Pass::Memory { .. }) {
            Some(next.next().ok_or(self.malformed("a closure shim missing its sret pointer"))?)
        } else {
            None
        };
        let me = self.take_param(self_lay, self_pass, &mut next)?;
        let mut arg_vals: Vec<V, A> = Vec::with_capacity_in(inputs.len() + 1, self.alloc);
        let mut arg_lays: Vec<LayoutId, A> = Vec::with_capacity_in(inputs.len() + 1, self.alloc);
        let mut arg_passes: Vec<Pass, A> = Vec::with_capacity_in(inputs.len() + 1, self.alloc);

        // If `which == FnOnce`, we own the closure environment and must drop it after calling
        // the `Fn`/`FnMut` body (and on unwind).
        let owned_env_ptr = match which {
            FnTrait::Fn | FnTrait::FnMut => {
                arg_vals.push(me);
                None
            }
            FnTrait::FnOnce => {
                let (ptr, off) = self.in_memory(me, env_lay)?;
                let addr = self.addr(ptr, off);
                arg_vals.push(V::One(addr));
                Some(addr)
            }
        };
        arg_lays.push(ptr_lay);
        arg_passes.push(ptr_pass);
        for (&l, &p) in in_lays.iter().zip(in_passes.iter()) {
            arg_vals.push(self.take_param(l, p, &mut next)?);
            arg_lays.push(l);
            arg_passes.push(p);
        }

        let target_ref = self.func_ref(target_inst);
        let env_needs_drop = owned_env_ptr.is_some() && self.needs_drop_of(closure_ty)?;
        let result = if env_needs_drop && self.panic == Panic::Unwind {
            // Invoke target so the owned closure environment is dropped even if the body unwinds.
            let mut c_passes: Vec<Pass, A> = Vec::with_capacity_in(arg_passes.len(), self.alloc);
            let mut c_values: Vec<Value, A> = Vec::with_capacity_in(arg_passes.len() + 1, self.alloc);
            let sret_slot = match ret_pass {
                Pass::Ref | Pass::Memory { .. } => {
                    let p = match sret {
                        Some(p) => p,
                        None => {
                            let s = self.new_slot(ret_lay, SlotKind::Temp)?;
                            self.b.slot_addr(s)
                        }
                    };
                    c_values.push(p);
                    Some(p)
                }
                _ => None,
            };
            for ((&v, &l), &p) in arg_vals.iter().zip(arg_lays.iter()).zip(arg_passes.iter()) {
                self.pass_arg(v, l, p, &mut c_values)?;
                c_passes.push(p);
            }
            let (c_params, c_rets) = abi::sig_of_in(&c_passes, ret_pass, self.alloc);
            let c_sig = self.b.sig(Conv::C, Variadic::No, &c_params, &c_rets);
            let normal = self.b.create_block(BlockKind::Normal);
            let pad = self.b.create_block(BlockKind::Cleanup);
            let r = self.b.invoke(Flags::NONE, target_ref, c_sig, &c_values, normal, pad);
            self.b.begin(pad);
            let exn = self.b.param(Ty::PTR);
            if let Some(env_ptr) = owned_env_ptr {
                let _ = self.emit_drop_sized_call(closure_ty, env_ptr, 0, true)?;
            }
            self.b.resume(exn);
            self.b.begin(normal);
            self.unpack_call_result(r, ret_lay, ret_pass, sret_slot)?
        } else {
            self.call_value(ShimTarget::Direct(target_ref), &arg_vals, &arg_lays, &arg_passes, ret_lay, ret_pass, sret, None)?
        };

        if env_needs_drop
            && let Some(env_ptr) = owned_env_ptr
        {
            if let Some(pad) = self.emit_drop_sized_call(closure_ty, env_ptr, 0, false)? {
                let after = self.b.create_block(BlockKind::Normal);
                self.b.jump((after, &[]));
                self.b.begin(pad);
                let exn = self.b.param(Ty::PTR);
                self.b.resume(exn);
                self.b.begin(after);
            }
        }

        if matches!(self.node(output)?, PNode::Never) {
            self.b.unreachable();
        } else {
            self.ret_value(result, ret_lay, ret_pass, sret)?;
        }
        Ok(())
    }

    fn run_fn_shim(&mut self, callee: PTy, tuple: PTy, which: FnTrait, sig: Option<&FnSig<'_>>) -> Result<(), BuildError> {
        let PNode::Tuple(tl) = self.node(tuple)? else {
            return Err(self.malformed("a fn shim's arguments not a tuple"));
        };
        let mut inputs: Vec<PTy, A> = Vec::new_in(self.alloc);
        inputs.extend_from_slice(self.subst.ground.view().list(tl).map_err(|_| self.malformed("a tuple list past the ground table"))?);

        let (direct_inst, callee_abi, output) = match self.node(callee)? {
            PNode::FnDef(item, args) => {
                let sig = sig.ok_or(self.malformed("a fn item shim built without its signature"))?;
                let slice = self.subst.ground.view().list(args).map_err(|_| self.malformed("a fn item's args past the ground table"))?;
                if slice != self.subst.args {
                    return Err(self.malformed("a fn item shim whose args do not match `subst.args`"));
                }
                if sig.inputs.len() != inputs.len() {
                    return Err(self.malformed("a fn item shim whose tuple and signature differ in length"));
                }
                let output = ground(sig.types, sig.output, self.subst.args, self.subst.ground, self.subst.scratch, self.adts.facts())
                    .map_err(|e| BuildError::Facts { stmt: None, error: e })?;
                let abi = self.adts.abi(item).ok_or(BuildError::NoAbi { item })?;
                (Some(Instance { item, args }), abi, output)
            }
            PNode::FnPtr(fl, header) => {
                let all = self.subst.ground.view().list(fl).map_err(|_| self.malformed("a fn ptr sig list past the ground table"))?;
                let (&output, ins) = all.split_last().ok_or(self.malformed("a fn pointer without its output"))?;
                if ins.len() != inputs.len() {
                    return Err(self.malformed("a fn pointer shim whose tuple and signature differ in length"));
                }
                let abi = pointer_abi(header.abi).ok_or(self.malformed("a fn pointer of unsupported ABI in fn shim"))?;
                (None, abi, output)
            }
            _ => return Err(self.malformed("a fn shim asked of neither fn item nor fn pointer")),
        };
        if callee_abi == Abi::RustCall {
            return Err(self.malformed("a \"rust-call\" fn used as an `Fn*` value"));
        }

        let ptr_ty = self.subst.ground.intern(PNode::Ptr(Mutbl::Not, callee)).map_err(|_| self.malformed("the ground table is full"))?;
        let self_ty = match which {
            FnTrait::Fn | FnTrait::FnMut => ptr_ty,
            FnTrait::FnOnce => callee,
        };
        let self_lay = self.layout(self_ty)?;
        let self_pass = self.pass_of(self_ty)?;

        let mut in_lays: Vec<LayoutId, A> = Vec::with_capacity_in(inputs.len(), self.alloc);
        let mut shim_passes: Vec<Pass, A> = Vec::with_capacity_in(inputs.len() + 1, self.alloc);
        shim_passes.push(self_pass);
        for &gi in inputs.iter() {
            let l = self.layout(gi)?;
            let p = self.pass_of(gi)?;
            in_lays.push(l);
            shim_passes.push(p);
        }
        let ret_lay = self.layout(output)?;
        let shim_ret_pass = self.pass_of(output)?;
        let (params, rets) = abi::sig_of_in(&shim_passes, shim_ret_pass, self.alloc);
        let own_sig = self.b.sig(Conv::C, Variadic::No, &params, &rets);
        self.b.set_signature(own_sig);
        let entry = self.b.create_block(BlockKind::Normal);
        self.b.begin(entry);
        let mut vals: Vec<Value, A> = Vec::with_capacity_in(params.len(), self.alloc);
        for p in params.iter() {
            vals.push(self.b.param(p.ty));
        }
        let mut next = vals.iter().copied();
        let sret = if matches!(shim_ret_pass, Pass::Ref | Pass::Memory { .. }) {
            Some(next.next().ok_or(self.malformed("a fn shim missing its sret pointer"))?)
        } else {
            None
        };
        let me = self.take_param(self_lay, self_pass, &mut next)?;
        let mut arg_vals: Vec<V, A> = Vec::with_capacity_in(inputs.len(), self.alloc);
        let mut call_passes: Vec<Pass, A> = Vec::with_capacity_in(inputs.len(), self.alloc);
        for ((&gi, &l), &sp) in inputs.iter().zip(in_lays.iter()).zip(shim_passes.iter().skip(1)) {
            arg_vals.push(self.take_param(l, sp, &mut next)?);
            call_passes.push(self.pass_of_abi(gi, callee_abi)?);
        }
        let (target, loc) = match direct_inst {
            Some(inst) => {
                let site = self.adts.fn_trait_method(which).unwrap_or(inst.item);
                let loc = self.def_site_caller_arg(inst, site)?;
                (ShimTarget::Direct(self.func_ref(inst)), loc)
            }
            None => {
                let fp = match which {
                    FnTrait::Fn | FnTrait::FnMut => {
                        let ptr = self.scalar(me)?;
                        self.b.load(Ty::PTR, Flags::NONE, ptr, 0, MemAttrs::aligned(3))
                    }
                    FnTrait::FnOnce => self.scalar(me)?,
                };
                (ShimTarget::Pointer(fp), None)
            }
        };
        let call_ret_pass = self.pass_of_abi(output, callee_abi)?;
        let reuse = if call_ret_pass == shim_ret_pass { sret } else { None };
        let res = self.call_value(target, &arg_vals, &in_lays, &call_passes, ret_lay, call_ret_pass, reuse, loc)?;
        if matches!(self.node(output)?, PNode::Never) {
            self.b.unreachable();
        } else {
            self.ret_value(res, ret_lay, shim_ret_pass, sret)?;
        }
        Ok(())
    }

    fn run_ctor_fn(&mut self, variant: u32, sig: &FnSig<'_>) -> Result<(), BuildError> {
        let generics = sig.parent_count.checked_add(sig.own_count).ok_or(self.malformed("a generic count past u32"))?;
        if self.subst.args.len() != usize::try_from(generics).unwrap_or(usize::MAX) {
            return Err(self.malformed("a ctor fn instance whose argument count is not its type's generic count"));
        }
        let n = sig.inputs.len();
        let mut in_lays: Vec<LayoutId, A> = Vec::with_capacity_in(n, self.alloc);
        let mut in_passes: Vec<Pass, A> = Vec::with_capacity_in(n, self.alloc);
        for &t in sig.inputs.iter() {
            let g = ground(sig.types, t, self.subst.args, self.subst.ground, self.subst.scratch, self.adts.facts())
                .map_err(|e| BuildError::Facts { stmt: None, error: e })?;
            let l = self.layout(g)?;
            let p = self.pass_of(g)?;
            in_lays.push(l);
            in_passes.push(p);
        }
        let output = ground(sig.types, sig.output, self.subst.args, self.subst.ground, self.subst.scratch, self.adts.facts())
            .map_err(|e| BuildError::Facts { stmt: None, error: e })?;
        let ret_lay = self.layout(output)?;
        let ret_pass = self.pass_of(output)?;
        let (params, rets) = abi::sig_of_in(&in_passes, ret_pass, self.alloc);
        let own_sig = self.b.sig(Conv::C, Variadic::No, &params, &rets);
        self.b.set_signature(own_sig);
        let entry = self.b.create_block(BlockKind::Normal);
        self.b.begin(entry);
        let mut vals: Vec<Value, A> = Vec::with_capacity_in(params.len(), self.alloc);
        for p in params.iter() {
            vals.push(self.b.param(p.ty));
        }
        let mut next = vals.iter().copied();
        let sret = if matches!(ret_pass, Pass::Ref | Pass::Memory { .. }) {
            Some(next.next().ok_or(self.malformed("a ctor fn missing its sret pointer"))?)
        } else {
            None
        };
        let mut fields: Vec<V, A> = Vec::with_capacity_in(n, self.alloc);
        for (&l, &p) in in_lays.iter().zip(in_passes.iter()) {
            fields.push(self.take_param(l, p, &mut next)?);
        }
        if self.layouts.get(ret_lay).is_zst() {
            self.b.ret(&[]);
            return Ok(());
        }
        let dst = match sret {
            Some(p) => p,
            None => {
                let slot = self.new_slot(ret_lay, SlotKind::Temp)?;
                self.b.slot_addr(slot)
            }
        };
        let vlay = match self.layouts.get(ret_lay).variants {
            Variants::Multiple { .. } => self.layouts.variant(ret_lay, variant).ok_or(self.malformed("a variant past its enum"))?,
            Variants::Single { .. } => ret_lay,
        };
        for (i, (&fv, &flay)) in fields.iter().zip(in_lays.iter()).enumerate() {
            let idx = u32::try_from(i).map_err(|_| self.malformed("a field index past u32"))?;
            let off = self
                .layouts
                .field_offset(vlay, idx)
                .and_then(|o| i32::try_from(o).ok())
                .ok_or(self.malformed("a ctor field offset past i32"))?;
            self.store_v(dst, off, flay, fv)?;
        }
        if let Some(tag) = self.layouts.tag_write(ret_lay, variant) {
            let at = i32::try_from(tag.offset).map_err(|_| self.malformed("a tag past 2 GiB"))?;
            let ty = match lir(tag.scalar.prim).0 {
                Ty::PTR => Ty::I64,
                ty => ty,
            };
            let bits = self.b.iconst(ty, tag.value);
            self.b.store(Flags::NONE, bits, dst, at, MemAttrs::aligned(tag.scalar.prim.align()));
        }
        if sret.is_some() {
            self.b.ret(&[]);
        } else {
            let res = self.load_at(dst, 0, ret_lay)?;
            self.ret_value(res, ret_lay, ret_pass, None)?;
        }
        Ok(())
    }

    fn run_reify_shim(&mut self, callee: ItemKey, sig: &FnSig<'_>) -> Result<(), BuildError> {
        let generics = sig.parent_count.checked_add(sig.own_count).ok_or(self.malformed("a generic count past u32"))?;
        if self.subst.args.len() != usize::try_from(generics).unwrap_or(usize::MAX) {
            return Err(self.malformed("a reify shim whose argument count is not its fn's generic count"));
        }
        let abi = self.adts.abi(callee).ok_or(BuildError::NoAbi { item: callee })?;
        if abi == Abi::RustCall {
            return Err(self.malformed("a \"rust-call\" fn taken as a fn pointer"));
        }
        let n = sig.inputs.len();
        let mut in_lays: Vec<LayoutId, A> = Vec::with_capacity_in(n, self.alloc);
        let mut in_passes: Vec<Pass, A> = Vec::with_capacity_in(n, self.alloc);
        for &t in sig.inputs.iter() {
            let g = ground(sig.types, t, self.subst.args, self.subst.ground, self.subst.scratch, self.adts.facts())
                .map_err(|e| BuildError::Facts { stmt: None, error: e })?;
            let l = self.layout(g)?;
            let p = self.pass_of_abi(g, abi)?;
            in_lays.push(l);
            in_passes.push(p);
        }
        let output = ground(sig.types, sig.output, self.subst.args, self.subst.ground, self.subst.scratch, self.adts.facts())
            .map_err(|e| BuildError::Facts { stmt: None, error: e })?;
        let ret_lay = self.layout(output)?;
        let ret_pass = self.pass_of_abi(output, abi)?;
        let (params, rets) = abi::sig_of_in(&in_passes, ret_pass, self.alloc);
        let own_sig = self.b.sig(Conv::C, Variadic::No, &params, &rets);
        self.b.set_signature(own_sig);
        let entry = self.b.create_block(BlockKind::Normal);
        self.b.begin(entry);
        let mut vals: Vec<Value, A> = Vec::with_capacity_in(params.len(), self.alloc);
        for p in params.iter() {
            vals.push(self.b.param(p.ty));
        }
        let mut next = vals.iter().copied();
        let sret = if matches!(ret_pass, Pass::Ref | Pass::Memory { .. }) {
            Some(next.next().ok_or(self.malformed("a reify shim missing its sret pointer"))?)
        } else {
            None
        };
        let mut arg_vals: Vec<V, A> = Vec::with_capacity_in(n, self.alloc);
        for (&l, &p) in in_lays.iter().zip(in_passes.iter()) {
            arg_vals.push(self.take_param(l, p, &mut next)?);
        }
        let args = self.subst.ground.intern_list(self.subst.args).map_err(|_| self.malformed("the ground table is full"))?;
        let target_inst = Instance { item: callee, args };
        let loc = self.def_site_caller_arg(target_inst, callee)?;
        let target_ref = self.func_ref(target_inst);
        let res = self.call_value(ShimTarget::Direct(target_ref), &arg_vals, &in_lays, &in_passes, ret_lay, ret_pass, sret, loc)?;
        if matches!(self.node(output)?, PNode::Never) {
            self.b.unreachable();
        } else {
            self.ret_value(res, ret_lay, ret_pass, sret)?;
        }
        Ok(())
    }

    fn run_vtable_shim(&mut self, target: Instance, sig: Option<&FnSig<'_>>) -> Result<(), BuildError> {
        let is_fn_once = self.adts.fn_trait_method(FnTrait::FnOnce) == Some(target.item);
        let mut target_args: Vec<PTy, A> = Vec::new_in(self.alloc);
        target_args.extend_from_slice(self.subst.ground.view().list(target.args).map_err(|_| self.malformed("a vtable shim target args past ground table"))?);
        let builtin_fn_once = if is_fn_once
            && let &[callee_ty, tuple_ty] = target_args.as_slice()
        {
            match self.node(callee_ty)? {
                PNode::Closure { .. } | PNode::FnDef(..) | PNode::FnPtr(..) => Some((callee_ty, tuple_ty)),
                _ => None,
            }
        } else {
            None
        };

        let mut arg_tys: Vec<PTy, A> = Vec::new_in(self.alloc);
        let (self_ty, output, callee_abi, has_site) = if let Some((callee_ty, tuple_ty)) = builtin_fn_once {
            let PNode::Tuple(tl) = self.node(tuple_ty)? else {
                return Err(self.malformed("a `FnOnce` vtable shim's arguments not a tuple"));
            };
            arg_tys.extend_from_slice(self.subst.ground.view().list(tl).map_err(|_| self.malformed("a tuple list past the ground table"))?);
            let output = match self.node(callee_ty)? {
                PNode::Closure { owner, ordinal, args } => {
                    let facts = self.adts.facts();
                    let c = facts.closure(owner, ordinal).ok_or(self.malformed("a closure without facts in vtable shim"))?;
                    let mut oa: Vec<PTy, A> = Vec::new_in(self.alloc);
                    oa.extend_from_slice(self.subst.ground.view().list(args).map_err(|_| self.malformed("closure args past ground table"))?);
                    let sig_ty = ground(c.types, c.sig, &oa, self.subst.ground, self.subst.scratch, facts).map_err(|e| BuildError::Facts { stmt: None, error: e })?;
                    let PNode::FnPtr(fl, _) = self.node(sig_ty)? else {
                        return Err(self.malformed("closure sig is not a fn ptr"));
                    };
                    *self.subst.ground.view().list(fl).map_err(|_| self.malformed("fn ptr list past ground table"))?.last().ok_or(self.malformed("closure sig without output"))?
                }
                PNode::FnDef(_, fn_args) => {
                    let sig = sig.ok_or(self.malformed("a fn item vtable shim without its signature"))?;
                    if self.subst.ground.view().list(fn_args).map_err(|_| self.malformed("fn args past ground table"))? != self.subst.args {
                        return Err(self.malformed("a fn item vtable shim with mismatched args"));
                    }
                    ground(sig.types, sig.output, self.subst.args, self.subst.ground, self.subst.scratch, self.adts.facts())
                        .map_err(|e| BuildError::Facts { stmt: None, error: e })?
                }
                PNode::FnPtr(fl, _) => {
                    *self.subst.ground.view().list(fl).map_err(|_| self.malformed("fn ptr list past ground table"))?.last().ok_or(self.malformed("fn ptr without output"))?
                }
                _ => return Err(self.malformed("unexpected receiver in built-in `FnOnce` vtable shim")),
            };
            (callee_ty, output, Abi::RustCall, false)
        } else {
            let sig = sig.ok_or(self.malformed("a vtable shim without its target's signature"))?;
            let abi = self.adts.abi(target.item).ok_or(BuildError::NoAbi { item: target.item })?;
            let (&first, rest) = sig.inputs.split_first().ok_or(self.malformed("a by-value vtable shim target without `self`"))?;
            let self_ty = ground(sig.types, first, self.subst.args, self.subst.ground, self.subst.scratch, self.adts.facts())
                .map_err(|e| BuildError::Facts { stmt: None, error: e })?;
            let output = ground(sig.types, sig.output, self.subst.args, self.subst.ground, self.subst.scratch, self.adts.facts())
                .map_err(|e| BuildError::Facts { stmt: None, error: e })?;
            if abi == Abi::RustCall {
                let (&last, mid) = rest.split_last().ok_or(self.malformed("a \"rust-call\" vtable shim target without its tuple"))?;
                for &t in mid {
                    arg_tys.push(
                        ground(sig.types, t, self.subst.args, self.subst.ground, self.subst.scratch, self.adts.facts())
                            .map_err(|e| BuildError::Facts { stmt: None, error: e })?,
                    );
                }
                let tuple_ty = ground(sig.types, last, self.subst.args, self.subst.ground, self.subst.scratch, self.adts.facts())
                    .map_err(|e| BuildError::Facts { stmt: None, error: e })?;
                let PNode::Tuple(tl) = self.node(tuple_ty)? else {
                    return Err(self.malformed("a \"rust-call\" vtable shim target's last param is not a tuple"));
                };
                arg_tys.extend_from_slice(self.subst.ground.view().list(tl).map_err(|_| self.malformed("tuple list past ground table"))?);
            } else {
                for &t in rest {
                    arg_tys.push(
                        ground(sig.types, t, self.subst.args, self.subst.ground, self.subst.scratch, self.adts.facts())
                            .map_err(|e| BuildError::Facts { stmt: None, error: e })?,
                    );
                }
            }
            (self_ty, output, abi, true)
        };

        let ptr_pass = Pass::Scalar { ty: Ty::PTR, ext: Ext::None, agg: false };
        let mut arg_lays: Vec<LayoutId, A> = Vec::with_capacity_in(arg_tys.len(), self.alloc);
        let mut shim_passes: Vec<Pass, A> = Vec::with_capacity_in(1 + arg_tys.len(), self.alloc);
        shim_passes.push(ptr_pass);
        for &gi in arg_tys.iter() {
            let l = self.layout(gi)?;
            let p = self.pass_of_abi(gi, callee_abi)?;
            arg_lays.push(l);
            shim_passes.push(p);
        }
        let ret_lay = self.layout(output)?;
        let ret_pass = self.pass_of_abi(output, callee_abi)?;
        let (params, rets) = abi::sig_of_in(&shim_passes, ret_pass, self.alloc);
        let own_sig = self.b.sig(Conv::C, Variadic::No, &params, &rets);
        self.b.set_signature(own_sig);
        let entry = self.b.create_block(BlockKind::Normal);
        self.b.begin(entry);
        let mut vals: Vec<Value, A> = Vec::with_capacity_in(params.len(), self.alloc);
        for p in params.iter() {
            vals.push(self.b.param(p.ty));
        }
        let mut next = vals.iter().copied();
        let sret = if matches!(ret_pass, Pass::Ref | Pass::Memory { .. }) {
            Some(next.next().ok_or(self.malformed("a vtable shim missing its sret pointer"))?)
        } else {
            None
        };
        let data_ptr = next.next().ok_or(self.malformed("a vtable shim missing its data pointer"))?;
        let self_lay = self.layout(self_ty)?;
        let self_pass = self.pass_of_abi(self_ty, callee_abi)?;
        let self_val = self.load_at(data_ptr, 0, self_lay)?;

        let mut call_vals: Vec<V, A> = Vec::with_capacity_in(1 + arg_tys.len(), self.alloc);
        let mut call_lays: Vec<LayoutId, A> = Vec::with_capacity_in(1 + arg_tys.len(), self.alloc);
        let mut call_passes: Vec<Pass, A> = Vec::with_capacity_in(1 + arg_tys.len(), self.alloc);
        call_vals.push(self_val);
        call_lays.push(self_lay);
        call_passes.push(self_pass);
        for (&l, &p) in arg_lays.iter().zip(shim_passes.iter().skip(1)) {
            call_vals.push(self.take_param(l, p, &mut next)?);
            call_lays.push(l);
            call_passes.push(p);
        }
        let loc = if has_site { self.def_site_caller_arg(target, target.item)? } else { None };
        let target_ref = self.func_ref(target);
        let res = self.call_value(ShimTarget::Direct(target_ref), &call_vals, &call_lays, &call_passes, ret_lay, ret_pass, sret, loc)?;
        if matches!(self.node(output)?, PNode::Never) {
            self.b.unreachable();
        } else {
            self.ret_value(res, ret_lay, ret_pass, sret)?;
        }
        Ok(())
    }

    /// Writes the enum/union tag of `variant` into `dst` (at offset 0) for layout `lay`.
    fn write_tag(&mut self, lay: LayoutId, variant: u32, dst: Value) -> Result<(), BuildError> {
        if let Some(tag) = self.layouts.tag_write(lay, variant) {
            let at = i32::try_from(tag.offset).map_err(|_| self.malformed("a tag past 2 GiB"))?;
            let ty = match lir(tag.scalar.prim).0 {
                Ty::PTR => Ty::I64,
                ty => ty,
            };
            let bits = self.b.iconst(ty, tag.value);
            self.b.store(Flags::NONE, bits, dst, at, MemAttrs::aligned(tag.scalar.prim.align()));
        }
        Ok(())
    }

    /// Calls `clone::<ft>(field_ptr)` and returns its result value, optionally returning a
    /// cleanup landing pad when `need_pad` is true under `Panic::Unwind` and `may_unwind`.
    fn call_clone_field(
        &mut self,
        clone: ItemKey,
        ft: PTy,
        field_ptr: Value,
        flay: LayoutId,
        fpass: Pass,
        sret_reuse: Option<Value>,
        need_pad: bool,
    ) -> Result<(V, Option<blaze_lir::Block>), BuildError> {
        let ptr_pass = Pass::Scalar { ty: Ty::PTR, ext: Ext::None, agg: false };
        let args = self.subst.ground.intern_list(&[ft]).map_err(|_| self.malformed("the ground table is full"))?;
        let inst = Instance { item: clone, args };
        let loc = self.def_site_caller_arg(inst, clone)?;
        let target_ref = self.func_ref(inst);
        let may_unwind = self.adts.nounwind(clone) != Some(true);
        if !(need_pad && self.panic == Panic::Unwind && may_unwind) {
            let v = self.call_value(
                ShimTarget::Direct(target_ref),
                &[V::One(field_ptr)],
                &[flay],
                &[ptr_pass],
                flay,
                fpass,
                sret_reuse,
                loc,
            )?;
            return Ok((v, None));
        }
        let mut c_passes: Vec<Pass, A> = Vec::with_capacity_in(2, self.alloc);
        let mut c_values: Vec<Value, A> = Vec::with_capacity_in(3, self.alloc);
        let sret_slot = match fpass {
            Pass::Ref | Pass::Memory { .. } => {
                let p = match sret_reuse {
                    Some(p) => p,
                    None => {
                        let s = self.new_slot(flay, SlotKind::Temp)?;
                        self.b.slot_addr(s)
                    }
                };
                c_values.push(p);
                Some(p)
            }
            _ => None,
        };
        c_values.push(field_ptr);
        c_passes.push(ptr_pass);
        if let Some(loc) = loc {
            c_values.push(loc);
            c_passes.push(ptr_pass);
        }
        let (c_params, c_rets) = abi::sig_of_in(&c_passes, fpass, self.alloc);
        let c_sig = self.b.sig(Conv::C, Variadic::No, &c_params, &c_rets);
        let normal = self.b.create_block(BlockKind::Normal);
        let pad = self.b.create_block(BlockKind::Cleanup);
        let r = self.b.invoke(Flags::NONE, target_ref, c_sig, &c_values, normal, pad);
        self.b.begin(normal);
        let v = self.unpack_call_result(r, flay, fpass, sret_slot)?;
        Ok((v, Some(pad)))
    }

    /// Builtin `Clone::clone` for tuples, records, closures, structs, structural unions, fn items,
    /// and fn pointers (`subst.args = [Self]`), matching `blaze-build`'s `clone_fn.rs`.
    fn run_builtin_clone(&mut self, clone: ItemKey, copy: bool) -> Result<(), BuildError> {
        let &[me] = self.subst.args else {
            return Err(self.malformed("a builtin `clone` whose instance is not `[Self]`"));
        };
        let mut fields: Vec<PTy, A> = Vec::new_in(self.alloc);
        let mut base = 0u64;
        let mut is_union = false;
        let held = match self.node(me)? {
            PNode::FnDef(..) | PNode::FnPtr(..) => None,
            PNode::Tuple(l) | PNode::Record(_, l) => {
                let slice = self.subst.ground.view().list(l).map_err(|_| self.malformed("a tuple list past the ground table"))?;
                fields.extend_from_slice(slice);
                Some(me)
            }
            PNode::Union(l) => {
                is_union = true;
                let slice = self.subst.ground.view().list(l).map_err(|_| self.malformed("a union list past the ground table"))?;
                fields.extend_from_slice(slice);
                if slice.is_empty() { None } else { Some(me) }
            }
            PNode::Closure { owner, ordinal, args } => {
                let facts = self.adts.facts();
                let c = facts.closure(owner, ordinal).ok_or(self.malformed("a closure without facts in builtin `clone`"))?;
                let mut oa: Vec<PTy, A> = Vec::new_in(self.alloc);
                oa.extend_from_slice(self.subst.ground.view().list(args).map_err(|_| self.malformed("closure args past ground table"))?);
                let env = ground(c.types, c.env, &oa, self.subst.ground, self.subst.scratch, facts)
                    .map_err(|e| BuildError::Facts { stmt: None, error: e })?;
                let PNode::Tuple(el) = self.node(env)? else {
                    return Err(self.malformed("a closure's environment that is not a tuple"));
                };
                let slice = self.subst.ground.view().list(el).map_err(|_| self.malformed("a closure env past the ground table"))?;
                fields.extend_from_slice(slice);
                if !fields.is_empty() {
                    let lc = self.layout(me)?;
                    base = self
                        .layouts
                        .field_offset(lc, 0)
                        .ok_or(self.malformed("a closure type's layout without its one field, the environment"))?;
                }
                Some(env)
            }
            PNode::Adt(adt_key, adt_args) => {
                let facts = self.adts.facts();
                let adt = facts.adt(adt_key).ok_or(self.malformed("a builtin `clone` of an ADT without facts"))?;
                if adt.kind != AdtKind::Struct || adt.variants.len() != 1 {
                    return Err(self.malformed("a builtin `clone` of an ADT that is not a struct"));
                }
                let mut argv: Vec<PTy, A> = Vec::new_in(self.alloc);
                argv.extend_from_slice(self.subst.ground.view().list(adt_args).map_err(|_| self.malformed("an ADT's args past the ground table"))?);
                for f in adt.fields.iter() {
                    let g = ground(adt.types, f.ty, &argv, self.subst.ground, self.subst.scratch, facts)
                        .map_err(|e| BuildError::Facts { stmt: None, error: e })?;
                    fields.push(g);
                }
                if fields.is_empty() { None } else { Some(me) }
            }
            _ => return Err(self.malformed("a builtin `clone` of a type whose `Clone` the language does not give")),
        };
        let copy = copy || held.is_none();
        let me_lay = self.layout(me)?;
        let ret_pass = self.pass_of(me)?;
        let ptr_pass = Pass::Scalar { ty: Ty::PTR, ext: Ext::None, agg: false };
        let (params, rets) = abi::sig_of_in(&[ptr_pass], ret_pass, self.alloc);
        let own_sig = self.b.sig(Conv::C, Variadic::No, &params, &rets);
        self.b.set_signature(own_sig);
        let entry = self.b.create_block(BlockKind::Normal);
        self.b.begin(entry);
        let mut vals: Vec<Value, A> = Vec::with_capacity_in(params.len(), self.alloc);
        for p in params.iter() {
            vals.push(self.b.param(p.ty));
        }
        let mut next = vals.iter().copied();
        let sret = if matches!(ret_pass, Pass::Ref | Pass::Memory { .. }) {
            Some(next.next().ok_or(self.malformed("a builtin `clone` missing its sret pointer"))?)
        } else {
            None
        };
        let at = next.next().ok_or(self.malformed("a builtin `clone` missing its `&self` pointer"))?;

        if copy {
            let v = self.load_at(at, 0, me_lay)?;
            return self.ret_value(v, me_lay, ret_pass, sret);
        }

        if is_union {
            if let &[only] = fields.as_slice() {
                let (v, _) = self.call_clone_field(clone, only, at, me_lay, ret_pass, sret, false)?;
                return self.ret_value(v, me_lay, ret_pass, sret);
            }
            if self.layouts.get(me_lay).is_zst() {
                return self.ret_value(V::None, me_lay, ret_pass, sret);
            }
            let dst = match sret {
                Some(p) => p,
                None => {
                    let slot = self.new_slot(me_lay, SlotKind::Temp)?;
                    self.b.slot_addr(slot)
                }
            };
            match self.layouts.get(me_lay).variants {
                Variants::Single { index } => {
                    let idx = usize::try_from(index).map_err(|_| self.malformed("a union variant past usize"))?;
                    if let Some(&ft) = fields.get(idx) {
                        let off = self
                            .layouts
                            .field_offset(me_lay, 0)
                            .and_then(|o| i32::try_from(o).ok())
                            .ok_or(self.malformed("a union field offset past i32"))?;
                        let flay = self.layout(ft)?;
                        let fpass = self.pass_of(ft)?;
                        let field_ptr = self.addr(at, off);
                        let dst_field = self.addr(dst, off);
                        let reuse = matches!(fpass, Pass::Ref | Pass::Memory { .. }).then_some(dst_field);
                        let (cloned, _) = self.call_clone_field(clone, ft, field_ptr, flay, fpass, reuse, false)?;
                        if reuse.is_none() {
                            self.store_v(dst, off, flay, cloned)?;
                        }
                    }
                    if sret.is_some() {
                        self.b.ret(&[]);
                    } else {
                        let res = self.load_at(dst, 0, me_lay)?;
                        self.ret_value(res, me_lay, ret_pass, None)?;
                    }
                    return Ok(());
                }
                Variants::Multiple { tag, count, .. } => {
                    let rty = match lir(tag.prim).0 {
                        Ty::I128 => Ty::I128,
                        _ => Ty::I64,
                    };
                    let dmask = mask(rty.bits(PTR_BITS));
                    let discr = read_discriminant(&mut self.b, &self.layouts, me_lay, Some((at, 0)), rty)
                        .ok_or(self.malformed("a structural union without a readable discriminant in `clone`"))?;
                    let join = self.b.create_block(BlockKind::Normal);
                    let none = self.b.create_block(BlockKind::Cold);
                    let cap = usize::try_from(count).map_err(|_| self.malformed("too many union variants"))?;
                    let mut arms: Vec<(u128, u32, blaze_lir::Block), A> = Vec::with_capacity_in(cap, self.alloc);
                    for v in 0..count {
                        let d = self.layouts.discriminant(me_lay, v).unwrap_or(i128::from(v)).cast_unsigned() & dmask;
                        let blk = self.b.create_block(BlockKind::Normal);
                        arms.push((d, v, blk));
                    }
                    let mut cases: Vec<(u128, (blaze_lir::Block, &[Value])), A> = Vec::with_capacity_in(arms.len(), self.alloc);
                    for &(d, _, blk) in arms.iter() {
                        cases.push((d, (blk, &[])));
                    }
                    self.b.switch(discr, (none, &[]), &cases);
                    self.b.begin(none);
                    self.b.unreachable();
                    for &(_, v, blk) in arms.iter() {
                        self.b.begin(blk);
                        let Some(vlay) = self.layouts.variant(me_lay, v) else {
                            self.b.unreachable();
                            continue;
                        };
                        let v_idx = usize::try_from(v).map_err(|_| self.malformed("a union variant past usize"))?;
                        let &ft = fields.get(v_idx).ok_or(self.malformed("a union variant past its members"))?;
                        let off = self
                            .layouts
                            .field_offset(vlay, 0)
                            .and_then(|o| i32::try_from(o).ok())
                            .ok_or(self.malformed("a union field offset past i32"))?;
                        let flay = self.layout(ft)?;
                        let fpass = self.pass_of(ft)?;
                        let field_ptr = self.addr(at, off);
                        let dst_field = self.addr(dst, off);
                        let reuse = matches!(fpass, Pass::Ref | Pass::Memory { .. }).then_some(dst_field);
                        let (cloned, _) = self.call_clone_field(clone, ft, field_ptr, flay, fpass, reuse, false)?;
                        if reuse.is_none() {
                            self.store_v(dst, off, flay, cloned)?;
                        }
                        self.write_tag(me_lay, v, dst)?;
                        self.b.jump((join, &[]));
                    }
                    self.b.begin(join);
                    if sret.is_some() {
                        self.b.ret(&[]);
                    } else {
                        let res = self.load_at(dst, 0, me_lay)?;
                        self.ret_value(res, me_lay, ret_pass, None)?;
                    }
                    return Ok(());
                }
            }
        }

        // Field-by-field clone for tuple / record / closure / struct, with reverse-order unwind
        // cleanup of already-cloned fields if a later field's `Clone::clone` panics.
        let held_ty = held.ok_or(self.malformed("non-copy builtin `clone` without held type"))?;
        let held_lay = self.layout(held_ty)?;
        let dst = match sret {
            Some(p) => p,
            None => {
                let slot = self.new_slot(me_lay, SlotKind::Temp)?;
                self.b.slot_addr(slot)
            }
        };
        let mut cloned_info: Vec<(PTy, i32, bool), A> = Vec::with_capacity_in(fields.len(), self.alloc);
        let mut pads: Vec<(usize, blaze_lir::Block), A> = Vec::new_in(self.alloc);
        let mut any_prior_drops = false;
        for (j, &ft) in fields.iter().enumerate() {
            let idx = u32::try_from(j).map_err(|_| self.malformed("a field index past u32"))?;
            let foff = self
                .layouts
                .field_offset(held_lay, idx)
                .ok_or(self.malformed("a field offset past its layout in builtin `clone`"))?;
            let off = base
                .checked_add(foff)
                .and_then(|o| i32::try_from(o).ok())
                .ok_or(self.malformed("a field offset past i32 in builtin `clone`"))?;
            let flay = self.layout(ft)?;
            let fpass = self.pass_of(ft)?;
            let field_ptr = self.addr(at, off);
            let dst_field = self.addr(dst, off);
            let reuse = matches!(fpass, Pass::Ref | Pass::Memory { .. }).then_some(dst_field);
            let (cloned, pad) = self.call_clone_field(clone, ft, field_ptr, flay, fpass, reuse, any_prior_drops)?;
            if let Some(pad) = pad {
                pads.push((j, pad));
            }
            if reuse.is_none() {
                self.store_v(dst, off, flay, cloned)?;
            }
            let drops = self.needs_drop_of(ft)?;
            cloned_info.push((ft, off, drops));
            if drops {
                any_prior_drops = true;
            }
        }
        if !pads.is_empty() {
            let done = self.b.create_block(BlockKind::Normal);
            self.b.jump((done, &[]));
            for &(j, pad) in pads.iter() {
                self.b.begin(pad);
                let exn = self.b.param(Ty::PTR);
                for &(ft, off, drops) in cloned_info.iter().take(j).rev() {
                    if drops {
                        let _ = self.emit_drop_sized_call(ft, dst, off, true)?;
                    }
                }
                self.b.resume(exn);
            }
            self.b.begin(done);
        }
        if sret.is_some() {
            self.b.ret(&[]);
        } else if self.layouts.get(me_lay).is_zst() {
            self.b.ret(&[]);
        } else {
            let res = self.load_at(dst, 0, me_lay)?;
            self.ret_value(res, me_lay, ret_pass, None)?;
        }
        Ok(())
    }

    /// Builtin `From::from(v: Src) -> Dst` for structural error unions (`subst.args = [Dst, Src]`),
    /// where `Src` is a strict member or strict sub-union of `Dst`.
    fn run_builtin_from(&mut self) -> Result<(), BuildError> {
        let &[dst, src] = self.subst.args else {
            return Err(self.malformed("a builtin `from` whose instance is not `[Self, T]`"));
        };
        let PNode::Union(dst_l_list) = self.node(dst)? else {
            return Err(self.malformed("a builtin `from` whose `Self` is not a structural union"));
        };
        let mut dst_members: Vec<PTy, A> = Vec::new_in(self.alloc);
        dst_members.extend_from_slice(self.subst.ground.view().list(dst_l_list).map_err(|_| self.malformed("a union list past the ground table"))?);

        let dst_lay = self.layout(dst)?;
        let ret_pass = self.pass_of(dst)?;
        let src_lay = self.layout(src)?;
        let src_pass = self.pass_of(src)?;
        let (params, rets) = abi::sig_of_in(&[src_pass], ret_pass, self.alloc);
        let own_sig = self.b.sig(Conv::C, Variadic::No, &params, &rets);
        self.b.set_signature(own_sig);
        let entry = self.b.create_block(BlockKind::Normal);
        self.b.begin(entry);
        let mut vals: Vec<Value, A> = Vec::with_capacity_in(params.len(), self.alloc);
        for p in params.iter() {
            vals.push(self.b.param(p.ty));
        }
        let mut next = vals.iter().copied();
        let sret = if matches!(ret_pass, Pass::Ref | Pass::Memory { .. }) {
            Some(next.next().ok_or(self.malformed("a builtin `from` missing its sret pointer"))?)
        } else {
            None
        };
        let v = self.take_param(src_lay, src_pass, &mut next)?;

        let mut inject_member: Option<u32> = None;
        let mut src_members: Vec<PTy, A> = Vec::new_in(self.alloc);
        match self.node(src)? {
            PNode::Union(src_l_list) => {
                src_members.extend_from_slice(self.subst.ground.view().list(src_l_list).map_err(|_| self.malformed("a source union list past the ground table"))?);
                if let [only] = src_members.as_slice() {
                    let j = dst_members
                        .iter()
                        .position(|dm| dm == only)
                        .ok_or(self.malformed("a builtin `from` missing a source union member"))?;
                    inject_member = Some(u32::try_from(j).map_err(|_| self.malformed("a union variant index past u32"))?);
                }
            }
            _ => {
                let j = dst_members
                    .iter()
                    .position(|&dm| dm == src)
                    .ok_or(self.malformed("a builtin `from` missing a source member"))?;
                inject_member = Some(u32::try_from(j).map_err(|_| self.malformed("a union variant index past u32"))?);
            }
        }

        if let Some(variant) = inject_member {
            if dst_members.len() == 1 {
                if variant != 0 {
                    return Err(self.malformed("a single-member union injection at a non-zero variant"));
                }
                return self.ret_value(v, dst_lay, ret_pass, sret);
            }
            if self.layouts.get(dst_lay).is_zst() {
                return self.ret_value(V::None, dst_lay, ret_pass, sret);
            }
            let dst_ptr = match sret {
                Some(p) => p,
                None => {
                    let slot = self.new_slot(dst_lay, SlotKind::Temp)?;
                    self.b.slot_addr(slot)
                }
            };
            let vlay = match self.layouts.get(dst_lay).variants {
                Variants::Multiple { .. } => self.layouts.variant(dst_lay, variant).ok_or(self.malformed("a union variant past its layout"))?,
                Variants::Single { .. } => dst_lay,
            };
            let foff = self
                .layouts
                .field_offset(vlay, 0)
                .and_then(|o| i32::try_from(o).ok())
                .ok_or(self.malformed("a union variant field offset past i32"))?;
            self.write_tag(dst_lay, variant, dst_ptr)?;
            self.store_v(dst_ptr, foff, src_lay, v)?;
            if sret.is_some() {
                self.b.ret(&[]);
            } else {
                let res = self.load_at(dst_ptr, 0, dst_lay)?;
                self.ret_value(res, dst_lay, ret_pass, None)?;
            }
            return Ok(());
        }

        // Multi-member source sub-union -> destination super-union remapping.
        if self.layouts.get(dst_lay).is_zst() {
            return self.ret_value(V::None, dst_lay, ret_pass, sret);
        }
        let (src_ptr, src_off) = self.in_memory(v, src_lay)?;
        let dst_ptr = match sret {
            Some(p) => p,
            None => {
                let slot = self.new_slot(dst_lay, SlotKind::Temp)?;
                self.b.slot_addr(slot)
            }
        };
        let copy_member = |cx: &mut Self, i: u32, j: u32, member_ty: PTy| -> Result<(), BuildError> {
            let svlay = match cx.layouts.get(src_lay).variants {
                Variants::Multiple { .. } => cx.layouts.variant(src_lay, i).ok_or(cx.malformed("a source union variant past its layout"))?,
                Variants::Single { .. } => src_lay,
            };
            let dvlay = match cx.layouts.get(dst_lay).variants {
                Variants::Multiple { .. } => cx.layouts.variant(dst_lay, j).ok_or(cx.malformed("a dest union variant past its layout"))?,
                Variants::Single { .. } => dst_lay,
            };
            let soff = cx
                .layouts
                .field_offset(svlay, 0)
                .and_then(|o| i32::try_from(o).ok())
                .ok_or(cx.malformed("a source union field offset past i32"))?;
            let doff = cx
                .layouts
                .field_offset(dvlay, 0)
                .and_then(|o| i32::try_from(o).ok())
                .ok_or(cx.malformed("a dest union field offset past i32"))?;
            let mlay = cx.layout(member_ty)?;
            cx.write_tag(dst_lay, j, dst_ptr)?;
            let pv = cx.load_at(src_ptr, src_off + soff, mlay)?;
            cx.store_v(dst_ptr, doff, mlay, pv)
        };
        match self.layouts.get(src_lay).variants {
            Variants::Single { index } => {
                let idx = usize::try_from(index).map_err(|_| self.malformed("a source union variant past usize"))?;
                if let Some(&sm) = src_members.get(idx) {
                    let j = dst_members
                        .iter()
                        .position(|&dm| dm == sm)
                        .ok_or(self.malformed("a remapped union missing the single present source member"))?;
                    let j = u32::try_from(j).map_err(|_| self.malformed("a union variant index past u32"))?;
                    copy_member(self, index, j, sm)?;
                }
            }
            Variants::Multiple { tag, .. } => {
                let rty = match lir(tag.prim).0 {
                    Ty::I128 => Ty::I128,
                    _ => Ty::I64,
                };
                let dmask = mask(rty.bits(PTR_BITS));
                let discr = read_discriminant(&mut self.b, &self.layouts, src_lay, Some((src_ptr, src_off)), rty)
                    .ok_or(self.malformed("a source union without a readable discriminant in `from`"))?;
                let join = self.b.create_block(BlockKind::Normal);
                let none = self.b.create_block(BlockKind::Cold);
                let mut arms: Vec<(u128, blaze_lir::Block, u32, u32, PTy), A> = Vec::with_capacity_in(src_members.len(), self.alloc);
                for (i_idx, &sm) in src_members.iter().enumerate() {
                    let Some(j_idx) = dst_members.iter().position(|&dm| dm == sm) else {
                        continue;
                    };
                    let i = u32::try_from(i_idx).map_err(|_| self.malformed("a union variant index past u32"))?;
                    let j = u32::try_from(j_idx).map_err(|_| self.malformed("a union variant index past u32"))?;
                    let d = self.layouts.discriminant(src_lay, i).unwrap_or(i128::from(i)).cast_unsigned() & dmask;
                    let blk = self.b.create_block(BlockKind::Normal);
                    arms.push((d, blk, i, j, sm));
                }
                let mut cases: Vec<(u128, (blaze_lir::Block, &[Value])), A> = Vec::with_capacity_in(arms.len(), self.alloc);
                for &(d, blk, ..) in arms.iter() {
                    cases.push((d, (blk, &[])));
                }
                self.b.switch(discr, (none, &[]), &cases);
                self.b.begin(none);
                self.b.unreachable();
                for &(_, blk, i, j, sm) in arms.iter() {
                    self.b.begin(blk);
                    copy_member(self, i, j, sm)?;
                    self.b.jump((join, &[]));
                }
                self.b.begin(join);
            }
        }
        if sret.is_some() {
            self.b.ret(&[]);
        } else {
            let res = self.load_at(dst_ptr, 0, dst_lay)?;
            self.ret_value(res, dst_lay, ret_pass, None)?;
        }
        Ok(())
    }

    /// `#[test]` `Termination::report` C `main` wrapper (`extern "C" fn(i32, *const *const c_char) -> i32`):
    /// ignores `argc`/`argv`, calls `test()`, passes its result to `<R as Termination>::report`,
    /// and returns the resulting `ExitCode` byte zero-extended to `i32`.
    fn run_test_report_shim(
        &mut self,
        test: ItemKey,
        test_sig: &FnSig<'_>,
        report_fn: ItemKey,
        report_sig: &FnSig<'_>,
    ) -> Result<(), BuildError> {
        let generics = test_sig.parent_count.checked_add(test_sig.own_count).ok_or(self.malformed("a generic count past u32"))?;
        if generics != 0 || !self.subst.args.is_empty() {
            return Err(self.malformed("a test report shim whose test fn has generic parameters"));
        }
        if !test_sig.inputs.is_empty() {
            return Err(self.malformed("a test report shim whose test fn takes parameters"));
        }
        let test_abi = self.adts.abi(test).ok_or(BuildError::NoAbi { item: test })?;
        let output = ground(test_sig.types, test_sig.output, &[], self.subst.ground, self.subst.scratch, self.adts.facts())
            .map_err(|e| BuildError::Facts { stmt: None, error: e })?;
        let output_lay = self.layout(output)?;
        let output_pass = self.pass_of_abi(output, test_abi)?;

        let own_sig = self.b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I32), Param::of(Ty::PTR)], &[Param::of(Ty::I32)]);
        self.b.set_signature(own_sig);
        let entry = self.b.create_block(BlockKind::Normal);
        self.b.begin(entry);
        let _argc = self.b.param(Ty::I32);
        let _argv = self.b.param(Ty::PTR);

        let test_inst = Instance { item: test, args: List::EMPTY };
        let test_loc = self.def_site_caller_arg(test_inst, test)?;
        let test_ref = self.func_ref(test_inst);
        let r_val = self.call_value(ShimTarget::Direct(test_ref), &[], &[], &[], output_lay, output_pass, None, test_loc)?;
        if matches!(self.node(output)?, PNode::Never) {
            self.b.unreachable();
            return Ok(());
        }

        let report_abi = self.adts.abi(report_fn).ok_or(BuildError::NoAbi { item: report_fn })?;
        let report_self_pass = self.pass_of_abi(output, report_abi)?;
        let exit_ty = ground(report_sig.types, report_sig.output, &[output], self.subst.ground, self.subst.scratch, self.adts.facts())
            .map_err(|e| BuildError::Facts { stmt: None, error: e })?;
        let exit_lay = self.layout(exit_ty)?;
        let exit_pass = self.pass_of_abi(exit_ty, report_abi)?;

        let report_args = self.subst.ground.intern_list(&[output]).map_err(|_| self.malformed("the ground table is full"))?;
        let report_inst = Instance { item: report_fn, args: report_args };
        let report_loc = self.def_site_caller_arg(report_inst, report_fn)?;
        let report_ref = self.func_ref(report_inst);
        let exit_val = self.call_value(
            ShimTarget::Direct(report_ref),
            &[r_val],
            &[output_lay],
            &[report_self_pass],
            exit_lay,
            exit_pass,
            None,
            report_loc,
        )?;
        if matches!(self.node(exit_ty)?, PNode::Never) {
            self.b.unreachable();
            return Ok(());
        }

        let exit_size = self.layouts.get(exit_lay).size;
        let status = match exit_val {
            V::None => self.b.iconst(Ty::I32, 0),
            V::One(v) => match self.b.ty(v) {
                Ty::I1 | Ty::I8 => self.b.convert(Op::Zext, Flags::NONE, Ty::I32, v),
                Ty::I32 => v,
                _ => return Err(self.malformed("a `Termination::report` return type that is neither `ExitCode`, `u8`, nor `i32`")),
            },
            V::Mem { ptr, off, .. } => match exit_size {
                1 => {
                    let byte = self.b.load(Ty::I8, Flags::NONE, ptr, off, MemAttrs::aligned(0));
                    self.b.convert(Op::Zext, Flags::NONE, Ty::I32, byte)
                }
                4 => self.b.load(Ty::I32, Flags::NONE, ptr, off, MemAttrs::aligned(2)),
                _ => return Err(self.malformed("a `Termination::report` return type whose size is neither 1 nor 4 bytes")),
            },
            V::Two(..) => return Err(self.malformed("a `Termination::report` return type that is a scalar pair")),
        };
        self.b.ret(&[status]);
        Ok(())
    }
}

/// Builds `core::ptr::drop_in_place::<T>` for the single ground type in `subst.args[0]`,
/// directly from layout and `Adts` facts with no legacy AST walk.
pub fn build_drop_glue<A: Allocator + Copy>(subst: &mut Subst<'_, '_>, adts: &dyn Adts, alloc: A) -> Result<Built<A>, BuildError> {
    if subst.args.len() != 1 {
        return Err(BuildError::Malformed { stmt: None, what: "`drop_in_place` requires exactly one type argument" });
    }
    let mut cx = ShimCx::new(subst, adts, alloc);
    cx.run_drop_glue()?;
    cx.finish()
}

/// Builds a thin entry shim calling closure `closure_ty` through weaker `FnTrait` `which`
/// (`Fn` -> `FnMut`/`FnOnce`, or `FnMut` -> `FnOnce`).
pub fn build_closure_shim<A: Allocator + Copy>(
    closure_ty: PTy,
    which: FnTrait,
    subst: &mut Subst<'_, '_>,
    adts: &dyn Adts,
    alloc: A,
) -> Result<Built<A>, BuildError> {
    let mut cx = ShimCx::new(subst, adts, alloc);
    cx.run_closure_shim(closure_ty, which)?;
    cx.finish()
}

/// Builds the `Fn`/`FnMut`/`FnOnce` shim for a fn item (`FnDef`) or fn pointer (`FnPtr`)
/// called with argument tuple `tuple`.
pub fn build_fn_shim<A: Allocator + Copy>(
    callee: PTy,
    tuple: PTy,
    which: FnTrait,
    sig: Option<&FnSig<'_>>,
    subst: &mut Subst<'_, '_>,
    adts: &dyn Adts,
    alloc: A,
) -> Result<Built<A>, BuildError> {
    let mut cx = ShimCx::new(subst, adts, alloc);
    cx.run_fn_shim(callee, tuple, which, sig)?;
    cx.finish()
}

/// Builds the constructor function of tuple struct or tuple enum variant `variant`.
pub fn build_ctor_fn<A: Allocator + Copy>(
    _ctor: ItemKey,
    variant: u32,
    sig: &FnSig<'_>,
    subst: &mut Subst<'_, '_>,
    adts: &dyn Adts,
    alloc: A,
) -> Result<Built<A>, BuildError> {
    let mut cx = ShimCx::new(subst, adts, alloc);
    cx.run_ctor_fn(variant, sig)?;
    cx.finish()
}

/// Builds the plain fn-pointer `ReifyShim` for a `#[track_caller]` fn `callee`.
pub fn build_reify_shim<A: Allocator + Copy>(
    callee: ItemKey,
    sig: &FnSig<'_>,
    subst: &mut Subst<'_, '_>,
    adts: &dyn Adts,
    alloc: A,
) -> Result<Built<A>, BuildError> {
    let mut cx = ShimCx::new(subst, adts, alloc);
    cx.run_reify_shim(callee, sig)?;
    cx.finish()
}

/// Builds the by-value `self` `VTableShim` for `target`.
pub fn build_vtable_shim<A: Allocator + Copy>(
    target: Instance,
    sig: Option<&FnSig<'_>>,
    subst: &mut Subst<'_, '_>,
    adts: &dyn Adts,
    alloc: A,
) -> Result<Built<A>, BuildError> {
    let mut cx = ShimCx::new(subst, adts, alloc);
    cx.run_vtable_shim(target, sig)?;
    cx.finish()
}

/// Builds the language-provided `Clone::clone` shim for `Self` (`subst.args = [Self]`): a tuple,
/// record, closure, struct, structural union, fn item, or fn pointer.
pub fn build_builtin_clone<A: Allocator + Copy>(
    clone: ItemKey,
    copy: bool,
    subst: &mut Subst<'_, '_>,
    adts: &dyn Adts,
    alloc: A,
) -> Result<Built<A>, BuildError> {
    let mut cx = ShimCx::new(subst, adts, alloc);
    cx.run_builtin_clone(clone, copy)?;
    cx.finish()
}

/// Builds the language-provided `From::from` shim for structural error unions
/// (`subst.args = [Dst, Src]`).
pub fn build_builtin_from<A: Allocator + Copy>(
    _from: ItemKey,
    subst: &mut Subst<'_, '_>,
    adts: &dyn Adts,
    alloc: A,
) -> Result<Built<A>, BuildError> {
    let mut cx = ShimCx::new(subst, adts, alloc);
    cx.run_builtin_from()?;
    cx.finish()
}

/// Builds the `#[test]` `Termination::report` C `main` wrapper for monomorphic test fn `test`.
pub fn build_test_report_shim<A: Allocator + Copy>(
    test: ItemKey,
    test_sig: &FnSig<'_>,
    report_fn: ItemKey,
    report_sig: &FnSig<'_>,
    subst: &mut Subst<'_, '_>,
    adts: &dyn Adts,
    alloc: A,
) -> Result<Built<A>, BuildError> {
    let mut cx = ShimCx::new(subst, adts, alloc);
    cx.run_test_report_shim(test, test_sig, report_fn, report_sig)?;
    cx.finish()
}
