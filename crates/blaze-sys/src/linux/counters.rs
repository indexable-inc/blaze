//! A thread's and the process's own counters: what the meter reads (blaze-meter, D169).
//!
//! - [`getrusage`] with [`Who::Thread`] (`RUSAGE_THREAD`): this thread's user and system time
//!   and its page faults, the minor (`ru_minflt`) and the major (`ru_majflt`); with
//!   [`Who::Process`] every thread's, the totals the meter reconciles against.
//! - [`perf_counters_open`] (`perf_event_open`): this thread's retired instructions and cycles
//!   in user mode, two hardware counters in one group, read together by one `read`
//!   ([`perf_counters_read`]). A kernel or a container that refuses them
//!   (`perf_event_paranoid` above 2, a seccomp filter, no PMU in the VM) answers with an
//!   [`Errno`], which the meter keeps as the counter's typed unavailability.

use super::{
    fs::{OwnedFd, arg, owned},
    nr,
    process::Rusage,
    raw,
};
use crate::{Errno, Fd};

/// Whose usage [`getrusage`] reads.
/// replaces libc::RUSAGE_THREAD (blaze_sys::linux::Who::Thread, with blaze_sys::linux::getrusage; Linux only)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Who {
    /// `RUSAGE_SELF`: every thread of the process, the ones that exited too.
    Process,
    /// `RUSAGE_THREAD`: the calling thread alone.
    Thread,
}

/// `getrusage`: the times and faults of `who`. One system call, no lock; used by blaze-meter's
/// `Meter::now` (a thread) and `Process::now` (the process).
#[blaze::noalloc]
#[blaze::nopanic]
pub fn getrusage(who: Who) -> Result<Rusage, Errno> {
    let who = match who {
        Who::Process => 0,
        Who::Thread => 1,
    };
    let mut usage = Rusage { user: [0; 2], system: [0; 2], max_rss_kb: 0, rest: [0; 13] };
    // SAFETY: the kernel writes one `struct rusage` (144 bytes of integers, any bytes valid)
    // into the live local given and keeps nothing.
    raw::check(unsafe { raw::syscall2(nr::GETRUSAGE, who, core::ptr::from_mut(&mut usage) as usize) })?;
    Ok(usage)
}

/// `struct perf_event_attr` in its first published size (`PERF_ATTR_SIZE_VER0`, 64 bytes):
/// the kernel reads `size` bytes and zero-extends the rest.
#[repr(C)]
struct PerfAttr {
    kind: u32,
    size: u32,
    config: u64,
    sample_period: u64,
    sample_type: u64,
    read_format: u64,
    /// The bit fields: `disabled` (bit 0), `exclude_kernel` (5), `exclude_hv` (6), ...
    flags: u64,
    wakeup_events: u32,
    bp_type: u32,
    bp_addr: u64,
}

/// `PERF_TYPE_HARDWARE`, and its `PERF_COUNT_HW_CPU_CYCLES` and `PERF_COUNT_HW_INSTRUCTIONS`.
const HARDWARE: u32 = 0;
const CYCLES: u64 = 0;
const INSTRUCTIONS: u64 = 1;
/// `PERF_FORMAT_GROUP`: one read gives every counter of the group.
const FORMAT_GROUP: u64 = 1 << 3;
/// `exclude_kernel` and `exclude_hv`: the thread's own code, which an unprivileged process may
/// count at `perf_event_paranoid` 2 (the default).
const USER_ONLY: u64 = (1 << 5) | (1 << 6);
/// `PERF_FLAG_FD_CLOEXEC`.
const FD_CLOEXEC: usize = 1 << 3;

/// This thread's instructions and cycles counters: the group's leader and its member.
#[derive(Debug)]
pub struct PerfCounters {
    pub instructions: OwnedFd,
    pub cycles: OwnedFd,
}

fn open_one(config: u64, group: Option<Fd>) -> Result<OwnedFd, Errno> {
    let attr = PerfAttr {
        kind: HARDWARE,
        size: size_of::<PerfAttr>() as u32,
        config,
        sample_period: 0,
        sample_type: 0,
        read_format: FORMAT_GROUP,
        flags: USER_ONLY,
        wakeup_events: 0,
        bp_type: 0,
        bp_addr: 0,
    };
    let group = group.map_or(usize::MAX, arg);
    // SAFETY: the kernel reads `attr.size` bytes of the live local given and keeps nothing of
    // it; pid 0 and cpu -1 are this thread on any CPU; the result is a new descriptor.
    owned(unsafe {
        raw::syscall5(nr::PERF_EVENT_OPEN, core::ptr::from_ref(&attr) as usize, 0, usize::MAX, group, FD_CLOEXEC)
    })
}

