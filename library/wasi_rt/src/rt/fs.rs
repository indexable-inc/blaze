//! Files, directories and links by path (wasi-libc's posix.c, at_fdcwd.c and cloudlibc's
//! `*at` functions, musl's realpath.c).
//!
//! A function that reads by path resolves it with [`path::at`]/[`path::resolve`] and makes the
//! one WASI path call with the descriptor and relative path that gives. One that changes the
//! file system makes the seed's own call ([`seed`], module `blaze_seed`) with the absolute
//! path [`path::absolute_at`] gives, since its host has no p1 calls that write. Permissions don't exist
//! in WASI preview 1: modes passed to `open`/`mkdir` are ignored, as wasi-libc ignores them, and
//! `chmod`/`fchmod` fail with `ENOSYS`.

use core::ffi::{c_char, c_int, c_long};
use core::mem::MaybeUninit;
use core::ptr;

use libc::{
    AT_FDCWD, AT_REMOVEDIR, AT_SYMLINK_FOLLOW, AT_SYMLINK_NOFOLLOW, EINVAL, ELOOP, ENAMETOOLONG, ENOENT, ENOSYS, ENOTSUP,
    O_ACCMODE, O_EXEC, O_NOFOLLOW, O_RDONLY, O_RDWR, O_SEARCH, O_WRONLY, S_IFBLK, S_IFCHR, S_IFDIR, S_IFLNK, S_IFREG,
    S_IFSOCK, UTIME_NOW, UTIME_OMIT, mode_t, timespec,
};
use wasip1::wasi_snapshot_preview1 as wasi;

use crate::rt::path::{self, Bytes};
use crate::rt::seed;
use crate::{fail, fail_long, ok};

const NSEC_PER_SEC: u64 = 1_000_000_000;

pub(crate) fn timespec_of(ns: u64) -> timespec {
    timespec { tv_sec: (ns / NSEC_PER_SEC) as _, tv_nsec: (ns % NSEC_PER_SEC) as _ }
}

/// A WASI file status as `struct stat` (wasi-libc's `to_public_stat`). WASI has no owners or
/// permission bits, so those are 0; the file type goes in `st_mode`.
pub(crate) unsafe fn to_stat(f: &wasip1::Filestat, out: *mut libc::stat) {
    let mut st: libc::stat = core::mem::zeroed();
    st.st_dev = f.dev;
    st.st_ino = f.ino;
    st.st_nlink = f.nlink;
    st.st_size = f.size as _;
    st.st_atim = timespec_of(f.atim);
    st.st_mtim = timespec_of(f.mtim);
    st.st_ctim = timespec_of(f.ctim);
    st.st_mode = match f.filetype {
        // wasi-libc leaves a block device's type out on preview 1 (and sets it on preview 2).
        wasip1::FILETYPE_BLOCK_DEVICE => S_IFBLK,
        wasip1::FILETYPE_CHARACTER_DEVICE => S_IFCHR,
        wasip1::FILETYPE_DIRECTORY => S_IFDIR,
        wasip1::FILETYPE_REGULAR_FILE => S_IFREG,
        wasip1::FILETYPE_SOCKET_DGRAM | wasip1::FILETYPE_SOCKET_STREAM => S_IFSOCK,
        wasip1::FILETYPE_SYMBOLIC_LINK => S_IFLNK,
        _ => 0,
    };
    ptr::write(out, st);
}

/// `path_filestat_get` under `fd`.
pub(crate) unsafe fn filestat_at(fd: c_int, rel: Bytes, follow: bool) -> Result<wasip1::Filestat, c_int> {
    let mut f = MaybeUninit::<wasip1::Filestat>::uninit();
    let (p, n) = rel.wasi();
    let flags = if follow { wasip1::LOOKUPFLAGS_SYMLINK_FOLLOW } else { 0 };
    ok(wasi::path_filestat_get(fd, flags as i32, p, n, f.as_mut_ptr() as usize))?;
    Ok(f.assume_init())
}

