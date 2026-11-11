//! The ring a thread waits on (Linux), one per thread that waits, made at its first wait: a
//! small io_uring of its own (`IORING_SETUP_SINGLE_ISSUER | DEFER_TASKRUN`, no kernel thread)
//! on which each blocking wait is one request, a futex wait (`IORING_OP_FUTEX_WAIT`, 6.7), a
//! child's exit (`IORING_OP_WAITID`, 6.7), a poll of one descriptor (`IORING_OP_POLL_ADD`) or
//! a sleep (`IORING_OP_TIMEOUT` to an absolute time). Each request's deadline is in the ring's
//! registered wait region (`IORING_REGISTER_MEM_REGION`, 6.13): the region holds the absolute
//! `CLOCK_MONOTONIC` time and the signal mask, `io_uring_enter` is told only its offset
//! (`IORING_ENTER_EXT_ARG_REG | ABS_TIMER`), and a retry after an interruption rewrites only
//! the region's time, recomputed from the wait's clock by [`super::until`]. A request whose
//! deadline passed is cancelled and its end taken before the wait returns, so the kernel
//! never writes memory the wait no longer holds.
//!
//! Linux 7.3 is blaze's floor (docs/v2/plan/design/deadline-waits.md step 5): its fix of
//! io_uring futex in-flight accounting is what lets an idle pool's threads park here. Nothing
//! is probed: a kernel without one of these fails the wait with an error naming it.
//!
//! The sandbox launcher stays off this ring: its seccomp filter refuses io_uring to what it
//! launches, and it waits between its fork and its exec, where nothing may be made. What it
//! launches is told so (`wait/choice.rs`), and a blaze there waits with direct system calls
//! (`wait/direct.rs`); a process not told so that meets the filter fails here, naming it.

use std::{
    cell::RefCell,
    io,
    os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd},
    ptr,
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

use super::{
    Clock, Deadline, Monotonic, Once, WaitError, Waited,
    linux::{KERNEL_SIGSET, WAITING_MASK},
    parking::Futex,
    reaping::Children,
    until,
};

const OP_POLL_ADD: u8 = 6;
const OP_TIMEOUT: u8 = 11;
const OP_ASYNC_CANCEL: u8 = 14;
const OP_WAITID: u8 = 50;
const OP_FUTEX_WAIT: u8 = 51;
const OP_FUTEX_WAKE: u8 = 52;

const SETUP_R_DISABLED: u32 = 1 << 6;
const SETUP_SUBMIT_ALL: u32 = 1 << 7;
const SETUP_SINGLE_ISSUER: u32 = 1 << 12;
const SETUP_DEFER_TASKRUN: u32 = 1 << 13;

const ENTER_GETEVENTS: u32 = 1 << 0;
const ENTER_EXT_ARG: u32 = 1 << 3;
const ENTER_ABS_TIMER: u32 = 1 << 5;
const ENTER_EXT_ARG_REG: u32 = 1 << 6;

const REGISTER_ENABLE_RINGS: libc::c_uint = 12;
const REGISTER_MEM_REGION: libc::c_uint = 34;
const MEM_REGION_TYPE_USER: u32 = 1;
const MEM_REGION_REG_WAIT_ARG: u64 = 1;
const REG_WAIT_TS: u32 = 1 << 0;

const TIMEOUT_ABS: u32 = 1 << 0;
/// `FUTEX2_SIZE_U32 | FUTEX2_PRIVATE`: a 32-bit word of this process.
const FUTEX2_U32_PRIVATE: i32 = 0x02 | 0x80;
/// `FUTEX_BITSET_MATCH_ANY`.
const FUTEX_ANY: u64 = 0xffff_ffff;

const OFF_SQ_RING: libc::off_t = 0;
const OFF_SQES: libc::off_t = 0x1000_0000;

/// Requests in flight at once: the wait's, its cancel and a wake.
const ENTRIES: u32 = 8;
/// The `user_data` of the wait's own request; every other completion is taken and dropped.
const WAITED: u64 = 1;
const CANCEL: u64 = 2;
const WAKE: u64 = 3;
/// How long a cancelled request may take to end: past it the ring is let go (its requests
/// end with it) and the wait fails naming it.
const LETTING_GO: Duration = Duration::from_secs(5);
/// The size of the registered region: one page, the wait argument at 0 and its mask after.
const REGION: usize = 4096;
const MASK_AT: usize = 64;

