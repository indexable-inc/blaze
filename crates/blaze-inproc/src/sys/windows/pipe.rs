//! The channel's transport: a named pipe `\\.\pipe\blaze-inproc-<pid>-<random>`, in byte mode,
//! opened overlapped at both ends so a read can time out. Its DACL lets this user do anything
//! and lets AppContainer processes (the sandbox's) of this user only connect, read and write;
//! its label lets a low-integrity process (an AppContainer's) write. Remote clients are refused,
//! the first instance is made with `FILE_FLAG_FIRST_PIPE_INSTANCE` (no process squats the name
//! before the daemon), and every peer's token is checked to be this user's.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_NO_DATA, ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
    ERROR_PIPE_NOT_CONNECTED, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::{
    ACL, ACL_REVISION, AddAccessAllowedAce, AddMandatoryAce, CopySid, CreateWellKnownSid, EqualSid,
    GetTokenInformation, InitializeAcl, InitializeSecurityDescriptor, PSID, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
    SECURITY_MAX_SID_SIZE, SetSecurityDescriptorDacl, SetSecurityDescriptorSacl, TOKEN_QUERY, TOKEN_USER, TokenUser,
    WELL_KNOWN_SID_TYPE, WinBuiltinAnyPackageSid, WinLowLabelSid,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ALL_ACCESS, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_READ_ATTRIBUTES,
    FILE_READ_DATA, FILE_WRITE_DATA, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile,
    SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, SYNCHRONIZE, WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PeekNamedPipe,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
};
use windows_sys::Win32::System::SystemServices::{SECURITY_DESCRIPTOR_REVISION, SYSTEM_MANDATORY_LABEL_NO_WRITE_UP};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, INFINITE, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    WaitForSingleObject,
};
use windows_sys::core::BOOL;

use super::os;
use crate::Error;

const PIPES: &str = r"\\.\pipe\";
const PREFIX: &str = "blaze-inproc-";
/// Each direction's buffer in the pipe: a frame of a stub miss fits many times.
const BUFFER: u32 = 64 << 10;
/// How long a start hook keeps trying while every instance is busy.
const CONNECTING: Duration = Duration::from_secs(30);
/// What an AppContainer process of this user may do with the pipe: connect, read and write,
/// never make an instance of it (`FILE_APPEND_DATA`, which would let it answer start hooks).
const CONNECT: u32 = FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE;

fn check(ok: BOOL, what: &str) -> Result<(), Error> {
    if ok == 0 { Err(os(what)) } else { Ok(()) }
}

fn event() -> Result<OwnedHandle, Error> {
    // SAFETY: an unnamed manual-reset event, not signaled; value-only arguments.
    let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if event.is_null() {
        return Err(os("CreateEventW for a pipe's transfers"));
    }
    // SAFETY: just made, owned by nothing else.
    Ok(unsafe { OwnedHandle::from_raw_handle(event) })
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

fn hung_up(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error().map(|it| it as u32),
        Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED | ERROR_NO_DATA)
    )
}

/// One end of a connected pipe.
pub(crate) struct Stream {
    pipe: OwnedHandle,
    event: OwnedHandle,
    /// Milliseconds a transfer may take; `INFINITE` for ever.
    timeout: u32,
}

impl Stream {
    fn new(pipe: OwnedHandle) -> Result<Stream, Error> {
        Ok(Stream { pipe, event: event()?, timeout: INFINITE })
    }

