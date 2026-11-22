//! The raw calls blaze-core makes outside its drivers' own modules, each behind a safe function
//! that safe code cannot misuse: one system call (or one view of bytes) per function, its
//! arguments borrowed for exactly as long as the call reads or writes them, its result checked.
//! The drivers (`uring`, `kqueue`, `iocp`, `threads`, `net`, `watch`, `fanotify`, `fsevents`,
//! `process::spawn`) and `fs::mapped` keep their own unsafe; everything else in the crate is
//! safe Rust (`#![deny(unsafe_code)]` at the root).
//!
//! [`Aligned::bytes`] is 100% safe and zero-copy via [`crate::pod::bytes_of_slice`]. Each raw
//! syscall helper has no safe std equivalent that makes one system call (`File::set_times` opens
//! the file first, `std::process::Child` reaps without the `rusage`, std has no `poll`, `waitid`
//! or `clonefile`).

/// Sixteen bytes on a sixteen-byte boundary: an image's records need their buffer that aligned
/// (`blaze_format::ALIGN`), as a mapped file's page is.
#[derive(Clone, Copy)]
#[repr(C, align(16))]
struct Block([u8; 16]);

const _: () = assert!(size_of::<Block>() == 16 && align_of::<Block>() == 16);
blaze_own::impl_pod!(Block(bytes: [u8; 16]));

/// Bytes copied into blocks of sixteen-byte alignment: a [`crate::Mapping`] made from bytes.
/// Its fields are private, so `len` is at most the blocks' bytes whatever safe code does.
pub(crate) struct Aligned {
    blocks: Box<[Block]>,
    len: usize,
}

impl Aligned {
    /// `bytes` copied, starting on a sixteen-byte boundary.
    pub(crate) fn copy(bytes: &[u8]) -> Aligned {
        // alloc: the copy, once per mapping made from bytes
        let mut blocks = vec![Block([0; 16]); bytes.len().div_ceil(16)].into_boxed_slice();
        for (block, chunk) in blocks.iter_mut().zip(bytes.chunks(16)) {
            block.0[..chunk.len()].copy_from_slice(chunk);
        }
        Aligned { blocks, len: bytes.len() }
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        debug_assert!(self.len <= self.blocks.len() * 16);
        &crate::pod::bytes_of_slice(&self.blocks)[..self.len]
    }
}

#[cfg(unix)]
pub(crate) use unix::*;

#[cfg(unix)]
mod unix {
    use blaze_sys::{
        Fd,
        posix::{io as sys_io, proc, signal, types::{nfds_t, pid_t, uid_t}},
    };
    use core::ffi::c_int;
    use std::{
        ffi::CStr,
        io,
        os::fd::{AsRawFd as _, BorrowedFd, FromRawFd as _, OwnedFd},
    };

    /// `n` of a call that returns -1 and sets `errno` on failure.
    #[cfg(target_os = "macos")]
    fn checked(n: c_int) -> io::Result<c_int> {
        match n {
            -1 => Err(io::Error::last_os_error()),
            n => Ok(n),
        }
    }

    /// The user the process runs as, for what it trusts as its own.
    pub(crate) fn euid() -> uid_t {
        blaze_sys::posix::thread::geteuid()
    }

    /// `SIGKILL` to `pid`. Memory-safe for any pid; the caller names a child it has not reaped,
    /// so that the pid is no one else's.
    pub(crate) fn kill(pid: pid_t) {
        let _ = signal::kill(pid, signal::SIGKILL);
    }

    /// Linux waits for a child on its wait ring (`blaze_core::wait::reap`).
    #[cfg(not(target_os = "linux"))]
    /// One `wait4` of `pid`, blocking until it exits: its status and what it used. `EINTR` is
    /// the caller's to retry.
    pub(crate) fn wait4(pid: pid_t) -> io::Result<(c_int, libc::rusage)> {
        let mut status = 0;
        // SAFETY: `rusage` is a C struct of integers, for which all-zero bytes is a valid value.
        let mut rusage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: `status` and `rusage` are live locals the call writes and does not keep; a
        // pid that is not a child fails with `ECHILD`, touching neither.
        let reaped = unsafe { proc::wait4(pid, &mut status, 0, (&raw mut rusage).cast()) };
        match reaped == pid {
            true => Ok((status, rusage)),
            false => Err(io::Error::last_os_error()),
        }
    }

