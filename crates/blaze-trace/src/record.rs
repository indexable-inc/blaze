//! The recording path: a span's edges and an event as fixed 32-byte entries in the calling
//! thread's ring ([`crate::ring`], 4-word slots), timed by the CPU's counter, formatted by the
//! reader later.
//!
//! - **A site** ([`Site`]) is a `static` the macro writes beside its [`Metadata`]: the metadata
//!   and one atomic word, its state: its 32-bit id in the process's site table ([`site`]) and
//!   two bits saying who records it (the rings, a [`Sink`]). Every macro reads that word
//!   (relaxed) and branches: a site nobody records costs one load and one predictable branch,
//!   and evaluates no field. The word starts as "unseen" (both bits set, id 0), so a site's
//!   first call takes the cold path once, which gives it an id and decides its bits; installing
//!   a recorder decides every known site's bits again.
//! - **An entry** is three payload words (the ring adds the stamp): word 0 the kind, a tag, a
//!   16-bit depth or field index and the 32-bit site id; word 1 the tick; word 2 the span. A
//!   span is two entries, one as it is entered and one as it is left: two counter reads and two
//!   slot writes, no allocation, no formatting, no `dyn` call, no lock and no atomic another
//!   thread writes. A span with fields, an explicit parent or a `record` writes more entries
//!   (an [`Entry::Open`], then one [`Entry::Field`] a field): numbers as raw words; text
//!   (`%`, `?`, `&str`) is the one thing formatted at the site, into [`Entry::Text`] entries of
//!   16 bytes, at most [`TraceLimits::most_text`] bytes a field ([`MOST_TEXT`] shipped), the
//!   rest counted as cut.
//! - **The clock** is the CPU's counter, read in user space (`cntvct_el0` on arm64, `rdtsc` on
//!   x86-64); the reader turns ticks to time ([`counter_hz`], or two (tick, time) pairs where
//!   the counter's rate is not readable). A simulation passes its clock in
//!   ([`RingConfig::clock`]): a `fn` the config holds, so the counter path costs one more
//!   predictable branch on a word already loaded.
//! - **CPU time** is never read per span (it is a system call on macOS): the driver writes it
//!   at a task's boundaries ([`task_boundary`]) and the reader shares a task's CPU among the
//!   spans inside it by their wall time.
//! - **A thread's ring** is claimed on its first entry and given back when the thread exits
//!   (the thread-exit hook of the platform's C runtime). A thread that finds every lane held
//!   records nothing, counted exactly ([`unrecorded`]).
//!
//! # The process's profile ring: the one static (C3)
//!
//! D170's C3 allows three statics with state, each named and capped: the allocator, the profile
//! ring and the signal handler. This module holds the profile ring's, and nothing else of this
//! crate has state in a `static`:
//!
//! - [`PROFILE`] ([`Profile`]): what the program's entry installed once (the rings' config and the
//!   entry's sink, the flight recorder), the site table (capped at [`MOST_SITES`] rows, a site
//!   past it recorded by nobody and counted in [`refused_sites`]), and its counters. Capped: its
//!   size is fixed at compile time (`MOST_SITES` pointers and seven words), it never allocates.
//! - [`THREAD`], the ring's per-thread half (`#[thread_local]`): the calling thread's producer of
//!   one lane of the installed rings (capped by the rings' lanes, [`crate::ring::MOST_LANES`] at
//!   most), its span depth, and the sink it set for itself. It is part of the profile ring, not a
//!   fourth static: a producer is a lane of the one ring, held by the thread that writes it.
//!
//! Everything else is a value: the rings themselves are made by the entry from its allocator
//! (`Rings::new_in(lanes, len, alloc)`) and handed in by reference ([`RingConfig::rings`]); the
//! clock is a value of the config ([`RingConfig::clock`]); the caps of a text field are the
//! config's [`TraceLimits`]; a sink is handed in by the entry ([`crate::install`]) or by a test
//! ([`crate::with_thread_sink`]), or straight to [`crate::Span::new`] and
//! [`crate::Event::dispatch`]. The sites are `static`s the macros write, but constant data: their
//! one word of state is the site's row in [`PROFILE`]'s table and who records it.

use core::cell::{Cell, UnsafeCell};
use core::fmt;
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU8, AtomicU32, AtomicU64, Ordering};

use blaze_own::SharedAlloc;