    pub(crate) fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        let at = buf.as_mut_ptr();
        // SAFETY: `buf` is valid for `len` bytes and stays borrowed until `transfer` returns,
        // which waits until the read is over.
        let done = self.transfer("ReadFile on the channel's pipe", |pipe, overlapped| unsafe {
            ReadFile(pipe, at, len, std::ptr::null_mut(), overlapped)
        })?;
        Ok(done.unwrap_or(0) as usize)
    }

    pub(crate) fn write_all(&mut self, mut bytes: &[u8]) -> Result<(), Error> {
        while !bytes.is_empty() {
            let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
            let at = bytes.as_ptr();
            // SAFETY: as in `read`, for a write of `bytes`.
            let done = self.transfer("WriteFile on the channel's pipe", |pipe, overlapped| unsafe {
                WriteFile(pipe, at, len, std::ptr::null_mut(), overlapped)
            })?;
            match done {
                None | Some(0) => return Err(Error::new("the other end of the channel hung up")),
                Some(wrote) => bytes = &bytes[(wrote as usize).min(bytes.len())..],
            }
        }
        Ok(())
    }

    /// Starts one overlapped transfer and waits until it is over: the bytes it moved, `None`
    /// when the other end hung up. A transfer still running when the timeout passes is
    /// cancelled, and waited for, before this returns: the system never touches the buffer or
    /// the `OVERLAPPED` after.
    fn transfer(&mut self, what: &str, start: impl FnOnce(HANDLE, *mut OVERLAPPED) -> BOOL) -> Result<Option<u32>, Error> {
        let pipe = self.pipe.as_raw_handle();
        let mut overlapped = OVERLAPPED { hEvent: self.event.as_raw_handle(), ..Default::default() };
        if start(pipe, &mut overlapped) == 0 {
            let error = std::io::Error::last_os_error();
            if hung_up(&error) {
                return Ok(None);
            }
            if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                return Err(Error::new(format!("Windows: {what}: {error}")));
            }
        }
        // SAFETY: waits on the event this stream owns.
        let waited = unsafe { WaitForSingleObject(self.event.as_raw_handle(), self.timeout) };
        let timed_out = waited == WAIT_TIMEOUT;
        if waited != WAIT_OBJECT_0 {
            // SAFETY: cancels the transfer `overlapped` names, if it is still running.
            unsafe { CancelIoEx(pipe, &overlapped) };
        }
        let mut done = 0u32;
        // SAFETY: waits until the transfer `overlapped` names is over, done, failed or
        // cancelled; writes only `done`.
        if unsafe { GetOverlappedResult(pipe, &overlapped, &mut done, 1) } != 0 {
            return Ok(Some(done));
        }
        let error = std::io::Error::last_os_error();
        if hung_up(&error) {
            return Ok(None);
        }
        if timed_out && error.raw_os_error() == Some(ERROR_OPERATION_ABORTED as i32) {
            return Err(Error::new(format!("{what}: nothing within {} ms", self.timeout)));
        }
        Err(Error::new(format!("Windows: {what}: {error}")))
    }

    /// Whether the other end hung up; it waits for nothing.
    pub(super) fn closed(&self) -> bool {
        let mut available = 0u32;
        // SAFETY: peeks no bytes, writes only `available`; the pipe is open while `self` is.
        let ok = unsafe {
            PeekNamedPipe(self.pipe.as_raw_handle(), std::ptr::null_mut(), 0, std::ptr::null_mut(), &mut available, std::ptr::null_mut())
        };
        ok == 0 && hung_up(&std::io::Error::last_os_error())
    }

    pub(crate) fn set_timeout(&mut self, timeout: Option<Duration>) -> Result<(), Error> {
        self.timeout = match timeout {
            None => INFINITE,
            Some(timeout) => u32::try_from(timeout.as_millis()).unwrap_or(INFINITE).min(INFINITE - 1),
        };
        Ok(())
    }
}

/// The daemon's end: instances of one pipe name, one always waiting for the next start hook.
pub(crate) struct Listener {
    name: Vec<u16>,
    security: Security,
    next: Mutex<Option<OwnedHandle>>,
    event: OwnedHandle,
    user: Sid,
}

impl Listener {
    /// A pipe of a name no one can guess. `dir` holds nothing on Windows: pipes live in the
    /// pipe namespace, and the DACL is what keeps others out.
    pub(crate) fn bind(dir: &Path) -> Result<(Listener, String), Error> {
        let _ = dir;
        let mut token = [0u8; 16];
        super::random(&mut token)?;
        let address = format!("{PIPES}{PREFIX}{}-{}", std::process::id(), crate::handshake::hex(&token));
        let name = wide(&address);
        let user = Sid::current_user()?;
        let security = Security::new(&user)?;
        let first = instance(&name, &security, true)?;
        let listener = Listener { name, security, next: Mutex::new(Some(first)), event: event()?, user };
        Ok((listener, address))
    }

    pub(crate) fn accept(&self) -> Result<(Stream, u32), Error> {
        let waiting = self.next.lock().take();
        let pipe = match waiting {
            Some(pipe) => pipe,
            None => instance(&self.name, &self.security, false)?,
        };
        wait_for_client(&pipe, &self.event)?;
        // The next instance waits before this one is handed out, so a start hook never finds none.
        // Where it can't be made, the next `accept` makes it and says why.
        if let Ok(next) = instance(&self.name, &self.security, false) {
            *self.next.lock() = Some(next);
        }
        let mut pid = 0;
        // SAFETY: a query of the pipe this listener owns; writes only `pid`.
        check(unsafe { GetNamedPipeClientProcessId(pipe.as_raw_handle(), &mut pid) }, "GetNamedPipeClientProcessId")?;
        let peer = Sid::of_process(pid)?;
        if !peer.equal(&self.user) {
            return Err(Error::new(format!("process {pid} of another user connected to blaze-inproc's pipe: refused")));
        }
        Ok((Stream::new(pipe)?, pid))
    }
}

