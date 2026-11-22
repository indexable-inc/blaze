//! `Win32::System::Threading` and `Win32::System::JobObjects`: processes and their start-up
//! records, waits, fibers, thread-pool waits, NUMA, and the job objects a child runs in.

#![allow(
    non_upper_case_globals,
    reason = "Win32 spells some constants in camel case (`JobObjectExtendedLimitInformation`)"
)]

use core::ffi::c_void;

use super::{
    Bool, Handle, PCWSTR, PWSTR, WinError,
    foundation::{WAIT_ABANDONED, WAIT_EVENT, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
    security::{SECURITY_ATTRIBUTES, TOKEN_ACCESS_MASK},
};

/// `PROCESS_CREATION_FLAGS`: `CreateProcessW`'s `dwCreationFlags`.
///
/// replaces windows_sys::Win32::System::Threading::PROCESS_CREATION_FLAGS
pub type PROCESS_CREATION_FLAGS = u32;
/// `STARTUPINFOW_FLAGS`: `STARTUPINFOW::dwFlags`.
///
/// replaces windows_sys::Win32::System::Threading::STARTUPINFOW_FLAGS
pub type STARTUPINFOW_FLAGS = u32;
/// `WORKER_THREAD_FLAGS`: `RegisterWaitForSingleObject`'s flags.
///
/// replaces windows_sys::Win32::System::Threading::WORKER_THREAD_FLAGS
pub type WORKER_THREAD_FLAGS = u32;
/// `LPPROC_THREAD_ATTRIBUTE_LIST`: an opaque attribute list the caller allocates (a pointer to
/// memory, not a kernel handle).
///
/// replaces windows_sys::Win32::System::Threading::LPPROC_THREAD_ATTRIBUTE_LIST
pub type LPPROC_THREAD_ATTRIBUTE_LIST = *mut c_void;
/// `LPFIBER_START_ROUTINE` (winbase.h): a fiber's entry.
///
/// replaces windows_sys::Win32::System::Threading::LPFIBER_START_ROUTINE
pub type LPFIBER_START_ROUTINE = Option<unsafe extern "system" fn(lpfiberparameter: *mut c_void)>;
/// `WAITORTIMERCALLBACK` (winnt.h): a thread-pool wait's callback; the `BOOLEAN` is whether
/// the wait timed out.
///
/// replaces windows_sys::Win32::System::Threading::WAITORTIMERCALLBACK
pub type WAITORTIMERCALLBACK = Option<unsafe extern "system" fn(param0: *mut c_void, param1: bool)>;
/// `JOB_OBJECT_LIMIT`: `JOBOBJECT_BASIC_LIMIT_INFORMATION::LimitFlags`.
///
/// replaces windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT
pub type JOB_OBJECT_LIMIT = u32;
/// `JOBOBJECTINFOCLASS`: which record `Query`/`SetInformationJobObject` read or write.
///
/// replaces windows_sys::Win32::System::JobObjects::JOBOBJECTINFOCLASS
pub type JOBOBJECTINFOCLASS = i32;

/// `CREATE_SUSPENDED` (4).
///
/// replaces windows_sys::Win32::System::Threading::CREATE_SUSPENDED
pub const CREATE_SUSPENDED: PROCESS_CREATION_FLAGS = 4u32;
/// `CREATE_UNICODE_ENVIRONMENT` (`0x400`): the environment block is UTF-16.
///
/// replaces windows_sys::Win32::System::Threading::CREATE_UNICODE_ENVIRONMENT
pub const CREATE_UNICODE_ENVIRONMENT: PROCESS_CREATION_FLAGS = 1024u32;
/// `EXTENDED_STARTUPINFO_PRESENT` (`0x80000`): the start-up record is a [`STARTUPINFOEXW`].
///
/// replaces windows_sys::Win32::System::Threading::EXTENDED_STARTUPINFO_PRESENT
pub const EXTENDED_STARTUPINFO_PRESENT: PROCESS_CREATION_FLAGS = 524288u32;
/// `CREATE_NO_WINDOW` (`0x8000000`).
///
/// replaces windows_sys::Win32::System::Threading::CREATE_NO_WINDOW
pub const CREATE_NO_WINDOW: PROCESS_CREATION_FLAGS = 134217728u32;
/// `STARTF_USESTDHANDLES` (`0x100`): the `hStd*` fields are the child's standard handles.
///
/// replaces windows_sys::Win32::System::Threading::STARTF_USESTDHANDLES
pub const STARTF_USESTDHANDLES: STARTUPINFOW_FLAGS = 256u32;
/// `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` (`0x20002`): the only handles a child inherits.
///
/// replaces windows_sys::Win32::System::Threading::PROC_THREAD_ATTRIBUTE_HANDLE_LIST
pub const PROC_THREAD_ATTRIBUTE_HANDLE_LIST: u32 = 131074u32;
/// `WT_EXECUTEONLYONCE` (8): the wait's callback runs once.
///
/// replaces windows_sys::Win32::System::Threading::WT_EXECUTEONLYONCE
pub const WT_EXECUTEONLYONCE: WORKER_THREAD_FLAGS = 8u32;
/// `INFINITE` (`0xFFFFFFFF`): a wait with no timeout.
///
/// replaces windows_sys::Win32::System::Threading::INFINITE
pub const INFINITE: u32 = 4294967295u32;

/// `PROCESS_ACCESS_RIGHTS`: what `OpenProcess` asks of a process.
///
/// replaces windows_sys::Win32::System::Threading::PROCESS_ACCESS_RIGHTS
pub type PROCESS_ACCESS_RIGHTS = u32;
/// `PROCESS_TERMINATE` (1).
///
/// replaces windows_sys::Win32::System::Threading::PROCESS_TERMINATE
pub const PROCESS_TERMINATE: PROCESS_ACCESS_RIGHTS = 1u32;
/// `PROCESS_VM_OPERATION` (8): change the process's address space (`VirtualAlloc2`,
/// `VirtualProtectEx`, `MapViewOfFile3`).
///
/// replaces windows_sys::Win32::System::Threading::PROCESS_VM_OPERATION
pub const PROCESS_VM_OPERATION: PROCESS_ACCESS_RIGHTS = 8u32;
/// `PROCESS_VM_READ` (16): `ReadProcessMemory`.
///
/// replaces windows_sys::Win32::System::Threading::PROCESS_VM_READ
pub const PROCESS_VM_READ: PROCESS_ACCESS_RIGHTS = 16u32;
/// `PROCESS_VM_WRITE` (32): `WriteProcessMemory`.
///
/// replaces windows_sys::Win32::System::Threading::PROCESS_VM_WRITE
pub const PROCESS_VM_WRITE: PROCESS_ACCESS_RIGHTS = 32u32;
/// `PROCESS_DUP_HANDLE` (64): duplicate handles into or out of the process.
///
/// replaces windows_sys::Win32::System::Threading::PROCESS_DUP_HANDLE
pub const PROCESS_DUP_HANDLE: PROCESS_ACCESS_RIGHTS = 64u32;
/// `PROCESS_QUERY_INFORMATION` (`0x400`).
///
/// replaces windows_sys::Win32::System::Threading::PROCESS_QUERY_INFORMATION
pub const PROCESS_QUERY_INFORMATION: PROCESS_ACCESS_RIGHTS = 1024u32;
/// `PROCESS_QUERY_LIMITED_INFORMATION` (`0x1000`): enough for `OpenProcessToken`'s query.
///
/// replaces windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION
pub const PROCESS_QUERY_LIMITED_INFORMATION: PROCESS_ACCESS_RIGHTS = 4096u32;
/// `PROCESS_SYNCHRONIZE` (`0x100000`): wait for the process to end.
///
/// replaces windows_sys::Win32::System::Threading::PROCESS_SYNCHRONIZE
pub const PROCESS_SYNCHRONIZE: PROCESS_ACCESS_RIGHTS = 1048576u32;

/// `JOB_OBJECT_LIMIT_ACTIVE_PROCESS` (8).
///
/// replaces windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_ACTIVE_PROCESS
pub const JOB_OBJECT_LIMIT_ACTIVE_PROCESS: JOB_OBJECT_LIMIT = 8u32;
/// `JOB_OBJECT_LIMIT_JOB_MEMORY` (`0x200`).
///
/// replaces windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_JOB_MEMORY
pub const JOB_OBJECT_LIMIT_JOB_MEMORY: JOB_OBJECT_LIMIT = 512u32;
/// `JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION` (`0x400`).
///
/// replaces windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
pub const JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION: JOB_OBJECT_LIMIT = 1024u32;
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (`0x2000`).
///
/// replaces windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
pub const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: JOB_OBJECT_LIMIT = 8192u32;
/// `JobObjectAssociateCompletionPortInformation` (7).
///
/// replaces windows_sys::Win32::System::JobObjects::JobObjectAssociateCompletionPortInformation
pub const JobObjectAssociateCompletionPortInformation: JOBOBJECTINFOCLASS = 7i32;
/// `JobObjectExtendedLimitInformation` (9).
///
/// replaces windows_sys::Win32::System::JobObjects::JobObjectExtendedLimitInformation
pub const JobObjectExtendedLimitInformation: JOBOBJECTINFOCLASS = 9i32;

/// `PROCESS_INFORMATION` (processthreadsapi.h): what `CreateProcessW` writes: the child's
/// process and first thread, both handles the caller now owns.
///
/// replaces windows_sys::Win32::System::Threading::PROCESS_INFORMATION
#[derive(Clone, Copy)]
#[repr(C)]
pub struct PROCESS_INFORMATION {
    pub hProcess: Handle,
    pub hThread: Handle,
    pub dwProcessId: u32,
    pub dwThreadId: u32,
}

/// `STARTUPINFOW` (processthreadsapi.h): a child's start-up record.
///
/// replaces windows_sys::Win32::System::Threading::STARTUPINFOW
#[derive(Clone, Copy)]
#[repr(C)]
pub struct STARTUPINFOW {
    pub cb: u32,
    pub lpReserved: PWSTR,
    pub lpDesktop: PWSTR,
    pub lpTitle: PWSTR,
    pub dwX: u32,
    pub dwY: u32,
    pub dwXSize: u32,
    pub dwYSize: u32,
    pub dwXCountChars: u32,
    pub dwYCountChars: u32,
    pub dwFillAttribute: u32,
    pub dwFlags: STARTUPINFOW_FLAGS,
    pub wShowWindow: u16,
    pub cbReserved2: u16,
    pub lpReserved2: *mut u8,
    pub hStdInput: Handle,
    pub hStdOutput: Handle,
    pub hStdError: Handle,
}

/// `STARTUPINFOEXW` (winbase.h): the start-up record with an attribute list.
///
/// replaces windows_sys::Win32::System::Threading::STARTUPINFOEXW
#[derive(Clone, Copy)]
#[repr(C)]
pub struct STARTUPINFOEXW {
    pub StartupInfo: STARTUPINFOW,
    pub lpAttributeList: LPPROC_THREAD_ATTRIBUTE_LIST,
}

/// `IO_COUNTERS` (winnt.h).
///
/// replaces windows_sys::Win32::System::Threading::IO_COUNTERS
#[derive(Clone, Copy)]
#[repr(C)]
pub struct IO_COUNTERS {
    pub ReadOperationCount: u64,
    pub WriteOperationCount: u64,
    pub OtherOperationCount: u64,
    pub ReadTransferCount: u64,
    pub WriteTransferCount: u64,
    pub OtherTransferCount: u64,
}

/// `JOBOBJECT_ASSOCIATE_COMPLETION_PORT` (winnt.h): the port a job posts its messages to.
///
/// replaces windows_sys::Win32::System::JobObjects::JOBOBJECT_ASSOCIATE_COMPLETION_PORT
#[derive(Clone, Copy)]
#[repr(C)]
pub struct JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
    pub CompletionKey: *mut c_void,
    pub CompletionPort: Handle,
}

