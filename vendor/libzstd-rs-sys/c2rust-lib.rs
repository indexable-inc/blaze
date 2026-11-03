//! zstd, short for *Zstandard*, is a fast lossless compression algorithm, targeting real-time
//! compression scenarios at zlib-level and better compression ratios. The zstd compression library
//! provides in-memory compression and decompression functions.
//!
//! The library supports regular compression levels from 1 up to [`ZSTD_maxCLevel`], which is
//! currently 22. Levels >= 20, labeled `--ultra`, should be used with caution, as they require
//! more memory. The library also offers negative compression levels, which extend the range of
//! speed vs. ratio preferences. The lower the level, the faster the speed (at the cost of
//! compression).
//!
//! (De-)compression can be done in:
//!   - a single step (described as *Simple API*) using [`ZSTD_compress`] and [`ZSTD_decompress`]
//!   - a single step, reusing a context (described as *Explicit context*) using
//!     [`ZSTD_createCCtx`] and [`ZSTD_createDCtx`] to create the context, and using
//!     [`ZSTD_compressCCtx`] and [`ZSTD_decompressDCtx`] to (de-)compress
//!   - unbounded multiple steps (described as *Streaming compression*), using
//!     [`ZSTD_createCStream`] and [`ZSTD_createDStream`] to create a stream, [`ZSTD_initCStream`]
//!     and [`ZSTD_initDStream`] to (re-)initialize the stream for (de)-compression, and
//!     [`ZSTD_compressStream2`] and [`ZSTD_decompressStream`] to consume input
//!
//! The compression ratio achievable on small data can be highly improved using a dictionary.
//! Dictionary compression can be performed in:
//!   - a single step (described as *Simple dictionary API*), using [`ZSTD_compress_usingDict`] and
//!     [`ZSTD_decompress_usingDict`]
//!   - a single step, reusing a dictionary (described as *Bulk-processing dictionary API*), using
//!     [`ZSTD_createCDict`] and [`ZSTD_createDDict`] to create a dictionary, and using
//!     [`ZSTD_compress_usingCDict`] and [`ZSTD_decompress_usingDDict`] to (de-)compress using the
//!     dictionary
//!
//! Advanced experimental APIs should never be used with a dynamically-linked library. They are not
//! "stable"; their definitions or signatures may change in the future. Only static linking is
//! allowed.

#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]
#![allow(clippy::too_many_arguments)]
#![cfg_attr(all(feature = "nightly", test), feature(test))]
#![cfg_attr(feature = "nightly", feature(likely_unlikely))]
#![cfg_attr(feature = "nightly", feature(cold_path))]
#![cfg_attr(feature = "nightly", feature(hint_prefetch))]
// FIXME
#![allow(clippy::missing_safety_doc)]

pub mod lib {
    pub mod common {
        pub(crate) mod allocations;
        pub(crate) mod bits;
        pub(crate) mod bitstream;
        pub(crate) mod compiler;
        pub mod debug;
        pub(crate) mod entropy_common;
        pub(crate) mod error_private;
        pub(crate) mod fse;
        pub(crate) mod fse_decompress;
        pub mod huf;
        pub(crate) mod mem;
        pub(crate) mod pool;
        pub(crate) mod reader;
        pub(crate) mod xxhash;
        pub mod zstd_common;
        pub(crate) mod zstd_internal;
        pub mod zstd_trace;
    } // mod common
    pub mod compress {
        pub mod fse_compress;
        pub mod hist;
        pub mod huf_compress;
        pub mod zstd_compress;
        pub(crate) mod zstd_compress_internal;
        pub mod zstd_compress_literals;
        pub mod zstd_compress_sequences;
        pub mod zstd_compress_superblock;
        pub mod zstd_double_fast;
        pub mod zstd_fast;
        pub mod zstd_lazy;
        pub mod zstd_ldm;
        pub mod zstd_opt;
        pub mod zstd_preSplit;
        mod zstdmt_compress;
    } // mod compress
    pub mod decompress;
    pub(crate) mod polyfill;
    pub(crate) mod dictBuilder {
        pub(crate) mod cover;
        pub(crate) mod divsufsort;
        pub(crate) mod fastcover;
        pub(crate) mod zdict;
    } // mod dictBuilder
    pub(crate) mod legacy {
        #![allow(unused_assignments)]
        pub(crate) mod zstd_v05;
        pub(crate) mod zstd_v06;
        pub(crate) mod zstd_v07;
    } // mod legacy
    pub mod zdict;
    pub mod zstd;
} // mod lib

