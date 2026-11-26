//! Cancellable, deadline-bounded positional reads (`IORING_OP_READ` + `IORING_OP_ASYNC_CANCEL`)
//! on already-opened Linux file descriptors via a tiny private 2-entry `io_uring`.
//!
//! Used by `blaze-std`'s hermetic executable verifier (`VerifiedExec::open_and_seal`) and NixOS
//! ELF runtime-closure hasher (`feed_nix_runtime_closure`) so a stalled FUSE, NFS, or failing
//! block filesystem read can never hang past the caller's deadline or cancellation (AGENTS.md
//! rule 18).

use core::{
    ptr::NonNull,
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

use super::{
    ETIME, EnterFlags, EnterWait, MapFlags, OwnedFd, Prot, UringParams, io_uring_enter, io_uring_setup, mmap, munmap,
};
use crate::{Errno, Fd, Timespec};

/// Fixed page-aligned I/O buffer size (64 KiB).
pub const READ_BUF_LEN: usize = 64 << 10;

/// `IORING_OP_READ` (Linux 5.6).
const OP_READ: u8 = 22;
/// `IORING_OP_ASYNC_CANCEL` (Linux 5.5).
const OP_ASYNC_CANCEL: u8 = 14;
/// `IORING_OFF_SQ_RING`.
const OFF_SQ_RING: u64 = 0;
/// `IORING_OFF_SQES`.
const OFF_SQES: u64 = 0x1000_0000;
/// `user_data` tag for the active read SQE.
const READ_TAG: u64 = 1;
/// `user_data` tag for the async cancel SQE.
const CANCEL_TAG: u64 = 2;

/// Why [`DeadlineReader::pread_until`] ended without returning bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadUntilError {
    /// The caller's deadline/cancellation callback returned `None`; the in-flight kernel read
    /// was cancelled.
    Stopped,
    /// The kernel returned an error on `io_uring_enter` or the read completion itself.
    Os(Errno),
}

#[derive(Clone, Copy)]
#[repr(C)]
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

impl Sqe {
    const ZERO: Self = Self {
        opcode: 0,
        flags: 0,
        ioprio: 0,
        fd: 0,
        off: 0,
        addr: 0,
        len: 0,
        op_flags: 0,
        user_data: 0,
        buf_index: 0,
        personality: 0,
        file_index: 0,
        addr3: 0,
        pad: 0,
    };
}

#[allow(dead_code, reason = "kernel ABI layout written by io_uring")]
#[expect(blaze::unwired, reason = "wired: kernel ABI layout written by io_uring, not Rust")]
#[derive(Clone, Copy)]
#[repr(C)]
struct Cqe {
    user_data: u64,
    res: i32,
    flags: u32,
}

const _: () = {
    assert!(size_of::<Sqe>() == 64);
    assert!(size_of::<Cqe>() == 16);
};

struct Mapping {
    at: NonNull<u8>,
    len: usize,
}

impl Mapping {
    #[blaze::noalloc]
    #[blaze::nopanic]
    fn map_fd(len: usize, fd: Fd, offset: u64) -> Result<Self, Errno> {
        let prot = Prot::READ.with(Prot::WRITE);
        let flags = MapFlags::SHARED.with(MapFlags::POPULATE);
        // SAFETY: `mmap` on an io_uring fd at the kernel-reported ring/SQE offset returns a
        // fresh mapping owned exclusively by this `Mapping`.
        let at = unsafe { mmap(None, len, prot, flags, Some(fd), offset) }?;
        Ok(Self { at, len })
    }

    #[blaze::noalloc]
    #[blaze::nopanic]
    fn map_anon(len: usize) -> Result<Self, Errno> {
        let prot = Prot::READ.with(Prot::WRITE);
        let flags = MapFlags::PRIVATE.with(MapFlags::ANONYMOUS);
        // SAFETY: anonymous private mapping has no fd dependency and is owned by this `Mapping`.
        let at = unsafe { mmap(None, len, prot, flags, None, 0) }?;
        Ok(Self { at, len })
    }

