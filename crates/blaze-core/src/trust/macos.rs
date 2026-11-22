//! macOS's roots, through Security.framework opened at first use.
//!
//! Why unsafe: Security and CoreFoundation are C APIs std does not wrap, and the system's trust
//! store (its anchors and the admin's and user's trust settings) has no other public reader.
//! Both frameworks are opened with `dlopen` on the first read and never at load (rule 45's
//! "where it is better": a program that never fetches pays nothing; the symbols are public
//! API). Every object a `Copy` call returns is released by [`Owned`]'s drop; every array is read
//! within its count.
//!
//! Trust (Apple's SecTrustSettings.h): a certificate's settings in a domain are an array of
//! dictionaries. An empty array means "trust as a root". Otherwise the first dictionary that
//! applies to SSL (no `kSecTrustSettingsPolicy`, or the Apple SSL policy) decides by its
//! `kSecTrustSettingsResult` (absent: trust as a root). `Unspecified` passes the decision on.
//! The user domain is asked before the admin domain, and the first decision wins. The system's
//! anchors are trusted unless a domain denies them; a root a domain trusts is added.

use std::{
    alloc::Allocator,
    ffi::{CStr, c_char, c_void},
};

use blaze_sys::posix::thread::{self, RtldFlags};

use super::{Looked, Place, Roots, TrustError, Why};

type CfRef = *const c_void;
type OsStatus = i32;

const SECURITY: &CStr = c"/System/Library/Frameworks/Security.framework/Security";
const CORE_FOUNDATION: &CStr = c"/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation";

/// `kSecTrustSettingsDomainUser` and `kSecTrustSettingsDomainAdmin`, in the order asked.
const DOMAINS: [(u32, &str); 2] = [(0, "the user's trust settings"), (1, "the admin's trust settings")];
const ERR_NO_TRUST_SETTINGS: OsStatus = -25263;
const ERR_ITEM_NOT_FOUND: OsStatus = -25300;
const CF_NUMBER_SINT32: isize = 3;
const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const RESULT_TRUST_ROOT: i32 = 1;
const RESULT_TRUST_AS_ROOT: i32 = 2;
const RESULT_DENY: i32 = 3;


/// The calls, found once ([`Frameworks::get`]); each type is the symbol's as its header
/// declares it.
struct Frameworks {
    array_get_count: unsafe extern "C" fn(CfRef) -> isize,
    array_get_value_at_index: unsafe extern "C" fn(CfRef, isize) -> CfRef,
    dictionary_get_value: unsafe extern "C" fn(CfRef, CfRef) -> CfRef,
    number_get_value: unsafe extern "C" fn(CfRef, isize, *mut c_void) -> u8,
    data_get_length: unsafe extern "C" fn(CfRef) -> isize,
    data_get_byte_ptr: unsafe extern "C" fn(CfRef) -> *const u8,
    equal: unsafe extern "C" fn(CfRef, CfRef) -> u8,
    release: unsafe extern "C" fn(CfRef),
    string_create_with_c_string: unsafe extern "C" fn(CfRef, *const c_char, u32) -> CfRef,
    copy_anchor_certificates: unsafe extern "C" fn(*mut CfRef) -> OsStatus,
    settings_copy_certificates: unsafe extern "C" fn(u32, *mut CfRef) -> OsStatus,
    settings_copy_trust_settings: unsafe extern "C" fn(CfRef, u32, *mut CfRef) -> OsStatus,
    certificate_copy_data: unsafe extern "C" fn(CfRef) -> CfRef,
    policy_copy_properties: unsafe extern "C" fn(CfRef) -> CfRef,
    /// `kSecPolicyOid` and `kSecPolicyAppleSSL`: `CFStringRef` constants, by address.
    policy_oid: *const CfRef,
    policy_apple_ssl: *const CfRef,
}

// SAFETY: function pointers and the addresses of two constants the frameworks never write;
// both frameworks stay loaded for the life of the process (never `dlclose`d).
unsafe impl Send for Frameworks {}
// SAFETY: as above.
unsafe impl Sync for Frameworks {}

