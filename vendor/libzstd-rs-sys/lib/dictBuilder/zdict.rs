use std::mem::MaybeUninit;
use std::time::{Duration, Instant};

use crate::lib::common::error_private::{ERR_getErrorName, ERR_isError, Error};
use crate::lib::common::huf::{CTable, HUF_CTABLE_WORKSPACE_SIZE_U32, HUF_WORKSPACE_SIZE};
use crate::lib::common::mem::{MEM_readLE32, MEM_writeLE32};
use crate::lib::common::xxhash::ZSTD_XXH64;
use crate::lib::common::zstd_internal::{
    repStartValue, LLFSELog, MLFSELog, MaxLL, MaxML, OffFSELog, ZSTD_REP_NUM,
};
use crate::lib::compress::fse_compress::{FSE_normalizeCount, FSE_writeNCount};
use crate::lib::compress::huf_compress::{HUF_buildCTable_wksp, HUF_writeCTable_wksp};
#[expect(deprecated)] // We still use deprecated functions internally
use crate::lib::compress::zstd_compress::{
    SeqDef, ZSTD_CCtx, ZSTD_CDict, ZSTD_compressBegin_usingCDict_deprecated,
    ZSTD_compressBlock_deprecated, ZSTD_compressedBlockState_t, ZSTD_createCDict_advanced,
    ZSTD_freeCCtxContent, ZSTD_freeCDict, ZSTD_getParams, ZSTD_initCCtx, ZSTD_loadCEntropy,
    ZSTD_reset_compressedBlockState, ZSTD_seqToCodes,
};
use crate::lib::dictBuilder::divsufsort::divsufsort;
use crate::lib::dictBuilder::fastcover::ZDICT_optimizeTrainFromBuffer_fastCover;
#[expect(deprecated)]
use crate::lib::zdict::experimental::{
    ZDICT_fastCover_params_t, ZDICT_legacy_params_t, ZDICT_CONTENTSIZE_MIN, ZDICT_DICTSIZE_MIN,
};
use crate::lib::zdict::ZDICT_params_t;
use crate::lib::zstd::{
    ZSTD_customMem, ZSTD_dct_rawContent, ZSTD_dlm_byRef, ZSTD_parameters, ZSTD_BLOCKSIZE_MAX,
    ZSTD_CLEVEL_DEFAULT, ZSTD_MAGIC_DICTIONARY,
};

#[repr(C)]
struct EStats_ress_t {
    /// dictionary
    dict: *mut ZSTD_CDict,
    /// working context
    zc: Box<ZSTD_CCtx>,
    /// must be [`ZSTD_BLOCKSIZE_MAX`] allocated
    workPlace: Box<[MaybeUninit<u8>]>,
}

#[derive(Copy, Clone, Default)]
#[repr(C)]
struct offsetCount_t {
    offset: u32,
    count: u32,
}

#[derive(Debug, Copy, Clone, Default)]
#[repr(C)]
struct DictItem {
    pos: u32,
    length: u32,
    savings: u32,
}

impl DictItem {
    fn init(&mut self) {
        self.pos = 1;
        self.length = 0;
        self.savings = -1i32 as u32;
    }
}

const MINRATIO: usize = 4;
const ZDICT_MAX_SAMPLES_SIZE: usize = 2000 << 20;
#[expect(deprecated)]
const ZDICT_MIN_SAMPLES_SIZE: usize = ZDICT_CONTENTSIZE_MIN * MINRATIO;

const NOISELENGTH: usize = 32;
static g_selectivity_default: u32 = 9;

/// Prints the bytes as characters, with non-printable characters replaced by '.', used for debug output
fn ZDICT_printHex(bytes: &[u8]) {
    let s = bytes.iter().map(|byte| {
        if (32..=126).contains(byte) {
            char::from(*byte)
        } else {
            '.' // non-printable character
        }
    });
    eprint!("{}", s.collect::<String>())
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZDICT_isError))]
pub extern "C" fn ZDICT_isError(errorCode: usize) -> core::ffi::c_uint {
    ERR_isError(errorCode) as _
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZDICT_getErrorName))]
pub extern "C" fn ZDICT_getErrorName(errorCode: usize) -> *const core::ffi::c_char {
    ERR_getErrorName(errorCode)
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZDICT_getDictID))]
pub unsafe extern "C" fn ZDICT_getDictID(
    dictBuffer: *const core::ffi::c_void,
    dictSize: usize,
) -> core::ffi::c_uint {
    if dictSize < 8 {
        return 0;
    }
    if MEM_readLE32(dictBuffer) != ZSTD_MAGIC_DICTIONARY {
        return 0;
    }
    MEM_readLE32(dictBuffer.byte_add(4))
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZDICT_getDictHeaderSize))]
pub unsafe extern "C" fn ZDICT_getDictHeaderSize(
    dictBuffer: *const core::ffi::c_void,
    dictSize: usize,
) -> usize {
    if dictSize <= 8 || MEM_readLE32(dictBuffer) != ZSTD_MAGIC_DICTIONARY {
        return Error::dictionary_corrupted.to_error_code();
    }

    // FIXME: in 1.92 we can use https://doc.rust-lang.org/std/boxed/struct.Box.html#method.new_zeroed
    let mut bs = Box::<ZSTD_compressedBlockState_t>::new_uninit();
    unsafe { ZSTD_reset_compressedBlockState(bs.as_mut_ptr()) };

    let mut wksp = Box::<[u32]>::new_uninit_slice(HUF_WORKSPACE_SIZE / 4);

    let dict = core::slice::from_raw_parts(dictBuffer as *const u8, dictSize);
    match ZSTD_loadCEntropy(
        bs.as_mut_ptr(),
        wksp.as_mut_ptr() as *mut core::ffi::c_void,
        dict,
    ) {
        Ok(size) => size,
        Err(err) => err.to_error_code(),
    }
}

fn ZDICT_count(pIn: &[u8], pMatch: &[u8]) -> usize {
    pIn.iter()
        .zip(pMatch)
        .position(|(a, b)| a != b)
        .unwrap_or(0)
}

