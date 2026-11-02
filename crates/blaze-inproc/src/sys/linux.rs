//! Linux on x86-64 and AArch64: a memory object is a `memfd`, sealed against shrinking and
//! growing, mapped shared and writable in the daemon. A run process is given a read-only
//! reopen of a code object over `SCM_RIGHTS` (`/proc/self/fd/N` opened `O_RDONLY`), so it can
//! map it to read, execute or copy but never to write it shared; the control words and cells
//! go as they are.
//!
//! The start hook lays an overlay over its executable with one `mmap` of `MAP_FIXED`, which
//! the kernel does under the process's `mmap_lock`: a thread running in the range fetches its
//! next instruction from the old page or the new one, and the bytes are equal.

use std::ffi::{CString, c_int, c_void};
use std::ops::Range;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicU64};
use std::time::Duration;

pub(crate) use super::unix::{
    Bells, Errno, Listener, Scope, Stream, backtrace, connect, hung_up, random, ring, serialize, wait_answer,
};
use super::Rights;
use super::unix::protection;
use crate::channel::Channel;
use crate::frame::{self, kinds};
use crate::handshake::Overlay;
use crate::{Access, Error};

fn os(what: &str) -> Error {
    Error::new(format!("{what}: {}", std::io::Error::last_os_error()))
}

pub(crate) fn page() -> u64 {
    // SAFETY: sysconf has no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page > 0 { page as u64 } else { 4096 }
}

#[cfg(target_arch = "aarch64")]
fn ctr() -> u64 {
    let ctr: u64;
    // SAFETY: Linux lets EL0 read CTR_EL0 (SCTLR_EL1.UCT, or it traps and emulates it); the
    // read touches no memory.
    unsafe { std::arch::asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack, preserves_flags)) };
    ctr
}

/// AArch64: the instruction cache must not alias by virtual address, since code is written
/// through the daemon's view and run through another process's. PIPT, or VIPT whose way is
/// no larger than a page, or no maintenance needed at all (`CTR_EL0.DIC`).
pub(crate) fn check_icache() -> Result<(), Error> {
    #[cfg(target_arch = "aarch64")]
    {
        let ctr = ctr();
        let dic = ctr & (1 << 29) != 0;
        let l1ip = (ctr >> 14) & 3;
        if dic || l1ip == 0b11 {
            return Ok(());
        }
        if l1ip == 0b10 {
            let way = icache_way().map_err(|it| {
                Error::new(format!("CTR_EL0 {ctr:#x} reports a VIPT instruction cache and its geometry is unknown: {it}"))
            })?;
            if way <= page() {
                return Ok(());
            }
            return Err(Error::new(format!(
                "CTR_EL0 {ctr:#x} reports a VIPT instruction cache with ways of {way} bytes, larger than a page of {}: \
                 code written through one mapping may stay stale through another",
                page()
            )));
        }
        Err(Error::new(format!("CTR_EL0 {ctr:#x} reports an instruction cache policy {l1ip:#b} with aliases")))
    }
    #[cfg(target_arch = "x86_64")]
    Ok(())
}

/// The bytes of one way of the level-1 instruction cache, from sysfs.
#[cfg(target_arch = "aarch64")]
fn icache_way() -> Result<u64, String> {
    for index in 0..8 {
        let dir = format!("/sys/devices/system/cpu/cpu0/cache/index{index}");
        let read = |name: &str| std::fs::read_to_string(format!("{dir}/{name}")).map(|it| it.trim().to_owned());
        let Ok(kind) = read("type") else { break };
        if kind != "Instruction" || !read("level").is_ok_and(|it| it == "1") {
            continue;
        }
        let size = read("size").map_err(|it| it.to_string())?;
        let size: u64 = size
            .strip_suffix('K')
            .map(|it| it.parse::<u64>().map(|it| it << 10))
            .unwrap_or_else(|| size.parse())
            .map_err(|_| format!("{dir}/size is {size}"))?;
        let ways: u64 = read("ways_of_associativity").map_err(|it| it.to_string())?.parse().map_err(|_| format!("{dir}/ways_of_associativity"))?;
        if ways == 0 {
            return Err(format!("{dir} has no ways"));
        }
        return Ok(size / ways);
    }
    Err("no level-1 instruction cache under /sys/devices/system/cpu/cpu0/cache".to_owned())
}

