use crate::lib::polyfill::{unlikely, PointerExt};

#[derive(Copy, Clone)]
#[repr(C)]
pub struct LdmState {
    pub window: ZSTD_window_t,
    pub hashTable: *mut LdmEntry,
    pub loadedDictEnd: u32,
    pub bucketOffsets: *mut u8,
    pub splitIndices: [usize; LDM_BATCH_SIZE],
    pub matchCandidates: [LdmMatchCandidate; LDM_BATCH_SIZE],
}

impl Default for LdmState {
    fn default() -> Self {
        Self {
            window: ZSTD_window_t::default(),
            hashTable: core::ptr::null_mut(),
            loadedDictEnd: 0,
            bucketOffsets: core::ptr::null_mut(),
            splitIndices: [0; LDM_BATCH_SIZE],
            matchCandidates: [LdmMatchCandidate::default(); LDM_BATCH_SIZE],
        }
    }
}

#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct LdmMatchCandidate {
    pub split: usize,
    pub hash: u32,
    pub checksum: u32,
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct LdmEntry {
    pub offset: u32,
    pub checksum: u32,
}

#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct LdmParams {
    pub enableLdm: ParamSwitch,
    pub hashLog: u32,
    pub bucketSizeLog: u32,
    pub minMatchLength: u32,
    pub hashRateLog: u32,
    pub windowLog: u32,
}

#[repr(C)]
pub struct LdmRollingHashState {
    pub rolling: u64,
    pub stopMask: u64,
}

use crate::lib::common::error_private::Error;
use crate::lib::common::xxhash::ZSTD_XXH64_slice;
use crate::lib::common::zstd_internal::{RepCodes, ZSTD_REP_NUM};
use crate::lib::compress::zstd_compress::{
    rawSeq, RawSeqStore_t, SeqStore_t, ZSTD_MatchState_t, ZSTD_cwksp_alloc_size,
    ZSTD_selectBlockCompressor, ZSTD_window_t,
};
use crate::lib::compress::zstd_compress_internal::{
    DictTableLoadMethod, TableFillPurpose, ZSTD_count_2segments_safe, ZSTD_count_safe,
    ZSTD_storeSeq, HASH_READ_SIZE,
};
use crate::lib::compress::zstd_double_fast::ZSTD_fillDoubleHashTable;
use crate::lib::compress::zstd_fast::ZSTD_fillHashTable;
use crate::lib::zstd::{
    ParamSwitch, ZSTD_btopt, ZSTD_btultra, ZSTD_compressionParameters, ZSTD_dfast, ZSTD_fast,
    ZSTD_HASHLOG_MAX, ZSTD_HASHLOG_MIN,
};

pub const LDM_BATCH_SIZE: usize = 64;

