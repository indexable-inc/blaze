//! The request's outside reads, and the answers to them now: what [`Effect::Outside`] (a filed
//! certificate's asks, checked before anything runs) and [`Effect::Reads`] (the request's own
//! reads, settled before its certificate is filed) need, with no daemon. A fact's answer
//! ([`Effect::Answer`]) is not here: it is a lookup in mapped tables
//! ([`blaze_keyed::front::Universe::answer`]), and the late reads it answers from are the
//! request's own ([`crate::Request::late`]).
//!
//! [`Effect::Outside`]: blaze_keyed::driver::Effect::Outside
//! [`Effect::Answer`]: blaze_keyed::driver::Effect::Answer
//! [`Effect::Reads`]: blaze_keyed::driver::Effect::Reads
//!
//! A certificate's shape is the bytes of its asks ([`blaze_keyed::outside`]: "the asks
//! themselves, in the kind's own encoding"); this module is that encoding's one home for the
//! certificate ([`encode`], [`asks`]): a count, then per ask its kind and its bytes. An ask is
//! the bytes of a file under the request's root ([`Ask::File`]) or the value of a variable
//! ([`Ask::Var`]); its answer is a content id, a different one for a file that is gone, a
//! directory, or a variable that is unset, so each of those is a value too.
//!
//! [`Answerer`] answers asks now over an injected [`Files`] (stat, read, a variable), with a
//! leaf memory ([`Leaves`], the command's table of stamps and ids from its last look): a file
//! whose stamp is the one remembered, and not racy by git's rule ([`blaze_vfs::racy`]), is
//! answered with no read. Everything it hashed is given back ([`Answerer::looked`]) for the
//! command to remember.
//!
//! Why not blaze-certify or blaze-solo: blaze-certify's read list is the daemon's
//! (blaze-record and blaze-protocol types, a slot file), and the keyed certificate is
//! blaze-keyed's outside record, so filing through it would be a second certificate; blaze-solo's
//! sweep is a whole-checkout sweep over its own table and is not a workspace dependency (the
//! root Cargo.toml lists no `blaze-solo`). This answerer asks only what the request read.
//!
//! Floors (rule 67), per ask: a file with a remembered steady stamp, one `lstat`; else one
//! `lstat`, one read and one hash of its bytes, and one `lstat` after (a file that moved while
//! it was read is answered as read and not remembered). A variable, one lookup. Nothing else.

#![blaze::sans_io]

use std::{alloc::Global, fmt};

use blaze_core::Stamp;
use blaze_ids::ContentId;

/// What a file that is gone answers.
pub const GONE_DOMAIN: &str = "blaze-host outside gone 1";
/// What a path that is a directory answers.
pub const DIR_DOMAIN: &str = "blaze-host outside directory 1";
/// What a variable answers: its value's bytes in this domain, or [`UNSET_DOMAIN`]'s empty id.
pub const VAR_DOMAIN: &str = "blaze-host outside variable 1";
pub const UNSET_DOMAIN: &str = "blaze-host outside unset 1";

const FILE: u8 = 1;
const VAR: u8 = 2;

/// One outside read of a request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ask<'a> {
    /// The bytes of the file at this path, `/`-separated, relative to the root [`Files`]
    /// answers from.
    File(&'a [u8]),
    /// The value of this variable.
    Var(&'a [u8]),
}

/// The answer to a file ask whose bytes are `bytes`: what a command gives as a read's answer
/// when the request began (the bytes it handed the front end).
pub fn file_answer(bytes: &[u8]) -> ContentId {
    ContentId::of(blaze_vfs::FILE_DOMAIN, bytes)
}

/// The answer to a variable ask whose value is `value`.
pub fn var_answer(value: Option<&[u8]>) -> ContentId {
    match value {
        Some(value) => ContentId::of(VAR_DOMAIN, value),
        None => ContentId::of(UNSET_DOMAIN, b""),
    }
}

/// `asks` as a shape, appended to `out`: count u32, then per ask kind u8, length u32, bytes.
/// Little-endian. The asks are written in the order given; the answers are in that order.
pub fn encode(asks: &[Ask<'_>], out: &mut Vec<u8, Global>) {
    out.extend_from_slice(&(asks.len() as u32).to_le_bytes());
    for ask in asks {
        let (kind, bytes) = match *ask {
            Ask::File(path) => (FILE, path),
            Ask::Var(name) => (VAR, name),
        };
        out.push(kind);
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(bytes);
    }
}

/// Why bytes are not a shape of this encoding, naming where.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShapeError {
    /// It ends at byte `at`, inside what it says is there.
    Short { at: usize },
    /// The ask at byte `at` is of kind `kind`, which names no ask.
    Kind { at: usize, kind: u8 },
    /// It goes on past its last ask, from byte `at`.
    Trailing { at: usize },
}

impl fmt::Display for ShapeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShapeError::Short { at } => write!(f, "the certificate's shape ends at byte {at}, inside an ask"),
            ShapeError::Kind { at, kind } => write!(f, "the certificate's ask at byte {at} is of kind {kind}, which names no ask"),
            ShapeError::Trailing { at } => write!(f, "the certificate's shape goes on past its last ask, from byte {at}"),
        }
    }
}