    /// Linux waits for a child on its wait ring (`blaze_core::wait::exited`).
    #[cfg(not(target_os = "linux"))]
    /// One `waitid(WEXITED | WNOWAIT)` of `pid`: returns once it has exited, leaving it
    /// unreaped. `EINTR` is the caller's to retry.
    pub(crate) fn wait_exited(pid: pid_t) -> io::Result<()> {
        proc::wait_id(pid, proc::WaitFlags::EXITED | proc::WaitFlags::NO_WAIT)
            .map(drop)
            .map_err(|error| io::Error::from_raw_os_error(i32::from(error.raw())))
    }

    /// One `waitid(WEXITED | WNOHANG | WNOWAIT)` of `pid`: whether it has exited, leaving it
    /// unreaped either way. `ECHILD` (no child `pid`: reaped already) is the caller's to read.
    pub(crate) fn has_exited(pid: pid_t) -> io::Result<bool> {
        let options = proc::WaitFlags::EXITED | proc::WaitFlags::NO_HANG | proc::WaitFlags::NO_WAIT;
        proc::wait_id(pid, options)
            .map(|child| child.is_some())
            .map_err(|error| io::Error::from_raw_os_error(i32::from(error.raw())))
    }

    /// `O_NONBLOCK` added to `fd`'s status flags.
    pub(crate) fn set_nonblocking(fd: BorrowedFd<'_>) -> io::Result<()> {
        let fd = Fd::new(fd.as_raw_fd());
        let flags = sys_io::status_flags(fd)
            .map_err(|error| io::Error::from_raw_os_error(i32::from(error.raw())))?;
        sys_io::set_status_flags(fd, flags | sys_io::OpenFlags::NONBLOCK)
            .map_err(|error| io::Error::from_raw_os_error(i32::from(error.raw())))
    }

    /// Waits for one of `fds` to be ready, up to `ms` milliseconds from now (negative: no
    /// limit; 0: a look that does not wait): how many are ready, 0 when the time ran out.
    /// Memory-safe for any descriptor numbers (a closed one reports `POLLNVAL`). A wait goes
    /// through `crate::wait::readable` on a deadline made here, so an interrupted wait goes on
    /// for what is left of `ms`, never anew; a caller that waits again in a loop holds its own
    /// `Deadline` and calls `wait::readable` itself.
    pub(crate) fn poll(fds: &mut [libc::pollfd], ms: c_int) -> io::Result<usize> {
        use crate::wait::{self, Clock as _, Deadline, Monotonic, Waited};
        if ms == 0 {
            return ready_now(fds);
        }
        let clock = Monotonic::new();
        let deadline = u64::try_from(ms)
            .ok()
            .map(|ms| Deadline::after(clock.now(), std::time::Duration::from_millis(ms)));
        match wait::readable("the descriptors of a blaze-core poll", fds, deadline, &clock) {
            Ok(Waited::Ready(ready)) => Ok(ready),
            Ok(Waited::Deadline) => Ok(0),
            Err(error) => Err(io::Error::other(error)),
        }
    }

    /// Which of `fds` are ready now: one `poll` with a timeout of 0, which never blocks.
    fn ready_now(fds: &mut [libc::pollfd]) -> io::Result<usize> {
        nfds_t::try_from(fds.len())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: both types are repr(C) pollfd with the same int, short, short fields.
        // The cast keeps the exclusive borrow and length, including room to write revents.
        let fds = unsafe {
            std::slice::from_raw_parts_mut(fds.as_mut_ptr().cast::<sys_io::PollFd>(), fds.len())
        };
        sys_io::poll(fds, Some(blaze_sys::Timespec::ZERO))
            .map_err(|error| io::Error::from_raw_os_error(i32::from(error.raw())))
    }

    /// One `read` of `fd` into `into`: how many bytes, 0 at its end.
    pub(crate) fn read(fd: BorrowedFd<'_>, into: &mut [u8]) -> io::Result<usize> {
        sys_io::read(Fd::new(fd.as_raw_fd()), into)
            .map_err(|error| io::Error::from_raw_os_error(i32::from(error.raw())))
    }

