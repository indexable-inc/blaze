//! The sampler: where the CPU went, by function, for every run at [`Level::Spans`] and up,
//! with no flag (D178, "The sampler is part of it").
//!
//! # The core (sans-IO, simulated)
//!
//! - [`Ring`]: one per worker, at most [`RING`] (1024) [`Sample`]s of 160 bytes (160 KiB a
//!   worker, reserved when the [`Roster`] is made, never grown). A sample is the `pc`, up to
//!   [`FRAMES`] (16) return addresses from the frame-pointer chain ([`walk`]), why the walk
//!   stopped ([`Stop`]: a truncated stack says [`Stop::Depth`], and its deepest callers are not
//!   kept), the worker's top-level span id ([`top_id`]) and the driver's tick. A full ring
//!   halves: every second sample is kept and from then on one tick in `stride` is, so what is
//!   kept stays uniform over the whole run (never the last 1024 only) and each sample stands
//!   for `stride` ticks. Ticks a worker was parked are counted apart and never walked.
//! - [`fold`] at exit, on one thread after the drivers stopped ([`Stopped`]): on-CPU ticks by
//!   leaf function for the run ([`TOP_FUNCS`]), by top-level span ([`TOP_SPANS`] spans, the
//!   [`SPAN_FUNCS`] leaves of each), and the nearest callers ([`CALLERS`]) of the top
//!   [`TOP_CALLERS_OF`] leaves; everything past a cap is an `other` count, never dropped.
//! - [`Symbols`]: an address to a function name as a pure function of the executable's bytes
//!   and its slide (`symbols.rs` says why blaze-native's readers are not used).
//!
//! # The drivers (thin)
//!
//! - macOS ([`mac::MacSampler`]): one sampler thread; each pass, for every worker not marked
//!   parked: `thread_suspend`, `thread_get_state(ARM_THREAD_STATE64)` (pc, fp, sp), the walk
//!   bounded to `[sp, top)` (the top is `pthread_get_stackaddr_np`, recorded by the worker as it
//!   starts: [`Roster::enter`]), `thread_resume`. Between suspend and resume nothing locks or
//!   allocates: the stopped worker may hold the allocator's or any lock, and the sampler only
//!   writes the worker's own ring, reserved before. All calls are public (blaze-sys
//!   `macos_sample`).
//! - Linux ([`linux::install`], default): a `timer_create(CLOCK_THREAD_CPUTIME_ID,
//!   SIGEV_THREAD_ID)` per worker, its `SIGPROF` handler reading pc, fp and sp from the
//!   `ucontext` into that worker's own ring. Why this and not `perf_event_open`: it needs no
//!   `perf_event_paranoid` setting and works in containers that refuse perf; a CPU-time clock
//!   never fires on a parked thread, so no tick is spent on one; and the handler runs on the
//!   worker itself, so its span mark and ring are its own, read with no cross-thread race. The
//!   other choice, `perf_event_open(PERF_COUNT_SW_CPU_CLOCK, PERF_SAMPLE_CALLCHAIN)` with a
//!   mapped ring per worker (blaze-profile's `linux.rs` does this), takes kernel callchains and
//!   costs no signal, and is the one to add where perf is allowed and signals are not wanted.
//!   The cost of signals: an interrupted system call that cannot restart returns `EINTR`
//!   (`SA_RESTART` restarts the rest), which `blaze_core::wait::until` already retries as a
//!   spurious wake.
//!
//! # Its place
//!
//! On at `Spans` and `Detail`, off at `Totals` (the rings are not reserved, the drivers not
//! started). The period is a typed option ([`Period`], default [`Period::DEFAULT`], 1 ms). The
//! sampler's own cost is its report row ([`SamplerRow`]).
//!
//! # Frame pointers
//!
//! The walk needs every frame of interest to keep a frame record. arm64 macOS always does (the
//! platform ABI). On Linux rustc omits them by default: the workspace must build with
//! `-C force-frame-pointers=yes` (`.cargo/config.toml` `[target.aarch64-unknown-linux-gnu]`
//! and `[target.x86_64-unknown-linux-gnu]` `rustflags`, and blaze's own codegen must keep the
//! same setting for the programs it builds); without it the walk ends at the first frame and
//! only leaves are named. A leaf function with no frame record (`non-leaf`) loses its own
//! caller: the walk starts at the caller's caller.
//!
//! # Hooks (lines for their owners)
//!
//! - The pool's worker start (`rt.rs` `starter`, the spawned closure, before `mine.work(at)`):
//!   `roster.enter(at)` (port and stack range on macOS; tid, stack mapping and timer on
//!   Linux). The slots are made before with [`slots_in`] and, for Linux, leaked to
//!   `&'static` (the timers' handler reads them for the rest of the process).
//! - The park (`task/threads.rs` `work`, around its park): `roster.park(at)` just before the
//!   futex or kevent wait and `roster.unpark(at)` just after it returns.
//! - `Cx::span` (cx.rs `span`, a top-level span only): `let previous = roster.set_span(at,
//!   top_id(path))` at the open and `roster.restore_span(at, previous)` at the close.
//! - The exit (`rt.rs`): stop the driver before the pool, take [`Stopped`], fold with the
//!   executable's bytes and slide, and [`super::Report::set_profile`].

