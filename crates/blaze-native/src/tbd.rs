//! A text-based stub of the macOS SDK (`<SDK>/usr/lib/lib<N>.tbd`,
//! `<SDK>/System/Library/Frameworks/<N>.framework/<N>.tbd`), read for the one fact the native
//! image takes from it: the INSTALL NAME, the path a load command names and dyld loads. ld64
//! links `-l<N>` and `-framework <N>` against these stubs and writes the stub's `install-name`
//! into the image's `LC_LOAD_DYLIB`; the path is a fact of the SDK, never one built from the
//! library's name (Foundation and AppKit are kept under `Versions/C`, Tcl under `Versions/8.5`,
//! `-framework System` is `/usr/lib/libSystem.B.dylib`). Sans-IO: bytes in, a name borrowed from
//! them out; the caller reads the file through the file system it was handed (rule 75).
//!
//! The form read is TAPI's YAML text stub of version 4 (llvm/lib/TextAPI/TextStub.cpp; version
//! 5 is JSON, TextStubV5.cpp), which every stub of the macOS 27.2 SDK is: a document opening
//! `--- !tapi-tbd`, with the top-level keys `tbd-version: 4`, `targets: [ <arch>-<platform>, .. ]`
//! (a flow list, which runs over several lines) and `install-name: '<path>'`. Only the FIRST
//! document is read: the documents after it are the libraries it re-exports (libSystem.B.tbd
//! holds 40), whose install names are not the one linked.
//!
//! - The targets must list arm64 macOS: `arm64-macos` or `arm64e-macos` (the SDK's system
//!   stubs list arm64e only, and an arm64 image links against them as ld64 links it).
//! - A stub that lists no exported symbol (PushToTalk.tbd of the 27.2 SDK) is linked all the
//!   same, as ld64 links it: nothing here reads the exports.
//! - Any other form (another version, no install name, a relative one, a quoted name with an
//!   escape in it, a key given twice) is refused by name ([`TbdError`]), never guessed.

use core::fmt;

/// The first line of a TAPI text stub.
const HEAD: &str = "--- !tapi-tbd";

/// What a stub says of itself that the image takes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stub<'a> {
    /// The library's install name, an absolute path: what the load command names.
    pub install_name: &'a str,
}

/// Why a stub's bytes give no install name for arm64 macOS; `line` is 1-based.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TbdError {
    NotUtf8,
    /// The first line is not `--- !tapi-tbd`.
    NotTapi,
    /// The first document has no `tbd-version`.
    NoVersion,
    /// The `tbd-version` is not 4, the only form read.
    Version { line: u32 },
    /// The first document has no `install-name`.
    NoInstallName,
    /// The `install-name` is not a plain absolute path (a quoted one with an escape in it, an
    /// empty or a relative one).
    InstallName { line: u32 },
    /// The first document has no `targets`.
    NoTargets,
    /// The `targets` value is not a flow list `[ .. ]` closed before the stub ends.
    Targets { line: u32 },
    /// The targets do not list arm64 macOS.
    NotForMacosArm64,
    /// A key of the first document given twice.
    Twice { line: u32 },
}

impl fmt::Display for TbdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TbdError::NotUtf8 => write!(f, "its bytes are not UTF-8"),
            TbdError::NotTapi => write!(f, "its first line is not `{HEAD}`: it is no TAPI text stub"),
            TbdError::NoVersion => write!(f, "its first document has no `tbd-version`"),
            TbdError::Version { line } => write!(f, "its `tbd-version` (line {line}) is not 4, the only form read"),
            TbdError::NoInstallName => write!(f, "its first document has no `install-name`"),
            TbdError::InstallName { line } => write!(f, "its `install-name` (line {line}) is not a plain absolute path"),
            TbdError::NoTargets => write!(f, "its first document has no `targets`"),
            TbdError::Targets { line } => write!(f, "its `targets` (line {line}) is not a flow list `[ .. ]` closed before the stub ends"),
            TbdError::NotForMacosArm64 => write!(f, "its `targets` list neither `arm64-macos` nor `arm64e-macos`"),
            TbdError::Twice { line } => write!(f, "a key of its first document is given twice (line {line})"),
        }
    }
}