const LLIMIT: usize = 64;
const MINMATCHLENGTH: usize = 7;
fn ZDICT_analyzePos(
    doneMarks: &mut [bool],
    suffix_slice: &[u32],
    mut start: u32,
    buffer: &[u8],
    minRatio: usize,
    notificationLevel: u32,
) -> DictItem {
    // The C implementation maps index `len` and `-1` to the length of the suffix array.
    let suffix = |index| {
        if index == usize::MAX || index == suffix_slice.len() {
            suffix_slice.len() as u32
        } else {
            suffix_slice[index]
        }
    };

    let mut pos = suffix(start as usize) as usize;
    let mut end = start;
    let mut solution = DictItem::default();

    doneMarks[pos] = true;

    // trivial repetition cases
    if buffer[pos..pos + 2] == buffer[pos + 2..pos + 4]
        || buffer[pos + 1..pos + 3] == buffer[pos + 3..pos + 5]
        || buffer[pos + 2..pos + 4] == buffer[pos + 4..pos + 6]
    {
        // skip and mark segment
        let pattern16 = &buffer[pos + 4..pos + 6];
        let mut patternEnd = 6usize;
        while buffer[pos + patternEnd..pos + patternEnd + 2] == *pattern16 {
            patternEnd += 2;
        }
        if buffer[pos + patternEnd] == buffer[pos + patternEnd - 1] {
            patternEnd += 1;
        }
        doneMarks[pos..][1..patternEnd].fill(true);
        return solution;
    }

    // look forward
    loop {
        end = end.wrapping_add(1);
        let length = ZDICT_count(&buffer[pos..], &buffer[suffix(end as usize) as usize..]);
        if length < MINMATCHLENGTH {
            break;
        }
    }

    // look backward
    loop {
        let length = ZDICT_count(
            &buffer[pos..],
            &buffer[suffix((start as usize).wrapping_sub(1)) as usize..],
        );
        if length >= MINMATCHLENGTH {
            start = start.wrapping_sub(1);
        }
        if length < MINMATCHLENGTH {
            break;
        }
    }

    // exit if not found a minimum number of repetitions
    if end.wrapping_sub(start) < minRatio as u32 {
        for idx in start..end {
            doneMarks[suffix(idx as usize) as usize] = true;
        }
        return solution;
    }

    let mut refinedStart = start;
    let mut refinedEnd = end;

    if notificationLevel >= 4 {
        eprintln!();
        eprint!(
            "found {:>3} matches of length >= {} at pos {:>7}  ",
            end.wrapping_sub(start),
            MINMATCHLENGTH,
            pos as core::ffi::c_uint,
        );
        eprintln!();
    }

    let mut mml = MINMATCHLENGTH as u32;
    loop {
        let mut currentChar = 0;
        let mut currentCount = 0u32;
        let mut currentID = refinedStart;
        let mut selectedCount = 0;
        let mut selectedID = currentID;

        for id in refinedStart..refinedEnd {
            if buffer[(suffix(id as usize) + mml) as usize] != currentChar {
                if currentCount > selectedCount {
                    selectedCount = currentCount;
                    selectedID = currentID;
                }
                currentID = id;
                currentChar = buffer[(suffix(id as usize) + mml) as usize];
                currentCount = 0;
            }
            currentCount = currentCount.wrapping_add(1);
        }

        if currentCount > selectedCount {
            selectedCount = currentCount;
            selectedID = currentID;
        }
        if selectedCount < minRatio as u32 {
            break;
        }
        refinedStart = selectedID;
        refinedEnd = refinedStart.wrapping_add(selectedCount);
        mml = mml.wrapping_add(1);
    }

    // evaluate gain based on new dict
    start = refinedStart;
    pos = suffix(refinedStart as usize) as usize;
    end = start;

    // look forward
    let mut lengthList = [0u32; LLIMIT];
    loop {
        end = end.wrapping_add(1);
        let mut length = ZDICT_count(&buffer[pos..], &buffer[suffix(end as usize) as usize..]);
        if length >= LLIMIT {
            length = LLIMIT - 1;
        }
        lengthList[length] += 1;
        if length < MINMATCHLENGTH {
            break;
        }
    }

    // look backward
    let mut length = MINMATCHLENGTH;
    while (length >= MINMATCHLENGTH) as core::ffi::c_int & (start > 0) as core::ffi::c_int != 0 {
        length = ZDICT_count(
            &buffer[pos..],
            &buffer[suffix(start.wrapping_sub(1) as usize) as usize..],
        );
        if length >= LLIMIT {
            length = LLIMIT - 1;
        }
        lengthList[length] += 1;
        if length >= MINMATCHLENGTH {
            start = start.wrapping_sub(1);
        }
    }

    // largest useful length
    let mut cumulLength = [0u32; LLIMIT];
    cumulLength[LLIMIT - 1] = lengthList[LLIMIT - 1];
    for i in (0..LLIMIT - 1).rev() {
        cumulLength[i] = cumulLength[i + 1].wrapping_add(lengthList[i]);
    }
    let mut maxLength = (MINMATCHLENGTH..LLIMIT)
        .rev()
        .find(|&i| cumulLength[i] >= minRatio as u32)
        .unwrap_or(MINMATCHLENGTH - 1);

    // reduce maxLength in case of final into repetitive data
    let mut l = maxLength as u32;
    let c = buffer[pos + maxLength - 1];
    while buffer[pos + l as usize - 2] == c {
        l = l.wrapping_sub(1);
    }
    maxLength = l as usize;
    if maxLength < MINMATCHLENGTH {
        return solution; // skip: no long-enough solution available
    }

    // calculate savings
    let mut savings = [0u32; LLIMIT];
    savings[5] = 0;

    for u_1 in MINMATCHLENGTH..=maxLength {
        savings[u_1] = savings[u_1.wrapping_sub(1)]
            .wrapping_add(lengthList[u_1].wrapping_mul(u_1.wrapping_sub(3) as u32));
    }

    if notificationLevel >= 4 {
        eprintln!(
            "Selected dict at position {}, of length {} : saves {} (ratio: {:.2})  ",
            pos,
            maxLength,
            savings[maxLength],
            savings[maxLength] as core::ffi::c_double / maxLength as core::ffi::c_double,
        );
    }

    solution.pos = pos as u32;
    solution.length = maxLength as u32;
    solution.savings = savings[maxLength];

    // mark positions done
    for id_0 in start..end {
        let testedPos = suffix(id_0 as usize) as usize;
        let length = if testedPos == pos {
            solution.length as usize
        } else {
            ZDICT_count(&buffer[pos..], &buffer[testedPos..]).min(solution.length as usize)
        };
        let pEnd = testedPos.wrapping_add(length);
        doneMarks[testedPos..pEnd].fill(true);
    }

    solution
}

fn isIncluded(ip: &[u8], into: &[u8], length: usize) -> bool {
    // NOTE: the slices may not actually have `length` elements,
    // that is OK if there is an unequal value before that.
    let a = ip.iter().take(length);
    let b = into.iter().take(length);

    a.eq(b)
}

fn ZDICT_tryMerge(
    table: &mut [DictItem],
    mut elt: DictItem,
    eltNbToSkip: u32,
    buffer: &[u8],
) -> u32 {
    let tableSize = table[0].pos;
    let eltEnd = elt.pos.wrapping_add(elt.length);
    let buf = buffer;

    /* tail overlap */
    for mut u in 1..tableSize as usize {
        if (u as u32 != eltNbToSkip) && table[u].pos > elt.pos && table[u].pos <= eltEnd {
            /* append */
            let addedLength = table[u].pos - elt.pos;
            table[u].length += addedLength;
            table[u].pos = elt.pos;
            table[u].savings += elt.savings * addedLength / elt.length; /* rough approx */
            table[u].savings += elt.length / 8; /* rough approx bonus */
            elt = table[u];
            /* sort : improve rank */
            while (u > 1) && (table[u - 1].savings < elt.savings) {
                table[u] = table[u - 1];
                u -= 1;
            }
            table[u] = elt;
            return u as u32;
        }
    }

    /* front overlap */
    for mut u in 1..tableSize as usize {
        if u == eltNbToSkip as usize {
            continue;
        }

        /* overlap, existing < new */
        if (table[u].pos + table[u].length >= elt.pos) && (table[u].pos < elt.pos) {
            /* append */
            let addedLength = eltEnd as i32 - (table[u].pos + table[u].length) as i32; /* note: can be negative */
            table[u].savings += elt.length / 8; /* rough approx bonus */
            if addedLength > 0 {
                /* otherwise, elt fully included into existing */
                table[u].length += addedLength.unsigned_abs();
                /* rough approx */
                table[u].savings += elt.savings * addedLength.unsigned_abs() / elt.length;
            }
            /* sort : improve rank */
            elt = table[u];
            while (u > 1) && (table[u - 1].savings < elt.savings) {
                table[u] = table[u - 1];
                u -= 1;
            }
            table[u] = elt;
            return u as u32;
        }

        if buf[table[u].pos as usize..][..8] == buf[elt.pos as usize + 1..][..8]
            && isIncluded(
                &buf[table[u].pos as usize..],
                &buf[elt.pos as usize + 1..],
                table[u].length as usize,
            )
        {
            let addedLength = Ord::max(1, elt.length.checked_sub(table[u].length).unwrap_or(1));
            table[u].pos = elt.pos;
            table[u].savings += elt.savings * addedLength / elt.length;
            table[u].length = Ord::min(elt.length, table[u].length + 1);
            return u as u32;
        }
    }

    0
}