/// The asks of `shape`, in order, each checked as it is read; the last item is an error when
/// the shape does not end where its count says.
pub fn asks(shape: &[u8]) -> Asks<'_> {
    Asks { shape, at: 0, left: None }
}

pub struct Asks<'a> {
    shape: &'a [u8],
    at: usize,
    /// Asks left; `None` before the count is read.
    left: Option<u32>,
}

impl<'a> Asks<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], ShapeError> {
        let end = self.at.checked_add(len).filter(|end| *end <= self.shape.len()).ok_or(ShapeError::Short { at: self.shape.len() })?;
        let bytes = &self.shape[self.at..end];
        self.at = end;
        Ok(bytes)
    }

    fn u32(&mut self) -> Result<u32, ShapeError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
}

impl<'a> Iterator for Asks<'a> {
    type Item = Result<Ask<'a>, ShapeError>;

    fn next(&mut self) -> Option<Self::Item> {
        let left = match self.left {
            Some(left) => left,
            None => match self.u32() {
                Ok(count) => count,
                Err(why) => {
                    self.left = Some(0);
                    self.at = self.shape.len();
                    return Some(Err(why));
                }
            },
        };
        if left == 0 {
            self.left = Some(0);
            if self.at < self.shape.len() {
                let at = self.at;
                self.at = self.shape.len();
                return Some(Err(ShapeError::Trailing { at }));
            }
            return None;
        }
        self.left = Some(left - 1);
        let start = self.at;
        let ask = (|| {
            let kind = self.take(1)?[0];
            let len = self.u32()? as usize;
            let bytes = self.take(len)?;
            match kind {
                FILE => Ok(Ask::File(bytes)),
                VAR => Ok(Ask::Var(bytes)),
                kind => Err(ShapeError::Kind { at: start, kind }),
            }
        })();
        if ask.is_err() {
            self.left = Some(0);
            self.at = self.shape.len();
        }
        Some(ask)
    }
}

/// A look at the world the answerer is given: the one IO seam of this module. The real one is
/// [`crate::disk::Disk`].
pub trait Files {
    /// The stamp of `path` now, by `lstat`: [`Stamp::GONE`] when nothing is there.
    fn stamp(&self, path: &[u8]) -> Result<Stamp, FilesError>;

    /// The bytes of the file at `path`, into `out` (cleared first).
    fn read(&self, path: &[u8], out: &mut Vec<u8, Global>) -> Result<(), FilesError>;

    /// The value of variable `name` now, into `out` (cleared first): `false` when it is unset.
    fn var(&self, name: &[u8], out: &mut Vec<u8, Global>) -> Result<bool, FilesError>;
}

/// A look that failed: which call, and the OS's error number.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FilesError {
    pub call: &'static str,
    pub errno: i32,
}

/// An ask that could not be answered: what was asked, and why.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum OutsideError {
    Shape(ShapeError),
    /// A look at `path` (or a variable's name) failed.
    Files { path: Vec<u8, Global>, error: FilesError },
}

