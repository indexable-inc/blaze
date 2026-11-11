//! What each WASI call does for the seed: the real file system, stdio and clocks of the machine
//! it bootstraps on. The seed is blaze itself, not user code: it reads and writes what native
//! blaze would. Build scripts and proc macros it runs are wasm modules inside it, sandboxed
//! there (`sandbox-everything`), never through this.
//!
//! - Descriptors 0, 1 and 2 are the runner's stdin, stdout and stderr; `blaze_wasi::ROOT_FD`
//!   is the one preopened directory `/`, the host's root, as for a macro; the files and
//!   directories the seed opens take the lowest free descriptor after it.
//! - The seed's own calls (`blaze_wasi::seed`, module `blaze_seed`) write the file system at
//!   absolute paths (directories made and removed, files renamed, linked and unlinked, sizes and
//!   times set, positioned reads and writes) and decommit its memory. The macro sandbox is
//!   never served them.
//! - A path is relative to the directory it is opened from; one that starts with `/` is refused
//!   (`ENOTCAPABLE`), as WASI paths never do (wasi-libc strips the root before the call).
//! - A directory lists in name order, so what the seed reads does not depend on the file
//!   system's order (`deterministic-output`). `.` and `..` are not listed.
//! - The realtime clock is the host's; the monotonic one counts from the runner's start; the
//!   CPU clocks are the process's and the thread's.
//! - A wait (`poll_oneoff`) stops the seed, naming it: the seed builds in one pass and never
//!   sleeps. `proc_exit` ends it with its code.

use std::{
    ffi::OsStr,
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{FileExt, FileTypeExt, MetadataExt, OpenOptionsExt},
        io::AsRawFd,
    },
    path::{Path, PathBuf},
    time::{Instant, SystemTime},
};

use blaze_wasi::{
    Answer, Dirent, Errno, Filestat, Filetype, Open, ROOT_FD, Stop, System,
    seed::{Seed, Time},
};

/// What a descriptor is.
enum Opened {
    Stdin,
    Stdout,
    Stderr,
    File(File),
    /// A directory: where it is, and its entries once listed.
    Dir {
        path: PathBuf,
        entries: Option<Vec<Entry>>,
    },
}

/// One entry of a listed directory.
struct Entry {
    name: Vec<u8>,
    ino: u64,
    filetype: Filetype,
}

/// The seed's machine.
pub(crate) struct Host {
    /// The arguments and the environment, each string NUL-terminated, and how many.
    args: (Vec<u8>, usize),
    env: (Vec<u8>, usize),
    /// Each descriptor's, by number; `None` is free.
    fds: Vec<Option<Opened>>,
    started: Instant,
    /// The code the seed exited with, once it called `proc_exit`.
    pub(crate) exit: Option<u32>,
}

/// `strings`, each NUL-terminated, back to back, and how many; one with a NUL inside is refused.
fn joined<'s>(
    strings: impl Iterator<Item = (&'s OsStr, Option<&'s OsStr>)>,
) -> Result<(Vec<u8>, usize), String> {
    // alloc: once per run
    let mut bytes = Vec::new();
    let mut n = 0;
    for (name, value) in strings {
        for part in [Some(name), value].into_iter().flatten() {
            if part.as_bytes().contains(&0) {
                // alloc: error path
                return Err(format!("{part:?} holds a NUL byte, which ends a C string early"));
            }
        }
        bytes.extend_from_slice(name.as_bytes());
        if let Some(value) = value {
            bytes.push(b'=');
            bytes.extend_from_slice(value.as_bytes());
        }
        bytes.push(0);
        n += 1;
    }
    Ok((bytes, n))
}

