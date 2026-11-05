use core::ptr;
use std::sync::{Condvar, Mutex};

use crate::lib::common::allocations::{ZSTD_customCalloc, ZSTD_customFree, ZSTD_customMalloc};
use crate::lib::common::bits::ZSTD_highbit32;
use crate::lib::common::error_private::{ERR_isError, Error};
use crate::lib::common::mem::{MEM_32bits, MEM_writeLE32};
use crate::lib::common::pool::{
    POOL_create_advanced, POOL_free, POOL_resize, POOL_sizeof, POOL_tryAdd, ZSTD_threadPool,
};
use crate::lib::common::xxhash::{
    XXH64_state_t, ZSTD_XXH64_digest, ZSTD_XXH64_reset, ZSTD_XXH64_update_slice,
};
use crate::lib::compress::zstd_compress::{
    rawSeq, RawSeqStore_t, ZSTD_CCtx, ZSTD_CCtxParams_setParameter, ZSTD_CCtx_params,
    ZSTD_CCtx_trace, ZSTD_CDict, ZSTD_compressBegin_advanced_internal, ZSTD_compressBound,
    ZSTD_compressContinue_internal, ZSTD_compressContinue_public, ZSTD_compressEnd_internal,
    ZSTD_createCCtx_advanced, ZSTD_createCDict_advanced, ZSTD_cycleLog, ZSTD_freeCCtx,
    ZSTD_freeCDict, ZSTD_getCParamsFromCCtxParams_internal, ZSTD_invalidateRepCodes,
    ZSTD_referenceExternalSequences, ZSTD_sizeof_CCtx, ZSTD_sizeof_CDict, ZSTD_window_t,
    ZSTD_writeLastEmptyBlock,
};
use crate::lib::compress::zstd_compress_internal::{prime8bytes, CParamMode, DictTableLoadMethod};
use crate::lib::compress::zstd_ldm::{LdmEntry, LdmParams, LdmState};
use crate::lib::polyfill::PointerExt;
use crate::lib::zstd::{
    ParamSwitch, ZSTD_EndDirective, ZSTD_btlazy2, ZSTD_btopt, ZSTD_btultra, ZSTD_btultra2,
    ZSTD_cParameter, ZSTD_customMem, ZSTD_dct_auto, ZSTD_dct_rawContent, ZSTD_dictContentType_e,
    ZSTD_dlm_byCopy, ZSTD_dlm_byRef, ZSTD_e_continue, ZSTD_e_end, ZSTD_e_flush,
    ZSTD_frameProgression, ZSTD_inBuffer, ZSTD_lazy2, ZSTD_outBuffer_s, ZSTD_strategy,
    ZSTD_BLOCKSIZELOG_MAX, ZSTD_BLOCKSIZE_MAX, ZSTD_CONTENTSIZE_UNKNOWN,
};

#[repr(C)]
pub struct ZSTDMT_CCtx {
    factory: *mut ZSTD_threadPool,
    jobs: *mut ZSTDMT_jobDescription,
    bufPool: *mut ZSTDMT_bufferPool,
    cctxPool: *mut ZSTDMT_CCtxPool,
    seqPool: *mut ZSTDMT_seqPool,
    params: ZSTD_CCtx_params,
    targetSectionSize: usize,
    targetPrefixSize: usize,
    jobReady: core::ffi::c_int,
    inBuff: InBuff_t,
    roundBuff: RoundBuff_t,
    serial: SerialState,
    rsync: RSyncState_t,
    jobIDMask: core::ffi::c_uint,
    doneJobID: core::ffi::c_uint,
    nextJobID: core::ffi::c_uint,
    frameEnded: core::ffi::c_uint,
    allJobsCompleted: core::ffi::c_uint,
    frameContentSize: core::ffi::c_ulonglong,
    consumed: core::ffi::c_ulonglong,
    produced: core::ffi::c_ulonglong,
    cMem: ZSTD_customMem,
    cdictLocal: *mut ZSTD_CDict,
    cdict: *const ZSTD_CDict,
    providedFactory: bool,
}

#[repr(C)]
struct RSyncState_t {
    hash: u64,
    hitMask: u64,
    primePower: u64,
}

struct SerialState {
    // All variables in the struct are protected by mutex.
    mutex: Mutex<()>,
    cond: Condvar,
    params: ZSTD_CCtx_params,
    ldmState: LdmState,
    xxhState: XXH64_state_t,
    nextJobID: core::ffi::c_uint,
    // Protects ldmWindow.
    // Must be acquired after the main mutex when acquiring both.
    ldmWindowMutex: Mutex<()>,
    ldmWindowCond: Condvar,   // Signaled when ldmWindow is updated
    ldmWindow: ZSTD_window_t, // A thread-safe copy of ldmState.window
}

impl Default for SerialState {
    fn default() -> Self {
        Self {
            mutex: Mutex::new(()),
            cond: Condvar::new(),
            ldmWindowMutex: Mutex::new(()),
            ldmWindowCond: Condvar::new(),
            params: ZSTD_CCtx_params::default(),
            ldmState: LdmState::default(),
            xxhState: XXH64_state_t::default(),
            nextJobID: 0,
            ldmWindow: ZSTD_window_t::default(),
        }
    }
}

#[derive(Copy, Clone)]
#[repr(C)]
struct RoundBuff_t {
    buffer: *mut u8,
    capacity: usize,
    pos: usize,
}

#[repr(C)]
struct InBuff_t {
    prefix: Range,
    buffer: Buffer,
    filled: usize,
}
type Buffer = buffer_s;
#[derive(Copy, Clone)]
#[repr(C)]
struct buffer_s {
    start: *mut core::ffi::c_void,
    capacity: usize,
}

#[derive(Copy, Clone, Default)]
#[repr(C)]
struct Range {
    start: *const core::ffi::c_void,
    size: usize,
}

type ZSTDMT_seqPool = ZSTDMT_bufferPool;
type ZSTDMT_bufferPool = ZSTDMT_bufferPool_s;

struct ZSTDMT_bufferPool_s {
    poolMutex: Mutex<()>,
    bufferSize: usize,
    totalBuffers: core::ffi::c_uint,
    nbBuffers: core::ffi::c_uint,
    cMem: ZSTD_customMem,
    buffers: *mut Buffer,
}

struct ZSTDMT_CCtxPool {
    poolMutex: Mutex<()>,
    totalCCtx: core::ffi::c_int,
    availCCtx: core::ffi::c_int,
    cMem: ZSTD_customMem,
    cctxs: *mut *mut ZSTD_CCtx,
}

struct ZSTDMT_jobDescription {
    /// SHARED - set0 by mtctx, then modified by worker AND read by mtctx
    consumed: usize,
    /// SHARED - set0 by mtctx, then modified by worker AND read by mtctx, then set0 by mtctx
    cSize: usize,
    /// Thread-safe - used by mtctx and worker
    job_mutex: Mutex<()>,
    /// Thread-safe - used by mtctx and worker
    job_cond: Condvar,
    /// Thread-safe - used by mtctx and (all) workers
    cctxPool: *mut ZSTDMT_CCtxPool,
    /// Thread-safe - used by mtctx and (all) workers
    bufPool: *mut ZSTDMT_bufferPool,
    /// Thread-safe - used by mtctx and (all) workers
    seqPool: *mut ZSTDMT_seqPool,
    /// Thread-safe - used by mtctx and (all) workers
    serial: *mut SerialState,
    /// set by worker (or mtctx), then read by worker & mtctx, then modified by mtctx => no barrier
    dstBuff: Buffer,
    /// set by mtctx, then read by worker & mtctx => no barrier
    prefix: Range,
    /// set by mtctx, then read by worker & mtctx => no barrier
    src: Range,
    /// set by mtctx, then read by worker => no barrier
    jobID: core::ffi::c_uint,
    /// set by mtctx, then read by worker => no barrier
    firstJob: core::ffi::c_uint,
    /// set by mtctx, then read by worker => no barrier
    lastJob: core::ffi::c_uint,
    /// set by mtctx, then read by worker => no barrier
    params: ZSTD_CCtx_params,
    /// set by mtctx, then read by worker => no barrier
    cdict: *const ZSTD_CDict,
    /// set by mtctx, then read by worker => no barrier
    fullFrameSize: core::ffi::c_ulonglong,
    /// used only by mtctx
    dstFlushed: usize,
    /// used only by mtctx
    frameChecksumNeeded: core::ffi::c_uint,
}

type ZSTD_outBuffer = ZSTD_outBuffer_s;
#[repr(C)]
struct SyncPoint {
    toLoad: usize,
    flush: core::ffi::c_int,
}

/// Return base^exponent
fn ZSTD_ipow(mut base: u64, mut exponent: u64) -> u64 {
    let mut power = 1u64;
    while exponent != 0 {
        if exponent & 1 != 0 {
            power = power.wrapping_mul(base);
        }
        exponent >>= 1;
        base = base.wrapping_mul(base);
    }
    power
}

const ZSTD_ROLL_HASH_CHAR_OFFSET: u64 = 10;

/// Add the buffer to the hash value
fn ZSTD_rollingHash_append(mut hash: u64, buf: &[u8]) -> u64 {
    for &byte in buf {
        hash = hash.wrapping_mul(prime8bytes);
        hash = hash.wrapping_add(u64::from(byte) + ZSTD_ROLL_HASH_CHAR_OFFSET);
    }
    hash
}

/// Compute the rolling hash value of the buffer.
#[inline]
fn ZSTD_rollingHash_compute(buf: &[u8]) -> u64 {
    ZSTD_rollingHash_append(0, buf)
}

/// Compute the primePower to be passed to ZSTD_rollingHash_rotate() for a hash
/// over a window of length bytes.
#[inline]
fn ZSTD_rollingHash_primePower(length: u32) -> u64 {
    ZSTD_ipow(prime8bytes, length.wrapping_sub(1) as u64)
}

/// Rotate the rolling hash by one byte.
#[inline]
fn ZSTD_rollingHash_rotate(mut hash: u64, toRemove: u8, toAdd: u8, primePower: u64) -> u64 {
    hash = hash
        .wrapping_sub((u64::from(toRemove) + ZSTD_ROLL_HASH_CHAR_OFFSET).wrapping_mul(primePower));
    hash = hash.wrapping_mul(prime8bytes);
    hash = hash.wrapping_add(u64::from(toAdd) + ZSTD_ROLL_HASH_CHAR_OFFSET);
    hash
}

pub const ZSTDMT_NBWORKERS_MAX: core::ffi::c_int = if MEM_32bits() { 64 } else { 256 };
pub const ZSTDMT_JOBSIZE_MIN: core::ffi::c_int = 512 * (1 << 10);
pub const ZSTDMT_JOBLOG_MAX: core::ffi::c_uint = if MEM_32bits() { 29 } else { 30 };
pub const ZSTDMT_JOBSIZE_MAX: core::ffi::c_int = if MEM_32bits() {
    512 * (1 << 20)
} else {
    1024 * (1 << 20)
};

const g_nullBuffer: Buffer = buffer_s {
    start: core::ptr::null_mut(),
    capacity: 0,
};

unsafe fn ZSTDMT_freeBufferPool(bufPool: *mut ZSTDMT_bufferPool) {
    if bufPool.is_null() {
        return;
    }

    if !((*bufPool).buffers).is_null() {
        for u in 0..(*bufPool).totalBuffers {
            ZSTD_customFree(
                (*((*bufPool).buffers).offset(u as isize)).start,
                (*((*bufPool).buffers).offset(u as isize)).capacity,
                (*bufPool).cMem,
            );
        }
        ZSTD_customFree(
            (*bufPool).buffers as *mut core::ffi::c_void,
            (*bufPool).bufferSize,
            (*bufPool).cMem,
        );
    }

    core::ptr::drop_in_place(core::ptr::addr_of_mut!((*bufPool).poolMutex));
    ZSTD_customFree(
        bufPool as *mut core::ffi::c_void,
        size_of::<ZSTDMT_bufferPool>(),
        (*bufPool).cMem,
    );
}

