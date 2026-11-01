//! The checker: [`Checker`] is the evaluator's `Machine`. At each hook it runs memory's checks,
//! then the race detector's and Tree Borrows', in the order Miri's `machine.rs` runs them
//! (`before_memory_read`, `before_memory_write`, `before_memory_deallocation`,
//! `init_alloc_extra`, `after_stack_push`/`before_stack_pop`, `retag_ptr_value`), and stops the
//! run at the first undefined behavior. [`run_test`] runs a test under it once per schedule and
//! renders what it found as Miri does.
//!
//! The order within a hook is Miri's, which puts the race detector before Tree Borrows for
//! plain accesses (a race is reported over the aliasing error it may cause) and after it for
//! atomic ones (`validate_atomic_*` run after the access itself):
//!
//! - plain read or write: memory, race, Tree Borrows;
//! - atomic load or store: memory, Tree Borrows, race; a read-modify-write's race check runs
//!   once, on its write;
//! - copy: both sides' memory checks, the source's race and Tree Borrows read, the
//!   destination's race and Tree Borrows write, then the overlap check (`mem_copy_repeatedly`);
//! - free: memory, race, Tree Borrows.

mod offset;
mod report;
#[cfg(test)]
mod tests;
mod valid;

use std::ops::Range;

use base_db::Db;
use blaze_hash::HashMap;
use hir_def::{FunctionId, HasModule};
use hir_ty::{
    InferBodyId,
    db::HirDatabase,
    mir::{MirEvalError, MirSpan},
    next_solver::Ty,
};
use hir_ty_mir::mir::{
    self as eval, Access, EvalThread, MResult, Machine, MemKind, Ptr, RetagMode, Site, Stop,
    TypedCx,
};

use self::valid::{Fail, Plans};
pub use self::{offset::OffsetFromError, valid::ValueError};
use crate::{
    borrows::{self, Borrows, NewPermission, PtrKind},
    ids::{AccessKind, AllocId, AtomicOrd, Prov, SiteId, Span, ThreadId},
    memory::{AlignMsg, Bytes, Kind, Memory},
    race::{self, Races},
    ub::{Found, Ub},
};

/// The shadow of a value the evaluator holds outside memory: [`Bytes`].
#[derive(Clone, Default, Debug)]
pub struct Shadow(pub Bytes);

impl eval::Shadow<Prov> for Shadow {
    fn init(len: usize) -> Shadow {
        Shadow(Bytes::init(len))
    }

    fn pointer(size: usize, prov: Option<Prov>) -> Shadow {
        Shadow(Bytes::pointer(size, prov))
    }

    fn slice(&self, range: Range<usize>) -> Shadow {
        Shadow(self.0.slice(range))
    }

    fn put(&mut self, at: usize, other: &Shadow) {
        self.0.put(at, &other.0);
    }

    fn prov_at(&self, at: usize, size: usize) -> Option<Prov> {
        self.0.prov_at(at, size)
    }
}

/// The evaluator's sites, interned into dense [`SiteId`]s (`SiteId(i + 1)` is `sites[i]`;
/// [`SiteId::START`] is before the first step). `step` interns one per statement, so a site is
/// found with no hashing: the body's table is looked up only when the body changes, and the
/// span indexes that table's column for its kind.
#[derive(Debug, Default)]
pub struct Sites<'db> {
    sites: Vec<Site<'db>>,
    bodies: HashMap<InferBodyId<'db>, u32>,
    /// By the index in `bodies`: per span kind, the `SiteId` of each arena index
    /// (`SiteId::START` where not yet interned).
    spans: Vec<[Vec<SiteId>; 7]>,
    current: Option<(InferBodyId<'db>, u32)>,
}

