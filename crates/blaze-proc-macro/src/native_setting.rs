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
use std::{
    fs,
    io::{self, Read as _},
};

use blaze_build_script::toolchain::{self, PinError, Sysroot};
use blaze_pins::{CLANG_JSON, CLANG_PATH};
use blaze_own::{AString, AVec, Alloc, String, Vec};

use crate::native_unit::{NativeSetting, Rpaths, sdk_version};

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
    /// The resolved SDK/sysroot folder.
    pub sdk: AString<'a>,
    /// Optional extra read-only root for split Linux kernel headers (e.g. NixOS where
    /// `<glibc-dev>/include/linux` symlinks into `/nix/store/<hash>-linux-headers-<ver>/include/linux`).
    pub sdk_extra: Option<AString<'a>>,
    /// Optional split Linux libc runtime library directory (e.g. NixOS
    /// `/nix/store/<hash>-glibc-<ver>/lib`, holding `crt1.o`, `libc.so`, `libc_nonshared.a`) used
    /// when process-tree steps (`configure`/`make`) link against glibc under Landlock.
    pub sdk_lib: Option<AString<'a>>,
    /// Optional resolved ELF dynamic linker path (e.g.
    /// `/nix/store/<hash>-glibc-<ver>/lib/ld-linux-aarch64.so.1`) preserved from Nix wrapper
    /// metadata (`nix-support/dynamic-linker` / `libc-ldflags`) or discovered inside `sdk_lib`.
    pub sdk_dynamic_linker: Option<AString<'a>>,
    /// Optional extra runtime library search directories (`-rpath`) preserved from Nix wrapper
    /// `nix-support/libc-ldflags` / `NIX_LDFLAGS`, validated and hashed into `sdk_version`.
    pub sdk_rpaths: AVec<'a, AString<'a>>,
    /// NUL-separated view of `sdk_rpaths` so [`NativeSetting`] stays `Copy + Send + Sync`.
    sdk_rpaths_nul: AString<'a>,
    /// The SDK's `Version` (macOS) or marker/tree SHA-256 hex (Linux host).
    pub sdk_version: AString<'a>,
    /// Which sysroot model `pins/clang.json` selected for the host.
    pub sysroot: Sysroot,
    /// The resolved work folder, made if absent.
    pub work: AString<'a>,
    pub deadline: Duration,
}

impl OwnedNativeSetting<'_> {
    /// The setting `script_unit::Setting::native` takes, with the command's jobserver `jobs`
    /// (`crate::jobserver::Jobserver::new`, its fifo under `work`).
    pub fn setting<'s>(&'s self, jobs: &'s crate::jobserver::Jobserver) -> NativeSetting<'s> {
        NativeSetting {
            shelf: self.shelf.as_str(),
            sdk: self.sdk.as_str(),
            sdk_extra: self.sdk_extra.as_deref(),
            sdk_lib: self.sdk_lib.as_deref(),
            sdk_dynamic_linker: self.sdk_dynamic_linker.as_deref(),
            sdk_rpaths: Rpaths(self.sdk_rpaths_nul.as_str()),
            sdk_version: self.sdk_version.as_str(),
            sysroot: self.sysroot,
            work: self.work.as_str(),
            deadline: self.deadline,
            jobs,
            shim: None,
        }
    }
}

/// Which sysroot model `pins/clang.json` selects for `host` (e.g. `MacosSdk` vs `LinuxHost`).
pub fn host_sysroot<'t>(host: &'t str) -> Result<Sysroot, PinError<'t>> {
    toolchain::sysroot(CLANG_JSON, host)
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
        NativeSetting { shim: Some(self.shim.as_str()), ..self.owned.setting(&self.jobs) }
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
            NativeSettingError::NoSdk { path, error } => write!(f, "{}: {error}: the target SDK/sysroot the native build scripts compile against (D112)", path.as_str()),
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
/// Optional Linux host runtime link inputs resolved at the command's edge (NixOS split glibc
/// runtime library dir, ELF dynamic linker path, and preserved `-rpath` directories).
#[derive(Clone, Copy, Default)]
pub struct LinuxLinkInputs<'s> {
    pub lib_dir: Option<&'s str>,
    pub dynamic_linker: Option<&'s str>,
    pub rpaths: &'s [&'s str],
}

