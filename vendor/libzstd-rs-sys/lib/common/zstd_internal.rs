use core::mem::MaybeUninit;

const fn const_max(a: usize, b: usize) -> usize {
    if a > b {
        a
    } else {
        b
    }
}

pub(crate) const ZSTD_OPT_NUM: usize = 1 << 12;

pub(crate) const ZSTD_REP_NUM: u32 = 3;
pub(crate) type RepCodes = [u32; ZSTD_REP_NUM as usize];
pub(crate) static repStartValue: RepCodes = [1, 4, 8];

pub(crate) const ZSTD_FRAMEIDSIZE: usize = 4;

pub(crate) const ZSTD_BLOCKHEADERSIZE: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum BlockType {
    #[default]
    Raw = 0,
    Rle = 1,
    Compressed = 2,
    Reserved = 3,
}

impl From<u32> for BlockType {
    fn from(value: u32) -> Self {
        match value {
            0 => Self::Raw,
            1 => Self::Rle,
            2 => Self::Compressed,
            3 => Self::Reserved,
            _ => panic!("invalid `BlockType`: {value}"),
        }
    }
}

#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq, Default)]
pub enum SymbolEncodingType {
    #[default]
    Basic = 0,
    Rle = 1,
    Compressed = 2,
    Repeat = 3,
}

impl TryFrom<u8> for SymbolEncodingType {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Basic),
            1 => Ok(Self::Rle),
            2 => Ok(Self::Compressed),
            3 => Ok(Self::Repeat),
            _ => Err(()),
        }
    }
}

pub(crate) const LONGNBSEQ: core::ffi::c_int = 0x7f00;

pub(crate) const MINMATCH: u8 = 3;

pub(crate) const Litbits: u32 = 8;
pub(crate) const LitHufLog: u32 = 11;
const _: () = assert!(MaxLit as u32 == (1u32 << Litbits) - 1);
pub(crate) const MaxLit: u8 = 255;
pub(crate) const MaxML: u8 = 52;
pub(crate) const MaxLL: u8 = 35;
pub(crate) const DefaultMaxOff: u8 = 28;
pub(crate) const MaxOff: u8 = 31;
pub(crate) const MaxSeq: usize = const_max(MaxLL as usize, MaxML as usize); /* Assumption : MaxOff < MaxLL,MaxML */
pub(crate) const MLFSELog: u32 = 9;
pub(crate) const LLFSELog: u32 = 9;
pub(crate) const OffFSELog: u32 = 8;
pub(crate) const MaxFSELog: usize = const_max(
    const_max(MLFSELog as usize, LLFSELog as usize),
    OffFSELog as usize,
);
pub(crate) const MaxMLBits: u8 = 16;
pub(crate) const MaxLLBits: u8 = 16;

/// header + <= 127 byte tree description
pub(crate) const ZSTD_MAX_HUF_HEADER_SIZE: usize = 128;
/// Each table cannot take more than #symbols * FSELog bits
pub(crate) const ZSTD_MAX_FSE_HEADERS_SIZE: usize = ((MaxML as u32 + 1) * MLFSELog
    + (MaxLL as u32 + 1) * LLFSELog
    + (MaxOff as u32 + 1) * OffFSELog)
    .div_ceil(8) as usize;

pub(crate) static LL_bits: [u8; MaxLL as usize + 1] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10, 11,
    12, 13, 14, 15, 16,
];
pub(crate) static LL_defaultNorm: [i16; MaxLL as usize + 1] = [
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];
pub(crate) const LL_DEFAULTNORMLOG: u32 = 6;
pub(crate) static LL_defaultNormLog: u32 = LL_DEFAULTNORMLOG;
pub(crate) static ML_bits: [u8; MaxML as usize + 1] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
];
pub(crate) static ML_defaultNorm: [i16; MaxML as usize + 1] = [
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
];
pub(crate) const ML_DEFAULTNORMLOG: u32 = 6;
pub(crate) static ML_defaultNormLog: u32 = ML_DEFAULTNORMLOG;
pub(crate) static OF_defaultNorm: [i16; DefaultMaxOff as usize + 1] = [
    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
];
pub(crate) const OF_DEFAULTNORMLOG: u32 = 5;
pub(crate) static OF_defaultNormLog: u32 = OF_DEFAULTNORMLOG;

