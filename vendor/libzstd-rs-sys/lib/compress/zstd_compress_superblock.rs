use crate::lib::common::bitstream::STREAM_ACCUMULATOR_MIN;
use crate::lib::common::error_private::Error;
use crate::lib::common::fse::FSE_CTable;
use crate::lib::common::huf::{CTable, HUF_flags_bmi2, HUF_SYMBOLVALUE_MAX};
use crate::lib::common::mem::{MEM_writeLE16, MEM_writeLE24, MEM_writeLE32};
use crate::lib::common::zstd_internal::{
    BlockType, DefaultMaxOff, LL_bits, LL_defaultNorm, LL_defaultNormLog, ML_bits, ML_defaultNorm,
    ML_defaultNormLog, MaxLL, MaxML, MaxOff, OF_defaultNorm, OF_defaultNormLog, SymbolEncodingType,
    LONGNBSEQ, MINMATCH, ZSTD_BLOCKHEADERSIZE,
};
use crate::lib::compress::hist::{HIST_countFast_wksp, HIST_count_wksp};
use crate::lib::compress::huf_compress::{
    HUF_compress1X_usingCTable, HUF_compress4X_usingCTable, HUF_estimateCompressedSize,
};
use crate::lib::compress::zstd_compress::{
    SeqDef, SeqStore_t, ZSTD_CCtx, ZSTD_CCtx_params, ZSTD_buildBlockEntropyStats,
    ZSTD_compressedBlockState_t, ZSTD_entropyCTablesMetadata_t, ZSTD_fseCTablesMetadata_t,
    ZSTD_hufCTablesMetadata_t,
};
use crate::lib::compress::zstd_compress_internal::{
    ZSTD_entropyCTables_t, ZSTD_fseCTables_t, ZSTD_getSequenceLength, ZSTD_hufCTables_t,
    ZSTD_noCompressBlock, ZSTD_updateRep,
};
use crate::lib::compress::zstd_compress_literals::{
    ZSTD_compressRleLiteralsBlock, ZSTD_noCompressLiterals,
};
use crate::lib::compress::zstd_compress_sequences::{
    ZSTD_crossEntropyCost, ZSTD_encodeSequences, ZSTD_fseBitCost,
};
use crate::lib::zstd::ZSTD_TARGETCBLOCKSIZE_MIN;

#[derive(Copy, Clone)]
#[repr(C)]
pub struct EstimatedBlockSize {
    pub estLitSize: usize,
    pub estBlockSize: usize,
}

