use std::mem::MaybeUninit;
use std::time::{Duration, Instant};

use crate::lib::common::error_private::{ERR_isError, Error};
use crate::lib::common::mem::MEM_64bits;
use crate::lib::common::pool::{POOL_add, POOL_create, POOL_free};
use crate::lib::compress::zstd_compress_internal::ZSTD_hash64Ptr_array;
use crate::lib::dictBuilder::cover::{
    COVER_best_finish, COVER_best_start, COVER_best_t, COVER_best_wait, COVER_computeEpochs,
    COVER_dictSelectionError, COVER_segment_t, COVER_selectDict, COVER_warnOnSmallCorpus,
};
use crate::lib::dictBuilder::zdict::finalize_dictionary;
use crate::lib::zdict::experimental::{
    ZDICT_cover_params_t, ZDICT_fastCover_params_t, ZDICT_DICTSIZE_MIN,
};
use crate::ZDICT_params_t;

#[derive(Debug, Copy, Clone, Default)]
#[repr(C)]
struct FASTCOVER_accel_t {
    finalize: core::ffi::c_uint,
    skip: core::ffi::c_uint,
}

#[repr(C)]
#[derive(Debug, Default)]
struct FASTCOVER_ctx_t<'a> {
    samples: &'a [u8],
    offsets: Box<[usize]>,
    samplesSizes: &'a [usize],
    nbSamples: usize,
    nbTrainSamples: usize,
    nbTestSamples: usize,
    nbDmers: usize,
    freqs: Box<[u32]>,
    d: core::ffi::c_uint,
    f: core::ffi::c_uint,
    accelParams: FASTCOVER_accel_t,
    displayLevel: core::ffi::c_int,
}

#[repr(C)]
struct FASTCOVER_tryParameters_data_t<'a, 'b> {
    ctx: &'b FASTCOVER_ctx_t<'a>,
    best: &'b COVER_best_t,
    dictBufferCapacity: usize,
    parameters: ZDICT_cover_params_t,
}

/// There are 32-bit indexes used to ref samples, so limit samples size to 4GB
/// on 64-bit builds.
/// For 32-bit builds we choose 1 GB: most 32-bit platforms have 2GB user-mode
/// addressable space and we allocate a large contiguous buffer, so 1GB is
/// already a high limit.
const FASTCOVER_MAX_SAMPLES_SIZE: usize = if MEM_64bits() { u32::MAX as _ } else { 1 << 30 };
const FASTCOVER_MAX_F: core::ffi::c_int = 31;
const FASTCOVER_MAX_ACCEL: core::ffi::c_int = 10;
const FASTCOVER_DEFAULT_SPLITPOINT: core::ffi::c_double = 0.75f64;
const DEFAULT_F: core::ffi::c_int = 20;
const DEFAULT_ACCEL: core::ffi::c_int = 1;

fn FASTCOVER_hashPtrToIndex(p: &[u8; 8], f: u32, d: core::ffi::c_uint) -> usize {
    match d {
        6 => ZSTD_hash64Ptr_array::<6>(p, f),
        _ => ZSTD_hash64Ptr_array::<8>(p, f),
    }
}

static FASTCOVER_defaultAccelParameters: [FASTCOVER_accel_t; 11] = {
    const fn accel(finalize: core::ffi::c_uint, skip: core::ffi::c_uint) -> FASTCOVER_accel_t {
        FASTCOVER_accel_t { finalize, skip }
    }

    [
        accel(100, 0),
        accel(100, 0),
        accel(50, 1),
        accel(34, 2),
        accel(25, 3),
        accel(20, 4),
        accel(17, 5),
        accel(14, 6),
        accel(13, 7),
        accel(11, 8),
        accel(10, 9),
    ]
};

