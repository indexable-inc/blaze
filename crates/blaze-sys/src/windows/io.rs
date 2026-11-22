//! Overlapped IO, completion ports, device controls and pipes: `Win32::System::IO`,
//! `Win32::System::Ioctl` and `Win32::System::Pipes`, declared from the Win32 API reference
//! (ioapiset.h, minwinbase.h, winioctl.h, winbase.h) as windows-sys 0.61.2 has them.
//!
//! A port's wrappers are below the declarations; a device control's buffers are the caller's,
//! so `DeviceIoControl` is used through its declaration.

use core::ffi::c_void;

use super::fs::{FILE_FLAGS_AND_ATTRIBUTES, FILE_ID_128};
use super::security::SECURITY_ATTRIBUTES;
use super::{Bool, Handle, PCWSTR, WinError};

// ---- Win32::System::IO -----------------------------------------------------------------------

/// `LPOVERLAPPED_COMPLETION_ROUTINE` (minwinbase.h): the routine an alertable wait runs when
/// the operation completes; `None` when the completion goes to a port.
/// replaces windows_sys::Win32::System::IO::LPOVERLAPPED_COMPLETION_ROUTINE
pub type LPOVERLAPPED_COMPLETION_ROUTINE =
    Option<unsafe extern "system" fn(dwerrorcode: u32, dwnumberofbytestransfered: u32, lpoverlapped: *mut OVERLAPPED)>;

/// `OVERLAPPED` (minwinbase.h): one asynchronous operation's state, which the kernel writes
/// until the operation completes; its address is the operation's identity on the port.
/// replaces windows_sys::Win32::System::IO::OVERLAPPED
#[repr(C)]
#[derive(Clone, Copy)]
pub struct OVERLAPPED {
    pub Internal: usize,
    pub InternalHigh: usize,
    pub Anonymous: OVERLAPPED_0,
    pub hEvent: Handle,
}

impl Default for OVERLAPPED {
    fn default() -> Self {
        OVERLAPPED { Internal: 0, InternalHigh: 0, Anonymous: OVERLAPPED_0::default(), hEvent: Handle::NULL }
    }
}

/// `OVERLAPPED`'s unnamed union (minwinbase.h): the file offset, or a pointer.
/// replaces windows_sys::Win32::System::IO::OVERLAPPED_0
#[repr(C)]
#[derive(Clone, Copy)]
pub union OVERLAPPED_0 {
    pub Anonymous: OVERLAPPED_0_0,
    pub Pointer: *mut c_void,
}

impl Default for OVERLAPPED_0 {
    fn default() -> Self {
        // The pointer is the widest member, so all 8 bytes are zero.
        OVERLAPPED_0 { Pointer: core::ptr::null_mut() }
    }
}

/// `OVERLAPPED`'s offset (minwinbase.h): its low and high 32 bits.
/// replaces windows_sys::Win32::System::IO::OVERLAPPED_0_0
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct OVERLAPPED_0_0 {
    pub Offset: u32,
    pub OffsetHigh: u32,
}

/// `OVERLAPPED_ENTRY` (minwinbase.h): one completion `GetQueuedCompletionStatusEx` removed.
/// replaces windows_sys::Win32::System::IO::OVERLAPPED_ENTRY
#[repr(C)]
#[derive(Clone, Copy)]
pub struct OVERLAPPED_ENTRY {
    pub lpCompletionKey: usize,
    pub lpOverlapped: *mut OVERLAPPED,
    pub Internal: usize,
    pub dwNumberOfBytesTransferred: u32,
}

impl Default for OVERLAPPED_ENTRY {
    fn default() -> Self {
        OVERLAPPED_ENTRY {
            lpCompletionKey: 0,
            lpOverlapped: core::ptr::null_mut(),
            Internal: 0,
            dwNumberOfBytesTransferred: 0,
        }
    }
}

