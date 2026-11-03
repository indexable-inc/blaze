use core::arch::asm;
use core::ffi::c_void;
use core::ops::Range;
use core::ptr::{self, NonNull};

use libc::ptrdiff_t;

use crate::lib::common::bitstream::{
    BIT_DStream_t, STREAM_ACCUMULATOR_MIN, STREAM_ACCUMULATOR_MIN_32, STREAM_ACCUMULATOR_MIN_64,
};
use crate::lib::common::compiler::{prefetch_area, prefetch_read_data, prefetch_val, Locality};
use crate::lib::common::entropy_common::FSE_readNCount;
use crate::lib::common::error_private::Error;
use crate::lib::common::fse::FSE_TABLESTEP;
use crate::lib::common::huf::{HUF_flags_bmi2, HUF_flags_disableAsm};
use crate::lib::common::mem::{MEM_32bits, MEM_readLE24};
use crate::lib::common::reader::Reader;
use crate::lib::common::zstd_internal::{
    BlockType, LLFSELog, LL_bits, MLFSELog, ML_bits, MaxFSELog, MaxLL, MaxLLBits, MaxML, MaxMLBits,
    MaxOff, MaxSeq, OffFSELog, Overlap, SymbolEncodingType, ZSTD_copy16, ZSTD_wildcopy,
    LL_DEFAULTNORMLOG, LONGNBSEQ, ML_DEFAULTNORMLOG, OF_DEFAULTNORMLOG, WILDCOPY_OVERLENGTH,
    WILDCOPY_VECLEN, ZSTD_BLOCKHEADERSIZE, ZSTD_REP_NUM,
};
use crate::lib::decompress::huf_decompress::{
    HUF_decompress1X1_DCtx_wksp, HUF_decompress1X_usingDTable, HUF_decompress4X_usingDTable,
};
use crate::lib::decompress::huf_decompress::{HUF_decompress4X_hufOnly_wksp, Writer};
use crate::lib::decompress::{blockProperties_t, SymbolTable};
use crate::lib::decompress::{
    LL_base, LitLocation, ML_base, OF_base, OF_bits, Workspace, ZSTD_DCtx, ZSTD_seqSymbol,
    ZSTD_seqSymbol_header,
};
use crate::lib::polyfill::{likely, unlikely};
use crate::lib::zstd::{ZSTD_BLOCKSIZE_MAX, ZSTD_WINDOWLOG_MAX, ZSTD_WINDOWLOG_MAX_32};

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum StreamingOperation {
    NotStreaming = 0,
    IsStreaming = 1,
}

impl TryFrom<u32> for StreamingOperation {
    type Error = ();

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::NotStreaming),
            1 => Ok(Self::IsStreaming),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum Offset {
    Regular = 0,
    Long = 1,
}

#[repr(C)]
pub struct seqState_t<'a> {
    DStream: BIT_DStream_t<'a>,
    stateLL: ZSTD_fseState<'a, { 1 << LLFSELog }>,
    stateOffb: ZSTD_fseState<'a, { 1 << OffFSELog }>,
    stateML: ZSTD_fseState<'a, { 1 << MLFSELog }>,
    prevOffset: [usize; 3],
}

impl ZSTD_DCtx {
    fn new_seq_state<'a>(&self, mut bit_stream: BIT_DStream_t<'a>) -> seqState_t<'a> {
        let stateLL = match self.LLTptr {
            None => ZSTD_fseState::new(&mut bit_stream, &LL_defaultDTable),
            Some(table) => ZSTD_fseState::new(&mut bit_stream, unsafe { &*table.as_ptr() }),
        };

        let stateOffb = match self.OFTptr {
            None => ZSTD_fseState::new(&mut bit_stream, &OF_defaultDTable),
            Some(table) => ZSTD_fseState::new(&mut bit_stream, unsafe { &*table.as_ptr() }),
        };

        let stateML = match self.MLTptr {
            None => ZSTD_fseState::new(&mut bit_stream, &ML_defaultDTable),
            Some(table) => ZSTD_fseState::new(&mut bit_stream, unsafe { &*table.as_ptr() }),
        };

        seqState_t {
            stateLL,
            stateOffb,
            stateML,
            DStream: bit_stream,
            prevOffset: self.entropy.rep.map(|v| v as usize),
        }
    }
}

#[repr(C)]
pub struct ZSTD_fseState<'a, const N: usize> {
    pub state: usize,
    pub table: &'a [ZSTD_seqSymbol; N],
}

impl<'a, const N: usize> ZSTD_fseState<'a, N> {
    pub(crate) fn new(bit_dstream: &mut BIT_DStream_t, dt: &'a SymbolTable<N>) -> Self {
        let table = &dt.symbols;

        let state = bit_dstream.read_bits(dt.header.tableLog);
        bit_dstream.reload();

        Self { state, table }
    }
}

#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct seq_t {
    pub litLength: usize,
    pub matchLength: usize,
    pub offset: usize,
}

#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct ZSTD_OffsetInfo {
    pub longOffsetShare: core::ffi::c_uint,
    pub maxNbAdditionalBits: core::ffi::c_uint,
}

impl ZSTD_DCtx {
    fn block_size_max(&self) -> usize {
        if self.isFrameDecompression {
            self.fParams.blockSizeMax as usize
        } else {
            ZSTD_BLOCKSIZE_MAX as usize
        }
    }
}

pub(crate) fn ZSTD_getcBlockSize(
    src: &[u8],
    bpPtr: &mut blockProperties_t,
) -> Result<usize, Error> {
    if src.len() < ZSTD_BLOCKHEADERSIZE {
        return Err(Error::srcSize_wrong);
    }
    let cBlockHeader = unsafe { MEM_readLE24(src.as_ptr().cast()) };
    let cSize = cBlockHeader >> 3;

    bpPtr.lastBlock = (cBlockHeader & 1) != 0;
    bpPtr.blockType = BlockType::from(cBlockHeader >> 1 & 0b11);
    bpPtr.origSize = cSize;

    match bpPtr.blockType {
        BlockType::Raw | BlockType::Compressed => Ok(cSize as usize),
        BlockType::Rle => Ok(1),
        BlockType::Reserved => Err(Error::corruption_detected),
    }
}

pub(crate) fn getc_block_size(src: &[u8]) -> Result<(blockProperties_t, usize), Error> {
    let [a, b, c, ..] = *src else {
        return Err(Error::srcSize_wrong);
    };

    let cBlockHeader = u32::from_le_bytes([a, b, c, 0]);
    let cSize = cBlockHeader >> 3;

    let bp = blockProperties_t {
        lastBlock: (cBlockHeader & 1) != 0,
        blockType: BlockType::from(cBlockHeader >> 1 & 0b11),
        origSize: cSize,
    };

    match bp.blockType {
        BlockType::Raw | BlockType::Compressed => Ok((bp, cSize as usize)),
        BlockType::Rle => Ok((bp, 1)),
        BlockType::Reserved => Err(Error::corruption_detected),
    }
}

fn ZSTD_allocateLiteralsBuffer(
    dctx: &mut ZSTD_DCtx,
    mut dst: Writer<'_>,
    litSize: usize,
    streaming: StreamingOperation,
    expectedWriteSize: usize,
    split_immediately: bool,
) {
    let dstCapacity = dst.capacity();
    let dst = dst.as_mut_ptr();

    let blockSizeMax = dctx.block_size_max();
    if streaming == StreamingOperation::NotStreaming
        && dstCapacity
            > blockSizeMax.wrapping_add(WILDCOPY_OVERLENGTH + litSize + WILDCOPY_OVERLENGTH)
    {
        dctx.litBuffer = dst.wrapping_add(blockSizeMax + WILDCOPY_OVERLENGTH);
        dctx.litBufferEnd = dctx.litBuffer.wrapping_add(litSize);
        dctx.litBufferLocation = LitLocation::ZSTD_in_dst;
    } else if litSize <= ZSTD_LITBUFFEREXTRASIZE {
        dctx.litBuffer = dctx.litExtraBuffer.as_mut_ptr();
        dctx.litBufferEnd = dctx.litBuffer.wrapping_add(litSize);
        dctx.litBufferLocation = LitLocation::ZSTD_not_in_dst;
    } else {
        if split_immediately {
            dctx.litBuffer = dst
                .wrapping_add(expectedWriteSize)
                .wrapping_sub(litSize)
                .wrapping_add(ZSTD_LITBUFFEREXTRASIZE)
                .wrapping_sub(WILDCOPY_OVERLENGTH);
            dctx.litBufferEnd = dctx
                .litBuffer
                .wrapping_add(litSize)
                .wrapping_sub(ZSTD_LITBUFFEREXTRASIZE);
        } else {
            dctx.litBuffer = dst.wrapping_add(expectedWriteSize).wrapping_sub(litSize);
            dctx.litBufferEnd = dst.wrapping_add(expectedWriteSize);
        }
        dctx.litBufferLocation = LitLocation::ZSTD_split;
    }
}

const ZSTD_LBMIN: usize = 64;
const ZSTD_LBMAX: usize = 128 << 10;

const ZSTD_DECODER_INTERNAL_BUFFER: usize = 1 << 16;

pub(crate) const ZSTD_LITBUFFEREXTRASIZE: usize = {
    // just a const clamp
    if ZSTD_DECODER_INTERNAL_BUFFER < ZSTD_LBMIN {
        ZSTD_LBMIN
    } else if ZSTD_DECODER_INTERNAL_BUFFER > ZSTD_LBMAX {
        ZSTD_LBMAX
    } else {
        ZSTD_DECODER_INTERNAL_BUFFER
    }
};

