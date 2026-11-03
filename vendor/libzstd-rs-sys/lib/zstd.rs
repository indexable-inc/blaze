use core::ffi::{c_int, c_uint, c_ulonglong};

#[cfg(doc)]
use crate::{
    ZSTD_CDict, ZSTD_DCtx, ZSTD_DCtx_refDDict, ZSTD_DCtx_reset, ZSTD_DCtx_setParameter, ZSTD_DDict,
    ZSTD_compressBound, ZSTD_compress_usingDict, ZSTD_decompress, ZSTD_freeDCtx,
};

pub const ZSTD_FRAMEHEADERSIZE_MAX: core::ffi::c_int = 18;
pub const ZSTD_SKIPPABLEHEADERSIZE: c_uint = 8;

// compression parameter bounds
pub const ZSTD_WINDOWLOG_MAX_32: core::ffi::c_int = 30;
pub const ZSTD_WINDOWLOG_MAX_64: core::ffi::c_int = 31;
pub const ZSTD_WINDOWLOG_MAX: core::ffi::c_int = match size_of::<usize>() {
    4 => ZSTD_WINDOWLOG_MAX_32,
    8 => ZSTD_WINDOWLOG_MAX_64,
    _ => panic!(),
};
pub const ZSTD_WINDOWLOG_MIN: core::ffi::c_int = 10;
pub const ZSTD_HASHLOG_MAX: core::ffi::c_int = if ZSTD_WINDOWLOG_MAX < 30 {
    ZSTD_WINDOWLOG_MAX
} else {
    30
};
pub const ZSTD_HASHLOG_MIN: core::ffi::c_int = 6;
pub const ZSTD_CHAINLOG_MAX_32: core::ffi::c_int = 29;
pub const ZSTD_CHAINLOG_MAX_64: core::ffi::c_int = 30;
pub const ZSTD_CHAINLOG_MAX: core::ffi::c_int = match size_of::<usize>() {
    4 => ZSTD_CHAINLOG_MAX_32,
    8 => ZSTD_CHAINLOG_MAX_64,
    _ => panic!(),
};
pub const ZSTD_CHAINLOG_MIN: core::ffi::c_int = ZSTD_HASHLOG_MIN;
pub const ZSTD_SEARCHLOG_MAX: core::ffi::c_int = ZSTD_WINDOWLOG_MAX - 1;
pub const ZSTD_SEARCHLOG_MIN: core::ffi::c_int = 1;
pub const ZSTD_MINMATCH_MAX: core::ffi::c_int = 7;
pub const ZSTD_MINMATCH_MIN: core::ffi::c_int = 3;
pub const ZSTD_TARGETLENGTH_MAX: core::ffi::c_int = ZSTD_BLOCKSIZE_MAX;
pub const ZSTD_TARGETLENGTH_MIN: core::ffi::c_int = 0;
pub const ZSTD_STRATEGY_MIN: core::ffi::c_int = ZSTD_fast as core::ffi::c_int;
pub const ZSTD_STRATEGY_MAX: core::ffi::c_int = ZSTD_btultra2 as core::ffi::c_int;

pub const ZSTD_OVERLAPLOG_MIN: core::ffi::c_int = 0;
pub const ZSTD_OVERLAPLOG_MAX: core::ffi::c_int = 9;

/// By default, the streaming decoder will refuse any frame requiring larger than
/// (1<<`ZSTD_WINDOWLOG_LIMIT_DEFAULT`) window size to preserve host's memory from unreasonable
/// requirements. This limit can be overridden using [`ZSTD_DCtx_setParameter`].
///
/// The limit does not apply for one-pass decoders (such as [`ZSTD_decompress`]), since no
/// additional memory is allocated.
pub const ZSTD_WINDOWLOG_LIMIT_DEFAULT: core::ffi::c_int = 27;
pub const ZSTD_WINDOWLOG_ABSOLUTEMIN: core::ffi::c_int = 10;

// LDM parameter bounds
pub const ZSTD_LDM_HASHLOG_MIN: core::ffi::c_int = ZSTD_HASHLOG_MIN;
pub const ZSTD_LDM_HASHLOG_MAX: core::ffi::c_int = ZSTD_HASHLOG_MAX;
pub const ZSTD_LDM_MINMATCH_MIN: core::ffi::c_int = 4;
pub const ZSTD_LDM_MINMATCH_MAX: core::ffi::c_int = 4096;
pub const ZSTD_LDM_BUCKETSIZELOG_MIN: core::ffi::c_int = 1;
pub const ZSTD_LDM_BUCKETSIZELOG_MAX: core::ffi::c_int = 8;
pub const ZSTD_LDM_HASHRATELOG_MIN: core::ffi::c_int = 0;
pub const ZSTD_LDM_HASHRATELOG_MAX: core::ffi::c_int = ZSTD_WINDOWLOG_MAX - ZSTD_HASHLOG_MIN;

