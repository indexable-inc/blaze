//! C's allocator is blaze's: `malloc`, `free`, `realloc` and the rest, from LLVM through its C
//! API and its `operator new` (libc++ and libstdc++ both allocate with `malloc`), zstd, flecs
//! and any other C or C++ linked in, go through the same counting allocator as Rust's
//! ([`blaze_graph::arena::Counting`] over [`Heaps`], mimalloc 3 with a heap per blaze-mem tag).
//! A block C allocates while a task runs is made in that task's tag's heap and counted to it,
//! and to the task's account, as a Rust block is; one allocated before `main` or outside any
//! task is the main heap's, counted outside any tag. Each is counted at its usable size as it
//! is made and again as it is freed ([`blaze_graph::arena::Malloc`]), and it is freed into its
//! heap as a Rust block is, so blaze-mem's give-back purges it the same way.
//!
//! - Linux: the executable defines the C allocator's symbols, which the dynamic linker binds
//!   every library's calls to before libc's, from the first instruction on. A block glibc's
//!   own allocator made (through `__libc_malloc`) is freed by it.
//! - macOS: a malloc zone, registered as a constructor runs and made the first zone, which
//!   libmalloc's `malloc` allocates from. A block made before (in the zone that was first then)
//!   is freed by its zone: libmalloc asks each zone's `size` which holds a block, and this one
//!   says 0 for any block mimalloc does not hold.
//!
//! [`check`] says whether the override is in place, and the program stops on it, naming why
//! (`program.rs`): there is no way to run with C's bytes uncounted.

#![allow(unsafe_code, reason = "the C allocator's entry points and libmalloc's zone, over mimalloc")]

use blaze_graph::arena::Counting;

use crate::heaps::Heaps;

/// The counting allocator C's calls go through: the global allocator's (`lib.rs`) under a second
/// name, counting into the same totals, tags and accounts.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
static C: Counting<Heaps> = Counting(Heaps);

/// Whether blaze's allocator serves C's `malloc` in this process: `Err` names what is missing
/// and why it matters. Called first thing in `main` (`program.rs`), whose process then stops.
pub fn check() -> Result<(), String> {
    platform::check()
}

/// Writes `said` and the address `at` to standard error, allocating nothing, and stops the
/// process: a free or size asked of a block no allocator holds, which would otherwise corrupt
/// one of them.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
#[cold]
fn stop(said: &str, at: *const std::ffi::c_void) -> ! {
    let mut hex = *b"0x0000000000000000";
    let mut addr = at.addr();
    for digit in hex[2..].iter_mut().rev() {
        *digit = b"0123456789abcdef"[addr & 0xf];
        addr >>= 4;
    }
    let mut err = std::io::stderr();
    _ = std::io::Write::write_all(&mut err, said.as_bytes());
    _ = std::io::Write::write_all(&mut err, b" (");
    _ = std::io::Write::write_all(&mut err, &hex);
    _ = std::io::Write::write_all(&mut err, b"; crates/blaze-engine/src/malloc.rs)\n");
    std::process::abort()
}

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
use other as platform;

/// Linux: the C allocator's symbols, defined here so the executable's come before libc's.
#[cfg(target_os = "linux")]
mod linux {
    use std::{
        ffi::{CStr, c_char, c_int, c_void},
        ptr,
    };

    use blaze_graph::arena::{Locals, Malloc as _};

    use super::{C, stop};
    use crate::heaps::Heaps;

    const ENOMEM: c_int = 12;
    const EINVAL: c_int = 22;

    unsafe extern "C" {
        fn __errno_location() -> *mut c_int;
        fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
        fn getpagesize() -> c_int;
        fn strdup(s: *const c_char) -> *mut c_char;
    }

    // glibc's own allocator, under the names it exports beside the ones blaze takes: what frees
    // a block it made.
    #[cfg(target_env = "gnu")]
    unsafe extern "C" {
        fn __libc_free(ptr: *mut c_void);
        fn __libc_realloc(ptr: *mut c_void, size: usize) -> *mut c_void;
    }

    /// Sets `errno` and gives C's null.
    fn failed(errno: c_int) -> *mut c_void {
        // SAFETY: this thread's errno, always there.
        unsafe { *__errno_location() = errno };
        ptr::null_mut()
    }

    /// A block of `size` bytes aligned to `align` (a power of two), counted to the running
    /// task's tag; null with `ENOMEM` where there is none.
    fn made(size: usize, align: usize, zeroed: bool) -> *mut c_void {
        // SAFETY: `align` is a power of two; Linux's thread-locals are static TLS, there from
        // the thread's first instruction.
        let block = unsafe { C.c_malloc(size, align, zeroed) };
        if block.is_null() { failed(ENOMEM) } else { block.cast() }
    }

    fn page() -> usize {
        // SAFETY: no argument.
        // A page is a positive number of bytes.
        unsafe { getpagesize() }.unsigned_abs() as usize
    }