#[rustfmt::skip]
static ZSTD_ldm_gearTab: [u64; 256] = [
    0xf5b8f72c5f77775c, 0x84935f266b7ac412, 0xb647ada9ca730ccc, 0xb065bb4b114fb1de,
    0x34584e7e8c3a9fd0, 0x4e97e17c6ae26b05, 0x3a03d743bc99a604, 0xcecd042422c4044f,
    0x76de76c58524259e, 0x9c8528f65badeaca, 0x86563706e2097529, 0x2902475fa375d889,
    0xafb32a9739a5ebe6, 0xce2714da3883e639, 0x021eaf821722e69e, 0x00037b628620b628,
    0x049a8d455d88caf5, 0x8556d711e6958140, 0x04f7ae74fc605c1f, 0x829f0c3468bd3a20,
    0x4ffdc885c625179e, 0x8473de048a3daf1b, 0x51008822b05646b2, 0x69d75d12b2d1cc5f,
    0x8c9d4a19159154bc, 0xc3cc10f4abbd4003, 0xd06ddc1cecb97391, 0xbe48e6e7ed80302e,
    0x3481db31cee03547, 0xacc3f67cdaa1d210, 0x65cb771d8c7f96cc, 0x8eb27177055723dd,
    0xc789950d44cd94be, 0x934feadc3700b12b, 0x5e485f11edbdf182, 0x1e2e2a46fd64767a,
    0x2969ca71d82efa7c, 0x9d46e9935ebbba2e, 0xe056b67e05e6822b, 0x94d73f55739d03a0,
    0xcd7010bdb69b5a03, 0x455ef9fcd79b82f4, 0x869cb54a8749c161, 0x38d1a4fa6185d225,
    0xb475166f94bbe9bb, 0xa4143548720959f1, 0x7aed4780ba6b26ba, 0xd0ce264439e02312,
    0x84366d746078d508, 0xa8ce973c72ed17be, 0x21c323a29a430b01, 0x9962d617e3af80ee,
    0xab0ce91d9c8cf75b, 0x530e8ee6d19a4dbc, 0x2ef68c0cf53f5d72, 0xc03a681640a85506,
    0x496e4e9f9c310967, 0x78580472b59b14a0, 0x273824c23b388577, 0x66bf923ad45cb553,
    0x47ae1a5a2492ba86, 0x35e304569e229659, 0x4765182a46870b6f, 0x6cbab625e9099412,
    0xddac9a2e598522c1, 0x7172086e666624f2, 0xdf5003ca503b7837, 0x88c0c1db78563d09,
    0x58d51865acfc289d, 0x177671aec65224f1, 0xfb79d8a241e967d7, 0x2be1e101cad9a49a,
    0x6625682f6e29186b, 0x399553457ac06e50, 0x035dffb4c23abb74, 0x429db2591f54aade,
    0xc52802a8037d1009, 0x6acb27381f0b25f3, 0xf45e2551ee4f823b, 0x8b0ea2d99580c2f7,
    0x3bed519cbcb4e1e1, 0x0ff452823dbb010a, 0x9d42ed614f3dd267, 0x5b9313c06257c57b,
    0xa114b8008b5e1442, 0xc1fe311c11c13d4b, 0x66e8763ea34c5568, 0x8b982af1c262f05d,
    0xee8876faaa75fbb7, 0x8a62a4d0d172bb2a, 0xc13d94a3b7449a97, 0x6dbbba9dc15d037c,
    0xc786101f1d92e0f1, 0xd78681a907a0b79b, 0xf61aaf2962c9abb9, 0x2cfd16fcd3cb7ad9,
    0x868c5b6744624d21, 0x25e650899c74ddd7, 0xba042af4a7c37463, 0x4eb1a539465a3eca,
    0xbe09dbf03b05d5ca, 0x774e5a362b5472ba, 0x47a1221229d183cd, 0x504b0ca18ef5a2df,
    0xdffbdfbde2456eb9, 0x46cd2b2fbee34634, 0xf2aef8fe819d98c3, 0x357f5276d4599d61,
    0x24a5483879c453e3, 0x088026889192b4b9, 0x28da96671782dbec, 0x4ef37c40588e9aaa,
    0x8837b90651bc9fb3, 0xc164f741d3f0e5d6, 0xbc135a0a704b70ba, 0x069cd868f7622ada,
    0xbc37ba89e0b9c0ab, 0x47c14a01323552f6, 0x4f00794bacee98bb, 0x7107de7d637a69d5,
    0x88af793bb6f2255e, 0xf3c6466b8799b598, 0xc288c616aa7f3b59, 0x81ca63cf42fca3fd,
    0x88d85ace36a2674b, 0x0d056bd3792389e7, 0xe55c396c4e9dd32d, 0xbefb504571e6c0a6,
    0x96ab32115e91e8cc, 0xbf8acb18de8f38d1, 0x66dae58801672606, 0x833b6017872317fb,
    0xb87c16f2d1c92864, 0xdb766a74e58b669c, 0x89659f85c61417be, 0xc8daad856011ea0c,
    0x76a4b565b6fe7eae, 0xa469d085f6237312, 0xaaf0365683a3e96c, 0x4dbb746f8424f7b8,
    0x0638755af4e4acc1, 0x3d7807f5bde64486, 0x17be6d8f5bbb7639, 0x0903f0cd44dc35dc,
    0x67b672eafdf1196c, 0xa676ff93ed4c82f1, 0x521d1004c5053d9d, 0x37ba9ad09ccc9202,
    0x84e54d297aacfb51, 0x0a0b4b776a143445, 0x0820d471e20b348e, 0x1874383cb83d46dc,
    0x97edeec7a1efe11c, 0xb330e50b1bdc42aa, 0x1dd91955ce70e032, 0xa514cdb88f2939d5,
    0x2791233fd90db9d3, 0x7b670a4cc50f7a9b, 0x77c07d2a05c6dfa5, 0xe3778b6646d0a6fa,
    0xb39c8eda47b56749, 0x933ed448addbef28, 0xaf846af6ab7d0bf4, 0x0e5af208eb666e49,
    0x5e6622f73534cd6a, 0x297daeca42ef5b6e, 0x862daef3d35539a6, 0xe68722498f8e1ea9,
    0x981c53093dc0d572, 0xfa09b0bfbf86fbf5, 0x30b1e96166219f15, 0x70e7d466bdc4fb83,
    0x5a66736e35f2a8e9, 0xcddb59d2b7c1baef, 0xd6c7d247d26d8996, 0xea4e39eac8de1ba3,
    0x539c8bb19fa3aff2, 0x09f90e4c5fd508d8, 0xa34e5956fbaf3385, 0x2e2f8e151d3ef375,
    0x173691e9b83faec1, 0xb85a8d56bf016379, 0x8382381267408ae3, 0xb90f901bbdc0096d,
    0x7c6ad32933bcec65, 0x76bb5e2f2c8ad595, 0x390f851a6cf46d28, 0xc3e6064da1c2da72,
    0xc52a0c101cfa5389, 0xd78eaf84a3fbc530, 0x3781b9e2288b997e, 0x73c2f6dea83d05c4,
    0x04228e364c5b5ed7, 0x9d7a3edf0da43911, 0x8edcfeda24686756, 0x5e7667a7b7a9b3a1,
    0x4c4f389fa143791d, 0xb08bc1023da7cddc, 0x7ab4be3ae529b1cc, 0x754e6132dbe74ff9,
    0x71635442a839df45, 0x2f6fb1643fbe52de, 0x961e0a42cf7a8177, 0xf3b45d83d89ef2ea,
    0xee3de4cf4a6e3e9b, 0xcd6848542c3295e7, 0xe4cee1664c78662f, 0x9947548b474c68c4,
    0x25d73777a5ed8b0b, 0x00c915b1d636b7fc, 0x21c2ba75d9b0d2da, 0x5f6b5dcf608a64a1,
    0xdcf333255ff9570c, 0x633b922418ced4ee, 0xc136dde0b004b34a, 0x58cc83b05d4b2f5a,
    0x5eb424dda28e42d2, 0x62df47369739cd98, 0xb4e0b42485e4ce17, 0x16e1f0c1f9a8d1e7,
    0x8ec3916707560ebf, 0x62ba6e2df2cc9db3, 0xcbf9f4ff77d83a16, 0x78d9d7d07d2bbcc4,
    0xef554ce1e02c41f4, 0x8d7581127eccf94d, 0xa9b53336cb3c8a05, 0x38c42c0bf45c4f91,
    0x640893cdf4488863, 0x80ec34bc575ea568, 0x39f324f5b48eaa40, 0xe9d9ed1f8eff527f,
    0x9224fc058cc5a214, 0xbaba00b04cfe7741, 0x309a9f120fcf52af, 0xa558f3ec65626212,
    0x424bec8b7adabe2f, 0x41622513a6aea433, 0xb88da2d5324ca798, 0xd287733b245528a4,
    0x9a44697e6d68aec3, 0x7b1093be2f49bb28, 0x50bbec632e3d8aad, 0x6cd90723e1ea8283,
    0x897b9e7431b02bf3, 0x219efdcb338a7047, 0x3b0311f0a27c0656, 0xdb17bf91c0db96e7,
    0x8cd4fd6b4e85a5b2, 0xfab071054ba6409d, 0x40d6fe831fa9dfd9, 0xaf358debad7d791e,
    0xeb8d0e25a65e3e58, 0xbbcbd3df14e08580, 0x0cf751f27ecdab2b, 0x2b4da14f2613d8f4,
];

