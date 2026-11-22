//! `Win32::Security` and `Win32::Security::Authorization`: tokens, SIDs, ACLs and security
//! descriptors, what the private-directory check and the home lookup read; and userenv's
//! `GetUserProfileDirectoryW`.

#![allow(
    non_upper_case_globals,
    reason = "Win32 spells some constants in camel case (`TokenUser`, `WinLocalSystemSid`)"
)]

use core::ffi::c_void;

use super::{Bool, Handle, PWSTR, foundation::WIN32_ERROR};

/// `PSID`: a pointer to a SID (winnt.h), not a kernel handle.
///
/// replaces windows_sys::Win32::Security::PSID
pub type PSID = *mut c_void;
/// `PSECURITY_DESCRIPTOR`: a pointer to a security descriptor (winnt.h).
///
/// replaces windows_sys::Win32::Security::PSECURITY_DESCRIPTOR
pub type PSECURITY_DESCRIPTOR = *mut c_void;
/// `OBJECT_SECURITY_INFORMATION` (`SECURITY_INFORMATION`): which parts of a descriptor.
///
/// replaces windows_sys::Win32::Security::OBJECT_SECURITY_INFORMATION
pub type OBJECT_SECURITY_INFORMATION = u32;
/// `TOKEN_ACCESS_MASK`: a token's access rights.
///
/// replaces windows_sys::Win32::Security::TOKEN_ACCESS_MASK
pub type TOKEN_ACCESS_MASK = u32;
/// `TOKEN_INFORMATION_CLASS`: what `GetTokenInformation` reads.
///
/// replaces windows_sys::Win32::Security::TOKEN_INFORMATION_CLASS
pub type TOKEN_INFORMATION_CLASS = i32;
/// `WELL_KNOWN_SID_TYPE`: `Win*Sid`.
///
/// replaces windows_sys::Win32::Security::WELL_KNOWN_SID_TYPE
pub type WELL_KNOWN_SID_TYPE = i32;
/// `SE_OBJECT_TYPE` (accctrl.h): what kind of object `GetSecurityInfo` reads.
///
/// replaces windows_sys::Win32::Security::Authorization::SE_OBJECT_TYPE
pub type SE_OBJECT_TYPE = i32;
/// `ACE_FLAGS`: an ACE's inheritance and audit flags.
///
/// replaces windows_sys::Win32::Security::ACE_FLAGS
pub type ACE_FLAGS = u32;
/// `ACE_REVISION`: an ACL's revision (`ACL_REVISION`).
///
/// replaces windows_sys::Win32::Security::ACE_REVISION
pub type ACE_REVISION = u32;
/// `SECURITY_DESCRIPTOR_CONTROL`: a descriptor's `SE_*` control bits.
///
/// replaces windows_sys::Win32::Security::SECURITY_DESCRIPTOR_CONTROL
pub type SECURITY_DESCRIPTOR_CONTROL = u16;

/// `ACL_REVISION` (2).
///
/// replaces windows_sys::Win32::Security::ACL_REVISION
pub const ACL_REVISION: ACE_REVISION = 2u32;
/// `SECURITY_MAX_SID_SIZE` (68): the bytes of the longest SID.
///
/// replaces windows_sys::Win32::Security::SECURITY_MAX_SID_SIZE
pub const SECURITY_MAX_SID_SIZE: u32 = 68u32;
/// `WinLowLabelSid` (66): the low mandatory integrity level's label.
///
/// replaces windows_sys::Win32::Security::WinLowLabelSid
pub const WinLowLabelSid: WELL_KNOWN_SID_TYPE = 66i32;
/// `WinBuiltinAnyPackageSid` (84): ALL APPLICATION PACKAGES, every AppContainer.
///
/// replaces windows_sys::Win32::Security::WinBuiltinAnyPackageSid
pub const WinBuiltinAnyPackageSid: WELL_KNOWN_SID_TYPE = 84i32;

