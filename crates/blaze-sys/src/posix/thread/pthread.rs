//! pthreads on macOS: the thread handle and its opaque types, keys (a thread's value under a
//! key, with a destructor run as the thread ends), the calling thread's stack and id, names,
//! the signal mask, and the QoS classes. Declared from pthread.h (libpthread in libSystem).
//!
//! On Linux pthreads are the C library's, with no kernel form, and are not offered: blaze-sys
//! references no C-library symbol there. What a Linux caller uses instead:
//!
//! - the calling thread's id: [`thread_id`] (`gettid`, [`crate::linux::sample::gettid`]);
//! - a signal to one thread: `crate::linux::signal::tgkill` with that id and
//!   [`crate::linux::getpid`];
//! - the signal mask: [`crate::linux::rt_sigprocmask`];
//! - a thread's value under a key: the thread pointer, or blaze's own per-thread word
//!   (sys.md, "threads");
//! - a thread's stack: its bounds as the thread was made (the creator's mapping), or the
//!   mapping in `/proc/self/maps` that holds the stack pointer;
//! - a lock and a condition: the futex ([`crate::linux::futex_wait_bitset`],
//!   [`crate::linux::futex_wake`]);
//! - a thread's name: `prctl(PR_GET_NAME)` ([`crate::linux::prctl`]) or
//!   `/proc/thread-self/comm`.

#[cfg(target_os = "macos")]
use core::ffi::{c_char, c_int, c_uint, c_void};

#[cfg(target_os = "macos")]
use crate::Errno;
#[cfg(target_os = "macos")]
use crate::posix::signal::sigset_t;
#[cfg(target_os = "macos")]
use crate::posix::types::size_t;

/// A thread, as pthreads names it: a pointer to the library's own record
/// (`struct _opaque_pthread_t *`, sys/_pthread/_pthread_t.h).
///
/// replaces libc::pthread_t
#[cfg(target_os = "macos")]
pub type pthread_t = *mut c_void;

/// A key of thread-specific data: `unsigned long` on macOS (sys/_pthread/_pthread_key_t.h).
///
/// replaces libc::pthread_key_t
#[cfg(target_os = "macos")]
pub type pthread_key_t = core::ffi::c_ulong;

/// A key's destructor: run with the thread's non-null value as the thread ends.
#[cfg(target_os = "macos")]
pub type pthread_key_destructor = Option<unsafe extern "C" fn(*mut c_void)>;

/// A thread's entry: `void *(*)(void *)`.
#[cfg(target_os = "macos")]
pub type pthread_start = extern "C" fn(*mut c_void) -> *mut c_void;

// ---------------------------------------------------------------------------------------------
// The opaque types. Only their size and alignment are the ABI; the bytes are the library's.
// ---------------------------------------------------------------------------------------------

/// `pthread_attr_t` (sys/_pthread/_pthread_types.h, macOS 64-bit: a `long` signature and 56
/// bytes).
///
/// replaces libc::pthread_attr_t
#[cfg(target_os = "macos")]
#[derive(Clone, Copy)]
#[repr(C)]
pub struct pthread_attr_t {
    sig: core::ffi::c_long,
    opaque: [u8; 56],
}

#[cfg(target_os = "macos")]
impl pthread_attr_t {
    /// All zero: the out-parameter `pthread_attr_init` fills. Not an attribute any other call
    /// may read until it has.
    pub const ZERO: pthread_attr_t = pthread_attr_t { sig: 0, opaque: [0; 56] };
}

/// `pthread_mutex_t` (sys/_pthread/_pthread_types.h, macOS 64-bit: a `long` signature and 56
/// bytes).
///
/// replaces libc::pthread_mutex_t
#[cfg(target_os = "macos")]
#[repr(C)]
pub struct pthread_mutex_t {
    sig: core::ffi::c_long,
    opaque: [u8; 56],
}

