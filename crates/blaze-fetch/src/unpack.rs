//! blaze's one checked unpacker of a tar stream into a directory. A registry's `.crate`
//! (blaze-registry's `download::unpack`, cargo's rules) and a pinned native input's archive
//! (blaze-cargo's `serve::native`, read-only, links kept) both go through [`unpack`], each with
//! its [`Rules`]. The caller decompresses, and bounds what it decompresses; this holds every
//! entry to:
//!
//! - one top directory, stripped: the one [`Rules::top`] names, or whichever the first entry is
//!   under;
//! - no absolute path, no `..`, no `\`, NUL or non-UTF-8 in any name, none longer than
//!   [`Rules::most_path`]; no top-level name [`Rules::reserved`] holds, compared as a
//!   case-insensitive disk compares;
//! - no two entries that are one name on the disk, nor an entry under a file or link: equal
//!   spellings, and with [`Rules::fold`] spellings that Unicode's canonical caseless match
//!   (NFD, full case folding, NFD; [`crate::tree::fold`]) takes as one, meant to cover what a
//!   case- and normalization-insensitive disk (APFS) takes as one; with [`Twice::Replace`] a file listed again
//!   replaces the first, as tar does;
//! - regular files and directories, and with [`Rules::links`] symbolic links whose target stays
//!   inside and passes through no other link of the archive (names compared as above), and hard
//!   links to a file written before them (copied); anything else (devices, FIFOs, sparse files)
//!   is refused;
//! - at most [`Rules::most_entries`] entries and [`Rules::most_bytes`] bytes of files.
//!
//! Nothing is ever written through a link. Every directory, a link's parent too, is made in
//! the first pass, before any link exists, each checked with `symlink_metadata` to be a
//! directory; every file is created new (`create_new`, which follows no link). Links are made
//! last, with `symlink` alone, which follows nothing at its last part. Once they are, each is
//! resolved by the system and must land under the root: a gap in the lexical checks is caught
//! by the disk, and the unpacking refused.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path},
};

use crate::{
    digest::Blake3,
    tree::{Entry, Kind, fold},
};

/// The rules of an archive's names, which [`Names`] holds each entry to with no IO: the one
/// home of them, for [`unpack`] and for a reader that keeps the files in memory (blaze-cargo's
/// `serve/fetch/archive.rs`).
#[derive(Clone, Copy)]
pub struct NameRules<'a> {
    /// The top directory every entry is under, or none to take the first entry's.
    pub top: Option<&'a str>,
    /// Whether names are compared folded ([`crate::tree::fold`]).
    pub fold: bool,
    /// What a file listed twice is.
    pub twice: Twice,
    /// Top-level names refused (compared folded), such as a mark the caller writes beside.
    pub reserved: &'a [&'a str],
    /// The longest name or link target, in bytes.
    pub most_path: usize,
}

/// What [`unpack`] holds an archive to, as the module says.
pub struct Rules<'a> {
    pub names: NameRules<'a>,
    /// Whether symbolic and hard links are kept (else refused).
    pub links: bool,
    /// The modes (and times) written.
    pub modes: Modes,
    /// File names dropped wherever they are (a `.crate`'s `.cargo-ok`).
    pub skip: &'a [&'a str],
    /// Whether an entry, by its path under the top, is unpacked; one left out is skipped.
    pub include: &'a dyn Fn(&Path) -> bool,
    /// Whether every file and directory written is synced to the disk before this returns.
    pub sync: bool,
    pub most_entries: u64,
    pub most_bytes: u64,
}

/// What a file listed twice is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Twice {
    Refuse,
    /// The later replaces the earlier, as tar (and so cargo) unpacks it.
    Replace,
}

/// The modes and times written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Modes {
    /// Every file `0o444`, or `0o555` when its entry is executable; every directory `0o555`
    /// once its entries are in (the root is the caller's).
    ReadOnly,
    /// tar's (and cargo's): each entry's mode, its low nine bits under `!mask`, and each file's
    /// time the entry's (`0` as `1`, as tar writes it for cargo).
    Cargo { mask: u32 },
}

/// What [`unpack`] wrote: every file and link, by its path under the root, sorted by path; and
/// the bytes of its files.
#[derive(Debug)]
pub struct Unpacked {
    pub entries: Vec<Entry>,
    pub bytes: u64,
}

/// Why an archive was not unpacked: the entry it is about, when it is about one, and why.
#[derive(Debug)]
pub struct UnpackRefused {
    pub path: Option<String>,
    pub why: String,
}

