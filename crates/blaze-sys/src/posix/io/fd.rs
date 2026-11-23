//! Descriptors: opening, reading and writing, `fcntl`'s operations, `dup`, pipes, `ioctl`,
//! seeking, syncing, and (Linux) `fallocate` and `memfd_create`, (macOS) `fcntl`'s own
//! commands for syncing, caching, read-ahead and preallocation. On Linux the typed raw system
//! calls of [`crate::linux`] (`linux::fdctl` for `fcntl`, `dup`, `ioctl`, `preadv`,
//! `fallocate`, `memfd_create`) are the items, and no C-library symbol is named; on macOS they
//! are declared here over libSystem with the same signatures. `fcntl` is no item of its own:
//! each command it stands for is a function named for what it does ([`status_flags`],
//! [`set_nonblocking`], [`set_close_on_exec`], [`dup_at_least`], ...), and its doc line says
//! which `F_*` it replaces.

#[cfg(target_os = "macos")]
use core::ffi::{c_int, c_ulong, c_void};

#[cfg(target_os = "macos")]
use super::{counted, said};
use crate::{Errno, Fd, Path};

// ---- Linux: the raw-system-call layer's own items. ----

/// replaces std::os::fd::OwnedFd for blaze-sys's calls (a descriptor this value closes).
/// Linux: [`crate::linux::OwnedFd`].
#[cfg(target_os = "linux")]
pub use crate::linux::OwnedFd;
/// replaces libc::AT_FDCWD. Linux: [`crate::linux::CWD`].
#[cfg(target_os = "linux")]
pub use crate::linux::CWD;
/// replaces libc::O_RDONLY (`READ`), libc::O_WRONLY (`WRITE`), libc::O_RDWR (`READ_WRITE`),
/// libc::O_CREAT (`CREATE`), libc::O_EXCL (`EXCLUSIVE`), libc::O_NOCTTY
/// (`NO_CONTROLLING_TTY`), libc::O_APPEND, libc::O_NONBLOCK, libc::O_DIRECT,
/// libc::O_DIRECTORY, libc::O_NOFOLLOW (`NO_FOLLOW`), libc::O_CLOEXEC (`CLOSE_ON_EXEC`),
/// libc::O_PATH. Linux: [`crate::linux::OpenFlags`].
/// replaces libc::O_DIRECT (blaze_sys::posix::io::OpenFlags::DIRECT; Linux only, macOS has none: use set_no_cache)
/// replaces libc::O_PATH (blaze_sys::posix::io::OpenFlags::PATH; Linux only)
/// replaces libc::O_TRUNC (blaze_sys::posix::io::OpenFlags::TRUNCATE; both systems)
/// replaces libc::O_DSYNC (blaze_sys::posix::io::OpenFlags::DATA_SYNC; both systems)
/// replaces libc::O_NOATIME (blaze_sys::posix::io::OpenFlags::NO_ACCESS_TIME; Linux only)
#[cfg(target_os = "linux")]
pub use crate::linux::OpenFlags;
/// replaces libc::openat. Linux: [`crate::linux::openat`].
#[cfg(target_os = "linux")]
pub use crate::linux::openat;
/// replaces libc::close. Linux: [`crate::linux::close`].
#[cfg(target_os = "linux")]
pub use crate::linux::close;
/// replaces libc::read. Linux: [`crate::linux::read`].
#[cfg(target_os = "linux")]
pub use crate::linux::read;
/// replaces libc::write. Linux: [`crate::linux::write`].
#[cfg(target_os = "linux")]
pub use crate::linux::write;
/// replaces libc::pread. Linux: [`crate::linux::pread`].
#[cfg(target_os = "linux")]
pub use crate::linux::pread;
/// replaces libc::pwrite. Linux: [`crate::linux::pwrite`].
#[cfg(target_os = "linux")]
pub use crate::linux::pwrite;
/// replaces libc::fsync. Linux: [`crate::linux::fsync`].
#[cfg(target_os = "linux")]
pub use crate::linux::fsync;
/// replaces libc::lseek (with `SEEK_DATA` or `SEEK_HOLE`, the seeks blaze makes). Linux:
/// [`crate::linux::lseek`].
/// replaces libc::SEEK_HOLE (blaze_sys::posix::io::Whence::Hole; both systems, each its own number)
/// replaces libc::SEEK_DATA (blaze_sys::posix::io::Whence::Data; both systems, each its own number)
#[cfg(target_os = "linux")]
pub use crate::linux::{Whence, lseek};
/// replaces libc::ftruncate. Linux: [`crate::linux::ftruncate`].
#[cfg(target_os = "linux")]
pub use crate::linux::ftruncate;
/// replaces libc::pipe2 (with libc::O_CLOEXEC as `PipeFlags::CLOSE_ON_EXEC`). Linux:
/// [`crate::linux::pipe2`].
#[cfg(target_os = "linux")]
pub use crate::linux::{PipeFlags, pipe2};
/// replaces libc::iovec (`IoSlice` over bytes to write, `IoSliceMut` over room to read into).
/// Linux: [`crate::linux::IoSlice`], [`crate::linux::IoSliceMut`].
#[cfg(target_os = "linux")]
pub use crate::linux::{IoSlice, IoSliceMut};

