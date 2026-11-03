use crate::lib::common::error_private::Error;
use crate::lib::common::huf::{
    CTable, HUF_flags_bmi2, HUF_flags_optimalDepth, HUF_flags_preferRepeat,
    HUF_flags_suspectUncompressible, HUF_repeat, HUF_OPTIMAL_DEPTH_THRESHOLD, HUF_SYMBOLVALUE_MAX,
};
use crate::lib::common::mem::{MEM_writeLE16, MEM_writeLE24, MEM_writeLE32};
use crate::lib::common::zstd_internal::{LitHufLog, SymbolEncodingType};
use crate::lib::compress::huf_compress::HUF_compress;
use crate::lib::compress::zstd_compress_internal::ZSTD_hufCTables_t;
use crate::lib::compress::zstd_compress_internal::ZSTD_minGain;
use crate::lib::zstd::{ZSTD_lazy, ZSTD_strategy};

const MIN_LITERALS_FOR_4_STREAMS: usize = 6;

pub type huf_compress_f = unsafe fn(
    *mut core::ffi::c_void,
    usize,
    &[u8],
    core::ffi::c_uint,
    core::ffi::c_uint,
    *mut core::ffi::c_void,
    usize,
    &mut CTable,
    &mut HUF_repeat,
    core::ffi::c_int,
) -> Result<usize, Error>;

pub unsafe fn ZSTD_noCompressLiterals(
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: &[u8],
) -> Result<usize, Error> {
    let ostart = dst as *mut u8;
    let flSize = 1 + usize::from(src.len() > 31) + usize::from(src.len() > 4095);

    if src.len().wrapping_add(flSize) > dstCapacity {
        return Err(Error::dstSize_tooSmall);
    }

    match flSize {
        1 => {
            // 2 - 1 - 5
            *ostart = (SymbolEncodingType::Basic as usize).wrapping_add(src.len() << 3) as u8;
        }
        2 => {
            // 2 - 2 - 12
            MEM_writeLE16(
                ostart as *mut core::ffi::c_void,
                (SymbolEncodingType::Basic as usize)
                    .wrapping_add(1 << 2)
                    .wrapping_add(src.len() << 4) as u16,
            );
        }
        3 => {
            // 2 - 2 - 20
            MEM_writeLE32(
                ostart as *mut core::ffi::c_void,
                (SymbolEncodingType::Basic as usize)
                    .wrapping_add(3 << 2)
                    .wrapping_add(src.len() << 4) as u32,
            );
        }
        _ => {} // not necessary : flSize is {1,2,3}
    }

    core::ptr::copy_nonoverlapping(src.as_ptr(), ostart.add(flSize), src.len());

    Ok(src.len().wrapping_add(flSize))
}

fn allBytesIdentical(src: &[u8]) -> bool {
    match src {
        [] => true,
        [first, rest @ ..] => rest.iter().all(|&byte| byte == *first),
    }
}

pub unsafe fn ZSTD_compressRleLiteralsBlock(
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: &[u8],
) -> Result<usize, Error> {
    let ostart = dst as *mut u8;
    let flSize = 1 + usize::from(src.len() > 31) + usize::from(src.len() > 4095);

    assert!(dstCapacity >= 4);
    assert!(allBytesIdentical(src));

    match flSize {
        1 => {
            // 2 - 1 - 5
            *ostart = (SymbolEncodingType::Rle as usize).wrapping_add(src.len() << 3) as u8;
        }
        2 => {
            // 2 - 2 - 12
            MEM_writeLE16(
                ostart as *mut core::ffi::c_void,
                (SymbolEncodingType::Rle as usize)
                    .wrapping_add(1 << 2)
                    .wrapping_add(src.len() << 4) as u16,
            );
        }
        3 => {
            // 2 - 2 - 20
            MEM_writeLE32(
                ostart as *mut core::ffi::c_void,
                (SymbolEncodingType::Rle as usize)
                    .wrapping_add(3 << 2)
                    .wrapping_add(src.len() << 4) as u32,
            );
        }
        _ => {} // not necessary : flSize is {1,2,3}
    }

    *ostart.add(flSize) = src[0];
    Ok(flSize + 1)
}

/// # Returns
/// The minimal amount of literals for literal compression to
/// be attempted.
/// Minimum is made tighter as compression strategy increases.
fn ZSTD_minLiteralsToCompress(strategy: ZSTD_strategy, huf_repeat: HUF_repeat) -> usize {
    // btultra2 : min 8 bytes;
    // then 2x larger for each successive compression strategy
    // max threshold 64 bytes
    let shift = (9 - strategy as core::ffi::c_int).min(3);

    if huf_repeat == HUF_repeat::Valid {
        6
    } else {
        8 << shift
    }
}

