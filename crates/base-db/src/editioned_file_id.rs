//! Defines [`EditionedFileId`], an interned wrapper around [`span::EditionedFileId`] so queries
//! can take it as a key.
//!
//! It is interned beside the handle of its file's text ([`crate::FileText`]), which names the
//! arena the text lives in: so the file's id, and every query keyed by it (its parse, its
//! errors), is homed where its text is, and an edit of the file drops them with the text and
//! nothing else. Cost (rule 67): interning is one probe of that arena's table; the handle is
//! one lookup of the snapshot's table.

use std::hash::Hash;

use crate::FileId;
use blaze_ids::ContentId;
use span::{Edition, Extensions};
use syntax::{SyntaxError, ast};

use crate::{Db, FileText};

#[blaze_memo::interned(debug, constructor = intern, unsafe(no_lifetime))]
pub struct EditionedFileId {
    /// The [`span::EditionedFileId`]'s bits.
    #[returns(copy)]
    bits: u32,
    /// The file's text, where it has one: its home.
    #[returns(copy)]
    home: Option<FileText>,
}

/// Why a file's parse was refused: the file has no text in this snapshot (a file of a crate
/// given by its address whose text was never set). The one place every parse passes refuses
/// it, typed, naming the file, before anything is asked of it: its id has no home
/// ([`EditionedFileId`]'s `home`), so a query keyed by it (its parse, whether it is a library
/// file) would be homed at the session arena and read a crate's arena, an engine panic. hir-def
/// refuses the same read earlier, naming the crate too (`pages::demand_text`); this is the
/// backstop for a caller that asks a parse without going through it. A driver fails the one
/// job that asked with it (rule 20).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoText {
    pub file: FileId,
    /// The file's path, where its source root names one.
    pub path: Option<String>,
}

impl NoText {
    /// Unwinds with `self` as the payload, logged first.
    pub fn throw(self) -> ! {
        tracing::error!(target: "blaze", "{self}");
        std::panic::panic_any(self)
    }

    /// The `NoText` a caught panic's payload is, if it is one.
    // dyn: a caught panic's payload, read once
    pub fn of_payload(payload: &(dyn std::any::Any + Send)) -> Option<&NoText> {
        payload.downcast_ref::<NoText>()
    }
}

impl std::fmt::Display for NoText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let path = self.path.as_deref().unwrap_or("a file with no path");
        write!(
            f,
            "base-db: the parse of {path} ({:?}) was asked, and the file has no text in this snapshot \
             (a file of a crate given by its address whose text was never set)",
            self.file
        )
    }
}

impl std::error::Error for NoText {}

/// [`EditionedFileId::parse`]: the tree, kept with the arena of the file's text.
#[blaze_memo::tracked(self_ty = EditionedFileId, returns(ref), owns, mem_tag = Parse)]
fn parse_(db: &Db, file: EditionedFileId) -> syntax::Parse<ast::SourceFile> {
    // The key is the interned file id, which holds the crate's syntax extensions beside the
    // edition (`span::EditionedFileId::extensions`): a file of a crate that turns one on is
    // another key, and another parse.
    let (file_id, edition) = file.unpack(db);
    // First, before any query keyed by the file: a file with no text has no home.
    if file.home(db).is_none() {
        // alloc: error path
        NoText { file: file_id, path: db.path_for_file(file_id).map(|it| it.to_string()) }.throw()
    }
    let (extensions, outline) = (file.extensions(db), file.is_library(db));
    let span = tracing::info_span!("parse_query", function = tracing::field::Empty);
    if !span.is_disabled() {
        // Only while a subscriber records the span, which interns it.
        let path = db
            .path_for_file(file_id)
            .map_or_else(|| format!("{{file {}}}", file_id.index()), |it| it.to_string());
        span.record("function", path.as_str());
    }
    let _p = span.entered();
    let input = db.file_text(file_id);
    input.reparse(db).parse(input.text(db), edition, extensions, outline)
}

#[blaze_memo::tracked]
impl EditionedFileId {
    /// Whether the file is in a library source root: a firewall, so an edit to the root's set
    /// of files that keeps the answer re-parses none of them.
    #[blaze_memo::tracked(returns(copy))]
    pub fn is_library(self, db: &Db) -> bool {
        let root = db.file_source_root(self.file_id(db)).source_root_id(db);
        db.source_root(root).source_root_ref(db).is_library
    }

