use core::marker::PhantomData;
use core::ops::{Bound, Range};
use core::ptr::NonNull;

use crate::lib::common::bitstream::{BIT_DStream_t, BitContainerType, StreamStatus};
use crate::lib::common::entropy_common::HUF_readStats_wksp;
use crate::lib::common::error_private::Error;
use crate::lib::common::huf::{
    HUF_flags_bmi2, HUF_flags_disableAsm, HUF_flags_disableFast, HUF_SYMBOLVALUE_MAX,
    HUF_TABLELOG_MAX,
};
use crate::lib::common::mem::{MEM_read64, MEM_readLEST, MEM_write16};
use crate::lib::decompress::Workspace;

#[cfg(any())] // blaze: upstream's x86_64 assembly loop (huf_decompress_amd64.S) is not in this copy
extern "C" {
    fn HUF_decompress4X1_usingDTable_internal_fast_asm_loop(args: &mut HUF_DecompressFastArgs);
    fn HUF_decompress4X2_usingDTable_internal_fast_asm_loop(args: &mut HUF_DecompressFastArgs);
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct algo_time_t {
    pub tableTime: u32,
    pub decode256Time: u32,
}

#[derive(Copy, Clone)]
#[repr(C)]
#[derive(Default)]
pub struct DTable {
    pub description: DTableDesc,
    pub data: DTableData,
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct DTableData {
    data: [u32; 4096],
}

impl DTableData {
    fn as_x1(&self) -> &[HUF_DEltX1; 4096] {
        unsafe { core::mem::transmute(&self.data) }
    }

    fn as_x1_mut(&mut self) -> &mut [HUF_DEltX1; 4096] {
        unsafe { core::mem::transmute(&mut self.data) }
    }

    fn as_x2(&self) -> &[HUF_DEltX2; 4096] {
        unsafe { core::mem::transmute(&self.data) }
    }

    fn as_x2_mut(&mut self) -> &mut [HUF_DEltX2; 4096] {
        unsafe { core::mem::transmute(&mut self.data) }
    }

    fn as_symbols(&self) -> &[u16; 2 * 4096] {
        unsafe { core::mem::transmute(&self.data) }
    }
}

impl Default for DTableData {
    fn default() -> Self {
        Self { data: [0; 4096] }
    }
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct DTableDesc {
    pub maxTableLog: u8,
    pub tableType: u8,
    pub tableLog: u8,
    pub reserved: u8,
}

impl Default for DTableDesc {
    fn default() -> Self {
        DTableDesc {
            maxTableLog: 12,
            tableType: 0,
            tableLog: 0,
            reserved: 0,
        }
    }
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct HUF_DEltX1 {
    pub nbBits: u8,
    pub byte: u8,
}
#[repr(C)]
pub struct HUF_ReadDTableX1_Workspace {
    rankVal: [u32; 13],
    rankStart: [u32; 13],
    statsWksp: crate::lib::common::entropy_common::Workspace,
    symbols: [u8; 256],
    huffWeight: [u8; 256],
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(C)]
pub struct HUF_DEltX2 {
    pub sequence: u16,
    pub nbBits: u8,
    pub length: u8,
}

pub type rankValCol_t = [u32; 13];
#[repr(C)]
pub struct HUF_ReadDTableX2_Workspace {
    rankVal: [rankValCol_t; 12],
    rankStats: [u32; 13],
    rankStart0: [u32; 15],
    sortedSymbol: [sortedSymbol_t; 256],
    weightList: [u8; 256],
    calleeWksp: crate::lib::common::entropy_common::Workspace,
}

impl Default for HUF_ReadDTableX2_Workspace {
    fn default() -> Self {
        Self {
            rankVal: [[0; 13]; 12],
            rankStats: [0; 13],
            rankStart0: [0; 15],
            sortedSymbol: [sortedSymbol_t::default(); 256],
            weightList: [0; 256],
            calleeWksp: crate::lib::common::entropy_common::Workspace::default(),
        }
    }
}

#[derive(Default, Copy, Clone)]
#[repr(C)]
pub struct sortedSymbol_t {
    pub symbol: u8,
}
#[repr(C)]
pub struct HUF_DecompressFastArgs<'a> {
    pub ip: [*const u8; 4],
    pub op: [*mut u8; 4],
    pub bits: [u64; 4],
    pub dt: &'a DTableData,
    pub ilowest: *const u8,
    pub oend: *mut u8,
    pub iend: [*const u8; 4],
}

pub const HUF_DECODER_FAST_TABLELOG: core::ffi::c_int = 11;
pub const HUF_ENABLE_FAST_DECODE: core::ffi::c_int = 1;

fn HUF_initFastDStream(ip: [u8; 8]) -> u64 {
    let lastByte = ip[7];
    let bitsConsumed = match lastByte.checked_ilog2() {
        Some(v) => 8 - v,
        None => 0,
    };
    let value = u64::from_le_bytes(ip) | 1;
    value << bitsConsumed
}

impl<'a> HUF_DecompressFastArgs<'a> {
    fn new(mut dst: Writer<'_>, src: &[u8], DTable: &'a DTable) -> Result<Option<Self>, Error> {
        // The fast decoding loop assumes 64-bit little-endian.
        if cfg!(target_endian = "big") || cfg!(target_pointer_width = "32") {
            return Ok(None);
        }

        if dst.is_empty() {
            return Ok(None);
        }

        // strict minimum : jump table + 1 byte per stream.
        let [b0, b1, b2, b3, b4, b5, _, _, _, _, ..] = *src else {
            return Err(Error::corruption_detected);
        };

        // Must have at least 8 bytes per stream because we don't handle initializing smaller bit containers.
        // If table log is not correct at this point, fallback to the old decoder.
        // On small inputs we don't have enough data to trigger the fast loop, so use the old decoder.
        let dtLog = DTable.description.tableLog as u32;
        if dtLog != HUF_DECODER_FAST_TABLELOG as u32 {
            return Ok(None);
        }

        let length1 = usize::from(u16::from_le_bytes([b0, b1]));
        let length2 = usize::from(u16::from_le_bytes([b2, b3]));
        let length3 = usize::from(u16::from_le_bytes([b4, b5]));
        let length4 = src.len().wrapping_sub(6 + length1 + length2 + length3);

        if 6 + length1 + length2 + length3 > src.len() {
            return Err(Error::corruption_detected);
        }

        let istart = src.as_ptr();
        let mut iend = [core::ptr::null(); 4];
        unsafe {
            iend[0] = istart.add(6); /* jumpTable */
            iend[1] = iend[0].add(length1);
            iend[2] = iend[1].add(length2);
            iend[3] = iend[2].add(length3);
        }

        // HUF_initFastDStream() requires this, and this small of an input won't benefit from the ASM loop anyways.
        if length1 < 8 || length2 < 8 || length3 < 8 || length4 < 8 {
            return Ok(None);
        }

        /* ip[] contains the position that is currently loaded into bits[]. */
        let mut ip = [core::ptr::null(); 4];
        unsafe {
            ip[0] = iend[1].sub(size_of::<u64>());
            ip[1] = iend[2].sub(size_of::<u64>());
            ip[2] = iend[3].sub(size_of::<u64>());
            ip[3] = src.as_ptr().add(src.len() - size_of::<u64>());
        }

        /* op[] contains the output pointers. */
        let mut op = [core::ptr::null_mut(); 4];
        unsafe {
            op[0] = dst.as_mut_ptr();
            op[1] = op[0].add(dst.capacity().div_ceil(4));
            op[2] = op[1].add(dst.capacity().div_ceil(4));
            op[3] = op[2].add(dst.capacity().div_ceil(4));
        }

        // No point to call the ASM loop for tiny outputs.
        if op[3] >= dst.as_mut_ptr_range().end {
            return Ok(None);
        }

        // bits[] is the bit container.
        //
        // It is read from the MSB down to the LSB.
        // It is shifted left as it is read, and zeros are
        // shifted in. After the lowest valid bit a 1 is
        // set, so that CountTrailingZeros(bits[]) can be used
        // to count how many bits we've consumed.
        let ends = [
            6 + length1,
            6 + length1 + length2,
            6 + length1 + length2 + length3,
            src.len(),
        ];
        let bits = ends.map(|end| HUF_initFastDStream(*src[..end].last_chunk().unwrap()));

        // The decoders must be sure to never read beyond ilowest.
        // This is lower than iend[0], but allowing decoders to read
        // down to ilowest can allow an extra iteration or two in the
        // fast loop.
        let args = Self {
            ip,
            op,
            bits,
            dt: &DTable.data,
            ilowest: istart,
            oend: dst.as_mut_ptr_range().end,
            iend,
        };

        Ok(Some(args))
    }
}

unsafe fn init_remaining_dstream<'a>(
    args: &'a HUF_DecompressFastArgs,
    stream: usize,
    segmentEnd: *mut u8,
) -> Result<BIT_DStream_t<'a>, Error> {
    if args.op[stream] > segmentEnd {
        return Err(Error::corruption_detected);
    }

    if args.ip[stream] < args.iend[stream].sub(8) {
        return Err(Error::corruption_detected);
    }

    let bitContainer = MEM_readLEST(args.ip[stream] as *const core::ffi::c_void);
    let bitsConsumed = args.bits[stream].trailing_zeros();
    let start = args.ilowest as *const core::ffi::c_char;
    let limitPtr = start.add(size_of::<usize>());
    let ptr = args.ip[stream] as *const core::ffi::c_char;

    Ok(BIT_DStream_t {
        bitContainer,
        bitsConsumed,
        ptr,
        start,
        limitPtr,
        _marker: PhantomData,
    })
}

/// Increase the tableLog to targetTableLog and rescales the stats.
///
/// If tableLog > targetTableLog this is a no-op.
/// @returns New tableLog
fn HUF_rescaleStats(
    huffWeight: &mut [u8; 256],
    rankVal: &mut rankValCol_t,
    nbSymbols: u32,
    tableLog: u32,
    targetTableLog: u32,
) -> u32 {
    if tableLog > targetTableLog {
        return tableLog;
    }

    if tableLog < targetTableLog {
        let scale = targetTableLog as usize - tableLog as usize;

        /* Increase the weight for all non-zero probability symbols by scale. */
        for weight in huffWeight[..nbSymbols as usize].iter_mut() {
            *weight += (if *weight == 0 { 0 } else { scale }) as u8;
        }

        // Update rankVal to reflect the new weights.
        // All weights except 0 get moved to weight + scale.
        // Weights [1, scale] are empty.
        for s in (scale + 1..targetTableLog as usize + 1).rev() {
            rankVal[s] = rankVal[s - scale];
        }

        rankVal[1..=scale].fill(0);
    }

    targetTableLog
}

#[inline(always)]
fn HUF_fillDTableX1<const N: usize>(dt: &mut [HUF_DEltX1], symbols: &[u8], nbBits: u8) {
    let (chunks, _) = dt.as_chunks_mut::<N>();
    for (chunk, &byte) in chunks.iter_mut().zip(symbols) {
        *chunk = [HUF_DEltX1 { nbBits, byte }; N];
    }
}

pub fn HUF_readDTableX1_wksp(
    DTable: &mut DTable,
    src: &[u8],
    workSpace: &mut Workspace,
    flags: core::ffi::c_int,
) -> Result<usize, Error> {
    let mut dtd = DTable.description;
    let dt = DTable.data.as_x1_mut();

    let mut tableLog = 0;
    let mut nbSymbols = 0;

    let wksp = workSpace.as_x1_mut();

    let iSize = HUF_readStats_wksp(
        &mut wksp.huffWeight,
        (HUF_SYMBOLVALUE_MAX + 1) as usize,
        &mut wksp.rankVal,
        &mut nbSymbols,
        &mut tableLog,
        src,
        &mut wksp.statsWksp,
        flags,
    )?;

    let maxTableLog = (dtd.maxTableLog as core::ffi::c_int + 1) as u32;
    let targetTableLog = if maxTableLog < 11 { maxTableLog } else { 11 };
    tableLog = HUF_rescaleStats(
        &mut wksp.huffWeight,
        &mut wksp.rankVal,
        nbSymbols,
        tableLog,
        targetTableLog,
    );
    if tableLog > (dtd.maxTableLog as core::ffi::c_int + 1) as u32 {
        return Err(Error::tableLog_tooLarge);
    }
    dtd.tableType = 0;
    dtd.tableLog = tableLog as u8;

    DTable.description = dtd;

    // Compute symbols and rankStart given rankVal:
    //
    // rankVal already contains the number of values of each weight.
    //
    // symbols contains the symbols ordered by weight. First are the rankVal[0]
    // weight 0 symbols, followed by the rankVal[1] weight 1 symbols, and so on.
    // symbols[0] is filled (but unused) to avoid a branch.
    //
    // rankStart contains the offset where each rank belongs in the DTable.
    // rankStart[0] is not filled because there are no entries in the table for
    // weight 0.
    let mut nextRankStart = 0u32;
    for n in 0..tableLog as usize + 1 {
        let curr = nextRankStart;
        nextRankStart += wksp.rankVal[n];
        wksp.rankStart[n] = curr;
    }

    for (n, w) in wksp.huffWeight[..nbSymbols as usize].iter().enumerate() {
        let w = usize::from(*w);
        wksp.symbols[wksp.rankStart[w] as usize] = n as u8;
        wksp.rankStart[w] += 1;
    }

    // fill DTable
    //
    // We fill all entries of each weight in order.
    // That way length is a constant for each iteration of the outer loop.
    // We can switch based on the length to a different inner loop which is
    // optimized for that particular case.
    let mut symbol = wksp.rankVal[0] as usize;
    let mut rankStart = 0;
    for w in 1..tableLog.wrapping_add(1) {
        let symbolCount = wksp.rankVal[w as usize] as usize;
        let length = 1 << w >> 1;
        let dt = &mut dt[rankStart..][..length * symbolCount];
        let symbols = &wksp.symbols[symbol..][..symbolCount];
        let nbBits = tableLog.wrapping_add(1).wrapping_sub(w) as u8;

        match length {
            1 => HUF_fillDTableX1::<1>(dt, symbols, nbBits),
            2 => HUF_fillDTableX1::<2>(dt, symbols, nbBits),
            4 => HUF_fillDTableX1::<4>(dt, symbols, nbBits),
            8 => HUF_fillDTableX1::<8>(dt, symbols, nbBits),
            _ => {
                for (chunk, &byte) in dt.chunks_exact_mut(length).zip(symbols) {
                    chunk.fill(HUF_DEltX1 { nbBits, byte });
                }
            }
        }

        symbol += symbolCount;
        rankStart += symbolCount * length;
    }

    Ok(iSize)
}

#[inline(always)]
fn HUF_decodeSymbolX1(Dstream: &mut BIT_DStream_t, dt: &[HUF_DEltX1; 4096], dtLog: u32) -> u8 {
    let HUF_DEltX1 { byte, nbBits, .. } = dt[Dstream.look_bits_fast(dtLog)];
    Dstream.skip_bits(u32::from(nbBits));
    byte
}

#[inline(always)]
fn HUF_decodeStreamX1(
    mut p: Writer<'_>,
    bitDPtr: &mut BIT_DStream_t,
    dt: &[HUF_DEltX1; 4096],
    dtLog: u32,
) -> usize {
    let capacity = p.capacity();

    if p.capacity() >= 4 {
        while bitDPtr.reload() == StreamStatus::Unfinished && p.capacity() >= 4 {
            if cfg!(target_pointer_width = "64") {
                p.write_u8(HUF_decodeSymbolX1(bitDPtr, dt, dtLog));
            }
            if cfg!(target_pointer_width = "64") || HUF_TABLELOG_MAX <= 12 {
                p.write_u8(HUF_decodeSymbolX1(bitDPtr, dt, dtLog));
            }
            if cfg!(target_pointer_width = "64") {
                p.write_u8(HUF_decodeSymbolX1(bitDPtr, dt, dtLog));
            }
            p.write_u8(HUF_decodeSymbolX1(bitDPtr, dt, dtLog));
        }
    } else {
        bitDPtr.reload();
    }

    if cfg!(target_pointer_width = "32") {
        while bitDPtr.reload() == StreamStatus::Unfinished && !p.is_empty() {
            p.write_u8(HUF_decodeSymbolX1(bitDPtr, dt, dtLog));
        }
    }

    while !p.is_empty() {
        p.write_u8(HUF_decodeSymbolX1(bitDPtr, dt, dtLog));
    }

    capacity - p.capacity()
}

#[inline(always)]
fn HUF_decompress1X1_usingDTable_internal_body(
    mut dst: Writer<'_>,
    src: &[u8],
    DTable: &DTable,
) -> Result<usize, Error> {
    let dt = DTable.data.as_x1();
    let dtd = DTable.description;
    let dtLog = dtd.tableLog as u32;

    let mut bitD = BIT_DStream_t::new(src)?;

    HUF_decodeStreamX1(dst.subslice(..), &mut bitD, dt, dtLog);

    if !bitD.is_empty() {
        return Err(Error::corruption_detected);
    }

    Ok(dst.capacity())
}

#[inline(always)]
fn HUF_decompress4X1_usingDTable_internal_body(
    mut dst: Writer<'_>,
    src: &[u8],
    DTable: &DTable,
) -> Result<usize, Error> {
    // strict minimum : jump table + 1 byte per stream.
    let [b0, b1, b2, b3, b4, b5, _, _, _, _, ..] = *src else {
        return Err(Error::corruption_detected);
    };

    if dst.capacity() < 6 {
        return Err(Error::corruption_detected);
    }

    let length1 = usize::from(u16::from_le_bytes([b0, b1]));
    let length2 = usize::from(u16::from_le_bytes([b2, b3]));
    let length3 = usize::from(u16::from_le_bytes([b4, b5]));

    if 6 + length1 + length2 + length3 > src.len() {
        return Err(Error::corruption_detected);
    }

    let istart1 = &src[6..][..length1];
    let istart2 = &src[6 + length1..][..length2];
    let istart3 = &src[6 + length1 + length2..][..length3];
    let istart4 = &src[6 + length1 + length2 + length3..];

    let Some((mut w1, mut w2, mut w3, mut w4)) = dst.quarter() else {
        return Err(Error::corruption_detected);
    };

    let mut end_signal = true;

    let mut bitD1 = BIT_DStream_t::new(istart1)?;
    let mut bitD2 = BIT_DStream_t::new(istart2)?;
    let mut bitD3 = BIT_DStream_t::new(istart3)?;
    let mut bitD4 = BIT_DStream_t::new(istart4)?;

    let dt = DTable.data.as_x1();
    let dtLog = DTable.description.tableLog as u32;

    if w4.capacity() >= size_of::<usize>() {
        while end_signal && w4.capacity() >= 4 {
            if cfg!(target_pointer_width = "64") {
                w1.write_u8(HUF_decodeSymbolX1(&mut bitD1, dt, dtLog));
                w2.write_u8(HUF_decodeSymbolX1(&mut bitD2, dt, dtLog));
                w3.write_u8(HUF_decodeSymbolX1(&mut bitD3, dt, dtLog));
                w4.write_u8(HUF_decodeSymbolX1(&mut bitD4, dt, dtLog));
            }

            if cfg!(target_pointer_width = "64") || HUF_TABLELOG_MAX <= 12 {
                w1.write_u8(HUF_decodeSymbolX1(&mut bitD1, dt, dtLog));
                w2.write_u8(HUF_decodeSymbolX1(&mut bitD2, dt, dtLog));
                w3.write_u8(HUF_decodeSymbolX1(&mut bitD3, dt, dtLog));
                w4.write_u8(HUF_decodeSymbolX1(&mut bitD4, dt, dtLog));
            }

            if cfg!(target_pointer_width = "64") {
                w1.write_u8(HUF_decodeSymbolX1(&mut bitD1, dt, dtLog));
                w2.write_u8(HUF_decodeSymbolX1(&mut bitD2, dt, dtLog));
                w3.write_u8(HUF_decodeSymbolX1(&mut bitD3, dt, dtLog));
                w4.write_u8(HUF_decodeSymbolX1(&mut bitD4, dt, dtLog));
            }

            w1.write_u8(HUF_decodeSymbolX1(&mut bitD1, dt, dtLog));
            w2.write_u8(HUF_decodeSymbolX1(&mut bitD2, dt, dtLog));
            w3.write_u8(HUF_decodeSymbolX1(&mut bitD3, dt, dtLog));
            w4.write_u8(HUF_decodeSymbolX1(&mut bitD4, dt, dtLog));

            end_signal &= bitD1.reload_fast() == StreamStatus::Unfinished;
            end_signal &= bitD2.reload_fast() == StreamStatus::Unfinished;
            end_signal &= bitD3.reload_fast() == StreamStatus::Unfinished;
            end_signal &= bitD4.reload_fast() == StreamStatus::Unfinished;
        }
    }

    HUF_decodeStreamX1(w1, &mut bitD1, dt, dtLog);
    HUF_decodeStreamX1(w2, &mut bitD2, dt, dtLog);
    HUF_decodeStreamX1(w3, &mut bitD3, dt, dtLog);
    HUF_decodeStreamX1(w4, &mut bitD4, dt, dtLog);

    if !(bitD1.is_empty() && bitD2.is_empty() && bitD3.is_empty() && bitD4.is_empty()) {
        return Err(Error::corruption_detected);
    }

    Ok(dst.capacity())
}

#[cfg_attr(target_arch = "x86_64", target_feature(enable = "bmi2"))]
fn HUF_decompress4X1_usingDTable_internal_bmi2(
    dst: Writer<'_>,
    src: &[u8],
    DTable: &DTable,
) -> Result<usize, Error> {
    HUF_decompress4X1_usingDTable_internal_body(dst, src, DTable)
}

fn HUF_decompress4X1_usingDTable_internal_default(
    dst: Writer<'_>,
    src: &[u8],
    DTable: &DTable,
) -> Result<usize, Error> {
    HUF_decompress4X1_usingDTable_internal_body(dst, src, DTable)
}

macro_rules! HUF_4X_FOR_EACH_STREAM_WITH_VAR {
    ($mac:ident, $var:literal) => {
        $mac!(0, $var);
        $mac!(1, $var);
        $mac!(2, $var);
        $mac!(3, $var);
    };
}

macro_rules! HUF_4X_FOR_EACH_STREAM {
    ($mac:ident ) => {
        $mac!(0);
        $mac!(1);
        $mac!(2);
        $mac!(3);
    };
}

unsafe extern "C" fn HUF_decompress4X1_usingDTable_internal_fast_c_loop(
    args: &mut HUF_DecompressFastArgs,
) {
    let dtable = args.dt.as_symbols();
    let oend = args.oend;
    let ilowest = args.ilowest;

    // Copy the arguments to local variables.
    let mut bits = args.bits;
    let mut ip = args.ip;
    let mut op = args.op;

    #[allow(clippy::assertions_on_constants)]
    {
        debug_assert!(cfg!(target_endian = "little"));
        debug_assert!(cfg!(target_pointer_width = "64"));
    }

    'out: loop {
        /* Assert loop preconditions */
        if cfg!(debug_assertions) {
            for stream in 0..4 {
                debug_assert!(op[stream] <= (if stream == 3 { oend } else { op[stream + 1] }));
                debug_assert!(ip[stream] >= ilowest);
            }
        }

        /* Compute olimit */

        // Each iteration consumes up to 11 bits * 5 = 55 bits < 7 bytes per stream.
        let oiters = oend.offset_from_unsigned(op[3]) / 5;
        let iiters = ip[0].offset_from_unsigned(ilowest) / 7;
        let iters = Ord::min(oiters, iiters);
        let symbols = iters * 5;

        // We can simply check that op[3] < olimit, instead of checking all
        // of our bounds, since we can't hit the other bounds until we've run
        // iters iterations, which only happens when op[3] == olimit.
        let olimit = op[3].add(symbols);

        /* Exit fast decoding loop once we reach the end. */
        if op[3] == olimit {
            break;
        }

        // Exit the decoding loop if any input pointer has crossed the
        // previous one. This indicates corruption, and a precondition
        // to our loop is that ip[i] >= ip[0].
        for stream in 1..4 {
            if ip[stream] < ip[stream - 1] {
                break 'out;
            }
        }

        if cfg!(debug_assertions) {
            for stream in 1..4 {
                debug_assert!(ip[stream] >= ip[stream - 1]);
            }
        }

        macro_rules! HUF_4X1_DECODE_SYMBOL {
            ($stream:expr, $symbol:expr) => {
                let index = bits[$stream] >> 53;
                let entry = dtable[index as usize];

                bits[$stream] <<= entry & 0x3F;
                op[$stream]
                    .offset($symbol)
                    .write(((entry >> 8) & 0xFF) as u8)
            };
        }

        macro_rules! HUF_4X1_RELOAD_STREAM {
            ($stream: expr) => {
                let ctz = bits[$stream].trailing_zeros();
                let nbBits = ctz & 7;
                let nbBytes = ctz >> 3;

                op[$stream] = op[$stream].add(5);
                ip[$stream] = ip[$stream].sub(nbBytes as usize);
                bits[$stream] = MEM_read64(ip[$stream] as *const core::ffi::c_void) | 1;
                bits[$stream] <<= nbBits;
            };
        }

        /* Manually unroll the loop because compilers don't consistently
         * unroll the inner loops, which destroys performance.
         */
        loop {
            /* Decode 5 symbols in each of the 4 streams */
            HUF_4X_FOR_EACH_STREAM_WITH_VAR!(HUF_4X1_DECODE_SYMBOL, 0);
            HUF_4X_FOR_EACH_STREAM_WITH_VAR!(HUF_4X1_DECODE_SYMBOL, 1);
            HUF_4X_FOR_EACH_STREAM_WITH_VAR!(HUF_4X1_DECODE_SYMBOL, 2);
            HUF_4X_FOR_EACH_STREAM_WITH_VAR!(HUF_4X1_DECODE_SYMBOL, 3);
            HUF_4X_FOR_EACH_STREAM_WITH_VAR!(HUF_4X1_DECODE_SYMBOL, 4);

            /* Reload each of the 4 the bitstreams */
            HUF_4X_FOR_EACH_STREAM!(HUF_4X1_RELOAD_STREAM);

            if op[3] >= olimit {
                break;
            }
        }
    }

    // Save the final values of each of the state variables back to args.
    args.bits = bits;
    args.ip = ip;
    args.op = op;
}