/// replaces libc::fcntl with libc::F_GETFL (`status_flags`), libc::F_SETFL
/// (`set_status_flags`, `set_nonblocking`), libc::F_GETFD (`close_on_exec`), libc::F_SETFD and
/// libc::FD_CLOEXEC (`set_close_on_exec`), libc::F_DUPFD and libc::F_DUPFD_CLOEXEC
/// (`dup_at_least`); libc::dup, libc::dup2 (`dup2(a, a)` checks `a` is open and returns it),
/// libc::dup3. Linux: [`crate::linux::fdctl`], raw system calls.
/// replaces libc::F_GETFL (with fcntl: blaze_sys::posix::io::status_flags; both systems)
/// replaces libc::F_SETFL (with fcntl: blaze_sys::posix::io::set_status_flags, or set_nonblocking for O_NONBLOCK; both systems)
/// replaces libc::F_GETFD (with fcntl: blaze_sys::posix::io::close_on_exec, which also answers whether fd is open; both systems)
/// replaces libc::F_SETFD (with fcntl: blaze_sys::posix::io::set_close_on_exec; both systems)
/// replaces libc::FD_CLOEXEC (with F_GETFD/F_SETFD: blaze_sys::posix::io::close_on_exec / set_close_on_exec take and give a bool; both systems)
/// replaces libc::F_DUPFD (with fcntl: blaze_sys::posix::io::dup_at_least(fd, lowest, false); both systems)
/// replaces libc::F_DUPFD_CLOEXEC (with fcntl: blaze_sys::posix::io::dup_at_least(fd, lowest, true); both systems)
/// replaces libc::dup3 (blaze_sys::posix::io::dup3; Linux only)
#[cfg(target_os = "linux")]
pub use crate::linux::fdctl::{
    close_on_exec, dup, dup2, dup3, dup_at_least, set_close_on_exec, set_nonblocking, set_status_flags, status_flags,
};
/// replaces libc::ioctl, libc::FIONBIO, and libc::ioctl with libc::FIONBIO (`fionbio`).
/// Linux: [`crate::linux::fdctl::ioctl`], one raw `ioctl` system call.
/// replaces libc::FIOCLEX (with ioctl: blaze_sys::posix::io::set_close_on_exec(fd, true), the same flag set by fcntl; both systems)
#[cfg(target_os = "linux")]
pub use crate::linux::fdctl::{FIONBIO, fionbio, ioctl};
/// replaces libc::preadv. Linux: [`crate::linux::fdctl::preadv`] (the offset as the call's
/// `pos_l`, `pos_h` halves).
#[cfg(target_os = "linux")]
pub use crate::linux::fdctl::preadv;
/// replaces libc::fallocate, libc::FALLOC_FL_KEEP_SIZE, libc::FALLOC_FL_PUNCH_HOLE. Linux:
/// [`crate::linux::fdctl::fallocate`].
#[cfg(target_os = "linux")]
pub use crate::linux::fdctl::{FallocateFlags, fallocate};
/// replaces libc::memfd_create, libc::MFD_CLOEXEC, libc::MFD_ALLOW_SEALING, libc::fcntl with
/// libc::F_ADD_SEALS (`add_seals`), libc::F_SEAL_SHRINK, libc::F_SEAL_GROW, libc::F_SEAL_SEAL,
/// libc::F_SEAL_WRITE (`Seals`). Linux: [`crate::linux::fdctl`].
/// replaces libc::F_ADD_SEALS (with fcntl: blaze_sys::posix::io::add_seals(fd, Seals); Linux only)
#[cfg(target_os = "linux")]
pub use crate::linux::fdctl::{MemfdFlags, Seals, add_seals, memfd_create};
/// `fcntl`'s command numbers, for the comparison test with libc's.
#[cfg(all(test, target_os = "linux"))]
pub(in crate::posix::io) use crate::linux::fdctl::{
    F_ADD_SEALS, F_DUPFD, F_DUPFD_CLOEXEC, F_GETFD, F_GETFL, F_SETFD, F_SETFL, FD_CLOEXEC,
};

#[cfg(target_os = "macos")]
pub use on_macos::*;

/// macOS's descriptor calls, over libSystem, with the Linux layer's signatures, and the
/// `fcntl` commands of Apple's systems.
#[cfg(target_os = "macos")]
mod on_macos {
    use core::ffi::{c_int, c_uint, c_void};

    use crate::{
        Errno, Fd, Path,
        macos::cold,
        posix::io::{counted, said},
    };

    /// libSystem's descriptor calls the cold table does not hold.
    pub(in crate::posix::io) mod sys_fd {
        use core::ffi::{c_int, c_void};