/// `JOBOBJECT_BASIC_LIMIT_INFORMATION` (winnt.h).
///
/// replaces windows_sys::Win32::System::JobObjects::JOBOBJECT_BASIC_LIMIT_INFORMATION
#[derive(Clone, Copy)]
#[repr(C)]
pub struct JOBOBJECT_BASIC_LIMIT_INFORMATION {
    pub PerProcessUserTimeLimit: i64,
    pub PerJobUserTimeLimit: i64,
    pub LimitFlags: JOB_OBJECT_LIMIT,
    pub MinimumWorkingSetSize: usize,
    pub MaximumWorkingSetSize: usize,
    pub ActiveProcessLimit: u32,
    pub Affinity: usize,
    pub PriorityClass: u32,
    pub SchedulingClass: u32,
}

/// `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` (winnt.h).
///
/// replaces windows_sys::Win32::System::JobObjects::JOBOBJECT_EXTENDED_LIMIT_INFORMATION
#[derive(Clone, Copy)]
#[repr(C)]
pub struct JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
    pub BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION,
    pub IoInfo: IO_COUNTERS,
    pub ProcessMemoryLimit: usize,
    pub JobMemoryLimit: usize,
    pub PeakProcessMemoryUsed: usize,
    pub PeakJobMemoryUsed: usize,
}

