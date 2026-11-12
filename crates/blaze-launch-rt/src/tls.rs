//! The program's thread-locals under the launcher.
//!
//! A linked program's thread-locals are the dynamic loader's: a block a thread, laid out at
//! link time. Under the launcher nothing is linked, so a thread-local static is a number
//! ([`Var::index`]) and each thread keeps a table of its own copies, made on first touch
//! from the static's initial bytes. Code reaches one through its record's pool
//! (`blaze_launch::place`): the pool holds [`thunk`] and the address of the static's
//! [`Var`], laid out as the platform's own descriptor is (Mach-O's thread-local variable
//! descriptor, ELF's TLS descriptor pair), so the code is the code a linked build runs.
//!
//! The thread's table hangs off one pthread key ([`key`]). Its destructor runs when the
//! thread ends, and frees the copies only on its third round: the program's own destructors
//! of thread-locals (std's, through `_tlv_atexit` on macOS, itself a key's destructor whose
//! order against this one nothing promises) may still read a copy in the rounds before.
//!
//! Built for AArch64. On x86-64 [`thunk`] is absent and a thread-local static is refused by
//! name when it is defined (`records`): nothing here has run on x86-64.

use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};

/// One thread-local static of the program: what [`get`] makes a thread's copy from. Lives
/// as long as the process (the records leak one a static).
#[repr(C)]
pub struct Var {
    /// The static's place in every thread's table.
    pub index: usize,
    pub size: usize,
    /// A power of two.
    pub align: usize,
    /// The initial bytes, `init_len` of them; the rest of `size` is zero.
    pub init: *const u8,
    pub init_len: usize,
}

/// A thread's copies, by [`Var::index`]: null until the thread first touches one.
struct Table {
    copies: Vec<(*mut u8, core::alloc::Layout)>,
    /// How many times the key's destructor has run for this thread.
    rounds: u8,
}

/// The launcher's pthread key plus one; 0 until [`key`] made it.
static KEY: AtomicUsize = AtomicUsize::new(0);

/// Makes the launcher's one pthread key, once. Called by the control thread before the first
/// thread-local static is defined, so before any thread can reach [`get`].
pub fn key() -> Result<libc::pthread_key_t, i32> {
    match KEY.load(Ordering::Acquire) {
        0 => {
            let mut key: libc::pthread_key_t = 0;
            // SAFETY: `key` is a live local the call fills; `ended` is a function of the
            // destructor's signature that lives as long as the process.
            let refused = unsafe { libc::pthread_key_create(&mut key, Some(ended)) };
            if refused != 0 {
                return Err(refused);
            }
            KEY.store(key as usize + 1, Ordering::Release);
            Ok(key)
        }
        made => Ok((made - 1) as libc::pthread_key_t),
    }
}

/// The key's destructor: `table` is the ending thread's [`Table`].
unsafe extern "C" fn ended(table: *mut c_void) {
    let table = table.cast::<Table>();
    // SAFETY: the key's value is only ever a `Table` this module boxed in `get`, owned by
    // the thread whose destructor this is; no other thread has it.
    let rounds = unsafe { &mut (*table).rounds };
    *rounds += 1;
    if *rounds < 3 {
        // Not yet: the program's own thread-local destructors may still run and read a
        // copy. Setting the value again has the system call this again in its next round
        // (POSIX: up to PTHREAD_DESTRUCTOR_ITERATIONS, at least 4).
        if let Ok(key) = key() {
            // SAFETY: the key is this module's; the value is the table this thread owns.
            unsafe { libc::pthread_setspecific(key, table.cast()) };
        }
        return;
    }
    // SAFETY: as above; the box was made by `get` and is dropped once, here.
    let table = unsafe { Box::from_raw(table) };
    for (copy, layout) in &table.copies {
        if !copy.is_null() {
            // SAFETY: allocated in `get` with this layout, freed once, as the thread ends.
            unsafe { std::alloc::dealloc(*copy, *layout) };
        }
    }
}

/// This thread's copy of the static `var` names: made, from the static's initial bytes, the
/// first time the thread asks. Called by [`thunk`], on a program thread.
///
/// # Safety
///
/// `var` is the address of a [`Var`] that lives as long as the process, whose `init` is
/// `init_len` readable bytes, `init_len <= size`, and [`key`] has been made.
unsafe extern "C" fn get(var: *const Var) -> *mut u8 {
    // SAFETY: the caller's contract.
    let var = unsafe { &*var };
    let key = (KEY.load(Ordering::Acquire) - 1) as libc::pthread_key_t;
    // SAFETY: the key was made (the caller's contract); reads this thread's value of it.
    let mut table = unsafe { libc::pthread_getspecific(key) }.cast::<Table>();
    if table.is_null() {
        // alloc: one table a thread that touches a thread-local, freed as the thread ends.
        table = Box::into_raw(Box::new(Table { copies: Vec::new(), rounds: 0 }));
        // SAFETY: the key is this module's; the value is a table this thread now owns.
        unsafe { libc::pthread_setspecific(key, table.cast()) };
    }
    // SAFETY: the table is this thread's own: no other thread has its address.
    let table = unsafe { &mut *table };
    if table.copies.len() <= var.index {
        let none = (core::ptr::null_mut(), core::alloc::Layout::new::<u8>());
        table.copies.resize(var.index + 1, none);
    }
    let (copy, _) = table.copies[var.index];
    if !copy.is_null() {
        return copy;
    }
    let Ok(layout) = core::alloc::Layout::from_size_align(var.size.max(1), var.align.max(1)) else {
        stop("a thread-local's size and alignment are no layout");
    };
    // SAFETY: `layout` has a size that is not zero.
    let copy = unsafe { std::alloc::alloc_zeroed(layout) };
    if copy.is_null() {
        stop("no memory for a thread's copy of a thread-local");
    }
    // SAFETY: `init` is `init_len` readable bytes and `copy` is `size >= init_len` writable
    // ones just allocated (the caller's contract), which cannot overlap.
    unsafe { core::ptr::copy_nonoverlapping(var.init, copy, var.init_len) };
    table.copies[var.index] = (copy, layout);
    copy
}