pub const LDM_MIN_MATCH_LENGTH: core::ffi::c_int = 64;

impl LdmRollingHashState {
    /// Initializes the rolling hash state such that it will honor the
    /// settings in params.
    fn new(params: &LdmParams) -> Self {
        let maxBitsInMask = params.minMatchLength.min(64);
        let hashRateLog = params.hashRateLog;

        // The choice of the splitting criterion is subject to two conditions:
        //   1. it has to trigger on average every 2^(hashRateLog) bytes;
        //   2. ideally, it has to depend on a window of minMatchLength bytes.
        //
        // In the gear hash algorithm, bit n depends on the last n bytes;
        // so in order to obtain a good quality splitting criterion it is
        // preferable to use bits with high weight.
        //
        // To match condition 1 we use a mask with hashRateLog bits set
        // and, because of the previous remark, we make sure these bits
        // have the highest possible weight while still respecting
        // condition 2.
        let stopMask = if hashRateLog > 0 && hashRateLog <= maxBitsInMask {
            (1u64 << hashRateLog).wrapping_sub(1) << maxBitsInMask.wrapping_sub(hashRateLog)
        } else {
            // In this degenerate case we simply honor the hash rate.
            (1u64 << hashRateLog).wrapping_sub(1)
        };

        LdmRollingHashState {
            rolling: u64::from(u32::MAX),
            stopMask,
        }
    }

    /// Feeds `data` into the hash without registering any splits. This effectively
    /// resets the hash state. This is used when skipping over data, either at the
    /// beginning of a block, or skipping sections.
    fn reset(&mut self, data: &[u8]) {
        let mut hash = self.rolling;
        let (chunks, rest) = data.as_chunks::<4>();

        for chunk in chunks {
            for &byte in chunk {
                hash = (hash << 1).wrapping_add(ZSTD_ldm_gearTab[usize::from(byte)]);
            }
        }

        for &byte in rest {
            hash = (hash << 1).wrapping_add(ZSTD_ldm_gearTab[usize::from(byte)]);
        }
    }