pub type HUF_DecompressFastLoopFn = unsafe extern "C" fn(&mut HUF_DecompressFastArgs) -> ();
unsafe fn HUF_decompress4X1_usingDTable_internal_fast(
    mut dst: Writer<'_>,
    src: &[u8],
    DTable: &DTable,
    loopFn: HUF_DecompressFastLoopFn,
) -> Result<usize, Error> {
    let oend = dst.as_mut_ptr_range().end;

    let Some(mut args) = HUF_DecompressFastArgs::new(dst.subslice(..), src, DTable)? else {
        return Ok(0);
    };

    debug_assert!(args.ip[0] >= args.ilowest);
    loopFn(&mut args);

    // Our loop guarantees that ip[] >= ilowest and that we haven't overwritten any op[].
    let istart = src.as_ptr();
    debug_assert!(args.ip[0] >= istart);
    debug_assert!(args.ip[1] >= istart);
    debug_assert!(args.ip[2] >= istart);
    debug_assert!(args.ip[3] >= istart);
    debug_assert!(args.op[3] <= oend);

    debug_assert_eq!(istart, args.ilowest);
    debug_assert_eq!(istart.wrapping_add(6), args.iend[0]);

    let segmentSize = dst.capacity().div_ceil(4);
    let mut segmentEnd = dst.as_mut_ptr_range().start;

    // Finish bit streams one by one.
    for (i, op) in args.op.iter().copied().enumerate() {
        segmentEnd = Ord::min(segmentEnd.wrapping_add(segmentSize), oend);

        let mut bit = init_remaining_dstream(&args, i, segmentEnd)?;

        // Decompress and validate that we've produced exactly the expected length.
        let length = HUF_decodeStreamX1(
            Writer::from_raw_parts(op, segmentEnd as usize - op as usize),
            &mut bit,
            DTable.data.as_x1(),
            HUF_DECODER_FAST_TABLELOG as u32,
        );

        if op.wrapping_add(length) != segmentEnd {
            return Err(Error::corruption_detected);
        }
    }

    Ok(dst.capacity())
}