impl fmt::Display for UnpackRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(path) => write!(f, "`{path}` {}", self.why),
            None => f.write_str(&self.why),
        }
    }
}

impl std::error::Error for UnpackRefused {}

fn at(path: &str, why: impl Into<String>) -> UnpackRefused {
    UnpackRefused { path: Some(path.to_owned()), why: why.into() } // alloc: error path
}

fn whole(why: impl Into<String>) -> UnpackRefused {
    UnpackRefused { path: None, why: why.into() }
}

/// The tar stream `from` unpacked into `into`, a directory that is there (and no link), as the
/// module says. The caller reads what is left of `from` after, if its format has a trailer.
pub fn unpack(from: impl Read, into: &Path, rules: &Rules<'_>) -> Result<Unpacked, UnpackRefused> {
    match fs::symlink_metadata(into) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return Err(whole(format!("{} is not a directory to unpack into", into.display())));
        }
        Err(error) => return Err(whole(format!("reading {}: {error}", into.display()))),
    }
    let mut state = State {
        into,
        rules,
        names: Names::new(rules.names),
        entries: 0,
        bytes: 0,
        dirs: BTreeSet::new(),
        modes: Vec::new(),
        out: BTreeMap::new(),
        links: Vec::new(),
        link_keys: BTreeSet::new(),
        buffer: vec![0; 1 << 16], // alloc: one copy buffer, once an archive
    };
    let mut archive = tar::Archive::new(from);
    let read = |e: io::Error| whole(format!("reading the archive: {e}"));
    for entry in archive.entries().map_err(read)? {
        state.entry(entry.map_err(read)?)?;
    }
    state.finish()
}

struct State<'r, 'a> {
    into: &'r Path,
    rules: &'r Rules<'a>,
    names: Names<'a>,
    entries: u64,
    bytes: u64,
    /// Every directory under the root made or found, as spelled: each checked once.
    dirs: BTreeSet<String>,
    /// The directories the archive lists with [`Modes::Cargo`]: each one's mode, set at the end.
    modes: Vec<(String, u32)>,
    /// Every file and link written, by its path.
    out: BTreeMap<String, Entry>,
    /// Links, made once every entry is in.
    links: Vec<(String, String)>,
    /// Every link's key ([`Names::key`]), for [`State::stays_in`].
    link_keys: BTreeSet<String>,
    buffer: Vec<u8>,
}

/// The names an archive's entries take, held to [`NameRules`] as each comes, with no IO.
pub struct Names<'a> {
    rules: NameRules<'a>,
    top: Option<String>,
    /// Every name the archive makes, an entry or a directory one is under, by its key
    /// ([`Names::key`]): how it is spelled, and whether it is a directory.
    names: BTreeMap<String, (String, bool)>,
}