#[allow(dead_code, reason = "a layout of linux/io_uring.h, read by the kernel")]
#[repr(C)]
#[derive(Default)]
struct SqOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    flags: u32,
    dropped: u32,
    array: u32,
    resv1: u32,
    user_addr: u64,
}

#[allow(dead_code, reason = "a layout of linux/io_uring.h, read by the kernel")]
#[repr(C)]
#[derive(Default)]
struct CqOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    overflow: u32,
    cqes: u32,
    flags: u32,
    resv1: u32,
    user_addr: u64,
}

#[allow(dead_code, reason = "a layout of linux/io_uring.h, read by the kernel")]
#[repr(C)]
#[derive(Default)]
struct Params {
    sq_entries: u32,
    cq_entries: u32,
    flags: u32,
    sq_thread_cpu: u32,
    sq_thread_idle: u32,
    features: u32,
    wq_fd: u32,
    resv: [u32; 3],
    sq_off: SqOffsets,
    cq_off: CqOffsets,
}

#[allow(dead_code, reason = "a layout of linux/io_uring.h, read by the kernel")]
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Sqe {
    opcode: u8,
    flags: u8,
    ioprio: u16,
    fd: i32,
    off: u64,
    addr: u64,
    len: u32,
    op_flags: u32,
    user_data: u64,
    buf_index: u16,
    personality: u16,
    file_index: u32,
    addr3: u64,
    pad: u64,
}

#[allow(dead_code, reason = "a layout of linux/io_uring.h, read by the kernel")]
#[repr(C)]
struct Cqe {
    user_data: u64,
    res: i32,
    flags: u32,
}

/// `struct __kernel_timespec`.
#[allow(dead_code, reason = "a layout of linux/io_uring.h, read by the kernel")]
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Timespec {
    sec: i64,
    nsec: i64,
}

/// `struct io_uring_reg_wait`: one wait's argument in the registered region.
#[allow(dead_code, reason = "a layout of linux/io_uring.h, read by the kernel")]
#[repr(C)]
struct RegWait {
    ts: Timespec,
    min_wait_usec: u32,
    flags: u32,
    sigmask: u64,
    sigmask_sz: u32,
    pad: [u32; 3],
    pad2: [u64; 2],
}

/// `struct io_uring_region_desc`.
#[allow(dead_code, reason = "a layout of linux/io_uring.h, read by the kernel")]
#[repr(C)]
#[derive(Default)]
struct RegionDesc {
    user_addr: u64,
    size: u64,
    flags: u32,
    id: u32,
    mmap_offset: u64,
    resv: [u64; 4],
}

/// `struct io_uring_mem_region_reg`.
#[allow(dead_code, reason = "a layout of linux/io_uring.h, read by the kernel")]
#[repr(C)]
struct MemRegionReg {
    region_uptr: u64,
    flags: u64,
    resv: [u64; 2],
}

/// A mapping, unmapped when dropped.
struct Map {
    at: *mut u8,
    len: usize,
}

impl Map {
    /// `len` bytes of `fd` at `offset`, or anonymous memory for `fd` -1.
    fn new(fd: RawFd, len: usize, offset: libc::off_t) -> io::Result<Map> {
        let flags = match fd {
            -1 => libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_POPULATE,
            _ => libc::MAP_SHARED | libc::MAP_POPULATE,
        };
        // SAFETY: a new mapping the kernel places; nothing of this process is overwritten.
        let at = unsafe {
            libc::mmap(ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, flags, fd, offset)
        };
        if at == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Map { at: at.cast(), len })
    }

    /// The `T` at `offset`, checked against the mapping.
    fn at<T>(&self, offset: usize) -> *mut T {
        assert!(
            offset + size_of::<T>() <= self.len,
            "offset {offset} of a {}-byte value is past the {}-byte wait ring mapping",
            size_of::<T>(),
            self.len,
        );
        self.at.wrapping_add(offset).cast()
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        // SAFETY: the mapping `new` made, unmapped once.
        unsafe { libc::munmap(self.at.cast(), self.len) };
    }
}

