//! Files and names: what the file effects and the beneath-only opens use.

use super::{nr, raw};
use crate::{Errno, Fd, Path, Timespec};

/// A descriptor this value owns: dropped, it is closed and the close's error is lost;
/// [`close`] is the form that reports it.
#[derive(Debug)]
#[repr(transparent)]
pub struct OwnedFd(i32);

impl OwnedFd {
    /// The owner of the descriptor numbered `raw`.
    ///
    /// # Safety
    ///
    /// `raw` is an open descriptor nothing else owns.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const unsafe fn from_raw(raw: i32) -> OwnedFd {
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
    pub fn into_raw(self) -> i32 {
        let raw = self.0;
        core::mem::forget(self);
        raw
    }
}

impl Drop for OwnedFd {
    fn drop(&mut self) {
        // SAFETY: `close` takes a number and no memory; this value owns the descriptor.
        _ = unsafe { raw::syscall1(nr::CLOSE, self.0 as usize) };
    }
}

/// A descriptor's number as an argument.
#[blaze::noalloc]
#[blaze::nopanic]
pub(super) const fn arg(fd: Fd) -> usize {
    // Sign-extended, so `AT_FDCWD` (-100) is the kernel's.
    fd.raw() as isize as usize
}

/// The call's new descriptor, owned.
#[blaze::noalloc]
#[blaze::nopanic]
pub(super) fn owned(result: usize) -> Result<OwnedFd, Errno> {
    let raw = raw::check(result)?;
    // SAFETY: the kernel returned a new descriptor to this call alone.
    Ok(unsafe { OwnedFd::from_raw(raw as i32) })
}

/// `AT_FDCWD`: paths are resolved from the process's current directory. For blaze-core's
/// drivers, which still take whole paths; bstd has no current directory.
pub const CWD: Fd = Fd::new(-100);

flag_set! {
    /// `open`'s flags. `O_DIRECTORY`, `O_NOFOLLOW`, `O_DIRECT` and `O_LARGEFILE` differ
    /// between x86-64 and aarch64; the rest are the same.
    OpenFlags: u32 {
        /// `O_RDONLY`.
        READ = 0;
        /// `O_WRONLY`.
        WRITE = 1;
        /// `O_RDWR`.
        READ_WRITE = 2;
        /// `O_CREAT`.
        CREATE = 0o100;
        /// `O_EXCL`.
        EXCLUSIVE = 0o200;
        /// `O_NOCTTY`.
        NO_CONTROLLING_TTY = 0o400;
        /// `O_TRUNC`.
        TRUNCATE = 0o1000;
        /// `O_APPEND`.
        APPEND = 0o2000;
        /// `O_NONBLOCK`.
        NONBLOCK = 0o4000;
        /// `O_DSYNC`.
        DATA_SYNC = 0o10000;
        /// `O_DIRECT`.
        DIRECT = if cfg!(target_arch = "aarch64") { 0o200000 } else { 0o40000 };
        /// `O_DIRECTORY`.
        DIRECTORY = if cfg!(target_arch = "aarch64") { 0o40000 } else { 0o200000 };
        /// `O_NOFOLLOW`.
        NO_FOLLOW = if cfg!(target_arch = "aarch64") { 0o100000 } else { 0o400000 };
        /// `O_NOATIME`.
        NO_ACCESS_TIME = 0o1000000;
        /// `O_CLOEXEC`.
        CLOSE_ON_EXEC = 0o2000000;
        /// `O_PATH`.
        PATH = 0o10000000;
    }
}

flag_set! {
    /// `openat2`'s `resolve` flags.
    /// replaces libc::RESOLVE_NO_XDEV (blaze_sys::linux::Resolve::NO_XDEV)
    /// replaces libc::RESOLVE_NO_MAGICLINKS (blaze_sys::linux::Resolve::NO_MAGICLINKS)
    /// replaces libc::RESOLVE_NO_SYMLINKS (blaze_sys::linux::Resolve::NO_SYMLINKS)
    /// replaces libc::RESOLVE_BENEATH (blaze_sys::linux::Resolve::BENEATH)
    /// replaces libc::RESOLVE_IN_ROOT (blaze_sys::linux::Resolve::IN_ROOT)
    Resolve: u64 {
        /// `RESOLVE_NO_XDEV`.
        NO_XDEV = 0x01;
        /// `RESOLVE_NO_MAGICLINKS`.
        NO_MAGICLINKS = 0x02;
        /// `RESOLVE_NO_SYMLINKS`.
        NO_SYMLINKS = 0x04;
        /// `RESOLVE_BENEATH`.
        BENEATH = 0x08;
        /// `RESOLVE_IN_ROOT`.
        IN_ROOT = 0x10;
        /// `RESOLVE_CACHED`.
        CACHED = 0x20;
    }
}

/// `struct open_how`, `openat2`'s argument.
/// replaces libc::open_how (blaze_sys::linux::OpenHow::new(OpenFlags, mode, Resolve); fields private)
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

impl OpenHow {
    /// An open with `flags`, the permission bits `mode` for a file it creates, and `resolve`.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn new(flags: OpenFlags, mode: u32, resolve: Resolve) -> OpenHow {
        OpenHow { flags: flags.bits() as u64, mode: mode as u64, resolve: resolve.bits() }
    }
}

/// `openat`: `path` opened from `dir`. Used by the `threads` driver's file effects, which
/// take whole paths ([`CWD`]).
/// replaces libc::syscall(libc::SYS_openat) (as blaze_sys::linux::openat(dir: Fd, Path, OpenFlags, mode); returns Result<OwnedFd, Errno>, the site's -1 check becomes the Err)
/// replaces libc::SYS_openat
#[blaze::noalloc]
#[blaze::nopanic]
pub fn openat(dir: Fd, path: Path<'_>, flags: OpenFlags, mode: u32) -> Result<OwnedFd, Errno> {
    // SAFETY: `path` is a live NUL-terminated string the kernel reads and does not keep.
    owned(unsafe {
        raw::syscall4(nr::OPENAT, arg(dir), path.as_ptr() as usize, flags.bits() as usize, mode as usize)
    })
}

/// `openat2`: `path` opened from `dir` as `how` says. [`Errno::NoSys`] before Linux 5.6.
/// replaces libc::syscall(libc::SYS_openat2) (as blaze_sys::linux::openat2(dir: Fd, Path, &OpenHow); the size argument is passed for you; returns Result<OwnedFd, Errno>)
/// replaces libc::SYS_openat2
#[blaze::noalloc]
#[blaze::nopanic]
pub fn openat2(dir: Fd, path: Path<'_>, how: &OpenHow) -> Result<OwnedFd, Errno> {
    // SAFETY: `path` is a live NUL-terminated string and `how` a live `open_how` of the size
    // passed; the kernel reads both and keeps neither.
    owned(unsafe {
        raw::syscall4(
            nr::OPENAT2,
            arg(dir),
            path.as_ptr() as usize,
            core::ptr::from_ref(how) as usize,
            size_of::<OpenHow>(),
        )
    })
}

