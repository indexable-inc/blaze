//! The handles a query reads a file and a source root by, and the inputs behind them.
//!
//! A file of the VFS is a set of inputs the world makes in its arena each time that arena is
//! made ([`crate::Files::make_root`]): [`VfsText`] or [`RawFile`], and [`VfsFileRoot`]; a source
//! root is a [`VfsRoot`]. A file outside the VFS that a macro read is no input: it is a row of
//! [`crate::Files`] whose reads are versions stamped by the commit that wrote them, and its
//! handle ([`FileText::Included`], [`IncludedFile::Read`], ...) is the row's number, homed in the
//! session. A read through such a handle sees the latest version stamped at or before its
//! snapshot's commit (the first version, for a snapshot older than every one), and lists the
//! fact of the row's path with that version's content
//! ([`crate::world::included_fact`]), so a reader is told apart by what it read.
//!
//! Cost (rule 67). A read through a VFS handle is the input's read: one slot load and one fact
//! listed, the floor. A read through an included handle is one acquire load of the row's newest
//! version, a walk back over the versions newer than the snapshot (none in a process with one
//! snapshot, the baseline), one fact listed, and once a version the path's fact id hashed (a
//! short path, 1 BLAKE3 call) and, for `include!`'s text, its source made (the text shared with
//! the read's bytes, its edit found against the version before: the floor of a reparse).

use std::alloc::Global;

use blaze_ids::{ContentId, Hasher};
use blaze_memo::{Database as _, Home, Homes};
use triomphe::Arc;

use crate::{
    Db, FileId, LookKind, Looks, SourceRoot, SourceRootId,
    content::{self, InputId},
    files,
    reparse::Reparse,
};

/// A file of the VFS with text: made by the world in its source root's arena.
#[blaze_memo::input(debug)]
pub struct VfsText {
    /// The text, which the file's parse tree shares: the VFS's bytes as they are.
    #[returns(ref)]
    #[get(text_field)]
    pub(crate) text: syntax::Text,
    #[returns(copy)]
    pub(crate) file_id: FileId,
    /// See [`FileText::appended_root_module`].
    #[returns(copy)]
    pub(crate) appended_root_module: Option<&'static str>,
    #[returns(copy)]
    pub(crate) content: InputId,
    /// What [`crate::EditionedFileId::parse`] makes the text's parse from: not in the content
    /// ID, as the parse is the same whatever it holds.
    #[returns(ref)]
    pub(crate) reparse: Reparse,
}

/// A file of the VFS whose bytes are not UTF-8: its bytes as the view loader gave them.
#[blaze_memo::input(debug)]
pub struct RawFile {
    #[returns(ref)]
    pub(crate) read: Result<std::sync::Arc<[u8]>, ReadError>,
    #[returns(copy)]
    pub(crate) content: InputId,
}

/// A file of the VFS known by its content alone, with neither text nor bytes in this snapshot:
/// a file of a crate given by its address (hir-def's `deps::give_keyed`), whose content is the
/// one the crate's record names it with ([`crate::Edit::set_file_given`]). What
/// [`crate::Files::file_content`] answers for it, so every reader that finds something by a
/// file's content (an entry set, a file's index) finds this file's and no other's; its text
/// is still asked for with [`crate::Files::file_text`], which it has not.
#[blaze_memo::input(debug)]
pub struct GivenFile {
    #[returns(copy)]
    pub(crate) content: InputId,
}

/// The source root a file of the VFS is in.
#[blaze_memo::input(debug)]
pub struct VfsFileRoot {
    #[returns(copy)]
    pub(crate) source_root_id: SourceRootId,
}

/// A source root of the VFS. Its content ID is of its files' paths and content IDs and
/// `is_library` ([`SourceRoot::DOMAIN`]), kept by [`crate::Files`] as either changes.
#[blaze_memo::input(debug)]
pub struct VfsRoot {
    #[returns(ref)]
    pub(crate) source_root: Arc<SourceRoot>,
    #[returns(copy)]
    pub(crate) content: InputId,
}

/// A file's text: a file of the VFS's, or the text `include!` parses of a file outside it
/// ([`crate::Files::included_source`]), by its row.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum FileText {
    Vfs(VfsText),
    Included(u32),
}

/// What `include_bytes!` and `include_str!` read: the bytes of a file of the VFS that is not
/// UTF-8, or a read of a file outside the VFS, by its row.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum IncludedFile {
    Vfs(RawFile),
    Read(u32),
}

