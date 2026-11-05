use crate::lib::common::bitstream::{BIT_CStream_t, BitContainerType, STREAM_ACCUMULATOR_MIN};
use crate::lib::common::error_private::Error;
use crate::lib::common::fse::{
    FSE_CState_t, FSE_CTable, FSE_bitCost, FSE_encodeSymbol, FSE_flushCState, FSE_repeat,
};
use crate::lib::common::mem::MEM_32bits;
use crate::lib::common::zstd_internal::{
    LLFSELog, LL_bits, MLFSELog, ML_bits, OffFSELog, SymbolEncodingType,
};
use crate::lib::compress::fse_compress::{
    FSE_buildCTable_rle, FSE_buildCTable_wksp, FSE_normalizeCount, FSE_optimalTableLog,
    FSE_writeNCount,
};
use crate::lib::compress::zstd_compress::{DefaultPolicy, SeqDef};
use crate::lib::zstd::{ZSTD_lazy, ZSTD_strategy};

#[derive(Copy, Clone)]
#[repr(C)]
pub struct ZSTD_BuildCTableWksp {
    pub norm: [i16; 53],
    pub wksp: [u32; 285],
}

// `LL_bits` and `ML_bits`, padded so that a lookup indexed by a `u8` needs no bounds check.
static LL_bits_u8: [u8; 256] = pad_bits(&LL_bits);
static ML_bits_u8: [u8; 256] = pad_bits(&ML_bits);

const fn pad_bits(bits: &[u8]) -> [u8; 256] {
    let mut padded = [0; 256];
    let mut i = 0;
    while i < bits.len() {
        padded[i] = bits[i];
        i += 1;
    }
    padded
}

/// -log2(x / 256) lookup table for x in [0, 256).
/// If x == 0: Return 0
/// Else: Return floor(-log2(x / 256) * 256)
static kInverseProbabilityLog256: [core::ffi::c_uint; 256] = [
    0, 2048, 1792, 1642, 1536, 1453, 1386, 1329, 1280, 1236, 1197, 1162, 1130, 1100, 1073, 1047,
    1024, 1001, 980, 960, 941, 923, 906, 889, 874, 859, 844, 830, 817, 804, 791, 779, 768, 756,
    745, 734, 724, 714, 704, 694, 685, 676, 667, 658, 650, 642, 633, 626, 618, 610, 603, 595, 588,
    581, 574, 567, 561, 554, 548, 542, 535, 529, 523, 517, 512, 506, 500, 495, 489, 484, 478, 473,
    468, 463, 458, 453, 448, 443, 438, 434, 429, 424, 420, 415, 411, 407, 402, 398, 394, 390, 386,
    382, 377, 373, 370, 366, 362, 358, 354, 350, 347, 343, 339, 336, 332, 329, 325, 322, 318, 315,
    311, 308, 305, 302, 298, 295, 292, 289, 286, 282, 279, 276, 273, 270, 267, 264, 261, 258, 256,
    253, 250, 247, 244, 241, 239, 236, 233, 230, 228, 225, 222, 220, 217, 215, 212, 209, 207, 204,
    202, 199, 197, 194, 192, 190, 187, 185, 182, 180, 178, 175, 173, 171, 168, 166, 164, 162, 159,
    157, 155, 153, 151, 149, 146, 144, 142, 140, 138, 136, 134, 132, 130, 128, 126, 123, 121, 119,
    117, 115, 114, 112, 110, 108, 106, 104, 102, 100, 98, 96, 94, 93, 91, 89, 87, 85, 83, 82, 80,
    78, 76, 74, 73, 71, 69, 67, 66, 64, 62, 61, 59, 57, 55, 54, 52, 50, 49, 47, 46, 44, 42, 41, 39,
    37, 36, 34, 33, 31, 30, 28, 26, 25, 23, 22, 20, 19, 17, 16, 14, 13, 11, 10, 8, 7, 5, 4, 2, 1,
];

fn ZSTD_getFSEMaxSymbolValue(ctable: &[FSE_CTable]) -> u16 {
    let [_, _, a, b] = ctable[0].to_ne_bytes();
    u16::from_ne_bytes([a, b])
}

/// Returns true if we should use ncount=-1 else we should
/// use ncount=1 for low probability symbols instead.
fn ZSTD_useLowProbCount(nbSeq: usize) -> bool {
    // Heuristic: This should cover most blocks <= 16K and
    // start to fade out after 16K to about 32K depending on
    // compressibility.
    nbSeq >= 2048
}