/// A thread's wait ring.
struct WaitRing {
    // Closed first: the kernel ends what is in flight, then the memory it names goes.
    fd: OwnedFd,
    sq_head: *const AtomicU32,
    sq_tail: *const AtomicU32,
    sq_mask: u32,
    sq_entries: u32,
    sqes: *mut Sqe,
    cq_head: *const AtomicU32,
    cq_tail: *const AtomicU32,
    cq_mask: u32,
    cqes: *const Cqe,
    tail: u32,
    /// The region's wait argument, which every waiting enter names.
    wait: *mut RegWait,
    /// What a `waitid` writes of its child, and a sleep's absolute time: the kernel's until
    /// the request ends.
    info: Box<libc::siginfo_t>,
    sleep: Box<Timespec>,
    /// A cancel that did not end its request in time: the ring is let go after this wait.
    lost: bool,
    _rings: Map,
    _sqe_map: Map,
    _region: Map,
}

fn errno() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// `CLOCK_MONOTONIC` now.
fn monotonic() -> io::Result<Duration> {
    let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: a timespec for the kernel to write.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Duration::new(u64::try_from(now.tv_sec).unwrap_or(0), now.tv_nsec as u32))
}

fn timespec(at: Duration) -> Timespec {
    Timespec {
        sec: i64::try_from(at.as_secs()).unwrap_or(i64::MAX),
        nsec: at.subsec_nanos().into(),
    }
}

fn register(fd: RawFd, opcode: libc::c_uint, arg: *const libc::c_void, n: libc::c_uint) -> i64 {
    // SAFETY: `arg` points at the structure `opcode` reads (or is null for none), live for the
    // call; the kernel copies what it keeps.
    unsafe { libc::syscall(libc::SYS_io_uring_register, fd, opcode, arg, n) }
}

impl WaitRing {
    fn new() -> io::Result<WaitRing> {
        let refused = |step: &str, error: io::Error| {
            io::Error::new(
                error.kind(),
                // alloc: the error of a setup that failed.
                format!(
                    "a thread's wait ring: {step}: {error} (blaze needs Linux 7.3 or later, and this is Linux {})",
                    crate::cgroup::kernel()
                ),
            )
        };
        let mut params = Params {
            flags: SETUP_SUBMIT_ALL | SETUP_R_DISABLED | SETUP_SINGLE_ISSUER | SETUP_DEFER_TASKRUN,
            ..Params::default()
        };
        // SAFETY: `params` is the `io_uring_params` the call reads and fills.
        let fd = unsafe {
            libc::syscall(libc::SYS_io_uring_setup, ENTRIES, &raw mut params as *mut libc::c_void)
        };
        if fd < 0 {
            let error = io::Error::last_os_error();
            // The sandbox's filter answers so, as a kernel with no io_uring does.
            let step = match error.raw_os_error() {
                Some(libc::ENOSYS) => {
                    "io_uring_setup (a kernel with no io_uring answers so, and so does blaze's sandbox to what it \
                     runs: a blaze the sandbox launched waits with direct system calls when its environment has \
                     the launcher's `__BLAZE_SANDBOXED=1`, which this process's has not)"
                }
                _ => "io_uring_setup",
            };
            return Err(refused(step, error));
        }
        // SAFETY: the call returned a descriptor of its own.
        let fd = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
        let (sq, cq) = (&params.sq_off, &params.cq_off);
        let sq_len = sq.array as usize + params.sq_entries as usize * size_of::<u32>();
        let cq_len = cq.cqes as usize + params.cq_entries as usize * size_of::<Cqe>();
        let rings = Map::new(fd.as_raw_fd(), sq_len.max(cq_len), OFF_SQ_RING)
            .map_err(|error| refused("mapping its rings", error))?;
        let sqe_map =
            Map::new(fd.as_raw_fd(), params.sq_entries as usize * size_of::<Sqe>(), OFF_SQES)
                .map_err(|error| refused("mapping its requests", error))?;
        let array: *mut u32 = rings.at(sq.array as usize);
        for index in 0..params.sq_entries {
            // SAFETY: the kernel's array has `sq_entries` slots in `rings`, not yet read by it.
            unsafe { array.add(index as usize).write(index) };
        }
        let region = Map::new(-1, REGION, 0).map_err(|error| refused("its wait region", error))?;
        let mask: *mut u64 = region.at(MASK_AT);
        // SAFETY: in the region, aligned, and no one else's.
        unsafe { mask.write(WAITING_MASK) };
        let wait: *mut RegWait = region.at(0);
        // SAFETY: as above; the kernel reads it at each waiting enter.
        unsafe {
            wait.write(RegWait {
                ts: Timespec::default(),
                min_wait_usec: 0,
                flags: 0,
                sigmask: mask as u64,
                sigmask_sz: KERNEL_SIGSET,
                pad: [0; 3],
                pad2: [0; 2],
            });
        };
        let desc = RegionDesc {
            user_addr: region.at as u64,
            size: REGION as u64,
            flags: MEM_REGION_TYPE_USER,
            ..RegionDesc::default()
        };
        let reg = MemRegionReg {
            region_uptr: ptr::from_ref(&desc) as u64,
            flags: MEM_REGION_REG_WAIT_ARG,
            resv: [0; 2],
        };
        if register(fd.as_raw_fd(), REGISTER_MEM_REGION, ptr::from_ref(&reg).cast(), 1) < 0 {
            return Err(refused("registering its wait region", io::Error::last_os_error()));
        }
        // Enabled by the thread that waits on it, its one issuer.
        if register(fd.as_raw_fd(), REGISTER_ENABLE_RINGS, ptr::null(), 0) < 0 {
            return Err(refused("enabling it", io::Error::last_os_error()));
        }
        // SAFETY: the kernel put each ring's mask, a `u32` it never changes, at its offset.
        let sq_mask = unsafe { *rings.at::<u32>(sq.ring_mask as usize) };
        // SAFETY: as above.
        let cq_mask = unsafe { *rings.at::<u32>(cq.ring_mask as usize) };
        Ok(WaitRing {
            sq_head: rings.at(sq.head as usize),
            sq_tail: rings.at(sq.tail as usize),
            sq_mask,
            sq_entries: params.sq_entries,
            sqes: sqe_map.at(0),
            cq_head: rings.at(cq.head as usize),
            cq_tail: rings.at(cq.tail as usize),
            cq_mask,
            cqes: rings.at(cq.cqes as usize),
            tail: 0,
            wait,
            // SAFETY: all-zero bytes is a valid `siginfo_t` and `Timespec`.
            info: Box::new(unsafe { std::mem::zeroed() }), // alloc: once per thread's ring.
            sleep: Box::new(Timespec::default()),          // alloc: once per thread's ring.
            lost: false,
            fd,
            _rings: rings,
            _sqe_map: sqe_map,
            _region: region,
        })
    }

