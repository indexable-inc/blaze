//! A file's status and the calls on names, modes and attributes the tree makes (on Linux by
//! system call, `crate::linux::task`; on macOS through libSystem): `struct stat` and `fstat`/`fstatat`, the `S_IF*` bits and device numbers,
//! `struct statfs`, macOS's `getattrlistbulk`, clones and `fcntl` structs, Linux's
//! `posix_fadvise`, `posix_fallocate` and `mount`, the extended attributes, and the typed calls
//! the shared (`cfg(unix)`) code uses on both systems: [`stat_fd`], [`stat_at`],
//! [`change_mode`], [`rename_at`], [`unlink_at`].

#[cfg(target_os = "macos")]
use core::ffi::{c_char, c_void};
use core::ffi::{c_int, c_uint};

#[cfg(target_os = "macos")]
use super::said;
use crate::{
    Errno, Fd, Path,
    posix::types::{dev_t, gid_t, mode_t, off_t, uid_t},
};

// ---- struct stat ------------------------------------------------------------------------

/// `struct stat` on Linux: the kernel's own (`crate::linux::task::Stat`, the bytes glibc's
/// has on both architectures, asserted there), what `newfstatat` fills.
/// replaces libc::stat
#[cfg(target_os = "linux")]
#[allow(non_camel_case_types, reason = "the C type's name, so a port is a change of path")]
pub type stat = crate::linux::task::Stat;

/// The calls on a file's status, names and owners by system call on Linux: `fstat` and
/// `fstatat` (`newfstatat`), `mknodat` and `mkfifo` (by `mknodat`), `fchownat` and `fchown`
/// (by `fchownat`), and `realpath` (an `O_PATH` open, then its link under `/proc/self/fd`:
/// procfs needed, [`RealPathError`] says which step failed).
/// replaces libc::fstat
/// replaces libc::fstatat
/// replaces libc::mkfifo
/// replaces libc::fchown
/// replaces libc::realpath
#[cfg(target_os = "linux")]
pub use crate::linux::task::{RealPathError, fchown, fchownat, fstat, fstatat, mkfifo, mknodat, realpath};

/// `struct stat` (sys/stat.h, macOS arm64: the 64-bit-inode form, `__DARWIN_STRUCT_STAT64`):
/// what `fstat` and `fstatat` fill.
/// replaces libc::stat
#[cfg(target_os = "macos")]
#[allow(non_camel_case_types, reason = "the C type's name, so a port is a change of path")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct stat {
    /// `st_dev`: the device the file is on.
    pub st_dev: dev_t,
    /// `st_mode`: its kind (`S_IFMT`) and permissions.
    pub st_mode: mode_t,
    /// `st_nlink`: its links.
    pub st_nlink: u16,
    /// `st_ino`: its inode.
    pub st_ino: u64,
    /// `st_uid`: its owner.
    pub st_uid: uid_t,
    /// `st_gid`: its group.
    pub st_gid: gid_t,
    /// `st_rdev`: the device a device file is.
    pub st_rdev: dev_t,
    /// `st_atime`: last read, seconds.
    pub st_atime: i64,
    /// Its nanoseconds.
    pub st_atime_nsec: i64,
    /// `st_mtime`: last written, seconds.
    pub st_mtime: i64,
    /// Its nanoseconds.
    pub st_mtime_nsec: i64,
    /// `st_ctime`: last changed (contents or status), seconds.
    pub st_ctime: i64,
    /// Its nanoseconds.
    pub st_ctime_nsec: i64,
    /// `st_birthtime`: made, seconds.
    pub st_birthtime: i64,
    /// Its nanoseconds.
    pub st_birthtime_nsec: i64,
    /// `st_size`: its length in bytes.
    pub st_size: off_t,
    /// `st_blocks`: 512-byte blocks held.
    pub st_blocks: i64,
    /// `st_blksize`: the best transfer size.
    pub st_blksize: i32,
    /// `st_flags`: its `chflags` flags.
    pub st_flags: u32,
    /// `st_gen`: its generation number.
    pub st_gen: u32,
    st_lspare: i32,
    st_qspare: [i64; 2],
}

// sys/stat.h, macOS arm64 (`struct stat64`'s layout): 144 bytes; the fields at these offsets.
#[cfg(target_os = "macos")]
const _: () = {
    assert!(size_of::<stat>() == 144);
    assert!(align_of::<stat>() == 8);
    assert!(core::mem::offset_of!(stat, st_dev) == 0);
    assert!(core::mem::offset_of!(stat, st_mode) == 4);
    assert!(core::mem::offset_of!(stat, st_nlink) == 6);
    assert!(core::mem::offset_of!(stat, st_ino) == 8);
    assert!(core::mem::offset_of!(stat, st_uid) == 16);
    assert!(core::mem::offset_of!(stat, st_gid) == 20);
    assert!(core::mem::offset_of!(stat, st_rdev) == 24);
    assert!(core::mem::offset_of!(stat, st_atime) == 32);
    assert!(core::mem::offset_of!(stat, st_mtime) == 48);
    assert!(core::mem::offset_of!(stat, st_mtime_nsec) == 56);
    assert!(core::mem::offset_of!(stat, st_ctime) == 64);
    assert!(core::mem::offset_of!(stat, st_size) == 96);
    assert!(core::mem::offset_of!(stat, st_blocks) == 104);
    assert!(core::mem::offset_of!(stat, st_blksize) == 112);
    assert!(core::mem::offset_of!(stat, st_flags) == 116);
};