// Advanced parameter bounds
pub const ZSTD_TARGETCBLOCKSIZE_MIN: core::ffi::c_int = 1340;
pub const ZSTD_TARGETCBLOCKSIZE_MAX: core::ffi::c_int = ZSTD_BLOCKSIZE_MAX;
pub const ZSTD_SRCSIZEHINT_MIN: core::ffi::c_int = 0;
pub const ZSTD_SRCSIZEHINT_MAX: core::ffi::c_int = core::ffi::c_int::MAX;

pub const ZSTD_BLOCKSIZELOG_MAX: c_int = 17;
pub const ZSTD_BLOCKSIZE_MAX: c_int = 1 << ZSTD_BLOCKSIZELOG_MAX;
/// The minimum valid max blocksize. Maximum blocksizes smaller than this make [`ZSTD_compressBound`] inaccurate.
pub const ZSTD_BLOCKSIZE_MAX_MIN: core::ffi::c_int = 1 << 10;
pub const ZSTD_CLEVEL_DEFAULT: c_int = 3;

#[cfg(target_pointer_width = "64")]
pub const ZSTD_MAX_INPUT_SIZE: usize = 0xff00ff00ff00ff00;
#[cfg(not(target_pointer_width = "64"))]
pub const ZSTD_MAX_INPUT_SIZE: usize = 0xff00ff00;

pub const ZSTD_MAGICNUMBER: c_uint = 0xfd2fb528;
pub const ZSTD_MAGIC_DICTIONARY: c_uint = 0xec30a437;
pub const ZSTD_MAGIC_SKIPPABLE_START: c_uint = 0x184d2a50;
pub const ZSTD_MAGIC_SKIPPABLE_MASK: c_uint = 0xfffffff0;

pub const ZSTD_VERSION_MAJOR: c_uint = 1;
pub const ZSTD_VERSION_MINOR: c_uint = 5;
pub const ZSTD_VERSION_RELEASE: c_uint = 8;
pub const ZSTD_VERSION_NUMBER: c_uint =
    ZSTD_VERSION_MAJOR * 100 * 100 + ZSTD_VERSION_MINOR * 100 + ZSTD_VERSION_RELEASE;

pub const ZSTD_CONTENTSIZE_UNKNOWN: c_ulonglong = (0 as c_ulonglong).wrapping_sub(1);
pub const ZSTD_CONTENTSIZE_ERROR: c_ulonglong = (0 as c_ulonglong).wrapping_sub(2);

pub type ZSTD_ErrorCode = core::ffi::c_uint;

pub const ZSTD_error_maxCode: ZSTD_ErrorCode = 120;
pub const ZSTD_error_externalSequences_invalid: ZSTD_ErrorCode = 107;
pub const ZSTD_error_sequenceProducer_failed: ZSTD_ErrorCode = 106;
pub const ZSTD_error_srcBuffer_wrong: ZSTD_ErrorCode = 105;
pub const ZSTD_error_dstBuffer_wrong: ZSTD_ErrorCode = 104;
pub const ZSTD_error_seekableIO: ZSTD_ErrorCode = 102;
pub const ZSTD_error_frameIndex_tooLarge: ZSTD_ErrorCode = 100;
pub const ZSTD_error_noForwardProgress_inputEmpty: ZSTD_ErrorCode = 82;
pub const ZSTD_error_noForwardProgress_destFull: ZSTD_ErrorCode = 80;
pub const ZSTD_error_dstBuffer_null: ZSTD_ErrorCode = 74;
pub const ZSTD_error_srcSize_wrong: ZSTD_ErrorCode = 72;
pub const ZSTD_error_dstSize_tooSmall: ZSTD_ErrorCode = 70;
pub const ZSTD_error_workSpace_tooSmall: ZSTD_ErrorCode = 66;
pub const ZSTD_error_memory_allocation: ZSTD_ErrorCode = 64;
pub const ZSTD_error_init_missing: ZSTD_ErrorCode = 62;
pub const ZSTD_error_stage_wrong: ZSTD_ErrorCode = 60;
pub const ZSTD_error_stabilityCondition_notRespected: ZSTD_ErrorCode = 50;
pub const ZSTD_error_cannotProduce_uncompressedBlock: ZSTD_ErrorCode = 49;
pub const ZSTD_error_maxSymbolValue_tooSmall: ZSTD_ErrorCode = 48;
pub const ZSTD_error_maxSymbolValue_tooLarge: ZSTD_ErrorCode = 46;
pub const ZSTD_error_tableLog_tooLarge: ZSTD_ErrorCode = 44;
pub const ZSTD_error_parameter_outOfBound: ZSTD_ErrorCode = 42;
pub const ZSTD_error_parameter_combination_unsupported: ZSTD_ErrorCode = 41;
pub const ZSTD_error_parameter_unsupported: ZSTD_ErrorCode = 40;
pub const ZSTD_error_dictionaryCreation_failed: ZSTD_ErrorCode = 34;
pub const ZSTD_error_dictionary_wrong: ZSTD_ErrorCode = 32;
pub const ZSTD_error_dictionary_corrupted: ZSTD_ErrorCode = 30;
pub const ZSTD_error_literals_headerWrong: ZSTD_ErrorCode = 24;
pub const ZSTD_error_checksum_wrong: ZSTD_ErrorCode = 22;
pub const ZSTD_error_corruption_detected: ZSTD_ErrorCode = 20;
pub const ZSTD_error_frameParameter_windowTooLarge: ZSTD_ErrorCode = 16;
pub const ZSTD_error_frameParameter_unsupported: ZSTD_ErrorCode = 14;
pub const ZSTD_error_version_unsupported: ZSTD_ErrorCode = 12;
pub const ZSTD_error_prefix_unknown: ZSTD_ErrorCode = 10;
pub const ZSTD_error_GENERIC: ZSTD_ErrorCode = 1;
pub const ZSTD_error_no_error: ZSTD_ErrorCode = 0;