impl Frameworks {
    fn get() -> Result<&'static Frameworks, Looked> {
        static LOADED: std::sync::OnceLock<Result<Frameworks, Looked>> = std::sync::OnceLock::new();
        LOADED.get_or_init(Frameworks::load).as_ref().map_err(Clone::clone)
    }

    fn load() -> Result<Frameworks, Looked> {
        let cf = open(CORE_FOUNDATION, "CoreFoundation.framework")?;
        let sec = open(SECURITY, "Security.framework")?;
        let cfs = "CoreFoundation.framework";
        let secs = "Security.framework";
        // SAFETY: each type is the symbol's as the header declares it (the struct's doc).
        unsafe {
            Ok(Frameworks {
                array_get_count: symbol(cf, cfs, c"CFArrayGetCount")?,
                array_get_value_at_index: symbol(cf, cfs, c"CFArrayGetValueAtIndex")?,
                dictionary_get_value: symbol(cf, cfs, c"CFDictionaryGetValue")?,
                number_get_value: symbol(cf, cfs, c"CFNumberGetValue")?,
                data_get_length: symbol(cf, cfs, c"CFDataGetLength")?,
                data_get_byte_ptr: symbol(cf, cfs, c"CFDataGetBytePtr")?,
                equal: symbol(cf, cfs, c"CFEqual")?,
                release: symbol(cf, cfs, c"CFRelease")?,
                string_create_with_c_string: symbol(cf, cfs, c"CFStringCreateWithCString")?,
                copy_anchor_certificates: symbol(sec, secs, c"SecTrustCopyAnchorCertificates")?,
                settings_copy_certificates: symbol(sec, secs, c"SecTrustSettingsCopyCertificates")?,
                settings_copy_trust_settings: symbol(sec, secs, c"SecTrustSettingsCopyTrustSettings")?,
                certificate_copy_data: symbol(sec, secs, c"SecCertificateCopyData")?,
                policy_copy_properties: symbol(sec, secs, c"SecPolicyCopyProperties")?,
                policy_oid: symbol(sec, secs, c"kSecPolicyOid")?,
                policy_apple_ssl: symbol(sec, secs, c"kSecPolicyAppleSSL")?,
            })
        }
    }

    /// The `count` and each item of the array `array`.
    fn items(&self, array: CfRef) -> impl Iterator<Item = CfRef> + '_ {
        // SAFETY: `array` is a live CFArray (each caller's); read within its count.
        let count = unsafe { (self.array_get_count)(array) }.max(0);
        // SAFETY: as above, `at` below the count.
        (0..count).map(move |at| unsafe { (self.array_get_value_at_index)(array, at) })
    }

    /// The DER of the certificate `cert`, copied out.
    fn der(&self, cert: CfRef) -> Option<Owned<'_>> {
        // SAFETY: `cert` is a live SecCertificate; the data is ours to release.
        let data = unsafe { (self.certificate_copy_data)(cert) };
        Owned::new(self, data)
    }

    fn bytes<'d>(&self, data: &'d Owned<'_>) -> &'d [u8] {
        // SAFETY: a live CFData, read within its length, borrowed for as long as it is held.
        unsafe {
            let len = (self.data_get_length)(data.0).max(0) as usize;
            let ptr = (self.data_get_byte_ptr)(data.0);
            if ptr.is_null() || len == 0 {
                return &[];
            }
            std::slice::from_raw_parts(ptr, len)
        }
    }

    fn string(&self, text: &CStr) -> Option<Owned<'_>> {
        // SAFETY: a NUL-terminated UTF-8 string; the result is ours to release.
        let made = unsafe { (self.string_create_with_c_string)(std::ptr::null(), text.as_ptr(), CF_STRING_ENCODING_UTF8) };
        Owned::new(self, made)
    }
}

/// A CoreFoundation object a `Create` or `Copy` call returned: released when dropped.
struct Owned<'f>(CfRef, &'f Frameworks);