fn FASTCOVER_selectSegment(
    ctx: &FASTCOVER_ctx_t,
    freqs: &mut [u32],
    begin: u32,
    end: u32,
    parameters: ZDICT_cover_params_t,
    segmentFreqs: &mut [u16],
) -> COVER_segment_t {
    let k = parameters.k;
    let d = parameters.d;
    let samples = ctx.samples;
    let f = ctx.f;
    let dmersInK = k.wrapping_sub(d).wrapping_add(1);
    let mut bestSegment = COVER_segment_t::default();
    let mut activeSegment = COVER_segment_t {
        begin,
        end: begin,
        score: 0,
    };
    for pos in begin..end {
        let idx =
            FASTCOVER_hashPtrToIndex(ctx.samples[pos as usize..][..8].try_into().unwrap(), f, d);
        if segmentFreqs[idx] == 0 {
            activeSegment.score = activeSegment.score.wrapping_add(freqs[idx]);
        }
        activeSegment.end = pos.wrapping_add(1);
        segmentFreqs[idx] += 1;
        if activeSegment.end.wrapping_sub(activeSegment.begin) == dmersInK.wrapping_add(1) {
            let delIndex = FASTCOVER_hashPtrToIndex(
                samples[activeSegment.begin as usize..][..8]
                    .try_into()
                    .unwrap(),
                f,
                d,
            );
            segmentFreqs[delIndex] -= 1;
            if segmentFreqs[delIndex] == 0 {
                activeSegment.score = activeSegment.score.wrapping_sub(freqs[delIndex]);
            }
            activeSegment.begin = activeSegment.begin.wrapping_add(1);
        }
        if activeSegment.score > bestSegment.score {
            bestSegment = activeSegment;
        }
    }
    for pos in activeSegment.begin..end {
        let delIndex =
            FASTCOVER_hashPtrToIndex(samples[pos as usize..][..8].try_into().unwrap(), f, d);
        segmentFreqs[delIndex] -= 1;
    }
    for pos in bestSegment.begin..bestSegment.end {
        let i = FASTCOVER_hashPtrToIndex(samples[pos as usize..][..8].try_into().unwrap(), f, d);
        freqs[i] = 0;
    }
    bestSegment
}

fn FASTCOVER_checkParameters(
    parameters: ZDICT_cover_params_t,
    maxDictSize: usize,
    f: core::ffi::c_uint,
    accel: core::ffi::c_uint,
) -> bool {
    if parameters.d == 0 || parameters.k == 0 {
        return false;
    }
    if parameters.d != 6 && parameters.d != 8 {
        return false;
    }
    if parameters.k as usize > maxDictSize {
        return false;
    }
    if parameters.d > parameters.k {
        return false;
    }
    if f > FASTCOVER_MAX_F as core::ffi::c_uint || f == 0 {
        return false;
    }
    if parameters.splitPoint <= 0.0 || parameters.splitPoint > 1.0 {
        return false;
    }
    if accel > 10 || accel == 0 {
        return false;
    }

    true
}

fn FASTCOVER_ctx_destroy(ctx: &mut FASTCOVER_ctx_t) {
    drop(core::mem::take(&mut (ctx.freqs)));
    drop(core::mem::take(&mut (ctx.offsets)));
}

fn FASTCOVER_computeFrequency(ctx: &mut FASTCOVER_ctx_t) {
    let f = ctx.f;
    let d = ctx.d;
    let skip = ctx.accelParams.skip;
    let readLength = if d > 8 { d } else { 8 };
    for i in 0..ctx.nbTrainSamples {
        let mut start = ctx.offsets[i];
        let currSampleEnd = ctx.offsets[i + 1];
        while start.wrapping_add(readLength as usize) <= currSampleEnd {
            let dmerIndex =
                FASTCOVER_hashPtrToIndex(ctx.samples[start..][..8].try_into().unwrap(), f, d);
            ctx.freqs[dmerIndex] += 1;
            start = start.wrapping_add(skip as usize).wrapping_add(1);
        }
    }
}

