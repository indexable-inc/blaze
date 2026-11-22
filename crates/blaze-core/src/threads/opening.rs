//! How the kqueue driver's file threads open the file of an effect a kept descriptor can serve
//! (thoughts.md Pillar 3, item 3a): by its name for each effect, or through the driver's table
//! of kept descriptors (crate::descriptors), which reads into memory use already, so a file a
//! command maps, reads and writes back is opened once.
//!
//! The choice is the type parameter [`Opening`] (rule 78): [`Kept`] (the default) and
//! [`ByName`] (each of these effects opens its file by name, the driver before 2026-11-21,
//! kept as the tested control). Either way an effect that changes what a name holds closes its
//! kept descriptors after it (`crate::threads::run`), so a kept descriptor reads what the name
//! held after this driver's last change of it.
//!
//! The effects it covers, each through the descriptor its contract needs:
//! - [`Effect::Map`]: `mmap` of the read-only descriptor; the mapping holds the file after.
//! - [`Effect::WriteBack`]: `fsync` of it (macOS's leaves the drive's cache as it is; it needs
//!   no write access).
//! - [`Effect::Read`] where it is queued (inline, it is the asker's: crate::threads::inline).
//! - [`Effect::WriteAt`], [`Effect::WriteFrom`], [`Effect::Truncate`]: through the kept
//!   writable descriptor (`Descriptors::writable`), so a file written in pieces and cut is
//!   opened once. `WriteAt` goes through the page cache whatever its size, as its contract says
//!   (the by-name route's `F_NOCACHE` for a large aligned write is that route's own);
//!   `WriteFrom` through the cache it names. A cut in place keeps the inode every kept
//!   descriptor of the name reads, so it lets none go (as `crate::threads::changes` says of a
//!   write in place).
//! - [`Effect::Barrier`] and [`Effect::Sync`] of a file already there: through the read-only
//!   descriptor, as `crate::fs::to_sync` opens it, since a sealed file (mode `0o444`) refuses a
//!   writable open its sync must not need.
//!
//! The rule for a file that is missing: `Truncate`, `Barrier` and `Sync` make it, which the
//! table (it opens what is there and makes nothing) cannot. Where its open says the file is not
//! there, the effect goes by its name (`by_name`), the route that makes it with its
//! directories. That is the contract's other case, as `crate::fs::to_sync` makes the file where
//! its own open finds none, never a second try after a failure: every other error of the kept
//! open fails the effect, naming the file.
//!
//! Not covered: [`Effect::Append`]. A positional descriptor has no end to write at but the
//! length its `fstat` gives, and two appends in flight on two threads would both write there,
//! one losing its bytes; `O_APPEND` puts each at the end as the system writes it. A queued
//! append is rare besides (the asker's thread makes it inline: crate::threads::inline). A
//! kept descriptor sees a file another process renamed over the name since it was opened as the
//! file it opened, as a read into memory does already: what this driver changes it forgets;
//! what another process changes under a kept name is the store's to refuse (its files are
//! named by content and never rewritten).
//!
//! The measure (rule 16): the opens by name of the effects this covers ([`OpenCount`]), beside
//! the table's own opens and closes (crate::Opens).

use std::{
    io,
    os::fd::AsRawFd as _,
    path::Path,
    sync::atomic::{AtomicU64, Ordering::Relaxed},
};

use blaze_sys::{Fd, posix::io as sys_io};

use crate::{Cache, Done, Effect, FileName, descriptors::Descriptors};

/// The seam: how a file thread opens the file of an effect a kept descriptor can serve.
pub trait Opening: Send + Sync + 'static {
    /// Whether through the table of kept descriptors.
    const KEPT: bool;
}

/// By its name, for each effect: the control.
#[derive(Clone, Copy, Debug)]
pub struct ByName;

impl Opening for ByName {
    const KEPT: bool = false;
}

/// Through the driver's kept descriptors: a file opened at its first such effect or read.
#[derive(Clone, Copy, Debug)]
pub struct Kept;

impl Opening for Kept {
    const KEPT: bool = true;
}

/// The opens by name of the effects [`Kept`] covers, made since the driver started: one each
/// under [`ByName`]; under [`Kept`] only those that made a missing file (the module's rule).
#[derive(Debug, Default)]
pub struct OpenCount {
    by_name: AtomicU64,
}

impl OpenCount {
    pub fn by_name(&self) -> u64 {
        self.by_name.load(Relaxed)
    }
}

/// Whether [`Kept`] covers `effect`.
fn covered(effect: &Effect) -> bool {
    matches!(
        effect,
        Effect::Map { .. }
            | Effect::WriteBack { .. }
            | Effect::Read { .. }
            | Effect::WriteAt { .. }
            | Effect::WriteFrom { .. }
            | Effect::Truncate { .. }
            | Effect::Barrier { .. }
            | Effect::Sync { .. }
    )
}

