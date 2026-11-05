use crate::lib::polyfill::{branch_barrier, PointerExt};

use crate::lib::common::compiler::{prefetch_area, prefetch_read_data, Locality};
use crate::lib::common::mem::{MEM_read32, MEM_read64};
use crate::lib::common::zstd_internal::{RepCodes, ZSTD_REP_NUM};
use crate::lib::compress::zstd_compress::{SeqStore_t, ZSTD_MatchState_t};
use crate::lib::compress::zstd_compress_internal::{
    kSearchStrength, DictTableLoadMethod, TableFillPurpose, ZSTD_comparePackedTags, ZSTD_count,
    ZSTD_count_2segments, ZSTD_hashPtr, ZSTD_index_overlap_check, ZSTD_storeSeq,
    ZSTD_writeTaggedIndex, HASH_READ_SIZE, REPCODE1_TO_OFFBASE, ZSTD_SHORT_CACHE_TAG_BITS,
};
use crate::lib::zstd::ZSTD_compressionParameters;

unsafe fn ZSTD_fillDoubleHashTableForCDict(
    ms: &mut ZSTD_MatchState_t,
    end: *const core::ffi::c_void,
    dtlm: DictTableLoadMethod,
) {
    let cParams = &ms.cParams;
    let hashLarge = ms.hashTable;
    let hBitsL = (cParams.hashLog).wrapping_add(ZSTD_SHORT_CACHE_TAG_BITS as core::ffi::c_uint);
    let mls = cParams.minMatch;
    let hashSmall = ms.chainTable;
    let hBitsS = (cParams.chainLog).wrapping_add(ZSTD_SHORT_CACHE_TAG_BITS as core::ffi::c_uint);
    let base = ms.window.base;
    let mut ip = base.offset(ms.nextToUpdate as isize);
    let iend = (end as *const u8).sub(HASH_READ_SIZE as usize);
    let fastHashFillStep = 3;

    // Always insert every fastHashFillStep position into the hash tables.
    // Insert the other positions into the large hash table if their entry
    // is empty.
    while ip.offset(fastHashFillStep as isize).sub(1) <= iend {
        let curr = ip.offset_from(base) as core::ffi::c_long as u32;
        for i in 0..fastHashFillStep {
            let smHashAndTag = ZSTD_hashPtr(
                ip.offset(i as isize) as *const core::ffi::c_void,
                hBitsS,
                mls,
            );
            let lgHashAndTag =
                ZSTD_hashPtr(ip.offset(i as isize) as *const core::ffi::c_void, hBitsL, 8);
            if i == 0 {
                ZSTD_writeTaggedIndex(hashSmall, smHashAndTag, curr.wrapping_add(i));
            }
            if i == 0 || *hashLarge.add(lgHashAndTag >> ZSTD_SHORT_CACHE_TAG_BITS) == 0 {
                ZSTD_writeTaggedIndex(hashLarge, lgHashAndTag, curr.wrapping_add(i));
            }
            // Only load extra positions for DictTableLoadMethod::Full
            if dtlm == DictTableLoadMethod::Fast {
                break;
            }
        }
        ip = ip.offset(fastHashFillStep as isize);
    }
}

unsafe fn ZSTD_fillDoubleHashTableForCCtx(
    ms: &mut ZSTD_MatchState_t,
    end: *const core::ffi::c_void,
    dtlm: DictTableLoadMethod,
) {
    let cParams = &ms.cParams;
    let hashLarge = ms.hashTable;
    let hBitsL = cParams.hashLog;
    let mls = cParams.minMatch;
    let hashSmall = ms.chainTable;
    let hBitsS = cParams.chainLog;
    let base = ms.window.base;
    let mut ip = base.wrapping_offset(ms.nextToUpdate as isize);
    let iend = (end as *const u8).wrapping_sub(HASH_READ_SIZE as usize);
    let fastHashFillStep = 3;

    // Always insert every fastHashFillStep position into the hash tables.
    // Insert the other positions into the large hash table if their entry
    // is empty.
    while ip.offset(fastHashFillStep as isize).sub(1) <= iend {
        let curr = ip.wrapping_offset_from(base) as core::ffi::c_long as u32;
        for i in 0..fastHashFillStep {
            let smHash = ZSTD_hashPtr(
                ip.offset(i as isize) as *const core::ffi::c_void,
                hBitsS,
                mls,
            );
            let lgHash = ZSTD_hashPtr(ip.offset(i as isize) as *const core::ffi::c_void, hBitsL, 8);
            if i == 0 {
                *hashSmall.add(smHash) = curr.wrapping_add(i);
            }
            if i == 0 || *hashLarge.add(lgHash) == 0 {
                *hashLarge.add(lgHash) = curr.wrapping_add(i);
            }
            // Only load extra positions for DictTableLoadMethod::Full
            if dtlm == DictTableLoadMethod::Fast {
                break;
            }
        }
        ip = ip.offset(fastHashFillStep as isize);
    }
}

pub unsafe fn ZSTD_fillDoubleHashTable(
    ms: &mut ZSTD_MatchState_t,
    end: *const core::ffi::c_void,
    dtlm: DictTableLoadMethod,
    tfp: TableFillPurpose,
) {
    if tfp == TableFillPurpose::ForCDict {
        ZSTD_fillDoubleHashTableForCDict(ms, end, dtlm);
    } else {
        ZSTD_fillDoubleHashTableForCCtx(ms, end, dtlm);
    }
}