fn FASTCOVER_ctx_init<'a>(
    ctx: &mut FASTCOVER_ctx_t<'a>,
    samples: &'a [u8],
    samplesSizes: &'a [usize],
    d: core::ffi::c_uint,
    splitPoint: core::ffi::c_double,
    f: core::ffi::c_uint,
    accelParams: FASTCOVER_accel_t,
    displayLevel: core::ffi::c_int,
) -> Result<(), Error> {
    let nbSamples = samplesSizes.len() as core::ffi::c_uint;
    let totalSamplesSize = samplesSizes.iter().sum::<usize>();
    let nbTrainSamples = if splitPoint < 1.0f64 {
        (nbSamples as core::ffi::c_double * splitPoint) as core::ffi::c_uint
    } else {
        nbSamples
    };
    let nbTestSamples = if splitPoint < 1.0f64 {
        nbSamples.wrapping_sub(nbTrainSamples)
    } else {
        nbSamples
    };
    let trainingSamplesSize = if splitPoint < 1.0f64 {
        samplesSizes[..nbTrainSamples as usize].iter().sum()
    } else {
        totalSamplesSize
    };
    let testSamplesSize = if splitPoint < 1.0f64 {
        samplesSizes[nbTrainSamples as usize..][..nbTestSamples as usize]
            .iter()
            .sum()
    } else {
        totalSamplesSize
    };

    ctx.displayLevel = displayLevel;
    if totalSamplesSize < Ord::max(d as usize, size_of::<u64>())
        || totalSamplesSize >= FASTCOVER_MAX_SAMPLES_SIZE
    {
        if displayLevel >= 1 {
            eprintln!(
                "Total samples size is too large ({} MB), maximum size is {} MB",
                totalSamplesSize >> 20,
                FASTCOVER_MAX_SAMPLES_SIZE >> 20,
            );
        }
        return Err(Error::srcSize_wrong);
    }
    if nbTrainSamples < 5 {
        if displayLevel >= 1 {
            eprintln!(
                "Total number of training samples is {} and is invalid",
                nbTrainSamples,
            );
        }
        return Err(Error::srcSize_wrong);
    }
    if nbTestSamples < 1 {
        if displayLevel >= 1 {
            eprintln!(
                "Total number of testing samples is {} and is invalid.",
                nbTestSamples,
            );
        }
        return Err(Error::srcSize_wrong);
    }
    if displayLevel >= 2 {
        eprintln!(
            "Training on {} samples of total size {}",
            nbTrainSamples, trainingSamplesSize,
        );
    }
    if displayLevel >= 2 {
        eprintln!(
            "Testing on {} samples of total size {}",
            nbTestSamples, testSamplesSize,
        );
    }
    ctx.samples = samples;
    ctx.samplesSizes = samplesSizes;
    ctx.nbSamples = nbSamples as usize;
    ctx.nbTrainSamples = nbTrainSamples as usize;
    ctx.nbTestSamples = nbTestSamples as usize;
    ctx.nbDmers = trainingSamplesSize.wrapping_sub(Ord::max(d as usize, size_of::<u64>())) + 1;
    ctx.d = d;
    ctx.f = f;
    ctx.accelParams = accelParams;
    ctx.offsets = Box::from(vec![0usize; nbSamples as usize + 1]);

    for i in 1..nbSamples as usize + 1 {
        ctx.offsets[i] = ctx.offsets[i - 1] + samplesSizes[i - 1];
    }

    ctx.freqs = Box::from(vec![0u32; 1 << f]);
    if displayLevel >= 2 {
        eprintln!("Computing frequencies");
    }
    FASTCOVER_computeFrequency(ctx);
    Ok(())
}

fn FASTCOVER_buildDictionary<'a>(
    ctx: &FASTCOVER_ctx_t,
    freqs: &mut [u32],
    dict: &'a mut [MaybeUninit<u8>],
    parameters: ZDICT_cover_params_t,
    segmentFreqs: &mut [u16],
) -> &'a [u8] {
    let mut tail = dict.len();
    let epochs = COVER_computeEpochs(dict.len() as u32, ctx.nbDmers as u32, parameters.k, 1);
    let maxZeroScoreRun = 10;
    let displayLevel = ctx.displayLevel;
    let mut zeroScoreRun = 0usize;
    let mut last_update_time = Instant::now();
    if displayLevel >= 2 {
        eprintln!(
            "Breaking content into {} epochs of size {}",
            epochs.num, epochs.size,
        );
    }
    let mut epoch: usize = 0;
    while tail > 0 {
        let epochBegin = (epoch * epochs.size as usize) as u32;
        let epochEnd = epochBegin.wrapping_add(epochs.size);
        let segment =
            FASTCOVER_selectSegment(ctx, freqs, epochBegin, epochEnd, parameters, segmentFreqs);
        if segment.score == 0 {
            zeroScoreRun = zeroScoreRun.wrapping_add(1);
            if zeroScoreRun >= maxZeroScoreRun {
                break;
            }
        } else {
            zeroScoreRun = 0;
            /* Trim the segment if necessary and if it is too small then we are done */
            let segmentSize = Ord::min(
                (segment.end - segment.begin + parameters.d - 1) as usize,
                tail,
            );
            if segmentSize < parameters.d as usize {
                break;
            }
            tail = tail.wrapping_sub(segmentSize);
            dict[tail..][..segmentSize].copy_from_slice(super::cover::as_uninit(
                &ctx.samples[segment.begin as usize..][..segmentSize],
            ));
            if displayLevel >= 2 {
                let refresh_rate = Duration::from_millis(150);
                if last_update_time.elapsed() > refresh_rate || displayLevel >= 4 {
                    last_update_time = Instant::now();
                    eprint!(
                        "\r{}%       ",
                        dict.len().wrapping_sub(tail) * 100 / dict.len()
                    );
                }
            }
        }
        epoch = epoch.wrapping_add(1) % epochs.num as usize;
    }
    if displayLevel >= 2 {
        println!("\r{:79 }\r", "");
    }

    // SAFETY: the `tail..` elements were copied from a `&[u8]` above.
    unsafe { super::cover::assume_init_ref(&dict[tail..]) }
}