use crate::ring::{Emitter, Producer};
use crate::{Installed, Metadata, Parent, Sink, Value, Values};

/// The words of a slot: the stamp and three payload words, 32 bytes.
pub const SLOT: usize = 4;
/// The payload words of an entry.
pub const WORDS: usize = SLOT - 1;

/// The allocator the rings are in: the process's, handed by the entry as a handle that crosses
/// threads (every thread pushes to its lane of them), named here because the rings are reached
/// from the profile ring's static and so cannot be generic.
pub type RingsAlloc = SharedAlloc<'static>;

/// The rings spans are recorded into.
pub type Rings = Emitter<RingsAlloc, SLOT>;

/// The most sites a process records: 2^15, 256 KB of table: the cap of [`PROFILE`]'s site table,
/// fixed at compile time because the table is the static itself.
pub const MOST_SITES: usize = 1 << 15;

/// The most bytes of one text field kept; the rest are counted as cut: [`TraceLimits::SHIPPED`]'s.
pub const MOST_TEXT: u64 = 256;

/// The recording path's caps as one value (D165), carried by the [`RingConfig`] the entry
/// installs; a simulation draws them down ([`TraceLimits::SHIPPED`] is the shipped instance).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TraceLimits {
    /// The most bytes of one text field written into the rings, at least 0; the rest are cut.
    pub most_text: u64,
}

impl TraceLimits {
    /// The caps shipped: [`MOST_TEXT`] bytes a text field.
    pub const SHIPPED: TraceLimits = TraceLimits { most_text: MOST_TEXT };
}

const RING: u32 = 1;
const SINK: u32 = 2;
const WHO: u32 = RING | SINK;
const ID_SHIFT: u32 = 2;
/// A site no call has reached yet: both bits set so its first call takes the cold path.
const UNSEEN: u32 = WHO;

// --- Sites ------------------------------------------------------------------------------

/// A site as the macros write it: its metadata and its state word.
pub struct Site {
    meta: &'static Metadata<'static>,
    state: AtomicU32,
}

impl Site {
    pub const fn new(meta: &'static Metadata<'static>) -> Site {
        Site { meta, state: AtomicU32::new(UNSEEN) }
    }

    /// Who records this call, decided before any field is evaluated: `None` costs one load and
    /// a branch. The calling thread's own sink comes first (a test's, a simulation's), then the
    /// rings, then the entry's sink; a sink is asked [`Sink::enabled`] only on the sink paths.
    #[inline(always)]
    pub fn how(&'static self) -> Option<How> {
        let mut state = self.state.load(Ordering::Relaxed);
        if state & WHO == 0 {
            return None;
        }
        if state == UNSEEN {
            state = register(self);
        }
        if state & SINK != 0
            && let Some(sink) = THREAD.sink.get()
        {
            return sink.enabled(self.meta).then_some(How::Sink(sink));
        }
        if state & RING != 0
            && let Some(config) = PROFILE.rings.get()
        {
            return Some(How::Ring(config, state >> ID_SHIFT));
        }
        if state & SINK != 0
            && let Some(sink) = crate::entry()
            && sink.enabled(self.meta)
        {
            return Some(How::Sink(sink));
        }
        None
    }

    #[inline(always)]
    pub const fn metadata(&self) -> &'static Metadata<'static> {
        self.meta
    }

    /// Its id in the site table ([`site`]); 0 until its first call.
    #[inline(always)]
    pub fn id(&self) -> u32 {
        self.state.load(Ordering::Relaxed) >> ID_SHIFT
    }

}