    /// Registers in the splits array all the split points found in `data`.
    /// This function terminates when either all the data has been processed or
    /// [`LDM_BATCH_SIZE`] splits are present in the splits array.
    ///
    /// Precondition: The splits array must not be full.
    ///
    /// # Returns
    ///
    /// The number of bytes processed.
    fn feed(
        &mut self,
        data: &[u8],
        splits: &mut [usize; LDM_BATCH_SIZE],
        numSplits: &mut usize,
    ) -> usize {
        let mut hash = self.rolling;
        let mask = self.stopMask;
        let mut n = 0usize;

        let (chunks, rest) = data.as_chunks::<4>();

        'done: {
            macro_rules! gear_iter_once {
                ($byte:expr) => {
                    hash = (hash << 1).wrapping_add(ZSTD_ldm_gearTab[usize::from($byte)]);
                    n += 1;
                    if unlikely(hash & mask == 0) {
                        splits[*numSplits] = n;
                        *numSplits += 1;
                        if *numSplits == LDM_BATCH_SIZE {
                            break 'done;
                        }
                    }
                };
            }

            for &[b0, b1, b2, b3] in chunks {
                gear_iter_once!(b0);
                gear_iter_once!(b1);
                gear_iter_once!(b2);
                gear_iter_once!(b3);
            }
            for &byte in rest {
                gear_iter_once!(byte);
            }
        }

        self.rolling = hash;

        n
    }
}

impl LdmParams {
    /// If the `hashRateLog` is not set, set it to its default value based on
    /// `windowLog` and `hashLog`.
    ///
    /// Ensures that `bucketSizeLog <= hashLog` (setting it to `hashLog` if it is not).
    ///
    /// Ensures that the `minMatchLength >= targetLength` during optimal parsing.
    pub fn adjust_parameters(&mut self, cParams: &ZSTD_compressionParameters) {
        self.windowLog = cParams.windowLog;

        if self.hashRateLog == 0 {
            if self.hashLog > 0 {
                // if params->hashLog is set, derive hashRateLog from it
                if self.windowLog > self.hashLog {
                    self.hashRateLog = self.windowLog.wrapping_sub(self.hashLog);
                }
            } else {
                // mapping from [fast, rate7] to [btultra2, rate4]
                self.hashRateLog =
                    7u32.wrapping_sub((cParams.strategy as core::ffi::c_uint).wrapping_div(3));
            }
        }
        if self.hashLog == 0 {
            self.hashLog = self
                .windowLog
                .wrapping_sub(self.hashRateLog)
                .clamp(ZSTD_HASHLOG_MIN as u32, ZSTD_HASHLOG_MAX as u32);
        }
        if self.minMatchLength == 0 {
            self.minMatchLength = LDM_MIN_MATCH_LENGTH as u32;
            if cParams.strategy >= ZSTD_btultra {
                self.minMatchLength /= 2;
            }
        }
        if self.bucketSizeLog == 0 {
            self.bucketSizeLog = cParams.strategy.clamp(4, 8);
        }
        self.bucketSizeLog = self.bucketSizeLog.min(self.hashLog);
    }

    /// Estimate the space needed for long distance matching tables or 0 if LDM is disabled.
    pub fn get_table_size(self) -> usize {
        let ldmHSize = 1usize << self.hashLog;
        let ldmBucketSizeLog = self.bucketSizeLog.min(self.hashLog) as usize;
        let ldmBucketSize = 1 << (self.hashLog as usize).wrapping_sub(ldmBucketSizeLog);
        let totalSize = ZSTD_cwksp_alloc_size(ldmBucketSize).wrapping_add(ZSTD_cwksp_alloc_size(
            ldmHSize.wrapping_mul(size_of::<LdmEntry>()),
        ));
        if self.enableLdm == ParamSwitch::Enable {
            totalSize
        } else {
            0
        }
    }

    /// Return an upper bound on the number of sequences that can be produced by
    /// the long distance matcher, or 0 if LDM is disabled.
    pub fn get_max_nb_seq(self, maxChunkSize: usize) -> usize {
        if self.enableLdm == ParamSwitch::Enable {
            maxChunkSize / self.minMatchLength as usize
        } else {
            0
        }
    }
}

/// Returns the number of bytes that match backwards before the end of `input` and `beforeMatch`.
fn ZSTD_ldm_countBackwardsMatch(input: &[u8], beforeMatch: &[u8]) -> usize {
    let n = Ord::min(input.len(), beforeMatch.len());
    let input = &input[input.len() - n..];
    let beforeMatch = &beforeMatch[beforeMatch.len() - n..];
    let mut matchLength = 0;
    while matchLength < n && input[n - 1 - matchLength] == beforeMatch[n - 1 - matchLength] {
        matchLength += 1;
    }
    matchLength
}

/// Returns the number of bytes that match backwards from the end of `beforeMatch`,
/// even with the backwards match spanning 2 different segments.
///
/// On reaching the start of `beforeMatch`, start counting from the end of `extDict`
fn ZSTD_ldm_countBackwardsMatch_2segments(
    input: &[u8],
    beforeMatch: &[u8],
    extDict: &[u8],
) -> usize {
    let mut matchLength = ZSTD_ldm_countBackwardsMatch(input, beforeMatch);
    if matchLength != beforeMatch.len() || beforeMatch.as_ptr() == extDict.as_ptr() {
        // If backwards match is entirely in the extDict or prefix, immediately return
        return matchLength;
    }

    matchLength = matchLength.wrapping_add(ZSTD_ldm_countBackwardsMatch(
        &input[..input.len() - matchLength],
        extDict,
    ));
    matchLength
}