    #[blaze::noalloc]
    #[blaze::nopanic]
    fn word(&self, offset: u32) -> Option<&AtomicU32> {
        let at = usize::try_from(offset).ok()?;
        if at.checked_add(size_of::<u32>())? > self.len || at % align_of::<AtomicU32>() != 0 {
            return None;
        }
        // SAFETY: bounds and alignment verified above inside this live mapping.
        Some(unsafe { &*self.at.as_ptr().wrapping_add(at).cast::<AtomicU32>() })
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: owned mapping created in `map_fd` or `map_anon`; unmapped once on drop.
        let _ = unsafe { munmap(self.at, self.len) };
    }
}

/// Private 2-entry Linux `io_uring` + 64 KiB mapped buffer for cancellable, deadline-bounded
/// positional reads on open file descriptors.
pub struct DeadlineReader {
    // Field drop order is safety-critical: `ring_fd` closes FIRST so the kernel cancels and
    // finishes any in-flight work referencing `buf`/`rings`/`sqes` before those mappings unmap.
    ring_fd: OwnedFd,
    buf: Mapping,
    rings: Mapping,
    sqes: Mapping,
    sq_entries: u32,
    sq_mask: u32,
    cq_mask: u32,
    sq_head: u32,
    sq_tail: u32,
    cq_head: u32,
    cq_tail: u32,
    cqes: u32,
    tail: u32,
    poisoned: bool,
}