    // firewall query
    #[blaze_memo::tracked(returns(as_deref), owns)]
    pub fn parse_errors(self, db: &Db) -> Option<Box<[SyntaxError]>> {
        self.parse(db).errors_exact()
    }

    /// The file's syntax errors with every lazy body's: a firewall, so a check of a
    /// dependency parses its bodies for their errors once.
    #[blaze_memo::tracked(returns(as_deref), owns)]
    pub fn parse_errors_with_bodies(self, db: &Db) -> Option<Box<[SyntaxError]>> {
        self.parse(db).errors_with_bodies_exact()
    }
}

impl EditionedFileId {
    /// The interned id of `field`, homed with its file's text.
    pub fn from_span_file_id(db: &Db, field: span::EditionedFileId) -> Self {
        let file = field.file_id();
        let home = db.has_file_text(file).then(|| db.file_text(file));
        Self::intern(db, field.as_u32(), home)
    }

    /// The file's tree, sharing its text. A file of a library source root (a dependency's) is an
    /// outline, whose `fn` bodies are parsed when first walked into (#1294); every other file is
    /// parsed in full. Decided by the source root, so a file two crates read gets one answer.
    /// After an edit it is made from the file's last parse, when one is kept (see
    /// [`Reparse`](crate::Reparse)): the same tree, in the time of the edited item's parse.
    #[inline]
    pub fn parse(self, db: &Db) -> &syntax::Parse<ast::SourceFile> {
        parse_(db, self)
    }

    /// The file read in `edition` for a crate that turns on no syntax extension.
    #[inline]
    pub fn new(db: &Db, file_id: FileId, edition: Edition) -> Self {
        Self::from_span_file_id(db, span::EditionedFileId::new(file_id, edition))
    }

    /// The file read in `edition` for a crate whose syntax `extensions` are on
    /// ([`crate::Crate::extensions`]): a file of the crate is always made with them, so it
    /// parses and prints under them.
    #[inline]
    pub fn new_with_extensions(
        db: &Db,
        file_id: FileId,
        edition: Edition,
        extensions: Extensions,
    ) -> Self {
        let file = span::EditionedFileId::new_with_extensions(file_id, edition, extensions);
        Self::from_span_file_id(db, file)
    }

    /// The [`span::EditionedFileId`] interned.
    #[inline]
    fn field(self, db: &Db) -> span::EditionedFileId {
        span::EditionedFileId::from_raw(self.bits(db))
    }

    /// The syntax extensions of the crate the file is read for, which it is parsed under: part
    /// of the interned id, so of the key of the parse and of every query keyed by the file.
    #[inline]
    pub fn extensions(self, db: &Db) -> Extensions {
        self.field(db).extensions()
    }

    #[inline]
    pub fn current_edition(db: &Db, file_id: FileId) -> Self {
        Self::from_span_file_id(db, span::EditionedFileId::current_edition(file_id))
    }

    #[inline]
    pub fn file_id(self, db: &Db) -> FileId {
        self.field(db).file_id()
    }

    #[inline]
    pub fn span_file_id(self, db: &Db) -> span::EditionedFileId {
        self.field(db)
    }

    #[inline]
    pub fn unpack(self, db: &Db) -> (FileId, span::Edition) {
        self.field(db).unpack()
    }

    #[inline]
    pub fn edition(self, db: &Db) -> Edition {
        self.field(db).edition()
    }

    /// Hands the file's next [`EditionedFileId::parse`] a tree already made of its text, in
    /// full and in this edition (an edit's tree reparsed in place, `blaze_intent::Seed`):
    /// `parse` returns that tree while the caller holds it, instead of parsing again. `content`
    /// is the [`FileText::content`](crate::FileText::content) the tree was made for. Nothing is
    /// taken, and it is `false`, when the file's text has moved on from it, the tree's text is
    /// not the file's, or the file already has a live parse in this edition. What `parse`
    /// returns is the same either way: debug builds check a seed against a whole parse.
    pub fn seed_parse(
        self,
        db: &Db,
        content: ContentId,
        parse: &syntax::Parse<ast::SourceFile>,
    ) -> bool {
        let (file_id, edition) = self.unpack(db);
        let extensions = self.extensions(db);
        let input = db.file_text(file_id);
        input.content(db) == content
            && input.reparse(db).seed(input.text(db), edition, extensions, parse)
    }
}
