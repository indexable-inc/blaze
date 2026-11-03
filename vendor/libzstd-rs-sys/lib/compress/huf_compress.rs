use core::ffi::{c_int, c_uint, c_void};

use crate::lib::common::bits::ZSTD_highbit32;
use crate::lib::common::entropy_common::HUF_readStats;
use crate::lib::common::error_private::Error;
use crate::lib::common::fse::{
    FSE_CTable, FSE_BUILD_CTABLE_WORKSPACE_SIZE_U32, FSE_CTABLE_SIZE_U32,
};
use crate::lib::common::huf::{
    CTable, HUF_CElt, HUF_CTableHeader, HUF_flags_bmi2, HUF_flags_optimalDepth,
    HUF_flags_preferRepeat, HUF_flags_suspectUncompressible, HUF_repeat, SymbolTable,
    HUF_BLOCKSIZE_MAX, HUF_CTABLEBOUND, HUF_CTABLE_WORKSPACE_SIZE, HUF_SYMBOLVALUE_MAX,
    HUF_SYMBOLVALUE_MAX_U8, HUF_TABLELOG_ABSOLUTEMAX, HUF_TABLELOG_DEFAULT, HUF_TABLELOG_MAX,
    HUF_WORKSPACE_SIZE,
};
use crate::lib::common::mem::{MEM_32bits, MEM_writeLE16, MEM_writeLEST};
use crate::lib::compress::fse_compress::{
    FSE_buildCTable_wksp, FSE_compress_usingCTable, FSE_normalizeCount, FSE_optimalTableLog,
    FSE_optimalTableLog_internal, FSE_writeNCount,
};
use crate::lib::compress::hist::{HIST_count_simple, HIST_count_wksp_array, HIST_WKSP_SIZE_U32};

#[cfg(doc)]
use crate::lib::common::bitstream::BIT_CStream_t;

pub type nodeElt = nodeElt_s;

#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct nodeElt_s {
    pub count: u32,
    pub parent: u16,
    pub byte: u8,
    pub nbBits: u8,
}

pub const HUF_WORKSPACE_MAX_ALIGNMENT: usize = 8;

fn HUF_alignUpWorkspace(
    workspace: *mut c_void,
    workspaceSizePtr: &mut usize,
    align: usize,
) -> *mut c_void {
    let mask = align - 1;
    let rem = workspace as usize & mask;
    let add = (align - rem) & mask;
    let aligned = workspace.wrapping_byte_add(add);

    debug_assert_eq!(align & (align - 1), 0); /* pow 2 */
    debug_assert!(align <= HUF_WORKSPACE_MAX_ALIGNMENT);

    if *workspaceSizePtr >= add {
        debug_assert!(add < align);
        debug_assert_eq!((aligned as usize) & mask, 0);
        *workspaceSizePtr -= add;
        aligned
    } else {
        *workspaceSizePtr = 0;
        core::ptr::null_mut()
    }
}

pub const MAX_FSE_TABLELOG_FOR_HUFF_HEADER: usize = 6;

#[derive(Copy, Clone)]
#[repr(C)]
pub struct HUF_CompressWeightsWksp {
    pub CTable:
        [FSE_CTable; FSE_CTABLE_SIZE_U32(MAX_FSE_TABLELOG_FOR_HUFF_HEADER, HUF_TABLELOG_MAX)],
    pub scratchBuffer: [u32; FSE_BUILD_CTABLE_WORKSPACE_SIZE_U32(
        HUF_TABLELOG_MAX,
        MAX_FSE_TABLELOG_FOR_HUFF_HEADER,
    )],
    pub count: [c_uint; HUF_TABLELOG_MAX + 1],
    pub norm: [i16; HUF_TABLELOG_MAX + 1],
}

/// Same as [`FSE_compress`], but dedicated to huff0's weights compression.
/// The use case needs much less stack memory.
/// Note : all elements within weightTable are supposed to be <= [`HUF_TABLELOG_MAX`].
unsafe fn HUF_compressWeights(
    dst: *mut c_void,
    dstSize: usize,
    weightTable: &[u8; HUF_SYMBOLVALUE_MAX as usize],
    wtSize: usize,
    workspace: *mut c_void,
    mut workspaceSize: usize,
) -> Result<usize, Error> {
    let ostart = dst as *mut u8;
    let mut op = ostart;
    let oend = ostart.add(dstSize);

    let mut maxSymbolValue = HUF_TABLELOG_MAX as u8;
    let mut tableLog = MAX_FSE_TABLELOG_FOR_HUFF_HEADER as u32;
    let wksp = HUF_alignUpWorkspace(workspace, &mut workspaceSize, align_of::<u32>())
        as *mut HUF_CompressWeightsWksp;

    if workspaceSize < size_of::<HUF_CompressWeightsWksp>() {
        return Err(Error::GENERIC);
    }

    /* init conditions */
    if wtSize <= 1 {
        return Ok(0); /* Not compressible */
    }

    /* Scan input and build symbol stats */
    {
        let maxCount = HIST_count_simple(
            ((*wksp).count).as_mut_ptr(),
            &mut maxSymbolValue,
            &weightTable[..wtSize],
        ); /* never fails */

        if maxCount as usize == wtSize {
            return Ok(1); /* only a single symbol in src : rle */
        }

        if maxCount == 1 {
            return Ok(0); /* each symbol present maximum once => not compressible */
        }
    }
    tableLog = FSE_optimalTableLog(tableLog, wtSize, maxSymbolValue);
    FSE_normalizeCount(
        &mut (*wksp).norm,
        tableLog,
        &(*wksp).count,
        wtSize,
        maxSymbolValue,
        /* useLowProbCount */ false,
    )?;

    /* Write table description header */
    {
        let hSize = FSE_writeNCount(
            op as *mut c_void,
            oend.offset_from_unsigned(op),
            &(*wksp).norm,
            maxSymbolValue,
            tableLog,
        )?;
        op = op.add(hSize);
    }

    /* Compress */
    FSE_buildCTable_wksp(
        &mut (*wksp).CTable,
        &(*wksp).norm,
        maxSymbolValue,
        tableLog,
        ((*wksp).scratchBuffer).as_mut_ptr() as *mut c_void,
        size_of::<[u32; 41]>(),
    )?;
    {
        let cSize = FSE_compress_usingCTable(
            op as *mut c_void,
            oend.offset_from_unsigned(op),
            &weightTable[..wtSize],
            &(*wksp).CTable,
        );
        if let Some(err) = Error::from_error_code(cSize) {
            return Err(err);
        }
        if cSize == 0 {
            return Ok(0); /* not enough space for compressed data */
        }
        op = op.add(cSize);
    }
    Ok(op.offset_from_unsigned(ostart))
}

impl HUF_CElt {
    const fn get_nb_bits(self) -> usize {
        self.0 & 0xff
    }

    const fn get_nb_bits_fast(self) -> usize {
        self.0
    }

    const fn get_value(self) -> usize {
        self.0 & !0xff
    }

    const fn get_value_fast(self) -> usize {
        self.0
    }

    const fn set_nb_bits(&mut self, nbBits: usize) {
        debug_assert!(nbBits <= HUF_TABLELOG_ABSOLUTEMAX);
        self.0 = nbBits;
    }

    const fn set_value(&mut self, value: usize) {
        let nbBits = self.get_nb_bits();
        if nbBits > 0 {
            debug_assert!(value >> nbBits == 0);
            self.0 |= value << (size_of::<HUF_CElt>() * 8 - nbBits);
        }
    }
}

/// The Huffman stream end mark: A 1-bit value = 1.
const HUF_END_MARK: HUF_CElt = {
    let mut endMark = HUF_CElt(0);
    endMark.set_nb_bits(1);
    endMark.set_value(1);
    endMark
};

#[derive(Copy, Clone)]
#[repr(C)]
pub struct HUF_WriteCTableWksp {
    pub wksp: HUF_CompressWeightsWksp,
    pub bitsToWeight: [u8; HUF_TABLELOG_MAX + 1], /* precomputed conversion table */
    pub huffWeight: [u8; HUF_SYMBOLVALUE_MAX as usize],
}