pub(crate) struct Object {
    fd: OwnedFd,
    len: u64,
}

impl Object {
    pub(crate) fn create(name: &str, len: u64) -> Result<Object, Error> {
        let name = CString::new(format!("blaze-inproc:{name}")).map_err(|_| Error::new(format!("a memory object named {name:?}")))?;
        // SAFETY: `name` is a NUL-terminated string live for the call.
        let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            let hint = if error.raw_os_error() == Some(libc::EACCES) { " (vm.memfd_noexec is 2?)" } else { "" };
            return Err(Error::new(format!("memfd_create: {error}{hint}")));
        }
        // SAFETY: `fd` was just made and is owned by nothing else.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let size = libc::off_t::try_from(len).map_err(|_| Error::new(format!("a memory object of {len} bytes")))?;
        // SAFETY: `fd` is an open memfd.
        if unsafe { libc::ftruncate(fd.as_raw_fd(), size) } != 0 {
            return Err(os(&format!("sizing a memory object to {len:#x} bytes")));
        }
        // SAFETY: `fd` is an open memfd made with MFD_ALLOW_SEALING.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_SHRINK | libc::F_SEAL_GROW) } != 0 {
            return Err(os("sealing a memory object's size"));
        }
        Ok(Object { fd, len })
    }

    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    pub(crate) fn duplicate(&self) -> Result<Object, Error> {
        let fd = self.fd.try_clone().map_err(|it| Error::new(format!("duplicating a memory object: {it}")))?;
        Ok(Object { fd, len: self.len })
    }

    pub(crate) fn map_here(&self, access: Access) -> Result<View, Error> {
        View::map(self.fd.as_raw_fd(), self.len, access)
    }

    /// A descriptor of the object opened read-only: what maps it can't write it shared.
    fn read_only(&self) -> Result<OwnedFd, Error> {
        let path = CString::new(format!("/proc/self/fd/{}", self.fd.as_raw_fd())).map_err(|_| Error::new("a /proc path"))?;
        // SAFETY: `path` is a NUL-terminated string live for the call.
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(os("reopening a memory object read-only through /proc/self/fd"));
        }
        // SAFETY: `fd` was just opened and is owned by nothing else.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

/// A mapping in this process, unmapped when dropped.
pub(crate) struct View {
    at: *mut u8,
    len: usize,
    writable: bool,
}

// SAFETY: a `View` is an address range this process mapped; nothing of it is tied to a thread,
// and every write through it goes through `&mut self` or atomics.
unsafe impl Send for View {}
// SAFETY: as above: shared access is reads of bytes no `&self` method writes, and atomics.
unsafe impl Sync for View {}

impl Drop for View {
    fn drop(&mut self) {
        if self.len != 0 {
            // SAFETY: `at..at + len` is this view's own mapping, which nothing borrows once it
            // drops (every borrow is of `self`).
            unsafe { libc::munmap(self.at.cast(), self.len) };
        }
    }
}