/// `path` opened beneath `dir` and nowhere else: `openat2` with `RESOLVE_BENEATH |
/// RESOLVE_NO_MAGICLINKS`, as blaze-core's `beneath.rs` opens (branch `core-effects-1109`).
/// The kernel resolves the whole path: an absolute link or a `..` that climbs out of `dir` is
/// [`Errno::XDev`], also one swapped in while it runs; a link that stays beneath is followed.
/// [`Errno::NoSys`] before Linux 5.6, where nothing else opens beneath (rule no-fallback).
/// Used by `Effect::OpenDir`, `Effect::OpenFile` and every effect that acts on a name
/// beneath a handle.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn open_beneath(dir: Fd, path: Path<'_>, flags: OpenFlags, mode: u32) -> Result<OwnedFd, Errno> {
    let resolve = Resolve::BENEATH.with(Resolve::NO_MAGICLINKS);
    openat2(dir, path, &OpenHow::new(flags.with(OpenFlags::CLOSE_ON_EXEC), mode, resolve))
}

/// `close`, reporting its error (a write-back error of some file systems). The descriptor is
/// gone either way: Linux frees the number before it can fail, so a close is never retried.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn close(fd: OwnedFd) -> Result<(), Errno> {
    let raw = fd.into_raw();
    // SAFETY: `close` takes a number and no memory; `fd` owned the descriptor.
    raw::check(unsafe { raw::syscall1(nr::CLOSE, raw as usize) }).map(|_| ())
}

/// `read`: up to `into.len()` bytes from the descriptor's position; 0 at the end.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn read(fd: Fd, into: &mut [u8]) -> Result<usize, Errno> {
    // SAFETY: `into` is live memory of `into.len()` bytes the kernel writes at most that
    // many bytes of.
    raw::check(unsafe { raw::syscall3(nr::READ, arg(fd), into.as_mut_ptr() as usize, into.len()) })
}

/// `pread64`: up to `into.len()` bytes from offset `at`; 0 at the end. It may read fewer than
/// asked (the caller continues). Used by `Effect::Read` and `Effect::ReadAll`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn pread(fd: Fd, into: &mut [u8], at: u64) -> Result<usize, Errno> {
    // SAFETY: as `read`.
    raw::check(unsafe {
        raw::syscall4(nr::PREAD64, arg(fd), into.as_mut_ptr() as usize, into.len(), at as usize)
    })
}

/// `write`: up to `bytes.len()` bytes at the descriptor's position (its end, for
/// `O_APPEND`). It may write fewer than asked (the caller continues). Used by
/// `Effect::Append`, and by the panic handler's one write to [`Fd::STDERR`].
#[blaze::noalloc]
#[blaze::nopanic]
pub fn write(fd: Fd, bytes: &[u8]) -> Result<usize, Errno> {
    // SAFETY: `bytes` is live memory of `bytes.len()` bytes the kernel only reads.
    raw::check(unsafe { raw::syscall3(nr::WRITE, arg(fd), bytes.as_ptr() as usize, bytes.len()) })
}

/// `write` of `len` bytes from the address `at`, which need not be readable: the kernel
/// copies from it and answers [`Errno::Fault`] where it is not mapped readable. The probe of
/// a stack walker (blaze-profile's `stack::probe`, into a pipe it drains), which asks the
/// kernel whether a frame address may be read before reading it. Safe: no byte of this
/// process is read or written by user code, and the kernel's copy is not a Rust access.
/// replaces libc::write (with an address that may be unreadable, a probe: blaze_sys::linux::write_from_address(fd, at, len); EFAULT is Err(Errno::Fault))
#[blaze::noalloc]
#[blaze::nopanic]
pub fn write_from_address(fd: Fd, at: usize, len: usize) -> Result<usize, Errno> {
    // SAFETY: the kernel checks `at..at + len` and copies from it only where readable; this
    // process's memory is not touched by user code.
    raw::check(unsafe { raw::syscall3(nr::WRITE, arg(fd), at, len) })
}

#[cfg(test)]
mod write_from_address_tests {
    use super::{PipeFlags, pipe2, read, write_from_address};
    use crate::Errno;

    /// A readable address is copied into the pipe; address 0 is `EFAULT`, nothing copied.
    #[test]
    fn an_unreadable_address_is_a_fault() {
        let (reader, writer) = pipe2(PipeFlags::CLOSE_ON_EXEC).unwrap();
        let bytes = *b"0123456789abcdef";
        assert_eq!(write_from_address(writer.fd(), bytes.as_ptr() as usize, 16), Ok(16));
        let mut back = [0u8; 16];
        assert_eq!(read(reader.fd(), &mut back), Ok(16));
        assert_eq!(back, bytes);
        assert_eq!(write_from_address(writer.fd(), 0, 16), Err(Errno::Fault));
    }
}

/// `pwrite64`: up to `bytes.len()` bytes at offset `at`. It may write fewer than asked (the
/// caller continues). Used by `Effect::WriteAt` and `Effect::Write`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn pwrite(fd: Fd, bytes: &[u8], at: u64) -> Result<usize, Errno> {
    // SAFETY: as `write`.
    raw::check(unsafe {
        raw::syscall4(nr::PWRITE64, arg(fd), bytes.as_ptr() as usize, bytes.len(), at as usize)
    })
}

/// `fsync`: the file's data and metadata on the device. Waits for the device. Used by
/// `Effect::Sync`, the temporary file of `Effect::Write`, and a directory's names.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn fsync(fd: Fd) -> Result<(), Errno> {
    // SAFETY: the call takes a number and no memory.
    raw::check(unsafe { raw::syscall1(nr::FSYNC, arg(fd)) }).map(|_| ())
}

/// `fdatasync`: the file's data, and the metadata a read of it needs, on the device. Waits
/// for the device. Used by the journal's appends.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn fdatasync(fd: Fd) -> Result<(), Errno> {
    // SAFETY: the call takes a number and no memory.
    raw::check(unsafe { raw::syscall1(nr::FDATASYNC, arg(fd)) }).map(|_| ())
}

/// What an [`lseek`] looks for from its offset.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Whence {
    /// `SEEK_DATA`: the first offset at or after the one given that holds data.
    Data = 3,
    /// `SEEK_HOLE`: the first offset at or after the one given that is in a hole; a file's
    /// end counts as one.
    Hole = 4,
}

/// `lseek` with `SEEK_DATA` or `SEEK_HOLE`: where the file's next data, or its next hole,
/// begins at or after `at`. [`Errno::Nxio`] when there is none: `at` is at or past the
/// file's end, or (for data) only a hole lies between `at` and the end. A file system that
/// keeps no record of holes says every byte is data. It moves the descriptor's position,
/// which nothing that uses `pread`, `pwrite` or [`copy_file_range`] reads. Used by
/// `Effect::CopyData` to copy a file's data and none of its holes.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn lseek(fd: Fd, at: u64, whence: Whence) -> Result<u64, Errno> {
    // SAFETY: the call takes numbers and no memory.
    raw::check(unsafe { raw::syscall3(nr::LSEEK, arg(fd), at as usize, whence as usize) })
        .map(|found| found as u64)
}

