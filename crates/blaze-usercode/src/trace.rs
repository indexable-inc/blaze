//! What a sandbox saw a program read, from the line it notes as each read happens to the
//! [`Traced`] its result is keyed by (docs/v2/plan/lanes/usercode/notes.md, "The record").
//!
//! A sandbox appends one [`Line`] per thing it saw, as it sees it, to the run's live record:
//! a warm process (a proc-macro server, a fork server) serves many requests, each bracketed by
//! the sandbox's marker, and its record is read after each answer, so a line is written before
//! the call it notes goes on. [`assemble`] turns the lines of one scope into a [`Traced`],
//! hashing what was read as it is when the run ends; [`encode`] and [`decode`] are the report
//! the sandbox's launcher leaves for the driver that ran it.

use std::{
    borrow::Cow,
    ffi::{OsStr, OsString},
    fmt,
    path::{Path, PathBuf},
};

use blaze_core::{Traced, Untraceable};
use blaze_format::cursor::{Put, Reader};
use blaze_ids::{ContentId, Hasher};
use blaze_mem::{Frame, Tag, frame, scratch};

/// The hash domain `$domain` of something [`os_bytes`] encodes names into: as it is where a
/// name is its bytes, and another on Windows, where a name is its UTF-16 units, so what an
/// older blaze keyed by other bytes there misses rather than being read as these.
macro_rules! os_domain {
    ($domain:literal) => {
        if cfg!(windows) { concat!($domain, " utf-16le") } else { $domain }
    };
}
pub(crate) use os_domain;

const LISTING_DOMAIN: &str = os_domain!("blaze v2 traced listing 1");

/// What a line says was seen.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Seen {
    /// `R`: a path opened to read, or run.
    Read,
    /// `L`: a path only looked at (`stat`, `access`, `readlink`): what it is, but not what is
    /// in it. Traced as a read: a superset of what it showed.
    Looked,
    /// `D`: a directory listed.
    Listed,
    /// `E`: an environment variable read by name, or `*` for the whole environment (a
    /// process that walked `environ`, or one whose reads by name can't be seen).
    Env,
    /// `T`: something that can't be traced: `clock`, `random`, `network` or `descriptor`.
    Untraceable,
    /// `W`: a write the sandbox refused. Not an input: the refusal is the same every run.
    RefusedWrite,
    /// `N`: an address the sandbox refused. Not an input, as a refused write is not.
    RefusedNet,
    /// `X`: the record is not whole, and why: what was read is not all known.
    Incomplete,
}

impl Seen {
    pub fn letter(self) -> char {
        match self {
            Seen::Read => 'R',
            Seen::Looked => 'L',
            Seen::Listed => 'D',
            Seen::Env => 'E',
            Seen::Untraceable => 'T',
            Seen::RefusedWrite => 'W',
            Seen::RefusedNet => 'N',
            Seen::Incomplete => 'X',
        }
    }

    pub fn of(letter: &str) -> Option<Seen> {
        Some(match letter {
            "R" => Seen::Read,
            "L" => Seen::Looked,
            "D" => Seen::Listed,
            "E" => Seen::Env,
            "T" => Seen::Untraceable,
            "W" => Seen::RefusedWrite,
            "N" => Seen::RefusedNet,
            "X" => Seen::Incomplete,
            _ => return None,
        })
    }
}

/// The name of an [`Untraceable`] in a `T` line.
pub fn untraceable_name(it: Untraceable) -> &'static str {
    match it {
        Untraceable::Clock => "clock",
        Untraceable::Random => "random",
        Untraceable::Network => "network",
        Untraceable::Descriptor => "descriptor",
    }
}

pub fn untraceable_of(name: &str) -> Option<Untraceable> {
    Some(match name {
        "clock" => Untraceable::Clock,
        "random" => Untraceable::Random,
        "network" => Untraceable::Network,
        "descriptor" => Untraceable::Descriptor,
        _ => return None,
    })
}

/// What an `E` line names for the whole environment.
pub const ALL_ENV: &str = "*";

/// The name a [`Traced`] of a run that read the whole environment has, beside every variable,
/// with the [`listing`] hash of the names (in hex) as its value: a variable added later is
/// one it would have seen. No variable has this name: a name is a C string.
pub const ALL_ENV_NAME: &str = "\0*";

/// One line of a record: the letter, the scope and what, by tabs, ended by a line break. The
/// scope is the name the sandbox's marker gave the thread or process that made the call (a
/// test, an expansion), or empty when none did: a line with an empty scope is every scope's.
/// What has a tab or a line break in it can't be written; the sandbox writes an `X` line for
/// it instead ([`Line::new`]).
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Line {
    pub seen: Seen,
    pub scope: String,
    pub what: String,
}

impl Line {
    /// The line, or an `X` line when `scope` or `what` can't be written in one.
    pub fn new(seen: Seen, scope: &str, what: &str) -> Line {
        let bad = |it: &str| it.contains(['\t', '\n']);
        // alloc: the edge, a line of the sandbox's record, which the line owns
        match (bad(scope), bad(what)) {
            // alloc: as above
            (false, false) => Line { seen, scope: scope.to_owned(), what: what.to_owned() },
            (true, _) => Line {
                seen: Seen::Incomplete,
                scope: String::new(), // alloc: none, empty
                what: "a scope with a tab or a line break in it".to_owned(), // alloc: a line that is an error
            },
            (false, true) => Line {
                seen: Seen::Incomplete,
                scope: scope.to_owned(), // alloc: a line that is an error
                what: "a path or a name with a tab or a line break in it".to_owned(), // alloc: as above
            },
        }
    }

    /// The line as the record has it, with its line break.
    pub fn text(&self) -> String {
        format!("{}\t{}\t{}\n", self.seen.letter(), self.scope, self.what) // alloc: the edge, the record written
    }

    /// The line `text` is, without its line break.
    pub fn parse(text: &str) -> Option<Line> {
        let mut parts = text.splitn(3, '\t');
        let seen = Seen::of(parts.next()?)?;
        let scope = parts.next()?.to_owned(); // alloc: the edge, a line of the sandbox's record
        let what = parts.next()?.to_owned(); // alloc: as above
        Some(Line { seen, scope, what })
    }
}