#[link(name = "kernel32", kind = "raw-dylib")]
unsafe extern "system" {
    /// `CancelIoEx` (ioapiset.h): cancels the operation `lpoverlapped` names, or all of the
    /// handle's when it is null; the cancelled operation still completes on the port.
    /// replaces windows_sys::Win32::System::IO::CancelIoEx
    pub fn CancelIoEx(hfile: Handle, lpoverlapped: *const OVERLAPPED) -> Bool;

    /// `CreateIoCompletionPort` (ioapiset.h): a new port when `existingcompletionport` is
    /// [`Handle::NULL`] (and `filehandle` [`Handle::INVALID`]), else the file bound to it;
    /// [`Handle::NULL`] on failure.
    /// replaces windows_sys::Win32::System::IO::CreateIoCompletionPort
    pub fn CreateIoCompletionPort(
        filehandle: Handle,
        existingcompletionport: Handle,
        completionkey: usize,
        numberofconcurrentthreads: u32,
    ) -> Handle;

    /// `DeviceIoControl` (ioapiset.h).
    /// replaces windows_sys::Win32::System::IO::DeviceIoControl
    pub fn DeviceIoControl(
        hdevice: Handle,
        dwiocontrolcode: u32,
        lpinbuffer: *const c_void,
        ninbuffersize: u32,
        lpoutbuffer: *mut c_void,
        noutbuffersize: u32,
        lpbytesreturned: *mut u32,
        lpoverlapped: *mut OVERLAPPED,
    ) -> Bool;

    /// `GetOverlappedResult` (ioapiset.h): how the operation `lpoverlapped` names ended and the
    /// bytes it moved; with `bwait` true it waits until the operation is over.
    /// replaces windows_sys::Win32::System::IO::GetOverlappedResult
    pub fn GetOverlappedResult(
        hfile: Handle,
        lpoverlapped: *const OVERLAPPED,
        lpnumberofbytestransferred: *mut u32,
        bwait: Bool,
    ) -> Bool;

    /// `GetQueuedCompletionStatusEx` (ioapiset.h): false with `WAIT_TIMEOUT` when none came.
    /// replaces windows_sys::Win32::System::IO::GetQueuedCompletionStatusEx
    pub fn GetQueuedCompletionStatusEx(
        completionport: Handle,
        lpcompletionportentries: *mut OVERLAPPED_ENTRY,
        ulcount: u32,
        ulnumentriesremoved: *mut u32,
        dwmilliseconds: u32,
        falertable: Bool,
    ) -> Bool;

    /// `PostQueuedCompletionStatus` (ioapiset.h): queues a completion the kernel never reads
    /// through; the receiver gets the three values back.
    /// replaces windows_sys::Win32::System::IO::PostQueuedCompletionStatus
    pub fn PostQueuedCompletionStatus(
        completionport: Handle,
        dwnumberofbytestransferred: u32,
        dwcompletionkey: usize,
        lpoverlapped: *const OVERLAPPED,
    ) -> Bool;
}

// ---- Win32::System::Ioctl --------------------------------------------------------------------

/// `FSCTL_QUERY_USN_JOURNAL` (winioctl.h): out `USN_JOURNAL_DATA_V1`.
/// replaces windows_sys::Win32::System::Ioctl::FSCTL_QUERY_USN_JOURNAL
pub const FSCTL_QUERY_USN_JOURNAL: u32 = 590068u32;
/// `FSCTL_GET_INTEGRITY_INFORMATION` (winioctl.h): out `FSCTL_GET_INTEGRITY_INFORMATION_BUFFER`.
/// replaces windows_sys::Win32::System::Ioctl::FSCTL_GET_INTEGRITY_INFORMATION
pub const FSCTL_GET_INTEGRITY_INFORMATION: u32 = 590460u32;
/// `FSCTL_READ_UNPRIVILEGED_USN_JOURNAL` (winioctl.h): in `READ_USN_JOURNAL_DATA_V1`, out a
/// next USN then records.
/// replaces windows_sys::Win32::System::Ioctl::FSCTL_READ_UNPRIVILEGED_USN_JOURNAL
pub const FSCTL_READ_UNPRIVILEGED_USN_JOURNAL: u32 = 590763u32;
/// `FSCTL_DUPLICATE_EXTENTS_TO_FILE` (winioctl.h): in `DUPLICATE_EXTENTS_DATA` (block clone,
/// ReFS and Dev Drive).
/// replaces windows_sys::Win32::System::Ioctl::FSCTL_DUPLICATE_EXTENTS_TO_FILE
pub const FSCTL_DUPLICATE_EXTENTS_TO_FILE: u32 = 623428u32;
/// `FSCTL_SET_INTEGRITY_INFORMATION` (winioctl.h): in `FSCTL_SET_INTEGRITY_INFORMATION_BUFFER`.
/// replaces windows_sys::Win32::System::Ioctl::FSCTL_SET_INTEGRITY_INFORMATION
pub const FSCTL_SET_INTEGRITY_INFORMATION: u32 = 639616u32;