#[cfg(target_os = "macos")]
impl stat {
    /// All zero, for a call to fill (in place of `mem::zeroed`). On Linux the type's own
    /// (`crate::linux::task::Stat::zeroed`).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn zeroed() -> stat {
        // SAFETY: every field is an integer or an array of them, for which zero bytes are valid.
        unsafe { core::mem::zeroed() }
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// `int fstat(int fd, struct stat *buf)` (sys/stat.h): the status of the open file `fd`.
    /// replaces libc::fstat
    pub fn fstat(fd: c_int, buf: *mut stat) -> c_int;

    /// `int mkfifo(const char *path, mode_t mode)` (sys/stat.h): a named pipe made at `path`.
    /// replaces libc::mkfifo
    pub fn mkfifo(path: *const c_char, mode: mode_t) -> c_int;

    /// `int fchown(int fd, uid_t owner, gid_t group)` (unistd.h): the open file's owner and
    /// group set; `uid_t::MAX` or `gid_t::MAX` (-1) leaves one as it is.
    /// replaces libc::fchown
    pub fn fchown(fd: c_int, owner: uid_t, group: gid_t) -> c_int;
}

/// `fstatat` (sys/stat.h), from the table of calls found at first use:
/// `int fstatat(int, const char *, struct stat *, int)`, `stat` a [`stat`].
/// replaces libc::fstatat
#[cfg(target_os = "macos")]
pub use crate::macos::cold::fstatat;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// `char *realpath(const char *path, char *resolved)` (stdlib.h): `path` with every link,
    /// `.` and `..` resolved, written to `resolved` (`PATH_MAX` bytes); null and `errno`.
    /// libSystem's POSIX form is `realpath$DARWIN_EXTSN` (the plain symbol is the legacy one
    /// that refuses a path whose last part does not exist).
    /// replaces libc::realpath
    #[link_name = "realpath$DARWIN_EXTSN"]
    pub fn realpath(path: *const c_char, resolved: *mut c_char) -> *mut c_char;
}

/// `fstat(fd)`: the status of the open file `fd`. [`Errno::BadF`]. On Linux the system call
/// (`newfstatat` with `AT_EMPTY_PATH`).
#[cfg(target_os = "linux")]
pub use crate::linux::task::fstat as stat_fd;

/// `fstatat(dir, path, flags)`: the status of `path` under `dir` (`AT_SYMLINK_NOFOLLOW` for
/// the link itself). [`Errno::NoEnt`], [`Errno::NotDir`], [`Errno::Acces`]. On Linux
/// `newfstatat`; the flags are the kernel's `AT_*` bits (`crate::linux::AtFlags`).
#[cfg(target_os = "linux")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn stat_at(dir: Fd, path: Path<'_>, flags: c_int) -> Result<stat, Errno> {
    fstatat(dir, path, crate::linux::AtFlags::from_bits(flags as u32))
}

/// `chown`'s open-file form, typed on both systems: the open file's owner and group set;
/// `uid_t::MAX` or `gid_t::MAX` (-1) leaves one as it is. [`Errno::Perm`].
#[blaze::noalloc]
#[blaze::nopanic]
pub fn change_owner(fd: Fd, owner: uid_t, group: gid_t) -> Result<(), Errno> {
    #[cfg(target_os = "linux")]
    return fchown(fd, owner, group);
    // SAFETY: the call takes numbers and no memory.
    #[cfg(target_os = "macos")]
    return said(unsafe { fchown(fd.raw(), owner, group) }).map(drop);
}

/// `mkfifo(path, mode)`, typed on both systems: a named pipe made at `path`.
/// [`Errno::Exist`].
#[blaze::noalloc]
#[blaze::nopanic]
pub fn make_fifo(path: Path<'_>, mode: mode_t) -> Result<(), Errno> {
    #[cfg(target_os = "linux")]
    return mkfifo(path, mode);
    // SAFETY: `path` is a live NUL-terminated string the call reads and does not keep.
    #[cfg(target_os = "macos")]
    return said(unsafe { mkfifo(path.as_ptr().cast(), mode) }).map(drop);
}

/// `fstat(fd)`: the status of the open file `fd`. [`Errno::BadF`].
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn stat_fd(fd: Fd) -> Result<stat, Errno> {
    let mut status = stat::zeroed();
    // SAFETY: `status` is a live `struct stat` (the layout asserted above) the call writes and
    // does not keep.
    said(unsafe { fstat(fd.raw(), &raw mut status) })?;
    Ok(status)
}

/// `fstatat(dir, path, flags)`: the status of `path` under `dir` (`AT_SYMLINK_NOFOLLOW` for
/// the link itself). [`Errno::NoEnt`], [`Errno::NotDir`], [`Errno::Acces`].
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn stat_at(dir: Fd, path: Path<'_>, flags: c_int) -> Result<stat, Errno> {
    let mut status = stat::zeroed();
    // SAFETY: `path` is a live NUL-terminated string the call reads; `status` a live `struct
    // stat` (the layout asserted above) it writes; it keeps neither.
    said(unsafe { fstatat(dir.raw(), path.as_ptr().cast(), (&raw mut status).cast(), flags) })?;
    Ok(status)
}

/// `AT_SYMLINK_NOFOLLOW` for [`stat_at`]: stat the symlink itself rather than its target.
#[cfg(target_os = "macos")]
pub const AT_SYMLINK_NOFOLLOW: c_int = 0x20;
/// `AT_SYMLINK_NOFOLLOW` for [`stat_at`]: stat the symlink itself rather than its target.
#[cfg(target_os = "linux")]
pub const AT_SYMLINK_NOFOLLOW: c_int = 0x100;