pub unsafe fn HUF_writeCTable_wksp(
    dst: *mut c_void,
    maxDstSize: usize,
    CTable: &CTable,
    maxSymbolValue: u8,
    huffLog: c_uint,
    workspace: *mut c_void,
    mut workspaceSize: usize,
) -> Result<usize, Error> {
    let ct = &CTable.elements;
    let op = dst as *mut u8;
    let wksp = HUF_alignUpWorkspace(workspace, &mut workspaceSize, align_of::<u32>())
        as *mut HUF_WriteCTableWksp;

    const {
        assert!(HUF_CTABLE_WORKSPACE_SIZE >= size_of::<HUF_WriteCTableWksp>());
    }

    debug_assert_eq!(CTable.header.maxSymbolValue, maxSymbolValue);
    debug_assert_eq!(c_uint::from(CTable.header.tableLog), huffLog);

    /* check conditions */
    if workspaceSize < size_of::<HUF_WriteCTableWksp>() {
        return Err(Error::GENERIC);
    }

    let maxSymbolValue = usize::from(maxSymbolValue);

    /* convert to weight */
    (*wksp).bitsToWeight[0] = 0;
    for n in 1..huffLog + 1 {
        (*wksp).bitsToWeight[n as usize] = (huffLog + 1 - n) as u8;
    }
    let bitsToWeight = &(*wksp).bitsToWeight;
    let huffWeight = &mut (*wksp).huffWeight;
    for (weight, &elt) in huffWeight[..maxSymbolValue]
        .iter_mut()
        .zip(&ct[..maxSymbolValue])
    {
        *weight = bitsToWeight[elt.get_nb_bits()];
    }

    /* attempt weights compression by FSE */
    if maxDstSize < 1 {
        return Err(Error::dstSize_tooSmall);
    }
    {
        let hSize = HUF_compressWeights(
            op.add(1) as *mut c_void,
            maxDstSize - 1,
            &(*wksp).huffWeight,
            maxSymbolValue,
            &mut (*wksp).wksp as *mut HUF_CompressWeightsWksp as *mut c_void,
            size_of::<HUF_CompressWeightsWksp>(),
        )?;
        if (hSize > 1) && (hSize < maxSymbolValue / 2) {
            /* FSE compressed */
            *op = hSize as u8;
            return Ok(hSize + 1);
        }
    }

    /* write raw values as 4-bits (max : 15) */
    if maxSymbolValue > 256 - 128 {
        return Err(Error::GENERIC); /* should not happen : likely means source cannot be compressed */
    }
    if maxSymbolValue.div_ceil(2) + 1 > maxDstSize {
        return Err(Error::dstSize_tooSmall); /* not enough space within dst buffer */
    }
    // 128 is the special-case marker; `maxSymbolValue <= 128` was just checked, so this fits a byte
    *op = (128 + maxSymbolValue - 1) as u8;
    (*wksp).huffWeight[maxSymbolValue] = 0;
    for n in (0..maxSymbolValue).step_by(2) {
        *op.add((n / 2) + 1) = ((*wksp).huffWeight[n] << 4) + (*wksp).huffWeight[n + 1];
    }
    Ok(maxSymbolValue.div_ceil(2) + 1)
}

pub fn HUF_readCTable(
    CTable: &mut CTable,
    maxSymbolValuePtr: &mut u8,
    src: &[u8],
    hasZeroWeights: &mut c_uint,
) -> Result<usize, Error> {
    let mut huffWeight: [u8; HUF_SYMBOLVALUE_MAX as usize + 1] =
        [0; HUF_SYMBOLVALUE_MAX as usize + 1];
    let mut rankVal: [u32; HUF_TABLELOG_ABSOLUTEMAX + 1] = [0; HUF_TABLELOG_ABSOLUTEMAX + 1]; /* large enough for values from 0 to 16 */
    let mut tableLog = 0;
    let mut nbSymbols = 0;

    /* get symbol weights */
    let readSize = HUF_readStats(
        &mut huffWeight,
        255 + 1,
        &mut rankVal,
        &mut nbSymbols,
        &mut tableLog,
        src,
    )?;
    *hasZeroWeights = c_uint::from(rankVal[0] > 0);

    /* check result */
    if tableLog > HUF_TABLELOG_MAX as u32 {
        return Err(Error::tableLog_tooLarge);
    }
    if nbSymbols > c_uint::from(*maxSymbolValuePtr) + 1 {
        return Err(Error::maxSymbolValue_tooSmall);
    }

    // the check above bounds `nbSymbols - 1` by `*maxSymbolValuePtr`
    *maxSymbolValuePtr = u8::try_from(nbSymbols - 1).map_err(|_| Error::maxSymbolValue_tooSmall)?;

    CTable.header = HUF_CTableHeader::new(tableLog, *maxSymbolValuePtr);

    let ct = &mut CTable.elements;

    /* Prepare base value per rank */
    {
        let mut nextRankStart = 0u32;
        for (i, val) in rankVal[1..tableLog as usize + 1].iter_mut().enumerate() {
            let curr = nextRankStart;
            nextRankStart += *val << i;
            *val = curr;
        }
    }

    /* fill nbBits */
    {
        for n in 0..nbSymbols as usize {
            let w = huffWeight[n] as u32;
            ct[n].set_nb_bits(((tableLog + 1 - w) as u8 as c_int & -c_int::from(w != 0)) as usize);
        }
    }

    /* fill val */
    {
        let mut nbPerRank: [u16; HUF_TABLELOG_MAX + 2] = [0; HUF_TABLELOG_MAX + 2]; /* support w=0=>n=tableLog+1 */
        let mut valPerRank: [u16; HUF_TABLELOG_MAX + 2] = [0; HUF_TABLELOG_MAX + 2];

        for &elt in &ct[..nbSymbols as usize] {
            nbPerRank[elt.get_nb_bits()] += 1;
        }

        /* determine stating value per rank */
        valPerRank[(tableLog + 1) as usize] = 0; /* for w==0 */

        let mut min = 0;
        for n in (1..(tableLog + 1) as usize).rev() {
            /* start at n=tablelog <-> w=1 */
            valPerRank[n] = min; /* get starting value within each rank */
            min += nbPerRank[n];
            min >>= 1;
        }

        /* assign value within rank, symbol order */
        for elt in &mut ct[..nbSymbols as usize] {
            let val = &mut valPerRank[elt.get_nb_bits()];
            elt.set_value(*val as usize);
            *val += 1;
        }
    }
    Ok(readSize)
}

pub fn HUF_getNbBitsFromCTable(CTable: &CTable, symbolValue: u32) -> u32 {
    debug_assert!(symbolValue <= HUF_SYMBOLVALUE_MAX);
    if symbolValue > CTable.header.maxSymbolValue as u32 {
        return 0;
    }
    CTable.elements[symbolValue as usize].get_nb_bits() as u32
}

