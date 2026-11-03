use crate::lib::common::bits::ZSTD_highbit32;
use crate::lib::common::bitstream::{BIT_CStream_t, BitContainerType};
use crate::lib::common::error_private::Error;
use crate::lib::common::fse::{
    FSE_CState_t, FSE_CTable, FSE_encodeSymbol, FSE_flushCState, FSE_symbolCompressionTransform,
    FSE_symbolTTIndex, FSE_writeU16Pair, FSE_BUILD_CTABLE_WORKSPACE_SIZE, FSE_DEFAULT_TABLELOG,
    FSE_MAX_TABLELOG, FSE_MIN_TABLELOG, FSE_NCOUNTBOUND, FSE_TABLESTEP,
};
use crate::lib::common::mem::MEM_write64;

/// Same as FSE_buildCTable(), but using an externally allocated scratch buffer (`workSpace`).
/// wkspSize should be sized to handle worst case situation, which is `1<<max_tableLog * sizeof(FSE_FUNCTION_TYPE)`
/// workSpace must also be properly aligned with FSE_FUNCTION_TYPE requirements
pub(crate) unsafe fn FSE_buildCTable_wksp(
    ct: &mut [FSE_CTable],
    normalizedCounter: &[core::ffi::c_short],
    maxSymbolValue: u8,
    tableLog: core::ffi::c_uint,
    workSpace: *mut core::ffi::c_void,
    wkspSize: usize,
) -> Result<(), Error> {
    let tableSize = 1usize << tableLog;
    let tableMask = tableSize.wrapping_sub(1);
    let ptr = ct.as_mut_ptr() as *mut core::ffi::c_void;
    let tableU16 = (ptr as *mut u16).add(2);
    let FSCT = (ptr as *mut u32)
        .add(1) // header
        .add(if tableLog != 0 { tableSize >> 1 } else { 1 })
        as *mut core::ffi::c_void;
    let symbolTT = FSCT as *mut FSE_symbolCompressionTransform;
    let step = FSE_TABLESTEP(tableSize);
    let maxSV1 = usize::from(maxSymbolValue) + 1;

    let cumul = workSpace as *mut u16;
    let tableSymbol = cumul.add(maxSV1 + 1) as *mut u8;

    let mut highThreshold = tableSize.wrapping_sub(1);

    if FSE_BUILD_CTABLE_WORKSPACE_SIZE(usize::from(maxSymbolValue), tableLog as usize) > wkspSize {
        return Err(Error::tableLog_tooLarge);
    }

    // CTable header
    *tableU16.sub(2) = tableLog as u16;
    *tableU16.sub(1) = u16::from(maxSymbolValue);

    // For explanations on how to distribute symbol values over the table :
    // https://fastcompression.blogspot.fr/2014/02/fse-distributing-symbol-values.html

    // symbol start positions
    *cumul = 0;
    for u in 1..maxSV1 + 1 {
        if normalizedCounter[u - 1] == -1 {
            // Low proba symbol
            *cumul.add(u) = (*cumul.add(u - 1) as core::ffi::c_int + 1) as u16;
            *tableSymbol.add(highThreshold) = (u - 1) as u8;
            highThreshold = highThreshold.wrapping_sub(1);
        } else {
            *cumul.add(u) = (*cumul.add(u - 1) as core::ffi::c_int
                + normalizedCounter[u - 1] as u16 as core::ffi::c_int)
                as u16;
        }
    }
    *cumul.add(maxSV1) = tableSize.wrapping_add(1) as u16;

    // Spread symbols
    if highThreshold == tableSize.wrapping_sub(1) {
        // Case for no low prob count symbols. Lay down 8 bytes at a time
        // to reduce branch misses since we are operating on a small block
        let spread = tableSymbol.add(tableSize);
        let add = 0x101010101010101u64;
        let mut pos = 0usize;
        let mut sv = 0u64;
        for &count in &normalizedCounter[..maxSV1] {
            let n = count as usize;
            // TODO: rewrite these writes similar to the one in FSE_buildDTable_internal
            // when spread becomes a slice
            MEM_write64(spread.add(pos) as *mut core::ffi::c_void, sv);
            for i in (8..n).step_by(8) {
                MEM_write64(spread.add(pos).add(i) as *mut core::ffi::c_void, sv);
            }
            pos = pos.wrapping_add(n);
            sv = sv.wrapping_add(add);
        }

        // Spread symbols across the table. Lack of lowprob symbols means that
        // we don't need variable sized inner loop, so we can unroll the loop and
        // reduce branch misses.
        let mut position = 0usize;
        let unroll = 2; // Experimentally determined optimal unroll
        for s in (0..tableSize).step_by(unroll) {
            for u_0 in 0..unroll {
                let uPosition = position.wrapping_add(u_0 * step) & tableMask;
                *tableSymbol.add(uPosition) = *spread.add(s.wrapping_add(u_0));
            }
            position = position.wrapping_add(unroll * step) & tableMask;
        }
    } else {
        let mut position = 0usize;
        for (symbol, &freq) in normalizedCounter[..maxSV1].iter().enumerate() {
            for _ in 0..freq {
                *tableSymbol.add(position) = symbol as u8;
                position = position.wrapping_add(step) & tableMask;
                while position > highThreshold {
                    position = position.wrapping_add(step) & tableMask; // Low proba area
                }
            }
        }
    }

    // Build table
    for u in 0..tableSize {
        let s = *tableSymbol.add(u);
        let fresh1 = &mut (*cumul.add(s as usize));
        *tableU16.offset(*fresh1 as isize) = tableSize.wrapping_add(u) as u16;
        *fresh1 = (*fresh1).wrapping_add(1);
    }

    // Build Symbol Transformation Table
    let mut total = 0u32;
    for (s, &count) in normalizedCounter[..maxSV1].iter().enumerate() {
        match count {
            0 => {
                // filling nonetheless, for compatibility with FSE_getMaxNbBits()
                (*symbolTT.add(s)).deltaNbBits = (tableLog.wrapping_add(1) << 16)
                    .wrapping_sub((1 << tableLog) as core::ffi::c_uint);
            }
            -1 | 1 => {
                (*symbolTT.add(s)).deltaNbBits =
                    (tableLog << 16).wrapping_sub((1 << tableLog) as core::ffi::c_uint);
                (*symbolTT.add(s)).deltaFindState = total.wrapping_sub(1) as core::ffi::c_int;
                total = total.wrapping_add(1);
            }
            _ => {
                let maxBitsOut =
                    tableLog.wrapping_sub(ZSTD_highbit32((count as u32).wrapping_sub(1)));
                let minStatePlus = (count as u32) << maxBitsOut;
                (*symbolTT.add(s)).deltaNbBits = (maxBitsOut << 16).wrapping_sub(minStatePlus);
                (*symbolTT.add(s)).deltaFindState =
                    total.wrapping_sub(count as core::ffi::c_uint) as core::ffi::c_int;
                total = total.wrapping_add(count as core::ffi::c_uint);
            }
        }
    }
    Ok(())
}

