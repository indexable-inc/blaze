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
        /// `IORING_SETUP_R_DISABLED` (5.10): the ring starts disabled, enabled by
        /// [`Register::EnableRings`] from the thread that will issue on it.
        /// replaces blaze_core::wait::ring::SETUP_R_DISABLED (a private copy; the libc crate has no io_uring items)
        R_DISABLED = 1 << 6;
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
/// replaces libc::syscall(libc::SYS_io_uring_setup) (as blaze_sys::linux::io_uring_setup(entries, &mut UringParams); returns Result<OwnedFd, Errno>, the site's -1 check becomes the Err; a probe with a null params pointer has no form here)
/// replaces libc::SYS_io_uring_setup
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
        /// `IORING_ENTER_ABS_TIMER` (6.12): the wait's time is an absolute time on the ring's
        /// clock (`CLOCK_MONOTONIC` unless registered otherwise), not a length.
        /// replaces blaze_core::wait::ring::ENTER_ABS_TIMER (a private copy; the libc crate has no io_uring items)
        ABS_TIMER = 1 << 5;
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
/// replaces libc::syscall(libc::SYS_io_uring_enter) (as blaze_sys::linux::io_uring_enter(ring: Fd, to_submit, min_complete, EnterFlags, Option<&EnterWait>); EnterWait builds IORING_ENTER_EXT_ARG's io_uring_getevents_arg for you; returns Result<u32, Errno>; a registered wait region (IORING_ENTER_EXT_ARG_REG) is blaze_sys::linux::io_uring_enter_reg_wait)
/// replaces libc::SYS_io_uring_enter
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
    /// `IORING_REGISTER_FILES2` (5.13; sparse with [`RSRC_REGISTER_SPARSE`], 5.19): a table of
    /// registered files, `nr_args` the struct's size.
    /// replaces libc::syscall(libc::SYS_io_uring_register) with IORING_REGISTER_FILES2 (as blaze_sys::linux::io_uring_register(ring, Register::Files2(&RsrcRegister)))
    Files2(&'a RsrcRegister),
    /// `IORING_REGISTER_FILES_UPDATE2` (5.13): slots of the table set (-1 empties one),
    /// `nr_args` the struct's size.
    /// replaces libc::syscall(libc::SYS_io_uring_register) with IORING_REGISTER_FILES_UPDATE2 (as blaze_sys::linux::io_uring_register(ring, Register::FilesUpdate2(&RsrcUpdate2)); the kernel's count of slots set is dropped: Ok means every slot asked was set)
    FilesUpdate2(&'a RsrcUpdate2),
    /// `IORING_REGISTER_FILE_ALLOC_RANGE` (6.0): the slots a direct open or accept may take,
    /// `nr_args` 0.
    /// replaces libc::syscall(libc::SYS_io_uring_register) with IORING_REGISTER_FILE_ALLOC_RANGE (as blaze_sys::linux::io_uring_register(ring, Register::FileAllocRange(&FileIndexRange)))
    FileAllocRange(&'a FileIndexRange),
    /// `IORING_REGISTER_PBUF_RING` (5.19): a ring of provided buffers, `nr_args` 1.
    /// replaces libc::syscall(libc::SYS_io_uring_register) with IORING_REGISTER_PBUF_RING (as blaze_sys::linux::io_uring_register(ring, Register::PbufRing(&BufReg)); `resv` is the caller's, for the kernels that check it inverted)
    PbufRing(&'a BufReg),
    /// `IORING_UNREGISTER_PBUF_RING` (5.19): the group `bgid` names given back, `nr_args` 1.
    /// replaces libc::syscall(libc::SYS_io_uring_register) with IORING_UNREGISTER_PBUF_RING (as blaze_sys::linux::io_uring_register(ring, Register::UnregisterPbufRing(&BufReg)))
    UnregisterPbufRing(&'a BufReg),
    /// `IORING_REGISTER_ENABLE_RINGS` (5.10): a ring set up with [`SetupFlags::R_DISABLED`]
    /// enabled, `nr_args` 0. With `SINGLE_ISSUER`, the calling thread becomes its issuer.
    /// replaces libc::syscall(libc::SYS_io_uring_register) with IORING_REGISTER_ENABLE_RINGS (as blaze_sys::linux::io_uring_register(ring, Register::EnableRings))
    EnableRings,
    /// `IORING_REGISTER_MEM_REGION` (6.13): a region of the caller's memory registered,
    /// `nr_args` 1; with [`MEM_REGION_REG_WAIT_ARG`], the region holds the wait arguments
    /// [`io_uring_enter_reg_wait`] names by offset.
    /// replaces libc::syscall(libc::SYS_io_uring_register) with IORING_REGISTER_MEM_REGION (as blaze_sys::linux::io_uring_register(ring, Register::MemRegion(&MemRegionReg)))
    MemRegion(&'a MemRegionReg),
}

/// `IORING_MEM_REGION_TYPE_USER`: [`RegionDesc::flags`]'s bit for memory the caller mapped
/// (`user_addr`), not memory the kernel allocates.
/// replaces blaze_core::wait::ring::MEM_REGION_TYPE_USER (a private copy; the libc crate has no io_uring items)
pub const MEM_REGION_TYPE_USER: u32 = 1;

/// `IORING_MEM_REGION_REG_WAIT_ARG`: [`MemRegionReg::flags`]'s bit for a region of
/// [`RegWait`] arguments.
/// replaces blaze_core::wait::ring::MEM_REGION_REG_WAIT_ARG (a private copy; the libc crate has no io_uring items)
pub const MEM_REGION_REG_WAIT_ARG: u64 = 1;

/// `IORING_REG_WAIT_TS`: [`RegWait::flags`]'s bit saying `ts` holds the wait's time.
/// replaces blaze_core::wait::ring::REG_WAIT_TS (a private copy; the libc crate has no io_uring items)
pub const REG_WAIT_TS: u32 = 1;

/// `struct io_uring_region_desc` (linux/io_uring.h, 64 bytes): one region of memory.
/// replaces blaze_core::wait::ring::RegionDesc (a private copy; the libc crate has no io_uring structs)
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct RegionDesc {
    /// `user_addr`: the region's address, with [`MEM_REGION_TYPE_USER`].
    pub user_addr: u64,
    /// `size`: its bytes, a whole number of pages.
    pub size: u64,
    /// `flags`: [`MEM_REGION_TYPE_USER`] or 0.
    pub flags: u32,
    /// `id`: 0.
    pub id: u32,
    /// `mmap_offset`: written by the kernel for a region it allocates.
    pub mmap_offset: u64,
    /// `__resv`: 0.
    pub resv: [u64; 4],
}

/// `struct io_uring_mem_region_reg` (linux/io_uring.h, 32 bytes): what
/// [`Register::MemRegion`] reads.
/// replaces blaze_core::wait::ring::MemRegionReg (a private copy; the libc crate has no io_uring structs)
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct MemRegionReg {
    /// `region_uptr`: the address of a live, writable [`RegionDesc`] for the call: the
    /// kernel copies it in and writes it back (`copy_to_user`, its `mmap_offset`), so it is
    /// made from `ptr::from_mut`, never from a shared reference.
    pub region_uptr: u64,
    /// `flags`: [`MEM_REGION_REG_WAIT_ARG`] or 0.
    pub flags: u64,
    /// `__resv`: 0.
    pub resv: [u64; 2],
}

/// `struct io_uring_reg_wait` (linux/io_uring.h, 64 bytes): one wait's argument in a
/// registered region, written by the caller before [`io_uring_enter_reg_wait`].
/// replaces blaze_core::wait::ring::RegWait (a private copy; the libc crate has no io_uring structs)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct RegWait {
    /// `ts`: the wait's time, with [`REG_WAIT_TS`].
    pub ts: Timespec,
    /// `min_wait_usec`: 0.
    pub min_wait_usec: u32,
    /// `flags`: [`REG_WAIT_TS`] or 0.
    pub flags: u32,
    /// `sigmask`: the address of a signal mask, or 0 for the thread's own.
    pub sigmask: u64,
    /// `sigmask_sz`: that mask's bytes (8), or 0.
    pub sigmask_sz: u32,
    /// `pad`: 0.
    pub pad: [u32; 3],
    /// `pad2`: 0.
    pub pad2: [u64; 2],
}

impl RegWait {
    /// Every field 0: no time, the thread's own mask.
    pub const ZERO: RegWait = RegWait {
        ts: Timespec::ZERO,
        min_wait_usec: 0,
        flags: 0,
        sigmask: 0,
        sigmask_sz: 0,
        pad: [0; 3],
        pad2: [0; 2],
    };
}

// linux/io_uring.h's region structs on a 64-bit target (asserted against the header's sizes).
const _: () = {
    assert!(size_of::<RegionDesc>() == 64);
    assert!(core::mem::offset_of!(RegionDesc, size) == 8);
    assert!(core::mem::offset_of!(RegionDesc, flags) == 16);
    assert!(core::mem::offset_of!(RegionDesc, id) == 20);
    assert!(core::mem::offset_of!(RegionDesc, mmap_offset) == 24);
    assert!(core::mem::offset_of!(RegionDesc, resv) == 32);
    assert!(size_of::<MemRegionReg>() == 32);
    assert!(core::mem::offset_of!(MemRegionReg, flags) == 8);
    assert!(core::mem::offset_of!(MemRegionReg, resv) == 16);
    assert!(size_of::<RegWait>() == 64);
    assert!(core::mem::offset_of!(RegWait, min_wait_usec) == 16);
    assert!(core::mem::offset_of!(RegWait, flags) == 20);
    assert!(core::mem::offset_of!(RegWait, sigmask) == 24);
    assert!(core::mem::offset_of!(RegWait, sigmask_sz) == 32);
    assert!(core::mem::offset_of!(RegWait, pad2) == 48);
};

/// `IORING_ENTER_EXT_ARG_REG` (6.13): the extended argument is an offset into the ring's
/// registered wait region.
const ENTER_EXT_ARG_REG: u32 = 1 << 6;

/// `io_uring_enter` with the wait's argument in the ring's registered region
/// ([`Register::MemRegion`] with [`MEM_REGION_REG_WAIT_ARG`]): `at` is the byte offset of
/// that [`RegWait`] in the region. Otherwise as [`io_uring_enter`]: how many submissions the
/// kernel took; [`ETIME`] where the time ran out; [`Errno::Intr`].
///
/// # Safety
///
/// As [`io_uring_enter`]'s; and `at` is the offset of a [`RegWait`] inside the registered
/// region, which the caller wrote and does not write during the call.
/// replaces libc::syscall(libc::SYS_io_uring_enter) with IORING_ENTER_EXT_ARG_REG (as blaze_sys::linux::io_uring_enter_reg_wait(ring, to_submit, min_complete, EnterFlags::GET_EVENTS | maybe ABS_TIMER, offset); EXT_ARG and EXT_ARG_REG are set here)
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn io_uring_enter_reg_wait(
    ring: Fd,
    to_submit: u32,
    min_complete: u32,
    flags: EnterFlags,
    at: usize,
) -> Result<u32, Errno> {
    let flags = flags.bits() | ENTER_EXT_ARG | ENTER_EXT_ARG_REG;
    // SAFETY: with EXT_ARG_REG the argument is an offset into the registered region, not a
    // pointer, and its size that of one `io_uring_reg_wait`; the caller answers for the
    // region's entry and the ring's submissions.
    raw::check(unsafe {
        raw::syscall6(
            nr::IO_URING_ENTER,
            arg(ring),
            to_submit as usize,
            min_complete as usize,
            flags as usize,
            at,
            size_of::<RegWait>(),
        )
    })
    .map(|taken| taken as u32)
}

/// `IORING_RSRC_REGISTER_SPARSE`: [`RsrcRegister::flags`]'s bit for a table of empty slots
/// (`data` and `tags` 0).
/// replaces blaze_core::uring::RSRC_REGISTER_SPARSE (a private copy; the libc crate has no io_uring items)
pub const RSRC_REGISTER_SPARSE: u32 = 1 << 0;

/// `struct io_uring_rsrc_register` (linux/io_uring.h, 32 bytes): a table of `nr` resources,
/// read by [`Register::Files2`].
/// replaces blaze_core::uring::RsrcRegister (a private copy; the libc crate has no io_uring structs)
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct RsrcRegister {
    /// `nr`: how many slots.
    pub nr: u32,
    /// `flags`: [`RSRC_REGISTER_SPARSE`] or 0.
    pub flags: u32,
    /// `resv2`: 0.
    pub resv2: u64,
    /// `data`: the address of `nr` descriptors (`i32`), or 0 when sparse.
    pub data: u64,
    /// `tags`: the address of `nr` `u64` tags, or 0.
    pub tags: u64,
}

/// `struct io_uring_rsrc_update2` (linux/io_uring.h, 32 bytes): `nr` slots from `offset`
/// set, read by [`Register::FilesUpdate2`].
/// replaces blaze_core::uring::RsrcUpdate2 (a private copy; the libc crate has no io_uring structs)
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct RsrcUpdate2 {
    /// `offset`: the first slot.
    pub offset: u32,
    /// `resv`: 0.
    pub resv: u32,
    /// `data`: the address of `nr` descriptors (`i32`; -1 empties a slot).
    pub data: u64,
    /// `tags`: the address of `nr` `u64` tags, or 0.
    pub tags: u64,
    /// `nr`: how many slots.
    pub nr: u32,
    /// `resv2`: 0.
    pub resv2: u32,
}

/// `struct io_uring_file_index_range` (linux/io_uring.h, 16 bytes): the slots `off..off +
/// len` of the file table, read by [`Register::FileAllocRange`].
/// replaces blaze_core::uring::FileIndexRange (a private copy; the libc crate has no io_uring structs)
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct FileIndexRange {
    /// `off`: the first slot.
    pub off: u32,
    /// `len`: how many.
    pub len: u32,
    /// `resv`: 0.
    pub resv: u64,
}

/// `struct io_uring_buf_reg` (linux/io_uring.h, 40 bytes): a ring of provided buffers for
/// group `bgid`, read by [`Register::PbufRing`] and [`Register::UnregisterPbufRing`].
/// replaces blaze_core::uring::BufReg (a private copy; the libc crate has no io_uring structs)
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct BufReg {
    /// `ring_addr`: the ring's address (page-aligned, mapped by the caller), 0 to unregister.
    pub ring_addr: u64,
    /// `ring_entries`: a power of two.
    pub ring_entries: u32,
    /// `bgid`: the buffer group.
    pub bgid: u16,
    /// `flags`: `IOU_PBUF_RING_*`, 0 for a caller-mapped ring.
    pub flags: u16,
    /// `resv`: 0 upstream; some vendor kernels check it inverted (the caller's probe says).
    pub resv: [u64; 3],
}

// linux/io_uring.h, the same on x86-64 and aarch64 (fixed-width fields, 8-aligned).
const _: () = {
    assert!(size_of::<RsrcRegister>() == 32);
    assert!(align_of::<RsrcRegister>() == 8);
    assert!(core::mem::offset_of!(RsrcRegister, nr) == 0);
    assert!(core::mem::offset_of!(RsrcRegister, flags) == 4);
    assert!(core::mem::offset_of!(RsrcRegister, resv2) == 8);
    assert!(core::mem::offset_of!(RsrcRegister, data) == 16);
    assert!(core::mem::offset_of!(RsrcRegister, tags) == 24);
    assert!(size_of::<RsrcUpdate2>() == 32);
    assert!(align_of::<RsrcUpdate2>() == 8);
    assert!(core::mem::offset_of!(RsrcUpdate2, offset) == 0);
    assert!(core::mem::offset_of!(RsrcUpdate2, resv) == 4);
    assert!(core::mem::offset_of!(RsrcUpdate2, data) == 8);
    assert!(core::mem::offset_of!(RsrcUpdate2, tags) == 16);
    assert!(core::mem::offset_of!(RsrcUpdate2, nr) == 24);
    assert!(core::mem::offset_of!(RsrcUpdate2, resv2) == 28);
    assert!(size_of::<FileIndexRange>() == 16);
    assert!(align_of::<FileIndexRange>() == 8);
    assert!(core::mem::offset_of!(FileIndexRange, off) == 0);
    assert!(core::mem::offset_of!(FileIndexRange, len) == 4);
    assert!(core::mem::offset_of!(FileIndexRange, resv) == 8);
    assert!(size_of::<BufReg>() == 40);
    assert!(align_of::<BufReg>() == 8);
    assert!(core::mem::offset_of!(BufReg, ring_addr) == 0);
    assert!(core::mem::offset_of!(BufReg, ring_entries) == 8);
    assert!(core::mem::offset_of!(BufReg, bgid) == 12);
    assert!(core::mem::offset_of!(BufReg, flags) == 14);
    assert!(core::mem::offset_of!(BufReg, resv) == 16);
};

/// `io_uring_register`, one typed request. Used by the ring's registered buffers
/// (`Effect::WriteFrom`'s spans of the page pool) and files.
///
/// # Safety
///
/// For [`Register::Buffers`], each address and length is memory that stays mapped until the
/// buffers are unregistered or the ring is closed. For [`Register::Files2`] and
/// [`Register::FilesUpdate2`], `data` (and `tags`, where not 0) is the address of `nr` live
/// entries for the call. For [`Register::PbufRing`], `ring_addr` is `ring_entries` entries of
/// memory that stays mapped while the group is registered. For [`Register::MemRegion`],
/// `region_uptr` names a live descriptor the caller holds exclusively (the kernel writes it
/// back), and its `user_addr` is `size` bytes, a whole number of pages and page-aligned, that
/// stay mapped until the ring is closed.
/// replaces libc::syscall(libc::SYS_io_uring_register) (as blaze_sys::linux::io_uring_register(ring: Fd, Register::...); opcodes 0..=3 (buffers, files), ENABLE_RINGS (12), FILES2 (13), FILES_UPDATE2 (14), PBUF_RING (22), UNREGISTER_PBUF_RING (23), FILE_ALLOC_RANGE (25), MEM_REGION (34); returns Result<(), Errno>)
/// replaces libc::SYS_io_uring_register
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn io_uring_register(ring: Fd, request: Register<'_>) -> Result<(), Errno> {
    let (opcode, pointer, count) = match request {
        Register::Buffers(buffers) => (0, buffers.as_ptr() as usize, buffers.len()),
        Register::UnregisterBuffers => (1, 0, 0),
        Register::Files(files) => (2, files.as_ptr() as usize, files.len()),
        Register::UnregisterFiles => (3, 0, 0),
        Register::Files2(table) => {
            (13, core::ptr::from_ref(table) as usize, size_of::<RsrcRegister>())
        }
        Register::FilesUpdate2(update) => {
            (14, core::ptr::from_ref(update) as usize, size_of::<RsrcUpdate2>())
        }
        Register::PbufRing(reg) => (22, core::ptr::from_ref(reg) as usize, 1),
        Register::UnregisterPbufRing(reg) => (23, core::ptr::from_ref(reg) as usize, 1),
        Register::FileAllocRange(range) => (25, core::ptr::from_ref(range) as usize, 0),
        Register::EnableRings => (12, 0, 0),
        Register::MemRegion(reg) => (34, core::ptr::from_ref(reg) as usize, 1),
    };
    // SAFETY: the argument (null for none) is `count` live entries, or the one live struct
    // the opcode reads, which the kernel reads during the call and does not keep; the caller
    // answers for the memory the structs' addresses name.
    raw::check(unsafe { raw::syscall4(nr::IO_URING_REGISTER, arg(ring), opcode, pointer, count) })
        .map(|_| ())
}

/// The typed registrations on the real kernel: a sparse table, its allocation range, a slot
/// set and emptied, and an unregister of a group the ring lacks (`ENOENT` upstream, `EINVAL`
/// where a kernel checks the reserved words inverted).
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_table_and_provided_buffers_register() {
        let mut params = UringParams::default();
        let ring = match io_uring_setup(4, &mut params) {
            Ok(ring) => ring,
            Err(Errno::NoSys | Errno::Perm) => {
                eprintln!("skipped: this host refuses io_uring_setup");
                return;
            }
            Err(other) => panic!("io_uring_setup: {other}"),
        };
        let sparse = RsrcRegister { nr: 8, flags: RSRC_REGISTER_SPARSE, ..RsrcRegister::default() };
        // SAFETY: sparse: no addresses; the range and the update name live locals.
        unsafe {
            match io_uring_register(ring.fd(), Register::Files2(&sparse)) {
                Ok(()) => {}
                Err(Errno::Inval) => {
                    eprintln!("skipped: this kernel has no sparse file table (5.19)");
                    return;
                }
                Err(other) => panic!("FILES2: {other}"),
            }
            let range = FileIndexRange { off: 4, len: 4, resv: 0 };
            match io_uring_register(ring.fd(), Register::FileAllocRange(&range)) {
                Ok(()) => {}
                Err(Errno::Inval) => eprintln!("this kernel has no FILE_ALLOC_RANGE (6.0)"),
                Err(other) => panic!("FILE_ALLOC_RANGE: {other}"),
            }
            let fds = [ring.fd().raw()];
            let set = RsrcUpdate2 { offset: 0, data: fds.as_ptr() as u64, nr: 1, ..RsrcUpdate2::default() };
            assert_eq!(io_uring_register(ring.fd(), Register::FilesUpdate2(&set)), Err(Errno::BadF), "a ring's own file is refused");
            let empty = [-1i32];
            let clear = RsrcUpdate2 { offset: 0, data: empty.as_ptr() as u64, nr: 1, ..RsrcUpdate2::default() };
            assert_eq!(io_uring_register(ring.fd(), Register::FilesUpdate2(&clear)), Ok(()));
            let none = BufReg { bgid: 7, ..BufReg::default() };
            let answer = io_uring_register(ring.fd(), Register::UnregisterPbufRing(&none));
            assert!(matches!(answer, Err(Errno::NoEnt | Errno::Inval)), "{answer:?}");
        }
    }

    /// A disabled ring gets a wait region, is enabled, and an entry whose registered wait
    /// names a time already past ends with `ETIME`: the wait ring's setup, end to end.
    #[test]
    fn a_registered_wait_region_ends_a_wait_at_its_time() {
        use super::super::mem::{MapFlags, Prot, mmap, munmap};
        use super::super::wait::{ClockId, clock_gettime};
        let flags = SetupFlags::R_DISABLED | SetupFlags::SINGLE_ISSUER | SetupFlags::DEFER_TASKRUN;
        let mut params = UringParams { flags: flags.bits(), ..UringParams::default() };
        let ring = match io_uring_setup(4, &mut params) {
            Ok(ring) => ring,
            Err(Errno::NoSys | Errno::Perm | Errno::Inval) => {
                eprintln!("skipped: no io_uring with R_DISABLED and DEFER_TASKRUN here");
                return;
            }
            Err(other) => panic!("io_uring_setup: {other}"),
        };
        const PAGE: usize = 1 << 16; // a whole number of pages on 4 KiB, 16 KiB and 64 KiB kernels
        // SAFETY: a new anonymous mapping the kernel places; nothing else uses it.
        let region = unsafe {
            mmap(None, PAGE, Prot::READ | Prot::WRITE, MapFlags::PRIVATE | MapFlags::ANONYMOUS, None, 0)
        }
        .unwrap();
        // The kernel writes the descriptor back (its `mmap_offset`): a writable local.
        let mut desc = RegionDesc { user_addr: region.as_ptr() as u64, size: PAGE as u64, flags: MEM_REGION_TYPE_USER, ..RegionDesc::default() };
        let reg = MemRegionReg { region_uptr: core::ptr::from_mut(&mut desc) as u64, flags: MEM_REGION_REG_WAIT_ARG, resv: [0; 2] };
        // SAFETY: the region stays mapped until after the ring is closed, below.
        match unsafe { io_uring_register(ring.fd(), Register::MemRegion(&reg)) } {
            Ok(()) => {}
            Err(Errno::Inval) => {
                eprintln!("skipped: this kernel has no MEM_REGION (6.13)");
                drop(ring);
                // SAFETY: the mapping made above; nothing uses it now.
                unsafe { munmap(region, PAGE) }.unwrap();
                return;
            }
            Err(other) => panic!("MEM_REGION: {other}"),
        }
        // SAFETY: no submission entries yet.
        assert_eq!(unsafe { io_uring_register(ring.fd(), Register::EnableRings) }, Ok(()));
        let past = clock_gettime(ClockId::Monotonic).unwrap();
        // SAFETY: offset 0 of the registered region, a live, aligned, writable page.
        unsafe { region.cast::<RegWait>().write(RegWait { ts: past, flags: REG_WAIT_TS, ..RegWait::ZERO }) };
        // SAFETY: no submissions; the region's entry at 0 was written above.
        let entered = unsafe {
            io_uring_enter_reg_wait(ring.fd(), 0, 1, EnterFlags::GET_EVENTS | EnterFlags::ABS_TIMER, 0)
        };
        assert_eq!(entered, Err(ETIME));
        drop(ring);
        // SAFETY: the ring that registered the region is closed; nothing uses the page.
        unsafe { munmap(region, PAGE) }.unwrap();
    }
}
