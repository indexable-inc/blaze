//! Kernel-enforced capability confinement (`openat` + `O_NOFOLLOW` component walk on Unix,
//! `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` on Linux) and zero-memset uninitialized
//! `pread` directly into a caller's `Vec<u8, A>`.

use core::{alloc::Allocator, mem::MaybeUninit};
use std::{fs::File, io, path::Path};

#[cfg(unix)]
use std::os::fd::{AsRawFd as _, BorrowedFd, FromRawFd as _, OwnedFd};

use crate::{FileName, threads::inline::LentRead};

/// Maximum `EINTR` retries on one kernel call (bounded-progress invariant).
#[cfg(unix)]
const INTERRUPTS_MOST: u32 = 64;

/// A cached kernel directory descriptor (`O_RDONLY | O_DIRECTORY | O_CLOEXEC`) for a driver's
/// root directory. Kept once per [`crate::threads::inline::Inline`] so confined opens start
/// directly at `root_fd` instead of walking the absolute path prefix from `/` on every call.
pub(crate) struct RootDir {
    #[cfg(unix)]
    fd: Option<OwnedFd>,
}

impl RootDir {
    /// Opens `root` as a directory descriptor if it exists; `None` if it cannot be opened yet
    /// (for example a root created lazily later), in which case callers open `root` on demand.
    pub(crate) fn open(root: &Path) -> RootDir {
        #[cfg(unix)]
        {
            RootDir { fd: open_dir_fd(root).ok() }
        }
        #[cfg(not(unix))]
        {
            let _ = root;
            RootDir {}
        }
    }

    #[cfg(unix)]
    pub(crate) fn as_fd(&self) -> Option<BorrowedFd<'_>> {
        self.fd.as_ref().map(|f| {
            use std::os::fd::AsFd as _;
            f.as_fd()
        })
    }
}

#[cfg(unix)]
fn open_dir_fd(path: &Path) -> io::Result<OwnedFd> {
    super::with_c_path(path, |cpath| {
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
        let mut interrupted = 0;
        loop {
            #[expect(unsafe_code, reason = "open(O_RDONLY | O_DIRECTORY | O_CLOEXEC) to cache the root dirfd")]
            let raw = unsafe { libc::open(cpath.as_ptr(), flags) };
            if raw >= 0 {
                #[expect(unsafe_code, reason = "raw >= 0 is a freshly opened kernel file descriptor")]
                return Ok(unsafe { OwnedFd::from_raw_fd(raw) });
            }
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) && interrupted < INTERRUPTS_MOST {
                interrupted += 1;
                continue;
            }
            return Err(err);
        }
    })
}

/// Opens `rel` beneath `root` strictly without following any symbolic link in any intermediate
/// directory or at the final leaf (`O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK` component-by-component
/// via `openat`, or `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` on Linux).
#[cfg(unix)]
pub(crate) fn open_beneath_readonly(root_dir: &RootDir, root: &Path, rel: &FileName) -> io::Result<File> {
    let owned_fallback;
    let base_fd: BorrowedFd<'_> = match root_dir.as_fd() {
        Some(fd) => fd,
        None => {
            owned_fallback = open_dir_fd(root)?;
            use std::os::fd::AsFd as _;
            owned_fallback.as_fd()
        }
    };

    #[cfg(target_os = "linux")]
    if let Some(res) = try_openat2_beneath(base_fd, rel) {
        return res;
    }

    walk_openat_nofollow(base_fd, rel)
}

#[cfg(target_os = "linux")]
fn try_openat2_beneath(base_fd: BorrowedFd<'_>, rel: &FileName) -> Option<io::Result<File>> {
    // Linux openat2(2) `struct open_how` (u64 flags, u64 mode, u64 resolve).
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }
    const RESOLVE_BENEATH: u64 = 0x08;
    const RESOLVE_NO_SYMLINKS: u64 = 0x04;

    let how = OpenHow {
        flags: (libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NONBLOCK) as u64,
        mode: 0,
        resolve: RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
    };
    let res = super::with_c_path(Path::new(rel.as_str()), |cpath| {
        let mut interrupted = 0;
        loop {
            #[expect(unsafe_code, reason = "Linux openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS) kernel-enforced confinement")]
            let raw = unsafe {
                libc::syscall(
                    libc::SYS_openat2,
                    base_fd.as_raw_fd(),
                    cpath.as_ptr(),
                    (&raw const how).cast::<libc::c_void>(),
                    core::mem::size_of::<OpenHow>(),
                )
            };
            if raw >= 0 {
                #[expect(unsafe_code, reason = "raw >= 0 is a freshly opened kernel file descriptor")]
                return Ok(unsafe { File::from_raw_fd(raw as libc::c_int) });
            }
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) && interrupted < INTERRUPTS_MOST {
                interrupted += 1;
                continue;
            }
            return Err(err);
        }
    });
    match res {
        Err(ref e) if matches!(e.raw_os_error(), Some(libc::ENOSYS | libc::EPERM)) => None,
        other => Some(other),
    }
}

#[cfg(unix)]
fn walk_openat_nofollow(base_fd: BorrowedFd<'_>, rel: &FileName) -> io::Result<File> {
    let raw_rel = rel.as_str();
    let mut parts = raw_rel.split('/').peekable();
    let mut owned_cur: Option<OwnedFd> = None;

    while let Some(part) = parts.next() {
        if part.is_empty() || part == "." || part == ".." {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid relative path component"));
        }
        let is_last = parts.peek().is_none();
        let dir_raw = match &owned_cur {
            Some(fd) => fd.as_raw_fd(),
            None => base_fd.as_raw_fd(),
        };
        let flags = if is_last {
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK
        } else {
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC
        };
        let next_fd = openat_one(dir_raw, part, flags)?;
        if is_last {
            return Ok(File::from(next_fd));
        }
        owned_cur = Some(next_fd);
    }
    Err(io::Error::new(io::ErrorKind::InvalidInput, "empty relative path"))
}