fn FSE_NCountWriteBound(maxSymbolValue: u8, tableLog: core::ffi::c_uint) -> usize {
    let maxHeaderSize = (u32::from(maxSymbolValue) + 1)
        .wrapping_mul(tableLog)
        .wrapping_add(4) // bitCount initialized at 4
        .wrapping_add(2) // first two symbols may use one additional bit each
        .wrapping_div(8)
        .wrapping_add(1) // round up to whole nb bytes
        .wrapping_add(2) // additional two bytes for bitstream flush
        as usize;
    if maxSymbolValue != 0 {
        maxHeaderSize
    } else {
        FSE_NCOUNTBOUND
    }
}

#[inline(always)]
unsafe fn FSE_writeNCount_generic<const SAFE: bool>(
    header: *mut core::ffi::c_void,
    headerBufferSize: usize,
    normalizedCounter: &[core::ffi::c_short],
    maxSymbolValue: u8,
    tableLog: core::ffi::c_uint,
) -> Result<usize, Error> {
    let ostart = header as *mut u8;
    let mut out = ostart;
    let oend = ostart.add(headerBufferSize);
    let tableSize = 1 << tableLog;
    let mut bitStream = 0;
    let mut bitCount: core::ffi::c_uint = 0;
    let mut symbol = 0;
    let alphabetSize = u32::from(maxSymbolValue) + 1;
    let mut previousIs0 = false;

    // Table Size
    bitStream = (bitStream as core::ffi::c_uint)
        .wrapping_add(tableLog.wrapping_sub(FSE_MIN_TABLELOG as core::ffi::c_uint) << bitCount);
    bitCount += 4;

    // Init
    let mut remaining = tableSize + 1; // +1 for extra accuracy
    let mut threshold = tableSize;
    let mut nbBits = tableLog as core::ffi::c_int + 1;

    // stops at 1
    while symbol < alphabetSize && remaining > 1 {
        if previousIs0 {
            let mut start = symbol;
            while symbol < alphabetSize && normalizedCounter[symbol as usize] == 0 {
                symbol = symbol.wrapping_add(1);
            }
            if symbol == alphabetSize {
                break; // incorrect distribution
            }
            while symbol >= start.wrapping_add(24) {
                start = start.wrapping_add(24);
                bitStream = (bitStream as core::ffi::c_uint).wrapping_add(0xffff << bitCount);
                if !SAFE && out > oend.sub(2) {
                    return Err(Error::dstSize_tooSmall); // Buffer overflow
                }
                *out = bitStream as u8;
                *out.add(1) = (bitStream >> 8) as u8;
                out = out.add(2);
                bitStream >>= 16;
            }
            while symbol >= start.wrapping_add(3) {
                start = start.wrapping_add(3);
                bitStream = (bitStream as core::ffi::c_uint).wrapping_add(3 << bitCount);
                bitCount += 2;
            }
            bitStream = (bitStream as core::ffi::c_uint)
                .wrapping_add(symbol.wrapping_sub(start) << bitCount);
            bitCount += 2;
            if bitCount > 16 {
                if !SAFE && out > oend.sub(2) {
                    return Err(Error::dstSize_tooSmall); // Buffer overflow
                }
                *out = bitStream as u8;
                *out.add(1) = (bitStream >> 8) as u8;
                out = out.add(2);
                bitStream >>= 16;
                bitCount -= 16;
            }
        }

        let mut count = normalizedCounter[symbol as usize] as core::ffi::c_int;
        symbol = symbol.wrapping_add(1);
        let max = 2 * threshold - 1 - remaining;
        remaining -= if count < 0 { -count } else { count };
        count += 1; // +1 for extra accuracy
        if count >= threshold {
            count += max;
        }
        bitStream = bitStream.wrapping_add((count as u32) << bitCount);
        bitCount += nbBits as core::ffi::c_uint;
        bitCount -= core::ffi::c_uint::from(count < max);
        previousIs0 = count == 1;
        if remaining < 1 {
            return Err(Error::GENERIC);
        }
        while remaining < threshold {
            nbBits -= 1;
            threshold >>= 1;
        }
        if bitCount > 16 {
            if !SAFE && out > oend.sub(2) {
                return Err(Error::dstSize_tooSmall); // Buffer overflow
            }
            *out = bitStream as u8;
            *out.add(1) = (bitStream >> 8) as u8;
            out = out.add(2);
            bitStream >>= 16;
            bitCount -= 16;
        }
    }

    if remaining != 1 {
        return Err(Error::GENERIC); // incorrect normalized distribution
    }

    // flush remaining bitStream
    if !SAFE && out > oend.sub(2) {
        return Err(Error::dstSize_tooSmall); // Buffer overflow
    }
    *out = bitStream as u8;
    *out.add(1) = (bitStream >> 8) as u8;
    out = out.add(bitCount.div_ceil(8) as usize);

    Ok(out.offset_from_unsigned(ostart))
}

