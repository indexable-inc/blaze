pub type ZSTD_getAllMatchesFn = unsafe fn(
    &mut [ZSTD_match_t; ZSTD_OPT_SIZE],
    &mut ZSTD_MatchState_t,
    *mut u32,
    *const u8,
    *const u8,
    &RepCodes,
    bool,
    u32,
) -> usize;

#[repr(C)]
pub struct ZSTD_optLdm_t {
    pub seqStore: RawSeqStore_t,
    pub startPosInBlock: u32,
    pub endPosInBlock: u32,
    pub offset: u32,
}

#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq, Default)]
pub enum BaseDirective {
    #[default]
    Possible = 0,
    Guaranteed = 1,
}

use crate::lib::common::bits::ZSTD_highbit32;
use crate::lib::common::fse::{FSE_CState_t, FSE_getMaxNbBits};
use crate::lib::common::huf::HUF_repeat;
use crate::lib::common::mem::MEM_read32;
use crate::lib::common::zstd_internal::{
    LL_bits, ML_bits, MaxLL, MaxLit, MaxML, MaxOff, RepCodes, MINMATCH, ZSTD_OPT_NUM, ZSTD_REP_NUM,
};
use crate::lib::compress::hist::HIST_count_simple;
use crate::lib::compress::huf_compress::HUF_getNbBitsFromCTable;
use crate::lib::compress::zstd_compress::{
    RawSeqStore_t, SeqStore_t, ZSTD_MatchState_t, ZSTD_optimal_t, ZSTD_resetSeqStore, ZSTD_OPT_SIZE,
};
use crate::lib::compress::zstd_compress_internal::{
    optState_t, DictMatchState, DictMode, DictModeMarker, ExtDict, NoDict, OptPrice, ZSTD_LLcode,
    ZSTD_MLcode, ZSTD_count, ZSTD_count_2segments, ZSTD_hash32Ptr, ZSTD_hashPtr,
    ZSTD_index_overlap_check, ZSTD_match_t, ZSTD_storeSeq, ZSTD_updateRep,
};
use crate::lib::compress::zstd_ldm::ZSTD_ldm_skipRawSeqStoreBytes;
use crate::lib::polyfill::PointerExt;
use crate::lib::zstd::{ParamSwitch, ZSTD_compressionParameters, ZSTD_BLOCKSIZE_MAX};

#[inline]
fn ZSTD_newRep(rep: &RepCodes, offBase: u32, ll0: bool) -> RepCodes {
    let mut newReps = *rep;
    ZSTD_updateRep(&mut newReps, offBase, ll0);
    newReps
}

pub const ZSTD_LITFREQ_ADD: u32 = 2;
pub const ZSTD_MAX_PRICE: core::ffi::c_int = 1 << 30;
/// if srcSize < ZSTD_PREDEF_THRESHOLD, symbols' cost is assumed static, directly determined by pre-defined distributions
pub const ZSTD_PREDEF_THRESHOLD: usize = 8;
pub const BITCOST_ACCURACY: u32 = 8;
pub const BITCOST_MULTIPLIER: u32 = 1 << BITCOST_ACCURACY;

/// provide estimated "cost" of a stat in full bits only
#[inline]
const fn ZSTD_bitWeight(stat: u32) -> u32 {
    (ZSTD_highbit32(stat.wrapping_add(1))).wrapping_mul(BITCOST_MULTIPLIER)
}

/// provide fractional-bit "cost" of a stat,
/// using linear interpolation approximation
#[inline]
const fn ZSTD_fracWeight(rawStat: u32) -> u32 {
    let stat = rawStat.wrapping_add(1);
    let hb = ZSTD_highbit32(stat);
    let BWeight = hb * BITCOST_MULTIPLIER;
    // FWeight was meant for "Fractional weight"
    // but it's effectively a value between 1 and 2
    // using fixed point arithmetic
    let FWeight = stat << BITCOST_ACCURACY >> hb;

    BWeight.wrapping_add(FWeight)
}

/// provide the cost of a stat, using either the full-bits or
/// fractional-bit estimate depending on `optLevel`
#[inline]
const fn WEIGHT(stat: u32, optLevel: u32) -> u32 {
    match optLevel {
        0 => ZSTD_bitWeight(stat),
        _ => ZSTD_fracWeight(stat),
    }
}

fn ZSTD_compressedLiterals(opt_state: &optState_t) -> bool {
    opt_state.literalCompressionMode != ParamSwitch::Disable
}

fn ZSTD_setBasePrices(opt_state: &mut optState_t, optLevel: u32) {
    if ZSTD_compressedLiterals(opt_state) {
        opt_state.litSumBasePrice = WEIGHT(opt_state.litSum, optLevel);
    }
    opt_state.litLengthSumBasePrice = WEIGHT(opt_state.litLengthSum, optLevel);
    opt_state.matchLengthSumBasePrice = WEIGHT(opt_state.matchLengthSum, optLevel);
    opt_state.offCodeSumBasePrice = WEIGHT(opt_state.offCodeSum, optLevel);
}

fn ZSTD_downscaleStats(
    table: &mut [core::ffi::c_uint],
    shift: u32,
    base_directive: BaseDirective,
) -> u32 {
    let mut sum = 0u32;
    for elt in table {
        let base = match base_directive {
            BaseDirective::Guaranteed => true,
            BaseDirective::Possible => *elt > 0,
        };
        let new_stat = u32::from(base) + (*elt >> shift);
        sum += new_stat;
        *elt = new_stat;
    }
    sum
}

/// Reduce all elt frequencies in table if sum too large.
/// Returns the resulting sum of elements.
fn ZSTD_scaleStats(table: &mut [core::ffi::c_uint], logTarget: u32) -> u32 {
    let prevsum = table.iter().sum();
    let factor = prevsum >> logTarget;

    if factor <= 1 {
        return prevsum;
    }

    ZSTD_downscaleStats(table, ZSTD_highbit32(factor), BaseDirective::Guaranteed)
}