#[cfg_attr(target_arch = "x86_64", target_feature(enable = "bmi2"))]
fn HUF_decompress1X1_usingDTable_internal_bmi2(
    dst: Writer<'_>,
    src: &[u8],
    DTable: &DTable,
) -> Result<usize, Error> {
    HUF_decompress1X1_usingDTable_internal_body(dst, src, DTable)
}

fn HUF_decompress1X1_usingDTable_internal_default(
    dst: Writer<'_>,
    src: &[u8],
    DTable: &DTable,
) -> Result<usize, Error> {
    HUF_decompress1X1_usingDTable_internal_body(dst, src, DTable)
}

fn HUF_decompress1X1_usingDTable_internal(
    dst: Writer<'_>,
    src: &[u8],
    DTable: &DTable,
    flags: core::ffi::c_int,
) -> Result<usize, Error> {
    match flags & HUF_flags_bmi2 as i32 {
        0 => HUF_decompress1X1_usingDTable_internal_default(dst, src, DTable),
        _ => unsafe { HUF_decompress1X1_usingDTable_internal_bmi2(dst, src, DTable) },
    }
}

fn HUF_decompress4X1_usingDTable_internal(
    mut dst: Writer<'_>,
    src: &[u8],
    DTable: &DTable,
    flags: core::ffi::c_int,
) -> Result<usize, Error> {
    if flags & HUF_flags_bmi2 as core::ffi::c_int != 0 {
        let loopFn = match flags & HUF_flags_disableAsm as i32 {
            #[cfg(any())] // blaze: upstream's x86_64 assembly loop (huf_decompress_amd64.S) is not in this copy
            0 => HUF_decompress4X1_usingDTable_internal_fast_asm_loop as HUF_DecompressFastLoopFn,
            _ => HUF_decompress4X1_usingDTable_internal_fast_c_loop as HUF_DecompressFastLoopFn,
        };

        if HUF_ENABLE_FAST_DECODE != 0 && flags & HUF_flags_disableFast as core::ffi::c_int == 0 {
            let ret = unsafe {
                HUF_decompress4X1_usingDTable_internal_fast(dst.subslice(..), src, DTable, loopFn)
            }?;
            if ret != 0 {
                return Ok(ret);
            }
        }

        // SAFETY: bmi2 is enabled.
        unsafe { HUF_decompress4X1_usingDTable_internal_bmi2(dst, src, DTable) }
    } else {
        HUF_decompress4X1_usingDTable_internal_default(dst, src, DTable)
    }
}

