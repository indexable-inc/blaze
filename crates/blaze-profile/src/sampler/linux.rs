//! The Linux sampler: one `perf_event_open` task-clock event per thread of the process, its
//! samples (thread, time, user call chain) read from the event's mapped buffer.

use std::{
    ffi::c_void,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use blaze_hash::{HashMap, HashSet};

use super::{DEPTH, Sample, Unsampled, keep};

pub(super) const HOW: &str =
    "perf_event_open task clock of each thread, user stacks by frame pointer";

/// How often the buffers are read and the threads listed.
const EVERY: Duration = Duration::from_millis(100);
/// Pages of samples an event's buffer holds (a power of two), after its header page: 64 KiB,
/// a second and more of a thread's samples at the deepest stacks.
const DATA_PAGES: usize = 16;

const PERF_TYPE_SOFTWARE: u32 = 1;
const PERF_COUNT_SW_TASK_CLOCK: u64 = 1;
const PERF_SAMPLE_TID: u64 = 1 << 1;
const PERF_SAMPLE_TIME: u64 = 1 << 2;
const PERF_SAMPLE_CALLCHAIN: u64 = 1 << 5;
const EXCLUDE_KERNEL: u64 = 1 << 5;
const EXCLUDE_HV: u64 = 1 << 6;
const FREQ: u64 = 1 << 10;
const EXCLUDE_CALLCHAIN_KERNEL: u64 = 1 << 21;
const USE_CLOCKID: u64 = 1 << 25;
const PERF_FLAG_FD_CLOEXEC: libc::c_ulong = 8;
const PERF_RECORD_SAMPLE: u32 = 9;
/// Call chain entries at or above this mark a context (kernel, user), not an address.
const PERF_CONTEXT_MAX: u64 = -4095i64 as u64;

/// `struct perf_event_attr` to `sample_max_stack`: `PERF_ATTR_SIZE_VER5`, Linux 4.8 on.
#[repr(C)]
#[derive(Default)]
struct Attr {
    kind: u32,
    size: u32,
    config: u64,
    sample_freq: u64,
    sample_type: u64,
    read_format: u64,
    flags: u64,
    wakeup_events: u32,
    bp_type: u32,
    config1: u64,
    config2: u64,
    branch_sample_type: u64,
    sample_regs_user: u64,
    sample_stack_user: u32,
    clockid: i32,
    sample_regs_intr: u64,
    aux_watermark: u32,
    sample_max_stack: u16,
    reserved_2: u16,
}

const _: () = assert!(size_of::<Attr>() == 112);

/// One thread's event: its file and its mapped buffer.
struct Event {
    fd: libc::c_int,
    map: *mut c_void,
    len: usize,
    page: usize,
}

impl Event {
    fn open(tid: libc::pid_t, hz: u32) -> Result<Event, std::io::Error> {
        let attr = Attr {
            kind: PERF_TYPE_SOFTWARE,
            size: size_of::<Attr>() as u32,
            config: PERF_COUNT_SW_TASK_CLOCK,
            sample_freq: u64::from(hz),
            sample_type: PERF_SAMPLE_TID | PERF_SAMPLE_TIME | PERF_SAMPLE_CALLCHAIN,
            flags: EXCLUDE_KERNEL | EXCLUDE_HV | FREQ | EXCLUDE_CALLCHAIN_KERNEL | USE_CLOCKID,
            clockid: libc::CLOCK_MONOTONIC,
            sample_max_stack: DEPTH as u16,
            ..Attr::default()
        };
        // SAFETY: `attr` is a `perf_event_attr` whose `size` says how much of it there is; the
        // call reads it and returns a new file or -1.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_perf_event_open,
                &raw const attr,
                tid,
                -1 as libc::c_int,
                -1 as libc::c_int,
                PERF_FLAG_FD_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let fd = fd as libc::c_int;
        // SAFETY: `sysconf` has no precondition.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let len = page * (1 + DATA_PAGES);
        // SAFETY: a shared mapping of the event's file, of the length the kernel asks for (a
        // header page and a power of two of pages); checked for failure below.
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if map == libc::MAP_FAILED {
            let why = std::io::Error::last_os_error();
            // SAFETY: `fd` is the event's file, closed once here.
            unsafe { libc::close(fd) };
            return Err(why);
        }
        Ok(Event { fd, map, len, page })
    }

    /// `perf_event_mmap_page`'s `data_head` and `data_tail`, at bytes 1024 and 1032.
    fn head_tail(&self) -> (&AtomicU64, &AtomicU64) {
        let (head, tail) = (
            self.map.wrapping_byte_add(1024).cast::<u64>(),
            self.map.wrapping_byte_add(1032).cast::<u64>(),
        );
        // SAFETY: the header page is mapped for the event's life and the kernel keeps these two
        // `u64`s, 8-aligned, at these offsets in it; the kernel writes `data_head` and the reader
        // `data_tail`, each atomically.
        let head = unsafe { AtomicU64::from_ptr(head) };
        // SAFETY: as above.
        let tail = unsafe { AtomicU64::from_ptr(tail) };
        (head, tail)
    }

    /// Copies `out.len()` bytes from offset `at` of the data area, round its end.
    fn copy(&self, at: u64, out: &mut [u8]) {
        let size = self.page * DATA_PAGES;
        let start = at as usize % size;
        let first = out.len().min(size - start);
        let data = self.map.wrapping_byte_add(self.page).cast::<u8>().cast_const();
        // SAFETY: bytes `[start, start + first)` of the data area, which follows the header page
        // inside the mapping; the kernel does not write them until `data_tail` passes them, and
        // `out` is a separate buffer of at least `first` bytes.
        unsafe { std::ptr::copy_nonoverlapping(data.wrapping_add(start), out.as_mut_ptr(), first) };
        if first < out.len() {
            let rest = &mut out[first..];
            // SAFETY: as above, the rest from the area's start, `rest.len() <= size` bytes.
            unsafe { std::ptr::copy_nonoverlapping(data, rest.as_mut_ptr(), rest.len()) };
        }
    }

    /// The samples written since the last read, into `batch`.
    fn drain(&self, clock: &Clock, batch: &mut Vec<Sample>) {
        let (head, tail) = self.head_tail();
        let end = head.load(Ordering::Acquire);
        let mut at = tail.load(Ordering::Relaxed);
        let mut record = [0u8; 16 + 8 * (DEPTH + 8)];
        while at < end {
            let mut header = [0u8; 8];
            self.copy(at, &mut header);
            let kind = u32::from_ne_bytes([header[0], header[1], header[2], header[3]]);
            let size = u16::from_ne_bytes([header[6], header[7]]) as usize;
            if size < 8 {
                break;
            }
            if kind == PERF_RECORD_SAMPLE && size <= record.len() + 8 {
                let body = &mut record[..size - 8];
                self.copy(at + 8, body);
                if let Some(sample) = parse(body, clock) {
                    batch.push(sample);
                }
            }
            at += size as u64;
        }
        tail.store(end, Ordering::Release);
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: the mapping made in `open`, of `len`, unmapped once.
        unsafe { libc::munmap(self.map, self.len) };
        // SAFETY: the event's file, closed once.
        unsafe { libc::close(self.fd) };
    }
}

fn word(body: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_ne_bytes(body.get(at..at + 8)?.try_into().ok()?))
}

