//! The front end's one database, [`Db`]: every query of hir-expand, hir-def, hir-ty and the
//! checkers takes `&Db` (docs/v2/plan/design/reconnect.md, "The database").
//!
//! A `Db` is one task's snapshot, and `'db` is that snapshot's life: every id, every `&'db`
//! borrow of a memo value and every view of a record is valid until the task ends, and none of
//! them is written anywhere that outlives it (an id is not `blaze_id::Portable`; a record names
//! an item by its key).
//!
//! It holds:
//! - the memo ([`blaze_memo::Memo`]): the task's tables, with the crates' inputs in their
//!   arenas, as the type checker's queries have always run on;
//! - the records ([`Records`]): every crate's surface records the task may read, and the text
//!   of the crate it compiles under that crate's [`HasText`];
//! - the task's table of local items ([`LocalTable`]): an item key to the small number the
//!   task knows it by;
//! - the crate list in topological order, and the task's file table (a file's content to the
//!   [`FileId`] the syntax layer's spans carry).
//!
//! The driver makes one per task: it builds the memo with a [`Crate`] input per crate, fills
//! and seals the records, and calls [`Db::new`]. Nothing here does IO.

use std::alloc::Global;

use blaze_core::lock::{Lock, LockGuard};
use blaze_surface::{AnyItem, ContentId, CrateItemKey, HasText, ItemKey, Local, LocalTable, SurfaceError};
use span::FileId;

use crate::given::{Gifts, GivenTwice};
use crate::{Crate, Nonce, OrNeed, Records};

/// The borrow checker's mode for the task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum BorrowckMode {
    /// Every body is borrow checked.
    #[default]
    On,
    /// No body is: a check that reports type errors alone.
    Off,
}

/// The front end's settings for one task: what the command line says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct FrontSettings {
    pub borrowck: BorrowckMode,
}

impl FrontSettings {
    /// The task's settings.
    pub fn get(db: &Db) -> FrontSettings {
        db.settings
    }
}

/// Why [`Db::new`] refused what it was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbError {
    /// Two crates of the list have one key.
    CrateTwice { krate: CrateItemKey },
    /// Two files of the file table have one content.
    FileTwice { file: ContentId },
    /// The file table would pass `u32::MAX` files.
    TooManyFiles,
    /// A crate was given one slot twice.
    GivenTwice(GivenTwice),
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DbError::CrateTwice { krate } => write!(f, "two crates of the task have the key {krate:x}"),
            DbError::FileTwice { file } => write!(f, "two files of the task have the content {file:x}"),
            DbError::TooManyFiles => f.write_str("the task's file table would pass u32::MAX files"),
            DbError::GivenTwice(twice) => twice.fmt(f),
        }
    }
}

impl std::error::Error for DbError {}

pub struct Db {
    memo: blaze_memo::Memo,
    records: Records,
    locals: Lock<LocalTable<AnyItem, Global>>,
    /// In topological order: a crate after every crate it depends on.
    crates: std::sync::Arc<[Crate], Global>,
    /// `crates` by key, sorted.
    by_key: Box<[(CrateItemKey, Crate)], Global>,
    /// A file's content at its [`FileId`]'s index.
    files: Box<[ContentId], Global>,
    /// `files` by content, sorted, to the index.
    file_ids: Box<[(ContentId, u32)], Global>,
    /// The files `include!`, `include_str!` and `include_bytes!` may name, sorted.
    includes: Includes,
    gifts: Gifts,
    settings: FrontSettings,
    nonce: Nonce,
}

/// A path written in a file, read relative to that file's directory (`include!`'s argument).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AnchoredPath<'a> {
    pub anchor: FileId,
    pub path: &'a str,
}

/// The include table of a task: the facts the driver found for every `include!`-family path of
/// the crate it compiles (from the crate's surface, with its sandboxed reads), each a file of the
/// task's table, with the bytes of one that is not UTF-8. Nothing here reads a disk.
#[derive(Default)]
pub struct Includes {
    /// (anchor, path as written) to the file, sorted.
    paths: Vec<(FileId, Box<str, Global>, FileId), Global>,
    /// The bytes of an included file that has no text, by file, sorted.
    bytes: Vec<(FileId, Box<[u8], Global>), Global>,
}

impl Includes {
    pub fn new() -> Includes {
        Includes { paths: Vec::new_in(Global), bytes: Vec::new_in(Global) }
    }

    /// `path`, written in `anchor`, names `file`.
    pub fn path(&mut self, anchor: FileId, path: &str, file: FileId) {
        let mut text = String::with_capacity_in(path.len(), Global);
        text.push_str(path);
        self.paths.push((anchor, text.into_boxed_str(), file));
    }

    /// `file`, which is not UTF-8, is `bytes`.
    pub fn bytes(&mut self, file: FileId, bytes: &[u8]) {
        let mut owned = Vec::with_capacity_in(bytes.len(), Global);
        owned.extend_from_slice(bytes);
        self.bytes.push((file, owned.into_boxed_slice()));
    }