fn FASTCOVER_tryParameters_wrapper(opaque: *mut core::ffi::c_void) {
    FASTCOVER_tryParameters(unsafe { Box::from_raw(opaque.cast()) })
}

#[expect(
    clippy::boxed_local,
    reason = "Box is needed because the main thread may destroy `data.ctx` immediately after the completion signal from `COVER_best_finish`"
)]
fn FASTCOVER_tryParameters(data: Box<FASTCOVER_tryParameters_data_t>) {
    let ctx = data.ctx;
    let parameters = data.parameters;
    let dictBufferCapacity = data.dictBufferCapacity;
    let mut segmentFreqs: Box<[u16]> = Box::from(vec![0u16; 1 << ctx.f]);
    let mut dict: Box<[MaybeUninit<u8>]> = Box::new_uninit_slice(dictBufferCapacity);

    let displayLevel = ctx.displayLevel;
    let mut freqs = ctx.freqs.clone();

    let dict_tail =
        FASTCOVER_buildDictionary(ctx, &mut freqs, &mut dict, parameters, &mut segmentFreqs);
    let nbFinalizeSamples =
        (ctx.nbTrainSamples * ctx.accelParams.finalize as usize / 100) as core::ffi::c_uint;
    let selection = COVER_selectDict(
        dict_tail,
        dictBufferCapacity,
        ctx.samples,
        ctx.samplesSizes,
        nbFinalizeSamples,
        ctx.nbTrainSamples,
        ctx.nbSamples,
        parameters,
        &ctx.offsets,
    );
    if selection.is_err() && displayLevel >= 1 {
        eprintln!("Failed to select dictionary");
    }

    drop(dict);
    let selection = selection.unwrap_or_else(COVER_dictSelectionError);
    COVER_best_finish(data.best, parameters, &selection);
}

fn FASTCOVER_convertToCoverParams(
    fastCoverParams: ZDICT_fastCover_params_t,
    coverParams: &mut ZDICT_cover_params_t,
) {
    coverParams.k = fastCoverParams.k;
    coverParams.d = fastCoverParams.d;
    coverParams.steps = fastCoverParams.steps;
    coverParams.nbThreads = fastCoverParams.nbThreads;
    coverParams.splitPoint = fastCoverParams.splitPoint;
    coverParams.zParams = fastCoverParams.zParams;
    coverParams.shrinkDict = fastCoverParams.shrinkDict;
}

fn FASTCOVER_convertToFastCoverParams(
    coverParams: ZDICT_cover_params_t,
    fastCoverParams: &mut ZDICT_fastCover_params_t,
    f: core::ffi::c_uint,
    accel: core::ffi::c_uint,
) {
    fastCoverParams.k = coverParams.k;
    fastCoverParams.d = coverParams.d;
    fastCoverParams.steps = coverParams.steps;
    fastCoverParams.nbThreads = coverParams.nbThreads;
    fastCoverParams.splitPoint = coverParams.splitPoint;
    fastCoverParams.f = f;
    fastCoverParams.accel = accel;
    fastCoverParams.zParams = coverParams.zParams;
    fastCoverParams.shrinkDict = coverParams.shrinkDict;
}