/// if first block (detected by opt_state->litLengthSum == 0): init statistics
///    take hints from dictionary if there is one
///    and init from zero if there is none,
///    using src for literals stats, and baseline stats for sequence symbols
/// otherwise downscale existing stats, to be used as seed for next block.
unsafe fn ZSTD_rescaleFreqs(
    opt_state: &mut optState_t,
    src: *const u8,
    srcSize: usize,
    optLevel: u32,
) {
    let compressedLiterals = ZSTD_compressedLiterals(opt_state);
    opt_state.priceType = OptPrice::Dynamic;

    if opt_state.litLengthSum == 0 {
        // no literals stats collected -> first block assumed -> init

        // heuristic: use pre-defined stats for too small inputs
        if srcSize <= ZSTD_PREDEF_THRESHOLD {
            opt_state.priceType = OptPrice::Predef;
        }

        if (*opt_state.symbolCosts).huf.repeatMode == HUF_repeat::Valid {
            // huffman stats covering the full value set: table presumed generated by dictionary
            opt_state.priceType = OptPrice::Dynamic;

            if compressedLiterals {
                // generate literals statistics from huffman table
                let symbolCosts = opt_state.symbolCosts;
                let mut litSum = 0u32;
                for (lit, freq) in opt_state.litFreq_mut().iter_mut().enumerate() {
                    let scaleLog = 11u32; // scale to 2K
                    let bitCost = HUF_getNbBitsFromCTable(&(*symbolCosts).huf.CTable, lit as u32);
                    *freq = if bitCost != 0 {
                        1 << scaleLog.wrapping_sub(bitCost)
                    } else {
                        1 // minimum to calculate cost
                    };
                    litSum = litSum.wrapping_add(*freq);
                }
                opt_state.litSum = litSum;
            }

            let llstate = FSE_CState_t::new(&(*opt_state.symbolCosts).fse.litlengthCTable);
            let mut litLengthSum = 0u32;
            for (ll, freq) in opt_state.litLengthFreq_mut().iter_mut().enumerate() {
                let scaleLog_0 = 10u32; // scale to 1K
                let bitCost_0 = FSE_getMaxNbBits(llstate.symbolTT, ll as u32);
                *freq = if bitCost_0 != 0 {
                    1 << scaleLog_0.wrapping_sub(bitCost_0)
                } else {
                    1 // minimum to calculate cost
                };
                litLengthSum = litLengthSum.wrapping_add(*freq);
            }
            opt_state.litLengthSum = litLengthSum;

            let mlstate = FSE_CState_t::new(&(*opt_state.symbolCosts).fse.matchlengthCTable);
            let mut matchLengthSum = 0u32;
            for (ml, freq) in opt_state.matchLengthFreq_mut().iter_mut().enumerate() {
                let scaleLog_1 = 10u32;
                let bitCost_1 = FSE_getMaxNbBits(mlstate.symbolTT, ml as u32);
                *freq = if bitCost_1 != 0 {
                    1 << scaleLog_1.wrapping_sub(bitCost_1)
                } else {
                    1 // minimum to calculate cost
                };
                matchLengthSum = matchLengthSum.wrapping_add(*freq);
            }
            opt_state.matchLengthSum = matchLengthSum;

            let ofstate = FSE_CState_t::new(&(*opt_state.symbolCosts).fse.offcodeCTable);
            let mut offCodeSum = 0u32;
            for (of, freq) in opt_state.offCodeFreq_mut().iter_mut().enumerate() {
                let scaleLog_2 = 10u32;
                let bitCost_2 = FSE_getMaxNbBits(ofstate.symbolTT, of as u32);
                *freq = if bitCost_2 != 0 {
                    1 << scaleLog_2.wrapping_sub(bitCost_2)
                } else {
                    1 // minimum to calculate cost
                };
                offCodeSum = offCodeSum.wrapping_add(*freq);
            }
            opt_state.offCodeSum = offCodeSum;
        } else {
            // first block, no dictionary
            if compressedLiterals {
                // base initial cost of literals on direct frequency within src
                let mut lit_0 = MaxLit;
                HIST_count_simple(
                    opt_state.litFreq.cast(),
                    &mut lit_0,
                    core::slice::from_raw_parts(src, srcSize), // use raw first block to init statistics
                );
                opt_state.litSum =
                    ZSTD_downscaleStats(opt_state.litFreq_mut(), 8, BaseDirective::Possible);
            }

            let baseLLfreqs: [u32; MaxLL as usize + 1] = [
                4, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
                1, 1, 1, 1, 1, 1, 1, 1,
            ];
            opt_state.litLengthFreq_mut().copy_from_slice(&baseLLfreqs);
            opt_state.litLengthSum = baseLLfreqs.iter().sum();

            opt_state.matchLengthFreq_mut().fill(1);
            opt_state.matchLengthSum = u32::from(MaxML) + 1;

            let baseOFCfreqs: [u32; MaxOff as usize + 1] = [
                6, 2, 1, 1, 2, 3, 4, 4, 4, 3, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
                1, 1, 1, 1,
            ];
            opt_state.offCodeFreq_mut().copy_from_slice(&baseOFCfreqs);
            opt_state.offCodeSum = baseOFCfreqs.iter().sum();
        }
    } else {
        // new block: scale down accumulated statistics
        if compressedLiterals {
            opt_state.litSum = ZSTD_scaleStats(opt_state.litFreq_mut(), 12);
        }
        opt_state.litLengthSum = ZSTD_scaleStats(opt_state.litLengthFreq_mut(), 11);
        opt_state.matchLengthSum = ZSTD_scaleStats(opt_state.matchLengthFreq_mut(), 11);
        opt_state.offCodeSum = ZSTD_scaleStats(opt_state.offCodeFreq_mut(), 11);
    }

    ZSTD_setBasePrices(opt_state, optLevel);
}

/// price of a single literal.
/// does not include price of literalLength symbol
unsafe fn ZSTD_rawLiteralCost(literal: u8, opt_state: &optState_t, optLevel: u32) -> u32 {
    if !ZSTD_compressedLiterals(opt_state) {
        return 8 * BITCOST_MULTIPLIER; // Uncompressed - 8 bytes per literal.
    }

    if opt_state.priceType == OptPrice::Predef {
        return 6 * BITCOST_MULTIPLIER; // 6 bit per literal - no statistic used
    }

    // dynamic statistics
    let litPriceMax = opt_state.litSumBasePrice.wrapping_sub(BITCOST_MULTIPLIER);
    let litPrice = Ord::min(
        WEIGHT(opt_state.litFreq()[usize::from(literal)], optLevel),
        litPriceMax,
    );
    opt_state.litSumBasePrice.wrapping_sub(litPrice)
}

/// Cost of literalLength symbol
unsafe fn ZSTD_litLengthPrice(litLength: u32, opt_state: &optState_t, optLevel: u32) -> u32 {
    if opt_state.priceType == OptPrice::Predef {
        return WEIGHT(litLength, optLevel);
    }

    // ZSTD_LLcode() can't compute litLength price for sizes >= ZSTD_BLOCKSIZE_MAX
    // because it isn't representable in the zstd format.
    // So instead just pretend it would cost 1 bit more than ZSTD_BLOCKSIZE_MAX - 1.
    // In such a case, the block would be all literals.
    if litLength == ZSTD_BLOCKSIZE_MAX as u32 {
        return BITCOST_MULTIPLIER.wrapping_add(ZSTD_litLengthPrice(
            (ZSTD_BLOCKSIZE_MAX - 1) as u32,
            opt_state,
            optLevel,
        ));
    }

    // dynamic statistics
    let llCode = ZSTD_LLcode(litLength);
    let freq = opt_state.litLengthFreq()[llCode as usize];
    (LL_bits[llCode as usize] as u32 * BITCOST_MULTIPLIER)
        .wrapping_add(opt_state.litLengthSumBasePrice)
        .wrapping_sub(WEIGHT(freq, optLevel))
}

/// Provides the cost of the match part (offset + matchLength) of a sequence.
/// Must be combined with ZSTD_fullLiteralsCost() to get the full cost of a sequence.
/// @offBase: sumtype, representing an offset or a repcode, and using numeric representation of ZSTD_storeSeq()
/// @optLevel: when <2, favors small offset for decompression speed (improved cache efficiency)
#[inline(always)]
unsafe fn ZSTD_getMatchPrice(
    offBase: u32,
    matchLength: u32,
    opt_state: &optState_t,
    optLevel: u32,
) -> u32 {
    let offCode = ZSTD_highbit32(offBase);
    let mlBase = matchLength.wrapping_sub(u32::from(MINMATCH));

    if opt_state.priceType == OptPrice::Predef {
        // fixed scheme, does not use statistics
        return WEIGHT(mlBase, optLevel)
            // emulated offset cost
            .wrapping_add(16u32.wrapping_add(offCode) * BITCOST_MULTIPLIER);
    }

    // dynamic statistics
    let offCodeFreq = opt_state.offCodeFreq()[offCode as usize];
    let mut price = (offCode * BITCOST_MULTIPLIER)
        .wrapping_add((opt_state.offCodeSumBasePrice).wrapping_sub(WEIGHT(offCodeFreq, optLevel)));
    if optLevel < 2 && offCode >= 20 {
        // handicap for long distance offsets, favor decompression speed
        price = price.wrapping_add(offCode.wrapping_sub(19) * 2 * BITCOST_MULTIPLIER);
    }

    // match Length
    let mlCode = ZSTD_MLcode(mlBase);
    let matchLengthFreq = opt_state.matchLengthFreq()[mlCode as usize];
    price = price.wrapping_add(
        (ML_bits[mlCode as usize] as u32 * BITCOST_MULTIPLIER).wrapping_add(
            (opt_state.matchLengthSumBasePrice).wrapping_sub(WEIGHT(matchLengthFreq, optLevel)),
        ),
    );

    price = price.wrapping_add(BITCOST_MULTIPLIER / 5); // heuristic: make matches a bit more costly to favor less sequences -> faster decompression speed

    price
}

/// assumption: literals + litLength <= iend
unsafe fn ZSTD_updateStats(
    opt_state: &mut optState_t,
    litLength: u32,
    literals: *const u8,
    offBase: u32,
    matchLength: u32,
) {
    // literals
    if ZSTD_compressedLiterals(opt_state) {
        let litFreq = opt_state.litFreq_mut();
        for u in 0..litLength {
            let freq = &mut litFreq[usize::from(*literals.add(u as usize))];
            *freq = freq.wrapping_add(ZSTD_LITFREQ_ADD);
        }
        opt_state.litSum = opt_state.litSum.wrapping_add(litLength * ZSTD_LITFREQ_ADD);
    }

    // literal Length
    let llCode = ZSTD_LLcode(litLength);
    let freq = &mut opt_state.litLengthFreq_mut()[llCode as usize];
    *freq = freq.wrapping_add(1);
    opt_state.litLengthSum = opt_state.litLengthSum.wrapping_add(1);

    // offset code: follows storeSeq() numeric representation
    let offCode = ZSTD_highbit32(offBase);
    let freq = &mut opt_state.offCodeFreq_mut()[offCode as usize];
    *freq = freq.wrapping_add(1);
    opt_state.offCodeSum = opt_state.offCodeSum.wrapping_add(1);

    // match Length
    let mlBase = matchLength.wrapping_sub(u32::from(MINMATCH));
    let mlCode = ZSTD_MLcode(mlBase);
    let freq = &mut opt_state.matchLengthFreq_mut()[mlCode as usize];
    *freq = freq.wrapping_add(1);
    opt_state.matchLengthSum = opt_state.matchLengthSum.wrapping_add(1);
}

