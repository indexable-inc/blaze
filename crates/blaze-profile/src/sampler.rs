//! Periodic CPU sampling, for a profiler that runs in production: [`start`]ed once by the
//! daemon, it takes [`HZ`] samples a second of each thread that records spans, while it runs
//! and while it blocks inside a span, each a stack of addresses with the span and build it was
//! taken in, kept in a fixed ring ([`KEPT`] samples), and names them only when a dump or flush
//! is written ([`crate::symbols`]).
//!
//! Both systems sample the same threads with the same meaning of `on_cpu`: a thread that holds
//! a ring, running or runnable (on), or blocked inside a span (off); a blocked thread outside
//! every span is not sampled. Each walks frame pointers (blaze keeps them on both).
//!
//! - Linux: `perf_event_open`, no signals: the sampler's thread opens a task-clock event with
//!   switch records on each such thread and reads its ring; the kernel walks the user stack of
//!   each sample on the CPU, and a block inside a span is weighed from its switch out and in
//!   into samples off the CPU with no stack (`linux.rs` says why). Each sample's span, job and
//!   fiber come from its ring's context log at the sample's time
//!   ([`crate::flight::Ring::context_at`]), since perf's samples arrive after the fact. Where
//!   the kernel refuses the event, [`start`] says why.
//! - macOS (aarch64): no kernel sampler is open to a process for itself, but a thread of the
//!   process may stop another: each tick the sampler stops each such thread
//!   (`thread_suspend`), reads its registers, walks its frame pointers inside its own stack,
//!   reads its innermost span, job and fiber from its ring, and lets it go. Nothing is
//!   allocated and no lock taken while a thread is stopped.
//!
//! Nothing signals a thread: a signal interrupts the kernel wait it lands in
//! (docs/v2/plan/design/deadline-waits.md).
//! - Elsewhere [`start`] says there is no sampler.

use std::{sync::atomic::AtomicBool, time::Instant};

use blaze_core::lock::{Lock, LockGuard};

use crate::{dump::Sampled, flight::Epoch};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod macos;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use macos as platform;

/// Samples a second of a thread's CPU time: prime, so it beats with no periodic work.
pub const HZ: u32 = 49;
/// Addresses a sample keeps, the innermost first: where the thread was, then each return
/// address up its stack.
pub(crate) const DEPTH: usize = 48;
/// Samples kept: about 400 bytes each, 3.3 MB.
const KEPT: usize = 8192;

#[derive(Clone, Copy)]
pub(crate) struct Sample {
    pub(crate) at: Instant,
    pub(crate) tid: u64,
    pub(crate) depth: u8,
    pub(crate) pcs: [u64; DEPTH],
    /// The innermost span its thread was inside, 0 for none or not known, and that span's
    /// build (0: none).
    pub(crate) span: u64,
    pub(crate) build: u32,
    /// Whether it was running or runnable (on) or blocked (off) as it was sampled: a thread
    /// blocked inside a span is sampled too, so a block no wait span says shows in its span
    /// (and in its stack on macOS; Linux's blocked samples have none, depth 0).
    pub(crate) on_cpu: bool,
    /// The job running on its thread as it was sampled (its slot's generation << 32 | its
    /// slot, as switch rows name it; a pool job's own number before it joined the graph), and
    /// its fiber: 0 for none. Not a column of the profile's `samples` table yet.
    pub(crate) job: u64,
    pub(crate) fiber: u32,
}

/// What a [`Ring`] holds.
struct Samples {
    ring: Vec<Sample>,
    next: u64,
    /// What the sampler is: on and how, or why it is off.
    state: String,
}

impl Samples {
    /// Keeps `batch`, over the oldest once [`KEPT`] are.
    fn keep(&mut self, batch: &[Sample]) {
        for sample in batch {
            if self.ring.len() < KEPT {
                self.ring.push(*sample);
            } else {
                let at = (self.next % KEPT as u64) as usize;
                self.ring[at] = *sample;
            }
            self.next += 1;
        }
    }
}

/// A ring of samples, the newest [`KEPT`]. The process's is [`process`], which the sampler
/// [`start`]ed in this process keeps into; a history reads the ring it is given
/// ([`crate::history::History::open`]), so a test's history reads a ring of the test's own and
/// never what a sampler of the same process took.
pub struct Ring(Lock<Samples>);

impl Ring {
    /// An empty ring.
    pub const fn new() -> Ring {
        // alloc: none until the first sample, then once a ring
        Ring(Lock::unrecorded("profile.sampler.ring", Samples { ring: Vec::new(), next: 0, state: String::new() }))
    }