#[inline]
pub(crate) unsafe fn ZSTD_copy16(dst: *mut u8, src: *const u8) {
    // We use `copy` instead of `copy_nonoverlapping` here because the literal buffer can now
    // be located within the dst buffer. In circumstances where the op "catches up" to where the
    // literal buffer is, there can be partial overlaps in this call on the final
    // copy if the literal is being shifted by less than 16 bytes.
    crate::lib::polyfill::cfg_select! {
        target_arch = "x86_64" => {
            #[repr(C)]
            struct V {
                x: i64,
                y: i64,
            }

            // This generates the same instructions, but surrounding code optimizes better.
            let v = core::ptr::read_unaligned(src as *const MaybeUninit<V>);
            core::ptr::write_unaligned(dst as *mut MaybeUninit<V>, v);
        }
        _ => {
            core::ptr::copy(src, dst, 16);
        }
    };
}

pub(crate) const WILDCOPY_OVERLENGTH: usize = 32;
pub(crate) const WILDCOPY_VECLEN: core::ffi::c_int = 16;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum Overlap {
    NoOverlap,
    OverlapSrcBeforeDst,
}

/// Custom version of [`ZSTD_memcpy`], can over read/write up to [`WILDCOPY_OVERLENGTH`] bytes (if length==0)
///
/// The `ovtype` controls the overlap detection
/// - [`Overlap::NoOverlap`]: The source and destination are guaranteed to be at least [`WILDCOPY_VECLEN`] bytes apart.
/// - [`Overlap::OverlapSrcBeforeDst`]: The src and dst may overlap, but they MUST be at least 8 bytes apart.
///   The src buffer must be before the dst buffer.
#[inline(always)]
pub(crate) unsafe fn ZSTD_wildcopy(
    mut op: *mut u8,
    mut ip: *const u8,
    length: usize,
    ovtype: Overlap,
) {
    let diff = op as isize - ip as isize;
    let oend = op.add(length);
    if ovtype == Overlap::OverlapSrcBeforeDst && diff < WILDCOPY_VECLEN as isize {
        debug_assert!(ip <= op.wrapping_sub(8).cast_const());

        // Handle short offset copies.
        loop {
            // SAFETY: ip and op are guaranteed to be at least 8 bytes apart.
            core::ptr::copy_nonoverlapping(ip, op, 8);

            op = op.add(8);
            ip = ip.add(8);

            if op >= oend {
                break;
            }
        }
    } else {
        debug_assert!(diff.abs() >= WILDCOPY_VECLEN as isize);

        // NOTE: ip and op are at least 8 apart, but may be less than 16 apart, so we cannot use
        // `copy_nonoverlapping` when copying more than 8 bytes.

        // Separate out the first 16-byte copy call because the copy length is
        // almost certain to be short, so the branches have different
        // probabilities. Since it is almost certain to be short, only do
        // one 16-byte copy in the first call. Then, do two calls per loop since
        // at that point it is more likely to have a high trip count.
        ZSTD_copy16(op, ip);

        if 16 >= length {
            return;
        }

        op = op.add(16);
        ip = ip.add(16);

        loop {
            ZSTD_copy16(op, ip);
            op = op.add(16);
            ip = ip.add(16);

            ZSTD_copy16(op, ip);
            op = op.add(16);
            ip = ip.add(16);

            if op >= oend {
                break;
            }
        }
    }
}

#[inline]
pub(crate) unsafe fn ZSTD_limitCopy(
    dst: *mut u8,
    dstCapacity: usize,
    src: *const u8,
    srcSize: usize,
) -> usize {
    let length = Ord::min(dstCapacity, srcSize);
    core::ptr::copy_nonoverlapping(src, dst, length);
    length
}

pub(crate) const ZSTD_WORKSPACETOOLARGE_FACTOR: core::ffi::c_int = 3;
pub(crate) const ZSTD_WORKSPACETOOLARGE_MAXDURATION: core::ffi::c_int = 128;

#[inline]
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub(crate) fn ZSTD_cpuSupportsBmi2() -> bool {
    is_x86_feature_detected!("bmi1") && is_x86_feature_detected!("bmi2")
}

#[inline]
#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
pub(crate) fn ZSTD_cpuSupportsBmi2() -> bool {
    false
}
