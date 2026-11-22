//! Windows, through the system DLLs' exports, declared here: no `windows-sys`, no import
//! library, nothing from a registry (D180).
//!
//! Every import is linked with `#[link(name = "<dll>", kind = "raw-dylib")]`: rustc writes the
//! import table itself from the declaration, so a build needs neither the Windows SDK's `.lib`
//! files nor a crate that ships them (`windows-targets`). `raw-dylib` is stable on x86-64 and
//! arm64 Windows, the two targets blaze builds for.
//!
//! Two layers, as on the other systems:
//!
//! - The declarations, by area, under the Win32 names and with the documented prototypes (the
//!   Win32 API reference; each says so): [`foundation`], [`fs`], [`io`], [`memory`], [`net`],
//!   [`security`], [`threading`]. A handle is a [`Handle`], a `BOOL` a [`Bool`], a socket a
//!   [`Socket`]: none is an integer (rule 76). Each item says what it replaces
//!   (`/// replaces windows_sys::Win32::...`), so a port greps for it.
//! - Typed wrappers in the crate's style: a result is `Result<_, WinError>`, the thread's last
//!   error or an `NTSTATUS` mapped to its Win32 code; each wrapper's `// SAFETY:` states the
//!   call's contract.
//!
//! sys.md's typed operations ([`UNBUILT`]) are still refusals naming themselves: the table below
//! is their design, and blaze-core's IOCP driver makes its calls through the declarations.
//!
//! | need | import |
//! |---|---|
//! | open beneath a handle | `NtCreateFile` with a `RootDirectory` handle |
//! | read, write at an offset | `NtReadFile`, `NtWriteFile` (overlapped, on the port) |
//! | sync | `NtFlushBuffersFileEx` (`FLUSH_FLAGS_FILE_DATA_SYNC_ONLY` for a barrier) |
//! | rename, remove | `NtSetInformationFile` (`FileRenameInformationEx`, `FileDispositionInformationEx`: POSIX semantics) |
//! | clone | `FSCTL_DUPLICATE_EXTENTS_TO_FILE` (ReFS, Dev Drive) |
//! | memory | `NtAllocateVirtualMemory`, `NtFreeVirtualMemory`, `NtProtectVirtualMemory` |
//! | park, unpark | `RtlWaitOnAddress`, `RtlWakeAddressSingle` (ntdll; `WaitOnAddress` is their kernel32 forwarder) |
//! | completions | `NtCreateIoCompletion`, `NtRemoveIoCompletionEx`, `NtSetIoCompletion` |
//! | a child's exit | `NtWaitForSingleObject` on the process handle, by a wait packet on the port (`NtAssociateWaitCompletionPacket`) |
//! | clock | `RtlQueryPerformanceCounter`, `NtQuerySystemTimePrecise` |
//! | random | `ProcessPrng` (bcryptprimitives) |
//! | sockets | `\Device\Afd` by `NtDeviceIoControlFile`, as the IOCP driver already does |

#![allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    reason = "the declarations keep the Win32 names, so a port and the API reference read alike"
)]

use core::ffi::c_void;

pub mod diagnostics;
pub mod foundation;
pub mod fs;
pub mod io;
pub mod memory;
pub mod net;
pub mod security;
pub mod system;
pub mod threading;

/// A kernel object's handle: a file, a port, a process, a job, an event. Not an integer and not
/// a pointer to anything the program may read (rule 76); [`Handle::NULL`] and
/// [`Handle::INVALID`] are the two values a call returns for "none", and which one is the
/// call's (each declaration's doc says).
///
/// replaces windows_sys::Win32::Foundation::HANDLE (and INVALID_HANDLE_VALUE, as
/// [`Handle::INVALID`])
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(transparent)]
pub struct Handle(*mut c_void);