/// The lines of a record's text. A last line with no line break is one the sandbox was
/// writing as it was read, and is left for the next read; any other line that is not a line
/// is an error: the record is not whole.
pub fn parse(text: &str) -> Result<(Vec<Line>, usize), BadRecord> {
    let mut lines = Vec::new(); // alloc: the edge, the lines the caller keeps
    let mut used = 0;
    for (number, piece) in text.split_inclusive('\n').enumerate() {
        let Some(line) = piece.strip_suffix('\n') else { break };
        lines.push(Line::parse(line).ok_or_else(|| BadRecord {
            line: number + 1,
            text: line.chars().take(200).collect(), // alloc: error path
        })?);
        used += piece.len();
    }
    Ok((lines, used))
}

/// A record with a line that is not one.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BadRecord {
    pub line: usize,
    pub text: String,
}

impl fmt::Display for BadRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {} of the sandbox's record is not a record's line: {:?}", self.line, self.text)
    }
}

/// What a read is checked against: the files, directories and environment as they are where
/// a result is to be used (a view and the machine, [`crate::inputs`]) or as they were when a
/// run ended (the machine, [`crate::inputs::Disk`]).
pub trait Inputs {
    type Error;
    /// The BLAKE3 hash of the file's bytes, [`DIRECTORY`] for a directory, `None` for nothing
    /// there. A file that is there and can't be read is an error, never `None` (#1480).
    fn file(&mut self, path: &Path) -> Result<Option<[u8; 32]>, Self::Error>;
    /// The [`listing`] hash of the directory's names, `None` for no directory there.
    fn dir(&mut self, path: &Path) -> Result<Option<[u8; 32]>, Self::Error>;
    /// The variable's value, lent: a check hashes it where it is.
    fn env(&mut self, name: &OsStr) -> Option<&OsStr>;
    /// The whole environment, for a run that read all of it: sorted by name, each name once,
    /// lent rather than copied, as [`crate::inputs::Env`] keeps it.
    fn env_all(&mut self) -> &[(OsString, OsString)];
    /// The names in the directory, each with whether it is a directory, pushed to `out` (given
    /// empty) and sorted ([`Names::sort`]); `false` for no directory there. What [`walk`]
    /// lists.
    fn names(&mut self, path: &Path, out: &mut Names<'_>) -> Result<bool, Self::Error>;
    /// What `path` is as a view holds it, a link not followed: what [`walk`] hashes. A path
    /// whose last name is not UTF-8 is an error, as a view has no such name.
    fn entry(&mut self, path: &Path) -> Result<Entry, Self::Error>;
    /// [`walk`]'s hash of everything below `dir`; a view's own, kept per directory, where the
    /// inputs have one.
    fn tree(&mut self, dir: &Path, package: bool) -> Result<Option<[u8; 32]>, Self::Error> {
        walk(self, dir, package)
    }
}

/// A path as a view holds it ([`Inputs::entry`]): a directory, or a file by its view content
/// ID ([`blaze_vfs::FILE_DOMAIN`], of its bytes; a link's are its target) and git mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Entry {
    Dir,
    File(ContentId, blaze_vfs::Mode),
    Missing,
}

/// A directory's names as [`Inputs::names`] gives them, in a task's frame, each with whether it
/// is a directory: on Unix a name is its bytes, all of them packed in one buffer; elsewhere an
/// `OsString` each, as [`os_in`] has them there.
pub struct Names<'f> {
    #[cfg(unix)]
    bytes: Vec<u8, &'f Frame>,
    /// Each name's `start..end` in `bytes`, and whether it is a directory.
    #[cfg(unix)]
    spans: Vec<(usize, usize, bool), &'f Frame>,
    #[cfg(not(unix))]
    names: Vec<(OsString, bool), &'f Frame>,
}

impl<'f> Names<'f> {
    pub fn new_in(frame: &'f Frame) -> Names<'f> {
        #[cfg(unix)]
        let names = Names { bytes: Vec::new_in(frame), spans: Vec::new_in(frame) };
        #[cfg(not(unix))]
        let names = Names { names: Vec::new_in(frame) };
        names
    }

    pub fn push(&mut self, name: &OsStr, dir: bool) {
        #[cfg(unix)]
        {
            let start = self.bytes.len();
            self.bytes.extend_from_slice(name.as_encoded_bytes());
            self.spans.push((start, self.bytes.len(), dir));
        }
        #[cfg(not(unix))]
        self.names.push((name.to_owned(), dir)); // alloc: a name off Unix, where no safe view of packed bytes gives it back.
    }

    /// In the order of the names' bytes, which is [`OsStr`]'s.
    pub fn sort(&mut self) {
        #[cfg(unix)]
        {
            let bytes = &self.bytes;
            self.spans.sort_unstable_by(|a, b| bytes[a.0..a.1].cmp(&bytes[b.0..b.1]));
        }
        #[cfg(not(unix))]
        self.names.sort_unstable();
    }

    pub fn len(&self) -> usize {
        #[cfg(unix)]
        let len = self.spans.len();
        #[cfg(not(unix))]
        let len = self.names.len();
        len
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, at: usize) -> (&OsStr, bool) {
        #[cfg(unix)]
        let got = {
            let (start, end, dir) = self.spans[at];
            (std::os::unix::ffi::OsStrExt::from_bytes(&self.bytes[start..end]), dir)
        };
        #[cfg(not(unix))]
        let got = {
            let (name, dir) = &self.names[at];
            (name.as_os_str(), *dir)
        };
        got
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&OsStr, bool)> + '_ {
        (0..self.len()).map(|at| self.get(at))
    }
}

/// What a [`Traced`] names a package's whole tree by, below the package's directory: the
/// input of cargo's rule for a script that printed no `rerun-if-changed` ([`crate::rule`]).
/// No file has this name: a name is a C string.
pub const PACKAGE: &str = "\0package";

/// What a [`Traced`] names everything below a directory by: the input of cargo's rule for a
/// `rerun-if-changed` of a directory, which cargo watches whole.
pub const TREE: &str = "\0tree";

/// The directory `path` is the [`PACKAGE`] or [`TREE`] entry of, and whether it is a
/// package, when it is one.
pub fn walked(path: &Path) -> Option<(&Path, bool)> {
    let name = path.file_name()?;
    let package = if name == PACKAGE {
        true
    } else if name == TREE {
        false
    } else {
        return None;
    };
    Some((path.parent()?, package))
}

/// The directory `path` is the [`PACKAGE`] entry of, when it is one.
pub fn package_of(path: &Path) -> Option<&Path> {
    walked(path).filter(|it| it.1).map(|it| it.0)
}