pub(crate) unsafe fn FSE_writeNCount(
    buffer: *mut core::ffi::c_void,
    bufferSize: usize,
    normalizedCounter: &[core::ffi::c_short],
    maxSymbolValue: u8,
    tableLog: core::ffi::c_uint,
) -> Result<usize, Error> {
    if tableLog > FSE_MAX_TABLELOG as core::ffi::c_uint {
        return Err(Error::tableLog_tooLarge); // Unsupported
    }
    if tableLog < FSE_MIN_TABLELOG as core::ffi::c_uint {
        return Err(Error::GENERIC); // Unsupported
    }

    if bufferSize >= FSE_NCountWriteBound(maxSymbolValue, tableLog) {
        // write in buffer is safe
        FSE_writeNCount_generic::<true>(
            buffer,
            bufferSize,
            normalizedCounter,
            maxSymbolValue,
            tableLog,
        )
    } else {
        FSE_writeNCount_generic::<false>(
            buffer,
            bufferSize,
            normalizedCounter,
            maxSymbolValue,
            tableLog,
        )
    }
}

/// Provides the minimum logSize to safely represent a distribution.
fn FSE_minTableLog(srcSize: usize, maxSymbolValue: u8) -> core::ffi::c_uint {
    let minBitsSrc = ZSTD_highbit32(srcSize as u32).wrapping_add(1);
    let minBitsSymbols = maxSymbolValue.ilog2().wrapping_add(2);
    minBitsSrc.min(minBitsSymbols)
}