/// Resolves `path` (including `.`, `..`, and symlinks) into `into` (no NUL terminator) and returns
/// the written slice.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn realpath_into<'a>(path: Path<'_>, into: &'a mut [u8]) -> Result<&'a [u8], Errno> {
    #[cfg(target_os = "macos")]
    {
        const DARWIN_PATH_MAX: usize = 1024;
        let mut resolved = [0u8; DARWIN_PATH_MAX];
        // SAFETY: `path` is NUL-terminated; `resolved` has `PATH_MAX` (1024) writable bytes,
        // matching Darwin's `realpath$DARWIN_EXTSN` contract.
        let ptr = unsafe { realpath(path.as_ptr().cast(), resolved.as_mut_ptr().cast()) };
        if ptr.is_null() {
            return Err(crate::posix::signal::errno());
        }
        let mut len = 0usize;
        while let Some(&byte) = resolved.get(len) {
            if byte == 0 {
                break;
            }
            len = len.wrapping_add(1);
        }
        let Some(dst) = into.get_mut(..len) else {
            return Err(Errno::NameTooLong);
        };
        let Some(src) = resolved.get(..len) else {
            return Err(Errno::NameTooLong);
        };
        dst.copy_from_slice(src);
        Ok(dst)
    }
    #[cfg(target_os = "linux")]
    {
        crate::linux::task::realpath(path, into).map_err(|err| match err {
            crate::linux::task::RealPathError::Path(e) | crate::linux::task::RealPathError::Proc(e) => e,
        })
    }
}

/// `S_IFMT` (sys/stat.h): the bits of `st_mode` that are the file's kind.
/// replaces libc::S_IFMT
pub const S_IFMT: mode_t = 0o170_000;
/// `S_IFDIR`: a directory.
/// replaces libc::S_IFDIR
pub const S_IFDIR: mode_t = 0o040_000;
/// `S_IFREG`: a regular file.
/// replaces libc::S_IFREG
pub const S_IFREG: mode_t = 0o100_000;
/// `S_IFLNK`: a symbolic link.
/// replaces libc::S_IFLNK
pub const S_IFLNK: mode_t = 0o120_000;
/// `S_IFIFO`: a named pipe.
/// replaces libc::S_IFIFO
pub const S_IFIFO: mode_t = 0o010_000;
/// `S_IFSOCK`: a socket.
/// replaces libc::S_IFSOCK
pub const S_IFSOCK: mode_t = 0o140_000;
/// `S_IFBLK`: a block device.
/// replaces libc::S_IFBLK
pub const S_IFBLK: mode_t = 0o060_000;
/// `S_IFCHR`: a character device.
/// replaces libc::S_IFCHR
pub const S_IFCHR: mode_t = 0o020_000;

/// `DT_UNKNOWN` (dirent.h): a directory entry whose kind the file system does not say.
/// replaces libc::DT_UNKNOWN
pub const DT_UNKNOWN: u8 = 0;
/// `DT_DIR` (dirent.h): a directory entry that is a directory.
/// replaces libc::DT_DIR
pub const DT_DIR: u8 = 4;

/// `STDOUT_FILENO` (unistd.h): standard output's descriptor.
/// replaces libc::STDOUT_FILENO
pub const STDOUT_FILENO: c_int = 1;

/// `PATH_MAX` (linux/limits.h): the longest path, with its NUL.
/// replaces libc::PATH_MAX
#[cfg(target_os = "linux")]
pub const PATH_MAX: c_int = 4096;
/// `PATH_MAX` (sys/syslimits.h): the longest path, with its NUL.
/// replaces libc::PATH_MAX
#[cfg(target_os = "macos")]
pub const PATH_MAX: c_int = 1024;

