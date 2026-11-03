use crate::internal::MEM_readLE32;
use crate::lib::common::bits::ZSTD_highbit32;
use crate::lib::common::error_private::Error;
use crate::lib::common::fse::{FSE_CTable, FSE_repeat};
use crate::lib::common::huf::{CTable, HUF_repeat};
use crate::lib::common::mem::{
    MEM_64bits, MEM_read16, MEM_read32, MEM_readLE64, MEM_readST, MEM_writeLE24,
};
use crate::lib::common::zstd_internal::{
    BlockType, MaxLL, MaxLit, MaxML, MaxOff, Overlap, RepCodes, ZSTD_copy16, ZSTD_wildcopy,
    MINMATCH, WILDCOPY_OVERLENGTH, ZSTD_BLOCKHEADERSIZE, ZSTD_REP_NUM,
};
use crate::lib::compress::zstd_compress::{
    SeqDef, SeqStore_t, ZSTD_CDict, ZSTD_MatchState_t, ZSTD_compressedBlockState_t,
    ZSTD_entropyCTablesMetadata_t, ZSTD_optimal_t, ZSTD_window_t, ZSTD_MAX_NB_BLOCK_SPLITS,
    ZSTD_OPT_SIZE,
};
use crate::lib::polyfill::PointerExt;
use crate::lib::zstd::{
    ParamSwitch, ZSTD_Sequence, ZSTD_btultra, ZSTD_dictContentType_e, ZSTD_strategy,
};

pub const kSearchStrength: core::ffi::c_int = 8;
pub const HASH_READ_SIZE: core::ffi::c_int = 8;
/// For btlazy2 strategy, index `ZSTD_DUBT_UNSORTED_MARK==1` means "unsorted".
/// It could be confused for a real successor at index "1", if sorted as larger than its predecessor.
/// It's not a big deal though: the candidate will just be sorted again.
/// Additionally, candidate position 1 will be lost.
/// But candidate 1 cannot hide a large tree of candidates, so it's a minimal loss.
/// The benefit is that `ZSTD_DUBT_UNSORTED_MARK` cannot be mishandled after table reuse with a different strategy.
/// This constant is required by `ZSTD_compressBlock_btlazy2()` and `ZSTD_reduceTable_internal()`
pub const ZSTD_DUBT_UNSORTED_MARK: core::ffi::c_int = 1;

/// Number of low bits of a hash table entry reserved for the match tag,
/// used by the short-cache matchfinders.
#[inline]
pub(crate) fn ZSTD_LLcode(litLength: u32) -> u32 {
    static LL_Code: [u8; 64] = [
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 16, 17, 17, 18, 18, 19, 19, 20,
        20, 20, 20, 21, 21, 21, 21, 22, 22, 22, 22, 22, 22, 22, 22, 23, 23, 23, 23, 23, 23, 23, 23,
        24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24, 24,
    ];
    static LL_deltaCode: u32 = 19;
    if litLength > 63 {
        ZSTD_highbit32(litLength).wrapping_add(LL_deltaCode)
    } else {
        LL_Code[litLength as usize] as core::ffi::c_uint
    }
}

/// Note: mlBase = matchLength - MINMATCH;
/// because it's the format it's stored in seqStore->sequences
#[inline]
pub(crate) fn ZSTD_MLcode(mlBase: u32) -> u32 {
    static ML_Code: [u8; 128] = [
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
        25, 26, 27, 28, 29, 30, 31, 32, 32, 33, 33, 34, 34, 35, 35, 36, 36, 36, 36, 37, 37, 37, 37,
        38, 38, 38, 38, 38, 38, 38, 38, 39, 39, 39, 39, 39, 39, 39, 39, 40, 40, 40, 40, 40, 40, 40,
        40, 40, 40, 40, 40, 40, 40, 40, 40, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41, 41,
        41, 41, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42,
        42, 42, 42, 42, 42, 42, 42, 42, 42, 42, 42,
    ];
    static ML_deltaCode: u32 = 36;
    if mlBase > 127 {
        ZSTD_highbit32(mlBase).wrapping_add(ML_deltaCode)
    } else {
        ML_Code[mlBase as usize] as core::ffi::c_uint
    }
}

/// Minimum compression required to generate a compress block or a compressed
/// literals section. note: use same formula for both situations
#[inline]
pub(crate) fn ZSTD_minGain(srcSize: usize, strat: ZSTD_strategy) -> usize {
    let minlog = if strat >= ZSTD_btultra {
        strat.wrapping_sub(1)
    } else {
        6
    };
    (srcSize >> minlog).wrapping_add(2)
}

impl ZSTD_window_t {
    #[inline]
    pub(crate) fn init(&mut self) {
        self.base = c" ".as_ptr() as *const u8;
        self.dictBase = c" ".as_ptr() as *const u8;
        self.dictLimit = ZSTD_WINDOW_START_INDEX as u32; // start from >0, so that 1st position is valid
        self.lowLimit = ZSTD_WINDOW_START_INDEX as u32; // it ensures first and later CCtx usages compress the same
        self.nextSrc = self.base.wrapping_offset(ZSTD_WINDOW_START_INDEX as isize);
        self.nbOverflowCorrections = 0;
    }

