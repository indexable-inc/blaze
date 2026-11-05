//! blaze's one `perf_event_open` reader, shared by ix-prof's sampler (`sampler/linux.rs`) and
//! blaze-profile's (`blaze_profile::sampler`): an event on one thread of this process, its
//! records (samples, context switches, losses) read from its mapped ring by a thread that
//! never blocks on it. No feature gate: it needs nothing but `libc`.
//!
//! One counter, the thread's task clock: a sample every `1/hz` of its CPU time, user mode only
//! (`exclude_kernel`), with the user call chain the kernel walks by frame pointer and a time on
//! `CLOCK_MONOTONIC` (`use_clockid`), the clock `Instant` and blaze's `mono_ns` read; and, when
//! asked, `PERF_RECORD_SWITCH` records in the same ring at every switch of the thread in and
//! out (`context_switch`, with `sample_id_all` for their thread and time), an out one saying
//! whether the thread was preempted. Both need no more than `kernel.perf_event_paranoid` 2, so a
//! process samples itself on a stock kernel without `CAP_PERFMON`.
//!
//! No context-switch counter: the kernel takes `PERF_COUNT_SW_CONTEXT_SWITCHES` samples in the
//! scheduler, in kernel mode, and drops every one of an event that excludes the kernel; one that
//! counts the kernel needs `kernel.perf_event_paranoid` 1 or `CAP_PERFMON`. So a switch out
//! comes with its time and whether it was a preemption, never the stack it blocked in.

use std::{
    ffi::c_void,
    sync::atomic::{AtomicU64, Ordering},
};

const PERF_TYPE_SOFTWARE: u32 = 1;
const PERF_COUNT_SW_TASK_CLOCK: u64 = 1;
const PERF_SAMPLE_TID: u64 = 1 << 1;
const PERF_SAMPLE_TIME: u64 = 1 << 2;
const PERF_SAMPLE_CALLCHAIN: u64 = 1 << 5;
const EXCLUDE_KERNEL: u64 = 1 << 5;
const EXCLUDE_HV: u64 = 1 << 6;
const FREQ: u64 = 1 << 10;
const SAMPLE_ID_ALL: u64 = 1 << 18;
const EXCLUDE_CALLCHAIN_KERNEL: u64 = 1 << 21;
const USE_CLOCKID: u64 = 1 << 25;
const CONTEXT_SWITCH: u64 = 1 << 26;
const PERF_FLAG_FD_CLOEXEC: libc::c_ulong = 8;
const PERF_RECORD_LOST: u32 = 2;
const PERF_RECORD_SAMPLE: u32 = 9;
const PERF_RECORD_LOST_SAMPLES: u32 = 13;
const PERF_RECORD_SWITCH: u32 = 14;
const PERF_RECORD_MISC_SWITCH_OUT: u16 = 1 << 13;
const PERF_RECORD_MISC_SWITCH_OUT_PREEMPT: u16 = 1 << 14;
/// Call chain entries at or above this mark a context (kernel, user), not an address.
const PERF_CONTEXT_MAX: u64 = -4095i64 as u64;
/// The largest record [`Event::drain`] reads: a chain of about 500 entries. A bigger one (never,
/// with `sample_max_stack` at most a few hundred) is skipped and counted as lost.
const RECORD_MAX: usize = 4096;

/// `struct perf_event_attr` to `sample_max_stack`: `PERF_ATTR_SIZE_VER5`, Linux 4.8 on.
#[repr(C)]
#[derive(Default)]
struct Attr {
    kind: u32,
    size: u32,
    config: u64,
    sample_period_or_freq: u64,
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

/// What an [`Event`] counts and samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Counter {
    /// The thread's CPU time, `hz` samples a CPU second, user mode only; with `switches`, a
    /// [`Record::Switch`] at every switch of the thread in and out too.
    TaskClock { hz: u32, switches: bool },
}

impl Counter {
    /// What the kernel needs to open it, for an error that names it.
    pub const fn needs(self) -> &'static str {
        match self {
            Counter::TaskClock { .. } => {
                "kernel.perf_event_paranoid 2 or less, or CAP_PERFMON, for a user-only task clock of a thread of its own"
            }
        }
    }

    fn attr(self, max_stack: u16) -> Attr {
        let Counter::TaskClock { hz, switches } = self;
        // `sample_id_all`: switch and loss records carry the thread and the time too.
        let switch_flags = if switches { CONTEXT_SWITCH | SAMPLE_ID_ALL } else { 0 };
        Attr {
            kind: PERF_TYPE_SOFTWARE,
            size: size_of::<Attr>() as u32,
            config: PERF_COUNT_SW_TASK_CLOCK,
            sample_period_or_freq: u64::from(hz.max(1)),
            sample_type: PERF_SAMPLE_TID | PERF_SAMPLE_TIME | PERF_SAMPLE_CALLCHAIN,
            flags: EXCLUDE_KERNEL
                | EXCLUDE_HV
                | FREQ
                | EXCLUDE_CALLCHAIN_KERNEL
                | USE_CLOCKID
                | switch_flags,
            clockid: libc::CLOCK_MONOTONIC,
            sample_max_stack: max_stack,
            ..Attr::default()
        }
    }
}