/// Try to enforce `targetNbBits` on the Huffman tree described in `huffNode`.
///
/// It attempts to convert all nodes with nbBits > `targetNbBits`
/// to employ `targetNbBits` instead. Then it adjusts the tree
/// so that it remains a valid canonical Huffman tree.
///
/// # Preconditions
///
/// The sum of the ranks of each symbol == 2^largestBits,
/// where largestBits == huffNode[lastNonNull].nbBits.
///
/// # Postconditions
///
/// The sum of the ranks of each symbol == 2^largestBits,
/// where largestBits is the return value (expected <= targetNbBits).
///
/// # Parameters
///
/// * `huffNode` - The Huffman tree modified in place to enforce targetNbBits.
///   It's presumed sorted, from most frequent to rarest symbol.
/// * `lastNonNull` - The symbol with the lowest count in the Huffman tree.
/// * `targetNbBits` - The allowed number of bits, which the Huffman tree
///   may not respect. After this function the Huffman tree will
///   respect targetNbBits.
///
/// # Returns
///
/// The maximum number of bits of the Huffman tree after adjustment.
fn HUF_setMaxHeight(huffNode: &mut [nodeElt], lastNonNull: u32, targetNbBits: u32) -> u32 {
    let largestBits = huffNode[lastNonNull as usize].nbBits as u32;
    /* early exit : no elt > targetNbBits, so the tree is already valid. */
    if largestBits <= targetNbBits {
        return largestBits;
    }

    /* there are several too large elements (at least >= 2) */
    {
        let mut totalCost: c_int = 0;
        let baseCost = (1 << (largestBits - targetNbBits)) as u32;
        let mut n = lastNonNull as c_int;

        /* Adjust any ranks > targetNbBits to targetNbBits.
         * Compute totalCost, which is how far the sum of the ranks is
         * we are over 2^largestBits after adjust the offending ranks.
         */
        while huffNode[n as usize].nbBits as u32 > targetNbBits {
            totalCost +=
                (baseCost - (1 << (largestBits - huffNode[n as usize].nbBits as u32))) as c_int;
            huffNode[n as usize].nbBits = targetNbBits as u8;
            n -= 1;
        }
        /* n stops at huffNode[n].nbBits <= targetNbBits */
        debug_assert!(huffNode[n as usize].nbBits as u32 <= targetNbBits);
        /* n end at index of smallest symbol using < targetNbBits */
        while huffNode[n as usize].nbBits as u32 == targetNbBits {
            n -= 1;
        }

        /* renorm totalCost from 2^largestBits to 2^targetNbBits
         * note : totalCost is necessarily a multiple of baseCost */
        debug_assert_eq!((totalCost as u32) & (baseCost - 1), 0);
        totalCost >>= largestBits - targetNbBits;
        debug_assert!(totalCost > 0);

        /* repay normalized cost */
        {
            let noSymbol = 0xf0f0f0f0u32;
            /* Get pos of last (smallest = lowest cum. count) symbol per rank */
            let mut rankLast: [u32; HUF_TABLELOG_MAX + 2] = [noSymbol; HUF_TABLELOG_MAX + 2];
            {
                let mut currentNbBits = targetNbBits;
                for pos in (0..n + 1).rev() {
                    if (huffNode[pos as usize].nbBits as u32) < currentNbBits {
                        currentNbBits = huffNode[pos as usize].nbBits as u32; /* < targetNbBits */
                        rankLast[(targetNbBits - currentNbBits) as usize] = pos as u32;
                    }
                }
            }

            while totalCost > 0 {
                /* Try to reduce the next power of 2 above totalCost because we
                 * gain back half the rank.
                 */
                let mut nBitsToDecrease = ZSTD_highbit32(totalCost as u32) + 1;
                debug_assert!(nBitsToDecrease as usize <= (HUF_TABLELOG_MAX + 1));
                while nBitsToDecrease > 1 {
                    let highPos = rankLast[nBitsToDecrease as usize];
                    let lowPos = rankLast[(nBitsToDecrease - 1) as usize];
                    /* Decrease highPos if no symbols of lowPos or if it is
                     * not cheaper to remove 2 lowPos than highPos.
                     */
                    if highPos != noSymbol {
                        if lowPos == noSymbol {
                            break;
                        }
                        let highTotal = huffNode[highPos as usize].count;
                        let lowTotal = 2 * huffNode[lowPos as usize].count;
                        if highTotal <= lowTotal {
                            break;
                        }
                    }
                    nBitsToDecrease -= 1;
                }
                /* only triggered when no more rank 1 symbol left => find closest one (note : there is necessarily at least one !) */
                debug_assert!(
                    rankLast[nBitsToDecrease as usize] != noSymbol || nBitsToDecrease == 1
                );

                /* HUF_MAX_TABLELOG test just to please gcc 5+; but it should not be necessary */
                while nBitsToDecrease <= HUF_TABLELOG_MAX as u32
                    && rankLast[nBitsToDecrease as usize] == noSymbol
                {
                    nBitsToDecrease += 1;
                }
                debug_assert_ne!(rankLast[nBitsToDecrease as usize], noSymbol);

                /* Increase the number of bits to gain back half the rank cost. */
                totalCost -= 1 << (nBitsToDecrease - 1);
                let fresh3 = &mut huffNode[rankLast[nBitsToDecrease as usize] as usize].nbBits;
                *fresh3 += 1;

                /* Fix up the new rank.
                 * If the new rank was empty, this symbol is now its smallest.
                 * Otherwise, this symbol will be the largest in the new rank so no adjustment.
                 */
                if rankLast[(nBitsToDecrease - 1) as usize] == noSymbol {
                    rankLast[(nBitsToDecrease - 1) as usize] = rankLast[nBitsToDecrease as usize];
                }

                /* Fix up the old rank.
                 * If the symbol was at position 0, meaning it was the highest weight symbol in the tree,
                 * it must be the only symbol in its rank, so the old rank now has no symbols.
                 * Otherwise, since the Huffman nodes are sorted by count, the previous position is now
                 * the smallest node in the rank. If the previous position belongs to a different rank,
                 * then the rank is now empty.
                 */
                if rankLast[nBitsToDecrease as usize] == 0 {
                    /* special case, reached largest symbol */
                    rankLast[nBitsToDecrease as usize] = noSymbol;
                } else {
                    rankLast[nBitsToDecrease as usize] -= 1;
                    if huffNode[rankLast[nBitsToDecrease as usize] as usize].nbBits as u32
                        != targetNbBits - nBitsToDecrease
                    {
                        rankLast[nBitsToDecrease as usize] = noSymbol;
                        /* this rank is now empty */
                    }
                }
            } /* while (totalCost > 0) */

            /* If we've removed too much weight, then we have to add it back.
             * To avoid overshooting again, we only adjust the smallest rank.
             * We take the largest nodes from the lowest rank 0 and move them
             * to rank 1. There's guaranteed to be enough rank 0 symbols because
             * TODO.
             */
            while totalCost < 0 {
                /* Sometimes, cost correction overshoot */
                /* special case : no rank 1 symbol (using targetNbBits-1);
                 * let's create one from largest rank 0 (using targetNbBits).
                 */
                if rankLast[1] == noSymbol {
                    while huffNode[n as usize].nbBits as u32 == targetNbBits {
                        n -= 1;
                    }
                    huffNode[(n + 1) as usize].nbBits -= 1;
                    debug_assert!(n >= 0);
                    rankLast[1] = (n + 1) as u32;
                    totalCost += 1;
                } else {
                    huffNode[(rankLast[1] + 1) as usize].nbBits -= 1;
                    rankLast[1] += 1;
                    totalCost += 1;
                }
            }
        } /* repay normalized cost */
    } /* there are several too large elements (at least >= 2) */
    targetNbBits
}

#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct rankPos {
    pub base: u16,
    pub curr: u16,
}

pub type huffNodeTable = [nodeElt; 2 * (HUF_SYMBOLVALUE_MAX as usize + 1)];

/// Number of buckets available for `HUF_sort`
pub const RANK_POSITION_TABLE_SIZE: usize = 192;

#[derive(Copy, Clone)]
#[repr(C)]
pub struct HUF_buildCTable_wksp_tables {
    pub huffNodeTbl: huffNodeTable,
    pub rankPosition: [rankPos; RANK_POSITION_TABLE_SIZE],
}

/// [`RANK_POSITION_DISTINCT_COUNT_CUTOFF`] == Cutoff point in `HUF_sort` buckets for which we use log2 bucketing.
/// Strategy is to use as many buckets as possible for representing distinct
/// counts while using the remainder to represent all "large" counts.
///
/// To satisfy this requirement for 192 buckets, we can do the following:
/// Let buckets 0-166 represent distinct counts of [0, 166]
/// Let buckets 166 to 192 represent all remaining counts up to [`RANK_POSITION_MAX_COUNT_LOG`] using log2 bucketing.
pub const RANK_POSITION_MAX_COUNT_LOG: usize = 32;
pub const RANK_POSITION_LOG_BUCKETS_BEGIN: c_int =
    ((RANK_POSITION_TABLE_SIZE - 1) - RANK_POSITION_MAX_COUNT_LOG - 1) as c_int; /* == 158 */
pub const RANK_POSITION_DISTINCT_COUNT_CUTOFF: c_uint = (RANK_POSITION_LOG_BUCKETS_BEGIN as c_uint)
    + (ZSTD_highbit32(RANK_POSITION_LOG_BUCKETS_BEGIN as u32)/* == 166 */);

/// Return the appropriate bucket index for a given count. See definition of
/// [`RANK_POSITION_DISTINCT_COUNT_CUTOFF`] for explanation of bucketing strategy.
fn HUF_getIndex(count: u32) -> u32 {
    if count < RANK_POSITION_DISTINCT_COUNT_CUTOFF {
        count
    } else {
        ZSTD_highbit32(count) + (RANK_POSITION_LOG_BUCKETS_BEGIN as c_uint)
    }
}

