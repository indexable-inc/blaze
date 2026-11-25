//! Kernel-enforced capability confinement (`openat` + `O_NOFOLLOW` component walk on Unix,
//! `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` on Linux) and zero-memset uninitialized
//! `pread` directly into a caller's `Vec<u8, A>`.

use core::{alloc::Allocator, mem::MaybeUninit};
use std::{
    fs::{self, File},
    io,
    path::Path,
};

use blaze_own::Share as _;

#[cfg(unix)]
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, FromRawFd as _, IntoRawFd as _, OwnedFd};

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

/// Kernel-confined read (whole file when `range` is `None`, or positional `[at..at+len]` when
/// `Some((at, len))`): opens `rel` strictly beneath `root_dir` via `openat`/`O_NOFOLLOW` (Linux:
/// `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)`) and reads straight into `into`'s
/// uninitialized spare capacity (zero `memset(0)`, zero intermediate copy).
pub(crate) fn read_confined_into_vec<A: Allocator>(
    root_dir: &RootDir,
    root: &Path,
    rel: &FileName,
    range: Option<(u64, usize)>,
    into: &mut Vec<u8, A>,
) -> io::Result<LentRead> {
    let file = match open_beneath_readonly(root_dir, root, rel) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(LentRead::NotFound),
        Err(error) => return Err(error),
    };
    let Some((at, len)) = range else {
        return read_open_into_vec(&file, into);
    };
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
    }
    if len == 0 {
        return Ok(LentRead::Read { len: 0 });
    }
    if into.try_reserve(len).is_err() {
        return Ok(LentRead::NoRoom { len: u64::try_from(len).unwrap_or(u64::MAX) });
    }
    let start = into.len();
    let Some(spare) = into.spare_capacity_mut().get_mut(..len) else {
        return Ok(LentRead::NoRoom { len: u64::try_from(len).unwrap_or(u64::MAX) });
    };
    let mut filled = 0usize;
    while filled < len {
        let Some(dst) = spare.get_mut(filled..) else { break };
        let Some(pos) = at.checked_add(u64::try_from(filled).unwrap_or(u64::MAX)) else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "read offset overflow"));
        };
        let n = read_at_uninit(&file, dst, pos)?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    #[expect(unsafe_code, reason = "pread initialized exactly `filled` bytes at `start..start + filled` within reserved capacity")]
    unsafe {
        into.set_len(start + filled);
    }
    Ok(LentRead::Read { len: u64::try_from(filled).unwrap_or(u64::MAX) })
}

/// Opens directory `rel` strictly beneath `root_dir` without following any symbolic link in any
/// component (Linux `openat2(O_DIRECTORY | RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` or Unix
/// component-by-component `openat(O_DIRECTORY | O_NOFOLLOW)`).
#[cfg(unix)]
fn open_beneath_dir(root_dir: &RootDir, root: &Path, rel: &FileName) -> io::Result<OwnedFd> {
    let owned_fallback;
    let base_fd: BorrowedFd<'_> = match root_dir.as_fd() {
        Some(fd) => fd,
        None => {
            owned_fallback = open_dir_fd(root)?;
            owned_fallback.as_fd()
        }
    };
    #[cfg(target_os = "linux")]
    if let Some(res) = try_openat2_beneath_dir(base_fd, rel) {
        return res;
    }
    let mut owned_cur: Option<OwnedFd> = None;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    for part in rel.as_str().split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid relative directory component"));
        }
        let dir_raw = match &owned_cur {
            Some(fd) => fd.as_raw_fd(),
            None => base_fd.as_raw_fd(),
        };
        owned_cur = Some(openat_one(dir_raw, part, flags)?);
    }
    owned_cur.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty relative directory"))
}