unsafe fn ZSTD_compressBlock_doubleFast_noDict_generic<const MLS: u32>(
    ms: &mut ZSTD_MatchState_t,
    seqStore: &mut SeqStore_t,
    rep: &mut RepCodes,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    let cParams = &ms.cParams;
    let hashLong = ms.hashTable;
    let hBitsL = cParams.hashLog;
    let hashSmall = ms.chainTable;
    let hBitsS = cParams.chainLog;
    let base = ms.window.base;
    let istart = src as *const u8;
    let mut anchor = istart;
    let endIndex = (istart.wrapping_offset_from(base) as usize).wrapping_add(srcSize) as u32;
    // presumes that, if there is a dictionary, it must be using Attach mode
    let prefixLowestIndex = ms.get_lowest_prefix_index(endIndex, cParams.windowLog);
    let prefixLowest = base.wrapping_offset(prefixLowestIndex as isize);
    let iend = istart.add(srcSize);
    let ilimit = iend.sub(HASH_READ_SIZE as usize);
    let mut offset_1 = rep[0];
    let mut offset_2 = rep[1];
    let mut offsetSaved1 = 0;
    let mut offsetSaved2 = 0;

    let mut mLength: usize = 0;
    let mut offset: u32 = 0;
    let mut curr: u32;

    // how many positions to search before increasing step size
    let kStepIncr = (1 << kSearchStrength) as usize;
    // the position at which to increment the step size if no match is found
    let mut nextStep;
    let mut step: usize; // the current step size

    let mut hl0: usize; // the long hash at ip
    let mut hl1: usize = 0; // the long hash at ip1

    let mut idxl0: u32; // the long match index for ip
    let mut idxl1: u32 = 0; // the long match index for ip1

    let mut matchl0; // the long match for ip
    let mut matchs0; // the short match for ip
    let mut matchl1 = core::ptr::null::<u8>(); // the long match for ip1
    let mut matchs0_safe; // matchs0 or safe address

    let mut ip = istart; // the current position
    let mut ip1; // the next position

    // Array of ~random data, should have low probability of matching data.
    // We load from here instead of from tables if matchl0/matchl1 are
    // invalid indices. Used to avoid unpredictable branches.
    let dummy: [u8; 10] = [0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0xe2, 0xb4];

    // init
    ip = ip.add(usize::from(ip == prefixLowest));
    let current = ip.wrapping_offset_from(base) as core::ffi::c_long as u32;
    let windowLow = ms.get_lowest_prefix_index(current, cParams.windowLog);
    let maxRep = current.wrapping_sub(windowLow);
    if offset_2 > maxRep {
        offsetSaved2 = offset_2;
        offset_2 = 0;
    }
    if offset_1 > maxRep {
        offsetSaved1 = offset_1;
        offset_1 = 0;
    }

    // Outer Loop: one iteration per match found and stored
    loop {
        's_428: {
            let mut current_block_83: u64;
            step = 1;
            nextStep = ip.wrapping_add(kStepIncr);
            ip1 = ip.wrapping_add(step);
            if ip1 <= ilimit {
                hl0 = ZSTD_hashPtr(ip as *const core::ffi::c_void, hBitsL, 8);
                idxl0 = *hashLong.add(hl0);
                matchl0 = base.wrapping_offset(idxl0 as isize);

                // Inner Loop: one iteration per search / position
                loop {
                    let hs0 = ZSTD_hashPtr(ip as *const core::ffi::c_void, hBitsS, MLS);
                    let idxs0 = *hashSmall.add(hs0);
                    curr = ip.wrapping_offset_from(base) as core::ffi::c_long as u32;
                    matchs0 = base.wrapping_offset(idxs0 as isize);

                    // update hash tables
                    let fresh2 = &mut (*hashSmall.add(hs0));
                    *fresh2 = curr;
                    *hashLong.add(hl0) = *fresh2;

                    // check noDict repcode
                    if (offset_1 > 0)
                        & (MEM_read32(ip.add(1).sub(offset_1 as usize) as *const core::ffi::c_void)
                            == MEM_read32(ip.add(1) as *const core::ffi::c_void))
                    {
                        mLength = (ZSTD_count(
                            ip.add(1).add(4),
                            ip.add(1).add(4).sub(offset_1 as usize),
                            iend,
                        ))
                        .wrapping_add(4);
                        ip = ip.add(1);
                        ZSTD_storeSeq(
                            seqStore,
                            ip.offset_from_unsigned(anchor),
                            anchor,
                            iend,
                            REPCODE1_TO_OFFBASE,
                            mLength,
                        );
                        current_block_83 = 18341544293284149774;
                        break;
                    } else {
                        hl1 = ZSTD_hashPtr(ip1 as *const core::ffi::c_void, hBitsL, 8);

                        // idxl0 > prefixLowestIndex is a (somewhat) unpredictable branch.
                        // However expression below compiles into conditional move. Since
                        // match is unlikely and we only *branch* on idxl0 > prefixLowestIndex
                        // if there is a match, all branches become predictable.
                        let matchl0_safe = {
                            core::hint::select_unpredictable(
                                idxl0 >= prefixLowestIndex,
                                matchl0,
                                dummy.as_ptr(),
                            )
                        };
                        // check prefix long match
                        if MEM_read64(matchl0_safe as *const core::ffi::c_void)
                            == MEM_read64(ip as *const core::ffi::c_void)
                            && {
                                branch_barrier!();
                                idxl0 >= prefixLowestIndex
                            }
                        {
                            mLength = (ZSTD_count(ip.add(8), matchl0.add(8), iend)).wrapping_add(8);
                            offset = ip.offset_from_unsigned(matchl0) as u32;
                            while (ip > anchor) & (matchl0 > prefixLowest)
                                && *ip.sub(1) as core::ffi::c_int
                                    == *matchl0.sub(1) as core::ffi::c_int
                            {
                                ip = ip.sub(1);
                                matchl0 = matchl0.sub(1);
                                mLength = mLength.wrapping_add(1);
                            }
                            current_block_83 = 14716613436827065636;
                            break;
                        } else {
                            idxl1 = *hashLong.add(hl1);
                            matchl1 = base.wrapping_offset(idxl1 as isize);

                            // Same optimization as matchl0 above
                            matchs0_safe = {
                                core::hint::select_unpredictable(
                                    idxs0 >= prefixLowestIndex,
                                    matchs0,
                                    dummy.as_ptr(),
                                )
                            };

                            // check prefix short match
                            if MEM_read32(matchs0_safe as *const core::ffi::c_void)
                                == MEM_read32(ip as *const core::ffi::c_void)
                                && {
                                    branch_barrier!();
                                    idxs0 >= prefixLowestIndex
                                }
                            {
                                current_block_83 = 6142208486753608565;
                                break;
                            }

                            if ip1 >= nextStep {
                                prefetch_read_data(ip1.wrapping_add(64), Locality::L1);
                                prefetch_read_data(ip1.wrapping_add(128), Locality::L1);
                                step = step.wrapping_add(1);
                                nextStep = nextStep.add(kStepIncr);
                            }
                            ip = ip1;
                            ip1 = ip1.add(step);

                            hl0 = hl1;
                            idxl0 = idxl1;
                            matchl0 = matchl1;

                            if ip1 > ilimit {
                                current_block_83 = 14575735148454673654;
                                break;
                            }
                        }
                    }
                }
                match current_block_83 {
                    14575735148454673654 => {}
                    _ => {
                        if current_block_83 == 6142208486753608565 {
                            // short match found: let's check for a longer one
                            mLength = (ZSTD_count(ip.add(4), matchs0.add(4), iend)).wrapping_add(4);
                            offset = ip.offset_from_unsigned(matchs0) as u32;

                            // check long match at +1 position
                            if idxl1 > prefixLowestIndex
                                && MEM_read64(matchl1 as *const core::ffi::c_void)
                                    == MEM_read64(ip1 as *const core::ffi::c_void)
                            {
                                let l1len =
                                    (ZSTD_count(ip1.add(8), matchl1.add(8), iend)).wrapping_add(8);
                                if l1len > mLength {
                                    // use the long match instead
                                    ip = ip1;
                                    mLength = l1len;
                                    offset = ip.offset_from_unsigned(matchl1) as u32;
                                    matchs0 = matchl1;
                                }
                            }

                            // complete backward
                            while (ip > anchor) & (matchs0 > prefixLowest)
                                && *ip.sub(1) as core::ffi::c_int
                                    == *matchs0.sub(1) as core::ffi::c_int
                            {
                                ip = ip.sub(1);
                                matchs0 = matchs0.sub(1);
                                mLength = mLength.wrapping_add(1);
                            }
                            current_block_83 = 14716613436827065636;
                        }
                        // _match_found: requires ip, offset, mLength
                        if current_block_83 == 14716613436827065636 {
                            offset_2 = offset_1;
                            offset_1 = offset;

                            if step < 4 {
                                // It is unsafe to write this value back to the hashtable when ip1 is
                                // greater than or equal to the new ip we will have after we're done
                                // processing this match. Rather than perform that test directly
                                // (ip1 >= ip + mLength), which costs speed in practice, we do a simpler
                                // more predictable test. The minmatch even if we take a short match is
                                // 4 bytes, so as long as step, the distance between ip and ip1
                                // (initially) is less than 4, we know ip1 < new ip.
                                *hashLong.add(hl1) =
                                    ip1.wrapping_offset_from(base) as core::ffi::c_long as u32;
                            }

                            ZSTD_storeSeq(
                                seqStore,
                                ip.offset_from_unsigned(anchor),
                                anchor,
                                iend,
                                offset.wrapping_add(ZSTD_REP_NUM),
                                mLength,
                            );
                        }

                        // match found
                        ip = ip.add(mLength);
                        anchor = ip;

                        if ip <= ilimit {
                            // Complementary insertion
                            // done after iLimit test, as candidates could be > iend-8
                            let indexToInsert = curr.wrapping_add(2);
                            *hashLong.add(ZSTD_hashPtr(
                                base.wrapping_offset(indexToInsert as isize)
                                    as *const core::ffi::c_void,
                                hBitsL,
                                8,
                            )) = indexToInsert;
                            *hashLong.add(ZSTD_hashPtr(
                                ip.sub(2) as *const core::ffi::c_void,
                                hBitsL,
                                8,
                            )) = ip.sub(2).wrapping_offset_from(base) as core::ffi::c_long as u32;
                            *hashSmall.add(ZSTD_hashPtr(
                                base.wrapping_offset(indexToInsert as isize)
                                    as *const core::ffi::c_void,
                                hBitsS,
                                MLS,
                            )) = indexToInsert;
                            *hashSmall.add(ZSTD_hashPtr(
                                ip.sub(1) as *const core::ffi::c_void,
                                hBitsS,
                                MLS,
                            )) = ip.sub(1).wrapping_offset_from(base) as core::ffi::c_long as u32;

                            // check immediate repcode
                            while ip <= ilimit
                                && (offset_2 > 0)
                                    & (MEM_read32(ip as *const core::ffi::c_void)
                                        == MEM_read32(
                                            ip.sub(offset_2 as usize) as *const core::ffi::c_void
                                        ))
                            {
                                // store sequence
                                let rLength =
                                    (ZSTD_count(ip.add(4), ip.add(4).sub(offset_2 as usize), iend))
                                        .wrapping_add(4);
                                core::mem::swap(&mut offset_2, &mut offset_1);
                                *hashSmall.add(ZSTD_hashPtr(
                                    ip as *const core::ffi::c_void,
                                    hBitsS,
                                    MLS,
                                )) = ip.wrapping_offset_from(base) as core::ffi::c_long as u32;
                                *hashLong.add(ZSTD_hashPtr(
                                    ip as *const core::ffi::c_void,
                                    hBitsL,
                                    8,
                                )) = ip.wrapping_offset_from(base) as core::ffi::c_long as u32;
                                ZSTD_storeSeq(
                                    seqStore,
                                    0,
                                    anchor,
                                    iend,
                                    REPCODE1_TO_OFFBASE,
                                    rLength,
                                );
                                ip = ip.add(rLength);
                                anchor = ip;
                            }
                        }
                        break 's_428;
                    }
                }
            }

            // If offset_1 started invalid (offsetSaved1 != 0) and became valid (offset_1 != 0),
            // rotate saved offsets. See comment in ZSTD_compressBlock_fast_noDict for more context.
            offsetSaved2 = if offsetSaved1 != 0 && offset_1 != 0 {
                offsetSaved1
            } else {
                offsetSaved2
            };

            // save reps for next block
            rep[0] = match offset_1 {
                0 => offsetSaved1,
                _ => offset_1,
            };
            rep[1] = match offset_2 {
                0 => offsetSaved2,
                _ => offset_2,
            };

            // Return the last literals size
            return iend.offset_from_unsigned(anchor);
        }
    }
}