impl<'f> Owned<'f> {
    fn new(frameworks: &'f Frameworks, it: CfRef) -> Option<Owned<'f>> {
        (!it.is_null()).then_some(Owned(it, frameworks))
    }
}

impl Drop for Owned<'_> {
    fn drop(&mut self) {
        // SAFETY: an object this process owns one reference of, released once.
        unsafe { (self.1.release)(self.0) }
    }
}

fn open(path: &'static CStr, name: &'static str) -> Result<*mut c_void, Looked> {
    // SAFETY: a NUL-terminated path; the handle is never closed, so what it gives stays valid.
    let handle = unsafe { thread::dlopen(path.as_ptr(), (RtldFlags::LAZY | RtldFlags::LOCAL).bits()) };
    match handle.is_null() {
        true => Err(Looked { place: Place::System(name), why: Why::Missing { symbol: None } }),
        false => Ok(handle),
    }
}

/// # Safety
///
/// `F` is a pointer type that is the symbol's type as its header declares it.
unsafe fn symbol<F: Copy>(handle: *mut c_void, framework: &'static str, name: &'static CStr) -> Result<F, Looked> {
    const {
        assert!(size_of::<F>() == size_of::<*mut c_void>(), "a symbol is one address");
        assert!(align_of::<F>() == align_of::<*mut c_void>(), "a symbol has pointer alignment");
    }
    // SAFETY: a handle `open` returned, never closed, and a NUL-terminated name.
    let found = unsafe { thread::dlsym(handle, name.as_ptr()) };
    if found.is_null() {
        let symbol = name.to_str().ok();
        return Err(Looked { place: Place::System(framework), why: Why::Missing { symbol } });
    }
    // SAFETY: `F` has pointer size & alignment (asserted above) and names the loaded symbol's ABI.
    Ok(unsafe { core::ptr::read((&raw const found).cast::<F>()) })
}

/// A domain's decision on one certificate for SSL.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Decision {
    Trust,
    Deny,
}

/// The decision of `settings` (a certificate's trust-settings array) for SSL, if one applies.
fn decide(f: &Frameworks, settings: CfRef, keys: &Keys<'_>) -> Option<Decision> {
    let mut any = false;
    for entry in f.items(settings) {
        any = true;
        // SAFETY: a live dictionary of the array, and a live key.
        let policy = unsafe { (f.dictionary_get_value)(entry, keys.policy.0) };
        if !policy.is_null() {
            // SAFETY: a live SecPolicy; its properties are ours to release.
            let Some(properties) = Owned::new(f, unsafe { (f.policy_copy_properties)(policy) }) else { continue };
            // SAFETY: the constants' addresses, read once each; a live dictionary.
            let ssl = unsafe {
                let oid = (f.dictionary_get_value)(properties.0, *f.policy_oid);
                !oid.is_null() && (f.equal)(oid, *f.policy_apple_ssl) != 0
            };
            if !ssl {
                continue;
            }
        }
        // SAFETY: a live dictionary and key; a CFNumber read as the SInt32 it holds.
        let result = unsafe {
            let number = (f.dictionary_get_value)(entry, keys.result.0);
            let mut value = RESULT_TRUST_ROOT;
            if !number.is_null() {
                (f.number_get_value)(number, CF_NUMBER_SINT32, (&raw mut value).cast());
            }
            value
        };
        match result {
            RESULT_TRUST_ROOT | RESULT_TRUST_AS_ROOT => return Some(Decision::Trust),
            RESULT_DENY => return Some(Decision::Deny),
            _ => {}
        }
    }
    // An empty array: trust as a root.
    (!any).then_some(Decision::Trust)
}

/// The dictionary keys `kSecTrustSettingsPolicy` and `kSecTrustSettingsResult`, which the
/// header defines as `CFSTR`s, not symbols: made once per read.
struct Keys<'f> {
    policy: Owned<'f>,
    result: Owned<'f>,
}

