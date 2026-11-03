use core::marker::PhantomData;
use core::ptr;

use libc::{calloc, free, malloc};

use crate::lib::common::error_private::Error;
use crate::lib::common::mem::{MEM_32bits, MEM_64bits, MEM_readLE32, MEM_readLEST};
use crate::lib::common::reader::Reader;
use crate::lib::common::xxhash::{
    XXH64_state_t, ZSTD_XXH64_digest, ZSTD_XXH64_reset, ZSTD_XXH64_update_slice,
};
use crate::lib::decompress::huf_decompress::{DTableDesc, Writer};
use crate::ZSTD_CONTENTSIZE_ERROR;

#[derive(Copy, Clone, Default)]
#[repr(C)]
pub(crate) struct ZSTDv07_frameParams {
    pub(crate) frameContentSize: core::ffi::c_ulonglong,
    pub(crate) windowSize: core::ffi::c_uint,
    pub(crate) dictID: core::ffi::c_uint,
    pub(crate) checksumFlag: core::ffi::c_uint,
}

#[repr(C)]
pub(crate) struct ZSTDv07_DCtx {
    LLTable: FSEv07_DTable<512>,
    OffTable: FSEv07_DTable<256>,
    MLTable: FSEv07_DTable<512>,
    hufTable: HUFv07_DTable,
    previousDstEnd: *const u8,
    base: *const u8,
    vBase: *const u8,
    dictEnd: *const u8,
    expected: usize,
    rep: [u32; ZSTDv07_REP_INIT],
    fParams: ZSTDv07_frameParams,
    bType: blockType_t,
    stage: ZSTDv07_dStage,
    litEntropy: u32,
    fseEntropy: u32,
    xxhState: XXH64_state_t,
    headerSize: usize,
    dictID: u32,
    litPtr: *const u8,
    litSize: usize,
    litBuffer: [u8; ZSTDv07_BLOCKSIZE_ABSOLUTEMAX + WILDCOPY_OVERLENGTH],
    headerBuffer: [u8; ZSTDv07_frameHeaderSize_max],
}
type ZSTDv07_dStage = core::ffi::c_uint;
const ZSTDds_skipFrame: ZSTDv07_dStage = 5;
const ZSTDds_decodeSkippableHeader: ZSTDv07_dStage = 4;
const ZSTDds_decompressBlock: ZSTDv07_dStage = 3;
const ZSTDds_decodeBlockHeader: ZSTDv07_dStage = 2;
const ZSTDds_decodeFrameHeader: ZSTDv07_dStage = 1;
const ZSTDds_getFrameHeaderSize: ZSTDv07_dStage = 0;
type blockType_t = core::ffi::c_uint;
const bt_end: blockType_t = 3;
const bt_rle: blockType_t = 2;
const bt_raw: blockType_t = 1;
const bt_compressed: blockType_t = 0;
#[derive(Copy, Clone)]
#[repr(C)]
struct blockProperties_t {
    blockType: blockType_t,
    origSize: u32,
}
#[derive(Copy, Clone)]
#[repr(C)]
struct seqState_t<'a> {
    DStream: BITv07_DStream_t<'a>,
    stateLL: FSEv07_DState_t<'a, 512>,
    stateOffb: FSEv07_DState_t<'a, 256>,
    stateML: FSEv07_DState_t<'a, 512>,
    prevOffset: [usize; ZSTDv07_REP_INIT],
}
#[derive(Copy, Clone)]
#[repr(C)]
struct FSEv07_DState_t<'a, const N: usize> {
    state: usize,
    table: &'a [FSEv07_decode_t; N],
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
struct FSEv07_decode_t {
    newState: core::ffi::c_ushort,
    symbol: core::ffi::c_uchar,
    nbBits: core::ffi::c_uchar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum StreamStatus {
    /// Fully refilled.
    Unfinished = 0,
    /// Still some bits left in the bitstream.
    EndOfBuffer = 1,
    /// Bitstream entirely consumed, bit-exact.
    Completed = 2,
    /// User requested more bits than present in bitstream.
    Overflow = 3,
}

#[derive(Copy, Clone)]
#[repr(C)]
struct FSEv07_DTableHeader {
    tableLog: u16,
    fastMode: u16,
}
#[repr(C)]
struct FSEv07_DTable<const N: usize> {
    header: FSEv07_DTableHeader,
    data: [FSEv07_decode_t; N],
}
#[derive(Copy, Clone)]
#[repr(C)]
struct HUFv07_DTable {
    description: DTableDesc,
    data: [u32; 4096],
}
impl HUFv07_DTable {
    fn as_x2(&self) -> &[HUFv07_DEltX2; 8192] {
        // This is safe as HUFv07_DEltX2 is only 2 bytes long, not 4 bytes like HUFv07_DEltX4.
        unsafe { core::mem::transmute(&self.data) }
    }

    fn as_x2_mut(&mut self) -> &mut [HUFv07_DEltX2; 8192] {
        // This is safe as HUFv07_DEltX2 is only 2 bytes long, not 4 bytes like HUFv07_DEltX4.
        unsafe { core::mem::transmute(&mut self.data) }
    }

    fn as_x4(&self) -> &[HUFv07_DEltX4; 4096] {
        unsafe { core::mem::transmute(&self.data) }
    }

    fn as_x4_mut(&mut self) -> &mut [HUFv07_DEltX4; 4096] {
        unsafe { core::mem::transmute(&mut self.data) }
    }
}

#[derive(Copy, Clone, Default)]
#[repr(transparent)]
struct LE16([u8; 2]);

#[derive(Copy, Clone)]
#[repr(C)]
struct HUFv07_DEltX4 {
    sequence: LE16,
    nbBits: u8,
    length: u8,
}
#[derive(Copy, Clone)]
#[repr(C)]
struct HUFv07_DEltX2 {
    byte: u8,
    nbBits: u8,
}
type rankVal_t = [[u32; 17]; 16];
#[derive(Copy, Clone)]
#[repr(C)]
struct sortedSymbol_t {
    symbol: u8,
    weight: u8,
}
type litBlockType_t = core::ffi::c_uint;
#[repr(C)]
pub(crate) struct ZBUFFv07_DCtx_s {
    zd: *mut ZSTDv07_DCtx,
    fParams: ZSTDv07_frameParams,
    stage: ZBUFFv07_dStage,
    inBuff: *mut u8,
    inBuffSize: usize,
    inPos: usize,
    outBuff: *mut u8,
    outBuffSize: usize,
    outStart: usize,
    outEnd: usize,
    blockSize: usize,
    headerBuffer: [u8; 18],
    lhSize: usize,
}
type ZBUFFv07_dStage = core::ffi::c_uint;
const ZBUFFds_flush: ZBUFFv07_dStage = 4;
const ZBUFFds_load: ZBUFFv07_dStage = 3;
const ZBUFFds_read: ZBUFFv07_dStage = 2;
const ZBUFFds_loadHeader: ZBUFFv07_dStage = 1;
const ZBUFFds_init: ZBUFFv07_dStage = 0;
type ZBUFFv07_DCtx = ZBUFFv07_DCtx_s;
const ZSTDv07_MAGICNUMBER: core::ffi::c_uint = 0xfd2fb527;
const ZSTDv07_MAGIC_SKIPPABLE_START: core::ffi::c_uint = 0x184d2a50;
const ZSTDv07_WINDOWLOG_MAX_32: core::ffi::c_int = 25;
const ZSTDv07_WINDOWLOG_MAX_64: core::ffi::c_int = 27;
const ZSTDv07_BLOCKSIZE_ABSOLUTEMAX: usize = 128 * 1024;

const ZSTDv07_frameHeaderSize_min: usize = 5;
const ZSTDv07_frameHeaderSize_max: usize = 18;
const ZSTDv07_skippableHeaderSize: usize = 8; // magic number + skippable frame length

#[inline]
fn BITv07_highbit32(val: u32) -> core::ffi::c_uint {
    (val.leading_zeros() ^ 31) as core::ffi::c_uint
}

#[derive(Copy, Clone)]
#[repr(C)]
struct BITv07_DStream_t<'a> {
    bitContainer: usize,
    bitsConsumed: core::ffi::c_uint,
    ptr: *const u8,
    start: *const u8,
    _marker: PhantomData<&'a [u8]>,
}

impl<'a> BITv07_DStream_t<'a> {
    #[inline]
    fn new(src: &'a [u8]) -> Result<Self, Error> {
        if src.is_empty() {
            return Err(Error::srcSize_wrong);
        }

        if src.len() >= size_of::<usize>() {
            // normal case
            let mut bitD = BITv07_DStream_t {
                bitContainer: 0,
                bitsConsumed: 0,
                ptr: unsafe { src.as_ptr().add(src.len()).sub(size_of::<usize>()) },
                start: src.as_ptr(),
                _marker: PhantomData,
            };

            bitD.bitContainer = unsafe { MEM_readLEST(bitD.ptr as *const core::ffi::c_void) };
            let lastByte = src[src.len() - 1];
            bitD.bitsConsumed = if lastByte != 0 {
                8 - BITv07_highbit32(lastByte as u32)
            } else {
                0
            };
            if lastByte == 0 {
                return Err(Error::GENERIC); // endMark not present
            }

            Ok(bitD)
        } else {
            let mut bitD = BITv07_DStream_t {
                bitContainer: 0,
                bitsConsumed: 0,
                ptr: src.as_ptr(),
                start: src.as_ptr(),
                _marker: PhantomData,
            };

            bitD.bitContainer = usize::from(src[0]);

            if src.len() == 7 {
                bitD.bitContainer += usize::from(src[6]) << (usize::BITS - 16);
            }

            if src.len() >= 6 {
                bitD.bitContainer += usize::from(src[5]) << (usize::BITS - 24);
            }

            if src.len() >= 5 {
                bitD.bitContainer += usize::from(src[4]) << (usize::BITS - 32);
            }

            if src.len() >= 4 {
                bitD.bitContainer += usize::from(src[3]) << 24;
            }

            if src.len() >= 3 {
                bitD.bitContainer += usize::from(src[2]) << 16;
            }

            if src.len() >= 2 {
                bitD.bitContainer += usize::from(src[1]) << 8;
            }

            let lastByte = src[src.len() - 1];
            bitD.bitsConsumed = if lastByte != 0 {
                8 - BITv07_highbit32(lastByte as u32)
            } else {
                0
            };
            if lastByte == 0 {
                // endMark not present
                return Err(Error::GENERIC);
            }
            bitD.bitsConsumed += (size_of::<usize>() - src.len()) as u32 * 8;

            Ok(bitD)
        }
    }

    #[inline]
    fn look_bits(&self, nbBits: u32) -> usize {
        let bitMask = usize::BITS - 1;
        self.bitContainer << (self.bitsConsumed & bitMask)
            >> 1
            >> (bitMask.wrapping_sub(nbBits) & bitMask)
    }
    #[inline]
    fn look_bits_fast(&self, nbBits: u32) -> usize {
        let bitMask = usize::BITS - 1;
        self.bitContainer << (self.bitsConsumed & bitMask)
            >> (bitMask.wrapping_add(1).wrapping_sub(nbBits) & bitMask)
    }
    #[inline]
    fn skip_bits(&mut self, nbBits: u32) {
        self.bitsConsumed = self.bitsConsumed.wrapping_add(nbBits);
    }
    #[inline]
    fn read_bits(&mut self, nbBits: u32) -> usize {
        let value = self.look_bits(nbBits);
        self.skip_bits(nbBits);
        value
    }
    #[inline]
    fn read_bits_fast(&mut self, nbBits: u32) -> usize {
        let value = self.look_bits_fast(nbBits);
        self.skip_bits(nbBits);
        value
    }
    #[inline]
    fn reload(&mut self) -> StreamStatus {
        if self.bitsConsumed > usize::BITS {
            return StreamStatus::Overflow;
        }
        if self.ptr >= unsafe { self.start.add(size_of::<usize>()) } {
            self.ptr = unsafe { self.ptr.sub(self.bitsConsumed as usize >> 3) };
            self.bitsConsumed &= 7;
            self.bitContainer = unsafe { MEM_readLEST(self.ptr as *const core::ffi::c_void) };
            return StreamStatus::Unfinished;
        }
        if self.ptr == self.start {
            if self.bitsConsumed < usize::BITS {
                return StreamStatus::EndOfBuffer;
            }
            return StreamStatus::Completed;
        }
        let mut nbBytes = self.bitsConsumed as usize >> 3;
        let mut result = StreamStatus::Unfinished;
        if unsafe { self.ptr.sub(nbBytes) } < self.start {
            nbBytes = unsafe { self.ptr.offset_from_unsigned(self.start) };
            result = StreamStatus::EndOfBuffer;
        }
        self.ptr = unsafe { self.ptr.sub(nbBytes) };
        self.bitsConsumed = self.bitsConsumed.wrapping_sub((nbBytes * 8) as u32);
        self.bitContainer = unsafe { MEM_readLEST(self.ptr as *const core::ffi::c_void) };
        result
    }
    #[inline]
    fn is_empty(&self) -> bool {
        self.ptr == self.start && self.bitsConsumed == usize::BITS
    }
}

#[inline]
fn FSEv07_initDState<'a, const N: usize>(
    bitD: &mut BITv07_DStream_t,
    dt: &'a FSEv07_DTable<N>,
) -> FSEv07_DState_t<'a, N> {
    let state = bitD.read_bits(u32::from(dt.header.tableLog));
    bitD.reload();
    FSEv07_DState_t {
        state,
        table: &dt.data,
    }
}
#[inline]
fn FSEv07_peekSymbol<const N: usize>(DStatePtr: &FSEv07_DState_t<N>) -> u8 {
    let DInfo = DStatePtr.table[DStatePtr.state];
    DInfo.symbol
}
#[inline]
fn FSEv07_updateState<const N: usize>(
    DStatePtr: &mut FSEv07_DState_t<N>,
    bitD: &mut BITv07_DStream_t,
) {
    let DInfo = DStatePtr.table[DStatePtr.state];
    let nbBits = DInfo.nbBits as u32;
    let lowBits = bitD.read_bits(nbBits);
    DStatePtr.state = (DInfo.newState as usize).wrapping_add(lowBits);
}
#[inline]
fn FSEv07_decodeSymbol<const N: usize>(
    DStatePtr: &mut FSEv07_DState_t<N>,
    bitD: &mut BITv07_DStream_t,
) -> core::ffi::c_uchar {
    let DInfo = DStatePtr.table[DStatePtr.state];
    let nbBits = DInfo.nbBits as u32;
    let symbol = DInfo.symbol;
    let lowBits = bitD.read_bits(nbBits);
    DStatePtr.state = (DInfo.newState as usize).wrapping_add(lowBits);
    symbol
}
#[inline]
fn FSEv07_decodeSymbolFast<const N: usize>(
    DStatePtr: &mut FSEv07_DState_t<N>,
    bitD: &mut BITv07_DStream_t,
) -> core::ffi::c_uchar {
    let DInfo = DStatePtr.table[DStatePtr.state];
    let nbBits = DInfo.nbBits as u32;
    let symbol = DInfo.symbol;
    let lowBits = bitD.read_bits_fast(nbBits);
    DStatePtr.state = (DInfo.newState as usize).wrapping_add(lowBits);
    symbol
}
const FSEv07_MAX_MEMORY_USAGE: core::ffi::c_int = 14;
const FSEv07_MAX_SYMBOL_VALUE: core::ffi::c_int = 255;
const FSEv07_MAX_TABLELOG: core::ffi::c_int = FSEv07_MAX_MEMORY_USAGE - 2;
const FSEv07_MIN_TABLELOG: core::ffi::c_int = 5;
const FSEv07_TABLELOG_ABSOLUTE_MAX: core::ffi::c_int = 15;
const HUFv07_TABLELOG_ABSOLUTEMAX: usize = 16;
const HUFv07_TABLELOG_MAX: core::ffi::c_int = 12;
const HUFv07_SYMBOLVALUE_MAX: usize = 255;

