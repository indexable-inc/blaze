//! [`Files`]: the world's files and source roots, the inputs a snapshot is made of them, and what
//! queries note about the files outside the VFS they read.
//!
//! The world side ([`Writer`], behind one lock no query takes) is plain owned data by stable
//! key: each file's text or bytes, appended module and source root by [`FileId`], each source
//! root's file set and content by [`SourceRootId`]. A writer compares what it is given with what
//! is held, writes only what differs and marks it ([`FileMarks`]); a commit reads the marks
//! ([`Files::marks`]), makes the dropped arenas' inputs again from what is held
//! ([`Files::make_root`] and the rest) and clears the marks once its snapshot is made. A query
//! reads the snapshot's rows (`db.snap().table`, one vector index) and the input there, never
//! the world.
//!
//! Each write of an open edit also keeps what it replaced, in the writer's undo log
//! ([`Files::begin`], [`Files::rollback`], [`Files::forget`]): a column's old value, a path's
//! old file, a root's file set as the edit found it (once per root per edit, the
//! copy-on-write copy it takes anyway), a root's trie writes, a named path's. An edit dropped
//! replays it backwards, O(its writes), and a root's content ID comes back the same: the trie's
//! shape is a function of its files. A file outside the VFS no row has is staged in the edit
//! and made a row at its commit ([`Files::publish_included`]), since rows are shared with
//! every snapshot's readers; a moved read of a row is a version no row names once the edit is
//! dropped. Held bytes ([`crate::IncludedFile::hold`]) are a cache of what is on disk, not a
//! write, and stay.
//!
//! A file outside the VFS that a macro read is a row of [`Files::included`]: its reads are
//! versions in [`Files::versions`] stamped by the commit that wrote them, so a reader holding an
//! older snapshot sees the read it saw ([`crate::IncludedFile`]). What a query notes
//! (`included_file`, `include_source`, `env_is_read`, `env_is_unset`) is a [`Table`] keyed by a
//! hash computed once: a lookup of what was noted already takes no lock.
//!
//! Cost (rule 67). A write is O(the bytes written) plus, for a file of a source root, the root's
//! content hashed again along the file's path in its trie (`O(log n)`, [`RootContent`]); an
//! equal write is a compare and marks nothing. A root's file set that gains or loses a file (a
//! view's edit, an alias) is not copied: the change is pending in the writer, read through by
//! every read before the commit ([`Written`]), and put in where the set is by the commit's last
//! step, once the old snapshot is released and before any query runs on the new one
//! ([`Files::apply_pending`]): O(the files that moved), the floor. The one case that copies
//! (`O(files of the root)`): a reader still holds the old snapshot through the commit, so its
//! input shares the set. A set given whole ([`Files::set_source_root`]) is the new set itself.
//! Making an arena's inputs is one input per file and root it holds, the floor. A named path
//! that moves marks the session ([`crate::named`]), as does an included source's read that
//! moves: every reader of a path may be any crate's, and an included source's parse is homed in
//! the session.

use std::{
    fmt,
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        OnceLock,
        atomic::{AtomicU32, Ordering},
    },
};

use blaze_core::lock::{Lock, LockGuard, ReadLock};

use crate::{AbsPath, AbsPathBuf, FileId, FileSet, VfsPath};
use blaze_format::cursor::Put;
use blaze_hash::HashMap;
use blaze_ids::{ContentId, Hasher};
use blaze_mem::{Frame, Tag, frame, scratch};
use blaze_memo::Inputs;
use smallvec::SmallVec;
use std::alloc::Global;
use triomphe::Arc;

use crate::{
    Db, FileSourceRootInput, FileText, GivenFile, IncludedFile, InputId, Looks, RawFile, ReadError,
    SourceRoot, SourceRootId, SourceRootInput, VfsFileRoot, VfsRoot, VfsText, content,
    handles::{included_id, note_included},
    named::Named,
    reparse::Reparse,
    table::{Log, MAX_FILE_ID, Table},
    view::ViewIndex,
    world::{CrateKey, FileMarks, Placed, facts},
};

#[cfg(test)]
mod tests;

/// The domain of an included file's fact id: of its path, as a key writes it ([`Files::ported`]).
pub(crate) const INCLUDED_PATH_DOMAIN: &str = "blaze v2 included path 1";

/// The domain of a source root's fact digest when its set has aliases: its content ID
/// ([`SourceRoot::DOMAIN`], which has no alias) and each alias with its file, so a root that
/// gains or loses an alias is another fact.
const ROOT_ALIASES_DOMAIN: &str = "blaze v2 source root aliases 1";

/// The source roots a setter changed: one, or a few for a file several roots have.
pub(crate) type Roots = SmallVec<[SourceRootId; 4]>;

/// Why [`Files::alias`] refused a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AliasError {
    /// The file is in no source root: an alias has the path of a file of one.
    NotInASourceRoot(FileId),
    /// The file is an alias: an alias is of the file itself.
    OfAnAlias(FileId),
    /// The file is a crate attribute's, which has no path.
    CrateAttr(FileId),
    /// The file is not an alias: only an alias's appended module is set this way.
    NotAnAlias(FileId),
    /// Every file ID is taken.
    NoFileId,
}

impl fmt::Display for AliasError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AliasError::NotInASourceRoot(file) => {
                write!(f, "{file:?} is in no source root, so it has no path to alias")
            }
            AliasError::OfAnAlias(file) => {
                write!(f, "{file:?} is an alias; alias the file it is of")
            }
            AliasError::CrateAttr(file) => {
                write!(f, "{file:?} is a crate attribute's file, which has no path")
            }
            AliasError::NotAnAlias(file) => write!(f, "{file:?} is not an alias"),
            AliasError::NoFileId => f.write_str("no file ID is left for an alias"),
        }
    }
}

impl std::error::Error for AliasError {}

/// The world's files and source roots, and what queries noted of the files outside the VFS.
/// Shared by every handle of the database (`Arc<Files>`), so a reader of one snapshot may run
/// while an edit writes the world.
pub struct Files {
    /// The world's files and roots. Every writer runs in an edit, which holds the database's
    /// `&mut`; no query takes this lock but to name a file in a panic or at a commit.
    writer: Lock<Writer>,
    /// The files outside the VFS that a macro read: `include_bytes!` and `include_str!` their
    /// bytes, `include!` its source as well ([`Included::source`]). The one home of what a
    /// macro read from the disk.
    included: Table<Included>,
    /// Every read of `included`, each with the one before it of its row.
    versions: Log<Version>,
    /// Which crate read which of `included` (by its index there).
    included_by: Table<(CrateKey, u32)>,
    /// Files a module declaration named that are not in the VFS, with the source root of the
    /// file that names them, and files a served def map names that its owner's source root
    /// lacks. An `include!` reads its file through `included` instead.
    included_sources: Table<(AbsPathBuf, SourceRootId)>,
    /// Which crate named which of `included_sources` (by its index there): a file is shared by
    /// crates through `#[path]`, so the source root of the file naming one does not tell the
    /// crates that did.
    sources_included_by: Table<(CrateKey, u32)>,
    /// The variables `env!` and `option_env!` have asked for, by the crate asking.
    env_read: Table<(CrateKey, Box<str>)>,
    /// The variables `env!` has found unset, by the crate asking.
    env_unset: Table<(CrateKey, Box<str>)>,
    /// What each proc-macro expansion read besides its call, by the crate the call is in and
    /// where the reads come from: a later note from the same place replaces them.
    expansions_read: ReadLock<HashMap<(CrateKey, ReadBy), Box<[ExpansionRead]>, Global>>,
    /// The files outside the VFS that a crate served from its records read when it was
    /// resolved, with the content ID each has now, which the records were found by
    /// ([`Files::included_are_served`]): no read of this database made them, so `included_by`
    /// has no row of them. Sorted by path.
    included_served: ReadLock<HashMap<CrateKey, Box<[(AbsPathBuf, ContentId)]>, Global>>,
    /// Every list of directories whose files are read from another directory that
    /// [`Files::set_view`] made, kept for readers that may hold one; `view` names the one in use.
    views: Log<Box<[View]>>,
    /// The index in `views` of the list in use, plus one; 0 for none.
    view: AtomicU32,
    /// How a key that must not say where this database's checkout is writes a path
    /// ([`Port`]): told once a database ([`Files::set_port`]), before any key is asked.
    port: OnceLock<std::sync::Arc<dyn Port>>,
    /// The one way to the disk for a file outside the VFS ([`Disk`]): `None` in a database
    /// opened as a snapshot ([`crate::Db::sealed`]), which therefore reads no file from disk.
    disk: Option<Disk>,
}

/// Leave to read a file outside the VFS from disk: a module's file no source root has, an
/// `include!`'s or `include_bytes!`'s file, read again by whoever watches the disk. Every read
/// of the disk here takes one ([`Files::included_file`], [`Files::included_source`], the
/// reloads), and only [`Files`] holds one, made with its database: a database made with none
/// ([`crate::Db::sealed`], what a command's snapshot is) reads nothing from disk, by
/// construction, and a `mod` or an `include!` whose file is not among its files is the
/// unresolved module or the include error that names it (rule 14: a snapshot is sans-IO).
#[derive(Debug)]
pub struct Disk {
    _made_by_files: (),
}

impl Files {
    /// The files of a database with no [`Disk`]: a snapshot's ([`crate::Db::sealed`]).
    pub(crate) fn sealed() -> Files {
        Files { disk: None, ..Files::default() }
    }

    /// This database's leave to read the disk: `None` in a snapshot ([`crate::Db::sealed`]).
    pub fn disk(&self) -> Option<&Disk> {
        self.disk.as_ref()
    }
}

/// How a path is written in a key that must not say where a checkout, its build directory or
/// the machine's shared directories are (docs/v2/plan/design/store.md, "Content keys"): each
/// of those directories, wherever it starts a path in `bytes`, as what it is (the workspace,
/// the build directory), so the same sources in another directory, under another home or on
/// another machine give the same bytes. The one implementation is blaze-usercode's roots
/// (`Roots::ported`), handed in by whoever knows the directories (blaze-cargo's
/// `serve::unit::portable`); a database told none writes every path as it is.
///
/// The invariant: for one set of directories, two byte strings are ported to the same bytes
/// only when they are equal (`Roots::unport` gives a ported string back), so a ported key
/// tells apart everything the unported one did but where the directories are.
///
/// A record's bytes are written the same way and read back against the reading database's
/// directories ([`Port::placed`]); what a record still holds of a directory after that is
/// found by [`Port::held`], the one searcher (`Roots::embedded`), so a record that names a
/// place is refused by the name of the place and never given to another checkout or home.
pub trait Port: Send + Sync + 'static {
    fn ported<'b>(&self, bytes: &'b [u8]) -> std::borrow::Cow<'b, [u8]>;

    /// What `bytes`, written by [`Port::ported`] under some directories, are under this
    /// database's: each directory's mark as its path here. An error naming the mark for
    /// one of a directory this database has none of, or bytes that are no ported string.
    fn placed<'b>(&self, bytes: &'b [u8]) -> Result<std::borrow::Cow<'b, [u8]>, String>;

    /// The directory whose path `bytes` hold anywhere, in any spelling, by what it is
    /// (`"workspace"`, `"fetched sources directory"`), the first found; `None` when they
    /// hold none. Wider than what [`Port::ported`] replaces, never narrower.
    fn held(&self, bytes: &[u8]) -> Option<&'static str>;
}

/// A [`Version`]'s `prev` when it is its row's first.
const NO_VERSION: u32 = u32::MAX;

/// A file outside the VFS that a macro read: its reads, newest first by `newest`.
pub(crate) struct Included {
    path: AbsPathBuf,
    /// The index in [`Files::versions`] of its newest read, published with release.
    newest: AtomicU32,
    /// The id of its fact ([`crate::world::included_fact`]), hashed the first time a read lists it.
    fact: OnceLock<ContentId>,
    /// The file `include!` parses it as, made the first time one does
    /// ([`Files::included_source`]).
    source: OnceLock<SourceOf>,
}

/// One read of an [`Included`] file, stamped by the commit whose snapshot first sees it.
pub(crate) struct Version {
    /// The read before it of its row, [`NO_VERSION`] for the first.
    prev: u32,
    /// The commit it was written for: a snapshot of a commit at or after it sees it.
    seq: u64,
    pub(crate) read: Result<std::sync::Arc<[u8]>, ReadError>,
    /// [`IncludedFile::content_of`] of `read`.
    pub(crate) content: ContentId,
    /// What `include!` parses of this read, made the first time one does.
    source: OnceLock<VersionSource>,
}

/// One read of an included source as `include!` parses it.
pub(crate) struct VersionSource {
    pub(crate) text: syntax::Text,
    /// [`FileText::content`].
    pub(crate) content: ContentId,
    pub(crate) reparse: Reparse,
    /// The content ID of its one-file source root.
    pub(crate) root_content: ContentId,
}

/// A file outside the VFS as `include!` reads it: a file of its own, with its text read from
/// its row's read, in a source root of its own that has it at its absolute path. So a span in
/// it, a path relative to it (a nested `include!`, rustc's `resolve_path` from the file the call
/// is in) and its path in an error resolve as in any file.
///
/// Its file ID and source root ID are both [`MAX_FILE_ID`] less its row
/// ([`crate::handles::included_id`]): counted down from the top, where the view's (counted up
/// from 0, [`Files::unused_file_id`]) never reach while one is there ([`Files::is_used`]).
pub(crate) struct SourceOf {
    pub(crate) is_library: bool,
    pub(crate) root: Arc<SourceRoot>,
    path: VfsPath,
}

/// The text an included source has for the read `read` of its file, the blaze-vfs ID of that
/// text's bytes, and its [`FileText::content`]: an empty text with [`FileText::no_text`]'s
/// content for a file that could not be read or is not UTF-8, which `include!` refuses before
/// it parses it.
fn source_text(
    read: &Result<std::sync::Arc<[u8]>, ReadError>,
) -> (syntax::Text, ContentId, ContentId) {
    let text = read.as_ref().ok().and_then(|bytes| {
        // clone: a count, the text shares the bytes
        let text = blaze_vfs::text_of(blaze_vfs::Bytes::from(std::sync::Arc::clone(bytes))).ok()?;
        Some((text, ContentId::of(blaze_vfs::FILE_DOMAIN, bytes)))
    });
    match text {
        Some((text, file)) => (text, file, FileText::content_of_file(file, None)),
        None => {
            (syntax::Text::from_static(""), ContentId::of(blaze_vfs::FILE_DOMAIN, b""), no_text())
        }
    }
}

/// The content ID of a source root that has only `file`, at `path`, with the content ID
/// `content`: by the one rule every source root's is ([`RootContent`]).
fn one_file_root_content(
    is_library: bool,
    file: FileId,
    path: &VfsPath,
    id: ContentId,
) -> ContentId {
    frame::task(Tag::Parse, |frame| {
        let mut files = Vec::with_capacity_in(1, frame);
        files.push((file, content::path_key(path), id));
        let root = SourceRootId(u32::MAX);
        let mut contents = HashMap::new_in(Global);
        contents.insert(root, RootContent::new(is_library, files, None, frame));
        RootContent::refresh(&mut contents, &[root], frame);
        contents.get(&root).expect("the root just refreshed").content
    })
}

/// The fact digest of a source root whose content ID is `content`: that, or, for a set with
/// aliases, that and its aliases ([`ROOT_ALIASES_DOMAIN`]). Of the set as the open edit has it,
/// its pending changes read through: what the commit puts in the set before any query runs.
fn root_digest(content: ContentId, root: Written<'_>) -> ContentId {
    let mut aliases: Vec<(u32, u32), Global> = Vec::new_in(Global);
    root.aliases(|alias, of| aliases.push((alias.index(), of.index())));
    if aliases.is_empty() {
        return content;
    }
    aliases.sort_unstable();
    let mut hasher = Hasher::new(ROOT_ALIASES_DOMAIN);
    hasher.id(&content);
    crate::content::len(&mut hasher, aliases.len());
    for (alias, of) in aliases {
        hasher.update(&alias.to_le_bytes()).update(&of.to_le_bytes());
    }
    hasher.finish()
}

