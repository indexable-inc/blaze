//! The blocking calls behind the effects, for [`crate::threads::Threads`]'s threads and for what
//! [`crate::uring::Uring`] does where it is submitted; and [`Mapped`], every crate's read-only
//! file mapping.

#[allow(unsafe_code, reason = "mmap and MapViewOfFile, and the byte view of what they map")]
mod mapped;

pub(crate) use mapped::map;
pub use mapped::{Advice, MapOptions, Mapped};

mod private;

pub(crate) use private::private;

/// macOS's listing of a directory with its stamps, in one call per buffer of entries.
#[cfg(target_os = "macos")]
mod bulk;

#[cfg(target_os = "macos")]
pub(crate) use bulk::entries;

/// The calls WASI makes its own way or has none of (the wasm32-wasip1 seed).
#[cfg(target_os = "wasi")]
mod wasi;

#[cfg(target_os = "wasi")]
pub(crate) use wasi::{clone, entries, seal, set_mode, set_times, stamps};

#[cfg(any(unix, target_os = "wasi"))]
use std::{fs::File, io::Write as _};
use std::{
    fs::OpenOptions,
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::UNIX_EPOCH,
};

use crate::{FileName, Meta};
#[cfg(unix)]
use crate::{Names, Stamp};

static TEMPORARY: AtomicU64 = AtomicU64::new(0);

/// A name next to `path` no other write of this process or another uses.
#[cfg(any(unix, windows))]
pub(crate) fn temporary_next_to(path: &Path) -> PathBuf {
    // alloc: once per caller that keeps the name (a listen's socket).
    let mut temporary = PathBuf::new();
    temporary_into(path, &mut temporary);
    temporary
}

/// [`temporary_next_to`] into `into`, which is emptied first: a name made in a reused buffer.
fn temporary_into(path: &Path, into: &mut PathBuf) {
    use std::fmt::Write as _;
    let n = TEMPORARY.fetch_add(1, Ordering::Relaxed);
    into.as_mut_os_string().clear();
    if let Some(parent) = path.parent() {
        into.push(parent);
    }
    let name = path.file_name().map(|it| it.to_string_lossy()).unwrap_or_default();
    blaze_mem::scratch::with_string(|file| {
        _ = write!(file, ".{name}.{}-{n}.tmp", writer());
        into.push(&*file);
    });
}

/// What tells this process's temporary names from every other's: its id.
#[cfg(not(target_os = "wasi"))]
fn writer() -> u32 {
    std::process::id()
}

/// What tells this process's temporary names from every other's: WASI has no process ids
/// (`std::process::id` panics there), so 64 random bits, drawn for each name.
#[cfg(target_os = "wasi")]
fn writer() -> u64 {
    std::random::random(..)
}

/// `f` of a NUL-terminated copy of `path`, made in the thread's byte buffer.
#[cfg(unix)]
fn with_c_path<R>(path: &Path, f: impl FnOnce(&std::ffi::CStr) -> io::Result<R>) -> io::Result<R> {
    use std::os::unix::ffi::OsStrExt as _;
    blaze_mem::scratch::with_bytes(|bytes| {
        bytes.extend_from_slice(path.as_os_str().as_bytes());
        bytes.push(0);
        let path = std::ffi::CStr::from_bytes_with_nul(bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path with a NUL"))?;
        f(path)
    })
}

/// The [`FileName`] `{dir}/{name}`, or an error naming `path` where `name` is not UTF-8 or
/// makes no name. The name is joined in the thread's string buffer, so only the `FileName` is
/// made.
fn child_name(
    dir: &FileName,
    name: &std::ffi::OsStr,
    path: impl FnOnce() -> PathBuf,
) -> io::Result<FileName> {
    use std::fmt::Write as _;
    let file = blaze_mem::scratch::with_string(|joined| {
        let name = name.to_str()?;
        _ = write!(joined, "{dir}/{name}");
        FileName::new(joined)
    });
    match file {
        Some(file) => Ok(file),
        None => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            // alloc: an error path.
            format!("{}: a name that is not UTF-8", path().display()),
        )),
    }
}

pub(crate) fn make_parent(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => std::fs::create_dir_all(parent),
        _ => Ok(()),
    }
}

/// What `make` makes at `path`, asked again once `path`'s directories are made where it
/// failed for want of one: they are looked up only when missing, not before every call.
pub(crate) fn making_parent<T>(
    path: &Path,
    mut make: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    match make() {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            make_parent(path)?;
            make()
        }
        other => other,
    }
}

/// The file at `path` cut (or grown with zeros) to `len` bytes: made, with its directories,
/// where it is missing. Opened by its name each time, never through a descriptor kept from an
/// earlier request: the name may be another file by now (a build directory removed and made
/// again by `blaze clean` and the next build).
pub(crate) fn truncate(path: &Path, len: u64) -> io::Result<()> {
    let open = || OpenOptions::new().write(true).create(true).truncate(false).open(path);
    making_parent(path, open)
        .and_then(|file| file.set_len(len))
        // alloc: an error path.
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cutting {} to {len} bytes: {error}", path.display()),
            )
        })
}