/// `OWNER_SECURITY_INFORMATION` (1).
///
/// replaces windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION
pub const OWNER_SECURITY_INFORMATION: OBJECT_SECURITY_INFORMATION = 1u32;
/// `DACL_SECURITY_INFORMATION` (4).
///
/// replaces windows_sys::Win32::Security::DACL_SECURITY_INFORMATION
pub const DACL_SECURITY_INFORMATION: OBJECT_SECURITY_INFORMATION = 4u32;
/// `TOKEN_QUERY` (8).
///
/// replaces windows_sys::Win32::Security::TOKEN_QUERY
pub const TOKEN_QUERY: TOKEN_ACCESS_MASK = 8u32;
/// `TokenUser` (1): the token's user, a [`TOKEN_USER`].
///
/// replaces windows_sys::Win32::Security::TokenUser
pub const TokenUser: TOKEN_INFORMATION_CLASS = 1i32;
/// `WinLocalSystemSid` (22).
///
/// replaces windows_sys::Win32::Security::WinLocalSystemSid
pub const WinLocalSystemSid: WELL_KNOWN_SID_TYPE = 22i32;
/// `WinBuiltinAdministratorsSid` (26).
///
/// replaces windows_sys::Win32::Security::WinBuiltinAdministratorsSid
pub const WinBuiltinAdministratorsSid: WELL_KNOWN_SID_TYPE = 26i32;
/// `WinCreatorOwnerRightsSid` (71).
///
/// replaces windows_sys::Win32::Security::WinCreatorOwnerRightsSid
pub const WinCreatorOwnerRightsSid: WELL_KNOWN_SID_TYPE = 71i32;
/// `SE_FILE_OBJECT` (1): a file or directory.
///
/// replaces windows_sys::Win32::Security::Authorization::SE_FILE_OBJECT
pub const SE_FILE_OBJECT: SE_OBJECT_TYPE = 1i32;

/// `ACL` (winnt.h): an access-control list's header; its ACEs follow it.
///
/// replaces windows_sys::Win32::Security::ACL
#[derive(Clone, Copy)]
#[repr(C)]
pub struct ACL {
    pub AclRevision: u8,
    pub Sbz1: u8,
    pub AclSize: u16,
    pub AceCount: u16,
    pub Sbz2: u16,
}

/// `SECURITY_ATTRIBUTES` (minwinbase.h): a descriptor and whether a handle is inherited.
///
/// replaces windows_sys::Win32::Security::SECURITY_ATTRIBUTES
#[derive(Clone, Copy)]
#[repr(C)]
pub struct SECURITY_ATTRIBUTES {
    pub nLength: u32,
    pub lpSecurityDescriptor: *mut c_void,
    pub bInheritHandle: Bool,
}

/// `SID_AND_ATTRIBUTES` (winnt.h).
///
/// replaces windows_sys::Win32::Security::SID_AND_ATTRIBUTES
#[derive(Clone, Copy)]
#[repr(C)]
pub struct SID_AND_ATTRIBUTES {
    pub Sid: PSID,
    pub Attributes: u32,
}

/// `TOKEN_USER` (winnt.h): what `GetTokenInformation(TokenUser)` writes first; the SID it
/// points to follows it in the same buffer.
///
/// replaces windows_sys::Win32::Security::TOKEN_USER
#[derive(Clone, Copy)]
#[repr(C)]
pub struct TOKEN_USER {
    pub User: SID_AND_ATTRIBUTES,
}

/// `SECURITY_DESCRIPTOR` (winnt.h): an absolute security descriptor, whose ACLs and SIDs are
/// pointed to, not inline; `InitializeSecurityDescriptor` sets it up.
///
/// replaces windows_sys::Win32::Security::SECURITY_DESCRIPTOR
#[derive(Clone, Copy)]
#[repr(C)]
pub struct SECURITY_DESCRIPTOR {
    pub Revision: u8,
    pub Sbz1: u8,
    pub Control: SECURITY_DESCRIPTOR_CONTROL,
    pub Owner: PSID,
    pub Group: PSID,
    pub Sacl: *mut ACL,
    pub Dacl: *mut ACL,
}

impl Default for SECURITY_DESCRIPTOR {
    fn default() -> Self {
        SECURITY_DESCRIPTOR {
            Revision: 0,
            Sbz1: 0,
            Control: 0,
            Owner: core::ptr::null_mut(),
            Group: core::ptr::null_mut(),
            Sacl: core::ptr::null_mut(),
            Dacl: core::ptr::null_mut(),
        }
    }
}