/// The source root of a file: a file of the VFS's, or the root of its own an included
/// source is in, by its row.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum FileSourceRootInput {
    Vfs(VfsFileRoot),
    Included(u32),
}

/// A source root: one of the VFS, or the one-file root of an included source, by its row.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum SourceRootInput {
    Vfs(VfsRoot),
    Included(u32),
}

// An included row lives as long as the database's files: homed in the session, which names no
// arena past it.
impl Home for FileText {
    #[inline]
    fn home(&self, homes: &mut Homes<'_>) {
        match self {
            FileText::Vfs(it) => it.home(homes),
            FileText::Included(_) => {}
        }
    }
}

impl Home for IncludedFile {
    #[inline]
    fn home(&self, homes: &mut Homes<'_>) {
        match self {
            IncludedFile::Vfs(it) => it.home(homes),
            IncludedFile::Read(_) => {}
        }
    }
}

impl Home for FileSourceRootInput {
    #[inline]
    fn home(&self, homes: &mut Homes<'_>) {
        match self {
            FileSourceRootInput::Vfs(it) => it.home(homes),
            FileSourceRootInput::Included(_) => {}
        }
    }
}

impl Home for SourceRootInput {
    #[inline]
    fn home(&self, homes: &mut Homes<'_>) {
        match self {
            SourceRootInput::Vfs(it) => it.home(homes),
            SourceRootInput::Included(_) => {}
        }
    }
}

/// The file ID and source root ID of the included source of row `row`: counted down from the
/// top ([`crate::table::MAX_FILE_ID`]), where the view's (counted up from 0) never reach while
/// one is there.
#[inline]
pub(crate) fn included_id(row: u32) -> u32 {
    crate::table::MAX_FILE_ID - row
}

impl FileText {
    /// A file text's content ID is of the blaze-vfs ID of its text's bytes
    /// (`ContentId::of(blaze_vfs::FILE_DOMAIN, ..)`), then of its appended root module (0, or 1
    /// and the module), which changes what the crate root is. Building on the vfs ID means a
    /// view's file, whose vfs ID is given and checked, is hashed once. The file's ID and path
    /// are not in it.
    pub const DOMAIN: &'static str = "blaze v2 file text 2";
    /// The content ID a file of a source root has when it has neither text nor bytes (an alias
    /// of a file whose bytes are not UTF-8). A file of the VFS whose bytes are not UTF-8 has
    /// the content ID of its bytes instead ([`crate::Files::file_content`]), which
    /// `include_bytes!` and `include_str!` read.
    pub const NO_TEXT_DOMAIN: &'static str = "blaze v2 file without text 1";

    pub fn content_of(text: &str, appended_root_module: Option<&str>) -> ContentId {
        FileText::content_of_file(
            ContentId::of(blaze_vfs::FILE_DOMAIN, text.as_bytes()),
            appended_root_module,
        )
    }

    /// [`FileText::content_of`] from the blaze-vfs ID of the text's bytes, without reading them.
    pub fn content_of_file(file: ContentId, appended_root_module: Option<&str>) -> ContentId {
        let mut hasher = Hasher::new(FileText::DOMAIN);
        hasher.id(&file).opt_part(appended_root_module);
        hasher.finish()
    }

    /// What [`FileText::content_of_file`] hashes under [`FileText::DOMAIN`], appended to `out`:
    /// for hashing many in one batch.
    pub(crate) fn content_bytes(
        file: &ContentId,
        appended_root_module: Option<&str>,
        out: &mut Vec<u8>,
    ) {
        out.extend_from_slice(&file.0);
        content::opt_bytes(out, appended_root_module);
    }

    pub fn no_text() -> ContentId {
        files::no_text()
    }

