use core::ptr;

use crate::lib::common::error_private::Error;
pub const HIST_WKSP_SIZE_U32: usize = 1024;
pub const HIST_WKSP_SIZE: usize = HIST_WKSP_SIZE_U32.wrapping_mul(size_of::<core::ffi::c_uint>());
pub const HIST_FAST_THRESHOLD: core::ffi::c_int = 1500;

#[derive(Debug, Clone, Copy, PartialEq)]
enum CheckInput {
    Trust,
    CheckMaxSymbolValue,
}

pub fn HIST_add(count: &mut [core::ffi::c_uint; 1024], src: &[u8]) {
    for &byte in src {
        count[usize::from(byte)] += 1;
    }
}

pub unsafe fn HIST_count_simple(
    count: *mut core::ffi::c_uint,
    maxSymbolValuePtr: &mut u8,
    src: &[u8],
) -> core::ffi::c_uint {
    let mut maxSymbolValue = *maxSymbolValuePtr;
    let mut largestCount = 0;

    ptr::write_bytes(
        count as *mut u8,
        0,
        (usize::from(maxSymbolValue) + 1) * size_of::<core::ffi::c_uint>(),
    );
    if src.is_empty() {
        *maxSymbolValuePtr = 0;
        return 0;
    }

    for &byte in src {
        *count.add(usize::from(byte)) += 1;
    }

    // `src` is non-empty, so (assuming no symbol exceeds `maxSymbolValue`, which this
    // variant deliberately does not check) at least one symbol has a non-zero count
    while *count.add(usize::from(maxSymbolValue)) == 0 {
        maxSymbolValue -= 1;
    }
    *maxSymbolValuePtr = maxSymbolValue;

    for s in 0..usize::from(maxSymbolValue) + 1 {
        largestCount = Ord::max(largestCount, *count.add(s));
    }

    largestCount
}

/// Store histogram into 4 intermediate tables, recombined at the end.
/// this design makes better use of OoO cpus,
/// and is noticeably faster when some values are heavily repeated.
/// But it needs some additional workspace for intermediate tables.
/// `workSpace` must be a U32 table of size >= HIST_WKSP_SIZE_U32,
/// and must be zeroed: the counts below accumulate into it.
///
/// # Returns
///
/// largest histogram frequency, or an error code (notably when
/// histogram's alphabet is larger than *maxSymbolValuePtr)
unsafe fn HIST_count_parallel_wksp(
    count: *mut core::ffi::c_uint,
    maxSymbolValuePtr: &mut u8,
    source: &[u8],
    check: CheckInput,
    workSpace: &mut [u32; 1024],
) -> Result<core::ffi::c_uint, Error> {
    // TEMP: this can probably be removed if we make this function fully safe
    // Some callers reuse `workSpace`'s memory as `count`: to prevent aliasing issues
    // with the `&mut` reference, skip writes to `count`.
    let aliasesWorkSpace = count as *mut u8 == workSpace.as_mut_ptr().cast::<u8>();

    let countSize = (usize::from(*maxSymbolValuePtr) + 1) * size_of::<core::ffi::c_uint>();
    let mut max = 0;

    let ([Counting1, Counting2, Counting3, Counting4], &mut []) = workSpace.as_chunks_mut::<256>()
    else {
        unreachable!()
    };

    // safety checks
    if source.is_empty() {
        if !aliasesWorkSpace {
            ptr::write_bytes(count as *mut u8, 0, countSize);
        }
        *maxSymbolValuePtr = 0;
        return Ok(0);
    }

    // by stripes of 16 bytes
    let (stripes, rest) = source.as_chunks::<16>();
    for stripe in stripes {
        for &[c0, c1, c2, c3] in stripe.as_chunks::<4>().0 {
            Counting1[usize::from(c0)] += 1;
            Counting2[usize::from(c1)] += 1;
            Counting3[usize::from(c2)] += 1;
            Counting4[usize::from(c3)] += 1;
        }
    }

    // finish last symbols
    for &byte in rest {
        Counting1[usize::from(byte)] += 1;
    }

    for s in 0..256 {
        Counting1[s] += Counting2[s] + Counting3[s] + Counting4[s];
        max = Ord::max(max, Counting1[s]);
    }

    // `source` is non-empty, so at least one symbol has a non-zero count
    let mut maxSymbolValue = u8::MAX;
    let mut it = Counting1.iter().rev();
    while let Some(0) = it.next() {
        maxSymbolValue -= 1;
    }

    if check != CheckInput::Trust && maxSymbolValue > *maxSymbolValuePtr {
        return Err(Error::maxSymbolValue_tooSmall);
    }
    *maxSymbolValuePtr = maxSymbolValue;
    if !aliasesWorkSpace {
        core::ptr::copy(workSpace.as_ptr().cast::<u8>(), count as *mut u8, countSize);
    }

    Ok(max)
}