/// `perf_event_open` twice: this thread's user-mode instructions (the group's leader) and
/// cycles, counting from now. Opened once a thread, by its first meter read.
/// replaces libc::syscall(libc::SYS_perf_event_open) (only for this thread's user-mode instructions+cycles group; as blaze_sys::linux::perf_counters_open(); returns Result<PerfCounters, Errno>; a caller's own perf_event_attr, pid or sampling ring is not this call)
/// replaces libc::SYS_perf_event_open
#[blaze::noalloc]
#[blaze::nopanic]
pub fn perf_counters_open() -> Result<PerfCounters, Errno> {
    let instructions = open_one(INSTRUCTIONS, None)?;
    let cycles = open_one(CYCLES, Some(instructions.fd()))?;
    Ok(PerfCounters { instructions, cycles })
}

/// `struct perf_event_attr` to `sample_max_stack` (linux/perf_event.h, `PERF_ATTR_SIZE_VER5`,
/// 112 bytes, Linux 4.8 on): what a caller's own event is, for [`perf_event_open`]. The kernel
/// reads `size` bytes and zero-extends the rest; [`perf_event_open`] writes `size` itself
/// (this struct's). Unions and bit fields are kept as the kernel's words: the
/// bit fields are [`PerfEventAttr::flags`] ([`PerfAttrFlags::bits`]).
/// replaces ix_prof::perf::Attr (a private copy; the libc crate has no perf_event_attr)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct PerfEventAttr {
    /// `type`: `PERF_TYPE_*` (0 hardware, 1 software, ...).
    pub kind: u32,
    /// `size`: how many bytes of this struct the kernel reads.
    pub size: u32,
    /// `config`: the event within its type (`PERF_COUNT_SW_TASK_CLOCK` is 1, ...).
    pub config: u64,
    /// `sample_period`, or `sample_freq` with [`PerfAttrFlags::FREQ`].
    pub sample_period_or_freq: u64,
    /// `sample_type`: `PERF_SAMPLE_*`, what each sample record holds.
    pub sample_type: u64,
    /// `read_format`: `PERF_FORMAT_*`.
    pub read_format: u64,
    /// The bit fields from `disabled` (bit 0) on: [`PerfAttrFlags::bits`].
    pub flags: u64,
    /// `wakeup_events`, or `wakeup_watermark` with `watermark`.
    pub wakeup_events_or_watermark: u32,
    /// `bp_type`.
    pub bp_type: u32,
    /// `bp_addr` / `kprobe_func` / `uprobe_path` / `config1`.
    pub config1: u64,
    /// `bp_len` / `kprobe_addr` / `probe_offset` / `config2`.
    pub config2: u64,
    /// `branch_sample_type`.
    pub branch_sample_type: u64,
    /// `sample_regs_user`.
    pub sample_regs_user: u64,
    /// `sample_stack_user`.
    pub sample_stack_user: u32,
    /// `clockid`, read with [`PerfAttrFlags::USE_CLOCKID`]: a `ClockId::id()`.
    pub clockid: i32,
    /// `sample_regs_intr`.
    pub sample_regs_intr: u64,
    /// `aux_watermark`.
    pub aux_watermark: u32,
    /// `sample_max_stack`: the deepest call chain a sample takes.
    pub sample_max_stack: u16,
    /// `__reserved_2`: 0.
    pub reserved_2: u16,
}

impl PerfEventAttr {
    /// Every field 0 and `size` this struct's: the start of an attribute.
    pub const ZERO: PerfEventAttr = PerfEventAttr {
        kind: 0,
        size: size_of::<PerfEventAttr>() as u32,
        config: 0,
        sample_period_or_freq: 0,
        sample_type: 0,
        read_format: 0,
        flags: 0,
        wakeup_events_or_watermark: 0,
        bp_type: 0,
        config1: 0,
        config2: 0,
        branch_sample_type: 0,
        sample_regs_user: 0,
        sample_stack_user: 0,
        clockid: 0,
        sample_regs_intr: 0,
        aux_watermark: 0,
        sample_max_stack: 0,
        reserved_2: 0,
    };
}