impl<'a> Names<'a> {
    pub fn new(rules: NameRules<'a>) -> Names<'a> {
        Names { rules, top: None, names: BTreeMap::new() }
    }

    /// How `path`, a name claimed before, is spelled in the archive, if it was.
    pub fn spelled(&self, path: &str) -> Option<&str> {
        self.names.get(&self.key(path)).map(|(spelled, _)| spelled.as_str())
    }

    /// `path` as the disk compares it.
    pub fn key(&self, path: &str) -> String {
        match self.rules.fold {
            true => fold(path),
            false => path.to_owned(), // alloc: once a name compared
        }
    }

    /// `name`, an entry's path in the archive, as a path under the root: its top directory
    /// taken off. None for the top directory itself.
    pub fn under_top(&mut self, name: &[u8]) -> Result<Option<String>, UnpackRefused> {
        let shown = || String::from_utf8_lossy(name).into_owned();
        if name.len() > self.rules.most_path {
            return Err(at(
                &shown(),
                format!("is {} bytes, more than {}", name.len(), self.rules.most_path),
            ));
        }
        if name.contains(&b'\\') || name.contains(&0) {
            return Err(at(&shown(), "has a `\\` or a NUL in it"));
        }
        let text = std::str::from_utf8(name).map_err(|_| at(&shown(), "is not UTF-8"))?;
        let mut parts: Vec<&str> = Vec::new(); // alloc: an entry's parts, once an entry
        for part in Path::new(text).components() {
            match part {
                Component::Normal(it) => parts.push(it.to_str().unwrap_or_default()),
                Component::CurDir => {}
                Component::ParentDir => return Err(at(text, "has a `..` in it")),
                Component::RootDir | Component::Prefix(_) => return Err(at(text, "is absolute")),
            }
        }
        let Some((first, rest)) = parts.split_first() else {
            return Err(at(text, "names nothing"));
        };
        let expected: Option<&str> = self.rules.top.or(self.top.as_deref());
        match expected {
            Some(top) if top != *first => {
                return Err(at(
                    text,
                    format!("is not under `{top}`, the top directory of the archive"),
                ));
            }
            Some(_) => {}
            None => self.top = Some((*first).to_owned()), // alloc: once an archive
        }
        let Some(head) = rest.first() else { return Ok(None) };
        if let Some(reserved) = self.rules.reserved.iter().find(|it| fold(it) == fold(head)) {
            return Err(at(
                text,
                format!("is named as `{reserved}`, which the archive may not hold"),
            ));
        }
        Ok(Some(rest.join("/")))
    }

    /// `path` taken as a name the archive makes, a directory when `dir`, and every directory
    /// it is under: whether it replaces a file listed before ([`Twice::Replace`]).
    pub fn claim(&mut self, path: &str, dir: bool) -> Result<bool, UnpackRefused> {
        for (end, _) in path.match_indices('/') {
            self.name(&path[..end], true)?;
        }
        self.name(path, dir)
    }

    fn name(&mut self, spelled: &str, dir: bool) -> Result<bool, UnpackRefused> {
        let key = self.key(spelled);
        let Some((was, was_dir)) = self.names.get(&key) else {
            self.names.insert(key, (spelled.to_owned(), dir)); // alloc: once a name
            return Ok(false);
        };
        if was != spelled {
            return Err(at(
                spelled,
                format!(
                    "and `{was}` are one name to a case-insensitive disk, and the archive has both"
                ),
            ));
        }
        match (*was_dir, dir) {
            (true, true) => Ok(false),
            (false, false) if self.rules.twice == Twice::Replace => Ok(true),
            (false, false) => Err(at(spelled, "is in the archive twice")),
            _ => Err(at(
                spelled,
                "is in the archive as a directory and as a file or link (something is under a file or link)",
            )),
        }
    }
}

/// Whether an entry is a directory: its type says so, or, as tar reads an old (pre-ustar) or
/// GNU header, its name ends in `/` and it is no link.
pub fn is_dir_entry(header: &tar::Header, name: &[u8]) -> bool {
    let kind = header.entry_type();
    kind.is_dir()
        || (!kind.is_hard_link()
            && !kind.is_symlink()
            && header.as_ustar().is_none()
            && name.ends_with(b"/"))
}

impl State<'_, '_> {
    /// Each directory of `dir` (a path under the root, or empty for the root) made, or checked
    /// to be one: never a link, as none is made before every directory is.
    fn make_dirs(&mut self, dir: &str) -> Result<(), UnpackRefused> {
        if dir.is_empty() {
            return Ok(());
        }
        for end in dir.match_indices('/').map(|(end, _)| end).chain([dir.len()]) {
            let part = &dir[..end];
            if self.dirs.contains(part) {
                continue;
            }
            let path = self.into.join(part);
            match fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => {
                    return Err(at(
                        part,
                        "has something not a directory in its place, and nothing is written through it",
                    ));
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    fs::create_dir(&path).map_err(|e| at(part, format!("can't be made: {e}")))?;
                }
                Err(e) => return Err(at(part, format!("can't be read: {e}"))),
            }
            self.dirs.insert(part.to_owned()); // alloc: once a directory
        }
        Ok(())
    }

    fn parent(path: &str) -> &str {
        path.rsplit_once('/').map_or("", |(parent, _)| parent)
    }