// winnt.h: the same on every Windows target.
const _: () = {
    assert!(size_of::<IO_COUNTERS>() == 48 && align_of::<IO_COUNTERS>() == 8);
};

// processthreadsapi.h, winbase.h and winnt.h, x86-64 and arm64 Windows (LLP64, 8-byte
// pointers and `SIZE_T`): identical.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
const _: () = {
    use core::mem::offset_of;
    assert!(size_of::<PROCESS_INFORMATION>() == 24 && align_of::<PROCESS_INFORMATION>() == 8);
    assert!(offset_of!(PROCESS_INFORMATION, hProcess) == 0);
    assert!(offset_of!(PROCESS_INFORMATION, hThread) == 8);
    assert!(offset_of!(PROCESS_INFORMATION, dwProcessId) == 16);
    assert!(offset_of!(PROCESS_INFORMATION, dwThreadId) == 20);
    assert!(size_of::<STARTUPINFOW>() == 104 && align_of::<STARTUPINFOW>() == 8);
    assert!(offset_of!(STARTUPINFOW, cb) == 0);
    assert!(offset_of!(STARTUPINFOW, lpReserved) == 8);
    assert!(offset_of!(STARTUPINFOW, dwFlags) == 60);
    assert!(offset_of!(STARTUPINFOW, wShowWindow) == 64);
    assert!(offset_of!(STARTUPINFOW, lpReserved2) == 72);
    assert!(offset_of!(STARTUPINFOW, hStdInput) == 80);
    assert!(offset_of!(STARTUPINFOW, hStdOutput) == 88);
    assert!(offset_of!(STARTUPINFOW, hStdError) == 96);
    assert!(size_of::<STARTUPINFOEXW>() == 112 && align_of::<STARTUPINFOEXW>() == 8);
    assert!(offset_of!(STARTUPINFOEXW, lpAttributeList) == 104);
    assert!(size_of::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>() == 16);
    assert!(offset_of!(JOBOBJECT_ASSOCIATE_COMPLETION_PORT, CompletionKey) == 0);
    assert!(offset_of!(JOBOBJECT_ASSOCIATE_COMPLETION_PORT, CompletionPort) == 8);
    assert!(size_of::<JOBOBJECT_BASIC_LIMIT_INFORMATION>() == 64);
    assert!(align_of::<JOBOBJECT_BASIC_LIMIT_INFORMATION>() == 8);
    assert!(offset_of!(JOBOBJECT_BASIC_LIMIT_INFORMATION, LimitFlags) == 16);
    assert!(offset_of!(JOBOBJECT_BASIC_LIMIT_INFORMATION, MinimumWorkingSetSize) == 24);
    assert!(offset_of!(JOBOBJECT_BASIC_LIMIT_INFORMATION, ActiveProcessLimit) == 40);
    assert!(offset_of!(JOBOBJECT_BASIC_LIMIT_INFORMATION, Affinity) == 48);
    assert!(offset_of!(JOBOBJECT_BASIC_LIMIT_INFORMATION, PriorityClass) == 56);
    assert!(offset_of!(JOBOBJECT_BASIC_LIMIT_INFORMATION, SchedulingClass) == 60);
    assert!(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() == 144);
    assert!(offset_of!(JOBOBJECT_EXTENDED_LIMIT_INFORMATION, IoInfo) == 64);
    assert!(offset_of!(JOBOBJECT_EXTENDED_LIMIT_INFORMATION, ProcessMemoryLimit) == 112);
    assert!(offset_of!(JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobMemoryLimit) == 120);
    assert!(offset_of!(JOBOBJECT_EXTENDED_LIMIT_INFORMATION, PeakProcessMemoryUsed) == 128);
    assert!(offset_of!(JOBOBJECT_EXTENDED_LIMIT_INFORMATION, PeakJobMemoryUsed) == 136);
};