/// `major(dev)` (sys/sysmacros.h, glibc): a device number's major part.
/// replaces libc::major
#[cfg(target_os = "linux")]
#[blaze::noalloc]
#[blaze::nopanic]
pub const fn major(dev: dev_t) -> c_uint {
    (((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff)) as c_uint
}

/// `minor(dev)` (sys/sysmacros.h, glibc): a device number's minor part.
/// replaces libc::minor
#[cfg(target_os = "linux")]
#[blaze::noalloc]
#[blaze::nopanic]
pub const fn minor(dev: dev_t) -> c_uint {
    ((dev & 0xff) | ((dev >> 12) & !0xff)) as c_uint
}

/// `makedev(major, minor)` (sys/sysmacros.h, glibc): a device number from its parts.
/// replaces libc::makedev
#[cfg(target_os = "linux")]
#[blaze::noalloc]
#[blaze::nopanic]
pub const fn makedev(major: c_uint, minor: c_uint) -> dev_t {
    let (major, minor) = (major as dev_t, minor as dev_t);
    ((major & 0xfff) << 8) | ((major & !0xfff) << 32) | (minor & 0xff) | ((minor & !0xff) << 12)
}

/// `major(dev)` (sys/types.h, macOS): a device number's major part, its top byte.
/// replaces libc::major
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub const fn major(dev: dev_t) -> i32 {
    (dev >> 24) & 0xff
}

/// `minor(dev)` (sys/types.h, macOS): a device number's minor part, its low 24 bits.
/// replaces libc::minor
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub const fn minor(dev: dev_t) -> i32 {
    dev & 0xff_ffff
}

/// `makedev(major, minor)` (sys/types.h, macOS): a device number from its parts.
/// replaces libc::makedev
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub const fn makedev(major: i32, minor: i32) -> dev_t {
    (major << 24) | minor
}

// ---- the shared calls on names and modes ------------------------------------------------

/// Which kind of name an [`unlink_at`] removes: Linux's own type.
#[cfg(target_os = "linux")]
pub use crate::linux::Unlink;

/// Which kind of name an [`unlink_at`] removes (the same type as Linux's).
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unlink {
    /// A name that is no directory.
    File,
    /// An empty directory (`AT_REMOVEDIR`).
    Dir,
}

/// `unlinkat(dir, path, what)`: the name `path` under `dir` removed. On Linux by system call
/// (`crate::linux::unlinkat`); on macOS through libSystem. [`Errno::NoEnt`],
/// [`Errno::NotEmpty`] for a directory with names in it.
/// replaces libc::unlinkat
#[cfg(target_os = "linux")]
pub use crate::linux::unlinkat as unlink_at;

/// `unlinkat(dir, path, what)`: the name `path` under `dir` removed. [`Errno::NoEnt`],
/// [`Errno::NotEmpty`] for a directory with names in it.
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn unlink_at(dir: Fd, path: Path<'_>, what: Unlink) -> Result<(), Errno> {
    /// `AT_REMOVEDIR` (sys/fcntl.h, macOS).
    const AT_REMOVEDIR: c_int = 0x0080;
    let flags = match what {
        Unlink::File => 0,
        Unlink::Dir => AT_REMOVEDIR,
    };
    // SAFETY: `path` is a live NUL-terminated string the call reads and does not keep.
    said(unsafe { crate::macos::cold::unlinkat(dir.raw(), path.as_ptr().cast(), flags) }).map(drop)
}

/// `unlinkat` (unistd.h), from the table of calls found at first use. The typed call on both
/// systems is [`unlink_at`].
/// replaces libc::unlinkat
#[cfg(target_os = "macos")]
pub use crate::macos::cold::unlinkat;

/// `unlink` (unistd.h), from the table of calls found at first use. On Linux the call is
/// [`unlink_at`]`(crate::linux::CWD, path, Unlink::File)`.
/// replaces libc::unlink
#[cfg(target_os = "macos")]
pub use crate::macos::cold::unlink;

/// `fchmod(fd, mode)`: the open file's permissions set. [`Errno::BadF`], [`Errno::Perm`].
#[blaze::noalloc]
#[blaze::nopanic]
pub fn change_mode(fd: Fd, mode: mode_t) -> Result<(), Errno> {
    #[cfg(target_os = "linux")]
    return crate::linux::fchmod(fd, mode);
    // SAFETY: the call takes numbers and no memory.
    #[cfg(target_os = "macos")]
    return said(unsafe { crate::macos::cold::fchmod(fd.raw(), mode) }).map(drop);
}

/// `fchmod` (sys/stat.h), from the table of calls found at first use. On Linux the call is
/// `crate::linux::fchmod`; on both, [`change_mode`].
/// replaces libc::fchmod
#[cfg(target_os = "macos")]
pub use crate::macos::cold::fchmod;

/// `fchmodat` (sys/stat.h), from the table of calls found at first use:
/// `int fchmodat(int dir, const char *path, mode_t mode, int flags)`.
/// replaces libc::fchmodat
#[cfg(target_os = "macos")]
pub use crate::macos::cold::fchmodat;

/// `mkdir` (sys/stat.h), from the table of calls found at first use:
/// `int mkdir(const char *path, mode_t mode)`. On Linux the call is
/// `crate::linux::mkdirat(crate::linux::CWD, path, mode)`.
/// replaces libc::mkdir
#[cfg(target_os = "macos")]
pub use crate::macos::cold::mkdir;

/// `utimensat` (sys/stat.h), from the table of calls found at first use:
/// `int utimensat(int dir, const char *path, const struct timespec times[2], int flags)`,
/// `times` two [`crate::Timespec`]s. On Linux the call is `crate::linux::utimensat`.
/// replaces libc::utimensat
#[cfg(target_os = "macos")]
pub use crate::macos::cold::utimensat;

/// `renameat(from_dir, from, to_dir, to)`: the name `from` moved to `to`, replacing what is
/// there. On Linux `renameat2` with no flags, by system call. [`Errno::NoEnt`],
/// [`Errno::XDev`].
#[blaze::noalloc]
#[blaze::nopanic]
pub fn rename_at(from_dir: Fd, from: Path<'_>, to_dir: Fd, to: Path<'_>) -> Result<(), Errno> {
    #[cfg(target_os = "linux")]
    return crate::linux::renameat2(from_dir, from, to_dir, to, crate::linux::RenameFlags::NONE);
    // SAFETY: `from` and `to` are live NUL-terminated strings the call reads and does not keep.
    #[cfg(target_os = "macos")]
    return said(unsafe { renameat(from_dir.raw(), from.as_ptr().cast(), to_dir.raw(), to.as_ptr().cast()) }).map(drop);
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// `int renameat(int from_dir, const char *from, int to_dir, const char *to)` (stdio.h).
    /// The typed call on both systems is [`rename_at`].
    /// replaces libc::renameat
    pub fn renameat(from_dir: c_int, from: *const c_char, to_dir: c_int, to: *const c_char) -> c_int;
}

// ---- struct statfs ----------------------------------------------------------------------

/// `struct statfs` (asm-generic/statfs.h): Linux's typed one, with named fields (`kind` for
/// `f_type`, `fsid` for `f_fsid`, ...). Filled by `crate::linux::fstatfs` (by system call)
/// and by [`statfs()`].
/// replaces libc::statfs
#[cfg(target_os = "linux")]
#[allow(non_camel_case_types, reason = "the C type's name, so a port is a change of path")]
pub type statfs = crate::linux::Statfs;

/// `fstatfs(fd)`: the file system the open file `fd` is on, by system call.
/// replaces libc::fstatfs
#[cfg(target_os = "linux")]
pub use crate::linux::fstatfs;

/// `statfs(path)`: the file system `path` is on, by system call (typed: `Result<statfs,
/// Errno>`).
/// replaces libc::statfs
#[cfg(target_os = "linux")]
pub use crate::linux::task::statfs;

/// `struct statfs` (sys/mount.h, macOS, 64-bit-inode form): what `statfs` and `fstatfs` fill.
/// replaces libc::statfs
#[cfg(target_os = "macos")]
#[allow(non_camel_case_types, reason = "the C type's name, so a port is a change of path")]
#[derive(Clone, Copy)]
#[repr(C)]
pub struct statfs {
    /// `f_bsize`: the fundamental block size.
    pub f_bsize: u32,
    /// `f_iosize`: the best transfer size.
    pub f_iosize: i32,
    /// `f_blocks`: its blocks.
    pub f_blocks: u64,
    /// `f_bfree`.
    pub f_bfree: u64,
    /// `f_bavail`: free blocks an unprivileged writer may use.
    pub f_bavail: u64,
    /// `f_files`: its inodes.
    pub f_files: u64,
    /// `f_ffree`.
    pub f_ffree: u64,
    /// `f_fsid`: the file system's id, two words.
    pub f_fsid: [i32; 2],
    /// `f_owner`: the uid that mounted it.
    pub f_owner: uid_t,
    /// `f_type`: its kind's number.
    pub f_type: u32,
    /// `f_flags`: the mount's `MNT_*` flags.
    pub f_flags: u32,
    /// `f_fssubtype`.
    pub f_fssubtype: u32,
    /// `f_fstypename`: its kind's name (`"apfs"`), NUL-terminated.
    pub f_fstypename: [c_char; 16],
    /// `f_mntonname`: where it is mounted, NUL-terminated.
    pub f_mntonname: [c_char; 1024],
    /// `f_mntfromname`: what is mounted, NUL-terminated.
    pub f_mntfromname: [c_char; 1024],
    /// `f_flags_ext`.
    pub f_flags_ext: u32,
    f_reserved: [u32; 7],
}

// sys/mount.h, macOS arm64 (`struct statfs64`'s layout): 2168 bytes; these fields.
#[cfg(target_os = "macos")]
const _: () = {
    assert!(size_of::<statfs>() == 2168);
    assert!(align_of::<statfs>() == 8);
    assert!(core::mem::offset_of!(statfs, f_bsize) == 0);
    assert!(core::mem::offset_of!(statfs, f_blocks) == 8);
    assert!(core::mem::offset_of!(statfs, f_bavail) == 24);
    assert!(core::mem::offset_of!(statfs, f_fsid) == 48);
    assert!(core::mem::offset_of!(statfs, f_owner) == 56);
    assert!(core::mem::offset_of!(statfs, f_type) == 60);
    assert!(core::mem::offset_of!(statfs, f_flags) == 64);
    assert!(core::mem::offset_of!(statfs, f_fstypename) == 72);
    assert!(core::mem::offset_of!(statfs, f_mntonname) == 88);
    assert!(core::mem::offset_of!(statfs, f_mntfromname) == 1112);
    assert!(core::mem::offset_of!(statfs, f_flags_ext) == 2136);
};

#[cfg(target_os = "macos")]
impl statfs {
    /// All zero, for a call to fill (in place of `mem::zeroed`).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn zeroed() -> statfs {
        // SAFETY: every field is an integer or an array of them, for which zero bytes are valid.
        unsafe { core::mem::zeroed() }
    }
}