use core::{alloc::Allocator, fmt};

use blaze_cbor::{array, null, text, uint};
use blaze_own::{Vec, planted::Plants};

use super::{SpanPath, SpanRow, TelemetryError};

mod symbols;

#[cfg(all(target_os = "linux", any(target_arch = "aarch64", target_arch = "x86_64")))]
pub mod linux;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub mod mac;

/// The ring, the slot, the sample and the walk are blaze-sys's (`blaze_sys::sample`): the
/// slot's one-writer cell and the drivers' reads of a stopped stack are `unsafe`, and every
/// `unsafe` of the sampler is there. What is here is pure.
pub use blaze_sys::sample::{
    ADDRESS, ALIGN, FRAMES, Marks, RING, Regs, Ring, Sample, SamplePlant, Slot, Stack, StillWriting, Stop, Walked, take,
    walk,
};
pub use symbols::{Found, Symbols, SymbolsError};

/// The leaf functions the report names for the run.
pub const TOP_FUNCS: usize = 32;
/// The top-level spans the report breaks down, and the leaves of each.
pub const TOP_SPANS: usize = 16;
pub const SPAN_FUNCS: usize = 8;
/// The leaves whose callers are named (K), and the callers of each.
pub const TOP_CALLERS_OF: usize = 10;
pub const CALLERS: usize = 4;
/// The bytes of a name the report keeps; a longer one is cut at a character's boundary.
pub const NAME_MOST: usize = 96;

/// The bugs a control plants in the sampler.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SamplerPlant {
    None,
    /// A worker marked parked is sampled as on-CPU.
    ParkedSampled,
    /// The walk reads a record outside the stack's bounds.
    WalkUnbounded,
}

impl Plants for SamplerPlant {}

impl SamplerPlant {
    /// The plant as blaze-sys's [`take`] and [`walk`] take it.
    pub const fn sys(self) -> SamplePlant {
        match self {
            SamplerPlant::None => SamplePlant::None,
            SamplerPlant::ParkedSampled => SamplePlant::ParkedSampled,
            SamplerPlant::WalkUnbounded => SamplePlant::WalkUnbounded,
        }
    }
}

/// The time between samples of one worker: on Linux of its CPU time, on macOS of wall time
/// between the sampler's passes.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Period {
    ns: u64,
}

impl Period {
    /// 1 ms (the module doc and the final note give its cost).
    pub const DEFAULT: Period = Period { ns: 1_000_000 };
    /// The shortest period taken: 100 µs.
    pub const SHORTEST_NS: u64 = 100_000;
    /// The longest: 1 s.
    pub const LONGEST_NS: u64 = 1_000_000_000;

    pub fn new(ns: u64) -> Result<Period, SamplerError> {
        if (Period::SHORTEST_NS..=Period::LONGEST_NS).contains(&ns) { Ok(Period { ns }) } else { Err(SamplerError::Period { ns }) }
    }

    pub const fn ns(self) -> u64 {
        self.ns
    }
}

/// What the sampler refused or met. A driver that meets one stops sampling and the report
/// says which (no retry, no other route).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SamplerError {
    Period { ns: u64 },
    Reserve { bytes: usize },
    /// A Mach call on worker `worker`'s thread: the call and its `kern_return_t`.
    Mach { worker: u32, call: &'static str, kern: i32 },
    /// A Linux call: the call, its errno's number and name.
    Linux { worker: u32, call: &'static str, errno: u16, name: &'static str },
    /// The sampler's sleep between passes failed.
    Sleep,
    /// Worker `worker` entered twice, or past the roster.
    Enter { worker: u32 },
    /// Worker `worker`'s handler was still running when the stop gave up on it.
    Busy { worker: u32 },
    /// Linux: no readable mapping of `/proc/self/maps` holds worker `worker`'s stack.
    NoStack { worker: u32 },
}