fn ZSTD_decodeLiteralsBlock(
    dctx: &mut ZSTD_DCtx,
    src: &[u8],
    dst: Writer<'_>,
    streaming: StreamingOperation,
) -> Result<usize, Error> {
    // for a non-null block
    const MIN_CBLOCK_SIZE: usize = 1 /*litCSize*/ + 1/* RLE or RAW */;
    if src.len() < MIN_CBLOCK_SIZE {
        return Err(Error::corruption_detected);
    }

    let blockSizeMax = dctx.block_size_max();

    let litEncType = SymbolEncodingType::try_from(src[0] & 0b11).unwrap();
    match litEncType {
        SymbolEncodingType::Repeat if !dctx.litEntropy => {
            return Err(Error::dictionary_corrupted);
        }
        SymbolEncodingType::Repeat | SymbolEncodingType::Compressed => {}
        SymbolEncodingType::Basic => {
            let (lhSize, litSize) = match src[0] >> 2 & 0b11 {
                1 => (2usize, (u16::from_le_bytes([src[0], src[1]]) >> 4) as usize),
                3 => {
                    let [a, b, c, ..] = *src else {
                        return Err(Error::corruption_detected);
                    };

                    (3, (u32::from_le_bytes([a, b, c, 0]) >> 4) as usize)
                }
                _ => (1, (src[0] >> 3) as usize),
            };

            if litSize > 0 && dst.is_null() {
                return Err(Error::dstSize_tooSmall);
            }
            if litSize > blockSizeMax {
                return Err(Error::corruption_detected);
            }

            let expectedWriteSize = Ord::min(dst.capacity(), blockSizeMax);
            if expectedWriteSize < litSize {
                return Err(Error::dstSize_tooSmall);
            }

            ZSTD_allocateLiteralsBuffer(dctx, dst, litSize, streaming, expectedWriteSize, true);

            if lhSize + litSize + WILDCOPY_OVERLENGTH > src.len() {
                if litSize.wrapping_add(lhSize) > src.len() {
                    return Err(Error::corruption_detected);
                }
                if dctx.litBufferLocation == LitLocation::ZSTD_split {
                    unsafe {
                        let len = litSize - ZSTD_LITBUFFEREXTRASIZE;
                        let src = &src[lhSize..][..len];

                        core::ptr::copy_nonoverlapping(src.as_ptr(), dctx.litBuffer, len)
                    };

                    dctx.litExtraBuffer[..ZSTD_LITBUFFEREXTRASIZE].copy_from_slice(
                        &src[lhSize + litSize - ZSTD_LITBUFFEREXTRASIZE..]
                            [..ZSTD_LITBUFFEREXTRASIZE],
                    );
                } else {
                    unsafe {
                        let src = &src[lhSize..][..litSize];

                        core::ptr::copy_nonoverlapping(src.as_ptr(), dctx.litBuffer, litSize)
                    };
                }
                dctx.litPtr = dctx.litBuffer;
                dctx.litSize = litSize;
                return Ok(lhSize.wrapping_add(litSize));
            }

            dctx.litPtr = src[lhSize..].as_ptr();
            dctx.litSize = litSize;
            dctx.litBufferEnd = unsafe { dctx.litPtr.add(litSize) };
            dctx.litBufferLocation = LitLocation::ZSTD_not_in_dst;

            return Ok(lhSize.wrapping_add(litSize));
        }
        SymbolEncodingType::Rle => {
            let (lhSize, litSize) = match src[0] >> 2 & 0b11 {
                1 => {
                    let [a, b, _, ..] = *src else {
                        return Err(Error::corruption_detected);
                    };

                    (2usize, (u16::from_le_bytes([a, b]) >> 4) as usize)
                }
                3 => {
                    let [a, b, c, _, ..] = *src else {
                        return Err(Error::corruption_detected);
                    };

                    (3, (u32::from_le_bytes([a, b, c, 0]) >> 4) as usize)
                }
                _ => (1, (src[0] >> 3) as usize),
            };

            if litSize > 0 && dst.is_null() {
                return Err(Error::dstSize_tooSmall);
            }
            if litSize > blockSizeMax {
                return Err(Error::corruption_detected);
            }

            let expectedWriteSize = Ord::min(dst.capacity(), blockSizeMax);
            if expectedWriteSize < litSize {
                return Err(Error::dstSize_tooSmall);
            }

            ZSTD_allocateLiteralsBuffer(dctx, dst, litSize, streaming, expectedWriteSize, true);

            if dctx.litBufferLocation == LitLocation::ZSTD_split {
                unsafe {
                    ptr::write_bytes(
                        dctx.litBuffer,
                        src[lhSize],
                        litSize - ZSTD_LITBUFFEREXTRASIZE,
                    )
                };

                dctx.litExtraBuffer[..ZSTD_LITBUFFEREXTRASIZE].fill(src[lhSize]);
            } else {
                unsafe {
                    ptr::write_bytes(dctx.litBuffer, src[lhSize], litSize);
                }
            }
            dctx.litPtr = dctx.litBuffer;
            dctx.litSize = litSize;
            return Ok(lhSize.wrapping_add(1));
        }
    }

    let [a, b, c, d, size_correction, ..] = *src else {
        return Err(Error::corruption_detected);
    };
    let lhc = u32::from_le_bytes([a, b, c, d]) as usize;

    let flags = {
        let bmi_flag = if dctx.bmi2 {
            HUF_flags_bmi2 as core::ffi::c_int
        } else {
            0
        };

        let disable_asm_flag = if dctx.disableHufAsm {
            HUF_flags_disableAsm as core::ffi::c_int
        } else {
            0
        };

        bmi_flag | disable_asm_flag
    };

    let lhlCode = (src[0] >> 2 & 0b11) as u32;
    let singleStream = lhlCode == 0;

    let (lhSize, litSize, litCSize) = match lhlCode {
        2 => (4, lhc >> 4 & 0x3fff, lhc >> 18),
        3 => (
            5,
            lhc >> 4 & 0x3ffff,
            (lhc >> 22) + ((size_correction as usize) << 10),
        ),
        _ => (3, lhc >> 4 & 0x3ff, lhc >> 14 & 0x3ff),
    };

    if litSize > 0 && dst.is_null() {
        return Err(Error::dstSize_tooSmall);
    }
    if litSize > blockSizeMax {
        return Err(Error::corruption_detected);
    }
    if !singleStream && litSize < 6 {
        return Err(Error::literals_headerWrong);
    }
    if litCSize.wrapping_add(lhSize) > src.len() {
        return Err(Error::corruption_detected);
    }

    let expectedWriteSize = Ord::min(dst.capacity(), blockSizeMax);
    if expectedWriteSize < litSize {
        return Err(Error::dstSize_tooSmall);
    }

    ZSTD_allocateLiteralsBuffer(dctx, dst, litSize, streaming, expectedWriteSize, false);

    /* prefetch huffman table if cold */
    if dctx.ddictIsCold && litSize > 768 {
        // NOTE: the litSize comparison is a heuristic.
        let ptr = match dctx.HUFptr {
            None => &raw const dctx.entropy.hufTable,
            Some(ptr) => ptr.as_ptr().cast_const(),
        };
        prefetch_val(ptr);
    }

    let writer = unsafe { Writer::from_raw_parts(dctx.litBuffer, litSize as _) };
    let huf_src = &src[lhSize..][..litCSize];

    let hufSuccess = if let SymbolEncodingType::Repeat = litEncType {
        let dtable = match dctx.HUFptr {
            None => &dctx.entropy.hufTable,
            Some(ptr) => unsafe { ptr.as_ref() },
        };

        if singleStream {
            HUF_decompress1X_usingDTable(writer, huf_src, dtable, flags)
        } else {
            HUF_decompress4X_usingDTable(writer, huf_src, dtable, flags)
        }
    } else if singleStream {
        HUF_decompress1X1_DCtx_wksp(
            &mut dctx.entropy.hufTable,
            writer,
            huf_src,
            &mut dctx.workspace,
            flags,
        )
    } else {
        HUF_decompress4X_hufOnly_wksp(
            &mut dctx.entropy.hufTable,
            writer,
            huf_src,
            &mut dctx.workspace,
            flags,
        )
    };

    if dctx.litBufferLocation == LitLocation::ZSTD_split {
        debug_assert!(litSize > ZSTD_LITBUFFEREXTRASIZE);

        unsafe {
            core::ptr::copy_nonoverlapping(
                dctx.litBufferEnd.sub(ZSTD_LITBUFFEREXTRASIZE),
                dctx.litExtraBuffer.as_mut_ptr(),
                ZSTD_LITBUFFEREXTRASIZE,
            );
            core::ptr::copy(
                dctx.litBuffer,
                dctx.litBuffer
                    .add(ZSTD_LITBUFFEREXTRASIZE - WILDCOPY_OVERLENGTH),
                litSize.wrapping_sub(ZSTD_LITBUFFEREXTRASIZE),
            );
            dctx.litBuffer = (dctx.litBuffer).add(ZSTD_LITBUFFEREXTRASIZE - WILDCOPY_OVERLENGTH);
            dctx.litBufferEnd = (dctx.litBufferEnd).sub(WILDCOPY_OVERLENGTH);
        }
    }

    if hufSuccess.is_err() {
        return Err(Error::corruption_detected);
    }

    dctx.litPtr = dctx.litBuffer;
    dctx.litSize = litSize;
    dctx.litEntropy = true;

    if let SymbolEncodingType::Compressed = litEncType {
        dctx.HUFptr = None;
    }

    Ok(litCSize.wrapping_add(lhSize))
}

const fn sequence_symbol(
    nextState: u16,
    nbAdditionalBits: u8,
    nbBits: u8,
    baseValue: u32,
) -> ZSTD_seqSymbol {
    ZSTD_seqSymbol {
        nextState,
        nbAdditionalBits,
        nbBits,
        baseValue,
    }
}

const fn sequence_header(fastMode: u32, tableLog: u32) -> ZSTD_seqSymbol_header {
    ZSTD_seqSymbol_header { fastMode, tableLog }
}

/// Pads a default table to the size of a decoded table, so that both have the same type.
const fn pad_table<const M: usize, const N: usize>(small: SymbolTable<M>) -> SymbolTable<N> {
    let mut symbols = [sequence_symbol(0, 0, 0, 0); N];
    let mut i = 0;
    while i < M {
        symbols[i] = small.symbols[i];
        i += 1;
    }
    SymbolTable {
        header: small.header,
        symbols,
    }
}

/// Default FSE distribution table for Literal Lengths.
#[rustfmt::skip]
static LL_defaultDTable: SymbolTable<{ 1 << LLFSELog }> = pad_table(SymbolTable::<{ 1 << LL_DEFAULTNORMLOG }> {
    /* header : fastMode, tableLog */
    header: sequence_header(0x00010101, LL_DEFAULTNORMLOG),
    /* nextState, nbAddBits, nbBits, baseVal */
    symbols: [
        sequence_symbol( 0,  0,  4,    0), sequence_symbol(16,  0,  4,    0),
        sequence_symbol(32,  0,  5,    1), sequence_symbol( 0,  0,  5,    3),
        sequence_symbol( 0,  0,  5,    4), sequence_symbol( 0,  0,  5,    6),
        sequence_symbol( 0,  0,  5,    7), sequence_symbol( 0,  0,  5,    9),
        sequence_symbol( 0,  0,  5,   10), sequence_symbol( 0,  0,  5,   12),
        sequence_symbol( 0,  0,  6,   14), sequence_symbol( 0,  1,  5,   16),
        sequence_symbol( 0,  1,  5,   20), sequence_symbol( 0,  1,  5,   22),
        sequence_symbol( 0,  2,  5,   28), sequence_symbol( 0,  3,  5,   32),
        sequence_symbol( 0,  4,  5,   48), sequence_symbol(32,  6,  5,   64),
        sequence_symbol( 0,  7,  5,  128), sequence_symbol( 0,  8,  6,  256),
        sequence_symbol( 0, 10,  6, 1024), sequence_symbol( 0, 12,  6, 4096),
        sequence_symbol(32,  0,  4,    0), sequence_symbol( 0,  0,  4,    1),
        sequence_symbol( 0,  0,  5,    2), sequence_symbol(32,  0,  5,    4),
        sequence_symbol( 0,  0,  5,    5), sequence_symbol(32,  0,  5,    7),
        sequence_symbol( 0,  0,  5,    8), sequence_symbol(32,  0,  5,   10),
        sequence_symbol( 0,  0,  5,   11), sequence_symbol( 0,  0,  6,   13),
        sequence_symbol(32,  1,  5,   16), sequence_symbol( 0,  1,  5,   18),
        sequence_symbol(32,  1,  5,   22), sequence_symbol( 0,  2,  5,   24),
        sequence_symbol(32,  3,  5,   32), sequence_symbol( 0,  3,  5,   40),
        sequence_symbol( 0,  6,  4,   64), sequence_symbol(16,  6,  4,   64),
        sequence_symbol(32,  7,  5,  128), sequence_symbol( 0,  9,  6,  512),
        sequence_symbol( 0, 11,  6, 2048), sequence_symbol(48,  0,  4,    0),
        sequence_symbol(16,  0,  4,    1), sequence_symbol(32,  0,  5,    2),
        sequence_symbol(32,  0,  5,    3), sequence_symbol(32,  0,  5,    5),
        sequence_symbol(32,  0,  5,    6), sequence_symbol(32,  0,  5,    8),
        sequence_symbol(32,  0,  5,    9), sequence_symbol(32,  0,  5,   11),
        sequence_symbol(32,  0,  5,   12), sequence_symbol( 0,  0,  6,   15),
        sequence_symbol(32,  1,  5,   18), sequence_symbol(32,  1,  5,   20),
        sequence_symbol(32,  2,  5,   24), sequence_symbol(32,  2,  5,   28),
        sequence_symbol(32,  3,  5,   40), sequence_symbol(32,  4,  5,   48),
        sequence_symbol( 0, 16,  6,65536), sequence_symbol( 0, 15,  6,32768),
        sequence_symbol( 0, 14,  6,16384), sequence_symbol( 0, 13,  6, 8192),
    ]
});

