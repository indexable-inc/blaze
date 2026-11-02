//! Whose a directory, a socket and a peer are on Windows: by security identifier, where
//! Unix has a user id and a mode. std and `uds_windows` have none of this, so each call is
//! Win32's own; the functions of the module are safe, and the pointers the system hands back
//! are kept by what frees them ([`Local`], [`Sid`]) for as long as they are read.

use std::{
    ffi::c_void,
    io,
    os::windows::{
        ffi::OsStrExt as _,
        io::{AsRawHandle as _, AsRawSocket as _, FromRawHandle as _, OwnedHandle},
    },
    path::Path,
    ptr,
};

use uds_windows::UnixStream;
use windows_sys::Win32::{
    Foundation::{ERROR_SUCCESS, HANDLE, LocalFree},
    Networking::WinSock::{SIO_AF_UNIX_GETPEERPID, WSAGetLastError, WSAIoctl},
    Security::{
        ACCESS_ALLOWED_ACE, ACL,
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
        },
        DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetSecurityDescriptorDacl,
        GetTokenInformation, IsWellKnownSid, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, TOKEN_INFORMATION_CLASS,
        TOKEN_QUERY, TokenOwner, TokenUser, WinBuiltinAdministratorsSid, WinCreatorOwnerRightsSid,
        WinLocalSystemSid,
    },
    System::SystemServices::{ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE},
    System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    },
};

/// A security identifier of a token, in the buffer the system wrote it to. Both
/// `TOKEN_USER` and `TOKEN_OWNER` begin with the pointer to it, which points into the same
/// buffer: a `Vec`'s heap does not move with it, so the pointer holds while this lives.
struct Sid(Vec<usize>);

impl Sid {
    fn of(token: &OwnedHandle, class: TOKEN_INFORMATION_CLASS) -> io::Result<Sid> {
        // The pointer, the attributes, and the longest identifier there is, of 68 bytes.
        let mut buffer = vec![0usize; 16];
        let mut len = 0;
        // SAFETY: the buffer is as long as is said and aligned for pointers, as `TOKEN_USER`
        // and `TOKEN_OWNER` are; a length past it would have the call write past its end. One
        // too short is refused. The token is open for as long as the borrow.
        let ok = unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                class,
                buffer.as_mut_ptr().cast(),
                size_of_val(&buffer[..]) as u32,
                &raw mut len,
            )
        };
        if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(Sid(buffer)) }
    }

    fn as_ptr(&self) -> PSID {
        self.0[0] as PSID
    }

    /// Whether `other`, an identifier the system wrote and that is alive for the call, is this.
    fn is(&self, other: PSID) -> bool {
        // SAFETY: this identifier is alive while `self` is ([`Sid`]), and `other` is by the
        // caller's word, which every caller here keeps by holding what `other` points into; a
        // freed one would be read after it is freed.
        unsafe { EqualSid(self.as_ptr(), other) != 0 }
    }

    /// As access control lists are written with it: `S-1-5-21-…`.
    fn written(&self) -> io::Result<String> {
        let mut text: *mut u16 = ptr::null_mut();
        // SAFETY: the identifier is alive while `self` is, and `text` is a live pointer for the
        // call to write; what it points to then is freed by the `LocalFree` below, once.
        if unsafe { ConvertSidToStringSidW(self.as_ptr(), &raw mut text) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let len = (0..)
            .take_while(|&at| {
                // SAFETY: `text` is the zero-ended UTF-16 `ConvertSidToStringSidW` allocated, and
                // `take_while` stops at its zero, so `at` never passes the end of it.
                let unit = unsafe { text.add(at) };
                // SAFETY: `unit` points at a `u16` of that allocation, which lives until the
                // `LocalFree` below.
                (unsafe { *unit }) != 0
            })
            .count();
        // SAFETY: the `len` units before the zero are initialized `u16`s of the allocation, which is
        // not written to or freed until the `LocalFree` below.
        let written = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
        // SAFETY: the system allocated `text` for the caller to free with `LocalFree`, and this is
        // its one free, after its last read; a second free would corrupt the heap.
        unsafe { LocalFree(text.cast()) };
        Ok(written)
    }
}

/// What the system allocated for a call and `LocalFree` gives back.
struct Local(*mut c_void);

impl Drop for Local {
    fn drop(&mut self) {
        // SAFETY: it is the system's allocation, which `LocalFree` gives back, and this is its
        // one owner, so it is freed once; a second free would corrupt the heap. A null one is
        // freed as nothing.
        unsafe { LocalFree(self.0) };
    }
}