// function safe only for comparisons
// assumption: memPtr must be at least 4 bytes before end of buffer
#[inline]
unsafe fn ZSTD_readMINMATCH(memPtr: *const core::ffi::c_void, length: u32) -> u32 {
    match length {
        3 => {
            if cfg!(target_endian = "little") {
                MEM_read32(memPtr) << 8
            } else {
                MEM_read32(memPtr) >> 8
            }
        }
        _ => MEM_read32(memPtr),
    }
}

// Update hashTable3 up to ip (excluded)
// Assumption: always within prefix (i.e. not within extDict)
unsafe fn ZSTD_insertAndFindFirstIndexHash3(
    ms: &ZSTD_MatchState_t,
    nextToUpdate3: *mut u32,
    ip: *const u8,
) -> u32 {
    let hashTable3 = ms.hashTable3;
    let hashLog3 = ms.hashLog3;
    let base = ms.window.base;
    let target = ip.wrapping_offset_from(base) as core::ffi::c_long as u32;
    let hash3 = ZSTD_hash32Ptr::<3>(ip as *const core::ffi::c_void, hashLog3);

    for idx in *nextToUpdate3..target {
        *hashTable3.add(ZSTD_hash32Ptr::<3>(
            base.wrapping_offset(idx as isize) as *const core::ffi::c_void,
            hashLog3,
        )) = idx;
    }

    *nextToUpdate3 = target;
    *hashTable3.add(hash3)
}

/// Add one or multiple positions to tree.
///
/// @param ip assumed <= iend-8 .
/// @param target The target of ZSTD_updateTree_internal() - we are filling to this position
///
/// # Returns
///
/// The number of positions added
unsafe fn ZSTD_insertBt1(
    ms: &ZSTD_MatchState_t,
    ip: *const u8,
    iend: *const u8,
    target: u32,
    mls: u32,
    extDict: bool,
) -> u32 {
    let cParams: *const ZSTD_compressionParameters = &ms.cParams;
    let hashTable = ms.hashTable;
    let hashLog = (*cParams).hashLog;
    let h = ZSTD_hashPtr(ip as *const core::ffi::c_void, hashLog, mls);
    let bt = ms.chainTable;
    let btLog = ((*cParams).chainLog).wrapping_sub(1);
    let btMask = ((1 << btLog) - 1) as u32;
    let mut matchIndex = *hashTable.add(h);
    let mut commonLengthSmaller = 0;
    let mut commonLengthLarger = 0;
    let base = ms.window.base;
    let dictBase = ms.window.dictBase;
    let dictLimit = ms.window.dictLimit;
    let dictEnd = dictBase.wrapping_offset(dictLimit as isize);
    let prefixStart = base.wrapping_offset(dictLimit as isize);
    let curr = ip.wrapping_offset_from(base) as core::ffi::c_long as u32;
    let btLow = if btMask >= curr {
        0
    } else {
        curr.wrapping_sub(btMask)
    };
    let mut smallerPtr = bt.offset((2 * (curr & btMask)) as isize);
    let mut largerPtr = smallerPtr.add(1);
    let mut dummy32: u32 = 0;
    // windowLow is based on target because
    // we only need positions that will be in the window at the end of the tree update.
    let windowLow = ms.get_lowest_match_index(target, (*cParams).windowLog);
    let mut matchEndIdx = curr.wrapping_add(8).wrapping_add(1);
    let mut bestLength = 8;
    let mut nbCompares = 1u32 << (*cParams).searchLog;

    *hashTable.add(h) = curr; // Update Hash Table

    while nbCompares != 0 && matchIndex >= windowLow {
        let nextPtr = bt.offset((2 * (matchIndex & btMask)) as isize);
        // guaranteed minimum nb of common bytes
        let mut matchLength = commonLengthSmaller.min(commonLengthLarger);

        let mut match_0: *const u8;
        if !extDict || (matchIndex as usize).wrapping_add(matchLength) >= dictLimit as usize {
            match_0 = base.wrapping_offset(matchIndex as isize);
            matchLength = matchLength.wrapping_add(ZSTD_count(
                ip.add(matchLength),
                match_0.add(matchLength),
                iend,
            ));
        } else {
            match_0 = dictBase.wrapping_offset(matchIndex as isize);
            matchLength = matchLength.wrapping_add(ZSTD_count_2segments(
                ip.add(matchLength),
                match_0.add(matchLength),
                iend,
                dictEnd,
                prefixStart,
            ));
            if (matchIndex as usize).wrapping_add(matchLength) >= dictLimit as usize {
                // to prepare for next usage of match[matchLength]
                match_0 = base.wrapping_offset(matchIndex as isize);
            }
        }

        if matchLength > bestLength {
            bestLength = matchLength;
            if matchLength > matchEndIdx.wrapping_sub(matchIndex) as usize {
                matchEndIdx = matchIndex.wrapping_add(matchLength as u32);
            }
        }

        // equal: no way to know if inf or sup
        if ip.add(matchLength) == iend {
            break; // drop, to guarantee consistency; miss a bit of compression, but other solutions can corrupt tree
        } else {
            if (*match_0.add(matchLength) as core::ffi::c_int)
                < *ip.add(matchLength) as core::ffi::c_int
            {
                // match is smaller than current
                *smallerPtr = matchIndex; // update smaller idx
                commonLengthSmaller = matchLength; // all smaller will now have at least this guaranteed common length
                if matchIndex <= btLow {
                    smallerPtr = &mut dummy32;
                    break; // beyond tree size, stop searching
                } else {
                    smallerPtr = nextPtr.add(1); // new "candidate" => larger than match, which was smaller than target
                    matchIndex = *nextPtr.add(1); // new matchIndex, larger than previous and closer to current
                }
            } else {
                // match is larger than current
                *largerPtr = matchIndex;
                commonLengthLarger = matchLength;
                if matchIndex <= btLow {
                    largerPtr = &mut dummy32;
                    break; // beyond tree size, stop searching
                } else {
                    largerPtr = nextPtr;
                    matchIndex = *nextPtr;
                }
            }
            nbCompares = nbCompares.wrapping_sub(1);
        }
    }

    *largerPtr = 0;
    *smallerPtr = *largerPtr;

    let mut positions = 0;
    if bestLength > 384 {
        // speed optimization
        positions = (bestLength.wrapping_sub(384) as u32).min(192);
    }
    positions.max(matchEndIdx.wrapping_sub(curr.wrapping_add(8)))
}

#[inline(always)]
unsafe fn ZSTD_updateTree_internal(
    ms: &mut ZSTD_MatchState_t,
    ip: *const u8,
    iend: *const u8,
    mls: u32,
    dictMode: DictMode,
) {
    let base = ms.window.base;
    let target = ip.wrapping_offset_from(base) as core::ffi::c_long as u32;
    let mut idx = ms.nextToUpdate;

    while idx < target {
        let forward = ZSTD_insertBt1(
            ms,
            base.wrapping_offset(idx as isize),
            iend,
            target,
            mls,
            dictMode == DictMode::ExtDict,
        );
        idx = idx.wrapping_add(forward);
    }

    ms.nextToUpdate = target;
}

pub unsafe fn ZSTD_updateTree(ms: &mut ZSTD_MatchState_t, ip: *const u8, iend: *const u8) {
    ZSTD_updateTree_internal(ms, ip, iend, ms.cParams.minMatch, DictMode::NoDict);
}

