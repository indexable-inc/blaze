use core::ptr;

use libc::{free, malloc, memcpy, ptrdiff_t};

use crate::lib::common::error_private::{ERR_isError, Error};
use crate::lib::common::mem::{
    MEM_32bits, MEM_64bits, MEM_readLE16, MEM_readLE32, MEM_readLE64, MEM_readLEST, MEM_writeLE16,
};
use crate::ZSTD_CONTENTSIZE_ERROR;

#[repr(C)]
pub(crate) struct ZSTDv06_DCtx {
    LLTable: [FSEv06_DTable; 513],
    OffTable: [FSEv06_DTable; 257],
    MLTable: [FSEv06_DTable; 513],
    hufTableX4: [core::ffi::c_uint; 4097],
    previousDstEnd: *const core::ffi::c_void,
    base: *const core::ffi::c_void,
    vBase: *const core::ffi::c_void,
    dictEnd: *const core::ffi::c_void,
    expected: usize,
    headerSize: usize,
    fParams: ZSTDv06_frameParams,
    bType: blockType_t,
    stage: ZSTDv06_dStage,
    flagRepeatTable: u32,
    litPtr: *const u8,
    litSize: usize,
    litBuffer: [u8; 131080],
    headerBuffer: [u8; 13],
}
type ZSTDv06_dStage = core::ffi::c_uint;
const ZSTDds_decompressBlock: ZSTDv06_dStage = 3;
const ZSTDds_decodeBlockHeader: ZSTDv06_dStage = 2;
const ZSTDds_decodeFrameHeader: ZSTDv06_dStage = 1;
const ZSTDds_getFrameHeaderSize: ZSTDv06_dStage = 0;
type blockType_t = core::ffi::c_uint;
const bt_end: blockType_t = 3;
const bt_rle: blockType_t = 2;
const bt_compressed: blockType_t = 0;
type ZSTDv06_frameParams = ZSTDv06_frameParams_s;
#[derive(Copy, Clone, Default)]
#[repr(C)]
pub(crate) struct ZSTDv06_frameParams_s {
    pub(crate) frameContentSize: core::ffi::c_ulonglong,
    pub(crate) windowLog: core::ffi::c_uint,
}
type FSEv06_DTable = core::ffi::c_uint;
#[derive(Copy, Clone)]
#[repr(C)]
struct blockProperties_t {
    blockType: blockType_t,
    origSize: u32,
}
#[derive(Copy, Clone)]
#[repr(C)]
struct seq_t {
    litLength: usize,
    matchLength: usize,
    offset: usize,
}
#[derive(Copy, Clone)]
#[repr(C)]
struct seqState_t {
    DStream: BITv06_DStream_t,
    stateLL: FSEv06_DState_t,
    stateOffb: FSEv06_DState_t,
    stateML: FSEv06_DState_t,
    prevOffset: [usize; 3],
}
#[derive(Copy, Clone)]
#[repr(C)]
struct FSEv06_DState_t {
    state: usize,
    table: *const core::ffi::c_void,
}
#[derive(Copy, Clone)]
#[repr(C)]
struct BITv06_DStream_t {
    bitContainer: usize,
    bitsConsumed: core::ffi::c_uint,
    ptr: *const core::ffi::c_char,
    start: *const core::ffi::c_char,
}
#[derive(Copy, Clone)]
#[repr(C)]
struct FSEv06_decode_t {
    newState: core::ffi::c_ushort,
    symbol: core::ffi::c_uchar,
    nbBits: core::ffi::c_uchar,
}
type BITv06_DStream_status = core::ffi::c_uint;
const BITv06_DStream_overflow: BITv06_DStream_status = 3;
const BITv06_DStream_completed: BITv06_DStream_status = 2;
const BITv06_DStream_endOfBuffer: BITv06_DStream_status = 1;
const BITv06_DStream_unfinished: BITv06_DStream_status = 0;
#[derive(Copy, Clone)]
#[repr(C)]
struct FSEv06_DTableHeader {
    tableLog: u16,
    fastMode: u16,
}

#[derive(Copy, Clone)]
#[repr(C)]
struct HUFv06_DEltX4 {
    sequence: u16,
    nbBits: u8,
    length: u8,
}
type decompressionAlgo =
    Option<unsafe fn(*mut core::ffi::c_void, usize, *const core::ffi::c_void, usize) -> usize>;
type rankVal_t = [[u32; 17]; 16];
#[derive(Copy, Clone)]
#[repr(C)]
struct sortedSymbol_t {
    symbol: u8,
    weight: u8,
}
type DTable_max_t = [u32; 4097];
#[derive(Copy, Clone)]
#[repr(C)]
struct HUFv06_DEltX2 {
    byte: u8,
    nbBits: u8,
}
#[derive(Copy, Clone)]
#[repr(C)]
struct algo_time_t {
    tableTime: u32,
    decode256Time: u32,
}
#[repr(C)]
pub(crate) struct ZBUFFv06_DCtx_s {
    zd: *mut ZSTDv06_DCtx,
    fParams: ZSTDv06_frameParams,
    stage: ZBUFFv06_dStage,
    inBuff: *mut core::ffi::c_char,
    inBuffSize: usize,
    inPos: usize,
    outBuff: *mut core::ffi::c_char,
    outBuffSize: usize,
    outStart: usize,
    outEnd: usize,
    blockSize: usize,
    headerBuffer: [u8; 13],
    lhSize: usize,
}
type ZBUFFv06_dStage = core::ffi::c_uint;
const ZBUFFds_flush: ZBUFFv06_dStage = 4;
const ZBUFFds_load: ZBUFFv06_dStage = 3;
const ZBUFFds_read: ZBUFFv06_dStage = 2;
const ZBUFFds_loadHeader: ZBUFFv06_dStage = 1;
const ZBUFFds_init: ZBUFFv06_dStage = 0;
type ZBUFFv06_DCtx = ZBUFFv06_DCtx_s;
const ZSTDv06_MAGICNUMBER: core::ffi::c_uint = 0xfd2fb526 as core::ffi::c_uint;
static ZSTDv06_frameHeaderSize_min: usize = 5;
const ZSTDv06_BLOCKSIZE_MAX: core::ffi::c_int = 128 * 1024;
const ZSTDv06_DICT_MAGIC: core::ffi::c_uint = 0xec30a436 as core::ffi::c_uint;
const ZSTDv06_REP_NUM: core::ffi::c_int = 3;
const ZSTDv06_REP_INIT: core::ffi::c_int = 3;
const ZSTDv06_REP_MOVE: core::ffi::c_int = ZSTDv06_REP_NUM - 1;
const ZSTDv06_WINDOWLOG_ABSOLUTEMIN: core::ffi::c_int = 12;
static ZSTDv06_fcs_fieldSize: [usize; 4] = [0, 1, 2, 8];
const ZSTDv06_BLOCKHEADERSIZE: core::ffi::c_int = 3;
static ZSTDv06_blockHeaderSize: usize = ZSTDv06_BLOCKHEADERSIZE as usize;
const MIN_SEQUENCES_SIZE: core::ffi::c_int = 1;
const MIN_CBLOCK_SIZE: core::ffi::c_int = 1 + 1 + MIN_SEQUENCES_SIZE;
const ZSTD_HUFFDTABLE_CAPACITY_LOG: core::ffi::c_int = 12;
const IS_HUF: core::ffi::c_int = 0;
const IS_PCH: core::ffi::c_int = 1;
const IS_RAW: core::ffi::c_int = 2;
const IS_RLE: core::ffi::c_int = 3;
const LONGNBSEQ: core::ffi::c_int = 0x7f00 as core::ffi::c_int;
const MINMATCH: core::ffi::c_int = 3;
const REPCODE_STARTVALUE: core::ffi::c_int = 1;
const MaxML: core::ffi::c_int = 52;
const MaxLL: core::ffi::c_int = 35;
const MaxOff: core::ffi::c_int = 28;
const MLFSELog: core::ffi::c_int = 9;
const LLFSELog: core::ffi::c_int = 9;
const OffFSELog: core::ffi::c_int = 8;
static LL_bits: [u32; 36] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10, 11,
    12, 13, 14, 15, 16,
];
static LL_defaultNorm: [i16; 36] = [
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];
static LL_defaultNormLog: u32 = 6;
static ML_bits: [u32; 53] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
];
static ML_defaultNorm: [i16; 53] = [
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
];
static ML_defaultNormLog: u32 = 6;
static OF_defaultNorm: [i16; 29] = [
    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
];
static OF_defaultNormLog: u32 = 5;
unsafe fn ZSTDv06_copy8(dst: *mut core::ffi::c_void, src: *const core::ffi::c_void) {
    memcpy(dst, src, 8);
}
const WILDCOPY_OVERLENGTH: core::ffi::c_int = 8;
#[inline]
unsafe fn ZSTDv06_wildcopy(
    dst: *mut core::ffi::c_void,
    src: *const core::ffi::c_void,
    length: ptrdiff_t,
) {
    let mut ip = src as *const u8;
    let mut op = dst as *mut u8;
    let oend = op.offset(length);
    loop {
        ZSTDv06_copy8(op as *mut core::ffi::c_void, ip as *const core::ffi::c_void);
        op = op.add(8);
        ip = ip.add(8);
        if op >= oend {
            break;
        }
    }
}
#[inline]
unsafe fn BITv06_highbit32(val: u32) -> core::ffi::c_uint {
    (val.leading_zeros() as i32 ^ 31) as core::ffi::c_uint
}

#[inline]
unsafe fn BITv06_initDStream(
    bitD: *mut BITv06_DStream_t,
    srcBuffer: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    if srcSize < 1 {
        ptr::write_bytes(bitD as *mut u8, 0, size_of::<BITv06_DStream_t>());
        return Error::srcSize_wrong.to_error_code();
    }

    let bitD = &mut *bitD;

    if srcSize >= size_of::<usize>() {
        // normal case
        bitD.start = srcBuffer as *const core::ffi::c_char;
        bitD.ptr = (srcBuffer as *const core::ffi::c_char)
            .add(srcSize)
            .sub(size_of::<usize>());
        bitD.bitContainer = MEM_readLEST(bitD.ptr as *const core::ffi::c_void);
        let lastByte = *(srcBuffer as *const u8).add(srcSize.wrapping_sub(1));
        if lastByte == 0 {
            return Error::GENERIC.to_error_code(); // endMark not present
        }
        bitD.bitsConsumed = 8 - BITv06_highbit32(lastByte as u32);
    } else {
        bitD.start = srcBuffer as *const core::ffi::c_char;
        bitD.ptr = bitD.start;
        bitD.bitContainer = *(bitD.start as *const u8) as usize;

        if srcSize == 7 {
            bitD.bitContainer +=
                (*(bitD.start as *const u8).add(6) as usize) << (usize::BITS as usize - 16);
        }

        if srcSize >= 6 {
            bitD.bitContainer +=
                (*(bitD.start as *const u8).add(5) as usize) << (usize::BITS as usize - 24);
        }

        if srcSize >= 5 {
            bitD.bitContainer +=
                (*(bitD.start as *const u8).add(4) as usize) << (usize::BITS as usize - 32);
        }

        if srcSize >= 4 {
            bitD.bitContainer += (*(bitD.start as *const u8).add(3) as usize) << 24;
        }

        if srcSize >= 3 {
            bitD.bitContainer += (*(bitD.start as *const u8).add(2) as usize) << 16;
        }

        if srcSize >= 2 {
            bitD.bitContainer += (*(bitD.start as *const u8).add(1) as usize) << 8;
        }

        let lastByte = *(srcBuffer as *const u8).add(srcSize - 1);
        if lastByte == 0 {
            // endMark not present
            return Error::GENERIC.to_error_code();
        }
        bitD.bitsConsumed = (8 as core::ffi::c_uint) - BITv06_highbit32(lastByte as u32);
        bitD.bitsConsumed += (size_of::<usize>() - srcSize) as u32 * 8;
    }
    srcSize
}