/// `USN_REASON_DATA_OVERWRITE` (winioctl.h).
/// replaces windows_sys::Win32::System::Ioctl::USN_REASON_DATA_OVERWRITE
pub const USN_REASON_DATA_OVERWRITE: u32 = 1u32;
/// `USN_REASON_DATA_EXTEND` (winioctl.h).
/// replaces windows_sys::Win32::System::Ioctl::USN_REASON_DATA_EXTEND
pub const USN_REASON_DATA_EXTEND: u32 = 2u32;
/// `USN_REASON_DATA_TRUNCATION` (winioctl.h).
/// replaces windows_sys::Win32::System::Ioctl::USN_REASON_DATA_TRUNCATION
pub const USN_REASON_DATA_TRUNCATION: u32 = 4u32;
/// `USN_REASON_NAMED_DATA_OVERWRITE` (winioctl.h).
/// replaces windows_sys::Win32::System::Ioctl::USN_REASON_NAMED_DATA_OVERWRITE
pub const USN_REASON_NAMED_DATA_OVERWRITE: u32 = 16u32;
/// `USN_REASON_NAMED_DATA_EXTEND` (winioctl.h).
/// replaces windows_sys::Win32::System::Ioctl::USN_REASON_NAMED_DATA_EXTEND
pub const USN_REASON_NAMED_DATA_EXTEND: u32 = 32u32;
/// `USN_REASON_NAMED_DATA_TRUNCATION` (winioctl.h).
/// replaces windows_sys::Win32::System::Ioctl::USN_REASON_NAMED_DATA_TRUNCATION
pub const USN_REASON_NAMED_DATA_TRUNCATION: u32 = 64u32;
/// `USN_REASON_FILE_CREATE` (winioctl.h).
/// replaces windows_sys::Win32::System::Ioctl::USN_REASON_FILE_CREATE
pub const USN_REASON_FILE_CREATE: u32 = 256u32;
/// `USN_REASON_FILE_DELETE` (winioctl.h).
/// replaces windows_sys::Win32::System::Ioctl::USN_REASON_FILE_DELETE
pub const USN_REASON_FILE_DELETE: u32 = 512u32;
/// `USN_REASON_EA_CHANGE` (winioctl.h).
/// replaces windows_sys::Win32::System::Ioctl::USN_REASON_EA_CHANGE
pub const USN_REASON_EA_CHANGE: u32 = 1024u32;
/// `USN_REASON_SECURITY_CHANGE` (winioctl.h).
/// replaces windows_sys::Win32::System::Ioctl::USN_REASON_SECURITY_CHANGE
pub const USN_REASON_SECURITY_CHANGE: u32 = 2048u32;
/// `USN_REASON_RENAME_OLD_NAME` (winioctl.h).
/// replaces windows_sys::Win32::System::Ioctl::USN_REASON_RENAME_OLD_NAME
pub const USN_REASON_RENAME_OLD_NAME: u32 = 4096u32;
/// `USN_REASON_RENAME_NEW_NAME` (winioctl.h).
/// replaces windows_sys::Win32::System::Ioctl::USN_REASON_RENAME_NEW_NAME
pub const USN_REASON_RENAME_NEW_NAME: u32 = 8192u32;
/// `USN_REASON_INDEXABLE_CHANGE` (winioctl.h).
/// replaces windows_sys::Win32::System::Ioctl::USN_REASON_INDEXABLE_CHANGE
pub const USN_REASON_INDEXABLE_CHANGE: u32 = 16384u32;
/// `USN_REASON_BASIC_INFO_CHANGE` (winioctl.h).
/// replaces windows_sys::Win32::System::Ioctl::USN_REASON_BASIC_INFO_CHANGE
pub const USN_REASON_BASIC_INFO_CHANGE: u32 = 32768u32;
/// `USN_REASON_HARD_LINK_CHANGE` (winioctl.h).
/// replaces windows_sys::Win32::System::Ioctl::USN_REASON_HARD_LINK_CHANGE
pub const USN_REASON_HARD_LINK_CHANGE: u32 = 65536u32;
/// `USN_REASON_COMPRESSION_CHANGE` (winioctl.h).
/// replaces windows_sys::Win32::System::Ioctl::USN_REASON_COMPRESSION_CHANGE
pub const USN_REASON_COMPRESSION_CHANGE: u32 = 131072u32;
/// `USN_REASON_ENCRYPTION_CHANGE` (winioctl.h).
/// replaces windows_sys::Win32::System::Ioctl::USN_REASON_ENCRYPTION_CHANGE
pub const USN_REASON_ENCRYPTION_CHANGE: u32 = 262144u32;
/// `USN_REASON_REPARSE_POINT_CHANGE` (winioctl.h).
/// replaces windows_sys::Win32::System::Ioctl::USN_REASON_REPARSE_POINT_CHANGE
pub const USN_REASON_REPARSE_POINT_CHANGE: u32 = 1048576u32;
/// `USN_REASON_STREAM_CHANGE` (winioctl.h).
/// replaces windows_sys::Win32::System::Ioctl::USN_REASON_STREAM_CHANGE
pub const USN_REASON_STREAM_CHANGE: u32 = 2097152u32;
/// `USN_REASON_INTEGRITY_CHANGE` (winioctl.h).
/// replaces windows_sys::Win32::System::Ioctl::USN_REASON_INTEGRITY_CHANGE
pub const USN_REASON_INTEGRITY_CHANGE: u32 = 8388608u32;
/// `USN_REASON_CLOSE` (winioctl.h).
/// replaces windows_sys::Win32::System::Ioctl::USN_REASON_CLOSE
pub const USN_REASON_CLOSE: u32 = 2147483648u32;

