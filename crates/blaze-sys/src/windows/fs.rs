//! Files, directories and names: `Win32::Storage::FileSystem`, declared from the Win32 API
//! reference (fileapi.h, winbase.h, minwinbase.h, winnt.h) as windows-sys 0.61.2 has them.
//!
//! The declarations keep the Win32 names and the windows-sys aliases (`FILE_SHARE_MODE`,
//! `FILE_FLAGS_AND_ATTRIBUTES`, ...); a handle is a [`Handle`], a `BOOL` a [`Bool`]. Below them
//! is a small set of typed wrappers for the calls the tree makes most, each taking a
//! [`WideCStr`] for a path, so a path without its NUL is a refusal at construction and not a
//! read past the end.

#![allow(
    non_upper_case_globals,
    reason = "the information classes keep their Win32 names (`FileBasicInfo`, `ExtendedFileIdType`)"
)]

use core::ffi::c_void;
use core::mem::MaybeUninit;

use super::foundation::FILETIME;
use super::io::{LPOVERLAPPED_COMPLETION_ROUTINE, OVERLAPPED};
use super::security::SECURITY_ATTRIBUTES;
use super::{Bool, GUID, Handle, PCWSTR, PWSTR, WinError};

// ---- Aliases ---------------------------------------------------------------------------------

/// replaces windows_sys::Win32::Storage::FileSystem::FILE_ACCESS_RIGHTS
pub type FILE_ACCESS_RIGHTS = u32;
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_ACTION
pub type FILE_ACTION = u32;
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_CREATION_DISPOSITION
pub type FILE_CREATION_DISPOSITION = u32;
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES
pub type FILE_FLAGS_AND_ATTRIBUTES = u32;
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_ID_TYPE
pub type FILE_ID_TYPE = i32;
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_INFO_BY_HANDLE_CLASS
pub type FILE_INFO_BY_HANDLE_CLASS = i32;
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_NOTIFY_CHANGE
pub type FILE_NOTIFY_CHANGE = u32;
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_SHARE_MODE
pub type FILE_SHARE_MODE = u32;
/// replaces windows_sys::Win32::Storage::FileSystem::GETFINALPATHNAMEBYHANDLE_FLAGS
pub type GETFINALPATHNAMEBYHANDLE_FLAGS = u32;
/// replaces windows_sys::Win32::Storage::FileSystem::MOVE_FILE_FLAGS
pub type MOVE_FILE_FLAGS = u32;

// ---- Constants -------------------------------------------------------------------------------

/// `CREATE_NEW` (fileapi.h): create, failing if the file exists.
/// replaces windows_sys::Win32::Storage::FileSystem::CREATE_NEW
pub const CREATE_NEW: FILE_CREATION_DISPOSITION = 1u32;
/// `OPEN_EXISTING` (fileapi.h).
/// replaces windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING
pub const OPEN_EXISTING: FILE_CREATION_DISPOSITION = 3u32;
/// `OPEN_ALWAYS` (fileapi.h): open, creating it if absent.
/// replaces windows_sys::Win32::Storage::FileSystem::OPEN_ALWAYS
pub const OPEN_ALWAYS: FILE_CREATION_DISPOSITION = 4u32;

/// `ExtendedFileIdType` (winbase.h): a `FILE_ID_DESCRIPTOR` holding a `FILE_ID_128`.
/// replaces windows_sys::Win32::Storage::FileSystem::ExtendedFileIdType
pub const ExtendedFileIdType: FILE_ID_TYPE = 2i32;

/// `FILE_ACTION_ADDED` (winnt.h): a `FILE_NOTIFY_INFORMATION` action.
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_ACTION_ADDED
pub const FILE_ACTION_ADDED: FILE_ACTION = 1u32;
/// `FILE_ACTION_RENAMED_NEW_NAME` (winnt.h).
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_ACTION_RENAMED_NEW_NAME
pub const FILE_ACTION_RENAMED_NEW_NAME: FILE_ACTION = 5u32;

/// `FILE_LIST_DIRECTORY` (winnt.h; the same bit as `FILE_READ_DATA`).
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_LIST_DIRECTORY
pub const FILE_LIST_DIRECTORY: FILE_ACCESS_RIGHTS = 1u32;
/// `FILE_APPEND_DATA` (winnt.h).
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_APPEND_DATA
pub const FILE_APPEND_DATA: FILE_ACCESS_RIGHTS = 4u32;
/// `FILE_TRAVERSE` (winnt.h).
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_TRAVERSE
pub const FILE_TRAVERSE: FILE_ACCESS_RIGHTS = 32u32;
/// `FILE_READ_ATTRIBUTES` (winnt.h).
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES
pub const FILE_READ_ATTRIBUTES: FILE_ACCESS_RIGHTS = 128u32;
/// `FILE_WRITE_ATTRIBUTES` (winnt.h).
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_WRITE_ATTRIBUTES
pub const FILE_WRITE_ATTRIBUTES: FILE_ACCESS_RIGHTS = 256u32;
/// `READ_CONTROL` (winnt.h): read the security descriptor, owner and DACL.
/// replaces windows_sys::Win32::Storage::FileSystem::READ_CONTROL
pub const READ_CONTROL: FILE_ACCESS_RIGHTS = 131072u32;
/// `FILE_READ_DATA` (winnt.h): read a file's data, or a pipe's.
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_READ_DATA
pub const FILE_READ_DATA: FILE_ACCESS_RIGHTS = 1u32;
/// `FILE_WRITE_DATA` (winnt.h): write a file's data, or a pipe's.
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_WRITE_DATA
pub const FILE_WRITE_DATA: FILE_ACCESS_RIGHTS = 2u32;
/// `SYNCHRONIZE` (winnt.h): wait on the object.
/// replaces windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE
pub const SYNCHRONIZE: FILE_ACCESS_RIGHTS = 1048576u32;
/// `FILE_ALL_ACCESS` (winnt.h): every right a file or pipe has.
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS
pub const FILE_ALL_ACCESS: FILE_ACCESS_RIGHTS = 2032127u32;