/// Who records a site's call ([`Site::how`]).
#[derive(Clone, Copy)]
pub enum How {
    /// The rings, under the site's id.
    Ring(&'static RingConfig, u32),
    Sink(&'static dyn Sink),
}

// --- The profile ring's static ----------------------------------------------------------

/// The process's profile ring: the one `static` with state of blaze's tracing (the module's
/// "The process's profile ring"), capped at compile time and never allocating.
pub struct Profile {
    /// The rings' config, written once by [`install_rings`].
    rings: Once<RingConfig>,
    /// The entry's sink (the flight recorder), written once by [`crate::install`].
    pub(crate) entry: Once<&'static dyn Sink>,
    /// The site table: row `id` is the site of that id, row 0 none.
    sites: [AtomicPtr<Site>; MOST_SITES],
    next_site: AtomicU32,
    refused_sites: AtomicU64,
    /// Moved by every change of who records what, so a site registering meanwhile decides again.
    generation: AtomicU32,
    /// Threads with a sink of their own ([`crate::set_thread`]): while any has, every site takes
    /// the sink path, which asks the thread's sink.
    pub(crate) thread_sinks: AtomicU32,
    /// Entries not recorded because their thread found every lane held.
    unrecorded: AtomicU64,
}

impl Profile {
    const fn new() -> Profile {
        Profile {
            rings: Once::new(),
            entry: Once::new(),
            sites: [const { AtomicPtr::new(ptr::null_mut()) }; MOST_SITES],
            next_site: AtomicU32::new(1),
            refused_sites: AtomicU64::new(0),
            generation: AtomicU32::new(0),
            thread_sinks: AtomicU32::new(0),
            unrecorded: AtomicU64::new(0),
        }
    }
}

/// The process's profile ring (C3's second allowed static): named, capped ([`Profile`]).
pub static PROFILE: Profile = Profile::new();

/// The site of id `id`, as the entries name it.
pub fn site(id: u32) -> Option<&'static Metadata<'static>> {
    let found = PROFILE.sites.get(id as usize)?.load(Ordering::Acquire);
    // SAFETY: only `register` stores here, and only `&'static Site` pointers.
    unsafe { found.as_ref() }.map(|it| it.meta)
}

/// Sites past [`MOST_SITES`], which record nothing.
pub fn refused_sites() -> u64 {
    PROFILE.refused_sites.load(Ordering::Relaxed)
}

/// Who records `meta` now: the rings and a sink's bits.
fn who(meta: &'static Metadata<'static>) -> u32 {
    let mut bits = 0;
    if let Some(config) = PROFILE.rings.get()
        && (config.wants)(meta)
    {
        bits |= RING;
    }
    if PROFILE.thread_sinks.load(Ordering::Acquire) > 0 || crate::entry().is_some_and(|it| it.enabled(meta)) {
        bits |= SINK;
    }
    bits
}

/// A site's first call: its id, and its bits decided.
#[cold]
#[inline(never)]
fn register(site: &'static Site) -> u32 {
    let id = PROFILE.next_site.fetch_add(1, Ordering::Relaxed);
    if id as usize >= MOST_SITES {
        PROFILE.refused_sites.fetch_add(1, Ordering::Relaxed);
        _ = site.state.compare_exchange(UNSEEN, 0, Ordering::Relaxed, Ordering::Relaxed);
        return site.state.load(Ordering::Relaxed);
    }
    PROFILE.sites[id as usize].store(ptr::from_ref(site).cast_mut(), Ordering::Release);
    let generation = PROFILE.generation.load(Ordering::Acquire);
    let state = id << ID_SHIFT | who(site.meta);
    if let Err(other) = site.state.compare_exchange(UNSEEN, state, Ordering::AcqRel, Ordering::Relaxed) {
        // Another thread registered it first; this id's table row names the same site.
        return other;
    }
    if PROFILE.generation.load(Ordering::Acquire) != generation {
        decide(site);
    }
    site.state.load(Ordering::Relaxed)
}

/// Decides `site`'s bits again, keeping its id.
fn decide(site: &'static Site) {
    let mut state = site.state.load(Ordering::Relaxed);
    loop {
        if state == UNSEEN {
            return;
        }
        let new = (state & !WHO) | (if state >> ID_SHIFT == 0 { 0 } else { who(site.meta) });
        match site.state.compare_exchange_weak(state, new, Ordering::AcqRel, Ordering::Relaxed) {
            Ok(_) => return,
            Err(now) => state = now,
        }
    }
}

/// Decides every known site's bits again: what installing a recorder does.
pub(crate) fn decide_all() {
    PROFILE.generation.fetch_add(1, Ordering::AcqRel);
    let known = (PROFILE.next_site.load(Ordering::Acquire) as usize).min(MOST_SITES);
    for row in &PROFILE.sites[1..known] {
        // SAFETY: as in `site`.
        if let Some(site) = unsafe { row.load(Ordering::Acquire).as_ref() } {
            decide(site);
        }
    }
}

// --- Written once -----------------------------------------------------------------------

/// A value written once, then only read: the entry's sink and the rings' config.
pub(crate) struct Once<T> {
    state: AtomicU8,
    value: UnsafeCell<Option<T>>,
}

const EMPTY: u8 = 0;
const WRITING: u8 = 1;
const READY: u8 = 2;

// SAFETY: written once, by the one `set` that moved `state` from EMPTY to WRITING, and read
// only after `state` is READY (release, then acquire), so no read races the write.
unsafe impl<T: Send + Sync> Sync for Once<T> {}

impl<T> Once<T> {
    pub(crate) const fn new() -> Once<T> {
        Once { state: AtomicU8::new(EMPTY), value: UnsafeCell::new(None) }
    }

    pub(crate) fn set(&self, value: T) -> Result<(), Installed> {
        if self.state.compare_exchange(EMPTY, WRITING, Ordering::Acquire, Ordering::Relaxed).is_err() {
            return Err(Installed);
        }
        // SAFETY: this call alone moved the state to WRITING, and no reader reads before READY.
        unsafe { *self.value.get() = Some(value) };
        self.state.store(READY, Ordering::Release);
        Ok(())
    }

    #[inline(always)]
    pub(crate) fn get(&self) -> Option<&T> {
        if self.state.load(Ordering::Acquire) != READY {
            return None;
        }
        // SAFETY: READY was stored after the one write, with release, and read here with acquire;
        // nothing writes it again.
        unsafe { (*self.value.get()).as_ref() }
    }
}

// --- The rings' config ------------------------------------------------------------------

/// What the program's entry gives [`install_rings`]: every input of the recording path as a
/// value (C3): the rings it made from its allocator, which sites they record, the clock and the
/// caps.
#[derive(Clone, Copy)]
pub struct RingConfig {
    pub rings: &'static Rings,
    /// Which sites the rings record, decided once a site.
    pub wants: fn(&'static Metadata<'static>) -> bool,
    pub clock: Clock,
    /// The recording path's caps: [`TraceLimits::SHIPPED`] in a program.
    pub limits: TraceLimits,
}

/// Where an entry's tick comes from: a value of the [`RingConfig`] the entry installs, so time
/// enters the recording path as the entry chose it, never read from anywhere else. `Counter` is
/// the CPU's counter, the profile ring's own clock (a register read in user space, part of the
/// allowed static's mechanism, as the ring's stamps are). `Sim` is a simulation's clock, a `fn`
/// the simulation hands in; it exists only with the `sim-clock` feature (and in this crate's
/// tests) so a program's tick is one `mrs`/`rdtsc` a read, no branch and no call.
#[derive(Clone, Copy)]
pub enum Clock {
    Counter,
    /// A simulation's clock, a call a read.
    #[cfg(any(test, feature = "sim-clock"))]
    Sim(fn() -> u64),
}

impl RingConfig {
    #[inline(always)]
    fn tick(&self) -> u64 {
        match self.clock {
            Clock::Counter => counter(),
            #[cfg(any(test, feature = "sim-clock"))]
            Clock::Sim(clock) => clock(),
        }
    }
}

/// Why the rings could not be installed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingsRefused {
    /// The process has its rings already.
    Installed,
    /// No CPU counter is readable on this target and no clock was given.
    NoCounter,
}

impl fmt::Display for RingsRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            RingsRefused::Installed => "blaze_trace::install_rings: the process has its rings already",
            RingsRefused::NoCounter => {
                "blaze_trace::install_rings: this target has no user-space CPU counter; pass a clock"
            }
        })
    }
}