fn instance(name: &[u16], security: &Security, first: bool) -> Result<OwnedHandle, Error> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security.descriptor(),
        bInheritHandle: 0,
    };
    let mut mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
    if first {
        mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    // SAFETY: `name` is NUL-terminated and `attributes` names a descriptor that lives as long as
    // `security`; both outlive the call. The handle it returns is checked and owned below.
    let pipe = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            BUFFER,
            BUFFER,
            0,
            &attributes,
        )
    };
    if pipe == INVALID_HANDLE_VALUE {
        return Err(os("CreateNamedPipeW of blaze-inproc's pipe"));
    }
    // SAFETY: just made, owned by nothing else.
    Ok(unsafe { OwnedHandle::from_raw_handle(pipe) })
}

fn wait_for_client(pipe: &OwnedHandle, event: &OwnedHandle) -> Result<(), Error> {
    let mut overlapped = OVERLAPPED { hEvent: event.as_raw_handle(), ..Default::default() };
    // SAFETY: starts waiting for a client on `pipe`; `overlapped` lives until the wait below is
    // over.
    if unsafe { ConnectNamedPipe(pipe.as_raw_handle(), &mut overlapped) } != 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error().map(|it| it as u32) {
        Some(ERROR_PIPE_CONNECTED) => return Ok(()),
        Some(ERROR_IO_PENDING) => {}
        _ => return Err(Error::new(format!("Windows: ConnectNamedPipe: {error}"))),
    }
    let mut done = 0;
    // SAFETY: waits until the connect `overlapped` names is over; then the system no longer
    // touches it.
    check(unsafe { GetOverlappedResult(pipe.as_raw_handle(), &overlapped, &mut done, 1) }, "ConnectNamedPipe")
}

/// The start hook's end, of the pipe `address` ([`Listener::bind`]).
pub(crate) fn connect(address: &str) -> Result<Stream, Error> {
    if !address.strip_prefix(PIPES).is_some_and(|it| it.starts_with(PREFIX)) {
        return Err(Error::new(format!("{address} is not a pipe of blaze-inproc's")));
    }
    let name = wide(address);
    let deadline = Instant::now() + CONNECTING;
    loop {
        // SAFETY: `name` is NUL-terminated; no security attributes or template. The server may
        // only identify this client, never act as it (`SECURITY_IDENTIFICATION`).
        let pipe = unsafe {
            CreateFileW(
                name.as_ptr(),
                CONNECT,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                std::ptr::null_mut(),
            )
        };
        if pipe != INVALID_HANDLE_VALUE {
            // SAFETY: just opened, owned by nothing else.
            return Stream::new(unsafe { OwnedHandle::from_raw_handle(pipe) });
        }
        let error = std::io::Error::last_os_error();
        let left = deadline.saturating_duration_since(Instant::now());
        if error.raw_os_error() != Some(ERROR_PIPE_BUSY as i32) || left.is_zero() {
            return Err(Error::new(format!("Windows: CreateFileW of {address}: {error}")));
        }
        let wait = u32::try_from(left.as_millis()).unwrap_or(INFINITE - 1).min(INFINITE - 1);
        // SAFETY: `name` is NUL-terminated.
        unsafe { WaitNamedPipeW(name.as_ptr(), wait) };
    }
}

const SID_WORDS: usize = (SECURITY_MAX_SID_SIZE as usize).div_ceil(4);

/// A SID in a buffer of its own, as long as the longest and aligned as a SID's fields are.
struct Sid(Box<[u32; SID_WORDS]>);

impl Sid {
    fn ptr(&self) -> PSID {
        self.0.as_ptr().cast_mut().cast()
    }

    fn well_known(kind: WELL_KNOWN_SID_TYPE) -> Result<Sid, Error> {
        let mut sid = Sid(Box::new([0; SID_WORDS]));
        let mut size = SECURITY_MAX_SID_SIZE;
        // SAFETY: writes at most `size` bytes into the buffer, which has that many.
        check(
            unsafe { CreateWellKnownSid(kind, std::ptr::null_mut(), sid.0.as_mut_ptr().cast(), &mut size) },
            "CreateWellKnownSid",
        )?;
        Ok(sid)
    }

    fn current_user() -> Result<Sid, Error> {
        // SAFETY: GetCurrentProcess returns a pseudo-handle, valid always.
        token_user(unsafe { GetCurrentProcess() })
    }

    fn of_process(pid: u32) -> Result<Sid, Error> {
        // SAFETY: value-only arguments; the handle it returns is checked and owned below.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return Err(os(format_args!("OpenProcess of the peer {pid}")));
        }
        // SAFETY: just opened, owned by nothing else.
        let process = unsafe { OwnedHandle::from_raw_handle(process) };
        token_user(process.as_raw_handle())
    }

    fn equal(&self, other: &Sid) -> bool {
        // SAFETY: both are SIDs CopySid or CreateWellKnownSid wrote.
        unsafe { EqualSid(self.ptr(), other.ptr()) != 0 }
    }
}