/// `FILE_ATTRIBUTE_READONLY` (winnt.h).
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_READONLY
pub const FILE_ATTRIBUTE_READONLY: FILE_FLAGS_AND_ATTRIBUTES = 1u32;
/// `FILE_ATTRIBUTE_DIRECTORY` (winnt.h).
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY
pub const FILE_ATTRIBUTE_DIRECTORY: FILE_FLAGS_AND_ATTRIBUTES = 16u32;
/// `FILE_ATTRIBUTE_NORMAL` (winnt.h).
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL
pub const FILE_ATTRIBUTE_NORMAL: FILE_FLAGS_AND_ATTRIBUTES = 128u32;
/// `FILE_ATTRIBUTE_REPARSE_POINT` (winnt.h).
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
pub const FILE_ATTRIBUTE_REPARSE_POINT: FILE_FLAGS_AND_ATTRIBUTES = 1024u32;
/// `PIPE_ACCESS_INBOUND` (winbase.h): `CreateNamedPipeW`'s open mode, client to server.
/// replaces windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_INBOUND
pub const PIPE_ACCESS_INBOUND: FILE_FLAGS_AND_ATTRIBUTES = 1u32;
/// `PIPE_ACCESS_OUTBOUND` (winbase.h): server to client.
/// replaces windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_OUTBOUND
pub const PIPE_ACCESS_OUTBOUND: FILE_FLAGS_AND_ATTRIBUTES = 2u32;
/// `PIPE_ACCESS_DUPLEX` (winbase.h): both ways.
/// replaces windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX
pub const PIPE_ACCESS_DUPLEX: FILE_FLAGS_AND_ATTRIBUTES = 3u32;
/// `SECURITY_IDENTIFICATION` (winbase.h): `CreateFileW` of a pipe's client: the server may
/// identify the client, never act as it.
/// replaces windows_sys::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION
pub const SECURITY_IDENTIFICATION: FILE_FLAGS_AND_ATTRIBUTES = 65536u32;
/// `SECURITY_SQOS_PRESENT` (winbase.h): the `SECURITY_*` impersonation level is given.
/// replaces windows_sys::Win32::Storage::FileSystem::SECURITY_SQOS_PRESENT
pub const SECURITY_SQOS_PRESENT: FILE_FLAGS_AND_ATTRIBUTES = 1048576u32;
/// `FILE_FLAG_FIRST_PIPE_INSTANCE` (winbase.h).
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_FLAG_FIRST_PIPE_INSTANCE
pub const FILE_FLAG_FIRST_PIPE_INSTANCE: FILE_FLAGS_AND_ATTRIBUTES = 524288u32;
/// `FILE_FLAG_OPEN_REPARSE_POINT` (winbase.h): open the link, not its target.
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT
pub const FILE_FLAG_OPEN_REPARSE_POINT: FILE_FLAGS_AND_ATTRIBUTES = 2097152u32;
/// `FILE_FLAG_BACKUP_SEMANTICS` (winbase.h): needed to open a directory.
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS
pub const FILE_FLAG_BACKUP_SEMANTICS: FILE_FLAGS_AND_ATTRIBUTES = 33554432u32;
/// `FILE_FLAG_SEQUENTIAL_SCAN` (winbase.h).
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_FLAG_SEQUENTIAL_SCAN
pub const FILE_FLAG_SEQUENTIAL_SCAN: FILE_FLAGS_AND_ATTRIBUTES = 134217728u32;
/// `FILE_FLAG_OVERLAPPED` (winbase.h): asynchronous IO, completed on a port or an event.
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED
pub const FILE_FLAG_OVERLAPPED: FILE_FLAGS_AND_ATTRIBUTES = 1073741824u32;

/// `INVALID_FILE_ATTRIBUTES` (fileapi.h): `GetFileAttributesW`'s failure.
/// replaces windows_sys::Win32::Storage::FileSystem::INVALID_FILE_ATTRIBUTES
pub const INVALID_FILE_ATTRIBUTES: u32 = 4294967295u32;

