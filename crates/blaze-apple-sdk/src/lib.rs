//! The macOS SDK: where its folder is, and what it is. Its one home (rule 30): the native
//! build-script runner (blaze-cli's scripts.rs, blaze-proc-macro's native setting) and the
//! linker (blaze-link's target.rs) take it from here.
//!
//! **Where** ([`resolve`]), as the platform's own tools find it (clang's driver, the `cc`
//! crate, rustc's Apple targets): `SDKROOT` when it is set, else the folder
//! `xcrun --sdk macosx --show-sdk-path` printed. An empty `SDKROOT` is unset, as those tools
//! read it. Either folder must be an absolute path whose `SDKSettings.json` gives a top-level
//! `Version`; one that does not is a refusal naming the value it came from (never the other
//! place tried instead: rule 3). With `SDKROOT` unset and xcrun failing, the refusal names both
//! attempts and says how to set `SDKROOT`.
//!
//! **What** ([`Sdk::identity`], an [`SdkId`]): its [`Version`], from `SDKSettings.json`'s
//! top-level `Version`, and its [`Build`], the `ProductBuildVersion` of its
//! [`SYSTEM_VERSION`]. A version alone is not one SDK's content: a beta and the release of
//! 27.2, or two re-spins, share `27.2` and differ in files. Apple gives each build of the
//! SDK's files its own id ("26B5044k", "26B118"), and one build shipped twice (the Command Line
//! Tools' and Xcode's 27.2, both 26B118) is the same bytes. A key of anything built or run
//! against the SDK holds the pair, from [`identity_of`] alone (rules 21, 30), never the
//! folder's path (rule 25): two machines with the SDK in two places share a key, and an SDK
//! upgraded in place moves every key that holds it. The version alone is what a Mach-O image's
//! `LC_BUILD_VERSION` says of the SDK ([`Sdk::version`], as [`Version::nibbles`]).
//!
//! **Edges.** Nothing here reads the environment, starts a program or reads a file (rules 14
//! and 75). The command's edge reads `SDKROOT` once at its start, runs xcrun ([`XCRUN`] with
//! [`XCRUN_ARGS`]) by [`XCRUN_DEADLINE`] (rule 18) only when it is unset, reads the settings
//! file ([`SETTINGS`]) and the build file ([`SYSTEM_VERSION`]), and hands what it read to
//! [`resolve`]; blaze-link's machine does the same through its `Ask`, so a test answers both.

#![no_std]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core, blaze::std_use, blaze::implicit_allocator, blaze::process_static)]
#![forbid(unsafe_code)]

// The tests' `format!` and `println!`: `core` has neither.
#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests;

use core::fmt;
use core::time::Duration;

/// The file in an SDK folder that says what it is: its top-level `Version` is half the identity.
pub const SETTINGS: &str = "SDKSettings.json";

/// The file in an SDK folder that says which build of it this is: its `ProductBuildVersion` is
/// the other half ([`Build`]).
pub const SYSTEM_VERSION: &str = "System/Library/CoreServices/SystemVersion.plist";

/// The most bytes a [`Build`] holds (Apple's are 5 to 9: "25F70", "26B5044k").
pub const BUILD_MOST: usize = 16;

/// The program asked where the SDK is when `SDKROOT` is unset: found on the edge's `PATH`, as
/// `cc` and clang's driver find it.
pub const XCRUN: &str = "xcrun";

/// Its arguments: the macOS SDK's folder, printed on one line.
pub const XCRUN_ARGS: [&str; 3] = ["--sdk", "macosx", "--show-sdk-path"];

/// How long xcrun is given (rule 18). It answers from its cache in milliseconds; a first run
/// after an Xcode update searches the developer folder, about a second. Past this it is killed
/// and the expiry is the refusal's words (rule 69), never waited out or retried.
pub const XCRUN_DEADLINE: Duration = Duration::from_secs(20);

/// An Apple version, `major[.minor[.patch]]`, as `xxxx.yy.zz` nibbles (`LC_BUILD_VERSION`'s
/// form): the SDK's identity, and a minimum OS (`MACOSX_DEPLOYMENT_TARGET`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Version(u32);

impl Version {
    /// `major.minor.patch`; `None` past `xxxx.yy.zz`.
    pub const fn new(major: u32, minor: u32, patch: u32) -> Option<Version> {
        match major <= 0xffff && minor <= 0xff && patch <= 0xff {
            true => Some(Version((major << 16) | (minor << 8) | patch)),
            false => None,
        }
    }