/// `DUPLICATE_EXTENTS_DATA` (winioctl.h): clone `ByteCount` bytes of `FileHandle` at
/// `SourceFileOffset` into the target file at `TargetFileOffset`.
/// replaces windows_sys::Win32::System::Ioctl::DUPLICATE_EXTENTS_DATA
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DUPLICATE_EXTENTS_DATA {
    pub FileHandle: Handle,
    pub SourceFileOffset: i64,
    pub TargetFileOffset: i64,
    pub ByteCount: i64,
}

impl Default for DUPLICATE_EXTENTS_DATA {
    fn default() -> Self {
        DUPLICATE_EXTENTS_DATA { FileHandle: Handle::NULL, SourceFileOffset: 0, TargetFileOffset: 0, ByteCount: 0 }
    }
}

/// `FSCTL_GET_INTEGRITY_INFORMATION_BUFFER` (winioctl.h).
/// replaces windows_sys::Win32::System::Ioctl::FSCTL_GET_INTEGRITY_INFORMATION_BUFFER
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FSCTL_GET_INTEGRITY_INFORMATION_BUFFER {
    pub ChecksumAlgorithm: u16,
    pub Reserved: u16,
    pub Flags: u32,
    pub ChecksumChunkSizeInBytes: u32,
    pub ClusterSizeInBytes: u32,
}

/// `FSCTL_SET_INTEGRITY_INFORMATION_BUFFER` (winioctl.h).
/// replaces windows_sys::Win32::System::Ioctl::FSCTL_SET_INTEGRITY_INFORMATION_BUFFER
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FSCTL_SET_INTEGRITY_INFORMATION_BUFFER {
    pub ChecksumAlgorithm: u16,
    pub Reserved: u16,
    pub Flags: u32,
}