    /// C's `malloc`.
    ///
    /// # Safety
    ///
    /// C's contract for `malloc`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn malloc(size: usize) -> *mut c_void {
        made(size, 1, false)
    }

    /// C's `calloc`.
    ///
    /// # Safety
    ///
    /// C's contract for `calloc`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn calloc(count: usize, size: usize) -> *mut c_void {
        match count.checked_mul(size) {
            Some(total) => made(total, 1, true),
            None => failed(ENOMEM),
        }
    }

    /// C's `realloc`. `realloc(p, 0)` keeps a block of its own, as mimalloc's does, where
    /// glibc's frees `p` and gives null; C17 leaves it to the allocator.
    ///
    /// # Safety
    ///
    /// C's contract for `realloc`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void {
        if ptr.is_null() {
            return made(size, 1, false);
        }
        if !Heaps.owns(ptr.cast::<u8>()) {
            // SAFETY: the caller says `ptr` is a live block of an allocator; not mimalloc's.
            return unsafe { foreign_realloc(ptr, size) };
        }
        // SAFETY: a live block of mimalloc's, the caller says.
        let block = unsafe { C.c_realloc(ptr.cast(), size, Locals::Live) };
        if block.is_null() { failed(ENOMEM) } else { block.cast() }
    }

    /// glibc's `reallocarray`.
    ///
    /// # Safety
    ///
    /// C's contract for `realloc`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn reallocarray(ptr: *mut c_void, count: usize, size: usize) -> *mut c_void {
        match count.checked_mul(size) {
            // SAFETY: the caller upholds `realloc`'s contract.
            Some(total) => unsafe { realloc(ptr, total) },
            None => failed(ENOMEM),
        }
    }

    /// C's `free`.
    ///
    /// # Safety
    ///
    /// C's contract for `free`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn free(ptr: *mut c_void) {
        if ptr.is_null() {
            return;
        }
        if Heaps.owns(ptr.cast::<u8>()) {
            // SAFETY: a live block of mimalloc's, the caller says, freed once.
            unsafe { C.c_free(ptr.cast(), None, Locals::Live) }
        } else {
            // SAFETY: the caller says `ptr` is a live block of an allocator; not mimalloc's.
            unsafe { foreign_free(ptr) }
        }
    }

    /// POSIX's `posix_memalign`.
    ///
    /// # Safety
    ///
    /// POSIX's contract for `posix_memalign`: `out` is writable.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn posix_memalign(out: *mut *mut c_void, align: usize, size: usize) -> c_int {
        if !align.is_power_of_two() || align % size_of::<*mut c_void>() != 0 {
            return EINVAL;
        }
        // SAFETY: a power of two.
        let block = unsafe { C.c_malloc(size, align, false) };
        if block.is_null() {
            return ENOMEM;
        }
        // SAFETY: the caller says `out` is writable.
        unsafe { *out = block.cast() };
        0
    }

    /// C11's `aligned_alloc`.
    ///
    /// # Safety
    ///
    /// C's contract for `aligned_alloc`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn aligned_alloc(align: usize, size: usize) -> *mut c_void {
        if !align.is_power_of_two() {
            return failed(EINVAL);
        }
        made(size, align, false)
    }

    /// glibc's `memalign`, which takes an alignment that is not a power of two as the next
    /// one that is.
    ///
    /// # Safety
    ///
    /// glibc's contract for `memalign`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn memalign(align: usize, size: usize) -> *mut c_void {
        match align.max(1).checked_next_power_of_two() {
            Some(align) => made(size, align, false),
            None => failed(EINVAL),
        }
    }

    /// `valloc`: aligned to a page.
    ///
    /// # Safety
    ///
    /// C's contract for `valloc`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn valloc(size: usize) -> *mut c_void {
        made(size, page(), false)
    }

    /// glibc's `pvalloc`: aligned to a page, whole pages.
    ///
    /// # Safety
    ///
    /// glibc's contract for `pvalloc`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn pvalloc(size: usize) -> *mut c_void {
        let page = page();
        match size.max(1).checked_next_multiple_of(page) {
            Some(whole) => made(whole, page, false),
            None => failed(ENOMEM),
        }
    }

    /// glibc's `malloc_usable_size`.
    ///
    /// # Safety
    ///
    /// `ptr` is null or a live block of C's allocator.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn malloc_usable_size(ptr: *mut c_void) -> usize {
        if ptr.is_null() {
            return 0;
        }
        if !Heaps.owns(ptr.cast::<u8>()) {
            stop("blaze: malloc_usable_size of a block mimalloc does not hold", ptr);
        }
        // SAFETY: a live block of mimalloc's.
        unsafe { Heaps.usable(ptr.cast()) }
    }

    /// Frees a block another allocator made: glibc's own (`__libc_malloc`), its owner.
    #[cfg(target_env = "gnu")]
    unsafe fn foreign_free(ptr: *mut c_void) {
        // SAFETY: the caller says `ptr` is a live block, and the only allocator in the process
        // besides mimalloc is glibc's, which stops on a pointer it does not hold.
        unsafe { __libc_free(ptr) }
    }

    #[cfg(target_env = "gnu")]
    unsafe fn foreign_realloc(ptr: *mut c_void, size: usize) -> *mut c_void {
        // SAFETY: as `foreign_free`.
        unsafe { __libc_realloc(ptr, size) }
    }

    #[cfg(not(target_env = "gnu"))]
    unsafe fn foreign_free(ptr: *mut c_void) {
        stop("blaze: free of a block mimalloc does not hold, and this libc names no allocator of its own to give it to", ptr)
    }

    #[cfg(not(target_env = "gnu"))]
    unsafe fn foreign_realloc(ptr: *mut c_void, _: usize) -> *mut c_void {
        stop("blaze: realloc of a block mimalloc does not hold, and this libc names no allocator of its own to give it to", ptr)
    }

    /// Every symbol of C's allocator this module defines, and its address here.
    fn ours() -> [(&'static CStr, *const ()); 11] {
        [
            (c"malloc", malloc as *const ()),
            (c"calloc", calloc as *const ()),
            (c"realloc", realloc as *const ()),
            (c"reallocarray", reallocarray as *const ()),
            (c"free", free as *const ()),
            (c"posix_memalign", posix_memalign as *const ()),
            (c"aligned_alloc", aligned_alloc as *const ()),
            (c"memalign", memalign as *const ()),
            (c"valloc", valloc as *const ()),
            (c"pvalloc", pvalloc as *const ()),
            (c"malloc_usable_size", malloc_usable_size as *const ()),
        ]
    }

    /// The dynamic linker binds each of [`ours`] to the definition here, not libc's (the
    /// executable must export them: a linker exports a symbol a shared library it links also
    /// defines), and a block libc's `strdup` makes is mimalloc's.
    pub(super) fn check() -> Result<(), String> {
        for (name, here) in ours() {
            // SAFETY: `RTLD_DEFAULT` (null on glibc and musl) and a NUL-terminated name.
            let bound = unsafe { dlsym(ptr::null_mut(), name.as_ptr()) };
            if bound.cast::<()>().cast_const() != here {
                return Err(format!(
                    "blaze's C allocator is not this process's: the dynamic linker binds `{}` to {bound:?}, not blaze's at {here:?} (crates/blaze-engine/src/malloc.rs), so C and C++ allocations (LLVM's) would go uncounted; the executable must define and export it",
                    name.to_string_lossy(),
                ));
            }
        }
        // SAFETY: a NUL-terminated string; the copy is freed once below.
        let copy = unsafe { strdup(c"blaze".as_ptr()) };
        if copy.is_null() {
            return Err("libc's strdup gave no block at start (crates/blaze-engine/src/malloc.rs)".to_owned()); // alloc: the error said once
        }
        let held = Heaps.owns(copy.cast_const().cast());
        // SAFETY: the block strdup made, freed once.
        unsafe { free(copy.cast()) };
        match held {
            true => Ok(()),
            false => Err("libc's strdup made a block mimalloc does not hold: libc's calls to malloc do not reach blaze's (crates/blaze-engine/src/malloc.rs), so C's allocations would go uncounted".to_owned()), // alloc: the error said once
        }
    }

    #[cfg(test)]
    mod tests {
        /// Every symbol blaze defines for C is the one the dynamic linker binds.
        #[test]
        fn the_dynamic_linker_binds_c_allocations_to_blaze() {
            super::check().expect("blaze's C allocator is the process's");
        }
    }
}