/// `copy_file_range`: up to `len` bytes of `from` at `from_at` copied by the kernel to `to`
/// at `to_at`, through no memory of this process; the bytes copied, which may be fewer (0 at
/// `from`'s end), so the caller loops. Neither descriptor's position moves, as both offsets
/// are given. Between two files of one file system on every kernel that has the call (4.5),
/// and across file systems from 5.3; [`Errno::XDev`] before that. A file system that can
/// share blocks may share them instead of copying. Used by `Effect::CopyData`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn copy_file_range(from: Fd, from_at: u64, to: Fd, to_at: u64, len: u64) -> Result<usize, Errno> {
    let (mut from_at, mut to_at) = (from_at as i64, to_at as i64);
    // SAFETY: the two offsets are live `loff_t`s of this frame the kernel reads and advances
    // during the call and does not keep; the rest are numbers. Flags are 0, the only value.
    raw::check(unsafe {
        raw::syscall6(
            nr::COPY_FILE_RANGE,
            arg(from),
            core::ptr::from_mut(&mut from_at) as usize,
            arg(to),
            core::ptr::from_mut(&mut to_at) as usize,
            len as usize,
            0,
        )
    })
}

flag_set! {
    /// `sync_file_range`'s flags.
    /// replaces libc::SYNC_FILE_RANGE_WAIT_BEFORE (blaze_sys::linux::SyncRange::WAIT_BEFORE)
    /// replaces libc::SYNC_FILE_RANGE_WRITE (blaze_sys::linux::SyncRange::WRITE)
    /// replaces libc::SYNC_FILE_RANGE_WAIT_AFTER (blaze_sys::linux::SyncRange::WAIT_AFTER)
    SyncRange: u32 {
        /// `SYNC_FILE_RANGE_WAIT_BEFORE`.
        WAIT_BEFORE = 1;
        /// `SYNC_FILE_RANGE_WRITE`: begin writeback of the range's dirty pages; waits for
        /// nothing.
        WRITE = 2;
        /// `SYNC_FILE_RANGE_WAIT_AFTER`.
        WAIT_AFTER = 4;
    }
}

/// `sync_file_range`: writeback of `len` bytes from `at` (to the end for 0) begun or waited
/// for, as `flags` say. It makes nothing durable by itself (no metadata, no cache flush).
/// With [`SyncRange::WRITE`] alone it is how writeback is started with no wait: all-1110's
/// `Effect::WriteBack` and bstd's `Durable::Started`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn sync_file_range(fd: Fd, at: u64, len: u64, flags: SyncRange) -> Result<(), Errno> {
    // SAFETY: the call takes numbers and no memory. On both architectures the arguments are
    // (fd, offset, nbytes, flags), each 64-bit value in one register.
    raw::check(unsafe {
        raw::syscall4(nr::SYNC_FILE_RANGE, arg(fd), at as usize, len as usize, flags.bits() as usize)
    })
    .map(|_| ())
}

/// `ftruncate`: the file cut, or grown with zeros, to `len` bytes. Used by
/// `Effect::Truncate`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn ftruncate(fd: Fd, len: u64) -> Result<(), Errno> {
    // SAFETY: the call takes numbers and no memory.
    raw::check(unsafe { raw::syscall2(nr::FTRUNCATE, arg(fd), len as usize) }).map(|_| ())
}

flag_set! {
    /// `renameat2`'s flags.
    /// replaces libc::RENAME_NOREPLACE (blaze_sys::linux::RenameFlags::NO_REPLACE)
    /// replaces libc::RENAME_EXCHANGE (blaze_sys::linux::RenameFlags::EXCHANGE)
    RenameFlags: u32 {
        /// `RENAME_NOREPLACE`: fail with [`Errno::Exist`] where the new name is taken.
        NO_REPLACE = 1;
        /// `RENAME_EXCHANGE`: swap the two names.
        EXCHANGE = 2;
    }
}

/// `renameat2`: the name `from` (in `from_dir`) made the name `to` (in `to_dir`), replacing
/// what was there unless `flags` say otherwise. It syncs nothing. Used by `Effect::Rename`,
/// the commit of `Effect::Write`, and all-1110's `RenameUnsynced`.
/// replaces libc::syscall(libc::SYS_renameat2) (as blaze_sys::linux::renameat2 with RenameFlags; returns Result<(), Errno>)
/// replaces libc::SYS_renameat2
/// replaces libc::syscall(libc::SYS_renameat) (renameat2 with no RenameFlags is renameat)
/// replaces libc::SYS_renameat
#[blaze::noalloc]
#[blaze::nopanic]
pub fn renameat2(
    from_dir: Fd,
    from: Path<'_>,
    to_dir: Fd,
    to: Path<'_>,
    flags: RenameFlags,
) -> Result<(), Errno> {
    // SAFETY: both paths are live NUL-terminated strings the kernel reads and does not keep.
    raw::check(unsafe {
        raw::syscall5(
            nr::RENAMEAT2,
            arg(from_dir),
            from.as_ptr() as usize,
            arg(to_dir),
            to.as_ptr() as usize,
            flags.bits() as usize,
        )
    })
    .map(|_| ())
}

/// What `unlinkat` removes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unlink {
    /// A name that is no directory.
    File,
    /// An empty directory (`AT_REMOVEDIR`).
    Dir,
}

/// `unlinkat`: the name `path` (in `dir`) removed. Used by `Effect::Remove` and the cleanup
/// of a failed `Effect::Write`.
/// replaces libc::syscall(libc::SYS_unlinkat) (as blaze_sys::linux::unlinkat(dir: Fd, Path, Unlink::File or Unlink::Dir for AT_REMOVEDIR); returns Result<(), Errno>)
/// replaces libc::SYS_unlinkat
#[blaze::noalloc]
#[blaze::nopanic]
pub fn unlinkat(dir: Fd, path: Path<'_>, what: Unlink) -> Result<(), Errno> {
    // SAFETY: `path` is a live NUL-terminated string the kernel reads and does not keep.
    raw::check(unsafe { raw::syscall3(nr::UNLINKAT, arg(dir), path.as_ptr() as usize, what.bits() as usize) })
        .map(|_| ())
}

impl Unlink {
    /// The `unlinkat` flags' bits: 0, or `AT_REMOVEDIR` (0x200) for [`Unlink::Dir`]. For a
    /// caller that writes them itself, as an io_uring `IORING_OP_UNLINKAT` entry's
    /// `unlink_flags`.
    /// replaces libc::AT_REMOVEDIR (Linux, as raw bits: blaze_sys::linux::Unlink::Dir.bits(), a u32; `Unlink::File.bits()` is the 0 beside it)
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn bits(self) -> u32 {
        match self {
            Unlink::File => 0,
            Unlink::Dir => 0x200,
        }
    }
}

#[cfg(test)]
mod unlink_bits_tests {
    use super::Unlink;

    /// `Unlink::Dir`'s bits are libc's `AT_REMOVEDIR`.
    #[test]
    fn unlink_dir_is_at_removedir() {
        assert_eq!(Unlink::Dir.bits(), libc::AT_REMOVEDIR as u32);
        assert_eq!(Unlink::File.bits(), 0);
    }
}

/// `MAX_HANDLE_SZ` (linux/fcntl.h): the most bytes a file handle has.
/// replaces libc::MAX_HANDLE_SZ (a usize here)
pub const MAX_HANDLE_SZ: usize = 128;