impl View {
    /// The first `len` bytes of `fd`, wherever the kernel puts them.
    fn map(fd: RawFd, len: u64, access: Access) -> Result<View, Error> {
        let at = map_anywhere(fd, len, access)?;
        let writable = matches!(access, Access::Copy | Access::Shared);
        Ok(View { at: at as *mut u8, len: len as usize, writable })
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn address(&self) -> u64 {
        self.at as u64
    }

    pub(crate) fn bytes(&self, range: Range<usize>) -> Result<&[u8], Error> {
        if range.start > range.end || range.end > self.len {
            return Err(Error::new(format!("bytes {range:?} of a view of {:#x}", self.len)));
        }
        // SAFETY: `range` is within the mapping (checked above), which lives as long as `self`;
        // nothing writes it while `&self` is borrowed but atomics on other words and the
        // daemon's `&mut` methods, which can't run meanwhile.
        Ok(unsafe { std::slice::from_raw_parts(self.at.add(range.start), range.end - range.start) })
    }

    pub(crate) fn bytes_mut(&mut self, range: Range<usize>) -> Result<&mut [u8], Error> {
        if !self.writable {
            return Err(Error::new("a write through a read-only view"));
        }
        if range.start > range.end || range.end > self.len {
            return Err(Error::new(format!("bytes {range:?} of a view of {:#x}", self.len)));
        }
        // SAFETY: as for `bytes`, and `&mut self` makes this the only borrow; the mapping is
        // writable (checked above).
        Ok(unsafe { std::slice::from_raw_parts_mut(self.at.add(range.start), range.end - range.start) })
    }

    pub(crate) fn word(&self, at: usize) -> &AtomicU64 {
        assert!(at.is_multiple_of(8) && at + 8 <= self.len, "a word at {at:#x} of a view of {:#x}", self.len);
        // SAFETY: `at` is 8-aligned and in the mapping (asserted), page-aligned itself, so the
        // word is aligned; every access to the word, in this process and others, is atomic.
        unsafe { AtomicU64::from_ptr(self.at.add(at).cast()) }
    }

    pub(crate) fn word32(&self, at: usize) -> &AtomicU32 {
        assert!(at.is_multiple_of(4) && at + 4 <= self.len, "a word at {at:#x} of a view of {:#x}", self.len);
        // SAFETY: as for `word`, 4-aligned.
        unsafe { AtomicU32::from_ptr(self.at.add(at).cast()) }
    }

    pub(crate) fn publish(&self, range: Range<usize>) {
        if range.start >= range.end || range.end > self.len {
            return;
        }
        // SAFETY: `range` is within this view's mapping (checked above), mapped while `self` lives.
        unsafe { sync_icache(self.at as usize + range.start, self.at as usize + range.end) };
    }
}

/// Makes code written at `start..end` right for instruction fetch on every core: on AArch64
/// `dc cvau`, `dsb ish`, `ic ivau`, `dsb ish`, `isb` by the line sizes `CTR_EL0` gives (each
/// step skipped when `CTR_EL0.IDC` or `DIC` says it is not needed); on x86-64, which keeps
/// fetch coherent with stores, a fence.
///
/// # Safety
///
/// `start..end` is mapped readable: a maintenance operation on an unmapped line faults.
unsafe fn sync_icache(start: usize, end: usize) {
    #[cfg(target_arch = "aarch64")]
    {
        let ctr = ctr();
        let (idc, dic) = (ctr & (1 << 28) != 0, ctr & (1 << 29) != 0);
        let dline = 4usize << ((ctr >> 16) & 15);
        let iline = 4usize << (ctr & 15);
        if !idc {
            let mut at = start & !(dline - 1);
            while at < end {
                // SAFETY: `at` is in `start..end` rounded down to a line that starts in it,
                // mapped (the caller's contract); `dc cvau` cleans the line and changes no value.
                unsafe { std::arch::asm!("dc cvau, {}", in(reg) at, options(nostack, preserves_flags)) };
                at += dline;
            }
        }
        // SAFETY: a barrier: it orders the cleans before what follows; touches no memory.
        unsafe { std::arch::asm!("dsb ish", options(nostack, preserves_flags)) };
        if !dic {
            let mut at = start & !(iline - 1);
            while at < end {
                // SAFETY: as above; `ic ivau` invalidates instruction cache lines for the
                // physical memory of `at`, which every view of it shares.
                unsafe { std::arch::asm!("ic ivau, {}", in(reg) at, options(nostack, preserves_flags)) };
                at += iline;
            }
            // SAFETY: a barrier, as above.
            unsafe { std::arch::asm!("dsb ish", options(nostack, preserves_flags)) };
        }
        // SAFETY: a context synchronization of this core only.
        unsafe { std::arch::asm!("isb", options(nostack, preserves_flags)) };
    }
    #[cfg(target_arch = "x86_64")]
    {
        let _ = (start, end);
        std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
    }
}

/// Anonymous memory of this process: code, then data right after it, in one mapping.
pub(crate) struct Exec {
    at: *mut u8,
    /// The code's pages, then the data's.
    code_pages: usize,
    len: usize,
    data: usize,
}

// SAFETY: the code is never written once `Exec::new` returns, and the data only through
// `&mut self`; nothing of the mapping is tied to a thread.
unsafe impl Send for Exec {}
// SAFETY: as above: `&self` reads only addresses.
unsafe impl Sync for Exec {}

impl Drop for Exec {
    fn drop(&mut self) {
        // SAFETY: `at..at + len` is this `Exec`'s own mapping; every slice of it borrowed `self`
        // or ended with `Exec::new`'s `write`.
        unsafe { libc::munmap(self.at.cast(), self.len) };
    }
}

impl Exec {
    pub(crate) fn new(code: usize, data: usize, write: impl FnOnce(&mut [u8], &mut [u8])) -> Result<Exec, Error> {
        let page = page() as usize;
        let code_pages = code.max(1).next_multiple_of(page);
        let len = code_pages + data.next_multiple_of(page);
        // SAFETY: an anonymous mapping at no fixed address: nothing of this process is replaced.
        let at = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if at == libc::MAP_FAILED {
            return Err(os(&format!("mmap of {len:#x} bytes of code and data")));
        }
        let exec = Exec { at: at.cast(), code_pages, len, data };
        // SAFETY: the fresh mapping of `len` readable and writable bytes, which nothing else
        // refers to; `code <= code_pages` and `code_pages + data <= len`, so the slices are
        // disjoint and in it. `write` can't keep them past its return (they borrow for the call).
        let (code_bytes, data_bytes) = unsafe {
            (std::slice::from_raw_parts_mut(exec.at, code), std::slice::from_raw_parts_mut(exec.at.add(code_pages), data))
        };
        write(code_bytes, data_bytes);
        // SAFETY: the code's pages of the mapping above, which nothing writes from here on.
        if unsafe { libc::mprotect(exec.at.cast(), code_pages, libc::PROT_READ | libc::PROT_EXEC) } != 0 {
            return Err(os(&format!("mprotect of {code_pages:#x} bytes of code to read and execute")));
        }
        // SAFETY: the code's pages, mapped readable.
        unsafe { sync_icache(exec.at as usize, exec.at as usize + code) };
        Ok(exec)
    }