/// The token of `process`, which the caller keeps open.
fn token_of(process: HANDLE) -> io::Result<OwnedHandle> {
    let mut token = ptr::null_mut();
    // SAFETY: `process` is open for the call by the caller's word, which both callers keep
    // (the process's own handle, or one an `OwnedHandle` holds); `token` is a live pointer for
    // the call to write, and the token is closed by what is returned.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the handle is open and nothing else owns it: `OpenProcessToken` just made it.
    // Were it owned twice, it would be closed twice, the second time maybe another's handle.
    Ok(unsafe { OwnedHandle::from_raw_handle(token) })
}

fn own_token() -> io::Result<OwnedHandle> {
    // SAFETY: it takes nothing, and the handle it gives is the process's own, which is never
    // closed, so it is open for as long as it is used.
    let process = unsafe { GetCurrentProcess() };
    token_of(process)
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain([0]).collect()
}

fn failed(status: u32) -> io::Result<()> {
    if status == ERROR_SUCCESS { Ok(()) } else { Err(io::Error::from_raw_os_error(status as i32)) }
}

/// Whether `path` is this user's by its owner, as [`the_users_alone`] reads it.
pub fn owned_by_user(path: &Path) -> io::Result<bool> {
    let (mut owner, mut descriptor) = (ptr::null_mut(), ptr::null_mut());
    // SAFETY: the path ends with a zero and lives for the statement, so it is read no further;
    // `owner` and `descriptor` are live pointers for the call to write, and the descriptor
    // `owner` points into is freed by `_descriptor` after `owner`'s last use. Freed before, the
    // comparisons below would read freed memory.
    failed(unsafe {
        GetNamedSecurityInfoW(
            wide(path).as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &raw mut owner,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &raw mut descriptor,
        )
    })?;
    let _descriptor = Local(descriptor);
    let token = own_token()?;
    Ok(Sid::of(&token, TokenUser)?.is(owner) || Sid::of(&token, TokenOwner)?.is(owner))
}

