use crate::lib::common::fse::{
    FSE_DTableHeader, FSE_decode_t, FSE_BUILD_DTABLE_WKSP_SIZE, FSE_DECOMPRESS_WKSP_SIZE,
    FSE_MAX_SYMBOL_VALUE, FSE_MAX_TABLELOG, FSE_TABLESTEP,
};
use crate::lib::common::{
    bitstream::{BIT_DStream_t, StreamStatus},
    entropy_common::{DTable, FSE_readNCount_bmi2, Workspace},
    error_private::Error,
};

#[derive(Copy, Clone, Debug, PartialEq)]
#[repr(C)]
pub(crate) struct FSE_DecompressWksp {
    pub ncount: [core::ffi::c_short; 256],
}

impl Default for FSE_DecompressWksp {
    fn default() -> Self {
        Self { ncount: [0; 256] }
    }
}

#[derive(Copy, Clone)]
#[repr(C)]
struct FSE_DState_t<'a> {
    state: usize,
    table: &'a [FSE_decode_t; 90],
}

impl<'a> FSE_DState_t<'a> {
    fn new(bitD: &mut BIT_DStream_t, dt: &'a DTable) -> Self {
        let state = bitD.read_bits(dt.header.tableLog as core::ffi::c_uint);
        let _ = bitD.reload();
        let table = &dt.elements;

        Self { state, table }
    }

    #[inline]
    fn decode_symbol(&mut self, bitD: &mut BIT_DStream_t) -> u8 {
        let FSE_decode_t {
            nbBits,
            symbol,
            newState,
        } = self.table[self.state];

        let lowBits = bitD.read_bits(u32::from(nbBits));
        self.state = usize::from(newState) + lowBits;

        symbol
    }

    #[inline]
    fn decode_symbol_fast(&mut self, bitD: &mut BIT_DStream_t) -> u8 {
        let FSE_decode_t {
            nbBits,
            symbol,
            newState,
        } = self.table[self.state];

        let lowBits = bitD.read_bits_fast(u32::from(nbBits));
        self.state = usize::from(newState) + lowBits;

        symbol
    }
}