/// Makes `config` the process's rings, once: from then on every site it wants is recorded
/// there, and a sink installed beside it gets only the sites the rings do not.
pub fn install_rings(config: RingConfig) -> Result<(), RingsRefused> {
    if matches!(config.clock, Clock::Counter) && !HAS_COUNTER {
        return Err(RingsRefused::NoCounter);
    }
    PROFILE.rings.set(config).map_err(|Installed| RingsRefused::Installed)?;
    decide_all();
    Ok(())
}

/// The process's rings, for their reader.
pub fn rings() -> Option<&'static Rings> {
    PROFILE.rings.get().map(|it| it.rings)
}

/// The tick the rings' clock reads now: what an entry's word 1 is in.
pub fn ticks() -> u64 {
    PROFILE.rings.get().map_or_else(counter, RingConfig::tick)
}

// --- The counter ------------------------------------------------------------------------

const HAS_COUNTER: bool = cfg!(any(target_arch = "aarch64", target_arch = "x86_64"));

/// The CPU's counter, read in user space; 0 on a target without one ([`install_rings`] then
/// wants a clock).
#[inline(always)]
pub fn counter() -> u64 {
    #[cfg(target_arch = "aarch64")]
    {
        let t: u64;
        // SAFETY: `cntvct_el0` is readable at EL0 on every arm64 host blaze runs on (macOS and
        // Linux enable it); the read has no side effect and writes only `t`.
        unsafe { core::arch::asm!("mrs {t}, cntvct_el0", t = out(reg) t, options(nomem, nostack, preserves_flags)) };
        t
    }
    #[cfg(target_arch = "x86_64")]
    {
        let (lo, hi): (u32, u32);
        // SAFETY: `rdtsc` is unprivileged unless CR4.TSD is set, which no host blaze runs on
        // sets; it writes only eax and edx.
        unsafe { core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags)) };
        u64::from(hi) << 32 | u64::from(lo)
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        0
    }
}