fn ZDICT_removeDictItem(table: &mut [DictItem], id: u32) {
    debug_assert_ne!(id, 0);
    if id == 0 {
        return; // protection, should never happen
    }
    let max = table[0].pos as usize; // convention: table[0].pos stores the number of elements
    for u in id as usize..max.wrapping_sub(1) {
        table[u] = table[u + 1];
    }
    table[0].pos -= 1;
}

fn ZDICT_insertDictItem(table: &mut [DictItem], elt: DictItem, buffer: &[u8]) {
    let maxSize = table.len() as u32;

    // merge if possible
    let mut mergeId = ZDICT_tryMerge(table, elt, 0, buffer);
    if mergeId != 0 {
        let mut newMerge = 1;
        while newMerge != 0 {
            newMerge = ZDICT_tryMerge(table, table[mergeId as usize], mergeId, buffer);
            if newMerge != 0 {
                ZDICT_removeDictItem(table, mergeId);
            }
            mergeId = newMerge;
        }
        return;
    }

    // insert
    let mut nextElt = table[0].pos;
    if nextElt >= maxSize {
        nextElt = maxSize.wrapping_sub(1);
    }
    let mut current = nextElt.wrapping_sub(1);
    while table[current as usize].savings < elt.savings {
        table[current.wrapping_add(1) as usize] = table[current as usize];
        current = current.wrapping_sub(1);
    }
    table[current as usize + 1] = elt;
    table[0].pos = nextElt.wrapping_add(1);
}

fn ZDICT_dictSize(dictList: &[DictItem]) -> usize {
    dictList[1..dictList[0].pos as usize]
        .iter()
        .map(|d| d.length as usize)
        .sum()
}

fn ZDICT_trainBuffer_legacy(
    dictList: &mut [DictItem],
    buffer: &[u8],
    mut bufferSize: usize,
    fileSizes: &[usize],
    mut nbFiles: usize,
    mut minRatio: usize,
    notificationLevel: u32,
) -> usize {
    let mut displayClock = Instant::now();
    let refresh_rate = Duration::from_millis(300);

    // init
    if notificationLevel >= 2 {
        eprintln!("\r{:70 }\r", ""); // clean display line
    }

    minRatio = Ord::max(minRatio, MINRATIO);

    // limit sample set size (divsufsort limitation)
    if bufferSize > ZDICT_MAX_SAMPLES_SIZE && notificationLevel >= 3 {
        eprintln!(
            "sample set too large : reduced to {} MB ...",
            2000 << 20 >> 20,
        );
    }
    while bufferSize > ZDICT_MAX_SAMPLES_SIZE {
        nbFiles = nbFiles.wrapping_sub(1);
        bufferSize = bufferSize.wrapping_sub(fileSizes[nbFiles]);
    }

    // sort
    if notificationLevel >= 2 {
        eprintln!(
            "sorting {} files of total size {} MB ...",
            nbFiles,
            bufferSize >> 20,
        );
    }
    let mut suffix = vec![0u32; bufferSize];
    let divSuftSortResult = divsufsort(
        &buffer[..bufferSize],
        unsafe { std::mem::transmute::<&mut [u32], &mut [i32]>(&mut suffix[..]) },
        false,
    );
    if divSuftSortResult != 0 {
        return Error::GENERIC.to_error_code();
    }

    // build reverse suffix sort
    let mut reverseSuffix = vec![0u32; bufferSize];
    for pos in 0..bufferSize {
        reverseSuffix[suffix[pos] as usize] = pos as u32;
    }

    // Note: filePos tracks borders between samples.
    // It's not used at this stage, but planned to become useful in a later update
    let mut filePos = vec![0u32; nbFiles];
    // filePos[0] is intentionally left 0
    for pos in 1..nbFiles {
        filePos[pos] =
            (filePos[pos - 1] as usize).wrapping_add(fileSizes[pos.wrapping_sub(1)]) as u32;
    }

    if notificationLevel >= 2 {
        eprintln!("finding patterns ...");
    }
    if notificationLevel >= 3 {
        eprintln!("minimum ratio : {} ", minRatio);
    }

    let mut doneMarks = vec![false; bufferSize + 16];
    let mut cursor = 0usize;
    while cursor < bufferSize {
        if doneMarks[cursor] {
            cursor += 1;
            continue;
        }

        let solution = ZDICT_analyzePos(
            &mut doneMarks,
            &suffix,
            reverseSuffix[cursor],
            buffer,
            minRatio,
            notificationLevel,
        );
        if solution.length == 0 {
            cursor += 1;
            continue;
        }

        ZDICT_insertDictItem(dictList, solution, buffer);
        cursor += solution.length as usize;

        if notificationLevel >= 2 && displayClock.elapsed() > refresh_rate {
            displayClock = Instant::now();
            eprint!(
                "\r{:4.2} % \r",
                cursor as core::ffi::c_double / bufferSize as core::ffi::c_double * 100.0f64,
            );
        }
    }

    0
}

fn fill_noise(buffer: &mut [u8]) {
    const prime1: u32 = 2654435761;
    const prime2: u32 = 2246822519;

    let mut acc = prime1;

    for v in buffer.iter_mut() {
        acc = acc.wrapping_mul(prime2);
        *v = (acc >> 21) as u8;
    }
}