    fn entry<R: Read>(&mut self, mut entry: tar::Entry<'_, R>) -> Result<(), UnpackRefused> {
        let kind = entry.header().entry_type();
        if kind.is_pax_global_extensions() {
            return Ok(());
        }
        let is_dir = is_dir_entry(entry.header(), &entry.path_bytes());
        self.entries += 1;
        if self.entries > self.rules.most_entries {
            return Err(whole(format!(
                "the archive has more than {} entries",
                self.rules.most_entries
            )));
        }
        let name = entry.path_bytes().into_owned(); // alloc: an entry's name, once an entry
        let Some(path) = self.names.under_top(&name)? else {
            if is_dir {
                return Ok(());
            }
            return Err(at(
                &String::from_utf8_lossy(&name),
                "is the top of the archive, and no directory",
            ));
        };
        if !(self.rules.include)(Path::new(&path)) {
            return Ok(());
        }
        if path.rsplit('/').next().is_some_and(|it| self.rules.skip.contains(&it)) {
            return Ok(());
        }
        let header_mode =
            entry.header().mode().map_err(|e| at(&path, format!("has no mode: {e}")))?;
        if is_dir {
            self.names.claim(&path, true)?;
            self.make_dirs(&path)?;
            if let Modes::Cargo { .. } = self.rules.modes {
                self.modes.push((path, header_mode));
            }
            return Ok(());
        }
        let is_file = kind.is_file() || kind == tar::EntryType::Continuous;
        if !is_file && !(self.rules.links && (kind.is_hard_link() || kind.is_symlink())) {
            return Err(at(&path, format!("is a {kind:?} entry, which this archive may not hold")));
        }
        let replaces = self.names.claim(&path, false)?;
        self.make_dirs(Self::parent(&path))?;
        let dest = self.into.join(&path);
        if replaces {
            fs::remove_file(&dest).map_err(|e| {
                at(&path, format!("is listed again, and the first can't be removed: {e}"))
            })?;
            self.out.remove(&path);
        }
        if is_file {
            let size = entry.size();
            self.add_bytes(size)?;
            let exec = header_mode & 0o111 != 0;
            let mtime = entry.header().mtime().ok();
            let (blake3, len) = self
                .write_new(&dest, &mut (&mut entry).take(size), exec, header_mode, mtime)
                .map_err(|e| at(&path, e))?;
            if len != size {
                return Err(at(&path, format!("ends after {len} of its {size} bytes")));
            }
            let kind = if exec { Kind::Exec } else { Kind::File };
            self.out.insert(path.clone(), Entry { path, kind, blake3, len }); // clone: its key and its entry
        } else if kind.is_hard_link() {
            let target =
                entry.link_name_bytes().ok_or_else(|| at(&path, "is a hard link to nothing"))?;
            let Some(from) = self.names.under_top(&target)? else {
                return Err(at(&path, "is a hard link to the top directory"));
            };
            let source = self.names.spelled(&from).and_then(|it| self.out.get(it));
            let Some(source) = source.filter(|it| it.kind != Kind::Link) else {
                return Err(at(
                    &path,
                    format!(
                        "is a hard link to `{from}`, which is no file the archive wrote before it"
                    ),
                ));
            };
            let (len, exec, source) =
                (source.len, source.kind == Kind::Exec, self.into.join(&source.path));
            self.add_bytes(len)?;
            let mut file = File::open(&source)
                .map_err(|e| at(&path, format!("can't read its source: {e}")))?;
            let (blake3, copied) = self
                .write_new(&dest, &mut file, exec, header_mode, None)
                .map_err(|e| at(&path, e))?;
            if copied != len {
                return Err(at(&path, format!("copied {copied} of its source's {len} bytes")));
            }
            let kind = if exec { Kind::Exec } else { Kind::File };
            self.out.insert(path.clone(), Entry { path, kind, blake3, len }); // clone: its key and its entry
        } else {
            let target =
                entry.link_name_bytes().ok_or_else(|| at(&path, "is a link to nothing"))?;
            let target = String::from_utf8(target.into_owned())
                .map_err(|_| at(&path, "is a link whose target is not UTF-8"))?;
            self.link_keys.insert(self.names.key(&path));
            self.links.push((path, target));
        }
        Ok(())
    }

    fn add_bytes(&mut self, len: u64) -> Result<(), UnpackRefused> {
        self.bytes = self.bytes.saturating_add(len);
        match self.bytes > self.rules.most_bytes {
            true => Err(whole(format!(
                "the archive's files are more than {} bytes",
                self.rules.most_bytes
            ))),
            false => Ok(()),
        }
    }