fn FSE_buildDTable_internal(
    dt: &mut DTable,
    normalizedCounter: &[core::ffi::c_short; 256],
    maxSymbolValue: u8,
    tableLog: core::ffi::c_uint,
) -> Result<(), Error> {
    let wkspSize = dt.elements[(1 << tableLog)..].len() * 4;
    let (header, elements, symbols, spread) = dt.destructure_mut(maxSymbolValue, tableLog);
    let maxSV1 = usize::from(maxSymbolValue) + 1;
    let tableSize = 1usize << tableLog;
    let mut highThreshold = tableSize.wrapping_sub(1);

    if FSE_BUILD_DTABLE_WKSP_SIZE(tableLog as usize, maxSymbolValue as usize) > wkspSize {
        return Err(Error::maxSymbolValue_tooLarge);
    }

    if tableLog > FSE_MAX_TABLELOG as core::ffi::c_uint {
        return Err(Error::tableLog_tooLarge);
    }

    let mut DTableH = FSE_DTableHeader {
        tableLog: tableLog as u16,
        fastMode: 1,
    };

    // Init: lay down lowprob symbols
    let largeLimit = (1 << tableLog.wrapping_sub(1)) as i16;
    for s in 0..maxSV1 {
        if normalizedCounter[s] == -1 {
            elements[highThreshold].symbol = s as u8;
            highThreshold = highThreshold.wrapping_sub(1);
            symbols[s] = 1;
        } else {
            if normalizedCounter[s] >= largeLimit {
                DTableH.fastMode = 0;
            }
            symbols[s] = normalizedCounter[s] as u16;
        }
    }

    *header = DTableH;

    // Spread symbols
    if highThreshold == tableSize.wrapping_sub(1) {
        let tableMask = tableSize.wrapping_sub(1);
        let step = FSE_TABLESTEP(tableSize);

        // First lay down the symbols in order.
        // We use a u64 to lay down 8 bytes at a time. This reduces branch
        // misses since small blocks generally have small table logs, so nearly
        // all symbols have counts <= 8. We ensure we have 8 bytes at the end of
        // our buffer to handle the over-write.
        let add = 0x101010101010101u64;
        let mut pos = 0usize;
        let mut sv = 0u64;
        for &v in &normalizedCounter[..maxSV1] {
            let n = v as usize;
            let data = &mut spread[pos..][..n.max(1).next_multiple_of(8)];
            for chunk in data.as_chunks_mut::<8>().0 {
                *chunk = sv.to_le_bytes();
            }
            pos = pos.wrapping_add(n);
            sv = sv.wrapping_add(add);
        }

        // Now we spread those positions across the table.
        // The benefit of doing it in two stages is that we avoid the
        // variable size inner loop, which caused lots of branch misses.
        // Now we can run through all the positions without any branch misses.
        // We unroll the loop twice, since that is what empirically worked best.
        let mut position = 0usize;
        let unroll = 2;
        for s in (0..tableSize).step_by(unroll) {
            for u in 0..unroll {
                let uPosition = position.wrapping_add(u * step) & tableMask;
                elements[uPosition].symbol = spread[s + u];
            }
            position = position.wrapping_add(unroll * step) & tableMask;
        }
    } else {
        let tableMask = tableSize.wrapping_sub(1);
        let step = (tableSize >> 1)
            .wrapping_add(tableSize >> 3)
            .wrapping_add(3);

        let mut position = 0usize;
        for (s, &v) in normalizedCounter[..maxSV1].iter().enumerate() {
            for _ in 0..v {
                elements[position].symbol = s as u8;
                position = position.wrapping_add(step) & tableMask;
                while position > highThreshold {
                    position = position.wrapping_add(step) & tableMask;
                }
            }
        }

        if position != 0 {
            return Err(Error::GENERIC); // position must reach all cells once, otherwise normalizedCounter is incorrect
        }
    }

    // Build decoding table
    for elt in &mut elements[..tableSize] {
        let symbol = usize::from(elt.symbol);
        let nextState = u32::from(symbols[symbol]);
        symbols[symbol] += 1;
        elt.nbBits = tableLog.wrapping_sub(nextState.ilog2()) as u8;
        elt.newState = (nextState << elt.nbBits).wrapping_sub(tableSize as u32) as u16;
    }

    Ok(())
}

#[inline(always)]
fn FSE_getSymbol<const FAST: bool>(state: &mut FSE_DState_t<'_>, bitD: &mut BIT_DStream_t) -> u8 {
    if FAST {
        state.decode_symbol_fast(bitD)
    } else {
        state.decode_symbol(bitD)
    }
}

#[inline(always)]
fn FSE_decompress_usingDTable_generic<const FAST: bool>(
    dst: &mut [u8],
    cSrc: &[u8],
    dt: &DTable,
) -> Result<usize, Error> {
    let mut op = 0;
    let omax = dst.len();
    let olimit = omax - 3;

    let mut bitD = BIT_DStream_t::new(cSrc)?;

    let mut state1 = FSE_DState_t::new(&mut bitD, dt);
    let mut state2 = FSE_DState_t::new(&mut bitD, dt);

    if let StreamStatus::Overflow = bitD.reload() {
        return Err(Error::corruption_detected);
    }

    while bitD.reload() == StreamStatus::Unfinished && op < olimit {
        dst[op] = FSE_getSymbol::<FAST>(&mut state1, &mut bitD);

        if (FSE_MAX_TABLELOG * 2 + 7) as u32 > usize::BITS {
            let _ = bitD.reload();
        }

        dst[op + 1] = FSE_getSymbol::<FAST>(&mut state2, &mut bitD);

        if (FSE_MAX_TABLELOG * 4 + 7) as u32 > usize::BITS
            && bitD.reload() != StreamStatus::Unfinished
        {
            op += 2;
            break;
        }

        dst[op + 2] = FSE_getSymbol::<FAST>(&mut state1, &mut bitD);

        if (FSE_MAX_TABLELOG * 2 + 7) as u32 > usize::BITS {
            let _ = bitD.reload();
        }

        dst[op + 3] = FSE_getSymbol::<FAST>(&mut state2, &mut bitD);

        op += 4;
    }

    loop {
        if op > omax - 2 {
            return Err(Error::dstSize_tooSmall);
        }

        dst[op] = FSE_getSymbol::<FAST>(&mut state1, &mut bitD);
        op += 1;

        if let StreamStatus::Overflow = bitD.reload() {
            dst[op] = FSE_getSymbol::<FAST>(&mut state2, &mut bitD);
            op += 1;
            break;
        } else {
            if op > omax - 2 {
                return Err(Error::dstSize_tooSmall);
            }

            dst[op] = FSE_getSymbol::<FAST>(&mut state2, &mut bitD);
            op += 1;

            match bitD.reload() {
                StreamStatus::Overflow => { /* fall through */ }
                _ => continue,
            }

            dst[op] = FSE_getSymbol::<FAST>(&mut state1, &mut bitD);
            op += 1;

            break;
        }
    }

    Ok(op)
}

