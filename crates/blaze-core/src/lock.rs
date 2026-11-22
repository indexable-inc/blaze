//! blaze's one lock home (rule `standard-structures`, docs/v2/plan/design/named-locks.md): a
//! mutex [`Lock`], a reader-writer lock [`ReadLock`] and a condition [`Signal`], each named at
//! construction (`Lock::new("store.packs", ..)`) and built on blaze's own word locks
//! ([`raw`]: a futex word on Linux, the wait on an address on macOS). Every other lock in the
//! tree is banned in `.clippy.toml`.
//!
//! The fast path is [`raw`]'s: an uncontended acquire is its one compare-and-swap, and a
//! release adds one relaxed load of a counter on the same cache line. Nothing reads a clock,
//! a thread-local or the profiler until an acquire has to wait. Then, on the slow path only:
//!
//! - the wait opens the profiler's `wait` span naming the lock (`every-wait-says-what`);
//! - the waiter's start and end come from the [`Probe`]'s clock (the profiler's, or a
//!   simulation's virtual one), with its span, thread and call site;
//! - the holder, seeing at its release that someone waits, leaves its span, thread, site and
//!   how long it held the lock in the lock's contention slot, and the waiter that takes the
//!   lock next reads it;
//! - the waiter hands one [`LockWait`] to [`Probe::record`].
//!
//! A holder that took the lock without waiting has no acquire time (that would put a clock read
//! on the fast path), so its hold is counted from when the first of the current waiters began
//! ([`Held::whole`] is false); a holder that waited knows its acquire time, and its hold is
//! whole. Under contention that never lets the waiter count reach zero, that first waiter can
//! be long gone, so such a hold is an upper bound. A writer waiting behind readers has no
//! holder stamp (shared holders are not stamped), so the wait-for graph draws no edge for it.
//!
//! The probe is the process's ([`install`], which blaze-profile calls when it starts) or the
//! lock's own ([`Lock::probed`], for a simulation). [`Lock::enqueue`] is the slow path split in
//! two halves that never block, so a deterministic simulation drives the same recording code a
//! real wait runs.
//!
//! **Every contended acquire is metered, with or without a probe** (D171, D179; docs/v2/plan/
//! design/lock-policy.md, "A lock earns its place"): on its way out the waiter hands its lock's
//! name, how it waited (a reader behind a writer, a writer behind readers), its wait and the
//! holder it waited behind (the holder's task, [`blaze_meter::doing`], and its hold) to
//! [`blaze_meter::lock_waited`], into the thread's lock table, which a task's `Used` and the
//! profile's `locks:` section read. Without a probe the clock is the lock's own: an [`Instant`]
//! its contention slot took when it was made, read only on a slow path. A lock with no
//! contention has no slot and pays nothing.
//!
//! THE FAST PATH, unchanged (rule 67): `lock`, `read`, `write` and their `try_` forms are the
//! raw lock's one compare-and-swap and the owner stamp (one relaxed store); a guard's drop is
//! one relaxed store and one relaxed load of `waiting`. Everything this file records is behind
//! `lock_slow`, `read_slow`, `write_slow` (each `#[cold] #[inline(never)]`, entered only after
//! the CAS failed), `try_lock_for` after its failed try, `enqueue`, and `Meta::released`
//! (`#[cold]`, entered only when `waiting` is not zero): no added instruction on a take or a
//! release that nobody waited behind. Acquires are not counted, so a lock never contended is not
//! seen by the record (counting one would be an add on every take).

use std::{
    any::type_name,
    fmt,
    ops::{Deref, DerefMut},
    panic::Location,
    ptr,
    sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering},
    time::{Duration, Instant},
};

pub mod graph;
pub(crate) mod raw;
pub mod stall;
#[cfg(test)]
mod tests;

pub use self::graph::{set_thread_mark, thread_mark};

/// How a lock was asked for or held.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mode {
    /// A [`Lock`]'s.
    Lock,
    /// A [`ReadLock`]'s shared read.
    Read,
    /// A [`ReadLock`]'s exclusive write.
    Write,
}

impl Mode {
    pub const fn name(self) -> &'static str {
        match self {
            Mode::Lock => "lock",
            Mode::Read => "read",
            Mode::Write => "write",
        }
    }
}

/// What a wait span names: a lock's acquire, or a [`Signal`]'s wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Awaits {
    Lock(&'static str, Mode),
    Signal(&'static str),
}

/// The holder a waiter waited behind, as it left itself at its release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Held {
    pub site: &'static Location<'static>,
    pub mode: Mode,
    /// Its span ([`Probe::span`]), 0 for none.
    pub span: u64,
    /// Its thread ([`Probe::thread`]).
    pub thread: u64,
    /// How long it held the lock, to its release.
    pub hold_ns: u64,
    /// Whether `hold_ns` is from its acquire (it waited for the lock itself) or only from when
    /// the first of the current waiters began (it took the lock without waiting).
    pub whole: bool,
    /// The task its thread ran ([`blaze_meter::doing`]): `""` for a thread that runs none.
    pub doing: &'static str,
    /// How long it had held the lock when the first of the current waiters began: known only
    /// for a whole hold (`None` otherwise, as its acquire time was never read).
    pub before_ns: Option<u64>,
}

