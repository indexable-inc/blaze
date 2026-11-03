use crate::lib::common::error_private::Error;
use crate::lib::common::fse::{
    FSE_DTableHeader, FSE_decode_t, FSE_MIN_TABLELOG, FSE_TABLELOG_ABSOLUTE_MAX,
};
use crate::lib::common::fse_decompress::{FSE_DecompressWksp, FSE_decompress_wksp_bmi2};
use crate::lib::common::huf::{
    HUF_flags_bmi2, HUF_READ_STATS_WORKSPACE_SIZE_U32, HUF_TABLELOG_MAX,
};
use crate::lib::polyfill::likely;

#[inline(always)]
fn FSE_readNCount_body(
    normalizedCounter: &mut [i16],
    maxSVPtr: &mut u8,
    tableLogPtr: &mut core::ffi::c_uint,
    headerBuffer: &[u8],
) -> Result<usize, Error> {
    let hbSize = headerBuffer.len();

    let iend = hbSize;
    let mut ip = 0usize;
    let mut charnum = 0 as core::ffi::c_uint;
    let maxSV1 = u32::from(*maxSVPtr) + 1;
    let mut previous_was_0 = false;
    if hbSize < 8 {
        let mut buffer = [0u8; 8];
        buffer[..hbSize].copy_from_slice(headerBuffer);

        let countSize = FSE_readNCount(normalizedCounter, maxSVPtr, tableLogPtr, &buffer)?;

        if countSize > hbSize {
            return Err(Error::corruption_detected);
        }
        return Ok(countSize);
    }

    normalizedCounter.fill(0);

    let read_u32_le = |offset| u32::from_le_bytes(headerBuffer[offset..][..4].try_into().unwrap());

    let mut bitStream = read_u32_le(ip);
    let mut nbBits = (bitStream & 0xf as core::ffi::c_int as u32)
        .wrapping_add(FSE_MIN_TABLELOG as u32) as core::ffi::c_int;
    if nbBits > FSE_TABLELOG_ABSOLUTE_MAX {
        return Err(Error::tableLog_tooLarge);
    }
    bitStream >>= 4;
    let mut bitCount: core::ffi::c_int = 4;
    *tableLogPtr = nbBits as core::ffi::c_uint;
    let mut remaining = (1 << nbBits) + 1;
    let mut threshold = 1 << nbBits;
    nbBits += 1;
    loop {
        if previous_was_0 {
            // Count the number of repeats. Each time the 2-bit repeat code is 0b11 there is another repeat.
            let mut repeats = bitStream.trailing_ones() >> 1;
            while repeats >= 12 {
                charnum = charnum.wrapping_add(3 * 12);
                if likely(ip <= iend - 7) {
                    ip += 3;
                } else {
                    bitCount =
                        bitCount.wrapping_sub(8 * iend.wrapping_sub(7).wrapping_sub(ip) as i32);
                    bitCount &= 31;
                    ip = iend - 4;
                }
                bitStream = read_u32_le(ip) >> bitCount;
                repeats = bitStream.trailing_ones() >> 1;
            }

            charnum = charnum.wrapping_add(3 * repeats);
            bitStream >>= 2 * repeats;
            bitCount += 2 * repeats as i32;

            /* Add the final repeat which isn't 0b11. */
            debug_assert!((bitStream & 0b11) < 3);
            charnum = charnum.wrapping_add(bitStream & 0b11);
            bitCount += 2;

            /* This is an error, but break and return an error
             * at the end, because returning out of a loop makes
             * it harder for the compiler to optimize.
             */
            if charnum >= maxSV1 {
                break;
            }

            /* We don't need to set the normalized count to 0
             * because we already memset the whole buffer to 0.
             */

            if (ip <= iend - 7) || (ip + (bitCount as usize >> 3)) <= iend - 4 {
                ip += bitCount as usize >> 3;
                bitCount &= 0b111;
            } else {
                bitCount -= 8 * ((iend - 4) - ip) as i32;
                bitCount &= 31;
                ip = iend - 4;
            }

            bitStream = read_u32_le(ip) >> bitCount;
        }

        let max = 2 * threshold - 1 - remaining;
        let mut count;
        if (bitStream & (threshold - 1) as u32) < max as u32 {
            count = (bitStream & (threshold - 1) as u32) as core::ffi::c_int;
            bitCount += nbBits - 1;
        } else {
            count = (bitStream & (2 * threshold - 1) as u32) as core::ffi::c_int;
            if count >= threshold {
                count -= max;
            }
            bitCount += nbBits;
        }
        count -= 1;

        if count >= 0 {
            remaining -= count;
        } else {
            remaining += count;
        }

        normalizedCounter[charnum as usize] = count as core::ffi::c_short;
        charnum += 1;
        previous_was_0 = count == 0;
        if remaining < threshold {
            if remaining <= 1 {
                break;
            }
            nbBits = remaining.ilog2() as i32 + 1;
            threshold = 1 << (nbBits - 1);
        }
        if charnum >= maxSV1 {
            break;
        }
        if (ip <= iend - 7) || (ip + (bitCount as usize >> 3)) <= iend - 4 {
            ip += bitCount as usize >> 3;
            bitCount &= 7;
        } else {
            bitCount -= 8 * ((iend - 4) - ip) as i32;
            bitCount &= 31;
            ip = iend - 4;
        }
        bitStream = read_u32_le(ip) >> bitCount;
    }

    if remaining != 1 {
        return Err(Error::corruption_detected);
    }
    if charnum > maxSV1 {
        return Err(Error::maxSymbolValue_tooSmall);
    }
    if bitCount > 32 {
        return Err(Error::corruption_detected);
    }

    match u8::try_from(charnum - 1) {
        Ok(v) => *maxSVPtr = v,
        Err(_) => return Err(Error::maxSymbolValue_tooSmall),
    }

    ip += ((bitCount + 7) >> 3) as usize;

    Ok(ip)
}