/// Fills the relevant tables for the ZSTD_fast and ZSTD_dfast strategies.
/// This is similar to ZSTD_loadDictionaryContent.
///
/// The tables for the other strategies are filled within their
/// block compressors.
unsafe fn ZSTD_ldm_fillFastTables(ms: &mut ZSTD_MatchState_t, end: *const core::ffi::c_void) {
    match ms.cParams.strategy {
        ZSTD_fast => {
            ZSTD_fillHashTable(
                ms,
                end,
                DictTableLoadMethod::Fast,
                TableFillPurpose::ForCCtx,
            );
        }
        ZSTD_dfast => {
            ZSTD_fillDoubleHashTable(
                ms,
                end,
                DictTableLoadMethod::Fast,
                TableFillPurpose::ForCCtx,
            );
        }
        _ => {}
    }
}

struct LdmHashTable<'a> {
    hash_table: &'a mut [LdmEntry],
    bucket_offsets: &'a mut [u8],
    bucket_size_log: u32,
}

impl LdmHashTable<'_> {
    /// Get the bucket associated with hash
    fn get_bucket(&mut self, hash: usize) -> &mut [LdmEntry] {
        &mut self.hash_table[hash << self.bucket_size_log..][..1 << self.bucket_size_log]
    }

    /// Insert the entry with corresponding hash into the hash table
    fn insert_entry(&mut self, hash: usize, entry: LdmEntry) {
        let offset = self.bucket_offsets[hash];

        self.get_bucket(hash)[usize::from(offset)] = entry;
        self.bucket_offsets[hash] =
            offset.wrapping_add(1) & ((1u32 << self.bucket_size_log) - 1) as u8;
    }

    /// Reduce table indexes by `reducerValue`
    fn reduce_table(&mut self, reducerValue: u32) {
        for entry in &mut *self.hash_table {
            entry.offset = entry.offset.saturating_sub(reducerValue);
        }
    }
}

impl LdmState {
    /// # Safety
    ///
    /// - `self.hashTable` must be valid for `1 << params.hashLog` entries.
    /// - `self.bucketOffsets` must be valid for `1 << (params.hashLog - params.bucketSizeLog)` entries.
    /// - No other reference to the hash table or bucket offsets may be live for the duration of `'a`.
    unsafe fn get_hash_table<'a>(&self, params: &LdmParams) -> LdmHashTable<'a> {
        let hBits = params.hashLog - params.bucketSizeLog;
        LdmHashTable {
            hash_table: core::slice::from_raw_parts_mut(self.hashTable, 1 << params.hashLog),
            bucket_offsets: core::slice::from_raw_parts_mut(self.bucketOffsets, 1 << hBits),
            bucket_size_log: params.bucketSizeLog,
        }
    }

    pub fn fill_hash_table(&mut self, src: &[u8], params: &LdmParams) {
        let minMatchLength = params.minMatchLength as usize;
        let hBits = params.hashLog - params.bucketSizeLog;
        let base = self.window.base;
        let mut hashTable = unsafe { self.get_hash_table(params) };

        let mut hashState = LdmRollingHashState::new(params);

        let mut pos = 0;
        while pos < src.len() {
            let mut numSplits = 0;
            let hashed = hashState.feed(&src[pos..], &mut self.splitIndices, &mut numSplits);

            for &splitIndex in &self.splitIndices[..numSplits] {
                let splitEnd = pos + splitIndex;
                if splitEnd >= minMatchLength {
                    let split = &src[splitEnd - minMatchLength..splitEnd];
                    let xxhash = ZSTD_XXH64_slice(split, 0);
                    let hash = (xxhash & (1u32 << hBits).wrapping_sub(1) as u64) as u32;
                    let entry = LdmEntry {
                        offset: split.as_ptr().wrapping_offset_from(base) as u32,
                        checksum: (xxhash >> 32) as u32,
                    };

                    hashTable.insert_entry(hash as usize, entry);
                }
            }

            pos += hashed;
        }
    }
}

/// Sets `ms.nextToUpdate` to a position corresponding closer to anchor
/// if it is far way
/// (after a long match, only update tables a limited amount).
unsafe fn ZSTD_ldm_limitTableUpdate(ms: &mut ZSTD_MatchState_t, anchor: *const u8) {
    let curr = anchor.wrapping_offset_from(ms.window.base) as u32;
    if curr > ms.nextToUpdate + 1024 {
        ms.nextToUpdate = curr - Ord::min(512, curr - ms.nextToUpdate - 1024);
    }
}