    pub(crate) fn code_address(&self) -> u64 {
        self.at as u64
    }

    pub(crate) fn data_address(&self) -> u64 {
        self.at as u64 + self.code_pages as u64
    }

    pub(crate) fn data(&mut self) -> &mut [u8] {
        // SAFETY: the data's `data` bytes after the code's pages, writable and in the mapping,
        // which lives as long as `self`; `&mut self` makes this the only borrow.
        unsafe { std::slice::from_raw_parts_mut(self.at.add(self.code_pages), self.data) }
    }
}

fn mapping(access: Access) -> c_int {
    if access == Access::Copy { libc::MAP_PRIVATE } else { libc::MAP_SHARED }
}

/// The first `len` bytes of `fd` mapped wherever the kernel finds room, for the rest of the
/// mapping's owner.
fn map_anywhere(fd: RawFd, len: u64, access: Access) -> Result<u64, Error> {
    let len_usize = usize::try_from(len).ok().filter(|it| *it != 0).ok_or_else(|| Error::new(format!("a map of {len:#x} bytes")))?;
    // SAFETY: `fd` is an open memory object; with no address the kernel picks free space, so
    // nothing of this process is replaced.
    let mapped = unsafe { libc::mmap(std::ptr::null_mut(), len_usize, protection(access), mapping(access), fd, 0) };
    if mapped == libc::MAP_FAILED {
        return Err(os(&format!("mmap of {len:#x} bytes")));
    }
    Ok(mapped as u64)
}

/// The daemon's side of a run process: a pidfd of it, which names that process even once its
/// ID is reused; the start hook maps what it is given itself.
pub(crate) struct Remote {
    process: OwnedFd,
}

impl Remote {
    pub(crate) fn of(pid: u32) -> Result<Remote, Error> {
        // SAFETY: pidfd_open takes a process ID and flags and returns a new descriptor or -1.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
        if fd < 0 {
            return Err(os(&format!("pidfd_open of process {pid}")));
        }
        // SAFETY: just made, owned by nothing else.
        Ok(Remote { process: unsafe { OwnedFd::from_raw_fd(fd as RawFd) } })
    }