/// The counter's ticks a second where the CPU says it (`cntfrq_el0` on arm64); `None` on
/// x86-64, where the reader times the counter against a clock instead.
pub fn counter_hz() -> Option<u64> {
    #[cfg(target_arch = "aarch64")]
    {
        let hz: u64;
        // SAFETY: as for `cntvct_el0`: `cntfrq_el0` is readable at EL0 and has no side effect.
        unsafe { core::arch::asm!("mrs {hz}, cntfrq_el0", hz = out(reg) hz, options(nomem, nostack, preserves_flags)) };
        Some(hz)
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        None
    }
}

// --- The thread -------------------------------------------------------------------------

/// Whether a thread may still claim a lane: a typed state, not a flag beside the producer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Claim {
    /// It has not tried, or holds a lane.
    Open,
    /// Every lane was held when it tried: it records nothing more, each entry counted.
    Refused,
}

/// The calling thread's state: the profile ring's per-thread half (the module's "The process's
/// profile ring"), the one thread-local of blaze's tracing. Its sink (when it set one), its
/// lane's producer and its depth.
pub(crate) struct Thread {
    pub(crate) sink: Cell<Option<&'static dyn Sink>>,
    producer: UnsafeCell<Option<Producer<'static, RingsAlloc, SLOT>>>,
    claim: Cell<Claim>,
    depth: Cell<u16>,
}

/// The calling thread's half of [`PROFILE`]: capped by the rings' lanes (a thread past them
/// holds no producer and counts what it would have recorded, [`unrecorded`]).
#[thread_local]
pub(crate) static THREAD: Thread =
    Thread { sink: Cell::new(None), producer: UnsafeCell::new(None), claim: Cell::new(Claim::Open), depth: Cell::new(0) };

/// Entries not recorded because their thread found every lane held.
pub fn unrecorded() -> u64 {
    PROFILE.unrecorded.load(Ordering::Relaxed)
}

/// The bit a span id's lane starts at: lanes are under 2^12, entries under 2^51.
const LANE_SHIFT: u32 = 51;

/// The span id of entry `n` of `lane`: never 0. An enter entry naming span 0 opened its span:
/// the span's id is that entry's.
#[inline(always)]
pub const fn span_id(lane: u32, n: u64) -> u64 {
    (lane as u64 + 1) << LANE_SHIFT | n
}

/// A span id's lane and entry number.
pub const fn span_parts(span: u64) -> Option<(u32, u64)> {
    match span >> LANE_SHIFT {
        0 => None,
        lane => Some(((lane - 1) as u32, span & ((1 << LANE_SHIFT) - 1))),
    }
}

/// The calling thread's state, found once a span edge and passed down: on macOS every access
/// to a thread-local is a call through its descriptor, so a span takes one, at its enter, and
/// its guard keeps the pointer for the exit.
#[inline(always)]
pub(crate) fn here() -> *const Thread {
    &raw const THREAD
}

/// The calling thread's state's address, for `examples/cost.rs`'s ladder alone: what one
/// thread-local access costs.
#[doc(hidden)]
#[inline(always)]
pub fn thread_state() -> *const u8 {
    here().cast()
}