impl Host {
    pub(crate) fn new(
        args: &[std::ffi::OsString],
        env: &[(std::ffi::OsString, std::ffi::OsString)],
    ) -> Result<Host, String> {
        let args = joined(args.iter().map(|arg| (arg.as_os_str(), None)))?;
        let env =
            joined(env.iter().map(|(name, value)| (name.as_os_str(), Some(value.as_os_str()))))?;
        // alloc: once per run
        let fds = vec![
            Some(Opened::Stdin),
            Some(Opened::Stdout),
            Some(Opened::Stderr),
            Some(Opened::Dir { path: PathBuf::from("/"), entries: None }),
        ];
        Ok(Host { args, env, fds, started: Instant::now(), exit: None })
    }

    fn get(&mut self, fd: u32) -> Result<&mut Opened, Errno> {
        self.fds.get_mut(fd as usize).and_then(Option::as_mut).ok_or(Errno::BADF)
    }

    /// The host path of `path` from the directory `at`.
    fn resolve(&mut self, at: u32, path: &[u8]) -> Result<PathBuf, Errno> {
        let base = match self.get(at)? {
            Opened::Dir { path, .. } => path,
            _ => return Err(Errno::NOTDIR),
        };
        if path.first() == Some(&b'/') {
            return Err(Errno::NOTCAPABLE);
        }
        Ok(base.join(OsStr::from_bytes(path))) // alloc: the host path of each path call
    }

    /// `opened` at the lowest free descriptor.
    fn insert(&mut self, opened: Opened) -> Result<u32, Stop> {
        let fd = match self.fds.iter().position(Option::is_none) {
            Some(free) => {
                self.fds[free] = Some(opened);
                free
            }
            None => {
                self.fds.push(Some(opened));
                self.fds.len() - 1
            }
        };
        // alloc: error path
        u32::try_from(fd).map_err(|_| Stop(format!("opened descriptor {fd}, past WASI's")))
    }
}

/// The WASI `errno` of a host error.
fn errno(error: &io::Error) -> Errno {
    match error.raw_os_error() {
        Some(libc::EACCES) => Errno::ACCES,
        Some(libc::EBADF) => Errno::BADF,
        Some(libc::EEXIST) => Errno::EXIST,
        Some(libc::EFAULT) => Errno::FAULT,
        Some(libc::EFBIG) => Errno::FBIG,
        Some(libc::EINTR) => Errno::INTR,
        Some(libc::EINVAL) => Errno::INVAL,
        Some(libc::EISDIR) => Errno::ISDIR,
        Some(libc::ELOOP) => Errno::LOOP,
        Some(libc::EMFILE) => Errno::MFILE,
        Some(libc::ENAMETOOLONG) => Errno::NAMETOOLONG,
        Some(libc::ENOENT) => Errno::NOENT,
        Some(libc::ENOMEM) => Errno::NOMEM,
        Some(libc::ENOSPC) => Errno::NOSPC,
        Some(libc::ENOTDIR) => Errno::NOTDIR,
        Some(libc::ENOTEMPTY) => Errno::NOTEMPTY,
        Some(libc::EOVERFLOW) => Errno::OVERFLOW,
        Some(libc::EPERM) => Errno::PERM,
        Some(libc::EPIPE) => Errno::PIPE,
        Some(libc::EROFS) => Errno::ROFS,
        Some(libc::ESPIPE) => Errno::SPIPE,
        Some(libc::EXDEV) => Errno::XDEV,
        _ => Errno::IO,
    }
}

fn filetype(meta: &Metadata) -> Filetype {
    let ty = meta.file_type();
    if ty.is_dir() {
        Filetype::Directory
    } else if ty.is_file() {
        Filetype::RegularFile
    } else if ty.is_symlink() {
        Filetype::SymbolicLink
    } else if ty.is_block_device() {
        Filetype::BlockDevice
    } else if ty.is_char_device() {
        Filetype::CharacterDevice
    } else {
        Filetype::Unknown
    }
}

/// A time in nanoseconds since the epoch, from its seconds and nanoseconds (0 before it).
fn ns(secs: i64, nsecs: i64) -> u64 {
    u64::try_from(secs)
        .unwrap_or(0)
        .saturating_mul(1_000_000_000)
        .saturating_add(u64::try_from(nsecs).unwrap_or(0))
}

