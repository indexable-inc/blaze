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

#[cfg(target_os = "macos")]
use blaze_sys::posix::thread;

use crate::table::{Birth, Mutator, Shared};

/// The launcher's `Shared`, leaked by [`adopt`]: what [`create`] announces births to.
static SHARED: AtomicPtr<Shared> = AtomicPtr::new(core::ptr::null_mut());

/// The key whose destructor says a thread ended, plus one; 0 until [`adopt`] made it.
static KEY: AtomicUsize = AtomicUsize::new(0);

/// A thread's [`Mutator`] and how many destructor rounds have passed.
struct Ending {
    me: Mutator,
    rounds: u8,
    /// The thread's registration with the sampler (sample/register.rs), given back at its end
    /// before its `Mutator` says it ended.
    sampled: Option<crate::sample::register::Entered>,
}

/// The rounds a thread's end waits through: the program's destructors run in the earlier.
const ROUNDS: u8 = 4;

/// Makes this process's launcher the one [`create`] tells of new threads, and gives the
/// calling thread (the program's main thread) its [`Mutator`], which it keeps until the
/// process ends. Called once, before any of the program's code runs.
pub fn adopt(shared: &Arc<Shared>) -> Result<Mutator, i32> {
    #[cfg(target_os = "macos")]
    let key = thread::key_create(Some(ended)).map_err(|error| i32::from(error.raw()))?;
    #[cfg(not(target_os = "macos"))]
    let key = {
        let mut key: libc::pthread_key_t = 0;
        // SAFETY: `key` is a live local the call fills; `ended` has the destructor's signature
        // and lives as long as the process.
        let refused = unsafe { libc::pthread_key_create(&mut key, Some(ended)) };
        if refused != 0 {
            return Err(refused);
        }
        key
    };
    KEY.store(key as usize + 1, Ordering::Release);
    // One count of the `Arc` is leaked: threads announce to it for the life of the process.
    SHARED.store(Arc::into_raw(Arc::clone(shared)).cast_mut(), Ordering::Release);
    let me = shared.birth().begin();
    // The main thread is sampled for the life of the process (D100 point 4).
    crate::sample::register::enter_main(me.thread().0);
    Ok(me)
}

/// The key's destructor: `ending` is the ending thread's [`Ending`].
unsafe extern "C" fn ended(ending: *mut c_void) {
    let ending = ending.cast::<Ending>();
    // SAFETY: the key's value is only ever an `Ending` boxed in `run`, owned by the thread
    // whose destructor this is.
    let rounds = unsafe { &mut (*ending).rounds };
    *rounds += 1;
    if *rounds < ROUNDS {
        let key = KEY.load(Ordering::Acquire) - 1;
        // SAFETY: the key is this module's; the value is this thread's own `Ending`. Set
        // again, the system calls this destructor again in its next round.
        #[cfg(target_os = "macos")]
        let armed = unsafe { thread::set_specific(key as thread::pthread_key_t, ending.cast()) }.is_ok();
        #[cfg(not(target_os = "macos"))]
        // SAFETY: the same live key and its own Ending.
        let armed = unsafe { libc::pthread_setspecific(key as libc::pthread_key_t, ending.cast()) } == 0;
        if armed {
            return;
        }
    }
    // SAFETY: boxed in `run` and dropped once, here: the `Mutator`'s drop says the thread
    // ended.
    let Ending { me, rounds: _, sampled } = *unsafe { Box::from_raw(ending) };
    // The sampler stops stopping this thread before the core is told it ended.
    if let Some(entered) = sampled {
        crate::sample::register::leave(entered);
    }
    let _ = crate::telemetry::emit(crate::telemetry::Event::thread_lifecycle(
        crate::telemetry::current_thread_id(),
        false,
        crate::telemetry::now_ns(),
    ));
    drop(me);
}

/// What a new thread is started with.
struct Born {
    birth: Birth,
    start: extern "C" fn(*mut c_void) -> *mut c_void,
    arg: *mut c_void,
    parent_tid: u32,
    spawn_frames: [u64; 3],
}

/// The new thread's first function: begins as a mutator, then runs the program's.
extern "C" fn run(born: *mut c_void) -> *mut c_void {
    // SAFETY: `born` is the `Born` `create` boxed for exactly this thread.
    let Born { birth, start, arg, parent_tid, spawn_frames } = *unsafe { Box::from_raw(born.cast::<Born>()) };
    let me = birth.begin();
    // Registered with the sampler (its port or CPU timer, its stack bounds) before a byte of the
    // program runs on this thread.
    let sampled = crate::sample::register::enter(me.thread().0);
    let _ = crate::telemetry::emit(crate::telemetry::Event::thread_birth(
        crate::telemetry::current_thread_id(),
        crate::telemetry::now_ns(),
        parent_tid,
        spawn_frames,
    ));
    // alloc: one `Ending` a thread, freed by the key's destructor as the thread ends.
    let ending = Box::into_raw(Box::new(Ending { me, rounds: 0, sampled }));
    let key = KEY.load(Ordering::Acquire) - 1;
    #[cfg(target_os = "macos")]
    // SAFETY: adopt created this live key with ended as its destructor for this boxed Ending.
    let _ = unsafe { thread::set_specific(key as thread::pthread_key_t, ending.cast()) };
    #[cfg(not(target_os = "macos"))]
    // SAFETY: the key was made by `adopt` before any thread could be created through
    // `create`; the value is this thread's own.
    unsafe { libc::pthread_setspecific(key as libc::pthread_key_t, ending.cast()) };
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
) -> core::ffi::c_int {
    let shared = SHARED.load(Ordering::Acquire);
    // SAFETY: `adopt` leaked a count of the `Arc` this points to (the caller's contract), so
    // it lives as long as the process; a count is taken for the borrow below and given
    // back, so the leaked one stays.
    let shared = unsafe {
        Arc::increment_strong_count(shared);
        Arc::from_raw(shared.cast_const())
    };
    // alloc: what the new thread begins with, freed by it.
    let parent_tid = crate::telemetry::current_thread_id();
    let spawn_frames = crate::telemetry::capture_caller_frames(1);
    let born = Box::into_raw(Box::new(Born { birth: shared.birth(), start, arg, parent_tid, spawn_frames }));
    #[cfg(target_os = "macos")]
    // SAFETY: thread is writable and attr is null or initialized, as guaranteed by the caller.
    // The macOS pointers name the same pthread ABI types, and run takes ownership of the box.
    let refused = unsafe { thread::pthread_create(thread.cast(), attr.cast(), run, born.cast()) };
    #[cfg(not(target_os = "macos"))]
    // SAFETY: the caller's contract for `thread`, `attr`; `run` takes the box.
    let refused = unsafe { libc::pthread_create(thread, attr, run, born.cast()) };
    if refused != 0 {
        // No thread took the box: dropped, its `Birth` says the thread ended.
        // SAFETY: boxed above and handed to no thread.
        drop(unsafe { Box::from_raw(born) });
    }
    refused
}