    /// Writes `sqe` for the next enter to submit: false where the ring is full.
    fn push(&mut self, sqe: Sqe) -> bool {
        // SAFETY: the kernel's counter in `rings`, mapped while `self` lives.
        let head = unsafe { (*self.sq_head).load(Ordering::Acquire) };
        if self.tail.wrapping_sub(head) >= self.sq_entries {
            return false;
        }
        // SAFETY: masked below `sq_entries`; a slot past the kernel's tail is not the kernel's.
        unsafe { self.sqes.add((self.tail & self.sq_mask) as usize).write(sqe) };
        self.tail = self.tail.wrapping_add(1);
        // SAFETY: only this thread writes the tail; the release publishes the request.
        unsafe { (*self.sq_tail).store(self.tail, Ordering::Release) };
        true
    }

    /// One `io_uring_enter`: submits what was pushed and, with `wait`, waits for a completion
    /// for up to `left` (no limit for `None`), the absolute time written to the region.
    fn enter(&mut self, wait: bool, left: Option<Duration>) -> io::Result<Once<()>> {
        // SAFETY: the kernel's counter, as in `push`.
        let head = unsafe { (*self.sq_head).load(Ordering::Acquire) };
        let submit = self.tail.wrapping_sub(head);
        let (min, mut flags, arg, size): (u32, u32, *const libc::c_void, usize) = match wait {
            true => (
                1,
                ENTER_GETEVENTS | ENTER_EXT_ARG | ENTER_EXT_ARG_REG,
                ptr::null(),
                size_of::<RegWait>(),
            ),
            false => (0, 0, ptr::null(), 0),
        };
        if wait {
            let (ts, ts_flags) = match left {
                Some(left) => (timespec(monotonic()?.saturating_add(left)), REG_WAIT_TS),
                None => (Timespec::default(), 0),
            };
            if ts_flags != 0 {
                flags |= ENTER_ABS_TIMER;
            }
            // SAFETY: the region's wait argument; the kernel reads it only inside an enter,
            // and this thread is its only writer.
            unsafe {
                (&raw mut (*self.wait).ts).write(ts);
                (&raw mut (*self.wait).flags).write(ts_flags);
            }
        }
        // SAFETY: the ring's descriptor; with `EXT_ARG_REG` the argument is an offset into
        // the registered region (0, its one wait argument), not a pointer.
        let entered = unsafe {
            libc::syscall(
                libc::SYS_io_uring_enter,
                self.fd.as_raw_fd(),
                submit,
                min,
                flags,
                arg,
                size,
            )
        };
        if entered >= 0 {
            return Ok(Once::Ready(()));
        }
        match errno() {
            libc::EINTR => Ok(Once::Interrupted),
            libc::ETIME => Ok(Once::TimedOut),
            libc::EBUSY | libc::EAGAIN => Ok(Once::Ready(())),
            error => Err(io::Error::from_raw_os_error(error)),
        }
    }

