//! The raw calls blaze-core makes outside its drivers' own modules, each behind a safe function
//! that safe code cannot misuse: one system call (or one view of bytes) per function, its
//! arguments borrowed for exactly as long as the call reads or writes them, its result checked.
//! The drivers (`uring`, `kqueue`, `iocp`, `threads`, `net`, `watch`, `fanotify`, `fsevents`,
//! `process::spawn`) and `fs::mapped` keep their own unsafe; everything else in the crate is
//! safe Rust (`#![deny(unsafe_code)]` at the root).
//!
//! What the safe versions would cost: [`Aligned::bytes`] would copy the bytes on every read (std
//! has no safe byte view of a slice of aligned blocks); each call has no safe std equivalent
//! that makes one system call (`File::set_times` opens the file first, `std::process::Child`
//! reaps without the `rusage`, std has no `poll`, `waitid` or `clonefile`).

/// Sixteen bytes on a sixteen-byte boundary: an image's records need their buffer that aligned
/// (`blaze_format::ALIGN`), as a mapped file's page is.
#[derive(Clone, Copy)]
#[repr(C, align(16))]
struct Block([u8; 16]);

const _: () = assert!(size_of::<Block>() == 16 && align_of::<Block>() == 16);

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
        // SAFETY: `Block` is sixteen `u8`s with no padding (`repr(C)`, asserted above), so
        // `blocks` is `16 * blocks.len()` initialized bytes, and `copy`, the one maker, set `len`
        // to at most that; the slice borrows `self`, so the box outlives it. A longer `len` would
        // read past the allocation.
        unsafe { std::slice::from_raw_parts(self.blocks.as_ptr().cast::<u8>(), self.len) }
    }
}

#[cfg(unix)]
pub(crate) use unix::*;

#[cfg(unix)]
mod unix {
    use std::{
        ffi::CStr,
        io,
        os::fd::{AsRawFd as _, BorrowedFd, FromRawFd as _, OwnedFd},
    };

    /// `n` of a call that returns -1 and sets `errno` on failure.
    fn checked(n: libc::c_int) -> io::Result<libc::c_int> {
        match n {
            -1 => Err(io::Error::last_os_error()),
            n => Ok(n),
        }
    }

    /// `n` of a call that returns a `ssize_t`, -1 on failure.
    fn checked_size(n: libc::ssize_t) -> io::Result<usize> {
        usize::try_from(n).map_err(|_| io::Error::last_os_error())
    }

    /// The user the process runs as, for what it trusts as its own.
    pub(crate) fn euid() -> libc::uid_t {
        // SAFETY: no arguments and no memory; `geteuid` always succeeds.
        unsafe { libc::geteuid() }
    }

    /// `SIGKILL` to `pid`. Memory-safe for any pid; the caller names a child it has not reaped,
    /// so that the pid is no one else's.
    pub(crate) fn kill(pid: libc::pid_t) {
        // SAFETY: no pointers; signalling a pid touches no memory of this process.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }

    /// Linux waits for a child on its wait ring (`blaze_core::wait::reap`).
    #[cfg(not(target_os = "linux"))]
    /// One `wait4` of `pid`, blocking until it exits: its status and what it used. `EINTR` is
    /// the caller's to retry.
    pub(crate) fn wait4(pid: libc::pid_t) -> io::Result<(libc::c_int, libc::rusage)> {
        let mut status = 0;
        // SAFETY: `rusage` is a C struct of integers, for which all-zero bytes is a valid value.
        let mut rusage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: `status` and `rusage` are live locals the call writes and does not keep; a
        // pid that is not a child fails with `ECHILD`, touching neither.
        let reaped = unsafe { libc::wait4(pid, &mut status, 0, &mut rusage) };
        match reaped == pid {
            true => Ok((status, rusage)),
            false => Err(io::Error::last_os_error()),
        }
    }

    /// Linux waits for a child on its wait ring (`blaze_core::wait::exited`).
    #[cfg(not(target_os = "linux"))]
    /// One `waitid(WEXITED | WNOWAIT)` of `pid`: returns once it has exited, leaving it
    /// unreaped. `EINTR` is the caller's to retry.
    pub(crate) fn wait_exited(pid: libc::pid_t) -> io::Result<()> {
        // SAFETY: `siginfo_t` is a C struct of integers and unions of them, for which all-zero
        // bytes is a valid value.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a live local the call writes and does not keep.
        let waited = unsafe {
            libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, libc::WEXITED | libc::WNOWAIT)
        };
        checked(waited).map(drop)
    }