/// Train a dictionary from an array of samples using a modified version of COVER algorithm.
///
/// Samples must be stored concatenated in a single flat buffer `samplesBuffer`,  supplied with an
/// array of sizes `samplesSizes`, providing the size of each sample, in order.
///
/// Only parameters `d` and `k` are required. All other parameters will use default values if not
/// provided.
///
/// The resulting dictionary will be saved into `dictBuffer`.
///
/// In general, a reasonable dictionary has a size of ~100 KB. It's possible to select smaller or
/// larger size, just by specifying `dictBufferCapacity`. In general, it's recommended to provide a
/// few thousands samples, though this can vary a lot. It's recommended that total size of all
/// samples be about ~x100 times the target size of dictionary.
///
/// # Returns
///
/// - the size of the dictionary stored into `dictBuffer` (<= `dictBufferCapacity`)
/// - an error code, which can be tested with [`crate::ZDICT_isError`]
///
/// Dictionary training will fail if there are not enough samples to construct a dictionary, or if
/// most of the samples are too small (< 8 bytes being the lower limit). If dictionary training
/// fails, you should use zstd without a dictionary, as the dictionary would've been ineffective
/// anyways. If you believe your samples would benefit from a dictionary please open an issue with
/// details, and we can look into it.
///
/// # Safety
///
/// Behavior is undefined if any of the following conditions are violated:
///
/// - `dictBufferCapacity` is 0 or `dictBuffer` and `dictBufferCapacity` satisfy the requirements
///   of [`core::slice::from_raw_parts_mut`].
/// - `nbSamples` is 0 or `samplesSizes` and `nbSamples` satisfy the requirements
///   of [`core::slice::from_raw_parts`].
/// - `sum(samplesSizes)` is 0 or `samplesBuffer` and `sum(samplesSizes)` satisfy the requirements
///   of [`core::slice::from_raw_parts`].
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZDICT_trainFromBuffer_fastCover))]
pub unsafe extern "C" fn ZDICT_trainFromBuffer_fastCover(
    dictBuffer: *mut core::ffi::c_void,
    dictBufferCapacity: usize,
    samplesBuffer: *const core::ffi::c_void,
    samplesSizes: *const usize,
    nbSamples: core::ffi::c_uint,
    parameters: ZDICT_fastCover_params_t,
) -> usize {
    let dict = unsafe { core::slice::from_raw_parts_mut(dictBuffer.cast(), dictBufferCapacity) };

    let samplesSizes = if samplesSizes.is_null() || nbSamples == 0 {
        &[]
    } else {
        core::slice::from_raw_parts(samplesSizes, nbSamples as usize)
    };
    let totalSamplesSize = samplesSizes.iter().sum::<usize>();
    let samples = if samplesBuffer.is_null() || totalSamplesSize == 0 {
        &[]
    } else {
        core::slice::from_raw_parts(samplesBuffer.cast::<u8>(), totalSamplesSize)
    };

    train_from_buffer_fastcover(dict, samples, samplesSizes, parameters)
}