/// macOS: a malloc zone over mimalloc, made the first zone, so libmalloc's `malloc` (and
/// libc++'s `operator new` on it, and CoreFoundation's default allocator) allocates from it.
///
/// libmalloc's API here is public (`<malloc/malloc.h>`), but two things it relies on are not:
/// - Which zone `malloc` allocates from is the first registered, and `malloc_zone_unregister`
///   moves the last zone into the slot it empties: registering this zone (last) and then
///   unregistering and registering again the first makes this one first. Undocumented
///   behaviour, the way mimalloc's own zone (`alloc-override-zone.c`) and jemalloc's take over;
///   the public way, a zone of one's own passed to `malloc_zone_malloc`, would leave every
///   `malloc` of C's uncounted. [`install`] checks that it worked.
/// - dyld's thread-local variables (rule macos-private-apis): dyld makes a thread's block of an
///   image's thread-locals with `malloc` the first time the thread touches one, and frees it
///   with `free` as the thread ends, keeping it under one `pthread` key per image, which each of
///   the image's descriptors in its `__thread_vars` section names (dyld's `TLVDescriptor`:
///   thunk, key, offset). Counting a block touches this image's thread-locals, so a `malloc`
///   that came from dyld making them would make them again, through itself. This zone reads the
///   key from the section once ([`thread_vars_key`]) and asks `pthread_getspecific` of it on
///   each call: while it is null, a block comes from the zone that was first before this one
///   (dyld's own block among them, a few KiB per thread, uncounted), and one of mimalloc's is
///   freed counting straight to the totals ([`Locals::Gone`]). No public interface says whether
///   a thread's thread-locals are there. [`tests`] resolves each symbol, and checks that the key
///   is null on a new thread until it touches a thread-local and not after.
#[cfg(target_os = "macos")]
mod macos {
    use std::{
        cell::UnsafeCell,
        ffi::{CStr, c_char, c_int, c_uint, c_ulong, c_void},
        ptr,
        sync::atomic::{AtomicPtr, AtomicU8, AtomicUsize, Ordering},
    };

    use blaze_graph::arena::{Locals, Malloc as _};

    use super::{C, stop};
    use crate::heaps::Heaps;

