//! Virtual memory and file mappings: `Win32::System::Memory`, declared from the Win32 API
//! reference (memoryapi.h, winnt.h) as windows-sys 0.61.2 has them, with typed wrappers for
//! reserving, committing and freeing.

use core::ffi::c_void;
use core::ptr::NonNull;

use super::security::SECURITY_ATTRIBUTES;
use super::{Bool, Handle, PCWSTR, WinError};

// ---- Aliases and constants -------------------------------------------------------------------

/// replaces windows_sys::Win32::System::Memory::FILE_MAP
pub type FILE_MAP = u32;
/// replaces windows_sys::Win32::System::Memory::PAGE_PROTECTION_FLAGS
pub type PAGE_PROTECTION_FLAGS = u32;
/// replaces windows_sys::Win32::System::Memory::VIRTUAL_ALLOCATION_TYPE
pub type VIRTUAL_ALLOCATION_TYPE = u32;
/// replaces windows_sys::Win32::System::Memory::VIRTUAL_FREE_TYPE
pub type VIRTUAL_FREE_TYPE = u32;
/// `PAGE_TYPE`: `MEMORY_BASIC_INFORMATION::Type` (`MEM_IMAGE`, `MEM_MAPPED`, `MEM_PRIVATE`).
/// replaces windows_sys::Win32::System::Memory::PAGE_TYPE
pub type PAGE_TYPE = u32;
/// `UNMAP_VIEW_OF_FILE_FLAGS`: `UnmapViewOfFile2`'s flags (also `MEM_PRESERVE_PLACEHOLDER`'s
/// type, which `VirtualFreeEx` takes too).
/// replaces windows_sys::Win32::System::Memory::UNMAP_VIEW_OF_FILE_FLAGS
pub type UNMAP_VIEW_OF_FILE_FLAGS = u32;
/// `MEM_EXTENDED_PARAMETER_TYPE` (winnt.h): what a [`MEM_EXTENDED_PARAMETER`] carries.
/// replaces windows_sys::Win32::System::Memory::MEM_EXTENDED_PARAMETER_TYPE
pub type MEM_EXTENDED_PARAMETER_TYPE = i32;

/// `FILE_MAP_READ` (memoryapi.h): a read-only view.
/// replaces windows_sys::Win32::System::Memory::FILE_MAP_READ
pub const FILE_MAP_READ: FILE_MAP = 4u32;

/// `MEM_COMMIT` (winnt.h): back reserved pages with commit charge; they read as zero.
/// replaces windows_sys::Win32::System::Memory::MEM_COMMIT
pub const MEM_COMMIT: VIRTUAL_ALLOCATION_TYPE = 4096u32;
/// `MEM_RESERVE` (winnt.h): take address space, no charge.
/// replaces windows_sys::Win32::System::Memory::MEM_RESERVE
pub const MEM_RESERVE: VIRTUAL_ALLOCATION_TYPE = 8192u32;
/// `MEM_DECOMMIT` (winnt.h): give committed pages back, keep the reservation.
/// replaces windows_sys::Win32::System::Memory::MEM_DECOMMIT
pub const MEM_DECOMMIT: VIRTUAL_FREE_TYPE = 16384u32;
/// `MEM_RELEASE` (winnt.h): free the whole reservation (size 0, its base address).
/// replaces windows_sys::Win32::System::Memory::MEM_RELEASE
pub const MEM_RELEASE: VIRTUAL_FREE_TYPE = 32768u32;

