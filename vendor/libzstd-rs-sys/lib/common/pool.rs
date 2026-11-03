use core::ptr;
use std::sync::{Condvar, Mutex};
use std::thread::JoinHandle;

use crate::lib::common::allocations::{ZSTD_customCalloc, ZSTD_customFree};
use crate::lib::zstd::ZSTD_customMem;

pub struct POOL_ctx {
    customMem: ZSTD_customMem,
    threads: *mut JoinHandle<()>,
    threadCapacity: usize,
    threadLimit: usize,
    queue: *mut POOL_job,
    queueHead: usize,
    queueTail: usize,
    queueSize: usize,
    numThreadsBusy: usize,
    queueEmpty: core::ffi::c_int,
    queueMutex: Mutex<()>,
    queuePushCond: Condvar,
    queuePopCond: Condvar,
    shutdown: core::ffi::c_int,
}

#[cfg_attr(target_family = "wasm", allow(dead_code))] // blaze: wasm spawns no threads
struct SendPoolCtxPtr(*mut POOL_ctx);
unsafe impl Send for SendPoolCtxPtr {}

#[derive(Copy, Clone)]
#[repr(C)]
pub(crate) struct POOL_job {
    function: POOL_function,
    opaque: *mut core::ffi::c_void,
}
pub type POOL_function = unsafe fn(*mut core::ffi::c_void) -> ();
pub type ZSTD_threadPool = POOL_ctx;