fn HUF_decompress4X1_DCtx_wksp(
    dctx: &mut DTable,
    dst: Writer<'_>,
    src: &[u8],
    workSpace: &mut Workspace,
    flags: core::ffi::c_int,
) -> Result<usize, Error> {
    let hSize = HUF_readDTableX1_wksp(dctx, src, workSpace, flags)?;
    if hSize >= src.len() {
        return Err(Error::srcSize_wrong);
    }

    HUF_decompress4X1_usingDTable_internal(dst, &src[hSize..], dctx, flags)
}

impl HUF_DEltX2 {
    #[inline]
    fn from_u32(value: u32) -> Self {
        let [a, b, c, d] = value.to_le_bytes();

        Self {
            sequence: u16::from_le_bytes([a, b]),
            nbBits: c,
            length: d,
        }
    }
}

fn HUF_buildDEltX2U32(symbol: u32, nbBits: u32, baseSeq: u32, level: core::ffi::c_int) -> u32 {
    const _: () = assert!(core::mem::offset_of!(HUF_DEltX2, sequence) == 0);
    const _: () = assert!(core::mem::offset_of!(HUF_DEltX2, nbBits) == 2);
    const _: () = assert!(core::mem::offset_of!(HUF_DEltX2, length) == 3);
    const _: () = assert!(size_of::<HUF_DEltX2>() == size_of::<u32>());

    let seq = if level == 1 {
        symbol
    } else {
        baseSeq.wrapping_add(symbol << 8)
    };

    // FIXME: properly encode as big-endian on big-endian targets. This is tricky because in theory
    // the wrapping additions above can influence higher bytes. In practice apparently that does not
    // happen.
    seq.wrapping_add(nbBits << 16)
        .wrapping_add((level as u32) << 24)
}

fn HUF_buildDEltX2(symbol: u8, nbBits: u32, baseSeq: u16, level: core::ffi::c_int) -> HUF_DEltX2 {
    HUF_DEltX2::from_u32(HUF_buildDEltX2U32(
        symbol as u32,
        nbBits,
        baseSeq as u32,
        level,
    ))
}

fn HUF_fillDTableX2ForWeight(
    DTableRank: &mut [HUF_DEltX2],
    sorted_symbols: &[sortedSymbol_t],
    nbBits: u32,
    tableLog: u32,
    baseSeq: u16,
    level: core::ffi::c_int,
) {
    let length = 1 << (tableLog.wrapping_sub(nbBits) & 0x1f);
    let chunks = DTableRank[..sorted_symbols.len() * length].chunks_exact_mut(length);

    for (sorted_symbol, chunk) in sorted_symbols.iter().zip(chunks) {
        let DElt = HUF_buildDEltX2(sorted_symbol.symbol, nbBits, baseSeq, level);
        chunk.fill(DElt);
    }
}

fn HUF_fillDTableX2Level2(
    DTable: &mut [HUF_DEltX2],
    targetLog: u32,
    consumedBits: u32,
    rankVal: &[u32; 13],
    minWeight: core::ffi::c_int,
    maxWeight1: core::ffi::c_int,
    sortedSymbols: &[sortedSymbol_t; 256],
    rankStart: &[u32; 15],
    nbBitsBaseline: u32,
    baseSeq: u16,
) {
    if minWeight > 1 {
        let length = 1 << (targetLog.wrapping_sub(consumedBits) & 0x1f);
        let elem = HUF_DEltX2::from_u32(HUF_buildDEltX2U32(baseSeq as u32, consumedBits, 0, 1));
        let skipSize = rankVal[minWeight as usize];
        match length {
            2 => {
                DTable[..2].fill(elem);
            }
            4 => {
                DTable[..4].fill(elem);
            }
            _ => {
                for i in (0..skipSize as usize).step_by(8) {
                    DTable[i..][..8].fill(elem);
                }
            }
        }
    }

    for w in minWeight as usize..maxWeight1 as usize {
        let nbBits = nbBitsBaseline.wrapping_sub(w as u32);
        let totalBits = nbBits.wrapping_add(consumedBits);

        HUF_fillDTableX2ForWeight(
            &mut DTable[rankVal[w] as usize..],
            &sortedSymbols[rankStart[w] as usize..rankStart[w + 1] as usize],
            totalBits,
            targetLog,
            baseSeq,
            2,
        );
    }
}

fn HUF_fillDTableX2(
    DTable: &mut [HUF_DEltX2; 4096],
    targetLog: u32,
    sortedList: &[sortedSymbol_t; 256],
    rankStart: &[u32; 15],
    rankValOrigin: &mut [rankValCol_t; 12],
    maxWeight: u32,
    nbBitsBaseline: u32,
) {
    let rankVal = rankValOrigin[0];
    let scaleLog = nbBitsBaseline.wrapping_sub(targetLog) as core::ffi::c_int;
    let minBits = nbBitsBaseline.wrapping_sub(maxWeight);

    let wEnd = maxWeight as core::ffi::c_int + 1;
    for w in 1..wEnd as usize {
        let range = rankStart[w] as usize..rankStart[w + 1] as usize;

        let nbBits = nbBitsBaseline.wrapping_sub(w as u32);
        if targetLog.wrapping_sub(nbBits) >= minBits {
            let mut start = rankVal[w] as core::ffi::c_int;
            let length = 1 << (targetLog.wrapping_sub(nbBits) & 0x1f as core::ffi::c_int as u32);
            let minWeight = Ord::max(nbBits.wrapping_add(scaleLog as u32) as core::ffi::c_int, 1);

            for s in range {
                HUF_fillDTableX2Level2(
                    &mut DTable[start as usize..],
                    targetLog,
                    nbBits,
                    &rankValOrigin[nbBits as usize],
                    minWeight,
                    wEnd,
                    sortedList,
                    rankStart,
                    nbBitsBaseline,
                    u16::from(sortedList[s].symbol),
                );
                start = (start as u32).wrapping_add(length) as core::ffi::c_int as core::ffi::c_int;
            }
        } else {
            HUF_fillDTableX2ForWeight(
                &mut DTable[rankVal[w] as usize..],
                &sortedList[range],
                nbBits,
                targetLog,
                0,
                1,
            );
        }
    }
}

pub fn HUF_readDTableX2_wksp(
    DTable: &mut DTable,
    src: &[u8],
    wksp: &mut HUF_ReadDTableX2_Workspace,
    flags: core::ffi::c_int,
) -> Result<usize, Error> {
    let mut dtd = DTable.description;

    let mut tableLog: u32 = 0;
    let mut nbSymbols: u32 = 0;
    let mut maxTableLog = dtd.maxTableLog as u32;

    let dt = DTable.data.as_x2_mut();

    wksp.rankStats.fill(0);
    wksp.rankStart0.fill(0);
    let rankStart = &mut wksp.rankStart0[1..];

    if maxTableLog > HUF_TABLELOG_MAX as u32 {
        return Err(Error::tableLog_tooLarge);
    }

    let iSize = HUF_readStats_wksp(
        &mut wksp.weightList,
        (HUF_SYMBOLVALUE_MAX + 1) as usize,
        &mut wksp.rankStats,
        &mut nbSymbols,
        &mut tableLog,
        src,
        &mut wksp.calleeWksp,
        flags,
    )?;
    if tableLog > maxTableLog {
        return Err(Error::tableLog_tooLarge);
    }
    if tableLog <= HUF_DECODER_FAST_TABLELOG as u32
        && maxTableLog > HUF_DECODER_FAST_TABLELOG as u32
    {
        maxTableLog = HUF_DECODER_FAST_TABLELOG as u32;
    }

    /* find maxWeight */
    let mut maxW: u32 = tableLog;
    while wksp.rankStats[maxW as usize] == 0 {
        maxW = maxW.wrapping_sub(1);
    }

    /* Get start index of each weight */
    let mut nextRankStart = 0u32;
    for w in 1..maxW + 1 {
        let curr = nextRankStart;
        nextRankStart += wksp.rankStats[w as usize];
        rankStart[w as usize] = curr;
    }

    rankStart[0] = nextRankStart;
    rankStart[maxW.wrapping_add(1) as usize] = nextRankStart;

    /* sort symbols by weight */
    for s in 0..nbSymbols {
        let w = usize::from(wksp.weightList[s as usize]);
        let r = rankStart[w];
        rankStart[w] += 1;
        wksp.sortedSymbol[r as usize].symbol = s as u8;
    }

    /* forget 0w symbols; this is beginning of weight(1) */
    rankStart[0] = 0;

    /* Build rankVal */
    let rescale = maxTableLog.wrapping_sub(tableLog).wrapping_sub(1) as core::ffi::c_int;
    let mut nextRankVal = 0u32;

    // Zero the memory to prevent uninitialized memory.
    wksp.rankVal = [[0u32; 13]; 12];

    for w in 1..maxW.wrapping_add(1) {
        let curr = nextRankVal;
        nextRankVal =
            nextRankVal.wrapping_add(wksp.rankStats[w as usize] << w.wrapping_add(rescale as u32));

        wksp.rankVal[0][w as usize] = curr;
    }

    let minBits = tableLog.wrapping_add(1).wrapping_sub(maxW);
    for consumed in minBits..maxTableLog.wrapping_sub(minBits).wrapping_add(1) {
        for w in 1..maxW.wrapping_add(1) as usize {
            wksp.rankVal[consumed as usize][w] = wksp.rankVal[0][w] >> consumed;
        }
    }

    HUF_fillDTableX2(
        dt,
        maxTableLog,
        &wksp.sortedSymbol,
        &wksp.rankStart0,
        &mut wksp.rankVal,
        maxW,
        tableLog.wrapping_add(1),
    );

    dtd.tableLog = maxTableLog as u8;
    dtd.tableType = 1;

    DTable.description = dtd;

    Ok(iSize)
}

