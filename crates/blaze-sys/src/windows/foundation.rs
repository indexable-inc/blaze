//! `Win32::Foundation` and `Win32::System::SystemServices`: handles' lifetime, the thread's last
//! error, the Win32 error codes the tree matches, `FILETIME`, wait results, and the few
//! system-services constants. `HANDLE`, `INVALID_HANDLE_VALUE` and `NTSTATUS` are the root's
//! ([`Handle`], [`Handle::INVALID`], [`NTSTATUS`]).

use core::ffi::c_void;

use super::{Bool, Handle, NTSTATUS, WinError};

/// `WIN32_ERROR`: a Win32 error code (winerror.h).
///
/// replaces windows_sys::Win32::Foundation::WIN32_ERROR
pub type WIN32_ERROR = u32;
/// `DUPLICATE_HANDLE_OPTIONS`: `DuplicateHandle`'s options.
///
/// replaces windows_sys::Win32::Foundation::DUPLICATE_HANDLE_OPTIONS
pub type DUPLICATE_HANDLE_OPTIONS = u32;
/// `GENERIC_ACCESS_RIGHTS`: the generic access bits of an access mask (winnt.h).
///
/// replaces windows_sys::Win32::Foundation::GENERIC_ACCESS_RIGHTS
pub type GENERIC_ACCESS_RIGHTS = u32;
/// `WAIT_EVENT`: what a `WaitFor*` call returns.
///
/// replaces windows_sys::Win32::Foundation::WAIT_EVENT
pub type WAIT_EVENT = u32;
/// `HLOCAL`: a local-heap block, what `LocalFree` frees (a pointer, not a kernel handle).
///
/// replaces windows_sys::Win32::Foundation::HLOCAL
pub type HLOCAL = *mut c_void;
/// `HMODULE`: a loaded module's base address, what `GetModuleHandleW` returns (a pointer, not
/// a kernel handle).
///
/// replaces windows_sys::Win32::Foundation::HMODULE
pub type HMODULE = *mut c_void;
/// `FARPROC`: an export's address as `GetProcAddress` returns it; `None` when the module does
/// not export the name.
///
/// replaces windows_sys::Win32::Foundation::FARPROC
pub type FARPROC = Option<unsafe extern "system" fn() -> isize>;

/// `DUPLICATE_CLOSE_SOURCE`: close the source handle, even on failure.
///
/// replaces windows_sys::Win32::Foundation::DUPLICATE_CLOSE_SOURCE
pub const DUPLICATE_CLOSE_SOURCE: DUPLICATE_HANDLE_OPTIONS = 1u32;
/// `DUPLICATE_SAME_ACCESS`: the duplicate has the source's access.
///
/// replaces windows_sys::Win32::Foundation::DUPLICATE_SAME_ACCESS
pub const DUPLICATE_SAME_ACCESS: DUPLICATE_HANDLE_OPTIONS = 2u32;

/// `GENERIC_READ`.
///
/// replaces windows_sys::Win32::Foundation::GENERIC_READ
pub const GENERIC_READ: GENERIC_ACCESS_RIGHTS = 2147483648u32;
/// `GENERIC_WRITE`.
///
/// replaces windows_sys::Win32::Foundation::GENERIC_WRITE
pub const GENERIC_WRITE: GENERIC_ACCESS_RIGHTS = 1073741824u32;