fn train_from_buffer_fastcover(
    dict: &mut [MaybeUninit<u8>],
    samples: &[u8],
    samplesSizes: &[usize],
    mut parameters: ZDICT_fastCover_params_t,
) -> usize {
    let dictBufferCapacity = dict.len();

    let mut ctx = FASTCOVER_ctx_t::default();
    let displayLevel = parameters.zParams.notificationLevel as core::ffi::c_int;
    parameters.splitPoint = 1.0f64;
    parameters.f = if parameters.f == 0 {
        DEFAULT_F as core::ffi::c_uint
    } else {
        parameters.f
    };
    parameters.accel = if parameters.accel == 0 {
        DEFAULT_ACCEL as core::ffi::c_uint
    } else {
        parameters.accel
    };
    let mut coverParams = ZDICT_cover_params_t::default();
    FASTCOVER_convertToCoverParams(parameters, &mut coverParams);
    if !FASTCOVER_checkParameters(
        coverParams,
        dictBufferCapacity,
        parameters.f,
        parameters.accel,
    ) {
        if displayLevel >= 1 {
            eprintln!("FASTCOVER parameters incorrect");
        }
        return Error::parameter_outOfBound.to_error_code();
    }
    if samplesSizes.is_empty() {
        if displayLevel >= 1 {
            eprintln!("FASTCOVER must have at least one input file");
        }
        return Error::srcSize_wrong.to_error_code();
    }
    if dictBufferCapacity < ZDICT_DICTSIZE_MIN {
        if displayLevel >= 1 {
            eprintln!("dictBufferCapacity must be at least {}", 256);
        }
        return Error::dstSize_tooSmall.to_error_code();
    }
    let accelParams = FASTCOVER_defaultAccelParameters[parameters.accel as usize];

    if let Err(err) = FASTCOVER_ctx_init(
        &mut ctx,
        samples,
        samplesSizes,
        coverParams.d,
        parameters.splitPoint,
        parameters.f,
        accelParams,
        displayLevel,
    ) {
        if displayLevel >= 1 {
            eprintln!("Failed to initialize context");
        }
        return err.to_error_code();
    }
    COVER_warnOnSmallCorpus(dictBufferCapacity, ctx.nbDmers, displayLevel);
    if displayLevel >= 2 {
        eprintln!("Building dictionary");
    }
    let mut segmentFreqs: Box<[u16]> = Box::from(vec![0u16; 1 << parameters.f]);

    let mut freqs = core::mem::take(&mut ctx.freqs);
    let dict_tail =
        FASTCOVER_buildDictionary(&ctx, &mut freqs, dict, coverParams, &mut segmentFreqs);
    ctx.freqs = freqs;

    let nbFinalizeSamples =
        (ctx.nbTrainSamples * ctx.accelParams.finalize as usize / 100) as core::ffi::c_uint;
    let samplesSizes = &samplesSizes[..nbFinalizeSamples as usize];
    let samples = &samples[..samplesSizes.iter().sum()];
    let customDictContentSize = dict_tail.len();
    let dictBuffer = dict.as_mut_ptr() as *mut core::ffi::c_void;
    let customDictContent = dictBuffer.wrapping_add(dictBufferCapacity - customDictContentSize);
    let dictionarySize = unsafe {
        finalize_dictionary(
            dictBuffer,
            dictBufferCapacity,
            customDictContent,
            customDictContentSize,
            samples,
            samplesSizes,
            coverParams.zParams,
        )
    }
    .unwrap_or_else(|err| err.to_error_code());
    if !ERR_isError(dictionarySize) && displayLevel >= 2 {
        eprintln!("Constructed dictionary of size {}", dictionarySize);
    }
    FASTCOVER_ctx_destroy(&mut ctx);
    drop(segmentFreqs);
    dictionarySize
}

/// This function tries many parameter combinations (specifically, `k` and `d` combinations) and
/// picks the best parameters.
///
/// `*parameters` is filled with the best parameters found, and the dictionary constructed with
/// those parameters is stored in `dictBuffer`.
///
/// The parameters `d`, `k`, `steps`, and `accel` are optional:
/// - If `d` is zero, we check `d` in 6..8.
/// - If `k` is zero, we check `d` in 50..2000.
/// - If `steps` is zero it defaults to its default value (40).
/// - If `accel` is zero, the default value of 1 is used.
///
/// # Returns
///
/// - the size of the dictionary stored into `dictBuffer` (<= `dictBufferCapacity`)
/// - an error code, which can be tested with [`crate::ZDICT_isError`]
///
/// Dictionary training will fail if there are not enough samples to construct a dictionary, or if
/// most of the samples are too small (< 8 bytes being the lower limit). If dictionary training
/// fails, you should use zstd without a dictionary, as the dictionary would've been ineffective
/// anyways. If you believe your samples would benefit from a dictionary please open an issue with
/// details, and we can look into it.
///
/// On success `*parameters` contains the parameters selected.
///
/// # Safety
///
/// Behavior is undefined if any of the following conditions are violated:
///
/// - `dictBufferCapacity` is 0 or `dictBuffer` and `dictBufferCapacity` satisfy the requirements
///   of [`core::slice::from_raw_parts_mut`].
/// - `nbSamples` is 0 or `samplesSizes` and `nbSamples` satisfy the requirements
///   of [`core::slice::from_raw_parts`].
/// - `sum(samplesSizes)` is 0 or `samplesBuffer` and `sum(samplesSizes)` satisfy the requirements
///   of [`core::slice::from_raw_parts`].
/// - `parameters` satisfies the requirements of [`pointer::as_mut`]
///
/// [`pointer::as_mut`]: https://doc.rust-lang.org/stable/core/primitive.pointer.html#method.as_mut
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZDICT_optimizeTrainFromBuffer_fastCover))]
pub unsafe extern "C" fn ZDICT_optimizeTrainFromBuffer_fastCover(
    dictBuffer: *mut core::ffi::c_void,
    dictBufferCapacity: usize,
    samplesBuffer: *const core::ffi::c_void,
    samplesSizes: *const usize,
    nbSamples: core::ffi::c_uint,
    parameters: *mut ZDICT_fastCover_params_t,
) -> usize {
    let dict = unsafe { core::slice::from_raw_parts_mut(dictBuffer.cast(), dictBufferCapacity) };

    let samplesSizes = if samplesSizes.is_null() || nbSamples == 0 {
        &[]
    } else {
        core::slice::from_raw_parts(samplesSizes, nbSamples as usize)
    };
    let totalSamplesSize = samplesSizes.iter().sum::<usize>();
    let samples = if samplesBuffer.is_null() || totalSamplesSize == 0 {
        &[]
    } else {
        core::slice::from_raw_parts(samplesBuffer.cast::<u8>(), totalSamplesSize)
    };

    let parameters = unsafe { parameters.as_mut().unwrap() };

    optimize_train_from_buffer_fastcover(dict, samples, samplesSizes, parameters)
}

