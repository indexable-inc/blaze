//! The CPU sampler, a driver: a thread that takes `hz` samples a second of each thread of the
//! process, with no registration and no code change in the program, and hands them to a sink
//! (the recorder) in batches. Ported from blaze-profile's sampler, which sampled only threads
//! that recorded a span.
//!
//! - Linux: `perf_event_open`, the task clock of each thread in `/proc/self/task` (an idle
//!   thread costs nothing), user call chains the kernel walks by frame pointer. Where the
//!   kernel refuses (`kernel.perf_event_paranoid` above 2 without `CAP_PERFMON`), [`start`]
//!   says so.
//! - macOS (arm64): each tick every thread of `task_threads` that is running is stopped
//!   (`thread_suspend`), its registers read, its frame pointers walked inside its own stack, and
//!   let go; nothing is allocated and no lock taken while it is stopped. A blocked thread is
//!   sampled every [`OFF_CPU_EVERY`]th tick, marked off the CPU, weighted for the ticks skipped.
//! - Elsewhere [`start`] says there is no sampler.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod macos;

/// Return addresses a sample keeps, the innermost first.
pub const DEPTH: usize = 64;
/// A blocked thread is sampled once in this many ticks.
pub const OFF_CPU_EVERY: u32 = 10;

#[derive(Clone, Copy)]
pub struct Sample {
    pub t_ns: i64,
    pub tid: u64,
    pub depth: u8,
    pub pcs: [u64; DEPTH],
    pub on_cpu: bool,
    pub weight_ns: u64,
}

impl Sample {
    pub fn pcs(&self) -> &[u64] {
        &self.pcs[..usize::from(self.depth)]
    }
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

/// Where samples go: a batch of samples and the names of threads seen for the first time.
pub type Sink = Arc<dyn Fn(&[Sample], &[(u64, String)]) + Send + Sync>;

/// A running sampler. Dropping it stops the thread and joins it: by the time the drop
/// returns, every sample taken (the last batch, and on Linux what was left in the kernel's
/// buffers) has gone to the sink.
pub struct Sampler {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    pub how: &'static str,
}

impl Drop for Sampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            // The loop parks between ticks; this ends the park now rather than at the tick.
            thread.thread().unpark();
            let _wait = tracing::info_span!(target: "blaze::ix_prof", "join", wait = "thread", on = "ix-prof-sampler").entered();
            // A panic in the sampler thread was already printed by the panic hook; the
            // profile is written with the samples that reached the sink.
            let _ = thread.join();
        }
    }
}

/// Starts a sampler thread at `hz` samples a CPU second a thread.
pub fn start(hz: u32, sink: Sink) -> Result<Sampler, Unsampled> {
    let hz = hz.clamp(1, 10_000);
    let stop = Arc::new(AtomicBool::new(false));
    let (thread, how) = run(hz, sink, Arc::clone(&stop))?;
    Ok(Sampler { stop, thread: Some(thread), how })
}

/// Waits out `d` on the sampler thread, or less when [`Sampler`]'s drop unparks it.
#[cfg(any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64")))]
fn pause(d: std::time::Duration) {
    #[allow(
        clippy::disallowed_methods,
        reason = "the sampler thread's own timer, a driver's: no pool thread or job waits here, and the stop unparks it"
    )]
    std::thread::park_timeout(d);
}

#[cfg(any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64")))]
fn run(
    hz: u32,
    sink: Sink,
    stop: Arc<AtomicBool>,
) -> Result<(std::thread::JoinHandle<()>, &'static str), Unsampled> {
    #[cfg(target_os = "linux")]
    use linux as platform;
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    use macos as platform;
    platform::check(hz)?;
    let thread = std::thread::Builder::new()
        .name("ix-prof-sampler".to_owned())
        .spawn(move || platform::run(hz, &sink, &stop))
        .map_err(|why| Unsampled(format!("its thread did not start: {why}")))?;
    Ok((thread, platform::HOW))
}

#[cfg(not(any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64"))))]
fn run(
    _hz: u32,
    _sink: Sink,
    _stop: Arc<AtomicBool>,
) -> Result<(std::thread::JoinHandle<()>, &'static str), Unsampled> {
    Err(Unsampled(format!(
        "ix-prof has no in-process sampler for {}-{} yet (a SIGPROF sampler is planned)",
        std::env::consts::ARCH,
        std::env::consts::OS
    )))
}