    /// `from` written to `dest`, a new file, hashed as it goes; its mode (and time) set as
    /// [`Rules::modes`] says, and synced when [`Rules::sync`]: its BLAKE3 and length.
    fn write_new(
        &mut self,
        dest: &Path,
        from: &mut impl Read,
        exec: bool,
        mode: u32,
        mtime: Option<u64>,
    ) -> Result<(Blake3, u64), String> {
        let open = || OpenOptions::new().write(true).create_new(true).open(dest);
        let mut out = match open() {
            // Another spelling of one name on a case-insensitive disk (`README` and `readme` on
            // APFS), which names alone don't fold under these rules: the later replaces it, as
            // tar writes it.
            Err(e)
                if e.kind() == io::ErrorKind::AlreadyExists
                    && self.rules.names.twice == Twice::Replace =>
            {
                fs::remove_file(dest).and_then(|()| open()).map_err(|e| {
                    format!("is another spelling of a file before it, which can't be replaced: {e}")
                })?
            }
            opened => opened.map_err(|e| format!("can't be made: {e}"))?,
        };
        let mut hasher = blaze_ids::b3::Hasher::new();
        let mut len = 0u64;
        loop {
            let read = match from.read(&mut self.buffer) {
                Ok(0) => break,
                Ok(it) => it,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(format!("can't be read from the archive: {e}")),
            };
            hasher.update(&self.buffer[..read]);
            out.write_all(&self.buffer[..read]).map_err(|e| format!("can't be written: {e}"))?;
            len += read as u64;
        }
        match self.rules.modes {
            Modes::ReadOnly => {
                out.set_permissions(read_only(&out, exec).map_err(|e| format!("has no mode: {e}"))?)
            }
            Modes::Cargo { mask } => {
                if let Some(mtime) = mtime {
                    let time = filetime::FileTime::from_unix_time(
                        i64::try_from(mtime.max(1)).unwrap_or(i64::MAX),
                        0,
                    );
                    filetime::set_file_handle_times(&out, Some(time), Some(time))
                        .map_err(|e| format!("can't be given its time: {e}"))?;
                }
                masked(&out, mode, mask)
            }
        }
        .map_err(|e| format!("can't be given its mode: {e}"))?;
        if self.rules.sync {
            // durable: only where the caller's rules ask (`rules.sync`): an unpacked tree whose mark will say it is whole
            out.sync_all().map_err(|e| format!("can't be synced: {e}"))?;
        }
        Ok((Blake3(hasher.finalize()), len))
    }

    /// Where `target`, the link's at `path`, lands under the root when resolved from its
    /// directory by its text, as a path under the root ("" for the root): none when it climbs
    /// out, or passes through another link of the archive on the way (names compared as
    /// [`Names::key`] does), as a link through a link could climb out by a `..` the first hides.
    fn stays_in(&self, path: &str, target: &str) -> Option<String> {
        if target.is_empty()
            || target.len() > self.rules.names.most_path
            || target.contains(['\\', '\0'])
            || target.starts_with('/')
        {
            return None;
        }
        let mut now: Vec<&str> = path.split('/').collect(); // alloc: once a link
        now.pop();
        let parts: Vec<&str> =
            target.split('/').filter(|it| !it.is_empty() && *it != ".").collect(); // alloc: as above
        for (i, part) in parts.iter().enumerate() {
            if *part == ".." {
                now.pop()?;
                continue;
            }
            now.push(part);
            if i + 1 < parts.len() && self.link_keys.contains(&self.names.key(&now.join("/"))) {
                return None;
            }
        }
        Some(now.join("/"))
    }