/// Pushes `payload` into `thread`'s ring (the calling thread's): its span id, `None` when
/// nothing was recorded.
#[inline(always)]
fn push(config: &RingConfig, thread: &Thread, payload: &[u64; WORDS]) -> Option<u64> {
    // SAFETY: `thread` is the calling thread's own; its producer is borrowed only here and in
    // `claim_and_push` and `ended`, none of which calls back into any of them.
    let producer = unsafe { &mut *thread.producer.get() };
    match producer {
        Some(producer) => Some(span_id(producer.lane().0, producer.push(payload))),
        None => claim_and_push(config, thread, producer, payload),
    }
}

#[cold]
#[inline(never)]
fn claim_and_push(
    config: &RingConfig,
    thread: &Thread,
    slot: &mut Option<Producer<'static, RingsAlloc, SLOT>>,
    payload: &[u64; WORDS],
) -> Option<u64> {
    if thread.claim.get() == Claim::Refused {
        PROFILE.unrecorded.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    match config.rings.claim() {
        Ok(producer) => {
            at_thread_exit();
            let producer = slot.insert(producer);
            Some(span_id(producer.lane().0, producer.push(payload)))
        }
        Err(_) => {
            thread.claim.set(Claim::Refused);
            PROFILE.unrecorded.fetch_add(1, Ordering::Relaxed);
            None
        }
    }
}

/// Gives the exiting thread's lane back.
unsafe extern "C" fn ended(_: *mut u8) {
    // SAFETY: as in `push`: the thread is exiting and records nothing more.
    drop(unsafe { (*THREAD.producer.get()).take() });
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// dyld's thread-exit hook for thread-locals (what std's `thread_local!` destructors use;
    /// private to the platform, rule 45): `dtor(arg)` runs as the thread exits. The link is the
    /// test that the symbol exists.
    fn _tlv_atexit(dtor: unsafe extern "C" fn(*mut u8), arg: *mut u8);
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
unsafe extern "C" {
    /// glibc's thread-exit hook (what std's `thread_local!` destructors use).
    fn __cxa_thread_atexit_impl(dtor: unsafe extern "C" fn(*mut u8), arg: *mut u8, dso: *mut u8) -> i32;
    static __dso_handle: u8;
}

/// Has the calling thread give its lane back as it exits. On a target with no such hook (wasm,
/// one thread) the lane is held until the process ends.
fn at_thread_exit() {
    #[cfg(target_os = "macos")]
    // SAFETY: `ended` is a valid destructor for the life of the process and ignores `arg`.
    unsafe {
        _tlv_atexit(ended, ptr::null_mut());
    }
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: as above; `__dso_handle` is this object's, as the C runtime defines it.
    unsafe {
        __cxa_thread_atexit_impl(ended, ptr::null_mut(), (&raw const __dso_handle).cast_mut());
    }
}

// --- Entries ----------------------------------------------------------------------------

/// The kinds of entry, word 0's low byte.
mod kind {
    pub const ENTER: u8 = 1;
    pub const EXIT: u8 = 2;
    pub const OPEN: u8 = 3;
    pub const EVENT: u8 = 4;
    pub const FIELD: u8 = 5;
    pub const TEXT: u8 = 6;
    pub const RECORD: u8 = 7;
    pub const CPU: u8 = 8;
}

/// The value tags of a field entry.
mod tag {
    pub const I64: u8 = 1;
    pub const U64: u8 = 2;
    pub const F64: u8 = 3;
    pub const BOOL: u8 = 4;
    pub const I128: u8 = 5;
    pub const U128: u8 = 6;
    pub const TEXT: u8 = 7;
}

#[inline(always)]
const fn head(kind: u8, tag: u8, small: u16, site: u32) -> u64 {
    (site as u64) << 32 | (small as u64) << 16 | (tag as u64) << 8 | kind as u64
}

/// A parent as an entry's word 2: 0 the thread's innermost span, `u64::MAX` none.
#[inline]
fn parent_word(parent: Parent) -> u64 {
    match parent {
        Parent::Current => 0,
        Parent::Root => u64::MAX,
        Parent::Explicit(id) => id.into_u64(),
    }
}

/// One entry, as the reader reads it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Entry {
    /// The thread entered `span` of `site` at `depth` (0 outermost).
    Enter { site: u32, depth: u16, tick: u64, span: u64 },
    Exit { site: u32, depth: u16, tick: u64, span: u64 },
    /// A span of `site` began with fields or a named parent: its id is this entry's; its fields
    /// follow.
    Open { site: u32, tick: u64, parent: Parent },
    /// An event of `site`; its fields follow.
    Event { site: u32, depth: u16, tick: u64, parent: Parent },
    /// `span` of `site` was given a field later; it follows.
    Record { site: u32, span: u64 },
    /// Field `index` of the open, event or record before it.
    Field { index: u16, value: Raw },
    /// 16 bytes (or `len`) of the text field that follows.
    Text { len: u8, bytes: [u8; 16] },
    /// The driver's CPU time of the thread at a task's boundary.
    Cpu { tick: u64, cpu_ns: u64 },
    /// A kind no writer of this version makes.
    Unknown { word: u64 },
}

/// A field's value as an entry holds it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Raw {
    I64(i64),
    U64(u64),
    F64(f64),
    Bool(bool),
    I128(i128),
    U128(u128),
    /// The text entries before it held `kept` bytes; `cut` more were not kept.
    Text { kept: u64, cut: u64 },
    Unknown { tag: u8 },
}