unsafe fn ZSTDMT_createBufferPool(
    maxNbBuffers: core::ffi::c_uint,
    cMem: ZSTD_customMem,
) -> *mut ZSTDMT_bufferPool {
    let bufPool = ZSTD_customCalloc(size_of::<ZSTDMT_bufferPool>(), cMem) as *mut ZSTDMT_bufferPool;
    if bufPool.is_null() {
        return core::ptr::null_mut();
    }
    core::ptr::write(
        core::ptr::addr_of_mut!((*bufPool).poolMutex),
        Mutex::new(()),
    );
    (*bufPool).buffers = ZSTD_customCalloc(
        (maxNbBuffers as usize).wrapping_mul(size_of::<Buffer>()),
        cMem,
    ) as *mut Buffer;
    if ((*bufPool).buffers).is_null() {
        ZSTDMT_freeBufferPool(bufPool);
        return core::ptr::null_mut();
    }
    (*bufPool).bufferSize = 64 * (1 << 10);
    (*bufPool).totalBuffers = maxNbBuffers;
    (*bufPool).nbBuffers = 0;
    (*bufPool).cMem = cMem;
    bufPool
}

/// Only works at initialization, not during compression.
unsafe fn ZSTDMT_sizeof_bufferPool(bufPool: *mut ZSTDMT_bufferPool) -> usize {
    let poolSize = size_of::<ZSTDMT_bufferPool>();
    let arraySize = ((*bufPool).totalBuffers as usize).wrapping_mul(size_of::<Buffer>());
    let mut totalBufferSize = 0usize;
    let _guard = (*bufPool).poolMutex.lock().unwrap();
    for u in 0..(*bufPool).totalBuffers {
        totalBufferSize =
            totalBufferSize.wrapping_add((*((*bufPool).buffers).offset(u as isize)).capacity);
    }

    poolSize
        .wrapping_add(arraySize)
        .wrapping_add(totalBufferSize)
}

/// All future buffers provided by this buffer pool will have _at least_ this size.
///
/// Note: it's better for all buffers to have same size, as they become freely
/// interchangeable, reducing malloc/free usages and memory fragmentation.
unsafe fn ZSTDMT_setBufferSize(bufPool: *mut ZSTDMT_bufferPool, bSize: usize) {
    let _guard = (*bufPool).poolMutex.lock().unwrap();
    (*bufPool).bufferSize = bSize;
}

unsafe fn ZSTDMT_expandBufferPool(
    srcBufPool: *mut ZSTDMT_bufferPool,
    maxNbBuffers: core::ffi::c_uint,
) -> *mut ZSTDMT_bufferPool {
    if srcBufPool.is_null() {
        return core::ptr::null_mut();
    }

    if (*srcBufPool).totalBuffers >= maxNbBuffers {
        return srcBufPool; // good enough
    }

    // need a larger buffer pool
    let cMem = (*srcBufPool).cMem;
    let bSize = (*srcBufPool).bufferSize; // forward parameters
    ZSTDMT_freeBufferPool(srcBufPool);
    let newBufPool = ZSTDMT_createBufferPool(maxNbBuffers, cMem);
    if newBufPool.is_null() {
        return newBufPool;
    }
    ZSTDMT_setBufferSize(newBufPool, bSize);
    newBufPool
}

/// Assumption: bufPool must be valid
///
/// Note: allocation may fail, in this case, start==NULL and size==0
///
/// # Returns
///
/// A buffer, with start pointer and size.
unsafe fn ZSTDMT_getBuffer(bufPool: *mut ZSTDMT_bufferPool) -> Buffer {
    let bSize = (*bufPool).bufferSize;
    let guard = (*bufPool).poolMutex.lock().unwrap();
    if (*bufPool).nbBuffers != 0 {
        // try to use an existing buffer
        (*bufPool).nbBuffers = ((*bufPool).nbBuffers).wrapping_sub(1);
        let buf = *((*bufPool).buffers).offset((*bufPool).nbBuffers as isize);
        let availBufferSize = buf.capacity;
        *((*bufPool).buffers).offset((*bufPool).nbBuffers as isize) = g_nullBuffer;
        if (availBufferSize >= bSize) && (availBufferSize >> 3 <= bSize) {
            // large enough, but not too much
            return buf;
        }
        // size conditions not respected: scratch this buffer, create new one
        ZSTD_customFree(buf.start, buf.capacity, (*bufPool).cMem);
    }
    drop(guard);

    let mut buffer = buffer_s {
        start: core::ptr::null_mut::<core::ffi::c_void>(),
        capacity: 0,
    };
    let start = ZSTD_customMalloc(bSize, (*bufPool).cMem);
    buffer.start = start; // note : start can be NULL if malloc fails
    buffer.capacity = if start.is_null() { 0 } else { bSize };
    start.is_null();
    buffer
}

/// Store buffer for later re-use, up to pool capacity.
unsafe fn ZSTDMT_releaseBuffer(bufPool: *mut ZSTDMT_bufferPool, buf: Buffer) {
    if buf.start.is_null() {
        return;
    }
    let guard = (*bufPool).poolMutex.lock().unwrap();
    if (*bufPool).nbBuffers < (*bufPool).totalBuffers {
        *((*bufPool).buffers).offset((*bufPool).nbBuffers as isize) = buf; // stored for later use
        (*bufPool).nbBuffers = ((*bufPool).nbBuffers).wrapping_add(1);
        return;
    }
    drop(guard);

    // Reached bufferPool capacity (note: should not happen)
    ZSTD_customFree(buf.start, buf.capacity, (*bufPool).cMem);
}

unsafe fn ZSTDMT_sizeof_seqPool(seqPool: *mut ZSTDMT_seqPool) -> usize {
    ZSTDMT_sizeof_bufferPool(seqPool)
}

fn bufferToSeq(buffer: Buffer) -> RawSeqStore_t {
    RawSeqStore_t {
        seq: buffer.start as *mut rawSeq,
        capacity: buffer.capacity.wrapping_div(size_of::<rawSeq>()),
        ..Default::default()
    }
}

fn seqToBuffer(seq: RawSeqStore_t) -> Buffer {
    Buffer {
        start: seq.seq as *mut core::ffi::c_void,
        capacity: seq.capacity.wrapping_mul(size_of::<rawSeq>()),
    }
}

unsafe fn ZSTDMT_getSeq(seqPool: *mut ZSTDMT_seqPool) -> RawSeqStore_t {
    if (*seqPool).bufferSize == 0 {
        return RawSeqStore_t::default();
    }
    bufferToSeq(ZSTDMT_getBuffer(seqPool))
}

unsafe fn ZSTDMT_releaseSeq(seqPool: *mut ZSTDMT_seqPool, seq: RawSeqStore_t) {
    ZSTDMT_releaseBuffer(seqPool, seqToBuffer(seq));
}

unsafe fn ZSTDMT_setNbSeq(seqPool: *mut ZSTDMT_seqPool, nbSeq: usize) {
    ZSTDMT_setBufferSize(seqPool, nbSeq.wrapping_mul(size_of::<rawSeq>()));
}

unsafe fn ZSTDMT_createSeqPool(
    nbWorkers: core::ffi::c_uint,
    cMem: ZSTD_customMem,
) -> *mut ZSTDMT_seqPool {
    let seqPool = ZSTDMT_createBufferPool(nbWorkers, cMem);
    if seqPool.is_null() {
        return core::ptr::null_mut();
    }
    ZSTDMT_setNbSeq(seqPool, 0);
    seqPool
}

unsafe fn ZSTDMT_freeSeqPool(seqPool: *mut ZSTDMT_seqPool) {
    ZSTDMT_freeBufferPool(seqPool);
}

unsafe fn ZSTDMT_expandSeqPool(pool: *mut ZSTDMT_seqPool, nbWorkers: u32) -> *mut ZSTDMT_seqPool {
    ZSTDMT_expandBufferPool(pool, nbWorkers)
}

/// Note: all CCtx borrowed from the pool must be reverted back to the pool _before_ freeing the pool
unsafe fn ZSTDMT_freeCCtxPool(pool: *mut ZSTDMT_CCtxPool) {
    if pool.is_null() {
        return;
    }
    core::ptr::drop_in_place(core::ptr::addr_of_mut!((*pool).poolMutex));
    if !((*pool).cctxs).is_null() {
        for cid in 0..(*pool).totalCCtx {
            ZSTD_freeCCtx(*((*pool).cctxs).offset(cid as isize));
        }
        ZSTD_customFree(
            (*pool).cctxs as *mut core::ffi::c_void,
            ((*pool).totalCCtx as usize).wrapping_mul(size_of::<*mut ZSTD_CCtx>()),
            (*pool).cMem,
        );
    }
    ZSTD_customFree(
        pool as *mut core::ffi::c_void,
        size_of::<ZSTDMT_CCtxPool>(),
        (*pool).cMem,
    );
}

/// implies nbWorkers >= 1, checked by caller ZSTDMT_createCCtx()
unsafe fn ZSTDMT_createCCtxPool(
    nbWorkers: core::ffi::c_int,
    cMem: ZSTD_customMem,
) -> *mut ZSTDMT_CCtxPool {
    let cctxPool = ZSTD_customCalloc(size_of::<ZSTDMT_CCtxPool>(), cMem) as *mut ZSTDMT_CCtxPool;
    if cctxPool.is_null() {
        return core::ptr::null_mut();
    }
    core::ptr::write(
        core::ptr::addr_of_mut!((*cctxPool).poolMutex),
        Mutex::new(()),
    );
    (*cctxPool).totalCCtx = nbWorkers;
    (*cctxPool).cctxs = ZSTD_customCalloc(
        (nbWorkers as usize).wrapping_mul(size_of::<*mut ZSTD_CCtx>()),
        cMem,
    ) as *mut *mut ZSTD_CCtx;
    if ((*cctxPool).cctxs).is_null() {
        ZSTDMT_freeCCtxPool(cctxPool);
        return core::ptr::null_mut();
    }
    (*cctxPool).cMem = cMem;
    *((*cctxPool).cctxs) = ZSTD_createCCtx_advanced(cMem);
    if (*((*cctxPool).cctxs)).is_null() {
        ZSTDMT_freeCCtxPool(cctxPool);
        return core::ptr::null_mut();
    }
    (*cctxPool).availCCtx = 1; // at least one cctx for single-thread mode
    cctxPool
}

unsafe fn ZSTDMT_expandCCtxPool(
    srcPool: *mut ZSTDMT_CCtxPool,
    nbWorkers: core::ffi::c_int,
) -> *mut ZSTDMT_CCtxPool {
    if srcPool.is_null() {
        return core::ptr::null_mut();
    }
    if nbWorkers <= (*srcPool).totalCCtx {
        return srcPool; // good enough
    }

    // need a larger cctx pool
    let cMem = (*srcPool).cMem;
    ZSTDMT_freeCCtxPool(srcPool);
    ZSTDMT_createCCtxPool(nbWorkers, cMem)
}

/// Only works during initialization phase, not during compression.
unsafe fn ZSTDMT_sizeof_CCtxPool(cctxPool: *mut ZSTDMT_CCtxPool) -> usize {
    let _guard = (*cctxPool).poolMutex.lock().unwrap();
    let nbWorkers = (*cctxPool).totalCCtx as core::ffi::c_uint;
    let poolSize = size_of::<ZSTDMT_CCtxPool>();
    let arraySize = ((*cctxPool).totalCCtx as usize).wrapping_mul(size_of::<*mut ZSTD_CCtx>());
    let mut totalCCtxSize = 0usize;
    for u in 0..nbWorkers {
        totalCCtxSize =
            totalCCtxSize.wrapping_add(ZSTD_sizeof_CCtx(*((*cctxPool).cctxs).offset(u as isize)));
    }
    poolSize.wrapping_add(arraySize).wrapping_add(totalCCtxSize)
}

