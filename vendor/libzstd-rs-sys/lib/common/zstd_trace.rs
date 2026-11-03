use crate::lib::compress::zstd_compress::{ZSTD_CCtx, ZSTD_CCtx_params_s, ZSTD_CCtx_s};
use crate::lib::decompress::{ZSTD_DCtx, ZSTD_DCtx_s};

#[derive(Default)]
#[repr(C)]
pub struct ZSTD_Trace {
    pub version: core::ffi::c_uint,
    pub streaming: core::ffi::c_int,
    pub dictionaryID: core::ffi::c_uint,
    pub dictionaryIsCold: bool,
    _padding: [u8; 3],
    pub dictionarySize: usize,
    pub uncompressedSize: usize,
    pub compressedSize: usize,
    pub params: *const ZSTD_CCtx_params_s,
    pub cctx: *const ZSTD_CCtx_s,
    pub dctx: *const ZSTD_DCtx_s,
}

pub type ZSTD_TraceCtx = core::ffi::c_ulonglong;

#[inline]
pub(crate) fn ZSTD_trace_compress_begin(_cctx: *const ZSTD_CCtx) -> ZSTD_TraceCtx {
    #[cfg(feature = "trace")]
    unsafe {
        return statics::ZSTD_trace_compress_begin(_cctx);
    }

    #[cfg(not(feature = "trace"))]
    0
}

#[inline]
pub(crate) fn ZSTD_trace_compress_end(_ctx: ZSTD_TraceCtx, _trace: &ZSTD_Trace) {
    #[cfg(feature = "trace")]
    unsafe {
        return statics::ZSTD_trace_compress_end(_ctx, _trace);
    }
}

#[inline]
pub(crate) fn ZSTD_trace_decompress_begin(_dctx: *const ZSTD_DCtx) -> ZSTD_TraceCtx {
    #[cfg(feature = "trace")]
    unsafe {
        return statics::ZSTD_trace_decompress_begin(_dctx);
    }

    #[cfg(not(feature = "trace"))]
    0
}

#[inline]
pub(crate) fn ZSTD_trace_decompress_end(_ctx: ZSTD_TraceCtx, _trace: &ZSTD_Trace) {
    #[cfg(feature = "trace")]
    unsafe {
        return statics::ZSTD_trace_decompress_end(_ctx, _trace);
    }
}

#[cfg(feature = "trace")]
mod statics {
    use super::{ZSTD_CCtx, ZSTD_Trace, ZSTD_TraceCtx};
    use crate::lib::decompress::ZSTD_DCtx;

    #[expect(
        improper_ctypes,
        reason = "ZSTD_CCtx contains types that are not FFI-safe. This is fine as it is opaque to C"
    )]
    extern "C" {
        pub(super) fn ZSTD_trace_compress_begin(cctx: *const ZSTD_CCtx) -> ZSTD_TraceCtx;
        pub(super) fn ZSTD_trace_compress_end(ctx: ZSTD_TraceCtx, trace: *const ZSTD_Trace);
        pub(super) fn ZSTD_trace_decompress_begin(dctx: *const ZSTD_DCtx) -> ZSTD_TraceCtx;
        pub(super) fn ZSTD_trace_decompress_end(ctx: ZSTD_TraceCtx, trace: *const ZSTD_Trace);
    }
}