fn filestat(meta: &Metadata) -> Filestat {
    Filestat {
        dev: meta.dev(),
        ino: meta.ino(),
        filetype: filetype(meta),
        nlink: meta.nlink(),
        size: meta.size(),
        atim: ns(meta.atime(), meta.atime_nsec()),
        mtim: ns(meta.mtime(), meta.mtime_nsec()),
        ctim: ns(meta.ctime(), meta.ctime_nsec()),
    }
}

/// `path`'s metadata, its last link followed or not.
fn metadata(path: &Path, follow: bool) -> io::Result<Metadata> {
    if follow { fs::metadata(path) } else { fs::symlink_metadata(path) }
}

/// The entries of the directory at `path`, sorted by name.
fn list(path: &Path) -> io::Result<Vec<Entry>> {
    // alloc: once per directory the seed lists
    let mut entries = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        entries.push(Entry {
            name: entry.file_name().as_bytes().to_vec(),
            ino: meta.ino(),
            filetype: filetype(&meta),
        }); // alloc: as above
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

/// A host CPU clock, in nanoseconds.
fn cpu_clock(id: libc::clockid_t) -> Result<u64, Errno> {
    let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `now` is a live `timespec` to write.
    if unsafe { libc::clock_gettime(id, &mut now) } != 0 {
        return Err(Errno::INVAL);
    }
    Ok(ns(now.tv_sec, now.tv_nsec))
}

impl System for Host {
    fn args(&mut self) -> Result<(&[u8], usize), Stop> {
        Ok((&self.args.0, self.args.1))
    }

    fn environ(&mut self) -> Result<(&[u8], usize), Stop> {
        Ok((&self.env.0, self.env.1))
    }

    fn clock_res(&mut self, id: u32) -> Answer<u64> {
        Ok(if id > 3 { Err(Errno::INVAL) } else { Ok(1) })
    }

    fn clock_time(&mut self, id: u32, _fuel_left: Option<u64>) -> Answer<u64> {
        Ok(match id {
            // The seed asked for the time of day: the host's clock is the answer.
            0 => Ok(SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |since| since.as_nanos() as u64)),
            1 => Ok(self.started.elapsed().as_nanos() as u64),
            2 => cpu_clock(libc::CLOCK_PROCESS_CPUTIME_ID),
            3 => cpu_clock(libc::CLOCK_THREAD_CPUTIME_ID),
            _ => Err(Errno::INVAL),
        })
    }

    fn random(&mut self, into: &mut [u8]) -> Result<(), Stop> {
        // `getentropy` gives at most 256 bytes a call.
        for chunk in into.chunks_mut(256) {
            // SAFETY: `chunk` is a live buffer of `chunk.len()` bytes, at most 256.
            if unsafe { libc::getentropy(chunk.as_mut_ptr().cast(), chunk.len()) } != 0 {
                // alloc: error path
                return Err(Stop(format!(
                    "asked for random bytes, and the host had none (getentropy): {}",
                    io::Error::last_os_error()
                )));
            }
        }
        Ok(())
    }

    fn write(&mut self, fd: u32, bytes: &[u8]) -> Answer<usize> {
        let wrote = match self.get(fd) {
            // Flushed at once: the seed's output reaches the terminal as it writes it.
            Ok(Opened::Stdout) => {
                let mut out = io::stdout().lock();
                out.write(bytes).and_then(|wrote| out.flush().map(|()| wrote))
            }
            Ok(Opened::Stderr) => io::stderr().write(bytes),
            Ok(Opened::File(file)) => file.write(bytes),
            Ok(Opened::Dir { .. }) => return Ok(Err(Errno::ISDIR)),
            Ok(Opened::Stdin) => return Ok(Err(Errno::BADF)),
            Err(errno) => return Ok(Err(errno)),
        };
        Ok(wrote.map_err(|e| errno(&e)))
    }

    fn read(&mut self, fd: u32, into: &mut [u8]) -> Answer<usize> {
        let got = match self.get(fd) {
            Ok(Opened::Stdin) => io::stdin().read(into),
            Ok(Opened::File(file)) => file.read(into),
            Ok(Opened::Dir { .. }) => return Ok(Err(Errno::ISDIR)),
            Ok(Opened::Stdout | Opened::Stderr) => return Ok(Err(Errno::BADF)),
            Err(errno) => return Ok(Err(errno)),
        };
        Ok(got.map_err(|e| errno(&e)))
    }

    fn fd_filetype(&mut self, fd: u32) -> Answer<Filetype> {
        Ok(match self.get(fd) {
            Ok(Opened::Stdin | Opened::Stdout | Opened::Stderr) => Ok(Filetype::CharacterDevice),
            Ok(Opened::Dir { .. }) => Ok(Filetype::Directory),
            Ok(Opened::File(file)) => {
                file.metadata().map(|meta| filetype(&meta)).map_err(|e| errno(&e))
            }
            Err(errno) => Err(errno),
        })
    }

    fn set_fd_flags(&mut self, fd: u32, _flags: u32) -> Answer<()> {
        Ok(Err(match self.get(fd) {
            Ok(_) => Errno::NOTSUP,
            Err(errno) => errno,
        }))
    }

    /// Stdio and `/` stay open: closing them is a success that keeps them.
    fn close(&mut self, fd: u32) -> Answer<()> {
        Ok(match fd {
            0..=ROOT_FD => Ok(()),
            _ => match self.fds.get_mut(fd as usize).and_then(Option::take) {
                Some(_) => Ok(()),
                None => Err(Errno::BADF),
            },
        })
    }

    fn seek(&mut self, fd: u32, offset: i64, whence: u32) -> Answer<u64> {
        let to = match (whence, u64::try_from(offset)) {
            (0, Ok(at)) => SeekFrom::Start(at),
            (0, Err(_)) => return Ok(Err(Errno::INVAL)),
            (1, _) => SeekFrom::Current(offset),
            (2, _) => SeekFrom::End(offset),
            _ => return Ok(Err(Errno::INVAL)),
        };
        Ok(match self.get(fd) {
            Ok(Opened::File(file)) => file.seek(to).map_err(|e| errno(&e)),
            Ok(Opened::Dir { .. }) if to == SeekFrom::Start(0) => Ok(0),
            Ok(Opened::Dir { .. }) => Err(Errno::INVAL),
            Ok(Opened::Stdin | Opened::Stdout | Opened::Stderr) => Err(Errno::SPIPE),
            Err(errno) => Err(errno),
        })
    }

    fn fd_stat(&mut self, fd: u32) -> Answer<Filestat> {
        let meta = match self.get(fd) {
            Ok(Opened::File(file)) => file.metadata(),
            Ok(Opened::Dir { path, .. }) => fs::metadata(path),
            Ok(Opened::Stdin | Opened::Stdout | Opened::Stderr) => {
                let stat = Filestat {
                    dev: 0,
                    ino: 0,
                    filetype: Filetype::CharacterDevice,
                    nlink: 1,
                    size: 0,
                    atim: 0,
                    mtim: 0,
                    ctim: 0,
                };
                return Ok(Ok(stat));
            }
            Err(errno) => return Ok(Err(errno)),
        };
        Ok(meta.map(|meta| filestat(&meta)).map_err(|e| errno(&e)))
    }

    fn open(&mut self, at: u32, path: &[u8], how: Open) -> Answer<u32> {
        let path = match self.resolve(at, path) {
            Ok(path) => path,
            Err(errno) => return Ok(Err(errno)),
        };
        let writes = how.write() || how.append() || how.truncate() || how.create();
        let found = metadata(&path, how.follow());
        if how.directory() || (!writes && found.as_ref().is_ok_and(Metadata::is_dir)) {
            return match found {
                Ok(meta) if meta.is_dir() => {
                    Ok(Ok(self.insert(Opened::Dir { path, entries: None })?))
                }
                Ok(_) => Ok(Err(Errno::NOTDIR)),
                Err(e) => Ok(Err(errno(&e))),
            };
        }
        let mut options = OpenOptions::new();
        options
            .read(how.read() || !writes)
            // std refuses to create or truncate a file opened neither to write nor to append.
            .write(how.write() || how.truncate() || (how.create() && !how.append()))
            .append(how.append())
            .truncate(how.truncate())
            .create(how.create() && !how.exclusive())
            .create_new(how.create() && how.exclusive());
        if !how.follow() {
            options.custom_flags(libc::O_NOFOLLOW);
        }
        match options.open(&path) {
            Ok(file) => Ok(Ok(self.insert(Opened::File(file))?)),
            Err(e) => Ok(Err(errno(&e))),
        }
    }

    fn path_stat(&mut self, at: u32, path: &[u8], follow: bool) -> Answer<Filestat> {
        Ok(match self.resolve(at, path) {
            Ok(path) => metadata(&path, follow).map(|meta| filestat(&meta)).map_err(|e| errno(&e)),
            Err(errno) => Err(errno),
        })
    }

    fn readlink(&mut self, at: u32, path: &[u8]) -> Answer<Vec<u8>> {
        Ok(match self.resolve(at, path) {
            // alloc: the link's target, given to the guest
            Ok(path) => fs::read_link(path)
                .map(|target| target.into_os_string().into_vec())
                .map_err(|e| errno(&e)),
            Err(errno) => Err(errno),
        })
    }

    fn dir_entry(&mut self, fd: u32, cookie: u64) -> Answer<Option<Dirent<'_>>> {
        let (path, entries) = match self.get(fd) {
            Ok(Opened::Dir { path, entries }) => (path, entries),
            Ok(_) => return Ok(Err(Errno::NOTDIR)),
            Err(errno) => return Ok(Err(errno)),
        };
        if entries.is_none() {
            match list(path) {
                Ok(listed) => *entries = Some(listed),
                Err(e) => return Ok(Err(errno(&e))),
            }
        }
        let entry = entries.as_ref().and_then(|entries| entries.get(usize::try_from(cookie).ok()?));
        Ok(Ok(entry.map(|entry| Dirent {
            ino: entry.ino,
            filetype: entry.filetype,
            name: &entry.name,
        })))
    }

    fn poll_oneoff(&mut self) -> Stop {
        // alloc: error path
        Stop("waits (`poll_oneoff`, as `thread::sleep` does), which the seed runner does not serve: the seed builds in one pass and never sleeps".to_owned())
    }

    fn exit(&mut self, code: u32) -> Stop {
        self.exit = Some(code);
        Stop(format!("exited with code {code}")) // alloc: once, at the end
    }
}