#[link(name = "kernel32", kind = "raw-dylib")]
unsafe extern "system" {
    /// `CreateProcessW` (processthreadsapi.h).
    ///
    /// replaces windows_sys::Win32::System::Threading::CreateProcessW
    pub fn CreateProcessW(
        lpapplicationname: PCWSTR,
        lpcommandline: PWSTR,
        lpprocessattributes: *const SECURITY_ATTRIBUTES,
        lpthreadattributes: *const SECURITY_ATTRIBUTES,
        binherithandles: Bool,
        dwcreationflags: PROCESS_CREATION_FLAGS,
        lpenvironment: *const c_void,
        lpcurrentdirectory: PCWSTR,
        lpstartupinfo: *const STARTUPINFOW,
        lpprocessinformation: *mut PROCESS_INFORMATION,
    ) -> Bool;
    /// `GetCurrentProcess` (processthreadsapi.h): the pseudo-handle -1, never closed.
    ///
    /// replaces windows_sys::Win32::System::Threading::GetCurrentProcess
    pub fn GetCurrentProcess() -> Handle;
    /// `GetExitCodeProcess` (processthreadsapi.h).
    ///
    /// replaces windows_sys::Win32::System::Threading::GetExitCodeProcess
    pub fn GetExitCodeProcess(hprocess: Handle, lpexitcode: *mut u32) -> Bool;
    /// `GetProcessId` (processthreadsapi.h): 0 on failure.
    ///
    /// replaces windows_sys::Win32::System::Threading::GetProcessId
    pub fn GetProcessId(process: Handle) -> u32;
    /// `TerminateProcess` (processthreadsapi.h).
    ///
    /// replaces windows_sys::Win32::System::Threading::TerminateProcess
    pub fn TerminateProcess(hprocess: Handle, uexitcode: u32) -> Bool;
    /// `ResumeThread` (processthreadsapi.h): the previous suspend count, `u32::MAX` on failure.
    ///
    /// replaces windows_sys::Win32::System::Threading::ResumeThread
    pub fn ResumeThread(hthread: Handle) -> u32;
    /// `InitializeProcThreadAttributeList` (processthreadsapi.h).
    ///
    /// replaces windows_sys::Win32::System::Threading::InitializeProcThreadAttributeList
    pub fn InitializeProcThreadAttributeList(
        lpattributelist: LPPROC_THREAD_ATTRIBUTE_LIST,
        dwattributecount: u32,
        dwflags: u32,
        lpsize: *mut usize,
    ) -> Bool;
    /// `UpdateProcThreadAttribute` (processthreadsapi.h).
    ///
    /// replaces windows_sys::Win32::System::Threading::UpdateProcThreadAttribute
    pub fn UpdateProcThreadAttribute(
        lpattributelist: LPPROC_THREAD_ATTRIBUTE_LIST,
        dwflags: u32,
        attribute: usize,
        lpvalue: *const c_void,
        cbsize: usize,
        lppreviousvalue: *mut c_void,
        lpreturnsize: *const usize,
    ) -> Bool;
    /// `DeleteProcThreadAttributeList` (processthreadsapi.h).
    ///
    /// replaces windows_sys::Win32::System::Threading::DeleteProcThreadAttributeList
    pub fn DeleteProcThreadAttributeList(lpattributelist: LPPROC_THREAD_ATTRIBUTE_LIST);
    /// `WaitForSingleObject` (synchapi.h).
    ///
    /// replaces windows_sys::Win32::System::Threading::WaitForSingleObject
    pub fn WaitForSingleObject(hhandle: Handle, dwmilliseconds: u32) -> WAIT_EVENT;
    /// `Sleep` (synchapi.h).
    ///
    /// replaces windows_sys::Win32::System::Threading::Sleep
    pub fn Sleep(dwmilliseconds: u32);
    /// `RegisterWaitForSingleObject` (winbase.h): a thread-pool wait on `hobject`.
    ///
    /// replaces windows_sys::Win32::System::Threading::RegisterWaitForSingleObject
    pub fn RegisterWaitForSingleObject(
        phnewwaitobject: *mut Handle,
        hobject: Handle,
        callback: WAITORTIMERCALLBACK,
        context: *const c_void,
        dwmilliseconds: u32,
        dwflags: WORKER_THREAD_FLAGS,
    ) -> Bool;
    /// `UnregisterWaitEx` (winbase.h).
    ///
    /// replaces windows_sys::Win32::System::Threading::UnregisterWaitEx
    pub fn UnregisterWaitEx(waithandle: Handle, completionevent: Handle) -> Bool;
    /// `ConvertThreadToFiberEx` (winbase.h): the thread's fiber, null on failure.
    ///
    /// replaces windows_sys::Win32::System::Threading::ConvertThreadToFiberEx
    pub fn ConvertThreadToFiberEx(lpparameter: *const c_void, dwflags: u32) -> *mut c_void;
    /// `ConvertFiberToThread` (winbase.h).
    ///
    /// replaces windows_sys::Win32::System::Threading::ConvertFiberToThread
    pub fn ConvertFiberToThread() -> Bool;
    /// `CreateFiberEx` (winbase.h): a new fiber, null on failure.
    ///
    /// replaces windows_sys::Win32::System::Threading::CreateFiberEx
    pub fn CreateFiberEx(
        dwstackcommitsize: usize,
        dwstackreservesize: usize,
        dwflags: u32,
        lpstartaddress: LPFIBER_START_ROUTINE,
        lpparameter: *const c_void,
    ) -> *mut c_void;
    /// `DeleteFiber` (winbase.h).
    ///
    /// replaces windows_sys::Win32::System::Threading::DeleteFiber
    pub fn DeleteFiber(lpfiber: *const c_void);
    /// `SwitchToFiber` (winbase.h).
    ///
    /// replaces windows_sys::Win32::System::Threading::SwitchToFiber
    pub fn SwitchToFiber(lpfiber: *const c_void);
    /// `IsThreadAFiber` (fibersapi.h).
    ///
    /// replaces windows_sys::Win32::System::Threading::IsThreadAFiber
    pub fn IsThreadAFiber() -> Bool;
    /// `GetNumaHighestNodeNumber` (systemtopologyapi.h).
    ///
    /// replaces windows_sys::Win32::System::Threading::GetNumaHighestNodeNumber
    pub fn GetNumaHighestNodeNumber(highestnodenumber: *mut u32) -> Bool;

    /// `CreateJobObjectW` (jobapi2.h): null on failure.
    ///
    /// replaces windows_sys::Win32::System::JobObjects::CreateJobObjectW
    pub fn CreateJobObjectW(lpjobattributes: *const SECURITY_ATTRIBUTES, lpname: PCWSTR) -> Handle;
    /// `AssignProcessToJobObject` (jobapi2.h).
    ///
    /// replaces windows_sys::Win32::System::JobObjects::AssignProcessToJobObject
    pub fn AssignProcessToJobObject(hjob: Handle, hprocess: Handle) -> Bool;
    /// `QueryInformationJobObject` (jobapi2.h).
    ///
    /// replaces windows_sys::Win32::System::JobObjects::QueryInformationJobObject
    pub fn QueryInformationJobObject(
        hjob: Handle,
        jobobjectinformationclass: JOBOBJECTINFOCLASS,
        lpjobobjectinformation: *mut c_void,
        cbjobobjectinformationlength: u32,
        lpreturnlength: *mut u32,
    ) -> Bool;
    /// `SetInformationJobObject` (jobapi2.h).
    ///
    /// replaces windows_sys::Win32::System::JobObjects::SetInformationJobObject
    pub fn SetInformationJobObject(
        hjob: Handle,
        jobobjectinformationclass: JOBOBJECTINFOCLASS,
        lpjobobjectinformation: *const c_void,
        cbjobobjectinformationlength: u32,
    ) -> Bool;
    /// `TerminateJobObject` (jobapi2.h).
    ///
    /// replaces windows_sys::Win32::System::JobObjects::TerminateJobObject
    pub fn TerminateJobObject(hjob: Handle, uexitcode: u32) -> Bool;

    /// `CreateEventW` (synchapi.h): [`Handle::NULL`] on failure.
    ///
    /// replaces windows_sys::Win32::System::Threading::CreateEventW
    pub fn CreateEventW(
        lpeventattributes: *const SECURITY_ATTRIBUTES,
        bmanualreset: Bool,
        binitialstate: Bool,
        lpname: PCWSTR,
    ) -> Handle;
    /// `SetEvent` (synchapi.h).
    ///
    /// replaces windows_sys::Win32::System::Threading::SetEvent
    pub fn SetEvent(hevent: Handle) -> Bool;
    /// `WaitForMultipleObjects` (synchapi.h): `WAIT_OBJECT_0 + i` for the first signaled.
    ///
    /// replaces windows_sys::Win32::System::Threading::WaitForMultipleObjects
    pub fn WaitForMultipleObjects(
        ncount: u32,
        lphandles: *const Handle,
        bwaitall: Bool,
        dwmilliseconds: u32,
    ) -> WAIT_EVENT;
    /// `GetCurrentThreadId` (processthreadsapi.h).
    ///
    /// replaces windows_sys::Win32::System::Threading::GetCurrentThreadId
    pub fn GetCurrentThreadId() -> u32;
    /// `OpenProcess` (processthreadsapi.h): [`Handle::NULL`] on failure.
    ///
    /// replaces windows_sys::Win32::System::Threading::OpenProcess
    pub fn OpenProcess(dwdesiredaccess: PROCESS_ACCESS_RIGHTS, binherithandle: Bool, dwprocessid: u32) -> Handle;
}