/// `struct file_handle` (linux/fcntl.h) with room for the largest handle (136 bytes): a file
/// named by its file system's own key, which outlives every name and descriptor of it. What
/// fanotify's `FAN_REPORT_FID` events carry and [`open_by_handle_at`] opens.
/// replaces libc::file_handle (with the flexible array as MAX_HANDLE_SZ bytes)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct FileHandle {
    /// `handle_bytes`: how many of `f_handle`'s bytes are the handle.
    pub handle_bytes: u32,
    /// `handle_type`: the file system's kind of handle.
    pub handle_type: i32,
    /// `f_handle`: the handle's bytes.
    pub f_handle: [u8; MAX_HANDLE_SZ],
}

// linux/fcntl.h, the same on x86-64 and aarch64; `f_handle` is a flexible array at 8.
const _: () = {
    assert!(size_of::<FileHandle>() == 8 + MAX_HANDLE_SZ);
    assert!(align_of::<FileHandle>() == 4);
    assert!(core::mem::offset_of!(FileHandle, handle_bytes) == 0);
    assert!(core::mem::offset_of!(FileHandle, handle_type) == 4);
    assert!(core::mem::offset_of!(FileHandle, f_handle) == 8);
};

impl FileHandle {
    /// No handle, with room for the largest: what [`name_to_handle_at`] fills.
    pub const EMPTY: FileHandle =
        FileHandle { handle_bytes: MAX_HANDLE_SZ as u32, handle_type: 0, f_handle: [0; MAX_HANDLE_SZ] };

    /// The handle of kind `handle_type` whose bytes are `bytes` (an event's, a record's).
    /// [`Errno::Overflow`] for more than [`MAX_HANDLE_SZ`] bytes.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn new(handle_type: i32, bytes: &[u8]) -> Result<FileHandle, Errno> {
        let mut handle = FileHandle { handle_bytes: 0, handle_type, f_handle: [0; MAX_HANDLE_SZ] };
        let into = handle.f_handle.get_mut(..bytes.len()).ok_or(Errno::Overflow)?;
        into.copy_from_slice(bytes);
        // At most MAX_HANDLE_SZ (checked above): the cut keeps it.
        handle.handle_bytes = bytes.len() as u32;
        Ok(handle)
    }

    /// The handle's bytes.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn bytes(&self) -> &[u8] {
        let len = (self.handle_bytes as usize).min(MAX_HANDLE_SZ);
        self.f_handle.get(..len).unwrap_or(&[])
    }
}

/// `name_to_handle_at`: the handle of `path` in `dir` (with [`AtFlags::EMPTY_PATH`] and
/// [`Path::EMPTY`], of `dir` itself; [`AtFlags::FOLLOW`] follows a final symbolic link) and
/// the id of the mount it is on. No descriptor is opened, so a directory the process may only
/// pass through has one too. [`Errno::OpNotSupp`] on a file system without handles. For
/// blaze-core's fanotify watch, which keys directories by handle.
/// replaces libc::name_to_handle_at
/// replaces libc::syscall(libc::SYS_name_to_handle_at) (as blaze_sys::linux::name_to_handle_at(dir: Fd, Path, AtFlags); returns Result<(FileHandle, mount_id: i32), Errno>, the site's nonzero check becomes the Err)
/// replaces libc::SYS_name_to_handle_at (the call; the number as data stays nr::NAME_TO_HANDLE_AT)
#[blaze::noalloc]
#[blaze::nopanic]
pub fn name_to_handle_at(dir: Fd, path: Path<'_>, flags: AtFlags) -> Result<(FileHandle, i32), Errno> {
    let mut handle = FileHandle::EMPTY;
    let mut mount_id: i32 = 0;
    // SAFETY: `path` is a live NUL-terminated string the kernel reads; `handle` is a live
    // `file_handle` whose `handle_bytes` says the room after its header (all of `f_handle`),
    // which the kernel writes at most; `mount_id` a live `int` it writes; it keeps none.
    raw::check(unsafe {
        raw::syscall5(
            nr::NAME_TO_HANDLE_AT,
            arg(dir),
            path.as_ptr() as usize,
            core::ptr::from_mut(&mut handle) as usize,
            core::ptr::from_mut(&mut mount_id) as usize,
            flags.bits() as usize,
        )
    })?;
    Ok((handle, mount_id))
}

/// `open_by_handle_at`: the file `handle` names, on the file system `mount` (any descriptor
/// of it) is on, opened with `flags` (as [`openat`]'s; `OpenFlags::PATH` opens no content).
/// [`Errno::Stale`] for a file removed since; [`Errno::Perm`] without
/// `CAP_DAC_READ_SEARCH`. For blaze-core's fanotify watch.
/// replaces libc::open_by_handle_at
/// replaces libc::syscall(libc::SYS_open_by_handle_at) (as blaze_sys::linux::open_by_handle_at(mount: Fd, &FileHandle, OpenFlags); returns Result<OwnedFd, Errno>, the site's negative check becomes the Err)
/// replaces libc::SYS_open_by_handle_at (the call; the number as data stays nr::OPEN_BY_HANDLE_AT)
#[blaze::noalloc]
#[blaze::nopanic]
pub fn open_by_handle_at(mount: Fd, handle: &FileHandle, flags: OpenFlags) -> Result<OwnedFd, Errno> {
    // SAFETY: `handle` is a live `file_handle` of `handle_bytes` bytes after its header
    // (`FileHandle::new` and `name_to_handle_at` keep it in the array), which the kernel only
    // reads and does not keep; the result is a new descriptor.
    owned(unsafe {
        raw::syscall3(nr::OPEN_BY_HANDLE_AT, arg(mount), core::ptr::from_ref(handle) as usize, flags.bits() as usize)
    })
}

#[cfg(test)]
mod file_handle_tests {
    use super::{AtFlags, CWD, FileHandle, MAX_HANDLE_SZ, OpenFlags, name_to_handle_at, open_by_handle_at};
    use crate::{Errno, Path};

    /// The struct is libc's `file_handle` with its array given room, `MAX_HANDLE_SZ` is
    /// libc's, and a directory's handle opens again where this process may (or is refused
    /// for want of `CAP_DAC_READ_SEARCH`).
    #[test]
    fn a_handle_is_taken_and_opened() {
        assert_eq!(MAX_HANDLE_SZ, libc::MAX_HANDLE_SZ as usize);
        assert_eq!(core::mem::offset_of!(FileHandle, f_handle), size_of::<libc::file_handle>());
        assert_eq!(FileHandle::new(1, &[0; MAX_HANDLE_SZ + 1]), Err(Errno::Overflow));
        let (handle, _mount) = match name_to_handle_at(CWD, Path::new(c"/"), AtFlags::FOLLOW) {
            Ok(it) => it,
            Err(Errno::OpNotSupp | Errno::NoSys) => {
                eprintln!("skipped: this file system or filter has no file handles");
                return;
            }
            Err(other) => panic!("name_to_handle_at(/): {other}"),
        };
        assert!(!handle.bytes().is_empty());
        let again = FileHandle::new(handle.handle_type, handle.bytes()).unwrap();
        let root = super::openat(CWD, Path::new(c"/"), OpenFlags::DIRECTORY | OpenFlags::CLOSE_ON_EXEC, 0).unwrap();
        match open_by_handle_at(root.fd(), &again, OpenFlags::PATH | OpenFlags::CLOSE_ON_EXEC) {
            Ok(opened) => assert!(opened.fd().raw() >= 0),
            Err(Errno::Perm | Errno::BadF) => eprintln!("open_by_handle_at refused: no CAP_DAC_READ_SEARCH"),
            Err(other) => panic!("open_by_handle_at: {other}"),
        }
    }
}