    fn seal(&mut self) {
        self.paths.sort_unstable_by(|a, b| (a.0, &*a.1).cmp(&(b.0, &*b.1)));
        self.bytes.sort_unstable_by_key(|(file, _)| *file);
    }
}

impl Db {
    /// The database of one task. `crates` are the memo's crate inputs in topological order,
    /// each with the key it was made with ([`Crate::key`]); `files` the task's file table (a
    /// span's [`FileId`] is an index into it); `records` sealed.
    pub fn new(
        memo: blaze_memo::Memo,
        records: Records,
        crates: Vec<(CrateItemKey, Crate), Global>,
        files: Vec<ContentId, Global>,
        mut includes: Includes,
        mut gifts: Gifts,
        settings: FrontSettings,
    ) -> Result<Db, DbError> {
        includes.seal();
        gifts.seal().map_err(DbError::GivenTwice)?;
        let mut by_key = crates.clone(); // clone: the task's crate list, once per task
        let crates: Vec<Crate, Global> = {
            let mut list = Vec::with_capacity_in(crates.len(), Global);
            list.extend(crates.iter().map(|(_, krate)| *krate));
            list
        };
        by_key.sort_unstable_by_key(|(key, _)| *key);
        if let Some(pair) = by_key.windows(2).find(|pair| pair[0].0 == pair[1].0) {
            return Err(DbError::CrateTwice { krate: pair[1].0 });
        }
        u32::try_from(files.len()).map_err(|_| DbError::TooManyFiles)?;
        let mut file_ids = Vec::with_capacity_in(files.len(), Global);
        file_ids.extend(files.iter().enumerate().map(|(at, &file)| (file, at as u32)));
        file_ids.sort_unstable_by_key(|(file, _)| *file);
        if let Some(pair) = file_ids.windows(2).find(|pair| pair[0].0 == pair[1].0) {
            return Err(DbError::FileTwice { file: pair[1].0 });
        }
        Ok(Db {
            memo,
            records,
            locals: Lock::new("base_db.locals", LocalTable::new_in(Global)),
            crates: std::sync::Arc::from(crates),
            by_key: by_key.into_boxed_slice(),
            files: files.into_boxed_slice(),
            file_ids: file_ids.into_boxed_slice(),
            includes,
            gifts,
            settings,
            nonce: Nonce::new(),
        })
    }

    /// The records every crate's questions read ([`blaze_surface::Items`]).
    #[inline]
    pub fn items(&self) -> &Records {
        &self.records
    }

    /// The records and the proof of the crate whose text the task holds, `None` for a task that
    /// holds only records: what decides, per question, whether an answer may be made from text
    /// (the crate is `krate`) or must be read (any other crate).
    #[inline]
    pub fn source_of(&self, krate: CrateItemKey) -> Option<(&Records, HasText)> {
        let proof = self.records.text_proof()?;
        (proof.krate() == krate).then_some((&self.records, proof))
    }

    /// The task's local number of an item.
    pub fn local(&self, key: ItemKey) -> Result<Local<AnyItem>, SurfaceError> {
        self.locals.lock().intern(key)
    }

    /// The item key of a local number of this task.
    pub fn key_of_local(&self, local: Local<AnyItem>) -> Result<ItemKey, SurfaceError> {
        self.locals.lock().key(local)
    }