/// The install name the stub `bytes` states for arm64 macOS, or why it states none.
pub fn read(bytes: &[u8]) -> Result<Stub<'_>, TbdError> {
    let text = core::str::from_utf8(bytes).map_err(|_| TbdError::NotUtf8)?;
    let mut lines = text.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line)).zip(1u32..);
    match lines.next() {
        Some((first, _)) if first.trim_end() == HEAD => {}
        _ => return Err(TbdError::NotTapi),
    }
    let mut version: Option<(&str, u32)> = None;
    let mut install: Option<(&str, u32)> = None;
    let mut targets: Option<(bool, u32)> = None;
    while let Some((line, at)) = lines.next() {
        // The first document ends at YAML's end marker or the next document's start.
        if line == "..." || line.starts_with("---") {
            break;
        }
        // An indented line belongs to a nested value; a blank line or a comment is no key.
        if line.is_empty() || line.starts_with([' ', '\t', '#']) {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        match key {
            "tbd-version" => set(&mut version, (value, at), at)?,
            "install-name" => set(&mut install, (value, at), at)?,
            "targets" => {
                let found = flow_list_has_macos_arm64(value, at, &mut lines)?;
                set(&mut targets, (found, at), at)?;
            }
            _ => {}
        }
    }
    match version {
        None => return Err(TbdError::NoVersion),
        Some((written, line)) if written != "4" => return Err(TbdError::Version { line }),
        Some(_) => {}
    }
    match targets {
        None => return Err(TbdError::NoTargets),
        Some((false, _)) => return Err(TbdError::NotForMacosArm64),
        Some((true, _)) => {}
    }
    let (written, line) = install.ok_or(TbdError::NoInstallName)?;
    let install_name = scalar(written).filter(|path| path.len() > 1 && path.starts_with('/')).ok_or(TbdError::InstallName { line })?;
    Ok(Stub { install_name })
}

/// `slot` set to `value`, once.
fn set<T>(slot: &mut Option<T>, value: T, line: u32) -> Result<(), TbdError> {
    if slot.is_some() {
        return Err(TbdError::Twice { line });
    }
    *slot = Some(value);
    Ok(())
}

/// A YAML scalar as written: single-quoted, double-quoted or plain. A quoted scalar with an
/// escape in it (`''`, a backslash) is not taken: no install name has one.
fn scalar(value: &str) -> Option<&str> {
    if let Some(inner) = value.strip_prefix('\'') {
        let inner = inner.strip_suffix('\'')?;
        return (!inner.contains('\'')).then_some(inner);
    }
    if let Some(inner) = value.strip_prefix('"') {
        let inner = inner.strip_suffix('"')?;
        return (!inner.contains(['"', '\\'])).then_some(inner);
    }
    (!value.is_empty() && !value.contains(" #")).then_some(value)
}

/// Whether the flow list that opens `value` (on line `at`) and runs on over `lines` names arm64
/// macOS; the lines it runs over are consumed.
fn flow_list_has_macos_arm64<'t>(value: &'t str, at: u32, lines: &mut impl Iterator<Item = (&'t str, u32)>) -> Result<bool, TbdError> {
    let mut text = value.strip_prefix('[').ok_or(TbdError::Targets { line: at })?;
    let mut found = false;
    loop {
        let (inside, closed) = match text.split_once(']') {
            Some((inside, _)) => (inside, true),
            None => (text, false),
        };
        found |= inside.split(',').any(|target| macos_arm64(target.trim()));
        if closed {
            return Ok(found);
        }
        match lines.next() {
            Some((next, _)) => text = next,
            None => return Err(TbdError::Targets { line: at }),
        }
    }
}

/// `arm64-macos` or `arm64e-macos`.
fn macos_arm64(target: &str) -> bool {
    matches!(target.rsplit_once('-'), Some(("arm64" | "arm64e", "macos")))
}

#[cfg(test)]
mod tests {
    //! The stubs are the heads of the macOS 27.2 SDK's own (Foundation.tbd, libz.tbd,
    //! PushToTalk.tbd, libSystem.B.tbd's first two documents), cut where the read stops.

    use super::{Stub, TbdError, read};

    const FOUNDATION: &str = "--- !tapi-tbd\n\
tbd-version:     4\n\
targets:         [ x86_64-macos, x86_64-maccatalyst, arm64e-macos, arm64e-maccatalyst, \n\
                   arm64e.x1-macos, arm64e.x1-maccatalyst ]\n\
install-name:    '/System/Library/Frameworks/Foundation.framework/Versions/C/Foundation'\n\
current-version: 5027.1.8\n\
reexported-libraries:\n\
  - targets:         [ x86_64-macos, arm64e-macos ]\n\
    libraries:       [ '/usr/lib/libobjc.A.dylib' ]\n\
exports:\n\
  - targets:         [ arm64e-macos ]\n\
    symbols:         [ _NSLog ]\n\
...\n";

    /// A stub with no exports at all (PushToTalk.tbd's whole first document).
    const NO_EXPORTS: &str = "--- !tapi-tbd\n\
tbd-version:     4\n\
targets:         [ x86_64-macos, arm64e-macos, arm64e.x1-macos ]\n\
install-name:    '/System/Library/Frameworks/PushToTalk.framework/Versions/A/PushToTalk'\n\
...\n";