/// `bytes` as the whole of `path`: written to a temporary file next to it, synced, renamed over
/// it, and the directory synced, so the new name survives a crash.
#[cfg(any(unix, target_os = "wasi"))]
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    blaze_mem::scratch::with_path(|temporary| {
        temporary_into(path, temporary);
        let written = (|| {
            let open = || OpenOptions::new().write(true).create_new(true).open(&*temporary);
            let mut file = making_parent(path, open)?;
            file.write_all(bytes)?;
            // durable: `write_atomically` is `Effect::Write` carried out: the bytes are on disk before the rename
            file.sync_all()
        })();
        if let Err(error) = written {
            _ = std::fs::remove_file(&*temporary);
            return Err(error);
        }
        commit_rename(temporary, path)
    })
}

/// [`crate::Effect::WriteUnsynced`]: `bytes` as the whole of `path`, written to a temporary
/// file next to it and renamed over it, with no sync of the file or the directory. A reader
/// sees the old file or the new; a crash may leave either, or the new name over any part of
/// the bytes.
#[cfg(any(unix, target_os = "wasi"))]
pub(crate) fn write_unsynced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    blaze_mem::scratch::with_path(|temporary| {
        temporary_into(path, temporary);
        let written = (|| {
            let open = || OpenOptions::new().write(true).create_new(true).open(&*temporary);
            making_parent(path, open)?.write_all(bytes)
        })();
        let renamed = written.and_then(|()| std::fs::rename(&*temporary, path));
        if renamed.is_err() {
            _ = std::fs::remove_file(&*temporary);
        }
        renamed
    })
}

/// [`crate::Effect::WriteBack`]: the data of the file at `path` that is only in memory written
/// to its device, waited for here (on a driver's thread; the effect's asker waits for
/// nothing). Opened to read, so a file sealed read-only is written back too. Nothing of the
/// file is made durable: no metadata is committed and no drive cache flushed.
#[cfg(target_os = "linux")]
#[expect(unsafe_code, reason = "sync_file_range, the one call that starts a file's write-back")]
pub(crate) fn write_back(path: &Path) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;
    let file = File::open(path)?;
    let flags = libc::SYNC_FILE_RANGE_WAIT_BEFORE
        | libc::SYNC_FILE_RANGE_WRITE
        | libc::SYNC_FILE_RANGE_WAIT_AFTER;
    // SAFETY: an open descriptor, and a range of the whole file (0 bytes means to its end).
    match unsafe { libc::sync_file_range(file.as_raw_fd(), 0, 0, flags) } {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

/// [`crate::Effect::WriteBack`] on Apple's systems and the other Unixes: `fsync`, which writes
/// the file's data out and, on Apple's, leaves it in the drive's cache (`F_FULLFSYNC` is the
/// durable one, and is not asked for).
#[cfg(all(unix, not(target_os = "linux")))]
#[expect(unsafe_code, reason = "fsync, the one call that writes a file's data back")]
pub(crate) fn write_back(path: &Path) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;
    let file = File::open(path)?;
    // SAFETY: an open descriptor.
    match unsafe { libc::fsync(file.as_raw_fd()) } {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

/// [`crate::Effect::WriteBack`] where the system has no write-back short of a sync (Windows,
/// WASI): the file's data synced. Opened to write, as a flush of a handle there needs.
#[cfg(not(unix))]
pub(crate) fn write_back(path: &Path) -> io::Result<()> {
    OpenOptions::new().write(true).open(path)?.sync_data()
}

/// [`crate::Effect::RenameUnsynced`]: `from` renamed over `to`, neither directory synced;
/// `from` is left as it was when the rename fails.
#[cfg(any(unix, target_os = "wasi"))]
pub(crate) fn rename_unsynced(from: &Path, to: &Path) -> io::Result<()> {
    making_parent(to, || std::fs::rename(from, to))
}

/// `from` renamed to `to`, and `to`'s directory synced.
#[cfg(any(unix, target_os = "wasi"))]
pub(crate) fn commit_rename(from: &Path, to: &Path) -> io::Result<()> {
    if let Err(error) = std::fs::rename(from, to) {
        _ = std::fs::remove_file(from);
        return Err(error);
    }
    sync_dir_of(to)
}

/// `from` renamed over `to`, made the new name durably; `from` is left as it was when the
/// rename fails.
#[cfg(any(unix, target_os = "wasi"))]
pub(crate) fn rename(from: &Path, to: &Path) -> io::Result<()> {
    making_parent(to, || std::fs::rename(from, to))?;
    sync_dir_of(to)?;
    match (from.parent(), to.parent()) {
        (Some(was), Some(is)) if was != is => sync_dir_of(from),
        _ => Ok(()),
    }
}

/// `bytes` written at `at` of the file at `path`, which exists.
pub(crate) fn write_at(path: &Path, at: u64, bytes: &[u8]) -> io::Result<()> {
    let file = OpenOptions::new().write(true).open(path)?;
    // As `uring.rs`'s `O_DIRECT`: a large aligned write stays out of the unified buffer cache;
    // macOS keeps it coherent with cached and mapped pages.
    #[cfg(target_os = "macos")]
    if bytes.len() >= 1 << 20 && at % 4096 == 0 && bytes.len() % 4096 == 0 {
        use std::os::fd::AsFd as _;
        crate::raw::no_cache(file.as_fd())?;
    }
    #[cfg(unix)]
    return std::os::unix::fs::FileExt::write_all_at(&file, bytes, at);
    #[cfg(target_os = "wasi")]
    return std::os::wasi::fs::FileExt::write_all_at(&file, bytes, at);
    #[cfg(windows)]
    {
        let mut done = 0;
        while done < bytes.len() {
            let n =
                std::os::windows::fs::FileExt::seek_write(&file, &bytes[done..], at + done as u64)?;
            if n == 0 {
                return Err(io::Error::from(io::ErrorKind::WriteZero));
            }
            done += n;
        }
        Ok(())
    }
}

/// The file at `path`, which exists, opened for [`crate::Effect::ReadInto`] (`write` false) or
/// [`crate::Effect::WriteFrom`] at offsets: never for appending, which on Linux would put a
/// positional write at the end. With [`crate::Cache::Bypassed`], around the page cache: Linux
/// `O_DIRECT`, macOS `F_NOCACHE`, Windows `FILE_FLAG_NO_BUFFERING`; WASI has none, and fails.
pub(crate) fn open_positional(path: &Path, write: bool, cache: crate::Cache) -> io::Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.read(true).write(write);
    let bypass = cache == crate::Cache::Bypassed;
    #[cfg(target_os = "linux")]
    if bypass {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_DIRECT);
    }
    #[cfg(windows)]
    if bypass {
        use std::os::windows::fs::OpenOptionsExt as _;
        /// `FILE_FLAG_NO_BUFFERING` (`winbase.h`): reads and writes of whole sectors, around
        /// the system's cache.
        const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
        options.custom_flags(FILE_FLAG_NO_BUFFERING);
    }
    #[cfg(target_os = "wasi")]
    if bypass {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            // alloc: an error path.
            format!(
                "{}: WASI (the wasm32-wasip1 seed) has no read or write around the page cache",
                path.display()
            ),
        ));
    }
    let file = options.open(path)?;
    // macOS opens no file around its cache; `F_NOCACHE` on the descriptor keeps its reads and
    // writes out of the unified buffer cache from then on.
    #[cfg(target_os = "macos")]
    if bypass {
        use std::os::fd::AsFd as _;
        crate::raw::no_cache(file.as_fd())?;
    }
    _ = bypass;
    Ok(file)
}