/// Insertion sort by descending order
#[inline(always)]
fn HUF_insertionSort(huffNode: &mut [nodeElt], low: c_int, high: c_int) {
    let size = high - low + 1;
    let huffNode = &mut huffNode[low as usize..];
    for i in 1..size {
        let key = huffNode[i as usize];
        let mut j = i - 1;
        while j >= 0 && huffNode[j as usize].count < key.count {
            huffNode[(j + 1) as usize] = huffNode[j as usize];
            j -= 1;
        }
        huffNode[(j + 1) as usize] = key;
    }
}

/// Pivot helper function for quicksort.
fn HUF_quickSortPartition(arr: &mut [nodeElt], low: c_int, high: c_int) -> c_int {
    /* Simply select rightmost element as pivot. "Better" selectors like
     * median-of-three don't experimentally appear to have any benefit.
     */
    let pivot = arr[high as usize].count;
    let mut i = low - 1;
    for j in low..high {
        if arr[j as usize].count > pivot {
            i += 1;
            arr.swap(i as usize, j as usize);
        }
    }
    arr.swap((i + 1) as usize, high as usize);
    i + 1
}

/// Classic quicksort by descending with partially iterative calls
/// to reduce worst case callstack size.
fn HUF_simpleQuickSort(arr: &mut [nodeElt], mut low: c_int, mut high: c_int) {
    let kInsertionSortThreshold = 8;

    if high - low < kInsertionSortThreshold {
        HUF_insertionSort(arr, low, high);
        return;
    }

    while low < high {
        let idx = HUF_quickSortPartition(arr, low, high);
        if idx - low < high - idx {
            HUF_simpleQuickSort(arr, low, idx - 1);
            low = idx + 1;
        } else {
            HUF_simpleQuickSort(arr, idx + 1, high);
            high = idx - 1;
        }
    }
}

/// Sorts the symbols [0, maxSymbolValue] by count[symbol] in decreasing order.
/// This is a typical bucket sorting strategy that uses either quicksort or insertion sort to sort each bucket.
///
/// # Parameters
///
/// * `huffNode` - Output: Sorted symbols by decreasing count. Only members `.count` and `.byte` are filled.
///   Must have (maxSymbolValue + 1) entries.
/// * `count` - Histogram of the symbols.
/// * `maxSymbolValue` - Maximum symbol value.
/// * `rankPosition` - This is a scratch workspace. Must have RANK_POSITION_TABLE_SIZE entries.
fn HUF_sort(
    huffNode: &mut [nodeElt],
    count: &[c_uint],
    maxSymbolValue: u8,
    rankPosition: &mut [rankPos; RANK_POSITION_TABLE_SIZE],
) {
    let maxSymbolValue1 = u32::from(maxSymbolValue) + 1;
    /* Compute base and set curr to base.
     * For symbol s let lowerRank = HUF_getIndex(count[n]) and rank = lowerRank + 1.
     * See HUF_getIndex to see bucketing strategy.
     * We attribute each symbol to lowerRank's base value, because we want to know where
     * each rank begins in the output, so for rank R we want to count ranks R+1 and above.
     */
    rankPosition.fill(rankPos::default());
    for &c in &count[..maxSymbolValue1 as usize] {
        let lowerRank = HUF_getIndex(c);
        debug_assert!((lowerRank as usize) < (RANK_POSITION_TABLE_SIZE - 1));
        rankPosition[lowerRank as usize].base += 1;
    }
    debug_assert_eq!(rankPosition[RANK_POSITION_TABLE_SIZE - 1].base, 0);

    /* Set up the rankPosition table */
    for n in (1..RANK_POSITION_TABLE_SIZE as u32).rev() {
        rankPosition[(n - 1) as usize].base = (rankPosition[(n - 1) as usize].base as c_int
            + rankPosition[n as usize].base as c_int)
            as u16;
        rankPosition[(n - 1) as usize].curr = rankPosition[(n - 1) as usize].base;
    }

    /* Insert each symbol into their appropriate bucket, setting up rankPosition table. */
    for (n, &c) in count[..maxSymbolValue1 as usize].iter().enumerate() {
        let r = HUF_getIndex(c) + 1;
        let pos = rankPosition[r as usize].curr as u32;
        rankPosition[r as usize].curr += 1;
        debug_assert!(pos < maxSymbolValue1);
        huffNode[pos as usize].count = c;
        huffNode[pos as usize].byte = n as u8;
    }

    /* Sort each bucket. */
    for n in RANK_POSITION_DISTINCT_COUNT_CUTOFF..(RANK_POSITION_TABLE_SIZE - 1) as u32 {
        let bucketSize =
            rankPosition[n as usize].curr as c_int - rankPosition[n as usize].base as c_int;
        let bucketStartIdx = rankPosition[n as usize].base as u32;
        if bucketSize > 1 {
            debug_assert!(bucketStartIdx < maxSymbolValue1);
            HUF_simpleQuickSort(&mut huffNode[bucketStartIdx as usize..], 0, bucketSize - 1);
        }
    }

    // huffNodes should be sorted
    debug_assert!((1..maxSymbolValue1 as usize).all(|i| huffNode[i - 1].count >= huffNode[i].count));
}

pub const STARTNODE: c_int = HUF_SYMBOLVALUE_MAX as i32 + 1;

/// Takes the huffNode array sorted by HUF_sort() and builds an unlimited-depth Huffman tree.
///
/// # Parameters
///
/// * `huffNode` - The whole node table: slot 0 is the barrier entry, and the array sorted
///   by HUF_sort() starts at slot 1. The Huffman tree is built in this array. Node indices
///   (including `nonNullRank` and the `parent` fields) count from the sorted array, hence
///   the `+ 1` on every access.
/// * `maxSymbolValue` - The maximum symbol value.
///
/// # Returns
///
/// The smallest node in the Huffman tree (by count).
fn HUF_buildTree(huffNode: &mut huffNodeTable, maxSymbolValue: u8) -> c_int {
    let mut nodeNb = STARTNODE;

    /* init for parents */
    let mut nonNullRank = c_int::from(maxSymbolValue);
    while huffNode[(nonNullRank + 1) as usize].count == 0 {
        nonNullRank -= 1;
    }
    let mut lowS = nonNullRank;
    let nodeRoot = nodeNb + lowS - 1;
    let mut lowN = nodeNb;
    huffNode[(nodeNb + 1) as usize].count =
        huffNode[(lowS + 1) as usize].count + huffNode[lowS as usize].count;
    huffNode[lowS as usize].parent = nodeNb as u16;
    huffNode[(lowS + 1) as usize].parent = nodeNb as u16;
    nodeNb += 1;
    lowS -= 2;
    for node in &mut huffNode[(nodeNb + 1) as usize..=(nodeRoot + 1) as usize] {
        node.count = 1 << 30;
    }
    huffNode[0].count = 1 << 31; /* fake entry, strong barrier */

    /* create parents */
    for nodeNb in nodeNb..nodeRoot + 1 {
        let n1 = if huffNode[(lowS + 1) as usize].count < huffNode[(lowN + 1) as usize].count {
            let val = lowS;
            lowS -= 1;
            val
        } else {
            let val = lowN;
            lowN += 1;
            val
        };
        let n2 = if huffNode[(lowS + 1) as usize].count < huffNode[(lowN + 1) as usize].count {
            let val = lowS;
            lowS -= 1;
            val
        } else {
            let val = lowN;
            lowN += 1;
            val
        };
        huffNode[(nodeNb + 1) as usize].count =
            huffNode[(n1 + 1) as usize].count + huffNode[(n2 + 1) as usize].count;
        huffNode[(n2 + 1) as usize].parent = nodeNb as u16;
        huffNode[(n1 + 1) as usize].parent = nodeNb as u16;
    }

    /* distribute weights (unlimited tree height) */
    huffNode[(nodeRoot + 1) as usize].nbBits = 0;
    for n in (STARTNODE..nodeRoot).rev() {
        let parent = huffNode[(n + 1) as usize].parent;
        huffNode[(n + 1) as usize].nbBits = huffNode[usize::from(parent) + 1].nbBits + 1;
    }
    for n in 0..nonNullRank + 1 {
        let parent = huffNode[(n + 1) as usize].parent;
        huffNode[(n + 1) as usize].nbBits = huffNode[usize::from(parent) + 1].nbBits + 1;
    }
    nonNullRank
}

