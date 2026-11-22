//! `MacFacts`: what this Mac offers, probed ONCE at start (D185), the macOS counterpart of
//! Linux's `KernelFacts`. It holds the OS version (sysctl `kern.osproductversion`), Mach's
//! timebase, and whether each entry point the [`crate::macos_kernel::Kernel`] strategy uses is
//! exported (`dlsym`). It is handed down like the clock and the allocator (rule 75); a caller
//! chooses a strategy FROM it, up front, and prints the choice. An absent entry is a fact and a
//! typed refusal of the strategy that needs it ([`crate::macos_kernel::KernelRefusal`]), never a
//! quiet switch to another form (rule 3).
//!
//! The slots, their forms, and the macOS that introduced each (the floor is the oldest form's):
//!
//! | slot | form | introduced | how it is detected |
//! |---|---|---|---|
//! | read, write, pread, close | `Public`: libSystem's `read`/`write`/`pread`/`close` | Mac OS X 10.0 | always there (bound at load or by the cold table) |
//! | | `Kernel`: `__read_nocancel`, `__write_nocancel`, `__pread_nocancel`, `__close_nocancel` | 10.5 (UNSURE: the `$NOCANCEL` variants arrived with the UNIX 03 conformance) | `dlsym`, each a fact |
//! | | `Trap`: `svc #0x80` with 3, 4, 153, 6 | 10.0's numbers, never promised | not detectable: chosen only by name, for the experiment, the OS version recorded |
//! | getpid | `Public`: `getpid`; `Kernel`: `__getpid`; `Trap`: 20 | 10.0 | as above |
//! | open | `Public`: `openat` (cold table) | 10.10 | always there |
//! | | `Kernel` and `Trap`: `__open_nocancel` | 10.5 (UNSURE) | `dlsym` |
//! | fcntl | `Public`: `fcntl`; `Kernel` and `Trap`: `__fcntl_nocancel` | 10.0; 10.5 (UNSURE) | `dlsym` |
//! | fstat | `Public`'s `fstat` for all three | 10.5 (the 64-bit-inode `struct stat`) | always there |
//! | the clock | `Public`: `clock_gettime(CLOCK_MONOTONIC)`; `Kernel` and `Trap`: `mach_continuous_time` | 10.12, both | `dlsym` for `mach_continuous_time` |
//! | wait, wake | `Public`: `os_sync_wait_on_address_with_deadline`, `os_sync_wake_by_address_any` | 14.4 | the cold table, at first call (and `every_cold_symbol_exists`) |
//! | | `Kernel` and `Trap`: `__ulock_wait2`, `__ulock_wake` | 11.0 for `__ulock_wait2` (`__ulock_wake` 10.12) | `dlsym`, each a fact |
//!
//! Display is compact, one line for the profile: `os: macOS 27.2.1, kernel entries 10 of 10,
//! ulock_wait2 yes`.

use core::{ffi::c_void, num::NonZeroU32};

use crate::{
    Errno,
    macos::cold,
    macos_kernel::{Entry, KERNEL_ENTRIES, Timebase, find},
};

/// A macOS version, `major.minor.patch` (`kern.osproductversion`: "27.2.1"; "26.1" has patch 0).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct OsVersion {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

impl OsVersion {
    /// The version `text` spells (`"27.2.1"`, `"26.1"`, `"15"`): one to three dot-separated
    /// decimal numbers, each fitting a `u16`; `None` for anything else.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn parse(text: &[u8]) -> Option<OsVersion> {
        let mut parts = [0u16; 3];
        let mut at = 0usize;
        let mut digits = 0usize;
        for &byte in text {
            match byte {
                b'0'..=b'9' => {
                    let part = parts.get_mut(at)?;
                    *part = part.checked_mul(10)?.checked_add(u16::from(byte - b'0'))?;
                    digits += 1;
                }
                b'.' if digits > 0 => {
                    at += 1;
                    digits = 0;
                    if at >= parts.len() {
                        return None;
                    }
                }
                _ => return None,
            }
        }
        if digits == 0 {
            return None;
        }
        let [major, minor, patch] = parts;
        Some(OsVersion { major, minor, patch })
    }
}

impl core::fmt::Display for OsVersion {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let OsVersion { major, minor, patch } = self;
        match patch {
            0 => write!(f, "{major}.{minor}"),
            _ => write!(f, "{major}.{minor}.{patch}"),
        }
    }
}

/// Why the facts could not be probed, naming what.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FactsError {
    /// `sysctlbyname("kern.osproductversion")` refused.
    Sysctl(Errno),
    /// `kern.osproductversion` was not a version (or did not fit the buffer).
    Version,
    /// `mach_timebase_info` gave a zero numerator or denominator.
    Timebase,
}

impl core::fmt::Display for FactsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FactsError::Sysctl(errno) => {
                write!(f, "blaze_sys::macos_facts: sysctl kern.osproductversion refused: {} (D185)", errno.name())
            }
            FactsError::Version => {
                write!(f, "blaze_sys::macos_facts: sysctl kern.osproductversion is not `major.minor[.patch]` (D185)")
            }
            FactsError::Timebase => write!(f, "blaze_sys::macos_facts: mach_timebase_info gave a zero term (D185)"),
        }
    }
}

