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
    public(c"lseek", "`Effect::CopyData` and `Effect::Prewarm`: `SEEK_DATA` (4) and `SEEK_HOLE` (3) find the data extents"),
    public(c"pread", "`Effect::CopyData`: one extent read into the driver's one bounded buffer"),
    public(c"pwrite", "`Effect::CopyData`: that buffer written at the same offset of the new file"),
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

/// The private symbols of the [`cold`] table, each with why it and not a public call (rule
/// macos-private-apis). They are not in [`SYMBOLS`]: `every_declared_symbol_exists` looks up
/// with `RTLD_DEFAULT`, which finds the cold table's own definition first; `every_cold_symbol_exists`
/// looks each up past it (`RTLD_NEXT`), and `every_private_symbol_says_why` reads this list too.
pub const COLD_PRIVATE: &[Symbol] = &[private(
    c"thread_selfcounts",
    "the meter (blaze-meter, D169): this thread's instructions, cycles and user and system time \
     in one call (`THSC_TIME_CPI`, 3), read without a lock at every stretch's two ends; the \
     public `thread_info(THREAD_BASIC_INFO)` gives the times alone, microsecond-grained and \
     through a Mach port, and no public call gives a thread's instructions",
)];

/// The declarations. Nothing calls them in slice 1 but the symbol test's `dlsym`.
#[allow(dead_code, reason = "declared for the slices that build on them; each is looked up by the symbol test")]
#[expect(blaze::unwired, reason = "unwired until the slices that build on these declarations call them")]
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

/// libSystem's calls a program makes only on some paths, found at their first call and never
/// bound at load (docs/v2/plan/design/std.md, "What a program pays for at start").
///
/// dyld binds every imported symbol before `main`, each a search of libSystem's re-exported
/// libraries: measured 2026-11-16 on this Mac, 71 imports cost 4.5 M instructions (about 64 k
/// each) and 0.18 ms per start, though the program called none of them; finding the same 71 by
/// `dlsym` cost 2.6 M. So each call of this table is defined here, under its C name
/// (`#[unsafe(no_mangle)]`): the static linker resolves every reference in the program to it
/// (blaze's own `libc::` calls and Rust std's alike: std's file, thread and socket calls are
/// most of the table), so the executable imports none of them. Its first call finds libSystem's
/// symbol (`dlsym(RTLD_NEXT, name)`: the images after this one, so never this definition) and
/// keeps its address; later calls load that and call it. A symbol this macOS lacks stops the
/// program naming it (rule 3), and `every_cold_symbol_exists` looks each up, so that is found
/// on the first macOS that lacks one, by a test. Linking a sub-library of libSystem directly to
/// skip the search is not done: those are private install names.
///
/// What it rests on, each stated where it matters: (1) a definition in the executable wins over
/// a dylib's for the executable's own references, and only those: libSystem's calls between its
/// own libraries are two-level and never reach these. (2) A first call runs `dlsym`, which is
/// not async-signal-safe: no entry may be first called in a signal handler or between a `fork`
/// and an `exec`; blaze spawns with `posix_spawn` and its handlers call none of them. (3) The
/// signature of each entry is its header's (named per entry), with a struct passed as
/// `*mut c_void`.
///
/// Every entry is the arm64 symbol. On x86-64 macOS, not a host blaze supports, the stat calls
/// are `$INODE64` symbols the program would reference instead, so those entries go unused.
pub mod cold {
    use core::{
        ffi::{CStr, c_char, c_int, c_uint, c_ulong, c_void},
        ptr,
        sync::atomic::{AtomicPtr, Ordering},
    };

    /// `RTLD_NEXT` (dlfcn.h): the images loaded after the caller's: libSystem's definition of a
    /// name, never this file's.
    const RTLD_NEXT: *mut c_void = -1isize as *mut c_void;

    /// The address of `name`, found on the first call: `slot` keeps it. `Relaxed` suffices:
    /// the address is of code dyld mapped before `main`, and two first calls that race each
    /// find and store the same one.
    #[inline]
    fn resolve(slot: &AtomicPtr<c_void>, name: &'static CStr) -> *mut c_void {
        let found = slot.load(Ordering::Relaxed);
        if !found.is_null() {
            return found;
        }
        resolve_cold(slot, name)
    }

    #[cold]
    #[inline(never)]
    fn resolve_cold(slot: &AtomicPtr<c_void>, name: &'static CStr) -> *mut c_void {
        // SAFETY: `dlsym` reads a live NUL-terminated name and returns an address or null.
        let found = unsafe { super::ffi::dlsym(RTLD_NEXT, name.as_ptr()) };
        if found.is_null() {
            missing(name);
        }
        slot.store(found, Ordering::Relaxed);
        found
    }