/// Returns the cost in bytes of encoding the normalized count header.
/// Returns an error if any of the helper functions return an error.
unsafe fn ZSTD_NCountCost(
    count: &[core::ffi::c_uint],
    max: u8,
    nbSeq: usize,
    FSELog: core::ffi::c_uint,
) -> Result<usize, Error> {
    let mut wksp: [u8; 512] = [0; 512];
    let mut norm: [i16; 53] = [0; 53];
    let tableLog = FSE_optimalTableLog(FSELog, nbSeq, max);
    FSE_normalizeCount(
        &mut norm,
        tableLog,
        count,
        nbSeq,
        max,
        ZSTD_useLowProbCount(nbSeq),
    )?;
    FSE_writeNCount(
        wksp.as_mut_ptr() as *mut core::ffi::c_void,
        size_of::<[u8; 512]>(),
        &norm,
        max,
        tableLog,
    )
}

/// Returns the cost in bits of encoding the distribution described by count
/// using the entropy bound.
fn ZSTD_entropyCost(count: &[core::ffi::c_uint], max: u8, total: usize) -> usize {
    let mut cost = 0u32;
    for &c in &count[..usize::from(max) + 1] {
        let mut norm = (256u32.wrapping_mul(c) as usize / total) as core::ffi::c_uint;
        if c != 0 && norm == 0 {
            norm = 1;
        }
        cost = cost.wrapping_add(c.wrapping_mul(kInverseProbabilityLog256[norm as usize]));
    }
    (cost >> 8) as usize
}

/// Returns the cost in bits of encoding the distribution in count using ctable.
/// Returns an error if ctable cannot represent all the symbols in count.
pub fn ZSTD_fseBitCost(
    ctable: &[FSE_CTable],
    count: &[core::ffi::c_uint],
    max: u8,
) -> Result<usize, Error> {
    let count = &count[..usize::from(max) + 1];
    let kAccuracyLog = 8;
    let mut cost = 0usize;
    let cstate = FSE_CState_t::new(ctable);
    if ZSTD_getFSEMaxSymbolValue(ctable) < u16::from(max) {
        return Err(Error::GENERIC);
    }
    for s in 0..u32::from(max) + 1 {
        let tableLog = cstate.stateLog;
        let badCost = tableLog.wrapping_add(1) << kAccuracyLog;
        let bitCost = FSE_bitCost(cstate.symbolTT, tableLog, s, kAccuracyLog);
        if count[s as usize] != 0 {
            if bitCost >= badCost {
                return Err(Error::GENERIC);
            }
            cost = cost.wrapping_add(count[s as usize] as usize * bitCost as usize);
        }
    }
    Ok(cost >> kAccuracyLog)
}

/// Returns the cost in bits of encoding the distribution in count using the
/// table described by norm. The max symbol support by norm is assumed >= max.
/// norm must be valid for every symbol with non-zero probability in count.
pub fn ZSTD_crossEntropyCost(
    norm: &[core::ffi::c_short],
    accuracyLog: core::ffi::c_uint,
    count: &[core::ffi::c_uint],
    max: u8,
) -> usize {
    let count = &count[..usize::from(max) + 1];
    let norm = &norm[..count.len()];
    let shift = 8u32 - accuracyLog;
    let mut cost = 0usize;
    for (&n, &c) in norm.iter().zip(count) {
        let normAcc = if n != -1 { n as u8 } else { 1 };
        let norm256 = normAcc << shift;
        cost += c.wrapping_mul(kInverseProbabilityLog256[usize::from(norm256)]) as usize;
    }
    cost >> 8
}