// SAFETY: a handle is a number in the process's handle table, the same from every thread; the
// kernel serializes what is done to the object. Nothing is read through the pointer.
unsafe impl Send for Handle {}
// SAFETY: as above.
unsafe impl Sync for Handle {}

impl Handle {
    /// No handle: what `CreateFileMappingW`, `CreateJobObjectW`, `OpenProcess` and most
    /// `Create*` calls return on failure.
    pub const NULL: Handle = Handle(core::ptr::null_mut());
    /// `INVALID_HANDLE_VALUE` (-1): what `CreateFileW`, `FindFirstFileW` and
    /// `CreateNamedPipeW` return on failure. Also `GetCurrentProcess()`'s pseudo-handle.
    pub const INVALID: Handle = Handle(usize::MAX as *mut c_void);

    /// The handle with this raw value: one a call or a child's inherited table gave.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn from_raw(raw: *mut c_void) -> Handle {
        Handle(raw)
    }

    /// The raw value, for a structure field the kernel reads (`STARTUPINFOW::hStdInput`).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn raw(self) -> *mut c_void {
        self.0
    }

    /// Whether this is [`Handle::NULL`].
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn is_null(self) -> bool {
        self.0.is_null()
    }

    /// Whether this is [`Handle::INVALID`].
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn is_invalid(self) -> bool {
        self == Handle::INVALID
    }
}

/// A Win32 `BOOL`: a 32-bit truth the call returns (nonzero is true); a false one means "see
/// `GetLastError`". Not a Rust `bool`, whose layout is one byte.
///
/// replaces windows_sys::core::BOOL
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(transparent)]
pub struct Bool(pub i32);

impl Bool {
    pub const FALSE: Bool = Bool(0);
    pub const TRUE: Bool = Bool(1);

    /// Whether it is true: any nonzero value is.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn is_true(self) -> bool {
        self.0 != 0
    }

    /// `Ok` when true, else the thread's last error: how every `BOOL` call is checked.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn ok(self) -> Result<(), WinError> {
        if self.is_true() { Ok(()) } else { Err(WinError::last()) }
    }
}

impl From<bool> for Bool {
    fn from(value: bool) -> Bool {
        if value { Bool::TRUE } else { Bool::FALSE }
    }
}

/// A Winsock socket: a kernel handle typed as `UINT_PTR` by the API.
///
/// replaces windows_sys::Win32::Networking::WinSock::SOCKET (and INVALID_SOCKET, as
/// [`Socket::INVALID`])
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(transparent)]
pub struct Socket(usize);

impl Socket {
    /// `INVALID_SOCKET` (`~0`): what `WSASocketW` returns on failure.
    pub const INVALID: Socket = Socket(usize::MAX);

    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn from_raw(raw: usize) -> Socket {
        Socket(raw)
    }

    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn raw(self) -> usize {
        self.0
    }

    /// The socket as the handle it is: what `CreateIoCompletionPort` and
    /// `SetFileCompletionNotificationModes` take.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn as_handle(self) -> Handle {
        Handle(self.0 as *mut c_void)
    }
}

/// `NTSTATUS`: ntdll's result; negative (the high bit) is an error.
///
/// replaces windows_sys::Win32::Foundation::NTSTATUS
pub type NTSTATUS = i32;
/// `PCWSTR`: a NUL-terminated UTF-16 string the call reads.
///
/// replaces windows_sys::core::PCWSTR
pub type PCWSTR = *const u16;
/// `PWSTR`: UTF-16 the call writes.
///
/// replaces windows_sys::core::PWSTR
pub type PWSTR = *mut u16;
/// `PSTR`: bytes the call writes, or a buffer it is handed (`WSABUF::buf`).
///
/// replaces windows_sys::core::PSTR
pub type PSTR = *mut u8;
/// `PCSTR`: a NUL-terminated byte string the call reads.
///
/// replaces windows_sys::core::PCSTR
pub type PCSTR = *const u8;
/// `HRESULT`: a COM result; negative is an error.
///
/// replaces windows_sys::core::HRESULT
pub type HRESULT = i32;

