//! The counting allocator: wraps any `GlobalAlloc`. Off (before [`crate::start`] turns it
//! on) it costs one relaxed load a call. On, it counts each thread's bytes and allocations
//! (what spans report as `alloc_bytes`/`allocs`) and samples allocations as Go does, a Poisson
//! process with a mean of `IX_PROF_ALLOC` bytes (512 KiB by default): a sampled allocation
//! keeps its stack (frame pointers) and its address in a fixed table, so its free is seen and
//! the heap profile has live bytes beside total ones.
//!
//! ```ignore
//! #[global_allocator]
//! static ALLOC: ix_prof::alloc::Counting<std::alloc::System> = ix_prof::alloc::Counting::new(std::alloc::System);
//! ```
//!
//! Nothing here allocates, locks or does IO on the allocation path: records go to a
//! preallocated lock-free queue the profiler's driver drains, and a full queue is counted.

use std::{
    alloc::{GlobalAlloc, Layout},
    cell::Cell,
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use crossbeam_queue::ArrayQueue;

/// Return addresses an allocation sample keeps.
pub const DEPTH: usize = 32;
/// Sampled blocks whose free is watched for.
const TABLE: usize = 1 << 16;
const EMPTY: u64 = 0;
const GONE: u64 = u64::MAX;

static ON: AtomicBool = AtomicBool::new(false);
static INSTALLED: AtomicBool = AtomicBool::new(false);
static RATE: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static FREED_BYTES: AtomicU64 = AtomicU64::new(0);
static DROPPED: AtomicU64 = AtomicU64::new(0);
static UNTRACKED: AtomicU64 = AtomicU64::new(0);
static QUEUE: OnceLock<ArrayQueue<Record>> = OnceLock::new();
static SAMPLED: [AtomicU64; TABLE] = [const { AtomicU64::new(EMPTY) }; TABLE];

/// A sampled allocation or the free of one.
#[derive(Clone, Copy)]
pub struct Record {
    pub t_ns: i64,
    pub tid: u64,
    pub address: u64,
    /// Bytes and allocations the sample stands for (scaled); for a free, 0 (the recorder
    /// knows what the block stood for).
    pub bytes: u64,
    pub count: u64,
    pub depth: u8,
    pub pcs: [u64; DEPTH],
    pub free: bool,
}

impl Record {
    pub fn pcs(&self) -> &[u64] {
        &self.pcs[..usize::from(self.depth)]
    }
}

struct Local {
    bytes: Cell<u64>,
    count: Cell<u64>,
    freed: Cell<u64>,
    /// Bytes left before the next sample.
    left: Cell<i64>,
    rng: Cell<u64>,
    /// Inside the profiler's own bookkeeping: its allocations are not counted.
    busy: Cell<bool>,
    stack_high: Cell<u64>,
}

thread_local! {
    static LOCAL: Local = const {
        Local {
            bytes: Cell::new(0),
            count: Cell::new(0),
            freed: Cell::new(0),
            left: Cell::new(0),
            rng: Cell::new(0),
            busy: Cell::new(false),
            stack_high: Cell::new(0),
        }
    };
}

/// Wraps `A`, counting and sampling while the profiler is on.
pub struct Counting<A> {
    inner: A,
}

impl<A> Counting<A> {
    pub const fn new(inner: A) -> Counting<A> {
        Counting { inner }
    }
}

/// Turns counting on, sampling a mean of one allocation every `rate` bytes (`None`: counts
/// only). Says whether a [`Counting`] allocator is installed (it has seen a call).
pub(crate) fn turn_on(rate: Option<u64>) -> bool {
    QUEUE.get_or_init(|| ArrayQueue::new(8192));
    RATE.store(rate.unwrap_or(0), Ordering::Relaxed);
    ON.store(true, Ordering::Release);
    INSTALLED.load(Ordering::Relaxed)
}

pub(crate) fn turn_off() {
    ON.store(false, Ordering::Release);
}

/// Whether a [`Counting`] allocator has served a call in this process.
pub fn installed() -> bool {
    INSTALLED.load(Ordering::Relaxed)
}

/// What this thread has allocated while on: (bytes, allocations, bytes freed).
pub fn thread_usage() -> Option<(u64, u64, u64)> {
    if !INSTALLED.load(Ordering::Relaxed) {
        return None;
    }
    LOCAL.try_with(|l| (l.bytes.get(), l.count.get(), l.freed.get())).ok()
}

/// Bytes allocated and freed in the process while on.
pub fn totals() -> (u64, u64) {
    (ALLOC_BYTES.load(Ordering::Relaxed), FREED_BYTES.load(Ordering::Relaxed))
}

/// Records lost: (queue full, table of sampled blocks full).
pub fn lost() -> (u64, u64) {
    (DROPPED.load(Ordering::Relaxed), UNTRACKED.load(Ordering::Relaxed))
}

/// Takes the records the allocator queued, into `into`.
pub fn drain(into: &mut Vec<Record>) {
    if let Some(q) = QUEUE.get() {
        while let Some(r) = q.pop() {
            into.push(r);
        }
    }
}

/// Runs `f` with this thread's allocations not counted or sampled (the profiler's own).
pub fn quiet<T>(f: impl FnOnce() -> T) -> T {
    let was = LOCAL.try_with(|l| l.busy.replace(true)).unwrap_or(true);
    let out = f();
    let _ = LOCAL.try_with(|l| l.busy.set(was));
    out
}

// SAFETY: every call forwards to `inner` with the caller's arguments and returns its result;
// the bookkeeping around it allocates nothing through this allocator (see `on_alloc`).
unsafe impl<A: GlobalAlloc> GlobalAlloc for Counting<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        let p = unsafe { self.inner.alloc(layout) };
        seen();
        if ON.load(Ordering::Relaxed) && !p.is_null() {
            on_alloc(p as u64, layout.size() as u64);
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        let p = unsafe { self.inner.alloc_zeroed(layout) };
        seen();
        if ON.load(Ordering::Relaxed) && !p.is_null() {
            on_alloc(p as u64, layout.size() as u64);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // Before the block can be handed out again, so its address names this block.
        if ON.load(Ordering::Relaxed) {
            on_free(ptr as u64, layout.size() as u64);
        }
        // SAFETY: the caller's contract, passed on.
        unsafe { self.inner.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let on = ON.load(Ordering::Relaxed);
        if on {
            on_free(ptr as u64, layout.size() as u64);
        }
        // SAFETY: the caller's contract, passed on.
        let p = unsafe { self.inner.realloc(ptr, layout, new_size) };
        if on && !p.is_null() {
            on_alloc(p as u64, new_size as u64);
        }
        p
    }
}

#[inline]
fn seen() {
    if !INSTALLED.load(Ordering::Relaxed) {
        INSTALLED.store(true, Ordering::Relaxed);
    }
}

fn slot(address: u64) -> usize {
    // Fibonacci hashing of the address, blocks being at least 8-aligned.
    ((address >> 3).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> (64 - TABLE.trailing_zeros())) as usize
}

const PROBES: usize = 16;

fn on_alloc(address: u64, size: u64) {
    let _ = LOCAL.try_with(|l| {
        if l.busy.get() {
            return;
        }
        l.bytes.set(l.bytes.get() + size);
        l.count.set(l.count.get() + 1);
        ALLOC_BYTES.fetch_add(size, Ordering::Relaxed);
        let rate = RATE.load(Ordering::Relaxed);
        if rate == 0 {
            return;
        }
        let left = l.left.get() - size as i64;
        if left > 0 {
            l.left.set(left);
            return;
        }
        l.left.set(next_interval(l, rate) as i64);
        l.busy.set(true);
        sample(l, address, size, rate);
        l.busy.set(false);
    });
}

fn on_free(address: u64, size: u64) {
    let _ = LOCAL.try_with(|l| {
        if l.busy.get() {
            return;
        }
        l.freed.set(l.freed.get() + size);
        FREED_BYTES.fetch_add(size, Ordering::Relaxed);
        if RATE.load(Ordering::Relaxed) == 0 {
            return;
        }
        let at = slot(address);
        for i in 0..PROBES {
            let s = &SAMPLED[(at + i) % TABLE];
            match s.load(Ordering::Relaxed) {
                EMPTY => return,
                it if it == address => {
                    if s.compare_exchange(address, GONE, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
                        push(Record {
                            t_ns: crate::clock::now_ns(),
                            tid: crate::clock::os_tid(),
                            address,
                            bytes: 0,
                            count: 0,
                            depth: 0,
                            pcs: [0; DEPTH],
                            free: true,
                        });
                    }
                    return;
                }
                _ => {}
            }
        }
    });
}

/// The next sampling interval: exponential with mean `rate` (Go's `nextSample`).
fn next_interval(l: &Local, rate: u64) -> u64 {
    let mut x = l.rng.get();
    if x == 0 {
        x = (crate::clock::os_tid() ^ 0x2545_f491_4f6c_dd1d) | 1;
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    l.rng.set(x);
    // A uniform in (0, 1].
    let u = ((x >> 11) as f64 + 1.0) / (1u64 << 53) as f64;
    (-(u.ln()) * rate as f64).max(1.0) as u64
}

#[inline(never)]
fn sample(l: &Local, address: u64, size: u64, rate: u64) {
    // Go's unsampling: a sample of an allocation of `size` stands for 1 / (1 - e^(-size/rate)).
    let scale = 1.0 / (1.0 - (-(size as f64) / rate as f64).exp());
    let mut r = Record {
        t_ns: crate::clock::now_ns(),
        tid: crate::clock::os_tid(),
        address,
        bytes: (size as f64 * scale).round() as u64,
        count: scale.round().max(1.0) as u64,
        depth: 0,
        pcs: [0; DEPTH],
        free: false,
    };
    walk(l, &mut r);
    let at = slot(address);
    let mut tracked = false;
    for i in 0..PROBES {
        let s = &SAMPLED[(at + i) % TABLE];
        let old = s.load(Ordering::Relaxed);
        if (old == EMPTY || old == GONE)
            && s.compare_exchange(old, address, Ordering::AcqRel, Ordering::Relaxed).is_ok()
        {
            tracked = true;
            break;
        }
    }
    if !tracked {
        UNTRACKED.fetch_add(1, Ordering::Relaxed);
    }
    push(r);
}

fn push(r: Record) {
    match QUEUE.get() {
        Some(q) if q.push(r).is_ok() => {}
        _ => {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// This thread's frame records, from the caller of `sample` up, inside its stack.
#[inline(always)]
fn walk(l: &Local, r: &mut Record) {
    let Some(mut fp) = frame_pointer() else { return };
    let low = fp;
    let high = match l.stack_high.get() {
        0 => {
            let high = stack_high().unwrap_or(0);
            l.stack_high.set(high);
            high
        }
        it => it,
    };
    if high == 0 {
        return;
    }
    while fp >= low && fp % 8 == 0 && fp.saturating_add(16) <= high && usize::from(r.depth) < DEPTH {
        let record = fp as *const u64;
        // SAFETY: `fp` is 8-aligned and its 16 bytes lie between this frame and the top of this
        // thread's stack, which is mapped while the thread runs.
        let next = unsafe { record.read_volatile() };
        // SAFETY: as above, the second word.
        let ret = unsafe { record.wrapping_add(1).read_volatile() };
        if ret == 0 {
            break;
        }
        r.pcs[usize::from(r.depth)] = ret;
        r.depth += 1;
        if next <= fp {
            break;
        }
        fp = next;
    }
}

#[inline(always)]
fn frame_pointer() -> Option<u64> {
    #[cfg(target_arch = "aarch64")]
    {
        let fp: u64;
        // SAFETY: reads a register.
        unsafe { std::arch::asm!("mov {}, x29", out(reg) fp, options(nomem, nostack, preserves_flags)) };
        Some(fp & ((1 << 47) - 1))
    }
    #[cfg(target_arch = "x86_64")]
    {
        let fp: u64;
        // SAFETY: reads a register.
        unsafe { std::arch::asm!("mov {}, rbp", out(reg) fp, options(nomem, nostack, preserves_flags)) };
        Some(fp)
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        None
    }
}

/// The top of this thread's stack. May allocate (glibc reads /proc for the main thread); the
/// caller is marked busy, so that goes straight to the inner allocator.
fn stack_high() -> Option<u64> {
    #[cfg(target_vendor = "apple")]
    {
        // SAFETY: the calling thread's own pthread, alive.
        let high = unsafe { libc::pthread_get_stackaddr_np(libc::pthread_self()) } as u64;
        (high != 0).then_some(high)
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: an attr to fill for the calling thread, destroyed below.
        let mut attr: libc::pthread_attr_t = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        if unsafe { libc::pthread_getattr_np(libc::pthread_self(), &mut attr) } != 0 {
            return None;
        }
        let mut addr: *mut libc::c_void = std::ptr::null_mut();
        let mut size: libc::size_t = 0;
        // SAFETY: `attr` was filled above.
        let ok = unsafe { libc::pthread_attr_getstack(&attr, &mut addr, &mut size) } == 0;
        // SAFETY: filled by `pthread_getattr_np`, destroyed once.
        unsafe { libc::pthread_attr_destroy(&mut attr) };
        ok.then(|| addr as u64 + size as u64)
    }
    #[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
    {
        None
    }
}