// windows-sys links these through the API set the Win32 reference names (kernelbase's exports).
#[link(name = "api-ms-win-core-synch-l1-2-0", kind = "raw-dylib")]
unsafe extern "system" {
    /// `WaitOnAddress` (synchapi.h): sleeps while the `addresssize` bytes at `address` equal
    /// those at `compareaddress`, until woken or the time runs out (false, `ERROR_TIMEOUT`).
    ///
    /// replaces windows_sys::Win32::System::Threading::WaitOnAddress
    pub fn WaitOnAddress(
        address: *const c_void,
        compareaddress: *const c_void,
        addresssize: usize,
        dwmilliseconds: u32,
    ) -> Bool;
    /// `WakeByAddressAll` (synchapi.h): wakes every thread in `WaitOnAddress` on `address`.
    ///
    /// replaces windows_sys::Win32::System::Threading::WakeByAddressAll
    pub fn WakeByAddressAll(address: *const c_void);
}

#[link(name = "advapi32", kind = "raw-dylib")]
unsafe extern "system" {
    /// `OpenProcessToken` (processthreadsapi.h; advapi32 per the API reference).
    ///
    /// replaces windows_sys::Win32::System::Threading::OpenProcessToken
    pub fn OpenProcessToken(
        processhandle: Handle,
        desiredaccess: TOKEN_ACCESS_MASK,
        tokenhandle: *mut Handle,
    ) -> Bool;
}