/// `PAGE_NOACCESS` (winnt.h).
/// replaces windows_sys::Win32::System::Memory::PAGE_NOACCESS
pub const PAGE_NOACCESS: PAGE_PROTECTION_FLAGS = 1u32;
/// `PAGE_READONLY` (winnt.h).
/// replaces windows_sys::Win32::System::Memory::PAGE_READONLY
pub const PAGE_READONLY: PAGE_PROTECTION_FLAGS = 2u32;
/// `PAGE_READWRITE` (winnt.h).
/// replaces windows_sys::Win32::System::Memory::PAGE_READWRITE
pub const PAGE_READWRITE: PAGE_PROTECTION_FLAGS = 4u32;
/// `PAGE_WRITECOPY` (winnt.h): readable, and copied on the first write (an image's data).
/// replaces windows_sys::Win32::System::Memory::PAGE_WRITECOPY
pub const PAGE_WRITECOPY: PAGE_PROTECTION_FLAGS = 8u32;
/// `PAGE_EXECUTE_READ` (winnt.h).
/// replaces windows_sys::Win32::System::Memory::PAGE_EXECUTE_READ
pub const PAGE_EXECUTE_READ: PAGE_PROTECTION_FLAGS = 32u32;
/// `PAGE_EXECUTE_READWRITE` (winnt.h): a section's most a view may take, not a page's.
/// replaces windows_sys::Win32::System::Memory::PAGE_EXECUTE_READWRITE
pub const PAGE_EXECUTE_READWRITE: PAGE_PROTECTION_FLAGS = 64u32;
/// `SEC_COMMIT` (winnt.h): `CreateFileMappingW`: the section's pages are committed with it.
/// replaces windows_sys::Win32::System::Memory::SEC_COMMIT
pub const SEC_COMMIT: PAGE_PROTECTION_FLAGS = 134217728u32;

/// `MEM_REPLACE_PLACEHOLDER` (winnt.h): `VirtualAlloc2` or `MapViewOfFile3` over exactly one
/// placeholder.
/// replaces windows_sys::Win32::System::Memory::MEM_REPLACE_PLACEHOLDER
pub const MEM_REPLACE_PLACEHOLDER: VIRTUAL_ALLOCATION_TYPE = 16384u32;
/// `MEM_RESERVE_PLACEHOLDER` (winnt.h): `VirtualAlloc2`: reserve the range as a placeholder.
/// replaces windows_sys::Win32::System::Memory::MEM_RESERVE_PLACEHOLDER
pub const MEM_RESERVE_PLACEHOLDER: VIRTUAL_ALLOCATION_TYPE = 262144u32;
/// `MEM_PRESERVE_PLACEHOLDER` (winnt.h): `VirtualFreeEx` with `MEM_RELEASE` splits a
/// placeholder; `UnmapViewOfFile2` leaves one where the view was.
/// replaces windows_sys::Win32::System::Memory::MEM_PRESERVE_PLACEHOLDER
pub const MEM_PRESERVE_PLACEHOLDER: UNMAP_VIEW_OF_FILE_FLAGS = 2u32;
/// `MEM_IMAGE` (winnt.h): the pages are a view of an image section.
/// replaces windows_sys::Win32::System::Memory::MEM_IMAGE
pub const MEM_IMAGE: PAGE_TYPE = 16777216u32;

// ---- Structures ------------------------------------------------------------------------------

/// `MEMORY_MAPPED_VIEW_ADDRESS` (memoryapi.h): a view's base, as `MapViewOfFile` returns it and
/// `UnmapViewOfFile` takes it; null on failure.
/// replaces windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MEMORY_MAPPED_VIEW_ADDRESS {
    pub Value: *mut c_void,
}

impl Default for MEMORY_MAPPED_VIEW_ADDRESS {
    fn default() -> Self {
        MEMORY_MAPPED_VIEW_ADDRESS { Value: core::ptr::null_mut() }
    }
}

/// `WIN32_MEMORY_RANGE_ENTRY` (memoryapi.h): one range for `PrefetchVirtualMemory`.
/// replaces windows_sys::Win32::System::Memory::WIN32_MEMORY_RANGE_ENTRY
#[repr(C)]
#[derive(Clone, Copy)]
pub struct WIN32_MEMORY_RANGE_ENTRY {
    pub VirtualAddress: *mut c_void,
    pub NumberOfBytes: usize,
}