/// One acquire that had to wait: what the profile's `lock_waits` table holds a row of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LockWait {
    /// The lock's name, as its construction gave it.
    pub lock: &'static str,
    /// The type it guards (`std::any::type_name`), which `blaze debug locks` reads its
    /// recommendation from (a map, an integer, a queue).
    pub value_type: &'static str,
    pub mode: Mode,
    /// Where the waiter asked for it.
    pub site: &'static Location<'static>,
    /// The waiter's span, 0 for none.
    pub span: u64,
    pub thread: u64,
    /// When the wait began, on the probe's clock.
    pub start_ns: u64,
    pub wait_ns: u64,
    /// The holder it waited behind, when the holder saw it waiting before it let go.
    pub holder: Option<Held>,
}

/// Where locks record their waits: the profiler (blaze-profile's `locks::Recorder`) or a
/// simulation. Every method is called only on a slow path, never while an uncontended lock is
/// taken or let go.
pub trait Probe: Sync {
    /// Now, in nanoseconds on the probe's clock (the injected clock: the profiler's monotonic
    /// one, or a simulation's virtual one).
    fn now_ns(&self) -> u64;
    /// The current span of the calling thread, 0 for none.
    fn span(&self) -> u64;
    /// The calling thread, as the probe numbers threads.
    fn thread(&self) -> u64;
    /// Opens the span of a wait on `awaits`; its token is given back to [`Probe::wait_end`].
    fn wait_begin(&self, awaits: Awaits) -> u64;
    fn wait_end(&self, token: u64);
    /// Takes one wait. Called while the waiter holds the lock it waited for, so it must not
    /// take that lock: a lock on the recording path itself is [`Lock::unrecorded`].
    fn record(&self, wait: &LockWait);
    /// Opens the span of a file call that may wait for a device, made on a driver's thread
    /// off its loop ([`io_wait`]): `op` is what the call does (`clone`, `write back`) and
    /// `file` the file it is made on. Its token is given back to [`Probe::wait_end`]; `None`
    /// from a probe that names no such call (a test's), which is the default.
    fn io_begin(&self, op: &'static str, file: &str) -> Option<u64> {
        let _ = (op, file);
        None
    }
}

/// Runs `call`, a file call that may wait for a device (a clone, a write-back, a listing),
/// under a span of the process's probe that names `op` and `file` (rule
/// every-wait-says-what): where a driver's thread off its loop spends its time is then in the
/// profile by file, where before the loop's own wait was all that showed. With no probe
/// installed the call runs as it is.
pub fn io_wait<R>(op: &'static str, file: &str, call: impl FnOnce() -> R) -> R {
    let probe = installed();
    let token = probe.and_then(|it| it.io_begin(op, file));
    let result = call();
    if let (Some(probe), Some(token)) = (probe, token) {
        probe.wait_end(token);
    }
    result
}

/// The process's probe; null for none.
static PROBE: AtomicPtr<&'static dyn Probe> = AtomicPtr::new(ptr::null_mut());

/// Makes `probe` the process's: every lock without its own records into it from now on. A
/// later install replaces it (a test's, then the profiler's).
pub fn install(probe: &'static &'static dyn Probe) {
    PROBE.store(ptr::from_ref(probe).cast_mut(), Ordering::Release);
}

/// The process's probe, if one is installed.
pub fn installed() -> Option<&'static dyn Probe> {
    let probe = PROBE.load(Ordering::Acquire);
    (!probe.is_null()).then(|| *deref_static(probe))
}

#[expect(unsafe_code, reason = "reads back a `&'static` that `install` stored as a pointer")]
fn deref_static(probe: *mut &'static dyn Probe) -> &'static &'static dyn Probe {
    // SAFETY: `PROBE` holds null or a pointer made from a `&'static &'static dyn Probe` in
    // `install`, which lives for the program and is never written through.
    unsafe { &*probe }
}

/// What a lock keeps of its contention: made the first time an acquire of it waits (or when
/// it is made with its own probe), and only ever touched on a slow path.
struct Contention {
    /// The lock's own probe; `None` records into the process's.
    probe: Option<&'static dyn Probe>,
    /// When the first of the current waiters began, on the probe's clock.
    since_ns: AtomicU64,
    /// The last holder that let go while someone waited, for the next waiter to take.
    held: raw::Mutex<Option<Held>>,
    /// The lock's own clock when it has no probe: nanoseconds from this instant, taken when the
    /// slot was made (its first contended acquire), read only on a slow path.
    base: Instant,
    /// The control's planted bug ([`Lock::probed_planted`]); `None` in shipped code.
    plant: LockPlant,
}

impl Contention {
    fn new(probe: Option<&'static dyn Probe>, plant: LockPlant) -> Contention {
        Contention {
            probe,
            since_ns: AtomicU64::new(0),
            held: raw::Mutex::new(None),
            base: Instant::now(),
            plant,
        }
    }
}