/// A `PERF_RECORD_SAMPLE` of `PERF_SAMPLE_TID | TIME | CALLCHAIN`: pid, tid, time, then the
/// chain's length and addresses.
fn parse(body: &[u8], clock: &Clock) -> Option<Sample> {
    let tid = u32::from_ne_bytes(body.get(4..8)?.try_into().ok()?);
    let time = word(body, 8)?;
    let nr = word(body, 16)? as usize;
    let mut sample =
        Sample { at: clock.instant(time), tid: u64::from(tid), depth: 0, pcs: [0; DEPTH], span: 0, build: 0, on_cpu: true };
    for i in 0..nr {
        let pc = word(body, 24 + 8 * i)?;
        if pc >= PERF_CONTEXT_MAX {
            continue;
        }
        if usize::from(sample.depth) == DEPTH {
            break;
        }
        sample.pcs[usize::from(sample.depth)] = pc;
        sample.depth += 1;
    }
    Some(sample)
}

/// `CLOCK_MONOTONIC` nanoseconds, the samples' times, turned into instants.
struct Clock {
    now: Instant,
    now_ns: u64,
}

impl Clock {
    fn now() -> Clock {
        let mut at = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: `at` is a timespec to write.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut at) };
        Clock { now: Instant::now(), now_ns: at.tv_sec as u64 * 1_000_000_000 + at.tv_nsec as u64 }
    }

    fn instant(&self, ns: u64) -> Instant {
        self.now
            .checked_sub(Duration::from_nanos(self.now_ns.saturating_sub(ns)))
            .unwrap_or(self.now)
    }
}