/// `FileBasicInfo` (minwinbase.h): `FILE_BASIC_INFO`.
/// replaces windows_sys::Win32::Storage::FileSystem::FileBasicInfo
pub const FileBasicInfo: FILE_INFO_BY_HANDLE_CLASS = 0i32;
/// `FileEndOfFileInfo` (minwinbase.h): `FILE_END_OF_FILE_INFO`.
/// replaces windows_sys::Win32::Storage::FileSystem::FileEndOfFileInfo
pub const FileEndOfFileInfo: FILE_INFO_BY_HANDLE_CLASS = 6i32;
/// `FileAttributeTagInfo` (minwinbase.h): `FILE_ATTRIBUTE_TAG_INFO`.
/// replaces windows_sys::Win32::Storage::FileSystem::FileAttributeTagInfo
pub const FileAttributeTagInfo: FILE_INFO_BY_HANDLE_CLASS = 9i32;
/// `FileIdBothDirectoryInfo` (minwinbase.h): a run of `FILE_ID_BOTH_DIR_INFO`.
/// replaces windows_sys::Win32::Storage::FileSystem::FileIdBothDirectoryInfo
pub const FileIdBothDirectoryInfo: FILE_INFO_BY_HANDLE_CLASS = 10i32;
/// `FileIdBothDirectoryRestartInfo` (minwinbase.h): as above, from the directory's start.
/// replaces windows_sys::Win32::Storage::FileSystem::FileIdBothDirectoryRestartInfo
pub const FileIdBothDirectoryRestartInfo: FILE_INFO_BY_HANDLE_CLASS = 11i32;
/// `FileIdInfo` (minwinbase.h): `FILE_ID_INFO`.
/// replaces windows_sys::Win32::Storage::FileSystem::FileIdInfo
pub const FileIdInfo: FILE_INFO_BY_HANDLE_CLASS = 18i32;

/// `FILE_NAME_NORMALIZED` (fileapi.h): `GetFinalPathNameByHandleW`'s default.
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_NAME_NORMALIZED
pub const FILE_NAME_NORMALIZED: GETFINALPATHNAMEBYHANDLE_FLAGS = 0u32;
/// `VOLUME_NAME_DOS` (fileapi.h): `C:\...`.
/// replaces windows_sys::Win32::Storage::FileSystem::VOLUME_NAME_DOS
pub const VOLUME_NAME_DOS: GETFINALPATHNAMEBYHANDLE_FLAGS = 0u32;
/// `VOLUME_NAME_GUID` (fileapi.h): `\\?\Volume{...}\...`.
/// replaces windows_sys::Win32::Storage::FileSystem::VOLUME_NAME_GUID
pub const VOLUME_NAME_GUID: GETFINALPATHNAMEBYHANDLE_FLAGS = 1u32;
/// `VOLUME_NAME_NT` (fileapi.h): `\Device\HarddiskVolumeN\...`.
/// replaces windows_sys::Win32::Storage::FileSystem::VOLUME_NAME_NT
pub const VOLUME_NAME_NT: GETFINALPATHNAMEBYHANDLE_FLAGS = 2u32;

/// `FILE_NOTIFY_CHANGE_FILE_NAME` (winnt.h).
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_NOTIFY_CHANGE_FILE_NAME
pub const FILE_NOTIFY_CHANGE_FILE_NAME: FILE_NOTIFY_CHANGE = 1u32;
/// `FILE_NOTIFY_CHANGE_DIR_NAME` (winnt.h).
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_NOTIFY_CHANGE_DIR_NAME
pub const FILE_NOTIFY_CHANGE_DIR_NAME: FILE_NOTIFY_CHANGE = 2u32;
/// `FILE_NOTIFY_CHANGE_ATTRIBUTES` (winnt.h).
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_NOTIFY_CHANGE_ATTRIBUTES
pub const FILE_NOTIFY_CHANGE_ATTRIBUTES: FILE_NOTIFY_CHANGE = 4u32;
/// `FILE_NOTIFY_CHANGE_SIZE` (winnt.h).
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_NOTIFY_CHANGE_SIZE
pub const FILE_NOTIFY_CHANGE_SIZE: FILE_NOTIFY_CHANGE = 8u32;
/// `FILE_NOTIFY_CHANGE_LAST_WRITE` (winnt.h).
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_NOTIFY_CHANGE_LAST_WRITE
pub const FILE_NOTIFY_CHANGE_LAST_WRITE: FILE_NOTIFY_CHANGE = 16u32;
/// `FILE_NOTIFY_CHANGE_CREATION` (winnt.h).
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_NOTIFY_CHANGE_CREATION
pub const FILE_NOTIFY_CHANGE_CREATION: FILE_NOTIFY_CHANGE = 64u32;

/// `FILE_SHARE_READ` (winnt.h).
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ
pub const FILE_SHARE_READ: FILE_SHARE_MODE = 1u32;
/// `FILE_SHARE_WRITE` (winnt.h).
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE
pub const FILE_SHARE_WRITE: FILE_SHARE_MODE = 2u32;
/// `FILE_SHARE_DELETE` (winnt.h).
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE
pub const FILE_SHARE_DELETE: FILE_SHARE_MODE = 4u32;

/// `MOVEFILE_REPLACE_EXISTING` (winbase.h).
/// replaces windows_sys::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING
pub const MOVEFILE_REPLACE_EXISTING: MOVE_FILE_FLAGS = 1u32;
/// `MOVEFILE_WRITE_THROUGH` (winbase.h): return only once the move is on disk.
/// replaces windows_sys::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH
pub const MOVEFILE_WRITE_THROUGH: MOVE_FILE_FLAGS = 8u32;