#[inline(always)]
unsafe fn ZSTD_insertBtAndGetAllMatches(
    matches: &mut [ZSTD_match_t; ZSTD_OPT_SIZE],
    ms: &mut ZSTD_MatchState_t,
    nextToUpdate3: *mut u32,
    ip: *const u8,
    iLimit: *const u8,
    dictMode: DictMode,
    rep: &RepCodes,
    ll0: bool,
    lengthToBeat: u32,
    mls: u32,
) -> usize {
    let cParams = &ms.cParams;
    let sufficient_len = cParams.targetLength.min((1 << 12) - 1);
    let base = ms.window.base;
    let curr = ip.wrapping_offset_from(base) as core::ffi::c_long as u32;
    let hashLog = cParams.hashLog;
    let minMatch = (if mls == 3 { 3 } else { 4 }) as u32;
    let hashTable = ms.hashTable;
    let h = ZSTD_hashPtr(ip as *const core::ffi::c_void, hashLog, mls);
    let mut matchIndex = *hashTable.add(h);
    let bt = ms.chainTable;
    let btLog = cParams.chainLog.wrapping_sub(1);
    let btMask = (1u32 << btLog).wrapping_sub(1);
    let mut commonLengthSmaller = 0;
    let mut commonLengthLarger = 0;
    let dictBase = ms.window.dictBase;
    let dictLimit = ms.window.dictLimit;
    let dictEnd = dictBase.wrapping_offset(dictLimit as isize);
    let prefixStart = base.wrapping_offset(dictLimit as isize);
    let btLow = if btMask >= curr {
        0
    } else {
        curr.wrapping_sub(btMask)
    };
    let windowLow = ms.get_lowest_match_index(curr, cParams.windowLog);
    let matchLow = if windowLow != 0 { windowLow } else { 1 };
    let mut smallerPtr = bt.offset((2 * (curr & btMask)) as isize);
    let mut largerPtr = bt.offset((2 * (curr & btMask)) as isize).add(1);
    let mut matchEndIdx = curr.wrapping_add(8).wrapping_add(1); // farthest referenced position of any match => detects repetitive patterns
    let mut dummy32: u32 = 0;
    let mut mnum = 0usize;
    let mut nbCompares = 1u32 << cParams.searchLog;

    let dms = if dictMode == DictMode::DictMatchState {
        ms.dictMatchState
    } else {
        core::ptr::null()
    };
    let dmsCParams = if dictMode == DictMode::DictMatchState {
        &(*dms).cParams
    } else {
        core::ptr::null()
    };
    let dmsBase = if dictMode == DictMode::DictMatchState {
        (*dms).window.base
    } else {
        core::ptr::null()
    };
    let dmsEnd = if dictMode == DictMode::DictMatchState {
        (*dms).window.nextSrc
    } else {
        core::ptr::null()
    };
    let dmsHighLimit = if dictMode == DictMode::DictMatchState {
        dmsEnd.offset_from(dmsBase) as core::ffi::c_long as u32
    } else {
        0
    };
    let dmsLowLimit = if dictMode == DictMode::DictMatchState {
        (*dms).window.lowLimit
    } else {
        0
    };
    let dmsIndexDelta = if dictMode == DictMode::DictMatchState {
        windowLow.wrapping_sub(dmsHighLimit)
    } else {
        0
    };
    let dmsHashLog = if dictMode == DictMode::DictMatchState {
        (*dmsCParams).hashLog
    } else {
        hashLog
    };
    let dmsBtLog = if dictMode == DictMode::DictMatchState {
        ((*dmsCParams).chainLog).wrapping_sub(1)
    } else {
        btLog
    };
    let dmsBtMask = if dictMode == DictMode::DictMatchState {
        (1u32 << dmsBtLog).wrapping_sub(1)
    } else {
        0
    };
    let dmsBtLow = if dictMode == DictMode::DictMatchState
        && dmsBtMask < dmsHighLimit.wrapping_sub(dmsLowLimit)
    {
        dmsHighLimit.wrapping_sub(dmsBtMask)
    } else {
        dmsLowLimit
    };

    let mut bestLength = lengthToBeat.wrapping_sub(1) as usize;

    // check repCode
    let lastR = ZSTD_REP_NUM.wrapping_add(ll0 as u32);
    let repOffsets = [rep[0], rep[1], rep[2], rep[0].wrapping_sub(1)];
    for (repCode, &repOffset) in (ll0 as u32..lastR).zip(&repOffsets[usize::from(ll0)..]) {
        let repIndex = curr.wrapping_sub(repOffset);
        let mut repLen = 0;
        // intentional overflow, discards 0 and -1, equivalent to `curr > repIndex >= dictLimit
        if repOffset.wrapping_sub(1) < curr.wrapping_sub(dictLimit) {
            // We must validate the repcode offset because when we're using a dictionary the
            // valid offset range shrinks when the dictionary goes out of bounds.
            if (repIndex >= windowLow)
                & (ZSTD_readMINMATCH(ip as *const core::ffi::c_void, minMatch)
                    == ZSTD_readMINMATCH(
                        ip.sub(repOffset as usize) as *const core::ffi::c_void,
                        minMatch,
                    ))
            {
                repLen = (ZSTD_count(
                    ip.offset(minMatch as isize),
                    ip.offset(minMatch as isize).sub(repOffset as usize),
                    iLimit,
                ) as u32)
                    .wrapping_add(minMatch);
            }
        } else {
            let repMatch = if dictMode == DictMode::DictMatchState {
                dmsBase
                    .offset(repIndex as isize)
                    .sub(dmsIndexDelta as usize)
            } else {
                dictBase.wrapping_offset(repIndex as isize)
            };

            // intentional overflow, equivalent to `curr > repIndex >= windowLow`
            if dictMode == DictMode::ExtDict
                && (repOffset.wrapping_sub(1) < curr.wrapping_sub(windowLow))
                    & ZSTD_index_overlap_check(dictLimit, repIndex)
                && ZSTD_readMINMATCH(ip as *const core::ffi::c_void, minMatch)
                    == ZSTD_readMINMATCH(repMatch as *const core::ffi::c_void, minMatch)
            {
                repLen = (ZSTD_count_2segments(
                    ip.offset(minMatch as isize),
                    repMatch.offset(minMatch as isize),
                    iLimit,
                    dictEnd,
                    prefixStart,
                ) as u32)
                    .wrapping_add(minMatch);
            }
            // intentional overflow, equivalent to `curr > repIndex >= dmsLowLimit`
            if dictMode == DictMode::DictMatchState
                && (repOffset.wrapping_sub(1)
                    < curr.wrapping_sub(dmsLowLimit.wrapping_add(dmsIndexDelta)))
                    & ZSTD_index_overlap_check(dictLimit, repIndex)
                && ZSTD_readMINMATCH(ip as *const core::ffi::c_void, minMatch)
                    == ZSTD_readMINMATCH(repMatch as *const core::ffi::c_void, minMatch)
            {
                repLen = (ZSTD_count_2segments(
                    ip.offset(minMatch as isize),
                    repMatch.offset(minMatch as isize),
                    iLimit,
                    dmsEnd,
                    prefixStart,
                ) as u32)
                    .wrapping_add(minMatch);
            }
        }

        // save longer solution
        if repLen as usize > bestLength {
            bestLength = repLen as usize;
            let off = repCode.wrapping_sub(ll0 as u32).wrapping_add(1); // expect value between 1 and 3
            matches[mnum] = ZSTD_match_t::new(off, repLen);
            mnum = mnum.wrapping_add(1);
            if (repLen > sufficient_len) | (ip.offset(repLen as isize) == iLimit) {
                return mnum;
            }
        }
    }

    // HC3 match finder
    if mls == 3 && bestLength < mls as usize {
        let matchIndex3 = ZSTD_insertAndFindFirstIndexHash3(ms, nextToUpdate3, ip);
        // heuristic: longer distance likely too expensive
        if (matchIndex3 >= matchLow) & (curr.wrapping_sub(matchIndex3) < (1 << 18)) {
            let mlen = if dictMode == DictMode::NoDict
                || dictMode == DictMode::DictMatchState
                || matchIndex3 >= dictLimit
            {
                let match_0 = base.wrapping_offset(matchIndex3 as isize);
                ZSTD_count(ip, match_0, iLimit)
            } else {
                let match_1 = dictBase.wrapping_offset(matchIndex3 as isize);
                ZSTD_count_2segments(ip, match_1, iLimit, dictEnd, prefixStart)
            };

            // save best solution
            if mlen >= mls as usize {
                bestLength = mlen;
                let off = curr.wrapping_sub(matchIndex3).wrapping_add(ZSTD_REP_NUM);
                matches[0] = ZSTD_match_t::new(off, mlen as u32);
                mnum = 1;
                if (mlen > sufficient_len as usize) | (ip.add(mlen) == iLimit) {
                    ms.nextToUpdate = curr.wrapping_add(1); // skip insertion
                    return 1;
                }
            }
        }

        // no dictMatchState lookup: dicts don't have a populated HC3 table
    }

    *hashTable.add(h) = curr; // Update Hash Table

    while nbCompares != 0 && matchIndex >= matchLow {
        let nextPtr = bt.offset((2 * (matchIndex & btMask)) as isize);
        // guaranteed minimum nb of common bytes
        let mut matchLength = commonLengthSmaller.min(commonLengthLarger);

        let mut match_2: *const u8;
        if dictMode == DictMode::NoDict
            || dictMode == DictMode::DictMatchState
            || (matchIndex as usize).wrapping_add(matchLength) >= dictLimit as usize
        {
            match_2 = base.wrapping_offset(matchIndex as isize);
            if matchIndex >= dictLimit {
                // ensure early section of match is equal as expected
                debug_assert_eq!(libc::memcmp(match_2.cast(), ip.cast(), matchLength), 0);
            }
            matchLength = matchLength.wrapping_add(ZSTD_count(
                ip.add(matchLength),
                match_2.add(matchLength),
                iLimit,
            ));
        } else {
            match_2 = dictBase.wrapping_offset(matchIndex as isize);
            matchLength = matchLength.wrapping_add(ZSTD_count_2segments(
                ip.add(matchLength),
                match_2.add(matchLength),
                iLimit,
                dictEnd,
                prefixStart,
            ));
            if (matchIndex as usize).wrapping_add(matchLength) >= dictLimit as usize {
                // prepare for match[matchLength] read
                match_2 = base.offset(matchIndex as isize);
            }
        }

        if matchLength > bestLength {
            if matchLength > matchEndIdx.wrapping_sub(matchIndex) as usize {
                matchEndIdx = matchIndex.wrapping_add(matchLength as u32);
            }
            bestLength = matchLength;
            let off = curr.wrapping_sub(matchIndex).wrapping_add(ZSTD_REP_NUM);
            matches[mnum] = ZSTD_match_t::new(off, matchLength as u32);
            mnum = mnum.wrapping_add(1);
            // equal: no way to know if inf or sup
            if (matchLength > ZSTD_OPT_NUM) | (ip.add(matchLength) == iLimit) {
                if dictMode == DictMode::DictMatchState {
                    nbCompares = 0; // break should also skip searching dms
                }
                break; // drop, to preserve bt consistency (miss a little bit of compression)
            }
        }

        if (*match_2.add(matchLength) as core::ffi::c_int)
            < *ip.add(matchLength) as core::ffi::c_int
        {
            // match smaller than current
            *smallerPtr = matchIndex; // update smaller idx
            commonLengthSmaller = matchLength; // all smaller will now have at least this guaranteed common length
            if matchIndex <= btLow {
                smallerPtr = &mut dummy32;
                break; // beyond tree size, stop the search
            } else {
                smallerPtr = nextPtr.add(1); // new candidate => larger than match, which was smaller than current
                matchIndex = *nextPtr.add(1); // new matchIndex, larger than previous, closer to current
            }
        } else {
            *largerPtr = matchIndex;
            commonLengthLarger = matchLength;
            if matchIndex <= btLow {
                largerPtr = &mut dummy32;
                break; // beyond tree size, stop the search
            } else {
                largerPtr = nextPtr;
                matchIndex = *nextPtr;
            }
        }

        nbCompares = nbCompares.wrapping_sub(1);
    }

    *largerPtr = 0;
    *smallerPtr = *largerPtr;

    if dictMode == DictMode::DictMatchState && nbCompares != 0 {
        let dmsH = ZSTD_hashPtr(ip as *const core::ffi::c_void, dmsHashLog, mls);
        let mut dictMatchIndex = *((*dms).hashTable).add(dmsH);
        let dmsBt: *const u32 = (*dms).chainTable;
        commonLengthLarger = 0;
        commonLengthSmaller = commonLengthLarger;
        while nbCompares != 0 && dictMatchIndex > dmsLowLimit {
            let nextPtr_0 = dmsBt.offset((2 * (dictMatchIndex & dmsBtMask)) as isize);
            let mut matchLength_0 = commonLengthSmaller.min(commonLengthLarger); // guaranteed minimum number of common bytes
            let mut match_3 = dmsBase.offset(dictMatchIndex as isize);
            matchLength_0 = matchLength_0.wrapping_add(ZSTD_count_2segments(
                ip.add(matchLength_0),
                match_3.add(matchLength_0),
                iLimit,
                dmsEnd,
                prefixStart,
            ));
            if (dictMatchIndex as usize).wrapping_add(matchLength_0) >= dmsHighLimit as usize {
                // to prepare for next usage of match[matchLength]
                match_3 = base
                    .offset(dictMatchIndex as isize)
                    .offset(dmsIndexDelta as isize);
            }

            if matchLength_0 > bestLength {
                matchIndex = dictMatchIndex.wrapping_add(dmsIndexDelta);
                if matchLength_0 > matchEndIdx.wrapping_sub(matchIndex) as usize {
                    matchEndIdx = matchIndex.wrapping_add(matchLength_0 as u32);
                }
                bestLength = matchLength_0;
                let off = curr.wrapping_sub(matchIndex).wrapping_add(ZSTD_REP_NUM);
                matches[mnum] = ZSTD_match_t::new(off, matchLength_0 as u32);
                mnum = mnum.wrapping_add(1);
                // equal: no way to know if inf or sup
                if (matchLength_0 > ZSTD_OPT_NUM) | (ip.add(matchLength_0) == iLimit) {
                    break; // drop, to guarantee consistency (miss a little bit of compression)
                }
            }

            if dictMatchIndex <= dmsBtLow {
                break; // beyond tree size, stop the search
            }
            if (*match_3.add(matchLength_0) as core::ffi::c_int)
                < *ip.add(matchLength_0) as core::ffi::c_int
            {
                commonLengthSmaller = matchLength_0; // all smaller will now have at least this guaranteed common length
                dictMatchIndex = *nextPtr_0.add(1); // new matchIndex larger than previous (closer to current)
            } else {
                // match is larger than current
                commonLengthLarger = matchLength_0;
                dictMatchIndex = *nextPtr_0;
            }
            nbCompares = nbCompares.wrapping_sub(1);
        }
    }

    ms.nextToUpdate = matchEndIdx.wrapping_sub(8); // skip repetitive patterns

    mnum
}

