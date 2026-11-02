//! The evaluator's machine, callable from the evaluator's `&self` paths (a place's
//! evaluation reads the pointers it goes through), each hook's `Stop` made the evaluator's
//! error.

use std::{cell::RefCell, ops::Range};

use super::machine::{
    Access, AtomicOrd, EvalThread, MResult, Machine, MemKind, Ptr, RetagMode, Site, Stop, TypedCx,
};
use crate::{mir::MirEvalError, next_solver::Ty};

type Result<'db, T> = std::result::Result<T, MirEvalError<'db>>;

fn stopped<'db, T>(it: MResult<T>) -> Result<'db, T> {
    it.map_err(|Stop| MirEvalError::MachineStop)
}

/// The machine, behind the evaluator's shared borrows. Each method borrows it for one hook and
/// hands out no reference to it; a hook that reached `Hooks` again would panic on the borrow.
/// Without `M::CHECKS` (only [`super::machine::Plain`], whose hooks are empty) the hooks that
/// return nothing the evaluator uses don't touch the cell, so a run without the checker pays
/// no borrow flag; `alloc`, `read` and `atomic_load` are called only under `M::CHECKS`, and
/// `schedule` only with a second thread. Const eval's machine
/// (`super::provenance::Provenance`) checks, so it pays one flag per hook.
pub(super) struct Hooks<M>(RefCell<M>);

impl<M> Hooks<M> {
    pub(super) fn new(machine: M) -> Self {
        Hooks(RefCell::new(machine))
    }

    pub(super) fn into_inner(self) -> M {
        self.0.into_inner()
    }

    #[inline(always)]
    fn with<R>(&self, f: impl FnOnce(&mut M) -> R) -> R {
        f(&mut *self.0.borrow_mut())
    }
}