    /// The program asked for a call this macOS does not export: it stops, naming it.
    #[cold]
    fn missing(name: &'static CStr) -> ! {
        panic!(
            "blaze_sys::macos::cold: libSystem on this macOS has no {name:?}, so the call cannot be made (rule 3: no other is \
             put in its place)"
        )
    }

    const fn name(text: &'static str) -> &'static CStr {
        match CStr::from_bytes_with_nul(text.as_bytes()) {
            Ok(name) => name,
            Err(_) => panic!("a cold symbol's name holds a NUL"),
        }
    }

    macro_rules! cold {
        ($(
            $(#[doc = $doc:literal])*
            fn $name:ident($($arg:ident: $ty:ty),*) $(-> $ret:ty)?;
        )*) => {
            $(
                $(#[doc = $doc])*
                ///
                /// # Safety
                ///
                /// The C call's own contract, as its header states it.
                #[unsafe(no_mangle)]
                pub unsafe extern "C" fn $name($($arg: $ty),*) $(-> $ret)? {
                    static SLOT: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
                    let found = resolve(&SLOT, name(concat!(stringify!($name), "\0")));
                    // SAFETY: `found` is the address of libSystem's symbol of this name, whose C
                    // type is the one declared here (each entry's header).
                    let call: unsafe extern "C" fn($($ty),*) $(-> $ret)? = unsafe { core::mem::transmute(found) };
                    // SAFETY: the caller's contract.
                    unsafe { call($($arg),*) }
                }
            )*
            /// Every symbol of the table, for `every_cold_symbol_exists`.
            pub const SYMBOLS: &[&CStr] = &[$(name(concat!(stringify!($name), "\0"))),* , c"openat", c"ioctl"];
        };
    }

    /// `openat` (fcntl.h), variadic: its mode is read, and passed on, only when `flags` create
    /// a file (`O_CREAT`), as the header's contract says the caller passes it then.
    ///
    /// # Safety
    ///
    /// The C call's own contract.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn openat(dir: c_int, path: *const c_char, flags: c_int, mut rest: ...) -> c_int {
        /// `O_CREAT` (fcntl.h).
        const O_CREAT: c_int = 0x200;
        static SLOT: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
        let found = resolve(&SLOT, c"openat");
        // SAFETY: libSystem's `openat`, called with its fixed arguments and, for a create, the
        // mode the caller passed (promoted to `c_uint` as C passes a `mode_t` through `...`).
        unsafe {
            let call: unsafe extern "C" fn(c_int, *const c_char, c_int, ...) -> c_int = core::mem::transmute(found);
            match flags & O_CREAT {
                0 => call(dir, path, flags),
                _ => call(dir, path, flags, rest.next_arg::<c_uint>()),
            }
        }
    }

    /// `ioctl` (sys/ioctl.h), variadic: a request whose direction bits say `IOC_VOID` takes no
    /// argument (`FIOCLEX`), every other one argument, a pointer (sys/ioccom.h's encoding), which
    /// is read and passed on.
    ///
    /// # Safety
    ///
    /// The C call's own contract.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn ioctl(fd: c_int, request: c_ulong, mut rest: ...) -> c_int {
        /// `IOC_DIRMASK` and `IOC_VOID` (sys/ioccom.h).
        const IOC_DIRMASK: c_ulong = 0xe000_0000;
        const IOC_VOID: c_ulong = 0x2000_0000;
        static SLOT: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
        let found = resolve(&SLOT, c"ioctl");
        // SAFETY: libSystem's `ioctl` with the arguments its request's encoding says it takes.
        unsafe {
            let call: unsafe extern "C" fn(c_int, c_ulong, ...) -> c_int = core::mem::transmute(found);
            match request & IOC_DIRMASK {
                IOC_VOID => call(fd, request),
                _ => call(fd, request, rest.next_arg::<*mut c_void>()),
            }
        }
    }

    cold! {
        /// crt_externs.h.
        fn _NSGetArgc() -> *mut c_int;
        /// crt_externs.h.
        fn _NSGetArgv() -> *mut *mut *mut c_char;
        /// crt_externs.h.
        fn _NSGetEnviron() -> *mut *mut *mut c_char;
        /// sys/socket.h; `addr` is a `struct sockaddr`.
        fn accept(fd: c_int, addr: *mut c_void, len: *mut u32) -> c_int;
        /// sys/stat.h.
        fn chmod(path: *const c_char, mode: u16) -> c_int;
        /// time.h; `clockid_t` is a `c_uint`, `at` a `struct timespec`.
        fn clock_gettime(clock: c_uint, at: *mut c_void) -> c_int;
        /// unistd.h.
        fn confstr(name: c_int, into: *mut c_char, len: usize) -> usize;
        /// dispatch/time.h; `dispatch_time_t` is a `u64`.
        fn dispatch_time(when: u64, delta: i64) -> u64;
        /// stdlib.h. Called once, at the end; its first call is that one.
        fn exit(status: c_int) -> !;
        /// sys/stat.h.
        fn fchmod(fd: c_int, mode: u16) -> c_int;
        /// dirent.h; returns a `DIR`.
        fn fdopendir(fd: c_int) -> *mut c_void;
        /// sys/file.h.
        fn flock(fd: c_int, operation: c_int) -> c_int;
        /// unistd.h.
        fn ftruncate(fd: c_int, len: i64) -> c_int;
        /// unistd.h.
        fn getpid() -> i32;
        /// unistd.h: whether a descriptor is a terminal.
        fn isatty(fd: c_int) -> c_int;
        /// sys/resource.h; `limit` is a `struct rlimit`.
        fn getrlimit(resource: c_int, limit: *mut c_void) -> c_int;
        /// sys/resource.h; `usage` is a `struct rusage`. The meter's process totals
        /// ([`super::process_usage`]).
        fn getrusage(who: c_int, usage: *mut c_void) -> c_int;
        /// Private (Kernel.framework's sys/resource_private.h; no SDK header declares it):
        /// `kind` a `thread_selfcounts_kind_t`, `into` the struct of that kind
        /// ([`super::COLD_PRIVATE`] says why).
        fn thread_selfcounts(kind: c_int, into: *mut c_void, len: usize) -> c_int;
        /// sys/event.h; `changes` and `events` are `struct kevent`s, `timeout` a `timespec`.
        fn kevent(kq: c_int, changes: *const c_void, nchanges: c_int, events: *mut c_void, nevents: c_int, timeout: *const c_void) -> c_int;
        /// sys/event.h.
        fn kqueue() -> c_int;
        /// unistd.h.
        fn linkat(from_dir: c_int, from: *const c_char, to_dir: c_int, to: *const c_char, flags: c_int) -> c_int;
        /// sys/stat.h.
        fn lstat(path: *const c_char, stat: *mut c_void) -> c_int;
        /// mach/mach_time.h.
        fn mach_absolute_time() -> u64;
        /// mach/mach_time.h; `info` is a `mach_timebase_info_data_t`.
        fn mach_timebase_info(info: *mut c_void) -> c_int;
        /// sys/stat.h.
        fn mkdir(path: *const c_char, mode: u16) -> c_int;
        /// unistd.h.
        fn pread(fd: c_int, into: *mut c_void, len: usize, at: i64) -> isize;
        /// libproc.h; `buffer` is an array of `pid_t` (`i32`), `buffersize` in bytes; returns
        /// the count of PIDs written or -1 on error.
        fn proc_listpgrppids(pgrpid: i32, buffer: *mut c_void, buffersize: c_int) -> c_int;
        /// libproc.h; `info` is a `rusage_info_*`.
        fn proc_pid_rusage(pid: c_int, flavor: c_int, info: *mut c_void) -> c_int;
        /// pthread.h; `attr` is a `pthread_attr_t`.
        fn pthread_attr_destroy(attr: *mut c_void) -> c_int;
        /// pthread.h.
        fn pthread_attr_init(attr: *mut c_void) -> c_int;
        /// pthread.h.
        fn pthread_attr_setstacksize(attr: *mut c_void, size: usize) -> c_int;
        /// pthread.h; `thread` is a `pthread_t`, an address.
        fn pthread_create(thread: *mut *mut c_void, attr: *const c_void, start: extern "C" fn(*mut c_void) -> *mut c_void, arg: *mut c_void) -> c_int;
        /// pthread.h.
        fn pthread_detach(thread: *mut c_void) -> c_int;
        /// pthread.h.
        fn pthread_join(thread: *mut c_void, value: *mut *mut c_void) -> c_int;
        /// unistd.h.
        fn pwrite(fd: c_int, from: *const c_void, len: usize, at: i64) -> isize;
        /// unistd.h.
        fn readlink(path: *const c_char, into: *mut c_char, len: usize) -> isize;
        /// stdio.h.
        fn rename(from: *const c_char, to: *const c_char) -> c_int;
        /// unistd.h.
        fn rmdir(path: *const c_char) -> c_int;
        /// sched.h.
        fn sched_yield() -> c_int;
        /// sys/socket.h.
        fn send(fd: c_int, from: *const c_void, len: usize, flags: c_int) -> isize;
        /// sys/stat.h.
        fn stat(path: *const c_char, stat: *mut c_void) -> c_int;
        /// unistd.h.
        fn unlink(path: *const c_char) -> c_int;
        /// unistd.h.
        fn unlinkat(dir: c_int, path: *const c_char, flags: c_int) -> c_int;
        /// dispatch/queue.h.
        fn dispatch_queue_create(label: *const c_char, attributes: *const c_void) -> *mut c_void;
        /// dispatch/queue.h.
        fn dispatch_sync_f(queue: *mut c_void, context: *mut c_void, work: extern "C" fn(*mut c_void));
        /// dispatch/object.h.
        fn dispatch_release(object: *mut c_void);
        /// sys/stat.h; `mode_t` is `u16`.
        fn fchmodat(dir: c_int, path: *const c_char, mode: u16, flags: c_int) -> c_int;
        /// sys/clonefile.h.
        fn fclonefileat(from: c_int, to_dir: c_int, to: *const c_char, flags: u32) -> c_int;
        /// sys/stat.h; `stat` is `struct stat`.
        fn fstatat(dir: c_int, path: *const c_char, stat: *mut c_void, flags: c_int) -> c_int;
        /// unistd.h.
        fn fsync(fd: c_int) -> c_int;
        /// sys/attr.h; `attrs` is `struct attrlist`.
        fn getattrlistbulk(dir: c_int, attrs: *mut c_void, buffer: *mut c_void, size: usize, options: u64) -> c_int;
        /// unistd.h; `uid_t` is `u32`.
        fn geteuid() -> u32;
        /// sys/resource.h.
        fn getiopolicy_np(iotype: c_int, scope: c_int) -> c_int;
        /// sys/resource.h.
        fn setiopolicy_np(iotype: c_int, scope: c_int, policy: c_int) -> c_int;
        /// sys/socket.h; `socklen_t` is `u32`.
        fn getsockopt(fd: c_int, level: c_int, name: c_int, value: *mut c_void, len: *mut u32) -> c_int;
        /// sys/socket.h.
        fn setsockopt(fd: c_int, level: c_int, name: c_int, value: *const c_void, len: u32) -> c_int;
        /// signal.h; `pid_t` is `i32`.
        fn kill(pid: i32, signal: c_int) -> c_int;
        /// unistd.h; `off_t` is `i64`.
        fn lseek(fd: c_int, at: i64, whence: c_int) -> i64;
        /// sys/mman.h.
        fn madvise(at: *mut c_void, len: usize, advice: c_int) -> c_int;
        /// sys/mman.h.
        fn mincore(at: *const c_void, len: usize, held: *mut c_char) -> c_int;
        /// os/os_sync_wait_on_address.h (macOS 14.4).
        fn os_sync_wait_on_address(addr: *mut c_void, value: u64, size: usize, flags: u32) -> c_int;
        /// os/os_sync_wait_on_address.h; `clockid` is `os_clockid_t`, a `u32`.
        fn os_sync_wait_on_address_with_deadline(addr: *mut c_void, value: u64, size: usize, flags: u32, clockid: u32, deadline: u64) -> c_int;
        /// os/os_sync_wait_on_address.h.
        fn os_sync_wake_by_address_all(addr: *mut c_void, size: usize, flags: u32) -> c_int;
        /// os/os_sync_wait_on_address.h.
        fn os_sync_wake_by_address_any(addr: *mut c_void, size: usize, flags: u32) -> c_int;
        /// sys/uio.h; `iov` is `struct iovec`.
        fn preadv(fd: c_int, iov: *const c_void, count: c_int, at: i64) -> isize;
        /// unistd.h.
        fn read(fd: c_int, into: *mut c_void, len: usize) -> isize;
        /// sys/socket.h.
        fn recv(fd: c_int, into: *mut c_void, len: usize, flags: c_int) -> isize;
        /// sys/mount.h; `stat` is `struct statfs`.
        fn statfs(path: *const c_char, stat: *mut c_void) -> c_int;
        /// sys/sysctl.h.
        fn sysctlbyname(name: *const c_char, old: *mut c_void, old_len: *mut usize, new: *mut c_void, new_len: usize) -> c_int;
        /// sys/stat.h; `times` is `struct timespec[2]`.
        fn utimensat(dir: c_int, path: *const c_char, times: *const c_void, flags: c_int) -> c_int;
        /// sys/wait.h; `usage` is `struct rusage`.
        fn wait4(pid: i32, status: *mut c_int, options: c_int, usage: *mut c_void) -> i32;
        /// sys/wait.h; `idtype_t` is a `c_uint`, `id_t` a `u32`, `info` a `siginfo_t`.
        fn waitid(idtype: c_uint, id: u32, info: *mut c_void, options: c_int) -> c_int;
        /// pthread/qos.h; `qos_class_t` is a `c_uint`.
        fn pthread_set_qos_class_self_np(class: c_uint, relative: c_int) -> c_int;
    }
}

/// Whether `fd` is a terminal: `isatty`, through the table of calls found at first use. Used by
/// `bstd::out::Out::is_terminal`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn is_terminal(fd: crate::Fd) -> bool {
    // SAFETY: `isatty` reads only the descriptor's number.
    unsafe { cold::isatty(fd.raw()) == 1 }
}