/// Whether the kernel opens an event on this thread, with what it says when it does not.
pub(super) fn check() -> Result<(), Unsampled> {
    // SAFETY: `gettid` has no precondition.
    let tid = unsafe { libc::gettid() };
    match Event::open(tid, super::HZ) {
        Ok(event) => {
            drop(event);
            Ok(())
        }
        Err(why) => {
            let paranoid =
                std::fs::read_to_string("/proc/sys/kernel/perf_event_paranoid").unwrap_or_default();
            // alloc: an error's message
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

/// The sampler's loop, for the process's life.
pub(super) fn run(hz: u32) {
    // SAFETY: `gettid` has no precondition.
    let own = unsafe { libc::gettid() };
    let mut events: HashMap<libc::pid_t, Event> = HashMap::default();
    let mut refused: HashSet<libc::pid_t> = HashSet::default();
    // alloc: the sampler's thread, at the edge, every 100 ms
    let mut now: Vec<libc::pid_t> = Vec::new();
    let mut batch: Vec<Sample> = Vec::with_capacity(4096);
    loop {
        let started = Instant::now();
        threads(own, &mut now);
        for &tid in &now {
            if events.contains_key(&tid) || refused.contains(&tid) {
                continue;
            }
            // A thread that ended between the listing and here is no error.
            match Event::open(tid, hz) {
                Ok(event) => _ = events.insert(tid, event),
                Err(_) => _ = refused.insert(tid),
            }
        }
        let clock = Clock::now();
        for event in events.values() {
            event.drain(&clock, &mut batch);
        }
        keep(&batch);
        batch.clear();
        now.sort_unstable();
        events.retain(|tid, _| now.binary_search(tid).is_ok());
        refused.retain(|tid| now.binary_search(tid).is_ok());
        #[allow(clippy::disallowed_methods, reason = "the sampler thread's own timer, a driver's: no pool thread or job waits here")]
        std::thread::sleep(EVERY.saturating_sub(started.elapsed()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sample_record_is_read_with_its_context_marks_left_out() {
        let clock = Clock { now: Instant::now(), now_ns: 5_000 };
        let mut body = Vec::new();
        body.extend_from_slice(&7u32.to_ne_bytes());
        body.extend_from_slice(&9u32.to_ne_bytes());
        body.extend_from_slice(&4_000u64.to_ne_bytes());
        body.extend_from_slice(&3u64.to_ne_bytes());
        body.extend_from_slice(&(-512i64 as u64).to_ne_bytes());
        body.extend_from_slice(&0x1000u64.to_ne_bytes());
        body.extend_from_slice(&0x2000u64.to_ne_bytes());
        let sample = parse(&body, &clock).unwrap();
        assert_eq!((sample.tid, sample.depth), (9, 2));
        assert_eq!(&sample.pcs[..2], &[0x1000, 0x2000]);
        assert_eq!(clock.now.duration_since(sample.at), Duration::from_nanos(1_000));
    }
}