/// `mkdirat`: the directory `path` (in `dir`) made with the permission bits `mode`. Used by
/// the parents a file effect makes, and `Effect::MakeDirUnder`.
/// replaces libc::syscall(libc::SYS_mkdirat) (as blaze_sys::linux::mkdirat(dir: Fd, Path, mode); returns Result<(), Errno>)
/// replaces libc::SYS_mkdirat
#[blaze::noalloc]
#[blaze::nopanic]
pub fn mkdirat(dir: Fd, path: Path<'_>, mode: u32) -> Result<(), Errno> {
    // SAFETY: `path` is a live NUL-terminated string the kernel reads and does not keep.
    raw::check(unsafe {
        raw::syscall3(nr::MKDIRAT, arg(dir), path.as_ptr() as usize, mode as usize)
    })
    .map(|_| ())
}

flag_set! {
    /// The `AT_*` flags of the calls on a name beneath a descriptor: `statx`, `linkat`,
    /// `utimensat`, `execveat`. Each call's doc says which it takes.
    /// replaces libc::AT_EMPTY_PATH (blaze_sys::linux::AtFlags::EMPTY_PATH, with blaze_sys::Path::EMPTY; Linux only)
    AtFlags: u32 {
        /// `AT_SYMLINK_NOFOLLOW`: a symbolic link itself, not what it names.
        NO_FOLLOW = 0x100;
        /// `AT_SYMLINK_FOLLOW`: `linkat` links what a symbolic link names, not the link.
        FOLLOW = 0x400;
        /// `AT_EMPTY_PATH`: with [`Path::EMPTY`], the descriptor itself.
        EMPTY_PATH = 0x1000;
    }
}

flag_set! {
    /// Which fields a `statx` asks for, and which the kernel filled.
    /// replaces libc::STATX_INO (blaze_sys::linux::StatxMask::INO)
    /// replaces libc::STATX_SIZE (blaze_sys::linux::StatxMask::SIZE)
    /// replaces libc::STATX_MTIME (blaze_sys::linux::StatxMask::MTIME)
    /// replaces libc::STATX_CTIME (blaze_sys::linux::StatxMask::CTIME)
    /// replaces libc::STATX_BTIME (blaze_sys::linux::StatxMask::BTIME)
    /// replaces libc::STATX_BASIC_STATS (blaze_sys::linux::StatxMask::BASIC)
    StatxMask: u32 {
        /// `STATX_TYPE`.
        TYPE = 0x001;
        /// `STATX_MODE`.
        MODE = 0x002;
        /// `STATX_NLINK`.
        NLINK = 0x004;
        /// `STATX_INO`.
        INO = 0x100;
        /// `STATX_SIZE`.
        SIZE = 0x200;
        /// `STATX_MTIME`.
        MTIME = 0x040;
        /// `STATX_CTIME`.
        CTIME = 0x080;
        /// `STATX_BASIC_STATS`: what `stat` gives.
        BASIC = 0x7ff;
        /// `STATX_MNT_ID`: the mount's id. replaces libc::STATX_MNT_ID
        MNT_ID = 0x1000;
        /// `STATX_WRITE_ATOMIC` (6.11): the atomic-write bounds. replaces libc::STATX_WRITE_ATOMIC
        WRITE_ATOMIC = 0x10000;
        /// `STATX_BTIME`.
        BTIME = 0x800;
    }
}

/// `struct statx_timestamp`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct StatxTime {
    /// Seconds since the epoch.
    pub sec: i64,
    /// Nanoseconds, below one second.
    pub nsec: u32,
    reserved: i32,
}

/// `struct statx`: 256 bytes, one layout on every architecture.
/// replaces libc::statx (the struct: blaze_sys::linux::Statx, returned by blaze_sys::linux::statx; the call: the same fn, `statx(fd, Path::EMPTY, AtFlags::EMPTY_PATH, mask) -> Result<Statx, Errno>`)
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Statx {
    /// Which fields the kernel filled.
    pub mask: u32,
    /// The preferred block size for IO.
    pub blksize: u32,
    /// The file's attributes (`STATX_ATTR_*`).
    pub attributes: u64,
    /// Its link count.
    pub nlink: u32,
    /// Its owner.
    pub uid: u32,
    /// Its group.
    pub gid: u32,
    /// Its type and permission bits (`S_IFMT` and the rest).
    pub mode: u16,
    spare0: u16,
    /// Its inode number.
    pub ino: u64,
    /// Its length in bytes.
    pub size: u64,
    /// The 512-byte blocks it holds.
    pub blocks: u64,
    /// Which of `attributes` the file system supports.
    pub attributes_mask: u64,
    /// When it was last read.
    pub atime: StatxTime,
    /// When it was made.
    pub btime: StatxTime,
    /// When its inode last changed.
    pub ctime: StatxTime,
    /// When its bytes last changed.
    pub mtime: StatxTime,
    /// The device it is, for a device file: major.
    pub rdev_major: u32,
    /// The device it is, for a device file: minor.
    pub rdev_minor: u32,
    /// The device it is on: major.
    pub dev_major: u32,
    /// The device it is on: minor.
    pub dev_minor: u32,
    /// The mount it is on.
    pub mnt_id: u64,
    /// The memory alignment direct IO needs.
    pub dio_mem_align: u32,
    /// The offset alignment direct IO needs.
    pub dio_offset_align: u32,
    /// The subvolume it is in (`STATX_SUBVOL`, 6.10).
    pub subvol: u64,
    /// The smallest write the file can take atomically (`STATX_WRITE_ATOMIC`, 6.11 for block
    /// devices, 6.13 for ext4 and xfs files; 0 where not said).
    pub atomic_write_unit_min: u32,
    /// The largest write the file can take atomically (`STATX_WRITE_ATOMIC`).
    pub atomic_write_unit_max: u32,
    /// The most segments an atomic write may have (`STATX_WRITE_ATOMIC`).
    pub atomic_write_segments_max: u32,
    /// `dio_read_offset_align` (6.14), `atomic_write_unit_max_opt` (6.16) and a spare: not read.
    spare2: [u32; 3],
    spare3: [u64; 8],
}

// The kernel's `struct statx` (include/uapi/linux/stat.h): 256 bytes, the atomic-write words at
// 0xa8..0xb4 after `stx_subvol` at 0xa0.
const _: () = {
    assert!(size_of::<Statx>() == 256);
    assert!(core::mem::offset_of!(Statx, mnt_id) == 0x90);
    assert!(core::mem::offset_of!(Statx, subvol) == 0xa0);
    assert!(core::mem::offset_of!(Statx, atomic_write_unit_min) == 0xa8);
    assert!(core::mem::offset_of!(Statx, atomic_write_unit_max) == 0xac);
    assert!(core::mem::offset_of!(Statx, atomic_write_segments_max) == 0xb0);
};

impl StatxTime {
    /// The epoch: every field zero.
    const ZERO: StatxTime = StatxTime { sec: 0, nsec: 0, reserved: 0 };
}