unsafe fn ZSTD_ldm_generateSequences_internal(
    ldmState: &mut LdmState,
    rawSeqStore: &mut RawSeqStore_t,
    params: &LdmParams,
    src: &[u8],
) -> Result<usize, Error> {
    // LDM parameters
    let extDict = ldmState.window.has_ext_dict();
    let minMatchLength = params.minMatchLength as usize;
    let hBits = params.hashLog - params.bucketSizeLog;
    let mut hashTable = ldmState.get_hash_table(params);

    // Prefix and extDict parameters
    let dictLimit = ldmState.window.dictLimit;
    let lowestIndex = if extDict {
        ldmState.window.lowLimit
    } else {
        dictLimit
    };
    let base = ldmState.window.base;
    let lowPrefixPtr = base.wrapping_offset(dictLimit as isize);
    let extDictSlice: &[u8] = if extDict {
        let dictStart = ldmState
            .window
            .dictBase
            .wrapping_offset(lowestIndex as isize);
        core::slice::from_raw_parts(dictStart, (dictLimit - lowestIndex) as usize)
    } else {
        &[]
    };

    // Input bounds
    let srcIndex = src.as_ptr().addr().wrapping_sub(base.addr());
    let ilimit = src.len().saturating_sub(HASH_READ_SIZE as usize);
    let prefix =
        core::slice::from_raw_parts(lowPrefixPtr, srcIndex + src.len() - dictLimit as usize);

    // Initialize the rolling hash state with the first minMatchLength bytes
    let Some(head) = src.get(..minMatchLength) else {
        return Ok(src.len());
    };
    let mut hashState = LdmRollingHashState::new(params);
    hashState.reset(head);

    // Input positions
    let mut anchor = 0;
    let mut ip = head.len();

    while ip < ilimit {
        let mut numSplits = 0;
        let hashed = hashState.feed(&src[ip..ilimit], &mut ldmState.splitIndices, &mut numSplits);

        for n in 0..numSplits {
            let splitEnd = ip + ldmState.splitIndices[n];
            let split = splitEnd - minMatchLength;
            let xxhash = ZSTD_XXH64_slice(&src[split..splitEnd], 0);
            let hash = (xxhash & (1u32 << hBits).wrapping_sub(1) as u64) as u32;
            ldmState.matchCandidates[n] = LdmMatchCandidate {
                split,
                hash,
                checksum: (xxhash >> 32) as u32,
            };
        }

        for &LdmMatchCandidate {
            split,
            checksum,
            hash,
        } in &ldmState.matchCandidates[..numSplits]
        {
            let mut forwardMatchLength = 0;
            let mut backwardMatchLength = 0;
            let mut bestMatchLength = 0;

            let bucket = hashTable.get_bucket(hash as usize);

            let mut bestEntry = None;
            let newEntry = LdmEntry {
                offset: (srcIndex + split) as u32,
                checksum,
            };

            // If a split point would generate a sequence overlapping with
            // the previous one, we merely register it in the hash table and
            // move on
            if split < anchor {
                hashTable.insert_entry(hash as usize, newEntry);
                continue;
            }

            let (beforeSplit, afterSplit) = src.split_at(split);
            let beforeSplit = &beforeSplit[anchor..];
            for cur in bucket {
                if cur.checksum != checksum || cur.offset <= lowestIndex {
                    continue;
                }

                let (curForwardMatchLength, curBackwardMatchLength) = if extDict {
                    let (beforeMatch, matchSegment) = if cur.offset < dictLimit {
                        extDictSlice.split_at((cur.offset - lowestIndex) as usize)
                    } else {
                        prefix.split_at((cur.offset - dictLimit) as usize)
                    };

                    let forward = ZSTD_count_2segments_safe(afterSplit, matchSegment, prefix);
                    if forward < minMatchLength {
                        continue;
                    }

                    let backward = ZSTD_ldm_countBackwardsMatch_2segments(
                        beforeSplit,
                        beforeMatch,
                        extDictSlice,
                    );

                    (forward, backward)
                } else {
                    let (beforeMatch, matchSegment) =
                        prefix.split_at((cur.offset - dictLimit) as usize);

                    let forward = ZSTD_count_safe(afterSplit, matchSegment);
                    if forward < minMatchLength {
                        continue;
                    }

                    let backward = ZSTD_ldm_countBackwardsMatch(beforeSplit, beforeMatch);

                    (forward, backward)
                };

                let curTotalMatchLength =
                    curForwardMatchLength.wrapping_add(curBackwardMatchLength);
                if curTotalMatchLength > bestMatchLength {
                    bestMatchLength = curTotalMatchLength;
                    forwardMatchLength = curForwardMatchLength;
                    backwardMatchLength = curBackwardMatchLength;
                    bestEntry = Some(*cur);
                }
            }

            let Some(bestEntry) = bestEntry else {
                // No match found -- insert an entry into the hash table
                // and process the next candidate match
                hashTable.insert_entry(hash as usize, newEntry);
                continue;
            };

            // Match found
            let offset = newEntry.offset.wrapping_sub(bestEntry.offset);
            let mLength = forwardMatchLength.wrapping_add(backwardMatchLength);

            let seq = rawSeqStore.seq.add(rawSeqStore.size);

            // Out of sequence storage
            if rawSeqStore.size == rawSeqStore.capacity {
                return Err(Error::dstSize_tooSmall);
            }
            (*seq).litLength = (split - backwardMatchLength - anchor) as u32;
            (*seq).matchLength = mLength as u32;
            (*seq).offset = offset;
            rawSeqStore.size = rawSeqStore.size.wrapping_add(1);

            // Insert the current entry into the hash table --- it must be
            // done after the previous block to avoid clobbering bestEntry
            hashTable.insert_entry(hash as usize, newEntry);

            anchor = split + forwardMatchLength;

            // If we find a match that ends after the data that we've hashed
            // then we have a repeating, overlapping, pattern. E.g. all zeros.
            // If one repetition of the pattern matches our `stopMask` then all
            // repetitions will. We don't need to insert them all into out table,
            // only the first one. So skip over overlapping matches.
            // This is a major speed boost (20x) for compressing a single byte
            // repeated, when that byte ends up in the table.
            if anchor > ip + hashed {
                hashState.reset(&src[anchor - minMatchLength..anchor]);
                // Continue the outer loop at anchor (ip + hashed == anchor).
                ip = anchor - hashed;
                break;
            }
        }

        ip += hashed;
    }

    Ok(src.len() - anchor)
}