pub type ZSTD_strategy = core::ffi::c_uint;
pub const ZSTD_btultra2: ZSTD_strategy = 9;
pub const ZSTD_btultra: ZSTD_strategy = 8;
pub const ZSTD_btopt: ZSTD_strategy = 7;
pub const ZSTD_btlazy2: ZSTD_strategy = 6;
pub const ZSTD_lazy2: ZSTD_strategy = 5;
pub const ZSTD_lazy: ZSTD_strategy = 4;
pub const ZSTD_greedy: ZSTD_strategy = 3;
pub const ZSTD_dfast: ZSTD_strategy = 2;
pub const ZSTD_fast: ZSTD_strategy = 1;

pub type ZSTD_EndDirective = core::ffi::c_uint;
/// Flush any remaining data _and_ close current frame.
/// note that frame is only closed after compressed data is fully flushed (return value == 0).
/// After that point, any additional data starts a new frame.
/// note : each frame is independent (does not reference any content from previous frame).
/// note : multithreaded compression will block to flush as much output as possible.
pub const ZSTD_e_end: ZSTD_EndDirective = 2;
/// Flush any data provided so far,
/// it creates (at least) one new block, that can be decoded immediately on reception;
/// frame will continue: any future data can still reference previously compressed data, improving compression.
/// note : multithreaded compression will block to flush as much output as possible.
pub const ZSTD_e_flush: ZSTD_EndDirective = 1;
/// Collect more data, encoder decides when to output compressed result, for optimal compression ratio.
pub const ZSTD_e_continue: ZSTD_EndDirective = 0;

#[derive(Copy, Clone)]
#[repr(C)]
pub struct ZSTD_Sequence {
    /// The offset of the match. (NOT the same as the offset code)
    /// If offset == 0 and matchLength == 0, this sequence represents the last
    /// literals in the block of litLength size.
    pub offset: core::ffi::c_uint,
    /// Literal length of the sequence.
    pub litLength: core::ffi::c_uint,
    /// Match length of the sequence.
    ///
    /// Note: Users of this API may provide a sequence with matchLength == litLength == offset == 0.
    /// In this case, we will treat the sequence as a marker for a block boundary.
    pub matchLength: core::ffi::c_uint,
    /// Represents which repeat offset is represented by the field 'offset'.
    /// Ranges from `[0, 3]`.
    ///
    /// Repeat offsets are essentially previous offsets from previous sequences sorted in
    /// recency order. For more detail, see doc/zstd_compression_format.md
    ///
    /// If rep == 0, then 'offset' does not contain a repeat offset.
    /// If rep > 0:
    ///  If litLength != 0:
    ///      rep == 1 --> offset == repeat_offset_1
    ///      rep == 2 --> offset == repeat_offset_2
    ///      rep == 3 --> offset == repeat_offset_3
    ///  If litLength == 0:
    ///      rep == 1 --> offset == repeat_offset_2
    ///      rep == 2 --> offset == repeat_offset_3
    ///      rep == 3 --> offset == repeat_offset_1 - 1
    ///
    /// Note: This field is optional. `ZSTD_generateSequences()` will calculate the value of
    /// 'rep', but repeat offsets do not necessarily need to be calculated from an external
    /// sequence provider perspective. For example, `ZSTD_compressSequences()` does not
    /// use this 'rep' field at all (as of now).
    pub rep: core::ffi::c_uint,
}