impl Statx {
    /// Every field zero: what [`statx`] hands the kernel to fill, so a field the kernel
    /// does not write (its mask says which it did) reads as 0.
    const ZERO: Statx = Statx {
        mask: 0,
        blksize: 0,
        attributes: 0,
        nlink: 0,
        uid: 0,
        gid: 0,
        mode: 0,
        spare0: 0,
        ino: 0,
        size: 0,
        blocks: 0,
        attributes_mask: 0,
        atime: StatxTime::ZERO,
        btime: StatxTime::ZERO,
        ctime: StatxTime::ZERO,
        mtime: StatxTime::ZERO,
        rdev_major: 0,
        rdev_minor: 0,
        dev_major: 0,
        dev_minor: 0,
        mnt_id: 0,
        dio_mem_align: 0,
        dio_offset_align: 0,
        subvol: 0,
        atomic_write_unit_min: 0,
        atomic_write_unit_max: 0,
        atomic_write_segments_max: 0,
        spare2: [0; 3],
        spare3: [0; 8],
    };

    /// `S_IFMT`: the bits of `mode` that are the file's type.
    pub const TYPE_MASK: u16 = 0o170000;
    /// `S_IFDIR`.
    pub const DIR: u16 = 0o040000;
    /// `S_IFREG`.
    pub const REGULAR: u16 = 0o100000;
    /// `S_IFLNK`.
    pub const LINK: u16 = 0o120000;

    /// Whether it is a directory.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn is_dir(&self) -> bool {
        self.mode & Statx::TYPE_MASK == Statx::DIR
    }
}

/// `statx`: what `path` (from `dir`) is. Used by `Effect::Stat`, `Effect::Lstat`
/// ([`AtFlags::NO_FOLLOW`]), `Effect::Stamp` and, with its inode and change time, the
/// generation a `Flushed` file carries.
/// replaces libc::syscall(libc::SYS_statx) (as blaze_sys::linux::statx(dir: Fd, Path, AtFlags, StatxMask); the struct statx is returned, not written through a pointer: Result<Statx, Errno>, the site's nonzero check becomes the Err; AT_STATX_DONT_SYNC has no AtFlags bit yet)
/// replaces libc::SYS_statx
#[blaze::noalloc]
#[blaze::nopanic]
pub fn statx(dir: Fd, path: Path<'_>, flags: AtFlags, mask: StatxMask) -> Result<Statx, Errno> {
    let mut stat = Statx::ZERO;
    // SAFETY: `path` is a live NUL-terminated string the kernel reads; `stat` is 256 live
    // bytes (integers only, any bytes valid) it writes a `struct statx` into; it keeps
    // neither.
    raw::check(unsafe {
        raw::syscall5(
            nr::STATX,
            arg(dir),
            path.as_ptr() as usize,
            flags.bits() as usize,
            mask.bits() as usize,
            core::ptr::from_mut(&mut stat) as usize,
        )
    })?;
    Ok(stat)
}

/// What a directory entry is (`d_type`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntryKind {
    /// `DT_UNKNOWN`: the file system does not say; a `statx` does.
    Unknown,
    /// `DT_DIR`.
    Dir,
    /// `DT_REG`.
    Regular,
    /// `DT_LNK`.
    Link,
    /// A FIFO, a device or a socket, with its `DT_*` number.
    Other(u8),
}

/// One directory entry of a [`getdents64`] buffer.
#[derive(Clone, Copy, Debug)]
pub struct Entry<'a> {
    /// The entry's inode number.
    pub ino: u64,
    /// What it is.
    pub kind: EntryKind,
    /// Its name, without the NUL; `.` and `..` are among them.
    pub name: &'a [u8],
}

/// The entries one [`getdents64`] filled, in the kernel's order.
#[derive(Clone, Copy, Debug)]
pub struct Entries<'a> {
    left: &'a [u8],
}

/// Where a `linux_dirent64`'s name starts: after `d_ino` (8), `d_off` (8), `d_reclen` (2) and
/// `d_type` (1).
const DIRENT_NAME: usize = 19;

impl<'a> Iterator for Entries<'a> {
    type Item = Entry<'a>;

    /// The next entry. A record that does not fit what is left ends the walk: the kernel
    /// writes whole records, so that is the end of the buffer.
    fn next(&mut self) -> Option<Entry<'a>> {
        let ino = u64::from_ne_bytes(*self.left.first_chunk::<8>()?);
        let reclen = u16::from_ne_bytes(*self.left.get(16..18)?.first_chunk::<2>()?) as usize;
        let kind = match *self.left.get(18)? {
            0 => EntryKind::Unknown,
            4 => EntryKind::Dir,
            8 => EntryKind::Regular,
            10 => EntryKind::Link,
            other => EntryKind::Other(other),
        };
        let record = self.left.get(..reclen)?;
        let padded = record.get(DIRENT_NAME..)?;
        let len = padded.iter().position(|&byte| byte == 0)?;
        let name = padded.get(..len)?;
        self.left = self.left.get(reclen..)?;
        Some(Entry { ino, kind, name })
    }
}

/// `getdents64`: the next entries of the directory `fd`, as many as fit `into`; an empty
/// walk at the directory's end. [`Errno::Inval`] where `into` is too small for one entry.
/// Used by `Effect::List` and `Effect::Entries`.
/// replaces libc::syscall(libc::SYS_getdents64) (as blaze_sys::linux::getdents64(fd: Fd, &mut [u8]); returns Result<Entries, Errno>, an iterator of typed Entry records instead of a byte count to parse; an empty walk is the end, the site's 0 return)
/// replaces libc::SYS_getdents64
#[blaze::noalloc]
#[blaze::nopanic]
pub fn getdents64(fd: Fd, into: &mut [u8]) -> Result<Entries<'_>, Errno> {
    // SAFETY: `into` is live memory of `into.len()` bytes the kernel writes whole records
    // into, at most that many bytes.
    let filled = raw::check(unsafe {
        raw::syscall3(nr::GETDENTS64, arg(fd), into.as_mut_ptr() as usize, into.len())
    })?;
    Ok(Entries { left: into.get(..filled).unwrap_or(&[]) })
}

/// `FICLONE`: `_IOW(0x94, 9, int)`.
const FICLONE: usize = 0x4004_9409;
/// `FICLONERANGE`: `_IOW(0x94, 13, struct file_clone_range)`.
const FICLONERANGE: usize = 0x4020_940d;

/// `TCGETS` (asm-generic/ioctls.h): a terminal's settings, which only a terminal has.
const TCGETS: usize = 0x5401;
/// `TIOCGWINSZ` (asm-generic/ioctls.h): a terminal's size.
const TIOCGWINSZ: usize = 0x5413;
/// `struct termios` (asm-generic/termbits.h) is 60 bytes on every Linux target; read whole and
/// dropped.
const TERMIOS: usize = 60;

/// Whether `fd` is a terminal: `ioctl(TCGETS)` succeeds on one only (glibc's `isatty`). Used
/// by `bstd::out::Out::is_terminal`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn is_terminal(fd: Fd) -> bool {
    let mut termios = [0u8; TERMIOS];
    // SAFETY: TCGETS writes one `struct termios`, `TERMIOS` bytes, into the buffer given.
    raw::check(unsafe { raw::syscall3(nr::IOCTL, arg(fd), TCGETS, termios.as_mut_ptr() as usize) }).is_ok()
}

