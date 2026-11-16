//! [`EditionedFileId`]: a file of the task's table with the edition and extensions it is
//! parsed with, and its parse.
//!
//! Only a file of the crate whose text the task holds has a parse: the text is read through
//! [`blaze_surface::Source`] with the task's proof ([`crate::Db::file_text`]), and any other
//! file stops the task with a [`crate::Need`] (a dependency is only its records).

use span::{Edition, Extensions, FileId};
use syntax::{SyntaxError, ast};

use crate::Db;

#[blaze_memo::interned(debug, constructor = intern, unsafe(no_lifetime))]
pub struct EditionedFileId {
    #[returns(copy)]
    bits: u32,
}

#[blaze_memo::tracked(self_ty = EditionedFileId, returns(ref), owns, mem_tag = Parse)]
fn parse_(db: &Db, file: EditionedFileId) -> syntax::Parse<ast::SourceFile> {
    let field = file.field(db);
    let text = db.file_text(field.file_id()).text(db).clone(); // clone: a count of the shared text
    ast::SourceFile::parse_shared_with(text, field.edition(), field.extensions())
}

#[blaze_memo::tracked]
impl EditionedFileId {
    #[blaze_memo::tracked(returns(as_deref), owns)]
    pub fn parse_errors(self, db: &Db) -> Option<Box<[SyntaxError]>> {
        self.parse(db).errors_exact()
    }
}

impl EditionedFileId {
    pub fn from_span_file_id(db: &Db, field: span::EditionedFileId) -> Self {
        Self::intern(db, field.as_u32())
    }

    #[inline]
    pub fn parse(self, db: &Db) -> &syntax::Parse<ast::SourceFile> {
        parse_(db, self)
    }

    #[inline]
    pub fn new(db: &Db, file_id: FileId, edition: Edition) -> Self {
        Self::from_span_file_id(db, span::EditionedFileId::new(file_id, edition))
    }

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

    #[inline]
    fn field(self, db: &Db) -> span::EditionedFileId {
        span::EditionedFileId::from_raw(self.bits(db))
    }

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
    pub fn unpack(self, db: &Db) -> (FileId, Edition) {
        self.field(db).unpack()
    }

    #[inline]
    pub fn edition(self, db: &Db) -> Edition {
        self.field(db).edition()
    }
}
