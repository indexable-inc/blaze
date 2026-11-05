//! The Linux kernel blaze runs on: its release, and the floor blaze refuses to start below.
//!
//! blaze needs Linux 7.3 or later (docs/kernel.md). Both the client and `blaze serve` call
//! [`check`] before anything else: a kernel below the floor is a [`TooOld`] that names the
//! running release, the floor, the features that need it and how to get a newer kernel. Above
//! the floor every feature in [`FEATURES`] is used unconditionally, with no probe and no
//! slower path for a kernel that lacks it (`no-fallback`): a 7.3 kernel can still be built
//! without one (`CONFIG_IO_URING=n`, a seccomp profile that refuses io_uring), and that is an
//! error naming the config or the filter, never a version.

#[cfg(test)]
mod tests;

use std::fmt;

/// A kernel's `major.minor`, as the start of `uname -r` says it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// The oldest kernel blaze runs on: the one ix VMs and ix CI runners boot (7.3.y in November
/// 2026). Andrew's call, 2026-11-05 (raised from 7.2 the same day): refuse older kernels with a
/// clear message, then use the newest features freely. It is the ix kernel, not the newest
/// feature in use: docs/kernel.md plans 7.3 features (failfs and fchroot, the io_uring futex
/// fix, userfaultfd RWP) that land on it.
pub const FLOOR: Version = Version { major: 7, minor: 3 };

/// A kernel feature blaze uses, the release that added it and what blaze uses it for. docs/kernel.md
/// has the whole map, with the planned ones.
#[derive(Clone, Copy, Debug)]
pub struct Feature {
    pub name: &'static str,
    pub since: Version,
    pub used_for: &'static str,
}

const fn v(major: u32, minor: u32) -> Version {
    Version { major, minor }
}

/// The features in use that need the newest kernels, newest first: what a [`TooOld`] names.
/// None is newer than [`FLOOR`].
pub const FEATURES: &[Feature] = &[
    Feature {
        name: "openat2 OPENAT2_REGULAR",
        since: v(7, 2),
        used_for: "store, cache, journal and source reads that open only a regular file",
    },
    Feature {
        name: "file IO error reporting",
        since: v(7, 0),
        used_for: "the store's durability path: a write the file system lost is committed again",
    },
    Feature {
        name: "RWF_DONTCACHE",
        since: v(6, 14),
        used_for: "store writes the build won't read back soon (records a settle moved colder, \
                   files only cloned from), on ext4, XFS and NFS",
    },
    Feature {
        name: "mseal",
        since: v(6, 10),
        used_for: "sealing the code memory's arena a dev run's start hook lays",
    },
    Feature {
        name: "cachestat",
        since: v(6, 5),
        used_for: "the store's settle, which keeps a record warm while its pages are in the page \
                   cache and moves unread ones out of it colder first",
    },
];

/// A file system blaze writes [`crate::Cache::Drop`] writes to with `RWF_DONTCACHE`: one whose
/// driver sets `FOP_DONTCACHE`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DontCache {
    /// Its `statfs` `f_type`, of `linux/magic.h`.
    pub magic: u32,
    pub name: &'static str,
    /// The release whose driver took the flag first.
    pub since: Version,
}

/// The file systems that take `RWF_DONTCACHE`, by `f_type`: the one list of them. Data, never
/// a probe: a [`crate::Cache::Drop`] write to any file system not here is a plain write, and
/// one to a file system here that answers `EOPNOTSUPP` (a DAX mount, a driver built without
/// it) fails naming the file, the file system and its release ([`DontCache::refused`]); it is
/// never retried without the flag (`no-fallback`). Through Linux 7.3 btrfs, f2fs, bcachefs,
/// tmpfs, FUSE (virtiofs too), overlayfs and 9p set no `FOP_DONTCACHE`, as far as is known; add
/// one here with the release that adds it. `0xef53` is also ext2's and ext3's, which Linux
/// mounts with the ext4 driver.
pub const DONTCACHE: &[DontCache] = &[
    DontCache { magic: 0xef53, name: "ext4", since: v(6, 17) },
    DontCache { magic: 0x5846_5342, name: "XFS", since: v(6, 15) },
    DontCache { magic: 0x6969, name: "NFS", since: v(6, 18) },
];

/// The file system of `f_type` (`statfs`'s, as a `u32`) where a [`crate::Cache::Drop`] write
/// takes `RWF_DONTCACHE`; `None` where it is a plain write.
pub fn dontcache(f_type: u32) -> Option<DontCache> {
    DONTCACHE.iter().copied().find(|it| it.magic == f_type)
}