/// The `timespec` of a time to set: `UTIME_OMIT` to keep it, `UTIME_NOW` for the clock.
fn timespec(time: Time) -> libc::timespec {
    match time {
        Time::Keep => libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT },
        Time::Now => libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_NOW },
        Time::At(ns) => libc::timespec {
            tv_sec: (ns / 1_000_000_000) as libc::time_t,
            tv_nsec: (ns % 1_000_000_000) as libc::c_long,
        },
    }
}

/// The `errno` of a host call's result: `rc` 0 is success, else the thread's `errno`.
fn checked(rc: libc::c_int) -> Result<(), Errno> {
    if rc == 0 { Ok(()) } else { Err(errno(&io::Error::last_os_error())) }
}

/// Sets the times of `path`, its last link followed or not.
fn set_times_at(path: &Path, follow: bool, atime: Time, mtime: Time) -> Result<(), Errno> {
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| Errno::INVAL)?; // alloc: the C path of one call
    let times = [timespec(atime), timespec(mtime)];
    let flags = if follow { 0 } else { libc::AT_SYMLINK_NOFOLLOW };
    // SAFETY: `path` is a NUL-terminated string and `times` two live `timespec`s, both alive
    // for the call.
    checked(unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), flags) })
}

/// A path the seed named, absolute (`blaze_wasi::seed` refuses a relative one).
fn host(path: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(path))
}