#[derive(Debug, Copy, Clone, Default)]
#[repr(C)]
pub struct ZSTD_compressionParameters {
    pub windowLog: core::ffi::c_uint,
    pub chainLog: core::ffi::c_uint,
    pub hashLog: core::ffi::c_uint,
    pub searchLog: core::ffi::c_uint,
    pub minMatch: core::ffi::c_uint,
    pub targetLength: core::ffi::c_uint,
    pub strategy: ZSTD_strategy,
}

#[derive(Debug, Copy, Clone, Default)]
#[repr(C)]
pub struct ZSTD_frameParameters {
    pub contentSizeFlag: core::ffi::c_int,
    pub checksumFlag: core::ffi::c_int,
    pub noDictIDFlag: core::ffi::c_int,
}

#[derive(Debug, Copy, Clone, Default)]
#[repr(C)]
pub struct ZSTD_parameters {
    pub cParams: ZSTD_compressionParameters,
    pub fParams: ZSTD_frameParameters,
}

/// This enum specifies the content type of a dictionary, either [`ZSTD_dct_auto`], [`ZSTD_dct_rawContent`], or [`ZSTD_dct_fullDict`].
pub type ZSTD_dictContentType_e = core::ffi::c_uint;
/// Refuses to load a dictionary if it does not respect Zstandard's specification, starting with
/// [`ZSTD_MAGIC_DICTIONARY`]
pub const ZSTD_dct_fullDict: ZSTD_dictContentType_e = 2;
/// Ensures dictionary is always loaded as `rawContent`, even if it starts with [`ZSTD_MAGIC_DICTIONARY`]
pub const ZSTD_dct_rawContent: ZSTD_dictContentType_e = 1;
/// Dictionary is "full" when starting with [`ZSTD_MAGIC_DICTIONARY`], otherwise it is "rawContent"
pub const ZSTD_dct_auto: ZSTD_dictContentType_e = 0;

/// This enum specifies the method used to load a dictionary, either [`ZSTD_dlm_byCopy`] or [`ZSTD_dlm_byRef`].
pub type ZSTD_dictLoadMethod_e = core::ffi::c_uint;
/// Copy dictionary content internally
pub const ZSTD_dlm_byCopy: ZSTD_dictLoadMethod_e = 0;
/// Reference dictionary content -- the dictionary buffer must outlive its users
pub const ZSTD_dlm_byRef: ZSTD_dictLoadMethod_e = 1;

#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct ZSTD_customMem {
    pub customAlloc: ZSTD_allocFunction,
    pub customFree: ZSTD_freeFunction,
    pub opaque: *mut core::ffi::c_void,
}
pub type ZSTD_freeFunction =
    Option<unsafe extern "C" fn(*mut core::ffi::c_void, *mut core::ffi::c_void) -> ()>;
pub type ZSTD_allocFunction =
    Option<unsafe extern "C" fn(*mut core::ffi::c_void, usize) -> *mut core::ffi::c_void>;

#[derive(PartialEq)]
#[repr(transparent)]
pub struct ZSTD_format_e(u32);

impl ZSTD_format_e {
    /// zstd frame format, specified in `zstd_compression_format.md` (default)
    pub const ZSTD_f_zstd1: Self = Self(Format::ZSTD_f_zstd1 as u32);

    /// Variant of zstd frame format, without initial 4-bytes magic number.
    ///
    /// Useful to save 4 bytes per generated frame.
    ///
    /// The decoder cannot recognize this format automatically, thus requiring this instruction.
    pub const ZSTD_f_zstd1_magicless: Self = Self(Format::ZSTD_f_zstd1_magicless as u32);
}

impl From<ZSTD_format_e> for u32 {
    fn from(value: ZSTD_format_e) -> Self {
        value.0
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
#[repr(u32)]
pub enum Format {
    #[default]
    ZSTD_f_zstd1 = 0,
    ZSTD_f_zstd1_magicless = 1,
}

impl Format {
    /// This function replaces the `ZSTD_FRAMEHEADERSIZE_MIN` macro.
    ///
    /// Returns the minimum size of the frame header.
    pub const fn frame_header_size_min(self) -> usize {
        match self {
            Format::ZSTD_f_zstd1 => 6,
            Format::ZSTD_f_zstd1_magicless => 2,
        }
    }

    /// This function replaces the `ZSTD_FRAMEHEADERSIZE_PREFIX` macro.
    ///
    /// Returns the minimum input size required to query the frame header size.
    pub const fn starting_input_length(self) -> usize {
        match self {
            Format::ZSTD_f_zstd1 => 5,
            Format::ZSTD_f_zstd1_magicless => 1,
        }
    }
}

impl TryFrom<ZSTD_format_e> for Format {
    type Error = ();

    fn try_from(value: ZSTD_format_e) -> Result<Self, Self::Error> {
        match value {
            ZSTD_format_e::ZSTD_f_zstd1 => Ok(Self::ZSTD_f_zstd1),
            ZSTD_format_e::ZSTD_f_zstd1_magicless => Ok(Self::ZSTD_f_zstd1_magicless),
            _ => Err(()),
        }
    }
}

impl TryFrom<i32> for Format {
    type Error = ();

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::ZSTD_f_zstd1),
            1 => Ok(Self::ZSTD_f_zstd1_magicless),
            _ => Err(()),
        }
    }
}

