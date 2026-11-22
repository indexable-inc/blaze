//! A MEASURED EXPERIMENT, not a default: three ways to make the hot calls on macOS, as one seam
//! (rule 78, strategy-is-a-type-parameter). The default stays [`Public`]. A caller chooses a
//! strategy once, up front, from [`crate::macos_facts::MacFacts`] (D185), and never tries one
//! form and then another at call time (rule 3).
//!
//! | strategy | how | what it might save |
//! |---|---|---|
//! | [`Public`] | libSystem's documented functions, exactly as blaze-sys calls them today (`posix::io`, `posix::proc`, the cold table) | nothing: the reference |
//! | [`Kernel`] | libsystem_kernel's own entry points one layer down: the `__*_nocancel` stubs, `__getpid`, `__ulock_wait2`/`__ulock_wake`, `mach_continuous_time`, each found by `dlsym` once into the value | the cold table's load-and-branch, the variadic C wrappers of `open` and `fcntl`, the kernel's pthread-cancellation test, the `errno` write of a wait |
//! | [`Trap`] (arm64 only) | the raw `svc #0x80` with XNU's numbers, for `getpid`, `read`, `write`, `pread`, `close`; [`Kernel`]'s form for the rest | the call into libsystem_kernel and its `cerror` path: `errno` is never written, the error is `x0` |
//!
//! The calls behind the seam are [`Calls`]: getpid, read, pread, write, open, close, fstat,
//! `fcntl(F_GETFL)`, the clock, and wait/wake on a word. The tests below hold the three equal
//! on each (and catch a planted divergence); `examples/cost.rs` prints nanoseconds a call.
//!
//! Notes on what is already true of the tree, read 2026-11-20:
//! - The park in blaze-core (`crates/blaze-core/src/wait/macos.rs`) does NOT use `__ulock_wait2`:
//!   it uses the public `os_sync_wait_on_address_with_deadline` / `os_sync_wake_by_address_any`
//!   (macOS 14.4), through the cold table, as its comment says (the documented face of the
//!   `__ulock` calls, with an absolute deadline). So [`Public`]'s wait is the park's call, and
//!   [`Kernel`]'s is the private `__ulock_wait2` that `macos::ffi` declares and nothing calls.
//! - `proc_pid_rusage` is already used (`macos::process_usage`, through the cold table); it is
//!   not a hot call and is not behind this seam.
//! - Not taken: `kevent_qos` and `kevent_id` (declared in `macos::ffi`). What the IO driver
//!   would gain from them is not a cheaper call but a different driver: a QoS per event,
//!   `KEVENT_FLAG_ERROR_EVENTS` (errors as events, no second pass), and workloops (`kevent_id`)
//!   that wake the worker owning a completion rather than a shared poller. None of that is the
//!   cost of one call this seam measures; it belongs to a kqueue-driver experiment.
//! - Not taken: `__workq_kernreturn`. It is pthread's request for kernel workqueue threads
//!   (what libdispatch runs on). blaze runs its own fixed set of workers and never asks the
//!   kernel for workqueue threads, so there is no call of blaze's it would replace.
//! - Not taken: `__fstat`. libsystem_kernel exports it beside `fstat`/`fstat64` (one address on
//!   arm64e, 27.2.1), but which `struct stat` it fills (the 64-bit-inode one or the old one) is
//!   not stated anywhere this lane could read, and a wrong layout is a memory error. [`Kernel`]
//!   and [`Trap`] use [`Public`]'s `fstat`, which on arm64 is libsystem_kernel's stub itself.
//!
//! These rest on facts read from this Mac (`dyld_info -arch arm64e -exports
//! /usr/lib/system/libsystem_kernel.dylib`, macOS 27.2.1, 2026-11-20): every `__*_nocancel`
//! symbol below is exported; `read` and `__read_nocancel` are different entries (the plain one
//! is the cancellable system call, 3; the other is `read_nocancel`, 396); `getpid` and
//! `__getpid` are one address; `fcntl` and `open` are C wrappers at other addresses than the
//! `__fcntl_nocancel`/`__open_nocancel` stubs. A later macOS may differ: the symbol test and
//! [`crate::macos_facts::MacFacts`] are where that is found.

use core::{
    ffi::{CStr, c_char, c_int, c_long, c_void},
    num::{NonZeroU32, NonZeroU64},
    sync::atomic::AtomicU32,
};

use crate::{
    Errno, Fd, Path,
    macos::{Api, Symbol, cold},
    macos_facts::MacFacts,
    posix::{
        io::{ClockId, OpenFlags, OwnedFd},
        proc::stat,
    },
};

/// A point on Mach's absolute clock (`mach_absolute_time`, which stops in sleep): the deadline
/// a [`Calls::wait`] takes, the clock the park's `os_sync_wait_on_address_with_deadline`
/// counts in (`OS_CLOCK_MACH_ABSOLUTE_TIME`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct MachTime(pub u64);

impl MachTime {
    /// Now on Mach's absolute clock, through the cold table's `mach_absolute_time` (a read of
    /// the commpage: no system call).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn now() -> MachTime {
        // SAFETY: `uint64_t mach_absolute_time(void)`: no arguments, no memory.
        MachTime(unsafe { cold::mach_absolute_time() })
    }

    /// `nanos` after this point, on `timebase`; it stops at `u64::MAX`.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn after(self, nanos: u64, timebase: Timebase) -> MachTime {
        MachTime(self.0.saturating_add(timebase.ticks(nanos)))
    }
}

/// Mach's timebase, a fact of this machine read once ([`MacFacts::timebase`]): a tick is
/// `numer / denom` nanoseconds (125 / 3 on Apple silicon). `denom` is never 0 by its type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Timebase {
    pub numer: NonZeroU32,
    pub denom: NonZeroU32,
}