#[inline(always)]
fn HUF_decodeSymbolX2(
    w: &mut Writer<'_>,
    DStream: &mut BIT_DStream_t,
    dt: &[HUF_DEltX2; 4096],
    dtLog: u32,
) {
    let HUF_DEltX2 {
        sequence,
        nbBits,
        length,
    } = dt[DStream.look_bits_fast(dtLog)];

    DStream.skip_bits(nbBits as u32);
    w.write_symbol_x2(sequence, length);
}

#[inline(always)]
fn HUF_decodeLastSymbolX2(
    w: &mut Writer<'_>,
    DStream: &mut BIT_DStream_t,
    dt: &[HUF_DEltX2; 4096],
    dtLog: u32,
) {
    let HUF_DEltX2 {
        sequence,
        nbBits,
        length,
    } = dt[DStream.look_bits_fast(dtLog)];

    w.write_u8(sequence.to_le_bytes()[0]);

    if length == 1 {
        DStream.skip_bits(u32::from(nbBits));
    } else if (DStream.bitsConsumed as usize) < BitContainerType::BITS as usize {
        DStream.skip_bits(u32::from(nbBits));
        DStream.bitsConsumed = Ord::min(DStream.bitsConsumed, BitContainerType::BITS);
    }
}

macro_rules! HUF_DECODE_SYMBOLX2_0 {
    ($($args:expr),*) => {
        HUF_decodeSymbolX2($($args),*)
    }
}

macro_rules! HUF_DECODE_SYMBOLX2_1 {
    ($($args:expr),*) => {
        if cfg!(target_pointer_width = "64") || HUF_TABLELOG_MAX <= 12 {
            HUF_decodeSymbolX2($($args),*)
        }
    }
}

macro_rules! HUF_DECODE_SYMBOLX2_2 {
    ($($args:expr),*) => {
        if cfg!(target_pointer_width = "64") {
            HUF_decodeSymbolX2($($args),*)
        }
    }
}

#[inline(always)]
fn HUF_decodeStreamX2(
    mut p: Writer<'_>,
    bitDPtr: &mut BIT_DStream_t,
    dt: &[HUF_DEltX2; 4096],
    dtLog: u32,
) -> usize {
    let capacity = p.capacity();

    /* up to 8 symbols at a time */
    if p.capacity() >= size_of::<usize>() {
        if dtLog <= 11 && cfg!(target_pointer_width = "64") {
            /* up to 10 symbols at a time */
            while (bitDPtr.reload() == StreamStatus::Unfinished) && p.capacity() >= 10 {
                HUF_DECODE_SYMBOLX2_0!(&mut p, bitDPtr, dt, dtLog);
                HUF_DECODE_SYMBOLX2_0!(&mut p, bitDPtr, dt, dtLog);
                HUF_DECODE_SYMBOLX2_0!(&mut p, bitDPtr, dt, dtLog);
                HUF_DECODE_SYMBOLX2_0!(&mut p, bitDPtr, dt, dtLog);
                HUF_DECODE_SYMBOLX2_0!(&mut p, bitDPtr, dt, dtLog);
            }
        } else {
            /* up to 8 symbols at a time */
            while bitDPtr.reload() == StreamStatus::Unfinished && p.capacity() >= size_of::<usize>()
            {
                HUF_DECODE_SYMBOLX2_2!(&mut p, bitDPtr, dt, dtLog);
                HUF_DECODE_SYMBOLX2_1!(&mut p, bitDPtr, dt, dtLog);
                HUF_DECODE_SYMBOLX2_2!(&mut p, bitDPtr, dt, dtLog);
                HUF_DECODE_SYMBOLX2_0!(&mut p, bitDPtr, dt, dtLog);
            }
        }
    } else {
        bitDPtr.reload();
    }

    /* closer to end : up to 2 symbols at a time */
    if p.capacity() >= 2 {
        while (bitDPtr.reload() == StreamStatus::Unfinished) && p.capacity() >= 2 {
            HUF_DECODE_SYMBOLX2_0!(&mut p, bitDPtr, dt, dtLog);
        }

        while p.capacity() >= 2 {
            HUF_DECODE_SYMBOLX2_0!(&mut p, bitDPtr, dt, dtLog);
        }
    }

    if !p.is_empty() {
        HUF_decodeLastSymbolX2(&mut p, bitDPtr, dt, dtLog);
    }

    capacity - p.capacity()
}

#[inline(always)]
fn HUF_decompress1X2_usingDTable_internal_body(
    mut dst: Writer<'_>,
    src: &[u8],
    DTable: &DTable,
) -> Result<usize, Error> {
    let mut bitD = BIT_DStream_t::new(src)?;

    let dt = DTable.data.as_x2();
    let dtd = DTable.description;
    HUF_decodeStreamX2(dst.subslice(..), &mut bitD, dt, dtd.tableLog as u32);
    if !bitD.is_empty() {
        return Err(Error::corruption_detected);
    }

    Ok(dst.capacity())
}