pub fn command_native_setting<'a>(
    home: &str,
    sdk: &str,
    command: &str,
    host: &'a str,
    alloc: Alloc<'a>,
) -> Result<OwnedNativeSetting<'a>, NativeSettingError<'a>> {
    command_native_setting_with_link(home, sdk, LinuxLinkInputs::default(), command, host, alloc)
}

/// Same as [`command_native_setting`], optionally naming a separate Linux libc link-library
/// directory (`sdk_lib`, e.g. NixOS `/nix/store/<hash>-glibc-<ver>/lib`) to validate, mount
/// read-only for process-tree linking, and include in `sdk_version`.
pub fn command_native_setting_with_lib<'a>(
    home: &str,
    sdk: &str,
    sdk_lib: Option<&str>,
    command: &str,
    host: &'a str,
    alloc: Alloc<'a>,
) -> Result<OwnedNativeSetting<'a>, NativeSettingError<'a>> {
    command_native_setting_with_link(
        home,
        sdk,
        LinuxLinkInputs { lib_dir: sdk_lib, dynamic_linker: None, rpaths: &[] },
        command,
        host,
        alloc,
    )
}

/// Same as [`command_native_setting`], with full optional NixOS Linux runtime link inputs
/// (`lib_dir`, `dynamic_linker`, and preserved `rpaths`).
pub fn command_native_setting_with_link<'a>(
    home: &str,
    sdk: &str,
    link: LinuxLinkInputs<'_>,
    command: &str,
    host: &'a str,
    alloc: Alloc<'a>,
) -> Result<OwnedNativeSetting<'a>, NativeSettingError<'a>> {
    let digest = hex(&toolchain::clang_pin(CLANG_JSON, host).map_err(NativeSettingError::Pin)?.sha256, alloc);
    let shelf = text(&[home, "/", SHELF_DIR, "/", digest.as_str()], alloc);
    let work = text(&[home, "/", NATIVE_WORK_DIR, "/", command], alloc);
    native_setting_with_link(shelf.as_str(), sdk, link, work.as_str(), host, alloc)
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
    native_setting_with_link(shelf, sdk, LinuxLinkInputs::default(), work, host, alloc)
}

/// Explicit bound on any single SDK marker, header, or runtime library file read while keying
/// a host sysroot (rule 18): rejects non-regular targets (FIFOs, `/dev/zero`, sockets) BEFORE
/// opening so malformed or hostile host sysroots can never hang or exhaust memory.
const MAX_SDK_FILE_BYTES: u64 = 64 * 1024 * 1024;

fn stat_meta(path: &str) -> io::Result<fs::Metadata> {
    fs::metadata(path)
}

fn open_regular(path: &str) -> io::Result<(fs::File, u64)> {
    let meta = stat_meta(path)?;
    if !meta.is_file() {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    let expected_len = meta.len();
    if expected_len > MAX_SDK_FILE_BYTES {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    let file = fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    Ok((file, expected_len))
}

fn read_bounded<'a>(path: &str, alloc: Alloc<'a>) -> io::Result<AVec<'a, u8>> {
    let (mut file, expected_len) = open_regular(path)?;
    let cap = usize::try_from(expected_len).unwrap_or(0);
    let mut out: AVec<'a, u8> = Vec::with_capacity_in(cap, alloc);
    let mut buf = [0u8; 16 * 1024];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total = total.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
        if total > MAX_SDK_FILE_BYTES {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        if let Some(chunk) = buf.get(..n) {
            out.extend_from_slice(chunk);
        }
    }
    Ok(out)
}