// linux/perf_event.h, PERF_ATTR_SIZE_VER5; the same on x86-64 and aarch64.
const _: () = {
    assert!(size_of::<PerfEventAttr>() == 112);
    assert!(align_of::<PerfEventAttr>() == 8);
    assert!(core::mem::offset_of!(PerfEventAttr, kind) == 0);
    assert!(core::mem::offset_of!(PerfEventAttr, size) == 4);
    assert!(core::mem::offset_of!(PerfEventAttr, config) == 8);
    assert!(core::mem::offset_of!(PerfEventAttr, sample_period_or_freq) == 16);
    assert!(core::mem::offset_of!(PerfEventAttr, sample_type) == 24);
    assert!(core::mem::offset_of!(PerfEventAttr, read_format) == 32);
    assert!(core::mem::offset_of!(PerfEventAttr, flags) == 40);
    assert!(core::mem::offset_of!(PerfEventAttr, wakeup_events_or_watermark) == 48);
    assert!(core::mem::offset_of!(PerfEventAttr, bp_type) == 52);
    assert!(core::mem::offset_of!(PerfEventAttr, config1) == 56);
    assert!(core::mem::offset_of!(PerfEventAttr, config2) == 64);
    assert!(core::mem::offset_of!(PerfEventAttr, branch_sample_type) == 72);
    assert!(core::mem::offset_of!(PerfEventAttr, sample_regs_user) == 80);
    assert!(core::mem::offset_of!(PerfEventAttr, sample_stack_user) == 88);
    assert!(core::mem::offset_of!(PerfEventAttr, clockid) == 92);
    assert!(core::mem::offset_of!(PerfEventAttr, sample_regs_intr) == 96);
    assert!(core::mem::offset_of!(PerfEventAttr, aux_watermark) == 104);
    assert!(core::mem::offset_of!(PerfEventAttr, sample_max_stack) == 108);
    assert!(core::mem::offset_of!(PerfEventAttr, reserved_2) == 110);
};

flag_set! {
    /// `perf_event_attr`'s bit fields (linux/perf_event.h), as [`PerfEventAttr::flags`] holds
    /// them.
    /// replaces ix_prof::perf's private EXCLUDE_KERNEL, EXCLUDE_HV, FREQ, SAMPLE_ID_ALL, EXCLUDE_CALLCHAIN_KERNEL, USE_CLOCKID, CONTEXT_SWITCH (the libc crate has none)
    PerfAttrFlags: u64 {
        /// `disabled`: off until enabled.
        DISABLED = 1 << 0;
        /// `inherit`: children count too.
        INHERIT = 1 << 1;
        /// `exclude_user`.
        EXCLUDE_USER = 1 << 4;
        /// `exclude_kernel`: user mode only (allowed at `perf_event_paranoid` 2).
        EXCLUDE_KERNEL = 1 << 5;
        /// `exclude_hv`.
        EXCLUDE_HV = 1 << 6;
        /// `exclude_idle`.
        EXCLUDE_IDLE = 1 << 7;
        /// `mmap`: `PERF_RECORD_MMAP` records.
        MMAP = 1 << 8;
        /// `comm`: `PERF_RECORD_COMM` records.
        COMM = 1 << 9;
        /// `freq`: `sample_freq`, not `sample_period`.
        FREQ = 1 << 10;
        /// `enable_on_exec`.
        ENABLE_ON_EXEC = 1 << 12;
        /// `task`: fork and exit records.
        TASK = 1 << 13;
        /// `watermark`: `wakeup_watermark`, not `wakeup_events`.
        WATERMARK = 1 << 14;
        /// `sample_id_all`: every record carries the sample id fields.
        SAMPLE_ID_ALL = 1 << 18;
        /// `exclude_callchain_kernel`.
        EXCLUDE_CALLCHAIN_KERNEL = 1 << 21;
        /// `exclude_callchain_user`.
        EXCLUDE_CALLCHAIN_USER = 1 << 22;
        /// `use_clockid`: times on [`PerfEventAttr::clockid`].
        USE_CLOCKID = 1 << 25;
        /// `context_switch`: `PERF_RECORD_SWITCH` records.
        CONTEXT_SWITCH = 1 << 26;
    }
}

flag_set! {
    /// `perf_event_open`'s flags (`PERF_FLAG_*`).
    /// replaces ix_prof::perf's private PERF_FLAG_FD_CLOEXEC (the libc crate has none)
    PerfOpenFlags: usize {
        /// `PERF_FLAG_FD_NO_GROUP`.
        FD_NO_GROUP = 1 << 0;
        /// `PERF_FLAG_FD_OUTPUT`.
        FD_OUTPUT = 1 << 1;
        /// `PERF_FLAG_PID_CGROUP`: `pid` is a cgroup's directory descriptor.
        PID_CGROUP = 1 << 2;
        /// `PERF_FLAG_FD_CLOEXEC`.
        FD_CLOEXEC = 1 << 3;
    }
}