/// A change of a source root's file set made where it is, kept pending until the commit
/// ([`Writer::pend`]): each is the [`SourceRoot`] method of its name.
#[derive(Debug)]
pub(crate) enum SetOp {
    /// [`SourceRoot::insert_file`].
    Insert(FileId, VfsPath),
    /// [`SourceRoot::remove_file`].
    Remove(FileId),
    /// [`SourceRoot::alias_file`]: the alias, then its file.
    Alias(FileId, FileId),
}

impl SetOp {
    /// Makes the change in `set`.
    fn apply(self, set: &mut SourceRoot) {
        match self {
            SetOp::Insert(file, path) => set.insert_file(file, path),
            SetOp::Remove(file) => _ = set.remove_file(file),
            SetOp::Alias(alias, of) => _ = set.alias_file(alias, of),
        }
    }
}

/// A source root's file set as the open edit has it: the set the last commit left, with the
/// edit's pending changes read through, answering as the set will once the commit puts them
/// in ([`FileSet`]'s rules replayed over the changes: a path names the last file put at it, a
/// file has the last path it was put at, a file taken out takes its aliases with it, an alias
/// is made only of a file in the set and only for a file that is not). Each read is
/// O(the pending changes), a few an edit, and allocates nothing.
#[derive(Clone, Copy)]
pub(crate) struct Written<'a> {
    set: &'a Arc<SourceRoot>,
    ops: &'a [SetOp],
}

impl<'a> Written<'a> {
    /// The set as the last commit left it, without the pending changes.
    pub(crate) fn set(self) -> &'a SourceRoot {
        self.set
    }

    /// Whether the edit has changes of the set pending.
    pub(crate) fn has_pending(self) -> bool {
        !self.ops.is_empty()
    }

    /// The path `file` was put at (never its file's, for an alias) after the first `upto`
    /// changes.
    fn own_path(self, file: FileId, upto: usize) -> Option<&'a VfsPath> {
        for op in self.ops[..upto].iter().rev() {
            match op {
                SetOp::Insert(it, path) if *it == file => return Some(path),
                SetOp::Remove(it) if *it == file => return None,
                _ => {}
            }
        }
        self.set.own_path(&file)
    }

    /// The file `alias` is an alias of after every change.
    fn alias_target(self, alias: FileId) -> Option<FileId> {
        let mut target = self.set.alias_target(&alias);
        for (at, op) in self.ops.iter().enumerate() {
            match *op {
                SetOp::Insert(it, _) | SetOp::Remove(it) if it == alias => target = None,
                // A file taken out takes its aliases with it.
                SetOp::Remove(it) if Some(it) == target => target = None,
                SetOp::Alias(it, of) if it == alias => {
                    if self.own_path(of, at).is_some() && self.own_path(alias, at).is_none() {
                        target = Some(of);
                    }
                }
                _ => {}
            }
        }
        target
    }

    /// The path of `file`, or of the file it is an alias of ([`SourceRoot::path_for_file`]).
    pub(crate) fn path_for_file(self, file: FileId) -> Option<&'a VfsPath> {
        let end = self.ops.len();
        self.own_path(file, end).or_else(|| self.own_path(self.alias_target(file)?, end))
    }

    /// The file at `path` ([`SourceRoot::file_for_path`]).
    pub(crate) fn file_for_path(self, path: &VfsPath) -> Option<FileId> {
        let mut found = self.set.file_for_path(path).copied();
        for (at, op) in self.ops.iter().enumerate() {
            match op {
                SetOp::Insert(file, put) if put == path => found = Some(*file),
                // A path keeps naming the file it named unless that file is taken out of it.
                SetOp::Remove(file) if found == Some(*file) && self.own_path(*file, at) == Some(path) => {
                    found = None;
                }
                _ => {}
            }
        }
        found
    }

    /// Hands `each` every alias with its file, in no order ([`SourceRoot::aliases`]).
    pub(crate) fn aliases(self, mut each: impl FnMut(FileId, FileId)) {
        for (alias, _) in self.set.aliases() {
            if let Some(of) = self.alias_target(alias) {
                each(alias, of);
            }
        }
        for (at, op) in self.ops.iter().enumerate() {
            let SetOp::Alias(alias, _) = *op else { continue };
            // Once each: an alias the set has, or one an earlier change made, was handed above.
            let earlier = self.set.alias_target(&alias).is_some()
                || self.ops[..at].iter().any(|it| matches!(*it, SetOp::Alias(other, _) if other == alias));
            if earlier {
                continue;
            }
            if let Some(of) = self.alias_target(alias) {
                each(alias, of);
            }
        }
    }

    /// Whether the set is `other` ([`Files::set_source_root`]'s check of a set given again).
    /// With no pending change a compare in place; with some, the set copied with them put in.
    fn is(self, other: &SourceRoot) -> bool {
        match self.ops.is_empty() {
            true => **self.set == *other,
            false => self.applied() == *other,
        }
    }

    /// The set with its pending changes put in, copied: for a read that wants the whole set
    /// before the commit ([`Files::written_root`], a set made again from its files), never on
    /// the path of a view's edit.
    pub(crate) fn applied(self) -> SourceRoot {
        // clone: a whole set, only for a read of all of it while its changes are pending
        let mut set = SourceRoot::clone(self.set);
        for op in self.ops {
            let op = match op {
                // clone: the path, into the copy
                SetOp::Insert(file, path) => SetOp::Insert(*file, path.clone()),
                SetOp::Remove(file) => SetOp::Remove(*file),
                SetOp::Alias(alias, of) => SetOp::Alias(*alias, *of),
            };
            op.apply(&mut set);
        }
        set
    }
}

/// A checkout that is looked at as the one it was loaded as with other contents.
#[derive(Debug, Clone)] // clone: setup, `set_view` copies the few other views once
struct View {
    base: PathBuf,
    over: PathBuf,
    built: Vec<PathBuf>,
}

/// A file's text as the world holds it.
#[derive(Clone)] // clone: counts, kept by an edit's undo of a write of it ([`ByFile`])
pub(crate) struct TextEntry {
    pub(crate) text: syntax::Text,
    /// The blaze-vfs ID of the text's bytes.
    pub(crate) vfs: ContentId,
    /// [`FileText::content`]: of `vfs` and the file's appended module.
    pub(crate) content: ContentId,
    /// What its parse is made from, the edit from the text before found ([`Reparse::after`]).
    pub(crate) reparse: Reparse,
}

/// The bytes of a file that is not UTF-8, as the world holds them.
#[derive(Clone)] // clone: a count, kept by an edit's undo of a write of it ([`ByFile`])
pub(crate) struct RawEntry {
    pub(crate) read: Result<std::sync::Arc<[u8]>, ReadError>,
    pub(crate) content: ContentId,
}

/// The world's files and roots, read and written only with [`Files::writer`] held.
#[derive(Default)]
pub(crate) struct Writer {
    /// Each file's text, while it has one.
    pub(crate) texts: ByFile<TextEntry>,
    /// Each file of the VFS whose bytes are not UTF-8, while it has no text.
    pub(crate) raws: ByFile<RawEntry>,
    /// The content each file known by its content alone was given ([`Files::set_file_given`]):
    /// what [`Files::file_content`] answers while it has neither text nor bytes.
    pub(crate) given: ByFile<ContentId>,
    /// The module the compiler appends to each crate root that has one
    /// ([`FileText::appended_root_module`]): kept while the file has no text.
    pub(crate) appended: ByFile<&'static str>,
    /// Each file's source root ([`Files::file_source_root`]).
    pub(crate) file_roots: ByFile<SourceRootId>,
    /// The files whose source root each root is, in no order: what [`Files::make_root`] places.
    pub(crate) by_root: HashMap<SourceRootId, Vec<FileId, Global>, Global>,
    /// Each source root's file set as the last commit left it (or as the open edit gave it
    /// whole): the open edit's changes made where the set is are in `pending`, never here.
    pub(crate) roots: HashMap<SourceRootId, Arc<SourceRoot>, Global>,
    /// The open edit's changes of each source root's file set made where it is (a view's files
    /// added and taken out, an alias made), in order: read through by every read of the set
    /// before its commit ([`Writer::written`]), cleared by a rollback, and put in the set by
    /// the commit, once it can no longer fail ([`Files::apply_pending`]).
    pending: HashMap<SourceRootId, Vec<SetOp, Global>, Global>,
    /// The content IDs of each source root's files, from which its own is kept.
    pub(crate) root_contents: HashMap<SourceRootId, RootContent, Global>,
    /// The source roots whose [`RootContent`] has each file.
    pub(crate) in_roots: ByFile<SmallVec<[SourceRootId; 1]>>,
    /// Every path a source root has named, with its file: a path keeps its file for good.
    pub(crate) by_path: HashMap<VfsPath, FileId, Global>,
    /// The blaze-vfs content ID each file was last set from by [`Files::set_view_root`].
    pub(crate) view_ids: ByFile<ContentId>,
    /// Where [`Files::unused_file_id`] starts looking for an unused file ID.
    pub(crate) next_file: u32,
    /// The crate whose crate attribute each file holds ([`crate::crate_attrs`]).
    pub(crate) crate_attrs: ByFile<CrateKey>,
    /// Files of crate attributes taken out, with no text: the next attribute's file is one of
    /// these.
    pub(crate) free_crate_attrs: Vec<FileId, Global>,
    /// The index of each view base's paths that [`Files::set_view_root`] keeps.
    pub(crate) views: HashMap<AbsPathBuf, ViewIndex, Global>,
    /// `by_path` as queries read it, across source roots ([`crate::named`]).
    pub(crate) named: Named,
    /// The aliases of each file ([`Files::alias`]), in the order they were made.
    pub(crate) aliases: ByFile<SmallVec<[FileId; 2]>>,
    /// The file each alias has the path and text of.
    pub(crate) alias_of: ByFile<FileId>,
    /// The module each alias appends to its file's text, with the text it appends
    /// ([`Files::set_alias_appended`]).
    pub(crate) suffixes: ByFile<(&'static str, Box<str>)>,
    /// What moved since the last commit.
    pub(crate) marks: FileMarks,
    /// Bytes set for files outside the VFS no row has yet ([`Files::set_included_file`]): made
    /// rows at the commit ([`Files::publish_included`]), so an edit dropped leaves no row.
    pub(crate) staged_included: Vec<(AbsPathBuf, std::sync::Arc<[u8]>), Global>,
    /// The open edit's writes of the maps above, the last last: what [`Writer::rollback`]
    /// undoes. The columns keep their own ([`ByFile`]), as do the roots' contents
    /// ([`RootContent::rollback`]) and the named paths ([`Named::rollback`]).
    undo: Vec<Undo, Global>,
    /// Each source root's file set as the open edit found it, kept at its first write of the
    /// set whole ([`Writer::put_root`]; `None`: it had none). A change made where the set is
    /// keeps nothing here: it is pending ([`Writer::pend`]), and a rollback just drops it.
    roots_before: HashMap<SourceRootId, Option<Arc<SourceRoot>>, Global>,
    /// What the open edit found of the few words written whole ([`Writer::begin`]).
    begun: Begun,
}

/// A write of [`Writer`]'s maps, with what it replaced: see [`Writer::rollback`].
pub(crate) enum Undo {
    /// The file was put in the root's `by_root` list.
    RootFilePut(SourceRootId, FileId),
    /// The file was taken out of the root's `by_root` list.
    RootFileTaken(SourceRootId, FileId),
    /// The root's [`RootContent`] was replaced whole: what it held (`None`: none).
    RootContent(SourceRootId, Option<RootContent>),
    /// The path was given a file: the file it had (`None`: no file).
    Path(VfsPath, Option<FileId>),
    /// The included row's newest read moved: the index in [`Files::versions`] it had.
    IncludedNewest(u32, u32),
}

/// What an edit found of the words a writer sets whole ([`Writer::begin`]).
#[derive(Default)]
struct Begun {
    next_file: u32,
    free_crate_attrs: Vec<FileId, Global>,
    /// The marks the edit found: none, but those a commit's last step left for the next
    /// ([`Files::publish_included`]).
    marks: FileMarks,
}

impl Writer {
    /// The source roots that have `file`.
    pub(crate) fn roots_of(&self, file: FileId) -> &[SourceRootId] {
        self.in_roots.get(file).map_or(&[], |it| it.as_slice())
    }

    /// The aliases of `file`, copied out so the writer can be written while they are gone over.
    pub(crate) fn aliases_of(&self, file: FileId) -> SmallVec<[FileId; 2]> {
        // clone: a file's few aliases, inline
        self.aliases.get(file).cloned().unwrap_or_default()
    }

    /// Notes that `file`'s inputs moved.
    #[inline]
    pub(crate) fn mark(&mut self, file: FileId) {
        self.marks.files.push(file);
    }

    /// The content ID `file` has in its source root ([`Files::file_content`]).
    pub(crate) fn content_of(&self, file: FileId) -> ContentId {
        match (self.texts.get(file), self.raws.get(file), self.given.get(file)) {
            (Some(text), _, _) => text.content,
            (None, Some(raw), _) => raw.content,
            (None, None, Some(&given)) => given,
            (None, None, None) => no_text(),
        }
    }

    /// Notes that a source root names `file` at `path`.
    pub(crate) fn note_path(&mut self, path: &VfsPath, file: FileId) {
        match self.by_path.get(path) {
            Some(&known) if known == file => {}
            Some(&known) => {
                // A path given another file: every view's index of paths is made again.
                // clone: the one copy of a path, made when a source root first names it or moves it.
                self.by_path.insert(path.clone(), file);
                // clone: as above, kept for undo
                self.undo.push(Undo::Path(path.clone(), Some(known)));
                self.views.clear();
                self.named.note(path, file);
            }
            None => {
                // clone: the one copy of a path, made when a source root first names it or moves it.
                self.by_path.insert(path.clone(), file);
                // clone: as above, kept for undo
                self.undo.push(Undo::Path(path.clone(), None));
                for (base, index) in &mut self.views {
                    index.note(base, path, file);
                }
                self.named.note(path, file);
            }
        }
    }

    /// Begins an edit: what it finds of the words set whole is kept, and its writes are kept
    /// for [`Writer::rollback`] from here on. O(1) but the crate attributes' spare files
    /// (a few) and the named paths' sums (a fixed 3 KiB).
    pub(crate) fn begin(&mut self) {
        debug_assert!(self.undo.is_empty() && self.roots_before.is_empty(), "an edit begins with none open");
        let mut free = Vec::with_capacity_in(self.free_crate_attrs.len(), Global);
        free.extend_from_slice(&self.free_crate_attrs);
        // clone: the marks, empty but for what a commit left for the next
        self.begun = Begun { next_file: self.next_file, free_crate_attrs: free, marks: self.marks.clone() };
        self.named.begin();
    }

    /// Ends the open edit, its writes kept: once a commit has made its snapshot.
    pub(crate) fn forget(&mut self) {
        let pending = self.pending.iter().find(|(_, ops)| !ops.is_empty()).map(|(root, _)| *root);
        if let Some(root) = pending {
            panic!(
                "the file set of the source root {root:?} has changes pending past its edit's \
                 commit: a commit puts them in (`Files::apply_pending`) before it forgets the edit"
            );
        }
        for column in self.columns() {
            column.forget();
        }
        self.undo.clear();
        self.roots_before.clear();
        self.named.forget();
        for content in self.root_contents.values_mut() {
            content.forget();
        }
    }

