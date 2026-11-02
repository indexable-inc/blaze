//! What an embedder of the evaluator adds to a run: functions it runs itself ([`Foreign`]),
//! calls into the program, the program's memory, and the statics a run changed.
//!
//! blaze runs a proc macro this way: it calls the macro's client, answers the bridge's calls
//! it claims, and keeps the statics the expansion changed beside it.

use std::mem;

use base_db::{ConstEvalBudget, Db};
use blaze_hash::HashMap;
use hir_def::{FunctionId, StaticId, attrs::AttrFlags};
use la_arena::ArenaMap;
use rustc_type_ir::inherent::IntoKind;
use smallvec::SmallVec;

use crate::{
    CallableDefId,
    db::HirDatabase,
    next_solver::{GenericArgs, Ty, TyKind},
};

use super::{
    Address, At, Evaluator, Interval, IntervalAndTy, Locals, MirBody, MirEvalError, MirSpan,
    Origin, Result, Then, from_bytes,
    machine::{Machine, Plain},
    not_supported, return_slot,
};

/// Functions an embedder runs in place of the evaluator, with the evaluator to read and write
/// the program's memory and call back into it.
pub trait Foreign<'db, M: Machine<'db> = Plain> {
    /// Whether this embedder runs calls of `func` itself. Asked once per function an
    /// evaluation calls, before the evaluator's intrinsics, shims and bodies, and kept for the
    /// evaluation. A call through a function pointer or a `Fn` trait to `func` is asked too.
    fn claims(&mut self, db: &'db Db, func: FunctionId) -> bool;
    /// A call of a claimed `func` with `generic_args`. `args` holds each argument's bytes, in
    /// order, copied out of the evaluator's memory. `ret` is the return place, zeroed and the
    /// size of the return type. [`Claimed::Declined`] gives the call back to the evaluator,
    /// which runs it in place as if `func` were not claimed and ignores `ret`.
    /// `Err(MirEvalError::MachineStop)` ends the evaluation; the embedder keeps why.
    fn call(
        &mut self,
        eval: &mut Evaluator<'db, M>,
        func: FunctionId,
        generic_args: GenericArgs<'db>,
        args: &[&[u8]],
        ret: &mut [u8],
    ) -> Result<'db, Claimed>;
}

/// What an embedder did with one call of a function it claims.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claimed {
    /// It ran the call: `ret` is what the call returns.
    Answered,
    /// It gave the call back to the evaluator's own intrinsics, shims and body.
    Declined,
}

/// The evaluator's state for its embedder: none of it is used by a run with nothing set.
pub(super) struct Embedder<'db, M: Machine<'db>> {
    // dyn: the embedder's, one per evaluation and reached only by the calls it claims
    foreign: Option<Box<dyn Foreign<'db, M> + 'db>>,
    /// [`Foreign::claims`] of each function asked about.
    claims: HashMap<FunctionId, bool>,
    /// The arguments of a claimed call, end to end, kept for the next one.
    args: Vec<u8>,
    /// The return place of a claimed call, kept for the next one.
    ret: Vec<u8>,
    /// With [`Evaluator::track_statics`], each static's bytes as first written, by where its
    /// memory starts.
    first_statics: Option<HashMap<usize, Box<[u8]>>>,
    /// What [`Evaluator::preset_static`] gave, until the static is first used (each thread's
    /// first use, for a `#[thread_local]` one).
    preset_statics: HashMap<StaticId, Box<[u8]>>,
}

impl<'db, M: Machine<'db>> Default for Embedder<'db, M> {
    fn default() -> Self {
        Embedder {
            foreign: None,
            claims: HashMap::default(),
            // alloc: none until the first claimed call, then reused
            args: Vec::new(),
            // alloc: none until the first claimed call, then reused
            ret: Vec::new(),
            first_statics: None,
            preset_statics: HashMap::default(),
        }
    }
}