    /// The text, which the file's parse tree shares ([`crate::EditionedFileId::parse`]): the
    /// VFS's bytes as they are, a heap file's allocation or a range of the mapped image. Takes
    /// the [`Db`]: an included source's text is the version its snapshot sees.
    pub fn text<'db>(self, db: &'db Db) -> &'db syntax::Text {
        match self {
            FileText::Vfs(it) => it.text_field(db),
            FileText::Included(row) => &db.files().included_text(db, row).text,
        }
    }

    pub fn file_id(self, db: &Db) -> FileId {
        match self {
            FileText::Vfs(it) => it.file_id(db),
            FileText::Included(row) => FileId::from_raw(included_id(row)),
        }
    }

    /// The module a crate root's text ends with that the compiler wrote, not the file on disk:
    /// blaze's test harness or list of proc macros. rustc injects its own after it applies the
    /// crate's `#![cfg]`, so a root that `#![cfg]` takes away still has it. An included source
    /// has none.
    pub fn appended_root_module(self, db: &Db) -> Option<&'static str> {
        match self {
            FileText::Vfs(it) => it.appended_root_module(db),
            FileText::Included(_) => None,
        }
    }

    pub fn content(self, db: &Db) -> ContentId {
        match self {
            FileText::Vfs(it) => it.content(db).id(),
            FileText::Included(row) => db.files().included_text(db, row).content,
        }
    }

    /// What [`crate::EditionedFileId::parse`] makes the text's parse from. Takes the [`Db`], as
    /// [`FileText::text`] does.
    pub fn reparse<'db>(self, db: &'db Db) -> &'db Reparse {
        match self {
            FileText::Vfs(it) => it.reparse(db),
            FileText::Included(row) => &db.files().included_text(db, row).reparse,
        }
    }
}

/// Where the lines of a file's text are ([`span::LineIndex`]): made the first time a
/// diagnostic, `line!`/`column!`, a caller location, a proc macro's `Span::line` or debug info
/// asks for a line or a column of the text, once per text, and never on load.
#[blaze_memo::tracked(returns(ref), owns)]
pub fn line_index(db: &Db, text: FileText) -> span::LineIndex {
    span::LineIndex::new(text.text(db))
}

/// Why a file `include_bytes!` or `include_str!` names could not be read: the error's kind and
/// the OS's code where it has one, which is what rustc's "couldn't read" message prints.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReadError {
    kind: std::io::ErrorKind,
    os: Option<i32>,
}

impl ReadError {
    pub fn of(error: &std::io::Error) -> ReadError {
        ReadError { kind: error.kind(), os: error.raw_os_error() }
    }
}

impl std::fmt::Display for ReadError {
    /// As `std::io::Error` writes itself: "No such file or directory (os error 2)".
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.os {
            Some(code) => std::fmt::Display::fmt(&std::io::Error::from_raw_os_error(code), f),
            None => std::fmt::Display::fmt(&self.kind, f),
        }
    }
}

/// Whether `error` says nothing is at the path, as `blaze_usercode::inputs` reads it: not
/// found, or a part of the path before its last is no directory.
fn not_there(error: &std::io::Error) -> bool {
    // `ENOTDIR`, on Linux and on macOS.
    #[cfg(unix)]
    const NOT_A_DIRECTORY: i32 = 20;
    #[cfg(not(unix))]
    const NOT_A_DIRECTORY: i32 = 267;
    matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory)
        || error.raw_os_error() == Some(NOT_A_DIRECTORY)
}

/// What [`IncludedFile::content_now`] last said of each path: one blaze holds the bytes of
/// ([`IncludedFile::hold`]), or one read from the disk under a stamp.
#[derive(Clone, Copy)]
enum Kept {
    Held(ContentId),
    /// Read under `stamp` (a `stat`, links followed) by a look at `looked_ns`, its content
    /// `id`, and the look's answer as [`IncludedFile::read_now`] pushes it (`None`: a read
    /// that pushes nothing).
    Stamped { stamp: blaze_core::Stamp, looked_ns: u64, id: ContentId, answer: Option<Option<[u8; 32]>> },
}

/// [`Kept`] of each path, a stamp memory: an entry is the answer while its stamp holds
/// ([`blaze_vfs::outside::holds`]), so a file is read again only when its stamp moved or was
/// racy.
#[expect(
    blaze::process_static,
    reason = "one row per included path ever read, never pruned: the database that reads the files should own it"
)]
static KEPT: std::sync::LazyLock<blaze_core::lock::Lock<blaze_hash::HashMap<std::path::PathBuf, Kept, Global>>> =
    std::sync::LazyLock::new(|| blaze_core::lock::Lock::new("db.included.kept", blaze_hash::HashMap::new_in(Global)));