/// `ioctl(TIOCGWINSZ)`: the terminal `fd`'s size in cells; [`Errno::NoTty`] when `fd` is not a
/// terminal. Used by `bstd::out::Out::size`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn window_size(fd: crate::Fd) -> Result<crate::WindowSize, crate::Errno> {
    /// `TIOCGWINSZ` (sys/ttycom.h): `_IOR('t', 104, struct winsize)`.
    const TIOCGWINSZ: core::ffi::c_ulong = 0x4008_7468;
    let mut size = crate::types::Winsize::default();
    // SAFETY: TIOCGWINSZ writes one `struct winsize` into the one given; `ioctl` passes the
    // pointer on (its request is not `IOC_VOID`).
    let said = unsafe { cold::ioctl(fd.raw(), TIOCGWINSZ, &raw mut size) };
    if said != 0 {
        // SAFETY: `__error` returns the calling thread's `errno`, valid while it runs.
        let code = unsafe { *ffi::__error() };
        return Err(crate::Errno::from_raw(code as u16));
    }
    Ok(crate::WindowSize { cols: size.cols, rows: size.rows })
}

/// `struct thsc_time_cpi` (sys/resource_private.h): what [`thread_counts`] reads. The kernel
/// may leave the instructions and cycles 0 "on certain hardware" (the header's words), and
/// the system time 0 where it is counted as user time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct ThreadCounts {
    pub instructions: u64,
    pub cycles: u64,
    /// In Mach absolute time units ([`timebase`]).
    pub user_mach: u64,
    pub system_mach: u64,
}

