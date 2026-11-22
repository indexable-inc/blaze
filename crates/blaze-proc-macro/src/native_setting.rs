//! The native scripts' setting at the command's edge (D112, brief N1, port27/tempo-deps NOTES).
//!
//! Until the deterministic sandbox runs, the tool-spawning build scripts run in the Seatbelt
//! native runner ([`crate::native_unit::native_outputs`]); that runner needs a
//! [`NativeSetting`]: the pinned C toolchain's shelf, the macOS SDK, a work folder and a
//! deadline. [`command_native_setting`] is its one producer: the command calls it once, beside
//! [`crate::fact_pins::command_spawn_setting`], and hands the result to every script of the
//! graph through `script_unit::Setting::native` (rule 75: nothing below the command reads the
//! machine for it). Each thing it needs and does not find is a typed error naming the path; it
//! never looks anywhere else (rule 3).
//!
//! - The shelf is `<home>/.blaze/v2/toolchain/clang/<sha256 hex>`, the hex being the host's row of
//!   the clang pin table ([`CLANG_JSON`], `pins/clang.json`); its folder name is the
//!   compiler's digest in a native run's key, so the resolved folder must keep that name.
//! - The SDK is the folder the command was given (its own argument, or the edge's reading of the
//!   machine), resolved once; its `SDKSettings.json` must give a `Version`, which the run key
//!   holds ([`sdk_version`]). Only a host whose pin table row says `macos-sdk` takes one: any
//!   other sysroot is refused by name (a pinned glibc sysroot is not built yet).
//! - The work folder is `<home>/.blaze/v2/native-work/<command>`, the command's own: packages
//!   held in records are written out and each run's `OUT_DIR` made under it, by run key.
//! - The deadline is [`NATIVE_RUN_DEADLINE`].
//!
//! Records of native runs are keyed by the toolchain's and SDK's digests and the host tools'
//! digests, and marked local (host_tools.rs, MARK "host-tool"): never offered to a shared cache.

use core::time::Duration;
use std::{fs, io};

use blaze_build_script::toolchain::{self, PinError, Sysroot};
use blaze_pins::{CLANG_JSON, CLANG_PATH};
use blaze_own::{AString, Alloc, String};

use crate::native_unit::{NativeSetting, sdk_version};

/// Where the pinned C toolchains are unpacked, under the home: one folder per archive digest.
pub const SHELF_DIR: &str = ".blaze/v2/toolchain/clang";

/// Where each command's native runs write, under the home: `<this>/<command>`.
pub const NATIVE_WORK_DIR: &str = ".blaze/v2/native-work";

/// The deadline of one native script run (rule 18). The longest of the five (D112) is
/// librocksdb-sys, which compiles RocksDB's C++ sources (several hundred translation units)
/// in one run with the pinned clang; on a loaded Mac that is minutes. Thirty minutes bounds it;
/// a run past it is a finding (rule 69), never waited out.
pub const NATIVE_RUN_DEADLINE: Duration = Duration::from_secs(30 * 60);

/// The command's native setting, its paths owned; [`OwnedNativeSetting::setting`] lends the
/// [`NativeSetting`] a `script_unit::Setting` holds.
#[derive(Debug)]
pub struct OwnedNativeSetting<'a> {
    /// The resolved shelf, named by the pin's SHA-256.
    pub shelf: AString<'a>,
    /// The resolved SDK folder.
    pub sdk: AString<'a>,
    /// The SDK's `Version` (in every native run's key, as `macos-sdk <version>`).
    pub sdk_version: AString<'a>,
    /// The resolved work folder, made if absent.
    pub work: AString<'a>,
    pub deadline: Duration,
}

impl OwnedNativeSetting<'_> {
    /// The setting `script_unit::Setting::native` takes, with the command's jobserver `jobs`
    /// (`crate::jobserver::Jobserver::new`, its fifo under `work`).
    pub fn setting<'s>(&'s self, jobs: &'s crate::jobserver::Jobserver) -> NativeSetting<'s> {
        NativeSetting { shelf: self.shelf.as_str(), sdk: self.sdk.as_str(), work: self.work.as_str(), deadline: self.deadline, jobs }
    }
}

/// What [`NativeRun::new`] opens the native runner from: a resolved setting
/// ([`command_native_setting`], or [`native_setting`] for a named shelf), the fact shim binary
/// to place in its work folder (D112, N7) and the worker count the native runs' children share.
pub struct NativeRunOpts<'a, 's> {
    pub setting: OwnedNativeSetting<'a>,
    /// The fact shim binary (`blaze-fact-shim`), as the edge found it.
    pub shim: &'s str,
    pub workers: core::num::NonZeroU32,
}

/// The native runner, open for one command: its setting, the fact shim placed in its work
/// folder and the command's jobserver (its fifo under that folder, `workers - 1` tokens). Made
/// once a command by [`NativeRun::new`], the one place both a command and an example open it;
/// [`NativeRun::end`] removes the fifo when the command's scripts are over.
pub struct NativeRun<'a> {
    pub owned: OwnedNativeSetting<'a>,
    /// The placed shim's real path.
    pub shim: std::string::String,
    pub jobs: crate::jobserver::Jobserver,
}