/// How a waiter in `mode` waited behind a holder in `holder`, for the meter.
fn behind(mode: Mode, holder: Option<Mode>) -> blaze_meter::Behind {
    match (mode, holder) {
        (Mode::Read, _) => blaze_meter::Behind::ReadBehindWrite,
        (Mode::Write, Some(Mode::Read)) => blaze_meter::Behind::WriteBehindRead,
        _ => blaze_meter::Behind::Holder,
    }
}

/// What every lock carries besides its [`raw`] lock: its name, how many wait for it now,
/// and its contention slot.
struct Meta {
    name: &'static str,
    /// Acquires waiting for it now: what a release checks, with one relaxed load.
    waiting: AtomicU32,
    /// Its contention slot, null until the first wait (or set at construction).
    contention: AtomicPtr<Contention>,
    /// A lock on the recording path itself: it never records (it would record into itself).
    unrecorded: bool,
    /// The thread mark of its exclusive holder ([`thread_mark`]), 0 when free or held shared:
    /// the wait-for graph's edge from a waiter to who it waits behind. One relaxed store on
    /// acquire and one on release, to the cache line the acquire's CAS already owns.
    owner: AtomicU64,
}

impl Meta {
    const fn new(name: &'static str, unrecorded: bool) -> Meta {
        Meta {
            name,
            waiting: AtomicU32::new(0),
            contention: AtomicPtr::new(ptr::null_mut()),
            unrecorded,
            owner: AtomicU64::new(0),
        }
    }

    fn slot(&self) -> Option<&Contention> {
        let it = self.contention.load(Ordering::Acquire);
        (!it.is_null()).then(|| self.read_slot(it))
    }

    #[expect(unsafe_code, reason = "the slot is a leaked-until-drop box owned by this lock")]
    fn read_slot(&self, it: *mut Contention) -> &Contention {
        // SAFETY: a non-null `contention` came from `Box::into_raw` in `slot_or_make` (or
        // `with_slot`) and is freed only in `Drop for Meta`, which `&self` outlives.
        unsafe { &*it }
    }

    /// Stamps the calling thread as the exclusive holder.
    #[inline]
    fn stamp(&self) {
        self.owner.store(thread_mark(), Ordering::Relaxed);
    }

    /// The probe this lock records into, if any.
    fn probe(&self) -> Option<&'static dyn Probe> {
        if self.unrecorded {
            return None;
        }
        match self.slot().and_then(|it| it.probe) {
            Some(own) => Some(own),
            None => installed(),
        }
    }

    /// The contention slot, made on the first wait.
    fn slot_or_make(&self) -> &Contention {
        if let Some(it) = self.slot() {
            return it;
        }
        // alloc: once a lock's life, on its first contended acquire; freed with the lock.
        let made = Box::into_raw(Box::new(Contention::new(None, LockPlant::None)));
        match self.contention.compare_exchange(
            ptr::null_mut(),
            made,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => self.read_slot(made),
            Err(theirs) => {
                drop(take_box(made));
                self.read_slot(theirs)
            }
        }
    }

    /// Now on this lock's clock: the probe's, else the slot's own instant (at least 1, since 0
    /// means "taken without waiting" to a guard).
    fn now_ns(probe: Option<&'static dyn Probe>, slot: &Contention) -> u64 {
        match probe {
            Some(probe) => probe.now_ns(),
            None => u64::try_from(slot.base.elapsed().as_nanos()).unwrap_or(u64::MAX).max(1),
        }
    }

    /// The release of a guard taken at `site` in `mode` (`acquired_ns` 0: without waiting),
    /// when someone waits: leaves the holder in the slot for the next waiter, with or without a
    /// probe (the meter takes it).
    #[cold]
    #[inline(never)]
    fn released(&self, site: &'static Location<'static>, mode: Mode, acquired_ns: u64) {
        let probe = self.probe();
        let slot = self.slot_or_make();
        let now = Meta::now_ns(probe, slot);
        let since = slot.since_ns.load(Ordering::Relaxed);
        let (from, whole) = match acquired_ns {
            0 => (since, false),
            at => (at, true),
        };
        let held = Held {
            site,
            mode,
            span: probe.map_or(0, |probe| probe.span()),
            thread: probe.map_or(0, |probe| probe.thread()),
            hold_ns: now.saturating_sub(from),
            whole,
            doing: blaze_meter::doing(),
            before_ns: whole.then(|| since.saturating_sub(acquired_ns)),
        };
        *slot.held.lock() = Some(held);
    }
}

#[expect(unsafe_code, reason = "frees the slot `slot_or_make` boxed")]
fn take_box(it: *mut Contention) -> Box<Contention> {
    // SAFETY: `it` came from `Box::into_raw` and is freed exactly once: by the loser of the
    // race that made it, or by `Drop for Meta`.
    unsafe { Box::from_raw(it) }
}

impl Drop for Meta {
    fn drop(&mut self) {
        let it = *self.contention.get_mut();
        if !it.is_null() {
            drop(take_box(it));
        }
    }
}

