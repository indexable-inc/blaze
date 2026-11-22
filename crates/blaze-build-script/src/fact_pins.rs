//! The one producer of the derived facts' pins ([`crate::facts::Pins`]): what the pinned clang
//! prints for `--version` in the guest, and the declared Apple userland's `sw_vers` values
//! (port27/tempo-deps NOTES, brief P; READINESS 1f, W5).
//!
//! Sans-IO: the clang pin table's text ([`blaze_pins::CLANG_JSON`]) and the userland's text
//! (its `SystemVersion.plist` and its kernel release, read once at the command's edge and handed
//! down: rule 75) in, owned pins out. Nothing here reads the machine.
//!
//! The pins are part of what a fact's answer is derived from, so they reach the script's record
//! through the fact's read: its subject is the question and its digest the reply made from these
//! pins; a later command re-answers the subject with ITS pins, so a changed userland or clang pin
//! misses exactly the runs that asked (rule 21). No path, host name or time is in them: the
//! clang's folder is the guest's ([`CLANG_GUEST_DIR`]), never the shelf's.
//!
//! `clang --version` of the pinned release, as clang's driver prints it (clang/lib/Basic/
//! Version.cpp `getClangToolFullVersion` and `getClangFullRepositoryVersion`; clang/lib/Driver/
//! Driver.cpp `Driver::PrintVersion`):
//!
//! ```text
//! clang version <version> (<repository> <revision>)
//! Target: <default triple, its darwin version the host kernel's release>
//! Thread model: posix
//! InstalledDir: <the folder of the running clang>
//! ```
//!
//! The `Build config:` line is printed only for a non-default build (assertions, expensive
//! checks): the release's is default. A `Configuration file:` line is printed only when a
//! `clang.cfg` is beside the program: the guest's tool folder holds none. The default triple's
//! darwin version is replaced by the running kernel's release (llvm/lib/TargetParser/Host.cpp
//! `updateTripleOSVersion`), so it is the declared userland's, never the build machine's.
#![blaze::sans_io]

use blaze_own::{AString, Alloc, String};
use blaze_pins::{CLANG_PATH, RUSTC_LLVM};

use crate::{facts::Pins, toolchain::scalar};

/// Where the guest sees the pinned C toolchain (`clang`, `clang++`, `llvm-ar`): the folder cc-rs
/// and a script's `cc` find a C compiler in on macOS. It is the `InstalledDir` the fact prints.
pub const CLANG_GUEST_DIR: &str = "/usr/bin";

/// The path of the userland's version file, read at the edge (for errors and the reader).
pub const SYSTEM_VERSION_PLIST: &str = "/System/Library/CoreServices/SystemVersion.plist";

/// The declared Apple userland, borrowed from its text: what `sw_vers` and the kernel's release
/// say in the guest.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AppleUserland<'u> {
    /// `ProductVersion` (`sw_vers -productVersion`), such as `27.0.1`.
    pub product_version: &'u str,
    /// `ProductBuildVersion` (`sw_vers -buildVersion`), such as `26A434`.
    pub build_version: &'u str,
    /// The kernel's release (`uname -r`, `kern.osrelease`), such as `27.0.0`.
    pub darwin_release: &'u str,
}

/// Why the userland's text declares no userland: the file and key, or the value not of its form.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UserlandError<'t> {
    /// The plist has no `<key>` with a `<string>` value after it.
    NoKey { key: &'static str },
    /// A value that is not of its form (`what` names it), carried as found.
    Malformed { what: &'static str, text: &'t str },
}

impl core::fmt::Display for UserlandError<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            UserlandError::NoKey { key } => {
                write!(f, "{SYSTEM_VERSION_PLIST}: no `{key}` string: the Apple userland is not declared, so its derived facts have no pins (READINESS 1f)")
            }
            UserlandError::Malformed { what, text } => {
                write!(f, "the Apple userland's {what} `{text}` is not of its form (dotted numbers, or letters and digits for a build): no pins (READINESS 1f)")
            }
        }
    }
}

/// Why no pins are made from the clang pin table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FactPinsError<'t> {
    /// The table lacks `field` at its top level, or it is not of its form.
    Pin { field: &'static str },
    /// The table's `version` is not the LLVM the facts' `-dumpversion` answers ([`blaze_pins::RUSTC_LLVM`]):
    /// two answers for one clang.
    VersionMismatch { pinned: &'t str },
    /// No default triple is stated for this target's pinned clang.
    NoTriple { target: &'t str },
}

impl core::fmt::Display for FactPinsError<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FactPinsError::Pin { field } => write!(f, "{CLANG_PATH}: no top-level `{field}` of its form: `clang --version` cannot be derived (brief P)"),
            FactPinsError::VersionMismatch { pinned } => write!(
                f,
                "{CLANG_PATH}: version {pinned} is not the LLVM {RUSTC_LLVM} the facts answer for `-dumpversion` (blaze_pins::RUSTC_LLVM): one clang, one version"
            ),
            FactPinsError::NoTriple { target } => {
                write!(f, "no default triple is stated for the pinned clang of {target}: its derived facts are not answered (brief P)")
            }
        }
    }
}