/// `pthread_cond_t` (sys/_pthread/_pthread_types.h, macOS 64-bit: a `long` signature and 40
/// bytes).
///
/// replaces libc::pthread_cond_t
#[cfg(target_os = "macos")]
#[repr(C)]
pub struct pthread_cond_t {
    sig: core::ffi::c_long,
    opaque: [u8; 40],
}

/// `PTHREAD_MUTEX_INITIALIZER` (pthread.h): the signature `_PTHREAD_MUTEX_SIG_init`
/// (0x32AAABA7) and zeros.
///
/// replaces libc::PTHREAD_MUTEX_INITIALIZER
#[cfg(target_os = "macos")]
pub const PTHREAD_MUTEX_INITIALIZER: pthread_mutex_t = pthread_mutex_t { sig: 0x32AA_ABA7, opaque: [0; 56] };

/// `PTHREAD_COND_INITIALIZER` (pthread.h): the signature `_PTHREAD_COND_SIG_init`
/// (0x3CB0B1BB) and zeros.
///
/// replaces libc::PTHREAD_COND_INITIALIZER
#[cfg(target_os = "macos")]
pub const PTHREAD_COND_INITIALIZER: pthread_cond_t = pthread_cond_t { sig: 0x3CB0_B1BB, opaque: [0; 40] };

// sys/_pthread/_pthread_types.h, macOS arm64.
#[cfg(target_os = "macos")]
const _: () = {
    assert!(size_of::<pthread_attr_t>() == 64);
    assert!(align_of::<pthread_attr_t>() == 8);
    assert!(size_of::<pthread_mutex_t>() == 64);
    assert!(align_of::<pthread_mutex_t>() == 8);
    assert!(size_of::<pthread_cond_t>() == 48);
    assert!(align_of::<pthread_cond_t>() == 8);
    assert!(size_of::<pthread_t>() == 8);
    assert!(size_of::<pthread_key_t>() == 8);
};

// ---------------------------------------------------------------------------------------------
// The declarations. Those the cold table defines are below, as calls of it with the header's
// signature.
// ---------------------------------------------------------------------------------------------