/// Default FSE distribution table for Offset Codes.
#[rustfmt::skip]
static OF_defaultDTable: SymbolTable<{ 1 << OffFSELog }> = pad_table(SymbolTable::<{ 1 << OF_DEFAULTNORMLOG }> {
    /* header : fastMode, tableLog */
    header: sequence_header(0x00010101, OF_DEFAULTNORMLOG),
    /* nextState, nbAddBits, nbBits, baseVal */
    symbols: [
        sequence_symbol( 0,  0,  5,    0),     sequence_symbol( 0,  6,  4,   61),
        sequence_symbol( 0,  9,  5,  509),     sequence_symbol( 0, 15,  5,32765),
        sequence_symbol( 0, 21,  5,2097149),   sequence_symbol( 0,  3,  5,    5),
        sequence_symbol( 0,  7,  4,  125),     sequence_symbol( 0, 12,  5, 4093),
        sequence_symbol( 0, 18,  5,262141),    sequence_symbol( 0, 23,  5,8388605),
        sequence_symbol( 0,  5,  5,   29),     sequence_symbol( 0,  8,  4,  253),
        sequence_symbol( 0, 14,  5,16381),     sequence_symbol( 0, 20,  5,1048573),
        sequence_symbol( 0,  2,  5,    1),     sequence_symbol(16,  7,  4,  125),
        sequence_symbol( 0, 11,  5, 2045),     sequence_symbol( 0, 17,  5,131069),
        sequence_symbol( 0, 22,  5,4194301),   sequence_symbol( 0,  4,  5,   13),
        sequence_symbol(16,  8,  4,  253),     sequence_symbol( 0, 13,  5, 8189),
        sequence_symbol( 0, 19,  5,524285),    sequence_symbol( 0,  1,  5,    1),
        sequence_symbol(16,  6,  4,   61),     sequence_symbol( 0, 10,  5, 1021),
        sequence_symbol( 0, 16,  5,65533),     sequence_symbol( 0, 28,  5,268435453),
        sequence_symbol( 0, 27,  5,134217725), sequence_symbol( 0, 26,  5,67108861),
        sequence_symbol( 0, 25,  5,33554429),  sequence_symbol( 0, 24,  5,16777213),
    ]
});

/// Default FSE distribution table for Match Lengths.
#[rustfmt::skip]
static ML_defaultDTable: SymbolTable<{ 1 << MLFSELog }> = pad_table(SymbolTable::<{ 1 << ML_DEFAULTNORMLOG }> {
    /* header : fastMode, tableLog */
    header: sequence_header(0x00010101, ML_DEFAULTNORMLOG),
    /* nextState, nbAddBits, nbBits, baseVal */
    symbols: [
        sequence_symbol( 0,  0,  6,    3),  sequence_symbol( 0,  0,  4,    4),
        sequence_symbol(32,  0,  5,    5),  sequence_symbol( 0,  0,  5,    6),
        sequence_symbol( 0,  0,  5,    8),  sequence_symbol( 0,  0,  5,    9),
        sequence_symbol( 0,  0,  5,   11),  sequence_symbol( 0,  0,  6,   13),
        sequence_symbol( 0,  0,  6,   16),  sequence_symbol( 0,  0,  6,   19),
        sequence_symbol( 0,  0,  6,   22),  sequence_symbol( 0,  0,  6,   25),
        sequence_symbol( 0,  0,  6,   28),  sequence_symbol( 0,  0,  6,   31),
        sequence_symbol( 0,  0,  6,   34),  sequence_symbol( 0,  1,  6,   37),
        sequence_symbol( 0,  1,  6,   41),  sequence_symbol( 0,  2,  6,   47),
        sequence_symbol( 0,  3,  6,   59),  sequence_symbol( 0,  4,  6,   83),
        sequence_symbol( 0,  7,  6,  131),  sequence_symbol( 0,  9,  6,  515),
        sequence_symbol(16,  0,  4,    4),  sequence_symbol( 0,  0,  4,    5),
        sequence_symbol(32,  0,  5,    6),  sequence_symbol( 0,  0,  5,    7),
        sequence_symbol(32,  0,  5,    9),  sequence_symbol( 0,  0,  5,   10),
        sequence_symbol( 0,  0,  6,   12),  sequence_symbol( 0,  0,  6,   15),
        sequence_symbol( 0,  0,  6,   18),  sequence_symbol( 0,  0,  6,   21),
        sequence_symbol( 0,  0,  6,   24),  sequence_symbol( 0,  0,  6,   27),
        sequence_symbol( 0,  0,  6,   30),  sequence_symbol( 0,  0,  6,   33),
        sequence_symbol( 0,  1,  6,   35),  sequence_symbol( 0,  1,  6,   39),
        sequence_symbol( 0,  2,  6,   43),  sequence_symbol( 0,  3,  6,   51),
        sequence_symbol( 0,  4,  6,   67),  sequence_symbol( 0,  5,  6,   99),
        sequence_symbol( 0,  8,  6,  259),  sequence_symbol(32,  0,  4,    4),
        sequence_symbol(48,  0,  4,    4),  sequence_symbol(16,  0,  4,    5),
        sequence_symbol(32,  0,  5,    7),  sequence_symbol(32,  0,  5,    8),
        sequence_symbol(32,  0,  5,   10),  sequence_symbol(32,  0,  5,   11),
        sequence_symbol( 0,  0,  6,   14),  sequence_symbol( 0,  0,  6,   17),
        sequence_symbol( 0,  0,  6,   20),  sequence_symbol( 0,  0,  6,   23),
        sequence_symbol( 0,  0,  6,   26),  sequence_symbol( 0,  0,  6,   29),
        sequence_symbol( 0,  0,  6,   32),  sequence_symbol( 0, 16,  6,65539),
        sequence_symbol( 0, 15,  6,32771),  sequence_symbol( 0, 14,  6,16387),
        sequence_symbol( 0, 13,  6, 8195),  sequence_symbol( 0, 12,  6, 4099),
        sequence_symbol( 0, 11,  6, 2051),  sequence_symbol( 0, 10,  6, 1027),
    ]
});

fn ZSTD_buildSeqTable_rle<const N: usize>(dt: &mut SymbolTable<N>, baseValue: u32, nbAddBits: u8) {
    dt.header = ZSTD_seqSymbol_header {
        fastMode: 0,
        tableLog: 0,
    };

    dt.symbols[0] = ZSTD_seqSymbol {
        nbBits: 0,
        nextState: 0,
        nbAdditionalBits: nbAddBits,
        baseValue,
    };
}

#[inline(always)]
fn ZSTD_buildFSETable_body<const N: usize>(
    dt: &mut SymbolTable<N>,
    normalizedCounter: &[i16],
    baseValue: &'static [u32],
    nbAdditionalBits: &'static [u8],
    tableLog: core::ffi::c_uint,
    wksp: &mut FseWorkspace,
) {
    let tableDecode = &mut dt.symbols;
    let tableSize = 1usize << tableLog;
    let mut highThreshold = tableSize.wrapping_sub(1);
    let mut DTableH = ZSTD_seqSymbol_header {
        fastMode: 1,
        tableLog,
    };

    let largeLimit = (1 << tableLog.wrapping_sub(1)) as i16;

    for (s, &v) in normalizedCounter.iter().enumerate() {
        if v == -1 {
            tableDecode[highThreshold].baseValue = s as u32;
            highThreshold = highThreshold.wrapping_sub(1);
            wksp.symbols[s] = 1;
        } else {
            if v >= largeLimit {
                DTableH.fastMode = 0;
            }
            wksp.symbols[s] = v as u16;
        }
    }

    dt.header = DTableH;

    if highThreshold == tableSize - 1 {
        let tableMask = tableSize - 1;
        let step = FSE_TABLESTEP(tableSize);
        let add = 0x101010101010101u64;
        let mut pos = 0usize;
        let mut sv = 0u64;
        for &v in normalizedCounter {
            let n = v as usize;
            wksp.spread[pos..][..8].copy_from_slice(&sv.to_le_bytes());
            for i in (8..n).step_by(8) {
                wksp.spread[pos..][i..][..8].copy_from_slice(&sv.to_le_bytes());
            }
            pos = pos.wrapping_add(n);
            sv = sv.wrapping_add(add);
        }

        debug_assert!(tableSize.is_multiple_of(2));
        let mut position = 0usize;
        for pair in wksp.spread[..tableSize].as_chunks::<2>().0 {
            for (u, &sym) in pair.iter().enumerate() {
                let uPosition = position.wrapping_add(u * step) & tableMask;
                tableDecode[uPosition].baseValue = sym as u32;
            }
            position = position.wrapping_add(2 * step) & tableMask;
        }
    } else {
        let tableMask = tableSize - 1;
        let step = FSE_TABLESTEP(tableSize);
        let mut position = 0usize;
        for (s, &v) in normalizedCounter.iter().enumerate() {
            for _ in 0..i32::from(v) {
                tableDecode[position].baseValue = s as u32;
                position = position.wrapping_add(step) & tableMask;
                while unlikely(position > highThreshold) {
                    position = position.wrapping_add(step) & tableMask;
                }
            }
        }
    }

    for seq_symbol in tableDecode[..tableSize].iter_mut() {
        let symbol = seq_symbol.baseValue as usize;
        let nextState = wksp.symbols[symbol] as u32;
        wksp.symbols[symbol] += 1;

        let nbBits = tableLog.wrapping_sub(nextState.ilog2()) as u8;

        *seq_symbol = ZSTD_seqSymbol {
            nbBits,
            nextState: (nextState << nbBits).wrapping_sub(tableSize as u32) as u16,
            nbAdditionalBits: nbAdditionalBits[symbol],
            baseValue: baseValue[symbol],
        };
    }
}

fn ZSTD_buildFSETable_body_default<const N: usize>(
    dt: &mut SymbolTable<N>,
    normalizedCounter: &[i16],
    baseValue: &'static [u32],
    nbAdditionalBits: &'static [u8],
    tableLog: core::ffi::c_uint,
    wksp: &mut FseWorkspace,
) {
    ZSTD_buildFSETable_body(
        dt,
        normalizedCounter,
        baseValue,
        nbAdditionalBits,
        tableLog,
        wksp,
    );
}

#[cfg_attr(target_arch = "x86_64", target_feature(enable = "bmi2"))]
fn ZSTD_buildFSETable_body_bmi2<const N: usize>(
    dt: &mut SymbolTable<N>,
    normalizedCounter: &[i16],
    baseValue: &'static [u32],
    nbAdditionalBits: &'static [u8],
    tableLog: core::ffi::c_uint,
    wksp: &mut FseWorkspace,
) {
    ZSTD_buildFSETable_body(
        dt,
        normalizedCounter,
        baseValue,
        nbAdditionalBits,
        tableLog,
        wksp,
    );
}

#[derive(Copy, Clone)]
#[repr(C, align(4))]
pub struct FseWorkspace {
    symbols: [u16; MaxSeq + 1],
    spread: [u8; (1 << MaxFSELog) + size_of::<u64>()],
}

pub fn ZSTD_buildFSETable<const N: usize>(
    dt: &mut SymbolTable<N>,
    normalizedCounter: &[i16],
    baseValue: &'static [u32],
    nbAdditionalBits: &'static [u8],
    tableLog: core::ffi::c_uint,
    wksp: &mut FseWorkspace,
    bmi2: bool,
) {
    if bmi2 {
        // SAFETY: the bmi2 feature is enabled.
        unsafe {
            ZSTD_buildFSETable_body_bmi2(
                dt,
                normalizedCounter,
                baseValue,
                nbAdditionalBits,
                tableLog,
                wksp,
            )
        };
    } else {
        ZSTD_buildFSETable_body_default(
            dt,
            normalizedCounter,
            baseValue,
            nbAdditionalBits,
            tableLog,
            wksp,
        );
    }
}

fn ZSTD_buildSeqTableNew<const N: usize>(
    DTableSpace: &mut SymbolTable<N>,
    DTablePtr: &mut Option<NonNull<SymbolTable<N>>>,
    type_0: SymbolEncodingType,
    mut max: u8,
    maxLog: u32,
    src: &[u8],
    baseValue: &'static [u32],
    nbAdditionalBits: &'static [u8],
    flagRepeatTable: bool,
    ddictIsCold: bool,
    nbSeq: core::ffi::c_int,
    wksp: &mut Workspace,
    bmi2: bool,
) -> Result<usize, Error> {
    match type_0 {
        SymbolEncodingType::Rle => {
            let [symbol, ..] = *src else {
                return Err(Error::srcSize_wrong);
            };

            if symbol > max {
                return Err(Error::corruption_detected);
            }

            let baseline = baseValue[usize::from(symbol)];
            let nbBits = nbAdditionalBits[usize::from(symbol)];
            ZSTD_buildSeqTable_rle(DTableSpace, baseline, nbBits);

            *DTablePtr = NonNull::new(DTableSpace);
            Ok(1)
        }
        SymbolEncodingType::Basic => {
            *DTablePtr = None;
            Ok(0)
        }
        SymbolEncodingType::Repeat => {
            if !flagRepeatTable {
                return Err(Error::corruption_detected);
            }
            if ddictIsCold && nbSeq > 24 {
                let pSize = size_of::<ZSTD_seqSymbol>().wrapping_mul(1 + (1usize << maxLog));
                if let Some(ptr) = *DTablePtr {
                    prefetch_area(ptr.as_ptr(), pSize);
                }
            }
            Ok(0)
        }
        SymbolEncodingType::Compressed => {
            let mut tableLog: core::ffi::c_uint = 0;
            let mut norm: [i16; 53] = [0; 53];
            let Ok(headerSize) = FSE_readNCount(&mut norm, &mut max, &mut tableLog, src) else {
                return Err(Error::corruption_detected);
            };
            if tableLog > maxLog {
                return Err(Error::corruption_detected);
            }
            ZSTD_buildFSETable(
                DTableSpace,
                &norm[..=max as usize],
                baseValue,
                nbAdditionalBits,
                tableLog,
                wksp.as_fse_workspace(),
                bmi2,
            );
            *DTablePtr = NonNull::new(DTableSpace);
            Ok(headerSize)
        }
    }
}