    pub(crate) fn map(&self, object: u32, _: Access) -> Result<u64, Error> {
        Err(Error::new(format!("a start hook asked the daemon to map object {object}: on Linux the hook maps it")))
    }

    pub(crate) fn overlay(&self, _: u64, _: &crate::handshake::Grant) -> Result<(), Error> {
        Err(Error::new("a start hook asked the daemon to lay its overlays: on Linux the hook lays them"))
    }

    pub(crate) fn kill(&self) -> Result<(), Error> {
        // SAFETY: `process` is an open pidfd; no siginfo, no flags.
        let sent = unsafe {
            libc::syscall(libc::SYS_pidfd_send_signal, self.process.as_raw_fd(), libc::SIGKILL, std::ptr::null::<libc::siginfo_t>(), 0u32)
        };
        if sent != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
            return Err(os("pidfd_send_signal"));
        }
        Ok(())
    }
}

/// Sleeps on a futex that is not private, so a word of a shared mapping is the same futex in
/// every process that maps it.
pub(crate) fn wait(word: &AtomicU64, expected: u32, timeout: Duration) -> bool {
    let time = libc::timespec {
        tv_sec: timeout.as_secs().min(i32::MAX as u64) as libc::time_t,
        tv_nsec: timeout.subsec_nanos() as libc::c_long,
    };
    // SAFETY: `word` is live and 8-aligned, so its first four bytes are an aligned `u32`, the
    // low half on these little-endian targets, which the kernel reads atomically; `time` is
    // live for the call; FUTEX_WAIT reads nothing else.
    let got = unsafe {
        libc::syscall(libc::SYS_futex, word.as_ptr().cast::<u32>(), libc::FUTEX_WAIT, expected, &raw const time, 0usize, 0u32)
    };
    got == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ETIMEDOUT)
}

pub(crate) fn wake(word: &AtomicU64) {
    // SAFETY: as in `wait`; FUTEX_WAKE reads nothing of the word.
    unsafe { libc::syscall(libc::SYS_futex, word.as_ptr().cast::<u32>(), libc::FUTEX_WAKE, i32::MAX, 0usize, 0usize, 0u32) };
}

pub(crate) fn send_objects(channel: &mut Channel, pid: u32, grant: &[u8], objects: &[(&Object, Rights)]) -> Result<Remote, Error> {
    let remote = Remote::of(pid)?;
    let mut fds: Vec<OwnedFd> = Vec::with_capacity(objects.len());
    for (object, rights) in objects {
        fds.push(match rights {
            Rights::Code => object.read_only()?,
            Rights::Words => object.fd.try_clone().map_err(|it| Error::new(format!("duplicating a words object: {it}")))?,
        });
    }
    let mut bytes = Vec::with_capacity(frame::HEADER + grant.len());
    frame::encode(kinds::GRANT, 0, grant, &mut bytes).map_err(|it| Error::new(format!("the grant: {it}")))?;
    let raw: Vec<RawFd> = fds.iter().map(AsRawFd::as_raw_fd).collect();
    if raw.len() > super::unix::MOST_FDS {
        return Err(Error::new(format!(
            "{} objects to grant, past the {} one message carries",
            raw.len(),
            super::unix::MOST_FDS
        )));
    }
    channel.stream().send(&bytes, &raw)?;
    Ok(remote)
}