fn FSE_readNCount_body_default(
    normalizedCounter: &mut [i16],
    maxSVPtr: &mut u8,
    tableLogPtr: &mut core::ffi::c_uint,
    headerBuffer: &[u8],
) -> Result<usize, Error> {
    FSE_readNCount_body(normalizedCounter, maxSVPtr, tableLogPtr, headerBuffer)
}
#[cfg_attr(target_arch = "x86_64", target_feature(enable = "bmi2"))]
fn FSE_readNCount_body_bmi2(
    normalizedCounter: &mut [i16],
    maxSVPtr: &mut u8,
    tableLogPtr: &mut core::ffi::c_uint,
    headerBuffer: &[u8],
) -> Result<usize, Error> {
    FSE_readNCount_body(normalizedCounter, maxSVPtr, tableLogPtr, headerBuffer)
}

pub(super) fn FSE_readNCount_bmi2(
    normalizedCounter: &mut [i16],
    maxSVPtr: &mut u8,
    tableLogPtr: &mut core::ffi::c_uint,
    headerBuffer: &[u8],
    bmi2: core::ffi::c_int,
) -> Result<usize, Error> {
    if bmi2 != 0 {
        // SAFETY: the bmi2 feature is enabled.
        unsafe { FSE_readNCount_body_bmi2(normalizedCounter, maxSVPtr, tableLogPtr, headerBuffer) }
    } else {
        FSE_readNCount_body_default(normalizedCounter, maxSVPtr, tableLogPtr, headerBuffer)
    }
}

pub(crate) fn FSE_readNCount(
    normalizedCounter: &mut [i16],
    maxSVPtr: &mut u8,
    tableLogPtr: &mut core::ffi::c_uint,
    headerBuffer: &[u8],
) -> Result<usize, Error> {
    FSE_readNCount_bmi2(normalizedCounter, maxSVPtr, tableLogPtr, headerBuffer, 0)
}