    /// Clears the window containing the history by simply setting it to empty.
    #[inline]
    pub(crate) fn clear(&mut self) {
        let end = self.nextSrc.wrapping_offset_from(self.base) as u32;

        self.lowLimit = end;
        self.dictLimit = end;
    }

    /// Reduces the indices to protect from index overflow.
    ///
    /// The least significant cycleLog bits of the indices must remain the same,
    /// which may be 0. Every index up to maxDist in the past must be valid.
    ///
    /// # Returns
    ///
    /// The correction made to the indices, which must be applied to every stored index.
    #[inline]
    pub(crate) fn correct_overflow(
        &mut self,
        cycleLog: u32,
        maxDist: u32,
        src: *const core::ffi::c_void,
    ) -> u32 {
        let cycleSize = 1u32 << cycleLog;
        let cycleMask = cycleSize.wrapping_sub(1);
        let curr = src.addr().wrapping_sub(self.base.addr()) as u32;
        let currentCycle = curr & cycleMask;
        // Ensure newCurrent - maxDist >= ZSTD_WINDOW_START_INDEX.
        let currentCycleCorrection = if currentCycle < ZSTD_WINDOW_START_INDEX as u32 {
            cycleSize.max(2)
        } else {
            0
        };
        let newCurrent = currentCycle
            .wrapping_add(currentCycleCorrection)
            .wrapping_add(maxDist.max(cycleSize));
        let correction = curr.wrapping_sub(newCurrent);

        if !ZSTD_WINDOW_OVERFLOW_CORRECT_FREQUENTLY {
            // Loose bound, should be around 1<<29 (see above)
            assert!(correction > 1 << 28);
        }

        self.base = self.base.wrapping_add(correction as usize);
        self.dictBase = self.dictBase.wrapping_add(correction as usize);
        if self.lowLimit < correction.wrapping_add(ZSTD_WINDOW_START_INDEX as u32) {
            self.lowLimit = ZSTD_WINDOW_START_INDEX as u32;
        } else {
            self.lowLimit = self.lowLimit.wrapping_sub(correction);
        }
        if self.dictLimit < correction.wrapping_add(ZSTD_WINDOW_START_INDEX as u32) {
            self.dictLimit = ZSTD_WINDOW_START_INDEX as u32;
        } else {
            self.dictLimit = self.dictLimit.wrapping_sub(correction);
        }

        self.nbOverflowCorrections = self.nbOverflowCorrections.wrapping_add(1);

        correction
    }
}

pub(crate) const ZSTD_SHORT_CACHE_TAG_BITS: core::ffi::c_int = 8;
pub(crate) const ZSTD_SHORT_CACHE_TAG_MASK: core::ffi::c_uint =
    (1u32 << ZSTD_SHORT_CACHE_TAG_BITS).wrapping_sub(1);

/// Helper function for ZSTD_fillHashTable and ZSTD_fillDoubleHashTable.
/// Unpacks hashAndTag into (hash, tag), then packs (index, tag) into hashTable[hash].
#[inline]
pub(crate) unsafe fn ZSTD_writeTaggedIndex(hashTable: *mut u32, hashAndTag: usize, index: u32) {
    let hash = hashAndTag >> ZSTD_SHORT_CACHE_TAG_BITS;
    let tag = (hashAndTag & ZSTD_SHORT_CACHE_TAG_MASK as usize) as u32;
    *hashTable.add(hash) = index << ZSTD_SHORT_CACHE_TAG_BITS | tag;
}

/// Helper function for short cache matchfinders.
/// Unpacks tag1 and tag2 from lower bits of packedTag1 and packedTag2, then checks if the tags match.
#[inline]
pub(crate) fn ZSTD_comparePackedTags(packedTag1: usize, packedTag2: usize) -> bool {
    let tag1 = (packedTag1 & ZSTD_SHORT_CACHE_TAG_MASK as usize) as u32;
    let tag2 = (packedTag2 & ZSTD_SHORT_CACHE_TAG_MASK as usize) as u32;
    tag1 == tag2
}

#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq, Default)]
pub enum CompressionStage {
    #[default]
    Created = 0,
    Init = 1,
    Ongoing = 2,
    Ending = 3,
}

#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq, Default)]
pub enum StreamStage {
    #[default]
    Init = 0,
    Load = 1,
    Flush = 2,
}

pub type ZSTD_prefixDict = ZSTD_prefixDict_s;

#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct ZSTD_prefixDict_s {
    pub dict: *const core::ffi::c_void,
    pub dictSize: usize,
    pub dictContentType: ZSTD_dictContentType_e,
}

