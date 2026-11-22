//! Which boot of the machine this is ([`Boot`]): a fact a native driver reads once as it is
//! made and hands out by [`crate::Driver::boot`], never read again and never put in a key. What
//! a program keeps across its runs and trusts only within one boot (a pack verified once per
//! boot) compares it.
//!
//! - macOS: `sysctlbyname("kern.bootsessionuuid")`, a UUID the kernel draws at each boot.
//! - Linux: `/proc/sys/kernel/random/boot_id`, the same.
//! - The simulation draws a new one at every power cut ([`crate::sim::Sim::crash`]).
//!
//! A host with neither has no boot fact: a typed [`NoBoot`] naming what was read, never a
//! made-up one (a boot that compared equal across a reboot would trust what a power cut tore).

use std::fmt;

/// 16 bytes naming the boot the process runs in: equal within one boot, different across two.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Boot(pub [u8; 16]);

/// The host gave no boot fact: what was asked, and what came back.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NoBoot {
    /// The source asked: a sysctl's name or a file's path.
    pub source: &'static str,
    pub why: String,
}

impl fmt::Display for NoBoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no boot fact from {}: {}", self.source, self.why)
    }
}

impl std::error::Error for NoBoot {}

/// A UUID's text (`8-4-4-4-12` hex digits, either case, a trailing newline or NUL allowed) as
/// its 16 bytes.
pub(crate) fn parse_uuid(source: &'static str, text: &[u8]) -> Result<Boot, NoBoot> {
    let refuse = |why: &str| NoBoot { source, why: format!("{why}: {:?}", String::from_utf8_lossy(text)) };
    let mut text = text;
    while let [rest @ .., b'\n' | b'\0'] = text {
        text = rest;
    }
    if text.len() != 36 {
        return Err(refuse("not a UUID of 36 characters"));
    }
    let mut bytes = [0u8; 16];
    let mut nibbles = 0usize;
    for (at, &c) in text.iter().enumerate() {
        if matches!(at, 8 | 13 | 18 | 23) {
            if c != b'-' {
                return Err(refuse("a UUID's dash missing"));
            }
            continue;
        }
        let digit = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => return Err(refuse("not a hex digit")),
        };
        bytes[nibbles / 2] |= digit << if nibbles % 2 == 0 { 4 } else { 0 };
        nibbles += 1;
    }
    Ok(Boot(bytes))
}

/// The running machine's boot, read once by a native driver as it is made.
#[cfg(target_os = "macos")]
#[allow(unsafe_code, reason = "one sysctlbyname call into a buffer of its stated length")]
pub(crate) fn system() -> Result<Boot, NoBoot> {
    const SOURCE: &str = "sysctl kern.bootsessionuuid";
    let mut buf = [0u8; 64];
    let mut len = buf.len();
    // SAFETY: the name is NUL-terminated; `buf` is writable for `len` bytes, and the kernel
    // writes at most `len` and sets `len` to what it wrote; no new value is given.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(NoBoot { source: SOURCE, why: std::io::Error::last_os_error().to_string() });
    }
    parse_uuid(SOURCE, &buf[..len.min(buf.len())])
}

/// The running machine's boot, read once by a native driver as it is made.
#[cfg(target_os = "linux")]
pub(crate) fn system() -> Result<Boot, NoBoot> {
    const SOURCE: &str = "/proc/sys/kernel/random/boot_id";
    match std::fs::read(SOURCE) {
        Ok(text) => parse_uuid(SOURCE, &text),
        Err(error) => Err(NoBoot { source: SOURCE, why: error.to_string() }),
    }
}

/// No boot fact on this host: the drivers here say so.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn system() -> Result<Boot, NoBoot> {
    Err(NoBoot { source: "this target", why: "it has no boot id the drivers read".to_owned() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_uuid_parses_to_its_bytes_and_a_wrong_one_is_refused() {
        let boot = parse_uuid("t", b"AA058D85-A3F5-4db4-9602-226DC4C0E13F\n").expect("a UUID");
        assert_eq!(boot.0[..4], [0xAA, 0x05, 0x8D, 0x85]);
        assert_eq!(boot.0[15], 0x3F);
        assert!(parse_uuid("t", b"AA058D85A3F54DB49602226DC4C0E13F").is_err());
        assert!(parse_uuid("t", b"AA058D85-A3F5-4DB4-9602-226DC4C0E13G").is_err());
    }

    /// The host's own boot reads, and twice in one boot reads the same.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn the_hosts_boot_reads_the_same_twice() {
        let first = system().expect("this host has a boot id");
        assert_eq!(system().expect("again"), first);
        assert_ne!(first, Boot([0; 16]));
    }
}