impl Timebase {
    /// `ticks` of Mach time in nanoseconds, without a 128-bit division: `(t / d) * n + (t % d)
    /// * n / d`, which is exact and stops at `u64::MAX`.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn nanos(self, ticks: u64) -> u64 {
        // Division by a `NonZeroU64` cannot panic.
        let (n, d) = (NonZeroU64::from(self.numer), NonZeroU64::from(self.denom));
        let whole = (ticks / d).saturating_mul(n.get());
        // `ticks % d < d <= u32::MAX` and `n <= u32::MAX`: the product fits a `u64`.
        whole.saturating_add((ticks % d).wrapping_mul(n.get()) / d)
    }

    /// `nanos` in ticks of Mach time, the inverse of [`Timebase::nanos`] (rounded down).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn ticks(self, nanos: u64) -> u64 {
        let (n, d) = (NonZeroU64::from(self.numer), NonZeroU64::from(self.denom));
        let whole = (nanos / n).saturating_mul(d.get());
        whole.saturating_add((nanos % n).wrapping_mul(d.get()) / n)
    }
}

/// The hot calls, one seam (rule 78): every strategy answers each the same way, and the code
/// on either side does not know which runs. Errors are [`Errno`]s.
pub trait Calls {
    /// The strategy's name, as the cost table prints it.
    const NAME: &'static str;
    /// `getpid`: this process's id.
    fn getpid(&self) -> i32;
    /// `read`: up to `into.len()` bytes at the descriptor's position; 0 at the end.
    fn read(&self, fd: Fd, into: &mut [u8]) -> Result<usize, Errno>;
    /// `pread`: up to `into.len()` bytes from offset `at`; 0 at the end.
    fn pread(&self, fd: Fd, into: &mut [u8], at: u64) -> Result<usize, Errno>;
    /// `write`: up to `bytes.len()` bytes at the descriptor's position.
    fn write(&self, fd: Fd, bytes: &[u8]) -> Result<usize, Errno>;
    /// `open`: `path` from the current directory; `mode` for a create.
    fn open(&self, path: Path<'_>, flags: OpenFlags, mode: u32) -> Result<OwnedFd, Errno>;
    /// `close`, reporting its error; the descriptor is gone either way.
    fn close(&self, fd: OwnedFd) -> Result<(), Errno>;
    /// `fstat`: the open file's status.
    fn fstat(&self, fd: Fd) -> Result<stat, Errno>;
    /// `fcntl(F_GETFL)`: the descriptor's status flags and access mode.
    fn status_flags(&self, fd: Fd) -> Result<OpenFlags, Errno>;
    /// The monotonic clock that counts sleep, in nanoseconds since an unspecified start (the
    /// same start for every strategy: Mach's continuous time).
    fn now_ns(&self) -> Result<u64, Errno>;
    /// One wait on `word` while it holds `expected`, until `deadline`: `Ok` when woken or when
    /// the word held another value (it returns at once then; the caller looks again);
    /// [`Errno::TimedOut`] at the deadline; [`Errno::Intr`] for a signal.
    fn wait(&self, word: &AtomicU32, expected: u32, deadline: MachTime) -> Result<(), Errno>;
    /// One wake of a waiter on `word`, of this process: whether one waited.
    fn wake(&self, word: &AtomicU32) -> Result<bool, Errno>;
}

/// A C call's `int` result: the value, or the thread's `errno` where it said -1.
#[blaze::noalloc]
#[blaze::nopanic]
fn int_said(result: c_int) -> Result<c_int, Errno> {
    if result < 0 { Err(crate::posix::signal::errno()) } else { Ok(result) }
}

/// A C call's `ssize_t` result: the count, or the thread's `errno` where it said -1.
#[blaze::noalloc]
#[blaze::nopanic]
fn count_said(result: isize) -> Result<usize, Errno> {
    if result < 0 { Err(crate::posix::signal::errno()) } else { Ok(result as usize) }
}

/// `OS_CLOCK_MACH_ABSOLUTE_TIME` (os/clock.h): the clock of a `with_deadline` wait.
const OS_CLOCK_MACH_ABSOLUTE_TIME: u32 = 32;
/// `OS_SYNC_WAIT_ON_ADDRESS_NONE` / `OS_SYNC_WAKE_BY_ADDRESS_NONE`: a word of this process.
const OS_SYNC_PRIVATE: u32 = 0;
/// The size of the word waited on.
const WORD: usize = size_of::<u32>();

// ---- Public ----

/// libSystem's documented functions, exactly as blaze-sys declares and calls them today: no
/// declaration here, each method is the existing wrapper (`posix::io::read`, the cold table's
/// `getpid`, ...). The reference the other two are measured against, and the default.
#[derive(Clone, Copy, Debug)]
pub struct Public;

impl Calls for Public {
    const NAME: &'static str = "Public";

    /// The cold table's `getpid` (unistd.h), which finds libSystem's at its first call.
    #[blaze::noalloc]
    #[blaze::nopanic]
    fn getpid(&self) -> i32 {
        // SAFETY: `pid_t getpid(void)`: no arguments, no memory.
        unsafe { cold::getpid() }
    }

    /// [`crate::posix::io::read`]: the cold table's `read`.
    #[blaze::noalloc]
    #[blaze::nopanic]
    fn read(&self, fd: Fd, into: &mut [u8]) -> Result<usize, Errno> {
        crate::posix::io::read(fd, into)
    }

    /// [`crate::posix::io::pread`]: the cold table's `pread`.
    #[blaze::noalloc]
    #[blaze::nopanic]
    fn pread(&self, fd: Fd, into: &mut [u8], at: u64) -> Result<usize, Errno> {
        crate::posix::io::pread(fd, into, at)
    }

    /// [`crate::posix::io::write`]: libSystem's `write`, bound at load.
    #[blaze::noalloc]
    #[blaze::nopanic]
    fn write(&self, fd: Fd, bytes: &[u8]) -> Result<usize, Errno> {
        crate::posix::io::write(fd, bytes)
    }

    /// [`crate::posix::io::open`]: the cold table's variadic `openat` from the current
    /// directory.
    #[blaze::noalloc]
    #[blaze::nopanic]
    fn open(&self, path: Path<'_>, flags: OpenFlags, mode: u32) -> Result<OwnedFd, Errno> {
        crate::posix::io::open(path, flags, mode)
    }

    /// [`crate::posix::io::close`]: libSystem's `close`, bound at load.
    #[blaze::noalloc]
    #[blaze::nopanic]
    fn close(&self, fd: OwnedFd) -> Result<(), Errno> {
        crate::posix::io::close(fd)
    }

