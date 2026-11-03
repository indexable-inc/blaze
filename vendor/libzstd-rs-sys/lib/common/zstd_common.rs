use crate::lib::common::error_private::{
    ERR_getErrorCode, ERR_getErrorName, ERR_getErrorString, ERR_isError,
};
use crate::lib::zstd::{ZSTD_ErrorCode, ZSTD_VERSION_NUMBER};

/// Get the zstd version number
///
/// # Returns
///
/// - the runtime library version, as `(MAJOR*100*100 + MINOR*100 + RELEASE)`
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_versionNumber))]
pub const extern "C" fn ZSTD_versionNumber() -> core::ffi::c_uint {
    ZSTD_VERSION_NUMBER as core::ffi::c_uint
}

/// Get a zstd version string
///
/// # Returns
///
/// - the runtime library version, like "1.5.8"
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_versionString))]
pub const extern "C" fn ZSTD_versionString() -> *const core::ffi::c_char {
    c"1.5.8".as_ptr()
}

/// Most functions returning a `usize` value can be tested for errors, using [`ZSTD_isError`].
///
/// # Returns
///
/// - 1 if the provided code is an error
/// - 0 otherwise
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_isError))]
pub const extern "C" fn ZSTD_isError(code: usize) -> core::ffi::c_uint {
    ERR_isError(code) as _
}

/// Provides a readable error string from a function result
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_getErrorName))]
pub extern "C" fn ZSTD_getErrorName(code: usize) -> *const core::ffi::c_char {
    ERR_getErrorName(code)
}

/// Convert a result into an error code, which can be compared to the errors enum list
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_getErrorCode))]
pub extern "C" fn ZSTD_getErrorCode(code: usize) -> ZSTD_ErrorCode {
    ERR_getErrorCode(code)
}

/// Provides a readable error string from an error code
///
/// Unlike [`ZSTD_getErrorName`], this method should not be used on `usize` function results
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_getErrorString))]
pub extern "C" fn ZSTD_getErrorString(code: ZSTD_ErrorCode) -> *const core::ffi::c_char {
    ERR_getErrorString(code)
}

/// This is mainly used for Zstd's determinism test suite, which is only run when this function
/// returns 1.
///
/// # Returns
///
/// - 1 if the library is built using standard compilation flags and participates in determinism
///   guarantees with other builds of the same version.
/// - 0 if the library was compiled with non-standard compilation flags that change the output of
///   the compressor.
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_isDeterministicBuild))]
pub extern "C" fn ZSTD_isDeterministicBuild() -> core::ffi::c_int {
    true as core::ffi::c_int
}