    fn lock(&self) -> LockGuard<'_, Samples> {
        self.0.lock()
    }

    /// Keeps `batch`, over the oldest once [`KEPT`] are.
    pub(crate) fn keep(&self, batch: &[Sample]) {
        self.lock().keep(batch);
    }

    /// The samples kept since sample number `next` (of every sample ever kept), into `into`;
    /// moves `next` past them. Samples written over before they were read are left out: their
    /// count is the result.
    pub(crate) fn since(&self, next: &mut u64, into: &mut Vec<Sample>) -> u64 {
        let samples = self.lock();
        let total = samples.next;
        let first = (*next).max(total.saturating_sub(KEPT as u64));
        let skipped = first.saturating_sub(*next);
        for i in first..total {
            into.push(samples.ring[(i % KEPT as u64) as usize]);
        }
        *next = total;
        skipped
    }
}

impl Default for Ring {
    fn default() -> Ring {
        Ring::new()
    }
}

static SAMPLES: Ring = Ring::new();

/// The process's ring: what the sampler [`start`]ed in this process keeps into, and what a
/// daemon's history reads.
pub fn process() -> &'static Ring {
    &SAMPLES
}

/// A ring nothing keeps into, for a test's history: it reads no CPU samples but its test's.
#[cfg(test)]
pub(crate) fn unsampled() -> &'static Ring {
    static NONE: Ring = Ring::new();
    &NONE
}

/// Keeps `batch` in the process's ring, over the oldest once [`KEPT`] are.
pub(crate) fn keep(batch: &[Sample]) {
    SAMPLES.keep(batch);
}

/// Why the process takes no CPU samples.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unsampled(pub String);

impl std::fmt::Display for Unsampled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no CPU samples: {}", self.0)
    }
}

impl std::error::Error for Unsampled {}

/// Starts the sampler's thread, once a process, at `hz` samples a CPU second a thread. A
/// sampler that cannot be made is an error saying why, and every dump says it too.
pub fn start(hz: u32) -> Result<(), Unsampled> {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return Ok(());
    }
    let started = run(hz.max(1));
    let mut samples = SAMPLES.lock();
    samples.state = match &started {
        // alloc: once a process
        Ok(how) => format!("on: {hz} Hz, {how}"),
        // alloc: once a process
        Err(why) => format!("off: {}", why.0),
    };
    if started.is_ok() {
        samples.ring.reserve_exact(KEPT);
    }
    started.map(drop)
}

#[cfg(any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64")))]
fn run(hz: u32) -> Result<&'static str, Unsampled> {
    platform::check()?;
    std::thread::Builder::new()
        // alloc: once a process
        .name("blaze-sampler".to_owned())
        .spawn(move || platform::run(hz))
        // alloc: an error's message
        .map_err(|why| Unsampled(format!("its thread did not start: {why}")))?;
    Ok(platform::HOW)
}

#[cfg(not(any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64"))))]
fn run(_hz: u32) -> Result<&'static str, Unsampled> {
    // alloc: an error's message
    Err(Unsampled(format!(
        "blaze has no in-process sampler for {}-{}",
        std::env::consts::ARCH,
        std::env::consts::OS
    )))
}

#[cfg(target_os = "linux")]
pub(crate) use linux::Thread;
/// What a thread's ring holds for the sampler, under the lock the thread takes to leave, so no
/// thread is stopped or read once it is gone: on macOS its port and stack, on Linux its tid
/// and the perf events the sampler opened on it.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(crate) use macos::Thread;

#[cfg(not(any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64"))))]
#[derive(Default)]
pub(crate) struct Thread;

#[cfg(not(any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64"))))]
impl Thread {
    pub(crate) fn enter(&mut self, _tid: u64) {}
    pub(crate) fn leave(&mut self) {}
}

/// The operating system's number for this thread, as `perf` and `top` show it.
pub(crate) fn os_tid() -> u64 {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // SAFETY: `gettid` has no precondition.
        let tid = unsafe { libc::gettid() };
        tid as u64
    }
    #[cfg(target_vendor = "apple")]
    {
        let mut tid = 0u64;
        // SAFETY: a null thread is the calling one; `tid` is a `u64` to write.
        unsafe { libc::pthread_threadid_np(0 as libc::pthread_t, &mut tid) };
        tid
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        std::thread_local!(static TID: u64 = NEXT.fetch_add(1, Ordering::Relaxed));
        TID.with(|it| *it)
    }
}

/// What the platform's sampler counted that a dump says beside its state; empty for none.
fn said() -> String {
    #[cfg(target_os = "linux")]
    return linux::said();
    #[cfg(not(target_os = "linux"))]
    String::new()
}