// ---- Structures ------------------------------------------------------------------------------

/// `FILETIME` zero, for the `Default`s below (not every `FILETIME` declaration derives one).
const NO_TIME: FILETIME = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };

/// `BY_HANDLE_FILE_INFORMATION` (fileapi.h): `GetFileInformationByHandle`'s answer.
/// replaces windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BY_HANDLE_FILE_INFORMATION {
    pub dwFileAttributes: u32,
    pub ftCreationTime: FILETIME,
    pub ftLastAccessTime: FILETIME,
    pub ftLastWriteTime: FILETIME,
    pub dwVolumeSerialNumber: u32,
    pub nFileSizeHigh: u32,
    pub nFileSizeLow: u32,
    pub nNumberOfLinks: u32,
    pub nFileIndexHigh: u32,
    pub nFileIndexLow: u32,
}

impl Default for BY_HANDLE_FILE_INFORMATION {
    fn default() -> Self {
        BY_HANDLE_FILE_INFORMATION {
            dwFileAttributes: 0,
            ftCreationTime: NO_TIME,
            ftLastAccessTime: NO_TIME,
            ftLastWriteTime: NO_TIME,
            dwVolumeSerialNumber: 0,
            nFileSizeHigh: 0,
            nFileSizeLow: 0,
            nNumberOfLinks: 0,
            nFileIndexHigh: 0,
            nFileIndexLow: 0,
        }
    }
}

/// `FILE_ATTRIBUTE_TAG_INFO` (winbase.h): the attributes and, for a reparse point, its tag.
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_TAG_INFO
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FILE_ATTRIBUTE_TAG_INFO {
    pub FileAttributes: u32,
    pub ReparseTag: u32,
}

/// `FILE_BASIC_INFO` (winbase.h): the four times (100 ns since 1601) and the attributes.
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_BASIC_INFO
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FILE_BASIC_INFO {
    pub CreationTime: i64,
    pub LastAccessTime: i64,
    pub LastWriteTime: i64,
    pub ChangeTime: i64,
    pub FileAttributes: u32,
}

/// `FILE_END_OF_FILE_INFO` (winbase.h): a file's length, set by `SetFileInformationByHandle`.
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_END_OF_FILE_INFO
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FILE_END_OF_FILE_INFO {
    pub EndOfFile: i64,
}

/// `FILE_ID_128` (winnt.h): a 128-bit file id (ReFS; NTFS ids fit in its low 64 bits).
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_ID_128
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FILE_ID_128 {
    pub Identifier: [u8; 16],
}

/// `FILE_ID_BOTH_DIR_INFO` (winbase.h): one entry of a directory read by
/// `GetFileInformationByHandleEx`; `FileName` is the first of `FileNameLength` bytes, and the
/// next entry is `NextEntryOffset` bytes on (0 for the last).
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_ID_BOTH_DIR_INFO
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FILE_ID_BOTH_DIR_INFO {
    pub NextEntryOffset: u32,
    pub FileIndex: u32,
    pub CreationTime: i64,
    pub LastAccessTime: i64,
    pub LastWriteTime: i64,
    pub ChangeTime: i64,
    pub EndOfFile: i64,
    pub AllocationSize: i64,
    pub FileAttributes: u32,
    pub FileNameLength: u32,
    pub EaSize: u32,
    pub ShortNameLength: i8,
    pub ShortName: [u16; 12],
    pub FileId: i64,
    pub FileName: [u16; 1],
}

/// `FILE_ID_DESCRIPTOR` (winbase.h): `OpenFileById`'s id, of the kind `Type` names.
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_ID_DESCRIPTOR
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FILE_ID_DESCRIPTOR {
    pub dwSize: u32,
    pub Type: FILE_ID_TYPE,
    pub Anonymous: FILE_ID_DESCRIPTOR_0,
}

/// `FILE_ID_DESCRIPTOR`'s unnamed union (winbase.h).
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_ID_DESCRIPTOR_0
#[repr(C)]
#[derive(Clone, Copy)]
pub union FILE_ID_DESCRIPTOR_0 {
    pub FileId: i64,
    pub ObjectId: GUID,
    pub ExtendedFileId: FILE_ID_128,
}

impl Default for FILE_ID_DESCRIPTOR_0 {
    fn default() -> Self {
        // The widest member, so all 16 bytes are zero.
        FILE_ID_DESCRIPTOR_0 { ExtendedFileId: FILE_ID_128 { Identifier: [0; 16] } }
    }
}

/// `FILE_ID_INFO` (winbase.h): the volume's serial and the file's 128-bit id.
/// replaces windows_sys::Win32::Storage::FileSystem::FILE_ID_INFO
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FILE_ID_INFO {
    pub VolumeSerialNumber: u64,
    pub FileId: FILE_ID_128,
}

/// `WIN32_FIND_DATAW` (minwinbase.h): one entry of `FindFirstFileW`/`FindNextFileW`.
/// replaces windows_sys::Win32::Storage::FileSystem::WIN32_FIND_DATAW
#[repr(C)]
#[derive(Clone, Copy)]
pub struct WIN32_FIND_DATAW {
    pub dwFileAttributes: u32,
    pub ftCreationTime: FILETIME,
    pub ftLastAccessTime: FILETIME,
    pub ftLastWriteTime: FILETIME,
    pub nFileSizeHigh: u32,
    pub nFileSizeLow: u32,
    pub dwReserved0: u32,
    pub dwReserved1: u32,
    pub cFileName: [u16; 260],
    pub cAlternateFileName: [u16; 14],
}