    /// `major[.minor[.patch]]` (surrounding white space allowed), each part decimal digits
    /// within its nibbles; `None` for anything else ("11.", "11.0.0.0", "+11", "11 .0").
    pub fn parse(text: &str) -> Option<Version> {
        let mut parts = text.trim().split('.');
        let part = |it: Option<&str>| -> Option<u32> {
            match it {
                None => Some(0),
                Some(it) if it.bytes().all(|b| b.is_ascii_digit()) => it.parse().ok(),
                Some(_) => None,
            }
        };
        let major = part(Some(parts.next()?))?;
        let minor = part(parts.next())?;
        let patch = part(parts.next())?;
        if parts.next().is_some() {
            return None;
        }
        Version::new(major, minor, patch)
    }

    /// As `LC_BUILD_VERSION` holds it.
    pub const fn nibbles(self) -> u32 {
        self.0
    }

    /// The bytes a key holds (rule 21): the nibbles, little-endian.
    pub const fn key(self) -> [u8; 4] {
        self.0.to_le_bytes()
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.0 >> 16, (self.0 >> 8) & 0xff)?;
        match self.0 & 0xff {
            0 => Ok(()),
            patch => write!(f, ".{patch}"),
        }
    }
}

/// Apple's id of one build of an SDK's files, as its [`SYSTEM_VERSION`]'s `ProductBuildVersion`
/// writes it ("26B118"): 1 to [`BUILD_MOST`] ASCII letters and digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Build {
    bytes: [u8; BUILD_MOST],
    len: u8,
}

impl Build {
    /// `text` (surrounding white space allowed) when it is 1 to [`BUILD_MOST`] ASCII letters
    /// and digits; `None` for anything else.
    pub fn parse(text: &str) -> Option<Build> {
        let text = text.trim().as_bytes();
        if text.is_empty() || text.len() > BUILD_MOST || !text.iter().all(u8::is_ascii_alphanumeric) {
            return None;
        }
        let mut bytes = [0; BUILD_MOST];
        bytes[..text.len()].copy_from_slice(text);
        Some(Build { bytes, len: u8::try_from(text.len()).ok()? })
    }
}

impl fmt::Display for Build {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for &byte in &self.bytes[..usize::from(self.len)] {
            fmt::Write::write_char(f, char::from(byte))?;
        }
        Ok(())
    }
}

/// The `ProductBuildVersion` of a `SystemVersion.plist`'s text (an XML property list: the
/// `<key>` then its `<string>`); `None` when it has none, or one that is no [`Build`].
pub fn system_version_build(text: &str) -> Option<Build> {
    let (_, after) = text.split_once("<key>ProductBuildVersion</key>")?;
    let (value, _) = after.trim_start().strip_prefix("<string>")?.split_once("</string>")?;
    Build::parse(value)
}

/// What a key of anything built or run against the SDK holds (rule 21): its version and its
/// build, never its folder.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SdkId {
    pub version: Version,
    pub build: Build,
}

impl SdkId {
    /// How many bytes [`SdkId::key`] gives.
    pub const KEY_LEN: usize = 4 + 1 + BUILD_MOST;

    /// The bytes a key holds: the version's nibbles little-endian, the build's length, its
    /// bytes padded with zeros. Fixed width, so no two identities give one key.
    pub fn key(&self) -> [u8; SdkId::KEY_LEN] {
        let mut out = [0; SdkId::KEY_LEN];
        out[..4].copy_from_slice(&self.version.key());
        out[4] = self.build.len;
        out[5..].copy_from_slice(&self.build.bytes);
        out
    }
}

impl fmt::Display for SdkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.version, self.build)
    }
}