    /// `malloc_zone_t` through version 10 (`claimed_address`), the fields libmalloc reads of a
    /// zone of that version.
    #[repr(C)]
    struct Zone {
        reserved1: *mut c_void,
        reserved2: *mut c_void,
        size: unsafe extern "C" fn(*mut Zone, *const c_void) -> usize,
        malloc: unsafe extern "C" fn(*mut Zone, usize) -> *mut c_void,
        calloc: unsafe extern "C" fn(*mut Zone, usize, usize) -> *mut c_void,
        valloc: unsafe extern "C" fn(*mut Zone, usize) -> *mut c_void,
        free: unsafe extern "C" fn(*mut Zone, *mut c_void),
        realloc: unsafe extern "C" fn(*mut Zone, *mut c_void, usize) -> *mut c_void,
        destroy: unsafe extern "C" fn(*mut Zone),
        zone_name: *const c_char,
        batch_malloc: unsafe extern "C" fn(*mut Zone, usize, *mut *mut c_void, c_uint) -> c_uint,
        batch_free: unsafe extern "C" fn(*mut Zone, *mut *mut c_void, c_uint),
        introspect: *const Introspection,
        version: c_uint,
        memalign: unsafe extern "C" fn(*mut Zone, usize, usize) -> *mut c_void,
        free_definite_size: unsafe extern "C" fn(*mut Zone, *mut c_void, usize),
        pressure_relief: unsafe extern "C" fn(*mut Zone, usize) -> usize,
        claimed_address: unsafe extern "C" fn(*mut Zone, *mut c_void) -> c_int,
    }

    /// `malloc_introspection_t`, every field through version 12's, each a no-op where there is
    /// nothing to do: libmalloc calls `force_lock`, `force_unlock` and `reinit_lock` of every
    /// zone around a `fork`, and tools (`leaks`, `heap`) the rest.
    #[repr(C)]
    struct Introspection {
        enumerator: unsafe extern "C" fn(c_uint, *mut c_void, c_uint, usize, *const c_void, *const c_void) -> c_int,
        good_size: unsafe extern "C" fn(*mut Zone, usize) -> usize,
        check: unsafe extern "C" fn(*mut Zone) -> c_int,
        print: unsafe extern "C" fn(*mut Zone, c_int),
        log: unsafe extern "C" fn(*mut Zone, *mut c_void),
        force_lock: unsafe extern "C" fn(*mut Zone),
        force_unlock: unsafe extern "C" fn(*mut Zone),
        statistics: unsafe extern "C" fn(*mut Zone, *mut Statistics),
        zone_locked: unsafe extern "C" fn(*mut Zone) -> c_int,
        enable_discharge_checking: unsafe extern "C" fn(*mut Zone) -> c_int,
        disable_discharge_checking: unsafe extern "C" fn(*mut Zone),
        discharge: unsafe extern "C" fn(*mut Zone, *mut c_void),
        /// A block (`^(void *, void *)`) in C; none.
        enumerate_discharged_pointers: usize,
        reinit_lock: unsafe extern "C" fn(*mut Zone),
        print_task: unsafe extern "C" fn(c_uint, c_uint, usize, *const c_void, *const c_void),
        task_statistics: unsafe extern "C" fn(c_uint, usize, *const c_void, *mut Statistics),
    }

    /// `malloc_statistics_t`.
    #[repr(C)]
    struct Statistics {
        blocks_in_use: c_uint,
        size_in_use: usize,
        max_size_in_use: usize,
        size_allocated: usize,
    }

    /// `Dl_info`.
    #[repr(C)]
    struct DlInfo {
        fname: *const c_char,
        fbase: *mut c_void,
        sname: *const c_char,
        saddr: *mut c_void,
    }

    /// dyld's `TLVDescriptor` (`threadLocalVariables.c`): one per thread-local variable in an
    /// image's `__thread_vars`, its `key` the image's `pthread` key, set as dyld loads it.
    #[repr(C)]
    struct Descriptor {
        thunk: usize,
        key: usize,
        offset: usize,
    }

    unsafe extern "C" {
        fn malloc_zone_register(zone: *mut Zone);
        fn malloc_zone_unregister(zone: *mut Zone);
        fn malloc_get_all_zones(
            task: c_uint,
            reader: *const c_void,
            addresses: *mut *mut usize,
            count: *mut c_uint,
        ) -> c_int;
        fn malloc_default_purgeable_zone() -> *mut Zone;
        fn malloc_zone_from_ptr(ptr: *const c_void) -> *mut Zone;
        fn malloc_zone_malloc(zone: *mut Zone, size: usize) -> *mut c_void;
        fn malloc_zone_calloc(zone: *mut Zone, count: usize, size: usize) -> *mut c_void;
        fn malloc_zone_memalign(zone: *mut Zone, align: usize, size: usize) -> *mut c_void;
        fn malloc_zone_free(zone: *mut Zone, ptr: *mut c_void);
        fn malloc_zone_realloc(zone: *mut Zone, ptr: *mut c_void, size: usize) -> *mut c_void;
        fn malloc(size: usize) -> *mut c_void;
        fn free(ptr: *mut c_void);
        fn pthread_getspecific(key: usize) -> *mut c_void;
        fn dladdr(addr: *const c_void, info: *mut DlInfo) -> c_int;
        fn getsectiondata(
            header: *const c_void,
            segment: *const c_char,
            section: *const c_char,
            size: *mut c_ulong,
        ) -> *mut u8;
        fn getpagesize() -> c_int;
        static mach_task_self_: c_uint;
    }

