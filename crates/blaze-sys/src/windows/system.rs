//! The loader and the machine: `Win32::System::LibraryLoader` (a module's base and its
//! exports), `Win32::System::SystemInformation` (the page size and allocation granularity) and
//! `Win32::System::ProcessStatus` (a process's modules), declared from the Win32 API reference
//! (libloaderapi.h, sysinfoapi.h, psapi.h) as windows-sys 0.61.2 has them.

use super::foundation::{FARPROC, HMODULE};
use super::{Bool, Handle, PCSTR, PCWSTR};

// ---- Win32::System::SystemInformation --------------------------------------------------------

/// `PROCESSOR_ARCHITECTURE` (winnt.h): `SYSTEM_INFO::wProcessorArchitecture`.
///
/// replaces windows_sys::Win32::System::SystemInformation::PROCESSOR_ARCHITECTURE
pub type PROCESSOR_ARCHITECTURE = u16;

/// `SYSTEM_INFO` (sysinfoapi.h): the machine as `GetSystemInfo` describes it.
///
/// replaces windows_sys::Win32::System::SystemInformation::SYSTEM_INFO
#[derive(Clone, Copy)]
#[repr(C)]
pub struct SYSTEM_INFO {
    pub Anonymous: SYSTEM_INFO_0,
    pub dwPageSize: u32,
    pub lpMinimumApplicationAddress: *mut core::ffi::c_void,
    pub lpMaximumApplicationAddress: *mut core::ffi::c_void,
    pub dwActiveProcessorMask: usize,
    pub dwNumberOfProcessors: u32,
    pub dwProcessorType: u32,
    pub dwAllocationGranularity: u32,
    pub wProcessorLevel: u16,
    pub wProcessorRevision: u16,
}

impl Default for SYSTEM_INFO {
    fn default() -> Self {
        SYSTEM_INFO {
            Anonymous: SYSTEM_INFO_0::default(),
            dwPageSize: 0,
            lpMinimumApplicationAddress: core::ptr::null_mut(),
            lpMaximumApplicationAddress: core::ptr::null_mut(),
            dwActiveProcessorMask: 0,
            dwNumberOfProcessors: 0,
            dwProcessorType: 0,
            dwAllocationGranularity: 0,
            wProcessorLevel: 0,
            wProcessorRevision: 0,
        }
    }
}

/// `SYSTEM_INFO`'s unnamed union (sysinfoapi.h): the OEM ID, or the architecture.
///
/// replaces windows_sys::Win32::System::SystemInformation::SYSTEM_INFO_0
#[derive(Clone, Copy)]
#[repr(C)]
pub union SYSTEM_INFO_0 {
    pub dwOemId: u32,
    pub Anonymous: SYSTEM_INFO_0_0,
}

impl Default for SYSTEM_INFO_0 {
    fn default() -> Self {
        // `dwOemId` is as wide as the union, so all of it is 0.
        SYSTEM_INFO_0 { dwOemId: 0 }
    }
}

/// `SYSTEM_INFO`'s architecture half (sysinfoapi.h).
///
/// replaces windows_sys::Win32::System::SystemInformation::SYSTEM_INFO_0_0
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct SYSTEM_INFO_0_0 {
    pub wProcessorArchitecture: PROCESSOR_ARCHITECTURE,
    pub wReserved: u16,
}

// sysinfoapi.h, x86-64 and arm64 Windows (LLP64, 8-byte pointers and `DWORD_PTR`): identical.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
const _: () = {
    use core::mem::offset_of;
    assert!(size_of::<SYSTEM_INFO_0_0>() == 4 && align_of::<SYSTEM_INFO_0_0>() == 2);
    assert!(size_of::<SYSTEM_INFO_0>() == 4 && align_of::<SYSTEM_INFO_0>() == 4);
    assert!(size_of::<SYSTEM_INFO>() == 48 && align_of::<SYSTEM_INFO>() == 8);
    assert!(offset_of!(SYSTEM_INFO, Anonymous) == 0);
    assert!(offset_of!(SYSTEM_INFO, dwPageSize) == 4);
    assert!(offset_of!(SYSTEM_INFO, lpMinimumApplicationAddress) == 8);
    assert!(offset_of!(SYSTEM_INFO, lpMaximumApplicationAddress) == 16);
    assert!(offset_of!(SYSTEM_INFO, dwActiveProcessorMask) == 24);
    assert!(offset_of!(SYSTEM_INFO, dwNumberOfProcessors) == 32);
    assert!(offset_of!(SYSTEM_INFO, dwProcessorType) == 36);
    assert!(offset_of!(SYSTEM_INFO, dwAllocationGranularity) == 40);
    assert!(offset_of!(SYSTEM_INFO, wProcessorLevel) == 44);
    assert!(offset_of!(SYSTEM_INFO, wProcessorRevision) == 46);
};

// ---- Functions -------------------------------------------------------------------------------

#[link(name = "kernel32", kind = "raw-dylib")]
unsafe extern "system" {
    /// `GetSystemInfo` (sysinfoapi.h): fills `lpsysteminfo`; it cannot fail.
    ///
    /// replaces windows_sys::Win32::System::SystemInformation::GetSystemInfo
    pub fn GetSystemInfo(lpsysteminfo: *mut SYSTEM_INFO);

    /// `GetModuleHandleW` (libloaderapi.h): the base of a loaded module, the executable's for a
    /// null name; null when no such module is loaded. Takes no reference to it.
    ///
    /// replaces windows_sys::Win32::System::LibraryLoader::GetModuleHandleW
    pub fn GetModuleHandleW(lpmodulename: PCWSTR) -> HMODULE;

    /// `GetProcAddress` (libloaderapi.h): the export `lpprocname` of `hmodule`, forwarders
    /// followed; `None` when it exports no such name.
    ///
    /// replaces windows_sys::Win32::System::LibraryLoader::GetProcAddress
    pub fn GetProcAddress(hmodule: HMODULE, lpprocname: PCSTR) -> FARPROC;

    /// `K32EnumProcessModules` (psapi.h; psapi's `EnumProcessModules`, which kernel32 exports
    /// under this name): the process's modules in load order, the executable first, into `cb`
    /// bytes at `lphmodule`; the bytes all of them need into `lpcbneeded`.
    ///
    /// replaces windows_sys::Win32::System::ProcessStatus::K32EnumProcessModules (and the local
    /// declaration in library/blaze_inproc_rt/src/sys/windows.rs)
    pub fn K32EnumProcessModules(
        hprocess: Handle,
        lphmodule: *mut HMODULE,
        cb: u32,
        lpcbneeded: *mut u32,
    ) -> Bool;
}

// ---- Typed wrappers --------------------------------------------------------------------------

/// `GetSystemInfo`: the machine's page size, allocation granularity, processors.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn get_system_info() -> SYSTEM_INFO {
    let mut info = SYSTEM_INFO::default();
    // SAFETY: the call only writes the `SYSTEM_INFO` it is given, which is live for the call,
    // and keeps no pointer to it.
    unsafe { GetSystemInfo(&raw mut info) };
    info
}