/// The identity of the SDK whose [`SETTINGS`] reads `settings` and whose [`SYSTEM_VERSION`]
/// reads `system_version`, with its `Version` as written: the one function every key's SDK part
/// comes from (rule 30).
pub fn identity_of<'t>(settings: &'t str, system_version: &str) -> Result<(&'t str, SdkId), NotSdk<'t>> {
    let (written, version) = settings_version(settings).ok_or(NotSdk::NoVersion)?;
    let build = system_version_build(system_version).ok_or(NotSdk::NoBuild)?;
    Ok((written, SdkId { version, build }))
}

/// The top-level `Version` of an `SDKSettings.json`'s text, as written and as a [`Version`]:
/// only the outermost object's key counts (a `VersionMap`'s nested `Version` never does).
/// `None` when it has none, or one that is not a version.
pub fn settings_version(text: &str) -> Option<(&str, Version)> {
    let written = top_level_string(text, "Version")?;
    Some((written, Version::parse(written)?))
}

/// The string value of `key` in the outermost object of the JSON `text`, as it is written;
/// `None` when it has none or is not well formed there.
fn top_level_string<'t>(text: &'t str, key: &str) -> Option<&'t str> {
    let bytes = text.as_bytes();
    let mut depth = 0u32;
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'{' | b'[' => {
                depth += 1;
                at += 1;
            }
            b'}' | b']' => {
                depth = depth.checked_sub(1)?;
                at += 1;
            }
            b'"' => {
                let (string, end) = string_at(text, at)?;
                at = end;
                if depth != 1 || string != key {
                    continue;
                }
                let Some(value) = text[at..].trim_start().strip_prefix(':') else { continue };
                let value = value.trim_start();
                if !value.starts_with('"') {
                    return None;
                }
                return string_at(text, text.len() - value.len()).map(|it| it.0);
            }
            _ => at += 1,
        }
    }
    None
}

/// The JSON string whose `"` is at `at`, and where it ends.
fn string_at(text: &str, at: usize) -> Option<(&str, usize)> {
    let bytes = text.as_bytes();
    let mut i = at + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some((text.get(at + 1..i)?, i + 1)),
            _ => i += 1,
        }
    }
    None
}

/// Where the folder came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Found {
    /// `SDKROOT`.
    SdkRoot,
    /// What xcrun printed, `SDKROOT` being unset.
    Xcrun,
}

/// The macOS SDK, resolved and checked once ([`resolve`]): only it makes one, so holding one is
/// the proof its folder has settings with a version (rule 74).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Sdk<'a> {
    folder: &'a str,
    found: Found,
    id: SdkId,
    written: &'a str,
}

impl<'a> Sdk<'a> {
    /// Its folder, as the machine names it: for the programs run against it (`-isysroot`,
    /// `SDKROOT` in a native script's environment) and the linker's search, never a key.
    pub fn folder(&self) -> &'a str {
        self.folder
    }

    /// Where its folder came from.
    pub fn found(&self) -> Found {
        self.found
    }

    /// What a key of anything built or run against it holds (rule 21): its version and build,
    /// never its folder (rule 25).
    pub fn identity(&self) -> SdkId {
        self.id
    }

    /// Its version alone: what a Mach-O image's `LC_BUILD_VERSION` says of it, never a key.
    pub fn version(&self) -> Version {
        self.id.version
    }

    /// Its `Version` as `SDKSettings.json` writes it.
    pub fn version_text(&self) -> &'a str {
        self.written
    }
}

/// How xcrun failed to say where the SDK is, as the edge saw it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum XcrunFailed<'a> {
    /// It could not be started, or a runner said why it failed: the system's words.
    Failed { why: &'a str },
    /// It exited with `status`, saying `said` on its error stream.
    Exited { status: i32, said: &'a str },
    /// A signal ended it.
    Signaled { signal: i32 },
    /// It was still running at the deadline ([`XCRUN_DEADLINE`]), and was killed.
    Expired { after: Duration },
    /// It printed what is not UTF-8.
    NotText,
    /// The host is not macOS: there is no xcrun to ask.
    NotMacos,
}

impl fmt::Display for XcrunFailed<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            XcrunFailed::Failed { why } => write!(f, "failed: {why}"),
            XcrunFailed::Exited { status, said } => write!(f, "exited with status {status}: {said}"),
            XcrunFailed::Signaled { signal } => write!(f, "was ended by signal {signal}"),
            XcrunFailed::Expired { after } => write!(f, "did not end within {} ms and was killed (rule 18)", after.as_millis()),
            XcrunFailed::NotText => f.write_str("printed what is not UTF-8"),
            XcrunFailed::NotMacos => f.write_str("is macOS's, and this host is not macOS"),
        }
    }
}

/// Why the settings file of a folder could not be read: the system's words.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Unread<'a> {
    pub why: &'a str,
}