fn parent_of_word(word: u64) -> Parent {
    match word {
        0 => Parent::Current,
        u64::MAX => Parent::Root,
        id => Parent::Explicit(crate::Id::from_u64(id)),
    }
}

/// Reads an entry's three payload words.
pub fn decode(words: [u64; WORDS]) -> Entry {
    let [w0, w1, w2] = words;
    let (kind, tag, small, site) = (w0 as u8, (w0 >> 8) as u8, (w0 >> 16) as u16, (w0 >> 32) as u32);
    match kind {
        kind::ENTER => Entry::Enter { site, depth: small, tick: w1, span: w2 },
        kind::EXIT => Entry::Exit { site, depth: small, tick: w1, span: w2 },
        kind::OPEN => Entry::Open { site, tick: w1, parent: parent_of_word(w2) },
        kind::EVENT => Entry::Event { site, depth: small, tick: w1, parent: parent_of_word(w2) },
        kind::RECORD => Entry::Record { site, span: w2 },
        kind::FIELD => Entry::Field {
            index: small,
            value: match tag {
                tag::I64 => Raw::I64(w1 as i64),
                tag::U64 => Raw::U64(w1),
                tag::F64 => Raw::F64(f64::from_bits(w1)),
                tag::BOOL => Raw::Bool(w1 != 0),
                tag::I128 => Raw::I128((u128::from(w2) << 64 | u128::from(w1)) as i128),
                tag::U128 => Raw::U128(u128::from(w2) << 64 | u128::from(w1)),
                tag::TEXT => Raw::Text { kept: w1, cut: w2 },
                tag => Raw::Unknown { tag },
            },
        },
        kind::TEXT => {
            let mut bytes = [0u8; 16];
            bytes[..8].copy_from_slice(&w1.to_le_bytes());
            bytes[8..].copy_from_slice(&w2.to_le_bytes());
            Entry::Text { len: tag.min(16), bytes }
        }
        kind::CPU => Entry::Cpu { tick: w1, cpu_ns: w2 },
        _ => Entry::Unknown { word: w0 },
    }
}

/// Text written into 16-byte entries as it is formatted, at most the config's
/// [`TraceLimits::most_text`] bytes.
struct Text<'c> {
    config: &'c RingConfig,
    chunk: [u8; 16],
    len: usize,
    kept: u64,
    cut: u64,
}

impl Text<'_> {
    fn flush(&mut self) {
        if self.len == 0 {
            return;
        }
        let low = u64::from_le_bytes(self.chunk[..8].try_into().unwrap_or([0; 8]));
        let high = u64::from_le_bytes(self.chunk[8..].try_into().unwrap_or([0; 8]));
        push(self.config, &THREAD, &[head(kind::TEXT, self.len as u8, 0, 0), low, high]);
        self.chunk = [0; 16];
        self.len = 0;
    }
}

impl fmt::Write for Text<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for &byte in text.as_bytes() {
            if self.kept >= self.config.limits.most_text {
                self.cut += 1;
                continue;
            }
            self.chunk[self.len] = byte;
            self.len += 1;
            self.kept += 1;
            if self.len == 16 {
                self.flush();
            }
        }
        Ok(())
    }
}

/// Writes each value given as a field entry (text first as its text entries).
fn fields(config: &RingConfig, values: Values<'_>) {
    for (index, value) in values.iter().enumerate() {
        if let Some(value) = value {
            field(config, index as u16, value);
        }
    }
}

