//! Defines [`EditionedFileId`], an interned wrapper around [`span::EditionedFileId`] that
//! is interned (so queries can take it) and stores only the underlying `span::EditionedFileId`.

use std::{hash::Hash, sync::Arc};

use blaze_ids::ContentId;
use blaze_mem::{Tag, tag};
use span::Edition;
use syntax::{SyntaxError, ast};
use crate::FileId;

use crate::{Db, trees::Trees};

#[blaze_query::interned(debug, constructor = from_span_file_id, unsafe(no_lifetime))]
#[derive(PartialOrd, Ord)]
pub struct EditionedFileId {
    #[returns(copy)]
    field: span::EditionedFileId,
}

/// [`EditionedFileId::parse`]. The query engine keeps every file's parse ([`Db::admit_parses`] lowers that
/// when the trees outgrow [`Tag::Parse`]'s budget), and the trees count against that tag.
#[blaze_query::tracked(self_ty = EditionedFileId, lru = 0, mem_tag = Parse, returns(ref))]
fn parse_(db: &Db, file: EditionedFileId) -> syntax::Parse<ast::SourceFile> {
    let (file_id, edition) = file.unpack(db);
    let span = tracing::info_span!("parse_query", function = tracing::field::Empty);
    if !span.is_disabled() {
        // alloc: only while a subscriber records the span, which interns it
        let path = db.path_for_file(file_id).map_or_else(|| format!("{{file {}}}", file_id.index()), |it| it.to_string());
        span.record("function", path.as_str());
    }
    let _p = span.entered();
    let input = db.file_text(file_id);
    let stored = db.trees().map(|it| &**it);
    let parse = input.reparse_field(db).parse(input.text(db), edition, file.is_library(db), stored);
    parse.unwrap_or_else(|error| {
        let root = db.source_root(db.file_source_root(file_id).source_root_id(db));
        let path = root.source_root_ref(db).path_for_file(&file_id).cloned();
        panic!("{error}, read for {file_id:?} at {path:?}: delete the daemon's image to store it again")
    })
}

/// What a database keeps its files' parses by: the query engine's capacity for them, which
/// [`Db::admit_parses`] sets, and the stored trees read in place of parsing ([`Trees`]).
pub(crate) struct Parses {
    /// How many parses the query engine keeps the values of past a view; 0 keeps every one.
    capacity: usize,
    pub(crate) trees: Option<Arc<Trees>>,
}

impl Parses {
    pub(crate) fn new(trees: Option<Arc<Trees>>) -> Parses {
        Parses { capacity: 0, trees }
    }
}

impl Db {
    /// Sets how many files' parses the query engine keeps past this view by the bytes their trees
    /// hold against [`Tag::Parse`]'s budget: every one while they fit; over it, as many as
    /// fit at the bytes a parse holds now (with an eighth to spare), the ones read longest
    /// ago let go at the next revision; under half of it again, twice as many, back to every
    /// one. The build calls it once a revision, before it writes the revision's inputs. A
    /// database with no budget set keeps every parse.
    pub fn admit_parses(&mut self) {
        let budget = tag::budget(Tag::Parse);
        if budget == u64::MAX {
            return;
        }
        let live = tag::snapshot().get(Tag::Parse).live;
        let capacity = self.parses.capacity;
        let next = if live > budget {
            let parsed = self.files().texts().filter(|it| it.reparse_field(self).has_parse()).count();
            let fit = parsed as u128 * u128::from(budget) * 7 / 8 / u128::from(live);
            match capacity {
                0 => (fit as usize).max(1),
                held => (fit as usize).clamp(1, held),
            }
        } else if capacity != 0 && live < budget / 2 {
            let files = self.files().texts().count();
            match capacity.saturating_mul(2) {
                more if more >= files => 0,
                more => more,
            }
        } else {
            capacity
        };
        if next != capacity {
            self.parses.capacity = next;
            parse_::set_lru_capacity(self, next);
        }
    }

    /// How many files' parses the query engine keeps now ([`Db::admit_parses`]); 0 is every one.
    pub fn parse_capacity(&self) -> usize {
        self.parses.capacity
    }
}

#[blaze_query::tracked]
impl EditionedFileId {
    /// Whether the file is in a library source root: a firewall, so an edit to the root's set
    /// of files that keeps the answer re-parses none of them.
    #[blaze_query::tracked(returns(copy))]
    pub fn is_library(self, db: &Db) -> bool {
        let root = db.file_source_root(self.file_id(db)).source_root_id(db);
        db.source_root(root).source_root_ref(db).is_library
    }

    // firewall query
    #[blaze_query::tracked(returns(as_deref))]
    pub fn parse_errors(self, db: &Db) -> Option<Box<[SyntaxError]>> {
        self.parse(db).errors_exact()
    }

    /// The file's syntax errors with every lazy body's: a firewall, so a check of a
    /// dependency parses its bodies for their errors once.
    #[blaze_query::tracked(returns(as_deref))]
    pub fn parse_errors_with_bodies(self, db: &Db) -> Option<Box<[SyntaxError]>> {
        self.parse(db).errors_with_bodies_exact()
    }
}

impl EditionedFileId {
    /// The file's tree, sharing its text. A file of a library source root (a dependency's) is an
    /// outline, whose `fn` bodies are parsed when first walked into (#1294); every other file is
    /// parsed in full. Decided by the source root, so a file two crates read gets one answer.
    /// After an edit it is made from the file's last parse, when one is kept (see
    /// [`Reparse`](crate::Reparse)): the same tree, in the time of the edited item's parse. A
    /// library file whose outline the daemon's image stores is read from it in place
    /// ([`Trees`]): the tree a parse makes, with the bodies walked before it was stored.
    #[inline]
    pub fn parse(self, db: &Db) -> &syntax::Parse<ast::SourceFile> {
        parse_(db, self)
    }

    #[inline]
    pub fn new(db: &Db, file_id: FileId, edition: Edition) -> Self {
        Self::from_span_file_id(db, span::EditionedFileId::new(file_id, edition))
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
        let input = db.file_text(file_id);
        input.content(db) == content && input.reparse_field(db).seed(input.text(db), edition, parse)
    }
}