/// `statfs` (sys/mount.h), from the table of calls found at first use:
/// `int statfs(const char *path, struct statfs *buf)`, `stat` a [`statfs`].
/// replaces libc::statfs
#[cfg(target_os = "macos")]
pub use crate::macos::cold::statfs;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// `int fstatfs(int fd, struct statfs *buf)` (sys/mount.h): the file system the open file
    /// `fd` is on.
    /// replaces libc::fstatfs
    pub fn fstatfs(fd: c_int, buf: *mut statfs) -> c_int;
}

/// `fstatfs(fd)`: the file system the open file `fd` is on. [`Errno::BadF`]. On Linux the call
/// is `crate::linux::fstatfs`.
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn stat_fs_fd(fd: Fd) -> Result<statfs, Errno> {
    let mut status = statfs::zeroed();
    // SAFETY: `status` is a live `struct statfs` (the layout asserted above) the call writes and
    // does not keep.
    said(unsafe { fstatfs(fd.raw(), &raw mut status) })?;
    Ok(status)
}

/// `MNT_LOCAL` (sys/mount.h, macOS): the file system is on a local disk.
/// replaces libc::MNT_LOCAL
#[cfg(target_os = "macos")]
pub const MNT_LOCAL: c_int = 0x0000_1000;

/// `MNT_IGNORE_OWNERSHIP` (sys/mount.h, macOS): the volume shows every file as owned by
/// whoever asks (uid 99 stored).
/// replaces libc::MNT_IGNORE_OWNERSHIP
#[cfg(target_os = "macos")]
pub const MNT_IGNORE_OWNERSHIP: c_int = 0x0020_0000;

