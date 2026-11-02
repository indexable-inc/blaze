//! A file's parse made from its last one on an edit (db's decision D8 in `v2/requests/db.md`;
//! docs/v2/plan/lanes/format.md, "an edit reparses the changed item").
//!
//! Each text a file is set to gets a [`Reparse`], set with the text on its
//! [`FileText`](crate::FileText) and not in its content ID:
//! - its own [`Slot`], which only [`EditionedFileId::parse`](crate::EditionedFileId::parse)
//!   writes, with the parse it made of the text;
//! - the slot of the last text of the file that has a parse kept, and the edit from that text to
//!   this one: its common prefix and suffix, found once by the setter (32 bytes a compare)
//!   while it holds both texts.
//!
//! `parse` reparses the kept parse when the slot is of the edit's "before" text and of the same
//! edition and kind (full or outline), else it parses the whole text. A reparse is the parse a
//! whole parse makes (`syntax::Parse::reparse_onto`; debug builds check each one against a whole
//! parse), so what `parse` returns is a function of the text and edition alone and the slot
//! only changes how long it takes.
//!
//! A slot holds its parse weakly: it lives as long as the query engine keeps the parse, so the slots add
//! no tree to what its `lru` cap keeps. Reading a slot is acquire loads (a `OnceLock` each, never
//! a lock, docs/v2/plan/design/concurrency.md) and a weak count, done only when `parse` runs:
//! a cache hit of `parse` reads none, and nothing here allocates outside the setter.

use std::{
    fmt,
    sync::{Arc, OnceLock},
};

use blaze_ids::ContentId;
use span::Edition;
use syntax::{Parse, SourceFile, Text, TextRange, TextSize, WeakParse};

use crate::trees::{TreeError, Trees};

/// How many parses of one text a slot keeps: one per edition and kind it is parsed in, and
/// again after the query engine let one go. Past that, the file's next edit is a whole parse.
const KEPT: usize = 4;

/// What [`EditionedFileId::parse`](crate::EditionedFileId::parse) makes a text's parse from:
/// see the module.
pub struct Reparse {
    slot: Arc<Slot>,
    from: Option<(Arc<Slot>, TextEdit)>,
}

/// One text of a file and the parses made of it.
struct Slot {
    /// The text, shared with its `FileText` and its parse's tree.
    text: Text,
    /// Its blaze-vfs content ID ([`blaze_vfs::FILE_DOMAIN`]).
    id: ContentId,
    kept: [OnceLock<Kept>; KEPT],
}

struct Kept {
    edition: Edition,
    outline: bool,
    parse: WeakParse<SourceFile>,
}

/// The edit that makes one text another: `delete` of the old replaced by `insert` bytes of the
/// new at the same start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TextEdit {
    /// The blaze-vfs content ID of the old text.
    pub(crate) before: ContentId,
    pub(crate) delete: TextRange,
    pub(crate) insert: TextSize,
}

impl fmt::Debug for Reparse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reparse")
            .field("text", &self.slot.id)
            .field("from", &self.from.as_ref().map(|(_, edit)| edit))
            .finish()
    }
}

impl Slot {
    fn new(text: &Text, id: ContentId) -> Arc<Slot> {
        // alloc: one slot per text set, in the setter; `text.clone()` is a refcount.
        Arc::new(Slot { text: text.clone(), id, kept: [const { OnceLock::new() }; KEPT] })
    }

    /// Whether a parse of the text is kept and still alive.
    fn has_parse(&self) -> bool {
        self.kept.iter().filter_map(OnceLock::get).any(|kept| kept.parse.upgrade().is_some())
    }
}

impl Reparse {
    /// The first text of a file, `id` its blaze-vfs content ID: parsed whole.
    pub(crate) fn new(text: &Text, id: ContentId) -> Reparse {
        Reparse { slot: Slot::new(text, id), from: None }
    }

    /// The text a file is set to after the one `self` is of: made from that text's parse if
    /// one is kept, else from the parse the text before it was made from.
    pub(crate) fn after(&self, text: &Text, id: ContentId) -> Reparse {
        let base = match self.slot.has_parse() {
            true => Some(&self.slot),
            false => self.from.as_ref().map(|(slot, _)| slot),
        };
        // clone: a refcount of the base slot, in the setter.
        let from = base.map(|base| (base.clone(), TextEdit::between(&base.text, text, base.id)));
        Reparse { slot: Slot::new(text, id), from }
    }

    /// The edit `parse` makes this text's parse with, if it has one to make it from.
    #[cfg(test)]
    pub(crate) fn edit(&self) -> Option<TextEdit> {
        self.from.as_ref().map(|(_, edit)| *edit)
    }