pub(crate) struct Granted {
    fd: OwnedFd,
    len: u64,
}

pub(crate) fn recv_objects(channel: &mut Channel, count: usize) -> Result<Vec<Granted>, Error> {
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let fd = channel
            .stream()
            .take_fd()
            .ok_or_else(|| Error::new(format!("the grant came with {index} descriptors of {count}")))?;
        // SAFETY: a zeroed `stat` is a valid one: plain integers.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `fd` is open; `stat` is live and writable.
        if unsafe { libc::fstat(fd.as_raw_fd(), &raw mut stat) } != 0 {
            return Err(os(&format!("granted object {index}")));
        }
        out.push(Granted { fd, len: stat.st_size as u64 });
    }
    if channel.stream().take_fd().is_some() {
        return Err(Error::new(format!("the grant came with more than {count} descriptors")));
    }
    Ok(out)
}

impl Granted {
    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    pub(crate) fn map_anywhere(&self, _: &mut Channel, access: Access) -> Result<u64, Error> {
        map_anywhere(self.fd.as_raw_fd(), self.len, access)
    }
}

/// A start hook's grant of `object` with `rights`, made in this process as the daemon makes it.
#[cfg(test)]
pub(crate) fn granted(object: &Object, rights: Rights) -> Result<Granted, Error> {
    let fd = match rights {
        Rights::Code => object.read_only()?,
        Rights::Words => object.fd.try_clone().map_err(|it| Error::new(format!("duplicating a words object: {it}")))?,
    };
    Ok(Granted { fd, len: object.len })
}

pub(crate) fn overlay(_: &mut Channel, objects: &[Granted], overlays: &[Overlay], slide: u64) -> Result<(), Error> {
    for overlay in overlays {
        let object = objects
            .get(overlay.object as usize)
            .ok_or_else(|| Error::new(format!("an overlay of object {} of {}", overlay.object, objects.len())))?;
        if overlay.offset.checked_add(overlay.len).is_none_or(|end| end > object.len) {
            return Err(Error::new(format!(
                "an overlay of {:#x} bytes at offset {:#x} of an object of {:#x}",
                overlay.len, overlay.offset, object.len
            )));
        }
        let at = overlay.at.wrapping_add(slide);
        let len = usize::try_from(overlay.len).map_err(|_| Error::new(format!("an overlay of {:#x} bytes", overlay.len)))?;
        let offset = libc::off_t::try_from(overlay.offset)
            .map_err(|_| Error::new(format!("an overlay at offset {:#x}", overlay.offset)))?;
        // SAFETY: `at..at + len` is, by `Grant::check` against the headers the system loader
        // mapped, either one of this executable's code segments, whose bytes the object holds
        // equal at `offset` (the file was written from it, and the daemon changes code a
        // thread can reach only by words a thread may see change), or inside a reservation,
        // which holds nothing. MAP_FIXED replaces those pages in one step, so a thread running
        // there, this one included, fetches the same instructions before and after.
        let mapped = unsafe {
            libc::mmap(
                at as *mut c_void,
                len,
                protection(overlay.access),
                mapping(overlay.access) | libc::MAP_FIXED,
                object.fd.as_raw_fd(),
                offset,
            )
        };
        if mapped == libc::MAP_FAILED {
            return Err(os(&format!("mmap of {len:#x} bytes of object {} over {at:#x}", overlay.object)));
        }
    }
    Ok(())
}