    /// One `preadv` of `fd` at `at` into `into`, in order: how many bytes, fewer at the file's
    /// end. A run of adjacent ranges of one file is one call.
    pub(crate) fn preadv(
        fd: BorrowedFd<'_>,
        into: &mut [io::IoSliceMut<'_>],
        at: u64,
    ) -> io::Result<usize> {
        core::ffi::c_int::try_from(into.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "a preadv of more buffers than an int counts")
        })?;
        blaze_sys::posix::types::off_t::try_from(at).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "a preadv at an offset past off_t")
        })?;
        // SAFETY: both IoSliceMut types have the Unix iovec ABI. The exclusive borrow covers
        // every descriptor and its writable buffer for this call, which retains neither.
        let slices =
            unsafe { std::slice::from_raw_parts_mut(into.as_mut_ptr().cast(), into.len()) };
        blaze_sys::posix::io::preadv(blaze_sys::Fd::new(fd.as_raw_fd()), slices, at)
            .map_err(|e| io::Error::from_raw_os_error(i32::from(e.raw())))
    }

    /// The process's soft limit of open descriptors (`RLIMIT_NOFILE`), `u64::MAX` where it
    /// has none.
    pub(crate) fn nofile() -> io::Result<u64> {
        use blaze_sys::posix::thread;
        let limit = thread::resource_limit(thread::RLIMIT_NOFILE)
            .map_err(|e| io::Error::from_raw_os_error(i32::from(e.raw())))?;
        Ok(match limit.rlim_cur == thread::RLIM_INFINITY {
            true => u64::MAX,
            false => limit.rlim_cur,
        })
    }

    /// One `write` of `bytes` to `fd`: how many it took.
    pub(crate) fn write(fd: BorrowedFd<'_>, bytes: &[u8]) -> io::Result<usize> {
        blaze_sys::posix::io::write(blaze_sys::Fd::new(fd.as_raw_fd()), bytes)
            .map_err(|e| io::Error::from_raw_os_error(i32::from(e.raw())))
    }

    /// Reinterprets one C ABI mirror struct `A` as its identical-layout counterpart `B` without
    /// `core::mem::transmute`, proving identical size and alignment at compile time.
    ///
    /// # Safety
    /// `A` and `B` must be ABI-identical C layout mirrors whose bit representation of `src` is a
    /// valid initialized `B`.
    #[inline]
    unsafe fn abi_mirror<A: Copy, B: Copy>(src: A) -> B {
        const {
            assert!(size_of::<A>() == size_of::<B>(), "C ABI mirror size mismatch");
            assert!(align_of::<A>() == align_of::<B>(), "C ABI mirror align mismatch");
        }
        // SAFETY: `A` and `B` have identical compile-time-verified size & alignment and caller
        // guarantees `src` is a valid initialized bit representation of `B`.
        unsafe { core::ptr::read((&raw const src).cast::<B>()) }
    }

    /// The `timespec` of `seconds` and `nanos` (below 10^9).
    pub(crate) fn timespec(seconds: blaze_sys::posix::types::time_t, nanos: u32) -> libc::timespec {
        debug_assert!(nanos < 1_000_000_000);
        libc::timespec { tv_sec: seconds, tv_nsec: i64::from(nanos) }
    }

    /// The access and modification times of `path` (relative to the working directory, a link
    /// followed) set to `times`, in that order: one `utimensat`.
    pub(crate) fn utimens(path: &CStr, times: &[libc::timespec; 2]) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            use blaze_sys::linux::{AtFlags, CWD, FileTime};
            let [access, modified] =
                times.map(|t| FileTime::At(blaze_sys::Timespec { sec: t.tv_sec, nsec: t.tv_nsec }));
            blaze_sys::linux::utimensat(
                CWD, blaze_sys::Path::new(path), access, modified, AtFlags::NONE,
            )
            .map_err(|e| io::Error::from_raw_os_error(i32::from(e.raw())))
        }
        #[cfg(target_os = "macos")]
        {
            // SAFETY: the path and both C timespecs are live and readable for the call.
            checked(unsafe {
                blaze_sys::posix::proc::utimensat(
                    blaze_sys::posix::thread::AT_FDCWD, path.as_ptr(), times.as_ptr().cast(), 0,
                )
            }).map(drop)
        }
    }

    /// The directory at `path` opened read-only, close-on-exec, as a descriptor of its own.
    pub(crate) fn open_dir(path: &CStr) -> io::Result<OwnedFd> {
        use blaze_sys::posix::io::OpenFlags;
        let flags = OpenFlags::READ | OpenFlags::DIRECTORY | OpenFlags::CLOSE_ON_EXEC;
        let fd = blaze_sys::posix::io::open(blaze_sys::Path::new(path), flags, 0)
            .map_err(|e| io::Error::from_raw_os_error(i32::from(e.raw())))?;
        // SAFETY: into_raw transfers sole ownership of the newly opened directory.
        Ok(unsafe { OwnedFd::from_raw_fd(fd.into_raw()) })
    }

    /// The `stat` of `name` relative to the directory `dir`, not followed through a symlink.
    #[inline]
    pub(crate) fn stat_at(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<libc::stat> {
        #[cfg(target_os = "linux")]
        let flags = blaze_sys::linux::AtFlags::NO_FOLLOW.bits() as i32;
        #[cfg(target_os = "macos")]
        let flags = blaze_sys::posix::thread::AT_SYMLINK_NOFOLLOW;
        let stat = blaze_sys::posix::proc::stat_at(
            blaze_sys::Fd::new(dir.as_raw_fd()), blaze_sys::Path::new(name), flags,
        )
        .map_err(|e| io::Error::from_raw_os_error(i32::from(e.raw())))?;
        // SAFETY: both stat types have the target's C stat layout and integer fields.
        Ok(unsafe { abi_mirror(stat) })
    }

    /// `target`, a new empty file, made to share `source`'s blocks: `FICLONE`.
    #[cfg(target_os = "linux")]
    pub(crate) fn ficlone(target: BorrowedFd<'_>, source: BorrowedFd<'_>) -> io::Result<()> {
        blaze_sys::linux::ficlone(
            blaze_sys::Fd::new(target.as_raw_fd()), blaze_sys::Fd::new(source.as_raw_fd()),
        )
        .map_err(|e| io::Error::from_raw_os_error(i32::from(e.raw())))
    }

    /// The `statfs` of the file system that holds `path`.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn statfs(path: &CStr) -> io::Result<libc::statfs> {
        #[cfg(target_os = "linux")]
        let stat = blaze_sys::linux::task::statfs(blaze_sys::Path::new(path))
            .map_err(|e| io::Error::from_raw_os_error(i32::from(e.raw())))?;
        #[cfg(target_os = "macos")]
        let stat = {
            let mut stat = blaze_sys::posix::proc::statfs::zeroed();
            // SAFETY: path is NUL-terminated and stat is writable for one C statfs.
            checked(unsafe { blaze_sys::posix::proc::statfs(path.as_ptr(), (&raw mut stat).cast()) })?;
            stat
        };
        // SAFETY: both statfs types represent the target's C statfs, with the same fields and layout.
        Ok(unsafe { abi_mirror(stat) })
    }

    /// `to` made a clone of the open file `from` (`fclonefileat`, no flags).
    #[cfg(target_os = "macos")]
    pub(crate) fn fclonefileat(from: BorrowedFd<'_>, to: &CStr) -> io::Result<()> {
        // SAFETY: a descriptor borrowed open for the call and a NUL-terminated path borrowed
        // across it, which keeps neither.
        checked(unsafe {
            blaze_sys::posix::proc::fclonefileat(
                from.as_raw_fd(), blaze_sys::posix::thread::AT_FDCWD, to.as_ptr(), 0,
            )
        })
        .map(drop)
    }

    /// `path`'s permission bits set to `mode`, a symbolic link not followed (`fchmodat` with
    /// `AT_SYMLINK_NOFOLLOW`, which sets a link's own bits). macOS only: glibc makes this call
    /// `fchmodat2`, which some seccomp filters refuse.
    #[cfg(target_os = "macos")]
    pub(crate) fn chmod_no_follow(path: &CStr, mode: u32) -> io::Result<()> {
        // SAFETY: a NUL-terminated path borrowed across the call.
        checked(unsafe {
            blaze_sys::posix::proc::fchmodat(
                blaze_sys::posix::thread::AT_FDCWD, path.as_ptr(), mode as blaze_sys::posix::types::mode_t,
                blaze_sys::posix::thread::AT_SYMLINK_NOFOLLOW,
            )
        })
        .map(drop)
    }

    /// The access and modification times of `path` set to now, a symbolic link not followed
    /// (`utimensat` with no times).
    #[cfg(target_os = "macos")]
    pub(crate) fn touch(path: &CStr) -> io::Result<()> {
        // SAFETY: a NUL-terminated path borrowed across the call; a null `times` means now.
        checked(unsafe {
            blaze_sys::posix::proc::utimensat(
                blaze_sys::posix::thread::AT_FDCWD, path.as_ptr(), std::ptr::null(),
                blaze_sys::posix::thread::AT_SYMLINK_NOFOLLOW,
            )
        })
        .map(drop)
    }

    /// One `getattrlistbulk` of the open directory `dir` (rule macos-private-apis;
    /// docs/macos.md, "File operations"): the next entries of it packed into `into`, each with
    /// the attributes `common` names, a directory's data length and a file's, as many as fit;
    /// how many, 0 once none is left. `getattrlistbulk(2)`, in libSystem from macOS 10.10: it
    /// wins over `readdir` and an `fstatat` per name by being one system call for a buffer of
    /// entries where those are one per name. blaze-sys resolves the libSystem symbol on its
    /// first call. The test below checks the symbol at run time.
    #[cfg(target_os = "macos")]
    pub(crate) fn attrs_bulk(
        dir: BorrowedFd<'_>,
        common: u32,
        into: &mut [u8],
    ) -> io::Result<usize> {
        use blaze_sys::posix::proc;
        let mut list = proc::attrlist {
            bitmapcount: proc::ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: common,
            volattr: 0,
            dirattr: proc::ATTR_DIR_DATALENGTH,
            fileattr: proc::ATTR_FILE_DATALENGTH,
            forkattr: 0,
        };
        // SAFETY: `dir` is borrowed open, `list` a live `attrlist` the call reads, and `into`
        // room for `into.len()` bytes the call writes and does not keep. No option is set:
        // an entry holds only the attributes its returned set names.
        let count = checked(unsafe {
            proc::getattrlistbulk(
                dir.as_raw_fd(), (&raw mut list).cast(), into.as_mut_ptr().cast(), into.len(), 0,
            )
        })?;
        Ok(count as usize)
    }

    /// `F_NOCACHE` on `fd`: its reads and writes stay out of the unified buffer cache.
    #[cfg(target_os = "macos")]
    pub(crate) fn no_cache(fd: BorrowedFd<'_>) -> io::Result<()> {
        blaze_sys::posix::io::set_no_cache(blaze_sys::Fd::new(fd.as_raw_fd()), true)
            .map_err(|e| io::Error::from_raw_os_error(i32::from(e.raw())))
    }

    /// `f` run with the calling thread's disk IO at idle priority, which is put back after: a
    /// read of [`crate::Priority::Idle`]. Linux: `ioprio_set` of the thread to
    /// `IOPRIO_CLASS_IDLE` (`who` 0 is the calling thread).
    #[cfg(target_os = "linux")]
    pub(crate) fn idle_io<R>(f: impl FnOnce() -> io::Result<R>) -> io::Result<R> {
        const IOPRIO_WHO_PROCESS: core::ffi::c_long = 1;
        const IOPRIO_CLASS_IDLE: core::ffi::c_long = 3;
        const IOPRIO_CLASS_SHIFT: core::ffi::c_long = 13;
        // SAFETY: no pointers; asks the calling thread's IO priority.
        let was = unsafe { libc::syscall(libc::SYS_ioprio_get, IOPRIO_WHO_PROCESS, 0) };
        if was < 0 {
            return Err(io::Error::last_os_error());
        }
        let idle = IOPRIO_CLASS_IDLE << IOPRIO_CLASS_SHIFT;
        // SAFETY: no pointers; sets the calling thread's IO priority.
        if unsafe { libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, idle) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let result = f();
        // SAFETY: as above, back to what it was.
        unsafe { libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, was) };
        result
    }

    /// `f` run with the calling thread's disk IO throttled, its policy put back after: a read of
    /// [`crate::Priority::Idle`]. macOS: `setiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_THREAD,
    /// IOPOL_THROTTLE)` (`<sys/resource.h>`, public), what the store's prefetch threads read with.
    #[cfg(target_os = "macos")]
    pub(crate) fn idle_io<R>(f: impl FnOnce() -> io::Result<R>) -> io::Result<R> {
        use blaze_sys::macos::cold::{getiopolicy_np, setiopolicy_np};
        use core::ffi::c_int;
        const IOPOL_TYPE_DISK: c_int = 0;
        const IOPOL_SCOPE_THREAD: c_int = 1;
        const IOPOL_THROTTLE: c_int = 3;
        // SAFETY: no pointers; asks the calling thread's disk policy.
        let was = unsafe { getiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_THREAD) };
        if was < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: no pointers; sets the calling thread's disk policy.
        checked(unsafe { setiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_THREAD, IOPOL_THROTTLE) })?;
        let result = f();
        // SAFETY: as above, back to what it was.
        unsafe { setiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_THREAD, was) };
        result
    }

    /// A new eventfd, count 0, close-on-exec and nonblocking.
    #[cfg(target_os = "linux")]
    pub(crate) fn eventfd() -> io::Result<OwnedFd> {
        use blaze_sys::linux::{EventFlags, eventfd2};
        let fd = eventfd2(0, EventFlags::CLOSE_ON_EXEC | EventFlags::NONBLOCK)
            .map_err(|e| io::Error::from_raw_os_error(i32::from(e.raw())))?;
        // SAFETY: into_raw transfers sole ownership of the new descriptor.
        Ok(unsafe { OwnedFd::from_raw_fd(fd.into_raw()) })
    }

    /// A pipe, its read end first, both ends close-on-exec: on Linux at once (`pipe2`), on the
    /// other Unixes, which have no `pipe2` everywhere, by `FD_CLOEXEC` straight after (a spawn
    /// of another thread's in between could inherit them; blaze's spawns close what they are
    /// not given).
    pub(crate) fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
        #[cfg(target_os = "linux")]
        let ends = blaze_sys::linux::pipe2(blaze_sys::linux::PipeFlags::CLOSE_ON_EXEC);
        #[cfg(not(target_os = "linux"))]
        let ends = blaze_sys::posix::io::pipe();
        let (read, write) = ends.map_err(|e| io::Error::from_raw_os_error(i32::from(e.raw())))?;
        #[cfg(not(target_os = "linux"))]
        for fd in [&read, &write] {
            blaze_sys::posix::io::set_close_on_exec(fd.fd(), true)
                .map_err(|e| io::Error::from_raw_os_error(i32::from(e.raw())))?;
        }
        // SAFETY: each into_raw transfers sole ownership of one open pipe end.
        Ok(unsafe { (OwnedFd::from_raw_fd(read.into_raw()), OwnedFd::from_raw_fd(write.into_raw())) })
    }

    /// The running kernel's `uname`.
    #[cfg(target_os = "linux")]
    pub(crate) fn uname() -> io::Result<libc::utsname> {
        let name = blaze_sys::linux::host::uname()
            .map_err(|e| io::Error::from_raw_os_error(i32::from(e.raw())))?;
        // SAFETY: both Linux utsname structures are the kernel's six consecutive 65-byte
        // character arrays. All bytes are initialized and valid in either signedness.
        Ok(unsafe { abi_mirror(name) })
    }

    /// `pid`'s physical footprint now and the most it has been: `proc_pid_rusage`'s
    /// `RUSAGE_INFO_V4`.
    #[cfg(target_os = "macos")]
    pub(crate) fn footprint(pid: blaze_sys::posix::types::pid_t) -> io::Result<(u64, u64)> {
        use blaze_sys::posix::thread::{RUSAGE_INFO_V4, proc_pid_rusage, rusage_info_v4};
        let mut info = rusage_info_v4 { uuid: [0; 16], words: [0; 35] };
        // SAFETY: info is writable for the full RUSAGE_INFO_V4 structure.
        let read = unsafe { proc_pid_rusage(pid, RUSAGE_INFO_V4, (&raw mut info).cast()) };
        checked(read)?;
        Ok((info.words[rusage_info_v4::FOOTPRINT], info.words[rusage_info_v4::MOST_FOOTPRINT]))
    }

    /// This process's physical footprint now and at most ([`footprint`] of itself).
    #[cfg(target_os = "macos")]
    pub(crate) fn own_footprint() -> io::Result<(u64, u64)> {
        footprint(blaze_sys::posix::thread::getpid())
    }

    /// This process's resident set now (`/proc/self/statm`'s second field, in pages, read into
    /// a buffer on the stack) and the most it has been (`getrusage`'s `ru_maxrss`, in KiB on
    /// Linux).
    #[cfg(target_os = "linux")]
    pub(crate) fn own_footprint() -> io::Result<(u64, u64)> {
        use std::io::Read as _;
        let invalid = || io::Error::from(io::ErrorKind::InvalidData);
        let mut bytes = [0u8; 256];
        let read = std::fs::File::open("/proc/self/statm")?.read(&mut bytes)?;
        let text = core::str::from_utf8(&bytes[..read]).map_err(|_| invalid())?;
        let pages: u64 = text.split_ascii_whitespace().nth(1).and_then(|n| n.parse().ok()).ok_or_else(invalid)?;
        // SAFETY: no arguments and no memory; -1 is its failure.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let page = u64::try_from(page).map_err(|_| io::Error::last_os_error())?;
        let usage = blaze_sys::linux::getrusage(blaze_sys::linux::Who::Process)
            .map_err(|e| io::Error::from_raw_os_error(i32::from(e.raw())))?;
        let most = u64::try_from(usage.max_rss_kb).unwrap_or(0).saturating_mul(1024);
        let now = pages.saturating_mul(page);
        Ok((now, most.max(now)))
    }
}