impl fmt::Display for SamplerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SamplerError::Period { ns } => {
                write!(f, "a sampling period of {ns} ns is outside {}..={} ns", Period::SHORTEST_NS, Period::LONGEST_NS)
            }
            SamplerError::Reserve { bytes } => write!(f, "could not reserve {bytes} bytes for the sampler's rings"),
            SamplerError::Mach { worker, call, kern } => write!(f, "`{call}` on worker {worker}'s thread returned kern_return_t {kern}"),
            SamplerError::Linux { worker, call, errno, name } => write!(f, "`{call}` for worker {worker} failed: {name} ({errno})"),
            SamplerError::Sleep => f.write_str("the sampler's sleep between passes failed"),
            SamplerError::Enter { worker } => write!(f, "worker {worker} entered the sampler's roster twice or is past it"),
            SamplerError::Busy { worker } => write!(f, "worker {worker}'s sampling handler did not leave before the stop's bound"),
            SamplerError::NoStack { worker } => write!(f, "no readable mapping in /proc/self/maps holds worker {worker}'s stack"),
        }
    }
}

/// The id a top-level span's samples carry: its depth-1 path's id.
pub fn top_id(path: &SpanPath) -> u64 {
    if path.depth() == 0 { 0 } else { SpanPath::ROOT.child(path.top()).id() }
}

/// A ring as the fold reads it.
#[derive(Clone, Copy, Debug)]
pub struct RingView<'a> {
    pub samples: &'a [Sample],
    /// The ticks each kept sample stands for.
    pub stride: u64,
    pub seen: u64,
    pub parked: u64,
    pub stops: [u64; 5],
}

impl<'a> RingView<'a> {
    pub fn of(ring: &'a Ring) -> RingView<'a> {
        RingView { samples: ring.samples(), stride: ring.stride(), seen: ring.seen(), parked: ring.parked(), stops: ring.stops() }
    }
}

/// The slots of `workers` workers, reserved with `alloc` before the pool starts: none at a level
/// that does not sample (`period` of `None`). 160 KiB a worker (the ring is inline). The Linux
/// driver needs them for the rest of the process (`&'static`: a leaked vector).
pub fn slots_in<A: Allocator>(workers: u32, period: Option<Period>, alloc: A) -> Result<Vec<Slot, A>, SamplerError> {
    let n = if period.is_some() { workers as usize } else { 0 };
    let mut slots = Vec::try_with_capacity_in(n, alloc).map_err(|_| SamplerError::Reserve { bytes: n * size_of::<Slot>() })?;
    for _ in 0..n {
        slots.push(Slot::new());
    }
    Ok(slots)
}

/// The most loads [`Roster::close`] spends on one slot's writer in flight (a writer is a walk
/// of at most 16 records: well under 10 µs).
pub const CLOSE_LOADS: u32 = 1 << 20;

/// Every worker's slot by worker index, and the period: the hooks' and the drivers' view.
#[derive(Clone, Copy)]
pub struct Roster<'s> {
    slots: &'s [Slot],
    period: Option<Period>,
}

/// The proof that no driver writes a ring any more: made only by [`Roster::close`].
pub struct Stopped {
    _made: (),
}

impl<'s> Roster<'s> {
    /// `slots` from [`slots_in`] (empty at a level that does not sample).
    pub fn new(slots: &'s [Slot], period: Option<Period>) -> Roster<'s> {
        Roster { slots, period: if slots.is_empty() { None } else { period } }
    }

    pub fn period(&self) -> Option<Period> {
        self.period
    }

    pub fn slots(&self) -> &'s [Slot] {
        self.slots
    }

