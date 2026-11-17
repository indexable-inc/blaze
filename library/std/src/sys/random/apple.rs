//! Random data on Apple platforms.
//!
//! `CCRandomGenerateBytes` calls into `CCRandomCopyBytes` with `kCCRandomDefault`.
//! `CCRandomCopyBytes` manages a CSPRNG which is seeded from the kernel's CSPRNG.
//! We use `CCRandomGenerateBytes` instead of `SecCopyBytes` because it is accessible via
//! `libSystem` (libc) while the other needs to link to `Security.framework`.
//!
//! Note that technically, `arc4random_buf` is available as well, but that calls
//! into the same system service anyway, and `CCRandomGenerateBytes` has been
//! proven to be App Store-compatible.

pub fn fill_bytes(bytes: &mut [u8]) {
    // blaze: a process given a provider takes its bytes from the seed (`sys::provider`); every
    // other process is as the nightly's.
    if crate::sys::provider::fill(bytes) {
        return;
    }
    let ret = unsafe { libc::CCRandomGenerateBytes(bytes.as_mut_ptr().cast(), bytes.len()) };
    assert_eq!(ret, libc::kCCSuccess, "failed to generate random data");
}
