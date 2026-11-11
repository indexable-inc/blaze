//! Files and names: what the file effects and the beneath-only opens use.

use core::mem::MaybeUninit;

use super::{nr, raw};
use crate::{Errno, Fd, Path};

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
#[blaze::noalloc]
#[blaze::nopanic]
pub fn openat(dir: Fd, path: Path<'_>, flags: OpenFlags, mode: u32) -> Result<OwnedFd, Errno> {
    // SAFETY: `path` is a live NUL-terminated string the kernel reads and does not keep.
    owned(unsafe {
        raw::syscall4(nr::OPENAT, arg(dir), path.as_ptr() as usize, flags.bits() as usize, mode as usize)
    })
}

/// `openat2`: `path` opened from `dir` as `how` says. [`Errno::NoSys`] before Linux 5.6.
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

flag_set! {
    /// `sync_file_range`'s flags.
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
#[blaze::noalloc]
#[blaze::nopanic]
pub fn unlinkat(dir: Fd, path: Path<'_>, what: Unlink) -> Result<(), Errno> {
    let flags: usize = match what {
        Unlink::File => 0,
        Unlink::Dir => 0x200,
    };
    // SAFETY: `path` is a live NUL-terminated string the kernel reads and does not keep.
    raw::check(unsafe { raw::syscall3(nr::UNLINKAT, arg(dir), path.as_ptr() as usize, flags) })
        .map(|_| ())
}

/// `mkdirat`: the directory `path` (in `dir`) made with the permission bits `mode`. Used by
/// the parents a file effect makes, and `Effect::MakeDirUnder`.
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
    /// The `AT_*` flags `statx` takes.
    AtFlags: u32 {
        /// `AT_SYMLINK_NOFOLLOW`: a symbolic link itself, not what it names.
        NO_FOLLOW = 0x100;
        /// `AT_EMPTY_PATH`: with [`Path::EMPTY`], the descriptor itself.
        EMPTY_PATH = 0x1000;
    }
}

flag_set! {
    /// Which fields a `statx` asks for, and which the kernel filled.
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
    spare3: [u64; 12],
}

impl Statx {
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
#[blaze::noalloc]
#[blaze::nopanic]
pub fn statx(dir: Fd, path: Path<'_>, flags: AtFlags, mask: StatxMask) -> Result<Statx, Errno> {
    let mut stat = MaybeUninit::<Statx>::zeroed();
    // SAFETY: `path` is a live NUL-terminated string the kernel reads; `stat` is 256 live
    // bytes it writes a `struct statx` into; it keeps neither.
    raw::check(unsafe {
        raw::syscall5(
            nr::STATX,
            arg(dir),
            path.as_ptr() as usize,
            flags.bits() as usize,
            mask.bits() as usize,
            stat.as_mut_ptr() as usize,
        )
    })?;
    // SAFETY: zeroed bytes are a valid `Statx` (integers only), and the kernel filled it.
    Ok(unsafe { stat.assume_init() })
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