    pub fn slot(&self, worker: u32) -> Option<&'s Slot> {
        self.slots.get(worker as usize)
    }

    /// Worker `worker` parks (hook: just before the park's kernel wait). One store.
    #[inline]
    pub fn park(&self, worker: u32) {
        if let Some(slot) = self.slot(worker) {
            slot.park();
        }
    }

    /// Worker `worker` is back from its park.
    #[inline]
    pub fn unpark(&self, worker: u32) {
        if let Some(slot) = self.slot(worker) {
            slot.unpark();
        }
    }

    /// Worker `worker` opened top-level span `id` ([`top_id`]): the span it replaces, for
    /// [`Roster::restore_span`].
    #[inline]
    pub fn set_span(&self, worker: u32, id: u64) -> u64 {
        match self.slot(worker) {
            Some(slot) => slot.set_span(id),
            None => 0,
        }
    }

    #[inline]
    pub fn restore_span(&self, worker: u32, previous: u64) {
        if let Some(slot) = self.slot(worker) {
            slot.restore_span(previous);
        }
    }

    /// Every slot closed to writers (after the driver stopped ticking): the proof the fold
    /// takes. A writer still in flight past [`CLOSE_LOADS`] is [`SamplerError::Busy`].
    pub fn close(&self) -> Result<Stopped, SamplerError> {
        for (worker, slot) in self.slots.iter().enumerate() {
            slot.close(CLOSE_LOADS).map_err(|StillWriting| SamplerError::Busy { worker: worker as u32 })?;
        }
        Ok(Stopped { _made: () })
    }

    /// Every ring, once closed.
    pub fn rings(&self, _stopped: &Stopped) -> impl Iterator<Item = RingView<'s>> {
        self.slots.iter().filter_map(Slot::ring).map(RingView::of)
    }

    /// A slot's marks, as a driver reads them.
    pub fn marks(&self, worker: u32) -> Option<Marks> {
        self.slot(worker).map(Slot::marks)
    }
}

/// The sampler's own row: its cost and how it ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SamplerRow {
    pub period_ns: u64,
    /// macOS: passes made, and those that began later than their period (the period
    /// stretched: a pass over many workers took longer than the period).
    pub passes: u64,
    pub late: u64,
    /// macOS: the sampler thread's wall time in its passes (Linux: none; the handler's time is
    /// in each worker's own CPU time and is not measured apart).
    pub own_ns: Option<u64>,
    /// What stopped it early.
    pub stopped: Option<SamplerError>,
}

impl SamplerRow {
    pub const fn new(period: Period) -> SamplerRow {
        SamplerRow { period_ns: period.ns(), passes: 0, late: 0, own_ns: None, stopped: None }
    }
}

/// An address to a function.
pub trait Symbolize {
    /// The function `address` is in: its index (equal for every address in it) and name.
    fn find(&self, address: u64) -> Option<(u32, &str)>;
    fn name(&self, index: u32) -> Option<&str>;
}

impl<A: Allocator> Symbolize for Symbols<'_, A> {
    fn find(&self, address: u64) -> Option<(u32, &str)> {
        Symbols::find(self, address).map(|found| (found.index, found.name))
    }

    fn name(&self, index: u32) -> Option<&str> {
        Symbols::name(self, index)
    }
}

/// A stripped executable, or none read: every address is outside.
pub struct NoSymbols;

impl Symbolize for NoSymbols {
    fn find(&self, _: u64) -> Option<(u32, &str)> {
        None
    }

    fn name(&self, _: u32) -> Option<&str> {
        None
    }
}

/// A name, copied in and cut to [`NAME_MOST`] bytes at a character's boundary.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Name {
    bytes: [u8; NAME_MOST],
    len: u8,
    cut: bool,
}

impl Name {
    pub fn of(name: &str) -> Name {
        let mut len = name.len().min(NAME_MOST);
        while !name.is_char_boundary(len) {
            len -= 1;
        }
        let mut bytes = [0u8; NAME_MOST];
        bytes[..len].copy_from_slice(&name.as_bytes()[..len]);
        Name { bytes, len: len as u8, cut: len < name.len() }
    }

    pub fn as_str(&self) -> &str {
        // The bytes were a `str` cut at a character boundary.
        core::str::from_utf8(&self.bytes[..self.len as usize]).unwrap_or("")
    }

    pub fn cut(&self) -> bool {
        self.cut
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())?;
        if self.cut { f.write_str("…") } else { Ok(()) }
    }
}

/// A function's ticks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FuncRow {
    /// Its key: the symbol's index, [`OUTSIDE`] or [`NO_CALLER`].
    pub key: u32,
    pub name: Name,
    pub ticks: u64,
}

/// The key of every address outside the executable's functions (a system library, the
/// kernel's trampolines, code made at run time).
pub const OUTSIDE: u32 = u32::MAX;
/// The caller key of a sample whose walk found no return address.
pub const NO_CALLER: u32 = u32::MAX - 1;

