use crate::lib::common::fse::FSE_DECOMPRESS_WKSP_SIZE_U32;
use crate::lib::zstd::ZSTD_btultra;

pub(crate) const HUF_BLOCKSIZE_MAX: usize = 128 * 1024;

pub const HUF_WORKSPACE_SIZE: usize = (8 << 10) + 512;

/// Max runtime value of tableLog (due to static allocation); can be modified up to [`HUF_TABLELOG_ABSOLUTEMAX`].
pub(crate) const HUF_TABLELOG_MAX: usize = 12;
/// Default tableLog value when none specified
pub(crate) const HUF_TABLELOG_DEFAULT: u32 = 11;
pub(crate) const HUF_SYMBOLVALUE_MAX: u32 = HUF_SYMBOLVALUE_MAX_U8 as u32;
pub(crate) const HUF_SYMBOLVALUE_MAX_U8: u8 = 255;

pub(crate) const HUF_CTABLE_WORKSPACE_SIZE_U32: usize =
    (4 * (HUF_SYMBOLVALUE_MAX as usize + 1)) + 192;

pub(crate) const HUF_CTABLE_WORKSPACE_SIZE: usize =
    HUF_CTABLE_WORKSPACE_SIZE_U32 * size_of::<u32>();

/// Absolute limit of [`HUF_MAX_TABLELOG`]. Beyond that value, code does not work
pub(crate) const HUF_TABLELOG_ABSOLUTEMAX: usize = 12;
const _: () = assert!(
    HUF_TABLELOG_MAX <= HUF_TABLELOG_ABSOLUTEMAX,
    "HUF_TABLELOG_MAX is too large !"
);

pub(crate) const HUF_CTABLEBOUND: usize = 129;

#[derive(Copy, Clone)]
#[repr(transparent)]
pub(crate) struct HUF_CElt(pub(crate) usize);

pub const fn HUF_CTABLE_SIZE_ST(maxSymbolValue: usize) -> usize {
    maxSymbolValue + 2 /* Use tables of usize, for proper alignment */
}

pub const fn HUF_CTABLE_SIZE(maxSymbolValue: usize) -> usize {
    HUF_CTABLE_SIZE_ST(maxSymbolValue) * size_of::<usize>()
}

pub(crate) const HUF_flags_bmi2: core::ffi::c_uint = 1;
pub(crate) const HUF_flags_optimalDepth: core::ffi::c_uint = 2;
pub(crate) const HUF_flags_preferRepeat: core::ffi::c_uint = 4;
pub(crate) const HUF_flags_suspectUncompressible: core::ffi::c_uint = 8;
pub(crate) const HUF_flags_disableAsm: core::ffi::c_uint = 16;
pub(crate) const HUF_flags_disableFast: core::ffi::c_uint = 32;

pub(crate) const HUF_OPTIMAL_DEPTH_THRESHOLD: core::ffi::c_int = ZSTD_btultra as core::ffi::c_int;

#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq, Default)]
pub enum HUF_repeat {
    /// Cannot use the previous table
    #[default]
    None = 0,
    /// Can use the previous table but it must be checked.
    ///
    /// Note: the previous table must have been constructed by `HUF_compress{1,4}X_repeat`.
    Check = 1,
    /// Can use the previous table and it is assumed to be valid
    Valid = 2,
}

pub(crate) const HUF_READ_STATS_WORKSPACE_SIZE_U32: usize =
    FSE_DECOMPRESS_WKSP_SIZE_U32(6, HUF_TABLELOG_MAX - 1);

#[derive(Copy, Clone, Default)]
#[repr(C)]
pub(crate) struct HUF_CTableHeader {
    pub(crate) tableLog: u8,
    pub(crate) maxSymbolValue: u8,
    pub(crate) unused: [u8; size_of::<usize>() - 2],
}

impl HUF_CTableHeader {
    pub(crate) fn new(tableLog: u32, maxSymbolValue: u8) -> Self {
        debug_assert!(tableLog < 256);

        Self {
            tableLog: tableLog as u8,
            maxSymbolValue,
            unused: [0; _],
        }
    }
}

/// A Huffman compression table: a header, followed by one entry per symbol.
#[derive(Copy, Clone)]
#[repr(C)]
pub struct CTable {
    pub(crate) header: HUF_CTableHeader,
    pub(crate) elements: SymbolTable,
}

/// The per-symbol part of a [`CTable`]: everything after the header.
pub(crate) type SymbolTable = [HUF_CElt; HUF_SYMBOLVALUE_MAX as usize + 1];

const _: () = assert!(size_of::<CTable>() == HUF_CTABLE_SIZE_ST(255) * size_of::<HUF_CElt>());
const _: () = assert!(align_of::<CTable>() == align_of::<HUF_CElt>());

// not derived: `[T; N]: Default` only exists for `N <= 32`, and `elements` is longer
impl Default for CTable {
    fn default() -> Self {
        Self {
            header: HUF_CTableHeader::default(),
            elements: [HUF_CElt(0); _],
        }
    }
}