impl DeadlineReader {
    /// Sets up a 2-entry `io_uring` and its 64 KiB page-aligned read buffer.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn new() -> Result<Self, Errno> {
        let mut params = UringParams::default();
        let ring_fd = io_uring_setup(4, &mut params)?;
        let (sq, cq) = (params.sq_off, params.cq_off);
        let sq_entries_usize = usize::try_from(params.sq_entries).map_err(|_| Errno::Fault)?;
        let cq_entries_usize = usize::try_from(params.cq_entries).map_err(|_| Errno::Fault)?;
        let sq_array_off = usize::try_from(sq.array).map_err(|_| Errno::Fault)?;
        let cq_cqes_off = usize::try_from(cq.cqes).map_err(|_| Errno::Fault)?;
        let sq_len = sq_array_off
            .checked_add(sq_entries_usize.checked_mul(size_of::<u32>()).ok_or(Errno::Fault)?)
            .ok_or(Errno::Fault)?;
        let cq_len = cq_cqes_off
            .checked_add(cq_entries_usize.checked_mul(size_of::<Cqe>()).ok_or(Errno::Fault)?)
            .ok_or(Errno::Fault)?;
        let rings = Mapping::map_fd(sq_len.max(cq_len), ring_fd.fd(), OFF_SQ_RING)?;
        let sqes_len = sq_entries_usize.checked_mul(size_of::<Sqe>()).ok_or(Errno::Fault)?;
        let sqes = Mapping::map_fd(sqes_len, ring_fd.fd(), OFF_SQES)?;
        let buf = Mapping::map_anon(READ_BUF_LEN)?;
        let mut index = 0u32;
        while index < params.sq_entries {
            let slot_off = sq
                .array
                .checked_add(index.checked_mul(4).ok_or(Errno::Fault)?)
                .ok_or(Errno::Fault)?;
            rings.word(slot_off).ok_or(Errno::Fault)?.store(index, Ordering::Relaxed);
            index = index.wrapping_add(1);
        }
        let sq_mask = rings.word(sq.ring_mask).ok_or(Errno::Fault)?.load(Ordering::Relaxed);
        let cq_mask = rings.word(cq.ring_mask).ok_or(Errno::Fault)?.load(Ordering::Relaxed);
        Ok(Self {
            ring_fd,
            buf,
            rings,
            sqes,
            sq_entries: params.sq_entries,
            sq_mask,
            cq_mask,
            sq_head: sq.head,
            sq_tail: sq.tail,
            cq_head: cq.head,
            cq_tail: cq.tail,
            cqes: cq.cqes,
            tail: 0,
            poisoned: false,
        })
    }

    #[blaze::noalloc]
    #[blaze::nopanic]
    fn push(&mut self, sqe: Sqe) -> bool {
        let (Some(head), Some(tail)) = (self.rings.word(self.sq_head), self.rings.word(self.sq_tail)) else {
            return false;
        };
        if self.tail.wrapping_sub(head.load(Ordering::Acquire)) >= self.sq_entries {
            return false;
        }
        let Ok(slot) = usize::try_from(self.tail & self.sq_mask) else {
            return false;
        };
        let Some(at) = slot.checked_mul(size_of::<Sqe>()) else {
            return false;
        };
        if at.saturating_add(size_of::<Sqe>()) > self.sqes.len {
            return false;
        }
        // SAFETY: `at` is 64-byte aligned inside `self.sqes` and past the kernel's SQ head.
        unsafe { self.sqes.at.as_ptr().wrapping_add(at).cast::<Sqe>().write(sqe) };
        self.tail = self.tail.wrapping_add(1);
        tail.store(self.tail, Ordering::Release);
        true
    }

    #[blaze::noalloc]
    #[blaze::nopanic]
    fn drain_for(&self, tag: u64) -> Option<i32> {
        let (Some(head_word), Some(tail_word)) = (self.rings.word(self.cq_head), self.rings.word(self.cq_tail)) else {
            return None;
        };
        let mut head = head_word.load(Ordering::Relaxed);
        let tail = tail_word.load(Ordering::Acquire);
        let mut found = None;
        while head != tail {
            if let Ok(slot) = usize::try_from(head & self.cq_mask)
                && let Ok(base) = usize::try_from(self.cqes)
                && let Some(off) = slot.checked_mul(size_of::<Cqe>())
                && let Some(at) = base.checked_add(off)
                && at.saturating_add(size_of::<Cqe>()) <= self.rings.len
            {
                // SAFETY: `at` is 16-byte aligned inside `self.rings` and published by `tail`.
                let cqe = unsafe { self.rings.at.as_ptr().wrapping_add(at).cast::<Cqe>().read() };
                if cqe.user_data == tag {
                    found = Some(cqe.res);
                }
            }
            head = head.wrapping_add(1);
        }
        head_word.store(head, Ordering::Release);
        found
    }

    #[blaze::noalloc]
    #[blaze::nopanic]
    fn cancel_read(&mut self) {
        let cancel = Sqe { opcode: OP_ASYNC_CANCEL, fd: -1, addr: READ_TAG, user_data: CANCEL_TAG, ..Sqe::ZERO };
        let submitted = if self.push(cancel) { 1 } else { 0 };
        let wait = EnterWait { left: Some(Timespec::from_duration(Duration::from_millis(20))), mask: None };
        // SAFETY: `cancel` references no user memory; `self.buf` stays mapped until `Drop`
        // closes `self.ring_fd` first.
        let _ = unsafe { io_uring_enter(self.ring_fd.fd(), submitted, 1, EnterFlags::GET_EVENTS, Some(&wait)) };
        if self.drain_for(READ_TAG).is_none() {
            // Drain once more in case the cancel CQE arrived slightly ahead of the read CQE.
            let wait2 = EnterWait { left: Some(Timespec::from_duration(Duration::from_millis(10))), mask: None };
            // SAFETY: same as above.
            let _ = unsafe { io_uring_enter(self.ring_fd.fd(), 0, 1, EnterFlags::GET_EVENTS, Some(&wait2)) };
            if self.drain_for(READ_TAG).is_none() {
                self.poisoned = true;
            }
        }
    }

    /// Performs one cancellable positional read (`IORING_OP_READ`, at `offset`; pass `u64::MAX`
    /// for non-seekable streams such as pipes) of up to `want.min(READ_BUF_LEN)` bytes from `fd`.
    ///
    /// Before every wait slice, calls `next_slice()`:
    /// - `Some(dur)` waits up to `dur` (looping on `ETIME`/`EINTR`);
    /// - `None` cancels the in-flight read via `IORING_OP_ASYNC_CANCEL` and returns
    ///   [`ReadUntilError::Stopped`].
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn pread_until(
        &mut self,
        fd: Fd,
        offset: u64,
        want: usize,
        mut next_slice: impl FnMut() -> Option<Duration>,
    ) -> Result<&[u8], ReadUntilError> {
        if self.poisoned {
            return Err(ReadUntilError::Stopped);
        }
        let Some(mut slice) = next_slice() else {
            return Err(ReadUntilError::Stopped);
        };
        let capped = want.min(READ_BUF_LEN);
        if capped == 0 {
            return Ok(&[]);
        }
        let Ok(len_u32) = u32::try_from(capped) else {
            return Err(ReadUntilError::Os(Errno::Inval));
        };
        let sqe = Sqe {
            opcode: OP_READ,
            fd: fd.raw(),
            off: offset,
            addr: self.buf.at.as_ptr() as usize as u64,
            len: len_u32,
            user_data: READ_TAG,
            ..Sqe::ZERO
        };
        if !self.push(sqe) {
            return Err(ReadUntilError::Os(Errno::Busy));
        }
        let mut to_submit = 1u32;
        loop {
            let wait = EnterWait { left: Some(Timespec::from_duration(slice)), mask: None };
            // SAFETY: `self.buf` stays alive until `READ_TAG` completes or `Drop` closes `ring_fd`
            // before unmapping `self.buf`.
            let entered = unsafe { io_uring_enter(self.ring_fd.fd(), to_submit, 1, EnterFlags::GET_EVENTS, Some(&wait)) };
            to_submit = 0;
            if let Some(res) = self.drain_for(READ_TAG) {
                if res < 0 {
                    let raw = res.unsigned_abs();
                    let code = u16::try_from(raw).unwrap_or(5);
                    return Err(ReadUntilError::Os(Errno::from_raw(code)));
                }
                let Ok(n) = usize::try_from(res) else {
                    return Err(ReadUntilError::Os(Errno::Io));
                };
                let got = n.min(capped);
                // SAFETY: the kernel completed `READ_TAG` with `got <= capped <= READ_BUF_LEN`
                // bytes into `self.buf` and no request is in flight.
                return Ok(unsafe { core::slice::from_raw_parts(self.buf.at.as_ptr(), got) });
            }
            if let Err(errno) = entered
                && errno != ETIME
                && errno != Errno::Intr
            {
                self.cancel_read();
                return Err(ReadUntilError::Os(errno));
            }
            match next_slice() {
                Some(next) => slice = next,
                None => {
                    self.cancel_read();
                    return Err(ReadUntilError::Stopped);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::{PipeFlags, pipe2, write};

    #[test]
    fn deadline_reader_cancels_blocked_pipe_read_within_bound() {
        let (rd, _wr) = pipe2(PipeFlags::CLOSE_ON_EXEC).expect("pipe2");
        let mut reader = DeadlineReader::new().expect("DeadlineReader::new");
        let started = std::time::Instant::now();
        let budget = Duration::from_millis(25);
        let res = reader.pread_until(rd.fd(), u64::MAX, 64, || {
            let left = budget.checked_sub(started.elapsed())?;
            (!left.is_zero()).then(|| left.min(Duration::from_millis(5)))
        });
        let elapsed = started.elapsed();
        assert_eq!(res, Err(ReadUntilError::Stopped), "blocked pipe read must cancel on deadline");
        assert!(elapsed < Duration::from_millis(250), "cancelled read hung too long: {elapsed:?}");
    }

    #[test]
    fn deadline_reader_reads_ready_pipe_bytes() {
        let (rd, wr) = pipe2(PipeFlags::CLOSE_ON_EXEC).expect("pipe2");
        write(wr.fd(), b"sealed-clang").expect("write");
        let mut reader = DeadlineReader::new().expect("DeadlineReader::new");
        let got = reader
            .pread_until(rd.fd(), u64::MAX, 64, || Some(Duration::from_millis(50)))
            .expect("read ready bytes");
        assert_eq!(got, b"sealed-clang");
    }
}