pub(crate) fn FSE_optimalTableLog_internal(
    maxTableLog: core::ffi::c_uint,
    srcSize: usize,
    maxSymbolValue: u8,
    minus: core::ffi::c_uint,
) -> core::ffi::c_uint {
    let maxBitsSrc = (ZSTD_highbit32(srcSize.wrapping_sub(1) as u32)).wrapping_sub(minus);
    let mut tableLog = maxTableLog;
    let minBits = FSE_minTableLog(srcSize, maxSymbolValue);

    if tableLog == 0 {
        tableLog = FSE_DEFAULT_TABLELOG as u32;
    }
    if maxBitsSrc < tableLog {
        tableLog = maxBitsSrc; // Accuracy can be reduced
    }
    if minBits > tableLog {
        tableLog = minBits; // Need a minimum to safely represent all symbol values
    }
    tableLog.clamp(FSE_MIN_TABLELOG as u32, FSE_MAX_TABLELOG as u32)
}

pub(crate) fn FSE_optimalTableLog(
    maxTableLog: core::ffi::c_uint,
    srcSize: usize,
    maxSymbolValue: u8,
) -> core::ffi::c_uint {
    FSE_optimalTableLog_internal(maxTableLog, srcSize, maxSymbolValue, 2)
}

/// Secondary normalization method.
/// To be used when primary method fails.
fn FSE_normalizeM2(
    norm: &mut [core::ffi::c_short],
    tableLog: u32,
    count: &[core::ffi::c_uint],
    mut total: usize,
    maxSymbolValue: u8,
    lowProbCount: core::ffi::c_short,
) -> Result<(), Error> {
    let count = &count[..usize::from(maxSymbolValue) + 1];
    let maxSV1 = usize::from(maxSymbolValue) + 1;
    const NOT_YET_ASSIGNED: i16 = -2;
    let mut distributed = 0usize;

    let lowThreshold = (total >> tableLog) as u32;
    let mut lowOne = ((total * 3) >> tableLog.wrapping_add(1)) as u32;

    let slice = &mut norm[0..=usize::from(maxSymbolValue)];
    for (current, &c) in slice.iter_mut().zip(count) {
        *current = if c == 0 {
            0
        } else if c <= lowThreshold {
            distributed = distributed.wrapping_add(1);
            total = total.wrapping_sub(c as usize);

            lowProbCount
        } else if c <= lowOne {
            distributed = distributed.wrapping_add(1);
            total = total.wrapping_sub(c as usize);

            1
        } else {
            NOT_YET_ASSIGNED
        }
    }
    let mut ToDistribute = (1usize << tableLog).wrapping_sub(distributed);

    if ToDistribute == 0 {
        return Ok(());
    }

    if total / ToDistribute > lowOne as usize {
        // risk of rounding to zero
        lowOne = (total * 3 / (ToDistribute * 2)) as u32;
        for (current, &c) in slice.iter_mut().zip(count) {
            if *current == NOT_YET_ASSIGNED && c <= lowOne {
                *current = 1;
                distributed = distributed.wrapping_add(1);
                total = total.wrapping_sub(c as usize);
            }
        }
        ToDistribute = (1usize << tableLog).wrapping_sub(distributed);
    }

    if distributed == maxSV1 {
        // all values are pretty poor;
        // probably incompressible data (should have already been detected);
        // find max, then give all remaining points to max
        let mut maxV = 0;
        let mut maxC = 0;
        for (s, &c) in count[..maxSV1].iter().enumerate() {
            if c > maxC {
                maxV = s;
                maxC = c;
            }
        }
        norm[maxV] += ToDistribute as i16;
        return Ok(());
    }

    if total == 0 {
        // all of the symbols were low enough for the lowOne or lowThreshold
        let mut s = 0;
        while ToDistribute > 0 {
            if norm[s] > 0 {
                ToDistribute = ToDistribute.wrapping_sub(1);
                norm[s] += 1;
            }
            s = s.wrapping_add(1) % maxSV1;
        }
        return Ok(());
    }

    let vStepLog = 62u32.wrapping_sub(tableLog) as u64;
    let mid = (1u64 << vStepLog.wrapping_sub(1)).wrapping_sub(1);
    let rStep = ((1 << vStepLog) * ToDistribute as u64).wrapping_add(mid) / total as u32 as u64;
    let mut tmpTotal = mid;
    for (current, &c) in slice.iter_mut().zip(count) {
        if *current == NOT_YET_ASSIGNED {
            let end = tmpTotal.wrapping_add(c as u64 * rStep);
            let sStart = (tmpTotal >> vStepLog) as u32;
            let sEnd = (end >> vStepLog) as u32;
            let weight = sEnd.wrapping_sub(sStart);
            if weight < 1 {
                return Err(Error::GENERIC);
            }
            tmpTotal = end;
            *current = weight as core::ffi::c_short;
        }
    }

    Ok(())
}