/// Stops the process: a thread-local could not be given, for `why`.
fn stop(why: &str) -> ! {
    use std::io::Write;

    // The process ends below whether or not its last words could be written.
    drop(writeln!(std::io::stderr().lock(), "blaze: {why}"));
    std::process::exit(101)
}

/// What a thread-local's descriptor in a record's pool names as its function: called with
/// `x0` the descriptor's address (its second word is the static's [`Var`]), it returns in
/// `x0` what the platform's own does, and changes no other register a caller may hold
/// (the platforms' own change `x0` alone, or `x0`, `x16` and `x17`):
///
/// - Mach-O (`tlv_get_addr`): the address of this thread's copy;
/// - ELF (a TLS descriptor's resolver): that address less the thread pointer, which the
///   caller adds back.
///
/// # Safety
///
/// Reached only by the call a thread-local access makes through its descriptor: `x0` is the
/// address of a pool's descriptor whose second word is a [`Var`] as [`get`] asks.
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
pub unsafe extern "C" fn thunk() {
    core::arch::naked_asm!(
        "stp x29, x30, [sp, #-16]!",
        "mov x29, sp",
        // x1 to x18 (144 bytes), q0 to q31 (512).
        "sub sp, sp, #656",
        "stp x1, x2, [sp, #0]",
        "stp x3, x4, [sp, #16]",
        "stp x5, x6, [sp, #32]",
        "stp x7, x8, [sp, #48]",
        "stp x9, x10, [sp, #64]",
        "stp x11, x12, [sp, #80]",
        "stp x13, x14, [sp, #96]",
        "stp x15, x16, [sp, #112]",
        "stp x17, x18, [sp, #128]",
        "stp q0, q1, [sp, #144]",
        "stp q2, q3, [sp, #176]",
        "stp q4, q5, [sp, #208]",
        "stp q6, q7, [sp, #240]",
        "stp q8, q9, [sp, #272]",
        "stp q10, q11, [sp, #304]",
        "stp q12, q13, [sp, #336]",
        "stp q14, q15, [sp, #368]",
        "stp q16, q17, [sp, #400]",
        "stp q18, q19, [sp, #432]",
        "stp q20, q21, [sp, #464]",
        "stp q22, q23, [sp, #496]",
        "stp q24, q25, [sp, #528]",
        "stp q26, q27, [sp, #560]",
        "stp q28, q29, [sp, #592]",
        "stp q30, q31, [sp, #624]",
        "ldr x0, [x0, #8]",
        "bl {get}",
        "ldp q30, q31, [sp, #624]",
        "ldp q28, q29, [sp, #592]",
        "ldp q26, q27, [sp, #560]",
        "ldp q24, q25, [sp, #528]",
        "ldp q22, q23, [sp, #496]",
        "ldp q20, q21, [sp, #464]",
        "ldp q18, q19, [sp, #432]",
        "ldp q16, q17, [sp, #400]",
        "ldp q14, q15, [sp, #368]",
        "ldp q12, q13, [sp, #336]",
        "ldp q10, q11, [sp, #304]",
        "ldp q8, q9, [sp, #272]",
        "ldp q6, q7, [sp, #240]",
        "ldp q4, q5, [sp, #208]",
        "ldp q2, q3, [sp, #176]",
        "ldp q0, q1, [sp, #144]",
        "ldp x17, x18, [sp, #128]",
        "ldp x15, x16, [sp, #112]",
        "ldp x13, x14, [sp, #96]",
        "ldp x11, x12, [sp, #80]",
        "ldp x9, x10, [sp, #64]",
        "ldp x7, x8, [sp, #48]",
        "ldp x5, x6, [sp, #32]",
        "ldp x3, x4, [sp, #16]",
        // On ELF the caller adds the thread pointer to what a descriptor's resolver gives.
        #[cfg(target_os = "linux")]
        "mrs x1, tpidr_el0",
        #[cfg(target_os = "linux")]
        "sub x0, x0, x1",
        "ldp x1, x2, [sp, #0]",
        "add sp, sp, #656",
        "ldp x29, x30, [sp], #16",
        "ret",
        get = sym get,
    )
}