    /// The parse of `text` (this one's text) in `edition`, an outline when `outline`, made from
    /// the kept parse when there is one, else read from `stored` when it has the text's
    /// ([`Trees`]); kept for the next text. A stored parse that is not the text's is an error.
    pub(crate) fn parse(
        &self,
        text: &Text,
        edition: Edition,
        outline: bool,
        stored: Option<&Trees>,
    ) -> Result<Parse<SourceFile>, TreeError> {
        debug_assert!(text.ptr_eq(&self.slot.text), "a reparse of another text");
        if let Some(own) = self.own(edition, outline) {
            return Ok(own);
        }
        let parse = match self.previous(edition, outline) {
            Some((previous, delete)) => {
                #[cfg(test)]
                REPARSED.with(|it| it.set(it.get() + 1));
                // clone: a refcount; the new tree shares the file's text.
                let parse = previous.reparse_onto(delete, text.clone(), edition);
                if cfg!(debug_assertions) {
                    assert_whole(&parse, text, edition, outline);
                }
                parse
            }
            None => match stored {
                Some(stored) => match stored.map(self.slot.id, text, edition, outline)? {
                    Some(parse) => parse,
                    None => whole(text.clone(), edition, outline), // clone: a refcount, as above.
                },
                None => whole(text.clone(), edition, outline), // clone: a refcount, as above.
            },
        };
        self.keep(&parse, edition, outline);
        Ok(parse)
    }

    /// Whether a parse of this text is kept and alive.
    pub(crate) fn has_parse(&self) -> bool {
        self.slot.has_parse()
    }

    /// The blaze-vfs content ID of this text.
    pub(crate) fn id(&self) -> ContentId {
        self.slot.id
    }

    /// The live outlines of this text, by edition: what the image stores of a library file.
    pub(crate) fn kept_outlines(&self) -> impl Iterator<Item = (Edition, Parse<SourceFile>)> + '_ {
        self.slot
            .kept
            .iter()
            .filter_map(OnceLock::get)
            .filter(|kept| kept.outline)
            .filter_map(|kept| Some((kept.edition, kept.parse.upgrade()?)))
    }

    /// Takes `parse`, a full parse its caller made of this text in `edition` (an edit's tree
    /// reparsed in place, `blaze_intent::Seed`), as the text's parse: [`Reparse::parse`] returns
    /// it while it lives. It is kept weakly, as the parses `parse` makes are, so the caller holds
    /// it until the file is parsed. `false`, and nothing kept, when its text is not this one or
    /// the slot has a live parse of the edition already.
    pub(crate) fn seed(&self, text: &Text, edition: Edition, parse: &Parse<SourceFile>) -> bool {
        debug_assert!(text.ptr_eq(&self.slot.text), "a seed of another text");
        if parse.root().text() != text.as_str() {
            return false;
        }
        if cfg!(debug_assertions) {
            assert_whole(parse, text, edition, false);
        }
        self.keep(parse, edition, false)
    }

    /// A live parse of this text in `edition` and of the kind: a seed, or one `parse` made
    /// that something still holds.
    fn own(&self, edition: Edition, outline: bool) -> Option<Parse<SourceFile>> {
        self.slot
            .kept
            .iter()
            .filter_map(OnceLock::get)
            .filter(|kept| kept.edition == edition && kept.outline == outline)
            .find_map(|kept| kept.parse.upgrade())
    }

    /// The kept parse of the edit's "before" text in `edition` and of the kind, and the range
    /// of that text the edit replaces.
    fn previous(&self, edition: Edition, outline: bool) -> Option<(Parse<SourceFile>, TextRange)> {
        let (slot, edit) = self.from.as_ref()?;
        if slot.id != edit.before {
            return None;
        }
        debug_assert_eq!(
            self.slot.text.len() + usize::from(edit.delete.len()),
            slot.text.len() + usize::from(edit.insert),
            "an edit that does not make the one text the other"
        );
        let parse = slot
            .kept
            .iter()
            .rev()
            .filter_map(OnceLock::get)
            .filter(|kept| kept.edition == edition && kept.outline == outline)
            .find_map(|kept| kept.parse.upgrade())?;
        Some((parse, edit.delete))
    }

    /// Keeps `parse` in this text's slot, unless a live one of its edition and kind is kept:
    /// whether it kept this one.
    fn keep(&self, parse: &Parse<SourceFile>, edition: Edition, outline: bool) -> bool {
        for kept in &self.slot.kept {
            match kept.get() {
                Some(it) if it.edition == edition && it.outline == outline => {
                    if it.parse.upgrade().is_some() {
                        return false;
                    }
                }
                Some(_) => {}
                None => {
                    let made = Kept { edition, outline, parse: parse.downgrade() };
                    if kept.set(made).is_ok() {
                        return true;
                    }
                }
            }
        }
        false
    }
}