/// `thread_selfcounts(THSC_TIME_CPI)`: the calling thread's instructions, cycles and user and
/// system time since it began, one system call (186), no lock. A private call
/// ([`COLD_PRIVATE`]); `ENOTSUP` where the hardware has no counters. Used by blaze-meter's
/// `Meter::now`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn thread_counts() -> Result<ThreadCounts, crate::Errno> {
    /// `THSC_TIME_CPI`.
    const TIME_CPI: c_int = 3;
    let mut counts = ThreadCounts::default();
    // SAFETY: `THSC_TIME_CPI` writes one `struct thsc_time_cpi` (four `uint64_t`s, the layout
    // above) into the buffer of the size given and keeps nothing.
    let said = unsafe { cold::thread_selfcounts(TIME_CPI, (&raw mut counts).cast(), size_of::<ThreadCounts>()) };
    if said != 0 {
        // SAFETY: `__error` returns the calling thread's `errno`, valid while it runs.
        let code = unsafe { *ffi::__error() };
        return Err(crate::Errno::from_raw(code as u16));
    }
    Ok(counts)
}

/// `mach_timebase_info`: Mach absolute time is `numer / denom` nanoseconds a unit (125 / 3 on
/// Apple silicon, 1 / 1 on Intel). Read once by a caller and kept: it never changes.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn timebase() -> (u32, u32) {
    let mut info = [0u32; 2];
    // SAFETY: `mach_timebase_info` writes one `mach_timebase_info_data_t` (two `uint32_t`s).
    unsafe { cold::mach_timebase_info(info.as_mut_ptr().cast()) };
    (info[0], info[1])
}