pub use crate::lib::zstd::{
    ParamSwitch, ZSTD_EndDirective, ZSTD_ResetDirective, ZSTD_Sequence, ZSTD_cParameter,
    ZSTD_dParameter, ZSTD_dictAttachPref_e, ZSTD_format_e, ZSTD_frameProgression, ZSTD_inBuffer,
    ZSTD_outBuffer, ZSTD_strategy, ZSTD_BLOCKSIZELOG_MAX, ZSTD_BLOCKSIZE_MAX,
    ZSTD_BLOCKSIZE_MAX_MIN, ZSTD_CHAINLOG_MAX_32, ZSTD_CHAINLOG_MAX_64, ZSTD_CHAINLOG_MIN,
    ZSTD_CLEVEL_DEFAULT, ZSTD_CONTENTSIZE_ERROR, ZSTD_CONTENTSIZE_UNKNOWN,
    ZSTD_FRAMEHEADERSIZE_MAX, ZSTD_HASHLOG_MIN, ZSTD_LDM_BUCKETSIZELOG_MAX,
    ZSTD_LDM_BUCKETSIZELOG_MIN, ZSTD_LDM_HASHLOG_MIN, ZSTD_LDM_HASHRATELOG_MIN,
    ZSTD_LDM_MINMATCH_MAX, ZSTD_LDM_MINMATCH_MIN, ZSTD_MAGICNUMBER, ZSTD_MAGIC_DICTIONARY,
    ZSTD_MAGIC_SKIPPABLE_MASK, ZSTD_MAGIC_SKIPPABLE_START, ZSTD_MINMATCH_MAX, ZSTD_MINMATCH_MIN,
    ZSTD_OVERLAPLOG_MAX, ZSTD_OVERLAPLOG_MIN, ZSTD_SEARCHLOG_MIN, ZSTD_SKIPPABLEHEADERSIZE,
    ZSTD_SRCSIZEHINT_MIN, ZSTD_TARGETCBLOCKSIZE_MAX, ZSTD_TARGETCBLOCKSIZE_MIN,
    ZSTD_TARGETLENGTH_MAX, ZSTD_TARGETLENGTH_MIN, ZSTD_VERSION_MAJOR, ZSTD_VERSION_MINOR,
    ZSTD_VERSION_NUMBER, ZSTD_VERSION_RELEASE, ZSTD_WINDOWLOG_LIMIT_DEFAULT, ZSTD_WINDOWLOG_MAX_32,
    ZSTD_WINDOWLOG_MAX_64, ZSTD_WINDOWLOG_MIN,
};

#[allow(deprecated)] // We still export deprecated functions
pub use crate::lib::decompress::{
    zstd_ddict::{
        ZSTD_DDict, ZSTD_createDDict, ZSTD_createDDict_byReference, ZSTD_estimateDDictSize,
        ZSTD_freeDDict, ZSTD_getDictID_fromDDict, ZSTD_sizeof_DDict,
    },
    zstd_decompress::{
        ZSTD_DCtx_getParameter, ZSTD_DCtx_loadDictionary, ZSTD_DCtx_refDDict, ZSTD_DCtx_refPrefix,
        ZSTD_DCtx_reset, ZSTD_DCtx_setParameter, ZSTD_DStream, ZSTD_DStreamInSize,
        ZSTD_DStreamOutSize, ZSTD_copyDCtx, ZSTD_createDCtx, ZSTD_createDStream,
        ZSTD_dParam_getBounds, ZSTD_decompress, ZSTD_decompressBegin,
        ZSTD_decompressBegin_usingDDict, ZSTD_decompressBegin_usingDict, ZSTD_decompressBound,
        ZSTD_decompressContinue, ZSTD_decompressDCtx, ZSTD_decompressStream,
        ZSTD_decompressStream_simpleArgs, ZSTD_decompress_usingDDict, ZSTD_decompress_usingDict,
        ZSTD_decompressionMargin, ZSTD_findDecompressedSize, ZSTD_findFrameCompressedSize,
        ZSTD_freeDCtx, ZSTD_getDecompressedSize, ZSTD_getDictID_fromDict, ZSTD_getDictID_fromFrame,
        ZSTD_getFrameContentSize, ZSTD_initDStream, ZSTD_initDStream_usingDDict,
        ZSTD_initDStream_usingDict, ZSTD_insertBlock, ZSTD_isFrame, ZSTD_isSkippableFrame,
        ZSTD_nextSrcSizeToDecompress, ZSTD_sizeof_DCtx,
    },
    zstd_decompress_block::ZSTD_decompressBlock,
    ZSTD_DCtx,
};