/// One top-level span's leaves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SpanFuncs {
    pub span: u64,
    pub name: Name,
    pub ticks: u64,
    pub funcs: [Option<FuncRow>; SPAN_FUNCS],
    pub other: u64,
}

/// One leaf's nearest callers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Callers {
    pub leaf: FuncRow,
    pub callers: [Option<FuncRow>; CALLERS],
    pub other: u64,
}

/// The sampler's part of the report.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Profile {
    /// On-CPU ticks seen, and the ticks the kept samples stand for (equal unless a ring halved
    /// on a tick count that is not a multiple of its stride: at most one stride a worker).
    pub on_cpu: u64,
    pub weighed: u64,
    pub kept: u64,
    /// Ticks a worker was marked parked: kept apart, never in a share.
    pub parked: u64,
    /// Walks by how they stopped ([`Stop::ALL`] order).
    pub stops: [u64; 5],
    pub funcs: [Option<FuncRow>; TOP_FUNCS],
    pub other_funcs: u64,
    pub spans: [Option<SpanFuncs>; TOP_SPANS],
    pub other_spans: u64,
    pub callers: [Option<Callers>; TOP_CALLERS_OF],
    /// Why no function has a name, where the executable's table was refused.
    pub symbols: Option<SymbolsError>,
    pub sampler: SamplerRow,
}

/// A kept sample, keyed for the fold's sorts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Entry {
    leaf: u32,
    caller: u32,
    span: u64,
    weight: u64,
}

fn key_of(symbols: &impl Symbolize, address: u64) -> u32 {
    symbols.find(address).map_or(OUTSIDE, |(index, _)| index)
}

fn name_of(symbols: &impl Symbolize, key: u32) -> Name {
    match key {
        OUTSIDE => Name::of("(outside the executable)"),
        NO_CALLER => Name::of("(no caller: the walk ended)"),
        index => Name::of(symbols.name(index).unwrap_or("(a function with no name)")),
    }
}

fn span_name(spans: &[SpanRow], id: u64) -> Name {
    if id == 0 {
        return Name::of("(no span)");
    }
    match spans.iter().find(|row| row.path.depth() == 1 && row.path.id() == id) {
        Some(row) => Name::of(row.path.top()),
        None => Name::of("(a span the table folded into other)"),
    }
}

/// Runs of equal `key` in sorted `entries` from `at`: the run's end and its weight.
fn run(entries: &[Entry], at: usize, same: impl Fn(&Entry, &Entry) -> bool) -> (usize, u64) {
    let mut end = at;
    let mut weight = 0u64;
    while end < entries.len() && same(&entries[at], &entries[end]) {
        weight += entries[end].weight;
        end += 1;
    }
    (end, weight)
}

/// The `N` largest of `rows` (by ticks, then key), the rest's sum.
fn top<const N: usize>(rows: &mut [(u32, u64)], symbols: &impl Symbolize) -> ([Option<FuncRow>; N], u64) {
    rows.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut out = [None; N];
    let mut other = 0u64;
    for (at, (key, ticks)) in rows.iter().enumerate() {
        if at < N {
            out[at] = Some(FuncRow { key: *key, name: name_of(symbols, *key), ticks: *ticks });
        } else {
            other += ticks;
        }
    }
    (out, other)
}

fn reserve<T, A: Allocator>(rows: &mut Vec<T, A>, more: usize) -> Result<(), TelemetryError> {
    rows.try_reserve(more).map_err(|_| TelemetryError::Reserve { what: "the sampler's fold", bytes: more * size_of::<T>() })
}