/// The hash of every file of the package at `dir` as cargo watches it: every file below it,
/// by its path from `dir` and its bytes, but what is in a directory whose name starts with a
/// `.` and the package's own `target`. `None` for no directory there.
pub fn package<I: Inputs + ?Sized>(inputs: &mut I, dir: &Path) -> Result<Option<[u8; 32]>, I::Error> {
    inputs.tree(dir, true)
}

/// The hash of every file below `dir`, as [`package`] hashes them; with `package`, a
/// package's, without its hidden names and its `target`. The view's own hash of the same
/// tree ([`blaze_query::Cx::tree`]) by the same type ([`blaze_query::TreeHash`]), one
/// directory at a time, so a tree hashed off the disk and one in a view agree.
pub fn walk<I: Inputs + ?Sized>(
    inputs: &mut I,
    dir: &Path,
    package: bool,
) -> Result<Option<[u8; 32]>, I::Error> {
    frame::task(Tag::Usercode, |frame| {
        scratch::with_path(|whole| {
            whole.push(dir);
            walk_in(inputs, whole, (package, true), frame)
        })
    })
}

/// One directory of [`walk`] at `whole`, `top` for the one asked for. `whole` is the one path
/// buffer of the walk, pushed and popped, never joined anew; every listing is in the walk's
/// frame, freed when the walk ends.
fn walk_in<I: Inputs + ?Sized>(
    inputs: &mut I,
    whole: &mut PathBuf,
    (package, top): (bool, bool),
    frame: &Frame,
) -> Result<Option<[u8; 32]>, I::Error> {
    let mut names = Names::new_in(frame);
    if !inputs.names(whole, &mut names)? {
        return Ok(None);
    }
    names.sort();
    let mut hash = blaze_query::TreeHash::new(package, top);
    for (name, _) in names.iter() {
        whole.push(name);
        let Some(text) = name.to_str() else {
            inputs.entry(whole)?;
            panic!("{} is not UTF-8, which `Inputs::entry` refuses", whole.display());
        };
        if hash.keeps(text) {
            match inputs.entry(whole)? {
                Entry::Dir => {
                    let below = walk_in(inputs, whole, (package, false), frame)?;
                    hash.dir(text, below);
                }
                Entry::File(id, mode) => hash.file(text, Some((id, mode))),
                Entry::Missing => hash.file(text, None),
            }
        }
        whole.pop();
    }
    Ok(Some(hash.finish()))
}

/// What a traced file that is a directory hashes as: what a look at it showed is that it is
/// one, and a listing of it is a [`Seen::Listed`] of its own.
pub const DIRECTORY: [u8; 32] = *b"blaze v2 traced: a directory\0\0\0\0";

/// The hash of a directory's listing: its names, sorted by their bytes, each framed.
pub fn listing<'a>(names: impl IntoIterator<Item = &'a OsStr>) -> [u8; 32] {
    frame::task(Tag::Usercode, |frame| {
        let mut sorted: Vec<Cow<'_, [u8]>, &Frame> = Vec::new_in(frame);
        sorted.extend(names.into_iter().map(os_bytes));
        sorted.sort_unstable();
        sorted.dedup();
        let mut hasher = Hasher::new(LISTING_DOMAIN);
        for name in sorted.iter() {
            hasher.part(name);
        }
        hasher.finish().0
    })
}

/// The lines of `scope` (all of them for `None`), with what they read as `inputs` has it now:
/// sorted and each once, so equal reads give equal [`Traced`]s whatever order they came in.
pub fn assemble<'l, I: Inputs>(
    lines: impl IntoIterator<Item = &'l Line>,
    scope: Option<&str>,
    inputs: &mut I,
) -> Result<Traced, I::Error> {
    let ours = |line: &&Line| match scope {
        None => true,
        Some(scope) => line.scope.is_empty() || line.scope == scope,
    };
    assemble_seen(lines.into_iter().filter(ours).map(|line| (line.seen, line.what.as_str())), inputs)
}

/// [`assemble`] of lines already of one scope, borrowed: each name is owned once, however
/// many lines name it.
pub fn assemble_seen<'a, I: Inputs>(
    lines: impl IntoIterator<Item = (Seen, &'a str)>,
    inputs: &mut I,
) -> Result<Traced, I::Error> {
    frame::task(Tag::Usercode, |frame| {
        let mut files: Vec<&str, &Frame> = Vec::new_in(frame);
        let mut dirs: Vec<&str, &Frame> = Vec::new_in(frame);
        let mut env: Vec<&str, &Frame> = Vec::new_in(frame);
        let mut all_env = false;
        let mut untraceable = Vec::new_in(frame);
        for (seen, what) in lines {
            match seen {
                Seen::Read | Seen::Looked => files.push(what),
                Seen::Listed => dirs.push(what),
                Seen::Env if what == ALL_ENV => all_env = true,
                Seen::Env => env.push(what),
                // A name this blaze doesn't know is something it can't trace either.
                Seen::Untraceable => untraceable.push(untraceable_of(what).unwrap_or(Untraceable::Descriptor)),
                Seen::Incomplete => untraceable.push(Untraceable::Descriptor),
                Seen::RefusedWrite | Seen::RefusedNet => {}
            }
        }
        // Each list sorted once here, as paths and names sort, each name once: `inputs` is
        // asked, and `traced` filled, in that order.
        for paths in [&mut files, &mut dirs] {
            paths.sort_unstable_by(|a, b| Path::new(a).cmp(Path::new(b)).then(a.cmp(b)));
            paths.dedup();
        }
        env.sort_unstable();
        env.dedup();
        untraceable.sort_unstable();
        untraceable.dedup();
        let mut traced = Traced::default();
        for &path in &files {
            let path = Path::new(path);
            let hash = inputs.file(path)?;
            traced.files.push((path.to_path_buf(), hash)); // alloc: the Traced the caller keeps
        }
        for &path in &dirs {
            let path = Path::new(path);
            match inputs.dir(path)? {
                Some(hash) => traced.dirs.push((path.to_path_buf(), hash)), // alloc: kept, as above
                // A listing of nothing is a look that found nothing.
                None if !traced.files.iter().any(|it| it.0 == path) => {
                    traced.files.push((path.to_path_buf(), None)); // alloc: kept, as above
                }
                None => {}
            }
        }
        traced.files.sort();
        // The whole environment, sorted and each name once, then what was asked by a name it
        // has not: each name once, as a run sees it.
        let mut whole = 0;
        if all_env {
            let all = inputs.env_all();
            traced.env.push((ALL_ENV_NAME.into(), Some(names_hash(all).into())));
            // clone: the Traced the caller keeps
            traced.env.extend(all.iter().map(|(name, value)| (name.clone(), Some(value.clone()))));
            whole = traced.env.len();
        }
        for &name in &env {
            let name = OsStr::new(name);
            let had = whole > 0
                && (name == ALL_ENV_NAME || traced.env[1..whole].binary_search_by(|it| it.0.as_os_str().cmp(name)).is_ok());
            if !had {
                let value = inputs.env(name).map(OsStr::to_owned);
                traced.env.push((name.to_owned(), value)); // alloc: kept, as above
            }
        }
        traced.env.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        traced.untraceable = untraceable.iter().copied().collect(); // alloc: kept, as above
        Ok(traced)
    })
}