pub(crate) fn own_image() -> Result<(u64, &'static [u8]), Error> {
    /// The executable's first `PT_LOAD`'s address and its bytes from the file.
    unsafe extern "C" fn first(info: *mut libc::dl_phdr_info, _: usize, data: *mut c_void) -> c_int {
        // SAFETY: the dynamic linker passes its own record of an object, live for the call,
        // and `data` is the `Option` `own_image` passed, borrowed by nothing else meanwhile.
        let (info, found) = unsafe { (&*info, &mut *data.cast::<Option<(u64, u64)>>()) };
        for index in 0..usize::from(info.dlpi_phnum) {
            // SAFETY: `dlpi_phdr` points at `dlpi_phnum` program headers of the object, mapped.
            let header = unsafe { &*info.dlpi_phdr.add(index) };
            if header.p_type == libc::PT_LOAD && header.p_offset == 0 {
                *found = Some((info.dlpi_addr.wrapping_add(header.p_vaddr), header.p_filesz.min(header.p_memsz)));
                break;
            }
        }
        // The first object is the executable: stop.
        1
    }
    let mut found: Option<(u64, u64)> = None;
    // SAFETY: `first` is a callback of the signature asked, given `found`, live for the call.
    unsafe { libc::dl_iterate_phdr(Some(first), (&raw mut found).cast()) };
    let (header, mapped) =
        found.ok_or_else(|| Error::new("the executable has no PT_LOAD of file offset 0: its headers are not mapped"))?;
    let read = |len: u64| -> Result<&'static [u8], Error> {
        if len > mapped {
            return Err(Error::new(format!(
                "the executable's headers are {len:#x} bytes, past its first segment of {mapped:#x} bytes"
            )));
        }
        // SAFETY: `header..header + len` is inside the executable's first segment, mapped
        // readable from its file for the life of the process (checked above), and nothing
        // writes it.
        Ok(unsafe { std::slice::from_raw_parts(header as *const u8, len as usize) })
    };
    let prefix = read(64)?;
    let len = crate::image::headers_len(crate::Format::Elf, prefix)
        .ok_or_else(|| Error::new("the executable's ELF header is not one"))?;
    Ok((header, read(len)?))
}

pub(crate) fn clone_file(from: &Path, to: &Path) -> Result<bool, Error> {
    let fail = |path: &Path, it: std::io::Error| Error::new(format!("{}: {it}", path.display()));
    let source = std::fs::File::open(from).map_err(|it| fail(from, it))?;
    let target = std::fs::OpenOptions::new().write(true).create_new(true).open(to).map_err(|it| fail(to, it))?;
    // SAFETY: both descriptors are open, the target writable and empty; FICLONE only shares
    // the source's blocks into it.
    if unsafe { libc::ioctl(target.as_raw_fd(), libc::FICLONE, source.as_raw_fd()) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    drop(target);
    std::fs::remove_file(to).map_err(|it| fail(to, it))?;
    match error.raw_os_error() {
        Some(libc::EXDEV | libc::EOPNOTSUPP | libc::ENOTTY | libc::EINVAL | libc::ENOSYS) => Ok(false),
        _ => Err(Error::new(format!("cloning {} to {}: {error}", from.display(), to.display()))),
    }
}

pub(crate) fn threads(pid: u32, out: &mut Vec<u64>) -> Result<(), Error> {
    out.clear();
    let dir = format!("/proc/{pid}/task");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(Error::new(format!("{dir}: {error}"))),
    };
    for entry in entries {
        let entry = entry.map_err(|it| Error::new(format!("{dir}: {it}")))?;
        if let Some(tid) = entry.file_name().to_str().and_then(|it| it.parse::<u64>().ok()) {
            out.push(tid);
        }
    }
    Ok(())
}

pub(crate) fn service() -> Option<String> {
    None
}

pub(crate) fn thread_id() -> u64 {
    // SAFETY: gettid has no preconditions.
    unsafe { libc::syscall(libc::SYS_gettid) as u64 }
}