#[inline(always)]
fn HUF_decompress4X2_usingDTable_internal_body(
    mut dst: Writer<'_>,
    src: &[u8],
    DTable: &DTable,
) -> Result<usize, Error> {
    // Strict minimum : jump table + 1 byte per stream.
    let [b0, b1, b2, b3, b4, b5, _, _, _, _, ..] = *src else {
        return Err(Error::corruption_detected);
    };

    // Stream 4-way split would not work.
    if dst.capacity() < 6 {
        return Err(Error::corruption_detected);
    }

    let length1 = usize::from(u16::from_le_bytes([b0, b1]));
    let length2 = usize::from(u16::from_le_bytes([b2, b3]));
    let length3 = usize::from(u16::from_le_bytes([b4, b5]));

    if 6 + length1 + length2 + length3 > src.len() {
        return Err(Error::corruption_detected);
    }

    let istart1 = &src[6..][..length1];
    let istart2 = &src[6 + length1..][..length2];
    let istart3 = &src[6 + length1 + length2..][..length3];
    let istart4 = &src[6 + length1 + length2 + length3..];

    let Some((mut op1, mut op2, mut op3, mut op4)) = dst.quarter() else {
        return Err(Error::corruption_detected);
    };

    let mut end_signal = true;

    let dtLog = DTable.description.tableLog as u32;

    if op4.is_empty() {
        return Err(Error::corruption_detected);
    }

    let mut bitD1 = BIT_DStream_t::new(istart1)?;
    let mut bitD2 = BIT_DStream_t::new(istart2)?;
    let mut bitD3 = BIT_DStream_t::new(istart3)?;
    let mut bitD4 = BIT_DStream_t::new(istart4)?;

    let dt = DTable.data.as_x2();

    /* 16-32 symbols per loop (4-8 symbols per stream) */
    if op4.capacity() >= size_of::<usize>() {
        while end_signal && op4.capacity() >= size_of::<usize>() {
            if cfg!(any(target_arch = "x86_64", target_arch = "x86")) {
                HUF_DECODE_SYMBOLX2_2!(&mut op1, &mut bitD1, dt, dtLog);
                HUF_DECODE_SYMBOLX2_1!(&mut op1, &mut bitD1, dt, dtLog);
                HUF_DECODE_SYMBOLX2_2!(&mut op1, &mut bitD1, dt, dtLog);
                HUF_DECODE_SYMBOLX2_0!(&mut op1, &mut bitD1, dt, dtLog);
                HUF_DECODE_SYMBOLX2_2!(&mut op2, &mut bitD2, dt, dtLog);
                HUF_DECODE_SYMBOLX2_1!(&mut op2, &mut bitD2, dt, dtLog);
                HUF_DECODE_SYMBOLX2_2!(&mut op2, &mut bitD2, dt, dtLog);
                HUF_DECODE_SYMBOLX2_0!(&mut op2, &mut bitD2, dt, dtLog);

                end_signal &= bitD1.reload_fast() == StreamStatus::Unfinished;
                end_signal &= bitD2.reload_fast() == StreamStatus::Unfinished;

                HUF_DECODE_SYMBOLX2_2!(&mut op3, &mut bitD3, dt, dtLog);
                HUF_DECODE_SYMBOLX2_1!(&mut op3, &mut bitD3, dt, dtLog);
                HUF_DECODE_SYMBOLX2_2!(&mut op3, &mut bitD3, dt, dtLog);
                HUF_DECODE_SYMBOLX2_0!(&mut op3, &mut bitD3, dt, dtLog);
                HUF_DECODE_SYMBOLX2_2!(&mut op4, &mut bitD4, dt, dtLog);
                HUF_DECODE_SYMBOLX2_1!(&mut op4, &mut bitD4, dt, dtLog);
                HUF_DECODE_SYMBOLX2_2!(&mut op4, &mut bitD4, dt, dtLog);
                HUF_DECODE_SYMBOLX2_0!(&mut op4, &mut bitD4, dt, dtLog);

                end_signal &= bitD3.reload_fast() == StreamStatus::Unfinished;
                end_signal &= bitD4.reload_fast() == StreamStatus::Unfinished;
            } else {
                HUF_DECODE_SYMBOLX2_2!(&mut op1, &mut bitD1, dt, dtLog);
                HUF_DECODE_SYMBOLX2_2!(&mut op2, &mut bitD2, dt, dtLog);
                HUF_DECODE_SYMBOLX2_2!(&mut op3, &mut bitD3, dt, dtLog);
                HUF_DECODE_SYMBOLX2_2!(&mut op4, &mut bitD4, dt, dtLog);
                HUF_DECODE_SYMBOLX2_1!(&mut op1, &mut bitD1, dt, dtLog);
                HUF_DECODE_SYMBOLX2_1!(&mut op2, &mut bitD2, dt, dtLog);
                HUF_DECODE_SYMBOLX2_1!(&mut op3, &mut bitD3, dt, dtLog);
                HUF_DECODE_SYMBOLX2_1!(&mut op4, &mut bitD4, dt, dtLog);
                HUF_DECODE_SYMBOLX2_2!(&mut op1, &mut bitD1, dt, dtLog);
                HUF_DECODE_SYMBOLX2_2!(&mut op2, &mut bitD2, dt, dtLog);
                HUF_DECODE_SYMBOLX2_2!(&mut op3, &mut bitD3, dt, dtLog);
                HUF_DECODE_SYMBOLX2_2!(&mut op4, &mut bitD4, dt, dtLog);
                HUF_DECODE_SYMBOLX2_0!(&mut op1, &mut bitD1, dt, dtLog);
                HUF_DECODE_SYMBOLX2_0!(&mut op2, &mut bitD2, dt, dtLog);
                HUF_DECODE_SYMBOLX2_0!(&mut op3, &mut bitD3, dt, dtLog);
                HUF_DECODE_SYMBOLX2_0!(&mut op4, &mut bitD4, dt, dtLog);

                end_signal &= bitD1.reload_fast() == StreamStatus::Unfinished;
                end_signal &= bitD2.reload_fast() == StreamStatus::Unfinished;
                end_signal &= bitD3.reload_fast() == StreamStatus::Unfinished;
                end_signal &= bitD4.reload_fast() == StreamStatus::Unfinished;
            }
        }
    }

    // Check for corruption.
    // NOTE: these conditions do in fact trigger for invalid input. That is why currently
    // `Writer::write_symbol_x2` does not assert that it is in-bounds.
    if op1.ptr.unwrap().as_ptr() > op1.end {
        return Err(Error::corruption_detected);
    }
    if op2.ptr.unwrap().as_ptr() > op2.end {
        return Err(Error::corruption_detected);
    }
    if op3.ptr.unwrap().as_ptr() > op3.end {
        return Err(Error::corruption_detected);
    }
    // NOTE: op4 is already verified within main loop.

    // Finish bit streams one by one.
    HUF_decodeStreamX2(op1, &mut bitD1, dt, dtLog);
    HUF_decodeStreamX2(op2, &mut bitD2, dt, dtLog);
    HUF_decodeStreamX2(op3, &mut bitD3, dt, dtLog);
    HUF_decodeStreamX2(op4, &mut bitD4, dt, dtLog);

    // Check.
    if !(bitD1.is_empty() && bitD2.is_empty() && bitD3.is_empty() && bitD4.is_empty()) {
        return Err(Error::corruption_detected);
    }

    // The decoded size.
    Ok(dst.capacity())
}

#[cfg_attr(target_arch = "x86_64", target_feature(enable = "bmi2"))]
fn HUF_decompress4X2_usingDTable_internal_bmi2(
    dst: Writer<'_>,
    src: &[u8],
    DTable: &DTable,
) -> Result<usize, Error> {
    HUF_decompress4X2_usingDTable_internal_body(dst, src, DTable)
}

fn HUF_decompress4X2_usingDTable_internal_default(
    dst: Writer<'_>,
    src: &[u8],
    DTable: &DTable,
) -> Result<usize, Error> {
    HUF_decompress4X2_usingDTable_internal_body(dst, src, DTable)
}

unsafe extern "C" fn HUF_decompress4X2_usingDTable_internal_fast_c_loop(
    args: &mut HUF_DecompressFastArgs,
) {
    let dtable: &[HUF_DEltX2; 4096] = core::mem::transmute(args.dt);
    let ilowest = args.ilowest;

    let mut bits = args.bits;
    let mut ip = args.ip;
    let mut op = args.op;

    let mut oend: [*mut u8; 4] = [core::ptr::null_mut::<u8>(); 4];
    oend[0] = op[1];
    oend[1] = op[2];
    oend[2] = op[3];
    oend[3] = args.oend;

    'out: loop {
        /* Assert loop preconditions */
        if cfg!(debug_assertions) {
            for stream in 0..4 {
                debug_assert!(op[stream] <= oend[stream]);
                debug_assert!(ip[stream] >= ilowest);
            }
        }

        /* Compute olimit */

        /* Each loop does 5 table lookups for each of the 4 streams.
         * Each table lookup consumes up to 11 bits of input, and produces
         * up to 2 bytes of output.
         */
        /* We can consume up to 7 bytes of input per iteration per stream.
         * We also know that each input pointer is >= ip[0]. So we can run
         * iters loops before running out of input.
         */
        let mut iters = ip[0].offset_from_unsigned(ilowest) / 7;

        /* Each iteration can produce up to 10 bytes of output per stream.
         * Each output stream my advance at different rates. So take the
         * minimum number of safe iterations among all the output streams.
         */
        for stream in 0..4 {
            let oiters = oend[stream].offset_from_unsigned(op[stream]) / 10;
            iters = Ord::min(iters, oiters);
        }

        /* Each iteration produces at least 5 output symbols. So until
         * op[3] crosses olimit, we know we haven't executed iters
         * iterations yet. This saves us maintaining an iters counter,
         * at the expense of computing the remaining # of iterations
         * more frequently.
         */
        let olimit = op[3].add(iters * 5);

        /* Exit the fast decoding loop once we reach the end. */
        if op[3] == olimit {
            break;
        }

        /* Exit the decoding loop if any input pointer has crossed the
         * previous one. This indicates corruption, and a precondition
         * to our loop is that ip[i] >= ip[0].
         */
        for stream in 1..4 {
            if ip[stream] < ip[stream - 1] {
                break 'out;
            }
        }

        for stream in 1..4 {
            debug_assert!(ip[stream] >= ip[stream - 1]);
        }

        macro_rules! HUF_4X2_DECODE_SYMBOL {
            ($stream:expr, $decode3:expr) => {
                if (($decode3) != 0 || ($stream) != 3) {
                    let index = (bits[($stream)] >> 53);
                    let entry = dtable[index as usize];
                    MEM_write16(op[($stream)].cast(), entry.sequence);
                    bits[($stream)] <<= (entry.nbBits) & 0x3F;
                    op[($stream)] = op[($stream)].add(usize::from(entry.length));
                }
            };
        }

        macro_rules! HUF_4X2_RELOAD_STREAM {
            ($stream:expr) => {
                HUF_4X2_DECODE_SYMBOL!(3, 1);
                {
                    let ctz = bits[($stream)].trailing_zeros();
                    let nbBits = ctz & 7;
                    let nbBytes = ctz >> 3;
                    ip[($stream)] = ip[$stream].sub(nbBytes as usize);
                    bits[($stream)] = MEM_read64(ip[($stream)].cast()) | 1;
                    bits[($stream)] <<= nbBits;
                }
            };
        }

        /* Manually unroll the loop because compilers don't consistently
         * unroll the inner loops, which destroys performance.
         */

        loop {
            /* Decode 5 symbols from each of the first 3 streams.
             * The final stream will be decoded during the reload phase
             * to reduce register pressure.
             */
            HUF_4X_FOR_EACH_STREAM_WITH_VAR!(HUF_4X2_DECODE_SYMBOL, 0);
            HUF_4X_FOR_EACH_STREAM_WITH_VAR!(HUF_4X2_DECODE_SYMBOL, 0);
            HUF_4X_FOR_EACH_STREAM_WITH_VAR!(HUF_4X2_DECODE_SYMBOL, 0);
            HUF_4X_FOR_EACH_STREAM_WITH_VAR!(HUF_4X2_DECODE_SYMBOL, 0);
            HUF_4X_FOR_EACH_STREAM_WITH_VAR!(HUF_4X2_DECODE_SYMBOL, 0);

            /* Decode one symbol from the final stream */
            HUF_4X2_DECODE_SYMBOL!(3, 1);

            /* Decode 4 symbols from the final stream & reload bitstreams.
             * The final stream is reloaded last, meaning that all 5 symbols
             * are decoded from the final stream before it is reloaded.
             */
            HUF_4X_FOR_EACH_STREAM!(HUF_4X2_RELOAD_STREAM);

            if op[3] >= olimit {
                break;
            }
        }
    }

    // Save the final values of each of the state variables back to args.
    args.bits = bits;
    args.ip = ip;
    args.op = op;
}