fn ZSTD_decodeSeqHeaders(
    dctx: &mut ZSTD_DCtx,
    nbSeqPtr: &mut core::ffi::c_int,
    src: &[u8],
) -> Result<usize, Error> {
    let mut ip = 0;
    let [nbSeq, ..] = *src else {
        return Err(Error::srcSize_wrong);
    };
    let mut nbSeq = i32::from(nbSeq);
    ip += 1;
    if nbSeq > 0x7f {
        if nbSeq == 0xff {
            let [_, a, b, ..] = *src else {
                return Err(Error::srcSize_wrong);
            };
            nbSeq = i32::from(u16::from_le_bytes([a, b])) + LONGNBSEQ;
            ip += 2;
        } else {
            if ip >= src.len() {
                return Err(Error::srcSize_wrong);
            }
            nbSeq = ((nbSeq - 0x80) << 8) + i32::from(src[ip]);
            ip += 1;
        }
    }
    *nbSeqPtr = nbSeq;
    if nbSeq == 0 {
        if ip != src.len() {
            return Err(Error::corruption_detected);
        }
        return Ok(ip);
    }

    /* FSE table descriptors */

    // Minimum possible size: 1 byte for symbol encoding types.
    if ip + 1 > src.len() {
        return Err(Error::srcSize_wrong);
    }

    // The last field, Reserved, must be all-zeroes.
    if src[ip] & 0b11 != 0 {
        return Err(Error::corruption_detected);
    }

    let byte = src[ip];
    let LLtype = SymbolEncodingType::try_from(byte >> 6).unwrap();
    let OFtype = SymbolEncodingType::try_from(byte >> 4 & 0b11).unwrap();
    let MLtype = SymbolEncodingType::try_from(byte >> 2 & 0b11).unwrap();

    /* Build DTables */

    ip += 1;
    let llhSize = ZSTD_buildSeqTableNew(
        &mut dctx.entropy.LLTable,
        &mut dctx.LLTptr,
        LLtype,
        MaxLL,
        LLFSELog,
        &src[ip..],
        &LL_base,
        &LL_bits,
        dctx.fseEntropy,
        dctx.ddictIsCold,
        nbSeq,
        &mut dctx.workspace,
        dctx.bmi2,
    )
    .map_err(|_| Error::corruption_detected)?;

    ip += llhSize;
    let ofhSize = ZSTD_buildSeqTableNew(
        &mut dctx.entropy.OFTable,
        &mut dctx.OFTptr,
        OFtype,
        MaxOff,
        OffFSELog,
        &src[ip..],
        &OF_base,
        &OF_bits,
        dctx.fseEntropy,
        dctx.ddictIsCold,
        nbSeq,
        &mut dctx.workspace,
        dctx.bmi2,
    )
    .map_err(|_| Error::corruption_detected)?;

    ip += ofhSize;
    let mlhSize = ZSTD_buildSeqTableNew(
        &mut dctx.entropy.MLTable,
        &mut dctx.MLTptr,
        MLtype,
        MaxML,
        MLFSELog,
        &src[ip..],
        &ML_base,
        &ML_bits,
        dctx.fseEntropy,
        dctx.ddictIsCold,
        nbSeq,
        &mut dctx.workspace,
        dctx.bmi2,
    )
    .map_err(|_| Error::corruption_detected)?;

    ip += mlhSize;

    Ok(ip)
}

///  Copies 8 bytes from ip to op and updates op and ip where ip <= op.
///  If the offset is < 8 then the offset is spread to at least 8 bytes.
///
///  Precondition: *ip <= *op
///  Postcondition: *op - *ip >= 8
#[inline(always)]
unsafe fn ZSTD_overlapCopy8(op: &mut *mut u8, ip: &mut *const u8, offset: usize) {
    if offset < 8 {
        /* close range match, overlap */
        *(*op).add(0) = *(*ip).add(0);
        *(*op).add(1) = *(*ip).add(1);
        *(*op).add(2) = *(*ip).add(2);
        *(*op).add(3) = *(*ip).add(3);

        static dec32table: [u8; 8] = [0, 1, 2, 1, 4, 4, 4, 4]; // added
        core::ptr::copy((*ip).add(usize::from(dec32table[offset])), (*op).add(4), 4);

        // dec32table[offset] + 8 - dec64table[offset]
        static advance: [u8; 8] = [0, 1, 2, 2, 4, 3, 2, 1];

        // Advance in one combined step so we never create an out-of-bounds pointer.
        *ip = (*ip).add(usize::from(advance[offset]));
    } else {
        core::ptr::copy(*ip, *op, 8);
        *ip = (*ip).add(8);
    }

    *op = (*op).add(8);

    debug_assert!(unsafe { (*op).offset_from(*ip) } >= 8);
}

/// Specialized version of `memcpy` that is allowed to READ up to [`WILDCOPY_OVERLENGTH`] past the input buffer
/// and write up to 16 bytes past `oend_w` (op >= `oend_w` is allowed).
/// This function is only called in the uncommon case where the sequence is near the end of the block. It
/// should be fast for a single long sequence, but can be slow for several short sequences.
///
/// @param ovtype controls the overlap detection
///     - [`Overlap::NoOverlap`]: The source and destination are guaranteed to be at least [`WILDCOPY_VECLEN`] bytes apart.
///     - [`Overlap::OverlapSrcBeforeDst`]: The src and dst may overlap and may be any distance apart.
///     The src buffer must be before the dst buffer.
unsafe fn ZSTD_safecopy(
    mut op: *mut u8,
    oend_w: *const u8,
    mut ip: *const u8,
    mut length: usize,
    ovtype: Overlap,
) {
    let diff = op as isize - ip as isize;
    let oend = op.add(length);

    debug_assert!(match ovtype {
        Overlap::NoOverlap => diff <= -8 || diff >= 8 || op >= oend_w.cast_mut(),
        Overlap::OverlapSrcBeforeDst => diff >= 0,
    });

    if length < 8 {
        /* Handle short lengths. */
        while op < oend {
            *op = *ip;
            ip = ip.add(1);
            op = op.add(1);
        }
        return;
    }
    if ovtype == Overlap::OverlapSrcBeforeDst {
        /* Copy 8 bytes and ensure the offset >= 8 when there can be overlap. */
        debug_assert!(length >= 8);
        debug_assert!(diff > 0);
        ZSTD_overlapCopy8(&mut op, &mut ip, diff as usize);
        length = length.wrapping_sub(8);
        debug_assert!(op.offset_from(ip) >= 8);
        debug_assert!(op <= oend);
    }
    if oend <= oend_w as *mut u8 {
        /* No risk of overwrite. */
        ZSTD_wildcopy(op, ip, length, ovtype);
        return;
    }
    if op <= oend_w as *mut u8 {
        /* Wildcopy until we get close to the end. */
        ZSTD_wildcopy(op, ip, oend_w.offset_from_unsigned(op), ovtype);
        ip = ip.add(oend_w.offset_from_unsigned(op));
        op = op.add(oend_w.offset_from_unsigned(op));
    }

    /* Handle the leftovers. */
    while op < oend {
        *op = *ip;
        ip = ip.add(1);
        op = op.add(1);
    }
}

/// This version allows overlap with dst before src, or handles the non-overlap case with dst after src
/// Kept separate from more common [`ZSTD_safecopy`] case to avoid performance impact to the safecopy common case */
unsafe fn ZSTD_safecopyDstBeforeSrc(mut op: *mut u8, mut ip: *const u8, length: usize) {
    let diff = op.offset_from(ip) as ptrdiff_t;
    let oend = op.add(length);
    if length < 8 || diff > -8 {
        /* Handle short lengths, close overlaps, and dst not before src. */
        while op < oend {
            *op = *ip;
            ip = ip.add(1);
            op = op.add(1);
        }
        return;
    }
    if op <= oend.sub(WILDCOPY_OVERLENGTH) && diff < -WILDCOPY_VECLEN as ptrdiff_t {
        ZSTD_wildcopy(
            op,
            ip,
            oend.sub(WILDCOPY_OVERLENGTH).offset_from_unsigned(op),
            Overlap::NoOverlap,
        );
        ip = ip.add(oend.sub(WILDCOPY_OVERLENGTH).offset_from_unsigned(op));
        op = op.add(oend.sub(WILDCOPY_OVERLENGTH).offset_from_unsigned(op));
    }

    /* Handle the leftovers. */
    while op < oend {
        *op = *ip;
        ip = ip.add(1);
        op = op.add(1);
    }
}

/// This version handles cases that are near the end of the output buffer. It requires
/// more careful checks to make sure there is no overflow. By separating out these hard
/// and unlikely cases, we can speed up the common cases.
///
/// NOTE: This function needs to be fast for a single long sequence, but doesn't need
/// to be optimized for many small sequences, since those fall into [`ZSTD_execSequence`].
#[inline(never)]
unsafe fn ZSTD_execSequenceEnd(
    mut op: *mut u8,
    oend: *mut u8,
    mut sequence: seq_t,
    litPtr: &mut *const u8,
    litLimit: *const u8,
    prefixStart: *const u8,
    virtualStart: *const u8,
    dictEnd: *const u8,
) -> Result<usize, Error> {
    let oLitEnd = op.add(sequence.litLength);
    let sequenceLength = sequence.litLength.wrapping_add(sequence.matchLength);
    let iLitEnd = (*litPtr).add(sequence.litLength);
    let mut match_0: *const u8 = oLitEnd.wrapping_sub(sequence.offset);
    let oend_w = oend.wrapping_sub(WILDCOPY_OVERLENGTH);

    /* bounds checks : careful of address space overflow in 32-bit mode */
    if sequenceLength > oend.offset_from_unsigned(op) {
        return Err(Error::dstSize_tooSmall);
    }
    if sequence.litLength > litLimit.offset_from_unsigned(*litPtr) {
        return Err(Error::corruption_detected);
    }

    debug_assert!(op < op.wrapping_add(sequenceLength));
    debug_assert!(oLitEnd < op.wrapping_add(sequenceLength));

    /* copy literals */
    ZSTD_safecopy(op, oend_w, *litPtr, sequence.litLength, Overlap::NoOverlap);
    op = oLitEnd;
    *litPtr = iLitEnd;

    /* copy Match */
    if sequence.offset > oLitEnd.offset_from_unsigned(prefixStart) {
        /* offset beyond prefix */
        if sequence.offset > (oLitEnd.addr() - virtualStart.addr()) {
            return Err(Error::corruption_detected);
        }
        match_0 = dictEnd.sub(prefixStart.addr() - match_0.addr());
        if match_0.add(sequence.matchLength) <= dictEnd {
            core::ptr::copy(match_0, oLitEnd, sequence.matchLength);
            return Ok(sequenceLength);
        }
        /* span extDict & currentPrefixSegment */
        let length1 = dictEnd.addr() - match_0.addr();
        core::ptr::copy(match_0, oLitEnd, length1);
        op = oLitEnd.add(length1);
        sequence.matchLength = sequence.matchLength.wrapping_sub(length1);
        match_0 = prefixStart;
    }
    ZSTD_safecopy(
        op,
        oend_w,
        match_0,
        sequence.matchLength,
        Overlap::OverlapSrcBeforeDst,
    );
    Ok(sequenceLength)
}