impl Host {
    /// The file `fd` opened: a directory is `EISDIR`, stdio `ESPIPE`.
    fn file(&mut self, fd: u32) -> Result<&mut File, Errno> {
        match self.get(fd)? {
            Opened::File(file) => Ok(file),
            Opened::Dir { .. } => Err(Errno::ISDIR),
            Opened::Stdin | Opened::Stdout | Opened::Stderr => Err(Errno::SPIPE),
        }
    }
}

/// The seed's own calls: the file system changed as native blaze would change it, and its
/// memory given back.
impl Seed for Host {
    fn create_dir(&mut self, path: &[u8]) -> Result<(), Errno> {
        fs::create_dir(host(path)).map_err(|e| errno(&e))
    }

    fn remove_dir(&mut self, path: &[u8]) -> Result<(), Errno> {
        fs::remove_dir(host(path)).map_err(|e| errno(&e))
    }

    fn unlink(&mut self, path: &[u8]) -> Result<(), Errno> {
        fs::remove_file(host(path)).map_err(|e| errno(&e))
    }

    fn rename(&mut self, from: &[u8], to: &[u8]) -> Result<(), Errno> {
        fs::rename(host(from), host(to)).map_err(|e| errno(&e))
    }

    fn hard_link(&mut self, from: &[u8], to: &[u8]) -> Result<(), Errno> {
        fs::hard_link(host(from), host(to)).map_err(|e| errno(&e))
    }