/// The rings folded into the report's profile: `symbols` names the functions (a caller's
/// return address is looked up one byte back, inside its call), `spans` the report's span rows
/// name the top-level spans, `scratch` holds the fold's working rows (the exit's frame).
pub fn fold<'r, A: Allocator + Copy>(
    rings: impl Iterator<Item = RingView<'r>>,
    symbols: &impl Symbolize,
    spans: &[SpanRow],
    sampler: SamplerRow,
    scratch: A,
) -> Result<Profile, TelemetryError> {
    let mut entries: Vec<Entry, A> = Vec::new_in(scratch);
    let mut on_cpu = 0u64;
    let mut parked = 0u64;
    let mut stops = [0u64; 5];
    for ring in rings {
        on_cpu += ring.seen;
        parked += ring.parked;
        for (at, n) in ring.stops.iter().enumerate() {
            stops[at] += n;
        }
        reserve(&mut entries, ring.samples.len())?;
        for sample in ring.samples {
            let caller = match sample.frames().first() {
                Some(ret) => key_of(symbols, ret.wrapping_sub(1)),
                None => NO_CALLER,
            };
            entries.push(Entry { leaf: key_of(symbols, sample.pc), caller, span: sample.span, weight: ring.stride });
        }
    }
    let kept = entries.len() as u64;
    let weighed: u64 = entries.iter().map(|e| e.weight).sum();

    // By leaf, and by (leaf, caller) inside each leaf's run.
    entries.sort_unstable_by(|a, b| a.leaf.cmp(&b.leaf).then(a.caller.cmp(&b.caller)));
    let mut leaves: Vec<(u32, u64), A> = Vec::new_in(scratch);
    reserve(&mut leaves, entries.len())?;
    let mut at = 0;
    while at < entries.len() {
        let (end, weight) = run(&entries, at, |a, b| a.leaf == b.leaf);
        leaves.push((entries[at].leaf, weight));
        at = end;
    }
    let (funcs, other_funcs) = top::<TOP_FUNCS>(&mut leaves, symbols);

    let mut callers = [None; TOP_CALLERS_OF];
    let mut rows: Vec<(u32, u64), A> = Vec::new_in(scratch);
    for (slot, func) in callers.iter_mut().zip(funcs.iter().flatten()) {
        let start = entries.partition_point(|e| e.leaf < func.key);
        let end = entries.partition_point(|e| e.leaf <= func.key);
        rows.clear();
        reserve(&mut rows, end - start)?;
        let mut at = start;
        while at < end {
            let (next, weight) = run(&entries[..end], at, |a, b| a.caller == b.caller);
            rows.push((entries[at].caller, weight));
            at = next;
        }
        let (named, other) = top::<CALLERS>(&mut rows, symbols);
        *slot = Some(Callers { leaf: *func, callers: named, other });
    }

    // By top-level span, then leaf.
    entries.sort_unstable_by(|a, b| a.span.cmp(&b.span).then(a.leaf.cmp(&b.leaf)));
    let mut groups: Vec<(u64, u64, usize, usize), A> = Vec::new_in(scratch);
    reserve(&mut groups, entries.len())?;
    let mut at = 0;
    while at < entries.len() {
        let (end, weight) = run(&entries, at, |a, b| a.span == b.span);
        groups.push((entries[at].span, weight, at, end));
        at = end;
    }
    groups.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut by_span = [None; TOP_SPANS];
    let mut other_spans = 0u64;
    for (n, (span, ticks, start, end)) in groups.iter().copied().enumerate() {
        if n >= TOP_SPANS {
            other_spans += ticks;
            continue;
        }
        rows.clear();
        reserve(&mut rows, end - start)?;
        let mut at = start;
        while at < end {
            let (next, weight) = run(&entries[..end], at, |a, b| a.leaf == b.leaf);
            rows.push((entries[at].leaf, weight));
            at = next;
        }
        let (named, other) = top::<SPAN_FUNCS>(&mut rows, symbols);
        by_span[n] = Some(SpanFuncs { span, name: span_name(spans, span), ticks, funcs: named, other });
    }
    Ok(Profile {
        on_cpu,
        weighed,
        kept,
        parked,
        stops,
        funcs,
        other_funcs,
        spans: by_span,
        other_spans,
        callers,
        symbols: None,
        sampler,
    })
}

/// `part` of `whole` in tenths of a percent.
struct Share(u64, u64);

impl fmt::Display for Share {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let permille = if self.1 == 0 { 0 } else { (u128::from(self.0) * 1000 / u128::from(self.1)) as u64 };
        write!(f, "{}.{}%", permille / 10, permille % 10)
    }
}

fn funcs_line(f: &mut fmt::Formatter<'_>, rows: &[Option<FuncRow>], other: u64, whole: u64) -> fmt::Result {
    let mut first = true;
    for row in rows.iter().flatten() {
        if row.ticks == 0 {
            continue;
        }
        if !core::mem::replace(&mut first, false) {
            f.write_str(", ")?;
        }
        write!(f, "`{}` {}", row.name, Share(row.ticks, whole))?;
    }
    if other > 0 {
        write!(f, ", other {}", Share(other, whole))?;
    }
    Ok(())
}