impl<'db> Sites<'db> {
    #[inline]
    pub fn intern(&mut self, site: Site<'db>) -> SiteId {
        let body = match self.current {
            Some((owner, body)) if owner == site.owner => body,
            _ => self.enter(site.owner),
        };
        let (kind, i) = match site.span {
            MirSpan::ExprId(it) => (0, it.into_raw().into_u32()),
            MirSpan::ExprEnd(it) => (1, it.into_raw().into_u32()),
            MirSpan::PatId(it) => (2, it.into_raw().into_u32()),
            MirSpan::BindingId(it) => (3, it.into_raw().into_u32()),
            MirSpan::SelfParam => (4, 0),
            MirSpan::Unknown => (5, 0),
            MirSpan::Inlined(it) => (6, it),
        };
        let Sites { sites, spans, .. } = self;
        let column = &mut spans[body as usize][kind];
        let i = i as usize;
        if column.len() <= i {
            column.resize(i + 1, SiteId::START);
        }
        if column[i] == SiteId::START {
            sites.push(site);
            column[i] = SiteId(sites.len() as u32);
        }
        column[i]
    }

    #[cold]
    fn enter(&mut self, owner: InferBodyId<'db>) -> u32 {
        let next = self.spans.len() as u32;
        let body = *self.bodies.entry(owner).or_insert(next);
        if body == next {
            self.spans.push(Default::default());
        }
        self.current = Some((owner, body));
        body
    }

    pub fn get(&self, id: SiteId) -> Option<Site<'db>> {
        let i = id.0.checked_sub(1)?;
        self.sites.get(i as usize).copied()
    }

    pub fn len(&self) -> usize {
        self.sites.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sites.is_empty()
    }
}

/// How [`run_test`] runs a test. Everything is on by default, as in Miri.
#[derive(Clone, Debug)]
pub struct Config {
    /// The first schedule's seed; schedule `i` runs with `seed + i`.
    pub seed: u64,
    /// How many schedules to run, each with its own seed (Miri's `-Zmiri-many-seeds`).
    pub schedules: u32,
    /// The race detector and scheduler; its `seed` is replaced by each schedule's.
    pub race: race::Config,
    /// The leak check at the end, and the error for threads still running then.
    pub leaks: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config { seed: 0, schedules: 1, race: race::Config::default(), leaks: true }
    }
}

/// What a test did under the checker.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Every schedule ran to the end with no undefined behavior.
    Passed { schedules: u32 },
    /// The test panicked or an assertion failed.
    Failed { message: String, seed: u64 },
    /// Undefined behavior, with its report in Miri's words.
    Ub { found: Found, report: String, seed: u64 },
    /// The evaluator or the checker can't run something the test does. A failure, never a pass.
    Unsupported { what: String },
    /// The evaluator stopped with an error of its own: no layout, a MIR lowering error, a stack
    /// overflow, undefined behavior it found itself.
    Error { message: String, seed: u64 },
}

impl Outcome {
    pub fn passed(&self) -> bool {
        matches!(self, Outcome::Passed { .. })
    }
}

/// The checker of one run: the evaluator's `Machine` with `--ub`.
pub struct Checker<'db> {
    mem: Memory,
    borrows: Borrows,
    races: Races,
    sites: Sites<'db>,
    plans: Plans<'db>,
    /// Where the running thread is.
    at: SiteId,
    thread: ThreadId,
    /// Whether `exit` ran for the main thread before `finish`.
    main_exited: bool,
    /// By `AllocId::index`: where each allocation was made and freed, for reports.
    made_at: Vec<SiteId>,
    freed_at: Vec<SiteId>,
    leaks: bool,
    /// The atomic load `read` just checked, whose value `atomic_load` asks for.
    loaded: Option<(AllocId, Span)>,
    /// Whether the bytes the last atomic `write` overwrote were all initialized.
    overwrote_init: bool,
    found: Option<Found>,
    unsupported: Option<String>,
    runnable: Vec<ThreadId>,
}

