//! The program's own threads, told to the core.
//!
//! The core frees a replaced body only once every thread alive when it was unlinked has
//! reported or ended (`blaze_launch::launch`), so it must know of every thread that can run
//! the program's code. A thread the program starts is started through [`create`], which the
//! records bind the program's `pthread_create` to (`records::own`): the parent announces the
//! birth (`Shared::birth`, which loads the epoch first) and only then starts the thread, and
//! the new thread begins as a [`Mutator`] before it runs a byte of the program.
//!
//! When a thread is said to have ended: by a pthread key's destructor, on the last round the
//! system gives destructors (POSIX: at least four), re-armed until then. The program's own
//! thread-local destructors run in such rounds too and are the program's code, so the thread
//! is not "in no code any more" until they are done; a destructor that runs in the last
//! round after this one is the gap, and is not proven closed. A thread that ends without
//! running destructors (`pthread_exit` from a signal handler is the textbook case) is never
//! said to have ended and holds every batch from then on: memory, not safety.
//!
//! A thread nothing here started (one a C library spawns, which then calls back into the
//! program) is not known and is not waited for: not built (`Launch::thread_found` is the
//! core's side of it).

use core::ffi::c_void;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::sync::Arc;

use crate::table::{Birth, Mutator, Shared};

/// The launcher's `Shared`, leaked by [`adopt`]: what [`create`] announces births to.
static SHARED: AtomicPtr<Shared> = AtomicPtr::new(core::ptr::null_mut());

/// The key whose destructor says a thread ended, plus one; 0 until [`adopt`] made it.
static KEY: AtomicUsize = AtomicUsize::new(0);

/// A thread's [`Mutator`] and how many destructor rounds have passed.
struct Ending {
    me: Mutator,
    rounds: u8,
}

/// The rounds a thread's end waits through: the program's destructors run in the earlier.
const ROUNDS: u8 = 4;

/// Makes this process's launcher the one [`create`] tells of new threads, and gives the
/// calling thread (the program's main thread) its [`Mutator`], which it keeps until the
/// process ends. Called once, before any of the program's code runs.
pub fn adopt(shared: &Arc<Shared>) -> Result<Mutator, i32> {
    let mut key: libc::pthread_key_t = 0;
    // SAFETY: `key` is a live local the call fills; `ended` has the destructor's signature
    // and lives as long as the process.
    let refused = unsafe { libc::pthread_key_create(&mut key, Some(ended)) };
    if refused != 0 {
        return Err(refused);
    }
    KEY.store(key as usize + 1, Ordering::Release);
    // One count of the `Arc` is leaked: threads announce to it for the life of the process.
    SHARED.store(Arc::into_raw(Arc::clone(shared)).cast_mut(), Ordering::Release);
    Ok(shared.birth().begin())
}

/// The key's destructor: `ending` is the ending thread's [`Ending`].
unsafe extern "C" fn ended(ending: *mut c_void) {
    let ending = ending.cast::<Ending>();
    // SAFETY: the key's value is only ever an `Ending` boxed in `run`, owned by the thread
    // whose destructor this is.
    let rounds = unsafe { &mut (*ending).rounds };
    *rounds += 1;
    if *rounds < ROUNDS {
        let key = (KEY.load(Ordering::Acquire) - 1) as libc::pthread_key_t;
        // SAFETY: the key is this module's; the value is this thread's own `Ending`. Set
        // again, the system calls this destructor again in its next round.
        if unsafe { libc::pthread_setspecific(key, ending.cast()) } == 0 {
            return;
        }
    }
    // SAFETY: boxed in `run` and dropped once, here: the `Mutator`'s drop says the thread
    // ended.
    drop(unsafe { Box::from_raw(ending) });
}

/// What a new thread is started with.
struct Born {
    birth: Birth,
    start: extern "C" fn(*mut c_void) -> *mut c_void,
    arg: *mut c_void,
}

/// The new thread's first function: begins as a mutator, then runs the program's.
extern "C" fn run(born: *mut c_void) -> *mut c_void {
    // SAFETY: `born` is the `Born` `create` boxed for exactly this thread.
    let Born { birth, start, arg } = *unsafe { Box::from_raw(born.cast::<Born>()) };
    // alloc: one `Ending` a thread, freed by the key's destructor as the thread ends.
    let ending = Box::into_raw(Box::new(Ending { me: birth.begin(), rounds: 0 }));
    let key = (KEY.load(Ordering::Acquire) - 1) as libc::pthread_key_t;
    // SAFETY: the key was made by `adopt` before any thread could be created through
    // `create`; the value is this thread's own.
    unsafe { libc::pthread_setspecific(key, ending.cast()) };
    start(arg)
}

/// `pthread_create`, as the program's code calls it under the launcher: the same, with the
/// thread's birth announced first.
///
/// # Safety
///
/// As `pthread_create`'s: `thread` and `attr` as it takes them, `start` a function the new
/// thread may run with `arg`. [`adopt`] has been called.
pub unsafe extern "C" fn create(
    thread: *mut libc::pthread_t,
    attr: *const libc::pthread_attr_t,
    start: extern "C" fn(*mut c_void) -> *mut c_void,
    arg: *mut c_void,
) -> libc::c_int {
    let shared = SHARED.load(Ordering::Acquire);
    // SAFETY: `adopt` leaked a count of the `Arc` this points to (the caller's contract), so
    // it lives as long as the process; a count is taken for the borrow below and given
    // back, so the leaked one stays.
    let shared = unsafe {
        Arc::increment_strong_count(shared);
        Arc::from_raw(shared.cast_const())
    };
    // alloc: what the new thread begins with, freed by it.
    let born = Box::into_raw(Box::new(Born { birth: shared.birth(), start, arg }));
    // SAFETY: the caller's contract for `thread`, `attr`; `run` takes the box.
    let refused = unsafe { libc::pthread_create(thread, attr, run, born.cast()) };
    if refused != 0 {
        // No thread took the box: dropped, its `Birth` says the thread ended.
        // SAFETY: boxed above and handed to no thread.
        drop(unsafe { Box::from_raw(born) });
    }
    refused
}