/// `ioctl(TIOCGWINSZ)`: the terminal `fd`'s size in cells; [`Errno::NoTty`] when `fd` is not a
/// terminal. Used by `bstd::out::Out::size`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn window_size(fd: Fd) -> Result<crate::WindowSize, Errno> {
    let mut size = crate::types::Winsize::default();
    // SAFETY: TIOCGWINSZ writes one `struct winsize` into the one given.
    raw::check(unsafe { raw::syscall3(nr::IOCTL, arg(fd), TIOCGWINSZ, core::ptr::from_mut(&mut size) as usize) })?;
    Ok(crate::WindowSize { cols: size.cols, rows: size.rows })
}

/// `ioctl(FICLONE)`: `to` made to share all of `from`'s blocks (a reflink). [`Errno::XDev`]
/// across file systems, [`Errno::OpNotSupp`] or [`Errno::Inval`] where the file system cannot
/// share blocks; nothing copies in its place. A source with dirty pages is written out first,
/// which is why bstd clones a `Flushed` file only. Used by `Effect::Clone`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn ficlone(to: Fd, from: Fd) -> Result<(), Errno> {
    // SAFETY: this ioctl takes the source's number as its argument, and no memory.
    raw::check(unsafe { raw::syscall3(nr::IOCTL, arg(to), FICLONE, arg(from)) }).map(|_| ())
}

/// `struct file_clone_range`.
/// replaces libc::file_clone_range (blaze_sys::linux::CloneRange, taken by blaze_sys::linux::ficlonerange)
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct CloneRange {
    src_fd: i64,
    src_offset: u64,
    src_length: u64,
    dest_offset: u64,
}

impl CloneRange {
    /// `len` bytes of `from` at `from_at` (to its end for 0), to land at `to_at`.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn new(from: Fd, from_at: u64, len: u64, to_at: u64) -> CloneRange {
        CloneRange { src_fd: from.raw() as i64, src_offset: from_at, src_length: len, dest_offset: to_at }
    }
}

/// `ioctl(FICLONERANGE)`: a range of another file's blocks shared into `to`, block-aligned.
/// Errors as [`ficlone`]. Used by a publish that derives a file from a base and its changed
/// ranges.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn ficlonerange(to: Fd, range: &CloneRange) -> Result<(), Errno> {
    // SAFETY: `range` is a live `file_clone_range` the kernel reads and does not keep.
    raw::check(unsafe {
        raw::syscall3(nr::IOCTL, arg(to), FICLONERANGE, core::ptr::from_ref(range) as usize)
    })
    .map(|_| ())
}

flag_set! {
    /// `pipe2`'s flags.
    PipeFlags: u32 {
        /// `O_NONBLOCK`.
        NONBLOCK = 0o4000;
        /// `O_CLOEXEC`.
        CLOSE_ON_EXEC = 0o2000000;
    }
}

/// `pipe2`: a pipe's read end and write end. Used by the stop pipes of the accept and watch
/// errands and a child's standard streams.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn pipe2(flags: PipeFlags) -> Result<(OwnedFd, OwnedFd), Errno> {
    let mut ends = [0i32; 2];
    // SAFETY: `ends` is two live `int`s the kernel writes.
    raw::check(unsafe { raw::syscall2(nr::PIPE2, ends.as_mut_ptr() as usize, flags.bits() as usize) })?;
    let [read, write] = ends;
    // SAFETY: the kernel returned two new descriptors to this call alone.
    Ok(unsafe { (OwnedFd::from_raw(read), OwnedFd::from_raw(write)) })
}

flag_set! {
    /// `eventfd2`'s flags.
    EventFlags: u32 {
        /// `EFD_SEMAPHORE`.
        SEMAPHORE = 1;
        /// `EFD_NONBLOCK`.
        NONBLOCK = 0o4000;
        /// `EFD_CLOEXEC`.
        CLOSE_ON_EXEC = 0o2000000;
    }
}

/// `eventfd2`: a counter that polls readable while it is not zero, starting at `initial`.
/// Used by the wake-ups of the io_uring driver and the watch errand.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn eventfd2(initial: u32, flags: EventFlags) -> Result<OwnedFd, Errno> {
    // SAFETY: the call takes numbers and no memory.
    owned(unsafe { raw::syscall2(nr::EVENTFD2, initial as usize, flags.bits() as usize) })
}

flag_set! {
    /// `getrandom`'s flags.
    RandomFlags: u32 {
        /// `GRND_NONBLOCK`: [`Errno::Again`] where the pool is not yet seeded, no wait.
        NONBLOCK = 1;
    }
}

/// `getrandom`: up to `into.len()` bytes from the kernel's generator; a request of 256 bytes
/// or fewer is filled whole once the pool is seeded. With no flag it waits for the seed at
/// boot, and not after. Used by the seed of `Io::rng` and of the maps' hasher.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn getrandom(into: &mut [u8], flags: RandomFlags) -> Result<usize, Errno> {
    // SAFETY: `into` is live memory of `into.len()` bytes the kernel writes at most that
    // many bytes of.
    raw::check(unsafe {
        raw::syscall3(nr::GETRANDOM, into.as_mut_ptr() as usize, into.len(), flags.bits() as usize)
    })
}

/// `linkat`: a second name `to` (in `to_dir`) for the file `from` (in `from_dir`). Takes
/// [`AtFlags::FOLLOW`] (link what a symbolic link names) and [`AtFlags::EMPTY_PATH`] (with
/// [`Path::EMPTY`], the descriptor `from_dir` itself: how an `O_TMPFILE` file gets its name).
/// [`Errno::Exist`] where `to` exists: nothing is replaced. For the store's hard-linked
/// checkouts and `Effect::Link`.
/// replaces libc::syscall(libc::SYS_linkat) (as blaze_sys::linux::linkat(from_dir: Fd, Path, to_dir: Fd, Path, AtFlags); returns Result<(), Errno>)
/// replaces libc::SYS_linkat
#[blaze::noalloc]
#[blaze::nopanic]
pub fn linkat(from_dir: Fd, from: Path<'_>, to_dir: Fd, to: Path<'_>, flags: AtFlags) -> Result<(), Errno> {
    // SAFETY: both paths are live NUL-terminated strings the kernel reads and does not keep.
    raw::check(unsafe {
        raw::syscall5(
            nr::LINKAT,
            arg(from_dir),
            from.as_ptr() as usize,
            arg(to_dir),
            to.as_ptr() as usize,
            flags.bits() as usize,
        )
    })
    .map(|_| ())
}

/// `symlinkat`: the symbolic link `link` (in `dir`) made, naming `target` as it is (never
/// resolved here). For a checkout's links and an archive's link entries (blaze-tar's reader).
#[blaze::noalloc]
#[blaze::nopanic]
pub fn symlinkat(target: Path<'_>, dir: Fd, link: Path<'_>) -> Result<(), Errno> {
    // SAFETY: both paths are live NUL-terminated strings the kernel reads and does not keep.
    raw::check(unsafe {
        raw::syscall3(nr::SYMLINKAT, target.as_ptr() as usize, arg(dir), link.as_ptr() as usize)
    })
    .map(|_| ())
}