    /// [`crate::posix::proc::stat_fd`]: libSystem's `fstat`, bound at load.
    #[blaze::noalloc]
    #[blaze::nopanic]
    fn fstat(&self, fd: Fd) -> Result<stat, Errno> {
        crate::posix::proc::stat_fd(fd)
    }

    /// [`crate::posix::io::status_flags`]: libSystem's variadic `fcntl` wrapper.
    #[blaze::noalloc]
    #[blaze::nopanic]
    fn status_flags(&self, fd: Fd) -> Result<OpenFlags, Errno> {
        crate::posix::io::status_flags(fd)
    }

    /// [`crate::posix::io::clock_gettime`] of `CLOCK_MONOTONIC`, which on macOS is Mach's
    /// continuous time in nanoseconds (it counts sleep).
    #[blaze::noalloc]
    #[blaze::nopanic]
    fn now_ns(&self) -> Result<u64, Errno> {
        let now = crate::posix::io::clock_gettime(ClockId::Monotonic)?;
        let (Ok(sec), Ok(nsec)) = (u64::try_from(now.sec), u64::try_from(now.nsec)) else {
            return Err(Errno::Overflow);
        };
        Ok(sec.saturating_mul(1_000_000_000).saturating_add(nsec))
    }

    /// The park's own call: the cold table's `os_sync_wait_on_address_with_deadline`
    /// (os/os_sync_wait_on_address.h, macOS 14.4), the public face of `__ulock_wait2`.
    #[blaze::noalloc]
    #[blaze::nopanic]
    fn wait(&self, word: &AtomicU32, expected: u32, deadline: MachTime) -> Result<(), Errno> {
        // SAFETY: `word` is a live aligned 4-byte word the kernel only reads; the call blocks
        // until a wake of its address, a signal or `deadline`, and keeps no pointer after.
        let got = unsafe {
            cold::os_sync_wait_on_address_with_deadline(
                word.as_ptr().cast(),
                u64::from(expected),
                WORD,
                OS_SYNC_PRIVATE,
                OS_CLOCK_MACH_ABSOLUTE_TIME,
                deadline.0,
            )
        };
        int_said(got).map(drop)
    }

    /// The park's wake: the cold table's `os_sync_wake_by_address_any`; `ENOENT` is "nobody
    /// waited", which is `Ok(false)`.
    #[blaze::noalloc]
    #[blaze::nopanic]
    fn wake(&self, word: &AtomicU32) -> Result<bool, Errno> {
        // SAFETY: a wake of a live word's address; the kernel reads no memory of it.
        let got = unsafe { cold::os_sync_wake_by_address_any(word.as_ptr().cast(), WORD, OS_SYNC_PRIVATE) };
        match int_said(got) {
            Ok(_) => Ok(true),
            Err(Errno::NoEnt) => Ok(false),
            Err(other) => Err(other),
        }
    }
}

// ---- Kernel ----

/// One entry point of libsystem_kernel the [`Kernel`] strategy calls, by its index in
/// [`KERNEL_ENTRIES`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(usize)]
pub enum Entry {
    OpenNocancel,
    ReadNocancel,
    WriteNocancel,
    CloseNocancel,
    PreadNocancel,
    FcntlNocancel,
    Getpid,
    UlockWait2,
    UlockWake,
    ContinuousTime,
}

impl Entry {
    /// Every entry, in [`KERNEL_ENTRIES`]' order.
    pub const ALL: [Entry; 10] = [
        Entry::OpenNocancel,
        Entry::ReadNocancel,
        Entry::WriteNocancel,
        Entry::CloseNocancel,
        Entry::PreadNocancel,
        Entry::FcntlNocancel,
        Entry::Getpid,
        Entry::UlockWait2,
        Entry::UlockWake,
        Entry::ContinuousTime,
    ];