impl LdmState {
    pub unsafe fn generate_sequences(
        &mut self,
        sequences: &mut RawSeqStore_t,
        params: &LdmParams,
        src: &[u8],
    ) -> Result<(), Error> {
        let maxDist = 1 << params.windowLog;
        let kMaxChunkSize = 1 << 20;
        let mut leftoverSize = 0;

        for chunk in src.chunks(kMaxChunkSize) {
            if sequences.size >= sequences.capacity {
                break;
            }
            let chunkStart = chunk.as_ptr_range().start;
            let chunkEnd = chunk.as_ptr_range().end;
            let prevSize = sequences.size;

            // 1. Perform overflow correction if necessary.
            if self.window.needs_overflow_correction(
                0,
                maxDist,
                self.loadedDictEnd,
                chunkStart as *const core::ffi::c_void,
                chunkEnd as *const core::ffi::c_void,
            ) {
                let correction = self.window.correct_overflow(
                    0,
                    maxDist,
                    chunkStart as *const core::ffi::c_void,
                );
                self.get_hash_table(params).reduce_table(correction);
                // invalidate dictionaries on overflow correction
                self.loadedDictEnd = 0;
            }

            // 2. We enforce the maximum offset allowed.
            //
            // kMaxChunkSize should be small enough that we don't lose too much of
            // the window through early invalidation.
            // TODO: * Test the chunk size.
            //       * Try invalidation after the sequence generation and test the
            //         offset against maxDist directly.
            //
            // NOTE: Because of dictionaries + sequence splitting we MUST make sure
            // that any offset used is valid at the END of the sequence, since it may
            // be split into two sequences. This condition holds when using
            // ZSTD_window_t::enforce_max_dist, but if we move to checking offsets
            // against maxDist directly, we'll have to carefully handle that case.
            self.window.enforce_max_dist(
                chunkEnd as *const core::ffi::c_void,
                maxDist,
                &mut self.loadedDictEnd,
                None,
            );

            // 3. Generate the sequences for the chunk, and get newLeftoverSize.
            let newLeftoverSize =
                ZSTD_ldm_generateSequences_internal(self, sequences, params, chunk)?;

            // 4. We add the leftover literals from previous iterations to the first
            //    newly generated sequence, or add the `newLeftoverSize` if none are
            //    generated.
            // Prepend the leftover literals from the last call
            if prevSize < sequences.size {
                let fresh5 = &mut (*sequences.seq.add(prevSize)).litLength;
                *fresh5 = (*fresh5).wrapping_add(leftoverSize as u32);
                leftoverSize = newLeftoverSize;
            } else {
                leftoverSize = leftoverSize.wrapping_add(chunk.len());
            }
        }

        Ok(())
    }
}

pub fn ZSTD_ldm_skipSequences(
    seqs: &mut [rawSeq],
    pos: &mut usize,
    mut srcSize: usize,
    minMatch: u32,
) {
    while srcSize > 0 && *pos < seqs.len() {
        let seq = &mut seqs[*pos];
        if srcSize <= seq.litLength as usize {
            // Skip past srcSize literals
            seq.litLength = seq.litLength.wrapping_sub(srcSize as u32);
            return;
        }
        srcSize = srcSize.wrapping_sub(seq.litLength as usize);
        seq.litLength = 0;
        if srcSize < seq.matchLength as usize {
            // Skip past the first srcSize of the match
            seq.matchLength = seq.matchLength.wrapping_sub(srcSize as u32);
            if seq.matchLength < minMatch {
                // The match is too short, omit it
                let matchLength = seq.matchLength;
                if let Some(next) = seqs.get_mut(*pos + 1) {
                    next.litLength = next.litLength.wrapping_add(matchLength);
                }
                *pos += 1;
            }
            return;
        }
        srcSize = srcSize.wrapping_sub(seq.matchLength as usize);
        seq.matchLength = 0;
        *pos += 1;
    }
}

