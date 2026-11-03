//! Operations on open descriptors (cloudlibc's unistd, sys/uio and fcntl, wasi-libc's isatty.c
//! and `__wasilibc_fd_renumber.c`).
//!
//! WASI reports a missing right as `ENOTCAPABLE`; where POSIX has a better answer (reading a
//! write-only descriptor is `EBADF`, seeking a pipe `ESPIPE`) it is translated, as wasi-libc does.
//! Positioned reads and writes, syncs, truncation and times are the seed's own calls
//! ([`seed`], module `blaze_seed`), since its host has none of p1's.

use core::ffi::{c_int, c_void};

use libc::{
    EBADF, EINVAL, ENOTCAPABLE, ENOTSUP, ENOTTY, ESPIPE, FD_CLOEXEC, F_GETFD, F_GETFL, F_SETFD, F_SETFL, O_RDONLY, O_RDWR,
    O_SEARCH, O_WRONLY, iovec, off_t, timespec,
};
use wasip1::wasi_snapshot_preview1 as wasi;

use crate::rt::fs::{status, utimens_times};
use crate::rt::{path, seed};
use crate::{fail, fail_long, ok};

#[unsafe(no_mangle)]
pub unsafe extern "C" fn close(fd: c_int) -> c_int {
    path::before_close();
    match ok(wasi::fd_close(fd)) {
        Ok(()) => {
            path::closed(fd);
            0
        }
        Err(e) => fail(e),
    }
}

/// Would make `newfd` refer to what `fd` did (WASI's `fd_renumber`): the seed's host has no
/// such call, so it is `ENOTSUP` (std calls it only to replace a stdio descriptor).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __wasilibc_fd_renumber(_fd: c_int, _newfd: c_int) -> c_int {
    fail(ENOTSUP)
}

/// `ENOTCAPABLE` as `EBADF`: the descriptor isn't open for this.
fn badf(e: c_int) -> c_int {
    if e == ENOTCAPABLE { EBADF } else { e }
}

unsafe fn readv_impl(fd: c_int, iov: *const iovec, count: usize) -> isize {
    let mut n: usize = 0;
    match ok(wasi::fd_read(fd, iov as usize, count, (&raw mut n) as usize)) {
        Ok(()) => n as isize,
        Err(e) => fail_long(badf(e)) as isize,
    }
}