    /// Its row of [`KERNEL_ENTRIES`].
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn symbol(self) -> Symbol {
        // The table has one row per variant, in this order (`ALL`; the test checks); the
        // array pattern is irrefutable, so no index can be out of range.
        let [open, read, write, close, pread, fcntl, getpid, wait2, wake, continuous] = KERNEL_ENTRIES;
        match self {
            Entry::OpenNocancel => open,
            Entry::ReadNocancel => read,
            Entry::WriteNocancel => write,
            Entry::CloseNocancel => close,
            Entry::PreadNocancel => pread,
            Entry::FcntlNocancel => fcntl,
            Entry::Getpid => getpid,
            Entry::UlockWait2 => wait2,
            Entry::UlockWake => wake,
            Entry::ContinuousTime => continuous,
        }
    }
}

/// Every entry point the [`Kernel`] strategy finds by `dlsym`, with why it and not the public
/// call, and what Apple says of it (rule 45). None is declared `extern`: each is looked up once
/// into the [`Kernel`] value, so this experiment adds no import to libSystem
/// (`tools/ratchets/blaze-sys-libsystem.txt` is unchanged) and a missing one is a fact of
/// [`MacFacts`] and a typed refusal, never a failed load.
pub const KERNEL_ENTRIES: [Symbol; 10] = [
    Symbol {
        name: c"__open_nocancel",
        api: Api::Private,
        why: "the `open_nocancel` system call stub (398) with the mode always in a register: \
              `open` is a variadic C wrapper that reads the mode by `va_arg` and calls `__open`, \
              and the cold table's `openat` adds its own variadic hop; the `_nocancel` form also \
              skips the kernel's pthread-cancellation test. Apple: private, in no SDK header; it \
              is what `open$NOCANCEL` reaches",
    },
    Symbol {
        name: c"__read_nocancel",
        api: Api::Private,
        why: "the `read_nocancel` system call stub (396), what `read$NOCANCEL` is: the plain \
              `read` is the cancellable call (3), whose kernel side tests pthread cancellation \
              first, and blaze reaches it through the cold table's load-and-branch. Apple: \
              private, in no SDK header",
    },
    Symbol {
        name: c"__write_nocancel",
        api: Api::Private,
        why: "the `write_nocancel` system call stub (397), what `write$NOCANCEL` is; no \
              cancellation test. Apple: private, in no SDK header",
    },
    Symbol {
        name: c"__close_nocancel",
        api: Api::Private,
        why: "the `close_nocancel` system call stub (399), what `close$NOCANCEL` is; a close \
              that is a cancellation point can be cancelled after the descriptor is freed, \
              which no caller of blaze's wants. Apple: private, in no SDK header",
    },
    Symbol {
        name: c"__pread_nocancel",
        api: Api::Private,
        why: "the `pread_nocancel` system call stub, what `pread$NOCANCEL` is, without the cold \
              table's hop: the store's small-record read. Apple: private, in no SDK header",
    },
    Symbol {
        name: c"__fcntl_nocancel",
        api: Api::Private,
        why: "the `fcntl_nocancel` system call stub (406), its argument a `long` in a register: \
              `fcntl` is a variadic C wrapper that switches on the command to read its argument. \
              Apple: private, in no SDK header",
    },
    Symbol {
        name: c"__getpid",
        api: Api::Private,
        why: "the `getpid` system call stub under its own name, so the call skips the cold \
              table's `getpid` definition (on 27.2.1 `getpid` and `__getpid` are one address: \
              the only saving is the table's hop). Apple: private, in no SDK header",
    },
    Symbol {
        name: c"__ulock_wait2",
        api: Api::Private,
        why: "the compare-and-wait under `os_sync_wait_on_address`, called with \
              `ULF_NO_ERRNO` so an error is the return value and `errno` is never written; a \
              relative nanosecond timeout. Apple: private (sys/ulock.h is not in the SDK); the \
              documented face is os_sync_wait_on_address (macOS 14.4). Already declared in \
              `macos::ffi`; looked up here by name so no import is added",
    },
    Symbol {
        name: c"__ulock_wake",
        api: Api::Private,
        why: "the wake under `os_sync_wake_by_address_any`, with `ULF_NO_ERRNO`. Apple: \
              private; the documented face is os_sync_wake_by_address_any (macOS 14.4). Already \
              declared in `macos::ffi`; looked up here by name so no import is added",
    },
    Symbol {
        name: c"mach_continuous_time",
        api: Api::Public,
        why: "the clock that counts sleep as a commpage read, scaled here by the timebase: \
              `clock_gettime(CLOCK_MONOTONIC)` reads the same clock behind a switch on the \
              clock id and a `timespec`. Apple: public (mach/mach_time.h, macOS 10.12)",
    },
];

/// Why a [`Kernel`] (or [`Trap`]) cannot be made on this Mac: a fact names it (D185); nothing
/// is put in its place (rule 3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KernelRefusal {
    /// [`MacFacts`] found no such entry point on this macOS.
    Missing { name: &'static CStr, os: crate::macos_facts::OsVersion },
    /// The facts said it was there, and `dlsym` now says it is not: the facts are not this
    /// process's.
    Vanished { name: &'static CStr },
}

impl core::fmt::Display for KernelRefusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            KernelRefusal::Missing { name, os } => write!(
                f,
                "blaze_sys::macos_kernel: the Kernel strategy needs {name:?}, which macOS {os} does not export \
                 (rule 45, D185: a missing entry is a refusal, never a switch to another form)"
            ),
            KernelRefusal::Vanished { name } => write!(
                f,
                "blaze_sys::macos_kernel: {name:?} was in the MacFacts given and `dlsym` does not find it in this \
                 process: the facts were probed elsewhere (D185)"
            ),
        }
    }
}

impl core::error::Error for KernelRefusal {}

/// `ssize_t __read_nocancel(int, void *, size_t)`, the stub's registers.
type ReadFn = unsafe extern "C" fn(c_int, *mut c_void, usize) -> isize;
/// `ssize_t __write_nocancel(int, const void *, size_t)`.
type WriteFn = unsafe extern "C" fn(c_int, *const c_void, usize) -> isize;
/// `ssize_t __pread_nocancel(int, void *, size_t, off_t)`.
type PreadFn = unsafe extern "C" fn(c_int, *mut c_void, usize, i64) -> isize;
/// `int __open_nocancel(const char *, int, int)`: the system call's three register arguments,
/// not `open`'s variadic prototype.
type OpenFn = unsafe extern "C" fn(*const c_char, c_int, c_int) -> c_int;
/// `int __close_nocancel(int)`.
type CloseFn = unsafe extern "C" fn(c_int) -> c_int;
/// `int __fcntl_nocancel(int, int, long)`: the system call's three register arguments, not
/// `fcntl`'s variadic prototype.
type FcntlFn = unsafe extern "C" fn(c_int, c_int, c_long) -> c_int;
/// `pid_t __getpid(void)`.
type GetpidFn = unsafe extern "C" fn() -> i32;
/// `int __ulock_wait2(uint32_t operation, void *addr, uint64_t value, uint64_t timeout_ns,
/// uint64_t value2)`: `macos::ffi::__ulock_wait2`'s signature.
type UlockWait2Fn = unsafe extern "C" fn(u32, *mut c_void, u64, u64, u64) -> c_int;
/// `int __ulock_wake(uint32_t operation, void *addr, uint64_t wake_value)`:
/// `macos::ffi::__ulock_wake`'s signature.
type UlockWakeFn = unsafe extern "C" fn(u32, *mut c_void, u64) -> c_int;
/// `uint64_t mach_continuous_time(void)`.
type ContinuousFn = unsafe extern "C" fn() -> u64;

/// `UL_COMPARE_AND_WAIT` (XNU bsd/sys/ulock.h): a 32-bit compare-and-wait, this process.
const UL_COMPARE_AND_WAIT: u32 = 1;
/// `ULF_NO_ERRNO` (bsd/sys/ulock.h): the error is returned negated, `errno` is not written.
const ULF_NO_ERRNO: u32 = 0x0100_0000;

/// `RTLD_DEFAULT` (dlfcn.h, macOS): every image loaded. None of [`KERNEL_ENTRIES`] is defined
/// in blaze's own image (none is in the cold table), so this finds libSystem's.
pub(crate) const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

/// The address of `name` in the loaded images, or null.
#[blaze::noalloc]
#[blaze::nopanic]
pub(crate) fn find(name: &CStr) -> *mut c_void {
    // SAFETY: `dlsym` reads a live NUL-terminated name and returns an address or null.
    unsafe { crate::macos::ffi::dlsym(RTLD_DEFAULT, name.as_ptr()) }
}