// winnt.h, x86-64 and arm64 Windows (8-byte pointers): identical.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
const _: () = {
    use core::mem::offset_of;
    assert!(size_of::<SECURITY_DESCRIPTOR>() == 40 && align_of::<SECURITY_DESCRIPTOR>() == 8);
    assert!(offset_of!(SECURITY_DESCRIPTOR, Revision) == 0);
    assert!(offset_of!(SECURITY_DESCRIPTOR, Control) == 2);
    assert!(offset_of!(SECURITY_DESCRIPTOR, Owner) == 8);
    assert!(offset_of!(SECURITY_DESCRIPTOR, Group) == 16);
    assert!(offset_of!(SECURITY_DESCRIPTOR, Sacl) == 24);
    assert!(offset_of!(SECURITY_DESCRIPTOR, Dacl) == 32);
};

// winnt.h: the same on every Windows target.
const _: () = {
    use core::mem::offset_of;
    assert!(size_of::<ACL>() == 8 && align_of::<ACL>() == 2);
    assert!(offset_of!(ACL, AclSize) == 2);
    assert!(offset_of!(ACL, AceCount) == 4);
};

// minwinbase.h and winnt.h, x86-64 and arm64 Windows (LLP64, 8-byte pointers): identical.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
const _: () = {
    use core::mem::offset_of;
    assert!(size_of::<SECURITY_ATTRIBUTES>() == 24 && align_of::<SECURITY_ATTRIBUTES>() == 8);
    assert!(offset_of!(SECURITY_ATTRIBUTES, nLength) == 0);
    assert!(offset_of!(SECURITY_ATTRIBUTES, lpSecurityDescriptor) == 8);
    assert!(offset_of!(SECURITY_ATTRIBUTES, bInheritHandle) == 16);
    assert!(size_of::<SID_AND_ATTRIBUTES>() == 16 && align_of::<SID_AND_ATTRIBUTES>() == 8);
    assert!(offset_of!(SID_AND_ATTRIBUTES, Sid) == 0);
    assert!(offset_of!(SID_AND_ATTRIBUTES, Attributes) == 8);
    assert!(size_of::<TOKEN_USER>() == 16);
    assert!(offset_of!(TOKEN_USER, User) == 0);
};