pub(crate) fn FSE_normalizeCount(
    normalizedCounter: &mut [core::ffi::c_short],
    mut tableLog: core::ffi::c_uint,
    count: &[core::ffi::c_uint],
    total: usize,
    maxSymbolValue: u8,
    useLowProbCount: bool,
) -> Result<core::ffi::c_uint, Error> {
    // Sanity checks
    if tableLog == 0 {
        tableLog = FSE_DEFAULT_TABLELOG as core::ffi::c_uint;
    }
    if tableLog < FSE_MIN_TABLELOG as core::ffi::c_uint {
        return Err(Error::GENERIC); // Unsupported size
    }
    if tableLog > FSE_MAX_TABLELOG as core::ffi::c_uint {
        return Err(Error::tableLog_tooLarge); // Unsupported size
    }
    if tableLog < FSE_minTableLog(total, maxSymbolValue) {
        return Err(Error::GENERIC); // Too small tableLog, compression potentially impossible
    }

    static rtbTable: [u32; 8] = [0, 473195, 504333, 520860, 550000, 700000, 750000, 830000];
    let lowProbCount = if useLowProbCount { -1 } else { 1 };
    let scale = 62u32.wrapping_sub(tableLog) as u64;
    let step = (1 << 62) / total as u32 as u64;
    let vStep = 1u64.wrapping_shl(scale.wrapping_sub(20) as u32);
    let mut stillToDistribute = 1 << tableLog;
    let mut largest = 0;
    let mut largestP = 0;
    let lowThreshold = (total >> tableLog) as u32;

    let slice = &mut normalizedCounter[0..=usize::from(maxSymbolValue)];
    let count = &count[..slice.len()];
    for (s, current) in slice.iter_mut().enumerate() {
        let c = count[s];
        if c as usize == total {
            return Ok(0); // rle special case
        }

        *current = if c == 0 {
            0
        } else if c <= lowThreshold {
            stillToDistribute -= 1;

            lowProbCount
        } else {
            let mut proba = ((u64::from(c) * step) >> scale) as i16;
            if proba < 8 {
                let restToBeat = vStep * rtbTable[proba as usize] as u64;
                proba = (proba as core::ffi::c_int
                    + core::ffi::c_int::from(
                        (u64::from(c) * step).wrapping_sub((proba as u64) << scale) > restToBeat,
                    )) as core::ffi::c_short;
            }
            if proba > largestP {
                largestP = proba;
                largest = s;
            }
            stillToDistribute -= proba as core::ffi::c_int;

            proba
        }
    }
    if -stillToDistribute >= normalizedCounter[largest] as core::ffi::c_int >> 1 {
        // corner case, need another normalization method
        FSE_normalizeM2(
            normalizedCounter,
            tableLog,
            count,
            total,
            maxSymbolValue,
            lowProbCount,
        )?;
    } else {
        normalizedCounter[largest] += stillToDistribute as core::ffi::c_short
    }

    Ok(tableLog)
}

/// Fake FSE_CTable, for rle input (always same symbol).
pub(crate) fn FSE_buildCTable_rle(ct: &mut [FSE_CTable], symbolValue: u8) {
    // header: a tableLog of zero, and `symbolValue` as the maximum symbol value
    ct[0] = FSE_writeU16Pair(0, symbolValue as u16);

    // the (two-entry) state table, zeroed just in case
    ct[1] = FSE_writeU16Pair(0, 0);

    // Build Symbol Transformation Table
    let index = FSE_symbolTTIndex(0) + 2 * symbolValue as usize;
    ct[index] = 0; // deltaFindState
    ct[index + 1] = 0; // deltaNbBits
}