impl Default for WIN32_MEMORY_RANGE_ENTRY {
    fn default() -> Self {
        WIN32_MEMORY_RANGE_ENTRY { VirtualAddress: core::ptr::null_mut(), NumberOfBytes: 0 }
    }
}

/// `MEMORY_BASIC_INFORMATION` (winnt.h): a region of pages that share their state, protection
/// and type, as `VirtualQueryEx` writes it (the 64-bit layout, with `PartitionId`).
/// replaces windows_sys::Win32::System::Memory::MEMORY_BASIC_INFORMATION
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MEMORY_BASIC_INFORMATION {
    pub BaseAddress: *mut c_void,
    pub AllocationBase: *mut c_void,
    pub AllocationProtect: PAGE_PROTECTION_FLAGS,
    pub PartitionId: u16,
    pub RegionSize: usize,
    pub State: VIRTUAL_ALLOCATION_TYPE,
    pub Protect: PAGE_PROTECTION_FLAGS,
    pub Type: PAGE_TYPE,
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
impl Default for MEMORY_BASIC_INFORMATION {
    fn default() -> Self {
        MEMORY_BASIC_INFORMATION {
            BaseAddress: core::ptr::null_mut(),
            AllocationBase: core::ptr::null_mut(),
            AllocationProtect: 0,
            PartitionId: 0,
            RegionSize: 0,
            State: 0,
            Protect: 0,
            Type: 0,
        }
    }
}

/// `MEM_EXTENDED_PARAMETER` (winnt.h): one extended parameter of `VirtualAlloc2` or
/// `MapViewOfFile3`: its type in the low 8 bits of the first word, its value in the second.
/// replaces windows_sys::Win32::System::Memory::MEM_EXTENDED_PARAMETER
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct MEM_EXTENDED_PARAMETER {
    pub Anonymous1: MEM_EXTENDED_PARAMETER_0,
    pub Anonymous2: MEM_EXTENDED_PARAMETER_1,
}

/// `MEM_EXTENDED_PARAMETER`'s bit fields (winnt.h): `Type : 8`, `Reserved : 56`.
/// replaces windows_sys::Win32::System::Memory::MEM_EXTENDED_PARAMETER_0
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct MEM_EXTENDED_PARAMETER_0 {
    pub _bitfield: u64,
}

/// `MEM_EXTENDED_PARAMETER`'s value (winnt.h).
/// replaces windows_sys::Win32::System::Memory::MEM_EXTENDED_PARAMETER_1
#[repr(C)]
#[derive(Clone, Copy)]
pub union MEM_EXTENDED_PARAMETER_1 {
    pub ULong64: u64,
    pub Pointer: *mut c_void,
    pub Size: usize,
    pub Handle: Handle,
    pub ULong: u32,
}

impl Default for MEM_EXTENDED_PARAMETER_1 {
    fn default() -> Self {
        // `ULong64` is as wide as the union (every member is at most 8 bytes), so all of it is 0.
        MEM_EXTENDED_PARAMETER_1 { ULong64: 0 }
    }
}

// Win32 API: memoryapi.h and winnt.h. LLP64: x86-64 and arm64 Windows lay these out alike.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const _: () = {
    use core::mem::offset_of;

    assert!(size_of::<MEMORY_BASIC_INFORMATION>() == 48);
    assert!(align_of::<MEMORY_BASIC_INFORMATION>() == 8);
    assert!(offset_of!(MEMORY_BASIC_INFORMATION, BaseAddress) == 0);
    assert!(offset_of!(MEMORY_BASIC_INFORMATION, AllocationBase) == 8);
    assert!(offset_of!(MEMORY_BASIC_INFORMATION, AllocationProtect) == 16);
    assert!(offset_of!(MEMORY_BASIC_INFORMATION, PartitionId) == 20);
    assert!(offset_of!(MEMORY_BASIC_INFORMATION, RegionSize) == 24);
    assert!(offset_of!(MEMORY_BASIC_INFORMATION, State) == 32);
    assert!(offset_of!(MEMORY_BASIC_INFORMATION, Protect) == 36);
    assert!(offset_of!(MEMORY_BASIC_INFORMATION, Type) == 40);
    assert!(size_of::<MEM_EXTENDED_PARAMETER_1>() == 8);
    assert!(size_of::<MEM_EXTENDED_PARAMETER>() == 16);
    assert!(align_of::<MEM_EXTENDED_PARAMETER>() == 8);
    assert!(offset_of!(MEM_EXTENDED_PARAMETER, Anonymous2) == 8);

    assert!(size_of::<MEMORY_MAPPED_VIEW_ADDRESS>() == 8);
    assert!(align_of::<MEMORY_MAPPED_VIEW_ADDRESS>() == 8);
    assert!(size_of::<WIN32_MEMORY_RANGE_ENTRY>() == 16);
    assert!(align_of::<WIN32_MEMORY_RANGE_ENTRY>() == 8);
    assert!(offset_of!(WIN32_MEMORY_RANGE_ENTRY, VirtualAddress) == 0);
    assert!(offset_of!(WIN32_MEMORY_RANGE_ENTRY, NumberOfBytes) == 8);
};