/// `perf_event_open(attr, pid, cpu, group, flags)`, the general form: an event as `attr`
/// says on thread `pid` (0: the calling thread; a tid of this process) and `cpu` (-1: any),
/// in the group `group` leads (`None`: its own leader). A sampling event's ring is then
/// [`super::mmap`]ped by the caller (`MapFlags::SHARED`, one header page and a power of two
/// of data pages, the page size from `crate::facts::Aux`). A kernel or a container that
/// refuses it answers with an [`Errno`]: [`Errno::Acces`] or [`Errno::Perm`] above its
/// `perf_event_paranoid`, [`Errno::NoEnt`] or [`Errno::OpNotSupp`] for an event it lacks,
/// [`Errno::NoSys`] under a filter. The meter's own counters are [`perf_counters_open`].
/// replaces libc::syscall(libc::SYS_perf_event_open) (the general form: as blaze_sys::linux::perf_event_open(&PerfEventAttr, pid, cpu, group: Option<Fd>, PerfOpenFlags); returns Result<OwnedFd, Errno>, the site's negative check becomes the Err)
/// replaces libc::SYS_perf_event_open (the call; the number as data stays nr::PERF_EVENT_OPEN)
#[blaze::noalloc]
#[blaze::nopanic]
pub fn perf_event_open(
    attr: &PerfEventAttr,
    pid: i32,
    cpu: i32,
    group: Option<Fd>,
    flags: PerfOpenFlags,
) -> Result<OwnedFd, Errno> {
    let group = group.map_or(usize::MAX, arg);
    // The size is this struct's, whatever the caller wrote: the kernel reads no byte past it.
    let attr = PerfEventAttr { size: size_of::<PerfEventAttr>() as u32, ..*attr };
    // SAFETY: the kernel reads `attr.size` bytes of the live local (all of it) and keeps
    // nothing of it; an address it finds in `config1` (a probe's name) it reads with its own
    // checks (`copy_from_user`), never writing this process's memory; the result is a new
    // descriptor.
    owned(unsafe {
        raw::syscall5(
            nr::PERF_EVENT_OPEN,
            core::ptr::from_ref(&attr) as usize,
            pid as isize as usize,
            cpu as isize as usize,
            group,
            flags.bits(),
        )
    })
}

/// One `read` of the group: (instructions, cycles) since [`perf_counters_open`].
#[blaze::noalloc]
#[blaze::nopanic]
pub fn perf_counters_read(leader: Fd) -> Result<(u64, u64), Errno> {
    // `PERF_FORMAT_GROUP` without other bits: `nr`, then each counter's value.
    let mut read = [0u64; 3];
    // SAFETY: the kernel writes at most 24 bytes into the live local of 24 bytes.
    let got = raw::check(unsafe { raw::syscall3(nr::READ, arg(leader), read.as_mut_ptr() as usize, size_of_val(&read)) })?;
    if got != size_of_val(&read) || read[0] != 2 {
        return Err(Errno::Inval);
    }
    Ok((read[1], read[2]))
}

#[cfg(test)]
mod perf_event_open_tests {
    use super::{PerfAttrFlags, PerfEventAttr, PerfOpenFlags, perf_event_open};
    use crate::Errno;

    /// A user-only task clock of this thread, the event ix-prof's sampler opens: a descriptor,
    /// or one of the refusals a kernel or container gives, never another error.
    #[test]
    fn a_task_clock_opens_or_is_refused() {
        let flags = PerfAttrFlags::EXCLUDE_KERNEL | PerfAttrFlags::EXCLUDE_HV | PerfAttrFlags::USE_CLOCKID;
        let attr = PerfEventAttr {
            kind: 1,
            config: 1,
            flags: flags.bits(),
            clockid: crate::linux::ClockId::Monotonic.id(),
            ..PerfEventAttr::ZERO
        };
        match perf_event_open(&attr, 0, -1, None, PerfOpenFlags::FD_CLOEXEC) {
            Ok(event) => assert!(event.fd().raw() >= 0),
            Err(Errno::Acces | Errno::Perm | Errno::NoSys | Errno::NoEnt | Errno::OpNotSupp) => {
                eprintln!("skipped: this host refuses perf_event_open");
            }
            Err(other) => panic!("perf_event_open: {other}"),
        }
    }
}