impl<'db> Checker<'db> {
    /// A checker for a target whose pointers are `ptr_size` bytes, scheduling with `seed`.
    pub fn new(ptr_size: u64, config: &Config, seed: u64) -> Checker<'db> {
        Checker::with_plans(ptr_size, config, seed, Plans::default())
    }

    fn with_plans(ptr_size: u64, config: &Config, seed: u64, plans: Plans<'db>) -> Checker<'db> {
        Checker {
            mem: Memory::with_pointer_size(ptr_size),
            borrows: Borrows::new(),
            races: Races::new(&race::Config { seed, ..config.race }),
            sites: Sites::default(),
            plans,
            at: SiteId::START,
            thread: ThreadId::MAIN,
            main_exited: false,
            // alloc: setup, once per run: an empty `Vec` allocates nothing until used.
            made_at: Vec::new(),
            // alloc: setup, once per run: an empty `Vec` allocates nothing until used.
            freed_at: Vec::new(),
            leaks: config.leaks,
            loaded: None,
            overwrote_init: false,
            found: None,
            unsupported: None,
            // alloc: setup, once per run: an empty `Vec` allocates nothing until used.
            runnable: Vec::new(),
        }
    }

    /// The undefined behavior the run stopped at, if any.
    pub fn found(&self) -> Option<&Found> {
        self.found.as_ref()
    }

    /// What the checker could not check, if the run stopped for that.
    pub fn unsupported(&self) -> Option<&str> {
        self.unsupported.as_deref()
    }

    pub fn sites(&self) -> &Sites<'db> {
        &self.sites
    }

    /// Records the first undefined behavior; the hook returns the `Stop`.
    #[cold]
    fn fail(&mut self, ub: Ub) -> Stop {
        if self.found.is_none() && self.unsupported.is_none() {
            self.found = Some(Found { ub, thread: self.thread, at: self.at });
        }
        Stop
    }

    #[cold]
    fn stop_unsupported(&mut self, what: String) -> Stop {
        if self.found.is_none() && self.unsupported.is_none() {
            self.unsupported = Some(what);
        }
        Stop
    }

    /// Memory's check of an access: the allocation, offset and provenance it touches (`None`
    /// for a zero-sized one).
    #[inline]
    fn place(
        &mut self,
        ptr: Ptr<Prov>,
        size: u64,
        align: u64,
        kind: AccessKind,
        msg: AlignMsg,
    ) -> MResult<Option<(AllocId, u64, Prov)>> {
        match self.mem.access_as(ptr.addr, ptr.prov, size, align, kind, msg) {
            Ok(Some((alloc, off))) => {
                let prov = ptr.prov.expect("memory names an allocation only for a pointer with provenance");
                Ok(Some((alloc, off, prov)))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(self.fail(e.into())),
        }
    }

    /// Miri: machine.rs, `before_memory_read` (data race, then borrow tracker).
    #[inline]
    fn plain_access(&mut self, alloc: AllocId, prov: Prov, span: Span, kind: AccessKind) -> MResult<()> {
        self.loaded = None;
        let race = match kind {
            AccessKind::Read => self.races.read(alloc, span, None, self.at),
            AccessKind::Write => self.races.write(alloc, span, None, self.at),
        };
        if let Err(e) = race {
            return Err(self.fail(e.into()));
        }
        if let Err(e) = self.borrows.access(alloc, prov, kind, span, self.at) {
            return Err(self.fail(e.into()));
        }
        Ok(())
    }

    /// Miri: concurrency/data_race.rs, `read_scalar_atomic`, `write_scalar_atomic`,
    /// `atomic_rmw_op_immediate`: the access (Tree Borrows through `before_memory_*`, with the
    /// race detector held off by `allow_data_races_*`), then `validate_atomic_*`.
    fn atomic_access(
        &mut self,
        alloc: AllocId,
        prov: Prov,
        span: Span,
        kind: AccessKind,
        (ord, rmw, cxchg_failed): (AtomicOrd, bool, bool),
    ) -> MResult<()> {
        if let Err(e) = self.borrows.access(alloc, prov, kind, span, self.at) {
            return Err(self.fail(e.into()));
        }
        self.loaded = (kind == AccessKind::Read && !rmw && !cxchg_failed).then_some((alloc, span));
        // A failed compare-exchange reads the latest store (`atomic_compare_exchange_scalar`).
        let race = match (kind, rmw) {
            _ if cxchg_failed => self.races.failed_cxchg(alloc, span, ord, self.at),
            (AccessKind::Read, false) => self.races.read(alloc, span, Some(ord), self.at),
            (AccessKind::Read, true) => Ok(()),
            (AccessKind::Write, false) => self.races.write(alloc, span, Some(ord), self.at),
            (AccessKind::Write, true) => self.races.rmw(alloc, span, ord, self.at),
        };
        race.map_err(|e| self.fail(e.into()))
    }

    fn access(&mut self, ptr: Ptr<Prov>, size: u64, align: u64, access: Access, kind: AccessKind) -> MResult<Option<(AllocId, u64)>> {
        let msg = match access {
            Access::Plain => AlignMsg::BasedOn,
            Access::Atomic { .. } => AlignMsg::AccessedPtr,
        };
        let Some((alloc, off, prov)) = self.place(ptr, size, align, kind, msg)? else { return Ok(None) };
        let span = Span::new(off, size);
        match access {
            Access::Plain => self.plain_access(alloc, prov, span, kind)?,
            Access::Atomic { ord, rmw, cxchg_failed } => {
                self.atomic_access(alloc, prov, span, kind, (ord, rmw, cxchg_failed))?
            }
        }
        Ok(Some((alloc, off)))
    }

    /// The allocation and offset of `size` bytes an atomic access already checked.
    fn locate(&mut self, ptr: Ptr<Prov>, size: u64) -> MResult<Option<(AllocId, u64)>> {
        self.mem.access(ptr.addr, ptr.prov, size, 1, AccessKind::Read).map_err(|e| self.fail(e.into()))
    }

    /// An in-place argument or the return place of a call, after `call`: retagged with a strong
    /// protector, as `&mut` on function entry, then written uninitialized through the new tag.
    // Miri: machine.rs, `protect_in_place_function_argument`; tree_borrows/mod.rs,
    // `tb_protect_place` (every permission `ReservedFrz`, strongly protected: what
    // `NewPermission::new` gives a protected `&mut` to a `Freeze` and `Unpin` pointee).
    fn protect_place(&mut self, place: Ptr<Prov>, size: u64) -> MResult<()> {
        let Some((alloc, off)) = self.locate(place, size)? else { return Ok(()) };
        let prov = place.prov.expect("memory names an allocation only for a pointer with provenance");
        let span = Span::new(off, size);
        let perm = NewPermission::new(PtrKind::Mut, borrows::RetagMode::FnEntry, true, true, &[]);
        let tag = match self.borrows.retag(alloc, prov, perm, span, self.at) {
            Ok(Some(tag)) => tag,
            Ok(None) => return Ok(()),
            Err(e) => return Err(self.fail(e.into())),
        };
        for i in 0..self.borrows.retag_accesses().len() {
            let (span, access) = self.borrows.retag_accesses()[i];
            if let Err(e) = self.races.retag(alloc, span, access, self.at) {
                return Err(self.fail(e.into()));
            }
        }
        let new = Prov::Concrete { alloc, tag };
        self.plain_access(alloc, new, span, AccessKind::Write)?;
        self.mem.uninit(alloc, span);
        Ok(())
    }

    fn record(column: &mut Vec<SiteId>, alloc: AllocId, at: SiteId) {
        let i = alloc.index();
        if column.len() <= i {
            column.resize(i + 1, SiteId::START);
        }
        column[i] = at;
    }
}