pub type ZSTD_inBuffer = ZSTD_inBuffer_s;
#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct ZSTD_inBuffer_s {
    /// Pointer to start of input buffer
    pub src: *const core::ffi::c_void,
    /// Size of input buffer
    pub size: usize,
    /// Position where reading stopped. Will be updated. Necessarily `0 <= pos <= size`.
    pub pos: usize,
}

pub type ZSTD_outBuffer = ZSTD_outBuffer_s;
#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct ZSTD_outBuffer_s {
    /// Pointer to start of output buffer
    pub dst: *mut core::ffi::c_void,
    /// Size of output buffer
    pub size: usize,
    /// Position where writing stopped. Will be updated. Necessarily `0 <= pos <= size`.
    pub pos: usize,
}

pub type ZSTD_bufferMode_e = core::ffi::c_uint;
pub const ZSTD_bm_stable: ZSTD_bufferMode_e = 1;
pub const ZSTD_bm_buffered: ZSTD_bufferMode_e = 0;

#[repr(u32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum BufferMode {
    Buffered,
    Stable,
}

impl TryFrom<u32> for BufferMode {
    type Error = ();

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Buffered),
            1 => Ok(Self::Stable),
            _ => Err(()),
        }
    }
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct ZSTD_frameProgression {
    pub ingested: core::ffi::c_ulonglong,
    pub consumed: core::ffi::c_ulonglong,
    pub produced: core::ffi::c_ulonglong,
    pub flushed: core::ffi::c_ulonglong,
    pub currentJobID: core::ffi::c_uint,
    pub nbActiveWorkers: core::ffi::c_uint,
}

pub mod experimental {
    pub use crate::lib::common::pool::{
        ZSTD_createThreadPool, ZSTD_freeThreadPool, ZSTD_threadPool,
    };
}

/// The advanced API pushes parameters one by one into an existing `ZSTD_DCtx` decompression
/// context. Parameters are sticky, and remain valid for all following frames using the same
/// context.
///
/// It's possible to reset parameters to default values using [`ZSTD_DCtx_reset`].
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZSTD_dParameter(u32);

impl ZSTD_dParameter {
    pub const ZSTD_d_experimentalParam1: Self = Self(1000);
    pub const ZSTD_d_experimentalParam2: Self = Self(1001);
    pub const ZSTD_d_experimentalParam3: Self = Self(1002);
    pub const ZSTD_d_experimentalParam4: Self = Self(1003);
    pub const ZSTD_d_experimentalParam5: Self = Self(1004);
    pub const ZSTD_d_experimentalParam6: Self = Self(1005);

    /// Experimental parameter: allowing selection between [`ZSTD_format_e`] input compression formats
    pub const ZSTD_d_format: Self = Self::ZSTD_d_experimentalParam1;

    /// Experimental parameter: tells the decompressor that the [`ZSTD_outBuffer`] will ALWAYS be
    /// the same between calls, except for the modifications that zstd makes to pos (the caller
    /// must not modify pos). This is checked by the decompressor, and decompression will fail if
    /// it ever changes. Therefore the `ZSTD_outBuffer` MUST be large enough to fit the entire
    /// decompressed frame. This will be checked when the frame content size is known. The data in
    /// the `ZSTD_outBuffer` in the range `dst..(dst + pos)` MUST not be modified during
    /// decompression or you will get data corruption.
    ///
    /// When this flag is enabled zstd won't allocate an output buffer, because it can write
    /// directly to the `ZSTD_outBuffer`, but it will still allocate an input buffer large enough
    /// to fit any compressed block. This will also avoid the `memcpy()` from the internal output
    /// buffer to the `ZSTD_outBuffer`. If you need to avoid the input buffer allocation use the
    /// buffer-less streaming API.
    ///
    /// Note: So long as the `ZSTD_outBuffer` always points to valid memory, using this flag is
    /// ALWAYS memory safe, and will never access out-of-bounds memory. However, decompression WILL
    /// fail if you violate the preconditions.
    ///
    /// **Warning:** The data in the `ZSTD_outBuffer` in the range `dst..(dst + pos)` MUST not be
    /// modified during decompression or you will get data corruption. This is because zstd needs
    /// to reference data in the `ZSTD_outBuffer` to regenerate matches. Normally zstd maintains
    /// its own buffer for this purpose, but passing this flag tells zstd to use the user provided
    /// buffer.
    ///
    /// Default is 0 == disabled. Set to 1 to enable.
    pub const ZSTD_d_stableOutBuffer: Self = Self::ZSTD_d_experimentalParam2;