/// Compresses literals section for a sub-block.
/// When we have to write the Huffman table we will sometimes choose a header
/// size larger than necessary. This is because we have to pick the header size
/// before we know the table size + compressed size, so we have a bound on the
/// table size. If we guessed incorrectly, we fall back to uncompressed literals.
///
/// We write the header when writeEntropy=1 and set entropyWritten=1 when we succeeded
/// in writing the header, otherwise it is set to 0.
///
/// hufMetadata->hType has literals block type info.
///     If it is [`SymbolEncodingType::Basic`], all sub-blocks literals section will be Raw_Literals_Block.
///     If it is [`SymbolEncodingType::Rle`], all sub-blocks literals section will be RLE_Literals_Block.
///     If it is [`SymbolEncodingType::Compressed`], first sub-block's literals section will be Compressed_Literals_Block
///     If it is [`SymbolEncodingType::Compressed`], first sub-block's literals section will be Treeless_Literals_Block
///     and the following sub-blocks' literals sections will be Treeless_Literals_Block.
///
/// # Returns
///
/// - The compressed size of literals section of a sub-block
/// - Or 0 if unable to compress
/// - Or an error code
unsafe fn ZSTD_compressSubBlock_literal(
    hufTable: &CTable,
    hufMetadata: &ZSTD_hufCTablesMetadata_t,
    literals: &[u8],
    dst: *mut core::ffi::c_void,
    dstSize: usize,
    bmi2: bool,
    writeEntropy: bool,
    entropyWritten: &mut bool,
) -> Result<usize, Error> {
    let header = (if writeEntropy { 200 } else { 0 }) as usize;
    let lhSize = 3
        + usize::from(literals.len() >= (1usize << 10).wrapping_sub(header))
        + usize::from(literals.len() >= (16usize * (1 << 10)).wrapping_sub(header));
    let ostart = dst as *mut u8;
    let oend = ostart.add(dstSize);
    let mut op = ostart.add(lhSize);
    let singleStream = lhSize == 3;
    let hType = if writeEntropy {
        hufMetadata.hType
    } else {
        SymbolEncodingType::Repeat
    };
    let mut cLitSize = 0usize;

    *entropyWritten = false;
    if literals.is_empty() || hufMetadata.hType == SymbolEncodingType::Basic {
        return ZSTD_noCompressLiterals(dst, dstSize, literals);
    } else if hufMetadata.hType == SymbolEncodingType::Rle {
        return ZSTD_compressRleLiteralsBlock(dst, dstSize, literals);
    }

    if writeEntropy && hufMetadata.hType == SymbolEncodingType::Compressed {
        core::ptr::copy_nonoverlapping(
            hufMetadata.hufDesBuffer.as_ptr(),
            op,
            hufMetadata.hufDesSize,
        );
        op = op.add(hufMetadata.hufDesSize);
        cLitSize = cLitSize.wrapping_add(hufMetadata.hufDesSize);
    }

    let flags = if bmi2 {
        HUF_flags_bmi2 as core::ffi::c_int
    } else {
        0
    };
    let cSize = if singleStream {
        HUF_compress1X_usingCTable(
            op as *mut core::ffi::c_void,
            oend.offset_from_unsigned(op),
            literals,
            hufTable,
            flags,
        )
    } else {
        HUF_compress4X_usingCTable(
            op as *mut core::ffi::c_void,
            oend.offset_from_unsigned(op),
            literals,
            hufTable,
            flags,
        )
    };
    let cSize = match cSize {
        Ok(0) | Err(_) => return Ok(0),
        Ok(cSize) => cSize,
    };
    op = op.add(cSize);
    cLitSize = cLitSize.wrapping_add(cSize);
    // If we expand and we aren't writing a header then emit uncompressed.
    if !writeEntropy && cLitSize >= literals.len() {
        return ZSTD_noCompressLiterals(dst, dstSize, literals);
    }
    // If we are writing headers then allow expansion that doesn't change our header size.
    if lhSize < 3 + usize::from(cLitSize >= (1 << 10)) + usize::from(cLitSize >= (16 * (1 << 10))) {
        return ZSTD_noCompressLiterals(dst, dstSize, literals);
    }

    // Build header
    match lhSize {
        3 => {
            // 2 - 2 - 10 - 10
            let lhc = (hType as core::ffi::c_uint)
                .wrapping_add(((!singleStream) as core::ffi::c_int as u32) << 2)
                .wrapping_add((literals.len() as u32) << 4)
                .wrapping_add((cLitSize as u32) << 14);
            MEM_writeLE24(ostart as *mut core::ffi::c_void, lhc);
        }
        4 => {
            // 2 - 2 - 14 - 14
            let lhc_0 = (hType as core::ffi::c_uint)
                .wrapping_add(2 << 2)
                .wrapping_add((literals.len() as u32) << 4)
                .wrapping_add((cLitSize as u32) << 18);
            MEM_writeLE32(ostart as *mut core::ffi::c_void, lhc_0);
        }
        5 => {
            // 2 - 2 - 18 - 18
            let lhc_1 = (hType as core::ffi::c_uint)
                .wrapping_add(3 << 2)
                .wrapping_add((literals.len() as u32) << 4)
                .wrapping_add((cLitSize as u32) << 22);
            MEM_writeLE32(ostart as *mut core::ffi::c_void, lhc_1);
            *ostart.add(4) = (cLitSize >> 10) as u8;
        }
        _ => {} // not possible : lhSize is {3,4,5}
    }
    *entropyWritten = true;
    Ok(op.offset_from_unsigned(ostart))
}

fn ZSTD_seqDecompressedSize(
    seqStore: &SeqStore_t,
    sequences: &[SeqDef],
    litSize: usize,
    lastSubBlock: bool,
) -> usize {
    let mut matchLengthSum = 0usize;
    let mut litLengthSum = 0usize;
    for seq in sequences {
        let seqLen = ZSTD_getSequenceLength(seqStore, seq);
        litLengthSum = litLengthSum.wrapping_add(seqLen.litLength as usize);
        matchLengthSum = matchLengthSum.wrapping_add(seqLen.matchLength as usize);
    }

    if !lastSubBlock {
        assert_eq!(litLengthSum, litSize);
    } else {
        assert!(litLengthSum <= litSize);
    }

    matchLengthSum.wrapping_add(litSize)
}

