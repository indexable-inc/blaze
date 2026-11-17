//! io_uring's three calls: what the `Uring` driver and the wait ring are made with. The
//! rings themselves (the mapped queues, the SQEs and CQEs) stay the driver's.

use super::{
    fs::{OwnedFd, arg, owned},
    nr, raw,
    wait::SigSet,
};
use crate::{Errno, Fd, Timespec};

flag_set! {
    /// `io_uring_setup`'s flags.
    SetupFlags: u32 {
        /// `IORING_SETUP_SQPOLL`.
        SQ_POLL = 1 << 1;
        /// `IORING_SETUP_CQSIZE`.
        CQ_SIZE = 1 << 3;
        /// `IORING_SETUP_CLAMP`.
        CLAMP = 1 << 4;
        /// `IORING_SETUP_SUBMIT_ALL`.
        SUBMIT_ALL = 1 << 7;
        /// `IORING_SETUP_COOP_TASKRUN`.
        COOP_TASKRUN = 1 << 8;
        /// `IORING_SETUP_SINGLE_ISSUER`.
        SINGLE_ISSUER = 1 << 12;
        /// `IORING_SETUP_DEFER_TASKRUN`.
        DEFER_TASKRUN = 1 << 13;
    }
}

/// `struct io_sqring_offsets`.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct SqOffsets {
    pub head: u32,
    pub tail: u32,
    pub ring_mask: u32,
    pub ring_entries: u32,
    pub flags: u32,
    pub dropped: u32,
    pub array: u32,
    pub resv1: u32,
    pub user_addr: u64,
}

/// `struct io_cqring_offsets`.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct CqOffsets {
    pub head: u32,
    pub tail: u32,
    pub ring_mask: u32,
    pub ring_entries: u32,
    pub overflow: u32,
    pub cqes: u32,
    pub flags: u32,
    pub resv1: u32,
    pub user_addr: u64,
}

/// `struct io_uring_params` (120 bytes): what a ring is asked to be, and what the kernel made.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct UringParams {
    pub sq_entries: u32,
    pub cq_entries: u32,
    /// The flags' bits ([`SetupFlags::bits`]).
    pub flags: u32,
    pub sq_thread_cpu: u32,
    pub sq_thread_idle: u32,
    /// What the kernel supports (`IORING_FEAT_*`), written by it.
    pub features: u32,
    pub wq_fd: u32,
    pub resv: [u32; 3],
    pub sq_off: SqOffsets,
    pub cq_off: CqOffsets,
}

/// `io_uring_setup`: a ring of `entries` submissions as `params` ask; the kernel writes what
/// it made into `params`. [`Errno::NoSys`] where a seccomp filter refuses io_uring (blaze's
/// sandbox): the process then waits with the direct driver, chosen once in `main`, never by
/// trying this call.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn io_uring_setup(entries: u32, params: &mut UringParams) -> Result<OwnedFd, Errno> {
    // SAFETY: `params` is a live `io_uring_params` the kernel reads and writes.
    owned(unsafe {
        raw::syscall2(nr::IO_URING_SETUP, entries as usize, core::ptr::from_mut(params) as usize)
    })
}

flag_set! {
    /// `io_uring_enter`'s flags. `IORING_ENTER_EXT_ARG` is set by [`io_uring_enter`] itself
    /// when it is given an [`EnterWait`].
    EnterFlags: u32 {
        /// `IORING_ENTER_GETEVENTS`: wait for `min_complete` completions.
        GET_EVENTS = 1 << 0;
        /// `IORING_ENTER_SQ_WAKEUP`: wake the kernel's submission thread.
        SQ_WAKEUP = 1 << 1;
    }
}

/// `IORING_ENTER_EXT_ARG`.
const ENTER_EXT_ARG: u32 = 1 << 3;

/// How an [`io_uring_enter`] waits: for up to `left` (no limit for `None`), under `mask` (the
/// thread's own for `None`).
#[derive(Clone, Copy, Debug)]
pub struct EnterWait {
    /// What is left of the wait's deadline, recomputed by `wait::until` before each entry.
    pub left: Option<Timespec>,
    /// The signal mask for the call's length.
    pub mask: Option<SigSet>,
}