fn optimize_train_from_buffer_fastcover(
    dict: &mut [MaybeUninit<u8>],
    samples: &[u8],
    samplesSizes: &[usize],
    parameters: &mut ZDICT_fastCover_params_t,
) -> usize {
    let dictBufferCapacity = dict.len();

    let nbThreads = parameters.nbThreads;
    let splitPoint = if parameters.splitPoint <= 0.0f64 {
        FASTCOVER_DEFAULT_SPLITPOINT
    } else {
        parameters.splitPoint
    };
    let kMinD = if parameters.d == 0 { 6 } else { parameters.d };
    let kMaxD = if parameters.d == 0 { 8 } else { parameters.d };
    let kMinK = if parameters.k == 0 { 50 } else { parameters.k };
    let kMaxK = if parameters.k == 0 {
        2000
    } else {
        parameters.k
    };
    let kSteps = if parameters.steps == 0 {
        40
    } else {
        parameters.steps
    };
    let kStepSize = if kMaxK.wrapping_sub(kMinK).wrapping_div(kSteps) > 1 {
        kMaxK.wrapping_sub(kMinK).wrapping_div(kSteps)
    } else {
        1
    };
    let kIterations = (1 as core::ffi::c_uint)
        .wrapping_add(kMaxD.wrapping_sub(kMinD).wrapping_div(2))
        .wrapping_mul(
            (1 as core::ffi::c_uint)
                .wrapping_add(kMaxK.wrapping_sub(kMinK).wrapping_div(kStepSize)),
        );
    let f = if parameters.f == 0 {
        DEFAULT_F as core::ffi::c_uint
    } else {
        parameters.f
    };
    let accel = if parameters.accel == 0 {
        DEFAULT_ACCEL as core::ffi::c_uint
    } else {
        parameters.accel
    };
    let shrinkDict = 0;
    let displayLevel = parameters.zParams.notificationLevel as core::ffi::c_int;
    let mut iteration = 1 as core::ffi::c_uint;
    let mut warned = 0;
    let mut last_update_time = Instant::now();
    if splitPoint <= 0.0 || splitPoint > 1.0 {
        if displayLevel >= 1 {
            eprintln!("Incorrect splitPoint");
        }
        return Error::parameter_outOfBound.to_error_code();
    }
    if accel == 0 || accel > FASTCOVER_MAX_ACCEL as core::ffi::c_uint {
        if displayLevel >= 1 {
            eprintln!("Incorrect accel");
        }
        return Error::parameter_outOfBound.to_error_code();
    }
    if kMinK < kMaxD || kMaxK < kMinK {
        if displayLevel >= 1 {
            eprintln!("Incorrect k");
        }
        return Error::parameter_outOfBound.to_error_code();
    }
    if samplesSizes.is_empty() {
        if displayLevel >= 1 {
            eprintln!("FASTCOVER must have at least one input file");
        }
        return Error::srcSize_wrong.to_error_code();
    }
    if dict.len() < ZDICT_DICTSIZE_MIN {
        if displayLevel >= 1 {
            eprintln!("dictBufferCapacity must be at least {}", 256);
        }
        return Error::dstSize_tooSmall.to_error_code();
    }

    let mut pool = core::ptr::null_mut();
    if nbThreads > 1 {
        pool = unsafe { POOL_create(nbThreads as usize, 1) };
        if pool.is_null() {
            return Error::memory_allocation.to_error_code();
        }
    }

    let best = COVER_best_t::new();
    let mut coverParams = ZDICT_cover_params_t::default();
    FASTCOVER_convertToCoverParams(*parameters, &mut coverParams);
    let accelParams = FASTCOVER_defaultAccelParameters[accel as usize];
    if displayLevel >= 2 {
        eprintln!("Trying {} different sets of parameters", kIterations);
    }

    for d in (kMinD..=kMaxD).step_by(2) {
        let mut ctx = FASTCOVER_ctx_t::default();
        if displayLevel >= 3 {
            eprintln!("d={}", d);
        }
        let childDisplayLevel = if displayLevel == 0 {
            0
        } else {
            displayLevel - 1
        };

        if let Err(err) = FASTCOVER_ctx_init(
            &mut ctx,
            samples,
            samplesSizes,
            d,
            splitPoint,
            f,
            accelParams,
            childDisplayLevel,
        ) {
            if displayLevel >= 1 {
                eprintln!("Failed to initialize context");
            }
            drop(COVER_best_wait(&best));
            unsafe { POOL_free(pool) };
            return err.to_error_code();
        }
        if warned == 0 {
            COVER_warnOnSmallCorpus(dictBufferCapacity, ctx.nbDmers, displayLevel);
            warned = 1;
        }

        for k in (kMinK..=kMaxK).step_by(kStepSize as usize) {
            if displayLevel >= 3 {
                eprintln!("k={}", k);
            }

            let parameters = ZDICT_cover_params_t {
                k,
                d,
                splitPoint,
                steps: kSteps,
                shrinkDict,
                zParams: ZDICT_params_t {
                    notificationLevel: ctx.displayLevel as core::ffi::c_uint,
                    compressionLevel: 0,
                    dictID: 0,
                },
                nbThreads: 0,
                shrinkDictMaxRegression: 0,
            };

            let data = Box::new(FASTCOVER_tryParameters_data_t {
                ctx: &ctx,
                best: &best,
                dictBufferCapacity,
                parameters,
            });

            if !FASTCOVER_checkParameters(data.parameters, dictBufferCapacity, data.ctx.f, accel) {
                if displayLevel >= 1 {
                    eprintln!("FASTCOVER parameters incorrect");
                }
                drop(data);
            } else {
                COVER_best_start(&best);
                if !pool.is_null() {
                    unsafe {
                        POOL_add(
                            pool,
                            FASTCOVER_tryParameters_wrapper,
                            Box::into_raw(data).cast(),
                        )
                    }
                } else {
                    FASTCOVER_tryParameters(data);
                }
                if displayLevel >= 2 {
                    let refresh_rate = Duration::from_millis(150);
                    if last_update_time.elapsed() > refresh_rate || displayLevel >= 4 {
                        last_update_time = Instant::now();
                        eprintln!(
                            "\r{}%       ",
                            iteration.wrapping_mul(100).wrapping_div(kIterations),
                        );
                    }
                }
                iteration = iteration.wrapping_add(1);
            }
        }

        drop(COVER_best_wait(&best));
        FASTCOVER_ctx_destroy(&mut ctx);
    }

    if displayLevel >= 2 {
        println!("\r{:79 }\r", "");
    }

    let best = COVER_best_wait(&best);

    let dictSize = best.dictSize;
    if ERR_isError(best.compressedSize) {
        let compressedSize = best.compressedSize;
        unsafe { POOL_free(pool) };
        return compressedSize;
    }
    FASTCOVER_convertToFastCoverParams(best.parameters, parameters, f, accel);
    dict[..dictSize].copy_from_slice(super::cover::as_uninit(&best.dict[..dictSize]));
    unsafe { POOL_free(pool) };
    dictSize
}