/// Build the CTable given the Huffman tree in huffNode.
///
/// # Parameters
///
/// * `CTable` - Output: The output Huffman CTable.
/// * `huffNode` - The Huffman tree.
/// * `nonNullRank` - The last and smallest node in the Huffman tree.
/// * `maxSymbolValue` - The maximum symbol value.
/// * `maxNbBits` - The exact maximum number of bits used in the Huffman tree.
fn HUF_buildCTableFromTree(
    CTable: &mut CTable,
    huffNode: &[nodeElt],
    nonNullRank: c_int,
    maxSymbolValue: u8,
    maxNbBits: u32,
) {
    /* fill result into ctable (val, nbBits) */
    let ct = &mut CTable.elements;
    let mut nbPerRank: [u16; HUF_TABLELOG_MAX + 1] = [0; HUF_TABLELOG_MAX + 1];
    let mut valPerRank: [u16; HUF_TABLELOG_MAX + 1] = [0; HUF_TABLELOG_MAX + 1];
    let alphabetSize = c_int::from(maxSymbolValue) + 1;
    for n in 0..nonNullRank + 1 {
        nbPerRank[huffNode[n as usize].nbBits as usize] += 1;
    }

    /* determine starting value per rank */
    let mut min = 0;
    for n in (1..maxNbBits as c_int + 1).rev() {
        valPerRank[n as usize] = min; /* get starting value within each rank */
        min = (min as c_int + nbPerRank[n as usize] as c_int) as u16;
        min = (min as c_int >> 1) as u16;
    }

    for n in 0..alphabetSize {
        /* push nbBits per symbol, symbol order */
        ct[huffNode[n as usize].byte as usize].set_nb_bits(huffNode[n as usize].nbBits as usize);
    }
    for n in 0..alphabetSize {
        let fresh19 = &mut valPerRank[ct[n as usize].get_nb_bits()];
        ct[n as usize].set_value(*fresh19 as usize); /* assign value within rank, symbol order */
        *fresh19 += 1;
    }

    CTable.header = HUF_CTableHeader::new(maxNbBits, maxSymbolValue);
}

/// Same as `HUF_buildCTable`, but using externally allocated scratch buffer.
/// `workSpace` must be aligned on 4-bytes boundaries, and be at least as large as sizeof([`HUF_buildCTable_wksp_tables`]).
pub unsafe fn HUF_buildCTable_wksp(
    CTable: &mut CTable,
    count: &[c_uint],
    maxSymbolValue: u8,
    mut maxNbBits: u32,
    workSpace: *mut c_void,
    mut wkspSize: usize,
) -> Result<core::ffi::c_uint, Error> {
    let wksp_tables = HUF_alignUpWorkspace(workSpace, &mut wkspSize, align_of::<u32>())
        as *mut HUF_buildCTable_wksp_tables;
    let huffNodeTbl = &mut (*wksp_tables).huffNodeTbl;

    const {
        assert!(HUF_CTABLE_WORKSPACE_SIZE == size_of::<HUF_buildCTable_wksp_tables>());
    }

    /* safety checks */
    if wkspSize < size_of::<HUF_buildCTable_wksp_tables>() {
        return Err(Error::workSpace_tooSmall);
    }
    if maxNbBits == 0 {
        maxNbBits = HUF_TABLELOG_DEFAULT;
    }
    huffNodeTbl.fill(nodeElt_s::default());
    /* sort, decreasing order */
    HUF_sort(
        &mut huffNodeTbl[1..],
        count,
        maxSymbolValue,
        &mut (*wksp_tables).rankPosition,
    );

    /* build tree */
    let nonNullRank = HUF_buildTree(huffNodeTbl, maxSymbolValue);

    /* determine and enforce maxTableLog */
    maxNbBits = HUF_setMaxHeight(&mut huffNodeTbl[1..], nonNullRank as u32, maxNbBits);
    if maxNbBits > HUF_TABLELOG_MAX as u32 {
        return Err(Error::GENERIC); /* check fit into table */
    }
    HUF_buildCTableFromTree(
        CTable,
        &huffNodeTbl[1..],
        nonNullRank,
        maxSymbolValue,
        maxNbBits,
    );
    Ok(maxNbBits)
}

pub fn HUF_estimateCompressedSize(CTable: &CTable, count: &[c_uint], maxSymbolValue: u8) -> usize {
    let ct = &CTable.elements;
    let mut nbBits = 0usize;
    for (elt, &c) in ct[..=usize::from(maxSymbolValue)].iter().zip(count) {
        nbBits += elt.get_nb_bits() * c as usize;
    }
    nbBits >> 3
}

pub fn HUF_validateCTable(CTable: &CTable, count: &[c_uint], maxSymbolValue: u8) -> bool {
    let header = CTable.header;
    let ct = &CTable.elements;
    let mut bad = false;

    debug_assert!(header.tableLog as usize <= HUF_TABLELOG_ABSOLUTEMAX);

    if header.maxSymbolValue < maxSymbolValue {
        return false;
    }
    for (elt, &c) in ct[..=usize::from(maxSymbolValue)].iter().zip(count) {
        // NOTE: use `&` rather than `&&` to keep the loop branch-free
        bad |= (c != 0) & (elt.get_nb_bits() == 0);
    }
    !bad
}

pub fn HUF_compressBound(size: usize) -> usize {
    HUF_CTABLEBOUND + (size + (size >> 8) + 8)
}

pub const HUF_BITS_IN_CONTAINER: usize = usize::BITS as usize;

/// Huffman uses its own `BIT_CStream_t` implementation.
///
/// There are three major differences from `BIT_CStream_t`:
///   1. `add_bits` takes a `HUF_CElt` (usize) which is
///      the pair (nbBits, value) in the format:
///        - Bits `[0, 4)`            = nbBits
///        - Bits `[4, 64 - nbBits)`  = 0
///        - Bits `[64 - nbBits, 64)` = value
///   2. The bitContainer is built from the upper bits and
///      right shifted. E.g. to add a new value of N bits
///      you right shift the bitContainer by N, then or in
///      the new value into the N upper bits.
///   3. The bitstream has two bit containers. You can add
///      bits to the second container and merge them into
///      the first container.
#[repr(C)]
pub struct HUF_CStream_t {
    pub bitContainer: [usize; 2],
    pub bitPos: [usize; 2],
    pub startPtr: *mut u8,
    pub ptr: *mut u8,
    pub endPtr: *mut u8,
}

impl HUF_CStream_t {
    pub fn new(startPtr: *mut c_void, dstCapacity: usize) -> Result<Self, Error> {
        if dstCapacity <= size_of::<usize>() {
            return Err(Error::dstSize_tooSmall);
        }

        let startPtr = startPtr as *mut u8;
        let endPtr = startPtr
            .wrapping_add(dstCapacity)
            .wrapping_sub(size_of::<usize>());

        Ok(HUF_CStream_t {
            bitContainer: [0; 2],
            bitPos: [0; 2],
            startPtr,
            ptr: startPtr,
            endPtr,
        })
    }

    /// Adds the symbol stored in HUF_CElt elt to the bitstream.
    ///
    /// # Parameters
    ///
    /// * `elt` - The element we're adding. This is a (nbBits, value) pair.
    ///   See the HUF_CStream_t docs for the format.
    /// * `idx` - Insert into bit container `0` (`false`) or `1` (`true`).
    /// * `kFast` - This is a template parameter. If the bitstream is guaranteed
    ///   to have at least 4 unused bits after this call it may be `true`,
    ///   otherwise it must be `false`.
    #[inline(always)]
    fn add_bits(&mut self, elt: HUF_CElt, idx: bool, kFast: bool) {
        debug_assert!(elt.get_nb_bits() <= HUF_TABLELOG_ABSOLUTEMAX);
        // This is efficient on x86-64 with BMI2 because shrx
        // only reads the low 6 bits of the register. The compiler
        // knows this and elides the mask. When fast is set,
        // every operation can use the same value loaded from elt.
        self.bitContainer[usize::from(idx)] >>= elt.get_nb_bits();
        self.bitContainer[usize::from(idx)] |= if kFast {
            elt.get_value_fast()
        } else {
            elt.get_value()
        };
        // We only read the low 8 bits of bitC->bitPos[idx] so it
        // doesn't matter that the high bits have noise from the value.
        let fresh21 = &mut self.bitPos[usize::from(idx)];
        *fresh21 = (*fresh21).wrapping_add(elt.get_nb_bits_fast());
        debug_assert!((self.bitPos[usize::from(idx)] & 0xFF) <= HUF_BITS_IN_CONTAINER);
        // The last 4-bits of elt are dirty if fast is set,
        // so we must not be overwriting bits that have already been
        // inserted into the bit container.
    }