const MAXREPOFFSET: usize = 1024;
unsafe fn ZDICT_countEStats(
    esr: &mut EStats_ress_t,
    params: &ZSTD_parameters,
    countLit: &mut [u32; 256],
    offsetcodeCount: &mut [u32; OFFCODE_MAX as usize + 1],
    matchlengthCount: &mut [u32; MaxML as usize + 1],
    litlengthCount: &mut [u32; MaxLL as usize + 1],
    repOffsets: &mut [u32; MAXREPOFFSET],
    src: &[u8],
    notificationLevel: u32,
) {
    let blockSizeMax = Ord::min(1 << 17, 1 << params.cParams.windowLog);
    let srcSize = Ord::min(src.len(), blockSizeMax);

    #[allow(deprecated)] // Still used internally
    let errorCode = ZSTD_compressBegin_usingCDict_deprecated(esr.zc.as_mut(), esr.dict);
    if ERR_isError(errorCode) {
        if notificationLevel >= 1 {
            eprintln!("warning : ZSTD_compressBegin_usingCDict failed");
        }
        return;
    }

    #[allow(deprecated)] // Still used internally
    let cSize = ZSTD_compressBlock_deprecated(
        esr.zc.as_mut(),
        esr.workPlace.as_mut_ptr().cast(),
        ZSTD_BLOCKSIZE_MAX as usize,
        src.as_ptr().cast::<core::ffi::c_void>(),
        srcSize,
    );
    if ERR_isError(cSize) {
        if notificationLevel >= 3 {
            eprintln!("warning : could not compress sample size {} ", srcSize);
        }
        return;
    }

    // if cSize is 0, the block is not compressible
    if cSize == 0 {
        return;
    }

    let seqStore = &esr.zc.seqStore;

    // literals stats
    let mut bytePtr = seqStore.litStart as *const u8;
    while bytePtr < seqStore.lit as *const u8 {
        countLit[usize::from(*bytePtr)] += 1;
        bytePtr = bytePtr.add(1);
    }

    // seqStats
    let nbSeq = seqStore
        .sequences
        .offset_from_unsigned(seqStore.sequencesStart);
    ZSTD_seqToCodes(seqStore);

    let codePtr: *const u8 = seqStore.ofCode;
    for u in 0..nbSeq {
        offsetcodeCount[*codePtr.add(u) as usize] += 1;
    }
    let codePtr: *const u8 = seqStore.mlCode;
    for u in 0..nbSeq {
        matchlengthCount[*codePtr.add(u) as usize] += 1;
    }
    let codePtr: *const u8 = seqStore.llCode;
    for u in 0..nbSeq {
        litlengthCount[*codePtr.add(u) as usize] += 1;
    }

    if nbSeq >= 2 {
        // rep offsets
        let seq: *const SeqDef = seqStore.sequencesStart;
        let mut offset1 = (*seq).offBase.wrapping_sub(ZSTD_REP_NUM) as usize;
        let mut offset2 = (*seq.add(1)).offBase.wrapping_sub(ZSTD_REP_NUM) as usize;
        if offset1 >= MAXREPOFFSET {
            offset1 = 0;
        }
        if offset2 >= MAXREPOFFSET {
            offset2 = 0;
        }
        repOffsets[offset1] += 3;
        repOffsets[offset2] += 1;
    }
}

fn ZDICT_insertSortCount(
    table: &mut [offsetCount_t; ZSTD_REP_NUM as usize + 1],
    val: u32,
    count: u32,
) {
    table[ZSTD_REP_NUM as usize] = offsetCount_t { offset: val, count };
    for u in (1..ZSTD_REP_NUM as usize).rev() {
        if table[u - 1].count >= table[u].count {
            break;
        }
        table.swap(u, u - 1);
    }
}

/// Rewrite `countLit` to contain a mostly flat but still compressible distribution of literals.
/// Necessary to avoid generating a non-compressible distribution that [`HUF_writeCTable`] cannot encode.
fn ZDICT_flatLit(countLit: &mut [core::ffi::c_uint; 256]) {
    countLit.fill(2);

    countLit[0] = 4;
    countLit[253] = 1;
    countLit[254] = 1;
}

const OFFCODE_MAX: u8 = 30; // only applicable to first block
unsafe fn ZDICT_analyzeEntropy(
    dstBuffer: *mut core::ffi::c_void,
    maxDstSize: usize,
    compressionLevel: core::ffi::c_int,
    src: &[u8],
    fileSizes: &[usize],
    dictBuffer: *const core::ffi::c_void,
    dictBufferSize: usize,
    notificationLevel: core::ffi::c_uint,
) -> Result<usize, Error> {
    let mut cctx = Box::new_uninit();
    ZSTD_initCCtx(cctx.as_mut_ptr(), ZSTD_customMem::default());

    let mut esr = EStats_ress_t {
        dict: core::ptr::null_mut(),
        zc: cctx.assume_init(),
        workPlace: Box::default(),
    };

    let eSize = analyze_entropy_internal(
        dstBuffer as *mut u8,
        maxDstSize,
        compressionLevel,
        src,
        fileSizes,
        dictBuffer,
        dictBufferSize,
        notificationLevel,
        &mut esr,
    );

    ZSTD_freeCDict(esr.dict);
    ZSTD_freeCCtxContent(esr.zc.as_mut());

    eSize
}