#[inline(always)]
unsafe fn ZSTD_btGetAllMatches_internal<DICT_MODE: DictModeMarker, const MLS: u32>(
    matches: &mut [ZSTD_match_t; ZSTD_OPT_SIZE],
    ms: &mut ZSTD_MatchState_t,
    nextToUpdate3: *mut u32,
    ip: *const u8,
    iHighLimit: *const u8,
    rep: &RepCodes,
    ll0: bool,
    lengthToBeat: u32,
) -> usize {
    if ip < ms.window.base.wrapping_offset(ms.nextToUpdate as isize) {
        return 0; // skipped area
    }
    ZSTD_updateTree_internal(ms, ip, iHighLimit, MLS, DICT_MODE::DICT_MODE);
    ZSTD_insertBtAndGetAllMatches(
        matches,
        ms,
        nextToUpdate3,
        ip,
        iHighLimit,
        DICT_MODE::DICT_MODE,
        rep,
        ll0,
        lengthToBeat,
        MLS,
    )
}

fn ZSTD_selectBtGetAllMatches(ms: &ZSTD_MatchState_t, dictMode: DictMode) -> ZSTD_getAllMatchesFn {
    let getAllMatchesFns: [[ZSTD_getAllMatchesFn; 4]; 3] = [
        [
            ZSTD_btGetAllMatches_internal::<NoDict, 3>,
            ZSTD_btGetAllMatches_internal::<NoDict, 4>,
            ZSTD_btGetAllMatches_internal::<NoDict, 5>,
            ZSTD_btGetAllMatches_internal::<NoDict, 6>,
        ],
        [
            ZSTD_btGetAllMatches_internal::<ExtDict, 3>,
            ZSTD_btGetAllMatches_internal::<ExtDict, 4>,
            ZSTD_btGetAllMatches_internal::<ExtDict, 5>,
            ZSTD_btGetAllMatches_internal::<ExtDict, 6>,
        ],
        [
            ZSTD_btGetAllMatches_internal::<DictMatchState, 3>,
            ZSTD_btGetAllMatches_internal::<DictMatchState, 4>,
            ZSTD_btGetAllMatches_internal::<DictMatchState, 5>,
            ZSTD_btGetAllMatches_internal::<DictMatchState, 6>,
        ],
    ];
    let mls = ms.cParams.minMatch.clamp(3, 6);
    getAllMatchesFns[dictMode as usize][mls.wrapping_sub(3) as usize]
}

