use core::mem::MaybeUninit;
use core::ptr;

use crate::lib::polyfill::PointerExt;

pub type ZSTD_CCtx = ZSTD_CCtx_s;

#[derive(Default)]
#[repr(C)]
pub struct ZSTD_CCtx_s {
    pub(super) stage: CompressionStage,
    pub(super) cParamsChanged: core::ffi::c_int,
    pub(super) bmi2: core::ffi::c_int,
    pub(super) requestedParams: ZSTD_CCtx_params,
    pub(super) appliedParams: ZSTD_CCtx_params,
    pub(super) simpleApiParams: ZSTD_CCtx_params,
    pub(super) dictID: u32,
    pub(super) dictContentSize: usize,
    pub(super) workspace: ZSTD_cwksp,
    pub(super) blockSizeMax: usize,
    pub(super) pledgedSrcSizePlusOne: core::ffi::c_ulonglong,
    pub(super) consumedSrcSize: core::ffi::c_ulonglong,
    pub(super) producedCSize: core::ffi::c_ulonglong,
    pub(super) xxhState: XXH64_state_t,
    pub(super) customMem: ZSTD_customMem,
    pub(super) pool: *mut ZSTD_threadPool,
    pub(super) staticSize: usize,
    pub(super) seqCollector: SeqCollector,
    pub(super) isFirstBlock: core::ffi::c_int,
    pub(super) initialized: core::ffi::c_int,
    pub(crate) seqStore: SeqStore_t,
    pub(super) ldmState: LdmState,
    pub(super) ldmSequences: *mut rawSeq,
    pub(super) maxNbLdmSequences: usize,
    pub(super) externSeqStore: RawSeqStore_t,
    pub(super) blockState: ZSTD_blockState_t,
    pub(super) tmpWorkspace: *mut core::ffi::c_void,
    pub(super) tmpWkspSize: usize,
    pub(super) bufferedPolicy: BufferedPolicy,
    pub(super) inBuff: *mut u8,
    pub(super) inBuffSize: usize,
    pub(super) inToCompress: usize,
    pub(super) inBuffPos: usize,
    pub(super) inBuffTarget: usize,
    pub(super) outBuff: *mut u8,
    pub(super) outBuffSize: usize,
    pub(super) outBuffContentSize: usize,
    pub(super) outBuffFlushedSize: usize,
    pub(super) streamStage: StreamStage,
    pub(super) frameEnded: u32,
    pub(super) expectedInBuffer: ZSTD_inBuffer,
    pub(super) stableIn_notConsumed: usize,
    pub(super) expectedOutBufferSize: usize,
    pub(super) localDict: ZSTD_localDict,
    pub(super) cdict: *const ZSTD_CDict,
    pub(super) prefixDict: ZSTD_prefixDict,
    pub(super) mtctx: *mut ZSTDMT_CCtx,
    pub(super) traceCtx: ZSTD_TraceCtx,
    pub(super) blockSplitCtx: ZSTD_blockSplitCtx,
    pub(super) extSeqBuf: *mut ZSTD_Sequence,
    pub(super) extSeqBufCapacity: usize,
}

#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct ZSTD_entropyCTablesMetadata_t {
    pub hufMetadata: ZSTD_hufCTablesMetadata_t,
    pub fseMetadata: ZSTD_fseCTablesMetadata_t,
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct ZSTD_fseCTablesMetadata_t {
    pub llType: SymbolEncodingType,
    pub ofType: SymbolEncodingType,
    pub mlType: SymbolEncodingType,
    pub fseTablesBuffer: [u8; ZSTD_MAX_FSE_HEADERS_SIZE],
    pub fseTablesSize: usize,
    pub lastCountSize: usize,
}

impl Default for ZSTD_fseCTablesMetadata_t {
    fn default() -> Self {
        Self {
            llType: Default::default(),
            ofType: Default::default(),
            mlType: Default::default(),
            fseTablesBuffer: [0; _],
            fseTablesSize: Default::default(),
            lastCountSize: Default::default(),
        }
    }
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct ZSTD_hufCTablesMetadata_t {
    pub hType: SymbolEncodingType,
    pub hufDesBuffer: [u8; ZSTD_MAX_HUF_HEADER_SIZE],
    pub hufDesSize: usize,
}

impl Default for ZSTD_hufCTablesMetadata_t {
    fn default() -> Self {
        Self {
            hType: Default::default(),
            hufDesBuffer: [0; _],
            hufDesSize: Default::default(),
        }
    }
}

#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct SeqStore_t {
    pub sequencesStart: *mut SeqDef,
    pub sequences: *mut SeqDef,
    pub litStart: *mut u8,
    pub lit: *mut u8,
    pub llCode: *mut u8,
    pub mlCode: *mut u8,
    pub ofCode: *mut u8,
    pub maxNbSeq: usize,
    pub maxNbLit: usize,
    pub longLengthType: LongLengthType,
    pub longLengthPos: u32,
}

impl SeqStore_t {
    pub(crate) unsafe fn seqs(&self) -> &[SeqDef] {
        core::slice::from_raw_parts(
            self.sequencesStart,
            self.sequences.offset_from_unsigned(self.sequencesStart),
        )
    }

    /// The code tables are only filled in by `ZSTD_seqToCodes`.
    pub(crate) unsafe fn llCodes(&self) -> &[u8] {
        core::slice::from_raw_parts(
            self.llCode,
            self.sequences.offset_from_unsigned(self.sequencesStart),
        )
    }

    pub(crate) unsafe fn mlCodes(&self) -> &[u8] {
        core::slice::from_raw_parts(
            self.mlCode,
            self.sequences.offset_from_unsigned(self.sequencesStart),
        )
    }

    pub(crate) unsafe fn ofCodes(&self) -> &[u8] {
        core::slice::from_raw_parts(
            self.ofCode,
            self.sequences.offset_from_unsigned(self.sequencesStart),
        )
    }
}

pub type SeqDef = SeqDef_s;

#[derive(Copy, Clone)]
#[repr(C)]
pub struct SeqDef_s {
    pub offBase: u32,
    pub litLength: u16,
    pub mlBase: u16,
}

pub type ZSTD_CDict = ZSTD_CDict_s;

#[repr(C)]
pub struct ZSTD_CDict_s {
    pub dictContent: *const core::ffi::c_void,
    pub dictContentSize: usize,
    /// The dictContentType the CDict was created with
    pub dictContentType: ZSTD_dictContentType_e,
    /// entropy workspace of HUF_WORKSPACE_SIZE bytes
    pub entropyWorkspace: *mut u32,
    pub workspace: ZSTD_cwksp,
    pub matchState: ZSTD_MatchState_t,
    pub cBlockState: ZSTD_compressedBlockState_t,
    pub customMem: ZSTD_customMem,
    pub dictID: u32,
    /// 0 indicates that advanced API was used to select CDict params
    pub compressionLevel: core::ffi::c_int,
    /// Indicates whether the CDict was created with params that would use
    /// row-based matchfinder. Unless the CDict is reloaded, we will use
    /// the same greedy/lazy matchfinder at compression time.
    pub useRowMatchFinder: ParamSwitch,
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct ZSTD_compressedBlockState_t {
    pub entropy: ZSTD_entropyCTables_t,
    pub rep: RepCodes,
}

#[repr(C)]
#[derive(Default)]
pub struct ZSTD_MatchState_t {
    pub window: ZSTD_window_t,
    pub loadedDictEnd: u32,
    pub nextToUpdate: u32,
    pub hashLog3: u32,
    pub rowHashLog: u32,
    pub tagTable: *mut u8,
    pub hashCache: [u32; 8],
    pub hashSalt: u64,
    pub hashSaltEntropy: u32,
    pub hashTable: *mut u32,
    pub hashTable3: *mut u32,
    pub chainTable: *mut u32,
    pub forceNonContiguous: core::ffi::c_int,
    pub dedicatedDictSearch: core::ffi::c_int,
    pub opt: optState_t,
    pub dictMatchState: *const ZSTD_MatchState_t,
    pub cParams: ZSTD_compressionParameters,
    pub ldmSeqStore: *const RawSeqStore_t,
    pub prefetchCDictTables: core::ffi::c_int,
    pub lazySkipping: core::ffi::c_int,
}

#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct RawSeqStore_t {
    pub seq: *mut rawSeq,
    pub pos: usize,
    pub posInSequence: usize,
    pub size: usize,
    pub capacity: usize,
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct rawSeq {
    pub offset: u32,
    pub litLength: u32,
    pub matchLength: u32,
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct ZSTD_optimal_t {
    pub price: core::ffi::c_int,
    pub off: u32,
    pub mlen: u32,
    pub litlen: u32,
    pub rep: RepCodes,
}

#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct ZSTD_window_t {
    pub nextSrc: *const u8,
    pub base: *const u8,
    pub dictBase: *const u8,
    pub dictLimit: u32,
    pub lowLimit: u32,
    pub nbOverflowCorrections: u32,
}

#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct ZSTD_cwksp {
    pub workspace: *mut core::ffi::c_void,
    pub workspaceEnd: *mut core::ffi::c_void,
    pub objectEnd: *mut core::ffi::c_void,
    pub tableEnd: *mut core::ffi::c_void,
    pub tableValidEnd: *mut core::ffi::c_void,
    pub allocStart: *mut core::ffi::c_void,
    pub initOnceStart: *mut core::ffi::c_void,
    pub allocFailed: u8,
    pub workspaceOversizedDuration: core::ffi::c_int,
    pub phase: CwkspAllocPhase,
    pub isStatic: CwkspAllocKind,
}

#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq, Default)]
pub enum CwkspAllocKind {
    #[default]
    Dynamic = 0,
    Static = 1,
}

#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum CwkspAllocPhase {
    #[default]
    Objects = 0,
    AlignedInitOnce = 1,
    Aligned = 2,
    Buffers = 3,
}

pub type ZSTD_CCtx_params = ZSTD_CCtx_params_s;

#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct ZSTD_CCtx_params_s {
    pub format: Format,
    pub cParams: ZSTD_compressionParameters,
    pub fParams: ZSTD_frameParameters,
    pub compressionLevel: core::ffi::c_int,
    pub forceWindow: core::ffi::c_int,
    pub targetCBlockSize: usize,
    pub srcSizeHint: core::ffi::c_int,
    pub attachDictPref: ZSTD_dictAttachPref_e,
    pub literalCompressionMode: ParamSwitch,
    pub nbWorkers: core::ffi::c_int,
    pub jobSize: usize,
    pub overlapLog: core::ffi::c_int,
    pub rsyncable: core::ffi::c_int,
    pub ldmParams: LdmParams,
    pub enableDedicatedDictSearch: core::ffi::c_int,
    pub inBufferMode: ZSTD_bufferMode_e,
    pub outBufferMode: ZSTD_bufferMode_e,
    pub blockDelimiters: ZSTD_SequenceFormat_e,
    pub validateSequences: core::ffi::c_int,
    pub postBlockSplitter: ParamSwitch,
    pub preBlockSplitter_level: core::ffi::c_int,
    pub maxBlockSize: usize,
    pub useRowMatchFinder: ParamSwitch,
    pub deterministicRefPrefix: core::ffi::c_int,
    pub customMem: ZSTD_customMem,
    pub prefetchCDictTables: ParamSwitch,
    pub enableMatchFinderFallback: core::ffi::c_int,
    pub extSeqProdState: *mut core::ffi::c_void,
    pub extSeqProdFunc: Option<ZSTD_sequenceProducer_F>,
    pub searchForExternalRepcodes: ParamSwitch,
}

impl ZSTD_CCtx_params {
    /// Safe version of [`ZSTD_CCtxParams_init`]
    pub fn new(compressionLevel: core::ffi::c_int) -> Self {
        Self {
            compressionLevel,
            fParams: ZSTD_frameParameters {
                contentSizeFlag: 1,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Initializes `cctxParams` from `params` and `compressionLevel`.
    ///
    /// If params are derived from a compression level then that compression level,
    /// otherwise [`ZSTD_NO_CLEVEL`].
    pub fn new_internal(params: &ZSTD_parameters, compressionLevel: core::ffi::c_int) -> Self {
        Self {
            cParams: params.cParams,
            fParams: params.fParams,
            // Should not matter, as all cParams are presumed properly defined.
            // But, set it for tracing anyway.
            compressionLevel,
            useRowMatchFinder: ZSTD_resolveRowMatchFinderMode(ParamSwitch::Auto, &params.cParams),
            postBlockSplitter: ZSTD_resolveBlockSplitterMode(ParamSwitch::Auto, &params.cParams),
            ldmParams: LdmParams {
                enableLdm: ZSTD_resolveEnableLdm(ParamSwitch::Auto, &params.cParams),
                ..Default::default()
            },
            validateSequences: ZSTD_resolveExternalSequenceValidation(0),
            maxBlockSize: ZSTD_resolveMaxBlockSize(0),
            searchForExternalRepcodes: ZSTD_resolveExternalRepcodeSearch(
                ParamSwitch::Auto,
                compressionLevel,
            ),
            ..Default::default()
        }
    }

    /// Sets cctxParams' cParams and fParams from validated zstd params, but otherwise leaves them alone.
    fn set_zstd_params(&mut self, params: &ZSTD_parameters) {
        self.cParams = params.cParams;
        self.fParams = params.fParams;
        // Should not matter, as all cParams are presumed properly defined.
        // But, set it for tracing anyway.
        self.compressionLevel = ZSTD_NO_CLEVEL;
    }

    #[inline]
    fn literals_compression_is_disabled(&self) -> bool {
        match self.literalCompressionMode {
            ParamSwitch::Enable => false,
            ParamSwitch::Disable => true,
            ParamSwitch::Auto => {
                self.cParams.strategy == ZSTD_fast && self.cParams.targetLength > 0
            }
        }
    }

    /// Returns `true` if an external sequence producer is registered.
    #[inline]
    fn has_ext_seq_prod(&self) -> bool {
        self.extSeqProdFunc.is_some()
    }

    /// Returns whether the target compressed block size param is being used.
    ///
    /// If used, compression will do best effort to make a compressed block size to be around
    /// `targetCBlockSize`.
    fn use_target_cblock_size(&self) -> bool {
        self.targetCBlockSize != 0
    }

    /// Returns whether the block splitting param is being used.
    ///
    /// If used, compression will do best effort to split a block in order to improve compression ratio.
    /// At the time this function is called, the parameter must be finalized.
    fn block_splitter_enabled(&self) -> bool {
        self.postBlockSplitter == ParamSwitch::Enable
    }
}

#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct ZSTD_symbolEncodingTypeStats_t {
    pub LLtype: SymbolEncodingType,
    pub Offtype: SymbolEncodingType,
    pub MLtype: SymbolEncodingType,
    pub size: usize,
    pub lastCountSize: usize,
    pub longOffsets: bool,
}

#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq, Default)]
pub enum DefaultPolicy {
    #[default]
    Disallowed = 0,
    Allowed = 1,
}

#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq, Default)]
pub enum BuildSeqStore {
    #[default]
    Compress = 0,
    NoCompress = 1,
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct ZSTD_SequencePosition {
    pub idx: u32,
    pub posInSequence: u32,
    pub posInSrc: usize,
}

pub type S64 = i64;

#[repr(C)]
pub struct seqStoreSplits {
    pub splitLocations: *mut u32,
    pub idx: usize,
}

/// Controls, for this `matchState` reset, whether the tables need to be cleared /
/// prepared for the coming compression ([`Self::MakeClean`]), or whether the
/// tables can be left unclean ([`Self::LeaveDirty`]), because we know that a
/// subsequent operation will overwrite the table space anyways (e.g., copying
/// the matchState contents in from a CDict).
#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq, Default)]
pub enum CompResetPolicy {
    #[default]
    MakeClean = 0,
    LeaveDirty = 1,
}

#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq, Default)]
pub enum ResetTarget {
    #[default]
    CDict = 0,
    CCtx = 1,
}

/// Controls, for this `matchState` reset, whether indexing can continue where it
/// left off ([`Self::Continue`]), or whether it needs to be restarted from zero
/// ([`Self::Reset`]).
#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq, Default)]
pub enum IndexResetPolicy {
    #[default]
    Continue = 0,
    Reset = 1,
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct ZSTD_cpuid_t {
    pub f1c: u32,
    pub f1d: u32,
    pub f7b: u32,
    pub f7c: u32,
}

impl ZSTD_cParameter {
    /// Get the upper and lower bound
    const fn get_bounds(&self) -> Result<(core::ffi::c_int, core::ffi::c_int), Error> {
        match *self {
            Self::ZSTD_c_compressionLevel => Ok((ZSTD_minCLevel(), ZSTD_maxCLevel())),
            Self::ZSTD_c_windowLog => Ok((ZSTD_WINDOWLOG_MIN, ZSTD_WINDOWLOG_MAX)),
            Self::ZSTD_c_hashLog => Ok((ZSTD_HASHLOG_MIN, ZSTD_HASHLOG_MAX)),
            Self::ZSTD_c_chainLog => Ok((ZSTD_CHAINLOG_MIN, ZSTD_CHAINLOG_MAX)),
            Self::ZSTD_c_searchLog => Ok((ZSTD_SEARCHLOG_MIN, ZSTD_SEARCHLOG_MAX)),
            Self::ZSTD_c_minMatch => Ok((ZSTD_MINMATCH_MIN, ZSTD_MINMATCH_MAX)),
            Self::ZSTD_c_targetLength => Ok((ZSTD_TARGETLENGTH_MIN, ZSTD_TARGETLENGTH_MAX)),
            Self::ZSTD_c_strategy => Ok((ZSTD_STRATEGY_MIN, ZSTD_STRATEGY_MAX)),
            Self::ZSTD_c_contentSizeFlag => Ok((0, 1)),
            Self::ZSTD_c_checksumFlag => Ok((0, 1)),
            Self::ZSTD_c_dictIDFlag => Ok((0, 1)),
            Self::ZSTD_c_nbWorkers => Ok((0, ZSTDMT_NBWORKERS_MAX)),
            Self::ZSTD_c_jobSize => Ok((0, ZSTDMT_JOBSIZE_MAX)),
            Self::ZSTD_c_overlapLog => Ok((ZSTD_OVERLAPLOG_MIN, ZSTD_OVERLAPLOG_MAX)),
            Self::ZSTD_c_enableDedicatedDictSearch => Ok((0, 1)),
            Self::ZSTD_c_enableLongDistanceMatching => Ok(ParamSwitch::BOUNDS),
            Self::ZSTD_c_ldmHashLog => Ok((ZSTD_LDM_HASHLOG_MIN, ZSTD_LDM_HASHLOG_MAX)),
            Self::ZSTD_c_ldmMinMatch => Ok((ZSTD_LDM_MINMATCH_MIN, ZSTD_LDM_MINMATCH_MAX)),
            Self::ZSTD_c_ldmBucketSizeLog => {
                Ok((ZSTD_LDM_BUCKETSIZELOG_MIN, ZSTD_LDM_BUCKETSIZELOG_MAX))
            }
            Self::ZSTD_c_ldmHashRateLog => Ok((ZSTD_LDM_HASHRATELOG_MIN, ZSTD_LDM_HASHRATELOG_MAX)),
            Self::ZSTD_c_rsyncable => Ok((0, 1)),
            Self::ZSTD_c_forceMaxWindow => Ok((0, 1)),
            Self::ZSTD_c_format => Ok((
                Format::ZSTD_f_zstd1 as core::ffi::c_int,
                Format::ZSTD_f_zstd1_magicless as core::ffi::c_int,
            )),
            Self::ZSTD_c_forceAttachDict => Ok((
                ZSTD_dictAttachPref_e::ZSTD_dictDefaultAttach.0 as core::ffi::c_int,
                ZSTD_dictAttachPref_e::ZSTD_dictForceLoad.0 as core::ffi::c_int,
            )),
            Self::ZSTD_c_literalCompressionMode => Ok(ParamSwitch::BOUNDS),
            Self::ZSTD_c_targetCBlockSize => {
                Ok((ZSTD_TARGETCBLOCKSIZE_MIN, ZSTD_TARGETCBLOCKSIZE_MAX))
            }
            Self::ZSTD_c_srcSizeHint => Ok((ZSTD_SRCSIZEHINT_MIN, ZSTD_SRCSIZEHINT_MAX)),
            Self::ZSTD_c_stableInBuffer | Self::ZSTD_c_stableOutBuffer => Ok((
                ZSTD_bm_buffered as core::ffi::c_int,
                ZSTD_bm_stable as core::ffi::c_int,
            )),
            Self::ZSTD_c_blockDelimiters => Ok((
                ZSTD_sf_noBlockDelimiters as core::ffi::c_int,
                ZSTD_sf_explicitBlockDelimiters as core::ffi::c_int,
            )),
            Self::ZSTD_c_validateSequences => Ok((0, 1)),
            Self::ZSTD_c_splitAfterSequences => Ok(ParamSwitch::BOUNDS),
            Self::ZSTD_c_blockSplitterLevel => Ok((0, ZSTD_BLOCKSPLITTER_LEVEL_MAX)),
            Self::ZSTD_c_useRowMatchFinder => Ok(ParamSwitch::BOUNDS),
            Self::ZSTD_c_deterministicRefPrefix => Ok((0, 1)),
            Self::ZSTD_c_prefetchCDictTables => Ok(ParamSwitch::BOUNDS),
            Self::ZSTD_c_enableSeqProducerFallback => Ok((0, 1)),
            Self::ZSTD_c_maxBlockSize => Ok((ZSTD_BLOCKSIZE_MAX_MIN, ZSTD_BLOCKSIZE_MAX)),
            Self::ZSTD_c_repcodeResolution => Ok(ParamSwitch::BOUNDS),
            _ => Err(Error::parameter_unsupported),
        }
    }

    /// Clamps the value into the bounded range of the cParam
    #[inline]
    fn clamp_bounds(&self, value: &mut core::ffi::c_int) -> Result<(), Error> {
        let (lowerBound, upperBound) = self.get_bounds()?;
        *value = (*value).clamp(lowerBound, upperBound);
        Ok(())
    }

    /// Same as [`Self::clamp_bounds`], but for unsigned valued
    #[inline]
    fn clamp_bounds_unsigned(&self, value: &mut core::ffi::c_uint) -> Result<(), Error> {
        let (lowerBound, upperBound) = self.get_bounds()?;
        *value = (*value as core::ffi::c_int).clamp(lowerBound, upperBound) as core::ffi::c_uint;
        Ok(())
    }

    /// Returns `Ok(())` if value is within cParam bounds, [`Error::parameter_outOfBound`] otherwise
    #[inline]
    fn check_bounds(&self, value: core::ffi::c_int) -> Result<(), Error> {
        let (lowerBound, upperBound) = self.get_bounds()?;
        if value >= lowerBound && value <= upperBound {
            Ok(())
        } else {
            Err(Error::parameter_outOfBound)
        }
    }

    /// Returns true if parameter is authorized to be updated mid-stream
    fn is_update_authorized(&self) -> bool {
        matches!(
            *self,
            ZSTD_cParameter::ZSTD_c_compressionLevel
                | ZSTD_cParameter::ZSTD_c_hashLog
                | ZSTD_cParameter::ZSTD_c_chainLog
                | ZSTD_cParameter::ZSTD_c_searchLog
                | ZSTD_cParameter::ZSTD_c_minMatch
                | ZSTD_cParameter::ZSTD_c_targetLength
                | ZSTD_cParameter::ZSTD_c_strategy
                | ZSTD_cParameter::ZSTD_c_blockSplitterLevel
        )
    }
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct ZSTD_bounds {
    pub error: usize,
    pub lowerBound: core::ffi::c_int,
    pub upperBound: core::ffi::c_int,
}

impl From<ZSTD_cParameter> for ZSTD_bounds {
    fn from(param: ZSTD_cParameter) -> Self {
        match param.get_bounds() {
            Ok((lowerBound, upperBound)) => ZSTD_bounds {
                error: 0,
                lowerBound,
                upperBound,
            },
            Err(err) => ZSTD_bounds {
                error: err.to_error_code(),
                lowerBound: 0,
                upperBound: 0,
            },
        }
    }
}

pub type ZSTD_CStream = ZSTD_CCtx;

pub type ZSTD_SequenceCopier_f = unsafe fn(
    *mut ZSTD_CCtx,
    &mut ZSTD_SequencePosition,
    &[ZSTD_Sequence],
    *const core::ffi::c_void,
    usize,
    ParamSwitch,
) -> Result<usize, Error>;

#[derive(Copy, Clone)]
#[repr(C)]
pub struct BlockSummary {
    pub nbSequences: usize,
    pub blockSize: usize,
    pub litSize: usize,
}

pub const ZSTD_BLOCKSPLITTER_LEVEL_MAX: core::ffi::c_int = 6;
pub const ZSTD_OPT_SIZE: usize = ZSTD_OPT_NUM + 3;
pub const ZSTD_MAX_NB_BLOCK_SPLITS: usize = 196;

#[inline]
unsafe fn ZSTD_rleCompressBlock(
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: u8,
    srcSize: usize,
    lastBlock: bool,
) -> Result<usize, Error> {
    let op = dst as *mut u8;
    let cBlockHeader = u32::from(lastBlock)
        .wrapping_add((BlockType::Rle as u32) << 1)
        .wrapping_add((srcSize << 3) as u32);
    if dstCapacity < 4 {
        return Err(Error::dstSize_tooSmall);
    }
    MEM_writeLE24(op as *mut core::ffi::c_void, cBlockHeader);
    *op.add(3) = src;
    Ok(4)
}

/// Similar to [`ZSTD_window_t::enforce_max_dist`], but only invalidates dictionary when
/// the input progresses beyond window size.
///
/// Assumption: loadedDictEndPtr and dictMatchStatePtr are valid (non NULL),
/// loadedDictEnd uses same referential as window->base, and
/// maxDist is the window size.
#[inline]
fn ZSTD_checkDictValidity(
    window: &ZSTD_window_t,
    blockEnd: *const core::ffi::c_void,
    maxDist: u32,
    loadedDictEndPtr: &mut u32,
    dictMatchStatePtr: &mut *const ZSTD_MatchState_t,
) {
    let blockEndIdx =
        (blockEnd as *const u8).wrapping_offset_from(window.base) as core::ffi::c_long as u32;
    let loadedDictEnd = *loadedDictEndPtr;
    if blockEndIdx > loadedDictEnd.wrapping_add(maxDist) || loadedDictEnd != window.dictLimit {
        *loadedDictEndPtr = 0;
        *dictMatchStatePtr = core::ptr::null();
    } else {
        // FIXME: add log
        // *loadedDictEndPtr != 0;
    }
}

use libc::ptrdiff_t;

use crate::lib::common::allocations::{ZSTD_customCalloc, ZSTD_customFree, ZSTD_customMalloc};
use crate::lib::common::bits::ZSTD_highbit32;
use crate::lib::common::bitstream::STREAM_ACCUMULATOR_MIN;
use crate::lib::common::entropy_common::FSE_readNCount;
use crate::lib::common::error_private::{ERR_isError, Error};
use crate::lib::common::fse::{FSE_CTable, FSE_repeat};
use crate::lib::common::huf::{
    CTable, HUF_flags_optimalDepth, HUF_repeat, HUF_OPTIMAL_DEPTH_THRESHOLD, HUF_SYMBOLVALUE_MAX,
    HUF_SYMBOLVALUE_MAX_U8, HUF_WORKSPACE_SIZE,
};
use crate::lib::common::mem::{
    MEM_32bits, MEM_readLE32, MEM_writeLE16, MEM_writeLE24, MEM_writeLE32, MEM_writeLE64,
};
use crate::lib::common::pool::ZSTD_threadPool;
use crate::lib::common::xxhash::{
    XXH64_state_t, ZSTD_XXH64_digest, ZSTD_XXH64_reset, ZSTD_XXH64_update_slice,
};
use crate::lib::common::zstd_internal::{
    repStartValue, BlockType, DefaultMaxOff, LLFSELog, LL_bits, LL_defaultNorm, LL_defaultNormLog,
    LitHufLog, MLFSELog, ML_bits, ML_defaultNorm, ML_defaultNormLog, MaxLL, MaxLit, MaxML, MaxOff,
    MaxSeq, OF_defaultNorm, OF_defaultNormLog, OffFSELog, RepCodes, SymbolEncodingType,
    ZSTD_cpuSupportsBmi2, ZSTD_limitCopy, LONGNBSEQ, MINMATCH, WILDCOPY_OVERLENGTH,
    ZSTD_BLOCKHEADERSIZE, ZSTD_MAX_FSE_HEADERS_SIZE, ZSTD_MAX_HUF_HEADER_SIZE, ZSTD_OPT_NUM,
    ZSTD_REP_NUM, ZSTD_WORKSPACETOOLARGE_FACTOR, ZSTD_WORKSPACETOOLARGE_MAXDURATION,
};
use crate::lib::common::zstd_trace::{
    ZSTD_Trace, ZSTD_TraceCtx, ZSTD_trace_compress_begin, ZSTD_trace_compress_end,
};
use crate::lib::compress::fse_compress::FSE_buildCTable_wksp;
use crate::lib::compress::hist::{HIST_countFast_wksp, HIST_count_wksp};
use crate::lib::compress::huf_compress::{
    HUF_buildCTable_wksp, HUF_estimateCompressedSize, HUF_optimalTableLog, HUF_readCTable,
    HUF_validateCTable, HUF_writeCTable_wksp,
};
use crate::lib::compress::zstd_compress_internal::{
    optState_t, BufferedPolicy, CParamMode, CompressionStage, DictMode, DictTableLoadMethod,
    LongLengthType, SeqCollector, StreamStage, TableFillPurpose, ZSTD_BlockCompressor_f,
    ZSTD_LLcode, ZSTD_MLcode, ZSTD_blockSplitCtx, ZSTD_blockState_t, ZSTD_entropyCTables_t,
    ZSTD_fseCTables_t, ZSTD_getSequenceLength, ZSTD_hufCTables_t, ZSTD_localDict, ZSTD_match_t,
    ZSTD_minGain, ZSTD_noCompressBlock, ZSTD_prefixDict, ZSTD_storeSeq, ZSTD_storeSeqOnly,
    ZSTD_updateRep, HASH_READ_SIZE, REPCODE1_TO_OFFBASE, REPCODE3_TO_OFFBASE, ZSTD_CHUNKSIZE_MAX,
    ZSTD_CURRENT_MAX, ZSTD_DUBT_UNSORTED_MARK, ZSTD_SHORT_CACHE_TAG_BITS, ZSTD_WINDOW_START_INDEX,
};
use crate::lib::compress::zstd_compress_literals::ZSTD_compressLiterals;
use crate::lib::compress::zstd_compress_sequences::{
    ZSTD_buildCTable, ZSTD_crossEntropyCost, ZSTD_encodeSequences, ZSTD_fseBitCost,
    ZSTD_selectEncodingType,
};
use crate::lib::compress::zstd_compress_superblock::ZSTD_compressSuperBlock;
use crate::lib::compress::zstd_double_fast::{
    ZSTD_compressBlock_doubleFast, ZSTD_compressBlock_doubleFast_dictMatchState,
    ZSTD_compressBlock_doubleFast_extDict, ZSTD_fillDoubleHashTable,
};
use crate::lib::compress::zstd_fast::{
    ZSTD_compressBlock_fast, ZSTD_compressBlock_fast_dictMatchState,
    ZSTD_compressBlock_fast_extDict, ZSTD_fillHashTable,
};
use crate::lib::compress::zstd_lazy::{
    ZSTD_compressBlock_btlazy2, ZSTD_compressBlock_btlazy2_dictMatchState,
    ZSTD_compressBlock_btlazy2_extDict, ZSTD_compressBlock_greedy,
    ZSTD_compressBlock_greedy_dedicatedDictSearch,
    ZSTD_compressBlock_greedy_dedicatedDictSearch_row, ZSTD_compressBlock_greedy_dictMatchState,
    ZSTD_compressBlock_greedy_dictMatchState_row, ZSTD_compressBlock_greedy_extDict,
    ZSTD_compressBlock_greedy_extDict_row, ZSTD_compressBlock_greedy_row, ZSTD_compressBlock_lazy,
    ZSTD_compressBlock_lazy2, ZSTD_compressBlock_lazy2_dedicatedDictSearch,
    ZSTD_compressBlock_lazy2_dedicatedDictSearch_row, ZSTD_compressBlock_lazy2_dictMatchState,
    ZSTD_compressBlock_lazy2_dictMatchState_row, ZSTD_compressBlock_lazy2_extDict,
    ZSTD_compressBlock_lazy2_extDict_row, ZSTD_compressBlock_lazy2_row,
    ZSTD_compressBlock_lazy_dedicatedDictSearch, ZSTD_compressBlock_lazy_dedicatedDictSearch_row,
    ZSTD_compressBlock_lazy_dictMatchState, ZSTD_compressBlock_lazy_dictMatchState_row,
    ZSTD_compressBlock_lazy_extDict, ZSTD_compressBlock_lazy_extDict_row,
    ZSTD_compressBlock_lazy_row, ZSTD_dedicatedDictSearch_lazy_loadDictionary,
    ZSTD_insertAndFindFirstIndex, ZSTD_row_update, ZSTD_LAZY_DDSS_BUCKET_LOG,
    ZSTD_ROW_HASH_TAG_BITS,
};
use crate::lib::compress::zstd_ldm::{
    LdmEntry, LdmParams, LdmState, ZSTD_ldm_blockCompress, ZSTD_ldm_skipRawSeqStoreBytes,
    ZSTD_ldm_skipSequences,
};
use crate::lib::compress::zstd_opt::{
    ZSTD_compressBlock_btopt, ZSTD_compressBlock_btopt_dictMatchState,
    ZSTD_compressBlock_btopt_extDict, ZSTD_compressBlock_btultra, ZSTD_compressBlock_btultra2,
    ZSTD_compressBlock_btultra_dictMatchState, ZSTD_compressBlock_btultra_extDict, ZSTD_updateTree,
};
use crate::lib::compress::zstd_preSplit::{ZSTD_splitBlock, ZSTD_SLIPBLOCK_WORKSPACESIZE};
use crate::lib::compress::zstdmt_compress::{
    ZSTDMT_CCtx, ZSTDMT_compressStream_generic, ZSTDMT_createCCtx_advanced, ZSTDMT_freeCCtx,
    ZSTDMT_getFrameProgression, ZSTDMT_initCStream_internal, ZSTDMT_nextInputSizeHint,
    ZSTDMT_sizeof_CCtx, ZSTDMT_toFlushNow, ZSTDMT_updateCParams_whileCompressing,
    ZSTDMT_JOBSIZE_MAX, ZSTDMT_JOBSIZE_MIN, ZSTDMT_NBWORKERS_MAX,
};
use crate::lib::zstd::{
    Format, ParamSwitch, ZSTD_EndDirective, ZSTD_ResetDirective, ZSTD_Sequence,
    ZSTD_SequenceFormat_e, ZSTD_bm_buffered, ZSTD_bm_stable, ZSTD_btlazy2, ZSTD_btopt,
    ZSTD_btultra, ZSTD_btultra2, ZSTD_bufferMode_e, ZSTD_cParameter, ZSTD_compressionParameters,
    ZSTD_customMem, ZSTD_dct_auto, ZSTD_dct_fullDict, ZSTD_dct_rawContent, ZSTD_dfast,
    ZSTD_dictAttachPref_e, ZSTD_dictContentType_e, ZSTD_dictLoadMethod_e, ZSTD_dlm_byCopy,
    ZSTD_dlm_byRef, ZSTD_e_continue, ZSTD_e_end, ZSTD_e_flush, ZSTD_fast, ZSTD_frameParameters,
    ZSTD_frameProgression, ZSTD_greedy, ZSTD_inBuffer, ZSTD_inBuffer_s, ZSTD_lazy, ZSTD_lazy2,
    ZSTD_outBuffer, ZSTD_outBuffer_s, ZSTD_parameters, ZSTD_sequenceProducer_F,
    ZSTD_sf_explicitBlockDelimiters, ZSTD_sf_noBlockDelimiters, ZSTD_strategy, ZSTD_BLOCKSIZE_MAX,
    ZSTD_BLOCKSIZE_MAX_MIN, ZSTD_CHAINLOG_MAX, ZSTD_CHAINLOG_MIN, ZSTD_CLEVEL_DEFAULT,
    ZSTD_CONTENTSIZE_UNKNOWN, ZSTD_HASHLOG_MAX, ZSTD_HASHLOG_MIN, ZSTD_LDM_BUCKETSIZELOG_MAX,
    ZSTD_LDM_BUCKETSIZELOG_MIN, ZSTD_LDM_HASHLOG_MAX, ZSTD_LDM_HASHLOG_MIN,
    ZSTD_LDM_HASHRATELOG_MAX, ZSTD_LDM_HASHRATELOG_MIN, ZSTD_LDM_MINMATCH_MAX,
    ZSTD_LDM_MINMATCH_MIN, ZSTD_MAGICNUMBER, ZSTD_MAGIC_DICTIONARY, ZSTD_MAGIC_SKIPPABLE_START,
    ZSTD_MAX_INPUT_SIZE, ZSTD_MINMATCH_MAX, ZSTD_MINMATCH_MIN, ZSTD_OVERLAPLOG_MAX,
    ZSTD_OVERLAPLOG_MIN, ZSTD_SEARCHLOG_MAX, ZSTD_SEARCHLOG_MIN, ZSTD_SKIPPABLEHEADERSIZE,
    ZSTD_SRCSIZEHINT_MAX, ZSTD_SRCSIZEHINT_MIN, ZSTD_STRATEGY_MAX, ZSTD_STRATEGY_MIN,
    ZSTD_TARGETCBLOCKSIZE_MAX, ZSTD_TARGETCBLOCKSIZE_MIN, ZSTD_TARGETLENGTH_MAX,
    ZSTD_TARGETLENGTH_MIN, ZSTD_VERSION_NUMBER, ZSTD_WINDOWLOG_ABSOLUTEMIN, ZSTD_WINDOWLOG_MAX,
    ZSTD_WINDOWLOG_MIN,
};

pub const MIN_CBLOCK_SIZE: core::ffi::c_int = 1 + 1;
pub const ZSTD_CWKSP_ALIGNMENT_BYTES: core::ffi::c_int = 64;

#[inline]
fn ZSTD_cwksp_assert_internal_consistency(ws: &mut ZSTD_cwksp) {
    assert!(ws.workspace <= ws.objectEnd);
    assert!(ws.objectEnd <= ws.tableEnd);
    assert!(ws.objectEnd <= ws.tableValidEnd);
    assert!(ws.tableEnd <= ws.allocStart);
    assert!(ws.tableValidEnd <= ws.allocStart);
    assert!(ws.allocStart <= ws.workspaceEnd);
    assert!(ws.initOnceStart <= ZSTD_cwksp_initialAllocStart(ws));
    assert!(ws.workspace <= ws.initOnceStart);
}

/// Align must be a power of 2.
#[inline]
fn ZSTD_cwksp_align(size: usize, align: usize) -> usize {
    let mask = align.wrapping_sub(1);
    size.wrapping_add(mask) & !mask
}

/// Use this to determine how much space in the workspace we will consume to allocate this object.
/// (Normally it should be exactly the size of the object, but under special conditions, like ASAN,
/// where we pad each object, it might be larger.)
///
/// Since tables aren't currently redzoned, you don't need to call through this to figure out how
/// much space you need for the matchState tables. Everything else is though.
///
/// Do not use for sizing aligned buffers. Instead, use ZSTD_cwksp_aligned64_alloc_size().
#[inline]
pub(crate) fn ZSTD_cwksp_alloc_size(size: usize) -> usize {
    if size == 0 {
        return 0;
    }
    size
}

#[inline]
fn ZSTD_cwksp_aligned_alloc_size(size: usize, alignment: usize) -> usize {
    ZSTD_cwksp_alloc_size(ZSTD_cwksp_align(size, alignment))
}

/// Returns an adjusted alloc size that is the nearest larger multiple of 64 bytes.
/// Used to determine the number of bytes required for a given "aligned".
#[inline]
fn ZSTD_cwksp_aligned64_alloc_size(size: usize) -> usize {
    ZSTD_cwksp_aligned_alloc_size(size, ZSTD_CWKSP_ALIGNMENT_BYTES as usize)
}

/// Returns the amount of additional space the cwksp must allocate for internal purposes
/// (currently only alignment).
#[inline]
fn ZSTD_cwksp_slack_space_required() -> usize {
    (ZSTD_CWKSP_ALIGNMENT_BYTES * 2) as usize
}

/// Return the number of additional bytes required to align a pointer to the given number of bytes.
/// alignBytes must be a power of two.
#[inline]
fn ZSTD_cwksp_bytes_to_align_ptr(ptr: *mut core::ffi::c_void, alignBytes: usize) -> usize {
    let alignBytesMask = alignBytes.wrapping_sub(1);

    alignBytes.wrapping_sub(ptr as usize & alignBytesMask) & alignBytesMask
}

/// Returns the initial value for allocStart which is used to determine the position from which
/// we can allocate from the end of the workspace.
#[inline]
fn ZSTD_cwksp_initialAllocStart(ws: &mut ZSTD_cwksp) -> *mut core::ffi::c_void {
    let mut endPtr = ws.workspaceEnd as *mut core::ffi::c_char;
    endPtr = endPtr.wrapping_sub(endPtr as usize % ZSTD_CWKSP_ALIGNMENT_BYTES as usize);
    endPtr as *mut core::ffi::c_void
}

/// Internal function. Do not use directly.
/// Reserves the given number of bytes within the aligned/buffer segment of the wksp,
/// which counts from the end of the wksp (as opposed to the object/table segment).
///
/// # Returns
///
/// A pointer to the beginning of that space.
#[inline]
fn ZSTD_cwksp_reserve_internal_buffer_space(
    ws: &mut ZSTD_cwksp,
    bytes: usize,
) -> *mut core::ffi::c_void {
    let alloc = ws.allocStart.wrapping_byte_sub(bytes);
    let bottom = ws.tableEnd;
    ZSTD_cwksp_assert_internal_consistency(ws);
    if alloc < bottom {
        ws.allocFailed = 1;
        return core::ptr::null_mut();
    }
    ws.tableValidEnd = Ord::min(ws.tableValidEnd, alloc);
    ws.allocStart = alloc;
    alloc
}

/// Moves the cwksp to the next phase, and does any necessary allocations.
/// cwksp initialization must necessarily go through each phase in order.
#[inline]
fn ZSTD_cwksp_internal_advance_phase(
    ws: &mut ZSTD_cwksp,
    phase: CwkspAllocPhase,
) -> Result<(), Error> {
    if phase > ws.phase {
        if ws.phase < CwkspAllocPhase::AlignedInitOnce && phase >= CwkspAllocPhase::AlignedInitOnce
        {
            ws.tableValidEnd = ws.objectEnd;
            ws.initOnceStart = ZSTD_cwksp_initialAllocStart(ws);
            let alloc = ws.objectEnd;
            let bytesToAlign =
                ZSTD_cwksp_bytes_to_align_ptr(alloc, ZSTD_CWKSP_ALIGNMENT_BYTES as usize);
            let objectEnd = alloc.wrapping_byte_add(bytesToAlign);
            if objectEnd > ws.workspaceEnd {
                return Err(Error::memory_allocation);
            }
            ws.objectEnd = objectEnd;
            ws.tableEnd = objectEnd;
            ws.tableValidEnd = Ord::max(ws.tableValidEnd, ws.tableEnd);
        }
        ws.phase = phase;
        ZSTD_cwksp_assert_internal_consistency(ws);
    }
    Ok(())
}

/// Returns whether this object/buffer/etc was allocated in this workspace.
#[inline]
fn ZSTD_cwksp_owns_buffer(ws: &ZSTD_cwksp, ptr: *const core::ffi::c_void) -> bool {
    !ptr.is_null()
        && ws.workspace <= ptr as *mut core::ffi::c_void
        && ptr < ws.workspaceEnd as *const core::ffi::c_void
}

/// Internal function. Do not use directly.
#[inline]
fn ZSTD_cwksp_reserve_internal(
    ws: &mut ZSTD_cwksp,
    bytes: usize,
    phase: CwkspAllocPhase,
) -> *mut core::ffi::c_void {
    if ZSTD_cwksp_internal_advance_phase(ws, phase).is_err() || bytes == 0 {
        return core::ptr::null_mut();
    }
    ZSTD_cwksp_reserve_internal_buffer_space(ws, bytes)
}

/// Reserves and returns unaligned memory.
#[inline]
fn ZSTD_cwksp_reserve_buffer(ws: &mut ZSTD_cwksp, bytes: usize) -> *mut u8 {
    ZSTD_cwksp_reserve_internal(ws, bytes, CwkspAllocPhase::Buffers) as *mut u8
}

/// Reserves and returns memory sized on and aligned on ZSTD_CWKSP_ALIGNMENT_BYTES (64 bytes).
/// This memory has been initialized at least once in the past. This doesn't mean it has been
/// initialized this time, and it might contain data from previous operations.
/// The main usage is for algorithms that might need read access into uninitialized memory.
/// The algorithm must maintain safety under these conditions and must make sure it doesn't leak
/// any of the past data (directly or in side channels).
#[inline]
unsafe fn ZSTD_cwksp_reserve_aligned_init_once(
    ws: &mut ZSTD_cwksp,
    bytes: usize,
) -> *mut core::ffi::c_void {
    let alignedBytes = ZSTD_cwksp_align(bytes, ZSTD_CWKSP_ALIGNMENT_BYTES as usize);
    let ptr = ZSTD_cwksp_reserve_internal(ws, alignedBytes, CwkspAllocPhase::AlignedInitOnce);
    if !ptr.is_null() && ptr < ws.initOnceStart {
        let initBytes = (ws.initOnceStart as *mut u8).offset_from_unsigned(ptr as *mut u8);
        ptr::write_bytes(ptr as *mut u8, 0, initBytes.min(alignedBytes));
        ws.initOnceStart = ptr;
    }
    ptr
}

/// Reserves and returns memory sized on and aligned on ZSTD_CWKSP_ALIGNMENT_BYTES (64 bytes).
#[inline]
fn ZSTD_cwksp_reserve_aligned64(ws: &mut ZSTD_cwksp, bytes: usize) -> *mut core::ffi::c_void {
    ZSTD_cwksp_reserve_internal(
        ws,
        ZSTD_cwksp_align(bytes, ZSTD_CWKSP_ALIGNMENT_BYTES as usize),
        CwkspAllocPhase::Aligned,
    )
}

/// Aligned on 64 bytes. These buffers have the special property that their values remain
/// constrained, allowing us to reuse them without memset()-ing them.
#[inline]
fn ZSTD_cwksp_reserve_table(ws: &mut ZSTD_cwksp, bytes: usize) -> *mut core::ffi::c_void {
    let phase = CwkspAllocPhase::AlignedInitOnce;
    if ws.phase < phase && ZSTD_cwksp_internal_advance_phase(ws, phase).is_err() {
        return core::ptr::null_mut();
    }
    let alloc = ws.tableEnd;
    let end = alloc.wrapping_byte_add(bytes);
    let top = ws.allocStart;
    ZSTD_cwksp_assert_internal_consistency(ws);
    if end > top {
        ws.allocFailed = 1;
        return core::ptr::null_mut();
    }
    ws.tableEnd = end;
    alloc
}

/// Aligned on sizeof(void*).
/// Note: should happen only once, at workspace first initialization
#[inline]
fn ZSTD_cwksp_reserve_object(ws: &mut ZSTD_cwksp, bytes: usize) -> *mut core::ffi::c_void {
    let roundedBytes = ZSTD_cwksp_align(bytes, size_of::<*mut core::ffi::c_void>());
    let alloc = ws.objectEnd;
    let end = alloc.wrapping_byte_add(roundedBytes);
    ZSTD_cwksp_assert_internal_consistency(ws);
    if ws.phase != CwkspAllocPhase::Objects || end > ws.workspaceEnd {
        ws.allocFailed = 1;
        return core::ptr::null_mut();
    }
    ws.objectEnd = end;
    ws.tableEnd = end;
    ws.tableValidEnd = end;
    alloc
}

#[inline]
fn ZSTD_cwksp_mark_tables_dirty(ws: &mut ZSTD_cwksp) {
    ws.tableValidEnd = ws.objectEnd;
    ZSTD_cwksp_assert_internal_consistency(ws);
}

#[inline]
fn ZSTD_cwksp_mark_tables_clean(ws: &mut ZSTD_cwksp) {
    ws.tableValidEnd = Ord::max(ws.tableValidEnd, ws.tableEnd);
    ZSTD_cwksp_assert_internal_consistency(ws);
}

/// Zero the part of the allocated tables not already marked clean.
#[inline]
unsafe fn ZSTD_cwksp_clean_tables(ws: &mut ZSTD_cwksp) {
    if ws.tableValidEnd < ws.tableEnd {
        ptr::write_bytes(
            ws.tableValidEnd,
            0,
            (ws.tableEnd as *mut u8).offset_from_unsigned(ws.tableValidEnd as *mut u8),
        );
    }
    ZSTD_cwksp_mark_tables_clean(ws);
}

/// Invalidates table allocations. All other allocations remain valid.
#[inline]
fn ZSTD_cwksp_clear_tables(ws: &mut ZSTD_cwksp) {
    ws.tableEnd = ws.objectEnd;
    ZSTD_cwksp_assert_internal_consistency(ws);
}

/// Invalidates all buffer, aligned, and table allocations.
/// Object allocations remain valid.
#[inline]
fn ZSTD_cwksp_clear(ws: &mut ZSTD_cwksp) {
    ws.tableEnd = ws.objectEnd;
    ws.allocStart = ZSTD_cwksp_initialAllocStart(ws);
    ws.allocFailed = 0;
    ws.phase = Ord::min(ws.phase, CwkspAllocPhase::AlignedInitOnce);
    ZSTD_cwksp_assert_internal_consistency(ws);
}

#[inline]
fn ZSTD_cwksp_sizeof(ws: &ZSTD_cwksp) -> usize {
    (ws.workspaceEnd as *mut u8).wrapping_offset_from(ws.workspace as *mut u8) as usize
}

/// The provided workspace takes ownership of the buffer [start, start+size).
/// Any existing values in the workspace are ignored (the previously managed buffer,
/// if present, must be separately freed).
#[inline]
fn ZSTD_cwksp_init(
    ws: &mut ZSTD_cwksp,
    start: *mut core::ffi::c_void,
    size: usize,
    isStatic: CwkspAllocKind,
) {
    ws.workspace = start;
    ws.workspaceEnd = start.wrapping_byte_add(size);
    ws.objectEnd = ws.workspace;
    ws.tableValidEnd = ws.objectEnd;
    ws.initOnceStart = ZSTD_cwksp_initialAllocStart(ws);
    ws.phase = CwkspAllocPhase::Objects;
    ws.isStatic = isStatic;
    ZSTD_cwksp_clear(ws);
    ws.workspaceOversizedDuration = 0;
    ZSTD_cwksp_assert_internal_consistency(ws);
}

#[inline]
unsafe fn ZSTD_cwksp_create(
    ws: &mut ZSTD_cwksp,
    size: usize,
    customMem: ZSTD_customMem,
) -> Result<(), Error> {
    let workspace = ZSTD_customMalloc(size, customMem);
    if workspace.is_null() {
        return Err(Error::memory_allocation);
    }
    ZSTD_cwksp_init(ws, workspace, size, CwkspAllocKind::Dynamic);
    Ok(())
}

/// `ws` must stay a raw pointer: `ZSTD_freeCDict` passes `&mut (*cdict).workspace` for a
/// cdict that lives *inside* the workspace this frees, and a `&mut` argument stays protected
/// for the whole call, so the deallocation would be UB (miri, `-Zmiri-tree-borrows`).
#[inline]
unsafe fn ZSTD_cwksp_free(ws: *mut ZSTD_cwksp, customMem: ZSTD_customMem) {
    let ptr = (*ws).workspace;
    let size = (*ws)
        .workspaceEnd
        .byte_offset_from_unsigned((*ws).workspace);
    ptr::write_bytes(ws as *mut u8, 0, size_of::<ZSTD_cwksp>());
    ZSTD_customFree(ptr, size, customMem);
}

/// Moves the management of a workspace from one cwksp to another. The src cwksp is left in an
/// invalid state (src must be re-init()'ed before it's used again).
#[inline]
fn ZSTD_cwksp_move(dst: &mut ZSTD_cwksp, src: &mut ZSTD_cwksp) {
    *dst = core::mem::take(src);
}

#[inline]
fn ZSTD_cwksp_reserve_failed(ws: &ZSTD_cwksp) -> bool {
    ws.allocFailed != 0
}

#[inline]
fn ZSTD_cwksp_available_space(ws: &mut ZSTD_cwksp) -> usize {
    (ws.allocStart as *mut u8).wrapping_offset_from(ws.tableEnd as *mut u8) as usize
}

#[inline]
fn ZSTD_cwksp_check_available(ws: &mut ZSTD_cwksp, additionalNeededSpace: usize) -> bool {
    ZSTD_cwksp_available_space(ws) >= additionalNeededSpace
}

#[inline]
fn ZSTD_cwksp_check_too_large(ws: &mut ZSTD_cwksp, additionalNeededSpace: usize) -> bool {
    ZSTD_cwksp_check_available(
        ws,
        additionalNeededSpace * ZSTD_WORKSPACETOOLARGE_FACTOR as usize,
    )
}

#[inline]
fn ZSTD_cwksp_check_wasteful(ws: &mut ZSTD_cwksp, additionalNeededSpace: usize) -> bool {
    ZSTD_cwksp_check_too_large(ws, additionalNeededSpace)
        && ws.workspaceOversizedDuration > ZSTD_WORKSPACETOOLARGE_MAXDURATION
}

#[inline]
fn ZSTD_cwksp_bump_oversized_duration(ws: &mut ZSTD_cwksp, additionalNeededSpace: usize) {
    if ZSTD_cwksp_check_too_large(ws, additionalNeededSpace) {
        ws.workspaceOversizedDuration += 1;
    } else {
        ws.workspaceOversizedDuration = 0;
    }
}

pub const ZSTD_LDM_DEFAULT_WINDOW_LOG: core::ffi::c_int = 27;

/// Maximum size of the hash table dedicated to find 3-bytes matches,
/// in log format, aka 17 => 1 << 17 == 128Ki positions.
/// This structure is only used in zstd_opt.
/// Since allocation is centralized for all strategies, it has to be known here.
/// The actual (selected) size of the hash table is then stored in ZSTD_MatchState_t.hashLog3,
/// so that zstd_opt.c doesn't need to know about this constant.
const ZSTD_HASHLOG3_MAX: u32 = 17;

// ------- Helper functions -------

/// Note that the result from this function is only valid for
/// the one-pass compression functions.
/// When employing the streaming mode,
/// if flushes are frequently altering the size of blocks,
/// the overhead from block headers can make the compressed data larger
/// than the return value of ZSTD_compressBound().
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_compressBound))]
pub extern "C" fn ZSTD_compressBound(srcSize: usize) -> usize {
    let r = if srcSize >= ZSTD_MAX_INPUT_SIZE {
        0
    } else {
        srcSize
            .wrapping_add(srcSize >> 8)
            .wrapping_add(if srcSize < (128 << 10) {
                (128usize << 10).wrapping_sub(srcSize) >> 11
            } else {
                0
            })
    };
    if r == 0 {
        return Error::srcSize_wrong.to_error_code();
    }
    r
}

// ------- Context memory management -------

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_createCCtx))]
pub unsafe extern "C" fn ZSTD_createCCtx() -> *mut ZSTD_CCtx {
    ZSTD_createCCtx_advanced(ZSTD_customMem::default())
}

pub(crate) unsafe fn ZSTD_initCCtx(cctx: *mut ZSTD_CCtx, memManager: ZSTD_customMem) {
    ptr::write_bytes(cctx as *mut u8, 0, size_of::<ZSTD_CCtx>());
    (*cctx).customMem = memManager;
    (*cctx).bmi2 = ZSTD_cpuSupportsBmi2() as _;
    let _err = ZSTD_CCtx_reset(cctx, ZSTD_ResetDirective::ZSTD_reset_parameters);
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_createCCtx_advanced))]
pub unsafe extern "C" fn ZSTD_createCCtx_advanced(customMem: ZSTD_customMem) -> *mut ZSTD_CCtx {
    let cctx = ZSTD_customMalloc(size_of::<ZSTD_CCtx>(), customMem) as *mut ZSTD_CCtx;
    if cctx.is_null() {
        return core::ptr::null_mut();
    }
    ZSTD_initCCtx(cctx, customMem);
    cctx
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_initStaticCCtx))]
pub unsafe extern "C" fn ZSTD_initStaticCCtx(
    workspace: *mut core::ffi::c_void,
    workspaceSize: usize,
) -> *mut ZSTD_CCtx {
    let mut ws = ZSTD_cwksp::default();
    if workspaceSize <= size_of::<ZSTD_CCtx>() {
        // minimum size
        return core::ptr::null_mut();
    }
    if workspace as usize & 7 != 0 {
        // must be 8-aligned
        return core::ptr::null_mut();
    }
    ZSTD_cwksp_init(&mut ws, workspace, workspaceSize, CwkspAllocKind::Static);

    let cctx = ZSTD_cwksp_reserve_object(&mut ws, size_of::<ZSTD_CCtx>()) as *mut ZSTD_CCtx;
    if cctx.is_null() {
        return core::ptr::null_mut();
    }

    ptr::write_bytes(cctx as *mut u8, 0, size_of::<ZSTD_CCtx>());
    ZSTD_cwksp_move(&mut (*cctx).workspace, &mut ws);
    (*cctx).staticSize = workspaceSize;

    // statically sized space. tmpWorkspace never moves (but prev/next block swap places)
    if !ZSTD_cwksp_check_available(
        &mut (*cctx).workspace,
        ((8usize << 10) + 512)
            .wrapping_add(size_of::<core::ffi::c_uint>().wrapping_mul(MaxSeq + 2))
            .max(ZSTD_SLIPBLOCK_WORKSPACESIZE)
            .wrapping_add(2usize.wrapping_mul(size_of::<ZSTD_compressedBlockState_t>())),
    ) {
        return core::ptr::null_mut();
    }
    (*cctx).blockState.prevCBlock = ZSTD_cwksp_reserve_object(
        &mut (*cctx).workspace,
        size_of::<ZSTD_compressedBlockState_t>(),
    ) as *mut ZSTD_compressedBlockState_t;
    (*cctx).blockState.nextCBlock = ZSTD_cwksp_reserve_object(
        &mut (*cctx).workspace,
        size_of::<ZSTD_compressedBlockState_t>(),
    ) as *mut ZSTD_compressedBlockState_t;
    (*cctx).tmpWorkspace = ZSTD_cwksp_reserve_object(
        &mut (*cctx).workspace,
        ((8usize << 10) + 512)
            .wrapping_add(size_of::<core::ffi::c_uint>().wrapping_mul(MaxSeq + 2))
            .max(ZSTD_SLIPBLOCK_WORKSPACESIZE),
    );
    (*cctx).tmpWkspSize = ((8usize << 10) + 512)
        .wrapping_add(size_of::<core::ffi::c_uint>().wrapping_mul(MaxSeq + 2))
        .max(ZSTD_SLIPBLOCK_WORKSPACESIZE);
    (*cctx).bmi2 = ZSTD_cpuSupportsBmi2() as _;
    cctx
}

/// Clears and frees all of the dictionaries in the CCtx.
unsafe fn ZSTD_clearAllDicts(cctx: *mut ZSTD_CCtx) {
    ZSTD_customFree(
        (*cctx).localDict.dictBuffer,
        (*cctx).localDict.dictSize,
        (*cctx).customMem,
    );
    ZSTD_freeCDict((*cctx).localDict.cdict);
    ptr::write_bytes(
        &mut (*cctx).localDict as *mut ZSTD_localDict as *mut u8,
        0,
        size_of::<ZSTD_localDict>(),
    );
    ptr::write_bytes(
        &mut (*cctx).prefixDict as *mut ZSTD_prefixDict as *mut u8,
        0,
        size_of::<ZSTD_prefixDict>(),
    );
    (*cctx).cdict = core::ptr::null();
}

unsafe fn ZSTD_sizeof_localDict(dict: ZSTD_localDict) -> usize {
    let bufferSize = if !dict.dictBuffer.is_null() {
        dict.dictSize
    } else {
        0
    };
    let cdictSize = ZSTD_sizeof_CDict(dict.cdict);
    bufferSize.wrapping_add(cdictSize)
}

pub(crate) unsafe fn ZSTD_freeCCtxContent(cctx: *mut ZSTD_CCtx) {
    ZSTD_clearAllDicts(cctx);
    ZSTDMT_freeCCtx((*cctx).mtctx);
    (*cctx).mtctx = core::ptr::null_mut();
    ZSTD_cwksp_free(&mut (*cctx).workspace, (*cctx).customMem);
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_freeCCtx))]
pub unsafe extern "C" fn ZSTD_freeCCtx(cctx: *mut ZSTD_CCtx) -> usize {
    if cctx.is_null() {
        return 0;
    }
    if (*cctx).staticSize != 0 {
        return Error::memory_allocation.to_error_code();
    }

    let cctxInWorkspace =
        ZSTD_cwksp_owns_buffer(&(*cctx).workspace, cctx as *const core::ffi::c_void);
    ZSTD_freeCCtxContent(cctx);
    if !cctxInWorkspace {
        ZSTD_customFree(
            cctx as *mut core::ffi::c_void,
            size_of::<ZSTD_CCtx>(),
            (*cctx).customMem,
        );
    }

    0
}

unsafe fn ZSTD_sizeof_mtctx(cctx: *const ZSTD_CCtx) -> usize {
    ZSTDMT_sizeof_CCtx((*cctx).mtctx)
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_sizeof_CCtx))]
pub unsafe extern "C" fn ZSTD_sizeof_CCtx(cctx: *const ZSTD_CCtx) -> usize {
    if cctx.is_null() {
        return 0;
    }

    // cctx may be in the workspace
    (if (*cctx).workspace.workspace == cctx as *mut core::ffi::c_void {
        0
    } else {
        size_of::<ZSTD_CCtx>()
    })
    .wrapping_add(ZSTD_cwksp_sizeof(&(*cctx).workspace))
    .wrapping_add(ZSTD_sizeof_localDict((*cctx).localDict))
    .wrapping_add(ZSTD_sizeof_mtctx(cctx))
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_sizeof_CStream))]
pub unsafe extern "C" fn ZSTD_sizeof_CStream(zcs: *const ZSTD_CStream) -> usize {
    ZSTD_sizeof_CCtx(zcs)
}

/// Returns true if the strategy supports using a row based matchfinder
fn ZSTD_rowMatchFinderSupported(strategy: ZSTD_strategy) -> bool {
    (ZSTD_greedy..=ZSTD_lazy2).contains(&strategy)
}

/// Returns true if the strategy and useRowMatchFinder mode indicate that we will use the row based
/// matchfinder for this compression.
fn ZSTD_rowMatchFinderUsed(strategy: ZSTD_strategy, mode: ParamSwitch) -> bool {
    ZSTD_rowMatchFinderSupported(strategy) && mode == ParamSwitch::Enable
}

/// Returns row matchfinder usage given an initial mode and cParams
fn ZSTD_resolveRowMatchFinderMode(
    mut mode: ParamSwitch,
    cParams: &ZSTD_compressionParameters,
) -> ParamSwitch {
    let kWindowLogLowerBound = 14;
    if mode != ParamSwitch::Auto {
        // if requested enabled, but no SIMD, we still will use row matchfinder
        return mode;
    }
    mode = ParamSwitch::Disable;
    if !ZSTD_rowMatchFinderSupported(cParams.strategy) {
        return mode;
    }
    if cParams.windowLog > kWindowLogLowerBound {
        mode = ParamSwitch::Enable;
    }
    mode
}

/// Returns block splitter usage (generally speaking, when using slower/stronger compression modes)
fn ZSTD_resolveBlockSplitterMode(
    mode: ParamSwitch,
    cParams: &ZSTD_compressionParameters,
) -> ParamSwitch {
    if mode != ParamSwitch::Auto {
        return mode;
    }

    if cParams.strategy >= ZSTD_btopt && cParams.windowLog >= 17 {
        ParamSwitch::Enable
    } else {
        ParamSwitch::Disable
    }
}

/// Returns 1 if the arguments indicate that we should allocate a chainTable, 0 otherwise.
///
/// We always should allocate a chaintable if we are allocating a matchstate for a DDS dictionary
/// matchstate. We do not allocate a chaintable if we are using ZSTD_fast, or are using the
/// row-based matchfinder.
fn ZSTD_allocateChainTable(
    strategy: ZSTD_strategy,
    useRowMatchFinder: ParamSwitch,
    forDDSDict: bool,
) -> bool {
    forDDSDict || strategy != ZSTD_fast && !ZSTD_rowMatchFinderUsed(strategy, useRowMatchFinder)
}

/// Returns Enable if compression parameters are such that we should
/// enable long distance matching (wlog >= 27, strategy >= btopt).
/// Returns Disable otherwise.
fn ZSTD_resolveEnableLdm(mode: ParamSwitch, cParams: &ZSTD_compressionParameters) -> ParamSwitch {
    if mode != ParamSwitch::Auto {
        return mode;
    }

    if cParams.strategy >= ZSTD_btopt && cParams.windowLog >= 27 {
        ParamSwitch::Enable
    } else {
        ParamSwitch::Disable
    }
}

fn ZSTD_resolveExternalSequenceValidation(mode: core::ffi::c_int) -> core::ffi::c_int {
    mode
}

/// Resolves maxBlockSize to the default if no value is present.
fn ZSTD_resolveMaxBlockSize(maxBlockSize: usize) -> usize {
    if maxBlockSize == 0 {
        ZSTD_BLOCKSIZE_MAX as usize
    } else {
        maxBlockSize
    }
}

fn ZSTD_resolveExternalRepcodeSearch(value: ParamSwitch, cLevel: core::ffi::c_int) -> ParamSwitch {
    if value != ParamSwitch::Auto {
        return value;
    }

    if cLevel < 10 {
        ParamSwitch::Disable
    } else {
        ParamSwitch::Enable
    }
}

impl ZSTD_compressionParameters {
    /// Returns true if compression parameters are such that CDict hashtable and chaintable indices
    /// are tagged. If so, the tags need to be removed in [`ZSTD_resetCCtx_byCopyingCDict`].
    fn cdict_indices_are_tagged(&self) -> bool {
        self.strategy == ZSTD_fast || self.strategy == ZSTD_dfast
    }

    fn supports_dedicated_dict_search(&self) -> bool {
        (ZSTD_greedy..=ZSTD_lazy2).contains(&self.strategy)
            && self.hashLog > self.chainLog
            && self.chainLog <= 24
    }

    /// Reverses the adjustment applied to cparams when enabling dedicated dict
    /// search. This is used to recover the params set to be used in the working
    /// context. (Otherwise, those tables would also grow.)
    fn revert_dedicated_dict_search(&mut self) {
        if (ZSTD_greedy..=ZSTD_lazy2).contains(&self.strategy) {
            self.hashLog = self.hashLog.wrapping_sub(ZSTD_LAZY_DDSS_BUCKET_LOG);
            self.hashLog = Ord::max(self.hashLog, ZSTD_HASHLOG_MIN as core::ffi::c_uint);
        }
    }
}

impl From<ZSTD_compressionParameters> for ZSTD_CCtx_params {
    fn from(cParams: ZSTD_compressionParameters) -> Self {
        // should not matter, as all cParams are presumed properly defined
        let mut cctxParams = ZSTD_CCtx_params_s::new(ZSTD_CLEVEL_DEFAULT);
        cctxParams.cParams = cParams;

        // Adjust advanced params according to cParams
        cctxParams.ldmParams.enableLdm =
            ZSTD_resolveEnableLdm(cctxParams.ldmParams.enableLdm, &cParams);
        if cctxParams.ldmParams.enableLdm == ParamSwitch::Enable {
            cctxParams.ldmParams.adjust_parameters(&cParams);
        }
        cctxParams.postBlockSplitter =
            ZSTD_resolveBlockSplitterMode(cctxParams.postBlockSplitter, &cParams);
        cctxParams.useRowMatchFinder =
            ZSTD_resolveRowMatchFinderMode(cctxParams.useRowMatchFinder, &cParams);
        cctxParams.validateSequences =
            ZSTD_resolveExternalSequenceValidation(cctxParams.validateSequences);
        cctxParams.maxBlockSize = ZSTD_resolveMaxBlockSize(cctxParams.maxBlockSize);
        cctxParams.searchForExternalRepcodes = ZSTD_resolveExternalRepcodeSearch(
            cctxParams.searchForExternalRepcodes,
            cctxParams.compressionLevel,
        );

        cctxParams
    }
}

unsafe fn ZSTD_createCCtxParams_advanced(customMem: ZSTD_customMem) -> *mut ZSTD_CCtx_params {
    let params =
        ZSTD_customCalloc(size_of::<ZSTD_CCtx_params>(), customMem) as *mut ZSTD_CCtx_params;
    if params.is_null() {
        return core::ptr::null_mut();
    }
    ZSTD_CCtxParams_init(params, ZSTD_CLEVEL_DEFAULT);
    (*params).customMem = customMem;
    params
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_createCCtxParams))]
pub unsafe extern "C" fn ZSTD_createCCtxParams() -> *mut ZSTD_CCtx_params {
    ZSTD_createCCtxParams_advanced(ZSTD_customMem::default())
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_freeCCtxParams))]
pub unsafe extern "C" fn ZSTD_freeCCtxParams(params: *mut ZSTD_CCtx_params) -> usize {
    if params.is_null() {
        return 0;
    }
    ZSTD_customFree(
        params as *mut core::ffi::c_void,
        size_of::<ZSTD_CCtx_params>(),
        (*params).customMem,
    );
    0
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_CCtxParams_reset))]
pub unsafe extern "C" fn ZSTD_CCtxParams_reset(params: *mut ZSTD_CCtx_params) -> usize {
    ZSTD_CCtxParams_init(params, ZSTD_CLEVEL_DEFAULT)
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_CCtxParams_init))]
pub unsafe extern "C" fn ZSTD_CCtxParams_init(
    cctxParams: *mut ZSTD_CCtx_params,
    compressionLevel: core::ffi::c_int,
) -> usize {
    if cctxParams.is_null() {
        return Error::GENERIC.to_error_code();
    }
    *cctxParams = ZSTD_CCtx_params::new(compressionLevel);

    0
}

pub const ZSTD_NO_CLEVEL: core::ffi::c_int = 0;

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_CCtxParams_init_advanced))]
pub unsafe extern "C" fn ZSTD_CCtxParams_init_advanced(
    cctxParams: *mut ZSTD_CCtx_params,
    params: ZSTD_parameters,
) -> usize {
    if cctxParams.is_null() {
        return Error::GENERIC.to_error_code();
    }
    let err_code = ZSTD_checkCParams(params.cParams);
    if ERR_isError(err_code) {
        return err_code;
    }
    *cctxParams = ZSTD_CCtx_params_s::new_internal(&params, ZSTD_NO_CLEVEL);

    0
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_cParam_getBounds))]
pub extern "C" fn ZSTD_cParam_getBounds(param: ZSTD_cParameter) -> ZSTD_bounds {
    ZSTD_bounds::from(param)
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_CCtx_setParameter))]
pub unsafe extern "C" fn ZSTD_CCtx_setParameter(
    cctx: *mut ZSTD_CCtx,
    param: ZSTD_cParameter,
    value: core::ffi::c_int,
) -> usize {
    (*cctx)
        .set_parameter(param, value)
        .unwrap_or_else(|e| e.to_error_code())
}

impl ZSTD_CCtx {
    /// Safe version of [`ZSTD_CCtx_setParameter`]
    pub fn set_parameter(
        &mut self,
        param: ZSTD_cParameter,
        value: core::ffi::c_int,
    ) -> Result<usize, Error> {
        if self.streamStage != StreamStage::Init {
            if param.is_update_authorized() {
                self.cParamsChanged = 1;
            } else {
                return Err(Error::stage_wrong);
            }
        }

        match param {
            ZSTD_cParameter::ZSTD_c_nbWorkers => {
                if value != 0 && self.staticSize != 0 {
                    Err(Error::parameter_unsupported)
                } else {
                    self.requestedParams.set_parameter(param, value)
                }
            }

            ZSTD_cParameter::ZSTD_c_compressionLevel
            | ZSTD_cParameter::ZSTD_c_windowLog
            | ZSTD_cParameter::ZSTD_c_hashLog
            | ZSTD_cParameter::ZSTD_c_chainLog
            | ZSTD_cParameter::ZSTD_c_searchLog
            | ZSTD_cParameter::ZSTD_c_minMatch
            | ZSTD_cParameter::ZSTD_c_targetLength
            | ZSTD_cParameter::ZSTD_c_strategy
            | ZSTD_cParameter::ZSTD_c_ldmHashRateLog
            | ZSTD_cParameter::ZSTD_c_format
            | ZSTD_cParameter::ZSTD_c_contentSizeFlag
            | ZSTD_cParameter::ZSTD_c_checksumFlag
            | ZSTD_cParameter::ZSTD_c_dictIDFlag
            | ZSTD_cParameter::ZSTD_c_forceMaxWindow
            | ZSTD_cParameter::ZSTD_c_forceAttachDict
            | ZSTD_cParameter::ZSTD_c_literalCompressionMode
            | ZSTD_cParameter::ZSTD_c_jobSize
            | ZSTD_cParameter::ZSTD_c_overlapLog
            | ZSTD_cParameter::ZSTD_c_rsyncable
            | ZSTD_cParameter::ZSTD_c_enableDedicatedDictSearch
            | ZSTD_cParameter::ZSTD_c_enableLongDistanceMatching
            | ZSTD_cParameter::ZSTD_c_ldmHashLog
            | ZSTD_cParameter::ZSTD_c_ldmMinMatch
            | ZSTD_cParameter::ZSTD_c_ldmBucketSizeLog
            | ZSTD_cParameter::ZSTD_c_targetCBlockSize
            | ZSTD_cParameter::ZSTD_c_srcSizeHint
            | ZSTD_cParameter::ZSTD_c_stableInBuffer
            | ZSTD_cParameter::ZSTD_c_stableOutBuffer
            | ZSTD_cParameter::ZSTD_c_blockDelimiters
            | ZSTD_cParameter::ZSTD_c_validateSequences
            | ZSTD_cParameter::ZSTD_c_splitAfterSequences
            | ZSTD_cParameter::ZSTD_c_blockSplitterLevel
            | ZSTD_cParameter::ZSTD_c_useRowMatchFinder
            | ZSTD_cParameter::ZSTD_c_deterministicRefPrefix
            | ZSTD_cParameter::ZSTD_c_prefetchCDictTables
            | ZSTD_cParameter::ZSTD_c_enableSeqProducerFallback
            | ZSTD_cParameter::ZSTD_c_maxBlockSize
            | ZSTD_cParameter::ZSTD_c_repcodeResolution => {
                self.requestedParams.set_parameter(param, value)
            }

            _ => Err(Error::parameter_unsupported),
        }
    }

    /// Safe version of [`ZSTD_CCtx_setCParams`]
    pub fn set_cparams(&mut self, cparams: ZSTD_compressionParameters) -> Result<(), Error> {
        cparams.check_bounds()?;
        self.set_parameter(
            ZSTD_cParameter::ZSTD_c_windowLog,
            cparams.windowLog as core::ffi::c_int,
        )?;
        self.set_parameter(
            ZSTD_cParameter::ZSTD_c_chainLog,
            cparams.chainLog as core::ffi::c_int,
        )?;
        self.set_parameter(
            ZSTD_cParameter::ZSTD_c_hashLog,
            cparams.hashLog as core::ffi::c_int,
        )?;
        self.set_parameter(
            ZSTD_cParameter::ZSTD_c_searchLog,
            cparams.searchLog as core::ffi::c_int,
        )?;
        self.set_parameter(
            ZSTD_cParameter::ZSTD_c_minMatch,
            cparams.minMatch as core::ffi::c_int,
        )?;
        self.set_parameter(
            ZSTD_cParameter::ZSTD_c_targetLength,
            cparams.targetLength as core::ffi::c_int,
        )?;
        self.set_parameter(
            ZSTD_cParameter::ZSTD_c_strategy,
            cparams.strategy as core::ffi::c_int,
        )?;
        Ok(())
    }

    /// Safe version of [`ZSTD_CCtx_setFParams`]
    pub fn set_fparams(&mut self, fparams: ZSTD_frameParameters) -> Result<(), Error> {
        self.set_parameter(
            ZSTD_cParameter::ZSTD_c_contentSizeFlag,
            core::ffi::c_int::from(fparams.contentSizeFlag != 0),
        )?;
        self.set_parameter(
            ZSTD_cParameter::ZSTD_c_checksumFlag,
            core::ffi::c_int::from(fparams.checksumFlag != 0),
        )?;
        self.set_parameter(
            ZSTD_cParameter::ZSTD_c_dictIDFlag,
            core::ffi::c_int::from(fparams.noDictIDFlag == 0),
        )?;
        Ok(())
    }

    /// Safe version of [`ZSTD_CCtx_setParams`]
    pub fn set_params(&mut self, params: ZSTD_parameters) -> Result<(), Error> {
        params.cParams.check_bounds()?;
        self.set_fparams(params.fParams)?;
        self.set_cparams(params.cParams)?;
        Ok(())
    }
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_CCtxParams_setParameter))]
pub unsafe extern "C" fn ZSTD_CCtxParams_setParameter(
    CCtxParams: *mut ZSTD_CCtx_params,
    param: ZSTD_cParameter,
    value: core::ffi::c_int,
) -> usize {
    (*CCtxParams)
        .set_parameter(param, value)
        .unwrap_or_else(|e| e.to_error_code())
}

impl ZSTD_CCtx_params {
    /// Safe version of [`ZSTD_CCtxParams_setParameter`]
    pub fn set_parameter(
        &mut self,
        param: ZSTD_cParameter,
        mut value: core::ffi::c_int,
    ) -> Result<usize, Error> {
        match param {
            ZSTD_cParameter::ZSTD_c_format => {
                let Ok(format) = Format::try_from(value) else {
                    return Err(Error::parameter_outOfBound);
                };

                self.format = format;
                Ok(self.format as usize)
            }
            ZSTD_cParameter::ZSTD_c_compressionLevel => {
                param.clamp_bounds(&mut value)?;
                self.compressionLevel = if value == 0 {
                    ZSTD_CLEVEL_DEFAULT
                } else {
                    value
                };
                if self.compressionLevel >= 0 {
                    return Ok(self.compressionLevel as usize);
                }
                Ok(0)
            }
            ZSTD_cParameter::ZSTD_c_windowLog => {
                if value != 0 {
                    ZSTD_cParameter::ZSTD_c_windowLog.check_bounds(value)?;
                }
                self.cParams.windowLog = value as u32;
                Ok(self.cParams.windowLog as usize)
            }
            ZSTD_cParameter::ZSTD_c_hashLog => {
                if value != 0 {
                    ZSTD_cParameter::ZSTD_c_hashLog.check_bounds(value)?;
                }
                self.cParams.hashLog = value as u32;
                Ok(self.cParams.hashLog as usize)
            }
            ZSTD_cParameter::ZSTD_c_chainLog => {
                if value != 0 {
                    ZSTD_cParameter::ZSTD_c_chainLog.check_bounds(value)?;
                }
                self.cParams.chainLog = value as u32;
                Ok(self.cParams.chainLog as usize)
            }
            ZSTD_cParameter::ZSTD_c_searchLog => {
                if value != 0 {
                    ZSTD_cParameter::ZSTD_c_searchLog.check_bounds(value)?;
                }
                self.cParams.searchLog = value as u32;
                Ok(value as usize)
            }
            ZSTD_cParameter::ZSTD_c_minMatch => {
                if value != 0 {
                    ZSTD_cParameter::ZSTD_c_minMatch.check_bounds(value)?;
                }
                self.cParams.minMatch = value as u32;
                Ok(self.cParams.minMatch as usize)
            }
            ZSTD_cParameter::ZSTD_c_targetLength => {
                ZSTD_cParameter::ZSTD_c_targetLength.check_bounds(value)?;
                self.cParams.targetLength = value as u32;
                Ok(self.cParams.targetLength as usize)
            }
            ZSTD_cParameter::ZSTD_c_strategy => {
                if value != 0 {
                    ZSTD_cParameter::ZSTD_c_strategy.check_bounds(value)?;
                }
                self.cParams.strategy = value as ZSTD_strategy;
                Ok(self.cParams.strategy as usize)
            }
            ZSTD_cParameter::ZSTD_c_contentSizeFlag => {
                self.fParams.contentSizeFlag = core::ffi::c_int::from(value != 0);
                Ok(self.fParams.contentSizeFlag as usize)
            }
            ZSTD_cParameter::ZSTD_c_checksumFlag => {
                self.fParams.checksumFlag = core::ffi::c_int::from(value != 0);
                Ok(self.fParams.checksumFlag as usize)
            }
            ZSTD_cParameter::ZSTD_c_dictIDFlag => {
                self.fParams.noDictIDFlag = core::ffi::c_int::from(value == 0);
                Ok(usize::from(self.fParams.noDictIDFlag == 0))
            }
            ZSTD_cParameter::ZSTD_c_forceMaxWindow => {
                self.forceWindow = core::ffi::c_int::from(value != 0);
                Ok(self.forceWindow as usize)
            }
            ZSTD_cParameter::ZSTD_c_forceAttachDict => {
                let Ok(pref) = ZSTD_dictAttachPref_e::try_from(value) else {
                    return Err(Error::parameter_outOfBound);
                };
                self.attachDictPref = pref;
                Ok(self.attachDictPref.0 as usize)
            }
            ZSTD_cParameter::ZSTD_c_literalCompressionMode => {
                let Ok(lcm) = ParamSwitch::try_from(value) else {
                    return Err(Error::parameter_outOfBound);
                };
                self.literalCompressionMode = lcm;
                Ok(self.literalCompressionMode as usize)
            }
            ZSTD_cParameter::ZSTD_c_nbWorkers => {
                param.clamp_bounds(&mut value)?;
                self.nbWorkers = value;
                Ok(self.nbWorkers as usize)
            }
            ZSTD_cParameter::ZSTD_c_jobSize => {
                if value != 0 && value < ZSTDMT_JOBSIZE_MIN {
                    value = ZSTDMT_JOBSIZE_MIN;
                }
                param.clamp_bounds(&mut value)?;
                self.jobSize = value as usize;
                Ok(self.jobSize)
            }
            ZSTD_cParameter::ZSTD_c_overlapLog => {
                param.clamp_bounds(&mut value)?;
                self.overlapLog = value;
                Ok(self.overlapLog as usize)
            }
            ZSTD_cParameter::ZSTD_c_rsyncable => {
                param.clamp_bounds(&mut value)?;
                self.rsyncable = value;
                Ok(self.rsyncable as usize)
            }
            ZSTD_cParameter::ZSTD_c_enableDedicatedDictSearch => {
                self.enableDedicatedDictSearch = core::ffi::c_int::from(value != 0);
                Ok(self.enableDedicatedDictSearch as usize)
            }
            ZSTD_cParameter::ZSTD_c_enableLongDistanceMatching => {
                let Ok(value) = ParamSwitch::try_from(value) else {
                    return Err(Error::parameter_outOfBound);
                };
                self.ldmParams.enableLdm = value;
                Ok(self.ldmParams.enableLdm as usize)
            }
            ZSTD_cParameter::ZSTD_c_ldmHashLog => {
                if value != 0 {
                    ZSTD_cParameter::ZSTD_c_ldmHashLog.check_bounds(value)?;
                }
                self.ldmParams.hashLog = value as u32;
                Ok(self.ldmParams.hashLog as usize)
            }
            ZSTD_cParameter::ZSTD_c_ldmMinMatch => {
                if value != 0 {
                    ZSTD_cParameter::ZSTD_c_ldmMinMatch.check_bounds(value)?;
                }
                self.ldmParams.minMatchLength = value as u32;
                Ok(self.ldmParams.minMatchLength as usize)
            }
            ZSTD_cParameter::ZSTD_c_ldmBucketSizeLog => {
                if value != 0 {
                    ZSTD_cParameter::ZSTD_c_ldmBucketSizeLog.check_bounds(value)?;
                }
                self.ldmParams.bucketSizeLog = value as u32;
                Ok(self.ldmParams.bucketSizeLog as usize)
            }
            ZSTD_cParameter::ZSTD_c_ldmHashRateLog => {
                if value != 0 {
                    ZSTD_cParameter::ZSTD_c_ldmHashRateLog.check_bounds(value)?;
                }
                self.ldmParams.hashRateLog = value as u32;
                Ok(self.ldmParams.hashRateLog as usize)
            }
            ZSTD_cParameter::ZSTD_c_targetCBlockSize => {
                if value != 0 {
                    value = value.max(ZSTD_TARGETCBLOCKSIZE_MIN);
                    ZSTD_cParameter::ZSTD_c_targetCBlockSize.check_bounds(value)?;
                }
                self.targetCBlockSize = value as usize;
                Ok(self.targetCBlockSize)
            }
            ZSTD_cParameter::ZSTD_c_srcSizeHint => {
                if value != 0 {
                    ZSTD_cParameter::ZSTD_c_srcSizeHint.check_bounds(value)?;
                }
                self.srcSizeHint = value;
                Ok(self.srcSizeHint as usize)
            }
            ZSTD_cParameter::ZSTD_c_stableInBuffer => {
                ZSTD_cParameter::ZSTD_c_stableInBuffer.check_bounds(value)?;
                self.inBufferMode = value as ZSTD_bufferMode_e;
                Ok(self.inBufferMode as usize)
            }
            ZSTD_cParameter::ZSTD_c_stableOutBuffer => {
                ZSTD_cParameter::ZSTD_c_stableOutBuffer.check_bounds(value)?;
                self.outBufferMode = value as ZSTD_bufferMode_e;
                Ok(self.outBufferMode as usize)
            }
            ZSTD_cParameter::ZSTD_c_blockDelimiters => {
                ZSTD_cParameter::ZSTD_c_blockDelimiters.check_bounds(value)?;
                self.blockDelimiters = value as ZSTD_SequenceFormat_e;
                Ok(self.blockDelimiters as usize)
            }
            ZSTD_cParameter::ZSTD_c_validateSequences => {
                ZSTD_cParameter::ZSTD_c_validateSequences.check_bounds(value)?;
                self.validateSequences = value;
                Ok(self.validateSequences as usize)
            }
            ZSTD_cParameter::ZSTD_c_splitAfterSequences => {
                let Ok(value) = ParamSwitch::try_from(value) else {
                    return Err(Error::parameter_outOfBound);
                };
                self.postBlockSplitter = value;
                Ok(self.postBlockSplitter as usize)
            }
            ZSTD_cParameter::ZSTD_c_blockSplitterLevel => {
                ZSTD_cParameter::ZSTD_c_blockSplitterLevel.check_bounds(value)?;
                self.preBlockSplitter_level = value;
                Ok(self.preBlockSplitter_level as usize)
            }
            ZSTD_cParameter::ZSTD_c_useRowMatchFinder => {
                let Ok(value) = ParamSwitch::try_from(value) else {
                    return Err(Error::parameter_outOfBound);
                };
                self.useRowMatchFinder = value;
                Ok(self.useRowMatchFinder as usize)
            }
            ZSTD_cParameter::ZSTD_c_deterministicRefPrefix => {
                ZSTD_cParameter::ZSTD_c_deterministicRefPrefix.check_bounds(value)?;
                self.deterministicRefPrefix = core::ffi::c_int::from(value != 0);
                Ok(self.deterministicRefPrefix as usize)
            }
            ZSTD_cParameter::ZSTD_c_prefetchCDictTables => {
                let Ok(value) = ParamSwitch::try_from(value) else {
                    return Err(Error::parameter_outOfBound);
                };
                self.prefetchCDictTables = value;
                Ok(self.prefetchCDictTables as usize)
            }
            ZSTD_cParameter::ZSTD_c_enableSeqProducerFallback => {
                ZSTD_cParameter::ZSTD_c_enableSeqProducerFallback.check_bounds(value)?;
                self.enableMatchFinderFallback = value;
                Ok(self.enableMatchFinderFallback as usize)
            }
            ZSTD_cParameter::ZSTD_c_maxBlockSize => {
                if value != 0 {
                    ZSTD_cParameter::ZSTD_c_maxBlockSize.check_bounds(value)?;
                }
                self.maxBlockSize = value as usize;
                Ok(self.maxBlockSize)
            }
            ZSTD_cParameter::ZSTD_c_repcodeResolution => {
                let Ok(value) = ParamSwitch::try_from(value) else {
                    return Err(Error::parameter_outOfBound);
                };
                self.searchForExternalRepcodes = value;
                Ok(self.searchForExternalRepcodes as usize)
            }
            _ => Err(Error::parameter_unsupported),
        }
    }
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_CCtx_getParameter))]
pub unsafe extern "C" fn ZSTD_CCtx_getParameter(
    cctx: *const ZSTD_CCtx,
    param: ZSTD_cParameter,
    value: *mut core::ffi::c_int,
) -> usize {
    ZSTD_CCtxParams_getParameter(&(*cctx).requestedParams, param, value)
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_CCtxParams_getParameter))]
pub unsafe extern "C" fn ZSTD_CCtxParams_getParameter(
    CCtxParams: *const ZSTD_CCtx_params,
    param: ZSTD_cParameter,
    value: *mut core::ffi::c_int,
) -> usize {
    match param {
        ZSTD_cParameter::ZSTD_c_format => {
            *value = (*CCtxParams).format as core::ffi::c_int;
        }
        ZSTD_cParameter::ZSTD_c_compressionLevel => {
            *value = (*CCtxParams).compressionLevel;
        }
        ZSTD_cParameter::ZSTD_c_windowLog => {
            *value = (*CCtxParams).cParams.windowLog as core::ffi::c_int;
        }
        ZSTD_cParameter::ZSTD_c_hashLog => {
            *value = (*CCtxParams).cParams.hashLog as core::ffi::c_int;
        }
        ZSTD_cParameter::ZSTD_c_chainLog => {
            *value = (*CCtxParams).cParams.chainLog as core::ffi::c_int;
        }
        ZSTD_cParameter::ZSTD_c_searchLog => {
            *value = (*CCtxParams).cParams.searchLog as core::ffi::c_int;
        }
        ZSTD_cParameter::ZSTD_c_minMatch => {
            *value = (*CCtxParams).cParams.minMatch as core::ffi::c_int;
        }
        ZSTD_cParameter::ZSTD_c_targetLength => {
            *value = (*CCtxParams).cParams.targetLength as core::ffi::c_int;
        }
        ZSTD_cParameter::ZSTD_c_strategy => {
            *value = (*CCtxParams).cParams.strategy as core::ffi::c_int;
        }
        ZSTD_cParameter::ZSTD_c_contentSizeFlag => {
            *value = (*CCtxParams).fParams.contentSizeFlag;
        }
        ZSTD_cParameter::ZSTD_c_checksumFlag => {
            *value = (*CCtxParams).fParams.checksumFlag;
        }
        ZSTD_cParameter::ZSTD_c_dictIDFlag => {
            *value = core::ffi::c_int::from((*CCtxParams).fParams.noDictIDFlag == 0);
        }
        ZSTD_cParameter::ZSTD_c_forceMaxWindow => {
            *value = (*CCtxParams).forceWindow;
        }
        ZSTD_cParameter::ZSTD_c_forceAttachDict => {
            *value = (*CCtxParams).attachDictPref.0 as core::ffi::c_int;
        }
        ZSTD_cParameter::ZSTD_c_literalCompressionMode => {
            *value = (*CCtxParams).literalCompressionMode as core::ffi::c_int;
        }
        ZSTD_cParameter::ZSTD_c_nbWorkers => {
            *value = (*CCtxParams).nbWorkers;
        }
        ZSTD_cParameter::ZSTD_c_jobSize => {
            *value = (*CCtxParams).jobSize as core::ffi::c_int;
        }
        ZSTD_cParameter::ZSTD_c_overlapLog => {
            *value = (*CCtxParams).overlapLog;
        }
        ZSTD_cParameter::ZSTD_c_rsyncable => {
            *value = (*CCtxParams).rsyncable;
        }
        ZSTD_cParameter::ZSTD_c_enableDedicatedDictSearch => {
            *value = (*CCtxParams).enableDedicatedDictSearch;
        }
        ZSTD_cParameter::ZSTD_c_enableLongDistanceMatching => {
            *value = (*CCtxParams).ldmParams.enableLdm as core::ffi::c_int;
        }
        ZSTD_cParameter::ZSTD_c_ldmHashLog => {
            *value = (*CCtxParams).ldmParams.hashLog as core::ffi::c_int;
        }
        ZSTD_cParameter::ZSTD_c_ldmMinMatch => {
            *value = (*CCtxParams).ldmParams.minMatchLength as core::ffi::c_int;
        }
        ZSTD_cParameter::ZSTD_c_ldmBucketSizeLog => {
            *value = (*CCtxParams).ldmParams.bucketSizeLog as core::ffi::c_int;
        }
        ZSTD_cParameter::ZSTD_c_ldmHashRateLog => {
            *value = (*CCtxParams).ldmParams.hashRateLog as core::ffi::c_int;
        }
        ZSTD_cParameter::ZSTD_c_targetCBlockSize => {
            *value = (*CCtxParams).targetCBlockSize as core::ffi::c_int;
        }
        ZSTD_cParameter::ZSTD_c_srcSizeHint => {
            *value = (*CCtxParams).srcSizeHint;
        }
        ZSTD_cParameter::ZSTD_c_stableInBuffer => {
            *value = (*CCtxParams).inBufferMode as core::ffi::c_int;
        }
        ZSTD_cParameter::ZSTD_c_stableOutBuffer => {
            *value = (*CCtxParams).outBufferMode as core::ffi::c_int;
        }
        ZSTD_cParameter::ZSTD_c_blockDelimiters => {
            *value = (*CCtxParams).blockDelimiters as core::ffi::c_int;
        }
        ZSTD_cParameter::ZSTD_c_validateSequences => {
            *value = (*CCtxParams).validateSequences;
        }
        ZSTD_cParameter::ZSTD_c_splitAfterSequences => {
            *value = (*CCtxParams).postBlockSplitter as core::ffi::c_int;
        }
        ZSTD_cParameter::ZSTD_c_blockSplitterLevel => {
            *value = (*CCtxParams).preBlockSplitter_level;
        }
        ZSTD_cParameter::ZSTD_c_useRowMatchFinder => {
            *value = (*CCtxParams).useRowMatchFinder as core::ffi::c_int;
        }
        ZSTD_cParameter::ZSTD_c_deterministicRefPrefix => {
            *value = (*CCtxParams).deterministicRefPrefix;
        }
        ZSTD_cParameter::ZSTD_c_prefetchCDictTables => {
            *value = (*CCtxParams).prefetchCDictTables as core::ffi::c_int;
        }
        ZSTD_cParameter::ZSTD_c_enableSeqProducerFallback => {
            *value = (*CCtxParams).enableMatchFinderFallback;
        }
        ZSTD_cParameter::ZSTD_c_maxBlockSize => {
            *value = (*CCtxParams).maxBlockSize as core::ffi::c_int;
        }
        ZSTD_cParameter::ZSTD_c_repcodeResolution => {
            *value = (*CCtxParams).searchForExternalRepcodes as core::ffi::c_int;
        }
        _ => return Error::parameter_unsupported.to_error_code(),
    }
    0
}

/// Just applies `params` into `cctx`. No action is performed, parameters are merely stored.
/// If ZSTDMT is enabled, parameters are pushed to cctx->mtctx.
/// This is possible even if a compression is ongoing.
/// In which case, new parameters will be applied on the fly, starting with next compression job.
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_CCtx_setParametersUsingCCtxParams))]
pub unsafe extern "C" fn ZSTD_CCtx_setParametersUsingCCtxParams(
    cctx: *mut ZSTD_CCtx,
    params: *const ZSTD_CCtx_params,
) -> usize {
    if (*cctx).streamStage != StreamStage::Init {
        return Error::stage_wrong.to_error_code();
    }
    if !((*cctx).cdict).is_null() {
        return Error::stage_wrong.to_error_code();
    }

    (*cctx).requestedParams = *params;

    0
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_CCtx_setCParams))]
pub unsafe extern "C" fn ZSTD_CCtx_setCParams(
    cctx: *mut ZSTD_CCtx,
    cparams: ZSTD_compressionParameters,
) -> usize {
    (*cctx)
        .set_cparams(cparams)
        .map(|()| 0)
        .unwrap_or_else(|e| e.to_error_code())
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_CCtx_setFParams))]
pub unsafe extern "C" fn ZSTD_CCtx_setFParams(
    cctx: *mut ZSTD_CCtx,
    fparams: ZSTD_frameParameters,
) -> usize {
    (*cctx)
        .set_fparams(fparams)
        .map(|()| 0)
        .unwrap_or_else(|e| e.to_error_code())
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_CCtx_setParams))]
pub unsafe extern "C" fn ZSTD_CCtx_setParams(
    cctx: *mut ZSTD_CCtx,
    params: ZSTD_parameters,
) -> usize {
    (*cctx)
        .set_params(params)
        .map(|()| 0)
        .unwrap_or_else(|e| e.to_error_code())
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_CCtx_setPledgedSrcSize))]
pub unsafe extern "C" fn ZSTD_CCtx_setPledgedSrcSize(
    cctx: *mut ZSTD_CCtx,
    pledgedSrcSize: core::ffi::c_ulonglong,
) -> usize {
    if (*cctx).streamStage != StreamStage::Init {
        return Error::stage_wrong.to_error_code();
    }

    (*cctx).pledgedSrcSizePlusOne = pledgedSrcSize.wrapping_add(1);

    0
}

/// Initializes the local dictionary using requested parameters.
///
/// NOTE: Initialization does not employ the pledged src size,
/// because the dictionary may be used for multiple compressions.
unsafe fn ZSTD_initLocalDict(cctx: *mut ZSTD_CCtx) -> Result<(), Error> {
    let dl: *mut ZSTD_localDict = &mut (*cctx).localDict;
    if ((*dl).dict).is_null() {
        // No local dictionary
        return Ok(());
    }
    if !((*dl).cdict).is_null() {
        // Local dictionary already initialized
        return Ok(());
    }

    (*dl).cdict = ZSTD_createCDict_advanced2(
        (*dl).dict,
        (*dl).dictSize,
        ZSTD_dlm_byRef,
        (*dl).dictContentType,
        &(*cctx).requestedParams,
        (*cctx).customMem,
    );
    if ((*dl).cdict).is_null() {
        return Err(Error::memory_allocation);
    }
    (*cctx).cdict = (*dl).cdict;

    Ok(())
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_CCtx_loadDictionary_advanced))]
pub unsafe extern "C" fn ZSTD_CCtx_loadDictionary_advanced(
    cctx: *mut ZSTD_CCtx,
    dict: *const core::ffi::c_void,
    dictSize: usize,
    dictLoadMethod: ZSTD_dictLoadMethod_e,
    dictContentType: ZSTD_dictContentType_e,
) -> usize {
    if (*cctx).streamStage != StreamStage::Init {
        return Error::stage_wrong.to_error_code();
    }
    ZSTD_clearAllDicts(cctx); // erase any previously set dictionary
    if dict.is_null() || dictSize == 0 {
        return 0; // no dictionary
    }
    if dictLoadMethod == ZSTD_dlm_byRef {
        (*cctx).localDict.dict = dict;
    } else {
        if (*cctx).staticSize != 0 {
            return Error::memory_allocation.to_error_code();
        }
        let dictBuffer = ZSTD_customMalloc(dictSize, (*cctx).customMem);
        if dictBuffer.is_null() {
            return Error::memory_allocation.to_error_code();
        }
        core::ptr::copy_nonoverlapping(dict.cast::<u8>(), dictBuffer.cast::<u8>(), dictSize);
        (*cctx).localDict.dictBuffer = dictBuffer;
        (*cctx).localDict.dict = dictBuffer;
    }
    (*cctx).localDict.dictSize = dictSize;
    (*cctx).localDict.dictContentType = dictContentType;

    0
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_CCtx_loadDictionary_byReference))]
pub unsafe extern "C" fn ZSTD_CCtx_loadDictionary_byReference(
    cctx: *mut ZSTD_CCtx,
    dict: *const core::ffi::c_void,
    dictSize: usize,
) -> usize {
    ZSTD_CCtx_loadDictionary_advanced(cctx, dict, dictSize, ZSTD_dlm_byRef, ZSTD_dct_auto)
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_CCtx_loadDictionary))]
pub unsafe extern "C" fn ZSTD_CCtx_loadDictionary(
    cctx: *mut ZSTD_CCtx,
    dict: *const core::ffi::c_void,
    dictSize: usize,
) -> usize {
    ZSTD_CCtx_loadDictionary_advanced(cctx, dict, dictSize, ZSTD_dlm_byCopy, ZSTD_dct_auto)
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_CCtx_refCDict))]
pub unsafe extern "C" fn ZSTD_CCtx_refCDict(
    cctx: *mut ZSTD_CCtx,
    cdict: *const ZSTD_CDict,
) -> usize {
    if (*cctx).streamStage != StreamStage::Init {
        return Error::stage_wrong.to_error_code();
    }

    // Free the existing local cdict (if any) to save memory.
    ZSTD_clearAllDicts(cctx);

    (*cctx).cdict = cdict;

    0
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_CCtx_refThreadPool))]
pub unsafe extern "C" fn ZSTD_CCtx_refThreadPool(
    cctx: *mut ZSTD_CCtx,
    pool: *mut ZSTD_threadPool,
) -> usize {
    if (*cctx).streamStage != StreamStage::Init {
        return Error::stage_wrong.to_error_code();
    }

    (*cctx).pool = pool;

    0
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_CCtx_refPrefix))]
pub unsafe extern "C" fn ZSTD_CCtx_refPrefix(
    cctx: *mut ZSTD_CCtx,
    prefix: *const core::ffi::c_void,
    prefixSize: usize,
) -> usize {
    ZSTD_CCtx_refPrefix_advanced(cctx, prefix, prefixSize, ZSTD_dct_rawContent)
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_CCtx_refPrefix_advanced))]
pub unsafe extern "C" fn ZSTD_CCtx_refPrefix_advanced(
    cctx: *mut ZSTD_CCtx,
    prefix: *const core::ffi::c_void,
    prefixSize: usize,
    dictContentType: ZSTD_dictContentType_e,
) -> usize {
    if (*cctx).streamStage != StreamStage::Init {
        return Error::stage_wrong.to_error_code();
    }

    ZSTD_clearAllDicts(cctx);

    if !prefix.is_null() && prefixSize > 0 {
        (*cctx).prefixDict.dict = prefix;
        (*cctx).prefixDict.dictSize = prefixSize;
        (*cctx).prefixDict.dictContentType = dictContentType;
    }

    0
}

/// Also dumps dictionary
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_CCtx_reset))]
pub unsafe extern "C" fn ZSTD_CCtx_reset(
    cctx: *mut ZSTD_CCtx,
    reset: ZSTD_ResetDirective,
) -> usize {
    if matches!(
        reset,
        ZSTD_ResetDirective::ZSTD_reset_session_only
            | ZSTD_ResetDirective::ZSTD_reset_session_and_parameters
    ) {
        (*cctx).streamStage = StreamStage::Init;
        (*cctx).pledgedSrcSizePlusOne = 0;
    }

    if matches!(
        reset,
        ZSTD_ResetDirective::ZSTD_reset_parameters
            | ZSTD_ResetDirective::ZSTD_reset_session_and_parameters
    ) {
        if (*cctx).streamStage != StreamStage::Init {
            return Error::stage_wrong.to_error_code();
        }
        ZSTD_clearAllDicts(cctx);
        return ZSTD_CCtxParams_reset(&mut (*cctx).requestedParams);
    }

    0
}

/// Control CParam values remain within authorized range.
///
/// # Returns
///
/// 0, or an error code if one value is beyond authorized range.
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_checkCParams))]
pub extern "C" fn ZSTD_checkCParams(cParams: ZSTD_compressionParameters) -> usize {
    cParams
        .check_bounds()
        .map(|()| 0)
        .unwrap_or_else(|e| e.to_error_code())
}

impl ZSTD_compressionParameters {
    pub fn check_bounds(&self) -> Result<(), Error> {
        ZSTD_cParameter::ZSTD_c_windowLog.check_bounds(self.windowLog as core::ffi::c_int)?;
        ZSTD_cParameter::ZSTD_c_chainLog.check_bounds(self.chainLog as core::ffi::c_int)?;
        ZSTD_cParameter::ZSTD_c_hashLog.check_bounds(self.hashLog as core::ffi::c_int)?;
        ZSTD_cParameter::ZSTD_c_searchLog.check_bounds(self.searchLog as core::ffi::c_int)?;
        ZSTD_cParameter::ZSTD_c_minMatch.check_bounds(self.minMatch as core::ffi::c_int)?;
        ZSTD_cParameter::ZSTD_c_targetLength.check_bounds(self.targetLength as core::ffi::c_int)?;
        ZSTD_cParameter::ZSTD_c_strategy.check_bounds(self.strategy as core::ffi::c_int)?;
        Ok(())
    }

    /// Clamp CParam values within valid range
    pub fn clamp_bounds(&mut self) {
        ZSTD_cParameter::ZSTD_c_windowLog
            .clamp_bounds_unsigned(&mut self.windowLog)
            .unwrap();
        ZSTD_cParameter::ZSTD_c_chainLog
            .clamp_bounds_unsigned(&mut self.chainLog)
            .unwrap();
        ZSTD_cParameter::ZSTD_c_hashLog
            .clamp_bounds_unsigned(&mut self.hashLog)
            .unwrap();
        ZSTD_cParameter::ZSTD_c_searchLog
            .clamp_bounds_unsigned(&mut self.searchLog)
            .unwrap();
        ZSTD_cParameter::ZSTD_c_minMatch
            .clamp_bounds_unsigned(&mut self.minMatch)
            .unwrap();
        ZSTD_cParameter::ZSTD_c_targetLength
            .clamp_bounds_unsigned(&mut self.targetLength)
            .unwrap();
        ZSTD_cParameter::ZSTD_c_strategy
            .clamp_bounds_unsigned(&mut self.strategy)
            .unwrap();
    }

    /// Optimize `cPar` for a specified input (`srcSize` and `dictSize`).
    ///
    /// Mostly downsize to reduce memory consumption and initialization latency.
    /// - `srcSize` can be `ZSTD_CONTENTSIZE_UNKNOWN` when not known (`srcSize==0` means 0!)
    /// - `mode` is the mode for parameter adjustment. See docs for [`CParamMode`].
    ///
    /// Condition: cPar is presumed validated (can be checked using [`ZSTD_checkCParams`]).
    fn optimize(
        mut self,
        mut srcSize: core::ffi::c_ulonglong,
        mut dictSize: usize,
        mode: CParamMode,
        mut useRowMatchFinder: ParamSwitch,
    ) -> Self {
        let minSrcSize = 513; // (1<<9) + 1
        let maxWindowResize = (1 << (ZSTD_WINDOWLOG_MAX - 1)) as u64;

        match mode {
            CParamMode::CreateCDict => {
                // Assume a small source size when creating a dictionary
                // with an unknown source size.
                if dictSize != 0 && srcSize == ZSTD_CONTENTSIZE_UNKNOWN {
                    srcSize = minSrcSize as core::ffi::c_ulonglong;
                }
            }
            CParamMode::AttachDict => {
                // Dictionary has its own dedicated parameters which have
                // already been selected. We are selecting parameters
                // for only the source.
                dictSize = 0;
            }
            CParamMode::NoAttachDict | CParamMode::Unknown => {
                // If we don't know the source size, don't make any
                // assumptions about it. We will already have selected
                // smaller parameters if a dictionary is in use.
            }
        }

        // resize windowLog if input is small enough, to use less memory
        if srcSize <= maxWindowResize && dictSize as u64 <= maxWindowResize {
            let tSize = srcSize.wrapping_add(dictSize as core::ffi::c_ulonglong) as u32;
            static hashSizeMin: u32 = (1 << ZSTD_HASHLOG_MIN) as u32;
            let srcLog = if tSize < hashSizeMin {
                ZSTD_HASHLOG_MIN as core::ffi::c_uint
            } else {
                (ZSTD_highbit32(tSize.wrapping_sub(1))).wrapping_add(1)
            };
            self.windowLog = Ord::min(self.windowLog, srcLog);
        }
        if srcSize != ZSTD_CONTENTSIZE_UNKNOWN {
            let dictAndWindowLog = ZSTD_dictAndWindowLog(self.windowLog, srcSize, dictSize as u64);
            let cycleLog = ZSTD_cycleLog(self.chainLog, self.strategy);
            self.hashLog = Ord::min(self.hashLog, dictAndWindowLog.wrapping_add(1));
            if cycleLog > dictAndWindowLog {
                self.chainLog =
                    (self.chainLog).wrapping_sub(cycleLog.wrapping_sub(dictAndWindowLog));
            }
        }

        if self.windowLog < ZSTD_WINDOWLOG_ABSOLUTEMIN as core::ffi::c_uint {
            // minimum wlog required for valid frame header
            self.windowLog = ZSTD_WINDOWLOG_ABSOLUTEMIN as core::ffi::c_uint;
        }

        // We can't use more than 32 bits of hash in total, so that means that we require:
        // (hashLog + 8) <= 32 && (chainLog + 8) <= 32
        if mode == CParamMode::CreateCDict && self.cdict_indices_are_tagged() {
            let maxShortCacheHashLog = (32 - ZSTD_SHORT_CACHE_TAG_BITS) as u32;
            self.hashLog = Ord::min(self.hashLog, maxShortCacheHashLog);
            self.chainLog = Ord::min(self.chainLog, maxShortCacheHashLog);
        }

        // At this point, we aren't 100% sure if we are using the row match finder.
        // Unless it is explicitly disabled, conservatively assume that it is enabled.
        // In this case it will only be disabled for small sources, so shrinking the
        // hash log a little bit shouldn't result in any ratio loss.
        if useRowMatchFinder == ParamSwitch::Auto {
            useRowMatchFinder = ParamSwitch::Enable;
        }

        // We can't hash more than 32-bits in total. So that means that we require:
        // (hashLog - rowLog + 8) <= 32
        if ZSTD_rowMatchFinderUsed(self.strategy, useRowMatchFinder) {
            // Switch to 32-entry rows if searchLog is 5 (or more)
            let rowLog = self.searchLog.clamp(4, 6);
            let maxRowHashLog = 32u32 - ZSTD_ROW_HASH_TAG_BITS;
            let maxHashLog = maxRowHashLog.wrapping_add(rowLog);
            self.hashLog = Ord::min(self.hashLog, maxHashLog);
        }

        self
    }

    fn override_cparams(&mut self, overrides: &ZSTD_compressionParameters) {
        if overrides.windowLog != 0 {
            self.windowLog = overrides.windowLog;
        }
        if overrides.hashLog != 0 {
            self.hashLog = overrides.hashLog;
        }
        if overrides.chainLog != 0 {
            self.chainLog = overrides.chainLog;
        }
        if overrides.searchLog != 0 {
            self.searchLog = overrides.searchLog;
        }
        if overrides.minMatch != 0 {
            self.minMatch = overrides.minMatch;
        }
        if overrides.targetLength != 0 {
            self.targetLength = overrides.targetLength;
        }
        if overrides.strategy != 0 {
            self.strategy = overrides.strategy;
        }
    }
}

/// Condition for correct operation: hashLog > 1.
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_cycleLog))]
pub extern "C" fn ZSTD_cycleLog(hashLog: u32, strat: ZSTD_strategy) -> u32 {
    let btScale = u32::from(strat >= ZSTD_btlazy2);
    hashLog.wrapping_sub(btScale)
}

/// Returns an adjusted window log that is large enough to fit the source and the dictionary.
/// The zstd format says that the entire dictionary is valid if one byte of the dictionary
/// is within the window. So the hashLog and chainLog should be large enough to reference both
/// the dictionary and the window. So we must use this adjusted dictAndWindowLog when downsizing
/// the hashLog and windowLog.
///
/// NOTE: srcSize must not be ZSTD_CONTENTSIZE_UNKNOWN.
fn ZSTD_dictAndWindowLog(windowLog: u32, srcSize: u64, dictSize: u64) -> u32 {
    let maxWindowSize = 1 << ZSTD_WINDOWLOG_MAX;
    if dictSize == 0 {
        // No dictionary ==> No change
        return windowLog;
    }

    let windowSize = (1 << windowLog) as u64;
    let dictAndWindowSize = dictSize.wrapping_add(windowSize);
    // If the window size is already large enough to fit both the source and the dictionary
    // then just use the window size. Otherwise adjust so that it fits the dictionary and
    // the window.
    if windowSize >= dictSize.wrapping_add(srcSize) {
        // Window size large enough already
        windowLog
    } else if dictAndWindowSize >= maxWindowSize {
        // Larger than max window log
        ZSTD_WINDOWLOG_MAX as u32
    } else {
        (ZSTD_highbit32((dictAndWindowSize as u32).wrapping_sub(1))).wrapping_add(1)
    }
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_adjustCParams))]
pub extern "C" fn ZSTD_adjustCParams(
    mut cPar: ZSTD_compressionParameters,
    mut srcSize: core::ffi::c_ulonglong,
    dictSize: usize,
) -> ZSTD_compressionParameters {
    cPar.clamp_bounds();
    if srcSize == 0 {
        srcSize = ZSTD_CONTENTSIZE_UNKNOWN;
    }

    cPar.optimize(srcSize, dictSize, CParamMode::Unknown, ParamSwitch::Auto)
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_getCParamsFromCCtxParams))]
pub unsafe extern "C" fn ZSTD_getCParamsFromCCtxParams(
    CCtxParams: *const ZSTD_CCtx_params,
    srcSizeHint: u64,
    dictSize: usize,
    mode: core::ffi::c_int,
) -> ZSTD_compressionParameters {
    let mode = CParamMode::from(mode);

    ZSTD_getCParamsFromCCtxParams_internal(&*CCtxParams, srcSizeHint, dictSize, mode)
}

pub fn ZSTD_getCParamsFromCCtxParams_internal(
    CCtxParams: &ZSTD_CCtx_params,
    mut srcSizeHint: u64,
    dictSize: usize,
    mode: CParamMode,
) -> ZSTD_compressionParameters {
    if srcSizeHint == ZSTD_CONTENTSIZE_UNKNOWN && CCtxParams.srcSizeHint > 0 {
        srcSizeHint = CCtxParams.srcSizeHint as u64;
    }
    let mut cParams =
        ZSTD_compressionParameters::new(CCtxParams.compressionLevel, srcSizeHint, dictSize, mode);
    if CCtxParams.ldmParams.enableLdm == ParamSwitch::Enable {
        cParams.windowLog = ZSTD_LDM_DEFAULT_WINDOW_LOG as core::ffi::c_uint;
    }
    cParams.override_cparams(&CCtxParams.cParams);

    // srcSizeHint == 0 means 0
    cParams.optimize(srcSizeHint, dictSize, mode, CCtxParams.useRowMatchFinder)
}

fn ZSTD_sizeof_matchState(
    cParams: &ZSTD_compressionParameters,
    useRowMatchFinder: ParamSwitch,
    enableDedicatedDictSearch: bool,
    forCCtx: bool,
) -> usize {
    // chain table size should be 0 for fast or row-hash strategies
    let chainSize = if ZSTD_allocateChainTable(
        cParams.strategy,
        useRowMatchFinder,
        enableDedicatedDictSearch && !forCCtx,
    ) {
        1usize << cParams.chainLog
    } else {
        0
    };
    let hSize = 1usize << cParams.hashLog;
    let hashLog3 = if forCCtx && cParams.minMatch == 3 {
        cParams.windowLog.min(ZSTD_HASHLOG3_MAX)
    } else {
        0
    };
    let h3Size = if hashLog3 != 0 { 1usize << hashLog3 } else { 0 };
    // We don't use ZSTD_cwksp_alloc_size() here because the tables aren't
    // surrounded by redzones in ASAN.
    let tableSpace = chainSize
        .wrapping_mul(size_of::<u32>())
        .wrapping_add(hSize.wrapping_mul(size_of::<u32>()))
        .wrapping_add(h3Size.wrapping_mul(size_of::<u32>()));
    let optPotentialSpace =
        (ZSTD_cwksp_aligned64_alloc_size((usize::from(MaxML) + 1).wrapping_mul(size_of::<u32>())))
            .wrapping_add(ZSTD_cwksp_aligned64_alloc_size(
                (usize::from(MaxLL) + 1).wrapping_mul(size_of::<u32>()),
            ))
            .wrapping_add(ZSTD_cwksp_aligned64_alloc_size(
                (usize::from(MaxOff) + 1).wrapping_mul(size_of::<u32>()),
            ))
            .wrapping_add(ZSTD_cwksp_aligned64_alloc_size(
                (usize::from(MaxLit) + 1).wrapping_mul(size_of::<u32>()),
            ))
            .wrapping_add(ZSTD_cwksp_aligned64_alloc_size(
                ZSTD_OPT_SIZE.wrapping_mul(size_of::<ZSTD_match_t>()),
            ))
            .wrapping_add(ZSTD_cwksp_aligned64_alloc_size(
                ZSTD_OPT_SIZE.wrapping_mul(size_of::<ZSTD_optimal_t>()),
            ));
    let lazyAdditionalSpace = if ZSTD_rowMatchFinderUsed(cParams.strategy, useRowMatchFinder) {
        ZSTD_cwksp_aligned64_alloc_size(hSize)
    } else {
        0
    };
    let optSpace = if forCCtx && cParams.strategy >= ZSTD_btopt {
        optPotentialSpace
    } else {
        0
    };
    let slackSpace = ZSTD_cwksp_slack_space_required();

    tableSpace
        .wrapping_add(optSpace)
        .wrapping_add(slackSpace)
        .wrapping_add(lazyAdditionalSpace)
}

/// Helper function for calculating memory requirements.
///
/// Gives a tighter bound than ZSTD_sequenceBound() by taking minMatch into account.
fn ZSTD_maxNbSeq(
    blockSize: usize,
    minMatch: core::ffi::c_uint,
    useSequenceProducer: bool,
) -> usize {
    let divider = (if minMatch == 3 || useSequenceProducer {
        3
    } else {
        4
    }) as u32;
    blockSize / divider as usize
}

fn ZSTD_estimateCCtxSize_usingCCtxParams_internal(
    cParams: &ZSTD_compressionParameters,
    ldmParams: &LdmParams,
    isStatic: bool,
    useRowMatchFinder: ParamSwitch,
    buffInSize: usize,
    buffOutSize: usize,
    pledgedSrcSize: u64,
    useSequenceProducer: bool,
    maxBlockSize: usize,
) -> usize {
    let windowSize = (1u64 << cParams.windowLog)
        .min(pledgedSrcSize as core::ffi::c_ulonglong) // pledgedSrcSize can be 0, so .clamp() would panic
        .max(1) as usize;
    let blockSize = ZSTD_resolveMaxBlockSize(maxBlockSize).min(windowSize);
    let maxNbSeq = ZSTD_maxNbSeq(blockSize, cParams.minMatch, useSequenceProducer);
    let tokenSpace = (ZSTD_cwksp_alloc_size(WILDCOPY_OVERLENGTH.wrapping_add(blockSize)))
        .wrapping_add(ZSTD_cwksp_aligned64_alloc_size(
            maxNbSeq.wrapping_mul(size_of::<SeqDef>()),
        ))
        .wrapping_add(3 * ZSTD_cwksp_alloc_size(maxNbSeq.wrapping_mul(size_of::<u8>())));
    let tmpWorkSpace = ZSTD_cwksp_alloc_size(
        ((8usize << 10) + 512)
            .wrapping_add(size_of::<core::ffi::c_uint>().wrapping_mul(MaxSeq + 2))
            .max(ZSTD_SLIPBLOCK_WORKSPACESIZE),
    );
    let blockStateSpace = 2 * ZSTD_cwksp_alloc_size(size_of::<ZSTD_compressedBlockState_t>());
    let matchStateSize = ZSTD_sizeof_matchState(cParams, useRowMatchFinder, false, true);

    let ldmSpace = ldmParams.get_table_size();
    let maxNbLdmSeq = ldmParams.get_max_nb_seq(blockSize);
    let ldmSeqSpace = if ldmParams.enableLdm == ParamSwitch::Enable {
        ZSTD_cwksp_aligned64_alloc_size(maxNbLdmSeq.wrapping_mul(size_of::<rawSeq>()))
    } else {
        0
    };

    let bufferSpace =
        ZSTD_cwksp_alloc_size(buffInSize).wrapping_add(ZSTD_cwksp_alloc_size(buffOutSize));
    let cctxSpace = if isStatic {
        ZSTD_cwksp_alloc_size(size_of::<ZSTD_CCtx>())
    } else {
        0
    };

    let maxNbExternalSeq = ZSTD_sequenceBound(blockSize);
    let externalSeqSpace = if useSequenceProducer {
        ZSTD_cwksp_aligned64_alloc_size(maxNbExternalSeq.wrapping_mul(size_of::<ZSTD_Sequence>()))
    } else {
        0
    };

    cctxSpace
        .wrapping_add(tmpWorkSpace)
        .wrapping_add(blockStateSpace)
        .wrapping_add(ldmSpace)
        .wrapping_add(ldmSeqSpace)
        .wrapping_add(matchStateSize)
        .wrapping_add(tokenSpace)
        .wrapping_add(bufferSpace)
        .wrapping_add(externalSeqSpace)
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_estimateCCtxSize_usingCCtxParams))]
pub unsafe extern "C" fn ZSTD_estimateCCtxSize_usingCCtxParams(
    params: *const ZSTD_CCtx_params,
) -> usize {
    let cParams = ZSTD_getCParamsFromCCtxParams_internal(
        &*params,
        ZSTD_CONTENTSIZE_UNKNOWN,
        0,
        CParamMode::NoAttachDict,
    );
    let useRowMatchFinder = ZSTD_resolveRowMatchFinderMode((*params).useRowMatchFinder, &cParams);

    if (*params).nbWorkers > 0 {
        return Error::GENERIC.to_error_code();
    }

    // estimateCCtxSize is for one-shot compression. So no buffers should
    // be needed. However, we still allocate two 0-sized buffers, which can
    // take space under ASAN.
    ZSTD_estimateCCtxSize_usingCCtxParams_internal(
        &cParams,
        &(*params).ldmParams,
        true,
        useRowMatchFinder,
        0,
        0,
        ZSTD_CONTENTSIZE_UNKNOWN,
        (*params).has_ext_seq_prod(),
        (*params).maxBlockSize,
    )
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_estimateCCtxSize_usingCParams))]
pub unsafe extern "C" fn ZSTD_estimateCCtxSize_usingCParams(
    cParams: ZSTD_compressionParameters,
) -> usize {
    let mut initialParams = ZSTD_CCtx_params::from(cParams);
    if ZSTD_rowMatchFinderSupported(cParams.strategy) {
        // Pick bigger of not using and using row-based matchfinder for greedy and lazy strategies
        initialParams.useRowMatchFinder = ParamSwitch::Disable;
        let noRowCCtxSize = ZSTD_estimateCCtxSize_usingCCtxParams(&initialParams);
        initialParams.useRowMatchFinder = ParamSwitch::Enable;
        let rowCCtxSize = ZSTD_estimateCCtxSize_usingCCtxParams(&initialParams);
        noRowCCtxSize.max(rowCCtxSize)
    } else {
        ZSTD_estimateCCtxSize_usingCCtxParams(&initialParams)
    }
}

static SRC_SIZE_TIERS: [core::ffi::c_ulonglong; 4] = [
    16 * (1 << 10),
    128 * (1 << 10),
    256 * (1 << 10),
    ZSTD_CONTENTSIZE_UNKNOWN,
];

unsafe extern "C" fn ZSTD_estimateCCtxSize_internal(compressionLevel: core::ffi::c_int) -> usize {
    let mut largestSize = 0;
    for srcSizeHint in SRC_SIZE_TIERS {
        // Choose the set of cParams for a given level across all srcSizes that give the largest cctxSize
        let cParams = ZSTD_compressionParameters::new(
            compressionLevel,
            srcSizeHint,
            0,
            CParamMode::NoAttachDict,
        );
        largestSize = ZSTD_estimateCCtxSize_usingCParams(cParams).max(largestSize);
    }
    largestSize
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_estimateCCtxSize))]
pub unsafe extern "C" fn ZSTD_estimateCCtxSize(compressionLevel: core::ffi::c_int) -> usize {
    let mut memBudget = 0;
    // Negative compression levels are used for fast mode, these should still do 1 iteration
    let start = compressionLevel.min(1);
    for level in start..compressionLevel + 1 {
        // Ensure monotonically increasing memory usage as compression level increases
        memBudget = Ord::max(memBudget, ZSTD_estimateCCtxSize_internal(level));
    }
    memBudget
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_estimateCStreamSize_usingCCtxParams))]
pub unsafe extern "C" fn ZSTD_estimateCStreamSize_usingCCtxParams(
    params: *const ZSTD_CCtx_params,
) -> usize {
    if (*params).nbWorkers > 0 {
        return Error::GENERIC.to_error_code();
    }
    let cParams = ZSTD_getCParamsFromCCtxParams_internal(
        &*params,
        ZSTD_CONTENTSIZE_UNKNOWN,
        0,
        CParamMode::NoAttachDict,
    );
    let blockSize = ZSTD_resolveMaxBlockSize((*params).maxBlockSize).min(1 << cParams.windowLog);
    let inBuffSize = if (*params).inBufferMode == ZSTD_bm_buffered {
        (1usize << cParams.windowLog).wrapping_add(blockSize)
    } else {
        0
    };
    let outBuffSize = if (*params).outBufferMode == ZSTD_bm_buffered {
        ZSTD_compressBound(blockSize).wrapping_add(1)
    } else {
        0
    };
    let useRowMatchFinder =
        ZSTD_resolveRowMatchFinderMode((*params).useRowMatchFinder, &(*params).cParams);

    ZSTD_estimateCCtxSize_usingCCtxParams_internal(
        &cParams,
        &(*params).ldmParams,
        true,
        useRowMatchFinder,
        inBuffSize,
        outBuffSize,
        ZSTD_CONTENTSIZE_UNKNOWN,
        (*params).has_ext_seq_prod(),
        (*params).maxBlockSize,
    )
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_estimateCStreamSize_usingCParams))]
pub unsafe extern "C" fn ZSTD_estimateCStreamSize_usingCParams(
    cParams: ZSTD_compressionParameters,
) -> usize {
    let mut initialParams = ZSTD_CCtx_params::from(cParams);
    if ZSTD_rowMatchFinderSupported(cParams.strategy) {
        // Pick bigger of not using and using row-based matchfinder for greedy and lazy strategies
        initialParams.useRowMatchFinder = ParamSwitch::Disable;
        let noRowCCtxSize = ZSTD_estimateCStreamSize_usingCCtxParams(&initialParams);
        initialParams.useRowMatchFinder = ParamSwitch::Enable;
        let rowCCtxSize = ZSTD_estimateCStreamSize_usingCCtxParams(&initialParams);
        noRowCCtxSize.max(rowCCtxSize)
    } else {
        ZSTD_estimateCStreamSize_usingCCtxParams(&initialParams)
    }
}

unsafe fn ZSTD_estimateCStreamSize_internal(compressionLevel: core::ffi::c_int) -> usize {
    let cParams = ZSTD_compressionParameters::new(
        compressionLevel,
        ZSTD_CONTENTSIZE_UNKNOWN,
        0,
        CParamMode::NoAttachDict,
    );
    ZSTD_estimateCStreamSize_usingCParams(cParams)
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_estimateCStreamSize))]
pub unsafe extern "C" fn ZSTD_estimateCStreamSize(compressionLevel: core::ffi::c_int) -> usize {
    let mut memBudget = 0;
    let start = compressionLevel.min(1);
    for level in start..compressionLevel + 1 {
        memBudget = Ord::max(memBudget, ZSTD_estimateCStreamSize_internal(level));
    }
    memBudget
}

/// Tells how much data has been consumed (input) and produced (output) for current frame.
///
/// Able to count progression inside worker threads (non-blocking mode).
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_getFrameProgression))]
pub unsafe extern "C" fn ZSTD_getFrameProgression(cctx: *const ZSTD_CCtx) -> ZSTD_frameProgression {
    if (*cctx).appliedParams.nbWorkers > 0 {
        return ZSTDMT_getFrameProgression((*cctx).mtctx);
    }
    let buffered = if ((*cctx).inBuff).is_null() {
        0
    } else {
        ((*cctx).inBuffPos).wrapping_sub((*cctx).inToCompress)
    };

    if buffered != 0 {
        assert!((*cctx).inBuffPos >= (*cctx).inToCompress)
    }
    assert!(buffered <= ZSTD_BLOCKSIZE_MAX as usize);

    ZSTD_frameProgression {
        ingested: ((*cctx).consumedSrcSize).wrapping_add(buffered as core::ffi::c_ulonglong),
        consumed: (*cctx).consumedSrcSize,
        produced: (*cctx).producedCSize,
        // simplified; some data might still be left within streaming output buffer
        flushed: (*cctx).producedCSize,
        currentJobID: 0,
        nbActiveWorkers: 0,
    }
}

/// Only useful for multithreading scenarios currently (nbWorkers >= 1).
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_toFlushNow))]
pub unsafe extern "C" fn ZSTD_toFlushNow(cctx: *mut ZSTD_CCtx) -> usize {
    if (*cctx).appliedParams.nbWorkers > 0 {
        return ZSTDMT_toFlushNow((*cctx).mtctx);
    }

    // over-simplification; could also check if context is currently running in
    // streaming mode, and in which case, report how many bytes are left to be
    // flushed within output buffer
    0
}

fn ZSTD_assertEqualCParams(
    cParams1: ZSTD_compressionParameters,
    cParams2: ZSTD_compressionParameters,
) {
    assert_eq!(cParams1.windowLog, cParams2.windowLog);
    assert_eq!(cParams1.chainLog, cParams2.chainLog);
    assert_eq!(cParams1.hashLog, cParams2.hashLog);
    assert_eq!(cParams1.searchLog, cParams2.searchLog);
    assert_eq!(cParams1.minMatch, cParams2.minMatch);
    assert_eq!(cParams1.targetLength, cParams2.targetLength);
    assert_eq!(cParams1.strategy, cParams2.strategy);
}

pub unsafe fn ZSTD_reset_compressedBlockState(bs: *mut ZSTD_compressedBlockState_t) {
    for i in 0..ZSTD_REP_NUM {
        (*bs).rep[i as usize] = repStartValue[i as usize];
    }
    (*bs).entropy.huf.repeatMode = HUF_repeat::None;
    (*bs).entropy.fse.offcode_repeatMode = FSE_repeat::None;
    (*bs).entropy.fse.matchlength_repeatMode = FSE_repeat::None;
    (*bs).entropy.fse.litlength_repeatMode = FSE_repeat::None;
}

/// Invalidate all the matches in the match finder tables.
/// Requires nextSrc and base to be set (can be NULL).
fn ZSTD_invalidateMatchState(ms: &mut ZSTD_MatchState_t) {
    ms.window.clear();
    ms.nextToUpdate = ms.window.dictLimit;
    ms.loadedDictEnd = 0;
    ms.opt.litLengthSum = 0; // force reset of btopt stats
    ms.dictMatchState = core::ptr::null();
}

/// Mixes bits in a 64 bits in a value, based on XXH3_rrmxmx
fn ZSTD_bitmix(mut val: u64, len: u64) -> u64 {
    val ^= val.rotate_right(49) ^ val.rotate_right(24);
    val = val.wrapping_mul(0x9fb21c651e98df25u64);
    val ^= (val >> 35).wrapping_add(len);
    val = val.wrapping_mul(0x9fb21c651e98df25u64);
    val ^ val >> 28
}

/// Mixes in the hashSalt and hashSaltEntropy to create a new hashSalt
fn ZSTD_advanceHashSalt(ms: &mut ZSTD_MatchState_t) {
    ms.hashSalt = ZSTD_bitmix(ms.hashSalt, 8) ^ ZSTD_bitmix(ms.hashSaltEntropy as u64, 4);
}

unsafe fn ZSTD_reset_matchState(
    ms: &mut ZSTD_MatchState_t,
    ws: &mut ZSTD_cwksp,
    cParams: &ZSTD_compressionParameters,
    useRowMatchFinder: ParamSwitch,
    crp: CompResetPolicy,
    forceResetIndex: IndexResetPolicy,
    forWho: ResetTarget,
) -> Result<(), Error> {
    // disable chain table allocation for fast or row-based strategies
    let chainSize = if ZSTD_allocateChainTable(
        cParams.strategy,
        useRowMatchFinder,
        ms.dedicatedDictSearch != 0 && forWho == ResetTarget::CDict,
    ) {
        1usize << cParams.chainLog
    } else {
        0
    };
    let hSize = 1usize << cParams.hashLog;
    let hashLog3 = if forWho == ResetTarget::CCtx && cParams.minMatch == 3 {
        cParams.windowLog.min(ZSTD_HASHLOG3_MAX)
    } else {
        0
    };
    let h3Size = if hashLog3 != 0 { 1usize << hashLog3 } else { 0 };

    if forceResetIndex == IndexResetPolicy::Reset {
        ms.window.init();
        ZSTD_cwksp_mark_tables_dirty(ws);
    }

    ms.hashLog3 = hashLog3;
    ms.lazySkipping = 0;

    ZSTD_invalidateMatchState(ms);

    ZSTD_cwksp_clear_tables(ws);

    // table Space
    ms.hashTable = ZSTD_cwksp_reserve_table(ws, hSize.wrapping_mul(size_of::<u32>())) as *mut u32;
    ms.chainTable =
        ZSTD_cwksp_reserve_table(ws, chainSize.wrapping_mul(size_of::<u32>())) as *mut u32;
    ms.hashTable3 = ZSTD_cwksp_reserve_table(ws, h3Size.wrapping_mul(size_of::<u32>())) as *mut u32;
    if ZSTD_cwksp_reserve_failed(ws) {
        return Err(Error::memory_allocation);
    }

    if crp == CompResetPolicy::MakeClean {
        // reset tables only
        ZSTD_cwksp_clean_tables(ws);
    }

    if ZSTD_rowMatchFinderUsed(cParams.strategy, useRowMatchFinder) {
        // Row match finder needs an additional table of hashes ("tags")
        let tagTableSize = hSize;
        // We want to generate a new salt in case we reset a Cctx, but we always want to use
        // 0 when we reset a Cdict
        if forWho == ResetTarget::CCtx {
            ms.tagTable = ZSTD_cwksp_reserve_aligned_init_once(ws, tagTableSize) as *mut u8;
            ZSTD_advanceHashSalt(ms);
        } else {
            // When we are not salting we want to always memset the memory
            ms.tagTable = ZSTD_cwksp_reserve_aligned64(ws, tagTableSize) as *mut u8;
            ptr::write_bytes(ms.tagTable, 0, tagTableSize);
            ms.hashSalt = 0;
        }

        // Switch to 32-entry rows if searchLog is 5 (or more)
        let rowLog = cParams.searchLog.clamp(4, 6);
        ms.rowHashLog = cParams.hashLog.wrapping_sub(rowLog);
    }

    // opt parser space
    if forWho == ResetTarget::CCtx && cParams.strategy >= ZSTD_btopt {
        ms.opt.litFreq =
            ZSTD_cwksp_reserve_aligned64(ws, size_of::<[core::ffi::c_uint; MaxLit as usize + 1]>())
                .cast();
        ms.opt.litLengthFreq =
            ZSTD_cwksp_reserve_aligned64(ws, size_of::<[core::ffi::c_uint; MaxLL as usize + 1]>())
                .cast();
        ms.opt.matchLengthFreq =
            ZSTD_cwksp_reserve_aligned64(ws, size_of::<[core::ffi::c_uint; MaxML as usize + 1]>())
                .cast();
        ms.opt.offCodeFreq =
            ZSTD_cwksp_reserve_aligned64(ws, size_of::<[core::ffi::c_uint; MaxOff as usize + 1]>())
                .cast();
        ms.opt.matchTable =
            ZSTD_cwksp_reserve_aligned64(ws, size_of::<[ZSTD_match_t; ZSTD_OPT_SIZE]>()).cast();
        ms.opt.priceTable =
            ZSTD_cwksp_reserve_aligned64(ws, size_of::<[ZSTD_optimal_t; ZSTD_OPT_SIZE]>()).cast();
    }

    ms.cParams = *cParams;

    if ZSTD_cwksp_reserve_failed(ws) {
        return Err(Error::memory_allocation);
    }

    Ok(())
}

pub const ZSTD_INDEXOVERFLOW_MARGIN: usize = 16 * (1 << 20);

/// Minor optimization: prefer memset() rather than reduceIndex() which is measurably slow in some
/// circumstances (reported for Visual Studio). Works when re-using a context for a lot of smallish
/// inputs: if all inputs are smaller than ZSTD_INDEXOVERFLOW_MARGIN, memset() will be triggered
/// before reduceIndex().
fn ZSTD_indexTooCloseToMax(w: ZSTD_window_t) -> bool {
    w.nextSrc.wrapping_offset_from(w.base) as usize
        > ZSTD_CURRENT_MAX.wrapping_sub(ZSTD_INDEXOVERFLOW_MARGIN)
}

/// When dictionaries are larger than [`ZSTD_CHUNKSIZE_MAX`] they can't be loaded in
/// one go generically. So we ensure that in that case we reset the tables to zero,
/// so that we can load as much of the dictionary as possible.
fn ZSTD_dictTooBig(loadedDictSize: usize) -> bool {
    loadedDictSize > ZSTD_CHUNKSIZE_MAX
}

/// loadedDictSize is the size of the dictionary to be loaded
/// into the context, if any. If no dictionary is used, or the
/// dictionary is being attached / copied, then pass 0.
///
/// Note: `params` are assumed fully validated at this stage.
unsafe fn ZSTD_resetCCtx_internal(
    zc: *mut ZSTD_CCtx,
    params: &ZSTD_CCtx_params,
    pledgedSrcSize: u64,
    loadedDictSize: usize,
    crp: CompResetPolicy,
    zbuff: BufferedPolicy,
) -> Result<(), Error> {
    let ws = &mut (*zc).workspace;

    (*zc).isFirstBlock = 1;

    // Set applied params early so we can modify them for LDM.
    (*zc).appliedParams = *params;

    if (*zc).appliedParams.ldmParams.enableLdm == ParamSwitch::Enable {
        // Adjust long distance matching parameters
        let cParams = (*zc).appliedParams.cParams;
        (*zc).appliedParams.ldmParams.adjust_parameters(&cParams);
    }

    // Point params at the applied params, now that LDM has modified them.
    let params = &(*zc).appliedParams;

    let windowSize = (1usize << params.cParams.windowLog)
        .min(pledgedSrcSize as usize) // pledgedSrcSize can be 0, so .clamp() would panic
        .max(1);
    let blockSize = params.maxBlockSize.min(windowSize);
    let maxNbSeq = ZSTD_maxNbSeq(
        blockSize,
        params.cParams.minMatch,
        params.has_ext_seq_prod(),
    );
    let buffOutSize =
        if zbuff == BufferedPolicy::Buffered && params.outBufferMode == ZSTD_bm_buffered {
            ZSTD_compressBound(blockSize).wrapping_add(1)
        } else {
            0
        };
    let buffInSize = if zbuff == BufferedPolicy::Buffered && params.inBufferMode == ZSTD_bm_buffered
    {
        windowSize.wrapping_add(blockSize)
    } else {
        0
    };
    let maxNbLdmSeq = params.ldmParams.get_max_nb_seq(blockSize);

    let indexTooClose = ZSTD_indexTooCloseToMax((*zc).blockState.matchState.window);
    let dictTooBig = ZSTD_dictTooBig(loadedDictSize);
    let mut needsIndexReset = if indexTooClose || dictTooBig || (*zc).initialized == 0 {
        IndexResetPolicy::Reset
    } else {
        IndexResetPolicy::Continue
    };

    let neededSpace = ZSTD_estimateCCtxSize_usingCCtxParams_internal(
        &params.cParams,
        &params.ldmParams,
        (*zc).staticSize != 0,
        params.useRowMatchFinder,
        buffInSize,
        buffOutSize,
        pledgedSrcSize,
        params.has_ext_seq_prod(),
        params.maxBlockSize,
    );

    if let Some(err) = Error::from_error_code(neededSpace) {
        return Err(err);
    }

    if (*zc).staticSize == 0 {
        ZSTD_cwksp_bump_oversized_duration(ws, 0);
    }

    // Check if workspace is large enough, alloc a new one if needed
    let workspaceTooSmall = ZSTD_cwksp_sizeof(ws) < neededSpace;
    let workspaceWasteful = ZSTD_cwksp_check_wasteful(ws, neededSpace);
    let resizeWorkspace = workspaceTooSmall || workspaceWasteful;

    if resizeWorkspace {
        if (*zc).staticSize != 0 {
            return Err(Error::memory_allocation);
        }
        needsIndexReset = IndexResetPolicy::Reset;
        ZSTD_cwksp_free(ws, (*zc).customMem);
        ZSTD_cwksp_create(ws, neededSpace, (*zc).customMem)?;

        // Statically sized space.
        // tmpWorkspace never moves,
        // though prev/next block swap places
        (*zc).blockState.prevCBlock =
            ZSTD_cwksp_reserve_object(ws, size_of::<ZSTD_compressedBlockState_t>())
                as *mut ZSTD_compressedBlockState_t;
        if ((*zc).blockState.prevCBlock).is_null() {
            return Err(Error::memory_allocation);
        }
        (*zc).blockState.nextCBlock =
            ZSTD_cwksp_reserve_object(ws, size_of::<ZSTD_compressedBlockState_t>())
                as *mut ZSTD_compressedBlockState_t;
        if ((*zc).blockState.nextCBlock).is_null() {
            return Err(Error::memory_allocation);
        }
        (*zc).tmpWorkspace = ZSTD_cwksp_reserve_object(
            ws,
            ((8usize << 10) + 512)
                .wrapping_add(size_of::<core::ffi::c_uint>().wrapping_mul(MaxSeq + 2))
                .max(ZSTD_SLIPBLOCK_WORKSPACESIZE),
        );
        if ((*zc).tmpWorkspace).is_null() {
            return Err(Error::memory_allocation);
        }
        (*zc).tmpWkspSize = ((8usize << 10) + 512)
            .wrapping_add(size_of::<core::ffi::c_uint>().wrapping_mul(MaxSeq + 2))
            .max(ZSTD_SLIPBLOCK_WORKSPACESIZE);
    }

    ZSTD_cwksp_clear(ws);

    // init params
    (*zc).blockState.matchState.cParams = params.cParams;
    (*zc).blockState.matchState.prefetchCDictTables =
        core::ffi::c_int::from(params.prefetchCDictTables == ParamSwitch::Enable);
    (*zc).pledgedSrcSizePlusOne = pledgedSrcSize.wrapping_add(1) as core::ffi::c_ulonglong;
    (*zc).consumedSrcSize = 0;
    (*zc).producedCSize = 0;
    if pledgedSrcSize as core::ffi::c_ulonglong == ZSTD_CONTENTSIZE_UNKNOWN {
        (*zc).appliedParams.fParams.contentSizeFlag = 0;
    }
    (*zc).blockSizeMax = blockSize;

    ZSTD_XXH64_reset(&mut (*zc).xxhState, 0);
    (*zc).stage = CompressionStage::Init;
    (*zc).dictID = 0;
    (*zc).dictContentSize = 0;

    ZSTD_reset_compressedBlockState((*zc).blockState.prevCBlock);

    ZSTD_reset_matchState(
        &mut (*zc).blockState.matchState,
        ws,
        &params.cParams,
        params.useRowMatchFinder,
        crp,
        needsIndexReset,
        ResetTarget::CCtx,
    )?;

    (*zc).seqStore.sequencesStart =
        ZSTD_cwksp_reserve_aligned64(ws, maxNbSeq.wrapping_mul(size_of::<SeqDef>())) as *mut SeqDef;

    // ldm hash table
    if params.ldmParams.enableLdm == ParamSwitch::Enable {
        // TODO: avoid memset?
        let ldmHSize = 1usize << params.ldmParams.hashLog;
        (*zc).ldmState.hashTable =
            ZSTD_cwksp_reserve_aligned64(ws, ldmHSize.wrapping_mul(size_of::<LdmEntry>()))
                as *mut LdmEntry;
        ptr::write_bytes(
            (*zc).ldmState.hashTable as *mut u8,
            0,
            ldmHSize.wrapping_mul(size_of::<LdmEntry>()),
        );
        (*zc).ldmSequences =
            ZSTD_cwksp_reserve_aligned64(ws, maxNbLdmSeq.wrapping_mul(size_of::<rawSeq>()))
                as *mut rawSeq;
        (*zc).maxNbLdmSequences = maxNbLdmSeq;

        (*zc).ldmState.window.init();
        (*zc).ldmState.loadedDictEnd = 0;
    }

    // reserve space for block-level external sequences
    if (*zc).appliedParams.has_ext_seq_prod() {
        let maxNbExternalSeq = ZSTD_sequenceBound(blockSize);
        (*zc).extSeqBufCapacity = maxNbExternalSeq;
        (*zc).extSeqBuf = ZSTD_cwksp_reserve_aligned64(
            ws,
            maxNbExternalSeq.wrapping_mul(size_of::<ZSTD_Sequence>()),
        ) as *mut ZSTD_Sequence;
    }

    // buffers
    // ZSTD_wildcopy() is used to copy into the literals buffer,
    // so we have to oversize the buffer by WILDCOPY_OVERLENGTH bytes.
    (*zc).seqStore.litStart =
        ZSTD_cwksp_reserve_buffer(ws, blockSize.wrapping_add(WILDCOPY_OVERLENGTH));
    (*zc).seqStore.maxNbLit = blockSize;

    (*zc).bufferedPolicy = zbuff;
    (*zc).inBuffSize = buffInSize;
    (*zc).inBuff = ZSTD_cwksp_reserve_buffer(ws, buffInSize);
    (*zc).outBuffSize = buffOutSize;
    (*zc).outBuff = ZSTD_cwksp_reserve_buffer(ws, buffOutSize);

    // ldm bucketOffsets table
    if params.ldmParams.enableLdm == ParamSwitch::Enable {
        // TODO: avoid memset?
        let numBuckets =
            1 << (params.ldmParams.hashLog).wrapping_sub(params.ldmParams.bucketSizeLog);
        (*zc).ldmState.bucketOffsets = ZSTD_cwksp_reserve_buffer(ws, numBuckets);
        ptr::write_bytes((*zc).ldmState.bucketOffsets, 0, numBuckets);
    }

    // sequences storage
    ZSTD_referenceExternalSequences(zc, core::ptr::null_mut(), 0);
    (*zc).seqStore.maxNbSeq = maxNbSeq;
    (*zc).seqStore.llCode = ZSTD_cwksp_reserve_buffer(ws, maxNbSeq.wrapping_mul(size_of::<u8>()));
    (*zc).seqStore.mlCode = ZSTD_cwksp_reserve_buffer(ws, maxNbSeq.wrapping_mul(size_of::<u8>()));
    (*zc).seqStore.ofCode = ZSTD_cwksp_reserve_buffer(ws, maxNbSeq.wrapping_mul(size_of::<u8>()));
    (*zc).initialized = 1;

    Ok(())
}

/// Ensures next compression will not use repcodes from previous block.
///
/// Note: only works with regular variant; do not use with extDict variant!
pub unsafe fn ZSTD_invalidateRepCodes(cctx: *mut ZSTD_CCtx) {
    (*(*cctx).blockState.prevCBlock).rep.fill(0);
}

/// Approximate sizes for each strategy past which copying the dictionary tables into the working
/// context is faster than using them in-place.
static attachDictSizeCutoffs: [usize; 10] = [
    8 * (1 << 10),
    8 * (1 << 10),
    16 * (1 << 10),
    32 * (1 << 10),
    32 * (1 << 10),
    32 * (1 << 10),
    32 * (1 << 10),
    32 * (1 << 10),
    8 * (1 << 10),
    8 * (1 << 10),
];

unsafe fn ZSTD_shouldAttachDict(
    cdict: *const ZSTD_CDict,
    params: &ZSTD_CCtx_params,
    pledgedSrcSize: u64,
) -> bool {
    let cutoff = attachDictSizeCutoffs[(*cdict).matchState.cParams.strategy as usize];
    let dedicatedDictSearch = (*cdict).matchState.dedicatedDictSearch;
    dedicatedDictSearch != 0
        || (pledgedSrcSize <= cutoff as u64
            || pledgedSrcSize as core::ffi::c_ulonglong == ZSTD_CONTENTSIZE_UNKNOWN
            || params.attachDictPref == ZSTD_dictAttachPref_e::ZSTD_dictForceAttach)
            && params.attachDictPref != ZSTD_dictAttachPref_e::ZSTD_dictForceCopy
            && params.forceWindow == 0
}

unsafe fn ZSTD_resetCCtx_byAttachingCDict(
    cctx: *mut ZSTD_CCtx,
    cdict: *const ZSTD_CDict,
    mut params: ZSTD_CCtx_params,
    pledgedSrcSize: u64,
    zbuff: BufferedPolicy,
) -> Result<(), Error> {
    let mut adjusted_cdict_cParams = (*cdict).matchState.cParams;
    let windowLog = params.cParams.windowLog;

    if (*cdict).matchState.dedicatedDictSearch != 0 {
        adjusted_cdict_cParams.revert_dedicated_dict_search();
    }

    params.cParams = adjusted_cdict_cParams.optimize(
        pledgedSrcSize,
        (*cdict).dictContentSize,
        CParamMode::AttachDict,
        params.useRowMatchFinder,
    );
    params.cParams.windowLog = windowLog;
    params.useRowMatchFinder = (*cdict).useRowMatchFinder;
    ZSTD_resetCCtx_internal(
        cctx,
        &params,
        pledgedSrcSize,
        0,
        CompResetPolicy::MakeClean,
        zbuff,
    )?;

    let cdictEnd = ((*cdict).matchState.window.nextSrc).offset_from((*cdict).matchState.window.base)
        as core::ffi::c_long as u32;
    let cdictLen = cdictEnd.wrapping_sub((*cdict).matchState.window.dictLimit);
    if cdictLen != 0 {
        (*cctx).blockState.matchState.dictMatchState = &(*cdict).matchState;

        // prep working match state so dict matches never have negative indices
        // when they are translated to the working context's index space.
        if (*cctx).blockState.matchState.window.dictLimit < cdictEnd {
            (*cctx).blockState.matchState.window.nextSrc =
                ((*cctx).blockState.matchState.window.base).wrapping_offset(cdictEnd as isize);
            (*cctx).blockState.matchState.window.clear();
        }
        // loadedDictEnd is expressed within the referential of the active context
        (*cctx).blockState.matchState.loadedDictEnd =
            (*cctx).blockState.matchState.window.dictLimit;
    }

    (*cctx).dictID = (*cdict).dictID;
    (*cctx).dictContentSize = (*cdict).dictContentSize;

    // copy block state
    core::ptr::copy_nonoverlapping(
        &raw const (*cdict).cBlockState,
        (*cctx).blockState.prevCBlock,
        1,
    );

    Ok(())
}

unsafe fn ZSTD_copyCDictTableIntoCCtx(
    dst: *mut u32,
    src: *const u32,
    tableSize: usize,
    cParams: &ZSTD_compressionParameters,
) {
    if cParams.cdict_indices_are_tagged() {
        // Remove tags from the CDict table if they are present.
        // See docs on "short cache" in zstd_compress_internal.h for context.
        for i in 0..tableSize {
            let taggedIndex = *src.add(i);
            let index = taggedIndex >> ZSTD_SHORT_CACHE_TAG_BITS;
            *dst.add(i) = index;
        }
    } else {
        core::ptr::copy_nonoverlapping(src, dst, tableSize);
    }
}

unsafe fn ZSTD_resetCCtx_byCopyingCDict(
    cctx: *mut ZSTD_CCtx,
    cdict: *const ZSTD_CDict,
    mut params: ZSTD_CCtx_params,
    pledgedSrcSize: u64,
    zbuff: BufferedPolicy,
) -> Result<(), Error> {
    let cdict_cParams = &(*cdict).matchState.cParams;

    let windowLog = params.cParams.windowLog;
    params.cParams = *cdict_cParams;
    params.cParams.windowLog = windowLog;
    params.useRowMatchFinder = (*cdict).useRowMatchFinder;
    ZSTD_resetCCtx_internal(
        cctx,
        &params,
        pledgedSrcSize,
        0,
        CompResetPolicy::LeaveDirty,
        zbuff,
    )?;

    ZSTD_cwksp_mark_tables_dirty(&mut (*cctx).workspace);

    // copy tables
    let chainSize =
        if ZSTD_allocateChainTable(cdict_cParams.strategy, (*cdict).useRowMatchFinder, false) {
            1 << cdict_cParams.chainLog
        } else {
            0
        };
    let hSize = 1 << cdict_cParams.hashLog;
    ZSTD_copyCDictTableIntoCCtx(
        (*cctx).blockState.matchState.hashTable,
        (*cdict).matchState.hashTable,
        hSize,
        cdict_cParams,
    );
    // Do not copy cdict's chainTable if cctx has parameters such that it would not use chainTable
    if ZSTD_allocateChainTable(
        (*cctx).appliedParams.cParams.strategy,
        (*cctx).appliedParams.useRowMatchFinder,
        false,
    ) {
        ZSTD_copyCDictTableIntoCCtx(
            (*cctx).blockState.matchState.chainTable,
            (*cdict).matchState.chainTable,
            chainSize,
            cdict_cParams,
        );
    }
    // copy tag table
    if ZSTD_rowMatchFinderUsed(cdict_cParams.strategy, (*cdict).useRowMatchFinder) {
        let tagTableSize = hSize;
        core::ptr::copy_nonoverlapping(
            (*cdict).matchState.tagTable,
            (*cctx).blockState.matchState.tagTable,
            tagTableSize,
        );
        (*cctx).blockState.matchState.hashSalt = (*cdict).matchState.hashSalt;
    }

    let h3log = (*cctx).blockState.matchState.hashLog3;
    let h3Size = if h3log != 0 { 1usize << h3log } else { 0 };
    ptr::write_bytes(
        (*cctx).blockState.matchState.hashTable3 as *mut u8,
        0,
        h3Size.wrapping_mul(size_of::<u32>()),
    );

    ZSTD_cwksp_mark_tables_clean(&mut (*cctx).workspace);

    // copy dictionary offsets
    let srcMatchState: *const ZSTD_MatchState_t = &(*cdict).matchState;
    let dstMatchState: &mut ZSTD_MatchState_t = &mut (*cctx).blockState.matchState;
    dstMatchState.window = (*srcMatchState).window;
    dstMatchState.nextToUpdate = (*srcMatchState).nextToUpdate;
    dstMatchState.loadedDictEnd = (*srcMatchState).loadedDictEnd;

    (*cctx).dictID = (*cdict).dictID;
    (*cctx).dictContentSize = (*cdict).dictContentSize;

    // copy block state
    core::ptr::copy_nonoverlapping(
        &raw const (*cdict).cBlockState,
        (*cctx).blockState.prevCBlock,
        1,
    );

    Ok(())
}

/// We have a choice between copying the dictionary context into the working context,
/// or referencing the dictionary context from the working context in-place.
/// We decide here which strategy to use.
unsafe fn ZSTD_resetCCtx_usingCDict(
    cctx: *mut ZSTD_CCtx,
    cdict: *const ZSTD_CDict,
    params: &ZSTD_CCtx_params,
    pledgedSrcSize: u64,
    zbuff: BufferedPolicy,
) -> Result<(), Error> {
    if ZSTD_shouldAttachDict(cdict, params, pledgedSrcSize) {
        ZSTD_resetCCtx_byAttachingCDict(cctx, cdict, *params, pledgedSrcSize, zbuff)
    } else {
        ZSTD_resetCCtx_byCopyingCDict(cctx, cdict, *params, pledgedSrcSize, zbuff)
    }
}

/// Duplicate an existing context `srcCCtx` into another one `dstCCtx`.
/// Only works during stage `CompressionStage::Init` (i.e. after creation, but
/// before first call to `ZSTD_compressContinue`).
/// The "context", in this case, refers to the hash and chain tables,
/// entropy tables, and dictionary references.
/// `windowLog` value is enforced if != 0, otherwise value is copied from srcCCtx.
unsafe fn ZSTD_copyCCtx_internal(
    dstCCtx: *mut ZSTD_CCtx,
    srcCCtx: *const ZSTD_CCtx,
    fParams: ZSTD_frameParameters,
    pledgedSrcSize: u64,
    zbuff: BufferedPolicy,
) -> Result<(), Error> {
    if (*srcCCtx).stage != CompressionStage::Init {
        return Err(Error::stage_wrong);
    }
    (*dstCCtx).customMem = (*srcCCtx).customMem;

    let mut params = (*dstCCtx).requestedParams;
    // Copy only compression parameters related to tables.
    params.cParams = (*srcCCtx).appliedParams.cParams;
    params.useRowMatchFinder = (*srcCCtx).appliedParams.useRowMatchFinder;
    params.postBlockSplitter = (*srcCCtx).appliedParams.postBlockSplitter;
    params.ldmParams = (*srcCCtx).appliedParams.ldmParams;
    params.fParams = fParams;
    params.maxBlockSize = (*srcCCtx).appliedParams.maxBlockSize;
    ZSTD_resetCCtx_internal(
        dstCCtx,
        &params,
        pledgedSrcSize,
        0,
        CompResetPolicy::LeaveDirty,
        zbuff,
    )?;

    ZSTD_cwksp_mark_tables_dirty(&mut (*dstCCtx).workspace);

    // copy tables
    let chainSize = if ZSTD_allocateChainTable(
        (*srcCCtx).appliedParams.cParams.strategy,
        (*srcCCtx).appliedParams.useRowMatchFinder,
        false,
    ) {
        1usize << (*srcCCtx).appliedParams.cParams.chainLog
    } else {
        0
    };
    let hSize = 1usize << (*srcCCtx).appliedParams.cParams.hashLog;
    let h3log = (*srcCCtx).blockState.matchState.hashLog3;
    let h3Size = if h3log != 0 { 1usize << h3log } else { 0 };
    core::ptr::copy_nonoverlapping(
        (*srcCCtx).blockState.matchState.hashTable,
        (*dstCCtx).blockState.matchState.hashTable,
        hSize,
    );
    core::ptr::copy_nonoverlapping(
        (*srcCCtx).blockState.matchState.chainTable,
        (*dstCCtx).blockState.matchState.chainTable,
        chainSize,
    );
    core::ptr::copy_nonoverlapping(
        (*srcCCtx).blockState.matchState.hashTable3,
        (*dstCCtx).blockState.matchState.hashTable3,
        h3Size,
    );

    ZSTD_cwksp_mark_tables_clean(&mut (*dstCCtx).workspace);

    // copy dictionary offsets
    let srcMatchState: *const ZSTD_MatchState_t = &(*srcCCtx).blockState.matchState;
    let dstMatchState: &mut ZSTD_MatchState_t = &mut (*dstCCtx).blockState.matchState;
    dstMatchState.window = (*srcMatchState).window;
    dstMatchState.nextToUpdate = (*srcMatchState).nextToUpdate;
    dstMatchState.loadedDictEnd = (*srcMatchState).loadedDictEnd;

    (*dstCCtx).dictID = (*srcCCtx).dictID;
    (*dstCCtx).dictContentSize = (*srcCCtx).dictContentSize;

    // copy block state
    core::ptr::copy_nonoverlapping(
        (*srcCCtx).blockState.prevCBlock,
        (*dstCCtx).blockState.prevCBlock,
        1,
    );

    Ok(())
}

#[deprecated(
    note = "This function will likely be removed in a future release. It is misleading and has very limited utility."
)]
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_copyCCtx))]
pub unsafe extern "C" fn ZSTD_copyCCtx(
    dstCCtx: *mut ZSTD_CCtx,
    srcCCtx: *const ZSTD_CCtx,
    mut pledgedSrcSize: core::ffi::c_ulonglong,
) -> usize {
    let mut fParams = {
        ZSTD_frameParameters {
            contentSizeFlag: 1,
            checksumFlag: 0,
            noDictIDFlag: 0,
        }
    };
    let zbuff = (*srcCCtx).bufferedPolicy;
    if pledgedSrcSize == 0 {
        pledgedSrcSize = ZSTD_CONTENTSIZE_UNKNOWN;
    }
    fParams.contentSizeFlag = core::ffi::c_int::from(pledgedSrcSize != ZSTD_CONTENTSIZE_UNKNOWN);

    match ZSTD_copyCCtx_internal(dstCCtx, srcCCtx, fParams, pledgedSrcSize, zbuff) {
        Ok(()) => 0,
        Err(err) => err.to_error_code(),
    }
}

pub const ZSTD_ROWSIZE: core::ffi::c_int = 16;

/// Reduce table indexes by `reducerValue`, or squash to zero.
/// PreserveMark preserves "unsorted mark" for btlazy2 strategy.
/// Presume table size is a multiple of ZSTD_ROWSIZE to help auto-vectorization.
#[inline(always)]
fn ZSTD_reduceTable_internal(table: &mut [u32], reducerValue: u32, preserveMark: bool) {
    // Protect special index values < ZSTD_WINDOW_START_INDEX.
    let reducerThreshold = reducerValue.wrapping_add(ZSTD_WINDOW_START_INDEX as u32);

    for row in table.as_chunks_mut::<{ ZSTD_ROWSIZE as usize }>().0 {
        for cell in row {
            *cell = if preserveMark && *cell == ZSTD_DUBT_UNSORTED_MARK as u32 {
                ZSTD_DUBT_UNSORTED_MARK as u32
            } else if *cell < reducerThreshold {
                0
            } else {
                (*cell).wrapping_sub(reducerValue)
            };
        }
    }
}

fn ZSTD_reduceTable(table: &mut [u32], reducerValue: u32) {
    ZSTD_reduceTable_internal(table, reducerValue, false);
}

fn ZSTD_reduceTable_btlazy2(table: &mut [u32], reducerValue: u32) {
    ZSTD_reduceTable_internal(table, reducerValue, true);
}

/// Rescale all indexes to avoid future overflow (indexes are U32).
unsafe fn ZSTD_reduceIndex(
    ms: &mut ZSTD_MatchState_t,
    params: &ZSTD_CCtx_params,
    reducerValue: u32,
) {
    let hSize = 1usize << params.cParams.hashLog;
    ZSTD_reduceTable(
        core::slice::from_raw_parts_mut(ms.hashTable, hSize),
        reducerValue,
    );

    if ZSTD_allocateChainTable(
        params.cParams.strategy,
        params.useRowMatchFinder,
        ms.dedicatedDictSearch != 0,
    ) {
        let chainSize = 1usize << params.cParams.chainLog;
        let chainTable = core::slice::from_raw_parts_mut(ms.chainTable, chainSize);
        if params.cParams.strategy == ZSTD_btlazy2 {
            ZSTD_reduceTable_btlazy2(chainTable, reducerValue);
        } else {
            ZSTD_reduceTable(chainTable, reducerValue);
        }
    }

    if ms.hashLog3 != 0 {
        let h3Size = 1usize << ms.hashLog3;
        ZSTD_reduceTable(
            core::slice::from_raw_parts_mut(ms.hashTable3, h3Size),
            reducerValue,
        );
    }
}

/// See doc/zstd_compression_format.md for detailed format description
#[inline(never)]
pub unsafe fn ZSTD_seqToCodes(seqStorePtr: &SeqStore_t) -> bool {
    let sequences: *const SeqDef = seqStorePtr.sequencesStart;
    let llCodeTable = seqStorePtr.llCode;
    let ofCodeTable = seqStorePtr.ofCode;
    let mlCodeTable = seqStorePtr.mlCode;
    let nbSeq = (seqStorePtr.sequences).offset_from_unsigned(seqStorePtr.sequencesStart);
    let mut longOffsets = false;
    for u in 0..nbSeq {
        let llv = (*sequences.add(u)).litLength as u32;
        let ofCode = ZSTD_highbit32((*sequences.add(u)).offBase);
        let mlv = (*sequences.add(u)).mlBase as u32;
        *llCodeTable.add(u) = ZSTD_LLcode(llv) as u8;
        *ofCodeTable.add(u) = ofCode as u8;
        *mlCodeTable.add(u) = ZSTD_MLcode(mlv) as u8;
        if MEM_32bits() && ofCode >= STREAM_ACCUMULATOR_MIN {
            longOffsets = true;
        }
    }
    if seqStorePtr.longLengthType == LongLengthType::Literal {
        *llCodeTable.offset(seqStorePtr.longLengthPos as isize) = MaxLL;
    }
    if seqStorePtr.longLengthType == LongLengthType::Match {
        *mlCodeTable.offset(seqStorePtr.longLengthPos as isize) = MaxML;
    }
    longOffsets
}

/// Returns a ZSTD_symbolEncodingTypeStats_t, or a zstd error.
/// Modifies `nextEntropy` to have the appropriate values as a side effect.
/// nbSeq must be greater than 0.
///
/// entropyWkspSize must be of size at least ENTROPY_WORKSPACE_SIZE - (MaxSeq + 1)*sizeof(U32)
unsafe fn ZSTD_buildSequencesStatistics(
    seqStorePtr: &SeqStore_t,
    nbSeq: usize,
    prevEntropy: &ZSTD_fseCTables_t,
    nextEntropy: &mut ZSTD_fseCTables_t,
    dst: *mut u8,
    dstEnd: *const u8,
    strategy: ZSTD_strategy,
    countWorkspace: *mut core::ffi::c_uint,
    entropyWorkspace: *mut core::ffi::c_void,
    entropyWkspSize: usize,
) -> Result<ZSTD_symbolEncodingTypeStats_t, Error> {
    let ostart = dst;
    let oend = dstEnd;
    let mut op = ostart;
    // convert length/distances into codes
    let mut stats = ZSTD_symbolEncodingTypeStats_t {
        longOffsets: ZSTD_seqToCodes(seqStorePtr),
        ..Default::default()
    };

    // build CTable for Literal Lengths
    let mut max = MaxLL;
    let mostFrequent = HIST_countFast_wksp(
        countWorkspace,
        &mut max,
        seqStorePtr.llCodes(),
        entropyWorkspace,
        entropyWkspSize,
    )
    .expect("can't fail") as usize;
    // `HIST_countFast_wksp` has filled every entry of the count table up to `max`.
    let count = core::slice::from_raw_parts_mut(countWorkspace, usize::from(max) + 1);
    nextEntropy.litlength_repeatMode = prevEntropy.litlength_repeatMode;
    stats.LLtype = ZSTD_selectEncodingType(
        &mut nextEntropy.litlength_repeatMode,
        count,
        max,
        mostFrequent,
        nbSeq,
        LLFSELog,
        &prevEntropy.litlengthCTable,
        &LL_defaultNorm,
        LL_defaultNormLog,
        DefaultPolicy::Allowed,
        strategy,
    );
    let countSize = ZSTD_buildCTable(
        op as *mut core::ffi::c_void,
        oend.offset_from_unsigned(op),
        &mut nextEntropy.litlengthCTable,
        LLFSELog,
        stats.LLtype,
        count,
        max,
        seqStorePtr.llCodes(),
        &LL_defaultNorm,
        LL_defaultNormLog,
        MaxLL,
        &prevEntropy.litlengthCTable,
        entropyWorkspace,
        entropyWkspSize,
    )?;
    if stats.LLtype == SymbolEncodingType::Compressed {
        stats.lastCountSize = countSize;
    }
    op = op.add(countSize);

    // build CTable for Offsets
    let mut max_0 = MaxOff;
    let mostFrequent_0 = HIST_countFast_wksp(
        countWorkspace,
        &mut max_0,
        seqStorePtr.ofCodes(),
        entropyWorkspace,
        entropyWkspSize,
    )
    .expect("can't fail") as usize;
    // `HIST_countFast_wksp` has filled every entry of the count table up to `max_0`.
    let count = core::slice::from_raw_parts_mut(countWorkspace, usize::from(max_0) + 1);
    // We can only use the basic table if max <= DefaultMaxOff, otherwise the offsets are too large
    let defaultPolicy = if max_0 <= DefaultMaxOff {
        DefaultPolicy::Allowed
    } else {
        DefaultPolicy::Disallowed
    };
    nextEntropy.offcode_repeatMode = prevEntropy.offcode_repeatMode;
    stats.Offtype = ZSTD_selectEncodingType(
        &mut nextEntropy.offcode_repeatMode,
        count,
        max_0,
        mostFrequent_0,
        nbSeq,
        OffFSELog,
        &prevEntropy.offcodeCTable,
        &OF_defaultNorm,
        OF_defaultNormLog,
        defaultPolicy,
        strategy,
    );
    let countSize = ZSTD_buildCTable(
        op as *mut core::ffi::c_void,
        oend.offset_from_unsigned(op),
        &mut nextEntropy.offcodeCTable,
        OffFSELog,
        stats.Offtype,
        count,
        max_0,
        seqStorePtr.ofCodes(),
        &OF_defaultNorm,
        OF_defaultNormLog,
        DefaultMaxOff,
        &prevEntropy.offcodeCTable,
        entropyWorkspace,
        entropyWkspSize,
    )?;
    if stats.Offtype == SymbolEncodingType::Compressed {
        stats.lastCountSize = countSize;
    }
    op = op.add(countSize);

    // build CTable for MatchLengths
    let mut max_1 = MaxML;
    let mostFrequent_1 = HIST_countFast_wksp(
        countWorkspace,
        &mut max_1,
        seqStorePtr.mlCodes(),
        entropyWorkspace,
        entropyWkspSize,
    )
    .expect("can't fail") as usize;
    // `HIST_countFast_wksp` has filled every entry of the count table up to `max_1`.
    let count = core::slice::from_raw_parts_mut(countWorkspace, usize::from(max_1) + 1);
    nextEntropy.matchlength_repeatMode = prevEntropy.matchlength_repeatMode;
    stats.MLtype = ZSTD_selectEncodingType(
        &mut nextEntropy.matchlength_repeatMode,
        count,
        max_1,
        mostFrequent_1,
        nbSeq,
        MLFSELog,
        &prevEntropy.matchlengthCTable,
        &ML_defaultNorm,
        ML_defaultNormLog,
        DefaultPolicy::Allowed,
        strategy,
    );
    let countSize = ZSTD_buildCTable(
        op as *mut core::ffi::c_void,
        oend.offset_from_unsigned(op),
        &mut nextEntropy.matchlengthCTable,
        MLFSELog,
        stats.MLtype,
        count,
        max_1,
        seqStorePtr.mlCodes(),
        &ML_defaultNorm,
        ML_defaultNormLog,
        MaxML,
        &prevEntropy.matchlengthCTable,
        entropyWorkspace,
        entropyWkspSize,
    )?;
    if stats.MLtype == SymbolEncodingType::Compressed {
        stats.lastCountSize = countSize;
    }
    op = op.add(countSize);

    stats.size = op.offset_from_unsigned(ostart);

    Ok(stats)
}

pub const SUSPECT_UNCOMPRESSIBLE_LITERAL_RATIO: core::ffi::c_int = 20;

/// Compresses both literals and sequences.
///
/// # Returns
///
/// - The compressed size of block
/// - Or a zstd error
#[inline]
unsafe fn ZSTD_entropyCompressSeqStore_internal(
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    literals: &[u8],
    seqStorePtr: &SeqStore_t,
    prevEntropy: &ZSTD_entropyCTables_t,
    nextEntropy: &mut ZSTD_entropyCTables_t,
    cctxParams: &ZSTD_CCtx_params,
    mut entropyWorkspace: *mut core::ffi::c_void,
    mut entropyWkspSize: usize,
    bmi2: bool,
) -> Result<usize, Error> {
    let strategy = cctxParams.cParams.strategy;
    let count = entropyWorkspace as *mut core::ffi::c_uint;
    let sequences = seqStorePtr.seqs();
    let nbSeq = sequences.len();
    let ostart = dst as *mut u8;
    let oend = ostart.add(dstCapacity);
    let mut op = ostart;

    entropyWorkspace = count.add(MaxSeq + 1) as *mut core::ffi::c_void;
    entropyWkspSize =
        entropyWkspSize.wrapping_sub((MaxSeq + 1).wrapping_mul(size_of::<core::ffi::c_uint>()));

    // Compress literals
    let numSequences = seqStorePtr
        .sequences
        .offset_from_unsigned(seqStorePtr.sequencesStart);
    // Base suspicion of uncompressibility on ratio of literals to sequences
    let suspectUncompressible = numSequences == 0
        || literals.len() / numSequences >= SUSPECT_UNCOMPRESSIBLE_LITERAL_RATIO as usize;

    let cSize = ZSTD_compressLiterals(
        op as *mut core::ffi::c_void,
        dstCapacity,
        literals,
        entropyWorkspace,
        entropyWkspSize,
        &prevEntropy.huf,
        &mut nextEntropy.huf,
        cctxParams.cParams.strategy,
        cctxParams.literals_compression_is_disabled(),
        suspectUncompressible,
        bmi2,
    )?;
    op = op.add(cSize);

    // Sequences Header
    if oend.offset_from_unsigned(op) < 3 + 1 {
        return Err(Error::dstSize_tooSmall);
    }
    if nbSeq < 128 {
        *op = nbSeq as u8;
        op = op.add(1);
    } else if nbSeq < LONGNBSEQ as usize {
        *op = (nbSeq >> 8).wrapping_add(0x80) as u8;
        *op.add(1) = nbSeq as u8;
        op = op.add(2);
    } else {
        *op = 0xff;
        MEM_writeLE16(
            op.add(1) as *mut core::ffi::c_void,
            nbSeq.wrapping_sub(LONGNBSEQ as usize) as u16,
        );
        op = op.add(3);
    }
    if nbSeq == 0 {
        // Copy the old tables over as if we repeated them
        core::ptr::copy_nonoverlapping(&raw const prevEntropy.fse, &raw mut nextEntropy.fse, 1);
        return Ok(op.offset_from_unsigned(ostart));
    }

    let seqHead = op;
    op = op.add(1);
    // build stats for sequences
    let stats = ZSTD_buildSequencesStatistics(
        seqStorePtr,
        nbSeq,
        &prevEntropy.fse,
        &mut nextEntropy.fse,
        op,
        oend,
        strategy,
        count,
        entropyWorkspace,
        entropyWkspSize,
    )?;
    *seqHead = ((stats.LLtype as u32) << 6)
        .wrapping_add((stats.Offtype as u32) << 4)
        .wrapping_add((stats.MLtype as u32) << 2) as u8;
    let lastCountSize = stats.lastCountSize;
    op = op.add(stats.size);
    let longOffsets = stats.longOffsets;

    let bitstreamSize = ZSTD_encodeSequences(
        op as *mut core::ffi::c_void,
        oend.offset_from_unsigned(op),
        &nextEntropy.fse.matchlengthCTable,
        seqStorePtr.mlCodes(),
        &nextEntropy.fse.offcodeCTable,
        seqStorePtr.ofCodes(),
        &nextEntropy.fse.litlengthCTable,
        seqStorePtr.llCodes(),
        sequences,
        longOffsets,
        bmi2,
    )?;
    op = op.add(bitstreamSize);
    // zstd versions <= 1.3.4 mistakenly report corruption when
    // FSE_readNCount() receives a buffer < 4 bytes.
    // Fixed by https://github.com/facebook/zstd/pull/1146.
    // This can happen when the last SymbolEncodingType::Compressed table present is 2
    // bytes and the bitstream is only one byte.
    // In this exceedingly rare case, we will simply emit an uncompressed
    // block, since it isn't worth optimizing.
    if lastCountSize != 0 && lastCountSize.wrapping_add(bitstreamSize) < 4 {
        // lastCountSize >= 2 && bitstreamSize > 0 ==> lastCountSize == 3
        return Ok(0);
    }

    Ok(op.offset_from_unsigned(ostart))
}

unsafe fn ZSTD_entropyCompressSeqStore_wExtLitBuffer(
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    literals: &[u8],
    blockSize: usize,
    seqStorePtr: &SeqStore_t,
    prevEntropy: &ZSTD_entropyCTables_t,
    nextEntropy: &mut ZSTD_entropyCTables_t,
    cctxParams: &ZSTD_CCtx_params,
    entropyWorkspace: *mut core::ffi::c_void,
    entropyWkspSize: usize,
    bmi2: bool,
) -> Result<usize, Error> {
    let cSize = match ZSTD_entropyCompressSeqStore_internal(
        dst,
        dstCapacity,
        literals,
        seqStorePtr,
        prevEntropy,
        nextEntropy,
        cctxParams,
        entropyWorkspace,
        entropyWkspSize,
        bmi2,
    ) {
        Ok(0) => return Ok(0),
        Ok(cSize) => cSize,
        // When srcSize <= dstCapacity, there is enough space to write a raw uncompressed block.
        // Since we ran out of space, block must be not compressible, so fall back to raw uncompressed block.
        Err(Error::dstSize_tooSmall) if blockSize <= dstCapacity => return Ok(0),
        Err(err) => return Err(err),
    };

    // Check compressibility
    let maxCSize = blockSize.wrapping_sub(ZSTD_minGain(blockSize, cctxParams.cParams.strategy));
    if cSize >= maxCSize {
        return Ok(0);
    }
    Ok(cSize)
}

unsafe fn ZSTD_entropyCompressSeqStore(
    seqStorePtr: &SeqStore_t,
    prevEntropy: &ZSTD_entropyCTables_t,
    nextEntropy: &mut ZSTD_entropyCTables_t,
    cctxParams: &ZSTD_CCtx_params,
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    srcSize: usize,
    entropyWorkspace: *mut core::ffi::c_void,
    entropyWkspSize: usize,
    bmi2: bool,
) -> Result<usize, Error> {
    ZSTD_entropyCompressSeqStore_wExtLitBuffer(
        dst,
        dstCapacity,
        core::slice::from_raw_parts(
            seqStorePtr.litStart,
            seqStorePtr.lit.offset_from_unsigned(seqStorePtr.litStart),
        ),
        srcSize,
        seqStorePtr,
        prevEntropy,
        nextEntropy,
        cctxParams,
        entropyWorkspace,
        entropyWkspSize,
        bmi2,
    )
}

/// Not static, but internal use only (used by long distance matcher).
///
/// Assumption: strat is a valid strategy
pub fn ZSTD_selectBlockCompressor(
    strat: ZSTD_strategy,
    useRowMatchFinder: ParamSwitch,
    dictMode: DictMode,
) -> Option<ZSTD_BlockCompressor_f> {
    if ZSTD_rowMatchFinderUsed(strat, useRowMatchFinder) {
        match dictMode {
            DictMode::NoDict => match strat {
                ZSTD_greedy => Some(ZSTD_compressBlock_greedy_row),
                ZSTD_lazy => Some(ZSTD_compressBlock_lazy_row),
                ZSTD_lazy2 => Some(ZSTD_compressBlock_lazy2_row),
                _ => unreachable!(),
            },
            DictMode::ExtDict => match strat {
                ZSTD_greedy => Some(ZSTD_compressBlock_greedy_extDict_row),
                ZSTD_lazy => Some(ZSTD_compressBlock_lazy_extDict_row),
                ZSTD_lazy2 => Some(ZSTD_compressBlock_lazy2_extDict_row),
                _ => unreachable!(),
            },
            DictMode::DictMatchState => match strat {
                ZSTD_greedy => Some(ZSTD_compressBlock_greedy_dictMatchState_row),
                ZSTD_lazy => Some(ZSTD_compressBlock_lazy_dictMatchState_row),
                ZSTD_lazy2 => Some(ZSTD_compressBlock_lazy2_dictMatchState_row),
                _ => unreachable!(),
            },
            DictMode::DedicatedDictSearch => match strat {
                ZSTD_greedy => Some(ZSTD_compressBlock_greedy_dedicatedDictSearch_row),
                ZSTD_lazy => Some(ZSTD_compressBlock_lazy_dedicatedDictSearch_row),
                ZSTD_lazy2 => Some(ZSTD_compressBlock_lazy2_dedicatedDictSearch_row),
                _ => unreachable!(),
            },
        }
    } else {
        match dictMode {
            DictMode::NoDict => match strat {
                0 => Some(ZSTD_compressBlock_fast),
                ZSTD_fast => Some(ZSTD_compressBlock_fast),
                ZSTD_dfast => Some(ZSTD_compressBlock_doubleFast),
                ZSTD_greedy => Some(ZSTD_compressBlock_greedy),
                ZSTD_lazy => Some(ZSTD_compressBlock_lazy),
                ZSTD_lazy2 => Some(ZSTD_compressBlock_lazy2),
                ZSTD_btlazy2 => Some(ZSTD_compressBlock_btlazy2),
                ZSTD_btopt => Some(ZSTD_compressBlock_btopt),
                ZSTD_btultra => Some(ZSTD_compressBlock_btultra),
                ZSTD_btultra2 => Some(ZSTD_compressBlock_btultra2),
                _ => unreachable!(),
            },
            DictMode::ExtDict => match strat {
                0 => Some(ZSTD_compressBlock_fast_extDict),
                ZSTD_fast => Some(ZSTD_compressBlock_fast_extDict),
                ZSTD_dfast => Some(ZSTD_compressBlock_doubleFast_extDict),
                ZSTD_greedy => Some(ZSTD_compressBlock_greedy_extDict),
                ZSTD_lazy => Some(ZSTD_compressBlock_lazy_extDict),
                ZSTD_lazy2 => Some(ZSTD_compressBlock_lazy2_extDict),
                ZSTD_btlazy2 => Some(ZSTD_compressBlock_btlazy2_extDict),
                ZSTD_btopt => Some(ZSTD_compressBlock_btopt_extDict),
                ZSTD_btultra => Some(ZSTD_compressBlock_btultra_extDict),
                ZSTD_btultra2 => Some(ZSTD_compressBlock_btultra_extDict),
                _ => unreachable!(),
            },
            DictMode::DictMatchState => match strat {
                0 => Some(ZSTD_compressBlock_fast_dictMatchState),
                ZSTD_fast => Some(ZSTD_compressBlock_fast_dictMatchState),
                ZSTD_dfast => Some(ZSTD_compressBlock_doubleFast_dictMatchState),
                ZSTD_greedy => Some(ZSTD_compressBlock_greedy_dictMatchState),
                ZSTD_lazy => Some(ZSTD_compressBlock_lazy_dictMatchState),
                ZSTD_lazy2 => Some(ZSTD_compressBlock_lazy2_dictMatchState),
                ZSTD_btlazy2 => Some(ZSTD_compressBlock_btlazy2_dictMatchState),
                ZSTD_btopt => Some(ZSTD_compressBlock_btopt_dictMatchState),
                ZSTD_btultra => Some(ZSTD_compressBlock_btultra_dictMatchState),
                ZSTD_btultra2 => Some(ZSTD_compressBlock_btultra_dictMatchState),
                _ => unreachable!(),
            },
            DictMode::DedicatedDictSearch => match strat {
                0 => None,
                ZSTD_fast => None,
                ZSTD_dfast => None,
                ZSTD_greedy => Some(ZSTD_compressBlock_greedy_dedicatedDictSearch),
                ZSTD_lazy => Some(ZSTD_compressBlock_lazy_dedicatedDictSearch),
                ZSTD_lazy2 => Some(ZSTD_compressBlock_lazy2_dedicatedDictSearch),
                ZSTD_btlazy2 => None,
                ZSTD_btopt => None,
                ZSTD_btultra => None,
                ZSTD_btultra2 => None,
                _ => unreachable!(),
            },
        }
    }
}

unsafe fn ZSTD_storeLastLiterals(
    seqStorePtr: &mut SeqStore_t,
    anchor: *const u8,
    lastLLSize: usize,
) {
    core::ptr::copy_nonoverlapping(anchor, seqStorePtr.lit, lastLLSize);
    seqStorePtr.lit = seqStorePtr.lit.add(lastLLSize);
}

pub fn ZSTD_resetSeqStore(ssPtr: &mut SeqStore_t) {
    ssPtr.lit = ssPtr.litStart;
    ssPtr.sequences = ssPtr.sequencesStart;
    ssPtr.longLengthType = LongLengthType::None;
}

/// Validates and post-processes sequences obtained through the external matchfinder API:
///   - Checks whether nbExternalSeqs represents an error condition.
///   - Appends a block delimiter to outSeqs if one is not already present.
///     See zstd.h for context regarding block delimiters.
///
/// # Returns
///
/// - The number of sequences after post-processing
/// - Or an error code
unsafe fn ZSTD_postProcessSequenceProducerResult(
    outSeqs: *mut ZSTD_Sequence,
    nbExternalSeqs: usize,
    outSeqsCapacity: usize,
    srcSize: usize,
) -> Result<usize, Error> {
    if nbExternalSeqs > outSeqsCapacity {
        return Err(Error::sequenceProducer_failed);
    }

    if nbExternalSeqs == 0 && srcSize > 0 {
        return Err(Error::sequenceProducer_failed);
    }

    if srcSize == 0 {
        ptr::write_bytes(
            &mut *outSeqs as *mut ZSTD_Sequence as *mut u8,
            0,
            size_of::<ZSTD_Sequence>(),
        );
        return Ok(1);
    }

    let lastSeq = *outSeqs.add(nbExternalSeqs.wrapping_sub(1));
    // We can return early if lastSeq is already a block delimiter.
    if lastSeq.offset == 0 && lastSeq.matchLength == 0 {
        return Ok(nbExternalSeqs);
    }

    // This error condition is only possible if the external matchfinder
    // produced an invalid parse, by definition of ZSTD_sequenceBound().
    if nbExternalSeqs == outSeqsCapacity {
        return Err(Error::sequenceProducer_failed);
    }

    // lastSeq is not a block delimiter, so we need to append one.
    ptr::write_bytes(
        &mut *outSeqs.add(nbExternalSeqs) as *mut ZSTD_Sequence as *mut u8,
        0,
        size_of::<ZSTD_Sequence>(),
    );
    Ok(nbExternalSeqs.wrapping_add(1))
}

/// Returns sum(litLen) + sum(matchLen) + lastLits for *seqBuf*.
/// Similar to another function in zstd_compress.c (determine_blockSize),
/// except it doesn't check for a block delimiter to end summation.
/// Removing the early exit allows the compiler to auto-vectorize.
/// This function can be deleted and replaced by determine_blockSize after we resolve issue #3456.
fn ZSTD_fastSequenceLengthSum(seqBuf: &[ZSTD_Sequence]) -> usize {
    let mut matchLenSum: usize = 0;
    let mut litLenSum: usize = 0;
    for seq in seqBuf {
        litLenSum = litLenSum.wrapping_add(seq.litLength as usize);
        matchLenSum = matchLenSum.wrapping_add(seq.matchLength as usize);
    }
    litLenSum.wrapping_add(matchLenSum)
}

/// Validate sequences produced by a block compressor.
unsafe fn ZSTD_validateSeqStore(seqStore: &SeqStore_t, cParams: &ZSTD_compressionParameters) {
    let matchLenLowerBound = match cParams.minMatch {
        3 => 3,
        _ => 4,
    };

    if cfg!(debug_assertions) {
        for seq in seqStore.seqs() {
            let seqLength = ZSTD_getSequenceLength(seqStore, seq);
            debug_assert!(seqLength.matchLength >= matchLenLowerBound);
        }
    }
}

unsafe fn ZSTD_buildSeqStore(
    zc: *mut ZSTD_CCtx,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> Result<BuildSeqStore, Error> {
    let ms: &mut ZSTD_MatchState_t = &mut (*zc).blockState.matchState;
    ZSTD_assertEqualCParams((*zc).appliedParams.cParams, ms.cParams);
    if srcSize
        < (MIN_CBLOCK_SIZE as usize)
            .wrapping_add(ZSTD_BLOCKHEADERSIZE)
            .wrapping_add(1)
            .wrapping_add(1)
    {
        if (*zc).appliedParams.cParams.strategy >= ZSTD_btopt {
            ZSTD_ldm_skipRawSeqStoreBytes(&mut (*zc).externSeqStore, srcSize);
        } else {
            let externSeqStore = &mut (*zc).externSeqStore;
            let seqs: &mut [rawSeq] = if externSeqStore.seq.is_null() {
                &mut []
            } else {
                core::slice::from_raw_parts_mut(externSeqStore.seq, externSeqStore.size)
            };
            ZSTD_ldm_skipSequences(
                seqs,
                &mut externSeqStore.pos,
                srcSize,
                (*zc).appliedParams.cParams.minMatch,
            );
        }
        // don't even attempt compression below a certain srcSize
        return Ok(BuildSeqStore::NoCompress);
    }
    ZSTD_resetSeqStore(&mut (*zc).seqStore);
    // required for optimal parser to read stats from dictionary
    ms.opt.symbolCosts = &mut (*(*zc).blockState.prevCBlock).entropy;
    // tell the optimal parser how we expect to compress literals
    ms.opt.literalCompressionMode = (*zc).appliedParams.literalCompressionMode;

    // limited update after a very long match
    let base = ms.window.base;
    let istart = src as *const u8;
    let curr = istart.wrapping_offset_from(base) as core::ffi::c_long as u32;

    if size_of::<ptrdiff_t>() == 8 {
        assert!(istart.wrapping_offset_from(base) < u32::MAX as ptrdiff_t); /* ensure no overflow */
    }

    if curr > ms.nextToUpdate.wrapping_add(384) {
        ms.nextToUpdate = curr.wrapping_sub(Ord::min(
            192,
            curr.wrapping_sub(ms.nextToUpdate).wrapping_sub(384),
        ));
    }

    // select and store sequences
    let dictMode = ms.dict_mode();
    for i in 0..ZSTD_REP_NUM {
        (*(*zc).blockState.nextCBlock).rep[i as usize] =
            (*(*zc).blockState.prevCBlock).rep[i as usize];
    }
    let lastLLSize: usize = if (*zc).externSeqStore.pos < (*zc).externSeqStore.size {
        if (*zc).appliedParams.has_ext_seq_prod() {
            return Err(Error::parameter_combination_unsupported);
        }

        ZSTD_ldm_blockCompress(
            &mut (*zc).externSeqStore,
            ms,
            &mut (*zc).seqStore,
            &mut (*(*zc).blockState.nextCBlock).rep,
            (*zc).appliedParams.useRowMatchFinder,
            src,
            srcSize,
        )
    } else if (*zc).appliedParams.ldmParams.enableLdm == ParamSwitch::Enable {
        let mut ldmSeqStore = RawSeqStore_t::default();
        if (*zc).appliedParams.has_ext_seq_prod() {
            return Err(Error::parameter_combination_unsupported);
        }
        ldmSeqStore.seq = (*zc).ldmSequences;
        ldmSeqStore.capacity = (*zc).maxNbLdmSequences;

        (*zc).ldmState.generate_sequences(
            &mut ldmSeqStore,
            &(*zc).appliedParams.ldmParams,
            core::slice::from_raw_parts(src.cast(), srcSize),
        )?;

        ZSTD_ldm_blockCompress(
            &mut ldmSeqStore,
            ms,
            &mut (*zc).seqStore,
            &mut (*(*zc).blockState.nextCBlock).rep,
            (*zc).appliedParams.useRowMatchFinder,
            src,
            srcSize,
        )
    } else if (*zc).appliedParams.has_ext_seq_prod() {
        let windowSize = 1 << (*zc).appliedParams.cParams.windowLog;

        let nbExternalSeqs = ((*zc).appliedParams.extSeqProdFunc).unwrap_unchecked()(
            (*zc).appliedParams.extSeqProdState,
            (*zc).extSeqBuf,
            (*zc).extSeqBufCapacity,
            src,
            srcSize,
            core::ptr::null(),
            0,
            (*zc).appliedParams.compressionLevel,
            windowSize as usize,
        );

        match ZSTD_postProcessSequenceProducerResult(
            (*zc).extSeqBuf,
            nbExternalSeqs,
            (*zc).extSeqBufCapacity,
            srcSize,
        ) {
            // Return early if there is no error, since we don't need to worry about last literals
            Ok(nbPostProcessedSeqs) => {
                let mut seqPos = ZSTD_SequencePosition {
                    idx: 0,
                    posInSequence: 0,
                    posInSrc: 0,
                };
                let extSeqs = core::slice::from_raw_parts((*zc).extSeqBuf, nbPostProcessedSeqs);
                let seqLenSum = ZSTD_fastSequenceLengthSum(extSeqs);
                if seqLenSum > srcSize {
                    return Err(Error::externalSequences_invalid);
                }
                ZSTD_transferSequences_wBlockDelim(
                    zc,
                    &mut seqPos,
                    extSeqs,
                    src,
                    srcSize,
                    (*zc).appliedParams.searchForExternalRepcodes,
                )?;
                ms.ldmSeqStore = core::ptr::null();
                return Ok(BuildSeqStore::Compress);
            }
            // Propagate the error if fallback is disabled
            Err(err) if (*zc).appliedParams.enableMatchFinderFallback == 0 => {
                return Err(err);
            }
            // Fallback to software matchfinder
            Err(_) => {
                let blockCompressor = ZSTD_selectBlockCompressor(
                    (*zc).appliedParams.cParams.strategy,
                    (*zc).appliedParams.useRowMatchFinder,
                    dictMode,
                );
                ms.ldmSeqStore = core::ptr::null();

                blockCompressor.unwrap_unchecked()(
                    ms,
                    &mut (*zc).seqStore,
                    &mut (*(*zc).blockState.nextCBlock).rep,
                    src,
                    srcSize,
                )
            }
        }
    } else {
        // not long range mode and no external matchfinder
        let blockCompressor_0 = ZSTD_selectBlockCompressor(
            (*zc).appliedParams.cParams.strategy,
            (*zc).appliedParams.useRowMatchFinder,
            dictMode,
        );
        ms.ldmSeqStore = core::ptr::null();

        blockCompressor_0.unwrap_unchecked()(
            ms,
            &mut (*zc).seqStore,
            &mut (*(*zc).blockState.nextCBlock).rep,
            src,
            srcSize,
        )
    };

    let lastLiterals = (src as *const u8).add(srcSize).sub(lastLLSize as usize);
    ZSTD_storeLastLiterals(&mut (*zc).seqStore, lastLiterals, lastLLSize);

    ZSTD_validateSeqStore(&(*zc).seqStore, &(*zc).appliedParams.cParams);
    Ok(BuildSeqStore::Compress)
}

unsafe fn ZSTD_copyBlockSequences(
    seqCollector: &mut SeqCollector,
    seqStore: &SeqStore_t,
    prevRepcodes: &RepCodes,
) -> Result<(), Error> {
    let inSeqs: *const SeqDef = seqStore.sequencesStart;
    let nbInSequences = seqStore.sequences.offset_from_unsigned(inSeqs);
    let nbInLiterals = seqStore.lit.offset_from_unsigned(seqStore.litStart);

    let outSeqs = if seqCollector.seqIndex == 0 {
        seqCollector.seqStart
    } else {
        seqCollector.seqStart.add(seqCollector.seqIndex)
    };
    let nbOutSequences = nbInSequences.wrapping_add(1);
    let mut nbOutLiterals = 0usize;

    if nbOutSequences > (seqCollector.maxSequences).wrapping_sub(seqCollector.seqIndex) {
        return Err(Error::dstSize_tooSmall);
    }

    let mut repcodes = *prevRepcodes;
    for i in 0..nbInSequences {
        (*outSeqs.add(i)).litLength = (*inSeqs.add(i)).litLength as core::ffi::c_uint;
        (*outSeqs.add(i)).matchLength = u32::from((*inSeqs.add(i)).mlBase) + u32::from(MINMATCH);
        (*outSeqs.add(i)).rep = 0;

        // Handle the possible single length >= 64K
        // There can only be one because we add MINMATCH to every match length,
        // and blocks are at most 128K.
        if i == seqStore.longLengthPos as usize {
            if seqStore.longLengthType == LongLengthType::Literal {
                let fresh4 = &mut (*outSeqs.add(i)).litLength;
                *fresh4 = (*fresh4).wrapping_add(0x10000);
            } else if seqStore.longLengthType == LongLengthType::Match {
                let fresh5 = &mut (*outSeqs.add(i)).matchLength;
                *fresh5 = (*fresh5).wrapping_add(0x10000);
            }
        }

        // Determine the raw offset given the offBase, which may be a repcode.
        let rawOffset = if 1 <= (*inSeqs.add(i)).offBase && (*inSeqs.add(i)).offBase <= ZSTD_REP_NUM
        {
            let repcode = (*inSeqs.add(i)).offBase;
            (*outSeqs.add(i)).rep = repcode;
            if (*outSeqs.add(i)).litLength != 0 {
                repcodes[repcode.wrapping_sub(1) as usize]
            } else if repcode == 3 {
                repcodes[0].wrapping_sub(1)
            } else {
                repcodes[repcode as usize]
            }
        } else {
            ((*inSeqs.add(i)).offBase).wrapping_sub(ZSTD_REP_NUM)
        };
        (*outSeqs.add(i)).offset = rawOffset;

        // Update repcode history for the sequence
        ZSTD_updateRep(
            &mut repcodes,
            (*inSeqs.add(i)).offBase,
            (*inSeqs.add(i)).litLength == 0,
        );
        nbOutLiterals = nbOutLiterals.wrapping_add((*outSeqs.add(i)).litLength as usize);
    }

    // Insert last literals (if any exist) in the block as a sequence with ml == off == 0.
    // If there are no last literals, then we'll emit (of: 0, ml: 0, ll: 0), which is a marker
    // for the block boundary, according to the API.
    let lastLLSize = nbInLiterals.wrapping_sub(nbOutLiterals);
    (*outSeqs.add(nbInSequences)).litLength = lastLLSize as u32;
    (*outSeqs.add(nbInSequences)).matchLength = 0;
    (*outSeqs.add(nbInSequences)).offset = 0;

    seqCollector.seqIndex = seqCollector.seqIndex.wrapping_add(nbOutSequences);

    Ok(())
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_sequenceBound))]
pub extern "C" fn ZSTD_sequenceBound(srcSize: usize) -> usize {
    let maxNbSeq = (srcSize / ZSTD_MINMATCH_MIN as usize).wrapping_add(1);
    let maxNbDelims = (srcSize / ZSTD_BLOCKSIZE_MAX_MIN as usize).wrapping_add(1);
    maxNbSeq.wrapping_add(maxNbDelims)
}

#[deprecated(
    since = "0.0.0+zstd.1.5.6",
    note = "For debugging only, will be replaced by `ZSTD_extractSequences`"
)]
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_generateSequences))]
pub unsafe extern "C" fn ZSTD_generateSequences(
    zc: *mut ZSTD_CCtx,
    outSeqs: *mut ZSTD_Sequence,
    outSeqsSize: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    let dstCapacity = ZSTD_compressBound(srcSize);

    let mut targetCBlockSize: core::ffi::c_int = 0;
    let err_code = ZSTD_CCtx_getParameter(
        zc,
        ZSTD_cParameter::ZSTD_c_targetCBlockSize,
        &mut targetCBlockSize,
    );
    if ERR_isError(err_code) {
        return err_code;
    }
    if targetCBlockSize != 0 {
        return Error::parameter_unsupported.to_error_code();
    }

    let mut nbWorkers: core::ffi::c_int = 0;
    let err_code_0 = ZSTD_CCtx_getParameter(zc, ZSTD_cParameter::ZSTD_c_nbWorkers, &mut nbWorkers);
    if ERR_isError(err_code_0) {
        return err_code_0;
    }
    if nbWorkers != 0 {
        return Error::parameter_unsupported.to_error_code();
    }

    let dst = ZSTD_customMalloc(dstCapacity, ZSTD_customMem::default());
    if dst.is_null() {
        return Error::memory_allocation.to_error_code();
    }

    (*zc).seqCollector = SeqCollector {
        collectSequences: 1,
        seqStart: outSeqs,
        seqIndex: 0,
        maxSequences: outSeqsSize,
    };

    let ret = ZSTD_compress2(zc, dst, dstCapacity, src, srcSize);
    ZSTD_customFree(dst, dstCapacity, ZSTD_customMem::default());
    let err_code_1 = ret;
    if ERR_isError(err_code_1) {
        return err_code_1;
    }

    (*zc).seqCollector.seqIndex
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_mergeBlockDelimiters))]
pub unsafe extern "C" fn ZSTD_mergeBlockDelimiters(
    sequences: *mut ZSTD_Sequence,
    seqsSize: usize,
) -> usize {
    let sequences: &mut [ZSTD_Sequence] = if sequences.is_null() {
        &mut []
    } else {
        core::slice::from_raw_parts_mut(sequences, seqsSize)
    };
    ZSTD_mergeBlockDelimiters_slice(sequences)
}

fn ZSTD_mergeBlockDelimiters_slice(sequences: &mut [ZSTD_Sequence]) -> usize {
    let mut out = 0usize;
    for in_0 in 0..sequences.len() {
        let seq = sequences[in_0];
        if seq.offset == 0 && seq.matchLength == 0 {
            if let Some(next) = sequences.get_mut(in_0 + 1) {
                next.litLength = next.litLength.wrapping_add(seq.litLength);
            }
        } else {
            sequences[out] = seq;
            out += 1;
        }
    }
    out
}

/// Unrolled loop to read four usizes of input at a time. Returns `true` if the input is RLE.
fn ZSTD_isRLE(src: &[u8]) -> bool {
    let Some(&value) = src.first() else {
        return true;
    };
    let valueST = usize::from_ne_bytes([value; size_of::<usize>()]);
    let (prefix, rest) = src.split_at(src.len() % (4 * size_of::<usize>()));

    // Check if prefix is RLE first before using unrolled loop
    if !prefix.iter().all(|&b| b == value) {
        return false;
    }

    let (chunks, []) = rest.as_chunks::<{ 4 * size_of::<usize>() }>() else {
        unreachable!()
    };
    chunks.iter().all(|chunk| {
        let (words, []) = chunk.as_chunks::<{ size_of::<usize>() }>() else {
            unreachable!()
        };
        words
            .iter()
            .all(|&word| usize::from_ne_bytes(word) == valueST)
    })
}

/// Returns true if the given block may be RLE.
/// This is just a heuristic based on the compressibility.
/// It may return both false positives and false negatives.
unsafe fn ZSTD_maybeRLE(seqStore: &SeqStore_t) -> bool {
    let nbSeqs = seqStore
        .sequences
        .offset_from_unsigned(seqStore.sequencesStart);
    let nbLits = seqStore.lit.offset_from_unsigned(seqStore.litStart);

    nbSeqs < 4 && nbLits < 10
}

fn ZSTD_blockState_confirmRepcodesAndEntropyTables(bs: &mut ZSTD_blockState_t) {
    core::mem::swap(&mut bs.prevCBlock, &mut bs.nextCBlock);
}

/// Writes the block header
unsafe fn writeBlockHeader(
    op: *mut core::ffi::c_void,
    cSize: usize,
    blockSize: usize,
    lastBlock: bool,
) {
    let cBlockHeader = if cSize == 1 {
        u32::from(lastBlock)
            .wrapping_add((BlockType::Rle as u32) << 1)
            .wrapping_add((blockSize << 3) as u32)
    } else {
        u32::from(lastBlock)
            .wrapping_add((BlockType::Compressed as u32) << 1)
            .wrapping_add((cSize << 3) as u32)
    };
    MEM_writeLE24(op, cBlockHeader);
}

/// Builds entropy for the literals.
/// Stores literals block type (raw, rle, compressed, repeat) and
/// huffman description table to hufMetadata.
/// Requires ENTROPY_WORKSPACE_SIZE workspace
///
/// # Returns
///
/// - The size of huffman description table
/// - Or an error code
unsafe fn ZSTD_buildBlockEntropyStats_literals(
    src: &[u8],
    prevHuf: &ZSTD_hufCTables_t,
    nextHuf: &mut ZSTD_hufCTables_t,
    hufMetadata: &mut ZSTD_hufCTablesMetadata_t,
    literalsCompressionIsDisabled: bool,
    workspace: *mut core::ffi::c_void,
    wkspSize: usize,
    hufFlags: core::ffi::c_int,
) -> Result<usize, Error> {
    let wkspStart = workspace as *mut u8;
    let wkspEnd = wkspStart.add(wkspSize);
    let countWkspStart = wkspStart;
    let countWksp = workspace as *mut core::ffi::c_uint;
    let countWkspSize =
        ((HUF_SYMBOLVALUE_MAX + 1) as usize).wrapping_mul(size_of::<core::ffi::c_uint>());
    let nodeWksp = countWkspStart.add(countWkspSize);
    let nodeWkspSize = wkspEnd.offset_from_unsigned(nodeWksp);
    let mut maxSymbolValue = HUF_SYMBOLVALUE_MAX_U8;
    let mut huffLog = LitHufLog;
    let mut repeat = prevHuf.repeatMode;

    // Prepare nextEntropy assuming reusing the existing table
    core::ptr::copy_nonoverlapping(prevHuf, nextHuf, 1);

    if literalsCompressionIsDisabled {
        hufMetadata.hType = SymbolEncodingType::Basic;
        return Ok(0);
    }

    // small ? don't even attempt compression (speed opt)
    let minLitSize = (if prevHuf.repeatMode == HUF_repeat::Valid {
        6
    } else {
        COMPRESS_LITERALS_SIZE_MIN
    }) as usize;
    if src.len() <= minLitSize {
        hufMetadata.hType = SymbolEncodingType::Basic;
        return Ok(0);
    }

    // Scan input and build symbol stats
    let largest =
        HIST_count_wksp(countWksp, &mut maxSymbolValue, src, workspace, wkspSize)? as usize;
    if largest == src.len() {
        // only one literal symbol
        hufMetadata.hType = SymbolEncodingType::Rle;
        return Ok(0);
    }
    if largest <= (src.len() >> 7).wrapping_add(4) {
        // heuristic: likely not compressible
        hufMetadata.hType = SymbolEncodingType::Basic;
        return Ok(0);
    }

    // `HIST_count_wksp` has filled every entry of the count table.
    let count = core::slice::from_raw_parts(countWksp, HUF_SYMBOLVALUE_MAX as usize + 1);

    // Validate the previous Huffman table
    if repeat == HUF_repeat::Check && !HUF_validateCTable(&prevHuf.CTable, count, maxSymbolValue) {
        repeat = HUF_repeat::None;
    }

    // Build Huffman Tree
    nextHuf.CTable = CTable::default();
    huffLog = HUF_optimalTableLog(
        huffLog,
        src.len(),
        maxSymbolValue,
        nodeWksp as *mut core::ffi::c_void,
        nodeWkspSize,
        &mut nextHuf.CTable,
        count,
        hufFlags,
    );
    let maxBits = HUF_buildCTable_wksp(
        &mut nextHuf.CTable,
        count,
        maxSymbolValue,
        huffLog,
        nodeWksp as *mut core::ffi::c_void,
        nodeWkspSize,
    )?;
    huffLog = maxBits as u32;
    // Build and write the CTable
    let newCSize = HUF_estimateCompressedSize(&nextHuf.CTable, count, maxSymbolValue);
    let hSize = HUF_writeCTable_wksp(
        hufMetadata.hufDesBuffer.as_mut_ptr() as *mut core::ffi::c_void,
        size_of::<[u8; ZSTD_MAX_HUF_HEADER_SIZE]>(),
        &nextHuf.CTable,
        maxSymbolValue,
        huffLog,
        nodeWksp as *mut core::ffi::c_void,
        nodeWkspSize,
    )?;
    // Check against repeating the previous CTable
    if repeat != HUF_repeat::None {
        let oldCSize = HUF_estimateCompressedSize(&prevHuf.CTable, count, maxSymbolValue);
        if oldCSize < src.len()
            && (oldCSize <= hSize.wrapping_add(newCSize) || hSize.wrapping_add(12) >= src.len())
        {
            core::ptr::copy_nonoverlapping(prevHuf, nextHuf, 1);
            hufMetadata.hType = SymbolEncodingType::Repeat;
            return Ok(0);
        }
    }
    if newCSize.wrapping_add(hSize) >= src.len() {
        core::ptr::copy_nonoverlapping(prevHuf, nextHuf, 1);
        hufMetadata.hType = SymbolEncodingType::Basic;
        return Ok(0);
    }
    hufMetadata.hType = SymbolEncodingType::Compressed;
    nextHuf.repeatMode = HUF_repeat::Check;

    Ok(hSize)
}

pub const COMPRESS_LITERALS_SIZE_MIN: core::ffi::c_int = 63;

/// Returns a [`ZSTD_symbolEncodingTypeStats_t`] with all encoding types as [`SymbolEncodingType::Basic`],
/// and updates nextEntropy to the appropriate repeatMode.
fn ZSTD_buildDummySequencesStatistics(
    nextEntropy: &mut ZSTD_fseCTables_t,
) -> ZSTD_symbolEncodingTypeStats_t {
    nextEntropy.litlength_repeatMode = FSE_repeat::None;
    nextEntropy.offcode_repeatMode = FSE_repeat::None;
    nextEntropy.matchlength_repeatMode = FSE_repeat::None;

    ZSTD_symbolEncodingTypeStats_t::default()
}

/// Builds entropy for the sequences.
/// Stores symbol compression modes and fse table to fseMetadata.
/// Requires ENTROPY_WORKSPACE_SIZE wksp.
///
/// # Returns
/// - The size of the fse tables
/// - Or an error code
unsafe fn ZSTD_buildBlockEntropyStats_sequences(
    seqStorePtr: &SeqStore_t,
    prevEntropy: &ZSTD_fseCTables_t,
    nextEntropy: &mut ZSTD_fseCTables_t,
    cctxParams: &ZSTD_CCtx_params,
    fseMetadata: &mut ZSTD_fseCTablesMetadata_t,
    workspace: *mut core::ffi::c_void,
    wkspSize: usize,
) -> Result<usize, Error> {
    let strategy = cctxParams.cParams.strategy;
    let nbSeq = (seqStorePtr.sequences).offset_from_unsigned(seqStorePtr.sequencesStart);
    let ostart = fseMetadata.fseTablesBuffer.as_mut_ptr();
    let oend = ostart.add(size_of::<[u8; 133]>());
    let op = ostart;
    let countWorkspace = workspace as *mut core::ffi::c_uint;
    let entropyWorkspace = countWorkspace.add(MaxSeq + 1);
    let entropyWorkspaceSize =
        wkspSize.wrapping_sub((MaxSeq + 1).wrapping_mul(size_of::<core::ffi::c_uint>()));

    let stats = if nbSeq != 0 {
        ZSTD_buildSequencesStatistics(
            seqStorePtr,
            nbSeq,
            prevEntropy,
            nextEntropy,
            op,
            oend,
            strategy,
            countWorkspace,
            entropyWorkspace as *mut core::ffi::c_void,
            entropyWorkspaceSize,
        )?
    } else {
        ZSTD_buildDummySequencesStatistics(nextEntropy)
    };

    fseMetadata.llType = stats.LLtype;
    fseMetadata.ofType = stats.Offtype;
    fseMetadata.mlType = stats.MLtype;
    fseMetadata.lastCountSize = stats.lastCountSize;

    Ok(stats.size)
}

/// Builds entropy for the block.
/// Requires workspace size ENTROPY_WORKSPACE_SIZE
///
/// Note: also employed in superblock
pub unsafe fn ZSTD_buildBlockEntropyStats(
    seqStorePtr: &SeqStore_t,
    prevEntropy: &ZSTD_entropyCTables_t,
    nextEntropy: &mut ZSTD_entropyCTables_t,
    cctxParams: &ZSTD_CCtx_params,
    entropyMetadata: *mut ZSTD_entropyCTablesMetadata_t,
    workspace: *mut core::ffi::c_void,
    wkspSize: usize,
) -> Result<(), Error> {
    let litSize = seqStorePtr.lit.offset_from_unsigned(seqStorePtr.litStart);
    let huf_useOptDepth =
        cctxParams.cParams.strategy >= HUF_OPTIMAL_DEPTH_THRESHOLD as core::ffi::c_uint;
    let hufFlags = if huf_useOptDepth {
        HUF_flags_optimalDepth as core::ffi::c_int
    } else {
        0
    };

    (*entropyMetadata).hufMetadata.hufDesSize = ZSTD_buildBlockEntropyStats_literals(
        core::slice::from_raw_parts(seqStorePtr.litStart, litSize),
        &prevEntropy.huf,
        &mut nextEntropy.huf,
        &mut (*entropyMetadata).hufMetadata,
        cctxParams.literals_compression_is_disabled(),
        workspace,
        wkspSize,
        hufFlags,
    )?;

    (*entropyMetadata).fseMetadata.fseTablesSize = ZSTD_buildBlockEntropyStats_sequences(
        seqStorePtr,
        &prevEntropy.fse,
        &mut nextEntropy.fse,
        cctxParams,
        &mut (*entropyMetadata).fseMetadata,
        workspace,
        wkspSize,
    )?;

    Ok(())
}

/// Returns the size estimate for the literals section (header + content) of a block
unsafe fn ZSTD_estimateBlockSize_literal(
    literals: &[u8],
    huf: &ZSTD_hufCTables_t,
    hufMetadata: &ZSTD_hufCTablesMetadata_t,
    workspace: *mut core::ffi::c_void,
    wkspSize: usize,
    writeEntropy: bool,
) -> usize {
    let countWksp = workspace as *mut core::ffi::c_uint;
    let mut maxSymbolValue = HUF_SYMBOLVALUE_MAX_U8;
    let literalSectionHeaderSize = 3
        + usize::from(literals.len() >= (1 << 10))
        + usize::from(literals.len() >= (16 * (1 << 10)));
    let singleStream = literals.len() < 256;

    match hufMetadata.hType {
        SymbolEncodingType::Basic => literals.len(),
        SymbolEncodingType::Rle => 1,
        SymbolEncodingType::Compressed | SymbolEncodingType::Repeat => {
            if HIST_count_wksp(
                countWksp,
                &mut maxSymbolValue,
                literals,
                workspace,
                wkspSize,
            )
            .is_err()
            {
                return literals.len();
            };
            // `HIST_count_wksp` has filled every entry of the count table.
            let count = core::slice::from_raw_parts(countWksp, HUF_SYMBOLVALUE_MAX as usize + 1);
            let mut cLitSizeEstimate =
                HUF_estimateCompressedSize(&huf.CTable, count, maxSymbolValue);
            if writeEntropy {
                cLitSizeEstimate = cLitSizeEstimate.wrapping_add(hufMetadata.hufDesSize);
            }
            if !singleStream {
                // multi-stream huffman uses 6-byte jump table
                cLitSizeEstimate = cLitSizeEstimate.wrapping_add(6);
            }
            cLitSizeEstimate.wrapping_add(literalSectionHeaderSize)
        }
    }
}

/// Returns the size estimate for the FSE-compressed symbols (of, ml, ll) of a block
unsafe fn ZSTD_estimateBlockSize_symbolType(
    encodingType: SymbolEncodingType,
    codeTable: &[u8],
    maxCode: u8,
    fseCTable: &[FSE_CTable],
    additionalBits: &[u8],
    defaultNorm: &[core::ffi::c_short],
    defaultNormLog: u32,
    defaultMax: u8,
    workspace: *mut core::ffi::c_void,
    wkspSize: usize,
) -> usize {
    let countWksp = workspace as *mut core::ffi::c_uint;
    let mut max = maxCode;

    HIST_countFast_wksp(countWksp, &mut max, codeTable, workspace, wkspSize).expect("can't fail");
    // `HIST_countFast_wksp` has filled every entry of the count table up to `max`.
    let count = core::slice::from_raw_parts(countWksp, usize::from(max) + 1);
    let mut cSymbolTypeSizeEstimateInBits = match encodingType {
        SymbolEncodingType::Basic => {
            // We selected this encoding type, so it must be valid.
            assert!(max <= defaultMax);

            ZSTD_crossEntropyCost(defaultNorm, defaultNormLog, count, max)
        }
        SymbolEncodingType::Rle => 0,
        SymbolEncodingType::Compressed | SymbolEncodingType::Repeat => {
            match ZSTD_fseBitCost(fseCTable, count, max) {
                Ok(cost) => cost,
                Err(_) => return codeTable.len() * 10,
            }
        }
    };

    for &code in codeTable {
        cSymbolTypeSizeEstimateInBits += if additionalBits.is_empty() {
            usize::from(code)
        } else {
            usize::from(additionalBits[usize::from(code)])
        };
    }

    cSymbolTypeSizeEstimateInBits >> 3
}

/// Returns the size estimate for the sequences section (header + content) of a block
unsafe fn ZSTD_estimateBlockSize_sequences(
    ofCodeTable: &[u8],
    llCodeTable: &[u8],
    mlCodeTable: &[u8],
    fseTables: &ZSTD_fseCTables_t,
    fseMetadata: &ZSTD_fseCTablesMetadata_t,
    workspace: *mut core::ffi::c_void,
    wkspSize: usize,
    writeEntropy: bool,
) -> usize {
    let nbSeq = ofCodeTable.len();
    let sequencesSectionHeaderSize =
        1 + 1 + usize::from(nbSeq >= 128) + usize::from(nbSeq >= LONGNBSEQ as usize);
    let mut cSeqSizeEstimate = 0usize;

    cSeqSizeEstimate = cSeqSizeEstimate.wrapping_add(ZSTD_estimateBlockSize_symbolType(
        fseMetadata.ofType,
        ofCodeTable,
        MaxOff,
        &fseTables.offcodeCTable,
        &[],
        &OF_defaultNorm,
        OF_defaultNormLog,
        DefaultMaxOff,
        workspace,
        wkspSize,
    ));
    cSeqSizeEstimate = cSeqSizeEstimate.wrapping_add(ZSTD_estimateBlockSize_symbolType(
        fseMetadata.llType,
        llCodeTable,
        MaxLL,
        &fseTables.litlengthCTable,
        &LL_bits,
        &LL_defaultNorm,
        LL_defaultNormLog,
        MaxLL,
        workspace,
        wkspSize,
    ));
    cSeqSizeEstimate = cSeqSizeEstimate.wrapping_add(ZSTD_estimateBlockSize_symbolType(
        fseMetadata.mlType,
        mlCodeTable,
        MaxML,
        &fseTables.matchlengthCTable,
        &ML_bits,
        &ML_defaultNorm,
        ML_defaultNormLog,
        MaxML,
        workspace,
        wkspSize,
    ));

    if writeEntropy {
        cSeqSizeEstimate = cSeqSizeEstimate.wrapping_add(fseMetadata.fseTablesSize);
    }

    cSeqSizeEstimate.wrapping_add(sequencesSectionHeaderSize)
}

/// Returns the size estimate for a given stream of literals, of, ll, ml
unsafe fn ZSTD_estimateBlockSize(
    literals: &[u8],
    ofCodeTable: &[u8],
    llCodeTable: &[u8],
    mlCodeTable: &[u8],
    entropy: &ZSTD_entropyCTables_t,
    entropyMetadata: *const ZSTD_entropyCTablesMetadata_t,
    workspace: *mut core::ffi::c_void,
    wkspSize: usize,
    writeLitEntropy: bool,
    writeSeqEntropy: bool,
) -> usize {
    let literalsSize = ZSTD_estimateBlockSize_literal(
        literals,
        &entropy.huf,
        &(*entropyMetadata).hufMetadata,
        workspace,
        wkspSize,
        writeLitEntropy,
    );
    let seqSize = ZSTD_estimateBlockSize_sequences(
        ofCodeTable,
        llCodeTable,
        mlCodeTable,
        &entropy.fse,
        &(*entropyMetadata).fseMetadata,
        workspace,
        wkspSize,
        writeSeqEntropy,
    );
    seqSize
        .wrapping_add(literalsSize)
        .wrapping_add(ZSTD_BLOCKHEADERSIZE)
}

/// Builds entropy statistics and uses them for blocksize estimation.
///
/// # Returns
///
/// The estimated compressed size of the seqStore, or a zstd error.
unsafe fn ZSTD_buildEntropyStatisticsAndEstimateSubBlockSize(
    seqStore: &mut SeqStore_t,
    zc: *mut ZSTD_CCtx,
) -> Result<usize, Error> {
    let entropyMetadata: *mut ZSTD_entropyCTablesMetadata_t =
        &mut (*zc).blockSplitCtx.entropyMetadata;
    ZSTD_buildBlockEntropyStats(
        seqStore,
        &(*(*zc).blockState.prevCBlock).entropy,
        &mut (*(*zc).blockState.nextCBlock).entropy,
        &(*zc).appliedParams,
        entropyMetadata,
        (*zc).tmpWorkspace,
        (*zc).tmpWkspSize,
    )?;

    Ok(ZSTD_estimateBlockSize(
        core::slice::from_raw_parts(
            seqStore.litStart,
            seqStore.lit.offset_from_unsigned(seqStore.litStart),
        ),
        seqStore.ofCodes(),
        seqStore.llCodes(),
        seqStore.mlCodes(),
        &(*(*zc).blockState.nextCBlock).entropy,
        entropyMetadata,
        (*zc).tmpWorkspace,
        (*zc).tmpWkspSize,
        (*entropyMetadata).hufMetadata.hType == SymbolEncodingType::Compressed,
        true,
    ))
}

/// Returns literals bytes represented in a seqStore
unsafe fn ZSTD_countSeqStoreLiteralsBytes(seqStore: &SeqStore_t) -> usize {
    let mut literalsBytes = 0usize;
    for (i, seq) in seqStore.seqs().iter().enumerate() {
        literalsBytes = literalsBytes.wrapping_add(seq.litLength as usize);
        if i == seqStore.longLengthPos as usize
            && seqStore.longLengthType == LongLengthType::Literal
        {
            literalsBytes = literalsBytes.wrapping_add(0x10000);
        }
    }
    literalsBytes
}

/// Returns match bytes represented in a seqStore
unsafe fn ZSTD_countSeqStoreMatchBytes(seqStore: &SeqStore_t) -> usize {
    let mut matchBytes = 0usize;
    for (i, seq) in seqStore.seqs().iter().enumerate() {
        matchBytes = matchBytes.wrapping_add(usize::from(seq.mlBase) + usize::from(MINMATCH));
        if i == seqStore.longLengthPos as usize && seqStore.longLengthType == LongLengthType::Match
        {
            matchBytes = matchBytes.wrapping_add(0x10000);
        }
    }
    matchBytes
}

/// Derives the seqStore that is a chunk of the originalSeqStore from [startIdx, endIdx).
/// Stores the result in resultSeqStore.
unsafe fn ZSTD_deriveSeqStoreChunk(
    resultSeqStore: &mut SeqStore_t,
    originalSeqStore: &SeqStore_t,
    startIdx: usize,
    endIdx: usize,
) {
    *resultSeqStore = *originalSeqStore;
    if startIdx > 0 {
        resultSeqStore.sequences = originalSeqStore.sequencesStart.add(startIdx);
        resultSeqStore.litStart =
            (resultSeqStore.litStart).add(ZSTD_countSeqStoreLiteralsBytes(resultSeqStore));
    }

    // Move longLengthPos into the correct position if necessary
    if originalSeqStore.longLengthType != LongLengthType::None {
        if (originalSeqStore.longLengthPos as usize) < startIdx
            || originalSeqStore.longLengthPos as usize > endIdx
        {
            resultSeqStore.longLengthType = LongLengthType::None;
        } else {
            resultSeqStore.longLengthPos =
                resultSeqStore.longLengthPos.wrapping_sub(startIdx as u32);
        }
    }
    resultSeqStore.sequencesStart = originalSeqStore.sequencesStart.add(startIdx);
    resultSeqStore.sequences = originalSeqStore.sequencesStart.add(endIdx);
    if endIdx
        != originalSeqStore
            .sequences
            .offset_from_unsigned(originalSeqStore.sequencesStart)
    {
        let literalsBytes = ZSTD_countSeqStoreLiteralsBytes(resultSeqStore);
        resultSeqStore.lit = resultSeqStore.litStart.add(literalsBytes);
    }
    resultSeqStore.llCode = resultSeqStore.llCode.add(startIdx);
    resultSeqStore.mlCode = resultSeqStore.mlCode.add(startIdx);
    resultSeqStore.ofCode = resultSeqStore.ofCode.add(startIdx);
}

/// Returns the raw offset represented by the combination of offBase, ll0, and repcode history.
/// offBase must represent a repcode in the numeric representation of ZSTD_storeSeq().
fn ZSTD_resolveRepcodeToRawOffset(rep: &RepCodes, offBase: u32, ll0: bool) -> u32 {
    let adjustedRepCode = offBase.wrapping_sub(1).wrapping_add(ll0 as u32);
    if adjustedRepCode == ZSTD_REP_NUM {
        return rep[0].wrapping_sub(1);
    }
    rep[adjustedRepCode as usize]
}

/// ZSTD_seqStore_resolveOffCodes() reconciles any possible divergences in offset history that
/// may arise due to emission of RLE/raw blocks that disturb the offset history, and replaces any
/// repcodes within the seqStore that may be invalid.
///
/// dRepcodes are updated as would be on the decompression side.
/// cRepcodes are updated exactly in accordance with the seqStore.
///
/// Note: this function assumes seq->offBase respects the following numbering scheme:
/// 0: invalid, 1-3: repcode 1-3, 4+: real_offset+3
unsafe fn ZSTD_seqStore_resolveOffCodes(
    dRepcodes: &mut RepCodes,
    cRepcodes: &mut RepCodes,
    seqStore: &SeqStore_t,
    nbSeq: usize,
) {
    let longLitLenIdx = match seqStore.longLengthType {
        LongLengthType::Literal => seqStore.longLengthPos as usize,
        LongLengthType::None | LongLengthType::Match => nbSeq,
    };

    for idx in 0..nbSeq {
        let seq = seqStore.sequencesStart.add(idx);
        let ll0 = (*seq).litLength == 0 && idx != longLitLenIdx;
        let offBase = (*seq).offBase;
        if (1..=ZSTD_REP_NUM).contains(&offBase) {
            let dRawOffset = ZSTD_resolveRepcodeToRawOffset(dRepcodes, offBase, ll0);
            let cRawOffset = ZSTD_resolveRepcodeToRawOffset(cRepcodes, offBase, ll0);
            // Adjust simulated decompression repcode history if we come across a mismatch. Replace
            // the repcode with the offset it actually references, determined by the compression
            // repcode history.
            if dRawOffset != cRawOffset {
                (*seq).offBase = cRawOffset.wrapping_add(ZSTD_REP_NUM);
            }
        }
        // Compression repcode history is always updated with values directly from the unmodified seqStore.
        // Decompression repcode history may use modified seq->offset value taken from compression repcode history.
        ZSTD_updateRep(dRepcodes, (*seq).offBase, ll0);
        ZSTD_updateRep(cRepcodes, offBase, ll0);
    }
}

/// ZSTD_compressSeqStore_singleBlock(): Compresses a seqStore into a block with a block header,
/// into the buffer dst.
///
/// # Returns
///
/// The total size of that block (including header) or a ZSTD error code.
unsafe fn ZSTD_compressSeqStore_singleBlock(
    zc: *mut ZSTD_CCtx,
    seqStore: &SeqStore_t,
    dRep: &mut RepCodes,
    cRep: &mut RepCodes,
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
    lastBlock: bool,
    isPartition: bool,
) -> Result<usize, Error> {
    let rleMaxLength = 25;
    let op = dst as *mut u8;
    let ip = src as *const u8;

    // In case of an RLE or raw block, the simulated decompression repcode history must be reset
    let dRepOriginal = *dRep;
    if isPartition {
        ZSTD_seqStore_resolveOffCodes(
            dRep,
            cRep,
            seqStore,
            seqStore
                .sequences
                .offset_from_unsigned(seqStore.sequencesStart),
        );
    }

    if dstCapacity < ZSTD_BLOCKHEADERSIZE {
        return Err(Error::dstSize_tooSmall);
    }
    let mut cSeqsSize = ZSTD_entropyCompressSeqStore(
        seqStore,
        &(*(*zc).blockState.prevCBlock).entropy,
        &mut (*(*zc).blockState.nextCBlock).entropy,
        &(*zc).appliedParams,
        op.add(ZSTD_BLOCKHEADERSIZE) as *mut core::ffi::c_void,
        dstCapacity.wrapping_sub(ZSTD_BLOCKHEADERSIZE),
        srcSize,
        (*zc).tmpWorkspace,
        (*zc).tmpWkspSize,
        (*zc).bmi2 != 0,
    )?;

    if (*zc).isFirstBlock == 0
        && cSeqsSize < rleMaxLength as usize
        && ZSTD_isRLE(core::slice::from_raw_parts(src.cast::<u8>(), srcSize))
    {
        // We don't want to emit our first block as a RLE even if it qualifies because
        // doing so will cause the decoder (cli only) to throw a "should consume all input error."
        // This is only an issue for zstd <= v1.4.3
        cSeqsSize = 1;
    }

    // Sequence collection not supported when block splitting */
    if (*zc).seqCollector.collectSequences != 0 {
        ZSTD_copyBlockSequences(&mut (*zc).seqCollector, seqStore, &dRepOriginal)?;
        ZSTD_blockState_confirmRepcodesAndEntropyTables(&mut (*zc).blockState);
        return Ok(0);
    }

    let cSize: usize;
    if cSeqsSize == 0 {
        cSize = ZSTD_noCompressBlock(
            op as *mut core::ffi::c_void,
            dstCapacity,
            ip as *const core::ffi::c_void,
            srcSize,
            lastBlock,
        )?;
        *dRep = dRepOriginal; // reset simulated decompression repcode history
    } else if cSeqsSize == 1 {
        cSize = ZSTD_rleCompressBlock(
            op as *mut core::ffi::c_void,
            dstCapacity,
            *ip,
            srcSize,
            lastBlock,
        )?;
        *dRep = dRepOriginal; // reset simulated decompression repcode history
    } else {
        ZSTD_blockState_confirmRepcodesAndEntropyTables(&mut (*zc).blockState);
        writeBlockHeader(op as *mut core::ffi::c_void, cSeqsSize, srcSize, lastBlock);
        cSize = ZSTD_BLOCKHEADERSIZE.wrapping_add(cSeqsSize);
    }

    (*(*zc).blockState.prevCBlock)
        .entropy
        .fse
        .offcode_repeatMode
        .require_check();

    Ok(cSize)
}

pub const MIN_SEQUENCES_BLOCK_SPLITTING: usize = 300;

/// Helper function to perform the recursive search for block splits.
/// Estimates the cost of seqStore prior to split, and estimates the cost of splitting the
/// sequences in half. If advantageous to split, then we recurse down the two sub-blocks.
/// If not, or if an error occurred in estimation, then we do not recurse.
///
/// Note: The recursion depth is capped by a heuristic minimum number of sequences, defined by
/// [`MIN_SEQUENCES_BLOCK_SPLITTING`]. In theory, this means the absolute largest recursion depth is
/// 10 == log2(maxNbSeqInBlock/MIN_SEQUENCES_BLOCK_SPLITTING). In practice, recursion depth
/// usually doesn't go beyond 4.
///
/// Furthermore, the number of splits is capped by [`ZSTD_MAX_NB_BLOCK_SPLITS`].
unsafe fn ZSTD_deriveBlockSplitsHelper(
    splits: &mut seqStoreSplits,
    startIdx: usize,
    endIdx: usize,
    zc: *mut ZSTD_CCtx,
    origSeqStore: &SeqStore_t,
) {
    let fullSeqStoreChunk: &mut SeqStore_t = &mut (*zc).blockSplitCtx.fullSeqStoreChunk;
    let firstHalfSeqStore: &mut SeqStore_t = &mut (*zc).blockSplitCtx.firstHalfSeqStore;
    let secondHalfSeqStore: &mut SeqStore_t = &mut (*zc).blockSplitCtx.secondHalfSeqStore;
    let midIdx = startIdx.wrapping_add(endIdx) / 2;

    if endIdx.wrapping_sub(startIdx) < MIN_SEQUENCES_BLOCK_SPLITTING
        || splits.idx >= ZSTD_MAX_NB_BLOCK_SPLITS
    {
        return;
    }
    ZSTD_deriveSeqStoreChunk(fullSeqStoreChunk, origSeqStore, startIdx, endIdx);
    ZSTD_deriveSeqStoreChunk(firstHalfSeqStore, origSeqStore, startIdx, midIdx);
    ZSTD_deriveSeqStoreChunk(secondHalfSeqStore, origSeqStore, midIdx, endIdx);
    let (Ok(estimatedOriginalSize), Ok(estimatedFirstHalfSize), Ok(estimatedSecondHalfSize)) = (
        ZSTD_buildEntropyStatisticsAndEstimateSubBlockSize(fullSeqStoreChunk, zc),
        ZSTD_buildEntropyStatisticsAndEstimateSubBlockSize(firstHalfSeqStore, zc),
        ZSTD_buildEntropyStatisticsAndEstimateSubBlockSize(secondHalfSeqStore, zc),
    ) else {
        return;
    };
    if estimatedFirstHalfSize.wrapping_add(estimatedSecondHalfSize) < estimatedOriginalSize {
        ZSTD_deriveBlockSplitsHelper(splits, startIdx, midIdx, zc, origSeqStore);
        *splits.splitLocations.add(splits.idx) = midIdx as u32;
        splits.idx = splits.idx.wrapping_add(1);
        ZSTD_deriveBlockSplitsHelper(splits, midIdx, endIdx, zc, origSeqStore);
    }
}

/// Base recursive function. Populates a table with intra-block partition indices that can improve
/// compression ratio.
///
/// # Returns
///
/// The number of splits made (which equals the size of the partition table - 1).
unsafe fn ZSTD_deriveBlockSplits(zc: *mut ZSTD_CCtx, partitions: *mut u32, nbSeq: usize) -> usize {
    let mut splits = seqStoreSplits {
        splitLocations: partitions,
        idx: 0,
    };
    if nbSeq <= 4 {
        // Refuse to try and split anything with less than 4 sequences
        return 0;
    }
    ZSTD_deriveBlockSplitsHelper(&mut splits, 0, nbSeq, zc, &(*zc).seqStore);
    *splits.splitLocations.add(splits.idx) = nbSeq as u32;
    splits.idx
}

/// ZSTD_compressBlock_splitBlock(): Attempts to split a given block into multiple blocks to
/// improve compression ratio.
///
/// # Returns
///
/// THe combined size of all blocks (which includes headers), or a ZSTD error code.
unsafe fn ZSTD_compressBlock_splitBlock_internal(
    zc: *mut ZSTD_CCtx,
    dst: *mut core::ffi::c_void,
    mut dstCapacity: usize,
    src: *const core::ffi::c_void,
    blockSize: usize,
    lastBlock: bool,
    nbSeq: usize,
) -> Result<usize, Error> {
    let mut cSize = 0usize;
    let mut ip = src as *const u8;
    let mut op = dst as *mut u8;
    let mut srcBytesTotal = 0usize;
    let partitions = ((*zc).blockSplitCtx.partitions).as_mut_ptr();
    let nextSeqStore: &mut SeqStore_t = &mut (*zc).blockSplitCtx.nextSeqStore;
    let currSeqStore: &mut SeqStore_t = &mut (*zc).blockSplitCtx.currSeqStore;
    let numSplits = ZSTD_deriveBlockSplits(zc, partitions, nbSeq);

    let mut dRep = (*(*zc).blockState.prevCBlock).rep;
    let mut cRep = (*(*zc).blockState.prevCBlock).rep;
    ptr::write_bytes(
        nextSeqStore as *mut SeqStore_t as *mut u8,
        0,
        size_of::<SeqStore_t>(),
    );

    if numSplits == 0 {
        return ZSTD_compressSeqStore_singleBlock(
            zc,
            &(*zc).seqStore,
            &mut dRep,
            &mut cRep,
            op as *mut core::ffi::c_void,
            dstCapacity,
            ip as *const core::ffi::c_void,
            blockSize,
            lastBlock,
            false,
        );
    }

    ZSTD_deriveSeqStoreChunk(currSeqStore, &(*zc).seqStore, 0, *partitions as usize);
    for i in 0..numSplits + 1 {
        let lastPartition = i == numSplits;
        let mut lastBlockEntireSrc = false;
        let mut srcBytes = ZSTD_countSeqStoreLiteralsBytes(currSeqStore)
            .wrapping_add(ZSTD_countSeqStoreMatchBytes(currSeqStore));
        srcBytesTotal = srcBytesTotal.wrapping_add(srcBytes);
        if lastPartition {
            // This is the final partition, need to account for possible last literals
            srcBytes = srcBytes.wrapping_add(blockSize.wrapping_sub(srcBytesTotal));
            lastBlockEntireSrc = lastBlock;
        } else {
            ZSTD_deriveSeqStoreChunk(
                nextSeqStore,
                &(*zc).seqStore,
                *partitions.add(i) as usize,
                *partitions.add(i.wrapping_add(1)) as usize,
            );
        }

        let cSizeChunk = ZSTD_compressSeqStore_singleBlock(
            zc,
            currSeqStore,
            &mut dRep,
            &mut cRep,
            op as *mut core::ffi::c_void,
            dstCapacity,
            ip as *const core::ffi::c_void,
            srcBytes,
            lastBlockEntireSrc,
            true,
        )?;

        ip = ip.add(srcBytes);
        op = op.add(cSizeChunk);
        dstCapacity = dstCapacity.wrapping_sub(cSizeChunk);
        cSize = cSize.wrapping_add(cSizeChunk);
        *currSeqStore = *nextSeqStore;
    }

    // cRep and dRep may have diverged during the compression.
    // If so, we use the dRep repcodes for the next block.
    (*(*zc).blockState.prevCBlock).rep = dRep;
    Ok(cSize)
}

unsafe fn ZSTD_compressBlock_splitBlock(
    zc: *mut ZSTD_CCtx,
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
    lastBlock: bool,
) -> Result<usize, Error> {
    let bss = ZSTD_buildSeqStore(zc, src, srcSize)?;

    if bss == BuildSeqStore::NoCompress {
        (*(*zc).blockState.prevCBlock)
            .entropy
            .fse
            .offcode_repeatMode
            .require_check();
        if (*zc).seqCollector.collectSequences != 0 {
            return Err(Error::sequenceProducer_failed);
        }
        return ZSTD_noCompressBlock(dst, dstCapacity, src, srcSize, lastBlock);
    }

    let nbSeq = ((*zc).seqStore.sequences).offset_from_unsigned((*zc).seqStore.sequencesStart);
    ZSTD_compressBlock_splitBlock_internal(zc, dst, dstCapacity, src, srcSize, lastBlock, nbSeq)
}

unsafe fn ZSTD_compressBlock_internal(
    zc: *mut ZSTD_CCtx,
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
    frame: bool,
) -> Result<usize, Error> {
    // This is an estimated upper bound for the length of an rle block.
    // This isn't the actual upper bound.
    // Finding the real threshold needs further investigation.
    let rleMaxLength = 25;
    let ip = src as *const u8;
    let op = dst as *mut u8;

    let bss = ZSTD_buildSeqStore(zc, src, srcSize)?;

    let mut cSize: usize;
    if bss == BuildSeqStore::NoCompress {
        if (*zc).seqCollector.collectSequences != 0 {
            return Err(Error::sequenceProducer_failed);
        }
        cSize = 0;
    } else {
        if (*zc).seqCollector.collectSequences != 0 {
            ZSTD_copyBlockSequences(
                &mut (*zc).seqCollector,
                &(*zc).seqStore,
                &(*(*zc).blockState.prevCBlock).rep,
            )?;
            ZSTD_blockState_confirmRepcodesAndEntropyTables(&mut (*zc).blockState);
            return Ok(0);
        }

        // encode sequences and literals
        cSize = ZSTD_entropyCompressSeqStore(
            &(*zc).seqStore,
            &(*(*zc).blockState.prevCBlock).entropy,
            &mut (*(*zc).blockState.nextCBlock).entropy,
            &(*zc).appliedParams,
            dst,
            dstCapacity,
            srcSize,
            (*zc).tmpWorkspace,
            (*zc).tmpWkspSize,
            (*zc).bmi2 != 0,
        )?;

        // We don't want to emit our first block as a RLE even if it qualifies because
        // doing so will cause the decoder (cli only) to throw a "should consume all input error."
        // This is only an issue for zstd <= v1.4.3
        if frame
            && (*zc).isFirstBlock == 0
            && cSize < rleMaxLength as usize
            && ZSTD_isRLE(core::slice::from_raw_parts(ip, srcSize))
        {
            cSize = 1;
            *op = *ip;
        }
    }
    if cSize > 1 {
        ZSTD_blockState_confirmRepcodesAndEntropyTables(&mut (*zc).blockState);
    }

    // We check that dictionaries have offset codes available for the first
    // block. After the first block, the offcode table might not have large
    // enough codes to represent the offsets in the data.
    (*(*zc).blockState.prevCBlock)
        .entropy
        .fse
        .offcode_repeatMode
        .require_check();

    Ok(cSize)
}

unsafe fn ZSTD_compressBlock_targetCBlockSize_body(
    zc: *mut ZSTD_CCtx,
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
    bss: BuildSeqStore,
    lastBlock: bool,
) -> Result<usize, Error> {
    if bss == BuildSeqStore::Compress {
        if (*zc).isFirstBlock == 0
            && ZSTD_maybeRLE(&(*zc).seqStore)
            && ZSTD_isRLE(core::slice::from_raw_parts(src.cast::<u8>(), srcSize))
        {
            return ZSTD_rleCompressBlock(
                dst,
                dstCapacity,
                *(src as *const u8),
                srcSize,
                lastBlock,
            );
        }
        match ZSTD_compressSuperBlock(zc, dst, dstCapacity, src, srcSize, lastBlock) {
            Ok(cSize) => {
                let maxCSize = srcSize
                    .wrapping_sub(ZSTD_minGain(srcSize, (*zc).appliedParams.cParams.strategy));
                if cSize != 0 && cSize < maxCSize.wrapping_add(ZSTD_BLOCKHEADERSIZE) {
                    ZSTD_blockState_confirmRepcodesAndEntropyTables(&mut (*zc).blockState);
                    return Ok(cSize);
                }
            }
            Err(Error::dstSize_tooSmall) => {} // continue to noCompressBlock
            Err(err) => return Err(err),
        };
    }

    // Superblock compression failed, attempt to emit a single no compress block.
    // The decoder will be able to stream this block since it is uncompressed.
    ZSTD_noCompressBlock(dst, dstCapacity, src, srcSize, lastBlock)
}

unsafe fn ZSTD_compressBlock_targetCBlockSize(
    zc: *mut ZSTD_CCtx,
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
    lastBlock: bool,
) -> Result<usize, Error> {
    let bss = ZSTD_buildSeqStore(zc, src, srcSize)?;

    let cSize = ZSTD_compressBlock_targetCBlockSize_body(
        zc,
        dst,
        dstCapacity,
        src,
        srcSize,
        bss,
        lastBlock,
    )?;

    (*(*zc).blockState.prevCBlock)
        .entropy
        .fse
        .offcode_repeatMode
        .require_check();

    Ok(cSize)
}

unsafe fn ZSTD_overflowCorrectIfNeeded(
    ms: &mut ZSTD_MatchState_t,
    ws: &mut ZSTD_cwksp,
    params: &ZSTD_CCtx_params,
    ip: *const core::ffi::c_void,
    iend: *const core::ffi::c_void,
) {
    let cycleLog = ZSTD_cycleLog(params.cParams.chainLog, params.cParams.strategy);
    let maxDist = 1 << params.cParams.windowLog;
    if ms
        .window
        .needs_overflow_correction(cycleLog, maxDist, ms.loadedDictEnd, ip, iend)
    {
        let correction = ms.window.correct_overflow(cycleLog, maxDist, ip);
        ZSTD_cwksp_mark_tables_dirty(ws);
        ZSTD_reduceIndex(ms, params, correction);
        ZSTD_cwksp_mark_tables_clean(ws);
        if ms.nextToUpdate < correction {
            ms.nextToUpdate = 0;
        } else {
            ms.nextToUpdate = ms.nextToUpdate.wrapping_sub(correction);
        }
        // invalidate dictionaries on overflow correction
        ms.loadedDictEnd = 0;
        ms.dictMatchState = core::ptr::null();
    }
}

unsafe fn ZSTD_optimalBlockSize(
    cctx: *mut ZSTD_CCtx,
    src: *const core::ffi::c_void,
    srcSize: usize,
    blockSizeMax: usize,
    mut splitLevel: core::ffi::c_int,
    strat: ZSTD_strategy,
    savings: S64,
) -> usize {
    // Split level based on compression strategy, from `fast` to `btultra2`
    static splitLevels: [core::ffi::c_int; 10] = [0, 0, 1, 2, 2, 3, 3, 4, 4, 4];
    // Note: conservatively only split full blocks (128 KB) currently.
    // While it's possible to go lower, let's keep it simple for a first implementation.
    // Besides, benefits of splitting are reduced when blocks are already small.
    if srcSize < (128 * (1 << 10)) || blockSizeMax < (128 * (1 << 10)) {
        return srcSize.min(blockSizeMax);
    }
    // Do not split incompressible data though:
    // Require verified savings to allow pre-splitting.
    // Note: as a consequence, the first full block is not split.
    if savings < 3 {
        return 128 * (1 << 10);
    }
    // Apply @splitLevel, or use default value (which depends on @strat).
    // Note that splitting heuristic is still conditioned by @savings >= 3,
    // so the first block will not reach this code path.
    if splitLevel == 1 {
        return 128 * (1 << 10);
    }
    if splitLevel == 0 {
        splitLevel = splitLevels[strat as usize];
    } else {
        splitLevel -= 2;
    }
    ZSTD_splitBlock(
        src,
        blockSizeMax,
        splitLevel,
        (*cctx).tmpWorkspace,
        (*cctx).tmpWkspSize,
    )
}

/// Compress a chunk of data into one or multiple blocks.
/// All blocks will be terminated, all input will be consumed.
/// Function will issue an error if there is not enough `dstCapacity` to hold the compressed content.
/// Frame is supposed already started (header already produced)
///
/// # Returns
///
/// The compressed size, or an error code
unsafe fn ZSTD_compress_frameChunk(
    cctx: *mut ZSTD_CCtx,
    dst: *mut core::ffi::c_void,
    mut dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
    lastFrameChunk: bool,
) -> Result<usize, Error> {
    let blockSizeMax = (*cctx).blockSizeMax;
    let mut remaining = srcSize;
    let mut ip = src as *const u8;
    let ostart = dst as *mut u8;
    let mut op = ostart;
    let maxDist = 1 << (*cctx).appliedParams.cParams.windowLog;
    let mut savings = (*cctx).consumedSrcSize as S64 - (*cctx).producedCSize as S64;

    if (*cctx).appliedParams.fParams.checksumFlag != 0 && srcSize != 0 {
        ZSTD_XXH64_update_slice(
            &mut (*cctx).xxhState,
            core::slice::from_raw_parts(src as *const u8, srcSize),
        );
    }

    while remaining != 0 {
        let ms: &mut ZSTD_MatchState_t = &mut (*cctx).blockState.matchState;
        let blockSize = ZSTD_optimalBlockSize(
            cctx,
            ip as *const core::ffi::c_void,
            remaining,
            blockSizeMax,
            (*cctx).appliedParams.preBlockSplitter_level,
            (*cctx).appliedParams.cParams.strategy,
            savings,
        );
        let lastBlock = lastFrameChunk && blockSize == remaining;

        if dstCapacity < ZSTD_BLOCKHEADERSIZE.wrapping_add(1 + 1).wrapping_add(1) {
            return Err(Error::dstSize_tooSmall);
        }

        ZSTD_overflowCorrectIfNeeded(
            ms,
            &mut (*cctx).workspace,
            &(*cctx).appliedParams,
            ip as *const core::ffi::c_void,
            ip.add(blockSize) as *const core::ffi::c_void,
        );
        ZSTD_checkDictValidity(
            &ms.window,
            ip.add(blockSize) as *const core::ffi::c_void,
            maxDist,
            &mut ms.loadedDictEnd,
            &mut ms.dictMatchState,
        );
        ms.window.enforce_max_dist(
            ip as *const core::ffi::c_void,
            maxDist,
            &mut ms.loadedDictEnd,
            Some(&mut ms.dictMatchState),
        );

        // Ensure hash/chain table insertion resumes no sooner than lowlimit
        ms.nextToUpdate = Ord::max(ms.nextToUpdate, ms.window.lowLimit);

        let mut cSize: usize;
        if (*cctx).appliedParams.use_target_cblock_size() {
            cSize = ZSTD_compressBlock_targetCBlockSize(
                cctx,
                op as *mut core::ffi::c_void,
                dstCapacity,
                ip as *const core::ffi::c_void,
                blockSize,
                lastBlock,
            )?;
        } else if (*cctx).appliedParams.block_splitter_enabled() {
            cSize = ZSTD_compressBlock_splitBlock(
                cctx,
                op as *mut core::ffi::c_void,
                dstCapacity,
                ip as *const core::ffi::c_void,
                blockSize,
                lastBlock,
            )?;
        } else {
            cSize = ZSTD_compressBlock_internal(
                cctx,
                op.add(ZSTD_BLOCKHEADERSIZE) as *mut core::ffi::c_void,
                dstCapacity.wrapping_sub(ZSTD_BLOCKHEADERSIZE),
                ip as *const core::ffi::c_void,
                blockSize,
                true,
            )?;
            if cSize == 0 {
                cSize = ZSTD_noCompressBlock(
                    op as *mut core::ffi::c_void,
                    dstCapacity,
                    ip as *const core::ffi::c_void,
                    blockSize,
                    lastBlock,
                )?;
            } else {
                let cBlockHeader = if cSize == 1 {
                    u32::from(lastBlock)
                        .wrapping_add((BlockType::Rle as u32) << 1)
                        .wrapping_add((blockSize << 3) as u32)
                } else {
                    u32::from(lastBlock)
                        .wrapping_add((BlockType::Compressed as u32) << 1)
                        .wrapping_add((cSize << 3) as u32)
                };
                MEM_writeLE24(op as *mut core::ffi::c_void, cBlockHeader);
                cSize = cSize.wrapping_add(ZSTD_BLOCKHEADERSIZE);
            }
        }

        // @savings is employed to ensure that splitting doesn't worsen expansion of incompressible data.
        // Without splitting, the maximum expansion is 3 bytes per full block.
        // An adversarial input could attempt to fudge the split detector,
        // and make it split incompressible data, resulting in more block headers.
        //
        // Note that, since ZSTD_COMPRESSBOUND() assumes a worst case scenario of 1KB per block,
        // and the splitter never creates blocks that small (current lower limit is 8 KB),
        // there is already no risk to expand beyond ZSTD_COMPRESSBOUND() limit.
        // But if the goal is to not expand by more than 3-bytes per 128 KB full block,
        // then yes, it becomes possible to make the block splitter oversplit incompressible data.
        //
        // Using @savings, we enforce an even more conservative condition,
        // requiring the presence of enough savings (at least 3 bytes) to authorize splitting,
        // otherwise only full blocks are used.
        // But being conservative is fine,
        // since splitting barely compressible blocks is not fruitful anyway.
        savings += blockSize as S64 - cSize as S64;
        ip = ip.add(blockSize);
        remaining = remaining.wrapping_sub(blockSize);
        op = op.add(cSize);
        dstCapacity = dstCapacity.wrapping_sub(cSize);
        (*cctx).isFirstBlock = 0;
    }

    if lastFrameChunk && op > ostart {
        (*cctx).stage = CompressionStage::Ending;
    }
    Ok(op.offset_from_unsigned(ostart))
}

unsafe fn ZSTD_writeFrameHeader(
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    params: &ZSTD_CCtx_params,
    pledgedSrcSize: u64,
    dictID: u32,
) -> Result<usize, Error> {
    let op = dst as *mut u8;
    let dictIDSizeCodeLength =
        u32::from(dictID > 0) + u32::from(dictID >= 256) + u32::from(dictID >= 65536);
    let dictIDSizeCode = if params.fParams.noDictIDFlag != 0 {
        0
    } else {
        dictIDSizeCodeLength
    };
    let checksumFlag = u32::from(params.fParams.checksumFlag > 0);
    let windowSize = 1 << params.cParams.windowLog;
    let singleSegment =
        u32::from(params.fParams.contentSizeFlag != 0 && windowSize as u64 >= pledgedSrcSize);
    let windowLogByte = ((params.cParams.windowLog)
        .wrapping_sub(ZSTD_WINDOWLOG_ABSOLUTEMIN as core::ffi::c_uint)
        << 3) as u8;
    let fcsCode = if params.fParams.contentSizeFlag != 0 {
        u32::from(pledgedSrcSize >= 256)
            + u32::from(pledgedSrcSize >= (65536 + 256))
            + u32::from(pledgedSrcSize >= 0xffffffff)
    } else {
        0
    };
    let frameHeaderDescriptionByte = dictIDSizeCode
        .wrapping_add(checksumFlag << 2)
        .wrapping_add(singleSegment << 5)
        .wrapping_add(fcsCode << 6) as u8;
    let mut pos = 0usize;

    if dstCapacity < 18 {
        return Err(Error::dstSize_tooSmall);
    }
    if params.format == Format::ZSTD_f_zstd1 {
        MEM_writeLE32(dst, ZSTD_MAGICNUMBER);
        pos = 4;
    }
    *op.add(pos) = frameHeaderDescriptionByte;
    pos = pos.wrapping_add(1);
    if singleSegment == 0 {
        *op.add(pos) = windowLogByte;
        pos = pos.wrapping_add(1);
    }
    match dictIDSizeCode {
        1 => {
            *op.add(pos) = dictID as u8;
            pos = pos.wrapping_add(1);
        }
        2 => {
            MEM_writeLE16(op.add(pos) as *mut core::ffi::c_void, dictID as u16);
            pos = pos.wrapping_add(2);
        }
        3 => {
            MEM_writeLE32(op.add(pos) as *mut core::ffi::c_void, dictID);
            pos = pos.wrapping_add(4);
        }
        _ => {}
    }
    match fcsCode {
        1 => {
            MEM_writeLE16(
                op.add(pos) as *mut core::ffi::c_void,
                pledgedSrcSize.wrapping_sub(256) as u16,
            );
            pos = pos.wrapping_add(2);
        }
        2 => {
            MEM_writeLE32(op.add(pos) as *mut core::ffi::c_void, pledgedSrcSize as u32);
            pos = pos.wrapping_add(4);
        }
        3 => {
            MEM_writeLE64(op.add(pos) as *mut core::ffi::c_void, pledgedSrcSize);
            pos = pos.wrapping_add(8);
        }
        _ => {
            if singleSegment != 0 {
                *op.add(pos) = pledgedSrcSize as u8;
                pos = pos.wrapping_add(1);
            }
        }
    }
    Ok(pos)
}

/// Writes out a skippable frame with the specified magic number variant (16 are supported),
/// from ZSTD_MAGIC_SKIPPABLE_START to ZSTD_MAGIC_SKIPPABLE_START+15, and the desired source data.
///
/// # Returns
///
/// The total number of bytes written, or a ZSTD error code.
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_writeSkippableFrame))]
pub unsafe extern "C" fn ZSTD_writeSkippableFrame(
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
    magicVariant: core::ffi::c_uint,
) -> usize {
    let op = dst as *mut u8;
    if dstCapacity < srcSize.wrapping_add(8) {
        return Error::dstSize_tooSmall.to_error_code();
    }
    if srcSize > 0xffffffff {
        return Error::srcSize_wrong.to_error_code();
    }
    if magicVariant > 15 {
        return Error::parameter_outOfBound.to_error_code();
    }

    MEM_writeLE32(
        op as *mut core::ffi::c_void,
        ZSTD_MAGIC_SKIPPABLE_START.wrapping_add(magicVariant),
    );
    MEM_writeLE32(op.add(4) as *mut core::ffi::c_void, srcSize as u32);
    core::ptr::copy_nonoverlapping(src.cast::<u8>(), op.add(8), srcSize);

    srcSize.wrapping_add(ZSTD_SKIPPABLEHEADERSIZE as usize)
}

/// Output an empty Block with end-of-frame mark to complete a frame.
///
/// # Returns
///
/// Size of data written into `dst` (== ZSTD_blockHeaderSize) or an error code if `dstCapacity`
/// is too small (<ZSTD_blockHeaderSize)
pub unsafe fn ZSTD_writeLastEmptyBlock(dst: *mut core::ffi::c_void, dstCapacity: usize) -> usize {
    if dstCapacity < ZSTD_BLOCKHEADERSIZE {
        return Error::dstSize_tooSmall.to_error_code();
    }
    let cBlockHeader24 = 1u32.wrapping_add((BlockType::Raw as u32) << 1);
    MEM_writeLE24(dst, cBlockHeader24);
    ZSTD_BLOCKHEADERSIZE
}

pub unsafe fn ZSTD_referenceExternalSequences(
    cctx: *mut ZSTD_CCtx,
    seq: *mut rawSeq,
    nbSeq: usize,
) {
    (*cctx).externSeqStore.seq = seq;
    (*cctx).externSeqStore.size = nbSeq;
    (*cctx).externSeqStore.capacity = nbSeq;
    (*cctx).externSeqStore.pos = 0;
    (*cctx).externSeqStore.posInSequence = 0;
}

pub(crate) unsafe fn ZSTD_compressContinue_internal(
    cctx: *mut ZSTD_CCtx,
    mut dst: *mut core::ffi::c_void,
    mut dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
    frame: bool,
    lastFrameChunk: bool,
) -> Result<usize, Error> {
    let ms: &mut ZSTD_MatchState_t = &mut (*cctx).blockState.matchState;
    let mut fhSize = 0;

    if (*cctx).stage == CompressionStage::Created {
        return Err(Error::stage_wrong);
    }

    if frame && (*cctx).stage == CompressionStage::Init {
        fhSize = ZSTD_writeFrameHeader(
            dst,
            dstCapacity,
            &(*cctx).appliedParams,
            ((*cctx).pledgedSrcSizePlusOne).wrapping_sub(1),
            (*cctx).dictID,
        )?;
        dstCapacity = dstCapacity.wrapping_sub(fhSize);
        dst = (dst as *mut core::ffi::c_char).add(fhSize) as *mut core::ffi::c_void;
        (*cctx).stage = CompressionStage::Ongoing;
    }

    if srcSize == 0 {
        // Do not generate an empty block if no input
        return Ok(fhSize);
    }

    if !ms.window.update(src, srcSize, ms.forceNonContiguous != 0) {
        ms.forceNonContiguous = 0;
        ms.nextToUpdate = ms.window.dictLimit;
    }
    if (*cctx).appliedParams.ldmParams.enableLdm == ParamSwitch::Enable {
        (*cctx).ldmState.window.update(src, srcSize, false);
    }

    if !frame {
        // Overflow check and correction for block mode
        ZSTD_overflowCorrectIfNeeded(
            ms,
            &mut (*cctx).workspace,
            &(*cctx).appliedParams,
            src,
            src.byte_add(srcSize),
        );
    }

    let cSize = if frame {
        ZSTD_compress_frameChunk(cctx, dst, dstCapacity, src, srcSize, lastFrameChunk)?
    } else {
        ZSTD_compressBlock_internal(cctx, dst, dstCapacity, src, srcSize, false)?
    };
    (*cctx).consumedSrcSize =
        ((*cctx).consumedSrcSize).wrapping_add(srcSize as core::ffi::c_ulonglong);
    (*cctx).producedCSize =
        ((*cctx).producedCSize).wrapping_add(cSize.wrapping_add(fhSize) as core::ffi::c_ulonglong);
    if (*cctx).pledgedSrcSizePlusOne != 0
        && ((*cctx).consumedSrcSize).wrapping_add(1) > (*cctx).pledgedSrcSizePlusOne
    {
        return Err(Error::srcSize_wrong);
    }

    Ok(cSize.wrapping_add(fhSize))
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_compressContinue_public))]
pub unsafe extern "C" fn ZSTD_compressContinue_public(
    cctx: *mut ZSTD_CCtx,
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    match ZSTD_compressContinue_internal(cctx, dst, dstCapacity, src, srcSize, true, false) {
        Ok(cSize) => cSize,
        Err(err) => err.to_error_code(),
    }
}

#[deprecated(
    since = "0.0.0+zstd.1.5.5",
    note = "The buffer-less API is deprecated in favor of the normal streaming API. See docs."
)]
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_compressContinue))]
pub unsafe extern "C" fn ZSTD_compressContinue(
    cctx: *mut ZSTD_CCtx,
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    ZSTD_compressContinue_public(cctx, dst, dstCapacity, src, srcSize)
}

#[deprecated]
unsafe fn ZSTD_getBlockSize_deprecated(cctx: *const ZSTD_CCtx) -> usize {
    let cParams = (*cctx).appliedParams.cParams;
    (*cctx)
        .appliedParams
        .maxBlockSize
        .min(1 << cParams.windowLog)
}

#[deprecated(
    note = "The block API is deprecated in favor of the normal compression API. See docs."
)]
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_getBlockSize))]
pub unsafe extern "C" fn ZSTD_getBlockSize(cctx: *const ZSTD_CCtx) -> usize {
    #[allow(deprecated)]
    ZSTD_getBlockSize_deprecated(cctx)
}

#[deprecated(note = "Deprecated definitions that are still used internally.")]
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_compressBlock_deprecated))]
pub unsafe extern "C" fn ZSTD_compressBlock_deprecated(
    cctx: *mut ZSTD_CCtx,
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    #[allow(deprecated)]
    let blockSizeMax = ZSTD_getBlockSize_deprecated(cctx);
    if srcSize > blockSizeMax {
        return Error::srcSize_wrong.to_error_code();
    }

    match ZSTD_compressContinue_internal(cctx, dst, dstCapacity, src, srcSize, false, false) {
        Ok(cSize) => cSize,
        Err(err) => err.to_error_code(),
    }
}

#[deprecated(
    note = "The block API is deprecated in favor of the normal compression API. See docs."
)]
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_compressBlock))]
pub unsafe extern "C" fn ZSTD_compressBlock(
    cctx: *mut ZSTD_CCtx,
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    #[allow(deprecated)]
    ZSTD_compressBlock_deprecated(cctx, dst, dstCapacity, src, srcSize)
}

unsafe fn ZSTD_loadDictionaryContent(
    ms: &mut ZSTD_MatchState_t,
    ls: Option<&mut LdmState>,
    ws: &mut ZSTD_cwksp,
    params: &ZSTD_CCtx_params,
    mut src: *const core::ffi::c_void,
    mut srcSize: usize,
    dtlm: DictTableLoadMethod,
    tfp: TableFillPurpose,
) {
    let mut ip = src as *const u8;
    let iend = ip.add(srcSize);
    // `Some` when the dict must also be loaded into the LDM matchfinders.
    let loadLdmDict = ls.filter(|_| params.ldmParams.enableLdm == ParamSwitch::Enable);

    // Assert that the ms params match the params we're being given
    ZSTD_assertEqualCParams(params.cParams, ms.cParams);

    // Ensure large dictionaries can't cause index overflow
    let mut maxDictSize = ZSTD_CURRENT_MAX.wrapping_sub(ZSTD_WINDOW_START_INDEX as usize);

    if params.cParams.cdict_indices_are_tagged() && tfp == TableFillPurpose::ForCDict {
        let shortCacheMaxDictSize = (1usize << (32 - ZSTD_SHORT_CACHE_TAG_BITS))
            .wrapping_sub(ZSTD_WINDOW_START_INDEX as usize);
        maxDictSize = maxDictSize.min(shortCacheMaxDictSize);
    }

    // If the dictionary is too large, only load the suffix of the dictionary.
    if srcSize > maxDictSize {
        ip = iend.sub(maxDictSize);
        src = ip as *const core::ffi::c_void;
        srcSize = maxDictSize;
    }

    if srcSize > ZSTD_CHUNKSIZE_MAX {
        // We must have cleared our windows when our source is this large.
        assert!(loadLdmDict.is_some());
    }

    ms.window.update(src, srcSize, false);

    if let Some(ls) = loadLdmDict {
        // Load the entire dict into LDM matchfinders.
        ls.window.update(src, srcSize, false);
        ls.loadedDictEnd = if params.forceWindow != 0 {
            0
        } else {
            iend.wrapping_offset_from(ls.window.base) as u32
        };
        ls.fill_hash_table(core::slice::from_raw_parts(ip, srcSize), &params.ldmParams);
    }

    // If the dict is larger than we can reasonably index in our tables, only load the suffix.
    let maxDictSize_0 = 1
        << Ord::min(
            Ord::max(params.cParams.hashLog + 3, params.cParams.chainLog + 1),
            31,
        );
    if srcSize > maxDictSize_0 as usize {
        ip = iend.sub(maxDictSize_0 as usize);
        srcSize = maxDictSize_0 as usize;
    }

    ms.nextToUpdate = ip.wrapping_offset_from(ms.window.base) as core::ffi::c_long as u32;
    ms.loadedDictEnd = if params.forceWindow != 0 {
        0
    } else {
        iend.wrapping_offset_from(ms.window.base) as core::ffi::c_long as u32
    };
    ms.forceNonContiguous = params.deterministicRefPrefix;

    if srcSize <= HASH_READ_SIZE as usize {
        return;
    }

    ZSTD_overflowCorrectIfNeeded(
        ms,
        ws,
        params,
        ip as *const core::ffi::c_void,
        iend as *const core::ffi::c_void,
    );

    match params.cParams.strategy as core::ffi::c_uint {
        1 => {
            ZSTD_fillHashTable(ms, iend as *const core::ffi::c_void, dtlm, tfp);
        }
        2 => {
            ZSTD_fillDoubleHashTable(ms, iend as *const core::ffi::c_void, dtlm, tfp);
        }
        3..=5 => {
            if ms.dedicatedDictSearch != 0 {
                ZSTD_dedicatedDictSearch_lazy_loadDictionary(ms, iend.sub(HASH_READ_SIZE as usize));
            } else if params.useRowMatchFinder == ParamSwitch::Enable {
                let tagTableSize = 1 << params.cParams.hashLog;
                ptr::write_bytes(ms.tagTable, 0, tagTableSize as usize);
                ZSTD_row_update(ms, iend.sub(HASH_READ_SIZE as usize));
            } else {
                ZSTD_insertAndFindFirstIndex(ms, iend.sub(HASH_READ_SIZE as usize));
            }
        }
        6..=9 => {
            ZSTD_updateTree(ms, iend.sub(HASH_READ_SIZE as usize), iend);
        }
        _ => {}
    }

    ms.nextToUpdate = iend.wrapping_offset_from(ms.window.base) as core::ffi::c_long as u32;
}

/// Dictionaries that assign zero probability to symbols that show up causes problems when FSE
/// encoding. Mark dictionaries with zero probability symbols as [`FSE_repeat::Check`] and only
/// dictionaries with 100% valid symbols can be assumed valid.
fn ZSTD_dictNCountRepeat(
    normalizedCounter: &[core::ffi::c_short],
    dictMaxSymbolValue: u8,
    maxSymbolValue: u8,
) -> FSE_repeat {
    if dictMaxSymbolValue < maxSymbolValue {
        return FSE_repeat::Check;
    }
    for &count in &normalizedCounter[..usize::from(maxSymbolValue) + 1] {
        if count as core::ffi::c_int == 0 {
            return FSE_repeat::Check;
        }
    }
    FSE_repeat::Valid
}

pub unsafe fn ZSTD_loadCEntropy(
    bs: *mut ZSTD_compressedBlockState_t,
    workspace: *mut core::ffi::c_void,
    dict: &[u8],
) -> Result<usize, Error> {
    let mut offcodeNCount: [core::ffi::c_short; 32] = [0; 32];
    let mut offcodeMaxValue = MaxOff;
    let mut pos = 8;
    (*bs).entropy.huf.repeatMode = HUF_repeat::Check;

    let mut maxSymbolValue = u8::MAX;
    let mut hasZeroWeights = 1;
    let hufHeaderSize = HUF_readCTable(
        &mut (*bs).entropy.huf.CTable,
        &mut maxSymbolValue,
        &dict[pos..],
        &mut hasZeroWeights,
    )
    .map_err(|_| Error::dictionary_corrupted)?;

    // We only set the loaded table as valid if it contains all non-zero
    // weights. Otherwise, we set it to check
    if hasZeroWeights == 0 && maxSymbolValue == u8::MAX {
        (*bs).entropy.huf.repeatMode = HUF_repeat::Valid;
    }

    pos += hufHeaderSize;

    let mut offcodeLog: core::ffi::c_uint = 0;
    let offcodeHeaderSize = FSE_readNCount(
        &mut offcodeNCount,
        &mut offcodeMaxValue,
        &mut offcodeLog,
        &dict[pos..],
    )
    .map_err(|_| Error::dictionary_corrupted)?;
    if offcodeLog > 8 {
        return Err(Error::dictionary_corrupted);
    }
    // fill all offset symbols to avoid garbage at end of table
    FSE_buildCTable_wksp(
        &mut (*bs).entropy.fse.offcodeCTable,
        &offcodeNCount,
        31,
        offcodeLog,
        workspace,
        (8 << 10) + 512,
    )
    .map_err(|_| Error::dictionary_corrupted)?;
    // Defer checking offcodeMaxValue because we need to know the size of the dictionary content
    pos += offcodeHeaderSize;

    let mut matchlengthNCount: [core::ffi::c_short; 53] = [0; 53];
    let mut matchlengthMaxValue = MaxML;
    let mut matchlengthLog: core::ffi::c_uint = 0;
    let matchlengthHeaderSize = FSE_readNCount(
        &mut matchlengthNCount,
        &mut matchlengthMaxValue,
        &mut matchlengthLog,
        &dict[pos..],
    )
    .map_err(|_| Error::dictionary_corrupted)?;
    if matchlengthLog > 9 {
        return Err(Error::dictionary_corrupted);
    }
    FSE_buildCTable_wksp(
        &mut (*bs).entropy.fse.matchlengthCTable,
        &matchlengthNCount,
        matchlengthMaxValue,
        matchlengthLog,
        workspace,
        (8 << 10) + 512,
    )
    .map_err(|_| Error::dictionary_corrupted)?;
    (*bs).entropy.fse.matchlength_repeatMode =
        ZSTD_dictNCountRepeat(&matchlengthNCount, matchlengthMaxValue, MaxML);
    pos += matchlengthHeaderSize;

    let mut litlengthNCount: [core::ffi::c_short; 36] = [0; 36];
    let mut litlengthMaxValue = MaxLL;
    let mut litlengthLog: core::ffi::c_uint = 0;
    let litlengthHeaderSize = FSE_readNCount(
        &mut litlengthNCount,
        &mut litlengthMaxValue,
        &mut litlengthLog,
        &dict[pos..],
    )
    .map_err(|_| Error::dictionary_corrupted)?;
    if litlengthLog > 9 {
        return Err(Error::dictionary_corrupted);
    }
    FSE_buildCTable_wksp(
        &mut (*bs).entropy.fse.litlengthCTable,
        &litlengthNCount,
        litlengthMaxValue,
        litlengthLog,
        workspace,
        (8 << 10) + 512,
    )
    .map_err(|_| Error::dictionary_corrupted)?;
    (*bs).entropy.fse.litlength_repeatMode =
        ZSTD_dictNCountRepeat(&litlengthNCount, litlengthMaxValue, MaxLL);
    pos += litlengthHeaderSize;

    let ([rep0, rep1, rep2, ..], _) = dict[pos..].as_chunks::<4>() else {
        return Err(Error::dictionary_corrupted);
    };

    (*bs).rep = [
        u32::from_le_bytes(*rep0),
        u32::from_le_bytes(*rep1),
        u32::from_le_bytes(*rep2),
    ];

    pos += 12;

    let dictContentSize = dict[pos..].len();
    let mut offcodeMax = MaxOff;
    if dictContentSize <= u32::MAX.wrapping_sub(128 * (1 << 10)) as usize {
        let maxOffset = (dictContentSize as u32).wrapping_add(128 * (1 << 10));
        // `maxOffset` is a `u32`, so its highest set bit is at most 31
        offcodeMax = ZSTD_highbit32(maxOffset) as u8;
    }
    // All offset values <= dictContentSize + 128 KB must be representable for a valid table
    (*bs).entropy.fse.offcode_repeatMode =
        ZSTD_dictNCountRepeat(&offcodeNCount, offcodeMaxValue, offcodeMax.min(31));

    // All repCodes must be <= dictContentSize and != 0
    for size in (*bs).rep {
        if !(1..=dictContentSize).contains(&(size as usize)) {
            return Err(Error::dictionary_corrupted);
        }
    }

    Ok(pos)
}

/// Assumptions: magic number supposed already checked, dictSize supposed >= 8.
///
/// # Returns
///
/// dictID, or an error code.
unsafe fn ZSTD_loadZstdDictionary(
    bs: *mut ZSTD_compressedBlockState_t,
    ms: &mut ZSTD_MatchState_t,
    ws: &mut ZSTD_cwksp,
    params: &ZSTD_CCtx_params,
    dict: *const core::ffi::c_void,
    dictSize: usize,
    dtlm: DictTableLoadMethod,
    tfp: TableFillPurpose,
    workspace: *mut core::ffi::c_void,
) -> Result<usize, Error> {
    let mut dictPtr = dict as *const u8;
    let dictEnd = dictPtr.add(dictSize);

    let dictID = (if params.fParams.noDictIDFlag != 0 {
        0
    } else {
        MEM_readLE32(dictPtr.add(4) as *const core::ffi::c_void)
    }) as usize;
    let dict = core::slice::from_raw_parts(dictPtr, dictSize);
    let eSize = ZSTD_loadCEntropy(bs, workspace, dict)?;
    dictPtr = dictPtr.add(eSize);

    let dictContentSize = dictEnd.offset_from_unsigned(dictPtr);
    ZSTD_loadDictionaryContent(
        ms,
        None,
        ws,
        params,
        dictPtr as *const core::ffi::c_void,
        dictContentSize,
        dtlm,
        tfp,
    );

    Ok(dictID)
}

/// # Returns
///
/// dictID, or an error code.
unsafe fn ZSTD_compress_insertDictionary(
    bs: *mut ZSTD_compressedBlockState_t,
    ms: &mut ZSTD_MatchState_t,
    ls: Option<&mut LdmState>,
    ws: &mut ZSTD_cwksp,
    params: &ZSTD_CCtx_params,
    dict: *const core::ffi::c_void,
    dictSize: usize,
    dictContentType: ZSTD_dictContentType_e,
    dtlm: DictTableLoadMethod,
    tfp: TableFillPurpose,
    workspace: *mut core::ffi::c_void,
) -> Result<usize, Error> {
    if dict.is_null() || dictSize < 8 {
        if dictContentType == ZSTD_dct_fullDict {
            return Err(Error::dictionary_wrong);
        }
        return Ok(0);
    }

    ZSTD_reset_compressedBlockState(bs);

    // dict restricted modes
    if dictContentType == ZSTD_dct_rawContent {
        ZSTD_loadDictionaryContent(ms, ls, ws, params, dict, dictSize, dtlm, tfp);
        return Ok(0);
    }

    if MEM_readLE32(dict) != ZSTD_MAGIC_DICTIONARY {
        if dictContentType == ZSTD_dct_auto {
            ZSTD_loadDictionaryContent(ms, ls, ws, params, dict, dictSize, dtlm, tfp);
            return Ok(0);
        }
        if dictContentType == ZSTD_dct_fullDict {
            return Err(Error::dictionary_wrong);
        }
    }

    // dict as full zstd dictionary
    ZSTD_loadZstdDictionary(bs, ms, ws, params, dict, dictSize, dtlm, tfp, workspace)
}

pub const ZSTD_USE_CDICT_PARAMS_SRCSIZE_CUTOFF: core::ffi::c_int = 128 * (1 << 10);

pub const ZSTD_USE_CDICT_PARAMS_DICTSIZE_MULTIPLIER: core::ffi::c_ulonglong = 6;

/// Assumption: either @dict OR @cdict (or none) is non-NULL, never both.
unsafe fn ZSTD_compressBegin_internal(
    cctx: *mut ZSTD_CCtx,
    dict: *const core::ffi::c_void,
    dictSize: usize,
    dictContentType: ZSTD_dictContentType_e,
    dtlm: DictTableLoadMethod,
    cdict: *const ZSTD_CDict,
    params: &ZSTD_CCtx_params,
    pledgedSrcSize: u64,
    zbuff: BufferedPolicy,
) -> Result<(), Error> {
    let dictContentSize = if !cdict.is_null() {
        (*cdict).dictContentSize
    } else {
        dictSize
    };
    (*cctx).traceCtx = ZSTD_trace_compress_begin(cctx);

    if !cdict.is_null()
        && (*cdict).dictContentSize > 0
        && (pledgedSrcSize < ZSTD_USE_CDICT_PARAMS_SRCSIZE_CUTOFF as u64
            || (pledgedSrcSize as core::ffi::c_ulonglong)
                < ((*cdict).dictContentSize as core::ffi::c_ulonglong)
                    .wrapping_mul(ZSTD_USE_CDICT_PARAMS_DICTSIZE_MULTIPLIER)
            || pledgedSrcSize as core::ffi::c_ulonglong == ZSTD_CONTENTSIZE_UNKNOWN
            || (*cdict).compressionLevel == 0)
        && params.attachDictPref != ZSTD_dictAttachPref_e::ZSTD_dictForceLoad
    {
        return ZSTD_resetCCtx_usingCDict(cctx, cdict, params, pledgedSrcSize, zbuff);
    }

    ZSTD_resetCCtx_internal(
        cctx,
        params,
        pledgedSrcSize,
        dictContentSize,
        CompResetPolicy::MakeClean,
        zbuff,
    )?;

    let dictID = if !cdict.is_null() {
        ZSTD_compress_insertDictionary(
            (*cctx).blockState.prevCBlock,
            &mut (*cctx).blockState.matchState,
            Some(&mut (*cctx).ldmState),
            &mut (*cctx).workspace,
            &(*cctx).appliedParams,
            (*cdict).dictContent,
            (*cdict).dictContentSize,
            (*cdict).dictContentType,
            dtlm,
            TableFillPurpose::ForCCtx,
            (*cctx).tmpWorkspace,
        )?
    } else {
        ZSTD_compress_insertDictionary(
            (*cctx).blockState.prevCBlock,
            &mut (*cctx).blockState.matchState,
            Some(&mut (*cctx).ldmState),
            &mut (*cctx).workspace,
            &(*cctx).appliedParams,
            dict,
            dictSize,
            dictContentType,
            dtlm,
            TableFillPurpose::ForCCtx,
            (*cctx).tmpWorkspace,
        )?
    };
    (*cctx).dictID = dictID as u32;
    (*cctx).dictContentSize = dictContentSize;

    Ok(())
}

pub unsafe fn ZSTD_compressBegin_advanced_internal(
    cctx: *mut ZSTD_CCtx,
    dict: *const core::ffi::c_void,
    dictSize: usize,
    dictContentType: ZSTD_dictContentType_e,
    dtlm: DictTableLoadMethod,
    cdict: *const ZSTD_CDict,
    params: &ZSTD_CCtx_params,
    pledgedSrcSize: core::ffi::c_ulonglong,
) -> Result<(), Error> {
    // compression parameters verification and optimization
    params.cParams.check_bounds()?;

    ZSTD_compressBegin_internal(
        cctx,
        dict,
        dictSize,
        dictContentType,
        dtlm,
        cdict,
        params,
        pledgedSrcSize,
        BufferedPolicy::NotBuffered,
    )
}

/// # Returns
///
/// 0, or an error code.
#[deprecated(
    since = "0.0.0+zstd.1.5.0",
    note = "Use the advanced API to access custom parameters."
)]
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_compressBegin_advanced))]
pub unsafe extern "C" fn ZSTD_compressBegin_advanced(
    cctx: *mut ZSTD_CCtx,
    dict: *const core::ffi::c_void,
    dictSize: usize,
    params: ZSTD_parameters,
    pledgedSrcSize: core::ffi::c_ulonglong,
) -> usize {
    let cctxParams = ZSTD_CCtx_params_s::new_internal(&params, ZSTD_NO_CLEVEL);
    match ZSTD_compressBegin_advanced_internal(
        cctx,
        dict,
        dictSize,
        ZSTD_dct_auto,
        DictTableLoadMethod::Fast,
        core::ptr::null(),
        &cctxParams,
        pledgedSrcSize,
    ) {
        Ok(()) => 0,
        Err(err) => err.to_error_code(),
    }
}

unsafe fn ZSTD_compressBegin_usingDict_deprecated(
    cctx: *mut ZSTD_CCtx,
    dict: *const core::ffi::c_void,
    dictSize: usize,
    compressionLevel: core::ffi::c_int,
) -> Result<(), Error> {
    let params = ZSTD_parameters::new(
        compressionLevel,
        ZSTD_CONTENTSIZE_UNKNOWN,
        dictSize,
        CParamMode::NoAttachDict,
    );
    let cctxParams = ZSTD_CCtx_params_s::new_internal(
        &params,
        if compressionLevel == 0 {
            ZSTD_CLEVEL_DEFAULT
        } else {
            compressionLevel
        },
    );

    ZSTD_compressBegin_internal(
        cctx,
        dict,
        dictSize,
        ZSTD_dct_auto,
        DictTableLoadMethod::Fast,
        core::ptr::null(),
        &cctxParams,
        ZSTD_CONTENTSIZE_UNKNOWN,
        BufferedPolicy::NotBuffered,
    )
}

#[deprecated(
    since = "0.0.0+zstd.1.5.5",
    note = "The buffer-less API is deprecated in favor of the normal streaming API. See docs."
)]
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_compressBegin_usingDict))]
pub unsafe extern "C" fn ZSTD_compressBegin_usingDict(
    cctx: *mut ZSTD_CCtx,
    dict: *const core::ffi::c_void,
    dictSize: usize,
    compressionLevel: core::ffi::c_int,
) -> usize {
    match ZSTD_compressBegin_usingDict_deprecated(cctx, dict, dictSize, compressionLevel) {
        Ok(()) => 0,
        Err(err) => err.to_error_code(),
    }
}

#[deprecated(
    since = "0.0.0+zstd.1.5.5",
    note = "The buffer-less API is deprecated in favor of the normal streaming API. See docs."
)]
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_compressBegin))]
pub unsafe extern "C" fn ZSTD_compressBegin(
    cctx: *mut ZSTD_CCtx,
    compressionLevel: core::ffi::c_int,
) -> usize {
    match ZSTD_compressBegin_usingDict_deprecated(cctx, core::ptr::null(), 0, compressionLevel) {
        Ok(()) => 0,
        Err(err) => err.to_error_code(),
    }
}

/// Ends a frame.
///
/// # Returns
///
/// The number of bytes written into `dst`, or an error.
unsafe fn ZSTD_writeEpilogue(
    cctx: *mut ZSTD_CCtx,
    dst: *mut core::ffi::c_void,
    mut dstCapacity: usize,
) -> Result<usize, Error> {
    let ostart = dst as *mut u8;
    let mut op = ostart;

    if (*cctx).stage == CompressionStage::Created {
        return Err(Error::stage_wrong);
    }

    // special case: empty frame
    if (*cctx).stage == CompressionStage::Init {
        let fhSize = ZSTD_writeFrameHeader(dst, dstCapacity, &(*cctx).appliedParams, 0, 0)?;
        dstCapacity = dstCapacity.wrapping_sub(fhSize);
        op = op.add(fhSize);
        (*cctx).stage = CompressionStage::Ongoing;
    }

    if (*cctx).stage != CompressionStage::Ending {
        // write one last empty block, make it the "last" block
        let cBlockHeader24 = 1u32
            .wrapping_add((BlockType::Raw as u32) << 1)
            .wrapping_add(0);
        if dstCapacity < 3 {
            return Err(Error::dstSize_tooSmall);
        }
        MEM_writeLE24(op as *mut core::ffi::c_void, cBlockHeader24);
        op = op.add(ZSTD_BLOCKHEADERSIZE);
        dstCapacity = dstCapacity.wrapping_sub(ZSTD_BLOCKHEADERSIZE);
    }

    if (*cctx).appliedParams.fParams.checksumFlag != 0 {
        let checksum = ZSTD_XXH64_digest(&mut (*cctx).xxhState) as u32;
        if dstCapacity < 4 {
            return Err(Error::dstSize_tooSmall);
        }
        MEM_writeLE32(op as *mut core::ffi::c_void, checksum);
        op = op.add(4);
    }

    // return to "created but no init" status
    (*cctx).stage = CompressionStage::Created;
    Ok(op.offset_from_unsigned(ostart))
}

pub unsafe fn ZSTD_CCtx_trace(cctx: *mut ZSTD_CCtx, extraCSize: usize) {
    if (*cctx).traceCtx != 0 {
        let streaming = core::ffi::c_int::from(
            (*cctx).inBuffSize > 0
                || (*cctx).outBuffSize > 0
                || (*cctx).appliedParams.nbWorkers > 0,
        );
        let mut trace = ZSTD_Trace::default();
        trace.version = ZSTD_VERSION_NUMBER as core::ffi::c_uint;
        trace.streaming = streaming;
        trace.dictionaryID = (*cctx).dictID;
        trace.dictionarySize = (*cctx).dictContentSize;
        trace.uncompressedSize = (*cctx).consumedSrcSize as usize;
        trace.compressedSize =
            ((*cctx).producedCSize).wrapping_add(extraCSize as core::ffi::c_ulonglong) as usize;
        trace.params = &mut (*cctx).appliedParams;
        trace.cctx = cctx;
        ZSTD_trace_compress_end((*cctx).traceCtx, &trace);
    }

    (*cctx).traceCtx = 0;
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_compressEnd_public))]
pub unsafe extern "C" fn ZSTD_compressEnd_public(
    cctx: *mut ZSTD_CCtx,
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    ZSTD_compressEnd_internal(cctx, dst, dstCapacity, src, srcSize)
        .unwrap_or_else(|err| err.to_error_code())
}

pub(crate) unsafe fn ZSTD_compressEnd_internal(
    cctx: *mut ZSTD_CCtx,
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> Result<usize, Error> {
    let cSize = ZSTD_compressContinue_internal(cctx, dst, dstCapacity, src, srcSize, true, true)?;
    let endResult = ZSTD_writeEpilogue(
        cctx,
        (dst as *mut core::ffi::c_char).add(cSize) as *mut core::ffi::c_void,
        dstCapacity.wrapping_sub(cSize),
    )?;
    // control src size
    if (*cctx).pledgedSrcSizePlusOne != 0
        && (*cctx).pledgedSrcSizePlusOne != ((*cctx).consumedSrcSize).wrapping_add(1)
    {
        return Err(Error::srcSize_wrong);
    }
    ZSTD_CCtx_trace(cctx, endResult);
    Ok(cSize.wrapping_add(endResult))
}

#[deprecated(
    since = "0.0.0+zstd.1.5.5",
    note = "The buffer-less API is deprecated in favor of the normal streaming API. See docs."
)]
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_compressEnd))]
pub unsafe extern "C" fn ZSTD_compressEnd(
    cctx: *mut ZSTD_CCtx,
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    ZSTD_compressEnd_public(cctx, dst, dstCapacity, src, srcSize)
}

#[deprecated(
    since = "0.0.0+zstd.1.5.0",
    note = "It can be replaced by [`ZSTD_compress2`], in combination with [`ZSTD_CCtx_setParameter`] and other parameter setters."
)]
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_compress_advanced))]
pub unsafe extern "C" fn ZSTD_compress_advanced(
    cctx: *mut ZSTD_CCtx,
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
    dict: *const core::ffi::c_void,
    dictSize: usize,
    params: ZSTD_parameters,
) -> usize {
    let err_code = ZSTD_checkCParams(params.cParams);
    if ERR_isError(err_code) {
        return err_code;
    }
    (*cctx).simpleApiParams = ZSTD_CCtx_params_s::new_internal(&params, ZSTD_NO_CLEVEL);
    ZSTD_compress_advanced_internal(
        cctx,
        dst,
        dstCapacity,
        src,
        srcSize,
        dict,
        dictSize,
        &(*cctx).simpleApiParams,
    )
}

pub unsafe fn ZSTD_compress_advanced_internal(
    cctx: *mut ZSTD_CCtx,
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
    dict: *const core::ffi::c_void,
    dictSize: usize,
    params: &ZSTD_CCtx_params,
) -> usize {
    if let Err(err) = ZSTD_compressBegin_internal(
        cctx,
        dict,
        dictSize,
        ZSTD_dct_auto,
        DictTableLoadMethod::Fast,
        core::ptr::null::<ZSTD_CDict>(),
        params,
        srcSize as u64,
        BufferedPolicy::NotBuffered,
    ) {
        return err.to_error_code();
    }
    ZSTD_compressEnd_public(cctx, dst, dstCapacity, src, srcSize)
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_compress_usingDict))]
pub unsafe extern "C" fn ZSTD_compress_usingDict(
    cctx: *mut ZSTD_CCtx,
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
    dict: *const core::ffi::c_void,
    dictSize: usize,
    compressionLevel: core::ffi::c_int,
) -> usize {
    let params = ZSTD_parameters::new(
        compressionLevel,
        srcSize as core::ffi::c_ulonglong,
        if !dict.is_null() { dictSize } else { 0 },
        CParamMode::NoAttachDict,
    );
    (*cctx).simpleApiParams = ZSTD_CCtx_params_s::new_internal(
        &params,
        if compressionLevel == 0 {
            ZSTD_CLEVEL_DEFAULT
        } else {
            compressionLevel
        },
    );

    ZSTD_compress_advanced_internal(
        cctx,
        dst,
        dstCapacity,
        src,
        srcSize,
        dict,
        dictSize,
        &(*cctx).simpleApiParams,
    )
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_compressCCtx))]
pub unsafe extern "C" fn ZSTD_compressCCtx(
    cctx: *mut ZSTD_CCtx,
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
    compressionLevel: core::ffi::c_int,
) -> usize {
    ZSTD_compress_usingDict(
        cctx,
        dst,
        dstCapacity,
        src,
        srcSize,
        core::ptr::null(),
        0,
        compressionLevel,
    )
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_compress))]
pub unsafe extern "C" fn ZSTD_compress(
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
    compressionLevel: core::ffi::c_int,
) -> usize {
    let mut ctxBody = ZSTD_CCtx_s::default();
    ZSTD_initCCtx(&mut ctxBody, ZSTD_customMem::default());
    let result = ZSTD_compressCCtx(
        &mut ctxBody,
        dst,
        dstCapacity,
        src,
        srcSize,
        compressionLevel,
    );
    // can't free ctxBody itself, as it's on stack; free only heap content
    ZSTD_freeCCtxContent(&mut ctxBody);

    result
}

/// Estimate amount of memory that will be needed to create a dictionary with following arguments
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_estimateCDictSize_advanced))]
pub extern "C" fn ZSTD_estimateCDictSize_advanced(
    dictSize: usize,
    cParams: ZSTD_compressionParameters,
    dictLoadMethod: ZSTD_dictLoadMethod_e,
) -> usize {
    (ZSTD_cwksp_alloc_size(size_of::<ZSTD_CDict>()))
        .wrapping_add(ZSTD_cwksp_alloc_size(HUF_WORKSPACE_SIZE))
        // enableDedicatedDictSearch == 1 ensures that CDict estimation will not be too small
        // in case we are using DDS with row-hash.
        .wrapping_add(ZSTD_sizeof_matchState(
            &cParams,
            ZSTD_resolveRowMatchFinderMode(ParamSwitch::Auto, &cParams),
            true,
            false,
        ))
        .wrapping_add(if dictLoadMethod == ZSTD_dlm_byRef {
            0
        } else {
            ZSTD_cwksp_alloc_size(ZSTD_cwksp_align(
                dictSize,
                size_of::<*mut core::ffi::c_void>(),
            ))
        })
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_estimateCDictSize))]
pub extern "C" fn ZSTD_estimateCDictSize(
    dictSize: usize,
    compressionLevel: core::ffi::c_int,
) -> usize {
    let cParams = ZSTD_compressionParameters::new(
        compressionLevel,
        ZSTD_CONTENTSIZE_UNKNOWN,
        dictSize,
        CParamMode::CreateCDict,
    );
    ZSTD_estimateCDictSize_advanced(dictSize, cParams, ZSTD_dlm_byCopy)
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_sizeof_CDict))]
pub unsafe extern "C" fn ZSTD_sizeof_CDict(cdict: *const ZSTD_CDict) -> usize {
    if cdict.is_null() {
        return 0;
    }

    // cdict may be in the workspace
    (if (*cdict).workspace.workspace == cdict as *mut core::ffi::c_void {
        0
    } else {
        size_of::<ZSTD_CDict>()
    })
    .wrapping_add(ZSTD_cwksp_sizeof(&(*cdict).workspace))
}

unsafe fn ZSTD_initCDict_internal(
    cdict: *mut ZSTD_CDict,
    dictBuffer: *const core::ffi::c_void,
    dictSize: usize,
    dictLoadMethod: ZSTD_dictLoadMethod_e,
    dictContentType: ZSTD_dictContentType_e,
    mut params: ZSTD_CCtx_params,
) -> Result<(), Error> {
    (*cdict).matchState.cParams = params.cParams;
    (*cdict).matchState.dedicatedDictSearch = params.enableDedicatedDictSearch;
    if dictLoadMethod == ZSTD_dlm_byRef || dictBuffer.is_null() || dictSize == 0 {
        (*cdict).dictContent = dictBuffer;
    } else {
        let internalBuffer = ZSTD_cwksp_reserve_object(
            &mut (*cdict).workspace,
            ZSTD_cwksp_align(dictSize, size_of::<*mut core::ffi::c_void>()),
        );
        if internalBuffer.is_null() {
            return Err(Error::memory_allocation);
        }
        (*cdict).dictContent = internalBuffer;
        core::ptr::copy_nonoverlapping(
            dictBuffer.cast::<u8>(),
            internalBuffer.cast::<u8>(),
            dictSize,
        );
    }
    (*cdict).dictContentSize = dictSize;
    (*cdict).dictContentType = dictContentType;

    (*cdict).entropyWorkspace =
        ZSTD_cwksp_reserve_object(&mut (*cdict).workspace, HUF_WORKSPACE_SIZE) as *mut u32;

    // Reset the state to no dictionary
    ZSTD_reset_compressedBlockState(&mut (*cdict).cBlockState);
    ZSTD_reset_matchState(
        &mut (*cdict).matchState,
        &mut (*cdict).workspace,
        &params.cParams,
        params.useRowMatchFinder,
        CompResetPolicy::MakeClean,
        IndexResetPolicy::Reset,
        ResetTarget::CDict,
    )?;

    //(Maybe) load the dictionary
    // Skips loading the dictionary if it is < 8 bytes.
    params.compressionLevel = ZSTD_CLEVEL_DEFAULT;
    params.fParams.contentSizeFlag = 1;
    let dictID = ZSTD_compress_insertDictionary(
        &mut (*cdict).cBlockState,
        &mut (*cdict).matchState,
        None,
        &mut (*cdict).workspace,
        &params,
        (*cdict).dictContent,
        (*cdict).dictContentSize,
        dictContentType,
        DictTableLoadMethod::Full,
        TableFillPurpose::ForCDict,
        (*cdict).entropyWorkspace as *mut core::ffi::c_void,
    )?;
    (*cdict).dictID = dictID as u32;

    Ok(())
}

unsafe fn ZSTD_createCDict_advanced_internal(
    dictSize: usize,
    dictLoadMethod: ZSTD_dictLoadMethod_e,
    cParams: ZSTD_compressionParameters,
    useRowMatchFinder: ParamSwitch,
    enableDedicatedDictSearch: bool,
    customMem: ZSTD_customMem,
) -> *mut ZSTD_CDict {
    let workspaceSize = (ZSTD_cwksp_alloc_size(size_of::<ZSTD_CDict>()))
        .wrapping_add(ZSTD_cwksp_alloc_size(HUF_WORKSPACE_SIZE))
        .wrapping_add(ZSTD_sizeof_matchState(
            &cParams,
            useRowMatchFinder,
            enableDedicatedDictSearch,
            false,
        ))
        .wrapping_add(if dictLoadMethod == ZSTD_dlm_byRef {
            0
        } else {
            ZSTD_cwksp_alloc_size(ZSTD_cwksp_align(
                dictSize,
                size_of::<*mut core::ffi::c_void>(),
            ))
        });
    let workspace = ZSTD_customMalloc(workspaceSize, customMem);
    let mut ws = ZSTD_cwksp::default();

    if workspace.is_null() {
        return core::ptr::null_mut();
    }

    ZSTD_cwksp_init(&mut ws, workspace, workspaceSize, CwkspAllocKind::Dynamic);

    let cdict = ZSTD_cwksp_reserve_object(&mut ws, size_of::<ZSTD_CDict>()) as *mut ZSTD_CDict;
    ZSTD_cwksp_move(&mut (*cdict).workspace, &mut ws);
    (*cdict).customMem = customMem;
    (*cdict).compressionLevel = ZSTD_NO_CLEVEL; // signals advanced API usage
    (*cdict).useRowMatchFinder = useRowMatchFinder;
    cdict
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_createCDict_advanced))]
pub unsafe extern "C" fn ZSTD_createCDict_advanced(
    dictBuffer: *const core::ffi::c_void,
    dictSize: usize,
    dictLoadMethod: ZSTD_dictLoadMethod_e,
    dictContentType: ZSTD_dictContentType_e,
    cParams: ZSTD_compressionParameters,
    customMem: ZSTD_customMem,
) -> *mut ZSTD_CDict {
    let mut cctxParams = ZSTD_CCtx_params_s::new(0);
    cctxParams.cParams = cParams;
    cctxParams.customMem = customMem;
    ZSTD_createCDict_advanced2(
        dictBuffer,
        dictSize,
        dictLoadMethod,
        dictContentType,
        &cctxParams,
        customMem,
    )
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_createCDict_advanced2))]
pub unsafe extern "C" fn ZSTD_createCDict_advanced2(
    dict: *const core::ffi::c_void,
    dictSize: usize,
    dictLoadMethod: ZSTD_dictLoadMethod_e,
    dictContentType: ZSTD_dictContentType_e,
    originalCctxParams: *const ZSTD_CCtx_params,
    customMem: ZSTD_customMem,
) -> *mut ZSTD_CDict {
    let mut cctxParams = *originalCctxParams;

    let mut cParams = if cctxParams.enableDedicatedDictSearch != 0 {
        let mut cParams =
            ZSTD_compressionParameters::new_for_dds(cctxParams.compressionLevel, dictSize);
        cParams.override_cparams(&cctxParams.cParams);
        cParams
    } else {
        ZSTD_getCParamsFromCCtxParams_internal(
            &cctxParams,
            ZSTD_CONTENTSIZE_UNKNOWN,
            dictSize,
            CParamMode::CreateCDict,
        )
    };

    if !cParams.supports_dedicated_dict_search() {
        // Fall back to non-DDSS params
        cctxParams.enableDedicatedDictSearch = 0;
        cParams = ZSTD_getCParamsFromCCtxParams_internal(
            &cctxParams,
            ZSTD_CONTENTSIZE_UNKNOWN,
            dictSize,
            CParamMode::CreateCDict,
        );
    }

    cctxParams.cParams = cParams;
    cctxParams.useRowMatchFinder =
        ZSTD_resolveRowMatchFinderMode(cctxParams.useRowMatchFinder, &cParams);

    let cdict = ZSTD_createCDict_advanced_internal(
        dictSize,
        dictLoadMethod,
        cctxParams.cParams,
        cctxParams.useRowMatchFinder,
        cctxParams.enableDedicatedDictSearch != 0,
        customMem,
    );

    if cdict.is_null()
        || ZSTD_initCDict_internal(
            cdict,
            dict,
            dictSize,
            dictLoadMethod,
            dictContentType,
            cctxParams,
        )
        .is_err()
    {
        ZSTD_freeCDict(cdict);
        return core::ptr::null_mut();
    }

    cdict
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_createCDict))]
pub unsafe extern "C" fn ZSTD_createCDict(
    dict: *const core::ffi::c_void,
    dictSize: usize,
    compressionLevel: core::ffi::c_int,
) -> *mut ZSTD_CDict {
    let cParams = ZSTD_compressionParameters::new(
        compressionLevel,
        ZSTD_CONTENTSIZE_UNKNOWN,
        dictSize,
        CParamMode::CreateCDict,
    );
    let cdict = ZSTD_createCDict_advanced(
        dict,
        dictSize,
        ZSTD_dlm_byCopy,
        ZSTD_dct_auto,
        cParams,
        ZSTD_customMem::default(),
    );

    if !cdict.is_null() {
        (*cdict).compressionLevel = if compressionLevel == 0 {
            ZSTD_CLEVEL_DEFAULT
        } else {
            compressionLevel
        };
    }

    cdict
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_createCDict_byReference))]
pub unsafe extern "C" fn ZSTD_createCDict_byReference(
    dict: *const core::ffi::c_void,
    dictSize: usize,
    compressionLevel: core::ffi::c_int,
) -> *mut ZSTD_CDict {
    let cParams = ZSTD_compressionParameters::new(
        compressionLevel,
        ZSTD_CONTENTSIZE_UNKNOWN,
        dictSize,
        CParamMode::CreateCDict,
    );
    let cdict = ZSTD_createCDict_advanced(
        dict,
        dictSize,
        ZSTD_dlm_byRef,
        ZSTD_dct_auto,
        cParams,
        ZSTD_customMem::default(),
    );

    if !cdict.is_null() {
        (*cdict).compressionLevel = if compressionLevel == 0 {
            ZSTD_CLEVEL_DEFAULT
        } else {
            compressionLevel
        };
    }

    cdict
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_freeCDict))]
pub unsafe extern "C" fn ZSTD_freeCDict(cdict: *mut ZSTD_CDict) -> usize {
    if cdict.is_null() {
        return 0;
    }
    let cMem = (*cdict).customMem;
    let cdictInWorkspace =
        ZSTD_cwksp_owns_buffer(&(*cdict).workspace, cdict as *const core::ffi::c_void);
    ZSTD_cwksp_free(&mut (*cdict).workspace, cMem);
    if !cdictInWorkspace {
        ZSTD_customFree(
            cdict as *mut core::ffi::c_void,
            (*cdict).dictContentSize,
            cMem,
        );
    }
    0
}

/// Generate a digested dictionary in provided memory area.
/// workspace: The memory area to emplace the dictionary into.
///            Provided pointer must 8-bytes aligned.
///            It must outlive dictionary usage.
/// workspaceSize: Use ZSTD_estimateCDictSize() to determine how large workspace must be.
/// cParams: use ZSTD_getCParams() to transform a compression level into its relevant cParams.
///
/// # Returns
///
/// pointer to ZSTD_CDict*, or NULL if error (size too small).
///
/// Note: There is no corresponding "free" function.
/// Since workspace was allocated externally, it must be freed externally.
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_initStaticCDict))]
pub unsafe extern "C" fn ZSTD_initStaticCDict(
    workspace: *mut core::ffi::c_void,
    workspaceSize: usize,
    dict: *const core::ffi::c_void,
    dictSize: usize,
    dictLoadMethod: ZSTD_dictLoadMethod_e,
    dictContentType: ZSTD_dictContentType_e,
    cParams: ZSTD_compressionParameters,
) -> *const ZSTD_CDict {
    let useRowMatchFinder = ZSTD_resolveRowMatchFinderMode(ParamSwitch::Auto, &cParams);
    // enableDedicatedDictSearch == 1 ensures matchstate is not too small in case this CDict will be used for DDS + row hash
    let matchStateSize = ZSTD_sizeof_matchState(&cParams, useRowMatchFinder, true, false);
    let neededSize = (ZSTD_cwksp_alloc_size(size_of::<ZSTD_CDict>()))
        .wrapping_add(if dictLoadMethod == ZSTD_dlm_byRef {
            0
        } else {
            ZSTD_cwksp_alloc_size(ZSTD_cwksp_align(
                dictSize,
                size_of::<*mut core::ffi::c_void>(),
            ))
        })
        .wrapping_add(ZSTD_cwksp_alloc_size(HUF_WORKSPACE_SIZE))
        .wrapping_add(matchStateSize);

    // 8-aligned
    if workspace as usize & 7 != 0 {
        return core::ptr::null();
    }

    let mut ws = ZSTD_cwksp::default();
    ZSTD_cwksp_init(&mut ws, workspace, workspaceSize, CwkspAllocKind::Static);
    let cdict = ZSTD_cwksp_reserve_object(&mut ws, size_of::<ZSTD_CDict>()) as *mut ZSTD_CDict;
    if cdict.is_null() {
        return core::ptr::null();
    }
    ZSTD_cwksp_move(&mut (*cdict).workspace, &mut ws);

    if workspaceSize < neededSize {
        return core::ptr::null();
    }

    let mut params = ZSTD_CCtx_params_s::new(0);
    params.cParams = cParams;
    params.useRowMatchFinder = useRowMatchFinder;
    (*cdict).useRowMatchFinder = useRowMatchFinder;
    (*cdict).compressionLevel = ZSTD_NO_CLEVEL;

    if ZSTD_initCDict_internal(
        cdict,
        dict,
        dictSize,
        dictLoadMethod,
        dictContentType,
        params,
    )
    .is_err()
    {
        return core::ptr::null();
    }

    cdict
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_getCParamsFromCDict))]
pub unsafe extern "C" fn ZSTD_getCParamsFromCDict(
    cdict: *const ZSTD_CDict,
) -> ZSTD_compressionParameters {
    (*cdict).matchState.cParams
}

/// Provides the dictID of the dictionary loaded into `cdict`.
/// Non-conformant dictionaries can still be loaded, but as content-only dictionaries.
///
/// # Returns
///
/// dictID, or 0 if the dictionary is not conformant to Zstandard specification, or empty.
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_getDictID_fromCDict))]
pub unsafe extern "C" fn ZSTD_getDictID_fromCDict(cdict: *const ZSTD_CDict) -> core::ffi::c_uint {
    if cdict.is_null() {
        return 0;
    }
    (*cdict).dictID
}

/// Implementation of various ZSTD_compressBegin_usingCDict* functions.
unsafe fn ZSTD_compressBegin_usingCDict_internal(
    cctx: *mut ZSTD_CCtx,
    cdict: *const ZSTD_CDict,
    fParams: ZSTD_frameParameters,
    pledgedSrcSize: core::ffi::c_ulonglong,
) -> Result<(), Error> {
    if cdict.is_null() {
        return Err(Error::dictionary_wrong);
    }

    // Initialize the cctxParams from the cdict
    let useCDictParams = pledgedSrcSize
        < ZSTD_USE_CDICT_PARAMS_SRCSIZE_CUTOFF as core::ffi::c_ulonglong
        || pledgedSrcSize
            < ((*cdict).dictContentSize as core::ffi::c_ulonglong)
                .wrapping_mul(ZSTD_USE_CDICT_PARAMS_DICTSIZE_MULTIPLIER)
        || pledgedSrcSize == ZSTD_CONTENTSIZE_UNKNOWN
        || (*cdict).compressionLevel == 0;
    let params = ZSTD_parameters {
        cParams: if useCDictParams {
            ZSTD_getCParamsFromCDict(cdict)
        } else {
            ZSTD_getCParams(
                (*cdict).compressionLevel,
                pledgedSrcSize,
                (*cdict).dictContentSize,
            )
        },
        fParams,
    };
    let mut cctxParams = ZSTD_CCtx_params_s::new_internal(&params, (*cdict).compressionLevel);

    // Increase window log to fit the entire dictionary and source if the
    // source size is known. Limit the increase to 19, which is the
    // window log for compression level 1 with the largest source size.
    if pledgedSrcSize != ZSTD_CONTENTSIZE_UNKNOWN {
        let limitedSrcSize = pledgedSrcSize.min(1 << 19) as u32;
        let limitedSrcLog = if limitedSrcSize > 1 {
            (ZSTD_highbit32(limitedSrcSize.wrapping_sub(1))).wrapping_add(1)
        } else {
            1
        };
        cctxParams.cParams.windowLog = cctxParams.cParams.windowLog.max(limitedSrcLog);
    }

    ZSTD_compressBegin_internal(
        cctx,
        core::ptr::null(),
        0,
        ZSTD_dct_auto,
        DictTableLoadMethod::Fast,
        cdict,
        &cctxParams,
        pledgedSrcSize,
        BufferedPolicy::NotBuffered,
    )
}

/// This function is DEPRECATED.
/// cdict must be != NULL
#[deprecated(
    since = "0.0.0+zstd.1.5.0",
    note = "Use the advanced API to access custom parameters."
)]
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_compressBegin_usingCDict_advanced))]
pub unsafe extern "C" fn ZSTD_compressBegin_usingCDict_advanced(
    cctx: *mut ZSTD_CCtx,
    cdict: *const ZSTD_CDict,
    fParams: ZSTD_frameParameters,
    pledgedSrcSize: core::ffi::c_ulonglong,
) -> usize {
    match ZSTD_compressBegin_usingCDict_internal(cctx, cdict, fParams, pledgedSrcSize) {
        Ok(()) => 0,
        Err(err) => err.to_error_code(),
    }
}

/// cdict must be != NULL
#[deprecated(note = "Deprecated definitions that are still used internally.")]
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_compressBegin_usingCDict_deprecated))]
pub unsafe extern "C" fn ZSTD_compressBegin_usingCDict_deprecated(
    cctx: *mut ZSTD_CCtx,
    cdict: *const ZSTD_CDict,
) -> usize {
    let fParams = {
        ZSTD_frameParameters {
            contentSizeFlag: 0,
            checksumFlag: 0,
            noDictIDFlag: 0,
        }
    };
    match ZSTD_compressBegin_usingCDict_internal(cctx, cdict, fParams, ZSTD_CONTENTSIZE_UNKNOWN) {
        Ok(()) => 0,
        Err(err) => err.to_error_code(),
    }
}

#[deprecated(
    since = "0.0.0+zstd.1.5.5",
    note = "The buffer-less API is deprecated in favor of the normal streaming API. See docs."
)]
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_compressBegin_usingCDict))]
pub unsafe extern "C" fn ZSTD_compressBegin_usingCDict(
    cctx: *mut ZSTD_CCtx,
    cdict: *const ZSTD_CDict,
) -> usize {
    #[allow(deprecated)]
    ZSTD_compressBegin_usingCDict_deprecated(cctx, cdict)
}

/// Implementation of various ZSTD_compress_usingCDict* functions.
unsafe fn ZSTD_compress_usingCDict_internal(
    cctx: *mut ZSTD_CCtx,
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
    cdict: *const ZSTD_CDict,
    fParams: ZSTD_frameParameters,
) -> Result<usize, Error> {
    ZSTD_compressBegin_usingCDict_internal(
        cctx,
        cdict,
        fParams,
        srcSize as core::ffi::c_ulonglong,
    )?;
    ZSTD_compressEnd_internal(cctx, dst, dstCapacity, src, srcSize)
}

/// This function is DEPRECATED.
#[deprecated(
    since = "0.0.0+zstd.1.5.0",
    note = "It can be replaced by [` ZSTD_compress2`], in combination with [`ZSTD_CCtx_loadDictionary`] and other parameter setters."
)]
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_compress_usingCDict_advanced))]
pub unsafe extern "C" fn ZSTD_compress_usingCDict_advanced(
    cctx: *mut ZSTD_CCtx,
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
    cdict: *const ZSTD_CDict,
    fParams: ZSTD_frameParameters,
) -> usize {
    ZSTD_compress_usingCDict_internal(cctx, dst, dstCapacity, src, srcSize, cdict, fParams)
        .unwrap_or_else(|err| err.to_error_code())
}

/// Compression using a digested Dictionary.
/// Faster startup than ZSTD_compress_usingDict(), recommended when same dictionary is used multiple times.
/// Note that compression parameters are decided at CDict creation time
/// while frame parameters are hardcoded.
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_compress_usingCDict))]
pub unsafe extern "C" fn ZSTD_compress_usingCDict(
    cctx: *mut ZSTD_CCtx,
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
    cdict: *const ZSTD_CDict,
) -> usize {
    let fParams = {
        ZSTD_frameParameters {
            contentSizeFlag: 1,
            checksumFlag: 0,
            noDictIDFlag: 0,
        }
    };
    ZSTD_compress_usingCDict_internal(cctx, dst, dstCapacity, src, srcSize, cdict, fParams)
        .unwrap_or_else(|err| err.to_error_code())
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_createCStream))]
pub unsafe extern "C" fn ZSTD_createCStream() -> *mut ZSTD_CStream {
    ZSTD_createCStream_advanced(ZSTD_customMem::default())
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_initStaticCStream))]
pub unsafe extern "C" fn ZSTD_initStaticCStream(
    workspace: *mut core::ffi::c_void,
    workspaceSize: usize,
) -> *mut ZSTD_CStream {
    ZSTD_initStaticCCtx(workspace, workspaceSize)
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_createCStream_advanced))]
pub unsafe extern "C" fn ZSTD_createCStream_advanced(
    customMem: ZSTD_customMem,
) -> *mut ZSTD_CStream {
    ZSTD_createCCtx_advanced(customMem)
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_freeCStream))]
pub unsafe extern "C" fn ZSTD_freeCStream(zcs: *mut ZSTD_CStream) -> usize {
    ZSTD_freeCCtx(zcs)
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_CStreamInSize))]
pub extern "C" fn ZSTD_CStreamInSize() -> usize {
    ZSTD_BLOCKSIZE_MAX as usize
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_CStreamOutSize))]
pub extern "C" fn ZSTD_CStreamOutSize() -> usize {
    ZSTD_compressBound(ZSTD_BLOCKSIZE_MAX as usize)
        .wrapping_add(ZSTD_BLOCKHEADERSIZE)
        .wrapping_add(4)
}

unsafe fn ZSTD_getCParamMode(
    cdict: *const ZSTD_CDict,
    params: &ZSTD_CCtx_params,
    pledgedSrcSize: u64,
) -> CParamMode {
    if !cdict.is_null() && ZSTD_shouldAttachDict(cdict, params, pledgedSrcSize) {
        CParamMode::AttachDict
    } else {
        CParamMode::NoAttachDict
    }
}

/// pledgedSrcSize == 0 means "unknown"
#[deprecated(
    since = "0.0.0+zstd.1.5.0",
    note = "Use [`ZSTD_CCtx_reset`], see zstd.h for detailed instructions"
)]
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_resetCStream))]
pub unsafe extern "C" fn ZSTD_resetCStream(
    zcs: *mut ZSTD_CStream,
    pss: core::ffi::c_ulonglong,
) -> usize {
    // temporary: 0 interpreted as "unknown" during transition period.
    // Users willing to specify "unknown" **must** use ZSTD_CONTENTSIZE_UNKNOWN.
    // 0 will be interpreted as "empty" in the future.
    let pledgedSrcSize = if pss == 0 {
        ZSTD_CONTENTSIZE_UNKNOWN
    } else {
        pss
    };

    let err_code = ZSTD_CCtx_reset(zcs, ZSTD_ResetDirective::ZSTD_reset_session_only);
    if ERR_isError(err_code) {
        return err_code;
    }
    let err_code_0 = ZSTD_CCtx_setPledgedSrcSize(zcs, pledgedSrcSize as core::ffi::c_ulonglong);
    if ERR_isError(err_code_0) {
        return err_code_0;
    }

    0
}

/// For lib/compress only. Used by zstdmt_compress.c.
/// Assumption 1: params are valid.
/// Assumption 2: either dict, or cdict, is defined, not both.
pub unsafe fn ZSTD_initCStream_internal(
    zcs: *mut ZSTD_CStream,
    dict: *const core::ffi::c_void,
    dictSize: usize,
    cdict: *const ZSTD_CDict,
    params: *const ZSTD_CCtx_params,
    pledgedSrcSize: core::ffi::c_ulonglong,
) -> usize {
    let err_code = ZSTD_CCtx_reset(zcs, ZSTD_ResetDirective::ZSTD_reset_session_only);
    if ERR_isError(err_code) {
        return err_code;
    }
    let err_code_0 = ZSTD_CCtx_setPledgedSrcSize(zcs, pledgedSrcSize);
    if ERR_isError(err_code_0) {
        return err_code_0;
    }
    (*zcs).requestedParams = *params;

    if !dict.is_null() {
        let err_code_1 = ZSTD_CCtx_loadDictionary(zcs, dict, dictSize);
        if ERR_isError(err_code_1) {
            return err_code_1;
        }
    } else {
        let err_code_2 = ZSTD_CCtx_refCDict(zcs, cdict);
        if ERR_isError(err_code_2) {
            return err_code_2;
        }
    }

    0
}

/// same as ZSTD_initCStream_usingCDict(), with control over frame parameters
#[deprecated(
    since = "0.0.0+zstd.1.5.0",
    note = "Use [`ZSTD_CCtx_reset`] and [`ZSTD_CCtx_refCDict`], see zstd.h for detailed instructions"
)]
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_initCStream_usingCDict_advanced))]
pub unsafe extern "C" fn ZSTD_initCStream_usingCDict_advanced(
    zcs: *mut ZSTD_CStream,
    cdict: *const ZSTD_CDict,
    fParams: ZSTD_frameParameters,
    pledgedSrcSize: core::ffi::c_ulonglong,
) -> usize {
    let err_code = ZSTD_CCtx_reset(zcs, ZSTD_ResetDirective::ZSTD_reset_session_only);
    if ERR_isError(err_code) {
        return err_code;
    }
    let err_code_0 = ZSTD_CCtx_setPledgedSrcSize(zcs, pledgedSrcSize);
    if ERR_isError(err_code_0) {
        return err_code_0;
    }
    (*zcs).requestedParams.fParams = fParams;
    let err_code_1 = ZSTD_CCtx_refCDict(zcs, cdict);
    if ERR_isError(err_code_1) {
        return err_code_1;
    }

    0
}

/// cdict must outlive compression session
#[deprecated(
    note = "Use [`ZSTD_CCtx_reset`] and [`ZSTD_CCtx_refCDict`], see zstd.h for detailed instructions"
)]
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_initCStream_usingCDict))]
pub unsafe extern "C" fn ZSTD_initCStream_usingCDict(
    zcs: *mut ZSTD_CStream,
    cdict: *const ZSTD_CDict,
) -> usize {
    let err_code = ZSTD_CCtx_reset(zcs, ZSTD_ResetDirective::ZSTD_reset_session_only);
    if ERR_isError(err_code) {
        return err_code;
    }
    let err_code_0 = ZSTD_CCtx_refCDict(zcs, cdict);
    if ERR_isError(err_code_0) {
        return err_code_0;
    }
    0
}

/// pledgedSrcSize must be exact.
/// if srcSize is not known at init time, use value ZSTD_CONTENTSIZE_UNKNOWN.
/// dict is loaded with default parameters ZSTD_dct_auto and ZSTD_dlm_byCopy.
#[deprecated(
    since = "0.0.0+zstd.1.5.0",
    note = "Use [`ZSTD_CCtx_reset`], see zstd.h for detailed instructions"
)]
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_initCStream_advanced))]
pub unsafe extern "C" fn ZSTD_initCStream_advanced(
    zcs: *mut ZSTD_CStream,
    dict: *const core::ffi::c_void,
    dictSize: usize,
    params: ZSTD_parameters,
    pss: core::ffi::c_ulonglong,
) -> usize {
    // for compatibility with older programs relying on this behavior.
    // Users should now specify ZSTD_CONTENTSIZE_UNKNOWN.
    // This line will be removed in the future.
    let pledgedSrcSize = if pss == 0 && params.fParams.contentSizeFlag == 0 {
        ZSTD_CONTENTSIZE_UNKNOWN
    } else {
        pss
    };
    let err_code = ZSTD_CCtx_reset(zcs, ZSTD_ResetDirective::ZSTD_reset_session_only);
    if ERR_isError(err_code) {
        return err_code;
    }
    let err_code_0 = ZSTD_CCtx_setPledgedSrcSize(zcs, pledgedSrcSize as core::ffi::c_ulonglong);
    if ERR_isError(err_code_0) {
        return err_code_0;
    }
    let err_code_1 = ZSTD_checkCParams(params.cParams);
    if ERR_isError(err_code_1) {
        return err_code_1;
    }
    (*zcs).requestedParams.set_zstd_params(&params);
    let err_code_2 = ZSTD_CCtx_loadDictionary(zcs, dict, dictSize);
    if ERR_isError(err_code_2) {
        return err_code_2;
    }

    0
}

#[deprecated(
    since = "0.0.0+zstd.1.5.0",
    note = "Use [`ZSTD_CCtx_reset`], see zstd.h for detailed instructions"
)]
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_initCStream_usingDict))]
pub unsafe extern "C" fn ZSTD_initCStream_usingDict(
    zcs: *mut ZSTD_CStream,
    dict: *const core::ffi::c_void,
    dictSize: usize,
    compressionLevel: core::ffi::c_int,
) -> usize {
    let err_code = ZSTD_CCtx_reset(zcs, ZSTD_ResetDirective::ZSTD_reset_session_only);
    if ERR_isError(err_code) {
        return err_code;
    }
    let err_code_0 = ZSTD_CCtx_setParameter(
        zcs,
        ZSTD_cParameter::ZSTD_c_compressionLevel,
        compressionLevel,
    );
    if ERR_isError(err_code_0) {
        return err_code_0;
    }
    let err_code_1 = ZSTD_CCtx_loadDictionary(zcs, dict, dictSize);
    if ERR_isError(err_code_1) {
        return err_code_1;
    }

    0
}

#[deprecated(
    since = "0.0.0+zstd.1.5.0",
    note = "Use [`ZSTD_CCtx_reset`], see zstd.h for detailed instructions"
)]
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_initCStream_srcSize))]
pub unsafe extern "C" fn ZSTD_initCStream_srcSize(
    zcs: *mut ZSTD_CStream,
    compressionLevel: core::ffi::c_int,
    pss: core::ffi::c_ulonglong,
) -> usize {
    // temporary : 0 interpreted as "unknown" during transition period.
    // Users willing to specify "unknown" **must** use ZSTD_CONTENTSIZE_UNKNOWN.
    // 0 will be interpreted as "empty" in the future.
    let pledgedSrcSize = if pss == 0 {
        ZSTD_CONTENTSIZE_UNKNOWN
    } else {
        pss
    };

    let err_code = ZSTD_CCtx_reset(zcs, ZSTD_ResetDirective::ZSTD_reset_session_only);
    if ERR_isError(err_code) {
        return err_code;
    }
    let err_code_0 = ZSTD_CCtx_refCDict(zcs, core::ptr::null::<ZSTD_CDict>());
    if ERR_isError(err_code_0) {
        return err_code_0;
    }
    let err_code_1 = ZSTD_CCtx_setParameter(
        zcs,
        ZSTD_cParameter::ZSTD_c_compressionLevel,
        compressionLevel,
    );
    if ERR_isError(err_code_1) {
        return err_code_1;
    }
    let err_code_2 = ZSTD_CCtx_setPledgedSrcSize(zcs, pledgedSrcSize as core::ffi::c_ulonglong);
    if ERR_isError(err_code_2) {
        return err_code_2;
    }

    0
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_initCStream))]
pub unsafe extern "C" fn ZSTD_initCStream(
    zcs: *mut ZSTD_CStream,
    compressionLevel: core::ffi::c_int,
) -> usize {
    let err_code = ZSTD_CCtx_reset(zcs, ZSTD_ResetDirective::ZSTD_reset_session_only);
    if ERR_isError(err_code) {
        return err_code;
    }
    let err_code_0 = ZSTD_CCtx_refCDict(zcs, core::ptr::null::<ZSTD_CDict>());
    if ERR_isError(err_code_0) {
        return err_code_0;
    }
    let err_code_1 = ZSTD_CCtx_setParameter(
        zcs,
        ZSTD_cParameter::ZSTD_c_compressionLevel,
        compressionLevel,
    );
    if ERR_isError(err_code_1) {
        return err_code_1;
    }

    0
}

unsafe fn ZSTD_nextInputSizeHint(cctx: *const ZSTD_CCtx) -> usize {
    if (*cctx).appliedParams.inBufferMode == ZSTD_bm_stable {
        return ((*cctx).blockSizeMax).wrapping_sub((*cctx).stableIn_notConsumed);
    }
    let mut hintInSize = ((*cctx).inBuffTarget).wrapping_sub((*cctx).inBuffPos);
    if hintInSize == 0 {
        hintInSize = (*cctx).blockSizeMax;
    }
    hintInSize
}

/// Internal function for all *compressStream*() variants.
///
/// # Returns
///
/// The hint size for next input to complete ongoing block
unsafe fn ZSTD_compressStream_generic(
    zcs: *mut ZSTD_CStream,
    output: *mut ZSTD_outBuffer,
    input: *mut ZSTD_inBuffer,
    flushMode: ZSTD_EndDirective,
) -> Result<usize, Error> {
    let istart = (*input).src as *const u8;
    let iend = if !istart.is_null() {
        istart.add((*input).size)
    } else {
        istart
    };
    let mut ip = if !istart.is_null() {
        istart.add((*input).pos)
    } else {
        istart
    };
    let ostart = (*output).dst as *mut u8;
    let oend = if !ostart.is_null() {
        ostart.add((*output).size)
    } else {
        ostart
    };
    let mut op = if !ostart.is_null() {
        ostart.add((*output).pos)
    } else {
        ostart
    };

    if (*zcs).appliedParams.inBufferMode == ZSTD_bm_stable {
        (*input).pos = ((*input).pos).wrapping_sub((*zcs).stableIn_notConsumed);
        if !ip.is_null() {
            ip = ip.sub((*zcs).stableIn_notConsumed);
        }
        (*zcs).stableIn_notConsumed = 0;
    }
    if (*zcs).appliedParams.inBufferMode == ZSTD_bm_buffered {
        assert!(!(*zcs).inBuff.is_null());
        assert!((*zcs).inBuffSize > 0);
    }
    if (*zcs).appliedParams.outBufferMode == ZSTD_bm_buffered {
        assert!(!(*zcs).outBuff.is_null());
        assert!((*zcs).outBuffSize > 0);
    }
    ((*input).src).is_null();
    ((*output).dst).is_null();

    loop {
        match (*zcs).streamStage {
            StreamStage::Init => return Err(Error::init_missing),
            StreamStage::Load => {
                if flushMode == ZSTD_e_end
                    && (oend.offset_from_unsigned(op)
                        >= ZSTD_compressBound(iend.offset_from_unsigned(ip))
                        || (*zcs).appliedParams.outBufferMode == ZSTD_bm_stable)
                    && (*zcs).inBuffPos == 0
                {
                    // shortcut to compression pass directly into output buffer
                    let cSize = ZSTD_compressEnd_internal(
                        zcs,
                        op as *mut core::ffi::c_void,
                        oend.offset_from_unsigned(op),
                        ip as *const core::ffi::c_void,
                        iend.offset_from_unsigned(ip),
                    )?;
                    ip = iend;
                    op = op.add(cSize);
                    (*zcs).frameEnded = 1;
                    ZSTD_CCtx_reset(zcs, ZSTD_ResetDirective::ZSTD_reset_session_only);
                    break;
                }
                if (*zcs).appliedParams.inBufferMode == ZSTD_bm_buffered {
                    let toLoad = ((*zcs).inBuffTarget).wrapping_sub((*zcs).inBuffPos);
                    let loaded = ZSTD_limitCopy(
                        ((*zcs).inBuff).add((*zcs).inBuffPos),
                        toLoad,
                        ip,
                        iend.offset_from_unsigned(ip),
                    );
                    (*zcs).inBuffPos = ((*zcs).inBuffPos).wrapping_add(loaded);
                    if !ip.is_null() {
                        ip = ip.add(loaded);
                    }
                    if flushMode == ZSTD_e_continue && (*zcs).inBuffPos < (*zcs).inBuffTarget
                        || flushMode == ZSTD_e_flush && (*zcs).inBuffPos == (*zcs).inToCompress
                    {
                        break;
                    }
                } else if flushMode == ZSTD_e_continue
                    && iend.offset_from_unsigned(ip) < (*zcs).blockSizeMax
                {
                    (*zcs).stableIn_notConsumed = iend.offset_from_unsigned(ip);
                    ip = iend;
                    break;
                } else if flushMode == ZSTD_e_flush && ip == iend {
                    break;
                }
                let inputBuffered = (*zcs).appliedParams.inBufferMode == ZSTD_bm_buffered;
                let mut oSize = oend.offset_from_unsigned(op);
                let iSize = if inputBuffered {
                    ((*zcs).inBuffPos).wrapping_sub((*zcs).inToCompress)
                } else if iend.offset_from_unsigned(ip) < (*zcs).blockSizeMax {
                    iend.offset_from_unsigned(ip)
                } else {
                    (*zcs).blockSizeMax
                };
                let cDst = if oSize >= ZSTD_compressBound(iSize)
                    || (*zcs).appliedParams.outBufferMode == ZSTD_bm_stable
                {
                    op as *mut core::ffi::c_void // compress into output buffer, to skip flush stage
                } else {
                    oSize = (*zcs).outBuffSize;
                    (*zcs).outBuff as *mut core::ffi::c_void
                };
                let cSize = if inputBuffered {
                    let lastBlock = flushMode == ZSTD_e_end && ip == iend;
                    let cSize = if lastBlock {
                        ZSTD_compressEnd_internal(
                            zcs,
                            cDst,
                            oSize,
                            ((*zcs).inBuff).add((*zcs).inToCompress) as *const core::ffi::c_void,
                            iSize,
                        )?
                    } else {
                        ZSTD_compressContinue_internal(
                            zcs,
                            cDst,
                            oSize,
                            ((*zcs).inBuff).add((*zcs).inToCompress) as *const core::ffi::c_void,
                            iSize,
                            true,
                            false,
                        )?
                    };
                    (*zcs).frameEnded = u32::from(lastBlock);
                    (*zcs).inBuffTarget = ((*zcs).inBuffPos).wrapping_add((*zcs).blockSizeMax);
                    if (*zcs).inBuffTarget > (*zcs).inBuffSize {
                        (*zcs).inBuffPos = 0;
                        (*zcs).inBuffTarget = (*zcs).blockSizeMax;
                    }
                    if !lastBlock {
                        assert!((*zcs).inBuffTarget <= (*zcs).inBuffSize);
                    }
                    (*zcs).inToCompress = (*zcs).inBuffPos;
                    cSize
                } else {
                    let lastBlock = flushMode == ZSTD_e_end && ip.add(iSize) == iend;
                    let cSize = if lastBlock {
                        ZSTD_compressEnd_internal(
                            zcs,
                            cDst,
                            oSize,
                            ip as *const core::ffi::c_void,
                            iSize,
                        )?
                    } else {
                        ZSTD_compressContinue_internal(
                            zcs,
                            cDst,
                            oSize,
                            ip as *const core::ffi::c_void,
                            iSize,
                            true,
                            false,
                        )?
                    };
                    if !ip.is_null() {
                        ip = ip.add(iSize);
                    }
                    (*zcs).frameEnded = u32::from(lastBlock);
                    if lastBlock {
                        assert_eq!(ip, iend);
                    }
                    cSize
                };
                if cDst == op as *mut core::ffi::c_void {
                    op = op.add(cSize);
                    if (*zcs).frameEnded != 0 {
                        ZSTD_CCtx_reset(zcs, ZSTD_ResetDirective::ZSTD_reset_session_only);
                        break;
                    }
                } else {
                    (*zcs).outBuffContentSize = cSize;
                    (*zcs).outBuffFlushedSize = 0;
                    (*zcs).streamStage = StreamStage::Flush;
                }
            }
            StreamStage::Flush => {
                let toFlush = ((*zcs).outBuffContentSize).wrapping_sub((*zcs).outBuffFlushedSize);
                let flushed = ZSTD_limitCopy(
                    op,
                    oend.offset_from_unsigned(op),
                    ((*zcs).outBuff).add((*zcs).outBuffFlushedSize),
                    toFlush,
                );
                if flushed != 0 {
                    op = op.add(flushed);
                }
                (*zcs).outBuffFlushedSize = ((*zcs).outBuffFlushedSize).wrapping_add(flushed);
                if toFlush != flushed {
                    // flush not fully completed, presumably because dst is too small
                    break;
                }
                (*zcs).outBuffFlushedSize = 0;
                (*zcs).outBuffContentSize = 0;
                if (*zcs).frameEnded != 0 {
                    ZSTD_CCtx_reset(zcs, ZSTD_ResetDirective::ZSTD_reset_session_only);
                    break;
                }
                (*zcs).streamStage = StreamStage::Load;
            }
        }
    }
    (*input).pos = ip.offset_from_unsigned(istart);
    (*output).pos = op.offset_from_unsigned(ostart);
    if (*zcs).frameEnded != 0 {
        return Ok(0);
    }
    Ok(ZSTD_nextInputSizeHint(zcs))
}

unsafe fn ZSTD_nextInputSizeHint_MTorST(cctx: *const ZSTD_CCtx) -> usize {
    if (*cctx).appliedParams.nbWorkers >= 1 {
        return ZSTDMT_nextInputSizeHint((*cctx).mtctx);
    }
    ZSTD_nextInputSizeHint(cctx)
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_compressStream))]
pub unsafe extern "C" fn ZSTD_compressStream(
    zcs: *mut ZSTD_CStream,
    output: *mut ZSTD_outBuffer,
    input: *mut ZSTD_inBuffer,
) -> usize {
    if let Err(err) = ZSTD_compressStream2_internal(zcs, output, input, ZSTD_e_continue) {
        return err.to_error_code();
    }
    ZSTD_nextInputSizeHint_MTorST(zcs)
}

/// After a compression call, set the expected input/output buffer.
/// This is validated at the start of the next compression call.
unsafe fn ZSTD_setBufferExpectations(
    cctx: *mut ZSTD_CCtx,
    output: *const ZSTD_outBuffer,
    input: *const ZSTD_inBuffer,
) {
    if (*cctx).appliedParams.inBufferMode == ZSTD_bm_stable {
        (*cctx).expectedInBuffer = *input;
    }

    if (*cctx).appliedParams.outBufferMode == ZSTD_bm_stable {
        (*cctx).expectedOutBufferSize = ((*output).size).wrapping_sub((*output).pos);
    }
}

/// Validate that the input/output buffers match the expectations set by
/// ZSTD_setBufferExpectations.
unsafe fn ZSTD_checkBufferStability(
    cctx: *const ZSTD_CCtx,
    output: *const ZSTD_outBuffer,
    input: *const ZSTD_inBuffer,
    _endOp: ZSTD_EndDirective,
) -> Result<(), Error> {
    if (*cctx).appliedParams.inBufferMode == ZSTD_bm_stable {
        let expect = (*cctx).expectedInBuffer;
        if expect.src != (*input).src || expect.pos != (*input).pos {
            return Err(Error::stabilityCondition_notRespected);
        }
    }

    if (*cctx).appliedParams.outBufferMode == ZSTD_bm_stable {
        let outBufferSize = ((*output).size).wrapping_sub((*output).pos);
        if (*cctx).expectedOutBufferSize != outBufferSize {
            return Err(Error::stabilityCondition_notRespected);
        }
    }

    Ok(())
}

/// If @endOp == ZSTD_e_end, @inSize becomes pledgedSrcSize.
/// Otherwise, it's ignored.
unsafe fn ZSTD_CCtx_init_compressStream2(
    cctx: *mut ZSTD_CCtx,
    endOp: ZSTD_EndDirective,
    inSize: usize,
) -> Result<(), Error> {
    let mut params = (*cctx).requestedParams;
    let prefixDict = (*cctx).prefixDict;
    ZSTD_initLocalDict(cctx)?;
    ptr::write_bytes(
        &mut (*cctx).prefixDict as *mut ZSTD_prefixDict as *mut u8,
        0,
        size_of::<ZSTD_prefixDict>(),
    );
    if !((*cctx).cdict).is_null() && ((*cctx).localDict.cdict).is_null() {
        // Let the cdict's compression level take priority over the requested params.
        // But do not take the cdict's compression level if the "cdict" is actually a localDict
        // generated from ZSTD_initLocalDict().
        params.compressionLevel = (*(*cctx).cdict).compressionLevel;
    }

    if endOp == ZSTD_e_end {
        (*cctx).pledgedSrcSizePlusOne = inSize.wrapping_add(1) as core::ffi::c_ulonglong;
    }

    let dictSize = if !prefixDict.dict.is_null() {
        prefixDict.dictSize
    } else if !((*cctx).cdict).is_null() {
        (*(*cctx).cdict).dictContentSize
    } else {
        0
    };
    let mode = ZSTD_getCParamMode(
        (*cctx).cdict,
        &params,
        ((*cctx).pledgedSrcSizePlusOne).wrapping_sub(1),
    );
    params.cParams = ZSTD_getCParamsFromCCtxParams_internal(
        &params,
        ((*cctx).pledgedSrcSizePlusOne).wrapping_sub(1),
        dictSize,
        mode,
    );

    params.postBlockSplitter =
        ZSTD_resolveBlockSplitterMode(params.postBlockSplitter, &params.cParams);
    params.ldmParams.enableLdm = ZSTD_resolveEnableLdm(params.ldmParams.enableLdm, &params.cParams);
    params.useRowMatchFinder =
        ZSTD_resolveRowMatchFinderMode(params.useRowMatchFinder, &params.cParams);
    params.validateSequences = ZSTD_resolveExternalSequenceValidation(params.validateSequences);
    params.maxBlockSize = ZSTD_resolveMaxBlockSize(params.maxBlockSize);
    params.searchForExternalRepcodes = ZSTD_resolveExternalRepcodeSearch(
        params.searchForExternalRepcodes,
        params.compressionLevel,
    );

    if params.has_ext_seq_prod() && params.nbWorkers >= 1 {
        return Err(Error::parameter_combination_unsupported);
    }

    if ((*cctx).pledgedSrcSizePlusOne).wrapping_sub(1)
        <= ZSTDMT_JOBSIZE_MIN as core::ffi::c_ulonglong
    {
        // do not invoke multi-threading when src size is too small
        params.nbWorkers = 0;
    }
    if params.nbWorkers > 0 {
        (*cctx).traceCtx = ZSTD_trace_compress_begin(cctx);

        // mt context creation
        if ((*cctx).mtctx).is_null() {
            (*cctx).mtctx = ZSTDMT_createCCtx_advanced(
                params.nbWorkers as u32,
                (*cctx).customMem,
                (*cctx).pool,
            );
            if ((*cctx).mtctx).is_null() {
                return Err(Error::memory_allocation);
            }
        }

        // mt compression
        ZSTDMT_initCStream_internal(
            (*cctx).mtctx,
            prefixDict.dict,
            prefixDict.dictSize,
            prefixDict.dictContentType,
            (*cctx).cdict,
            params,
            ((*cctx).pledgedSrcSizePlusOne).wrapping_sub(1),
        )?;

        (*cctx).dictID = if !((*cctx).cdict).is_null() {
            (*(*cctx).cdict).dictID
        } else {
            0
        };
        (*cctx).dictContentSize = if !((*cctx).cdict).is_null() {
            (*(*cctx).cdict).dictContentSize
        } else {
            prefixDict.dictSize
        };
        (*cctx).consumedSrcSize = 0;
        (*cctx).producedCSize = 0;
        (*cctx).streamStage = StreamStage::Load;
        (*cctx).appliedParams = params;
    } else {
        let pledgedSrcSize = ((*cctx).pledgedSrcSizePlusOne).wrapping_sub(1);
        ZSTD_compressBegin_internal(
            cctx,
            prefixDict.dict,
            prefixDict.dictSize,
            prefixDict.dictContentType,
            DictTableLoadMethod::Fast,
            (*cctx).cdict,
            &params,
            pledgedSrcSize,
            BufferedPolicy::Buffered,
        )?;

        (*cctx).inToCompress = 0;
        (*cctx).inBuffPos = 0;

        if (*cctx).appliedParams.inBufferMode == ZSTD_bm_buffered {
            // for small input: avoid automatic flush on reaching end of block, since
            // it would require to add a 3-bytes null block to end frame
            (*cctx).inBuffTarget = ((*cctx).blockSizeMax)
                .wrapping_add(usize::from((*cctx).blockSizeMax as u64 == pledgedSrcSize));
        } else {
            (*cctx).inBuffTarget = 0;
        }

        (*cctx).outBuffFlushedSize = 0;
        (*cctx).outBuffContentSize = (*cctx).outBuffFlushedSize;
        (*cctx).streamStage = StreamStage::Load;
        (*cctx).frameEnded = 0;
    }

    Ok(())
}

/// # Returns
///
/// The minimum amount of data remaining to be flushed from internal buffers
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_compressStream2))]
pub unsafe extern "C" fn ZSTD_compressStream2(
    cctx: *mut ZSTD_CCtx,
    output: *mut ZSTD_outBuffer,
    input: *mut ZSTD_inBuffer,
    endOp: ZSTD_EndDirective,
) -> usize {
    ZSTD_compressStream2_internal(cctx, output, input, endOp)
        .unwrap_or_else(|err| err.to_error_code())
}

unsafe fn ZSTD_compressStream2_internal(
    cctx: *mut ZSTD_CCtx,
    output: *mut ZSTD_outBuffer,
    input: *mut ZSTD_inBuffer,
    endOp: ZSTD_EndDirective,
) -> Result<usize, Error> {
    if (*output).pos > (*output).size {
        return Err(Error::dstSize_tooSmall);
    }
    if (*input).pos > (*input).size {
        return Err(Error::srcSize_wrong);
    }
    if endOp > ZSTD_e_end as core::ffi::c_int as u32 {
        return Err(Error::parameter_outOfBound);
    }

    // transparent initialization stage
    if (*cctx).streamStage == StreamStage::Init {
        // no obligation to start from pos==0
        let inputSize = ((*input).size).wrapping_sub((*input).pos);
        let totalInputSize = inputSize.wrapping_add((*cctx).stableIn_notConsumed);
        if (*cctx).requestedParams.inBufferMode == ZSTD_bm_stable
            && endOp == ZSTD_e_continue
            && totalInputSize < ZSTD_BLOCKSIZE_MAX as usize
        {
            if (*cctx).stableIn_notConsumed != 0 {
                // check stable source guarantees
                if (*input).src != (*cctx).expectedInBuffer.src {
                    return Err(Error::stabilityCondition_notRespected);
                }
                if (*input).pos != (*cctx).expectedInBuffer.size {
                    return Err(Error::stabilityCondition_notRespected);
                }
            }
            // pretend input was consumed, to give a sense forward progress
            (*input).pos = (*input).size;
            // save stable inBuffer, for later control, and flush/end
            (*cctx).expectedInBuffer = *input;
            // but actually input wasn't consumed, so keep track of position from where compression shall resume
            (*cctx).stableIn_notConsumed = ((*cctx).stableIn_notConsumed).wrapping_add(inputSize);
            // don't initialize yet, wait for the first block of flush() order, for better parameters adaptation
            return Ok((*cctx).requestedParams.format.frame_header_size_min());
        }
        ZSTD_CCtx_init_compressStream2(cctx, endOp, totalInputSize)?;
        ZSTD_setBufferExpectations(cctx, output, input);
    }

    ZSTD_checkBufferStability(cctx, output, input, endOp)?;

    // compression stage
    if (*cctx).appliedParams.nbWorkers > 0 {
        let mut flushMin: usize;
        if (*cctx).cParamsChanged != 0 {
            ZSTDMT_updateCParams_whileCompressing((*cctx).mtctx, &(*cctx).requestedParams);
            (*cctx).cParamsChanged = 0;
        }
        if (*cctx).stableIn_notConsumed != 0 {
            // some early data was skipped - make it available for consumption
            (*input).pos = ((*input).pos).wrapping_sub((*cctx).stableIn_notConsumed);
            (*cctx).stableIn_notConsumed = 0;
        }
        loop {
            let ipos = (*input).pos;
            let opos = (*output).pos;
            let result = ZSTDMT_compressStream_generic((*cctx).mtctx, output, input, endOp);
            (*cctx).consumedSrcSize = ((*cctx).consumedSrcSize)
                .wrapping_add(((*input).pos).wrapping_sub(ipos) as core::ffi::c_ulonglong);
            (*cctx).producedCSize = ((*cctx).producedCSize)
                .wrapping_add(((*output).pos).wrapping_sub(opos) as core::ffi::c_ulonglong);
            flushMin = result.inspect_err(|_| {
                ZSTD_CCtx_reset(cctx, ZSTD_ResetDirective::ZSTD_reset_session_only);
            })?;
            if endOp == ZSTD_e_end && flushMin == 0 {
                ZSTD_CCtx_trace(cctx, 0);
                ZSTD_CCtx_reset(cctx, ZSTD_ResetDirective::ZSTD_reset_session_only);
            }

            if endOp == ZSTD_e_continue {
                // We only require some progress with ZSTD_e_continue, not maximal progress.
                // We're done if we've consumed or produced any bytes, or either buffer is full.
                if (*input).pos != ipos
                    || (*output).pos != opos
                    || (*input).pos == (*input).size
                    || (*output).pos == (*output).size
                {
                    break;
                }
            } else if flushMin == 0 || (*output).pos == (*output).size {
                // We require maximal progress. We're done when the flush is complete or the
                // output buffer is full.
                break;
            }
        }

        // Either we don't require maximum forward progress, we've finished the
        // flush, or we are out of output space.
        ZSTD_setBufferExpectations(cctx, output, input);
        return Ok(flushMin);
    }

    ZSTD_compressStream_generic(cctx, output, input, endOp)?;
    ZSTD_setBufferExpectations(cctx, output, input);

    Ok(((*cctx).outBuffContentSize).wrapping_sub((*cctx).outBuffFlushedSize))
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_compressStream2_simpleArgs))]
pub unsafe extern "C" fn ZSTD_compressStream2_simpleArgs(
    cctx: *mut ZSTD_CCtx,
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    dstPos: *mut usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
    srcPos: *mut usize,
    endOp: ZSTD_EndDirective,
) -> usize {
    let mut output = ZSTD_outBuffer_s {
        dst,
        size: dstCapacity,
        pos: *dstPos,
    };
    let mut input = ZSTD_inBuffer_s {
        src,
        size: srcSize,
        pos: *srcPos,
    };

    // ZSTD_compressStream2() will check validity of dstPos and srcPos
    let cErr = ZSTD_compressStream2(cctx, &mut output, &mut input, endOp);
    *dstPos = output.pos;
    *srcPos = input.pos;
    cErr
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_compress2))]
pub unsafe extern "C" fn ZSTD_compress2(
    cctx: *mut ZSTD_CCtx,
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    let originalInBufferMode = (*cctx).requestedParams.inBufferMode;
    let originalOutBufferMode = (*cctx).requestedParams.outBufferMode;
    ZSTD_CCtx_reset(cctx, ZSTD_ResetDirective::ZSTD_reset_session_only);

    // Enable stable input/output buffers.
    (*cctx).requestedParams.inBufferMode = ZSTD_bm_stable;
    (*cctx).requestedParams.outBufferMode = ZSTD_bm_stable;

    let mut oPos = 0;
    let mut iPos = 0;
    let result = ZSTD_compressStream2_simpleArgs(
        cctx,
        dst,
        dstCapacity,
        &mut oPos,
        src,
        srcSize,
        &mut iPos,
        ZSTD_e_end,
    );

    // Reset to the original values.
    (*cctx).requestedParams.inBufferMode = originalInBufferMode;
    (*cctx).requestedParams.outBufferMode = originalOutBufferMode;

    let err_code = result;
    if ERR_isError(err_code) {
        return err_code;
    }
    if result != 0 {
        // compression not completed, due to lack of output space
        return Error::dstSize_tooSmall.to_error_code();
    }

    oPos
}

/// offBase must use the format required by ZSTD_storeSeq().
///
/// # Returns
///
/// A ZSTD error if sequence is not valid
fn ZSTD_validateSequence(
    offBase: u32,
    matchLength: u32,
    minMatch: u32,
    posInSrc: usize,
    windowLog: u32,
    dictSize: usize,
    useSequenceProducer: bool,
) -> Result<(), Error> {
    let windowSize = 1 << windowLog;
    // posInSrc represents the amount of data the decoder would decode up to this point.
    // As long as the amount of data decoded is less than or equal to window size, offsets may be
    // larger than the total length of output decoded in order to reference the dict, even larger than
    // window size. After output surpasses windowSize, we're limited to windowSize offsets again.
    let offsetBound = if posInSrc > windowSize as usize {
        windowSize as usize
    } else {
        posInSrc.wrapping_add(dictSize)
    };
    let matchLenLowerBound = (if minMatch == 3 || useSequenceProducer {
        3
    } else {
        4
    }) as usize;

    if offBase as usize > offsetBound.wrapping_add(3) {
        return Err(Error::externalSequences_invalid);
    }

    // Validate maxNbSeq is large enough for the given matchLength and minMatch
    if (matchLength as usize) < matchLenLowerBound {
        return Err(Error::externalSequences_invalid);
    }

    Ok(())
}

/// Returns an offset code, given a sequence's raw offset, the ongoing repcode array, and whether
/// litLength == 0
fn ZSTD_finalizeOffBase(rawOffset: u32, rep: &RepCodes, ll0: bool) -> u32 {
    if !ll0 && rawOffset == rep[0] {
        REPCODE1_TO_OFFBASE
    } else if rawOffset == rep[1] {
        2u32.wrapping_sub(ll0 as u32)
    } else if rawOffset == rep[2] {
        3u32.wrapping_sub(ll0 as u32)
    } else if ll0 && rawOffset == rep[0].wrapping_sub(1) {
        REPCODE3_TO_OFFBASE
    } else {
        rawOffset.wrapping_add(ZSTD_REP_NUM)
    }
}

/// This function scans through an array of ZSTD_Sequence,
/// storing the sequences it reads, until it reaches a block delimiter.
/// Note that the block delimiter includes the last literals of the block.
/// `blockSize` must be == sum(sequence_lengths).
///
/// # Returns
///
/// `blockSize` on success, or a ZSTD_error
unsafe fn ZSTD_transferSequences_wBlockDelim(
    cctx: *mut ZSTD_CCtx,
    seqPos: &mut ZSTD_SequencePosition,
    inSeqs: &[ZSTD_Sequence],
    src: *const core::ffi::c_void,
    blockSize: usize,
    externalRepSearch: ParamSwitch,
) -> Result<usize, Error> {
    let mut idx = seqPos.idx;
    let startIdx = idx;
    let mut ip = src as *const u8;
    let iend = ip.add(blockSize);

    let dictSize = if !((*cctx).cdict).is_null() {
        (*(*cctx).cdict).dictContentSize as u32
    } else if !((*cctx).prefixDict.dict).is_null() {
        (*cctx).prefixDict.dictSize as u32
    } else {
        0
    };

    let mut updatedRepcodes = (*(*cctx).blockState.prevCBlock).rep;
    for seq in &inSeqs[startIdx as usize..] {
        if seq.matchLength == 0 && seq.offset == 0 {
            break;
        }
        let litLength = seq.litLength;
        let matchLength = seq.matchLength;

        let offBase: u32;
        if externalRepSearch == ParamSwitch::Disable {
            offBase = seq.offset.wrapping_add(ZSTD_REP_NUM);
        } else {
            let ll0 = litLength == 0;
            offBase = ZSTD_finalizeOffBase(seq.offset, &updatedRepcodes, ll0);
            ZSTD_updateRep(&mut updatedRepcodes, offBase, ll0);
        }

        if (*cctx).appliedParams.validateSequences != 0 {
            seqPos.posInSrc = seqPos
                .posInSrc
                .wrapping_add(litLength.wrapping_add(matchLength) as usize);
            ZSTD_validateSequence(
                offBase,
                matchLength,
                (*cctx).appliedParams.cParams.minMatch,
                seqPos.posInSrc,
                (*cctx).appliedParams.cParams.windowLog,
                dictSize as usize,
                (*cctx).appliedParams.has_ext_seq_prod(),
            )?;
        }
        if idx.wrapping_sub(seqPos.idx) as usize >= (*cctx).seqStore.maxNbSeq {
            return Err(Error::externalSequences_invalid);
        }
        ZSTD_storeSeq(
            &mut (*cctx).seqStore,
            litLength as usize,
            ip,
            iend,
            offBase,
            matchLength as usize,
        );
        ip = ip.offset(matchLength.wrapping_add(litLength) as isize);
        idx = idx.wrapping_add(1);
    }

    if idx as usize == inSeqs.len() {
        return Err(Error::externalSequences_invalid);
    }

    // If we skipped repcode search while parsing, we need to update repcodes now
    if externalRepSearch == ParamSwitch::Disable && idx != startIdx {
        let rep = &mut updatedRepcodes;
        let lastSeqIdx = idx.wrapping_sub(1); // index of last non-block-delimiter sequence

        if lastSeqIdx >= startIdx.wrapping_add(2) {
            rep[2] = inSeqs[lastSeqIdx.wrapping_sub(2) as usize].offset;
            rep[1] = inSeqs[lastSeqIdx.wrapping_sub(1) as usize].offset;
            rep[0] = inSeqs[lastSeqIdx as usize].offset;
        } else if lastSeqIdx == startIdx.wrapping_add(1) {
            rep[2] = rep[0];
            rep[1] = inSeqs[lastSeqIdx.wrapping_sub(1) as usize].offset;
            rep[0] = inSeqs[lastSeqIdx as usize].offset;
        } else {
            rep[2] = rep[1];
            rep[1] = rep[0];
            rep[0] = inSeqs[lastSeqIdx as usize].offset;
        }
    }

    (*(*cctx).blockState.nextCBlock).rep = updatedRepcodes;

    if inSeqs[idx as usize].litLength != 0 {
        ZSTD_storeLastLiterals(
            &mut (*cctx).seqStore,
            ip,
            inSeqs[idx as usize].litLength as usize,
        );
        ip = ip.offset(inSeqs[idx as usize].litLength as isize);
        seqPos.posInSrc = seqPos
            .posInSrc
            .wrapping_add(inSeqs[idx as usize].litLength as usize);
    }

    if ip != iend {
        return Err(Error::externalSequences_invalid);
    }

    seqPos.idx = idx.wrapping_add(1);

    Ok(blockSize)
}

/// This function attempts to scan through @blockSize bytes in @src
/// represented by the sequences in @inSeqs, storing any (partial) sequences.
///
/// Occasionally, we may want to reduce the actual number of bytes consumed from @src
/// to avoid splitting a match, notably if it would produce a match smaller than MINMATCH.
///
/// # Returns
///
/// The number of bytes consumed from `src`, necessarily <= `blockSize`.
/// Otherwise, it may return a ZSTD error if something went wrong.
unsafe fn ZSTD_transferSequences_noDelim(
    cctx: *mut ZSTD_CCtx,
    seqPos: &mut ZSTD_SequencePosition,
    inSeqs: &[ZSTD_Sequence],
    src: *const core::ffi::c_void,
    blockSize: usize,
    externalRepSearch: ParamSwitch,
) -> Result<usize, Error> {
    let mut idx = seqPos.idx;
    let mut startPosInSequence = seqPos.posInSequence;
    let mut endPosInSequence = seqPos.posInSequence.wrapping_add(blockSize as u32);
    let istart = src as *const u8;
    let mut ip = istart;
    let mut iend = istart.add(blockSize);
    let mut bytesAdjustment = 0;
    let mut finalMatchSplit = false;

    /* TODO(embg) support fast parsing mode in noBlockDelim mode */
    let _ = externalRepSearch;

    let dictSize = if !((*cctx).cdict).is_null() {
        (*(*cctx).cdict).dictContentSize
    } else if !((*cctx).prefixDict.dict).is_null() {
        (*cctx).prefixDict.dictSize
    } else {
        0
    };
    let mut updatedRepcodes = (*(*cctx).blockState.prevCBlock).rep;
    for currSeq in inSeqs.get(idx as usize..).unwrap_or_default() {
        if endPosInSequence == 0 || finalMatchSplit {
            break;
        }
        let mut litLength = currSeq.litLength;
        let mut matchLength = currSeq.matchLength;
        let rawOffset = currSeq.offset;

        // Modify the sequence depending on where endPosInSequence lies
        if endPosInSequence >= currSeq.litLength.wrapping_add(currSeq.matchLength) {
            if startPosInSequence >= litLength {
                startPosInSequence = startPosInSequence.wrapping_sub(litLength);
                litLength = 0;
                matchLength = matchLength.wrapping_sub(startPosInSequence);
            } else {
                litLength = litLength.wrapping_sub(startPosInSequence);
            }
            // Move to the next sequence
            endPosInSequence = (endPosInSequence as core::ffi::c_uint)
                .wrapping_sub(currSeq.litLength.wrapping_add(currSeq.matchLength));
            startPosInSequence = 0;
        } else {
            // This is the final (partial) sequence we're adding from inSeqs, and endPosInSequence
            // does not reach the end of the match. So, we have to split the sequence
            if endPosInSequence <= litLength {
                // This sequence ends inside the literals, break to store the last literals
                break;
            }
            litLength = if startPosInSequence >= litLength {
                0
            } else {
                litLength.wrapping_sub(startPosInSequence)
            };
            let mut firstHalfMatchLength = endPosInSequence
                .wrapping_sub(startPosInSequence)
                .wrapping_sub(litLength);
            if matchLength as usize > blockSize
                && firstHalfMatchLength >= (*cctx).appliedParams.cParams.minMatch
            {
                // Only ever split the match if it is larger than the block size
                let secondHalfMatchLength = (currSeq.matchLength)
                    .wrapping_add(currSeq.litLength)
                    .wrapping_sub(endPosInSequence);
                if secondHalfMatchLength < (*cctx).appliedParams.cParams.minMatch {
                    // Move the endPosInSequence backward so that it creates match of minMatch length
                    endPosInSequence = (endPosInSequence as core::ffi::c_uint).wrapping_sub(
                        ((*cctx).appliedParams.cParams.minMatch)
                            .wrapping_sub(secondHalfMatchLength),
                    );
                    bytesAdjustment = ((*cctx).appliedParams.cParams.minMatch)
                        .wrapping_sub(secondHalfMatchLength);
                    firstHalfMatchLength = firstHalfMatchLength.wrapping_sub(bytesAdjustment);
                }
                matchLength = firstHalfMatchLength;
                // Flag that we split the last match - after storing the sequence, exit the loop,
                // but keep the value of endPosInSequence
                finalMatchSplit = true;
            } else {
                // Move the position in sequence backwards so that we don't split match, and break to store
                // the last literals. We use the original currSeq.litLength as a marker for where endPosInSequence
                // should go. We prefer to do this whenever it is not necessary to split the match, or if doing so
                // would cause the first half of the match to be too small
                bytesAdjustment = endPosInSequence.wrapping_sub(currSeq.litLength);
                endPosInSequence = currSeq.litLength;
                break;
            }
        }

        // Check if this offset can be represented with a repcode
        let ll0 = litLength == 0;
        let offBase = ZSTD_finalizeOffBase(rawOffset, &updatedRepcodes, ll0);
        ZSTD_updateRep(&mut updatedRepcodes, offBase, ll0);

        if (*cctx).appliedParams.validateSequences != 0 {
            seqPos.posInSrc = seqPos
                .posInSrc
                .wrapping_add(litLength.wrapping_add(matchLength) as usize);
            ZSTD_validateSequence(
                offBase,
                matchLength,
                (*cctx).appliedParams.cParams.minMatch,
                seqPos.posInSrc,
                (*cctx).appliedParams.cParams.windowLog,
                dictSize,
                (*cctx).appliedParams.has_ext_seq_prod(),
            )?;
        }

        if idx.wrapping_sub(seqPos.idx) as usize >= (*cctx).seqStore.maxNbSeq {
            return Err(Error::externalSequences_invalid);
        }

        ZSTD_storeSeq(
            &mut (*cctx).seqStore,
            litLength as usize,
            ip,
            iend,
            offBase,
            matchLength as usize,
        );
        ip = ip.offset(matchLength.wrapping_add(litLength) as isize);
        if !finalMatchSplit {
            // Next Sequence
            idx = idx.wrapping_add(1);
        }
    }

    seqPos.idx = idx;
    seqPos.posInSequence = endPosInSequence;
    (*(*cctx).blockState.nextCBlock).rep = updatedRepcodes;

    iend = iend.sub(bytesAdjustment as usize);
    if ip != iend {
        // Store any last literals
        let lastLLSize = iend.offset_from_unsigned(ip);
        ZSTD_storeLastLiterals(&mut (*cctx).seqStore, ip, lastLLSize);
        seqPos.posInSrc = seqPos.posInSrc.wrapping_add(lastLLSize);
    }

    Ok(iend.offset_from_unsigned(istart))
}

/// @seqPos represents a position within @inSeqs,
/// it is read and updated by this function,
/// once the goal to produce a block of size @blockSize is reached.
///
/// # Returns
///
/// The number of bytes consumed from @src, necessarily <= @blockSize.
fn ZSTD_selectSequenceCopier(mode: ZSTD_SequenceFormat_e) -> ZSTD_SequenceCopier_f {
    if mode == ZSTD_sf_explicitBlockDelimiters {
        ZSTD_transferSequences_wBlockDelim
    } else {
        ZSTD_transferSequences_noDelim
    }
}

/// Discover the size of next block by searching for the delimiter.
/// Note that a block delimiter **must** exist in this mode, otherwise it's an input error.
/// The block size retrieved will be later compared to ensure it remains within bounds
fn blockSize_explicitDelimiter(
    inSeqs: &[ZSTD_Sequence],
    seqPos: ZSTD_SequencePosition,
) -> Result<usize, Error> {
    let mut end = false;
    let mut blockSize = 0usize;

    for seq in inSeqs.get(seqPos.idx as usize..).unwrap_or_default() {
        end = seq.offset == 0;
        blockSize = blockSize.wrapping_add(seq.litLength.wrapping_add(seq.matchLength) as usize);
        if end {
            if seq.matchLength != 0 {
                return Err(Error::externalSequences_invalid);
            }
            break;
        }
    }

    if !end {
        return Err(Error::externalSequences_invalid);
    }

    Ok(blockSize)
}

fn determine_blockSize(
    mode: ZSTD_SequenceFormat_e,
    blockSize: usize,
    remaining: usize,
    inSeqs: &[ZSTD_Sequence],
    seqPos: ZSTD_SequencePosition,
) -> Result<usize, Error> {
    if mode == ZSTD_sf_noBlockDelimiters {
        // Note: more a "target" block size
        return Ok(remaining.min(blockSize));
    }

    let explicitBlockSize = blockSize_explicitDelimiter(inSeqs, seqPos)?;
    if explicitBlockSize > blockSize {
        return Err(Error::externalSequences_invalid);
    }
    if explicitBlockSize > remaining {
        return Err(Error::externalSequences_invalid);
    }
    Ok(explicitBlockSize)
}

/// Compress all provided sequences, block-by-block.
///
/// # Returns
///
/// The cumulative size of all compressed blocks (including their headers),
/// or a ZSTD error.
unsafe fn ZSTD_compressSequences_internal(
    cctx: *mut ZSTD_CCtx,
    dst: *mut core::ffi::c_void,
    mut dstCapacity: usize,
    inSeqs: &[ZSTD_Sequence],
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> Result<usize, Error> {
    let mut cSize = 0usize;
    let mut remaining = srcSize;
    let mut seqPos = {
        ZSTD_SequencePosition {
            idx: 0,
            posInSequence: 0,
            posInSrc: 0,
        }
    };

    let mut ip = src as *const u8;
    let mut op = dst as *mut u8;
    let sequenceCopier = ZSTD_selectSequenceCopier((*cctx).appliedParams.blockDelimiters);

    // Special case: empty frame
    if remaining == 0 {
        let cBlockHeader24 = 1u32.wrapping_add((BlockType::Raw as u32) << 1);
        if dstCapacity < 4 {
            return Err(Error::dstSize_tooSmall);
        }
        MEM_writeLE32(op as *mut core::ffi::c_void, cBlockHeader24);
        op = op.add(ZSTD_BLOCKHEADERSIZE);
        dstCapacity = dstCapacity.wrapping_sub(ZSTD_BLOCKHEADERSIZE);
        cSize = cSize.wrapping_add(ZSTD_BLOCKHEADERSIZE);
    }

    while remaining != 0 {
        let mut blockSize = determine_blockSize(
            (*cctx).appliedParams.blockDelimiters,
            (*cctx).blockSizeMax,
            remaining,
            inSeqs,
            seqPos,
        )?;
        let lastBlock = blockSize == remaining;
        ZSTD_resetSeqStore(&mut (*cctx).seqStore);

        blockSize = sequenceCopier(
            cctx,
            &mut seqPos,
            inSeqs,
            ip as *const core::ffi::c_void,
            blockSize,
            (*cctx).appliedParams.searchForExternalRepcodes,
        )?;

        // If blocks are too small, emit as a nocompress block
        if blockSize
            < (MIN_CBLOCK_SIZE as usize)
                .wrapping_add(ZSTD_BLOCKHEADERSIZE)
                .wrapping_add(1)
                .wrapping_add(1)
        {
            let cBlockSize = ZSTD_noCompressBlock(
                op as *mut core::ffi::c_void,
                dstCapacity,
                ip as *const core::ffi::c_void,
                blockSize,
                lastBlock,
            )?;

            cSize = cSize.wrapping_add(cBlockSize);
            ip = ip.add(blockSize);
            op = op.add(cBlockSize);
            remaining = remaining.wrapping_sub(blockSize);
            dstCapacity = dstCapacity.wrapping_sub(cBlockSize);
        } else {
            if dstCapacity < ZSTD_BLOCKHEADERSIZE {
                return Err(Error::dstSize_tooSmall);
            }
            let mut compressedSeqsSize = ZSTD_entropyCompressSeqStore(
                &(*cctx).seqStore,
                &(*(*cctx).blockState.prevCBlock).entropy,
                &mut (*(*cctx).blockState.nextCBlock).entropy,
                &(*cctx).appliedParams,
                op.add(ZSTD_BLOCKHEADERSIZE) as *mut core::ffi::c_void,
                dstCapacity.wrapping_sub(ZSTD_BLOCKHEADERSIZE),
                blockSize,
                (*cctx).tmpWorkspace,
                (*cctx).tmpWkspSize,
                (*cctx).bmi2 != 0,
            )?;

            if (*cctx).isFirstBlock == 0
                && ZSTD_maybeRLE(&(*cctx).seqStore)
                && ZSTD_isRLE(core::slice::from_raw_parts(ip, blockSize))
            {
                // Note: don't emit the first block as RLE even if it qualifies because
                // doing so will cause the decoder (cli <= v1.4.3 only) to throw an (invalid) error
                // "should consume all input error."
                compressedSeqsSize = 1;
            }

            let cBlockSize = if compressedSeqsSize == 0 {
                // ZSTD_noCompressBlock writes the block header as well
                ZSTD_noCompressBlock(
                    op as *mut core::ffi::c_void,
                    dstCapacity,
                    ip as *const core::ffi::c_void,
                    blockSize,
                    lastBlock,
                )?
            } else if compressedSeqsSize == 1 {
                ZSTD_rleCompressBlock(
                    op as *mut core::ffi::c_void,
                    dstCapacity,
                    *ip,
                    blockSize,
                    lastBlock,
                )?
            } else {
                // Error checking and repcodes update
                ZSTD_blockState_confirmRepcodesAndEntropyTables(&mut (*cctx).blockState);
                (*(*cctx).blockState.prevCBlock)
                    .entropy
                    .fse
                    .offcode_repeatMode
                    .require_check();

                // Write block header into beginning of block
                let cBlockHeader = u32::from(lastBlock)
                    .wrapping_add((BlockType::Compressed as u32) << 1)
                    .wrapping_add((compressedSeqsSize << 3) as u32);
                MEM_writeLE24(op as *mut core::ffi::c_void, cBlockHeader);
                ZSTD_BLOCKHEADERSIZE.wrapping_add(compressedSeqsSize)
            };

            cSize = cSize.wrapping_add(cBlockSize);

            if lastBlock {
                break;
            }
            ip = ip.add(blockSize);
            op = op.add(cBlockSize);
            remaining = remaining.wrapping_sub(blockSize);
            dstCapacity = dstCapacity.wrapping_sub(cBlockSize);
            (*cctx).isFirstBlock = 0;
        }
    }

    Ok(cSize)
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_compressSequences))]
pub unsafe extern "C" fn ZSTD_compressSequences(
    cctx: *mut ZSTD_CCtx,
    dst: *mut core::ffi::c_void,
    mut dstCapacity: usize,
    inSeqs: *const ZSTD_Sequence,
    inSeqsSize: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
) -> usize {
    let mut op = dst as *mut u8;
    let mut cSize = 0usize;

    // Transparent initialization stage, same as compressStream2()
    if let Err(err) = ZSTD_CCtx_init_compressStream2(cctx, ZSTD_e_end, srcSize) {
        return err.to_error_code();
    }

    // Begin writing output, starting with frame header
    let frameHeaderSize = match ZSTD_writeFrameHeader(
        op as *mut core::ffi::c_void,
        dstCapacity,
        &(*cctx).appliedParams,
        srcSize as u64,
        (*cctx).dictID,
    ) {
        Ok(size) => size,
        Err(err) => return err.to_error_code(),
    };
    op = op.add(frameHeaderSize);
    dstCapacity = dstCapacity.wrapping_sub(frameHeaderSize);
    cSize = cSize.wrapping_add(frameHeaderSize);

    if (*cctx).appliedParams.fParams.checksumFlag != 0 && srcSize != 0 {
        ZSTD_XXH64_update_slice(
            &mut (*cctx).xxhState,
            core::slice::from_raw_parts(src as *const u8, srcSize),
        );
    }

    // Now generate compressed blocks
    let inSeqs = if inSeqs.is_null() || inSeqsSize == 0 {
        &[]
    } else {
        core::slice::from_raw_parts(inSeqs, inSeqsSize)
    };
    let cBlocksSize = match ZSTD_compressSequences_internal(
        cctx,
        op as *mut core::ffi::c_void,
        dstCapacity,
        inSeqs,
        src,
        srcSize,
    ) {
        Ok(cBlocksSize) => cBlocksSize,
        Err(err) => return err.to_error_code(),
    };
    cSize = cSize.wrapping_add(cBlocksSize);
    dstCapacity = dstCapacity.wrapping_sub(cBlocksSize);

    // Complete with frame checksum, if needed
    if (*cctx).appliedParams.fParams.checksumFlag != 0 {
        let checksum = ZSTD_XXH64_digest(&mut (*cctx).xxhState) as u32;
        if dstCapacity < 4 {
            return Error::dstSize_tooSmall.to_error_code();
        }
        MEM_writeLE32(
            (dst as *mut core::ffi::c_char).add(cSize) as *mut core::ffi::c_void,
            checksum,
        );
        cSize = cSize.wrapping_add(4);
    }

    cSize
}

/// Convert sequences from external ZSTD_Sequence format to internal SeqDef format:
///   - offset -> offBase = offset + ZSTD_REP_NUM
///   - litLength -> (U16) litLength
///   - matchLength -> (U16)(matchLength - MINMATCH)
///   - rep is ignored
///
/// Store only 8 bytes per SeqDef (offBase[4], litLength[2], mlBase[2]).
///
/// # Returns
///
/// - 0 on success, with no long length detected.
/// - > 0 if there is one long length (> 65535), indicating the position and type.
pub fn convertSequences_noRepcodes(
    dstSeqs: &mut [MaybeUninit<SeqDef>],
    inSeqs: &[ZSTD_Sequence],
) -> usize {
    let mut longLen = 0;

    for (n, seq) in inSeqs.iter().enumerate() {
        let seq_def = SeqDef {
            offBase: seq.offset.wrapping_add(ZSTD_REP_NUM),
            litLength: seq.litLength as u16,
            mlBase: seq.matchLength.wrapping_sub(u32::from(MINMATCH)) as u16,
        };

        dstSeqs[n] = MaybeUninit::new(seq_def);

        // Check for long length > 65535
        if seq.matchLength > 65535 + 3 {
            longLen = n.wrapping_add(1);
        }
        if seq.litLength > 65535 {
            longLen = n.wrapping_add(inSeqs.len()).wrapping_add(1);
        }
    }

    longLen
}

/// Precondition: Sequences must end on an explicit Block Delimiter.
///
/// Note: sequence validation functionality has been disabled (removed).
/// This is helpful to generate a lean main pipeline, improving performance.
/// It may be re-inserted later.
pub unsafe fn ZSTD_convertBlockSequences(
    cctx: *mut ZSTD_CCtx,
    inSeqs: &[ZSTD_Sequence],
    repcodeResolution: bool,
) -> Result<(), Error> {
    if inSeqs.len() >= (*cctx).seqStore.maxNbSeq {
        return Err(Error::externalSequences_invalid);
    }

    let mut updatedRepcodes = (*(*cctx).blockState.prevCBlock).rep;

    // Convert Sequences from public format to internal format
    if !repcodeResolution {
        let longl = convertSequences_noRepcodes(
            core::slice::from_raw_parts_mut(
                (*cctx).seqStore.sequencesStart.cast(),
                inSeqs.len() - 1,
            ),
            &inSeqs[..inSeqs.len() - 1],
        );
        (*cctx).seqStore.sequences = ((*cctx).seqStore.sequencesStart).add(inSeqs.len() - 1);
        if longl != 0 {
            if longl < inSeqs.len() {
                (*cctx).seqStore.longLengthType = LongLengthType::Match;
                (*cctx).seqStore.longLengthPos = longl.wrapping_sub(1) as u32;
            } else {
                (*cctx).seqStore.longLengthType = LongLengthType::Literal;
                (*cctx).seqStore.longLengthPos =
                    longl.wrapping_sub(inSeqs.len() - 1).wrapping_sub(1) as u32;
            }
        }
    } else {
        for seq in &inSeqs[..inSeqs.len() - 1] {
            let litLength = seq.litLength;
            let matchLength = seq.matchLength;
            let ll0 = litLength == 0;
            let offBase = ZSTD_finalizeOffBase(seq.offset, &updatedRepcodes, ll0);
            ZSTD_storeSeqOnly(
                &mut (*cctx).seqStore,
                litLength as usize,
                offBase,
                matchLength as usize,
            );
            ZSTD_updateRep(&mut updatedRepcodes, offBase, ll0);
        }
    }

    // If we skipped repcode search while parsing, we need to update repcodes now
    if !repcodeResolution && inSeqs.len() > 1 {
        let rep = &mut updatedRepcodes;

        if inSeqs.len() >= 4 {
            let lastSeqIdx = inSeqs.len() - 2; // index of last full sequence
            rep[2] = inSeqs[lastSeqIdx - 2].offset;
            rep[1] = inSeqs[lastSeqIdx - 1].offset;
            rep[0] = inSeqs[lastSeqIdx].offset;
        } else if inSeqs.len() == 3 {
            rep[2] = rep[0];
            rep[1] = inSeqs[0].offset;
            rep[0] = inSeqs[1].offset;
        } else {
            rep[2] = rep[1];
            rep[1] = rep[0];
            rep[0] = inSeqs[0].offset;
        }
    }

    (*(*cctx).blockState.nextCBlock).rep = updatedRepcodes;

    Ok(())
}

/// The function assumes `litMatchLength` is a packed 64-bit value where the
/// lower 32 bits represent the match length. The check varies based on the
/// system's endianness:
/// - On little-endian systems, it verifies if the entire 64-bit value is at most
///   0xFFFFFFFF, indicating the match length (lower 32 bits) is zero.
/// - On big-endian systems, it directly checks if the lower 32 bits are zero.
///
/// # Returns
///
/// `true` if the match length is zero
#[inline(always)]
const fn matchLengthHalfIsZero(litMatchLength: u64) -> bool {
    if cfg!(target_endian = "little") {
        litMatchLength <= 0xffffffff
    } else {
        litMatchLength as u32 == 0
    }
}

#[inline(always)]
fn readLitMatchLength(seq: &ZSTD_Sequence) -> u64 {
    let (lo, hi) = if cfg!(target_endian = "little") {
        (seq.litLength, seq.matchLength)
    } else {
        (seq.matchLength, seq.litLength)
    };
    u64::from(lo) | (u64::from(hi) << 32)
}

pub fn ZSTD_get1BlockSummary(seqs: &[ZSTD_Sequence]) -> Result<BlockSummary, Error> {
    // Use multiple accumulators for efficient use of wide out-of-order machines.
    let mut litMatchSize0 = 0u64;
    let mut litMatchSize1 = 0u64;
    let mut litMatchSize2 = 0u64;
    let mut litMatchSize3 = 0u64;

    // Index of the block delimiter (the first sequence with a matchLength of 0).
    let n = 'out: {
        let (chunks, remainder) = seqs.as_chunks::<4>();

        // Process the input in 4 independent streams to reach high throughput.
        for (i, [s0, s1, s2, s3]) in chunks.iter().enumerate() {
            let mut litMatchLength = readLitMatchLength(s0);
            litMatchSize0 = litMatchSize0.wrapping_add(litMatchLength);
            if matchLengthHalfIsZero(litMatchLength) {
                break 'out 4 * i;
            }

            litMatchLength = readLitMatchLength(s1);
            litMatchSize1 = litMatchSize1.wrapping_add(litMatchLength);
            if matchLengthHalfIsZero(litMatchLength) {
                break 'out 4 * i + 1;
            }

            litMatchLength = readLitMatchLength(s2);
            litMatchSize2 = litMatchSize2.wrapping_add(litMatchLength);
            if matchLengthHalfIsZero(litMatchLength) {
                break 'out 4 * i + 2;
            }

            litMatchLength = readLitMatchLength(s3);
            litMatchSize3 = litMatchSize3.wrapping_add(litMatchLength);
            if matchLengthHalfIsZero(litMatchLength) {
                break 'out 4 * i + 3;
            }
        }

        for (j, seq) in remainder.iter().enumerate() {
            let litMatchLength = readLitMatchLength(seq);
            litMatchSize0 = litMatchSize0.wrapping_add(litMatchLength);
            if matchLengthHalfIsZero(litMatchLength) {
                break 'out 4 * chunks.len() + j;
            }
        }

        return Err(Error::externalSequences_invalid); // No end terminator.
    };

    litMatchSize0 = litMatchSize0.wrapping_add(
        litMatchSize1
            .wrapping_add(litMatchSize2)
            .wrapping_add(litMatchSize3),
    );
    if cfg!(target_endian = "little") {
        let litSize = litMatchSize0 as u32 as usize;
        Ok(BlockSummary {
            nbSequences: n.wrapping_add(1),
            blockSize: (litSize as u64).wrapping_add(litMatchSize0 >> 32) as usize,
            litSize,
        })
    } else {
        let litSize = (litMatchSize0 >> 32) as usize;
        Ok(BlockSummary {
            nbSequences: n.wrapping_add(1),
            blockSize: litSize.wrapping_add(litMatchSize0 as u32 as usize),
            litSize,
        })
    }
}

unsafe fn ZSTD_compressSequencesAndLiterals_internal(
    cctx: *mut ZSTD_CCtx,
    dst: *mut core::ffi::c_void,
    mut dstCapacity: usize,
    mut inSeqs: &[ZSTD_Sequence],
    mut literals: &[u8],
    srcSize: usize,
) -> Result<usize, Error> {
    let mut remaining = srcSize;
    let mut cSize = 0usize;
    let mut op = dst as *mut u8;
    let repcodeResolution = (*cctx).appliedParams.searchForExternalRepcodes == ParamSwitch::Enable;

    if inSeqs.is_empty() {
        return Err(Error::externalSequences_invalid);
    }

    // Special case: empty frame
    if inSeqs.len() == 1 && inSeqs[0].litLength == 0 {
        let cBlockHeader24 = 1u32.wrapping_add((BlockType::Raw as u32) << 1);
        if dstCapacity < 3 {
            return Err(Error::dstSize_tooSmall);
        }
        MEM_writeLE24(op as *mut core::ffi::c_void, cBlockHeader24);
        op = op.add(ZSTD_BLOCKHEADERSIZE);
        dstCapacity = dstCapacity.wrapping_sub(ZSTD_BLOCKHEADERSIZE);
        cSize = cSize.wrapping_add(ZSTD_BLOCKHEADERSIZE);
    }

    while !inSeqs.is_empty() {
        let block = ZSTD_get1BlockSummary(inSeqs)?;
        let lastBlock = block.nbSequences == inSeqs.len();
        if block.litSize > literals.len() {
            return Err(Error::externalSequences_invalid);
        }
        ZSTD_resetSeqStore(&mut (*cctx).seqStore);

        ZSTD_convertBlockSequences(cctx, &inSeqs[..block.nbSequences], repcodeResolution)?;
        inSeqs = &inSeqs[block.nbSequences..];
        remaining = remaining.wrapping_sub(block.blockSize);

        // Note: when blockSize is very small, other variant send it uncompressed.
        // Here, we still send the sequences, because we don't have the original source to send it uncompressed.
        // One could imagine in theory reproducing the source from the sequences,
        // but that's complex and costly memory intensive, and goes against the objectives of this variant.

        if dstCapacity < ZSTD_BLOCKHEADERSIZE {
            return Err(Error::dstSize_tooSmall);
        }

        let mut compressedSeqsSize = ZSTD_entropyCompressSeqStore_internal(
            op.add(ZSTD_BLOCKHEADERSIZE) as *mut core::ffi::c_void,
            dstCapacity.wrapping_sub(ZSTD_BLOCKHEADERSIZE),
            &literals[..block.litSize],
            &(*cctx).seqStore,
            &(*(*cctx).blockState.prevCBlock).entropy,
            &mut (*(*cctx).blockState.nextCBlock).entropy,
            &(*cctx).appliedParams,
            (*cctx).tmpWorkspace,
            (*cctx).tmpWkspSize,
            (*cctx).bmi2 != 0,
        )?;
        // Note: the spec forbids for any compressed block to be larger than maximum block size
        if compressedSeqsSize > (*cctx).blockSizeMax {
            compressedSeqsSize = 0;
        }
        literals = &literals[block.litSize..];

        // Note: difficult to check source for RLE block when only Literals are provided,
        // but it could be considered from analyzing the sequence directly

        if compressedSeqsSize == 0 {
            // Sending uncompressed blocks is out of reach, because the source is not provided.
            // In theory, one could use the sequences to regenerate the source, like a decompressor,
            // but it's complex, and memory hungry, killing the purpose of this variant.
            // Current outcome: generate an error code.
            return Err(Error::cannotProduce_uncompressedBlock);
        }

        // Error checking and repcodes update
        ZSTD_blockState_confirmRepcodesAndEntropyTables(&mut (*cctx).blockState);
        (*(*cctx).blockState.prevCBlock)
            .entropy
            .fse
            .offcode_repeatMode
            .require_check();

        // Write block header into beginning of block
        let cBlockHeader = u32::from(lastBlock)
            .wrapping_add((BlockType::Compressed as u32) << 1)
            .wrapping_add((compressedSeqsSize << 3) as u32);
        MEM_writeLE24(op as *mut core::ffi::c_void, cBlockHeader);
        let cBlockSize = ZSTD_BLOCKHEADERSIZE.wrapping_add(compressedSeqsSize);

        cSize = cSize.wrapping_add(cBlockSize);
        op = op.add(cBlockSize);
        dstCapacity = dstCapacity.wrapping_sub(cBlockSize);
        (*cctx).isFirstBlock = 0;

        if lastBlock {
            break;
        }
    }

    if !literals.is_empty() {
        return Err(Error::externalSequences_invalid);
    }
    if remaining != 0 {
        return Err(Error::externalSequences_invalid);
    }

    Ok(cSize)
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_compressSequencesAndLiterals))]
pub unsafe extern "C" fn ZSTD_compressSequencesAndLiterals(
    cctx: *mut ZSTD_CCtx,
    dst: *mut core::ffi::c_void,
    mut dstCapacity: usize,
    inSeqs: *const ZSTD_Sequence,
    inSeqsSize: usize,
    literals: *const core::ffi::c_void,
    litSize: usize,
    litCapacity: usize,
    decompressedSize: usize,
) -> usize {
    let mut op = dst as *mut u8;
    let mut cSize = 0usize;

    // Transparent initialization stage, same as compressStream2()
    if litCapacity < litSize {
        return Error::workSpace_tooSmall.to_error_code();
    }
    if let Err(err) = ZSTD_CCtx_init_compressStream2(cctx, ZSTD_e_end, decompressedSize) {
        return err.to_error_code();
    }

    if (*cctx).appliedParams.blockDelimiters == ZSTD_sf_noBlockDelimiters {
        return Error::frameParameter_unsupported.to_error_code();
    }
    if (*cctx).appliedParams.validateSequences != 0 {
        return Error::parameter_unsupported.to_error_code();
    }
    if (*cctx).appliedParams.fParams.checksumFlag != 0 {
        return Error::frameParameter_unsupported.to_error_code();
    }

    // Begin writing output, starting with frame header
    let frameHeaderSize = match ZSTD_writeFrameHeader(
        op as *mut core::ffi::c_void,
        dstCapacity,
        &(*cctx).appliedParams,
        decompressedSize as u64,
        (*cctx).dictID,
    ) {
        Ok(size) => size,
        Err(err) => return err.to_error_code(),
    };
    op = op.add(frameHeaderSize);
    dstCapacity = dstCapacity.wrapping_sub(frameHeaderSize);
    cSize = cSize.wrapping_add(frameHeaderSize);

    // Now generate compressed blocks
    let literals = if literals.is_null() || litSize == 0 {
        &[]
    } else {
        core::slice::from_raw_parts(literals.cast::<u8>(), litSize)
    };
    let inSeqs = if inSeqs.is_null() || inSeqsSize == 0 {
        &[]
    } else {
        core::slice::from_raw_parts(inSeqs, inSeqsSize)
    };
    let cBlocksSize = match ZSTD_compressSequencesAndLiterals_internal(
        cctx,
        op as *mut core::ffi::c_void,
        dstCapacity,
        inSeqs,
        literals,
        decompressedSize,
    ) {
        Ok(cBlocksSize) => cBlocksSize,
        Err(err) => return err.to_error_code(),
    };
    cSize = cSize.wrapping_add(cBlocksSize);

    cSize
}

unsafe fn inBuffer_forEndFlush(zcs: *const ZSTD_CStream) -> ZSTD_inBuffer {
    let stableInput = (*zcs).appliedParams.inBufferMode == ZSTD_bm_stable;
    if stableInput {
        (*zcs).expectedInBuffer
    } else {
        ZSTD_inBuffer_s::default()
    }
}

/// # Returns
///
/// amount of data remaining to flush
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_flushStream))]
pub unsafe extern "C" fn ZSTD_flushStream(
    zcs: *mut ZSTD_CStream,
    output: *mut ZSTD_outBuffer,
) -> usize {
    let mut input = inBuffer_forEndFlush(zcs);
    input.size = input.pos; // do not ingest more input during flush
    ZSTD_compressStream2(zcs, output, &mut input, ZSTD_e_flush)
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_endStream))]
pub unsafe extern "C" fn ZSTD_endStream(
    zcs: *mut ZSTD_CStream,
    output: *mut ZSTD_outBuffer,
) -> usize {
    let mut input = inBuffer_forEndFlush(zcs);
    let remainingToFlush = match ZSTD_compressStream2_internal(zcs, output, &mut input, ZSTD_e_end)
    {
        Ok(remaining) => remaining,
        Err(err) => return err.to_error_code(),
    };
    if (*zcs).appliedParams.nbWorkers > 0 {
        return remainingToFlush; // minimal estimation
    }

    // single thread mode: attempt to calculate remaining to flush more precisely
    let lastBlockSize = match (*zcs).frameEnded {
        0 => ZSTD_BLOCKHEADERSIZE,
        _ => 0,
    };
    let checksumSize = match (*zcs).frameEnded {
        0 => ((*zcs).appliedParams.fParams.checksumFlag as usize) * 4,
        _ => 0,
    };

    remainingToFlush
        .wrapping_add(lastBlockSize)
        .wrapping_add(checksumSize)
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_maxCLevel))]
pub const extern "C" fn ZSTD_maxCLevel() -> core::ffi::c_int {
    ZSTD_MAX_CLEVEL
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_minCLevel))]
pub const extern "C" fn ZSTD_minCLevel() -> core::ffi::c_int {
    -ZSTD_TARGETLENGTH_MAX
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_defaultCLevel))]
pub const extern "C" fn ZSTD_defaultCLevel() -> core::ffi::c_int {
    ZSTD_CLEVEL_DEFAULT
}

fn ZSTD_getCParamRowSize(srcSizeHint: u64, mut dictSize: usize, mode: CParamMode) -> u64 {
    if mode == CParamMode::AttachDict {
        dictSize = 0;
    }

    let unknown = srcSizeHint as core::ffi::c_ulonglong == ZSTD_CONTENTSIZE_UNKNOWN;
    let addedSize = if unknown && dictSize > 0 { 500 } else { 0 };
    if unknown && dictSize == 0 {
        ZSTD_CONTENTSIZE_UNKNOWN
    } else {
        srcSizeHint
            .wrapping_add(dictSize as u64)
            .wrapping_add(addedSize)
    }
}

/// Size values are optional, provide 0 if not known or unused.
///
/// # Returns
///
/// `ZSTD_compressionParameters` structure for a selected compression level, srcSize and dictSize.
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_getCParams))]
pub extern "C" fn ZSTD_getCParams(
    compressionLevel: core::ffi::c_int,
    mut srcSizeHint: core::ffi::c_ulonglong,
    dictSize: usize,
) -> ZSTD_compressionParameters {
    if srcSizeHint == 0 {
        srcSizeHint = ZSTD_CONTENTSIZE_UNKNOWN;
    }
    ZSTD_compressionParameters::new(compressionLevel, srcSizeHint, dictSize, CParamMode::Unknown)
}

/// Same idea as ZSTD_getCParams().
/// Fields of `ZSTD_frameParameters` are set to default values.
///
/// # Returns
///
/// a `ZSTD_parameters` structure (instead of `ZSTD_compressionParameters`).
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_getParams))]
pub extern "C" fn ZSTD_getParams(
    compressionLevel: core::ffi::c_int,
    mut srcSizeHint: core::ffi::c_ulonglong,
    dictSize: usize,
) -> ZSTD_parameters {
    if srcSizeHint == 0 {
        srcSizeHint = ZSTD_CONTENTSIZE_UNKNOWN;
    }
    ZSTD_parameters::new(compressionLevel, srcSizeHint, dictSize, CParamMode::Unknown)
}

pub const ZSTD_MAX_CLEVEL: core::ffi::c_int = 22;

static ZSTD_defaultCParameters: [[ZSTD_compressionParameters; 23]; 4] = [
    // "default" - for any srcSize > 256 KB
    [
        {
            ZSTD_compressionParameters {
                windowLog: 19,
                chainLog: 12,
                hashLog: 13,
                searchLog: 1,
                minMatch: 6,
                targetLength: 1,
                strategy: ZSTD_fast,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 19,
                chainLog: 13,
                hashLog: 14,
                searchLog: 1,
                minMatch: 7,
                targetLength: 0,
                strategy: ZSTD_fast,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 20,
                chainLog: 15,
                hashLog: 16,
                searchLog: 1,
                minMatch: 6,
                targetLength: 0,
                strategy: ZSTD_fast,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 21,
                chainLog: 16,
                hashLog: 17,
                searchLog: 1,
                minMatch: 5,
                targetLength: 0,
                strategy: ZSTD_dfast,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 21,
                chainLog: 18,
                hashLog: 18,
                searchLog: 1,
                minMatch: 5,
                targetLength: 0,
                strategy: ZSTD_dfast,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 21,
                chainLog: 18,
                hashLog: 19,
                searchLog: 3,
                minMatch: 5,
                targetLength: 2,
                strategy: ZSTD_greedy,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 21,
                chainLog: 18,
                hashLog: 19,
                searchLog: 3,
                minMatch: 5,
                targetLength: 4,
                strategy: ZSTD_lazy,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 21,
                chainLog: 19,
                hashLog: 20,
                searchLog: 4,
                minMatch: 5,
                targetLength: 8,
                strategy: ZSTD_lazy,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 21,
                chainLog: 19,
                hashLog: 20,
                searchLog: 4,
                minMatch: 5,
                targetLength: 16,
                strategy: ZSTD_lazy2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 22,
                chainLog: 20,
                hashLog: 21,
                searchLog: 4,
                minMatch: 5,
                targetLength: 16,
                strategy: ZSTD_lazy2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 22,
                chainLog: 21,
                hashLog: 22,
                searchLog: 5,
                minMatch: 5,
                targetLength: 16,
                strategy: ZSTD_lazy2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 22,
                chainLog: 21,
                hashLog: 22,
                searchLog: 6,
                minMatch: 5,
                targetLength: 16,
                strategy: ZSTD_lazy2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 22,
                chainLog: 22,
                hashLog: 23,
                searchLog: 6,
                minMatch: 5,
                targetLength: 32,
                strategy: ZSTD_lazy2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 22,
                chainLog: 22,
                hashLog: 22,
                searchLog: 4,
                minMatch: 5,
                targetLength: 32,
                strategy: ZSTD_btlazy2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 22,
                chainLog: 22,
                hashLog: 23,
                searchLog: 5,
                minMatch: 5,
                targetLength: 32,
                strategy: ZSTD_btlazy2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 22,
                chainLog: 23,
                hashLog: 23,
                searchLog: 6,
                minMatch: 5,
                targetLength: 32,
                strategy: ZSTD_btlazy2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 22,
                chainLog: 22,
                hashLog: 22,
                searchLog: 5,
                minMatch: 5,
                targetLength: 48,
                strategy: ZSTD_btopt,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 23,
                chainLog: 23,
                hashLog: 22,
                searchLog: 5,
                minMatch: 4,
                targetLength: 64,
                strategy: ZSTD_btopt,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 23,
                chainLog: 23,
                hashLog: 22,
                searchLog: 6,
                minMatch: 3,
                targetLength: 64,
                strategy: ZSTD_btultra,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 23,
                chainLog: 24,
                hashLog: 22,
                searchLog: 7,
                minMatch: 3,
                targetLength: 256,
                strategy: ZSTD_btultra2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 25,
                chainLog: 25,
                hashLog: 23,
                searchLog: 7,
                minMatch: 3,
                targetLength: 256,
                strategy: ZSTD_btultra2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 26,
                chainLog: 26,
                hashLog: 24,
                searchLog: 7,
                minMatch: 3,
                targetLength: 512,
                strategy: ZSTD_btultra2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 27,
                chainLog: 27,
                hashLog: 25,
                searchLog: 9,
                minMatch: 3,
                targetLength: 999,
                strategy: ZSTD_btultra2,
            }
        },
    ],
    // for srcSize <= 256 KB
    [
        {
            ZSTD_compressionParameters {
                windowLog: 18,
                chainLog: 12,
                hashLog: 13,
                searchLog: 1,
                minMatch: 5,
                targetLength: 1,
                strategy: ZSTD_fast,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 18,
                chainLog: 13,
                hashLog: 14,
                searchLog: 1,
                minMatch: 6,
                targetLength: 0,
                strategy: ZSTD_fast,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 18,
                chainLog: 14,
                hashLog: 14,
                searchLog: 1,
                minMatch: 5,
                targetLength: 0,
                strategy: ZSTD_dfast,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 18,
                chainLog: 16,
                hashLog: 16,
                searchLog: 1,
                minMatch: 4,
                targetLength: 0,
                strategy: ZSTD_dfast,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 18,
                chainLog: 16,
                hashLog: 17,
                searchLog: 3,
                minMatch: 5,
                targetLength: 2,
                strategy: ZSTD_greedy,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 18,
                chainLog: 17,
                hashLog: 18,
                searchLog: 5,
                minMatch: 5,
                targetLength: 2,
                strategy: ZSTD_greedy,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 18,
                chainLog: 18,
                hashLog: 19,
                searchLog: 3,
                minMatch: 5,
                targetLength: 4,
                strategy: ZSTD_lazy,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 18,
                chainLog: 18,
                hashLog: 19,
                searchLog: 4,
                minMatch: 4,
                targetLength: 4,
                strategy: ZSTD_lazy,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 18,
                chainLog: 18,
                hashLog: 19,
                searchLog: 4,
                minMatch: 4,
                targetLength: 8,
                strategy: ZSTD_lazy2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 18,
                chainLog: 18,
                hashLog: 19,
                searchLog: 5,
                minMatch: 4,
                targetLength: 8,
                strategy: ZSTD_lazy2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 18,
                chainLog: 18,
                hashLog: 19,
                searchLog: 6,
                minMatch: 4,
                targetLength: 8,
                strategy: ZSTD_lazy2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 18,
                chainLog: 18,
                hashLog: 19,
                searchLog: 5,
                minMatch: 4,
                targetLength: 12,
                strategy: ZSTD_btlazy2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 18,
                chainLog: 19,
                hashLog: 19,
                searchLog: 7,
                minMatch: 4,
                targetLength: 12,
                strategy: ZSTD_btlazy2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 18,
                chainLog: 18,
                hashLog: 19,
                searchLog: 4,
                minMatch: 4,
                targetLength: 16,
                strategy: ZSTD_btopt,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 18,
                chainLog: 18,
                hashLog: 19,
                searchLog: 4,
                minMatch: 3,
                targetLength: 32,
                strategy: ZSTD_btopt,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 18,
                chainLog: 18,
                hashLog: 19,
                searchLog: 6,
                minMatch: 3,
                targetLength: 128,
                strategy: ZSTD_btopt,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 18,
                chainLog: 19,
                hashLog: 19,
                searchLog: 6,
                minMatch: 3,
                targetLength: 128,
                strategy: ZSTD_btultra,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 18,
                chainLog: 19,
                hashLog: 19,
                searchLog: 8,
                minMatch: 3,
                targetLength: 256,
                strategy: ZSTD_btultra,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 18,
                chainLog: 19,
                hashLog: 19,
                searchLog: 6,
                minMatch: 3,
                targetLength: 128,
                strategy: ZSTD_btultra2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 18,
                chainLog: 19,
                hashLog: 19,
                searchLog: 8,
                minMatch: 3,
                targetLength: 256,
                strategy: ZSTD_btultra2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 18,
                chainLog: 19,
                hashLog: 19,
                searchLog: 10,
                minMatch: 3,
                targetLength: 512,
                strategy: ZSTD_btultra2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 18,
                chainLog: 19,
                hashLog: 19,
                searchLog: 12,
                minMatch: 3,
                targetLength: 512,
                strategy: ZSTD_btultra2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 18,
                chainLog: 19,
                hashLog: 19,
                searchLog: 13,
                minMatch: 3,
                targetLength: 999,
                strategy: ZSTD_btultra2,
            }
        },
    ],
    // for srcSize <= 128 KB
    [
        {
            ZSTD_compressionParameters {
                windowLog: 17,
                chainLog: 12,
                hashLog: 12,
                searchLog: 1,
                minMatch: 5,
                targetLength: 1,
                strategy: ZSTD_fast,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 17,
                chainLog: 12,
                hashLog: 13,
                searchLog: 1,
                minMatch: 6,
                targetLength: 0,
                strategy: ZSTD_fast,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 17,
                chainLog: 13,
                hashLog: 15,
                searchLog: 1,
                minMatch: 5,
                targetLength: 0,
                strategy: ZSTD_fast,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 17,
                chainLog: 15,
                hashLog: 16,
                searchLog: 2,
                minMatch: 5,
                targetLength: 0,
                strategy: ZSTD_dfast,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 17,
                chainLog: 17,
                hashLog: 17,
                searchLog: 2,
                minMatch: 4,
                targetLength: 0,
                strategy: ZSTD_dfast,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 17,
                chainLog: 16,
                hashLog: 17,
                searchLog: 3,
                minMatch: 4,
                targetLength: 2,
                strategy: ZSTD_greedy,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 17,
                chainLog: 16,
                hashLog: 17,
                searchLog: 3,
                minMatch: 4,
                targetLength: 4,
                strategy: ZSTD_lazy,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 17,
                chainLog: 16,
                hashLog: 17,
                searchLog: 3,
                minMatch: 4,
                targetLength: 8,
                strategy: ZSTD_lazy2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 17,
                chainLog: 16,
                hashLog: 17,
                searchLog: 4,
                minMatch: 4,
                targetLength: 8,
                strategy: ZSTD_lazy2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 17,
                chainLog: 16,
                hashLog: 17,
                searchLog: 5,
                minMatch: 4,
                targetLength: 8,
                strategy: ZSTD_lazy2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 17,
                chainLog: 16,
                hashLog: 17,
                searchLog: 6,
                minMatch: 4,
                targetLength: 8,
                strategy: ZSTD_lazy2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 17,
                chainLog: 17,
                hashLog: 17,
                searchLog: 5,
                minMatch: 4,
                targetLength: 8,
                strategy: ZSTD_btlazy2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 17,
                chainLog: 18,
                hashLog: 17,
                searchLog: 7,
                minMatch: 4,
                targetLength: 12,
                strategy: ZSTD_btlazy2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 17,
                chainLog: 18,
                hashLog: 17,
                searchLog: 3,
                minMatch: 4,
                targetLength: 12,
                strategy: ZSTD_btopt,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 17,
                chainLog: 18,
                hashLog: 17,
                searchLog: 4,
                minMatch: 3,
                targetLength: 32,
                strategy: ZSTD_btopt,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 17,
                chainLog: 18,
                hashLog: 17,
                searchLog: 6,
                minMatch: 3,
                targetLength: 256,
                strategy: ZSTD_btopt,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 17,
                chainLog: 18,
                hashLog: 17,
                searchLog: 6,
                minMatch: 3,
                targetLength: 128,
                strategy: ZSTD_btultra,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 17,
                chainLog: 18,
                hashLog: 17,
                searchLog: 8,
                minMatch: 3,
                targetLength: 256,
                strategy: ZSTD_btultra,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 17,
                chainLog: 18,
                hashLog: 17,
                searchLog: 10,
                minMatch: 3,
                targetLength: 512,
                strategy: ZSTD_btultra,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 17,
                chainLog: 18,
                hashLog: 17,
                searchLog: 5,
                minMatch: 3,
                targetLength: 256,
                strategy: ZSTD_btultra2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 17,
                chainLog: 18,
                hashLog: 17,
                searchLog: 7,
                minMatch: 3,
                targetLength: 512,
                strategy: ZSTD_btultra2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 17,
                chainLog: 18,
                hashLog: 17,
                searchLog: 9,
                minMatch: 3,
                targetLength: 512,
                strategy: ZSTD_btultra2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 17,
                chainLog: 18,
                hashLog: 17,
                searchLog: 11,
                minMatch: 3,
                targetLength: 999,
                strategy: ZSTD_btultra2,
            }
        },
    ],
    // for srcSize <= 16 KB
    [
        {
            ZSTD_compressionParameters {
                windowLog: 14,
                chainLog: 12,
                hashLog: 13,
                searchLog: 1,
                minMatch: 5,
                targetLength: 1,
                strategy: ZSTD_fast,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 14,
                chainLog: 14,
                hashLog: 15,
                searchLog: 1,
                minMatch: 5,
                targetLength: 0,
                strategy: ZSTD_fast,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 14,
                chainLog: 14,
                hashLog: 15,
                searchLog: 1,
                minMatch: 4,
                targetLength: 0,
                strategy: ZSTD_fast,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 14,
                chainLog: 14,
                hashLog: 15,
                searchLog: 2,
                minMatch: 4,
                targetLength: 0,
                strategy: ZSTD_dfast,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 14,
                chainLog: 14,
                hashLog: 14,
                searchLog: 4,
                minMatch: 4,
                targetLength: 2,
                strategy: ZSTD_greedy,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 14,
                chainLog: 14,
                hashLog: 14,
                searchLog: 3,
                minMatch: 4,
                targetLength: 4,
                strategy: ZSTD_lazy,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 14,
                chainLog: 14,
                hashLog: 14,
                searchLog: 4,
                minMatch: 4,
                targetLength: 8,
                strategy: ZSTD_lazy2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 14,
                chainLog: 14,
                hashLog: 14,
                searchLog: 6,
                minMatch: 4,
                targetLength: 8,
                strategy: ZSTD_lazy2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 14,
                chainLog: 14,
                hashLog: 14,
                searchLog: 8,
                minMatch: 4,
                targetLength: 8,
                strategy: ZSTD_lazy2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 14,
                chainLog: 15,
                hashLog: 14,
                searchLog: 5,
                minMatch: 4,
                targetLength: 8,
                strategy: ZSTD_btlazy2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 14,
                chainLog: 15,
                hashLog: 14,
                searchLog: 9,
                minMatch: 4,
                targetLength: 8,
                strategy: ZSTD_btlazy2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 14,
                chainLog: 15,
                hashLog: 14,
                searchLog: 3,
                minMatch: 4,
                targetLength: 12,
                strategy: ZSTD_btopt,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 14,
                chainLog: 15,
                hashLog: 14,
                searchLog: 4,
                minMatch: 3,
                targetLength: 24,
                strategy: ZSTD_btopt,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 14,
                chainLog: 15,
                hashLog: 14,
                searchLog: 5,
                minMatch: 3,
                targetLength: 32,
                strategy: ZSTD_btultra,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 14,
                chainLog: 15,
                hashLog: 15,
                searchLog: 6,
                minMatch: 3,
                targetLength: 64,
                strategy: ZSTD_btultra,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 14,
                chainLog: 15,
                hashLog: 15,
                searchLog: 7,
                minMatch: 3,
                targetLength: 256,
                strategy: ZSTD_btultra,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 14,
                chainLog: 15,
                hashLog: 15,
                searchLog: 5,
                minMatch: 3,
                targetLength: 48,
                strategy: ZSTD_btultra2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 14,
                chainLog: 15,
                hashLog: 15,
                searchLog: 6,
                minMatch: 3,
                targetLength: 128,
                strategy: ZSTD_btultra2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 14,
                chainLog: 15,
                hashLog: 15,
                searchLog: 7,
                minMatch: 3,
                targetLength: 256,
                strategy: ZSTD_btultra2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 14,
                chainLog: 15,
                hashLog: 15,
                searchLog: 8,
                minMatch: 3,
                targetLength: 256,
                strategy: ZSTD_btultra2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 14,
                chainLog: 15,
                hashLog: 15,
                searchLog: 8,
                minMatch: 3,
                targetLength: 512,
                strategy: ZSTD_btultra2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 14,
                chainLog: 15,
                hashLog: 15,
                searchLog: 9,
                minMatch: 3,
                targetLength: 512,
                strategy: ZSTD_btultra2,
            }
        },
        {
            ZSTD_compressionParameters {
                windowLog: 14,
                chainLog: 15,
                hashLog: 15,
                searchLog: 10,
                minMatch: 3,
                targetLength: 999,
                strategy: ZSTD_btultra2,
            }
        },
    ],
];

impl ZSTD_compressionParameters {
    /// Get `ZSTD_compressionParameters` for a selected compression level, srcSize and dictSize.
    ///
    /// - `srcSizeHint == 0` means 0, use ZSTD_CONTENTSIZE_UNKNOWN for unknown
    /// - `dictSize == 0` does mean unknown or unused
    /// - `mode` controls how we treat the `dictSize`, see docs for [`CParamMode`]
    fn new(
        compressionLevel: core::ffi::c_int,
        srcSizeHint: core::ffi::c_ulonglong,
        dictSize: usize,
        mode: CParamMode,
    ) -> Self {
        let rSize = ZSTD_getCParamRowSize(srcSizeHint, dictSize, mode);
        let tableID = u32::from(rSize <= (256 * (1 << 10)))
            + u32::from(rSize <= (128 * (1 << 10)))
            + u32::from(rSize <= (16 * (1 << 10)));

        let row = if compressionLevel == 0 {
            ZSTD_CLEVEL_DEFAULT
        } else {
            compressionLevel.clamp(0, ZSTD_MAX_CLEVEL) // entry 0 is baseline for fast mode
        };

        let mut cp = ZSTD_defaultCParameters[tableID as usize][row as usize];
        // acceleration factor
        if compressionLevel < 0 {
            let clampedCompressionLevel = ZSTD_minCLevel().max(compressionLevel);
            cp.targetLength = -clampedCompressionLevel as core::ffi::c_uint;
        }

        // refine parameters based on srcSize & dictSize
        cp.optimize(srcSizeHint, dictSize, mode, ParamSwitch::Auto)
    }

    /// Same as [`Self::new`], but initialized for dedicated dict search.
    fn new_for_dds(
        compressionLevel: core::ffi::c_int,
        dictSize: usize,
    ) -> ZSTD_compressionParameters {
        let mut cParams =
            ZSTD_compressionParameters::new(compressionLevel, 0, dictSize, CParamMode::CreateCDict);
        if (ZSTD_greedy..=ZSTD_lazy2).contains(&cParams.strategy) {
            cParams.hashLog = cParams.hashLog.wrapping_add(ZSTD_LAZY_DDSS_BUCKET_LOG);
        }
        cParams
    }
}

impl ZSTD_parameters {
    /// Get `ZSTD_parameters` for a selected compression level, srcSize and dictSize.
    /// Fields of `ZSTD_frameParameters` are set to default values.
    ///
    /// - `srcSizeHint == 0` means 0, use ZSTD_CONTENTSIZE_UNKNOWN for unknown
    /// - `dictSize == 0` does mean unknown or unused
    /// - `mode` controls how we treat the `dictSize`, see docs for [`CParamMode`]
    fn new(
        compressionLevel: core::ffi::c_int,
        srcSizeHint: core::ffi::c_ulonglong,
        dictSize: usize,
        mode: CParamMode,
    ) -> Self {
        ZSTD_parameters {
            cParams: ZSTD_compressionParameters::new(compressionLevel, srcSizeHint, dictSize, mode),
            fParams: ZSTD_frameParameters {
                contentSizeFlag: 1,
                checksumFlag: 0,
                noDictIDFlag: 0,
            },
        }
    }
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_registerSequenceProducer))]
pub unsafe extern "C" fn ZSTD_registerSequenceProducer(
    zc: *mut ZSTD_CCtx,
    extSeqProdState: *mut core::ffi::c_void,
    extSeqProdFunc: Option<ZSTD_sequenceProducer_F>,
) {
    ZSTD_CCtxParams_registerSequenceProducer(
        &mut (*zc).requestedParams,
        extSeqProdState,
        extSeqProdFunc,
    );
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZSTD_CCtxParams_registerSequenceProducer))]
pub unsafe extern "C" fn ZSTD_CCtxParams_registerSequenceProducer(
    params: *mut ZSTD_CCtx_params,
    extSeqProdState: *mut core::ffi::c_void,
    extSeqProdFunc: Option<ZSTD_sequenceProducer_F>,
) {
    if extSeqProdFunc.is_some() {
        (*params).extSeqProdFunc = extSeqProdFunc;
        (*params).extSeqProdState = extSeqProdState;
    } else {
        (*params).extSeqProdFunc = None;
        (*params).extSeqProdState = core::ptr::null_mut();
    }
}