/// A record read from an event's ring.
#[derive(Clone, Copy, Debug)]
pub enum Record<'a> {
    /// A sample: the thread, its time on `CLOCK_MONOTONIC` in nanoseconds, its call chain.
    Sample { tid: u32, time: u64, chain: Chain<'a> },
    /// The thread was switched in (`out` false) or out at `time`; an out switch says whether
    /// the thread was preempted (still runnable) or blocked.
    Switch { tid: u32, time: u64, out: bool, preempt: bool },
    /// Records the kernel could not write, the ring being full.
    Lost { lost: u64 },
}

/// A sample's call chain as the kernel wrote it, context marks and all.
#[derive(Clone, Copy, Debug)]
pub struct Chain<'a>(&'a [u8]);

impl Chain<'_> {
    /// Its addresses, innermost first, the context marks left out, into `into`: how many.
    pub fn pcs(&self, into: &mut [u64]) -> usize {
        let mut depth = 0;
        for pc in self.0.chunks_exact(8).filter_map(|it| it.try_into().ok()).map(u64::from_ne_bytes)
        {
            if pc >= PERF_CONTEXT_MAX {
                continue;
            }
            let Some(slot) = into.get_mut(depth) else { break };
            *slot = pc;
            depth += 1;
        }
        depth
    }
}

/// One event on one thread: its file and its mapped ring. Dropping it closes both.
pub struct Event {
    fd: libc::c_int,
    map: *mut c_void,
    len: usize,
    page: usize,
    data_pages: usize,
}

// SAFETY: the event owns its file and its mapping alone; the mapping is read through atomics
// (its head and tail) and copies, so the thread that holds the event may be any.
unsafe impl Send for Event {}

impl Event {
    /// Opens `counter` on the thread `tid` of this process (any CPU), its call chains at most
    /// `max_stack` deep, with a ring of `data_pages` pages (a power of two) after its header.
    pub fn open(
        tid: libc::pid_t,
        counter: Counter,
        max_stack: u16,
        data_pages: usize,
    ) -> Result<Event, std::io::Error> {
        let attr = counter.attr(max_stack);
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
        let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap_or(4096);
        let data_pages = data_pages.max(1).next_power_of_two();
        let len = page * (1 + data_pages);
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
        Ok(Event { fd, map, len, page, data_pages })
    }

    /// `perf_event_mmap_page`'s `data_head` and `data_tail`, at bytes 1024 and 1032.
    fn head_tail(&self) -> (&AtomicU64, &AtomicU64) {
        let (head, tail) = (
            self.map.wrapping_byte_add(1024).cast::<u64>(),
            self.map.wrapping_byte_add(1032).cast::<u64>(),
        );
        // SAFETY: the header page is mapped for the event's life and the kernel keeps these two
        // `u64`s, 8-aligned, at these offsets in it; the kernel writes `data_head` and the
        // reader `data_tail`, each atomically.
        let head = unsafe { AtomicU64::from_ptr(head) };
        // SAFETY: as above.
        let tail = unsafe { AtomicU64::from_ptr(tail) };
        (head, tail)
    }

    /// Copies `out.len()` bytes from offset `at` of the data area, round its end.
    fn copy(&self, at: u64, out: &mut [u8]) {
        let size = self.page * self.data_pages;
        let start = at as usize % size;
        let first = out.len().min(size - start);
        let data = self.map.wrapping_byte_add(self.page).cast::<u8>().cast_const();
        // SAFETY: bytes `[start, start + first)` of the data area, which follows the header
        // page inside the mapping; the kernel does not write them until `data_tail` passes
        // them, and `out` is a separate buffer of at least `first` bytes.
        unsafe { std::ptr::copy_nonoverlapping(data.wrapping_add(start), out.as_mut_ptr(), first) };
        if first < out.len() {
            let rest = &mut out[first..];
            // SAFETY: as above, the rest from the area's start, `rest.len() <= size` bytes.
            unsafe { std::ptr::copy_nonoverlapping(data, rest.as_mut_ptr(), rest.len()) };
        }
    }