#[cfg_attr(target_family = "wasm", allow(dead_code))] // blaze: wasm spawns no threads
unsafe fn POOL_thread(ctx: *mut POOL_ctx) {
    if ctx.is_null() {
        return;
    }
    loop {
        let mut guard = (*ctx).queueMutex.lock().unwrap();
        #[expect(clippy::while_immutable_condition)]
        while (*ctx).queueEmpty != 0 || (*ctx).numThreadsBusy >= (*ctx).threadLimit {
            if (*ctx).shutdown != 0 {
                return;
            }
            guard = (*ctx).queuePopCond.wait(guard).unwrap();
        }
        let job = *((*ctx).queue).add((*ctx).queueHead);
        (*ctx).queueHead = ((*ctx).queueHead).wrapping_add(1) % (*ctx).queueSize;
        (*ctx).numThreadsBusy += 1;
        (*ctx).queueEmpty = ((*ctx).queueHead == (*ctx).queueTail) as core::ffi::c_int;
        (*ctx).queuePushCond.notify_one();
        drop(guard);
        (job.function)(job.opaque);
        guard = (*ctx).queueMutex.lock().unwrap();
        (*ctx).numThreadsBusy -= 1;
        (*ctx).queuePushCond.notify_one();
    }
}
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_createThreadPool))]
pub unsafe extern "C" fn ZSTD_createThreadPool(numThreads: usize) -> *mut ZSTD_threadPool {
    POOL_create(numThreads, 0)
}
pub unsafe fn POOL_create(numThreads: usize, queueSize: usize) -> *mut POOL_ctx {
    POOL_create_advanced(numThreads, queueSize, ZSTD_customMem::default())
}
pub(crate) unsafe fn POOL_create_advanced(
    numThreads: usize,
    queueSize: usize,
    customMem: ZSTD_customMem,
) -> *mut POOL_ctx {
    if numThreads == 0 {
        return core::ptr::null_mut();
    }
    let ctx = ZSTD_customCalloc(size_of::<POOL_ctx>(), customMem) as *mut POOL_ctx;
    if ctx.is_null() {
        return core::ptr::null_mut();
    }
    (*ctx).queueSize = queueSize.wrapping_add(1);
    (*ctx).queue =
        ZSTD_customCalloc((*ctx).queueSize * size_of::<POOL_job>(), customMem) as *mut POOL_job;
    (*ctx).queueHead = 0;
    (*ctx).queueTail = 0;
    (*ctx).numThreadsBusy = 0;
    (*ctx).queueEmpty = 1;
    ptr::write(ptr::addr_of_mut!((*ctx).queueMutex), Mutex::new(()));
    ptr::write(ptr::addr_of_mut!((*ctx).queuePushCond), Condvar::new());
    ptr::write(ptr::addr_of_mut!((*ctx).queuePopCond), Condvar::new());
    (*ctx).shutdown = 0;
    (*ctx).threads = ZSTD_customCalloc(numThreads * size_of::<JoinHandle<()>>(), customMem)
        as *mut JoinHandle<()>;
    (*ctx).threadCapacity = 0;
    (*ctx).customMem = customMem;
    if (*ctx).threads.is_null() || (*ctx).queue.is_null() {
        POOL_free(ctx);
        return core::ptr::null_mut();
    }
    // blaze: wasm has no threads; a job runs inline in `POOL_add` (`run_inline`).
    #[cfg(not(target_family = "wasm"))]
    for i in 0..numThreads {
        let ctx = SendPoolCtxPtr(ctx);
        core::ptr::write(
            (*ctx.0).threads.add(i),
            std::thread::spawn(|| {
                let ctx = ctx;
                POOL_thread(ctx.0)
            }),
        );
    }
    (*ctx).threadCapacity = numThreads;
    (*ctx).threadLimit = numThreads;
    ctx
}
unsafe fn POOL_join(ctx: *mut POOL_ctx) {
    let guard = (*ctx).queueMutex.lock().unwrap();
    (*ctx).shutdown = 1;
    drop(guard);
    (*ctx).queuePushCond.notify_all();
    (*ctx).queuePopCond.notify_all();
    #[cfg(not(target_family = "wasm"))]
    for i in 0..(*ctx).threadCapacity {
        core::ptr::read((*ctx).threads.add(i)).join().unwrap();
    }
}
pub unsafe fn POOL_free(ctx: *mut POOL_ctx) {
    if ctx.is_null() {
        return;
    }
    POOL_join(ctx);
    ptr::drop_in_place(ptr::addr_of_mut!((*ctx).queueMutex));
    ptr::drop_in_place(ptr::addr_of_mut!((*ctx).queuePushCond));
    ptr::drop_in_place(ptr::addr_of_mut!((*ctx).queuePopCond));
    ZSTD_customFree(
        (*ctx).queue as *mut core::ffi::c_void,
        (*ctx).queueSize * size_of::<POOL_job>(),
        (*ctx).customMem,
    );
    ZSTD_customFree(
        (*ctx).threads as *mut core::ffi::c_void,
        (*ctx).threadCapacity * size_of::<JoinHandle<()>>(),
        (*ctx).customMem,
    );
    ZSTD_customFree(
        ctx as *mut core::ffi::c_void,
        size_of::<POOL_ctx>(),
        (*ctx).customMem,
    );
}
pub unsafe fn POOL_joinJobs(ctx: *mut POOL_ctx) {
    let mut guard = (*ctx).queueMutex.lock().unwrap();
    #[expect(clippy::while_immutable_condition)]
    while (*ctx).queueEmpty == 0 || (*ctx).numThreadsBusy > 0 {
        guard = (*ctx).queuePushCond.wait(guard).unwrap();
    }
}
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_freeThreadPool))]
pub unsafe extern "C" fn ZSTD_freeThreadPool(pool: *mut ZSTD_threadPool) {
    POOL_free(pool);
}
pub(crate) unsafe fn POOL_sizeof(ctx: *const POOL_ctx) -> usize {
    if ctx.is_null() {
        return 0;
    }
    size_of::<POOL_ctx>()
        + (*ctx).queueSize * size_of::<POOL_job>()
        + (*ctx).threadCapacity * size_of::<JoinHandle<()>>()
}
unsafe fn POOL_resize_internal(ctx: *mut POOL_ctx, numThreads: usize) -> core::ffi::c_int {
    if numThreads <= (*ctx).threadCapacity {
        if numThreads == 0 {
            return 1;
        }
        (*ctx).threadLimit = numThreads;
        return 0;
    }
    let threadPool = ZSTD_customCalloc(
        numThreads.wrapping_mul(size_of::<JoinHandle<()>>()),
        (*ctx).customMem,
    ) as *mut JoinHandle<()>;
    if threadPool.is_null() {
        return 1;
    }
    libc::memcpy(
        threadPool as *mut core::ffi::c_void,
        (*ctx).threads as *const core::ffi::c_void,
        (*ctx).threadCapacity * size_of::<JoinHandle<()>>(),
    );
    ZSTD_customFree(
        (*ctx).threads as *mut core::ffi::c_void,
        (*ctx).threadCapacity * size_of::<JoinHandle<()>>(),
        (*ctx).customMem,
    );
    (*ctx).threads = threadPool;
    #[cfg(not(target_family = "wasm"))]
    for threadId in (*ctx).threadCapacity..numThreads {
        let ctx = SendPoolCtxPtr(ctx);
        core::ptr::write(
            ((*ctx.0).threads).add(threadId),
            std::thread::spawn(|| {
                let ctx = ctx;
                POOL_thread(ctx.0)
            }),
        );
    }
    (*ctx).threadCapacity = numThreads;
    (*ctx).threadLimit = numThreads;
    0
}
pub(crate) unsafe fn POOL_resize(ctx: *mut POOL_ctx, numThreads: usize) -> core::ffi::c_int {
    if ctx.is_null() {
        return 1;
    }
    let _guard = (*ctx).queueMutex.lock().unwrap();
    let result = POOL_resize_internal(ctx, numThreads);
    (*ctx).queuePopCond.notify_all();
    result
}
unsafe fn isQueueFull(ctx: *const POOL_ctx) -> core::ffi::c_int {
    if (*ctx).queueSize > 1 {
        ((*ctx).queueHead == ((*ctx).queueTail).wrapping_add(1) % (*ctx).queueSize)
            as core::ffi::c_int
    } else {
        ((*ctx).numThreadsBusy == (*ctx).threadLimit || (*ctx).queueEmpty == 0) as core::ffi::c_int
    }
}
/// blaze: on wasm, where no thread can be spawned, a job runs to its end in the call that adds
/// it, as upstream's C pool built without `ZSTD_MULTITHREAD` runs it. zstdmt's frames depend on
/// how its input is cut into jobs, not on where the jobs run, so they are the bytes threads make.
#[cfg(target_family = "wasm")]
const RUN_INLINE: bool = true;
#[cfg(not(target_family = "wasm"))]
const RUN_INLINE: bool = false;