/// This version is intended to be used during instances where the litBuffer is still split.
/// It is kept separate to avoid performance impact for the good case.
#[inline(never)]
unsafe fn ZSTD_execSequenceEndSplitLitBuffer(
    mut op: Writer<'_>,
    oend: *mut u8,
    oend_w: *const u8,
    mut sequence: seq_t,
    litPtr: &mut *const u8,
    litLimit: *const u8,
    prefixStart: *const u8,
    virtualStart: *const u8,
    dictEnd: *const u8,
) -> Result<usize, Error> {
    let oLitEnd = op.as_mut_ptr().add(sequence.litLength);
    let sequenceLength = sequence.litLength.wrapping_add(sequence.matchLength);
    let iLitEnd = (*litPtr).add(sequence.litLength);
    let mut match_0: *const u8 = oLitEnd.sub(sequence.offset);

    /* bounds checks : careful of address space overflow in 32-bit mode */
    if sequenceLength > oend.offset_from_unsigned(op.as_mut_ptr()) {
        return Err(Error::dstSize_tooSmall);
    }
    if sequence.litLength > litLimit.offset_from_unsigned(*litPtr) {
        return Err(Error::corruption_detected);
    }

    debug_assert!(op.as_mut_ptr() < op.as_mut_ptr().wrapping_add(sequenceLength));
    debug_assert!(oLitEnd < op.as_mut_ptr().wrapping_add(sequenceLength));

    /* copy literals */
    if op.as_mut_ptr() > *litPtr as *mut u8
        && op.as_mut_ptr() < (*litPtr).add(sequence.litLength) as *mut u8
    {
        return Err(Error::dstSize_tooSmall);
    }
    ZSTD_safecopyDstBeforeSrc(op.as_mut_ptr(), *litPtr, sequence.litLength);
    op = op.subslice(sequence.litLength..);
    *litPtr = iLitEnd;

    /* copy Match */
    if sequence.offset > oLitEnd.offset_from_unsigned(prefixStart) {
        /* offset beyond prefix */
        if sequence.offset > oLitEnd.offset_from_unsigned(virtualStart) {
            return Err(Error::corruption_detected);
        }
        match_0 = dictEnd.sub(sequence.offset - oLitEnd.offset_from_unsigned(prefixStart));
        if match_0.add(sequence.matchLength) <= dictEnd {
            core::ptr::copy(match_0, oLitEnd, sequence.matchLength);
            return Ok(sequenceLength);
        }

        /* span extDict & currentPrefixSegment */
        let length1 = dictEnd.offset_from_unsigned(match_0);
        core::ptr::copy(match_0, oLitEnd, length1);
        op = op.subslice(length1..);
        sequence.matchLength = sequence.matchLength.wrapping_sub(length1);
        match_0 = prefixStart;
    }

    ZSTD_safecopy(
        op.as_mut_ptr(),
        oend_w,
        match_0,
        sequence.matchLength,
        Overlap::OverlapSrcBeforeDst,
    );
    Ok(sequenceLength)
}

#[inline(always)]
unsafe fn ZSTD_execSequence(
    mut op: Writer<'_>,
    oend: *mut u8,
    mut sequence: seq_t,
    litPtr: &mut *const u8,
    litLimit: *const u8,
    prefixStart: *const u8,
    virtualStart: *const u8,
    dictEnd: *const u8,
) -> Result<usize, Error> {
    let mut op = op.as_mut_ptr();
    let oLitEnd = op.add(sequence.litLength);
    let sequenceLength = sequence.litLength.wrapping_add(sequence.matchLength);
    let oMatchEnd = op.add(sequenceLength);
    let oend_w = oend.wrapping_sub(WILDCOPY_OVERLENGTH);
    let iLitEnd = (*litPtr).add(sequence.litLength);
    let mut match_0: *const u8 = oLitEnd.wrapping_sub(sequence.offset);

    debug_assert!(!op.is_null(), "Precondition");
    debug_assert!(oend_w < oend, "No underflow");

    if cfg!(target_arch = "aarch64") {
        // prefetch sequence starting from match that will be used for copy later.
        prefetch_read_data(match_0, Locality::L1);
    }

    // Handle edge cases in a slow path:
    //   - Read beyond end of literals
    //   - Match end is within WILDCOPY_OVERLIMIT of oend
    //   - 32-bit mode and the match length overflows
    if unlikely(
        iLitEnd > litLimit
            || oMatchEnd > oend_w
            || MEM_32bits() && oend.offset_from_unsigned(op) < sequenceLength.wrapping_add(32),
    ) {
        return ZSTD_execSequenceEnd(
            op,
            oend,
            sequence,
            litPtr,
            litLimit,
            prefixStart,
            virtualStart,
            dictEnd,
        );
    }

    /* Assumptions (everything else goes into ZSTD_execSequenceEnd()) */
    debug_assert!(op <= oLitEnd, "No overflow");
    debug_assert!(oLitEnd < oMatchEnd, "Non-zero match & no overflow");
    debug_assert!(oMatchEnd <= oend, "No underflow");
    debug_assert!(iLitEnd <= litLimit, "Literal length is in bounds");
    debug_assert!(oLitEnd <= oend_w, "Can wildcopy literals");
    debug_assert!(oMatchEnd <= oend_w, "Can wildcopy matches");

    // Copy Literals:
    // Split out litLength <= 16 since it is nearly always true. +1.6% on gcc-9.
    // We likely don't need the full 32-byte wildcopy.
    const _: () = assert!(WILDCOPY_OVERLENGTH >= 16);
    ZSTD_copy16(op, *litPtr);
    if unlikely(sequence.litLength > 16) {
        ZSTD_wildcopy(
            op.add(16),
            (*litPtr).add(16),
            sequence.litLength.wrapping_sub(16),
            Overlap::NoOverlap,
        );
    }
    op = oLitEnd;
    *litPtr = iLitEnd; // Update for next sequence.

    // Copy match.
    if unlikely(sequence.offset > oLitEnd.offset_from_unsigned(prefixStart)) {
        // offset beyond prefix -> go into extDict.
        if unlikely(sequence.offset > (oLitEnd.addr() - virtualStart.addr())) {
            return Err(Error::corruption_detected);
        }
        match_0 = dictEnd.offset(match_0.addr() as isize - prefixStart.addr() as isize);
        if match_0.add(sequence.matchLength) <= dictEnd {
            core::ptr::copy(match_0, oLitEnd, sequence.matchLength);
            return Ok(sequenceLength);
        }

        // span extDict & currentPrefixSegment.
        let length1 = dictEnd.offset_from_unsigned(match_0);
        core::ptr::copy(match_0, oLitEnd, length1);
        op = oLitEnd.add(length1);
        sequence.matchLength = sequence.matchLength.wrapping_sub(length1);
        match_0 = prefixStart;
    }

    /* Match within prefix of 1 or more bytes */
    debug_assert!(op <= oMatchEnd);
    debug_assert!(oMatchEnd <= oend_w);
    debug_assert!(match_0 >= prefixStart);
    debug_assert!(sequence.matchLength >= 1);

    // Nearly all offsets are >= WILDCOPY_VECLEN bytes, which means we can use wildcopy
    // without overlap checking.
    if likely(sequence.offset >= 16) {
        // We bet on a full wildcopy for matches, since we expect matches to be
        // longer than literals (in general). In silesia, ~10% of matches are longer
        // than 16 bytes.
        ZSTD_wildcopy(op, match_0, sequence.matchLength, Overlap::NoOverlap);
        return Ok(sequenceLength);
    }
    debug_assert!(sequence.offset < WILDCOPY_VECLEN as usize);

    // Copy 8 bytes and spread the offset to be >= 8.
    ZSTD_overlapCopy8(&mut op, &mut match_0, sequence.offset);

    // If the match length is > 8 bytes, then continue with the wildcopy.
    if sequence.matchLength > 8 {
        ZSTD_wildcopy(
            op,
            match_0,
            sequence.matchLength.wrapping_sub(8),
            Overlap::OverlapSrcBeforeDst,
        );
    }
    Ok(sequenceLength)
}

#[inline(always)]
unsafe fn ZSTD_execSequenceSplitLitBuffer(
    mut op: Writer<'_>,
    oend: *mut u8,
    oend_w: *const u8,
    mut sequence: seq_t,
    litPtr: &mut *const u8,
    litLimit: *const u8,
    prefixStart: *const u8,
    virtualStart: *const u8,
    dictEnd: *const u8,
) -> Result<usize, Error> {
    let oLitEnd = op.as_mut_ptr().add(sequence.litLength);
    let sequenceLength = sequence.litLength.wrapping_add(sequence.matchLength);
    let oMatchEnd = op.as_mut_ptr().add(sequenceLength);
    let iLitEnd = (*litPtr).add(sequence.litLength);
    let mut match_0: *const u8 = oLitEnd.sub(sequence.offset);

    debug_assert!(!op.is_null(), "precondition");
    debug_assert!(oend_w < oend, "No underflow");

    // Handle edge cases in a slow path:
    //   - Read beyond end of literals
    //   - Match end is within WILDCOPY_OVERLIMIT of oend
    //   - 32-bit mode and the match length overflows
    if unlikely(
        iLitEnd > litLimit
            || oMatchEnd > oend_w as *mut u8
            || MEM_32bits()
                && (oend.offset_from_unsigned(op.as_mut_ptr())) < sequenceLength.wrapping_add(32),
    ) {
        return ZSTD_execSequenceEndSplitLitBuffer(
            op,
            oend,
            oend_w,
            sequence,
            litPtr,
            litLimit,
            prefixStart,
            virtualStart,
            dictEnd,
        );
    }

    // Assumptions (everything else goes into ZSTD_execSequenceEnd())
    debug_assert!(op.as_mut_ptr() <= oLitEnd, "No overflow");
    debug_assert!(oLitEnd < oMatchEnd, "Non-zero match & no overflow");
    debug_assert!(oMatchEnd <= oend, "No underflow");
    debug_assert!(iLitEnd <= litLimit, "Literal length is in bounds");
    debug_assert!(oLitEnd <= oend_w.cast_mut(), "Can wildcopy literals");
    debug_assert!(oMatchEnd <= oend_w.cast_mut(), "Can wildcopy matches");

    // Copy Literals:
    // Split out litLength <= 16 since it is nearly always true. +1.6% on gcc-9.
    // We likely don't need the full 32-byte wildcopy.
    const _: () = assert!(WILDCOPY_OVERLENGTH >= 16);
    ZSTD_copy16(op.as_mut_ptr(), *litPtr);
    if unlikely(sequence.litLength > 16) {
        ZSTD_wildcopy(
            op.as_mut_ptr().add(16),
            (*litPtr).add(16),
            sequence.litLength.wrapping_sub(16),
            Overlap::NoOverlap,
        );
    }
    op = op.subslice(sequence.litLength..);
    *litPtr = iLitEnd; // Update for the next sequence.

    // Copy Match
    if unlikely(sequence.offset > oLitEnd.offset_from_unsigned(prefixStart)) {
        if sequence.offset > oLitEnd.offset_from_unsigned(virtualStart) {
            return Err(Error::corruption_detected);
        }
        match_0 = dictEnd.sub(sequence.offset - oLitEnd.offset_from_unsigned(prefixStart));
        if match_0.add(sequence.matchLength) <= dictEnd {
            core::ptr::copy(match_0, oLitEnd, sequence.matchLength);
            return Ok(sequenceLength);
        }

        /* span extDict & currentPrefixSegment */
        let length1 = dictEnd.offset_from_unsigned(match_0);
        core::ptr::copy(match_0, oLitEnd, length1);
        op = op.subslice(length1..);
        sequence.matchLength = sequence.matchLength.wrapping_sub(length1);
        match_0 = prefixStart;
    }

    /* Match within prefix of 1 or more bytes */
    debug_assert!(op.as_mut_ptr() <= oMatchEnd);
    debug_assert!(oMatchEnd <= oend_w.cast_mut());
    debug_assert!(match_0 >= prefixStart);
    debug_assert!(sequence.matchLength >= 1);

    if likely(sequence.offset >= WILDCOPY_VECLEN as usize) {
        // We bet on a full wildcopy for matches, since we expect matches to be
        // longer than literals (in general). In silesia, ~10% of matches are longer
        // than 16 bytes.
        ZSTD_wildcopy(
            op.as_mut_ptr(),
            match_0,
            sequence.matchLength,
            Overlap::NoOverlap,
        );
        return Ok(sequenceLength);
    }
    debug_assert!(sequence.offset < WILDCOPY_VECLEN as usize);

    // Copy 8 bytes and spread the offset to be >= 8.
    ZSTD_overlapCopy8(&mut op.as_mut_ptr(), &mut match_0, sequence.offset);
    op = op.subslice(8..);

    // If the match length is > 8 bytes, then continue with the wildcopy.
    if sequence.matchLength > 8 {
        ZSTD_wildcopy(
            op.as_mut_ptr(),
            match_0,
            sequence.matchLength.wrapping_sub(8),
            Overlap::OverlapSrcBeforeDst,
        );
    }
    Ok(sequenceLength)
}