/// `effect` run on a file thread as `O` opens its file; every other effect as
/// `crate::threads::run` runs it.
pub(crate) fn run<O: Opening>(root: &Path, kept: &Descriptors, count: &OpenCount, effect: Effect) -> io::Result<Done> {
    if !covered(&effect) {
        return crate::threads::run(root, kept, effect);
    }
    if !O::KEPT {
        return by_name(root, kept, count, effect);
    }
    match effect {
        Effect::Map { file, extent, .. } => {
            let opened = kept.file(root, &file, Cache::Kept)?;
            // SAFETY: `Effect::Map`'s contract, as crate::fs::map_extent's: while any `Mapping`
            // of the file lives, the bytes below the length it had when mapped are never written
            // again and the file is never truncated; the store and the image replace a file only
            // by renaming another over it. The kept descriptor is of the file the name held at
            // this driver's last change of it (the module doc); the mapping keeps that file.
            let mapped = unsafe {
                match extent {
                    crate::Extent::Whole => crate::fs::MapOptions::new().map(&opened),
                    crate::Extent::Range { at, len } => crate::fs::MapOptions::new().map_range(&opened, at, len),
                }
            }
            .map_err(|error| named(&file, &error))?;
            Ok(Done::Mapped(crate::Mapping::file(mapped)))
        }
        Effect::WriteBack { file, .. } => {
            let opened = kept.file(root, &file, Cache::Kept)?;
            sys_io::fsync(Fd::new(opened.as_raw_fd()))
                .map_err(|errno| named(&file, &io::Error::from_raw_os_error(i32::from(errno.raw()))))?;
            Ok(Done::WrittenBack)
        }
        Effect::Read { file, at, len, .. } => {
            let opened = kept.file(root, &file, Cache::Kept)?;
            // alloc: the read's payload, as crate::fs::read's.
            let mut bytes = vec![0; len];
            let read = crate::fs::read_into(&opened, &mut bytes, at, Cache::Kept).map_err(|error| named(&file, &error))?;
            bytes.truncate(read);
            Ok(Done::Read(bytes))
        }
        Effect::WriteAt { file, at, bytes, .. } => {
            // The page cache, whatever the write's size: `Effect::WriteAt`'s contract.
            let opened = kept.writable(root, &file, Cache::Kept)?;
            crate::fs::write_from(&opened, &bytes, at).map_err(|error| named(&file, &error))?;
            Ok(Done::Written)
        }
        Effect::WriteFrom { file, at, from, cache, .. } => {
            let span = crate::spans::resolve(&from, &file, at, cache, "a write from memory to")
                // alloc: an error path.
                .map_err(|why| io::Error::other(why.0))?;
            let opened = kept.writable(root, &file, cache)?;
            span.with(|bytes| crate::fs::write_from(&opened, bytes, at)).map_err(|error| named(&file, &error))?;
            Ok(Done::Written)
        }
        Effect::Truncate { file, len, tag } => match kept.writable(root, &file, Cache::Kept) {
            // Cut in place: no kept descriptor of the name is let go (the module doc).
            Ok(opened) => {
                opened.set_len(len).map_err(|error| named(&file, &error))?;
                Ok(Done::Truncated)
            }
            // Missing: made by its name (the module's rule).
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                by_name(root, kept, count, Effect::Truncate { file, len, tag })
            }
            Err(error) => Err(error),
        },
        Effect::Barrier { file, tag } => match kept.file(root, &file, Cache::Kept) {
            Ok(opened) => {
                sys_io::barrier_fsync(Fd::new(opened.as_raw_fd()))
                    .map_err(|errno| named(&file, &io::Error::from_raw_os_error(i32::from(errno.raw()))))?;
                Ok(Done::Ordered)
            }
            // Missing: made by its name (the module's rule).
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                by_name(root, kept, count, Effect::Barrier { file, tag })
            }
            Err(error) => Err(error),
        },
        Effect::Sync { file, tag } => match kept.file(root, &file, Cache::Kept) {
            Ok(opened) => {
                // durable: the effect is the sync its caller asked for (`F_FULLFSYNC` here).
                opened.sync_all().map_err(|error| named(&file, &error))?;
                Ok(Done::Synced)
            }
            // Missing: made by its name (the module's rule).
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                by_name(root, kept, count, Effect::Sync { file, tag })
            }
            Err(error) => Err(error),
        },
        other => crate::threads::run(root, kept, other),
    }
}

/// A covered effect run by its file's name, counted: every one under [`ByName`]; under
/// [`Kept`], a `Truncate`, `Barrier` or `Sync` whose file the kept open found missing, which
/// this route makes (the module's rule).
fn by_name(root: &Path, kept: &Descriptors, count: &OpenCount, effect: Effect) -> io::Result<Done> {
    count.by_name.fetch_add(1, Relaxed);
    crate::threads::run(root, kept, effect)
}

/// `error` of `file`, saying which.
fn named(file: &FileName, error: &io::Error) -> io::Error {
    // alloc: an error path.
    io::Error::new(error.kind(), format!("{file}: {error}"))
}