    /// Takes every completion posted: the result of the wait's own request, if it ended.
    fn take(&mut self) -> Option<i32> {
        // SAFETY: only this thread writes the head.
        let mut head = unsafe { (*self.cq_head).load(Ordering::Relaxed) };
        // SAFETY: the kernel moves the tail; the acquire pairs with its release of entries.
        let tail = unsafe { (*self.cq_tail).load(Ordering::Acquire) };
        let mut waited = None;
        while head != tail {
            // SAFETY: masked below the completion ring's entries; posted (the acquire above)
            // and not reused until the head moves past it.
            let Cqe { user_data, res, .. } =
                unsafe { self.cqes.add((head & self.cq_mask) as usize).read() };
            if user_data == WAITED {
                waited = Some(res);
            }
            head = head.wrapping_add(1);
        }
        // SAFETY: as above; the release orders the reads before the kernel's reuse.
        unsafe { (*self.cq_head).store(head, Ordering::Release) };
        waited
    }

    /// `sqe` as the wait's own request, until it ends or `deadline` on `clock`: its result,
    /// or `Deadline` once it was cancelled unended (or ended `ECANCELED` by the cancel).
    fn wait_for(
        &mut self,
        what: &'static str,
        sqe: Sqe,
        deadline: Option<Deadline>,
        clock: &impl Clock,
    ) -> Result<Waited<i32>, WaitError> {
        _ = self.take();
        if !self.push(Sqe { user_data: WAITED, ..sqe }) {
            return Err(WaitError::io(what, io::Error::other("this thread's wait ring is full")));
        }
        let waited = until(what, clock, deadline, |left| {
            let entered = self.enter(true, left)?;
            if let Some(res) = self.take() {
                return Ok(Once::Ready(res));
            }
            Ok(match entered {
                Once::Ready(()) | Once::Spurious => Once::Spurious,
                Once::Interrupted => Once::Interrupted,
                Once::TimedOut => Once::TimedOut,
            })
        });
        match waited {
            Ok(Waited::Ready(res)) => Ok(Waited::Ready(res)),
            Ok(Waited::Deadline) => match self.cancel(what)? {
                res if res == -libc::ECANCELED => Ok(Waited::Deadline),
                // It ended between the deadline and the cancel: what it ended with.
                res => Ok(Waited::Ready(res)),
            },
            Err(error) => {
                // Ended before the error is returned, whatever it ended with.
                self.cancel(what)?;
                Err(error)
            }
        }
    }