impl<const N: usize> ZSTD_fseState<'_, N> {
    #[inline(always)]
    fn symbol(&self) -> ZSTD_seqSymbol {
        const { assert!(N.is_power_of_two()) };
        // A valid table only produces states below `1 << tableLog`, so the mask is a no-op.
        self.table[self.state & (N - 1)]
    }

    #[inline(always)]
    fn update_with_d_info(&mut self, bitD: &mut BIT_DStream_t, nextState: u16, nbBits: u32) {
        let lowBits = bitD.read_bits(nbBits);
        self.state = usize::from(nextState) + lowBits;
    }
}

/// We need to add at most `(ZSTD_WINDOWLOG_MAX_32 - 1)` bits to read the maximum
/// offset bits. But we can only read at most [`STREAM_ACCUMULATOR_MIN_32`]
/// bits before reloading. This value is the maximum number of bytes we read
/// after reloading when we are decoding long offsets.
const LONG_OFFSETS_MAX_EXTRA_BITS_32: u32 =
    (ZSTD_WINDOWLOG_MAX_32 as u32).saturating_sub(STREAM_ACCUMULATOR_MIN_32);

#[inline(always)]
fn ZSTD_decodeSequence(
    seqState: &mut seqState_t,
    longOffsets: Offset,
    is_last_sequence: bool,
) -> seq_t {
    let mut seq = seq_t {
        litLength: 0,
        matchLength: 0,
        offset: 0,
    };
    let llDInfo = seqState.stateLL.symbol();
    let mlDInfo = seqState.stateML.symbol();
    let ofDInfo = seqState.stateOffb.symbol();

    seq.matchLength = mlDInfo.baseValue as usize;
    seq.litLength = llDInfo.baseValue as usize;
    let ofBase = ofDInfo.baseValue;

    let llBits = llDInfo.nbAdditionalBits;
    let mlBits = mlDInfo.nbAdditionalBits;
    let ofBits = ofDInfo.nbAdditionalBits;

    let totalBits = llBits + mlBits + ofBits;

    let llNext = llDInfo.nextState;
    let mlNext = mlDInfo.nextState;
    let ofNext = ofDInfo.nextState;

    let llnbBits = llDInfo.nbBits as u32;
    let mlnbBits = mlDInfo.nbBits as u32;
    let ofnbBits = ofDInfo.nbBits as u32;

    debug_assert!(llBits <= MaxLLBits);
    debug_assert!(mlBits <= MaxMLBits);

    debug_assert!(ofBits <= MaxOff);

    let mut offset: usize;
    if ofBits > 1 {
        const { assert!(Offset::Long as usize == 1) };
        const { assert!(LONG_OFFSETS_MAX_EXTRA_BITS_32 == 5) };
        const { assert!(STREAM_ACCUMULATOR_MIN_32 > LONG_OFFSETS_MAX_EXTRA_BITS_32) };
        const { assert!(STREAM_ACCUMULATOR_MIN_32 - LONG_OFFSETS_MAX_EXTRA_BITS_32 >= MaxMLBits as u32) };

        if MEM_32bits()
            && longOffsets != Offset::Regular
            && u32::from(ofBits) >= STREAM_ACCUMULATOR_MIN_32
        {
            // Always read extra bits, this keeps the logic simple,
            // avoids branches, and avoids accidentally reading 0 bits.
            let extraBits = LONG_OFFSETS_MAX_EXTRA_BITS_32;
            offset = (ofBase as usize).wrapping_add(
                seqState
                    .DStream
                    .read_bits_fast(u32::from(ofBits) - extraBits)
                    << extraBits,
            );
            seqState.DStream.reload();
            offset = offset.wrapping_add(seqState.DStream.read_bits_fast(extraBits));
        } else {
            offset =
                (ofBase as usize).wrapping_add(seqState.DStream.read_bits_fast(u32::from(ofBits)));
            if MEM_32bits() {
                seqState.DStream.reload();
            }
        }

        seqState.prevOffset[2] = seqState.prevOffset[1];
        seqState.prevOffset[1] = seqState.prevOffset[0];
        seqState.prevOffset[0] = offset;
    } else {
        let ll0 = usize::from(llDInfo.baseValue == 0);
        if likely(ofBits == 0) {
            offset = seqState.prevOffset[ll0];
            seqState.prevOffset[1] = seqState.prevOffset[usize::from(ll0 == 0)];
            seqState.prevOffset[0] = offset;
        } else {
            offset = (ofBase.wrapping_add(ll0 as u32) as usize)
                .wrapping_add(seqState.DStream.read_bits_fast(1));

            let mut temp = match offset {
                3 => seqState.prevOffset[0] - 1,
                _ => seqState.prevOffset[offset],
            };
            temp = temp.wrapping_sub((temp == 0) as _); /* 0 is not valid: input corrupted => force offset to -1 => corruption detected at execSequence */

            if offset != 1 {
                seqState.prevOffset[2] = seqState.prevOffset[1];
            }
            seqState.prevOffset[1] = seqState.prevOffset[0];
            seqState.prevOffset[0] = temp;
            offset = temp;
        }
    }
    seq.offset = offset;

    if mlBits > 0 {
        seq.matchLength = seq
            .matchLength
            .wrapping_add(seqState.DStream.read_bits_fast(mlBits as core::ffi::c_uint));
    }

    if cfg!(target_pointer_width = "32")
        && (u32::from(mlBits + llBits)
            >= STREAM_ACCUMULATOR_MIN_32 - LONG_OFFSETS_MAX_EXTRA_BITS_32)
    {
        seqState.DStream.reload();
    }
    if cfg!(target_pointer_width = "64") && unlikely(totalBits >= 57 - (9 + 9 + 8)) {
        seqState.DStream.reload();
    }

    // Ensure there are enough bits to read the rest of data in 64-bit mode.
    const { assert!(16 + LLFSELog + MLFSELog + OffFSELog < STREAM_ACCUMULATOR_MIN_64) };

    if llBits > 0 {
        seq.litLength = (seq.litLength)
            .wrapping_add(seqState.DStream.read_bits_fast(llBits as core::ffi::c_uint));
    }
    if MEM_32bits() {
        seqState.DStream.reload();
    }

    // Don't update FSE state for last Sequence.
    if !is_last_sequence {
        seqState
            .stateLL
            .update_with_d_info(&mut seqState.DStream, llNext, llnbBits);
        seqState
            .stateML
            .update_with_d_info(&mut seqState.DStream, mlNext, mlnbBits);
        if MEM_32bits() {
            seqState.DStream.reload();
        }
        seqState
            .stateOffb
            .update_with_d_info(&mut seqState.DStream, ofNext, ofnbBits);
        seqState.DStream.reload();
    }

    seq
}

#[inline(always)]
fn ZSTD_decompressSequences_bodySplitLitBuffer(
    dctx: &mut ZSTD_DCtx,
    dst: Writer<'_>,
    seq: &[u8],
    mut nbSeq: core::ffi::c_int,
    offset: Offset,
) -> Result<usize, Error> {
    let maxDstSize = dst.capacity();
    let mut op = dst;
    let mut litPtr = dctx.litPtr;
    let mut litBufferEnd = dctx.litBufferEnd;
    let prefixStart = dctx.prefixStart as *const u8;
    let vBase = dctx.virtualStart as *const u8;
    let dictEnd = dctx.dictEnd as *const u8;
    if nbSeq != 0 {
        let DStream = BIT_DStream_t::new(seq).map_err(|_| Error::corruption_detected)?;
        dctx.fseEntropy = true;
        let mut seqState = dctx.new_seq_state(DStream);

        let mut sequence = seq_t::default();

        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        if !cfg!(miri) {
            unsafe { asm!(".p2align 6", options(preserves_flags, att_syntax)) }
        }

        while nbSeq != 0 {
            sequence = ZSTD_decodeSequence(&mut seqState, offset, nbSeq == 1);

            if litPtr.wrapping_add(sequence.litLength) > dctx.litBufferEnd {
                break;
            }

            let oneSeqSize = unsafe {
                ZSTD_execSequenceSplitLitBuffer(
                    op.subslice(..),
                    op.as_mut_ptr_range().end,
                    litPtr.add(sequence.litLength).sub(WILDCOPY_OVERLENGTH),
                    sequence,
                    &mut litPtr,
                    litBufferEnd,
                    prefixStart,
                    vBase,
                    dictEnd,
                )?
            };

            op = op.subslice(oneSeqSize..);
            nbSeq -= 1;
        }

        if nbSeq > 0 {
            let leftoverLit = unsafe { dctx.litBufferEnd.offset_from_unsigned(litPtr) };
            if leftoverLit != 0 {
                if leftoverLit > op.capacity() {
                    return Err(Error::dstSize_tooSmall);
                }
                unsafe { ZSTD_safecopyDstBeforeSrc(op.as_mut_ptr(), litPtr, leftoverLit) };
                sequence.litLength = sequence.litLength.wrapping_sub(leftoverLit);
                op = op.subslice(leftoverLit..);
            }
            litPtr = dctx.litExtraBuffer.as_mut_ptr();
            litBufferEnd = dctx.litExtraBuffer[ZSTD_LITBUFFEREXTRASIZE..].as_mut_ptr();
            dctx.litBufferLocation = LitLocation::ZSTD_not_in_dst;
            let oneSeqSize_0 = unsafe {
                ZSTD_execSequence(
                    op.subslice(..),
                    op.as_mut_ptr_range().end,
                    sequence,
                    &mut litPtr,
                    litBufferEnd,
                    prefixStart,
                    vBase,
                    dictEnd,
                )?
            };
            op = op.subslice(oneSeqSize_0..);
            nbSeq -= 1;
        }
        if nbSeq > 0 {
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            if !cfg!(miri) {
                unsafe {
                    asm!(
                        ".p2align 6",
                        "nop",
                        ".p2align 4",
                        "nop",
                        ".p2align 3",
                        options(preserves_flags)
                    )
                }
            }

            while nbSeq != 0 {
                let sequence_0 = ZSTD_decodeSequence(&mut seqState, offset, nbSeq == 1);
                let oneSeqSize_1 = unsafe {
                    ZSTD_execSequence(
                        op.subslice(..),
                        op.as_mut_ptr_range().end,
                        sequence_0,
                        &mut litPtr,
                        litBufferEnd,
                        prefixStart,
                        vBase,
                        dictEnd,
                    )?
                };
                op = op.subslice(oneSeqSize_1..);
                nbSeq -= 1;
            }
        }
        if nbSeq != 0 {
            return Err(Error::corruption_detected);
        }
        if !seqState.DStream.is_empty() {
            return Err(Error::corruption_detected);
        }

        dctx.entropy.rep = seqState.prevOffset.map(|v| v as u32);
    }

    if dctx.litBufferLocation == LitLocation::ZSTD_split {
        let lastLLSize = unsafe { litBufferEnd.offset_from_unsigned(litPtr) };
        if lastLLSize > op.capacity() {
            return Err(Error::dstSize_tooSmall);
        }
        if !op.is_null() {
            unsafe { core::ptr::copy(litPtr, op.as_mut_ptr(), lastLLSize) };
            op = op.subslice(lastLLSize..);
        }
        litPtr = dctx.litExtraBuffer.as_mut_ptr();
        litBufferEnd = dctx.litExtraBuffer[ZSTD_LITBUFFEREXTRASIZE..].as_mut_ptr();
        dctx.litBufferLocation = LitLocation::ZSTD_not_in_dst;
    }

    let lastLLSize_0 = unsafe { litBufferEnd.offset_from_unsigned(litPtr) };
    if lastLLSize_0 > op.capacity() {
        return Err(Error::dstSize_tooSmall);
    }

    if !op.is_null() {
        unsafe { core::ptr::copy_nonoverlapping(litPtr, op.as_mut_ptr(), lastLLSize_0) };
        op = op.subslice(lastLLSize_0..);
    }

    Ok(maxDstSize - op.capacity())
}