/// Why the native runner could not be opened; each names the path.
#[derive(Debug)]
pub enum NativeRunError {
    /// The fact shim could not be placed (D112, N7).
    Shim(crate::native_facts::NativeFactsError),
    /// The jobserver could not be made.
    Jobs(crate::jobserver::JobserverError),
}

impl core::fmt::Display for NativeRunError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            NativeRunError::Shim(error) => write!(f, "the native runner's fact shim (D112, N7): {error}"),
            NativeRunError::Jobs(error) => write!(f, "the native runner's jobserver (D112): {error}"),
        }
    }
}

impl core::error::Error for NativeRunError {}

impl<'a> NativeRun<'a> {
    /// The runner over `opts.setting`: the shim copied into the work folder, then the
    /// jobserver made there. A failure of either is typed and names the path; nothing is left
    /// open by a failure (the jobserver is made last).
    pub fn new(opts: NativeRunOpts<'a, '_>) -> Result<NativeRun<'a>, NativeRunError> {
        let NativeRunOpts { setting, shim, workers } = opts;
        let shim = crate::native_facts::place_shim(setting.work.as_str(), shim).map_err(NativeRunError::Shim)?;
        let jobs = crate::jobserver::Jobserver::new(crate::jobserver::JobserverOpts { work: setting.work.as_str(), workers, id: std::process::id() }).map_err(NativeRunError::Jobs)?;
        Ok(NativeRun { owned: setting, shim, jobs })
    }

    /// The setting `script_unit::Setting::native` takes.
    pub fn setting(&self) -> NativeSetting<'_> {
        self.owned.setting(&self.jobs)
    }

    /// The jobserver ended: its descriptor closed, its fifo removed.
    pub fn end(&self) -> Result<(), crate::jobserver::JobserverError> {
        self.jobs.end()
    }
}

/// Why the command has no native setting; each names the path or the pin row.
#[derive(Debug)]
pub enum NativeSettingError<'a> {
    /// The pin table has no toolchain or no macOS SDK sysroot for the host.
    Pin(PinError<'a>),
    /// The host's sysroot is a pinned one (Linux's form): not built yet, refused by name.
    PinnedSysroot { host: &'a str },
    /// The pinned toolchain is not unpacked on the shelf.
    NoShelf { path: AString<'a>, error: io::Error },
    /// The shelf resolves to a folder not named by the pin's SHA-256 (its name is the key's
    /// compiler digest).
    ShelfNotPin { path: AString<'a>, real: AString<'a> },
    /// The shelf lacks a member of [`toolchain::SHELF_MEMBERS`] (an unpack from before the
    /// member was added: nm-row's `bin/llvm-nm`), named.
    ShelfIncomplete { shelf: AString<'a>, member: &'static str },
    /// The SDK folder or its `SDKSettings.json` could not be read.
    NoSdk { path: AString<'a>, error: io::Error },
    /// The SDK's `SDKSettings.json` gives no `Version`.
    SdkVersion { path: AString<'a> },
    /// The work folder could not be made or resolved.
    Work { path: AString<'a>, error: io::Error },
    /// A resolved path is not UTF-8 (Seatbelt's profile and the keys hold text).
    NotUtf8 { path: AString<'a> },
}

impl core::fmt::Display for NativeSettingError<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            NativeSettingError::Pin(error) => write!(f, "{error}: no native setting, so the tool-spawning build scripts cannot run (D112)"),
            NativeSettingError::PinnedSysroot { host } => write!(
                f,
                "{}: {host}'s sysroot is a pinned archive, which native scripts do not take yet (only `macos-sdk` is built, D112)",
                CLANG_PATH
            ),
            NativeSettingError::NoShelf { path, error } => write!(
                f,
                "{}: {error}: the pinned C toolchain ({}) is not unpacked there, so the tool-spawning build scripts cannot run (D112)",
                path.as_str(),
                CLANG_PATH
            ),
            NativeSettingError::ShelfNotPin { path, real } => write!(
                f,
                "{}: resolves to {}, whose name is not the pin's SHA-256 (the folder's name is the compiler's digest in the run key, D69)",
                path.as_str(),
                real.as_str()
            ),
            NativeSettingError::ShelfIncomplete { shelf, member } => write!(
                f,
                "{}/{member}: not on the shelf: the pinned archive ({}) was unpacked there without this member (toolchain::SHELF_MEMBERS, nm-row); unpack it from the same archive beside the rest, so the tool-spawning build scripts cannot run (D112)",
                shelf.as_str(),
                CLANG_PATH
            ),
            NativeSettingError::NoSdk { path, error } => write!(f, "{}: {error}: the macOS SDK the native build scripts compile against (D112)", path.as_str()),
            NativeSettingError::SdkVersion { path } => write!(f, "{}: no `Version`, which a native script's key holds (D69)", path.as_str()),
            NativeSettingError::Work { path, error } => write!(f, "{}: {error}: the native build scripts' work folder (D112)", path.as_str()),
            NativeSettingError::NotUtf8 { path } => write!(f, "{}: resolves to a path that is not UTF-8 (Seatbelt's profile holds text)", path.as_str()),
        }
    }
}

impl core::error::Error for NativeSettingError<'_> {}

fn text<'a>(parts: &[&str], alloc: Alloc<'a>) -> AString<'a> {
    let mut out: AString<'a> = String::new_in(alloc);
    for part in parts {
        out.push_str(part);
    }
    out
}

fn hex<'a>(bytes: &[u8; 32], alloc: Alloc<'a>) -> AString<'a> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out: AString<'a> = String::new_in(alloc);
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 15)]));
    }
    out
}