pub unsafe fn ZSTD_compressLiterals(
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: &[u8],
    entropyWorkspace: *mut core::ffi::c_void,
    entropyWorkspaceSize: usize,
    prevHuf: &ZSTD_hufCTables_t,
    nextHuf: &mut ZSTD_hufCTables_t,
    strategy: ZSTD_strategy,
    disableLiteralCompression: bool,
    suspectUncompressible: bool,
    bmi2: bool,
) -> Result<usize, Error> {
    let lhSize =
        3 + usize::from(src.len() >= (1 << 10)) + usize::from(src.len() >= (16 * (1 << 10)));
    let ostart = dst as *mut u8;
    let mut singleStream = src.len() < 256;
    let mut hType = SymbolEncodingType::Compressed;

    // Prepare nextEntropy assuming reusing the existing table
    core::ptr::copy_nonoverlapping(prevHuf, nextHuf, 1);

    if disableLiteralCompression {
        return ZSTD_noCompressLiterals(dst, dstCapacity, src);
    }

    // if too small, don't even attempt compression (speed opt)
    if src.len() < ZSTD_minLiteralsToCompress(strategy, prevHuf.repeatMode) {
        return ZSTD_noCompressLiterals(dst, dstCapacity, src);
    }

    if dstCapacity < lhSize.wrapping_add(1) {
        return Err(Error::dstSize_tooSmall);
    }

    let mut repeat = prevHuf.repeatMode;
    let flags = (if bmi2 {
        HUF_flags_bmi2 as core::ffi::c_int
    } else {
        0
    }) | (if (strategy as core::ffi::c_uint) < ZSTD_lazy && src.len() <= 1024 {
        HUF_flags_preferRepeat as core::ffi::c_int
    } else {
        0
    }) | (if strategy >= HUF_OPTIMAL_DEPTH_THRESHOLD as core::ffi::c_uint {
        HUF_flags_optimalDepth as core::ffi::c_int
    } else {
        0
    }) | (if suspectUncompressible {
        HUF_flags_suspectUncompressible as core::ffi::c_int
    } else {
        0
    });
    if repeat == HUF_repeat::Valid && lhSize == 3 {
        singleStream = true;
    }
    let huf_compress: huf_compress_f = if singleStream {
        HUF_compress::<1>
    } else {
        HUF_compress::<4>
    };
    let cLitSize = huf_compress(
        ostart.add(lhSize) as *mut core::ffi::c_void,
        dstCapacity.wrapping_sub(lhSize),
        src,
        HUF_SYMBOLVALUE_MAX,
        LitHufLog,
        entropyWorkspace,
        entropyWorkspaceSize,
        &mut nextHuf.CTable,
        &mut repeat,
        flags,
    );
    if repeat != HUF_repeat::None {
        // reused the existing table
        hType = SymbolEncodingType::Repeat;
    }

    let minGain = ZSTD_minGain(src.len(), strategy);
    let cLitSize = match cLitSize {
        Ok(cLitSize) if cLitSize > 0 && cLitSize < src.len().wrapping_sub(minGain) => cLitSize,
        _ => {
            core::ptr::copy_nonoverlapping(prevHuf, nextHuf, 1);
            return ZSTD_noCompressLiterals(dst, dstCapacity, src);
        }
    };

    // A return value of 1 signals that the alphabet consists of a single symbol.
    // However, in some rare circumstances, it could be the compressed size (a single byte).
    // For that outcome to have a chance to happen, it's necessary that `src.len() < 8`.
    // (it's also necessary to not generate statistics).
    // Therefore, in such a case, actively check that all bytes are identical.
    if cLitSize == 1 && (src.len() >= 8 || allBytesIdentical(src)) {
        core::ptr::copy_nonoverlapping(prevHuf, nextHuf, 1);
        return ZSTD_compressRleLiteralsBlock(dst, dstCapacity, src);
    }

    if hType == SymbolEncodingType::Compressed {
        // using a newly constructed table
        nextHuf.repeatMode = HUF_repeat::Check;
    }

    // Build header
    match lhSize {
        3 => {
            // 2 - 2 - 10 - 10
            if !singleStream {
                assert!(src.len() >= MIN_LITERALS_FOR_4_STREAMS)
            }

            let lhc = (hType as core::ffi::c_uint)
                .wrapping_add(((!singleStream) as core::ffi::c_int as u32) << 2)
                .wrapping_add((src.len() as u32) << 4)
                .wrapping_add((cLitSize as u32) << 14);
            MEM_writeLE24(ostart as *mut core::ffi::c_void, lhc);
        }
        4 => {
            // 2 - 2 - 14 - 14
            let lhc_0 = (hType as core::ffi::c_uint)
                .wrapping_add(2 << 2)
                .wrapping_add((src.len() as u32) << 4)
                .wrapping_add((cLitSize as u32) << 18);
            MEM_writeLE32(ostart as *mut core::ffi::c_void, lhc_0);
        }
        5 => {
            // 2 - 2 - 18 - 18
            let lhc_1 = (hType as core::ffi::c_uint)
                .wrapping_add(3 << 2)
                .wrapping_add((src.len() as u32) << 4)
                .wrapping_add((cLitSize as u32) << 22);
            MEM_writeLE32(ostart as *mut core::ffi::c_void, lhc_1);
            *ostart.add(4) = (cLitSize >> 10) as u8;
        }
        _ => {} // not possible : lhSize is {3,4,5}
    }

    Ok(lhSize.wrapping_add(cLitSize))
}
