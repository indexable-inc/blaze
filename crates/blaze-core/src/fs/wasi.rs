//! The file calls of [`super`] that WASI (wasm32-wasip1, the seed's target:
//! docs/future/bootstrap.md) makes its own way, or has none of.
//!
//! Stamps are made from std's `stat`, one name after another: there are no threads to sweep
//! with. WASI files have no owner and no permission bits, and WASI has no call that shares
//! blocks, so a mode, a seal and a clone stop with an error naming the call, the file and WASI,
//! never doing something else in their place; so does a check that a directory is the user's
//! alone ([`super::private`]).

use std::{
    fs::{FileTimes, Metadata},
    io,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{FileName, Names, S_IFDIR, S_IFLNK, S_IFREG, Stamp};

/// The error of `call`, which WASI cannot make of `path`, saying `why`.
pub(crate) fn refused(path: &Path, call: std::fmt::Arguments<'_>, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        // alloc: an error path.
        format!("{}: {call} is refused on WASI (the wasm32-wasip1 seed): {why}", path.display()),
    )
}

/// Why no mode can be set or kept.
const NO_MODES: &str = "WASI files have no permission bits";

/// [`crate::Effect::Mode`]: refused, as WASI has no permission bits to set.
pub(crate) fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    Err(refused(path, format_args!("setting its mode to {mode:#o}"), NO_MODES))
}

/// [`crate::Effect::Seal`]: refused, as WASI has no permission bits to make a file read-only
/// with.
pub(crate) fn seal(path: &Path, mode: u32) -> io::Result<()> {
    Err(refused(path, format_args!("sealing it (mode {mode:#o})"), NO_MODES))
}

/// [`crate::Effect::Clone`]: refused, as WASI has no call that shares blocks; nothing copies
/// instead.
pub(crate) fn clone(from: &Path, to: &Path) -> io::Result<()> {
    Err(refused(
        to,
        format_args!("cloning {} to it", from.display()),
        "WASI has no call that shares blocks, and blaze does not copy instead",
    ))
}

/// [`crate::Effect::Times`]: the access and modification times of `path`, through a symbolic
/// link, both `modified_ns` since the epoch (WASI's `path_filestat_set_times`, whose times are
/// 64-bit nanoseconds, so every `modified_ns` fits). An error names `path`.
pub(crate) fn set_times(path: &Path, modified_ns: u64) -> io::Result<()> {
    let time = UNIX_EPOCH + std::time::Duration::from_nanos(modified_ns);
    std::fs::set_times(path, FileTimes::new().set_accessed(time).set_modified(time))
        // alloc: an error path.
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))
}

/// Nanoseconds since the epoch, wrapping for a time before it: distinct times stay distinct,
/// which is all a stamp needs. `None` for a time the system could not say.
fn ns(time: io::Result<SystemTime>) -> Option<u64> {
    Some(match time.ok()?.duration_since(UNIX_EPOCH) {
        Ok(after) => after.as_nanos() as u64,
        Err(before) => (before.duration().as_nanos() as u64).wrapping_neg(),
    })
}

/// `st_mode` as a system with none is made to say it ([`Stamp`]): a link, a directory, a file
/// its owner may write. `None` for anything else (a device, a socket), which WASI names but
/// gives no mode of.
fn mode(kind: std::fs::FileType) -> Option<u32> {
    match () {
        () if kind.is_symlink() => Some(S_IFLNK | 0o777),
        () if kind.is_dir() => Some(S_IFDIR | 0o755),
        () if kind.is_file() => Some(S_IFREG | 0o644),
        () => None,
    }
}

/// The [`Stamp`] of what `meta` says of a name, not followed through a symlink; unknown where
/// its kind or a time can't be said. The change time is std's `created`, which on WASI is
/// `filestat`'s `ctim`, the inode's change time.
fn stamp_of(meta: &Metadata) -> Stamp {
    use std::os::wasi::fs::MetadataExt as _;
    let (Some(mode), Some(mtime_ns), Some(ctime_ns)) =
        (mode(meta.file_type()), ns(meta.modified()), ns(meta.created()))
    else {
        return Stamp::UNKNOWN;
    };
    Stamp { ino: meta.ino(), size: meta.len(), mtime_ns, ctime_ns, mode }
}

/// The stamp of what `got` says of one name: gone where there is nothing there, unknown where
/// the system would not say.
fn stamp_or_gone(got: io::Result<Metadata>) -> Stamp {
    match got {
        Ok(meta) => stamp_of(&meta),
        Err(error)
            if matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) =>
        {
            Stamp::GONE
        }
        Err(_) => Stamp::UNKNOWN,
    }
}

/// [`crate::Effect::Stamp`]: every name's stamp under `root`, in order, one `stat` after
/// another. Fails only when `root` is not a directory that can be read.
pub(crate) fn stamps(root: &Path, names: &Names) -> io::Result<Vec<Stamp>> {
    // alloc: an error path.
    let named =
        |error: io::Error| io::Error::new(error.kind(), format!("{}: {error}", root.display()));
    if !std::fs::metadata(root).map_err(named)?.is_dir() {
        return Err(named(io::Error::from(io::ErrorKind::NotADirectory)));
    }
    // alloc: the batch's payload.
    let mut out = vec![Stamp::GONE; names.len()];
    for (at, stamp) in out.iter_mut().enumerate() {
        *stamp = blaze_mem::scratch::with_path(|path| {
            path.push(root);
            path.push(names.get(at));
            stamp_or_gone(std::fs::symlink_metadata(&*path))
        });
    }
    Ok(out)
}

/// [`crate::Effect::Entries`]: what is directly in `dir` (named `name` under the root), each
/// with its stamp, sorted by name; symbolic links are stamped themselves.
pub(crate) fn entries(dir: &Path, name: &FileName) -> io::Result<Vec<(FileName, Stamp)>> {
    // alloc: the listing's payload.
    let mut found = Vec::new();
    // alloc: once per listing, std's handle of the directory.
    let read = std::fs::read_dir(dir)
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", dir.display())));
    for entry in read? {
        let entry = entry?;
        // alloc: std's copy of the entry's name, and the name, the listing's payload.
        let file = super::child_name(name, &entry.file_name(), || entry.path())?;
        // Of the entry itself, not followed through a symlink.
        found.push((file, stamp_or_gone(entry.metadata())));
    }
    found.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}