/// `READ_USN_JOURNAL_DATA_V1` (winioctl.h): which records a journal read returns.
/// replaces windows_sys::Win32::System::Ioctl::READ_USN_JOURNAL_DATA_V1
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct READ_USN_JOURNAL_DATA_V1 {
    pub StartUsn: i64,
    pub ReasonMask: u32,
    pub ReturnOnlyOnClose: u32,
    pub Timeout: u64,
    pub BytesToWaitFor: u64,
    pub UsnJournalID: u64,
    pub MinMajorVersion: u16,
    pub MaxMajorVersion: u16,
}

/// `USN_JOURNAL_DATA_V1` (winioctl.h): a volume's change journal.
/// replaces windows_sys::Win32::System::Ioctl::USN_JOURNAL_DATA_V1
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct USN_JOURNAL_DATA_V1 {
    pub UsnJournalID: u64,
    pub FirstUsn: i64,
    pub NextUsn: i64,
    pub LowestValidUsn: i64,
    pub MaxUsn: i64,
    pub MaximumSize: u64,
    pub AllocationDelta: u64,
    pub MinSupportedMajorVersion: u16,
    pub MaxSupportedMajorVersion: u16,
}

/// `USN_RECORD_V2` (winioctl.h): one change, 64-bit file references; the name is
/// `FileNameLength` bytes at `FileNameOffset` from the record's start.
/// replaces windows_sys::Win32::System::Ioctl::USN_RECORD_V2
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct USN_RECORD_V2 {
    pub RecordLength: u32,
    pub MajorVersion: u16,
    pub MinorVersion: u16,
    pub FileReferenceNumber: u64,
    pub ParentFileReferenceNumber: u64,
    pub Usn: i64,
    pub TimeStamp: i64,
    pub Reason: u32,
    pub SourceInfo: u32,
    pub SecurityId: u32,
    pub FileAttributes: u32,
    pub FileNameLength: u16,
    pub FileNameOffset: u16,
    pub FileName: [u16; 1],
}

/// `USN_RECORD_V3` (winioctl.h): as `USN_RECORD_V2` with 128-bit file references.
/// replaces windows_sys::Win32::System::Ioctl::USN_RECORD_V3
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct USN_RECORD_V3 {
    pub RecordLength: u32,
    pub MajorVersion: u16,
    pub MinorVersion: u16,
    pub FileReferenceNumber: FILE_ID_128,
    pub ParentFileReferenceNumber: FILE_ID_128,
    pub Usn: i64,
    pub TimeStamp: i64,
    pub Reason: u32,
    pub SourceInfo: u32,
    pub SecurityId: u32,
    pub FileAttributes: u32,
    pub FileNameLength: u16,
    pub FileNameOffset: u16,
    pub FileName: [u16; 1],
}

// ---- Win32::System::Pipes --------------------------------------------------------------------

/// replaces windows_sys::Win32::System::Pipes::NAMED_PIPE_MODE
pub type NAMED_PIPE_MODE = u32;

/// `PIPE_WAIT` (winbase.h): blocking mode (0).
/// replaces windows_sys::Win32::System::Pipes::PIPE_WAIT
pub const PIPE_WAIT: NAMED_PIPE_MODE = 0u32;
/// `PIPE_READMODE_BYTE` (winbase.h): read as a byte stream (0).
/// replaces windows_sys::Win32::System::Pipes::PIPE_READMODE_BYTE
pub const PIPE_READMODE_BYTE: NAMED_PIPE_MODE = 0u32;
/// `PIPE_TYPE_BYTE` (winbase.h): written as a byte stream (0).
/// replaces windows_sys::Win32::System::Pipes::PIPE_TYPE_BYTE
pub const PIPE_TYPE_BYTE: NAMED_PIPE_MODE = 0u32;
/// `PIPE_REJECT_REMOTE_CLIENTS` (winbase.h).
/// replaces windows_sys::Win32::System::Pipes::PIPE_REJECT_REMOTE_CLIENTS
pub const PIPE_REJECT_REMOTE_CLIENTS: NAMED_PIPE_MODE = 8u32;
/// `PIPE_UNLIMITED_INSTANCES` (winbase.h): as many instances as the system allows (255).
/// replaces windows_sys::Win32::System::Pipes::PIPE_UNLIMITED_INSTANCES
pub const PIPE_UNLIMITED_INSTANCES: u32 = 255u32;