fn kind(kind: MemKind) -> Kind {
    match kind {
        MemKind::Stack => Kind::Stack,
        MemKind::Rust => Kind::Rust,
        MemKind::C => Kind::C,
        MemKind::Global => Kind::Global,
        MemKind::ConstHeap => Kind::ConstHeap,
        MemKind::Machine => Kind::Machine,
        MemKind::Mmap => Kind::Mmap,
    }
}

impl<'db> Machine<'db> for Checker<'db> {
    type Prov = Prov;
    type Shadow = Shadow;

    #[inline]
    fn step(&mut self, at: Site<'db>) {
        self.at = self.sites.intern(at);
        // Miri: provenance_gc.rs, `run_provenance_gc`, every `-Zmiri-provenance-gc` retags.
        // The evaluator holds no shadow with provenance across a step, so memory's tags are
        // every tag still held.
        if self.borrows.gc_due() {
            let mem = &self.mem;
            self.borrows.gc(|live| mem.tags(&mut |_, tag| live.mark(tag)));
        }
    }

    // Miri: machine.rs, `init_alloc_extra`: the race detector's clocks (stack and heap memory
    // at the allocating thread's time, the rest at time zero), then the borrow tracker's tree.
    fn alloc(&mut self, addr: u64, size: u64, align: u64, mem_kind: MemKind, zeroed: bool) -> MResult<Prov> {
        let alloc = match self.mem.alloc(addr, size, align, kind(mem_kind), zeroed) {
            Ok(it) => it,
            Err(e) => return Err(self.fail(e.into())),
        };
        let stack_of = match mem_kind {
            MemKind::Stack | MemKind::Rust | MemKind::C | MemKind::Mmap => Some(self.thread),
            MemKind::Global | MemKind::ConstHeap | MemKind::Machine => None,
        };
        self.races.new_alloc(alloc, size, stack_of);
        let tag = self.borrows.new_alloc(alloc, size, self.at);
        Checker::record(&mut self.made_at, alloc, self.at);
        Ok(Prov::Concrete { alloc, tag })
    }

    // Miri: interpret/memory.rs, `deallocate_ptr`; machine.rs, `before_memory_deallocation`
    // (data race, then borrow tracker).
    fn free(&mut self, ptr: Ptr<Prov>, size: Option<u64>, align: Option<u64>, mem_kind: MemKind) -> MResult<()> {
        let alloc = match self.mem.free((ptr.addr, ptr.prov), size, align, kind(mem_kind)) {
            Ok(it) => it,
            Err(e) => return Err(self.fail(e.into())),
        };
        let prov = ptr.prov.expect("memory frees only through a pointer with provenance");
        let size = self.mem.info(alloc).size;
        Checker::record(&mut self.freed_at, alloc, self.at);
        if let Err(e) = self.races.free(alloc, size, self.at) {
            return Err(self.fail(e.into()));
        }
        self.borrows.free(alloc, prov, size, self.at).map_err(|e| self.fail(e.into()))
    }

    fn root(&mut self, addr: u64) -> Option<Prov> {
        let alloc = self.mem.live_at(addr)?;
        let tag = self.borrows.root(alloc)?;
        Some(Prov::Concrete { alloc, tag })
    }

    #[inline]
    fn read(&mut self, ptr: Ptr<Prov>, size: u64, align: u64, access: Access) -> MResult<Shadow> {
        Ok(match self.access(ptr, size, align, access, AccessKind::Read)? {
            Some((alloc, off)) => Shadow(self.mem.read(alloc, Span::new(off, size))),
            None => Shadow(Bytes::init(0)),
        })
    }

    #[inline]
    fn write(&mut self, ptr: Ptr<Prov>, size: u64, align: u64, access: Access, shadow: &Shadow) -> MResult<()> {
        debug_assert_eq!(shadow.0.len() as u64, size, "a write's shadow is its size");
        if let Some((alloc, off)) = self.access(ptr, size, align, access, AccessKind::Write)? {
            if let Access::Atomic { rmw, .. } = access {
                self.overwrote_init = rmw || self.mem.require_init(alloc, Span::new(off, size)).is_ok();
            }
            self.mem.write(alloc, off, &shadow.0);
        }
        Ok(())
    }

    // Miri: interpret/memory.rs, `mem_copy_repeatedly`: both sides checked (the source read,
    // the destination written), then each side's `before_memory_*` hook, then the overlap check.
    fn copy(&mut self, dst: Ptr<Prov>, src: Ptr<Prov>, size: u64, align: u64, overlapping: bool) -> MResult<()> {
        let from = self.place(src, size, align, AccessKind::Read, AlignMsg::BasedOn)?;
        let to = self.place(dst, size, align, AccessKind::Write, AlignMsg::BasedOn)?;
        let (Some((salloc, soff, sprov)), Some((dalloc, doff, dprov))) = (from, to) else { return Ok(()) };
        self.plain_access(salloc, sprov, Span::new(soff, size), AccessKind::Read)?;
        self.plain_access(dalloc, dprov, Span::new(doff, size), AccessKind::Write)?;
        if !overlapping && let Err(e) = self.mem.check_nonoverlapping((dalloc, doff), (salloc, soff), size) {
            return Err(self.fail(e.into()));
        }
        self.mem.copy((dalloc, doff), (salloc, soff), size);
        Ok(())
    }

    // Miri: interpret/operand.rs, `read_scalar` of an immediate (`Scalar::Uninit` fails with
    // `InvalidUninitBytes`).
    fn require_init(&mut self, shadow: &Shadow, range: Range<usize>) -> MResult<()> {
        shadow.0.require_init(range).map_err(|e| self.fail(e.into()))
    }

    // Miri: concurrency/weak_memory.rs, `buffered_atomic_read`. A read-modify-write reads the
    // latest store (`buffered_atomic_rmw`), so only a load may read another.
    fn atomic_load(&mut self, ptr: Ptr<Prov>, size: u64, _ord: AtomicOrd, _latest: &[u8]) -> MResult<Option<Vec<u8>>> {
        let Some((alloc, off)) = self.locate(ptr, size)? else { return Ok(None) };
        let span = Span::new(off, size);
        if self.loaded.take() != Some((alloc, span)) {
            return Ok(None);
        }
        match self.races.load_value(alloc, span) {
            Ok(value) => Ok(value.map(<[u8]>::to_vec)),
            Err(e) => Err(self.fail(e.into())),
        }
    }

    // Miri: concurrency/weak_memory.rs, `buffered_atomic_write`: a location's first atomic
    // store keeps what it held before as the store before it (`None`: not all initialized).
    fn atomic_store(&mut self, ptr: Ptr<Prov>, ord: AtomicOrd, bytes: &[u8], old: &[u8]) -> MResult<()> {
        let size = bytes.len() as u64;
        let Some((alloc, off)) = self.locate(ptr, size)? else { return Ok(()) };
        let old = self.overwrote_init.then_some(old);
        self.races.store_value(alloc, Span::new(off, size), ord, bytes, old);
        Ok(())
    }

    // Miri: intrinsics/atomic.rs, `atomic_fence` and `compiler_fence` (a compiler fence does
    // nothing at run time).
    fn fence(&mut self, ord: AtomicOrd, compiler_only: bool) -> MResult<()> {
        if !compiler_only {
            self.races.fence(ord);
        }
        Ok(())
    }

    fn typed(&mut self, cx: &TypedCx<'_, 'db>, place: Ptr<Prov>, ty: Ty<'db>, bytes: &[u8], mode: RetagMode) -> MResult<()> {
        let loc = if bytes.is_empty() {
            None
        } else {
            self.locate(place, bytes.len() as u64)?
        };
        let parts = valid::Parts { mem: &mut self.mem, borrows: &mut self.borrows, races: &mut self.races, site: self.at };
        match valid::typed(&mut self.plans, cx, parts, loc, ty, bytes, mode) {
            Ok(()) => Ok(()),
            Err(Fail::Ub(ub)) => Err(self.fail(ub)),
            Err(Fail::Unsupported(what)) => Err(self.stop_unsupported(what)),
        }
    }

    // Miri: intrinsics.rs, `ptr_offset_inbounds`.
    fn offset(&mut self, ptr: Ptr<Prov>, by: i64) -> MResult<()> {
        self.mem.offset_inbounds(ptr.addr, ptr.prov, by).map_err(|e| self.fail(e.into()))
    }

    fn offset_from(&mut self, a: Ptr<Prov>, b: Ptr<Prov>, unsigned: bool) -> MResult<()> {
        offset::check(&self.mem, (a.addr, a.prov), (b.addr, b.prov), unsigned)
            .map_err(|e| self.fail(Ub::OffsetFrom(e)))
    }

    // Miri: alloc_addresses/mod.rs, `expose_provenance`; borrow_tracker `expose_tag`.
    fn expose(&mut self, ptr: Ptr<Prov>) -> MResult<()> {
        if let Some(Prov::Concrete { alloc, tag }) = ptr.prov {
            self.mem.expose(alloc, tag);
            self.borrows.expose(alloc, tag);
        }
        Ok(())
    }

    // Miri: alloc_addresses/mod.rs, `ptr_from_addr_cast` under permissive provenance: a
    // wildcard, resolved at each access to the exposed allocation the address is in.
    fn from_exposed(&mut self, _addr: u64) -> Option<Prov> {
        Some(Prov::Wildcard)
    }

    // Miri: intern.rs, `intern_const_alloc_recursive`: interned globals are `Mutability::Not`.
    fn freeze(&mut self, addr: u64) {
        if let Some(alloc) = self.mem.live_at(addr) {
            self.mem.set_immutable(alloc);
        }
    }

    // Miri: a static is a global allocation, its value in it before any thread runs
    // (`VClockAlloc::new_allocation` at time zero); the evaluator lays it out on first use.
    fn initialized(&mut self, addr: u64) {
        if let Some(alloc) = self.mem.live_at(addr) {
            let size = self.mem.info(alloc).size;
            self.races.initialized(alloc, size);
        }
    }

    // Miri: machine.rs, `after_stack_push` and `before_stack_pop` (`on_stack_pop`).
    fn call(&mut self) -> MResult<()> {
        self.borrows.push_frame();
        Ok(())
    }

    fn protect(&mut self, place: Ptr<Prov>, size: u64) -> MResult<()> {
        self.protect_place(place, size)
    }

    fn ret(&mut self) -> MResult<()> {
        self.borrows.pop_frame(self.at).map_err(|e| self.fail(e.into()))
    }

    fn spawn(&mut self, thread: EvalThread) -> MResult<()> {
        let child = self.races.spawn();
        if child != ThreadId(thread) {
            // alloc: error path: the message of a failure.
            return Err(self.stop_unsupported(format!(
                "the evaluator numbered a new thread {thread}, the race detector {}: both count \
                 threads in creation order from the main thread's 0",
                child.0
            )));
        }
        Ok(())
    }

    fn schedule(&mut self, _running: EvalThread, runnable: &[EvalThread]) -> MResult<EvalThread> {
        self.runnable.clear();
        self.runnable.extend(runnable.iter().map(|&t| ThreadId(t)));
        match self.races.schedule(self.thread, &self.runnable) {
            Ok(next) => {
                self.thread = next;
                self.borrows.set_thread(next);
                Ok(next.0)
            }
            Err(deadlock) => Err(self.fail(Ub::Deadlock(deadlock))),
        }
    }

    fn join(&mut self, joined: EvalThread) -> MResult<()> {
        self.races.join(ThreadId(joined)).map_err(|e| self.fail(e.into()))
    }

    // Miri: concurrency/thread.rs, `detach_thread`.
    fn detach(&mut self, thread: EvalThread) -> MResult<()> {
        self.races.detach(ThreadId(thread)).map_err(|e| self.fail(e.into()))
    }

    fn exit(&mut self) -> MResult<()> {
        self.races.exit();
        self.main_exited |= self.thread == ThreadId::MAIN;
        Ok(())
    }

    // Miri: concurrency/thread.rs, `yield_active_thread`.
    fn yield_now(&mut self) {
        self.races.yield_active();
    }

    fn thread_name(&mut self, thread: EvalThread, name: &[u8]) {
        self.races.set_thread_name(ThreadId(thread), name);
    }

    fn release(&mut self, object: u64) -> MResult<()> {
        self.races.release(object);
        Ok(())
    }

    fn acquire(&mut self, object: u64) -> MResult<()> {
        self.races.acquire(object);
        Ok(())
    }

    // Miri: eval.rs, `eval_entry`: with the leak check on, threads still running when the main
    // thread returns are an error, and otherwise `report_leaks`.
    fn finish(&mut self) -> MResult<()> {
        if !self.leaks {
            return Ok(());
        }
        if !self.main_exited {
            self.races.exit();
            self.main_exited = true;
        }
        if !self.races.all_terminated() {
            return Err(self.fail(Ub::ThreadsLeft));
        }
        self.mem.leaks().map_err(|leaks| self.fail(Ub::Leak(leaks)))
    }
}