#[inline]
unsafe fn BITv06_lookBits(bitD: *const BITv06_DStream_t, nbBits: u32) -> usize {
    let bitMask = usize::BITS - 1;
    (*bitD).bitContainer << ((*bitD).bitsConsumed & bitMask)
        >> 1
        >> (bitMask.wrapping_sub(nbBits) & bitMask)
}
#[inline]
unsafe fn BITv06_lookBitsFast(bitD: *const BITv06_DStream_t, nbBits: u32) -> usize {
    let bitMask = usize::BITS - 1;
    (*bitD).bitContainer << ((*bitD).bitsConsumed & bitMask)
        >> (bitMask.wrapping_add(1).wrapping_sub(nbBits) & bitMask)
}
#[inline]
unsafe fn BITv06_skipBits(bitD: *mut BITv06_DStream_t, nbBits: u32) {
    (*bitD).bitsConsumed = ((*bitD).bitsConsumed).wrapping_add(nbBits);
}
#[inline]
unsafe fn BITv06_readBits(bitD: *mut BITv06_DStream_t, nbBits: u32) -> usize {
    let value = BITv06_lookBits(bitD, nbBits);
    BITv06_skipBits(bitD, nbBits);
    value
}
#[inline]
unsafe fn BITv06_readBitsFast(bitD: *mut BITv06_DStream_t, nbBits: u32) -> usize {
    let value = BITv06_lookBitsFast(bitD, nbBits);
    BITv06_skipBits(bitD, nbBits);
    value
}
#[inline]
unsafe fn BITv06_reloadDStream(bitD: *mut BITv06_DStream_t) -> BITv06_DStream_status {
    if (*bitD).bitsConsumed > usize::BITS {
        return BITv06_DStream_overflow;
    }
    if (*bitD).ptr >= ((*bitD).start).add(size_of::<usize>()) {
        (*bitD).ptr = ((*bitD).ptr).offset(-(((*bitD).bitsConsumed >> 3) as isize));
        (*bitD).bitsConsumed &= 7;
        (*bitD).bitContainer = MEM_readLEST((*bitD).ptr as *const core::ffi::c_void);
        return BITv06_DStream_unfinished;
    }
    if (*bitD).ptr == (*bitD).start {
        if (*bitD).bitsConsumed < usize::BITS {
            return BITv06_DStream_endOfBuffer;
        }
        return BITv06_DStream_completed;
    }
    let mut nbBytes = (*bitD).bitsConsumed as usize >> 3;
    let mut result = BITv06_DStream_unfinished;
    if ((*bitD).ptr).sub(nbBytes) < (*bitD).start {
        nbBytes = ((*bitD).ptr).offset_from_unsigned((*bitD).start);
        result = BITv06_DStream_endOfBuffer;
    }
    (*bitD).ptr = ((*bitD).ptr).sub(nbBytes);
    (*bitD).bitsConsumed = ((*bitD).bitsConsumed).wrapping_sub((nbBytes * 8) as u32);
    (*bitD).bitContainer = MEM_readLEST((*bitD).ptr as *const core::ffi::c_void);
    result
}
#[inline]
unsafe fn BITv06_endOfDStream(DStream: *const BITv06_DStream_t) -> core::ffi::c_uint {
    ((*DStream).ptr == (*DStream).start && (*DStream).bitsConsumed == usize::BITS)
        as core::ffi::c_uint
}
#[inline]
unsafe fn FSEv06_initDState(
    DStatePtr: *mut FSEv06_DState_t,
    bitD: *mut BITv06_DStream_t,
    dt: *const FSEv06_DTable,
) {
    let ptr = dt as *const core::ffi::c_void;
    let DTableH = ptr as *const FSEv06_DTableHeader;
    (*DStatePtr).state = BITv06_readBits(bitD, (*DTableH).tableLog as core::ffi::c_uint);
    BITv06_reloadDStream(bitD);
    (*DStatePtr).table = dt.add(1) as *const core::ffi::c_void;
}
#[inline]
unsafe fn FSEv06_peekSymbol(DStatePtr: *const FSEv06_DState_t) -> u8 {
    let DInfo = *((*DStatePtr).table as *const FSEv06_decode_t).add((*DStatePtr).state);
    DInfo.symbol
}
#[inline]
unsafe fn FSEv06_updateState(DStatePtr: *mut FSEv06_DState_t, bitD: *mut BITv06_DStream_t) {
    let DInfo = *((*DStatePtr).table as *const FSEv06_decode_t).add((*DStatePtr).state);
    let nbBits = DInfo.nbBits as u32;
    let lowBits = BITv06_readBits(bitD, nbBits);
    (*DStatePtr).state = (DInfo.newState as usize).wrapping_add(lowBits);
}
#[inline]
unsafe fn FSEv06_decodeSymbol(
    DStatePtr: *mut FSEv06_DState_t,
    bitD: *mut BITv06_DStream_t,
) -> core::ffi::c_uchar {
    let DInfo = *((*DStatePtr).table as *const FSEv06_decode_t).add((*DStatePtr).state);
    let nbBits = DInfo.nbBits as u32;
    let symbol = DInfo.symbol;
    let lowBits = BITv06_readBits(bitD, nbBits);
    (*DStatePtr).state = (DInfo.newState as usize).wrapping_add(lowBits);
    symbol
}
#[inline]
unsafe fn FSEv06_decodeSymbolFast(
    DStatePtr: *mut FSEv06_DState_t,
    bitD: *mut BITv06_DStream_t,
) -> core::ffi::c_uchar {
    let DInfo = *((*DStatePtr).table as *const FSEv06_decode_t).add((*DStatePtr).state);
    let nbBits = DInfo.nbBits as u32;
    let symbol = DInfo.symbol;
    let lowBits = BITv06_readBitsFast(bitD, nbBits);
    (*DStatePtr).state = (DInfo.newState as usize).wrapping_add(lowBits);
    symbol
}
const FSEv06_MAX_MEMORY_USAGE: core::ffi::c_int = 14;
const FSEv06_MAX_SYMBOL_VALUE: core::ffi::c_int = 255;
const FSEv06_MAX_TABLELOG: core::ffi::c_int = FSEv06_MAX_MEMORY_USAGE - 2;
const FSEv06_MIN_TABLELOG: core::ffi::c_int = 5;
const FSEv06_TABLELOG_ABSOLUTE_MAX: core::ffi::c_int = 15;
unsafe fn HUFv06_isError(code: usize) -> core::ffi::c_uint {
    ERR_isError(code) as _
}
unsafe fn FSEv06_abs(a: core::ffi::c_short) -> core::ffi::c_short {
    (if (a as core::ffi::c_int) < 0 {
        -(a as core::ffi::c_int)
    } else {
        a as core::ffi::c_int
    }) as core::ffi::c_short
}
unsafe fn FSEv06_readNCount(
    normalizedCounter: *mut core::ffi::c_short,
    maxSVPtr: *mut core::ffi::c_uint,
    tableLogPtr: *mut core::ffi::c_uint,
    headerBuffer: *const core::ffi::c_void,
    hbSize: usize,
) -> usize {
    let istart = headerBuffer as *const u8;
    let iend = istart.add(hbSize);
    let mut ip = istart;
    let mut nbBits: core::ffi::c_int = 0;
    let mut remaining: core::ffi::c_int = 0;
    let mut threshold: core::ffi::c_int = 0;
    let mut bitStream: u32 = 0;
    let mut bitCount: core::ffi::c_int = 0;
    let mut charnum = 0;
    let mut previous0 = false;
    if hbSize < 4 {
        return Error::srcSize_wrong.to_error_code();
    }
    bitStream = MEM_readLE32(ip as *const core::ffi::c_void);
    nbBits = (bitStream & 0xf as core::ffi::c_int as u32).wrapping_add(FSEv06_MIN_TABLELOG as u32)
        as core::ffi::c_int;
    if nbBits > FSEv06_TABLELOG_ABSOLUTE_MAX {
        return Error::tableLog_tooLarge.to_error_code();
    }
    bitStream >>= 4;
    bitCount = 4;
    *tableLogPtr = nbBits as core::ffi::c_uint;
    remaining = (1 << nbBits) + 1;
    threshold = 1 << nbBits;
    nbBits += 1;
    while remaining > 1 && charnum <= *maxSVPtr {
        if previous0 {
            let mut n0 = charnum;
            while bitStream & 0xffff as core::ffi::c_int as u32 == 0xffff as core::ffi::c_int as u32
            {
                n0 = n0.wrapping_add(24);
                if ip < iend.sub(5) {
                    ip = ip.add(2);
                    bitStream = MEM_readLE32(ip as *const core::ffi::c_void) >> bitCount;
                } else {
                    bitStream >>= 16;
                    bitCount += 16;
                }
            }
            while bitStream & 3 == 3 {
                n0 = n0.wrapping_add(3);
                bitStream >>= 2;
                bitCount += 2;
            }
            n0 = n0.wrapping_add(bitStream & 3);
            bitCount += 2;
            if n0 > *maxSVPtr {
                return Error::maxSymbolValue_tooSmall.to_error_code();
            }
            while charnum < n0 {
                *normalizedCounter.offset(charnum as isize) = 0;
                charnum = charnum.wrapping_add(1);
            }
            if ip <= iend.sub(7) || ip.offset((bitCount >> 3) as isize) <= iend.sub(4) {
                ip = ip.offset((bitCount >> 3) as isize);
                bitCount &= 7;
                bitStream = MEM_readLE32(ip as *const core::ffi::c_void) >> bitCount;
            } else {
                bitStream >>= 2;
            }
        }
        let max = (2 * threshold - 1 - remaining) as core::ffi::c_short;
        let mut count: core::ffi::c_short = 0;
        if (bitStream & (threshold - 1) as u32) < max as u32 {
            count = (bitStream & (threshold - 1) as u32) as core::ffi::c_short;
            bitCount += nbBits - 1;
        } else {
            count = (bitStream & (2 * threshold - 1) as u32) as core::ffi::c_short;
            if count as core::ffi::c_int >= threshold {
                count = (count as core::ffi::c_int - max as core::ffi::c_int) as core::ffi::c_short;
            }
            bitCount += nbBits;
        }
        count -= 1;
        remaining -= FSEv06_abs(count) as core::ffi::c_int;
        *normalizedCounter.offset(charnum as isize) = count;
        charnum = charnum.wrapping_add(1);
        previous0 = count == 0;
        while remaining < threshold {
            nbBits -= 1;
            threshold >>= 1;
        }
        if ip <= iend.sub(7) || ip.offset((bitCount >> 3) as isize) <= iend.sub(4) {
            ip = ip.offset((bitCount >> 3) as isize);
            bitCount &= 7;
        } else {
            bitCount -= (8 * iend.sub(4).offset_from(ip) as core::ffi::c_long) as core::ffi::c_int;
            ip = iend.sub(4);
        }
        bitStream = MEM_readLE32(ip as *const core::ffi::c_void) >> (bitCount & 31);
    }
    if remaining != 1 {
        return Error::GENERIC.to_error_code();
    }
    *maxSVPtr = charnum.wrapping_sub(1);
    ip = ip.offset(((bitCount + 7) >> 3) as isize);
    if ip.offset_from_unsigned(istart) > hbSize {
        return Error::srcSize_wrong.to_error_code();
    }
    ip.offset_from_unsigned(istart)
}
unsafe fn FSEv06_buildDTable(
    dt: *mut FSEv06_DTable,
    normalizedCounter: *const core::ffi::c_short,
    maxSymbolValue: core::ffi::c_uint,
    tableLog: core::ffi::c_uint,
) -> usize {
    let tdPtr = dt.add(1) as *mut core::ffi::c_void;
    let tableDecode = tdPtr as *mut FSEv06_decode_t;
    let mut symbolNext: [u16; 256] = [0; 256];
    let maxSV1 = maxSymbolValue.wrapping_add(1);
    let tableSize = (1 << tableLog) as u32;
    let mut highThreshold = tableSize.wrapping_sub(1);
    if maxSymbolValue > FSEv06_MAX_SYMBOL_VALUE as core::ffi::c_uint {
        return Error::maxSymbolValue_tooLarge.to_error_code();
    }
    if tableLog > FSEv06_MAX_TABLELOG as core::ffi::c_uint {
        return Error::tableLog_tooLarge.to_error_code();
    }
    let mut DTableH = FSEv06_DTableHeader {
        tableLog: 0,
        fastMode: 0,
    };
    DTableH.tableLog = tableLog as u16;
    DTableH.fastMode = 1;
    let largeLimit = (1 << tableLog.wrapping_sub(1)) as i16;
    for s in 0..maxSV1 {
        if *normalizedCounter.offset(s as isize) as core::ffi::c_int == -1 {
            (*tableDecode.offset(highThreshold as isize)).symbol = s as u8;
            highThreshold = highThreshold.wrapping_sub(1);
            *symbolNext.as_mut_ptr().offset(s as isize) = 1;
        } else {
            if *normalizedCounter.offset(s as isize) as core::ffi::c_int
                >= largeLimit as core::ffi::c_int
            {
                DTableH.fastMode = 0;
            }
            *symbolNext.as_mut_ptr().offset(s as isize) =
                *normalizedCounter.offset(s as isize) as u16;
        }
    }
    memcpy(
        dt as *mut core::ffi::c_void,
        &mut DTableH as *mut FSEv06_DTableHeader as *const core::ffi::c_void,
        size_of::<FSEv06_DTableHeader>(),
    );
    let tableMask = tableSize.wrapping_sub(1);
    let step = (tableSize >> 1)
        .wrapping_add(tableSize >> 3)
        .wrapping_add(3);
    let mut position = 0u32;
    for s in 0..maxSV1 {
        for _ in 0..*normalizedCounter.offset(s as isize) as core::ffi::c_int {
            (*tableDecode.offset(position as isize)).symbol = s as u8;
            position = position.wrapping_add(step) & tableMask;
            while position > highThreshold {
                position = position.wrapping_add(step) & tableMask;
            }
        }
    }
    if position != 0 {
        return Error::GENERIC.to_error_code();
    }
    for u in 0..tableSize {
        let symbol = (*tableDecode.offset(u as isize)).symbol;
        let fresh3 = &mut (*symbolNext.as_mut_ptr().offset(symbol as isize));
        let nextState = *fresh3;
        *fresh3 = (*fresh3).wrapping_add(1);
        (*tableDecode.offset(u as isize)).nbBits =
            tableLog.wrapping_sub(BITv06_highbit32(nextState as u32)) as u8;
        (*tableDecode.offset(u as isize)).newState = (((nextState as core::ffi::c_int)
            << (*tableDecode.offset(u as isize)).nbBits as core::ffi::c_int)
            as u32)
            .wrapping_sub(tableSize) as u16;
    }
    0
}
unsafe fn FSEv06_buildDTable_rle(dt: *mut FSEv06_DTable, symbolValue: u8) -> usize {
    let ptr = dt as *mut core::ffi::c_void;
    let DTableH = ptr as *mut FSEv06_DTableHeader;
    let dPtr = dt.add(1) as *mut core::ffi::c_void;
    let cell = dPtr as *mut FSEv06_decode_t;
    (*DTableH).tableLog = 0;
    (*DTableH).fastMode = 0;
    (*cell).newState = 0;
    (*cell).symbol = symbolValue;
    (*cell).nbBits = 0;
    0
}
#[inline(always)]
unsafe fn FSEv06_decompress_usingDTable_generic(
    dst: *mut core::ffi::c_void,
    maxDstSize: usize,
    cSrc: *const core::ffi::c_void,
    cSrcSize: usize,
    dt: *const FSEv06_DTable,
    fast: core::ffi::c_uint,
) -> usize {
    let ostart = dst as *mut u8;
    let mut op = ostart;
    let omax = op.add(maxDstSize);
    let olimit = omax.sub(3);
    let mut bitD = BITv06_DStream_t {
        bitContainer: 0,
        bitsConsumed: 0,
        ptr: core::ptr::null::<core::ffi::c_char>(),
        start: core::ptr::null::<core::ffi::c_char>(),
    };
    let mut state1 = FSEv06_DState_t {
        state: 0,
        table: core::ptr::null::<core::ffi::c_void>(),
    };
    let mut state2 = FSEv06_DState_t {
        state: 0,
        table: core::ptr::null::<core::ffi::c_void>(),
    };
    let errorCode = BITv06_initDStream(&mut bitD, cSrc, cSrcSize);
    if ERR_isError(errorCode) {
        return errorCode;
    }
    FSEv06_initDState(&mut state1, &mut bitD, dt);
    FSEv06_initDState(&mut state2, &mut bitD, dt);
    while BITv06_reloadDStream(&mut bitD) == BITv06_DStream_unfinished && op < olimit {
        *op = (if fast != 0 {
            FSEv06_decodeSymbolFast(&mut state1, &mut bitD) as core::ffi::c_int
        } else {
            FSEv06_decodeSymbol(&mut state1, &mut bitD) as core::ffi::c_int
        }) as u8;
        if FSEv06_MAX_TABLELOG * 2 + 7 > usize::BITS as core::ffi::c_int {
            BITv06_reloadDStream(&mut bitD);
        }
        *op.add(1) = (if fast != 0 {
            FSEv06_decodeSymbolFast(&mut state2, &mut bitD) as core::ffi::c_int
        } else {
            FSEv06_decodeSymbol(&mut state2, &mut bitD) as core::ffi::c_int
        }) as u8;
        if FSEv06_MAX_TABLELOG * 4 + 7 > usize::BITS as core::ffi::c_int
            && BITv06_reloadDStream(&mut bitD) > BITv06_DStream_unfinished
        {
            op = op.add(2);
            break;
        }
        *op.add(2) = (if fast != 0 {
            FSEv06_decodeSymbolFast(&mut state1, &mut bitD) as core::ffi::c_int
        } else {
            FSEv06_decodeSymbol(&mut state1, &mut bitD) as core::ffi::c_int
        }) as u8;
        if FSEv06_MAX_TABLELOG * 2 + 7 > usize::BITS as std::ffi::c_int {
            BITv06_reloadDStream(&mut bitD);
        }
        *op.add(3) = (if fast != 0 {
            FSEv06_decodeSymbolFast(&mut state2, &mut bitD) as core::ffi::c_int
        } else {
            FSEv06_decodeSymbol(&mut state2, &mut bitD) as core::ffi::c_int
        }) as u8;
        op = op.add(4);
    }
    loop {
        if op > omax.sub(2) {
            return Error::dstSize_tooSmall.to_error_code();
        }
        *op = (if fast != 0 {
            FSEv06_decodeSymbolFast(&mut state1, &mut bitD) as core::ffi::c_int
        } else {
            FSEv06_decodeSymbol(&mut state1, &mut bitD) as core::ffi::c_int
        }) as u8;
        op = op.add(1);
        if BITv06_reloadDStream(&mut bitD) == BITv06_DStream_overflow {
            *op = (if fast != 0 {
                FSEv06_decodeSymbolFast(&mut state2, &mut bitD) as core::ffi::c_int
            } else {
                FSEv06_decodeSymbol(&mut state2, &mut bitD) as core::ffi::c_int
            }) as u8;
            op = op.add(1);
            break;
        } else {
            if op > omax.sub(2) {
                return Error::dstSize_tooSmall.to_error_code();
            }
            *op = (if fast != 0 {
                FSEv06_decodeSymbolFast(&mut state2, &mut bitD) as core::ffi::c_int
            } else {
                FSEv06_decodeSymbol(&mut state2, &mut bitD) as core::ffi::c_int
            }) as u8;
            op = op.add(1);
            if BITv06_reloadDStream(&mut bitD) != BITv06_DStream_overflow {
                continue;
            }
            *op = (if fast != 0 {
                FSEv06_decodeSymbolFast(&mut state1, &mut bitD) as core::ffi::c_int
            } else {
                FSEv06_decodeSymbol(&mut state1, &mut bitD) as core::ffi::c_int
            }) as u8;
            op = op.add(1);
            break;
        }
    }
    op.offset_from_unsigned(ostart)
}
unsafe fn FSEv06_decompress_usingDTable(
    dst: *mut core::ffi::c_void,
    originalSize: usize,
    cSrc: *const core::ffi::c_void,
    cSrcSize: usize,
    dt: *const FSEv06_DTable,
) -> usize {
    let ptr = dt as *const core::ffi::c_void;
    let DTableH = ptr as *const FSEv06_DTableHeader;
    let fastMode = (*DTableH).fastMode as u32;
    if fastMode != 0 {
        return FSEv06_decompress_usingDTable_generic(dst, originalSize, cSrc, cSrcSize, dt, 1);
    }
    FSEv06_decompress_usingDTable_generic(dst, originalSize, cSrc, cSrcSize, dt, 0)
}
unsafe fn FSEv06_decompress(
    dst: *mut core::ffi::c_void,
    maxDstSize: usize,
    cSrc: *const core::ffi::c_void,
    mut cSrcSize: usize,
) -> usize {
    let istart = cSrc as *const u8;
    let mut ip = istart;
    let mut counting: [core::ffi::c_short; 256] = [0; 256];
    let mut dt: DTable_max_t = [0; 4097];
    let mut tableLog: core::ffi::c_uint = 0;
    let mut maxSymbolValue = FSEv06_MAX_SYMBOL_VALUE as core::ffi::c_uint;
    if cSrcSize < 2 {
        return Error::srcSize_wrong.to_error_code();
    }
    let NCountLength = FSEv06_readNCount(
        counting.as_mut_ptr(),
        &mut maxSymbolValue,
        &mut tableLog,
        istart as *const core::ffi::c_void,
        cSrcSize,
    );
    if ERR_isError(NCountLength) {
        return NCountLength;
    }
    if NCountLength >= cSrcSize {
        return Error::srcSize_wrong.to_error_code();
    }
    ip = ip.add(NCountLength);
    cSrcSize = cSrcSize.wrapping_sub(NCountLength);
    let errorCode = FSEv06_buildDTable(
        dt.as_mut_ptr(),
        counting.as_mut_ptr(),
        maxSymbolValue,
        tableLog,
    );
    if ERR_isError(errorCode) {
        return errorCode;
    }
    FSEv06_decompress_usingDTable(
        dst,
        maxDstSize,
        ip as *const core::ffi::c_void,
        cSrcSize,
        dt.as_mut_ptr(),
    )
}
const HUFv06_ABSOLUTEMAX_TABLELOG: core::ffi::c_int = 16;
const HUFv06_MAX_TABLELOG: core::ffi::c_int = 12;
const HUFv06_MAX_SYMBOL_VALUE: core::ffi::c_int = 255;
#[inline]
unsafe fn HUFv06_readStats(
    huffWeight: *mut u8,
    hwSize: usize,
    rankStats: *mut u32,
    nbSymbolsPtr: *mut u32,
    tableLogPtr: *mut u32,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    let mut weightTotal: u32 = 0;
    let mut ip = src as *const u8;
    let mut iSize: usize = 0;
    let mut oSize: usize = 0;
    if srcSize == 0 {
        return Error::srcSize_wrong.to_error_code();
    }
    iSize = *ip as usize;
    if iSize >= 128 {
        if iSize >= 242 {
            static l: [u32; 14] = [1, 2, 3, 4, 7, 8, 15, 16, 31, 32, 63, 64, 127, 128];
            oSize = l[iSize.wrapping_sub(242)] as usize;
            core::ptr::write_bytes(huffWeight, 1, hwSize);
            iSize = 0;
        } else {
            oSize = iSize.wrapping_sub(127);
            iSize = oSize.wrapping_add(1) / 2;
            if iSize.wrapping_add(1) > srcSize {
                return Error::srcSize_wrong.to_error_code();
            }
            if oSize >= hwSize {
                return Error::corruption_detected.to_error_code();
            }
            ip = ip.add(1);
            for n in (0..oSize).step_by(2) {
                *huffWeight.add(n) = (*ip.add(n / 2) as core::ffi::c_int >> 4) as u8;
                *huffWeight.add(n + 1) = (*ip.add(n / 2) as core::ffi::c_int & 15) as u8;
            }
        }
    } else {
        if iSize.wrapping_add(1) > srcSize {
            return Error::srcSize_wrong.to_error_code();
        }
        oSize = FSEv06_decompress(
            huffWeight as *mut core::ffi::c_void,
            hwSize.wrapping_sub(1),
            ip.add(1) as *const core::ffi::c_void,
            iSize,
        );
        if ERR_isError(oSize) {
            return oSize;
        }
    }
    core::ptr::write_bytes(rankStats, 0, (HUFv06_ABSOLUTEMAX_TABLELOG + 1) as usize);
    weightTotal = 0;
    let mut n_0: u32 = 0;
    while (n_0 as usize) < oSize {
        if *huffWeight.offset(n_0 as isize) as core::ffi::c_int >= HUFv06_ABSOLUTEMAX_TABLELOG {
            return Error::corruption_detected.to_error_code();
        }
        let fresh9 = &mut (*rankStats.offset(*huffWeight.offset(n_0 as isize) as isize));
        *fresh9 = (*fresh9).wrapping_add(1);
        weightTotal = weightTotal
            .wrapping_add((1 << *huffWeight.offset(n_0 as isize) as core::ffi::c_int >> 1) as u32);
        n_0 = n_0.wrapping_add(1);
    }
    if weightTotal == 0 {
        return Error::corruption_detected.to_error_code();
    }
    let tableLog = BITv06_highbit32(weightTotal).wrapping_add(1);
    if tableLog > HUFv06_ABSOLUTEMAX_TABLELOG as u32 {
        return Error::corruption_detected.to_error_code();
    }
    *tableLogPtr = tableLog;
    let total = (1 << tableLog) as u32;
    let rest = total.wrapping_sub(weightTotal);
    let verif = (1 << BITv06_highbit32(rest)) as u32;
    let lastWeight = BITv06_highbit32(rest).wrapping_add(1);
    if verif != rest {
        return Error::corruption_detected.to_error_code();
    }
    *huffWeight.add(oSize) = lastWeight as u8;
    let fresh10 = &mut (*rankStats.offset(lastWeight as isize));
    *fresh10 = (*fresh10).wrapping_add(1);
    if *rankStats.add(1) < 2 || *rankStats.add(1) & 1 != 0 {
        return Error::corruption_detected.to_error_code();
    }
    *nbSymbolsPtr = oSize.wrapping_add(1) as u32;
    iSize.wrapping_add(1)
}
unsafe fn HUFv06_readDTableX2(
    DTable: *mut u16,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    let mut huffWeight: [u8; 256] = [0; 256];
    let mut rankVal: [u32; 17] = [0; 17];
    let mut tableLog = 0;
    let mut iSize: usize = 0;
    let mut nbSymbols = 0;
    let mut nextRankStart: u32 = 0;
    let dtPtr = DTable.add(1) as *mut core::ffi::c_void;
    let dt = dtPtr as *mut HUFv06_DEltX2;
    iSize = HUFv06_readStats(
        huffWeight.as_mut_ptr(),
        (HUFv06_MAX_SYMBOL_VALUE + 1) as usize,
        rankVal.as_mut_ptr(),
        &mut nbSymbols,
        &mut tableLog,
        src,
        srcSize,
    );
    if HUFv06_isError(iSize) != 0 {
        return iSize;
    }
    if tableLog > *DTable as u32 {
        return Error::tableLog_tooLarge.to_error_code();
    }
    *DTable = tableLog as u16;
    nextRankStart = 0;
    for n in 1..tableLog.wrapping_add(1) {
        let current = nextRankStart;
        nextRankStart = nextRankStart
            .wrapping_add(*rankVal.as_mut_ptr().offset(n as isize) << n.wrapping_sub(1));
        *rankVal.as_mut_ptr().offset(n as isize) = current;
    }
    for n in 0..nbSymbols {
        let w = *huffWeight.as_mut_ptr().offset(n as isize) as u32;
        let length = (1 << w >> 1) as u32;
        let mut D = HUFv06_DEltX2 { byte: 0, nbBits: 0 };
        D.byte = n as u8;
        D.nbBits = tableLog.wrapping_add(1).wrapping_sub(w) as u8;
        let start = *rankVal.as_mut_ptr().offset(w as isize);
        for i in start..start.wrapping_add(length) {
            *dt.offset(i as isize) = D;
        }
        let fresh11 = &mut (*rankVal.as_mut_ptr().offset(w as isize));
        *fresh11 = (*fresh11).wrapping_add(length);
    }
    iSize
}
unsafe fn HUFv06_decodeSymbolX2(
    Dstream: *mut BITv06_DStream_t,
    dt: *const HUFv06_DEltX2,
    dtLog: u32,
) -> u8 {
    let val = BITv06_lookBitsFast(Dstream, dtLog);
    let c = (*dt.add(val)).byte;
    BITv06_skipBits(Dstream, (*dt.add(val)).nbBits as u32);
    c
}
#[inline]
unsafe fn HUFv06_decodeStreamX2(
    mut p: *mut u8,
    bitDPtr: *mut BITv06_DStream_t,
    pEnd: *mut u8,
    dt: *const HUFv06_DEltX2,
    dtLog: u32,
) -> usize {
    let pStart = p;
    while BITv06_reloadDStream(bitDPtr) == BITv06_DStream_unfinished && p <= pEnd.sub(4) {
        if MEM_64bits() {
            *p = HUFv06_decodeSymbolX2(bitDPtr, dt, dtLog);
            p = p.add(1);
        }
        if MEM_64bits() || HUFv06_MAX_TABLELOG <= 12 {
            *p = HUFv06_decodeSymbolX2(bitDPtr, dt, dtLog);
            p = p.add(1);
        }
        if MEM_64bits() {
            *p = HUFv06_decodeSymbolX2(bitDPtr, dt, dtLog);
            p = p.add(1);
        }
        *p = HUFv06_decodeSymbolX2(bitDPtr, dt, dtLog);
        p = p.add(1);
    }
    while BITv06_reloadDStream(bitDPtr) == BITv06_DStream_unfinished && p < pEnd {
        *p = HUFv06_decodeSymbolX2(bitDPtr, dt, dtLog);
        p = p.add(1);
    }
    while p < pEnd {
        *p = HUFv06_decodeSymbolX2(bitDPtr, dt, dtLog);
        p = p.add(1);
    }
    pEnd.offset_from_unsigned(pStart)
}
unsafe fn HUFv06_decompress1X2_usingDTable(
    dst: *mut core::ffi::c_void,
    dstSize: usize,
    cSrc: *const core::ffi::c_void,
    cSrcSize: usize,
    DTable: *const u16,
) -> usize {
    let op = dst as *mut u8;
    let oend = op.add(dstSize);
    let dtLog = *DTable as u32;
    let dtPtr = DTable as *const core::ffi::c_void;
    let dt = (dtPtr as *const HUFv06_DEltX2).add(1);
    let mut bitD = BITv06_DStream_t {
        bitContainer: 0,
        bitsConsumed: 0,
        ptr: core::ptr::null::<core::ffi::c_char>(),
        start: core::ptr::null::<core::ffi::c_char>(),
    };
    let errorCode = BITv06_initDStream(&mut bitD, cSrc, cSrcSize);
    if HUFv06_isError(errorCode) != 0 {
        return errorCode;
    }
    HUFv06_decodeStreamX2(op, &mut bitD, oend, dt, dtLog);
    if BITv06_endOfDStream(&bitD) == 0 {
        return Error::corruption_detected.to_error_code();
    }
    dstSize
}
unsafe fn HUFv06_decompress1X2(
    dst: *mut core::ffi::c_void,
    dstSize: usize,
    cSrc: *const core::ffi::c_void,
    mut cSrcSize: usize,
) -> usize {
    let mut DTable: [core::ffi::c_ushort; 4097] = [12; 4097];
    let mut ip = cSrc as *const u8;
    let errorCode = HUFv06_readDTableX2(DTable.as_mut_ptr(), cSrc, cSrcSize);
    if HUFv06_isError(errorCode) != 0 {
        return errorCode;
    }
    if errorCode >= cSrcSize {
        return Error::srcSize_wrong.to_error_code();
    }
    ip = ip.add(errorCode);
    cSrcSize = cSrcSize.wrapping_sub(errorCode);
    HUFv06_decompress1X2_usingDTable(
        dst,
        dstSize,
        ip as *const core::ffi::c_void,
        cSrcSize,
        DTable.as_mut_ptr(),
    )
}
unsafe fn HUFv06_decompress4X2_usingDTable(
    dst: *mut core::ffi::c_void,
    dstSize: usize,
    cSrc: *const core::ffi::c_void,
    cSrcSize: usize,
    DTable: *const u16,
) -> usize {
    if cSrcSize < 10 {
        return Error::corruption_detected.to_error_code();
    }
    let istart = cSrc as *const u8;
    let ostart = dst as *mut u8;
    let oend = ostart.add(dstSize);
    let dtPtr = DTable as *const core::ffi::c_void;
    let dt = (dtPtr as *const HUFv06_DEltX2).add(1);
    let dtLog = *DTable as u32;
    let mut errorCode: usize = 0;
    let mut bitD1 = BITv06_DStream_t {
        bitContainer: 0,
        bitsConsumed: 0,
        ptr: core::ptr::null::<core::ffi::c_char>(),
        start: core::ptr::null::<core::ffi::c_char>(),
    };
    let mut bitD2 = BITv06_DStream_t {
        bitContainer: 0,
        bitsConsumed: 0,
        ptr: core::ptr::null::<core::ffi::c_char>(),
        start: core::ptr::null::<core::ffi::c_char>(),
    };
    let mut bitD3 = BITv06_DStream_t {
        bitContainer: 0,
        bitsConsumed: 0,
        ptr: core::ptr::null::<core::ffi::c_char>(),
        start: core::ptr::null::<core::ffi::c_char>(),
    };
    let mut bitD4 = BITv06_DStream_t {
        bitContainer: 0,
        bitsConsumed: 0,
        ptr: core::ptr::null::<core::ffi::c_char>(),
        start: core::ptr::null::<core::ffi::c_char>(),
    };
    let length1 = MEM_readLE16(istart as *const core::ffi::c_void) as usize;
    let length2 = MEM_readLE16(istart.add(2) as *const core::ffi::c_void) as usize;
    let length3 = MEM_readLE16(istart.add(4) as *const core::ffi::c_void) as usize;
    let mut length4: usize = 0;
    let istart1 = istart.add(6);
    let istart2 = istart1.add(length1);
    let istart3 = istart2.add(length2);
    let istart4 = istart3.add(length3);
    let segmentSize = dstSize.wrapping_add(3) / 4;
    let opStart2 = ostart.add(segmentSize);
    let opStart3 = opStart2.add(segmentSize);
    let opStart4 = opStart3.add(segmentSize);
    let mut op1 = ostart;
    let mut op2 = opStart2;
    let mut op3 = opStart3;
    let mut op4 = opStart4;
    let mut endSignal: u32 = 0;
    length4 = cSrcSize.wrapping_sub(
        length1
            .wrapping_add(length2)
            .wrapping_add(length3)
            .wrapping_add(6),
    );
    if length4 > cSrcSize {
        return Error::corruption_detected.to_error_code();
    }
    errorCode = BITv06_initDStream(&mut bitD1, istart1 as *const core::ffi::c_void, length1);
    if HUFv06_isError(errorCode) != 0 {
        return errorCode;
    }
    errorCode = BITv06_initDStream(&mut bitD2, istart2 as *const core::ffi::c_void, length2);
    if HUFv06_isError(errorCode) != 0 {
        return errorCode;
    }
    errorCode = BITv06_initDStream(&mut bitD3, istart3 as *const core::ffi::c_void, length3);
    if HUFv06_isError(errorCode) != 0 {
        return errorCode;
    }
    errorCode = BITv06_initDStream(&mut bitD4, istart4 as *const core::ffi::c_void, length4);
    if HUFv06_isError(errorCode) != 0 {
        return errorCode;
    }
    endSignal = BITv06_reloadDStream(&mut bitD1) as core::ffi::c_uint
        | BITv06_reloadDStream(&mut bitD2) as core::ffi::c_uint
        | BITv06_reloadDStream(&mut bitD3) as core::ffi::c_uint
        | BITv06_reloadDStream(&mut bitD4) as core::ffi::c_uint;
    while endSignal == BITv06_DStream_unfinished as core::ffi::c_int as u32 && op4 < oend.sub(7) {
        if MEM_64bits() {
            *op1 = HUFv06_decodeSymbolX2(&mut bitD1, dt, dtLog);
            op1 = op1.add(1);
        }
        if MEM_64bits() {
            *op2 = HUFv06_decodeSymbolX2(&mut bitD2, dt, dtLog);
            op2 = op2.add(1);
        }
        if MEM_64bits() {
            *op3 = HUFv06_decodeSymbolX2(&mut bitD3, dt, dtLog);
            op3 = op3.add(1);
        }
        if MEM_64bits() {
            *op4 = HUFv06_decodeSymbolX2(&mut bitD4, dt, dtLog);
            op4 = op4.add(1);
        }
        if MEM_64bits() || HUFv06_MAX_TABLELOG <= 12 {
            *op1 = HUFv06_decodeSymbolX2(&mut bitD1, dt, dtLog);
            op1 = op1.add(1);
        }
        if MEM_64bits() || HUFv06_MAX_TABLELOG <= 12 {
            *op2 = HUFv06_decodeSymbolX2(&mut bitD2, dt, dtLog);
            op2 = op2.add(1);
        }
        if MEM_64bits() || HUFv06_MAX_TABLELOG <= 12 {
            *op3 = HUFv06_decodeSymbolX2(&mut bitD3, dt, dtLog);
            op3 = op3.add(1);
        }
        if MEM_64bits() || HUFv06_MAX_TABLELOG <= 12 {
            *op4 = HUFv06_decodeSymbolX2(&mut bitD4, dt, dtLog);
            op4 = op4.add(1);
        }
        if MEM_64bits() {
            *op1 = HUFv06_decodeSymbolX2(&mut bitD1, dt, dtLog);
            op1 = op1.add(1);
        }
        if MEM_64bits() {
            *op2 = HUFv06_decodeSymbolX2(&mut bitD2, dt, dtLog);
            op2 = op2.add(1);
        }
        if MEM_64bits() {
            *op3 = HUFv06_decodeSymbolX2(&mut bitD3, dt, dtLog);
            op3 = op3.add(1);
        }
        if MEM_64bits() {
            *op4 = HUFv06_decodeSymbolX2(&mut bitD4, dt, dtLog);
            op4 = op4.add(1);
        }
        *op1 = HUFv06_decodeSymbolX2(&mut bitD1, dt, dtLog);
        op1 = op1.add(1);
        *op2 = HUFv06_decodeSymbolX2(&mut bitD2, dt, dtLog);
        op2 = op2.add(1);
        *op3 = HUFv06_decodeSymbolX2(&mut bitD3, dt, dtLog);
        op3 = op3.add(1);
        *op4 = HUFv06_decodeSymbolX2(&mut bitD4, dt, dtLog);
        op4 = op4.add(1);
        endSignal = BITv06_reloadDStream(&mut bitD1) as core::ffi::c_uint
            | BITv06_reloadDStream(&mut bitD2) as core::ffi::c_uint
            | BITv06_reloadDStream(&mut bitD3) as core::ffi::c_uint
            | BITv06_reloadDStream(&mut bitD4) as core::ffi::c_uint;
    }
    if op1 > opStart2 {
        return Error::corruption_detected.to_error_code();
    }
    if op2 > opStart3 {
        return Error::corruption_detected.to_error_code();
    }
    if op3 > opStart4 {
        return Error::corruption_detected.to_error_code();
    }
    HUFv06_decodeStreamX2(op1, &mut bitD1, opStart2, dt, dtLog);
    HUFv06_decodeStreamX2(op2, &mut bitD2, opStart3, dt, dtLog);
    HUFv06_decodeStreamX2(op3, &mut bitD3, opStart4, dt, dtLog);
    HUFv06_decodeStreamX2(op4, &mut bitD4, oend, dt, dtLog);
    endSignal = BITv06_endOfDStream(&bitD1)
        & BITv06_endOfDStream(&bitD2)
        & BITv06_endOfDStream(&bitD3)
        & BITv06_endOfDStream(&bitD4);
    if endSignal == 0 {
        return Error::corruption_detected.to_error_code();
    }
    dstSize
}
unsafe fn HUFv06_decompress4X2(
    dst: *mut core::ffi::c_void,
    dstSize: usize,
    cSrc: *const core::ffi::c_void,
    mut cSrcSize: usize,
) -> usize {
    let mut DTable: [core::ffi::c_ushort; 4097] = [12; 4097];
    let mut ip = cSrc as *const u8;
    let errorCode = HUFv06_readDTableX2(DTable.as_mut_ptr(), cSrc, cSrcSize);
    if HUFv06_isError(errorCode) != 0 {
        return errorCode;
    }
    if errorCode >= cSrcSize {
        return Error::srcSize_wrong.to_error_code();
    }
    ip = ip.add(errorCode);
    cSrcSize = cSrcSize.wrapping_sub(errorCode);
    HUFv06_decompress4X2_usingDTable(
        dst,
        dstSize,
        ip as *const core::ffi::c_void,
        cSrcSize,
        DTable.as_mut_ptr(),
    )
}
unsafe fn HUFv06_fillDTableX4Level2(
    DTable: *mut HUFv06_DEltX4,
    sizeLog: u32,
    consumed: u32,
    rankValOrigin: *const u32,
    minWeight: core::ffi::c_int,
    sortedSymbols: *const sortedSymbol_t,
    sortedListSize: u32,
    nbBitsBaseline: u32,
    baseSeq: u16,
) {
    let mut DElt = HUFv06_DEltX4 {
        sequence: 0,
        nbBits: 0,
        length: 0,
    };
    let mut rankVal: [u32; 17] = [0; 17];
    memcpy(
        rankVal.as_mut_ptr() as *mut core::ffi::c_void,
        rankValOrigin as *const core::ffi::c_void,
        size_of::<[u32; 17]>(),
    );
    if minWeight > 1 {
        let skipSize = *rankVal.as_mut_ptr().offset(minWeight as isize);
        MEM_writeLE16(
            &mut DElt.sequence as *mut u16 as *mut core::ffi::c_void,
            baseSeq,
        );
        DElt.nbBits = consumed as u8;
        DElt.length = 1;
        for i in 0..skipSize {
            *DTable.offset(i as isize) = DElt;
        }
    }
    for s in 0..sortedListSize {
        let symbol = (*sortedSymbols.offset(s as isize)).symbol as u32;
        let weight = (*sortedSymbols.offset(s as isize)).weight as u32;
        let nbBits = nbBitsBaseline.wrapping_sub(weight);
        let length = (1 << sizeLog.wrapping_sub(nbBits)) as u32;
        let start = *rankVal.as_mut_ptr().offset(weight as isize);
        let end = start.wrapping_add(length);
        MEM_writeLE16(
            &mut DElt.sequence as *mut u16 as *mut core::ffi::c_void,
            (baseSeq as u32).wrapping_add(symbol << 8) as u16,
        );
        DElt.nbBits = nbBits.wrapping_add(consumed) as u8;
        DElt.length = 2;
        for i in start..end {
            *DTable.offset(i as isize) = DElt;
        }
        let fresh35 = &mut (*rankVal.as_mut_ptr().offset(weight as isize));
        *fresh35 = (*fresh35).wrapping_add(length);
    }
}
unsafe fn HUFv06_fillDTableX4(
    DTable: *mut HUFv06_DEltX4,
    targetLog: u32,
    sortedList: *const sortedSymbol_t,
    sortedListSize: u32,
    rankStart: *const u32,
    rankValOrigin: *mut [u32; 17],
    maxWeight: u32,
    nbBitsBaseline: u32,
) {
    let mut rankVal: [u32; 17] = [0; 17];
    let scaleLog = nbBitsBaseline.wrapping_sub(targetLog) as core::ffi::c_int;
    let minBits = nbBitsBaseline.wrapping_sub(maxWeight);
    memcpy(
        rankVal.as_mut_ptr() as *mut core::ffi::c_void,
        rankValOrigin as *const core::ffi::c_void,
        size_of::<[u32; 17]>(),
    );
    for s in 0..sortedListSize {
        let symbol = (*sortedList.offset(s as isize)).symbol as u16;
        let weight = (*sortedList.offset(s as isize)).weight as u32;
        let nbBits = nbBitsBaseline.wrapping_sub(weight);
        let start = *rankVal.as_mut_ptr().offset(weight as isize);
        let length = (1 << targetLog.wrapping_sub(nbBits)) as u32;
        if targetLog.wrapping_sub(nbBits) >= minBits {
            let mut sortedRank: u32 = 0;
            let minWeight = Ord::max(nbBits.wrapping_add(scaleLog as u32) as core::ffi::c_int, 1);
            sortedRank = *rankStart.offset(minWeight as isize);
            HUFv06_fillDTableX4Level2(
                DTable.offset(start as isize),
                targetLog.wrapping_sub(nbBits),
                nbBits,
                (*rankValOrigin.offset(nbBits as isize)).as_mut_ptr(),
                minWeight,
                sortedList.offset(sortedRank as isize),
                sortedListSize.wrapping_sub(sortedRank),
                nbBitsBaseline,
                symbol,
            );
        } else {
            let mut DElt = HUFv06_DEltX4 {
                sequence: 0,
                nbBits: 0,
                length: 0,
            };
            MEM_writeLE16(
                &mut DElt.sequence as *mut u16 as *mut core::ffi::c_void,
                symbol,
            );
            DElt.nbBits = nbBits as u8;
            DElt.length = 1;
            let end = start.wrapping_add(length);
            for u in start..end {
                *DTable.offset(u as isize) = DElt;
            }
        }
        let fresh36 = &mut (*rankVal.as_mut_ptr().offset(weight as isize));
        *fresh36 = (*fresh36).wrapping_add(length);
    }
}
unsafe fn HUFv06_readDTableX4(
    DTable: *mut u32,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    let mut weightList: [u8; 256] = [0; 256];
    let mut sortedSymbol: [sortedSymbol_t; 256] = [sortedSymbol_t {
        symbol: 0,
        weight: 0,
    }; 256];
    let mut rankStats: [u32; 17] = [0; 17];
    let mut rankStart0: [u32; 18] = [0; 18];
    let rankStart = rankStart0.as_mut_ptr().add(1);
    let mut rankVal: rankVal_t = [[0; 17]; 16];
    let mut tableLog: u32 = 0;
    let mut maxW: u32 = 0;
    let mut sizeOfSort: u32 = 0;
    let mut nbSymbols: u32 = 0;
    let memLog = *DTable;
    let mut iSize: usize = 0;
    let dtPtr = DTable as *mut core::ffi::c_void;
    let dt = (dtPtr as *mut HUFv06_DEltX4).add(1);
    if memLog > HUFv06_ABSOLUTEMAX_TABLELOG as u32 {
        return Error::tableLog_tooLarge.to_error_code();
    }
    iSize = HUFv06_readStats(
        weightList.as_mut_ptr(),
        (HUFv06_MAX_SYMBOL_VALUE + 1) as usize,
        rankStats.as_mut_ptr(),
        &mut nbSymbols,
        &mut tableLog,
        src,
        srcSize,
    );
    if HUFv06_isError(iSize) != 0 {
        return iSize;
    }
    if tableLog > memLog {
        return Error::tableLog_tooLarge.to_error_code();
    }
    maxW = tableLog;
    while *rankStats.as_mut_ptr().offset(maxW as isize) == 0 {
        maxW = maxW.wrapping_sub(1);
    }
    let mut nextRankStart = 0u32;
    for w in 1..maxW.wrapping_add(1) {
        let current = nextRankStart;
        nextRankStart = nextRankStart.wrapping_add(*rankStats.as_mut_ptr().offset(w as isize));
        *rankStart.offset(w as isize) = current;
    }
    *rankStart = nextRankStart;
    sizeOfSort = nextRankStart;
    for s in 0..nbSymbols {
        let w_0 = *weightList.as_mut_ptr().offset(s as isize) as u32;
        let fresh37 = &mut (*rankStart.offset(w_0 as isize));
        let r = *fresh37;
        *fresh37 = (*fresh37).wrapping_add(1);
        (*sortedSymbol.as_mut_ptr().offset(r as isize)).symbol = s as u8;
        (*sortedSymbol.as_mut_ptr().offset(r as isize)).weight = w_0 as u8;
    }
    *rankStart = 0;
    let rankVal0 = (*rankVal.as_mut_ptr()).as_mut_ptr();
    let rescale = memLog.wrapping_sub(tableLog).wrapping_sub(1) as core::ffi::c_int;
    let mut nextRankVal = 0u32;
    for w in 1..maxW.wrapping_add(1) {
        let current_0 = nextRankVal;
        nextRankVal = nextRankVal.wrapping_add(
            *rankStats.as_mut_ptr().offset(w as isize) << w.wrapping_add(rescale as u32),
        );
        *rankVal0.offset(w as isize) = current_0;
    }
    let minBits = tableLog.wrapping_add(1).wrapping_sub(maxW);
    for consumed in minBits..memLog.wrapping_sub(minBits).wrapping_add(1) {
        let rankValPtr = (*rankVal.as_mut_ptr().offset(consumed as isize)).as_mut_ptr();
        for w in 1..maxW.wrapping_add(1) {
            *rankValPtr.offset(w as isize) = *rankVal0.offset(w as isize) >> consumed;
        }
    }
    HUFv06_fillDTableX4(
        dt,
        memLog,
        sortedSymbol.as_mut_ptr(),
        sizeOfSort,
        rankStart0.as_mut_ptr(),
        rankVal.as_mut_ptr(),
        maxW,
        tableLog.wrapping_add(1),
    );
    iSize
}
unsafe fn HUFv06_decodeSymbolX4(
    op: *mut core::ffi::c_void,
    DStream: *mut BITv06_DStream_t,
    dt: *const HUFv06_DEltX4,
    dtLog: u32,
) -> u32 {
    let val = BITv06_lookBitsFast(DStream, dtLog);
    memcpy(op, dt.add(val) as *const core::ffi::c_void, 2);
    BITv06_skipBits(DStream, (*dt.add(val)).nbBits as u32);
    (*dt.add(val)).length as u32
}
unsafe fn HUFv06_decodeLastSymbolX4(
    op: *mut core::ffi::c_void,
    DStream: *mut BITv06_DStream_t,
    dt: *const HUFv06_DEltX4,
    dtLog: u32,
) -> u32 {
    let val = BITv06_lookBitsFast(DStream, dtLog);
    memcpy(op, dt.add(val) as *const core::ffi::c_void, 1);
    if (*dt.add(val)).length as core::ffi::c_int == 1 {
        BITv06_skipBits(DStream, (*dt.add(val)).nbBits as u32);
    } else if (*DStream).bitsConsumed < usize::BITS {
        BITv06_skipBits(DStream, (*dt.add(val)).nbBits as u32);
        (*DStream).bitsConsumed = Ord::min((*DStream).bitsConsumed, usize::BITS);
    }
    1
}
#[inline]
unsafe fn HUFv06_decodeStreamX4(
    mut p: *mut u8,
    bitDPtr: *mut BITv06_DStream_t,
    pEnd: *mut u8,
    dt: *const HUFv06_DEltX4,
    dtLog: u32,
) -> usize {
    let pStart = p;
    while BITv06_reloadDStream(bitDPtr) == BITv06_DStream_unfinished && p < pEnd.sub(7) {
        if MEM_64bits() {
            p = p.offset(
                HUFv06_decodeSymbolX4(p as *mut core::ffi::c_void, bitDPtr, dt, dtLog) as isize,
            );
        }
        if MEM_64bits() || HUFv06_MAX_TABLELOG <= 12 {
            p = p.offset(
                HUFv06_decodeSymbolX4(p as *mut core::ffi::c_void, bitDPtr, dt, dtLog) as isize,
            );
        }
        if MEM_64bits() {
            p = p.offset(
                HUFv06_decodeSymbolX4(p as *mut core::ffi::c_void, bitDPtr, dt, dtLog) as isize,
            );
        }
        p = p.offset(
            HUFv06_decodeSymbolX4(p as *mut core::ffi::c_void, bitDPtr, dt, dtLog) as isize,
        );
    }
    while BITv06_reloadDStream(bitDPtr) == BITv06_DStream_unfinished && p <= pEnd.sub(2) {
        p = p.offset(
            HUFv06_decodeSymbolX4(p as *mut core::ffi::c_void, bitDPtr, dt, dtLog) as isize,
        );
    }
    while p <= pEnd.sub(2) {
        p = p.offset(
            HUFv06_decodeSymbolX4(p as *mut core::ffi::c_void, bitDPtr, dt, dtLog) as isize,
        );
    }
    if p < pEnd {
        p = p.offset(
            HUFv06_decodeLastSymbolX4(p as *mut core::ffi::c_void, bitDPtr, dt, dtLog) as isize,
        );
    }
    p.offset_from_unsigned(pStart)
}
unsafe fn HUFv06_decompress1X4_usingDTable(
    dst: *mut core::ffi::c_void,
    dstSize: usize,
    cSrc: *const core::ffi::c_void,
    cSrcSize: usize,
    DTable: *const u32,
) -> usize {
    let istart = cSrc as *const u8;
    let ostart = dst as *mut u8;
    let oend = ostart.add(dstSize);
    let dtLog = *DTable;
    let dtPtr = DTable as *const core::ffi::c_void;
    let dt = (dtPtr as *const HUFv06_DEltX4).add(1);
    let mut bitD = BITv06_DStream_t {
        bitContainer: 0,
        bitsConsumed: 0,
        ptr: core::ptr::null::<core::ffi::c_char>(),
        start: core::ptr::null::<core::ffi::c_char>(),
    };
    let errorCode = BITv06_initDStream(&mut bitD, istart as *const core::ffi::c_void, cSrcSize);
    if HUFv06_isError(errorCode) != 0 {
        return errorCode;
    }
    HUFv06_decodeStreamX4(ostart, &mut bitD, oend, dt, dtLog);
    if BITv06_endOfDStream(&bitD) == 0 {
        return Error::corruption_detected.to_error_code();
    }
    dstSize
}
unsafe fn HUFv06_decompress4X4_usingDTable(
    dst: *mut core::ffi::c_void,
    dstSize: usize,
    cSrc: *const core::ffi::c_void,
    cSrcSize: usize,
    DTable: *const u32,
) -> usize {
    if cSrcSize < 10 {
        return Error::corruption_detected.to_error_code();
    }
    let istart = cSrc as *const u8;
    let ostart = dst as *mut u8;
    let oend = ostart.add(dstSize);
    let dtPtr = DTable as *const core::ffi::c_void;
    let dt = (dtPtr as *const HUFv06_DEltX4).add(1);
    let dtLog = *DTable;
    let mut errorCode: usize = 0;
    let mut bitD1 = BITv06_DStream_t {
        bitContainer: 0,
        bitsConsumed: 0,
        ptr: core::ptr::null::<core::ffi::c_char>(),
        start: core::ptr::null::<core::ffi::c_char>(),
    };
    let mut bitD2 = BITv06_DStream_t {
        bitContainer: 0,
        bitsConsumed: 0,
        ptr: core::ptr::null::<core::ffi::c_char>(),
        start: core::ptr::null::<core::ffi::c_char>(),
    };
    let mut bitD3 = BITv06_DStream_t {
        bitContainer: 0,
        bitsConsumed: 0,
        ptr: core::ptr::null::<core::ffi::c_char>(),
        start: core::ptr::null::<core::ffi::c_char>(),
    };
    let mut bitD4 = BITv06_DStream_t {
        bitContainer: 0,
        bitsConsumed: 0,
        ptr: core::ptr::null::<core::ffi::c_char>(),
        start: core::ptr::null::<core::ffi::c_char>(),
    };
    let length1 = MEM_readLE16(istart as *const core::ffi::c_void) as usize;
    let length2 = MEM_readLE16(istart.add(2) as *const core::ffi::c_void) as usize;
    let length3 = MEM_readLE16(istart.add(4) as *const core::ffi::c_void) as usize;
    let mut length4: usize = 0;
    let istart1 = istart.add(6);
    let istart2 = istart1.add(length1);
    let istart3 = istart2.add(length2);
    let istart4 = istart3.add(length3);
    let segmentSize = dstSize.wrapping_add(3) / 4;
    let opStart2 = ostart.add(segmentSize);
    let opStart3 = opStart2.add(segmentSize);
    let opStart4 = opStart3.add(segmentSize);
    let mut op1 = ostart;
    let mut op2 = opStart2;
    let mut op3 = opStart3;
    let mut op4 = opStart4;
    let mut endSignal: u32 = 0;
    length4 = cSrcSize.wrapping_sub(
        length1
            .wrapping_add(length2)
            .wrapping_add(length3)
            .wrapping_add(6),
    );
    if length4 > cSrcSize {
        return Error::corruption_detected.to_error_code();
    }
    errorCode = BITv06_initDStream(&mut bitD1, istart1 as *const core::ffi::c_void, length1);
    if HUFv06_isError(errorCode) != 0 {
        return errorCode;
    }
    errorCode = BITv06_initDStream(&mut bitD2, istart2 as *const core::ffi::c_void, length2);
    if HUFv06_isError(errorCode) != 0 {
        return errorCode;
    }
    errorCode = BITv06_initDStream(&mut bitD3, istart3 as *const core::ffi::c_void, length3);
    if HUFv06_isError(errorCode) != 0 {
        return errorCode;
    }
    errorCode = BITv06_initDStream(&mut bitD4, istart4 as *const core::ffi::c_void, length4);
    if HUFv06_isError(errorCode) != 0 {
        return errorCode;
    }
    endSignal = BITv06_reloadDStream(&mut bitD1) as core::ffi::c_uint
        | BITv06_reloadDStream(&mut bitD2) as core::ffi::c_uint
        | BITv06_reloadDStream(&mut bitD3) as core::ffi::c_uint
        | BITv06_reloadDStream(&mut bitD4) as core::ffi::c_uint;
    while endSignal == BITv06_DStream_unfinished as core::ffi::c_int as u32 && op4 < oend.sub(7) {
        if MEM_64bits() {
            op1 = op1.offset(HUFv06_decodeSymbolX4(
                op1 as *mut core::ffi::c_void,
                &mut bitD1,
                dt,
                dtLog,
            ) as isize);
        }
        if MEM_64bits() {
            op2 = op2.offset(HUFv06_decodeSymbolX4(
                op2 as *mut core::ffi::c_void,
                &mut bitD2,
                dt,
                dtLog,
            ) as isize);
        }
        if MEM_64bits() {
            op3 = op3.offset(HUFv06_decodeSymbolX4(
                op3 as *mut core::ffi::c_void,
                &mut bitD3,
                dt,
                dtLog,
            ) as isize);
        }
        if MEM_64bits() {
            op4 = op4.offset(HUFv06_decodeSymbolX4(
                op4 as *mut core::ffi::c_void,
                &mut bitD4,
                dt,
                dtLog,
            ) as isize);
        }
        if MEM_64bits() || HUFv06_MAX_TABLELOG <= 12 {
            op1 = op1.offset(HUFv06_decodeSymbolX4(
                op1 as *mut core::ffi::c_void,
                &mut bitD1,
                dt,
                dtLog,
            ) as isize);
        }
        if MEM_64bits() || HUFv06_MAX_TABLELOG <= 12 {
            op2 = op2.offset(HUFv06_decodeSymbolX4(
                op2 as *mut core::ffi::c_void,
                &mut bitD2,
                dt,
                dtLog,
            ) as isize);
        }
        if MEM_64bits() || HUFv06_MAX_TABLELOG <= 12 {
            op3 = op3.offset(HUFv06_decodeSymbolX4(
                op3 as *mut core::ffi::c_void,
                &mut bitD3,
                dt,
                dtLog,
            ) as isize);
        }
        if MEM_64bits() || HUFv06_MAX_TABLELOG <= 12 {
            op4 = op4.offset(HUFv06_decodeSymbolX4(
                op4 as *mut core::ffi::c_void,
                &mut bitD4,
                dt,
                dtLog,
            ) as isize);
        }
        if MEM_64bits() {
            op1 = op1.offset(HUFv06_decodeSymbolX4(
                op1 as *mut core::ffi::c_void,
                &mut bitD1,
                dt,
                dtLog,
            ) as isize);
        }
        if MEM_64bits() {
            op2 = op2.offset(HUFv06_decodeSymbolX4(
                op2 as *mut core::ffi::c_void,
                &mut bitD2,
                dt,
                dtLog,
            ) as isize);
        }
        if MEM_64bits() {
            op3 = op3.offset(HUFv06_decodeSymbolX4(
                op3 as *mut core::ffi::c_void,
                &mut bitD3,
                dt,
                dtLog,
            ) as isize);
        }
        if MEM_64bits() {
            op4 = op4.offset(HUFv06_decodeSymbolX4(
                op4 as *mut core::ffi::c_void,
                &mut bitD4,
                dt,
                dtLog,
            ) as isize);
        }
        op1 = op1.offset(
            HUFv06_decodeSymbolX4(op1 as *mut core::ffi::c_void, &mut bitD1, dt, dtLog) as isize,
        );
        op2 = op2.offset(
            HUFv06_decodeSymbolX4(op2 as *mut core::ffi::c_void, &mut bitD2, dt, dtLog) as isize,
        );
        op3 = op3.offset(
            HUFv06_decodeSymbolX4(op3 as *mut core::ffi::c_void, &mut bitD3, dt, dtLog) as isize,
        );
        op4 = op4.offset(
            HUFv06_decodeSymbolX4(op4 as *mut core::ffi::c_void, &mut bitD4, dt, dtLog) as isize,
        );
        endSignal = BITv06_reloadDStream(&mut bitD1) as core::ffi::c_uint
            | BITv06_reloadDStream(&mut bitD2) as core::ffi::c_uint
            | BITv06_reloadDStream(&mut bitD3) as core::ffi::c_uint
            | BITv06_reloadDStream(&mut bitD4) as core::ffi::c_uint;
    }
    if op1 > opStart2 {
        return Error::corruption_detected.to_error_code();
    }
    if op2 > opStart3 {
        return Error::corruption_detected.to_error_code();
    }
    if op3 > opStart4 {
        return Error::corruption_detected.to_error_code();
    }
    HUFv06_decodeStreamX4(op1, &mut bitD1, opStart2, dt, dtLog);
    HUFv06_decodeStreamX4(op2, &mut bitD2, opStart3, dt, dtLog);
    HUFv06_decodeStreamX4(op3, &mut bitD3, opStart4, dt, dtLog);
    HUFv06_decodeStreamX4(op4, &mut bitD4, oend, dt, dtLog);
    endSignal = BITv06_endOfDStream(&bitD1)
        & BITv06_endOfDStream(&bitD2)
        & BITv06_endOfDStream(&bitD3)
        & BITv06_endOfDStream(&bitD4);
    if endSignal == 0 {
        return Error::corruption_detected.to_error_code();
    }
    dstSize
}
unsafe fn HUFv06_decompress4X4(
    dst: *mut core::ffi::c_void,
    dstSize: usize,
    cSrc: *const core::ffi::c_void,
    mut cSrcSize: usize,
) -> usize {
    let mut DTable: [core::ffi::c_uint; 4097] = [12; 4097];
    let mut ip = cSrc as *const u8;
    let hSize = HUFv06_readDTableX4(DTable.as_mut_ptr(), cSrc, cSrcSize);
    if HUFv06_isError(hSize) != 0 {
        return hSize;
    }
    if hSize >= cSrcSize {
        return Error::srcSize_wrong.to_error_code();
    }
    ip = ip.add(hSize);
    cSrcSize = cSrcSize.wrapping_sub(hSize);
    HUFv06_decompress4X4_usingDTable(
        dst,
        dstSize,
        ip as *const core::ffi::c_void,
        cSrcSize,
        DTable.as_mut_ptr(),
    )
}
static algoTime: [[algo_time_t; 3]; 16] = [
    [
        {
            algo_time_t {
                tableTime: 0,
                decode256Time: 0,
            }
        },
        {
            algo_time_t {
                tableTime: 1,
                decode256Time: 1,
            }
        },
        {
            algo_time_t {
                tableTime: 2,
                decode256Time: 2,
            }
        },
    ],
    [
        {
            algo_time_t {
                tableTime: 0,
                decode256Time: 0,
            }
        },
        {
            algo_time_t {
                tableTime: 1,
                decode256Time: 1,
            }
        },
        {
            algo_time_t {
                tableTime: 2,
                decode256Time: 2,
            }
        },
    ],
    [
        {
            algo_time_t {
                tableTime: 38,
                decode256Time: 130,
            }
        },
        {
            algo_time_t {
                tableTime: 1313,
                decode256Time: 74,
            }
        },
        {
            algo_time_t {
                tableTime: 2151,
                decode256Time: 38,
            }
        },
    ],
    [
        {
            algo_time_t {
                tableTime: 448,
                decode256Time: 128,
            }
        },
        {
            algo_time_t {
                tableTime: 1353,
                decode256Time: 74,
            }
        },
        {
            algo_time_t {
                tableTime: 2238,
                decode256Time: 41,
            }
        },
    ],
    [
        {
            algo_time_t {
                tableTime: 556,
                decode256Time: 128,
            }
        },
        {
            algo_time_t {
                tableTime: 1353,
                decode256Time: 74,
            }
        },
        {
            algo_time_t {
                tableTime: 2238,
                decode256Time: 47,
            }
        },
    ],
    [
        {
            algo_time_t {
                tableTime: 714,
                decode256Time: 128,
            }
        },
        {
            algo_time_t {
                tableTime: 1418,
                decode256Time: 74,
            }
        },
        {
            algo_time_t {
                tableTime: 2436,
                decode256Time: 53,
            }
        },
    ],
    [
        {
            algo_time_t {
                tableTime: 883,
                decode256Time: 128,
            }
        },
        {
            algo_time_t {
                tableTime: 1437,
                decode256Time: 74,
            }
        },
        {
            algo_time_t {
                tableTime: 2464,
                decode256Time: 61,
            }
        },
    ],
    [
        {
            algo_time_t {
                tableTime: 897,
                decode256Time: 128,
            }
        },
        {
            algo_time_t {
                tableTime: 1515,
                decode256Time: 75,
            }
        },
        {
            algo_time_t {
                tableTime: 2622,
                decode256Time: 68,
            }
        },
    ],
    [
        {
            algo_time_t {
                tableTime: 926,
                decode256Time: 128,
            }
        },
        {
            algo_time_t {
                tableTime: 1613,
                decode256Time: 75,
            }
        },
        {
            algo_time_t {
                tableTime: 2730,
                decode256Time: 75,
            }
        },
    ],
    [
        {
            algo_time_t {
                tableTime: 947,
                decode256Time: 128,
            }
        },
        {
            algo_time_t {
                tableTime: 1729,
                decode256Time: 77,
            }
        },
        {
            algo_time_t {
                tableTime: 3359,
                decode256Time: 77,
            }
        },
    ],
    [
        {
            algo_time_t {
                tableTime: 1107,
                decode256Time: 128,
            }
        },
        {
            algo_time_t {
                tableTime: 2083,
                decode256Time: 81,
            }
        },
        {
            algo_time_t {
                tableTime: 4006,
                decode256Time: 84,
            }
        },
    ],
    [
        {
            algo_time_t {
                tableTime: 1177,
                decode256Time: 128,
            }
        },
        {
            algo_time_t {
                tableTime: 2379,
                decode256Time: 87,
            }
        },
        {
            algo_time_t {
                tableTime: 4785,
                decode256Time: 88,
            }
        },
    ],
    [
        {
            algo_time_t {
                tableTime: 1242,
                decode256Time: 128,
            }
        },
        {
            algo_time_t {
                tableTime: 2415,
                decode256Time: 93,
            }
        },
        {
            algo_time_t {
                tableTime: 5155,
                decode256Time: 84,
            }
        },
    ],
    [
        {
            algo_time_t {
                tableTime: 1349,
                decode256Time: 128,
            }
        },
        {
            algo_time_t {
                tableTime: 2644,
                decode256Time: 106,
            }
        },
        {
            algo_time_t {
                tableTime: 5260,
                decode256Time: 106,
            }
        },
    ],
    [
        {
            algo_time_t {
                tableTime: 1455,
                decode256Time: 128,
            }
        },
        {
            algo_time_t {
                tableTime: 2422,
                decode256Time: 124,
            }
        },
        {
            algo_time_t {
                tableTime: 4174,
                decode256Time: 124,
            }
        },
    ],
    [
        {
            algo_time_t {
                tableTime: 722,
                decode256Time: 128,
            }
        },
        {
            algo_time_t {
                tableTime: 1891,
                decode256Time: 145,
            }
        },
        {
            algo_time_t {
                tableTime: 1936,
                decode256Time: 146,
            }
        },
    ],
];
unsafe fn HUFv06_decompress(
    dst: *mut core::ffi::c_void,
    dstSize: usize,
    cSrc: *const core::ffi::c_void,
    cSrcSize: usize,
) -> usize {
    static decompress: [decompressionAlgo; 3] = [
        Some(
            HUFv06_decompress4X2
                as unsafe fn(
                    *mut core::ffi::c_void,
                    usize,
                    *const core::ffi::c_void,
                    usize,
                ) -> usize,
        ),
        Some(
            HUFv06_decompress4X4
                as unsafe fn(
                    *mut core::ffi::c_void,
                    usize,
                    *const core::ffi::c_void,
                    usize,
                ) -> usize,
        ),
        None,
    ];
    let mut Dtime: [u32; 3] = [0; 3];
    if dstSize == 0 {
        return Error::dstSize_tooSmall.to_error_code();
    }
    if cSrcSize > dstSize {
        return Error::corruption_detected.to_error_code();
    }
    if cSrcSize == dstSize {
        memcpy(dst, cSrc, dstSize);
        return dstSize;
    }
    if cSrcSize == 1 {
        core::ptr::write_bytes(dst.cast::<u8>(), *(cSrc as *const u8), dstSize);
        return dstSize;
    }
    let Q = (cSrcSize * 16 / dstSize) as u32;
    let D256 = (dstSize >> 8) as u32;
    for n in 0..3 {
        *Dtime.as_mut_ptr().offset(n as isize) = ((*(*algoTime.as_ptr().offset(Q as isize))
            .as_ptr()
            .offset(n as isize))
        .tableTime)
            .wrapping_add(
                (*(*algoTime.as_ptr().offset(Q as isize))
                    .as_ptr()
                    .offset(n as isize))
                .decode256Time
                    * D256,
            );
    }
    let fresh39 = &mut (*Dtime.as_mut_ptr().add(1));
    *fresh39 = (*fresh39).wrapping_add(*Dtime.as_mut_ptr().add(1) >> 4);
    let fresh40 = &mut (*Dtime.as_mut_ptr().add(2));
    *fresh40 = (*fresh40).wrapping_add(*Dtime.as_mut_ptr().add(2) >> 3);
    let mut algoNb = 0;
    if *Dtime.as_mut_ptr().add(1) < *Dtime.as_mut_ptr() {
        algoNb = 1;
    }
    (*decompress.as_ptr().offset(algoNb as isize)).unwrap_unchecked()(dst, dstSize, cSrc, cSrcSize)
}
unsafe fn ZSTDv06_copy4(dst: *mut core::ffi::c_void, src: *const core::ffi::c_void) {
    memcpy(dst, src, 4);
}
unsafe fn ZSTDv06_decompressBegin(dctx: *mut ZSTDv06_DCtx) -> usize {
    (*dctx).expected = ZSTDv06_frameHeaderSize_min;
    (*dctx).stage = ZSTDds_getFrameHeaderSize;
    (*dctx).previousDstEnd = core::ptr::null();
    (*dctx).base = core::ptr::null();
    (*dctx).vBase = core::ptr::null();
    (*dctx).dictEnd = core::ptr::null();
    *((*dctx).hufTableX4).as_mut_ptr() = ZSTD_HUFFDTABLE_CAPACITY_LOG as core::ffi::c_uint;
    (*dctx).flagRepeatTable = 0;
    0
}
pub(crate) unsafe fn ZSTDv06_createDCtx() -> *mut ZSTDv06_DCtx {
    let dctx = malloc(size_of::<ZSTDv06_DCtx>()) as *mut ZSTDv06_DCtx;
    if dctx.is_null() {
        return core::ptr::null_mut();
    }
    ZSTDv06_decompressBegin(dctx);
    dctx
}
pub(crate) unsafe fn ZSTDv06_freeDCtx(dctx: *mut ZSTDv06_DCtx) -> usize {
    free(dctx as *mut core::ffi::c_void);
    0
}
unsafe fn ZSTDv06_frameHeaderSize(src: *const core::ffi::c_void, srcSize: usize) -> usize {
    if srcSize < ZSTDv06_frameHeaderSize_min {
        return Error::srcSize_wrong.to_error_code();
    }
    let fcsId = (*(src as *const u8).add(4) as core::ffi::c_int >> 6) as u32;
    ZSTDv06_frameHeaderSize_min.wrapping_add(*ZSTDv06_fcs_fieldSize.as_ptr().offset(fcsId as isize))
}