impl core::error::Error for FactsError {}

/// What this Mac offers, probed once ([`MacFacts::probe`]) and handed down.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MacFacts {
    /// The OS version, `kern.osproductversion`.
    pub os: OsVersion,
    /// Mach's timebase, `mach_timebase_info`.
    pub timebase: Timebase,
    /// Whether each of [`KERNEL_ENTRIES`] is exported, by [`Entry`]'s index.
    present: [bool; KERNEL_ENTRIES.len()],
}

impl MacFacts {
    /// The probe: one `sysctlbyname`, one `mach_timebase_info` and one `dlsym` per entry. Made
    /// once at start; never redone mid-run.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn probe() -> Result<MacFacts, FactsError> {
        let mut text = [0u8; 32];
        let mut len = text.len();
        // SAFETY: the name is a live NUL-terminated string; `text` is live memory of `len`
        // bytes the call writes at most `len` of, and writes the length back; nothing is set.
        let said = unsafe {
            cold::sysctlbyname(
                c"kern.osproductversion".as_ptr(),
                text.as_mut_ptr().cast::<c_void>(),
                &raw mut len,
                core::ptr::null_mut(),
                0,
            )
        };
        if said != 0 {
            return Err(FactsError::Sysctl(crate::posix::signal::errno()));
        }
        // The string's bytes, without the NUL the kernel counts.
        let written = text.get(..len).ok_or(FactsError::Version)?;
        let end = written.iter().position(|&byte| byte == 0).unwrap_or(written.len());
        let os = OsVersion::parse(written.get(..end).ok_or(FactsError::Version)?).ok_or(FactsError::Version)?;

        let (numer, denom) = crate::macos::timebase();
        let (Some(numer), Some(denom)) = (NonZeroU32::new(numer), NonZeroU32::new(denom)) else {
            return Err(FactsError::Timebase);
        };

        let mut present = [false; KERNEL_ENTRIES.len()];
        for (slot, symbol) in present.iter_mut().zip(KERNEL_ENTRIES.iter()) {
            *slot = !find(symbol.name).is_null();
        }
        Ok(MacFacts { os, timebase: Timebase { numer, denom }, present })
    }

    /// Whether `entry` is exported on this Mac.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn has(&self, entry: Entry) -> bool {
        self.present.get(entry as usize).copied().unwrap_or(false)
    }

    /// How many of [`KERNEL_ENTRIES`] are exported.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn kernel_entries(&self) -> usize {
        self.present.iter().filter(|&&there| there).count()
    }
}

impl core::fmt::Display for MacFacts {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let yes = |there: bool| if there { "yes" } else { "no" };
        write!(
            f,
            "os: macOS {}, kernel entries {} of {}, ulock_wait2 {}",
            self.os,
            self.kernel_entries(),
            KERNEL_ENTRIES.len(),
            yes(self.has(Entry::UlockWait2)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse() {
        let v = |major, minor, patch| Some(OsVersion { major, minor, patch });
        assert_eq!(OsVersion::parse(b"27.2.1"), v(27, 2, 1));
        assert_eq!(OsVersion::parse(b"26.1"), v(26, 1, 0));
        assert_eq!(OsVersion::parse(b"15"), v(15, 0, 0));
        for bad in [&b""[..], b"26.", b".1", b"26..1", b"1.2.3.4", b"26.1b", b"70000.1"] {
            assert_eq!(OsVersion::parse(bad), None, "{:?}", core::str::from_utf8(bad));
        }
        assert_eq!(v(26, 1, 0).unwrap().to_string(), "26.1");
        assert_eq!(v(27, 2, 1).unwrap().to_string(), "27.2.1");
    }

    /// The probe answers on this Mac, as `sw_vers` would, and its line has the compact shape.
    #[test]
    #[expect(clippy::print_stdout, reason = "the test's output records this Mac's facts")]
    fn the_probe_answers() {
        let facts = MacFacts::probe().expect("this Mac's facts");
        println!("{facts}");
        assert!(facts.os.major >= 11, "{facts}");
        assert_eq!(facts.kernel_entries(), KERNEL_ENTRIES.len(), "every entry on this Mac: {facts}");
        let line = facts.to_string();
        assert!(line.starts_with("os: macOS ") && line.ends_with("ulock_wait2 yes"), "{line}");
        assert!(line.contains(&format!("kernel entries {} of {}", KERNEL_ENTRIES.len(), KERNEL_ENTRIES.len())));
    }

    /// A fact that an entry is absent refuses the strategy, naming it (the control: the
    /// strategy is made when every fact is there).
    #[test]
    fn an_absent_entry_refuses_the_kernel_strategy() {
        let mut facts = MacFacts::probe().unwrap();
        assert!(crate::macos_kernel::Kernel::new(&facts).is_ok());
        if let Some(slot) = facts.present.get_mut(Entry::UlockWait2 as usize) {
            *slot = false;
        }
        let refusal = crate::macos_kernel::Kernel::new(&facts).unwrap_err();
        assert_eq!(refusal, crate::macos_kernel::KernelRefusal::Missing { name: c"__ulock_wait2", os: facts.os });
        assert!(refusal.to_string().contains("__ulock_wait2"));
        assert!(facts.to_string().ends_with("ulock_wait2 no"));
    }
}