#[inline(always)]
unsafe fn ZSTD_decompressSequences_body(
    dctx: &mut ZSTD_DCtx,
    mut dst: Writer<'_>,
    seq: &[u8],
    nbSeq: core::ffi::c_int,
    offset: Offset,
) -> Result<usize, Error> {
    let capacity = dst.capacity();

    let oend = match dctx.litBufferLocation {
        LitLocation::ZSTD_not_in_dst => dst.as_mut_ptr_range().end,
        LitLocation::ZSTD_split | LitLocation::ZSTD_in_dst => dctx.litBuffer,
    };

    let mut op = dst;
    let mut litPtr = dctx.litPtr;
    let litEnd = litPtr.add(dctx.litSize);
    let prefixStart = dctx.prefixStart as *const u8;
    let vBase = dctx.virtualStart as *const u8;
    let dictEnd = dctx.dictEnd as *const u8;
    if nbSeq != 0 {
        let DStream = BIT_DStream_t::new(seq).map_err(|_| Error::corruption_detected)?;
        dctx.fseEntropy = true;
        let mut seqState = dctx.new_seq_state(DStream);

        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        if !cfg!(miri) {
            asm!(".p2align 6", options(preserves_flags, att_syntax));
            asm!("nop", options(preserves_flags, att_syntax));
            asm!(".p2align 4", options(preserves_flags, att_syntax));
            asm!("nop", options(preserves_flags, att_syntax));
            asm!(".p2align 3", options(preserves_flags, att_syntax));
        }

        for _ in 0..nbSeq - 1 {
            let sequence = ZSTD_decodeSequence(&mut seqState, offset, false);
            let oneSeqSize = ZSTD_execSequence(
                op.subslice(..),
                oend,
                sequence,
                &mut litPtr,
                litEnd,
                prefixStart,
                vBase,
                dictEnd,
            )?;

            op = op.subslice(oneSeqSize..);
        }

        let sequence = ZSTD_decodeSequence(&mut seqState, offset, true);
        let oneSeqSize = ZSTD_execSequence(
            op.subslice(..),
            oend,
            sequence,
            &mut litPtr,
            litEnd,
            prefixStart,
            vBase,
            dictEnd,
        )?;

        op = op.subslice(oneSeqSize..);

        if !seqState.DStream.is_empty() {
            return Err(Error::corruption_detected);
        }

        dctx.entropy.rep = seqState.prevOffset.map(|v| v as u32);
    }

    let lastLLSize = litEnd.offset_from_unsigned(litPtr);
    if lastLLSize > oend.offset_from_unsigned(op.as_mut_ptr()) {
        return Err(Error::dstSize_tooSmall);
    }

    if !op.is_null() {
        core::ptr::copy_nonoverlapping(litPtr, op.as_mut_ptr(), lastLLSize);
        op = op.subslice(lastLLSize..);
    }

    Ok(capacity - op.capacity())
}

fn ZSTD_decompressSequences_default(
    dctx: &mut ZSTD_DCtx,
    dst: Writer<'_>,
    seqStart: &[u8],
    nbSeq: core::ffi::c_int,
    offset: Offset,
) -> Result<usize, Error> {
    unsafe { ZSTD_decompressSequences_body(dctx, dst, seqStart, nbSeq, offset) }
}

fn ZSTD_decompressSequencesSplitLitBuffer_default(
    dctx: &mut ZSTD_DCtx,
    dst: Writer<'_>,
    seqStart: &[u8],
    nbSeq: core::ffi::c_int,
    offset: Offset,
) -> Result<usize, Error> {
    ZSTD_decompressSequences_bodySplitLitBuffer(dctx, dst, seqStart, nbSeq, offset)
}

#[inline(always)]
fn ZSTD_prefetchMatch(
    prefetchPos: usize,
    sequence: seq_t,
    prefixStart: *const u8,
    dictEnd: *const u8,
) -> usize {
    let matchBase = if sequence.offset > prefetchPos.wrapping_add(sequence.litLength) {
        dictEnd
    } else {
        prefixStart
    };

    let match_ = matchBase
        .wrapping_add(prefetchPos)
        .wrapping_sub(sequence.offset);

    prefetch_read_data(match_, Locality::L1);
    prefetch_read_data(match_.wrapping_add(64), Locality::L1);

    prefetchPos.wrapping_add(sequence.matchLength)
}

#[inline(always)]
unsafe fn ZSTD_decompressSequencesLong_body(
    dctx: &mut ZSTD_DCtx,
    mut dst: Writer<'_>,
    seq: &[u8],
    nbSeq: core::ffi::c_int,
    offset: Offset,
) -> Result<usize, Error> {
    let dst_capacity = dst.capacity();
    let oend = if dctx.litBufferLocation == LitLocation::ZSTD_in_dst {
        dctx.litBuffer
    } else {
        dst.as_mut_ptr_range().end
    };
    let mut op = dst;
    let mut litPtr = dctx.litPtr;
    let mut litBufferEnd = dctx.litBufferEnd;
    let prefixStart = dctx.prefixStart as *const u8;
    let dictStart = dctx.virtualStart as *const u8;
    let dictEnd = dctx.dictEnd as *const u8;
    if nbSeq != 0 {
        let seqAdvance = if nbSeq < 8 { nbSeq } else { 8 };
        let DStream = BIT_DStream_t::new(seq).map_err(|_| Error::corruption_detected)?;
        dctx.fseEntropy = true;
        let mut seqState = dctx.new_seq_state(DStream);

        let mut prefetchPos = op.as_mut_ptr().offset_from_unsigned(prefixStart);
        let mut sequences: [seq_t; 8] = [seq_t::default(); 8];

        for seqNb in 0..seqAdvance {
            let sequence = ZSTD_decodeSequence(&mut seqState, offset, seqNb == nbSeq - 1);
            prefetchPos = ZSTD_prefetchMatch(prefetchPos, sequence, prefixStart, dictEnd);
            sequences[seqNb as usize] = sequence;
        }

        for seqNb in seqAdvance..nbSeq {
            let sequence_0 = ZSTD_decodeSequence(&mut seqState, offset, seqNb == nbSeq - 1);
            if dctx.litBufferLocation == LitLocation::ZSTD_split
                && litPtr
                    .add(sequences[((seqNb - ADVANCED_SEQS) & STORED_SEQS_MASK) as usize].litLength)
                    > dctx.litBufferEnd
            {
                let leftoverLit = dctx.litBufferEnd.offset_from_unsigned(litPtr);
                if leftoverLit != 0 {
                    if leftoverLit > oend.offset_from_unsigned(op.as_mut_ptr()) {
                        return Err(Error::dstSize_tooSmall);
                    }
                    ZSTD_safecopyDstBeforeSrc(op.as_mut_ptr(), litPtr, leftoverLit);
                    sequences[((seqNb - ADVANCED_SEQS) & STORED_SEQS_MASK) as usize].litLength -=
                        leftoverLit;
                    op = op.subslice(leftoverLit..);
                }
                litPtr = dctx.litExtraBuffer.as_mut_ptr();
                litBufferEnd = dctx.litExtraBuffer[ZSTD_LITBUFFEREXTRASIZE..].as_mut_ptr();
                dctx.litBufferLocation = LitLocation::ZSTD_not_in_dst;
                let oneSeqSize = ZSTD_execSequence(
                    op.subslice(..),
                    oend,
                    sequences[((seqNb - ADVANCED_SEQS) & STORED_SEQS_MASK) as usize],
                    &mut litPtr,
                    litBufferEnd,
                    prefixStart,
                    dictStart,
                    dictEnd,
                )?;

                prefetchPos = ZSTD_prefetchMatch(prefetchPos, sequence_0, prefixStart, dictEnd);
                sequences[(seqNb & STORED_SEQS_MASK) as usize] = sequence_0;
                op = op.subslice(oneSeqSize..);
            } else {
                let sequence = sequences[((seqNb - ADVANCED_SEQS) & STORED_SEQS_MASK) as usize];
                let oneSeqSize_0 = if dctx.litBufferLocation == LitLocation::ZSTD_split {
                    ZSTD_execSequenceSplitLitBuffer(
                        op.subslice(..),
                        oend,
                        litPtr.add(sequence.litLength).sub(WILDCOPY_OVERLENGTH),
                        sequences[((seqNb - ADVANCED_SEQS) & STORED_SEQS_MASK) as usize],
                        &mut litPtr,
                        litBufferEnd,
                        prefixStart,
                        dictStart,
                        dictEnd,
                    )?
                } else {
                    ZSTD_execSequence(
                        op.subslice(..),
                        oend,
                        sequence,
                        &mut litPtr,
                        litBufferEnd,
                        prefixStart,
                        dictStart,
                        dictEnd,
                    )?
                };

                prefetchPos = ZSTD_prefetchMatch(prefetchPos, sequence_0, prefixStart, dictEnd);
                sequences[(seqNb & STORED_SEQS_MASK) as usize] = sequence_0;
                op = op.subslice(oneSeqSize_0..);
            }
        }

        if !seqState.DStream.is_empty() {
            return Err(Error::corruption_detected);
        }

        for seqNb in nbSeq - seqAdvance..nbSeq {
            let sequence = &mut sequences[(seqNb & STORED_SEQS_MASK) as usize];
            if dctx.litBufferLocation == LitLocation::ZSTD_split
                && litPtr.add(sequence.litLength) > dctx.litBufferEnd
            {
                let leftoverLit_0 = dctx.litBufferEnd.offset_from_unsigned(litPtr);
                if leftoverLit_0 != 0 {
                    if leftoverLit_0 > oend.offset_from_unsigned(op.as_mut_ptr()) {
                        return Err(Error::dstSize_tooSmall);
                    }
                    ZSTD_safecopyDstBeforeSrc(op.as_mut_ptr(), litPtr, leftoverLit_0);
                    sequence.litLength = sequence.litLength.wrapping_sub(leftoverLit_0);
                    op = op.subslice(leftoverLit_0..);
                }
                litPtr = dctx.litExtraBuffer.as_mut_ptr();
                litBufferEnd = dctx.litExtraBuffer[ZSTD_LITBUFFEREXTRASIZE..].as_mut_ptr();
                dctx.litBufferLocation = LitLocation::ZSTD_not_in_dst;
                let oneSeqSize_1 = ZSTD_execSequence(
                    op.subslice(..),
                    oend,
                    *sequence,
                    &mut litPtr,
                    litBufferEnd,
                    prefixStart,
                    dictStart,
                    dictEnd,
                )?;
                op = op.subslice(oneSeqSize_1..);
            } else {
                let oneSeqSize_2 = if dctx.litBufferLocation == LitLocation::ZSTD_split {
                    ZSTD_execSequenceSplitLitBuffer(
                        op.subslice(..),
                        oend,
                        litPtr.add(sequence.litLength).sub(WILDCOPY_OVERLENGTH),
                        *sequence,
                        &mut litPtr,
                        litBufferEnd,
                        prefixStart,
                        dictStart,
                        dictEnd,
                    )?
                } else {
                    ZSTD_execSequence(
                        op.subslice(..),
                        oend,
                        *sequence,
                        &mut litPtr,
                        litBufferEnd,
                        prefixStart,
                        dictStart,
                        dictEnd,
                    )?
                };

                op = op.subslice(oneSeqSize_2..);
            }
        }

        dctx.entropy.rep = seqState.prevOffset.map(|v| v as u32);
    }

    if dctx.litBufferLocation == LitLocation::ZSTD_split {
        let lastLLSize = litBufferEnd.offset_from_unsigned(litPtr);
        if lastLLSize > oend.offset_from_unsigned(op.as_mut_ptr()) {
            return Err(Error::dstSize_tooSmall);
        }
        if !op.is_null() {
            core::ptr::copy(litPtr, op.as_mut_ptr(), lastLLSize);
            op = op.subslice(lastLLSize..);
        }
        litPtr = dctx.litExtraBuffer.as_mut_ptr();
        litBufferEnd = dctx.litExtraBuffer[ZSTD_LITBUFFEREXTRASIZE..].as_mut_ptr();
    }

    let lastLLSize_0 = litBufferEnd.offset_from_unsigned(litPtr);
    if lastLLSize_0 > oend.offset_from_unsigned(op.as_mut_ptr()) {
        return Err(Error::dstSize_tooSmall);
    }

    if !op.is_null() {
        unsafe {
            core::ptr::copy(litPtr, op.as_mut_ptr(), lastLLSize_0);
        }
        op = op.subslice(lastLLSize_0..);
    }

    Ok(dst_capacity - op.capacity())
}