/// `path` resolved (Seatbelt matches resolved paths), as text in `alloc`; `missing` makes the
/// error for a path that cannot be resolved.
fn resolved<'a>(
    path: &AString<'a>,
    missing: impl FnOnce(AString<'a>, io::Error) -> NativeSettingError<'a>,
    alloc: Alloc<'a>,
) -> Result<AString<'a>, NativeSettingError<'a>> {
    let real = fs::canonicalize(path.as_str()).map_err(|error| missing(String::from_str_in(path.as_str(), alloc), error))?;
    let real = real.to_str().ok_or_else(|| NativeSettingError::NotUtf8 { path: String::from_str_in(path.as_str(), alloc) })?;
    Ok(String::from_str_in(real, alloc))
}

/// The command's native setting for scripts run on `host`: the shelf of the host's clang pin
/// under `home`, the SDK folder `sdk`, the work folder of `command` under `home` (made if
/// absent), and [`NATIVE_RUN_DEADLINE`]. Called once per command at its edge (rule 75); a
/// missing shelf or SDK is a typed error naming the path (rule 3).
pub fn command_native_setting<'a>(
    home: &str,
    sdk: &str,
    command: &str,
    host: &'a str,
    alloc: Alloc<'a>,
) -> Result<OwnedNativeSetting<'a>, NativeSettingError<'a>> {
    let digest = hex(&toolchain::clang_pin(CLANG_JSON, host).map_err(NativeSettingError::Pin)?.sha256, alloc);
    let shelf = text(&[home, "/", SHELF_DIR, "/", digest.as_str()], alloc);
    let work = text(&[home, "/", NATIVE_WORK_DIR, "/", command], alloc);
    native_setting(shelf.as_str(), sdk, work.as_str(), host, alloc)
}

/// The native setting from a named `shelf`, `sdk` and `work` folder (an example or a test that
/// names its own; a command takes [`command_native_setting`]): the shelf must resolve to a
/// folder named by `host`'s clang pin, the SDK must give its `Version`, the work folder is made
/// if absent. Each path is resolved once.
pub fn native_setting<'a>(
    shelf: &str,
    sdk: &str,
    work: &str,
    host: &'a str,
    alloc: Alloc<'a>,
) -> Result<OwnedNativeSetting<'a>, NativeSettingError<'a>> {
    let pin = toolchain::clang_pin(CLANG_JSON, host).map_err(NativeSettingError::Pin)?;
    match toolchain::sysroot(CLANG_JSON, host).map_err(NativeSettingError::Pin)? {
        Sysroot::MacosSdk => {}
        Sysroot::Pinned { .. } => return Err(NativeSettingError::PinnedSysroot { host }),
    }

    let digest = hex(&pin.sha256, alloc);
    let named = String::from_str_in(shelf, alloc);
    let shelf = resolved(&named, |path, error| NativeSettingError::NoShelf { path, error }, alloc)?;
    if shelf.as_str().rsplit('/').next() != Some(digest.as_str()) {
        return Err(NativeSettingError::ShelfNotPin { path: named, real: shelf });
    }
    // nm-row: every member the native runs start or load is on the shelf (a link must lead to
    // a file); a shelf unpacked before a member was added is refused naming it.
    let member_there = |member: &str| fs::metadata(text(&[shelf.as_str(), "/", member], alloc).as_str()).is_ok();
    if let Some(member) = toolchain::shelf_missing(member_there) {
        return Err(NativeSettingError::ShelfIncomplete { shelf, member });
    }

    let sdk = resolved(&String::from_str_in(sdk, alloc), |path, error| NativeSettingError::NoSdk { path, error }, alloc)?;
    let settings_path = text(&[sdk.as_str(), "/SDKSettings.json"], alloc);
    let settings = fs::read_to_string(settings_path.as_str()).map_err(|error| NativeSettingError::NoSdk { path: settings_path.clone(), error })?;
    let version = sdk_version(&settings).ok_or(NativeSettingError::SdkVersion { path: settings_path })?;
    let sdk_version = String::from_str_in(version, alloc);

    let work = String::from_str_in(work, alloc);
    fs::create_dir_all(work.as_str()).map_err(|error| NativeSettingError::Work { path: work.clone(), error })?;
    let work = resolved(&work, |path, error| NativeSettingError::Work { path, error }, alloc)?;

    Ok(OwnedNativeSetting { shelf, sdk, sdk_version, work, deadline: NATIVE_RUN_DEADLINE })
}
