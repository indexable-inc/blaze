//! What the evaluator tells a checker of undefined behavior (`hir-ty-ub`'s `check::Checker`):
//! every memory event, typed copy, frame and thread switch. [`Plain`] is the machine of every
//! run without the checker: its hooks are empty and its provenance and shadow are `()`, so
//! the evaluator generic over it is the evaluator without one. Const eval runs
//! `super::provenance::Provenance`, which tracks only which words are pointers.

use std::ops::Range;

use base_db::Db;
use rustc_abi::TargetDataLayout;

use super::super::{MirSpan, VTableMap};
use crate::{InferBodyId, ParamEnvAndCrate, next_solver::Ty};

/// The evaluator's error for a stop the machine asked for; what it found the machine keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Stop;

pub type MResult<T> = Result<T, Stop>;

/// A pointer: its address, and the provenance the machine gave it (`None`: an integer).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Ptr<P> {
    pub addr: u64,
    pub prov: Option<P>,
}

/// What made an allocation, which says how it may be freed and whether it may leak.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MemKind {
    /// A local, live from its `StorageLive` (or the frame's start, for arguments, the return
    /// place and locals without storage statements) to its `StorageDead` or the frame's end.
    Stack,
    /// `__rust_alloc` and `__rust_alloc_zeroed`, freed by `__rust_dealloc`.
    Rust,
    /// `malloc`, `calloc`, `posix_memalign`, freed by `free`.
    C,
    /// A static, a constant's memory, a promoted temporary, a vtable, a string literal: never
    /// freed, and roots of the leak check.
    Global,
    /// `const_allocate`.
    ConstHeap,
    /// The evaluator's own: program arguments, the environment, thread-local storage, the
    /// thread shims' `errno` cells and dispatch semaphores. Roots of the leak check; the
    /// program frees only the semaphores, through `dispatch_release`.
    Machine,
    /// An anonymous `mmap`, freed by `munmap`. Like the heap, and unlike [`Self::Machine`],
    /// it is made at the allocating thread's time (Miri's `MiriMemoryKind::Mmap`).
    Mmap,
}

/// How bytes are accessed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Access {
    Plain,
    /// An atomic load, store, or read-modify-write (the read and the write of one RMW are one
    /// `read` then one `write` with this, with nothing between them). A failed compare-exchange
    /// is one `read` with its failure ordering and `cxchg_failed`, and no write.
    Atomic { ord: AtomicOrd, rmw: bool, cxchg_failed: bool },
}

/// `core::sync::atomic::Ordering`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AtomicOrd {
    Relaxed,
    Acquire,
    Release,
    AcqRel,
    SeqCst,
}

impl AtomicOrd {
    /// Whether an operation of this ordering acquires: Acquire, AcqRel or SeqCst.
    pub fn acquires(self) -> bool {
        matches!(self, AtomicOrd::Acquire | AtomicOrd::AcqRel | AtomicOrd::SeqCst)
    }

    /// Whether an operation of this ordering releases: Release, AcqRel or SeqCst.
    pub fn releases(self) -> bool {
        matches!(self, AtomicOrd::Release | AtomicOrd::AcqRel | AtomicOrd::SeqCst)
    }
}

/// The retag a typed copy does (rustc_const_eval's `RetagMode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RetagMode {
    Default,
    TwoPhase,
    FnEntry,
    Raw,
    None,
}

/// Which bytes of a value the evaluator holds outside memory are initialized and which carry
/// a pointer's provenance. `()` for [`Plain`]. The evaluator keeps none across a
/// [`Machine::step`]: between statements every value is in memory.
pub trait Shadow<P>: Clone + Default + std::fmt::Debug {
    /// `len` initialized bytes with no provenance: a value the evaluator computed.
    fn init(len: usize) -> Self;
    /// A pointer of `size` bytes with provenance `prov`, initialized.
    fn pointer(size: usize, prov: Option<P>) -> Self;
    /// Bytes `range` of these.
    fn slice(&self, range: Range<usize>) -> Self;
    /// `other` put over these at `at`, growing them when it ends past them; bytes between
    /// their old end and `at` are uninitialized.
    fn put(&mut self, at: usize, other: &Self);
    /// The provenance of the pointer that starts at `at`, if a whole one does.
    fn prov_at(&self, at: usize, size: usize) -> Option<P>;
    /// Calls `f` with the start of each whole pointer of `size` bytes with provenance in the
    /// first `len` bytes of these, in order.
    fn pointers(&self, len: usize, size: usize, mut f: impl FnMut(usize)) {
        if size == 0 || size > len {
            return;
        }
        for at in 0..=len - size {
            if self.prov_at(at, size).is_some() {
                f(at);
            }
        }
    }
}