/// The derived facts' pins, owned.
#[derive(Debug, PartialEq, Eq)]
pub struct OwnedPins<'a> {
    pub clang_version_text: AString<'a>,
    pub macos_product_version: AString<'a>,
    pub macos_build_version: AString<'a>,
}

impl OwnedPins<'_> {
    /// Borrowed, as [`crate::facts::answer`] takes them.
    pub fn pins(&self) -> Pins<'_> {
        Pins {
            clang_version_text: self.clang_version_text.as_str(),
            macos_product_version: self.macos_product_version.as_str(),
            macos_build_version: self.macos_build_version.as_str(),
        }
    }
}

/// The `<string>` value after `<key>key</key>` in a plist's text.
fn plist_string<'t>(plist: &'t str, key: &'static str) -> Result<&'t str, UserlandError<'t>> {
    let no_key = UserlandError::NoKey { key };
    let mut at = 0usize;
    loop {
        let found = plist.get(at..).and_then(|rest| rest.find("<key>")).ok_or(no_key)? + at;
        let rest = &plist[found + "<key>".len()..];
        at = found + "<key>".len();
        let Some(name) = rest.strip_prefix(key) else { continue };
        let Some(after) = name.strip_prefix("</key>") else { continue };
        let value = after.trim_start().strip_prefix("<string>").ok_or(no_key)?;
        let end = value.find("</string>").ok_or(no_key)?;
        return Ok(&value[..end]);
    }
}

/// Whether `text` is one to three dot-separated runs of ASCII digits.
fn dotted(text: &str) -> bool {
    let parts = text.split('.');
    let mut count = 0usize;
    for part in parts {
        count += 1;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
    }
    (1..=3).contains(&count)
}

/// The declared userland from its `SystemVersion.plist` text and its kernel release text
/// (trailing whitespace ignored). Each value is checked for its form, never guessed.
pub fn apple_userland<'t>(plist: &'t str, darwin_release: &'t str) -> Result<AppleUserland<'t>, UserlandError<'t>> {
    let product_version = plist_string(plist, "ProductVersion")?;
    if !dotted(product_version) {
        return Err(UserlandError::Malformed { what: "ProductVersion", text: product_version });
    }
    let build_version = plist_string(plist, "ProductBuildVersion")?;
    if build_version.is_empty() || !build_version.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(UserlandError::Malformed { what: "ProductBuildVersion", text: build_version });
    }
    let darwin_release = darwin_release.trim_end();
    if !dotted(darwin_release) {
        return Err(UserlandError::Malformed { what: "kernel release", text: darwin_release });
    }
    Ok(AppleUserland { product_version, build_version, darwin_release })
}

/// The pinned clang's default triple's architecture and vendor for `target` (the part before
/// the OS version clang appends): the macOS release prints `arm64`, as its configure named it.
fn triple_stem(target: &str) -> Option<&'static str> {
    match target {
        "aarch64-apple-darwin" => Some("arm64-apple-darwin"),
        _ => None,
    }
}

/// The derived facts' pins for a script whose guest is `target` (its host): `clang --version`
/// composed from the pin table `pin` ([`blaze_pins::CLANG_JSON`]) and `userland`, the clang seen in
/// `clang_dir` ([`CLANG_GUEST_DIR`]); the `sw_vers` values from `userland`.
pub fn fact_pins<'a, 't>(
    pin: &'t str,
    target: &'t str,
    userland: &AppleUserland<'_>,
    clang_dir: &str,
    alloc: Alloc<'a>,
) -> Result<OwnedPins<'a>, FactPinsError<'t>> {
    let field = |field: &'static str| scalar(pin, field).filter(|v| !v.is_empty() && !v.contains(' ')).ok_or(FactPinsError::Pin { field });
    let version = field("version")?;
    if version != RUSTC_LLVM {
        return Err(FactPinsError::VersionMismatch { pinned: version });
    }
    let repository = field("repository")?;
    let revision = field("revision")?;
    if revision.len() != 40 || !revision.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(FactPinsError::Pin { field: "revision" });
    }
    let stem = triple_stem(target).ok_or(FactPinsError::NoTriple { target })?;

    let mut text = String::new_in(alloc);
    for part in ["clang version ", version, " (", repository, " ", revision, ")\nTarget: ", stem, userland.darwin_release] {
        text.push_str(part);
    }
    for part in ["\nThread model: posix\nInstalledDir: ", clang_dir, "\n"] {
        text.push_str(part);
    }
    Ok(OwnedPins {
        clang_version_text: text,
        macos_product_version: String::from_str_in(userland.product_version, alloc),
        macos_build_version: String::from_str_in(userland.build_version, alloc),
    })
}