unsafe fn ZSTDMT_getCCtx(cctxPool: *mut ZSTDMT_CCtxPool) -> *mut ZSTD_CCtx {
    let _guard = (*cctxPool).poolMutex.lock().unwrap();
    if (*cctxPool).availCCtx != 0 {
        (*cctxPool).availCCtx -= 1;
        let cctx = *((*cctxPool).cctxs).offset((*cctxPool).availCCtx as isize);
        return cctx;
    }
    ZSTD_createCCtx_advanced((*cctxPool).cMem) // note: can be NULL, when creation fails!
}

unsafe fn ZSTDMT_releaseCCtx(pool: *mut ZSTDMT_CCtxPool, cctx: *mut ZSTD_CCtx) {
    if cctx.is_null() {
        return;
    }

    let _guard = (*pool).poolMutex.lock().unwrap();
    if (*pool).availCCtx < (*pool).totalCCtx {
        *((*pool).cctxs).offset((*pool).availCCtx as isize) = cctx;
        (*pool).availCCtx += 1;
    } else {
        // pool overflow: should not happen, since totalCCtx==nbWorkers
        ZSTD_freeCCtx(cctx);
    }
}

unsafe fn ZSTDMT_serialState_reset(
    serialState: &mut SerialState,
    seqPool: *mut ZSTDMT_seqPool,
    mut params: ZSTD_CCtx_params,
    jobSize: usize,
    dict: *const core::ffi::c_void,
    dictSize: usize,
    dictContentType: ZSTD_dictContentType_e,
) -> Result<(), Error> {
    // Adjust parameters
    if params.ldmParams.enableLdm == ParamSwitch::Enable {
        params.ldmParams.adjust_parameters(&params.cParams);
    } else {
        params.ldmParams = LdmParams::default();
    }

    serialState.nextJobID = 0;
    if params.fParams.checksumFlag != 0 {
        ZSTD_XXH64_reset(&mut serialState.xxhState, 0);
    }

    if params.ldmParams.enableLdm == ParamSwitch::Enable {
        let cMem = params.customMem;
        let hashLog = params.ldmParams.hashLog;
        let hashSize = (1usize << hashLog).wrapping_mul(size_of::<LdmEntry>());
        let bucketLog = (params.ldmParams.hashLog).wrapping_sub(params.ldmParams.bucketSizeLog);
        let prevBucketLog = (serialState.params.ldmParams.hashLog)
            .wrapping_sub(serialState.params.ldmParams.bucketSizeLog);
        let numBuckets = 1 << bucketLog;
        // Size the seq pool tables
        ZSTDMT_setNbSeq(seqPool, params.ldmParams.get_max_nb_seq(jobSize));
        // Reset the window
        serialState.ldmState.window.init();
        // Resize tables and output space if necessary.
        if serialState.ldmState.hashTable.is_null()
            || serialState.params.ldmParams.hashLog < hashLog
        {
            ZSTD_customFree(
                serialState.ldmState.hashTable as *mut core::ffi::c_void,
                hashSize,
                cMem,
            );
            serialState.ldmState.hashTable = ZSTD_customMalloc(hashSize, cMem) as *mut LdmEntry;
        }
        if serialState.ldmState.bucketOffsets.is_null() || prevBucketLog < bucketLog {
            ZSTD_customFree(
                serialState.ldmState.bucketOffsets as *mut core::ffi::c_void,
                1 << prevBucketLog,
                cMem,
            );
            serialState.ldmState.bucketOffsets = ZSTD_customMalloc(numBuckets, cMem) as *mut u8;
        }
        if serialState.ldmState.hashTable.is_null() || serialState.ldmState.bucketOffsets.is_null()
        {
            return Err(Error::memory_allocation);
        }
        // Zero the tables
        ptr::write_bytes(serialState.ldmState.hashTable as *mut u8, 0, hashSize);
        ptr::write_bytes(serialState.ldmState.bucketOffsets, 0, numBuckets);

        // Update window state and fill hash table with dict
        serialState.ldmState.loadedDictEnd = 0;
        if dictSize > 0 && dictContentType == ZSTD_dct_rawContent {
            let dictEnd = (dict as *const u8).add(dictSize);
            serialState.ldmState.window.update(dict, dictSize, false);
            serialState.ldmState.fill_hash_table(
                core::slice::from_raw_parts(dict.cast::<u8>(), dictSize),
                &params.ldmParams,
            );
            serialState.ldmState.loadedDictEnd = if params.forceWindow != 0 {
                0
            } else {
                dictEnd.offset_from(serialState.ldmState.window.base) as core::ffi::c_long as u32
            };
        }

        // Initialize serialState's copy of ldmWindow.
        serialState.ldmWindow = serialState.ldmState.window;
    }

    serialState.params = params;
    serialState.params.jobSize = jobSize as u32 as usize;

    Ok(())
}

unsafe fn ZSTDMT_serialState_free(serialState: &mut SerialState) {
    let cMem = serialState.params.customMem;
    core::ptr::drop_in_place(&raw mut serialState.mutex);
    core::ptr::drop_in_place(&raw mut serialState.cond);
    core::ptr::drop_in_place(&raw mut serialState.ldmWindowMutex);
    core::ptr::drop_in_place(&raw mut serialState.ldmWindowCond);
    let hashLog = serialState.params.ldmParams.hashLog;
    let hashSize = (1usize << hashLog).wrapping_mul(size_of::<LdmEntry>());
    let bucketLog = (serialState.params.ldmParams.hashLog)
        .wrapping_sub(serialState.params.ldmParams.bucketSizeLog);
    let numBuckets = 1usize << bucketLog;
    ZSTD_customFree(
        serialState.ldmState.hashTable as *mut core::ffi::c_void,
        hashSize,
        cMem,
    );
    ZSTD_customFree(
        serialState.ldmState.bucketOffsets as *mut core::ffi::c_void,
        numBuckets,
        cMem,
    );
}

unsafe fn ZSTDMT_serialState_genSequences(
    serialState: *mut SerialState,
    seqStore: &mut RawSeqStore_t,
    src: Range,
    jobID: core::ffi::c_uint,
) {
    // Wait for our turn
    let mut guard = (*serialState).mutex.lock().unwrap();
    #[expect(clippy::while_immutable_condition)]
    while (*serialState).nextJobID < jobID {
        guard = (*serialState).cond.wait(guard).unwrap();
    }

    // A future job may error and skip our job
    if (*serialState).nextJobID == jobID {
        // It is now our turn, do any processing necessary
        if (*serialState).params.ldmParams.enableLdm == ParamSwitch::Enable {
            (*serialState)
                .ldmState
                .window
                .update(src.start, src.size, false);
            (*serialState)
                .ldmState
                .generate_sequences(
                    seqStore,
                    &(*serialState).params.ldmParams,
                    if src.size == 0 {
                        &[]
                    } else {
                        core::slice::from_raw_parts(src.start.cast(), src.size)
                    },
                )
                .expect("We provide a large enough buffer to never fail.");

            // Update ldmWindow to match the ldmState.window and signal the main
            // thread if it is waiting for a buffer.
            let _guard = (*serialState).ldmWindowMutex.lock().unwrap();
            (*serialState).ldmWindow = (*serialState).ldmState.window;
            (*serialState).ldmWindowCond.notify_one();
        }
        if (*serialState).params.fParams.checksumFlag != 0 && src.size > 0 {
            ZSTD_XXH64_update_slice(
                &mut (*serialState).xxhState,
                core::slice::from_raw_parts(src.start as *const u8, src.size),
            );
        }
    }

    // Now it is the next job's turn
    (*serialState).nextJobID = ((*serialState).nextJobID).wrapping_add(1);
    (*serialState).cond.notify_all();
}

unsafe fn ZSTDMT_serialState_applySequences(
    _serialState: *const SerialState,
    jobCCtx: *mut ZSTD_CCtx,
    seqStore: &RawSeqStore_t,
) {
    if seqStore.size > 0 {
        ZSTD_referenceExternalSequences(jobCCtx, seqStore.seq, seqStore.size);
    }
}

unsafe fn ZSTDMT_serialState_ensureFinished(
    serialState: *mut SerialState,
    jobID: core::ffi::c_uint,
    _cSize: usize,
) {
    let _guard = (*serialState).mutex.lock().unwrap();
    if (*serialState).nextJobID <= jobID {
        // Skipping past job because of error
        (*serialState).nextJobID = jobID.wrapping_add(1);
        (*serialState).cond.notify_all();
        let _guard = (*serialState).ldmWindowMutex.lock().unwrap();
        (*serialState).ldmWindow.clear();
        (*serialState).ldmWindowCond.notify_one();
    }
}

