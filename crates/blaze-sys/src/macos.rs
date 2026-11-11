//! macOS, through libSystem.
//!
//! macOS's system call numbers are not a stable interface; libSystem's symbols are. This
//! layer declares them itself (no `libc` crate) and, where rule macos-private-apis applies,
//! declares the private or undocumented one, with which and why in [`SYMBOLS`]. The test
//! `every_declared_symbol_exists` looks each up with `dlsym`, so a symbol Apple removes fails
//! a test on the first macOS that lacks it, and never a link of user code.
//!
//! Slice 1 builds the declarations and that test. No typed operation is built: each is an
//! [`crate::Unbuilt`] naming itself ([`UNBUILT`]), and blaze-core's macOS drivers stay on the
//! ban list (sys.md, "Migration").

use core::ffi::{CStr, c_char, c_int, c_uint, c_void};

/// Whether a symbol is in Apple's public headers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Api {
    /// Declared in the SDK's public headers.
    Public,
    /// Private or undocumented: exported by libSystem, declared in no public header.
    Private,
}

/// One libSystem symbol this layer declares: its name, whether it is public, and what blaze
/// uses it for (for a private one: why it and not the public call).
#[derive(Clone, Copy, Debug)]
pub struct Symbol {
    pub name: &'static CStr,
    pub api: Api,
    pub why: &'static str,
}

const fn public(name: &'static CStr, why: &'static str) -> Symbol {
    Symbol { name, api: Api::Public, why }
}

const fn private(name: &'static CStr, why: &'static str) -> Symbol {
    Symbol { name, api: Api::Private, why }
}

/// Every symbol declared in [`ffi`], which the symbol test looks up.
pub const SYMBOLS: &[Symbol] = &[
    private(
        c"__ulock_wait2",
        "parking: a compare-and-wait on a 32-bit word with a nanosecond timeout, the futex of \
         macOS; the public `os_sync_wait_on_address_with_timeout` is macOS 14.4 and later, and \
         a pthread condition variable costs a mutex and two calls a park",
    ),
    private(
        c"__ulock_wait",
        "parking before macOS 11, which has no `__ulock_wait2`: the same wait with a \
         microsecond timeout",
    ),
    private(c"__ulock_wake", "the wake of `__ulock_wait2`: one waiter or all, by address"),
    private(
        c"kevent_qos",
        "the kqueue driver's submit and wait: `kevent64` with a QoS per event and \
         `KEVENT_FLAG_*` (no allocation in the kernel for the error path, a workloop's events); \
         `kevent` has neither",
    ),
    private(
        c"kevent_id",
        "workloops: a kqueue the kernel schedules a thread for by id, so a completion wakes \
         the worker that owns it and not a shared poller",
    ),
    private(
        c"memorystatus_control",
        "the process's footprint limit and pressure level, the number the kernel enforces \
         (rule one-memory-measure); no public call reads the limit",
    ),
    private(
        c"os_unfair_lock_lock_with_options",
        "a lock's slow path with `OS_UNFAIR_LOCK_ADAPTIVE_SPIN`: spins while the owner runs; \
         the public `os_unfair_lock_lock` parks at once",
    ),
    public(c"os_unfair_lock_lock", "the lock under `blaze_core::lock`'s named locks"),
    public(c"os_unfair_lock_trylock", "its try"),
    public(c"os_unfair_lock_unlock", "its unlock"),
    public(c"clonefile", "`Effect::Clone` by path: a copy-on-write clone on APFS"),
    public(c"fclonefileat", "`Effect::Clone` from an open file to a name beneath a directory"),
    public(
        c"fcopyfile",
        "`copy_to` between open files (`COPYFILE_DATA`); never where a clone was asked for",
    ),
    public(
        c"fcntl",
        "`F_BARRIERFSYNC` (85: the file's bytes ordered before later writes, no drive-cache \
         flush) for `Durable::Started` and the journal's barrier, `F_FULLFSYNC` (51: through \
         the drive's cache) for `Durable::Yes`, `F_NOCACHE` (48) for large aligned writes",
    ),
    public(c"renameatx_np", "`Effect::Rename` with `RENAME_EXCL` and `RENAME_SWAP`"),
    public(c"openat", "opens, with `O_RESOLVE_BENEATH` (macOS 15) for a handle's"),
    public(c"mach_absolute_time", "the monotonic clock: one read of the commpage, no call"),
    public(c"mach_continuous_time", "the clock that counts sleep, for deadlines across it"),
    public(c"getentropy", "the seed of `Io::rng` and of the maps' hasher"),
    public(c"__error", "the thread's `errno`, read after a call that returned -1"),
    public(c"dlsym", "the symbol test itself, and optional symbols by name"),
];