#[inline(always)]
unsafe fn FSE_flushBits<const FAST: bool>(bitC: &mut BIT_CStream_t) {
    if FAST {
        bitC.flush_bits_fast();
    } else {
        bitC.flush_bits();
    }
}

#[inline(always)]
unsafe fn FSE_compress_usingCTable_generic<const FAST: bool>(
    dst: *mut core::ffi::c_void,
    dstSize: usize,
    src: &[u8],
    ct: &[FSE_CTable],
) -> usize {
    // init
    if src.len() <= 2 {
        return 0;
    }

    let Ok(mut bitC) = BIT_CStream_t::new(dst, dstSize) else {
        return 0; // not enough space available to write a bitstream
    };

    let (mut rest, mut CState1, mut CState2) = match src {
        [rest @ .., c, b, a] if src.len() & 1 != 0 => {
            let mut CState1 = FSE_CState_t::new2(ct, u32::from(*a));
            let CState2 = FSE_CState_t::new2(ct, u32::from(*b));
            FSE_encodeSymbol(&mut bitC, &mut CState1, u32::from(*c));
            FSE_flushBits::<FAST>(&mut bitC);
            (rest, CState1, CState2)
        }
        [rest @ .., a, b] => {
            let CState2 = FSE_CState_t::new2(ct, u32::from(*b));
            let CState1 = FSE_CState_t::new2(ct, u32::from(*a));
            (rest, CState1, CState2)
        }
        _ => unreachable!(),
    };

    // join to mod 4
    if BitContainerType::BITS > (FSE_MAX_TABLELOG * 4 + 7) as u32 && rest.len() & 2 != 0 {
        let [prefix @ .., b, a] = rest else {
            unreachable!()
        };
        FSE_encodeSymbol(&mut bitC, &mut CState2, u32::from(*a));
        FSE_encodeSymbol(&mut bitC, &mut CState1, u32::from(*b));
        FSE_flushBits::<FAST>(&mut bitC);
        rest = prefix;
    }

    // 2 or 4 encoding per loop
    // this test must be static
    if const { BitContainerType::BITS > (FSE_MAX_TABLELOG * 4 + 7) as u32 } {
        let (remainder, chunks) = rest.as_rchunks::<4>();
        debug_assert!(remainder.is_empty());
        for &[d, c, b, a] in chunks.iter().rev() {
            FSE_encodeSymbol(&mut bitC, &mut CState2, u32::from(a));
            FSE_encodeSymbol(&mut bitC, &mut CState1, u32::from(b));
            FSE_encodeSymbol(&mut bitC, &mut CState2, u32::from(c));
            FSE_encodeSymbol(&mut bitC, &mut CState1, u32::from(d));
            FSE_flushBits::<FAST>(&mut bitC);
        }
    } else {
        let (remainder, chunks) = rest.as_rchunks::<2>();
        debug_assert!(remainder.is_empty());
        for &[b, a] in chunks.iter().rev() {
            FSE_encodeSymbol(&mut bitC, &mut CState2, u32::from(a));

            // this test must be static
            if const { BitContainerType::BITS < (FSE_MAX_TABLELOG * 2 + 7) as u32 } {
                FSE_flushBits::<FAST>(&mut bitC);
            }

            FSE_encodeSymbol(&mut bitC, &mut CState1, u32::from(b));
            FSE_flushBits::<FAST>(&mut bitC);
        }
    }

    FSE_flushCState(&mut bitC, &CState2);
    FSE_flushCState(&mut bitC, &CState1);
    bitC.close()
}

pub(crate) unsafe fn FSE_compress_usingCTable(
    dst: *mut core::ffi::c_void,
    dstSize: usize,
    src: &[u8],
    ct: &[FSE_CTable],
) -> usize {
    let fast = dstSize >= src.len() + (src.len() >> 7) + 4 + size_of::<usize>();

    if fast {
        FSE_compress_usingCTable_generic::<true>(dst, dstSize, src, ct)
    } else {
        FSE_compress_usingCTable_generic::<false>(dst, dstSize, src, ct)
    }
}