pub(crate) fn HUF_readStats(
    huffWeight: &mut [u8; 256],
    hwSize: usize,
    rankStats: &mut [u32; 13],
    nbSymbolsPtr: &mut u32,
    tableLogPtr: &mut u32,
    src: &[u8],
) -> Result<usize, Error> {
    // We can remove this at some point, it's just a check that the constants are correct.
    const _: () = assert!(HUF_READ_STATS_WORKSPACE_SIZE_U32 == 219);

    const _: () = assert!(HUF_READ_STATS_WORKSPACE_SIZE_U32 == size_of::<Workspace>() / 4);

    let mut wksp = Workspace::new();

    HUF_readStats_wksp(
        huffWeight,
        hwSize,
        rankStats,
        nbSymbolsPtr,
        tableLogPtr,
        src,
        &mut wksp,
        0,
    )
}
#[inline(always)]
fn HUF_readStats_body(
    huffWeight: &mut [u8; 256],
    hwSize: usize,
    rankStats: &mut [u32; 13],
    nbSymbolsPtr: &mut u32,
    tableLogPtr: &mut u32,
    mut ip: &[u8],
    workspace: &mut Workspace,
    bmi2: bool,
) -> Result<usize, Error> {
    let srcSize = ip.len();
    if srcSize == 0 {
        return Err(Error::srcSize_wrong);
    }

    let mut iSize = usize::from(ip[0]);
    let oSize: usize;
    if iSize >= 128 {
        // Special header case.
        oSize = iSize.wrapping_sub(127);
        iSize = oSize.wrapping_add(1) / 2;
        if iSize.wrapping_add(1) > srcSize {
            return Err(Error::srcSize_wrong);
        }
        if oSize >= hwSize {
            return Err(Error::corruption_detected);
        }
        ip = &ip[1..];
        for n in (0..oSize).step_by(2) {
            huffWeight[n] = ip[n / 2] >> 4;
            huffWeight[n + 1] = ip[n / 2] & 0b1111;
        }
    } else {
        // Normal case: header compressed with FSE.
        if iSize.wrapping_add(1) > srcSize {
            return Err(Error::srcSize_wrong);
        }

        oSize = FSE_decompress_wksp_bmi2(
            // At most (hwSize-1) values decoded, the last one is implied.
            &mut huffWeight[..hwSize - 1],
            &ip[1..][..iSize],
            6,
            // TODO this should probably be a (4-byte aligned) byte slice from the start.
            workspace,
            bmi2,
        )?;
    }

    // Collect weight stats.
    rankStats[..HUF_TABLELOG_MAX + 1].fill(0);
    let mut weightTotal = 0;
    for weight in huffWeight[..oSize].iter() {
        let Some(rank_stat) = rankStats.get_mut(usize::from(*weight)) else {
            return Err(Error::corruption_detected);
        };
        *rank_stat += 1;
        weightTotal += (1 << weight >> 1) as u32;
    }
    if weightTotal == 0 {
        return Err(Error::corruption_detected);
    }

    // Get last non-null symbol weight (implied, total must be 2^n).
    let tableLog = weightTotal.ilog2() + 1;
    if tableLog > HUF_TABLELOG_MAX as u32 {
        return Err(Error::corruption_detected);
    }
    *tableLogPtr = tableLog;

    // Determine last weight.
    let total = 1u32 << tableLog;
    let rest = total.wrapping_sub(weightTotal);
    let verif = 1u32 << rest.ilog2();
    let lastWeight = rest.ilog2() + 1;
    if verif != rest {
        return Err(Error::corruption_detected);
    }
    huffWeight[oSize] = lastWeight as u8;
    rankStats[lastWeight as usize] += 1;

    // Check tree construction validity.
    if rankStats[1] < 2 || rankStats[1] & 1 != 0 {
        return Err(Error::corruption_detected);
    }

    // Store results.
    *nbSymbolsPtr = oSize.wrapping_add(1) as u32;
    Ok(iSize.wrapping_add(1))
}