unsafe fn HUF_decompress4X2_usingDTable_internal_fast(
    mut dst: Writer<'_>,
    src: &[u8],
    DTable: &DTable,
    loopFn: HUF_DecompressFastLoopFn,
) -> Result<usize, Error> {
    let oend = dst.as_mut_ptr_range().end;

    let Some(mut args) = HUF_DecompressFastArgs::new(dst.subslice(..), src, DTable)? else {
        return Ok(0);
    };

    debug_assert!(args.ip[0] >= args.ilowest);
    loopFn(&mut args);

    // note : op4 already verified within main loop.
    let ilowest = src.as_ptr();
    debug_assert!(args.ip[0] >= ilowest);
    debug_assert!(args.ip[1] >= ilowest);
    debug_assert!(args.ip[2] >= ilowest);
    debug_assert!(args.ip[3] >= ilowest);
    debug_assert!(args.op[3] <= oend);

    debug_assert_eq!(ilowest, args.ilowest);
    debug_assert_eq!(ilowest.add(6), args.iend[0]);

    let segmentSize = dst.capacity().div_ceil(4);
    let mut segmentEnd = dst.as_mut_ptr();

    for (i, op) in args.op.iter().copied().enumerate() {
        if segmentSize <= oend.offset_from_unsigned(segmentEnd) {
            segmentEnd = segmentEnd.add(segmentSize);
        } else {
            segmentEnd = oend;
        }

        let mut bit = init_remaining_dstream(&args, i, segmentEnd)?;

        let length = HUF_decodeStreamX2(
            Writer::from_raw_parts(op, segmentEnd as usize - op as usize),
            &mut bit,
            DTable.data.as_x2(),
            HUF_DECODER_FAST_TABLELOG as u32,
        );

        if op.add(length) != segmentEnd {
            return Err(Error::corruption_detected);
        }
    }

    Ok(dst.capacity())
}

fn HUF_decompress4X2_usingDTable_internal(
    mut dst: Writer<'_>,
    src: &[u8],
    DTable: &DTable,
    flags: core::ffi::c_int,
) -> Result<usize, Error> {
    if flags & HUF_flags_bmi2 as core::ffi::c_int != 0 {
        let loopFn = match flags & HUF_flags_disableAsm as core::ffi::c_int {
            #[cfg(any())] // blaze: upstream's x86_64 assembly loop (huf_decompress_amd64.S) is not in this copy
            0 => HUF_decompress4X2_usingDTable_internal_fast_asm_loop as HUF_DecompressFastLoopFn,
            _ => HUF_decompress4X2_usingDTable_internal_fast_c_loop as HUF_DecompressFastLoopFn,
        };

        if HUF_ENABLE_FAST_DECODE != 0 && flags & HUF_flags_disableFast as core::ffi::c_int == 0 {
            let ret = unsafe {
                HUF_decompress4X2_usingDTable_internal_fast(dst.subslice(..), src, DTable, loopFn)
            }?;
            if ret != 0 {
                return Ok(ret);
            }
        }

        // SAFETY: the bmi2 feature is enabled.
        unsafe { HUF_decompress4X2_usingDTable_internal_bmi2(dst, src, DTable) }
    } else {
        HUF_decompress4X2_usingDTable_internal_default(dst, src, DTable)
    }
}

#[cfg_attr(target_arch = "x86_64", target_feature(enable = "bmi2"))]
fn HUF_decompress1X2_usingDTable_internal_bmi2(
    dst: Writer<'_>,
    src: &[u8],
    DTable: &DTable,
) -> Result<usize, Error> {
    HUF_decompress1X2_usingDTable_internal_body(dst, src, DTable)
}
fn HUF_decompress1X2_usingDTable_internal_default(
    dst: Writer<'_>,
    src: &[u8],
    DTable: &DTable,
) -> Result<usize, Error> {
    HUF_decompress1X2_usingDTable_internal_body(dst, src, DTable)
}

fn HUF_decompress1X2_usingDTable_internal(
    dst: Writer<'_>,
    src: &[u8],
    DTable: &DTable,
    flags: core::ffi::c_int,
) -> Result<usize, Error> {
    if flags & HUF_flags_bmi2 as core::ffi::c_int != 0 {
        // SAFETY: the bmi2 feature is enabled.
        unsafe { HUF_decompress1X2_usingDTable_internal_bmi2(dst, src, DTable) }
    } else {
        HUF_decompress1X2_usingDTable_internal_default(dst, src, DTable)
    }
}

pub fn HUF_decompress1X2_DCtx_wksp(
    dctx: &mut DTable,
    dst: Writer<'_>,
    src: &[u8],
    workSpace: &mut Workspace,
    flags: core::ffi::c_int,
) -> Result<usize, Error> {
    let hSize = HUF_readDTableX2_wksp(dctx, src, workSpace.as_x2_mut(), flags)?;
    if hSize >= src.len() {
        return Err(Error::srcSize_wrong);
    }

    HUF_decompress1X2_usingDTable_internal(dst, &src[hSize..], dctx, flags)
}

fn HUF_decompress4X2_DCtx_wksp(
    dctx: &mut DTable,
    dst: Writer<'_>,
    src: &[u8],
    workSpace: &mut Workspace,
    flags: core::ffi::c_int,
) -> Result<usize, Error> {
    let hSize = HUF_readDTableX2_wksp(dctx, src, workSpace.as_x2_mut(), flags)?;
    if hSize >= src.len() {
        return Err(Error::srcSize_wrong);
    }

    HUF_decompress4X2_usingDTable_internal(dst, &src[hSize..], dctx, flags)
}

static algoTime: [[algo_time_t; 2]; 16] = [
    [
        {
            algo_time_t {
                tableTime: 0,
                decode256Time: 0,
            }
        },
        {
            algo_time_t {
                tableTime: 1,
                decode256Time: 1,
            }
        },
    ],
    [
        {
            algo_time_t {
                tableTime: 0,
                decode256Time: 0,
            }
        },
        {
            algo_time_t {
                tableTime: 1,
                decode256Time: 1,
            }
        },
    ],
    [
        {
            algo_time_t {
                tableTime: 150,
                decode256Time: 216,
            }
        },
        {
            algo_time_t {
                tableTime: 381,
                decode256Time: 119,
            }
        },
    ],
    [
        {
            algo_time_t {
                tableTime: 170,
                decode256Time: 205,
            }
        },
        {
            algo_time_t {
                tableTime: 514,
                decode256Time: 112,
            }
        },
    ],
    [
        {
            algo_time_t {
                tableTime: 177,
                decode256Time: 199,
            }
        },
        {
            algo_time_t {
                tableTime: 539,
                decode256Time: 110,
            }
        },
    ],
    [
        {
            algo_time_t {
                tableTime: 197,
                decode256Time: 194,
            }
        },
        {
            algo_time_t {
                tableTime: 644,
                decode256Time: 107,
            }
        },
    ],
    [
        {
            algo_time_t {
                tableTime: 221,
                decode256Time: 192,
            }
        },
        {
            algo_time_t {
                tableTime: 735,
                decode256Time: 107,
            }
        },
    ],
    [
        {
            algo_time_t {
                tableTime: 256,
                decode256Time: 189,
            }
        },
        {
            algo_time_t {
                tableTime: 881,
                decode256Time: 106,
            }
        },
    ],
    [
        {
            algo_time_t {
                tableTime: 359,
                decode256Time: 188,
            }
        },
        {
            algo_time_t {
                tableTime: 1167,
                decode256Time: 109,
            }
        },
    ],
    [
        {
            algo_time_t {
                tableTime: 582,
                decode256Time: 187,
            }
        },
        {
            algo_time_t {
                tableTime: 1570,
                decode256Time: 114,
            }
        },
    ],
    [
        {
            algo_time_t {
                tableTime: 688,
                decode256Time: 187,
            }
        },
        {
            algo_time_t {
                tableTime: 1712,
                decode256Time: 122,
            }
        },
    ],
    [
        {
            algo_time_t {
                tableTime: 825,
                decode256Time: 186,
            }
        },
        {
            algo_time_t {
                tableTime: 1965,
                decode256Time: 136,
            }
        },
    ],
    [
        {
            algo_time_t {
                tableTime: 976,
                decode256Time: 185,
            }
        },
        {
            algo_time_t {
                tableTime: 2131,
                decode256Time: 150,
            }
        },
    ],
    [
        {
            algo_time_t {
                tableTime: 1180,
                decode256Time: 186,
            }
        },
        {
            algo_time_t {
                tableTime: 2070,
                decode256Time: 175,
            }
        },
    ],
    [
        {
            algo_time_t {
                tableTime: 1377,
                decode256Time: 185,
            }
        },
        {
            algo_time_t {
                tableTime: 1731,
                decode256Time: 202,
            }
        },
    ],
    [
        {
            algo_time_t {
                tableTime: 1412,
                decode256Time: 185,
            }
        },
        {
            algo_time_t {
                tableTime: 1695,
                decode256Time: 202,
            }
        },
    ],
];

enum Decoder {
    A1,
    A2,
}

///  Tells which decoder is likely to decode faster,
///  based on a set of pre-computed metrics.
///
/// @return : `0==HUF_decompress4X1`, `1==HUF_decompress4X2` .
///  Assumption : 0 < `dst_size` <= 128 KB */
fn HUF_selectDecoder(dst_size: usize, src_size: usize) -> Decoder {
    let D256 = (dst_size >> 8) as u32;

    let Q = if src_size >= dst_size {
        15
    } else {
        src_size * 16 / dst_size
    };

    let [time0, time1] = algoTime[Q];
    let DTime0 = time0.tableTime + time0.decode256Time * D256;
    let DTime1 = time1.tableTime + time1.decode256Time * D256;

    if (DTime1 + (DTime1 >> 5)) < DTime0 {
        Decoder::A2
    } else {
        Decoder::A1
    }
}