/// [`ZSTDv06_getFrameParams`] :
/// decode Frame Header, or provide expected `srcSize`.
/// @return : 0, `fparamsPtr` is correctly filled,
///          >0, `srcSize` is too small, result is expected `srcSize`,
///           or an error code, which can be tested using [`ZSTDv06_isError`]
pub(crate) unsafe fn ZSTDv06_getFrameParams(
    fparamsPtr: *mut ZSTDv06_frameParams,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    let ip = src as *const u8;

    if srcSize < ZSTDv06_frameHeaderSize_min {
        return ZSTDv06_frameHeaderSize_min;
    }
    if MEM_readLE32(src) != ZSTDv06_MAGICNUMBER {
        return Error::prefix_unknown.to_error_code();
    }

    // ensure there is enough `srcSize` to fully read/decode frame header
    let fhsize = ZSTDv06_frameHeaderSize(src, srcSize);
    if srcSize < fhsize {
        return fhsize;
    }

    ptr::write_bytes(fparamsPtr as *mut u8, 0, size_of::<ZSTDv06_frameParams>());

    let frameDesc = *ip.add(4);
    (*fparamsPtr).windowLog =
        (frameDesc & 0xf) as core::ffi::c_uint + ZSTDv06_WINDOWLOG_ABSOLUTEMIN as core::ffi::c_uint;
    if frameDesc & 0x20 != 0 {
        // reserved 1 bit
        return Error::frameParameter_unsupported.to_error_code();
    }
    match frameDesc as core::ffi::c_int >> 6 {
        // fcsId
        0 => (*fparamsPtr).frameContentSize = 0,
        1 => (*fparamsPtr).frameContentSize = *ip.add(5) as core::ffi::c_ulonglong,
        2 => {
            (*fparamsPtr).frameContentSize =
                MEM_readLE16(ip.add(5) as *const core::ffi::c_void) as core::ffi::c_ulonglong + 256;
        }
        3 => {
            (*fparamsPtr).frameContentSize =
                MEM_readLE64(ip.add(5) as *const core::ffi::c_void) as core::ffi::c_ulonglong;
        }
        _ => unreachable!(),
    }

    0
}
unsafe fn ZSTDv06_decodeFrameHeader(
    zc: *mut ZSTDv06_DCtx,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    let result = ZSTDv06_getFrameParams(&mut (*zc).fParams, src, srcSize);
    if MEM_32bits() && (*zc).fParams.windowLog > 25 {
        return Error::frameParameter_unsupported.to_error_code();
    }
    result
}
unsafe fn ZSTDv06_getcBlockSize(
    src: *const core::ffi::c_void,
    srcSize: usize,
    bpPtr: *mut blockProperties_t,
) -> usize {
    let in_0 = src as *const u8;
    let mut cSize: u32 = 0;
    if srcSize < ZSTDv06_blockHeaderSize {
        return Error::srcSize_wrong.to_error_code();
    }
    (*bpPtr).blockType = (*in_0 as core::ffi::c_int >> 6) as blockType_t;
    cSize = (*in_0.add(2) as core::ffi::c_int
        + ((*in_0.add(1) as core::ffi::c_int) << 8)
        + ((*in_0 as core::ffi::c_int & 7) << 16)) as u32;
    (*bpPtr).origSize = if (*bpPtr).blockType == bt_rle {
        cSize
    } else {
        0
    };
    if (*bpPtr).blockType == bt_end {
        return 0;
    }
    if (*bpPtr).blockType == bt_rle {
        return 1;
    }
    cSize as usize
}
unsafe fn ZSTDv06_copyRawBlock(
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    if dst.is_null() {
        return Error::dstSize_tooSmall.to_error_code();
    }
    if srcSize > dstCapacity {
        return Error::dstSize_tooSmall.to_error_code();
    }
    memcpy(dst, src, srcSize);
    srcSize
}
unsafe fn ZSTDv06_decodeLiteralsBlock(
    dctx: *mut ZSTDv06_DCtx,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    let istart = src as *const u8;
    if srcSize < MIN_CBLOCK_SIZE as usize {
        return Error::corruption_detected.to_error_code();
    }
    match *istart as core::ffi::c_int >> 6 {
        IS_HUF => {
            let mut litSize: usize = 0;
            let mut litCSize: usize = 0;
            let mut singleStream = 0;
            let mut lhSize = (*istart as core::ffi::c_int >> 4 & 3) as u32;
            if srcSize < 5 {
                return Error::corruption_detected.to_error_code();
            }
            match lhSize {
                2 => {
                    lhSize = 4;
                    litSize = (((*istart as core::ffi::c_int & 15) << 10)
                        + ((*istart.add(1) as core::ffi::c_int) << 2)
                        + (*istart.add(2) as core::ffi::c_int >> 6))
                        as usize;
                    litCSize = (((*istart.add(2) as core::ffi::c_int & 63) << 8)
                        + *istart.add(3) as core::ffi::c_int)
                        as usize;
                }
                3 => {
                    lhSize = 5;
                    litSize = (((*istart as core::ffi::c_int & 15) << 14)
                        + ((*istart.add(1) as core::ffi::c_int) << 6)
                        + (*istart.add(2) as core::ffi::c_int >> 2))
                        as usize;
                    litCSize = (((*istart.add(2) as core::ffi::c_int & 3) << 16)
                        + ((*istart.add(3) as core::ffi::c_int) << 8)
                        + *istart.add(4) as core::ffi::c_int)
                        as usize;
                }
                0 | 1 => {
                    lhSize = 3;
                    singleStream = (*istart as core::ffi::c_int & 16) as usize;
                    litSize = (((*istart as core::ffi::c_int & 15) << 6)
                        + (*istart.add(1) as core::ffi::c_int >> 2))
                        as usize;
                    litCSize = (((*istart.add(1) as core::ffi::c_int & 3) << 8)
                        + *istart.add(2) as core::ffi::c_int)
                        as usize;
                }
                _ => unreachable!(),
            }
            if litSize > ZSTDv06_BLOCKSIZE_MAX as usize {
                return Error::corruption_detected.to_error_code();
            }
            if litCSize.wrapping_add(lhSize as usize) > srcSize {
                return Error::corruption_detected.to_error_code();
            }
            if ERR_isError(if singleStream != 0 {
                HUFv06_decompress1X2(
                    ((*dctx).litBuffer).as_mut_ptr() as *mut core::ffi::c_void,
                    litSize,
                    istart.offset(lhSize as isize) as *const core::ffi::c_void,
                    litCSize,
                )
            } else {
                HUFv06_decompress(
                    ((*dctx).litBuffer).as_mut_ptr() as *mut core::ffi::c_void,
                    litSize,
                    istart.offset(lhSize as isize) as *const core::ffi::c_void,
                    litCSize,
                )
            }) {
                return Error::corruption_detected.to_error_code();
            }
            (*dctx).litPtr = (&raw mut (*dctx).litBuffer).cast();
            (*dctx).litSize = litSize;
            ptr::write_bytes(
                ((*dctx).litBuffer).as_mut_ptr().add((*dctx).litSize),
                0,
                WILDCOPY_OVERLENGTH as usize,
            );
            litCSize.wrapping_add(lhSize as usize)
        }
        IS_PCH => {
            let mut litSize_0: usize = 0;
            let mut litCSize_0: usize = 0;
            let mut lhSize_0 = (*istart as core::ffi::c_int >> 4 & 3) as u32;
            if lhSize_0 != 1 {
                return Error::corruption_detected.to_error_code();
            }
            if (*dctx).flagRepeatTable == 0 {
                return Error::dictionary_corrupted.to_error_code();
            }
            lhSize_0 = 3;
            litSize_0 = (((*istart as core::ffi::c_int & 15) << 6)
                + (*istart.add(1) as core::ffi::c_int >> 2)) as usize;
            litCSize_0 = (((*istart.add(1) as core::ffi::c_int & 3) << 8)
                + *istart.add(2) as core::ffi::c_int) as usize;
            if litCSize_0.wrapping_add(lhSize_0 as usize) > srcSize {
                return Error::corruption_detected.to_error_code();
            }
            let errorCode = HUFv06_decompress1X4_usingDTable(
                ((*dctx).litBuffer).as_mut_ptr() as *mut core::ffi::c_void,
                litSize_0,
                istart.offset(lhSize_0 as isize) as *const core::ffi::c_void,
                litCSize_0,
                ((*dctx).hufTableX4).as_mut_ptr(),
            );
            if ERR_isError(errorCode) {
                return Error::corruption_detected.to_error_code();
            }
            (*dctx).litPtr = ((*dctx).litBuffer).as_mut_ptr();
            (*dctx).litSize = litSize_0;
            ptr::write_bytes(
                ((*dctx).litBuffer).as_mut_ptr().add((*dctx).litSize),
                0,
                WILDCOPY_OVERLENGTH as usize,
            );
            litCSize_0.wrapping_add(lhSize_0 as usize)
        }
        IS_RAW => {
            let mut litSize_1: usize = 0;
            let mut lhSize_1 = (*istart as core::ffi::c_int >> 4 & 3) as u32;
            match lhSize_1 {
                2 => {
                    litSize_1 = (((*istart as core::ffi::c_int & 15) << 8)
                        + *istart.add(1) as core::ffi::c_int)
                        as usize;
                }
                3 => {
                    litSize_1 = (((*istart as core::ffi::c_int & 15) << 16)
                        + ((*istart.add(1) as core::ffi::c_int) << 8)
                        + *istart.add(2) as core::ffi::c_int)
                        as usize;
                }
                0 | 1 => {
                    lhSize_1 = 1;
                    litSize_1 = (*istart as core::ffi::c_int & 31) as usize;
                }
                _ => unreachable!(),
            }
            if (lhSize_1 as usize)
                .wrapping_add(litSize_1)
                .wrapping_add(WILDCOPY_OVERLENGTH as usize)
                > srcSize
            {
                if litSize_1.wrapping_add(lhSize_1 as usize) > srcSize {
                    return Error::corruption_detected.to_error_code();
                }
                memcpy(
                    ((*dctx).litBuffer).as_mut_ptr() as *mut core::ffi::c_void,
                    istart.offset(lhSize_1 as isize) as *const core::ffi::c_void,
                    litSize_1,
                );
                (*dctx).litPtr = ((*dctx).litBuffer).as_mut_ptr();
                (*dctx).litSize = litSize_1;
                ptr::write_bytes(
                    ((*dctx).litBuffer).as_mut_ptr().add((*dctx).litSize),
                    0,
                    WILDCOPY_OVERLENGTH as usize,
                );
                return (lhSize_1 as usize).wrapping_add(litSize_1);
            }
            (*dctx).litPtr = istart.offset(lhSize_1 as isize);
            (*dctx).litSize = litSize_1;
            (lhSize_1 as usize).wrapping_add(litSize_1)
        }
        IS_RLE => {
            let mut litSize_2: usize = 0;
            let mut lhSize_2 = (*istart as core::ffi::c_int >> 4 & 3) as u32;
            match lhSize_2 {
                2 => {
                    litSize_2 = (((*istart as core::ffi::c_int & 15) << 8)
                        + *istart.add(1) as core::ffi::c_int)
                        as usize;
                }
                3 => {
                    litSize_2 = (((*istart as core::ffi::c_int & 15) << 16)
                        + ((*istart.add(1) as core::ffi::c_int) << 8)
                        + *istart.add(2) as core::ffi::c_int)
                        as usize;
                    if srcSize < 4 {
                        return Error::corruption_detected.to_error_code();
                    }
                }
                0 | 1 => {
                    lhSize_2 = 1;
                    litSize_2 = (*istart as core::ffi::c_int & 31) as usize;
                }
                _ => unreachable!(),
            }
            if litSize_2 > ZSTDv06_BLOCKSIZE_MAX as usize {
                return Error::corruption_detected.to_error_code();
            }
            core::ptr::write_bytes(
                ((*dctx).litBuffer).as_mut_ptr(),
                *istart.offset(lhSize_2 as isize),
                litSize_2.wrapping_add(WILDCOPY_OVERLENGTH as usize),
            );
            (*dctx).litPtr = ((*dctx).litBuffer).as_mut_ptr();
            (*dctx).litSize = litSize_2;
            lhSize_2.wrapping_add(1) as usize
        }
        _ => Error::corruption_detected.to_error_code(),
    }
}
unsafe fn ZSTDv06_buildSeqTable(
    DTable: *mut FSEv06_DTable,
    type_0: u32,
    mut max: u32,
    maxLog: u32,
    src: *const core::ffi::c_void,
    srcSize: usize,
    defaultNorm: *const i16,
    defaultLog: u32,
    flagRepeatTable: u32,
) -> usize {
    match type_0 {
        1 => {
            if srcSize == 0 {
                return Error::srcSize_wrong.to_error_code();
            }
            if *(src as *const u8) as u32 > max {
                return Error::corruption_detected.to_error_code();
            }
            FSEv06_buildDTable_rle(DTable, *(src as *const u8));
            1
        }
        0 => {
            FSEv06_buildDTable(DTable, defaultNorm, max, defaultLog);
            0
        }
        2 => {
            if flagRepeatTable == 0 {
                return Error::corruption_detected.to_error_code();
            }
            0
        }
        3 => {
            let mut tableLog: u32 = 0;
            let mut norm: [i16; 53] = [0; 53];
            let headerSize =
                FSEv06_readNCount(norm.as_mut_ptr(), &mut max, &mut tableLog, src, srcSize);
            if ERR_isError(headerSize) {
                return Error::corruption_detected.to_error_code();
            }
            if tableLog > maxLog {
                return Error::corruption_detected.to_error_code();
            }
            FSEv06_buildDTable(DTable, norm.as_mut_ptr(), max, tableLog);
            headerSize
        }
        _ => unreachable!(),
    }
}
unsafe fn ZSTDv06_decodeSeqHeaders(
    nbSeqPtr: *mut core::ffi::c_int,
    DTableLL: *mut FSEv06_DTable,
    DTableML: *mut FSEv06_DTable,
    DTableOffb: *mut FSEv06_DTable,
    flagRepeatTable: u32,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    let istart = src as *const u8;
    let iend = istart.add(srcSize);
    let mut ip = istart;
    if srcSize < MIN_SEQUENCES_SIZE as usize {
        return Error::srcSize_wrong.to_error_code();
    }
    let mut nbSeq = *ip as core::ffi::c_int;
    ip = ip.add(1);
    if nbSeq == 0 {
        *nbSeqPtr = 0;
        return 1;
    }
    if nbSeq > 0x7f as core::ffi::c_int {
        if nbSeq == 0xff as core::ffi::c_int {
            if ip.add(2) > iend {
                return Error::srcSize_wrong.to_error_code();
            }
            nbSeq = MEM_readLE16(ip as *const core::ffi::c_void) as core::ffi::c_int + LONGNBSEQ;
            ip = ip.add(2);
        } else {
            if ip >= iend {
                return Error::srcSize_wrong.to_error_code();
            }
            nbSeq = ((nbSeq - 0x80 as core::ffi::c_int) << 8) + *ip as core::ffi::c_int;
            ip = ip.add(1);
        }
    }
    *nbSeqPtr = nbSeq;
    if ip.add(4) > iend {
        return Error::srcSize_wrong.to_error_code();
    }
    let LLtype = (*ip as core::ffi::c_int >> 6) as u32;
    let Offtype = (*ip as core::ffi::c_int >> 4 & 3) as u32;
    let MLtype = (*ip as core::ffi::c_int >> 2 & 3) as u32;
    ip = ip.add(1);
    let bhSize = ZSTDv06_buildSeqTable(
        DTableLL,
        LLtype,
        MaxLL as u32,
        LLFSELog as u32,
        ip as *const core::ffi::c_void,
        iend.offset_from_unsigned(ip),
        LL_defaultNorm.as_ptr(),
        LL_defaultNormLog,
        flagRepeatTable,
    );
    if ERR_isError(bhSize) {
        return Error::corruption_detected.to_error_code();
    }
    ip = ip.add(bhSize);
    let bhSize_0 = ZSTDv06_buildSeqTable(
        DTableOffb,
        Offtype,
        MaxOff as u32,
        OffFSELog as u32,
        ip as *const core::ffi::c_void,
        iend.offset_from_unsigned(ip),
        OF_defaultNorm.as_ptr(),
        OF_defaultNormLog,
        flagRepeatTable,
    );
    if ERR_isError(bhSize_0) {
        return Error::corruption_detected.to_error_code();
    }
    ip = ip.add(bhSize_0);
    let bhSize_1 = ZSTDv06_buildSeqTable(
        DTableML,
        MLtype,
        MaxML as u32,
        MLFSELog as u32,
        ip as *const core::ffi::c_void,
        iend.offset_from_unsigned(ip),
        ML_defaultNorm.as_ptr(),
        ML_defaultNormLog,
        flagRepeatTable,
    );
    if ERR_isError(bhSize_1) {
        return Error::corruption_detected.to_error_code();
    }
    ip = ip.add(bhSize_1);
    ip.offset_from_unsigned(istart)
}
unsafe fn ZSTDv06_decodeSequence(seq: *mut seq_t, seqState: *mut seqState_t) {
    let llCode = FSEv06_peekSymbol(&(*seqState).stateLL) as u32;
    let mlCode = FSEv06_peekSymbol(&(*seqState).stateML) as u32;
    let ofCode = FSEv06_peekSymbol(&(*seqState).stateOffb) as u32;
    let llBits = *LL_bits.as_ptr().offset(llCode as isize);
    let mlBits = *ML_bits.as_ptr().offset(mlCode as isize);
    let ofBits = ofCode;
    let totalBits = llBits.wrapping_add(mlBits).wrapping_add(ofBits);
    static LL_base: [u32; 36] = [
        0,
        1,
        2,
        3,
        4,
        5,
        6,
        7,
        8,
        9,
        10,
        11,
        12,
        13,
        14,
        15,
        16,
        18,
        20,
        22,
        24,
        28,
        32,
        40,
        48,
        64,
        0x80 as core::ffi::c_int as u32,
        0x100 as core::ffi::c_int as u32,
        0x200 as core::ffi::c_int as u32,
        0x400 as core::ffi::c_int as u32,
        0x800 as core::ffi::c_int as u32,
        0x1000 as core::ffi::c_int as u32,
        0x2000 as core::ffi::c_int as u32,
        0x4000 as core::ffi::c_int as u32,
        0x8000 as core::ffi::c_int as u32,
        0x10000 as core::ffi::c_int as u32,
    ];
    static ML_base: [u32; 53] = [
        0,
        1,
        2,
        3,
        4,
        5,
        6,
        7,
        8,
        9,
        10,
        11,
        12,
        13,
        14,
        15,
        16,
        17,
        18,
        19,
        20,
        21,
        22,
        23,
        24,
        25,
        26,
        27,
        28,
        29,
        30,
        31,
        32,
        34,
        36,
        38,
        40,
        44,
        48,
        56,
        64,
        80,
        96,
        0x80 as core::ffi::c_int as u32,
        0x100 as core::ffi::c_int as u32,
        0x200 as core::ffi::c_int as u32,
        0x400 as core::ffi::c_int as u32,
        0x800 as core::ffi::c_int as u32,
        0x1000 as core::ffi::c_int as u32,
        0x2000 as core::ffi::c_int as u32,
        0x4000 as core::ffi::c_int as u32,
        0x8000 as core::ffi::c_int as u32,
        0x10000 as core::ffi::c_int as u32,
    ];
    static OF_base: [u32; 29] = [
        0,
        1,
        3,
        7,
        0xf as core::ffi::c_int as u32,
        0x1f as core::ffi::c_int as u32,
        0x3f as core::ffi::c_int as u32,
        0x7f as core::ffi::c_int as u32,
        0xff as core::ffi::c_int as u32,
        0x1ff as core::ffi::c_int as u32,
        0x3ff as core::ffi::c_int as u32,
        0x7ff as core::ffi::c_int as u32,
        0xfff as core::ffi::c_int as u32,
        0x1fff as core::ffi::c_int as u32,
        0x3fff as core::ffi::c_int as u32,
        0x7fff as core::ffi::c_int as u32,
        0xffff as core::ffi::c_int as u32,
        0x1ffff as core::ffi::c_int as u32,
        0x3ffff as core::ffi::c_int as u32,
        0x7ffff as core::ffi::c_int as u32,
        0xfffff as core::ffi::c_int as u32,
        0x1fffff as core::ffi::c_int as u32,
        0x3fffff as core::ffi::c_int as u32,
        0x7fffff as core::ffi::c_int as u32,
        0xffffff as core::ffi::c_int as u32,
        0x1ffffff as core::ffi::c_int as u32,
        0x3ffffff as core::ffi::c_int as u32,
        1,
        1,
    ];
    let mut offset: usize = 0;
    if ofCode == 0 {
        offset = 0;
    } else {
        offset = (*OF_base.as_ptr().offset(ofCode as isize) as usize)
            .wrapping_add(BITv06_readBits(&mut (*seqState).DStream, ofBits));
        if MEM_32bits() {
            BITv06_reloadDStream(&mut (*seqState).DStream);
        }
    }
    if offset < ZSTDv06_REP_NUM as usize {
        if llCode == 0 && offset <= 1 {
            offset = 1usize.wrapping_sub(offset);
        }
        if offset != 0 {
            let temp = *((*seqState).prevOffset).as_mut_ptr().add(offset);
            if offset != 1 {
                *((*seqState).prevOffset).as_mut_ptr().add(2) =
                    *((*seqState).prevOffset).as_mut_ptr().add(1);
            }
            *((*seqState).prevOffset).as_mut_ptr().add(1) = *((*seqState).prevOffset).as_mut_ptr();
            offset = temp;
            *((*seqState).prevOffset).as_mut_ptr() = offset;
        } else {
            offset = *((*seqState).prevOffset).as_mut_ptr();
        }
    } else {
        offset = offset.wrapping_sub(ZSTDv06_REP_MOVE as usize);
        *((*seqState).prevOffset).as_mut_ptr().add(2) =
            *((*seqState).prevOffset).as_mut_ptr().add(1);
        *((*seqState).prevOffset).as_mut_ptr().add(1) = *((*seqState).prevOffset).as_mut_ptr();
        *((*seqState).prevOffset).as_mut_ptr() = offset;
    }
    (*seq).offset = offset;
    (*seq).matchLength = ((*ML_base.as_ptr().offset(mlCode as isize)).wrapping_add(MINMATCH as u32)
        as usize)
        .wrapping_add(if mlCode > 31 {
            BITv06_readBits(&mut (*seqState).DStream, mlBits)
        } else {
            0
        });
    if MEM_32bits() && mlBits.wrapping_add(llBits) > 24 {
        BITv06_reloadDStream(&mut (*seqState).DStream);
    }
    (*seq).litLength =
        (*LL_base.as_ptr().offset(llCode as isize) as usize).wrapping_add(if llCode > 15 {
            BITv06_readBits(&mut (*seqState).DStream, llBits)
        } else {
            0
        });
    if MEM_32bits() || totalBits > (64 - 7 - (LLFSELog + MLFSELog + OffFSELog)) as u32 {
        BITv06_reloadDStream(&mut (*seqState).DStream);
    }
    FSEv06_updateState(&mut (*seqState).stateLL, &mut (*seqState).DStream);
    FSEv06_updateState(&mut (*seqState).stateML, &mut (*seqState).DStream);
    if MEM_32bits() {
        BITv06_reloadDStream(&mut (*seqState).DStream);
    }
    FSEv06_updateState(&mut (*seqState).stateOffb, &mut (*seqState).DStream);
}
unsafe fn ZSTDv06_execSequence(
    mut op: *mut u8,
    oend: *mut u8,
    mut sequence: seq_t,
    litPtr: *mut *const u8,
    litLimit: *const u8,
    base: *const u8,
    vBase: *const u8,
    dictEnd: *const u8,
) -> usize {
    let oLitEnd = op.add(sequence.litLength);
    let sequenceLength = sequence.litLength.wrapping_add(sequence.matchLength);
    let oMatchEnd = op.add(sequenceLength);
    let oend_8 = oend.wrapping_sub(8);
    let iLitEnd = (*litPtr).add(sequence.litLength);
    let mut match_0: *const u8 = oLitEnd.wrapping_sub(sequence.offset);
    let seqLength = sequence.litLength.wrapping_add(sequence.matchLength);
    if seqLength > oend.offset_from_unsigned(op) {
        return Error::dstSize_tooSmall.to_error_code();
    }
    if sequence.litLength > litLimit.offset_from_unsigned(*litPtr) {
        return Error::corruption_detected.to_error_code();
    }
    if oLitEnd > oend_8 {
        return Error::dstSize_tooSmall.to_error_code();
    }
    if oMatchEnd > oend {
        return Error::dstSize_tooSmall.to_error_code();
    }
    if iLitEnd > litLimit {
        return Error::corruption_detected.to_error_code();
    }
    ZSTDv06_wildcopy(
        op as *mut core::ffi::c_void,
        *litPtr as *const core::ffi::c_void,
        sequence.litLength as ptrdiff_t,
    );
    op = oLitEnd;
    *litPtr = iLitEnd;
    if sequence.offset > oLitEnd.offset_from_unsigned(base) {
        if sequence.offset > oLitEnd.offset_from_unsigned(vBase) {
            return Error::corruption_detected.to_error_code();
        }
        match_0 = dictEnd.offset(-(base.offset_from(match_0) as core::ffi::c_long as isize));
        if match_0.add(sequence.matchLength) <= dictEnd {
            core::ptr::copy(match_0, oLitEnd, sequence.matchLength);
            return sequenceLength;
        }
        let length1 = dictEnd.offset_from_unsigned(match_0);
        core::ptr::copy(match_0, oLitEnd, length1);
        op = oLitEnd.add(length1);
        sequence.matchLength = sequence.matchLength.wrapping_sub(length1);
        match_0 = base;
        if op > oend_8 || sequence.matchLength < MINMATCH as usize {
            while op < oMatchEnd {
                *op = *match_0;
                op = op.add(1);
                match_0 = match_0.add(1);
            }
            return sequenceLength;
        }
    }
    if sequence.offset < 8 {
        static dec32table: [u32; 8] = [0, 1, 2, 1, 4, 4, 4, 4];
        static dec64table: [core::ffi::c_int; 8] = [8, 8, 8, 7, 8, 9, 10, 11];
        let sub2 = *dec64table.as_ptr().add(sequence.offset);
        *op = *match_0;
        *op.add(1) = *match_0.add(1);
        *op.add(2) = *match_0.add(2);
        *op.add(3) = *match_0.add(3);
        match_0 = match_0.offset(*dec32table.as_ptr().add(sequence.offset) as isize);
        ZSTDv06_copy4(
            op.add(4) as *mut core::ffi::c_void,
            match_0 as *const core::ffi::c_void,
        );
        match_0 = match_0.offset(-(sub2 as isize));
    } else {
        ZSTDv06_copy8(
            op as *mut core::ffi::c_void,
            match_0 as *const core::ffi::c_void,
        );
    }
    op = op.add(8);
    match_0 = match_0.add(8);
    if oMatchEnd > oend.offset(-((16 - MINMATCH) as isize)) {
        if op < oend_8 {
            ZSTDv06_wildcopy(
                op as *mut core::ffi::c_void,
                match_0 as *const core::ffi::c_void,
                oend_8.offset_from(op) as ptrdiff_t,
            );
            match_0 = match_0.add(oend_8.offset_from_unsigned(op));
            op = oend_8;
        }
        while op < oMatchEnd {
            *op = *match_0;
            op = op.add(1);
            match_0 = match_0.add(1);
        }
    } else {
        ZSTDv06_wildcopy(
            op as *mut core::ffi::c_void,
            match_0 as *const core::ffi::c_void,
            sequence.matchLength as ptrdiff_t - 8,
        );
    }
    sequenceLength
}
unsafe fn ZSTDv06_decompressSequences(
    dctx: *mut ZSTDv06_DCtx,
    dst: *mut core::ffi::c_void,
    maxDstSize: usize,
    seqStart: *const core::ffi::c_void,
    seqSize: usize,
) -> usize {
    let mut ip = seqStart as *const u8;
    let iend = ip.add(seqSize);
    let ostart = dst as *mut u8;
    let oend = ostart.add(maxDstSize);
    let mut op = ostart;
    let mut litPtr = (*dctx).litPtr;
    let litEnd = litPtr.add((*dctx).litSize);
    let DTableLL = ((*dctx).LLTable).as_mut_ptr();
    let DTableML = ((*dctx).MLTable).as_mut_ptr();
    let DTableOffb = ((*dctx).OffTable).as_mut_ptr();
    let base = (*dctx).base as *const u8;
    let vBase = (*dctx).vBase as *const u8;
    let dictEnd = (*dctx).dictEnd as *const u8;
    let mut nbSeq: core::ffi::c_int = 0;
    let seqHSize = ZSTDv06_decodeSeqHeaders(
        &mut nbSeq,
        DTableLL,
        DTableML,
        DTableOffb,
        (*dctx).flagRepeatTable,
        ip as *const core::ffi::c_void,
        seqSize,
    );
    if ERR_isError(seqHSize) {
        return seqHSize;
    }
    ip = ip.add(seqHSize);
    (*dctx).flagRepeatTable = 0;
    if nbSeq != 0 {
        let mut sequence = seq_t {
            litLength: 0,
            matchLength: 0,
            offset: 0,
        };
        let mut seqState = seqState_t {
            DStream: BITv06_DStream_t {
                bitContainer: 0,
                bitsConsumed: 0,
                ptr: core::ptr::null::<core::ffi::c_char>(),
                start: core::ptr::null::<core::ffi::c_char>(),
            },
            stateLL: FSEv06_DState_t {
                state: 0,
                table: core::ptr::null::<core::ffi::c_void>(),
            },
            stateOffb: FSEv06_DState_t {
                state: 0,
                table: core::ptr::null::<core::ffi::c_void>(),
            },
            stateML: FSEv06_DState_t {
                state: 0,
                table: core::ptr::null::<core::ffi::c_void>(),
            },
            prevOffset: [0; 3],
        };
        ptr::write_bytes(
            &mut sequence as *mut seq_t as *mut u8,
            0,
            size_of::<seq_t>(),
        );
        sequence.offset = REPCODE_STARTVALUE as usize;
        seqState.prevOffset[..ZSTDv06_REP_INIT as usize].fill(REPCODE_STARTVALUE as usize);
        let errorCode = BITv06_initDStream(
            &mut seqState.DStream,
            ip as *const core::ffi::c_void,
            iend.offset_from_unsigned(ip),
        );
        if ERR_isError(errorCode) {
            return Error::corruption_detected.to_error_code();
        }
        FSEv06_initDState(&mut seqState.stateLL, &mut seqState.DStream, DTableLL);
        FSEv06_initDState(&mut seqState.stateOffb, &mut seqState.DStream, DTableOffb);
        FSEv06_initDState(&mut seqState.stateML, &mut seqState.DStream, DTableML);
        while BITv06_reloadDStream(&mut seqState.DStream) <= BITv06_DStream_completed && nbSeq != 0
        {
            nbSeq -= 1;
            ZSTDv06_decodeSequence(&mut sequence, &mut seqState);
            let oneSeqSize = ZSTDv06_execSequence(
                op,
                oend,
                sequence,
                &mut litPtr,
                litEnd,
                base,
                vBase,
                dictEnd,
            );
            if ERR_isError(oneSeqSize) {
                return oneSeqSize;
            }
            op = op.add(oneSeqSize);
        }
        if nbSeq != 0 {
            return Error::corruption_detected.to_error_code();
        }
    }
    let lastLLSize = litEnd.offset_from_unsigned(litPtr);
    if litPtr > litEnd {
        return Error::corruption_detected.to_error_code();
    }
    if op.add(lastLLSize) > oend {
        return Error::dstSize_tooSmall.to_error_code();
    }
    if lastLLSize > 0 {
        memcpy(
            op as *mut core::ffi::c_void,
            litPtr as *const core::ffi::c_void,
            lastLLSize,
        );
        op = op.add(lastLLSize);
    }
    op.offset_from_unsigned(ostart)
}
unsafe fn ZSTDv06_checkContinuity(dctx: *mut ZSTDv06_DCtx, dst: *const core::ffi::c_void) {
    if dst != (*dctx).previousDstEnd {
        (*dctx).dictEnd = (*dctx).previousDstEnd;
        (*dctx).vBase = (dst as *const core::ffi::c_char).offset(
            -(((*dctx).previousDstEnd as *const core::ffi::c_char)
                .offset_from((*dctx).base as *const core::ffi::c_char)
                as core::ffi::c_long as isize),
        ) as *const core::ffi::c_void;
        (*dctx).base = dst;
        (*dctx).previousDstEnd = dst;
    }
}
unsafe fn ZSTDv06_decompressBlock_internal(
    dctx: *mut ZSTDv06_DCtx,
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    mut srcSize: usize,
) -> usize {
    let mut ip = src as *const u8;
    if srcSize >= ZSTDv06_BLOCKSIZE_MAX as usize {
        return Error::srcSize_wrong.to_error_code();
    }
    let litCSize = ZSTDv06_decodeLiteralsBlock(dctx, src, srcSize);
    if ERR_isError(litCSize) {
        return litCSize;
    }
    ip = ip.add(litCSize);
    srcSize = srcSize.wrapping_sub(litCSize);
    ZSTDv06_decompressSequences(
        dctx,
        dst,
        dstCapacity,
        ip as *const core::ffi::c_void,
        srcSize,
    )
}
unsafe fn ZSTDv06_decompressFrame(
    dctx: *mut ZSTDv06_DCtx,
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    let mut ip = src as *const u8;
    let iend = ip.add(srcSize);
    let ostart = dst as *mut u8;
    let mut op = ostart;
    let oend = ostart.wrapping_add(dstCapacity);
    let mut remainingSize = srcSize;
    let mut blockProperties = {
        blockProperties_t {
            blockType: bt_compressed,
            origSize: 0,
        }
    };
    if srcSize < ZSTDv06_frameHeaderSize_min.wrapping_add(ZSTDv06_blockHeaderSize) {
        return Error::srcSize_wrong.to_error_code();
    }
    let frameHeaderSize = ZSTDv06_frameHeaderSize(src, ZSTDv06_frameHeaderSize_min);
    if ERR_isError(frameHeaderSize) {
        return frameHeaderSize;
    }
    if srcSize < frameHeaderSize.wrapping_add(ZSTDv06_blockHeaderSize) {
        return Error::srcSize_wrong.to_error_code();
    }
    if ZSTDv06_decodeFrameHeader(dctx, src, frameHeaderSize) != 0 {
        return Error::corruption_detected.to_error_code();
    }
    ip = ip.add(frameHeaderSize);
    remainingSize = remainingSize.wrapping_sub(frameHeaderSize);
    loop {
        let mut decodedSize = 0;
        let cBlockSize = ZSTDv06_getcBlockSize(
            ip as *const core::ffi::c_void,
            iend.offset_from_unsigned(ip),
            &mut blockProperties,
        );
        if ERR_isError(cBlockSize) {
            return cBlockSize;
        }
        ip = ip.add(ZSTDv06_blockHeaderSize);
        remainingSize = remainingSize.wrapping_sub(ZSTDv06_blockHeaderSize);
        if cBlockSize > remainingSize {
            return Error::srcSize_wrong.to_error_code();
        }
        match blockProperties.blockType as core::ffi::c_uint {
            0 => {
                decodedSize = ZSTDv06_decompressBlock_internal(
                    dctx,
                    op as *mut core::ffi::c_void,
                    oend.offset_from_unsigned(op),
                    ip as *const core::ffi::c_void,
                    cBlockSize,
                );
            }
            1 => {
                decodedSize = ZSTDv06_copyRawBlock(
                    op as *mut core::ffi::c_void,
                    oend.offset_from_unsigned(op),
                    ip as *const core::ffi::c_void,
                    cBlockSize,
                );
            }
            2 => return Error::GENERIC.to_error_code(),
            3 => {
                if remainingSize != 0 {
                    return Error::srcSize_wrong.to_error_code();
                }
            }
            _ => return Error::GENERIC.to_error_code(),
        }
        if cBlockSize == 0 {
            break;
        }
        if ERR_isError(decodedSize) {
            return decodedSize;
        }
        op = op.add(decodedSize);
        ip = ip.add(cBlockSize);
        remainingSize = remainingSize.wrapping_sub(cBlockSize);
    }
    op.offset_from_unsigned(ostart)
}
pub(crate) unsafe fn ZSTDv06_decompress_usingDict(
    dctx: *mut ZSTDv06_DCtx,
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
    dict: *const core::ffi::c_void,
    dictSize: usize,
) -> usize {
    ZSTDv06_decompressBegin_usingDict(dctx, dict, dictSize);
    ZSTDv06_checkContinuity(dctx, dst);
    ZSTDv06_decompressFrame(dctx, dst, dstCapacity, src, srcSize)
}
unsafe fn ZSTD_errorFrameSizeInfoLegacy(
    cSize: *mut usize,
    dBound: *mut core::ffi::c_ulonglong,
    ret: usize,
) {
    *cSize = ret;
    *dBound = ZSTD_CONTENTSIZE_ERROR;
}
pub(crate) unsafe fn ZSTDv06_findFrameSizeInfoLegacy(
    src: *const core::ffi::c_void,
    srcSize: usize,
    cSize: *mut usize,
    dBound: *mut core::ffi::c_ulonglong,
) {
    let mut ip = src as *const u8;
    let mut remainingSize = srcSize;
    let mut nbBlocks = 0usize;
    let mut blockProperties = {
        blockProperties_t {
            blockType: bt_compressed,
            origSize: 0,
        }
    };
    let frameHeaderSize = ZSTDv06_frameHeaderSize(src, srcSize);
    if ERR_isError(frameHeaderSize) {
        ZSTD_errorFrameSizeInfoLegacy(cSize, dBound, frameHeaderSize);
        return;
    }
    if MEM_readLE32(src) != ZSTDv06_MAGICNUMBER {
        ZSTD_errorFrameSizeInfoLegacy(cSize, dBound, Error::prefix_unknown.to_error_code());
        return;
    }
    if srcSize < frameHeaderSize.wrapping_add(ZSTDv06_blockHeaderSize) {
        ZSTD_errorFrameSizeInfoLegacy(cSize, dBound, Error::srcSize_wrong.to_error_code());
        return;
    }
    ip = ip.add(frameHeaderSize);
    remainingSize = remainingSize.wrapping_sub(frameHeaderSize);
    loop {
        let cBlockSize = ZSTDv06_getcBlockSize(
            ip as *const core::ffi::c_void,
            remainingSize,
            &mut blockProperties,
        );
        if ERR_isError(cBlockSize) {
            ZSTD_errorFrameSizeInfoLegacy(cSize, dBound, cBlockSize);
            return;
        }
        ip = ip.add(ZSTDv06_blockHeaderSize);
        remainingSize = remainingSize.wrapping_sub(ZSTDv06_blockHeaderSize);
        if cBlockSize > remainingSize {
            ZSTD_errorFrameSizeInfoLegacy(cSize, dBound, Error::srcSize_wrong.to_error_code());
            return;
        }
        if cBlockSize == 0 {
            break;
        }
        ip = ip.add(cBlockSize);
        remainingSize = remainingSize.wrapping_sub(cBlockSize);
        nbBlocks = nbBlocks.wrapping_add(1);
    }
    *cSize = ip.offset_from_unsigned(src as *const u8);
    *dBound = (nbBlocks * ZSTDv06_BLOCKSIZE_MAX as usize) as core::ffi::c_ulonglong;
}
unsafe fn ZSTDv06_nextSrcSizeToDecompress(dctx: *mut ZSTDv06_DCtx) -> usize {
    (*dctx).expected
}
unsafe fn ZSTDv06_decompressContinue(
    dctx: *mut ZSTDv06_DCtx,
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    if srcSize != (*dctx).expected {
        return Error::srcSize_wrong.to_error_code();
    }
    if dstCapacity != 0 {
        ZSTDv06_checkContinuity(dctx, dst);
    }
    match (*dctx).stage as core::ffi::c_uint {
        0 => {
            if srcSize != ZSTDv06_frameHeaderSize_min {
                return Error::srcSize_wrong.to_error_code();
            }
            (*dctx).headerSize = ZSTDv06_frameHeaderSize(src, ZSTDv06_frameHeaderSize_min);
            if ERR_isError((*dctx).headerSize) {
                return (*dctx).headerSize;
            }
            memcpy(
                ((*dctx).headerBuffer).as_mut_ptr() as *mut core::ffi::c_void,
                src,
                ZSTDv06_frameHeaderSize_min,
            );
            if (*dctx).headerSize > ZSTDv06_frameHeaderSize_min {
                (*dctx).expected = ((*dctx).headerSize).wrapping_sub(ZSTDv06_frameHeaderSize_min);
                (*dctx).stage = ZSTDds_decodeFrameHeader;
                return 0;
            }
            (*dctx).expected = 0;
        }
        1 => {}
        2 => {
            let mut bp = blockProperties_t {
                blockType: bt_compressed,
                origSize: 0,
            };
            let cBlockSize = ZSTDv06_getcBlockSize(src, ZSTDv06_blockHeaderSize, &mut bp);
            if ERR_isError(cBlockSize) {
                return cBlockSize;
            }
            if bp.blockType == bt_end {
                (*dctx).expected = 0;
                (*dctx).stage = ZSTDds_getFrameHeaderSize;
            } else {
                (*dctx).expected = cBlockSize;
                (*dctx).bType = bp.blockType;
                (*dctx).stage = ZSTDds_decompressBlock;
            }
            return 0;
        }
        3 => {
            let mut rSize: usize = 0;
            match (*dctx).bType as core::ffi::c_uint {
                0 => {
                    rSize = ZSTDv06_decompressBlock_internal(dctx, dst, dstCapacity, src, srcSize);
                }
                1 => {
                    rSize = ZSTDv06_copyRawBlock(dst, dstCapacity, src, srcSize);
                }
                2 => return Error::GENERIC.to_error_code(),
                3 => {
                    rSize = 0;
                }
                _ => return Error::GENERIC.to_error_code(),
            }
            (*dctx).stage = ZSTDds_decodeBlockHeader;
            (*dctx).expected = ZSTDv06_blockHeaderSize;
            if ERR_isError(rSize) {
                return rSize;
            }
            (*dctx).previousDstEnd =
                (dst as *mut core::ffi::c_char).add(rSize) as *const core::ffi::c_void;
            return rSize;
        }
        _ => return Error::GENERIC.to_error_code(),
    }
    let mut result: usize = 0;
    memcpy(
        ((*dctx).headerBuffer)
            .as_mut_ptr()
            .add(ZSTDv06_frameHeaderSize_min) as *mut core::ffi::c_void,
        src,
        (*dctx).expected,
    );
    result = ZSTDv06_decodeFrameHeader(
        dctx,
        ((*dctx).headerBuffer).as_mut_ptr() as *const core::ffi::c_void,
        (*dctx).headerSize,
    );
    if ERR_isError(result) {
        return result;
    }
    (*dctx).expected = ZSTDv06_blockHeaderSize;
    (*dctx).stage = ZSTDds_decodeBlockHeader;
    0
}
unsafe fn ZSTDv06_refDictContent(
    dctx: *mut ZSTDv06_DCtx,
    dict: *const core::ffi::c_void,
    dictSize: usize,
) {
    (*dctx).dictEnd = (*dctx).previousDstEnd;
    (*dctx).vBase = (dict as *const core::ffi::c_char).offset(
        -(((*dctx).previousDstEnd as *const core::ffi::c_char)
            .offset_from((*dctx).base as *const core::ffi::c_char) as core::ffi::c_long
            as isize),
    ) as *const core::ffi::c_void;
    (*dctx).base = dict;
    (*dctx).previousDstEnd =
        (dict as *const core::ffi::c_char).add(dictSize) as *const core::ffi::c_void;
}
unsafe fn ZSTDv06_loadEntropy(
    dctx: *mut ZSTDv06_DCtx,
    mut dict: *const core::ffi::c_void,
    mut dictSize: usize,
) -> usize {
    let mut hSize: usize = 0;
    let mut offcodeHeaderSize: usize = 0;
    let mut matchlengthHeaderSize: usize = 0;
    let mut litlengthHeaderSize: usize = 0;
    hSize = HUFv06_readDTableX4(((*dctx).hufTableX4).as_mut_ptr(), dict, dictSize);
    if ERR_isError(hSize) {
        return Error::dictionary_corrupted.to_error_code();
    }
    dict = (dict as *const core::ffi::c_char).add(hSize) as *const core::ffi::c_void;
    dictSize = dictSize.wrapping_sub(hSize);
    let mut offcodeNCount: [core::ffi::c_short; 29] = [0; 29];
    let mut offcodeMaxValue = MaxOff as u32;
    let mut offcodeLog: u32 = 0;
    offcodeHeaderSize = FSEv06_readNCount(
        offcodeNCount.as_mut_ptr(),
        &mut offcodeMaxValue,
        &mut offcodeLog,
        dict,
        dictSize,
    );
    if ERR_isError(offcodeHeaderSize) {
        return Error::dictionary_corrupted.to_error_code();
    }
    if offcodeLog > OffFSELog as u32 {
        return Error::dictionary_corrupted.to_error_code();
    }
    let errorCode = FSEv06_buildDTable(
        ((*dctx).OffTable).as_mut_ptr(),
        offcodeNCount.as_mut_ptr(),
        offcodeMaxValue,
        offcodeLog,
    );
    if ERR_isError(errorCode) {
        return Error::dictionary_corrupted.to_error_code();
    }
    dict = (dict as *const core::ffi::c_char).add(offcodeHeaderSize) as *const core::ffi::c_void;
    dictSize = dictSize.wrapping_sub(offcodeHeaderSize);
    let mut matchlengthNCount: [core::ffi::c_short; 53] = [0; 53];
    let mut matchlengthMaxValue = MaxML as core::ffi::c_uint;
    let mut matchlengthLog: core::ffi::c_uint = 0;
    matchlengthHeaderSize = FSEv06_readNCount(
        matchlengthNCount.as_mut_ptr(),
        &mut matchlengthMaxValue,
        &mut matchlengthLog,
        dict,
        dictSize,
    );
    if ERR_isError(matchlengthHeaderSize) {
        return Error::dictionary_corrupted.to_error_code();
    }
    if matchlengthLog > MLFSELog as core::ffi::c_uint {
        return Error::dictionary_corrupted.to_error_code();
    }
    let errorCode_0 = FSEv06_buildDTable(
        ((*dctx).MLTable).as_mut_ptr(),
        matchlengthNCount.as_mut_ptr(),
        matchlengthMaxValue,
        matchlengthLog,
    );
    if ERR_isError(errorCode_0) {
        return Error::dictionary_corrupted.to_error_code();
    }
    dict =
        (dict as *const core::ffi::c_char).add(matchlengthHeaderSize) as *const core::ffi::c_void;
    dictSize = dictSize.wrapping_sub(matchlengthHeaderSize);
    let mut litlengthNCount: [core::ffi::c_short; 36] = [0; 36];
    let mut litlengthMaxValue = MaxLL as core::ffi::c_uint;
    let mut litlengthLog: core::ffi::c_uint = 0;
    litlengthHeaderSize = FSEv06_readNCount(
        litlengthNCount.as_mut_ptr(),
        &mut litlengthMaxValue,
        &mut litlengthLog,
        dict,
        dictSize,
    );
    if ERR_isError(litlengthHeaderSize) {
        return Error::dictionary_corrupted.to_error_code();
    }
    if litlengthLog > LLFSELog as core::ffi::c_uint {
        return Error::dictionary_corrupted.to_error_code();
    }
    let errorCode_1 = FSEv06_buildDTable(
        ((*dctx).LLTable).as_mut_ptr(),
        litlengthNCount.as_mut_ptr(),
        litlengthMaxValue,
        litlengthLog,
    );
    if ERR_isError(errorCode_1) {
        return Error::dictionary_corrupted.to_error_code();
    }
    (*dctx).flagRepeatTable = 1;
    hSize
        .wrapping_add(offcodeHeaderSize)
        .wrapping_add(matchlengthHeaderSize)
        .wrapping_add(litlengthHeaderSize)
}
unsafe fn ZSTDv06_decompress_insertDictionary(
    dctx: *mut ZSTDv06_DCtx,
    mut dict: *const core::ffi::c_void,
    mut dictSize: usize,
) -> usize {
    let mut eSize: usize = 0;
    let magic = MEM_readLE32(dict);
    if magic != ZSTDv06_DICT_MAGIC {
        ZSTDv06_refDictContent(dctx, dict, dictSize);
        return 0;
    }
    dict = (dict as *const core::ffi::c_char).add(4) as *const core::ffi::c_void;
    dictSize = dictSize.wrapping_sub(4);
    eSize = ZSTDv06_loadEntropy(dctx, dict, dictSize);
    if ERR_isError(eSize) {
        return Error::dictionary_corrupted.to_error_code();
    }
    dict = (dict as *const core::ffi::c_char).add(eSize) as *const core::ffi::c_void;
    dictSize = dictSize.wrapping_sub(eSize);
    ZSTDv06_refDictContent(dctx, dict, dictSize);
    0
}
unsafe fn ZSTDv06_decompressBegin_usingDict(
    dctx: *mut ZSTDv06_DCtx,
    dict: *const core::ffi::c_void,
    dictSize: usize,
) -> usize {
    let errorCode = ZSTDv06_decompressBegin(dctx);
    if ERR_isError(errorCode) {
        return errorCode;
    }
    if !dict.is_null() && dictSize != 0 {
        let errorCode_0 = ZSTDv06_decompress_insertDictionary(dctx, dict, dictSize);
        if ERR_isError(errorCode_0) {
            return Error::dictionary_corrupted.to_error_code();
        }
    }
    0
}
pub(crate) unsafe fn ZBUFFv06_createDCtx() -> *mut ZBUFFv06_DCtx {
    let zbd = malloc(size_of::<ZBUFFv06_DCtx>()) as *mut ZBUFFv06_DCtx;
    if zbd.is_null() {
        return core::ptr::null_mut();
    }
    ptr::write_bytes(zbd as *mut u8, 0, size_of::<ZBUFFv06_DCtx>());
    (*zbd).zd = ZSTDv06_createDCtx();
    if ((*zbd).zd).is_null() {
        ZBUFFv06_freeDCtx(zbd);
        return core::ptr::null_mut();
    }
    (*zbd).stage = ZBUFFds_init;
    zbd
}
pub(crate) unsafe fn ZBUFFv06_freeDCtx(zbd: *mut ZBUFFv06_DCtx) -> usize {
    if zbd.is_null() {
        return 0;
    }
    ZSTDv06_freeDCtx((*zbd).zd);
    free((*zbd).inBuff as *mut core::ffi::c_void);
    free((*zbd).outBuff as *mut core::ffi::c_void);
    free(zbd as *mut core::ffi::c_void);
    0
}
pub(crate) unsafe fn ZBUFFv06_decompressInitDictionary(
    zbd: *mut ZBUFFv06_DCtx,
    dict: *const core::ffi::c_void,
    dictSize: usize,
) -> usize {
    (*zbd).stage = ZBUFFds_loadHeader;
    (*zbd).outEnd = 0;
    (*zbd).outStart = (*zbd).outEnd;
    (*zbd).inPos = (*zbd).outStart;
    (*zbd).lhSize = (*zbd).inPos;
    ZSTDv06_decompressBegin_usingDict((*zbd).zd, dict, dictSize)
}
#[inline]
unsafe fn ZBUFFv06_limitCopy(
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    let length = if dstCapacity < srcSize {
        dstCapacity
    } else {
        srcSize
    };
    if length > 0 {
        memcpy(dst, src, length);
    }
    length
}

