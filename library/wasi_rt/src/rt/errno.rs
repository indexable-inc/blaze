//! `errno` and the messages for its values.
//!
//! The values are WASI's own errno numbers, which libc's wasi module uses as its `E*` constants.

use core::ffi::{CStr, c_char, c_int};

use libc::*;

/// std reads and writes this as `#[thread_local] static errno` (std/src/sys/io/error/unix.rs);
/// without the atomics feature a wasm thread-local is an ordinary global.
#[thread_local]
#[unsafe(no_mangle)]
pub static mut errno: c_int = 0;

#[inline]
pub(crate) unsafe fn set(value: c_int) {
    errno = value;
}

#[inline]
pub(crate) unsafe fn get() -> c_int {
    errno
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __errno_location() -> *mut c_int {
    &raw mut errno
}

/// musl's message for each errno WASI has (wasi-libc's `__strerror.h`).
fn message(e: c_int) -> &'static CStr {
    match e {
        0 => c"Success",
        EILSEQ => c"Illegal byte sequence",
        EDOM => c"Domain error",
        ERANGE => c"Result not representable",
        ENOTTY => c"Not a tty",
        EACCES => c"Permission denied",
        EPERM => c"Operation not permitted",
        ENOENT => c"No such file or directory",
        ESRCH => c"No such process",
        EEXIST => c"File exists",
        EOVERFLOW => c"Value too large for data type",
        ENOSPC => c"No space left on device",
        ENOMEM => c"Out of memory",
        EBUSY => c"Resource busy",
        EINTR => c"Interrupted system call",
        EAGAIN => c"Resource temporarily unavailable",
        ESPIPE => c"Invalid seek",
        EXDEV => c"Cross-device link",
        EROFS => c"Read-only file system",
        ENOTEMPTY => c"Directory not empty",
        ECONNRESET => c"Connection reset by peer",
        ETIMEDOUT => c"Operation timed out",
        ECONNREFUSED => c"Connection refused",
        EHOSTUNREACH => c"Host is unreachable",
        EADDRINUSE => c"Address in use",
        EPIPE => c"Broken pipe",
        EIO => c"I/O error",
        ENXIO => c"No such device or address",
        ENODEV => c"No such device",
        ENOTDIR => c"Not a directory",
        EISDIR => c"Is a directory",
        ETXTBSY => c"Text file busy",
        ENOEXEC => c"Exec format error",
        EINVAL => c"Invalid argument",
        E2BIG => c"Argument list too long",
        ELOOP => c"Symbolic link loop",
        ENAMETOOLONG => c"Filename too long",
        ENFILE => c"Too many open files in system",
        EMFILE => c"No file descriptors available",
        EBADF => c"Bad file descriptor",
        ECHILD => c"No child process",
        EFAULT => c"Bad address",
        EFBIG => c"File too large",
        EMLINK => c"Too many links",
        ENOLCK => c"No locks available",
        EDEADLK => c"Resource deadlock would occur",
        ENOTRECOVERABLE => c"State not recoverable",
        EOWNERDEAD => c"Previous owner died",
        ECANCELED => c"Operation canceled",
        ENOSYS => c"Function not implemented",
        ENOMSG => c"No message of desired type",
        EIDRM => c"Identifier removed",
        ENOLINK => c"Link has been severed",
        EPROTO => c"Protocol error",
        EBADMSG => c"Bad message",
        ENOTSOCK => c"Not a socket",
        EDESTADDRREQ => c"Destination address required",
        EMSGSIZE => c"Message too large",
        EPROTOTYPE => c"Protocol wrong type for socket",
        ENOPROTOOPT => c"Protocol not available",
        EPROTONOSUPPORT => c"Protocol not supported",
        ENOTSUP => c"Not supported",
        EAFNOSUPPORT => c"Address family not supported by protocol",
        EADDRNOTAVAIL => c"Address not available",
        ENETDOWN => c"Network is down",
        ENETUNREACH => c"Network unreachable",
        ENETRESET => c"Connection reset by network",
        ECONNABORTED => c"Connection aborted",
        ENOBUFS => c"No buffer space available",
        EISCONN => c"Socket is connected",
        ENOTCONN => c"Socket not connected",
        EALREADY => c"Operation already in progress",
        EINPROGRESS => c"Operation in progress",
        ESTALE => c"Stale file handle",
        EDQUOT => c"Quota exceeded",
        EMULTIHOP => c"Multihop attempted",
        ENOTCAPABLE => c"Capabilities insufficient",
        // musl (and so wasi-libc) says "Success" for a number it doesn't know; that misleads.
        _ => c"No error information",
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn strerror(e: c_int) -> *mut c_char {
    message(e).as_ptr().cast_mut()
}

/// The XSI `strerror_r`: the message, truncated to fit with `ERANGE` when `buf` is too small.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strerror_r(e: c_int, buf: *mut c_char, len: usize) -> c_int {
    let msg = message(e).to_bytes();
    let buf = buf.cast::<u8>();
    if msg.len() >= len {
        if len != 0 {
            core::ptr::copy_nonoverlapping(msg.as_ptr(), buf, len - 1);
            *buf.add(len - 1) = 0;
        }
        return ERANGE;
    }
    core::ptr::copy_nonoverlapping(msg.as_ptr(), buf, msg.len());
    *buf.add(msg.len()) = 0;
    0
}