        unsafe extern "C" {
            /// unistd.h: `int close(int fd)`.
            pub(in crate::posix::io) fn close(fd: c_int) -> c_int;
            /// unistd.h: `ssize_t write(int fd, const void *buf, size_t nbyte)`.
            pub(in crate::posix::io) fn write(fd: c_int, from: *const c_void, len: usize) -> isize;
            /// unistd.h: `int dup(int fildes)`.
            pub(in crate::posix::io) fn dup(fd: c_int) -> c_int;
            /// unistd.h: `int dup2(int fildes, int fildes2)`.
            pub(in crate::posix::io) fn dup2(from: c_int, to: c_int) -> c_int;
            /// unistd.h: `int pipe(int fildes[2])`.
            pub(in crate::posix::io) fn pipe(ends: *mut c_int) -> c_int;
        }
    }

    /// A descriptor this value owns: dropped, it is closed and the close's error is lost;
    /// [`close`] is the form that reports it. [`crate::linux::OwnedFd`]'s API. replaces
    /// std::os::fd::OwnedFd for blaze-sys's calls.
    #[derive(Debug)]
    #[repr(transparent)]
    pub struct OwnedFd(c_int);

    impl OwnedFd {
        /// The owner of the descriptor numbered `raw`.
        ///
        /// # Safety
        ///
        /// `raw` is an open descriptor nothing else owns.
        #[blaze::noalloc]
        #[blaze::nopanic]
        pub const unsafe fn from_raw(raw: c_int) -> OwnedFd {
            OwnedFd(raw)
        }

        /// The descriptor, lent.
        #[blaze::noalloc]
        #[blaze::nopanic]
        pub const fn fd(&self) -> Fd {
            Fd::new(self.0)
        }

        /// The descriptor's number, no longer owned: nothing closes it.
        #[blaze::noalloc]
        #[blaze::nopanic]
        pub fn into_raw(self) -> c_int {
            let raw = self.0;
            core::mem::forget(self);
            raw
        }
    }

    impl Drop for OwnedFd {
        fn drop(&mut self) {
            // SAFETY: `close` takes a number and no memory; this value owns the descriptor.
            _ = unsafe { sys_fd::close(self.0) };
        }
    }

    /// `AT_FDCWD` (fcntl.h, macOS: -2): paths are resolved from the process's current
    /// directory. replaces libc::AT_FDCWD.
    pub const CWD: Fd = Fd::new(-2);

    flag_set! {
        /// `open`'s flags (fcntl.h, macOS's numbers). macOS has no `O_DIRECT` (an uncached
        /// file is [`set_no_cache`]) and no `O_PATH`.
        OpenFlags: u32 {
            /// `O_RDONLY`. replaces libc::O_RDONLY.
            READ = 0x0000;
            /// `O_WRONLY`. replaces libc::O_WRONLY.
            WRITE = 0x0001;
            /// `O_RDWR`. replaces libc::O_RDWR.
            READ_WRITE = 0x0002;
            /// `O_NONBLOCK`. replaces libc::O_NONBLOCK.
            NONBLOCK = 0x0004;
            /// `O_APPEND`. replaces libc::O_APPEND.
            APPEND = 0x0008;
            /// `O_NOFOLLOW`. replaces libc::O_NOFOLLOW.
            NO_FOLLOW = 0x0100;
            /// `O_CREAT`. replaces libc::O_CREAT.
            CREATE = 0x0200;
            /// `O_TRUNC`.
            TRUNCATE = 0x0400;
            /// `O_EXCL`. replaces libc::O_EXCL.
            EXCLUSIVE = 0x0800;
            /// `O_EVTONLY`: open for event notifications only (`EVFILT_VNODE`), without blocking
            /// volume unmount or updating access time. replaces libc::O_EVTONLY.
            EVTONLY = 0x8000;
            /// `O_NOCTTY`. replaces libc::O_NOCTTY.
            NO_CONTROLLING_TTY = 0x0002_0000;
            /// `O_DIRECTORY`. replaces libc::O_DIRECTORY.
            DIRECTORY = 0x0010_0000;
            /// `O_DSYNC`.
            DATA_SYNC = 0x0040_0000;
            /// `O_CLOEXEC`. replaces libc::O_CLOEXEC.
            CLOSE_ON_EXEC = 0x0100_0000;
            /// `O_NOFOLLOW_ANY`: no component of the path may be a symbolic link.
            NO_FOLLOW_ANY = 0x2000_0000;
        }
    }

