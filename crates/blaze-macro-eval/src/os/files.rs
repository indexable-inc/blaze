//! The files a macro reads: its descriptors and directory streams, each path walked as the
//! kernel walks it (made absolute against the working directory or an `*at` call's directory,
//! each `..` and link followed), and refused when where it leads is outside the roots.
//!
//! A descriptor holds what it names whole, read at its open: a file's bytes, so a `read` is a
//! copy from them, or a directory's sorted names, listed at its first `readdir`. Its slot is
//! kept when it is closed, buffers and all, for the next open.

use std::{
    fs,
    hash::Hasher as _,
    io::{self, Read as _},
};

use blaze_usercode::trace::Seen;

use super::{
    Answer, Done, Noted, Os, fail, fail_null, ok,
    target::{DIRENT_MAX, Errno, Kind, STAT_MAX, STATX_SIZE, Stat, Target},
};
use crate::guest::{Guest, Stop, put_word};

/// The first descriptor a macro is given: 0, 1 and 2 are the evaluator's.
pub(crate) const FIRST_FD: i64 = 3;

/// How many links a walk follows before it is a loop, as Linux's `MAXSYMLINKS`.
const MAX_LINKS: u32 = 40;

const W_OK: i64 = 2;
const X_OK: i64 = 1;

/// `fcntl`'s commands the shims answer: the same on Linux and macOS.
const F_GETFD: i64 = 1;
const F_GETFL: i64 = 3;

const SEEK_SET: i64 = 0;
const SEEK_CUR: i64 = 1;
const SEEK_END: i64 = 2;

/// What a fresh directory stream's `struct dirent` starts as.
const ZEROS: [u8; DIRENT_MAX] = [0; DIRENT_MAX];

/// The variables whose directories a macro may read.
pub(super) const MANIFEST_DIR: &str = "CARGO_MANIFEST_DIR";
pub(super) const OUT_DIR: &str = "OUT_DIR";

/// The directories a macro may read, each its real path.
pub(super) struct Roots {
    dirs: [Option<String>; 2],
}

impl Roots {
    /// The root `path` is below or is, when it is in one.
    fn holding(&self, path: &str) -> Option<&str> {
        self.dirs.iter().flatten().map(String::as_str).find(|root| under(path, root))
    }
}

/// Whether the absolute `path` names the real directory `dir` as it is spelt: `dir` with any
/// trailing `/` and `/.`, and nothing else (no `..` or link, which would make the answer
/// depend on what the walk passes through).
fn same_dir(path: &str, dir: &str) -> bool {
    let mut path = path;
    loop {
        if let Some(rest) = path.strip_suffix("/.") {
            path = rest;
        } else if path.len() > 1
            && let Some(rest) = path.strip_suffix('/')
        {
            path = rest;
        } else {
            break;
        }
    }
    let path = if path.is_empty() { "/" } else { path };
    path == dir
}

/// Whether `path` is `dir` or below it.
fn under(path: &str, dir: &str) -> bool {
    dir == "/"
        || (path.starts_with(dir) && matches!(path.as_bytes().get(dir.len()), None | Some(b'/')))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Open {
    #[default]
    Closed,
    File,
    Dir,
}

/// One descriptor's slot.
#[derive(Default)]
pub(super) struct Slot {
    open: Open,
    /// The real path of what it names.
    path: String,
    ino: u64,
    /// A file's bytes, and where the next `read` starts.
    bytes: Vec<u8>,
    at: u64,
    /// A directory's names, end to end, and each entry, sorted; listed once.
    names: Vec<u8>,
    entries: Vec<Entry>,
    listed: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Entry {
    start: u32,
    end: u32,
    kind: Kind,
    ino: u64,
}

/// A directory stream: the `DIR *` the program holds is the address of its `struct dirent`,
/// which each `readdir` fills and gives back. Kept when closed, with its `dirent`, for the next.
pub(super) struct Stream {
    /// Its descriptor, `None` once closed.
    fd: Option<i64>,
    next: usize,
    dirent: u64,
}

/// Where a relative path starts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum At {
    /// The working directory: the one cargo runs rustc in (the call's, `Os::cwd`).
    Cwd,
    /// `/`, the one directory a wasm macro's WASI is given (`wasm/wasi.rs`): wasi-libc hands
    /// the host an absolute path made relative to it.
    Root,
    /// A directory the macro opened, by its descriptor.
    Fd(i64),
}

impl At {
    /// What the `*at` calls' directory `at` of `t` names: `AT_FDCWD` or a descriptor.
    fn of(t: Target, at: i64) -> At {
        if at == t.at_flags().fdcwd { At::Cwd } else { At::Fd(at) }
    }
}

/// One entry of a directory a macro listed, as `readdir` and WASI's `fd_readdir` give it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct DirEntry<'s> {
    pub(crate) name: &'s [u8],
    pub(crate) kind: Kind,
    pub(crate) ino: u64,
}

/// What a walk found at the end of a path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Found {
    /// A file, a directory, a link not followed or something else, and its size (a link's,
    /// that of its target's path).
    Is(Kind, u64),
    /// Nothing, as `ENOENT`, or `ENOTDIR` for a path through a file.
    Missing(Errno),
}

