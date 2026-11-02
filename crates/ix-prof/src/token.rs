//! The bearer token of a loopback endpoint (ix-prof's `/debug/pprof`, `blaze pprof --http`):
//! 128 bits from the kernel, compared in time that does not depend on where it differs. A
//! loopback port is reachable by every local account; the token is what keeps a profile to
//! the user who started it.

/// The token's length in bytes before hex: 128 bits, unguessable.
pub const BYTES: usize = 16;

/// A new token, as 32 lowercase hex digits.
pub fn new() -> Result<String, String> {
    let bytes = random::<BYTES>()?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Whether `given` is the token, in time that does not depend on where they differ (only the
/// length, which is public, ends it early).
pub fn matches(given: Option<&str>, want: &str) -> bool {
    let Some(given) = given else { return false };
    if given.len() != want.len() {
        return false;
    }
    given.bytes().zip(want.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

/// The token in an `Authorization: Bearer <token>` header's value.
pub fn bearer(value: &str) -> Option<&str> {
    let (scheme, token) = value.trim().split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}

/// `N` (at most 256) bytes from the kernel's random source: `getrandom(2)` on Linux and
/// Android, `getentropy(3)` elsewhere. No file descriptor, and no `/dev` needed (a chroot or a
/// minimal sandbox has none).
#[cfg(unix)]
fn random<const N: usize>() -> Result<[u8; N], String> {
    const { assert!(N <= 256, "getentropy gives at most 256 bytes a call") };
    let mut bytes = [0u8; N];
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut got = 0;
        // Bounded: getrandom gives up to 256 bytes whole once the pool is ready; a signal
        // before then is the only short read, and N tries cover N bytes.
        for _ in 0..=N {
            if got == N {
                break;
            }
            // SAFETY: the pointer and length are the unfilled tail of `bytes`.
            let n = unsafe { libc::getrandom(bytes[got..].as_mut_ptr().cast(), N - got, 0) };
            if n < 0 {
                let why = std::io::Error::last_os_error();
                if why.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(format!("getrandom: {why}"));
            }
            got += n as usize;
        }
        if got != N {
            return Err(format!("getrandom gave {got} of {N} bytes"));
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        // SAFETY: `bytes` is N <= 256 writable bytes, getentropy's limit.
        if unsafe { libc::getentropy(bytes.as_mut_ptr().cast(), N) } != 0 {
            return Err(format!("getentropy: {}", std::io::Error::last_os_error()));
        }
    }
    Ok(bytes)
}

#[cfg(not(unix))]
fn random<const N: usize>() -> Result<[u8; N], String> {
    Err(format!("no kernel random source is wired up for {}", std::env::consts::OS))
}

#[cfg(test)]
mod tests {
    #[test]
    fn two_tokens_differ_and_each_is_32_hex_digits() {
        let (a, b) = (super::new().unwrap(), super::new().unwrap());
        assert_ne!(a, b);
        assert!(a.len() == 32 && a.bytes().all(|c| c.is_ascii_hexdigit()), "{a}");
    }

    #[test]
    fn a_bearer_header_gives_its_token_and_nothing_else_does() {
        assert_eq!(super::bearer("Bearer abc"), Some("abc"));
        assert_eq!(super::bearer(" bearer  abc "), Some("abc"));
        assert_eq!(super::bearer("Basic abc"), None);
        assert_eq!(super::bearer("abc"), None);
    }
}