/// `struct timeval` on 64-bit macOS: a `time_t` and an `int` of microseconds, padded to 16.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Timeval {
    pub sec: i64,
    pub usec: i32,
    pad: i32,
}

/// `struct rusage` on 64-bit macOS: two [`Timeval`]s and fourteen `long`s (`ru_maxrss` in
/// bytes, not Linux's kilobytes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Rusage {
    pub user: Timeval,
    pub system: Timeval,
    pub max_rss_bytes: i64,
    pub ixrss: i64,
    pub idrss: i64,
    pub isrss: i64,
    /// Page reclaims: faults served without I/O.
    pub minflt: i64,
    /// Page faults that read.
    pub majflt: i64,
    pub rest: [i64; 8],
}

/// `struct rusage_info_v4` (sys/resource.h): its UUID, then 35 `uint64_t`s; the meter reads
/// the ones named by the constants below.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct RusageInfoV4 {
    pub uuid: [u8; 16],
    pub words: [u64; 35],
}

impl RusageInfoV4 {
    /// `ri_user_time` and `ri_system_time`, in Mach absolute time units.
    pub const USER: usize = 0;
    pub const SYSTEM: usize = 1;
    /// `ri_pageins`.
    pub const PAGEINS: usize = 4;
    /// `ri_phys_footprint`, bytes.
    pub const FOOTPRINT: usize = 7;
    /// `ri_child_user_time` and `ri_child_system_time`, in Mach absolute time units (reaped
    /// children of this process).
    pub const CHILD_USER: usize = 10;
    pub const CHILD_SYSTEM: usize = 11;
    /// `ri_lifetime_max_phys_footprint`, bytes.
    pub const MOST_FOOTPRINT: usize = 28;
    /// `ri_instructions` and `ri_cycles`: the whole process's, every thread that ran.
    pub const INSTRUCTIONS: usize = 29;
    pub const CYCLES: usize = 30;