/// This is a POOL_function type
unsafe fn ZSTDMT_compressionJob(jobDescription: *mut core::ffi::c_void) {
    let mut current_block: u64;
    let job = jobDescription as *mut ZSTDMT_jobDescription;
    let mut jobParams = (*job).params; // do not modify job->params ! copy it, modify the copy
    let cctx = ZSTDMT_getCCtx((*job).cctxPool);
    let mut rawSeqStore = ZSTDMT_getSeq((*job).seqPool);
    let mut dstBuff = (*job).dstBuff;
    let mut lastCBlockSize = 0;

    if cctx.is_null() {
        let guard = (*job).job_mutex.lock().unwrap();
        (*job).cSize = Error::memory_allocation.to_error_code();
        drop(guard);
    } else {
        if dstBuff.start.is_null() {
            // streaming job: doesn't provide a dstBuffer
            dstBuff = ZSTDMT_getBuffer((*job).bufPool);
            if dstBuff.start.is_null() {
                let guard = (*job).job_mutex.lock().unwrap();
                (*job).cSize = Error::memory_allocation.to_error_code();
                drop(guard);
                current_block = 17100290475540901977;
            } else {
                (*job).dstBuff = dstBuff; // this value can be read in ZSTDMT_flush, when it copies the whole job
                current_block = 7976072742316086414;
            }
        } else {
            current_block = 7976072742316086414;
        }

        match current_block {
            17100290475540901977 => {}
            _ => {
                if jobParams.ldmParams.enableLdm == ParamSwitch::Enable && rawSeqStore.seq.is_null()
                {
                    let guard = (*job).job_mutex.lock().unwrap();
                    (*job).cSize = Error::memory_allocation.to_error_code();
                    drop(guard);
                } else {
                    // Don't compute the checksum for chunks, since we compute it externally,
                    // but write it in the header.
                    if (*job).jobID != 0 {
                        jobParams.fParams.checksumFlag = 0;
                    }

                    // Don't run LDM for the chunks, since we handle it externally
                    jobParams.ldmParams.enableLdm = ParamSwitch::Disable;
                    // Correct nbWorkers to 0.
                    jobParams.nbWorkers = 0;

                    // init

                    // Perform serial step as early as possible
                    ZSTDMT_serialState_genSequences(
                        (*job).serial,
                        &mut rawSeqStore,
                        (*job).src,
                        (*job).jobID,
                    );

                    if !((*job).cdict).is_null() {
                        if let Err(err) = ZSTD_compressBegin_advanced_internal(
                            cctx,
                            core::ptr::null(),
                            0,
                            ZSTD_dct_auto,
                            DictTableLoadMethod::Fast,
                            (*job).cdict,
                            &jobParams,
                            (*job).fullFrameSize,
                        ) {
                            let guard = (*job).job_mutex.lock().unwrap();
                            (*job).cSize = err.to_error_code();
                            drop(guard);
                            current_block = 17100290475540901977;
                        } else {
                            current_block = 16738040538446813684;
                        }
                    } else {
                        let pledgedSrcSize = if (*job).firstJob != 0 {
                            (*job).fullFrameSize
                        } else {
                            (*job).src.size as core::ffi::c_ulonglong
                        };
                        let forceWindowError = ZSTD_CCtxParams_setParameter(
                            &mut jobParams,
                            ZSTD_cParameter::ZSTD_c_forceMaxWindow,
                            core::ffi::c_int::from((*job).firstJob == 0),
                        );
                        if ERR_isError(forceWindowError) {
                            let guard = (*job).job_mutex.lock().unwrap();
                            (*job).cSize = forceWindowError;
                            drop(guard);
                            current_block = 17100290475540901977;
                        } else {
                            if (*job).firstJob == 0 {
                                let err = ZSTD_CCtxParams_setParameter(
                                    &mut jobParams,
                                    ZSTD_cParameter::ZSTD_c_deterministicRefPrefix,
                                    0,
                                );
                                if ERR_isError(err) {
                                    let guard = (*job).job_mutex.lock().unwrap();
                                    (*job).cSize = err;
                                    drop(guard);
                                    current_block = 17100290475540901977;
                                } else {
                                    current_block = 2543120759711851213;
                                }
                            } else {
                                current_block = 2543120759711851213;
                            }

                            match current_block {
                                17100290475540901977 => {}
                                _ => {
                                    if let Err(err) = ZSTD_compressBegin_advanced_internal(
                                        cctx,
                                        (*job).prefix.start,
                                        (*job).prefix.size,
                                        ZSTD_dct_rawContent,
                                        DictTableLoadMethod::Fast,
                                        core::ptr::null(),
                                        &jobParams,
                                        pledgedSrcSize as core::ffi::c_ulonglong,
                                    ) {
                                        let guard = (*job).job_mutex.lock().unwrap();
                                        (*job).cSize = err.to_error_code();
                                        drop(guard);
                                        current_block = 17100290475540901977;
                                    } else {
                                        current_block = 16738040538446813684;
                                    }
                                }
                            }
                        }
                    }
                    match current_block {
                        17100290475540901977 => {}
                        _ => {
                            // External Sequences can only be applied after CCtx initialization
                            ZSTDMT_serialState_applySequences((*job).serial, cctx, &rawSeqStore);

                            if (*job).firstJob == 0 {
                                // flush and overwrite frame header when it's not first job
                                let hSize = ZSTD_compressContinue_public(
                                    cctx,
                                    dstBuff.start,
                                    dstBuff.capacity,
                                    (*job).src.start,
                                    0,
                                );
                                if ERR_isError(hSize) {
                                    let guard = (*job).job_mutex.lock().unwrap();
                                    (*job).cSize = hSize;
                                    drop(guard);
                                    current_block = 17100290475540901977;
                                } else {
                                    ZSTD_invalidateRepCodes(cctx);
                                    current_block = 6560072651652764009;
                                }
                            } else {
                                current_block = 6560072651652764009;
                            }

                            match current_block {
                                17100290475540901977 => {}
                                _ => {
                                    // compress the entire job by smaller chunks, for better granularity
                                    let chunkSize = (4 * ZSTD_BLOCKSIZE_MAX) as usize;
                                    let nbChunks = (((*job).src.size)
                                        .wrapping_add(chunkSize.wrapping_sub(1))
                                        / chunkSize)
                                        as core::ffi::c_int;
                                    let mut ip = (*job).src.start as *const u8;
                                    let ostart = dstBuff.start as *mut u8;
                                    let mut op = ostart;
                                    let oend = op.add(dstBuff.capacity);

                                    if size_of::<usize>() > size_of::<i32>() {
                                        /* check overflow */
                                        assert!(
                                            ((*job).src.size as u64)
                                                < i32::MAX as u64 * chunkSize as u64
                                        );
                                    }
                                    assert_eq!((*job).cSize, 0);

                                    let mut chunkNb = 1;
                                    loop {
                                        if chunkNb >= nbChunks {
                                            current_block = 851619935621435220;
                                            break;
                                        }
                                        let cSize = ZSTD_compressContinue_public(
                                            cctx,
                                            op as *mut core::ffi::c_void,
                                            oend.offset_from_unsigned(op),
                                            ip as *const core::ffi::c_void,
                                            chunkSize,
                                        );
                                        if ERR_isError(cSize) {
                                            let guard = (*job).job_mutex.lock().unwrap();
                                            (*job).cSize = cSize;
                                            drop(guard);
                                            current_block = 17100290475540901977;
                                            break;
                                        } else {
                                            ip = ip.add(chunkSize);
                                            op = op.add(cSize);
                                            // stats
                                            let guard = (*job).job_mutex.lock().unwrap();
                                            (*job).cSize = ((*job).cSize).wrapping_add(cSize);
                                            (*job).consumed = chunkSize * chunkNb as usize;
                                            (*job).job_cond.notify_one(); // warns some more data is ready to be flushed
                                            drop(guard);
                                            chunkNb += 1;
                                        }
                                    }

                                    match current_block {
                                        17100290475540901977 => {}
                                        _ => {
                                            // last block
                                            if (nbChunks > 0) || ((*job).lastJob > 0) {
                                                // must output a "last block" flag
                                                let lastBlockSize1 =
                                                    (*job).src.size & chunkSize.wrapping_sub(1);
                                                let lastBlockSize = if (lastBlockSize1 == 0)
                                                    & ((*job).src.size >= chunkSize)
                                                {
                                                    chunkSize
                                                } else {
                                                    lastBlockSize1
                                                };
                                                let cSize_0 = if (*job).lastJob != 0 {
                                                    ZSTD_compressEnd_internal(
                                                        cctx,
                                                        op as *mut core::ffi::c_void,
                                                        oend.offset_from_unsigned(op),
                                                        ip as *const core::ffi::c_void,
                                                        lastBlockSize,
                                                    )
                                                } else {
                                                    ZSTD_compressContinue_internal(
                                                        cctx,
                                                        op as *mut core::ffi::c_void,
                                                        oend.offset_from_unsigned(op),
                                                        ip as *const core::ffi::c_void,
                                                        lastBlockSize,
                                                        true,
                                                        false,
                                                    )
                                                };
                                                match cSize_0 {
                                                    Ok(cSize) => {
                                                        lastCBlockSize = cSize;
                                                        current_block = 200744462051969938;
                                                    }
                                                    Err(err) => {
                                                        let guard =
                                                            (*job).job_mutex.lock().unwrap();
                                                        (*job).cSize = err.to_error_code();
                                                        drop(guard);
                                                        current_block = 17100290475540901977;
                                                    }
                                                }
                                            } else {
                                                current_block = 200744462051969938;
                                            }
                                            match current_block {
                                                17100290475540901977 => {}
                                                _ => {
                                                    if (*job).firstJob == 0 {
                                                        // Double check that we don't have an ext-dict, because then our
                                                        // repcode invalidation doesn't work.
                                                        assert!(!(*cctx)
                                                            .blockState
                                                            .matchState
                                                            .window
                                                            .has_ext_dict());
                                                    }
                                                    ZSTD_CCtx_trace(cctx, 0);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    ZSTDMT_serialState_ensureFinished((*job).serial, (*job).jobID, (*job).cSize);

    // release resources
    ZSTDMT_releaseSeq((*job).seqPool, rawSeqStore);
    ZSTDMT_releaseCCtx((*job).cctxPool, cctx);

    // report
    let _guard = (*job).job_mutex.lock().unwrap();
    if ERR_isError((*job).cSize) {
        assert_eq!(lastCBlockSize, 0);
    }
    (*job).cSize = ((*job).cSize).wrapping_add(lastCBlockSize);
    (*job).consumed = (*job).src.size; // when job->consumed == job->src.size , compression job is presumed completed
    (*job).job_cond.notify_one();
}

const kNullRoundBuff: RoundBuff_t = RoundBuff_t {
    buffer: core::ptr::null_mut(),
    capacity: 0,
    pos: 0,
};

const RSYNC_LENGTH: u8 = 32;

/// Don't create chunks smaller than the zstd block size.
/// This stops us from regressing compression ratio too much,
/// and ensures our output fits in ZSTD_compressBound().
///
/// If this is shrunk < ZSTD_BLOCKSIZELOG_MIN then
/// ZSTD_COMPRESSBOUND() will need to be updated.
const RSYNC_MIN_BLOCK_LOG: core::ffi::c_int = ZSTD_BLOCKSIZELOG_MAX;
const RSYNC_MIN_BLOCK_SIZE: core::ffi::c_int = 1 << RSYNC_MIN_BLOCK_LOG;

unsafe fn ZSTDMT_freeJobsTable(
    jobTable: *mut ZSTDMT_jobDescription,
    nbJobs: u32,
    cMem: ZSTD_customMem,
) {
    if jobTable.is_null() {
        return;
    }

    for jobNb in 0..nbJobs {
        core::ptr::drop_in_place(core::ptr::addr_of_mut!(
            (*jobTable.offset(jobNb as isize)).job_mutex
        ));
        core::ptr::drop_in_place(core::ptr::addr_of_mut!(
            (*jobTable.offset(jobNb as isize)).job_cond
        ));
    }

    ZSTD_customFree(
        jobTable as *mut core::ffi::c_void,
        (nbJobs as usize).wrapping_mul(size_of::<ZSTDMT_jobDescription>()),
        cMem,
    );
}

/// Allocate and init a job table.
/// Updates *nbJobsPtr to the next power of 2 value, as size of table.
unsafe fn ZSTDMT_createJobsTable(
    nbJobsPtr: &mut u32,
    cMem: ZSTD_customMem,
) -> *mut ZSTDMT_jobDescription {
    let nbJobsLog2 = ZSTD_highbit32(*nbJobsPtr).wrapping_add(1);
    let nbJobs = (1 << nbJobsLog2) as u32;

    let jobTable = ZSTD_customCalloc(
        (nbJobs as usize).wrapping_mul(size_of::<ZSTDMT_jobDescription>()),
        cMem,
    ) as *mut ZSTDMT_jobDescription;
    if jobTable.is_null() {
        return core::ptr::null_mut();
    }

    *nbJobsPtr = nbJobs;
    for jobNb in 0..nbJobs {
        core::ptr::write(
            core::ptr::addr_of_mut!((*jobTable.offset(jobNb as isize)).job_mutex),
            Mutex::new(()),
        );
        core::ptr::write(
            core::ptr::addr_of_mut!((*jobTable.offset(jobNb as isize)).job_cond),
            Condvar::new(),
        );
    }

    jobTable
}

unsafe fn ZSTDMT_expandJobsTable(mtctx: *mut ZSTDMT_CCtx, nbWorkers: u32) -> Result<(), Error> {
    let mut nbJobs = nbWorkers.wrapping_add(2);
    if nbJobs > ((*mtctx).jobIDMask).wrapping_add(1) {
        // need more job capacity
        ZSTDMT_freeJobsTable(
            (*mtctx).jobs,
            ((*mtctx).jobIDMask).wrapping_add(1),
            (*mtctx).cMem,
        );
        (*mtctx).jobIDMask = 0;
        (*mtctx).jobs = ZSTDMT_createJobsTable(&mut nbJobs, (*mtctx).cMem);
        if ((*mtctx).jobs).is_null() {
            return Err(Error::memory_allocation);
        }
        (*mtctx).jobIDMask = nbJobs.wrapping_sub(1);
    }

    Ok(())
}

/// Internal use only.
unsafe fn ZSTDMT_CCtxParam_setNbWorkers(
    params: &mut ZSTD_CCtx_params,
    nbWorkers: core::ffi::c_uint,
) -> usize {
    ZSTD_CCtxParams_setParameter(
        params,
        ZSTD_cParameter::ZSTD_c_nbWorkers,
        nbWorkers as core::ffi::c_int,
    )
}

#[inline]
unsafe fn ZSTDMT_createCCtx_advanced_internal(
    mut nbWorkers: core::ffi::c_uint,
    cMem: ZSTD_customMem,
    pool: *mut ZSTD_threadPool,
) -> *mut ZSTDMT_CCtx {
    let mut nbJobs = nbWorkers.wrapping_add(2);
    if nbWorkers < 1 {
        return core::ptr::null_mut();
    }
    nbWorkers = nbWorkers.min(ZSTDMT_NBWORKERS_MAX as core::ffi::c_uint);

    let mtctx = ZSTD_customCalloc(size_of::<ZSTDMT_CCtx>(), cMem) as *mut ZSTDMT_CCtx;
    if mtctx.is_null() {
        return core::ptr::null_mut();
    }
    ZSTDMT_CCtxParam_setNbWorkers(&mut (*mtctx).params, nbWorkers);
    (*mtctx).cMem = cMem;
    (*mtctx).allJobsCompleted = 1;
    if !pool.is_null() {
        (*mtctx).factory = pool;
        (*mtctx).providedFactory = true;
    } else {
        (*mtctx).factory = POOL_create_advanced(nbWorkers as usize, 0, cMem);
        (*mtctx).providedFactory = false;
    }
    (*mtctx).jobs = ZSTDMT_createJobsTable(&mut nbJobs, cMem);
    (*mtctx).jobIDMask = nbJobs.wrapping_sub(1);
    (*mtctx).bufPool = ZSTDMT_createBufferPool(2u32.wrapping_mul(nbWorkers).wrapping_add(3), cMem);
    (*mtctx).cctxPool = ZSTDMT_createCCtxPool(nbWorkers as core::ffi::c_int, cMem);
    (*mtctx).seqPool = ZSTDMT_createSeqPool(nbWorkers, cMem);
    core::ptr::write(&raw mut (*mtctx).serial, SerialState::default());
    (*mtctx).roundBuff = kNullRoundBuff;
    if ((*mtctx).factory).is_null() as core::ffi::c_int
        | ((*mtctx).jobs).is_null() as core::ffi::c_int
        | ((*mtctx).bufPool).is_null() as core::ffi::c_int
        | ((*mtctx).cctxPool).is_null() as core::ffi::c_int
        | ((*mtctx).seqPool).is_null() as core::ffi::c_int
        != 0
    {
        ZSTDMT_freeCCtx(mtctx);
        return core::ptr::null_mut();
    }
    mtctx
}

pub unsafe fn ZSTDMT_createCCtx_advanced(
    nbWorkers: core::ffi::c_uint,
    cMem: ZSTD_customMem,
    pool: *mut ZSTD_threadPool,
) -> *mut ZSTDMT_CCtx {
    ZSTDMT_createCCtx_advanced_internal(nbWorkers, cMem, pool)
}

/// Note: ensure all workers are killed first!
unsafe fn ZSTDMT_releaseAllJobResources(mtctx: *mut ZSTDMT_CCtx) {
    for jobID in 0..(*mtctx).jobIDMask + 1 {
        // Copy the mutex/cond out
        let mutex = core::ptr::read(core::ptr::addr_of!(
            (*((*mtctx).jobs).offset(jobID as isize)).job_mutex
        ));
        let cond = core::ptr::read(core::ptr::addr_of!(
            (*((*mtctx).jobs).offset(jobID as isize)).job_cond
        ));

        ZSTDMT_releaseBuffer(
            (*mtctx).bufPool,
            (*((*mtctx).jobs).offset(jobID as isize)).dstBuff,
        );

        // Clear the job description, but keep the mutex/cond
        ptr::write_bytes(
            &mut *((*mtctx).jobs).offset(jobID as isize) as *mut ZSTDMT_jobDescription as *mut u8,
            0,
            size_of::<ZSTDMT_jobDescription>(),
        );
        core::ptr::write(
            core::ptr::addr_of_mut!((*((*mtctx).jobs).offset(jobID as isize)).job_mutex),
            mutex,
        );
        core::ptr::write(
            core::ptr::addr_of_mut!((*((*mtctx).jobs).offset(jobID as isize)).job_cond),
            cond,
        );
    }

    (*mtctx).inBuff.buffer = g_nullBuffer;
    (*mtctx).inBuff.filled = 0;
    (*mtctx).allJobsCompleted = 1;
}

unsafe fn ZSTDMT_waitForAllJobsCompleted(mtctx: *mut ZSTDMT_CCtx) {
    while (*mtctx).doneJobID < (*mtctx).nextJobID {
        let jobID = (*mtctx).doneJobID & (*mtctx).jobIDMask;
        let mut guard = (*((*mtctx).jobs).offset(jobID as isize))
            .job_mutex
            .lock()
            .unwrap();
        while (*((*mtctx).jobs).offset(jobID as isize)).consumed
            < (*((*mtctx).jobs).offset(jobID as isize)).src.size
        {
            guard = (*((*mtctx).jobs).offset(jobID as isize))
                .job_cond
                .wait(guard)
                .unwrap();
        }
        (*mtctx).doneJobID += 1;
    }
}

pub unsafe fn ZSTDMT_freeCCtx(mtctx: *mut ZSTDMT_CCtx) -> usize {
    if mtctx.is_null() {
        return 0;
    }
    if !(*mtctx).providedFactory {
        POOL_free((*mtctx).factory); // stop and free worker threads
    }
    ZSTDMT_releaseAllJobResources(mtctx); // release job resources into pools first
    ZSTDMT_freeJobsTable(
        (*mtctx).jobs,
        ((*mtctx).jobIDMask).wrapping_add(1),
        (*mtctx).cMem,
    );
    ZSTDMT_freeBufferPool((*mtctx).bufPool);
    ZSTDMT_freeCCtxPool((*mtctx).cctxPool);
    ZSTDMT_freeSeqPool((*mtctx).seqPool);
    ZSTDMT_serialState_free(&mut (*mtctx).serial);
    ZSTD_freeCDict((*mtctx).cdictLocal);
    if !((*mtctx).roundBuff.buffer).is_null() {
        ZSTD_customFree(
            (*mtctx).roundBuff.buffer as *mut core::ffi::c_void,
            (*mtctx).roundBuff.capacity,
            (*mtctx).cMem,
        );
    }
    ZSTD_customFree(
        mtctx as *mut core::ffi::c_void,
        size_of::<ZSTDMT_CCtx>(),
        (*mtctx).cMem,
    );
    0
}

pub unsafe fn ZSTDMT_sizeof_CCtx(mtctx: *mut ZSTDMT_CCtx) -> usize {
    if mtctx.is_null() {
        return 0;
    }
    size_of::<ZSTDMT_CCtx>()
        .wrapping_add(POOL_sizeof((*mtctx).factory))
        .wrapping_add(ZSTDMT_sizeof_bufferPool((*mtctx).bufPool))
        .wrapping_add(
            (((*mtctx).jobIDMask).wrapping_add(1) as usize)
                .wrapping_mul(size_of::<ZSTDMT_jobDescription>()),
        )
        .wrapping_add(ZSTDMT_sizeof_CCtxPool((*mtctx).cctxPool))
        .wrapping_add(ZSTDMT_sizeof_seqPool((*mtctx).seqPool))
        .wrapping_add(ZSTD_sizeof_CDict((*mtctx).cdictLocal))
        .wrapping_add((*mtctx).roundBuff.capacity)
}

unsafe fn ZSTDMT_resize(
    mtctx: *mut ZSTDMT_CCtx,
    nbWorkers: core::ffi::c_uint,
) -> Result<(), Error> {
    if POOL_resize((*mtctx).factory, nbWorkers as usize) != 0 {
        return Err(Error::memory_allocation);
    }
    ZSTDMT_expandJobsTable(mtctx, nbWorkers)?;
    (*mtctx).bufPool = ZSTDMT_expandBufferPool(
        (*mtctx).bufPool,
        2u32.wrapping_mul(nbWorkers).wrapping_add(3),
    );
    if ((*mtctx).bufPool).is_null() {
        return Err(Error::memory_allocation);
    }
    (*mtctx).cctxPool = ZSTDMT_expandCCtxPool((*mtctx).cctxPool, nbWorkers as core::ffi::c_int);
    if ((*mtctx).cctxPool).is_null() {
        return Err(Error::memory_allocation);
    }
    (*mtctx).seqPool = ZSTDMT_expandSeqPool((*mtctx).seqPool, nbWorkers);
    if ((*mtctx).seqPool).is_null() {
        return Err(Error::memory_allocation);
    }
    ZSTDMT_CCtxParam_setNbWorkers(&mut (*mtctx).params, nbWorkers);
    Ok(())
}

/// Updates a selected set of compression parameters, remaining compatible with currently active frame.
/// New parameters will be applied to next compression job.
pub unsafe fn ZSTDMT_updateCParams_whileCompressing(
    mtctx: *mut ZSTDMT_CCtx,
    cctxParams: &ZSTD_CCtx_params,
) {
    let saved_wlog = (*mtctx).params.cParams.windowLog; // Do not modify windowLog while compressing
    let compressionLevel = cctxParams.compressionLevel;
    (*mtctx).params.compressionLevel = compressionLevel;

    let mut cParams = ZSTD_getCParamsFromCCtxParams_internal(
        cctxParams,
        ZSTD_CONTENTSIZE_UNKNOWN,
        0,
        CParamMode::NoAttachDict,
    );
    cParams.windowLog = saved_wlog;
    (*mtctx).params.cParams = cParams;
}

/// Tells how much data has been consumed (input) and produced (output) for current frame.
/// Able to count progression inside worker threads.
/// Note: mutex will be acquired during statistics collection inside workers.
pub unsafe fn ZSTDMT_getFrameProgression(mtctx: *mut ZSTDMT_CCtx) -> ZSTD_frameProgression {
    let mut fps = ZSTD_frameProgression {
        ingested: ((*mtctx).consumed)
            .wrapping_add((*mtctx).inBuff.filled as core::ffi::c_ulonglong),
        consumed: (*mtctx).consumed,
        produced: (*mtctx).produced,
        flushed: (*mtctx).produced,
        currentJobID: (*mtctx).nextJobID,
        nbActiveWorkers: 0,
    };

    let lastJobNb = ((*mtctx).nextJobID).wrapping_add((*mtctx).jobReady as core::ffi::c_uint);
    for jobNb in (*mtctx).doneJobID..lastJobNb {
        let wJobID = jobNb & (*mtctx).jobIDMask;
        let jobPtr: *mut ZSTDMT_jobDescription = ((*mtctx).jobs).offset(wJobID as isize);
        let _guard = (*jobPtr).job_mutex.lock().unwrap();
        let cResult = (*jobPtr).cSize;
        let produced = if ERR_isError(cResult) { 0 } else { cResult };
        let flushed = if ERR_isError(cResult) {
            0
        } else {
            (*jobPtr).dstFlushed
        };
        fps.ingested = fps.ingested.wrapping_add((*jobPtr).src.size as u64);
        fps.consumed = fps.consumed.wrapping_add((*jobPtr).consumed as u64);
        fps.produced = fps.produced.wrapping_add(produced as u64);
        fps.flushed = fps.flushed.wrapping_add(flushed as u64);
        fps.nbActiveWorkers = fps.nbActiveWorkers.wrapping_add(core::ffi::c_uint::from(
            (*jobPtr).consumed < (*jobPtr).src.size,
        ));
    }
    fps
}

pub unsafe fn ZSTDMT_toFlushNow(mtctx: *mut ZSTDMT_CCtx) -> usize {
    let jobID = (*mtctx).doneJobID;
    if jobID == (*mtctx).nextJobID {
        return 0; // no active job => nothing to flush
    }

    // look into oldest non-fully-flushed job
    let wJobID = jobID & (*mtctx).jobIDMask;
    let jobPtr: *mut ZSTDMT_jobDescription =
        &mut *((*mtctx).jobs).offset(wJobID as isize) as *mut ZSTDMT_jobDescription;
    let _guard = (*jobPtr).job_mutex.lock().unwrap();

    let cResult = (*jobPtr).cSize;
    let produced = if ERR_isError(cResult) { 0 } else { cResult };
    let flushed = if ERR_isError(cResult) {
        0
    } else {
        (*jobPtr).dstFlushed
    };
    let toFlush = produced.wrapping_sub(flushed);
    // if toFlush==0, nothing is available to flush.
    // However, jobID is expected to still be active:
    // if jobID was already completed and fully flushed,
    // ZSTDMT_flushProduced() should have already moved onto next job.
    // Therefore, some input has not yet been consumed.
    if toFlush == 0 {
        assert!((*jobPtr).consumed < (*jobPtr).src.size);
    }

    toFlush
}

fn ZSTDMT_computeTargetJobLog(params: &ZSTD_CCtx_params) -> core::ffi::c_uint {
    let jobLog = if params.ldmParams.enableLdm == ParamSwitch::Enable {
        // In Long Range Mode, the windowLog is typically oversized.
        // In which case, it's preferable to determine the jobSize
        // based on cycleLog instead.
        ZSTD_cycleLog(params.cParams.chainLog, params.cParams.strategy)
            .wrapping_add(3)
            .max(21)
    } else {
        params.cParams.windowLog.wrapping_add(2).max(20)
    };
    jobLog.min(ZSTDMT_JOBLOG_MAX)
}

fn ZSTDMT_overlapLog_default(strat: ZSTD_strategy) -> core::ffi::c_int {
    match strat {
        ZSTD_btultra2 => 9,
        ZSTD_btultra | ZSTD_btopt => 8,
        ZSTD_btlazy2 | ZSTD_lazy2 => 7,
        _ => 6,
    }
}

fn ZSTDMT_overlapLog(ovlog: core::ffi::c_int, strat: ZSTD_strategy) -> core::ffi::c_int {
    if ovlog == 0 {
        return ZSTDMT_overlapLog_default(strat);
    }
    ovlog
}

fn ZSTDMT_computeOverlapSize(params: &ZSTD_CCtx_params) -> usize {
    let overlapRLog = 9 - ZSTDMT_overlapLog(params.overlapLog, params.cParams.strategy);
    let mut ovLog = (if overlapRLog >= 8 {
        0
    } else {
        (params.cParams.windowLog).wrapping_sub(overlapRLog as core::ffi::c_uint)
    }) as core::ffi::c_int;
    if params.ldmParams.enableLdm == ParamSwitch::Enable {
        // In Long Range Mode, the windowLog is typically oversized.
        // In which case, it's preferable to determine the jobSize
        // based on chainLog instead.
        // Then, ovLog becomes a fraction of the jobSize, rather than windowSize
        ovLog = (params
            .cParams
            .windowLog
            .min(ZSTDMT_computeTargetJobLog(params).wrapping_sub(2)))
        .wrapping_sub(overlapRLog as core::ffi::c_uint) as core::ffi::c_int;
    }
    if ovLog == 0 {
        0
    } else {
        1 << ovLog
    }
}

pub unsafe fn ZSTDMT_initCStream_internal(
    mtctx: *mut ZSTDMT_CCtx,
    dict: *const core::ffi::c_void,
    dictSize: usize,
    dictContentType: ZSTD_dictContentType_e,
    cdict: *const ZSTD_CDict,
    mut params: ZSTD_CCtx_params,
    pledgedSrcSize: core::ffi::c_ulonglong,
) -> Result<(), Error> {
    // init
    if params.nbWorkers != (*mtctx).params.nbWorkers {
        ZSTDMT_resize(mtctx, params.nbWorkers as core::ffi::c_uint)?;
    }

    if params.jobSize != 0 {
        params.jobSize = params
            .jobSize
            .clamp(ZSTDMT_JOBSIZE_MIN as usize, ZSTDMT_JOBSIZE_MAX as usize);
    }

    if (*mtctx).allJobsCompleted == 0 {
        // previous compression not correctly finished
        ZSTDMT_waitForAllJobsCompleted(mtctx);
        ZSTDMT_releaseAllJobResources(mtctx);
        (*mtctx).allJobsCompleted = 1;
    }

    (*mtctx).params = params;
    (*mtctx).frameContentSize = pledgedSrcSize;
    ZSTD_freeCDict((*mtctx).cdictLocal);
    if !dict.is_null() {
        (*mtctx).cdictLocal = ZSTD_createCDict_advanced(
            dict,
            dictSize,
            ZSTD_dlm_byCopy,
            dictContentType, // note: a loadPrefix becomes an internal CDict
            params.cParams,
            (*mtctx).cMem,
        );
        (*mtctx).cdict = (*mtctx).cdictLocal;
        if ((*mtctx).cdictLocal).is_null() {
            return Err(Error::memory_allocation);
        }
    } else {
        (*mtctx).cdictLocal = core::ptr::null_mut();
        (*mtctx).cdict = cdict;
    }

    (*mtctx).targetPrefixSize = ZSTDMT_computeOverlapSize(&params);
    (*mtctx).targetSectionSize = params.jobSize;
    if (*mtctx).targetSectionSize == 0 {
        (*mtctx).targetSectionSize = (1 << ZSTDMT_computeTargetJobLog(&params)) as usize;
    }

    if params.rsyncable != 0 {
        // Aim for the targetSectionSize as the average job size.
        let jobSizeKB = ((*mtctx).targetSectionSize >> 10) as u32;
        let rsyncBits = ZSTD_highbit32(jobSizeKB).wrapping_add(10);
        // We refuse to create jobs < RSYNC_MIN_BLOCK_SIZE bytes, so make sure our
        // expected job size is at least 4x larger.
        (*mtctx).rsync.hash = 0;
        (*mtctx).rsync.hitMask = (1u64 << rsyncBits).wrapping_sub(1);
        (*mtctx).rsync.primePower = ZSTD_rollingHash_primePower(u32::from(RSYNC_LENGTH));
    }
    if (*mtctx).targetSectionSize < (*mtctx).targetPrefixSize {
        (*mtctx).targetSectionSize = (*mtctx).targetPrefixSize; // job size must be >= overlap size
    }
    ZSTDMT_setBufferSize(
        (*mtctx).bufPool,
        ZSTD_compressBound((*mtctx).targetSectionSize),
    );

    // If ldm is enabled we need windowSize space.
    let windowSize = (if (*mtctx).params.ldmParams.enableLdm == ParamSwitch::Enable {
        1 << (*mtctx).params.cParams.windowLog
    } else {
        0
    }) as usize;
    // Two buffers of slack, plus extra space for the overlap.
    // This is the minimum slack that LDM works with. One extra because
    // flush might waste up to targetSectionSize-1 bytes. Another extra
    // for the overlap (if > 0), then one to fill which doesn't overlap
    // with the LDM window.
    let nbSlackBuffers = 2 + usize::from((*mtctx).targetPrefixSize > 0);
    let slackSize = (*mtctx).targetSectionSize * nbSlackBuffers;
    // Compute the total size, and always have enough slack
    let nbWorkers = ((*mtctx).params.nbWorkers.max(1)) as usize;
    let sectionsSize = (*mtctx).targetSectionSize * nbWorkers;
    let capacity = windowSize.max(sectionsSize).wrapping_add(slackSize);
    if (*mtctx).roundBuff.capacity < capacity {
        if !((*mtctx).roundBuff.buffer).is_null() {
            ZSTD_customFree(
                (*mtctx).roundBuff.buffer as *mut core::ffi::c_void,
                (*mtctx).roundBuff.capacity,
                (*mtctx).cMem,
            );
        }
        (*mtctx).roundBuff.buffer = ZSTD_customMalloc(capacity, (*mtctx).cMem) as *mut u8;
        if ((*mtctx).roundBuff.buffer).is_null() {
            (*mtctx).roundBuff.capacity = 0;
            return Err(Error::memory_allocation);
        }
        (*mtctx).roundBuff.capacity = capacity;
    }

    (*mtctx).roundBuff.pos = 0;
    (*mtctx).inBuff.buffer = g_nullBuffer;
    (*mtctx).inBuff.filled = 0;
    (*mtctx).inBuff.prefix = Range::default();
    (*mtctx).doneJobID = 0;
    (*mtctx).nextJobID = 0;
    (*mtctx).frameEnded = 0;
    (*mtctx).allJobsCompleted = 0;
    (*mtctx).consumed = 0;
    (*mtctx).produced = 0;

    // update dictionary
    ZSTD_freeCDict((*mtctx).cdictLocal);
    (*mtctx).cdictLocal = core::ptr::null_mut();
    (*mtctx).cdict = core::ptr::null();
    if !dict.is_null() {
        if dictContentType == ZSTD_dct_rawContent {
            (*mtctx).inBuff.prefix.start = dict as *const u8 as *const core::ffi::c_void;
            (*mtctx).inBuff.prefix.size = dictSize;
        } else {
            // note: a loadPrefix becomes an internal CDict
            (*mtctx).cdictLocal = ZSTD_createCDict_advanced(
                dict,
                dictSize,
                ZSTD_dlm_byRef,
                dictContentType,
                params.cParams,
                (*mtctx).cMem,
            );
            (*mtctx).cdict = (*mtctx).cdictLocal;
            if ((*mtctx).cdictLocal).is_null() {
                return Err(Error::memory_allocation);
            }
        }
    } else {
        (*mtctx).cdict = cdict;
    }

    ZSTDMT_serialState_reset(
        &mut (*mtctx).serial,
        (*mtctx).seqPool,
        params,
        (*mtctx).targetSectionSize,
        dict,
        dictSize,
        dictContentType,
    )
}

/// Write a single empty block with an end-of-frame to finish a frame.
/// Job must be created from streaming variant.
/// This function is always successful if expected conditions are fulfilled.
unsafe fn ZSTDMT_writeLastEmptyBlock(job: *mut ZSTDMT_jobDescription) {
    (*job).dstBuff = ZSTDMT_getBuffer((*job).bufPool);
    if ((*job).dstBuff.start).is_null() {
        (*job).cSize = Error::memory_allocation.to_error_code();
        return;
    }
    (*job).src = Range::default();
    (*job).cSize = ZSTD_writeLastEmptyBlock((*job).dstBuff.start, (*job).dstBuff.capacity);
}

unsafe fn ZSTDMT_createCompressionJob(
    mtctx: *mut ZSTDMT_CCtx,
    srcSize: usize,
    endOp: ZSTD_EndDirective,
) {
    let jobID = (*mtctx).nextJobID & (*mtctx).jobIDMask;
    let endFrame = endOp == ZSTD_e_end;

    if (*mtctx).nextJobID > ((*mtctx).doneJobID).wrapping_add((*mtctx).jobIDMask) {
        // will not create new job: table is full
        return;
    }

    if (*mtctx).jobReady == 0 {
        let src = (*mtctx).inBuff.buffer.start as *const u8;
        (*((*mtctx).jobs).offset(jobID as isize)).src.start = src as *const core::ffi::c_void;
        (*((*mtctx).jobs).offset(jobID as isize)).src.size = srcSize;
        (*((*mtctx).jobs).offset(jobID as isize)).prefix = (*mtctx).inBuff.prefix;
        (*((*mtctx).jobs).offset(jobID as isize)).consumed = 0;
        (*((*mtctx).jobs).offset(jobID as isize)).cSize = 0;
        (*((*mtctx).jobs).offset(jobID as isize)).params = (*mtctx).params;
        (*((*mtctx).jobs).offset(jobID as isize)).cdict = if (*mtctx).nextJobID == 0 {
            (*mtctx).cdict
        } else {
            core::ptr::null()
        };
        (*((*mtctx).jobs).offset(jobID as isize)).fullFrameSize = (*mtctx).frameContentSize;
        (*((*mtctx).jobs).offset(jobID as isize)).dstBuff = g_nullBuffer;
        (*((*mtctx).jobs).offset(jobID as isize)).cctxPool = (*mtctx).cctxPool;
        (*((*mtctx).jobs).offset(jobID as isize)).bufPool = (*mtctx).bufPool;
        (*((*mtctx).jobs).offset(jobID as isize)).seqPool = (*mtctx).seqPool;
        (*((*mtctx).jobs).offset(jobID as isize)).serial = &mut (*mtctx).serial;
        (*((*mtctx).jobs).offset(jobID as isize)).jobID = (*mtctx).nextJobID;
        (*((*mtctx).jobs).offset(jobID as isize)).firstJob =
            core::ffi::c_uint::from((*mtctx).nextJobID == 0);
        (*((*mtctx).jobs).offset(jobID as isize)).lastJob = endFrame as core::ffi::c_uint;
        (*((*mtctx).jobs).offset(jobID as isize)).frameChecksumNeeded = core::ffi::c_uint::from(
            (*mtctx).params.fParams.checksumFlag != 0 && endFrame && (*mtctx).nextJobID > 0,
        );
        (*((*mtctx).jobs).offset(jobID as isize)).dstFlushed = 0;

        // Update the round buffer pos and clear the input buffer to be reset
        (*mtctx).roundBuff.pos = ((*mtctx).roundBuff.pos).wrapping_add(srcSize);
        (*mtctx).inBuff.buffer = g_nullBuffer;
        (*mtctx).inBuff.filled = 0;

        // Set the prefix for next job
        if !endFrame {
            let newPrefixSize = srcSize.min((*mtctx).targetPrefixSize);
            (*mtctx).inBuff.prefix.start =
                src.add(srcSize).sub(newPrefixSize) as *const core::ffi::c_void;
            (*mtctx).inBuff.prefix.size = newPrefixSize;
        } else {
            // endFrame==1 => no need for another input buffer
            (*mtctx).inBuff.prefix = Range::default();
            (*mtctx).frameEnded = endFrame as core::ffi::c_uint;
            if (*mtctx).nextJobID == 0 {
                // single job exception: checksum is already calculated directly within worker thread
                (*mtctx).params.fParams.checksumFlag = 0;
            }
        }

        if srcSize == 0 && (*mtctx).nextJobID > 0 {
            // single job must also write frame header
            ZSTDMT_writeLastEmptyBlock(((*mtctx).jobs).offset(jobID as isize));
            (*mtctx).nextJobID = ((*mtctx).nextJobID).wrapping_add(1);
            return;
        }
    }

    if POOL_tryAdd(
        (*mtctx).factory,
        ZSTDMT_compressionJob,
        &mut *((*mtctx).jobs).offset(jobID as isize) as *mut ZSTDMT_jobDescription
            as *mut core::ffi::c_void,
    ) != 0
    {
        (*mtctx).nextJobID = ((*mtctx).nextJobID).wrapping_add(1);
        (*mtctx).jobReady = 0;
    } else {
        (*mtctx).jobReady = 1;
    }
}

/// Flush whatever data has been produced but not yet flushed in current job.
/// Move to next job if current one is fully flushed.
/// `output`: `pos` will be updated with amount of data flushed.
/// `blockToFlush`: if true, the function will block and wait if there is no data available to flush.
///
/// # Returns
///
/// The amount of data remaining within the internal buffer:
/// - 0 if no more
/// - 1 if unknown but > 0
/// - or an error
unsafe fn ZSTDMT_flushProduced(
    mtctx: *mut ZSTDMT_CCtx,
    output: *mut ZSTD_outBuffer,
    blockToFlush: bool,
    end: ZSTD_EndDirective,
) -> Result<usize, Error> {
    let wJobID = (*mtctx).doneJobID & (*mtctx).jobIDMask;

    let mut guard = (*((*mtctx).jobs).offset(wJobID as isize))
        .job_mutex
        .lock()
        .unwrap();

    if blockToFlush && (*mtctx).doneJobID < (*mtctx).nextJobID {
        while (*((*mtctx).jobs).offset(wJobID as isize)).dstFlushed
            == (*((*mtctx).jobs).offset(wJobID as isize)).cSize
        {
            // nothing to flush
            if (*((*mtctx).jobs).offset(wJobID as isize)).consumed
                == (*((*mtctx).jobs).offset(wJobID as isize)).src.size
            {
                break;
            }
            // block when nothing to flush but some to come
            guard = (*((*mtctx).jobs).offset(wJobID as isize))
                .job_cond
                .wait(guard)
                .unwrap();
        }
    }

    // try to flush something
    let mut cSize = (*((*mtctx).jobs).offset(wJobID as isize)).cSize; // shared
    let srcConsumed = (*((*mtctx).jobs).offset(wJobID as isize)).consumed; // shared
    let srcSize = (*((*mtctx).jobs).offset(wJobID as isize)).src.size; // read-only, could be done after mutex lock, but no-declaration-after-statement
    drop(guard);
    if let Some(err) = Error::from_error_code(cSize) {
        ZSTDMT_waitForAllJobsCompleted(mtctx);
        ZSTDMT_releaseAllJobResources(mtctx);
        return Err(err);
    }

    // add frame checksum if necessary (can only happen once)
    if srcConsumed == srcSize && (*((*mtctx).jobs).offset(wJobID as isize)).frameChecksumNeeded != 0
    {
        // job completed -> worker no longer active
        let checksum = ZSTD_XXH64_digest(&mut (*mtctx).serial.xxhState) as u32;
        MEM_writeLE32(
            ((*((*mtctx).jobs).offset(wJobID as isize)).dstBuff.start as *mut core::ffi::c_char)
                .add((*((*mtctx).jobs).offset(wJobID as isize)).cSize)
                as *mut core::ffi::c_void,
            checksum,
        );
        cSize = cSize.wrapping_add(4);
        let fresh10 = &mut (*((*mtctx).jobs).offset(wJobID as isize)).cSize;
        *fresh10 = (*fresh10).wrapping_add(4); // can write this shared value, as worker is no longer active
        (*((*mtctx).jobs).offset(wJobID as isize)).frameChecksumNeeded = 0;
    }

    if cSize > 0 {
        // compression is ongoing or completed
        let toFlush = cSize
            .wrapping_sub((*((*mtctx).jobs).offset(wJobID as isize)).dstFlushed)
            .min(((*output).size).wrapping_sub((*output).pos));
        if toFlush > 0 {
            core::ptr::copy_nonoverlapping(
                ((*((*mtctx).jobs).offset(wJobID as isize)).dstBuff.start)
                    .cast::<u8>()
                    .add((*((*mtctx).jobs).offset(wJobID as isize)).dstFlushed),
                ((*output).dst).cast::<u8>().add((*output).pos),
                toFlush,
            );
        }
        (*output).pos = ((*output).pos).wrapping_add(toFlush);
        let fresh11 = &mut (*((*mtctx).jobs).offset(wJobID as isize)).dstFlushed;
        *fresh11 = (*fresh11).wrapping_add(toFlush); // can write: this value is only used by mtctx

        if srcConsumed == srcSize && (*((*mtctx).jobs).offset(wJobID as isize)).dstFlushed == cSize
        {
            // job is completed and output buffer fully flushed
            ZSTDMT_releaseBuffer(
                (*mtctx).bufPool,
                (*((*mtctx).jobs).offset(wJobID as isize)).dstBuff,
            );
            (*((*mtctx).jobs).offset(wJobID as isize)).dstBuff = g_nullBuffer;
            (*((*mtctx).jobs).offset(wJobID as isize)).cSize = 0; // ensure this job slot is considered "not started" in future check
            (*mtctx).consumed = ((*mtctx).consumed).wrapping_add(srcSize as core::ffi::c_ulonglong);
            (*mtctx).produced = ((*mtctx).produced).wrapping_add(cSize as core::ffi::c_ulonglong);
            (*mtctx).doneJobID = ((*mtctx).doneJobID).wrapping_add(1);
        }
    }

    // return value: how many bytes left in buffer ; fake it to 1 when unknown but >0
    if cSize > (*((*mtctx).jobs).offset(wJobID as isize)).dstFlushed {
        return Ok(cSize.wrapping_sub((*((*mtctx).jobs).offset(wJobID as isize)).dstFlushed));
    }
    if srcSize > srcConsumed {
        return Ok(1); // current job not completely compressed
    }

    if (*mtctx).doneJobID < (*mtctx).nextJobID {
        return Ok(1); // some more jobs ongoing
    }
    if (*mtctx).jobReady != 0 {
        return Ok(1); // one job is ready to push, just not yet in the list
    }
    if (*mtctx).inBuff.filled > 0 {
        return Ok(1); // input is not empty, and still needs to be converted into a job
    }
    (*mtctx).allJobsCompleted = (*mtctx).frameEnded; // all jobs are entirely flushed => if this one is last one, frame is completed
    if end == ZSTD_e_end {
        return Ok(usize::from((*mtctx).frameEnded == 0)); // for ZSTD_e_end, question becomes: is frame completed ?
    }

    Ok(0) // internal buffers fully flushed
}

/// Returns the range of data used by the earliest job that is not yet complete.
/// If the data of the first job is broken up into two segments, we cover both sections.
unsafe fn ZSTDMT_getInputDataInUse(mtctx: *mut ZSTDMT_CCtx) -> Range {
    let firstJobID = (*mtctx).doneJobID;
    let lastJobID = (*mtctx).nextJobID;

    // no need to check during first round
    let roundBuffCapacity = (*mtctx).roundBuff.capacity;
    let nbJobs1stRoundMin = roundBuffCapacity / (*mtctx).targetSectionSize;
    if (lastJobID as usize) < nbJobs1stRoundMin {
        return Range::default();
    }

    for jobID in firstJobID..lastJobID {
        let wJobID = jobID & (*mtctx).jobIDMask;

        let guard = (*((*mtctx).jobs).offset(wJobID as isize))
            .job_mutex
            .lock()
            .unwrap();
        let consumed = (*((*mtctx).jobs).offset(wJobID as isize)).consumed;
        drop(guard);

        if consumed < (*((*mtctx).jobs).offset(wJobID as isize)).src.size {
            let mut range = (*((*mtctx).jobs).offset(wJobID as isize)).prefix;
            if range.size == 0 {
                // Empty prefix
                range = (*((*mtctx).jobs).offset(wJobID as isize)).src;
            }
            // Job source in multiple segments not supported yet
            return range;
        }
    }

    Range::default()
}

/// Returns `true` iff buffer and range overlap.
fn ZSTDMT_isOverlapped(buffer: Buffer, range: Range) -> bool {
    if range.start.is_null() || buffer.start.is_null() {
        return false;
    }

    let bufferStart = buffer.start.addr();
    let rangeStart = range.start.addr();
    let bufferEnd = bufferStart.wrapping_add(buffer.capacity);
    let rangeEnd = rangeStart.wrapping_add(range.size);

    // Empty ranges cannot overlap
    if bufferStart == bufferEnd || rangeStart == rangeEnd {
        return false;
    }

    bufferStart < rangeEnd && rangeStart < bufferEnd
}

fn ZSTDMT_doesOverlapWindow(buffer: Buffer, window: ZSTD_window_t) -> bool {
    let mut extDict = Range::default();
    let mut prefix = Range::default();

    extDict.start =
        window.dictBase.wrapping_offset(window.lowLimit as isize) as *const core::ffi::c_void;
    extDict.size = window.dictLimit.wrapping_sub(window.lowLimit) as usize;

    let prefixStart = window.base.wrapping_offset(window.dictLimit as isize);
    prefix.start = prefixStart as *const core::ffi::c_void;
    prefix.size = window.nextSrc.wrapping_offset_from(prefixStart) as usize;

    ZSTDMT_isOverlapped(buffer, extDict) || ZSTDMT_isOverlapped(buffer, prefix)
}

unsafe fn ZSTDMT_waitForLdmComplete(mtctx: *mut ZSTDMT_CCtx, buffer: Buffer) {
    if (*mtctx).params.ldmParams.enableLdm == ParamSwitch::Enable {
        let mut guard = (*mtctx).serial.ldmWindowMutex.lock().unwrap();
        while ZSTDMT_doesOverlapWindow(buffer, (*mtctx).serial.ldmWindow) {
            guard = (*mtctx).serial.ldmWindowCond.wait(guard).unwrap();
        }
    }
}

/// Attempts to set the inBuff to the next section to fill.
/// If any part of the new section is still in use we give up.
/// Returns `true` if the buffer is filled.
unsafe fn ZSTDMT_tryGetInputRange(mtctx: *mut ZSTDMT_CCtx) -> bool {
    let inUse = ZSTDMT_getInputDataInUse(mtctx);
    let spaceLeft = ((*mtctx).roundBuff.capacity).wrapping_sub((*mtctx).roundBuff.pos);
    let spaceNeeded = (*mtctx).targetSectionSize;
    let mut buffer = buffer_s {
        start: core::ptr::null_mut::<core::ffi::c_void>(),
        capacity: 0,
    };

    if spaceLeft < spaceNeeded {
        // ZSTD_invalidateRepCodes() doesn't work for extDict variants.
        // Simply copy the prefix to the beginning in that case.
        let start = (*mtctx).roundBuff.buffer;
        let prefixSize = (*mtctx).inBuff.prefix.size;
        buffer.start = start as *mut core::ffi::c_void;
        buffer.capacity = prefixSize;
        if ZSTDMT_isOverlapped(buffer, inUse) {
            return false;
        }
        ZSTDMT_waitForLdmComplete(mtctx, buffer);
        core::ptr::copy((*mtctx).inBuff.prefix.start.cast::<u8>(), start, prefixSize);
        (*mtctx).inBuff.prefix.start = start as *const core::ffi::c_void;
        (*mtctx).roundBuff.pos = prefixSize;
    }
    buffer.start =
        ((*mtctx).roundBuff.buffer).add((*mtctx).roundBuff.pos) as *mut core::ffi::c_void;
    buffer.capacity = spaceNeeded;

    if ZSTDMT_isOverlapped(buffer, inUse) {
        return false;
    }

    ZSTDMT_waitForLdmComplete(mtctx, buffer);

    (*mtctx).inBuff.buffer = buffer;
    (*mtctx).inBuff.filled = 0;

    true
}

/// Searches through the input for a synchronization point. If one is found, we
/// will instruct the caller to flush, and return the number of bytes to load.
/// Otherwise, we will load as many bytes as possible and instruct the caller
/// to continue as normal.
unsafe fn findSynchronizationPoint(mtctx: *const ZSTDMT_CCtx, input: ZSTD_inBuffer) -> SyncPoint {
    let istart = (input.src as *const u8).add(input.pos);
    let primePower = (*mtctx).rsync.primePower;
    let hitMask = (*mtctx).rsync.hitMask;

    let mut syncPoint = SyncPoint {
        toLoad: 0,
        flush: 0,
    };

    syncPoint.toLoad = (input.size)
        .wrapping_sub(input.pos)
        .min(((*mtctx).targetSectionSize).wrapping_sub((*mtctx).inBuff.filled));
    syncPoint.flush = 0;
    if (*mtctx).params.rsyncable == 0 {
        // Rsync is disabled.
        return syncPoint;
    }
    if ((*mtctx).inBuff.filled)
        .wrapping_add(input.size)
        .wrapping_sub(input.pos)
        < RSYNC_MIN_BLOCK_SIZE as usize
    {
        // We don't emit synchronization points if it would produce too small blocks.
        // We don't have enough input to find a synchronization point, so don't look.
        return syncPoint;
    }
    if ((*mtctx).inBuff.filled).wrapping_add(syncPoint.toLoad) < usize::from(RSYNC_LENGTH) {
        // Not enough to compute the hash.
        // We will miss any synchronization points in this RSYNC_LENGTH byte
        // window. However, since it depends only in the internal buffers, if the
        // state is already synchronized, we will remain synchronized.
        // Additionally, the probability that we miss a synchronization point is
        // low: RSYNC_LENGTH / targetSectionSize.
        return syncPoint;
    }

    // Initialize the loop variables.
    let mut hash: u64;
    let prev: *const u8;
    let pos: usize;
    if (*mtctx).inBuff.filled < RSYNC_MIN_BLOCK_SIZE as usize {
        // We don't need to scan the first RSYNC_MIN_BLOCK_SIZE positions
        // because they can't possibly be a sync point. So we can start
        // part way through the input buffer.
        pos = (RSYNC_MIN_BLOCK_SIZE as usize).wrapping_sub((*mtctx).inBuff.filled);
        if pos >= usize::from(RSYNC_LENGTH) {
            prev = istart.add(pos).sub(usize::from(RSYNC_LENGTH));
            hash = ZSTD_rollingHash_compute(core::slice::from_raw_parts(
                prev,
                usize::from(RSYNC_LENGTH),
            ));
        } else {
            prev = ((*mtctx).inBuff.buffer.start as *const u8)
                .add((*mtctx).inBuff.filled)
                .sub(usize::from(RSYNC_LENGTH));
            hash = ZSTD_rollingHash_compute(core::slice::from_raw_parts(
                prev.add(pos),
                usize::from(RSYNC_LENGTH).wrapping_sub(pos),
            ));
            hash = ZSTD_rollingHash_append(hash, core::slice::from_raw_parts(istart, pos));
        }
    } else {
        // We have enough bytes buffered to initialize the hash,
        // and have processed enough bytes to find a sync point.
        // Start scanning at the beginning of the input.
        pos = 0;
        prev = ((*mtctx).inBuff.buffer.start as *const u8)
            .add((*mtctx).inBuff.filled)
            .sub(usize::from(RSYNC_LENGTH));
        hash =
            ZSTD_rollingHash_compute(core::slice::from_raw_parts(prev, usize::from(RSYNC_LENGTH)));
        if hash & hitMask == hitMask {
            // We're already at a sync point so don't load any more until
            // we're able to flush this sync point.
            // This likely happened because the job table was full so we
            // couldn't add our job.
            syncPoint.toLoad = 0;
            syncPoint.flush = 1;
            return syncPoint;
        }
    }

    // Starting with the hash of the previous RSYNC_LENGTH bytes, roll
    // through the input. If we hit a synchronization point, then cut the
    // job off, and tell the compressor to flush the job. Otherwise, load
    // all the bytes and continue as normal.
    // If we go too long without a synchronization point (targetSectionSize)
    // then a block will be emitted anyways, but this is okay, since if we
    // are already synchronized we will remain synchronized.
    for pos in pos..syncPoint.toLoad {
        let toRemove = if pos < usize::from(RSYNC_LENGTH) {
            *prev.add(pos)
        } else {
            *istart.add(pos.wrapping_sub(usize::from(RSYNC_LENGTH)))
        };
        hash = ZSTD_rollingHash_rotate(hash, toRemove, *istart.add(pos), primePower);
        if hash & hitMask == hitMask {
            syncPoint.toLoad = pos.wrapping_add(1);
            syncPoint.flush = 1;
            break;
        }
    }
    syncPoint
}

pub unsafe fn ZSTDMT_nextInputSizeHint(mtctx: *const ZSTDMT_CCtx) -> usize {
    let mut hintInSize = ((*mtctx).targetSectionSize).wrapping_sub((*mtctx).inBuff.filled);
    if hintInSize == 0 {
        hintInSize = (*mtctx).targetSectionSize;
    }
    hintInSize
}

/// internal use only - exposed to be invoked from zstd_compress.c
/// assumption: output and input are valid (pos <= size)
///
/// # Returns
///
/// The minimum amount of data remaining to flush, 0 if none
pub unsafe fn ZSTDMT_compressStream_generic(
    mtctx: *mut ZSTDMT_CCtx,
    output: *mut ZSTD_outBuffer,
    input: *mut ZSTD_inBuffer,
    mut endOp: ZSTD_EndDirective,
) -> Result<usize, Error> {
    let mut forwardInputProgress = false;

    if (*mtctx).frameEnded != 0 && endOp == ZSTD_e_continue {
        // current frame being ended. Only flush/end are allowed
        return Err(Error::stage_wrong);
    }

    // fill input buffer
    if (*mtctx).jobReady == 0 && (*input).size > (*input).pos {
        // support NULL input
        if ((*mtctx).inBuff.buffer.start).is_null() && !ZSTDMT_tryGetInputRange(mtctx) {
            // It is only possible for this operation to fail if there are
            // still compression jobs ongoing.
            assert_ne!((*mtctx).doneJobID, (*mtctx).nextJobID);
        }
        if !((*mtctx).inBuff.buffer.start).is_null() {
            let syncPoint = findSynchronizationPoint(mtctx, *input);
            if syncPoint.flush != 0 && endOp == ZSTD_e_continue {
                endOp = ZSTD_e_flush;
            }
            core::ptr::copy_nonoverlapping(
                ((*input).src).cast::<u8>().add((*input).pos),
                ((*mtctx).inBuff.buffer.start)
                    .cast::<u8>()
                    .add((*mtctx).inBuff.filled),
                syncPoint.toLoad,
            );
            (*input).pos = ((*input).pos).wrapping_add(syncPoint.toLoad);
            (*mtctx).inBuff.filled = ((*mtctx).inBuff.filled).wrapping_add(syncPoint.toLoad);
            forwardInputProgress = syncPoint.toLoad > 0;
        }
    }
    if (*input).pos < (*input).size && endOp == ZSTD_e_end {
        // Can't end yet because the input is not fully consumed.
        // We are in one of these cases:
        // - mtctx->inBuff is NULL & empty: we couldn't get an input buffer so don't create a new job.
        // - We filled the input buffer: flush this job but don't end the frame.
        // - We hit a synchronization point: flush this job but don't end the frame.
        endOp = ZSTD_e_flush;
    }

    if (*mtctx).jobReady != 0
        || (*mtctx).inBuff.filled >= (*mtctx).targetSectionSize  // filled enough: let's compress
        || endOp != ZSTD_e_continue
            && (*mtctx).inBuff.filled > 0  // something to flush: let's go
        || endOp == ZSTD_e_end // must finish the frame with a zero-size block
            && (*mtctx).frameEnded == 0
    {
        let jobSize = (*mtctx).inBuff.filled;
        ZSTDMT_createCompressionJob(mtctx, jobSize, endOp);
    }

    // check for potential compressed data ready to be flushed
    let remainingToFlush = ZSTDMT_flushProduced(
        mtctx,
        output,
        !forwardInputProgress, // block if there was no forward input progress
        endOp,
    )?;
    if (*input).pos < (*input).size {
        return Ok(remainingToFlush.max(1)); // input not consumed: do not end flush yet
    }
    Ok(remainingToFlush)
}