#[cfg(target_os = "macos")]
#[allow(clashing_extern_declarations, reason = "blaze-sys's own typed signatures; macos_sample.rs declares a few of these with the same types")]
unsafe extern "C" {
    /// `pthread_self` (pthread.h): the calling thread. Cannot fail.
    ///
    /// replaces libc::pthread_self
    pub safe fn pthread_self() -> pthread_t;

    /// `pthread_kill` (signal.h): `signal` sent to `thread` of this process; the error's number
    /// returned (0 for none). Linux's kernel form is `crate::linux::signal::tgkill`.
    ///
    /// replaces libc::pthread_kill
    pub fn pthread_kill(thread: pthread_t, signal: c_int) -> c_int;

    /// `pthread_sigmask` (signal.h): the calling thread's signal mask changed by `how`
    /// (`SIG_BLOCK`, `SIG_UNBLOCK`, `SIG_SETMASK`) with `set` (null: unchanged), the old one
    /// written to `old` (null: not asked); the error's number returned. Linux's kernel form is
    /// [`crate::linux::rt_sigprocmask`].
    ///
    /// replaces libc::pthread_sigmask
    pub fn pthread_sigmask(how: c_int, set: *const sigset_t, old: *mut sigset_t) -> c_int;

    /// `pthread_key_create` (pthread.h): a new key written to `key`, its value null in every
    /// thread; the error's number returned (`EAGAIN` when the process has no key left).
    ///
    /// replaces libc::pthread_key_create
    pub fn pthread_key_create(key: *mut pthread_key_t, destructor: pthread_key_destructor) -> c_int;

    /// `pthread_key_delete` (pthread.h): `key` given back; no destructor runs.
    ///
    /// replaces libc::pthread_key_delete
    pub fn pthread_key_delete(key: pthread_key_t) -> c_int;

    /// `pthread_getspecific` (pthread.h): the calling thread's value under `key` (null where
    /// none was set).
    ///
    /// replaces libc::pthread_getspecific
    pub fn pthread_getspecific(key: pthread_key_t) -> *mut c_void;

    /// `pthread_setspecific` (pthread.h): the calling thread's value under `key` set.
    ///
    /// replaces libc::pthread_setspecific
    pub fn pthread_setspecific(key: pthread_key_t, value: *const c_void) -> c_int;

    /// `pthread_mutex_lock` (pthread.h).
    ///
    /// replaces libc::pthread_mutex_lock
    pub fn pthread_mutex_lock(mutex: *mut pthread_mutex_t) -> c_int;

    /// `pthread_mutex_unlock` (pthread.h).
    ///
    /// replaces libc::pthread_mutex_unlock
    pub fn pthread_mutex_unlock(mutex: *mut pthread_mutex_t) -> c_int;

    /// `pthread_cond_signal` (pthread.h): one waiter of `cond` woken.
    ///
    /// replaces libc::pthread_cond_signal
    pub fn pthread_cond_signal(cond: *mut pthread_cond_t) -> c_int;

    /// `pthread_cond_wait` (pthread.h): `mutex` (held) released while the thread waits on
    /// `cond`, and held again before it returns. A wake may be spurious.
    ///
    /// replaces libc::pthread_cond_wait
    pub fn pthread_cond_wait(cond: *mut pthread_cond_t, mutex: *mut pthread_mutex_t) -> c_int;

    /// `pthread_attr_getstack` (pthread.h): the lowest address and the size of the stack an
    /// attribute names.
    ///
    /// replaces libc::pthread_attr_getstack
    pub fn pthread_attr_getstack(attr: *const pthread_attr_t, low: *mut *mut c_void, size: *mut size_t) -> c_int;

    /// `pthread_getname_np` (pthread.h, macOS 10.6): `thread`'s name, NUL-terminated, into
    /// `name` of `len` bytes (`ERANGE` where it does not fit).
    ///
    /// replaces libc::pthread_getname_np
    pub fn pthread_getname_np(thread: pthread_t, name: *mut c_char, len: size_t) -> c_int;

    /// `pthread_get_stackaddr_np` (pthread.h, macOS): the top of `thread`'s stack (it grows
    /// down from it).
    ///
    /// replaces libc::pthread_get_stackaddr_np
    pub fn pthread_get_stackaddr_np(thread: pthread_t) -> *mut c_void;

    /// `pthread_get_stacksize_np` (pthread.h, macOS): its stack's size in bytes.
    ///
    /// replaces libc::pthread_get_stacksize_np
    pub fn pthread_get_stacksize_np(thread: pthread_t) -> size_t;

    /// `pthread_threadid_np` (pthread.h, macOS 10.6): the system-wide 64-bit id of `thread`
    /// (null: the calling thread) into `id`.
    ///
    /// replaces libc::pthread_threadid_np
    pub fn pthread_threadid_np(thread: pthread_t, id: *mut u64) -> c_int;

    /// `pthread_mach_thread_np` (pthread.h, macOS): `thread`'s Mach port in this task.
    ///
    /// replaces libc::pthread_mach_thread_np
    pub fn pthread_mach_thread_np(thread: pthread_t) -> super::mach::mach_port_t;

    /// `pthread_from_mach_thread_np` (pthread.h, macOS): the pthread of a Mach thread port of
    /// this task; null for a thread pthreads did not make.
    ///
    /// replaces libc::pthread_from_mach_thread_np
    pub fn pthread_from_mach_thread_np(port: super::mach::mach_port_t) -> pthread_t;

    /// `pthread_get_qos_class_np` (pthread/qos.h, macOS 10.10): `thread`'s QoS class (a
    /// `qos_class_t`, an `unsigned int`) and its relative priority.
    ///
    /// replaces libc::pthread_get_qos_class_np
    pub fn pthread_get_qos_class_np(thread: pthread_t, class: *mut c_uint, relative: *mut c_int) -> c_int;
}