    /// Cancels the wait's own request and takes its end, within [`LETTING_GO`].
    fn cancel(&mut self, what: &'static str) -> Result<i32, WaitError> {
        let sqe =
            Sqe { opcode: OP_ASYNC_CANCEL, addr: WAITED, user_data: CANCEL, ..Sqe::default() };
        if !self.push(sqe) {
            self.lost = true;
            return Err(WaitError::io(
                what,
                io::Error::other("no room in the wait ring to cancel"),
            ));
        }
        let clock = Monotonic::new();
        let deadline = Deadline::after(clock.now(), LETTING_GO);
        let ended = until(what, &clock, Some(deadline), |left| {
            let entered = self.enter(true, left)?;
            if let Some(res) = self.take() {
                return Ok(Once::Ready(res));
            }
            Ok(match entered {
                Once::Interrupted => Once::Interrupted,
                Once::TimedOut => Once::TimedOut,
                Once::Ready(()) | Once::Spurious => Once::Spurious,
            })
        });
        match ended {
            Ok(Waited::Ready(res)) => Ok(res),
            Ok(Waited::Deadline) => {
                self.lost = true;
                Err(WaitError::io(
                    what,
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "its request in the thread's wait ring did not end within 5 s of its cancel",
                    ),
                ))
            }
            Err(error) => {
                self.lost = true;
                Err(error)
            }
        }
    }

    fn futex_wait(
        &mut self,
        what: &'static str,
        word: &AtomicU32,
        expected: u32,
        deadline: Option<Deadline>,
        clock: &impl Clock,
    ) -> Result<Waited<()>, WaitError> {
        let sqe = Sqe {
            opcode: OP_FUTEX_WAIT,
            fd: FUTEX2_U32_PRIVATE,
            addr: word.as_ptr() as u64,
            off: u64::from(expected),
            addr3: FUTEX_ANY,
            ..Sqe::default()
        };
        match self.wait_for(what, sqe, deadline, clock)? {
            Waited::Deadline => Ok(Waited::Deadline),
            // Woken, or the word no longer held `expected`: the parker looks again.
            Waited::Ready(res) if res >= 0 || res == -libc::EAGAIN => Ok(Waited::Ready(())),
            Waited::Ready(res) => Err(WaitError::io(what, io::Error::from_raw_os_error(-res))),
        }
    }

    /// Wakes one waiter on `word` through the ring: false where it could not be submitted.
    fn futex_wake(&mut self, word: &AtomicU32) -> bool {
        _ = self.take();
        let sqe = Sqe {
            opcode: OP_FUTEX_WAKE,
            fd: FUTEX2_U32_PRIVATE,
            addr: word.as_ptr() as u64,
            off: 1,
            addr3: FUTEX_ANY,
            user_data: WAKE,
            ..Sqe::default()
        };
        // Submitted and done inside the enter: the wake is issued inline.
        let woke = self.push(sqe) && matches!(self.enter(false, None), Ok(Once::Ready(())));
        _ = self.take();
        woke
    }

    fn waitid(
        &mut self,
        what: &'static str,
        pid: libc::pid_t,
        keep: bool,
        deadline: Option<Deadline>,
        clock: &impl Clock,
    ) -> Result<Waited<()>, WaitError> {
        let options = libc::WEXITED | if keep { libc::WNOWAIT } else { 0 };
        let sqe = Sqe {
            opcode: OP_WAITID,
            len: libc::P_PID as u32,
            fd: pid,
            file_index: options as u32,
            off: ptr::from_mut(&mut *self.info) as u64,
            ..Sqe::default()
        };
        match self.wait_for(what, sqe, deadline, clock)? {
            Waited::Deadline => Ok(Waited::Deadline),
            Waited::Ready(0) => Ok(Waited::Ready(())),
            Waited::Ready(res) => Err(WaitError::io(what, io::Error::from_raw_os_error(-res))),
        }
    }

    fn poll(
        &mut self,
        what: &'static str,
        fd: RawFd,
        events: libc::c_short,
        deadline: Option<Deadline>,
        clock: &impl Clock,
    ) -> Result<Waited<libc::c_short>, WaitError> {
        let sqe =
            Sqe { opcode: OP_POLL_ADD, fd, op_flags: u32::from(events as u16), ..Sqe::default() };
        match self.wait_for(what, sqe, deadline, clock)? {
            Waited::Deadline => Ok(Waited::Deadline),
            Waited::Ready(res) if res >= 0 => Ok(Waited::Ready(res as libc::c_short)),
            Waited::Ready(res) => Err(WaitError::io(what, io::Error::from_raw_os_error(-res))),
        }
    }

    fn sleep_until(
        &mut self,
        what: &'static str,
        deadline: Deadline,
        clock: &impl Clock,
    ) -> Result<(), WaitError> {
        let left = deadline.left(clock.now());
        let at = monotonic().map_err(|error| WaitError::io(what, error))?.saturating_add(left);
        *self.sleep = timespec(at);
        let sqe = Sqe {
            opcode: OP_TIMEOUT,
            addr: ptr::from_ref(&*self.sleep) as u64,
            len: 1,
            op_flags: TIMEOUT_ABS,
            ..Sqe::default()
        };
        match self.wait_for(what, sqe, Some(deadline), clock)? {
            // The ring's timeout fired (`ETIME`), or the deadline passed on the wait's clock.
            Waited::Deadline => Ok(()),
            Waited::Ready(res) if res == -libc::ETIME || res >= 0 => Ok(()),
            Waited::Ready(res) => Err(WaitError::io(what, io::Error::from_raw_os_error(-res))),
        }
    }
}