pub use crate::lib::common::zstd_common::{
    ZSTD_getErrorName, ZSTD_isError, ZSTD_versionNumber, ZSTD_versionString,
};

pub use crate::lib::dictBuilder::cover::{
    ZDICT_optimizeTrainFromBuffer_cover, ZDICT_trainFromBuffer_cover,
};
pub use crate::lib::dictBuilder::fastcover::{
    ZDICT_optimizeTrainFromBuffer_fastCover, ZDICT_trainFromBuffer_fastCover,
};
pub use crate::lib::dictBuilder::zdict::ZDICT_trainFromBuffer_legacy;
pub use crate::lib::zdict::{
    experimental::{ZDICT_cover_params_t, ZDICT_fastCover_params_t, ZDICT_legacy_params_t},
    ZDICT_getDictID, ZDICT_getErrorName, ZDICT_isError, ZDICT_params_t, ZDICT_trainFromBuffer,
};

#[allow(deprecated)] // We still export deprecated functions
pub use crate::lib::compress::zstd_compress::{
    ZSTD_CCtx, ZSTD_CCtx_getParameter, ZSTD_CCtx_loadDictionary, ZSTD_CCtx_refCDict,
    ZSTD_CCtx_refPrefix, ZSTD_CCtx_reset, ZSTD_CCtx_setParameter, ZSTD_CCtx_setPledgedSrcSize,
    ZSTD_CDict, ZSTD_CStreamInSize, ZSTD_CStreamOutSize, ZSTD_cParam_getBounds, ZSTD_compress,
    ZSTD_compress2, ZSTD_compressBlock, ZSTD_compressBound, ZSTD_compressCCtx,
    ZSTD_compressSequences, ZSTD_compressSequencesAndLiterals, ZSTD_compressStream,
    ZSTD_compressStream2, ZSTD_compress_usingCDict, ZSTD_compress_usingDict, ZSTD_copyCCtx,
    ZSTD_createCCtx, ZSTD_createCDict, ZSTD_createCDict_byReference, ZSTD_createCStream,
    ZSTD_endStream, ZSTD_flushStream, ZSTD_freeCCtx, ZSTD_freeCDict, ZSTD_generateSequences,
    ZSTD_getBlockSize, ZSTD_getDictID_fromCDict, ZSTD_getFrameProgression, ZSTD_initCStream,
    ZSTD_initCStream_srcSize, ZSTD_initCStream_usingCDict, ZSTD_initCStream_usingDict,
    ZSTD_maxCLevel, ZSTD_mergeBlockDelimiters, ZSTD_minCLevel, ZSTD_sequenceBound,
    ZSTD_sizeof_CCtx, ZSTD_sizeof_CDict, ZSTD_BLOCKSPLITTER_LEVEL_MAX,
};

pub mod internal {
    // Needed by benchzstd
    pub use crate::lib::common::xxhash::ZSTD_XXH64;

    // Needed by fileio
    pub use crate::lib::common::mem::{MEM_readLE24, MEM_readLE32};

    // Needed by fileio_asyncio
    pub use crate::lib::common::pool::{
        POOL_add, POOL_create, POOL_ctx, POOL_free, POOL_function, POOL_joinJobs,
    };
}

crate::lib::polyfill::cfg_select! {
    feature = "custom-prefix" => {
        #[cfg(feature = "export-symbols")]
        macro_rules! prefix {
            ($name:expr) => {
                concat!(env!("LIBZSTD_RS_SYS_PREFIX"), stringify!($name))
            };
        }
    }
    feature = "semver-prefix" => {
        #[cfg(feature = "export-symbols")]
        macro_rules! prefix {
            ($name:expr) => {
                concat!(
                    "LIBZSTD_RS_SYS_v",
                    env!("CARGO_PKG_VERSION_MAJOR"),
                    "_",
                    env!("CARGO_PKG_VERSION_MINOR"),
                    "_x_",
                    stringify!($name)
                )
            };
        }
    }
    _ => {
        #[cfg(feature = "export-symbols")]
        macro_rules! prefix {
            ($name:expr) => {
                stringify!($name)
            };
        }
    }
}

#[cfg(feature = "export-symbols")]
pub(crate) use prefix;
