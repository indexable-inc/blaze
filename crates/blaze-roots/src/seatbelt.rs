//! macOS Seatbelt SBPL profile renderer (one home, rule 30; sans-IO, `#![no_std]`).
//!
//! Used by `bstd::procs::hermetic::seatbelt` for every `Level::Inputs` run on macOS and by
//! `scripts/src/bin/proc_macro_modules` for the native build-script builder so both render the
//! exact same deny-by-default profile without duplicating SBPL rules or pulling `bstd`'s full
//! dependency closure into bootstrap scripts.

#![blaze::sans_io]

use blaze_own::{AVec, Alloc};

/// The loader and the system libraries, read whole (found by narrowing a `(deny default)` profile
/// on macOS 27, arm64). `/usr/lib` holds dyld and libSystem; `/System/Library` the frameworks and
/// `dyld/`; the Cryptex the dyld shared cache.
pub const MACOS_SYSTEM: [&str; 3] = ["/usr/lib", "/System/Library", "/System/Volumes/Preboot/Cryptexes/OS"];

/// What every profile allows before the caller's rules (D65/D69):
/// - `process-fork`: a fork runs only the confined program again, under the same profile; what it
///   may then `execve` is limited to the `Exec` rules.
/// - `sysctl-read`: the C runtime asks the page size and CPU count at start.
/// - `signal (target same-sandbox)`: a program signals itself and its own helpers.
/// - `file-read*` of `/` itself (literal, not subtree): the loader reads `/` before `main`.
const PREAMBLE: &[u8] = b"(version 1)\n(deny default)\n(allow process-fork)\n(allow sysctl-read)\n\
(allow signal (target same-sandbox))\n(allow file-read* (literal \"/\"))\n";

/// Same as [`PREAMBLE`] but omits `(allow process-fork)` for confined workers that may spawn
/// threads (which macOS permits without `process-fork`) but must not fork child processes.
const PREAMBLE_NO_FORK: &[u8] = b"(version 1)\n(deny default)\n(allow sysctl-read)\n\
(allow signal (target same-sandbox))\n(allow file-read* (literal \"/\"))\n";

/// What an SBPL rule grants.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SbplAccess {
    /// Open to read (and stat, and list where it is a folder).
    Read,
    /// Read, create, write, remove.
    Write,
    /// Run (and read, which a run needs).
    Exec,
}

/// One path rule in a Seatbelt profile.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SbplRule<'a> {
    pub path: &'a str,
    /// `true` for `(subpath ...)`, `false` for `(literal ...)`.
    pub subtree: bool,
    pub access: SbplAccess,
}

/// Why [`render_sbpl`] refused to render a profile.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SbplError {
    OutOfMemory,
    /// Rule `at` has a non-canonical path or a character an SBPL string cannot quote.
    BadPath { at: u32 },
}

/// One spelling of one absolute path: starts at `/`, no empty, `.` or `..` part, no trailing `/`
/// (except `/` itself), valid UTF-8, and no quote, backslash, or control byte.
pub fn one_spelling(path: &[u8]) -> bool {
    if path == b"/" {
        return true;
    }
    if path.first() != Some(&b'/') || path.last() == Some(&b'/') || core::str::from_utf8(path).is_err() {
        return false;
    }
    if path.iter().any(|&b| b < 0x20 || b == 0x7f || b == b'"' || b == b'\\') {
        return false;
    }
    let Some(rest) = path.get(1..) else { return false };
    rest.split(|&b| b == b'/').all(|part| !part.is_empty() && part != b"." && part != b"..")
}

/// Renders `rules` into a NUL-terminated SBPL profile in `alloc` (permitting `process-fork`).
pub fn render_sbpl<'a, 'r>(rules: impl Iterator<Item = SbplRule<'r>>, alloc: Alloc<'a>) -> Result<AVec<'a, u8>, SbplError> {
    render_sbpl_with_fork(rules, true, alloc)
}

/// Renders `rules` into a NUL-terminated SBPL profile in `alloc`, optionally denying `process-fork`.
pub fn render_sbpl_with_fork<'a, 'r>(
    rules: impl Iterator<Item = SbplRule<'r>>,
    allow_fork: bool,
    alloc: Alloc<'a>,
) -> Result<AVec<'a, u8>, SbplError> {
    let mut out = AVec::new_in(alloc);
    let mut put = |bytes: &[u8]| out.try_extend_from_slice(bytes).map_err(|_| SbplError::OutOfMemory);
    put(if allow_fork { PREAMBLE } else { PREAMBLE_NO_FORK })?;
    for (idx, rule) in rules.enumerate() {
        let path = rule.path.as_bytes();
        if !one_spelling(path) {
            return Err(SbplError::BadPath { at: u32::try_from(idx).unwrap_or(u32::MAX) });
        }
        let filter: &[u8] = if rule.subtree { b"subpath" } else { b"literal" };
        let ops: &[&[u8]] = match rule.access {
            SbplAccess::Exec => &[b"process-exec", b"file-read*"],
            SbplAccess::Read => &[b"file-read*"],
            SbplAccess::Write => &[b"file-read*", b"file-write*"],
        };
        for op in ops {
            put(b"(allow ")?;
            put(op)?;
            put(b" (")?;
            put(filter)?;
            put(b" \"")?;
            put(path)?;
            put(b"\"))\n")?;
        }
        // Each ancestor folder's metadata: resolving the path stats every folder above it.
        let mut end = 0usize;
        while let Some(tail) = path.get(end + 1..) {
            let Some(at) = tail.iter().position(|&b| b == b'/') else { break };
            end += 1 + at;
            if let Some(ancestor) = path.get(..end) {
                put(b"(allow file-read-metadata (literal \"")?;
                put(ancestor)?;
                put(b"\"))\n")?;
            }
        }
    }
    put(&[0])?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_sbpl_no_fork_denies_process_fork_while_preserving_rules() {
        let alloc = blaze_own::testing::heap().alloc();
        let rules = [
            SbplRule { path: "/opt/bin/worker", subtree: false, access: SbplAccess::Exec },
            SbplRule { path: "/workspace/crate", subtree: true, access: SbplAccess::Read },
            SbplRule { path: "/dev/null", subtree: false, access: SbplAccess::Write },
        ];
        let with_fork = render_sbpl(rules.into_iter(), alloc).expect("with fork");
        let no_fork = render_sbpl_with_fork(rules.into_iter(), false, alloc).expect("no fork");

        let with_text = core::str::from_utf8(&with_fork[..with_fork.len() - 1]).expect("utf8");
        let no_text = core::str::from_utf8(&no_fork[..no_fork.len() - 1]).expect("utf8");

        assert!(with_text.contains("(allow process-fork)"));
        assert!(!no_text.contains("process-fork"), "no-fork profile must omit process-fork:\n{no_text}");
        assert!(no_text.starts_with("(version 1)\n(deny default)\n"));
        assert!(no_text.contains("(allow process-exec (literal \"/opt/bin/worker\"))"));
        assert!(no_text.contains("(allow file-read* (subpath \"/workspace/crate\"))"));
        assert!(!no_text.contains("network"));
    }
}
