//! The records' texts read at ITEM grain: an edit inside one body leaves every other item of its
//! file readable, and the edited body readable at its new place.
//!
//! The records design: a record is valid by its key, or else by its read list checked against
//! today's facts, at item grain. After an edit inside a fn's body, the crate's scope record
//! still stands (no surface moved), but its items record holds the file's OLD digest. Reading
//! an item's text through the whole file's digest (`ItemsRecord::whole`) refuses every item of
//! the edited file as `Stale`, so the edited body was never checked (RECORDS-PIPELINE.md,
//! "Runner 2", gap (b)). Here each item is asked of blaze-scope's item-grain question
//! (`RecordItems::item_now`, items_record/grain.rs):
//!
//! - its file's text unchanged: the row as written (`ItemNow::Same`), its spans read through
//!   the whole file's digest, checked once per file per command, as before;
//! - its file changed and one entry of today's text has its chain: the row's facts hold (the
//!   record holds no head and every row fact is covered by the `Surface` its scope reads, D114
//!   ruling), at TODAY's place (`ItemNow::Fresh`): its spans are into today's text, never the
//!   record's old absolute offsets (rule 71), so a signature edited in type only is read as
//!   it is today, and what reads it is moved by the item's `Signature` (blaze-cli);
//! - no single entry of today's text has its chain: that item alone is refused
//!   ([`SourceError::ItemStale`], naming the item and its file). The scope is then remade by
//!   its own key (a rename or removal moves `Surface`); nothing here papers over it.
//!
//! A member of a trait or impl (a member row, read through its owner) whose file changed is read
//! the same way, as its own item row; one with no item row keeps the whole file's refusal. An
//! expansion's text stays checked whole: an expansion is remade as one record.

use blaze_id::ItemKey;
use blaze_scope::items_record::{ItemNow, ItemRow, ItemsRecordError, MemberRow, TextSpan};

use super::{Log, Records, SourceError};

/// An item row as it holds against today's text.
#[derive(Clone, Copy, Debug)]
pub(super) struct RowNow<'n> {
    pub(super) row: ItemRow<'n>,
    /// `Some`: the item's file changed and the row's spans are into this text, the file as it
    /// is now. `None`: the file is the one the record saw, and a span is read through the
    /// whole file's digest.
    pub(super) today: Option<&'n str>,
}

impl<'n> Records<'n> {
    /// The item `key` of crate `krate` against today's text of its file. `None` when the crate
    /// holds no item with text under the key (as `ItemsRecord::item` answers), or with the
    /// reason filed in the log.
    pub(super) fn row_now(&'n self, krate: usize, key: ItemKey) -> Option<RowNow<'n>> {
        let answer = self.items.item_now(key);
        self.now_of(krate, answer)
    }

    /// An `item_now` answer, its refusal filed.
    fn now_of(&'n self, krate: usize, answer: Result<Option<ItemNow<'n>>, ItemsRecordError>) -> Option<RowNow<'n>> {
        match self.read(krate, answer)?? {
            ItemNow::Same(row) => Some(RowNow { row, today: None }),
            ItemNow::Fresh { row, text, .. } => Some(RowNow { row, today: Some(text) }),
            ItemNow::Stale { key, file, chain } => {
                self.log.put(SourceError::ItemStale { krate, name: self.log.name(krate), key, file, chain });
                None
            }
        }
    }

    /// The stretch `span` of `now`'s row (its text or its impl header): from today's text when
    /// its file changed, else through the whole file's digest. A stretch outside the text is
    /// filed as `ItemsRecordError::Range`.
    pub(super) fn span_now(&'n self, krate: usize, now: &RowNow<'n>, span: TextSpan) -> Option<&'n str> {
        let Some(today) = now.today else {
            return self.read(krate, self.items.text(krate, span));
        };
        let got = (span.start as usize).checked_add(span.len as usize).and_then(|end| today.get(span.start as usize..end));
        if got.is_none() {
            match self.keys.get(krate) {
                Some(key) => self.log.put(SourceError::Items { krate, name: self.log.name(krate), error: ItemsRecordError::Range { krate: *key, start: span.start, len: span.len } }),
                None => self.log.put(SourceError::Items { krate, name: self.log.name(krate), error: ItemsRecordError::Short { what: "a crate with an items record", at: krate } }),
            }
        }
        got
    }

    /// The text of `member` of `owner` (crate `krate`): through its file's digest; when that
    /// file changed, at item grain as the member's own item row ([`Records::row_now`]). A member
    /// with no item row keeps the whole file's refusal, filed.
    pub(super) fn member_text_now(&'n self, krate: usize, owner: ItemKey, member: &MemberRow<'n>) -> Option<&'n str> {
        let error = match self.items.member_text(owner, member) {
            Ok(text) => return Some(text),
            Err(error) => error,
        };
        let file_changed = matches!(error, ItemsRecordError::Stale { file: Some(_), call: None, .. });
        if !file_changed {
            self.log.put(SourceError::Items { krate, name: self.log.name(krate), error });
            return None;
        }
        match self.items.item_now(member.key) {
            Ok(None) => {
                self.log.put(SourceError::Items { krate, name: self.log.name(krate), error });
                None
            }
            answer => {
                let now = self.now_of(krate, answer)?;
                let Some(span) = now.row.text else {
                    self.log.put(SourceError::Items { krate, name: self.log.name(krate), error: ItemsRecordError::NoPlace { key: member.key } });
                    return None;
                };
                self.span_now(krate, &now, span)
            }
        }
    }

    /// The whole text of file `file` of crate `krate` as it is now, for a reader that reads
    /// every item in it at item grain (project_records.rs: a body's start is its text's offset
    /// in this text, and its outline entry is found by its chain in this text's outline). The
    /// file unchanged: proved by its digest, as [`Records::whole_text`]. Changed: today's text,
    /// whose items are each proved by [`Records::row_now`] when they are read; never the
    /// record's offsets into it. `None` with the reason filed.
    pub fn file_now(&'n self, krate: usize, file: u32) -> Option<&'n str> {
        self.items_of(krate)?;
        self.read(krate, self.items.file_now(krate, file))
    }
}

impl Log<'_> {
    /// How many refusals so far are a whole FILE's text changed (`ItemsRecordError::Stale` of a
    /// file): what reading at item grain removes for an edit inside a body. Counted over the
    /// errors kept ([`super::MOST_ERRORS`]).
    pub fn stale_files(&self) -> usize {
        self.errors().iter().filter(|e| matches!(e, SourceError::Items { error: ItemsRecordError::Stale { file: Some(_), .. }, .. })).count()
    }

    /// How many items so far were refused at item grain ([`SourceError::ItemStale`]: their chain
    /// moved or their place is gone).
    pub fn stale_items(&self) -> usize {
        self.errors().iter().filter(|e| matches!(e, SourceError::ItemStale { .. })).count()
    }
}