    /// Zeros the bit container @ index 1.
    #[inline(always)]
    fn zero_index_1(&mut self) {
        self.bitContainer[1] = 0;
        self.bitPos[1] = 0;
    }

    /// Merges the bit container @ index 1 into the bit container @ index 0.
    #[inline(always)]
    fn merge_index_1(&mut self) {
        debug_assert!((self.bitPos[1] & 0xFF) < HUF_BITS_IN_CONTAINER);
        self.bitContainer[0] >>= self.bitPos[1] & 0xff;
        self.bitContainer[0] |= self.bitContainer[1];
        self.bitPos[0] += self.bitPos[1];
        debug_assert!((self.bitPos[0] & 0xFF) <= HUF_BITS_IN_CONTAINER);
    }

    /// Flushes the bits in the bit container @ index 0.
    ///
    /// # Parameters
    ///
    /// * `kFast` - If kFast is set then we must know a-priori that
    ///   the bit container will not overflow.
    ///
    /// # Postconditions
    ///
    /// bitPos will be < 8.
    #[inline(always)]
    unsafe fn flush_bits(&mut self, kFast: bool) {
        // The upper bits of bitPos are noisy, so we must mask by 0xFF.
        let nbBits = self.bitPos[0] & 0xff;
        let nbBytes = nbBits >> 3;
        // The top nbBits bits of bitContainer are the ones we need.
        let bitContainer = self.bitContainer[0] >> (HUF_BITS_IN_CONTAINER - nbBits);
        // Mask bitPos to account for the bytes we consumed.
        self.bitPos[0] &= 7;
        debug_assert!(nbBits > 0);
        debug_assert!(nbBits <= usize::BITS as usize);
        debug_assert!(self.ptr <= self.endPtr);
        MEM_writeLEST(self.ptr as *mut c_void, bitContainer);
        self.ptr = self.ptr.add(nbBytes);
        debug_assert!(!kFast || self.ptr <= self.endPtr);
        if !kFast && self.ptr > self.endPtr {
            self.ptr = self.endPtr;
        }
        // bitContainer doesn't need to be modified because the leftover
        // bits are already the top bitPos bits. And we don't care about
        // noise in the lower values.
    }

    /// # Returns
    ///
    /// Size of CStream, in bytes, or 0 if it could not fit into dstBuffer
    unsafe fn close(mut self) -> usize {
        self.add_bits(HUF_END_MARK, false, false);
        self.flush_bits(false);
        let nbBits = self.bitPos[0] & 0xff;
        if self.ptr >= self.endPtr {
            return 0; // overflow detected
        }
        self.ptr.offset_from_unsigned(self.startPtr) + usize::from(nbBits > 0)
    }

    #[inline(always)]
    fn encode_symbol(&mut self, symbol: u8, CTable: &SymbolTable, idx: bool, fast: bool) {
        self.add_bits(CTable[usize::from(symbol)], idx, fast);
    }
}

#[inline(always)]
unsafe fn HUF_compress1X_usingCTable_internal_body_loop<const K_UNROLL: usize>(
    bitC: &mut HUF_CStream_t,
    src: &[u8],
    ct: &SymbolTable,
    kFastFlush: bool,
    kLastFast: bool,
) {
    let (blocks, rem) = src.as_chunks::<K_UNROLL>();

    /* Join to K_UNROLL */
    if !rem.is_empty() {
        for &byte in rem.iter().rev() {
            bitC.encode_symbol(byte, ct, false, false);
        }
        bitC.flush_bits(kFastFlush);
    }

    let (pairs, single) = blocks.as_chunks::<2>();

    /* Join to 2 * K_UNROLL */
    if let [block] = single {
        for u in 1..K_UNROLL {
            bitC.encode_symbol(block[K_UNROLL - u], ct, false, true);
        }
        bitC.encode_symbol(block[0], ct, false, kLastFast);
        bitC.flush_bits(kFastFlush);
    }

    for [lo, hi] in pairs.iter().rev() {
        /* Encode K_UNROLL symbols into the bitstream @ index 0. */
        for u in 1..K_UNROLL {
            bitC.encode_symbol(hi[K_UNROLL - u], ct, false, true);
        }
        bitC.encode_symbol(hi[0], ct, false, kLastFast);
        bitC.flush_bits(kFastFlush);
        /* Encode K_UNROLL symbols into the bitstream @ index 1.
         * This allows us to start filling the bit container
         * without any data dependencies.
         */
        bitC.zero_index_1();
        for u in 1..K_UNROLL {
            bitC.encode_symbol(lo[K_UNROLL - u], ct, true, true);
        }
        bitC.encode_symbol(lo[0], ct, true, kLastFast);
        /* Merge bitstream @ index 1 into the bitstream @ index 0 */
        bitC.merge_index_1();
        bitC.flush_bits(kFastFlush);
    }
}

/// Returns a tight upper bound on the output space needed by Huffman
/// with 8 bytes buffer to handle over-writes. If the output is at least
/// this large we don't need to do bounds checks during Huffman encoding.
fn HUF_tightCompressBound(srcSize: usize, tableLog: usize) -> usize {
    ((srcSize * tableLog) >> 3) + 8
}

#[inline(always)]
unsafe fn HUF_compress1X_usingCTable_internal_body(
    dst: *mut c_void,
    dstSize: usize,
    src: &[u8],
    CTable: &CTable,
) -> Result<usize, Error> {
    let tableLog = CTable.header.tableLog as u32;
    let ct = &CTable.elements;

    /* init */
    if dstSize < 8 {
        return Ok(0); /* not enough space to compress */
    }

    let mut bitC = match HUF_CStream_t::new(dst, dstSize) {
        Ok(bitC) => bitC,
        Err(_) => return Ok(0),
    };

    if dstSize < HUF_tightCompressBound(src.len(), tableLog as usize) || tableLog > 11 {
        if MEM_32bits() {
            HUF_compress1X_usingCTable_internal_body_loop::<2>(&mut bitC, src, ct, false, false);
        } else {
            HUF_compress1X_usingCTable_internal_body_loop::<4>(&mut bitC, src, ct, false, false);
        }
    } else if MEM_32bits() {
        match tableLog {
            11 => {
                HUF_compress1X_usingCTable_internal_body_loop::<2>(&mut bitC, src, ct, true, false);
            }
            8..=10 => {
                HUF_compress1X_usingCTable_internal_body_loop::<2>(&mut bitC, src, ct, true, true);
            }
            _ => {
                HUF_compress1X_usingCTable_internal_body_loop::<3>(&mut bitC, src, ct, true, true);
            }
        }
    } else {
        match tableLog {
            11 => {
                HUF_compress1X_usingCTable_internal_body_loop::<5>(&mut bitC, src, ct, true, false);
            }
            10 => {
                HUF_compress1X_usingCTable_internal_body_loop::<5>(&mut bitC, src, ct, true, true);
            }
            9 => {
                HUF_compress1X_usingCTable_internal_body_loop::<6>(&mut bitC, src, ct, true, false);
            }
            8 => {
                HUF_compress1X_usingCTable_internal_body_loop::<7>(&mut bitC, src, ct, true, false);
            }
            7 => {
                HUF_compress1X_usingCTable_internal_body_loop::<8>(&mut bitC, src, ct, true, false);
            }
            _ => {
                HUF_compress1X_usingCTable_internal_body_loop::<9>(&mut bitC, src, ct, true, true);
            }
        }
    }
    debug_assert!(bitC.ptr <= bitC.endPtr);
    Ok(bitC.close())
}

