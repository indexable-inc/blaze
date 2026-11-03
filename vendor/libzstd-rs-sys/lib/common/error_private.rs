use core::ffi::c_char;

use crate::lib::zstd::{ZSTD_ErrorCode, ZSTD_error_maxCode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    #[allow(clippy::upper_case_acronyms)]
    GENERIC = 1,
    prefix_unknown = 10,
    version_unsupported = 12,
    frameParameter_unsupported = 14,
    frameParameter_windowTooLarge = 16,
    corruption_detected = 20,
    checksum_wrong = 22,
    literals_headerWrong = 24,
    dictionary_corrupted = 30,
    dictionary_wrong = 32,
    dictionaryCreation_failed = 34,
    parameter_unsupported = 40,
    parameter_combination_unsupported = 41,
    parameter_outOfBound = 42,
    tableLog_tooLarge = 44,
    maxSymbolValue_tooLarge = 46,
    maxSymbolValue_tooSmall = 48,
    cannotProduce_uncompressedBlock = 49,
    stabilityCondition_notRespected = 50,
    stage_wrong = 60,
    init_missing = 62,
    memory_allocation = 64,
    workSpace_tooSmall = 66,
    dstSize_tooSmall = 70,
    srcSize_wrong = 72,
    dstBuffer_null = 74,
    noForwardProgress_destFull = 80,
    noForwardProgress_inputEmpty = 82,
    frameIndex_tooLarge = 100,
    seekableIO = 102,
    dstBuffer_wrong = 104,
    srcBuffer_wrong = 105,
    sequenceProducer_failed = 106,
    externalSequences_invalid = 107,
    maxCode = 120,
}

impl Error {
    pub fn to_error_code(self) -> usize {
        -(self as core::ffi::c_int) as usize
    }

    #[allow(unused)]
    pub fn from_error_code(code: usize) -> Option<Self> {
        if !ERR_isError(code) {
            return None;
        }

        Self::try_from(code.wrapping_neg() as u32).ok()
    }
}

impl TryFrom<u32> for Error {
    type Error = ();

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        use Error::*;

        Ok(match value {
            1 => GENERIC,
            10 => prefix_unknown,
            12 => version_unsupported,
            14 => frameParameter_unsupported,
            16 => frameParameter_windowTooLarge,
            20 => corruption_detected,
            22 => checksum_wrong,
            24 => literals_headerWrong,
            30 => dictionary_corrupted,
            32 => dictionary_wrong,
            34 => dictionaryCreation_failed,
            40 => parameter_unsupported,
            41 => parameter_combination_unsupported,
            42 => parameter_outOfBound,
            44 => tableLog_tooLarge,
            46 => maxSymbolValue_tooLarge,
            48 => maxSymbolValue_tooSmall,
            49 => cannotProduce_uncompressedBlock,
            50 => stabilityCondition_notRespected,
            60 => stage_wrong,
            62 => init_missing,
            64 => memory_allocation,
            66 => workSpace_tooSmall,
            70 => dstSize_tooSmall,
            72 => srcSize_wrong,
            74 => dstBuffer_null,
            80 => noForwardProgress_destFull,
            82 => noForwardProgress_inputEmpty,
            100 => frameIndex_tooLarge,
            102 => seekableIO,
            104 => dstBuffer_wrong,
            105 => srcBuffer_wrong,
            106 => sequenceProducer_failed,
            107 => externalSequences_invalid,
            120 => maxCode,
            _ => return Err(()),
        })
    }
}

type ERR_enum = ZSTD_ErrorCode;

pub(crate) const fn ERR_isError(code: usize) -> bool {
    code > -(ZSTD_error_maxCode as core::ffi::c_int) as usize
}

pub(crate) const fn ERR_getErrorCode(code: usize) -> ZSTD_ErrorCode {
    if !ERR_isError(code) {
        return 0;
    }

    code.wrapping_neg() as _
}

pub(crate) fn ERR_getErrorString(code: ERR_enum) -> *const c_char {
    match code as core::ffi::c_uint {
        0 => c"No error detected".as_ptr(),
        1 => c"Error (generic)".as_ptr(),
        10 => c"Unknown frame descriptor".as_ptr(),
        12 => c"Version not supported".as_ptr(),
        14 => c"Unsupported frame parameter".as_ptr(),
        16 => c"Frame requires too much memory for decoding".as_ptr(),
        20 => c"Data corruption detected".as_ptr(),
        22 => c"Restored data doesn't match checksum".as_ptr(),
        24 => c"Header of Literals' block doesn't respect format specification".as_ptr(),
        40 => c"Unsupported parameter".as_ptr(),
        41 => c"Unsupported combination of parameters".as_ptr(),
        42 => c"Parameter is out of bound".as_ptr(),
        62 => c"Context should be init first".as_ptr(),
        64 => c"Allocation error : not enough memory".as_ptr(),
        66 => c"workSpace buffer is not large enough".as_ptr(),
        60 => c"Operation not authorized at current processing stage".as_ptr(),
        44 => c"tableLog requires too much memory : unsupported".as_ptr(),
        46 => c"Unsupported max Symbol Value : too large".as_ptr(),
        48 => c"Specified maxSymbolValue is too small".as_ptr(),
        49 => c"This mode cannot generate an uncompressed block".as_ptr(),
        50 => c"pledged buffer stability condition is not respected".as_ptr(),
        30 => c"Dictionary is corrupted".as_ptr(),
        32 => c"Dictionary mismatch".as_ptr(),
        34 => c"Cannot create Dictionary from provided samples".as_ptr(),
        70 => c"Destination buffer is too small".as_ptr(),
        72 => c"Src size is incorrect".as_ptr(),
        74 => c"Operation on NULL destination buffer".as_ptr(),
        80 => c"Operation made no progress over multiple calls, due to output buffer being full"
            .as_ptr(),
        82 => c"Operation made no progress over multiple calls, due to input being empty".as_ptr(),
        100 => c"Frame index is too large".as_ptr(),
        102 => c"An I/O error occurred when reading/seeking".as_ptr(),
        104 => c"Destination buffer is wrong".as_ptr(),
        105 => c"Source buffer is wrong".as_ptr(),
        106 => c"Block-level external sequence producer returned an error code".as_ptr(),
        107 => c"External sequences are not valid".as_ptr(),
        120 => c"Unspecified error code".as_ptr(),
        _ => c"Unspecified error code".as_ptr(),
    }
}

pub(crate) fn ERR_getErrorName(code: usize) -> *const core::ffi::c_char {
    ERR_getErrorString(ERR_getErrorCode(code))
}