    /// This process's own user + system CPU time, in Mach absolute time units.
    #[inline]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn self_cpu_ticks(&self) -> u64 {
        let [user, system, ..] = self.words;
        user.saturating_add(system)
    }

    /// Reaped children's user + system CPU time, in Mach absolute time units.
    #[inline]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn child_cpu_ticks(&self) -> u64 {
        let [_, _, _, _, _, _, _, _, _, _, child_user, child_system, ..] = self.words;
        child_user.saturating_add(child_system)
    }

    /// Combined user + system CPU time (self + reaped children), in Mach absolute time units.
    #[inline]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn total_cpu_ticks(&self) -> u64 {
        self.self_cpu_ticks().saturating_add(self.child_cpu_ticks())
    }

    /// Physical footprint (`ri_phys_footprint`), in bytes.
    #[inline]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn footprint(&self) -> u64 {
        let [_, _, _, _, _, _, _, footprint, ..] = self.words;
        footprint
    }
}

/// `getrusage(RUSAGE_SELF)` and `proc_pid_rusage(getpid(), RUSAGE_INFO_V4)`: what the whole
/// process used since it began, every thread's, the meter's totals to reconcile its stretches
/// against (blaze-meter `Process::now`).
#[blaze::noalloc]
#[blaze::nopanic]
pub fn process_usage() -> Result<(Rusage, RusageInfoV4), crate::Errno> {
    /// `RUSAGE_SELF` and `RUSAGE_INFO_V4`.
    const SELF: c_int = 0;
    const INFO_V4: c_int = 4;
    let mut usage = Rusage::default();
    let mut info = RusageInfoV4 { uuid: [0; 16], words: [0; 35] };
    // SAFETY: `getrusage` writes one `struct rusage` (the layout above) into the one given.
    let said = unsafe { cold::getrusage(SELF, (&raw mut usage).cast()) };
    if said != 0 {
        // SAFETY: as in `thread_counts`.
        return Err(crate::Errno::from_raw(unsafe { *ffi::__error() } as u16));
    }
    // SAFETY: `RUSAGE_INFO_V4` writes one `struct rusage_info_v4` (the layout above) for the
    // pid given, this process's own.
    let said = unsafe { cold::proc_pid_rusage(cold::getpid(), INFO_V4, (&raw mut info).cast()) };
    if said != 0 {
        // SAFETY: as above.
        return Err(crate::Errno::from_raw(unsafe { *ffi::__error() } as u16));
    }
    Ok((usage, info))
}

/// `sched_yield`: the CPU given to another runnable thread, if one waits; back at once
/// otherwise. A spin's back-off before it parks (blaze-core's locks, blaze-spin).
#[blaze::noalloc]
#[blaze::nopanic]
pub fn sched_yield() -> Result<(), crate::Errno> {
    // SAFETY: `int sched_yield(void)`: no arguments, no memory.
    let said = unsafe { cold::sched_yield() };
    if said != 0 {
        // SAFETY: as in `thread_counts`.
        return Err(crate::Errno::from_raw(unsafe { *ffi::__error() } as u16));
    }
    Ok(())
}