/// Calculates the beginning and end of the next match in the current block.
/// Updates 'pos' and 'posInSequence' of the ldmSeqStore.
unsafe fn ZSTD_opt_getNextMatchAndUpdateSeqStore(
    optLdm: &mut ZSTD_optLdm_t,
    currPosInBlock: u32,
    blockBytesRemaining: u32,
) {
    // Setting match end position to MAX to ensure we never use an LDM during this block
    if optLdm.seqStore.size == 0 || optLdm.seqStore.pos >= optLdm.seqStore.size {
        optLdm.startPosInBlock = u32::MAX;
        optLdm.endPosInBlock = u32::MAX;
        return;
    }
    // Calculate appropriate bytes left in matchLength and litLength
    // after adjusting based on ldmSeqStore->posInSequence
    let currSeq = *optLdm.seqStore.seq.add(optLdm.seqStore.pos);
    let currBlockEndPos = currPosInBlock.wrapping_add(blockBytesRemaining);
    let literalsBytesRemaining = if optLdm.seqStore.posInSequence < currSeq.litLength as usize {
        (currSeq.litLength).wrapping_sub(optLdm.seqStore.posInSequence as u32)
    } else {
        0
    };
    let matchBytesRemaining = if literalsBytesRemaining == 0 {
        (currSeq.matchLength)
            .wrapping_sub((optLdm.seqStore.posInSequence as u32).wrapping_sub(currSeq.litLength))
    } else {
        currSeq.matchLength
    };

    // If there are more literal bytes than bytes remaining in block, no ldm is possible
    if literalsBytesRemaining >= blockBytesRemaining {
        optLdm.startPosInBlock = u32::MAX;
        optLdm.endPosInBlock = u32::MAX;
        ZSTD_ldm_skipRawSeqStoreBytes(&mut optLdm.seqStore, blockBytesRemaining as usize);
        return;
    }

    // Matches may be < minMatch by this process. In that case, we will reject them
    // when we are deciding whether or not to add the ldm
    optLdm.startPosInBlock = currPosInBlock.wrapping_add(literalsBytesRemaining);
    optLdm.endPosInBlock = optLdm.startPosInBlock.wrapping_add(matchBytesRemaining);
    optLdm.offset = currSeq.offset;

    if optLdm.endPosInBlock > currBlockEndPos {
        // Match ends after the block ends, we can't use the whole match
        optLdm.endPosInBlock = currBlockEndPos;
        ZSTD_ldm_skipRawSeqStoreBytes(
            &mut optLdm.seqStore,
            currBlockEndPos.wrapping_sub(currPosInBlock) as usize,
        );
    } else {
        // Consume number of bytes equal to size of sequence left
        ZSTD_ldm_skipRawSeqStoreBytes(
            &mut optLdm.seqStore,
            literalsBytesRemaining.wrapping_add(matchBytesRemaining) as usize,
        );
    }
}

/// Adds a match if it's long enough,
/// based on it's 'matchStartPosInBlock' and 'matchEndPosInBlock',
/// into 'matches'. Maintains the correct ordering of 'matches'.
unsafe fn ZSTD_optLdm_maybeAddMatch(
    matches: &mut [ZSTD_match_t; ZSTD_OPT_SIZE],
    nbMatches: &mut usize,
    optLdm: &ZSTD_optLdm_t,
    currPosInBlock: u32,
    minMatch: u32,
) {
    let posDiff = currPosInBlock.wrapping_sub(optLdm.startPosInBlock);
    // Note: ZSTD_match_t actually contains offBase and matchLength (before subtracting MINMATCH)
    let candidateMatchLength = (optLdm.endPosInBlock)
        .wrapping_sub(optLdm.startPosInBlock)
        .wrapping_sub(posDiff);

    // Ensure that current block position is not outside of the match
    if currPosInBlock < optLdm.startPosInBlock
        || currPosInBlock >= optLdm.endPosInBlock
        || candidateMatchLength < minMatch
    {
        return;
    }

    if *nbMatches == 0
        || *nbMatches < ZSTD_OPT_NUM && candidateMatchLength > matches[*nbMatches - 1].len
    {
        let candidateOffBase = optLdm.offset.wrapping_add(ZSTD_REP_NUM);
        matches[*nbMatches] = ZSTD_match_t::new(candidateOffBase, candidateMatchLength);
        *nbMatches = (*nbMatches).wrapping_add(1);
    }
}

/// Wrapper function to update ldm seq store and call ldm functions as necessary.
unsafe fn ZSTD_optLdm_processMatchCandidate(
    optLdm: &mut ZSTD_optLdm_t,
    matches: &mut [ZSTD_match_t; ZSTD_OPT_SIZE],
    nbMatches: &mut usize,
    currPosInBlock: u32,
    remainingBytes: u32,
    minMatch: u32,
) {
    if optLdm.seqStore.size == 0 || optLdm.seqStore.pos >= optLdm.seqStore.size {
        return;
    }

    if currPosInBlock >= optLdm.endPosInBlock {
        if currPosInBlock > optLdm.endPosInBlock {
            // The position at which ZSTD_optLdm_processMatchCandidate() is called is not necessarily
            // at the end of a match from the ldm seq store, and will often be some bytes
            // over beyond matchEndPosInBlock. As such, we need to correct for these "overshoots"
            let posOvershoot = currPosInBlock.wrapping_sub(optLdm.endPosInBlock);
            ZSTD_ldm_skipRawSeqStoreBytes(&mut optLdm.seqStore, posOvershoot as usize);
        }
        ZSTD_opt_getNextMatchAndUpdateSeqStore(optLdm, currPosInBlock, remainingBytes);
    }
    ZSTD_optLdm_maybeAddMatch(matches, nbMatches, optLdm, currPosInBlock, minMatch);
}

