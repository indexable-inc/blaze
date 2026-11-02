//! The blocking calls behind the effects, for [`crate::threads::Threads`]'s threads and for what
//! [`crate::uring::Uring`] does where it is submitted; and [`Mapped`], every crate's read-only
//! file mapping.

#[allow(unsafe_code, reason = "mmap and MapViewOfFile, and the byte view of what they map")]
mod mapped;

pub(crate) use mapped::map;
pub use mapped::{Advice, MapOptions, Mapped};

mod private;

pub(crate) use private::private;

#[cfg(unix)]
use std::{fs::File, io::Write as _};
use std::{
    fs::OpenOptions,
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::UNIX_EPOCH,
};

#[cfg(unix)]
use crate::{Names, Stamp};
use crate::{FileName, Meta};

static TEMPORARY: AtomicU64 = AtomicU64::new(0);

/// A name next to `path` no other write of this process or another uses.
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
        _ = write!(file, ".{name}.{}-{n}.tmp", std::process::id());
        into.push(&*file);
    });
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
fn child_name(dir: &FileName, name: &std::ffi::OsStr, path: impl FnOnce() -> PathBuf) -> io::Result<FileName> {
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
pub(crate) fn making_parent<T>(path: &Path, mut make: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    match make() {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            make_parent(path)?;
            make()
        }
        other => other,
    }
}

/// `bytes` as the whole of `path`: written to a temporary file next to it, synced, renamed over
/// it, and the directory synced, so the new name survives a crash.
#[cfg(unix)]
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    blaze_mem::scratch::with_path(|temporary| {
        temporary_into(path, temporary);
        let written = (|| {
            let open = || OpenOptions::new().write(true).create_new(true).open(&*temporary);
            let mut file = making_parent(path, open)?;
            file.write_all(bytes)?;
            file.sync_all()
        })();
        if let Err(error) = written {
            _ = std::fs::remove_file(&*temporary);
            return Err(error);
        }
        commit_rename(temporary, path)
    })
}

/// `from` renamed to `to`, and `to`'s directory synced.
#[cfg(unix)]
pub(crate) fn commit_rename(from: &Path, to: &Path) -> io::Result<()> {
    if let Err(error) = std::fs::rename(from, to) {
        _ = std::fs::remove_file(from);
        return Err(error);
    }
    sync_dir_of(to)
}

/// `from` renamed over `to`, made the new name durably; `from` is left as it was when the
/// rename fails.
#[cfg(unix)]
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
    #[cfg(windows)]
    {
        let mut done = 0;
        while done < bytes.len() {
            let n = std::os::windows::fs::FileExt::seek_write(
                &file,
                &bytes[done..],
                at + done as u64,
            )?;
            if n == 0 {
                return Err(io::Error::from(io::ErrorKind::WriteZero));
            }
            done += n;
        }
        Ok(())
    }
}

#[cfg(unix)]
pub(crate) fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
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
pub(crate) use crate::iocp::files::{clone, rename, set_mode, set_times, write_atomically};

#[cfg(unix)]
fn sync_dir_of(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => File::open(parent)?.sync_all(),
        _ => Ok(()),
    }
}

pub(crate) fn stat(path: &Path) -> io::Result<Option<Meta>> {
    match std::fs::metadata(path) {
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
            std::fs::read_dir(&*path).map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))
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
        Err(error) if error.kind() != io::ErrorKind::NotFound => match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.is_dir() => std::fs::remove_dir(path),
            _ => Err(error),
        },
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
                "the file system of {} cannot share blocks with {} ({error}); blaze does not \
                 copy instead",
                to.display(),
                from.display()
            ),
        ),
        _ => error,
    })
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

#[cfg(target_os = "linux")]
fn clone_file(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::fd::AsFd as _;
    let source = File::open(from)?;
    let target = OpenOptions::new().write(true).create_new(true).open(to)?;
    let Err(error) = crate::raw::ficlone(target.as_fd(), source.as_fd()) else {
        return Ok(());
    };
    drop(target);
    _ = std::fs::remove_file(to);
    Err(error)
}

#[cfg(target_os = "macos")]
fn clone_file(from: &Path, to: &Path) -> io::Result<()> {
    with_c_path(from, |source| with_c_path(to, |target| crate::raw::clonefile(source, target)))
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
    let threads = sweepers(names.len(), std::thread::available_parallelism().map_or(1, |it| it.get()));
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
/// name; symbolic links are stamped themselves.
#[cfg(unix)]
pub(crate) fn entries(dir: &Path, name: &FileName) -> io::Result<Vec<(FileName, Stamp)>> {
    use std::os::fd::AsFd as _;
    let fd = open_dir(dir)?;
    // alloc: the listing's payload.
    let mut found = Vec::new();
    // alloc: once per listing, std's handle of the directory.
    let read = std::fs::read_dir(dir).map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", dir.display())));
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
        let opened = making_parent(&appended, || OpenOptions::new().create(true).append(true).open(&appended));
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