/// How many times one read or write at an offset is asked again after the system cut it short
/// with `EINTR` before it fails: a bound on the loop (rule bounded-progress), far past what a
/// signal storm makes of a regular file's read.
const INTERRUPTS: u32 = 64;

/// `into` filled from `at` of `file` (opened by [`open_positional`]): how many bytes were read,
/// fewer than `into` only at the end of the file. Around the page cache, a read that ends short
/// of a whole block is at the end, and none follows it (one at an unaligned offset is refused).
pub(crate) fn read_into(
    file: &std::fs::File,
    into: &mut [u8],
    at: u64,
    cache: crate::Cache,
) -> io::Result<usize> {
    let (mut read, mut interrupted) = (0, 0);
    while read < into.len() {
        match read_at(file, &mut into[read..], at + read as u64) {
            Ok(0) => break,
            Ok(n) => {
                read += n;
                if cache == crate::Cache::Bypassed && read as u64 % crate::DIRECT_ALIGN != 0 {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted && interrupted < INTERRUPTS => {
                interrupted += 1;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(read)
}

/// All of `from` written at `at` of `file` (opened by [`open_positional`] to write).
pub(crate) fn write_from(file: &std::fs::File, from: &[u8], at: u64) -> io::Result<()> {
    let (mut done, mut interrupted) = (0, 0);
    while done < from.len() {
        match write_at_once(file, &from[done..], at + done as u64) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
            Ok(n) => done += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted && interrupted < INTERRUPTS => {
                interrupted += 1;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn read_at(file: &std::fs::File, buffer: &mut [u8], at: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buffer, at)
}

#[cfg(windows)]
fn read_at(file: &std::fs::File, buffer: &mut [u8], at: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buffer, at)
}

#[cfg(target_os = "wasi")]
fn read_at(file: &std::fs::File, buffer: &mut [u8], at: u64) -> io::Result<usize> {
    std::os::wasi::fs::FileExt::read_at(file, buffer, at)
}

#[cfg(unix)]
fn write_at_once(file: &std::fs::File, bytes: &[u8], at: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::write_at(file, bytes, at)
}

#[cfg(windows)]
fn write_at_once(file: &std::fs::File, bytes: &[u8], at: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_write(file, bytes, at)
}

#[cfg(target_os = "wasi")]
fn write_at_once(file: &std::fs::File, bytes: &[u8], at: u64) -> io::Result<usize> {
    std::os::wasi::fs::FileExt::write_at(file, bytes, at)
}

/// [`crate::Effect::Mode`]: `path`'s permission bits set to `mode`, through a symbolic link
/// (`chmod`).
#[cfg(unix)]
pub(crate) fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// [`crate::Effect::Seal`]: `path` opened `O_NOFOLLOW` (a symbolic link fails with `ELOOP`;
/// `O_NONBLOCK` keeps a FIFO from blocking the open), refused unless a regular file, and its
/// bits set on the descriptor (`fchmod`). No `fchmodat` with a flag, which glibc 2.39 and
/// later make the `fchmodat2` call that older seccomp filters refuse with `EPERM`. An error
/// names `path`.
#[cfg(unix)]
pub(crate) fn seal(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    // alloc: an error path.
    let named = |error: io::Error| {
        io::Error::new(error.kind(), format!("{}: sealing it: {error}", path.display()))
    };
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                // alloc: an error path.
                format!("{} {}", path.display(), crate::SEAL_THROUGH_A_LINK),
            ));
        }
        // A socket fails its open before any `fstat`: refused naming it, as one that opens.
        Err(error) => match error.raw_os_error().and_then(|code| no_file_after_open(path, code)) {
            Some(kind) => return Err(not_sealed(path, kind)),
            None => return Err(named(error)),
        },
    };
    let kind = file.metadata().map_err(named)?.file_type();
    if !kind.is_file() {
        return Err(not_sealed(path, kind_of(kind)));
    }
    file.set_permissions(std::fs::Permissions::from_mode(mode)).map_err(named)
}

#[cfg(unix)]
fn not_sealed(path: &Path, kind: &str) -> io::Error {
    // alloc: an error path.
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{} ({kind}) {}", path.display(), crate::SEAL_OF_NO_FILE),
    )
}

/// The access and modification times of `path` (through a symbolic link) both `modified_ns`
/// since the epoch, for [`crate::Effect::Times`]: one `utimensat`, the path made in the
/// thread's byte buffer. An error names `path`.
#[cfg(unix)]
pub(crate) fn set_times(path: &Path, modified_ns: u64) -> io::Result<()> {
    let Ok(seconds) = libc::time_t::try_from(modified_ns / 1_000_000_000) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            // alloc: an error path.
            format!(
                "{}: {modified_ns} ns since the epoch is past what this system's time_t holds",
                path.display()
            ),
        ));
    };
    let time = crate::raw::timespec(seconds, (modified_ns % 1_000_000_000) as u32);
    with_c_path(path, |c| crate::raw::utimens(c, &[time, time]))
        // alloc: an error path.
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))
}