    /// The most alignment `malloc_zone_malloc` gives without asking.
    const WORD: usize = align_of::<usize>();

    /// The zone, writable (libmalloc's `malloc_set_zone_name` writes its name) and shared.
    struct Shared(UnsafeCell<Zone>);
    // SAFETY: libmalloc reads the zone from any thread; nothing in blaze writes it after
    // [`install`], and the function pointers in it are for any thread.
    unsafe impl Sync for Shared {}

    static INTROSPECT: Introspection = Introspection {
        enumerator,
        good_size,
        check: true_of,
        print: print_of,
        log: log_of,
        force_lock: nothing,
        force_unlock: nothing,
        statistics,
        zone_locked: false_of,
        enable_discharge_checking: false_of,
        disable_discharge_checking: nothing,
        discharge: log_of,
        enumerate_discharged_pointers: 0,
        reinit_lock: nothing,
        print_task,
        task_statistics,
    };

    static ZONE: Shared = Shared(UnsafeCell::new(Zone {
        reserved1: ptr::null_mut(),
        reserved2: ptr::null_mut(),
        size,
        malloc: zone_malloc,
        calloc: zone_calloc,
        valloc: zone_valloc,
        free: zone_free,
        realloc: zone_realloc,
        destroy: nothing,
        zone_name: c"blaze".as_ptr(),
        batch_malloc,
        batch_free,
        introspect: &raw const INTROSPECT,
        version: 10,
        memalign: zone_memalign,
        free_definite_size,
        pressure_relief,
        claimed_address,
    }));

    fn ours() -> *mut Zone {
        ZONE.0.get()
    }

    /// The zone that was first before this one: where a block made while the thread's
    /// thread-locals are not there comes from.
    static SYSTEM: AtomicPtr<Zone> = AtomicPtr::new(ptr::null_mut());

    /// This image's thread-locals' `pthread` key ([`thread_vars_key`]).
    static TLV_KEY: AtomicUsize = AtomicUsize::new(0);

    /// Where a block made or freed on this thread now is counted: [`Locals::Gone`] while dyld
    /// has not made this thread's thread-locals, or has freed them as the thread ends. One
    /// `pthread_getspecific`, a load from the thread's own slots.
    #[inline]
    fn locals() -> Locals {
        // SAFETY: a key dyld made; reads this thread's slot for it and allocates nothing.
        match unsafe { pthread_getspecific(TLV_KEY.load(Ordering::Relaxed)) }.is_null() {
            true => Locals::Gone,
            false => Locals::Live,
        }
    }

    /// A block for C: mimalloc's, counted to the running task's tag, or while the thread's
    /// thread-locals are not there the first zone's before this one, uncounted.
    fn made(size: usize, align: usize, zeroed: bool) -> *mut c_void {
        match locals() {
            // SAFETY: `align` is a power of two; the thread-locals are there.
            Locals::Live => unsafe { C.c_malloc(size, align, zeroed) }.cast(),
            Locals::Gone => {
                let system = SYSTEM.load(Ordering::Relaxed);
                // SAFETY: a zone libmalloc registered, which lives for the process.
                unsafe {
                    match (align <= WORD, zeroed) {
                        (true, false) => malloc_zone_malloc(system, size),
                        (true, true) => malloc_zone_calloc(system, 1, size),
                        // No zeroed block aligned past a word is asked of C.
                        (false, _) => malloc_zone_memalign(system, align, size),
                    }
                }
            }
        }
    }

    unsafe extern "C" fn size(_: *mut Zone, ptr: *const c_void) -> usize {
        match Heaps.owns(ptr.cast::<u8>()) {
            // SAFETY: mimalloc holds it; libmalloc asks only of a live block.
            true => unsafe { Heaps.usable(ptr.cast_mut().cast()) },
            false => 0,
        }
    }

    unsafe extern "C" fn zone_malloc(_: *mut Zone, size: usize) -> *mut c_void {
        made(size, 1, false)
    }

    unsafe extern "C" fn zone_calloc(_: *mut Zone, count: usize, size: usize) -> *mut c_void {
        match count.checked_mul(size) {
            Some(total) => made(total, 1, true),
            None => ptr::null_mut(),
        }
    }

    unsafe extern "C" fn zone_valloc(_: *mut Zone, size: usize) -> *mut c_void {
        // SAFETY: no argument. A page is a positive number of bytes.
        made(size, unsafe { getpagesize() }.unsigned_abs() as usize, false)
    }

    unsafe extern "C" fn zone_memalign(_: *mut Zone, align: usize, size: usize) -> *mut c_void {
        // libmalloc's `posix_memalign` and `aligned_alloc` refuse an alignment that is not a
        // power of two before they ask the zone; `malloc_zone_memalign` itself does not.
        match align.is_power_of_two() {
            true => made(size, align, false),
            false => ptr::null_mut(),
        }
    }