#[inline(always)]
fn FSE_decompress_wksp_body(
    dst: &mut [u8],
    cSrc: &[u8],
    maxLog: core::ffi::c_uint,
    workspace: &mut Workspace,
    bmi2: core::ffi::c_int,
) -> Result<usize, Error> {
    let mut tableLog: core::ffi::c_uint = 0;
    let mut maxSymbolValue = FSE_MAX_SYMBOL_VALUE;
    const { assert!(size_of::<Workspace>() >= size_of::<FSE_DecompressWksp>()) };
    let NCountLength = FSE_readNCount_bmi2(
        &mut workspace.a.ncount,
        &mut maxSymbolValue,
        &mut tableLog,
        cSrc,
        bmi2,
    )?;

    if tableLog > maxLog {
        return Err(Error::tableLog_tooLarge);
    }
    let ip = &cSrc[NCountLength as usize..];

    if FSE_DECOMPRESS_WKSP_SIZE(tableLog as usize, maxSymbolValue as usize) > size_of::<Workspace>()
    {
        return Err(Error::tableLog_tooLarge);
    }

    let () = FSE_buildDTable_internal(
        &mut workspace.dtable,
        &workspace.a.ncount,
        maxSymbolValue,
        tableLog,
    )?;

    if workspace.dtable.header.fastMode != 0 {
        FSE_decompress_usingDTable_generic::<true>(dst, ip, &workspace.dtable)
    } else {
        FSE_decompress_usingDTable_generic::<false>(dst, ip, &workspace.dtable)
    }
}

fn FSE_decompress_wksp_body_default(
    dst: &mut [u8],
    cSrc: &[u8],
    maxLog: core::ffi::c_uint,
    workSpace: &mut Workspace,
) -> Result<usize, Error> {
    FSE_decompress_wksp_body(dst, cSrc, maxLog, workSpace, 0)
}

#[cfg_attr(target_arch = "x86_64", target_feature(enable = "bmi2"))]
fn FSE_decompress_wksp_body_bmi2(
    dst: &mut [u8],
    cSrc: &[u8],
    maxLog: core::ffi::c_uint,
    workSpace: &mut Workspace,
) -> Result<usize, Error> {
    FSE_decompress_wksp_body(dst, cSrc, maxLog, workSpace, 1)
}

pub(super) fn FSE_decompress_wksp_bmi2(
    dst: &mut [u8],
    cSrc: &[u8],
    maxLog: core::ffi::c_uint,
    workSpace: &mut Workspace,
    bmi2: bool,
) -> Result<usize, Error> {
    if bmi2 {
        // SAFETY: the bmi2 feature is enabled.
        unsafe { FSE_decompress_wksp_body_bmi2(dst, cSrc, maxLog, workSpace) }
    } else {
        FSE_decompress_wksp_body_default(dst, cSrc, maxLog, workSpace)
    }
}