#[derive(Debug, Copy, Clone, PartialEq)]
#[repr(C, align(4))]
pub(super) struct DTable {
    pub(super) header: FSE_DTableHeader,
    pub(super) elements: [FSE_decode_t; 90],
}

impl Default for DTable {
    fn default() -> Self {
        Self::new()
    }
}

impl DTable {
    const fn new() -> Self {
        Self {
            header: FSE_DTableHeader {
                tableLog: 0,
                fastMode: 0,
            },
            elements: [FSE_decode_t {
                newState: 0,
                symbol: 0,
                nbBits: 0,
            }; 90],
        }
    }

    pub(super) fn destructure_mut(
        &mut self,
        max_symbol_value: u8,
        tableLog: u32,
    ) -> (
        &mut FSE_DTableHeader,
        &mut [FSE_decode_t],
        &mut [u16],
        &mut [u8],
    ) {
        let (elements, rest) = self.elements.split_at_mut(1usize << tableLog);
        let rest = unsafe {
            core::slice::from_raw_parts_mut(rest.as_mut_ptr().cast::<u16>(), rest.len() * 2)
        };

        let (symbols, rest) = rest.split_at_mut(usize::from(max_symbol_value) + 1);
        let spread = unsafe {
            core::slice::from_raw_parts_mut(rest.as_mut_ptr().cast::<u8>(), rest.len() * 2)
        };

        (&mut self.header, elements, symbols, spread)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Default)]
pub(crate) struct Workspace {
    pub(super) a: FSE_DecompressWksp,
    pub(super) dtable: DTable,
}

impl Workspace {
    const _SIZE: () = assert!(size_of::<Self>() == 4 * 219);

    const fn new() -> Self {
        Self {
            a: FSE_DecompressWksp {
                ncount: [0i16; 256],
            },
            dtable: DTable::new(),
        }
    }
}

#[cfg(test)]
impl quickcheck::Arbitrary for Workspace {
    fn arbitrary(g: &mut quickcheck::Gen) -> Self {
        Self {
            a: {
                let mut arr = [0i16; 256];
                for elem in &mut arr {
                    *elem = i16::arbitrary(g);
                }
                FSE_DecompressWksp { ncount: arr }
            },
            dtable: DTable::arbitrary(g),
        }
    }
}

#[cfg(test)]
impl quickcheck::Arbitrary for DTable {
    fn arbitrary(g: &mut quickcheck::Gen) -> Self {
        Self {
            header: FSE_DTableHeader {
                tableLog: u16::arbitrary(g),
                fastMode: u16::arbitrary(g),
            },
            elements: {
                let mut arr = [FSE_decode_t {
                    newState: 0,
                    symbol: 0,
                    nbBits: 0,
                }; 90];
                for elem in &mut arr {
                    *elem = FSE_decode_t {
                        newState: quickcheck::Arbitrary::arbitrary(g),
                        symbol: quickcheck::Arbitrary::arbitrary(g),
                        nbBits: quickcheck::Arbitrary::arbitrary(g),
                    }
                }

                arr
            },
        }
    }
}

pub(crate) fn HUF_readStats_wksp(
    huffWeight: &mut [u8; 256],
    hwSize: usize,
    rankStats: &mut [u32; 13],
    nbSymbolsPtr: &mut u32,
    tableLogPtr: &mut u32,
    src: &[u8],
    workspace: &mut Workspace,
    flags: core::ffi::c_int,
) -> Result<usize, Error> {
    let use_bmi2 = flags & HUF_flags_bmi2 as core::ffi::c_int != 0;

    HUF_readStats_body(
        huffWeight,
        hwSize,
        rankStats,
        nbSymbolsPtr,
        tableLogPtr,
        src,
        workspace,
        use_bmi2,
    )
}

#[cfg(test)]
mod tests {
    use crate::lib::common::error_private::ERR_getErrorCode;