    /// Frees `ptr`: mimalloc's into its heap, counted; another zone's (a block made before
    /// this zone was first, given to `malloc_zone_free` of the default zone, which sends it
    /// here) to that zone.
    unsafe extern "C" fn zone_free(_: *mut Zone, ptr: *mut c_void) {
        if ptr.is_null() {
            return;
        }
        if Heaps.owns(ptr.cast::<u8>()) {
            // SAFETY: a live block of mimalloc's, freed once.
            unsafe { C.c_free(ptr.cast(), None, locals()) };
            return;
        }
        // SAFETY: any address; this zone's `size` says 0 for it.
        let owner = unsafe { malloc_zone_from_ptr(ptr) };
        if owner.is_null() || owner == ours() {
            stop("blaze: free of a block no malloc zone holds", ptr);
        }
        // SAFETY: `owner` holds the block.
        unsafe { malloc_zone_free(owner, ptr) }
    }

    /// The size libmalloc passes is this zone's `size` of the block, which `free` reads again
    /// rather than trust a caller's.
    unsafe extern "C" fn free_definite_size(zone: *mut Zone, ptr: *mut c_void, _: usize) {
        // SAFETY: as `zone_free`.
        unsafe { zone_free(zone, ptr) }
    }

    unsafe extern "C" fn zone_realloc(_: *mut Zone, ptr: *mut c_void, size: usize) -> *mut c_void {
        if ptr.is_null() {
            return made(size, 1, false);
        }
        if Heaps.owns(ptr.cast::<u8>()) {
            // SAFETY: a live block of mimalloc's.
            return unsafe { C.c_realloc(ptr.cast(), size, locals()) }.cast();
        }
        // SAFETY: any address; this zone's `size` says 0 for it.
        let owner = unsafe { malloc_zone_from_ptr(ptr) };
        if owner.is_null() || owner == ours() {
            stop("blaze: realloc of a block no malloc zone holds", ptr);
        }
        // SAFETY: `owner` holds the block.
        unsafe { malloc_zone_realloc(owner, ptr, size) }
    }

    unsafe extern "C" fn batch_malloc(
        zone: *mut Zone,
        size: usize,
        results: *mut *mut c_void,
        count: c_uint,
    ) -> c_uint {
        for at in 0..count {
            // SAFETY: as `zone_malloc`.
            let block = unsafe { zone_malloc(zone, size) };
            if block.is_null() {
                return at;
            }
            // SAFETY: libmalloc's caller gives room for `count` results.
            unsafe { *results.add(at as usize) = block };
        }
        count
    }

    unsafe extern "C" fn batch_free(zone: *mut Zone, blocks: *mut *mut c_void, count: c_uint) {
        for at in 0..count as usize {
            // SAFETY: `count` live blocks, each freed once.
            unsafe {
                zone_free(zone, *blocks.add(at));
                *blocks.add(at) = ptr::null_mut();
            }
        }
    }

    /// The system's memory pressure: blaze-mem's give-back, the same purge as the daemon's,
    /// where this thread's thread-locals are there to take its part.
    unsafe extern "C" fn pressure_relief(_: *mut Zone, _: usize) -> usize {
        if locals() == Locals::Live {
            blaze_mem::tag::give_back_heap();
        }
        0
    }

    unsafe extern "C" fn claimed_address(_: *mut Zone, ptr: *mut c_void) -> c_int {
        c_int::from(Heaps.owns(ptr.cast::<u8>()))
    }

    unsafe extern "C" fn enumerator(
        _: c_uint,
        _: *mut c_void,
        _: c_uint,
        _: usize,
        _: *const c_void,
        _: *const c_void,
    ) -> c_int {
        0
    }

    unsafe extern "C" fn good_size(_: *mut Zone, size: usize) -> usize {
        // SAFETY: arithmetic on a size.
        unsafe { libmimalloc_sys::mi_good_size(size) }
    }

    unsafe extern "C" fn true_of(_: *mut Zone) -> c_int {
        1
    }

    unsafe extern "C" fn false_of(_: *mut Zone) -> c_int {
        0
    }

    unsafe extern "C" fn nothing(_: *mut Zone) {}

    unsafe extern "C" fn print_of(_: *mut Zone, _: c_int) {}

    unsafe extern "C" fn log_of(_: *mut Zone, _: *mut c_void) {}

    unsafe extern "C" fn statistics(_: *mut Zone, stats: *mut Statistics) {
        // SAFETY: libmalloc's caller gives a writable record. What mimalloc holds is in the
        // memory line, not here.
        unsafe { stats.write(Statistics { blocks_in_use: 0, size_in_use: 0, max_size_in_use: 0, size_allocated: 0 }) }
    }

    unsafe extern "C" fn print_task(_: c_uint, _: c_uint, _: usize, _: *const c_void, _: *const c_void) {}

    unsafe extern "C" fn task_statistics(_: c_uint, _: usize, _: *const c_void, stats: *mut Statistics) {
        // SAFETY: as `statistics`.
        unsafe { statistics(ptr::null_mut(), stats) }
    }

    /// How [`install`] went, read by [`check`].
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    #[repr(u8)]
    pub(super) enum Outcome {
        NotRun = 0,
        Installed,
        NoImage,
        NoThreadVars,
        KeysDiffer,
        KeyNotLive,
        NoZones,
        NotFirst,
        NotOurs,
    }