    fn finish(mut self) -> Result<Unpacked, UnpackRefused> {
        let links = std::mem::take(&mut self.links);
        let mut lands: Vec<String> = Vec::with_capacity(links.len()); // alloc: once an archive
        for (path, target) in &links {
            let Some(land) = self.stays_in(path, target) else {
                return Err(at(
                    path,
                    format!(
                        "is a link to `{target}`, which is not inside the archive or goes through another link"
                    ),
                ));
            };
            symlink(target, &self.into.join(path))
                .map_err(|e| at(path, format!("can't be linked to `{target}`: {e}")))?;
            lands.push(land);
        }
        // Each link resolved by the system, which must land where its text says: the root
        // joined with that, or, when that is itself a link of the archive (checked as its own),
        // somewhere under the root.
        if !links.is_empty() {
            let root = fs::canonicalize(self.into)
                .map_err(|e| whole(format!("resolving {}: {e}", self.into.display())))?;
            for ((path, target), land) in links.iter().zip(&lands) {
                let to_link = self.link_keys.contains(&self.names.key(land));
                match fs::canonicalize(self.into.join(path)) {
                    Ok(real) if real == root.join(land) => {}
                    Ok(real) if to_link && real.starts_with(&root) => {}
                    Ok(real) => {
                        return Err(at(
                            path,
                            format!(
                                "is a link to `{target}`, which the disk resolves to {}, not `{land}` under the archive",
                                real.display()
                            ),
                        ));
                    }
                    Err(e) => {
                        return Err(at(
                            path,
                            format!(
                                "is a link to `{target}`, which resolves to nothing in the archive: {e}"
                            ),
                        ));
                    }
                }
                let blake3 = Blake3::of(target.as_bytes());
                let entry = Entry {
                    path: path.clone(),
                    kind: Kind::Link,
                    blake3,
                    len: target.len() as u64,
                }; // clone: its entry
                self.out.insert(path.clone(), entry); // clone: its key
            }
        }
        let mut dirs: Vec<&String> = self.dirs.iter().collect(); // alloc: once an archive
        // Deepest first, so each is synced and sealed while its parent still can be written.
        dirs.sort_by_key(|it| std::cmp::Reverse(it.matches('/').count()));
        for dir in dirs {
            let path = self.into.join(dir);
            if self.rules.sync {
                sync_dir(&path).map_err(|e| at(dir, format!("can't be synced: {e}")))?;
            }
            if self.rules.modes == Modes::ReadOnly {
                seal_dir(&path).map_err(|e| at(dir, format!("can't be made read-only: {e}")))?;
            }
        }
        if let Modes::Cargo { mask } = self.rules.modes {
            for (dir, mode) in &self.modes {
                set_dir_mode(&self.into.join(dir), *mode, mask)
                    .map_err(|e| at(dir, format!("can't be given its mode: {e}")))?;
            }
        }
        if self.rules.sync {
            sync_dir(self.into)
                .map_err(|e| whole(format!("syncing {}: {e}", self.into.display())))?;
        }
        Ok(Unpacked { entries: self.out.into_values().collect(), bytes: self.bytes })
    }
}

/// Every file and link under `root`, not following links, as [`unpack`] lists them, but for the
/// top-level names in `skip`; sorted by path. With `read_only`, a file anyone may write is
/// refused, naming it. Each file is read a chunk at a time.
pub fn walk(root: &Path, skip: &[&str], read_only: bool) -> Result<Vec<Entry>, UnpackRefused> {
    let mut out: Vec<Entry> = Vec::new(); // alloc: once a walk
    let mut buffer = vec![0u8; 1 << 16]; // alloc: one read buffer, once a walk
    let mut dirs: Vec<String> = vec![String::new()]; // alloc: once a walk
    while let Some(dir) = dirs.pop() {
        let path = if dir.is_empty() { root.to_owned() } else { root.join(&dir) };
        let fail = |e: io::Error| at(&dir, format!("can't be read: {e}"));
        for item in fs::read_dir(&path).map_err(fail)? {
            let item = item.map_err(fail)?;
            let name = item.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| at(&dir, format!("holds {name:?}, which is not UTF-8")))?;
            if dir.is_empty() && skip.contains(&name) {
                continue;
            }
            let rel = if dir.is_empty() { name.to_owned() } else { format!("{dir}/{name}") }; // alloc: once an entry
            let meta = fs::symlink_metadata(item.path())
                .map_err(|e| at(&rel, format!("can't be read: {e}")))?;
            if meta.is_dir() {
                dirs.push(rel);
            } else if meta.file_type().is_symlink() {
                let target = fs::read_link(item.path())
                    .map_err(|e| at(&rel, format!("can't be read: {e}")))?;
                let target = target
                    .to_str()
                    .ok_or_else(|| at(&rel, "is a link whose target is not UTF-8"))?
                    .replace('\\', "/");
                out.push(Entry {
                    path: rel,
                    kind: Kind::Link,
                    blake3: Blake3::of(target.as_bytes()),
                    len: target.len() as u64,
                });
            } else if meta.is_file() {
                if read_only && writable(&meta) {
                    return Err(at(&rel, "can be written, and was made read-only"));
                }
                let mut file =
                    File::open(item.path()).map_err(|e| at(&rel, format!("can't be read: {e}")))?;
                let mut hasher = blaze_ids::b3::Hasher::new();
                let mut len = 0u64;
                loop {
                    let read = match file.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(it) => it,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(at(&rel, format!("can't be read: {e}"))),
                    };
                    hasher.update(&buffer[..read]);
                    len += read as u64;
                }
                let kind = if executable(&meta) { Kind::Exec } else { Kind::File };
                out.push(Entry { path: rel, kind, blake3: Blake3(hasher.finalize()), len });
            } else {
                return Err(at(&rel, "is no file, directory or link"));
            }
        }
    }
    out.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// The first path `got` and `want` (each sorted by path) differ at, and how: none when they