/// The value [`ALL_ENV_NAME`] has for the environment `all`.
pub fn names_hash(all: &[(OsString, OsString)]) -> String {
    names_hex(all).iter().map(|&it| char::from(it)).collect() // alloc: the value the Traced keeps
}

/// [`names_hash`]'s bytes, made on the stack: what a cache check hashes into a key.
pub(crate) fn names_hex(all: &[(OsString, OsString)]) -> [u8; 64] {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    // An environment is sorted and each name once, so it is its own listing's order.
    let hash = match all.is_sorted_by(|a, b| os_bytes(&a.0) < os_bytes(&b.0)) {
        true => {
            let mut hasher = Hasher::new(LISTING_DOMAIN);
            for (name, _) in all {
                hasher.part(&os_bytes(name));
            }
            hasher.finish().0
        }
        false => listing(all.iter().map(|it| it.0.as_os_str())),
    };
    let mut hex = [0; 64];
    for (at, byte) in hash.iter().enumerate() {
        hex[2 * at] = DIGITS[usize::from(byte >> 4)];
        hex[2 * at + 1] = DIGITS[usize::from(byte & 15)];
    }
    hex
}

/// A run's result key, hashed as its inputs are read ([`crate::RunKey::traced`]): a
/// normalized [`Traced`]'s files, then its directories, its variables and what it could not
/// trace, in that order, each entry tagged and each name framed. A cache check hashes a
/// shape's inputs straight into one ([`crate::index::find`]) and builds no [`Traced`].
///
/// The entries are gathered into a buffer on the stack and given to BLAKE3 [`KEYER_BATCH`]
/// bytes at a time: given a piece of several chunks, it hashes them side by side in SIMD lanes,
/// where one tiny update per tag and name would compress one block at a time. The hash is the
/// same either way.
pub(crate) struct Keyer {
    hasher: Hasher,
    batch: [u8; KEYER_BATCH],
    len: usize,
}

/// Eight BLAKE3 chunks: as many as AVX2 hashes at once.
const KEYER_BATCH: usize = 8 * 1024;

impl Keyer {
    pub(crate) fn new(run: &crate::RunKey) -> Keyer {
        let mut hasher = Hasher::new(crate::RunKey::DOMAIN);
        hasher.id(&run.code).id(&run.given);
        Keyer { hasher, batch: [0; KEYER_BATCH], len: 0 }
    }

    fn put(&mut self, bytes: &[u8]) {
        if self.len + bytes.len() > KEYER_BATCH {
            self.hasher.update(&self.batch[..self.len]);
            self.len = 0;
            if bytes.len() > KEYER_BATCH {
                self.hasher.update(bytes);
                return;
            }
        }
        self.batch[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
    }

    /// As [`Hasher::part`]: length-prefixed.
    fn part(&mut self, bytes: &[u8]) {
        self.put(&(bytes.len() as u64).to_le_bytes());
        self.put(bytes);
    }

    pub(crate) fn file(&mut self, path: &[u8], hash: Option<&[u8; 32]>) {
        self.put(b"f");
        self.part(path);
        match hash {
            Some(hash) => {
                self.put(&[1]);
                self.put(hash);
            }
            None => self.put(&[0]),
        }
    }

    pub(crate) fn dir(&mut self, path: &[u8], hash: &[u8; 32]) {
        self.put(b"d");
        self.part(path);
        self.put(hash);
    }

    pub(crate) fn env(&mut self, name: &[u8], value: Option<&[u8]>) {
        self.put(b"e");
        self.part(name);
        match value {
            Some(value) => {
                self.put(&[1]);
                self.part(value);
            }
            None => self.put(&[0]),
        }
    }

    pub(crate) fn untraceable(&mut self, name: &[u8]) {
        self.put(b"u");
        self.part(name);
    }

    /// The key of `traced` as [`normalize`] leaves it: one not normal is sorted by reference,
    /// in a frame, and never copied.
    pub(crate) fn traced(mut self, traced: &Traced) -> ContentId {
        if normal(traced) {
            self.entries(&traced.files, &traced.dirs, &traced.env, &traced.untraceable);
            return self.finish();
        }
        frame::task(Tag::Usercode, |frame| {
            self.entries(
                sorted_in(&traced.files, frame),
                sorted_in(&traced.dirs, frame),
                sorted_in(&traced.env, frame),
                sorted_in(&traced.untraceable, frame),
            );
        });
        self.finish()
    }

    fn entries<'t>(
        &mut self,
        files: impl IntoIterator<Item = &'t (PathBuf, Option<[u8; 32]>)>,
        dirs: impl IntoIterator<Item = &'t (PathBuf, [u8; 32])>,
        env: impl IntoIterator<Item = &'t (OsString, Option<OsString>)>,
        untraceable: impl IntoIterator<Item = &'t Untraceable>,
    ) {
        for (path, hash) in files {
            self.file(&os_bytes(path.as_os_str()), hash.as_ref());
        }
        for (path, hash) in dirs {
            self.dir(&os_bytes(path.as_os_str()), hash);
        }
        for (name, value) in env {
            let value = value.as_deref().map(os_bytes);
            self.env(&os_bytes(name), value.as_deref());
        }
        for it in untraceable {
            self.untraceable(untraceable_name(*it).as_bytes());
        }
    }

    pub(crate) fn finish(mut self) -> ContentId {
        self.hasher.update(&self.batch[..self.len]);
        self.len = 0;
        self.hasher.finish()
    }
}