    static STATE: AtomicU8 = AtomicU8::new(Outcome::NotRun as u8);

    /// Installs the zone as a constructor runs, before `main` and any thread of blaze's; C++
    /// static constructors run before or after it by link order, and a block one made before
    /// is freed by the zone that made it.
    #[used]
    #[unsafe(link_section = "__DATA,__mod_init_func")]
    static INSTALL: unsafe extern "C" fn() = install;

    unsafe extern "C" fn install() {
        // SAFETY: one thread, before `main`.
        let outcome = unsafe { try_install() };
        STATE.store(outcome as u8, Ordering::Release);
    }

    /// The key every thread-local of this image is kept under: the one key every descriptor
    /// in its `__thread_vars` names (dyld's private layout; see the module).
    pub(super) fn thread_vars_key() -> Result<usize, Outcome> {
        let mut info = DlInfo {
            fname: ptr::null(),
            fbase: ptr::null_mut(),
            sname: ptr::null(),
            saddr: ptr::null_mut(),
        };
        // SAFETY: an address of this image's code; `info` is ours to fill.
        if unsafe { dladdr((install as *const ()).cast(), &raw mut info) } == 0 || info.fbase.is_null() {
            return Err(Outcome::NoImage);
        }
        let mut bytes: c_ulong = 0;
        // SAFETY: the image's Mach-O header (`dli_fbase`) and NUL-terminated names;
        // `getsectiondata` adds the image's slide.
        let vars = unsafe {
            getsectiondata(info.fbase, c"__DATA".as_ptr(), c"__thread_vars".as_ptr(), &raw mut bytes)
        };
        let count = bytes as usize / size_of::<Descriptor>();
        if vars.is_null() || count == 0 {
            return Err(Outcome::NoThreadVars);
        }
        // SAFETY: the section is `count` descriptors, which dyld wrote as it loaded the image
        // and nothing writes after.
        let descriptors = unsafe { std::slice::from_raw_parts(vars.cast::<Descriptor>(), count) };
        let key = descriptors[0].key;
        match descriptors.iter().all(|it| it.key == key) {
            true => Ok(key),
            false => Err(Outcome::KeysDiffer),
        }
    }

    /// The first zone, the one `malloc` allocates from: null where libmalloc lists none.
    fn first() -> *mut Zone {
        let mut zones: *mut usize = ptr::null_mut();
        let mut count: c_uint = 0;
        // SAFETY: this task's zones, read in place (no reader); both out-pointers ours.
        let listed = unsafe {
            malloc_get_all_zones(mach_task_self_, ptr::null(), &raw mut zones, &raw mut count)
        };
        if listed != 0 || count == 0 || zones.is_null() {
            return ptr::null_mut();
        }
        // SAFETY: `count` zones, at least one.
        ptr::with_exposed_provenance_mut(unsafe { *zones })
    }

    unsafe fn try_install() -> Outcome {
        let key = match thread_vars_key() {
            Ok(key) => key,
            Err(outcome) => return outcome,
        };
        // A thread-local of this image's, touched: dyld makes this thread's block now, from the
        // system's zone, this one not yet registered.
        std::hint::black_box(blaze_graph::arena::held());
        // SAFETY: a key dyld made.
        if unsafe { pthread_getspecific(key) }.is_null() {
            return Outcome::KeyNotLive;
        }
        TLV_KEY.store(key, Ordering::Relaxed);
        // The real first zone, not `malloc_default_zone()`, a stand-in that sends each call to
        // whichever zone is first: this one, once registered.
        let system = first();
        if system.is_null() {
            return Outcome::NoZones;
        }
        SYSTEM.store(system, Ordering::Relaxed);
        // SAFETY: libmalloc's own zones and ours, which lives for the process. The purgeable
        // zone is made now and registered again last, as mimalloc does, so it never takes
        // the first slot from this one.
        unsafe {
            let purgeable = malloc_default_purgeable_zone();
            malloc_zone_register(ours());
            // Bounded: each turn moves this zone a slot nearer the front, where the first is
            // unregistered and the last (this one) takes its slot.
            for _ in 0..64 {
                let first = first();
                if first == ours() || first.is_null() {
                    break;
                }
                malloc_zone_unregister(first);
                malloc_zone_register(first);
            }
            if !purgeable.is_null() && purgeable != ours() {
                malloc_zone_unregister(purgeable);
                malloc_zone_register(purgeable);
            }
        }
        if first() != ours() {
            return Outcome::NotFirst;
        }
        // SAFETY: C's `malloc` and `free`, now this zone's.
        let block = unsafe { malloc(64) };
        let held = !block.is_null() && Heaps.owns(block.cast::<u8>());
        // SAFETY: the block just made, freed once.
        unsafe { free(block) };
        match held {
            true => Outcome::Installed,
            false => Outcome::NotOurs,
        }
    }

