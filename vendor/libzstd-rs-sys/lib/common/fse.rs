use libc::ptrdiff_t;

use crate::lib::common::bitstream::{BIT_CStream_t, BitContainerType};

pub(crate) type FSE_CTable = core::ffi::c_uint;

pub(crate) const FSE_NCOUNTBOUND: usize = 512;

pub(crate) const fn FSE_CTABLE_SIZE_U32(maxTableLog: usize, maxSymbolValue: usize) -> usize {
    1 + (1 << (maxTableLog - 1)) + ((maxSymbolValue + 1) * 2)
}

pub(crate) const fn FSE_DTABLE_SIZE_U32(maxTableLog: usize) -> usize {
    1 + (1 << maxTableLog)
}

pub(crate) const fn FSE_BUILD_CTABLE_WORKSPACE_SIZE_U32(
    maxSymbolValue: usize,
    tableLog: usize,
) -> usize {
    ((maxSymbolValue + 2) + (1 << tableLog)) / 2 + size_of::<u64>() / size_of::<u32>()
    /* additional 8 bytes for potential table overwrite */
}

pub(crate) const fn FSE_BUILD_CTABLE_WORKSPACE_SIZE(
    maxSymbolValue: usize,
    tableLog: usize,
) -> usize {
    size_of::<core::ffi::c_uint>() * FSE_BUILD_CTABLE_WORKSPACE_SIZE_U32(maxSymbolValue, tableLog)
}

pub(crate) const fn FSE_BUILD_DTABLE_WKSP_SIZE(maxTableLog: usize, maxSymbolValue: usize) -> usize {
    size_of::<u16>() * (maxSymbolValue + 1) + (1 << maxTableLog) + 8
}

pub(crate) const fn FSE_DECOMPRESS_WKSP_SIZE_U32(
    maxTableLog: usize,
    maxSymbolValue: usize,
) -> usize {
    FSE_DTABLE_SIZE_U32(maxTableLog)
        + 1
        + FSE_BUILD_DTABLE_WKSP_SIZE(maxTableLog, maxSymbolValue).div_ceil(size_of::<u32>())
        + (FSE_MAX_SYMBOL_VALUE as usize).div_ceil(2)
        + 1
}

pub(crate) const fn FSE_DECOMPRESS_WKSP_SIZE(maxTableLog: usize, maxSymbolValue: usize) -> usize {
    size_of::<core::ffi::c_uint>() * FSE_DECOMPRESS_WKSP_SIZE_U32(maxTableLog, maxSymbolValue)
}

#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq, Default)]
pub enum FSE_repeat {
    /// Cannot use the previous table
    #[default]
    None = 0,
    /// Can use the previous table but it must be checked
    Check = 1,
    /// Can use the previous table and it is assumed to be valid
    Valid = 2,
}

impl FSE_repeat {
    /// Downgrades [`FSE_repeat::Valid`] to [`FSE_repeat::Check`], leaves other states unchanged
    #[inline]
    pub(crate) fn require_check(&mut self) {
        if *self == FSE_repeat::Valid {
            *self = FSE_repeat::Check;
        }
    }
}

#[repr(C)]
pub struct FSE_CState_t<'a> {
    pub value: ptrdiff_t,
    pub stateTable: &'a [u16],
    pub symbolTT: &'a [FSE_symbolCompressionTransform],
    pub stateLog: core::ffi::c_uint,
}

#[derive(Copy, Clone)]
#[repr(C)]
pub(crate) struct FSE_symbolCompressionTransform {
    pub(crate) deltaFindState: core::ffi::c_int,
    pub(crate) deltaNbBits: u32,
}

/// Pack two consecutive `u16` values into one `u32`.
#[inline]
pub(crate) fn FSE_writeU16Pair(first: u16, second: u16) -> FSE_CTable {
    let [a, b] = first.to_ne_bytes();
    let [c, d] = second.to_ne_bytes();

    u32::from_ne_bytes([a, b, c, d])
}

/// Read the `index`th `u16` of a `&[u32]`.
#[inline]
fn FSE_readU16(ct: &[FSE_CTable], index: usize) -> u16 {
    let bytes = ct[index / 2].to_ne_bytes();
    if index.is_multiple_of(2) {
        u16::from_ne_bytes([bytes[0], bytes[1]])
    } else {
        u16::from_ne_bytes([bytes[2], bytes[3]])
    }
}

impl<'a> FSE_CState_t<'a> {
    #[inline]
    pub fn new(ct: &'a [FSE_CTable]) -> Self {
        // the table header occupies the first two bytes of `ct`
        let tableLog = FSE_readU16(ct, 0) as u32;

        // the state table follows the header
        let stateTable = &ct[1..];
        let symbolTT = &ct[FSE_symbolTTIndex(tableLog)..];

        // SAFETY: `u16` has a smaller alignment than `u32`, and any bit pattern is valid.
        let stateTable: &[u16] = unsafe {
            core::slice::from_raw_parts(stateTable.as_ptr().cast(), stateTable.len() * 2)
        };

        const _: () = assert!(size_of::<FSE_symbolCompressionTransform>() == size_of::<[u32; 2]>());
        const _: () =
            assert!(align_of::<FSE_symbolCompressionTransform>() <= align_of::<[u32; 2]>());

        // SAFETY: `FSE_symbolCompressionTransform` is a `#[repr(C)]` pair of 32-bit integers, so
        // it has the same size as `[u32; 2]`, no padding, and any bit pattern is valid.
        let (pairs, _) = symbolTT.as_chunks::<2>();
        let symbolTT: &[FSE_symbolCompressionTransform] =
            unsafe { core::slice::from_raw_parts(pairs.as_ptr().cast(), pairs.len()) };

        FSE_CState_t {
            value: 1 << tableLog,
            stateTable,
            symbolTT,
            stateLog: tableLog,
        }
    }