    /// The task's table of local items, for a question of [`blaze_surface::Items`]'s `local_*`
    /// forms. Held for the question alone (rule 27: never across a wait).
    pub fn locals(&self) -> LockGuard<'_, LocalTable<AnyItem, Global>> {
        self.locals.lock()
    }

    /// The crate of a key, if the task holds it.
    pub fn crate_of_key(&self, krate: CrateItemKey) -> Option<Crate> {
        let at = self.by_key.binary_search_by_key(&krate, |(key, _)| *key).ok()?;
        Some(self.by_key[at].1)
    }

    /// The crate whose items' keys begin as `item`'s does.
    pub fn crate_of_item(&self, item: ItemKey) -> Option<Crate> {
        self.by_key.iter().find(|(key, _)| item.is_of(*key)).map(|(_, krate)| *krate)
    }

    /// The crates in topological order.
    #[inline]
    pub fn crates(&self) -> &std::sync::Arc<[Crate], Global> {
        &self.crates
    }

    /// The content of a file of the task's table, if it holds `file`.
    pub fn try_file_content(&self, file: FileId) -> Option<ContentId> {
        self.files.get(file.index() as usize).copied()
    }

    /// The content of a file of the task's table. Every [`FileId`] a span or a parse carries is
    /// of the table, so one that is not is a bug of the driver, which this names.
    pub fn file_content(&self, file: FileId) -> ContentId {
        match self.try_file_content(file) {
            Some(content) => content,
            None => panic!("file {} is not of the task's file table", file.index()),
        }
    }

    /// The number of a file of the task's table, by its content.
    pub fn file_id(&self, content: ContentId) -> Option<FileId> {
        let at = self.file_ids.binary_search_by_key(&content, |(file, _)| *file).ok()?;
        Some(FileId::from_raw(self.file_ids[at].1))
    }

    /// The text of a file of the crate the task compiles, as a handle whose
    /// [`FileText::text`] reads it. The read stops the task ([`crate::Need`]) for a file of
    /// another crate: its text is never the asker's to read (docs/v2/plan/design/bottom.md,
    /// point 5).
    pub fn file_text(&self, file: FileId) -> FileText {
        FileText::new(self, file)
    }

    /// Whether the task holds the text of `file`: a file of the crate it compiles.
    pub fn has_file_text(&self, file: FileId) -> bool {
        match (self.try_file_content(file), self.records.text_proof()) {
            (Some(content), Some(proof)) => self.records.shared_text(proof, content).is_ok(),
            _ => false,
        }
    }

    pub(crate) fn gifts(&self) -> &Gifts {
        &self.gifts
    }

    /// The file `path` names from its anchor's directory, if the include table holds it.
    pub fn resolve_path(&self, path: AnchoredPath<'_>) -> Option<FileId> {
        let paths = &self.includes.paths;
        let at = paths.binary_search_by(|row| (row.0, &*row.1).cmp(&(path.anchor, path.path))).ok()?;
        Some(paths[at].2)
    }

    /// The bytes of an included file: its text, or the bytes the table holds for one that is not
    /// UTF-8.
    pub fn included_bytes(&self, file: FileId) -> Option<&[u8]> {
        if self.has_file_text(file) {
            return Some(self.text_of(file).as_bytes());
        }
        let bytes = &self.includes.bytes;
        let at = bytes.binary_search_by_key(&file, |(at, _)| *at).ok()?;
        Some(&bytes[at].1)
    }

    fn text_of(&self, file: FileId) -> &syntax::Text {
        let content = self.file_content(file);
        let Some(proof) = self.records.text_proof() else {
            return Err(SurfaceError::NoFileRecord { fact: "text", file: content }).or_need();
        };
        self.records.shared_text(proof, content).or_need()
    }

    /// What tells two databases of one process apart.
    #[inline]
    pub fn nonce(&self) -> Nonce {
        self.nonce
    }
}

impl blaze_memo::Database for Db {
    #[inline]
    fn memo(&self) -> &blaze_memo::Memo {
        &self.memo
    }
}

/// A file's text as a memo value: what [`line_index`] and the parse are keyed by.
#[blaze_memo::interned(debug, unsafe(no_lifetime))]
pub struct FileText {
    #[returns(copy)]
    pub file: FileId,
}

impl FileText {
    /// The text: a file of the crate the task compiles, or a [`crate::Need`] stop.
    pub fn text(self, db: &Db) -> &syntax::Text {
        db.text_of(self.file(db))
    }
}

/// Where the lines of a file's text are ([`span::LineIndex`]): made the first time a
/// diagnostic, `line!`/`column!`, a caller location or debug info asks, once per text.
#[blaze_memo::tracked(returns(ref), owns)]
pub fn line_index(db: &Db, text: FileText) -> span::LineIndex {
    span::LineIndex::new(text.text(db))
}

/// The content of a set of cfg atoms: sorted by their bytes, so the set's order is no input.
pub fn cfg_options_content(cfg: &cfg::CfgOptions) -> ContentId {
    let mut atoms = Vec::new_in(Global);
    for atom in cfg {
        let mut bytes = Vec::new_in(Global);
        match atom {
            cfg::CfgAtom::Flag(name) => {
                bytes.push(0);
                put_part(&mut bytes, name.as_str().as_bytes());
            }
            cfg::CfgAtom::KeyValue { key, value } => {
                bytes.push(1);
                put_part(&mut bytes, key.as_str().as_bytes());
                put_part(&mut bytes, value.as_str().as_bytes());
            }
        }
        atoms.push(bytes);
    }
    atoms.sort_unstable();
    let mut hasher = blaze_ids::Hasher::new(CFG_OPTIONS_DOMAIN);
    hasher.update(&(atoms.len() as u64).to_le_bytes());
    for atom in &atoms {
        hasher.part(atom);
    }
    hasher.finish()
}

pub const CFG_OPTIONS_DOMAIN: &str = "blaze v2 cfg options 1";

fn put_part(bytes: &mut Vec<u8, Global>, part: &[u8]) {
    bytes.extend_from_slice(&(part.len() as u64).to_le_bytes());
    bytes.extend_from_slice(part);
}

/// The crates in topological order.
///
/// **Warning**: a reader of this reads every crate's input; a query of one item asks
/// [`Db::crate_of_key`] or the crate graph record instead.
pub fn all_crates(db: &Db) -> std::sync::Arc<[Crate], Global> {
    std::sync::Arc::clone(db.crates())
}

/// The crates that hold `file`: in the new layer a file is of one crate, the one whose text the
/// task holds, or of none the task can name.
pub fn relevant_crates(db: &Db, file: FileId) -> Vec<Crate, Global> {
    let mut out = Vec::new_in(Global);
    if db.try_file_content(file).is_some()
        && let Some(proof) = db.items().text_proof()
        && let Some(krate) = db.crate_of_key(proof.krate())
    {
        out.push(krate);
    }
    out
}