/// A wait in progress: begun by [`Lock::enqueue`] (or a slow path), ended by taking the lock.
/// While it lives it counts as waiting, so the holder's release leaves itself for it.
struct Waiting<'a> {
    meta: &'a Meta,
    probe: Option<&'static dyn Probe>,
    site: &'static Location<'static>,
    mode: Mode,
    value_type: &'static str,
    start_ns: u64,
    span: u64,
    token: u64,
    /// Its row in the wait-for graph, while it waits: held for its drop, which takes the row
    /// out, and never read.
    _registered: Option<graph::Registered>,
    ended: bool,
}

/// A planted bug for the lock record's control (rule 53): set on a probed lock's slot only.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LockPlant {
    None,
    /// A contended acquire is charged to the meter under another lock's name.
    ChargesAnotherName,
}

impl crate::planted::Plants for LockPlant {}

/// The name a [`LockPlant::ChargesAnotherName`] lock charges its waits under.
pub const PLANTED_OTHER_NAME: &str = "lock.planted.another";

impl<'a> Waiting<'a> {
    fn begin(
        meta: &'a Meta,
        site: &'static Location<'static>,
        mode: Mode,
        value_type: &'static str,
    ) -> Waiting<'a> {
        let probe = meta.probe();
        // The slot and the waiting count, with or without a probe: the holder's release leaves
        // itself for the meter too.
        let slot = meta.slot_or_make();
        let start_ns = Meta::now_ns(probe, slot);
        if meta.waiting.fetch_add(1, Ordering::Relaxed) == 0 {
            slot.since_ns.store(start_ns, Ordering::Relaxed);
        }
        let (span, token) = match probe {
            None => (0, 0),
            Some(probe) => (probe.span(), probe.wait_begin(Awaits::Lock(meta.name, mode))),
        };
        // With no probe there is no clock to age a wait by, so the graph does not see it: the
        // daemon, whose watchdog reads the graph, always has the profiler's probe installed.
        let registered = probe.map(|_| graph::register(
            graph::lock(meta.name, mode, ptr::from_ref(meta).addr()),
            span,
            start_ns,
            None,
            Some(site),
            Some(&meta.owner),
        ));
        Waiting {
            meta,
            probe,
            site,
            mode,
            value_type,
            start_ns,
            span,
            token,
            _registered: registered,
            ended: false,
        }
    }

    /// The wait ended at `now` (taken, or given up), behind `holder`: its record in the
    /// thread's lock table and meter ([`blaze_meter::lock_waited`]), under the lock's name.
    fn charge(&self, slot: &Contention, now: u64, holder: Option<Held>) {
        let name = match slot.plant {
            LockPlant::None => self.meta.name,
            LockPlant::ChargesAnotherName => PLANTED_OTHER_NAME,
        };
        let metered = holder.map(|it| blaze_meter::LockHolder { doing: it.doing, hold_ns: it.hold_ns, whole: it.whole });
        blaze_meter::lock_waited(name, behind(self.mode, holder.map(|it| it.mode)), now.saturating_sub(self.start_ns), metered);
    }

    /// The lock taken: ends the span, records the wait, and gives the guard's acquire time.
    fn taken(mut self) -> u64 {
        self.ended = true;
        self._registered = None;
        if let Some(probe) = self.probe {
            probe.wait_end(self.token);
        }
        self.meta.waiting.fetch_sub(1, Ordering::Relaxed);
        let slot = self.meta.slot_or_make();
        let now = Meta::now_ns(self.probe, slot);
        let holder = slot.held.lock().take();
        self.charge(slot, now, holder);
        if let Some(probe) = self.probe {
            probe.record(&LockWait {
                lock: self.meta.name,
                value_type: self.value_type,
                mode: self.mode,
                site: self.site,
                span: self.span,
                thread: probe.thread(),
                start_ns: self.start_ns,
                wait_ns: now.saturating_sub(self.start_ns),
                holder,
            });
        }
        // 0 means "took it without waiting" to a release.
        now.max(1)
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if self.ended {
            return;
        }
        // Given up (a timed acquire that timed out, a simulation's dropped wait): a wait with no
        // holder, charged all the same.
        if let Some(probe) = self.probe {
            probe.wait_end(self.token);
        }
        self.meta.waiting.fetch_sub(1, Ordering::Relaxed);
        let slot = self.meta.slot_or_make();
        let now = Meta::now_ns(self.probe, slot);
        self.charge(slot, now, None);
    }
}

// --- Lock -------------------------------------------------------------------------------------

/// A named mutex: a [`raw::Mutex`] whose waits the profile sees.
pub struct Lock<T: ?Sized> {
    meta: Meta,
    raw: raw::Mutex<T>,
}

/// The guard of a [`Lock`].
#[must_use = "a lock is let go when its guard drops"]
pub struct LockGuard<'a, T: ?Sized> {
    meta: &'a Meta,
    site: &'static Location<'static>,
    /// When it was taken, on the probe's clock; 0 when it was taken without waiting.
    acquired_ns: u64,
    inner: raw::MutexGuard<'a, T>,
}