impl Default for WIN32_FIND_DATAW {
    fn default() -> Self {
        WIN32_FIND_DATAW {
            dwFileAttributes: 0,
            ftCreationTime: NO_TIME,
            ftLastAccessTime: NO_TIME,
            ftLastWriteTime: NO_TIME,
            nFileSizeHigh: 0,
            nFileSizeLow: 0,
            dwReserved0: 0,
            dwReserved1: 0,
            cFileName: [0; 260],
            cAlternateFileName: [0; 14],
        }
    }
}

// Win32 API: fileapi.h, winbase.h, minwinbase.h, winnt.h. LLP64: x86-64 and arm64 Windows
// lay these out alike.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const _: () = {
    use core::mem::offset_of;

    assert!(size_of::<BY_HANDLE_FILE_INFORMATION>() == 52);
    assert!(align_of::<BY_HANDLE_FILE_INFORMATION>() == 4);
    assert!(offset_of!(BY_HANDLE_FILE_INFORMATION, dwFileAttributes) == 0);
    assert!(offset_of!(BY_HANDLE_FILE_INFORMATION, ftCreationTime) == 4);
    assert!(offset_of!(BY_HANDLE_FILE_INFORMATION, ftLastAccessTime) == 12);
    assert!(offset_of!(BY_HANDLE_FILE_INFORMATION, ftLastWriteTime) == 20);
    assert!(offset_of!(BY_HANDLE_FILE_INFORMATION, dwVolumeSerialNumber) == 28);
    assert!(offset_of!(BY_HANDLE_FILE_INFORMATION, nFileSizeHigh) == 32);
    assert!(offset_of!(BY_HANDLE_FILE_INFORMATION, nFileSizeLow) == 36);
    assert!(offset_of!(BY_HANDLE_FILE_INFORMATION, nNumberOfLinks) == 40);
    assert!(offset_of!(BY_HANDLE_FILE_INFORMATION, nFileIndexHigh) == 44);
    assert!(offset_of!(BY_HANDLE_FILE_INFORMATION, nFileIndexLow) == 48);

    assert!(size_of::<FILE_ATTRIBUTE_TAG_INFO>() == 8);
    assert!(align_of::<FILE_ATTRIBUTE_TAG_INFO>() == 4);
    assert!(offset_of!(FILE_ATTRIBUTE_TAG_INFO, ReparseTag) == 4);

    assert!(size_of::<FILE_BASIC_INFO>() == 40);
    assert!(align_of::<FILE_BASIC_INFO>() == 8);
    assert!(offset_of!(FILE_BASIC_INFO, LastAccessTime) == 8);
    assert!(offset_of!(FILE_BASIC_INFO, LastWriteTime) == 16);
    assert!(offset_of!(FILE_BASIC_INFO, ChangeTime) == 24);
    assert!(offset_of!(FILE_BASIC_INFO, FileAttributes) == 32);

    assert!(size_of::<FILE_END_OF_FILE_INFO>() == 8);
    assert!(align_of::<FILE_END_OF_FILE_INFO>() == 8);

    assert!(size_of::<FILE_ID_128>() == 16);
    assert!(align_of::<FILE_ID_128>() == 1);

    assert!(size_of::<FILE_ID_BOTH_DIR_INFO>() == 112);
    assert!(align_of::<FILE_ID_BOTH_DIR_INFO>() == 8);
    assert!(offset_of!(FILE_ID_BOTH_DIR_INFO, FileIndex) == 4);
    assert!(offset_of!(FILE_ID_BOTH_DIR_INFO, CreationTime) == 8);
    assert!(offset_of!(FILE_ID_BOTH_DIR_INFO, LastWriteTime) == 24);
    assert!(offset_of!(FILE_ID_BOTH_DIR_INFO, ChangeTime) == 32);
    assert!(offset_of!(FILE_ID_BOTH_DIR_INFO, EndOfFile) == 40);
    assert!(offset_of!(FILE_ID_BOTH_DIR_INFO, AllocationSize) == 48);
    assert!(offset_of!(FILE_ID_BOTH_DIR_INFO, FileAttributes) == 56);
    assert!(offset_of!(FILE_ID_BOTH_DIR_INFO, FileNameLength) == 60);
    assert!(offset_of!(FILE_ID_BOTH_DIR_INFO, EaSize) == 64);
    assert!(offset_of!(FILE_ID_BOTH_DIR_INFO, ShortNameLength) == 68);
    assert!(offset_of!(FILE_ID_BOTH_DIR_INFO, ShortName) == 70);
    assert!(offset_of!(FILE_ID_BOTH_DIR_INFO, FileId) == 96);
    assert!(offset_of!(FILE_ID_BOTH_DIR_INFO, FileName) == 104);

    assert!(size_of::<FILE_ID_DESCRIPTOR_0>() == 16);
    assert!(align_of::<FILE_ID_DESCRIPTOR_0>() == 8);
    assert!(size_of::<FILE_ID_DESCRIPTOR>() == 24);
    assert!(align_of::<FILE_ID_DESCRIPTOR>() == 8);
    assert!(offset_of!(FILE_ID_DESCRIPTOR, Type) == 4);
    assert!(offset_of!(FILE_ID_DESCRIPTOR, Anonymous) == 8);

    assert!(size_of::<FILE_ID_INFO>() == 24);
    assert!(align_of::<FILE_ID_INFO>() == 8);
    assert!(offset_of!(FILE_ID_INFO, FileId) == 8);

    assert!(size_of::<WIN32_FIND_DATAW>() == 592);
    assert!(align_of::<WIN32_FIND_DATAW>() == 4);
    assert!(offset_of!(WIN32_FIND_DATAW, ftLastWriteTime) == 20);
    assert!(offset_of!(WIN32_FIND_DATAW, nFileSizeHigh) == 28);
    assert!(offset_of!(WIN32_FIND_DATAW, nFileSizeLow) == 32);
    assert!(offset_of!(WIN32_FIND_DATAW, cFileName) == 44);
    assert!(offset_of!(WIN32_FIND_DATAW, cAlternateFileName) == 564);
};