/// `struct io_uring_getevents_arg`.
#[repr(C)]
struct GeteventsArg {
    sigmask: u64,
    sigmask_sz: u32,
    pad: u32,
    ts: u64,
}

/// One `io_uring_enter`: `to_submit` submissions handed over, and with
/// [`EnterFlags::GET_EVENTS`] a wait for `min_complete` completions as `wait` says: how many
/// submissions the kernel took. A wait whose time ran out is [`ETIME`] (the kernel's `ETIME`,
/// not `ETIMEDOUT`); [`Errno::Intr`]; [`Errno::Busy`] where completions are held back. The one
/// blocking call of the io_uring driver, made through `wait::until`.
///
/// # Safety
///
/// Every submission entry the ring holds is valid for its operation: its buffers and paths
/// live until its completion is reaped.
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn io_uring_enter(
    ring: Fd,
    to_submit: u32,
    min_complete: u32,
    flags: EnterFlags,
    wait: Option<&EnterWait>,
) -> Result<u32, Errno> {
    let argument;
    let (flags, pointer, size) = match wait {
        None => (flags.bits(), 0, 0),
        Some(wait) => {
            argument = GeteventsArg {
                sigmask: wait.mask.as_ref().map_or(0, |it| core::ptr::from_ref(it) as u64),
                sigmask_sz: if wait.mask.is_some() { size_of::<SigSet>() as u32 } else { 0 },
                pad: 0,
                ts: wait.left.as_ref().map_or(0, |it| core::ptr::from_ref(it) as u64),
            };
            (flags.bits() | ENTER_EXT_ARG, core::ptr::from_ref(&argument) as usize, size_of::<GeteventsArg>())
        }
    };
    // SAFETY: the argument (null for none) is a live `io_uring_getevents_arg` of the size
    // passed, naming a live mask and timespec or null; the kernel keeps none of them. The
    // caller answers for the ring's entries.
    raw::check(unsafe {
        raw::syscall6(
            nr::IO_URING_ENTER,
            arg(ring),
            to_submit as usize,
            min_complete as usize,
            flags as usize,
            pointer,
            size,
        )
    })
    .map(|taken| taken as u32)
}

/// `ETIME`: what an [`io_uring_enter`] whose time ran out returns (62 on Linux; not
/// `ETIMEDOUT`).
pub const ETIME: Errno = Errno::Unlisted(62);

/// One typed `io_uring_register`.
#[derive(Clone, Copy, Debug)]
pub enum Register<'a> {
    /// `IORING_REGISTER_BUFFERS`: fixed buffers, pinned for the ring's life. Each is an
    /// `iovec`: an address and a length.
    Buffers(&'a [[usize; 2]]),
    /// `IORING_UNREGISTER_BUFFERS`.
    UnregisterBuffers,
    /// `IORING_REGISTER_FILES`: fixed descriptors (-1 for an empty slot).
    Files(&'a [i32]),
    /// `IORING_UNREGISTER_FILES`.
    UnregisterFiles,
}

/// `io_uring_register`, one typed request. Used by the ring's registered buffers
/// (`Effect::WriteFrom`'s spans of the page pool) and files.
///
/// # Safety
///
/// For [`Register::Buffers`], each address and length is memory that stays mapped until the
/// buffers are unregistered or the ring is closed.
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn io_uring_register(ring: Fd, request: Register<'_>) -> Result<(), Errno> {
    let (opcode, pointer, count) = match request {
        Register::Buffers(buffers) => (0, buffers.as_ptr() as usize, buffers.len()),
        Register::UnregisterBuffers => (1, 0, 0),
        Register::Files(files) => (2, files.as_ptr() as usize, files.len()),
        Register::UnregisterFiles => (3, 0, 0),
    };
    // SAFETY: the array (null for none) is `count` live entries the kernel reads; the caller
    // answers for the memory registered buffers name.
    raw::check(unsafe { raw::syscall4(nr::IO_URING_REGISTER, arg(ring), opcode, pointer, count) })
        .map(|_| ())
}
