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
const FIRST_FD: i64 = 3;

/// How many links a walk follows before it is a loop, as Linux's `MAXSYMLINKS`.
const MAX_LINKS: u32 = 40;

const W_OK: i64 = 2;
const X_OK: i64 = 1;

const SEEK_SET: i64 = 0;
const SEEK_CUR: i64 = 1;
const SEEK_END: i64 = 2;

/// What a fresh directory stream's `struct dirent` starts as.
const ZEROS: [u8; DIRENT_MAX] = [0; DIRENT_MAX];

/// The variables whose directories a macro may read.
pub(super) const MANIFEST_DIR: &str = "CARGO_MANIFEST_DIR";
pub(super) const OUT_DIR: &str = "OUT_DIR";

/// The directories a macro may read, each its real path, and the working directory: the
/// package's, as cargo runs rustc there.
pub(super) struct Roots {
    dirs: [Option<String>; 2],
}

impl Roots {
    fn cwd(&self) -> Option<&str> {
        self.dirs[0].as_deref()
    }

    /// The root `path` is below or is, when it is in one.
    fn holding(&self, path: &str) -> Option<&str> {
        self.dirs.iter().flatten().map(String::as_str).find(|root| under(path, root))
    }
}

/// Whether `path` is `dir` or below it.
fn under(path: &str, dir: &str) -> bool {
    dir == "/" || (path.starts_with(dir) && matches!(path.as_bytes().get(dir.len()), None | Some(b'/')))
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

    /// `path` made absolute from the directory `at` names, in [`Os::given`].
    fn absolute(&mut self, t: Target, at: i64, path: &str) -> Result<Result<(), Errno>, Stop> {
        self.given.clear();
        if !path.starts_with('/') {
            if at == t.at_flags().fdcwd {
                self.ready();
                let Some(cwd) = self.roots.as_ref().and_then(Roots::cwd) else {
                    // alloc: error path
                    return Err(Stop(format!(
                        "names the relative path `{path}`, and the calling crate has no \
                         `{MANIFEST_DIR}` to resolve it from"
                    )));
                };
                self.given.push_str(cwd);
            } else {
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
        t: Target,
        (at, path): (i64, &str),
        follow: bool,
        seen: Seen,
    ) -> Result<Result<(Kind, u64), Errno>, Stop> {
        if path.is_empty() {
            return Ok(Err(Errno::Noent));
        }
        if let Err(errno) = self.absolute(t, at, path)? {
            return Ok(Err(errno));
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
                return Err(Stop(format!("follows the link `{}`, whose target is not UTF-8", self.real)));
            }
        };
        if !self.roots.as_ref().is_some_and(|roots| roots.holding(&self.real).is_some()) {
            self.trace.note(Noted::RefusedRead, &self.real);
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

    pub(super) fn open(&mut self, t: Target, g: &mut impl Guest, ret: &mut [u8], (at, addr, flags): (i64, u64, i64)) -> Done {
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
        let (kind, _) = match self.locate(t, (at, path), follow, Seen::Read)? {
            Ok(found) => found,
            Err(errno) => return fail(t, g, ret, errno),
        };
        match kind {
            Kind::File if flags & open.directory != 0 => fail(t, g, ret, Errno::Notdir),
            Kind::Link => fail(t, g, ret, Errno::Loop),
            // alloc: error path
            Kind::Other => Err(Stop(format!("opens `{}`, which is neither a file nor a directory", self.real))),
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
                    let read = fs::File::open(&self.real).and_then(|mut file| file.read_to_end(&mut slot.bytes));
                    if let Err(error) = read {
                        let path = std::mem::take(&mut self.real);
                        self.cant(&path, &error);
                        self.real = path;
                        return fail(t, g, ret, Errno::Io);
                    }
                }
                let slot = &mut self.fds[fd];
                slot.open = if kind == Kind::File { Open::File } else { Open::Dir };
                ok(ret, (fd as i64 + FIRST_FD) as u64)
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
        let Some(slot) = slot_mut(&mut self.fds, fd) else { return fail(t, g, ret, Errno::Badf) };
        if slot.open == Open::Dir {
            return fail(t, g, ret, Errno::Isdir);
        }
        let from = match offset {
            Some(offset) if offset < 0 => return fail(t, g, ret, Errno::Inval),
            Some(offset) => offset as u64,
            None => slot.at,
        };
        let start = from.min(slot.bytes.len() as u64) as usize;
        let end = start + (len as usize).min(slot.bytes.len() - start);
        if end > start {
            g.write(buf, &slot.bytes[start..end])?;
        }
        if offset.is_none() {
            slot.at = from + (end - start) as u64;
        }
        ok(ret, (end - start) as u64)
    }

    pub(super) fn readv(&mut self, t: Target, g: &mut impl Guest, ret: &mut [u8], (fd, iov, count): (i64, u64, i64)) -> Done {
        if count < 0 {
            return fail(t, g, ret, Errno::Inval);
        }
        let Some(slot) = slot_mut(&mut self.fds, fd) else { return fail(t, g, ret, Errno::Badf) };
        if slot.open == Open::Dir {
            return fail(t, g, ret, Errno::Isdir);
        }
        let mut total = 0;
        for n in 0..count as u64 {
            let base = g.read_u64(iov + 16 * n)?;
            let len = g.read_u64(iov + 16 * n + 8)?;
            let start = slot.at.min(slot.bytes.len() as u64) as usize;
            let end = start + (len as usize).min(slot.bytes.len() - start);
            if end > start {
                g.write(base, &slot.bytes[start..end])?;
            }
            slot.at += (end - start) as u64;
            total += (end - start) as u64;
        }
        ok(ret, total)
    }

    pub(super) fn close(&mut self, t: Target, g: &mut impl Guest, ret: &mut [u8], fd: i64) -> Done {
        let Some(slot) = slot_mut(&mut self.fds, fd) else { return fail(t, g, ret, Errno::Badf) };
        slot.open = Open::Closed;
        ok(ret, 0)
    }

    pub(super) fn lseek(&mut self, t: Target, g: &mut impl Guest, ret: &mut [u8], (fd, offset, whence): (i64, i64, i64)) -> Done {
        let Some(slot) = slot_mut(&mut self.fds, fd) else { return fail(t, g, ret, Errno::Badf) };
        if slot.open == Open::Dir {
            return match (offset, whence) {
                (0, SEEK_SET) => ok(ret, 0),
                _ => fail(t, g, ret, Errno::Inval),
            };
        }
        let from = match whence {
            SEEK_SET => 0,
            SEEK_CUR => slot.at as i64,
            SEEK_END => slot.bytes.len() as i64,
            _ => return fail(t, g, ret, Errno::Inval),
        };
        match from.checked_add(offset) {
            Some(at) if at >= 0 => {
                slot.at = at as u64;
                ok(ret, at as u64)
            }
            _ => fail(t, g, ret, Errno::Inval),
        }
    }

    pub(super) fn fstat(&mut self, t: Target, g: &mut impl Guest, ret: &mut [u8], (fd, buf): (i64, u64)) -> Done {
        let Some(slot) = slot(&self.fds, fd) else { return fail(t, g, ret, Errno::Badf) };
        let stat = match slot.open {
            Open::File => Stat { kind: Kind::File, size: slot.bytes.len() as u64, ino: slot.ino },
            Open::Dir | Open::Closed => Stat { kind: Kind::Dir, size: 0, ino: slot.ino },
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
        let stat = if path.is_empty() && flags & at_flags.empty_path != 0 && at_flags.empty_path != 0 {
            if at == at_flags.fdcwd {
                match self.look(t, (at, "."), true)? {
                    Ok(stat) => stat,
                    Err(errno) => return fail(t, g, ret, errno),
                }
            } else {
                match slot(&self.fds, at) {
                    Some(slot) => match slot.open {
                        Open::File => Stat { kind: Kind::File, size: slot.bytes.len() as u64, ino: slot.ino },
                        Open::Dir | Open::Closed => Stat { kind: Kind::Dir, size: 0, ino: slot.ino },
                    },
                    None => return fail(t, g, ret, Errno::Badf),
                }
            }
        } else {
            let follow = flags & at_flags.symlink_nofollow == 0;
            match self.look(t, (at, path), follow)? {
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

    /// A look at what `path` names, as a `stat` says it.
    fn look(&mut self, t: Target, at: (i64, &str), follow: bool) -> Result<Result<Stat, Errno>, Stop> {
        let (kind, size) = match self.locate(t, at, follow, Seen::Looked)? {
            Ok(found) => found,
            Err(errno) => return Ok(Err(errno)),
        };
        let size = match kind {
            Kind::File | Kind::Link => size,
            Kind::Dir => 0,
            Kind::Other => {
                // alloc: error path
                return Err(Stop(format!("looks at `{}`, which is neither a file, a directory nor a link", self.real)));
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
        let stat = match self.look(t, (at, path), follow)? {
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

    pub(super) fn getcwd(&mut self, t: Target, g: &mut impl Guest, ret: &mut [u8], (buf, size): (u64, u64)) -> Done {
        self.ready();
        let Some(cwd) = self.roots.as_ref().and_then(Roots::cwd) else {
            // alloc: error path
            return Err(Stop(format!(
                "asks for its working directory, and the calling crate has no `{MANIFEST_DIR}`"
            )));
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

    pub(super) fn realpath(&mut self, t: Target, g: &mut impl Guest, ret: &mut [u8], (addr, resolved): (u64, u64)) -> Done {
        let path = match Self::path(t, g, addr)? {
            Ok(path) => path,
            Err(errno) => return fail_null(t, g, ret, errno),
        };
        if let Err(errno) = self.locate(t, (t.at_flags().fdcwd, path), true, Seen::Looked)? {
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

    pub(super) fn readlink(&mut self, t: Target, g: &mut impl Guest, ret: &mut [u8], (addr, buf, size): (u64, u64, u64)) -> Done {
        let path = match Self::path(t, g, addr)? {
            Ok(path) => path,
            Err(errno) => return fail(t, g, ret, errno),
        };
        match self.locate(t, (t.at_flags().fdcwd, path), false, Seen::Looked)? {
            Ok((Kind::Link, _)) => {}
            Ok(_) => return fail(t, g, ret, Errno::Inval),
            Err(errno) => return fail(t, g, ret, errno),
        }
        // alloc: the link's target, as std reads it
        let target = match fs::read_link(&self.real) {
            Ok(target) => target,
            Err(error) => {
                let path = std::mem::take(&mut self.real);
                self.cant(&path, &error);
                self.real = path;
                return fail(t, g, ret, Errno::Io);
            }
        };
        let bytes = target.as_os_str().as_encoded_bytes();
        let len = bytes.len().min(size as usize);
        g.write(buf, &bytes[..len])?;
        ok(ret, len as u64)
    }

    pub(super) fn opendir(&mut self, t: Target, g: &mut impl Guest, ret: &mut [u8], addr: u64) -> Done {
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

    pub(super) fn fdopendir(&mut self, t: Target, g: &mut impl Guest, ret: &mut [u8], fd: i64) -> Done {
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

    pub(super) fn readdir(&mut self, t: Target, g: &mut impl Guest, ret: &mut [u8], dir: u64) -> Done {
        let Some(at) = self.stream(dir) else { return fail_null(t, g, ret, Errno::Badf) };
        let Some(fd) = self.streams[at].fd else { return fail_null(t, g, ret, Errno::Badf) };
        let Some(index) = slot_index(&self.fds, fd) else { return fail_null(t, g, ret, Errno::Badf) };
        if !self.fds[index].listed {
            if let Err(error) = self.list(index) {
                let path = std::mem::take(&mut self.fds[index].path);
                self.cant(&path, &error);
                self.fds[index].path = path;
                return fail_null(t, g, ret, Errno::Io);
            }
        }
        let slot = &self.fds[index];
        let next = self.streams[at].next;
        let Some(entry) = slot.entries.get(next) else {
            put_word(ret, 0);
            return Ok(Answer::Answered);
        };
        let name = &slot.names[entry.start as usize..entry.end as usize];
        let mut out = [0; DIRENT_MAX];
        g.write(dir, t.put_dirent((name, entry.kind, entry.ino, next as u64), &mut out))?;
        self.streams[at].next += 1;
        ok(ret, dir)
    }

    /// Lists the directory of the slot `index`, sorted, with `.` and `..` first as a
    /// `readdir` gives them, and notes it listed.
    fn list(&mut self, index: usize) -> io::Result<()> {
        let slot = &mut self.fds[index];
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

    pub(super) fn closedir(&mut self, t: Target, g: &mut impl Guest, ret: &mut [u8], dir: u64) -> Done {
        let Some(at) = self.stream(dir) else { return fail(t, g, ret, Errno::Badf) };
        let Some(fd) = self.streams[at].fd.take() else { return fail(t, g, ret, Errno::Badf) };
        if let Some(slot) = slot_mut(&mut self.fds, fd) {
            slot.open = Open::Closed;
        }
        ok(ret, 0)
    }

    pub(super) fn dirfd(&mut self, t: Target, g: &mut impl Guest, ret: &mut [u8], dir: u64) -> Done {
        match self.stream(dir).and_then(|at| self.streams[at].fd) {
            Some(fd) => ok(ret, fd as u64),
            None => fail(t, g, ret, Errno::Badf),
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
            None => Err(Stop(format!("frees `{addr:#x}`, which no `malloc`, `realpath` or `getcwd` gave it"))),
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