#[cfg(test)]
mod tests {
    use blaze_sim::Config;

    use super::Aligned;

    /// The aligned view against the safe reference, the bytes themselves.
    #[test]
    fn aligned_bytes_are_the_bytes_copied() {
        blaze_sim::check(Config::DEFAULT, |tape| {
            let bytes: Vec<u8> = tape.bytes(0..=299, "bytes");
            let aligned = Aligned::copy(&bytes);
            assert_eq!(aligned.bytes(), &bytes[..]);
            assert_eq!(aligned.bytes().as_ptr() as usize % 16, 0);
        })
    }

    #[cfg(unix)]
    #[test]
    fn a_pipe_reads_back_what_was_written_and_polls_ready() {
        use std::os::fd::AsFd as _;
        let (reader, writer) = std::io::pipe().unwrap();
        assert_eq!(super::write(writer.as_fd(), b"blaze").unwrap(), 5);
        let mut fds = [libc::pollfd {
            fd: std::os::fd::AsRawFd::as_raw_fd(&reader),
            events: blaze_sys::posix::io::PollEvents::IN.bits(),
            revents: 0,
        }];
        assert_eq!(super::poll(&mut fds, 0).unwrap(), 1);
        let mut into = [0; 16];
        assert_eq!(super::read(reader.as_fd(), &mut into).unwrap(), 5);
        assert_eq!(&into[..5], b"blaze");
        drop(writer);
        assert_eq!(super::read(reader.as_fd(), &mut into).unwrap(), 0);
    }