/// libsystem_kernel's own entry points one layer down ([`KERNEL_ENTRIES`]), each found by
/// `dlsym` once when the value is made and held in it (no `static`). `fstat` is [`Public`]'s:
/// it has no `_nocancel` form (it is no cancellation point) and is already the kernel's stub.
#[derive(Clone, Copy, Debug)]
pub struct Kernel {
    open: OpenFn,
    read: ReadFn,
    write: WriteFn,
    close: CloseFn,
    pread: PreadFn,
    fcntl: FcntlFn,
    getpid: GetpidFn,
    ulock_wait2: UlockWait2Fn,
    ulock_wake: UlockWakeFn,
    continuous: ContinuousFn,
    timebase: Timebase,
}

impl Kernel {
    /// The strategy, from this process's facts: a refusal naming the first entry `facts` lacks.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn new(facts: &MacFacts) -> Result<Kernel, KernelRefusal> {
        let at = |entry: Entry| -> Result<*mut c_void, KernelRefusal> {
            let name = entry.symbol().name;
            if !facts.has(entry) {
                return Err(KernelRefusal::Missing { name, os: facts.os });
            }
            let found = find(name);
            if found.is_null() { Err(KernelRefusal::Vanished { name }) } else { Ok(found) }
        };
        // SAFETY (every transmute below): each address is libsystem_kernel's definition of the
        // name its entry names, a function whose C type is the alias it is cast to (each
        // alias's doc names the prototype; the `_nocancel` stubs take the system call's
        // arguments in registers, as XNU's bsd/kern/syscalls.master lists them). A function
        // address and a function pointer have one size and representation on arm64 macOS.
        unsafe {
            Ok(Kernel {
                open: core::mem::transmute::<*mut c_void, OpenFn>(at(Entry::OpenNocancel)?),
                read: core::mem::transmute::<*mut c_void, ReadFn>(at(Entry::ReadNocancel)?),
                write: core::mem::transmute::<*mut c_void, WriteFn>(at(Entry::WriteNocancel)?),
                close: core::mem::transmute::<*mut c_void, CloseFn>(at(Entry::CloseNocancel)?),
                pread: core::mem::transmute::<*mut c_void, PreadFn>(at(Entry::PreadNocancel)?),
                fcntl: core::mem::transmute::<*mut c_void, FcntlFn>(at(Entry::FcntlNocancel)?),
                getpid: core::mem::transmute::<*mut c_void, GetpidFn>(at(Entry::Getpid)?),
                ulock_wait2: core::mem::transmute::<*mut c_void, UlockWait2Fn>(at(Entry::UlockWait2)?),
                ulock_wake: core::mem::transmute::<*mut c_void, UlockWakeFn>(at(Entry::UlockWake)?),
                continuous: core::mem::transmute::<*mut c_void, ContinuousFn>(at(Entry::ContinuousTime)?),
                timebase: facts.timebase,
            })
        }
    }
}

/// `F_GETFL` (sys/fcntl.h).
const F_GETFL: c_int = 3;

impl Calls for Kernel {
    const NAME: &'static str = "Kernel";

    #[blaze::noalloc]
    #[blaze::nopanic]
    fn getpid(&self) -> i32 {
        // SAFETY: `__getpid` takes nothing and touches no memory.
        unsafe { (self.getpid)() }
    }

    #[blaze::noalloc]
    #[blaze::nopanic]
    fn read(&self, fd: Fd, into: &mut [u8]) -> Result<usize, Errno> {
        // SAFETY: `into` is live memory of `into.len()` bytes the call writes at most that
        // many of and keeps no pointer to; on an error the stub sets `errno` (`cerror_nocancel`).
        count_said(unsafe { (self.read)(fd.raw(), into.as_mut_ptr().cast(), into.len()) })
    }

    #[blaze::noalloc]
    #[blaze::nopanic]
    fn pread(&self, fd: Fd, into: &mut [u8], at: u64) -> Result<usize, Errno> {
        let Ok(at) = i64::try_from(at) else {
            return Err(Errno::Inval);
        };
        // SAFETY: as `read`.
        count_said(unsafe { (self.pread)(fd.raw(), into.as_mut_ptr().cast(), into.len(), at) })
    }

    #[blaze::noalloc]
    #[blaze::nopanic]
    fn write(&self, fd: Fd, bytes: &[u8]) -> Result<usize, Errno> {
        // SAFETY: `bytes` is live memory of `bytes.len()` bytes the call only reads.
        count_said(unsafe { (self.write)(fd.raw(), bytes.as_ptr().cast(), bytes.len()) })
    }

    #[blaze::noalloc]
    #[blaze::nopanic]
    fn open(&self, path: Path<'_>, flags: OpenFlags, mode: u32) -> Result<OwnedFd, Errno> {
        // SAFETY: `path` is a live NUL-terminated string the call reads and does not keep; the
        // mode is a register argument the kernel reads only for `O_CREAT`.
        let fd = int_said(unsafe { (self.open)(path.as_ptr().cast(), flags.bits() as c_int, mode as c_int) })?;
        // SAFETY: the call returned a new descriptor to this call alone.
        Ok(unsafe { OwnedFd::from_raw(fd) })
    }

    #[blaze::noalloc]
    #[blaze::nopanic]
    fn close(&self, fd: OwnedFd) -> Result<(), Errno> {
        let raw = fd.into_raw();
        // SAFETY: the call takes a number and no memory; `fd` owned the descriptor, which is
        // freed whether or not the call reports an error.
        int_said(unsafe { (self.close)(raw) }).map(drop)
    }

    /// [`Public`]'s: `fstat` has no `_nocancel` form and is already libsystem_kernel's stub.
    #[blaze::noalloc]
    #[blaze::nopanic]
    fn fstat(&self, fd: Fd) -> Result<stat, Errno> {
        Public.fstat(fd)
    }

    #[blaze::noalloc]
    #[blaze::nopanic]
    fn status_flags(&self, fd: Fd) -> Result<OpenFlags, Errno> {
        // SAFETY: `F_GETFL` reads no argument and no memory.
        let flags = int_said(unsafe { (self.fcntl)(fd.raw(), F_GETFL, 0) })?;
        Ok(OpenFlags::from_bits(flags as u32))
    }