/// are the same.
pub fn first_difference(got: &[Entry], want: &[Entry]) -> Option<(String, &'static str)> {
    let (mut g, mut w) = (got.iter().peekable(), want.iter().peekable());
    loop {
        match (g.peek(), w.peek()) {
            (None, None) => return None,
            (Some(it), None) => return Some((it.path.clone(), "is there, and was not unpacked")), // clone: an error's path
            (None, Some(it)) => return Some((it.path.clone(), "was unpacked, and is gone")), // clone: as above
            (Some(a), Some(b)) => match a.path.cmp(&b.path) {
                std::cmp::Ordering::Less => {
                    return Some((a.path.clone(), "is there, and was not unpacked"));
                } // clone: as above
                std::cmp::Ordering::Greater => {
                    return Some((b.path.clone(), "was unpacked, and is gone"));
                } // clone: as above
                std::cmp::Ordering::Equal if a.kind != b.kind => {
                    return Some((a.path.clone(), "is of another kind than was unpacked"));
                } // clone: as above
                std::cmp::Ordering::Equal if a != b => {
                    return Some((a.path.clone(), "holds other bytes than were unpacked"));
                } // clone: as above
                std::cmp::Ordering::Equal => {
                    g.next();
                    w.next();
                }
            },
        }
    }
}

#[cfg(unix)]
fn symlink(target: &str, at: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, at)
}

#[cfg(not(unix))]
fn symlink(target: &str, at: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!("no symbolic link is made here (to `{target}` at {})", at.display()),
    ))
}

fn read_only(file: &File, exec: bool) -> io::Result<fs::Permissions> {
    let mut perms = file.metadata()?.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(if exec { 0o555 } else { 0o444 });
    }
    #[cfg(not(unix))]
    {
        let _ = exec;
        perms.set_readonly(true);
    }
    Ok(perms)
}

#[cfg(unix)]
fn masked(file: &File, mode: u32, mask: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(mode & 0o777 & !mask))
}

#[cfg(not(unix))]
fn masked(_: &File, _: u32, _: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_dir_mode(dir: &Path, mode: u32, mask: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(mode & 0o777 & !mask))
}

#[cfg(not(unix))]
fn set_dir_mode(_: &Path, _: u32, _: u32) -> io::Result<()> {
    Ok(())
}

/// `dir` made read-only (and searchable).
pub fn seal_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o555))
    }
    #[cfg(not(unix))]
    {
        // A read-only directory means something else on Windows; its files are read-only.
        let _ = dir;
        Ok(())
    }
}

/// `dir`'s entries synced to the disk (a no-op where a directory can't be opened to sync).
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        // durable: `sync_dir` is asked by a caller that commits an unpacked tree by its name
        File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

#[cfg(unix)]
fn writable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o222 != 0
}

#[cfg(not(unix))]
fn writable(meta: &fs::Metadata) -> bool {
    !meta.permissions().readonly()
}

#[cfg(unix)]
fn executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_: &fs::Metadata) -> bool {
    false
}

/// `dir`, an unpacking that did not finish or a layout to remove, removed: each directory made
/// writable first, never following a link.
pub fn remove_all(dir: &Path) -> io::Result<()> {
    match fs::symlink_metadata(dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
        Ok(meta) if !meta.is_dir() => return fs::remove_file(dir),
        Ok(_) => {}
    }
    make_writable(dir)?;
    fs::remove_dir_all(dir)
}

