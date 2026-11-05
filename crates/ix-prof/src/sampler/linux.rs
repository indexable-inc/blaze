//! The Linux sampler: one `perf_event_open` task-clock event per thread of the process, its
//! samples (thread, time, user call chain) read from the event's mapped buffer by blaze's one
//! perf reader ([`crate::perf`]), which blaze-profile's sampler reads its events with too.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use super::{DEPTH, Sample, Sink, Unsampled};
use crate::perf::{Counter, Event, Record};

pub(super) const HOW: &str =
    "perf_event_open task clock of each thread of /proc/self/task, user stacks by frame pointer";

/// How often the buffers are read and the threads listed.
const EVERY: Duration = Duration::from_millis(100);
/// Pages of samples an event's buffer holds (a power of two), after its header page: 64 KiB,
/// a second and more of a thread's samples at the deepest stacks.
const DATA_PAGES: usize = 16;

/// One thread's task clock, through blaze's one perf reader ([`crate::perf`]).
fn open(tid: libc::pid_t, hz: u32) -> Result<Event, std::io::Error> {
    Event::open(tid, Counter::TaskClock { hz, switches: false }, DEPTH as u16, DATA_PAGES)
}

/// The samples written to `event` since the last read, into `batch`.
fn drain(event: &Event, period_ns: u64, batch: &mut Vec<Sample>) {
    event.drain(&mut |record| {
        let Record::Sample { tid, time, chain } = record else { return };
        // The event counts on CLOCK_MONOTONIC, the clock `crate::clock::now_ns` reads.
        let mut sample = Sample {
            t_ns: time as i64,
            tid: u64::from(tid),
            depth: 0,
            pcs: [0; DEPTH],
            on_cpu: true,
            weight_ns: period_ns,
        };
        sample.depth = chain.pcs(&mut sample.pcs) as u8;
        batch.push(sample);
    });
}

/// Whether the kernel opens an event on this thread, with what it says when it does not.
pub(super) fn check(hz: u32) -> Result<(), Unsampled> {
    // SAFETY: `gettid` has no precondition.
    let tid = unsafe { libc::gettid() };
    match open(tid, hz) {
        Ok(event) => {
            drop(event);
            Ok(())
        }
        Err(why) => {
            let paranoid =
                std::fs::read_to_string("/proc/sys/kernel/perf_event_paranoid").unwrap_or_default();
            Err(Unsampled(format!(
                "perf_event_open refused: {why} (kernel.perf_event_paranoid = {}; 2 or less, or CAP_PERFMON, lets a process sample itself)",
                paranoid.trim()
            )))
        }
    }
}

/// The threads of the process now, but the sampler's.
fn threads(own: libc::pid_t, into: &mut Vec<libc::pid_t>) {
    into.clear();
    let Ok(dir) = std::fs::read_dir("/proc/self/task") else { return };
    for entry in dir.flatten() {
        if let Some(tid) = entry.file_name().to_str().and_then(|it| it.parse().ok())
            && tid != own
        {
            into.push(tid);
        }
    }
}

/// The sampler's loop, until `stop`.
pub(super) fn run(hz: u32, sink: &Sink, stop: &AtomicBool) {
    let period_ns = 1_000_000_000 / u64::from(hz);
    // SAFETY: `gettid` has no precondition.
    let own = unsafe { libc::gettid() };
    let mut events: BTreeMap<libc::pid_t, Event> = BTreeMap::new();
    let mut refused: BTreeSet<libc::pid_t> = BTreeSet::new();
    let mut now: Vec<libc::pid_t> = Vec::new();
    let mut batch: Vec<Sample> = Vec::with_capacity(4096);
    let mut names: Vec<(u64, String)> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let started = Instant::now();
        threads(own, &mut now);
        for &tid in &now {
            if events.contains_key(&tid) || refused.contains(&tid) {
                continue;
            }
            // A thread that ended between the listing and here is no error.
            match open(tid, hz) {
                Ok(event) => {
                    _ = events.insert(tid, event);
                    if let Ok(name) = std::fs::read_to_string(format!("/proc/self/task/{tid}/comm"))
                    {
                        names.push((tid as u64, name.trim_end().to_owned()));
                    }
                }
                Err(_) => _ = refused.insert(tid),
            }
        }
        for event in events.values() {
            drain(event, period_ns, &mut batch);
        }
        sink(&batch, &names);
        batch.clear();
        names.clear();
        now.sort_unstable();
        events.retain(|tid, _| now.binary_search(tid).is_ok());
        refused.retain(|tid| now.binary_search(tid).is_ok());
        super::pause(EVERY.saturating_sub(started.elapsed()));
    }
    // The tail: what the kernel wrote since the last read, before `Sampler`'s drop returns
    // and the events (and their buffers) are closed.
    for event in events.values() {
        drain(event, period_ns, &mut batch);
    }
    sink(&batch, &names);
}