pub(super) enum Fail {
    Errno(Errno),
    Io(io::Error),
    /// A link whose target is not UTF-8.
    NotUtf8,
}

/// Which `struct` a look at a file fills.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Form {
    Stat,
    Statx,
}

/// Walks the absolute path `given` as the kernel does, each `.`, `..` and link resolved, the
/// last link only with `follow`, leaving the real path in `out` (`rest` is the walk's own).
/// `start` is a real directory `given` is in, whose own walk is skipped. A missing path's
/// real path is that of the first part that is missing.
pub(super) fn walk(
    given: &str,
    follow: bool,
    start: Option<&str>,
    out: &mut String,
    rest: &mut String,
) -> Result<Found, Fail> {
    out.clear();
    rest.clear();
    match start {
        Some(root) => {
            out.push_str(root.trim_end_matches('/'));
            rest.push_str(&given[root.len()..]);
        }
        None => rest.push_str(given),
    }
    let mut links = 0;
    let mut at = 0;
    let found = loop {
        while rest.as_bytes().get(at) == Some(&b'/') {
            at += 1;
        }
        if at == rest.len() {
            break Found::Is(Kind::Dir, 0);
        }
        let end = rest[at..].find('/').map_or(rest.len(), |it| at + it);
        let last = rest[end..].trim_start_matches('/').is_empty();
        let slash = end < rest.len();
        let part = at..end;
        at = end;
        match &rest[part.start..part.end] {
            "." => continue,
            ".." => {
                out.truncate(out.rfind('/').unwrap_or(0));
                continue;
            }
            _ => {}
        }
        let parent = out.len();
        out.push('/');
        out.push_str(&rest[part]);
        let meta = match fs::symlink_metadata(&*out) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // The whole path that is missing, so that it is the one keyed and checked
                // against the roots, not only its first missing part.
                for part in rest[at..].split('/').filter(|it| !it.is_empty()) {
                    out.push('/');
                    out.push_str(part);
                }
                break Found::Missing(Errno::Noent);
            }
            Err(error) if error.kind() == io::ErrorKind::NotADirectory => {
                break Found::Missing(Errno::Notdir);
            }
            Err(error) => return Err(Fail::Io(error)),
        };
        if meta.is_symlink() && (!last || follow || slash) {
            links += 1;
            if links > MAX_LINKS {
                return Err(Fail::Errno(Errno::Loop));
            }
            let target = fs::read_link(&*out).map_err(Fail::Io)?;
            let Some(target) = target.to_str() else { return Err(Fail::NotUtf8) };
            out.truncate(parent);
            if target.starts_with('/') {
                out.clear();
            }
            rest.replace_range(..at, target);
            at = 0;
            continue;
        }
        if last {
            if slash && !meta.is_dir() {
                break Found::Missing(Errno::Notdir);
            }
            let kind = if meta.is_dir() {
                Kind::Dir
            } else if meta.is_file() {
                Kind::File
            } else if meta.is_symlink() {
                Kind::Link
            } else {
                Kind::Other
            };
            break Found::Is(kind, meta.len());
        }
        if !meta.is_dir() {
            break Found::Missing(Errno::Notdir);
        }
    };
    if out.is_empty() {
        out.push('/');
    }
    Ok(found)
}

/// The inode of the real path `path`: the same for it every run, and nonzero.
pub(super) fn ino_of(path: &[u8]) -> u64 {
    let mut hasher = blaze_hash::FixedHasher::default();
    hasher.write(path);
    hasher.finish() | 1
}

impl<'a> Os<'a> {
    /// The roots, found and noted on the first path: which directories may be read depends on
    /// the two variables, so an expansion that reads a path is keyed by them.
    fn ready(&mut self) {
        if self.roots.is_some() {
            return;
        }
        self.trace.note(Noted::Seen(Seen::Env), MANIFEST_DIR);
        self.trace.note(Noted::Seen(Seen::Env), OUT_DIR);
        let mut dirs = [None, None];
        for (dir, name) in dirs.iter_mut().zip([MANIFEST_DIR, OUT_DIR]) {
            let Some((_, value)) = self.env.iter().find(|it| it.0 == name) else { continue };
            if !value.starts_with('/') {
                continue;
            }
            // A root that is not there is walked as far as it is, so that a path below it
            // is the same path a walk of the path itself makes.
            if walk(value, true, None, &mut self.real, &mut self.rest).is_ok() {
                // alloc: setup, once per expansion that reads a path
                *dir = Some(self.real.clone());
            }
        }
        self.roots = Some(Roots { dirs });
    }

    /// The real path of the working directory ([`Os::cwd`]), walked once, in
    /// [`Os::cwd_real`]. Not noted: the expansion's key holds the directory it was given (the
    /// call's directory), and its real path is walked as the roots' are, once per expansion.
    fn find_cwd(&mut self) {
        if self.cwd_real.is_some() {
            return;
        }
        let dir = self.cwd;
        let real = match dir.starts_with('/') {
            // A directory that is not there is walked as far as it is, as a root is.
            true => match walk(dir, true, None, &mut self.real, &mut self.rest) {
                // alloc: once per expansion that names a relative path or asks its directory
                Ok(_) => Some(self.real.clone()),
                Err(_) => None,
            },
            false => None,
        };
        self.cwd_real = Some(real);
    }