pub unsafe fn ZSTD_selectEncodingType(
    repeatMode: &mut FSE_repeat,
    count: &[core::ffi::c_uint],
    max: u8,
    mostFrequent: usize,
    nbSeq: usize,
    FSELog: core::ffi::c_uint,
    prevCTable: &[FSE_CTable],
    defaultNorm: &[core::ffi::c_short],
    defaultNormLog: u32,
    isDefaultAllowed: DefaultPolicy,
    strategy: ZSTD_strategy,
) -> SymbolEncodingType {
    if mostFrequent == nbSeq {
        *repeatMode = FSE_repeat::None;
        if isDefaultAllowed == DefaultPolicy::Allowed && nbSeq <= 2 {
            // Prefer SymbolEncodingType::Basic over SymbolEncodingType::Rle when there are 2 or fewer symbols,
            // since RLE uses 1 byte, but SymbolEncodingType::Basic uses 5-6 bits per symbol.
            // If basic encoding isn't possible, always choose RLE.
            return SymbolEncodingType::Basic;
        }
        return SymbolEncodingType::Rle;
    }
    if (strategy as core::ffi::c_uint) < ZSTD_lazy {
        if isDefaultAllowed == DefaultPolicy::Allowed {
            let staticFse_nbSeq_max = 1000;
            let mult = 10u32.wrapping_sub(strategy as core::ffi::c_uint) as usize;
            let baseLog = 3;
            let dynamicFse_nbSeq_min = ((1 << defaultNormLog) * mult) >> baseLog;
            if *repeatMode == FSE_repeat::Valid && nbSeq < staticFse_nbSeq_max {
                return SymbolEncodingType::Repeat;
            }
            if nbSeq < dynamicFse_nbSeq_min
                || mostFrequent < nbSeq >> defaultNormLog.wrapping_sub(1)
            {
                // The format allows default tables to be repeated, but it isn't useful.
                // When using simple heuristics to select encoding type, we don't want
                // to confuse these tables with dictionaries. When running more careful
                // analysis, we don't need to waste time checking both repeating tables
                // and default tables.
                *repeatMode = FSE_repeat::None;
                return SymbolEncodingType::Basic;
            }
        }
    } else {
        let basicCost = if isDefaultAllowed == DefaultPolicy::Allowed {
            Ok(ZSTD_crossEntropyCost(
                defaultNorm,
                defaultNormLog,
                count,
                max,
            ))
        } else {
            Err(Error::GENERIC)
        };
        let repeatCost = if *repeatMode != FSE_repeat::None {
            ZSTD_fseBitCost(prevCTable, count, max)
        } else {
            Err(Error::GENERIC)
        };
        let nCountCost = match ZSTD_NCountCost(count, max, nbSeq, FSELog) {
            Ok(nCountCost) => nCountCost,
            Err(_) => unreachable!(),
        };
        let compressedCost = (nCountCost << 3).wrapping_add(ZSTD_entropyCost(count, max, nbSeq));

        if isDefaultAllowed == DefaultPolicy::Allowed {
            assert!(basicCost.is_ok());
            assert!(!(*repeatMode == FSE_repeat::Valid && repeatCost.is_err()));
        }
        let repeatCost = repeatCost.unwrap_or(usize::MAX);
        let basicCost = basicCost.unwrap_or(usize::MAX);
        if basicCost <= repeatCost && basicCost <= compressedCost {
            *repeatMode = FSE_repeat::None;
            return SymbolEncodingType::Basic;
        }
        if repeatCost <= compressedCost {
            return SymbolEncodingType::Repeat;
        }
    }
    *repeatMode = FSE_repeat::Check;
    SymbolEncodingType::Compressed
}

pub unsafe fn ZSTD_buildCTable(
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    nextCTable: &mut [FSE_CTable],
    FSELog: u32,
    type_0: SymbolEncodingType,
    count: &mut [core::ffi::c_uint],
    max: u8,
    codeTable: &[u8],
    defaultNorm: &[i16],
    defaultNormLog: u32,
    defaultMax: u8,
    prevCTable: &[FSE_CTable],
    entropyWorkspace: *mut core::ffi::c_void,
    entropyWorkspaceSize: usize,
) -> Result<usize, Error> {
    let nbSeq = codeTable.len();
    let op = dst as *mut u8;
    let oend: *const u8 = op.add(dstCapacity);

    match type_0 {
        SymbolEncodingType::Rle => {
            FSE_buildCTable_rle(nextCTable, max);
            if dstCapacity == 0 {
                return Err(Error::dstSize_tooSmall);
            }
            *op = codeTable[0];
            Ok(1)
        }
        SymbolEncodingType::Repeat => {
            nextCTable[..prevCTable.len()].copy_from_slice(prevCTable);
            Ok(0)
        }
        SymbolEncodingType::Basic => {
            FSE_buildCTable_wksp(
                nextCTable,
                defaultNorm,
                defaultMax,
                defaultNormLog,
                entropyWorkspace,
                entropyWorkspaceSize,
            )?;
            Ok(0)
        }
        SymbolEncodingType::Compressed => {
            let wksp = entropyWorkspace as *mut ZSTD_BuildCTableWksp;
            let mut nbSeq_1 = nbSeq;
            let tableLog = FSE_optimalTableLog(FSELog, nbSeq, max);
            let lastCode = usize::from(codeTable[nbSeq - 1]);
            if count[lastCode] > 1 {
                count[lastCode] = count[lastCode].wrapping_sub(1);
                nbSeq_1 = nbSeq_1.wrapping_sub(1);
            }
            FSE_normalizeCount(
                &mut (*wksp).norm,
                tableLog,
                count,
                nbSeq_1,
                max,
                ZSTD_useLowProbCount(nbSeq_1),
            )?;
            let nCountSize = FSE_writeNCount(
                op as *mut core::ffi::c_void,
                oend.offset_from_unsigned(op),
                &(*wksp).norm,
                max,
                tableLog,
            )?;
            FSE_buildCTable_wksp(
                nextCTable,
                &(*wksp).norm,
                max,
                tableLog,
                ((*wksp).wksp).as_mut_ptr() as *mut core::ffi::c_void,
                size_of::<[u32; 285]>(),
            )?;
            Ok(nCountSize)
        }
    }
}