    #[blaze::noalloc]
    #[blaze::nopanic]
    fn now_ns(&self) -> Result<u64, Errno> {
        // SAFETY: `mach_continuous_time` takes nothing; a commpage read.
        Ok(self.timebase.nanos(unsafe { (self.continuous)() }))
    }

    #[blaze::noalloc]
    #[blaze::nopanic]
    fn wait(&self, word: &AtomicU32, expected: u32, deadline: MachTime) -> Result<(), Errno> {
        let now = MachTime::now();
        if deadline <= now {
            return Err(Errno::TimedOut);
        }
        // At least 1 ns: a timeout of 0 is "forever" to `__ulock_wait2`.
        let left = self.timebase.nanos(deadline.0 - now.0).max(1);
        // SAFETY: `word` is a live aligned 4-byte word the kernel only reads; the call blocks
        // until a wake of its address, a signal or `left` nanoseconds; with `ULF_NO_ERRNO` an
        // error is its negated return and `errno` is untouched.
        let got = unsafe {
            (self.ulock_wait2)(UL_COMPARE_AND_WAIT | ULF_NO_ERRNO, word.as_ptr().cast(), u64::from(expected), left, 0)
        };
        if got >= 0 { Ok(()) } else { Err(Errno::from_raw(got.unsigned_abs() as u16)) }
    }

    #[blaze::noalloc]
    #[blaze::nopanic]
    fn wake(&self, word: &AtomicU32) -> Result<bool, Errno> {
        // SAFETY: a wake of a live word's address; the kernel reads no memory of it.
        let got = unsafe { (self.ulock_wake)(UL_COMPARE_AND_WAIT | ULF_NO_ERRNO, word.as_ptr().cast(), 0) };
        match got {
            0.. => Ok(true),
            _ => match Errno::from_raw(got.unsigned_abs() as u16) {
                Errno::NoEnt => Ok(false),
                other => Err(other),
            },
        }
    }
}

// ---- Trap ----

/// The raw system-call instruction with Apple's numbers, on arm64 macOS only, for `getpid`,
/// `read`, `write`, `pread` and `close`. Every other call ([`Calls::open`], `fstat`,
/// `fcntl`, the clock, wait and wake) is [`Kernel`]'s form (`fstat` [`Public`]'s), held inside.
///
/// THE SYSTEM CALL NUMBERS ARE NOT A STABLE INTERFACE. Apple supports only libSystem: the
/// numbers, the registers and the error convention may change in any macOS update, and a binary
/// that traps itself is not supported. Go shipped direct system calls on macOS and moved back to
/// libSystem in Go 1.12 after OS updates broke binaries (`gettimeofday`'s convention changed
/// under it). This strategy exists to be measured; it is never a default, and the OS version it
/// ran on is recorded with every result ([`crate::macos_facts::MacFacts`]'s Display).
#[cfg(target_arch = "aarch64")]
#[derive(Clone, Copy, Debug)]
pub struct Trap {
    kernel: Kernel,
}

/// XNU's numbers, from bsd/kern/syscalls.master (`3 AUE_NULL ALL { user_ssize_t read(...) }`
/// and its neighbours). Not a stable interface (see [`Trap`]).
#[cfg(target_arch = "aarch64")]
mod nr {
    /// `read`.
    pub(super) const READ: usize = 3;
    /// `write`.
    pub(super) const WRITE: usize = 4;
    /// `close`.
    pub(super) const CLOSE: usize = 6;
    /// `getpid`.
    pub(super) const GETPID: usize = 20;
    /// `pread`.
    pub(super) const PREAD: usize = 153;
}

/// One system call of up to four arguments: the number in `x16`, `svc #0x80`; on return the
/// carry flag set means failure and `x0` is the errno, clear means `x0` is the result.
///
/// # Safety
///
/// `number` is a system call whose arguments are `a0..a3` and whose memory contract (what it
/// reads or writes through a pointer argument) the caller meets.
#[cfg(target_arch = "aarch64")]
#[blaze::noalloc]
#[blaze::nopanic]
#[inline(always)]
unsafe fn trap(number: usize, a0: usize, a1: usize, a2: usize, a3: usize) -> Result<usize, Errno> {
    let result: usize;
    let failed: usize;
    // SAFETY: the caller's contract for the call itself. What this rests on, stated (XNU's
    // arm64 `unix_syscall` and its return path, not documented by Apple): the kernel saves and
    // restores every user register except `x0`, `x1` and the flags, which it sets (`x1` is a
    // second result for two-result calls); it returns to the instruction after `svc` both on
    // success and failure, and to the `svc` itself for a restarted call with `x0`..`x3` and
    // `x16` as given; the stack is not used. `x1`..`x3` and `x16` are marked clobbered all
    // the same.
    unsafe {
        core::arch::asm!(
            "svc #0x80",
            "cset {failed}, cs",
            failed = out(reg) failed,
            inlateout("x0") a0 => result,
            inlateout("x1") a1 => _,
            inlateout("x2") a2 => _,
            inlateout("x3") a3 => _,
            inlateout("x16") number => _,
            options(nostack),
        );
    }
    if failed != 0 { Err(Errno::from_raw(result as u16)) } else { Ok(result) }
}

#[cfg(target_arch = "aarch64")]
impl Trap {
    /// The strategy, from this process's facts: it needs [`Kernel`] for the calls it does not
    /// trap, so it refuses as [`Kernel::new`] does.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn new(facts: &MacFacts) -> Result<Trap, KernelRefusal> {
        Ok(Trap { kernel: Kernel::new(facts)? })
    }
}

#[cfg(target_arch = "aarch64")]
impl Calls for Trap {
    const NAME: &'static str = "Trap";

    #[blaze::noalloc]
    #[blaze::nopanic]
    fn getpid(&self) -> i32 {
        // SAFETY: `getpid` takes no arguments and cannot fail.
        match unsafe { trap(nr::GETPID, 0, 0, 0, 0) } {
            Ok(pid) => pid as i32,
            Err(_) => -1,
        }
    }