#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct ZSTD_localDict {
    pub dictBuffer: *mut core::ffi::c_void,
    pub dict: *const core::ffi::c_void,
    pub dictSize: usize,
    pub dictContentType: ZSTD_dictContentType_e,
    pub cdict: *mut ZSTD_CDict,
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct ZSTD_hufCTables_t {
    pub CTable: CTable,
    pub repeatMode: HUF_repeat,
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct ZSTD_fseCTables_t {
    pub offcodeCTable: [FSE_CTable; 193],
    pub matchlengthCTable: [FSE_CTable; 363],
    pub litlengthCTable: [FSE_CTable; 329],
    pub offcode_repeatMode: FSE_repeat,
    pub matchlength_repeatMode: FSE_repeat,
    pub litlength_repeatMode: FSE_repeat,
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct ZSTD_entropyCTables_t {
    pub huf: ZSTD_hufCTables_t,
    pub fse: ZSTD_fseCTables_t,
}

#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq, Default)]
pub enum LongLengthType {
    #[default]
    None = 0,
    Literal = 1,
    Match = 2,
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct ZSTD_SequenceLength {
    pub litLength: u32,
    pub matchLength: u32,
}

#[inline]
pub(crate) fn ZSTD_getSequenceLength(seqStore: &SeqStore_t, seq: &SeqDef) -> ZSTD_SequenceLength {
    let mut seqLen = ZSTD_SequenceLength {
        litLength: u32::from(seq.litLength),
        matchLength: u32::from(seq.mlBase) + u32::from(MINMATCH),
    };

    let seqIdx = (seq as *const SeqDef)
        .addr()
        .wrapping_sub(seqStore.sequencesStart.addr())
        / size_of::<SeqDef>();
    if seqStore.longLengthPos as usize == seqIdx {
        if seqStore.longLengthType == LongLengthType::Literal {
            seqLen.litLength += 0x10000;
        }
        if seqStore.longLengthType == LongLengthType::Match {
            seqLen.matchLength += 0x10000;
        }
    }

    seqLen
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct ZSTD_match_t {
    /// Offset sumtype code for the match, using `ZSTD_storeSeq()` format
    pub off: u32,
    /// Raw length of match
    pub len: u32,
}

impl ZSTD_match_t {
    pub fn new(off: u32, len: u32) -> Self {
        Self { off, len }
    }
}

#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq, Default)]
pub enum OptPrice {
    #[default]
    Dynamic = 0,
    Predef = 1,
}

#[repr(C)]
#[derive(Default)]
pub struct optState_t {
    pub litFreq: *mut [core::ffi::c_uint; MaxLit as usize + 1],
    pub litLengthFreq: *mut [core::ffi::c_uint; MaxLL as usize + 1],
    pub matchLengthFreq: *mut [core::ffi::c_uint; MaxML as usize + 1],
    pub offCodeFreq: *mut [core::ffi::c_uint; MaxOff as usize + 1],
    pub matchTable: *mut [ZSTD_match_t; ZSTD_OPT_SIZE],
    pub priceTable: *mut [ZSTD_optimal_t; ZSTD_OPT_SIZE],
    pub litSum: u32,
    pub litLengthSum: u32,
    pub matchLengthSum: u32,
    pub offCodeSum: u32,
    pub litSumBasePrice: u32,
    pub litLengthSumBasePrice: u32,
    pub matchLengthSumBasePrice: u32,
    pub offCodeSumBasePrice: u32,
    pub priceType: OptPrice,
    pub symbolCosts: *const ZSTD_entropyCTables_t,
    pub literalCompressionMode: ParamSwitch,
}

/// The statistics tables are reserved in the workspace by `ZSTD_reset_matchState`, and so
/// are only valid when the strategy is at least `ZSTD_btopt`.
impl optState_t {
    pub(crate) unsafe fn litFreq(&self) -> &[core::ffi::c_uint; MaxLit as usize + 1] {
        &*self.litFreq
    }

    pub(crate) unsafe fn litFreq_mut(&mut self) -> &mut [core::ffi::c_uint; MaxLit as usize + 1] {
        &mut *self.litFreq
    }

    pub(crate) unsafe fn litLengthFreq(&self) -> &[core::ffi::c_uint; MaxLL as usize + 1] {
        &*self.litLengthFreq
    }

    pub(crate) unsafe fn litLengthFreq_mut(
        &mut self,
    ) -> &mut [core::ffi::c_uint; MaxLL as usize + 1] {
        &mut *self.litLengthFreq
    }

    pub(crate) unsafe fn matchLengthFreq(&self) -> &[core::ffi::c_uint; MaxML as usize + 1] {
        &*self.matchLengthFreq
    }

    pub(crate) unsafe fn matchLengthFreq_mut(
        &mut self,
    ) -> &mut [core::ffi::c_uint; MaxML as usize + 1] {
        &mut *self.matchLengthFreq
    }

    pub(crate) unsafe fn offCodeFreq(&self) -> &[core::ffi::c_uint; MaxOff as usize + 1] {
        &*self.offCodeFreq
    }

    pub(crate) unsafe fn offCodeFreq_mut(
        &mut self,
    ) -> &mut [core::ffi::c_uint; MaxOff as usize + 1] {
        &mut *self.offCodeFreq
    }
}

#[repr(C)]
#[derive(Default)]
pub struct ZSTD_blockState_t {
    pub prevCBlock: *mut ZSTD_compressedBlockState_t,
    pub nextCBlock: *mut ZSTD_compressedBlockState_t,
    pub matchState: ZSTD_MatchState_t,
}

#[repr(C)]
#[derive(Default)]
pub struct SeqCollector {
    pub collectSequences: core::ffi::c_int,
    pub seqStart: *mut ZSTD_Sequence,
    pub seqIndex: usize,
    pub maxSequences: usize,
}

/// Indicates whether this compression proceeds directly from user-provided
/// source buffer to user-provided destination buffer (`BufferedPolicy::NotBuffered`), or
/// whether the context needs to buffer the input/output (`BufferedPolicy::Buffered`).
#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq, Default)]
pub enum BufferedPolicy {
    #[default]
    NotBuffered = 0,
    Buffered = 1,
}

/// Struct that contains all elements of block splitter that should be allocated
/// in a wksp.
#[derive(Copy, Clone)]
#[repr(C)]
pub struct ZSTD_blockSplitCtx {
    pub fullSeqStoreChunk: SeqStore_t,
    pub firstHalfSeqStore: SeqStore_t,
    pub secondHalfSeqStore: SeqStore_t,
    pub currSeqStore: SeqStore_t,
    pub nextSeqStore: SeqStore_t,
    pub partitions: [u32; ZSTD_MAX_NB_BLOCK_SPLITS],
    pub entropyMetadata: ZSTD_entropyCTablesMetadata_t,
}

impl Default for ZSTD_blockSplitCtx {
    fn default() -> Self {
        Self {
            fullSeqStoreChunk: Default::default(),
            firstHalfSeqStore: Default::default(),
            secondHalfSeqStore: Default::default(),
            currSeqStore: Default::default(),
            nextSeqStore: Default::default(),
            partitions: [0; _],
            entropyMetadata: Default::default(),
        }
    }
}

#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq, Default)]
pub enum DictTableLoadMethod {
    #[default]
    Fast = 0,
    Full = 1,
}

#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq, Default)]
pub enum TableFillPurpose {
    #[default]
    ForCCtx = 0,
    ForCDict = 1,
}

#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq, Default, Debug)]
pub enum DictMode {
    #[default]
    NoDict = 0,
    ExtDict = 1,
    DictMatchState = 2,
    DedicatedDictSearch = 3,
}

/// A type-level [`DictMode`], a workaround for const generics not supporting enums yet.
pub trait DictModeMarker {
    const DICT_MODE: DictMode;
}

pub struct NoDict;
impl DictModeMarker for NoDict {
    const DICT_MODE: DictMode = DictMode::NoDict;
}

pub struct ExtDict;
impl DictModeMarker for ExtDict {
    const DICT_MODE: DictMode = DictMode::ExtDict;
}

pub struct DictMatchState;
impl DictModeMarker for DictMatchState {
    const DICT_MODE: DictMode = DictMode::DictMatchState;
}

pub struct DedicatedDictSearch;
impl DictModeMarker for DedicatedDictSearch {
    const DICT_MODE: DictMode = DictMode::DedicatedDictSearch;
}

#[repr(u32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub enum CParamMode {
    /// Compression with [`DictMode::NoDict`] or [`DictMode::ExtDict`].
    /// In this mode we use both the srcSize and the dictSize
    /// when selecting and adjusting parameters.
    #[default]
    NoAttachDict = 0,
    /// Compression with [`DictMode::DictMatchState`] or [`DictMode::DedicatedDictSearch`].
    /// In this mode we only take the srcSize into account when selecting
    /// and adjusting parameters.
    AttachDict = 1,
    /// Creating a CDict.
    /// In this mode we take both the source size and the dictionary size
    /// into account when selecting and adjusting the parameters.
    CreateCDict = 2,
    /// `ZSTD_getCParams`, `ZSTD_getParams`, `ZSTD_adjustParams`.
    /// We don't know what these parameters are for. We default to the legacy
    /// behavior of taking both the source size and the dict size into account
    /// when selecting and adjusting parameters.
    Unknown = 3,
}

impl From<i32> for CParamMode {
    fn from(value: i32) -> Self {
        match value {
            0 => Self::NoAttachDict,
            1 => Self::AttachDict,
            2 => Self::CreateCDict,
            _ => Self::Unknown,
        }
    }
}

pub type ZSTD_BlockCompressor_f = unsafe fn(
    &mut ZSTD_MatchState_t,
    &mut SeqStore_t,
    &mut RepCodes,
    *const core::ffi::c_void,
    usize,
) -> usize;

/// Writes uncompressed block to dst buffer from given src.
/// Returns the size of the block
#[inline]
pub unsafe fn ZSTD_noCompressBlock(
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
    lastBlock: bool,
) -> Result<usize, Error> {
    let cBlockHeader24 = u32::from(lastBlock)
        .wrapping_add((BlockType::Raw as u32) << 1)
        .wrapping_add((srcSize << 3) as u32);
    if srcSize.wrapping_add(ZSTD_BLOCKHEADERSIZE) > dstCapacity {
        return Err(Error::dstSize_tooSmall);
    }
    MEM_writeLE24(dst, cBlockHeader24);
    core::ptr::copy_nonoverlapping(
        src.cast::<u8>(),
        dst.byte_add(ZSTD_BLOCKHEADERSIZE).cast::<u8>(),
        srcSize,
    );
    Ok(ZSTD_BLOCKHEADERSIZE.wrapping_add(srcSize))
}

/// In 32-bit mode: we want to avoid crossing the 2 GB limit,
/// reducing risks of side effects in case of signed operations on indexes.
///
/// In 64-bit mode: we want to ensure that adding the maximum job size (512 MB)
/// doesn't overflow u32 index capacity (4 GB)
pub(crate) const ZSTD_CURRENT_MAX: usize = if MEM_64bits() {
    3500 * (1 << 20)
} else {
    2000 * (1 << 20)
};

/// Maximum chunk size before overflow correction needs to be called again
pub(crate) const ZSTD_CHUNKSIZE_MAX: usize = u32::MAX as usize - ZSTD_CURRENT_MAX;

pub const REPCODE1_TO_OFFBASE: u32 = 1;
pub const REPCODE3_TO_OFFBASE: u32 = 3;

#[inline(always)]
pub(crate) unsafe fn ZSTD_storeSeqOnly(
    seqStorePtr: &mut SeqStore_t,
    litLength: usize,
    offBase: u32,
    matchLength: usize,
) {
    if litLength > 0xffff {
        seqStorePtr.longLengthType = LongLengthType::Literal;
        seqStorePtr.longLengthPos =
            (seqStorePtr.sequences).offset_from_unsigned(seqStorePtr.sequencesStart) as u32;
    }
    (*seqStorePtr.sequences).litLength = litLength as u16;
    (*seqStorePtr.sequences).offBase = offBase;
    let mlBase = matchLength.wrapping_sub(usize::from(MINMATCH));
    if mlBase > 0xffff {
        seqStorePtr.longLengthType = LongLengthType::Match;
        seqStorePtr.longLengthPos =
            (seqStorePtr.sequences).offset_from_unsigned(seqStorePtr.sequencesStart) as u32;
    }
    (*seqStorePtr.sequences).mlBase = mlBase as u16;
    seqStorePtr.sequences = seqStorePtr.sequences.add(1);
}
#[inline(always)]
pub(crate) unsafe fn ZSTD_storeSeq(
    seqStorePtr: &mut SeqStore_t,
    litLength: usize,
    literals: *const u8,
    litLimit: *const u8,
    offBase: u32,
    matchLength: usize,
) {
    let litLimit_w = litLimit.wrapping_sub(WILDCOPY_OVERLENGTH);
    let litEnd = literals.add(litLength);
    if litEnd <= litLimit_w {
        ZSTD_copy16(seqStorePtr.lit, literals);
        if litLength > 16 {
            ZSTD_wildcopy(
                seqStorePtr.lit.add(16),
                literals.add(16),
                litLength.wrapping_sub(16),
                Overlap::NoOverlap,
            );
        }
    } else {
        ZSTD_safecopyLiterals(seqStorePtr.lit, literals, litEnd, litLimit_w);
    }
    seqStorePtr.lit = seqStorePtr.lit.add(litLength);
    ZSTD_storeSeqOnly(seqStorePtr, litLength, offBase, matchLength);
}

#[inline]
pub(crate) fn ZSTD_updateRep(rep: &mut RepCodes, offBase: u32, ll0: bool) {
    if offBase > ZSTD_REP_NUM {
        rep[2] = rep[1];
        rep[1] = rep[0];
        rep[0] = offBase.wrapping_sub(ZSTD_REP_NUM);
    } else {
        let repCode = offBase.wrapping_sub(1).wrapping_add(ll0 as u32);
        if repCode > 0 {
            let currentOffset = if repCode == ZSTD_REP_NUM {
                rep[0].wrapping_sub(1)
            } else {
                rep[repCode as usize]
            };
            rep[2] = if repCode >= 2 { rep[1] } else { rep[2] };
            rep[1] = rep[0];
            rep[0] = currentOffset;
        }
    }
}

pub(crate) unsafe fn ZSTD_safecopyLiterals(
    mut op: *mut u8,
    mut ip: *const u8,
    iend: *const u8,
    ilimit_w: *const u8,
) {
    if ip <= ilimit_w {
        ZSTD_wildcopy(
            op,
            ip,
            ilimit_w.offset_from_unsigned(ip),
            Overlap::NoOverlap,
        );
        op = op.add(ilimit_w.offset_from_unsigned(ip));
        ip = ilimit_w;
    }
    while ip < iend {
        *op = *ip;
        ip = ip.add(1);
        op = op.add(1);
    }
}

#[inline]
const fn ZSTD_NbCommonBytes(val: usize) -> u32 {
    if cfg!(target_endian = "little") {
        val.trailing_zeros() >> 3
    } else {
        val.leading_zeros() >> 3
    }
}

#[inline]
pub(crate) unsafe fn ZSTD_count(
    mut pIn: *const u8,
    mut pMatch: *const u8,
    pInLimit: *const u8,
) -> usize {
    let pStart = pIn;
    let pInLoopLimit = pInLimit.sub(size_of::<usize>().wrapping_sub(1) as usize);
    if pIn < pInLoopLimit {
        let diff = MEM_readST(pMatch as *const core::ffi::c_void)
            ^ MEM_readST(pIn as *const core::ffi::c_void);
        if diff != 0 {
            return ZSTD_NbCommonBytes(diff) as usize;
        }
        pIn = pIn.add(size_of::<usize>());
        pMatch = pMatch.add(size_of::<usize>());
        while pIn < pInLoopLimit {
            let diff_0 = MEM_readST(pMatch as *const core::ffi::c_void)
                ^ MEM_readST(pIn as *const core::ffi::c_void);
            if diff_0 == 0 {
                pIn = pIn.add(size_of::<usize>());
                pMatch = pMatch.add(size_of::<usize>());
            } else {
                pIn = pIn.offset(ZSTD_NbCommonBytes(diff_0) as isize);
                return pIn.offset_from_unsigned(pStart);
            }
        }
    }
    if MEM_64bits()
        && pIn < pInLimit.sub(3)
        && MEM_read32(pMatch as *const core::ffi::c_void)
            == MEM_read32(pIn as *const core::ffi::c_void)
    {
        pIn = pIn.add(4);
        pMatch = pMatch.add(4);
    }
    if pIn < pInLimit.sub(1)
        && MEM_read16(pMatch as *const core::ffi::c_void) as core::ffi::c_int
            == MEM_read16(pIn as *const core::ffi::c_void) as core::ffi::c_int
    {
        pIn = pIn.add(2);
        pMatch = pMatch.add(2);
    }
    if pIn < pInLimit && *pMatch as core::ffi::c_int == *pIn as core::ffi::c_int {
        pIn = pIn.add(1);
    }
    pIn.offset_from_unsigned(pStart)
}

/// The length of the common prefix of `input` and `match_`.
///
/// This version seems best for long matches, while [`ZSTD_count`] appears more optimal for short
/// matches that exit early.
#[inline]
pub(crate) fn ZSTD_count_safe(input: &[u8], match_: &[u8]) -> usize {
    const N: usize = size_of::<usize>();

    let len = Ord::min(input.len(), match_.len());
    let (inputChunks, inputRest) = input[..len].as_chunks::<N>();
    let (matchChunks, matchRest) = match_[..len].as_chunks::<N>();

    for (i, (a, b)) in inputChunks.iter().zip(matchChunks).enumerate() {
        let diff = usize::from_ne_bytes(*a) ^ usize::from_ne_bytes(*b);
        if diff != 0 {
            return i * N + ZSTD_NbCommonBytes(diff) as usize;
        }
    }

    let tail = inputRest
        .iter()
        .zip(matchRest)
        .take_while(|(a, b)| a == b)
        .count();

    inputChunks.len() * N + tail
}

/// Like [`ZSTD_count_safe`], but a match that runs to the end of `match_` continues in `prefix`.
#[inline]
pub(crate) fn ZSTD_count_2segments_safe(input: &[u8], match_: &[u8], prefix: &[u8]) -> usize {
    let matchLength = ZSTD_count_safe(input, match_);
    if matchLength != match_.len() {
        return matchLength;
    }
    matchLength + ZSTD_count_safe(&input[matchLength..], prefix)
}

#[inline]
pub(crate) unsafe fn ZSTD_count_2segments(
    ip: *const u8,
    match_0: *const u8,
    iEnd: *const u8,
    mEnd: *const u8,
    iStart: *const u8,
) -> usize {
    let vEnd = if ip.wrapping_add(mEnd.offset_from_unsigned(match_0)) < iEnd {
        ip.add(mEnd.offset_from_unsigned(match_0))
    } else {
        iEnd
    };
    let matchLength = ZSTD_count(ip, match_0, vEnd);
    if match_0.add(matchLength) != mEnd {
        return matchLength;
    }
    matchLength.wrapping_add(ZSTD_count(ip.add(matchLength), iStart, iEnd))
}

const prime3bytes: u32 = 506832829;
const prime4bytes: u32 = 2654435761;

/// Hash the first `MLS` bytes of the little-endian value `u`, salted with `s`.
const fn ZSTD_hash32<const MLS: u32>(u: u32, h: u32, s: u32) -> u32 {
    const { assert!(3 <= MLS && MLS <= 4) }

    let prime = match MLS {
        3 => prime3bytes,
        _ => prime4bytes,
    };

    (((u << (32 - 8 * MLS)).wrapping_mul(prime)) ^ s) >> 32u32.wrapping_sub(h)
}

#[inline]
pub(crate) unsafe fn ZSTD_hash32Ptr<const MLS: u32>(
    ptr: *const core::ffi::c_void,
    h: u32,
) -> usize {
    ZSTD_hash32::<MLS>(MEM_readLE32(ptr), h, 0) as usize
}
unsafe fn ZSTD_hash32PtrS<const MLS: u32>(ptr: *const core::ffi::c_void, h: u32, s: u32) -> usize {
    ZSTD_hash32::<MLS>(MEM_readLE32(ptr), h, s) as usize
}

const prime5bytes: u64 = 889523592379;
const prime6bytes: u64 = 227718039650203;
const prime7bytes: u64 = 58295818150454627;
pub(crate) const prime8bytes: u64 = 0xcf1bbcdcb7a56463;

/// Hash the first `MLS` bytes of the little-endian value `u`, salted with `s`.
const fn ZSTD_hash64<const MLS: u32>(u: u64, h: u32, s: u64) -> usize {
    const { assert!(5 <= MLS && MLS <= 8) }

    let prime = match MLS {
        5 => prime5bytes,
        6 => prime6bytes,
        7 => prime7bytes,
        _ => prime8bytes,
    };

    ((((u << (64 - 8 * MLS)).wrapping_mul(prime)) ^ s) >> 64u32.wrapping_sub(h)) as usize
}

unsafe fn ZSTD_hash64Ptr<const MLS: u32>(p: *const core::ffi::c_void, h: u32) -> usize {
    ZSTD_hash64::<MLS>(MEM_readLE64(p), h, 0)
}
unsafe fn ZSTD_hash64PtrS<const MLS: u32>(p: *const core::ffi::c_void, h: u32, s: u64) -> usize {
    ZSTD_hash64::<MLS>(MEM_readLE64(p), h, s)
}
pub(crate) fn ZSTD_hash64Ptr_array<const MLS: u32>(p: &[u8; 8], h: u32) -> usize {
    ZSTD_hash64::<MLS>(u64::from_le_bytes(*p), h, 0)
}

#[inline(always)]
pub(crate) unsafe fn ZSTD_hashPtr(p: *const core::ffi::c_void, hBits: u32, mls: u32) -> usize {
    match mls {
        5 => ZSTD_hash64Ptr::<5>(p, hBits),
        6 => ZSTD_hash64Ptr::<6>(p, hBits),
        7 => ZSTD_hash64Ptr::<7>(p, hBits),
        8 => ZSTD_hash64Ptr::<8>(p, hBits),
        _ => ZSTD_hash32Ptr::<4>(p, hBits),
    }
}

#[inline(always)]
pub(crate) unsafe fn ZSTD_hashPtrSalted(
    p: *const core::ffi::c_void,
    hBits: u32,
    mls: u32,
    hashSalt: u64,
) -> usize {
    match mls {
        5 => ZSTD_hash64PtrS::<5>(p, hBits, hashSalt),
        6 => ZSTD_hash64PtrS::<6>(p, hBits, hashSalt),
        7 => ZSTD_hash64PtrS::<7>(p, hBits, hashSalt),
        8 => ZSTD_hash64PtrS::<8>(p, hBits, hashSalt),
        _ => ZSTD_hash32PtrS::<4>(p, hBits, hashSalt as u32),
    }
}

impl ZSTD_MatchState_t {
    /// Returns the lowest allowed match index. It may either be in the ext-dict or the prefix.
    #[inline]
    pub(crate) fn get_lowest_match_index(&self, curr: u32, windowLog: core::ffi::c_uint) -> u32 {
        let maxDistance = 1 << windowLog;
        let lowestValid = self.window.lowLimit;
        let withinWindow = if curr.wrapping_sub(lowestValid) > maxDistance {
            curr.wrapping_sub(maxDistance)
        } else {
            lowestValid
        };
        let isDictionary = self.loadedDictEnd != 0;

        if isDictionary {
            lowestValid
        } else {
            withinWindow
        }
    }

    /// Returns the lowest allowed match index in the prefix.
    #[inline]
    pub(crate) fn get_lowest_prefix_index(&self, curr: u32, windowLog: core::ffi::c_uint) -> u32 {
        let maxDistance = 1 << windowLog;
        let lowestValid = self.window.dictLimit;
        let withinWindow = if curr.wrapping_sub(lowestValid) > maxDistance {
            curr.wrapping_sub(maxDistance)
        } else {
            lowestValid
        };
        let isDictionary = self.loadedDictEnd != 0;

        if isDictionary {
            lowestValid
        } else {
            withinWindow
        }
    }

    /// Inspects the `matchState` and figures out what `dictMode` should be passed to the compressor.
    #[inline]
    pub(crate) unsafe fn dict_mode(&self) -> DictMode {
        if self.window.has_ext_dict() {
            DictMode::ExtDict
        } else if !self.dictMatchState.is_null() {
            if (*self.dictMatchState).dedicatedDictSearch != 0 {
                DictMode::DedicatedDictSearch
            } else {
                DictMode::DictMatchState
            }
        } else {
            DictMode::NoDict
        }
    }
}

#[inline]
pub(crate) fn ZSTD_index_overlap_check(prefixLowestIndex: u32, repIndex: u32) -> bool {
    prefixLowestIndex.wrapping_sub(1).wrapping_sub(repIndex) >= 3
}

/// Setting this to true tells zstd to run the overflow correction code
/// much more frequently. This is very inefficient, and should only be
/// used for tests and fuzzers.
pub const ZSTD_WINDOW_OVERFLOW_CORRECT_FREQUENTLY: bool = false;
pub const ZSTD_WINDOW_START_INDEX: core::ffi::c_int = 2;

impl ZSTD_window_t {
    /// Returns true if the window has a non-empty extDict.
    #[inline]
    pub(crate) fn has_ext_dict(&self) -> bool {
        self.lowLimit < self.dictLimit
    }

    /// Updates the window by appending [src, src + srcSize) to the window.
    ///
    /// If it is not contiguous, the current prefix becomes the extDict, and we forget about the
    /// extDict. Handles overlap of the prefix and extDict.
    ///
    /// Returns `true` if the segment is contiguous.
    #[inline]
    pub(crate) fn update(
        &mut self,
        src: *const core::ffi::c_void,
        srcSize: usize,
        forceNonContiguous: bool,
    ) -> bool {
        let ip = src as *const u8;
        let mut contiguous = true;
        if srcSize == 0 {
            return contiguous;
        }

        // Check if blocks follow each other
        if src != self.nextSrc as *const core::ffi::c_void || forceNonContiguous {
            // not contiguous
            let distanceFromBase = self.nextSrc.wrapping_offset_from(self.base) as usize;
            self.lowLimit = self.dictLimit;
            self.dictLimit = distanceFromBase as u32;
            self.dictBase = self.base;
            self.base = ip.wrapping_sub(distanceFromBase);
            if self.dictLimit.wrapping_sub(self.lowLimit) < HASH_READ_SIZE as u32 {
                self.lowLimit = self.dictLimit;
            }
            contiguous = false;
        }
        self.nextSrc = ip.wrapping_add(srcSize);

        // if input and dictionary overlap: reduce dictionary (area presumed modified by input)
        if (ip.wrapping_add(srcSize) > self.dictBase.wrapping_offset(self.lowLimit as isize))
            && (ip < self.dictBase.wrapping_offset(self.dictLimit as isize))
        {
            let highInputIdx = ip
                .wrapping_add(srcSize)
                .addr()
                .wrapping_sub(self.dictBase.addr());
            let lowLimitMax = (highInputIdx as u32).min(self.dictLimit);
            self.lowLimit = lowLimitMax;
        }

        contiguous
    }

    /// Updates lowLimit so that:
    ///    (srcEnd - base) - lowLimit == maxDist + loadedDictEnd
    ///
    /// It ensures index is valid as long as index >= lowLimit.
    /// This must be called before a block compression call.
    ///
    /// loadedDictEnd is only defined if a dictionary is in use for current compression.
    /// As the name implies, loadedDictEnd represents the index at end of dictionary.
    /// The value lies within context's referential, it can be directly compared to blockEndIdx.
    ///
    /// If loadedDictEndPtr is NULL, no dictionary is in use, and we use loadedDictEnd == 0.
    /// If loadedDictEndPtr is not NULL, we set it to zero after updating lowLimit.
    /// This is because dictionaries are allowed to be referenced fully
    /// as long as the last byte of the dictionary is in the window.
    /// Once input has progressed beyond window size, dictionary cannot be referenced anymore.
    ///
    /// In normal dict mode, the dictionary lies between lowLimit and dictLimit.
    /// In dictMatchState mode, lowLimit and dictLimit are the same,
    /// and the dictionary is below them.
    /// forceWindow and dictMatchState are therefore incompatible.
    #[inline]
    pub(crate) fn enforce_max_dist(
        &mut self,
        blockEnd: *const core::ffi::c_void,
        maxDist: u32,
        loadedDictEndPtr: &mut u32,
        dictMatchStatePtr: Option<&mut *const ZSTD_MatchState_t>,
    ) {
        let blockEndIdx =
            (blockEnd as *const u8).wrapping_offset_from(self.base) as core::ffi::c_long as u32;
        let loadedDictEnd = *loadedDictEndPtr;

        // - When there is no dictionary: loadedDictEnd == 0.
        //   In which case, the test (blockEndIdx > maxDist) is merely to avoid
        //   overflowing next operation `newLowLimit = blockEndIdx - maxDist`.
        // - When there is a standard dictionary:
        //   Index referential is copied from the dictionary,
        //   which means it starts from 0.
        //   In which case, loadedDictEnd == dictSize,
        //   and it makes sense to compare `blockEndIdx > maxDist + dictSize`
        //   since `blockEndIdx` also starts from zero.
        // - When there is an attached dictionary:
        //   loadedDictEnd is expressed within the referential of the context,
        //   so it can be directly compared against blockEndIdx.
        if blockEndIdx > maxDist.wrapping_add(loadedDictEnd) {
            let newLowLimit = blockEndIdx.wrapping_sub(maxDist);
            self.lowLimit = Ord::max(self.lowLimit, newLowLimit);
            self.dictLimit = Ord::max(self.dictLimit, self.lowLimit);
            // On reaching window size, dictionaries are invalidated
            *loadedDictEndPtr = 0;
            if let Some(dictMatchStatePtr) = dictMatchStatePtr {
                *dictMatchStatePtr = core::ptr::null();
            }
        }
    }

    /// Returns true if the indices are getting too large and overflow protection is needed.
    #[inline]
    pub(crate) fn needs_overflow_correction(
        &self,
        cycleLog: u32,
        maxDist: u32,
        loadedDictEnd: u32,
        src: *const core::ffi::c_void,
        srcEnd: *const core::ffi::c_void,
    ) -> bool {
        if ZSTD_WINDOW_OVERFLOW_CORRECT_FREQUENTLY
            && self.can_overflow_correct(cycleLog, maxDist, loadedDictEnd, src)
        {
            return true;
        }

        let curr = srcEnd.addr() - self.base.addr();
        curr > ZSTD_CURRENT_MAX
    }

    /// Returns true if the indices are large enough for overflow correction
    /// to work correctly without impacting compression ratio.
    #[inline]
    fn can_overflow_correct(
        &self,
        cycleLog: u32,
        maxDist: u32,
        loadedDictEnd: u32,
        src: *const core::ffi::c_void,
    ) -> bool {
        let cycleSize = 1u32 << cycleLog;
        let curr = src.addr().wrapping_sub(self.base.addr()) as u32;
        let minIndexToOverflowCorrect = cycleSize
            .wrapping_add(maxDist.max(cycleSize))
            .wrapping_add(ZSTD_WINDOW_START_INDEX as u32);
        let adjustment = self.nbOverflowCorrections.wrapping_add(1);
        let adjustedIndex = minIndexToOverflowCorrect
            .wrapping_mul(adjustment)
            .max(minIndexToOverflowCorrect);
        let indexLargeEnough = curr > adjustedIndex;
        let dictionaryInvalidated = curr > maxDist.wrapping_add(loadedDictEnd);
        indexLargeEnough && dictionaryInvalidated
    }
}