/// `sysctlbyname` of a 32-bit integer: its value, or `None` where this macOS has no such name
/// (`ENOENT`: the `hw.optional.arm.FEAT_*` a chip predates are absent, not 0). Any other
/// refusal, and a value of another size, are errors: [`crate::Errno::Inval`] for the size.
/// Used by [`crate::cpu::CpuFeatures::detect`].
#[blaze::noalloc]
#[blaze::nopanic]
pub fn sysctl_u32(name: &CStr) -> Result<Option<u32>, crate::Errno> {
    let mut value = 0u32;
    let mut len = size_of::<u32>();
    // SAFETY: `name` is a live NUL-terminated string; `value` is live memory of `len` bytes the
    // call writes at most `len` of, and `len` is written back; nothing new is set (null, 0).
    let said = unsafe {
        cold::sysctlbyname(name.as_ptr(), (&raw mut value).cast(), &raw mut len, core::ptr::null_mut(), 0)
    };
    if said != 0 {
        // SAFETY: as in `thread_counts`.
        let errno = crate::Errno::from_raw(unsafe { *ffi::__error() } as u16);
        return if errno == crate::Errno::NoEnt { Ok(None) } else { Err(errno) };
    }
    if len != size_of::<u32>() {
        return Err(crate::Errno::Inval);
    }
    Ok(Some(value))
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

    /// `RTLD_NEXT`: the images after this one, where libSystem's definitions are.
    const RTLD_NEXT: *mut c_void = -1isize as *mut c_void;

    /// Every entry of the cold table is a symbol this macOS's libSystem exports.
    #[test]
    fn every_cold_symbol_exists() {
        for name in cold::SYMBOLS {
            // SAFETY: as above.
            let found = unsafe { ffi::dlsym(RTLD_NEXT, name.as_ptr()) };
            assert!(!found.is_null(), "libSystem has no `{}` on this macOS", name.to_string_lossy());
        }
        assert!(cold::SYMBOLS.len() >= 70);
    }

    /// The program's references reach the table, not libSystem: the first definition of
    /// `geteuid` in the process is this file's, it is not libSystem's, and a call through it (and
    /// through `libc::`, which links to it) answers as libSystem's own does.
    #[test]
    fn the_programs_calls_reach_the_table() {
        let ours = cold::geteuid as unsafe extern "C" fn() -> u32 as *mut c_void;
        // SAFETY: as above.
        let first = unsafe { ffi::dlsym(RTLD_DEFAULT, c"geteuid".as_ptr()) };
        // SAFETY: as above.
        let system = unsafe { ffi::dlsym(RTLD_NEXT, c"geteuid".as_ptr()) };
        assert_eq!(first, ours, "the executable's `geteuid` is the table's");
        assert_ne!(system, ours);
        // SAFETY: libSystem's `geteuid`, `uid_t geteuid(void)`.
        let system: unsafe extern "C" fn() -> u32 = unsafe { core::mem::transmute(system) };
        // SAFETY: no arguments; `kill` with signal 0 only checks the process can be signalled.
        unsafe {
            assert_eq!(cold::geteuid(), system());
            assert_eq!(libc::geteuid(), system());
            assert_eq!(cold::kill(libc::getpid(), 0), 0);
        }
    }

    /// A pipe is not a terminal, and has no window size: `ENOTTY`, named.
    #[test]
    fn a_pipe_is_not_a_terminal() {
        let mut ends = [0; 2];
        // SAFETY: `pipe` writes two descriptors into the array given.
        assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0);
        for end in ends {
            assert!(!is_terminal(crate::Fd::new(end)));
            assert_eq!(window_size(crate::Fd::new(end)), Err(crate::Errno::NoTty));
            // SAFETY: an end of the pipe made above, closed once.
            unsafe { libc::close(end) };
        }
    }

    /// The variadic entries pass their optional argument exactly when it is there: `openat`
    /// creates with the mode given, and `ioctl`'s `FIOCLEX` (no argument) and `FIONBIO` (one)
    /// both take effect.
    #[test]
    fn the_variadic_entries_pass_what_was_given() {
        let dir = std::env::temp_dir().join(format!("blaze-sys-cold-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = std::ffi::CString::new(dir.join("made").into_os_string().into_encoded_bytes()).unwrap();
        // SAFETY: a live NUL-terminated path; the descriptor is closed below.
        let fd = unsafe { cold::openat(libc::AT_FDCWD, path.as_ptr(), libc::O_CREAT | libc::O_WRONLY | libc::O_EXCL, 0o640 as c_uint) };
        assert!(fd >= 0, "openat created the file");
        let mode = std::fs::metadata(dir.join("made")).unwrap();
        assert_eq!(std::os::unix::fs::PermissionsExt::mode(&mode.permissions()) & 0o777, 0o640);
        // SAFETY: `fd` is open; FIOCLEX takes no argument, FIONBIO an `int` to read.
        unsafe {
            assert_eq!(cold::ioctl(fd, libc::FIOCLEX), 0);
            assert_eq!(libc::fcntl(fd, libc::F_GETFD) & libc::FD_CLOEXEC, libc::FD_CLOEXEC);
            let mut on: c_int = 1;
            assert_eq!(cold::ioctl(fd, libc::FIONBIO, &raw mut on), 0);
            assert_ne!(libc::fcntl(fd, libc::F_GETFL) & libc::O_NONBLOCK, 0);
            libc::close(fd);
        }
        std::fs::remove_dir_all(&dir).unwrap();
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
        for symbol in SYMBOLS.iter().chain(COLD_PRIVATE).filter(|it| it.api == Api::Private) {
            assert!(symbol.why.len() > 40, "{:?} gives no reason", symbol.name);
        }
        assert!(COLD_PRIVATE.iter().all(|it| cold::SYMBOLS.contains(&it.name)), "a cold private symbol is in the cold table");
    }

    /// The meter's calls (rule macos-private-apis: the private one's symbol exists, and it
    /// answers): `thread_selfcounts` past this file's definition, and the layouts are libc's.
    #[test]
    fn the_meters_calls_exist_and_answer() {
        // SAFETY: as above.
        let found = unsafe { ffi::dlsym(RTLD_NEXT, c"thread_selfcounts".as_ptr()) };
        assert!(!found.is_null(), "libSystem has no `thread_selfcounts` on this macOS");
        let before = thread_counts().expect("THSC_TIME_CPI answers on this Mac");
        let mut spin = 0u64;
        for at in 0..1_000_000u64 {
            spin = core::hint::black_box(spin.wrapping_add(at));
        }
        let after = thread_counts().expect("THSC_TIME_CPI answers on this Mac");
        assert!(after.instructions > before.instructions + 1_000_000, "a million adds retire a million instructions: {before:?} {after:?}");
        assert!(after.user_mach + after.system_mach > before.user_mach + before.system_mach);
        let (numer, denom) = timebase();
        assert!(numer > 0 && denom > 0);
        let (usage, info) = process_usage().expect("the process's own usage");
        assert!(usage.minflt > 0 && info.words[RusageInfoV4::INSTRUCTIONS] >= after.instructions);
        assert_eq!(size_of::<Rusage>(), size_of::<libc::rusage>());
        assert_eq!(core::mem::offset_of!(Rusage, minflt), core::mem::offset_of!(libc::rusage, ru_minflt));
        assert_eq!(size_of::<RusageInfoV4>(), size_of::<libc::rusage_info_v4>());
        let word = |at: usize| 16 + 8 * at;
        assert_eq!(word(RusageInfoV4::INSTRUCTIONS), core::mem::offset_of!(libc::rusage_info_v4, ri_instructions));
        assert_eq!(word(RusageInfoV4::CYCLES), core::mem::offset_of!(libc::rusage_info_v4, ri_cycles));
        assert_eq!(word(RusageInfoV4::FOOTPRINT), core::mem::offset_of!(libc::rusage_info_v4, ri_phys_footprint));
        assert_eq!(word(RusageInfoV4::MOST_FOOTPRINT), core::mem::offset_of!(libc::rusage_info_v4, ri_lifetime_max_phys_footprint));
        assert_eq!(word(RusageInfoV4::PAGEINS), core::mem::offset_of!(libc::rusage_info_v4, ri_pageins));
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

    /// `sched_yield` answers; `sysctl_u32` reads what libc's `sysctlbyname` reads, says `None`
    /// for a name this macOS lacks, and refuses a value wider than 32 bits.
    #[test]
    fn sched_yield_and_sysctl_answer_as_libc() {
        assert_eq!(sched_yield(), Ok(()));
        let mut libcs = 0u32;
        let mut len = size_of::<u32>();
        // SAFETY: as in `sysctl_u32`.
        let said = unsafe {
            libc::sysctlbyname(c"hw.ncpu".as_ptr(), (&raw mut libcs).cast(), &raw mut len, core::ptr::null_mut(), 0)
        };
        assert_eq!(said, 0);
        assert_eq!(sysctl_u32(c"hw.ncpu"), Ok(Some(libcs)));
        assert_eq!(sysctl_u32(c"hw.optional.arm.FEAT_NO_SUCH_FEATURE"), Ok(None));
        // 64 bits into 32: the kernel's `ENOMEM` for the short buffer, or `Inval` for the size.
        assert!(sysctl_u32(c"hw.memsize").is_err(), "hw.memsize is 64 bits");
    }
}