#[cfg(target_os = "linux")]
fn try_openat2_beneath_dir(base_fd: BorrowedFd<'_>, rel: &FileName) -> Option<io::Result<OwnedFd>> {
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }
    const RESOLVE_BENEATH: u64 = 0x08;
    const RESOLVE_NO_SYMLINKS: u64 = 0x04;
    let raw_flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
    let Ok(flags) = u64::try_from(raw_flags) else {
        return None;
    };
    let how = OpenHow { flags, mode: 0, resolve: RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS };
    let res = super::with_c_path(Path::new(rel.as_str()), |cpath| {
        let mut interrupted = 0;
        loop {
            #[expect(unsafe_code, reason = "Linux openat2(O_DIRECTORY | RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS) confined directory open")]
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
                let Ok(fd) = libc::c_int::try_from(raw) else {
                    return Err(io::Error::other("openat2 returned out-of-range fd"));
                };
                #[expect(unsafe_code, reason = "fd >= 0 is a freshly opened kernel directory descriptor")]
                return Ok(unsafe { OwnedFd::from_raw_fd(fd) });
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

/// Kernel-confined directory listing: opens `rel` strictly beneath `root_dir` without following
/// any symbolic link (`openat2(O_DIRECTORY | RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` on Linux,
/// component `openat(O_DIRECTORY | O_NOFOLLOW)` on Unix) and reads entries straight from that
/// descriptor via `fdopendir` + `readdir` + `fstatat(AT_SYMLINK_NOFOLLOW)` (zero TOCTOU).
#[cfg(unix)]
pub(crate) fn entries_confined(
    root_dir: &RootDir,
    root: &Path,
    rel: &FileName,
    name: &crate::DirName,
) -> io::Result<crate::Listing> {
    let dir_fd = open_beneath_dir(root_dir, root, rel)?;
    let raw_fd = dir_fd.into_raw_fd();
    #[expect(unsafe_code, reason = "fdopendir takes ownership of the freshly opened directory fd")]
    let dir_ptr = unsafe { libc::fdopendir(raw_fd) };
    if dir_ptr.is_null() {
        let err = io::Error::last_os_error();
        #[expect(unsafe_code, reason = "fdopendir failed so raw_fd is still owned by us and must be closed")]
        unsafe {
            libc::close(raw_fd);
        }
        return Err(err);
    }
    struct DirGuard(*mut libc::DIR);
    impl Drop for DirGuard {
        fn drop(&mut self) {
            #[expect(unsafe_code, reason = "closing the DIR* opened by fdopendir")]
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    let guard = DirGuard(dir_ptr);
    #[expect(unsafe_code, reason = "dirfd borrows the valid DIR* held by guard")]
    let borrowed_raw = unsafe { libc::dirfd(guard.0) };
    #[expect(unsafe_code, reason = "borrowed_raw is the valid fd owned by guard for the loop duration")]
    let borrowed = unsafe { BorrowedFd::borrow_raw(borrowed_raw) };
    const CONFINED_LISTING_MOST: usize = 65_536;
    let mut found = crate::ListingBuilder::new(name.share());
    let mut seen: usize = 0;
    let mut interrupted = 0;
    loop {
        // Clear errno before readdir so we distinguish end-of-stream (errno == 0) from IO error.
        #[cfg(target_os = "linux")]
        #[expect(unsafe_code, reason = "clearing thread-local errno before libc::readdir")]
        unsafe {
            *libc::__errno_location() = 0;
        }
        #[cfg(target_os = "macos")]
        #[expect(unsafe_code, reason = "clearing thread-local errno before libc::readdir")]
        unsafe {
            *libc::__error() = 0;
        }
        #[expect(unsafe_code, reason = "readdir on our exclusively owned DIR*")]
        let ent = unsafe { libc::readdir(guard.0) };
        if ent.is_null() {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) && interrupted < INTERRUPTS_MOST {
                interrupted += 1;
                continue;
            }
            if err.raw_os_error().is_some_and(|code| code != 0) {
                return Err(err);
            }
            break;
        }
        interrupted = 0;
        #[expect(unsafe_code, reason = "ent is non-null from readdir; d_name is NUL-terminated C string")]
        let cname = unsafe { core::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) };
        let bytes = cname.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        if seen >= CONFINED_LISTING_MOST {
            return Err(io::Error::other("confined directory listing exceeds entry limit"));
        }
        seen += 1;
        let stamp = super::stamp_at(borrowed.as_fd(), cname);
        found.push(bytes, stamp)?;
    }
    Ok(found.finish()?)
}

/// Streams `readdir` over `rel` (or `root_dir` when `rel` is `None`) in O(1) memory without
/// materializing a bounded [`crate::Listing`], unlinking any regular-file `<key>.part-<tag>`
/// scratch entry for which `should_remove(name, tag)` returns `true`. Returns `Ok(0)` when the
/// directory does not exist yet.
#[cfg(unix)]
pub(crate) fn sweep_parts_confined(
    root_dir: &RootDir,
    root: &Path,
    rel: Option<&FileName>,
    mut should_remove: impl FnMut(&str, u64) -> bool,
) -> io::Result<u32> {
    let dir_fd = match rel {
        Some(rel) => match open_beneath_dir(root_dir, root, rel) {
            Ok(fd) => fd,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(err) => return Err(err),
        },
        None => match open_dir_fd(root) {
            Ok(fd) => fd,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(err) => return Err(err),
        },
    };
    let raw_fd = dir_fd.into_raw_fd();
    #[expect(unsafe_code, reason = "fdopendir takes ownership of the freshly opened directory fd")]
    let dir_ptr = unsafe { libc::fdopendir(raw_fd) };
    if dir_ptr.is_null() {
        let err = io::Error::last_os_error();
        #[expect(unsafe_code, reason = "fdopendir failed so raw_fd is still owned by us and must be closed")]
        unsafe {
            libc::close(raw_fd);
        }
        return Err(err);
    }
    struct DirGuard(*mut libc::DIR);
    impl Drop for DirGuard {
        fn drop(&mut self) {
            #[expect(unsafe_code, reason = "closing the DIR* opened by fdopendir")]
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    let guard = DirGuard(dir_ptr);
    #[expect(unsafe_code, reason = "dirfd borrows the valid DIR* held by guard")]
    let borrowed_raw = unsafe { libc::dirfd(guard.0) };
    let mut removed = 0u32;
    let mut interrupted = 0;
    loop {
        #[cfg(target_os = "linux")]
        #[expect(unsafe_code, reason = "clearing thread-local errno before libc::readdir")]
        unsafe {
            *libc::__errno_location() = 0;
        }
        #[cfg(target_os = "macos")]
        #[expect(unsafe_code, reason = "clearing thread-local errno before libc::readdir")]
        unsafe {
            *libc::__error() = 0;
        }
        #[expect(unsafe_code, reason = "readdir on our exclusively owned DIR*")]
        let ent = unsafe { libc::readdir(guard.0) };
        if ent.is_null() {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) && interrupted < INTERRUPTS_MOST {
                interrupted += 1;
                continue;
            }
            if err.raw_os_error().is_some_and(|code| code != 0) {
                return Err(err);
            }
            break;
        }
        interrupted = 0;
        #[expect(unsafe_code, reason = "ent is non-null from readdir; d_name is NUL-terminated C string")]
        let cname = unsafe { core::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) };
        let Ok(name) = cname.to_str() else {
            continue;
        };
        let Some(tag) = crate::reserve::part_tag(name) else {
            continue;
        };
        if !should_remove(name, tag) {
            continue;
        }
        let mut st = MaybeUninit::<libc::stat>::uninit();
        #[expect(unsafe_code, reason = "fstatat(AT_SYMLINK_NOFOLLOW) on our open directory fd")]
        let rc = unsafe { libc::fstatat(borrowed_raw, cname.as_ptr(), st.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::NotFound {
                continue;
            }
            return Err(err);
        }
        #[expect(unsafe_code, reason = "fstatat succeeded and initialized st")]
        let st = unsafe { st.assume_init() };
        if (st.st_mode & libc::S_IFMT) != libc::S_IFREG {
            continue;
        }
        #[expect(unsafe_code, reason = "unlinkat on our open directory fd for a verified regular .part- scratch file")]
        let urc = unsafe { libc::unlinkat(borrowed_raw, cname.as_ptr(), 0) };
        if urc == 0 {
            removed = removed.saturating_add(1);
        } else {
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::NotFound {
                return Err(err);
            }
        }
    }
    Ok(removed)
}

#[cfg(not(unix))]
pub(crate) fn entries_confined(
    _root_dir: &RootDir,
    root: &Path,
    rel: &FileName,
    name: &crate::DirName,
) -> io::Result<crate::Listing> {
    super::entries(&root.join(rel.as_str()), name)
}

#[cfg(not(unix))]
pub(crate) fn sweep_parts_confined(
    _root_dir: &RootDir,
    root: &Path,
    rel: Option<&FileName>,
    mut should_remove: impl FnMut(&str, u64) -> bool,
) -> io::Result<u32> {
    let dir = match rel {
        Some(rel) => root.join(rel.as_str()),
        None => root.to_path_buf(),
    };
    let read = match fs::read_dir(&dir) {
        Ok(read) => read,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err),
    };
    let mut removed = 0u32;
    for entry in read {
        let entry = entry?;
        let fname = entry.file_name();
        let Some(name) = fname.to_str() else { continue };
        let Some(tag) = crate::reserve::part_tag(name) else { continue };
        if !should_remove(name, tag) {
            continue;
        }
        if entry.file_type()?.is_file() {
            match fs::remove_file(entry.path()) {
                Ok(()) => removed = removed.saturating_add(1),
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
        }
    }
    Ok(removed)
}