    /// Gives each record written since the last read to `each`, oldest first, and frees their
    /// room in the ring. Never blocks.
    pub fn drain(&self, each: &mut dyn FnMut(Record<'_>)) {
        let (head, tail) = self.head_tail();
        let end = head.load(Ordering::Acquire);
        let mut at = tail.load(Ordering::Relaxed);
        let mut record = [0u8; RECORD_MAX];
        while at < end {
            let mut header = [0u8; 8];
            self.copy(at, &mut header);
            let kind = u32::from_ne_bytes([header[0], header[1], header[2], header[3]]);
            let misc = u16::from_ne_bytes([header[4], header[5]]);
            let size = usize::from(u16::from_ne_bytes([header[6], header[7]]));
            if size < 8 {
                // A header the kernel never writes: the rest of the ring cannot be framed.
                break;
            }
            if size - 8 <= record.len() {
                let body = &mut record[..size - 8];
                self.copy(at + 8, body);
                if let Some(it) = parse(kind, misc, body) {
                    each(it);
                }
            } else if kind == PERF_RECORD_SAMPLE {
                each(Record::Lost { lost: 1 });
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

fn half(body: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(body.get(at..at + 4)?.try_into().ok()?))
}

/// A record's body, of the sample type both counters use (`TID | TIME | CALLCHAIN`):
///
/// - `PERF_RECORD_SAMPLE`: pid, tid, time, the chain's length and its entries.
/// - `PERF_RECORD_SWITCH`: only its `sample_id` (`sample_id_all`): pid, tid, time.
/// - `PERF_RECORD_LOST`: id, lost, then its `sample_id`; `PERF_RECORD_LOST_SAMPLES`: lost.
///
/// Any other kind is `None`.
pub fn parse(kind: u32, misc: u16, body: &[u8]) -> Option<Record<'_>> {
    match kind {
        PERF_RECORD_SAMPLE => {
            let tid = half(body, 4)?;
            let time = word(body, 8)?;
            let nr = usize::try_from(word(body, 16)?).ok()?;
            let chain = body.get(24..24 + 8 * nr.min((body.len().saturating_sub(24)) / 8))?;
            Some(Record::Sample { tid, time, chain: Chain(chain) })
        }
        PERF_RECORD_SWITCH => Some(Record::Switch {
            tid: half(body, 4)?,
            time: word(body, 8)?,
            out: misc & PERF_RECORD_MISC_SWITCH_OUT != 0,
            preempt: misc & PERF_RECORD_MISC_SWITCH_OUT_PREEMPT != 0,
        }),
        PERF_RECORD_LOST => Some(Record::Lost { lost: word(body, 8)? }),
        PERF_RECORD_LOST_SAMPLES => Some(Record::Lost { lost: word(body, 0)? }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sample_record_is_read_with_its_context_marks_left_out() {
        let mut body = Vec::new();
        body.extend_from_slice(&7u32.to_ne_bytes());
        body.extend_from_slice(&9u32.to_ne_bytes());
        body.extend_from_slice(&4_000u64.to_ne_bytes());
        body.extend_from_slice(&3u64.to_ne_bytes());
        body.extend_from_slice(&(-512i64 as u64).to_ne_bytes());
        body.extend_from_slice(&0x1000u64.to_ne_bytes());
        body.extend_from_slice(&0x2000u64.to_ne_bytes());
        let Some(Record::Sample { tid, time, chain }) = parse(PERF_RECORD_SAMPLE, 0, &body) else {
            panic!("a sample record reads as a sample");
        };
        let mut pcs = [0u64; 4];
        let depth = chain.pcs(&mut pcs);
        assert_eq!((tid, time), (9, 4_000));
        assert_eq!(&pcs[..depth], &[0x1000, 0x2000]);
    }

    #[test]
    fn a_switch_record_says_in_or_out_and_preempted() {
        let mut body = Vec::new();
        body.extend_from_slice(&7u32.to_ne_bytes());
        body.extend_from_slice(&9u32.to_ne_bytes());
        body.extend_from_slice(&5_000u64.to_ne_bytes());
        let read = |misc| match parse(PERF_RECORD_SWITCH, misc, &body) {
            Some(Record::Switch { tid: 9, time: 5_000, out, preempt }) => (out, preempt),
            other => panic!("{other:?}"),
        };
        assert_eq!(read(0), (false, false));
        assert_eq!(read(PERF_RECORD_MISC_SWITCH_OUT), (true, false));
        assert_eq!(
            read(PERF_RECORD_MISC_SWITCH_OUT | PERF_RECORD_MISC_SWITCH_OUT_PREEMPT),
            (true, true)
        );
    }
}