pub fn HUF_decompress1X_usingDTable(
    dst: Writer<'_>,
    src: &[u8],
    DTable: &DTable,
    flags: core::ffi::c_int,
) -> Result<usize, Error> {
    match DTable.description.tableType {
        0 => HUF_decompress1X1_usingDTable_internal(dst, src, DTable, flags),
        _ => HUF_decompress1X2_usingDTable_internal(dst, src, DTable, flags),
    }
}

pub fn HUF_decompress1X1_DCtx_wksp(
    dctx: &mut DTable,
    dst: Writer<'_>,
    src: &[u8],
    workSpace: &mut Workspace,
    flags: core::ffi::c_int,
) -> Result<usize, Error> {
    let hSize = HUF_readDTableX1_wksp(dctx, src, workSpace, flags)?;
    if hSize >= src.len() {
        return Err(Error::srcSize_wrong);
    }

    HUF_decompress1X1_usingDTable_internal(dst, &src[hSize..], dctx, flags)
}

pub fn HUF_decompress4X_usingDTable(
    dst: Writer<'_>,
    src: &[u8],
    DTable: &DTable,
    flags: core::ffi::c_int,
) -> Result<usize, Error> {
    match DTable.description.tableType {
        0 => HUF_decompress4X1_usingDTable_internal(dst, src, DTable, flags),
        _ => HUF_decompress4X2_usingDTable_internal(dst, src, DTable, flags),
    }
}

pub fn HUF_decompress4X_hufOnly_wksp(
    dctx: &mut DTable,
    dst: Writer<'_>,
    src: &[u8],
    workSpace: &mut Workspace,
    flags: core::ffi::c_int,
) -> Result<usize, Error> {
    if dst.is_empty() {
        return Err(Error::dstSize_tooSmall);
    }
    if src.is_empty() {
        return Err(Error::corruption_detected);
    }

    match HUF_selectDecoder(dst.capacity(), src.len()) {
        Decoder::A1 => HUF_decompress4X1_DCtx_wksp(dctx, dst, src, workSpace, flags),
        Decoder::A2 => HUF_decompress4X2_DCtx_wksp(dctx, dst, src, workSpace, flags),
    }
}

#[derive(Debug)]
pub struct Writer<'a> {
    ptr: Option<NonNull<u8>>,
    end: *mut u8,
    _marker: core::marker::PhantomData<&'a mut [u8]>,
}

impl<'a> Writer<'a> {
    pub(crate) fn from_slice(slice: &'a mut [u8]) -> Self {
        let len = slice.len();
        let ptr = NonNull::new(slice.as_mut_ptr()).unwrap();
        Self {
            ptr: Some(ptr),
            end: unsafe { ptr.as_ptr().add(len) },
            _marker: core::marker::PhantomData,
        }
    }

    /// # Safety
    ///
    /// - `ptr` must point to `len` readable and writable bytes
    /// - `ptr` may be NULL only if `len == 0`
    pub unsafe fn from_raw_parts(ptr: *mut u8, len: usize) -> Self {
        let ptr = NonNull::new(ptr);

        if ptr.is_none() {
            assert_eq!(len, 0);
        }

        Self {
            ptr,
            end: match ptr {
                None => core::ptr::null_mut(),
                Some(ptr) => unsafe { ptr.as_ptr().add(len) },
            },
            _marker: core::marker::PhantomData,
        }
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        match self.ptr {
            None => 0,
            Some(ptr) => unsafe { self.end.offset_from_unsigned(ptr.as_ptr()) },
        }
    }

    #[inline]
    pub fn is_null(&self) -> bool {
        self.ptr.is_none()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        match self.ptr {
            None => true,
            Some(ptr) => ptr.as_ptr() == self.end,
        }
    }

    #[inline]
    pub fn as_ptr(&self) -> *const u8 {
        match self.ptr {
            None => core::ptr::null(),
            Some(ptr) => ptr.as_ptr(),
        }
    }

    #[inline]
    pub fn as_ptr_range(&self) -> Range<*const u8> {
        self.as_ptr()..self.as_ptr().wrapping_add(self.capacity())
    }

    #[inline]
    pub unsafe fn as_slice(&self) -> &[u8] {
        match self.ptr {
            None => &[],
            Some(ptr) => unsafe { core::slice::from_raw_parts(ptr.as_ptr(), self.capacity()) },
        }
    }

    #[inline]
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        match self.ptr {
            None => core::ptr::null_mut(),
            Some(ptr) => ptr.as_ptr(),
        }
    }

    #[inline]
    pub fn as_mut_ptr_range(&mut self) -> core::ops::Range<*mut u8> {
        match self.ptr {
            None => core::ptr::null_mut()..core::ptr::null_mut(),
            Some(ptr) => ptr.as_ptr()..self.end,
        }
    }

    pub fn subslice<R: core::ops::RangeBounds<usize>>(&mut self, range: R) -> Self {
        let Some(ptr) = self.ptr else {
            match (range.start_bound(), range.end_bound()) {
                (Bound::Unbounded, Bound::Unbounded)
                | (
                    Bound::Included(&0),
                    Bound::Included(&0) | Bound::Excluded(&1) | Bound::Unbounded,
                ) => {
                    return Self {
                        ptr: self.ptr,
                        end: self.end,
                        _marker: self._marker,
                    };
                }
                _ => panic!("out of bounds"),
            }
        };

        let new_ptr = match range.start_bound() {
            Bound::Included(&count) => ptr.as_ptr().wrapping_add(count),
            Bound::Excluded(_) => unreachable!("I think?"),
            Bound::Unbounded => ptr.as_ptr(),
        };

        if new_ptr > self.end {
            panic!("out of bounds");
        }

        let new_end = match range.end_bound() {
            Bound::Included(&count) => ptr.as_ptr().wrapping_add(count + 1),
            Bound::Excluded(&count) => ptr.as_ptr().wrapping_add(count),
            Bound::Unbounded => self.end,
        };

        if new_end > self.end {
            panic!("out of bounds");
        }

        Self {
            ptr: NonNull::new(new_ptr),
            end: new_end,
            _marker: core::marker::PhantomData,
        }
    }

    pub(crate) fn quarter(&mut self) -> Option<(Self, Self, Self, Self)> {
        let capacity = self.capacity();
        let segment_size = capacity.div_ceil(4);
        let range = self.as_mut_ptr_range();
        let remainder = capacity - 3 * segment_size;

        if (range.end as usize - range.start as usize) < 6 {
            panic!("length must be at least six when splitting into 4 streams");
        }

        unsafe {
            let w1 = Self::from_raw_parts(range.start, segment_size);
            let w2 = Self::from_raw_parts(range.start.add(segment_size), segment_size);
            let w3 = Self::from_raw_parts(range.start.add(2 * segment_size), segment_size);
            let w4 = Self::from_raw_parts(range.start.add(3 * segment_size), remainder);

            // If the capacity is 6, `6.div_ceil(4)` is 2, but 4 * 2 > 6.
            if !range.contains(&w4.ptr.unwrap().as_ptr()) {
                return None;
            }

            debug_assert!(w1.end <= range.end);
            debug_assert!(w2.end <= range.end);
            debug_assert!(w3.end <= range.end);
            debug_assert!(w4.end <= range.end);

            Some((w1, w2, w3, w4))
        }
    }

    pub(crate) fn write_u8(&mut self, byte: u8) {
        let Some(ptr) = self.ptr else {
            panic!("write out of bounds");
        };

        if ptr.as_ptr() >= self.end {
            panic!("write out of bounds");
        }

        // SAFETY: `ptr < end` and we're allowed to write to this memory.
        unsafe { ptr.as_ptr().write(byte) }

        // SAFETY: `ptr..end` is a contiguous allocation.
        self.ptr = unsafe { NonNull::new(ptr.as_ptr().add(1)) }
    }

    pub(crate) fn write_symbol_x2(&mut self, value: u16, length: u8) {
        debug_assert!(length <= 2);

        let Some(ptr) = self.ptr else {
            panic!("write out of bounds");
        };

        // we can't actually assert this, an earlier reader may write into the next.
        // that then returns an error. We should return a result here later.
        // assert!( self.ptr.wrapping_add(length as usize) <= self.end, "write out of bounds {:?} {length}", self.as_mut_ptr_range());

        // SAFETY: `ptr < end` and we're allowed to write to this memory.
        unsafe { ptr.as_ptr().cast::<u16>().write_unaligned(value.to_le()) }

        // SAFETY: `ptr..end` is a contiguous allocation.
        self.ptr = unsafe { NonNull::new(ptr.as_ptr().add(length as usize)) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_DEltX2_to_u32() {
        assert_eq!(
            HUF_buildDEltX2U32(0xAABB, 0xCC, 0xDD, 0xEE).to_le_bytes(),
            [0xDD, 0xBB, 0x76, 0xEF]
        );
        assert_eq!(HUF_buildDEltX2U32(1, 2, 3, 4).to_le_bytes(), [3, 1, 2, 4]);
    }

    #[test]
    fn test_buildDEltX2() {
        assert_eq!(
            HUF_buildDEltX2(0xAA, 0xBB, 0xCC, 0xDD),
            HUF_DEltX2 {
                sequence: 0xAACC,
                nbBits: 0xBB,
                length: 0xDD,
            }
        );

        assert_eq!(
            HUF_buildDEltX2(1, 2, 3, 4),
            HUF_DEltX2 {
                sequence: 0x0103,
                nbBits: 2,
                length: 4,
            }
        );
    }

    #[test]
    fn writer_subslice() {
        let mut arr = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let mut w = unsafe { Writer::from_raw_parts(arr.as_mut_ptr(), arr.len()) };

        assert_eq!(w.subslice(..).capacity(), 8);
        assert_eq!(w.subslice(..4).capacity(), 4);
        assert_eq!(w.subslice(4..).capacity(), 4);

        assert_eq!(w.subslice(..=4).capacity(), 5);
    }
}