/// Where the evaluator is, for reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Site<'db> {
    pub owner: InferBodyId<'db>,
    pub span: MirSpan,
}

/// What `typed` needs of the evaluator to read a value at its type.
pub struct TypedCx<'a, 'db> {
    pub db: &'db Db,
    pub env: ParamEnvAndCrate<'db>,
    pub target: &'db TargetDataLayout,
    /// The type a vtable id (a `dyn` pointer's metadata) stands for.
    pub vtables: &'a VTableMap<'db>,
}

/// The evaluator's id of a thread; the main thread is 0.
pub type EvalThread = u32;

pub trait Machine<'db> {
    type Prov: Copy + Eq + std::hash::Hash + std::fmt::Debug;
    type Shadow: Shadow<Self::Prov>;

    /// Whether the machine looks at anything: `false` only for [`Plain`], whose evaluator
    /// then skips what only the hooks need (provenance, per-local allocations, typed copies)
    /// and calls no hook but `schedule`.
    const CHECKS: bool = true;

    /// Where the evaluator is: called before each statement and each terminator.
    fn step(&mut self, at: Site<'db>);

    /// A new allocation; returns its root provenance, which the evaluator puts on every
    /// pointer it makes to it itself (a local's place, a constant's address).
    fn alloc(&mut self, addr: u64, size: u64, align: u64, kind: MemKind, zeroed: bool)
    -> MResult<Self::Prov>;
    fn free(
        &mut self,
        ptr: Ptr<Self::Prov>,
        size: Option<u64>,
        align: Option<u64>,
        kind: MemKind,
    ) -> MResult<()>;
    /// The root provenance of the allocation `addr` is in (for pointers inside a constant's
    /// memory the evaluator lays out itself).
    fn root(&mut self, addr: u64) -> Option<Self::Prov>;
    fn read(&mut self, ptr: Ptr<Self::Prov>, size: u64, align: u64, access: Access)
    -> MResult<Self::Shadow>;
    fn write(
        &mut self,
        ptr: Ptr<Self::Prov>,
        size: u64,
        align: u64,
        access: Access,
        shadow: &Self::Shadow,
    ) -> MResult<()>;
    /// `size` bytes from `src` to `dst`: `copy_nonoverlapping` (`overlapping: false`, which
    /// fails if they overlap), `copy`, and the evaluator's own moves between places.
    fn copy(
        &mut self,
        dst: Ptr<Self::Prov>,
        src: Ptr<Self::Prov>,
        size: u64,
        align: u64,
        overlapping: bool,
    ) -> MResult<()>;
    /// Fails unless every byte of `range` of `shadow` is initialized: the bytes are about to be
    /// used as a scalar (arithmetic, a comparison, a `SwitchInt`, a cast, an address).
    fn require_init(&mut self, shadow: &Self::Shadow, range: Range<usize>) -> MResult<()>;
    /// An atomic load's value under weak memory: the bytes it reads, when the store it reads
    /// from is not the latest (`None`: the bytes in memory). Called right after its `read`,
    /// with no other hook between them, `step` and `schedule` included.
    fn atomic_load(
        &mut self,
        ptr: Ptr<Self::Prov>,
        size: u64,
        ord: AtomicOrd,
        latest: &[u8],
    ) -> MResult<Option<Vec<u8>>>;
    /// An atomic store's bytes, and the bytes they replace, for later loads under weak
    /// memory. Called right after its `write`, with no other hook between them, `step` and
    /// `schedule` included.
    fn atomic_store(
        &mut self,
        ptr: Ptr<Self::Prov>,
        ord: AtomicOrd,
        bytes: &[u8],
        old: &[u8],
    ) -> MResult<()>;
    fn fence(&mut self, ord: AtomicOrd, compiler_only: bool) -> MResult<()>;
    /// `ptr_offset_from(a, b)` (`unsigned`: `ptr_offset_from_unsigned`): both in bounds of one
    /// allocation, as rustc's evaluation requires.
    fn offset_from(&mut self, a: Ptr<Self::Prov>, b: Ptr<Self::Prov>, unsigned: bool)
    -> MResult<()>;

    /// After every typed copy into `place`: checks the value at `ty` and retags what it holds
    /// by `mode`. `bytes` are the value's bytes, already written.
    fn typed(
        &mut self,
        cx: &TypedCx<'_, 'db>,
        place: Ptr<Self::Prov>,
        ty: Ty<'db>,
        bytes: &[u8],
        mode: RetagMode,
    ) -> MResult<()>;
    /// A place projected through a dereference (`(*p).f`, `(*p)[i]`) or `ptr::offset`:
    /// in bounds of its allocation, as an inbounds GEP requires. `wrapping_offset` and
    /// `arith_offset` don't call this.
    fn offset(&mut self, ptr: Ptr<Self::Prov>, by: i64) -> MResult<()>;
    /// A pointer cast to an integer (`as usize`, `expose_provenance`).
    fn expose(&mut self, ptr: Ptr<Self::Prov>) -> MResult<()>;
    /// An integer cast to a pointer (`as *const T`, `with_exposed_provenance`).
    fn from_exposed(&mut self, addr: u64) -> Option<Self::Prov>;
    /// The provenance of a pointer to the function `id` (a [`VTableMap`] id), which is in no
    /// allocation. A machine that gives functions none sees such a pointer as an integer.
    fn function(&mut self, _id: u64) -> Option<Self::Prov> {
        None
    }
    /// The [`MemKind::Global`] allocation that starts at `addr` is read-only from now on, its
    /// initial bytes written; a write to it is undefined behavior. By rustc's interning
    /// (`intern_const_alloc_recursive`): a constant's value and all it points to; a static's
    /// own memory when it is not `static mut` and its type is `Freeze`; what a static points
    /// to unless it is `static mut`. Promoted temporaries stay writable: blaze's body writes
    /// them itself.
    fn freeze(&mut self, addr: u64);
    /// The [`MemKind::Global`] allocation that starts at `addr` has the initial bytes the
    /// evaluator laid out when the program first used it (a static's value, the pointer to a
    /// static): they are its value from before every thread, as a static's is in Miri, not a
    /// write of the thread that happened to use it first.
    fn initialized(&mut self, addr: u64);

    /// A frame starts: called before the arguments are copied into the callee's locals.
    fn call(&mut self) -> MResult<()>;
    /// An argument passed in place (`Operand::Move`) or the return place, after `call`:
    /// protected for the call and uninitialized.
    fn protect(&mut self, place: Ptr<Self::Prov>, size: u64) -> MResult<()>;
    /// A frame ends, by return or by unwinding: called before the return value is copied
    /// into the caller's destination. Ends the protectors of the frame's `FnEntry` retags.
    fn ret(&mut self) -> MResult<()>;

    /// The evaluator keeps one code stack per thread and runs the one `schedule` picks: at
    /// every terminator, when the running thread blocks or ends, and at a yield.
    /// A new thread, numbered by the evaluator in creation order, before it first runs.
    fn spawn(&mut self, thread: EvalThread) -> MResult<()>;
    fn schedule(&mut self, running: EvalThread, runnable: &[EvalThread]) -> MResult<EvalThread>;
    fn join(&mut self, joined: EvalThread) -> MResult<()>;
    fn detach(&mut self, thread: EvalThread) -> MResult<()>;
    fn exit(&mut self) -> MResult<()>;
    /// `sched_yield`, `thread::yield_now`, `spin_loop`: called before the `schedule` it makes.
    fn yield_now(&mut self);
    /// `pthread_setname_np`.
    fn thread_name(&mut self, thread: EvalThread, name: &[u8]);
    /// A synchronization: `release` at a futex wake, an unlock, a condvar notify; `acquire`
    /// when the woken or locking thread goes on. `object` is the address of the futex word or
    /// lock.
    fn release(&mut self, object: u64) -> MResult<()>;
    fn acquire(&mut self, object: u64) -> MResult<()>;

    /// The main thread returned: the leak check, and threads still running.
    fn finish(&mut self) -> MResult<()>;
}