    /// `getattrlistbulk`, which `Effect::Entries` lists a directory with on macOS, resolves
    /// at run time (rule macos-private-apis): a system that drops it fails here, naming it.
    #[cfg(target_os = "macos")]
    #[test]
    fn getattrlistbulk_is_in_libsystem() {
        use blaze_sys::posix::thread::{RTLD_DEFAULT, symbol};
        // SAFETY: RTLD_DEFAULT is the documented handle for a lookup in every loaded image.
        let found = unsafe { symbol(RTLD_DEFAULT, c"getattrlistbulk") };
        assert!(
            found.is_some(),
            "getattrlistbulk is gone from this macOS (it is there from 10.10): \
             Effect::Entries needs it, and there is no other listing"
        );
    }

    /// One call fills the buffer with the root's first entries, and says how many.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_bulk_call_packs_entries_of_the_root() {
        use std::os::fd::AsFd as _;
        let dir = super::open_dir(c"/").unwrap();
        let mut into = vec![0_u8; 8 << 10];
        use blaze_sys::posix::proc::{ATTR_CMN_NAME, ATTR_CMN_RETURNED_ATTRS};
        let common = ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_NAME;
        let count = super::attrs_bulk(dir.as_fd(), common, &mut into).unwrap();
        assert!(count > 0, "the root lists nothing");
        let first = u32::from_ne_bytes(into[..4].try_into().unwrap()) as usize;
        assert!((24..=into.len()).contains(&first), "the first entry is {first} bytes");
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_name_stats_as_not_found() {
        use std::os::fd::AsFd as _;
        let dir = super::open_dir(c"/").unwrap();
        assert!(super::stat_at(dir.as_fd(), c"tmp").is_ok());
        let missing = super::stat_at(dir.as_fd(), c"blaze-no-such-name-at-the-root").unwrap_err();
        assert_eq!(missing.raw_os_error(), Some(i32::from(blaze_sys::Errno::NoEnt.raw())));
    }
}