/// `ERROR_SUCCESS` (0).
///
/// replaces windows_sys::Win32::Foundation::ERROR_SUCCESS
pub const ERROR_SUCCESS: WIN32_ERROR = 0u32;
/// `ERROR_INVALID_FUNCTION` (1).
///
/// replaces windows_sys::Win32::Foundation::ERROR_INVALID_FUNCTION
pub const ERROR_INVALID_FUNCTION: WIN32_ERROR = 1u32;
/// `ERROR_FILE_NOT_FOUND` (2).
///
/// replaces windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND
pub const ERROR_FILE_NOT_FOUND: WIN32_ERROR = 2u32;
/// `ERROR_PATH_NOT_FOUND` (3).
///
/// replaces windows_sys::Win32::Foundation::ERROR_PATH_NOT_FOUND
pub const ERROR_PATH_NOT_FOUND: WIN32_ERROR = 3u32;
/// `ERROR_ACCESS_DENIED` (5).
///
/// replaces windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED
pub const ERROR_ACCESS_DENIED: WIN32_ERROR = 5u32;
/// `ERROR_NO_MORE_FILES` (18).
///
/// replaces windows_sys::Win32::Foundation::ERROR_NO_MORE_FILES
pub const ERROR_NO_MORE_FILES: WIN32_ERROR = 18u32;
/// `ERROR_HANDLE_EOF` (38).
///
/// replaces windows_sys::Win32::Foundation::ERROR_HANDLE_EOF
pub const ERROR_HANDLE_EOF: WIN32_ERROR = 38u32;
/// `ERROR_NETNAME_DELETED` (64).
///
/// replaces windows_sys::Win32::Foundation::ERROR_NETNAME_DELETED
pub const ERROR_NETNAME_DELETED: WIN32_ERROR = 64u32;
/// `ERROR_INVALID_PARAMETER` (87).
///
/// replaces windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER
pub const ERROR_INVALID_PARAMETER: WIN32_ERROR = 87u32;
/// `ERROR_BROKEN_PIPE` (109).
///
/// replaces windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE
pub const ERROR_BROKEN_PIPE: WIN32_ERROR = 109u32;
/// `ERROR_INSUFFICIENT_BUFFER` (122).
///
/// replaces windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER
pub const ERROR_INSUFFICIENT_BUFFER: WIN32_ERROR = 122u32;
/// `ERROR_INVALID_NAME` (123).
///
/// replaces windows_sys::Win32::Foundation::ERROR_INVALID_NAME
pub const ERROR_INVALID_NAME: WIN32_ERROR = 123u32;
/// `ERROR_BAD_PATHNAME` (161).
///
/// replaces windows_sys::Win32::Foundation::ERROR_BAD_PATHNAME
pub const ERROR_BAD_PATHNAME: WIN32_ERROR = 161u32;
/// `ERROR_ALREADY_EXISTS` (183).
///
/// replaces windows_sys::Win32::Foundation::ERROR_ALREADY_EXISTS
pub const ERROR_ALREADY_EXISTS: WIN32_ERROR = 183u32;
/// `ERROR_NO_DATA` (232).
///
/// replaces windows_sys::Win32::Foundation::ERROR_NO_DATA
pub const ERROR_NO_DATA: WIN32_ERROR = 232u32;
/// `ERROR_PIPE_BUSY` (231): every instance of the named pipe is busy.
///
/// replaces windows_sys::Win32::Foundation::ERROR_PIPE_BUSY
pub const ERROR_PIPE_BUSY: WIN32_ERROR = 231u32;
/// `ERROR_PIPE_NOT_CONNECTED` (233): no process is on the other end of the pipe.
///
/// replaces windows_sys::Win32::Foundation::ERROR_PIPE_NOT_CONNECTED
pub const ERROR_PIPE_NOT_CONNECTED: WIN32_ERROR = 233u32;
/// `ERROR_PIPE_CONNECTED` (535): a client connected before `ConnectNamedPipe` was called.
///
/// replaces windows_sys::Win32::Foundation::ERROR_PIPE_CONNECTED
pub const ERROR_PIPE_CONNECTED: WIN32_ERROR = 535u32;
/// `ERROR_TIMEOUT` (1460): what `WaitOnAddress` leaves when its time ran out.
///
/// replaces windows_sys::Win32::Foundation::ERROR_TIMEOUT
pub const ERROR_TIMEOUT: WIN32_ERROR = 1460u32;
/// `ERROR_DIRECTORY` (267).
///
/// replaces windows_sys::Win32::Foundation::ERROR_DIRECTORY
pub const ERROR_DIRECTORY: WIN32_ERROR = 267u32;
/// `ERROR_DELETE_PENDING` (303).
///
/// replaces windows_sys::Win32::Foundation::ERROR_DELETE_PENDING
pub const ERROR_DELETE_PENDING: WIN32_ERROR = 303u32;
/// `ERROR_OPERATION_ABORTED` (995).
///
/// replaces windows_sys::Win32::Foundation::ERROR_OPERATION_ABORTED
pub const ERROR_OPERATION_ABORTED: WIN32_ERROR = 995u32;
/// `ERROR_IO_PENDING` (997).
///
/// replaces windows_sys::Win32::Foundation::ERROR_IO_PENDING
pub const ERROR_IO_PENDING: WIN32_ERROR = 997u32;
/// `ERROR_NOTIFY_ENUM_DIR` (1022).
///
/// replaces windows_sys::Win32::Foundation::ERROR_NOTIFY_ENUM_DIR
pub const ERROR_NOTIFY_ENUM_DIR: WIN32_ERROR = 1022u32;
/// `ERROR_JOURNAL_DELETE_IN_PROGRESS` (1178).
///
/// replaces windows_sys::Win32::Foundation::ERROR_JOURNAL_DELETE_IN_PROGRESS
pub const ERROR_JOURNAL_DELETE_IN_PROGRESS: WIN32_ERROR = 1178u32;
/// `ERROR_JOURNAL_NOT_ACTIVE` (1179).
///
/// replaces windows_sys::Win32::Foundation::ERROR_JOURNAL_NOT_ACTIVE
pub const ERROR_JOURNAL_NOT_ACTIVE: WIN32_ERROR = 1179u32;
/// `ERROR_JOURNAL_ENTRY_DELETED` (1181).
///
/// replaces windows_sys::Win32::Foundation::ERROR_JOURNAL_ENTRY_DELETED
pub const ERROR_JOURNAL_ENTRY_DELETED: WIN32_ERROR = 1181u32;
/// `ERROR_CONNECTION_ABORTED` (1236).
///
/// replaces windows_sys::Win32::Foundation::ERROR_CONNECTION_ABORTED
pub const ERROR_CONNECTION_ABORTED: WIN32_ERROR = 1236u32;
/// `ERROR_CANT_ACCESS_FILE` (1920).
///
/// replaces windows_sys::Win32::Foundation::ERROR_CANT_ACCESS_FILE
pub const ERROR_CANT_ACCESS_FILE: WIN32_ERROR = 1920u32;