    /// Experimental parameter: tells the decompressor to skip checksum validation during
    /// decompression, regardless of whether checksumming was specified during compression. This
    /// offers some slight performance benefits, and may be useful for debugging.
    ///
    /// Default is 0 == disabled. Set to 1 to enable.
    pub const ZSTD_d_forceIgnoreChecksum: Self = Self::ZSTD_d_experimentalParam3;

    /// Experimental parameter: if enabled and [`ZSTD_DCtx`] is allocated on the heap, then
    /// additional memory will be allocated to store references to multiple [`ZSTD_DDict`]. That
    /// is, multiple calls of [`ZSTD_DCtx_refDDict`] using a given [`ZSTD_DCtx`], rather than
    /// overwriting the previous `DDict` reference, will instead store all references. At
    /// decompression time, the appropriate `dictID` is selected from the set of `DDict`s based on
    /// the `dictID` in the frame.
    ///
    /// **Warning:** Enabling this parameter and calling [`ZSTD_DCtx_refDDict`] will trigger memory
    /// allocation for the hash table. [`ZSTD_freeDCtx`] also frees this memory. Memory is
    /// allocated as per `ZSTD_DCtx::customMem`.
    ///
    /// Although this function allocates memory for the table, the user is still responsible for
    /// memory management of the underlying [`ZSTD_DDict`] themselves.
    ///
    /// Default is 0 == disabled. Set to 1 to enable.
    pub const ZSTD_d_refMultipleDDicts: Self = Self::ZSTD_d_experimentalParam4;

    /// This parameter can be used to disable Huffman assembly at runtime.
    ///
    /// Set to 1 to disable the Huffman assembly implementation. The default value is 0, which
    /// allows zstd to use the Huffman assembly implementation if available.
    pub const ZSTD_d_disableHuffmanAssembly: Self = Self::ZSTD_d_experimentalParam5;

    /// Forces the decompressor to reject blocks whose content size is larger than the configured
    /// `maxBlockSize`. When `maxBlockSize` is larger than the `windowSize`, the `windowSize` is
    /// used instead. This saves memory on the decoder when you know all blocks are small.
    ///
    /// Allowed values are between 1KB and [`ZSTD_BLOCKSIZE_MAX`] (128KB).
    /// The default is [`ZSTD_BLOCKSIZE_MAX`], and setting to 0 will set to the default.
    ///
    /// This option is typically used in conjunction with [`ZSTD_cParameter::ZSTD_c_maxBlockSize`].
    ///
    /// **Warning:** This causes the decoder to reject otherwise valid frames that have block sizes
    /// larger than the configured `maxBlockSize`.
    pub const ZSTD_d_maxBlockSize: Self = Self::ZSTD_d_experimentalParam6;

    /// Select a size limit (in power of 2) beyond which the streaming API will refuse to allocate
    /// memory buffer in order to protect the host from unreasonable memory requirements.
    ///
    /// This parameter is only useful in streaming mode, since no internal buffer is allocated in
    /// single-pass mode. By default, a decompression context accepts window sizes less than or
    /// equal to (1 << [`ZSTD_WINDOWLOG_LIMIT_DEFAULT`]).
    ///
    /// A value of 0 means "use default maximum `windowLog`".
    pub const ZSTD_d_windowLogMax: Self = Self(100);
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForceIgnoreChecksum {
    ValidateChecksum = 0,
    IgnoreChecksum = 1,
}

impl TryFrom<i32> for ForceIgnoreChecksum {
    type Error = ();

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::ValidateChecksum),
            1 => Ok(Self::IgnoreChecksum),
            _ => Err(()),
        }
    }
}

#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZSTD_cParameter(pub(crate) u32);