    /// `openat`: `path` opened from `dir`, through the cold table; `mode` is passed for a
    /// create only. [`crate::linux::openat`]'s signature. replaces libc::openat.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn openat(dir: Fd, path: Path<'_>, flags: OpenFlags, mode: u32) -> Result<OwnedFd, Errno> {
        // SAFETY: `path` is a live NUL-terminated string the call reads and does not keep; the
        // mode is passed through `...` as the `c_uint` C promotes a `mode_t` to.
        let fd = said(unsafe { cold::openat(dir.raw(), path.as_ptr().cast(), flags.bits() as c_int, mode as c_uint) })?;
        // SAFETY: the call returned a new descriptor to this call alone.
        Ok(unsafe { OwnedFd::from_raw(fd) })
    }

    /// `close`, reporting its error. The descriptor is gone either way (macOS frees the number
    /// before it can fail, `EINTR` included), so a close is never retried. replaces libc::close.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn close(fd: OwnedFd) -> Result<(), Errno> {
        let raw = fd.into_raw();
        // SAFETY: `close` takes a number and no memory; `fd` owned the descriptor.
        said(unsafe { sys_fd::close(raw) }).map(|_| ())
    }

    /// `read`: up to `into.len()` bytes from the descriptor's position; 0 at the end, through
    /// the cold table. replaces libc::read.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn read(fd: Fd, into: &mut [u8]) -> Result<usize, Errno> {
        // SAFETY: `into` is live memory of `into.len()` bytes the call writes at most that
        // many bytes of.
        counted(unsafe { cold::read(fd.raw(), into.as_mut_ptr().cast(), into.len()) })
    }

    /// `pread`: up to `into.len()` bytes from offset `at`; 0 at the end. replaces libc::pread.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn pread(fd: Fd, into: &mut [u8], at: u64) -> Result<usize, Errno> {
        let Ok(at) = i64::try_from(at) else {
            return Err(Errno::Inval);
        };
        // SAFETY: as `read`.
        counted(unsafe { cold::pread(fd.raw(), into.as_mut_ptr().cast(), into.len(), at) })
    }

    /// `write`: up to `bytes.len()` bytes at the descriptor's position (its end, for
    /// `O_APPEND`); it may write fewer. replaces libc::write.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn write(fd: Fd, bytes: &[u8]) -> Result<usize, Errno> {
        // SAFETY: `bytes` is live memory of `bytes.len()` bytes the call only reads.
        counted(unsafe { sys_fd::write(fd.raw(), bytes.as_ptr().cast::<c_void>(), bytes.len()) })
    }

    /// `pwrite`: up to `bytes.len()` bytes at offset `at`; it may write fewer. replaces
    /// libc::pwrite.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn pwrite(fd: Fd, bytes: &[u8], at: u64) -> Result<usize, Errno> {
        let Ok(at) = i64::try_from(at) else {
            return Err(Errno::Inval);
        };
        // SAFETY: as `write`.
        counted(unsafe { cold::pwrite(fd.raw(), bytes.as_ptr().cast(), bytes.len(), at) })
    }

    /// `fsync`: the file's data handed to the drive, and its metadata; on macOS this does not
    /// flush the drive's own cache ([`full_fsync`] does). replaces libc::fsync.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn fsync(fd: Fd) -> Result<(), Errno> {
        // SAFETY: the call takes a number and no memory.
        said(unsafe { cold::fsync(fd.raw()) }).map(|_| ())
    }

    /// What an [`lseek`] looks for from its offset (unistd.h: macOS's numbers are Linux's
    /// swapped).
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    #[repr(u32)]
    pub enum Whence {
        /// `SEEK_HOLE`: the first offset at or after the one given that is in a hole; a file's
        /// end counts as one.
        Hole = 3,
        /// `SEEK_DATA`: the first offset at or after the one given that holds data.
        Data = 4,
    }

    /// `lseek` with `SEEK_DATA` or `SEEK_HOLE`: where the file's next data, or its next hole,
    /// begins at or after `at`. [`Errno::Nxio`] when there is none. [`crate::linux::lseek`]'s
    /// signature. replaces libc::lseek.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn lseek(fd: Fd, at: u64, whence: Whence) -> Result<u64, Errno> {
        let Ok(at) = i64::try_from(at) else {
            return Err(Errno::Inval);
        };
        // SAFETY: the call takes numbers and no memory.
        let found = unsafe { cold::lseek(fd.raw(), at, whence as c_int) };
        if found < 0 {
            return Err(crate::posix::signal::errno());
        }
        Ok(found as u64)
    }

    /// `ftruncate`: the file made `len` bytes long. replaces libc::ftruncate.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn ftruncate(fd: Fd, len: u64) -> Result<(), Errno> {
        let Ok(len) = i64::try_from(len) else {
            return Err(Errno::FBig);
        };
        // SAFETY: the call takes numbers and no memory.
        said(unsafe { cold::ftruncate(fd.raw(), len) }).map(|_| ())
    }

    /// `struct iovec` over bytes to write (sys/_types/_iovec_t.h). replaces libc::iovec.
    #[derive(Clone, Copy)]
    #[repr(C)]
    pub struct IoSlice<'a> {
        base: *const u8,
        len: usize,
        bytes: core::marker::PhantomData<&'a [u8]>,
    }

    impl<'a> IoSlice<'a> {
        /// `bytes`, lent for a write.
        #[blaze::noalloc]
        #[blaze::nopanic]
        pub const fn new(bytes: &'a [u8]) -> IoSlice<'a> {
            IoSlice { base: bytes.as_ptr(), len: bytes.len(), bytes: core::marker::PhantomData }
        }
    }

    /// `struct iovec` over room to read into. replaces libc::iovec.
    #[repr(C)]
    pub struct IoSliceMut<'a> {
        base: *mut u8,
        len: usize,
        bytes: core::marker::PhantomData<&'a mut [u8]>,
    }

    impl<'a> IoSliceMut<'a> {
        /// `into`, lent for a read.
        #[blaze::noalloc]
        #[blaze::nopanic]
        pub const fn new(into: &'a mut [u8]) -> IoSliceMut<'a> {
            IoSliceMut { base: into.as_mut_ptr(), len: into.len(), bytes: core::marker::PhantomData }
        }

        /// `room`, not yet initialized, lent for a read: the kernel only writes it, and the
        /// count returned is how many bytes from its start it initialized. Linux's is
        /// [`crate::linux::IoSliceMut::uninit`].
        /// replaces libc::iovec (over uninitialized room, macOS)
        #[blaze::noalloc]
        #[blaze::nopanic]
        pub const fn uninit(room: &'a mut [core::mem::MaybeUninit<u8>]) -> IoSliceMut<'a> {
            IoSliceMut { base: room.as_mut_ptr().cast::<u8>(), len: room.len(), bytes: core::marker::PhantomData }
        }
    }

    // sys/_types/_iovec_t.h, macOS arm64: `void *iov_base; size_t iov_len;`.
    const _: () = {
        assert!(size_of::<IoSlice<'_>>() == 16);
        assert!(align_of::<IoSlice<'_>>() == 8);
        assert!(core::mem::offset_of!(IoSlice<'_>, len) == 8);
        assert!(size_of::<IoSliceMut<'_>>() == 16);
        assert!(align_of::<IoSliceMut<'_>>() == 8);
        assert!(core::mem::offset_of!(IoSliceMut<'_>, len) == 8);
    };

    /// `F_PREALLOCATE` (sys/fcntl.h).
    pub(in crate::posix::io) const F_PREALLOCATE: c_int = 42;
    /// `F_RDADVISE`.
    pub(in crate::posix::io) const F_RDADVISE: c_int = 44;
    /// `F_NOCACHE`.
    pub(in crate::posix::io) const F_NOCACHE: c_int = 48;
    /// `F_FULLFSYNC`.
    pub(in crate::posix::io) const F_FULLFSYNC: c_int = 51;
    /// `F_BARRIERFSYNC`.
    pub(in crate::posix::io) const F_BARRIERFSYNC: c_int = 85;

    /// `fcntl(fd, command, arg)` with a pointer argument, through libSystem's `fcntl`.
    ///
    /// # Safety
    ///
    /// `arg` is what `command` reads or writes, live for the call.
    #[blaze::noalloc]
    #[blaze::nopanic]
    unsafe fn fcntl_ptr(fd: Fd, command: c_int, arg: *mut c_void) -> Result<c_int, Errno> {
        // SAFETY: the caller's contract.
        said(unsafe { crate::macos::ffi::fcntl(fd.raw(), command, arg) })
    }

    /// `fcntl(F_BARRIERFSYNC)`: the file's bytes ordered before any later write reaches the
    /// drive, without flushing the drive's cache. replaces libc::fcntl with libc::F_BARRIERFSYNC.
    /// replaces libc::F_BARRIERFSYNC (with fcntl: blaze_sys::posix::io::barrier_fsync; macOS only)
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn barrier_fsync(fd: Fd) -> Result<(), Errno> {
        super::fcntl_int(fd, F_BARRIERFSYNC, 0).map(|_| ())
    }

    /// `fcntl(F_FULLFSYNC)`: the file's bytes through the drive's own cache onto its medium.
    /// replaces libc::F_FULLFSYNC (with fcntl: blaze_sys::posix::io::full_fsync; macOS only)
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn full_fsync(fd: Fd) -> Result<(), Errno> {
        super::fcntl_int(fd, F_FULLFSYNC, 0).map(|_| ())
    }

    /// `fcntl(F_NOCACHE)`: the file's reads and writes kept out of the page cache (`on`) or
    /// in it. replaces libc::fcntl with libc::F_NOCACHE.
    /// replaces libc::F_NOCACHE (with fcntl: blaze_sys::posix::io::set_no_cache(fd, on); macOS only)
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn set_no_cache(fd: Fd, on: bool) -> Result<(), Errno> {
        super::fcntl_int(fd, F_NOCACHE, c_int::from(on)).map(|_| ())
    }

    /// `struct radvisory` (sys/fcntl.h): `F_RDADVISE`'s argument. replaces libc::radvisory.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    #[repr(C)]
    pub struct Radvisory {
        pub offset: i64,
        pub count: c_int,
    }

    // sys/fcntl.h, macOS arm64, as the libc crate lays it: `off_t ra_offset; int ra_count;`,
    // padded to 16.
    const _: () = {
        assert!(size_of::<Radvisory>() == 16);
        assert!(align_of::<Radvisory>() == 8);
        assert!(core::mem::offset_of!(Radvisory, offset) == 0);
        assert!(core::mem::offset_of!(Radvisory, count) == 8);
    };

    /// `fcntl(F_RDADVISE)`: the system's read-ahead of `count` bytes of `fd` at `at` begun,
    /// not waited for. replaces libc::fcntl with libc::F_RDADVISE.
    /// replaces libc::F_RDADVISE (with fcntl and libc::radvisory: blaze_sys::posix::io::read_advise(fd, at, count); macOS only)
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn read_advise(fd: Fd, at: u64, count: u32) -> Result<(), Errno> {
        let (Ok(offset), Ok(count)) = (i64::try_from(at), c_int::try_from(count)) else {
            return Err(Errno::Inval);
        };
        let mut advice = Radvisory { offset, count };
        // SAFETY: `F_RDADVISE` reads one `struct radvisory`, live for the call.
        unsafe { fcntl_ptr(fd, F_RDADVISE, (&raw mut advice).cast()) }.map(|_| ())
    }

    flag_set! {
        /// `fstore_t`'s `fst_flags` (sys/fcntl.h).
        AllocateFlags: u32 {
            /// `F_ALLOCATECONTIG`: one contiguous run, or nothing. replaces libc::F_ALLOCATECONTIG.
            CONTIGUOUS = 0x0000_0002;
            /// `F_ALLOCATEALL`: all of it, or nothing. replaces libc::F_ALLOCATEALL.
            ALL = 0x0000_0004;
        }
    }

    /// `fstore_t`'s `fst_posmode` (sys/fcntl.h): where the range's offset counts from.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    #[repr(transparent)]
    pub struct PosMode(pub c_int);

    impl PosMode {
        /// `F_PEOFPOSMODE`: from the file's physical end. replaces libc::F_PEOFPOSMODE.
        pub const PHYSICAL_END: PosMode = PosMode(3);
        /// `F_VOLPOSMODE`: from the volume's start.
        /// replaces libc::F_VOLPOSMODE (blaze_sys::posix::io::PosMode::VOLUME; macOS only)
        pub const VOLUME: PosMode = PosMode(4);
    }

    /// `fstore_t` (sys/fcntl.h): `F_PREALLOCATE`'s argument; the call writes `bytes_alloc`.
    /// replaces libc::fstore_t.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    #[repr(C)]
    pub struct Fstore {
        pub flags: AllocateFlags,
        pub posmode: PosMode,
        pub offset: i64,
        pub length: i64,
        /// Written by the call: the bytes it allocated.
        pub bytes_alloc: i64,
    }

    // sys/fcntl.h, macOS arm64: `unsigned int fst_flags; int fst_posmode; off_t fst_offset;
    // off_t fst_length; off_t fst_bytesalloc;`.
    const _: () = {
        assert!(size_of::<Fstore>() == 32);
        assert!(align_of::<Fstore>() == 8);
        assert!(core::mem::offset_of!(Fstore, flags) == 0);
        assert!(core::mem::offset_of!(Fstore, posmode) == 4);
        assert!(core::mem::offset_of!(Fstore, offset) == 8);
        assert!(core::mem::offset_of!(Fstore, length) == 16);
        assert!(core::mem::offset_of!(Fstore, bytes_alloc) == 24);
    };

    /// `fcntl(F_PREALLOCATE)`: the blocks `store` asks for allocated to `fd` without changing
    /// its length; `store.bytes_alloc` says how many bytes were. replaces libc::fcntl with
    /// libc::F_PREALLOCATE.
    /// replaces libc::F_PREALLOCATE (with fcntl and libc::fstore_t: blaze_sys::posix::io::preallocate(fd, &mut Fstore); macOS only)
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn preallocate(fd: Fd, store: &mut Fstore) -> Result<(), Errno> {
        // SAFETY: `F_PREALLOCATE` reads and writes one `fstore_t`, live for the call (the
        // layout asserted above).
        unsafe { fcntl_ptr(fd, F_PREALLOCATE, core::ptr::from_mut(store).cast()) }.map(|_| ())
    }

    /// `F_DUPFD_CLOEXEC` (sys/fcntl.h, macOS).
    pub(in crate::posix::io) const F_DUPFD_CLOEXEC: c_int = 67;
    /// `FIONBIO` (sys/filio.h): `_IOW('f', 126, int)`.
    pub(in crate::posix::io) const FIONBIO_REQUEST: core::ffi::c_ulong = 0x8004_667e;
}

// ---- Both systems ----

/// `open`: `path` opened from the current directory, `mode` the permission bits of a file it
/// creates. `openat(CWD, ..)`. replaces libc::open, libc::open64.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn open(path: Path<'_>, flags: OpenFlags, mode: u32) -> Result<OwnedFd, Errno> {
    openat(CWD, path, flags, mode)
}

/// Non-blocking `flock` operations supported across both macOS and Linux.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FlockOp {
    SharedNonBlock,
    ExclusiveNonBlock,
    Unlock,
}

/// Non-blocking advisory file lock (`flock`) across macOS and Linux.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn flock(fd: Fd, op: FlockOp) -> Result<(), Errno> {
    #[cfg(target_os = "macos")]
    {
        let raw_op: c_int = match op {
            FlockOp::SharedNonBlock => 1 | 4,
            FlockOp::ExclusiveNonBlock => 2 | 4,
            FlockOp::Unlock => 8,
        };
        // SAFETY: `flock` takes only integer fd and operation flags, touching no memory.
        said(unsafe { crate::macos::cold::flock(fd.raw(), raw_op) }).map(|_| ())
    }
    #[cfg(target_os = "linux")]
    {
        use crate::linux::LockOp;
        let lock_op = match op {
            FlockOp::SharedNonBlock => LockOp::SHARED | LockOp::NONBLOCK,
            FlockOp::ExclusiveNonBlock => LockOp::EXCLUSIVE | LockOp::NONBLOCK,
            FlockOp::Unlock => LockOp::UNLOCK,
        };
        crate::linux::flock(fd, lock_op)
    }
}

// ---- macOS: fcntl, dup, ioctl, preadv over libSystem ----

/// `F_DUPFD` (sys/fcntl.h).
#[cfg(target_os = "macos")]
pub(in crate::posix::io) const F_DUPFD: c_int = 0;
/// `F_GETFD`.
#[cfg(target_os = "macos")]
pub(in crate::posix::io) const F_GETFD: c_int = 1;
/// `F_SETFD`.
#[cfg(target_os = "macos")]
pub(in crate::posix::io) const F_SETFD: c_int = 2;
/// `F_GETFL`.
#[cfg(target_os = "macos")]
pub(in crate::posix::io) const F_GETFL: c_int = 3;
/// `F_SETFL`.
#[cfg(target_os = "macos")]
pub(in crate::posix::io) const F_SETFL: c_int = 4;
/// `FD_CLOEXEC`: the one descriptor flag.
#[cfg(target_os = "macos")]
pub(in crate::posix::io) const FD_CLOEXEC: c_int = 1;

/// `FIONBIO`: the request of [`ioctl`] that sets a descriptor's non-blocking mode from an
/// `int` (sys/filio.h: `_IOW('f', 126, int)`). replaces libc::FIONBIO.
#[cfg(target_os = "macos")]
pub const FIONBIO: c_ulong = on_macos::FIONBIO_REQUEST;

/// `fcntl(fd, command, arg)` with an `int` argument, 0 for a command that takes none (an
/// extra argument through `...` is not read), through libSystem's `fcntl`.
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
fn fcntl_int(fd: Fd, command: c_int, arg: c_int) -> Result<c_int, Errno> {
    // SAFETY: every command this file passes takes an `int` or nothing, and no memory.
    said(unsafe { crate::macos::ffi::fcntl(fd.raw(), command, arg) })
}

/// `fcntl(F_GETFL)`: the descriptor's status flags and access mode. replaces libc::fcntl with
/// libc::F_GETFL.
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn status_flags(fd: Fd) -> Result<OpenFlags, Errno> {
    fcntl_int(fd, F_GETFL, 0).map(|flags| OpenFlags::from_bits(flags as u32))
}

/// `fcntl(F_SETFL)`: the descriptor's status flags set to `flags` (only `O_APPEND`,
/// `O_NONBLOCK` and, on Linux, `O_DIRECT` and `O_NOATIME` change; the access mode does not).
/// replaces libc::fcntl with libc::F_SETFL.
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn set_status_flags(fd: Fd, flags: OpenFlags) -> Result<(), Errno> {
    fcntl_int(fd, F_SETFL, flags.bits() as c_int).map(|_| ())
}

/// `fcntl(F_GETFL)` then `fcntl(F_SETFL)` with `O_NONBLOCK` added (`on`) or taken away, the
/// other flags kept. replaces the pair libc::fcntl(fd, libc::F_GETFL) and
/// libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK).
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn set_nonblocking(fd: Fd, on: bool) -> Result<(), Errno> {
    let now = status_flags(fd)?;
    let then = if on {
        now.with(OpenFlags::NONBLOCK)
    } else {
        OpenFlags::from_bits(now.bits() & !OpenFlags::NONBLOCK.bits())
    };
    if then == now {
        return Ok(());
    }
    set_status_flags(fd, then)
}

/// `fcntl(F_GETFD)`: whether the descriptor closes on exec; [`Errno::BadF`] for a number no
/// descriptor has (the check whether one is open). replaces libc::fcntl with libc::F_GETFD.
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn close_on_exec(fd: Fd) -> Result<bool, Errno> {
    fcntl_int(fd, F_GETFD, 0).map(|flags| flags & FD_CLOEXEC != 0)
}

/// `fcntl(F_SETFD, FD_CLOEXEC)` (`on`) or `fcntl(F_SETFD, 0)`: whether the descriptor closes
/// on exec. replaces libc::fcntl with libc::F_SETFD, libc::FD_CLOEXEC.
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn set_close_on_exec(fd: Fd, on: bool) -> Result<(), Errno> {
    fcntl_int(fd, F_SETFD, if on { FD_CLOEXEC } else { 0 }).map(|_| ())
}

/// `fcntl(F_DUPFD_CLOEXEC, lowest)` (`close_on_exec`) or `fcntl(F_DUPFD, lowest)`: a second
/// descriptor of the same open file, numbered `lowest` or the lowest free number above it.
/// replaces libc::fcntl with libc::F_DUPFD_CLOEXEC, libc::F_DUPFD.
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn dup_at_least(fd: Fd, lowest: c_int, close_on_exec: bool) -> Result<OwnedFd, Errno> {
    let cloexec = on_macos::F_DUPFD_CLOEXEC;
    let copy = fcntl_int(fd, if close_on_exec { cloexec } else { F_DUPFD }, lowest)?;
    // SAFETY: the call returned a new descriptor to this call alone.
    Ok(unsafe { OwnedFd::from_raw(copy) })
}

/// `dup`: a second descriptor of the same open file, the lowest free number, not closed on
/// exec. replaces libc::dup.
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn dup(fd: Fd) -> Result<OwnedFd, Errno> {
    // SAFETY: the call takes a number and no memory.
    let copy = said(unsafe { sys_fd::dup(fd.raw()) })?;
    // SAFETY: the call returned a new descriptor to this call alone.
    Ok(unsafe { OwnedFd::from_raw(copy) })
}

/// `dup2`: the number `to` made a descriptor of `from`'s open file, closing what `to` was
/// first (an owner of `to` now owns the copy); `to` itself, not closed on exec. Nothing
/// changes where they are the same. replaces libc::dup2.
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn dup2(from: Fd, to: Fd) -> Result<Fd, Errno> {
    // SAFETY: the call takes numbers and no memory.
    said(unsafe { sys_fd::dup2(from.raw(), to.raw()) }).map(Fd::new)
}

/// `pipe`: a pipe's read end and write end, neither closed on exec nor non-blocking (the
/// flags are a second call on macOS, which has no `pipe2`). replaces libc::pipe.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn pipe() -> Result<(OwnedFd, OwnedFd), Errno> {
    #[cfg(target_os = "linux")]
    {
        crate::linux::pipe2(PipeFlags::NONE)
    }
    #[cfg(target_os = "macos")]
    {
        let mut ends: [c_int; 2] = [0; 2];
        // SAFETY: `ends` is two live `int`s the call writes.
        said(unsafe { sys_fd::pipe(ends.as_mut_ptr()) })?;
        let [read, write] = ends;
        // SAFETY: the call returned two new descriptors to this call alone.
        Ok(unsafe { (OwnedFd::from_raw(read), OwnedFd::from_raw(write)) })
    }
}

/// `ioctl(fd, request, arg)`: a device's own request; the value it returns (0 for most),
/// through the cold table, which passes `arg` on for every request whose encoding says it
/// takes one. The typed requests have their own functions ([`fionbio`]). replaces libc::ioctl.
///
/// # Safety
///
/// `arg` is what `request` reads or writes, live for the call (or null for a request that
/// takes nothing).
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn ioctl(fd: Fd, request: c_ulong, arg: *mut c_void) -> Result<c_int, Errno> {
    // SAFETY: the caller's contract.
    said(unsafe { crate::macos::cold::ioctl(fd.raw(), request, arg) })
}

/// `ioctl(FIONBIO)`: the descriptor's non-blocking mode set (`on`) or cleared in one call (a
/// socket's, where `fcntl` would take two). replaces libc::ioctl with libc::FIONBIO.
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn fionbio(fd: Fd, on: bool) -> Result<(), Errno> {
    let mut on = c_int::from(on);
    // SAFETY: `FIONBIO` reads one `int`, a live local.
    unsafe { ioctl(fd, FIONBIO, (&raw mut on).cast()) }.map(|_| ())
}

/// `preadv`: up to the slices' total from offset `at`, filling them in order; 0 at the end.
/// It may read fewer than asked (the caller continues). replaces libc::preadv.
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn preadv(fd: Fd, into: &mut [IoSliceMut<'_>], at: u64) -> Result<usize, Errno> {
    let count = super::c_count(into.len())?;
    let Ok(at) = i64::try_from(at) else {
        return Err(Errno::Inval);
    };
    let iov = into.as_mut_ptr().cast_const().cast::<c_void>();
    // SAFETY: `into` is `count` live `struct iovec`s (`IoSliceMut` is laid out as one), each
    // over live memory of its length the call writes at most that many bytes of; nothing is
    // kept after the call.
    counted(unsafe { crate::macos::cold::preadv(fd.raw(), iov, count, at) })
}

/// `readlink`: what the symbolic link `path` names, written into `into` and lent back, no NUL
/// added; a result that fills `into` is refused as [`Errno::NameTooLong`] (it may be cut), as
/// [`crate::linux::readlinkat`] refuses it. replaces libc::readlink.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn readlink<'a>(path: Path<'_>, into: &'a mut [u8]) -> Result<&'a [u8], Errno> {
    #[cfg(target_os = "linux")]
    {
        crate::linux::readlinkat(CWD, path, into)
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: `path` is a live NUL-terminated string the call reads; `into` is live memory
        // of `into.len()` bytes it writes at most that many of; it keeps neither.
        let len = counted(unsafe { crate::macos::cold::readlink(path.as_ptr().cast(), into.as_mut_ptr().cast(), into.len()) })?;
        // The buffer, lent back for as long as the caller lent it.
        let into: &'a [u8] = into;
        if len >= into.len() {
            return Err(Errno::NameTooLong);
        }
        into.get(..len).ok_or(Errno::NameTooLong)
    }
}

// ---- Linux only ----

/// replaces libc::FICLONE (the request; the call is `ficlone(to, from)`). Linux:
/// [`crate::linux::ficlone`].
#[cfg(target_os = "linux")]
pub use crate::linux::ficlone;