/// `STATUS_COMMITMENT_LIMIT` (`0xC000012D`): the system's commit limit was reached.
///
/// replaces windows_sys::Win32::Foundation::STATUS_COMMITMENT_LIMIT
pub const STATUS_COMMITMENT_LIMIT: NTSTATUS = 0xC000012D_u32 as _;

/// `WAIT_OBJECT_0` (0): the object is signaled.
///
/// replaces windows_sys::Win32::Foundation::WAIT_OBJECT_0
pub const WAIT_OBJECT_0: WAIT_EVENT = 0u32;
/// `WAIT_ABANDONED` (0x80): a mutex whose owner exited without releasing it.
///
/// replaces windows_sys::Win32::Foundation::WAIT_ABANDONED
pub const WAIT_ABANDONED: WAIT_EVENT = 128u32;
/// `WAIT_TIMEOUT` (258): the time ran out first.
///
/// replaces windows_sys::Win32::Foundation::WAIT_TIMEOUT
pub const WAIT_TIMEOUT: WAIT_EVENT = 258u32;
/// `WAIT_FAILED` (`0xFFFFFFFF`): see `GetLastError`.
///
/// replaces windows_sys::Win32::Foundation::WAIT_FAILED
pub const WAIT_FAILED: WAIT_EVENT = 4294967295u32;

/// `FILE_SUPPORTS_BLOCK_REFCOUNTING`: a volume flag (`GetVolumeInformationByHandleW`): block
/// cloning works (ReFS, Dev Drive).
///
/// replaces windows_sys::Win32::System::SystemServices::FILE_SUPPORTS_BLOCK_REFCOUNTING
pub const FILE_SUPPORTS_BLOCK_REFCOUNTING: u32 = 134217728u32;
/// `JOB_OBJECT_MSG_JOB_MEMORY_LIMIT` (10): a job's port message, the job hit its memory limit.
///
/// replaces windows_sys::Win32::System::SystemServices::JOB_OBJECT_MSG_JOB_MEMORY_LIMIT
pub const JOB_OBJECT_MSG_JOB_MEMORY_LIMIT: u32 = 10u32;
/// `SECURITY_DESCRIPTOR_REVISION` (1): what `InitializeSecurityDescriptor` takes.
///
/// replaces windows_sys::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION
pub const SECURITY_DESCRIPTOR_REVISION: u32 = 1u32;
/// `SYSTEM_MANDATORY_LABEL_NO_WRITE_UP` (1): a label ACE's policy: a process of a lower
/// integrity level may not write the object.
///
/// replaces windows_sys::Win32::System::SystemServices::SYSTEM_MANDATORY_LABEL_NO_WRITE_UP
pub const SYSTEM_MANDATORY_LABEL_NO_WRITE_UP: u32 = 1u32;