impl Profile {
    pub fn symbols_refused(&mut self, why: SymbolsError) {
        self.symbols = Some(why);
    }

    /// The profile's CBOR (a positional array, keys and names only, no address).
    pub fn encode_into<B: Allocator>(&self, out: &mut Vec<u8, B>) {
        fn row<B: Allocator>(out: &mut Vec<u8, B>, row: &FuncRow) {
            array(out, 2);
            text(out, row.name.as_str());
            uint(out, row.ticks);
        }
        fn rows<B: Allocator>(out: &mut Vec<u8, B>, rows: &[Option<FuncRow>], other: u64) {
            array(out, 2);
            array(out, rows.iter().flatten().count() as u64);
            for r in rows.iter().flatten() {
                row(out, r);
            }
            uint(out, other);
        }
        array(out, 11);
        uint(out, self.on_cpu);
        uint(out, self.weighed);
        uint(out, self.kept);
        uint(out, self.parked);
        array(out, 5);
        for n in self.stops {
            uint(out, n);
        }
        rows(out, &self.funcs, self.other_funcs);
        array(out, self.spans.iter().flatten().count() as u64);
        for span in self.spans.iter().flatten() {
            array(out, 3);
            text(out, span.name.as_str());
            uint(out, span.ticks);
            rows(out, &span.funcs, span.other);
        }
        uint(out, self.other_spans);
        array(out, self.callers.iter().flatten().count() as u64);
        for callers in self.callers.iter().flatten() {
            array(out, 2);
            row(out, &callers.leaf);
            rows(out, &callers.callers, callers.other);
        }
        match self.symbols {
            Some(why) => text(out, symbols_says(why)),
            None => null(out),
        }
        let s = &self.sampler;
        array(out, 5);
        uint(out, s.period_ns);
        uint(out, s.passes);
        uint(out, s.late);
        match s.own_ns {
            Some(ns) => uint(out, ns),
            None => null(out),
        }
        match s.stopped {
            Some(_) => text(out, "stopped"),
            None => null(out),
        }
    }
}

/// A refusal's kind, for the encoding (no offset of the host's file).
fn symbols_says(why: SymbolsError) -> &'static str {
    match why {
        SymbolsError::Short { .. } => "short",
        SymbolsError::Magic { .. } => "magic",
        SymbolsError::Kind { .. } => "kind",
        SymbolsError::NoTable => "stripped",
        SymbolsError::NoText => "no text",
        SymbolsError::BadName { .. } => "bad name",
        SymbolsError::Reserve { .. } => "reserve",
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.on_cpu == 0 && self.parked == 0 {
            return Ok(());
        }
        write!(f, "profile: {} on-CPU ticks", self.on_cpu)?;
        if self.weighed != self.on_cpu || self.kept != self.on_cpu {
            write!(f, " ({} kept, standing for {})", self.kept, self.weighed)?;
        }
        write!(f, ", {} ticks a worker was parked (apart)", self.parked)?;
        for stop in Stop::ALL {
            let n = self.stops[stop.code()];
            if n > 0 && stop != Stop::Ended {
                write!(f, "; {n} walks stopped {}", stop.says())?;
            }
        }
        if let Some(why) = self.symbols {
            write!(f, "; NO NAMES: {why}")?;
        }
        writeln!(f)?;
        if self.weighed > 0 {
            f.write_str("on-CPU: ")?;
            funcs_line(f, &self.funcs, self.other_funcs, self.weighed)?;
            writeln!(f)?;
        }
        for span in self.spans.iter().flatten() {
            write!(f, "on-CPU in `{}` ({}): ", span.name, Share(span.ticks, self.weighed))?;
            funcs_line(f, &span.funcs, span.other, span.ticks)?;
            writeln!(f)?;
        }
        for callers in self.callers.iter().flatten() {
            write!(f, "callers of `{}`: ", callers.leaf.name)?;
            funcs_line(f, &callers.callers, callers.other, callers.leaf.ticks)?;
            writeln!(f)?;
        }
        let s = &self.sampler;
        write!(f, "sampler: every {} ns", s.period_ns)?;
        if s.passes > 0 {
            write!(f, ", {} passes ({} late)", s.passes, s.late)?;
        }
        if let Some(ns) = s.own_ns {
            write!(f, ", own time {}", blaze_meter::Ns(ns))?;
        }
        if let Some(why) = s.stopped {
            write!(f, ", STOPPED EARLY: {why}")?;
        }
        writeln!(f)
    }
}