// ---- macOS: attribute lists, clones, fcntl's structs --------------------------------------

/// `attrgroup_t` (sys/attr.h): a group of `ATTR_*` bits.
/// replaces libc::attrgroup_t
#[cfg(target_os = "macos")]
#[allow(non_camel_case_types, reason = "the C type's name, so a port is a change of path")]
pub type attrgroup_t = u32;

/// `struct attrlist` (sys/attr.h): which attributes `getattrlistbulk` returns.
/// replaces libc::attrlist
#[cfg(target_os = "macos")]
#[allow(non_camel_case_types, reason = "the C type's name, so a port is a change of path")]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct attrlist {
    /// `bitmapcount`: [`ATTR_BIT_MAP_COUNT`].
    pub bitmapcount: u16,
    /// `reserved`: 0.
    pub reserved: u16,
    /// `commonattr`: `ATTR_CMN_*`.
    pub commonattr: attrgroup_t,
    /// `volattr`: `ATTR_VOL_*`.
    pub volattr: attrgroup_t,
    /// `dirattr`: `ATTR_DIR_*`.
    pub dirattr: attrgroup_t,
    /// `fileattr`: `ATTR_FILE_*`.
    pub fileattr: attrgroup_t,
    /// `forkattr`: `ATTR_FORK_*`.
    pub forkattr: attrgroup_t,
}

// sys/attr.h: 24 bytes, aligned to 4.
#[cfg(target_os = "macos")]
const _: () = {
    assert!(size_of::<attrlist>() == 24);
    assert!(align_of::<attrlist>() == 4);
    assert!(core::mem::offset_of!(attrlist, commonattr) == 4);
    assert!(core::mem::offset_of!(attrlist, dirattr) == 12);
    assert!(core::mem::offset_of!(attrlist, fileattr) == 16);
};

/// `ATTR_BIT_MAP_COUNT` (sys/attr.h): `attrlist.bitmapcount`.
/// replaces libc::ATTR_BIT_MAP_COUNT
#[cfg(target_os = "macos")]
pub const ATTR_BIT_MAP_COUNT: u16 = 5;
/// `ATTR_CMN_NAME`: the entry's name (an `attrreference_t`).
/// replaces libc::ATTR_CMN_NAME
#[cfg(target_os = "macos")]
pub const ATTR_CMN_NAME: attrgroup_t = 0x0000_0001;
/// `ATTR_CMN_DEVID`: the device it is on.
/// replaces libc::ATTR_CMN_DEVID
#[cfg(target_os = "macos")]
pub const ATTR_CMN_DEVID: attrgroup_t = 0x0000_0002;
/// `ATTR_CMN_OBJTYPE`: its kind (`fsobj_type_t`).
/// replaces libc::ATTR_CMN_OBJTYPE
#[cfg(target_os = "macos")]
pub const ATTR_CMN_OBJTYPE: attrgroup_t = 0x0000_0008;
/// `ATTR_CMN_MODTIME`: last written.
/// replaces libc::ATTR_CMN_MODTIME
#[cfg(target_os = "macos")]
pub const ATTR_CMN_MODTIME: attrgroup_t = 0x0000_0400;
/// `ATTR_CMN_CHGTIME`: last changed.
/// replaces libc::ATTR_CMN_CHGTIME
#[cfg(target_os = "macos")]
pub const ATTR_CMN_CHGTIME: attrgroup_t = 0x0000_0800;
/// `ATTR_CMN_ACCESSMASK`: its permissions.
/// replaces libc::ATTR_CMN_ACCESSMASK
#[cfg(target_os = "macos")]
pub const ATTR_CMN_ACCESSMASK: attrgroup_t = 0x0002_0000;
/// `ATTR_CMN_FILEID`: its inode.
/// replaces libc::ATTR_CMN_FILEID
#[cfg(target_os = "macos")]
pub const ATTR_CMN_FILEID: attrgroup_t = 0x0200_0000;
/// `ATTR_CMN_RETURNED_ATTRS`: which attributes the entry has (an `attribute_set_t`, first).
/// replaces libc::ATTR_CMN_RETURNED_ATTRS
#[cfg(target_os = "macos")]
pub const ATTR_CMN_RETURNED_ATTRS: attrgroup_t = 0x8000_0000;
/// `ATTR_DIR_DATALENGTH`: a directory's length.
/// replaces libc::ATTR_DIR_DATALENGTH
#[cfg(target_os = "macos")]
pub const ATTR_DIR_DATALENGTH: attrgroup_t = 0x0000_0020;
/// `ATTR_FILE_DATALENGTH`: a file's data fork's length.
/// replaces libc::ATTR_FILE_DATALENGTH
#[cfg(target_os = "macos")]
pub const ATTR_FILE_DATALENGTH: attrgroup_t = 0x0000_0200;

/// `getattrlistbulk` (sys/attr.h), from the table of calls found at first use:
/// `int getattrlistbulk(int dir, struct attrlist *, void *buf, size_t, uint64_t options)`,
/// `attrs` an [`attrlist`].
/// replaces libc::getattrlistbulk
#[cfg(target_os = "macos")]
pub use crate::macos::cold::getattrlistbulk;