thread_local! {
    /// A planted bug, for a test's control: [`IncludedFile::content_now`] looks at the disk
    /// on every ask, as until 2026-11-12. Never set outside a test.
    #[doc(hidden)]
    pub static LOOKS_EVERY_TIME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl IncludedFile {
    /// Of its bytes (1 and the bytes), or of why it could not be read (0 and the error's
    /// text): a message that names the error changes with it.
    pub const DOMAIN: &'static str = "blaze v2 included file 2";

    pub fn content_of(read: Result<&[u8], &ReadError>) -> ContentId {
        let mut hasher = Hasher::new(IncludedFile::DOMAIN);
        match read {
            // alloc: the error's text, on the error path
            Err(error) => hasher.update(&[0]).part(error.to_string().as_bytes()),
            Ok(bytes) => hasher.update(&[1]).part(bytes),
        };
        hasher.finish()
    }

    /// The file at `path` read from disk now, as the front end reads a file outside the VFS
    /// ([`crate::Files::included_file`] reads the path its view has it at). Read from the disk
    /// every time: no watch vouches for the file here. The look is pushed to `looks`, the list
    /// of the build the file is read for, as a [`LookKind::File`] look.
    ///
    /// The invariant: the answer pushed is what `blaze_usercode::inputs::Files` says of the
    /// path when asked again: the BLAKE3 hash of a regular file's bytes, `None` for a path
    /// that is not there. Anything else read (a directory, a device, a file that can't be
    /// read) pushes nothing: its answer there is one this crate does not make.
    pub fn read_now(
        path: &std::path::Path,
        looks: &Looks,
    ) -> Result<std::sync::Arc<[u8]>, ReadError> {
        let looked_ns = blaze_core::system_wall_ns();
        let stamp = blaze_vfs::outside::stat(path);
        let read = std::fs::read(path);
        if let Some(answer) = IncludedFile::answer_of(read.as_deref(), &stamp) {
            looks.push_stamped(path, LookKind::File, answer, stamp, looked_ns);
        }
        read.map(Into::into).map_err(|it| ReadError::of(&it))
    }

    /// The answer the look of a read says, the file's stamp before the read being `stamp`
    /// ([`IncludedFile::read_now`]; `None`: a read whose answer this crate does not make,
    /// pushed nowhere).
    fn answer_of(read: Result<&[u8], &std::io::Error>, stamp: &blaze_core::Stamp) -> Option<Option<[u8; 32]>> {
        match read {
            // A regular file alone hashes as its bytes; a pipe or a device read here does not.
            Ok(bytes) if stamp.is_file() => Some(Some(blaze_ids::b3::hash(bytes))),
            Err(error) if not_there(error) => Some(None),
            Ok(_) | Err(_) => None,
        }
    }

    /// [`IncludedFile::content`] of the file at `path` as it is on disk now
    /// ([`IncludedFile::read_now`]): what a unit record that read it is filed by, and what a
    /// build compares with what it read of the file ([`crate::Files::included_is_read`]).
    ///
    /// The invariant: the file is read only when its stamp moved or was racy. Each ask is one
    /// `stat` (the floor: a file whose bytes may have changed is told so only by a look);
    /// the answer kept under a stamp that holds ([`blaze_vfs::outside::holds`]) is the answer,
    /// a path that is not there too ([`blaze_core::Stamp::GONE`], asked of the whole path).
    /// Until 2026-11-12 every ask read the file; from then to 2026-11-14 a watch of the file's
    /// directory vouched for it with no syscall, and setting one up walked the tree under it
    /// on the daemon's loop (256,216 directories under `/root`), which killed the daemon on
    /// every real project. Every ask pushes its look to `looks` with the stamp, answered from
    /// memory or not, so a build's list is the same whatever was remembered.
    pub fn content_now(path: &std::path::Path, looks: &Looks) -> ContentId {
        let kept = KEPT.lock().get(path).copied();
        if let Some(Kept::Held(id)) = kept
            && !LOOKS_EVERY_TIME.get()
        {
            return id;
        }
        let looked_ns = blaze_core::system_wall_ns();
        let stamp = blaze_vfs::outside::stat(path);
        if let Some(Kept::Stamped { stamp: was, looked_ns: at, id, answer }) = kept
            && blaze_vfs::outside::holds(&was, at, &stamp)
            && !LOOKS_EVERY_TIME.get()
        {
            if let Some(answer) = answer {
                looks.push_stamped(path, LookKind::File, answer, was, at);
            }
            return id;
        }
        let read = std::fs::read(path);
        let answer = IncludedFile::answer_of(read.as_deref(), &stamp);
        if let Some(answer) = answer {
            looks.push_stamped(path, LookKind::File, answer, stamp, looked_ns);
        }
        let id = match &read {
            Ok(bytes) => IncludedFile::content_of(Ok(bytes)),
            Err(error) => IncludedFile::content_of(Err(&ReadError::of(error))),
        };
        // Kept under the stamp from before the read: a write during the read moves it, so the
        // next ask reads again. A stamp that can't be asked is never kept.
        if !stamp.is_unknown() {
            KEPT.lock().insert(path.to_path_buf(), Kept::Stamped { stamp, looked_ns, id, answer });
        }
        id
    }

    /// The file at `path` is one blaze holds the bytes of (a file a build script's run left
    /// in its `OUT_DIR`, which blaze writes there itself from the run's kept result): its
    /// content is `content` until it is held again, and the disk is not looked at for it
    /// ([`IncludedFile::content_now`]). Without this each build looked at every such file a
    /// crate includes (serde's `private.rs`, bindgen's `bindings.rs`: about 25 looks a build
    /// of reth), their directory hearing blaze's own restore of them, so no watch vouched and
    /// the build was never answered again without a walk.
    pub fn hold(path: &std::path::Path, content: ContentId) {
        KEPT.lock().insert(path.to_path_buf(), Kept::Held(content));
    }

    /// The bytes, or why they could not be read. Takes the [`Db`]: a read outside the VFS is
    /// the version its snapshot sees.
    pub fn read<'db>(self, db: &'db Db) -> &'db Result<std::sync::Arc<[u8]>, ReadError> {
        match self {
            IncludedFile::Vfs(it) => it.read(db),
            IncludedFile::Read(row) => &db.files().included_read(db, row).read,
        }
    }

    pub fn bytes<'db>(self, db: &'db Db) -> Option<&'db std::sync::Arc<[u8]>> {
        self.read(db).as_ref().ok()
    }

    pub fn content(self, db: &Db) -> ContentId {
        match self {
            IncludedFile::Vfs(it) => it.content(db).id(),
            IncludedFile::Read(row) => db.files().included_read(db, row).content,
        }
    }
}