    /// One `waitid(WEXITED | WNOHANG | WNOWAIT)` of `pid`: whether it has exited, leaving it
    /// unreaped either way. `ECHILD` (no child `pid`: reaped already) is the caller's to read.
    pub(crate) fn has_exited(pid: libc::pid_t) -> io::Result<bool> {
        // SAFETY: as in `wait_exited`: all-zero bytes is a valid `siginfo_t`.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a live local the call writes and does not keep.
        let waited = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        checked(waited)?;
        // With `WNOHANG` a child that has not exited leaves `info` as it was: its pid 0.
        // SAFETY: `info` is whole and initialized (zeroed, then written by the call); `si_pid`
        // reads the field of a `SIGCHLD`'s record, which is what `waitid` fills.
        Ok(unsafe { info.si_pid() } != 0)
    }

    /// `O_NONBLOCK` added to `fd`'s status flags.
    pub(crate) fn set_nonblocking(fd: BorrowedFd<'_>) -> io::Result<()> {
        // SAFETY: `fd` is borrowed open for the call; `F_GETFL` takes no pointer.
        let flags = checked(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) })?;
        // SAFETY: as above; `F_SETFL` takes an integer.
        checked(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) })
            .map(drop)
    }

    /// Waits for one of `fds` to be ready, up to `ms` milliseconds from now (negative: no
    /// limit; 0: a look that does not wait): how many are ready, 0 when the time ran out.
    /// Memory-safe for any descriptor numbers (a closed one reports `POLLNVAL`). A wait goes
    /// through `crate::wait::readable` on a deadline made here, so an interrupted wait goes on
    /// for what is left of `ms`, never anew; a caller that waits again in a loop holds its own
    /// `Deadline` and calls `wait::readable` itself.
    pub(crate) fn poll(fds: &mut [libc::pollfd], ms: libc::c_int) -> io::Result<usize> {
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
    #[expect(
        clippy::disallowed_methods,
        reason = "a timeout of 0 asks which descriptors are ready now and never blocks, so no deadline applies (rule deadline-waits)"
    )]
    fn ready_now(fds: &mut [libc::pollfd]) -> io::Result<usize> {
        let n = libc::nfds_t::try_from(fds.len())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: `fds` is a live, exclusive slice of `n` pollfds; the kernel writes only their
        // `revents` and keeps no pointer after the call returns.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), n, 0) };
        checked(ready).map(|it| it as usize)
    }

    /// One `read` of `fd` into `into`: how many bytes, 0 at its end.
    pub(crate) fn read(fd: BorrowedFd<'_>, into: &mut [u8]) -> io::Result<usize> {
        // SAFETY: `into` is a live, exclusive buffer of `into.len()` bytes the call writes at
        // most that many of, and `fd` is borrowed open for the call.
        checked_size(unsafe { libc::read(fd.as_raw_fd(), into.as_mut_ptr().cast(), into.len()) })
    }

    /// One `write` of `bytes` to `fd`: how many it took.
    pub(crate) fn write(fd: BorrowedFd<'_>, bytes: &[u8]) -> io::Result<usize> {
        // SAFETY: `bytes` is a live buffer of `bytes.len()` bytes the call only reads, and `fd`
        // is borrowed open for the call.
        checked_size(unsafe { libc::write(fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) })
    }

    /// The `timespec` of `seconds` and `nanos` (below 10^9).
    pub(crate) fn timespec(seconds: libc::time_t, nanos: u32) -> libc::timespec {
        debug_assert!(nanos < 1_000_000_000);
        // SAFETY: `timespec` is integers, and private padding on some targets (so it cannot be
        // written as a literal): all-zero bytes is a valid value.
        let mut time: libc::timespec = unsafe { std::mem::zeroed() };
        time.tv_sec = seconds;
        // Below 10^9, which every target's `tv_nsec` holds.
        time.tv_nsec = nanos as _;
        time
    }

    /// The access and modification times of `path` (relative to the working directory, a link
    /// followed) set to `times`, in that order: one `utimensat`.
    pub(crate) fn utimens(path: &CStr, times: &[libc::timespec; 2]) -> io::Result<()> {
        // SAFETY: a NUL-terminated path and two `timespec`s, both borrowed across the call,
        // which reads them and keeps neither; no flags.
        checked(unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), 0) })
            .map(drop)
    }

    /// The directory at `path` opened read-only, close-on-exec, as a descriptor of its own.
    pub(crate) fn open_dir(path: &CStr) -> io::Result<OwnedFd> {
        // SAFETY: a NUL-terminated path, borrowed across the call; the result is a new
        // descriptor or -1, checked below.
        let fd = checked(unsafe {
            libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC)
        })?;
        // SAFETY: `open` succeeded, so `fd` is a new descriptor nothing else owns; owned once,
        // here. Owning one twice would close it under the other owner.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// The `stat` of `name` relative to the directory `dir`, not followed through a symlink.
    #[inline]
    pub(crate) fn stat_at(dir: BorrowedFd<'_>, name: &CStr) -> io::Result<libc::stat> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `dir` is borrowed open, `name` NUL-terminated, and `stat` room for one
        // `libc::stat` the call writes whole on success and does not keep.
        let failed = unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        checked(failed)?;
        // SAFETY: the call succeeded, so it filled the `stat`; read before then it would be
        // uninitialized.
        Ok(unsafe { stat.assume_init() })
    }

    /// `target`, a new empty file, made to share `source`'s blocks: `FICLONE`.
    #[cfg(target_os = "linux")]
    pub(crate) fn ficlone(target: BorrowedFd<'_>, source: BorrowedFd<'_>) -> io::Result<()> {
        /// `_IOW(0x94, 9, int)` of `linux/fs.h`.
        const FICLONE: libc::c_ulong = 0x4004_9409;
        // SAFETY: two descriptors borrowed open for the call; `FICLONE` takes the source's
        // number as its argument, no pointer.
        checked(unsafe { libc::ioctl(target.as_raw_fd(), FICLONE as _, source.as_raw_fd()) })
            .map(drop)
    }

    /// The `statfs` of the file system that holds `path`.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn statfs(path: &CStr) -> io::Result<libc::statfs> {
        let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: `path` NUL-terminated and borrowed across the call, `stat` room for one
        // `libc::statfs` the call writes whole on success and does not keep.
        checked(unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) })?;
        // SAFETY: the call succeeded, so it filled the `statfs`.
        Ok(unsafe { stat.assume_init() })
    }

    /// `to` made a clone of the open file `from` (`fclonefileat`, no flags).
    #[cfg(target_os = "macos")]
    pub(crate) fn fclonefileat(from: BorrowedFd<'_>, to: &CStr) -> io::Result<()> {
        // SAFETY: a descriptor borrowed open for the call and a NUL-terminated path borrowed
        // across it, which keeps neither.
        checked(unsafe { libc::fclonefileat(from.as_raw_fd(), libc::AT_FDCWD, to.as_ptr(), 0) })
            .map(drop)
    }

    /// `path`'s permission bits set to `mode`, a symbolic link not followed (`fchmodat` with
    /// `AT_SYMLINK_NOFOLLOW`, which sets a link's own bits). macOS only: glibc makes this call
    /// `fchmodat2`, which some seccomp filters refuse.
    #[cfg(target_os = "macos")]
    pub(crate) fn chmod_no_follow(path: &CStr, mode: u32) -> io::Result<()> {
        // SAFETY: a NUL-terminated path borrowed across the call.
        checked(unsafe {
            libc::fchmodat(
                libc::AT_FDCWD,
                path.as_ptr(),
                mode as libc::mode_t,
                libc::AT_SYMLINK_NOFOLLOW,
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
            libc::utimensat(
                libc::AT_FDCWD,
                path.as_ptr(),
                std::ptr::null(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        })
        .map(drop)
    }

    /// `F_NOCACHE` on `fd`: its reads and writes stay out of the unified buffer cache.
    #[cfg(target_os = "macos")]
    pub(crate) fn no_cache(fd: BorrowedFd<'_>) -> io::Result<()> {
        // SAFETY: `fd` is borrowed open for the call; the flag takes an integer and changes only
        // how its I/O uses the cache.
        checked(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_NOCACHE, 1) }).map(drop)
    }

    /// `f` run with the calling thread's disk IO at idle priority, which is put back after: a
    /// read of [`crate::Priority::Idle`]. Linux: `ioprio_set` of the thread to
    /// `IOPRIO_CLASS_IDLE` (`who` 0 is the calling thread).
    #[cfg(target_os = "linux")]
    pub(crate) fn idle_io<R>(f: impl FnOnce() -> io::Result<R>) -> io::Result<R> {
        const IOPRIO_WHO_PROCESS: libc::c_long = 1;
        const IOPRIO_CLASS_IDLE: libc::c_long = 3;
        const IOPRIO_CLASS_SHIFT: libc::c_long = 13;
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
        const IOPOL_TYPE_DISK: libc::c_int = 0;
        const IOPOL_SCOPE_THREAD: libc::c_int = 1;
        const IOPOL_THROTTLE: libc::c_int = 3;
        unsafe extern "C" {
            fn getiopolicy_np(iotype: libc::c_int, scope: libc::c_int) -> libc::c_int;
            fn setiopolicy_np(
                iotype: libc::c_int,
                scope: libc::c_int,
                policy: libc::c_int,
            ) -> libc::c_int;
        }
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
        // SAFETY: no pointers; the result is a new descriptor or -1, checked below.
        let fd = checked(unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) })?;
        // SAFETY: `eventfd` succeeded, so `fd` is a new descriptor nothing else owns; owned
        // once, here.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// The running kernel's `uname`.
    #[cfg(target_os = "linux")]
    pub(crate) fn uname() -> io::Result<libc::utsname> {
        // SAFETY: `utsname` is arrays of C chars, for which all-zero bytes is a valid value.
        let mut name: libc::utsname = unsafe { std::mem::zeroed() };
        // SAFETY: `name` is a live local the call fills and does not keep.
        checked(unsafe { libc::uname(&mut name) })?;
        Ok(name)
    }

    /// `pid`'s physical footprint now and the most it has been: `proc_pid_rusage`'s
    /// `RUSAGE_INFO_V4`.
    #[cfg(target_os = "macos")]
    pub(crate) fn footprint(pid: libc::pid_t) -> io::Result<(u64, u64)> {
        // SAFETY: `rusage_info_v4` is a C struct of integers, for which all-zero bytes is a
        // valid value.
        let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a live local of the size `RUSAGE_INFO_V4` names, which the call
        // writes and does not keep; a smaller buffer would be written past its end.
        let read = unsafe {
            libc::proc_pid_rusage(
                pid,
                libc::RUSAGE_INFO_V4,
                (&raw mut info).cast::<libc::rusage_info_t>(),
            )
        };
        checked(read)?;
        Ok((info.ri_phys_footprint, info.ri_lifetime_max_phys_footprint))
    }
}