    /// Each column, for [`Writer::forget`] and [`Writer::rollback`] to go over its undo.
    fn columns(&mut self) -> [&mut dyn Column; 11] {
        [
            &mut self.texts,
            &mut self.raws,
            &mut self.given,
            &mut self.appended,
            &mut self.file_roots,
            &mut self.in_roots,
            &mut self.view_ids,
            &mut self.crate_attrs,
            &mut self.aliases,
            &mut self.alias_of,
            &mut self.suffixes,
        ]
    }

    /// Puts back what the open edit wrote, the last write first: the writer as the edit found
    /// it, but the view indexes (a cache of `by_path`, made again on their next use) and the
    /// rows of [`Files::versions`] written (unreachable: each included row's newest is put
    /// back, returned here for the caller to store, last first). O(the edit's writes), plus a
    /// hash of each root content put back along the paths it moved.
    pub(crate) fn rollback(&mut self) -> Vec<(u32, u32), Global> {
        let mut newest = Vec::new_in(Global);
        for column in self.columns() {
            column.rollback();
        }
        while let Some(undo) = self.undo.pop() {
            match undo {
                Undo::RootFilePut(root, file) => {
                    if let Some(files) = self.by_root.get_mut(&root)
                        && let Some(at) = files.iter().position(|it| *it == file)
                    {
                        files.swap_remove(at);
                    }
                }
                Undo::RootFileTaken(root, file) => {
                    self.by_root.entry(root).or_insert_with(|| Vec::new_in(Global)).push(file);
                }
                Undo::RootContent(root, Some(content)) => _ = self.root_contents.insert(root, content),
                Undo::RootContent(root, None) => _ = self.root_contents.remove(&root),
                Undo::Path(path, Some(file)) => _ = self.by_path.insert(path, file),
                Undo::Path(path, None) => _ = self.by_path.remove(&path),
                Undo::IncludedNewest(row, prev) => newest.push((row, prev)),
            }
        }
        for (root, set) in self.roots_before.drain() {
            match set {
                Some(set) => _ = self.roots.insert(root, set),
                None => _ = self.roots.remove(&root),
            }
        }
        // The pending changes were never put in a set: dropped, the lists' capacity kept.
        for ops in self.pending.values_mut() {
            ops.clear();
        }
        let mut moved: Vec<SourceRootId, Global> = Vec::new_in(Global);
        for (&root, content) in &mut self.root_contents {
            if content.rollback() {
                moved.push(root);
            }
        }
        if !moved.is_empty() {
            frame::task(Tag::Parse, |frame| RootContent::refresh(&mut self.root_contents, &moved, frame));
        }
        let begun = std::mem::take(&mut self.begun);
        self.next_file = begun.next_file;
        self.free_crate_attrs = begun.free_crate_attrs;
        self.views.clear();
        self.named.rollback();
        self.marks.set_to(&begun.marks);
        self.staged_included.clear();
        newest
    }

    /// Sets the file set of `root`, kept for undo at the edit's first write; its pending changes
    /// are dropped with the set they were of.
    pub(crate) fn put_root(&mut self, root: SourceRootId, set: Arc<SourceRoot>) {
        self.keep_root(root);
        self.drop_pending(root);
        self.roots.insert(root, set);
    }

    /// Keeps `root`'s file set for undo, at the edit's first write of it.
    pub(crate) fn keep_root(&mut self, root: SourceRootId) {
        if !self.roots_before.contains_key(&root) {
            // clone: a count, the set as the edit found it
            let before = self.roots.get(&root).cloned();
            self.roots_before.insert(root, before);
        }
    }

    /// `root`'s file set as the open edit has it, its pending changes read through.
    pub(crate) fn written(&self, root: SourceRootId) -> Option<Written<'_>> {
        let set = self.roots.get(&root)?;
        let ops = self.pending.get(&root).map_or(&[][..], |it| &it[..]);
        Some(Written { set, ops })
    }

    /// Puts `op` on `root`'s pending changes ([`Writer::pending`]); the caller marks the root.
    /// The set itself is not touched, so a snapshot that holds it, and the edit's undo, need no
    /// copy. Allocates only past the list's capacity, which is kept across edits.
    pub(crate) fn pend(&mut self, root: SourceRootId, op: SetOp) {
        assert!(
            self.roots.contains_key(&root),
            "a change of the file set of the source root {root:?}, which the world lacks"
        );
        self.pending.entry(root).or_insert_with(|| Vec::new_in(Global)).push(op);
    }

    /// Whether `root` has pending changes.
    fn has_pending(&self, root: SourceRootId) -> bool {
        self.pending.get(&root).is_some_and(|it| !it.is_empty())
    }

    /// Drops `root`'s pending changes: its set is given whole ([`Writer::put_root`]), so they
    /// changed a set no longer kept.
    fn drop_pending(&mut self, root: SourceRootId) {
        if let Some(ops) = self.pending.get_mut(&root) {
            ops.clear();
        }
    }

    /// Puts `file` in `root`'s `by_root` list.
    fn put_root_file(&mut self, root: SourceRootId, file: FileId) {
        self.by_root.entry(root).or_insert_with(|| Vec::new_in(Global)).push(file);
        self.undo.push(Undo::RootFilePut(root, file));
    }

    /// Takes `file` out of `root`'s `by_root` list, if it is there.
    fn take_root_file(&mut self, root: SourceRootId, file: FileId) {
        if let Some(files) = self.by_root.get_mut(&root)
            && let Some(at) = files.iter().position(|it| *it == file)
        {
            files.swap_remove(at);
            self.undo.push(Undo::RootFileTaken(root, file));
        }
    }

    /// Sets the text of `file` (with the blaze-vfs ID of its bytes and its
    /// [`FileText::content`]), or that it has none (`None`), and returns its content ID in a
    /// source root. A text equal to the one held keeps it and what its parse is made from, and
    /// marks nothing; another has its edit from the old one found, for its next parse
    /// ([`Reparse::after`]). The source roots that have the file are left to the caller.
    pub(crate) fn write_text(
        &mut self,
        file: FileId,
        text: Option<(syntax::Text, ContentId, ContentId)>,
    ) -> ContentId {
        if self.raws.remove(file).is_some() {
            self.mark(file);
        }
        let Some((text, vfs, content)) = text else {
            if self.texts.remove(file).is_some() {
                self.mark(file);
            }
            return no_text();
        };
        // An equal text is read, not written: a write in place keeps the old entry for undo.
        if self.texts.get(file).is_some_and(|entry| entry.text == text && entry.content == content) {
            return content;
        }
        let moved = match self.texts.get_mut(file) {
            Some(entry) => {
                let mut moved = false;
                if entry.text != text {
                    entry.reparse = entry.reparse.after(&text, vfs);
                    entry.text = text;
                    entry.vfs = vfs;
                    moved = true;
                }
                if entry.content != content {
                    entry.content = content;
                    moved = true;
                }
                moved
            }
            None => {
                let reparse = Reparse::new(&text, vfs);
                self.texts.insert(file, TextEntry { text, vfs, content, reparse });
                true
            }
        };
        if moved {
            self.mark(file);
        }
        content
    }

    /// Sets `file` to a file with no text whose bytes (not UTF-8) are `bytes`, and returns its
    /// content ID in a source root ([`Files::file_content`]): the view loader's write of such a
    /// file. The source roots that have the file are left to the caller.
    pub(crate) fn write_bytes(&mut self, file: FileId, bytes: &blaze_vfs::Bytes) -> ContentId {
        debug_assert!(
            std::str::from_utf8(&bytes[..]).is_err(),
            "{file:?}: bytes that are text are its text"
        );
        if self.texts.remove(file).is_some() {
            self.mark(file);
        }
        let content = IncludedFile::content_of(Ok(&bytes[..]));
        if self.raws.get(file).is_some_and(|it| it.content == content) {
            return content;
        }
        // clone: a count when the view's bytes are shared
        let shared = bytes.shared().cloned();
        // alloc: a copy of a mapped image's range, once per write of a file that is not text
        let read = Ok(shared.unwrap_or_else(|| std::sync::Arc::from(&bytes[..])));
        self.raws.insert(file, RawEntry { read, content });
        self.mark(file);
        content
    }

    /// Puts `file` in the source root `root`; marks it, and the root it left, when it moved.
    pub(crate) fn set_file_root(&mut self, file: FileId, root: SourceRootId) {
        let old = self.file_roots.get(file).copied();
        if old == Some(root) {
            return;
        }
        self.file_roots.insert(file, root);
        if let Some(old) = old {
            self.take_root_file(old, file);
            self.marks.roots.push(old);
        }
        self.put_root_file(root, file);
        self.mark(file);
    }

    /// Takes `file` out of its source root, if it has one; marks it and the root it left.
    pub(crate) fn clear_file_root(&mut self, file: FileId) {
        let Some(old) = self.file_roots.remove(file) else { return };
        self.take_root_file(old, file);
        self.marks.roots.push(old);
        self.mark(file);
    }


    /// Brings the content IDs of `roots` up to date and marks them.
    pub(crate) fn write_root_contents(&mut self, roots: &[SourceRootId], frame: &Frame) {
        RootContent::refresh(&mut self.root_contents, roots, frame);
        self.marks.roots.extend_from_slice(roots);
    }

    /// Gives each alias of `of` the text `of` is given (`None`: no text) with its own suffix
    /// ([`Writer::write_alias`]), and hands `written` each alias.
    pub(crate) fn write_aliases(
        &mut self,
        of: FileId,
        text: Option<(&syntax::Text, ContentId)>,
        mut written: impl FnMut(FileId),
    ) {
        if !self.aliases.contains_key(of) {
            return;
        }
        for alias in self.aliases_of(of) {
            self.write_alias(alias, text);
            written(alias);
        }
    }

    /// Sets `alias`'s text to its file's (`of_text`, with the blaze-vfs ID of its bytes) and its
    /// suffix after it, and its appended root module to the suffix's: with no suffix, the
    /// alias's text is its file's bytes, shared.
    fn write_alias(&mut self, alias: FileId, of_text: Option<(&syntax::Text, ContentId)>) {
        let suffix = self.suffixes.get(alias);
        let module = suffix.map(|(module, _)| *module);
        let text = of_text.map(|(text, file)| match suffix {
            // clone: a count of the bytes
            None => (text.clone(), file, FileText::content_of_file(file, None)),
            Some((module, suffix)) => {
                // alloc: an appended alias's text, once per write of its file or its suffix
                let mut whole = String::with_capacity(text.len() + suffix.len());
                whole.push_str(text);
                whole.push_str(suffix);
                let file = ContentId::of(blaze_vfs::FILE_DOMAIN, whole.as_bytes());
                (syntax::Text::from(whole), file, FileText::content_of_file(file, Some(module)))
            }
        });
        self.write_text(alias, text);
        let held = self.appended.get(alias).copied();
        if held != module {
            match module {
                Some(module) => self.appended.insert(alias, module),
                None => _ = self.appended.remove(alias),
            }
            self.mark(alias);
        }
    }

    /// Places `file`'s inputs in the arena being made: its text or bytes, and its source root.
    fn make_file(&self, inputs: &Inputs<'_>, file: FileId, placed: &mut Placed<'_>) {
        if let Some(text) = self.texts.get(file) {
            let appended = self.appended.get(file).copied();
            let input = VfsText::new(
                inputs,
                facts::text(file, text.content),
                // clone: a count of the bytes, the input shares them
                text.text.clone(),
                file,
                appended,
                InputId(text.content),
                // clone: the one slot the world's entry and the input share
                text.reparse.clone(),
            );
            placed.text(file, input);
        } else if let Some(raw) = self.raws.get(file) {
            let input = RawFile::new(
                inputs,
                facts::raw(file, raw.content),
                // clone: a count of the bytes
                raw.read.clone(),
                InputId(raw.content),
            );
            placed.raw(file, input);
        } else if let Some(&given) = self.given.get(file) {
            let input = GivenFile::new(inputs, facts::given(file, given), InputId(given));
            placed.given(file, input);
        }
        if let Some(&root) = self.file_roots.get(file) {
            let input = VfsFileRoot::new(inputs, facts::file_root(file, root), root);
            placed.file_root(file, input);
        }
    }
}

/// A setter's value per file, by [`FileId::index`]: a direct index where a hash map took a hash
/// and a probe. File IDs are dense, so it costs `size_of::<Option<T>>()` bytes per ID up to the
/// largest one set (33 for a [`ContentId`]).
///
/// Every write keeps the value it replaced in `undo` until the edit ends ([`ByFile::rollback`],
/// [`ByFile::forget`]): one entry per write, the old value moved out (a removal, an insert) or
/// copied (a write in place, [`ByFile::get_mut`] and [`ByFile::slot`], of a `T` cheap to copy).
pub(crate) struct ByFile<T> {
    rows: Vec<Option<T>, Global>,
    /// Each write of the open edit: the file's index and the value it had before.
    undo: Vec<(u32, Option<T>), Global>,
}

impl<T> Default for ByFile<T> {
    fn default() -> ByFile<T> {
        ByFile { rows: Vec::new_in(Global), undo: Vec::new_in(Global) }
    }
}

impl<T> ByFile<T> {
    #[inline]
    pub(crate) fn get(&self, file: FileId) -> Option<&T> {
        self.rows.get(file.index() as usize)?.as_ref()
    }

    pub(crate) fn contains_key(&self, file: FileId) -> bool {
        self.get(file).is_some()
    }

    /// One past the largest file ID it has a place for.
    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    /// `file`'s place, the column grown to it; nothing kept for undo.
    fn place(&mut self, file: FileId) -> &mut Option<T> {
        let at = file.index() as usize;
        if at >= self.rows.len() {
            // alloc: once per new largest file ID, amortized by `Vec`'s doubling.
            self.rows.resize_with(at + 1, || None);
        }
        &mut self.rows[at]
    }

    pub(crate) fn insert(&mut self, file: FileId, value: T) {
        let old = std::mem::replace(self.place(file), Some(value));
        self.undo.push((file.index(), old));
    }

    /// Each file with a value, by file ID.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (FileId, &T)> + '_ {
        self.rows
            .iter()
            .enumerate()
            .filter_map(|(at, it)| Some((FileId::from_raw(at as u32), it.as_ref()?)))
    }

}

/// A [`ByFile`] column's undo, whatever its values: what [`Writer::forget`] and
/// [`Writer::rollback`] go over.
trait Column {
    /// Ends the open edit, its writes kept: what a commit does once its snapshot is made.
    fn forget(&mut self);

    /// Puts back what each write of the open edit replaced, the last first: the column as it
    /// was when the edit began (its length aside, which only holds `None` past it).
    fn rollback(&mut self);
}

impl<T> Column for ByFile<T> {
    fn forget(&mut self) {
        self.undo.clear();
    }

    fn rollback(&mut self) {
        while let Some((at, old)) = self.undo.pop() {
            self.rows[at as usize] = old;
        }
    }
}

/// The writes that hand the caller the value or a place in it: the undo keeps a copy of the
/// old value, so `T` is one cheap to copy (a count, an ID, a few inline words).
impl<T: Clone> ByFile<T> {
    /// `file`'s value to write in place: copied for undo first, so a caller that may write
    /// nothing reads [`ByFile::get`] before it asks.
    pub(crate) fn get_mut(&mut self, file: FileId) -> Option<&mut T> {
        let at = file.index();
        let old = self.rows.get(at as usize)?.as_ref()?.clone(); // clone: kept for undo
        self.undo.push((at, Some(old)));
        self.rows[at as usize].as_mut()
    }

    /// `file`'s place, the column grown to it, its value copied for undo first.
    pub(crate) fn slot(&mut self, file: FileId) -> &mut Option<T> {
        let old = self.get(file).cloned(); // clone: kept for undo
        self.undo.push((file.index(), old));
        self.place(file)
    }