unsafe fn analyze_entropy_internal(
    mut dstPtr: *mut u8,
    mut maxDstSize: usize,
    mut compressionLevel: core::ffi::c_int,
    src: &[u8],
    fileSizes: &[usize],
    dictBuffer: *const core::ffi::c_void,
    dictBufferSize: usize,
    notificationLevel: core::ffi::c_uint,
    esr: &mut EStats_ress_t,
) -> Result<usize, Error> {
    let mut hufTable = CTable::default();

    const KB: usize = 1 << 10;
    let offcodeMax = dictBufferSize.wrapping_add(128 * KB).ilog2() as u8;
    if offcodeMax > OFFCODE_MAX {
        return Err(Error::dictionaryCreation_failed); // dictionary too large
    }

    let mut offcodeNCount = [0i16; OFFCODE_MAX as usize + 1];
    let mut matchLengthNCount = [0i16; MaxML as usize + 1];
    let mut litLengthNCount = [0i16; MaxLL as usize + 1];

    let mut countLit = [1u32; 256]; // any character must be described
    let mut offcodeCount = [1u32; OFFCODE_MAX as usize + 1];
    let mut matchLengthCount = [1u32; MaxML as usize + 1];
    let mut litLengthCount = [1u32; MaxLL as usize + 1];

    let mut repOffset = [0; MAXREPOFFSET];
    repOffset[1] = 1;
    repOffset[4] = 1;
    repOffset[8] = 1;

    let mut bestRepOffset = [offsetCount_t::default(); ZSTD_REP_NUM as usize + 1];

    let averageSampleSize = fileSizes
        .iter()
        .sum::<usize>()
        .checked_div(fileSizes.len())
        .unwrap_or(0);
    if compressionLevel == 0 {
        compressionLevel = ZSTD_CLEVEL_DEFAULT;
    }
    let params = ZSTD_getParams(
        compressionLevel,
        averageSampleSize as core::ffi::c_ulonglong,
        dictBufferSize,
    );
    esr.dict = ZSTD_createCDict_advanced(
        dictBuffer,
        dictBufferSize,
        ZSTD_dlm_byRef,
        ZSTD_dct_rawContent,
        params.cParams,
        ZSTD_customMem::default(),
    );

    esr.workPlace = Box::new_uninit_slice(ZSTD_BLOCKSIZE_MAX as usize);
    if esr.dict.is_null() {
        if notificationLevel >= 1 {
            eprintln!("Not enough memory");
        }
        return Err(Error::memory_allocation);
    }

    // collect stats on all samples
    let mut pos = 0usize;
    for fileSize in fileSizes {
        ZDICT_countEStats(
            esr,
            &params,
            &mut countLit,
            &mut offcodeCount,
            &mut matchLengthCount,
            &mut litLengthCount,
            &mut repOffset,
            &src[pos..][..*fileSize],
            notificationLevel,
        );
        pos = pos.wrapping_add(*fileSize);
    }
    if notificationLevel >= 4 {
        eprintln!("Offset Code Frequencies :");
        for (i, count) in offcodeCount.iter().enumerate() {
            eprintln!("{:>2} :{:>7} ", i, count);
        }
    }

    // analyze, build stats, starting with literals
    let mut wksp = [0u32; HUF_CTABLE_WORKSPACE_SIZE_U32];
    let huffLog = 11;
    let mut maxNbBits = HUF_buildCTable_wksp(
        &mut hufTable,
        &countLit,
        255,
        huffLog,
        wksp.as_mut_ptr() as *mut core::ffi::c_void,
        size_of::<[u32; HUF_CTABLE_WORKSPACE_SIZE_U32]>(),
    )
    .inspect_err(|_| {
        if notificationLevel >= 1 {
            eprintln!(" HUF_buildCTable error");
        }
    })?;
    if maxNbBits == 8 {
        // not compressible: will fail on HUF_writeCTable
        if notificationLevel >= 2 {
            eprintln!("warning : pathological dataset : literals are not compressible : samples are noisy or too regular ");
        }
        ZDICT_flatLit(&mut countLit); // replace distribution by a fake "mostly flat but still compressible" distribution, that HUF_writeCTable can encode
        maxNbBits = HUF_buildCTable_wksp(
            &mut hufTable,
            &countLit,
            255,
            huffLog,
            wksp.as_mut_ptr() as *mut core::ffi::c_void,
            size_of::<[u32; HUF_CTABLE_WORKSPACE_SIZE_U32]>(),
        )
        .inspect_err(|_| {
            if notificationLevel >= 1 {
                eprintln!(" HUF_buildCTable error");
            }
        })?;
        debug_assert_eq!(maxNbBits, 9);
    }
    let huffLog = maxNbBits as u32;

    // look for most common first offsets
    for (offset, rep) in repOffset[1..MAXREPOFFSET].iter().enumerate() {
        ZDICT_insertSortCount(&mut bestRepOffset, offset as u32, *rep);
    }

    let total: u32 = offcodeCount[..offcodeMax as usize + 1].iter().sum();
    let offLog = FSE_normalizeCount(
        &mut offcodeNCount,
        OffFSELog,
        &offcodeCount,
        total as usize,
        offcodeMax,
        true,
    )
    .inspect_err(|_| {
        if notificationLevel >= 1 {
            eprintln!("FSE_normalizeCount error with offcodeCount");
        }
    })?;

    let total: u32 = matchLengthCount.iter().sum();
    let mlLog = FSE_normalizeCount(
        &mut matchLengthNCount,
        MLFSELog,
        &matchLengthCount,
        total as usize,
        MaxML,
        true,
    )
    .inspect_err(|_| {
        if notificationLevel >= 1 {
            eprintln!("FSE_normalizeCount error with matchLengthCount");
        }
    })?;

    let total: u32 = litLengthCount.iter().sum();
    let llLog = FSE_normalizeCount(
        &mut litLengthNCount,
        LLFSELog,
        &litLengthCount,
        total as usize,
        MaxLL,
        true,
    )
    .inspect_err(|_| {
        if notificationLevel >= 1 {
            eprintln!("FSE_normalizeCount error with litLengthCount");
        }
    })?;

    // write result to buffer
    let hhSize = HUF_writeCTable_wksp(
        dstPtr as *mut core::ffi::c_void,
        maxDstSize,
        &hufTable,
        255,
        huffLog,
        wksp.as_mut_ptr() as *mut core::ffi::c_void,
        size_of::<[u32; HUF_CTABLE_WORKSPACE_SIZE_U32]>(),
    )
    .inspect_err(|_| {
        if notificationLevel >= 1 {
            eprintln!("HUF_writeCTable error");
        }
    })?;
    dstPtr = dstPtr.add(hhSize);
    maxDstSize = maxDstSize.wrapping_sub(hhSize);
    let mut eSize = hhSize;

    let ohSize = FSE_writeNCount(
        dstPtr as *mut core::ffi::c_void,
        maxDstSize,
        &offcodeNCount,
        OFFCODE_MAX,
        offLog,
    )
    .inspect_err(|_| {
        if notificationLevel >= 1 {
            eprintln!("FSE_writeNCount error with offcodeNCount");
        }
    })?;
    dstPtr = dstPtr.add(ohSize);
    maxDstSize = maxDstSize.wrapping_sub(ohSize);
    eSize = eSize.wrapping_add(ohSize);

    let mhSize = FSE_writeNCount(
        dstPtr as *mut core::ffi::c_void,
        maxDstSize,
        &matchLengthNCount,
        MaxML,
        mlLog,
    )
    .inspect_err(|_| {
        if notificationLevel >= 1 {
            eprintln!("FSE_writeNCount error with matchLengthNCount ");
        }
    })?;
    dstPtr = dstPtr.add(mhSize);
    maxDstSize = maxDstSize.wrapping_sub(mhSize);
    eSize = eSize.wrapping_add(mhSize);

    let lhSize = FSE_writeNCount(
        dstPtr as *mut core::ffi::c_void,
        maxDstSize,
        &litLengthNCount,
        MaxLL,
        llLog,
    )
    .inspect_err(|_| {
        if notificationLevel >= 1 {
            eprintln!("FSE_writeNCount error with litlengthNCount ");
        }
    })?;
    dstPtr = dstPtr.add(lhSize);
    maxDstSize = maxDstSize.wrapping_sub(lhSize);
    eSize = eSize.wrapping_add(lhSize);

    if maxDstSize < 12 {
        if notificationLevel >= 1 {
            eprintln!("not enough space to write RepOffsets ");
        }
        return Err(Error::dstSize_tooSmall);
    }

    MEM_writeLE32(dstPtr as *mut core::ffi::c_void, repStartValue[0]);
    MEM_writeLE32(dstPtr.add(4) as *mut core::ffi::c_void, repStartValue[1]);
    MEM_writeLE32(dstPtr.add(8) as *mut core::ffi::c_void, repStartValue[2]);

    Ok(eSize.wrapping_add(12))
}

#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZDICT_finalizeDictionary))]
pub unsafe extern "C" fn ZDICT_finalizeDictionary(
    dictBuffer: *mut core::ffi::c_void,
    dictBufferCapacity: usize,
    customDictContent: *const core::ffi::c_void,
    dictContentSize: usize,
    samplesBuffer: *const core::ffi::c_void,
    samplesSizes: *const usize,
    nbSamples: core::ffi::c_uint,
    params: ZDICT_params_t,
) -> usize {
    let samplesSizes = if samplesSizes.is_null() || nbSamples == 0 {
        &[]
    } else {
        core::slice::from_raw_parts(samplesSizes, nbSamples as usize)
    };
    let totalSamplesSize = samplesSizes.iter().sum::<usize>();
    let samples = if samplesBuffer.is_null() || totalSamplesSize == 0 {
        &[]
    } else {
        core::slice::from_raw_parts(samplesBuffer.cast::<u8>(), totalSamplesSize)
    };

    finalize_dictionary(
        dictBuffer,
        dictBufferCapacity,
        customDictContent,
        dictContentSize,
        samples,
        samplesSizes,
        params,
    )
    .map_err(|e| e.to_error_code())
    .unwrap_or_else(|e| e)
}

const HBUFFSIZE: usize = 256; // should be large enough for all entropy headers