#[cfg(windows)]
pub(crate) use crate::iocp::files::{
    clone, rename, rename_unsynced, seal, set_mode, set_times, write_atomically, write_unsynced,
};

#[cfg(any(unix, target_os = "wasi"))]
fn sync_dir_of(path: &Path) -> io::Result<()> {
    match path.parent() {
        // durable: the directory's sync of `Effect::Write` and `Effect::Rename`: the new name survives a crash
        Some(parent) if !parent.as_os_str().is_empty() => File::open(parent)?.sync_all(),
        _ => Ok(()),
    }
}

/// [`crate::Effect::Stat`], through a symbolic link, or with `follow` false
/// [`crate::Effect::Lstat`], of the link itself.
pub(crate) fn stat(path: &Path, follow: bool) -> io::Result<Option<Meta>> {
    let meta = match follow {
        true => std::fs::metadata(path),
        false => std::fs::symlink_metadata(path),
    };
    match meta {
        Ok(meta) => Ok(Some(meta_of(&meta))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn meta_of(meta: &std::fs::Metadata) -> Meta {
    Meta {
        len: meta.len(),
        dir: meta.is_dir(),
        modified_ns: meta
            .modified()
            .ok()
            .and_then(|it| it.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |it| u64::try_from(it.as_nanos()).unwrap_or(u64::MAX)),
    }
}

/// Every file and directory under `dir` (named `name` under the root), sorted by name;
/// symbolic links are not followed.
pub(crate) fn list(dir: &Path, name: &FileName) -> io::Result<Vec<(FileName, Meta)>> {
    // alloc: the listing's payload.
    let mut found: Vec<(FileName, Meta)> = Vec::new();
    // Each directory still to read, by its index in `found`; `None` for `dir` itself.
    let mut todo: Vec<Option<usize>> = Vec::with_capacity(16); // alloc: once per listing.
    todo.push(None);
    while let Some(at) = todo.pop() {
        let read = blaze_mem::scratch::with_path(|path| {
            path.push(dir);
            if let Some(at) = at {
                let file: &FileName = &found[at].0;
                path.push(&file.as_str()[name.as_str().len() + 1..]);
            }
            // alloc: once per directory, std's handle of it.
            std::fs::read_dir(&*path).map_err(|error| {
                io::Error::new(error.kind(), format!("{}: {error}", path.display()))
            })
        })?;
        for entry in read {
            let entry = entry?;
            // Of the entry itself, not followed through a symlink.
            let meta = entry.metadata()?;
            let under = at.map_or(name, |at| &found[at].0);
            // alloc: std's copy of the entry's name, and the name, the listing's payload.
            let file = child_name(under, &entry.file_name(), || entry.path())?;
            if meta.is_dir() {
                todo.push(Some(found.len()));
            }
            found.push((file, meta_of(&meta)));
        }
    }
    found.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

/// `path` gone, a file or symbolic link unlinked, an empty directory removed; one call for a
/// file, the directory looked up only once its unlink fails.
pub(crate) fn remove(path: &Path) -> io::Result<()> {
    let removed = match std::fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            match std::fs::symlink_metadata(path) {
                Ok(meta) if meta.is_dir() => std::fs::remove_dir(path),
                _ => Err(error),
            }
        }
        other => other,
    };
    match removed {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

pub(crate) fn link(from: &Path, to: &Path) -> io::Result<()> {
    making_parent(to, || std::fs::hard_link(from, to))
}

/// `to` made sharing `from`'s blocks, or an error that says the file system cannot.
#[cfg(unix)]
pub(crate) fn clone(from: &Path, to: &Path) -> io::Result<()> {
    making_parent(to, || clone_file(from, to)).map_err(|error| match error.raw_os_error() {
        Some(code) if refuses_sharing(code) => io::Error::new(
            io::ErrorKind::Unsupported,
            // alloc: an error path.
            format!(
                "the file system of {} ({}) cannot share blocks with {} ({}): {error}; blaze \
                 does not copy instead",
                to.display(),
                volume_of(to.parent().unwrap_or(to)),
                from.display(),
                volume_of(from)
            ),
        ),
        _ => error,
    })
}

/// The volume that holds `path`, for an error: where it is mounted and its file system on
/// macOS; the file system's type and the device on Linux. Two different devices are what a
/// cross-volume clone (`EXDEV`) refuses.
#[cfg(unix)]
pub(crate) fn volume_of(path: &Path) -> String {
    use std::os::unix::fs::MetadataExt as _;
    // alloc: an error path.
    let device = match std::fs::metadata(path) {
        Ok(meta) => {
            format!("device {}:{}", libc::major(meta.dev() as _), libc::minor(meta.dev() as _))
        }
        Err(error) => format!("device unknown: {error}"),
    };
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    match with_c_path(path, crate::raw::statfs) {
        Ok(stat) => format!("{}, {device}", file_system(&stat)),
        Err(error) => format!("statfs failed: {error}, {device}"),
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    device
}

#[cfg(target_os = "macos")]
fn file_system(stat: &libc::statfs) -> String {
    let text = |chars: &[libc::c_char]| {
        let bytes: Vec<u8> = chars.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect(); // alloc: an error path.
        String::from_utf8_lossy(&bytes).into_owned()
    };
    // alloc: an error path.
    format!("{} on {}", text(&stat.f_fstypename), text(&stat.f_mntonname))
}

#[cfg(target_os = "linux")]
fn file_system(stat: &libc::statfs) -> String {
    // `f_type`'s magic numbers, of `linux/magic.h`.
    let magic = stat.f_type as u32;
    let name = match magic {
        0x9123_683e => "btrfs",
        0x5846_5342 => "xfs",
        0xca45_1a4e => "bcachefs",
        0xef53 => "ext2/3/4",
        0xf2f5_2010 => "f2fs",
        0x2fc1_2fc1 => "zfs",
        0x0102_1994 => "tmpfs",
        0x794c_7630 => "overlayfs",
        0x6573_5546 => "fuse or virtiofs",
        0x0102_1997 => "9p",
        0x6969 => "nfs",
        _ => "a file system",
    };
    // alloc: an error path.
    format!("{name} (magic {magic:#x})")
}

#[cfg(target_os = "linux")]
fn refuses_sharing(code: i32) -> bool {
    matches!(code, libc::EOPNOTSUPP | libc::EXDEV | libc::EINVAL)
}

#[cfg(target_os = "macos")]
fn refuses_sharing(code: i32) -> bool {
    matches!(code, libc::ENOTSUP | libc::EXDEV)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn refuses_sharing(_: i32) -> bool {
    false
}

/// The source of a clone opened, a symbolic link refused ([`crate::CLONE_THROUGH_A_LINK`]):
/// `O_NOFOLLOW` fails a link at the last name with `ELOOP`. `O_NONBLOCK` keeps a FIFO put
/// there from blocking the open; a regular file ignores it. Anything else that is no regular
/// file (a FIFO, a socket, a device) is refused naming it, never reported as a file system
/// that cannot share blocks.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_source(from: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(from)
        .map_err(|error| match error.raw_os_error() {
            // alloc: an error path.
            Some(libc::ELOOP) => io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} {}", from.display(), crate::CLONE_THROUGH_A_LINK),
            ),
            Some(code) => match no_file_after_open(from, code) {
                // alloc: an error path.
                Some(kind) => io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} ({kind}) {}", from.display(), crate::CLONE_OF_NO_FILE),
                ),
                None => error,
            },
            None => error,
        })?;
    let kind = file
        .metadata()
        // alloc: an error path.
        .map_err(|error| {
            io::Error::new(error.kind(), format!("{}: reading what it is: {error}", from.display()))
        })?
        .file_type();
    match kind.is_file() {
        true => Ok(file),
        false => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            // alloc: an error path.
            format!("{} ({}) {}", from.display(), kind_of(kind), crate::CLONE_OF_NO_FILE),
        )),
    }
}