fn hash_regular(sha: &mut blaze_sha256::Sha256, path: &str) -> io::Result<()> {
    let (mut file, expected_len) = open_regular(path)?;
    sha.update(&expected_len.to_le_bytes());
    let mut buf = [0u8; 16 * 1024];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total = total.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
        if total > MAX_SDK_FILE_BYTES {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        if let Some(chunk) = buf.get(..n) {
            sha.update(chunk);
        }
    }
    if total != expected_len {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    Ok(())
}

/// Resolved and content-keyed Linux host C library sysroot (headers + optional split NixOS
/// kernel headers, libc runtime libraries, dynamic linker, and rpaths), produced independently
/// of the clang shelf so script task keys (`scripts::parts`) can hash the Linux sysroot identity
/// even before a shelf is checked (Rule 21, Rule 30).
#[derive(Debug)]
pub struct ResolvedLinuxSysroot<'a> {
    pub sdk: AString<'a>,
    pub sdk_extra: Option<AString<'a>>,
    pub sdk_lib: Option<AString<'a>>,
    pub sdk_dynamic_linker: Option<AString<'a>>,
    pub sdk_rpaths: AVec<'a, AString<'a>>,
    pub sdk_version: AString<'a>,
    pub sdk_digest: [u8; 32],
}

/// Resolves and hashes a `Sysroot::LinuxHost` sysroot (`sdk` + optional NixOS `link` inputs)
/// into its canonical roots and path-independent 32-byte SHA-256 content digest.
pub fn resolve_linux_sysroot<'a>(
    sdk: &str,
    link: LinuxLinkInputs<'_>,
    alloc: Alloc<'a>,
) -> Result<ResolvedLinuxSysroot<'a>, NativeSettingError<'a>> {
    let sdk = resolved(&String::from_str_in(sdk, alloc), |path, error| NativeSettingError::NoSdk { path, error }, alloc)?;
    let settings_path = text(&[sdk.as_str(), "/", toolchain::LINUX_HOST_SYSROOT_MARKER], alloc);
    let settings = read_bounded(settings_path.as_str(), alloc)
        .map_err(|error| NativeSettingError::NoSdk { path: settings_path, error })?;
    // On NixOS, `<glibc-dev>/include/linux` (and `asm/`, `asm-generic/`) symlinks into a
    // separate `/nix/store/<hash>-linux-headers-<ver>/include/...` store path. Mount that
    // root read-only beside `sdk` so Landlock permits following kernel-header symlinks.
    let linux_inc = text(&[sdk.as_str(), "/include/linux"], alloc);
    let sdk_extra = match resolved(&linux_inc, |path, error| NativeSettingError::NoSdk { path, error }, alloc) {
        Ok(real_linux) => {
            let inside_sdk = real_linux.as_str().strip_prefix(sdk.as_str()).is_some_and(|rest| rest.starts_with('/'));
            if inside_sdk {
                None
            } else {
                real_linux
                    .as_str()
                    .strip_suffix("/include/linux")
                    .filter(|root| !root.is_empty())
                    .map(|root| String::from_str_in(root, alloc))
            }
        }
        Err(NativeSettingError::NoSdk { error, .. })
            if matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) =>
        {
            None
        }
        Err(error) => return Err(error),
    };
    let sdk_lib = match link.lib_dir {
        Some(lib) => Some(resolved(&String::from_str_in(lib, alloc), |path, error| NativeSettingError::NoSdk { path, error }, alloc)?),
        None => None,
    };
    let mut sdk_dynamic_linker = match link.dynamic_linker {
        Some(dyn_linker) => Some(resolved(&String::from_str_in(dyn_linker, alloc), |path, error| NativeSettingError::NoSdk { path, error }, alloc)?),
        None => None,
    };
    let mut sdk_rpaths: AVec<'a, AString<'a>> = Vec::new_in(alloc);
    for raw_rpath in link.rpaths {
        let real_rpath = resolved(&String::from_str_in(raw_rpath, alloc), |path, error| NativeSettingError::NoSdk { path, error }, alloc)?;
        let same_as_lib = sdk_lib.as_ref().is_some_and(|l| l.as_str() == real_rpath.as_str());
        if !same_as_lib && !sdk_rpaths.iter().any(|seen| seen.as_str() == real_rpath.as_str()) {
            sdk_rpaths.push(real_rpath);
        }
    }
    // Standard FHS `/usr` without split kernel headers or split libc runtime stays keyed by
    // `include/features.h` so we never scan unrelated packages under `/usr/include`. Flat or
    // split sysroots (e.g. NixOS `<glibc-dev>` + `<linux-headers>` + `<glibc-runtime>/lib`)
    // hash every file byte and symlinked directory subtree under `<sdk>/include`,
    // `<sdk_extra>/include`, `<sdk_lib>`, and any extra runtime rpaths / dynamic linker in
    // sorted relative-path order so `sdk_version` is a complete, path-independent content
    // identity (Rule 21, Rule 25).
    let is_fhs_single = sdk.as_str().ends_with("/usr")
        && sdk_extra.is_none()
        && sdk_lib.is_none()
        && sdk_dynamic_linker.is_none()
        && sdk_rpaths.is_empty();
    let sdk_digest = if is_fhs_single {
        blaze_sha256::digest(&settings)
    } else {
        let mut sha = blaze_sha256::Sha256::new();
        sha.update(b"blaze linux-host sysroot headers 1\0");
        let hash_dir = |sha: &mut blaze_sha256::Sha256,
                        root_dir: &str,
                        tag: &[u8],
                        discover_loader: &mut Option<AString<'a>>|
         -> Result<(), NativeSettingError<'a>> {
            const MAX_LINK_DEPTH: usize = 16;
            let root_real = resolved(&String::from_str_in(root_dir, alloc), |path, error| NativeSettingError::NoSdk { path, error }, alloc)?;
            let mut init_anc: AVec<'a, AString<'a>> = Vec::new_in(alloc);
            init_anc.push(String::from_str_in(root_real.as_str(), alloc));
            let mut stack: AVec<'a, (AString<'a>, AString<'a>, AVec<'a, AString<'a>>)> = Vec::new_in(alloc);
            stack.push((String::from_str_in("", alloc), root_real, init_anc));
            sha.update(tag);
            while let Some((prefix, cur_dir, ancestors)) = stack.pop() {
                let listed = crate::native_script::listed_under(cur_dir.as_str()).map_err(|_| NativeSettingError::NoSdk {
                    path: String::from_str_in(cur_dir.as_str(), alloc),
                    error: io::Error::from(io::ErrorKind::Other),
                })?;
                sha.update(b"D");
                sha.update(&u64::try_from(prefix.len()).unwrap_or(u64::MAX).to_le_bytes());
                sha.update(prefix.as_bytes());
                sha.update(&u64::try_from(listed.len()).unwrap_or(u64::MAX).to_le_bytes());
                let mut sub_dirs: AVec<'a, (AString<'a>, AString<'a>, AVec<'a, AString<'a>>)> = Vec::new_in(alloc);
                for entry in listed {
                    let name = entry.name.as_str();
                    let full = text(&[cur_dir.as_str(), "/", name], alloc);
                    if discover_loader.is_none()
                        && prefix.is_empty()
                        && !name.contains('/')
                        && name.starts_with("ld-linux")
                        && name.contains(".so")
                        && matches!(entry.kind, bstd::fs::EntryKind::File | bstd::fs::EntryKind::Link)
                    {
                        *discover_loader = Some(String::from_str_in(full.as_str(), alloc));
                    }
                    match entry.kind {
                        bstd::fs::EntryKind::File => {
                            sha.update(b"F");
                            sha.update(&u64::try_from(name.len()).unwrap_or(u64::MAX).to_le_bytes());
                            sha.update(name.as_bytes());
                            hash_regular(sha, full.as_str())
                                .map_err(|error| NativeSettingError::NoSdk { path: full, error })?;
                        }
                                bstd::fs::EntryKind::Link => {
                                    sha.update(b"L");
                                    sha.update(&u64::try_from(name.len()).unwrap_or(u64::MAX).to_le_bytes());
                                    sha.update(name.as_bytes());
                                    let meta = stat_meta(full.as_str())
                                        .map_err(|error| NativeSettingError::NoSdk { path: String::from_str_in(full.as_str(), alloc), error })?;
                            if meta.is_file() {
                                sha.update(b"f");
                                hash_regular(sha, full.as_str())
                                    .map_err(|error| NativeSettingError::NoSdk { path: full, error })?;
                            } else if meta.is_dir() {
                                let real_target = resolved(
                                    &full,
                                    |path, err| NativeSettingError::NoSdk { path, error: err },
                                    alloc,
                                )?;
                                if ancestors.len() >= MAX_LINK_DEPTH
                                    || ancestors.iter().any(|a| a.as_str() == real_target.as_str())
                                {
                                    return Err(NativeSettingError::NoSdk {
                                        path: full,
                                        error: io::Error::from(io::ErrorKind::InvalidInput),
                                    });
                                }
                                sha.update(b"d");
                                let mut next_anc: AVec<'a, AString<'a>> = Vec::with_capacity_in(ancestors.len() + 1, alloc);
                                for a in ancestors.iter() {
                                    next_anc.push(String::from_str_in(a.as_str(), alloc));
                                }
                                next_anc.push(String::from_str_in(real_target.as_str(), alloc));
                                let next_prefix = if prefix.is_empty() {
                                    String::from_str_in(name, alloc)
                                } else {
                                    text(&[prefix.as_str(), "/", name], alloc)
                                };
                                sub_dirs.push((next_prefix, real_target, next_anc));
                            } else {
                                return Err(NativeSettingError::NoSdk {
                                    path: full,
                                    error: io::Error::from(io::ErrorKind::InvalidInput),
                                });
                            }
                        }
                        _ => {
                            sha.update(b"O");
                            sha.update(&u64::try_from(name.len()).unwrap_or(u64::MAX).to_le_bytes());
                            sha.update(name.as_bytes());
                        }
                    }
                }
                while let Some(sub) = sub_dirs.pop() {
                    stack.push(sub);
                }
            }
            Ok(())
        };
        let mut ignored_loader: Option<AString<'a>> = None;
        let sdk_inc = text(&[sdk.as_str(), "/include"], alloc);
        hash_dir(&mut sha, sdk_inc.as_str(), b"sdk\0", &mut ignored_loader)?;
        if let Some(extra) = sdk_extra.as_ref() {
            let extra_inc = text(&[extra.as_str(), "/include"], alloc);
            hash_dir(&mut sha, extra_inc.as_str(), b"extra\0", &mut ignored_loader)?;
        }
        if let Some(lib) = sdk_lib.as_ref() {
            let mut discovered_loader: Option<AString<'a>> = None;
            hash_dir(&mut sha, lib.as_str(), b"lib\0", &mut discovered_loader)?;
            if sdk_dynamic_linker.is_none()
                && let Some(found) = discovered_loader
            {
                sdk_dynamic_linker = Some(resolved(&found, |path, error| NativeSettingError::NoSdk { path, error }, alloc)?);
            }
        }
        if let Some(dyn_linker) = sdk_dynamic_linker.as_ref() {
            let loader_name = dyn_linker.as_str().rsplit('/').next().unwrap_or("");
            sha.update(b"dyn\0");
            sha.update(&u64::try_from(loader_name.len()).unwrap_or(u64::MAX).to_le_bytes());
            sha.update(loader_name.as_bytes());
            hash_regular(&mut sha, dyn_linker.as_str())
                .map_err(|error| NativeSettingError::NoSdk { path: String::from_str_in(dyn_linker.as_str(), alloc), error })?;
        }
        for rpath in sdk_rpaths.iter() {
            hash_dir(&mut sha, rpath.as_str(), b"rpath\0", &mut ignored_loader)?;
        }
        sha.finish()
    };
    let sdk_version = hex(&sdk_digest, alloc);
    Ok(ResolvedLinuxSysroot { sdk, sdk_extra, sdk_lib, sdk_dynamic_linker, sdk_rpaths, sdk_version, sdk_digest })
}