/// `pthread_create` (pthread.h): a thread running `start(arg)`, its handle written to `thread`;
/// `attr` null for the defaults. The error's number returned. Through the cold table.
///
/// # Safety
///
/// pthread.h's contract: `thread` is writable, `attr` null or initialized, and `arg` valid for
/// what `start` does with it on the new thread.
///
/// replaces libc::pthread_create
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn pthread_create(thread: *mut pthread_t, attr: *const pthread_attr_t, start: pthread_start, arg: *mut c_void) -> c_int {
    // SAFETY: the caller's contract. The cold entry is libSystem's `pthread_create`; its
    // `pthread_t *` is `*mut *mut c_void` (a `pthread_t` is a pointer) and its attribute a
    // `*const c_void`, the same pointers.
    unsafe { crate::macos::cold::pthread_create(thread, attr.cast(), start, arg) }
}

/// `pthread_attr_destroy` (pthread.h). Through the cold table.
///
/// # Safety
///
/// `attr` was initialized (`pthread_attr_init`) and is not read after.
///
/// replaces libc::pthread_attr_destroy
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn pthread_attr_destroy(attr: *mut pthread_attr_t) -> c_int {
    // SAFETY: the caller's contract; the cold entry takes the attribute as `*mut c_void`.
    unsafe { crate::macos::cold::pthread_attr_destroy(attr.cast()) }
}

/// `pthread_set_qos_class_self_np(class, relative)` (pthread/qos.h, macOS 10.10): the calling
/// thread's QoS class (a `qos_class_t`, as its `unsigned int`) and relative priority (0 or
/// below, to `QOS_MIN_RELATIVE_PRIORITY`); the error's number returned. The cold table's
/// entry, re-exported.
///
/// replaces libc::pthread_set_qos_class_self_np
#[cfg(target_os = "macos")]
pub use crate::macos::cold::pthread_set_qos_class_self_np;

// ---------------------------------------------------------------------------------------------
// Typed wrappers.
// ---------------------------------------------------------------------------------------------

/// A pthread call's result: 0, or the error's number (pthreads return it; they set no
/// `errno`).
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
fn checked(code: c_int) -> Result<(), Errno> {
    if code == 0 { Ok(()) } else { Err(Errno::from_raw(code as u16)) }
}

/// A new thread key with `destructor`: `pthread_key_create`. [`Errno::Again`] when the
/// process has used all of its keys. Used by blaze-launch-rt's thread-local tables and
/// blaze-ladder's TLS index. macOS only.
///
/// replaces libc::pthread_key_create (the typed form)
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn key_create(destructor: pthread_key_destructor) -> Result<pthread_key_t, Errno> {
    let mut key: pthread_key_t = 0;
    // SAFETY: `key` is a live local the call writes once; the destructor, if any, is a
    // function the caller gave for this key's values.
    checked(unsafe { pthread_key_create(&raw mut key, destructor) })?;
    Ok(key)
}

/// `pthread_key_delete`: `key` given back; no destructor runs for values still set.
/// [`Errno::Inval`] for a key that is not one. macOS only.
///
/// replaces libc::pthread_key_delete (the typed form)
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn key_delete(key: pthread_key_t) -> Result<(), Errno> {
    // SAFETY: the call takes a number; an unknown key is refused with `EINVAL`.
    checked(unsafe { pthread_key_delete(key) })
}

/// `pthread_setspecific`: the calling thread's value under `key` set. [`Errno::NoMem`] where
/// the library could not grow the thread's table. macOS only.
///
/// # Safety
///
/// `key` came from [`key_create`] and was not deleted (POSIX leaves any other key undefined),
/// and `value` is what that key's destructor may be called with as this thread ends.
///
/// replaces libc::pthread_setspecific (the typed form)
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn set_specific(key: pthread_key_t, value: *const c_void) -> Result<(), Errno> {
    // SAFETY: the caller's contract; the library keeps the pointer, never reads it.
    checked(unsafe { pthread_setspecific(key, value) })
}