    pub(crate) fn remove(&mut self, file: FileId) -> Option<T> {
        let old = self.rows.get_mut(file.index() as usize)?.take()?;
        // clone: kept for undo, the caller has the value
        self.undo.push((file.index(), Some(old.clone())));
        Some(old)
    }
}

impl Default for Files {
    fn default() -> Files {
        Files {
            writer: Lock::new("db.files.writer", Default::default()),
            included: Table::new(1 << 22),
            versions: Log::new(1 << 24),
            included_by: Table::new(1 << 22),
            included_sources: Table::new(1 << 22),
            sources_included_by: Table::new(1 << 22),
            env_read: Table::new(1 << 22),
            env_unset: Table::new(1 << 22),
            expansions_read: ReadLock::new("db.files.expansions_read", Default::default()),
            included_served: ReadLock::new("db.files.included_served", Default::default()),
            views: Log::new(1 << 16),
            view: AtomicU32::new(0),
            port: OnceLock::new(),
            disk: Some(Disk { _made_by_files: () }),
        }
    }
}

impl fmt::Debug for Files {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Files").finish_non_exhaustive()
    }
}

/// The row `at` of a snapshot's table.
#[inline]
fn row<T: Copy>(rows: &[Option<T>], at: u32) -> Option<T> {
    rows.get(at as usize).copied().flatten()
}

// The read side: what a query asks of a snapshot.
impl Files {
    #[track_caller]
    pub(crate) fn lock(&self) -> LockGuard<'_, Writer> {
        self.writer.lock()
    }

    /// The text of `file_id` in `db`'s snapshot. A file with none (its bytes are not UTF-8, or
    /// it has neither text nor bytes) is a bug of the caller, which panics naming the file's
    /// path and why it has none: a reader of every file of a source root reads
    /// [`Files::file_content`] instead.
    pub fn file_text(&self, db: &Db, file_id: FileId) -> FileText {
        if let Some(text) = row(&db.snap().table.texts, file_id.index()) {
            return FileText::Vfs(text);
        }
        match self.source_row(file_id.index()) {
            Some(at) => FileText::Included(at),
            None => self.no_text(db, file_id),
        }
    }

    /// [`Files::file_text`]'s panic for `file_id`, which has no text: its path, if a source root
    /// named one, and why it has none.
    #[cold]
    #[inline(never)]
    fn no_text(&self, db: &Db, file_id: FileId) -> ! {
        let table = &db.snap().table;
        let why = match (row(&table.raws, file_id.index()), row(&table.given, file_id.index())) {
            (Some(_), _) => {
                "its bytes are not UTF-8, so it has no text (read its content with `file_content`, its bytes with `raw_file`)"
            }
            (None, Some(_)) => {
                "it is known by its content alone (a file of a crate given by its address), and its text was never set"
            }
            (None, None) => {
                "it has neither text nor bytes in this snapshot: it was never set, its text was taken away, or it was set after the snapshot's commit"
            }
        };
        // A writer may hold the lock: then the path is left out rather than waited for.
        let path = self.writer.try_lock().and_then(|writer| {
            // alloc: a panic's message
            writer.by_path.iter().find(|(_, it)| **it == file_id).map(|(path, _)| path.to_string())
        });
        match path {
            Some(path) => panic!(
                "no text for the file `{path}` ({file_id:?}): {why}; this is a bug of the reader"
            ),
            None => panic!(
                "no text for {file_id:?}, a file no source root named: {why}; this is a bug of the reader"
            ),
        }
    }

    /// Whether the file was loaded as text. A file of the VFS that is not UTF-8 has an id and
    /// no text.
    pub fn has_file_text(&self, db: &Db, file_id: FileId) -> bool {
        row(&db.snap().table.texts, file_id.index()).is_some()
            || self.source_row(file_id.index()).is_some()
    }

    /// The bytes of a file of the VFS that has no text (they are not UTF-8), as the view loader
    /// gave them: what `include_bytes!` and `include_str!` read of it. `None` for a file with
    /// text, and for one with neither (an alias of a file without text).
    pub fn raw_file(&self, db: &Db, file_id: FileId) -> Option<IncludedFile> {
        row(&db.snap().table.raws, file_id.index()).map(IncludedFile::Vfs)
    }

    /// The content ID `file` has in its source root, the one every reader that hashes a file
    /// takes: its text's ([`FileText::content`]); for a file with no text, its bytes'
    /// ([`IncludedFile::content`] of [`Files::raw_file`]); for one known by its content alone,
    /// the content it was given ([`GivenFile`], [`Files::set_file_given`]); for one with none of
    /// these, [`FileText::no_text`].
    ///
    /// The invariant: two files of a crate given by its address share a content here only where
    /// the record names them with equal content (or their texts are equal), and none of them is
    /// [`FileText::no_text`]; so no reader that finds something by a file's content (an entry
    /// set, a file's index, a macro's tokens) finds one file's under another's.
    pub fn file_content(&self, db: &Db, file: FileId) -> ContentId {
        let table = &db.snap().table;
        if let Some(text) = row(&table.texts, file.index()) {
            return text.content(db).id();
        }
        if let Some(raw) = row(&table.raws, file.index()) {
            return raw.content(db).id();
        }
        if let Some(given) = row(&table.given, file.index()) {
            return given.content(db).id();
        }
        match self.source_row(file.index()) {
            Some(at) => FileText::Included(at).content(db),
            None => FileText::no_text(),
        }
    }

    /// The source root `id` in `db`'s snapshot.
    pub fn source_root(&self, db: &Db, id: SourceRootId) -> SourceRootInput {
        if let Some(root) = row(&db.snap().table.roots, id.0) {
            return SourceRootInput::Vfs(root);
        }
        match self.source_row(id.0) {
            Some(at) => SourceRootInput::Included(at),
            None => panic!(
                "Unable to fetch `SourceRootInput` with `SourceRootId` ({id:?}): this snapshot has no such root; this is a bug"
            ),
        }
    }

    /// The source root of `id` in `db`'s snapshot.
    pub fn file_source_root(&self, db: &Db, id: FileId) -> FileSourceRootInput {
        if let Some(root) = row(&db.snap().table.file_roots, id.index()) {
            return FileSourceRootInput::Vfs(root);
        }
        match self.source_row(id.index()) {
            Some(at) => FileSourceRootInput::Included(at),
            None => panic!(
                "Unable to get `FileSourceRootInput` with `FileId` ({id:?}, path: {}): this snapshot puts it in no source root; this is a bug",
                self.path_of(id)
                    // alloc: the panic's message.
                    .map_or_else(|| "<unknown>".to_owned(), |path| path.to_string()),
            ),
        }
    }

    /// The included row whose source has the file ID or source root ID `raw`, once one does.
    #[inline]
    fn source_row(&self, raw: u32) -> Option<u32> {
        let at = MAX_FILE_ID.checked_sub(raw)?;
        self.included.try_get(at)?.source.get()?;
        Some(at)
    }

    /// The read of row `row` that a snapshot of commit `seq` sees: the latest stamped at or
    /// before it, else the first. A walk back over the versions newer than `seq`: none in a
    /// process with one snapshot.
    fn version_at(&self, row: u32, seq: u64) -> &Version {
        let mut at = self.included.get(row).newest.load(Ordering::Acquire);
        // Bounded: each step goes to an older version of the row, and the first has none.
        loop {
            let version = self.versions.get(at).expect("a row names only versions published");
            if version.seq <= seq || version.prev == NO_VERSION {
                return version;
            }
            debug_assert!(version.prev < at, "a version's previous is older");
            at = version.prev;
        }
    }

    /// The newest read of row `row`: what the world holds.
    fn newest(&self, row: u32) -> &Version {
        self.version_at(row, u64::MAX)
    }

    /// The id of row `row`'s fact: of its path as a key writes it.
    fn fact_id(&self, row: u32) -> ContentId {
        let included = self.included.get(row);
        *included.fact.get_or_init(|| {
            let path = self.ported(included.path.as_str().as_bytes());
            crate::world::included_fact(&path, ContentId::default()).id
        })
    }

    /// The read of row `row` that `db`'s snapshot sees, listed as read by the running query.
    pub(crate) fn included_read(&self, db: &Db, row: u32) -> &Version {
        let version = self.version_at(row, db.seq());
        note_included(db, self.fact_id(row), version.content);
        version
    }

    /// The text `include!` parses of the read of row `row` that `db`'s snapshot sees, listed
    /// as read by the running query.
    pub(crate) fn included_text(&self, db: &Db, row: u32) -> &VersionSource {
        let version = self.included_read(db, row);
        self.version_source(row, version)
    }

    /// The one-file source root of row `row`'s included source.
    pub(crate) fn included_root(&self, row: u32) -> &SourceOf {
        self.included
            .get(row)
            .source
            .get()
            .unwrap_or_else(|| panic!("the included row {row} has no source: nothing parsed it"))
    }

    /// `version` of row `row` as `include!` parses it, made the first time one asks: its edit
    /// from the version before found when that one was parsed ([`Reparse::after`]). Two
    /// readers that ask first at once each make it; the first kept.
    fn version_source<'a>(&'a self, row: u32, version: &'a Version) -> &'a VersionSource {
        version.source.get_or_init(|| {
            let of = self.included_root(row);
            let (text, vfs, content) = source_text(&version.read);
            let before = match version.prev {
                NO_VERSION => None,
                prev => self.versions.get(prev).and_then(|it| it.source.get()),
            };
            let reparse = match before {
                Some(before) => before.reparse.after(&text, vfs),
                None => Reparse::new(&text, vfs),
            };
            let file = FileId::from_raw(included_id(row));
            let root_content = one_file_root_content(of.is_library, file, &of.path, content);
            VersionSource { text, content, reparse, root_content }
        })
    }
}

// The world side: what a commit asks of the world's files.
impl Files {
    /// What moved since the last commit, copied: a commit takes them only once its snapshot is
    /// made ([`Files::clear_marks`]), so one that fails keeps them for the edit's undo.
    /// O(the marks).
    pub(crate) fn marks(&self) -> FileMarks {
        self.lock().marks.clone() // clone: the marks, O(what the edit wrote)
    }

    /// The source roots whose file set the open edit wrote, given whole
    /// ([`Writer::keep_root`]) or changed where it is ([`Writer::pend`]), sorted: where a
    /// crate's root file's path may have moved. A text's write is not one. Allocates nothing
    /// when there is none.
    pub(crate) fn roots_written(&self) -> Vec<SourceRootId, Global> {
        let writer = self.lock();
        let pending = writer.pending.iter().filter(|(_, ops)| !ops.is_empty()).map(|(root, _)| *root);
        let mut roots = Vec::new_in(Global);
        roots.extend(writer.roots_before.keys().copied().chain(pending));
        roots.sort_unstable_by_key(|it| it.0);
        roots.dedup();
        roots
    }

    /// Puts each source root's pending changes in its set ([`Writer::pend`]), in the input of
    /// `snap`, the snapshot the commit just made, where its root's input holds it: the commit's
    /// step after its last that can fail, once the old snapshot is released (the database
    /// holds `snap` alone) and before any query runs on `snap` ([`blaze_memo::Memo::input_mut`]
    /// refuses after). The world holds no count of the set while it is written (it takes its
    /// own out first, and a copy of the input's after), so the set is written where it is:
    /// copied only when a reader still holds the old snapshot, whose input shares the set,
    /// the one case that copies. O(the changes) but that copy.
    ///
    /// # Panics
    ///
    /// Where `snap` has a reader already, or its table has no input for a root with changes
    /// (the commit made every changed root's input again: each change marks its root).
    pub(crate) fn apply_pending(&self, snap: &mut std::sync::Arc<crate::world::Snap, Global>) {
        let mut writer = self.lock();
        if !writer.pending.values().any(|ops| !ops.is_empty()) {
            return;
        }
        let snap = std::sync::Arc::get_mut(snap).unwrap_or_else(|| {
            panic!("a commit puts its file-set changes in a snapshot a reader already holds")
        });
        let writer = &mut *writer;
        for (&root, ops) in writer.pending.iter_mut() {
            if ops.is_empty() {
                continue;
            }
            let input = row(&snap.table.roots, root.0).unwrap_or_else(|| {
                panic!("the commit made no input for the source root {root:?}, whose file set changed")
            });
            // The world's count out, so the input's is the set's last but a reader's.
            let mine = writer
                .roots
                .remove(&root)
                .unwrap_or_else(|| panic!("the source root {root:?} has changes and no set"));
            let set = input.source_root_mut(&mut snap.memo);
            debug_assert!(Arc::ptr_eq(&mine, set), "the input of {root:?} holds the world's set");
            drop(mine);
            // clone: only while a reader holds the old snapshot, whose input shares the set
            let files = Arc::make_mut(set);
            for op in ops.drain(..) {
                op.apply(files);
            }
            // clone: a count, the world's again
            writer.roots.insert(root, Arc::clone(set));
        }
    }

    /// Takes the marks: the commit's snapshot holds what they say moved. Cleared in place, so
    /// the next edit's marks are written into the capacity this one's grew: a warm keystroke
    /// allocates no mark.
    pub(crate) fn clear_marks(&self) {
        self.lock().marks.clear();
    }

    /// Begins an edit of the files ([`Writer::begin`]).
    pub(crate) fn begin(&self) {
        self.lock().begin();
    }

    /// Ends the open edit, its writes kept ([`Writer::forget`]).
    pub(crate) fn forget(&self) {
        self.lock().forget();
    }

    /// Puts back what the open edit wrote ([`Writer::rollback`]), and each included row's
    /// newest read it moved.
    pub(crate) fn rollback(&self) {
        let newest = self.lock().rollback();
        // The last write first: a row moved twice ends at what the edit found.
        for (row, prev) in newest {
            self.included.get(row).newest.store(prev, Ordering::Release);
        }
    }

    /// Whether anything moved since the last commit ([`Files::marks`] has a mark).
    pub(crate) fn has_marks(&self) -> bool {
        let writer = self.lock();
        let marks = &writer.marks;
        !(marks.files.is_empty() && marks.roots.is_empty() && !marks.session && marks.included.is_empty())
    }

    /// The source root `file` is in, when that root is set.
    pub(crate) fn root_of(&self, file: FileId) -> Option<SourceRootId> {
        let writer = self.lock();
        let root = *writer.file_roots.get(file)?;
        writer.roots.contains_key(&root).then_some(root)
    }

    /// Every source root set, sorted.
    pub(crate) fn roots(&self) -> Vec<SourceRootId, Global> {
        let writer = self.lock();
        let mut roots = Vec::with_capacity_in(writer.roots.len(), Global);
        roots.extend(writer.roots.keys().copied());
        roots.sort_unstable_by_key(|it| it.0);
        roots
    }

    /// The source root `root` as the writer has it now, written since the last commit or not
    /// ([`crate::Edit::written_source_root`]).
    pub(crate) fn written_root(&self, root: SourceRootId) -> Option<Arc<SourceRoot>> {
        let writer = self.lock();
        let written = writer.written(root)?;
        Some(match writer.has_pending(root) {
            // alloc: the set with the edit's pending changes in, for a read of all of it
            true => Arc::new(written.applied()),
            // clone: a count, once per read of a root between two writes
            false => Arc::clone(written.set),
        })
    }

    /// `file`'s text and its content ID in a source root as the writer has them now
    /// ([`crate::Edit::written_file_text`]): none for a file with no text.
    pub(crate) fn written_text(&self, file: FileId) -> Option<(syntax::Text, ContentId)> {
        let writer = self.lock();
        // clone: a count of the text's bytes, once per read of a text between two writes
        writer.texts.get(file).map(|it| (it.text.clone(), it.content))
    }