/// The system's roots, as the user and the admin left them.
pub fn roots<A: Allocator + Clone>(alloc: A) -> Result<Roots<A>, TrustError> {
    let f = Frameworks::get().map_err(|looked| TrustError { looked: vec![looked] })?;
    let mut looked = Vec::new();
    let fail = |looked: Vec<Looked>| Err(TrustError { looked });
    let missing = |key: &'static str| Looked { place: Place::System("CoreFoundation.framework"), why: Why::Missing { symbol: Some(key) } };
    let Some(policy) = f.string(c"kSecTrustSettingsPolicy") else { return fail(vec![missing("CFStringCreateWithCString")]) };
    let Some(result) = f.string(c"kSecTrustSettingsResult") else { return fail(vec![missing("CFStringCreateWithCString")]) };
    let keys = Keys { policy, result };

    // Each domain's decisions, the user's first: (DER, decision), the first for a DER kept.
    let mut decided: Vec<(Owned<'_>, Decision)> = Vec::new();
    for (domain, name) in DOMAINS {
        let mut certs: CfRef = std::ptr::null();
        // SAFETY: an out pointer to a local; the array, when given, is ours to release.
        let status = unsafe { (f.settings_copy_certificates)(domain, &raw mut certs) };
        match status {
            0 => {}
            ERR_NO_TRUST_SETTINGS => {
                looked.push(Looked { place: Place::System(name), why: Why::Gave(0) });
                continue;
            }
            status => {
                looked.push(Looked { place: Place::System(name), why: Why::Status { call: "SecTrustSettingsCopyCertificates", status } });
                return fail(looked);
            }
        }
        let Some(certs) = Owned::new(f, certs) else { continue };
        let mut trusted = 0u32;
        for cert in f.items(certs.0) {
            let mut settings: CfRef = std::ptr::null();
            // SAFETY: a live certificate of the array; the settings are ours to release.
            let status = unsafe { (f.settings_copy_trust_settings)(cert, domain, &raw mut settings) };
            let decision = match status {
                0 => {
                    let Some(settings) = Owned::new(f, settings) else { continue };
                    decide(f, settings.0, &keys)
                }
                ERR_ITEM_NOT_FOUND | ERR_NO_TRUST_SETTINGS => None,
                status => {
                    looked.push(Looked {
                        place: Place::System(name),
                        why: Why::Status { call: "SecTrustSettingsCopyTrustSettings", status },
                    });
                    return fail(looked);
                }
            };
            let Some(decision) = decision else { continue };
            let Some(der) = f.der(cert) else { continue };
            if decided.iter().any(|(it, _)| f.bytes(it) == f.bytes(&der)) {
                continue;
            }
            trusted += u32::from(decision == Decision::Trust);
            decided.push((der, decision));
        }
        looked.push(Looked { place: Place::System(name), why: Why::Gave(trusted) });
    }

    let mut roots = Roots::new_in(alloc);
    // One read of the store made these: the count is the value's ([`Roots::reads`]).
    roots.reads = 1;
    let denied = |der: &[u8]| decided.iter().any(|(it, d)| *d == Decision::Deny && f.bytes(it) == der);
    let mut anchors: CfRef = std::ptr::null();
    // SAFETY: an out pointer to a local; the array, when given, is ours to release.
    let status = unsafe { (f.copy_anchor_certificates)(&raw mut anchors) };
    let place = Place::System("the system's anchors");
    if status != 0 {
        looked.push(Looked { place, why: Why::Status { call: "SecTrustCopyAnchorCertificates", status } });
        return fail(looked);
    }
    let mut gave = 0u32;
    if let Some(anchors) = Owned::new(f, anchors) {
        for cert in f.items(anchors.0) {
            let Some(der) = f.der(cert) else { continue };
            let der = f.bytes(&der);
            if denied(der) {
                continue;
            }
            if let Err(over) = roots.add(der) {
                looked.push(Looked { place: place.clone(), why: Why::Over(over) });
                return fail(looked);
            }
            gave += 1;
        }
    }
    looked.push(Looked { place, why: Why::Gave(gave) });
    for (der, decision) in &decided {
        if *decision == Decision::Trust
            && let Err(over) = roots.add(f.bytes(der))
        {
            looked.push(Looked { place: Place::System("the trust settings"), why: Why::Over(over) });
            return fail(looked);
        }
    }
    if roots.is_empty() {
        return fail(looked);
    }
    Ok(roots)
}