// ---- Functions -------------------------------------------------------------------------------

#[link(name = "kernel32", kind = "raw-dylib")]
unsafe extern "system" {
    /// `CreateFileMappingW` (memoryapi.h): [`Handle::NULL`] on failure (not `INVALID`).
    /// replaces windows_sys::Win32::System::Memory::CreateFileMappingW
    pub fn CreateFileMappingW(
        hfile: Handle,
        lpfilemappingattributes: *const SECURITY_ATTRIBUTES,
        flprotect: PAGE_PROTECTION_FLAGS,
        dwmaximumsizehigh: u32,
        dwmaximumsizelow: u32,
        lpname: PCWSTR,
    ) -> Handle;

    /// `MapViewOfFile` (memoryapi.h): a null `Value` on failure.
    /// replaces windows_sys::Win32::System::Memory::MapViewOfFile
    pub fn MapViewOfFile(
        hfilemappingobject: Handle,
        dwdesiredaccess: FILE_MAP,
        dwfileoffsethigh: u32,
        dwfileoffsetlow: u32,
        dwnumberofbytestomap: usize,
    ) -> MEMORY_MAPPED_VIEW_ADDRESS;

    /// `PrefetchVirtualMemory` (memoryapi.h): a hint to read the ranges in.
    /// replaces windows_sys::Win32::System::Memory::PrefetchVirtualMemory
    pub fn PrefetchVirtualMemory(
        hprocess: Handle,
        numberofentries: usize,
        virtualaddresses: *const WIN32_MEMORY_RANGE_ENTRY,
        flags: u32,
    ) -> Bool;

    /// `UnmapViewOfFile` (memoryapi.h).
    /// replaces windows_sys::Win32::System::Memory::UnmapViewOfFile
    pub fn UnmapViewOfFile(lpbaseaddress: MEMORY_MAPPED_VIEW_ADDRESS) -> Bool;

    /// `VirtualAlloc` (memoryapi.h): null on failure.
    /// replaces windows_sys::Win32::System::Memory::VirtualAlloc
    pub fn VirtualAlloc(
        lpaddress: *const c_void,
        dwsize: usize,
        flallocationtype: VIRTUAL_ALLOCATION_TYPE,
        flprotect: PAGE_PROTECTION_FLAGS,
    ) -> *mut c_void;

    /// `VirtualAllocExNuma` (memoryapi.h): null on failure; `nndpreferred` is the node whose
    /// memory the pages come from when they are first touched.
    /// replaces windows_sys::Win32::System::Memory::VirtualAllocExNuma
    pub fn VirtualAllocExNuma(
        hprocess: Handle,
        lpaddress: *const c_void,
        dwsize: usize,
        flallocationtype: VIRTUAL_ALLOCATION_TYPE,
        flprotect: u32,
        nndpreferred: u32,
    ) -> *mut c_void;

    /// `VirtualFree` (memoryapi.h).
    /// replaces windows_sys::Win32::System::Memory::VirtualFree
    pub fn VirtualFree(lpaddress: *mut c_void, dwsize: usize, dwfreetype: VIRTUAL_FREE_TYPE) -> Bool;

    /// `VirtualFreeEx` (memoryapi.h): `VirtualFree` in `hprocess`; with `MEM_RELEASE |
    /// MEM_PRESERVE_PLACEHOLDER` it splits a placeholder at `lpaddress + dwsize`.
    /// replaces windows_sys::Win32::System::Memory::VirtualFreeEx
    pub fn VirtualFreeEx(
        hprocess: Handle,
        lpaddress: *mut c_void,
        dwsize: usize,
        dwfreetype: VIRTUAL_FREE_TYPE,
    ) -> Bool;

    /// `VirtualProtectEx` (memoryapi.h): the pages' new protection in `hprocess`; the old one
    /// of the first page into `lpfloldprotect`, which must not be null.
    /// replaces windows_sys::Win32::System::Memory::VirtualProtectEx
    pub fn VirtualProtectEx(
        hprocess: Handle,
        lpaddress: *const c_void,
        dwsize: usize,
        flnewprotect: PAGE_PROTECTION_FLAGS,
        lpfloldprotect: *mut PAGE_PROTECTION_FLAGS,
    ) -> Bool;

    /// `VirtualQueryEx` (memoryapi.h): the region of `hprocess` holding `lpaddress`; the bytes
    /// written, 0 on failure.
    /// replaces windows_sys::Win32::System::Memory::VirtualQueryEx
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    pub fn VirtualQueryEx(
        hprocess: Handle,
        lpaddress: *const c_void,
        lpbuffer: *mut MEMORY_BASIC_INFORMATION,
        dwlength: usize,
    ) -> usize;
}