    /// The real path of the working directory, or `None` when the expansion was given none it
    /// can resolve from: what the wasm module's glue enters (`guest/entry.rs`) before its
    /// macro runs, as rustc's process is in it.
    pub(crate) fn current_dir(&mut self) -> Option<&str> {
        self.find_cwd();
        self.cwd_real.as_ref().and_then(Option::as_deref)
    }

    /// Why a relative path or the working directory can't be answered: the expansion was given
    /// no absolute directory to start from.
    fn no_cwd(&self, what: std::fmt::Arguments<'_>) -> Stop {
        // alloc: error path
        Stop(format!(
            "{what}, and was given the working directory `{}`, which is not absolute",
            self.cwd
        ))
    }

    /// `path` made absolute from the directory `at` names, in [`Os::given`].
    fn absolute(&mut self, at: At, path: &str) -> Result<Result<(), Errno>, Stop> {
        self.given.clear();
        if !path.starts_with('/') {
            if let At::Cwd = at {
                self.find_cwd();
                match &self.cwd_real {
                    Some(Some(cwd)) => self.given.push_str(cwd),
                    _ => return Err(self.no_cwd(format_args!("names the relative path `{path}`"))),
                }
            } else if let At::Fd(at) = at {
                match slot(&self.fds, at) {
                    Some(slot) if slot.open == Open::Dir => self.given.push_str(&slot.path),
                    Some(_) => return Ok(Err(Errno::Notdir)),
                    None => return Ok(Err(Errno::Badf)),
                }
            }
        }
        for part in path.split('/').filter(|it| !it.is_empty()) {
            self.given.push('/');
            self.given.push_str(part);
        }
        if path.ends_with('/') || self.given.is_empty() {
            self.given.push('/');
        }
        Ok(Ok(()))
    }

    /// What `path` from the directory `at` names, walked to [`Os::real`] and noted as `seen`:
    /// the real path, and the path as given when it differs (a link or a `..` on the way,
    /// which a change can make lead elsewhere). A path outside the roots is refused.
    fn locate(
        &mut self,
        (at, path): (At, &str),
        follow: bool,
        seen: Seen,
    ) -> Result<Result<(Kind, u64), Errno>, Stop> {
        // Each path a macro names is a span of its expansion, the path as it gave it (its
        // `subject`, the profile's `function` column).
        let _span = tracing::info_span!(target: "blaze", "proc macro reads a path", subject = path, seen = ?seen).entered();
        if path.is_empty() {
            return Ok(Err(Errno::Noent));
        }
        if let Err(errno) = self.absolute(at, path)? {
            return Ok(Err(errno));
        }
        // A look at the working directory itself (wasi-libc's `chdir` makes one before it
        // enters it, `stat(".")` another) says it is a directory, as it is for rustc's process:
        // nothing the key does not hold, so it is noted as nothing, read nowhere and refused
        // nowhere, even when the directory is outside the roots (a workspace's root). What is
        // in it is not answered: a listing or an open of it is walked and checked as any path.
        if seen == Seen::Looked {
            self.find_cwd();
            if let Some(Some(cwd)) = &self.cwd_real
                && same_dir(&self.given, cwd)
            {
                self.real.clear();
                self.real.push_str(cwd);
                return Ok(Ok((Kind::Dir, 0)));
            }
        }
        self.ready();
        let roots = self.roots.as_ref();
        let start = roots.and_then(|roots| roots.holding(&self.given));
        let walked = walk(&self.given, follow, start, &mut self.real, &mut self.rest);
        let found = match walked {
            Ok(found) => found,
            Err(Fail::Io(error)) => {
                // alloc: error path
                let path = std::mem::take(&mut self.real);
                self.cant(&path, &error);
                self.real = path;
                return Ok(Err(Errno::Io));
            }
            Err(Fail::Errno(errno)) => {
                // alloc: the message of an expansion that can't be keyed (error path)
                let why = format!("{}: a loop of links", self.given);
                self.unkeyable.get_or_insert(why);
                return Ok(Err(errno));
            }
            Err(Fail::NotUtf8) => {
                // alloc: error path
                return Err(Stop(format!(
                    "follows the link `{}`, whose target is not UTF-8",
                    self.real
                )));
            }
        };
        if !self.roots.as_ref().is_some_and(|roots| roots.holding(&self.real).is_some()) {
            self.trace.note(Noted::RefusedRead, &self.real);
            if self.refusal_stops {
                return Err(self.refusal(path));
            }
            return Ok(Err(Errno::Acces));
        }
        self.trace.note(Noted::Seen(seen), &self.real);
        if self.given != self.real {
            self.trace.note(Noted::Seen(seen), &self.given);
        }
        Ok(match found {
            Found::Is(kind, size) => Ok((kind, size)),
            Found::Missing(errno) => Err(errno),
        })
    }