/// `fclonefileat` (sys/clonefile.h), from the table of calls found at first use:
/// `int fclonefileat(int from_fd, int to_dir, const char *to, uint32_t flags)`.
/// replaces libc::fclonefileat
#[cfg(target_os = "macos")]
pub use crate::macos::cold::fclonefileat;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// `int clonefile(const char *from, const char *to, uint32_t flags)` (sys/clonefile.h): `to`
    /// made a copy of `from` that shares its blocks (APFS); [`Errno::NotSup`] on a volume that
    /// cannot, [`Errno::XDev`] across volumes.
    /// replaces libc::clonefile
    pub fn clonefile(from: *const c_char, to: *const c_char, flags: u32) -> c_int;
}

/// `clonefile(from, to)`: `to` made a block-sharing copy of `from`, with no flags.
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn clone_file(from: Path<'_>, to: Path<'_>) -> Result<(), Errno> {
    // SAFETY: `from` and `to` are live NUL-terminated strings the call reads and does not keep.
    said(unsafe { clonefile(from.as_ptr().cast(), to.as_ptr().cast(), 0) }).map(drop)
}

/// `struct radvisory` (sys/fcntl.h, macOS): `F_RDADVISE`'s read-ahead advice. The header
/// packs it to 4 (`#pragma pack(4)`), so it is 12 bytes, `ra_count` right after the offset.
/// replaces libc::radvisory
#[cfg(target_os = "macos")]
#[allow(non_camel_case_types, reason = "the C type's name, so a port is a change of path")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C, packed(4))]
pub struct radvisory {
    /// `ra_offset`: where the read will start.
    pub ra_offset: off_t,
    /// `ra_count`: how many bytes it will read.
    pub ra_count: c_int,
}

// sys/fcntl.h (`#pragma pack(4)`): 12 bytes, aligned to 4.
#[cfg(target_os = "macos")]
const _: () = {
    assert!(size_of::<radvisory>() == 12);
    assert!(align_of::<radvisory>() == 4);
    assert!(core::mem::offset_of!(radvisory, ra_offset) == 0);
    assert!(core::mem::offset_of!(radvisory, ra_count) == 8);
};

/// `fstore_t` (sys/fcntl.h, macOS): `F_PREALLOCATE`'s request.
/// replaces libc::fstore_t
#[cfg(target_os = "macos")]
#[allow(non_camel_case_types, reason = "the C type's name, so a port is a change of path")]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct fstore_t {
    /// `fst_flags`: `F_ALLOCATECONTIG`, `F_ALLOCATEALL`.
    pub fst_flags: c_uint,
    /// `fst_posmode`: `F_PEOFPOSMODE` or `F_VOLPOSMODE`.
    pub fst_posmode: c_int,
    /// `fst_offset`: where the room starts.
    pub fst_offset: off_t,
    /// `fst_length`: how much room.
    pub fst_length: off_t,
    /// `fst_bytesalloc`: how much the call got (out).
    pub fst_bytesalloc: off_t,
}

// sys/fcntl.h: 32 bytes; these fields.
#[cfg(target_os = "macos")]
const _: () = {
    assert!(size_of::<fstore_t>() == 32);
    assert!(align_of::<fstore_t>() == 8);
    assert!(core::mem::offset_of!(fstore_t, fst_posmode) == 4);
    assert!(core::mem::offset_of!(fstore_t, fst_offset) == 8);
    assert!(core::mem::offset_of!(fstore_t, fst_length) == 16);
    assert!(core::mem::offset_of!(fstore_t, fst_bytesalloc) == 24);
};

// ---- Linux: advice, room, mounts --------------------------------------------------------

/// `POSIX_FADV_WILLNEED` (fcntl.h): the range will be read soon.
/// replaces libc::POSIX_FADV_WILLNEED
#[cfg(target_os = "linux")]
pub const POSIX_FADV_WILLNEED: c_int = 3;

/// `MNT_DETACH` (sys/mount.h): `umount2` detaches now and frees once unused.
/// replaces libc::MNT_DETACH
#[cfg(target_os = "linux")]
pub const MNT_DETACH: c_int = 2;