// ---- Functions -------------------------------------------------------------------------------

#[link(name = "kernel32", kind = "raw-dylib")]
unsafe extern "system" {
    /// `CreateDirectoryW` (fileapi.h).
    /// replaces windows_sys::Win32::Storage::FileSystem::CreateDirectoryW
    pub fn CreateDirectoryW(lppathname: PCWSTR, lpsecurityattributes: *const SECURITY_ATTRIBUTES) -> Bool;

    /// `CreateFileW` (fileapi.h): [`Handle::INVALID`] on failure.
    /// replaces windows_sys::Win32::Storage::FileSystem::CreateFileW
    pub fn CreateFileW(
        lpfilename: PCWSTR,
        dwdesiredaccess: u32,
        dwsharemode: FILE_SHARE_MODE,
        lpsecurityattributes: *const SECURITY_ATTRIBUTES,
        dwcreationdisposition: FILE_CREATION_DISPOSITION,
        dwflagsandattributes: FILE_FLAGS_AND_ATTRIBUTES,
        htemplatefile: Handle,
    ) -> Handle;

    /// `DeleteFileW` (fileapi.h).
    /// replaces windows_sys::Win32::Storage::FileSystem::DeleteFileW
    pub fn DeleteFileW(lpfilename: PCWSTR) -> Bool;

    /// `FindClose` (fileapi.h): ends a `FindFirstFileW` search.
    /// replaces windows_sys::Win32::Storage::FileSystem::FindClose
    pub fn FindClose(hfindfile: Handle) -> Bool;

    /// `FindFirstFileW` (fileapi.h): [`Handle::INVALID`] on failure.
    /// replaces windows_sys::Win32::Storage::FileSystem::FindFirstFileW
    pub fn FindFirstFileW(lpfilename: PCWSTR, lpfindfiledata: *mut WIN32_FIND_DATAW) -> Handle;

    /// `FindNextFileW` (fileapi.h): false with `ERROR_NO_MORE_FILES` at the end.
    /// replaces windows_sys::Win32::Storage::FileSystem::FindNextFileW
    pub fn FindNextFileW(hfindfile: Handle, lpfindfiledata: *mut WIN32_FIND_DATAW) -> Bool;

    /// `GetFileAttributesW` (fileapi.h): [`INVALID_FILE_ATTRIBUTES`] on failure.
    /// replaces windows_sys::Win32::Storage::FileSystem::GetFileAttributesW
    pub fn GetFileAttributesW(lpfilename: PCWSTR) -> u32;

    /// `GetFileInformationByHandle` (fileapi.h).
    /// replaces windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle
    pub fn GetFileInformationByHandle(hfile: Handle, lpfileinformation: *mut BY_HANDLE_FILE_INFORMATION) -> Bool;

    /// `GetFileInformationByHandleEx` (winbase.h): the structure the class names.
    /// replaces windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandleEx
    pub fn GetFileInformationByHandleEx(
        hfile: Handle,
        fileinformationclass: FILE_INFO_BY_HANDLE_CLASS,
        lpfileinformation: *mut c_void,
        dwbuffersize: u32,
    ) -> Bool;

    /// `GetFileSizeEx` (fileapi.h).
    /// replaces windows_sys::Win32::Storage::FileSystem::GetFileSizeEx
    pub fn GetFileSizeEx(hfile: Handle, lpfilesize: *mut i64) -> Bool;

    /// `GetFinalPathNameByHandleW` (fileapi.h): the length written without the NUL, or the
    /// length needed with it when the buffer is short, or 0 on failure.
    /// replaces windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW
    pub fn GetFinalPathNameByHandleW(
        hfile: Handle,
        lpszfilepath: PWSTR,
        cchfilepath: u32,
        dwflags: GETFINALPATHNAMEBYHANDLE_FLAGS,
    ) -> u32;

    /// `GetFullPathNameW` (fileapi.h): as `GetFinalPathNameByHandleW`'s count.
    /// replaces windows_sys::Win32::Storage::FileSystem::GetFullPathNameW
    pub fn GetFullPathNameW(lpfilename: PCWSTR, nbufferlength: u32, lpbuffer: PWSTR, lpfilepart: *mut PWSTR) -> u32;

    /// `GetLongPathNameW` (fileapi.h): as `GetFinalPathNameByHandleW`'s count.
    /// replaces windows_sys::Win32::Storage::FileSystem::GetLongPathNameW
    pub fn GetLongPathNameW(lpszshortpath: PCWSTR, lpszlongpath: PWSTR, cchbuffer: u32) -> u32;

    /// `GetVolumeInformationByHandleW` (fileapi.h).
    /// replaces windows_sys::Win32::Storage::FileSystem::GetVolumeInformationByHandleW
    pub fn GetVolumeInformationByHandleW(
        hfile: Handle,
        lpvolumenamebuffer: PWSTR,
        nvolumenamesize: u32,
        lpvolumeserialnumber: *mut u32,
        lpmaximumcomponentlength: *mut u32,
        lpfilesystemflags: *mut u32,
        lpfilesystemnamebuffer: PWSTR,
        nfilesystemnamesize: u32,
    ) -> Bool;

    /// `MoveFileExW` (winbase.h).
    /// replaces windows_sys::Win32::Storage::FileSystem::MoveFileExW
    pub fn MoveFileExW(lpexistingfilename: PCWSTR, lpnewfilename: PCWSTR, dwflags: MOVE_FILE_FLAGS) -> Bool;

    /// `OpenFileById` (winbase.h): [`Handle::INVALID`] on failure.
    /// replaces windows_sys::Win32::Storage::FileSystem::OpenFileById
    pub fn OpenFileById(
        hvolumehint: Handle,
        lpfileid: *const FILE_ID_DESCRIPTOR,
        dwdesiredaccess: u32,
        dwsharemode: FILE_SHARE_MODE,
        lpsecurityattributes: *const SECURITY_ATTRIBUTES,
        dwflagsandattributes: FILE_FLAGS_AND_ATTRIBUTES,
    ) -> Handle;

    /// `ReadDirectoryChangesW` (winbase.h): with an `OVERLAPPED`, completes on the port.
    /// replaces windows_sys::Win32::Storage::FileSystem::ReadDirectoryChangesW
    pub fn ReadDirectoryChangesW(
        hdirectory: Handle,
        lpbuffer: *mut c_void,
        nbufferlength: u32,
        bwatchsubtree: Bool,
        dwnotifyfilter: FILE_NOTIFY_CHANGE,
        lpbytesreturned: *mut u32,
        lpoverlapped: *mut OVERLAPPED,
        lpcompletionroutine: LPOVERLAPPED_COMPLETION_ROUTINE,
    ) -> Bool;

    /// `ReadFile` (fileapi.h).
    /// replaces windows_sys::Win32::Storage::FileSystem::ReadFile
    pub fn ReadFile(
        hfile: Handle,
        lpbuffer: *mut u8,
        nnumberofbytestoread: u32,
        lpnumberofbytesread: *mut u32,
        lpoverlapped: *mut OVERLAPPED,
    ) -> Bool;

    /// `RemoveDirectoryW` (fileapi.h): an empty directory.
    /// replaces windows_sys::Win32::Storage::FileSystem::RemoveDirectoryW
    pub fn RemoveDirectoryW(lppathname: PCWSTR) -> Bool;

    /// `SetFileAttributesW` (fileapi.h).
    /// replaces windows_sys::Win32::Storage::FileSystem::SetFileAttributesW
    pub fn SetFileAttributesW(lpfilename: PCWSTR, dwfileattributes: FILE_FLAGS_AND_ATTRIBUTES) -> Bool;

    /// `SetFileCompletionNotificationModes` (winbase.h): `FILE_SKIP_*` flags.
    /// replaces windows_sys::Win32::Storage::FileSystem::SetFileCompletionNotificationModes
    pub fn SetFileCompletionNotificationModes(filehandle: Handle, flags: u8) -> Bool;

    /// `SetFileInformationByHandle` (fileapi.h): the structure the class names.
    /// replaces windows_sys::Win32::Storage::FileSystem::SetFileInformationByHandle
    pub fn SetFileInformationByHandle(
        hfile: Handle,
        fileinformationclass: FILE_INFO_BY_HANDLE_CLASS,
        lpfileinformation: *const c_void,
        dwbuffersize: u32,
    ) -> Bool;

    /// `SetFileTime` (fileapi.h): a null time is left as it is.
    /// replaces windows_sys::Win32::Storage::FileSystem::SetFileTime
    pub fn SetFileTime(
        hfile: Handle,
        lpcreationtime: *const FILETIME,
        lplastaccesstime: *const FILETIME,
        lplastwritetime: *const FILETIME,
    ) -> Bool;

    /// `WriteFile` (fileapi.h).
    /// replaces windows_sys::Win32::Storage::FileSystem::WriteFile
    pub fn WriteFile(
        hfile: Handle,
        lpbuffer: *const u8,
        nnumberofbytestowrite: u32,
        lpnumberofbyteswritten: *mut u32,
        lpoverlapped: *mut OVERLAPPED,
    ) -> Bool;
}