unsafe fn ZSTD_compressBlock_opt_generic<const OPT_LEVEL: u32>(
    ms: &mut ZSTD_MatchState_t,
    seqStore: &mut SeqStore_t,
    rep: &mut RepCodes,
    src: *const core::ffi::c_void,
    srcSize: usize,
    dictMode: DictMode,
) -> usize {
    let istart = src as *const u8;
    let mut ip = istart;
    let mut anchor = istart;
    let iend = istart.add(srcSize);
    let ilimit = iend.sub(8);
    let base = ms.window.base;
    let prefixStart = base.wrapping_offset(ms.window.dictLimit as isize);
    let cParams: *const ZSTD_compressionParameters = &ms.cParams;

    let getAllMatches = ZSTD_selectBtGetAllMatches(ms, dictMode);

    let sufficient_len = (*cParams).targetLength.min((1 << 12) - 1);
    let minMatch = (if (*cParams).minMatch == 3 { 3 } else { 4 }) as u32;
    let mut nextToUpdate3 = ms.nextToUpdate;

    let opt = &mut *ms.opt.priceTable;
    let matches = &mut *ms.opt.matchTable;
    let mut lastStretch = ZSTD_optimal_t {
        price: 0,
        off: 0,
        mlen: 0,
        litlen: 0,
        rep: [0; 3],
    };
    let mut optLdm = ZSTD_optLdm_t {
        seqStore: if !ms.ldmSeqStore.is_null() {
            *ms.ldmSeqStore
        } else {
            RawSeqStore_t::default()
        },
        startPosInBlock: 0,
        endPosInBlock: 0,
        offset: 0,
    };

    ZSTD_opt_getNextMatchAndUpdateSeqStore(
        &mut optLdm,
        ip.offset_from_unsigned(istart) as u32,
        iend.offset_from_unsigned(ip) as u32,
    );

    // init
    ZSTD_rescaleFreqs(&mut ms.opt, src as *const u8, srcSize, OPT_LEVEL);
    ip = ip.add(usize::from(ip == prefixStart));

    // Match Loop
    while ip < ilimit {
        // find first match
        let litlen = ip.offset_from_unsigned(anchor) as u32;
        let mut nbMatches = getAllMatches(
            matches,
            ms,
            &mut nextToUpdate3,
            ip,
            iend,
            rep,
            litlen == 0,
            minMatch,
        );
        ZSTD_optLdm_processMatchCandidate(
            &mut optLdm,
            matches,
            &mut nbMatches,
            ip.offset_from_unsigned(istart) as u32,
            iend.offset_from_unsigned(ip) as u32,
            minMatch,
        );
        if nbMatches == 0 {
            ip = ip.add(1);
        } else {
            // Match found: let's store this solution, and eventually find more candidates.
            // During this forward pass, @opt is used to store stretches,
            // defined as "a match followed by N literals".
            // Note how this is different from a Sequence, which is "N literals followed by a match".
            // Storing stretches allows us to store different match predecessors
            // for each literal position part of a literals run.

            // initialize opt[0]
            opt[0].mlen = 0; // there are only literals so far
            opt[0].litlen = litlen;
            opt[0].off = 0; // initialized to prevent UB

            // No need to include the actual price of the literals before the first match
            // because it is static for the duration of the forward pass, and is included
            // in every subsequent price. But, we include the literal length because
            // the cost variation of litlen depends on the value of litlen.
            opt[0].price = ZSTD_litLengthPrice(litlen, &ms.opt, OPT_LEVEL) as core::ffi::c_int;
            opt[0].rep = *rep;

            // large match -> immediate encoding
            let maxML = matches[nbMatches - 1].len;
            let maxOffBase = matches[nbMatches - 1].off;
            let mut cur: usize;
            let mut last_pos: usize;
            if maxML > sufficient_len {
                lastStretch.litlen = 0;
                lastStretch.mlen = maxML;
                lastStretch.off = maxOffBase;
                cur = 0;
                last_pos = maxML as usize;
            } else {
                // set prices for first matches starting position == 0
                for (pos, o) in (1..).zip(&mut opt[1..minMatch as usize]) {
                    o.price = ZSTD_MAX_PRICE;
                    o.mlen = 0;
                    o.litlen = litlen.wrapping_add(pos);
                    o.off = 0; // initialized to prevent UB
                    o.rep = [0; ZSTD_REP_NUM as usize]; // initialized to prevent UB
                }
                let mut pos = minMatch as usize;
                for m in &matches[..nbMatches] {
                    let offBase = m.off;
                    let end = m.len as usize;
                    while pos <= end {
                        let matchPrice = ZSTD_getMatchPrice(offBase, pos as u32, &ms.opt, OPT_LEVEL)
                            as core::ffi::c_int;
                        let sequencePrice = opt[0].price + matchPrice;
                        opt[pos].mlen = pos as u32;
                        opt[pos].off = offBase;
                        opt[pos].litlen = 0; // end of match
                        opt[pos].price = sequencePrice
                            + ZSTD_litLengthPrice(0, &ms.opt, OPT_LEVEL) as core::ffi::c_int;
                        opt[pos].rep = [0; ZSTD_REP_NUM as usize]; // initialized to prevent UB
                        pos = pos.wrapping_add(1);
                    }
                }
                last_pos = pos.wrapping_sub(1);
                opt[pos].price = ZSTD_MAX_PRICE;

                // check further positions
                cur = 1;
                loop {
                    if cur > last_pos {
                        lastStretch = opt[last_pos];
                        cur = last_pos.wrapping_sub(lastStretch.mlen as usize);
                        break;
                    }
                    let inr = ip.add(cur);

                    // Fix current position with one literal if cheaper
                    let litlen_0 = opt[cur - 1].litlen.wrapping_add(1);
                    let price = opt[cur - 1].price
                        + ZSTD_rawLiteralCost(*ip.add(cur).sub(1), &ms.opt, OPT_LEVEL)
                            as core::ffi::c_int
                        + (ZSTD_litLengthPrice(litlen_0, &ms.opt, OPT_LEVEL) as core::ffi::c_int
                            - ZSTD_litLengthPrice(litlen_0.wrapping_sub(1), &ms.opt, OPT_LEVEL)
                                as core::ffi::c_int);
                    if price <= opt[cur].price {
                        let prevMatch = opt[cur];
                        opt[cur] = opt[cur - 1];
                        opt[cur].litlen = litlen_0;
                        opt[cur].price = price;
                        if OPT_LEVEL >= 1
                            && prevMatch.litlen == 0
                            && (ZSTD_litLengthPrice(1, &ms.opt, OPT_LEVEL) as core::ffi::c_int
                                - ZSTD_litLengthPrice(0, &ms.opt, OPT_LEVEL) as core::ffi::c_int)
                                < 0
                            && ip.add(cur) < iend
                        {
                            // check next position, in case it would be cheaper
                            let with1literal = prevMatch.price
                                + ZSTD_rawLiteralCost(*ip.add(cur), &ms.opt, OPT_LEVEL)
                                    as core::ffi::c_int
                                + (ZSTD_litLengthPrice(1, &ms.opt, OPT_LEVEL) as core::ffi::c_int
                                    - ZSTD_litLengthPrice(0, &ms.opt, OPT_LEVEL)
                                        as core::ffi::c_int);
                            let withMoreLiterals = price
                                + ZSTD_rawLiteralCost(*ip.add(cur), &ms.opt, OPT_LEVEL)
                                    as core::ffi::c_int
                                + (ZSTD_litLengthPrice(litlen_0.wrapping_add(1), &ms.opt, OPT_LEVEL)
                                    as core::ffi::c_int
                                    - ZSTD_litLengthPrice(
                                        litlen_0.wrapping_add(1).wrapping_sub(1),
                                        &ms.opt,
                                        OPT_LEVEL,
                                    ) as core::ffi::c_int);
                            if with1literal < withMoreLiterals && with1literal < opt[cur + 1].price
                            {
                                // update offset history - before it disappears
                                let prev = cur.wrapping_sub(prevMatch.mlen as usize);
                                let newReps = ZSTD_newRep(
                                    &opt[prev].rep,
                                    prevMatch.off,
                                    opt[prev].litlen == 0,
                                );
                                opt[cur + 1] = prevMatch;
                                opt[cur + 1].rep = newReps;
                                opt[cur + 1].litlen = 1;
                                opt[cur + 1].price = with1literal;
                                last_pos = Ord::max(last_pos, cur + 1);
                            }
                        }
                    }

                    // Offset history is not updated during match comparison.
                    // Do it here, now that the match is selected and confirmed.
                    if opt[cur].litlen == 0 {
                        // just finished a match => alter offset history
                        let prev = cur.wrapping_sub(opt[cur].mlen as usize);
                        let newReps =
                            ZSTD_newRep(&opt[prev].rep, opt[cur].off, opt[prev].litlen == 0);
                        opt[cur].rep = newReps;
                    }

                    // last match must start at a minimum distance of 8 from oend
                    if inr <= ilimit {
                        if cur == last_pos {
                            lastStretch = opt[last_pos];
                            cur = last_pos.wrapping_sub(lastStretch.mlen as usize);
                            break;
                        }

                        // skip unpromising positions; about ~+6% speed, -0.01 ratio
                        if !(OPT_LEVEL == 0
                            && opt[cur + 1].price <= opt[cur].price + BITCOST_MULTIPLIER as i32 / 2)
                        {
                            let previousPrice = opt[cur].price;
                            let basePrice = previousPrice
                                + ZSTD_litLengthPrice(0, &ms.opt, OPT_LEVEL) as core::ffi::c_int;
                            let mut nbMatches = getAllMatches(
                                matches,
                                ms,
                                &mut nextToUpdate3,
                                inr,
                                iend,
                                &opt[cur].rep,
                                opt[cur].litlen == 0,
                                minMatch,
                            );

                            ZSTD_optLdm_processMatchCandidate(
                                &mut optLdm,
                                matches,
                                &mut nbMatches,
                                inr.offset_from_unsigned(istart) as u32,
                                iend.offset_from_unsigned(inr) as u32,
                                minMatch,
                            );

                            if nbMatches != 0 {
                                let longestML = matches[nbMatches - 1].len;
                                if longestML > sufficient_len
                                    || cur.wrapping_add(longestML as usize) >= ZSTD_OPT_NUM
                                    || ip.add(cur).offset(longestML as isize) >= iend
                                {
                                    lastStretch.mlen = longestML;
                                    lastStretch.off = matches[nbMatches - 1].off;
                                    lastStretch.litlen = 0;
                                    last_pos = cur.wrapping_add(longestML as usize);
                                    break;
                                } else {
                                    // set prices using matches found at position == cur
                                    for matchNb in 0..nbMatches {
                                        let offset = matches[matchNb].off;
                                        let lastML = matches[matchNb].len;
                                        let startML = if matchNb > 0 {
                                            matches[matchNb - 1].len.wrapping_add(1)
                                        } else {
                                            minMatch
                                        };

                                        // scan downward
                                        for mlen in (startML..lastML + 1).rev() {
                                            let pos = cur.wrapping_add(mlen as usize);
                                            let price = basePrice
                                                + ZSTD_getMatchPrice(
                                                    offset, mlen, &ms.opt, OPT_LEVEL,
                                                )
                                                    as core::ffi::c_int;

                                            if pos > last_pos || price < opt[pos].price {
                                                while last_pos < pos {
                                                    // fill empty positions, for future comparisons
                                                    last_pos = last_pos.wrapping_add(1);
                                                    opt[last_pos].price = ZSTD_MAX_PRICE;
                                                    // just needs to be != 0, to mean "not an end of match
                                                    opt[last_pos].litlen = 1;
                                                    // initialized to prevent UB
                                                    opt[last_pos].mlen = 0;
                                                    opt[last_pos].off = 0;
                                                    opt[last_pos].rep = [0; ZSTD_REP_NUM as usize];
                                                }
                                                opt[pos].mlen = mlen;
                                                opt[pos].off = offset;
                                                opt[pos].litlen = 0;
                                                opt[pos].price = price;
                                            } else if OPT_LEVEL == 0 {
                                                break; // early update abort; gets ~+10% speed for about -0.01 ratio loss
                                            }
                                        }
                                    }

                                    opt[last_pos.wrapping_add(1)].price = ZSTD_MAX_PRICE;
                                }
                            }
                        }
                    }

                    cur = cur.wrapping_add(1);
                }
            }

            if lastStretch.mlen == 0 {
                // no solution: all matches have been converted into literals
                ip = ip.add(last_pos);
            } else {
                // Update offset history
                if lastStretch.litlen == 0 {
                    // finishing on a match: update offset history
                    let reps = ZSTD_newRep(&opt[cur].rep, lastStretch.off, opt[cur].litlen == 0);
                    *rep = reps;
                } else {
                    *rep = lastStretch.rep;
                    cur = cur.wrapping_sub(lastStretch.litlen as usize);
                }

                // Let's write the shortest path solution.
                // It is stored in @opt in reverse order,
                // starting from @storeEnd (==cur+2),
                // effectively partially @opt overwriting.
                // Content is changed too:
                // - So far, @opt stored stretches, aka a match followed by literals
                // - Now, it will store sequences, aka literals followed by a match
                let storeEnd = cur.wrapping_add(2);
                let mut storeStart;
                let mut stretchPos = cur;

                if lastStretch.litlen > 0 {
                    // last "sequence" is unfinished: just a bunch of literals
                    opt[storeEnd].litlen = lastStretch.litlen;
                    opt[storeEnd].mlen = 0;
                    storeStart = storeEnd.wrapping_sub(1);
                    opt[storeStart] = lastStretch;
                }
                opt[storeEnd] = lastStretch; // note: litlen will be fixed
                storeStart = storeEnd;

                loop {
                    let nextStretch = opt[stretchPos];
                    opt[storeStart].litlen = nextStretch.litlen;
                    if nextStretch.mlen == 0 {
                        // reaching beginning of segment
                        break;
                    }
                    storeStart = storeStart.wrapping_sub(1);
                    opt[storeStart] = nextStretch; // note: litlen will be fixed
                    stretchPos = stretchPos
                        .wrapping_sub(nextStretch.litlen.wrapping_add(nextStretch.mlen) as usize);
                }

                // save sequences
                for entry in &opt[storeStart..storeEnd + 1] {
                    let llen = entry.litlen;
                    let mlen = entry.mlen;
                    let offBase = entry.off;
                    let advance = llen.wrapping_add(mlen);

                    if mlen == 0 {
                        // only literals => must be last "sequence", actually starting a new stream of sequences
                        ip = anchor.offset(llen as isize); // last "sequence" is a bunch of literals => don't progress anchor
                    } else {
                        ZSTD_updateStats(&mut ms.opt, llen, anchor, offBase, mlen);
                        ZSTD_storeSeq(
                            seqStore,
                            llen as usize,
                            anchor,
                            iend,
                            offBase,
                            mlen as usize,
                        );
                        anchor = anchor.offset(advance as isize);
                        ip = anchor;
                    }
                }

                // update all costs
                ZSTD_setBasePrices(&mut ms.opt, OPT_LEVEL);
            }
        }
    }

    // Return the last literals size
    iend.offset_from_unsigned(anchor)
}