#[cfg(test)]
thread_local! {
    /// How many parses on this thread were made from a kept one.
    pub(crate) static REPARSED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
thread_local! {
    /// How many whole parses (a lex and a parse of the whole text) this thread made.
    pub(crate) static WHOLE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(crate) fn whole(text: Text, edition: Edition, outline: bool) -> Parse<SourceFile> {
    #[cfg(test)]
    WHOLE.with(|it| it.set(it.get() + 1));
    match outline {
        true => SourceFile::parse_outline(text, edition),
        false => SourceFile::parse_shared(text, edition),
    }
}

/// Panics unless `made` is the parse a whole parse of `text` makes: the same tree and errors.
pub(crate) fn assert_whole(made: &Parse<SourceFile>, text: &Text, edition: Edition, outline: bool) {
    // clone: the check of debug builds and tests, not of a release parse.
    let whole = whole(text.clone(), edition, outline);
    assert!(
        made.root().file() == whole.root().file(),
        "a reparse whose tree is not the whole parse's"
    );
    let errors = |parse: &Parse<SourceFile>| {
        // alloc: as above, only to compare the two parses' errors.
        let mut errors: Vec<_> = parse
            .errors_with_bodies()
            .into_iter()
            .map(|it| (it.range().start(), it.range().end(), it.to_string())) // alloc: as above.
            .collect();
        errors.sort_unstable();
        errors
    };
    assert_eq!(errors(made), errors(&whole), "a reparse whose errors are not the whole parse's");
}

impl TextEdit {
    /// The edit between `old` (whose blaze-vfs content ID is `before`) and `new`: the bytes
    /// between their longest common prefix and suffix, at character boundaries.
    pub(crate) fn between(old: &str, new: &str, before: ContentId) -> TextEdit {
        let mut prefix = common_prefix(old.as_bytes(), new.as_bytes());
        // A char that ends at `prefix` in one text ends there in the other: their bytes up to
        // it are the same, and its first byte says how long it is.
        while !old.is_char_boundary(prefix) {
            prefix -= 1;
        }
        let mut suffix = common_suffix(&old.as_bytes()[prefix..], &new.as_bytes()[prefix..]);
        // Likewise a char that starts at the suffix's start: that byte is the same in both.
        while !old.is_char_boundary(old.len() - suffix) {
            suffix -= 1;
        }
        let offset = |at: usize| TextSize::try_from(at).expect("a text under 4 GiB");
        TextEdit {
            before,
            delete: TextRange::new(offset(prefix), offset(old.len() - suffix)),
            insert: offset(new.len() - suffix - prefix),
        }
    }
}

const BLOCK: usize = 32;

/// How many bytes `a` and `b` start with in common, 32 at a time: a compare of two `[u8; 32]`
/// is a vector compare (two 16-byte ones on NEON and SSE2, one on AVX2), and only the block
/// they differ in is looked into.
pub(crate) fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    let len = a.len().min(b.len());
    let mut at = 0;
    while at + BLOCK <= len {
        let x: &[u8; BLOCK] = a[at..at + BLOCK].try_into().expect("a block");
        let y: &[u8; BLOCK] = b[at..at + BLOCK].try_into().expect("a block");
        if x != y {
            return at + first_difference(x, y);
        }
        at += BLOCK;
    }
    at + common_prefix_scalar(&a[at..len], &b[at..len])
}

/// How many bytes `a` and `b` end with in common, 32 at a time as [`common_prefix`].
pub(crate) fn common_suffix(a: &[u8], b: &[u8]) -> usize {
    let len = a.len().min(b.len());
    let (a, b) = (&a[a.len() - len..], &b[b.len() - len..]);
    let mut back = 0;
    while back + BLOCK <= len {
        let end = len - back;
        let x: &[u8; BLOCK] = a[end - BLOCK..end].try_into().expect("a block");
        let y: &[u8; BLOCK] = b[end - BLOCK..end].try_into().expect("a block");
        if x != y {
            return back + equal_at_end(x, y);
        }
        back += BLOCK;
    }
    back + common_suffix_scalar(&a[..len - back], &b[..len - back])
}

fn lane(block: &[u8; BLOCK], at: usize) -> u64 {
    u64::from_le_bytes(block[at * 8..at * 8 + 8].try_into().expect("8 bytes"))
}

/// The first byte two different blocks differ at.
fn first_difference(x: &[u8; BLOCK], y: &[u8; BLOCK]) -> usize {
    (0..BLOCK / 8)
        .find_map(|at| {
            let differ = lane(x, at) ^ lane(y, at);
            (differ != 0).then(|| at * 8 + (differ.trailing_zeros() / 8) as usize)
        })
        .unwrap_or(BLOCK)
}

/// How many bytes two different blocks end with in common.
fn equal_at_end(x: &[u8; BLOCK], y: &[u8; BLOCK]) -> usize {
    (0..BLOCK / 8)
        .rev()
        .find_map(|at| {
            let differ = lane(x, at) ^ lane(y, at);
            (differ != 0).then(|| BLOCK - 1 - (at * 8 + 7 - (differ.leading_zeros() / 8) as usize))
        })
        .unwrap_or(BLOCK)
}

/// [`common_prefix`] a byte at a time: the reference the tests hold it to.
pub(crate) fn common_prefix_scalar(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// [`common_suffix`] a byte at a time.
pub(crate) fn common_suffix_scalar(a: &[u8], b: &[u8]) -> usize {
    a.iter().rev().zip(b.iter().rev()).take_while(|(x, y)| x == y).count()
}

#[cfg(test)]
mod seed_tests {
    use std::cell::Cell;

    use blaze_query::db::Durability;
    use span::Edition;
    use syntax::SourceFile;
    use triomphe::Arc;
    use crate::{FileId, FileSet, VfsPath};

    use super::{REPARSED, assert_whole};
    use crate::{Db, EditionedFileId, SourceRoot, SourceRootId};

    fn one_file(db: &mut Db, text: &str) -> (FileId, EditionedFileId) {
        let file = FileId::from_raw(0);
        db.set_file_text_with_durability(file, text, Durability::LOW);
        db.set_file_source_root_with_durability(file, SourceRootId(0), Durability::MEDIUM);
        let mut set = FileSet::default();
        set.insert(file, VfsPath::new_virtual_path("/a/lib.rs".to_owned()));
        let root = Arc::new(SourceRoot::new_local(set));
        db.set_source_root_with_durability(SourceRootId(0), root, Durability::MEDIUM);
        (file, EditionedFileId::new(db, file, Edition::Edition2021))
    }

    /// fastcompile-intent 3: a seed is the file's parse, with no parse of its own, and is the
    /// tree a whole parse makes.
    #[test]
    fn a_seeded_parse_is_the_seed() {
        let mut db = Db::for_tests();
        let (file, id) = one_file(&mut db, "fn f() {}\n");
        let _ = id.parse(&db);
        let text = "fn f() { 1 + 2 }\nstruct S;\n";
        db.set_file_text(file, text);
        let seed = SourceFile::parse_shared(text, Edition::Edition2021);
        let content = db.file_text(file).content(&db);
        assert!(id.seed_parse(&db, content, &seed));
        assert!(!id.seed_parse(&db, content, &seed), "a live parse of the edition is kept already");
        let start = REPARSED.with(Cell::get);
        let parse = id.parse(&db);
        assert_eq!(REPARSED.with(Cell::get), start, "no reparse");
        assert!(std::ptr::eq(parse.root().file(), seed.root().file()), "the seed's own tree");
        assert_whole(parse, db.file_text(file).text(&db), Edition::Edition2021, false);
    }

    #[test]
    fn a_seed_of_another_text_is_refused() {
        let mut db = Db::for_tests();
        let (file, id) = one_file(&mut db, "fn f() {}\n");
        let old = db.file_text(file).content(&db);
        db.set_file_text(file, "fn g() {}\n");
        let stale = SourceFile::parse_shared("fn f() {}\n", Edition::Edition2021);
        assert!(!id.seed_parse(&db, old, &stale), "the file moved on from the seed's text");
        let content = db.file_text(file).content(&db);
        assert!(!id.seed_parse(&db, content, &stale), "the tree's text is not the file's");
        let parse = id.parse(&db);
        assert_eq!(parse.root().text(), "fn g() {}\n");
        assert!(!std::ptr::eq(parse.root().file(), stale.root().file()));
    }

    #[test]
    fn a_dropped_seed_is_parsed_again() {
        let mut db = Db::for_tests();
        let (file, id) = one_file(&mut db, "fn f() {}\n");
        let text = "fn h() -> u8 { 0 }\n";
        db.set_file_text(file, text);
        let content = db.file_text(file).content(&db);
        let seed = SourceFile::parse_shared(text, Edition::Edition2021);
        assert!(id.seed_parse(&db, content, &seed));
        drop(seed);
        let parse = id.parse(&db);
        assert_eq!(parse.root().text(), text);
        assert_whole(parse, db.file_text(file).text(&db), Edition::Edition2021, false);
    }
}