/// The samples taken since `since`, named, and what the sampler is.
pub(crate) fn taken(since: Instant, epoch: &Epoch) -> (Vec<Sampled>, String) {
    let samples = SAMPLES.lock();
    let state = match samples.state.is_empty() {
        // alloc: a dump, on demand
        true => "off: not started in this process (`blaze serve` starts it)".to_owned(),
        // alloc: a dump, on demand
        false => format!("{}{}", samples.state, said()),
    };
    // alloc: a dump's copy of the samples, on demand
    let kept: Vec<Sample> = samples.ring.iter().filter(|it| it.at >= since).copied().collect();
    drop(samples);
    let frames = crate::symbols::frames(kept.iter().map(|it| &it.pcs[..usize::from(it.depth)]));
    let mut sampled: Vec<Sampled> = kept
        .iter()
        .map(|it| Sampled {
            tid: it.tid,
            at_us: epoch.us(it.at),
            span: it.span,
            job: it.job,
            fiber: it.fiber,
            // Innermost first, as dumps read it: each address's frames (outermost first)
            // reversed, so an inlined function comes before the one it was inlined into.
            stack: it.pcs[..usize::from(it.depth)]
                .iter()
                .enumerate()
                .flat_map(|(i, pc)| {
                    frames
                        .get(&crate::symbols::lookup(i, *pc))
                        .into_iter()
                        .flat_map(|it| it.iter().rev())
                        .cloned()
                })
                // alloc: a dump, on demand
                .collect(),
        })
        // alloc: a dump, on demand
        .collect();
    sampled.sort_by_key(|it| it.at_us);
    (sampled, state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_keeps_the_newest_samples() {
        let at = Instant::now();
        let sample = |tid| Sample {
            at,
            tid,
            depth: 0,
            pcs: [0; DEPTH],
            span: 0,
            build: 0,
            on_cpu: true,
            job: 0,
            fiber: 0,
        };
        let batch: Vec<Sample> = (0..KEPT as u64 + 10).map(sample).collect();
        // A ring of its own: the process's is the sampler's, which another test runs.
        let mut samples = Samples { ring: Vec::new(), next: 0, state: String::new() };
        samples.keep(&batch);
        assert_eq!(samples.ring.len(), KEPT);
        let mut tids: Vec<u64> = samples.ring.iter().map(|it| it.tid).collect();
        tids.sort_unstable();
        assert_eq!(tids.first(), Some(&10));
        assert_eq!(tids.last(), Some(&(KEPT as u64 + 9)));
    }

    /// On a system with a sampler, a thread that spins is sampled, and its samples name it.
    #[test]
    fn a_busy_thread_is_sampled_and_named() {
        if let Err(why) = start(HZ * 20) {
            // The system refused the sampler (Linux: `perf_event_open` refused by
            // `kernel.perf_event_paranoid` or a seccomp filter): the error says so, and so do
            // dumps.
            assert!(!why.0.is_empty());
            return;
        }
        #[inline(never)]
        fn spins_for_the_sampler(until: Instant) -> u64 {
            let mut n = 0u64;
            while Instant::now() < until {
                n = std::hint::black_box(n.wrapping_add(1));
            }
            n
        }
        let since = Instant::now();
        std::thread::Builder::new()
            .name("spinner".to_owned())
            .spawn(|| {
                // Both samplers sample threads that recorded a span (hold a ring).
                drop(tracing::info_span!(target: "blaze", "spinning").entered());
                crate::flight::plot(crate::flight::Plot::Queued, 1);
                spins_for_the_sampler(Instant::now() + std::time::Duration::from_millis(1500))
            })
            .unwrap()
            .join()
            .unwrap();
        let mut kept = None;
        blaze_graph::spin::SpinWait::within(
            "the sampler to keep the spinner's samples",
            std::time::Duration::from_secs(10),
            || {
                let (samples, state) = taken(since, &Epoch::now());
                let named = samples
                    .iter()
                    .any(|it| it.stack.iter().any(|name| name.contains("spins_for_the_sampler")));
                kept = Some((samples, state));
                named
            },
        );
        let (samples, state) = kept.expect("taken at least once");
        assert!(state.starts_with("on:"), "{state}");
        assert!(!samples.is_empty(), "a thread that spun for 1.5 s at 980 Hz was not sampled");
        assert!(
            samples
                .iter()
                .any(|it| it.stack.iter().any(|name| name.contains("spins_for_the_sampler"))),
            "{:?}",
            samples.iter().map(|it| it.stack.first()).collect::<Vec<_>>()
        );
    }
}