pub unsafe fn ZSTD_compressBlock_btopt(
    ms: &mut ZSTD_MatchState_t,
    seqStore: &mut SeqStore_t,
    rep: &mut RepCodes,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    ZSTD_compressBlock_opt_generic::<0>(ms, seqStore, rep, src, srcSize, DictMode::NoDict)
}

/// Make a first compression pass, just to seed stats with more accurate starting values.
/// Only works on first block, with no dictionary and no ldm.
/// This function cannot error out, its narrow contract must be respected.
unsafe fn ZSTD_initStats_ultra(
    ms: &mut ZSTD_MatchState_t,
    seqStore: &mut SeqStore_t,
    rep: &mut RepCodes,
    src: *const core::ffi::c_void,
    srcSize: usize,
) {
    let mut tmpRep = *rep; // updated rep codes will sink here

    // generate stats into ms->opt
    ZSTD_compressBlock_opt_generic::<2>(ms, seqStore, &mut tmpRep, src, srcSize, DictMode::NoDict);

    // invalidate first scan from history, only keep entropy stats
    ZSTD_resetSeqStore(seqStore);
    ms.window.base = ms.window.base.wrapping_sub(srcSize);
    ms.window.dictLimit = ms.window.dictLimit.wrapping_add(srcSize as u32);
    ms.window.lowLimit = ms.window.dictLimit;
    ms.nextToUpdate = ms.window.dictLimit;
}

pub unsafe fn ZSTD_compressBlock_btultra(
    ms: &mut ZSTD_MatchState_t,
    seqStore: &mut SeqStore_t,
    rep: &mut RepCodes,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    ZSTD_compressBlock_opt_generic::<2>(ms, seqStore, rep, src, srcSize, DictMode::NoDict)
}

pub unsafe fn ZSTD_compressBlock_btultra2(
    ms: &mut ZSTD_MatchState_t,
    seqStore: &mut SeqStore_t,
    rep: &mut RepCodes,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    let curr = (src as *const u8).wrapping_offset_from(ms.window.base) as core::ffi::c_long as u32;

    // 2-passes strategy:
    // this strategy makes a first pass over first block to collect statistics
    // in order to seed next round's statistics with it.
    // After 1st pass, function forgets history, and starts a new block.
    // Consequently, this can only work if no data has been previously loaded in tables,
    // aka, no dictionary, no prefix, no ldm preprocessing.
    // The compression ratio gain is generally small (~0.5% on first block),
    // the cost is 2x cpu time on first block.
    if ms.opt.litLengthSum == 0
        && seqStore.sequences == seqStore.sequencesStart
        && ms.window.dictLimit == ms.window.lowLimit
        && curr == ms.window.dictLimit
        && srcSize > ZSTD_PREDEF_THRESHOLD
    {
        ZSTD_initStats_ultra(ms, seqStore, rep, src, srcSize);
    }

    ZSTD_compressBlock_opt_generic::<2>(ms, seqStore, rep, src, srcSize, DictMode::NoDict)
}

pub unsafe fn ZSTD_compressBlock_btopt_dictMatchState(
    ms: &mut ZSTD_MatchState_t,
    seqStore: &mut SeqStore_t,
    rep: &mut RepCodes,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    ZSTD_compressBlock_opt_generic::<0>(ms, seqStore, rep, src, srcSize, DictMode::DictMatchState)
}

pub unsafe fn ZSTD_compressBlock_btopt_extDict(
    ms: &mut ZSTD_MatchState_t,
    seqStore: &mut SeqStore_t,
    rep: &mut RepCodes,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    ZSTD_compressBlock_opt_generic::<0>(ms, seqStore, rep, src, srcSize, DictMode::ExtDict)
}

pub unsafe fn ZSTD_compressBlock_btultra_dictMatchState(
    ms: &mut ZSTD_MatchState_t,
    seqStore: &mut SeqStore_t,
    rep: &mut RepCodes,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    ZSTD_compressBlock_opt_generic::<2>(ms, seqStore, rep, src, srcSize, DictMode::DictMatchState)
}

pub unsafe fn ZSTD_compressBlock_btultra_extDict(
    ms: &mut ZSTD_MatchState_t,
    seqStore: &mut SeqStore_t,
    rep: &mut RepCodes,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    ZSTD_compressBlock_opt_generic::<2>(ms, seqStore, rep, src, srcSize, DictMode::ExtDict)
}

// note: no btultra2 variant for extDict nor dictMatchState,
// because btultra2 is not meant to work with dictionaries
// and is only specific for the first block (no prefix)