/// Why a folder is not an SDK folder.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NotSdk<'a> {
    /// It is not an absolute path (`SDKROOT=macosx`, an SDK's name, is xcrun's form, which the
    /// tools blaze stands for do not take).
    NotAbsolute,
    /// Its [`SETTINGS`] could not be read.
    Unread(Unread<'a>),
    /// Its [`SETTINGS`] has no top-level `Version` that is `major[.minor[.patch]]`.
    NoVersion,
    /// Its [`SYSTEM_VERSION`] could not be read.
    BuildUnread(Unread<'a>),
    /// Its [`SYSTEM_VERSION`] has no `ProductBuildVersion` that is a [`Build`].
    NoBuild,
}

impl fmt::Display for NotSdk<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NotSdk::NotAbsolute => f.write_str("it is not an absolute path"),
            NotSdk::Unread(Unread { why }) => write!(f, "its {SETTINGS} cannot be read ({why})"),
            NotSdk::NoVersion => write!(f, "its {SETTINGS} has no top-level `Version` that is `major[.minor[.patch]]`"),
            NotSdk::BuildUnread(Unread { why }) => write!(f, "its {SYSTEM_VERSION} cannot be read ({why})"),
            NotSdk::NoBuild => write!(f, "its {SYSTEM_VERSION} has no `ProductBuildVersion` of 1 to {BUILD_MOST} ASCII letters and digits"),
        }
    }
}

/// Why the command has no macOS SDK: each names the value it looked at, or both attempts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SdkRefused<'a> {
    /// `SDKROOT` is set to `value`, which is no SDK folder.
    SdkRoot { value: &'a str, why: NotSdk<'a> },
    /// `SDKROOT` is unset, and xcrun printed `printed`, which is no SDK folder.
    Xcrun { printed: &'a str, why: NotSdk<'a> },
    /// `SDKROOT` is unset, and xcrun did not say.
    Neither { xcrun: XcrunFailed<'a> },
}

/// How to give blaze the SDK, the end of every refusal but `SDKROOT`'s own.
const HOW: &str = "set SDKROOT to the SDK folder (`export SDKROOT=\"$(xcrun --sdk macosx --show-sdk-path)\"`, or a folder such as /Library/Developer/CommandLineTools/SDKs/MacOSX.sdk), or install the Command Line Tools (`xcode-select --install`)";

impl fmt::Display for SdkRefused<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c] = XCRUN_ARGS;
        match self {
            SdkRefused::SdkRoot { value, why } => write!(
                f,
                "the macOS SDK: SDKROOT={value} names no SDK folder: {why}; set SDKROOT to an SDK folder (`{XCRUN} {a} {b} {c}` prints one), or unset it to take the one xcrun names"
            ),
            SdkRefused::Xcrun { printed, why } => {
                write!(f, "the macOS SDK: SDKROOT is not set, and `{XCRUN} {a} {b} {c}` printed {printed}, which is no SDK folder: {why}; {HOW}")
            }
            SdkRefused::Neither { xcrun } => write!(f, "the macOS SDK: SDKROOT is not set, and `{XCRUN} {a} {b} {c}` {xcrun}; {HOW}"),
        }
    }
}

impl core::error::Error for SdkRefused<'_> {}

/// The macOS SDK from what the edge read: `sdkroot`, `SDKROOT`'s value (an empty one is unset);
/// `xcrun`, asked only when that is unset, what xcrun printed or how it failed; `settings`, the
/// text of a folder's [`SETTINGS`] (`<folder>/SDKSettings.json`), and `system_version`, the
/// text of its [`SYSTEM_VERSION`], each asked once for the one folder chosen (the second only
/// when the first was read). No other place is tried when the chosen folder is refused (rule 3).
pub fn resolve<'a>(
    sdkroot: Option<&'a str>,
    xcrun: impl FnOnce() -> Result<&'a str, XcrunFailed<'a>>,
    settings: impl FnOnce(&'a str) -> Result<&'a str, Unread<'a>>,
    system_version: impl FnOnce(&'a str) -> Result<&'a str, Unread<'a>>,
) -> Result<Sdk<'a>, SdkRefused<'a>> {
    let (folder, found) = match sdkroot.filter(|value| !value.is_empty()) {
        Some(value) => (value, Found::SdkRoot),
        None => (xcrun().map_err(|xcrun| SdkRefused::Neither { xcrun })?.trim(), Found::Xcrun),
    };
    let refuse = |why: NotSdk<'a>| match found {
        Found::SdkRoot => SdkRefused::SdkRoot { value: folder, why },
        Found::Xcrun => SdkRefused::Xcrun { printed: folder, why },
    };
    if !folder.starts_with('/') {
        return Err(refuse(NotSdk::NotAbsolute));
    }
    let text = settings(folder).map_err(|unread| refuse(NotSdk::Unread(unread)))?;
    let built = system_version(folder).map_err(|unread| refuse(NotSdk::BuildUnread(unread)))?;
    let (written, id) = identity_of(text, built).map_err(refuse)?;
    Ok(Sdk { folder, found, id, written })
}