    /// Whether the source root `root` is a library's.
    pub(crate) fn root_is_library(&self, root: SourceRootId) -> bool {
        self.lock().roots.get(&root).is_some_and(|it| it.is_library)
    }

    /// The path `file` has in its source root, or else in a root that has it.
    pub(crate) fn path_of(&self, file: FileId) -> Option<VfsPath> {
        let writer = self.lock();
        let first = writer.file_roots.get(file).into_iter();
        // clone: a path the caller keeps (a crate's root, a panic's message)
        first.chain(writer.roots_of(file)).find_map(|root| {
            writer.written(*root)?.path_for_file(file).cloned()
        })
    }

    /// Makes the source root `root`'s input in the arena being made, and the inputs of each
    /// file whose root it is but the crate attributes' (made in their crates' arenas).
    pub(crate) fn make_root(&self, inputs: &Inputs<'_>, root: SourceRootId, placed: &mut Placed<'_>) {
        let writer = self.lock();
        let written = writer
            .written(root)
            .unwrap_or_else(|| panic!("the world has no source root {root:?} to make"));
        let content = writer
            .root_contents
            .get(&root)
            .expect("a source root's content is kept from when it is set")
            .content;
        // The fact is of the set as the commit leaves it (its pending changes read through);
        // the input holds the set as it is, which the commit's last step brings to that
        // (`Files::apply_pending`) before any query reads it.
        let fact = facts::root(root, root_digest(content, written));
        // clone: a count, the input's
        placed.root(root, VfsRoot::new(inputs, fact, Arc::clone(written.set), InputId(content)));
        let mut files: Vec<FileId, Global> = Vec::new_in(Global);
        if let Some(of) = writer.by_root.get(&root) {
            files.extend(of.iter().copied().filter(|&it| !writer.crate_attrs.contains_key(it)));
        }
        files.sort_unstable_by_key(|it| it.index());
        for file in files {
            writer.make_file(inputs, file, placed);
        }
    }

    /// Makes the inputs of `files` (crate attributes' files) in the arena being made.
    pub(crate) fn make_files(&self, inputs: &Inputs<'_>, files: &[FileId], placed: &mut Placed<'_>) {
        let writer = self.lock();
        for &file in files {
            writer.make_file(inputs, file, placed);
        }
    }

    /// Makes the inputs of every file in no source root that is set, but the crate attributes'
    /// (`is_attr`), in the session arena being made, by file ID.
    pub(crate) fn make_rootless(
        &self,
        inputs: &Inputs<'_>,
        is_attr: &dyn Fn(FileId) -> bool,
        placed: &mut Placed<'_>,
    ) {
        let writer = self.lock();
        let len = writer
            .texts
            .len()
            .max(writer.raws.len())
            .max(writer.given.len())
            .max(writer.file_roots.len());
        for at in 0..len as u32 {
            let file = FileId::from_raw(at);
            let has = writer.texts.contains_key(file)
                || writer.raws.contains_key(file)
                || writer.given.contains_key(file)
                || writer.file_roots.contains_key(file);
            if !has || is_attr(file) || writer.crate_attrs.contains_key(file) {
                continue;
            }
            if writer.file_roots.get(file).is_some_and(|root| writer.roots.contains_key(root)) {
                continue;
            }
            writer.make_file(inputs, file, placed);
        }
    }

    /// Makes the session arena's inputs of the files: the named paths ([`crate::named`]).
    pub(crate) fn make_session(&self, inputs: &Inputs<'_>) {
        self.lock().named.make(inputs);
    }

    /// The crates that read the included row `row`: whose arenas a read that moved drops.
    pub(crate) fn included_readers(&self, row: u32) -> Vec<CrateKey, Global> {
        let mut readers = Vec::new_in(Global);
        readers.extend(self.included_by.iter().filter(|(_, at)| *at == row).map(|&(by, _)| by));
        readers
    }

    /// The crate whose crate attribute `file` holds, as the world has it.
    pub(crate) fn attr_owner(&self, file: FileId) -> Option<CrateKey> {
        self.lock().crate_attrs.get(file).copied()
    }
}

// The writers: each takes the lock, writes what differs and marks it.
impl Files {
    pub(crate) fn set_file_text(&self, file_id: FileId, text: &str) {
        frame::task(Tag::Parse, |frame| {
            let mut writer = self.lock();
            self.refuse_crate_attr(&writer, file_id);
            if let Some(of) = writer.alias_of.get(file_id) {
                panic!("{file_id:?} is an alias of {of:?}: its text is {of:?}'s, set that");
            }
            writer.view_ids.remove(file_id);
            let appended = writer.appended.get(file_id).copied();
            let file = ContentId::of(blaze_vfs::FILE_DOMAIN, text.as_bytes());
            let content = FileText::content_of_file(file, appended);
            let text = syntax::Text::from(text);
            writer.write_aliases(file_id, Some((&text, file)), |_| {});
            writer.write_text(file_id, Some((text, file, content)));
            let mut changed = Roots::new();
            index_file(&mut writer, file_id, content, &mut changed);
            writer.write_root_contents(&changed, frame);
        })
    }

    /// Sets the module the compiler appends to the crate root `file_id`'s text, see
    /// [`FileText::appended_root_module`]. Marks nothing if it is that already.
    ///
    /// # Panics
    ///
    /// For an alias (set with [`Files::set_alias_appended`]) and for a file with no text.
    pub(crate) fn set_appended_root_module(&self, file_id: FileId, module: Option<&'static str>) {
        frame::task(Tag::Parse, |frame| {
            let mut writer = self.lock();
            if let Some(of) = writer.alias_of.get(file_id) {
                panic!(
                    "{file_id:?} is an alias of {of:?}: its module is set with `Edit::set_alias_appended`"
                );
            }
            if writer.appended.get(file_id).copied() == module {
                return;
            }
            let Some(vfs) = writer.texts.get(file_id).map(|it| it.vfs) else {
                panic!(
                    "`set_appended_root_module` of {file_id:?}, which has no text: set its text first"
                );
            };
            match module {
                Some(module) => writer.appended.insert(file_id, module),
                None => _ = writer.appended.remove(file_id),
            }
            let content = FileText::content_of_file(vfs, module);
            writer.texts.get_mut(file_id).expect("as above").content = content;
            writer.mark(file_id);
            let mut changed = Roots::new();
            index_file(&mut writer, file_id, content, &mut changed);
            writer.write_root_contents(&changed, frame);
        })
    }

    /// Has `alias` append `appended` (its module's name, and the text after its file's) to its
    /// file's text from now on, or nothing (`None`), and writes its text so: a write of its
    /// file keeps the suffix, writing the file's new text with it after.
    pub(crate) fn set_alias_appended(
        &self,
        alias: FileId,
        appended: Option<(&'static str, &str)>,
    ) -> Result<(), AliasError> {
        let mut writer = self.lock();
        let Some(&of) = writer.alias_of.get(alias) else {
            return Err(AliasError::NotAnAlias(alias));
        };
        let now = writer.suffixes.get(alias).map(|(module, text)| (*module, &**text));
        if now == appended {
            return Ok(());
        }
        match appended {
            // alloc: an alias's suffix, once per change of it
            Some((module, text)) => writer.suffixes.insert(alias, (module, text.into())),
            None => _ = writer.suffixes.remove(alias),
        }
        // clone: a count of the bytes
        let text = writer.texts.get(of).map(|it| (it.text.clone(), it.vfs));
        let of_text = text.as_ref().map(|(text, file)| (text, *file));
        writer.write_alias(alias, of_text);
        Ok(())
    }

    /// A new file with `of`'s path and text (see [`FileSet::alias`]): in each source root that
    /// has `of`, found by file and never by path, so `mod` declarations in it resolve as in
    /// `of` while every path still names `of`. It keeps `of`'s text (every write of `of` writes
    /// its aliases in the same edit) and goes when `of` leaves its last source root, but its
    /// appended root module is its own.
    pub(crate) fn alias(&self, of: FileId) -> Result<FileId, AliasError> {
        let mut writer = self.lock();
        if writer.crate_attrs.contains_key(of) {
            return Err(AliasError::CrateAttr(of));
        }
        if writer.alias_of.contains_key(of) {
            return Err(AliasError::OfAnAlias(of));
        }
        // alloc: inline, a file's few source roots
        let roots: Roots = writer.roots_of(of).iter().copied().collect();
        let Some(&file_root) = writer.file_roots.get(of) else {
            return Err(AliasError::NotInASourceRoot(of));
        };
        if roots.is_empty() {
            return Err(AliasError::NotInASourceRoot(of));
        }
        let mut next = writer.next_file;
        let alias = self.unused_file_id(&writer, &mut next).ok_or(AliasError::NoFileId)?;
        writer.next_file = next;
        for &root in &roots {
            // Pending until the commit, as a view's edit is: the set is never copied for it.
            writer.pend(root, SetOp::Alias(alias, of));
            writer.marks.roots.push(root);
        }
        writer.set_file_root(alias, file_root);
        // clone: a count of the bytes, which the alias shares
        let text = writer.texts.get(of).map(|it| (it.text.clone(), it.vfs));
        let of_text = text.as_ref().map(|(text, file)| (text, *file));
        writer.write_alias(alias, of_text);
        writer.alias_of.insert(alias, of);
        writer.aliases.slot(of).get_or_insert_default().push(alias);
        Ok(alias)
    }

    /// The aliases of `of` ([`Files::alias`]), in the order they were made.
    pub fn aliases_of(&self, of: FileId) -> SmallVec<[FileId; 2]> {
        self.lock().aliases_of(of)
    }

    /// The file `alias` is an alias of, if it is one.
    pub fn alias_of(&self, alias: FileId) -> Option<FileId> {
        self.lock().alias_of.get(alias).copied()
    }

    /// Takes out the aliases of each of `files` that is in no source root now: each loses its
    /// text, and is handed to `removed`.
    pub(crate) fn drop_orphan_aliases(
        &self,
        writer: &mut Writer,
        files: impl IntoIterator<Item = FileId>,
        mut removed: impl FnMut(FileId),
    ) {
        for of in files {
            if !writer.roots_of(of).is_empty() {
                continue;
            }
            let Some(aliases) = writer.aliases.remove(of) else { continue };
            for alias in aliases {
                writer.alias_of.remove(alias);
                writer.suffixes.remove(alias);
                if writer.appended.remove(alias).is_some() {
                    writer.mark(alias);
                }
                writer.write_text(alias, None);
                removed(alias);
            }
        }
    }

    pub(crate) fn set_source_root(&self, source_root_id: SourceRootId, mut source_root: Arc<SourceRoot>) {
        frame::task(Tag::Parse, |frame| {
            let mut writer = self.lock();
            for file in source_root.iter() {
                self.refuse_crate_attr(&writer, file);
            }
            keep_aliases(&writer, &mut source_root);
            if writer.written(source_root_id).is_some_and(|it| it.is(&source_root)) {
                return;
            }
            self.index_root(&mut writer, source_root_id, &source_root, frame);
            RootContent::refresh(&mut writer.root_contents, &[source_root_id], frame);
            writer.put_root(source_root_id, source_root);
            writer.marks.roots.push(source_root_id);
            let mut orphans: SmallVec<[FileId; 4]> = SmallVec::new();
            orphans.extend(
                writer
                    .aliases
                    .iter()
                    .map(|(of, _)| of)
                    .filter(|&of| writer.roots_of(of).is_empty()),
            );
            self.drop_orphan_aliases(&mut writer, orphans, |_| {});
            if writer.named.publish(frame) {
                writer.marks.session = true;
            }
        })
    }

    /// Puts `id` in the source root `source_root_id`; the root it left is marked too.
    pub(crate) fn set_file_source_root(&self, id: FileId, source_root_id: SourceRootId) {
        let mut writer = self.lock();
        self.refuse_crate_attr(&writer, id);
        writer.set_file_root(id, source_root_id);
    }

    /// Has `id` known by `content` alone ([`GivenFile`]): what [`Files::file_content`] answers
    /// for it while it has neither text nor bytes, which a write of either takes precedence over
    /// (and a removal of them falls back to). `None` takes it away. Marks the file when it moved.
    pub(crate) fn set_file_given(&self, id: FileId, content: Option<ContentId>) {
        frame::task(Tag::Parse, |frame| {
            let mut writer = self.lock();
            self.refuse_crate_attr(&writer, id);
            if writer.given.get(id).copied() == content {
                return;
            }
            match content {
                Some(content) => writer.given.insert(id, content),
                None => _ = writer.given.remove(id),
            }
            writer.mark(id);
            let now = writer.content_of(id);
            let mut changed = Roots::new();
            index_file(&mut writer, id, now, &mut changed);
            writer.write_root_contents(&changed, frame);
        })
    }

    /// Whether anything has `id`: a file ID [`Files::set_view_root`] may not give a new path.
    pub(crate) fn is_used(&self, writer: &Writer, id: FileId) -> bool {
        writer.texts.contains_key(id)
            || writer.raws.contains_key(id)
            || writer.given.contains_key(id)
            || writer.file_roots.contains_key(id)
            || writer.in_roots.contains_key(id)
            || writer.view_ids.contains_key(id)
            || writer.crate_attrs.contains_key(id)
            || writer.alias_of.contains_key(id)
            || self.source_row(id.index()).is_some()
    }

    /// The first file ID from `next` on that nothing has: the one place base-db hands out file
    /// IDs, for a view's new files ([`Files::set_view_root`]), crate attributes' files
    /// ([`crate::crate_attrs`]) and callers that number their own ([`Files::new_file_id`]).
    pub(crate) fn unused_file_id(&self, writer: &Writer, next: &mut u32) -> Option<FileId> {
        while *next <= MAX_FILE_ID {
            let id = FileId::from_raw(*next);
            *next += 1;
            if !self.is_used(writer, id) {
                return Some(id);
            }
        }
        None
    }

    /// A file ID nothing has and no earlier call gave, for a caller that numbers files of its
    /// own (a VFS reload, the doctest builder): set after a crate graph with crate attributes,
    /// it never meets an attribute's file or a view's. `None` once every file ID is taken.
    pub(crate) fn new_file_id(&self) -> Option<FileId> {
        let mut writer = self.lock();
        let mut next = writer.next_file;
        let id = self.unused_file_id(&writer, &mut next);
        writer.next_file = next;
        id
    }

    /// Whether the world has the source root `id`.
    pub(crate) fn has_source_root(writer: &Writer, id: SourceRootId) -> bool {
        writer.roots.contains_key(&id)
    }

    /// Puts in the content of the source root `id` as `root` has it, noting which of its files
    /// and paths it has; the content it replaces is kept for undo. Nodes whose files are what
    /// they were keep their hashes.
    fn index_root(&self, writer: &mut Writer, id: SourceRootId, root: &SourceRoot, frame: &Frame) {
        let mut left = Vec::new_in(frame);
        if let Some(old) = writer.root_contents.get(&id) {
            left.extend(old.files());
        }
        for file in left {
            if writer.in_roots.get(file).is_some_and(|it| it.contains(&id))
                && let Some(roots) = writer.in_roots.get_mut(file)
            {
                roots.retain(|it| *it != id);
            }
        }
        let mut files = Vec::with_capacity_in(root.len(), frame);
        let mut ids = 0;
        for file in root.iter() {
            ids += 1;
            let Some(path) = root.path_for_file(&file) else { continue };
            // A path two files were given is the last one's.
            if root.file_for_path(path) != Some(&file) {
                continue;
            }
            writer.note_path(path, file);
            let roots = writer.in_roots.slot(file).get_or_insert_default();
            if !roots.contains(&id) {
                roots.push(id);
            }
            files.push((file, content::path_key(path), writer.content_of(file)));
        }
        // Every file whose path names it back, and as many paths as files.
        let one_to_one = files.len() == ids && ids == root.len();
        let old = writer.root_contents.remove(&id);
        let mut content = RootContent::new(root.is_library, files, old.as_ref(), frame);
        content.one_to_one = one_to_one;
        writer.undo.push(Undo::RootContent(id, old));
        writer.root_contents.insert(id, content);
    }

    /// Has the files under `base` read from where they are under `over` from now on, but for
    /// those under one of `built`: what a build writes is not part of a checkout. Nothing is
    /// read again for it: whoever watches the disk does that.
    pub fn set_view(&self, base: &Path, over: &Path, built: Vec<PathBuf>) {
        // Held so that two of these don't each drop the other's change.
        let _writer = self.lock();
        let mut views: Vec<View> =
            self.views_in_use().iter().filter(|it| it.base != base).cloned().collect(); // alloc: setup: a new list of a few views, published once.
        if base != over {
            // alloc: as above.
            views.push(View { base: base.to_owned(), over: over.to_owned(), built });
        }
        let raw = self.views.push(views.into_boxed_slice());
        self.view.store(raw + 1, Ordering::Release);
    }

    fn views_in_use(&self) -> &[View] {
        match self.view.load(Ordering::Acquire) {
            0 => &[],
            plus_one => self.views.get(plus_one - 1).map_or(&[], |it| &**it),
        }
    }

    /// Where the file known as `path` is read from.
    pub fn on_disk(&self, path: &Path) -> PathBuf {
        let over = self
            .views_in_use()
            .iter()
            .filter(|it| !it.built.iter().any(|built| path.starts_with(built)))
            .find_map(|it| Some(it.over.join(path.strip_prefix(&it.base).ok()?)));
        // alloc: the edge: the path is handed to `std::fs::read` on a miss.
        over.unwrap_or_else(|| path.to_owned())
    }

    /// [`Files::on_disk`] into `out`, emptied first: for a caller that asks it of every file on
    /// every look and keeps one buffer for it.
    pub fn on_disk_into(&self, path: &Path, out: &mut PathBuf) {
        out.as_mut_os_string().clear();
        let over = self
            .views_in_use()
            .iter()
            .filter(|it| !it.built.iter().any(|built| path.starts_with(built)))
            .find_map(|it| Some((&it.over, path.strip_prefix(&it.base).ok()?)));
        match over {
            Some((over, rest)) => {
                out.push(over);
                out.push(rest);
            }
            None => out.push(path),
        }
    }
}

// Files outside the VFS.
impl Files {
    /// The files outside the VFS that `include!`, `include_bytes!` or `include_str!` named, in
    /// no order.
    pub fn included_paths(&self) -> impl Iterator<Item = &AbsPath> + '_ {
        self.included.iter().map(|it| it.path.as_path())
    }