/// `FILETIME` (minwindef.h): 100-nanosecond intervals since 1601-01-01 UTC, in two halves.
///
/// replaces windows_sys::Win32::Foundation::FILETIME
#[derive(Clone, Copy)]
#[repr(C)]
pub struct FILETIME {
    pub dwLowDateTime: u32,
    pub dwHighDateTime: u32,
}

// minwindef.h, x86-64 and arm64 Windows alike: two `DWORD`s.
const _: () = {
    assert!(size_of::<FILETIME>() == 8);
    assert!(align_of::<FILETIME>() == 4);
    assert!(core::mem::offset_of!(FILETIME, dwLowDateTime) == 0);
    assert!(core::mem::offset_of!(FILETIME, dwHighDateTime) == 4);
};

#[link(name = "kernel32", kind = "raw-dylib")]
unsafe extern "system" {
    /// `CloseHandle` (handleapi.h).
    ///
    /// replaces windows_sys::Win32::Foundation::CloseHandle
    pub fn CloseHandle(hobject: Handle) -> Bool;
    /// `DuplicateHandle` (handleapi.h).
    ///
    /// replaces windows_sys::Win32::Foundation::DuplicateHandle
    pub fn DuplicateHandle(
        hsourceprocesshandle: Handle,
        hsourcehandle: Handle,
        htargetprocesshandle: Handle,
        lptargethandle: *mut Handle,
        dwdesiredaccess: u32,
        binherithandle: Bool,
        dwoptions: DUPLICATE_HANDLE_OPTIONS,
    ) -> Bool;
    /// `GetLastError` (errhandlingapi.h).
    ///
    /// replaces windows_sys::Win32::Foundation::GetLastError
    pub fn GetLastError() -> WIN32_ERROR;
    /// `LocalFree` (winbase.h): frees what a call allocated for the caller (a security
    /// descriptor from `GetSecurityInfo`); null on success.
    ///
    /// replaces windows_sys::Win32::Foundation::LocalFree
    pub fn LocalFree(hmem: HLOCAL) -> HLOCAL;
    /// `SetLastError` (errhandlingapi.h): sets the calling thread's last error.
    ///
    /// replaces windows_sys::Win32::Foundation::SetLastError
    pub fn SetLastError(dwerrcode: WIN32_ERROR);
}

#[link(name = "ntdll", kind = "raw-dylib")]
unsafe extern "system" {
    /// `RtlNtStatusToDosError` (winternl.h): the Win32 code an `NTSTATUS` means.
    ///
    /// replaces windows_sys::Win32::Foundation::RtlNtStatusToDosError
    pub fn RtlNtStatusToDosError(status: NTSTATUS) -> u32;
}

/// `CloseHandle`: closes `handle`; the handle is gone afterwards whatever the result.
///
/// # Safety
///
/// The caller owns `handle` (no other owner closes or uses it after this), and it is not a
/// pseudo-handle another owner relies on: closing a handle that is not yours closes someone
/// else's object, or one the number was reused for.
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn close_handle(handle: Handle) -> Result<(), WinError> {
    // SAFETY: the caller owns `handle` (this function's contract); the call takes a number and
    // reads no memory of ours.
    unsafe { CloseHandle(handle) }.ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// winerror.h's values for the codes the IOCP driver tells apart.
    #[test]
    fn the_codes_are_winerror_h_s() {
        assert_eq!((ERROR_IO_PENDING, ERROR_OPERATION_ABORTED, ERROR_HANDLE_EOF), (997, 995, 38));
        assert_eq!(STATUS_COMMITMENT_LIMIT as u32, 0xC000_012D);
        assert_eq!(WAIT_FAILED, u32::MAX);
    }
}