/// `GUID` (guiddef.h): 16 bytes, `Data1` to `Data4`.
///
/// replaces windows_sys::core::GUID
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(C)]
pub struct GUID {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

impl GUID {
    /// The GUID written `{xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx}`, as one 128-bit number in
    /// that order (windows-sys's `GUID::from_u128`).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn from_u128(value: u128) -> GUID {
        GUID {
            data1: (value >> 96) as u32,
            data2: (value >> 80 & 0xffff) as u16,
            data3: (value >> 64 & 0xffff) as u16,
            data4: (value as u64).to_be_bytes(),
        }
    }
}

/// A Win32 error code (winerror.h): the thread's last error, a Winsock error, or an `NTSTATUS`
/// mapped to one. A typed error, never a bare number in a `Result`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(transparent)]
pub struct WinError(pub u32);

impl WinError {
    /// The calling thread's last error (`GetLastError`), read right after the call that failed.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn last() -> WinError {
        // SAFETY: `GetLastError` reads the calling thread's own slot and takes nothing.
        WinError(unsafe { foundation::GetLastError() })
    }

    /// The calling thread's last Winsock error (`WSAGetLastError`).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn last_socket() -> WinError {
        // SAFETY: as `last`; `WSAGetLastError` reads the thread's Winsock error.
        WinError(unsafe { net::WSAGetLastError() } as u32)
    }

    /// The Win32 code an `NTSTATUS` means (`RtlNtStatusToDosError`; `ERROR_MR_MID_NOT_FOUND`,
    /// 317, for a status with no mapping, which is the refusal itself, not a stand-in).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn from_ntstatus(status: NTSTATUS) -> WinError {
        // SAFETY: a pure table lookup in ntdll; it takes a number and reads no memory.
        WinError(unsafe { foundation::RtlNtStatusToDosError(status) })
    }

    /// The code.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn code(self) -> u32 {
        self.0
    }
}

impl core::fmt::Display for WinError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Windows error {} ({:#x})", self.0, self.0)
    }
}

impl core::error::Error for WinError {}

// The layouts the declarations rest on (LLP64 on x86-64 and arm64 alike: the Win32 headers).
const _: () = {
    assert!(size_of::<Handle>() == size_of::<usize>());
    assert!(size_of::<Bool>() == 4 && align_of::<Bool>() == 4);
    assert!(size_of::<Socket>() == size_of::<usize>());
    assert!(size_of::<GUID>() == 16 && align_of::<GUID>() == 4);
};

crate::unbuilt! {
    "Windows":
    open_beneath, close, read_at, write_at, sync_barrier, sync_full, set_len, rename, remove,
    make_dir, stat, list, clone_file, reserve, commit, release, protect, park, unpark,
    completion_port, child_exit, clock, sleep_until, spawn, exit, random, socket, connect, bind,
    listen, accept, send, receive, shutdown, pipe,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unbuilt_operation_names_itself() {
        let refusal = park();
        assert_eq!((refusal.operation, refusal.target), ("park", "Windows"));
        assert!(UNBUILT.contains(&"park"));
        assert!(refusal.to_string().contains("`park` on Windows"));
    }

    /// WSAID_CONNECTEX (mswsock.h) is `{25a207b9-ddf3-4660-8ee9-76e58c74063e}`.
    #[test]
    fn a_guid_is_made_from_its_written_form() {
        let connect_ex = GUID::from_u128(0x25a207b9_ddf3_4660_8ee9_76e58c74063e);
        assert_eq!(
            (connect_ex.data1, connect_ex.data2, connect_ex.data3, connect_ex.data4),
            (0x25a2_07b9, 0xddf3, 0x4660, [0x8e, 0xe9, 0x76, 0xe5, 0x8c, 0x74, 0x06, 0x3e])
        );
    }
}