impl<T> Lock<T> {
    /// A lock named `name` (what the profile and `blaze debug locks` call it: say what it
    /// guards, `"store.packs"`, `"sched.ready"`) holding `value`.
    pub const fn new(name: &'static str, value: T) -> Lock<T> {
        let raw = raw::Mutex::new(value);
        Lock { meta: Meta::new(name, false), raw }
    }

    /// A lock on the recording path itself (the profiler's), which never records its waits:
    /// recording them would take it again.
    pub const fn unrecorded(name: &'static str, value: T) -> Lock<T> {
        let raw = raw::Mutex::new(value);
        Lock { meta: Meta::new(name, true), raw }
    }

    /// A lock that records into `probe` rather than the process's: a simulation's.
    pub fn probed(name: &'static str, value: T, probe: &'static dyn Probe) -> Lock<T> {
        Lock::probed_planted(name, value, probe, LockPlant::None)
    }

    /// [`Lock::probed`] with a planted bug in its record: the lock record's control.
    pub fn probed_planted(name: &'static str, value: T, probe: &'static dyn Probe, plant: LockPlant) -> Lock<T> {
        let lock = Lock::new(name, value);
        // alloc: a simulation's lock, made with its slot.
        let slot = Box::into_raw(Box::new(Contention::new(Some(probe), plant)));
        lock.meta.contention.store(slot, Ordering::Release);
        lock
    }

    pub fn into_inner(self) -> T {
        self.raw.into_inner()
    }
}

impl<T: ?Sized> Lock<T> {
    pub fn name(&self) -> &'static str {
        self.meta.name
    }

    /// Allocate the contention slot up front so the first contended acquire on a hot path makes
    /// no heap allocation.
    pub(crate) fn prewarm_contention(&self) {
        let _ = self.meta.slot_or_make();
    }

    /// The lock, waiting for it if another holds it; a wait is recorded.
    #[inline]
    #[track_caller]
    pub fn lock(&self) -> LockGuard<'_, T> {
        let site = Location::caller();
        match self.raw.try_lock() {
            Some(inner) => {
                self.meta.stamp();
                LockGuard { meta: &self.meta, site, acquired_ns: 0, inner }
            }
            None => self.lock_slow(site),
        }
    }

    #[cold]
    #[inline(never)]
    fn lock_slow(&self, site: &'static Location<'static>) -> LockGuard<'_, T> {
        let waiting = Waiting::begin(&self.meta, site, Mode::Lock, type_name::<T>());
        let inner = self.raw.lock();
        self.meta.stamp();
        let acquired_ns = waiting.taken();
        LockGuard { meta: &self.meta, site, acquired_ns, inner }
    }

    /// The lock if no one holds it, without waiting (and so without a record).
    #[inline]
    #[track_caller]
    pub fn try_lock(&self) -> Option<LockGuard<'_, T>> {
        let site = Location::caller();
        self.raw.try_lock().map(|inner| {
            self.meta.stamp();
            LockGuard { meta: &self.meta, site, acquired_ns: 0, inner }
        })
    }

    /// The lock, waiting at most `most` for it; a wait that ends with the lock is recorded (one
    /// that times out opens and closes its span, and is charged to the meter with no holder).
    #[track_caller]
    pub fn try_lock_for(&self, most: Duration) -> Option<LockGuard<'_, T>> {
        let site = Location::caller();
        if let Some(inner) = self.raw.try_lock() {
            self.meta.stamp();
            return Some(LockGuard { meta: &self.meta, site, acquired_ns: 0, inner });
        }
        let waiting = Waiting::begin(&self.meta, site, Mode::Lock, type_name::<T>());
        let inner = self.raw.try_lock_for(most)?;
        self.meta.stamp();
        let acquired_ns = waiting.taken();
        Some(LockGuard { meta: &self.meta, site, acquired_ns, inner })
    }

    /// Begins a wait for the lock without blocking: the slow path's first half, for a
    /// simulation to drive (it steps the holder, then [`Queued::take`]s). The wait counts as
    /// waiting from now, as a blocked acquire's does.
    #[track_caller]
    pub fn enqueue(&self) -> Queued<'_, T> {
        let waiting = Waiting::begin(&self.meta, Location::caller(), Mode::Lock, type_name::<T>());
        Queued { lock: self, waiting }
    }

    pub fn is_locked(&self) -> bool {
        self.raw.is_locked()
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.raw.get_mut()
    }

    pub fn data_ptr(&self) -> *mut T {
        self.raw.data_ptr()
    }
}

/// A wait for a [`Lock`] begun by [`Lock::enqueue`].
pub struct Queued<'a, T: ?Sized> {
    lock: &'a Lock<T>,
    waiting: Waiting<'a>,
}