/// Where the open of a clone's source failed with `code`: what the name is, when that is no
/// regular file. A socket fails its open before any `fstat` (`ENXIO` on Linux, `EOPNOTSUPP` on
/// macOS), and so does a device with no driver behind it (`ENXIO`, `ENODEV`); the name is then
/// looked at without following a link, so it is refused with [`crate::CLONE_OF_NO_FILE`] and
/// written again, never reported as the open's error. `None` for any other code, or where the
/// name is gone or a regular file again.
#[cfg(unix)]
pub(crate) fn no_file_after_open(from: &Path, code: i32) -> Option<&'static str> {
    if !matches!(code, libc::ENXIO | libc::ENODEV | libc::EOPNOTSUPP) {
        return None;
    }
    let kind = std::fs::symlink_metadata(from).ok()?.file_type();
    (!kind.is_file()).then(|| kind_of(kind))
}

/// What a file is from its `st_mode` type bits (`S_IFMT`), for an error (uring's clone, which
/// keeps only those bits).
#[cfg(target_os = "linux")]
pub(crate) fn kind_of_mode(mode: u32) -> &'static str {
    match mode & libc::S_IFMT as u32 {
        it if it == libc::S_IFREG as u32 => "a regular file",
        it if it == libc::S_IFDIR as u32 => "a directory",
        it if it == libc::S_IFLNK as u32 => "a symbolic link",
        it if it == libc::S_IFIFO as u32 => "a FIFO",
        it if it == libc::S_IFSOCK as u32 => "a socket",
        it if it == libc::S_IFBLK as u32 || it == libc::S_IFCHR as u32 => "a device",
        _ => "an unknown kind of file",
    }
}