#[link(name = "kernel32", kind = "raw-dylib")]
unsafe extern "system" {
    /// `ConnectNamedPipe` (namedpipeapi.h): waits for a client on an instance; overlapped, false
    /// with `ERROR_IO_PENDING` while it waits, or `ERROR_PIPE_CONNECTED` when one came first.
    /// replaces windows_sys::Win32::System::Pipes::ConnectNamedPipe
    pub fn ConnectNamedPipe(hnamedpipe: Handle, lpoverlapped: *mut OVERLAPPED) -> Bool;

    /// `GetNamedPipeClientProcessId` (winbase.h): the process ID of the instance's client.
    /// replaces windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId
    pub fn GetNamedPipeClientProcessId(pipe: Handle, clientprocessid: *mut u32) -> Bool;

    /// `PeekNamedPipe` (namedpipeapi.h): copies without removing; every pointer may be null.
    /// replaces windows_sys::Win32::System::Pipes::PeekNamedPipe
    pub fn PeekNamedPipe(
        hnamedpipe: Handle,
        lpbuffer: *mut c_void,
        nbuffersize: u32,
        lpbytesread: *mut u32,
        lptotalbytesavail: *mut u32,
        lpbytesleftthismessage: *mut u32,
    ) -> Bool;

    /// `WaitNamedPipeW` (namedpipeapi.h): waits until an instance of the pipe is free.
    /// replaces windows_sys::Win32::System::Pipes::WaitNamedPipeW
    pub fn WaitNamedPipeW(lpnamedpipename: PCWSTR, ntimeout: u32) -> Bool;

    /// `CreateNamedPipeW` (winbase.h): [`Handle::INVALID`] on failure.
    /// replaces windows_sys::Win32::System::Pipes::CreateNamedPipeW
    pub fn CreateNamedPipeW(
        lpname: PCWSTR,
        dwopenmode: FILE_FLAGS_AND_ATTRIBUTES,
        dwpipemode: NAMED_PIPE_MODE,
        nmaxinstances: u32,
        noutbuffersize: u32,
        ninbuffersize: u32,
        ndefaulttimeout: u32,
        lpsecurityattributes: *const SECURITY_ATTRIBUTES,
    ) -> Handle;
}

// ---- Layouts ---------------------------------------------------------------------------------