pub const STORED_SEQS: core::ffi::c_int = 8;
pub const STORED_SEQS_MASK: core::ffi::c_int = STORED_SEQS - 1;
pub const ADVANCED_SEQS: core::ffi::c_int = STORED_SEQS;

fn ZSTD_decompressSequencesLong_default(
    dctx: &mut ZSTD_DCtx,
    dst: Writer<'_>,
    seqStart: &[u8],
    nbSeq: core::ffi::c_int,
    offset: Offset,
) -> Result<usize, Error> {
    unsafe { ZSTD_decompressSequencesLong_body(dctx, dst, seqStart, nbSeq, offset) }
}

#[cfg_attr(target_arch = "x86_64", target_feature(enable = "bmi2"))]
fn ZSTD_decompressSequences_bmi2(
    dctx: &mut ZSTD_DCtx,
    dst: Writer<'_>,
    seqStart: &[u8],
    nbSeq: core::ffi::c_int,
    offset: Offset,
) -> Result<usize, Error> {
    unsafe { ZSTD_decompressSequences_body(dctx, dst, seqStart, nbSeq, offset) }
}

#[cfg_attr(target_arch = "x86_64", target_feature(enable = "bmi2"))]
fn ZSTD_decompressSequencesSplitLitBuffer_bmi2(
    dctx: &mut ZSTD_DCtx,
    dst: Writer<'_>,
    seqStart: &[u8],
    nbSeq: core::ffi::c_int,
    offset: Offset,
) -> Result<usize, Error> {
    ZSTD_decompressSequences_bodySplitLitBuffer(dctx, dst, seqStart, nbSeq, offset)
}

#[cfg_attr(target_arch = "x86_64", target_feature(enable = "bmi2"))]
fn ZSTD_decompressSequencesLong_bmi2(
    dctx: &mut ZSTD_DCtx,
    dst: Writer<'_>,
    seqStart: &[u8],
    nbSeq: core::ffi::c_int,
    offset: Offset,
) -> Result<usize, Error> {
    unsafe { ZSTD_decompressSequencesLong_body(dctx, dst, seqStart, nbSeq, offset) }
}

fn ZSTD_decompressSequences(
    dctx: &mut ZSTD_DCtx,
    dst: Writer<'_>,
    seqStart: &[u8],
    nbSeq: core::ffi::c_int,
    offset: Offset,
) -> Result<usize, Error> {
    if dctx.bmi2 {
        unsafe { ZSTD_decompressSequences_bmi2(dctx, dst, seqStart, nbSeq, offset) }
    } else {
        ZSTD_decompressSequences_default(dctx, dst, seqStart, nbSeq, offset)
    }
}

fn ZSTD_decompressSequencesSplitLitBuffer(
    dctx: &mut ZSTD_DCtx,
    dst: Writer<'_>,
    seqStart: &[u8],
    nbSeq: core::ffi::c_int,
    offset: Offset,
) -> Result<usize, Error> {
    if dctx.bmi2 {
        unsafe { ZSTD_decompressSequencesSplitLitBuffer_bmi2(dctx, dst, seqStart, nbSeq, offset) }
    } else {
        ZSTD_decompressSequencesSplitLitBuffer_default(dctx, dst, seqStart, nbSeq, offset)
    }
}

fn ZSTD_decompressSequencesLong(
    dctx: &mut ZSTD_DCtx,
    dst: Writer<'_>,
    seqStart: &[u8],
    nbSeq: core::ffi::c_int,
    offset: Offset,
) -> Result<usize, Error> {
    if dctx.bmi2 {
        unsafe { ZSTD_decompressSequencesLong_bmi2(dctx, dst, seqStart, nbSeq, offset) }
    } else {
        ZSTD_decompressSequencesLong_default(dctx, dst, seqStart, nbSeq, offset)
    }
}

impl<const N: usize> SymbolTable<N> {
    fn get_offset_info(&self, nbSeq: usize) -> ZSTD_OffsetInfo {
        let mut info = ZSTD_OffsetInfo::default();

        if nbSeq == 0 {
            return info;
        }

        let tableLog = self.header.tableLog;
        for seq_symbol in &self.symbols[..1 << tableLog] {
            info.maxNbAdditionalBits = Ord::max(
                info.maxNbAdditionalBits,
                u32::from(seq_symbol.nbAdditionalBits),
            );

            if seq_symbol.nbAdditionalBits > 22 {
                info.longOffsetShare += 1;
            }
        }
        info.longOffsetShare <<= OffFSELog.wrapping_sub(tableLog);

        info
    }
}

/// @returns The maximum offset we can decode in one read of our bitstream, without
/// reloading more bits in the middle of the offset bits read. Any offsets larger
/// than this must use the long offset decoder.
const fn ZSTD_maxShortOffset() -> usize {
    match size_of::<usize>() {
        4 => {
            // The maximum offBase is (1 << (STREAM_ACCUMULATOR_MIN + 1)) - 1.
            // This offBase would require STREAM_ACCUMULATOR_MIN extra bits.
            // Then we have to subtract ZSTD_REP_NUM to get the maximum possible offset.
            let maxOffbase = (1usize << (STREAM_ACCUMULATOR_MIN + 1)).wrapping_sub(1);

            maxOffbase.wrapping_sub(ZSTD_REP_NUM as usize)
        }
        8 => {
            // We can decode any offset without reloading bits.
            // This might change if the max window size grows.
            const { assert!(ZSTD_WINDOWLOG_MAX <= 31) }

            -(1 as core::ffi::c_int) as usize
        }
        _ => unreachable!(),
    }
}

pub(crate) fn ZSTD_decompressBlock_internal_help(
    dctx: &mut ZSTD_DCtx,
    mut dst: Writer<'_>,
    src: &[u8],
    streaming: StreamingOperation,
) -> Result<usize, Error> {
    if src.len() > dctx.block_size_max() {
        return Err(Error::srcSize_wrong);
    }

    let litCSize = ZSTD_decodeLiteralsBlock(dctx, src, dst.subslice(..), streaming)?;

    let mut ip = &src[litCSize..];

    let blockSizeMax = Ord::min(dst.capacity(), dctx.block_size_max());
    let totalHistorySize =
        dst.as_mut_ptr().wrapping_add(blockSizeMax) as usize - dctx.virtualStart as usize;
    let mut offset = if MEM_32bits() && totalHistorySize > ZSTD_maxShortOffset() {
        Offset::Long
    } else {
        Offset::Regular
    };
    let mut use_prefetch_decoder = dctx.ddictIsCold;
    let mut nbSeq = 0;
    let seqHSize = ZSTD_decodeSeqHeaders(dctx, &mut nbSeq, ip)?;
    ip = &ip[seqHSize..];
    if dst.is_empty() && nbSeq > 0 {
        return Err(Error::dstSize_tooSmall);
    }
    if size_of::<usize>() == 8
        && size_of::<usize>() == size_of::<*mut core::ffi::c_void>()
        && (usize::MAX - dst.as_mut_ptr() as usize) < (1 << 20)
    {
        return Err(Error::dstSize_tooSmall);
    }
    if offset == Offset::Long
        || !use_prefetch_decoder && totalHistorySize > (1 << 24) as usize && nbSeq > 8
    {
        let info = match dctx.OFTptr {
            None => OF_defaultDTable.get_offset_info(nbSeq as usize),
            Some(table) => (unsafe { &*table.as_ptr() }).get_offset_info(nbSeq as usize),
        };

        if offset == Offset::Long && info.maxNbAdditionalBits <= STREAM_ACCUMULATOR_MIN {
            offset = Offset::Regular;
        }

        if !use_prefetch_decoder {
            let minShare = (if size_of::<usize>() == 8 { 7 } else { 20 }) as u32;
            use_prefetch_decoder = info.longOffsetShare >= minShare;
        }
    }

    dctx.ddictIsCold = false;

    if use_prefetch_decoder {
        ZSTD_decompressSequencesLong(dctx, dst.subslice(..), ip, nbSeq, offset)
    } else if dctx.litBufferLocation == LitLocation::ZSTD_split {
        ZSTD_decompressSequencesSplitLitBuffer(dctx, dst, ip, nbSeq, offset)
    } else {
        ZSTD_decompressSequences(dctx, dst, ip, nbSeq, offset)
    }
}

pub fn ZSTD_checkContinuity(dctx: &mut ZSTD_DCtx, range: Range<*const u8>) {
    if range.start.cast() != dctx.previousDstEnd && !range.is_empty() {
        dctx.dictEnd = dctx.previousDstEnd;
        let delta = dctx.previousDstEnd.addr() - dctx.prefixStart.addr();
        dctx.virtualStart = range.start.wrapping_sub(delta).cast();
        dctx.prefixStart = range.start.cast();
        dctx.previousDstEnd = range.start.cast();
    }
}

#[deprecated(
    note = "The block API is deprecated in favor of the normal compression API. See docs."
)]
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_decompressBlock))]
pub unsafe extern "C" fn ZSTD_decompressBlock(
    dctx: *mut ZSTD_DCtx,
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    let dst = Writer::from_raw_parts(dst.cast::<u8>(), dstCapacity);
    let src = Reader::from_raw_parts(src.cast::<u8>(), srcSize);

    let dctx: &mut ZSTD_DCtx = dctx.as_mut().unwrap();
    let mut dst = dst;
    dctx.isFrameDecompression = false;

    ZSTD_checkContinuity(dctx, dst.as_ptr_range());

    // FIXME: can src and dst overlap in this case?
    let dSize = ZSTD_decompressBlock_internal_help(
        dctx,
        dst.subslice(..),
        src.as_slice(),
        StreamingOperation::NotStreaming,
    )
    .unwrap_or_else(|err| err.to_error_code());

    dctx.previousDstEnd = dst.as_ptr().byte_add(dSize).cast::<c_void>();
    dSize
}

#[cfg(test)]
mod test {
    use crate::{lib::zstd::*, ZSTD_isError};
    use core::ffi::*;

    #[test]
    fn basic_decompress() {
        rs(&[40, 181, 47, 253, 48, 21, 44, 0, 0, 0, 253, 49, 0, 21]);
    }

    /// Trigger what was previously a pointer subtraction in `ZSTD_overlapCopy8`.
    /// Check that we don't create an OOB pointer.
    #[test]
    fn overlap_copy8_near_start_of_output() {
        use crate::lib::compress::zstd_compress::{ZSTD_compress, ZSTD_compressBound};

        let input = vec![b'a'; 64];
        let mut compressed = vec![0u8; ZSTD_compressBound(input.len())];
        let n = unsafe {
            ZSTD_compress(
                compressed.as_mut_ptr().cast(),
                compressed.len(),
                input.as_ptr().cast(),
                input.len(),
                1,
            )
        };
        assert_eq!(ZSTD_isError(n), 0);

        let (len, decompressed) = rs(&compressed[..n]);
        assert_eq!(len, input.len());
        assert_eq!(decompressed, input);
    }

    fn rs(compressed: &[u8]) -> (usize, Vec<u8>) {
        use crate::lib::decompress::zstd_decompress::*;

        let compressed_ptr = compressed.as_ptr() as *const c_void;
        let compressed_size = compressed.len();

        // Get decompressed size from frame header
        let decompressed_size =
            unsafe { ZSTD_getFrameContentSize(compressed_ptr, compressed_size) };
        if decompressed_size == ZSTD_CONTENTSIZE_ERROR {
            panic!("ZSTD_CONTENTSIZE_ERROR");
        } else if decompressed_size == ZSTD_CONTENTSIZE_UNKNOWN {
            panic!("ZSTD_CONTENTSIZE_UNKNOWN");
        }

        // Allocate buffer for decompressed output
        let mut decompressed = vec![0u8; Ord::min(decompressed_size as usize, 1 << 20)];
        let result = unsafe {
            ZSTD_decompress(
                decompressed.as_mut_ptr() as *mut c_void,
                decompressed.len(),
                compressed_ptr,
                compressed_size,
            )
        };

        (result as usize, decompressed)
    }
}