/// [`native_setting`] with optional Linux host runtime link inputs (`lib_dir`, `dynamic_linker`,
/// `rpaths`).
pub fn native_setting_with_link<'a>(
    shelf: &str,
    sdk: &str,
    link: LinuxLinkInputs<'_>,
    work: &str,
    host: &'a str,
    alloc: Alloc<'a>,
) -> Result<OwnedNativeSetting<'a>, NativeSettingError<'a>> {
    let pin = toolchain::clang_pin(CLANG_JSON, host).map_err(NativeSettingError::Pin)?;
    let sysroot = toolchain::sysroot(CLANG_JSON, host).map_err(NativeSettingError::Pin)?;
    if matches!(sysroot, Sysroot::Pinned { .. }) {
        return Err(NativeSettingError::PinnedSysroot { host });
    }

    let digest = hex(&pin.sha256, alloc);
    let named = String::from_str_in(shelf, alloc);
    let shelf = resolved(&named, |path, error| NativeSettingError::NoShelf { path, error }, alloc)?;
    if shelf.as_str().rsplit('/').next() != Some(digest.as_str()) {
        return Err(NativeSettingError::ShelfNotPin { path: named, real: shelf });
    }
    // nm-row: every member the native runs start or load is on the shelf (a link must lead to
    // a file); a shelf unpacked before a member was added is refused naming it.
    let member_there = |member: &str| stat_meta(text(&[shelf.as_str(), "/", member], alloc).as_str()).is_ok();
    if let Some(member) = toolchain::shelf_missing_for(sysroot, member_there) {
        return Err(NativeSettingError::ShelfIncomplete { shelf, member });
    }

    let (sdk, sdk_extra, sdk_lib, sdk_dynamic_linker, sdk_rpaths, sdk_version) = match sysroot {
        Sysroot::MacosSdk => {
            let sdk = resolved(&String::from_str_in(sdk, alloc), |path, error| NativeSettingError::NoSdk { path, error }, alloc)?;
            let settings_path = text(&[sdk.as_str(), "/SDKSettings.json"], alloc);
            let settings = match read_bounded(settings_path.as_str(), alloc) {
                Ok(bytes) => bytes,
                Err(error) => return Err(NativeSettingError::NoSdk { path: settings_path, error }),
            };
            let Some(version) = core::str::from_utf8(&settings).ok().and_then(sdk_version) else {
                return Err(NativeSettingError::SdkVersion { path: settings_path });
            };
            (sdk, None, None, None, Vec::new_in(alloc), String::from_str_in(version, alloc))
        }
        Sysroot::LinuxHost => {
            let linux = resolve_linux_sysroot(sdk, link, alloc)?;
            (linux.sdk, linux.sdk_extra, linux.sdk_lib, linux.sdk_dynamic_linker, linux.sdk_rpaths, linux.sdk_version)
        }
        Sysroot::Pinned { .. } => return Err(NativeSettingError::PinnedSysroot { host }),
    };

    let mut sdk_rpaths_nul: AString<'a> = String::new_in(alloc);
    for (idx, rpath) in sdk_rpaths.iter().enumerate() {
        if idx > 0 {
            sdk_rpaths_nul.push('\0');
        }
        sdk_rpaths_nul.push_str(rpath.as_str());
    }

    let work = String::from_str_in(work, alloc);
    fs::create_dir_all(work.as_str()).map_err(|error| NativeSettingError::Work { path: String::from_str_in(work.as_str(), alloc), error })?;
    let work = resolved(&work, |path, error| NativeSettingError::Work { path, error }, alloc)?;

    Ok(OwnedNativeSetting {
        shelf,
        sdk,
        sdk_extra,
        sdk_lib,
        sdk_dynamic_linker,
        sdk_rpaths,
        sdk_rpaths_nul,
        sdk_version,
        sysroot,
        work,
        deadline: NATIVE_RUN_DEADLINE,
    })
}