// Win32 API: minwinbase.h (OVERLAPPED, OVERLAPPED_ENTRY) and winioctl.h (the rest). LLP64:
// x86-64 and arm64 Windows lay these out alike.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const _: () = {
    use core::mem::offset_of;

    assert!(size_of::<OVERLAPPED_0_0>() == 8);
    assert!(align_of::<OVERLAPPED_0_0>() == 4);
    assert!(offset_of!(OVERLAPPED_0_0, OffsetHigh) == 4);
    assert!(size_of::<OVERLAPPED_0>() == 8);
    assert!(align_of::<OVERLAPPED_0>() == 8);
    assert!(size_of::<OVERLAPPED>() == 32);
    assert!(align_of::<OVERLAPPED>() == 8);
    assert!(offset_of!(OVERLAPPED, Internal) == 0);
    assert!(offset_of!(OVERLAPPED, InternalHigh) == 8);
    assert!(offset_of!(OVERLAPPED, Anonymous) == 16);
    assert!(offset_of!(OVERLAPPED, hEvent) == 24);

    assert!(size_of::<OVERLAPPED_ENTRY>() == 32);
    assert!(align_of::<OVERLAPPED_ENTRY>() == 8);
    assert!(offset_of!(OVERLAPPED_ENTRY, lpCompletionKey) == 0);
    assert!(offset_of!(OVERLAPPED_ENTRY, lpOverlapped) == 8);
    assert!(offset_of!(OVERLAPPED_ENTRY, Internal) == 16);
    assert!(offset_of!(OVERLAPPED_ENTRY, dwNumberOfBytesTransferred) == 24);

    assert!(size_of::<DUPLICATE_EXTENTS_DATA>() == 32);
    assert!(align_of::<DUPLICATE_EXTENTS_DATA>() == 8);
    assert!(offset_of!(DUPLICATE_EXTENTS_DATA, SourceFileOffset) == 8);
    assert!(offset_of!(DUPLICATE_EXTENTS_DATA, TargetFileOffset) == 16);
    assert!(offset_of!(DUPLICATE_EXTENTS_DATA, ByteCount) == 24);

    assert!(size_of::<FSCTL_GET_INTEGRITY_INFORMATION_BUFFER>() == 16);
    assert!(align_of::<FSCTL_GET_INTEGRITY_INFORMATION_BUFFER>() == 4);
    assert!(offset_of!(FSCTL_GET_INTEGRITY_INFORMATION_BUFFER, Flags) == 4);
    assert!(offset_of!(FSCTL_GET_INTEGRITY_INFORMATION_BUFFER, ChecksumChunkSizeInBytes) == 8);
    assert!(offset_of!(FSCTL_GET_INTEGRITY_INFORMATION_BUFFER, ClusterSizeInBytes) == 12);
    assert!(size_of::<FSCTL_SET_INTEGRITY_INFORMATION_BUFFER>() == 8);
    assert!(align_of::<FSCTL_SET_INTEGRITY_INFORMATION_BUFFER>() == 4);
    assert!(offset_of!(FSCTL_SET_INTEGRITY_INFORMATION_BUFFER, Flags) == 4);

    assert!(size_of::<READ_USN_JOURNAL_DATA_V1>() == 48);
    assert!(align_of::<READ_USN_JOURNAL_DATA_V1>() == 8);
    assert!(offset_of!(READ_USN_JOURNAL_DATA_V1, ReasonMask) == 8);
    assert!(offset_of!(READ_USN_JOURNAL_DATA_V1, ReturnOnlyOnClose) == 12);
    assert!(offset_of!(READ_USN_JOURNAL_DATA_V1, Timeout) == 16);
    assert!(offset_of!(READ_USN_JOURNAL_DATA_V1, BytesToWaitFor) == 24);
    assert!(offset_of!(READ_USN_JOURNAL_DATA_V1, UsnJournalID) == 32);
    assert!(offset_of!(READ_USN_JOURNAL_DATA_V1, MinMajorVersion) == 40);
    assert!(offset_of!(READ_USN_JOURNAL_DATA_V1, MaxMajorVersion) == 42);

    assert!(size_of::<USN_JOURNAL_DATA_V1>() == 64);
    assert!(align_of::<USN_JOURNAL_DATA_V1>() == 8);
    assert!(offset_of!(USN_JOURNAL_DATA_V1, FirstUsn) == 8);
    assert!(offset_of!(USN_JOURNAL_DATA_V1, NextUsn) == 16);
    assert!(offset_of!(USN_JOURNAL_DATA_V1, LowestValidUsn) == 24);
    assert!(offset_of!(USN_JOURNAL_DATA_V1, MaxUsn) == 32);
    assert!(offset_of!(USN_JOURNAL_DATA_V1, MaximumSize) == 40);
    assert!(offset_of!(USN_JOURNAL_DATA_V1, AllocationDelta) == 48);
    assert!(offset_of!(USN_JOURNAL_DATA_V1, MinSupportedMajorVersion) == 56);
    assert!(offset_of!(USN_JOURNAL_DATA_V1, MaxSupportedMajorVersion) == 58);

    assert!(size_of::<USN_RECORD_V2>() == 64);
    assert!(align_of::<USN_RECORD_V2>() == 8);
    assert!(offset_of!(USN_RECORD_V2, MajorVersion) == 4);
    assert!(offset_of!(USN_RECORD_V2, FileReferenceNumber) == 8);
    assert!(offset_of!(USN_RECORD_V2, ParentFileReferenceNumber) == 16);
    assert!(offset_of!(USN_RECORD_V2, Usn) == 24);
    assert!(offset_of!(USN_RECORD_V2, TimeStamp) == 32);
    assert!(offset_of!(USN_RECORD_V2, Reason) == 40);
    assert!(offset_of!(USN_RECORD_V2, FileAttributes) == 52);
    assert!(offset_of!(USN_RECORD_V2, FileNameLength) == 56);
    assert!(offset_of!(USN_RECORD_V2, FileNameOffset) == 58);
    assert!(offset_of!(USN_RECORD_V2, FileName) == 60);

    assert!(size_of::<USN_RECORD_V3>() == 80);
    assert!(align_of::<USN_RECORD_V3>() == 8);
    assert!(offset_of!(USN_RECORD_V3, MajorVersion) == 4);
    assert!(offset_of!(USN_RECORD_V3, FileReferenceNumber) == 8);
    assert!(offset_of!(USN_RECORD_V3, ParentFileReferenceNumber) == 24);
    assert!(offset_of!(USN_RECORD_V3, Usn) == 40);
    assert!(offset_of!(USN_RECORD_V3, TimeStamp) == 48);
    assert!(offset_of!(USN_RECORD_V3, Reason) == 56);
    assert!(offset_of!(USN_RECORD_V3, FileAttributes) == 68);
    assert!(offset_of!(USN_RECORD_V3, FileNameLength) == 72);
    assert!(offset_of!(USN_RECORD_V3, FileNameOffset) == 74);
    assert!(offset_of!(USN_RECORD_V3, FileName) == 76);
};