// windows-sys links these through the API set the Win32 reference names for them (they are
// kernelbase's exports; Windows 10 1803 and later).
#[link(name = "api-ms-win-core-memory-l1-1-6", kind = "raw-dylib")]
unsafe extern "system" {
    /// `MapViewOfFile3` (memoryapi.h): a view of `filemapping` in `process`, anywhere (a null
    /// `baseaddress`) or over a placeholder (`MEM_REPLACE_PLACEHOLDER`); a null `Value` on
    /// failure.
    /// replaces windows_sys::Win32::System::Memory::MapViewOfFile3
    pub fn MapViewOfFile3(
        filemapping: Handle,
        process: Handle,
        baseaddress: *const c_void,
        offset: u64,
        viewsize: usize,
        allocationtype: VIRTUAL_ALLOCATION_TYPE,
        pageprotection: u32,
        extendedparameters: *mut MEM_EXTENDED_PARAMETER,
        parametercount: u32,
    ) -> MEMORY_MAPPED_VIEW_ADDRESS;

    /// `VirtualAlloc2` (memoryapi.h): `VirtualAlloc` in `process`, with placeholders; null on
    /// failure.
    /// replaces windows_sys::Win32::System::Memory::VirtualAlloc2
    pub fn VirtualAlloc2(
        process: Handle,
        baseaddress: *const c_void,
        size: usize,
        allocationtype: VIRTUAL_ALLOCATION_TYPE,
        pageprotection: u32,
        extendedparameters: *mut MEM_EXTENDED_PARAMETER,
        parametercount: u32,
    ) -> *mut c_void;
}

#[link(name = "api-ms-win-core-memory-l1-1-5", kind = "raw-dylib")]
unsafe extern "system" {
    /// `UnmapViewOfFile2` (memoryapi.h): unmaps a view of `process`; `MEM_PRESERVE_PLACEHOLDER`
    /// leaves a placeholder where it was.
    /// replaces windows_sys::Win32::System::Memory::UnmapViewOfFile2
    pub fn UnmapViewOfFile2(
        process: Handle,
        baseaddress: MEMORY_MAPPED_VIEW_ADDRESS,
        unmapflags: UNMAP_VIEW_OF_FILE_FLAGS,
    ) -> Bool;
}