impl DontCache {
    /// Why a write to `path` on this file system failed with `EOPNOTSUPP` for its
    /// `RWF_DONTCACHE`.
    pub fn refused(&self, path: &dyn fmt::Display) -> String {
        let DontCache { name, since, .. } = self;
        // alloc: an error's message, made as it fails.
        format!(
            "{path} on {name} refused RWF_DONTCACHE (EOPNOTSUPP), which {name} takes from Linux \
             {since} (rule no-fallback: a write blaze asked to keep out of the page cache is \
             never written again without the flag; a DAX mount refuses it too, docs/kernel.md)"
        )
    }
}

/// The kernel is older than [`FLOOR`], or says a release blaze cannot read.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TooOld {
    /// `uname -r` said `running`, below the floor.
    Below { running: String, version: Version },
    /// `uname -r` said `running`, which does not start with `major.minor`.
    Unread { running: String },
    /// `uname` itself failed.
    NoUname { error: String },
}

impl fmt::Display for TooOld {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TooOld::Below { running, .. } => {
                write!(f, "blaze needs Linux {FLOOR} or later, and this is Linux {running}")?
            }
            TooOld::Unread { running } => write!(
                f,
                "blaze needs Linux {FLOOR} or later, and this kernel's release `{running}` \
                 does not start with a version blaze can read"
            )?,
            TooOld::NoUname { error } => write!(
                f,
                "blaze needs Linux {FLOOR} or later, and uname failed ({error}), so the \
                 running kernel is unknown"
            )?,
        }
        f.write_str(" (rule kernel-floor, docs/kernel.md). It uses, with no slower path:")?;
        for feature in FEATURES {
            write!(f, "\n  - {} (Linux {}): {}", feature.name, feature.since, feature.used_for)?;
        }
        write!(
            f,
            "\nFix: boot Linux {FLOOR} or later: your distribution's newer kernel package (a \
             mainline build, Ubuntu's HWE kernel, Fedora's or Arch's current kernel), or run \
             blaze in an ix VM (`ix new`), which boots 7.3. A container shares its host's \
             kernel, so the host needs it."
        )
    }
}

impl std::error::Error for TooOld {}

/// The `major.minor` `release` starts with (`7.3.1`, `6.8.0-1070-azure`, `2.6.60`), if it
/// does.
pub fn parse(release: &str) -> Option<Version> {
    let mut parts = release.splitn(3, '.');
    let major = parts.next()?.parse().ok()?;
    let minor_part = parts.next()?;
    let digits = minor_part.find(|it: char| !it.is_ascii_digit()).unwrap_or(minor_part.len());
    let minor = minor_part[..digits].parse().ok()?;
    Some(Version { major, minor })
}

/// Whether `release` is at or above [`FLOOR`]: its version, or why not.
pub fn admits(release: &str) -> Result<Version, TooOld> {
    match parse(release) {
        Some(version) if version >= FLOOR => Ok(version),
        // alloc: an error's message, made as it fails.
        Some(version) => Err(TooOld::Below { running: release.to_owned(), version }),
        // alloc: an error's message, made as it fails.
        None => Err(TooOld::Unread { running: release.to_owned() }),
    }
}

/// The running kernel at or above [`FLOOR`]: its version, or a [`TooOld`] to stop with. Run
/// by the client and by `blaze serve` before they do anything else.
#[cfg(target_os = "linux")]
pub fn check() -> Result<Version, TooOld> {
    // alloc: an error's message, made as it fails.
    let running = running().map_err(|error| TooOld::NoUname { error: error.to_string() })?;
    admits(&running)
}

/// The running kernel's release, as `uname -r` says it.
#[cfg(target_os = "linux")]
pub fn running() -> std::io::Result<String> {
    let name = crate::raw::uname()?;
    let release: Vec<u8> =
        // alloc: once per process, or for an error's message.
        name.release.iter().take_while(|&&it| it != 0).map(|&it| it as u8).collect();
    // alloc: once per process, or for an error's message.
    Ok(String::from_utf8_lossy(&release).into_owned())
}

/// The running kernel's release for an error's message: `uname -r`'s, or "of an unknown
/// release".
#[cfg(target_os = "linux")]
pub(crate) fn release() -> String {
    // alloc: only for an error's message.
    running().unwrap_or_else(|_| "of an unknown release".into())
}