#[cfg_attr(target_arch = "x86_64", target_feature(enable = "bmi2"))]
unsafe fn HUF_compress1X_usingCTable_internal_bmi2(
    dst: *mut c_void,
    dstSize: usize,
    src: &[u8],
    CTable: &CTable,
) -> Result<usize, Error> {
    HUF_compress1X_usingCTable_internal_body(dst, dstSize, src, CTable)
}

unsafe fn HUF_compress1X_usingCTable_internal_default(
    dst: *mut c_void,
    dstSize: usize,
    src: &[u8],
    CTable: &CTable,
) -> Result<usize, Error> {
    HUF_compress1X_usingCTable_internal_body(dst, dstSize, src, CTable)
}

unsafe fn HUF_compress1X_usingCTable_internal(
    dst: *mut c_void,
    dstSize: usize,
    src: &[u8],
    CTable: &CTable,
    flags: c_int,
) -> Result<usize, Error> {
    if flags & HUF_flags_bmi2 as c_int != 0 {
        return HUF_compress1X_usingCTable_internal_bmi2(dst, dstSize, src, CTable);
    }
    HUF_compress1X_usingCTable_internal_default(dst, dstSize, src, CTable)
}

pub unsafe fn HUF_compress1X_usingCTable(
    dst: *mut c_void,
    dstSize: usize,
    src: &[u8],
    CTable: &CTable,
    flags: c_int,
) -> Result<usize, Error> {
    HUF_compress1X_usingCTable_internal(dst, dstSize, src, CTable, flags)
}

unsafe fn HUF_compress4X_usingCTable_internal(
    dst: *mut c_void,
    dstSize: usize,
    src: &[u8],
    CTable: &CTable,
    flags: c_int,
) -> Result<usize, Error> {
    let segmentSize = src.len().div_ceil(4); /* first 3 segments */
    let ostart = dst as *mut u8;
    let oend = ostart.add(dstSize);
    let mut op = ostart;

    if dstSize < (6 + 1 + 1 + 1 + 8) {
        return Ok(0); /* minimum space to compress successfully */
    }

    if src.len() < 12 {
        return Ok(0); /* no saving possible : too small input */
    }
    let (src0, rest) = src.split_at(segmentSize);
    let (src1, rest) = rest.split_at(segmentSize);
    let (src2, src3) = rest.split_at(segmentSize);
    op = op.add(6); /* jumpTable */

    debug_assert!(op <= oend);

    {
        assert!(op <= oend);
        let cSize = HUF_compress1X_usingCTable_internal(
            op as *mut c_void,
            oend.offset_from_unsigned(op),
            src0,
            CTable,
            flags,
        )?;
        if cSize == 0 || cSize > 65535 {
            return Ok(0);
        }
        MEM_writeLE16(ostart as *mut c_void, cSize as u16);
        op = op.add(cSize);
    }

    debug_assert!(op <= oend);
    {
        let cSize_0 = HUF_compress1X_usingCTable_internal(
            op as *mut c_void,
            oend.offset_from_unsigned(op),
            src1,
            CTable,
            flags,
        )?;
        if cSize_0 == 0 || cSize_0 > 65535 {
            return Ok(0);
        }
        MEM_writeLE16(ostart.add(2) as *mut c_void, cSize_0 as u16);
        op = op.add(cSize_0);
    }

    debug_assert!(op <= oend);
    {
        let cSize_1 = HUF_compress1X_usingCTable_internal(
            op as *mut c_void,
            oend.offset_from_unsigned(op),
            src2,
            CTable,
            flags,
        )?;
        if cSize_1 == 0 || cSize_1 > 65535 {
            return Ok(0);
        }
        MEM_writeLE16(ostart.add(4) as *mut c_void, cSize_1 as u16);
        op = op.add(cSize_1);
    }

    debug_assert!(op <= oend);
    {
        let cSize_2 = HUF_compress1X_usingCTable_internal(
            op as *mut c_void,
            oend.offset_from_unsigned(op),
            src3,
            CTable,
            flags,
        )?;
        if cSize_2 == 0 || cSize_2 > 65535 {
            return Ok(0);
        }
        op = op.add(cSize_2);
    }

    Ok(op.offset_from_unsigned(ostart))
}

pub unsafe fn HUF_compress4X_usingCTable(
    dst: *mut c_void,
    dstSize: usize,
    src: &[u8],
    CTable: &CTable,
    flags: c_int,
) -> Result<usize, Error> {
    HUF_compress4X_usingCTable_internal(dst, dstSize, src, CTable, flags)
}

#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq)]
pub enum HUF_nbStreams_e {
    Single = 0,
    Four = 1,
}

unsafe fn HUF_compressCTable_internal(
    ostart: *mut u8,
    mut op: *mut u8,
    oend: *mut u8,
    src: &[u8],
    nbStreams: HUF_nbStreams_e,
    CTable: &CTable,
    flags: c_int,
) -> Result<usize, Error> {
    let cSize = match nbStreams {
        HUF_nbStreams_e::Single => HUF_compress1X_usingCTable_internal(
            op as *mut c_void,
            oend.offset_from_unsigned(op),
            src,
            CTable,
            flags,
        )?,
        HUF_nbStreams_e::Four => HUF_compress4X_usingCTable_internal(
            op as *mut c_void,
            oend.offset_from_unsigned(op),
            src,
            CTable,
            flags,
        )?,
    };

    if cSize == 0 {
        return Ok(0); /* uncompressible */
    }
    op = op.add(cSize);

    /* check compressibility */
    debug_assert!(op >= ostart);
    if op.offset_from_unsigned(ostart) >= src.len() - 1 {
        return Ok(0);
    }

    Ok(op.offset_from_unsigned(ostart))
}