/// Compresses sequences section for a sub-block.
/// fseMetadata->llType, fseMetadata->ofType, and fseMetadata->mlType have
/// symbol compression modes for the super-block.
/// The first successfully compressed block will have these in its header.
/// We set entropyWritten=1 when we succeed in compressing the sequences.
/// The following sub-blocks will always have repeat mode.
///
/// # Returns
///
/// - The compressed size of sequences section of a sub-block
/// - Or 0 if it is unable to compress
/// - Or error code.
unsafe fn ZSTD_compressSubBlock_sequences(
    fseTables: &ZSTD_fseCTables_t,
    fseMetadata: &ZSTD_fseCTablesMetadata_t,
    sequences: &[SeqDef],
    llCode: &[u8],
    mlCode: &[u8],
    ofCode: &[u8],
    cctxParams: &ZSTD_CCtx_params,
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    bmi2: bool,
    writeEntropy: bool,
    entropyWritten: &mut bool,
) -> Result<usize, Error> {
    let nbSeq = sequences.len();
    let longOffsets = cctxParams.cParams.windowLog > STREAM_ACCUMULATOR_MIN;
    let ostart = dst as *mut u8;
    let oend = ostart.add(dstCapacity);
    let mut op = ostart;

    *entropyWritten = false;
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
        return Ok(op.offset_from_unsigned(ostart));
    }

    // seqHead : flags for FSE encoding type
    let seqHead = op;
    op = op.add(1);

    if writeEntropy {
        let LLtype = fseMetadata.llType as u32;
        let Offtype = fseMetadata.ofType as u32;
        let MLtype = fseMetadata.mlType as u32;
        *seqHead = (LLtype << 6)
            .wrapping_add(Offtype << 4)
            .wrapping_add(MLtype << 2) as u8;
        core::ptr::copy_nonoverlapping(
            fseMetadata.fseTablesBuffer.as_ptr(),
            op,
            fseMetadata.fseTablesSize,
        );
        op = op.add(fseMetadata.fseTablesSize);
    } else {
        let repeat = SymbolEncodingType::Repeat as u32;
        *seqHead = (repeat << 6)
            .wrapping_add(repeat << 4)
            .wrapping_add(repeat << 2) as u8;
    }

    let bitstreamSize = ZSTD_encodeSequences(
        op as *mut core::ffi::c_void,
        oend.offset_from_unsigned(op),
        &fseTables.matchlengthCTable,
        mlCode,
        &fseTables.offcodeCTable,
        ofCode,
        &fseTables.litlengthCTable,
        llCode,
        sequences,
        longOffsets,
        bmi2,
    )?;
    op = op.add(bitstreamSize);
    // zstd versions <= 1.3.4 mistakenly report corruption when
    // FSE_readNCount() receives a buffer < 4 bytes.
    // Fixed by https://github.com/facebook/zstd/pull/1146.
    // This can happen when the last SymbolEncodingType::Compressed table
    // present is 2 bytes and the bitstream is only one byte.
    // In this exceedingly rare case, we will simply emit an uncompressed
    // block, since it isn't worth optimizing.
    if writeEntropy
        && fseMetadata.lastCountSize != 0
        && fseMetadata.lastCountSize.wrapping_add(bitstreamSize) < 4
    {
        return Ok(0);
    }

    // zstd versions <= 1.4.0 mistakenly report error when
    // sequences section body size is less than 3 bytes.
    // Fixed by https://github.com/facebook/zstd/pull/1664.
    // This can happen when the previous sequences section block is compressed
    // with rle mode and the current block's sequences section is compressed
    // with repeat mode where sequences section body size can be 1 byte.
    if op.offset_from_unsigned(seqHead) < 4 {
        return Ok(0);
    }

    *entropyWritten = true;
    Ok(op.offset_from_unsigned(ostart))
}