unsafe fn ZSTD_compressBlock_doubleFast_dictMatchState_generic<const MLS: u32>(
    ms: &mut ZSTD_MatchState_t,
    seqStore: &mut SeqStore_t,
    rep: &mut RepCodes,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    let mut current_block: u64;
    let cParams = &ms.cParams;
    let hashLong = ms.hashTable;
    let hBitsL = cParams.hashLog;
    let hashSmall = ms.chainTable;
    let hBitsS = cParams.chainLog;
    let base = ms.window.base;
    let istart = src as *const u8;
    let mut ip = istart;
    let mut anchor = istart;
    let endIndex = (istart.wrapping_offset_from(base) as usize).wrapping_add(srcSize) as u32;
    // presumes that, if there is a dictionary, it must be using Attach mode
    let prefixLowestIndex = ms.get_lowest_prefix_index(endIndex, cParams.windowLog);
    let prefixLowest = base.wrapping_offset(prefixLowestIndex as isize);
    let iend = istart.add(srcSize);
    let ilimit = iend.sub(HASH_READ_SIZE as usize);
    let mut offset_1 = rep[0];
    let mut offset_2 = rep[1];

    let dms = ms.dictMatchState;
    let dictCParams: *const ZSTD_compressionParameters = &(*dms).cParams;
    let dictHashLong: *const u32 = (*dms).hashTable;
    let dictHashSmall: *const u32 = (*dms).chainTable;
    let dictStartIndex = (*dms).window.dictLimit;
    let dictBase = (*dms).window.base;
    let dictStart = dictBase.offset(dictStartIndex as isize);
    let dictEnd = (*dms).window.nextSrc;
    let dictIndexDelta =
        prefixLowestIndex.wrapping_sub(dictEnd.offset_from(dictBase) as core::ffi::c_long as u32);
    let dictHBitsL =
        ((*dictCParams).hashLog).wrapping_add(ZSTD_SHORT_CACHE_TAG_BITS as core::ffi::c_uint);
    let dictHBitsS =
        ((*dictCParams).chainLog).wrapping_add(ZSTD_SHORT_CACHE_TAG_BITS as core::ffi::c_uint);
    let dictAndPrefixLength =
        ip.offset_from_unsigned(prefixLowest) + dictEnd.offset_from_unsigned(dictStart);

    if ms.prefetchCDictTables != 0 {
        let hashTableBytes = (1usize << (*dictCParams).hashLog).wrapping_mul(size_of::<u32>());
        let chainTableBytes = (1usize << (*dictCParams).chainLog).wrapping_mul(size_of::<u32>());
        prefetch_area(dictHashLong as *const core::ffi::c_char, hashTableBytes);
        prefetch_area(dictHashSmall as *const core::ffi::c_char, chainTableBytes);
    }

    // init
    ip = ip.add(usize::from(dictAndPrefixLength == 0));

    // Main Search Loop (< instead of <=, because repcode check at ip+1)
    while ip < ilimit {
        let mut mLength: usize = 0;
        let mut offset: u32 = 0;
        let h2 = ZSTD_hashPtr(ip as *const core::ffi::c_void, hBitsL, 8);
        let h = ZSTD_hashPtr(ip as *const core::ffi::c_void, hBitsS, MLS);
        let dictHashAndTagL = ZSTD_hashPtr(ip as *const core::ffi::c_void, dictHBitsL, 8);
        let dictHashAndTagS = ZSTD_hashPtr(ip as *const core::ffi::c_void, dictHBitsS, MLS);
        let dictMatchIndexAndTagL = *dictHashLong.add(dictHashAndTagL >> ZSTD_SHORT_CACHE_TAG_BITS);
        let dictMatchIndexAndTagS =
            *dictHashSmall.add(dictHashAndTagS >> ZSTD_SHORT_CACHE_TAG_BITS);
        let dictTagsMatchL =
            ZSTD_comparePackedTags(dictMatchIndexAndTagL as usize, dictHashAndTagL);
        let dictTagsMatchS =
            ZSTD_comparePackedTags(dictMatchIndexAndTagS as usize, dictHashAndTagS);
        let curr = ip.wrapping_offset_from(base) as core::ffi::c_long as u32;
        let matchIndexL = *hashLong.add(h2);
        let mut matchIndexS = *hashSmall.add(h);
        let mut matchLong = base.wrapping_offset(matchIndexL as isize);
        let mut match_0 = base.wrapping_offset(matchIndexS as isize);
        let repIndex = curr.wrapping_add(1).wrapping_sub(offset_1);
        let repMatch = if repIndex < prefixLowestIndex {
            dictBase.offset(repIndex.wrapping_sub(dictIndexDelta) as isize)
        } else {
            base.wrapping_offset(repIndex as isize)
        };

        // update hash tables
        let fresh3 = &mut (*hashSmall.add(h));
        *fresh3 = curr;
        *hashLong.add(h2) = *fresh3;

        // check repcode
        if ZSTD_index_overlap_check(prefixLowestIndex, repIndex)
            && MEM_read32(repMatch as *const core::ffi::c_void)
                == MEM_read32(ip.add(1) as *const core::ffi::c_void)
        {
            let repMatchEnd = if repIndex < prefixLowestIndex {
                dictEnd
            } else {
                iend
            };
            mLength = (ZSTD_count_2segments(
                ip.add(1).add(4),
                repMatch.add(4),
                iend,
                repMatchEnd,
                prefixLowest,
            ))
            .wrapping_add(4);
            ip = ip.add(1);
            ZSTD_storeSeq(
                seqStore,
                ip.offset_from_unsigned(anchor),
                anchor,
                iend,
                REPCODE1_TO_OFFBASE,
                mLength,
            );
        } else {
            // check prefix long match
            if matchIndexL >= prefixLowestIndex
                && MEM_read64(matchLong as *const core::ffi::c_void)
                    == MEM_read64(ip as *const core::ffi::c_void)
            {
                mLength = (ZSTD_count(ip.add(8), matchLong.add(8), iend)).wrapping_add(8);
                offset = ip.offset_from_unsigned(matchLong) as u32;
                while (ip > anchor) & (matchLong > prefixLowest)
                    && *ip.sub(1) as core::ffi::c_int == *matchLong.sub(1) as core::ffi::c_int
                {
                    ip = ip.sub(1);
                    matchLong = matchLong.sub(1);
                    mLength = mLength.wrapping_add(1);
                }
            } else {
                // check dictMatchState long match
                if dictTagsMatchL {
                    let dictMatchIndexL = dictMatchIndexAndTagL >> ZSTD_SHORT_CACHE_TAG_BITS;
                    let mut dictMatchL = dictBase.offset(dictMatchIndexL as isize);

                    if dictMatchL > dictStart
                        && MEM_read64(dictMatchL as *const core::ffi::c_void)
                            == MEM_read64(ip as *const core::ffi::c_void)
                    {
                        mLength = (ZSTD_count_2segments(
                            ip.add(8),
                            dictMatchL.add(8),
                            iend,
                            dictEnd,
                            prefixLowest,
                        ))
                        .wrapping_add(8);
                        offset = curr
                            .wrapping_sub(dictMatchIndexL)
                            .wrapping_sub(dictIndexDelta);
                        while (ip > anchor) & (dictMatchL > dictStart)
                            && *ip.sub(1) as core::ffi::c_int
                                == *dictMatchL.sub(1) as core::ffi::c_int
                        {
                            ip = ip.sub(1);
                            dictMatchL = dictMatchL.sub(1);
                            mLength = mLength.wrapping_add(1);
                        }
                        current_block = 17830677668754335218;
                    } else {
                        current_block = 6721012065216013753;
                    }
                } else {
                    current_block = 6721012065216013753;
                }

                match current_block {
                    17830677668754335218 => {}
                    _ => {
                        if matchIndexS > prefixLowestIndex {
                            // short match candidate
                            if MEM_read32(match_0 as *const core::ffi::c_void)
                                == MEM_read32(ip as *const core::ffi::c_void)
                            {
                                current_block = 2631791190359682872;
                            } else {
                                current_block = 5372832139739605200;
                            }
                        } else if dictTagsMatchS {
                            // check dictMatchState short match
                            let dictMatchIndexS =
                                dictMatchIndexAndTagS >> ZSTD_SHORT_CACHE_TAG_BITS;
                            match_0 = dictBase.offset(dictMatchIndexS as isize);
                            matchIndexS = dictMatchIndexS.wrapping_add(dictIndexDelta);

                            if match_0 > dictStart
                                && MEM_read32(match_0 as *const core::ffi::c_void)
                                    == MEM_read32(ip as *const core::ffi::c_void)
                            {
                                current_block = 2631791190359682872;
                            } else {
                                current_block = 5372832139739605200;
                            }
                        } else {
                            current_block = 5372832139739605200;
                        }

                        match current_block {
                            5372832139739605200 => {
                                ip = ip
                                    .add((ip.offset_from_unsigned(anchor) >> kSearchStrength) + 1);
                                continue;
                            }
                            _ => {
                                let hl3 =
                                    ZSTD_hashPtr(ip.add(1) as *const core::ffi::c_void, hBitsL, 8);
                                let dictHashAndTagL3 = ZSTD_hashPtr(
                                    ip.add(1) as *const core::ffi::c_void,
                                    dictHBitsL,
                                    8,
                                );
                                let matchIndexL3 = *hashLong.add(hl3);
                                let dictMatchIndexAndTagL3 = *dictHashLong
                                    .add(dictHashAndTagL3 >> ZSTD_SHORT_CACHE_TAG_BITS);
                                let dictTagsMatchL3 = ZSTD_comparePackedTags(
                                    dictMatchIndexAndTagL3 as usize,
                                    dictHashAndTagL3,
                                );
                                let mut matchL3 = base.wrapping_offset(matchIndexL3 as isize);
                                *hashLong.add(hl3) = curr.wrapping_add(1);

                                // check prefix long +1 match
                                if matchIndexL3 >= prefixLowestIndex
                                    && MEM_read64(matchL3 as *const core::ffi::c_void)
                                        == MEM_read64(ip.add(1) as *const core::ffi::c_void)
                                {
                                    mLength = (ZSTD_count(ip.add(9), matchL3.add(8), iend))
                                        .wrapping_add(8);
                                    ip = ip.add(1);
                                    offset = ip.offset_from_unsigned(matchL3) as u32;
                                    while (ip > anchor) & (matchL3 > prefixLowest)
                                        && *ip.sub(1) as core::ffi::c_int
                                            == *matchL3.sub(1) as core::ffi::c_int
                                    {
                                        ip = ip.sub(1);
                                        matchL3 = matchL3.sub(1);
                                        mLength = mLength.wrapping_add(1);
                                    }
                                } else {
                                    // check dict long +1 match
                                    if dictTagsMatchL3 {
                                        let dictMatchIndexL3 =
                                            dictMatchIndexAndTagL3 >> ZSTD_SHORT_CACHE_TAG_BITS;
                                        let mut dictMatchL3 =
                                            dictBase.offset(dictMatchIndexL3 as isize);
                                        if dictMatchL3 > dictStart
                                            && MEM_read64(dictMatchL3 as *const core::ffi::c_void)
                                                == MEM_read64(ip.add(1) as *const core::ffi::c_void)
                                        {
                                            mLength = (ZSTD_count_2segments(
                                                ip.add(1).add(8),
                                                dictMatchL3.add(8),
                                                iend,
                                                dictEnd,
                                                prefixLowest,
                                            ))
                                            .wrapping_add(8);
                                            ip = ip.add(1);
                                            offset = curr
                                                .wrapping_add(1)
                                                .wrapping_sub(dictMatchIndexL3)
                                                .wrapping_sub(dictIndexDelta);
                                            while (ip > anchor) & (dictMatchL3 > dictStart)
                                                && *ip.sub(1) as core::ffi::c_int
                                                    == *dictMatchL3.sub(1) as core::ffi::c_int
                                            {
                                                ip = ip.sub(1);
                                                dictMatchL3 = dictMatchL3.sub(1);
                                                mLength = mLength.wrapping_add(1);
                                            }
                                            current_block = 17830677668754335218;
                                        } else {
                                            current_block = 1209030638129645089;
                                        }
                                    } else {
                                        current_block = 1209030638129645089;
                                    }

                                    // if no long +1 match, explore the short match we found
                                    match current_block {
                                        17830677668754335218 => {}
                                        _ => {
                                            if matchIndexS < prefixLowestIndex {
                                                mLength = (ZSTD_count_2segments(
                                                    ip.add(4),
                                                    match_0.add(4),
                                                    iend,
                                                    dictEnd,
                                                    prefixLowest,
                                                ))
                                                .wrapping_add(4);
                                                offset = curr.wrapping_sub(matchIndexS);
                                                while (ip > anchor) & (match_0 > dictStart)
                                                    && *ip.sub(1) as core::ffi::c_int
                                                        == *match_0.sub(1) as core::ffi::c_int
                                                {
                                                    ip = ip.sub(1);
                                                    match_0 = match_0.sub(1);
                                                    mLength = mLength.wrapping_add(1);
                                                }
                                            } else {
                                                mLength =
                                                    (ZSTD_count(ip.add(4), match_0.add(4), iend))
                                                        .wrapping_add(4);
                                                offset = ip.offset_from_unsigned(match_0) as u32;
                                                while (ip > anchor) & (match_0 > prefixLowest)
                                                    && *ip.sub(1) as core::ffi::c_int
                                                        == *match_0.sub(1) as core::ffi::c_int
                                                {
                                                    ip = ip.sub(1);
                                                    match_0 = match_0.sub(1);
                                                    mLength = mLength.wrapping_add(1);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            offset_2 = offset_1;
            offset_1 = offset;

            ZSTD_storeSeq(
                seqStore,
                ip.offset_from_unsigned(anchor),
                anchor,
                iend,
                offset.wrapping_add(ZSTD_REP_NUM),
                mLength,
            );
        }

        // match found
        ip = ip.add(mLength);
        anchor = ip;

        if ip <= ilimit {
            // Complementary insertion
            // done after iLimit test, as candidates could be > iend-8
            let indexToInsert = curr.wrapping_add(2);
            *hashLong.add(ZSTD_hashPtr(
                base.wrapping_add(indexToInsert as usize) as *const core::ffi::c_void,
                hBitsL,
                8,
            )) = indexToInsert;
            *hashLong.add(ZSTD_hashPtr(
                ip.sub(2) as *const core::ffi::c_void,
                hBitsL,
                8,
            )) = ip.sub(2).wrapping_offset_from(base) as core::ffi::c_long as u32;
            *hashSmall.add(ZSTD_hashPtr(
                base.wrapping_offset(indexToInsert as isize) as *const core::ffi::c_void,
                hBitsS,
                MLS,
            )) = indexToInsert;
            *hashSmall.add(ZSTD_hashPtr(
                ip.sub(1) as *const core::ffi::c_void,
                hBitsS,
                MLS,
            )) = ip.sub(1).wrapping_offset_from(base) as core::ffi::c_long as u32;

            // check immediate repcode
            while ip <= ilimit {
                let current2 = ip.wrapping_offset_from(base) as core::ffi::c_long as u32;
                let repIndex2 = current2.wrapping_sub(offset_2);
                let repMatch2 = if repIndex2 < prefixLowestIndex {
                    dictBase
                        .offset(repIndex2 as isize)
                        .sub(dictIndexDelta as usize)
                } else {
                    base.wrapping_offset(repIndex2 as isize)
                };
                if !(ZSTD_index_overlap_check(prefixLowestIndex, repIndex2)
                    && MEM_read32(repMatch2 as *const core::ffi::c_void)
                        == MEM_read32(ip as *const core::ffi::c_void))
                {
                    break;
                }
                let repEnd2 = if repIndex2 < prefixLowestIndex {
                    dictEnd
                } else {
                    iend
                };
                let repLength2 = (ZSTD_count_2segments(
                    ip.add(4),
                    repMatch2.add(4),
                    iend,
                    repEnd2,
                    prefixLowest,
                ))
                .wrapping_add(4);
                core::mem::swap(&mut offset_2, &mut offset_1);
                ZSTD_storeSeq(seqStore, 0, anchor, iend, REPCODE1_TO_OFFBASE, repLength2);
                *hashSmall.add(ZSTD_hashPtr(ip as *const core::ffi::c_void, hBitsS, MLS)) =
                    current2;
                *hashLong.add(ZSTD_hashPtr(ip as *const core::ffi::c_void, hBitsL, 8)) = current2;
                ip = ip.add(repLength2);
                anchor = ip;
            }
        }
    }

    // save reps for next block
    rep[0] = offset_1;
    rep[1] = offset_2;

    // Return the last literals size
    iend.offset_from_unsigned(anchor)
}

pub unsafe fn ZSTD_compressBlock_doubleFast(
    ms: &mut ZSTD_MatchState_t,
    seqStore: &mut SeqStore_t,
    rep: &mut RepCodes,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    let mls = ms.cParams.minMatch;
    match mls {
        5 => ZSTD_compressBlock_doubleFast_noDict_generic::<5>(ms, seqStore, rep, src, srcSize),
        6 => ZSTD_compressBlock_doubleFast_noDict_generic::<6>(ms, seqStore, rep, src, srcSize),
        7 => ZSTD_compressBlock_doubleFast_noDict_generic::<7>(ms, seqStore, rep, src, srcSize),
        _ => ZSTD_compressBlock_doubleFast_noDict_generic::<4>(ms, seqStore, rep, src, srcSize),
    }
}

pub unsafe fn ZSTD_compressBlock_doubleFast_dictMatchState(
    ms: &mut ZSTD_MatchState_t,
    seqStore: &mut SeqStore_t,
    rep: &mut RepCodes,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    let mls = ms.cParams.minMatch;
    match mls {
        5 => ZSTD_compressBlock_doubleFast_dictMatchState_generic::<5>(
            ms, seqStore, rep, src, srcSize,
        ),
        6 => ZSTD_compressBlock_doubleFast_dictMatchState_generic::<6>(
            ms, seqStore, rep, src, srcSize,
        ),
        7 => ZSTD_compressBlock_doubleFast_dictMatchState_generic::<7>(
            ms, seqStore, rep, src, srcSize,
        ),
        _ => ZSTD_compressBlock_doubleFast_dictMatchState_generic::<4>(
            ms, seqStore, rep, src, srcSize,
        ),
    }
}

unsafe fn ZSTD_compressBlock_doubleFast_extDict_generic<const MLS: u32>(
    ms: &mut ZSTD_MatchState_t,
    seqStore: &mut SeqStore_t,
    rep: &mut RepCodes,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    let cParams = &ms.cParams;
    let hashLong = ms.hashTable;
    let hBitsL = cParams.hashLog;
    let hashSmall = ms.chainTable;
    let hBitsS = cParams.chainLog;
    let istart = src as *const u8;
    let mut ip = istart;
    let mut anchor = istart;
    let iend = istart.add(srcSize);
    let ilimit = iend.sub(8);
    let base = ms.window.base;
    let endIndex = (istart.wrapping_offset_from(base) as usize).wrapping_add(srcSize) as u32;
    let lowLimit = ms.get_lowest_match_index(endIndex, cParams.windowLog);
    let dictStartIndex = lowLimit;
    let dictLimit = ms.window.dictLimit;
    let prefixStartIndex = dictLimit.max(lowLimit);
    let prefixStart = base.wrapping_offset(prefixStartIndex as isize);
    let dictBase = ms.window.dictBase;
    let dictStart = dictBase.wrapping_offset(dictStartIndex as isize);
    let dictEnd = dictBase.wrapping_offset(prefixStartIndex as isize);
    let mut offset_1 = rep[0];
    let mut offset_2 = rep[1];

    // if extDict is invalidated due to maxDistance, switch to "regular" variant
    if prefixStartIndex == dictStartIndex {
        return ZSTD_compressBlock_doubleFast(ms, seqStore, rep, src, srcSize);
    }

    // Search Loop (< instead of <=, because ip+1)
    while ip < ilimit {
        let hSmall = ZSTD_hashPtr(ip as *const core::ffi::c_void, hBitsS, MLS);
        let matchIndex = *hashSmall.add(hSmall);
        let matchBase = if matchIndex < prefixStartIndex {
            dictBase
        } else {
            base
        };
        let mut match_0 = matchBase.wrapping_offset(matchIndex as isize);

        let hLong = ZSTD_hashPtr(ip as *const core::ffi::c_void, hBitsL, 8);
        let matchLongIndex = *hashLong.add(hLong);
        let matchLongBase = if matchLongIndex < prefixStartIndex {
            dictBase
        } else {
            base
        };
        let mut matchLong = matchLongBase.wrapping_offset(matchLongIndex as isize);

        let curr = ip.wrapping_offset_from(base) as core::ffi::c_long as u32;
        let repIndex = curr.wrapping_add(1).wrapping_sub(offset_1); // offset_1 expected <= curr +1
        let repBase = if repIndex < prefixStartIndex {
            dictBase
        } else {
            base
        };
        let repMatch = repBase.wrapping_offset(repIndex as isize);
        let mut mLength: usize;
        // update hash table
        let fresh4 = &mut (*hashLong.add(hLong));
        *fresh4 = curr;
        *hashSmall.add(hSmall) = *fresh4;

        // note: we are searching at curr+1
        if ZSTD_index_overlap_check(prefixStartIndex, repIndex)
            & (offset_1 <= curr.wrapping_add(1).wrapping_sub(dictStartIndex))
            && MEM_read32(repMatch as *const core::ffi::c_void)
                == MEM_read32(ip.add(1) as *const core::ffi::c_void)
        {
            let repMatchEnd = if repIndex < prefixStartIndex {
                dictEnd
            } else {
                iend
            };
            mLength = (ZSTD_count_2segments(
                ip.add(1).add(4),
                repMatch.add(4),
                iend,
                repMatchEnd,
                prefixStart,
            ))
            .wrapping_add(4);
            ip = ip.add(1);
            ZSTD_storeSeq(
                seqStore,
                ip.offset_from_unsigned(anchor),
                anchor,
                iend,
                REPCODE1_TO_OFFBASE,
                mLength,
            );
        } else if matchLongIndex > dictStartIndex
            && MEM_read64(matchLong as *const core::ffi::c_void)
                == MEM_read64(ip as *const core::ffi::c_void)
        {
            let matchEnd = if matchLongIndex < prefixStartIndex {
                dictEnd
            } else {
                iend
            };
            let lowMatchPtr = if matchLongIndex < prefixStartIndex {
                dictStart
            } else {
                prefixStart
            };
            mLength =
                (ZSTD_count_2segments(ip.add(8), matchLong.add(8), iend, matchEnd, prefixStart))
                    .wrapping_add(8);
            let offset = curr.wrapping_sub(matchLongIndex);
            while (ip > anchor) & (matchLong > lowMatchPtr)
                && *ip.sub(1) as core::ffi::c_int == *matchLong.sub(1) as core::ffi::c_int
            {
                ip = ip.sub(1);
                matchLong = matchLong.sub(1);
                mLength = mLength.wrapping_add(1);
            }
            offset_2 = offset_1;
            offset_1 = offset;
            ZSTD_storeSeq(
                seqStore,
                ip.offset_from_unsigned(anchor),
                anchor,
                iend,
                offset.wrapping_add(ZSTD_REP_NUM),
                mLength,
            );
        } else if matchIndex > dictStartIndex
            && MEM_read32(match_0 as *const core::ffi::c_void)
                == MEM_read32(ip as *const core::ffi::c_void)
        {
            let h3 = ZSTD_hashPtr(ip.add(1) as *const core::ffi::c_void, hBitsL, 8);
            let matchIndex3 = *hashLong.add(h3);
            let match3Base = if matchIndex3 < prefixStartIndex {
                dictBase
            } else {
                base
            };
            let mut match3 = match3Base.wrapping_offset(matchIndex3 as isize);
            *hashLong.add(h3) = curr.wrapping_add(1);

            let offset_0: u32;
            if matchIndex3 > dictStartIndex
                && MEM_read64(match3 as *const core::ffi::c_void)
                    == MEM_read64(ip.add(1) as *const core::ffi::c_void)
            {
                let matchEnd_0 = if matchIndex3 < prefixStartIndex {
                    dictEnd
                } else {
                    iend
                };
                let lowMatchPtr_0 = if matchIndex3 < prefixStartIndex {
                    dictStart
                } else {
                    prefixStart
                };
                mLength =
                    (ZSTD_count_2segments(ip.add(9), match3.add(8), iend, matchEnd_0, prefixStart))
                        .wrapping_add(8);
                ip = ip.add(1);
                offset_0 = curr.wrapping_add(1).wrapping_sub(matchIndex3);
                while (ip > anchor) & (match3 > lowMatchPtr_0)
                    && *ip.sub(1) as core::ffi::c_int == *match3.sub(1) as core::ffi::c_int
                {
                    ip = ip.sub(1);
                    match3 = match3.sub(1);
                    mLength = mLength.wrapping_add(1);
                }
            } else {
                let matchEnd_1 = if matchIndex < prefixStartIndex {
                    dictEnd
                } else {
                    iend
                };
                let lowMatchPtr_1 = if matchIndex < prefixStartIndex {
                    dictStart
                } else {
                    prefixStart
                };
                mLength = (ZSTD_count_2segments(
                    ip.add(4),
                    match_0.add(4),
                    iend,
                    matchEnd_1,
                    prefixStart,
                ))
                .wrapping_add(4);
                offset_0 = curr.wrapping_sub(matchIndex);
                while (ip > anchor) & (match_0 > lowMatchPtr_1)
                    && *ip.sub(1) as core::ffi::c_int == *match_0.sub(1) as core::ffi::c_int
                {
                    ip = ip.sub(1);
                    match_0 = match_0.sub(1);
                    mLength = mLength.wrapping_add(1);
                }
            }
            offset_2 = offset_1;
            offset_1 = offset_0;
            ZSTD_storeSeq(
                seqStore,
                ip.offset_from_unsigned(anchor),
                anchor,
                iend,
                offset_0.wrapping_add(ZSTD_REP_NUM),
                mLength,
            );
        } else {
            ip = ip.add((ip.offset_from_unsigned(anchor) >> kSearchStrength) + 1);
            continue;
        }

        // move to next sequence start
        ip = ip.add(mLength);
        anchor = ip;

        if ip <= ilimit {
            // Complementary insertion
            // done after iLimit test, as candidates could be > iend-8
            let indexToInsert = curr.wrapping_add(2);
            *hashLong.add(ZSTD_hashPtr(
                base.wrapping_offset(indexToInsert as isize) as *const core::ffi::c_void,
                hBitsL,
                8,
            )) = indexToInsert;
            *hashLong.add(ZSTD_hashPtr(
                ip.sub(2) as *const core::ffi::c_void,
                hBitsL,
                8,
            )) = ip.sub(2).wrapping_offset_from(base) as core::ffi::c_long as u32;
            *hashSmall.add(ZSTD_hashPtr(
                base.wrapping_offset(indexToInsert as isize) as *const core::ffi::c_void,
                hBitsS,
                MLS,
            )) = indexToInsert;
            *hashSmall.add(ZSTD_hashPtr(
                ip.sub(1) as *const core::ffi::c_void,
                hBitsS,
                MLS,
            )) = ip.sub(1).wrapping_offset_from(base) as core::ffi::c_long as u32;

            // check immediate repcode
            while ip <= ilimit {
                let current2 = ip.wrapping_offset_from(base) as core::ffi::c_long as u32;
                let repIndex2 = current2.wrapping_sub(offset_2);
                let repMatch2 = if repIndex2 < prefixStartIndex {
                    dictBase.offset(repIndex2 as isize)
                } else {
                    base.wrapping_offset(repIndex2 as isize)
                };
                if !(ZSTD_index_overlap_check(prefixStartIndex, repIndex2)
                    & (offset_2 <= current2.wrapping_sub(dictStartIndex))
                    && MEM_read32(repMatch2 as *const core::ffi::c_void)
                        == MEM_read32(ip as *const core::ffi::c_void))
                {
                    break;
                }
                let repEnd2 = if repIndex2 < prefixStartIndex {
                    dictEnd
                } else {
                    iend
                };
                let repLength2 =
                    (ZSTD_count_2segments(ip.add(4), repMatch2.add(4), iend, repEnd2, prefixStart))
                        .wrapping_add(4);
                core::mem::swap(&mut offset_2, &mut offset_1);
                ZSTD_storeSeq(seqStore, 0, anchor, iend, REPCODE1_TO_OFFBASE, repLength2);
                *hashSmall.add(ZSTD_hashPtr(ip as *const core::ffi::c_void, hBitsS, MLS)) =
                    current2;
                *hashLong.add(ZSTD_hashPtr(ip as *const core::ffi::c_void, hBitsL, 8)) = current2;
                ip = ip.add(repLength2);
                anchor = ip;
            }
        }
    }

    // save reps for next block
    rep[0] = offset_1;
    rep[1] = offset_2;

    // Return the last literals size
    iend.offset_from_unsigned(anchor)
}

pub unsafe fn ZSTD_compressBlock_doubleFast_extDict(
    ms: &mut ZSTD_MatchState_t,
    seqStore: &mut SeqStore_t,
    rep: &mut RepCodes,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    let mls = ms.cParams.minMatch;
    match mls {
        5 => ZSTD_compressBlock_doubleFast_extDict_generic::<5>(ms, seqStore, rep, src, srcSize),
        6 => ZSTD_compressBlock_doubleFast_extDict_generic::<6>(ms, seqStore, rep, src, srcSize),
        7 => ZSTD_compressBlock_doubleFast_extDict_generic::<7>(ms, seqStore, rep, src, srcSize),
        _ => ZSTD_compressBlock_doubleFast_extDict_generic::<4>(ms, seqStore, rep, src, srcSize),
    }
}