// ---- Typed wrappers --------------------------------------------------------------------------

/// A NUL-terminated UTF-16 string a call reads: a path, a pipe's or a mapping's name. Made only
/// from units that end in their one NUL, so the pointer a call takes ends where the slice does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WideCStr<'a>(&'a [u16]);

impl<'a> WideCStr<'a> {
    /// The string, when `units` ends in a NUL and holds no other; `None` otherwise.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn new(units: &'a [u16]) -> Option<WideCStr<'a>> {
        let (last, text) = units.split_last()?;
        if *last != 0 || text.contains(&0) {
            return None;
        }
        Some(WideCStr(units))
    }

    /// The pointer a declaration takes.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn as_ptr(self) -> PCWSTR {
        self.0.as_ptr()
    }

    /// The units, the NUL included.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn units_with_nul(self) -> &'a [u16] {
        self.0
    }
}

/// `CreateFileW` with no security attributes and no template: the opened handle, which the
/// caller closes (`foundation::CloseHandle`).
#[blaze::noalloc]
#[blaze::nopanic]
pub fn create_file(
    path: WideCStr<'_>,
    access: u32,
    share: FILE_SHARE_MODE,
    disposition: FILE_CREATION_DISPOSITION,
    flags: FILE_FLAGS_AND_ATTRIBUTES,
) -> Result<Handle, WinError> {
    // SAFETY: `path` is NUL-terminated within its slice (`WideCStr`); a null security pointer
    // and a null template are documented as "none"; the other arguments are numbers.
    let handle = unsafe {
        CreateFileW(path.as_ptr(), access, share, core::ptr::null(), disposition, flags, Handle::NULL)
    };
    if handle.is_invalid() { Err(WinError::last()) } else { Ok(handle) }
}