fn make_writable(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o755))?;
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            make_writable(&entry.path())?;
        } else if !kind.is_symlink() && cfg!(not(unix)) {
            // Windows refuses to remove a read-only file.
            let mut perms = entry.metadata()?.permissions();
            #[allow(
                clippy::permissions_set_readonly_false,
                reason = "removing a tree blaze unpacked"
            )]
            perms.set_readonly(false);
            fs::set_permissions(entry.path(), perms)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tar entry whose name and link target are written as given, with no check: what a
    /// hostile archive holds, which `tar::Builder` would refuse to write.
    fn raw(kind: tar::EntryType, name: &str, link: &str, data: &[u8]) -> (tar::Header, Vec<u8>) {
        let mut header = tar::Header::new_old();
        header.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
        header.as_old_mut().linkname[..link.len()].copy_from_slice(link.as_bytes());
        header.set_entry_type(kind);
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        (header, data.to_vec())
    }

    fn tar(entries: &[(tar::Header, Vec<u8>)]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for (header, data) in entries {
            tar.append(header, data.as_slice()).unwrap();
        }
        tar.into_inner().unwrap()
    }

    fn every(_: &Path) -> bool {
        true
    }

    fn layout_rules() -> Rules<'static> {
        Rules {
            names: NameRules {
                top: None,
                fold: true,
                twice: Twice::Refuse,
                reserved: &[".blaze-native"],
                most_path: 4096,
            },
            links: true,
            modes: Modes::ReadOnly,
            skip: &[],
            include: &every,
            sync: false,
            most_entries: 1 << 20,
            most_bytes: 1 << 30,
        }
    }

    /// An entry that climbs out with `..`, one that is absolute, a link out of the root
    /// (directly, through another link, or through a link spelled in another case, as an
    /// APFS disk finds it), an entry under a link, an entry named as the caller's mark in
    /// another case, and two names one under full case folding (`ſ` and `s`, `ß` and `ss`, `ﬀ`
    /// and `ff`) are each refused, and nothing is written outside.
    #[cfg(unix)] // Its well-formed archive has a link, which only Unix lays out.
    #[test]
    fn unpack_refuses_paths_out_of_the_root() {
        use tar::EntryType::{Directory, Regular, Symlink};
        let ok = || raw(Regular, "top/bin/tool", "", b"ok");
        let hostile: [(&str, Vec<(tar::Header, Vec<u8>)>); 11] = [
            (
                "`..`",
                vec![raw(Directory, "top/", "", b""), raw(Regular, "top/../escaped", "", b"x")],
            ),
            ("absolute", vec![raw(Regular, "/tmp/escaped", "", b"x")]),
            ("link out", vec![ok(), raw(Symlink, "top/lib/out", "../../..", b"")]),
            (
                "link through a link",
                vec![
                    ok(),
                    raw(Symlink, "top/a/up", "..", b""),
                    raw(Symlink, "top/b", "a/up/..", b""),
                ],
            ),
            (
                "link through a link in another case",
                vec![
                    ok(),
                    raw(Symlink, "top/d/up", "..", b""),
                    raw(Symlink, "top/A", "D/up/D/up/../../../escaped", b""),
                ],
            ),
            (
                "under a link",
                vec![raw(Symlink, "top/l", ".", b""), raw(Regular, "top/l/x", "", b"x")],
            ),
            (
                "one name in two cases",
                vec![raw(Regular, "top/d/x", "", b"x"), raw(Symlink, "top/D", "..", b"")],
            ),
            (
                "the mark in another case",
                vec![ok(), raw(Symlink, "top/.Blaze-Native", "bin/tool", b"")],
            ),
            (
                "`ſ` as `s`",
                vec![raw(Regular, "top/s/x", "", b"x"), raw(Symlink, "top/\u{17f}", "..", b"")],
            ),
            (
                "`ß` as `ss`",
                vec![raw(Regular, "top/ss", "", b"x"), raw(Regular, "top/\u{df}", "", b"y")],
            ),
            (
                "`ﬀ` as `ff`",
                vec![raw(Regular, "top/ff", "", b"x"), raw(Regular, "top/\u{fb00}", "", b"y")],
            ),
        ];
        for (what, entries) in hostile {
            let scratch = tempfile::tempdir().unwrap();
            let into = scratch.path().join("layout");
            fs::create_dir(&into).unwrap();
            let refused = unpack(tar(&entries).as_slice(), &into, &layout_rules());
            assert!(refused.is_err(), "{what}: unpacked");
            assert!(!scratch.path().join("escaped").exists(), "{what}: wrote outside");
            remove_all(&into).unwrap();
        }
        // The same shapes, kept inside, unpack, and walk back to what was unpacked.
        let scratch = tempfile::tempdir().unwrap();
        let into = scratch.path().join("layout");
        fs::create_dir(&into).unwrap();
        let fine = [ok(), raw(Symlink, "top/bin/alias", "tool", b"")];
        let unpacked = unpack(tar(&fine).as_slice(), &into, &layout_rules()).unwrap();
        assert_eq!(fs::read(into.join("bin/alias")).unwrap(), b"ok");
        assert_eq!(first_difference(&walk(&into, &[], true).unwrap(), &unpacked.entries), None);
        remove_all(&into).unwrap();
    }
}