impl ZSTD_cParameter {
    pub const ZSTD_c_experimentalParam1: Self = Self(500);
    pub const ZSTD_c_experimentalParam2: Self = Self(10);
    pub const ZSTD_c_experimentalParam3: Self = Self(1000);
    pub const ZSTD_c_experimentalParam4: Self = Self(1001);
    pub const ZSTD_c_experimentalParam5: Self = Self(1002);
    pub const ZSTD_c_experimentalParam7: Self = Self(1004);
    pub const ZSTD_c_experimentalParam8: Self = Self(1005);
    pub const ZSTD_c_experimentalParam9: Self = Self(1006);
    pub const ZSTD_c_experimentalParam10: Self = Self(1007);
    pub const ZSTD_c_experimentalParam11: Self = Self(1008);
    pub const ZSTD_c_experimentalParam12: Self = Self(1009);
    pub const ZSTD_c_experimentalParam13: Self = Self(1010);
    pub const ZSTD_c_experimentalParam14: Self = Self(1011);
    pub const ZSTD_c_experimentalParam15: Self = Self(1012);
    pub const ZSTD_c_experimentalParam16: Self = Self(1013);
    pub const ZSTD_c_experimentalParam17: Self = Self(1014);
    pub const ZSTD_c_experimentalParam18: Self = Self(1015);
    pub const ZSTD_c_experimentalParam19: Self = Self(1016);
    pub const ZSTD_c_experimentalParam20: Self = Self(1017);

    pub const ZSTD_c_rsyncable: Self = Self::ZSTD_c_experimentalParam1;
    pub const ZSTD_c_format: Self = Self::ZSTD_c_experimentalParam2;
    pub const ZSTD_c_forceMaxWindow: Self = Self::ZSTD_c_experimentalParam3;
    pub const ZSTD_c_forceAttachDict: Self = Self::ZSTD_c_experimentalParam4;
    pub const ZSTD_c_literalCompressionMode: Self = Self::ZSTD_c_experimentalParam5;
    pub const ZSTD_c_srcSizeHint: Self = Self::ZSTD_c_experimentalParam7;
    pub const ZSTD_c_enableDedicatedDictSearch: Self = Self::ZSTD_c_experimentalParam8;
    pub const ZSTD_c_stableInBuffer: Self = Self::ZSTD_c_experimentalParam9;
    pub const ZSTD_c_stableOutBuffer: Self = Self::ZSTD_c_experimentalParam10;
    pub const ZSTD_c_blockDelimiters: Self = Self::ZSTD_c_experimentalParam11;
    pub const ZSTD_c_validateSequences: Self = Self::ZSTD_c_experimentalParam12;
    pub const ZSTD_c_blockSplitterLevel: Self = Self::ZSTD_c_experimentalParam20;
    pub const ZSTD_c_splitAfterSequences: Self = Self::ZSTD_c_experimentalParam13;
    pub const ZSTD_c_useRowMatchFinder: Self = Self::ZSTD_c_experimentalParam14;
    pub const ZSTD_c_deterministicRefPrefix: Self = Self::ZSTD_c_experimentalParam15;
    pub const ZSTD_c_prefetchCDictTables: Self = Self::ZSTD_c_experimentalParam16;
    pub const ZSTD_c_enableSeqProducerFallback: Self = Self::ZSTD_c_experimentalParam17;
    pub const ZSTD_c_maxBlockSize: Self = Self::ZSTD_c_experimentalParam18;
    pub const ZSTD_c_repcodeResolution: Self = Self::ZSTD_c_experimentalParam19;
    /// Older name of [`ZSTD_cParameter::ZSTD_c_repcodeResolution`]
    pub const ZSTD_c_searchForExternalRepcodes: Self = Self::ZSTD_c_experimentalParam19;

    pub const ZSTD_c_compressionLevel: Self = Self(100);
    pub const ZSTD_c_windowLog: Self = Self(101);
    pub const ZSTD_c_hashLog: Self = Self(102);
    pub const ZSTD_c_chainLog: Self = Self(103);
    pub const ZSTD_c_searchLog: Self = Self(104);
    pub const ZSTD_c_minMatch: Self = Self(105);
    pub const ZSTD_c_targetLength: Self = Self(106);
    pub const ZSTD_c_strategy: Self = Self(107);
    pub const ZSTD_c_targetCBlockSize: Self = Self(130);
    pub const ZSTD_c_enableLongDistanceMatching: Self = Self(160);
    pub const ZSTD_c_ldmHashLog: Self = Self(161);
    pub const ZSTD_c_ldmMinMatch: Self = Self(162);
    pub const ZSTD_c_ldmBucketSizeLog: Self = Self(163);
    pub const ZSTD_c_ldmHashRateLog: Self = Self(164);
    pub const ZSTD_c_contentSizeFlag: Self = Self(200);
    pub const ZSTD_c_checksumFlag: Self = Self(201);
    pub const ZSTD_c_dictIDFlag: Self = Self(202);
    pub const ZSTD_c_nbWorkers: Self = Self(400);
    pub const ZSTD_c_jobSize: Self = Self(401);
    pub const ZSTD_c_overlapLog: Self = Self(402);
}