impl<'db, M: Machine<'db>> Hooks<M> {
    /// A hook that returns nothing: skipped without `M::CHECKS`.
    #[inline(always)]
    fn unit(&self, f: impl FnOnce(&mut M) -> MResult<()>) -> Result<'db, ()> {
        match M::CHECKS {
            true => stopped(self.with(f)),
            false => Ok(()),
        }
    }

    #[inline(always)]
    pub(super) fn step(&self, at: Site<'db>) {
        if M::CHECKS {
            self.with(|m| m.step(at))
        }
    }
    #[inline(always)]
    pub(super) fn alloc(
        &self,
        addr: u64,
        size: u64,
        align: u64,
        kind: MemKind,
        zeroed: bool,
    ) -> Result<'db, M::Prov> {
        stopped(self.with(|m| m.alloc(addr, size, align, kind, zeroed)))
    }
    #[inline(always)]
    pub(super) fn free(
        &self,
        ptr: Ptr<M::Prov>,
        size: Option<u64>,
        align: Option<u64>,
        kind: MemKind,
    ) -> Result<'db, ()> {
        self.unit(|m| m.free(ptr, size, align, kind))
    }
    #[inline(always)]
    pub(super) fn root(&self, addr: u64) -> Option<M::Prov> {
        match M::CHECKS {
            true => self.with(|m| m.root(addr)),
            false => None,
        }
    }
    #[inline(always)]
    pub(super) fn read(
        &self,
        ptr: Ptr<M::Prov>,
        size: u64,
        align: u64,
        access: Access,
    ) -> Result<'db, M::Shadow> {
        stopped(self.with(|m| m.read(ptr, size, align, access)))
    }
    #[inline(always)]
    pub(super) fn write(
        &self,
        ptr: Ptr<M::Prov>,
        size: u64,
        align: u64,
        access: Access,
        shadow: &M::Shadow,
    ) -> Result<'db, ()> {
        self.unit(|m| m.write(ptr, size, align, access, shadow))
    }
    #[inline(always)]
    pub(super) fn copy(
        &self,
        dst: Ptr<M::Prov>,
        src: Ptr<M::Prov>,
        size: u64,
        align: u64,
        overlapping: bool,
    ) -> Result<'db, ()> {
        self.unit(|m| m.copy(dst, src, size, align, overlapping))
    }
    #[inline(always)]
    pub(super) fn require_init(&self, shadow: &M::Shadow, range: Range<usize>) -> Result<'db, ()> {
        self.unit(|m| m.require_init(shadow, range))
    }
    #[inline(always)]
    pub(super) fn atomic_load(
        &self,
        ptr: Ptr<M::Prov>,
        size: u64,
        ord: AtomicOrd,
        latest: &[u8],
    ) -> Result<'db, Option<Vec<u8>>> {
        stopped(self.with(|m| m.atomic_load(ptr, size, ord, latest)))
    }
    #[inline(always)]
    pub(super) fn atomic_store(
        &self,
        ptr: Ptr<M::Prov>,
        ord: AtomicOrd,
        bytes: &[u8],
        old: &[u8],
    ) -> Result<'db, ()> {
        self.unit(|m| m.atomic_store(ptr, ord, bytes, old))
    }
    #[inline(always)]
    pub(super) fn fence(&self, ord: AtomicOrd, compiler_only: bool) -> Result<'db, ()> {
        self.unit(|m| m.fence(ord, compiler_only))
    }
    #[inline(always)]
    pub(super) fn offset_from(
        &self,
        a: Ptr<M::Prov>,
        b: Ptr<M::Prov>,
        unsigned: bool,
    ) -> Result<'db, ()> {
        self.unit(|m| m.offset_from(a, b, unsigned))
    }
    #[inline(always)]
    pub(super) fn typed(
        &self,
        cx: &TypedCx<'_, 'db>,
        place: Ptr<M::Prov>,
        ty: Ty<'db>,
        bytes: &[u8],
        mode: RetagMode,
    ) -> Result<'db, ()> {
        self.unit(|m| m.typed(cx, place, ty, bytes, mode))
    }
    #[inline(always)]
    pub(super) fn offset(&self, ptr: Ptr<M::Prov>, by: i64) -> Result<'db, ()> {
        self.unit(|m| m.offset(ptr, by))
    }
    #[inline(always)]
    pub(super) fn expose(&self, ptr: Ptr<M::Prov>) -> Result<'db, ()> {
        self.unit(|m| m.expose(ptr))
    }
    #[inline(always)]
    pub(super) fn from_exposed(&self, addr: u64) -> Option<M::Prov> {
        match M::CHECKS {
            true => self.with(|m| m.from_exposed(addr)),
            false => None,
        }
    }
    #[inline(always)]
    pub(super) fn function(&self, id: u64) -> Option<M::Prov> {
        match M::CHECKS {
            true => self.with(|m| m.function(id)),
            false => None,
        }
    }
    #[inline(always)]
    pub(super) fn freeze(&self, addr: u64) {
        if M::CHECKS {
            self.with(|m| m.freeze(addr))
        }
    }
    #[inline(always)]
    pub(super) fn initialized(&self, addr: u64) {
        if M::CHECKS {
            self.with(|m| m.initialized(addr))
        }
    }
    #[inline(always)]
    pub(super) fn call(&self) -> Result<'db, ()> {
        self.unit(|m| m.call())
    }
    #[inline(always)]
    pub(super) fn protect(&self, place: Ptr<M::Prov>, size: u64) -> Result<'db, ()> {
        self.unit(|m| m.protect(place, size))
    }
    #[inline(always)]
    pub(super) fn ret(&self) -> Result<'db, ()> {
        self.unit(|m| m.ret())
    }
    #[inline(always)]
    pub(super) fn spawn(&self, thread: EvalThread) -> Result<'db, ()> {
        self.unit(|m| m.spawn(thread))
    }
    #[inline(always)]
    pub(super) fn schedule(
        &self,
        running: EvalThread,
        runnable: &[EvalThread],
    ) -> Result<'db, EvalThread> {
        stopped(self.with(|m| m.schedule(running, runnable)))
    }
    #[inline(always)]
    pub(super) fn join(&self, joined: EvalThread) -> Result<'db, ()> {
        self.unit(|m| m.join(joined))
    }
    #[inline(always)]
    pub(super) fn detach(&self, thread: EvalThread) -> Result<'db, ()> {
        self.unit(|m| m.detach(thread))
    }
    #[inline(always)]
    pub(super) fn exit(&self) -> Result<'db, ()> {
        self.unit(|m| m.exit())
    }
    #[inline(always)]
    pub(super) fn yield_now(&self) {
        if M::CHECKS {
            self.with(|m| m.yield_now())
        }
    }
    #[inline(always)]
    pub(super) fn thread_name(&self, thread: EvalThread, name: &[u8]) {
        if M::CHECKS {
            self.with(|m| m.thread_name(thread, name))
        }
    }
    #[inline(always)]
    pub(super) fn release(&self, object: u64) -> Result<'db, ()> {
        self.unit(|m| m.release(object))
    }
    #[inline(always)]
    pub(super) fn acquire(&self, object: u64) -> Result<'db, ()> {
        self.unit(|m| m.acquire(object))
    }
    #[inline(always)]
    pub(super) fn finish(&self) -> Result<'db, ()> {
        self.unit(|m| m.finish())
    }
}