/// Whether `traced` is as [`normalize`] leaves it: each list strictly increasing.
pub(crate) fn normal(traced: &Traced) -> bool {
    traced.files.is_sorted_by(|a, b| a < b)
        && traced.dirs.is_sorted_by(|a, b| a < b)
        && traced.env.is_sorted_by(|a, b| a < b)
        && traced.untraceable.is_sorted_by(|a, b| a < b)
}

/// `items` by reference, sorted and each once, as [`normalize`] leaves them.
pub(crate) fn sorted_in<'t, 'f, T: Ord>(items: &'t [T], frame: &'f Frame) -> Vec<&'t T, &'f Frame> {
    let mut sorted = Vec::with_capacity_in(items.len(), frame);
    sorted.extend(items);
    sorted.sort_unstable();
    sorted.dedup();
    sorted
}

/// `traced` sorted and each entry once: what [`assemble`] makes, for a [`Traced`] made
/// otherwise (a wasm macro's host, which sees every call itself).
pub fn normalize(traced: &mut Traced) {
    traced.files.sort();
    traced.files.dedup();
    traced.dirs.sort();
    traced.dirs.dedup();
    traced.env.sort();
    traced.env.dedup();
    traced.untraceable.sort();
    traced.untraceable.dedup();
}

/// The bytes of `name` as the program had them: borrowed where a name is bytes, and on
/// Windows its UTF-16 units, little-endian, which every even run of bytes is, so
/// [`os_from`] reads back any it is given. What hashes these is under an [`os_domain!`].
pub fn os_bytes(name: &OsStr) -> Cow<'_, [u8]> {
    #[cfg(unix)]
    {
        Cow::Borrowed(std::os::unix::ffi::OsStrExt::as_bytes(name))
    }
    #[cfg(target_os = "wasi")]
    {
        Cow::Borrowed(std::os::wasi::ffi::OsStrExt::as_bytes(name))
    }
    #[cfg(windows)]
    {
        Cow::Owned(le_of_wide(std::os::windows::ffi::OsStrExt::encode_wide(name), name.len()))
    }
    #[cfg(not(any(unix, windows, target_os = "wasi")))]
    {
        Cow::Borrowed(name.as_encoded_bytes())
    }
}

/// The name [`os_bytes`] gave `bytes` of, or why they are not one. The bytes come from the
/// store or another process, so they are checked, never taken on trust.
pub(crate) fn os_from(bytes: &[u8]) -> Result<OsString, String> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        Ok(OsStr::from_bytes(bytes).to_owned()) // alloc: the name the caller keeps
    }
    #[cfg(target_os = "wasi")]
    {
        use std::os::wasi::ffi::OsStrExt as _;
        Ok(OsStr::from_bytes(bytes).to_owned()) // alloc: the name the caller keeps
    }
    #[cfg(windows)]
    {
        Ok(std::os::windows::ffi::OsStringExt::from_wide(&wide_of_le(bytes)?))
    }
    #[cfg(not(any(unix, windows, target_os = "wasi")))]
    {
        std::str::from_utf8(bytes)
            .map(OsString::from)
            .map_err(|_| "a name is not UTF-8".to_owned()) // alloc: error path
    }
}

/// The name [`os_bytes`] gave `bytes` of, borrowed in place where a name is its bytes.
pub(crate) fn os_in(bytes: &[u8]) -> Result<Cow<'_, OsStr>, String> {
    #[cfg(unix)]
    {
        Ok(Cow::Borrowed(std::os::unix::ffi::OsStrExt::from_bytes(bytes)))
    }
    #[cfg(not(unix))]
    {
        Ok(Cow::Owned(os_from(bytes)?))
    }
}

/// The path [`os_bytes`] gave `bytes` of, borrowed in place where a path is its bytes.
pub(crate) fn path_in(bytes: &[u8]) -> Result<Cow<'_, Path>, String> {
    Ok(match os_in(bytes)? {
        Cow::Borrowed(name) => Cow::Borrowed(Path::new(name)),
        Cow::Owned(name) => Cow::Owned(PathBuf::from(name)),
    })
}