/// `GetFileAttributesW`: the attributes of what `path` names (following no link but the last
/// component's, as the call does).
#[blaze::noalloc]
#[blaze::nopanic]
pub fn get_file_attributes(path: WideCStr<'_>) -> Result<FILE_FLAGS_AND_ATTRIBUTES, WinError> {
    // SAFETY: `path` is NUL-terminated within its slice; the call reads only it.
    let attributes = unsafe { GetFileAttributesW(path.as_ptr()) };
    if attributes == INVALID_FILE_ATTRIBUTES { Err(WinError::last()) } else { Ok(attributes) }
}

/// `GetFileSizeEx`: the file's length in bytes.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn get_file_size(file: Handle) -> Result<u64, WinError> {
    let mut size = 0i64;
    // SAFETY: `size` is a live `i64` the call writes; a handle that is not an open file is the
    // call's own error.
    unsafe { GetFileSizeEx(file, &mut size) }.ok()?;
    // A length is never negative.
    Ok(size as u64)
}

/// A fixed-size structure `GetFileInformationByHandleEx` and `SetFileInformationByHandle` take
/// under one class: the class is the type's, so a structure cannot be passed under another's.
///
/// # Safety
///
/// `CLASS` is the class whose documented structure is `Self`, with `Self`'s exact layout, and
/// every bit pattern the call writes is a valid `Self` (plain integers).
pub unsafe trait FileInformation: Copy {
    /// The class the structure goes with.
    const CLASS: FILE_INFO_BY_HANDLE_CLASS;
}

// SAFETY: winbase.h pairs each class with this structure; all four are plain integers.
unsafe impl FileInformation for FILE_BASIC_INFO {
    const CLASS: FILE_INFO_BY_HANDLE_CLASS = FileBasicInfo;
}
// SAFETY: as above.
unsafe impl FileInformation for FILE_END_OF_FILE_INFO {
    const CLASS: FILE_INFO_BY_HANDLE_CLASS = FileEndOfFileInfo;
}
// SAFETY: as above.
unsafe impl FileInformation for FILE_ATTRIBUTE_TAG_INFO {
    const CLASS: FILE_INFO_BY_HANDLE_CLASS = FileAttributeTagInfo;
}
// SAFETY: as above.
unsafe impl FileInformation for FILE_ID_INFO {
    const CLASS: FILE_INFO_BY_HANDLE_CLASS = FileIdInfo;
}

/// `GetFileInformationByHandleEx` for one fixed-size structure.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn get_file_information<T: FileInformation>(file: Handle) -> Result<T, WinError> {
    let mut info = MaybeUninit::<T>::uninit();
    // SAFETY: the buffer is `size_of::<T>()` bytes the call may write; `T::CLASS` is `T`'s
    // class (the trait's contract), so the call writes exactly one `T`.
    unsafe {
        GetFileInformationByHandleEx(file, T::CLASS, info.as_mut_ptr().cast(), size_of::<T>() as u32)
    }
    .ok()?;
    // SAFETY: the call succeeded, so it wrote the whole `T`, and any bits it writes are a
    // valid `T` (the trait's contract).
    Ok(unsafe { info.assume_init() })
}

/// `SetFileInformationByHandle` for one fixed-size structure.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn set_file_information<T: FileInformation>(file: Handle, info: &T) -> Result<(), WinError> {
    // SAFETY: `info` is a live `T` of `size_of::<T>()` bytes the call only reads, under `T`'s
    // own class.
    unsafe {
        SetFileInformationByHandle(
            file,
            T::CLASS,
            core::ptr::from_ref(info).cast(),
            size_of::<T>() as u32,
        )
    }
    .ok()
}