/// Advice, room and mounts by system call on Linux: `fadvise64` (glibc's `posix_fadvise`;
/// advice a [`FileAdvice`]), `fallocate` (`posix_fallocate` is it with
/// [`FallocateFlags::NONE`]: the linux/fdctl.rs call), `mount` ([`MountFlags`]) and `umount2`
/// ([`UnmountFlags`]; `MNT_DETACH` is [`UnmountFlags::DETACH`]).
/// replaces libc::posix_fadvise
/// replaces libc::posix_fallocate
/// replaces libc::mount
/// replaces libc::umount2
#[cfg(target_os = "linux")]
pub use crate::linux::fdctl::{FallocateFlags, fallocate};
/// replaces libc::POSIX_FADV_NORMAL (blaze_sys::posix::proc::FileAdvice::Normal; Linux only)
/// replaces libc::POSIX_FADV_RANDOM (blaze_sys::posix::proc::FileAdvice::Random; Linux only)
/// replaces libc::POSIX_FADV_SEQUENTIAL (blaze_sys::posix::proc::FileAdvice::Sequential; Linux only)
/// replaces libc::POSIX_FADV_DONTNEED (blaze_sys::posix::proc::FileAdvice::DontNeed; Linux only)
/// replaces libc::POSIX_FADV_NOREUSE (blaze_sys::posix::proc::FileAdvice::NoReuse; Linux only)
/// replaces libc::MS_RDONLY (blaze_sys::posix::proc::MountFlags::READ_ONLY; Linux only)
/// replaces libc::MS_NOSUID (blaze_sys::posix::proc::MountFlags::NO_SUID; Linux only)
/// replaces libc::MS_NODEV (blaze_sys::posix::proc::MountFlags::NO_DEV; Linux only)
/// replaces libc::MS_NOEXEC (blaze_sys::posix::proc::MountFlags::NO_EXEC; Linux only)
/// replaces libc::MS_REMOUNT (blaze_sys::posix::proc::MountFlags::REMOUNT; Linux only)
/// replaces libc::MS_BIND (blaze_sys::posix::proc::MountFlags::BIND; Linux only)
/// replaces libc::MS_REC (blaze_sys::posix::proc::MountFlags::RECURSIVE; Linux only)
/// replaces libc::MS_PRIVATE (blaze_sys::posix::proc::MountFlags::PRIVATE; Linux only)
/// replaces libc::MNT_FORCE (blaze_sys::posix::proc::UnmountFlags::FORCE; Linux only)
/// replaces libc::MNT_EXPIRE (blaze_sys::posix::proc::UnmountFlags::EXPIRE; Linux only)
/// replaces libc::UMOUNT_NOFOLLOW (blaze_sys::posix::proc::UnmountFlags::NO_FOLLOW; Linux only)
#[cfg(target_os = "linux")]
pub use crate::linux::task::{FileAdvice, MountFlags, UnmountFlags, fadvise64, mount, umount2};

/// `posix_fadvise(fd, offset, len, POSIX_FADV_WILLNEED)`: the range read ahead.
#[cfg(target_os = "linux")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn advise_will_need(fd: Fd, offset: off_t, len: off_t) -> Result<(), Errno> {
    fadvise64(fd, offset, len, FileAdvice::WillNeed)
}

/// `posix_fallocate(fd, offset, len)`: room held for the range, the file grown to cover it.
/// [`Errno::NoSpc`]; [`Errno::Inval`] for a negative offset or a length not above 0 (as
/// `posix_fallocate`), refused before the call.
#[cfg(target_os = "linux")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn allocate(fd: Fd, offset: off_t, len: off_t) -> Result<(), Errno> {
    if offset < 0 || len <= 0 {
        return Err(Errno::Inval);
    }
    fallocate(fd, FallocateFlags::NONE, offset as u64, len as u64)
}

// ---- extended attributes ----------------------------------------------------------------

/// The extended attributes by system call on Linux, typed (`setxattr(path, name, value,
/// XattrFlags)`, `removexattr(path, name)`, `fgetxattr(fd, name, into) -> length`).
/// replaces libc::setxattr
/// replaces libc::removexattr
/// replaces libc::fgetxattr
/// replaces libc::XATTR_CREATE (blaze_sys::posix::proc::XattrFlags::CREATE on Linux; macOS's setxattr takes the c_int options, XATTR_CREATE there is 0x2)
/// replaces libc::XATTR_REPLACE (blaze_sys::posix::proc::XattrFlags::REPLACE on Linux; macOS's setxattr takes the c_int options, XATTR_REPLACE there is 0x4)
#[cfg(target_os = "linux")]
pub use crate::linux::task::{XattrFlags, fgetxattr, removexattr, setxattr};

/// `XATTR_CREATE` (sys/xattr.h, macOS): `setxattr`'s `options` bit that refuses
/// ([`Errno::Exist`]) where the attribute is there.
/// replaces libc::XATTR_CREATE (blaze_sys::posix::proc::XATTR_CREATE on macOS, an options c_int for setxattr; XattrFlags::CREATE on Linux)
#[cfg(target_os = "macos")]
pub const XATTR_CREATE: c_int = 0x0002;

/// `XATTR_REPLACE` (sys/xattr.h, macOS): `setxattr`'s `options` bit that refuses
/// ([`Errno::NoData`]) where the attribute is not there.
/// replaces libc::XATTR_REPLACE (blaze_sys::posix::proc::XATTR_REPLACE on macOS, an options c_int for setxattr; XattrFlags::REPLACE on Linux)
#[cfg(target_os = "macos")]
pub const XATTR_REPLACE: c_int = 0x0004;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// `int setxattr(const char *path, const char *name, const void *value, size_t size,
    /// u_int32_t position, int options)` (sys/xattr.h, macOS: a resource fork's position and
    /// `XATTR_*` options beside Linux's arguments).
    /// replaces libc::setxattr
    pub fn setxattr(
        path: *const c_char,
        name: *const c_char,
        value: *const c_void,
        size: usize,
        position: u32,
        options: c_int,
    ) -> c_int;

    /// `int removexattr(const char *path, const char *name, int options)` (sys/xattr.h).
    /// replaces libc::removexattr
    pub fn removexattr(path: *const c_char, name: *const c_char, options: c_int) -> c_int;

    /// `ssize_t fgetxattr(int fd, const char *name, void *value, size_t size, u_int32_t
    /// position, int options)` (sys/xattr.h): the attribute's length; [`Errno::NoData`] where
    /// it is absent (macOS's `ENOATTR` is the same number).
    /// replaces libc::fgetxattr
    pub fn fgetxattr(fd: c_int, name: *const c_char, value: *mut c_void, size: usize, position: u32, options: c_int) -> isize;
}