    /// libSystem.B.tbd: the first document is libSystem's; the second, a re-exported library's.
    const UMBRELLA: &str = "--- !tapi-tbd\n\
tbd-version:     4\n\
targets:         [ arm64e-macos ]\n\
install-name:    '/usr/lib/libSystem.B.dylib'\n\
reexported-libraries:\n\
  - targets:         [ arm64e-macos ]\n\
    libraries:       [ '/usr/lib/system/libcache.dylib' ]\n\
--- !tapi-tbd\n\
tbd-version:     4\n\
targets:         [ arm64e-macos ]\n\
install-name:    '/usr/lib/system/libcache.dylib'\n\
...\n";

    fn name(text: &str) -> Result<&str, TbdError> {
        read(text.as_bytes()).map(|Stub { install_name }| install_name)
    }

    /// The install name is the stub's, read: Foundation's under `Versions/C`, a stub with no
    /// exports linked all the same, and the umbrella's own name, not a re-exported one.
    /// TWIN (the target rule): the same stub listing only x86_64 is refused, and one listing
    /// plain `arm64-macos` on its second line is taken.
    ///
    /// Prints `tbd: Foundation /System/Library/Frameworks/Foundation.framework/Versions/C/Foundation`.
    #[test]
    fn the_install_name_is_the_first_documents() {
        assert_eq!(name(FOUNDATION), Ok("/System/Library/Frameworks/Foundation.framework/Versions/C/Foundation"));
        assert_eq!(name(NO_EXPORTS), Ok("/System/Library/Frameworks/PushToTalk.framework/Versions/A/PushToTalk"));
        assert_eq!(name(UMBRELLA), Ok("/usr/lib/libSystem.B.dylib"));
        let intel = "--- !tapi-tbd\ntbd-version: 4\ntargets: [ x86_64-macos ]\ninstall-name: '/usr/lib/libz.1.dylib'\n...\n";
        assert_eq!(name(intel), Err(TbdError::NotForMacosArm64));
        let later = "--- !tapi-tbd\ntbd-version: 4\ntargets: [ x86_64-macos,\n  arm64-macos ]\ninstall-name: /usr/lib/libz.1.dylib\n";
        assert_eq!(name(later), Ok("/usr/lib/libz.1.dylib"));
        std::println!("tbd: Foundation {}", name(FOUNDATION).unwrap_or_default());
    }

    /// Each other form is refused by name. CONTROL: the stub each case breaks reads.
    ///
    /// Prints `tbd: 8 forms refused`.
    #[test]
    fn other_forms_are_refused_by_name() {
        let good = "--- !tapi-tbd\ntbd-version: 4\ntargets: [ arm64e-macos ]\ninstall-name: '/usr/lib/libz.1.dylib'\n...\n";
        assert_eq!(name(good), Ok("/usr/lib/libz.1.dylib"));
        let cases: [(&str, TbdError); 8] = [
            ("{\"tapi_tbd_version\": 5}", TbdError::NotTapi),
            ("--- !tapi-tbd\ntbd-version: 3\ntargets: [ arm64e-macos ]\ninstall-name: '/usr/lib/libz.1.dylib'\n", TbdError::Version { line: 2 }),
            ("--- !tapi-tbd\ntargets: [ arm64e-macos ]\ninstall-name: '/usr/lib/libz.1.dylib'\n", TbdError::NoVersion),
            ("--- !tapi-tbd\ntbd-version: 4\ntargets: [ arm64e-macos ]\n...\ninstall-name: '/usr/lib/libz.1.dylib'\n", TbdError::NoInstallName),
            ("--- !tapi-tbd\ntbd-version: 4\ntargets: [ arm64e-macos ]\ninstall-name: 'libz.1.dylib'\n", TbdError::InstallName { line: 4 }),
            ("--- !tapi-tbd\ntbd-version: 4\ntargets: [ arm64e-macos,\n", TbdError::Targets { line: 3 }),
            ("--- !tapi-tbd\ntbd-version: 4\ninstall-name: '/usr/lib/libz.1.dylib'\n", TbdError::NoTargets),
            ("--- !tapi-tbd\ntbd-version: 4\ntargets: [ arm64e-macos ]\ninstall-name: '/a'\ninstall-name: '/b'\n", TbdError::Twice { line: 5 }),
        ];
        for (text, refused) in cases {
            assert_eq!(name(text), Err(refused), "{text}");
        }
        assert_eq!(read(&[0xFF, 0xFE]), Err(TbdError::NotUtf8));
        std::println!("tbd: {} forms refused", cases.len());
    }
}