impl<'a, T: ?Sized> Queued<'a, T> {
    /// The lock if it is free now, ending the wait and recording it; else the wait goes on.
    pub fn take(self) -> Result<LockGuard<'a, T>, Queued<'a, T>> {
        match self.lock.raw.try_lock() {
            None => Err(self),
            Some(inner) => {
                let Queued { lock, waiting } = self;
                lock.meta.stamp();
                let site = waiting.site;
                let acquired_ns = waiting.taken();
                Ok(LockGuard { meta: &lock.meta, site, acquired_ns, inner })
            }
        }
    }
}

impl<T: ?Sized> Drop for LockGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        self.meta.owner.store(0, Ordering::Relaxed);
        if self.meta.waiting.load(Ordering::Relaxed) != 0 {
            self.meta.released(self.site, Mode::Lock, self.acquired_ns);
        }
    }
}

impl<T: ?Sized> Deref for LockGuard<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T: ?Sized> DerefMut for LockGuard<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for LockGuard<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for Lock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut it = f.debug_struct("Lock");
        it.field("name", &self.meta.name);
        match self.raw.try_lock() {
            Some(held) => it.field("value", &&*held),
            None => it.field("value", &format_args!("<locked>")),
        };
        it.finish()
    }
}

// --- ReadLock ---------------------------------------------------------------------------------

/// A named reader-writer lock: a [`raw::RwLock`] whose waits the profile sees. Reach for it
/// only where reads hold the lock long; a value read often and written rarely is an arc-swap
/// or RCU, never a `ReadLock` (`blaze debug locks` says which).
pub struct ReadLock<T: ?Sized> {
    meta: Meta,
    raw: raw::RwLock<T>,
}

/// A shared read of a [`ReadLock`].
#[must_use = "a lock is let go when its guard drops"]
pub struct ReadGuard<'a, T: ?Sized> {
    meta: &'a Meta,
    site: &'static Location<'static>,
    acquired_ns: u64,
    inner: raw::RwLockReadGuard<'a, T>,
}

/// The exclusive write of a [`ReadLock`].
#[must_use = "a lock is let go when its guard drops"]
pub struct WriteGuard<'a, T: ?Sized> {
    meta: &'a Meta,
    site: &'static Location<'static>,
    acquired_ns: u64,
    inner: raw::RwLockWriteGuard<'a, T>,
}

impl<T> ReadLock<T> {
    /// A lock named `name` holding `value`; see [`Lock::new`].
    pub const fn new(name: &'static str, value: T) -> ReadLock<T> {
        let raw = raw::RwLock::new(value);
        ReadLock { meta: Meta::new(name, false), raw }
    }

    /// A lock on the recording path itself; see [`Lock::unrecorded`].
    pub const fn unrecorded(name: &'static str, value: T) -> ReadLock<T> {
        let raw = raw::RwLock::new(value);
        ReadLock { meta: Meta::new(name, true), raw }
    }

    pub fn into_inner(self) -> T {
        self.raw.into_inner()
    }
}

impl<T: ?Sized> ReadLock<T> {
    pub fn name(&self) -> &'static str {
        self.meta.name
    }

    #[inline]
    #[track_caller]
    pub fn read(&self) -> ReadGuard<'_, T> {
        let site = Location::caller();
        match self.raw.try_read() {
            Some(inner) => ReadGuard { meta: &self.meta, site, acquired_ns: 0, inner },
            None => self.read_slow(site),
        }
    }

    #[cold]
    #[inline(never)]
    fn read_slow(&self, site: &'static Location<'static>) -> ReadGuard<'_, T> {
        let waiting = Waiting::begin(&self.meta, site, Mode::Read, type_name::<T>());
        let inner = self.raw.read();
        let acquired_ns = waiting.taken();
        ReadGuard { meta: &self.meta, site, acquired_ns, inner }
    }

    #[inline]
    #[track_caller]
    pub fn write(&self) -> WriteGuard<'_, T> {
        let site = Location::caller();
        match self.raw.try_write() {
            Some(inner) => {
                self.meta.stamp();
                WriteGuard { meta: &self.meta, site, acquired_ns: 0, inner }
            }
            None => self.write_slow(site),
        }
    }

    #[cold]
    #[inline(never)]
    fn write_slow(&self, site: &'static Location<'static>) -> WriteGuard<'_, T> {
        let waiting = Waiting::begin(&self.meta, site, Mode::Write, type_name::<T>());
        let inner = self.raw.write();
        self.meta.stamp();
        let acquired_ns = waiting.taken();
        WriteGuard { meta: &self.meta, site, acquired_ns, inner }
    }

    #[inline]
    #[track_caller]
    pub fn try_read(&self) -> Option<ReadGuard<'_, T>> {
        let site = Location::caller();
        self.raw.try_read().map(|inner| ReadGuard { meta: &self.meta, site, acquired_ns: 0, inner })
    }

    #[inline]
    #[track_caller]
    pub fn try_write(&self) -> Option<WriteGuard<'_, T>> {
        let site = Location::caller();
        self.raw.try_write().map(|inner| {
            self.meta.stamp();
            WriteGuard { meta: &self.meta, site, acquired_ns: 0, inner }
        })
    }

    #[inline]
    #[track_caller]
    pub fn try_read_for(&self, timeout: Duration) -> Option<ReadGuard<'_, T>> {
        let site = Location::caller();
        if let Some(inner) = self.raw.try_read() {
            return Some(ReadGuard { meta: &self.meta, site, acquired_ns: 0, inner });
        }
        let waiting = Waiting::begin(&self.meta, site, Mode::Read, type_name::<T>());
        let inner = self.raw.try_read_for(timeout)?;
        let acquired_ns = waiting.taken();
        Some(ReadGuard { meta: &self.meta, site, acquired_ns, inner })
    }

    #[inline]
    #[track_caller]
    pub fn try_write_for(&self, timeout: Duration) -> Option<WriteGuard<'_, T>> {
        let site = Location::caller();
        if let Some(inner) = self.raw.try_write() {
            self.meta.stamp();
            return Some(WriteGuard { meta: &self.meta, site, acquired_ns: 0, inner });
        }
        let waiting = Waiting::begin(&self.meta, site, Mode::Write, type_name::<T>());
        let inner = self.raw.try_write_for(timeout)?;
        self.meta.stamp();
        let acquired_ns = waiting.taken();
        Some(WriteGuard { meta: &self.meta, site, acquired_ns, inner })
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.raw.get_mut()
    }

    pub fn is_locked(&self) -> bool {
        self.raw.is_locked()
    }
}