pub(super) unsafe fn finalize_dictionary(
    dictBuffer: *mut core::ffi::c_void,
    dictBufferCapacity: usize,
    customDictContent: *const core::ffi::c_void,
    mut dictContentSize: usize,
    samples: &[u8],
    samplesSizes: &[usize],
    params: ZDICT_params_t,
) -> Result<usize, Error> {
    let mut header: [u8; 256] = [0; 256];
    let compressionLevel = if params.compressionLevel == 0 {
        ZSTD_CLEVEL_DEFAULT
    } else {
        params.compressionLevel
    };
    let notificationLevel = params.notificationLevel;
    // the final dictionary content must be at least as large as the largest repcode
    let minContentSize = *repStartValue.iter().max().unwrap() as usize;

    // check conditions
    if dictBufferCapacity < dictContentSize {
        return Err(Error::dstSize_tooSmall);
    }
    if dictBufferCapacity < ZDICT_DICTSIZE_MIN {
        return Err(Error::dstSize_tooSmall);
    }

    // dictionary header
    header[..4].copy_from_slice(&ZSTD_MAGIC_DICTIONARY.to_le_bytes());
    let randomID = ZSTD_XXH64(customDictContent, dictContentSize, 0);
    let compliantID = (randomID % ((1 as core::ffi::c_uint) << 31).wrapping_sub(32768) as u64)
        .wrapping_add(32768) as u32;
    let dictID = if params.dictID != 0 {
        params.dictID
    } else {
        compliantID
    };
    header[4..][..4].copy_from_slice(&dictID.to_le_bytes());

    // entropy tables
    if notificationLevel >= 2 {
        eprintln!("\r{:70 }\r", ""); // clean display line
        eprintln!("statistics ...");
    }
    let hSize = 8;
    let eSize = ZDICT_analyzeEntropy(
        header[hSize..].as_mut_ptr() as *mut core::ffi::c_void,
        HBUFFSIZE.wrapping_sub(hSize),
        compressionLevel,
        samples,
        samplesSizes,
        customDictContent,
        dictContentSize,
        notificationLevel,
    )?;
    let hSize = hSize.wrapping_add(eSize);

    // shrink the content size if it doesn't fit in the buffer
    if hSize.wrapping_add(dictContentSize) > dictBufferCapacity {
        dictContentSize = dictBufferCapacity.wrapping_sub(hSize);
    }

    // pad the dictionary content with zeros if it is too small
    let paddingSize = if dictContentSize < minContentSize {
        if hSize.wrapping_add(minContentSize) > dictBufferCapacity {
            return Err(Error::dstSize_tooSmall);
        }
        minContentSize.wrapping_sub(dictContentSize)
    } else {
        0
    };

    let dictSize = hSize
        .wrapping_add(paddingSize)
        .wrapping_add(dictContentSize);

    // The dictionary consists of the header, optional padding, and the content.
    // The padding comes before the content because the "best" position in the
    // dictionary is the last byte.
    let outDictHeader = dictBuffer as *mut u8;
    let outDictPadding = outDictHeader.add(hSize);
    let outDictContent = outDictPadding.add(paddingSize);

    // First copy the `customDictContent` into its final location.
    // `customDictContent` and `dictBuffer` may overlap, so we must do this before
    // any other writes into the output buffer.
    core::ptr::copy(
        customDictContent.cast::<u8>(),
        outDictContent,
        dictContentSize,
    );
    // Then copy the header & padding into the output buffer.
    core::ptr::copy_nonoverlapping(header.as_ptr(), outDictHeader, hSize);
    core::ptr::write_bytes(outDictPadding, 0, paddingSize);

    Ok(dictSize)
}

unsafe fn ZDICT_addEntropyTablesFromBuffer_advanced(
    dictBuffer: &mut [MaybeUninit<u8>],
    dictContentSize: usize,
    samples: &[u8],
    samplesSizes: &[usize],
    params: ZDICT_params_t,
) -> usize {
    let dictBufferCapacity = dictBuffer.len();
    let dictBuffer = dictBuffer.as_mut_ptr().cast::<core::ffi::c_void>();

    let compressionLevel = if params.compressionLevel == 0 {
        ZSTD_CLEVEL_DEFAULT
    } else {
        params.compressionLevel
    };
    let notificationLevel = params.notificationLevel;
    let mut hSize = 8;

    // calculate entropy tables
    if notificationLevel >= 2 {
        eprintln!("\r{:70 }\r", ""); // clean display line
        eprintln!("statistics ...");
    }
    let res = ZDICT_analyzeEntropy(
        dictBuffer.byte_add(hSize),
        dictBufferCapacity.wrapping_sub(hSize),
        compressionLevel,
        samples,
        samplesSizes,
        dictBuffer
            .byte_add(dictBufferCapacity)
            .byte_sub(dictContentSize),
        dictContentSize,
        notificationLevel,
    );
    match res {
        Ok(eSize) => hSize = hSize.wrapping_add(eSize),
        Err(err) => return err.to_error_code(),
    }

    // add dictionary header (after entropy tables)
    MEM_writeLE32(dictBuffer, ZSTD_MAGIC_DICTIONARY);
    let randomID = ZSTD_XXH64(
        dictBuffer
            .byte_add(dictBufferCapacity)
            .byte_sub(dictContentSize),
        dictContentSize,
        0,
    );
    let compliantID = (randomID % ((1 as core::ffi::c_uint) << 31).wrapping_sub(32768) as u64)
        .wrapping_add(32768) as u32;
    let dictID = if params.dictID != 0 {
        params.dictID
    } else {
        compliantID
    };
    MEM_writeLE32(dictBuffer.byte_add(4), dictID);

    if hSize.wrapping_add(dictContentSize) < dictBufferCapacity {
        core::ptr::copy(
            dictBuffer
                .byte_add(dictBufferCapacity)
                .byte_sub(dictContentSize),
            dictBuffer.byte_add(hSize),
            dictContentSize,
        )
    }
    Ord::min(dictBufferCapacity, hSize.wrapping_add(dictContentSize))
}

