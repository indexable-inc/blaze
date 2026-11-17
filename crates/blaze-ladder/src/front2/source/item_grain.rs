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
use blaze_scope::resolve::At;
use blaze_surface::ItemIndex;

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

    /// `member` of `owner` (crate `krate`) as it is now: its outline entry and its whole text.
    /// Its file unchanged: the row as written, its text through the file's digest. Its file
    /// changed: a member has no item row of its own, so it is read through its OWNER's row of
    /// today ([`Records::row_now`]: the impl or trait found by its chain), as the one child of
    /// the owner's entry in today's outline of its kind and name, the same rank among the
    /// owner's members of that kind and name as the record's row (`cfg`'d twins keep their
    /// order: a member added or removed moves the file's `Surface`, so the record is remade).
    /// Its text is then the member's own, so a reader of a sibling never sees an edit of it.
    /// Its owner gone today is that owner's [`SourceError::ItemStale`]; the member gone from a
    /// present owner is `ItemsRecordError::Gone` of its file; each filed.
    pub(super) fn member_now(&'n self, krate: usize, owner: ItemKey, member: &MemberRow<'n>) -> Option<(ItemIndex, &'n str)> {
        let error = match self.items.member_text(owner, member) {
            Ok(text) => return Some((member.outline, text)),
            Err(error) => error,
        };
        let file = match error {
            ItemsRecordError::Stale { file: Some(file), call: None, .. } => file,
            error => {
                self.log.put(SourceError::Items { krate, name: self.log.name(krate), error });
                return None;
            }
        };
        let now = self.row_now(krate, owner)?;
        let (Some(today), Some(TextSpan { at: At::File(owner_file), .. })) = (now.today, now.row.text) else {
            // The owner reads as the record saw it while its member's file changed: they are
            // not in one text, which a member written in its owner's body always is.
            self.log.put(SourceError::Items { krate, name: self.log.name(krate), error });
            return None;
        };
        if owner_file != file {
            self.log.put(SourceError::Items { krate, name: self.log.name(krate), error });
            return None;
        }
        let rank = self.member_rank(krate, owner, member)?;
        let record = self.items_of(krate)?;
        let mut outlines = self.outlines.borrow_mut();
        if outlines.get(&(krate, file)).is_none() {
            if outlines.len() >= super::MOST_OUTLINES {
                drop(outlines);
                self.log.put(SourceError::Gap { question: "a member of a changed file: more files outlined than the most one command keeps (source.rs `MOST_OUTLINES`)", key: Some(member.key) });
                return None;
            }
            outlines.insert((krate, file), blaze_file_items::outline(today, record.edition(), self.alloc));
        }
        let outline = outlines.get(&(krate, file))?;
        let found = outline
            .children(Some(now.row.outline))
            .filter(|(_, child)| child.kind() == member.kind && child.name.map(|range| child.text(today, range)) == member.name)
            .nth(rank)
            .map(|(at, child)| (at, child.range.start, child.range.end));
        drop(outlines);
        let Some((at, start, end)) = found else {
            let gone = match self.keys.get(krate) {
                Some(key) => ItemsRecordError::Gone { krate: *key, file: Some(file), call: None },
                None => ItemsRecordError::Short { what: "a crate with an items record", at: krate },
            };
            self.log.put(SourceError::Items { krate, name: self.log.name(krate), error: gone });
            return None;
        };
        let Some(text) = today.get(start as usize..end as usize) else {
            let range = match self.keys.get(krate) {
                Some(key) => ItemsRecordError::Range { krate: *key, start, len: end.saturating_sub(start) },
                None => ItemsRecordError::Short { what: "a crate with an items record", at: krate },
            };
            self.log.put(SourceError::Items { krate, name: self.log.name(krate), error: range });
            return None;
        };
        Some((at, text))
    }

    /// How many members of `owner` written in its own text share `member`'s kind and name and
    /// come before it (by the record's outline entries, the text's order).
    fn member_rank(&'n self, krate: usize, owner: ItemKey, member: &MemberRow<'n>) -> Option<usize> {
        let rows = self.read(krate, self.items.members(owner))??;
        let mut rank = 0;
        for row in rows.iter() {
            let row = self.read(krate, row)?;
            if row.written_by.is_none() && row.kind == member.kind && row.name == member.name && row.outline.0 < member.outline.0 {
                rank += 1;
            }
        }
        Some(rank)
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