unsafe fn POOL_add_internal(
    ctx: *mut POOL_ctx,
    function: POOL_function,
    opaque: *mut core::ffi::c_void,
) {
    let job = POOL_job { function, opaque };
    if (*ctx).shutdown != 0 {
        return;
    }
    (*ctx).queueEmpty = 0;
    *((*ctx).queue).add((*ctx).queueTail) = job;
    (*ctx).queueTail = ((*ctx).queueTail).wrapping_add(1) % (*ctx).queueSize;
    (*ctx).queuePopCond.notify_one();
}
pub unsafe fn POOL_add(
    ctx: *mut POOL_ctx,
    function: POOL_function,
    opaque: *mut core::ffi::c_void,
) {
    if RUN_INLINE {
        if (*ctx).shutdown == 0 {
            function(opaque);
        }
        return;
    }
    let mut guard = (*ctx).queueMutex.lock().unwrap();
    while isQueueFull(ctx) != 0 && (*ctx).shutdown == 0 {
        guard = (*ctx).queuePushCond.wait(guard).unwrap();
    }
    POOL_add_internal(ctx, function, opaque);
}
pub(crate) unsafe fn POOL_tryAdd(
    ctx: *mut POOL_ctx,
    function: POOL_function,
    opaque: *mut core::ffi::c_void,
) -> core::ffi::c_int {
    if RUN_INLINE {
        POOL_add(ctx, function, opaque);
        return 1;
    }
    let _guard = (*ctx).queueMutex.lock().unwrap();
    if isQueueFull(ctx) != 0 {
        return 0;
    }
    POOL_add_internal(ctx, function, opaque);
    1
}