impl<T: ?Sized> Drop for ReadGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        if self.meta.waiting.load(Ordering::Relaxed) != 0 {
            self.meta.released(self.site, Mode::Read, self.acquired_ns);
        }
    }
}

impl<T: ?Sized> Drop for WriteGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        self.meta.owner.store(0, Ordering::Relaxed);
        if self.meta.waiting.load(Ordering::Relaxed) != 0 {
            self.meta.released(self.site, Mode::Write, self.acquired_ns);
        }
    }
}

impl<T: ?Sized> Deref for ReadGuard<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T: ?Sized> Deref for WriteGuard<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T: ?Sized> DerefMut for WriteGuard<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for ReadLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut it = f.debug_struct("ReadLock");
        it.field("name", &self.meta.name);
        match self.raw.try_read() {
            Some(held) => it.field("value", &&*held),
            None => it.field("value", &format_args!("<locked>")),
        };
        it.finish()
    }
}

// --- Signal -----------------------------------------------------------------------------------

/// Runs `wait`, a wait for a notify of the signal `name` (a [`Signal`]'s, a
/// [`crate::chan`]'s), inside its span and wait-graph row; `left` is the time from now to its
/// deadline (`None`: it has none), which the row says on the probe's clock.
pub(crate) fn signal_span<R>(
    name: &'static str,
    left: Option<Duration>,
    wait: impl FnOnce() -> R,
) -> R {
    let probe = installed();
    let token = probe.map(|it| it.wait_begin(Awaits::Signal(name)));
    let registered = probe.map(|probe| {
        let now_ns = probe.now_ns();
        let deadline_ns = left
            .map(|it| now_ns.saturating_add(u64::try_from(it.as_nanos()).unwrap_or(u64::MAX)));
        graph::register(stall::Blocked::Signal(name), probe.span(), now_ns, deadline_ns, None, None)
    });
    let result = wait();
    drop(registered);
    if let (Some(probe), Some(token)) = (probe, token) {
        probe.wait_end(token);
    }
    result
}

/// A named condition on a [`Lock`]: a [`raw::Cond`], each wait inside a span naming it. Only
/// a driver thread or the pool's idle park waits on one (`deadline-waits`): a fiber parks on
/// the scheduler instead. A wait may end with no notify, as any condition's: wait in a loop on
/// what it is for ([`Signal::wait_while`]).
pub struct Signal {
    name: &'static str,
    raw: raw::Cond,
}