/// Refuses a `path` that this user did not make, or that its access control list opens
/// to anyone but this user, the system and the administrators, who can take it anyway.
/// The rights of the owner, whoever that is, are this user's once the owner is.
/// What an administrator makes is owned by the group of administrators, which is the
/// owner of the token then.
pub fn the_users_alone(path: &Path) -> io::Result<()> {
    let (mut owner, mut descriptor) = (ptr::null_mut(), ptr::null_mut());
    let mut list: *mut ACL = ptr::null_mut();
    // SAFETY: the path ends with a zero and lives for the statement, so it is read no further;
    // `owner`, `list` and `descriptor` are live pointers for the call to write, and the
    // descriptor `owner` and `list` point into is freed by `_descriptor` after their last use.
    // Freed before, the reads of the list below would read freed memory.
    failed(unsafe {
        GetNamedSecurityInfoW(
            wide(path).as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &raw mut owner,
            ptr::null_mut(),
            &raw mut list,
            ptr::null_mut(),
            &raw mut descriptor,
        )
    })?;
    let _descriptor = Local(descriptor);
    let refused = |why: &str| {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{} {why}", path.display())))
    };
    let token = own_token()?;
    let user = Sid::of(&token, TokenUser)?;
    if !user.is(owner) && !Sid::of(&token, TokenOwner)?.is(owner) {
        return refused("belongs to another user");
    }
    // No list at all lets everyone in.
    if list.is_null() {
        return refused("can be opened by other users (it has no access control list)");
    }
    // SAFETY: the list is not null (checked above) and is the one the system wrote into the
    // descriptor `_descriptor` keeps alive, whose header says how many entries it has.
    let entries = unsafe { (*list).AceCount };
    for at in 0..u32::from(entries) {
        let mut entry: *mut c_void = ptr::null_mut();
        // SAFETY: the list is alive (above), `at` is below its count, so the call reads only
        // its entries, and `entry` is a live pointer for it to write.
        if unsafe { GetAce(list, at, &raw mut entry) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let entry = entry.cast::<ACCESS_ALLOWED_ACE>();
        // SAFETY: `GetAce` set `entry` to an entry of `list`, which lives in `descriptor` until
        // `_descriptor` drops at the end of this function; every entry begins with an `ACE_HEADER`,
        // so this reads only the header.
        let kind = unsafe { (*entry).Header.AceType };
        // SAFETY: the same entry; `&raw mut` makes no reference, and for the two kinds read further
        // (allowed and denied) the identifier begins at `SidStart`. For any other kind it is never read.
        let whose = unsafe { (&raw mut (*entry).SidStart).cast::<c_void>() };
        match u32::from(kind) {
            ACCESS_DENIED_ACE_TYPE => {}
            ACCESS_ALLOWED_ACE_TYPE
                if user.is(whose)
                    // SAFETY: `whose` is the identifier of an allowed entry of `list`, alive until
                    // `_descriptor` drops; `IsWellKnownSid` only reads it.
                    || unsafe { IsWellKnownSid(whose, WinLocalSystemSid) } != 0
                    // SAFETY: the same identifier, only read.
                    || unsafe { IsWellKnownSid(whose, WinBuiltinAdministratorsSid) } != 0
                    // SAFETY: the same identifier, only read.
                    || unsafe { IsWellKnownSid(whose, WinCreatorOwnerRightsSid) } != 0 => {}
            _ => return refused("can be opened by other users (`icacls /inheritance:r` it)"),
        }
    }
    Ok(())
}

/// Leaves `path` open to this user, the system and the administrators, and to nobody its
/// parent is open to. What is made later in one that is a directory inherits that.
pub fn only_the_user_opens(path: &Path) -> io::Result<()> {
    let user = Sid::of(&own_token()?, TokenUser)?.written()?;
    let inherited = if path.is_dir() { "OICI" } else { "" };
    let list =
        format!("D:P(A;{inherited};FA;;;{user})(A;{inherited};FA;;;SY)(A;{inherited};FA;;;BA)");
    let list: Vec<u16> = list.encode_utf16().chain([0]).collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: the text ends with a zero, so it is read no further, and `descriptor` is a live
    // pointer for the call to write; what it points to then is freed by `_descriptor`, once.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            list.as_ptr(),
            SDDL_REVISION_1,
            &raw mut descriptor,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let _descriptor = Local(descriptor);
    let (mut present, mut defaulted) = (0, 0);
    let mut acl: *mut ACL = ptr::null_mut();
    // SAFETY: the descriptor is the one just made, alive while `_descriptor` is, and the three
    // pointers are live for the call to write; the list it gives points into the descriptor.
    if unsafe {
        GetSecurityDescriptorDacl(descriptor, &raw mut present, &raw mut acl, &raw mut defaulted)
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the path ends with a zero and lives for the statement, and the list points into
    // the descriptor `_descriptor` keeps alive past the call, which only reads it.
    failed(unsafe {
        SetNamedSecurityInfoW(
            wide(path).as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            acl,
            ptr::null_mut(),
        )
    })
}

/// Turns away a peer that is another user: the socket tells the process at the other end,
/// and its token the user. A process of another user that this one may not open is turned
/// away by that.
pub fn check_peer(stream: &UnixStream) -> io::Result<()> {
    let (mut process, mut len) = (0u32, 0u32);
    // SAFETY: the socket is `stream`'s, open for as long as the borrow; `SIO_AF_UNIX_GETPEERPID`
    // writes a `u32`, and `process` is a live `u32` whose size is the length given, so the call
    // writes no further. A longer length would have it write past `process`. No overlapped
    // operation is asked for, so nothing is written after the call returns.
    let failed = unsafe {
        WSAIoctl(
            stream.as_raw_socket() as usize,
            SIO_AF_UNIX_GETPEERPID,
            ptr::null(),
            0,
            (&raw mut process).cast(),
            size_of::<u32>() as u32,
            &raw mut len,
            ptr::null_mut(),
            None,
        )
    };
    if failed != 0 {
        // SAFETY: it takes nothing, touches no memory of the caller's, and reads the error of
        // this thread.
        let error = unsafe { WSAGetLastError() };
        return Err(io::Error::from_raw_os_error(error));
    }
    // SAFETY: `OpenProcess` takes no pointer; any `process` is either opened or refused with
    // null. The handle it opens is owned below, so it is closed once.
    let peer = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process) };
    if peer.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `peer` is an open handle that nothing else owns: `OpenProcess` just made it.
    // Were it owned twice, it would be closed twice, the second time maybe another's handle.
    let peer = unsafe { OwnedHandle::from_raw_handle(peer) };
    let theirs = Sid::of(&token_of(peer.as_raw_handle())?, TokenUser)?;
    if Sid::of(&own_token()?, TokenUser)?.is(theirs.as_ptr()) {
        return Ok(());
    }
    let who = theirs.written()?;
    Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("the peer is user {who}")))
}