/// Runs the test `test` (a `fn()`) under the checker, once per schedule of `config`, and says
/// what the first schedule that did not pass found.
pub fn run_test<'db>(db: &'db Db, test: FunctionId, config: &Config) -> Outcome {
    let krate = test.krate(db);
    let ptr_size = match db.target_data_layout(krate) {
        Ok(target) => target.pointer_size().bytes(),
        // alloc: the edge: the outcome of a run.
        Err(e) => return Outcome::Error { message: format!("no target data layout: {e:?}"), seed: config.seed },
    };
    let schedules = config.schedules.max(1);
    let mut plans = Plans::default();
    for i in 0..schedules {
        let seed = config.seed.wrapping_add(u64::from(i));
        let checker = Checker::with_plans(ptr_size, config, seed, plans);
        let (result, _output, mut checker) = hir_ty_mir::mir::run_function(db, test, checker);
        if let Some(outcome) = outcome(db, &checker, result, seed) {
            return outcome;
        }
        plans = std::mem::take(&mut checker.plans);
    }
    Outcome::Passed { schedules }
}

/// The outcome of one schedule; `None` when it passed.
fn outcome<'db>(
    db: &'db Db,
    checker: &Checker<'db>,
    result: Result<(), MirEvalError<'db>>,
    seed: u64,
) -> Option<Outcome> {
    // clone: the edge: the outcome of a run.
    let found = checker.found.clone();
    // alloc: the edge: the outcome of a run.
    let mut stack = Vec::new();
    let error = match &result {
        Ok(()) => None,
        Err(e) => Some(innermost(e, &mut stack)),
    };
    if let Some(found) = found {
        let report = report::render(db, checker, &found, &stack);
        return Some(Outcome::Ub { found, report, seed });
    }
    if let Some(what) = &checker.unsupported {
        // clone: the edge: the outcome of a run.
        return Some(Outcome::Unsupported { what: what.clone() });
    }
    Some(match error? {
        // clone: the edge: the outcome of a run.
        MirEvalError::Panic(message) => Outcome::Failed { message: message.clone(), seed },
        // clone: the edge: the outcome of a run. An abort fails the test, as under rustc's harness.
        MirEvalError::Aborted(message) => Outcome::Failed { message: message.clone(), seed },
        // clone: the edge: the outcome of a run.
        MirEvalError::NotSupported(what) => Outcome::Unsupported { what: what.clone() },
        MirEvalError::MachineStop => Outcome::Error {
            // alloc: the edge: the outcome of a run.
            message: "the evaluator stopped for the checker, which recorded nothing".to_owned(),
            seed,
        },
        MirEvalError::UndefinedBehavior(what) => {
            // alloc: the edge: the outcome of a run.
            Outcome::Error { message: format!("Undefined Behavior (found by the evaluator): {what}"), seed }
        }
        // alloc: the edge: the outcome of a run.
        other => Outcome::Error { message: format!("{other:?}"), seed },
    })
}

/// A frame of the call stack: the function (or closure) and where in its body it was.
pub(crate) type Frame<'db> = (either::Either<FunctionId, hir_ty::db::InternedClosureId<'db>>, MirSpan, hir_ty::InferBodyId<'db>);

/// The error under `InFunction`s, with their frames innermost first in `stack`.
fn innermost<'e, 'db>(mut error: &'e MirEvalError<'db>, stack: &mut Vec<Frame<'db>>) -> &'e MirEvalError<'db> {
    // alloc: the edge: the outcome of a run.
    let mut levels = Vec::new();
    while let MirEvalError::InFunction(inner, frames) = error {
        levels.push(frames);
        error = inner;
    }
    for frames in levels.into_iter().rev() {
        stack.extend(frames.iter().copied());
    }
    error
}