unsafe fn FSEv07_readNCount(
    normalizedCounter: &mut [core::ffi::c_short],
    maxSVPtr: &mut core::ffi::c_uint,
    tableLogPtr: &mut core::ffi::c_uint,
    headerBuffer: &[u8],
) -> Result<usize, Error> {
    let istart = headerBuffer.as_ptr();
    let iend = istart.add(headerBuffer.len());
    let mut ip = istart;
    if headerBuffer.len() < 4 {
        return Err(Error::srcSize_wrong);
    }
    let mut bitStream = MEM_readLE32(ip as *const core::ffi::c_void);
    let mut nbBits = (bitStream & 0xf).wrapping_add(FSEv07_MIN_TABLELOG as u32) as core::ffi::c_int;
    if nbBits > FSEv07_TABLELOG_ABSOLUTE_MAX {
        return Err(Error::tableLog_tooLarge);
    }
    bitStream >>= 4;
    let mut bitCount = 4;
    *tableLogPtr = nbBits as core::ffi::c_uint;
    let mut remaining = (1 << nbBits) + 1;
    let mut threshold = 1 << nbBits;
    nbBits += 1;

    let mut charnum = 0;
    let mut previous0 = false;
    while remaining > 1 && charnum <= *maxSVPtr {
        if previous0 {
            let mut n0 = charnum;
            while bitStream & 0xffff == 0xffff {
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
                return Err(Error::maxSymbolValue_tooSmall);
            }
            while charnum < n0 {
                normalizedCounter[charnum as usize] = 0;
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
        remaining -= count.abs() as core::ffi::c_int;
        normalizedCounter[charnum as usize] = count;
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
        return Err(Error::GENERIC);
    }
    *maxSVPtr = charnum.wrapping_sub(1);

    ip = ip.offset(((bitCount + 7) >> 3) as isize);
    if ip.offset_from_unsigned(istart) > headerBuffer.len() {
        return Err(Error::srcSize_wrong);
    }
    Ok(ip.offset_from_unsigned(istart))
}
fn HUFv07_readStats(
    huffWeight: &mut [u8; HUFv07_SYMBOLVALUE_MAX + 1],
    rankStats: &mut [u32; HUFv07_TABLELOG_ABSOLUTEMAX + 1],
    nbSymbolsPtr: &mut u32,
    tableLogPtr: &mut u32,
    src: &[u8],
) -> Result<usize, Error> {
    let mut weightTotal: u32 = 0;
    let mut ip = src;
    let mut iSize: usize = 0;
    let mut oSize: usize = 0;
    if src.is_empty() {
        return Err(Error::srcSize_wrong);
    }
    iSize = ip[0] as usize;
    if iSize >= 128 {
        if iSize >= 242 {
            static l: [u32; 14] = [1, 2, 3, 4, 7, 8, 15, 16, 31, 32, 63, 64, 127, 128];
            oSize = l[iSize.wrapping_sub(242)] as usize;
            huffWeight.fill(1);
            iSize = 0;
        } else {
            oSize = iSize.wrapping_sub(127);
            iSize = oSize.wrapping_add(1) / 2;
            if iSize.wrapping_add(1) > src.len() {
                return Err(Error::srcSize_wrong);
            }
            if oSize >= huffWeight.len() {
                return Err(Error::corruption_detected);
            }
            ip = &ip[1..];
            let mut n: usize = 0;
            while n < oSize {
                huffWeight[n] = ip[n / 2] >> 4;
                huffWeight[n + 1] = ip[n / 2] & 15;
                n += 2;
            }
        }
    } else {
        if iSize.wrapping_add(1) > src.len() {
            return Err(Error::srcSize_wrong);
        }
        oSize = FSEv07_decompress(
            Writer::from_slice(&mut huffWeight[..HUFv07_SYMBOLVALUE_MAX]),
            &ip[1..iSize + 1],
        )?;
    }
    rankStats.fill(0);
    weightTotal = 0;
    for &weight in &huffWeight[..oSize] {
        if weight as usize >= HUFv07_TABLELOG_ABSOLUTEMAX {
            return Err(Error::corruption_detected);
        }
        rankStats[usize::from(weight)] += 1;
        weightTotal = weightTotal.wrapping_add((1 << weight as core::ffi::c_int >> 1) as u32);
    }
    if weightTotal == 0 {
        return Err(Error::corruption_detected);
    }
    let tableLog = BITv07_highbit32(weightTotal).wrapping_add(1);
    if tableLog > HUFv07_TABLELOG_ABSOLUTEMAX as u32 {
        return Err(Error::corruption_detected);
    }
    *tableLogPtr = tableLog;
    let total = (1 << tableLog) as u32;
    let rest = total.wrapping_sub(weightTotal);
    let verif = (1 << BITv07_highbit32(rest)) as u32;
    let lastWeight = BITv07_highbit32(rest).wrapping_add(1);
    if verif != rest {
        return Err(Error::corruption_detected);
    }
    huffWeight[oSize] = lastWeight as u8;
    rankStats[lastWeight as usize] += 1;
    if rankStats[1] < 2 || rankStats[1] & 1 != 0 {
        return Err(Error::corruption_detected);
    }
    *nbSymbolsPtr = oSize.wrapping_add(1) as u32;
    Ok(iSize.wrapping_add(1))
}
fn FSEv07_buildDTable<const N: usize>(
    dt: &mut FSEv07_DTable<N>,
    normalizedCounter: &[core::ffi::c_short],
    maxSymbolValue: core::ffi::c_uint,
    tableLog: core::ffi::c_uint,
) -> Result<(), Error> {
    let tableDecode = &mut dt.data;
    let mut symbolNext: [u16; 256] = [0; 256];
    let maxSV1 = maxSymbolValue as usize + 1;
    let tableSize = (1 << tableLog) as u32;
    let mut highThreshold = tableSize.wrapping_sub(1);
    if maxSymbolValue > FSEv07_MAX_SYMBOL_VALUE as core::ffi::c_uint {
        return Err(Error::maxSymbolValue_tooLarge);
    }
    if tableLog > FSEv07_MAX_TABLELOG as core::ffi::c_uint {
        return Err(Error::tableLog_tooLarge);
    }
    let mut DTableH = FSEv07_DTableHeader {
        tableLog: 0,
        fastMode: 0,
    };
    DTableH.tableLog = tableLog as u16;
    DTableH.fastMode = 1;
    let largeLimit = (1 << tableLog.wrapping_sub(1)) as i16;
    for s in 0..maxSV1 {
        if normalizedCounter[s] == -1 {
            tableDecode[highThreshold as usize].symbol = s as u8;
            highThreshold = highThreshold.wrapping_sub(1);
            symbolNext[s] = 1;
        } else {
            if normalizedCounter[s] >= largeLimit {
                DTableH.fastMode = 0;
            }
            symbolNext[s] = normalizedCounter[s] as u16;
        }
    }
    dt.header = DTableH;
    let tableMask = tableSize.wrapping_sub(1);
    let step = (tableSize >> 1)
        .wrapping_add(tableSize >> 3)
        .wrapping_add(3);
    let mut position = 0u32;
    #[allow(clippy::needless_range_loop)]
    for s in 0..maxSV1 {
        for _ in 0..normalizedCounter[s] as core::ffi::c_int {
            tableDecode[position as usize].symbol = s as u8;
            position = position.wrapping_add(step) & tableMask;
            while position > highThreshold {
                position = position.wrapping_add(step) & tableMask;
            }
        }
    }
    if position != 0 {
        return Err(Error::GENERIC);
    }
    for u in 0..tableSize {
        let symbol = tableDecode[u as usize].symbol;
        let nextState = symbolNext[usize::from(symbol)];
        symbolNext[usize::from(symbol)] += 1;
        tableDecode[u as usize].nbBits =
            tableLog.wrapping_sub(BITv07_highbit32(nextState as u32)) as u8;
        tableDecode[u as usize].newState = (((nextState as core::ffi::c_int)
            << tableDecode[u as usize].nbBits as core::ffi::c_int)
            as u32)
            .wrapping_sub(tableSize) as u16;
    }
    Ok(())
}
fn FSEv07_buildDTable_rle<const N: usize>(dt: &mut FSEv07_DTable<N>, symbolValue: u8) {
    let DTableH = &mut dt.header;
    let cell = &mut dt.data[0];
    DTableH.tableLog = 0;
    DTableH.fastMode = 0;
    cell.newState = 0;
    cell.symbol = symbolValue;
    cell.nbBits = 0;
}
#[inline(always)]
fn FSEv07_decompress_usingDTable_generic<const N: usize>(
    mut dst: Writer<'_>,
    cSrc: &[u8],
    dt: &FSEv07_DTable<N>,
    fast: core::ffi::c_uint,
) -> Result<usize, Error> {
    let dst_capacity = dst.capacity();
    let mut bitD = BITv07_DStream_t::new(cSrc)?;
    let mut state1 = FSEv07_initDState(&mut bitD, dt);
    let mut state2 = FSEv07_initDState(&mut bitD, dt);
    while bitD.reload() == StreamStatus::Unfinished && dst.capacity() < 4 {
        dst.write_u8(if fast != 0 {
            FSEv07_decodeSymbolFast(&mut state1, &mut bitD)
        } else {
            FSEv07_decodeSymbol(&mut state1, &mut bitD)
        });
        if (FSEv07_MAX_TABLELOG * 2 + 7) as u32 > usize::BITS {
            bitD.reload();
        }
        dst.write_u8(if fast != 0 {
            FSEv07_decodeSymbolFast(&mut state2, &mut bitD)
        } else {
            FSEv07_decodeSymbol(&mut state2, &mut bitD)
        });
        if (FSEv07_MAX_TABLELOG * 4 + 7) as u32 > usize::BITS
            && bitD.reload() > StreamStatus::Unfinished
        {
            break;
        }
        dst.write_u8(if fast != 0 {
            FSEv07_decodeSymbolFast(&mut state1, &mut bitD)
        } else {
            FSEv07_decodeSymbol(&mut state1, &mut bitD)
        });
        if (FSEv07_MAX_TABLELOG * 2 + 7) as u32 > usize::BITS {
            bitD.reload();
        }
        dst.write_u8(if fast != 0 {
            FSEv07_decodeSymbolFast(&mut state2, &mut bitD)
        } else {
            FSEv07_decodeSymbol(&mut state2, &mut bitD)
        });
    }
    loop {
        if dst.capacity() < 2 {
            return Err(Error::dstSize_tooSmall);
        }
        dst.write_u8(if fast != 0 {
            FSEv07_decodeSymbolFast(&mut state1, &mut bitD)
        } else {
            FSEv07_decodeSymbol(&mut state1, &mut bitD)
        });
        if bitD.reload() == StreamStatus::Overflow {
            dst.write_u8(if fast != 0 {
                FSEv07_decodeSymbolFast(&mut state2, &mut bitD)
            } else {
                FSEv07_decodeSymbol(&mut state2, &mut bitD)
            });
            break;
        } else {
            if dst.capacity() < 2 {
                return Err(Error::dstSize_tooSmall);
            }
            dst.write_u8(if fast != 0 {
                FSEv07_decodeSymbolFast(&mut state2, &mut bitD)
            } else {
                FSEv07_decodeSymbol(&mut state2, &mut bitD)
            });
            if bitD.reload() != StreamStatus::Overflow {
                continue;
            }
            dst.write_u8(if fast != 0 {
                FSEv07_decodeSymbolFast(&mut state1, &mut bitD)
            } else {
                FSEv07_decodeSymbol(&mut state1, &mut bitD)
            });
            break;
        }
    }
    Ok(dst_capacity - dst.capacity())
}
fn FSEv07_decompress_usingDTable<const N: usize>(
    dst: Writer<'_>,
    cSrc: &[u8],
    dt: &FSEv07_DTable<N>,
) -> Result<usize, Error> {
    if dt.header.fastMode != 0 {
        return FSEv07_decompress_usingDTable_generic(dst, cSrc, dt, 1);
    }
    FSEv07_decompress_usingDTable_generic(dst, cSrc, dt, 0)
}
fn FSEv07_decompress(dst: Writer<'_>, cSrc: &[u8]) -> Result<usize, Error> {
    let istart = cSrc;
    let mut ip = istart;
    let mut counting: [core::ffi::c_short; 256] = [0; 256];
    let mut dt = FSEv07_DTable {
        header: FSEv07_DTableHeader {
            tableLog: 0,
            fastMode: 0,
        },
        data: [FSEv07_decode_t {
            newState: 0,
            symbol: 0,
            nbBits: 0,
        }; 4096],
    };
    let mut tableLog: core::ffi::c_uint = 0;
    let mut maxSymbolValue = FSEv07_MAX_SYMBOL_VALUE as core::ffi::c_uint;
    if cSrc.len() < 2 {
        return Err(Error::srcSize_wrong);
    }
    let NCountLength =
        unsafe { FSEv07_readNCount(&mut counting, &mut maxSymbolValue, &mut tableLog, istart)? };
    if NCountLength >= cSrc.len() {
        return Err(Error::srcSize_wrong);
    }
    ip = &ip[NCountLength..];
    FSEv07_buildDTable(&mut dt, &counting, maxSymbolValue, tableLog)?;
    FSEv07_decompress_usingDTable(dst, ip, &dt)
}
fn HUFv07_readDTableX2(DTable: &mut HUFv07_DTable, src: &[u8]) -> Result<usize, Error> {
    let mut huffWeight: [u8; HUFv07_SYMBOLVALUE_MAX + 1] = [0; HUFv07_SYMBOLVALUE_MAX + 1];
    let mut rankVal: [u32; HUFv07_TABLELOG_ABSOLUTEMAX + 1] = [0; HUFv07_TABLELOG_ABSOLUTEMAX + 1];
    let mut tableLog = 0;
    let mut nbSymbols = 0;
    let mut iSize: usize = 0;
    iSize = HUFv07_readStats(
        &mut huffWeight,
        &mut rankVal,
        &mut nbSymbols,
        &mut tableLog,
        src,
    )?;
    let mut dtd = DTable.description;
    if tableLog > (dtd.maxTableLog as core::ffi::c_int + 1) as u32 {
        return Err(Error::tableLog_tooLarge);
    }
    dtd.tableType = 0;
    dtd.tableLog = tableLog as u8;
    DTable.description = dtd;
    let dt = DTable.as_x2_mut();
    let mut nextRankStart = 0u32;
    #[allow(clippy::needless_range_loop)]
    for n in 1..tableLog as usize + 1 {
        let current = nextRankStart;
        nextRankStart += rankVal[n] << (n - 1);
        rankVal[n] = current;
    }
    for n in 0..nbSymbols {
        let w = huffWeight[n as usize];
        let length = (1 << w >> 1) as u32;
        let D = HUFv07_DEltX2 {
            byte: n as u8,
            nbBits: tableLog.wrapping_add(1).wrapping_sub(w as u32) as u8,
        };
        dt[rankVal[usize::from(w)] as usize..][..length as usize].fill(D);
        rankVal[usize::from(w)] += length;
    }
    Ok(iSize)
}
fn HUFv07_decodeSymbolX2(
    Dstream: &mut BITv07_DStream_t,
    dt: &[HUFv07_DEltX2; 8192],
    dtLog: u32,
) -> u8 {
    let val = Dstream.look_bits_fast(dtLog);
    let c = dt[val].byte;
    Dstream.skip_bits(dt[val].nbBits as u32);
    c
}
#[inline]
fn HUFv07_decodeStreamX2(
    mut dst: Writer<'_>,
    bitDPtr: &mut BITv07_DStream_t,
    dt: &[HUFv07_DEltX2; 8192],
    dtLog: u32,
) -> usize {
    let dst_capacity = dst.capacity();
    while bitDPtr.reload() == StreamStatus::Unfinished && dst.capacity() >= 4 {
        if MEM_64bits() {
            dst.write_u8(HUFv07_decodeSymbolX2(bitDPtr, dt, dtLog));
        }
        if MEM_64bits() || HUFv07_TABLELOG_MAX <= 12 {
            dst.write_u8(HUFv07_decodeSymbolX2(bitDPtr, dt, dtLog));
        }
        if MEM_64bits() {
            dst.write_u8(HUFv07_decodeSymbolX2(bitDPtr, dt, dtLog));
        }
        dst.write_u8(HUFv07_decodeSymbolX2(bitDPtr, dt, dtLog));
    }
    while bitDPtr.reload() == StreamStatus::Unfinished && dst.capacity() >= 1 {
        dst.write_u8(HUFv07_decodeSymbolX2(bitDPtr, dt, dtLog));
    }
    while dst.capacity() >= 1 {
        dst.write_u8(HUFv07_decodeSymbolX2(bitDPtr, dt, dtLog));
    }
    dst_capacity - dst.capacity()
}
fn HUFv07_decompress1X2_usingDTable_internal(
    dst: Writer<'_>,
    cSrc: &[u8],
    DTable: &HUFv07_DTable,
) -> Result<(), Error> {
    let dt = DTable.as_x2();
    let dtd = DTable.description;
    let dtLog = dtd.tableLog as u32;
    let mut bitD = BITv07_DStream_t::new(cSrc)?;
    HUFv07_decodeStreamX2(dst, &mut bitD, dt, dtLog);
    if !bitD.is_empty() {
        return Err(Error::corruption_detected);
    }
    Ok(())
}
fn HUFv07_decompress1X2_DCtx(
    DCtx: &mut HUFv07_DTable,
    dst: Writer<'_>,
    cSrc: &[u8],
) -> Result<(), Error> {
    let hSize = HUFv07_readDTableX2(DCtx, cSrc)?;
    if hSize >= cSrc.len() {
        return Err(Error::srcSize_wrong);
    }
    HUFv07_decompress1X2_usingDTable_internal(dst, &cSrc[hSize..], DCtx)
}
fn HUFv07_decompress4X2_usingDTable_internal(
    mut dst: Writer<'_>,
    cSrc: &[u8],
    DTable: &HUFv07_DTable,
) -> Result<usize, Error> {
    if cSrc.len() < 10 {
        return Err(Error::corruption_detected);
    }
    if dst.capacity() < 6 {
        return Err(Error::corruption_detected);
    }
    let mut ip = cSrc;
    let dstSize = dst.capacity();
    let dt = DTable.as_x2();
    let length1 = usize::from(u16::from_le_bytes(ip[0..2].try_into().unwrap()));
    let length2 = usize::from(u16::from_le_bytes(ip[2..4].try_into().unwrap()));
    let length3 = usize::from(u16::from_le_bytes(ip[4..6].try_into().unwrap()));
    ip = &ip[6..];
    let (istart1, istart2, istart3, istart4);
    (istart1, ip) = ip
        .split_at_checked(length1)
        .ok_or(Error::corruption_detected)?;
    (istart2, ip) = ip
        .split_at_checked(length2)
        .ok_or(Error::corruption_detected)?;
    (istart3, ip) = ip
        .split_at_checked(length3)
        .ok_or(Error::corruption_detected)?;
    istart4 = ip;
    let Some((mut op1, mut op2, mut op3, mut op4)) = dst.quarter() else {
        return Err(Error::corruption_detected);
    };
    let dtLog = DTable.description.tableLog as u32;
    let mut bitD1 = BITv07_DStream_t::new(istart1)?;
    let mut bitD2 = BITv07_DStream_t::new(istart2)?;
    let mut bitD3 = BITv07_DStream_t::new(istart3)?;
    let mut bitD4 = BITv07_DStream_t::new(istart4)?;
    let mut endSignal = true;
    endSignal &= bitD1.reload() == StreamStatus::Unfinished;
    endSignal &= bitD2.reload() == StreamStatus::Unfinished;
    endSignal &= bitD3.reload() == StreamStatus::Unfinished;
    endSignal &= bitD4.reload() == StreamStatus::Unfinished;
    while endSignal
        && op1.capacity() >= 8
        && op2.capacity() >= 8
        && op3.capacity() >= 8
        && op4.capacity() >= 8
    {
        if MEM_64bits() {
            op1.write_u8(HUFv07_decodeSymbolX2(&mut bitD1, dt, dtLog));
            op2.write_u8(HUFv07_decodeSymbolX2(&mut bitD2, dt, dtLog));
            op3.write_u8(HUFv07_decodeSymbolX2(&mut bitD3, dt, dtLog));
            op4.write_u8(HUFv07_decodeSymbolX2(&mut bitD4, dt, dtLog));
        }
        if MEM_64bits() || HUFv07_TABLELOG_MAX <= 12 {
            op1.write_u8(HUFv07_decodeSymbolX2(&mut bitD1, dt, dtLog));
            op2.write_u8(HUFv07_decodeSymbolX2(&mut bitD2, dt, dtLog));
            op3.write_u8(HUFv07_decodeSymbolX2(&mut bitD3, dt, dtLog));
            op4.write_u8(HUFv07_decodeSymbolX2(&mut bitD4, dt, dtLog));
        }
        if MEM_64bits() {
            op1.write_u8(HUFv07_decodeSymbolX2(&mut bitD1, dt, dtLog));
            op2.write_u8(HUFv07_decodeSymbolX2(&mut bitD2, dt, dtLog));
            op3.write_u8(HUFv07_decodeSymbolX2(&mut bitD3, dt, dtLog));
            op4.write_u8(HUFv07_decodeSymbolX2(&mut bitD4, dt, dtLog));
        }
        op1.write_u8(HUFv07_decodeSymbolX2(&mut bitD1, dt, dtLog));
        op2.write_u8(HUFv07_decodeSymbolX2(&mut bitD2, dt, dtLog));
        op3.write_u8(HUFv07_decodeSymbolX2(&mut bitD3, dt, dtLog));
        op4.write_u8(HUFv07_decodeSymbolX2(&mut bitD4, dt, dtLog));
        endSignal &= bitD1.reload() == StreamStatus::Unfinished;
        endSignal &= bitD2.reload() == StreamStatus::Unfinished;
        endSignal &= bitD3.reload() == StreamStatus::Unfinished;
        endSignal &= bitD4.reload() == StreamStatus::Unfinished;
    }
    HUFv07_decodeStreamX2(op1, &mut bitD1, dt, dtLog);
    HUFv07_decodeStreamX2(op2, &mut bitD2, dt, dtLog);
    HUFv07_decodeStreamX2(op3, &mut bitD3, dt, dtLog);
    HUFv07_decodeStreamX2(op4, &mut bitD4, dt, dtLog);
    if !(bitD1.is_empty() && bitD2.is_empty() && bitD3.is_empty() && bitD4.is_empty()) {
        return Err(Error::corruption_detected);
    }
    Ok(dstSize)
}
fn HUFv07_decompress4X2_DCtx(
    dctx: &mut HUFv07_DTable,
    dst: Writer<'_>,
    cSrc: &[u8],
) -> Result<usize, Error> {
    let hSize = HUFv07_readDTableX2(dctx, cSrc)?;
    if hSize >= cSrc.len() {
        return Err(Error::srcSize_wrong);
    }
    HUFv07_decompress4X2_usingDTable_internal(dst, &cSrc[hSize..], dctx)
}
fn HUFv07_fillDTableX4Level2(
    DTable: &mut [HUFv07_DEltX4],
    sizeLog: u32,
    consumed: u32,
    rankValOrigin: &[u32; HUFv07_TABLELOG_ABSOLUTEMAX + 1],
    minWeight: core::ffi::c_int,
    sortedSymbols: &[sortedSymbol_t],
    nbBitsBaseline: u32,
    baseSeq: u16,
) {
    let mut DElt = HUFv07_DEltX4 {
        sequence: LE16([0; 2]),
        nbBits: 0,
        length: 0,
    };
    let mut rankVal: [u32; 17] = *rankValOrigin;
    if minWeight > 1 {
        let skipSize = rankVal[minWeight as usize];
        DElt.sequence = LE16(baseSeq.to_le_bytes());
        DElt.nbBits = consumed as u8;
        DElt.length = 1;
        DTable[..skipSize as usize].fill(DElt);
    }
    for sym in sortedSymbols {
        let symbol = sym.symbol as u32;
        let weight = sym.weight as u32;
        let nbBits = nbBitsBaseline.wrapping_sub(weight);
        let length = (1 << sizeLog.wrapping_sub(nbBits)) as u32;
        let start = rankVal[weight as usize];
        DElt.sequence = LE16(u16::to_le_bytes(
            (baseSeq as u32).wrapping_add(symbol << 8) as u16
        ));
        DElt.nbBits = nbBits.wrapping_add(consumed) as u8;
        DElt.length = 2;
        DTable[start as usize..][..length as usize].fill(DElt);
        rankVal[weight as usize] += length;
    }
}
fn HUFv07_fillDTableX4(
    DTable: &mut [HUFv07_DEltX4; 4096],
    targetLog: u32,
    sortedList: &[sortedSymbol_t],
    rankStart: &[u32; HUFv07_TABLELOG_ABSOLUTEMAX + 2],
    rankValOrigin: &mut [[u32; HUFv07_TABLELOG_ABSOLUTEMAX + 1]; HUFv07_TABLELOG_ABSOLUTEMAX],
    maxWeight: u32,
    nbBitsBaseline: u32,
) {
    let scaleLog = nbBitsBaseline.wrapping_sub(targetLog) as core::ffi::c_int;
    let minBits = nbBitsBaseline.wrapping_sub(maxWeight);
    let mut rankVal: [u32; 17] = rankValOrigin[0];
    for sym in sortedList {
        let symbol = sym.symbol as u16;
        let weight = sym.weight as u32;
        let nbBits = nbBitsBaseline.wrapping_sub(weight);
        let start = rankVal[weight as usize];
        let length = (1 << targetLog.wrapping_sub(nbBits)) as u32;
        if targetLog.wrapping_sub(nbBits) >= minBits {
            let mut sortedRank: u32 = 0;
            let minWeight = Ord::max(nbBits.wrapping_add(scaleLog as u32) as core::ffi::c_int, 1);
            sortedRank = rankStart[minWeight as usize];
            HUFv07_fillDTableX4Level2(
                &mut DTable[start as usize..],
                targetLog.wrapping_sub(nbBits),
                nbBits,
                &rankValOrigin[nbBits as usize],
                minWeight,
                &sortedList[sortedRank as usize..],
                nbBitsBaseline,
                symbol,
            );
        } else {
            let DElt = HUFv07_DEltX4 {
                sequence: LE16(symbol.to_le_bytes()),
                nbBits: nbBits as u8,
                length: 1,
            };
            DTable[start as usize..][..length as usize].fill(DElt);
        }
        rankVal[weight as usize] += length;
    }
}
fn HUFv07_readDTableX4(DTable: &mut HUFv07_DTable, src: &[u8]) -> Result<usize, Error> {
    let mut weightList: [u8; HUFv07_SYMBOLVALUE_MAX + 1] = [0; HUFv07_SYMBOLVALUE_MAX + 1];
    let mut sortedSymbol: [sortedSymbol_t; 256] = [sortedSymbol_t {
        symbol: 0,
        weight: 0,
    }; 256];
    let mut rankStats: [u32; HUFv07_TABLELOG_ABSOLUTEMAX + 1] =
        [0; HUFv07_TABLELOG_ABSOLUTEMAX + 1];
    let mut rankStart0: [u32; HUFv07_TABLELOG_ABSOLUTEMAX + 2] =
        [0; HUFv07_TABLELOG_ABSOLUTEMAX + 2];
    let rankStart = &mut rankStart0[1..];
    let mut rankVal: rankVal_t = [[0; 17]; 16];
    let mut tableLog: u32 = 0;
    let mut sizeOfSort: u32 = 0;
    let mut nbSymbols: u32 = 0;
    let mut dtd = DTable.description;
    let maxTableLog = dtd.maxTableLog as u32;
    let mut iSize: usize = 0;
    let dt = DTable.as_x4_mut();
    if maxTableLog > HUFv07_TABLELOG_ABSOLUTEMAX as u32 {
        return Err(Error::tableLog_tooLarge);
    }
    iSize = HUFv07_readStats(
        &mut weightList,
        &mut rankStats,
        &mut nbSymbols,
        &mut tableLog,
        src,
    )?;
    if tableLog > maxTableLog {
        return Err(Error::tableLog_tooLarge);
    }
    let mut maxW = tableLog;
    while rankStats[maxW as usize] == 0 {
        maxW -= 1
    }
    let mut nextRankStart = 0u32;
    for w in 1..maxW + 1 {
        let current = nextRankStart;
        nextRankStart = nextRankStart.wrapping_add(rankStats[w as usize]);
        rankStart[w as usize] = current;
    }
    rankStart[0] = nextRankStart;
    sizeOfSort = nextRankStart;
    for s in 0..nbSymbols {
        let w = weightList[s as usize];
        let r = rankStart[usize::from(w)] as usize;
        rankStart[usize::from(w)] += 1;
        sortedSymbol[r].symbol = s as u8;
        sortedSymbol[r].weight = w;
    }
    rankStart[0] = 0;
    let rescale = maxTableLog.wrapping_sub(tableLog).wrapping_sub(1) as core::ffi::c_int;
    let mut nextRankVal = 0u32;
    for w in 1..maxW + 1 {
        let current = nextRankVal;
        nextRankVal += rankStats[w as usize] << w.wrapping_add(rescale as u32);
        rankVal[0][w as usize] = current;
    }
    let minBits = tableLog.wrapping_add(1).wrapping_sub(maxW);
    for consumed in minBits..maxTableLog.wrapping_sub(minBits).wrapping_add(1) {
        for w in 1..maxW + 1 {
            rankVal[consumed as usize][w as usize] = rankVal[0][w as usize] >> consumed;
        }
    }
    HUFv07_fillDTableX4(
        dt,
        maxTableLog,
        &sortedSymbol[..sizeOfSort as usize],
        &rankStart0,
        &mut rankVal,
        maxW,
        tableLog + 1,
    );
    dtd.tableLog = maxTableLog as u8;
    dtd.tableType = 1;
    DTable.description = dtd;
    Ok(iSize)
}
fn HUFv07_decodeSymbolX4(
    dst: &mut Writer<'_>,
    DStream: &mut BITv07_DStream_t,
    dt: &[HUFv07_DEltX4; 4096],
    dtLog: u32,
) {
    let val = DStream.look_bits_fast(dtLog);
    dst.write_symbol_x2(u16::from_le_bytes(dt[val].sequence.0), dt[val].length);
    DStream.skip_bits(dt[val].nbBits as u32);
}
fn HUFv07_decodeLastSymbolX4(
    dst: &mut Writer<'_>,
    DStream: &mut BITv07_DStream_t,
    dt: &[HUFv07_DEltX4],
    dtLog: u32,
) {
    let val = DStream.look_bits_fast(dtLog);
    dst.write_u8(dt[val].sequence.0[0]);
    if dt[val].length == 1 {
        DStream.skip_bits(dt[val].nbBits as u32);
    } else if DStream.bitsConsumed < usize::BITS {
        DStream.skip_bits(dt[val].nbBits as u32);
        DStream.bitsConsumed = Ord::min(DStream.bitsConsumed, usize::BITS);
    }
}
#[inline]
fn HUFv07_decodeStreamX4(
    mut dst: Writer<'_>,
    bitDPtr: &mut BITv07_DStream_t,
    dt: &[HUFv07_DEltX4; 4096],
    dtLog: u32,
) -> usize {
    let dst_capacity = dst.capacity();
    while bitDPtr.reload() == StreamStatus::Unfinished && dst.capacity() >= 8 {
        if MEM_64bits() {
            HUFv07_decodeSymbolX4(&mut dst, bitDPtr, dt, dtLog);
        }
        if MEM_64bits() || HUFv07_TABLELOG_MAX <= 12 {
            HUFv07_decodeSymbolX4(&mut dst, bitDPtr, dt, dtLog);
        }
        if MEM_64bits() {
            HUFv07_decodeSymbolX4(&mut dst, bitDPtr, dt, dtLog);
        }
        HUFv07_decodeSymbolX4(&mut dst, bitDPtr, dt, dtLog);
    }
    while bitDPtr.reload() == StreamStatus::Unfinished && dst.capacity() >= 2 {
        HUFv07_decodeSymbolX4(&mut dst, bitDPtr, dt, dtLog);
    }
    while dst.capacity() >= 2 {
        HUFv07_decodeSymbolX4(&mut dst, bitDPtr, dt, dtLog);
    }
    if dst.capacity() > 0 {
        HUFv07_decodeLastSymbolX4(&mut dst, bitDPtr, dt, dtLog);
    }
    dst_capacity - dst.capacity()
}
fn HUFv07_decompress1X4_usingDTable_internal(
    dst: Writer<'_>,
    cSrc: &[u8],
    DTable: &HUFv07_DTable,
) -> Result<(), Error> {
    let mut bitD = BITv07_DStream_t::new(cSrc)?;
    let dt = DTable.as_x4();
    let dtd = DTable.description;
    HUFv07_decodeStreamX4(dst, &mut bitD, dt, dtd.tableLog as u32);
    if !bitD.is_empty() {
        return Err(Error::corruption_detected);
    }
    Ok(())
}
fn HUFv07_decompress1X4_usingDTable(
    dst: Writer<'_>,
    cSrc: &[u8],
    DTable: &HUFv07_DTable,
) -> Result<(), Error> {
    let dtd = DTable.description;
    if dtd.tableType != 1 {
        return Err(Error::GENERIC);
    }
    HUFv07_decompress1X4_usingDTable_internal(dst, cSrc, DTable)
}
fn HUFv07_decompress4X4_usingDTable_internal(
    mut dst: Writer<'_>,
    cSrc: &[u8],
    DTable: &HUFv07_DTable,
) -> Result<usize, Error> {
    if cSrc.len() < 10 {
        return Err(Error::corruption_detected);
    }
    let mut ip = cSrc;
    let dstSize = dst.capacity();
    let dt = DTable.as_x4();
    let length1 = usize::from(u16::from_le_bytes(ip[0..2].try_into().unwrap()));
    let length2 = usize::from(u16::from_le_bytes(ip[2..4].try_into().unwrap()));
    let length3 = usize::from(u16::from_le_bytes(ip[4..6].try_into().unwrap()));
    ip = &ip[6..];
    let (istart1, istart2, istart3, istart4);
    (istart1, ip) = ip
        .split_at_checked(length1)
        .ok_or(Error::corruption_detected)?;
    (istart2, ip) = ip
        .split_at_checked(length2)
        .ok_or(Error::corruption_detected)?;
    (istart3, ip) = ip
        .split_at_checked(length3)
        .ok_or(Error::corruption_detected)?;
    istart4 = ip;
    let Some((mut op1, mut op2, mut op3, mut op4)) = dst.quarter() else {
        return Err(Error::corruption_detected);
    };
    let dtLog = DTable.description.tableLog as u32;
    let mut bitD1 = BITv07_DStream_t::new(istart1)?;
    let mut bitD2 = BITv07_DStream_t::new(istart2)?;
    let mut bitD3 = BITv07_DStream_t::new(istart3)?;
    let mut bitD4 = BITv07_DStream_t::new(istart4)?;
    let mut endSignal = true;
    endSignal &= bitD1.reload() == StreamStatus::Unfinished;
    endSignal &= bitD2.reload() == StreamStatus::Unfinished;
    endSignal &= bitD3.reload() == StreamStatus::Unfinished;
    endSignal &= bitD4.reload() == StreamStatus::Unfinished;
    while endSignal
        && op1.capacity() >= 8
        && op2.capacity() >= 8
        && op3.capacity() >= 8
        && op4.capacity() >= 8
    {
        if MEM_64bits() {
            HUFv07_decodeSymbolX4(&mut op1, &mut bitD1, dt, dtLog);
            HUFv07_decodeSymbolX4(&mut op2, &mut bitD2, dt, dtLog);
            HUFv07_decodeSymbolX4(&mut op3, &mut bitD3, dt, dtLog);
            HUFv07_decodeSymbolX4(&mut op4, &mut bitD4, dt, dtLog);
        }
        if MEM_64bits() || HUFv07_TABLELOG_MAX <= 12 {
            HUFv07_decodeSymbolX4(&mut op1, &mut bitD1, dt, dtLog);
            HUFv07_decodeSymbolX4(&mut op2, &mut bitD2, dt, dtLog);
            HUFv07_decodeSymbolX4(&mut op3, &mut bitD3, dt, dtLog);
            HUFv07_decodeSymbolX4(&mut op4, &mut bitD4, dt, dtLog);
        }
        if MEM_64bits() {
            HUFv07_decodeSymbolX4(&mut op1, &mut bitD1, dt, dtLog);
            HUFv07_decodeSymbolX4(&mut op2, &mut bitD2, dt, dtLog);
            HUFv07_decodeSymbolX4(&mut op3, &mut bitD3, dt, dtLog);
            HUFv07_decodeSymbolX4(&mut op4, &mut bitD4, dt, dtLog);
        }

        HUFv07_decodeSymbolX4(&mut op1, &mut bitD1, dt, dtLog);
        HUFv07_decodeSymbolX4(&mut op2, &mut bitD2, dt, dtLog);
        HUFv07_decodeSymbolX4(&mut op3, &mut bitD3, dt, dtLog);
        HUFv07_decodeSymbolX4(&mut op4, &mut bitD4, dt, dtLog);
        endSignal &= bitD1.reload() == StreamStatus::Unfinished;
        endSignal &= bitD2.reload() == StreamStatus::Unfinished;
        endSignal &= bitD3.reload() == StreamStatus::Unfinished;
        endSignal &= bitD4.reload() == StreamStatus::Unfinished;
    }
    HUFv07_decodeStreamX4(op1, &mut bitD1, dt, dtLog);
    HUFv07_decodeStreamX4(op2, &mut bitD2, dt, dtLog);
    HUFv07_decodeStreamX4(op3, &mut bitD3, dt, dtLog);
    HUFv07_decodeStreamX4(op4, &mut bitD4, dt, dtLog);
    if !(bitD1.is_empty() && bitD2.is_empty() && bitD3.is_empty() && bitD4.is_empty()) {
        return Err(Error::corruption_detected);
    }
    Ok(dstSize)
}
fn HUFv07_decompress4X4_DCtx(
    dctx: &mut HUFv07_DTable,
    dst: Writer<'_>,
    cSrc: &[u8],
) -> Result<usize, Error> {
    let hSize = HUFv07_readDTableX4(dctx, cSrc)?;
    if hSize >= cSrc.len() {
        return Err(Error::srcSize_wrong);
    }
    HUFv07_decompress4X4_usingDTable_internal(dst, &cSrc[hSize..], dctx)
}

#[repr(C)]
#[derive(Copy, Clone)]
struct algo_time_t {
    tableTime: u32,
    decode256Time: u32,
}
static algoTime: [[algo_time_t; 2]; 16] = [
    [
        algo_time_t {
            tableTime: 0,
            decode256Time: 0,
        },
        algo_time_t {
            tableTime: 1,
            decode256Time: 1,
        },
    ],
    [
        algo_time_t {
            tableTime: 0,
            decode256Time: 0,
        },
        algo_time_t {
            tableTime: 1,
            decode256Time: 1,
        },
    ],
    [
        algo_time_t {
            tableTime: 38,
            decode256Time: 130,
        },
        algo_time_t {
            tableTime: 1313,
            decode256Time: 74,
        },
    ],
    [
        algo_time_t {
            tableTime: 448,
            decode256Time: 128,
        },
        algo_time_t {
            tableTime: 1353,
            decode256Time: 74,
        },
    ],
    [
        algo_time_t {
            tableTime: 556,
            decode256Time: 128,
        },
        algo_time_t {
            tableTime: 1353,
            decode256Time: 74,
        },
    ],
    [
        algo_time_t {
            tableTime: 714,
            decode256Time: 128,
        },
        algo_time_t {
            tableTime: 1418,
            decode256Time: 74,
        },
    ],
    [
        algo_time_t {
            tableTime: 883,
            decode256Time: 128,
        },
        algo_time_t {
            tableTime: 1437,
            decode256Time: 74,
        },
    ],
    [
        algo_time_t {
            tableTime: 897,
            decode256Time: 128,
        },
        algo_time_t {
            tableTime: 1515,
            decode256Time: 75,
        },
    ],
    [
        algo_time_t {
            tableTime: 926,
            decode256Time: 128,
        },
        algo_time_t {
            tableTime: 1613,
            decode256Time: 75,
        },
    ],
    [
        algo_time_t {
            tableTime: 947,
            decode256Time: 128,
        },
        algo_time_t {
            tableTime: 1729,
            decode256Time: 77,
        },
    ],
    [
        algo_time_t {
            tableTime: 1107,
            decode256Time: 128,
        },
        algo_time_t {
            tableTime: 2083,
            decode256Time: 81,
        },
    ],
    [
        algo_time_t {
            tableTime: 1177,
            decode256Time: 128,
        },
        algo_time_t {
            tableTime: 2379,
            decode256Time: 87,
        },
    ],
    [
        algo_time_t {
            tableTime: 1242,
            decode256Time: 128,
        },
        algo_time_t {
            tableTime: 2415,
            decode256Time: 93,
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
    ],
    [
        algo_time_t {
            tableTime: 1455,
            decode256Time: 128,
        },
        algo_time_t {
            tableTime: 2422,
            decode256Time: 124,
        },
    ],
    [
        algo_time_t {
            tableTime: 722,
            decode256Time: 128,
        },
        algo_time_t {
            tableTime: 1891,
            decode256Time: 145,
        },
    ],
];
fn HUFv07_selectDecoder(dstSize: usize, cSrcSize: usize) -> bool {
    let Q = cSrcSize * 16 / dstSize; // Q < 16 since dstSize > cSrcSize
    let D256 = (dstSize >> 8) as u32;
    let [time0, time1] = algoTime[Q];
    let DTime0 = time0.tableTime + time0.decode256Time * D256;
    let mut DTime1 = time1.tableTime + time1.decode256Time * D256;
    DTime1 = DTime1 + (DTime1 >> 3); // advantage to algorithm using less memory, for cache eviction
    DTime1 < DTime0
}

fn HUFv07_decompress4X_hufOnly(
    dctx: &mut HUFv07_DTable,
    dst: Writer<'_>,
    cSrc: &[u8],
) -> Result<usize, Error> {
    if dst.capacity() == 0 {
        return Err(Error::dstSize_tooSmall);
    }
    if cSrc.len() >= dst.capacity() || cSrc.len() <= 1 {
        return Err(Error::corruption_detected);
    }
    if HUFv07_selectDecoder(dst.capacity(), cSrc.len()) {
        HUFv07_decompress4X4_DCtx(dctx, dst, cSrc)
    } else {
        HUFv07_decompress4X2_DCtx(dctx, dst, cSrc)
    }
}
const ZSTDv07_DICT_MAGIC: core::ffi::c_uint = 0xec30a437;
const ZSTDv07_REP_NUM: usize = 3;
const ZSTDv07_REP_INIT: usize = ZSTDv07_REP_NUM;
const repStartValue: [u32; ZSTDv07_REP_NUM] = [1, 4, 8];
const ZSTDv07_WINDOWLOG_ABSOLUTEMIN: core::ffi::c_int = 10;
static ZSTDv07_fcs_fieldSize: [usize; 4] = [0, 2, 4, 8];
static ZSTDv07_did_fieldSize: [usize; 4] = [0, 1, 2, 4];
const ZSTDv07_BLOCKHEADERSIZE: core::ffi::c_int = 3;
static ZSTDv07_blockHeaderSize: usize = ZSTDv07_BLOCKHEADERSIZE as usize;
const MIN_SEQUENCES_SIZE: core::ffi::c_int = 1;
const MIN_CBLOCK_SIZE: core::ffi::c_int = 1 + 1 + MIN_SEQUENCES_SIZE;
const LONGNBSEQ: core::ffi::c_int = 0x7f00;
const MINMATCH: usize = 3;
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
const WILDCOPY_OVERLENGTH: usize = 8;
#[inline]
unsafe fn ZSTDv07_wildcopy(dst: *mut u8, src: *const u8, length: isize) {
    let mut ip = src;
    let mut op = dst;
    let oend = op.offset(length);
    loop {
        core::ptr::copy_nonoverlapping(ip, op, 8);
        op = op.add(8);
        ip = ip.add(8);
        if op >= oend {
            break;
        }
    }
}
fn ZSTDv07_decompressBegin(dctx: &mut ZSTDv07_DCtx) {
    dctx.expected = ZSTDv07_frameHeaderSize_min;
    dctx.stage = ZSTDds_getFrameHeaderSize;
    dctx.previousDstEnd = core::ptr::null();
    dctx.base = core::ptr::null();
    dctx.vBase = core::ptr::null();
    dctx.dictEnd = core::ptr::null();
    dctx.hufTable.description = DTableDesc::default();
    dctx.fseEntropy = 0;
    dctx.litEntropy = dctx.fseEntropy;
    dctx.dictID = 0;
    dctx.rep = repStartValue;
}
pub(crate) unsafe fn ZSTDv07_createDCtx() -> *mut ZSTDv07_DCtx {
    let mut dctx = core::ptr::null_mut::<ZSTDv07_DCtx>();
    dctx = calloc(1, size_of::<ZSTDv07_DCtx>()) as *mut ZSTDv07_DCtx;
    if dctx.is_null() {
        return core::ptr::null_mut();
    }
    ZSTDv07_decompressBegin(&mut *dctx);
    dctx
}
pub(crate) unsafe fn ZSTDv07_freeDCtx(dctx: *mut ZSTDv07_DCtx) -> usize {
    if dctx.is_null() {
        return 0;
    }
    free(dctx as *mut core::ffi::c_void);
    0
}
fn ZSTDv07_frameHeaderSize(src: Reader<'_>) -> Result<usize, Error> {
    if src.len() < ZSTDv07_frameHeaderSize_min {
        return Err(Error::srcSize_wrong);
    }
    let src = src.subslice(..ZSTDv07_frameHeaderSize_min);
    let src = src.as_slice();
    let fhd = src[4];
    let dictID = (fhd & 3) as usize;
    let directMode = (fhd >> 5 & 1) as u32;
    let fcsId = (fhd >> 6) as usize;
    Ok(ZSTDv07_frameHeaderSize_min
        .wrapping_add((directMode == 0) as usize)
        .wrapping_add(ZSTDv07_did_fieldSize[dictID])
        .wrapping_add(ZSTDv07_fcs_fieldSize[fcsId])
        .wrapping_add(
            (directMode != 0 && ZSTDv07_fcs_fieldSize[fcsId] == 0) as core::ffi::c_int as usize,
        ))
}
pub(crate) fn ZSTDv07_getFrameParams(
    fparamsPtr: &mut ZSTDv07_frameParams,
    src: &[u8],
) -> Result<usize, Error> {
    if src.len() < ZSTDv07_frameHeaderSize_min {
        return Ok(ZSTDv07_frameHeaderSize_min);
    }
    *fparamsPtr = ZSTDv07_frameParams::default();

    let (magic, ip) = src.split_first_chunk::<4>().unwrap();
    let magic = u32::from_le_bytes(*magic);
    if magic != ZSTDv07_MAGICNUMBER {
        if magic & 0xfffffff0 == ZSTDv07_MAGIC_SKIPPABLE_START {
            if src.len() < ZSTDv07_skippableHeaderSize {
                return Ok(ZSTDv07_skippableHeaderSize);
            }
            fparamsPtr.frameContentSize =
                u32::from_le_bytes(*ip.first_chunk().unwrap()) as core::ffi::c_ulonglong;
            fparamsPtr.windowSize = 0;
            return Ok(0);
        }
        return Err(Error::prefix_unknown);
    }
    let fhsize = ZSTDv07_frameHeaderSize(Reader::from_slice(src))?;
    if src.len() < fhsize {
        return Ok(fhsize);
    }
    let fhdByte = ip[0];
    let mut pos = 1_usize;
    let dictIDSizeCode = (fhdByte & 3) as u32;
    let checksumFlag = (fhdByte >> 2 & 1) as u32;
    let directMode = (fhdByte >> 5 & 1) as u32;
    let fcsID = (fhdByte as core::ffi::c_int >> 6) as u32;
    let windowSizeMax = 1
        << (if MEM_32bits() {
            ZSTDv07_WINDOWLOG_MAX_32
        } else {
            ZSTDv07_WINDOWLOG_MAX_64
        }) as u32;
    let mut windowSize = 0u32;
    let mut dictID = 0;
    let mut frameContentSize = 0;
    if fhdByte & 0x8 != 0 {
        return Err(Error::frameParameter_unsupported);
    }
    if directMode == 0 {
        let wlByte = ip[pos];
        pos += 1;
        let windowLog = ((wlByte as core::ffi::c_int >> 3) + ZSTDv07_WINDOWLOG_ABSOLUTEMIN) as u32;
        if windowLog
            > (if MEM_32bits() {
                ZSTDv07_WINDOWLOG_MAX_32
            } else {
                ZSTDv07_WINDOWLOG_MAX_64
            }) as u32
        {
            return Err(Error::frameParameter_unsupported);
        }
        windowSize = 1 << windowLog;
        windowSize =
            windowSize.wrapping_add((windowSize >> 3) * (wlByte as core::ffi::c_int & 7) as u32);
    }
    match dictIDSizeCode {
        0 => {}
        1 => {
            dictID = u32::from(ip[pos]);
            pos += 1;
        }
        2 => {
            dictID = u32::from(u16::from_le_bytes(ip[pos..pos + 2].try_into().unwrap()));
            pos += 2;
        }
        3 => {
            dictID = u32::from_le_bytes(ip[pos..pos + 4].try_into().unwrap());
            pos += 4;
        }
        _ => unreachable!(),
    }
    match fcsID {
        0 => {
            if directMode != 0 {
                frameContentSize = ip[pos] as u64;
            }
        }
        1 => {
            frameContentSize = (u16::from_le_bytes(ip[pos..pos + 2].try_into().unwrap())
                as core::ffi::c_int
                + 256) as u64;
        }
        2 => {
            frameContentSize = u64::from(u32::from_le_bytes(ip[pos..pos + 4].try_into().unwrap()));
        }
        3 => {
            frameContentSize = u64::from_le_bytes(ip[pos..pos + 8].try_into().unwrap());
        }
        _ => unreachable!(),
    }
    if windowSize == 0 {
        windowSize = frameContentSize as u32;
    }
    if windowSize > windowSizeMax {
        return Err(Error::frameParameter_unsupported);
    }
    fparamsPtr.frameContentSize = frameContentSize as core::ffi::c_ulonglong;
    fparamsPtr.windowSize = windowSize;
    fparamsPtr.dictID = dictID;
    fparamsPtr.checksumFlag = checksumFlag;
    Ok(0)
}
unsafe fn ZSTDv07_decodeFrameHeader(dctx: *mut ZSTDv07_DCtx, src: &[u8]) -> Result<usize, Error> {
    let result = ZSTDv07_getFrameParams(&mut (*dctx).fParams, src);
    if (*dctx).fParams.dictID != 0 && (*dctx).dictID != (*dctx).fParams.dictID {
        return Err(Error::dictionary_wrong);
    }
    if (*dctx).fParams.checksumFlag != 0 {
        ZSTD_XXH64_reset(&mut (*dctx).xxhState, 0);
    }
    result
}
fn ZSTDv07_getcBlockSize(src: Reader<'_>, bpPtr: &mut blockProperties_t) -> Result<usize, Error> {
    let mut cSize: u32 = 0;
    if src.len() < ZSTDv07_blockHeaderSize {
        return Err(Error::srcSize_wrong);
    }
    let src = src.subslice(..ZSTDv07_blockHeaderSize);
    let src = src.as_slice();
    bpPtr.blockType = (src[0] >> 6) as blockType_t;
    cSize = u32::from(src[2]) + (u32::from(src[1]) << 8) + (u32::from(src[0] & 7) << 16);
    bpPtr.origSize = if bpPtr.blockType == bt_rle { cSize } else { 0 };
    if bpPtr.blockType == bt_end {
        return Ok(0);
    }
    if bpPtr.blockType == bt_rle {
        return Ok(1);
    }
    Ok(cSize as usize)
}
fn ZSTDv07_copyRawBlock(mut dst: Writer<'_>, src: &[u8]) -> Result<usize, Error> {
    if src.len() > dst.capacity() {
        return Err(Error::dstSize_tooSmall);
    }
    if !src.is_empty() {
        unsafe { ptr::copy_nonoverlapping(src.as_ptr(), dst.as_mut_ptr(), src.len()) };
    }
    Ok(src.len())
}

/// # Safety
///
/// `src` must outlive the last decompress call that covers the same compressed block.
unsafe fn ZSTDv07_decodeLiteralsBlock(dctx: &mut ZSTDv07_DCtx, src: &[u8]) -> Result<usize, Error> {
    if src.len() < MIN_CBLOCK_SIZE as usize {
        return Err(Error::corruption_detected);
    }
    match (src[0] >> 6) as litBlockType_t {
        0 => {
            let mut litSize: usize = 0;
            let mut litCSize: usize = 0;
            let mut singleStream = 0;
            let mut lhSize = usize::from((src[0] >> 4) & 3);
            if src.len() < 5 {
                return Err(Error::corruption_detected);
            }
            match lhSize {
                2 => {
                    lhSize = 4;
                    litSize = ((usize::from(src[0]) & 15) << 10)
                        + (usize::from(src[1]) << 2)
                        + (usize::from(src[2]) >> 6);
                    litCSize = ((usize::from(src[2]) & 63) << 8) + usize::from(src[3]);
                }
                3 => {
                    lhSize = 5;
                    litSize = ((usize::from(src[0]) & 15) << 14)
                        + (usize::from(src[1]) << 6)
                        + (usize::from(src[2]) >> 2);
                    litCSize = ((usize::from(src[2]) & 3) << 16)
                        + (usize::from(src[3]) << 8)
                        + usize::from(src[3]);
                }
                0 | 1 => {
                    lhSize = 3;
                    singleStream = usize::from(src[0]) & 16;
                    litSize = ((usize::from(src[0]) & 15) << 6) + (usize::from(src[1]) >> 2);
                    litCSize = ((usize::from(src[1]) & 3) << 8) + usize::from(src[2]);
                }
                _ => unreachable!(),
            }
            if litSize > ZSTDv07_BLOCKSIZE_ABSOLUTEMAX {
                return Err(Error::corruption_detected);
            }
            if litCSize.wrapping_add(lhSize) > src.len() {
                return Err(Error::corruption_detected);
            }
            if singleStream != 0 {
                HUFv07_decompress1X2_DCtx(
                    &mut dctx.hufTable,
                    Writer::from_slice(&mut dctx.litBuffer[..litSize]),
                    &src[lhSize..lhSize + litCSize],
                )
                .map_err(|_| Error::corruption_detected)?;
            } else {
                HUFv07_decompress4X_hufOnly(
                    &mut dctx.hufTable,
                    Writer::from_slice(&mut dctx.litBuffer[..litSize]),
                    &src[lhSize..lhSize + litCSize],
                )
                .map_err(|_| Error::corruption_detected)?;
            }
            dctx.litPtr = (&raw mut dctx.litBuffer).cast();
            dctx.litSize = litSize;
            dctx.litEntropy = 1;
            dctx.litBuffer[dctx.litSize..dctx.litSize + WILDCOPY_OVERLENGTH].fill(0);
            Ok(litCSize.wrapping_add(lhSize))
        }
        1 => {
            let mut litSize: usize = 0;
            let mut litCSize: usize = 0;
            let mut lhSize = usize::from((src[0] >> 4) & 3);
            if lhSize != 1 {
                return Err(Error::corruption_detected);
            }
            if dctx.litEntropy == 0 {
                return Err(Error::dictionary_corrupted);
            }
            lhSize = 3;
            litSize = ((usize::from(src[0]) & 15) << 6) + (usize::from(src[1]) >> 2);
            litCSize = ((usize::from(src[1]) & 3) << 8) + usize::from(src[2]);
            if litCSize.wrapping_add(lhSize) > src.len() {
                return Err(Error::corruption_detected);
            }
            HUFv07_decompress1X4_usingDTable(
                Writer::from_slice(&mut dctx.litBuffer[..litSize]),
                &src[lhSize..lhSize + litCSize],
                &dctx.hufTable,
            )
            .map_err(|_| Error::corruption_detected)?;
            dctx.litPtr = dctx.litBuffer.as_mut_ptr();
            dctx.litSize = litSize;
            dctx.litBuffer[dctx.litSize..dctx.litSize + WILDCOPY_OVERLENGTH].fill(0);
            Ok(litCSize + lhSize)
        }
        2 => {
            let mut litSize: usize = 0;
            let mut lhSize = usize::from((src[0] >> 4) & 3);
            match lhSize {
                2 => {
                    litSize = ((usize::from(src[0]) & 15) << 8) + usize::from(src[1]);
                }
                3 => {
                    litSize = ((usize::from(src[0]) & 15) << 16)
                        + (usize::from(src[1]) << 8)
                        + usize::from(src[2]);
                }
                0 | 1 => {
                    lhSize = 1;
                    litSize = usize::from(src[0]) & 31;
                }
                _ => unreachable!(),
            }
            if lhSize
                .wrapping_add(litSize)
                .wrapping_add(WILDCOPY_OVERLENGTH)
                > src.len()
            {
                if litSize.wrapping_add(lhSize) > src.len() {
                    return Err(Error::corruption_detected);
                }
                dctx.litBuffer[..litSize].copy_from_slice(&src[lhSize..lhSize + litSize]);
                dctx.litPtr = dctx.litBuffer.as_mut_ptr();
                dctx.litSize = litSize;
                dctx.litBuffer[dctx.litSize..dctx.litSize + WILDCOPY_OVERLENGTH].fill(0);
                return Ok(lhSize.wrapping_add(litSize));
            }
            dctx.litPtr = src[lhSize..].as_ptr();
            dctx.litSize = litSize;
            Ok(lhSize + litSize)
        }
        3 => {
            let mut litSize: usize = 0;
            let mut lhSize = usize::from((src[0] >> 4) & 3);
            match lhSize {
                2 => {
                    litSize = ((usize::from(src[0]) & 15) << 8) + usize::from(src[1]);
                }
                3 => {
                    litSize = ((usize::from(src[0]) & 15) << 16)
                        + (usize::from(src[1]) << 8)
                        + usize::from(src[2]);
                    if src.len() < 4 {
                        return Err(Error::corruption_detected);
                    }
                }
                0 | 1 => {
                    lhSize = 1;
                    litSize = usize::from(src[0]) & 31;
                }
                _ => unreachable!(),
            }
            if litSize > ZSTDv07_BLOCKSIZE_ABSOLUTEMAX {
                return Err(Error::corruption_detected);
            }
            dctx.litBuffer[..litSize + WILDCOPY_OVERLENGTH].fill(src[lhSize]);
            dctx.litPtr = dctx.litBuffer.as_mut_ptr();
            dctx.litSize = litSize;
            Ok(lhSize + 1)
        }
        _ => Err(Error::corruption_detected),
    }
}

fn ZSTDv07_buildSeqTable<const N: usize>(
    DTable: &mut FSEv07_DTable<N>,
    type_0: u32,
    mut max: u32,
    maxLog: u32,
    src: &[u8],
    defaultNorm: &[i16],
    defaultLog: u32,
    flagRepeatTable: u32,
) -> Result<usize, Error> {
    match type_0 {
        1 => {
            if src.is_empty() {
                return Err(Error::srcSize_wrong);
            }
            if src[0] as u32 > max {
                return Err(Error::corruption_detected);
            }
            FSEv07_buildDTable_rle(DTable, src[0]);
            Ok(1)
        }
        0 => {
            let _ = FSEv07_buildDTable(DTable, defaultNorm, max, defaultLog);
            Ok(0)
        }
        2 => {
            if flagRepeatTable == 0 {
                return Err(Error::corruption_detected);
            }
            Ok(0)
        }
        3 => {
            let mut tableLog: u32 = 0;
            let mut norm: [i16; 53] = [0; 53];
            let headerSize = unsafe { FSEv07_readNCount(&mut norm, &mut max, &mut tableLog, src) }
                .map_err(|_| Error::corruption_detected)?;

            if tableLog > maxLog {
                return Err(Error::corruption_detected);
            }
            let _ = FSEv07_buildDTable(DTable, &norm, max, tableLog);
            Ok(headerSize)
        }
        _ => unreachable!(),
    }
}
fn ZSTDv07_decodeSeqHeaders(
    nbSeqPtr: &mut core::ffi::c_int,
    DTableLL: &mut FSEv07_DTable<512>,
    DTableML: &mut FSEv07_DTable<512>,
    DTableOffb: &mut FSEv07_DTable<256>,
    flagRepeatTable: u32,
    src: &[u8],
) -> Result<usize, Error> {
    let mut ip = src;
    if src.len() < MIN_SEQUENCES_SIZE as usize {
        return Err(Error::srcSize_wrong);
    }
    let mut nbSeq = ip[0] as core::ffi::c_int;
    ip = &ip[1..];
    if nbSeq == 0 {
        *nbSeqPtr = 0;
        return Ok(1);
    }
    if nbSeq > 0x7f {
        if nbSeq == 0xff {
            if ip.len() < 2 {
                return Err(Error::srcSize_wrong);
            }
            nbSeq = u16::from_le_bytes(ip[..2].try_into().unwrap()) as core::ffi::c_int + LONGNBSEQ;
            ip = &ip[2..];
        } else {
            if ip.is_empty() {
                return Err(Error::srcSize_wrong);
            }
            nbSeq = ((nbSeq - 0x80) << 8) + ip[0] as core::ffi::c_int;
            ip = &ip[1..];
        }
    }
    *nbSeqPtr = nbSeq;
    if ip.len() < 4 {
        return Err(Error::srcSize_wrong);
    }
    let LLtype = (ip[0] >> 6) as u32;
    let OFtype = (ip[0] >> 4 & 3) as u32;
    let MLtype = (ip[0] >> 2 & 3) as u32;
    ip = &ip[1..];
    let llhSize = ZSTDv07_buildSeqTable(
        DTableLL,
        LLtype,
        MaxLL as u32,
        LLFSELog as u32,
        ip,
        &LL_defaultNorm,
        LL_defaultNormLog,
        flagRepeatTable,
    )
    .map_err(|_| Error::corruption_detected)?;
    ip = &ip[llhSize..];
    let ofhSize = ZSTDv07_buildSeqTable(
        DTableOffb,
        OFtype,
        MaxOff as u32,
        OffFSELog as u32,
        ip,
        &OF_defaultNorm,
        OF_defaultNormLog,
        flagRepeatTable,
    )
    .map_err(|_| Error::corruption_detected)?;
    ip = &ip[ofhSize..];
    let mlhSize = ZSTDv07_buildSeqTable(
        DTableML,
        MLtype,
        MaxML as u32,
        MLFSELog as u32,
        ip,
        &ML_defaultNorm,
        ML_defaultNormLog,
        flagRepeatTable,
    )
    .map_err(|_| Error::corruption_detected)?;
    ip = &ip[mlhSize..];
    Ok(src.len() - ip.len())
}
fn ZSTDv07_decodeSequence(seqState: &mut seqState_t) -> seq_t {
    let mut seq = seq_t {
        litLength: 0,
        matchLength: 0,
        offset: 0,
    };
    let llCode = FSEv07_peekSymbol(&seqState.stateLL) as u32;
    let mlCode = FSEv07_peekSymbol(&seqState.stateML) as u32;
    let ofCode = FSEv07_peekSymbol(&seqState.stateOffb) as u32;
    let llBits = LL_bits[llCode as usize];
    let mlBits = ML_bits[mlCode as usize];
    let ofBits = ofCode;
    let totalBits = llBits.wrapping_add(mlBits).wrapping_add(ofBits);
    static LL_base: [u32; 36] = [
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 18, 20, 22, 24, 28, 32, 40, 48,
        64, 0x80, 0x100, 0x200, 0x400, 0x800, 0x1000, 0x2000, 0x4000, 0x8000, 0x10000,
    ];
    static ML_base: [u32; 53] = [
        3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
        27, 28, 29, 30, 31, 32, 33, 34, 35, 37, 39, 41, 43, 47, 51, 59, 67, 83, 99, 0x83, 0x103,
        0x203, 0x403, 0x803, 0x1003, 0x2003, 0x4003, 0x8003, 0x10003,
    ];
    static OF_base: [u32; 29] = [
        0, 1, 1, 5, 0xd, 0x1d, 0x3d, 0x7d, 0xfd, 0x1fd, 0x3fd, 0x7fd, 0xffd, 0x1ffd, 0x3ffd,
        0x7ffd, 0xfffd, 0x1fffd, 0x3fffd, 0x7fffd, 0xffffd, 0x1ffffd, 0x3ffffd, 0x7ffffd, 0xfffffd,
        0x1fffffd, 0x3fffffd, 0x7fffffd, 0xffffffd,
    ];
    let mut offset: usize = 0;
    if ofCode == 0 {
        offset = 0;
    } else {
        offset =
            (OF_base[ofCode as usize] as usize).wrapping_add(seqState.DStream.read_bits(ofBits));
        if MEM_32bits() {
            seqState.DStream.reload();
        }
    }
    if ofCode <= 1 {
        if (llCode == 0) && (offset <= 1) {
            offset = 1_usize.wrapping_sub(offset);
        }
        if offset != 0 {
            let temp = seqState.prevOffset[offset];
            if offset != 1 {
                seqState.prevOffset[2] = seqState.prevOffset[1];
            }
            seqState.prevOffset[1] = seqState.prevOffset[0];
            offset = temp;
            seqState.prevOffset[0] = offset;
        } else {
            offset = seqState.prevOffset[0];
        }
    } else {
        seqState.prevOffset[2] = seqState.prevOffset[1];
        seqState.prevOffset[1] = seqState.prevOffset[0];
        seqState.prevOffset[0] = offset;
    }
    seq.offset = offset;
    seq.matchLength = (ML_base[mlCode as usize] as usize).wrapping_add(if mlCode > 31 {
        seqState.DStream.read_bits(mlBits)
    } else {
        0
    });
    if MEM_32bits() && mlBits.wrapping_add(llBits) > 24 {
        seqState.DStream.reload();
    }
    seq.litLength = (LL_base[llCode as usize] as usize).wrapping_add(if llCode > 15 {
        seqState.DStream.read_bits(llBits)
    } else {
        0
    });
    if MEM_32bits() || totalBits > (64 - 7 - (LLFSELog + MLFSELog + OffFSELog)) as u32 {
        seqState.DStream.reload();
    }
    FSEv07_updateState(&mut seqState.stateLL, &mut seqState.DStream);
    FSEv07_updateState(&mut seqState.stateML, &mut seqState.DStream);
    if MEM_32bits() {
        seqState.DStream.reload();
    }
    FSEv07_updateState(&mut seqState.stateOffb, &mut seqState.DStream);
    seq
}
unsafe fn ZSTDv07_execSequence(
    dst: Writer<'_>,
    mut sequence: seq_t,
    litPtr: &mut &[u8],
    base: *const u8,
    vBase: *const u8,
    dictEnd: *const u8,
) -> Result<usize, Error> {
    let mut op = dst;
    if sequence.litLength.wrapping_add(WILDCOPY_OVERLENGTH) > op.capacity() {
        return Err(Error::dstSize_tooSmall);
    }
    let mut oLitEnd = op.subslice(sequence.litLength..);
    let sequenceLength = sequence.litLength + sequence.matchLength;
    if sequenceLength > op.capacity() {
        return Err(Error::dstSize_tooSmall);
    }
    let mut oMatchEnd = op.subslice(sequenceLength..);
    let mut match_0: *const u8 = oLitEnd.as_ptr().wrapping_sub(sequence.offset);
    if sequence.litLength > litPtr.len() {
        return Err(Error::corruption_detected);
    }
    ZSTDv07_wildcopy(
        op.as_mut_ptr(),
        (*litPtr).as_ptr(),
        sequence.litLength as isize,
    );
    op = oLitEnd.subslice(..);
    *litPtr = &litPtr[sequence.litLength..];
    if sequence.offset > oLitEnd.as_ptr().offset_from_unsigned(base) {
        if sequence.offset > oLitEnd.as_ptr().offset_from_unsigned(vBase) {
            return Err(Error::corruption_detected);
        }
        match_0 = dictEnd.offset(-base.offset_from(match_0));
        if match_0.add(sequence.matchLength) <= dictEnd {
            core::ptr::copy(match_0, oLitEnd.as_mut_ptr(), sequence.matchLength);
            return Ok(sequenceLength);
        }
        let length1 = dictEnd.offset_from_unsigned(match_0);
        core::ptr::copy(match_0, oLitEnd.as_mut_ptr(), length1);
        op = oLitEnd.subslice(length1..);
        sequence.matchLength = sequence.matchLength.wrapping_sub(length1);
        match_0 = base;
        if op.capacity() < WILDCOPY_OVERLENGTH || sequence.matchLength < MINMATCH {
            while op.as_mut_ptr() < oMatchEnd.as_mut_ptr() {
                op.write_u8(*match_0);
                match_0 = match_0.add(1);
            }
            return Ok(sequenceLength);
        }
    }
    if sequence.offset < 8 {
        static dec32table: [u32; 8] = [0, 1, 2, 1, 4, 4, 4, 4];
        static dec64table: [u32; 8] = [8, 8, 8, 7, 8, 9, 10, 11];
        op.write_u8(*match_0);
        op.write_u8(*match_0.add(1));
        op.write_u8(*match_0.add(2));
        op.write_u8(*match_0.add(3));
        match_0 = match_0.add(dec32table[sequence.offset] as usize);
        ptr::copy_nonoverlapping(match_0, op.as_mut_ptr(), 4);
        match_0 = match_0.sub(dec64table[sequence.offset] as usize);
        op = op.subslice(4..);
    } else {
        ptr::copy_nonoverlapping(match_0, op.as_mut_ptr(), 8);
        op = op.subslice(8..);
    }
    match_0 = match_0.add(8);
    if oMatchEnd.capacity() < 16 - MINMATCH {
        if op.capacity() > WILDCOPY_OVERLENGTH {
            ZSTDv07_wildcopy(
                op.as_mut_ptr(),
                match_0,
                (op.capacity() - WILDCOPY_OVERLENGTH) as isize,
            );
            match_0 = match_0.add(op.capacity() - WILDCOPY_OVERLENGTH);
            op = op.subslice(op.capacity() - WILDCOPY_OVERLENGTH..);
        }
        while op.as_ptr() < oMatchEnd.as_ptr() {
            op.write_u8(*match_0);
            match_0 = match_0.add(1);
        }
    } else {
        ZSTDv07_wildcopy(op.as_mut_ptr(), match_0, sequence.matchLength as isize - 8);
    }
    Ok(sequenceLength)
}
fn ZSTDv07_decompressSequences(
    dctx: &mut ZSTDv07_DCtx,
    dst: Writer<'_>,
    seqStart: &[u8],
) -> Result<usize, Error> {
    let mut ip = seqStart;
    let dst_capacity = dst.capacity();
    let mut op = dst;
    let mut litPtr = unsafe { core::slice::from_raw_parts(dctx.litPtr, dctx.litSize) };
    let DTableLL = &mut dctx.LLTable;
    let DTableML = &mut dctx.MLTable;
    let DTableOffb = &mut dctx.OffTable;
    let base = dctx.base;
    let vBase = dctx.vBase;
    let dictEnd = dctx.dictEnd;
    let mut nbSeq: core::ffi::c_int = 0;
    let seqHSize = ZSTDv07_decodeSeqHeaders(
        &mut nbSeq,
        DTableLL,
        DTableML,
        DTableOffb,
        dctx.fseEntropy,
        ip,
    )?;
    ip = &ip[seqHSize..];
    if nbSeq != 0 {
        dctx.fseEntropy = 1;
        let prevOffset: [usize; ZSTDv07_REP_INIT] = core::array::from_fn(|i| dctx.rep[i] as usize);

        let mut DStream = match BITv07_DStream_t::new(ip) {
            Ok(DStream) => DStream,
            Err(_) => return Err(Error::corruption_detected),
        };
        let mut seqState = seqState_t {
            stateLL: FSEv07_initDState(&mut DStream, DTableLL),
            stateOffb: FSEv07_initDState(&mut DStream, DTableOffb),
            stateML: FSEv07_initDState(&mut DStream, DTableML),
            DStream,
            prevOffset,
        };

        while seqState.DStream.reload() <= StreamStatus::Completed && nbSeq != 0 {
            nbSeq -= 1;
            let sequence = ZSTDv07_decodeSequence(&mut seqState);
            let oneSeqSize = unsafe {
                ZSTDv07_execSequence(op.subslice(..), sequence, &mut litPtr, base, vBase, dictEnd)?
            };
            op = op.subslice(oneSeqSize..);
        }
        if nbSeq != 0 {
            return Err(Error::corruption_detected);
        }
        dctx.rep = core::array::from_fn(|i| seqState.prevOffset[i] as u32);
    }
    let lastLLSize = litPtr.len();
    if lastLLSize > op.capacity() {
        return Err(Error::dstSize_tooSmall);
    }
    if lastLLSize > 0 {
        unsafe { ptr::copy_nonoverlapping(litPtr.as_ptr(), op.as_mut_ptr(), lastLLSize) };
        op = op.subslice(lastLLSize..);
    }
    Ok(dst_capacity - op.capacity())
}
unsafe fn ZSTDv07_checkContinuity(dctx: &mut ZSTDv07_DCtx, dst: *const u8) {
    if dst != dctx.previousDstEnd {
        dctx.dictEnd = dctx.previousDstEnd;
        dctx.vBase = dst.offset(-dctx.previousDstEnd.offset_from(dctx.base));
        dctx.base = dst;
        dctx.previousDstEnd = dst;
    }
}

/// # Safety
///
/// `src` must outlive the last decompress call that covers the same compressed block.
unsafe fn ZSTDv07_decompressBlock_internal(
    dctx: &mut ZSTDv07_DCtx,
    dst: Writer<'_>,
    src: &[u8],
) -> Result<usize, Error> {
    let mut ip = src;
    if src.len() >= ZSTDv07_BLOCKSIZE_ABSOLUTEMAX {
        return Err(Error::srcSize_wrong);
    }
    let litCSize = ZSTDv07_decodeLiteralsBlock(dctx, src)?;
    ip = &ip[litCSize..];
    ZSTDv07_decompressSequences(dctx, dst, ip)
}

fn ZSTDv07_generateNxBytes(mut dst: Writer<'_>, byte: u8, length: usize) -> Result<usize, Error> {
    if length > dst.capacity() {
        return Err(Error::dstSize_tooSmall);
    }
    if length > 0 {
        unsafe { core::ptr::write_bytes(dst.as_mut_ptr(), byte, length) };
    }
    Ok(length)
}
fn ZSTDv07_decompressFrame(
    dctx: &mut ZSTDv07_DCtx,
    dst: Writer<'_>,
    src: Reader<'_>,
) -> Result<usize, Error> {
    let mut ip = src;
    let dstCapacity = dst.capacity();
    let mut op = dst;
    if ip.len() < ZSTDv07_frameHeaderSize_min.wrapping_add(ZSTDv07_blockHeaderSize) {
        return Err(Error::srcSize_wrong);
    }
    let frameHeaderSize = ZSTDv07_frameHeaderSize(ip.subslice(..))?;
    if ip.len() < frameHeaderSize.wrapping_add(ZSTDv07_blockHeaderSize) {
        return Err(Error::srcSize_wrong);
    }
    if unsafe { ZSTDv07_decodeFrameHeader(dctx, ip.subslice(..frameHeaderSize).as_slice()) }
        != Ok(0)
    {
        return Err(Error::corruption_detected);
    }
    ip = ip.subslice(frameHeaderSize..);
    loop {
        let mut blockProperties = blockProperties_t {
            blockType: bt_compressed,
            origSize: 0,
        };
        let cBlockSize = ZSTDv07_getcBlockSize(ip.subslice(..), &mut blockProperties)?;
        ip = ip.subslice(ZSTDv07_blockHeaderSize..);
        if cBlockSize > ip.len() {
            return Err(Error::srcSize_wrong);
        }
        let decodedSize = match blockProperties.blockType {
            bt_compressed => unsafe {
                ZSTDv07_decompressBlock_internal(
                    dctx,
                    op.subslice(..),
                    ip.subslice(..cBlockSize).as_slice(),
                )?
            },
            bt_raw => ZSTDv07_copyRawBlock(op.subslice(..), ip.subslice(..cBlockSize).as_slice())?,
            bt_rle => ZSTDv07_generateNxBytes(
                op.subslice(..),
                ip.subslice(..1).as_slice()[0],
                blockProperties.origSize as usize,
            )?,
            bt_end => {
                if !ip.is_empty() {
                    return Err(Error::srcSize_wrong);
                }
                break;
            }
            _ => return Err(Error::GENERIC),
        };
        if dctx.fParams.checksumFlag != 0 {
            ZSTD_XXH64_update_slice(&mut dctx.xxhState, unsafe {
                op.subslice(..decodedSize).as_slice()
            });
        }
        op = op.subslice(decodedSize..);
        ip = ip.subslice(cBlockSize..);
    }
    Ok(dstCapacity - op.capacity())
}
pub(crate) unsafe fn ZSTDv07_decompress_usingDict(
    dctx: &mut ZSTDv07_DCtx,
    dst: Writer<'_>,
    src: Reader<'_>,
    dict: *const core::ffi::c_void,
    dictSize: usize,
) -> Result<usize, Error> {
    let _ = ZSTDv07_decompressBegin_usingDict(dctx, dict, dictSize);
    ZSTDv07_checkContinuity(dctx, dst.as_ptr());
    ZSTDv07_decompressFrame(dctx, dst, src)
}
fn ZSTD_errorFrameSizeInfoLegacy(
    cSize: &mut usize,
    dBound: &mut core::ffi::c_ulonglong,
    ret: Error,
) {
    *cSize = ret.to_error_code();
    *dBound = ZSTD_CONTENTSIZE_ERROR;
}
pub(crate) fn ZSTDv07_findFrameSizeInfoLegacy(
    src: Reader<'_>,
    cSize: &mut usize,
    dBound: &mut core::ffi::c_ulonglong,
) {
    let srcSize = src.len();
    let mut ip = src;
    let mut nbBlocks = 0_usize;
    if ip.len() < ZSTDv07_frameHeaderSize_min.wrapping_add(ZSTDv07_blockHeaderSize) {
        ZSTD_errorFrameSizeInfoLegacy(cSize, dBound, Error::srcSize_wrong);
        return;
    }
    let frameHeaderSize = match ZSTDv07_frameHeaderSize(ip.subslice(..)) {
        Ok(frameHeaderSize) => frameHeaderSize,
        Err(err) => {
            ZSTD_errorFrameSizeInfoLegacy(cSize, dBound, err);
            return;
        }
    };
    if u32::from_le_bytes(ip.subslice(..4).as_slice().try_into().unwrap()) != ZSTDv07_MAGICNUMBER {
        ZSTD_errorFrameSizeInfoLegacy(cSize, dBound, Error::prefix_unknown);
        return;
    }
    if ip.len() < frameHeaderSize.wrapping_add(ZSTDv07_blockHeaderSize) {
        ZSTD_errorFrameSizeInfoLegacy(cSize, dBound, Error::srcSize_wrong);
        return;
    }
    ip = ip.subslice(frameHeaderSize..);
    loop {
        let mut blockProperties = blockProperties_t {
            blockType: bt_compressed,
            origSize: 0,
        };
        let cBlockSize = match ZSTDv07_getcBlockSize(ip.subslice(..), &mut blockProperties) {
            Ok(cBlockSize) => cBlockSize,
            Err(err) => {
                ZSTD_errorFrameSizeInfoLegacy(cSize, dBound, err);
                return;
            }
        };
        ip = ip.subslice(ZSTDv07_blockHeaderSize..);
        if blockProperties.blockType == bt_end {
            break;
        }
        if cBlockSize > ip.len() {
            ZSTD_errorFrameSizeInfoLegacy(cSize, dBound, Error::srcSize_wrong);
            return;
        }
        ip = ip.subslice(cBlockSize..);
        nbBlocks = nbBlocks.wrapping_add(1);
    }
    *cSize = srcSize - ip.len();
    *dBound = (nbBlocks * ZSTDv07_BLOCKSIZE_ABSOLUTEMAX) as core::ffi::c_ulonglong;
}
fn ZSTDv07_nextSrcSizeToDecompress(dctx: &ZSTDv07_DCtx) -> usize {
    dctx.expected
}
fn ZSTDv07_isSkipFrame(dctx: &ZSTDv07_DCtx) -> bool {
    dctx.stage == ZSTDds_skipFrame
}
unsafe fn ZSTDv07_decompressContinue(
    dctx: &mut ZSTDv07_DCtx,
    mut dst: Writer<'_>,
    src: Reader<'_>,
) -> Result<usize, Error> {
    if src.len() != dctx.expected {
        return Err(Error::srcSize_wrong);
    }
    if dst.capacity() != 0 {
        ZSTDv07_checkContinuity(dctx, dst.as_ptr());
    }
    match dctx.stage as core::ffi::c_uint {
        0 => {
            if src.len() != ZSTDv07_frameHeaderSize_min {
                return Err(Error::srcSize_wrong);
            }
            if u32::from_le_bytes(src.subslice(..4).as_slice().try_into().unwrap()) & 0xfffffff0
                == ZSTDv07_MAGIC_SKIPPABLE_START
            {
                ptr::copy_nonoverlapping(
                    src.as_ptr(),
                    dctx.headerBuffer.as_mut_ptr(),
                    ZSTDv07_frameHeaderSize_min,
                );
                dctx.expected =
                    ZSTDv07_skippableHeaderSize.wrapping_sub(ZSTDv07_frameHeaderSize_min);
                dctx.stage = ZSTDds_decodeSkippableHeader;
                return Ok(0);
            }
            dctx.headerSize = ZSTDv07_frameHeaderSize(src.subslice(..))?;
            ptr::copy_nonoverlapping(
                src.as_ptr(),
                dctx.headerBuffer.as_mut_ptr(),
                ZSTDv07_frameHeaderSize_min,
            );
            if dctx.headerSize > ZSTDv07_frameHeaderSize_min {
                dctx.expected = dctx.headerSize.wrapping_sub(ZSTDv07_frameHeaderSize_min);
                dctx.stage = ZSTDds_decodeFrameHeader;
                return Ok(0);
            }
            dctx.expected = 0;
        }
        1 => {}
        2 => {
            let mut bp = blockProperties_t {
                blockType: bt_compressed,
                origSize: 0,
            };
            let cBlockSize = ZSTDv07_getcBlockSize(src.subslice(..), &mut bp)?;
            if bp.blockType == bt_end {
                if dctx.fParams.checksumFlag != 0 {
                    let h64 = ZSTD_XXH64_digest(&mut dctx.xxhState);
                    let h32 = (h64 >> 11) as u32 & ((1 << 22) - 1) as u32;
                    let ip = src.subslice(..3);
                    let ip = ip.as_slice();
                    let check32 = u32::from(ip[2])
                        + (u32::from(ip[1]) << 8)
                        + ((u32::from(ip[0]) & 0x3f) << 16);
                    if check32 != h32 {
                        return Err(Error::checksum_wrong);
                    }
                }
                dctx.expected = 0;
                dctx.stage = ZSTDds_getFrameHeaderSize;
            } else {
                dctx.expected = cBlockSize;
                dctx.bType = bp.blockType;
                dctx.stage = ZSTDds_decompressBlock;
            }
            return Ok(0);
        }
        3 => {
            let rSize = match dctx.bType {
                0 => ZSTDv07_decompressBlock_internal(dctx, dst.subslice(..), src.as_slice()),
                1 => ZSTDv07_copyRawBlock(dst.subslice(..), src.as_slice()),
                2 => return Err(Error::GENERIC),
                3 => Ok(0),
                _ => return Err(Error::GENERIC),
            };
            dctx.stage = ZSTDds_decodeBlockHeader;
            dctx.expected = ZSTDv07_blockHeaderSize;
            let rSize = rSize?;
            dctx.previousDstEnd = dst.as_ptr().add(rSize);
            if dctx.fParams.checksumFlag != 0 {
                ZSTD_XXH64_update_slice(&mut dctx.xxhState, dst.subslice(..rSize).as_slice());
            }
            return Ok(rSize);
        }
        4 => {
            ptr::copy_nonoverlapping(
                src.as_ptr(),
                dctx.headerBuffer[ZSTDv07_frameHeaderSize_min..].as_mut_ptr(),
                dctx.expected,
            );
            dctx.expected =
                u32::from_le_bytes(dctx.headerBuffer[4..8].try_into().unwrap()) as usize;
            dctx.stage = ZSTDds_skipFrame;
            return Ok(0);
        }
        5 => {
            dctx.expected = 0;
            dctx.stage = ZSTDds_getFrameHeaderSize;
            return Ok(0);
        }
        _ => return Err(Error::GENERIC),
    }
    ptr::copy_nonoverlapping(
        src.as_ptr(),
        dctx.headerBuffer[ZSTDv07_frameHeaderSize_min..].as_mut_ptr(),
        dctx.expected,
    );
    ZSTDv07_decodeFrameHeader(dctx, &(&dctx.headerBuffer)[..dctx.headerSize])?;
    dctx.expected = ZSTDv07_blockHeaderSize;
    dctx.stage = ZSTDds_decodeBlockHeader;
    Ok(0)
}
unsafe fn ZSTDv07_refDictContent(dctx: &mut ZSTDv07_DCtx, dict: &[u8]) {
    dctx.dictEnd = dctx.previousDstEnd;
    dctx.vBase = dict
        .as_ptr()
        .offset(-dctx.previousDstEnd.offset_from(dctx.base));
    dctx.base = dict.as_ptr();
    dctx.previousDstEnd = dict.as_ptr().add(dict.len());
}
unsafe fn ZSTDv07_loadEntropy(dctx: &mut ZSTDv07_DCtx, mut dict: &[u8]) -> Result<usize, Error> {
    let dictSize = dict.len();
    let hSize =
        HUFv07_readDTableX4(&mut dctx.hufTable, dict).map_err(|_| Error::dictionary_corrupted)?;
    dict = &dict[hSize..];
    let mut offcodeNCount: [core::ffi::c_short; 29] = [0; 29];
    let mut offcodeMaxValue = MaxOff as u32;
    let mut offcodeLog: u32 = 0;
    let offcodeHeaderSize = FSEv07_readNCount(
        &mut offcodeNCount,
        &mut offcodeMaxValue,
        &mut offcodeLog,
        dict,
    )
    .map_err(|_| Error::dictionary_corrupted)?;
    if offcodeLog > OffFSELog as u32 {
        return Err(Error::dictionary_corrupted);
    }
    FSEv07_buildDTable(
        &mut dctx.OffTable,
        &offcodeNCount,
        offcodeMaxValue,
        offcodeLog,
    )
    .map_err(|_| Error::dictionary_corrupted)?;
    dict = &dict[offcodeHeaderSize..];
    let mut matchlengthNCount: [core::ffi::c_short; 53] = [0; 53];
    let mut matchlengthMaxValue = MaxML as core::ffi::c_uint;
    let mut matchlengthLog: core::ffi::c_uint = 0;
    let matchlengthHeaderSize = FSEv07_readNCount(
        &mut matchlengthNCount,
        &mut matchlengthMaxValue,
        &mut matchlengthLog,
        dict,
    )
    .map_err(|_| Error::dictionary_corrupted)?;
    if matchlengthLog > MLFSELog as core::ffi::c_uint {
        return Err(Error::dictionary_corrupted);
    }
    FSEv07_buildDTable(
        &mut dctx.MLTable,
        &matchlengthNCount,
        matchlengthMaxValue,
        matchlengthLog,
    )
    .map_err(|_| Error::dictionary_corrupted)?;
    dict = &dict[matchlengthHeaderSize..];
    let mut litlengthNCount: [core::ffi::c_short; 36] = [0; 36];
    let mut litlengthMaxValue = MaxLL as core::ffi::c_uint;
    let mut litlengthLog: core::ffi::c_uint = 0;
    let litlengthHeaderSize = FSEv07_readNCount(
        &mut litlengthNCount,
        &mut litlengthMaxValue,
        &mut litlengthLog,
        dict,
    )
    .map_err(|_| Error::dictionary_corrupted)?;
    if litlengthLog > LLFSELog as core::ffi::c_uint {
        return Err(Error::dictionary_corrupted);
    }
    FSEv07_buildDTable(
        &mut dctx.LLTable,
        &litlengthNCount,
        litlengthMaxValue,
        litlengthLog,
    )
    .map_err(|_| Error::dictionary_corrupted)?;
    dict = &dict[litlengthHeaderSize..];
    if dict.len() < 12 {
        return Err(Error::dictionary_corrupted);
    }
    *dctx.rep.as_mut_ptr() = MEM_readLE32(dict.as_ptr().cast());
    if *dctx.rep.as_mut_ptr() == 0 || *dctx.rep.as_mut_ptr() as usize >= dict.len() {
        return Err(Error::dictionary_corrupted);
    }
    *dctx.rep.as_mut_ptr().add(1) = MEM_readLE32(dict[4..].as_ptr().cast());
    if *dctx.rep.as_mut_ptr().add(1) == 0 || *dctx.rep.as_mut_ptr().add(1) as usize >= dict.len() {
        return Err(Error::dictionary_corrupted);
    }
    *dctx.rep.as_mut_ptr().add(2) = MEM_readLE32(dict[8..].as_ptr().cast());
    if *dctx.rep.as_mut_ptr().add(2) == 0 || *dctx.rep.as_mut_ptr().add(2) as usize >= dict.len() {
        return Err(Error::dictionary_corrupted);
    }
    dict = &dict[12..];
    dctx.fseEntropy = 1;
    dctx.litEntropy = dctx.fseEntropy;
    Ok(dictSize - dict.len())
}
unsafe fn ZSTDv07_decompress_insertDictionary(
    dctx: &mut ZSTDv07_DCtx,
    mut dict: &[u8],
) -> Result<(), Error> {
    if dict.len() < 8 {
        ZSTDv07_refDictContent(dctx, dict);
        return Ok(());
    }
    let magic = MEM_readLE32(dict.as_ptr().cast());
    if magic != ZSTDv07_DICT_MAGIC {
        ZSTDv07_refDictContent(dctx, dict);
        return Ok(());
    }
    dctx.dictID = MEM_readLE32(dict[4..].as_ptr() as *const core::ffi::c_void);
    dict = &dict[8..];
    let eSize = ZSTDv07_loadEntropy(dctx, dict).map_err(|_| Error::dictionary_corrupted)?;
    dict = &dict[eSize..];
    ZSTDv07_refDictContent(dctx, dict);
    Ok(())
}
unsafe fn ZSTDv07_decompressBegin_usingDict(
    dctx: &mut ZSTDv07_DCtx,
    dict: *const core::ffi::c_void,
    dictSize: usize,
) -> Result<(), Error> {
    ZSTDv07_decompressBegin(dctx);
    if !dict.is_null() && dictSize != 0 {
        ZSTDv07_decompress_insertDictionary(
            &mut *dctx,
            core::slice::from_raw_parts(dict.cast::<u8>(), dictSize),
        )
        .map_err(|_| Error::dictionary_corrupted)?;
    }
    Ok(())
}
pub(crate) unsafe fn ZBUFFv07_createDCtx() -> *mut ZBUFFv07_DCtx {
    let mut zbd = core::ptr::null_mut::<ZBUFFv07_DCtx>();
    zbd = calloc(1, size_of::<ZBUFFv07_DCtx>()) as *mut ZBUFFv07_DCtx;
    if zbd.is_null() {
        return core::ptr::null_mut();
    }
    (*zbd).zd = ZSTDv07_createDCtx();
    if ((*zbd).zd).is_null() {
        ZBUFFv07_freeDCtx(zbd);
        return core::ptr::null_mut();
    }
    (*zbd).stage = ZBUFFds_init;
    zbd
}
pub(crate) unsafe fn ZBUFFv07_freeDCtx(zbd: *mut ZBUFFv07_DCtx) -> usize {
    if zbd.is_null() {
        return 0;
    }
    ZSTDv07_freeDCtx((*zbd).zd);
    if !((*zbd).inBuff).is_null() {
        free((*zbd).inBuff as *mut core::ffi::c_void);
    }
    if !((*zbd).outBuff).is_null() {
        free((*zbd).outBuff as *mut core::ffi::c_void);
    }
    free(zbd as *mut core::ffi::c_void);
    0
}
pub(crate) unsafe fn ZBUFFv07_decompressInitDictionary(
    zbd: &mut ZBUFFv07_DCtx,
    dict: *const core::ffi::c_void,
    dictSize: usize,
) -> Result<(), Error> {
    zbd.stage = ZBUFFds_loadHeader;
    zbd.outEnd = 0;
    zbd.outStart = zbd.outEnd;
    zbd.inPos = zbd.outStart;
    zbd.lhSize = zbd.inPos;
    ZSTDv07_decompressBegin_usingDict(&mut *zbd.zd, dict, dictSize)
}
#[inline]
unsafe fn ZBUFFv07_limitCopy(
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
        ptr::copy_nonoverlapping(src, dst, length);
    }
    length
}

#[allow(clippy::drop_non_drop)]
pub(crate) unsafe fn ZBUFFv07_decompressContinue(
    zbd: &mut ZBUFFv07_DCtx,
    dst: *mut core::ffi::c_void,
    dstCapacityPtr: *mut usize,
    src: *const core::ffi::c_void,
    srcSizePtr: *mut usize,
) -> Result<usize, Error> {
    let istart = src as *const u8;
    let iend = istart.add(*srcSizePtr);
    let mut ip = istart;
    let ostart = dst as *mut u8;
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
        match zbd.stage {
            ZBUFFds_init => return Err(Error::init_missing),
            ZBUFFds_loadHeader => {
                let hSize =
                    ZSTDv07_getFrameParams(&mut zbd.fParams, &zbd.headerBuffer[..zbd.lhSize])?;
                if hSize != 0 {
                    // if hSize!=0, hSize > zbd->lhSize
                    let toLoad = hSize - zbd.lhSize;
                    if toLoad > iend.offset_from_unsigned(ip) {
                        // not enough input to load full header
                        if !ip.is_null() {
                            ptr::copy_nonoverlapping(
                                ip,
                                zbd.headerBuffer.as_mut_ptr().add(zbd.lhSize),
                                iend.offset_from_unsigned(ip),
                            );
                        }
                        zbd.lhSize = zbd.lhSize.wrapping_add(iend.offset_from_unsigned(ip));
                        *dstCapacityPtr = 0;
                        return Ok(hSize
                            .wrapping_sub(zbd.lhSize)
                            .wrapping_add(ZSTDv07_blockHeaderSize));
                    }
                    ptr::copy_nonoverlapping(
                        ip,
                        zbd.headerBuffer.as_mut_ptr().add(zbd.lhSize),
                        toLoad,
                    );
                    zbd.lhSize = hSize;
                    ip = ip.add(toLoad);
                    continue;
                }

                // Consume header
                let h1Size = ZSTDv07_nextSrcSizeToDecompress(&*zbd.zd); // == ZSTDv07_frameHeaderSize_min
                ZSTDv07_decompressContinue(
                    &mut *zbd.zd,
                    Writer::from_slice(&mut []),
                    Reader::from_raw_parts(zbd.headerBuffer.as_ptr(), h1Size),
                )?;
                if h1Size < zbd.lhSize {
                    // long header
                    let h2Size = ZSTDv07_nextSrcSizeToDecompress(&*zbd.zd);
                    ZSTDv07_decompressContinue(
                        &mut *zbd.zd,
                        Writer::from_slice(&mut []),
                        Reader::from_raw_parts(zbd.headerBuffer.as_ptr().add(h1Size), h2Size),
                    )?;
                }
                zbd.fParams.windowSize = core::cmp::max(zbd.fParams.windowSize, 1 << 10);

                // Frame header instruct buffer sizes
                let blockSize = core::cmp::min(zbd.fParams.windowSize, 128 * 1024) as usize;
                zbd.blockSize = blockSize;
                if zbd.inBuffSize < blockSize {
                    free(zbd.inBuff as *mut core::ffi::c_void);
                    zbd.inBuffSize = blockSize;
                    zbd.inBuff = malloc(blockSize) as *mut u8;
                    if zbd.inBuff.is_null() {
                        return Err(Error::memory_allocation);
                    }
                }
                let neededOutSize = (zbd.fParams.windowSize as usize)
                    .wrapping_add(blockSize)
                    .wrapping_add(WILDCOPY_OVERLENGTH * 2);
                if zbd.outBuffSize < neededOutSize {
                    free(zbd.outBuff as *mut core::ffi::c_void);
                    zbd.outBuffSize = neededOutSize;
                    zbd.outBuff = malloc(neededOutSize) as *mut u8;
                    if zbd.outBuff.is_null() {
                        return Err(Error::memory_allocation);
                    }
                }
                zbd.stage = ZBUFFds_read;
                current_block = Block::Read;
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
            _ => return Err(Error::GENERIC),
        }
        if current_block == Block::Read {
            drop(current_block);

            let neededInSize = ZSTDv07_nextSrcSizeToDecompress(&*zbd.zd);
            if neededInSize == 0 {
                // end of frame
                zbd.stage = ZBUFFds_init;
                notDone = 0;
                continue;
            }
            if iend.offset_from_unsigned(ip) >= neededInSize {
                // decode directly from src
                let isSkipFrame = ZSTDv07_isSkipFrame(&*zbd.zd);
                let decodedSize = ZSTDv07_decompressContinue(
                    &mut *zbd.zd,
                    if isSkipFrame {
                        Writer::from_slice(&mut [])
                    } else {
                        Writer::from_raw_parts(zbd.outBuff, zbd.outBuffSize)
                            .subslice(zbd.outStart..)
                    },
                    Reader::from_raw_parts(ip, neededInSize),
                )?;
                ip = ip.add(neededInSize);
                if decodedSize == 0 && !isSkipFrame {
                    // this was just a header
                    continue;
                }
                zbd.outEnd = zbd.outStart.wrapping_add(decodedSize);
                zbd.stage = ZBUFFds_flush;
                continue;
            } else if ip == iend {
                // no more input
                notDone = 0;
                continue;
            }
            zbd.stage = ZBUFFds_load;
            current_block = Block::Load;
        }

        if current_block == Block::Load {
            drop(current_block);
            let neededInSize_0 = ZSTDv07_nextSrcSizeToDecompress(&*zbd.zd);
            // should always be <= remaining space within inBuff
            let toLoad = neededInSize_0.wrapping_sub(zbd.inPos);
            let mut loadedSize: usize = 0;
            if toLoad > zbd.inBuffSize.wrapping_sub(zbd.inPos) {
                return Err(Error::corruption_detected); // should never happen
            }
            loadedSize = ZBUFFv07_limitCopy(
                zbd.inBuff.add(zbd.inPos) as *mut core::ffi::c_void,
                toLoad,
                ip as *const core::ffi::c_void,
                iend.offset_from_unsigned(ip),
            );
            ip = ip.add(loadedSize);
            zbd.inPos = zbd.inPos.wrapping_add(loadedSize);
            if loadedSize < toLoad {
                // not enough input, wait for more
                notDone = 0;
                continue;
            }

            // decode loaded input
            let isSkipFrame_0 = ZSTDv07_isSkipFrame(&*zbd.zd);
            let decodedSize_0 = ZSTDv07_decompressContinue(
                &mut *zbd.zd,
                Writer::from_raw_parts(zbd.outBuff, zbd.outBuffSize).subslice(zbd.outStart..),
                Reader::from_raw_parts(zbd.inBuff, neededInSize_0),
            )?;
            zbd.inPos = 0; // input is consumed
            if decodedSize_0 == 0 && !isSkipFrame_0 {
                zbd.stage = ZBUFFds_read;
                continue;
            }
            zbd.outEnd = zbd.outStart.wrapping_add(decodedSize_0);
            zbd.stage = ZBUFFds_flush;
            current_block = Block::Flush;
        }

        if current_block == Block::Flush {
            drop(current_block);

            let toFlushSize = zbd.outEnd.wrapping_sub(zbd.outStart);
            let flushedSize = ZBUFFv07_limitCopy(
                op as *mut core::ffi::c_void,
                oend.offset_from_unsigned(op),
                zbd.outBuff.add(zbd.outStart) as *const core::ffi::c_void,
                toFlushSize,
            );
            op = op.add(flushedSize);
            zbd.outStart = zbd.outStart.wrapping_add(flushedSize);
            if flushedSize == toFlushSize {
                zbd.stage = ZBUFFds_read;
                if zbd.outStart.wrapping_add(zbd.blockSize) > zbd.outBuffSize {
                    zbd.outEnd = 0;
                    zbd.outStart = zbd.outEnd;
                }
            }
            // cannot flush everything
            notDone = 0;
        }
    }

    // result
    *srcSizePtr = ip.offset_from_unsigned(istart);
    *dstCapacityPtr = op.offset_from_unsigned(ostart);
    let mut nextSrcSizeHint = ZSTDv07_nextSrcSizeToDecompress(&*zbd.zd);
    nextSrcSizeHint = nextSrcSizeHint.wrapping_sub(zbd.inPos); // already loaded
    Ok(nextSrcSizeHint)
}