/// How a [`wait_for_single_object`] ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wait {
    /// `WAIT_OBJECT_0`: the object is signaled.
    Signaled,
    /// `WAIT_ABANDONED`: a mutex whose owner thread exited holding it; the caller now owns it.
    Abandoned,
    /// `WAIT_TIMEOUT`: the time ran out first.
    TimedOut,
}

/// `WaitForSingleObject` on `handle` for up to `timeout_ms` milliseconds ([`INFINITE`] waits
/// without end, which a caller passes only where its own deadline is elsewhere). `WAIT_FAILED`
/// is the thread's last error; a return the API does not document for a non-alertable wait is
/// passed on as `WinError` of that raw value, never read as one of the three.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn wait_for_single_object(handle: Handle, timeout_ms: u32) -> Result<Wait, WinError> {
    // SAFETY: the call takes a handle value and a number and reads no memory of ours; a handle
    // that is not a waitable object is the call's own error (`WAIT_FAILED`), not undefined.
    match unsafe { WaitForSingleObject(handle, timeout_ms) } {
        WAIT_OBJECT_0 => Ok(Wait::Signaled),
        WAIT_ABANDONED => Ok(Wait::Abandoned),
        WAIT_TIMEOUT => Ok(Wait::TimedOut),
        WAIT_FAILED => Err(WinError::last()),
        other => Err(WinError(other)),
    }
}

/// `GetCurrentProcess`: this process's pseudo-handle ([`Handle::INVALID`]'s value), valid
/// everywhere in this process and never closed.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn get_current_process() -> Handle {
    // SAFETY: the call takes nothing and returns a constant pseudo-handle.
    unsafe { GetCurrentProcess() }
}

/// `Sleep` for `ms` milliseconds ([`INFINITE`] never returns).
#[blaze::noalloc]
#[blaze::nopanic]
pub fn sleep(ms: u32) {
    // SAFETY: the call takes a number and touches no memory of ours.
    unsafe { Sleep(ms) }
}

/// `GetNumaHighestNodeNumber`: the highest NUMA node number (0 on a machine with one node).
#[blaze::noalloc]
#[blaze::nopanic]
pub fn numa_highest_node() -> Result<u32, WinError> {
    let mut highest = 0u32;
    // SAFETY: `highest` is a live `u32` the call writes; it keeps no pointer.
    unsafe { GetNumaHighestNodeNumber(&raw mut highest) }.ok()?;
    Ok(highest)
}