impl Signal {
    pub const fn new(name: &'static str) -> Signal {
        let raw = raw::Cond::new();
        Signal { name, raw }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Runs `wait` inside the signal's span and wait-graph row; `left` is the time from now to
    /// its deadline (`None`: it has none), which the row says on the probe's clock.
    fn span<R>(&self, left: Option<Duration>, wait: impl FnOnce() -> R) -> R {
        signal_span(self.name, left, wait)
    }

    /// Waits for a notify, letting `guard`'s lock go meanwhile.
    pub fn wait<T: ?Sized>(&self, guard: &mut LockGuard<'_, T>) {
        self.span(None, || _ = self.raw.wait(&mut guard.inner, None));
    }

    /// Waits while `condition` holds.
    pub fn wait_while<T: ?Sized>(
        &self,
        guard: &mut LockGuard<'_, T>,
        mut condition: impl FnMut(&mut T) -> bool,
    ) {
        self.span(None, || {
            while condition(&mut *guard.inner) {
                self.raw.wait(&mut guard.inner, None);
            }
        });
    }

    /// Waits for a notify by `deadline`: whether the deadline passed first.
    pub fn wait_until<T: ?Sized>(&self, guard: &mut LockGuard<'_, T>, deadline: Instant) -> bool {
        let left = deadline.saturating_duration_since(Instant::now());
        self.span(Some(left), || self.raw.wait(&mut guard.inner, Some(left)))
    }

    /// Waits for a notify for at most `most`: whether it passed first.
    pub fn wait_for<T: ?Sized>(&self, guard: &mut LockGuard<'_, T>, most: Duration) -> bool {
        self.span(Some(most), || self.raw.wait(&mut guard.inner, Some(most)))
    }

    /// Waits while `condition` holds, by `deadline`: whether the deadline passed first (with
    /// `condition` still holding).
    pub fn wait_while_until<T: ?Sized>(
        &self,
        guard: &mut LockGuard<'_, T>,
        condition: impl FnMut(&mut T) -> bool,
        deadline: Instant,
    ) -> bool {
        let now = Instant::now();
        let left = deadline.saturating_duration_since(now);
        self.span(Some(left), || Self::wait_while_within(&self.raw, guard, condition, now, left))
    }

    /// Waits while `condition` holds, for at most `most`: whether it passed first (with
    /// `condition` still holding).
    pub fn wait_while_for<T: ?Sized>(
        &self,
        guard: &mut LockGuard<'_, T>,
        condition: impl FnMut(&mut T) -> bool,
        most: Duration,
    ) -> bool {
        let now = Instant::now();
        self.span(Some(most), || Self::wait_while_within(&self.raw, guard, condition, now, most))
    }

    /// The loop of the two waits above: each wait is given what is left now of `most` from
    /// `start`.
    fn wait_while_within<T: ?Sized>(
        raw: &raw::Cond,
        guard: &mut LockGuard<'_, T>,
        mut condition: impl FnMut(&mut T) -> bool,
        start: Instant,
        most: Duration,
    ) -> bool {
        while condition(&mut *guard.inner) {
            let left = most.saturating_sub(start.elapsed());
            if left.is_zero() {
                return true;
            }
            raw.wait(&mut guard.inner, Some(left));
        }
        false
    }

    pub fn notify_one(&self) {
        self.raw.notify(false);
    }

    pub fn notify_all(&self) {
        self.raw.notify(true);
    }
}

impl fmt::Debug for Signal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Signal").field("name", &self.name).finish()
    }
}

// --- The shared map's lock (decision D64) -----------------------------------------------------

/// The lock blaze-hash's shared map takes for a shard's writers: this crate's [`Lock`], so a
/// writer's wait is a named, recorded deadline wait. blaze-hash sits below this crate and cannot
/// name it, so its map takes the lock as a type parameter and this is the one instantiation.
pub enum MapLock {}

// SAFETY: a `Lock` hands its value to one guard at a time, and its release happens before the
// next acquire (the raw lock's Release unlock and Acquire lock).
#[expect(unsafe_code, reason = "the shared map's lock contract (D64): `Lock` is a mutex")]
unsafe impl blaze_hash::shared::ShardLock for MapLock {
    type Lock<T> = Lock<T>;
    type Guard<'a, T: 'a> = LockGuard<'a, T>;

    fn new<T>(name: &'static str, value: T) -> Lock<T> {
        Lock::new(name, value)
    }

    #[track_caller]
    fn lock<T>(lock: &Lock<T>) -> LockGuard<'_, T> {
        lock.lock()
    }
}

/// A map of ordinary keys shared between threads (rule standard-structures): read with no lock,
/// written under a shard's [`MapLock`]; made by `SharedMap::new_in(name, alloc)`, `name` saying
/// what it holds. The `.clippy.toml` ban on `blaze_hash::SharedMap` keeps every other crate on
/// this instantiation.
#[expect(clippy::disallowed_types, reason = "the one instantiation of the shared map (D64)")]
pub type SharedMap<K, V, A> = blaze_hash::SharedMap<K, V, A, MapLock>;

/// A set shared between threads, as [`SharedMap`].
#[expect(clippy::disallowed_types, reason = "the one instantiation of the shared set (D64)")]
pub type SharedSet<K, A> = blaze_hash::SharedSet<K, A, MapLock>;

/// A map on hashes shared between threads, indexed by the hash itself, as [`SharedMap`].
#[expect(clippy::disallowed_types, reason = "the one instantiation of the shared map (D64)")]
pub type SharedByHash<K, V, A> = blaze_hash::SharedByHash<K, V, A, MapLock>;

/// [`SharedMap`] on a named hasher `S`: for digest keys, `blaze_hash::DigestState`, which takes
/// a key's digest word as its hash (blaze-hash digest.rs) where `SharedMap`'s `FixedState` runs
/// `Hash` over every byte.
#[expect(clippy::disallowed_types, reason = "the one instantiation of the shared map (D64)")]
pub type SharedMapBy<K, V, S, A> = blaze_hash::shared::Map<K, V, S, A, MapLock>;

/// [`SharedSet`] on a named hasher `S`, as [`SharedMapBy`].
#[expect(clippy::disallowed_types, reason = "the one instantiation of the shared set (D64)")]
pub type SharedSetBy<K, S, A> = blaze_hash::shared::Set<K, S, A, MapLock>;
