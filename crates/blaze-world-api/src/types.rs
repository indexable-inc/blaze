//! The schema's types. Every text and list is in the caller's allocator (rule 75); a path is a
//! [`Path`] checked once when made (rule 76), so a value of these types always encodes to bytes
//! its decode accepts (within the limits).

use blaze_id::ItemKey;
use blaze_own::{AString, AVec, Alloc};

use crate::{Call, EditWhy, EntryKind, WireCode, error::BadPath};

/// A world: the digest of a tree, the key of its answer's certificate (row X1). Two callers
/// that make one patch on one world get one id.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct WorldId(pub [u8; 32]);

/// A finding's key: the key of the check that found it (rule 81), so a finding is named across
/// requests by what it is, not by when it was printed.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct FindingId(pub [u8; 32]);

/// A file's path in a world: relative, `/`-separated, no empty, `.` or `..` part, no NUL and no
/// `\`. Nothing named by one can be outside the world's tree.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Path<'a>(AString<'a>);

impl<'a> Path<'a> {
    pub fn new_in(text: &str, alloc: Alloc<'a>) -> Result<Path<'a>, BadPath> {
        match relative(text) {
            true => Ok(Path(AString::from_str_in(text, alloc))),
            false => Err(BadPath),
        }
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// Whether `text` is a [`Path`].
pub(crate) fn relative(text: &str) -> bool {
    !text.is_empty()
        && !text.bytes().any(|b| b == 0 || b == b'\\')
        && text.split('/').all(|part| !part.is_empty() && part != "." && part != "..")
}

/// A checkout: the absolute path of a working directory on the machine that serves the call.
/// Only `open` and `commit` name one; no world holds it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Checkout<'a>(AString<'a>);

impl<'a> Checkout<'a> {
    pub fn new_in(text: &str, alloc: Alloc<'a>) -> Result<Checkout<'a>, BadPath> {
        match absolute(text) {
            true => Ok(Checkout(AString::from_str_in(text, alloc))),
            false => Err(BadPath),
        }
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// Whether `text` is a [`Checkout`].
pub(crate) fn absolute(text: &str) -> bool {
    text.starts_with('/') && !text.bytes().any(|b| b == 0) && !text.split('/').any(|part| part == "..")
}

/// One request: what a connection carries in.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Request<'a> {
    /// Today's disk as a world. Reads; never builds.
    Open { checkout: Checkout<'a> },
    /// The world `base` with `edits` applied in order: all of them, or a refusal naming the
    /// first that cannot be, and no world made.
    Patch { base: WorldId, edits: AVec<'a, Edit<'a>> },
    Read { world: WorldId, what: What<'a> },
    /// The answer for `world`; `select` empty means the whole world.
    Check { world: WorldId, select: AVec<'a, Target<'a>>, lints: bool },
    Diff { from: WorldId, to: WorldId },
    /// Write `world`'s files onto `onto`, only if `onto` is still `expect`.
    Commit { world: WorldId, onto: Checkout<'a>, expect: WorldId },
    Explain { world: WorldId, subject: Subject },
}

impl Request<'_> {
    pub fn call(&self) -> Call {
        match self {
            Request::Open { .. } => Call::Open,
            Request::Patch { .. } => Call::Patch,
            Request::Read { .. } => Call::Read,
            Request::Check { .. } => Call::Check,
            Request::Diff { .. } => Call::Diff,
            Request::Commit { .. } => Call::Commit,
            Request::Explain { .. } => Call::Explain,
        }
    }
}

/// One edit of a patch. Offsets are bytes of the base world's file.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Edit<'a> {
    /// The file made or replaced.
    WriteFile { path: Path<'a>, text: AString<'a> },
    DeleteFile { path: Path<'a> },
    RenameFile { from: Path<'a>, to: Path<'a> },
    /// `start..end` of the file replaced by `text`, if those bytes are `expect`.
    ReplaceRange { path: Path<'a>, start: u64, end: u64, text: AString<'a>, expect: AString<'a> },
    /// The item's whole text replaced, the item named by the key the agent's edit tool gave
    /// (row W2): no file is outlined to find it.
    ReplaceItem { item: ItemKey, text: AString<'a> },
}

/// What a read reads.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum What<'a> {
    File { path: Path<'a> },
    Item { key: ItemKey },
    /// A directory's names; `None` is the world's root (the empty path on the wire).
    Dir { path: Option<Path<'a>> },
}

/// What a check is asked about.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Target<'a> {
    Path { path: Path<'a> },
    Item { key: ItemKey },
}

/// What an explain explains.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Subject {
    Finding { id: FindingId },
    Item { key: ItemKey },
}

/// One response: what a connection carries out.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Response<'a> {
    /// The world `open` or `patch` made.
    World { world: WorldId },
    Refused { refusal: Refusal<'a> },
    /// A read file's text (`start` 0), or an item's text and where in its file it starts.
    Text { path: Path<'a>, start: u64, text: AString<'a> },
    Listing { entries: AVec<'a, Entry<'a>> },
    Checked { answer: Answer<'a> },
    Changes { changes: AVec<'a, Change<'a>> },
    Committed { files_written: u64 },
    /// The checkout is not the world the commit expected: nothing was written.
    Moved { now: WorldId },
    /// Why the subject was entered, and what it read, as names.
    Explained { why: AString<'a>, reads: AVec<'a, AString<'a>> },
}

/// A check's answer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Answer<'a> {
    pub world: WorldId,
    pub findings: AVec<'a, Finding<'a>>,
    /// The answer is the certificate's: every finding of the world is here.
    pub certified: bool,
    pub entered: Entered,
    pub ms: u64,
}

/// Tasks entered, by stage.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entered {
    pub names: u64,
    pub heads: u64,
    pub bodies: u64,
    pub lints: u64,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry<'a> {
    pub name: AString<'a>,
    pub kind: EntryKind,
}

/// A finding: an error or a lint's, keyed like its check (rule 81).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Finding<'a> {
    pub id: FindingId,
    /// The rule's name or the error's code.
    pub code: AString<'a>,
    pub message: AString<'a>,
    pub at: Spot<'a>,
    pub notes: AVec<'a, Note<'a>>,
    /// Edits that fix it: applying them is a `patch`. Empty: no fix.
    pub fix: AVec<'a, Edit<'a>>,
}

/// A place in an item.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Spot<'a> {
    pub item: ItemKey,
    pub path: Path<'a>,
    pub place: Place,
}

/// `start..end` in bytes from the item's start (rule 71), and the line and column (1-based)
/// of `start` in its file, for printing only.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Place {
    pub start: u64,
    pub end: u64,
    pub line: u64,
    pub column: u64,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Note<'a> {
    pub message: AString<'a>,
    /// Where it points; empty: nowhere.
    pub at: AVec<'a, Spot<'a>>,
}

/// One difference between two worlds: by file, and by item where both worlds hold the item's
/// key. A rename is a delete and an add.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Change<'a> {
    FileAdded { path: Path<'a> },
    FileDeleted { path: Path<'a> },
    FileChanged { path: Path<'a> },
    ItemAdded { item: ItemKey, path: Path<'a> },
    ItemRemoved { item: ItemKey, path: Path<'a> },
    ItemChanged { item: ItemKey, path: Path<'a> },
}

/// Why a call made no answer. Every call can be refused; the spec lists which refusal each can
/// give.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Refusal<'a> {
    /// A patch's edit `index` (from 0) cannot be applied; no world was made.
    Edit { index: u64, why: EditWhy },
    /// No record of the world: the helper cannot rebuild its tree.
    NoSuchWorld { world: WorldId },
    NotACheckout { checkout: Checkout<'a> },
    NoSuchPath { path: Path<'a> },
    NoSuchItem { item: ItemKey },
    NoSuchFinding { id: FindingId },
    /// The request's bytes were refused ([`crate::WireError`], as sent back).
    Wire { at: u64, what: WireCode, field: AString<'a>, said: u64 },
    /// The disk refused a read or a write of `path`.
    Io { path: Path<'a>, message: AString<'a> },
    /// The call is in the schema but nothing serves it for this world yet (a later row's).
    Unsupported { call: Call, why: AString<'a> },
}