    /// A file outside the VFS that `include!`, `include_bytes!` or `include_str!` names, read
    /// from disk the first time it is asked for, its read stamped with `db`'s commit. Whoever
    /// watches the disk keeps it current with [`Files::reload_included_file`]. Two readers that
    /// ask for it first at once each read it, with no lock held; the first to finish is kept.
    pub fn included_file(&self, disk: &Disk, db: &Db, krate: CrateKey, path: &AbsPath) -> IncludedFile {
        IncludedFile::Read(self.included_at(disk, db, krate, path))
    }

    /// The row in [`Files::included`] of [`Files::included_file`]'s file, noted as read by
    /// `krate`.
    fn included_at(&self, disk: &Disk, db: &Db, krate: CrateKey, path: &AbsPath) -> u32 {
        let hash = self.included.hash(path);
        let at = match self.included.find(hash, |it| it.path.as_path() == path) {
            Some(at) => at,
            None => {
                // A first read is made inside a query (`include_bytes!`'s expansion), which
                // no one build owns: its look is kept by none. Each build asks of every
                // path read here again with its own looks ([`crate::Edit::reload_included_files`],
                // [`IncludedFile::content_now`]).
                let read = self.read_included(disk, path, &Looks::none());
                let content = IncludedFile::content_of(read.as_deref());
                let seq = db.seq();
                let make = || {
                    let version = Version {
                        prev: NO_VERSION,
                        seq,
                        read,
                        content,
                        source: OnceLock::new(),
                    };
                    Included {
                        path: path.to_path_buf(),
                        newest: AtomicU32::new(self.versions.push(version)),
                        fact: OnceLock::new(),
                        source: OnceLock::new(),
                    }
                };
                self.included.insert(hash, |it| it.path.as_path() == path, make).0
            }
        };
        self.included_by.insert(
            self.included_by.hash((krate, at)),
            |&(by, of)| by == krate && of == at,
            || (krate, at),
        );
        at
    }

    /// The file `include!` parses for a file outside the VFS ([`SourceOf`]), made from
    /// [`Files::included_file`]'s read the first time one asks, with that read. `Err` with the
    /// file ID it would have when a file of the view already has that ID: only a view with
    /// more files than there are IDs left above them.
    pub fn included_source(
        &self,
        disk: &Disk,
        db: &Db,
        krate: CrateKey,
        path: &AbsPath,
        is_library: bool,
    ) -> (IncludedFile, Result<FileId, FileId>) {
        let at = self.included_at(disk, db, krate, path);
        let file = IncludedFile::Read(at);
        let raw = included_id(at);
        let id = FileId::from_raw(raw);
        let included = self.included.get(at);
        if included.source.get().is_some() {
            return (file, Ok(id));
        }
        let table = &db.snap().table;
        let taken = row(&table.texts, raw).is_some()
            || row(&table.raws, raw).is_some()
            || row(&table.given, raw).is_some()
            || row(&table.file_roots, raw).is_some()
            || row(&table.roots, raw).is_some();
        if taken {
            return (file, Err(id));
        }
        let vfs_path = VfsPath::from(path.to_path_buf());
        let mut files = FileSet::default();
        // clone: the one path of the root's set, once per included source
        files.insert(id, vfs_path.clone());
        let root = match is_library {
            true => SourceRoot::new_library(files),
            false => SourceRoot::new_local(files),
        };
        // A reader that made it first at once keeps its own.
        included.source.get_or_init(|| SourceOf { is_library, root: Arc::new(root), path: vfs_path });
        (file, Ok(id))
    }

    /// The files outside the VFS that `krate` has read (with `include!`, `include_bytes!` or
    /// `include_str!`), with the content ID of what `db`'s snapshot sees of each
    /// ([`IncludedFile::content`]: of its bytes, or of why it could not be read). Sorted by
    /// path. A crate served from its records in place of being resolved has read, for every
    /// key made from this, the files its records were found by
    /// ([`Files::included_are_served`]): they are here too, each path once.
    pub fn included_by(&self, db: &Db, krate: CrateKey) -> Vec<(AbsPathBuf, ContentId)> {
        let mut read = self.included_read_by_reads(db, krate);
        if let Some(served) = self.included_served.read().get(&krate) {
            for (path, content) in served.iter() {
                if !read.iter().any(|it| it.0 == *path) {
                    read.push((path.clone(), *content)); // clone: the edge, a path each
                }
            }
            read.sort_by(|a, b| a.0.cmp(&b.0));
        }
        read
    }

    /// Notes the files outside the VFS that `krate`, served from its records in place of
    /// being resolved, was found by (`hir_def::deps::filed::search`), in place of those it
    /// was served by before: none when it is not served by any, so a crate no longer served
    /// leaves no file behind.
    pub fn included_are_served(&self, krate: CrateKey, mut files: Vec<(AbsPathBuf, ContentId)>) {
        if files.is_empty() {
            // Most crates include nothing outside the VFS: no write lock for them.
            if self.included_served.read().contains_key(&krate) {
                self.included_served.write().remove(&krate);
            }
            return;
        }
        files.sort_by(|a, b| a.0.cmp(&b.0));
        self.included_served.write().insert(krate, files.into_boxed_slice());
    }

    /// [`Files::included_by`] for the reads this database made itself.
    fn included_read_by_reads(&self, db: &Db, krate: CrateKey) -> Vec<(AbsPathBuf, ContentId)> {
        let seq = db.seq();
        let mut read: Vec<_> = self
            .included_by
            .iter()
            .filter(|(by, _)| *by == krate)
            .map(|&(_, at)| {
                let included = self.included.get(at);
                // clone: the edge: the list leaves base-db, read once per build (a path each).
                (included.path.clone(), self.version_at(at, seq).content)
            })
            .collect(); // alloc: the edge, as above.
        read.sort_by(|a, b| a.0.cmp(&b.0));
        read
    }

    /// Whether `krate` read the file outside the VFS that `file` is the read of: every such
    /// read is noted with its crate ([`Files::included_at`]), so a write of `file` reaches
    /// exactly the crates this is true of. A file of the VFS ([`IncludedFile::Vfs`]) is read
    /// by no note here.
    pub fn included_read_by(&self, krate: CrateKey, file: IncludedFile) -> bool {
        let IncludedFile::Read(row) = file else { return false };
        self.included_by.iter().any(|&(by, at)| by == krate && at == row)
    }

    /// The one read of the disk for a file outside the VFS: it takes the database's [`Disk`],
    /// which a snapshot has none of.
    fn read_included(
        &self,
        _disk: &Disk,
        path: &AbsPath,
        looks: &Looks,
    ) -> Result<std::sync::Arc<[u8]>, ReadError> {
        IncludedFile::read_now(&self.on_disk(path.as_ref()), looks)
    }

    fn find_row(&self, path: &AbsPath) -> Option<u32> {
        self.included.find(self.included.hash(path), |it| it.path.as_path() == path)
    }

    pub(crate) fn find_included(&self, path: &AbsPath) -> Option<IncludedFile> {
        self.find_row(path).map(IncludedFile::Read)
    }

    /// Notes a file a frozen def map names that the VFS has not: whoever
    /// owns the VFS puts it in the source root, see [`Files::included_sources`].
    pub fn include_source(&self, path: &AbsPath, root: SourceRootId, krate: CrateKey) {
        let (at, _) = self.included_sources.insert(
            self.included_sources.hash(path),
            |(known, _)| known.as_path() == path,
            || (path.to_path_buf(), root),
        );
        self.sources_included_by.insert(
            self.sources_included_by.hash((krate, at)),
            |&(by, of)| by == krate && of == at,
            || (krate, at),
        );
    }

    /// The crates that named each file of [`Files::included_sources`], sorted.
    pub fn sources_included_by(&self) -> Vec<(CrateKey, AbsPathBuf)> {
        let mut named: Vec<_> = self
            .sources_included_by
            .iter()
            // clone: the edge: a sorted list for whoever loads included sources.
            .map(|&(krate, at)| (krate, self.included_sources.get(at).0.clone()))
            .collect(); // alloc: as above.
        named.sort();
        named
    }

    /// Sorted by path.
    pub fn included_sources(&self) -> Vec<(AbsPathBuf, SourceRootId)> {
        // alloc: the edge, as above.
        let mut sources: Vec<_> = self.included_sources.iter().cloned().collect();
        sources.sort();
        sources
    }

    pub fn env_is_read(&self, krate: CrateKey, key: &str) {
        note_env(&self.env_read, krate, key);
    }

    /// Tells the database how its portable keys write paths ([`Port`]): once, before any is
    /// asked. A second telling is refused (`false`): a key made under the first would stay,
    /// made from other directories than the second names.
    pub fn set_port(&self, port: std::sync::Arc<dyn Port>) -> bool {
        self.port.set(port).is_ok()
    }