/// The bytes of a Windows name's UTF-16 `units`, little-endian: what [`os_bytes`] gives
/// there. `hint` is the name's encoded length, about its unit count.
#[cfg(any(windows, test))]
fn le_of_wide(units: impl Iterator<Item = u16>, hint: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(2 * hint);
    for unit in units {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

/// The UTF-16 units [`le_of_wide`] gave `bytes` of. A Windows name is any units, lone
/// surrogates too, so any even number of bytes is one.
#[cfg(any(windows, test))]
fn wide_of_le(bytes: &[u8]) -> Result<Vec<u16>, String> {
    let units = bytes.chunks_exact(2);
    if !units.remainder().is_empty() {
        return Err("a name is an odd number of bytes, not UTF-16 units".to_owned()); // alloc: error path
    }
    Ok(units.map(|unit| u16::from_le_bytes([unit[0], unit[1]])).collect()) // alloc: the units a name is made of
}

/// What a sandbox's launcher leaves for the driver that ran it ([`encode`]).
#[derive(PartialEq, Eq, Debug)]
pub enum Report {
    /// The sandbox was made and the program ran; what it read when the sandbox traces.
    Ran(Option<Traced>),
    /// The sandbox could not be made, and why: the spawn failed, nothing ran outside it.
    NotMade(String),
}

/// Another on Windows, whose names are [`os_bytes`]' UTF-16 units: a report of an older
/// blaze's bytes there is not one.
const MAGIC: &[u8; 16] = if cfg!(windows) { b"blaze v2 report2" } else { b"blaze v2 report1" };

/// `report` as bytes: the magic, then each part length-prefixed, little-endian. Equal reports
/// are equal bytes.
pub fn encode(report: &Report) -> Vec<u8> {
    let mut out = MAGIC.to_vec(); // alloc: the edge, the bytes the launcher leaves the driver
    match report {
        Report::NotMade(why) => {
            out.put_u8(0);
            out.put_bytes_u64(why.as_bytes());
        }
        Report::Ran(None) => out.put_u8(1),
        Report::Ran(Some(traced)) => {
            out.put_u8(2);
            out.put_u64(traced.files.len() as u64);
            for (path, hash) in &traced.files {
                out.put_bytes_u64(&os_bytes(path.as_os_str()));
                match hash {
                    Some(hash) => {
                        out.put_u8(1);
                        out.put_bytes(hash);
                    }
                    None => out.put_u8(0),
                }
            }
            out.put_u64(traced.dirs.len() as u64);
            for (path, hash) in &traced.dirs {
                out.put_bytes_u64(&os_bytes(path.as_os_str()));
                out.put_bytes(hash);
            }
            out.put_u64(traced.env.len() as u64);
            for (name, value) in &traced.env {
                out.put_bytes_u64(&os_bytes(name));
                match value {
                    Some(value) => {
                        out.put_u8(1);
                        out.put_bytes_u64(&os_bytes(value));
                    }
                    None => out.put_u8(0),
                }
            }
            out.put_u64(traced.untraceable.len() as u64);
            for it in &traced.untraceable {
                out.put_bytes_u64(untraceable_name(*it).as_bytes());
            }
        }
    }
    out
}

/// The report `bytes` are, or why they are not one. Each list's count is checked against the
/// bytes left at one byte an entry, so no count makes a loop run past what was written.
pub fn decode(bytes: &[u8]) -> Result<Report, String> {
    let mut at = Reader::new(bytes);
    if at.take(MAGIC.len())? != MAGIC {
        return Err("not a sandbox's report".to_owned()); // alloc: error path
    }
    let report = match at.u8()? {
        0 => Report::NotMade(String::from_utf8_lossy(at.bytes_u64()?).into_owned()), // alloc: why a sandbox was not made
        1 => Report::Ran(None),
        2 => {
            let mut traced = Traced::default();
            for _ in 0..at.count_u64(1)? {
                let path = PathBuf::from(os_from(at.bytes_u64()?)?);
                let hash = match at.u8()? {
                    0 => None,
                    1 => Some(*at.array()?),
                    _ => return Err("a file's hash is neither there nor not".to_owned()), // alloc: error path
                };
                traced.files.push((path, hash));
            }
            for _ in 0..at.count_u64(1)? {
                let path = PathBuf::from(os_from(at.bytes_u64()?)?);
                traced.dirs.push((path, *at.array()?));
            }
            for _ in 0..at.count_u64(1)? {
                let name = os_from(at.bytes_u64()?)?;
                let value = match at.u8()? {
                    0 => None,
                    1 => Some(os_from(at.bytes_u64()?)?),
                    _ => return Err("a variable's value is neither there nor not".to_owned()), // alloc: error path
                };
                traced.env.push((name, value));
            }
            for _ in 0..at.count_u64(1)? {
                let name = at.bytes_u64()?;
                let untraceable = std::str::from_utf8(name).ok().and_then(untraceable_of);
                // alloc: error path
                let why = || format!("no untraceable is {:?}", String::from_utf8_lossy(name));
                traced.untraceable.push(untraceable.ok_or_else(why)?);
            }
            Report::Ran(Some(traced))
        }
        other => return Err(format!("no report is of kind {other}")), // alloc: error path
    };
    match at.is_done() {
        true => Ok(report),
        false => Err("the report has bytes after its end".to_owned()), // alloc: error path
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn a_batched_key_is_the_key_of_one_update_per_piece() {
        let run = crate::RunKey {
            code: ContentId::of("blaze v2 test 1", b"code"),
            given: ContentId::of("blaze v2 test 1", b"given"),
        };
        let paths: Vec<Vec<u8>> = (0..400).map(|n| format!("/usr/include/x86_64/header-{n}.h").into_bytes()).collect();
        let huge = vec![b'v'; KEYER_BATCH + 17];
        let mut batched = Keyer::new(&run);
        let mut reference = Hasher::new(crate::RunKey::DOMAIN);
        reference.id(&run.code).id(&run.given);
        for (n, path) in paths.iter().enumerate() {
            let hash = [n as u8; 32];
            batched.file(path, (n % 3 != 0).then_some(&hash));
            reference.update(b"f").part(path);
            match n % 3 != 0 {
                true => _ = reference.update(&[1]).update(&hash),
                false => _ = reference.update(&[0]),
            }
        }
        batched.dir(b"/src", &[9; 32]);
        reference.update(b"d").part(b"/src").update(&[9; 32]);
        batched.env(b"HUGE", Some(&huge));
        reference.update(b"e").part(b"HUGE").update(&[1]).part(&huge);
        batched.env(b"GONE", None);
        reference.update(b"e").part(b"GONE").update(&[0]);
        batched.untraceable(b"clock");
        reference.update(b"u").part(b"clock");
        assert_eq!(batched.finish(), reference.finish());
    }

    struct Fixed {
        files: BTreeMap<PathBuf, [u8; 32]>,
        dirs: BTreeMap<PathBuf, Vec<&'static str>>,
        env: Vec<(OsString, OsString)>,
    }

    impl Inputs for Fixed {
        type Error = String;
        fn entry(&mut self, path: &Path) -> Result<crate::trace::Entry, String> {
            Ok(match (self.dirs.contains_key(path), self.files.get(path)) {
                (true, _) => crate::trace::Entry::Dir,
                (false, Some(hash)) => crate::trace::Entry::File(blaze_ids::ContentId(*hash), blaze_vfs::Mode::Regular),
                (false, None) => crate::trace::Entry::Missing,
            })
        }
        fn file(&mut self, path: &Path) -> Result<Option<[u8; 32]>, String> {
            if self.dirs.contains_key(path) {
                return Ok(Some(DIRECTORY));
            }
            Ok(self.files.get(path).copied())
        }
        fn dir(&mut self, path: &Path) -> Result<Option<[u8; 32]>, String> {
            Ok(self.dirs.get(path).map(|names| listing(names.iter().map(OsStr::new))))
        }
        fn env(&mut self, name: &OsStr) -> Option<&OsStr> {
            self.env.iter().find(|it| it.0 == name).map(|it| it.1.as_os_str())
        }
        fn env_all(&mut self) -> &[(OsString, OsString)] {
            self.env.sort();
            &self.env
        }
        fn names(&mut self, path: &Path, out: &mut Names<'_>) -> Result<bool, String> {
            let Some(names) = self.dirs.get(path) else { return Ok(false) };
            for name in names {
                out.push(OsStr::new(name), self.dirs.contains_key(&path.join(name)));
            }
            out.sort();
            Ok(true)
        }
    }

    #[test]
    fn a_package_is_every_file_but_hidden_ones_and_its_target() {
        let mut inputs = Fixed {
            files: [
                ("/p/build.rs".into(), [1; 32]),
                ("/p/src/lib.rs".into(), [2; 32]),
                ("/p/target/x".into(), [3; 32]),
                ("/p/.git/HEAD".into(), [4; 32]),
            ]
            .into(),
            dirs: [
                ("/p".into(), vec!["build.rs", "src", "target", ".git"]),
                ("/p/src".into(), vec!["lib.rs"]),
                ("/p/target".into(), vec!["x"]),
                ("/p/.git".into(), vec!["HEAD"]),
            ]
            .into(),
            env: Vec::new(),
        };
        let before = package(&mut inputs, Path::new("/p")).unwrap().unwrap();
        inputs.files.insert("/p/target/x".into(), [9; 32]);
        inputs.files.insert("/p/.git/HEAD".into(), [9; 32]);
        assert_eq!(package(&mut inputs, Path::new("/p")).unwrap(), Some(before));
        inputs.files.insert("/p/src/lib.rs".into(), [9; 32]);
        assert_ne!(package(&mut inputs, Path::new("/p")).unwrap(), Some(before));
        assert_eq!(package(&mut inputs, Path::new("/none")).unwrap(), None);
        assert_eq!(package_of(&Path::new("/p").join(PACKAGE)), Some(Path::new("/p")));
        assert_eq!(package_of(Path::new("/p/src")), None);
    }

    fn fixed() -> Fixed {
        Fixed {
            files: [("/ws/a.txt".into(), [1; 32]), ("/usr/bin/cc".into(), [2; 32])].into(),
            dirs: [("/ws/data".into(), vec!["b", "a"])].into(),
            env: vec![("PATH".into(), "/usr/bin".into()), ("CC".into(), "cc".into())],
        }
    }

    #[test]
    fn a_line_round_trips_and_a_tab_makes_it_incomplete() {
        let line = Line::new(Seen::Env, "tests::a", "CC");
        assert_eq!(line.text(), "E\ttests::a\tCC\n");
        assert_eq!(Line::parse(line.text().trim_end_matches('\n')), Some(line));
        assert_eq!(Line::new(Seen::Read, "", "/a\tb").seen, Seen::Incomplete);
        assert_eq!(Line::new(Seen::Read, "x\ny", "/a").seen, Seen::Incomplete);
    }

    #[test]
    fn a_record_being_written_leaves_its_last_line() {
        let (lines, used) = parse("R\t\t/a\nE\t\tP").unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(used, "R\t\t/a\n".len());
        assert!(parse("Q\t\t/a\n").is_err());
    }

    #[test]
    fn assembling_is_sorted_once_and_by_scope() {
        let lines = [
            Line::new(Seen::Read, "", "/usr/bin/cc"),
            Line::new(Seen::Read, "e1", "/ws/a.txt"),
            Line::new(Seen::Looked, "e1", "/ws/a.txt"),
            Line::new(Seen::Read, "e2", "/ws/missing"),
            Line::new(Seen::Listed, "e1", "/ws/data"),
            Line::new(Seen::Listed, "e1", "/ws/nodir"),
            Line::new(Seen::Env, "e1", "CC"),
            Line::new(Seen::Env, "e1", "UNSET"),
            Line::new(Seen::RefusedWrite, "e1", "/etc/x"),
        ];
        let traced = assemble(&lines, Some("e1"), &mut fixed()).unwrap();
        assert_eq!(
            traced.files,
            vec![
                ("/usr/bin/cc".into(), Some([2; 32])),
                ("/ws/a.txt".into(), Some([1; 32])),
                ("/ws/nodir".into(), None),
            ]
        );
        assert_eq!(traced.dirs, vec![("/ws/data".into(), listing(["a", "b"].map(OsStr::new)))]);
        assert_eq!(traced.env, vec![("CC".into(), Some("cc".into())), ("UNSET".into(), None)]);
        assert!(traced.untraceable.is_empty());
        let other = assemble(&lines, Some("e2"), &mut fixed()).unwrap();
        assert_eq!(other.files, vec![("/usr/bin/cc".into(), Some([2; 32])), ("/ws/missing".into(), None)]);
    }

    #[test]
    fn the_whole_environment_and_what_cannot_be_traced() {
        let lines = [
            Line::new(Seen::Env, "", ALL_ENV),
            Line::new(Seen::Untraceable, "", "clock"),
            Line::new(Seen::Untraceable, "", "sundial"),
            Line::new(Seen::Incomplete, "", "a program that did not load the library"),
        ];
        let mut inputs = fixed();
        let traced = assemble(&lines, None, &mut inputs).unwrap();
        assert_eq!(traced.env.len(), 3);
        assert_eq!(traced.env[0].0, ALL_ENV_NAME);
        assert_eq!(traced.env[0].1.as_deref(), Some(OsStr::new(&names_hash(&inputs.env))));
        assert_eq!(traced.untraceable, vec![Untraceable::Clock, Untraceable::Descriptor]);
    }

    /// [`Fixed`], noting what it was asked in the order it was asked.
    struct Asked(Fixed, Vec<String>);

    impl Inputs for Asked {
        type Error = String;
        fn entry(&mut self, path: &Path) -> Result<Entry, String> {
            self.0.entry(path)
        }
        fn file(&mut self, path: &Path) -> Result<Option<[u8; 32]>, String> {
            self.1.push(format!("file {}", path.display()));
            self.0.file(path)
        }
        fn dir(&mut self, path: &Path) -> Result<Option<[u8; 32]>, String> {
            self.1.push(format!("dir {}", path.display()));
            self.0.dir(path)
        }
        fn env(&mut self, name: &OsStr) -> Option<&OsStr> {
            self.1.push(format!("env {}", name.display()));
            self.0.env(name)
        }
        fn env_all(&mut self) -> &[(OsString, OsString)] {
            self.1.push("env_all".into());
            self.0.env_all()
        }
        fn names(&mut self, path: &Path, out: &mut Names<'_>) -> Result<bool, String> {
            self.0.names(path, out)
        }
    }

    #[test]
    fn inputs_are_asked_in_sorted_order_whatever_order_the_lines_came_in() {
        let mut lines: Vec<Line> = [
            (Seen::Read, "/ws/z"),
            (Seen::Read, "/usr/bin/cc"),
            (Seen::Looked, "/ws/a.txt"),
            (Seen::Read, "/ws/a.txt"),
            (Seen::Listed, "/ws/data"),
            (Seen::Listed, "/ws/b"),
            (Seen::Env, "ZED"),
            (Seen::Env, "CC"),
            (Seen::Env, "CC"),
            (Seen::Untraceable, "random"),
            (Seen::Untraceable, "clock"),
            (Seen::Untraceable, "random"),
        ]
        .into_iter()
        .map(|(seen, what)| Line::new(seen, "", what))
        .collect();
        let mut first = Asked(fixed(), Vec::new());
        let traced = assemble(&lines, None, &mut first).unwrap();
        assert_eq!(
            first.1,
            [
                "file /usr/bin/cc",
                "file /ws/a.txt",
                "file /ws/z",
                "dir /ws/b",
                "dir /ws/data",
                "env CC",
                "env ZED",
            ]
        );
        let paths: Vec<&Path> = traced.files.iter().map(|it| it.0.as_path()).collect();
        assert_eq!(paths, [Path::new("/usr/bin/cc"), Path::new("/ws/a.txt"), Path::new("/ws/b"), Path::new("/ws/z")]);
        assert_eq!(traced.env, vec![("CC".into(), Some("cc".into())), ("ZED".into(), None)]);
        assert_eq!(traced.untraceable, vec![Untraceable::Clock, Untraceable::Random]);
        for turn in 1..lines.len() {
            lines.rotate_left(turn);
            lines.reverse();
            let mut again = Asked(fixed(), Vec::new());
            assert_eq!(assemble(&lines, None, &mut again).unwrap(), traced);
            assert_eq!(again.1, first.1);
        }
    }

    #[test]
    fn listings_ignore_order() {
        assert_eq!(listing(["b", "a"].map(OsStr::new)), listing(["a", "b"].map(OsStr::new)));
        assert_ne!(listing(["ab"].map(OsStr::new)), listing(["a", "b"].map(OsStr::new)));
    }

    /// A walk of a package allocates nothing once the thread's frame and path buffer have
    /// grown: each listing is packed in the walk's frame, the path pushed and popped.
    #[cfg(unix)]
    #[test]
    fn a_warm_walk_allocates_nothing() {
        struct Tree(BTreeMap<PathBuf, Vec<(&'static str, bool)>>);
        impl Inputs for Tree {
            type Error = String;
            fn entry(&mut self, path: &Path) -> Result<Entry, String> {
                Ok(match self.0.contains_key(path) {
                    true => Entry::Dir,
                    false => Entry::File(ContentId([7; 32]), blaze_vfs::Mode::Regular),
                })
            }
            fn file(&mut self, _: &Path) -> Result<Option<[u8; 32]>, String> {
                Ok(Some([7; 32]))
            }
            fn dir(&mut self, _: &Path) -> Result<Option<[u8; 32]>, String> {
                Ok(None)
            }
            fn env(&mut self, _: &OsStr) -> Option<&OsStr> {
                None
            }
            fn env_all(&mut self) -> &[(OsString, OsString)] {
                &[]
            }
            fn names(&mut self, path: &Path, out: &mut Names<'_>) -> Result<bool, String> {
                let Some(names) = self.0.get(path) else { return Ok(false) };
                for (name, dir) in names {
                    out.push(OsStr::new(name), *dir);
                }
                out.sort();
                Ok(true)
            }
        }
        let mut tree = Tree(BTreeMap::from([
            ("/p".into(), vec![("src", true), ("build.rs", false), (".git", true), ("target", true)]),
            ("/p/src".into(), vec![("lib.rs", false), ("deep", true)]),
            ("/p/src/deep".into(), vec![("a.rs", false)]),
        ]));
        let cold = walk(&mut tree, Path::new("/p"), true).unwrap();
        let warm = blaze_alloc_count::assert_at_most(0, "a package walked, warm", || {
            walk(&mut tree, Path::new("/p"), true).unwrap()
        });
        assert_eq!(warm, cold);
        let listed = blaze_alloc_count::assert_at_most(0, "a listing hashed", || {
            listing(["b", "a", "a"].map(OsStr::new))
        });
        assert_eq!(listed, listing(["a", "b"].map(OsStr::new)));
    }

    #[test]
    fn reports_round_trip() {
        let lines = [
            Line::new(Seen::Read, "", "/ws/a.txt"),
            Line::new(Seen::Listed, "", "/ws/data"),
            Line::new(Seen::Env, "", "CC"),
            Line::new(Seen::Env, "", "UNSET"),
            Line::new(Seen::Untraceable, "", "random"),
        ];
        let traced = assemble(&lines, None, &mut fixed()).unwrap();
        for report in [
            Report::Ran(Some(traced)),
            Report::Ran(None),
            Report::NotMade("no user namespaces".into()),
        ] {
            let bytes = encode(&report);
            assert_eq!(decode(&bytes), Ok(report));
            assert!(decode(&bytes[..bytes.len() - 1]).is_err());
        }
        let mut counts_past_its_end = MAGIC.to_vec();
        counts_past_its_end.extend_from_slice(b"\x02\xff\xff\xff\xff\xff\xff\xff\xff");
        assert!(decode(&counts_past_its_end).is_err());
    }

    #[test]
    fn windows_names_round_trip_as_their_units() {
        let lone: [u16; 5] = [0x61, 0xd800, 0xdc00, 0xdfff, 0x2e];
        for units in [vec![], "a.rs".encode_utf16().collect(), "日本/🦀".encode_utf16().collect(), lone.to_vec()] {
            let bytes = le_of_wide(units.iter().copied(), units.len());
            assert_eq!(bytes.len(), 2 * units.len());
            assert_eq!(wide_of_le(&bytes), Ok(units));
        }
        assert_eq!(le_of_wide([0x012e].into_iter(), 1), [0x2e, 0x01]);
        for odd in [&b"a"[..], b"abc"] {
            assert!(wide_of_le(odd).is_err(), "{odd:?}");
        }
    }

    #[test]
    fn os_bytes_and_os_from_agree() {
        for name in ["", "a.rs", "é/ß", "\0*"] {
            let bytes = os_bytes(OsStr::new(name));
            assert_eq!(os_from(&bytes).as_deref(), Ok(OsStr::new(name)));
            assert_eq!(os_in(&bytes).as_deref(), Ok(OsStr::new(name)));
            assert_eq!(path_in(&bytes).as_deref(), Ok(Path::new(name)));
        }
        #[cfg(unix)]
        assert!(matches!(os_bytes(OsStr::new("a")), Cow::Borrowed(b"a")));
    }
}