/// Opens `rel` under directory `fd` with `open`'s flags (cloudlibc's openat.c): asks for every
/// right the directory passes on except the ones the access mode rules out.
pub(crate) unsafe fn open_under(fd: c_int, rel: Bytes, oflag: c_int) -> Result<c_int, c_int> {
    use wasip1::{
        RIGHTS_FD_ALLOCATE, RIGHTS_FD_DATASYNC, RIGHTS_FD_FILESTAT_SET_SIZE, RIGHTS_FD_READ, RIGHTS_FD_READDIR,
        RIGHTS_FD_WRITE,
    };
    let mut max = !(RIGHTS_FD_DATASYNC
        | RIGHTS_FD_READ
        | RIGHTS_FD_WRITE
        | RIGHTS_FD_ALLOCATE
        | RIGHTS_FD_READDIR
        | RIGHTS_FD_FILESTAT_SET_SIZE);
    match oflag & O_ACCMODE {
        O_RDONLY | O_RDWR | O_WRONLY => {
            if oflag & O_RDONLY != 0 {
                max |= RIGHTS_FD_READ | RIGHTS_FD_READDIR;
            }
            if oflag & O_WRONLY != 0 {
                max |= RIGHTS_FD_DATASYNC | RIGHTS_FD_WRITE | RIGHTS_FD_ALLOCATE | RIGHTS_FD_FILESTAT_SET_SIZE;
            }
        }
        O_EXEC | O_SEARCH => {}
        _ => return Err(EINVAL),
    }
    let dir = wasip1::fd_fdstat_get(fd as u32).map_err(|e| c_int::from(e.raw()))?;
    let lookup = if oflag & O_NOFOLLOW == 0 { wasip1::LOOKUPFLAGS_SYMLINK_FOLLOW } else { 0 };
    let (p, n) = rel.wasi();
    let mut opened: u32 = 0;
    ok(wasi::path_open(
        fd,
        lookup as i32,
        p,
        n,
        (oflag >> 12) & 0xfff,
        (max & dir.fs_rights_inheriting) as i64,
        dir.fs_rights_inheriting as i64,
        oflag & 0xfff,
        (&raw mut opened) as usize,
    ))?;
    Ok(opened as c_int)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn open(path: *const c_char, oflag: c_int, _mode: ...) -> c_int {
    openat_impl(AT_FDCWD, path, oflag)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn openat(dirfd: c_int, path: *const c_char, oflag: c_int, _mode: ...) -> c_int {
    openat_impl(dirfd, path, oflag)
}

/// Opens `path` under `dirfd` and keeps the absolute path it opened, which an `*at` call under
/// the new descriptor names its path from ([`path::absolute_at`]).
pub(crate) unsafe fn openat_impl(dirfd: c_int, path: *const c_char, oflag: c_int) -> c_int {
    let opened = path::absolute_at(dirfd, path, 1).and_then(|abs| {
        let fd = path::at(dirfd, path, 0).and_then(|(fd, rel)| open_under(fd, rel, oflag))?;
        match path::opened(fd, abs) {
            Ok(()) => Ok(fd),
            Err(e) => {
                wasi::fd_close(fd);
                Err(e)
            }
        }
    });
    match opened {
        Ok(fd) => fd,
        Err(e) => fail(e),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fstatat(dirfd: c_int, path: *const c_char, buf: *mut libc::stat, flags: c_int) -> c_int {
    if flags & !AT_SYMLINK_NOFOLLOW != 0 {
        return fail(EINVAL);
    }
    let follow = flags & AT_SYMLINK_NOFOLLOW == 0;
    match path::at(dirfd, path, 0).and_then(|(fd, rel)| filestat_at(fd, rel, follow)) {
        Ok(f) => {
            to_stat(&f, buf);
            0
        }
        Err(e) => fail(e),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn stat(path: *const c_char, buf: *mut libc::stat) -> c_int {
    fstatat(AT_FDCWD, path, buf, 0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lstat(path: *const c_char, buf: *mut libc::stat) -> c_int {
    fstatat(AT_FDCWD, path, buf, AT_SYMLINK_NOFOLLOW)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fstat(fd: c_int, buf: *mut libc::stat) -> c_int {
    match wasip1::fd_filestat_get(fd as u32) {
        Ok(f) => {
            to_stat(&f, buf);
            0
        }
        Err(e) => fail(e.raw().into()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mkdirat(dirfd: c_int, path: *const c_char, _mode: mode_t) -> c_int {
    status(path::absolute_at(dirfd, path, 0).and_then(|abs| seed::create_dir(abs)))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mkdir(path: *const c_char, mode: mode_t) -> c_int {
    mkdirat(AT_FDCWD, path, mode)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn unlinkat(dirfd: c_int, path: *const c_char, flags: c_int) -> c_int {
    if flags & !AT_REMOVEDIR != 0 {
        return fail(EINVAL);
    }
    let r = path::absolute_at(dirfd, path, 0).and_then(|abs| {
        if flags & AT_REMOVEDIR != 0 { seed::remove_dir(abs) } else { seed::unlink(abs) }
    });
    status(r)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn unlink(path: *const c_char) -> c_int {
    unlinkat(AT_FDCWD, path, 0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rmdir(path: *const c_char) -> c_int {
    unlinkat(AT_FDCWD, path, AT_REMOVEDIR)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn renameat(
    olddirfd: c_int,
    old: *const c_char,
    newdirfd: c_int,
    new: *const c_char,
) -> c_int {
    let r = path::absolute_at(olddirfd, old, 0).and_then(|from| {
        let to = path::absolute_at(newdirfd, new, 1)?;
        seed::rename(from, to)
    });
    status(r)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rename(old: *const c_char, new: *const c_char) -> c_int {
    renameat(AT_FDCWD, old, AT_FDCWD, new)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn linkat(
    olddirfd: c_int,
    old: *const c_char,
    newdirfd: c_int,
    new: *const c_char,
    flags: c_int,
) -> c_int {
    if flags & !AT_SYMLINK_FOLLOW != 0 {
        return fail(EINVAL);
    }
    // The seed's `hard_link` links the path itself; following a link first is not one of its
    // calls (`link`, std's `hard_link`, passes no flag).
    if flags & AT_SYMLINK_FOLLOW != 0 {
        return fail(ENOTSUP);
    }
    let r = path::absolute_at(olddirfd, old, 0).and_then(|from| {
        let to = path::absolute_at(newdirfd, new, 1)?;
        seed::hard_link(from, to)
    });
    status(r)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn link(old: *const c_char, new: *const c_char) -> c_int {
    linkat(AT_FDCWD, old, AT_FDCWD, new, 0)
}

/// The link's contents are stored as given, never resolved.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn symlinkat(target: *const c_char, dirfd: c_int, path: *const c_char) -> c_int {
    let target = crate::cstr(target);
    status(path::absolute_at(dirfd, path, 0).and_then(|abs| seed::symlink(target, abs)))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn symlink(target: *const c_char, path: *const c_char) -> c_int {
    symlinkat(target, AT_FDCWD, path)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn readlinkat(dirfd: c_int, path: *const c_char, buf: *mut c_char, size: usize) -> isize {
    let r = path::at(dirfd, path, 0).and_then(|(fd, rel)| {
        let (p, n) = rel.wasi();
        let mut used: usize = 0;
        ok(wasi::path_readlink(fd, p, n, buf as usize, size, (&raw mut used) as usize))?;
        Ok(used)
    });
    match r {
        Ok(used) => used as isize,
        Err(e) => fail_long(e) as isize,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn readlink(path: *const c_char, buf: *mut c_char, size: usize) -> isize {
    readlinkat(AT_FDCWD, path, buf, size)
}

/// A `timespec` as WASI nanoseconds, refusing what it can't hold exactly (cloudlibc's
/// `timespec_to_timestamp_exact`).
fn timestamp_exact(t: &timespec) -> Option<u64> {
    let nsec = t.tv_nsec as i64;
    if nsec < 0 || nsec >= NSEC_PER_SEC as i64 || t.tv_sec < 0 {
        return None;
    }
    (t.tv_sec as u64).checked_mul(NSEC_PER_SEC)?.checked_add(nsec as u64)
}

/// `utimensat`'s times as the seed's (atime, mtime): each [`seed::KEEP`], [`seed::NOW`] or
/// nanoseconds since the epoch (wasi-libc's `utimens_get_timestamps`). A time the seed can't
/// tell from those two is `EINVAL`.
pub(crate) unsafe fn utimens_times(times: *const timespec) -> Result<(u64, u64), c_int> {
    if times.is_null() {
        return Ok((seed::NOW, seed::NOW));
    }
    let mut out = [0u64; 2];
    for (i, slot) in out.iter_mut().enumerate() {
        let t = &*times.add(i);
        *slot = match t.tv_nsec as c_long {
            UTIME_NOW => seed::NOW,
            UTIME_OMIT => seed::KEEP,
            _ => timestamp_exact(t).filter(|&ns| ns < seed::NOW).ok_or(EINVAL)?,
        };
    }
    Ok((out[0], out[1]))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn utimensat(dirfd: c_int, path: *const c_char, times: *const timespec, flags: c_int) -> c_int {
    if flags & !AT_SYMLINK_NOFOLLOW != 0 {
        return fail(EINVAL);
    }
    let follow = flags & AT_SYMLINK_NOFOLLOW == 0;
    let r = utimens_times(times).and_then(|(atime, mtime)| {
        let abs = path::absolute_at(dirfd, path, 0)?;
        seed::set_times(abs, follow, atime, mtime)
    });
    status(r)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn chmod(_path: *const c_char, _mode: mode_t) -> c_int {
    fail(ENOSYS)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fchmod(_fd: c_int, _mode: mode_t) -> c_int {
    fail(ENOSYS)
}

pub(crate) unsafe fn status(r: Result<(), c_int>) -> c_int {
    match r {
        Ok(()) => 0,
        Err(e) => fail(e),
    }
}

/// musl's `realpath`, over this runtime's `readlink` and `getcwd`: resolves `.`, `..` and
/// symbolic links component by component, without touching the cwd.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn realpath(filename: *const c_char, resolved: *mut c_char) -> *mut c_char {
    match realpath_impl(filename, resolved) {
        Ok(p) => p,
        Err(e) => {
            fail(e);
            ptr::null_mut()
        }
    }
}

const PATH_MAX: usize = 4096;
const SYMLOOP_MAX: usize = 40;

unsafe fn realpath_impl(filename: *const c_char, resolved: *mut c_char) -> Result<*mut c_char, c_int> {
    // The path still to resolve sits at the end of `stack`, from `p`; resolved components go to
    // `output`, up to `q`. A link's contents are pushed onto the front of what remains.
    let mut stack = [0u8; PATH_MAX + 1];
    let mut output = [0u8; PATH_MAX];
    if filename.is_null() {
        return Err(EINVAL);
    }
    let l = crate::rt::mem::strnlen(filename, stack.len());
    if l == 0 {
        return Err(ENOENT);
    }
    if l >= PATH_MAX {
        return Err(ENAMETOOLONG);
    }
    let mut p = stack.len() - l - 1;
    let mut q = 0usize;
    ptr::copy_nonoverlapping(filename.cast::<u8>(), stack.as_mut_ptr().add(p), l + 1);
    let (mut links, mut nup) = (0usize, 0usize);
    let mut check_dir = false;

    'restart: loop {
        let mut first = true;
        loop {
            if !first {
                while stack[p] == b'/' {
                    p += 1;
                }
            }
            first = false;

            // A leading `/` resets the output to the root.
            if stack[p] == b'/' {
                check_dir = false;
                nup = 0;
                q = 0;
                output[q] = b'/';
                q += 1;
                p += 1;
                // An initial `//` (exactly two) is kept.
                if stack[p] == b'/' && stack[p + 1] != b'/' {
                    output[q] = b'/';
                    q += 1;
                }
                continue;
            }

            let mut z = p;
            while stack[z] != 0 && stack[z] != b'/' {
                z += 1;
            }
            let l0 = z - p;
            let mut l = l0;
            if l == 0 && !check_dir {
                break 'restart;
            }
            // `.` is skipped, keeping check_dir.
            if l == 1 && stack[p] == b'.' {
                p += l;
                continue;
            }
            // Copy the component onto the output to readlink it; only keep it once it is
            // known not to be a link.
            if q > 0 && output[q - 1] != b'/' {
                if p == 0 {
                    return Err(ENAMETOOLONG);
                }
                p -= 1;
                stack[p] = b'/';
                l += 1;
            }
            if q + l >= PATH_MAX {
                return Err(ENAMETOOLONG);
            }
            output[q..q + l].copy_from_slice(&stack[p..p + l]);
            output[q + l] = 0;
            p += l;

            let mut up = false;
            let mut read_link = true;
            if l0 == 2 && stack[p - 2] == b'.' && stack[p - 1] == b'.' {
                up = true;
                // `..` with nothing before it to cancel is kept, to apply to the cwd later.
                if q <= 3 * nup {
                    nup += 1;
                    q += l;
                    continue;
                }
                // When what came before is known to be directories, `..` needs no readlink.
                if !check_dir {
                    read_link = false;
                }
            }
            let mut k = -1isize;
            if read_link {
                k = readlink(output.as_ptr().cast(), stack.as_mut_ptr().cast(), p);
                if k == p as isize {
                    return Err(ENAMETOOLONG);
                }
                if k == 0 {
                    return Err(ENOENT);
                }
                if k < 0 && crate::rt::errno::get() != EINVAL {
                    return Err(crate::rt::errno::get());
                }
            }
            if k < 0 {
                // Not a link (readlink's EINVAL), or a `..` that needs no check.
                check_dir = false;
                if up {
                    while q > 0 && output[q - 1] != b'/' {
                        q -= 1;
                    }
                    if q > 1 && (q > 2 || output[0] != b'/') {
                        q -= 1;
                    }
                    continue;
                }
                if l0 != 0 {
                    q += l;
                }
                check_dir = stack[p] != 0;
                continue;
            }
            let k = k as usize;
            links += 1;
            if links == SYMLOOP_MAX {
                return Err(ELOOP);
            }
            // Link contents ending in `/`: drop the slashes already on the stack.
            if stack[k - 1] == b'/' {
                while stack[p] == b'/' {
                    p += 1;
                }
            }
            p -= k;
            stack.copy_within(0..k, p);
            continue 'restart;
        }
    }

    output[q] = 0;

    if output[0] != b'/' {
        if crate::rt::path::getcwd(stack.as_mut_ptr().cast(), stack.len()).is_null() {
            return Err(crate::rt::errno::get());
        }
        let mut l = crate::rt::mem::strlen(stack.as_ptr().cast());
        // Cancel the leading `..` components against the cwd.
        let mut p = 0usize;
        while nup > 0 {
            nup -= 1;
            while l > 1 && stack[l - 1] != b'/' {
                l -= 1;
            }
            if l > 1 {
                l -= 1;
            }
            p += 2;
            if p < q {
                p += 1;
            }
        }
        if q - p != 0 && stack[l - 1] != b'/' {
            stack[l] = b'/';
            l += 1;
        }
        if l + (q - p) + 1 >= PATH_MAX {
            return Err(ENAMETOOLONG);
        }
        output.copy_within(p..q + 1, l);
        output[..l].copy_from_slice(&stack[..l]);
        q = l + q - p;
    }

    if resolved.is_null() {
        let copy = crate::rt::mem::dup(&output[..q]);
        if copy.is_null() {
            return Err(crate::rt::errno::get());
        }
        Ok(copy)
    } else {
        ptr::copy_nonoverlapping(output.as_ptr(), resolved.cast::<u8>(), q + 1);
        Ok(resolved)
    }
}