    use super::*;

    use quickcheck::quickcheck;

    #[derive(Debug, Clone, PartialEq)]
    struct Input {
        huffWeight: [u8; 256],
        rankStats: [u32; 13],
        nbSymbolsPtr: u32,
        tableLogPtr: u32,
        src: Vec<u8>,
        workspace: Workspace,
        bmi2: bool,
    }

    impl quickcheck::Arbitrary for Input {
        fn arbitrary(g: &mut quickcheck::Gen) -> Self {
            Input {
                huffWeight: {
                    let mut arr = [0u8; 256];
                    for elem in &mut arr {
                        *elem = u8::arbitrary(g);
                    }
                    arr
                },
                rankStats: {
                    let mut arr = [0u32; 13];
                    for elem in &mut arr {
                        *elem = u32::arbitrary(g);
                    }
                    arr
                },
                nbSymbolsPtr: u32::arbitrary(g),
                tableLogPtr: u32::arbitrary(g),
                src: Vec::<u8>::arbitrary(g),
                workspace: Workspace::arbitrary(g),
                bmi2: bool::arbitrary(g),
            }
        }
    }

    quickcheck! {
        #[cfg(not(miri))]
        fn new_matches_old(input: Input) -> bool {
            unsafe {
                let expected = {
                    let Input {
                        mut huffWeight,
                        mut rankStats,
                        mut nbSymbolsPtr,
                        mut tableLogPtr,
                        src,
                        workspace,
                        bmi2,
                    } = input.clone();

                    #[allow(clippy::missing_transmute_annotations)]
                    let mut workspace =  core::mem::transmute(workspace) ;

                    // Pull in libzstd.a
                    use zstd_sys as _;

                    // This is not meant to be exported by the C version of ZSTD,
                    // so zstd_sys doesn't provide a declaration for it. The
                    // reason declaring this function works anyway is that static
                    // libraries don't provide a way to not export symbols.
                    extern "C" {
                        fn HUF_readStats_wksp(
                            huffWeight: &mut [u8; 256],
                            hwSize: usize,
                            rankStats: &mut [u32; 13],
                            nbSymbolsPtr: &mut u32,
                            tableLogPtr: &mut u32,
                            src: *const core::ffi::c_void,
                            srcSize: usize,
                            workspace: &mut [u32; 219],
                            workspace_size: usize,
                            flags: core::ffi::c_int,
                        ) -> usize;
                    }

                    let v = HUF_readStats_wksp(
                        &mut huffWeight,
                        256,
                        &mut rankStats,
                        &mut nbSymbolsPtr,
                        &mut tableLogPtr,
                        src.as_ptr().cast::<core::ffi::c_void>(),
                        src.len(),
                        &mut workspace,
                        size_of::<Workspace>(),
                        if bmi2 { HUF_flags_bmi2 as core::ffi::c_int } else { 0 },
                    );

                    let v = match ERR_getErrorCode(v) {
                        0 => Ok(v),
                        code => Err(Error::try_from(code).unwrap()),
                    };

                    (v, huffWeight, rankStats, nbSymbolsPtr, tableLogPtr, workspace)
                };
                let actual = {
                    let Input {
                        mut huffWeight,
                        mut rankStats,
                        mut nbSymbolsPtr,
                        mut tableLogPtr,
                        src,
                        mut workspace,
                        bmi2,
                    } = input.clone();
                    let v = HUF_readStats_body(
                        &mut huffWeight,
                        256,
                        &mut rankStats,
                        &mut nbSymbolsPtr,
                        &mut tableLogPtr,
                        &src,
                        &mut workspace,
                        bmi2,
                    );

                    #[allow(clippy::missing_transmute_annotations)]
                    (v, huffWeight, rankStats, nbSymbolsPtr, tableLogPtr, core::mem::transmute::<_, [u32; 219]>(workspace))
                };
                assert_eq!(expected, actual);
                expected == actual
            }
        }
    }
}