/// No checker: every run without `--ub` but const eval's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Plain;

impl<P> Shadow<P> for () {
    #[inline(always)]
    fn init(_: usize) -> Self {}
    #[inline(always)]
    fn pointer(_: usize, _: Option<P>) -> Self {}
    #[inline(always)]
    fn slice(&self, _: Range<usize>) -> Self {}
    #[inline(always)]
    fn put(&mut self, _: usize, _: &Self) {}
    #[inline(always)]
    fn prov_at(&self, _: usize, _: usize) -> Option<P> {
        None
    }
}

impl<'db> Machine<'db> for Plain {
    type Prov = ();
    type Shadow = ();

    const CHECKS: bool = false;

    #[inline(always)]
    fn step(&mut self, _: Site<'db>) {}
    #[inline(always)]
    fn alloc(&mut self, _: u64, _: u64, _: u64, _: MemKind, _: bool) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn free(&mut self, _: Ptr<()>, _: Option<u64>, _: Option<u64>, _: MemKind) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn root(&mut self, _: u64) -> Option<()> {
        None
    }
    #[inline(always)]
    fn read(&mut self, _: Ptr<()>, _: u64, _: u64, _: Access) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn write(&mut self, _: Ptr<()>, _: u64, _: u64, _: Access, _: &()) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn copy(&mut self, _: Ptr<()>, _: Ptr<()>, _: u64, _: u64, _: bool) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn require_init(&mut self, _: &(), _: Range<usize>) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn atomic_load(
        &mut self,
        _: Ptr<()>,
        _: u64,
        _: AtomicOrd,
        _: &[u8],
    ) -> MResult<Option<Vec<u8>>> {
        Ok(None)
    }
    #[inline(always)]
    fn atomic_store(&mut self, _: Ptr<()>, _: AtomicOrd, _: &[u8], _: &[u8]) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn fence(&mut self, _: AtomicOrd, _: bool) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn offset_from(&mut self, _: Ptr<()>, _: Ptr<()>, _: bool) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn typed(
        &mut self,
        _: &TypedCx<'_, 'db>,
        _: Ptr<()>,
        _: Ty<'db>,
        _: &[u8],
        _: RetagMode,
    ) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn offset(&mut self, _: Ptr<()>, _: i64) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn expose(&mut self, _: Ptr<()>) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn from_exposed(&mut self, _: u64) -> Option<()> {
        None
    }
    #[inline(always)]
    fn freeze(&mut self, _: u64) {}
    #[inline(always)]
    fn initialized(&mut self, _: u64) {}
    #[inline(always)]
    fn call(&mut self) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn protect(&mut self, _: Ptr<()>, _: u64) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn ret(&mut self) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn spawn(&mut self, _: EvalThread) -> MResult<()> {
        Ok(())
    }
    /// The running thread goes on while it can, then the first runnable: a run without the
    /// checker is deterministic and switches only when it must.
    #[inline(always)]
    fn schedule(&mut self, running: EvalThread, runnable: &[EvalThread]) -> MResult<EvalThread> {
        Ok(match runnable.contains(&running) {
            true => running,
            false => runnable.first().copied().unwrap_or(running),
        })
    }
    #[inline(always)]
    fn join(&mut self, _: EvalThread) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn detach(&mut self, _: EvalThread) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn exit(&mut self) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn yield_now(&mut self) {}
    #[inline(always)]
    fn thread_name(&mut self, _: EvalThread, _: &[u8]) {}
    #[inline(always)]
    fn release(&mut self, _: u64) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn acquire(&mut self, _: u64) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn finish(&mut self) -> MResult<()> {
        Ok(())
    }
}