    fn symlink(&mut self, target: &[u8], path: &[u8]) -> Result<(), Errno> {
        std::os::unix::fs::symlink(OsStr::from_bytes(target), host(path)).map_err(|e| errno(&e))
    }

    fn set_times(
        &mut self,
        path: &[u8],
        follow: bool,
        atime: Time,
        mtime: Time,
    ) -> Result<(), Errno> {
        set_times_at(host(path), follow, atime, mtime)
    }

    fn fd_set_times(&mut self, fd: u32, atime: Time, mtime: Time) -> Result<(), Errno> {
        match self.get(fd)? {
            Opened::File(file) => {
                let times = [timespec(atime), timespec(mtime)];
                // SAFETY: the file's descriptor is open for the call; `times` is two live `timespec`s.
                checked(unsafe { libc::futimens(file.as_raw_fd(), times.as_ptr()) })
            }
            Opened::Dir { path, .. } => set_times_at(path, true, atime, mtime),
            Opened::Stdin | Opened::Stdout | Opened::Stderr => Err(Errno::BADF),
        }
    }

    fn set_len(&mut self, fd: u32, len: u64) -> Result<(), Errno> {
        self.file(fd)?.set_len(len).map_err(|e| errno(&e))
    }

    fn sync(&mut self, fd: u32, data_only: bool) -> Result<(), Errno> {
        let file = self.file(fd)?;
        // durable: carries out the guest's own `fd_sync` or `fd_datasync`: the program asked
        if data_only { file.sync_data() } else { file.sync_all() }.map_err(|e| errno(&e))
    }

    fn pread(&mut self, fd: u32, into: &mut [u8], offset: u64) -> Result<usize, Errno> {
        self.file(fd)?.read_at(into, offset).map_err(|e| errno(&e))
    }

    fn pwrite(&mut self, fd: u32, bytes: &[u8], offset: u64) -> Result<usize, Errno> {
        self.file(fd)?.write_at(bytes, offset).map_err(|e| errno(&e))
    }

    fn decommit(&mut self, range: &mut [u8]) -> Result<(), Stop> {
        // SAFETY: `range` is bytes of the seed's memory, a `memory::Reservation`'s readable and
        // writable pages, which only the seed, stopped in this call, references.
        unsafe { crate::memory::decommit(range.as_mut_ptr(), range.len()) }.map_err(|why| {
            // alloc: error path
            Stop(format!(
                "decommitted {} bytes at {:p} of its memory, which the host refused: {why}",
                range.len(),
                range.as_ptr()
            ))
        })
    }
}