unsafe fn writev_impl(fd: c_int, iov: *const iovec, count: usize) -> isize {
    let mut n: usize = 0;
    match ok(wasi::fd_write(fd, iov as usize, count, (&raw mut n) as usize)) {
        Ok(()) => n as isize,
        Err(e) => fail_long(badf(e)) as isize,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn read(fd: c_int, buf: *mut c_void, len: usize) -> isize {
    let iov = iovec { iov_base: buf, iov_len: len };
    readv_impl(fd, &iov, 1)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn write(fd: c_int, buf: *const c_void, len: usize) -> isize {
    let iov = iovec { iov_base: buf.cast_mut(), iov_len: len };
    writev_impl(fd, &iov, 1)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn readv(fd: c_int, iov: *const iovec, count: c_int) -> isize {
    if count < 0 {
        return fail_long(EINVAL) as isize;
    }
    readv_impl(fd, iov, count as usize)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn writev(fd: c_int, iov: *const iovec, count: c_int) -> isize {
    if count < 0 {
        return fail_long(EINVAL) as isize;
    }
    writev_impl(fd, iov, count as usize)
}

/// A positioned read or write refused with `ENOTCAPABLE`: `EBADF` when the descriptor lacks
/// the right, otherwise it can't be positioned (`ESPIPE`).
unsafe fn positioned_error(fd: c_int, e: c_int, right: wasip1::Rights) -> c_int {
    if e != ENOTCAPABLE {
        return e;
    }
    match wasip1::fd_fdstat_get(fd as u32) {
        Ok(st) if st.fs_rights_base & right == 0 => EBADF,
        Ok(_) => ESPIPE,
        Err(_) => e,
    }
}

/// Each buffer of `iov` in turn at `offset` on, through the seed's `pread` or `pwrite`
/// (`write`), until one moves fewer bytes than it holds: how many moved. An error after some
/// bytes moved ends the call with those, as a short read or write.
unsafe fn positioned(fd: c_int, iov: *const iovec, count: usize, offset: off_t, write: bool) -> isize {
    if offset < 0 {
        return fail_long(EINVAL) as isize;
    }
    let mut done: usize = 0;
    for i in 0..count {
        let buf = &*iov.add(i);
        let at = offset as u64 + done as u64;
        let moved = match write {
            true => seed::pwrite(fd, buf.iov_base.cast::<u8>(), buf.iov_len, at),
            false => seed::pread(fd, buf.iov_base.cast::<u8>(), buf.iov_len, at),
        };
        match moved {
            Ok(n) => {
                done += n;
                if n < buf.iov_len {
                    break;
                }
            }
            Err(_) if done > 0 => break,
            Err(e) => {
                let right = if write { wasip1::RIGHTS_FD_WRITE } else { wasip1::RIGHTS_FD_READ };
                return fail_long(positioned_error(fd, e, right)) as isize;
            }
        }
    }
    done as isize
}

unsafe fn preadv_impl(fd: c_int, iov: *const iovec, count: usize, offset: off_t) -> isize {
    positioned(fd, iov, count, offset, false)
}

unsafe fn pwritev_impl(fd: c_int, iov: *const iovec, count: usize, offset: off_t) -> isize {
    positioned(fd, iov, count, offset, true)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pread(fd: c_int, buf: *mut c_void, len: usize, offset: off_t) -> isize {
    let iov = iovec { iov_base: buf, iov_len: len };
    preadv_impl(fd, &iov, 1, offset)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pwrite(fd: c_int, buf: *const c_void, len: usize, offset: off_t) -> isize {
    let iov = iovec { iov_base: buf.cast_mut(), iov_len: len };
    pwritev_impl(fd, &iov, 1, offset)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn preadv(fd: c_int, iov: *const iovec, count: c_int, offset: off_t) -> isize {
    if count < 0 {
        return fail_long(EINVAL) as isize;
    }
    preadv_impl(fd, iov, count as usize, offset)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pwritev(fd: c_int, iov: *const iovec, count: c_int, offset: off_t) -> isize {
    if count < 0 {
        return fail_long(EINVAL) as isize;
    }
    pwritev_impl(fd, iov, count as usize, offset)
}

/// `SEEK_SET`/`SEEK_CUR`/`SEEK_END` are WASI's whence values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lseek(fd: c_int, offset: off_t, whence: c_int) -> off_t {
    let mut at: u64 = 0;
    match ok(wasi::fd_seek(fd, offset, whence, (&raw mut at) as usize)) {
        Ok(()) => at as off_t,
        Err(e) => fail_long(if e == ENOTCAPABLE { ESPIPE } else { e }),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsync(fd: c_int) -> c_int {
    // POSIX: EINVAL for a descriptor that can't be synced.
    status(seed::sync(fd, false).map_err(|e| if e == ENOTCAPABLE { EINVAL } else { e }))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fdatasync(fd: c_int) -> c_int {
    status(seed::sync(fd, true).map_err(badf))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ftruncate(fd: c_int, len: off_t) -> c_int {
    if len < 0 {
        return fail(EINVAL);
    }
    status(seed::set_len(fd, len as u64))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn futimens(fd: c_int, times: *const timespec) -> c_int {
    status(utimens_times(times).and_then(|(atime, mtime)| seed::fd_set_times(fd, atime, mtime)))
}

/// `F_GETFD`/`F_SETFD`, `F_GETFL`/`F_SETFL`; other commands are `EINVAL`. WASI has no `exec`,
/// so every descriptor reads as close-on-exec. Unlike wasi-libc, `F_GETFD`/`F_SETFD` check the
/// descriptor, so a closed one is `EBADF` (std asks this to tell whether an fd is open).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fcntl(fd: c_int, cmd: c_int, mut args: ...) -> c_int {
    match cmd {
        F_GETFD | F_SETFD => match wasip1::fd_fdstat_get(fd as u32) {
            Ok(_) => {
                if cmd == F_GETFD {
                    FD_CLOEXEC
                } else {
                    0
                }
            }
            Err(e) => fail(e.raw().into()),
        },
        F_GETFL => match wasip1::fd_fdstat_get(fd as u32) {
            // The access mode, approximated from the rights.
            Ok(st) => {
                let rights = st.fs_rights_base;
                let readable = rights & (wasip1::RIGHTS_FD_READ | wasip1::RIGHTS_FD_READDIR) != 0;
                let writable = rights & wasip1::RIGHTS_FD_WRITE != 0;
                let mode = match (readable, writable) {
                    (true, true) => O_RDWR,
                    (true, false) => O_RDONLY,
                    (false, true) => O_WRONLY,
                    (false, false) => O_SEARCH,
                };
                c_int::from(st.fs_flags) | mode
            }
            Err(e) => fail(e.raw().into()),
        },
        F_SETFL => {
            let flags = args.next_arg::<c_int>();
            status(ok(wasi::fd_fdstat_set_flags(fd, flags & 0xfff)))
        }
        _ => fail(EINVAL),
    }
}

/// A terminal is a character device that can't seek or tell; anything else is `ENOTTY`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn isatty(fd: c_int) -> c_int {
    match wasip1::fd_fdstat_get(fd as u32) {
        Ok(st)
            if st.fs_filetype == wasip1::FILETYPE_CHARACTER_DEVICE
                && st.fs_rights_base & (wasip1::RIGHTS_FD_SEEK | wasip1::RIGHTS_FD_TELL) == 0 =>
        {
            1
        }
        Ok(_) => {
            fail(ENOTTY);
            0
        }
        Err(e) => {
            fail(e.raw().into());
            0
        }
    }
}