    #[blaze::noalloc]
    #[blaze::nopanic]
    fn read(&self, fd: Fd, into: &mut [u8]) -> Result<usize, Errno> {
        // SAFETY: `into` is live memory of `into.len()` bytes the kernel writes at most that
        // many of and keeps no pointer to.
        unsafe { trap(nr::READ, fd.raw() as usize, into.as_mut_ptr() as usize, into.len(), 0) }
    }

    #[blaze::noalloc]
    #[blaze::nopanic]
    fn pread(&self, fd: Fd, into: &mut [u8], at: u64) -> Result<usize, Errno> {
        if i64::try_from(at).is_err() {
            return Err(Errno::Inval);
        }
        // SAFETY: as `read`; the offset is the 64-bit `off_t` in `x3`.
        unsafe { trap(nr::PREAD, fd.raw() as usize, into.as_mut_ptr() as usize, into.len(), at as usize) }
    }

    #[blaze::noalloc]
    #[blaze::nopanic]
    fn write(&self, fd: Fd, bytes: &[u8]) -> Result<usize, Errno> {
        // SAFETY: `bytes` is live memory of `bytes.len()` bytes the kernel only reads.
        unsafe { trap(nr::WRITE, fd.raw() as usize, bytes.as_ptr() as usize, bytes.len(), 0) }
    }

    /// [`Kernel`]'s `__open_nocancel`: not trapped.
    #[blaze::noalloc]
    #[blaze::nopanic]
    fn open(&self, path: Path<'_>, flags: OpenFlags, mode: u32) -> Result<OwnedFd, Errno> {
        self.kernel.open(path, flags, mode)
    }

    #[blaze::noalloc]
    #[blaze::nopanic]
    fn close(&self, fd: OwnedFd) -> Result<(), Errno> {
        let raw = fd.into_raw();
        // SAFETY: the call takes a number and no memory; `fd` owned the descriptor.
        unsafe { trap(nr::CLOSE, raw as usize, 0, 0, 0) }.map(drop)
    }

    /// [`Public`]'s `fstat`: not trapped.
    #[blaze::noalloc]
    #[blaze::nopanic]
    fn fstat(&self, fd: Fd) -> Result<stat, Errno> {
        self.kernel.fstat(fd)
    }

    /// [`Kernel`]'s `__fcntl_nocancel`: not trapped.
    #[blaze::noalloc]
    #[blaze::nopanic]
    fn status_flags(&self, fd: Fd) -> Result<OpenFlags, Errno> {
        self.kernel.status_flags(fd)
    }

    /// [`Kernel`]'s `mach_continuous_time`: a commpage read, no system call to trap.
    #[blaze::noalloc]
    #[blaze::nopanic]
    fn now_ns(&self) -> Result<u64, Errno> {
        self.kernel.now_ns()
    }

    /// [`Kernel`]'s `__ulock_wait2`: not trapped.
    #[blaze::noalloc]
    #[blaze::nopanic]
    fn wait(&self, word: &AtomicU32, expected: u32, deadline: MachTime) -> Result<(), Errno> {
        self.kernel.wait(word, expected, deadline)
    }