#[allow(clippy::drop_non_drop)]
pub(crate) unsafe fn ZBUFFv06_decompressContinue(
    zbd: *mut ZBUFFv06_DCtx,
    dst: *mut core::ffi::c_void,
    dstCapacityPtr: *mut usize,
    src: *const core::ffi::c_void,
    srcSizePtr: *mut usize,
) -> usize {
    let istart = src as *const core::ffi::c_char;
    let iend = istart.add(*srcSizePtr);
    let mut ip = istart;
    let ostart = dst as *mut core::ffi::c_char;
    let oend = ostart.add(*dstCapacityPtr);
    let mut op = ostart;
    let mut notDone = 1;

    while notDone != 0 {
        #[derive(Eq, PartialEq)]
        enum Block {
            Read,
            Load,
            Flush,
        }
        let mut current_block: Block;
        match (*zbd).stage as core::ffi::c_uint {
            ZBUFFds_init => return Error::init_missing.to_error_code(),
            ZBUFFds_loadHeader => {
                let hSize = ZSTDv06_getFrameParams(
                    &mut (*zbd).fParams,
                    ((*zbd).headerBuffer).as_mut_ptr() as *const core::ffi::c_void,
                    (*zbd).lhSize,
                );
                if hSize != 0 {
                    // if hSize!=0, hSize > zbd->lhSize
                    let toLoad = hSize - (*zbd).lhSize;
                    if ERR_isError(hSize) {
                        return hSize;
                    }
                    if toLoad > iend.offset_from_unsigned(ip) {
                        // not enough input to load full header
                        if !ip.is_null() {
                            memcpy(
                                ((*zbd).headerBuffer).as_mut_ptr().add((*zbd).lhSize)
                                    as *mut core::ffi::c_void,
                                ip as *const core::ffi::c_void,
                                iend.offset_from_unsigned(ip),
                            );
                        }
                        (*zbd).lhSize = ((*zbd).lhSize).wrapping_add(iend.offset_from_unsigned(ip));
                        *dstCapacityPtr = 0;
                        // remaining header bytes + next block header
                        return hSize
                            .wrapping_sub((*zbd).lhSize)
                            .wrapping_add(ZSTDv06_blockHeaderSize);
                    }
                    memcpy(
                        ((*zbd).headerBuffer).as_mut_ptr().add((*zbd).lhSize)
                            as *mut core::ffi::c_void,
                        ip as *const core::ffi::c_void,
                        toLoad,
                    );
                    (*zbd).lhSize = hSize;
                    ip = ip.add(toLoad);
                    continue;
                } else {
                    // Consume header
                    let h1Size = ZSTDv06_nextSrcSizeToDecompress((*zbd).zd); // == ZSTDv06_frameHeaderSize_min
                    let h1Result = ZSTDv06_decompressContinue(
                        (*zbd).zd,
                        core::ptr::null_mut(),
                        0,
                        ((*zbd).headerBuffer).as_mut_ptr() as *const core::ffi::c_void,
                        h1Size,
                    );
                    if ERR_isError(h1Result) {
                        return h1Result;
                    }
                    if h1Size < (*zbd).lhSize {
                        // long header
                        let h2Size = ZSTDv06_nextSrcSizeToDecompress((*zbd).zd);
                        let h2Result = ZSTDv06_decompressContinue(
                            (*zbd).zd,
                            core::ptr::null_mut(),
                            0,
                            ((*zbd).headerBuffer).as_mut_ptr().add(h1Size)
                                as *const core::ffi::c_void,
                            h2Size,
                        );
                        if ERR_isError(h2Result) {
                            return h2Result;
                        }
                    }

                    // Frame header instruct buffer sizes
                    let blockSize =
                        core::cmp::min(1 << (*zbd).fParams.windowLog, 128 * 1024) as usize;
                    (*zbd).blockSize = blockSize;
                    if (*zbd).inBuffSize < blockSize {
                        free((*zbd).inBuff as *mut core::ffi::c_void);
                        (*zbd).inBuffSize = blockSize;
                        (*zbd).inBuff = malloc(blockSize) as *mut core::ffi::c_char;
                        if ((*zbd).inBuff).is_null() {
                            return Error::memory_allocation.to_error_code();
                        }
                    }

                    let neededOutSize = (1usize << (*zbd).fParams.windowLog)
                        .wrapping_add(blockSize)
                        .wrapping_add((WILDCOPY_OVERLENGTH * 2) as usize);
                    if (*zbd).outBuffSize < neededOutSize {
                        free((*zbd).outBuff as *mut core::ffi::c_void);
                        (*zbd).outBuffSize = neededOutSize;
                        (*zbd).outBuff = malloc(neededOutSize) as *mut core::ffi::c_char;
                        if ((*zbd).outBuff).is_null() {
                            return Error::memory_allocation.to_error_code();
                        }
                    }
                    (*zbd).stage = ZBUFFds_read;
                    current_block = Block::Read;
                }
            }
            ZBUFFds_read => {
                current_block = Block::Read;
            }
            ZBUFFds_load => {
                current_block = Block::Load;
            }
            ZBUFFds_flush => {
                current_block = Block::Flush;
            }
            _ => return Error::GENERIC.to_error_code(),
        }
        if current_block == Block::Read {
            drop(current_block);

            let neededInSize = ZSTDv06_nextSrcSizeToDecompress((*zbd).zd);
            if neededInSize == 0 {
                // end of frame
                (*zbd).stage = ZBUFFds_init;
                notDone = 0;
                continue;
            }

            if iend.offset_from_unsigned(ip) >= neededInSize {
                // decode directly from src
                let decodedSize = ZSTDv06_decompressContinue(
                    (*zbd).zd,
                    ((*zbd).outBuff).add((*zbd).outStart) as *mut core::ffi::c_void,
                    ((*zbd).outBuffSize).wrapping_sub((*zbd).outStart),
                    ip as *const core::ffi::c_void,
                    neededInSize,
                );
                if ERR_isError(decodedSize) {
                    return decodedSize;
                }
                ip = ip.add(neededInSize);
                if decodedSize == 0 {
                    // this was just a header
                    continue;
                }
                (*zbd).outEnd = ((*zbd).outStart).wrapping_add(decodedSize);
                (*zbd).stage = ZBUFFds_flush;
                continue;
            }
            if ip == iend {
                // no more input
                notDone = 0;
                continue;
            }
            (*zbd).stage = ZBUFFds_load;
            current_block = Block::Load;
        }
        if current_block == Block::Load {
            drop(current_block);

            let neededInSize_0 = ZSTDv06_nextSrcSizeToDecompress((*zbd).zd);
            // should always be <= remaining space within inBuff
            let toLoad_0 = neededInSize_0.wrapping_sub((*zbd).inPos);
            let mut loadedSize: usize = 0;
            if toLoad_0 > ((*zbd).inBuffSize).wrapping_sub((*zbd).inPos) {
                return Error::corruption_detected.to_error_code();
            }
            loadedSize = ZBUFFv06_limitCopy(
                ((*zbd).inBuff).add((*zbd).inPos) as *mut core::ffi::c_void,
                toLoad_0,
                ip as *const core::ffi::c_void,
                iend.offset_from_unsigned(ip),
            );
            ip = ip.add(loadedSize);
            (*zbd).inPos = ((*zbd).inPos).wrapping_add(loadedSize);
            if loadedSize < toLoad_0 {
                // not enough input, wait for more
                notDone = 0;
                continue;
            }

            // decode loaded input
            let decodedSize_0 = ZSTDv06_decompressContinue(
                (*zbd).zd,
                ((*zbd).outBuff).add((*zbd).outStart) as *mut core::ffi::c_void,
                ((*zbd).outBuffSize).wrapping_sub((*zbd).outStart),
                (*zbd).inBuff as *const core::ffi::c_void,
                neededInSize_0,
            );
            if ERR_isError(decodedSize_0) {
                return decodedSize_0;
            }
            (*zbd).inPos = 0; // input is consumed
            if decodedSize_0 == 0 {
                // this was just a header
                (*zbd).stage = ZBUFFds_read;
                continue;
            }
            (*zbd).outEnd = ((*zbd).outStart).wrapping_add(decodedSize_0);
            (*zbd).stage = ZBUFFds_flush;
            current_block = Block::Flush;
        }
        if current_block == Block::Flush {
            drop(current_block);

            let toFlushSize = ((*zbd).outEnd).wrapping_sub((*zbd).outStart);
            let flushedSize = ZBUFFv06_limitCopy(
                op as *mut core::ffi::c_void,
                oend.offset_from_unsigned(op),
                ((*zbd).outBuff).add((*zbd).outStart) as *const core::ffi::c_void,
                toFlushSize,
            );
            op = op.add(flushedSize);
            (*zbd).outStart = ((*zbd).outStart).wrapping_add(flushedSize);
            if flushedSize == toFlushSize {
                (*zbd).stage = ZBUFFds_read;
                if ((*zbd).outStart).wrapping_add((*zbd).blockSize) > (*zbd).outBuffSize {
                    (*zbd).outEnd = 0;
                    (*zbd).outStart = (*zbd).outEnd;
                }
            }
            // cannot flush everything
            notDone = 0;
            continue;
        }
    }
    *srcSizePtr = ip.offset_from_unsigned(istart);
    *dstCapacityPtr = op.offset_from_unsigned(ostart);
    let mut nextSrcSizeHint = ZSTDv06_nextSrcSizeToDecompress((*zbd).zd);
    if nextSrcSizeHint > ZSTDv06_blockHeaderSize {
        nextSrcSizeHint = nextSrcSizeHint.wrapping_add(ZSTDv06_blockHeaderSize);
    }
    nextSrcSizeHint = nextSrcSizeHint.wrapping_sub((*zbd).inPos); // already loaded
    nextSrcSizeHint
}