/// What a file that is no regular file is, for an error.
#[cfg(unix)]
fn kind_of(kind: std::fs::FileType) -> &'static str {
    use std::os::unix::fs::FileTypeExt as _;
    match () {
        () if kind.is_dir() => "a directory",
        () if kind.is_symlink() => "a symbolic link",
        () if kind.is_fifo() => "a FIFO",
        () if kind.is_socket() => "a socket",
        () if kind.is_block_device() || kind.is_char_device() => "a device",
        () => "an unknown kind of file",
    }
}

#[cfg(target_os = "linux")]
fn clone_file(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::fd::AsFd as _;
    let source = open_source(from)?;
    let target = OpenOptions::new().write(true).create_new(true).open(to)?;
    let Err(error) = crate::raw::ficlone(target.as_fd(), source.as_fd()) else {
        return Ok(());
    };
    drop(target);
    _ = std::fs::remove_file(to);
    Err(error)
}

/// The source is opened `O_NOFOLLOW` (a symbolic link refused, as on Linux) and cloned from
/// its descriptor (`fclonefileat`), so no link swapped in between is followed or cloned.
/// `fclonefileat` copies the source's mode, owner, flags and times onto the clone: a store's
/// placed copy is read-only (0o444) and as old as its first write, so the clone is given a new
/// file's mode (0o666 less the umask) and the time now, as Linux's `FICLONE` into a new file
/// and a `Write` leave it, each set on the name `to` without following a link there.
#[cfg(target_os = "macos")]
fn clone_file(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::fd::AsFd as _;
    let source = open_source(from)?;
    with_c_path(to, |target| crate::raw::fclonefileat(source.as_fd(), target))?;
    drop(source);
    let made = created_mode()
        .and_then(|mode| with_c_path(to, |target| crate::raw::chmod_no_follow(target, mode)))
        .and_then(|()| with_c_path(to, crate::raw::touch));
    if let Err(error) = made {
        _ = std::fs::remove_file(to);
        // alloc: an error path.
        return Err(io::Error::new(
            error.kind(),
            format!("{}: giving the clone a new file's mode and time: {error}", to.display()),
        ));
    }
    Ok(())
}

/// The mode a file this process creates with 0o666 is given: 0o666 less the umask. Read once,
/// from a file made and removed in the temporary directory, as `umask(2)` can only be read by
/// setting it, which would race every other thread's creates.
#[cfg(target_os = "macos")]
fn created_mode() -> io::Result<u32> {
    use std::os::unix::fs::PermissionsExt as _;
    static MODE: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    if let Some(&mode) = MODE.get() {
        return Ok(mode);
    }
    let probe = temporary_next_to(&std::env::temp_dir().join("blaze-umask"));
    let made = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .and_then(|file| file.metadata());
    _ = std::fs::remove_file(&probe);
    let mode = made
        .map_err(|error| {
            // alloc: an error path.
            io::Error::new(
                error.kind(),
                format!("reading the umask from {}: {error}", probe.display()),
            )
        })?
        .permissions()
        .mode()
        & 0o777;
    Ok(*MODE.get_or_init(|| mode))
}

/// The [`Stamp`] of a `stat` of a name, not followed through a symlink.
#[cfg(unix)]
pub(crate) fn stamp_of(stat: &libc::stat) -> Stamp {
    let ns = |s: i64, n: i64| (s as u64).wrapping_mul(1_000_000_000).wrapping_add(n as u64);
    Stamp {
        ino: stat.st_ino as u64,
        size: stat.st_size as u64,
        mtime_ns: ns(stat.st_mtime as i64, stat.st_mtime_nsec as i64),
        ctime_ns: ns(stat.st_ctime as i64, stat.st_ctime_nsec as i64),
        #[allow(clippy::useless_conversion, reason = "st_mode is u16 on macOS, u32 on Linux")]
        mode: u32::from(stat.st_mode),
    }
}

/// The root's directory, opened once for a batch of `fstatat`s relative to it.
#[cfg(unix)]
fn open_dir(root: &Path) -> io::Result<std::os::fd::OwnedFd> {
    with_c_path(root, crate::raw::open_dir)
        // alloc: an error path.
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", root.display())))
}