/// `readlinkat`: what the symbolic link `path` (in `dir`; [`Path::EMPTY`] for `dir` itself, an
/// `O_PATH` descriptor of the link) names, written into `into` and lent back, no NUL added.
/// The kernel cuts a target longer than `into` without saying so, so a result that fills
/// `into` is refused as [`Errno::NameTooLong`]: the caller asks again with more room
/// (`PATH_MAX`, 4096, always fits), never with a cut name.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn readlinkat<'a>(dir: Fd, path: Path<'_>, into: &'a mut [u8]) -> Result<&'a [u8], Errno> {
    // SAFETY: `path` is a live NUL-terminated string the kernel reads; `into` is live memory of
    // `into.len()` bytes it writes at most that many bytes of; it keeps neither.
    let len = raw::check(unsafe {
        raw::syscall4(nr::READLINKAT, arg(dir), path.as_ptr() as usize, into.as_mut_ptr() as usize, into.len())
    })?;
    // The buffer, lent back for as long as the caller lent it.
    let into: &'a [u8] = into;
    if len >= into.len() {
        return Err(Errno::NameTooLong);
    }
    into.get(..len).ok_or(Errno::NameTooLong)
}

/// `fchmod`: the permission bits of the open file `fd` set to `mode` (`0o7777` at most). For a
/// file an effect wrote whose mode a record names (an executable, an archive's entry).
#[blaze::noalloc]
#[blaze::nopanic]
pub fn fchmod(fd: Fd, mode: u32) -> Result<(), Errno> {
    // SAFETY: the call takes numbers and no memory.
    raw::check(unsafe { raw::syscall2(nr::FCHMOD, arg(fd), mode as usize) }).map(|_| ())
}

/// One of the two times a [`utimensat`] sets.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileTime {
    /// The time now on the kernel's clock (`UTIME_NOW`).
    Now,
    /// Left as it is (`UTIME_OMIT`).
    Keep,
    /// This time since 1970.
    At(Timespec),
}

/// `UTIME_NOW` and `UTIME_OMIT` (linux/stat.h): the nanoseconds that mean them.
const UTIME_NOW: i64 = (1 << 30) - 1;
const UTIME_OMIT: i64 = (1 << 30) - 2;

impl FileTime {
    #[blaze::noalloc]
    #[blaze::nopanic]
    const fn timespec(self) -> Timespec {
        match self {
            FileTime::Now => Timespec { sec: 0, nsec: UTIME_NOW },
            FileTime::Keep => Timespec { sec: 0, nsec: UTIME_OMIT },
            FileTime::At(at) => at,
        }
    }
}

/// `utimensat`: the access and modification times of `path` (in `dir`; [`Path::EMPTY`] with
/// [`AtFlags::EMPTY_PATH`] for `dir` itself) set. Takes [`AtFlags::NO_FOLLOW`] (a symbolic
/// link's own times) and [`AtFlags::EMPTY_PATH`]. [`Errno::Inval`] for an `At` time whose
/// nanoseconds are a second or more. For a checkout's mtimes, which a build script's
/// `rerun-if-changed` reads, and an archive's entries.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn utimensat(dir: Fd, path: Path<'_>, access: FileTime, modified: FileTime, flags: AtFlags) -> Result<(), Errno> {
    let times = [access.timespec(), modified.timespec()];
    // SAFETY: `path` is a live NUL-terminated string and `times` two live `struct timespec`s
    // (the layout of `Timespec`); the kernel reads both and keeps neither.
    raw::check(unsafe {
        raw::syscall4(nr::UTIMENSAT, arg(dir), path.as_ptr() as usize, times.as_ptr() as usize, flags.bits() as usize)
    })
    .map(|_| ())
}

/// `struct statfs` (asm-generic/statfs.h with 64-bit words: x86-64 and aarch64 alike), what
/// [`fstatfs`] fills.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct Statfs {
    /// `f_type`: the file system's magic number (`EXT4_SUPER_MAGIC`, `TMPFS_MAGIC`, ...).
    pub kind: i64,
    /// `f_bsize`: the best transfer size.
    pub block_size: i64,
    /// `f_blocks`: the blocks of `fragment_size` it has.
    pub blocks: u64,
    /// `f_bfree`.
    pub blocks_free: u64,
    /// `f_bavail`: free blocks an unprivileged writer may use.
    pub blocks_available: u64,
    /// `f_files`: its inodes.
    pub files: u64,
    /// `f_ffree`.
    pub files_free: u64,
    /// `f_fsid`: the file system's id, two words.
    pub fsid: [i32; 2],
    /// `f_namelen`: the longest name.
    pub name_max: i64,
    /// `f_frsize`: the fragment size.
    pub fragment_size: i64,
    /// `f_flags`: the mount's `ST_*` flags.
    pub flags: i64,
    spare: [i64; 4],
}

impl Statfs {
    /// All zero, for the kernel to fill.
    pub const ZERO: Statfs = Statfs {
        kind: 0,
        block_size: 0,
        blocks: 0,
        blocks_free: 0,
        blocks_available: 0,
        files: 0,
        files_free: 0,
        fsid: [0; 2],
        name_max: 0,
        fragment_size: 0,
        flags: 0,
        spare: [0; 4],
    };
}

/// `fstatfs`: the file system the open file `fd` is on. For blaze-core's fanotify watcher (the
/// file system's id that its events name) and the store's choice of clone or copy by file
/// system kind.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn fstatfs(fd: Fd) -> Result<Statfs, Errno> {
    let mut stat = Statfs::ZERO;
    // SAFETY: `stat` is a live `struct statfs` (integers only, any bytes valid) the kernel
    // writes and does not keep.
    raw::check(unsafe { raw::syscall2(nr::FSTATFS, arg(fd), core::ptr::from_mut(&mut stat) as usize) })?;
    Ok(stat)
}

flag_set! {
    /// `flock`'s operation: one of `SHARED`, `EXCLUSIVE` or `UNLOCK`, and `NONBLOCK` beside it.
    /// replaces libc::LOCK_SH (blaze_sys::linux::LockOp::SHARED; Linux only, macOS has no flock item yet)
    /// replaces libc::LOCK_EX (blaze_sys::linux::LockOp::EXCLUSIVE; Linux only)
    /// replaces libc::LOCK_NB (blaze_sys::linux::LockOp::NONBLOCK; Linux only)
    /// replaces libc::LOCK_UN (blaze_sys::linux::LockOp::UNLOCK; Linux only)
    LockOp: u32 {
        /// `LOCK_SH`.
        SHARED = 1;
        /// `LOCK_EX`.
        EXCLUSIVE = 2;
        /// `LOCK_NB`: [`Errno::Again`] where another holds the lock, no wait.
        NONBLOCK = 4;
        /// `LOCK_UN`.
        UNLOCK = 8;
    }
}

/// `flock`: the advisory lock on the open file `fd` taken or dropped. Without
/// [`LockOp::NONBLOCK`] it waits for the holder with no deadline, so blaze's callers always
/// pass it and wait in their own bounded loop (rule 44). For the store's and a home's lock
/// files.
/// replaces libc::flock (blaze_sys::linux::flock(fd, LockOp) -> Result<(), Errno>; Linux only, macOS has no item yet)
#[blaze::noalloc]
#[blaze::nopanic]
pub fn flock(fd: Fd, op: LockOp) -> Result<(), Errno> {
    // SAFETY: the call takes numbers and no memory.
    raw::check(unsafe { raw::syscall2(nr::FLOCK, arg(fd), op.bits() as usize) }).map(|_| ())
}