/// The declarations. Nothing calls them in slice 1 but the symbol test's `dlsym`.
#[allow(dead_code, reason = "declared for the slices that build on them; each is looked up by the symbol test")]
#[allow(clashing_extern_declarations, reason = "this crate's own typed signatures, not the libc crate's")]
pub(crate) mod ffi {
    use super::{c_char, c_int, c_uint, c_void};

    /// `struct kevent_qos_s`.
    #[repr(C)]
    pub(crate) struct KeventQos {
        pub(crate) ident: u64,
        pub(crate) filter: i16,
        pub(crate) flags: u16,
        pub(crate) qos: i32,
        pub(crate) udata: u64,
        pub(crate) fflags: u32,
        pub(crate) xflags: u32,
        pub(crate) data: i64,
        pub(crate) ext: [u64; 4],
    }

    unsafe extern "C" {
        pub(crate) fn __ulock_wait2(operation: u32, address: *mut c_void, value: u64, timeout_ns: u64, value2: u64) -> c_int;
        pub(crate) fn __ulock_wait(operation: u32, address: *mut c_void, value: u64, timeout_us: u32) -> c_int;
        pub(crate) fn __ulock_wake(operation: u32, address: *mut c_void, wake_value: u64) -> c_int;
        pub(crate) fn kevent_qos(
            kq: c_int,
            changes: *const KeventQos,
            nchanges: c_int,
            events: *mut KeventQos,
            nevents: c_int,
            data_out: *mut c_void,
            data_available: *mut usize,
            flags: c_uint,
        ) -> c_int;
        pub(crate) fn kevent_id(
            id: u64,
            changes: *const KeventQos,
            nchanges: c_int,
            events: *mut KeventQos,
            nevents: c_int,
            data_out: *mut c_void,
            data_available: *mut usize,
            flags: c_uint,
        ) -> c_int;
        pub(crate) fn memorystatus_control(command: u32, pid: i32, flags: u32, buffer: *mut c_void, size: usize) -> c_int;
        pub(crate) fn os_unfair_lock_lock_with_options(lock: *mut u32, options: u32);
        pub(crate) fn os_unfair_lock_lock(lock: *mut u32);
        pub(crate) fn os_unfair_lock_trylock(lock: *mut u32) -> bool;
        pub(crate) fn os_unfair_lock_unlock(lock: *mut u32);
        pub(crate) fn clonefile(from: *const c_char, to: *const c_char, flags: u32) -> c_int;
        pub(crate) fn fclonefileat(from: c_int, to_dir: c_int, to: *const c_char, flags: u32) -> c_int;
        pub(crate) fn fcopyfile(from: c_int, to: c_int, state: *mut c_void, flags: u32) -> c_int;
        pub(crate) fn fcntl(fd: c_int, command: c_int, ...) -> c_int;
        pub(crate) fn renameatx_np(from_dir: c_int, from: *const c_char, to_dir: c_int, to: *const c_char, flags: c_uint) -> c_int;
        pub(crate) fn openat(dir: c_int, path: *const c_char, flags: c_int, ...) -> c_int;
        pub(crate) fn mach_absolute_time() -> u64;
        pub(crate) fn mach_continuous_time() -> u64;
        pub(crate) fn getentropy(into: *mut c_void, len: usize) -> c_int;
        pub(crate) fn __error() -> *mut c_int;
        pub(crate) fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    }
}