#[inline(always)]
unsafe fn ZSTD_encodeSequences_body(
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    CTable_MatchLength: &[FSE_CTable; 363],
    mlCodeTable: &[u8],
    CTable_OffsetBits: &[FSE_CTable; 193],
    ofCodeTable: &[u8],
    CTable_LitLength: &[FSE_CTable; 329],
    llCodeTable: &[u8],
    sequences: &[SeqDef],
    longOffsets: bool,
) -> Result<usize, Error> {
    let nbSeq = sequences.len();
    let (lastSeq, sequences) = sequences.split_last().unwrap();
    let (&lastMl, mlCodeTable) = mlCodeTable[..nbSeq].split_last().unwrap();
    let (&lastOf, ofCodeTable) = ofCodeTable[..nbSeq].split_last().unwrap();
    let (&lastLl, llCodeTable) = llCodeTable[..nbSeq].split_last().unwrap();

    let mut blockStream = BIT_CStream_t::new(dst, dstCapacity)?;

    // first symbols
    let mut stateMatchLength = FSE_CState_t::new2(CTable_MatchLength, lastMl as u32);
    let mut stateOffsetBits = FSE_CState_t::new2(CTable_OffsetBits, lastOf as u32);
    let mut stateLitLength = FSE_CState_t::new2(CTable_LitLength, lastLl as u32);
    blockStream.add_bits(
        lastSeq.litLength as BitContainerType,
        LL_bits_u8[usize::from(lastLl)] as core::ffi::c_uint,
    );
    if MEM_32bits() {
        blockStream.flush_bits();
    }
    blockStream.add_bits(
        lastSeq.mlBase as BitContainerType,
        ML_bits_u8[usize::from(lastMl)] as core::ffi::c_uint,
    );
    if MEM_32bits() {
        blockStream.flush_bits();
    }
    if longOffsets {
        let ofBits = lastOf as u32;
        let extraBits = ofBits.wrapping_sub(ofBits.min(STREAM_ACCUMULATOR_MIN - 1));
        if extraBits != 0 {
            blockStream.add_bits(lastSeq.offBase as BitContainerType, extraBits);
            blockStream.flush_bits();
        }
        blockStream.add_bits(
            (lastSeq.offBase >> extraBits) as BitContainerType,
            ofBits.wrapping_sub(extraBits),
        );
    } else {
        blockStream.add_bits(
            lastSeq.offBase as BitContainerType,
            lastOf as core::ffi::c_uint,
        );
    }
    blockStream.flush_bits();

    for (((seq, &llCode), &ofCode), &mlCode) in sequences
        .iter()
        .zip(llCodeTable)
        .zip(ofCodeTable)
        .zip(mlCodeTable)
        .rev()
    {
        let llBits = LL_bits_u8[usize::from(llCode)] as u32;
        let ofBits_0 = ofCode as u32;
        let mlBits = ML_bits_u8[usize::from(mlCode)] as u32;
        FSE_encodeSymbol(
            &mut blockStream,
            &mut stateOffsetBits,
            ofCode as core::ffi::c_uint,
        );
        FSE_encodeSymbol(
            &mut blockStream,
            &mut stateMatchLength,
            mlCode as core::ffi::c_uint,
        );
        if MEM_32bits() {
            blockStream.flush_bits();
        }
        FSE_encodeSymbol(
            &mut blockStream,
            &mut stateLitLength,
            llCode as core::ffi::c_uint,
        );
        if MEM_32bits()
            || ofBits_0.wrapping_add(mlBits).wrapping_add(llBits)
                >= 64 - 7 - (LLFSELog + MLFSELog + OffFSELog)
        {
            blockStream.flush_bits();
        }
        blockStream.add_bits(seq.litLength as BitContainerType, llBits);
        if MEM_32bits() && llBits.wrapping_add(mlBits) > 24 {
            blockStream.flush_bits();
        }
        blockStream.add_bits(seq.mlBase as BitContainerType, mlBits);
        if MEM_32bits() || ofBits_0.wrapping_add(mlBits).wrapping_add(llBits) > 56 {
            blockStream.flush_bits();
        }
        if longOffsets {
            let extraBits_0 = ofBits_0.wrapping_sub(ofBits_0.min(STREAM_ACCUMULATOR_MIN - 1));
            if extraBits_0 != 0 {
                blockStream.add_bits(seq.offBase as BitContainerType, extraBits_0);
                blockStream.flush_bits();
            }
            blockStream.add_bits(
                (seq.offBase >> extraBits_0) as BitContainerType,
                ofBits_0.wrapping_sub(extraBits_0),
            );
        } else {
            blockStream.add_bits(seq.offBase as BitContainerType, ofBits_0);
        }
        blockStream.flush_bits();
    }

    FSE_flushCState(&mut blockStream, &stateMatchLength);
    FSE_flushCState(&mut blockStream, &stateOffsetBits);
    FSE_flushCState(&mut blockStream, &stateLitLength);

    let streamSize = blockStream.close();
    if streamSize == 0 {
        return Err(Error::dstSize_tooSmall);
    }
    Ok(streamSize)
}