impl FileSourceRootInput {
    /// The root's ID: an included source's root is numbered as its file is.
    pub fn source_root_id<D: ?Sized + blaze_memo::Database>(self, db: &D) -> SourceRootId {
        match self {
            FileSourceRootInput::Vfs(it) => it.source_root_id(db),
            FileSourceRootInput::Included(row) => SourceRootId(included_id(row)),
        }
    }
}

impl SourceRootInput {
    pub fn source_root(self, db: &Db) -> Arc<SourceRoot> {
        // clone: the owned copy this API hands out; a reader that only looks uses `source_root_ref`.
        Arc::clone(self.root_arc(db))
    }

    /// [`SourceRootInput::source_root`] without a clone of the `Arc` (a write to the count
    /// every reader of the root shares).
    pub fn source_root_ref(self, db: &Db) -> &SourceRoot {
        self.root_arc(db)
    }

    fn root_arc(self, db: &Db) -> &Arc<SourceRoot> {
        match self {
            SourceRootInput::Vfs(it) => it.source_root(db),
            SourceRootInput::Included(row) => &db.files().included_root(row).root,
        }
    }

    /// The root's content ID. Takes the [`Db`]: an included source's root's content is of the
    /// version its snapshot sees.
    pub fn content(self, db: &Db) -> ContentId {
        match self {
            SourceRootInput::Vfs(it) => it.content(db).id(),
            SourceRootInput::Included(row) => db.files().included_text(db, row).root_content,
        }
    }
}

/// The kind a read of a file outside the VFS is listed with ([`note_included`]): a kind of its
/// own, so a read list tells an included file's read (`include!`'s text, `include_bytes!`'s
/// bytes, an included source) from every other fact read around the inputs
/// ([`blaze_memo::AROUND_INPUTS`]). A classifier that answers by kind refuses it by its name,
/// as it did the reserved kind.
pub const INCLUDED_READ: blaze_memo::KindId = blaze_memo::KindId::of(&INCLUDED_READ_KIND);

static INCLUDED_READ_KIND: blaze_memo::Kind =
    blaze_memo::Kind::new("base_db::included_file", blaze_memo::CycleKind::Panic);

/// Lists the fact of an included row's read as read by the running query, of kind
/// [`INCLUDED_READ`]: `id` the row's path fact id, `content` the version's.
pub(crate) fn note_included(db: &Db, id: ContentId, content: ContentId) {
    let fact = blaze_memo::Fact { id, digest: content };
    blaze_memo::read(db.memo(), blaze_memo::Read::Input { kind: INCLUDED_READ, fact });
}