/// Warning: `samplesBuffer` must be followed by the noisy guard band!!!
///
/// # Returns
///
/// - the size of the dictionary stored into `dictBuffer` (<= `dictBufferCapacity`)
/// - an error code, which can be tested with [`ZDICT_isError`]
fn ZDICT_trainFromBuffer_unsafe_legacy(
    dictBuffer: *mut core::ffi::c_void,
    maxDictSize: usize,
    samples: &[u8],
    samplesSizes: &[usize],
    params: ZDICT_legacy_params_t,
) -> usize {
    let nbSamples = samplesSizes.len();
    let dictListSize = Ord::max(Ord::max(10000, nbSamples), maxDictSize / 16);
    let mut dictList = vec![DictItem::default(); dictListSize];
    let selectivity = if params.selectivityLevel == 0 {
        g_selectivity_default
    } else {
        params.selectivityLevel
    };
    let minRep = if selectivity > 30 {
        MINRATIO
    } else {
        nbSamples >> selectivity
    };
    let targetDictSize = maxDictSize;
    let samplesBuffSize = samplesSizes.iter().sum();
    let notificationLevel = params.zParams.notificationLevel;

    // checks
    if maxDictSize < ZDICT_DICTSIZE_MIN {
        // requested dictionary size is too small
        return Error::dstSize_tooSmall.to_error_code();
    }
    if samplesBuffSize < ZDICT_MIN_SAMPLES_SIZE {
        // not enough source to create dictionary
        return Error::dictionaryCreation_failed.to_error_code();
    }

    dictList[0].init();

    // The samples must be followed by the noise band.
    debug_assert!(samples.len() >= samplesBuffSize + NOISELENGTH);

    // build dictionary
    ZDICT_trainBuffer_legacy(
        &mut dictList,
        samples,
        samplesBuffSize,
        samplesSizes,
        nbSamples,
        minRep,
        notificationLevel,
    );

    // display best matches
    if params.zParams.notificationLevel >= 3 {
        let nb = Ord::min(25, dictList[0].pos as usize);
        let dictContentSize = ZDICT_dictSize(&dictList);
        eprintln!(
            "\n {} segments found, of total size {} ",
            dictList[0].pos - 1,
            dictContentSize,
        );
        eprintln!("list {} best segments ", nb - 1);
        for (u, dictItem) in dictList[1..nb].iter().enumerate() {
            let pos = dictItem.pos as usize;
            let length = dictItem.length as usize;
            let printedLength = Ord::min(40, length);

            debug_assert!(pos + length <= samplesBuffSize);
            if pos > samplesBuffSize || pos + length > samplesBuffSize {
                return Error::GENERIC.to_error_code(); // should never happen
            }
            eprint!(
                "{:3}:{:3} bytes at pos {:8}, savings {:7} bytes |",
                u, length, pos, dictItem.savings,
            );
            ZDICT_printHex(&samples[..printedLength]);
            eprintln!("|");
        }
    }

    // create dictionary
    let dictContentSize = ZDICT_dictSize(&dictList);
    #[expect(deprecated)]
    if dictContentSize < ZDICT_CONTENTSIZE_MIN {
        // dictionary content too small
        return Error::dictionaryCreation_failed.to_error_code();
    }

    if dictContentSize < targetDictSize / 4 && notificationLevel >= 2 {
        eprintln!(
            "!  warning : selected content significantly smaller than requested ({} < {}) ",
            dictContentSize, maxDictSize,
        );
        if samplesBuffSize < 10 * targetDictSize {
            eprintln!(
                "!  consider increasing the number of samples (total size : {} MB)",
                samplesBuffSize >> 20,
            );
        }
        if minRep > MINRATIO {
            eprintln!(
                "!  consider increasing selectivity to produce larger dictionary (-s{}) ",
                selectivity.wrapping_add(1),
            );
            eprintln!(
                "!  note : larger dictionaries are not necessarily better, test its efficiency on samples "
            );
        }
    }

    if dictContentSize > targetDictSize * 3
        && nbSamples > 2 * MINRATIO
        && selectivity > 1
        && notificationLevel >= 2
    {
        let mut proposedSelectivity = selectivity.wrapping_sub(1);
        while nbSamples >> proposedSelectivity <= MINRATIO {
            proposedSelectivity = proposedSelectivity.wrapping_sub(1);
        }
        eprintln!(
            "!  note : calculated dictionary significantly larger than requested ({} > {}) ",
            dictContentSize, maxDictSize,
        );
        eprintln!(
            "!  consider increasing dictionary size, or produce denser dictionary (-s{}) ",
            proposedSelectivity,
        );
        eprintln!("!  always test dictionary efficiency on real samples");
    }

    // limit dictionary size
    let max = dictList[0].pos as usize; // convention: dictList[0].pos contains the number of useful elements
    let mut currentSize = 0usize;
    let mut n = 1usize;
    while n < max {
        currentSize = currentSize.wrapping_add(dictList[n].length as usize);
        if currentSize > targetDictSize {
            currentSize = currentSize.wrapping_sub(dictList[n].length as usize);
            break;
        } else {
            n = n.wrapping_add(1);
        }
    }
    dictList[0].pos = n as u32;
    let dictContentSize = currentSize;

    let dictBuffer = unsafe {
        if dictBuffer.is_null() || maxDictSize == 0 {
            &mut []
        } else {
            core::slice::from_raw_parts_mut(dictBuffer.cast::<MaybeUninit<u8>>(), maxDictSize)
        }
    };

    if let Err(e) = build_dictionary_content(dictBuffer, samples, &dictList) {
        return e.to_error_code();
    }

    unsafe {
        ZDICT_addEntropyTablesFromBuffer_advanced(
            dictBuffer,
            dictContentSize,
            samples,
            samplesSizes,
            params.zParams,
        )
    }
}

fn build_dictionary_content(
    dictBuffer: &mut [MaybeUninit<u8>],
    samples: &[u8],
    dictList: &[DictItem],
) -> Result<(), Error> {
    // convention: table[0].pos stores the number of elements
    let max = dictList[0].pos;

    let mut ptr = dictBuffer.len();
    for item in &dictList[1..max as usize] {
        let l = item.length as usize;
        ptr = match ptr.checked_sub(l) {
            None => return Err(Error::GENERIC), // should not happen
            Some(v) => v,
        };

        let src = uninit_slice(&samples[item.pos as usize..][..l]);
        dictBuffer[ptr..][..l].copy_from_slice(src);
    }

    Ok(())
}

fn uninit_slice<T>(slice: &[T]) -> &[MaybeUninit<T>] {
    unsafe { &*(slice as *const [T] as *const [MaybeUninit<T>]) }
}

/// Train a dictionary from an array of samples.
///
/// Samples must be stored concatenated in a single flat buffer `samplesBuffer`, supplied with an
/// array of sizes `samplesSizes`, providing the size of each sample, in order.
///
/// The resulting dictionary will be saved into `dictBuffer`.
///
/// `params` is optional and can be provided with values set to 0 to mean "default".
///
/// In general, a reasonable dictionary has a size of ~100 KB. It's possible to select smaller or
/// larger size, just by specifying `dictBufferCapacity`. In general, it's recommended to provide a
/// few thousands samples, though this can vary a lot. It's recommended that total size of all
/// samples be about ~x100 times the target size of dictionary.
///
/// # Returns
///
/// - the size of the dictionary stored into `dictBuffer` (<= `dictBufferCapacity`)
/// - an error code, which can be tested with [`ZDICT_isError`]
///
/// Dictionary training will fail if there are not enough samples to construct a dictionary, or if
/// most of the samples are too small (< 8 bytes being the lower limit). If dictionary training
/// fails, you should use zstd without a dictionary, as the dictionary would've been ineffective
/// anyways. If you believe your samples would benefit from a dictionary please open an issue with
/// details, and we can look into it.
///
/// # Safety
///
/// Behavior is undefined if any of the following conditions are violated:
///
/// - `dictBufferCapacity` is 0 or `dictBuffer` and `dictBufferCapacity` satisfy the requirements
///   of [`core::slice::from_raw_parts_mut`].
/// - `nbSamples` is 0 or `samplesSizes` and `nbSamples` satisfy the requirements
///   of [`core::slice::from_raw_parts`].
/// - `sum(samplesSizes)` is 0 or `samplesBuffer` and `sum(samplesSizes)` satisfy the requirements
///   of [`core::slice::from_raw_parts`].
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZDICT_trainFromBuffer_legacy))]
pub unsafe extern "C" fn ZDICT_trainFromBuffer_legacy(
    dictBuffer: *mut core::ffi::c_void,
    dictBufferCapacity: usize,
    samplesBuffer: *const core::ffi::c_void,
    samplesSizes: *const usize,
    nbSamples: core::ffi::c_uint,
    params: ZDICT_legacy_params_t,
) -> usize {
    let samplesSizes = if samplesSizes.is_null() || nbSamples == 0 {
        &[]
    } else {
        core::slice::from_raw_parts(samplesSizes, nbSamples as usize)
    };

    let sBuffSize: usize = samplesSizes.iter().sum();
    if sBuffSize < ZDICT_MIN_SAMPLES_SIZE {
        // not enough content => no dictionary
        return 0;
    }

    let mut new_buf = vec![0u8; sBuffSize.wrapping_add(NOISELENGTH)];
    core::ptr::copy_nonoverlapping(samplesBuffer.cast::<u8>(), new_buf.as_mut_ptr(), sBuffSize);
    fill_noise(&mut new_buf[sBuffSize..]); // guard band, for end of buffer condition

    ZDICT_trainFromBuffer_unsafe_legacy(
        dictBuffer,
        dictBufferCapacity,
        &new_buf,
        samplesSizes,
        params,
    )
}