    pub(super) fn check() -> Result<(), String> {
        // Names the constructor, so the linker keeps the object it is in.
        std::hint::black_box(&INSTALL);
        let outcome = match STATE.load(Ordering::Acquire) {
            1 => Outcome::Installed,
            2 => Outcome::NoImage,
            3 => Outcome::NoThreadVars,
            4 => Outcome::KeysDiffer,
            5 => Outcome::KeyNotLive,
            6 => Outcome::NoZones,
            7 => Outcome::NotFirst,
            8 => Outcome::NotOurs,
            _ => Outcome::NotRun,
        };
        let why = match outcome {
            Outcome::Installed if first() == ours() => return Ok(()),
            Outcome::Installed => "another malloc zone was made first after blaze's was",
            Outcome::NotRun => "its constructor never ran: the object holding it was not linked, or not as a constructor (__DATA,__mod_init_func)",
            Outcome::NoImage => "dladdr names no image for blaze's own code",
            Outcome::NoThreadVars => "the image has no __DATA,__thread_vars section, where dyld keeps the key of its thread-locals",
            Outcome::KeysDiffer => "the descriptors in __thread_vars name more than one key: dyld's thread-local layout changed",
            Outcome::KeyNotLive => "the key read from __thread_vars holds nothing for a thread that touched a thread-local: dyld's thread-local layout changed",
            Outcome::NoZones => "malloc_get_all_zones lists no zone",
            Outcome::NotFirst => "registering it and the first zone again did not make it first (libmalloc's zone order changed)",
            Outcome::NotOurs => "a malloc after it was made first gave a block mimalloc does not hold",
        };
        Err(format!(
            "blaze's malloc zone is not this process's (crates/blaze-engine/src/malloc.rs): {why}, so C and C++ allocations (LLVM's) would go uncounted"
        ))
    }

    #[cfg(test)]
    mod tests {
        use std::ffi::{CStr, c_void};

        use super::*;

        unsafe extern "C" {
            fn dlsym(handle: *mut c_void, name: *const std::ffi::c_char) -> *mut c_void;
            fn pthread_create(
                thread: *mut usize,
                attr: *const c_void,
                start: extern "C" fn(*mut c_void) -> *mut c_void,
                arg: *mut c_void,
            ) -> c_int;
            fn pthread_join(thread: usize, out: *mut *mut c_void) -> c_int;
        }

        /// `RTLD_DEFAULT`.
        const DEFAULT: *mut c_void = ptr::without_provenance_mut(usize::MAX - 1);

        /// Each symbol of libmalloc's, dyld's and libSystem's the zone calls resolves (rule
        /// macos-private-apis): an OS without one fails here, naming it.
        #[test]
        fn every_symbol_the_zone_calls_resolves() {
            let names: [&CStr; 14] = [
                c"malloc_zone_register",
                c"malloc_zone_unregister",
                c"malloc_get_all_zones",
                c"malloc_default_purgeable_zone",
                c"malloc_zone_from_ptr",
                c"malloc_zone_malloc",
                c"malloc_zone_calloc",
                c"malloc_zone_memalign",
                c"malloc_zone_free",
                c"malloc_zone_realloc",
                c"pthread_getspecific",
                c"dladdr",
                c"getsectiondata",
                c"mach_task_self_",
            ];
            for name in names {
                // SAFETY: a NUL-terminated name, looked up everywhere.
                let found = unsafe { dlsym(DEFAULT, name.as_ptr()) };
                assert!(!found.is_null(), "{name:?} is gone from this macOS");
            }
            super::super::check().expect("the zone is first");
        }

        /// What a thread new to this image's thread-locals saw: its slot for the key before it
        /// touched one, and after.
        extern "C" fn touch(key: *mut c_void) -> *mut c_void {
            let key = key.addr();
            // SAFETY: a key dyld made; this thread touched no thread-local of this image yet
            // (a raw pthread, which no Rust start code ran on).
            let before = unsafe { pthread_getspecific(key) }.is_null();
            std::hint::black_box(blaze_graph::arena::held());
            // SAFETY: as above.
            let after = unsafe { pthread_getspecific(key) }.is_null();
            ptr::without_provenance_mut(usize::from(before) | usize::from(after) << 1)
        }

        /// dyld's private thread-local layout, as the zone reads it: one key for the image,
        /// nothing under it on a thread until the thread touches a thread-local, and its block
        /// after. If dyld changes it, this fails before the zone counts into another's memory.
        #[test]
        fn the_thread_vars_key_is_null_until_a_thread_touches_a_thread_local() {
            let key = thread_vars_key().expect("this image's __thread_vars name one key");
            let mut thread = 0usize;
            let mut seen: *mut c_void = ptr::null_mut();
            // SAFETY: a thread of `touch`, joined below; the key passed as its argument.
            unsafe {
                assert_eq!(pthread_create(&raw mut thread, ptr::null(), touch, ptr::without_provenance_mut(key)), 0);
                assert_eq!(pthread_join(thread, &raw mut seen), 0);
            }
            assert_eq!(seen.addr() & 1, 1, "a new thread's slot for the key is empty before it touches a thread-local");
            assert_eq!(seen.addr() & 2, 0, "and holds its block after");
        }
    }
}

/// Neither Linux nor macOS: C's allocations are the system's, outside blaze's count, as no
/// override for this target is written yet.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod other {
    pub(super) fn check() -> Result<(), String> {
        Ok(())
    }
}