// ---- Typed wrappers --------------------------------------------------------------------------

flag_set! {
    /// What `VirtualAlloc` does to the range.
    Allocation: u32 {
        /// `MEM_COMMIT`.
        COMMIT = MEM_COMMIT;
        /// `MEM_RESERVE`.
        RESERVE = MEM_RESERVE;
    }
}

/// A page protection: one of them, never a set.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Protect {
    /// `PAGE_NOACCESS`: a reservation's pages.
    NoAccess,
    /// `PAGE_READONLY`.
    ReadOnly,
    /// `PAGE_READWRITE`.
    ReadWrite,
}

impl Protect {
    /// The `PAGE_*` value.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn bits(self) -> PAGE_PROTECTION_FLAGS {
        match self {
            Protect::NoAccess => PAGE_NOACCESS,
            Protect::ReadOnly => PAGE_READONLY,
            Protect::ReadWrite => PAGE_READWRITE,
        }
    }
}

/// What `VirtualFree` does: the two forms take different sizes, so the size is the choice's.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum MemFree {
    /// `MEM_DECOMMIT` of `bytes` from the address: the pages go, the reservation stays.
    Decommit { bytes: usize },
    /// `MEM_RELEASE` of the whole reservation whose base is the address (size 0, as the call
    /// requires).
    Release,
}

/// `GetCurrentProcess()`'s pseudo-handle, which is `INVALID_HANDLE_VALUE` (processthreadsapi.h).
const CURRENT_PROCESS: Handle = Handle::INVALID;

/// `VirtualAlloc`: reserves and/or commits `bytes` at `address` (null: where the system
/// chooses), at `protect`. The range's base, rounded down to the granularity.
///
/// # Safety
///
/// `address` is null, or it lies in a reservation this caller owns and no live reference
/// reads or writes pages of the range in a way `protect` forbids (committing changes the
/// protection of pages already committed).
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn virtual_alloc(
    address: *mut c_void,
    bytes: usize,
    kind: Allocation,
    protect: Protect,
) -> Result<NonNull<c_void>, WinError> {
    // SAFETY: the caller's contract above; the call reads no memory of ours.
    let base = unsafe { VirtualAlloc(address, bytes, kind.bits(), protect.bits()) };
    NonNull::new(base).ok_or_else(WinError::last)
}

/// `VirtualAllocExNuma` in this process, at an address the system chooses: a new range whose
/// pages come from NUMA node `node` when first touched.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn virtual_alloc_numa(bytes: usize, kind: Allocation, protect: Protect, node: u32) -> Result<NonNull<c_void>, WinError> {
    // SAFETY: a null address makes a new range nothing else refers to, so no reference can be
    // affected; the process is this one (its pseudo-handle).
    let base = unsafe {
        VirtualAllocExNuma(CURRENT_PROCESS, core::ptr::null(), bytes, kind.bits(), protect.bits(), node)
    };
    NonNull::new(base).ok_or_else(WinError::last)
}

/// `VirtualFree`: decommits a range or releases a whole reservation.
///
/// # Safety
///
/// `address` is in a range this caller owns (for [`MemFree::Release`], its reservation's base,
/// as `virtual_alloc` returned it), and nothing reads or writes the freed pages afterwards.
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn virtual_free(address: NonNull<c_void>, how: MemFree) -> Result<(), WinError> {
    let (bytes, kind) = match how {
        MemFree::Decommit { bytes } => (bytes, MEM_DECOMMIT),
        MemFree::Release => (0, MEM_RELEASE),
    };
    // SAFETY: the caller's contract above; the size is the one each form requires.
    unsafe { VirtualFree(address.as_ptr(), bytes, kind) }.ok()
}