    /// `bytes` as a portable key writes them ([`Port`]): as they are while no port is told.
    pub fn ported<'b>(&self, bytes: &'b [u8]) -> std::borrow::Cow<'b, [u8]> {
        match self.port.get() {
            Some(port) => port.ported(bytes),
            None => std::borrow::Cow::Borrowed(bytes),
        }
    }

    /// What `bytes`, written by [`Files::ported`] in some database, are in this one
    /// ([`Port::placed`]): as they are while no port is told, as `ported` left them so.
    pub fn placed<'b>(&self, bytes: &'b [u8]) -> Result<std::borrow::Cow<'b, [u8]>, String> {
        match self.port.get() {
            Some(port) => port.placed(bytes),
            None => Ok(std::borrow::Cow::Borrowed(bytes)),
        }
    }

    /// The directory of this database's port whose path `bytes` still hold ([`Port::held`]):
    /// none while no port is told, since such a database names no directory.
    pub fn held(&self, bytes: &[u8]) -> Option<&'static str> {
        self.port.get().and_then(|port| port.held(bytes))
    }

    /// Sorted.
    pub fn env_read(&self) -> Vec<(CrateKey, String)> {
        env_list(&self.env_read)
    }

    pub fn env_is_unset(&self, krate: CrateKey, key: &str) {
        note_env(&self.env_unset, krate, key);
    }

    /// Sorted.
    pub fn env_unset(&self) -> Vec<(CrateKey, String)> {
        env_list(&self.env_unset)
    }

    /// Notes what `by` in `krate` read, in place of what it read before: the latest expansion
    /// of a call (one read, or none for an expansion that read nothing), or each read of a
    /// value served in place of resolving (`hir_def::deps::filed::search`), none when it was
    /// not served. So the log says what each call's expansion reads now: a call whose
    /// expansion was unknown and traces now has only the trace.
    pub fn expansions_are_read(&self, krate: CrateKey, by: ReadBy, reads: Vec<ExpansionRead>) {
        let at = (krate, by);
        if reads.is_empty() {
            // Most expansions read nothing and were never noted: no write lock for them.
            if self.expansions_read.read().contains_key(&at) {
                self.expansions_read.write().remove(&at);
            }
            return;
        }
        self.expansions_read.write().insert(at, reads.into_boxed_slice());
    }

    /// Sorted, each once. A call no longer in its crate keeps what it last read: whoever
    /// refuses a value for an unknown read asks the crate's calls as they are now
    /// (`hir_def::deps::closure_expansion_reads`).
    pub fn expansions_read(&self) -> Vec<(CrateKey, ExpansionRead)> {
        let log = self.expansions_read.read();
        let mut list: Vec<_> = (log.iter())
            // clone: the edge: a sorted list for whoever keys a value by what was read.
            .flat_map(|(&(krate, _), reads)| reads.iter().map(move |it| (krate, it.clone())))
            .collect(); // alloc: as above.
        drop(log);
        list.sort();
        list.dedup();
        list
    }

    /// How many rows the log holds: one per call site whose latest expansion read something,
    /// and one per value served by its reads, each by its crate. What bounds the log's memory:
    /// it grows with the sites ever expanded, not with the edits ([`ReadBy::Call`]).
    pub fn expansion_rows(&self) -> usize {
        self.expansions_read.read().len()
    }

    /// What `by` in `krate` read, as [`Files::expansions_are_read`] last noted it: a crate's
    /// image filed by its reads keeps what its dependencies' were served by
    /// (`hir_def::deps::reads`).
    pub fn expansions_read_by(&self, krate: CrateKey, by: ReadBy) -> Vec<ExpansionRead> {
        // clone: the edge: a few names and hashes, once per dependency of a crate imaged.
        let reads = self.expansions_read.read().get(&(krate, by)).map(|it| it.to_vec());
        reads.unwrap_or_default() // alloc: as above.
    }

    /// Sorted.
    pub fn included_files(&self) -> Vec<AbsPathBuf> {
        // alloc: the edge, as above.
        let mut paths: Vec<_> = self.included.iter().map(|it| it.path.clone()).collect();
        paths.sort();
        paths
    }

    /// Whether the file on disk is what the world last read of it (its newest read; `db` is
    /// the edit's). Asked through [`IncludedFile::content_now`]: no look at the disk while a
    /// watch vouches for what was last read of it; a look made is pushed to `looks`.
    pub fn included_is_read(&self, _disk: &Disk, _db: &Db, path: &AbsPath, looks: &Looks) -> bool {
        let Some(at) = self.find_row(path) else { return false };
        self.newest(at).content == IncludedFile::content_now(&self.on_disk(path.as_ref()), looks)
    }

    /// Reads the file again, and says whether it changed: a read that moved is a new version
    /// stamped `seq` (the commit the edit makes), and its row is marked, so its readers' arenas
    /// are made again; the text `include!` parses of it changes with it. The read's look at the
    /// disk is pushed to `looks`.
    pub(crate) fn reload_included_file(&self, disk: &Disk, seq: u64, path: &AbsPath, looks: &Looks) -> bool {
        let Some(at) = self.find_row(path) else { return false };
        let read = self.read_included(disk, path, looks);
        self.set_included_read(seq, at, read)
    }

    /// The file outside the VFS at `path` set to `bytes`, as though read from disk now, stamped
    /// `seq`: for a file blaze writes and holds the bytes of before they reach the disk (a device
    /// build's MSL, which its dependents' `include_str!(env!("BLAZE_GPU_MSL_<CRATE>"))` reads),
    /// so a crate that reads it reads these. One no crate has read yet is made with them, so the
    /// first read is of these bytes, not of the disk. Kept among [`Files::included_paths`]: a
    /// later read of the disk that finds other bytes sets those, and a record that read it is
    /// filed by its content as any included file's ([`IncludedFile::content_now`]). Whether it
    /// changed.
    ///
    /// One no row has is staged in the edit and made a row by its commit
    /// ([`Files::publish_included`]): the rows are shared with every snapshot's readers, so a
    /// row is made only for bytes a commit keeps. It marks nothing: no crate read it.
    pub(crate) fn set_included_file(&self, seq: u64, path: &AbsPath, bytes: std::sync::Arc<[u8]>) -> bool {
        if let Some(at) = self.find_row(path) {
            return self.set_included_read(seq, at, Ok(bytes));
        }
        let mut writer = self.lock();
        match writer.staged_included.iter_mut().find(|(staged, _)| staged.as_path() == path) {
            Some((_, staged)) if **staged == *bytes => return false,
            Some((_, staged)) => *staged = bytes,
            None => writer.staged_included.push((path.to_path_buf(), bytes)),
        }
        true
    }

    /// Whether this edit set bytes of `path`, a file outside the VFS no row has
    /// ([`Files::set_included_file`]).
    pub(crate) fn is_staged_included(&self, path: &AbsPath) -> bool {
        self.lock().staged_included.iter().any(|(staged, _)| staged.as_path() == path)
    }

    /// Makes the rows of the bytes this edit staged ([`Files::set_included_file`]), each read
    /// stamped `seq`, the snapshot the commit leaves (a row's first read is seen by every
    /// snapshot): a commit's last step. A row a reader made since (a query that read the file
    /// from disk while the bytes were staged) gets the bytes as its newest read stamped
    /// `seq + 1`, marked for the next commit as any read that moved: the snapshot `seq` keeps
    /// the read its queries saw.
    pub(crate) fn publish_included(&self, seq: u64) {
        let staged = std::mem::replace(&mut self.lock().staged_included, Vec::new_in(Global));
        for (path, bytes) in staged {
            // By the borrowed path, as every other lookup of a row hashes it.
            let hash = self.included.hash(path.as_path());
            if let Some(at) = self.included.find(hash, |it| it.path == path) {
                self.set_included_read(seq + 1, at, Ok(bytes));
                continue;
            }
            let read = Ok(bytes);
            let content = IncludedFile::content_of(read.as_deref());
            let make = || {
                let version = Version { prev: NO_VERSION, seq, read, content, source: OnceLock::new() };
                // clone: the row's one copy of its path
                Included { path: path.clone(), newest: AtomicU32::new(self.versions.push(version)), fact: OnceLock::new(), source: OnceLock::new() }
            };
            self.included.insert(hash, |it| it.path == path, make);
        }
    }

    /// Row `at`'s read set to `read`, a new version stamped `seq`, when it is another than the
    /// newest. Whether it changed. Marks the row, and the session when the row has a source
    /// (its parse is homed there).
    fn set_included_read(
        &self,
        seq: u64,
        at: u32,
        read: Result<std::sync::Arc<[u8]>, ReadError>,
    ) -> bool {
        // Held across the push, so two writers never both take one newest as their previous.
        let mut writer = self.lock();
        let included = self.included.get(at);
        let prev = included.newest.load(Ordering::Acquire);
        let newest = self.versions.get(prev).expect("a row names only versions published");
        if newest.read == read {
            return false;
        }
        let content = IncludedFile::content_of(read.as_deref());
        let version = Version { prev, seq, read, content, source: OnceLock::new() };
        included.newest.store(self.versions.push(version), Ordering::Release);
        // An edit dropped puts `prev` back: the version written stays in the log, which no row
        // names then, so no snapshot reads it.
        writer.undo.push(Undo::IncludedNewest(at, prev));
        writer.marks.included.push(at);
        if included.source.get().is_some() {
            writer.marks.session = true;
        }
        true
    }

    /// The path outside the VFS of `file`, when it is the file `include!` parses of one
    /// ([`Files::included_source`]): what names it in the program, as a source root names its
    /// files.
    pub fn included_source_path(&self, file: FileId) -> Option<&AbsPath> {
        let at = self.source_row(file.index())?;
        Some(self.included.get(at).path.as_path())
    }

    /// The file `include!` parses of the file outside the VFS at `path`, once one has
    /// ([`Files::included_source`]).
    pub fn included_source_file(&self, path: &AbsPath) -> Option<FileId> {
        let at = self.find_row(path)?;
        self.included.get(at).source.get()?;
        Some(FileId::from_raw(included_id(at)))
    }
}

/// Puts in `root` the aliases of its files it lacks: a set given whole keeps the aliases a file
/// of it was given.
fn keep_aliases(writer: &Writer, root: &mut Arc<SourceRoot>) {
    for (of, aliases) in writer.aliases.iter() {
        if root.path_for_file(&of).is_none() {
            continue;
        }
        for &alias in aliases {
            if root.path_for_file(&alias).is_none() {
                // clone: only for a set a reader still holds (copy on write)
                Arc::make_mut(root).alias_file(alias, of);
            }
        }
    }
}

/// Where reads in [`Files::expansions_read`] come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReadBy {
    /// The latest expansion of a proc-macro call, by the call's key by its site
    /// (`hir_expand::MacroCallId::site_key`): the log outlives the snapshots, and a call's id
    /// is of one (an edit of its crate makes its arena again, and the call takes another id),
    /// so an id here would leave each expansion's reads under an id no later expansion has,
    /// never replaced. Keyed by its site, the log holds one row per call site its crate ever
    /// expanded: an edit that keeps a call where it is (its item's kind, name and index)
    /// replaces its row, and only a call that moved or went adds or keeps one.
    Call(ContentId),
    /// A value served by its reads in place of resolving its crate, by the key it is filed
    /// under: a crate's image, or a check of its bodies.
    Served(ContentId),
}

/// What a proc-macro expansion read besides its call, as [`Files::expansions_read`] has it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExpansionRead {
    /// The names it read, as its server encodes them, and the hash of the values they had.
    Traced { shape: Box<[u8]>, values: ContentId },
    /// Not known, and why: no key covers what it read.
    Unknown(&'static str),
}

fn note_env(table: &Table<(CrateKey, Box<str>)>, krate: CrateKey, key: &str) {
    table.insert(
        table.hash((krate, key)),
        |(by, known)| *by == krate && **known == *key,
        || (krate, key.into()),
    );
}

fn env_list(table: &Table<(CrateKey, Box<str>)>) -> Vec<(CrateKey, String)> {
    // alloc: the edge, as above.
    let mut list: Vec<_> = table.iter().map(|(krate, key)| (*krate, key.to_string())).collect();
    list.sort();
    list
}

/// Puts `content` as `file`'s in each source root that has it, adding to `changed` those whose
/// content it changes. Their content IDs are written by [`Writer::write_root_contents`].
pub(crate) fn index_file(
    writer: &mut Writer,
    file: FileId,
    content: ContentId,
    changed: &mut Roots,
) {
    let Some(roots) = writer.in_roots.get(file) else { return };
    for &root in roots {
        let Some(index) = writer.root_contents.get_mut(&root) else { continue };
        if index.set(file, content) && !changed.contains(&root) {
            changed.push(root);
        }
    }
}

/// The files of one source root as its content ID is made of them (see [`SourceRoot::DOMAIN`]):
/// a trie of [`SourceRoot::FANOUT`] ways by the hex digits of each path's
/// [`SourceRoot::PATH_DOMAIN`] hash, with each node's hash kept, so a change hashes again its
/// leaf and the nodes above it: `O(log n)` of a root of `n` files. Its shape is a function of
/// its files alone (a node is a leaf while it has at most [`SourceRoot::LEAF`] files, or at the
/// last digit), so equal files give an equal ID however they were edited in.
#[derive(Debug)]
pub(crate) struct RootContent {
    pub(crate) is_library: bool,
    /// Whether the root's [`FileSet`] names each of its files at one path and each path for
    /// one file, as every set a [`FileSetConfig::partition`](crate::FileSetConfig::partition)
    /// makes does: then a file is taken out of
    /// it in place ([`Files::set_view_root`]).
    pub(crate) one_to_one: bool,
    /// Each file's entry in `entries`.
    files: HashMap<FileId, u32, Global>,
    entries: Vec<Entry>,
    /// Entries no file has, for the next file to take.
    free_entries: Vec<u32>,
    /// The trie; its root is node 0, which is never freed.
    nodes: Vec<Node>,
    free_nodes: Vec<u32>,
    /// Whether `content` is out of date though node 0's hash may not be: `is_library` changed.
    stale: bool,
    /// The root's content ID, when neither `stale` nor node 0 is out of date.
    content: ContentId,
    /// The open edit's writes, each with what it replaced ([`RootContent::rollback`]): undone
    /// by its files, not its nodes, as the trie's shape and hashes are a function of the files.
    undo: Vec<ContentUndo, Global>,
}

/// A write of a [`RootContent`]'s files, with what it replaced.
#[derive(Debug)]
enum ContentUndo {
    /// The file was put in.
    Inserted(FileId),
    /// The file was taken out: its key, path hash and content ID.
    Removed(FileId, Box<[u8]>, ContentId, ContentId),
    /// The file's content ID moved: the one it had.
    Set(FileId, ContentId),
    /// `one_to_one` was set: what it was.
    OneToOne(bool),
}

#[derive(Debug)]
struct Entry {
    file: FileId,
    /// Its path, encoded ([`content::path_key`]).
    key: Box<[u8]>,
    /// The key's [`SourceRoot::PATH_DOMAIN`] hash: its place in the trie.
    path: ContentId,
    content: ContentId,
}

#[derive(Debug)]
struct Node {
    /// Whether its files are `entries`; an inner node's are under `children`.
    leaf: bool,
    /// A leaf's entries, in no order: one past [`SourceRoot::LEAF`] fits, for the insert that
    /// splits it.
    entries: SmallVec<[u32; SourceRoot::LEAF + 1]>,
    /// An inner node's children by the next digit, [`NO_NODE`] where it has no file.
    children: [u32; SourceRoot::FANOUT],
    /// How many files are under it.
    count: u32,
    /// Whether `hash` is out of date.
    dirty: bool,
    hash: ContentId,
}

const NO_NODE: u32 = u32::MAX;
/// A path hash has 64 hex digits: a node that deep is a leaf whatever it holds.
const MAX_DEPTH: usize = 64;

const _: () = assert!(SourceRoot::FANOUT == 16, "a digit is half a byte");

/// The digit of `path` that picks a child at `depth`.
fn digit(path: &ContentId, depth: usize) -> usize {
    let byte = path.0[depth / 2];
    (if depth % 2 == 0 { byte >> 4 } else { byte & 0xf }) as usize
}

/// The hash of a leaf with no file, which a child with no node stands for.
fn empty_leaf() -> ContentId {
    static EMPTY: OnceLock<ContentId> = OnceLock::new();
    *EMPTY.get_or_init(|| ContentId::of(SourceRoot::LEAF_DOMAIN, &0u64.to_le_bytes()))
}

impl Node {
    fn leaf() -> Node {
        Node {
            leaf: true,
            entries: SmallVec::new(),
            children: [NO_NODE; SourceRoot::FANOUT],
            count: 0,
            dirty: true,
            hash: ContentId::default(),
        }
    }
}

impl RootContent {
    fn empty(is_library: bool) -> RootContent {
        RootContent {
            is_library,
            one_to_one: false,
            files: HashMap::new_in(Global),
            entries: Vec::new(), // alloc: none until a file is put in; grows with the root (setup).
            free_entries: Vec::new(), // alloc: as above, with the files taken out.
            // alloc: once per source root set (setup).
            nodes: vec![Node::leaf()],
            free_nodes: Vec::new(), // alloc: as above, with the nodes freed.
            stale: true,
            content: ContentId::default(),
            undo: Vec::new_in(Global),
        }
    }

    /// Ends the open edit, its writes kept.
    fn forget(&mut self) {
        self.undo.clear();
    }

    /// Puts back the files the open edit wrote, the last write first, and says whether it
    /// wrote any: then the caller hashes the root again ([`RootContent::refresh`]), which
    /// gives the content ID it had, the trie's shape being its files'. O(writes × depth).
    fn rollback(&mut self) -> bool {
        let undo = std::mem::replace(&mut self.undo, Vec::new_in(Global));
        let wrote = !undo.is_empty();
        for write in undo.into_iter().rev() {
            match write {
                ContentUndo::Inserted(file) => _ = self.remove_entry(file),
                ContentUndo::Removed(file, key, path, content) => self.insert_entry(file, key, path, content),
                ContentUndo::Set(file, content) => _ = self.set_entry(file, content),
                ContentUndo::OneToOne(was) => self.one_to_one = was,
            }
        }
        wrote
    }

    /// Sets `one_to_one`, kept for undo.
    pub(crate) fn set_one_to_one(&mut self, one_to_one: bool) {
        if self.one_to_one != one_to_one {
            self.undo.push(ContentUndo::OneToOne(self.one_to_one));
            self.one_to_one = one_to_one;
        }
    }

    /// The content of a root of `files` (file, path key, content ID), each path once. Each key
    /// whose file had it in `old` keeps its path hash, the others are hashed in one batch, and
    /// each node whose files are those of the node in its place in `old` keeps its hash.
    fn new(
        is_library: bool,
        files: Vec<(FileId, Box<[u8]>, ContentId), &Frame>,
        old: Option<&RootContent>,
        frame: &Frame,
    ) -> RootContent {
        let mut paths = Vec::with_capacity_in(files.len(), frame);
        let mut hash = Vec::new_in(frame);
        for (at, (file, key, _)) in files.iter().enumerate() {
            match old.and_then(|old| old.path_of(*file, key)) {
                Some(path) => paths.push(path),
                None => {
                    paths.push(ContentId::default());
                    hash.push(at);
                }
            }
        }
        let keys = frame.alloc_slice_from_iter(hash.iter().map(|&at| &*files[at].1));
        let hashes = content::hash_many(SourceRoot::PATH_DOMAIN, keys, frame);
        for (&at, &path) in hash.iter().zip(hashes) {
            paths[at] = path;
        }
        let mut content = RootContent::empty(is_library);
        content.files.reserve(files.len());
        content.entries.reserve(files.len());
        for ((file, key, id), path) in files.into_iter().zip(paths) {
            // A content made whole is replaced whole in an edit: nothing to undo inside it.
            content.put(file, key, path, id, false);
        }
        if let Some(old) = old {
            content.adopt(0, old, 0);
            if !content.nodes[0].dirty && !old.stale && old.is_library == is_library {
                content.content = old.content;
                content.stale = false;
            }
        }
        content
    }