// ---- Typed wrappers --------------------------------------------------------------------------

/// `CreateIoCompletionPort(INVALID, NULL, 0, threads)`: a new port, which the caller closes.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn create_completion_port(threads: u32) -> Result<Handle, WinError> {
    // SAFETY: an invalid file and a null port are the documented form for "make a port"; the
    // call reads no memory.
    let port = unsafe { CreateIoCompletionPort(Handle::INVALID, Handle::NULL, 0, threads) };
    if port.is_null() { Err(WinError::last()) } else { Ok(port) }
}

/// `CreateIoCompletionPort(file, port, key, 0)`: binds `file` (opened overlapped) to `port`,
/// so its completions arrive there carrying `key`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn associate_completion_port(file: Handle, port: Handle, key: usize) -> Result<(), WinError> {
    // SAFETY: two handles and two numbers; the call reads no memory.
    let bound = unsafe { CreateIoCompletionPort(file, port, key, 0) };
    if bound.is_null() { Err(WinError::last()) } else { Ok(()) }
}

/// `GetQueuedCompletionStatusEx`, not alertable: fills the front of `entries` and returns how
/// many it filled; a timeout is `Err(WinError(258))` (`WAIT_TIMEOUT`), as the call reports it.
/// More than `u32::MAX` entries are offered as `u32::MAX`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn get_completions(port: Handle, entries: &mut [OVERLAPPED_ENTRY], timeout_ms: u32) -> Result<usize, WinError> {
    let count = u32::try_from(entries.len()).unwrap_or(u32::MAX);
    let mut removed = 0u32;
    // SAFETY: `entries` is `count` or more live entries the call may write; `removed` is a
    // live `u32`; nothing is kept past the call.
    unsafe { GetQueuedCompletionStatusEx(port, entries.as_mut_ptr(), count, &mut removed, timeout_ms, Bool::FALSE) }
        .ok()?;
    Ok(removed as usize)
}

/// `PostQueuedCompletionStatus`: queues a completion of `bytes`, `key` and `overlapped` on
/// `port`. The kernel never reads through `overlapped`; whoever receives it does.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn post_completion(port: Handle, bytes: u32, key: usize, overlapped: *const OVERLAPPED) -> Result<(), WinError> {
    // SAFETY: the pointer is carried, not read, by the kernel (ioapiset.h); the rest are numbers.
    unsafe { PostQueuedCompletionStatus(port, bytes, key, overlapped) }.ok()
}