/// Same as [`HIST_countFast`], but using an externally provided scratch buffer.
/// `workSpace` is a writable buffer which must be 4-bytes aligned,
/// `workSpaceSize` must be >= HIST_WKSP_SIZE
pub unsafe fn HIST_countFast_wksp(
    count: *mut core::ffi::c_uint,
    maxSymbolValuePtr: &mut u8,
    source: &[u8],
    workSpace: *mut core::ffi::c_void,
    workSpaceSize: usize,
) -> Result<core::ffi::c_uint, Error> {
    // checked before the workspace, which this path does not touch
    if source.len() < HIST_FAST_THRESHOLD as usize {
        // heuristic threshold
        return Ok(HIST_count_simple(count, maxSymbolValuePtr, source));
    }
    if workSpace as usize & 3 != 0 {
        // must be aligned on 4-bytes boundaries
        return Err(Error::GENERIC);
    }
    if workSpaceSize < HIST_WKSP_SIZE {
        return Err(Error::workSpace_tooSmall);
    }

    // SAFETY: we've validated the length and the alignment, and initialized the memory.
    unsafe { core::ptr::write_bytes(workSpace, 0u8, HIST_WKSP_SIZE) };
    let workSpace = unsafe { &mut *workSpace.cast::<[u32; HIST_WKSP_SIZE_U32]>() };

    HIST_countFast_wksp_array(count, maxSymbolValuePtr, source, workSpace)
}

/// Same as [`HIST_countFast_wksp`], but taking the scratch buffer as an array.
///
/// `workSpace` must be zeroed.
pub unsafe fn HIST_countFast_wksp_array(
    count: *mut core::ffi::c_uint,
    maxSymbolValuePtr: &mut u8,
    source: &[u8],
    workSpace: &mut [u32; HIST_WKSP_SIZE_U32],
) -> Result<core::ffi::c_uint, Error> {
    if source.len() < HIST_FAST_THRESHOLD as usize {
        // heuristic threshold
        return Ok(HIST_count_simple(count, maxSymbolValuePtr, source));
    }

    HIST_count_parallel_wksp(
        count,
        maxSymbolValuePtr,
        source,
        CheckInput::Trust,
        workSpace,
    )
}

/// Same as [`HIST_count`], but using an externally provided scratch buffer.
/// `workSpace` size must be table of >= HIST_WKSP_SIZE_U32 unsigned
pub unsafe fn HIST_count_wksp(
    count: *mut core::ffi::c_uint,
    maxSymbolValuePtr: &mut u8,
    source: &[u8],
    workSpace: *mut core::ffi::c_void,
    workSpaceSize: usize,
) -> Result<core::ffi::c_uint, Error> {
    if workSpace as usize & 3 != 0 {
        // must be aligned on 4-bytes boundaries
        return Err(Error::GENERIC);
    }
    if workSpaceSize < HIST_WKSP_SIZE {
        return Err(Error::workSpace_tooSmall);
    }

    if *maxSymbolValuePtr < u8::MAX {
        // SAFETY: we've validated the length and the alignment, and initialized the memory.
        unsafe { core::ptr::write_bytes(workSpace, 0u8, HIST_WKSP_SIZE) };
        let workSpace = unsafe { &mut *workSpace.cast::<[u32; HIST_WKSP_SIZE_U32]>() };

        return HIST_count_wksp_array(count, maxSymbolValuePtr, source, workSpace);
    }

    // this path may not touch the workspace at all, so leave the zeroing to it
    *maxSymbolValuePtr = u8::MAX;
    HIST_countFast_wksp(count, maxSymbolValuePtr, source, workSpace, workSpaceSize)
}

/// Same as [`HIST_count_wksp`], but taking the scratch buffer as an array.
///
/// `workSpace` must be zeroed.
pub unsafe fn HIST_count_wksp_array(
    count: *mut core::ffi::c_uint,
    maxSymbolValuePtr: &mut u8,
    source: &[u8],
    workSpace: &mut [u32; HIST_WKSP_SIZE_U32],
) -> Result<core::ffi::c_uint, Error> {
    if *maxSymbolValuePtr < u8::MAX {
        HIST_count_parallel_wksp(
            count,
            maxSymbolValuePtr,
            source,
            CheckInput::CheckMaxSymbolValue,
            workSpace,
        )
    } else {
        *maxSymbolValuePtr = u8::MAX;

        HIST_countFast_wksp_array(count, maxSymbolValuePtr, source, workSpace)
    }
}

/// fast variant (unsafe : won't check if src contains values beyond count[] limit)
pub unsafe fn HIST_countFast(
    count: *mut core::ffi::c_uint,
    maxSymbolValuePtr: &mut u8,
    source: &[u8],
) -> Result<core::ffi::c_uint, Error> {
    // zeroed, as `HIST_countFast_wksp_array` requires
    let mut tmpCounters: [core::ffi::c_uint; HIST_WKSP_SIZE_U32] = [0; HIST_WKSP_SIZE_U32];
    HIST_countFast_wksp_array(count, maxSymbolValuePtr, source, &mut tmpCounters)
}

pub unsafe fn HIST_count(
    count: *mut core::ffi::c_uint,
    maxSymbolValuePtr: &mut u8,
    src: &[u8],
) -> Result<core::ffi::c_uint, Error> {
    // zeroed, as `HIST_count_wksp_array` requires
    let mut tmpCounters: [core::ffi::c_uint; HIST_WKSP_SIZE_U32] = [0; HIST_WKSP_SIZE_U32];
    HIST_count_wksp_array(count, maxSymbolValuePtr, src, &mut tmpCounters)
}