/// If the sequence length is longer than remaining then the sequence is split
/// between this block and the next.
///
/// Returns the current sequence to handle, or if the rest of the block should
/// be literals, it returns a sequence with offset == 0.
fn maybeSplitSequence(
    seqs: &mut [rawSeq],
    pos: &mut usize,
    remaining: u32,
    minMatch: u32,
) -> rawSeq {
    let mut sequence = seqs[*pos];

    // Likely: No partial sequence
    if remaining >= sequence.litLength.wrapping_add(sequence.matchLength) {
        *pos += 1;
        return sequence;
    }

    // Cut the sequence short (offset == 0 ==> rest is literals).
    if remaining <= sequence.litLength {
        sequence.offset = 0;
    } else if remaining < sequence.litLength.wrapping_add(sequence.matchLength) {
        sequence.matchLength = remaining.wrapping_sub(sequence.litLength);
        if sequence.matchLength < minMatch {
            sequence.offset = 0;
        }
    }

    // Skip past `remaining` bytes for the future sequences.
    ZSTD_ldm_skipSequences(seqs, pos, remaining as usize, minMatch);
    sequence
}

/// Moves forward in @rawSeqStore by @nbBytes,
/// which will update the fields 'pos' and 'posInSequence'.
pub unsafe fn ZSTD_ldm_skipRawSeqStoreBytes(rawSeqStore: &mut RawSeqStore_t, nbBytes: usize) {
    let mut currPos = rawSeqStore.posInSequence.wrapping_add(nbBytes) as u32;
    while currPos != 0 && rawSeqStore.pos < rawSeqStore.size {
        let currSeq = *rawSeqStore.seq.add(rawSeqStore.pos);
        if currPos >= currSeq.litLength.wrapping_add(currSeq.matchLength) {
            currPos = currPos.wrapping_sub(currSeq.litLength.wrapping_add(currSeq.matchLength));
            rawSeqStore.pos = rawSeqStore.pos.wrapping_add(1);
        } else {
            rawSeqStore.posInSequence = currPos as usize;
            break;
        }
    }

    if currPos == 0 || rawSeqStore.pos == rawSeqStore.size {
        rawSeqStore.posInSequence = 0;
    }
}

pub unsafe fn ZSTD_ldm_blockCompress(
    rawSeqStore: &mut RawSeqStore_t,
    ms: &mut ZSTD_MatchState_t,
    seqStore: &mut SeqStore_t,
    rep: &mut RepCodes,
    useRowMatchFinder: ParamSwitch,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    let cParams = &ms.cParams;
    let minMatch = cParams.minMatch;
    let blockCompressor =
        ZSTD_selectBlockCompressor(cParams.strategy, useRowMatchFinder, ms.dict_mode());

    // Input bounds
    let istart = src as *const u8;
    let iend = istart.add(srcSize);

    // Input positions
    let mut ip = istart;

    // If using opt parser, use LDMs only as candidates rather than always accepting them
    if cParams.strategy >= ZSTD_btopt {
        ms.ldmSeqStore = rawSeqStore;
        let lastLLSize = blockCompressor.unwrap_unchecked()(ms, seqStore, rep, src, srcSize);
        ZSTD_ldm_skipRawSeqStoreBytes(rawSeqStore, srcSize);
        return lastLLSize;
    }

    // Loop through each sequence and apply the block compressor to the literals
    while rawSeqStore.pos < rawSeqStore.size && ip < iend {
        // maybeSplitSequence updates rawSeqStore.pos
        let seqs = core::slice::from_raw_parts_mut(rawSeqStore.seq, rawSeqStore.size);
        let sequence = maybeSplitSequence(
            seqs,
            &mut rawSeqStore.pos,
            iend.offset_from_unsigned(ip) as u32,
            minMatch,
        );

        // End signal
        if sequence.offset == 0 {
            break;
        }

        // Fill tables for block compressor
        ZSTD_ldm_limitTableUpdate(ms, ip);
        ZSTD_ldm_fillFastTables(ms, ip as *const core::ffi::c_void);

        // Run the block compressor
        let newLitLength = blockCompressor.unwrap_unchecked()(
            ms,
            seqStore,
            rep,
            ip as *const core::ffi::c_void,
            sequence.litLength as usize,
        );
        ip = ip.offset(sequence.litLength as isize);
        // Update the repcodes
        for i in (1..ZSTD_REP_NUM as usize).rev() {
            rep[i] = rep[i - 1];
        }
        rep[0] = sequence.offset;
        // Store the sequence
        ZSTD_storeSeq(
            seqStore,
            newLitLength,
            ip.sub(newLitLength),
            iend,
            sequence.offset.wrapping_add(ZSTD_REP_NUM),
            sequence.matchLength as usize,
        );
        ip = ip.offset(sequence.matchLength as isize);
    }

    // Fill the tables for the block compressor
    ZSTD_ldm_limitTableUpdate(ms, ip);
    ZSTD_ldm_fillFastTables(ms, ip as *const core::ffi::c_void);

    // Compress the last literals
    blockCompressor.unwrap_unchecked()(
        ms,
        seqStore,
        rep,
        ip as *const core::ffi::c_void,
        iend.offset_from_unsigned(ip),
    )
}