#[derive(Copy, Clone)]
#[repr(C)]
pub union workspace_union {
    pub buildCTable_wksp: HUF_buildCTable_wksp_tables,
    pub writeCTable_wksp: HUF_WriteCTableWksp,
    pub hist_wksp: [u32; HIST_WKSP_SIZE_U32],
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct HUF_compress_tables_t {
    pub count: [c_uint; HUF_SYMBOLVALUE_MAX as usize + 1],
    pub CTable: CTable,
    pub wksps: workspace_union,
}

pub const SUSPECT_INCOMPRESSIBLE_SAMPLE_SIZE: usize = 4096;
pub const SUSPECT_INCOMPRESSIBLE_SAMPLE_RATIO: usize = 10; /* Must be >= 2 */

pub fn HUF_cardinality(count: &[c_uint], maxSymbolValue: u8) -> c_uint {
    let mut cardinality = 0;
    for &c in &count[..usize::from(maxSymbolValue) + 1] {
        if c != 0 {
            cardinality += 1;
        }
    }
    cardinality
}

pub fn HUF_minTableLog(symbolCardinality: c_uint) -> c_uint {
    ZSTD_highbit32(symbolCardinality) + 1
}

pub unsafe fn HUF_optimalTableLog(
    maxTableLog: c_uint,
    srcSize: usize,
    maxSymbolValue: u8,
    workSpace: *mut c_void,
    wkspSize: usize,
    table: &mut CTable,
    count: &[c_uint],
    flags: c_int,
) -> c_uint {
    debug_assert!(srcSize > 1); /* Not supported, RLE should be used instead */
    debug_assert!(wkspSize >= size_of::<HUF_buildCTable_wksp_tables>());

    if flags & HUF_flags_optimalDepth as c_int == 0 {
        /* cheap evaluation, based on FSE */
        return FSE_optimalTableLog_internal(maxTableLog, srcSize, maxSymbolValue, 1);
    }
    let dst = workSpace.byte_offset(size_of::<HUF_WriteCTableWksp>() as isize);
    let dstSize = wkspSize - size_of::<HUF_WriteCTableWksp>();
    let symbolCardinality = HUF_cardinality(count, maxSymbolValue);
    let minTableLog = HUF_minTableLog(symbolCardinality);
    let mut optSize = usize::MAX - 1;
    let mut optLog = maxTableLog;

    /* Search until size increases */
    for optLogGuess in minTableLog..maxTableLog + 1 {
        if let Ok(maxBits) = HUF_buildCTable_wksp(
            table,
            count,
            maxSymbolValue,
            optLogGuess,
            workSpace,
            wkspSize,
        ) {
            if maxBits < optLogGuess && optLogGuess > minTableLog {
                break;
            }
            let hSize = HUF_writeCTable_wksp(
                dst,
                dstSize,
                table,
                maxSymbolValue,
                maxBits,
                workSpace,
                wkspSize,
            );
            if let Ok(hSize) = hSize {
                let newSize = HUF_estimateCompressedSize(table, count, maxSymbolValue) + hSize;
                if newSize > optSize + 1 {
                    break;
                }
                if newSize < optSize {
                    optSize = newSize;
                    optLog = optLogGuess;
                }
            }
        }
    }
    debug_assert!(optLog as usize <= HUF_TABLELOG_MAX);
    optLog
}

/// `workSpace_align4` must be aligned on 4-bytes boundaries,
/// and occupies the same space as a table of HUF_WORKSPACE_SIZE_U64 unsigned
pub(crate) unsafe fn HUF_compress<const NB_STREAMS: u32>(
    dst: *mut c_void,
    dstSize: usize,
    src: &[u8],
    maxSymbolValue: c_uint,
    mut huffLog: c_uint,
    workSpace: *mut c_void,
    mut wkspSize: usize,
    oldHufTable: &mut CTable,
    repeat: &mut HUF_repeat,
    flags: c_int,
) -> Result<usize, Error> {
    const { assert!(matches!(NB_STREAMS, 1 | 4)) };

    let nbStreams = match NB_STREAMS {
        1 => HUF_nbStreams_e::Single,
        4 => HUF_nbStreams_e::Four,
        _ => unreachable!(),
    };

    let table = HUF_alignUpWorkspace(workSpace, &mut wkspSize, align_of::<usize>())
        as *mut HUF_compress_tables_t;

    let ostart = dst as *mut u8;
    let oend = ostart.add(dstSize);
    let mut op = ostart;

    /* checks & inits */
    const {
        assert!(
            size_of::<HUF_compress_tables_t>() + HUF_WORKSPACE_MAX_ALIGNMENT <= HUF_WORKSPACE_SIZE
        );
    }

    if wkspSize < size_of::<HUF_compress_tables_t>() {
        return Err(Error::workSpace_tooSmall);
    }

    // Initialize the CTable so we can take a (mutable) reference to its contents.
    core::ptr::write_bytes(&raw mut (*table).CTable, 0, 1);

    if src.is_empty() {
        return Ok(0); /* Uncompressed */
    }

    if dstSize == 0 {
        return Ok(0); /* cannot fit anything within dst budget */
    }

    if src.len() > HUF_BLOCKSIZE_MAX {
        return Err(Error::srcSize_wrong);
    }

    if huffLog > HUF_TABLELOG_MAX as c_uint {
        return Err(Error::tableLog_tooLarge); /* current block size limit */
    }

    // The value is restricted to the u8 range, so let's just use a u8.
    const _: () = assert!(HUF_SYMBOLVALUE_MAX == 255);

    let Ok(mut maxSymbolValue) = u8::try_from(maxSymbolValue) else {
        return Err(Error::maxSymbolValue_tooLarge);
    };

    if maxSymbolValue == 0 {
        maxSymbolValue = HUF_SYMBOLVALUE_MAX_U8;
    }

    if huffLog == 0 {
        huffLog = HUF_TABLELOG_DEFAULT;
    }

    /* Heuristic : If old table is valid, use it for small inputs */
    if flags & HUF_flags_preferRepeat as c_int != 0 && *repeat == HUF_repeat::Valid {
        return HUF_compressCTable_internal(ostart, op, oend, src, nbStreams, oldHufTable, flags);
    }

    /* If uncompressible data is suspected, do a smaller sampling first */
    if flags & HUF_flags_suspectUncompressible as c_int != 0
        && src.len() >= SUSPECT_INCOMPRESSIBLE_SAMPLE_SIZE * SUSPECT_INCOMPRESSIBLE_SAMPLE_RATIO
    {
        let mut largestTotal = 0usize;
        let mut maxSymbolValueBegin = maxSymbolValue;
        let largestBegin = HIST_count_simple(
            ((*table).count).as_mut_ptr(),
            &mut maxSymbolValueBegin,
            &src[..SUSPECT_INCOMPRESSIBLE_SAMPLE_SIZE],
        ) as usize;
        if let Some(err) = Error::from_error_code(largestBegin) {
            return Err(err);
        }
        largestTotal += largestBegin;
        let mut maxSymbolValueEnd = maxSymbolValue;
        let largestEnd = HIST_count_simple(
            ((*table).count).as_mut_ptr(),
            &mut maxSymbolValueEnd,
            &src[src.len() - SUSPECT_INCOMPRESSIBLE_SAMPLE_SIZE..],
        ) as usize;
        if let Some(err) = Error::from_error_code(largestEnd) {
            return Err(err);
        }
        largestTotal += largestEnd;
        if largestTotal <= ((2 * SUSPECT_INCOMPRESSIBLE_SAMPLE_SIZE) >> 7) + 4 {
            return Ok(0); /* heuristic : probably not compressible enough */
        }
    }

    /* Scan input and build symbol stats */
    (*table).wksps.hist_wksp.fill(0);
    let largest = HIST_count_wksp_array(
        ((*table).count).as_mut_ptr(),
        &mut maxSymbolValue,
        src,
        &mut (*table).wksps.hist_wksp,
    )? as usize;
    if largest == src.len() {
        *ostart = src[0];
        return Ok(1); /* single symbol, rle */
    }
    if largest <= (src.len() >> 7) + 4 {
        return Ok(0); /* heuristic : probably not compressible enough */
    }

    /* Check validity of previous table */
    if *repeat == HUF_repeat::Check
        && !HUF_validateCTable(oldHufTable, &(*table).count, maxSymbolValue)
    {
        *repeat = HUF_repeat::None;
    }

    /* Heuristic : use existing table for small inputs */
    if flags & HUF_flags_preferRepeat as c_int != 0 && *repeat != HUF_repeat::None {
        return HUF_compressCTable_internal(ostart, op, oend, src, nbStreams, oldHufTable, flags);
    }

    /* Build Huffman Tree */
    huffLog = HUF_optimalTableLog(
        huffLog,
        src.len(),
        maxSymbolValue,
        &mut (*table).wksps as *mut workspace_union as *mut c_void,
        size_of::<workspace_union>(),
        &mut (*table).CTable,
        &(*table).count,
        flags,
    );
    let maxBits = HUF_buildCTable_wksp(
        &mut (*table).CTable,
        &(*table).count,
        maxSymbolValue,
        huffLog,
        &mut (*table).wksps.buildCTable_wksp as *mut HUF_buildCTable_wksp_tables as *mut c_void,
        size_of::<HUF_buildCTable_wksp_tables>(),
    )?;
    huffLog = maxBits as u32;

    /* Write table description header */
    {
        let hSize = HUF_writeCTable_wksp(
            op as *mut c_void,
            dstSize,
            &(*table).CTable,
            maxSymbolValue,
            huffLog,
            &mut (*table).wksps.writeCTable_wksp as *mut HUF_WriteCTableWksp as *mut c_void,
            size_of::<HUF_WriteCTableWksp>(),
        )?;

        /* Check if using previous huffman table is beneficial */
        if *repeat != HUF_repeat::None {
            let oldSize = HUF_estimateCompressedSize(oldHufTable, &(*table).count, maxSymbolValue);
            let newSize =
                HUF_estimateCompressedSize(&(*table).CTable, &(*table).count, maxSymbolValue);

            if oldSize <= hSize + newSize || hSize + 12 >= src.len() {
                return HUF_compressCTable_internal(
                    ostart,
                    op,
                    oend,
                    src,
                    nbStreams,
                    oldHufTable,
                    flags,
                );
            }
        }

        /* Use the new huffman table */
        if hSize + 12 >= src.len() {
            return Ok(0);
        }
        op = op.add(hSize);
        *repeat = HUF_repeat::None;
        *oldHufTable = (*table).CTable;
    }
    HUF_compressCTable_internal(ostart, op, oend, src, nbStreams, &(*table).CTable, flags)
}
