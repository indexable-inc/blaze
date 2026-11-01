//! Periodic CPU sampling, for a profiler that runs in production: [`start`]ed once by the
//! daemon, it takes [`HZ`] samples a second of CPU time of each of the process's threads, each a
//! stack of return addresses kept in a fixed ring ([`KEPT`] samples), and names them only when
//! a dump is written ([`crate::symbols`]).
//!
//! - Linux: `perf_event_open`, the task clock of each thread (so an idle thread costs nothing),
//!   user-space call chains the kernel walks by frame pointer, read from each event's mapped
//!   buffer by the sampler's thread every 100 ms. Where the kernel refuses
//!   (`kernel.perf_event_paranoid` above 2 for a user without `CAP_PERFMON`), [`start`] says so.
//! - macOS (aarch64): no kernel sampler is open to a process for itself, but a thread of the
//!   process may stop another: each tick the sampler stops each running thread that recorded a
//!   span (`thread_suspend`), reads its registers, walks its frame pointers inside its own stack
//!   (frame pointers are always kept on arm64 macOS), and lets it go. Nothing is allocated and
//!   no lock taken while a thread is stopped.
//! - Elsewhere [`start`] says there is no sampler.

use std::{sync::atomic::AtomicBool, time::Instant};

use parking_lot::{Mutex, MutexGuard};

use crate::{Sampled, flight::Epoch};

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
/// Return addresses a sample keeps, the innermost first.
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
    /// Whether it was on a CPU: a blocked thread is sampled too where the sampler can stop it
    /// (macOS), so a block no wait span says shows in its stack.
    pub(crate) on_cpu: bool,
}

struct Samples {
    ring: Vec<Sample>,
    next: u64,
    /// What the sampler is: on and how, or why it is off.
    state: String,
}

static SAMPLES: Mutex<Samples> =
    // alloc: none until the first sample, then once a process
    Mutex::new(Samples { ring: Vec::new(), next: 0, state: String::new() });

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock()
}

/// Keeps `batch`, over the oldest once [`KEPT`] are.
pub(crate) fn keep(batch: &[Sample]) {
    let mut samples = lock(&SAMPLES);
    for sample in batch {
        if samples.ring.len() < KEPT {
            samples.ring.push(*sample);
        } else {
            let at = (samples.next % KEPT as u64) as usize;
            samples.ring[at] = *sample;
        }
        samples.next += 1;
    }
}

/// The samples kept since sample number `next` (of every sample ever kept), into `into`; moves
/// `next` past them. Samples written over before they were read are left out: their count is
/// the result.
pub(crate) fn since(next: &mut u64, into: &mut Vec<Sample>) -> u64 {
    let samples = lock(&SAMPLES);
    let total = samples.next;
    let first = (*next).max(total.saturating_sub(KEPT as u64));
    let skipped = first.saturating_sub(*next);
    for i in first..total {
        into.push(samples.ring[(i % KEPT as u64) as usize]);
    }
    *next = total;
    skipped
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
    let mut samples = lock(&SAMPLES);
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

/// What a thread's ring holds for the sampler: on macOS its port and stack, under the lock the
/// thread takes to leave, so no thread is stopped once it is gone.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(crate) use macos::Thread;

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
#[derive(Default)]
pub(crate) struct Thread;

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
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

/// The samples taken since `since`, named, and what the sampler is.
pub(crate) fn taken(since: Instant, epoch: &Epoch) -> (Vec<Sampled>, String) {
    let samples = lock(&SAMPLES);
    let state = match samples.state.is_empty() {
        // alloc: a dump, on demand
        true => "off: not started in this process (`blaze serve` starts it)".to_owned(),
        // clone: a dump, on demand
        false => samples.state.clone(),
    };
    // alloc: a dump's copy of the samples, on demand
    let kept: Vec<Sample> = samples.ring.iter().filter(|it| it.at >= since).copied().collect();
    drop(samples);
    let pcs: Vec<u64> =
        // alloc: a dump, on demand
        kept.iter().flat_map(|it| it.pcs[..usize::from(it.depth)].iter().copied()).collect();
    let names = crate::symbols::names(&pcs);
    let mut sampled: Vec<Sampled> = kept
        .iter()
        .map(|it| Sampled {
            tid: it.tid,
            at_us: epoch.us(it.at),
            stack: it.pcs[..usize::from(it.depth)]
                .iter()
                // alloc: a dump, on demand
                .map(|pc| names.get(pc).cloned().unwrap_or_else(|| format!("{pc:#x}")))
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
        let sample = |tid| Sample { at, tid, depth: 0, pcs: [0; DEPTH], span: 0, build: 0, on_cpu: true };
        let batch: Vec<Sample> = (0..KEPT as u64 + 10).map(sample).collect();
        keep(&batch);
        let samples = lock(&SAMPLES);
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
            // The kernel refuses perf events to this user: the error says so, and so do dumps.
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
                // The macOS sampler stops threads that recorded a span.
                drop(tracing::info_span!(target: "blaze", "spinning").entered());
                crate::flight::plot(crate::flight::Plot::Queued, 1);
                spins_for_the_sampler(Instant::now() + std::time::Duration::from_millis(1500))
            })
            .unwrap()
            .join()
            .unwrap();
        let mut kept = None;
        blaze_graph::spin::SpinWait::within("the sampler to keep the spinner's samples", std::time::Duration::from_secs(10), || {
            let (samples, state) = taken(since, &Epoch::now());
            let named = samples.iter().any(|it| it.stack.iter().any(|name| name.contains("spins_for_the_sampler")));
            kept = Some((samples, state));
            named
        });
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