impl fmt::Display for OutsideError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OutsideError::Shape(why) => write!(f, "{why}"),
            OutsideError::Files { path, error } => write!(
                f,
                "the outside read of {}: {} failed with errno {} (rule errors-name-the-thing: an outside read is answered or the request stops)",
                String::from_utf8_lossy(path),
                error.call,
                error.errno
            ),
        }
    }
}

/// One remembered look: a file's path, its stamp, and the id of the bytes read under it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Leaf<'l> {
    pub path: &'l [u8],
    pub stamp: Stamp,
    pub id: ContentId,
}

/// The command's leaf memory: rows sorted by path, each once, looked at no earlier than
/// `instant_ns` (the wall clock before that look's first stat, a lower bound of every look).
#[derive(Clone, Copy, Debug)]
pub struct Leaves<'l> {
    pub instant_ns: u64,
    pub rows: &'l [Leaf<'l>],
}

impl Leaves<'_> {
    pub const NONE: Leaves<'static> = Leaves { instant_ns: 0, rows: &[] };

    fn find(&self, path: &[u8]) -> Option<&Leaf<'_>> {
        self.rows.binary_search_by(|row| row.path.cmp(path)).ok().map(|at| &self.rows[at])
    }
}

/// A file hashed by this request, to remember: its path, the stamp it was read under, its id.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Looked {
    pub path: Vec<u8, Global>,
    pub stamp: Stamp,
    pub id: ContentId,
}

/// Answers asks now.
pub struct Answerer<'f, 'l, F: Files> {
    files: &'f F,
    leaves: Leaves<'l>,
    looked: Vec<Looked, Global>,
    bytes: Vec<u8, Global>,
}

impl<'f, 'l, F: Files> Answerer<'f, 'l, F> {
    pub fn new(files: &'f F, leaves: Leaves<'l>) -> Answerer<'f, 'l, F> {
        Answerer { files, leaves, looked: Vec::new_in(Global), bytes: Vec::new_in(Global) }
    }

    /// The answer to `ask` now.
    pub fn answer(&mut self, ask: Ask<'_>) -> Result<ContentId, OutsideError> {
        let failed = |path: &[u8], error: FilesError| {
            let mut owned = Vec::with_capacity_in(path.len(), Global);
            owned.extend_from_slice(path);
            OutsideError::Files { path: owned, error }
        };
        match ask {
            Ask::Var(name) => {
                let set = self.files.var(name, &mut self.bytes).map_err(|error| failed(name, error))?;
                Ok(var_answer(set.then_some(&self.bytes[..])))
            }
            Ask::File(path) => {
                let stamp = self.files.stamp(path).map_err(|error| failed(path, error))?;
                if stamp.is_gone() {
                    return Ok(ContentId::of(GONE_DOMAIN, b""));
                }
                if stamp.is_dir() {
                    return Ok(ContentId::of(DIR_DOMAIN, b""));
                }
                // A link's stamp does not move when its target does: never vouched for.
                if !stamp.is_link()
                    && let Some(leaf) = self.leaves.find(path)
                    && leaf.stamp == stamp
                    && !blaze_vfs::racy(&stamp, self.leaves.instant_ns)
                {
                    return Ok(leaf.id);
                }
                self.files.read(path, &mut self.bytes).map_err(|error| failed(path, error))?;
                let id = file_answer(&self.bytes);
                let after = self.files.stamp(path).map_err(|error| failed(path, error))?;
                if after == stamp && !stamp.is_link() {
                    let mut owned = Vec::with_capacity_in(path.len(), Global);
                    owned.extend_from_slice(path);
                    self.looked.push(Looked { path: owned, stamp, id });
                }
                Ok(id)
            }
        }
    }

    /// The answers now of every ask of `shape`, in order, into `out` (cleared first).
    pub fn answer_shape(&mut self, shape: &[u8], out: &mut Vec<ContentId, Global>) -> Result<(), OutsideError> {
        out.clear();
        for ask in asks(shape) {
            let ask = ask.map_err(OutsideError::Shape)?;
            out.push(self.answer(ask)?);
        }
        Ok(())
    }

    /// What this answerer hashed, in the order it did, a path at most once per look.
    pub fn looked(self) -> Vec<Looked, Global> {
        self.looked
    }
}