#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZSTD_ResetDirective(pub(crate) u32);
impl ZSTD_ResetDirective {
    pub const ZSTD_reset_session_only: Self = Self(1);
    pub const ZSTD_reset_parameters: Self = Self(2);
    pub const ZSTD_reset_session_and_parameters: Self = Self(3);
}

/// Note: this enum and the behavior it controls are effectively internal
/// implementation details of the compressor. They are expected to continue
/// to evolve and should be considered only in the context of extremely
/// advanced performance tuning.
///
/// Zstd currently supports the use of a [`ZSTD_CDict`] in three ways:
///
/// - The contents of the `CDict` can be copied into the working context. This
///   means that the compression can search both the dictionary and input
///   while operating on a single set of internal tables. This makes
///   the compression faster per byte of input. However, the initial copy of
///   the `CDict`'s tables incurs a fixed cost at the beginning of the
///   compression. For small compressions (< 8 KB), that copy can dominate
///   the cost of the compression.
///
/// - The `CDict`'s tables can be used in-place. In this model, compression is
///   slower per input byte, because the compressor has to search two sets of
///   tables. However, this model incurs no start-up cost (as long as the
///   working context's tables can be reused). For small inputs, this can be
///   faster than copying the `CDict`'s tables.
///
/// - The `CDict`'s tables are not used at all, and instead we use the working
///   context alone to reload the dictionary and use params based on the source
///   size. See `ZSTD_compress_insertDictionary` and [`ZSTD_compress_usingDict`].
///   This method is effective when the dictionary sizes are very small relative
///   to the input size, and the input size is fairly large to begin with.
///
/// Zstd has a simple internal heuristic that selects which strategy to use
/// at the beginning of a compression. However, if experimentation shows that
/// Zstd is making poor choices, it is possible to override that choice with
/// this enum.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ZSTD_dictAttachPref_e(pub(crate) u32);
impl ZSTD_dictAttachPref_e {
    /// Use the default heuristic
    pub const ZSTD_dictDefaultAttach: Self = Self(0);
    /// Never copy the dictionary
    pub const ZSTD_dictForceAttach: Self = Self(1);
    /// Always copy the dictionary
    pub const ZSTD_dictForceCopy: Self = Self(2);
    /// Always reload the dictionary
    pub const ZSTD_dictForceLoad: Self = Self(3);
}

impl TryFrom<i32> for ZSTD_dictAttachPref_e {
    type Error = ();

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::ZSTD_dictDefaultAttach),
            1 => Ok(Self::ZSTD_dictForceAttach),
            2 => Ok(Self::ZSTD_dictForceCopy),
            3 => Ok(Self::ZSTD_dictForceLoad),
            _ => Err(()),
        }
    }
}

/// Note: this enum controls features which are conditionally beneficial.
///
/// Zstd can take a decision on whether or not to enable the feature (`ZSTD_ps_auto`),
/// or set the switch to `ZSTD_ps_enable` or `ZSTD_ps_disable` force enable/disable the feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ParamSwitch {
    /// Let the library automatically determine whether the feature shall be enabled
    #[default]
    Auto = 0,
    /// Force-enable the feature
    Enable = 1,
    /// Force-disable the feature
    Disable = 2,
}

impl ParamSwitch {
    /// The lower and upper bound of this enum
    pub const BOUNDS: (core::ffi::c_int, core::ffi::c_int) = (Self::Auto as _, Self::Disable as _);

    pub fn to_i32(self) -> i32 {
        self as i32
    }
}

impl TryFrom<i32> for ParamSwitch {
    type Error = ();

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Auto),
            1 => Ok(Self::Enable),
            2 => Ok(Self::Disable),
            _ => Err(()),
        }
    }
}

pub type ZSTD_SequenceFormat_e = core::ffi::c_uint;
/// `ZSTD_Sequence[]` has no block delimiters, just sequences
pub const ZSTD_sf_noBlockDelimiters: ZSTD_SequenceFormat_e = 0;
/// `ZSTD_Sequence[]` contains explicit block delimiters
pub const ZSTD_sf_explicitBlockDelimiters: ZSTD_SequenceFormat_e = 1;

pub type ZSTD_sequenceProducer_F = unsafe extern "C" fn(
    *mut core::ffi::c_void,
    *mut ZSTD_Sequence,
    usize,
    *const core::ffi::c_void,
    usize,
    *const core::ffi::c_void,
    usize,
    core::ffi::c_int,
    usize,
) -> usize;