crate::unbuilt! {
    "macOS":
    open_beneath, openat, close, read, pread, write, pwrite, sync_barrier, sync_full, ftruncate,
    rename, unlink, mkdir, stat, list, clone_file, copy_file, map, unmap, advise, protect,
    park, unpark, poll, kevent, workloop, child_exit, clock, sleep_until, spawn, exit, random,
    socket, connect, bind, listen, accept, send, receive, shutdown, pipe, footprint_limit,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `RTLD_DEFAULT` on macOS: every image loaded into the process.
    const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

    /// Every symbol this layer declares is one libSystem exports on this macOS, the private
    /// ones too.
    #[test]
    fn every_declared_symbol_exists() {
        for symbol in SYMBOLS {
            // SAFETY: `dlsym` reads a live NUL-terminated name and returns an address or null.
            let found = unsafe { ffi::dlsym(RTLD_DEFAULT, symbol.name.as_ptr()) };
            assert!(
                !found.is_null(),
                "libSystem has no `{}` ({:?}) on this macOS; blaze uses it for: {}",
                symbol.name.to_string_lossy(),
                symbol.api,
                symbol.why
            );
        }
    }

    /// The control of the test above: a name libSystem does not export is not found, so a
    /// found symbol means something.
    #[test]
    fn a_missing_symbol_is_not_found() {
        // SAFETY: as above.
        let found = unsafe { ffi::dlsym(RTLD_DEFAULT, c"__blaze_sys_no_such_symbol".as_ptr()) };
        assert!(found.is_null());
    }

    /// Every private symbol says why it and not the public call.
    #[test]
    fn every_private_symbol_says_why() {
        for symbol in SYMBOLS.iter().filter(|it| it.api == Api::Private) {
            assert!(symbol.why.len() > 40, "{:?} gives no reason", symbol.name);
        }
    }

    #[test]
    fn an_unbuilt_operation_names_itself() {
        let refusal = park();
        assert_eq!((refusal.operation, refusal.target), ("park", "macOS"));
        assert!(UNBUILT.contains(&"park") && UNBUILT.len() >= 40);
        assert!(refusal.to_string().contains("`park` on macOS"));
    }

    #[test]
    fn the_errno_table_is_libcs_on_macos() {
        use crate::Errno;
        let libcs: &[(Errno, c_int)] = &[
            (Errno::Again, libc::EAGAIN),
            (Errno::Deadlk, libc::EDEADLK),
            (Errno::NameTooLong, libc::ENAMETOOLONG),
            (Errno::NoLck, libc::ENOLCK),
            (Errno::NoSys, libc::ENOSYS),
            (Errno::NotEmpty, libc::ENOTEMPTY),
            (Errno::Loop, libc::ELOOP),
            (Errno::Overflow, libc::EOVERFLOW),
            (Errno::NotSock, libc::ENOTSOCK),
            (Errno::MsgSize, libc::EMSGSIZE),
            (Errno::ProtoNoSupport, libc::EPROTONOSUPPORT),
            (Errno::OpNotSupp, libc::EOPNOTSUPP),
            (Errno::AfNoSupport, libc::EAFNOSUPPORT),
            (Errno::AddrInUse, libc::EADDRINUSE),
            (Errno::AddrNotAvail, libc::EADDRNOTAVAIL),
            (Errno::NetDown, libc::ENETDOWN),
            (Errno::NetUnreach, libc::ENETUNREACH),
            (Errno::ConnAborted, libc::ECONNABORTED),
            (Errno::ConnReset, libc::ECONNRESET),
            (Errno::NoBufs, libc::ENOBUFS),
            (Errno::IsConn, libc::EISCONN),
            (Errno::NotConn, libc::ENOTCONN),
            (Errno::TimedOut, libc::ETIMEDOUT),
            (Errno::ConnRefused, libc::ECONNREFUSED),
            (Errno::HostUnreach, libc::EHOSTUNREACH),
            (Errno::Already, libc::EALREADY),
            (Errno::InProgress, libc::EINPROGRESS),
            (Errno::Stale, libc::ESTALE),
            (Errno::DQuot, libc::EDQUOT),
            (Errno::Canceled, libc::ECANCELED),
            (Errno::Perm, libc::EPERM),
            (Errno::NoEnt, libc::ENOENT),
            (Errno::Intr, libc::EINTR),
            (Errno::BadF, libc::EBADF),
            (Errno::Exist, libc::EEXIST),
            (Errno::XDev, libc::EXDEV),
            (Errno::Inval, libc::EINVAL),
            (Errno::NoSpc, libc::ENOSPC),
            (Errno::Pipe, libc::EPIPE),
        ];
        for &(errno, libcs) in libcs {
            assert_eq!(c_int::from(errno.raw()), libcs, "{}", errno.name());
            assert_eq!(Errno::from_raw(errno.raw()), errno);
        }
    }
}