#[cfg(unix)]
fn openat_one(dir_fd: libc::c_int, component: &str, flags: libc::c_int) -> io::Result<OwnedFd> {
    super::with_c_path(Path::new(component), |cpath| {
        let mut interrupted = 0;
        loop {
            #[expect(unsafe_code, reason = "openat(O_NOFOLLOW | O_CLOEXEC) single-component confined walk")]
            let raw = unsafe { libc::openat(dir_fd, cpath.as_ptr(), flags) };
            if raw >= 0 {
                #[expect(unsafe_code, reason = "raw >= 0 is a freshly opened kernel file descriptor")]
                return Ok(unsafe { OwnedFd::from_raw_fd(raw) });
            }
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) && interrupted < INTERRUPTS_MOST {
                interrupted += 1;
                continue;
            }
            return Err(err);
        }
    })
}

#[cfg(not(unix))]
pub(crate) fn open_beneath_readonly(_root_dir: &RootDir, root: &Path, rel: &FileName) -> io::Result<File> {
    File::open(root.join(rel.as_str()))
}

/// Positional read directly into an uninitialized slice (zero `memset(0)` on Unix via `pread`).
#[cfg(unix)]
pub(crate) fn read_at_uninit(file: &File, into: &mut [MaybeUninit<u8>], at: u64) -> io::Result<usize> {
    if into.is_empty() {
        return Ok(0);
    }
    let count = into.len().min(i32::MAX as usize);
    let offset = libc::off_t::try_from(at)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "offset exceeds off_t"))?;
    let mut interrupted = 0;
    loop {
        #[expect(unsafe_code, reason = "pread directly into MaybeUninit<u8> slice to avoid redundant memset(0)")]
        let n = unsafe { libc::pread(file.as_raw_fd(), into.as_mut_ptr().cast(), count, offset) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EINTR) && interrupted < INTERRUPTS_MOST {
            interrupted += 1;
            continue;
        }
        return Err(err);
    }
}

#[cfg(not(unix))]
pub(crate) fn read_at_uninit(file: &File, into: &mut [MaybeUninit<u8>], at: u64) -> io::Result<usize> {
    for b in into.iter_mut() {
        b.write(0);
    }
    #[expect(unsafe_code, reason = "all elements were initialized to 0 immediately above")]
    let slice = unsafe { core::slice::from_raw_parts_mut(into.as_mut_ptr().cast::<u8>(), into.len()) };
    super::read_at(file, slice, at)
}

/// Reads an already-opened regular `file` directly into `into`'s uninitialized spare capacity:
/// `fstat` for length, `try_reserve(len + 1)` (NO zero-fill!), `pread` directly into spare
/// capacity, and `set_len` once the full `stated` bytes land. On `NoRoom`, `Moved`, or `Err`,
/// `into.len()` stays untouched.
pub(crate) fn read_open_into_vec<A: Allocator>(file: &File, into: &mut Vec<u8, A>) -> io::Result<LentRead> {
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
    }
    let stated = meta.len();
    let Some(asked) = usize::try_from(stated).ok().and_then(|n| n.checked_add(1)) else {
        return Ok(LentRead::NoRoom { len: stated });
    };
    if into.try_reserve(asked).is_err() {
        return Ok(LentRead::NoRoom { len: stated });
    }
    let start = into.len();
    let spare = &mut into.spare_capacity_mut()[..asked];
    let mut filled = 0usize;
    while filled < spare.len() {
        let n = read_at_uninit(file, &mut spare[filled..], filled as u64)?;
        if n == 0 {
            break;
        }
        filled += n;
        if filled as u64 >= stated && filled < spare.len() {
            break;
        }
    }
    let read = filled as u64;
    if read != stated {
        return Ok(LentRead::Moved { stated, read });
    }
    #[expect(unsafe_code, reason = "pread initialized exactly `filled == stated` bytes at `start..start + filled` within reserved capacity")]
    unsafe {
        into.set_len(start + filled);
    }
    Ok(LentRead::Read { len: stated })
}

/// [`crate::Effect::ReadAll`] straight into `into`'s uninitialized spare capacity (zero
/// `memset(0)`, zero intermediate `Vec`, zero copy). A missing file reads as 0 bytes.
pub(crate) fn read_whole_into_vec<A: Allocator>(path: &Path, into: &mut Vec<u8, A>) -> io::Result<LentRead> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(LentRead::Read { len: 0 }),
        Err(error) => return Err(error),
    };
    read_open_into_vec(&file, into)
}

/// Kernel-confined whole-file read: opens `rel` strictly beneath `root_dir` via
/// `openat`/`O_NOFOLLOW` (refusing any symlink at any component with `ELOOP`) and reads straight
/// into `into`'s uninitialized spare capacity (zero `memset(0)`, zero copy).
pub(crate) fn read_whole_confined_into_vec<A: Allocator>(
    root_dir: &RootDir,
    root: &Path,
    rel: &FileName,
    into: &mut Vec<u8, A>,
) -> io::Result<LentRead> {
    let file = open_beneath_readonly(root_dir, root, rel)?;
    read_open_into_vec(&file, into)
}