unsafe fn ZSTD_encodeSequences_default(
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    CTable_MatchLength: &[FSE_CTable; 363],
    mlCodeTable: &[u8],
    CTable_OffsetBits: &[FSE_CTable; 193],
    ofCodeTable: &[u8],
    CTable_LitLength: &[FSE_CTable; 329],
    llCodeTable: &[u8],
    sequences: &[SeqDef],
    longOffsets: bool,
) -> Result<usize, Error> {
    ZSTD_encodeSequences_body(
        dst,
        dstCapacity,
        CTable_MatchLength,
        mlCodeTable,
        CTable_OffsetBits,
        ofCodeTable,
        CTable_LitLength,
        llCodeTable,
        sequences,
        longOffsets,
    )
}

#[cfg_attr(target_arch = "x86_64", target_feature(enable = "bmi2"))]
unsafe fn ZSTD_encodeSequences_bmi2(
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    CTable_MatchLength: &[FSE_CTable; 363],
    mlCodeTable: &[u8],
    CTable_OffsetBits: &[FSE_CTable; 193],
    ofCodeTable: &[u8],
    CTable_LitLength: &[FSE_CTable; 329],
    llCodeTable: &[u8],
    sequences: &[SeqDef],
    longOffsets: bool,
) -> Result<usize, Error> {
    ZSTD_encodeSequences_body(
        dst,
        dstCapacity,
        CTable_MatchLength,
        mlCodeTable,
        CTable_OffsetBits,
        ofCodeTable,
        CTable_LitLength,
        llCodeTable,
        sequences,
        longOffsets,
    )
}

pub unsafe fn ZSTD_encodeSequences(
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    CTable_MatchLength: &[FSE_CTable; 363],
    mlCodeTable: &[u8],
    CTable_OffsetBits: &[FSE_CTable; 193],
    ofCodeTable: &[u8],
    CTable_LitLength: &[FSE_CTable; 329],
    llCodeTable: &[u8],
    sequences: &[SeqDef],
    longOffsets: bool,
    bmi2: bool,
) -> Result<usize, Error> {
    if bmi2 {
        return ZSTD_encodeSequences_bmi2(
            dst,
            dstCapacity,
            CTable_MatchLength,
            mlCodeTable,
            CTable_OffsetBits,
            ofCodeTable,
            CTable_LitLength,
            llCodeTable,
            sequences,
            longOffsets,
        );
    }
    ZSTD_encodeSequences_default(
        dst,
        dstCapacity,
        CTable_MatchLength,
        mlCodeTable,
        CTable_OffsetBits,
        ofCodeTable,
        CTable_LitLength,
        llCodeTable,
        sequences,
        longOffsets,
    )
}