#[link(name = "advapi32", kind = "raw-dylib")]
unsafe extern "system" {
    /// `AddAccessAllowedAce` (securitybaseapi.h): appends an allowed ACE of `psid` to `pacl`.
    ///
    /// replaces windows_sys::Win32::Security::AddAccessAllowedAce
    pub fn AddAccessAllowedAce(pacl: *mut ACL, dwacerevision: ACE_REVISION, accessmask: u32, psid: PSID) -> Bool;
    /// `AddMandatoryAce` (securitybaseapi.h): appends a mandatory label ACE to a SACL.
    ///
    /// replaces windows_sys::Win32::Security::AddMandatoryAce
    pub fn AddMandatoryAce(
        pacl: *mut ACL,
        dwacerevision: ACE_REVISION,
        aceflags: ACE_FLAGS,
        mandatorypolicy: u32,
        plabelsid: PSID,
    ) -> Bool;
    /// `CopySid` (securitybaseapi.h): copies `psourcesid` into `ndestinationsidlength` bytes.
    ///
    /// replaces windows_sys::Win32::Security::CopySid
    pub fn CopySid(ndestinationsidlength: u32, pdestinationsid: PSID, psourcesid: PSID) -> Bool;
    /// `CreateWellKnownSid` (securitybaseapi.h): writes the SID into `*cbsid` bytes at `psid`.
    ///
    /// replaces windows_sys::Win32::Security::CreateWellKnownSid
    pub fn CreateWellKnownSid(
        wellknownsidtype: WELL_KNOWN_SID_TYPE,
        domainsid: PSID,
        psid: PSID,
        cbsid: *mut u32,
    ) -> Bool;
    /// `InitializeAcl` (securitybaseapi.h): an empty ACL in `nacllength` bytes.
    ///
    /// replaces windows_sys::Win32::Security::InitializeAcl
    pub fn InitializeAcl(pacl: *mut ACL, nacllength: u32, dwaclrevision: ACE_REVISION) -> Bool;
    /// `InitializeSecurityDescriptor` (securitybaseapi.h): an absolute descriptor with no
    /// owner, group or ACLs.
    ///
    /// replaces windows_sys::Win32::Security::InitializeSecurityDescriptor
    pub fn InitializeSecurityDescriptor(psecuritydescriptor: PSECURITY_DESCRIPTOR, dwrevision: u32) -> Bool;
    /// `SetSecurityDescriptorDacl` (securitybaseapi.h): points the descriptor at `pdacl`.
    ///
    /// replaces windows_sys::Win32::Security::SetSecurityDescriptorDacl
    pub fn SetSecurityDescriptorDacl(
        psecuritydescriptor: PSECURITY_DESCRIPTOR,
        bdaclpresent: Bool,
        pdacl: *const ACL,
        bdacldefaulted: Bool,
    ) -> Bool;
    /// `SetSecurityDescriptorSacl` (securitybaseapi.h): points the descriptor at `psacl`.
    ///
    /// replaces windows_sys::Win32::Security::SetSecurityDescriptorSacl
    pub fn SetSecurityDescriptorSacl(
        psecuritydescriptor: PSECURITY_DESCRIPTOR,
        bsaclpresent: Bool,
        psacl: *const ACL,
        bsacldefaulted: Bool,
    ) -> Bool;
    /// `EqualSid` (securitybaseapi.h).
    ///
    /// replaces windows_sys::Win32::Security::EqualSid
    pub fn EqualSid(psid1: PSID, psid2: PSID) -> Bool;
    /// `GetAce` (securitybaseapi.h): a pointer to the ACE at `dwaceindex` inside `pacl`.
    ///
    /// replaces windows_sys::Win32::Security::GetAce
    pub fn GetAce(pacl: *const ACL, dwaceindex: u32, pace: *mut *mut c_void) -> Bool;
    /// `GetTokenInformation` (securitybaseapi.h).
    ///
    /// replaces windows_sys::Win32::Security::GetTokenInformation
    pub fn GetTokenInformation(
        tokenhandle: Handle,
        tokeninformationclass: TOKEN_INFORMATION_CLASS,
        tokeninformation: *mut c_void,
        tokeninformationlength: u32,
        returnlength: *mut u32,
    ) -> Bool;
    /// `IsWellKnownSid` (securitybaseapi.h).
    ///
    /// replaces windows_sys::Win32::Security::IsWellKnownSid
    pub fn IsWellKnownSid(psid: PSID, wellknownsidtype: WELL_KNOWN_SID_TYPE) -> Bool;
    /// `GetSecurityInfo` (aclapi.h): returns the error itself; the descriptor it allocates is
    /// freed with `LocalFree`, and the owner and DACL pointers point into it.
    ///
    /// replaces windows_sys::Win32::Security::Authorization::GetSecurityInfo
    pub fn GetSecurityInfo(
        handle: Handle,
        objecttype: SE_OBJECT_TYPE,
        securityinfo: OBJECT_SECURITY_INFORMATION,
        ppsidowner: *mut PSID,
        ppsidgroup: *mut PSID,
        ppdacl: *mut *mut ACL,
        ppsacl: *mut *mut ACL,
        ppsecuritydescriptor: *mut PSECURITY_DESCRIPTOR,
    ) -> WIN32_ERROR;
}

#[link(name = "userenv", kind = "raw-dylib")]
unsafe extern "system" {
    /// `GetUserProfileDirectoryW` (userenv.h): the profile directory of the token's user, into
    /// `lpprofiledir` of `*lpcchsize` UTF-16 units; `ERROR_INSUFFICIENT_BUFFER` with the size
    /// needed in `*lpcchsize` when it does not fit.
    ///
    /// replaces windows_sys::Win32::UI::Shell::GetUserProfileDirectoryW (and the local
    /// declaration in blaze-core/src/home.rs)
    pub fn GetUserProfileDirectoryW(htoken: Handle, lpprofiledir: PWSTR, lpcchsize: *mut u32) -> Bool;
}

#[link(name = "bcryptprimitives", kind = "raw-dylib")]
unsafe extern "system" {
    /// `ProcessPrng` (bcryptprimitives.dll; documented in the CNG reference, not in a header):
    /// fills `cbdata` bytes at `pbdata` from the process's generator; always true.
    ///
    /// replaces windows_sys::Win32::Security::Cryptography::ProcessPrng
    pub fn ProcessPrng(pbdata: *mut u8, cbdata: usize) -> Bool;
}

/// `ProcessPrng` over `out`: random bytes from the system's generator. The call is documented
/// never to fail; a false one is the thread's last error, not ignored.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn process_prng(out: &mut [u8]) -> Result<(), super::WinError> {
    // SAFETY: `out` is `out.len()` writable bytes, the only memory the call writes; it keeps no
    // pointer past the call.
    unsafe { ProcessPrng(out.as_mut_ptr(), out.len()) }.ok()
}