impl<'db, M: Machine<'db>> Evaluator<'db, M> {
    /// Sets the embedder whose calls this evaluation makes, giving back the one before.
    pub fn set_foreign(
        &mut self,
        // dyn: the embedder's, set once per evaluation
        foreign: Option<Box<dyn Foreign<'db, M> + 'db>>,
        // dyn: the embedder's, set once per evaluation
    ) -> Option<Box<dyn Foreign<'db, M> + 'db>> {
        self.embedder.claims.clear();
        mem::replace(&mut self.embedder.foreign, foreign)
    }

    /// A nested call of `func` with `generic_args` and the bytes of each argument, as MIR's
    /// `Call` of it makes one: intrinsics, shims and the embedder's claims included. The bytes
    /// of what it returns.
    pub fn call(
        &mut self,
        func: FunctionId,
        generic_args: GenericArgs<'db>,
        args: &[&[u8]],
    ) -> Result<'db, Vec<u8>> {
        let callee = Ty::new_fn_def(
            self.interner(),
            CallableDefId::FunctionId(func).into(),
            generic_args,
        );
        self.call_value(callee, None, args)
    }

    /// [`Self::call`] of what the function-pointer value `pointer` points to.
    pub fn call_pointer(&mut self, pointer: &[u8], args: &[&[u8]]) -> Result<'db, Vec<u8>> {
        let callee = self.vtable_map.ty(from_bytes!(usize, pointer))?;
        self.call_value(callee, Some(pointer), args)
    }

    /// The bytes of a pointer to `func` with `generic_args`, as a `ReifyFnPointer` cast of its
    /// `FnDef` makes them.
    pub fn fn_pointer(
        &mut self,
        func: FunctionId,
        generic_args: GenericArgs<'db>,
    ) -> Result<'db, Vec<u8>> {
        let ty = Ty::new_fn_def(
            self.interner(),
            CallableDefId::FunctionId(func).into(),
            generic_args,
        );
        let id = self.vtable_map.id(ty);
        // alloc: one per pointer an embedder asks for
        Ok(id.to_le_bytes()[..self.ptr_size()].to_vec())
    }

    /// `len` bytes at `addr`, an address as the program holds it.
    pub fn read_bytes(&self, addr: u64, len: usize) -> Result<'db, &[u8]> {
        self.read_memory(Address::from_usize(addr as usize), len)
    }

    /// Writes `bytes` at `addr`, an address as the program holds it.
    pub fn write_bytes(&mut self, addr: u64, bytes: &[u8]) -> Result<'db, ()> {
        self.write_memory(Address::from_usize(addr as usize), bytes)
    }

    /// `bytes` copied to fresh heap memory the program never frees, as the `getenv` shim makes
    /// its value. Its address as the program holds it.
    pub fn alloc_bytes(&mut self, bytes: &[u8], align: usize) -> Result<'db, u64> {
        let addr = self.heap_allocate(bytes.len(), align)?;
        self.write_memory(addr, bytes)?;
        Ok(addr.to_usize() as u64)
    }

    /// In place of the const-eval budget of the crate the evaluation is for, with all of its
    /// steps left.
    pub fn set_budget(&mut self, budget: ConstEvalBudget) {
        self.budget = budget;
        self.steps_left = budget.steps;
        self.lint_mark = super::lint_mark(self.limits_frames, budget.steps);
    }

    /// The steps (basic blocks run) spent of the budget since it was last set.
    pub fn steps_spent(&self) -> u64 {
        self.budget.steps - self.steps_left
    }

    /// The most bytes of stack and heap together the evaluation has held so far, over every
    /// budget it was given.
    pub fn memory_peak(&self) -> u64 {
        self.memory_peak
    }

    /// The `-C overflow-checks` of the crate a run's code is compiled as: what a body that
    /// inherits its checks uses. Unset, such a body checks (a constant's evaluation).
    pub fn set_inherited_overflow_checks(&mut self, checks: bool) {
        self.inherited_overflow_checks = Some(checks);
    }

    /// From now on, each static's bytes as first written (its initializer, or what
    /// [`Self::preset_static`] gave) are kept, for [`Self::changed_statics`]. A static used
    /// before is not.
    pub fn track_statics(&mut self) {
        self.embedder.first_statics.get_or_insert_default();
    }

    /// Each static tracked since [`Self::track_statics`] whose bytes now differ from those first
    /// written, `#[thread_local]` ones of the running thread included: its bytes now, and
    /// whether an aligned pointer-sized word of them is an address of the evaluator's stack or
    /// heap.
    pub fn changed_statics(&self, out: &mut Vec<(StaticId, Vec<u8>, bool)>) -> Result<'db, ()> {
        let Some(first) = &self.embedder.first_statics else {
            return Ok(());
        };
        let ptr_size = self.ptr_size();
        for &(start, size, st) in &self.static_memory {
            let Some(was) = first.get(&start) else {
                continue;
            };
            if AttrFlags::query(self.db, st.into()).contains(AttrFlags::THREAD_LOCAL) {
                // Another thread's copy, unless the running thread's points here.
                let Some(&location) = self.thread_statics.get(&st) else {
                    continue;
                };
                if from_bytes!(usize, self.peek_memory(location, ptr_size)?) != start {
                    continue;
                }
            }
            let now = self.peek_memory(Address::from_usize(start), size)?;
            if now == &**was {
                continue;
            }
            let holds_address = (start.next_multiple_of(ptr_size)..start + size)
                .step_by(ptr_size)
                .filter(|word| word + ptr_size <= start + size)
                .any(|word| {
                    let mut bytes = [0; 8];
                    bytes[..ptr_size].copy_from_slice(&now[word - start..][..ptr_size]);
                    let addr = Address::from_usize(u64::from_le_bytes(bytes) as usize);
                    matches!(addr.at(), At::Stack(_) | At::Heap(_))
                });
            // alloc: one per changed static, kept by the embedder
            out.push((st, now.to_vec(), holds_address));
        }
        Ok(())
    }

    /// When `id` is first used, its memory starts as `bytes`, which are its size, instead of
    /// its initializer.
    pub fn preset_static(&mut self, id: StaticId, bytes: &[u8]) {
        self.embedder.preset_statics.insert(id, bytes.into());
    }

    /// Whether the embedder runs calls of `def`: asked once, then remembered.
    #[inline]
    pub(super) fn foreign_claims(&mut self, def: FunctionId) -> bool {
        let db = self.db;
        let Some(foreign) = &mut self.embedder.foreign else {
            return false;
        };
        *self.embedder.claims.entry(def).or_insert_with(|| foreign.claims(db, def))
    }

    /// A call of `def`, which the embedder claims. Whether it answered the call; when it
    /// declined, nothing was written and the evaluator runs the call itself.
    pub(super) fn exec_foreign(
        &mut self,
        def: FunctionId,
        args: &[IntervalAndTy<'db>],
        generic_args: GenericArgs<'db>,
        destination: Interval,
    ) -> Result<'db, bool> {
        let mut buffer = mem::take(&mut self.embedder.args);
        buffer.clear();
        let mut ends = SmallVec::<[usize; 8]>::new();
        for arg in args {
            buffer.extend_from_slice(arg.interval.get(self)?);
            ends.push(buffer.len());
        }
        let mut ret = mem::take(&mut self.embedder.ret);
        ret.clear();
        ret.resize(destination.size, 0);
        let Some(mut foreign) = self.embedder.foreign.take() else {
            return Err(MirEvalError::InternalError("a claimed call with no embedder".into()));
        };
        let result = {
            let mut slices = SmallVec::<[&[u8]; 8]>::new();
            let mut start = 0;
            for &end in &ends {
                slices.push(&buffer[start..end]);
                start = end;
            }
            foreign.call(self, def, generic_args, &slices, &mut ret)
        };
        if self.embedder.foreign.is_none() {
            self.embedder.foreign = Some(foreign);
        }
        self.embedder.args = buffer;
        let result = result.and_then(|claimed| match claimed {
            Claimed::Answered => destination.write_from_bytes(self, &ret).map(|()| true),
            Claimed::Declined => Ok(false),
        });
        self.embedder.ret = ret;
        result
    }

    /// Where a static's memory is, kept from its first write with [`Self::track_statics`].
    pub(super) fn record_static(
        &mut self,
        start: Address,
        size: usize,
        st: StaticId,
    ) -> Result<'db, ()> {
        self.static_memory.push((start.to_usize(), size, st));
        if self.embedder.first_statics.is_some() {
            // alloc: once per static, with `track_statics`
            let bytes: Box<[u8]> = self.peek_memory(start, size)?.into();
            if let Some(first) = &mut self.embedder.first_statics {
                first.insert(start.to_usize(), bytes);
            }
        }
        Ok(())
    }

    /// The location of `st` made from what [`Self::preset_static`] gave, if it gave any.
    pub(super) fn preset_static_location(&mut self, st: StaticId) -> Result<'db, Option<Address>> {
        if self.embedder.preset_statics.is_empty() {
            return Ok(None);
        }
        let Some(bytes) = self.embedder.preset_statics.remove(&st) else {
            return Ok(None);
        };
        let thread_local =
            AttrFlags::query(self.db, st.into()).contains(AttrFlags::THREAD_LOCAL);
        let ty = self.db.type_for_static(st).instantiate_identity().skip_norm_wip();
        let layout = self.layout(ty)?;
        let (size, align) = (layout.size.bytes_usize(), layout.align.bytes() as usize);
        if bytes.len() != size {
            let given = bytes.len();
            not_supported!("{given} preset bytes of a static of {size}");
        }
        let memory = self.heap_allocate(size, align)?;
        self.write_memory(memory, &bytes)?;
        let location = if thread_local {
            let location = self.heap_allocate(self.ptr_size(), self.ptr_size())?;
            self.write_pointer(location, memory)?;
            self.thread_statics.insert(st, location);
            // Each thread's copy starts from them.
            self.embedder.preset_statics.insert(st, bytes);
            location
        } else {
            self.locate_static(st, memory)?
        };
        self.freeze_static(st, ty, Interval::new(memory, size));
        self.record_static(memory, size, st)?;
        Ok(Some(location))
    }

    /// A nested call of `callee`, an `FnDef` or a closure's type, through `pointer` when it is
    /// a function pointer's value.
    fn call_value(
        &mut self,
        callee: Ty<'db>,
        pointer: Option<&[u8]>,
        args: &[&[u8]],
    ) -> Result<'db, Vec<u8>> {
        let (body, params, ret) = self.call_target(callee)?;
        if params.len() != args.len() {
            let (given, takes) = (args.len(), params.len());
            not_supported!("{given} arguments to {callee:?}, which takes {takes}");
        }
        let Some(body) = self.body_running.or(body) else {
            not_supported!("calling {callee:?}, which has no body, with nothing running");
        };
        let locals = Locals { ptr: ArenaMap::new(), body };
        let mut values = SmallVec::<[IntervalAndTy<'db>; 8]>::new();
        for (bytes, ty) in args.iter().zip(params) {
            let layout = self.layout(ty)?;
            if layout.size.bytes_usize() != bytes.len() {
                let given = bytes.len();
                not_supported!("{given} bytes for an argument of {ty:?} to {callee:?}");
            }
            let align = layout.align.bytes() as usize;
            let addr = self.allocate(bytes.len(), align, Origin::Machine, true)?;
            self.write_memory(addr, bytes)?;
            values.push(IntervalAndTy { interval: Interval::new(addr, bytes.len()), ty });
        }
        let layout = self.layout(ret)?;
        let size = layout.size.bytes_usize();
        let destination = Interval::new(
            self.allocate(size, layout.align.bytes() as usize, Origin::Machine, true)?,
            size,
        );
        let through = match pointer {
            Some(pointer) => {
                let at = self.allocate(pointer.len(), self.ptr_size(), Origin::Machine, true)?;
                self.write_memory(at, pointer)?;
                Some(Interval::new(at, pointer.len()))
            }
            None => None,
        };
        let then = mem::replace(&mut self.call_then, Then::Nested);
        let called = match (through, callee.kind()) {
            (Some(pointer), _) => self.exec_fn_pointer(
                pointer,
                destination,
                &values,
                &locals,
                Then::Nested,
                MirSpan::Unknown,
            ),
            (None, TyKind::FnDef(def, generic_args)) => self.exec_fn_def(
                def.0,
                generic_args.skip_binder(),
                destination,
                &values,
                &locals,
                Then::Nested,
                MirSpan::Unknown,
            ),
            // alloc: error path
            (None, _) => Err(MirEvalError::NotSupported(format!("calling {callee:?}"))),
        };
        self.call_then = then;
        if let Some(pointer) = through {
            self.free_call_block(pointer)?;
        }
        if called?.is_some() || self.pending.take().is_some() {
            not_supported!("a call of {callee:?} from the embedder that did not run to its end");
        }
        if self.waiting.take().is_some() {
            not_supported!("a call of {callee:?} from the embedder that blocked");
        }
        // alloc: one per embedder's call, given to it
        let result = self.read_memory(destination.addr, destination.size)?.to_vec();
        for value in &values {
            self.free_call_block(value.interval)?;
        }
        self.free_call_block(destination)?;
        Ok(result)
    }

    /// The body `callee` runs, when it has one, its parameters' types and its return type.
    fn call_target(
        &mut self,
        callee: Ty<'db>,
    ) -> Result<'db, (Option<&'db MirBody<'db>>, SmallVec<[Ty<'db>; 8]>, Ty<'db>)> {
        match callee.kind() {
            TyKind::FnDef(def, generic_args) => {
                let generic_args = generic_args.skip_binder();
                let sig = self
                    .db
                    .callable_item_signature(def.0)
                    .instantiate(self.interner(), generic_args)
                    .skip_norm_wip()
                    .skip_binder();
                let normalize = |ty| {
                    super::super::monomorphization::normalized_ty(self.db, ty, self.param_env)
                        // alloc: error path
                        .map_err(|e| MirEvalError::NotSupported(format!("{callee:?}: {e:?}")))
                };
                let params =
                    sig.inputs().iter().copied().map(normalize).collect::<Result<'db, _>>()?;
                let ret = normalize(sig.output())?;
                let body = match def.0 {
                    CallableDefId::FunctionId(func) if self.body_running.is_none() => Some(
                        super::super::optimize::interpreted_body(
                            self.db,
                            super::super::MirOwner::Body(func.into()),
                            generic_args.store(),
                            self.param_env.store(),
                        )
                        .map_err(|e| MirEvalError::MirLowerError(func, e))?,
                    ),
                    _ => None,
                };
                Ok((body, params, ret))
            }
            TyKind::Closure(closure, generic_args) => {
                let body = super::super::optimize::interpreted_body(
                    self.db,
                    super::super::MirOwner::Closure(closure.0),
                    generic_args.store(),
                    self.param_env.store(),
                )
                .map_err(|e| MirEvalError::MirLowerErrorForClosure(closure.0, e))?;
                // The first parameter is the closure itself, which a pointer to it has none of.
                let params = body.param_locals[1..].iter().map(|&it| body.locals[it].ty.as_ref());
                // alloc: a `SmallVec`, one per embedder's call
                let params = params.collect();
                Ok((Some(body), params, body.locals[return_slot()].ty.as_ref()))
            }
            _ => not_supported!("calling {callee:?}, which is not a function"),
        }
    }

    /// Frees a block [`Self::call_value`] made.
    fn free_call_block(&mut self, block: Interval) -> Result<'db, ()> {
        if block.size == 0 {
            return Ok(());
        }
        self.deallocate(block.addr, Origin::Machine, "an embedder's call")
    }
}