/// Compresses a single sub-block.
///
/// # Returns
///
/// - The compressed size of the sub-block
/// - Or 0 if it failed to compress.
unsafe fn ZSTD_compressSubBlock(
    entropy: &ZSTD_entropyCTables_t,
    entropyMetadata: &ZSTD_entropyCTablesMetadata_t,
    sequences: &[SeqDef],
    literals: &[u8],
    llCode: &[u8],
    mlCode: &[u8],
    ofCode: &[u8],
    cctxParams: &ZSTD_CCtx_params,
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    bmi2: bool,
    writeLitEntropy: bool,
    writeSeqEntropy: bool,
    litEntropyWritten: &mut bool,
    seqEntropyWritten: &mut bool,
    lastBlock: bool,
) -> Result<usize, Error> {
    let ostart = dst as *mut u8;
    let oend = ostart.add(dstCapacity);
    let mut op = ostart.add(ZSTD_BLOCKHEADERSIZE);

    let cLitSize = ZSTD_compressSubBlock_literal(
        &entropy.huf.CTable,
        &entropyMetadata.hufMetadata,
        literals,
        op as *mut core::ffi::c_void,
        oend.offset_from_unsigned(op),
        bmi2,
        writeLitEntropy,
        litEntropyWritten,
    )?;
    if cLitSize == 0 {
        return Ok(0);
    }
    op = op.add(cLitSize);

    let cSeqSize = ZSTD_compressSubBlock_sequences(
        &entropy.fse,
        &entropyMetadata.fseMetadata,
        sequences,
        llCode,
        mlCode,
        ofCode,
        cctxParams,
        op as *mut core::ffi::c_void,
        oend.offset_from_unsigned(op),
        bmi2,
        writeSeqEntropy,
        seqEntropyWritten,
    )?;
    if cSeqSize == 0 {
        return Ok(0);
    }
    op = op.add(cSeqSize);

    // Write block header
    let cSize = (op.offset_from_unsigned(ostart)).wrapping_sub(ZSTD_BLOCKHEADERSIZE);
    let cBlockHeader24 = u32::from(lastBlock)
        .wrapping_add((BlockType::Compressed as u32) << 1)
        .wrapping_add((cSize << 3) as u32);
    MEM_writeLE24(ostart as *mut core::ffi::c_void, cBlockHeader24);

    Ok(op.offset_from_unsigned(ostart))
}

unsafe fn ZSTD_estimateSubBlockSize_literal(
    literals: &[u8],
    huf: &ZSTD_hufCTables_t,
    hufMetadata: &ZSTD_hufCTablesMetadata_t,
    workspace: *mut core::ffi::c_void,
    wkspSize: usize,
    writeEntropy: bool,
) -> usize {
    let countWksp = workspace as *mut core::ffi::c_uint;
    let mut maxSymbolValue = u8::MAX;
    let literalSectionHeaderSize = 3; // Use hard coded size of 3 bytes

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
            cLitSizeEstimate.wrapping_add(literalSectionHeaderSize)
        }
    }
}

unsafe fn ZSTD_estimateSubBlockSize_symbolType(
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
            if max <= defaultMax {
                ZSTD_crossEntropyCost(defaultNorm, defaultNormLog, count, max)
            } else {
                return codeTable.len() * 10;
            }
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
            // for offset, offset code is also the number of additional bits
            usize::from(code)
        } else {
            usize::from(additionalBits[usize::from(code)])
        }
    }
    cSymbolTypeSizeEstimateInBits / 8
}