/// The range of a thread's stack: `[low, high)`, the stack growing down from `high`. On Linux
/// the value a thread's creator or `/proc/self/maps` gives (blaze-sys has no call for it).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StackRange {
    /// Its lowest address (the guard page is below it).
    pub low: usize,
    /// One past its highest address.
    pub high: usize,
}

/// The calling thread's stack: `pthread_get_stackaddr_np` and `pthread_get_stacksize_np`
/// (which cannot fail). Used by the fault reports, the profilers' walks and the scheduler's
/// stack checks. macOS only: on Linux a thread's stack is the C library's record, with no
/// kernel form; take it from the thread's creation or from `/proc/self/maps`.
///
/// replaces libc::pthread_get_stackaddr_np with libc::pthread_get_stacksize_np (macOS), the
/// typed form
/// replaces libc::pthread_getattr_np (with pthread_attr_getstack: blaze_sys::posix::thread::this_thread_stack on macOS; Linux has no kernel form, glibc's record of the thread)
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn this_thread_stack() -> Result<StackRange, Errno> {
    let me = pthread_self();
    // SAFETY: `me` is the calling thread, alive for the call; both read its own record.
    let (top, size) = unsafe { (pthread_get_stackaddr_np(me), pthread_get_stacksize_np(me)) };
    let high = top.addr();
    Ok(StackRange { low: high.saturating_sub(size), high })
}

/// The calling thread's stack high water: the bytes from its top ([`StackRange::high`]) down to
/// the lowest word of it that is not zero. A thread's stack is a fresh mapping on macOS (pthreads
/// maps it per thread, zero-filled, its guard page below [`StackRange::low`]), so a word below
/// every frame the thread ever had reads zero: the scan goes up from `low` to the first word
/// written. A frame's lowest words may be zero themselves, so the mark is a lower bound short by
/// at most one frame's unwritten tail. Meant for a thread made for the measurement (a test's
/// check on a thread of a stated stack): on a thread that reuses a stack it reads the reuse.
/// Reads the range below the caller's frame: `range.high - low` bytes at most, word by word.
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn stack_high_water() -> Result<usize, Errno> {
    let range = this_thread_stack()?;
    let local = 0usize;
    // The caller's frame and those above it are live: the scan stops below this local.
    let below = core::hint::black_box(&raw const local).addr();
    let word = core::mem::size_of::<usize>();
    // `low` is page aligned (a mapping's start), so word aligned.
    let mut at = range.low;
    while at.saturating_add(word) <= below {
        // SAFETY: `[low, high)` is this thread's own stack, mapped readable for its life
        // (`this_thread_stack`, the guard page is below `low`); `at` is word aligned and below
        // the live frames, so no reference covers it and no other thread writes it. A volatile
        // read of a dead stack word reads what was last written there, or zero if never.
        let value = unsafe { core::ptr::read_volatile(core::ptr::with_exposed_provenance::<usize>(at)) };
        if value != 0 {
            return Ok(range.high.saturating_sub(at));
        }
        at = at.saturating_add(word);
    }
    Ok(range.high.saturating_sub(below))
}

/// The calling thread's system-wide id: `gettid` on Linux ([`crate::linux::sample::gettid`],
/// a system call), `pthread_threadid_np(NULL)` on macOS. Cannot fail. Used by the profilers'
/// samples.
///
/// replaces libc::gettid (Linux) and libc::pthread_threadid_np (macOS), the typed form
#[blaze::noalloc]
#[blaze::nopanic]
pub fn thread_id() -> u64 {
    #[cfg(target_os = "macos")]
    {
        let mut id = 0u64;
        // SAFETY: a null thread is the calling one; `id` is a live local written once.
        unsafe { pthread_threadid_np(core::ptr::null_mut(), &raw mut id) };
        id
    }
    #[cfg(target_os = "linux")]
    {
        crate::linux::sample::gettid() as u64
    }
}