    /// [`Kernel`]'s `__ulock_wake`: not trapped.
    #[blaze::noalloc]
    #[blaze::nopanic]
    fn wake(&self, word: &AtomicU32) -> Result<bool, Errno> {
        self.kernel.wake(word)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every entry the Kernel strategy uses is one this macOS exports (rule 45), and says why;
    /// the table's rows are in [`Entry::ALL`]'s order.
    #[test]
    fn every_kernel_entry_exists_and_says_why() {
        assert_eq!(KERNEL_ENTRIES.len(), Entry::ALL.len());
        for (at, entry) in Entry::ALL.iter().enumerate() {
            assert_eq!(*entry as usize, at);
            let symbol = entry.symbol();
            assert!(
                !find(symbol.name).is_null(),
                "libsystem_kernel has no {:?} on this macOS; the Kernel strategy uses it for: {}",
                symbol.name,
                symbol.why
            );
            assert!(symbol.why.len() > 40, "{:?} gives no reason", symbol.name);
        }
        assert!(find(c"__blaze_sys_no_such_entry").is_null(), "the control: a name nobody exports is not found");
    }

    /// What one strategy answered, for the equality across strategies.
    #[derive(Debug, PartialEq, Eq)]
    struct Seen {
        pid: i32,
        zero: (usize, u8),
        written: usize,
        pread: (usize, [u8; 8]),
        size: i64,
        access: u32,
        wake_alone: bool,
        wait_differs: bool,
        timed_out: bool,
    }

    /// The bytes the temp file holds.
    const BYTES: &[u8] = b"blaze-sys macos_kernel: one file read by three strategies";

    fn observe<C: Calls>(calls: &C, dir: &std::path::Path) -> Seen {
        let path = std::ffi::CString::new(dir.join(C::NAME).into_os_string().into_encoded_bytes()).unwrap();
        let path = Path::new(&path);
        let made = OpenFlags::WRITE.with(OpenFlags::CREATE).with(OpenFlags::TRUNCATE).with(OpenFlags::CLOSE_ON_EXEC);
        let out = calls.open(path, made, 0o600).expect("create the temp file");
        let written = calls.write(out.fd(), BYTES).expect("write");
        calls.close(out).expect("close after write");

        let file = calls.open(path, OpenFlags::READ, 0).expect("open to read");
        let mut eight = [0u8; 8];
        let got = calls.pread(file.fd(), &mut eight, 10).expect("pread");
        let size = calls.fstat(file.fd()).expect("fstat").st_size;
        let access = calls.status_flags(file.fd()).expect("fcntl F_GETFL").bits() & 0x3;
        calls.close(file).expect("close");

        let zero = calls.open(Path::new(c"/dev/zero"), OpenFlags::READ, 0).expect("open /dev/zero");
        let mut one = [0xffu8; 1];
        let n = calls.read(zero.fd(), &mut one).expect("read /dev/zero");
        calls.close(zero).expect("close /dev/zero");

        let word = AtomicU32::new(7);
        let wake_alone = calls.wake(&word).expect("a wake with no waiter");
        let soon = MachTime::now().after(1_000_000_000, facts().timebase);
        let wait_differs = calls.wait(&word, 8, soon).is_ok();
        let timed_out = calls.wait(&word, 7, MachTime::now().after(1_000_000, facts().timebase)) == Err(Errno::TimedOut);

        Seen {
            pid: calls.getpid(),
            zero: (n, one[0]),
            written,
            pread: (got, eight),
            size,
            access,
            wake_alone,
            wait_differs,
            timed_out,
        }
    }

    fn facts() -> MacFacts {
        MacFacts::probe().expect("this Mac's facts")
    }

    fn temp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("blaze-sys-macos-kernel-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The three strategies answer every hot call the same (rule 78), on this macOS, which the
    /// output records (the Trap's numbers are not a stable interface).
    #[test]
    #[expect(clippy::print_stdout, reason = "the test's output records the macOS the Trap's numbers ran on")]
    fn the_strategies_agree() {
        let facts = facts();
        println!("macos_kernel: {facts}");
        let dir = temp("agree");
        let public = observe(&Public, &dir);
        assert_eq!(public.pid as u32, std::process::id());
        assert_eq!(public.zero, (1, 0));
        assert_eq!(public.written, BYTES.len());
        assert_eq!(&public.pread.1, &BYTES[10..18]);
        assert_eq!(public.size, BYTES.len() as i64);
        assert!(!public.wake_alone && public.wait_differs && public.timed_out, "{public:?}");
        let kernel = Kernel::new(&facts).expect("the Kernel strategy on this Mac");
        assert_eq!(observe(&kernel, &dir), public, "Kernel against Public");
        #[cfg(target_arch = "aarch64")]
        {
            let trap = Trap::new(&facts).expect("the Trap strategy on this Mac");
            assert_eq!(observe(&trap, &dir), public, "Trap against Public, on {facts}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The clocks: each monotonic, and the three within a millisecond of each other read back
    /// to back (one clock, Mach's continuous time, read three ways).
    #[test]
    fn the_clocks_agree() {
        let facts = facts();
        let kernel = Kernel::new(&facts).unwrap();
        let a = Public.now_ns().unwrap();
        let b = kernel.now_ns().unwrap();
        let c = Public.now_ns().unwrap();
        let d = kernel.now_ns().unwrap();
        assert!(a <= c && b <= d, "monotonic: {a} {c}, {b} {d}");
        assert!(b.abs_diff(a) < 1_000_000 && d.abs_diff(c) < 1_000_000, "close: {a} {b} {c} {d}");
        #[cfg(target_arch = "aarch64")]
        {
            let trap = Trap::new(&facts).unwrap();
            let e = trap.now_ns().unwrap();
            assert!(e >= d && e.abs_diff(Public.now_ns().unwrap()) < 1_000_000);
        }
    }

    /// Errors agree too: each strategy reports a closed descriptor as `EBADF`, by `errno` for
    /// the C calls and by `x0` for the trap.
    #[test]
    fn errors_agree() {
        let facts = facts();
        let kernel = Kernel::new(&facts).unwrap();
        let bad = Fd::new(1_000_000);
        let mut one = [0u8; 1];
        assert_eq!(Public.read(bad, &mut one), Err(Errno::BadF));
        assert_eq!(kernel.read(bad, &mut one), Err(Errno::BadF));
        assert_eq!(kernel.pread(bad, &mut one, 0), Err(Errno::BadF));
        #[cfg(target_arch = "aarch64")]
        {
            let trap = Trap::new(&facts).unwrap();
            assert_eq!(trap.read(bad, &mut one), Err(Errno::BadF));
            assert_eq!(trap.pread(bad, &mut one, 0), Err(Errno::BadF));
            assert_eq!(trap.write(bad, &one), Err(Errno::BadF));
        }
    }

    /// The control of the equality (a planted divergence): a strategy whose `pread` reads one
    /// byte short is caught by the same comparison.
    #[test]
    fn a_planted_divergence_is_caught() {
        struct Short;
        impl Calls for Short {
            const NAME: &'static str = "Short";
            fn getpid(&self) -> i32 {
                Public.getpid()
            }
            fn read(&self, fd: Fd, into: &mut [u8]) -> Result<usize, Errno> {
                Public.read(fd, into)
            }
            fn pread(&self, fd: Fd, into: &mut [u8], at: u64) -> Result<usize, Errno> {
                let len = into.len().saturating_sub(1);
                Public.pread(fd, into.get_mut(..len).unwrap(), at)
            }
            fn write(&self, fd: Fd, bytes: &[u8]) -> Result<usize, Errno> {
                Public.write(fd, bytes)
            }
            fn open(&self, path: Path<'_>, flags: OpenFlags, mode: u32) -> Result<OwnedFd, Errno> {
                Public.open(path, flags, mode)
            }
            fn close(&self, fd: OwnedFd) -> Result<(), Errno> {
                Public.close(fd)
            }
            fn fstat(&self, fd: Fd) -> Result<stat, Errno> {
                Public.fstat(fd)
            }
            fn status_flags(&self, fd: Fd) -> Result<OpenFlags, Errno> {
                Public.status_flags(fd)
            }
            fn now_ns(&self) -> Result<u64, Errno> {
                Public.now_ns()
            }
            fn wait(&self, word: &AtomicU32, expected: u32, deadline: MachTime) -> Result<(), Errno> {
                Public.wait(word, expected, deadline)
            }
            fn wake(&self, word: &AtomicU32) -> Result<bool, Errno> {
                Public.wake(word)
            }
        }
        let dir = temp("planted");
        assert_ne!(observe(&Short, &dir), observe(&Public, &dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The timebase's conversions are exact inverses on whole ticks, and agree with 128-bit
    /// arithmetic.
    #[test]
    fn the_timebase_converts_exactly() {
        let base = Timebase { numer: NonZeroU32::new(125).unwrap(), denom: NonZeroU32::new(3).unwrap() };
        for ticks in [0u64, 1, 2, 3, 24_000_000, 1 << 40, u64::MAX / 200] {
            let wide = (u128::from(ticks) * 125 / 3) as u64;
            assert_eq!(base.nanos(ticks), wide, "{ticks}");
            assert!(base.ticks(base.nanos(ticks)).abs_diff(ticks) <= 1, "{ticks}");
        }
    }
}