    /// The files the root has.
    pub(crate) fn files(&self) -> impl Iterator<Item = FileId> + '_ {
        self.files.keys().copied()
    }

    /// The path hash of `file`, if the root has it at `key`.
    fn path_of(&self, file: FileId, key: &[u8]) -> Option<ContentId> {
        let entry = &self.entries[*self.files.get(&file)? as usize];
        (*entry.key == *key).then_some(entry.path)
    }

    /// Takes the hash of each node whose files are those of the node in its place in `old`
    /// (`old_node`), where `old` has that hashed; whether `node`'s files are those. A node
    /// `old` has out of date has its nodes above out of date too, so a hash is taken only
    /// where all below it could be.
    fn adopt(&mut self, node: u32, old: &RootContent, old_node: u32) -> bool {
        let (new, was) = (&self.nodes[node as usize], &old.nodes[old_node as usize]);
        let same = match (new.leaf, was.leaf) {
            (true, true) => {
                new.count == was.count
                    && new.entries.iter().all(|&at| {
                        let entry = &self.entries[at as usize];
                        was.entries.iter().any(|&known| {
                            let known = &old.entries[known as usize];
                            known.path == entry.path
                                && known.content == entry.content
                                && known.key == entry.key
                        })
                    })
            }
            (false, false) => {
                let (children, old_children) = (new.children, was.children);
                let mut same = true;
                for (child, old_child) in children.into_iter().zip(old_children) {
                    // Every child is adopted, not only up to the first that differs.
                    same &= match (child, old_child) {
                        (NO_NODE, NO_NODE) => true,
                        (NO_NODE, _) | (_, NO_NODE) => false,
                        _ => self.adopt(child, old, old_child),
                    };
                }
                same
            }
            (true, false) | (false, true) => false,
        };
        let was = &old.nodes[old_node as usize];
        if same && !was.dirty {
            let this = &mut self.nodes[node as usize];
            this.hash = was.hash;
            this.dirty = false;
        }
        same
    }

    /// Puts `file` in the root at `key` (whose [`SourceRoot::PATH_DOMAIN`] hash is `path`) with
    /// `content`: a file another file's key had is taken from it.
    pub(crate) fn insert(
        &mut self,
        file: FileId,
        key: Box<[u8]>,
        path: ContentId,
        content: ContentId,
    ) {
        self.put(file, key, path, content, true);
    }

    /// [`RootContent::insert`], each write kept for undo when `log`.
    fn put(&mut self, file: FileId, key: Box<[u8]>, path: ContentId, content: ContentId, log: bool) {
        if let Some((key, path, content)) = self.remove_entry(file)
            && log
        {
            self.undo.push(ContentUndo::Removed(file, key, path, content));
        }
        if let Some(other) = self.find(&key, &path) {
            let other = self.entries[other as usize].file;
            if let Some((key, path, content)) = self.remove_entry(other)
                && log
            {
                self.undo.push(ContentUndo::Removed(other, key, path, content));
            }
        }
        self.insert_entry(file, key, path, content);
        if log {
            self.undo.push(ContentUndo::Inserted(file));
        }
    }

    /// Puts `file` in the root at `key`, which no file has: nothing kept for undo.
    fn insert_entry(&mut self, file: FileId, key: Box<[u8]>, path: ContentId, content: ContentId) {
        let entry = Entry { file, key, path, content };
        let at = match self.free_entries.pop() {
            Some(at) => {
                self.entries[at as usize] = entry;
                at
            }
            None => {
                self.entries.push(entry);
                (self.entries.len() - 1) as u32
            }
        };
        self.files.insert(file, at);
        self.place(at);
    }

    /// The entry at `key` (whose hash is `path`), if the root has one.
    fn find(&self, key: &[u8], path: &ContentId) -> Option<u32> {
        let (mut node, mut depth) = (0, 0);
        loop {
            let this = &self.nodes[node as usize];
            if this.leaf {
                return this
                    .entries
                    .iter()
                    .copied()
                    .find(|&at| *self.entries[at as usize].key == *key);
            }
            node = this.children[digit(path, depth)];
            if node == NO_NODE {
                return None;
            }
            depth += 1;
        }
    }

    fn new_node(&mut self) -> u32 {
        match self.free_nodes.pop() {
            Some(at) => {
                self.nodes[at as usize] = Node::leaf();
                at
            }
            None => {
                self.nodes.push(Node::leaf());
                (self.nodes.len() - 1) as u32
            }
        }
    }

    /// Puts entry `at` in the leaf its path leads to, made if there is none, and splits that
    /// leaf if it grows past [`SourceRoot::LEAF`].
    fn place(&mut self, at: u32) {
        let path = self.entries[at as usize].path;
        let (mut node, mut depth) = (0, 0);
        loop {
            let this = &mut self.nodes[node as usize];
            this.count += 1;
            this.dirty = true;
            if this.leaf {
                this.entries.push(at);
                if this.entries.len() > SourceRoot::LEAF && depth < MAX_DEPTH {
                    self.split(node, depth);
                }
                return;
            }
            let digit = digit(&path, depth);
            node = match this.children[digit] {
                NO_NODE => {
                    let child = self.new_node();
                    self.nodes[node as usize].children[digit] = child;
                    child
                }
                child => child,
            };
            depth += 1;
        }
    }

    /// Makes the leaf `node` at `depth` an inner node, its entries moved to new leaves by
    /// their digit at `depth`; a leaf that still has too many is split again.
    fn split(&mut self, node: u32, depth: usize) {
        let entries = std::mem::take(&mut self.nodes[node as usize].entries);
        self.nodes[node as usize].leaf = false;
        for &at in &entries {
            let digit = digit(&self.entries[at as usize].path, depth);
            let child = match self.nodes[node as usize].children[digit] {
                NO_NODE => {
                    let child = self.new_node();
                    self.nodes[node as usize].children[digit] = child;
                    child
                }
                child => child,
            };
            let child = &mut self.nodes[child as usize];
            child.entries.push(at);
            child.count += 1;
        }
        if depth + 1 == MAX_DEPTH {
            return;
        }
        for digit in 0..SourceRoot::FANOUT {
            let child = self.nodes[node as usize].children[digit];
            if child != NO_NODE && self.nodes[child as usize].count as usize > SourceRoot::LEAF {
                self.split(child, depth + 1);
            }
        }
    }

    /// Takes `file` out of the root; whether it had it. The highest inner node left with at
    /// most [`SourceRoot::LEAF`] files becomes a leaf, so the shape stays the one its files
    /// give.
    pub(crate) fn remove(&mut self, file: FileId) -> bool {
        match self.remove_entry(file) {
            Some((key, path, content)) => {
                self.undo.push(ContentUndo::Removed(file, key, path, content));
                true
            }
            None => false,
        }
    }

    /// [`RootContent::remove`], nothing kept for undo: the file's key, path hash and content
    /// ID, if the root had it.
    fn remove_entry(&mut self, file: FileId) -> Option<(Box<[u8]>, ContentId, ContentId)> {
        let at = self.files.remove(&file)?;
        let path = self.entries[at as usize].path;
        let content = self.entries[at as usize].content;
        // The nodes from the root to the entry's leaf.
        let mut trail: SmallVec<[u32; 16]> = SmallVec::new();
        let mut node = 0;
        let mut depth = 0;
        loop {
            let this = &mut self.nodes[node as usize];
            this.count -= 1;
            this.dirty = true;
            trail.push(node);
            if this.leaf {
                if let Some(place) = this.entries.iter().position(|&it| it == at) {
                    this.entries.swap_remove(place);
                }
                break;
            }
            node = this.children[digit(&path, depth)];
            depth += 1;
        }
        let key = std::mem::take(&mut self.entries[at as usize].key);
        self.free_entries.push(at);
        let collapse = trail.iter().copied().find(|&node| {
            let node = &self.nodes[node as usize];
            !node.leaf && node.count as usize <= SourceRoot::LEAF
        });
        match collapse {
            Some(node) => self.collapse(node),
            None => {
                // A leaf left empty under a node that stays inner goes.
                if let &[.., parent, leaf] = &trail[..]
                    && self.nodes[leaf as usize].count == 0
                {
                    self.nodes[parent as usize].children[digit(&path, trail.len() - 2)] = NO_NODE;
                    self.free_nodes.push(leaf);
                }
            }
        }
        Some((key, path, content))
    }

    /// Makes the inner node `node` a leaf of every entry under it, the nodes below freed.
    fn collapse(&mut self, node: u32) {
        let mut entries = SmallVec::new();
        let children = std::mem::replace(
            &mut self.nodes[node as usize].children,
            [NO_NODE; SourceRoot::FANOUT],
        );
        for child in children {
            if child != NO_NODE {
                self.gather(child, &mut entries);
            }
        }
        let this = &mut self.nodes[node as usize];
        this.leaf = true;
        this.entries = entries;
        this.dirty = true;
    }

    fn gather(&mut self, node: u32, into: &mut SmallVec<[u32; SourceRoot::LEAF + 1]>) {
        let this = &mut self.nodes[node as usize];
        into.extend(this.entries.drain(..));
        let children = this.children;
        for child in children {
            if child != NO_NODE {
                self.gather(child, into);
            }
        }
        self.free_nodes.push(node);
    }

    /// Sets the content ID of `file`, if the root has it; whether the root's changed. Kept
    /// for undo.
    fn set(&mut self, file: FileId, content: ContentId) -> bool {
        let Some(&at) = self.files.get(&file) else { return false };
        let old = self.entries[at as usize].content;
        if !self.set_entry(file, content) {
            return false;
        }
        self.undo.push(ContentUndo::Set(file, old));
        true
    }

    /// [`RootContent::set`], nothing kept for undo.
    fn set_entry(&mut self, file: FileId, content: ContentId) -> bool {
        let Some(&at) = self.files.get(&file) else { return false };
        let entry = &mut self.entries[at as usize];
        if entry.content == content {
            return false;
        }
        entry.content = content;
        let path = entry.path;
        let (mut node, mut depth) = (0, 0);
        loop {
            let this = &mut self.nodes[node as usize];
            this.dirty = true;
            if this.leaf {
                return true;
            }
            node = this.children[digit(&path, depth)];
            depth += 1;
        }
    }

    /// What [`SourceRoot::LEAF_DOMAIN`] hashes of a leaf: its files sorted by key, each as its
    /// key and content ID. `sorted` is room for the order.
    fn leaf_bytes(&self, node: u32, sorted: &mut Vec<u32, &Frame>, out: &mut Vec<u8>) {
        sorted.clear();
        sorted.extend_from_slice(&self.nodes[node as usize].entries);
        sorted.sort_unstable_by(|&a, &b| {
            self.entries[a as usize].key.cmp(&self.entries[b as usize].key)
        });
        out.put_u64(sorted.len() as u64);
        for &at in sorted.iter() {
            let entry = &self.entries[at as usize];
            out.put_bytes_u64(&entry.key);
            out.put_bytes(&entry.content.0);
        }
    }

    /// What [`SourceRoot::NODE_DOMAIN`] hashes of an inner node: its children's hashes in
    /// digit order, a child with no node as [`empty_leaf`].
    fn node_bytes(&self, node: u32, out: &mut Vec<u8>) {
        for &child in &self.nodes[node as usize].children {
            let hash = match child {
                NO_NODE => empty_leaf(),
                child => self.nodes[child as usize].hash,
            };
            out.put_bytes(&hash.0);
        }
    }

    /// Adds the nodes out of date to `leaves` and `inner` (with their depths), from `node`
    /// at `depth` down, and the greatest depth to `deepest`.
    fn dirty_nodes(
        &self,
        root: SourceRootId,
        node: u32,
        depth: usize,
        leaves: &mut Vec<(SourceRootId, u32), &Frame>,
        inner: &mut Vec<(SourceRootId, u32, usize), &Frame>,
        deepest: &mut usize,
    ) {
        let this = &self.nodes[node as usize];
        if !this.dirty {
            return;
        }
        if this.leaf {
            leaves.push((root, node));
            return;
        }
        inner.push((root, node, depth));
        *deepest = (*deepest).max(depth);
        for &child in &this.children {
            if child != NO_NODE {
                self.dirty_nodes(root, child, depth + 1, leaves, inner, deepest);
            }
        }
    }

    /// Brings each of `roots`' content IDs up to date: every leaf out of date in any of them
    /// hashed in one batch, then the inner nodes a batch a depth, deepest first, then the
    /// roots in one.
    pub(crate) fn refresh(
        contents: &mut HashMap<SourceRootId, RootContent, Global>,
        roots: &[SourceRootId],
        frame: &Frame,
    ) {
        let mut refreshed = Vec::new_in(frame);
        let mut leaves = Vec::new_in(frame);
        let mut inner = Vec::new_in(frame);
        let mut deepest = 0;
        for (place, root) in roots.iter().enumerate() {
            if roots[..place].contains(root) {
                continue;
            }
            let Some(index) = contents.get_mut(root) else { continue };
            index.stale |= index.nodes[0].dirty;
            if index.stale {
                refreshed.push(*root);
                index.dirty_nodes(*root, 0, 0, &mut leaves, &mut inner, &mut deepest);
            }
        }
        if refreshed.is_empty() {
            return;
        }
        fn of(
            contents: &HashMap<SourceRootId, RootContent, Global>,
            root: SourceRootId,
        ) -> &RootContent {
            contents.get(&root).expect("a root out of date has its content")
        }
        let mut bytes = scratch::lend_bytes();
        let mut ranges: Vec<Range<usize>, &Frame> = Vec::new_in(frame);
        let mut sorted = Vec::new_in(frame);
        for &(root, node) in &leaves {
            let start = bytes.len();
            of(contents, root).leaf_bytes(node, &mut sorted, &mut bytes);
            ranges.push(start..bytes.len());
        }
        let hashes = content::hash_ranges(SourceRoot::LEAF_DOMAIN, &bytes, &ranges, frame);
        for (&(root, node), &hash) in leaves.iter().zip(hashes) {
            let node = &mut contents.get_mut(&root).expect("as above").nodes[node as usize];
            node.hash = hash;
            node.dirty = false;
        }

        let mut level = Vec::new_in(frame);
        for depth in (0..=deepest).rev() {
            bytes.clear();
            ranges.clear();
            level.clear();
            for &(root, node, at) in &inner {
                if at != depth {
                    continue;
                }
                let start = bytes.len();
                of(contents, root).node_bytes(node, &mut bytes);
                ranges.push(start..bytes.len());
                level.push((root, node));
            }
            let hashes = content::hash_ranges(SourceRoot::NODE_DOMAIN, &bytes, &ranges, frame);
            for (&(root, node), &hash) in level.iter().zip(hashes) {
                let node = &mut contents.get_mut(&root).expect("as above").nodes[node as usize];
                node.hash = hash;
                node.dirty = false;
            }
        }

        bytes.clear();
        ranges.clear();
        for &root in &refreshed {
            let index = of(contents, root);
            let start = bytes.len();
            bytes.push(index.is_library as u8);
            bytes.extend_from_slice(&index.nodes[0].hash.0);
            ranges.push(start..bytes.len());
        }
        let hashes = content::hash_ranges(SourceRoot::DOMAIN, &bytes, &ranges, frame);
        for (root, &hash) in refreshed.iter().zip(hashes) {
            let index = contents.get_mut(root).expect("as above");
            index.content = hash;
            index.stale = false;
        }
    }
}

/// [`FileText::no_text`], hashed once.
pub(crate) fn no_text() -> ContentId {
    static NO_TEXT: OnceLock<ContentId> = OnceLock::new();
    *NO_TEXT.get_or_init(|| ContentId::of(FileText::NO_TEXT_DOMAIN, b""))
}