/// The user of the process `process` names.
fn token_user(process: HANDLE) -> Result<Sid, Error> {
    let mut token = std::ptr::null_mut();
    // SAFETY: opens the token of a process handle the caller holds; writes only `token`.
    check(unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }, "OpenProcessToken")?;
    // SAFETY: just opened, owned by nothing else.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut buffer = [0u64; (size_of::<TOKEN_USER>() + SECURITY_MAX_SID_SIZE as usize).div_ceil(8)];
    let mut len = 0;
    // SAFETY: writes at most the buffer's size into it; the buffer is 8-aligned, as TOKEN_USER is.
    check(
        unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                buffer.as_mut_ptr().cast(),
                size_of_val(&buffer) as u32,
                &mut len,
            )
        },
        "GetTokenInformation of the token's user",
    )?;
    // SAFETY: the call wrote a TOKEN_USER at the start of the buffer, whose SID points into it.
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut sid = Sid(Box::new([0; SID_WORDS]));
    // SAFETY: copies the SID the token named, valid per the call above, into a buffer of
    // `SECURITY_MAX_SID_SIZE` bytes.
    check(
        unsafe { CopySid(SECURITY_MAX_SID_SIZE, sid.0.as_mut_ptr().cast(), user.User.Sid) },
        "CopySid of the token's user",
    )?;
    Ok(sid)
}

/// The pipe's security descriptor and the ACLs it points into.
struct Security {
    descriptor: Box<SECURITY_DESCRIPTOR>,
    _dacl: Box<[u64; 32]>,
    _sacl: Box<[u64; 16]>,
}

// SAFETY: the descriptor's only pointers are into the ACL buffers it owns, boxed so they never
// move and never written after `Security::new`; the system only reads them.
unsafe impl Send for Security {}
// SAFETY: as above: nothing writes any of it after `Security::new`.
unsafe impl Sync for Security {}

impl Security {
    fn new(user: &Sid) -> Result<Security, Error> {
        let packages = Sid::well_known(WinBuiltinAnyPackageSid)?;
        let low = Sid::well_known(WinLowLabelSid)?;
        let mut dacl = Box::new([0u64; 32]);
        let mut sacl = Box::new([0u64; 16]);
        let dacl_at = dacl.as_mut_ptr().cast::<ACL>();
        let sacl_at = sacl.as_mut_ptr().cast::<ACL>();
        let (dacl_len, sacl_len) = (size_of_val(&*dacl) as u32, size_of_val(&*sacl) as u32);
        // SAFETY: initializes and fills ACLs in 8-aligned buffers of the sizes given (two allowed
        // ACEs and one label ACE, of SIDs of at most `SECURITY_MAX_SID_SIZE` bytes, fit in them),
        // copying the SIDs it is given, which are valid (made above).
        unsafe {
            check(InitializeAcl(dacl_at, dacl_len, ACL_REVISION), "InitializeAcl")?;
            check(AddAccessAllowedAce(dacl_at, ACL_REVISION, FILE_ALL_ACCESS, user.ptr()), "AddAccessAllowedAce")?;
            check(AddAccessAllowedAce(dacl_at, ACL_REVISION, CONNECT, packages.ptr()), "AddAccessAllowedAce")?;
            check(InitializeAcl(sacl_at, sacl_len, ACL_REVISION), "InitializeAcl")?;
            check(
                AddMandatoryAce(sacl_at, ACL_REVISION, 0, SYSTEM_MANDATORY_LABEL_NO_WRITE_UP, low.ptr()),
                "AddMandatoryAce",
            )?;
        }
        let mut descriptor = Box::new(SECURITY_DESCRIPTOR::default());
        let at = (&mut *descriptor as *mut SECURITY_DESCRIPTOR).cast();
        // SAFETY: sets up the absolute descriptor in its box, pointing at the ACLs above, which
        // move with it into `Security` (their heap buffers stay put) and outlive every use.
        unsafe {
            check(InitializeSecurityDescriptor(at, SECURITY_DESCRIPTOR_REVISION), "InitializeSecurityDescriptor")?;
            check(SetSecurityDescriptorDacl(at, 1, dacl_at, 0), "SetSecurityDescriptorDacl")?;
            check(SetSecurityDescriptorSacl(at, 1, sacl_at, 0), "SetSecurityDescriptorSacl")?;
        }
        Ok(Security { descriptor, _dacl: dacl, _sacl: sacl })
    }

    fn descriptor(&self) -> *mut std::ffi::c_void {
        (&*self.descriptor as *const SECURITY_DESCRIPTOR).cast_mut().cast()
    }
}