fn field(config: &RingConfig, index: u16, value: &Value<'_>) {
    let (tag, low, high) = match *value {
        Value::I64(it) => (tag::I64, it as u64, 0),
        Value::U64(it) => (tag::U64, it, 0),
        Value::F64(it) => (tag::F64, it.to_bits(), 0),
        Value::Bool(it) => (tag::BOOL, u64::from(it), 0),
        Value::I128(it) => (tag::I128, it as u64, (it as u128 >> 64) as u64),
        Value::U128(it) => (tag::U128, it as u64, (it >> 64) as u64),
        Value::Str(_) | Value::Debug(_) | Value::Display(_) => {
            let mut text = Text { config, chunk: [0; 16], len: 0, kept: 0, cut: 0 };
            _ = match *value {
                Value::Str(it) => fmt::Write::write_str(&mut text, it),
                Value::Debug(it) => fmt::write(&mut text, format_args!("{it:?}")),
                Value::Display(it) => fmt::write(&mut text, format_args!("{it}")),
                _ => Ok(()),
            };
            text.flush();
            (tag::TEXT, text.kept, text.cut)
        }
    };
    push(config, &THREAD, &[head(kind::FIELD, tag, index, 0), low, high]);
}

/// The rings' config, when installed.
#[inline(always)]
pub(crate) fn config() -> Option<&'static RingConfig> {
    PROFILE.rings.get()
}

/// A span begins with fields or a named parent: its open entry and fields; its id.
pub(crate) fn open(config: &RingConfig, site: u32, parent: Parent, values: Values<'_>) -> u64 {
    let id = push(config, &THREAD, &[head(kind::OPEN, 0, 0, site), config.tick(), parent_word(parent)]);
    fields(config, values);
    id.unwrap_or(0)
}

/// An event: its entry and fields.
pub(crate) fn event(config: &RingConfig, site: u32, parent: Parent, values: Values<'_>) {
    let thread = &THREAD;
    push(config, thread, &[head(kind::EVENT, 0, thread.depth.get(), site), config.tick(), parent_word(parent)]);
    fields(config, values);
}

/// `span` was given field `index` its site names.
pub(crate) fn record(config: &RingConfig, site: u32, span: u64, index: u16, value: &Value<'_>) {
    push(config, &THREAD, &[head(kind::RECORD, 0, 0, site), 0, span]);
    field(config, index, value);
}

/// The calling thread enters `span` (0: a span with no entry yet, whose id this entry's
/// becomes): the span's id, and the thread's state for its exit ([`exit`]).
#[inline(always)]
pub(crate) fn enter(config: &RingConfig, site: u32, span: u64) -> (u64, *const Thread) {
    let here = here();
    // SAFETY: the calling thread's own state, live while the thread is.
    let thread = unsafe { &*here };
    let depth = thread.depth.get();
    thread.depth.set(depth.wrapping_add(1));
    let id = push(config, thread, &[head(kind::ENTER, 0, depth, site), config.tick(), span]);
    (if span == 0 { id.unwrap_or(0) } else { span }, here)
}

/// The thread whose state `here` is leaves `span`.
///
/// # Safety
///
/// `here` is what [`enter`] returned on the calling thread.
#[inline(always)]
pub(crate) unsafe fn exit(config: &RingConfig, here: *const Thread, site: u32, span: u64) {
    // SAFETY: the caller's promise: the calling thread's own state, live while it is.
    let thread = unsafe { &*here };
    let depth = thread.depth.get().wrapping_sub(1);
    thread.depth.set(depth);
    push(config, thread, &[head(kind::EXIT, 0, depth, site), config.tick(), span]);
}

/// A task's boundary on the calling thread, with the thread's CPU time as the driver read it:
/// the reader shares the CPU between two boundaries among the spans between them by wall time.
pub fn task_boundary(cpu_ns: u64) {
    if let Some(config) = PROFILE.rings.get() {
        push(config, &THREAD, &[head(kind::CPU, 0, 0, 0), config.tick(), cpu_ns]);
    }
}

/// Pushes three words of the caller's into the thread's ring, as an entry of kind 0x80 and up
/// (the caller's to define): a driver's own records, and the comparison in `examples/cost.rs`.
/// Its span id, `None` when nothing was recorded.
#[inline(always)]
pub fn emit(words: [u64; WORDS]) -> Option<u64> {
    let config = PROFILE.rings.get()?;
    push(config, &THREAD, &words)
}