    /// Same as [`Self::new`], but the first symbol to include (which will be the last to be read)
    /// uses the smallest state value possible, saving the cost of this symbol
    #[inline]
    pub fn new2(ct: &'a [FSE_CTable], symbol: u32) -> Self {
        let mut state = FSE_CState_t::new(ct);
        let symbolTT = state.symbolTT[symbol as usize];
        let nbBitsOut = symbolTT.deltaNbBits.wrapping_add(1 << 15) >> 16;
        let value = (nbBitsOut << 16).wrapping_sub(symbolTT.deltaNbBits) as ptrdiff_t;

        let index = (value >> nbBitsOut) + symbolTT.deltaFindState as ptrdiff_t;
        state.value = state.stateTable[index as usize] as ptrdiff_t;
        state
    }
}

#[inline]
pub(crate) const fn FSE_symbolTTIndex(tableLog: u32) -> usize {
    let skip_header = 1;

    match tableLog {
        0 => skip_header + 1,
        _ => skip_header + (1 << (tableLog - 1)),
    }
}

#[inline]
pub(crate) fn FSE_encodeSymbol(
    bitC: &mut BIT_CStream_t,
    state: &mut FSE_CState_t,
    symbol: core::ffi::c_uint,
) {
    let symbolTT = state.symbolTT[symbol as usize];
    let nbBitsOut = ((state.value + symbolTT.deltaNbBits as ptrdiff_t) >> 16) as u32;
    bitC.add_bits(state.value as BitContainerType, nbBitsOut);
    let index = (state.value >> nbBitsOut) + symbolTT.deltaFindState as ptrdiff_t;
    state.value = state.stateTable[index as usize] as isize;
}

#[inline]
pub(crate) unsafe fn FSE_flushCState(bitC: &mut BIT_CStream_t, statePtr: &FSE_CState_t) {
    bitC.add_bits(statePtr.value as BitContainerType, statePtr.stateLog);
    bitC.flush_bits();
}

#[inline]
pub(crate) fn FSE_getMaxNbBits(
    symbolTT: &[FSE_symbolCompressionTransform],
    symbolValue: u32,
) -> u32 {
    (symbolTT[symbolValue as usize].deltaNbBits).wrapping_add(((1 << 16) - 1) as u32) >> 16
}

#[inline]
pub(crate) fn FSE_bitCost(
    symbolTT: &[FSE_symbolCompressionTransform],
    tableLog: u32,
    symbolValue: u32,
    accuracyLog: u32,
) -> u32 {
    let minNbBits = symbolTT[symbolValue as usize].deltaNbBits >> 16;
    let threshold = minNbBits.wrapping_add(1) << 16;
    let tableSize = (1 << tableLog) as u32;
    let deltaFromThreshold = threshold
        .wrapping_sub((symbolTT[symbolValue as usize].deltaNbBits).wrapping_add(tableSize));
    let normalizedDeltaFromThreshold = deltaFromThreshold << accuracyLog >> tableLog;
    let bitMultiplier = (1 << accuracyLog) as u32;
    (minNbBits.wrapping_add(1) * bitMultiplier).wrapping_sub(normalizedDeltaFromThreshold)
}

#[derive(Copy, Clone, Debug, PartialEq, Default)]
#[repr(C)]
pub(crate) struct FSE_DTableHeader {
    pub(crate) tableLog: u16,
    pub(crate) fastMode: u16,
}

#[derive(Copy, Clone, Debug, PartialEq, Default)]
#[repr(C)]
pub(crate) struct FSE_decode_t {
    pub(crate) newState: u16,
    pub(crate) symbol: u8,
    pub(crate) nbBits: u8,
}

pub const FSE_MAX_MEMORY_USAGE: core::ffi::c_int = 14;
pub const FSE_DEFAULT_MEMORY_USAGE: core::ffi::c_int = 13;
const _: () = assert!(
    FSE_DEFAULT_MEMORY_USAGE <= FSE_MAX_MEMORY_USAGE,
    "FSE_DEFAULT_MEMORY_USAGE must be <= FSE_MAX_MEMORY_USAGE"
);

/// Maximum symbol value authorized.
pub(crate) const FSE_MAX_SYMBOL_VALUE: u8 = 255;

pub(crate) const FSE_MAX_TABLELOG: core::ffi::c_int = FSE_MAX_MEMORY_USAGE - 2;
pub(crate) const FSE_DEFAULT_TABLELOG: core::ffi::c_int = FSE_DEFAULT_MEMORY_USAGE - 2;
pub(crate) const FSE_MIN_TABLELOG: core::ffi::c_int = 5;

pub(crate) const FSE_TABLELOG_ABSOLUTE_MAX: core::ffi::c_int = 15;
const _: () = assert!(
    FSE_MAX_TABLELOG <= FSE_TABLELOG_ABSOLUTE_MAX,
    "FSE_MAX_TABLELOG > FSE_TABLELOG_ABSOLUTE_MAX is not supported"
);

#[inline]
pub(crate) const fn FSE_TABLESTEP(tableSize: usize) -> usize {
    (tableSize >> 1) + (tableSize >> 3) + 3
}