    /// Why the expansion stops on reading `path` (as the macro named it), which leads to
    /// [`Os::real`], outside the directories it may read.
    fn refusal(&self, path: &str) -> Stop {
        let root = |n: usize| match self.roots.as_ref().and_then(|roots| roots.dirs[n].as_deref()) {
            Some(dir) => dir,
            None => "unset",
        };
        let real = match path == self.real {
            true => String::new(),
            // alloc: error path
            false => format!(" (which is `{}`)", self.real),
        };
        // alloc: error path
        Stop(format!(
            "reads `{path}`{real}, outside the directories a proc macro may read: its crate's \
             {MANIFEST_DIR} (`{}`) and {OUT_DIR} (`{}`)",
            root(0),
            root(1)
        ))
    }

    /// Stops on a read of `/` itself, which a wasm macro's WASI names as its one directory.
    pub(crate) fn refuse_root(&mut self) -> Stop {
        self.real.clear();
        self.real.push('/');
        self.trace.note(Noted::RefusedRead, "/");
        self.ready();
        self.refusal("/")
    }

    /// A path the program passed, or why not: `EFAULT` for none. One that is not UTF-8 stops
    /// the expansion, as the trace names paths by UTF-8.
    fn path<'g>(t: Target, g: &'g impl Guest, addr: u64) -> Result<Result<&'g str, Errno>, Stop> {
        if addr == 0 {
            return Ok(Err(Errno::Fault));
        }
        let bytes = g.read_c_str(addr, t.path_max())?;
        match std::str::from_utf8(bytes) {
            Ok(path) => Ok(Ok(path)),
            // alloc: error path
            Err(_) => Err(Stop(format!(
                "names the path {:?}, which is not UTF-8, and blaze traces what a proc macro \
                 reads by UTF-8 paths",
                // alloc: error path
                String::from_utf8_lossy(bytes)
            ))),
        }
    }

    /// A free slot, the lowest, as the kernel gives the lowest free descriptor.
    fn free_slot(&mut self) -> usize {
        match self.fds.iter().position(|it| it.open == Open::Closed) {
            Some(at) => at,
            None => {
                self.fds.push(Slot::default());
                self.fds.len() - 1
            }
        }
    }

    pub(super) fn open(
        &mut self,
        t: Target,
        g: &mut impl Guest,
        ret: &mut [u8],
        (at, addr, flags): (i64, u64, i64),
    ) -> Done {
        let path = match Self::path(t, g, addr)? {
            Ok(path) => path,
            Err(errno) => return fail(t, g, ret, errno),
        };
        let open = t.open_flags();
        if flags & open.writes != 0 {
            // alloc: error path
            return Err(Stop(format!("opens `{path}` to write, which a proc macro may not")));
        }
        let follow = flags & open.nofollow == 0;
        match self.open_path((At::of(t, at), path), follow, flags & open.directory != 0)? {
            Ok(fd) => ok(ret, fd as u64),
            Err(errno) => fail(t, g, ret, errno),
        }
    }

    /// Opens what `path` from `at` names to read, the last link followed with `follow`, a
    /// directory only with `directory`: its descriptor. The shims check that it does not
    /// write before they ask.
    pub(crate) fn open_path(
        &mut self,
        (at, path): (At, &str),
        follow: bool,
        directory: bool,
    ) -> Result<Result<i64, Errno>, Stop> {
        let (kind, _) = match self.locate((at, path), follow, Seen::Read)? {
            Ok(found) => found,
            Err(errno) => return Ok(Err(errno)),
        };
        match kind {
            Kind::File if directory => Ok(Err(Errno::Notdir)),
            Kind::Link => Ok(Err(Errno::Loop)),
            // alloc: error path
            Kind::Other => {
                Err(Stop(format!("opens `{}`, which is neither a file nor a directory", self.real)))
            }
            Kind::File | Kind::Dir => {
                let fd = self.free_slot();
                let slot = &mut self.fds[fd];
                slot.path.clear();
                slot.path.push_str(&self.real);
                slot.ino = ino_of(self.real.as_bytes());
                slot.at = 0;
                slot.bytes.clear();
                slot.listed = false;
                if kind == Kind::File {
                    let _span = tracing::info_span!(target: "blaze", "proc macro reads a file", subject = %self.real).entered();
                    let read = fs::File::open(&self.real)
                        .and_then(|mut file| file.read_to_end(&mut slot.bytes));
                    if let Err(error) = read {
                        let path = std::mem::take(&mut self.real);
                        self.cant(&path, &error);
                        self.real = path;
                        return Ok(Err(Errno::Io));
                    }
                }
                let slot = &mut self.fds[fd];
                slot.open = if kind == Kind::File { Open::File } else { Open::Dir };
                Ok(Ok(fd as i64 + FIRST_FD))
            }
        }
    }

    pub(super) fn read(
        &mut self,
        t: Target,
        g: &mut impl Guest,
        ret: &mut [u8],
        (fd, buf, len, offset): (i64, u64, u64, Option<i64>),
    ) -> Done {
        let offset = match offset {
            Some(offset) if offset < 0 => return fail(t, g, ret, Errno::Inval),
            Some(offset) => Some(offset as u64),
            None => None,
        };
        match self.read_fd(fd, len, offset) {
            Ok(bytes) => {
                if !bytes.is_empty() {
                    g.write(buf, bytes)?;
                }
                ok(ret, bytes.len() as u64)
            }
            Err(errno) => fail(t, g, ret, errno),
        }
    }

    /// The next bytes of the file `fd`, at most `len`: from `offset`, or from where the last
    /// read ended, which it moves past them.
    pub(crate) fn read_fd(
        &mut self,
        fd: i64,
        len: u64,
        offset: Option<u64>,
    ) -> Result<&[u8], Errno> {
        let Some(slot) = slot_mut(&mut self.fds, fd) else { return Err(Errno::Badf) };
        if slot.open == Open::Dir {
            return Err(Errno::Isdir);
        }
        let from = offset.unwrap_or(slot.at);
        let start = from.min(slot.bytes.len() as u64) as usize;
        let end = start + (len.min(usize::MAX as u64) as usize).min(slot.bytes.len() - start);
        if offset.is_none() {
            slot.at = from + (end - start) as u64;
        }
        Ok(&slot.bytes[start..end])
    }

    pub(super) fn readv(
        &mut self,
        t: Target,
        g: &mut impl Guest,
        ret: &mut [u8],
        (fd, iov, count): (i64, u64, i64),
    ) -> Done {
        if count < 0 {
            return fail(t, g, ret, Errno::Inval);
        }
        if let Err(errno) = self.read_fd(fd, 0, None) {
            return fail(t, g, ret, errno);
        }
        let mut total = 0;
        for n in 0..count as u64 {
            let base = g.read_u64(iov + 16 * n)?;
            let len = g.read_u64(iov + 16 * n + 8)?;
            let Ok(bytes) = self.read_fd(fd, len, None) else { break };
            if !bytes.is_empty() {
                g.write(base, bytes)?;
            }
            total += bytes.len() as u64;
        }
        ok(ret, total)
    }

    pub(super) fn close(&mut self, t: Target, g: &mut impl Guest, ret: &mut [u8], fd: i64) -> Done {
        match self.close_fd(fd) {
            Ok(()) => ok(ret, 0),
            Err(errno) => fail(t, g, ret, errno),
        }
    }

    pub(crate) fn close_fd(&mut self, fd: i64) -> Result<(), Errno> {
        let Some(slot) = slot_mut(&mut self.fds, fd) else { return Err(Errno::Badf) };
        slot.open = Open::Closed;
        Ok(())
    }

    pub(super) fn lseek(
        &mut self,
        t: Target,
        g: &mut impl Guest,
        ret: &mut [u8],
        (fd, offset, whence): (i64, i64, i64),
    ) -> Done {
        match self.seek_fd(fd, offset, whence) {
            Ok(at) => ok(ret, at),
            Err(errno) => fail(t, g, ret, errno),
        }
    }

    /// Moves where the next read of `fd` starts, `whence` `SEEK_SET`, `SEEK_CUR` or
    /// `SEEK_END` (0, 1 and 2 on every target the shims model, and in WASI): where it is now.
    pub(crate) fn seek_fd(&mut self, fd: i64, offset: i64, whence: i64) -> Result<u64, Errno> {
        let Some(slot) = slot_mut(&mut self.fds, fd) else { return Err(Errno::Badf) };
        if slot.open == Open::Dir {
            return match (offset, whence) {
                (0, SEEK_SET) => Ok(0),
                _ => Err(Errno::Inval),
            };
        }
        let from = match whence {
            SEEK_SET => 0,
            SEEK_CUR => slot.at as i64,
            SEEK_END => slot.bytes.len() as i64,
            _ => return Err(Errno::Inval),
        };
        match from.checked_add(offset) {
            Some(at) if at >= 0 => {
                slot.at = at as u64;
                Ok(at as u64)
            }
            _ => Err(Errno::Inval),
        }
    }

    pub(super) fn fstat(
        &mut self,
        t: Target,
        g: &mut impl Guest,
        ret: &mut [u8],
        (fd, buf): (i64, u64),
    ) -> Done {
        let stat = match self.stat_fd(fd) {
            Ok(stat) => stat,
            Err(errno) => return fail(t, g, ret, errno),
        };
        let mut out = [0; STAT_MAX];
        g.write(buf, t.put_stat(&stat, &mut out))?;
        ok(ret, 0)
    }

    /// `fstatat`, and `stat` and `lstat` as one at the working directory, and `statx` as
    /// Linux's `syscall` makes it, which std uses for every look there.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn fstatat(
        &mut self,
        t: Target,
        g: &mut impl Guest,
        ret: &mut [u8],
        (at, addr): (i64, u64),
        buf: u64,
        flags: i64,
        form: Form,
    ) -> Done {
        let at_flags = t.at_flags();
        let path = match Self::path(t, g, addr)? {
            Ok(path) => path,
            Err(errno) => return fail(t, g, ret, errno),
        };
        let stat =
            if path.is_empty() && flags & at_flags.empty_path != 0 && at_flags.empty_path != 0 {
                if at == at_flags.fdcwd {
                    match self.look((At::Cwd, "."), true)? {
                        Ok(stat) => stat,
                        Err(errno) => return fail(t, g, ret, errno),
                    }
                } else {
                    match self.stat_fd(at) {
                        Ok(stat) => stat,
                        Err(errno) => return fail(t, g, ret, errno),
                    }
                }
            } else {
                let follow = flags & at_flags.symlink_nofollow == 0;
                match self.look((At::of(t, at), path), follow)? {
                    Ok(stat) => stat,
                    Err(errno) => return fail(t, g, ret, errno),
                }
            };
        if buf == 0 {
            return fail(t, g, ret, Errno::Fault);
        }
        match form {
            Form::Stat => {
                let mut out = [0; STAT_MAX];
                g.write(buf, t.put_stat(&stat, &mut out))?;
            }
            Form::Statx => {
                let mut out = [0; STATX_SIZE];
                g.write(buf, t.put_statx(&stat, &mut out))?;
            }
        }
        ok(ret, 0)
    }

    /// What the open `fd` is, as an `fstat` says it.
    pub(crate) fn stat_fd(&self, fd: i64) -> Result<Stat, Errno> {
        let Some(slot) = slot(&self.fds, fd) else { return Err(Errno::Badf) };
        Ok(match slot.open {
            Open::File => Stat { kind: Kind::File, size: slot.bytes.len() as u64, ino: slot.ino },
            Open::Dir | Open::Closed => Stat { kind: Kind::Dir, size: 0, ino: slot.ino },
        })
    }

    /// A look at what `path` names, as a `stat` says it.
    pub(crate) fn look(
        &mut self,
        at: (At, &str),
        follow: bool,
    ) -> Result<Result<Stat, Errno>, Stop> {
        let (kind, size) = match self.locate(at, follow, Seen::Looked)? {
            Ok(found) => found,
            Err(errno) => return Ok(Err(errno)),
        };
        let size = match kind {
            Kind::File | Kind::Link => size,
            Kind::Dir => 0,
            Kind::Other => {
                // alloc: error path
                return Err(Stop(format!(
                    "looks at `{}`, which is neither a file, a directory nor a link",
                    self.real
                )));
            }
        };
        Ok(Ok(Stat { kind, size, ino: ino_of(self.real.as_bytes()) }))
    }

    pub(super) fn faccessat(
        &mut self,
        t: Target,
        g: &mut impl Guest,
        ret: &mut [u8],
        (at, addr): (i64, u64),
        mode: i64,
        flags: i64,
    ) -> Done {
        let path = match Self::path(t, g, addr)? {
            Ok(path) => path,
            Err(errno) => return fail(t, g, ret, errno),
        };
        let follow = flags & t.at_flags().symlink_nofollow == 0;
        let stat = match self.look((At::of(t, at), path), follow)? {
            Ok(stat) => stat,
            Err(errno) => return fail(t, g, ret, errno),
        };
        if mode & W_OK != 0 {
            return fail(t, g, ret, Errno::Rofs);
        }
        if mode & X_OK != 0 && stat.kind != Kind::Dir {
            return fail(t, g, ret, Errno::Acces);
        }
        ok(ret, 0)
    }

    pub(super) fn getcwd(
        &mut self,
        t: Target,
        g: &mut impl Guest,
        ret: &mut [u8],
        (buf, size): (u64, u64),
    ) -> Done {
        self.find_cwd();
        let Some(Some(cwd)) = &self.cwd_real else {
            return Err(self.no_cwd(format_args!("asks for its working directory")));
        };
        self.bytes.clear();
        self.bytes.extend_from_slice(cwd.as_bytes());
        self.bytes.push(0);
        if buf == 0 {
            let addr = g.alloc(&self.bytes, 1)?;
            self.allocated.push(addr);
            return ok(ret, addr);
        }
        if (size as usize) < self.bytes.len() {
            return fail_null(t, g, ret, Errno::Range);
        }
        g.write(buf, &self.bytes)?;
        ok(ret, buf)
    }

    pub(super) fn realpath(
        &mut self,
        t: Target,
        g: &mut impl Guest,
        ret: &mut [u8],
        (addr, resolved): (u64, u64),
    ) -> Done {
        let path = match Self::path(t, g, addr)? {
            Ok(path) => path,
            Err(errno) => return fail_null(t, g, ret, errno),
        };
        if let Err(errno) = self.locate((At::Cwd, path), true, Seen::Looked)? {
            return fail_null(t, g, ret, errno);
        }
        self.bytes.clear();
        self.bytes.extend_from_slice(self.real.as_bytes());
        self.bytes.push(0);
        if resolved == 0 {
            let addr = g.alloc(&self.bytes, 1)?;
            self.allocated.push(addr);
            return ok(ret, addr);
        }
        if self.bytes.len() > t.path_max() {
            return fail_null(t, g, ret, Errno::Nametoolong);
        }
        g.write(resolved, &self.bytes)?;
        ok(ret, resolved)
    }

    pub(super) fn readlink(
        &mut self,
        t: Target,
        g: &mut impl Guest,
        ret: &mut [u8],
        (addr, buf, size): (u64, u64, u64),
    ) -> Done {
        let path = match Self::path(t, g, addr)? {
            Ok(path) => path,
            Err(errno) => return fail(t, g, ret, errno),
        };
        let target = match self.link_target((At::Cwd, path))? {
            Ok(target) => target,
            Err(errno) => return fail(t, g, ret, errno),
        };
        let bytes = target.as_os_str().as_encoded_bytes();
        let len = bytes.len().min(size as usize);
        g.write(buf, &bytes[..len])?;
        ok(ret, len as u64)
    }

    /// The target of the link `path` from `at` names, `EINVAL` when it is not a link.
    pub(crate) fn link_target(
        &mut self,
        at: (At, &str),
    ) -> Result<Result<std::path::PathBuf, Errno>, Stop> {
        match self.locate(at, false, Seen::Looked)? {
            Ok((Kind::Link, _)) => {}
            Ok(_) => return Ok(Err(Errno::Inval)),
            Err(errno) => return Ok(Err(errno)),
        }
        // alloc: the link's target, as std reads it
        match fs::read_link(&self.real) {
            Ok(target) => Ok(Ok(target)),
            Err(error) => {
                let path = std::mem::take(&mut self.real);
                self.cant(&path, &error);
                self.real = path;
                Ok(Err(Errno::Io))
            }
        }
    }

    pub(super) fn opendir(
        &mut self,
        t: Target,
        g: &mut impl Guest,
        ret: &mut [u8],
        addr: u64,
    ) -> Done {
        let flags = t.open_flags().directory;
        let mut fd = [0; 8];
        match self.open(t, g, &mut fd, (t.at_flags().fdcwd, addr, flags))? {
            Answer::Answered => {}
            Answer::Evaluator => return Ok(Answer::Evaluator),
        }
        let fd = i64::from_le_bytes(fd);
        match fd {
            -1 => ok(ret, 0),
            fd => self.fdopendir(t, g, ret, fd),
        }
    }

    pub(super) fn fdopendir(
        &mut self,
        t: Target,
        g: &mut impl Guest,
        ret: &mut [u8],
        fd: i64,
    ) -> Done {
        match slot(&self.fds, fd) {
            Some(slot) if slot.open == Open::Dir => {}
            Some(_) => return fail_null(t, g, ret, Errno::Notdir),
            None => return fail_null(t, g, ret, Errno::Badf),
        }
        let at = match self.streams.iter().position(|it| it.fd.is_none()) {
            Some(at) => at,
            None => {
                let dirent = g.alloc(&ZEROS[..t.dirent_layout().size], 8)?;
                self.streams.push(Stream { fd: None, next: 0, dirent });
                self.streams.len() - 1
            }
        };
        let stream = &mut self.streams[at];
        stream.fd = Some(fd);
        stream.next = 0;
        ok(ret, stream.dirent)
    }

    pub(super) fn readdir(
        &mut self,
        t: Target,
        g: &mut impl Guest,
        ret: &mut [u8],
        dir: u64,
    ) -> Done {
        let Some(at) = self.stream(dir) else { return fail_null(t, g, ret, Errno::Badf) };
        let Some(fd) = self.streams[at].fd else { return fail_null(t, g, ret, Errno::Badf) };
        let next = self.streams[at].next;
        let entry = match self.dir_entry(fd, next as u64) {
            Ok(Some(entry)) => entry,
            Ok(None) => {
                put_word(ret, 0);
                return Ok(Answer::Answered);
            }
            Err(errno) => return fail_null(t, g, ret, errno),
        };
        let mut out = [0; DIRENT_MAX];
        g.write(dir, t.put_dirent((entry.name, entry.kind, entry.ino, next as u64), &mut out))?;
        self.streams[at].next += 1;
        ok(ret, dir)
    }

    /// Entry `n` of the directory `fd`, `.` and `..` first and the rest sorted by name, or
    /// `None` past its last; listed, and noted listed, on the first ask.
    pub(crate) fn dir_entry(&mut self, fd: i64, n: u64) -> Result<Option<DirEntry<'_>>, Errno> {
        let Some(index) = slot_index(&self.fds, fd) else { return Err(Errno::Badf) };
        if self.fds[index].open != Open::Dir {
            return Err(Errno::Notdir);
        }
        if !self.fds[index].listed {
            if let Err(error) = self.list(index) {
                let path = std::mem::take(&mut self.fds[index].path);
                self.cant(&path, &error);
                self.fds[index].path = path;
                return Err(Errno::Io);
            }
        }
        let slot = &self.fds[index];
        let Some(entry) = usize::try_from(n).ok().and_then(|n| slot.entries.get(n)) else {
            return Ok(None);
        };
        let name = &slot.names[entry.start as usize..entry.end as usize];
        Ok(Some(DirEntry { name, kind: entry.kind, ino: entry.ino }))
    }

    /// Lists the directory of the slot `index`, sorted, with `.` and `..` first as a
    /// `readdir` gives them, and notes it listed.
    fn list(&mut self, index: usize) -> io::Result<()> {
        let slot = &mut self.fds[index];
        let _span = tracing::info_span!(target: "blaze", "proc macro lists a directory", subject = %slot.path).entered();
        slot.names.clear();
        slot.entries.clear();
        let parent_ino = match slot.path.rfind('/') {
            Some(0) | None => ino_of(b"/"),
            Some(at) => ino_of(&slot.path.as_bytes()[..at]),
        };
        for (name, ino) in [(&b"."[..], slot.ino), (&b".."[..], parent_ino)] {
            let start = slot.names.len() as u32;
            slot.names.extend_from_slice(name);
            slot.entries.push(Entry { start, end: slot.names.len() as u32, kind: Kind::Dir, ino });
        }
        // Each entry's path, its directory's once and its name pushed and popped, hashed
        // whole into its inode as `ino_of` hashes a path looked at.
        let path = &mut self.scratch;
        path.clear();
        path.extend_from_slice(slot.path.as_bytes());
        if !slot.path.ends_with('/') {
            path.push(b'/');
        }
        let dir = path.len();
        for entry in fs::read_dir(&slot.path)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let kind = if kind.is_dir() {
                Kind::Dir
            } else if kind.is_file() {
                Kind::File
            } else if kind.is_symlink() {
                Kind::Link
            } else {
                Kind::Other
            };
            let name = entry.file_name();
            let name = name.as_encoded_bytes();
            path.truncate(dir);
            path.extend_from_slice(name);
            let start = slot.names.len() as u32;
            slot.names.extend_from_slice(name);
            let end = slot.names.len() as u32;
            slot.entries.push(Entry { start, end, kind, ino: ino_of(path) });
        }
        let names = &slot.names;
        slot.entries[2..].sort_unstable_by(|a, b| {
            names[a.start as usize..a.end as usize].cmp(&names[b.start as usize..b.end as usize])
        });
        slot.listed = true;
        self.trace.note(Noted::Seen(Seen::Listed), &slot.path);
        Ok(())
    }

    pub(super) fn closedir(
        &mut self,
        t: Target,
        g: &mut impl Guest,
        ret: &mut [u8],
        dir: u64,
    ) -> Done {
        let Some(at) = self.stream(dir) else { return fail(t, g, ret, Errno::Badf) };
        let Some(fd) = self.streams[at].fd.take() else { return fail(t, g, ret, Errno::Badf) };
        if let Some(slot) = slot_mut(&mut self.fds, fd) {
            slot.open = Open::Closed;
        }
        ok(ret, 0)
    }

    pub(super) fn dirfd(
        &mut self,
        t: Target,
        g: &mut impl Guest,
        ret: &mut [u8],
        dir: u64,
    ) -> Done {
        match self.stream(dir).and_then(|at| self.streams[at].fd) {
            Some(fd) => ok(ret, fd as u64),
            None => fail(t, g, ret, Errno::Badf),
        }
    }

    /// `fcntl(fd, cmd, ...)`: `F_GETFD`, which std's `OwnedFd` asks of a descriptor as it
    /// drops it (`debug_assert_fd_is_open`, under `ub_checks`), and `F_GETFL`, which its
    /// `File`'s `Debug` asks: every descriptor open to read, close-on-exec, as std opens it.
    /// Any other command stops the expansion.
    pub(super) fn fcntl(
        &mut self,
        t: Target,
        g: &mut impl Guest,
        ret: &mut [u8],
        (fd, cmd): (i64, i64),
    ) -> Done {
        let open = (0..FIRST_FD).contains(&fd) || slot(&self.fds, fd).is_some();
        match cmd {
            F_GETFD | F_GETFL if !open => fail(t, g, ret, Errno::Badf),
            // FD_CLOEXEC, which std opens every file with; stdio is not.
            F_GETFD => ok(ret, u64::from(fd >= FIRST_FD)),
            // O_RDONLY, 0 on every target the shims model.
            F_GETFL => ok(ret, 0),
            // alloc: error path
            _ => Err(Stop(format!(
                "calls `fcntl` on descriptor {fd} with the command {cmd}, which a proc macro's shims answer only for `F_GETFD` and `F_GETFL`"
            ))),
        }
    }

    /// `free` of what `realpath` or `getcwd` allocated, or of nothing. The evaluator has no
    /// `malloc`, so any other pointer was never C's to free.
    pub(super) fn free(&mut self, addr: u64) -> Done {
        if addr == 0 {
            return Ok(Answer::Answered);
        }
        match self.allocated.iter().position(|&it| it == addr) {
            Some(at) => {
                self.allocated.swap_remove(at);
                Ok(Answer::Answered)
            }
            // alloc: error path
            None => Err(Stop(format!(
                "frees `{addr:#x}`, which no `malloc`, `realpath` or `getcwd` gave it"
            ))),
        }
    }

    /// The open stream whose `DIR *` is `dir`.
    fn stream(&self, dir: u64) -> Option<usize> {
        self.streams.iter().position(|it| it.dirent == dir && it.fd.is_some())
    }
}

fn slot_index(fds: &[Slot], fd: i64) -> Option<usize> {
    let at = usize::try_from(fd.checked_sub(FIRST_FD)?).ok()?;
    (fds.get(at)?.open != Open::Closed).then_some(at)
}

fn slot(fds: &[Slot], fd: i64) -> Option<&Slot> {
    slot_index(fds, fd).map(|at| &fds[at])
}

fn slot_mut(fds: &mut [Slot], fd: i64) -> Option<&mut Slot> {
    slot_index(fds, fd).map(|at| &mut fds[at])
}