/// Train a dictionary from an array of samples.
///
/// Calls single-threaded [`ZDICT_optimizeTrainFromBuffer_fastCover`], with `d=8`, `steps=4`,
/// `f=20`, and `accel=1`.
///
/// Samples must be stored concatenated in a single flat buffer `samplesBuffer`,  supplied with an
/// array of sizes `samplesSizes`, providing the size of each sample, in order. The resulting
/// dictionary will be saved into `dictBuffer`.
///
/// In general, a reasonable dictionary has a size of ~100 KB. It's possible to select smaller or
/// larger size, just by specifying `dictBufferCapacity`. In general, it's recommended to provide a
/// few thousands samples, though this can vary a lot. It's recommended that total size of all
/// samples be about ~x100 times the target size of dictionary.
///
/// # Returns
///
/// - the size of the dictionary stored into `dictBuffer` (<= `dictBufferCapacity`)
/// - an error code, which can be tested with [`ZDICT_isError`]
///
/// Dictionary training will fail if there are not enough samples to construct a dictionary, or if
/// most of the samples are too small (< 8 bytes being the lower limit). If dictionary training
/// fails, you should use zstd without a dictionary, as the dictionary would've been ineffective
/// anyways. If you believe your samples would benefit from a dictionary please open an issue with
/// details, and we can look into it.
///
/// # Safety
///
/// Behavior is undefined if any of the following conditions are violated:
///
/// - `dictBufferCapacity` is 0 or `dictBuffer` and `dictBufferCapacity` satisfy the requirements
///   of [`core::slice::from_raw_parts_mut`].
/// - `nbSamples` is 0 or `samplesSizes` and `nbSamples` satisfy the requirements
///   of [`core::slice::from_raw_parts`].
/// - `sum(samplesSizes)` is 0 or `samplesBuffer` and `sum(samplesSizes)` satisfy the requirements
///   of [`core::slice::from_raw_parts`].
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZDICT_trainFromBuffer))]
pub unsafe extern "C" fn ZDICT_trainFromBuffer(
    dictBuffer: *mut core::ffi::c_void,
    dictBufferCapacity: usize,
    samplesBuffer: *const core::ffi::c_void,
    samplesSizes: *const usize,
    nbSamples: core::ffi::c_uint,
) -> usize {
    let mut params = ZDICT_fastCover_params_t {
        d: 8,
        steps: 4,
        ..Default::default()
    };

    // Use default level since no compression level information is available
    params.zParams.compressionLevel = ZSTD_CLEVEL_DEFAULT;

    ZDICT_optimizeTrainFromBuffer_fastCover(
        dictBuffer,
        dictBufferCapacity,
        samplesBuffer,
        samplesSizes,
        nbSamples,
        &mut params,
    )
}

#[deprecated = "use ZDICT_finalizeDictionary() instead"]
#[cfg_attr(feature = "export-symbols", export_name = crate::prefix!(ZDICT_addEntropyTablesFromBuffer))]
pub unsafe extern "C" fn ZDICT_addEntropyTablesFromBuffer(
    dictBuffer: *mut core::ffi::c_void,
    dictContentSize: usize,
    dictBufferCapacity: usize,
    samplesBuffer: *const core::ffi::c_void,
    samplesSizes: *const usize,
    nbSamples: core::ffi::c_uint,
) -> usize {
    let dictBuffer = if dictBuffer.is_null() || dictBufferCapacity == 0 {
        &mut []
    } else {
        core::slice::from_raw_parts_mut(dictBuffer.cast::<MaybeUninit<u8>>(), dictBufferCapacity)
    };

    let samplesSizes = if samplesSizes.is_null() || nbSamples == 0 {
        &[]
    } else {
        core::slice::from_raw_parts(samplesSizes, nbSamples as usize)
    };
    let totalSamplesSize = samplesSizes.iter().sum::<usize>();
    let samples = if samplesBuffer.is_null() || totalSamplesSize == 0 {
        &[]
    } else {
        core::slice::from_raw_parts(samplesBuffer.cast::<u8>(), totalSamplesSize)
    };

    let params = ZDICT_params_t::default();
    ZDICT_addEntropyTablesFromBuffer_advanced(
        dictBuffer,
        dictContentSize,
        samples,
        samplesSizes,
        params,
    )
}

#[cfg(test)]
mod test {
    use super::*;

    const DICT: &[u8] = include_bytes!("../../test-libzstd-rs-sys/test-data/test-dict.dat");

    #[test]
    fn test_get_dict_header_size() {
        let empty: &[u8] = &[];
        let code = unsafe { ZDICT_getDictHeaderSize(empty.as_ptr().cast(), empty.len()) };
        assert_eq!(
            Error::from_error_code(code),
            Some(Error::dictionary_corrupted)
        );

        let no_magic: &[u8] = &[0; 8];
        let code = unsafe { ZDICT_getDictHeaderSize(no_magic.as_ptr().cast(), no_magic.len()) };
        assert_eq!(
            Error::from_error_code(code),
            Some(Error::dictionary_corrupted)
        );

        let code = unsafe { ZDICT_getDictHeaderSize(DICT.as_ptr().cast(), DICT.len()) };
        match Error::from_error_code(code) {
            Some(err) => panic!("{:?}", err),
            None => assert_eq!(code, 133),
        }
    }

    #[test]
    fn test_get_dict_id() {
        let empty: &[u8] = &[];
        let code = unsafe { ZDICT_getDictID(empty.as_ptr().cast(), empty.len()) };
        assert_eq!(code, 0);

        let no_magic: &[u8] = &[0; 8];
        let code = unsafe { ZDICT_getDictID(no_magic.as_ptr().cast(), no_magic.len()) };
        assert_eq!(code, 0);

        let code = unsafe { ZDICT_getDictID(DICT.as_ptr().cast(), DICT.len()) };
        assert_eq!(code, 1877512422);
    }

    #[test]
    fn test_fill_noise() {
        let mut buf = vec![0u8; 16];
        fill_noise(&mut buf);
        assert_eq!(
            buf,
            [226, 51, 247, 105, 221, 225, 137, 112, 5, 188, 15, 79, 183, 243, 110, 209]
        );
    }
}