/// One name's stamp, relative to the directory `dir`.
#[cfg(unix)]
fn stamp_at(dir: std::os::fd::BorrowedFd<'_>, name: &std::ffi::CStr) -> Stamp {
    match crate::raw::stat_at(dir, name) {
        Ok(stat) => stamp_of(&stat),
        Err(error) => match error.raw_os_error() {
            Some(libc::ENOENT | libc::ENOTDIR) => Stamp::GONE,
            _ => Stamp::UNKNOWN,
        },
    }
}

/// How many names one thread of a sweep stamps.
#[cfg(unix)]
const SWEEP_CHUNK: usize = 2048;

/// How many threads stamp `names` names: one per [`SWEEP_CHUNK`] of them, at most one per core.
/// A cached `fstatat` is some 0.4 µs, so a thread spawned for 2,048 of them (some 0.8 ms) costs
/// it a few percent.
#[cfg(unix)]
fn sweepers(names: usize, cores: usize) -> usize {
    names.div_ceil(SWEEP_CHUNK).clamp(1, cores.max(1))
}

/// Every name's stamp, in order: [`crate::Effect::Stamp`] where the system has no batched
/// stat, as `fstatat`s relative to the root on [`sweepers`] threads at once. Fails only when
/// the root cannot be opened.
#[cfg(unix)]
pub(crate) fn stamps(root: &Path, names: &Names) -> io::Result<Vec<Stamp>> {
    use std::os::fd::AsFd as _;
    let dir = open_dir(root)?;
    let dir = dir.as_fd();
    // alloc: the batch's payload.
    let mut out = vec![Stamp::GONE; names.len()];
    let threads =
        sweepers(names.len(), std::thread::available_parallelism().map_or(1, |it| it.get()));
    if threads == 1 {
        for (at, stamp) in out.iter_mut().enumerate() {
            *stamp = stamp_at(dir, names.c_str(at));
        }
        return Ok(out);
    }
    let each = names.len().div_ceil(threads);
    std::thread::scope(|scope| {
        for (n, stamps) in out.chunks_mut(each).enumerate() {
            scope.spawn(move || {
                for (at, stamp) in stamps.iter_mut().enumerate() {
                    *stamp = stamp_at(dir, names.c_str(n * each + at));
                }
            });
        }
    });
    Ok(out)
}