thread_local! {
    /// This thread's wait ring, made at its first wait.
    static RING: RefCell<Option<WaitRing>> = const { RefCell::new(None) };
}

/// `f` on this thread's wait ring, made if it has none; let go after a cancel that failed.
#[inline(never)]
fn with_ring<T>(
    what: &'static str,
    f: impl FnOnce(&mut WaitRing) -> Result<T, WaitError>,
) -> Result<T, WaitError> {
    RING.with(|cell| {
        let mut held = cell.try_borrow_mut().map_err(|_| {
            WaitError::io(
                what,
                io::Error::other("this thread's wait ring is held by a wait it is inside"),
            )
        })?;
        if held.is_none() {
            *held = Some(WaitRing::new().map_err(|error| WaitError::io(what, error))?);
        }
        let Some(ring) = held.as_mut() else { unreachable!("made above") };
        let done = f(ring);
        if ring.lost {
            *held = None;
        }
        done
    })
}

/// The futex of a [`super::Parker`] on Linux: a wait through this thread's ring, a wake
/// through the waker's ring where it has one and with a plain `FUTEX_WAKE` where it has none.
pub(crate) struct Futexes;

impl Futex for Futexes {
    fn wait<C: Clock>(
        &self,
        what: &'static str,
        word: &AtomicU32,
        expected: u32,
        deadline: Option<Deadline>,
        clock: &C,
    ) -> Result<Waited<()>, WaitError> {
        with_ring(what, |ring| ring.futex_wait(what, word, expected, deadline, clock))
    }

    #[inline(never)]
    fn wake(&self, word: &AtomicU32) {
        let woke = RING
            .try_with(|cell| match cell.try_borrow_mut() {
                Ok(mut held) => held.as_mut().is_some_and(|ring| ring.futex_wake(word)),
                Err(_) => false,
            })
            .unwrap_or(false);
        if !woke {
            // SAFETY: a wake of a word of this process; the kernel reads no memory of it.
            unsafe {
                libc::syscall(
                    libc::SYS_futex,
                    word.as_ptr(),
                    libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG,
                    1,
                    ptr::null::<libc::timespec>(),
                    ptr::null::<u32>(),
                    0,
                )
            };
        }
    }
}

/// This process's children on Linux: a reap is `wait4(WNOHANG)`, a wait for an exit is
/// `IORING_OP_WAITID` on this thread's ring.
pub(crate) struct Kids;

impl Children for Kids {
    type Reaped = (libc::c_int, libc::rusage);

    fn reap_now(&self, pid: i32) -> io::Result<Option<Self::Reaped>> {
        let mut status = 0;
        // SAFETY: all-zero bytes is a valid `rusage`.
        let mut rusage: libc::rusage = unsafe { std::mem::zeroed() };
        // `WNOHANG` never blocks; an interruption is retried a few times, then is the error.
        for _ in 0..4 {
            // SAFETY: live locals the call writes and does not keep.
            let reaped = unsafe { libc::wait4(pid, &mut status, libc::WNOHANG, &mut rusage) };
            match reaped {
                0 => return Ok(None),
                n if n == pid => return Ok(Some((status, rusage))),
                _ if errno() == libc::EINTR => {}
                _ => return Err(io::Error::last_os_error()),
            }
        }
        Err(io::Error::from_raw_os_error(libc::EINTR))
    }

    fn exited<C: Clock>(
        &self,
        what: &'static str,
        pid: i32,
        keep: bool,
        deadline: Option<Deadline>,
        clock: &C,
    ) -> Result<Waited<()>, WaitError> {
        with_ring(what, |ring| ring.waitid(what, pid, keep, deadline, clock))
    }
}

/// Waits until `fd` is ready for `events` through this thread's ring: its `revents`.
pub(crate) fn poll(
    what: &'static str,
    fd: RawFd,
    events: libc::c_short,
    deadline: Option<Deadline>,
    clock: &impl Clock,
) -> Result<Waited<libc::c_short>, WaitError> {
    with_ring(what, |ring| ring.poll(what, fd, events, deadline, clock))
}

/// Sleeps until `deadline` on `clock` on a ring timeout.
pub(crate) fn sleep_until(
    what: &'static str,
    deadline: Deadline,
    clock: &impl Clock,
) -> Result<(), WaitError> {
    with_ring(what, |ring| ring.sleep_until(what, deadline, clock))
}
