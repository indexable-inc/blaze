//! How a thread reaches its heap, and learns when it ends.
//!
//! The thread's end comes from a pthread key's destructor, the one hook every Unix calls for each
//! thread with a value set, which runs without allocating. The heap's address is the key's
//! value, so the destructor is handed it.
//!
//! The read on every allocation and free (rule 45, measured by `examples/cost.rs`'s `tls` rows on
//! the Mac, 2026-11-15):
//!
//! - macOS on arm64: the key's slot read directly, `TPIDRRO_EL0` (the thread's TSD base, its low
//!   three bits the CPU number, masked off) indexed by the key: what `pthread_getspecific` does
//!   inside libsystem_pthread (`_pthread_getspecific_direct`), without the call. A private ABI
//!   (the TSD layout), tested equal to `pthread_getspecific` (`tests.rs`). A `#[thread_local]`
//!   there is a call through its TLV descriptor (`tlv_get_addr`) per access.
//! - Linux and the rest: a `#[thread_local]` word, a load at a fixed offset from the thread
//!   pointer in an executable (initial-exec), set beside the key.
//! - Under Miri, which runs no `asm!`: `pthread_getspecific`.

use core::{ffi::c_void, ptr};

use crate::{
    MallocError,
    heap::{self, Heap},
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

#[cfg(target_os = "macos")]
type Key = core::ffi::c_ulong;
#[cfg(not(target_os = "macos"))]
type Key = core::ffi::c_uint;

unsafe extern "C" {
    fn pthread_key_create(
        key: *mut Key,
        destructor: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> core::ffi::c_int;
    fn pthread_setspecific(key: Key, value: *const c_void) -> core::ffi::c_int;
    fn pthread_getspecific(key: Key) -> *mut c_void;
}

/// The key, plus one (0: not made yet).
static KEY: AtomicUsize = AtomicUsize::new(0);

/// Making the key: 0 not begun, 1 under way, 2 made.
static MAKING: AtomicUsize = AtomicUsize::new(0);

/// Heaps adopted from ended threads, process-wide.
pub(crate) static ADOPTED: AtomicU64 = AtomicU64::new(0);

pub(crate) fn count_adoption() {
    ADOPTED.fetch_add(1, Ordering::Relaxed);
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64", not(miri))))]
#[thread_local]
static CURRENT: core::cell::Cell<*const Heap> = core::cell::Cell::new(ptr::null());

/// The thread's heap, or null before its first allocation and after its end.
#[inline(always)]
pub(crate) fn current() -> *const Heap {
    #[cfg(all(target_os = "macos", target_arch = "aarch64", not(miri)))]
    {
        let key = KEY.load(Ordering::Relaxed);
        if key == 0 {
            return ptr::null();
        }
        // SAFETY: a made key's slot in the thread's TSD array, which libsystem_pthread keeps for
        // every thread for its life (see the module's note).
        unsafe { tsd_slot(key - 1).cast() }
    }
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64", not(miri))))]
    {
        CURRENT.get()
    }
}

/// The value of TSD slot `key` of the calling thread, read directly.
///
/// # Safety
///
/// `key` is a key `pthread_key_create` made.
#[cfg(all(target_os = "macos", target_arch = "aarch64", not(miri)))]
#[inline(always)]
pub(crate) unsafe fn tsd_slot(key: usize) -> *mut c_void {
    let base: usize;
    // SAFETY: `mrs` of TPIDRRO_EL0 reads a register EL0 may read, the thread's TSD base, which
    // does not change for the thread's life (so `pure`, `nomem`).
    unsafe {
        core::arch::asm!("mrs {}, tpidrro_el0", out(reg) base, options(nomem, nostack, preserves_flags, pure));
    }
    let tsd = (base & !7) as *const *mut c_void;
    // SAFETY: the TSD array holds a slot for every key (`PTHREAD_KEYS_MAX`); the caller's key is
    // one; the slot is the thread's own.
    unsafe { *tsd.add(key) }
}

/// The value of the heap key for this thread through `pthread_getspecific`: the reference the
/// direct read is tested against.
pub(crate) fn by_call() -> *mut c_void {
    let key = KEY.load(Ordering::Acquire);
    if key == 0 {
        return ptr::null_mut();
    }
    // SAFETY: a made key.
    unsafe { pthread_getspecific((key - 1) as Key) }
}

/// The made key, plus one.
pub(crate) fn key() -> usize {
    KEY.load(Ordering::Acquire)
}

/// Makes the key, once for the process.
pub(crate) fn prepare() -> Result<(), MallocError> {
    if KEY.load(Ordering::Acquire) != 0 {
        return Ok(());
    }
    if MAKING.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_ok() {
        let mut key: Key = 0;
        // SAFETY: `key` is a place for the result; the destructor is a plain `extern "C"` fn.
        let code = unsafe { pthread_key_create(&mut key, Some(ended)) };
        if code != 0 {
            MAKING.store(0, Ordering::Release);
            return Err(MallocError::Thread { call: "pthread_key_create", code });
        }
        KEY.store(key as usize + 1, Ordering::Release);
        MAKING.store(2, Ordering::Release);
        return Ok(());
    }
    blaze_spin::SpinWait::until("another thread making blaze-malloc's thread key", || {
        MAKING.load(Ordering::Acquire) != 1
    });
    if KEY.load(Ordering::Acquire) == 0 {
        return Err(MallocError::Thread { call: "pthread_key_create", code: -1 });
    }
    Ok(())
}

/// Makes `heap` the thread's, and its end the destructor's.
pub(crate) fn install(heap: &'static Heap) -> Result<(), MallocError> {
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64", not(miri))))]
    CURRENT.set(heap);
    let key = KEY.load(Ordering::Acquire) - 1;
    // SAFETY: a made key; the value is a static's address.
    let code = unsafe { pthread_setspecific(key as Key, ptr::from_ref(heap).cast()) };
    if code != 0 {
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64", not(miri))))]
        CURRENT.set(ptr::null());
        return Err(MallocError::Thread { call: "pthread_setspecific", code });
    }
    Ok(())
}

/// The key's destructor: the thread is ending with `value`, its heap. The system cleared the
/// key's slot before the call; this clears the thread-local word, so a free during the rest of
/// the thread's end goes to the heap's queue, and an allocation claims a heap again (which the
/// system destructs on its next round).
unsafe extern "C" fn ended(value: *mut c_void) {
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64", not(miri))))]
    CURRENT.set(ptr::null());
    if value.is_null() {
        return;
    }
    // SAFETY: the key's values are only `HEAPS` addresses (`install`).
    let heap: &'static Heap = unsafe { &*value.cast_const().cast::<Heap>() };
    heap::abandon(heap);
}