#[cfg(test)]
mod tests {
    use hegel::TestCase;
    use hegel::generators as gs;

    use super::Aligned;

    /// The aligned view against the safe reference, the bytes themselves.
    #[hegel::test]
    fn aligned_bytes_are_the_bytes_copied(tc: TestCase) {
        let bytes: Vec<u8> = tc.draw(gs::binary().max_size(299));
        let aligned = Aligned::copy(&bytes);
        assert_eq!(aligned.bytes(), &bytes[..]);
        assert_eq!(aligned.bytes().as_ptr() as usize % 16, 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_pipe_reads_back_what_was_written_and_polls_ready() {
        use std::os::fd::AsFd as _;
        let (reader, writer) = std::io::pipe().unwrap();
        assert_eq!(super::write(writer.as_fd(), b"blaze").unwrap(), 5);
        let mut fds = [libc::pollfd {
            fd: std::os::fd::AsRawFd::as_raw_fd(&reader),
            events: libc::POLLIN,
            revents: 0,
        }];
        assert_eq!(super::poll(&mut fds, 0).unwrap(), 1);
        let mut into = [0; 16];
        assert_eq!(super::read(reader.as_fd(), &mut into).unwrap(), 5);
        assert_eq!(&into[..5], b"blaze");
        drop(writer);
        assert_eq!(super::read(reader.as_fd(), &mut into).unwrap(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_name_stats_as_not_found() {
        use std::os::fd::AsFd as _;
        let dir = super::open_dir(c"/").unwrap();
        assert!(super::stat_at(dir.as_fd(), c"tmp").is_ok());
        let missing = super::stat_at(dir.as_fd(), c"blaze-no-such-name-at-the-root").unwrap_err();
        assert_eq!(missing.raw_os_error(), Some(libc::ENOENT));
    }
}