/// What is directly in `dir` (named `name` under the root), each with its stamp, sorted by
/// name; symbolic links are stamped themselves. macOS lists with `getattrlistbulk`
/// ([`bulk::entries`]).
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn entries(dir: &Path, name: &FileName) -> io::Result<Vec<(FileName, Stamp)>> {
    use std::os::fd::AsFd as _;
    let fd = open_dir(dir)?;
    // alloc: the listing's payload.
    let mut found = Vec::new();
    // alloc: once per listing, std's handle of the directory.
    let read = std::fs::read_dir(dir)
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", dir.display())));
    for entry in read? {
        let entry = entry?;
        // alloc: std's copy of the entry's name.
        let leaf = entry.file_name();
        // alloc: the name, the listing's payload.
        let file = child_name(name, &leaf, || entry.path())?;
        let stamp = with_c_path(Path::new(&leaf), |leaf| Ok(stamp_at(fd.as_fd(), leaf)))?;
        found.push((file, stamp));
    }
    found.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn clone_file(_: &Path, _: &Path) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "no call that shares blocks on this system"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("blaze-fs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Pins C1 of review 4: a seal of a symbolic link is refused, naming the link, and the
    /// file it points to keeps its mode (the seal acts on the descriptor of the name itself,
    /// never through the link).
    #[test]
    fn a_seal_of_a_symbolic_link_is_refused_and_leaves_the_target_alone() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = dir("seal-link");
        let target = root.join("target");
        std::fs::write(&target, b"bytes").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let refused = seal(&link, 0o444).unwrap_err().to_string();
        assert!(refused.contains(crate::SEAL_THROUGH_A_LINK), "{refused}");
        assert!(refused.contains(&*link.to_string_lossy()), "{refused}");
        assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o644);
        seal(&target, 0o444).unwrap();
        assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o444);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A seal of a FIFO is refused as no regular file, without blocking on its open
    /// (`O_NONBLOCK`): a test that hangs here is that fix undone.
    #[test]
    fn a_seal_of_a_fifo_is_refused_without_blocking() {
        let root = dir("seal-fifo");
        let fifo = root.join("fifo");
        // std's `mkfifo` is unstable, and this module makes no raw calls.
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        let refused = seal(&fifo, 0o444).unwrap_err().to_string();
        assert!(
            refused.contains(crate::SEAL_OF_NO_FILE) && refused.contains("a FIFO"),
            "{refused}"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Pins C1 of review 5: a socket fails its open before any `fstat` (`ENXIO`, `EOPNOTSUPP`);
    /// a clone from it and a seal of it are still refused as no regular file, naming it, so the
    /// store writes the file again rather than failing the place with the open's error.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_socket_is_refused_as_no_regular_file_by_a_clone_and_a_seal() {
        let root = dir("socket");
        let socket = root.join("sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let refused = open_source(&socket).unwrap_err().to_string();
        assert!(
            refused.contains(crate::CLONE_OF_NO_FILE) && refused.contains("a socket"),
            "{refused}"
        );
        let refused = seal(&socket, 0o444).unwrap_err().to_string();
        assert!(
            refused.contains(crate::SEAL_OF_NO_FILE) && refused.contains("a socket"),
            "{refused}"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Pins C3 of review 4: a stat that does not follow ([`crate::Effect::Lstat`]) gives a
    /// symbolic link's own length; one that follows ([`crate::Effect::Stat`]) the target's.
    #[test]
    fn a_stat_not_followed_gives_the_link_its_own_length() {
        let root = dir("lstat");
        let target = root.join("target");
        std::fs::write(&target, vec![7u8; 100_000]).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let own = stat(&link, false).unwrap().expect("there");
        assert_eq!(own.len, target.as_os_str().len() as u64, "the link's own length");
        assert!(!own.dir);
        assert_eq!(stat(&link, true).unwrap().expect("there").len, 100_000);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A write, rename or link whose directories are missing makes them and is done; one
    /// whose directory is there is done as asked, and one that fails for another reason
    /// fails as it would have, its directories made all the same.
    #[test]
    fn missing_directories_are_made_where_a_call_needs_them() {
        let root = dir("parents");
        write_atomically(&root.join("a/b/whole"), b"whole").unwrap();
        assert_eq!(std::fs::read(root.join("a/b/whole")).unwrap(), b"whole");
        write_atomically(&root.join("a/b/whole"), b"again").unwrap();
        assert_eq!(std::fs::read(root.join("a/b/whole")).unwrap(), b"again");
        rename(&root.join("a/b/whole"), &root.join("c/d/moved")).unwrap();
        assert_eq!(std::fs::read(root.join("c/d/moved")).unwrap(), b"again");
        link(&root.join("c/d/moved"), &root.join("e/linked")).unwrap();
        assert_eq!(std::fs::read(root.join("e/linked")).unwrap(), b"again");
        let missing = rename(&root.join("nothing"), &root.join("f/to")).unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
        assert!(root.join("f").is_dir());
        let appended = root.join("g/h");
        let opened = making_parent(&appended, || {
            OpenOptions::new().create(true).append(true).open(&appended)
        });
        std::io::Write::write_all(&mut opened.unwrap(), b"line").unwrap();
        assert_eq!(std::fs::read(root.join("g/h")).unwrap(), b"line");
        let under_a_file = write_atomically(&root.join("e/linked/under"), b"x").unwrap_err();
        assert_eq!(under_a_file.raw_os_error(), Some(libc::ENOTDIR));
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A remove unlinks a file or symbolic link, removes an empty directory, is done when
    /// nothing is there, and fails on a directory that is not empty, leaving it.
    #[test]
    fn a_remove_takes_files_links_and_empty_directories() {
        let root = dir("remove");
        std::fs::write(root.join("file"), b"x").unwrap();
        std::fs::create_dir(root.join("empty")).unwrap();
        std::fs::create_dir_all(root.join("full/in")).unwrap();
        std::os::unix::fs::symlink(root.join("full"), root.join("link")).unwrap();
        for it in ["file", "empty", "link", "missing"] {
            remove(&root.join(it)).unwrap();
            assert!(std::fs::symlink_metadata(root.join(it)).is_err(), "{it}");
        }
        assert!(remove(&root.join("full")).is_err());
        assert!(root.join("full/in").is_dir());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A sweep takes a thread per 2,048 names, never more than the cores and never none; the
    /// chunks the threads take cover every name once.
    #[test]
    fn a_sweep_takes_a_thread_per_chunk_up_to_the_cores() {
        assert_eq!(sweepers(0, 16), 1);
        assert_eq!(sweepers(SWEEP_CHUNK, 16), 1);
        assert_eq!(sweepers(SWEEP_CHUNK + 1, 16), 2);
        assert_eq!(sweepers(50_000, 16), 16);
        assert_eq!(sweepers(50_000, 64), 50_000usize.div_ceil(SWEEP_CHUNK));
        assert_eq!(sweepers(50_000, 0), 1);
        for names in [1, SWEEP_CHUNK, SWEEP_CHUNK + 1, 50_000, 1 << 20] {
            for cores in [1, 3, 8, 16, 64, 256] {
                let threads = sweepers(names, cores);
                let each = names.div_ceil(threads);
                assert!(threads <= cores.max(1) && (threads - 1) * each < names, "{names} {cores}");
            }
        }
    }

    /// A sweep over more names than one thread takes gives each name its own stamp, in order:
    /// the files are there, the rest gone.
    #[test]
    fn a_sweep_on_many_threads_keeps_each_stamp_in_its_place() {
        use std::os::fd::AsFd as _;
        let root = dir("sweep");
        let count = 3 * SWEEP_CHUNK + 5;
        let mut names = Names::default();
        for at in 0..count {
            let name = format!("f{at}");
            if at % 3 == 0 {
                std::fs::write(root.join(&name), vec![b'x'; at % 7]).unwrap();
            }
            assert!(names.push_str(&name));
        }
        let swept = stamps(&root, &names).unwrap();
        assert_eq!(swept.len(), count);
        let one = open_dir(&root).unwrap();
        for (at, stamp) in swept.iter().enumerate() {
            assert_eq!(*stamp, stamp_at(one.as_fd(), names.c_str(at)), "name {at}");
            match at % 3 {
                0 => assert_eq!(stamp.size, (at % 7) as u64, "name {at}"),
                _ => assert_eq!(*stamp, Stamp::GONE, "name {at}"),
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