unsafe fn ZSTD_estimateSubBlockSize_sequences(
    ofCodeTable: &[u8],
    llCodeTable: &[u8],
    mlCodeTable: &[u8],
    fseTables: &ZSTD_fseCTables_t,
    fseMetadata: &ZSTD_fseCTablesMetadata_t,
    workspace: *mut core::ffi::c_void,
    wkspSize: usize,
    writeEntropy: bool,
) -> usize {
    let sequencesSectionHeaderSize = 3; // Use hard coded size of 3 bytes
    let mut cSeqSizeEstimate = 0usize;
    if ofCodeTable.is_empty() {
        return sequencesSectionHeaderSize;
    }
    cSeqSizeEstimate = cSeqSizeEstimate.wrapping_add(ZSTD_estimateSubBlockSize_symbolType(
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
    cSeqSizeEstimate = cSeqSizeEstimate.wrapping_add(ZSTD_estimateSubBlockSize_symbolType(
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
    cSeqSizeEstimate = cSeqSizeEstimate.wrapping_add(ZSTD_estimateSubBlockSize_symbolType(
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

unsafe fn ZSTD_estimateSubBlockSize(
    literals: &[u8],
    ofCodeTable: &[u8],
    llCodeTable: &[u8],
    mlCodeTable: &[u8],
    entropy: &ZSTD_entropyCTables_t,
    entropyMetadata: &ZSTD_entropyCTablesMetadata_t,
    workspace: *mut core::ffi::c_void,
    wkspSize: usize,
    writeLitEntropy: bool,
    writeSeqEntropy: bool,
) -> EstimatedBlockSize {
    let estLitSize = ZSTD_estimateSubBlockSize_literal(
        literals,
        &entropy.huf,
        &entropyMetadata.hufMetadata,
        workspace,
        wkspSize,
        writeLitEntropy,
    );
    let estBlockSize = ZSTD_estimateSubBlockSize_sequences(
        ofCodeTable,
        llCodeTable,
        mlCodeTable,
        &entropy.fse,
        &entropyMetadata.fseMetadata,
        workspace,
        wkspSize,
        writeSeqEntropy,
    )
    .wrapping_add(estLitSize.wrapping_add(ZSTD_BLOCKHEADERSIZE));

    EstimatedBlockSize {
        estLitSize,
        estBlockSize,
    }
}

fn ZSTD_needSequenceEntropyTables(fseMetadata: &ZSTD_fseCTablesMetadata_t) -> bool {
    if fseMetadata.llType == SymbolEncodingType::Compressed
        || fseMetadata.llType == SymbolEncodingType::Rle
    {
        return true;
    }
    if fseMetadata.mlType == SymbolEncodingType::Compressed
        || fseMetadata.mlType == SymbolEncodingType::Rle
    {
        return true;
    }
    if fseMetadata.ofType == SymbolEncodingType::Compressed
        || fseMetadata.ofType == SymbolEncodingType::Rle
    {
        return true;
    }
    false
}

fn countLiterals(seqStore: &SeqStore_t, sp: &[SeqDef]) -> usize {
    let mut total = 0usize;
    for seq in sp {
        total = total.wrapping_add(ZSTD_getSequenceLength(seqStore, seq).litLength as usize);
    }
    total
}

pub const BYTESCALE: core::ffi::c_int = 256;

fn sizeBlockSequences(
    seqs: &[SeqDef],
    targetBudget: usize,
    avgLitCost: usize,
    avgSeqCost: usize,
    firstSubBlock: bool,
) -> usize {
    let mut budget = 0usize;

    // entropy headers, generous estimate
    let headerSize = firstSubBlock as usize * 120 * BYTESCALE as usize;
    budget = budget.wrapping_add(headerSize);

    // first sequence => at least one sequence
    budget =
        budget.wrapping_add((seqs[0].litLength as usize * avgLitCost).wrapping_add(avgSeqCost));
    if budget > targetBudget {
        return 1;
    }
    let mut inSize =
        usize::from(seqs[0].litLength) + usize::from(seqs[0].mlBase) + usize::from(MINMATCH);

    // loop over sequences
    let mut n = 1;
    for seq in &seqs[1..] {
        let currentCost = (seq.litLength as usize * avgLitCost).wrapping_add(avgSeqCost);
        budget = budget.wrapping_add(currentCost);
        inSize = inSize.wrapping_add(
            usize::from(seq.litLength) + usize::from(seq.mlBase) + usize::from(MINMATCH),
        );
        // stop when sub-block budget is reached,
        // though continue to expand until the sub-block is deemed compressible
        if budget > targetBudget && budget < inSize * BYTESCALE as usize {
            break;
        }

        n += 1;
    }

    n
}

/// Breaks super-block into multiple sub-blocks and compresses them.
/// Entropy will be written into the first block.
/// The following blocks use repeat_mode to compress.
/// Sub-blocks are all compressed, except the last one when beneficial.
///
/// # Returns
///
/// - The compressed size of the super block (which features multiple ZSTD blocks)
/// - or 0 if it failed to compress.
unsafe fn ZSTD_compressSubBlock_multi(
    seqStorePtr: &SeqStore_t,
    prevCBlock: *const ZSTD_compressedBlockState_t,
    nextCBlock: *mut ZSTD_compressedBlockState_t,
    entropyMetadata: &ZSTD_entropyCTablesMetadata_t,
    cctxParams: &ZSTD_CCtx_params,
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
    bmi2: bool,
    lastBlock: bool,
    workspace: *mut core::ffi::c_void,
    wkspSize: usize,
) -> Result<usize, Error> {
    let sequences = seqStorePtr.seqs();
    let nbSeqs = sequences.len();
    let mut sp = sequences; // tracks progresses within seqStorePtr->sequences
    let lstart: *const u8 = seqStorePtr.litStart;
    let lend: *const u8 = seqStorePtr.lit;
    let nbLiterals = lend.offset_from_unsigned(lstart);
    let mut literals = core::slice::from_raw_parts(lstart, nbLiterals);
    let mut ip = src as *const u8;
    let iend = ip.add(srcSize);
    let ostart = dst as *mut u8;
    let oend = ostart.add(dstCapacity);
    let mut op = ostart;
    let mut llCodes = core::slice::from_raw_parts(seqStorePtr.llCode, nbSeqs);
    let mut mlCodes = core::slice::from_raw_parts(seqStorePtr.mlCode, nbSeqs);
    let mut ofCodes = core::slice::from_raw_parts(seqStorePtr.ofCode, nbSeqs);
    let minTarget = ZSTD_TARGETCBLOCKSIZE_MIN as usize; // enforce minimum size, to reduce undesirable side effects
    let targetCBlockSize = minTarget.max(cctxParams.targetCBlockSize);
    let mut writeLitEntropy = entropyMetadata.hufMetadata.hType == SymbolEncodingType::Compressed;
    let mut writeSeqEntropy = true;

    // let's start by a general estimation for the full block
    #[allow(clippy::manual_checked_ops)]
    if nbSeqs > 0 {
        let ebs = ZSTD_estimateSubBlockSize(
            literals,
            ofCodes,
            llCodes,
            mlCodes,
            &(*nextCBlock).entropy,
            entropyMetadata,
            workspace,
            wkspSize,
            writeLitEntropy,
            writeSeqEntropy,
        );

        // quick estimation
        let avgLitCost = (ebs.estLitSize * BYTESCALE as usize)
            .checked_div(nbLiterals)
            .unwrap_or(BYTESCALE as usize);

        let avgSeqCost =
            ebs.estBlockSize.wrapping_sub(ebs.estLitSize) * BYTESCALE as usize / nbSeqs;

        let nbSubBlocks =
            (ebs.estBlockSize.wrapping_add(targetCBlockSize / 2) / targetCBlockSize).max(1);
        let mut blockBudgetSupp = 0;
        let avgBlockBudget = ebs.estBlockSize * BYTESCALE as usize / nbSubBlocks;
        // simplification: if estimates states that the full superblock doesn't compress, just bail out immediately
        // this will result in the production of a single uncompressed block covering srcSize.
        if ebs.estBlockSize > srcSize {
            return Ok(0);
        }

        // compress and write sub-blocks
        for n in 0..nbSubBlocks.wrapping_sub(1) {
            // determine nb of sequences for current sub-block + nbLiterals from next sequence
            let seqCount = sizeBlockSequences(
                sp,
                avgBlockBudget.wrapping_add(blockBudgetSupp),
                avgLitCost,
                avgSeqCost,
                n == 0,
            );
            // if reached last sequence : break to last sub-block (simplification)
            if seqCount == sp.len() {
                break;
            }

            // compress sub-block
            let mut litEntropyWritten = false;
            let mut seqEntropyWritten = false;
            let litSize = countLiterals(seqStorePtr, &sp[..seqCount]);
            let decompressedSize =
                ZSTD_seqDecompressedSize(seqStorePtr, &sp[..seqCount], litSize, false);
            let cSize = ZSTD_compressSubBlock(
                &(*nextCBlock).entropy,
                entropyMetadata,
                &sp[..seqCount],
                &literals[..litSize],
                &llCodes[..seqCount],
                &mlCodes[..seqCount],
                &ofCodes[..seqCount],
                cctxParams,
                op as *mut core::ffi::c_void,
                oend.offset_from_unsigned(op),
                bmi2,
                writeLitEntropy,
                writeSeqEntropy,
                &mut litEntropyWritten,
                &mut seqEntropyWritten,
                false,
            )?;

            // check compressibility, update state components
            if cSize > 0 && cSize < decompressedSize {
                ip = ip.add(decompressedSize);
                literals = &literals[litSize..];
                op = op.add(cSize);
                llCodes = &llCodes[seqCount..];
                mlCodes = &mlCodes[seqCount..];
                ofCodes = &ofCodes[seqCount..];
                // Entropy only needs to be written once
                if litEntropyWritten {
                    writeLitEntropy = false;
                }
                if seqEntropyWritten {
                    writeSeqEntropy = false;
                }
                sp = &sp[seqCount..];
                blockBudgetSupp = 0;
            }
            // otherwise : do not compress yet, coalesce current sub-block with following one
        }
    }

    // write last block
    let mut litEntropyWritten = false;
    let mut seqEntropyWritten = false;
    let litSize_0 = literals.len();
    let seqCount_0 = sp.len();
    let decompressedSize_0 = ZSTD_seqDecompressedSize(seqStorePtr, sp, litSize_0, true);
    let cSize_0 = ZSTD_compressSubBlock(
        &(*nextCBlock).entropy,
        entropyMetadata,
        sp,
        literals,
        llCodes,
        mlCodes,
        ofCodes,
        cctxParams,
        op as *mut core::ffi::c_void,
        oend.offset_from_unsigned(op),
        bmi2,
        writeLitEntropy,
        writeSeqEntropy,
        &mut litEntropyWritten,
        &mut seqEntropyWritten,
        lastBlock,
    )?;

    // update pointers, the nb of literals borrowed from next sequence must be preserved
    if cSize_0 > 0 && cSize_0 < decompressedSize_0 {
        ip = ip.add(decompressedSize_0);
        op = op.add(cSize_0);

        // Entropy only needs to be written once
        if litEntropyWritten {
            writeLitEntropy = false;
        }
        if seqEntropyWritten {
            writeSeqEntropy = false;
        }
        sp = &sp[seqCount_0..];
    }

    if writeLitEntropy {
        core::ptr::copy_nonoverlapping(
            &raw const (*prevCBlock).entropy.huf,
            &raw mut (*nextCBlock).entropy.huf,
            1,
        );
    }
    if writeSeqEntropy && ZSTD_needSequenceEntropyTables(&entropyMetadata.fseMetadata) {
        // If we haven't written our entropy tables, then we've violated our contract and
        // must emit an uncompressed block.
        return Ok(0);
    }

    if ip < iend {
        // some data left: last part of the block sent uncompressed
        let rSize = iend.offset_from_unsigned(ip);
        let cSize_1 = ZSTD_noCompressBlock(
            op as *mut core::ffi::c_void,
            oend.offset_from_unsigned(op),
            ip as *const core::ffi::c_void,
            rSize,
            lastBlock,
        )?;
        op = op.add(cSize_1);

        // We have to regenerate the repcodes because we've skipped some sequences
        if !sp.is_empty() {
            let mut rep = (*prevCBlock).rep;
            for seq in &sequences[..nbSeqs - sp.len()] {
                ZSTD_updateRep(
                    &mut rep,
                    seq.offBase,
                    ZSTD_getSequenceLength(seqStorePtr, seq).litLength == 0,
                );
            }
            (*nextCBlock).rep = rep;
        }
    }

    Ok(op.offset_from_unsigned(ostart))
}

pub unsafe fn ZSTD_compressSuperBlock(
    zc: *mut ZSTD_CCtx,
    dst: *mut core::ffi::c_void,
    dstCapacity: usize,
    src: *const core::ffi::c_void,
    srcSize: usize,
    lastBlock: bool,
) -> Result<usize, Error> {
    let mut entropyMetadata = ZSTD_entropyCTablesMetadata_t::default();

    ZSTD_buildBlockEntropyStats(
        &(*zc).seqStore,
        &(*(*zc).blockState.prevCBlock).entropy,
        &mut (*(*zc).blockState.nextCBlock).entropy,
        &(*zc).appliedParams,
        &mut entropyMetadata,
        (*zc).tmpWorkspace,
        (*zc).tmpWkspSize,
    )?;

    ZSTD_compressSubBlock_multi(
        &(*zc).seqStore,
        (*zc).blockState.prevCBlock,
        (*zc).blockState.nextCBlock,
        &entropyMetadata,
        &(*zc).appliedParams,
        dst,
        dstCapacity,
        src,
        srcSize,
        (*zc).bmi2 != 0,
        lastBlock,
        (*zc).tmpWorkspace,
        (*zc).tmpWkspSize,
    )
}
